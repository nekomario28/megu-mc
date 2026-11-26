// SPDX-License-Identifier: LGPL-3.0-or-later

use megu_client::protocol::{DecodedChunk, DecodedLightData, DecodedSection, DimensionMeta};
use megu_client::world::{
    BiomeClimateEntry, BiomeTintKind, BiomeTintResolver, BlockTable, WorldState,
};
use megu_client_gpu::renderer::{select_composed_depth_store_op, stage_compact_verts};
use megu_client_gpu::subworld_render::{SubWorldRenderBank, SubworldOwner};
use megu_client_gpu::vanilla_assets_bridge::{
    admit_resource_packs, MAX_SHARED_ASSET_PACK_SOURCE_BYTES,
};
use megu_client_mesh::ffi::PackedVertex;
use megu_client_mesh::{CompactVoxelVertex, SectionCoord, OPAQUE};
use megu_core::hw_adaptive_engine::GpuComputeParadigm;
use megu_core::render_cs::{RenderGraph, ResourceDesc};
use megu_proto::subworld_wire::{
    SubworldFrameMode, SubworldPoseRecord, SubworldSyncFrame as WireSubworldSyncFrame,
};

fn test_block_table() -> BlockTable {
    let json = r##"{
        "blocks": [
            ["minecraft:air", 0, 1],
            ["minecraft:grass_block", 1, 1]
        ],
        "flags": [1, 8],
        "full_sides": [0, 63],
        "shapes": [{"min": [0.0, 0.0, 0.0], "max": [1.0, 1.0, 1.0]}],
        "collision_shapes": [[], [0]]
    }"##;
    BlockTable::from_json_bytes(json.as_bytes()).expect("synthetic block table")
}

fn empty_light() -> DecodedLightData {
    DecodedLightData {
        sky_mask: Vec::new(),
        block_mask: Vec::new(),
        empty_sky_mask: Vec::new(),
        empty_block_mask: Vec::new(),
        sky_arrays: Vec::new(),
        block_arrays: Vec::new(),
    }
}

fn chunk_with_edits(biome: u32, edits: &[(usize, u32)]) -> DecodedChunk {
    let dimension = DimensionMeta::default();
    let mut sections = Vec::new();
    for section in 0..dimension.section_count() {
        let mut states = Box::new([0u32; 4096]);
        if section == 4 {
            for &(index, state) in edits {
                states[index] = state;
            }
        }
        sections.push(DecodedSection {
            non_air: i16::from(section == 4 && !edits.is_empty()),
            states,
            biomes: [biome; 64],
        });
    }
    DecodedChunk {
        cx: 0,
        cz: 0,
        sections,
        light: empty_light(),
        block_entities: Vec::new(),
    }
}

fn cell_index(x: usize, y: usize, z: usize) -> usize {
    y * 256 + z * 16 + x
}

#[test]
fn dynamic_mod_ids_survive_chunk_and_single_block_paths_with_light_facts() {
    let dynamic_id = 1_000_042u32;
    let facts_byte = (14u8 << 4) | 15;
    let mut world = WorldState::new(DimensionMeta::default(), test_block_table());
    assert_eq!(
        world.install_dynamic_mod_registry_states(&[(
            dynamic_id,
            "examplemod:emissive_lamp".to_string(),
            facts_byte,
        )]),
        1
    );
    assert!(world.state_count > dynamic_id as usize);
    assert_eq!(
        world.blocks.state_name(dynamic_id),
        Some("examplemod:emissive_lamp")
    );
    let fact = world
        .blocks
        .dynamic_light_fact(dynamic_id)
        .expect("dynamic light facts");
    assert_eq!(fact.opacity, 15);
    assert_eq!(fact.emission, 14);
    assert!(fact.occlusion.iter().all(|&face| face == 0x0f));

    let index = cell_index(1, 0, 1);
    world
        .apply_chunk(chunk_with_edits(39, &[(index, dynamic_id)]))
        .expect("dynamic chunk applies");
    assert_eq!(world.get_block_state(1, 0, 1), Some(dynamic_id));
    world
        .apply_block_update([2, 0, 1], dynamic_id)
        .expect("dynamic single-block update");
    assert_eq!(world.get_block_state(2, 0, 1), Some(dynamic_id));

    let mesh = world
        .mesh_cache
        .section(SectionCoord(0, 0, 0))
        .expect("section mesh snapshot");
    assert_eq!(mesh.get(1, 0, 1).block.state_id, dynamic_id);
    assert_eq!(mesh.get(2, 0, 1).block.state_id, dynamic_id);
    assert_eq!(mesh.get(1, 0, 1).block.flags & OPAQUE, OPAQUE);
}

#[test]
fn canonical_biome_climates_are_distinct_and_registry_updates_invalidate_sections() {
    let resolver = BiomeTintResolver::default();
    let ids = [14, 28, 53, 39]; // desert, jungle, swamp, plains
    let tints: Vec<u32> = ids
        .into_iter()
        .map(|id| resolver.resolve(id, BiomeTintKind::Grass, 0))
        .collect();
    assert!(tints.iter().all(|&tint| tint != 0));
    for i in 0..tints.len() {
        for j in i + 1..tints.len() {
            assert_ne!(tints[i], tints[j], "biome tints must not be flat");
        }
    }
    assert_eq!(
        resolver.resolve(53, BiomeTintKind::Water, 0),
        0x617b64,
        "swamp water override comes from the canonical table"
    );

    let mut world = WorldState::new(DimensionMeta::default(), test_block_table());
    world
        .apply_chunk(chunk_with_edits(39, &[(cell_index(0, 0, 0), 1)]))
        .expect("grass chunk applies");
    let _ = world.mesh_cache.rebuild_dirty(usize::MAX);
    assert_eq!(world.mesh_cache.pending(), 0);
    let accepted = world.update_biome_climates_from_registry(&[BiomeClimateEntry {
        biome_id: 39,
        temperature: 0.2,
        downfall: 0.9,
        grass_color: Some(0x123456),
        foliage_color: None,
        water_color: Some(0x3f76e4),
        grass_color_modifier: None,
    }]);
    assert_eq!(accepted, 1);
    assert!(
        world.mesh_cache.pending() > 0,
        "climate replacement must dirty affected loaded sections"
    );
    let mesh = world
        .mesh_cache
        .section(SectionCoord(0, 0, 0))
        .expect("updated mesh snapshot");
    assert_eq!(mesh.get(0, 0, 0).block.tint, 0x123456);
}

#[test]
fn tbdr_first_frame_world_env_depth_consumer_forces_depth_store() {
    assert_eq!(
        select_composed_depth_store_op(GpuComputeParadigm::TileBasedTbdr, None, false, true,),
        wgpu::StoreOp::Store
    );
    assert_eq!(
        select_composed_depth_store_op(GpuComputeParadigm::TileBasedTbdr, None, false, false,),
        wgpu::StoreOp::Discard
    );

    let mut graph = RenderGraph::new();
    let depth = graph.add_transient_resource(ResourceDesc {
        bytes: 1280 * 720 * 4,
        kind_tag: 1,
    });
    let world = graph.add_pass("composed-world", &[], &[depth], true);
    graph.add_pass("world-env-pass", &[depth], &[], true);
    let compiled = graph.compile().expect("graph compiles");
    let world_pos = compiled
        .order
        .iter()
        .position(|&pass| pass == world)
        .expect("world pass in order") as u32;
    let (_, last) = compiled.lifetime(depth).expect("depth is live");
    assert!(last > world_pos, "environment pass extends depth lifetime");
}

#[test]
fn shared_asset_budget_admits_high_priority_packs_before_low_priority() {
    let unique = format!(
        "megu-w1b-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("time")
            .as_nanos()
    );
    let root = std::env::temp_dir().join(unique);
    std::fs::create_dir_all(&root).expect("temp root");
    let low = root.join("low-large.pack");
    let high = root.join("high-small.pack");
    std::fs::write(&high, b"tiny").expect("high pack");
    std::fs::File::create(&low)
        .expect("low pack")
        .set_len(MAX_SHARED_ASSET_PACK_SOURCE_BYTES + 1)
        .expect("size low pack");

    let admitted = admit_resource_packs(&[low.clone(), high.clone()]);
    assert_eq!(admitted, vec![high.clone()]);

    let low_small = root.join("low-small.pack");
    std::fs::write(&low_small, b"low").expect("small low pack");
    let admitted = admit_resource_packs(&[low_small.clone(), high.clone()]);
    assert_eq!(admitted, vec![low_small.clone(), high.clone()]);

    let _ = std::fs::remove_file(low);
    let _ = std::fs::remove_file(high);
    let _ = std::fs::remove_file(low_small);
    let _ = std::fs::remove_dir(root);
}

fn wire_record(
    id: u64,
    dim: u32,
    revision: u64,
    pos: [f64; 3],
    pivot: [f32; 3],
    voxels: Vec<([i16; 3], u32)>,
) -> SubworldPoseRecord {
    SubworldPoseRecord {
        subworld_id: id,
        dimension_id: dim,
        revision,
        pos,
        quat: [0.0, 0.0, 0.0, 1.0],
        vel: [0.0; 3],
        ang_vel: [0.0; 3],
        pivot,
        flags: 0,
        voxels,
    }
}

#[test]
fn subworld_network_reconciliation_preserves_local_and_cross_dimension_entries() {
    let mut bank = SubWorldRenderBank::new();
    bank.upsert_contraption_subworld(1, [0.0; 3], [0.0, 1.0, 0.0], 0.0, &(Vec::new(), Vec::new()));
    bank.apply_sync_frame_detailed(&WireSubworldSyncFrame {
        mode: SubworldFrameMode::Baseline,
        records: vec![
            wire_record(2, 0, 1, [0.0; 3], [0.0; 3], Vec::new()),
            wire_record(3, 1, 1, [0.0; 3], [0.0; 3], Vec::new()),
            wire_record(4, 0, 1, [0.0; 3], [0.0; 3], Vec::new()),
        ],
    });
    assert_eq!(bank.len(), 4);

    bank.apply_sync_frame_detailed(&WireSubworldSyncFrame {
        mode: SubworldFrameMode::Baseline,
        records: vec![wire_record(2, 0, 2, [1.0, 0.0, 0.0], [0.0; 3], Vec::new())],
    });

    assert!(
        bank.get(1).is_some(),
        "local hull survives network reconciliation"
    );
    assert!(bank.get_network(2, 0).is_some());
    assert!(bank.get_network(3, 1).is_some(), "other dimension survives");
    assert!(
        bank.get_network(4, 0).is_none(),
        "stale same-dimension hull is removed"
    );
    assert!(matches!(
        bank.get(1).expect("local").owner,
        SubworldOwner::Local
    ));
}

#[test]
fn subworld_pivot_normalized_asymmetric_hull_and_pose_delta_preserves_geometry() {
    let half = std::f32::consts::FRAC_1_SQRT_2;
    let mut record = wire_record(
        9,
        0,
        1,
        [10.0, 10.0, 10.0],
        [0.5, 0.5, 0.5],
        vec![([-1, 0, 0], 7), ([0, 0, 0], 7)],
    );
    record.quat = [0.0, 0.0, half, half];
    let mut bank = SubWorldRenderBank::new();
    bank.apply_sync_frame_detailed(&WireSubworldSyncFrame {
        mode: SubworldFrameMode::Spawn,
        records: vec![record.clone()],
    });
    let entry = bank.get_network(9, 0).expect("spawned network hull");
    assert!(!entry.vertices.is_empty());
    assert!(!entry.indices.is_empty());
    assert_eq!(entry.voxel_origin, [-1.0, 0.0, 0.0]);
    let pos = entry.transform.position;
    assert!((pos[0] - 10.5).abs() < 1.0e-4, "x: {pos:?}");
    assert!((pos[1] - 8.5).abs() < 1.0e-4, "y: {pos:?}");
    assert!((pos[2] - 9.5).abs() < 1.0e-4, "z: {pos:?}");
    let vertex_count = entry.vertices.len();

    let mut pose = wire_record(9, 0, 2, [20.0, 10.0, 10.0], [0.5, 0.5, 0.5], Vec::new());
    pose.quat = record.quat;
    bank.apply_sync_frame_detailed(&WireSubworldSyncFrame {
        mode: SubworldFrameMode::Pose,
        records: vec![pose],
    });
    let entry = bank.get_network(9, 0).expect("hull survives pose delta");
    assert_eq!(entry.vertices.len(), vertex_count);
    assert!(
        !entry.vertices.is_empty(),
        "pose delta must not empty geometry"
    );
    assert!((entry.transform.position[0] - 20.5).abs() < 1.0e-4);
}

#[test]
fn dynamic_mod_quads_route_to_unpacked_terrain_instead_of_degenerating() {
    let dynamic_id = 1_000_042u32;
    assert!(
        dynamic_id > megu_client_mesh::COMPACT_VERTEX_MAX_STATE_ID,
        "test requires the dynamic id to exceed the compact vertex domain"
    );

    // Negative control: the compact stream genuinely cannot represent this
    // state id, so a packed quad carrying it would degenerate to four zeroed
    // vertices without the routing.
    let unrepresentable = PackedVertex {
        position: 0,
        state_id: dynamic_id,
        attributes: 0,
        tint: 0,
    };
    assert!(!CompactVoxelVertex::is_representable(&unrepresentable));
    let mut scratch = Vec::new();
    let bad = stage_compact_verts(&mut scratch, &[unrepresentable; 4]);
    assert_eq!(bad, 1, "unrouted dynamic quads degenerate in compact stream");
    assert!(scratch.iter().all(|&byte| byte == 0));

    // Positive path: a full-cube dynamic mod block meshed in a real world
    // routes every emitted quad into the general unpacked terrain stream.
    let facts_byte = (14u8 << 4) | 15;
    let mut world = WorldState::new(DimensionMeta::default(), test_block_table());
    world.install_dynamic_mod_registry_states(&[(
        dynamic_id,
        "examplemod:emissive_lamp".to_string(),
        facts_byte,
    )]);
    world
        .apply_chunk(chunk_with_edits(39, &[(cell_index(1, 0, 1), dynamic_id)]))
        .expect("dynamic chunk applies");
    let coord = SectionCoord(0, 0, 0);
    world
        .rebuild_dirty_bounded(usize::MAX, [8.0, 8.0, 8.0])
        .expect("mesh rebuild");

    let unpacked = world
        .unpacked_terrain_quads(coord)
        .expect("dynamic quads must route to the unpacked terrain stream");
    let dynamic_quads: Vec<_> = unpacked
        .iter()
        .filter(|quad| quad.state_id == dynamic_id)
        .collect();
    assert!(
        !dynamic_quads.is_empty(),
        "dynamic block emits rendered quads, not zeroed vertices"
    );
    for quad in dynamic_quads {
        let area_span: f32 = (0..3)
            .map(|axis| {
                let mut min = f32::MAX;
                let mut max = f32::MIN;
                for position in quad.positions {
                    min = min.min(position[axis]);
                    max = max.max(position[axis]);
                }
                max - min
            })
            .sum();
        assert!(area_span > 0.5, "quad geometry is non-degenerate");
    }

    // Nothing carrying the dynamic id remains in the packed stream, so the
    // compact staging path reports zero degenerate quads for the section.
    if let Some(packed) = world.packed_mesh(coord) {
        assert!(
            packed.iter().all(|vertex| vertex.state_id != dynamic_id),
            "packed stream must not carry compact-unrepresentable state ids"
        );
        let mut scratch = Vec::new();
        assert_eq!(stage_compact_verts(&mut scratch, packed), 0);
    }
}
