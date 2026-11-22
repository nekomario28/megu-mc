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
            0x7159_fc99_57f4_dfab,
            0x0df8_06a8_6308_2b90,
            0xd2b8_e3ce_83df_c354,
            0x57e1_2326_d3c8_76ef,
            0xadb2_2247_a802_6927,
            0xd91a_afee_95f9_288c,
            0x8d49_cbd3_c89f_2c11,
            0x387c_d69e_0687_e4d6,
        ],
        // BulbLeaf stages 0..=7
        [
            0x3636_434d_9446_c34e,
            0xcf46_ffe4_5f52_7df6,
            0xd433_6bdb_636a_7b92,
            0x12a0_2a50_c960_8465,
            0x297c_711e_b63a_7b8b,
            0x0b99_1418_3192_6262,
            0xa733_a25e_cbac_cfee,
            0xb920_4826_599f_d568,
        ],
        // VineFruit stages 0..=7
        [
            0x2450_c191_f52f_4421,
            0x0f41_a2c6_b611_1ef9,
            0xfdbd_f237_451c_3875,
            0xfd73_5689_cbea_ae5d,
            0xcba0_7075_50a3_228d,
            0xe81d_d4bb_b1dc_f3d9,
            0x787f_3da8_7e7f_33b5,
            0xb223_94ef_eb7a_2b89,
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
    assert_eq!(mesh_fingerprint(&a), 0xf001_31dc_bb07_8b1d);
    let up_only = build_3d_ore_relief(&vein_mask(), Direction::Up.bit());
    assert_eq!(mesh_fingerprint(&up_only), 0x1143_f836_42d6_a895);
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
