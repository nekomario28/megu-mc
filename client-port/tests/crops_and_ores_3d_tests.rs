// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later

use mc_spine_client_mesh::mesher::{
    evaluate_block_visual_pipeline, BlockVisualInput, VanillaPlusPipelineConfig,
};
use mc_spine_client_mesh::vanilla_plus_3d::*;
use mc_spine_client_mesh::vanilla_plus_nature::NatureModRegistry;
use mc_spine_client_mesh::Direction;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn spec(archetype: Crop3dArchetype, stage: u8) -> Crop3dSpec {
    Crop3dSpec {
        archetype,
        stage,
        max_stage: 7,
        seed: 0x5eed_cafe_1234,
        block_pos: [3, 64, -7],
    }
}

fn max_y(mesh: &ExtrudedMesh) -> f32 {
    mesh.quads
        .iter()
        .flat_map(|q| q.positions)
        .fold(0.0f32, |m, p| m.max(p[1]))
}

fn assert_finite(mesh: &ExtrudedMesh) {
    for (i, q) in mesh.quads.iter().enumerate() {
        for p in &q.positions {
            for c in p {
                assert!(c.is_finite(), "quad {i} has non-finite position {p:?}");
            }
        }
        for c in &q.normal {
            assert!(c.is_finite(), "quad {i} has non-finite normal");
        }
    }
}

fn assert_block_bounds(mesh: &ExtrudedMesh, slack: f32) {
    for (i, q) in mesh.quads.iter().enumerate() {
        for p in &q.positions {
            for &c in p {
                assert!(
                    (-slack..=1.0 + slack).contains(&c),
                    "quad {i} coordinate {c} outside block bounds"
                );
            }
        }
    }
}

fn vein_mask() -> [u8; 256] {
    // Two disconnected vein blobs like a real ore sprite.
    let mut m = [0u8; 256];
    for y in 2..6 {
        for x in 3..8 {
            m[y * 16 + x] = 200;
        }
    }
    for y in 9..13 {
        for x in 10..14 {
            m[y * 16 + x] = 255;
        }
    }
    m
}

fn three_d_cfg() -> VanillaPlusPipelineConfig {
    let mut cfg = VanillaPlusPipelineConfig::default();
    cfg.three_d.enabled = true;
    cfg
}

// ---------------------------------------------------------------------------
// Procedural 3D crop archetypes
// ---------------------------------------------------------------------------

#[test]
fn sheaf_stalk_grows_monotonically() {
    let mut prev_h = -1.0f32;
    let mut prev_quads = 0usize;
    for stage in 0..=7u8 {
        let mesh = build_3d_crop_mesh(&spec(Crop3dArchetype::SheafStalk, stage));
        assert!(!mesh.quads.is_empty(), "empty sheaf at stage {stage}");
        let h = max_y(&mesh);
        assert!(
            h > prev_h,
            "stage {stage} height {h} did not grow over {prev_h}"
        );
        prev_h = h;
        assert!(mesh.quad_count() >= prev_quads);
        prev_quads = mesh.quad_count();
        assert_finite(&mesh);
        assert_block_bounds(&mesh, 1e-5);
    }
    // Grain heads appear only in the upper half of growth.
    let young = build_3d_crop_mesh(&spec(Crop3dArchetype::SheafStalk, 3));
    let mature = build_3d_crop_mesh(&spec(Crop3dArchetype::SheafStalk, 7));
    assert!(mature.quad_count() > young.quad_count());
    assert!(mature.quads.iter().any(|q| q.tint_index == 0));
}

#[test]
fn bulb_leaf_grows_monotonically() {
    let mut prev_h = -1.0f32;
    for stage in 0..=7u8 {
        let mesh = build_3d_crop_mesh(&spec(Crop3dArchetype::BulbLeaf, stage));
        assert!(!mesh.quads.is_empty(), "empty bulb at stage {stage}");
        let h = max_y(&mesh);
        assert!(
            h > prev_h,
            "stage {stage} height {h} did not grow over {prev_h}"
        );
        prev_h = h;
        assert_finite(&mesh);
        assert_block_bounds(&mesh, 1e-5);
    }
    // The ground bulb is always present.
    for stage in [0u8, 3, 7] {
        let mesh = build_3d_crop_mesh(&spec(Crop3dArchetype::BulbLeaf, stage));
        assert!(mesh.quads.iter().any(|q| q.tint_index == 0));
    }
}

#[test]
fn vine_fruit_pods_emerge_in_upper_stages() {
    let mut prev_h = -1.0f32;
    let mut prev_quads = 0usize;
    for stage in 0..=7u8 {
        let mesh = build_3d_crop_mesh(&spec(Crop3dArchetype::VineFruit, stage));
        assert!(!mesh.quads.is_empty(), "empty vine at stage {stage}");
        let h = max_y(&mesh);
        assert!(
            h > prev_h,
            "stage {stage} height {h} did not grow over {prev_h}"
        );
        prev_h = h;
        assert!(mesh.quad_count() >= prev_quads);
        prev_quads = mesh.quad_count();
        assert_finite(&mesh);
        assert_block_bounds(&mesh, 1e-5);
    }
    // Fruit pods (tint 1) exist only in the upper stages.
    let young = build_3d_crop_mesh(&spec(Crop3dArchetype::VineFruit, 3));
    assert!(young.quads.iter().all(|q| q.tint_index != 1));
    let mature = build_3d_crop_mesh(&spec(Crop3dArchetype::VineFruit, 7));
    assert_eq!(
        mature.quads.iter().filter(|q| q.tint_index == 1).count(),
        18,
        "three pod boxes x six faces"
    );
}

#[test]
fn crop_meshes_are_deterministic() {
    for arch in [
        Crop3dArchetype::SheafStalk,
        Crop3dArchetype::BulbLeaf,
        Crop3dArchetype::VineFruit,
    ] {
        let a = build_3d_crop_mesh(&spec(arch, 5));
        let b = build_3d_crop_mesh(&spec(arch, 5));
        assert_eq!(a, b, "{arch:?} not deterministic");
        assert_eq!(mesh_fingerprint(&a), mesh_fingerprint(&b));

        // A different seed produces a different pose.
        let mut other = spec(arch, 5);
        other.seed = 0xdead_beef;
        assert_ne!(
            mesh_fingerprint(&a),
            mesh_fingerprint(&build_3d_crop_mesh(&other)),
            "{arch:?} ignores seed"
        );
        // A different block position varies the pose too.
        let mut moved = spec(arch, 5);
        moved.block_pos = [4, 64, -7];
        assert_ne!(
            mesh_fingerprint(&a),
            mesh_fingerprint(&build_3d_crop_mesh(&moved)),
            "{arch:?} ignores block_pos"
        );
    }
}

#[test]
fn degenerate_crop_spec_fails_closed() {
    let mesh = build_3d_crop_mesh(&Crop3dSpec {
        archetype: Crop3dArchetype::SheafStalk,
        stage: 9,
        max_stage: 0,
        seed: 0,
        block_pos: [0, 0, 0],
    });
    assert!(!mesh.quads.is_empty());
    assert_finite(&mesh);
    assert_block_bounds(&mesh, 1e-5);
    // max_stage == 0 clamps to the stage-0 silhouette.
    let zero = build_3d_crop_mesh(&Crop3dSpec {
        archetype: Crop3dArchetype::BulbLeaf,
        stage: 0,
        max_stage: 0,
        seed: 7,
        block_pos: [1, 2, 3],
    });
    let min_stage = build_3d_crop_mesh(&Crop3dSpec {
        archetype: Crop3dArchetype::BulbLeaf,
        stage: 0,
        max_stage: 5,
        seed: 7,
        block_pos: [1, 2, 3],
    });
    assert_eq!(mesh_fingerprint(&zero), mesh_fingerprint(&min_stage));
}

#[test]
fn crop_golden_fingerprints_pin_every_stage() {
    // Golden pins: any drift in crop geometry flips a hash.
    const GOLDENS: [[u64; 8]; 3] = [
        // SheafStalk stages 0..=7
        [
            0xbcea_9347_b5af_7871,
            0xd565_aa6f_3385_cd11,
            0xaae8_1d4d_6a44_c701,
            0x8ead_e68d_d593_1265,
            0xfb03_8a1a_e69d_8dc9,
            0x8438_9b50_9427_eda9,
            0x6c33_99d2_c9c5_df11,
            0xf34f_1c72_e992_6ce1,
        ],
        // BulbLeaf stages 0..=7
        [
            0xbaf7_f44d_4237_b4c9,
            0xea43_46fd_12b2_99f5,
            0x88e0_3910_c081_a6b9,
            0x759c_1919_c6f6_8c59,
            0xc7b0_80ba_0bfc_c6a5,
            0x1f7b_01f8_cb3c_f2dd,
            0x8aa0_2aa7_26ec_0c05,
            0x938c_5eb9_13b1_8ba5,
        ],
        // VineFruit stages 0..=7
        [
            0x994a_b12d_906f_c1a1,
            0x8760_38fc_c37d_8271,
            0x4765_a7fa_be05_b511,
            0x6a6b_1de3_ee36_78f9,
            0x39d9_edd2_61b5_ec75,
            0x5613_9373_da1c_6905,
            0x1c4d_34ae_fb32_780d,
            0x2197_fdac_7a76_44e1,
        ],
    ];
    for (row, arch) in [
        Crop3dArchetype::SheafStalk,
        Crop3dArchetype::BulbLeaf,
        Crop3dArchetype::VineFruit,
    ]
    .iter()
    .enumerate()
    {
        for stage in 0..=7u8 {
            let mesh = build_3d_crop_mesh(&spec(*arch, stage));
            assert_eq!(
                mesh_fingerprint(&mesh),
                GOLDENS[row][stage as usize],
                "{arch:?} stage {stage} fingerprint drifted"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// 0.5-texel ore relief
// ---------------------------------------------------------------------------

#[test]
fn ore_relief_depth_is_exactly_half_texel() {
    assert_eq!(ORE_RELIEF_DEPTH, 0.5 / 16.0);
    let mesh = build_3d_ore_relief(&vein_mask(), Direction::Up.bit());
    assert!(!mesh.quads.is_empty());
    // Every vertex sits in the [1.0, 1.0 + depth] slab.
    for q in &mesh.quads {
        for p in &q.positions {
            assert!((1.0 - 1e-6..=1.0 + ORE_RELIEF_DEPTH + 1e-6).contains(&p[1]));
        }
    }
    let top = max_y(&mesh);
    assert!(
        (top - (1.0 + ORE_RELIEF_DEPTH)).abs() < 1e-7,
        "relief peak {top} != 1 + half texel"
    );
}

#[test]
fn ore_relief_hidden_faces_emit_nothing() {
    assert_eq!(build_3d_ore_relief(&vein_mask(), 0).quad_count(), 0);
    // Bits outside the 6-face mask are ignored too.
    assert_eq!(build_3d_ore_relief(&vein_mask(), 0xC0).quad_count(), 0);
}

#[test]
fn ore_relief_single_face_stays_on_that_face() {
    let single = build_3d_ore_relief(&vein_mask(), Direction::Up.bit());
    let all = build_3d_ore_relief(&vein_mask(), 0x3F);
    // Each exposed face contributes the same plate; six faces = 6x quads.
    assert_eq!(all.quad_count(), single.quad_count() * 6);

    for (dir, lo, hi) in [
        (Direction::Up, 1.0, 1.0 + ORE_RELIEF_DEPTH),
        (Direction::Down, -ORE_RELIEF_DEPTH, 0.0),
    ] {
        let mesh = build_3d_ore_relief(&vein_mask(), dir.bit());
        assert!(!mesh.quads.is_empty(), "{dir:?} produced nothing");
        for q in &mesh.quads {
            for p in &q.positions {
                assert!(
                    (lo - 1e-6..=hi + 1e-6).contains(&p[1]),
                    "{dir:?} vertex y={} outside relief slab",
                    p[1]
                );
            }
        }
    }
    // Side faces keep geometry within their thin outward slabs.
    let east = build_3d_ore_relief(&vein_mask(), Direction::East.bit());
    for q in &east.quads {
        for p in &q.positions {
            assert!((1.0 - 1e-6..=1.0 + ORE_RELIEF_DEPTH + 1e-6).contains(&p[0]));
        }
    }
}

#[test]
fn ore_relief_fails_closed_on_empty_input() {
    let empty = [0u8; 256];
    assert_eq!(build_3d_ore_relief(&empty, 0x3F).quad_count(), 0);
    // A non-ore block never classifies for the category at all.
    assert_eq!(classify_vanilla_plus_block("minecraft:stone", false), None);
    assert_eq!(classify_vanilla_plus_block("minecraft:dirt", false), None);
}

#[test]
fn ore_relief_is_deterministic_and_well_formed() {
    let a = build_3d_ore_relief(&vein_mask(), 0x3F);
    let b = build_3d_ore_relief(&vein_mask(), 0x3F);
    assert_eq!(a, b);
    assert_eq!(mesh_fingerprint(&a), mesh_fingerprint(&b));
    assert_finite(&a);
    for q in &a.quads {
        for uv in &q.uv {
            assert!((-1e-6..=1.0 + 1e-6).contains(&uv[0]));
            assert!((-1e-6..=1.0 + 1e-6).contains(&uv[1]));
        }
    }
    // Golden pins for the standard vein fixture.
    assert_eq!(mesh_fingerprint(&a), 0xa1da_e31c_0097_46dd);
    let up_only = build_3d_ore_relief(&vein_mask(), Direction::Up.bit());
    assert_eq!(mesh_fingerprint(&up_only), 0x0c47_e059_704c_a795);
}

#[test]
fn ore_ids_classify_for_relief() {
    for id in [
        "minecraft:iron_ore",
        "minecraft:diamond_ore",
        "minecraft:deepslate_gold_ore",
        "minecraft:nether_quartz_ore",
        "minecraft:ancient_debris",
        "mymod:unobtainium_ore",
    ] {
        assert_eq!(
            classify_vanilla_plus_block(id, false),
            Some(VanillaPlusCategory::Ores3dRelief),
            "{id}"
        );
    }
}

#[test]
fn ore_category_toggle_gates_decision() {
    let mut cfg = VanillaPlus3dConfig {
        enabled: true,
        ..VanillaPlus3dConfig::default()
    };
    let hint = BlockModelHint::default();
    assert_eq!(
        vanilla_plus_decision(&cfg, "minecraft:iron_ore", &hint, 4.0),
        MeshUpgradeDecision::Upgrade3d(VanillaPlusCategory::Ores3dRelief)
    );
    cfg.ores_3d_relief = false;
    assert_eq!(
        vanilla_plus_decision(&cfg, "minecraft:iron_ore", &hint, 4.0),
        MeshUpgradeDecision::Use2dVanilla
    );
}

// ---------------------------------------------------------------------------
// Resource-pack deference wins over built-in crops and ore relief
// ---------------------------------------------------------------------------

#[test]
fn resource_pack_deference_beats_3d_crops_and_ore_relief() {
    let cfg = VanillaPlus3dConfig {
        enabled: true,
        ..VanillaPlus3dConfig::default()
    };
    let hint = BlockModelHint {
        has_custom_3d_resourcepack_model: true,
        is_cross_model: true,
    };
    for id in ["minecraft:wheat", "minecraft:iron_ore"] {
        assert_eq!(
            vanilla_plus_decision(&cfg, id, &hint, 4.0),
            MeshUpgradeDecision::DeferToResourcePack,
            "{id} must defer to the resource pack"
        );
    }
    // And end-to-end through the pipeline: no mesh is emitted.
    let reg = NatureModRegistry::new();
    let pcfg = three_d_cfg();
    let vein = vein_mask();
    for id in ["minecraft:wheat", "minecraft:iron_ore"] {
        let inp = BlockVisualInput {
            block_id: id,
            distance_sq: 16.0,
            has_custom_3d_resourcepack_model: true,
            crop_3d: Some(spec(Crop3dArchetype::SheafStalk, 7)),
            ore_vein_mask: Some(&vein),
            ..Default::default()
        };
        let r = evaluate_block_visual_pipeline(&inp, &pcfg, &reg);
        assert_eq!(r.upgrade_decision, MeshUpgradeDecision::DeferToResourcePack);
        assert!(r.upgrade_mesh.is_none(), "{id} emitted a mesh over a pack");
    }
}

// ---------------------------------------------------------------------------
// Live mesher dispatch
// ---------------------------------------------------------------------------

#[test]
fn mesher_dispatch_runs_3d_crop_mesh() {
    let reg = NatureModRegistry::new();
    let cfg = three_d_cfg();
    let crop = spec(Crop3dArchetype::SheafStalk, 6);
    let inp = BlockVisualInput {
        block_id: "minecraft:wheat",
        distance_sq: 16.0,
        crop_3d: Some(crop),
        ..Default::default()
    };
    let r = evaluate_block_visual_pipeline(&inp, &cfg, &reg);
    assert_eq!(
        r.upgrade_decision,
        MeshUpgradeDecision::Upgrade3d(VanillaPlusCategory::CropsAndFlora)
    );
    let mesh = r.upgrade_mesh.expect("crop upgrade mesh");
    // The dispatch path produced exactly the procedural crop mesh.
    assert_eq!(
        mesh_fingerprint(&mesh),
        mesh_fingerprint(&build_3d_crop_mesh(&crop))
    );

    // Without a spec the legacy cross fallback still runs.
    let inp = BlockVisualInput {
        block_id: "minecraft:wheat",
        distance_sq: 16.0,
        ..Default::default()
    };
    let r = evaluate_block_visual_pipeline(&inp, &cfg, &reg);
    let fallback = r.upgrade_mesh.expect("cross fallback mesh");
    assert!(!fallback.quads.is_empty());
    assert_ne!(mesh_fingerprint(&fallback), mesh_fingerprint(&mesh));
}

#[test]
fn mesher_dispatch_runs_ore_relief() {
    let reg = NatureModRegistry::new();
    let cfg = three_d_cfg();
    let vein = vein_mask();
    let inp = BlockVisualInput {
        block_id: "minecraft:iron_ore",
        distance_sq: 16.0,
        ore_vein_mask: Some(&vein),
        exposed_faces: Direction::Up.bit() | Direction::North.bit(),
        ..Default::default()
    };
    let r = evaluate_block_visual_pipeline(&inp, &cfg, &reg);
    assert_eq!(
        r.upgrade_decision,
        MeshUpgradeDecision::Upgrade3d(VanillaPlusCategory::Ores3dRelief)
    );
    let mesh = r.upgrade_mesh.expect("ore relief mesh");
    let want = build_3d_ore_relief(&vein, Direction::Up.bit() | Direction::North.bit());
    assert_eq!(mesh, want);
    assert!(!mesh.quads.is_empty());

    // No vein mask -> the relief fails closed: nothing is emitted and the
    // block reports fully vanilla rather than an empty override.
    let inp = BlockVisualInput {
        block_id: "minecraft:iron_ore",
        distance_sq: 16.0,
        ..Default::default()
    };
    let r = evaluate_block_visual_pipeline(&inp, &cfg, &reg);
    assert_eq!(r.upgrade_decision, MeshUpgradeDecision::Use2dVanilla);
    assert!(r.upgrade_mesh.is_none());
    assert!(r.is_vanilla());
}
