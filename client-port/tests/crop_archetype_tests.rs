// SPDX-License-Identifier: LGPL-3.0-or-later

//! Clean-room tests for [`crop_archetype`]. Every sprite below is a
//! synthetic 16x16 mask drawn from first principles — zero Mojang or mod
//! texture bytes are referenced anywhere.

use megu_client_mesh::crop_archetype::*;
use megu_client_mesh::vanilla_plus_3d::Crop3dArchetype;

// ---------------------------------------------------------------------------
// Sprite builders (all coordinates are texels, y = 0 is the top row)
// ---------------------------------------------------------------------------

const GREEN: [u8; 3] = [60, 140, 55];
const GOLD: [u8; 3] = [222, 186, 60];
const ORANGE: [u8; 3] = [228, 132, 42];
const RED: [u8; 3] = [198, 44, 44];

fn sprite() -> [u8; SPRITE_BYTES] {
    [0u8; SPRITE_BYTES]
}

fn px(b: &mut [u8; SPRITE_BYTES], x: usize, y: usize, c: [u8; 3]) {
    let i = (y * SPRITE_DIM + x) * 4;
    b[i..i + 4].copy_from_slice(&[c[0], c[1], c[2], 255]);
}

fn rect(b: &mut [u8; SPRITE_BYTES], x0: usize, y0: usize, x1: usize, y1: usize, c: [u8; 3]) {
    for y in y0..=y1 {
        for x in x0..=x1 {
            px(b, x, y, c);
        }
    }
}

/// Bresenham line — deterministic integer rasterisation for leaf blades.
fn line(b: &mut [u8; SPRITE_BYTES], x0: i32, y0: i32, x1: i32, y1: i32, c: [u8; 3]) {
    let dx = (x1 - x0).abs();
    let dy = -(y1 - y0).abs();
    let sx = if x0 < x1 { 1 } else { -1 };
    let sy = if y0 < y1 { 1 } else { -1 };
    let mut err = dx + dy;
    let (mut x, mut y) = (x0, y0);
    loop {
        if (0..16).contains(&x) && (0..16).contains(&y) {
            px(b, x as usize, y as usize, c);
        }
        if x == x1 && y == y1 {
            break;
        }
        let e2 = 2 * err;
        if e2 >= dy {
            err += dy;
            x += sx;
        }
        if e2 <= dx {
            err += dx;
            y += sy;
        }
    }
}

// ---------------------------------------------------------------------------
// Synthetic crop sprites
// ---------------------------------------------------------------------------

/// Wheat-like sheaf: five 1px stalks (y5..15) each capped by a 3x3 grain
/// head (y2..4). Heads of neighbouring stalks touch, so the whole sprite is
/// one connected mass; the gold heads sit entirely in the top band.
fn wheat_like() -> [u8; SPRITE_BYTES] {
    let mut b = sprite();
    for &sx in &[2usize, 5, 8, 11, 14] {
        rect(&mut b, sx - 1, 2, sx + 1, 4, GOLD);
        rect(&mut b, sx, 5, sx, 15, GREEN);
    }
    b
}

/// Carrot-like bulb: five green leaf blades fanning from a base near
/// (7..8, 11) to tips in the top rows, over a solid orange bulb mass at the
/// bottom. The produce colour (orange) lives entirely in the lower half.
fn carrot_like() -> [u8; SPRITE_BYTES] {
    let mut b = sprite();
    for &(x1, y1) in &[(3i32, 3i32), (5, 2), (8, 1), (11, 3), (13, 5)] {
        line(&mut b, 7, 11, x1, y1, GREEN);
    }
    line(&mut b, 8, 12, 13, 5, GREEN);
    rect(&mut b, 4, 12, 11, 15, ORANGE);
    rect(&mut b, 5, 11, 10, 11, ORANGE);
    b
}

/// Tomato-like vine: a 2px stem (x7..8, y4..15) with leaf nubs, plus three
/// detached 2x2 red fruit pods in the upper region.
fn tomato_like() -> [u8; SPRITE_BYTES] {
    let mut b = sprite();
    rect(&mut b, 7, 4, 8, 15, GREEN);
    for &(x, y) in &[(6usize, 8usize), (9, 9), (6, 12), (9, 13)] {
        px(&mut b, x, y, GREEN);
    }
    for &(x, y) in &[(3usize, 4usize), (11, 5), (4, 9)] {
        rect(&mut b, x, y, x + 1, y + 1, RED);
    }
    b
}

/// Dandelion-like flower: thin stem, small leaf, round petal head — a
/// non-crop cross sprite.
fn flower_like() -> [u8; SPRITE_BYTES] {
    let mut b = sprite();
    rect(&mut b, 8, 8, 8, 15, GREEN);
    rect(&mut b, 5, 5, 10, 7, GOLD);
    rect(&mut b, 6, 4, 9, 5, GOLD);
    b
}

/// Sparse deterministic noise: ~20 single texels scattered by a fixed
/// stride pattern, alternating two hues.
fn noise_like() -> [u8; SPRITE_BYTES] {
    let mut b = sprite();
    for i in 0..20usize {
        let x = (i * 7 + 3) % 16;
        let y = (i * 5 + 1) % 16;
        px(&mut b, x, y, if i % 2 == 0 { GREEN } else { GOLD });
    }
    b
}

fn solid_like() -> [u8; SPRITE_BYTES] {
    let mut b = sprite();
    rect(&mut b, 0, 0, 15, 15, GREEN);
    b
}

// ---------------------------------------------------------------------------
// Input helpers
// ---------------------------------------------------------------------------

fn input<'a>(
    block_id: &'a str,
    states: &'a [(&'a str, &'a str)],
    tags: &'a [&'a str],
    parents: &'a [&'a str],
    rgba: Option<&'a [u8]>,
) -> CropInput<'a> {
    CropInput {
        block_id,
        states,
        tags,
        tag_flags: CropTagFlags::default(),
        parents,
        rgba,
    }
}

fn crop_input<'a>(rgba: Option<&'a [u8]>) -> CropInput<'a> {
    input(
        "minecraft:wheat",
        &[("age", "7"), ("max_age", "7")],
        &["minecraft:crops"],
        &["minecraft:block/crop"],
        rgba,
    )
}

// ---------------------------------------------------------------------------
// Step 1: the crop gate
// ---------------------------------------------------------------------------

#[test]
fn crop_gate_accepts_tags_age_and_parent() {
    let tex = wheat_like();
    // Strong tag alone is enough — no age, no parents.
    let d = classify(&input(
        "mymod:ghost_wheat",
        &[],
        &["c:crops"],
        &[],
        Some(&tex),
    ));
    assert!(d.is_crop);
    assert_eq!(d.archetype, Some(CropArchetype::SheafStalk));

    // `age` + crop/cross parent is enough with no tags at all.
    let d = classify(&input(
        "mymod:nameless_thing",
        &[("age", "3"), ("max_age", "7")],
        &[],
        &["some:namespace:block/cross"],
        Some(&tex),
    ));
    assert!(d.is_crop);
    assert_eq!(d.stage, 3);
    assert_eq!(d.max_stage, 7);

    // Pre-resolved tag flags union with the tag list.
    let mut flagged = input("x:y", &[], &[], &[], Some(&tex));
    flagged.tag_flags.c_crops = true;
    assert!(classify(&flagged).is_crop);

    // A supporting tag corroborated by `age` counts as a crop.
    let d = classify(&input(
        "minecraft:cave_vines",
        &[("age", "2")],
        &["minecraft:bee_growables"],
        &[],
        None,
    ));
    assert!(d.is_crop);
    assert_eq!(d.archetype, None, "no texture -> no archetype");
}

#[test]
fn crop_gate_rejects_flowers_saplings_grass_mushrooms_vines() {
    let tex = flower_like();
    let cases = [
        // Flower: cross parent, no age, no crop tags.
        input(
            "minecraft:dandelion",
            &[],
            &["minecraft:flowers"],
            &["minecraft:block/cross"],
            Some(&tex),
        ),
        // Sapling: `stage`, not `age`.
        input(
            "minecraft:oak_sapling",
            &[("stage", "0")],
            &["minecraft:saplings"],
            &["minecraft:block/cross"],
            Some(&tex),
        ),
        // Grass: tinted cross parent, no age.
        input(
            "minecraft:short_grass",
            &[],
            &[],
            &["minecraft:block/tinted_cross"],
            Some(&tex),
        ),
        // Mushroom: cross parent, no age.
        input(
            "minecraft:red_mushroom",
            &[],
            &[],
            &["minecraft:block/cross"],
            Some(&tex),
        ),
        // Vine block: facing states, non-crop parent.
        input(
            "minecraft:vine",
            &[("north", "true"), ("up", "false")],
            &[],
            &["minecraft:block/vine"],
            Some(&tex),
        ),
        // Support tag alone is not enough — no age, no crop parent.
        input(
            "mymod:plain_block",
            &[],
            &["minecraft:bee_growables"],
            &["minecraft:block/cube"],
            Some(&tex),
        ),
    ];
    for case in &cases {
        let id = case.block_id;
        let d = classify(case);
        assert!(!d.is_crop, "{id} must not classify as a crop");
        assert_eq!(d.archetype, None);
        assert_eq!(d.confidence, 0);
    }
}

#[test]
fn crop_tags_override_non_crop_look() {
    // "unless crop tags say otherwise": a flower-shaped sprite tagged as a
    // crop still passes the gate; the texture then decides the archetype.
    let tex = flower_like();
    let d = classify(&input(
        "minecraft:dandelion",
        &[],
        &["minecraft:crops"],
        &["minecraft:block/cross"],
        Some(&tex),
    ));
    assert!(d.is_crop);
    assert_eq!(
        d.archetype, None,
        "flower sprite is too weak for an archetype"
    );
    assert!(d.reasons & REASON_CROP_TAG != 0);
    assert!(d.reasons & REASON_LOW_CONFIDENCE != 0);
}

// ---------------------------------------------------------------------------
// Step 2: archetype classification
// ---------------------------------------------------------------------------

#[test]
fn tall_strands_classify_as_sheaf_stalk() {
    let tex = wheat_like();
    let d = classify(&crop_input(Some(&tex)));
    assert!(d.is_crop);
    assert_eq!(d.archetype, Some(CropArchetype::SheafStalk));
    assert!(d.confidence >= 80, "confidence {}", d.confidence);
    assert!(d.reasons & REASON_TALL_COLUMNS != 0);
    assert!(d.reasons & REASON_TOP_PRODUCE != 0);
    assert!(d.reasons & REASON_WIDE_TOP != 0);
}

#[test]
fn low_bulb_with_leaves_classifies_as_bulb_leaf() {
    let tex = carrot_like();
    let d = classify(&crop_input(Some(&tex)));
    assert!(d.is_crop);
    assert_eq!(d.archetype, Some(CropArchetype::BulbLeaf));
    assert!(d.confidence >= 60, "confidence {}", d.confidence);
    assert!(d.reasons & REASON_BOTTOM_PRODUCE != 0);
    assert!(d.reasons & REASON_WIDE_BASE != 0);
}

#[test]
fn vine_with_coloured_dots_classifies_as_vine_fruit() {
    let tex = tomato_like();
    let d = classify(&crop_input(Some(&tex)));
    assert!(d.is_crop);
    assert_eq!(d.archetype, Some(CropArchetype::VineFruit));
    assert!(d.confidence >= 80, "confidence {}", d.confidence);
    assert!(d.reasons & REASON_DOT_ISOLATES != 0);
}

#[test]
fn degenerate_sprites_yield_no_archetype() {
    // Empty sprite: crop gate still passes, archetype fails closed.
    let empty = sprite();
    let d = classify(&crop_input(Some(&empty)));
    assert!(d.is_crop);
    assert_eq!(d.archetype, None);
    assert_eq!(d.confidence, 0);
    assert!(d.reasons & REASON_TEXTURE_DEGENERATE != 0);

    // Solid slab: same fail-closed path.
    let solid = solid_like();
    let d = classify(&crop_input(Some(&solid)));
    assert!(d.is_crop);
    assert_eq!(d.archetype, None);
    assert!(d.reasons & REASON_TEXTURE_DEGENERATE != 0);

    // Salt-and-pepper noise: enough texels to analyse, zero structure.
    let noise = noise_like();
    let d = classify(&crop_input(Some(&noise)));
    assert!(d.is_crop);
    assert_eq!(d.archetype, None);
    assert!(d.confidence < 80, "noise confidence {}", d.confidence);

    // Missing and truncated textures behave identically.
    let d = classify(&crop_input(None));
    assert!(d.is_crop);
    assert_eq!(d.archetype, None);
    assert!(d.reasons & REASON_NO_TEXTURE != 0);
    let short = [0u8; 512];
    let d = classify(&crop_input(Some(&short)));
    assert_eq!(d.archetype, None);
    assert!(d.reasons & REASON_NO_TEXTURE != 0);
}

#[test]
fn stage_progression_keeps_archetype_and_reports_stage() {
    let tex = wheat_like();
    let mut archetypes = Vec::new();
    for age in 0..=7u8 {
        let age_s = age.to_string();
        let st: &[(&str, &str)] = &[("age", age_s.as_str()), ("max_age", "7")];
        let d = classify(&input(
            "minecraft:wheat",
            st,
            &["minecraft:crops"],
            &["minecraft:block/crop"],
            Some(&tex),
        ));
        assert!(d.is_crop);
        assert_eq!(d.archetype, Some(CropArchetype::SheafStalk), "age {age}");
        assert_eq!(d.stage, age);
        assert_eq!(d.max_stage, 7);
        archetypes.push(d.archetype);
    }
    assert!(archetypes
        .iter()
        .all(|a| *a == Some(CropArchetype::SheafStalk)));
}

#[test]
fn shuffled_tag_order_is_invariant() {
    let tex = wheat_like();
    let a = classify(&input(
        "minecraft:wheat",
        &[("age", "7")],
        &["minecraft:crops", "c:crops", "minecraft:bee_growables"],
        &["minecraft:block/crop"],
        Some(&tex),
    ));
    let b = classify(&input(
        "minecraft:wheat",
        &[("age", "7")],
        &["minecraft:bee_growables", "c:crops", "minecraft:crops"],
        &["minecraft:block/crop"],
        Some(&tex),
    ));
    let c = classify(&input(
        "minecraft:wheat",
        &[("age", "7")],
        &["#C:CROPS", "minecraft:maintains_farmland"],
        &["minecraft:block/crop"],
        Some(&tex),
    ));
    assert_eq!(a, b);
    assert_eq!(decision_fingerprint(&a), decision_fingerprint(&b));
    // `#`-prefixed and differently-cased tags normalise identically.
    assert_eq!(a.is_crop, c.is_crop);
    assert_eq!(a.archetype, c.archetype);
    assert_eq!(a.confidence, c.confidence);
}

#[test]
fn archetype_maps_onto_crop3d_archetype() {
    assert_eq!(
        Crop3dArchetype::from(CropArchetype::SheafStalk),
        Crop3dArchetype::SheafStalk
    );
    assert_eq!(
        Crop3dArchetype::from(CropArchetype::BulbLeaf),
        Crop3dArchetype::BulbLeaf
    );
    assert_eq!(
        Crop3dArchetype::from(CropArchetype::VineFruit),
        Crop3dArchetype::VineFruit
    );
    assert_eq!(CropArchetype::SheafStalk.as_str(), "sheaf_stalk");
}

#[test]
fn classify_is_deterministic() {
    let tex = tomato_like();
    let inp = crop_input(Some(&tex));
    let a = classify(&inp);
    let b = classify(&inp);
    assert_eq!(a, b);
    assert_eq!(decision_fingerprint(&a), decision_fingerprint(&b));
}

// ---------------------------------------------------------------------------
// Golden fingerprints
// ---------------------------------------------------------------------------

#[test]
fn golden_decision_fingerprints() {
    let wheat = wheat_like();
    let carrot = carrot_like();
    let tomato = tomato_like();
    let flower = flower_like();
    let noise = noise_like();
    let empty = sprite();

    let cases: [(CropDecision, &str); 8] = [
        (classify(&crop_input(Some(&wheat))), "wheat sheaf"),
        (
            classify(&input(
                "minecraft:carrots",
                &[("age", "7"), ("max_age", "7")],
                &["minecraft:crops"],
                &["minecraft:block/crop"],
                Some(&carrot),
            )),
            "carrot bulb",
        ),
        (
            classify(&input(
                "mymod:tomato_vine",
                &[("age", "4"), ("max_age", "4")],
                &[],
                &["mymod:block/cross"],
                Some(&tomato),
            )),
            "tomato vine (modded, tagless)",
        ),
        (
            classify(&input(
                "minecraft:dandelion",
                &[],
                &["minecraft:flowers"],
                &["minecraft:block/cross"],
                Some(&flower),
            )),
            "flower non-crop",
        ),
        (classify(&crop_input(Some(&noise))), "noise no-archetype"),
        (classify(&crop_input(Some(&empty))), "empty degenerate"),
        (classify(&crop_input(None)), "no texture"),
        (
            classify(&input(
                "minecraft:wheat",
                &[("age", "3"), ("age_max", "7")],
                &["minecraft:crops"],
                &["minecraft:block/crop"],
                Some(&wheat),
            )),
            "age_max alias, stage 3",
        ),
    ];

    // Golden pins: any drift in gate, archetype, confidence, stage or
    // reasons flips a hash. EXPECTED documents what each pin encodes.
    const EXPECTED: [(bool, Option<CropArchetype>); 8] = [
        (true, Some(CropArchetype::SheafStalk)),
        (true, Some(CropArchetype::BulbLeaf)),
        (true, Some(CropArchetype::VineFruit)),
        (false, None),
        (true, None),
        (true, None),
        (true, None),
        (true, Some(CropArchetype::SheafStalk)),
    ];
    const GOLDENS: [u64; 8] = [
        0x1586_c021_6431_26d7, // wheat sheaf
        0x6519_c268_863e_be3b, // carrot bulb
        0xb379_b1ef_8d16_5b42, // tomato vine (modded, tagless)
        0xe62f_21f7_72e2_4537, // flower non-crop
        0xae86_cc97_3415_7a87, // noise no-archetype
        0xfb1d_440a_bcfa_49f3, // empty degenerate
        0xfbc7_c2ff_f642_b7d3, // no texture
        0x9a25_8428_4c9a_8d63, // age_max alias, stage 3
    ];
    for (i, (d, label)) in cases.iter().enumerate() {
        assert_eq!(
            (d.is_crop, d.archetype),
            EXPECTED[i],
            "{label} decision drifted"
        );
        let fp = decision_fingerprint(d);
        assert_eq!(fp, GOLDENS[i], "{label} fingerprint drifted: {fp:#018x}");
    }
}
