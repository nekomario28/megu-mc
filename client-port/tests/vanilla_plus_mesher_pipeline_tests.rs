// SPDX-License-Identifier: LGPL-3.0-or-later

use megu_client_mesh::mesher::{
    evaluate_block_visual_pipeline, BlockVisualEnhancementResult, BlockVisualInput,
    VanillaPlusPipelineConfig,
};
use megu_client_mesh::vanilla_plus_3d::{MeshUpgradeDecision, VanillaPlusCategory};
use megu_client_mesh::vanilla_plus_nature::{NatureModRegistry, WindSwayCategory};
use megu_client_mesh::vanilla_plus_visuals::{
    CtmNeighbors, CtmTile, SideFaceStyle, RELIEF_TINT_EMISSIVE,
};
use megu_client_mesh::Direction;

fn input(block_id: &str) -> BlockVisualInput<'_> {
    BlockVisualInput {
        block_id,
        ..Default::default()
    }
}

fn enabled_cfg() -> VanillaPlusPipelineConfig {
    let mut cfg = VanillaPlusPipelineConfig::default();
    cfg.three_d.enabled = true;
    cfg.classic_ores.enabled = true;
    cfg.visuals.connected_textures = true;
    cfg.visuals.better_grass = true;
    cfg.visuals.better_snow = true;
    cfg.visuals.block_relief_3d = true;
    cfg.nature.enabled = true;
    cfg
}

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

#[test]
fn default_config_is_fully_vanilla() {
    let cfg = VanillaPlusPipelineConfig::default();
    assert!(cfg.is_all_disabled());
    assert!(!cfg.three_d.enabled);
    assert!(!cfg.classic_ores.enabled);
    assert!(!cfg.visuals.connected_textures);
    assert!(!cfg.visuals.better_grass);
    assert!(!cfg.visuals.better_snow);
    assert!(!cfg.visuals.block_relief_3d);
    assert!(!cfg.visuals.skin_3d_layers);
    assert!(!cfg.nature.enabled);
}

#[test]
fn is_all_disabled_tracks_every_pack() {
    let mut cfg = VanillaPlusPipelineConfig::default();
    cfg.three_d.enabled = true;
    assert!(!cfg.is_all_disabled());

    let mut cfg = VanillaPlusPipelineConfig::default();
    cfg.classic_ores.enabled = true;
    assert!(!cfg.is_all_disabled());

    let mut cfg = VanillaPlusPipelineConfig::default();
    cfg.nature.enabled = true;
    assert!(!cfg.is_all_disabled());

    // `visuals` has no master switch: any single toggle enables the pack.
    let mut cfg = VanillaPlusPipelineConfig::default();
    cfg.visuals.connected_textures = true;
    assert!(!cfg.is_all_disabled());
    let mut cfg = VanillaPlusPipelineConfig::default();
    cfg.visuals.better_grass = true;
    assert!(!cfg.is_all_disabled());
    let mut cfg = VanillaPlusPipelineConfig::default();
    cfg.visuals.better_snow = true;
    assert!(!cfg.is_all_disabled());
    let mut cfg = VanillaPlusPipelineConfig::default();
    cfg.visuals.block_relief_3d = true;
    assert!(!cfg.is_all_disabled());
    let mut cfg = VanillaPlusPipelineConfig::default();
    cfg.visuals.skin_3d_layers = true;
    assert!(!cfg.is_all_disabled());
}

// ---------------------------------------------------------------------------
// Zero-override guarantees
// ---------------------------------------------------------------------------

#[test]
fn default_pipeline_produces_zero_overrides() {
    let cfg = VanillaPlusPipelineConfig::default();
    let reg = NatureModRegistry::new();
    for id in [
        "minecraft:wheat",
        "minecraft:iron_ore",
        "minecraft:glass",
        "minecraft:grass_block",
        "minecraft:furnace",
        "biomesoplenty:maple_leaves",
        "create:experience_ore",
    ] {
        let inp = BlockVisualInput {
            block_id: id,
            tags: &["c:ores", "minecraft:leaves", "c:crops"],
            ctm_neighbors: CtmNeighbors::ALL,
            distance_sq: 100.0,
            lower_diagonal_same_type: true,
            snowy: true,
            adjacent_snow: true,
            is_cross_model: true,
            furnace_lit: true,
            ..Default::default()
        };
        let r = evaluate_block_visual_pipeline(&inp, &cfg, &reg);
        assert_eq!(r, BlockVisualEnhancementResult::VANILLA, "{id}");
        assert!(r.is_vanilla(), "{id}");
        assert!(r.classic_ore.is_none());
        assert!(r.ctm_tile.is_none());
        assert_eq!(r.side_face, SideFaceStyle::VanillaSide);
        assert!(!r.better_snow.emit_snow_layer);
        assert!(r.relief_mesh.is_none());
        assert_eq!(r.upgrade_decision, MeshUpgradeDecision::Use2dVanilla);
        assert!(r.upgrade_mesh.is_none());
        assert!(r.leaf_profile.is_none());
        assert!(r.wind_sway.is_none());
    }
}

#[test]
fn external_resourcepack_override_bypasses_everything() {
    let cfg = enabled_cfg();
    let mut reg = NatureModRegistry::new();
    reg.register_mod_leaf("biomesoplenty:maple_leaves", [0.8, 0.2, 0.1], true);
    for id in [
        "minecraft:wheat",
        "minecraft:iron_ore",
        "minecraft:glass",
        "minecraft:grass_block",
        "minecraft:furnace",
        "biomesoplenty:maple_leaves",
        "create:experience_ore",
    ] {
        let inp = BlockVisualInput {
            block_id: id,
            tags: &["c:ores", "minecraft:leaves"],
            ctm_neighbors: CtmNeighbors::ALL,
            distance_sq: 16.0,
            has_external_custom_override: true,
            lower_diagonal_same_type: true,
            snowy: true,
            adjacent_snow: true,
            on_solid_ground: true,
            snowy_cardinal_neighbors: 4,
            furnace_lit: true,
            ..Default::default()
        };
        let r = evaluate_block_visual_pipeline(&inp, &cfg, &reg);
        assert_eq!(r, BlockVisualEnhancementResult::VANILLA, "{id}");
        assert!(r.is_vanilla(), "{id}");
    }
}

// ---------------------------------------------------------------------------
// Vanilla+ 3D routing
// ---------------------------------------------------------------------------

#[test]
fn wheat_routes_to_3d_crop_mesh_within_lod() {
    let cfg = enabled_cfg();
    let reg = NatureModRegistry::new();
    let inp = BlockVisualInput {
        block_id: "minecraft:wheat",
        tags: &["minecraft:crops"],
        distance_sq: 100.0, // 10 blocks, inside the 24-block LOD cutoff.
        block_pos: [3, 64, -7],
        ..Default::default()
    };
    let r = evaluate_block_visual_pipeline(&inp, &cfg, &reg);
    assert_eq!(
        r.upgrade_decision,
        MeshUpgradeDecision::Upgrade3d(VanillaPlusCategory::CropsAndFlora)
    );
    let mesh = r.upgrade_mesh.expect("wheat 3D upgrade mesh");
    assert!(!mesh.quads.is_empty());
    for q in &mesh.quads {
        for p in &q.positions {
            for c in p {
                assert!(c.is_finite() && (-1e-4..=1.0 + 1e-4).contains(c));
            }
        }
    }
    // Wheat is also a swaying crop under the nature pack.
    assert_eq!(r.wind_sway, Some(WindSwayCategory::Crop));
}

#[test]
fn wheat_beyond_lod_falls_back_to_2d() {
    let cfg = enabled_cfg();
    let reg = NatureModRegistry::new();
    let inp = BlockVisualInput {
        block_id: "minecraft:wheat",
        distance_sq: 40.0 * 40.0, // 40 blocks > 24-block LOD cutoff.
        ..Default::default()
    };
    let r = evaluate_block_visual_pipeline(&inp, &cfg, &reg);
    assert_eq!(r.upgrade_decision, MeshUpgradeDecision::Use2dLodFallback);
    assert!(r.upgrade_mesh.is_none());
}

#[test]
fn custom_resourcepack_model_defers_3d_upgrade() {
    let cfg = enabled_cfg();
    let reg = NatureModRegistry::new();
    let inp = BlockVisualInput {
        block_id: "minecraft:wheat",
        distance_sq: 4.0,
        has_custom_3d_resourcepack_model: true,
        ..Default::default()
    };
    let r = evaluate_block_visual_pipeline(&inp, &cfg, &reg);
    assert_eq!(r.upgrade_decision, MeshUpgradeDecision::DeferToResourcePack);
    assert!(r.upgrade_mesh.is_none());
}

#[test]
fn torch_and_ladder_route_to_3d_fixtures() {
    let cfg = enabled_cfg();
    let reg = NatureModRegistry::new();
    let torch = evaluate_block_visual_pipeline(
        &BlockVisualInput {
            block_id: "minecraft:torch",
            distance_sq: 9.0,
            ..Default::default()
        },
        &cfg,
        &reg,
    );
    assert_eq!(
        torch.upgrade_decision,
        MeshUpgradeDecision::Upgrade3d(VanillaPlusCategory::FixturesAndChains)
    );
    assert!(!torch.upgrade_mesh.unwrap().quads.is_empty());

    let ladder = evaluate_block_visual_pipeline(
        &BlockVisualInput {
            block_id: "minecraft:ladder",
            distance_sq: 9.0,
            facing: Direction::East,
            ..Default::default()
        },
        &cfg,
        &reg,
    );
    assert_eq!(
        ladder.upgrade_decision,
        MeshUpgradeDecision::Upgrade3d(VanillaPlusCategory::RailsAndLadders)
    );
    assert!(!ladder.upgrade_mesh.unwrap().quads.is_empty());
}

// ---------------------------------------------------------------------------
// Classic Ores routing
// ---------------------------------------------------------------------------

#[test]
fn iron_ore_gets_classic_sprite_plan() {
    let cfg = enabled_cfg();
    let reg = NatureModRegistry::new();
    let r = evaluate_block_visual_pipeline(&input("minecraft:iron_ore"), &cfg, &reg);
    let plan = r.classic_ore.expect("iron ore synthesis plan");
    assert_eq!(plan.host.as_str(), "stone");
    // Curated clean-room iron palette: warm beige mineral body.
    assert_eq!(plan.palette.primary, [206, 152, 112]);
}

#[test]
fn deepslate_and_nether_ores_get_matching_hosts() {
    let cfg = enabled_cfg();
    let reg = NatureModRegistry::new();
    let deep = evaluate_block_visual_pipeline(
        &input("minecraft:deepslate_diamond_ore"),
        &cfg,
        &reg,
    );
    assert_eq!(deep.classic_ore.unwrap().host.as_str(), "deepslate");
    let nether =
        evaluate_block_visual_pipeline(&input("minecraft:nether_quartz_ore"), &cfg, &reg);
    assert_eq!(nether.classic_ore.unwrap().host.as_str(), "netherrack");
}

#[test]
fn special_and_mod_ores_stay_preserved() {
    let cfg = enabled_cfg();
    let reg = NatureModRegistry::new();
    // Special mod ore: no pixel data -> pixel heuristic fails closed.
    let r = evaluate_block_visual_pipeline(
        &BlockVisualInput {
            block_id: "create:experience_ore",
            tags: &["c:ores"],
            ..Default::default()
        },
        &cfg,
        &reg,
    );
    assert!(r.classic_ore.is_none());
    assert!(r.is_vanilla());

    // Unique vanilla blocks that merely look ore-adjacent are never touched.
    for id in [
        "minecraft:ancient_debris",
        "minecraft:gilded_blackstone",
        "minecraft:budding_amethyst",
        "minecraft:amethyst_cluster",
    ] {
        let r = evaluate_block_visual_pipeline(&input(id), &cfg, &reg);
        assert!(r.classic_ore.is_none(), "{id}");
    }

    // A mod ore outside scope (VanillaOnly) is excluded even though it is an
    // ore, and a plain non-ore block is never a candidate.
    let mut cfg = enabled_cfg();
    cfg.classic_ores.scope = megu_client_mesh::classic_ores::ClassicOresScope::VanillaOnly;
    let r = evaluate_block_visual_pipeline(&input("create:zinc_ore"), &cfg, &reg);
    assert!(r.classic_ore.is_none());
    let r = evaluate_block_visual_pipeline(&input("minecraft:stone"), &cfg, &reg);
    assert!(r.classic_ore.is_none());
}

#[test]
fn ore_guards_still_apply_through_pipeline() {
    let cfg = enabled_cfg();
    let reg = NatureModRegistry::new();
    // Animated strip -> preserved.
    let r = evaluate_block_visual_pipeline(
        &BlockVisualInput {
            block_id: "minecraft:iron_ore",
            ore_has_mcmeta_animation: true,
            ..Default::default()
        },
        &cfg,
        &reg,
    );
    assert!(r.classic_ore.is_none());
    // HD texture -> preserved.
    let r = evaluate_block_visual_pipeline(
        &BlockVisualInput {
            block_id: "minecraft:iron_ore",
            ore_texture_width: 32,
            ore_texture_height: 32,
            ..Default::default()
        },
        &cfg,
        &reg,
    );
    assert!(r.classic_ore.is_none());
    // Custom model -> preserved.
    let r = evaluate_block_visual_pipeline(
        &BlockVisualInput {
            block_id: "minecraft:iron_ore",
            has_custom_model: true,
            ..Default::default()
        },
        &cfg,
        &reg,
    );
    assert!(r.classic_ore.is_none());
}

// ---------------------------------------------------------------------------
// Vanilla+ Visuals routing
// ---------------------------------------------------------------------------

#[test]
fn connected_glass_resolves_ctm_tile() {
    let cfg = enabled_cfg();
    let reg = NatureModRegistry::new();
    // Fully surrounded glass: innermost Full47 tile, all borders hidden.
    let r = evaluate_block_visual_pipeline(
        &BlockVisualInput {
            block_id: "minecraft:glass",
            ctm_neighbors: CtmNeighbors::ALL,
            ..Default::default()
        },
        &cfg,
        &reg,
    );
    let Some(CtmTile::Full47 { index, suppress }) = r.ctm_tile else {
        panic!("expected Full47 tile, got {:?}", r.ctm_tile);
    };
    assert_eq!(index, 46);
    assert!(suppress.hide_top && suppress.hide_right && suppress.hide_bottom && suppress.hide_left);

    // Isolated glass: tile 0.
    let r = evaluate_block_visual_pipeline(&input("minecraft:glass"), &cfg, &reg);
    let Some(CtmTile::Full47 { index, .. }) = r.ctm_tile else {
        panic!("expected Full47 tile, got {:?}", r.ctm_tile);
    };
    assert_eq!(index, 0);
}

#[test]
fn grass_block_gets_better_side_coating() {
    let cfg = enabled_cfg();
    let reg = NatureModRegistry::new();
    // One-block slope: lower diagonal same type, same level not opaque.
    let r = evaluate_block_visual_pipeline(
        &BlockVisualInput {
            block_id: "minecraft:grass_block",
            face: Direction::North,
            lower_diagonal_same_type: true,
            ..Default::default()
        },
        &cfg,
        &reg,
    );
    assert_eq!(r.side_face, SideFaceStyle::FullTopTexture);
    // Snowy grass beside snow.
    let r = evaluate_block_visual_pipeline(
        &BlockVisualInput {
            block_id: "minecraft:grass_block",
            face: Direction::East,
            snowy: true,
            adjacent_snow: true,
            ..Default::default()
        },
        &cfg,
        &reg,
    );
    assert_eq!(r.side_face, SideFaceStyle::SnowySide);
    // Flat plain: stays vanilla.
    let r = evaluate_block_visual_pipeline(
        &BlockVisualInput {
            block_id: "minecraft:grass_block",
            face: Direction::North,
            ..Default::default()
        },
        &cfg,
        &reg,
    );
    assert_eq!(r.side_face, SideFaceStyle::VanillaSide);
}

#[test]
fn furnace_gets_relief_mesh_with_emissive_ember() {
    let cfg = enabled_cfg();
    let reg = NatureModRegistry::new();
    let r = evaluate_block_visual_pipeline(
        &BlockVisualInput {
            block_id: "minecraft:furnace",
            distance_sq: 16.0, // 4 blocks, inside the 20-block relief cutoff.
            facing: Direction::South,
            furnace_lit: true,
            ..Default::default()
        },
        &cfg,
        &reg,
    );
    let mesh = r.relief_mesh.expect("furnace relief mesh");
    let emissive = mesh
        .quads
        .iter()
        .filter(|q| q.tint_index == RELIEF_TINT_EMISSIVE)
        .count();
    assert_eq!(emissive, 1, "lit furnace emits exactly one ember quad");
    for q in &mesh.quads {
        for p in &q.positions {
            for c in p {
                assert!(c.is_finite() && (-1e-4..=1.0 + 1e-4).contains(c));
            }
        }
    }

    // Beyond the relief LOD cutoff the furnace stays flat.
    let r = evaluate_block_visual_pipeline(
        &BlockVisualInput {
            block_id: "minecraft:furnace",
            distance_sq: 30.0 * 30.0,
            facing: Direction::South,
            ..Default::default()
        },
        &cfg,
        &reg,
    );
    assert!(r.relief_mesh.is_none());
}

// ---------------------------------------------------------------------------
// Vanilla+ Nature routing
// ---------------------------------------------------------------------------

#[test]
fn modded_leaves_get_sway_and_tint_profile() {
    let cfg = enabled_cfg();
    let mut reg = NatureModRegistry::new();
    reg.register_mod_leaf("biomesoplenty:maple_leaves", [0.8, 0.2, 0.1], true);
    let r = evaluate_block_visual_pipeline(&input("biomesoplenty:maple_leaves"), &cfg, &reg);
    let profile = r.leaf_profile.expect("registered mod leaf profile");
    assert_eq!(profile.rgb, [0.8, 0.2, 0.1]);
    assert!(profile.allow_autumn_tint);
    assert_eq!(r.wind_sway, Some(WindSwayCategory::Leaves));

    // Unregistered `*_leaves` ids still classify via the suffix.
    let r = evaluate_block_visual_pipeline(&input("mymod:ember_leaves"), &cfg, &reg);
    assert!(r.leaf_profile.is_some());
    assert_eq!(r.wind_sway, Some(WindSwayCategory::Leaves));
}

#[test]
fn nature_sub_toggles_gate_leaf_and_sway() {
    let reg = NatureModRegistry::new();
    // Leaves classified by tag, ambience off -> tint gone but sway stays.
    let mut cfg = enabled_cfg();
    cfg.nature.nature_ambience = false;
    let r = evaluate_block_visual_pipeline(
        &BlockVisualInput {
            block_id: "minecraft:oak_leaves",
            tags: &["minecraft:leaves"],
            ..Default::default()
        },
        &cfg,
        &reg,
    );
    assert!(r.leaf_profile.is_none());
    assert_eq!(r.wind_sway, Some(WindSwayCategory::Leaves));

    let mut cfg = enabled_cfg();
    cfg.nature.gentle_wind_sway = false;
    let r = evaluate_block_visual_pipeline(
        &BlockVisualInput {
            block_id: "minecraft:oak_leaves",
            tags: &["minecraft:leaves"],
            ..Default::default()
        },
        &cfg,
        &reg,
    );
    assert!(r.leaf_profile.is_some());
    assert!(r.wind_sway.is_none());
}

// ---------------------------------------------------------------------------
// Robustness
// ---------------------------------------------------------------------------

#[test]
fn non_finite_distance_fails_far() {
    let cfg = enabled_cfg();
    let reg = NatureModRegistry::new();
    for d in [f32::NAN, f32::INFINITY, -1.0] {
        let r = evaluate_block_visual_pipeline(
            &BlockVisualInput {
                block_id: "minecraft:wheat",
                distance_sq: d,
                ..Default::default()
            },
            &cfg,
            &reg,
        );
        assert_eq!(
            r.upgrade_decision,
            MeshUpgradeDecision::Use2dLodFallback,
            "distance_sq {d}"
        );
        assert!(r.upgrade_mesh.is_none());
    }
}

#[test]
fn unrecognised_blocks_get_no_overrides() {
    let cfg = enabled_cfg();
    let reg = NatureModRegistry::new();
    for id in ["minecraft:stone", "minecraft:dirt", "create:brass_casing", ""] {
        let r = evaluate_block_visual_pipeline(&input(id), &cfg, &reg);
        assert!(r.classic_ore.is_none(), "{id}");
        assert!(r.ctm_tile.is_none(), "{id}");
        assert!(r.relief_mesh.is_none(), "{id}");
        assert!(r.upgrade_mesh.is_none(), "{id}");
        assert!(r.wind_sway.is_none(), "{id}");
    }
}
