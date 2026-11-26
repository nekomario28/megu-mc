// SPDX-License-Identifier: LGPL-3.0-or-later

use std::sync::Arc;
use std::time::Duration;

use megu_client::protocol::DimensionMeta;
use megu_client::world::{
    enrich_state_names_from_blocks_json, BlockAtlasSpriteLookup, BlockTable, WorldState,
};
use megu_client_gpu::mesh_pool::{MeshJob, MeshJobPool, MeshResult};
use megu_client_gpu::renderer::{request_gpu_device, ComposedFrame, GpuRenderer, OFFSCREEN_FORMAT};
use megu_client_gpu::subworld_render::SubWorldRenderBank;
use megu_client_gpu::Camera;
use megu_client_mesh::atlas::{build_mipmaps_linear, AtlasBuilder};
use megu_client_mesh::vanilla_block_models::VanillaBlockModelCatalog;
use megu_client_mesh::{Block, BlockShape, Light, MeshCache, Section, SectionCoord, CUTOUT};

const STONE: u32 = 1;
const STAIRS: u32 = 2;
const FENCE: u32 = 3;
const COORD: SectionCoord = SectionCoord(0, 0, 0);

fn entry(path: &str, json: &str) -> (String, String) {
    (path.to_owned(), json.to_owned())
}

fn cube_model(texture: &str) -> String {
    format!(
        r##"{{"textures":{{"all":"{texture}"}},"elements":[{{"from":[0,0,0],"to":[16,16,16],"faces":{{"down":{{"texture":"#all","cullface":"down"}},"up":{{"texture":"#all","cullface":"up"}},"north":{{"texture":"#all","cullface":"north"}},"south":{{"texture":"#all","cullface":"south"}},"west":{{"texture":"#all","cullface":"west"}},"east":{{"texture":"#all","cullface":"east"}}}}}}]}}"##
    )
}

fn synthetic_catalog() -> VanillaBlockModelCatalog {
    VanillaBlockModelCatalog::new(vec![vec![
        entry(
            "assets/minecraft/blockstates/stone.json",
            r##"{"variants":{"":{"model":"minecraft:block/stone"}}}"##,
        ),
        entry(
            "assets/minecraft/blockstates/oak_stairs.json",
            r##"{"variants":{"facing=east,half=bottom,shape=straight":{"model":"minecraft:block/oak_stairs"}}}"##,
        ),
        entry(
            "assets/minecraft/blockstates/oak_fence.json",
            r##"{"multipart":[{"apply":{"model":"minecraft:block/fence_post"}},{"when":{"north":"true"},"apply":{"model":"minecraft:block/fence_north"}},{"when":{"south":"true"},"apply":{"model":"minecraft:block/fence_south"}}]}"##,
        ),
        entry(
            "assets/minecraft/models/block/stone.json",
            &cube_model("minecraft:block/stone"),
        ),
        entry(
            "assets/minecraft/models/block/oak_stairs.json",
            r##"{"textures":{"side":"minecraft:block/oak_planks"},"elements":[{"from":[0,0,0],"to":[16,8,16],"faces":{"up":{"texture":"#side"},"north":{"texture":"#side"}}},{"from":[8,8,0],"to":[16,16,16],"faces":{"up":{"texture":"#side"},"east":{"texture":"#side"}}}]}"##,
        ),
        entry(
            "assets/minecraft/models/block/fence_post.json",
            r##"{"textures":{"side":"minecraft:block/oak_planks"},"elements":[{"from":[6,0,6],"to":[10,16,10],"faces":{"up":{"texture":"#side"},"north":{"texture":"#side"},"east":{"texture":"#side"}}}]}"##,
        ),
        entry(
            "assets/minecraft/models/block/fence_north.json",
            r##"{"textures":{"side":"minecraft:block/oak_planks"},"elements":[{"from":[7,6,0],"to":[9,9,6],"faces":{"up":{"texture":"#side"},"north":{"texture":"#side"}}}]}"##,
        ),
        entry(
            "assets/minecraft/models/block/fence_south.json",
            r##"{"textures":{"side":"minecraft:block/oak_planks"},"elements":[{"from":[7,12,10],"to":[9,15,16],"faces":{"up":{"texture":"#side"},"south":{"texture":"#side"}}}]}"##,
        ),
    ]])
}

fn state_names() -> Arc<[String]> {
    vec![
        "minecraft:air".to_owned(),
        "minecraft:stone".to_owned(),
        "minecraft:oak_stairs[facing=east,half=bottom,shape=straight]".to_owned(),
        "minecraft:oak_fence[north=true,east=false,south=true,west=false]".to_owned(),
    ]
    .into()
}

fn stone_block() -> Block {
    Block::opaque_cube(STONE)
}

fn stair_block() -> Block {
    Block {
        state_id: STAIRS,
        flags: CUTOUT,
        ..Block::AIR
    }
    .with_shape(BlockShape::Stair)
}

fn fence_block() -> Block {
    Block {
        state_id: FENCE,
        flags: CUTOUT,
        ..Block::AIR
    }
    .with_shape(BlockShape::FencePost)
}

fn cross_plant_block() -> Block {
    Block {
        state_id: 4,
        flags: CUTOUT,
        ..Block::AIR
    }
    .with_shape(BlockShape::CrossPlant)
}

fn test_section() -> Section {
    let light = Light::new(0, 15);
    let mut section = Section::filled(Block::AIR, light);
    section.set(2, 2, 2, stair_block(), light);
    section.set(6, 2, 2, fence_block(), light);
    section.set(10, 2, 2, stone_block(), light);
    section.set(13, 2, 2, cross_plant_block(), light);
    section
}

fn snapshot_input() -> megu_client_mesh::SectionMeshInput {
    snapshot_input_for(test_section())
}

fn snapshot_input_for(section: Section) -> megu_client_mesh::SectionMeshInput {
    let mut cache = MeshCache::default();
    assert!(cache.insert(COORD, section));
    cache
        .snapshot_section_input(COORD, 7)
        .expect("inserted section snapshots")
}

fn cube_classified_fallback_section() -> Section {
    let light = Light::new(0, 15);
    let mut section = Section::filled(Block::AIR, light);
    section.set(2, 2, 2, Block::opaque_cube(STAIRS), light);
    section.set(6, 2, 2, Block::opaque_cube(FENCE), light);
    section.set(10, 2, 2, stone_block(), light);
    section
}

fn stone_only_section() -> Section {
    let light = Light::new(0, 15);
    let mut section = Section::filled(Block::AIR, light);
    section.set(10, 2, 2, stone_block(), light);
    section
}

fn wait_for_mesh(pool: &MeshJobPool) -> megu_client_mesh::BuiltSectionMesh {
    let mut built = None;
    for _ in 0..500 {
        pool.drain(|result| {
            if let MeshResult::SectionMesh { built: mesh } = result {
                built = Some(mesh);
            }
        });
        if built.is_some() {
            break;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    built.expect("worker returned a section mesh")
}

fn drain_baked_faces(pool: &MeshJobPool) -> Vec<megu_client_mesh::BakedTerrainFace> {
    let mut faces = Vec::new();
    pool.drain_baked(|result| faces.extend(result.faces));
    faces
}

fn block_table() -> BlockTable {
    let json = r##"{
        "blocks": [
            ["minecraft:air", 0, 1],
            ["minecraft:stone", 1, 1],
            ["minecraft:oak_stairs[facing=east,half=bottom,shape=straight]", 2, 1],
            ["minecraft:oak_fence[north=true,east=false,south=true,west=false]", 3, 1],
            ["minecraft:poppy", 4, 1]
        ],
        "flags": [1, 8, 8, 8, 8],
        "full_sides": [0, 63, 0, 0, 0],
        "shapes": [{"min": [0.0, 0.0, 0.0], "max": [1.0, 1.0, 1.0]}],
        "collision_shapes": [[], [0], [0], [0], []]
    }"##;
    BlockTable::from_json_bytes(json.as_bytes()).expect("synthetic block table")
}

fn sprite_lookup() -> BlockAtlasSpriteLookup {
    Arc::new(|texture: &str| {
        matches!(
            texture,
            "minecraft:block/stone" | "minecraft:block/oak_planks"
        )
        .then_some(0)
    })
}

fn world_with_catalog() -> WorldState {
    let mut world = WorldState::new(DimensionMeta::default(), block_table());
    assert!(!world.has_block_model_catalog());
    world.set_block_model_catalog(Arc::new(synthetic_catalog()), sprite_lookup());
    assert!(world.has_block_model_catalog());
    assert!(world.block_model_catalog().is_some());
    world
}

#[test]
fn world_state_emits_baked_stair_and_fence_geometry_when_catalog_is_attached() {
    let mut world = world_with_catalog();
    assert!(world.mesh_cache.insert(COORD, test_section()));

    let rebuilt = world
        .rebuild_dirty_bounded(8, [8.0, 8.0, 8.0])
        .expect("synchronous world rebuild");
    assert_eq!(rebuilt, 1);

    let baked = world
        .baked_terrain_faces(COORD)
        .expect("shaped baked sidecar retained");
    let stair_faces = baked.iter().filter(|face| face.state_id == STAIRS).count();
    let fence_faces = baked.iter().filter(|face| face.state_id == FENCE).count();
    assert_eq!(
        stair_faces, 4,
        "two stair cuboids emit their authored faces"
    );
    assert_eq!(
        fence_faces, 7,
        "post plus north/south fence connectors emit their authored faces"
    );
    assert!(
        baked.iter().all(|face| face.state_id != STONE),
        "certified full-cube stone stayed on the greedy fast path"
    );
    assert!(
        baked
            .iter()
            .all(|face| face.quad.texture == "minecraft:block/oak_planks"),
        "baked faces resolve the model texture reference"
    );
    assert!(
        world
            .packed_mesh(COORD)
            .is_some_and(|packed| !packed.is_empty()),
        "stone remained represented in the legacy packed stream"
    );
    assert!(
        world
            .unpacked_terrain_quads(COORD)
            .is_some_and(|quads| !quads.is_empty()),
        "catalog-missing cross-plant geometry stayed in the unpacked sidecar"
    );
}

fn world_with_inserted(section: Section, catalog: bool) -> WorldState {
    let mut world = if catalog {
        world_with_catalog()
    } else {
        WorldState::new(DimensionMeta::default(), block_table())
    };
    assert!(world.mesh_cache.insert(COORD, section));
    assert_eq!(
        world
            .rebuild_dirty_bounded(8, [8.0, 8.0, 8.0])
            .expect("synchronous world rebuild"),
        1
    );
    world
}

#[test]
fn world_state_stone_fast_path_is_identical_with_and_without_catalog() {
    let with_catalog = world_with_inserted(stone_only_section(), true);
    let without_catalog = world_with_inserted(stone_only_section(), false);

    assert!(with_catalog.baked_terrain_faces(COORD).is_none());
    assert_eq!(
        with_catalog.packed_mesh(COORD),
        without_catalog.packed_mesh(COORD)
    );
    assert_eq!(
        with_catalog.direction_buckets(COORD),
        without_catalog.direction_buckets(COORD)
    );
}

#[test]
fn world_state_without_catalog_uses_plain_fallback_and_no_baked_sidecar() {
    let world = world_with_inserted(cube_classified_fallback_section(), false);

    assert!(!world.has_block_model_catalog());
    assert!(world.block_model_catalog().is_none());
    assert!(world.baked_terrain_faces(COORD).is_none());
    assert!(
        world
            .packed_mesh(COORD)
            .is_some_and(|packed| !packed.is_empty()),
        "plain fallback still packs shaped/cube terrain"
    );
}

#[test]
fn mesh_pool_emits_baked_stair_and_fence_geometry_when_catalog_is_attached() {
    let mut pool = MeshJobPool::new(1);
    pool.set_block_model_catalog(Some(Arc::new(synthetic_catalog())), state_names());
    assert!(pool.try_submit(MeshJob::SectionMesh {
        input: snapshot_input(),
    }));

    let built = wait_for_mesh(&pool);
    let baked = drain_baked_faces(&pool);

    assert_eq!(built.coord, COORD);
    assert_eq!(built.revision, 7);
    assert!(!built.opaque_packed.is_empty());
    let stair_faces = baked.iter().filter(|face| face.state_id == STAIRS).count();
    let fence_faces = baked.iter().filter(|face| face.state_id == FENCE).count();
    assert_eq!(
        stair_faces, 4,
        "two stair cuboids emit their authored faces"
    );
    assert_eq!(
        fence_faces, 7,
        "post plus north/south fence connectors emit their authored faces"
    );
    assert!(
        baked.iter().all(|face| face.state_id != STONE),
        "certified full-cube stone stayed on the greedy fast path"
    );
    assert!(
        baked
            .iter()
            .all(|face| face.quad.texture == "minecraft:block/oak_planks"),
        "baked faces resolve the model texture reference"
    );
    assert!(
        baked.iter().any(|face| face
            .quad
            .positions
            .iter()
            .any(|position| position.iter().any(|axis| axis.fract().abs() > 0.001))),
        "baked sidecar preserves fractional shaped geometry"
    );
}

#[test]
fn mesh_pool_stone_fast_path_is_identical_with_and_without_catalog() {
    let mut baked_pool = MeshJobPool::new(1);
    baked_pool.set_block_model_catalog(Some(Arc::new(synthetic_catalog())), state_names());
    assert!(baked_pool.try_submit(MeshJob::SectionMesh {
        input: snapshot_input_for(stone_only_section()),
    }));
    let with_catalog = wait_for_mesh(&baked_pool);
    assert!(
        drain_baked_faces(&baked_pool).is_empty(),
        "certified stone cube emitted no replacement faces"
    );

    let plain_pool = MeshJobPool::new(1);
    assert!(plain_pool.try_submit(MeshJob::SectionMesh {
        input: snapshot_input_for(stone_only_section()),
    }));
    let without_catalog = wait_for_mesh(&plain_pool);

    assert_eq!(with_catalog.opaque_packed, without_catalog.opaque_packed);
    assert_eq!(with_catalog.occluder_quads, without_catalog.occluder_quads);
    assert_eq!(
        with_catalog.visibility_mask,
        without_catalog.visibility_mask
    );
}

#[test]
fn mesh_pool_without_catalog_uses_plain_fallback_and_emits_no_baked_sidecar() {
    let pool = MeshJobPool::new(1);
    assert!(pool.try_submit(MeshJob::SectionMesh {
        input: snapshot_input(),
    }));

    let built = wait_for_mesh(&pool);
    assert!(!built.opaque_packed.is_empty());
    assert!(drain_baked_faces(&pool).is_empty());
}

#[test]
fn enrich_state_names_expands_properties_in_exact_vanilla_cartesian_order() {
    let blocks_json = r##"[
        {"name":"oak_stairs","minStateId":2,"maxStateId":17,"states":[
            {"name":"facing","type":"enum","values":["north","east"],"num_values":2},
            {"name":"half","type":"enum","values":["bottom","top"],"num_values":2},
            {"name":"shape","type":"enum","values":["straight","inner_left"],"num_values":2},
            {"name":"waterlogged","type":"bool","values":["false","true"],"num_values":2}
        ]},
        {"name":"oak_fence","minStateId":18,"maxStateId":49,"states":[
            {"name":"north","type":"bool","values":["false","true"],"num_values":2},
            {"name":"east","type":"bool","values":["false","true"],"num_values":2},
            {"name":"south","type":"bool","values":["false","true"],"num_values":2},
            {"name":"west","type":"bool","values":["false","true"],"num_values":2},
            {"name":"waterlogged","type":"bool","values":["false","true"],"num_values":2}
        ]},
        {"name":"wheat","minStateId":50,"maxStateId":57,"states":[
            {"name":"age","type":"int","values":[0,1,2,3,4,5,6,7],"num_values":8}
        ]}
    ]"##;
    let mut names = vec![String::new(); 58];
    for (index, name) in names.iter_mut().enumerate() {
        *name = format!("state{index}");
    }

    let rewritten = enrich_state_names_from_blocks_json(&mut names, blocks_json.as_bytes())
        .expect("synthetic blocks manifest expands");

    assert_eq!(rewritten, 56);
    assert_eq!(names[0], "state0");
    assert_eq!(
        names[2],
        "minecraft:oak_stairs[facing=north,half=bottom,shape=straight,waterlogged=false]"
    );
    assert_eq!(
        names[3],
        "minecraft:oak_stairs[facing=north,half=bottom,shape=straight,waterlogged=true]"
    );
    assert_eq!(
        names[4],
        "minecraft:oak_stairs[facing=north,half=bottom,shape=inner_left,waterlogged=false]"
    );
    assert_eq!(
        names[6],
        "minecraft:oak_stairs[facing=north,half=top,shape=straight,waterlogged=false]"
    );
    assert_eq!(
        names[10],
        "minecraft:oak_stairs[facing=east,half=bottom,shape=straight,waterlogged=false]"
    );
    assert_eq!(
        names[18],
        "minecraft:oak_fence[east=false,north=false,south=false,waterlogged=false,west=false]"
    );
    assert_eq!(
        names[19],
        "minecraft:oak_fence[east=false,north=false,south=false,waterlogged=true,west=false]"
    );
    assert_eq!(
        names[20],
        "minecraft:oak_fence[east=false,north=false,south=false,waterlogged=false,west=true]"
    );
    assert_eq!(
        names[24],
        "minecraft:oak_fence[east=false,north=false,south=true,waterlogged=false,west=true]"
    );
    assert_eq!(
        names[34],
        "minecraft:oak_fence[east=false,north=true,south=false,waterlogged=false,west=false]"
    );
    assert_eq!(names[50], "minecraft:wheat[age=0]");
    assert_eq!(names[57], "minecraft:wheat[age=7]");
}

fn tiny_block_atlas(renderer: &mut GpuRenderer) {
    let mut builder = AtlasBuilder::for_blocks();
    let brown = [132, 84, 38, 255].repeat(16 * 16);
    let gray = [120, 120, 124, 255].repeat(16 * 16);
    let poppy = (0usize..16 * 16)
        .flat_map(|index| {
            if (index / 16).abs_diff(8) <= 1 || (index % 16).abs_diff(8) <= 1 {
                [196, 24, 24, 255]
            } else {
                [0, 0, 0, 0]
            }
        })
        .collect::<Vec<_>>();
    builder
        .add("minecraft:block/oak_planks", 16, 16, brown)
        .expect("oak atlas sprite");
    builder
        .add("minecraft:block/stone", 16, 16, gray)
        .expect("stone atlas sprite");
    builder
        .add("minecraft:block/poppy", 16, 16, poppy)
        .expect("cutout poppy atlas sprite");
    let atlas = builder.build().expect("tiny block atlas");
    let mips = build_mipmaps_linear(&atlas, atlas.max_mip_levels(), None).expect("atlas mips");
    renderer
        .upload_block_atlas(&atlas, &mips)
        .expect("upload atlas");
    renderer
        .set_block_state_sprites(&[
            (STONE, "minecraft:block/stone"),
            (STAIRS, "minecraft:block/oak_planks"),
            (FENCE, "minecraft:block/oak_planks"),
            (4, "minecraft:block/poppy"),
        ])
        .expect("state sprite table");
}

fn render_baked_frame(renderer: &mut GpuRenderer, camera: &Camera) -> Vec<u8> {
    let mut bank = SubWorldRenderBank::new();
    let atmosphere = megu_client_gpu::atmosphere::BiomeAtmosphereBlender::new(
        megu_client_gpu::atmosphere::AtmosphereParams::default(),
    );
    let fades = megu_client_gpu::atmosphere::ChunkFadeTracker::new();
    renderer
        .render_offscreen_composed_frame(
            96,
            96,
            camera,
            &mut ComposedFrame {
                subworlds: &mut bank,
                lights: &[],
                atmosphere: &atmosphere,
                chunk_fades: &fades,
                ui_rgba: None,
                world_revision: 1,
                world_time_ticks: 6000,
            },
        )
        .expect("shaped terrain visual frame")
}

fn changed_pixels(baseline: &[u8], changed: &[u8]) -> usize {
    let (baseline, baseline_tail) = baseline.as_chunks::<4>();
    let (changed, changed_tail) = changed.as_chunks::<4>();
    assert!(baseline_tail.is_empty() && changed_tail.is_empty());
    baseline
        .iter()
        .zip(changed.iter())
        .filter(|(before, after)| before != after)
        .count()
}

#[test]
fn gpu_renderer_uploads_and_draws_baked_shaped_terrain_faces() {
    let Ok(ctx) = request_gpu_device(megu_client_mesh::RenderBackendPreference::Auto, None, None)
    else {
        eprintln!("SKIP shaped terrain GPU test: no wgpu adapter available");
        return;
    };
    let mut renderer = GpuRenderer::new(ctx, OFFSCREEN_FORMAT);
    tiny_block_atlas(&mut renderer);
    let camera = Camera {
        position: [8.0, 8.0, 24.0],
        yaw_deg: 0.0,
        pitch_deg: -8.0,
        far: 128.0,
        ..Camera::default()
    };
    let baseline = render_baked_frame(&mut renderer, &camera);

    let mut world = world_with_catalog();
    assert!(world.mesh_cache.insert(COORD, test_section()));
    assert_eq!(
        world
            .rebuild_dirty_bounded(8, [8.0, 8.0, 8.0])
            .expect("world mesh rebuild"),
        1
    );
    assert!(world
        .baked_terrain_faces(COORD)
        .is_some_and(|faces| !faces.is_empty()));

    assert_eq!(renderer.shaped_terrain_entry_count(), 0);
    assert!(renderer.update_live_terrain(&world) > 0);
    assert!(
        renderer.shaped_terrain_entry_count() > 0,
        "baked stair/fence sidecar produced shaped draw entries"
    );
    assert_eq!(
        renderer.shaped_terrain_vertex_count(),
        60,
        "11 baked stair/fence quads plus four unpacked cross-plant quads stage 15 quads/60 vertices"
    );
    let shaped = render_baked_frame(&mut renderer, &camera);
    assert!(
        renderer.shaped_terrain_gpu_entry_count() > 0,
        "shaped entries uploaded into GPU-side vertex/index buffers"
    );
    let changed = changed_pixels(&baseline, &shaped);
    assert!(
        changed > 16,
        "shaped baked terrain must rasterize visible pixels, got {changed}"
    );
}
