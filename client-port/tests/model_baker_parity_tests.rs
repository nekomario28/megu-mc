// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later

//! Vanilla 1.21.1 parity pins for `model_baker`: clockwise face-UV
//! `rotation`, the `down` default-UV V flip, `uvlock`, and the clean-room
//! `blockstates/*.json` resolver.
//!
//! Expected values below are hand-traced from `FaceBakery`/`BlockFaceUV`/
//! `BlockElement.uvsByFace`/`BlockMath.getUVLockTransform` semantics, not
//! copied from the implementation under test.

use std::collections::BTreeMap;

use megu_client_mesh::model_baker::blockstate::{
    self, bake_blockstate, resolve_blockstate, BlockStateDef, BlockStateResolution,
};
use megu_client_mesh::model_baker::*;
use megu_client_mesh::Direction;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

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

fn props(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

fn assert_uv(quad: &BakedModelQuad, expected: [[f32; 2]; 4], what: &str) {
    for (corner, (&actual, &want)) in quad.uv.iter().zip(&expected).enumerate() {
        assert!(
            (actual[0] - want[0]).abs() < 1e-5 && (actual[1] - want[1]).abs() < 1e-5,
            "{what} corner {corner}: expected {want:?}, got {actual:?} (all: {:?})",
            quad.uv
        );
    }
}

/// A full-cube model whose six faces all share `face_json` fields.
fn cube_with_faces(face_fields: &str) -> String {
    format!(
        r##"{{"textures":{{"t":"pack:block/t"}},
        "elements":[{{"from":[0,0,0],"to":[16,16,16],
        "faces":{{
            "down":{{"texture":"#t"{f}}},"up":{{"texture":"#t"{f}}},
            "north":{{"texture":"#t"{f}}},"south":{{"texture":"#t"{f}}},
            "west":{{"texture":"#t"{f}}},"east":{{"texture":"#t"{f}}}
        }}}}]}}"##,
        f = face_fields
    )
}

// ---------------------------------------------------------------------------
// Face `rotation` is clockwise on every face (Vanilla `getShiftedIndex`)
// ---------------------------------------------------------------------------

/// Vanilla vertex order is one slot ahead of the port's `FACE_CORNERS`
/// winding on all six faces, and `BlockFaceUV` shifts UV assignment
/// *clockwise* by `rotation / 90`. With `uv = [1,2,3,4]` the four UV-corner
/// values are `(u0,v0)=(1,2)`, `(u0,v1)=(1,4)`, `(u1,v1)=(3,4)`, `(u1,v0)=(3,2)`
/// and the per-corner expectations for a uniform winding are:
///
/// * rot 0:   slot (c+1)%4 -> [(1,4),(3,4),(3,2),(1,2)]
/// * rot 90:  slot (c+2)%4 -> [(3,4),(3,2),(1,2),(1,4)]
/// * rot 180: slot (c+3)%4 -> [(3,2),(1,2),(1,4),(3,4)]
/// * rot 270: slot  c%4    -> [(1,2),(1,4),(3,4),(3,2)]
#[test]
fn face_rotation_is_clockwise_on_all_faces() {
    let cases: [(u16, [[f32; 2]; 4]); 4] = [
        (0, [[1.0, 4.0], [3.0, 4.0], [3.0, 2.0], [1.0, 2.0]]),
        (90, [[3.0, 4.0], [3.0, 2.0], [1.0, 2.0], [1.0, 4.0]]),
        (180, [[3.0, 2.0], [1.0, 2.0], [1.0, 4.0], [3.0, 4.0]]),
        (270, [[1.0, 2.0], [1.0, 4.0], [3.0, 4.0], [3.0, 2.0]]),
    ];
    for (rotation, expected) in cases {
        let models = pack(&[(
            "block/rot",
            &cube_with_faces(&format!(r#", "uv":[1,2,3,4], "rotation":{rotation}"#)),
        )]);
        let stack = make_stack(vec![models]);
        let baked = bake_model(&stack, "block/rot", 0, 0).unwrap();
        assert_eq!(baked.quads.len(), 6, "rotation {rotation}");
        // Quad order follows Direction::ALL: down, up, north, south, west, east.
        let normals = [
            Direction::Down,
            Direction::Up,
            Direction::North,
            Direction::South,
            Direction::West,
            Direction::East,
        ];
        for (index, quad) in baked.quads.iter().enumerate() {
            assert_uv(
                quad,
                expected,
                &format!("rotation {rotation} {:?} face", normals[index]),
            );
        }
    }
}

/// Same clockwise pin on a *non-cube* element: the UV table must not depend
/// on element bounds, only on corner winding + rotation.
#[test]
fn face_rotation_clockwise_on_partial_element() {
    let models = pack(&[(
        "block/partial_rot",
        r##"{"textures":{"t":"pack:block/t"},
        "elements":[{"from":[2,4,6],"to":[10,12,14],
        "faces":{"south":{"texture":"#t","uv":[1,2,3,4],"rotation":90}}}]}"##,
    )]);
    let baked = bake_model(&make_stack(vec![models]), "block/partial_rot", 0, 0).unwrap();
    assert_eq!(baked.quads.len(), 1);
    assert_uv(
        &baked.quads[0],
        [[3.0, 4.0], [3.0, 2.0], [1.0, 2.0], [1.0, 4.0]],
        "south rotation 90",
    );
}

// ---------------------------------------------------------------------------
// `down` default UV flips V against +z (Vanilla `BlockElement.uvsByFace`)
// ---------------------------------------------------------------------------

#[test]
fn down_default_uv_flips_v_axis() {
    // Element [2,4,6]..[10,12,14]: vanilla uvsByFace gives
    //   down = [from.x, 16-to.z, to.x, 16-from.z] = [2, 2, 10, 10]
    //   up   = [from.x, from.z,     to.x, to.z    ] = [2, 6, 10, 14]
    let models = pack(&[(
        "block/partial",
        r##"{"textures":{"t":"pack:block/t"},
        "elements":[{"from":[2,4,6],"to":[10,12,14],
        "faces":{"down":{"texture":"#t"},"up":{"texture":"#t"}}}]}"##,
    )]);
    let baked = bake_model(&make_stack(vec![models]), "block/partial", 0, 0).unwrap();
    assert_eq!(baked.quads.len(), 2);
    let down = &baked.quads[0];
    let up = &baked.quads[1];
    // Down rect [2,2,10,10] -> corners (u0,v0)=(2,2) (u0,v1)=(2,10)
    // (u1,v1)=(10,10) (u1,v0)=(10,2); slot (c+1)%4 assigns:
    //   [(u0,v1),(u1,v1),(u1,v0),(u0,v0)] = [(2,10),(10,10),(10,2),(2,2)]
    assert_uv(
        down,
        [[2.0, 10.0], [10.0, 10.0], [10.0, 2.0], [2.0, 2.0]],
        "down default uv",
    );
    // Up rect [2,6,10,14] -> [(u0,v1),(u1,v1),(u1,v0),(u0,v0)] =
    //   [(2,14),(10,14),(10,6),(2,6)]
    assert_uv(
        up,
        [[2.0, 14.0], [10.0, 14.0], [10.0, 6.0], [2.0, 6.0]],
        "up default uv",
    );
    // Sanity: down and up UV rects must differ — the old shared mapping
    // produced identical V ranges for both.
    assert_ne!(down.uv, up.uv);
}

// ---------------------------------------------------------------------------
// `uvlock` (Vanilla `FaceBakery.recomputeUVs` + `BlockMath.getUVLockTransform`)
// ---------------------------------------------------------------------------

/// Traced by hand from `getUVLockTransform`:
///
/// * `up` face under blockstate `y:90`: `G2L[up]·Mᵀ·L2G[up]` reduces to a
///   +90° in-plane UV rotation, so rect `[0,0,16,16]` stays put and the
///   in-face rotation becomes 270 -> corner UVs `[(0,0),(0,16),(16,16),(16,0)]`.
///   Without uvlock the same bake keeps rotation 0 -> `[(0,16),(16,16),(16,0),(0,0)]`.
#[test]
fn uvlock_counter_rotates_up_face_uvs_under_y_rotation() {
    let stack = make_stack(vec![pack(&[("block/cube", &cube_with_faces(""))])]);
    let unlocked = bake_model_with_uvlock(&stack, "block/cube", 0, 90, false).unwrap();
    let locked = bake_model_with_uvlock(&stack, "block/cube", 0, 90, true).unwrap();
    let up_unlocked = unlocked
        .quads
        .iter()
        .find(|q| q.normal == [0.0, 1.0, 0.0])
        .unwrap();
    let up_locked = locked
        .quads
        .iter()
        .find(|q| q.normal == [0.0, 1.0, 0.0])
        .unwrap();
    assert_uv(
        up_unlocked,
        [[0.0, 16.0], [16.0, 16.0], [16.0, 0.0], [0.0, 0.0]],
        "up y90 no uvlock",
    );
    assert_uv(
        up_locked,
        [[0.0, 0.0], [0.0, 16.0], [16.0, 16.0], [16.0, 0.0]],
        "up y90 uvlock",
    );
}

/// `north` face under blockstate `x:90` rotates to `down`; the UV lock
/// transform maps the UV plane by 180°, leaving rect `[0,0,16,16]` with
/// in-face rotation 180 -> `[(16,0),(0,0),(0,16),(16,16)]`.
#[test]
fn uvlock_counter_rotates_north_face_uvs_under_x_rotation() {
    let stack = make_stack(vec![pack(&[("block/cube", &cube_with_faces(""))])]);
    let locked = bake_model_with_uvlock(&stack, "block/cube", 90, 0, true).unwrap();
    let north = locked
        .quads
        .iter()
        .find(|q| q.normal == [0.0, 0.0, -1.0])
        .expect("a quad keeps the north normal after x:90");
    assert_uv(
        north,
        [[16.0, 0.0], [0.0, 0.0], [0.0, 16.0], [16.0, 16.0]],
        "north x90 uvlock",
    );
}

/// uvlock with no blockstate rotation and no face rotation must be a no-op
/// (the lock transform is the identity).
#[test]
fn uvlock_identity_rotation_is_noop() {
    let stack = make_stack(vec![pack(&[("block/cube", &cube_with_faces(""))])]);
    let unlocked = bake_model_with_uvlock(&stack, "block/cube", 0, 0, false).unwrap();
    let locked = bake_model_with_uvlock(&stack, "block/cube", 0, 0, true).unwrap();
    assert_eq!(unlocked, locked);
}

/// `bake_model` keeps its historical signature and never applies uvlock.
#[test]
fn bake_model_delegates_without_uvlock() {
    let stack = make_stack(vec![pack(&[("block/cube", &cube_with_faces(""))])]);
    let plain = bake_model(&stack, "block/cube", 0, 90).unwrap();
    let explicit = bake_model_with_uvlock(&stack, "block/cube", 0, 90, false).unwrap();
    assert_eq!(plain, explicit);
}

// ---------------------------------------------------------------------------
// blockstates/*.json resolver
// ---------------------------------------------------------------------------

fn blockstate_def(json: &str) -> BlockStateDef {
    BlockStateDef::parse(json).expect("blockstate JSON must parse")
}

#[test]
fn variants_select_by_property_constraints() {
    // Pipe syntax (`a|b`) belongs to multipart `when` values only; variant
    // keys are exact `prop=value` segments. Note a `""` catch-all would
    // overlap every specific key (Vanilla "Overlapping definition"), so it
    // gets its own def below.
    let def = blockstate_def(
        r##"{"variants":{
            "facing=north":{"model":"block/north_model"},
            "facing=south,half=top":{"model":"block/south_top","x":90,"y":0,"uvlock":true}
        }}"##,
    );
    let north = resolve_blockstate(&def, &props(&[("facing", "north")]), [0, 0, 0]).unwrap();
    assert_eq!(
        north,
        BlockStateResolution::Variant(blockstate::VariantSpec {
            model: "block/north_model".into(),
            x_deg: 0,
            y_deg: 0,
            uvlock: false,
            weight: 1,
        })
    );
    let south_top = resolve_blockstate(
        &def,
        &props(&[("facing", "south"), ("half", "top")]),
        [0, 0, 0],
    )
    .unwrap();
    match south_top {
        BlockStateResolution::Variant(v) => {
            assert_eq!(v.model, "block/south_top");
            assert_eq!(v.x_deg, 90);
            assert!(v.uvlock);
        }
        other => panic!("expected Variant, got {other:?}"),
    }
    // Multi-constraint key requires ALL segments; "half=bottom" misses and,
    // with no catch-all or multipart, the state fails closed.
    assert!(matches!(
        resolve_blockstate(
            &def,
            &props(&[("facing", "south"), ("half", "bottom")]),
            [0, 0, 0],
        ),
        Err(ModelError::BlockState(_))
    ));
    // The empty-string key alone is the unconditional catch-all.
    let catch_all = blockstate_def(r##"{"variants":{"":{"model":"block/fallback"}}}"##);
    let fallback =
        resolve_blockstate(&catch_all, &props(&[("anything", "goes")]), [0, 0, 0]).unwrap();
    assert_eq!(
        fallback,
        BlockStateResolution::Variant(blockstate::VariantSpec {
            model: "block/fallback".into(),
            x_deg: 0,
            y_deg: 0,
            uvlock: false,
            weight: 1,
        })
    );
}

#[test]
fn variant_key_unknown_property_fails_closed() {
    let def = blockstate_def(r##"{"variants":{"facing=north":{"model":"block/n"}}}"##);
    // props lacks "facing" entirely -> unknown property -> error.
    assert!(matches!(
        resolve_blockstate(&def, &props(&[("half", "top")]), [0, 0, 0]),
        Err(ModelError::BlockState(_))
    ));
    // A property present but unmatched value simply doesn't match the key.
    assert!(matches!(
        resolve_blockstate(&def, &props(&[("facing", "south")]), [0, 0, 0]),
        Err(ModelError::BlockState(_))
    ));
}

#[test]
fn overlapping_variant_keys_fail_closed() {
    // "" matches every state, so it overlaps "facing=north" on north.
    let def = blockstate_def(
        r##"{"variants":{"":{"model":"block/all"},"facing=north":{"model":"block/n"}}}"##,
    );
    assert!(matches!(
        resolve_blockstate(&def, &props(&[("facing", "north")]), [0, 0, 0]),
        Err(ModelError::BlockState(_))
    ));
    // Other states still resolve through the catch-all.
    let south = resolve_blockstate(&def, &props(&[("facing", "south")]), [0, 0, 0]).unwrap();
    assert!(matches!(south, BlockStateResolution::Variant(v) if v.model == "block/all"));
}

#[test]
fn weighted_variants_pick_deterministically_by_position() {
    let def = blockstate_def(
        r##"{"variants":{"": [
            {"model":"block/a","weight":1},
            {"model":"block/b","weight":3}
        ]}}"##,
    );
    let empty = props(&[]);
    // Same inputs -> same pick, every time (zero RNG/clock).
    let first = resolve_blockstate(&def, &empty, [3, -7, 42]).unwrap();
    for _ in 0..8 {
        assert_eq!(
            resolve_blockstate(&def, &empty, [3, -7, 42]).unwrap(),
            first
        );
    }
    // Across positions both weighted entries must be reachable — proves the
    // position hash actually selects rather than always taking the first.
    let mut seen = std::collections::BTreeSet::new();
    for x in 0..64 {
        if let BlockStateResolution::Variant(v) =
            resolve_blockstate(&def, &empty, [x, 0, 0]).unwrap()
        {
            seen.insert(v.model.clone());
        }
    }
    assert_eq!(seen.len(), 2, "both weighted variants must be reachable");
}

#[test]
fn multipart_when_or_and_and_pipe_predicates() {
    let def = blockstate_def(
        r##"{"multipart":[
            {"when":{"OR":[{"facing":"north"},{"facing":"south"}]},
             "apply":{"model":"block/north_south"}},
            {"when":{"AND":[{"facing":"east"},{"half":"top"}]},
             "apply":{"model":"block/east_top"}},
            {"when":{"facing":"west|east","half":"!top"},
             "apply":{"model":"block/west_east_not_top"}},
            {"apply":{"model":"block/base"}}
        ]}"##,
    );
    // north: selector 0 + unconditional selector 3.
    let north = resolve_blockstate(&def, &props(&[("facing", "north"), ("half", "bottom")]), [0, 0, 0]).unwrap();
    let BlockStateResolution::Multipart(parts) = north else {
        panic!("expected multipart resolution");
    };
    let models: Vec<&str> = parts.iter().map(|v| v.model.as_str()).collect();
    assert_eq!(models, ["block/north_south", "block/base"]);

    // east+top: selector 1 AND-matches; selector 2's "!top" rejects;
    // selector 0's OR misses. Only unconditional remains.
    let east_top = resolve_blockstate(
        &def,
        &props(&[("facing", "east"), ("half", "top")]),
        [0, 0, 0],
    )
    .unwrap();
    let BlockStateResolution::Multipart(parts) = east_top else {
        panic!("expected multipart resolution");
    };
    let models: Vec<&str> = parts.iter().map(|v| v.model.as_str()).collect();
    assert_eq!(models, ["block/east_top", "block/base"]);

    // west+bottom: pipe values match "west", negation "!top" passes on
    // "bottom"; OR/AND selectors miss.
    let west_bottom = resolve_blockstate(
        &def,
        &props(&[("facing", "west"), ("half", "bottom")]),
        [0, 0, 0],
    )
    .unwrap();
    let BlockStateResolution::Multipart(parts) = west_bottom else {
        panic!("expected multipart resolution");
    };
    let models: Vec<&str> = parts.iter().map(|v| v.model.as_str()).collect();
    assert_eq!(models, ["block/west_east_not_top", "block/base"]);

    // up: nothing conditional matches; the unconditional selector still does.
    let up = resolve_blockstate(&def, &props(&[("facing", "up"), ("half", "top")]), [0, 0, 0]).unwrap();
    let BlockStateResolution::Multipart(parts) = up else {
        panic!("expected multipart resolution");
    };
    assert_eq!(parts.len(), 1);
    assert_eq!(parts[0].model, "block/base");
}

#[test]
fn multipart_bake_concatenates_matching_parts() {
    let def = blockstate_def(
        r##"{"multipart":[
            {"when":{"facing":"north"},"apply":{"model":"block/n_part"}},
            {"apply":{"model":"block/base"}}
        ]}"##,
    );
    let models = pack(&[
        (
            "block/n_part",
            r##"{"textures":{"t":"pack:block/n"},
            "elements":[{"from":[0,0,0],"to":[16,16,16],
            "faces":{"north":{"texture":"#t"}}}]}"##,
        ),
        (
            "block/base",
            r##"{"textures":{"t":"pack:block/b"},
            "elements":[{"from":[0,0,0],"to":[16,16,16],
            "faces":{"up":{"texture":"#t"}}}]}"##,
        ),
    ]);
    let stack = make_stack(vec![models]);
    let north = bake_blockstate(&stack, &def, &props(&[("facing", "north")]), [0, 0, 0]).unwrap();
    // north_part contributes 1 quad, base contributes 1 -> 2 total.
    assert_eq!(north.quads.len(), 2);
    assert_eq!(north.quads[0].texture, "pack:block/n");
    assert_eq!(north.quads[1].texture, "pack:block/b");

    let south = bake_blockstate(&stack, &def, &props(&[("facing", "south")]), [0, 0, 0]).unwrap();
    assert_eq!(south.quads.len(), 1);
    assert_eq!(south.quads[0].texture, "pack:block/b");
}

#[test]
fn bake_blockstate_variants_thread_rotation_and_uvlock() {
    // A variant with y:90 + uvlock:true must bake identically to calling
    // bake_model_with_uvlock directly on the resolved spec.
    let def = blockstate_def(
        r##"{"variants":{"facing=north":{"model":"block/cube","y":90,"uvlock":true}}}"##,
    );
    let stack = make_stack(vec![pack(&[("block/cube", &cube_with_faces(""))])]);
    let via_state =
        bake_blockstate(&stack, &def, &props(&[("facing", "north")]), [1, 2, 3]).unwrap();
    let direct = bake_model_with_uvlock(&stack, "block/cube", 0, 90, true).unwrap();
    assert_eq!(via_state, direct);
}

#[test]
fn variants_win_over_multipart_when_both_match() {
    // Vanilla pre-fills multipart for every state, then variant keys
    // overwrite it — a matching variant replaces the multipart result.
    let def = blockstate_def(
        r##"{"variants":{"facing=north":{"model":"block/n"}},
        "multipart":[{"apply":{"model":"block/base"}}]}"##,
    );
    let north = resolve_blockstate(&def, &props(&[("facing", "north")]), [0, 0, 0]).unwrap();
    assert!(matches!(north, BlockStateResolution::Variant(v) if v.model == "block/n"));
    let south = resolve_blockstate(&def, &props(&[("facing", "south")]), [0, 0, 0]).unwrap();
    assert!(matches!(south, BlockStateResolution::Multipart(_)));
}

// ---------------------------------------------------------------------------
// Fail-closed parsing and resolution
// ---------------------------------------------------------------------------

#[test]
fn blockstate_malformed_json_fails_closed() {
    for bad in ["", "not json", "[1,2,3]", r##"{"variants":{"":}}"##] {
        assert!(
            BlockStateDef::parse(bad).is_err(),
            "{bad:?} must fail closed"
        );
    }
    // Depth bomb: same MAX_JSON_DEPTH bound as model parsing.
    let deep = "{\"a\":".repeat(10_000) + "1" + &"}".repeat(10_000);
    assert!(matches!(
        BlockStateDef::parse(&deep),
        Err(ModelError::Json(_))
    ));
}

#[test]
fn blockstate_schema_violations_fail_closed() {
    for bad in [
        // Neither variants nor multipart.
        r##"{}"##,
        r##"{"variants":{}}"##,
        r##"{"multipart":[]}"##,
        // variants must be an object of variant-or-array values.
        r##"{"variants":[]}"##,
        r##"{"variants":{"":"just a string"}}"##,
        // Empty variant array (Vanilla JsonParseException).
        r##"{"variants":{"":[]}}"##,
        // Variant missing "model".
        r##"{"variants":{"":{"x":90}}}"##,
        // Rotation not a multiple of 90.
        r##"{"variants":{"":{"model":"block/x","x":45}}}"##,
        r##"{"variants":{"":{"model":"block/x","y":-30}}}"##,
        // weight < 1.
        r##"{"variants":{"":{"model":"block/x","weight":0}}}"##,
        // uvlock must be boolean.
        r##"{"variants":{"":{"model":"block/x","uvlock":"yes"}}}"##,
        // multipart must be an array; selectors need "apply".
        r##"{"multipart":{}}"##,
        r##"{"multipart":[{"when":{"facing":"north"}}]}"##,
        // Empty "when" object.
        r##"{"multipart":[{"when":{},"apply":{"model":"block/x"}}]}"##,
        // "when" values must be strings; "|"/"!" produce empty values.
        r##"{"multipart":[{"when":{"facing":3},"apply":{"model":"block/x"}}]}"##,
        r##"{"multipart":[{"when":{"facing":"|"},"apply":{"model":"block/x"}}]}"##,
        r##"{"multipart":[{"when":{"facing":"!"},"apply":{"model":"block/x"}}]}"##,
        // Malformed variant-key segment (no '=' and non-empty).
        r##"{"variants":{"facing":{"model":"block/x"}}}"##,
    ] {
        assert!(
            BlockStateDef::parse(bad).is_err(),
            "{bad} must fail closed"
        );
    }
}

#[test]
fn blockstate_negative_and_wrapped_rotations_normalize() {
    // x:-90 wraps to 270, x:450 wraps to 90 — both valid BlockModelRotation
    // combos, normalized at parse time.
    let def = blockstate_def(
        r##"{"variants":{"":{"model":"block/x","x":-90,"y":450}}}"##,
    );
    let resolved = resolve_blockstate(&def, &props(&[]), [0, 0, 0]).unwrap();
    let BlockStateResolution::Variant(v) = resolved else {
        panic!("expected variant");
    };
    assert_eq!((v.x_deg, v.y_deg), (270, 90));
}

#[test]
fn blockstate_condition_unknown_property_fails_closed() {
    let def = blockstate_def(
        r##"{"multipart":[{"when":{"nosuchprop":"x"},"apply":{"model":"block/x"}}]}"##,
    );
    assert!(matches!(
        resolve_blockstate(&def, &props(&[("facing", "north")]), [0, 0, 0]),
        Err(ModelError::BlockState(_))
    ));
}

#[test]
fn blockstate_no_matching_variant_fails_closed() {
    // "variants" present but no key (not even "") matches the state, and no
    // multipart fallback exists.
    let def = blockstate_def(r##"{"variants":{"facing=north":{"model":"block/n"}}}"##);
    assert!(matches!(
        resolve_blockstate(&def, &props(&[("facing", "south")]), [0, 0, 0]),
        Err(ModelError::BlockState(_))
    ));
}

#[test]
fn blockstate_bake_missing_model_fails_closed() {
    let def = blockstate_def(r##"{"variants":{"":{"model":"block/ghost"}}}"##);
    let stack = make_stack(vec![BTreeMap::new()]);
    assert!(matches!(
        bake_blockstate(&stack, &def, &props(&[]), [0, 0, 0]),
        Err(ModelError::MissingModel(_))
    ));
}
