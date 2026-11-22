// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later

//! Integration tests for [`mod_adapter_core::unified_bundle`]: the
//! `megu-mod-bundle/1` declarative mod schema — full JSON roundtrip of a
//! rich unknown mod, live `UnifiedModRuntime` subsystem driving, fail-closed
//! version/corruption/bounds handling, and clean export of the built-in
//! dimension profiles. All bundle data below is invented test data.

use std::collections::{BTreeMap, BTreeSet};

use mod_adapter_core::creature_primitives::{
    interact_with_item, CreatureInteractResult, CreatureState, PetCommandMode,
};
use mod_adapter_core::dimension_mod::{builtin_dimension_mod_profiles, detect_vertical_portal};
use mod_adapter_core::unified_bundle::{
    export_builtin_dimension_bundles, BundleError, DeclarativeModBundle, UnifiedModRuntime,
    BUNDLE_SCHEMA,
};
use mod_adapter_core::version_compat::{McVersion, VersionRange};
use mod_adapter_core::world_extensions::ProgressionVerdict;

/// A rich invented bundle: all four portal trigger kinds, chained
/// progression barriers, a weighted biome region, an amphibious and a
/// gliding creature, custom channels, and a Maven range admitting both
/// `1.21.1` and `26.3`.
const RICH_BUNDLE_JSON: &str = r##"{
  "schema": "megu-mod-bundle/1",
  "mod_id": "mirrorlands",
  "mc_version_range": "[1.21,)",
  "portals": [
    {
      "name": "mirrorlands:sky_portal",
      "target_dim": "mirrorlands:sky",
      "teleport_grace_ticks": 30,
      "trigger": {
        "kind": "vertical_frame",
        "frame_block": "mirrorlands:mirror_stone",
        "igniter_item": "mirrorlands:mirror_shard",
        "min_width": 2,
        "min_height": 3,
        "max_width": 9,
        "max_height": 9,
        "portal_block": "mirrorlands:sky_portal"
      }
    },
    {
      "name": "mirrorlands:still_pool",
      "target_dim": "mirrorlands:depths",
      "trigger": {
        "kind": "horizontal_pool",
        "border_tags_or_blocks": ["#minecraft:flowers", "mirrorlands:silt"],
        "fluid_block": "minecraft:water",
        "min_size": 4,
        "drop_item": "mirrorlands:pearl",
        "portal_block": "mirrorlands:still_portal"
      }
    },
    {
      "name": "mirrorlands:sky_rocket",
      "target_dim": "mirrorlands:sky",
      "trigger": {
        "kind": "altitude_threshold",
        "min_y_trigger": 480,
        "required_vehicle_or_item": "mirrorlands:glider_pack"
      }
    },
    {
      "name": "mirrorlands:waystone",
      "target_dim": "mirrorlands:mirror",
      "trigger": {
        "kind": "direct_block_interact",
        "block_id": "mirrorlands:waystone",
        "required_item": null
      }
    }
  ],
  "progression_barriers": [
    {
      "id": "mirrorlands:frost_gate",
      "dimension": "mirrorlands:mirror",
      "target_biomes": ["mirrorlands:frost_field", "mirrorlands:glacier_rim"],
      "required_advancements": ["mirrorlands:enter_mirror"],
      "block_break_protected": true,
      "block_place_protected": true,
      "enforce_effects": [
        {"effect": "minecraft:slowness", "duration_ticks": 100, "amplifier": 1}
      ],
      "periodic_damage": {"hearts": 0.5, "interval_ticks": 40}
    },
    {
      "id": "mirrorlands:heart_gate",
      "dimension": "mirrorlands:mirror",
      "target_biomes": ["mirrorlands:heartwood"],
      "required_advancements": ["mirrorlands:enter_mirror", "mirrorlands:sleep_drake"],
      "enforce_effects": [
        {"effect": "minecraft:blindness", "duration_ticks": 60, "amplifier": 0},
        {"effect": "minecraft:mining_fatigue", "duration_ticks": 100, "amplifier": 4}
      ],
      "periodic_damage": null
    }
  ],
  "biome_regions": [
    {
      "mod_id": "mirrorlands",
      "dimension": "mirrorlands:mirror",
      "weight": 1000,
      "biome_overrides": {
        "minecraft:plains": "mirrorlands:frost_field",
        "minecraft:forest": "mirrorlands:heartwood"
      }
    }
  ],
  "creatures": [
    {
      "entity_type": "mirrorlands:mudskipper",
      "max_health": 14.0,
      "tame_items": ["mirrorlands:sponge_snack", "#mirrorlands:fishy_treats"],
      "tame_chance": 0.35,
      "breed_items": ["mirrorlands:kelp_wrap"],
      "heal_items": [{"item": "mirrorlands:algae_cookie", "heal": 3.0}],
      "baby_growth_ticks": 12000,
      "breed_cooldown_ticks": 3000,
      "supports_wander_mode": true,
      "can_shoulder_perch": false,
      "rideable_when_tamed": false,
      "movement": {"kind": "amphibious", "land_speed": 0.1, "swim_speed": 0.6}
    },
    {
      "entity_type": "mirrorlands:sky_ray",
      "max_health": 40.0,
      "tame_items": ["mirrorlands:cloud_berry"],
      "tame_chance": 1.0,
      "breed_items": ["mirrorlands:cloud_berry"],
      "supports_wander_mode": false,
      "can_shoulder_perch": true,
      "rideable_when_tamed": true,
      "movement": {
        "kind": "gliding_flight",
        "ground_speed": 0.2,
        "fly_speed": 0.9,
        "glide_fall_scale": 0.25
      }
    }
  ],
  "custom_channels": ["mirrorlands:sync_v1", "mirrorlands:sky_effects"]
}"##;

fn rich_bundle() -> DeclarativeModBundle {
    DeclarativeModBundle::from_json(RICH_BUNDLE_JSON).expect("rich bundle must parse")
}

fn advancements(ids: &[&str]) -> BTreeSet<String> {
    ids.iter().map(|s| (*s).to_string()).collect()
}

// ===========================================================================
// Roundtrip & schema validation
// ===========================================================================

#[test]
fn rich_bundle_roundtrips_through_json() {
    let bundle = rich_bundle();
    assert_eq!(bundle.mod_id, "mirrorlands");
    assert_eq!(bundle.portals.len(), 4);
    assert_eq!(bundle.progression_barriers.len(), 2);
    assert_eq!(bundle.biome_regions.len(), 1);
    assert_eq!(bundle.creatures.len(), 2);
    assert_eq!(bundle.custom_channels.len(), 2);

    // from_json -> to_json -> from_json is a fixed point, and the emitted
    // document is deterministic (identical bytes on re-emit).
    let once = bundle.to_json();
    let twice = DeclarativeModBundle::from_json(&once)
        .expect("emitted JSON must re-parse")
        .to_json();
    assert_eq!(once, twice);
    let reparsed = DeclarativeModBundle::from_json(&twice).unwrap();
    assert_eq!(reparsed, bundle);

    // Key order is deterministic sorted: schema first in the sorted order.
    assert!(once.contains("\"schema\": \"megu-mod-bundle/1\""));
}

#[test]
fn bundle_version_range_covers_both_targets() {
    let bundle = rich_bundle();
    let range = VersionRange::parse(&bundle.mc_version_range).unwrap();
    assert!(range.contains(McVersion::V1_21_1));
    assert!(range.contains(McVersion::V26_3));
}

// ===========================================================================
// UnifiedModRuntime drives every subsystem from one JSON document
// ===========================================================================

#[test]
fn register_bundle_drives_live_subsystems() {
    let bundle = rich_bundle();
    let mut runtime = UnifiedModRuntime::default();
    let summary = runtime
        .register_bundle(&bundle, McVersion::V1_21_1)
        .expect("dual-target bundle must install on 1.21.1");

    assert_eq!(summary.mod_id, "mirrorlands");
    assert_eq!(summary.target, McVersion::V1_21_1);
    assert_eq!(summary.portals_installed, 4);
    assert_eq!(summary.progression_barriers_installed, 2);
    assert_eq!(summary.biome_regions_installed, 1);
    assert_eq!(summary.creatures_installed, 2);
    assert_eq!(summary.custom_channels_declared, 2);
    assert_eq!(summary.custom_channels_newly_registered, 2);
    assert_eq!(
        runtime.installed_mod_ids(),
        &["mirrorlands".to_string()]
    );
    assert!(runtime.is_channel_registered("mirrorlands:sync_v1"));
    assert!(!runtime.is_channel_registered("mirrorlands:absent"));

    // --- Portals: live vertical-frame detection through the installed rule.
    let rule = runtime
        .portal_registry()
        .find("mirrorlands:sky_portal")
        .expect("installed portal rule");
    let mut world: BTreeMap<(i32, i32, i32), String> = BTreeMap::new();
    // 2x3 interior mirror_stone frame in the x plane at z=7.
    for dy in -1..=3 {
        for da in -1..=2 {
            let on_frame = dy == -1 || dy == 3 || da == -1 || da == 2;
            world.insert(
                (3 + da, 64 + dy, 7),
                if on_frame {
                    "mirrorlands:mirror_stone".to_string()
                } else {
                    "minecraft:air".to_string()
                },
            );
        }
    }
    let block_at = |x: i32, y: i32, z: i32| {
        Some(
            world
                .get(&(x, y, z))
                .cloned()
                .unwrap_or_else(|| "minecraft:air".to_string()),
        )
    };
    let transition = detect_vertical_portal(rule, [4, 65, 7], &block_at)
        .expect("frame must activate");
    assert_eq!(transition.target_dim, "mirrorlands:sky");
    assert_eq!(transition.cell_count, 6);

    // --- Progression: fresh player locked, advancement-holding allowed.
    let engine = runtime.progression_engine();
    let verdict = engine.check_progression(
        "mirrorlands:mirror",
        "mirrorlands:frost_field",
        &advancements(&[]),
        false,
    );
    let ProgressionVerdict::Locked {
        rule_id,
        missing_advancements,
        periodic_damage,
        ..
    } = verdict
    else {
        panic!("fresh player in frost_field must be locked");
    };
    assert_eq!(rule_id, "mirrorlands:frost_gate");
    assert_eq!(missing_advancements, ["mirrorlands:enter_mirror"]);
    assert_eq!(periodic_damage, Some((0.5, 40)));
    assert_eq!(
        engine.check_progression(
            "mirrorlands:mirror",
            "mirrorlands:frost_field",
            &advancements(&["mirrorlands:enter_mirror"]),
            false,
        ),
        ProgressionVerdict::Allowed
    );
    // The second gate chains on two advancements.
    assert!(matches!(
        engine.check_progression(
            "mirrorlands:mirror",
            "mirrorlands:heartwood",
            &advancements(&["mirrorlands:enter_mirror"]),
            false,
        ),
        ProgressionVerdict::Locked { ref rule_id, .. } if rule_id == "mirrorlands:heart_gate"
    ));

    // --- Biome blend: the installed region wins cells and remaps biomes.
    let blender = runtime.biome_blender();
    let mut saw_region_win = false;
    for qx in 0..64 {
        if blender.winning_region_id(7, "mirrorlands:mirror", qx, 0) == "mirrorlands" {
            saw_region_win = true;
            assert_eq!(
                blender.resolve_biome(
                    7,
                    "mirrorlands:mirror",
                    qx,
                    0,
                    "minecraft:plains"
                ),
                "mirrorlands:frost_field"
            );
        }
    }
    assert!(saw_region_win, "weight-1000 region must win some cell");

    // --- Creatures: taming through the installed spec.
    let spec = runtime
        .creature_spec("mirrorlands:mudskipper")
        .expect("installed creature spec");
    let mut state = CreatureState::new(1, "mirrorlands:mudskipper", 14.0, [0.0, 64.0, 0.0]);
    let owner = [7u8; 16];
    let no_tags = BTreeSet::new();
    // rng_roll 0.10 < tame_chance 0.35 -> the feed succeeds.
    let result = interact_with_item(
        &mut state,
        spec,
        &owner,
        Some("mirrorlands:sponge_snack"),
        &no_tags,
        0.10,
    );
    assert_eq!(result, CreatureInteractResult::TameAttempt { succeeded: true });
    assert!(state.is_tamed());
    assert_eq!(state.command_mode, PetCommandMode::Follow);

    // The gliding spec carried its profile through unchanged.
    let ray = runtime.creature_spec("mirrorlands:sky_ray").unwrap();
    assert!(ray.rideable_when_tamed && ray.can_shoulder_perch);
}

#[test]
fn register_bundle_accepts_26_3_target() {
    let bundle = rich_bundle();
    let mut runtime = UnifiedModRuntime::default();
    let summary = runtime
        .register_bundle(&bundle, McVersion::V26_3)
        .expect("dual-target bundle must install on 26.3");
    assert_eq!(summary.target, McVersion::V26_3);
    assert_eq!(runtime.portal_registry().rules().len(), 4);
}

// ===========================================================================
// Fail-closed handling
// ===========================================================================

#[test]
fn version_mismatch_refuses_install() {
    let json = RICH_BUNDLE_JSON.replace("[1.21,)", "[1.20.1,1.20.2)");
    let bundle = DeclarativeModBundle::from_json(&json).unwrap();

    for target in [McVersion::V1_21_1, McVersion::V26_3] {
        let mut runtime = UnifiedModRuntime::default();
        match runtime.register_bundle(&bundle, target) {
            Err(BundleError::IncompatibleVersion {
                mod_id,
                target: t,
                declared,
            }) => {
                assert_eq!(mod_id, "mirrorlands");
                assert_eq!(t, target);
                assert_eq!(declared, "[1.20.1,1.20.2)");
            }
            other => panic!("1.20.1-only bundle must be refused on {target}: {other:?}"),
        }
        // Nothing was installed.
        assert!(runtime.portal_registry().rules().is_empty());
        assert!(runtime.installed_mod_ids().is_empty());
    }
}

#[test]
fn corrupt_unknown_schema_and_unparseable_range_fail_closed() {
    // Not JSON at all.
    assert!(matches!(
        DeclarativeModBundle::from_json("{ not json"),
        Err(BundleError::Json(_))
    ));
    // Wrong root shape.
    assert!(DeclarativeModBundle::from_json("[]").is_err());
    assert!(DeclarativeModBundle::from_json("null").is_err());
    // Unknown or missing schema.
    for bad in [
        r#"{"schema": "megu-mod-bundle/2"}"#,
        r#"{"schema": "megu-mod-bundle/1x"}"#,
        r#"{"mod_id": "mirrorlands"}"#,
    ] {
        assert!(
            matches!(
                DeclarativeModBundle::from_json(bad),
                Err(BundleError::UnknownSchema(_))
            ),
            "must reject {bad}"
        );
    }
    // Invalid mod id / version range.
    for bad in [
        r#"{"schema": "megu-mod-bundle/1", "mod_id": "Bad Mod!"}"#,
        r#"{"schema": "megu-mod-bundle/1", "mod_id": "mirrorlands",
            "mc_version_range": "not a range"}"#,
        r#"{"schema": "megu-mod-bundle/1", "mod_id": "mirrorlands",
            "mc_version_range": "[1.21"}"#,
    ] {
        assert!(
            matches!(
                DeclarativeModBundle::from_json(bad),
                Err(BundleError::Validation(_))
            ),
            "must reject {bad}"
        );
    }
}

#[test]
fn unknown_trigger_movement_and_keys_fail_closed() {
    let base = rich_bundle().to_json();

    let bad_trigger = base.replace(r#""kind": "vertical_frame""#, r#""kind": "wormhole""#);
    assert!(matches!(
        DeclarativeModBundle::from_json(&bad_trigger),
        Err(BundleError::Validation(_))
    ));

    let bad_movement = base.replace(r#""kind": "amphibious""#, r#""kind": "teleport""#);
    assert!(matches!(
        DeclarativeModBundle::from_json(&bad_movement),
        Err(BundleError::Validation(_))
    ));

    // Unknown keys anywhere in the document are rejected.
    let extra_key = base.replace(
        r#""mod_id": "mirrorlands""#,
        r#""mod_id": "mirrorlands", "surprise": true"#,
    );
    assert!(matches!(
        DeclarativeModBundle::from_json(&extra_key),
        Err(BundleError::Validation(_))
    ));

    // Non-resource-location ids and out-of-range numbers.
    let bad_id = base.replace(
        r#""target_dim": "mirrorlands:sky""#,
        r#""target_dim": "Mirror Lands:Sky""#,
    );
    assert!(DeclarativeModBundle::from_json(&bad_id).is_err());
    let bad_chance = base.replace(r#""tame_chance": 0.35"#, r#""tame_chance": 1.5"#);
    assert!(DeclarativeModBundle::from_json(&bad_chance).is_err());
}

#[test]
fn oversized_sections_fail_closed() {
    // A portals array over the 1024-entry section cap.
    let mut json = String::from(
        r#"{"schema": "megu-mod-bundle/1", "mod_id": "mirrorlands",
            "mc_version_range": "[1.21,)", "portals": ["#,
    );
    for i in 0..1100 {
        if i > 0 {
            json.push(',');
        }
        json.push_str(
            r#"{"name": "mirrorlands:p", "target_dim": "mirrorlands:sky",
               "trigger": {"kind": "direct_block_interact",
               "block_id": "mirrorlands:waystone"}}"#,
        );
    }
    json.push_str(
        r#"], "progression_barriers": [], "biome_regions": [],
            "creatures": [], "custom_channels": []}"#,
    );
    assert!(matches!(
        DeclarativeModBundle::from_json(&json),
        Err(BundleError::Validation(_))
    ));

    // Nested lists are capped too (border entries > 512).
    let mut borders = String::new();
    for i in 0..600 {
        if i > 0 {
            borders.push(',');
        }
        borders.push_str(&format!("\"mirrorlands:b{i}\""));
    }
    let json = RICH_BUNDLE_JSON.replace(
        r##""border_tags_or_blocks": ["#minecraft:flowers", "mirrorlands:silt"]"##,
        &format!("\"border_tags_or_blocks\": [{borders}]"),
    );
    assert!(matches!(
        DeclarativeModBundle::from_json(&json),
        Err(BundleError::Validation(_))
    ));
}

#[test]
fn validate_rejects_hand_constructed_invalid_bundle() {
    let mut bundle = rich_bundle();
    bundle.mc_version_range = "[[[".to_string();
    assert!(bundle.validate().is_err());
    let mut runtime = UnifiedModRuntime::default();
    assert!(runtime
        .register_bundle(&bundle, McVersion::V1_21_1)
        .is_err());
    assert!(runtime.portal_registry().rules().is_empty());
}

// ===========================================================================
// Built-in dimension profiles are pure data
// ===========================================================================

#[test]
fn export_builtin_dimension_bundles_roundtrips_every_profile() {
    let profiles = builtin_dimension_mod_profiles();
    let bundles = export_builtin_dimension_bundles();
    assert_eq!(bundles.len(), profiles.len());

    for (bundle, profile) in bundles.iter().zip(profiles.iter()) {
        assert_eq!(bundle.mod_id, profile.mod_id);
        assert_eq!(bundle.portals, profile.portal_rules);
        // The exported bundle admits both targets.
        let range = VersionRange::parse(&bundle.mc_version_range).unwrap();
        assert!(range.contains(McVersion::V1_21_1));
        assert!(range.contains(McVersion::V26_3));
        // Clean JSON roundtrip.
        let back = DeclarativeModBundle::from_json(&bundle.to_json())
            .unwrap_or_else(|e| panic!("{} must re-parse: {e}", profile.mod_id));
        assert_eq!(&back, bundle);
    }

    // Every exported bundle installs into a fresh unified runtime on each
    // target and contributes all its portal rules.
    for target in [McVersion::V1_21_1, McVersion::V26_3] {
        let mut runtime = UnifiedModRuntime::default();
        let expected_portals: usize = profiles.iter().map(|p| p.portal_rules.len()).sum();
        for bundle in &bundles {
            let summary = runtime.register_bundle(bundle, target).unwrap();
            assert_eq!(summary.portals_installed, bundle.portals.len());
        }
        assert_eq!(runtime.portal_registry().rules().len(), expected_portals);
        assert!(
            runtime
                .portal_registry()
                .find("aether:the_aether_portal")
                .is_some()
        );
        assert_eq!(
            runtime.installed_mod_ids(),
            profiles
                .iter()
                .map(|p| p.mod_id.to_string())
                .collect::<Vec<_>>()
        );
    }
}

#[test]
fn schema_constant_is_the_only_accepted_tag() {
    assert_eq!(BUNDLE_SCHEMA, "megu-mod-bundle/1");
    let json = RICH_BUNDLE_JSON.replace(BUNDLE_SCHEMA, "other-schema/9");
    assert!(matches!(
        DeclarativeModBundle::from_json(&json),
        Err(BundleError::UnknownSchema(_))
    ));
}
