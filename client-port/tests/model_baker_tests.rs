// SPDX-License-Identifier: LGPL-3.0-or-later

use std::collections::BTreeMap;

use megu_client_mesh::model_baker::*;
use megu_client_mesh::Direction;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Parses `(model_id, json)` pairs into one pack's model map.
fn pack(entries: &[(&str, &str)]) -> BTreeMap<String, RawModelJson> {
    entries
        .iter()
        .map(|(id, json)| {
            (
                id.to_string(),
                RawModelJson::parse(json).expect("fixture JSON must parse"),
            )
        })
        .collect()
}

fn make_stack(packs: Vec<BTreeMap<String, RawModelJson>>) -> ResourcePackModelStack {
    let mut stack = ResourcePackModelStack::new();
    for p in packs {
        stack.push_pack(p);
    }
    stack
}

/// All six faces sharing one texture reference, compact for fixtures.
fn faces_all(texture: &str) -> String {
    format!(
        r#""down":{{"texture":"{t}"}},"up":{{"texture":"{t}"}},
        "north":{{"texture":"{t}"}},"south":{{"texture":"{t}"}},
        "west":{{"texture":"{t}"}},"east":{{"texture":"{t}"}}"#,
        t = texture
    )
}

fn assert_vec3(actual: [f32; 3], expected: [f32; 3], what: &str) {
    for (axis, (&a, &e)) in actual.iter().zip(&expected).enumerate() {
        assert!(
            (a - e).abs() < 1e-6,
            "{what}: axis {axis}: expected {expected:?}, got {actual:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// Multi-pack overlay
// ---------------------------------------------------------------------------

#[test]
fn higher_priority_pack_overlays_base_model() {
    // Base "vanilla" pack: a flat full cube.
    let base = pack(&[(
        "block/lantern",
        &format!(
            r##"{{"textures":{{"all":"minecraft:block/base_tex"}},
            "elements":[{{"from":[0,0,0],"to":[16,16,16],
            "faces":{{{}}}}}]}}"##,
            faces_all("#all")
        ),
    )]);
    // Higher-priority user 3D pack: same id, multi-element lantern-like
    // model (a post plus a crossbar), namespaced texture ids.
    let user = pack(&[(
        "block/lantern",
        r##"{"textures":{"frame":"mypack:block/frame","glass":"mypack:block/glass"},
        "elements":[
            {"from":[7,0,7],"to":[9,16,9],"faces":{"up":{"texture":"#frame"}}},
            {"from":[2,14,7],"to":[14,16,9],"faces":{
                "up":{"texture":"#glass"},"down":{"texture":"#glass"},
                "north":{"texture":"#glass"},"south":{"texture":"#glass"},
                "west":{"texture":"#glass"},"east":{"texture":"#glass"}}}
        ]}"##,
    )]);
    let stack = make_stack(vec![base, user]);
    let baked = bake_model(&stack, "minecraft:block/lantern", 0, 0).unwrap();
    // The user pack's two-element model fully replaced the base cube.
    assert_eq!(baked.quads.len(), 7);
    assert_eq!(
        baked
            .quads
            .iter()
            .filter(|q| q.texture == "mypack:block/frame")
            .count(),
        1
    );
    assert_eq!(
        baked
            .quads
            .iter()
            .filter(|q| q.texture == "mypack:block/glass")
            .count(),
        6
    );
    assert!(
        baked.quads.iter().all(|q| !q.texture.contains("base_tex")),
        "base pack model must be fully shadowed"
    );
    // Post element [7,0,7]..[9,16,9] scaled to block space.
    assert_vec3(
        baked.quads[0].positions[0],
        [7.0 / 16.0, 1.0, 9.0 / 16.0],
        "up v0",
    );
}

// ---------------------------------------------------------------------------
// parent chain + #texture aliases + cycles
// ---------------------------------------------------------------------------

fn chained_stack() -> ResourcePackModelStack {
    let models = pack(&[
        // Leaf: namespaceless parent id exercises normalization.
        (
            "block/leaf",
            r##"{"parent":"block/mid","textures":{"key":"#side"}}"##,
        ),
        (
            "block/mid",
            r##"{"parent":"block/root","textures":{"side":"#all"}}"##,
        ),
        (
            "block/root",
            &format!(
                r##"{{"ambientocclusion":false,"textures":{{"all":"minecraft:block/stone"}},
                "elements":[{{"from":[0,0,0],"to":[16,16,16],"faces":{{{}}}}}]}}"##,
                faces_all("#key")
            ),
        ),
    ]);
    make_stack(vec![models])
}

#[test]
fn parent_chain_merges_textures_and_inherits_elements() {
    let baked = bake_model(&chained_stack(), "block/leaf", 0, 0).unwrap();
    assert_eq!(baked.quads.len(), 6);
    // #key -> #side -> #all -> minecraft:block/stone across the merged map.
    assert!(baked
        .quads
        .iter()
        .all(|q| q.texture == "minecraft:block/stone"));
    // ambientocclusion came from the root, the only model defining it.
    assert!(!baked.ambient_occlusion);
    // The leaf defines no elements: the root cube is inherited wholesale.
    assert_vec3(baked.quads[0].positions[0], [0.0, 0.0, 0.0], "corner");
}

#[test]
fn child_empty_elements_replaces_parent_elements() {
    let models = pack(&[
        ("block/leaf", r##"{"parent":"block/root","elements":[]}"##),
        (
            "block/root",
            &format!(
                r##"{{"textures":{{"a":"minecraft:block/stone"}},
                "elements":[{{"from":[0,0,0],"to":[16,16,16],"faces":{{{}}}}}]}}"##,
                faces_all("#a")
            ),
        ),
    ]);
    let stack = make_stack(vec![models]);
    let baked = bake_model(&stack, "block/leaf", 0, 0).unwrap();
    assert_eq!(baked.quads.len(), 0);
}

#[test]
fn parent_cycle_fails_closed() {
    let models = pack(&[
        ("block/a", r##"{"parent":"block/b"}"##),
        ("block/b", r##"{"parent":"block/a"}"##),
    ]);
    let stack = make_stack(vec![models]);
    assert!(matches!(
        bake_model(&stack, "block/a", 0, 0),
        Err(ModelError::ParentCycle(_))
    ));
}

#[test]
fn parent_depth_beyond_limit_fails_closed() {
    // 40-deep chain; MAX_PARENT_DEPTH is 32.
    let mut entries: Vec<(String, String)> = Vec::new();
    for i in 0..40 {
        let json = if i == 39 {
            r##"{"textures":{"a":"minecraft:block/stone"},"elements":[{"from":[0,0,0],"to":[16,16,16],"faces":{"up":{"texture":"#a"}}}]}"##.to_string()
        } else {
            format!(r##"{{"parent":"block/m{}"}}"##, i + 1)
        };
        entries.push((format!("block/m{i}"), json));
    }
    let models: BTreeMap<String, RawModelJson> = entries
        .iter()
        .map(|(id, json)| (id.clone(), RawModelJson::parse(json).unwrap()))
        .collect();
    let stack = make_stack(vec![models]);
    assert_eq!(
        bake_model(&stack, "block/m0", 0, 0),
        Err(ModelError::ParentDepthExceeded)
    );
}

#[test]
fn texture_alias_cycle_fails_closed() {
    let models = pack(&[(
        "block/cycle",
        r##"{"textures":{"a":"#b","b":"#a"},
        "elements":[{"from":[0,0,0],"to":[16,16,16],
        "faces":{"up":{"texture":"#a"}}}]}"##,
    )]);
    let stack = make_stack(vec![models]);
    assert!(matches!(
        bake_model(&stack, "block/cycle", 0, 0),
        Err(ModelError::TextureAliasCycle(_))
    ));
}

#[test]
fn unresolved_texture_alias_fails_closed() {
    let models = pack(&[(
        "block/missing",
        r##"{"elements":[{"from":[0,0,0],"to":[16,16,16],
        "faces":{"up":{"texture":"#nowhere"}}}]}"##,
    )]);
    let stack = make_stack(vec![models]);
    assert_eq!(
        bake_model(&stack, "block/missing", 0, 0),
        Err(ModelError::UnresolvedTextureAlias("nowhere".into()))
    );
}

#[test]
fn missing_model_fails_closed() {
    let stack = make_stack(vec![BTreeMap::new()]);
    assert_eq!(
        bake_model(&stack, "block/ghost", 0, 0),
        Err(ModelError::MissingModel("minecraft:block/ghost".into()))
    );
}

// ---------------------------------------------------------------------------
// Element rotation + rescale + blockstate rotation
// ---------------------------------------------------------------------------

fn rotated_lantern() -> ResourcePackModelStack {
    // Bar from [6,6,7] to [10,10,9], rotated 45° about Y through [8,8,8]
    // with Vanilla rescale. cullface "north" on the north face must rotate
    // with the blockstate transform.
    let models = pack(&[(
        "block/bar",
        r##"{"textures":{"t":"pack:block/bar"},
        "elements":[{"from":[6,6,7],"to":[10,10,9],
        "rotation":{"origin":[8,8,8],"axis":"y","angle":45,"rescale":true},
        "faces":{
            "north":{"texture":"#t","cullface":"north"},
            "up":{"texture":"#t","uv":[0,0,4,2],"rotation":90,"tintindex":3}}}]}"##,
    )]);
    make_stack(vec![models])
}

#[test]
fn element_rotation_rescale_and_blockstate_rotate_vertices() {
    let stack = rotated_lantern();
    let baked = bake_model(&stack, "block/bar", 0, 90).unwrap();
    let north = baked
        .quads
        .iter()
        .find(|q| q.cullface.is_some())
        .expect("north quad has cullface");
    // Rescale sqrt(2) on x/z then +45° Y rotation lands the model-unit
    // corners on exact quarter-block positions; blockstate y:90 then maps
    // (x,y,z) -> (-z,y,x) about the centre.
    assert_vec3(north.positions[0], [0.6875, 0.375, 0.5625], "v0");
    assert_vec3(north.positions[1], [0.4375, 0.375, 0.3125], "v1");
    assert_vec3(north.positions[2], [0.4375, 0.625, 0.3125], "v2");
    assert_vec3(north.positions[3], [0.6875, 0.625, 0.5625], "v3");
    // cullface "north" rotated by blockstate y:90 -> east.
    assert_eq!(north.cullface, Some(Direction::East));
    // Face normal: north rotated 45° about Y, then the blockstate step.
    assert_vec3(
        north.normal,
        [
            std::f32::consts::FRAC_1_SQRT_2,
            0.0,
            -std::f32::consts::FRAC_1_SQRT_2,
        ],
        "normal",
    );
}

#[test]
fn rescale_flag_changes_geometry() {
    // Without rescale the same 45°-rotated bar must bake different vertex
    // positions — proves the rescale path executes rather than being
    // silently skipped.
    let models = pack(&[(
        "block/bar",
        r##"{"textures":{"t":"pack:block/bar"},
        "elements":[{"from":[6,6,7],"to":[10,10,9],
        "rotation":{"origin":[8,8,8],"axis":"y","angle":45,"rescale":false},
        "faces":{"north":{"texture":"#t"}}}]}"##,
    )]);
    let stack = make_stack(vec![models]);
    let baked = bake_model(&stack, "block/bar", 0, 0).unwrap();
    let rescaled = bake_model(&rotated_lantern(), "block/bar", 0, 0).unwrap();
    let v1 = baked.quads[0].positions[1];
    let v1r = rescaled
        .quads
        .iter()
        .find(|q| q.cullface.is_some())
        .unwrap()
        .positions[1];
    // Unrescaled (6,6,7) -> offset (-2,-1) rotated +45° about (8,8,8):
    // x' = (-2-1)/sqrt2, z' = (-1+2)/sqrt2 -> (8-3/sqrt2, 8+1/sqrt2).
    let s = std::f32::consts::FRAC_1_SQRT_2;
    assert_vec3(
        v1,
        [(8.0 - 3.0 * s) / 16.0, 6.0 / 16.0, (8.0 + s) / 16.0],
        "unrescaled",
    );
    assert!(
        (v1[0] - v1r[0]).abs() > 1e-3 || (v1[2] - v1r[2]).abs() > 1e-3,
        "rescale must change the baked vertex positions"
    );
}

#[test]
fn blockstate_x_rotation_moves_cullface_and_vertices() {
    let stack = rotated_lantern();
    let baked = bake_model(&stack, "block/bar", 90, 0).unwrap();
    let north = baked.quads.iter().find(|q| q.cullface.is_some()).unwrap();
    // Blockstate x:90 maps north -> down under the (x,y,z)->(x,z,-y) step.
    assert_eq!(north.cullface, Some(Direction::Down));
}

#[test]
fn non_90_blockstate_rotation_fails_closed() {
    let stack = rotated_lantern();
    assert!(matches!(
        bake_model(&stack, "block/bar", 45, 0),
        Err(ModelError::InvalidRotationAngle(_))
    ));
    assert!(matches!(
        bake_model(&stack, "block/bar", 0, -30),
        Err(ModelError::InvalidRotationAngle(_))
    ));
}

// ---------------------------------------------------------------------------
// Face-level data: uv, uv rotation, tintindex, shade
// ---------------------------------------------------------------------------

#[test]
fn face_uv_rotation_tintindex_and_shade_plumb_through() {
    let stack = rotated_lantern();
    let baked = bake_model(&stack, "block/bar", 0, 0).unwrap();
    let up = baked
        .quads
        .iter()
        .find(|q| q.tintindex == 3)
        .expect("up face has tintindex 3");
    // Up-face UV rect [0,0,4,2] with in-face rotation 90: `BlockFaceUV`
    // shifts UV assignment clockwise by one slot (verified against Vanilla
    // 1.21.1), so the corner pairs become
    // (u1,v1) (u1,v0) (u0,v0) (u0,v1).
    assert_eq!(up.uv[0], [4.0, 2.0]);
    assert_eq!(up.uv[1], [4.0, 0.0]);
    assert_eq!(up.uv[2], [0.0, 0.0]);
    assert_eq!(up.uv[3], [0.0, 2.0]);
    assert!(up.shade, "shade defaults to true");

    let models = pack(&[(
        "block/unshaded",
        r##"{"textures":{"a":"minecraft:block/stone"},
        "elements":[{"from":[0,0,0],"to":[16,16,16],"shade":false,
        "faces":{"up":{"texture":"#a","tintindex":-1}}}]}"##,
    )]);
    let baked = bake_model(&make_stack(vec![models]), "block/unshaded", 0, 0).unwrap();
    assert!(!baked.quads[0].shade);
    assert_eq!(baked.quads[0].tintindex, -1);
}

#[test]
fn default_uv_derives_from_element_bounds() {
    let models = pack(&[(
        "block/partial",
        r##"{"textures":{"a":"minecraft:block/stone"},
        "elements":[{"from":[2,4,6],"to":[10,12,14],
        "faces":{"south":{"texture":"#a"}}}]}"##,
    )]);
    let stack = make_stack(vec![models]);
    let baked = bake_model(&stack, "block/partial", 0, 0).unwrap();
    // South default uv: [from.x, 16-to.y, to.x, 16-from.y] = [2,4,10,12].
    assert_eq!(baked.quads[0].uv[0], [2.0, 12.0]);
    assert_eq!(baked.quads[0].uv[1], [10.0, 12.0]);
    assert_eq!(baked.quads[0].uv[2], [10.0, 4.0]);
    assert_eq!(baked.quads[0].uv[3], [2.0, 4.0]);
}

// ---------------------------------------------------------------------------
// Item overrides
// ---------------------------------------------------------------------------

fn override_stack() -> ResourcePackModelStack {
    let models = pack(&[(
        "item/wand",
        r##"{"textures":{"layer0":"item/wand"},
        "overrides":[
            {"predicate":{"custom_model_data":1},"model":"item/wand_a"},
            {"predicate":{"custom_model_data":4},"model":"mypack:item/wand_b"},
            {"predicate":{"custom_model_data":2},"model":"item/wand_c"}
        ]}"##,
    )]);
    make_stack(vec![models])
}

#[test]
fn custom_model_data_picks_highest_threshold() {
    let stack = override_stack();
    // Overrides are deliberately unsorted: the pick must be the highest
    // predicate <= value, not positional.
    assert_eq!(
        resolve_item_override(&stack, "item/wand", Some(5)).unwrap(),
        "mypack:item/wand_b"
    );
    assert_eq!(
        resolve_item_override(&stack, "item/wand", Some(4)).unwrap(),
        "mypack:item/wand_b"
    );
    assert_eq!(
        resolve_item_override(&stack, "item/wand", Some(3)).unwrap(),
        "minecraft:item/wand_c"
    );
    assert_eq!(
        resolve_item_override(&stack, "item/wand", Some(1)).unwrap(),
        "minecraft:item/wand_a"
    );
    // No predicate <= 0 and no custom_model_data at all fall back to the
    // base model id (normalised).
    assert_eq!(
        resolve_item_override(&stack, "item/wand", Some(0)).unwrap(),
        "minecraft:item/wand"
    );
    assert_eq!(
        resolve_item_override(&stack, "item/wand", None).unwrap(),
        "minecraft:item/wand"
    );
}

#[test]
fn override_on_missing_model_fails_closed() {
    let stack = override_stack();
    assert_eq!(
        resolve_item_override(&stack, "item/ghost", Some(1)),
        Err(ModelError::MissingModel("minecraft:item/ghost".into()))
    );
}

// ---------------------------------------------------------------------------
// Hostile input: depth bombs, element bombs, non-finite / out-of-bounds
// ---------------------------------------------------------------------------

#[test]
fn json_depth_bomb_fails_closed() {
    let deep = "[".repeat(10_000) + &"]".repeat(10_000);
    assert!(matches!(
        RawModelJson::parse(&deep),
        Err(ModelError::Json(_))
    ));
    let deep_obj = "{\"a\":".repeat(10_000) + "1" + &"}".repeat(10_000);
    assert!(matches!(
        RawModelJson::parse(&deep_obj),
        Err(ModelError::Json(_))
    ));
}

#[test]
fn element_count_bomb_fails_closed() {
    let element = r##"{"from":[0,0,0],"to":[1,1,1],"faces":{"up":{"texture":"#a"}}}"##;
    let json = format!(
        r##"{{"textures":{{"a":"minecraft:block/stone"}},"elements":[{}]}}"##,
        vec![element; MAX_ELEMENTS_PER_MODEL + 1].join(",")
    );
    assert_eq!(
        RawModelJson::parse(&json),
        Err(ModelError::TooManyElements(MAX_ELEMENTS_PER_MODEL + 1))
    );
}

#[test]
fn out_of_bounds_and_nonfinite_coordinates_fail_closed() {
    for bad in [
        // Below and above Vanilla's -16..=32 model-unit bounds.
        r##"{"from":[-17,0,0],"to":[1,1,1]"##,
        r##"{"from":[0,0,0],"to":[1,1,33]"##,
        // 1e999 overflows f64 -> parser rejects before schema mapping.
        r##"{"from":[1e999,0,0],"to":[1,1,1]"##,
    ] {
        let json = format!(
            r##"{{"elements":[{bad},"faces":{{"up":{{"texture":"minecraft:block/stone"}}}}}}}}]}}"##
        );
        assert!(
            RawModelJson::parse(&json).is_err(),
            "{bad} must fail closed"
        );
    }
    // Rotation origin outside the overhang bounds also fails closed.
    let json = r##"{"elements":[{"from":[0,0,0],"to":[1,1,1],
        "rotation":{"origin":[40,8,8],"axis":"y","angle":45},
        "faces":{"up":{"texture":"minecraft:block/stone"}}}]}"##;
    assert!(matches!(
        RawModelJson::parse(json),
        Err(ModelError::OutOfRangeCoordinate(_))
    ));
    // A programmatically constructed NaN bypasses the parser; the baker
    // re-validates and still fails closed.
    let mut model = RawModelJson::parse(
        r##"{"elements":[{"from":[0,0,0],"to":[1,1,1],
        "faces":{"up":{"texture":"minecraft:block/stone"}}}]}"##,
    )
    .unwrap();
    model.elements.as_mut().unwrap()[0].from[0] = f32::NAN;
    let stack = make_stack(vec![BTreeMap::from([("block/nan".to_string(), model)])]);
    assert_eq!(
        bake_model(&stack, "block/nan", 0, 0),
        Err(ModelError::NonFiniteValue("elements[0].from[0]".into()))
    );
}

#[test]
fn malformed_and_empty_input_fail_closed() {
    // Note: "{}" parses — every model field is optional.
    for bad in ["", "not json", "[1,2,3]", r##"{"elements":{}}"##] {
        assert!(RawModelJson::parse(bad).is_err(), "{bad:?} must fail");
    }
}

#[test]
fn unicode_escapes_decode_in_model_fields() {
    // Surrogate pair in a texture id decodes to the astral scalar.
    let model = RawModelJson::parse(
        r##"{"textures":{"a":"pack:block/\uD83D\uDD25"},"elements":[
        {"from":[0,0,0],"to":[1,1,1],"faces":{"up":{"texture":"#a"}}}]}"##,
    )
    .unwrap();
    assert_eq!(model.textures["a"], "pack:block/\u{1F525}");
    // A lone surrogate is malformed, not lossy.
    assert!(RawModelJson::parse(r#"{"parent":"\uD83D"}"#).is_err());
}
