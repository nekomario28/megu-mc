// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! Lane RAM-VRAM-APP-1: `MeshCache::insert_compressed` wiring, monotonic
//! per-section `mesh_revision`, and the post-upload zero-duplication CPU
//! release (`release_uploaded_section_cpu_copies`) per
//! `audit/ram-vram-role-separation-and-gpu-hiz-design-20261123.md`
//! §3.2 + §7 step 2 (V2).

use std::collections::BTreeSet;

use megu_client::protocol::{DecodedChunk, DecodedLightData, DecodedSection, DimensionMeta};
use megu_client::world::{BlockTable, WorldState};
use megu_client_mesh::SectionCoord;

const STONE: u32 = 1;
const WATER: u32 = 80;
const AIR: u32 = 0;

fn test_dimension() -> DimensionMeta {
    DimensionMeta {
        name: "minecraft:overworld".to_string(),
        min_y: 0,
        height: 64,
        has_skylight: true,
    }
}

fn uniform_section(state: u32) -> DecodedSection {
    DecodedSection {
        non_air: if state == AIR {
            0
        } else {
            megu_client_mesh::SECTION_CELLS as i16
        },
        states: Box::new([state; megu_client_mesh::SECTION_CELLS]),
        biomes: [0; 64],
    }
}

/// Stone floor at local y=0, water layer at local y=1, air above: one mixed
/// opaque + fluid section that produces both opaque and translucent quads.
fn mixed_section() -> DecodedSection {
    let mut states = [AIR; megu_client_mesh::SECTION_CELLS];
    states[..256].fill(STONE);
    states[256..512].fill(WATER);
    DecodedSection {
        non_air: 512,
        states: Box::new(states),
        biomes: [0; 64],
    }
}

fn chunk(cx: i32, cz: i32, sections: Vec<DecodedSection>) -> DecodedChunk {
    DecodedChunk {
        cx,
        cz,
        sections,
        light: DecodedLightData {
            sky_mask: Vec::new(),
            block_mask: Vec::new(),
            empty_sky_mask: Vec::new(),
            empty_block_mask: Vec::new(),
            sky_arrays: Vec::new(),
            block_arrays: Vec::new(),
        },
        block_entities: Vec::new(),
    }
}

fn loaded_world() -> WorldState {
    let mut world = WorldState::new(test_dimension(), BlockTable::builtin());
    world
        .apply_chunk(chunk(
            0,
            0,
            vec![
                uniform_section(STONE),
                mixed_section(),
                uniform_section(WATER),
                uniform_section(AIR),
            ],
        ))
        .unwrap();
    world
        .apply_chunk(chunk(
            1,
            0,
            vec![
                uniform_section(STONE),
                uniform_section(AIR),
                uniform_section(AIR),
                uniform_section(AIR),
            ],
        ))
        .unwrap();
    world
}

#[test]
fn ram_vram_v2_compressed_sections_revision_bump_and_zero_cpu_opaque_after_upload() {
    let mut world = loaded_world();
    let camera = [8.0, 4.0, 8.0];
    world.rebuild_dirty_bounded(usize::MAX, camera).unwrap();

    let stone = SectionCoord(0, 0, 0);
    let mixed = SectionCoord(0, 1, 0);
    let water = SectionCoord(0, 2, 0);
    let air = SectionCoord(0, 3, 0);
    let stone_b = SectionCoord(1, 0, 0);

    // `insert_compressed`: uniform stone and water sections hold zero heap
    // bytes inside `MeshCache`; the mixed section is dense.
    assert_eq!(world.mesh_cache.is_uniform_section(stone), Some(true));
    assert_eq!(world.mesh_cache.blocks_heap_bytes(stone), Some(0));
    assert_eq!(world.mesh_cache.is_uniform_section(water), Some(true));
    assert_eq!(world.mesh_cache.blocks_heap_bytes(water), Some(0));
    assert_eq!(world.mesh_cache.is_uniform_section(mixed), Some(false));
    assert!(world.mesh_cache.blocks_heap_bytes(mixed).unwrap() > 0);

    // Uniform air never enters the mesh cache or the packed set.
    assert!(world.mesh_cache.mesh(air).is_none());
    assert!(world.section_mesh_revision(air).is_none());

    // Every meshed section got a monotonic revision on its first rebuild.
    let meshed = [stone, mixed, water, stone_b];
    assert_eq!(world.packed_mesh_count(), meshed.len());
    let revs: Vec<u64> = meshed
        .iter()
        .map(|&coord| world.section_mesh_revision(coord).expect("meshed revision"))
        .collect();
    assert!(
        revs.iter().all(|&rev| rev >= 1),
        "revisions start at 1 so 0 stays a never-uploaded sentinel"
    );
    assert_eq!(
        revs.iter().copied().collect::<BTreeSet<_>>().len(),
        revs.len(),
        "revisions must be unique per rebuild"
    );

    // Pre-release orders for sections with translucent quads, used below to
    // prove a post-release re-sort still emits the same index set.
    let pre_water_order = world.translucent_order(water).unwrap().to_vec();
    assert!(!pre_water_order.is_empty(), "uniform water emits translucent quads");

    // Post-upload release: every CPU duplicate drops, the meta stays, and
    // the revision is untouched so unchanged frames keep matching.
    let mut total_freed = 0usize;
    for &coord in &meshed {
        total_freed += world.release_uploaded_section_cpu_copies(coord);
    }
    assert!(total_freed > 0, "packed vertices + opaque quads must free bytes");
    assert_eq!(world.packed_mesh_count(), 0, "no CPU PackedVertex copies remain");
    for &coord in &meshed {
        let meta = world
            .released_mesh_meta(coord)
            .expect("released section keeps meta");
        assert!(meta.quad_count > 0);
        assert_eq!(meta.vertex_count, meta.quad_count * 4);
        assert_eq!(
            world.section_mesh_revision(coord),
            Some(revs[meshed.iter().position(|&c| c == coord).unwrap()]),
            "release must leave the mesh revision intact"
        );
        // Idempotent: a second release frees nothing more.
        assert_eq!(world.release_uploaded_section_cpu_copies(coord), 0);
    }
    let water_meta = *world.released_mesh_meta(water).unwrap();
    assert_eq!(
        water_meta.kept_translucent_quads, water_meta.quad_count,
        "an all-water mesh keeps every quad for re-sorts"
    );
    let stone_meta = *world.released_mesh_meta(stone).unwrap();
    assert_eq!(
        stone_meta.kept_translucent_quads, 0,
        "an all-opaque mesh keeps nothing on the CPU"
    );
    assert!(world.mesh_cache.mesh(stone).unwrap().is_empty());
    assert_eq!(
        world.mesh_cache.mesh(water).unwrap().len(),
        water_meta.kept_translucent_quads
    );
    // Releasing an unloaded/dirty coordinate is a no-op.
    assert_eq!(world.release_uploaded_section_cpu_copies(air), 0);
    assert!(world.released_mesh_meta(air).is_none());

    // A camera move across the resort threshold re-sorts the released
    // translucent quads; the released sections must stay released (no
    // translucent-only packed mesh resurrects) while the order is
    // re-derived against the uploaded mesh's index space.
    let moved_camera = [8.0, 4.0, 40.0];
    world
        .rebuild_dirty_bounded(usize::MAX, moved_camera)
        .unwrap();
    assert_eq!(
        world.packed_mesh_count(),
        0,
        "a translucent re-sort must not resurrect released CPU copies"
    );
    for &coord in &meshed {
        assert!(world.released_mesh_meta(coord).is_some());
    }
    if let Some(order) = world.translucent_order(water) {
        assert_eq!(order.len(), water_meta.kept_translucent_quads);
        assert!(
            order.iter().all(|&idx| (idx as usize) < water_meta.quad_count),
            "order must index the uploaded mesh, not the kept subset"
        );
        let mut released_set = order.to_vec();
        released_set.sort_unstable();
        let mut uploaded_set = pre_water_order.clone();
        uploaded_set.sort_unstable();
        assert_eq!(
            released_set, uploaded_set,
            "the post-release order permutes exactly the uploaded translucent indices"
        );
    }

    // A block edit re-dirties the released section; rebuilding re-populates
    // `packed_meshes` under a strictly higher revision.
    world.apply_block_update([3, 3, 3], AIR).unwrap();
    assert!(world.pending_mesh_sections() > 0);
    assert_eq!(
        world.mesh_cache.is_uniform_section(stone),
        Some(false),
        "the first non-uniform write promotes storage to dense"
    );
    world.rebuild_dirty_bounded(usize::MAX, moved_camera).unwrap();
    let rev_after_edit = world.section_mesh_revision(stone).unwrap();
    assert!(
        rev_after_edit > revs[0],
        "re-meshing a dirtied section must bump its revision"
    );
    assert_eq!(world.packed_mesh_count(), 1);
    assert!(world.packed_mesh(stone).is_some());
    assert!(
        world.released_mesh_meta(stone).is_none(),
        "a rebuilt section clears its released meta"
    );

    // Restoring the cell to stone collapses the section back to the
    // zero-heap uniform form and bumps the revision again.
    world.apply_block_update([3, 3, 3], STONE).unwrap();
    world.rebuild_dirty_bounded(usize::MAX, moved_camera).unwrap();
    assert!(
        world.section_mesh_revision(stone).unwrap() > rev_after_edit,
        "revisions are monotonic across rebuilds"
    );
    assert_eq!(world.mesh_cache.is_uniform_section(stone), Some(true));
    assert_eq!(world.mesh_cache.blocks_heap_bytes(stone), Some(0));

    // Unloading the chunk retires every section bookkeeping entry.
    world.unload_chunk(0, 0);
    for coord in [stone, mixed, water, air] {
        assert!(world.section_mesh_revision(coord).is_none());
        assert!(world.released_mesh_meta(coord).is_none());
        assert!(world.packed_mesh(coord).is_none());
        assert!(world.translucent_order(coord).is_none());
    }
}
