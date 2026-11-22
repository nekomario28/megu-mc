// SPDX-License-Identifier: LGPL-3.0-or-later
//! Clean-room synthetic fixtures for bounded CEM/CTM resource-pack parsing.

use mc_spine_server_files::optifine_compat::{
    model_fingerprint, parse_ctm_properties, parse_emissive_properties, parse_jem, parse_jpm,
    resolve_jem_with_jpm, CtmConnect, CtmLayer, CtmMethod, CtmSymmetry, CtmTile,
    OptifineCompatError, MAX_ANIMATIONS, MAX_BOXES_PER_PART, MAX_CTM_TILES, MAX_PARTS,
    MAX_SUBMODEL_DEPTH,
};
use std::collections::BTreeMap;

const JEM: &str = r#"{
    "texture": "entity/example.png",
    "textureSize": [128, 128],
    "shadowSize": 0.5,
    "models": [{
        "part": "body",
        "id": "main",
        "model": "body.jpm",
        "invertAxis": "xy",
        "translate": [1, 2, 3],
        "rotate": [0.1, 0.2, 0.3],
        "scale": 1.25,
        "attach": true,
        "animations": [{
            "body.rx": "sin(limb_swing) * 0.5",
            "head.rx": "0"
        }]
    }]
}"#;

const JPM: &str = r#"{
    "texture": "parts/body.png",
    "textureSize": [64, 64],
    "invertAxis": "z",
    "translate": [4, 5, 6],
    "rotate": [0.4, 0.5, 0.6],
    "mirrorTexture": "uv",
    "boxes": [
        {
            "coordinates": [0, 1, 2, 3, 4, 5],
            "textureOffset": [1, 2],
            "sizeAdd": 0.25
        },
        {
            "coordinates": [1, 2, 3, 4, 5, 6],
            "uvNorth": [0, 0, 4, 4],
            "uvSouth": [4, 0, 8, 4],
            "uvEast": [8, 0, 12, 4],
            "uvWest": [12, 0, 16, 4],
            "uvUp": [0, 4, 4, 8],
            "uvDown": [4, 4, 8, 8]
        }
    ],
    "submodels": [{
        "id": "tail",
        "translate": [1, 0, 0],
        "boxes": [{"coordinates": [0, 0, 0, 1, 1, 2], "textureOffset": [0, 8]}],
        "submodels": [{
            "id": "tip",
            "boxes": [{"coordinates": [0, 0, 0, 1, 1, 1], "textureOffset": [4, 8]}]
        }]
    }]
}"#;

fn resolved_fixture() -> mc_spine_server_files::optifine_compat::ResolvedEntityModel {
    let mut models = BTreeMap::new();
    models.insert("body.jpm".to_owned(), parse_jpm(JPM).unwrap());
    resolve_jem_with_jpm(&parse_jem(JEM).unwrap(), &models).unwrap()
}

#[test]
fn jem_and_jpm_resolve_nested_models_and_uvs() {
    let jem = parse_jem(JEM).unwrap();
    let jpm = parse_jpm(JPM).unwrap();
    assert_eq!(jem.texture.as_deref(), Some("entity/example.png"));
    assert_eq!(jem.texture_size, [128, 128]);
    assert_eq!(jem.shadow_size, Some(0.5));
    assert_eq!(jem.models[0].model.as_deref(), Some("body.jpm"));
    assert!(jem.models[0].attach);
    assert_eq!(jem.models[0].scale, 1.25);
    assert_eq!(jpm.texture_size, [64, 64]);
    assert_eq!(jpm.mirror_texture, "uv");
    assert_eq!(jpm.submodels[0].submodels[0].id.as_deref(), Some("tip"));

    let model = resolved_fixture();
    let body = &model.models[0];
    assert_eq!(body.part, "body");
    assert_eq!(body.id.as_deref(), Some("main"));
    assert_eq!(body.texture.as_deref(), Some("parts/body.png"));
    assert_eq!(body.texture_size, [64, 64]);
    assert_eq!(body.invert_axis, "xy");
    assert_eq!(body.translate, [1.0, 2.0, 3.0]);
    assert_eq!(body.rotate, [0.1, 0.2, 0.3]);
    assert!(body.attach);
    assert_eq!(body.mirror_texture, "uv");
    assert_eq!(body.boxes.len(), 2);
    assert_eq!(body.boxes[0].texture_offset, Some([1.0, 2.0]));
    assert_eq!(body.boxes[0].size_add, 0.25);
    let faces = &body.boxes[1];
    assert_eq!(faces.uv_north, Some([0.0, 0.0, 4.0, 4.0]));
    assert_eq!(faces.uv_south, Some([4.0, 0.0, 8.0, 4.0]));
    assert_eq!(faces.uv_east, Some([8.0, 0.0, 12.0, 4.0]));
    assert_eq!(faces.uv_west, Some([12.0, 0.0, 16.0, 4.0]));
    assert_eq!(faces.uv_up, Some([0.0, 4.0, 4.0, 8.0]));
    assert_eq!(faces.uv_down, Some([4.0, 4.0, 8.0, 8.0]));
    assert_eq!(body.submodels.len(), 1);
    assert_eq!(body.submodels[0].id.as_deref(), Some("tail"));
    assert_eq!(body.submodels[0].submodels[0].id.as_deref(), Some("tip"));
    assert_eq!(body.submodels[0].submodels[0].boxes.len(), 1);
    assert_eq!(body.animations[0]["body.rx"], "sin(limb_swing) * 0.5");
    assert_eq!(body.animations[0]["head.rx"], "0");
}

#[test]
fn fingerprint_is_stable_across_parses_and_json_key_order() {
    let expected = 15_242_115_270_945_292_707u64;
    assert_eq!(model_fingerprint(&resolved_fixture()), expected);
    let alternate = r#"{
        "models": [{
            "animations": [{"head.rx": "0", "body.rx": "sin(limb_swing) * 0.5"}],
            "attach": true, "scale": 1.25,
            "rotate": [0.1, 0.2, 0.3], "translate": [1, 2, 3],
            "invertAxis": "xy", "model": "body.jpm", "id": "main", "part": "body"
        }],
        "shadowSize": 0.5, "textureSize": [128, 128],
        "texture": "entity/example.png"
    }"#;
    let mut lookup = BTreeMap::new();
    lookup.insert("body.jpm".to_owned(), parse_jpm(JPM).unwrap());
    let reordered = resolve_jem_with_jpm(&parse_jem(alternate).unwrap(), &lookup).unwrap();
    assert_eq!(reordered, resolved_fixture());
    assert_eq!(model_fingerprint(&reordered), expected);
}

#[test]
fn ctm_standard_and_compact_expand_exact_tiles_and_normalize_ids() {
    let rule = parse_ctm_properties(
        "method=ctm\ntiles=0-46\nmatchBlocks=stone minecraft:oak_planks:axis=y\n\
         matchTiles=block/glass minecraft:block/stone\nconnect=tile\nfaces=top sides\n\
         biomes=!forest plains\nheights=-64--1 0-255\nsymmetry=opposite\n\
         tintIndex=2\ntintBlock=minecraft:grass_block\nlayer=cutout_mipped\n",
    )
    .unwrap();
    assert_eq!(rule.method, CtmMethod::Ctm);
    assert_eq!(rule.tiles.len(), 47);
    assert_eq!(rule.tiles[0], CtmTile::Index(0));
    assert_eq!(rule.tiles[46], CtmTile::Index(46));
    assert_eq!(
        rule.match_blocks,
        ["minecraft:stone", "minecraft:oak_planks:axis=y"]
    );
    assert_eq!(
        rule.match_tiles,
        ["minecraft:block/glass", "minecraft:block/stone"]
    );
    assert_eq!(rule.connect, CtmConnect::Tile);
    assert_eq!(rule.faces, 0b01_1111);
    assert!(rule.biomes_inverted);
    assert_eq!(rule.biomes, ["minecraft:forest", "minecraft:plains"]);
    assert_eq!(rule.heights.len(), 2);
    assert_eq!(
        (rule.heights[0].min, rule.heights[0].max),
        (Some(-64), Some(-1))
    );
    assert_eq!(rule.symmetry, Some(CtmSymmetry::Opposite));
    assert_eq!(rule.tint_index, Some(2));
    assert_eq!(rule.tint_block.as_deref(), Some("minecraft:grass_block"));
    assert_eq!(rule.layer, Some(CtmLayer::CutoutMipped));

    let compact =
        parse_ctm_properties("method=ctm_compact\ntiles=0-4\nfaces=bottom,north\n").unwrap();
    assert_eq!(compact.method, CtmMethod::CtmCompact);
    assert_eq!(
        compact.tiles,
        (0..5).map(CtmTile::Index).collect::<Vec<_>>()
    );
    assert_eq!(compact.faces, 0b10_0001);
}

#[test]
fn random_weights_skip_tiles_and_sprite_paths() {
    let rule = parse_ctm_properties(
        "method=random\ntiles=0-2 <skip> textures/stone\nweights=1 2 3 4 5\n\
         faces=north south east west bottom\nsymmetry=all\nlayer=translucent\n",
    )
    .unwrap();
    assert_eq!(rule.method, CtmMethod::Random);
    assert_eq!(rule.tiles.len(), 5);
    assert_eq!(rule.tiles[3], CtmTile::Skip);
    assert_eq!(rule.tiles[4], CtmTile::Sprite("textures/stone".to_owned()));
    assert_eq!(rule.weights, Some(vec![1, 2, 3, 4, 5]));
    assert_eq!(rule.faces, 0b10_1111);
    assert_eq!(rule.symmetry, Some(CtmSymmetry::All));
    assert_eq!(rule.layer, Some(CtmLayer::Translucent));
}

#[test]
fn repeat_and_overlay_accept_explicit_tile_ranges() {
    let repeat =
        parse_ctm_properties("method=repeat\nwidth=12\nheight=2\ntiles=0-11 16-27\n").unwrap();
    assert_eq!(repeat.method, CtmMethod::Repeat);
    assert_eq!(repeat.width, Some(12));
    assert_eq!(repeat.height, Some(2));
    assert_eq!(repeat.tiles.len(), 24);
    assert_eq!(repeat.tiles[11], CtmTile::Index(11));
    assert_eq!(repeat.tiles[12], CtmTile::Index(16));
    assert_eq!(repeat.tiles[23], CtmTile::Index(27));

    let overlay = parse_ctm_properties("method=overlay\ntiles=0-16\nfaces=all\n").unwrap();
    assert_eq!(overlay.method, CtmMethod::Overlay);
    assert_eq!(overlay.tiles.len(), 17);
    assert_eq!(overlay.faces, 0b11_1111);
}

#[test]
fn remaining_method_variants_are_deterministic() {
    for (method, expected, count) in [
        ("horizontal", CtmMethod::Horizontal, 4),
        ("vertical", CtmMethod::Vertical, 4),
        ("horizontal+vertical", CtmMethod::HorizontalVertical, 7),
        ("vertical+horizontal", CtmMethod::VerticalHorizontal, 7),
        ("top", CtmMethod::Top, 1),
        ("fixed", CtmMethod::Fixed, 1),
        ("overlay_ctm", CtmMethod::OverlayCtm, 47),
        ("overlay_fixed", CtmMethod::OverlayFixed, 1),
    ] {
        let tiles = format!("0-{}", count - 1);
        let rule = parse_ctm_properties(&format!("method={method}\ntiles={tiles}\n")).unwrap();
        assert_eq!(rule.method, expected);
        assert_eq!(rule.tiles.len(), count);
    }
    for (method, expected) in [
        ("overlay_random", CtmMethod::OverlayRandom),
        ("overlay_repeat", CtmMethod::OverlayRepeat),
    ] {
        let config = if method == "overlay_repeat" {
            format!("method={method}\ntiles=0-3\nwidth=2\nheight=2\n")
        } else {
            format!("method={method}\ntiles=0-3\n")
        };
        assert_eq!(parse_ctm_properties(&config).unwrap().method, expected);
    }
}

#[test]
fn emissive_suffix_default_and_custom_are_safe() {
    assert_eq!(parse_emissive_properties("").unwrap().suffix_emissive, "_e");
    assert_eq!(
        parse_emissive_properties("# synthetic\nsuffix.emissive=_glow\n")
            .unwrap()
            .suffix_emissive,
        "_glow"
    );
    for suffix in ["../secret", "a/b", r"a\u005cb", "..", "", "."] {
        assert!(
            parse_emissive_properties(&format!("suffix.emissive={suffix}")).is_err(),
            "unsafe suffix accepted: {suffix:?}"
        );
    }
}

#[test]
fn hostile_numeric_values_and_texture_sizes_fail_closed() {
    for coordinates in ["[1e999,0,0,1,1,1]", "[1e40,0,0,1,1,1]", "[NaN,0,0,1,1,1]"] {
        let jpm = format!(r#"{{"boxes":[{{"coordinates":{coordinates},"textureOffset":[0,0]}}]}}"#);
        assert!(parse_jpm(&jpm).is_err(), "accepted {coordinates}");
    }
    for size in ["[0,64]", "[4097,64]", "[1.5,64]", "[-1,64]"] {
        assert!(parse_jpm(&format!(r#"{{"textureSize":{size}}}"#)).is_err());
    }
    assert!(parse_jem(r#"{"models":[{"part":"head","scale":1e999}]}"#).is_err());
    assert!(parse_jpm(r#"{"translate":[0,0,-1e999]}"#).is_err());
}

#[test]
fn depth_parts_boxes_and_animation_bounds_are_enforced() {
    let mut nested = r#"{"id":"leaf"}"#.to_owned();
    for _ in 0..MAX_SUBMODEL_DEPTH {
        nested = format!(r#"{{"id":"node","submodel":{nested}}}"#);
    }
    assert!(parse_jpm(&format!(r#"{{"submodel":{nested}}}"#)).is_err());

    let entries = (0..=MAX_PARTS)
        .map(|i| format!(r#"{{"part":"p{i}"}}"#))
        .collect::<Vec<_>>()
        .join(",");
    assert!(parse_jem(&format!(r#"{{"models":[{entries}]}}"#)).is_err());

    let boxes = (0..=MAX_BOXES_PER_PART)
        .map(|_| r#"{"coordinates":[0,0,0,1,1,1],"textureOffset":[0,0]}"#)
        .collect::<Vec<_>>()
        .join(",");
    assert!(parse_jpm(&format!(r#"{{"boxes":[{boxes}]}}"#)).is_err());

    let animations = (0..=MAX_ANIMATIONS)
        .map(|_| r#"{"body.rx":"0"}"#)
        .collect::<Vec<_>>()
        .join(",");
    assert!(parse_jem(&format!(
        r#"{{"models":[{{"part":"body","animations":[{animations}]}}]}}"#
    ))
    .is_err());
}

#[test]
fn referenced_models_detect_cycles_and_missing_files() {
    let jem = parse_jem(r#"{"models":[{"part":"body","model":"a.jpm"}]}"#).unwrap();
    let mut lookup = BTreeMap::new();
    assert!(matches!(
        resolve_jem_with_jpm(&jem, &lookup),
        Err(OptifineCompatError::MissingModelReference(_))
    ));
    lookup.insert(
        "a.jpm".to_owned(),
        parse_jpm(r#"{"submodel":{"id":"one","model":"b.jpm"}}"#).unwrap(),
    );
    lookup.insert(
        "b.jpm".to_owned(),
        parse_jpm(r#"{"submodel":{"id":"two","model":"a.jpm"}}"#).unwrap(),
    );
    assert!(matches!(
        resolve_jem_with_jpm(&jem, &lookup),
        Err(OptifineCompatError::ModelReferenceCycle(_))
    ));
}

#[test]
fn malformed_paths_duplicates_and_box_uvs_fail_closed() {
    for reference in [
        "../escape.jpm",
        "/absolute.jpm",
        "foo/../bar.jpm",
        "C:/evil.jpm",
        r"dir\file.jpm",
    ] {
        assert!(parse_jem(&format!(
            r#"{{"models":[{{"part":"body","model":"{reference}"}}]}}"#
        ))
        .is_err());
    }
    assert!(parse_jem(r#"{"models":[{"part":"body","part":"head"}]}"#).is_err());
    assert!(parse_ctm_properties("method=random\nmethod=ctm\ntiles=0-46\n").is_err());
    assert!(parse_emissive_properties("suffix.emissive=_e\nsuffix.emissive=_glow").is_err());
    assert!(parse_jpm(r#"{"boxes":[{"coordinates":[0,0,0,1,1,1]}]}"#).is_err());
    assert!(parse_jpm(
        r#"{"boxes":[{"coordinates":[0,0,0,1,1,1],"textureOffset":[0,0],"uvNorth":[0,0,1,1]}]}"#
    )
    .is_err());
}

#[test]
fn ctm_tiles_weights_and_paths_are_bounded() {
    assert!(parse_ctm_properties(&format!("method=random\ntiles=0-{MAX_CTM_TILES}\n")).is_err());
    assert!(parse_ctm_properties("method=repeat\nwidth=1024\nheight=2\ntiles=0-3\n").is_err());
    assert!(parse_ctm_properties("method=random\ntiles=0-2\nweights=1 2\n").is_err());
    assert!(parse_ctm_properties("method=random\ntiles=0-2\nweights=1 0 3\n").is_err());
    assert!(parse_ctm_properties("method=ctm\ntiles=0-2\n").is_err());
    assert!(parse_ctm_properties("method=random\ntiles=../escape.png\n").is_err());
    assert!(parse_ctm_properties("method=future_method\ntiles=0\n").is_err());
}
