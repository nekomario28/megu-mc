// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later

use mc_spine_client_mesh::vanilla_plus_3d::*;
use mc_spine_client_mesh::Direction;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn assert_unit_normals(mesh: &ExtrudedMesh) {
    for (i, q) in mesh.quads.iter().enumerate() {
        let len = (q.normal[0] * q.normal[0]
            + q.normal[1] * q.normal[1]
            + q.normal[2] * q.normal[2])
            .sqrt();
        assert!(
            (len - 1.0).abs() < 1e-4,
            "quad {i} normal {:?} is not unit (len {len})",
            q.normal
        );
    }
}

fn assert_uvs_in_range(mesh: &ExtrudedMesh) {
    for (i, q) in mesh.quads.iter().enumerate() {
        for uv in &q.uv {
            assert!(
                (-1e-6..=1.0 + 1e-6).contains(&uv[0]) && (-1e-6..=1.0 + 1e-6).contains(&uv[1]),
                "quad {i} uv {uv:?} outside 0..=1"
            );
        }
    }
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

fn assert_within_block(mesh: &ExtrudedMesh, slack: f32) {
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

fn solid_mask() -> [u8; 256] {
    [255u8; 256]
}

fn enabled_config() -> VanillaPlus3dConfig {
    VanillaPlus3dConfig {
        enabled: true,
        ..VanillaPlus3dConfig::default()
    }
}

// ---------------------------------------------------------------------------
// Opt-in config & decision pipeline
// ---------------------------------------------------------------------------

#[test]
fn default_config_is_disabled_and_yields_vanilla() {
    let config = VanillaPlus3dConfig::default();
    assert!(!config.enabled);
    for id in ["wheat", "rail", "amethyst_cluster", "sun"] {
        assert_eq!(
            vanilla_plus_decision(&config, id, &BlockModelHint::default(), 4.0),
            MeshUpgradeDecision::Use2dVanilla,
            "disabled feature must never alter {id}"
        );
    }
}

#[test]
fn custom_resourcepack_model_defers() {
    let config = enabled_config();
    let hint = BlockModelHint {
        has_custom_3d_resourcepack_model: true,
        is_cross_model: true,
    };
    assert_eq!(
        vanilla_plus_decision(&config, "minecraft:wheat", &hint, 4.0),
        MeshUpgradeDecision::DeferToResourcePack
    );
    // Opting out of deference lets the upgrade proceed.
    let config = VanillaPlus3dConfig {
        respect_custom_resourcepack_models: false,
        ..enabled_config()
    };
    assert_eq!(
        vanilla_plus_decision(&config, "minecraft:wheat", &hint, 4.0),
        MeshUpgradeDecision::Upgrade3d(VanillaPlusCategory::CropsAndFlora)
    );
}

#[test]
fn distance_lod_switches_to_flat_fallback() {
    let config = enabled_config();
    let hint = BlockModelHint::default();
    let cutoff = config.effective_lod_cutoff();
    assert_eq!(cutoff, 24.0);
    assert_eq!(
        vanilla_plus_decision(&config, "wheat", &hint, cutoff),
        MeshUpgradeDecision::Upgrade3d(VanillaPlusCategory::CropsAndFlora)
    );
    assert_eq!(
        vanilla_plus_decision(&config, "wheat", &hint, cutoff + 0.01),
        MeshUpgradeDecision::Use2dLodFallback
    );
}

#[test]
fn lod_cutoff_is_clamped() {
    let low = VanillaPlus3dConfig {
        lod_cutoff_blocks: 1.0,
        ..enabled_config()
    };
    assert_eq!(low.effective_lod_cutoff(), MIN_LOD_CUTOFF_BLOCKS);
    let high = VanillaPlus3dConfig {
        lod_cutoff_blocks: 500.0,
        ..enabled_config()
    };
    assert_eq!(high.effective_lod_cutoff(), MAX_LOD_CUTOFF_BLOCKS);
}

#[test]
fn per_category_toggles_gate_decisions() {
    let config = VanillaPlus3dConfig {
        crops_and_flora: false,
        ..enabled_config()
    };
    let hint = BlockModelHint::default();
    assert_eq!(
        vanilla_plus_decision(&config, "wheat", &hint, 4.0),
        MeshUpgradeDecision::Use2dVanilla
    );
    assert_eq!(
        vanilla_plus_decision(&config, "rail", &hint, 4.0),
        MeshUpgradeDecision::Upgrade3d(VanillaPlusCategory::RailsAndLadders)
    );
}

#[test]
fn classifier_covers_vanilla_and_modded() {
    use VanillaPlusCategory::*;
    let cases: &[(&str, VanillaPlusCategory)] = &[
        ("sun", Celestial3d),
        ("moon", Celestial3d),
        ("minecraft:wheat", CropsAndFlora),
        ("carrots", CropsAndFlora),
        ("potatoes", CropsAndFlora),
        ("beetroots", CropsAndFlora),
        ("nether_wart", CropsAndFlora),
        ("dandelion", CropsAndFlora),
        ("oak_sapling", CropsAndFlora),
        ("tall_grass", CropsAndFlora),
        ("amethyst_cluster", CrystalsAndClusters),
        ("large_amethyst_bud", CrystalsAndClusters),
        ("pointed_dripstone", CrystalsAndClusters),
        ("sea_pickle", CrystalsAndClusters),
        ("rail", RailsAndLadders),
        ("powered_rail", RailsAndLadders),
        ("detector_rail", RailsAndLadders),
        ("activator_rail", RailsAndLadders),
        ("ladder", RailsAndLadders),
        ("chain", FixturesAndChains),
        ("iron_bars", FixturesAndChains),
        ("torch", FixturesAndChains),
        ("soul_torch", FixturesAndChains),
        ("redstone_torch", FixturesAndChains),
        ("lantern", FixturesAndChains),
        ("soul_lantern", FixturesAndChains),
        ("mymod:magic_crystal", CrystalsAndClusters),
    ];
    for &(id, want) in cases {
        assert_eq!(
            classify_vanilla_plus_block(id, false),
            Some(want),
            "misclassified {id}"
        );
    }
    // Unknown non-cross block: no upgrade.
    assert_eq!(classify_vanilla_plus_block("stone", false), None);
    // Unknown modded cross sprite: generic crop/flora fallback.
    assert_eq!(
        classify_vanilla_plus_block("mymod:starflower", true),
        Some(CropsAndFlora)
    );
}

// ---------------------------------------------------------------------------
// Generic texel-grid sprite extruder
// ---------------------------------------------------------------------------

#[test]
fn extrude_empty_mask_produces_no_quads() {
    let mesh = extrude_alpha_mask_16x16(&[0u8; 256], TEXEL);
    assert_eq!(mesh.quad_count(), 0);
}

#[test]
fn extrude_subthreshold_mask_produces_no_quads() {
    let mesh = extrude_alpha_mask_16x16(&[ALPHA_CUTOFF - 1; 256], TEXEL);
    assert_eq!(mesh.quad_count(), 0);
}

#[test]
fn extrude_single_pixel_is_exactly_six_quads() {
    let mut mask = [0u8; 256];
    mask[5 * 16 + 9] = 255;
    let mesh = extrude_alpha_mask_16x16(&mask, TEXEL);
    // Front + back + four boundary rims; zero hidden interior faces.
    assert_eq!(mesh.quad_count(), 6);
    assert_unit_normals(&mesh);
    assert_uvs_in_range(&mesh);
    let mut normals: Vec<[i32; 3]> = mesh
        .quads
        .iter()
        .map(|q| {
            [
                q.normal[0].round() as i32,
                q.normal[1].round() as i32,
                q.normal[2].round() as i32,
            ]
        })
        .collect();
    normals.sort();
    assert_eq!(
        normals,
        vec![
            [-1, 0, 0],
            [0, -1, 0],
            [0, 0, -1],
            [0, 0, 1],
            [0, 1, 0],
            [1, 0, 0],
        ]
    );
}

#[test]
fn extrude_solid_mask_merges_perimeter_rims() {
    let mesh = extrude_alpha_mask_16x16(&solid_mask(), TEXEL);
    // One merged front, one merged back, four merged perimeter rims.
    assert_eq!(mesh.quad_count(), 6);
    // Strictly cheaper than naive per-pixel cubes.
    assert!(mesh.quad_count() < 256 * 6);
    assert_unit_normals(&mesh);
    assert_uvs_in_range(&mesh);
    assert_within_block(&mesh, 1e-6);
}

#[test]
fn extrude_modded_crop_mask_is_well_formed() {
    // A "modded" flower silhouette: stem column plus a head blob — nothing
    // hand-authored, just an arbitrary alpha pattern.
    let mut mask = [0u8; 256];
    for y in 8..16 {
        mask[y * 16 + 7] = 255;
        mask[y * 16 + 8] = 255;
    }
    for y in 2..8 {
        for x in 4..12 {
            mask[y * 16 + x] = 255;
        }
    }
    let mesh = extrude_alpha_mask_16x16(&mask, TEXEL);
    assert!(mesh.quad_count() > 6);
    assert!(mesh.quad_count() < 256 * 6);
    assert_unit_normals(&mesh);
    assert_uvs_in_range(&mesh);
    assert_finite(&mesh);
}

// ---------------------------------------------------------------------------
// Cross meshes
// ---------------------------------------------------------------------------

#[test]
fn cross_meshes_stay_in_bounds_and_have_unit_normals() {
    let mask = solid_mask();
    for layout in [CrossLayout::Diagonal, CrossLayout::HashGrid] {
        for block in [[0, 0, 0], [17, 64, -9], [-3, 128, 42]] {
            let mesh = build_3d_cross_mesh(&mask, layout, block);
            assert!(!mesh.quads.is_empty(), "empty cross mesh for {layout:?}");
            assert_unit_normals(&mesh);
            assert_uvs_in_range(&mesh);
            assert_within_block(&mesh, 1e-5);
        }
    }
}

#[test]
fn cross_jitter_is_deterministic_and_subtle() {
    let mask = solid_mask();
    let a = build_3d_cross_mesh(&mask, CrossLayout::Diagonal, [5, 70, 5]);
    let b = build_3d_cross_mesh(&mask, CrossLayout::Diagonal, [5, 70, 5]);
    let c = build_3d_cross_mesh(&mask, CrossLayout::Diagonal, [6, 70, 5]);
    assert_eq!(a, b, "same block must produce identical geometry");
    assert_ne!(a, c, "different blocks should get different micro-offsets");
    assert_within_block(&a, 1e-5);
    assert_within_block(&c, 1e-5);
}

// ---------------------------------------------------------------------------
// Crystals
// ---------------------------------------------------------------------------

#[test]
fn crystals_all_orientations_and_stages() {
    for dir in Direction::ALL {
        for stage in [
            CrystalStage::SmallBud,
            CrystalStage::MediumBud,
            CrystalStage::LargeBud,
            CrystalStage::Cluster,
        ] {
            let mesh = build_3d_crystal_mesh(stage, dir);
            assert!(
                !mesh.quads.is_empty(),
                "empty crystal for {stage:?} facing {dir:?}"
            );
            assert_finite(&mesh);
            assert_unit_normals(&mesh);
            assert_uvs_in_range(&mesh);
            assert_within_block(&mesh, 1e-4);
        }
    }
}

#[test]
fn crystal_tip_points_along_facing() {
    for dir in Direction::ALL {
        let mesh = build_3d_crystal_mesh(CrystalStage::LargeBud, dir);
        let d = match dir {
            Direction::Down => [0.0, -1.0, 0.0],
            Direction::Up => [0.0, 1.0, 0.0],
            Direction::North => [0.0, 0.0, -1.0],
            Direction::South => [0.0, 0.0, 1.0],
            Direction::West => [-1.0, 0.0, 0.0],
            Direction::East => [1.0, 0.0, 0.0],
        };
        // The farthest vertex measured from the attachment face (the plane
        // 0.5 blocks behind the centre along `d`) must extend along `d`.
        let proj_of = |p: [f32; 3]| {
            (p[0] - 0.5) * d[0] + (p[1] - 0.5) * d[1] + (p[2] - 0.5) * d[2] + 0.5
        };
        let farthest = mesh
            .quads
            .iter()
            .flat_map(|q| q.positions)
            .max_by(|a, b| proj_of(*a).partial_cmp(&proj_of(*b)).unwrap())
            .unwrap();
        assert!(
            proj_of(farthest) > 0.4,
            "crystal does not extend along {dir:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// Rails, ladders, fixtures
// ---------------------------------------------------------------------------

#[test]
fn rails_all_shapes_and_power_states() {
    let shapes = [
        RailShape::NorthSouth,
        RailShape::EastWest,
        RailShape::AscendingNorth,
        RailShape::AscendingSouth,
        RailShape::AscendingEast,
        RailShape::AscendingWest,
        RailShape::CurveSouthEast,
        RailShape::CurveSouthWest,
        RailShape::CurveNorthWest,
        RailShape::CurveNorthEast,
    ];
    for shape in shapes {
        for powered in [false, true] {
            let mesh = build_3d_rail_mesh(shape, powered);
            assert!(
                mesh.quad_count() >= 12,
                "rail {shape:?} produced too little geometry"
            );
            assert_finite(&mesh);
            assert_unit_normals(&mesh);
            assert_uvs_in_range(&mesh);
            assert_within_block(&mesh, 1e-4);
            if powered {
                assert!(mesh.quads.iter().any(|q| q.tint_index == 1));
            } else {
                assert!(mesh.quads.iter().all(|q| q.tint_index <= 0));
            }
        }
    }
}

#[test]
fn ladders_all_wall_facings() {
    for dir in [
        Direction::North,
        Direction::South,
        Direction::East,
        Direction::West,
    ] {
        let mesh = build_3d_ladder_mesh(dir);
        assert!(mesh.quad_count() >= 12, "ladder {dir:?} too small");
        assert_finite(&mesh);
        assert_unit_normals(&mesh);
        assert_uvs_in_range(&mesh);
        assert_within_block(&mesh, 1e-4);
    }
}

#[test]
fn fixtures_produce_geometry() {
    for kind in [
        FixtureKind::Chain,
        FixtureKind::IronBars,
        FixtureKind::Torch,
        FixtureKind::Lantern,
    ] {
        let mesh = build_3d_fixture_mesh(kind);
        assert!(!mesh.quads.is_empty(), "empty fixture {kind:?}");
        assert_finite(&mesh);
        assert_unit_normals(&mesh);
        assert_uvs_in_range(&mesh);
        assert_within_block(&mesh, 1e-4);
    }
}

// ---------------------------------------------------------------------------
// Celestial bodies
// ---------------------------------------------------------------------------

#[test]
fn sun_mesh_is_bounded_and_deterministic() {
    let a = build_3d_sun_mesh();
    let b = build_3d_sun_mesh();
    assert!(!a.quads.is_empty());
    assert_eq!(a, b);
    assert_finite(&a);
    assert_unit_normals(&a);
    assert_within_block(&a, 1e-4);
}

#[test]
fn moon_all_eight_phases() {
    for phase in 0..8u8 {
        let mesh = build_3d_moon_mesh(phase);
        assert!(!mesh.quads.is_empty(), "empty moon for phase {phase}");
        assert_finite(&mesh);
        assert_unit_normals(&mesh);
        assert_uvs_in_range(&mesh);
        assert_eq!(mesh, build_3d_moon_mesh(phase), "phase {phase} not deterministic");
    }
}

#[test]
fn moon_phase_lit_ratios_are_distinct() {
    let full = moon_lit_fraction(0);
    let waning_gibbous = moon_lit_fraction(1);
    let third_quarter = moon_lit_fraction(2);
    let waning_crescent = moon_lit_fraction(3);
    let new = moon_lit_fraction(4);
    let waxing_crescent = moon_lit_fraction(5);
    let first_quarter = moon_lit_fraction(6);
    let waxing_gibbous = moon_lit_fraction(7);

    assert_eq!(full, 1.0);
    assert_eq!(new, 0.0);
    assert!(full > waning_gibbous);
    assert!(waning_gibbous > third_quarter);
    assert!(third_quarter > waning_crescent);
    assert!(waning_crescent > new);
    assert!(waxing_crescent > new);
    assert!(first_quarter > waxing_crescent);
    assert!(waxing_gibbous > first_quarter);
    assert!(full > waxing_gibbous);
    // Waning and waxing halves mirror each other.
    assert_eq!(waning_crescent, waxing_crescent);
    assert_eq!(third_quarter, first_quarter);
    assert_eq!(waning_gibbous, waxing_gibbous);
}

// ---------------------------------------------------------------------------
// End-to-end
// ---------------------------------------------------------------------------

#[test]
fn decision_to_mesh_pipeline() {
    let config = enabled_config();
    let hint = BlockModelHint {
        has_custom_3d_resourcepack_model: false,
        is_cross_model: true,
    };
    match vanilla_plus_decision(&config, "mymod:starflower", &hint, 12.0) {
        MeshUpgradeDecision::Upgrade3d(VanillaPlusCategory::CropsAndFlora) => {
            let mesh = build_3d_cross_mesh(&solid_mask(), CrossLayout::Diagonal, [1, 64, 1]);
            assert!(mesh.quad_count() > 0);
        }
        other => panic!("expected crop upgrade, got {other:?}"),
    }
}
