// SPDX-License-Identifier: LGPL-3.0-or-later

use megu_client_mesh::vanilla_block_models::{
    BakedBlockStateCache, VanillaBlockModelCatalog, parse_block_state_name,
};
use megu_client_mesh::{
    Block, BlockShape, CUTOUT, Direction, Layer, Light, Section, TINTED, WATER, WATERLOGGED,
    mesh_section_with_baked_models,
};

fn entry(path: &str, json: &str) -> (String, String) {
    (path.to_owned(), json.to_owned())
}

fn cube_model(texture: &str) -> String {
    format!(
        r##"{{"textures":{{"all":"{texture}"}},"elements":[{{"from":[0,0,0],"to":[16,16,16],"faces":{{"down":{{"texture":"#all","cullface":"down"}},"up":{{"texture":"#all","cullface":"up"}},"north":{{"texture":"#all","cullface":"north"}},"south":{{"texture":"#all","cullface":"south"}},"west":{{"texture":"#all","cullface":"west"}},"east":{{"texture":"#all","cullface":"east"}}}}}}]}}"##
    )
}

#[test]
fn canonical_state_parser_handles_namespace_and_properties() {
    assert_eq!(
        parse_block_state_name(
            "minecraft:oak_stairs[facing=north,half=bottom,shape=straight,waterlogged=false]"
        )
        .unwrap(),
        (
            "minecraft".into(),
            "oak_stairs".into(),
            vec![
                ("facing".into(), "north".into()),
                ("half".into(), "bottom".into()),
                ("shape".into(), "straight".into()),
                ("waterlogged".into(), "false".into()),
            ]
        )
    );
    assert_eq!(
        parse_block_state_name("stone").unwrap(),
        ("minecraft".into(), "stone".into(), vec![])
    );
    assert!(parse_block_state_name("stone[a=1,a=2]").is_err());
}

#[test]
fn variants_match_property_subsets_and_bake_parent_and_rotations() {
    let layers = vec![vec![
        entry(
            "assets/minecraft/blockstates/oak_stairs.json",
            r##"{"variants":{"facing=east":{"model":"minecraft:block/child","x":180,"y":90,"uvlock":true}}}"##,
        ),
        entry(
            "assets/minecraft/models/block/parent.json",
            r##"{"textures":{"side":"minecraft:block/oak_planks"},"elements":[{"from":[0,0,0],"to":[8,8,8],"faces":{"north":{"texture":"#side"}}}]}"##,
        ),
        entry(
            "assets/minecraft/models/block/child.json",
            r##"{"parent":"minecraft:block/parent","textures":{"side":"minecraft:block/oak_stairs"}}"##,
        ),
    ]];
    let catalog = VanillaBlockModelCatalog::new(layers);
    let mut cache = BakedBlockStateCache::new(
        catalog,
        vec!["minecraft:oak_stairs[facing=east,half=top,shape=straight]".into()],
    );
    let mesh = cache.get_or_bake(0).unwrap();
    assert_eq!(mesh.quads.len(), 1);
    assert_eq!(mesh.quads[0].texture, "minecraft:block/oak_stairs");
    assert!(!mesh.is_full_opaque_cube);
    assert!(
        mesh.quads[0]
            .positions
            .iter()
            .any(|position| (position[0] - 0.5).abs() > 0.01 || (position[1] - 0.5).abs() > 0.01)
    );
}

#[test]
fn multipart_applies_every_matching_or_and_pipe_selector() {
    let catalog = VanillaBlockModelCatalog::new(vec![vec![
        entry(
            "assets/minecraft/blockstates/oak_fence.json",
            r##"{"multipart":[{"when":{"OR":[{"north":"true"},{"south":"true"}]},"apply":{"model":"minecraft:block/north"}},{"when":{"AND":[{"north":"true"},{"east":"false"}]},"apply":{"model":"minecraft:block/east"}},{"when":{"west":"false|true"},"apply":{"model":"minecraft:block/west"}}]}"##,
        ),
        entry(
            "assets/minecraft/models/block/north.json",
            r##"{"elements":[{"from":[0,0,0],"to":[4,16,4],"faces":{"north":{"texture":"minecraft:block/north"}}}]}"##,
        ),
        entry(
            "assets/minecraft/models/block/east.json",
            r##"{"elements":[{"from":[4,0,0],"to":[8,16,4],"faces":{"east":{"texture":"minecraft:block/east"}}}]}"##,
        ),
        entry(
            "assets/minecraft/models/block/west.json",
            r##"{"elements":[{"from":[8,0,0],"to":[12,16,4],"faces":{"west":{"texture":"minecraft:block/west"}}}]}"##,
        ),
    ]]);
    let mut cache = BakedBlockStateCache::new(
        catalog,
        vec!["minecraft:oak_fence[north=true,east=false,south=true,west=false]".into()],
    );
    let mesh = cache.get_or_bake(0).unwrap();
    assert_eq!(mesh.quads.len(), 3);
    assert_eq!(mesh.textures.len(), 3);
}

#[test]
fn cross_plant_keeps_tint_and_bakes_forty_five_degree_element_rotation() {
    let catalog = VanillaBlockModelCatalog::new(vec![vec![
        entry(
            "assets/minecraft/blockstates/dandelion.json",
            r##"{"variants":{"":{"model":"minecraft:block/cross"}}}"##,
        ),
        entry(
            "assets/minecraft/models/block/cross.json",
            r##"{"textures":{"cross":"minecraft:block/short_grass"},"elements":[{"from":[6,0,7],"to":[10,16,9],"rotation":{"origin":[8,8,8],"axis":"y","angle":45},"faces":{"north":{"texture":"#cross","tintindex":0},"south":{"texture":"#cross","tintindex":0}}}]}"##,
        ),
    ]]);
    let mut cache = BakedBlockStateCache::new(catalog, vec!["minecraft:dandelion".into()]);
    let mesh = cache.get_or_bake(0).unwrap();
    assert_eq!(mesh.quads.len(), 2);
    assert!(mesh.uses_tint);
    assert_eq!(mesh.quads[0].tintindex, 0);
    assert_eq!(mesh.textures, vec!["minecraft:block/short_grass"]);
    assert!(
        mesh.quads[0]
            .positions
            .iter()
            .any(|position| (position[0] - 0.5).abs() > 0.02)
    );
}

#[test]
fn higher_priority_resource_pack_model_replaces_vanilla_cleanly() {
    let catalog = VanillaBlockModelCatalog::new(vec![
        vec![
            entry(
                "assets/minecraft/blockstates/stone.json",
                r##"{"variants":{"":{"model":"minecraft:block/stone"}}}"##,
            ),
            entry(
                "assets/minecraft/models/block/stone.json",
                &cube_model("minecraft:block/stone"),
            ),
        ],
        vec![entry(
            "assets/minecraft/models/block/stone.json",
            r##"{"elements":[{"from":[0,0,0],"to":[8,16,16],"faces":{"west":{"texture":"pack:block/replacement"}}}]}"##,
        )],
    ]);
    let mut cache = BakedBlockStateCache::new(catalog, vec!["minecraft:stone".into()]);
    let mesh = cache.get_or_bake(0).unwrap();
    assert_eq!(mesh.quads.len(), 1);
    assert_eq!(mesh.textures, vec!["pack:block/replacement"]);
    assert!(!mesh.is_full_opaque_cube);
}

#[test]
fn state_cache_retains_hits_and_misses_and_classifies_full_cubes() {
    let catalog = VanillaBlockModelCatalog::new(vec![vec![
        entry(
            "assets/minecraft/blockstates/stone.json",
            r##"{"variants":{"":{"model":"minecraft:block/stone"}}}"##,
        ),
        entry(
            "assets/minecraft/blockstates/oak_stairs.json",
            r##"{"variants":{"":{"model":"minecraft:block/stairs"}}}"##,
        ),
        entry(
            "assets/minecraft/blockstates/glass.json",
            r##"{"variants":{"":{"model":"minecraft:block/glass"}}}"##,
        ),
        entry(
            "assets/minecraft/models/block/stone.json",
            &cube_model("minecraft:block/stone"),
        ),
        entry(
            "assets/minecraft/models/block/glass.json",
            &cube_model("minecraft:block/glass"),
        ),
        entry(
            "assets/minecraft/models/block/stairs.json",
            r##"{"elements":[{"from":[0,0,0],"to":[16,8,16],"faces":{"north":{"texture":"minecraft:block/oak_planks"}}}]}"##,
        ),
    ]]);
    let states = vec![
        "minecraft:stone".into(),
        "minecraft:oak_stairs".into(),
        "missing".into(),
        "minecraft:glass".into(),
    ];
    let mut unclassified = BakedBlockStateCache::new(catalog.clone(), states.clone());
    assert!(!unclassified.get_or_bake(0).unwrap().is_full_opaque_cube);
    let mut cache =
        BakedBlockStateCache::new_with_greedy_material_check(catalog, states, |state, quad| {
            state == 0 && quad.texture == "minecraft:block/stone"
        });
    assert!(cache.get_or_bake(0).unwrap().is_full_opaque_cube);
    assert!(cache.get_or_bake(0).unwrap().is_full_opaque_cube);
    assert!(!cache.get_or_bake(1).unwrap().is_full_opaque_cube);
    assert!(cache.is_non_cube_baked(1));
    assert!(cache.get_or_bake(2).is_none());
    assert!(cache.get_or_bake(2).is_none());
    assert!(!cache.get_or_bake(3).unwrap().is_full_opaque_cube);
    let stats = cache.stats();
    assert_eq!(stats.loaded_blockstates, 3);
    assert_eq!(stats.baked_hits, 3);
    assert_eq!(stats.baked_misses, 1);
    assert_eq!(stats.full_cube_fast_paths, 1);

    let mut section = Section::filled(Block::AIR, Light::new(0, 15));
    section.set(0, 0, 0, Block::opaque_cube(0), Light::new(0, 15));
    let (greedy, baked) = mesh_section_with_baked_models(&section.view(), [0; 3], &mut cache);
    assert_eq!(greedy.len(), 6);
    assert!(baked.is_empty());
}

#[test]
fn section_bakes_position_weighted_faces_and_preserves_water_and_face_tint() {
    let catalog = VanillaBlockModelCatalog::new(vec![vec![
        entry(
            "assets/minecraft/blockstates/test.json",
            r##"{"multipart":[
                {"when":{"waterlogged":"true"},"apply":[{"model":"block/tinted","weight":2},{"model":"block/tinted","y":90}]},
                {"apply":{"model":"block/untinted"}}
            ]}"##,
        ),
        entry(
            "assets/minecraft/models/block/tinted.json",
            r##"{"elements":[{"from":[0,0,0],"to":[16,8,16],"faces":{"west":{"texture":"minecraft:block/grass","tintindex":0,"cullface":"west"}}}]}"##,
        ),
        entry(
            "assets/minecraft/models/block/untinted.json",
            r##"{"ambientocclusion":false,"elements":[{"from":[0,0,0],"to":[16,8,16],"faces":{"up":{"texture":"minecraft:block/dirt"}}}]}"##,
        ),
    ]]);
    let mut cache = BakedBlockStateCache::new(catalog, vec!["test[waterlogged=true]".into()]);
    let block = Block {
        flags: CUTOUT | WATERLOGGED | TINTED,
        tint: 0x7cbd6b,
        ..Block::AIR
    }
    .with_shape(BlockShape::SlabBottom);
    assert_eq!(block.shape(), BlockShape::SlabBottom);
    let light = Light::new(3, 12);
    let mut section = Section::filled(Block::AIR, light);
    section.set(2, 4, 6, block, light);
    let legacy_shapes = megu_client_mesh::mesh_section(&section.view());
    assert!(legacy_shapes.iter().any(|face| face.layer == Layer::Cutout
        && face.direction == Direction::Up
        && face.positions[0][1] == 4.5));
    assert!(legacy_shapes.iter().any(|face| face.fluid_kind == WATER));

    let mut directions = std::collections::HashSet::new();
    for origin in (0..20).map(|n| [16 * n, -32, 48]) {
        let (legacy, baked) = mesh_section_with_baked_models(&section.view(), origin, &mut cache);
        assert!(!legacy.is_empty());
        assert!(legacy.iter().all(|face| face.fluid_kind == WATER));
        assert_eq!(baked.len(), 2);
        assert_eq!(baked[0].light, light);
        assert!(baked[0].ambient_occlusion);
        assert_eq!(baked[0].biome_tint, Some(block.tint));
        assert_eq!(baked[0].quad.texture, "minecraft:block/grass");
        directions.insert(baked[0].quad.cullface.unwrap());
        assert_eq!(baked[1].biome_tint, None);
        // Multipart ambient occlusion follows the first matching selector's
        // model rather than varying per concatenated part.
        assert!(baked[1].ambient_occlusion);
        assert_eq!(baked[1].quad.texture, "minecraft:block/dirt");
        assert!(baked[1].quad.positions.iter().all(|point| point[1] == 4.5));
    }
    assert_eq!(directions.len(), 2);
}
