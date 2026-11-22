// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later

//! End-to-end tests for automatic mod-crop classification: a caller that
//! supplies [`CropInput`] facts on [`BlockVisualInput::crop_auto_input`]
//! gets `crop_archetype::classify` -> `build_3d_crop_mesh` routing inside
//! [`evaluate_block_visual_pipeline`] — no hardcoded mod name list anywhere.
//!
//! Every sprite below is a synthetic 16x16 mask drawn from first
//! principles; zero Mojang or mod texture bytes are referenced.

use mc_spine_client_mesh::crop_archetype::{self, CropArchetype, CropInput, CropTagFlags};
use mc_spine_client_mesh::mesher::{
    evaluate_block_visual_pipeline, BlockVisualInput, VanillaPlusPipelineConfig,
};
use mc_spine_client_mesh::vanilla_plus_3d::{
    build_3d_crop_mesh, build_3d_cross_mesh, mesh_fingerprint, Crop3dSpec, CrossLayout,
    MeshUpgradeDecision, VanillaPlusCategory,
};
use mc_spine_client_mesh::vanilla_plus_nature::NatureModRegistry;

// ---------------------------------------------------------------------------
// Synthetic sprites (mirrors crop_archetype_tests fixtures)
// ---------------------------------------------------------------------------

const SPRITE_DIM: usize = 16;
const SPRITE_BYTES: usize = SPRITE_DIM * SPRITE_DIM * 4;

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

/// Wheat/rice sheaf: five stalks capped by gold grain heads.
fn wheat_like() -> [u8; SPRITE_BYTES] {
    let mut b = sprite();
    for &sx in &[2usize, 5, 8, 11, 14] {
        rect(&mut b, sx - 1, 2, sx + 1, 4, GOLD);
        rect(&mut b, sx, 5, sx, 15, GREEN);
    }
    b
}

/// Carrot/onion bulb: green leaf blades over a solid orange ground mass.
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

/// Tomato vine: 2px stem with leaf nubs plus three detached red pods.
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

/// Dandelion-like flower: thin stem, small leaf, round petal head.
fn flower_like() -> [u8; SPRITE_BYTES] {
    let mut b = sprite();
    rect(&mut b, 8, 8, 8, 15, GREEN);
    rect(&mut b, 5, 5, 10, 7, GOLD);
    rect(&mut b, 6, 4, 9, 5, GOLD);
    b
}

/// Sparse deterministic noise: ~20 single texels, alternating two hues.
fn noise_like() -> [u8; SPRITE_BYTES] {
    let mut b = sprite();
    for i in 0..20usize {
        let x = (i * 7 + 3) % 16;
        let y = (i * 5 + 1) % 16;
        px(&mut b, x, y, if i % 2 == 0 { GREEN } else { GOLD });
    }
    b
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn three_d_cfg() -> VanillaPlusPipelineConfig {
    let mut cfg = VanillaPlusPipelineConfig::default();
    cfg.three_d.enabled = true;
    cfg
}

fn crop_input<'a>(
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

/// FNV-1a 64-bit — the same constants `mesher::auto_crop_spec` applies to
/// `input.block_id` when it derives the `Crop3dSpec` seed. Replicated here
/// so the expected mesh can be rebuilt bit-for-bit.
fn fnv1a64(bytes: &[u8]) -> u64 {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut h = OFFSET;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(PRIME);
    }
    h
}

/// The `Crop3dSpec` the pipeline must derive for `block_id` + `decision`.
fn expected_spec(
    block_id: &str,
    block_pos: [i32; 3],
    decision: &crop_archetype::CropDecision,
) -> Crop3dSpec {
    Crop3dSpec {
        archetype: decision.archetype.expect("test archetype").into(),
        stage: decision.stage,
        max_stage: decision.max_stage,
        seed: fnv1a64(block_id.as_bytes()),
        block_pos,
    }
}

fn pipeline_input<'a>(
    block_id: &'a str,
    crop: CropInput<'a>,
    block_pos: [i32; 3],
) -> BlockVisualInput<'a> {
    BlockVisualInput {
        block_id,
        is_cross_model: true,
        distance_sq: 16.0, // 4 blocks, inside the 24-block LOD cutoff.
        block_pos,
        crop_auto_input: Some(crop),
        ..Default::default()
    }
}

// ---------------------------------------------------------------------------
// Automatic mod-crop classification end-to-end
// ---------------------------------------------------------------------------

#[test]
fn mod_crops_classify_and_build_3d_mesh_automatically() {
    let cfg = three_d_cfg();
    let reg = NatureModRegistry::new();
    let pos = [5, 70, -11];

    let rice_tex = wheat_like();
    let onion_tex = carrot_like();
    let tomato_tex = tomato_like();

    // Each case exercises a different gate path: strong `c:crops` tag,
    // `minecraft:crops` tag, and tagless `age` + crop/cross parent — all
    // resolved from caller facts, never from a block-name list.
    let cases: [(&str, CropInput<'_>, CropArchetype, &str); 3] = [
        (
            "farmersdelight:rice_crop",
            crop_input(
                "farmersdelight:rice_crop",
                &[("age", "3"), ("max_age", "7")],
                &["c:crops"],
                &["farmersdelight:block/crop"],
                Some(&rice_tex),
            ),
            CropArchetype::SheafStalk,
            "rice sheaf via c:crops tag",
        ),
        (
            "modcrops:onion_crop",
            crop_input(
                "modcrops:onion_crop",
                &[("age", "7"), ("max_age", "7")],
                &["minecraft:crops"],
                &["modcrops:block/cross"],
                Some(&onion_tex),
            ),
            CropArchetype::BulbLeaf,
            "onion bulb via minecraft:crops tag",
        ),
        (
            "modcrops:tomato_vine",
            crop_input(
                "modcrops:tomato_vine",
                &[("age", "4"), ("max_age", "4")],
                &[],
                &["modcrops:block/cross"],
                Some(&tomato_tex),
            ),
            CropArchetype::VineFruit,
            "tomato vine via age + parent, no tags",
        ),
    ];

    for (id, crop, want_arch, label) in cases {
        let inp = pipeline_input(id, crop, pos);
        let r = evaluate_block_visual_pipeline(&inp, &cfg, &reg);
        assert_eq!(
            r.upgrade_decision,
            MeshUpgradeDecision::Upgrade3d(VanillaPlusCategory::CropsAndFlora),
            "{label}: {id} did not reach the 3D crop category"
        );
        let mesh = r
            .upgrade_mesh
            .unwrap_or_else(|| panic!("{label}: {id} emitted no upgrade mesh"));
        assert!(!mesh.quads.is_empty(), "{label}: {id} empty mesh");

        // Prove the full classify -> spec -> build_3d_crop_mesh path ran:
        // the emitted mesh must be bit-identical to the spec the classifier
        // decision implies.
        let decision = crop_archetype::classify(&crop);
        assert!(decision.is_crop, "{label}: {id} not a crop");
        assert_eq!(decision.archetype, Some(want_arch), "{label}: {id}");
        let want = build_3d_crop_mesh(&expected_spec(id, pos, &decision));
        assert_eq!(
            mesh_fingerprint(&mesh),
            mesh_fingerprint(&want),
            "{label}: {id} mesh is not the classified crop build"
        );
    }
}

#[test]
fn ambiguous_and_flower_inputs_fall_back_to_cross_mesh() {
    let cfg = three_d_cfg();
    let reg = NatureModRegistry::new();
    let pos = [2, 3, 4];

    // A real flower: the crop gate rejects it outright, so the generic
    // diagonal cross fallback runs even though `crop_auto_input` exists.
    let flower_tex = flower_like();
    let dandelion = crop_input(
        "minecraft:dandelion",
        &[],
        &["minecraft:flowers"],
        &["minecraft:block/cross"],
        Some(&flower_tex),
    );
    let inp = pipeline_input("minecraft:dandelion", dandelion, pos);
    let r = evaluate_block_visual_pipeline(&inp, &cfg, &reg);
    assert_eq!(
        r.upgrade_decision,
        MeshUpgradeDecision::Upgrade3d(VanillaPlusCategory::CropsAndFlora)
    );
    let mesh = r.upgrade_mesh.expect("dandelion cross fallback");
    let want = build_3d_cross_mesh(&[255u8; 256], CrossLayout::Diagonal, pos);
    assert_eq!(mesh_fingerprint(&mesh), mesh_fingerprint(&want));

    // A crop-tagged block whose sprite carries no archetype evidence:
    // `is_crop` passes but `archetype` stays `None`, so it still falls back
    // — and `*_crop` ids take the `#` grid layout, proving the fallback
    // honours `cross_layout_for`.
    let noise_tex = noise_like();
    let mystery = crop_input(
        "mymod:mystery_crop",
        &[("age", "5"), ("max_age", "7")],
        &["c:crops"],
        &["mymod:block/crop"],
        Some(&noise_tex),
    );
    let decision = crop_archetype::classify(&mystery);
    assert!(decision.is_crop);
    assert_eq!(decision.archetype, None, "noise must stay ambiguous");
    let inp = pipeline_input("mymod:mystery_crop", mystery, pos);
    let r = evaluate_block_visual_pipeline(&inp, &cfg, &reg);
    let mesh = r.upgrade_mesh.expect("ambiguous crop cross fallback");
    let want = build_3d_cross_mesh(&[255u8; 256], CrossLayout::HashGrid, pos);
    assert_eq!(mesh_fingerprint(&mesh), mesh_fingerprint(&want));
}

#[test]
fn explicit_crop_3d_spec_wins_over_auto_input() {
    let cfg = three_d_cfg();
    let reg = NatureModRegistry::new();
    let wheat_tex = wheat_like();
    let crop = crop_input(
        "minecraft:wheat",
        &[("age", "7"), ("max_age", "7")],
        &["minecraft:crops"],
        &["minecraft:block/crop"],
        Some(&wheat_tex),
    );
    let explicit = Crop3dSpec {
        archetype: mc_spine_client_mesh::vanilla_plus_3d::Crop3dArchetype::VineFruit,
        stage: 2,
        max_stage: 7,
        seed: 42,
        block_pos: [9, 9, 9],
    };
    let inp = BlockVisualInput {
        block_id: "minecraft:wheat",
        is_cross_model: true,
        distance_sq: 16.0,
        block_pos: [1, 2, 3],
        crop_3d: Some(explicit),
        crop_auto_input: Some(crop),
        ..Default::default()
    };
    let r = evaluate_block_visual_pipeline(&inp, &cfg, &reg);
    let mesh = r.upgrade_mesh.expect("explicit spec mesh");
    assert_eq!(
        mesh_fingerprint(&mesh),
        mesh_fingerprint(&build_3d_crop_mesh(&explicit)),
        "explicit crop_3d must win over crop_auto_input"
    );
}

#[test]
fn resource_pack_deference_still_beats_auto_crop() {
    let cfg = three_d_cfg();
    let reg = NatureModRegistry::new();
    let wheat_tex = wheat_like();
    let crop = crop_input(
        "farmersdelight:rice_crop",
        &[("age", "7"), ("max_age", "7")],
        &["c:crops"],
        &["farmersdelight:block/crop"],
        Some(&wheat_tex),
    );

    // An enabled external resource pack always wins: the whole pipeline
    // short-circuits to VANILLA before classification ever runs.
    let inp = BlockVisualInput {
        has_external_custom_override: true,
        ..pipeline_input("farmersdelight:rice_crop", crop, [0, 0, 0])
    };
    let r = evaluate_block_visual_pipeline(&inp, &cfg, &reg);
    assert!(r.is_vanilla());
    assert!(r.upgrade_mesh.is_none());

    // A custom 3D resource-pack model defers inside the 3D pack itself.
    let inp = BlockVisualInput {
        has_custom_3d_resourcepack_model: true,
        ..pipeline_input("farmersdelight:rice_crop", crop, [0, 0, 0])
    };
    let r = evaluate_block_visual_pipeline(&inp, &cfg, &reg);
    assert_eq!(r.upgrade_decision, MeshUpgradeDecision::DeferToResourcePack);
    assert!(r.upgrade_mesh.is_none());
}

#[test]
fn auto_crop_is_deterministic_across_calls_and_tag_order() {
    let cfg = three_d_cfg();
    let reg = NatureModRegistry::new();
    let wheat_tex = wheat_like();

    let mk = |tags: &'static [&'static str]| {
        crop_input(
            "mymod:generic_grain",
            &[("age", "6"), ("max_age", "7")],
            tags,
            &["mymod:block/crop"],
            Some(&wheat_tex),
        )
    };
    let a = pipeline_input(
        "mymod:generic_grain",
        mk(&["c:crops", "minecraft:bee_growables"]),
        [4, 4, 4],
    );
    let b = pipeline_input(
        "mymod:generic_grain",
        mk(&["minecraft:bee_growables", "c:crops"]),
        [4, 4, 4],
    );

    let ra = evaluate_block_visual_pipeline(&a, &cfg, &reg);
    let rb = evaluate_block_visual_pipeline(&b, &cfg, &reg);
    let ma = ra.upgrade_mesh.expect("mesh a");
    let mb = rb.upgrade_mesh.expect("mesh b");
    assert_eq!(mesh_fingerprint(&ma), mesh_fingerprint(&mb));
    assert_eq!(ma, mb);

    // Repeated calls are bit-identical.
    let rc = evaluate_block_visual_pipeline(&a, &cfg, &reg);
    assert_eq!(
        mesh_fingerprint(&rc.upgrade_mesh.unwrap()),
        mesh_fingerprint(&ma)
    );

    // The block position feeds the pose jitter: moving the block must move
    // the mesh, proving the spec's block_pos actually reaches the builder.
    let moved = pipeline_input("mymod:generic_grain", mk(&["c:crops"]), [5, 4, 4]);
    let rm = evaluate_block_visual_pipeline(&moved, &cfg, &reg);
    assert_ne!(
        mesh_fingerprint(&rm.upgrade_mesh.unwrap()),
        mesh_fingerprint(&ma),
        "block_pos must affect the auto-crop pose"
    );
}
