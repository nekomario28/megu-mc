// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later

//! Integration tests for `ecosystem_compat`: map-mod fair-play mode, the
//! unified accessory-slot model, and the dual-loader JAR scanner. JAR
//! fixtures are real stored-entry ZIP archives assembled byte-by-byte — no
//! external jars, no JVM, no crates.

use mod_adapter_core::ecosystem_compat::{
    derive_world_level_id, fair_play_control_signals, plan_dual_loader_mods_dir,
    scan_mod_jar, should_transmit_entity, AccessoryInventory, AccessoryItem, AccessoryError,
    AttributeModifier, AttributeTotals, ChannelPayload, FairPlayEnforcer, JarScanError,
    MapFairPlayConfig, ModEnvironment, ModLoaderKind, ModifierOperation, CAVE_LAYER_Y,
    FAIR_PLAY_TRACK_RADIUS, NO_MINIMAP_CODE, XAERO_FAIR_PLAY_CODE, XAERO_MINIMAP_CHANNEL,
    XAERO_RESET_CODE, XAERO_WORLDMAP_CHANNEL,
};
use mod_adapter_core::version_compat::McVersion;

// ---------------------------------------------------------------------------
// ① Map-mod fair-play mode
// ---------------------------------------------------------------------------

#[test]
fn fair_play_config_parses_server_properties_and_toml_pairs() {
    let text = r#"
# mc-spine fair-play section
motd=fairest of them all
view-distance: 10

fair-play = true
allow-cave-map = false
allow-entity-radar = false
allow-waypoint-teleport = true
world-level-id = 13371337
"#;
    let cfg = MapFairPlayConfig::from_properties_text(text).expect("properties parse");
    assert!(cfg.fair_play);
    assert!(!cfg.allow_cave_map);
    assert!(!cfg.allow_entity_radar);
    assert!(cfg.allow_waypoint_teleport);
    assert_eq!(cfg.world_level_id, 13_371_337);

    // TOML-style quoted values and prefixed/dotted keys land on the same fields.
    let cfg = MapFairPlayConfig::from_kv_pairs([
        ("mc_spine.fair-play", "\"true\""),
        ("allow_cave_map", "'false'"),
        ("xaero.allow-entity-radar", "off"),
        ("world_level_id", "-42"),
    ])
    .expect("kv pairs parse");
    assert!(cfg.fair_play);
    assert!(!cfg.allow_entity_radar);
    assert_eq!(cfg.world_level_id, -42);

    // Fail closed: malformed boolean, non-integer id, explicit zero id,
    // and a line with no separator are all hard errors.
    assert!(MapFairPlayConfig::from_kv_pairs([("fair_play", "maybe")]).is_err());
    assert!(MapFairPlayConfig::from_kv_pairs([("world_level_id", "abc")]).is_err());
    assert!(MapFairPlayConfig::from_kv_pairs([("world_level_id", "0")]).is_err());
    assert!(MapFairPlayConfig::from_properties_text("just-a-lonely-line\n").is_err());
}

#[test]
fn world_level_id_is_deterministic_and_nonzero() {
    let a = derive_world_level_id("world");
    let a2 = derive_world_level_id("world");
    let b = derive_world_level_id("world_nether");
    assert_eq!(a, a2);
    assert_ne!(a, 0);
    assert_ne!(a, b);
    assert!(a > 0 && b > 0);
}

#[test]
fn fair_play_signals_emit_xaero_codes_and_level_id_payload() {
    // Fully locked down: fair code + no-minimap code + both level payloads.
    let cfg = MapFairPlayConfig {
        fair_play: true,
        allow_cave_map: false,
        allow_entity_radar: false,
        allow_waypoint_teleport: false,
        world_level_id: 0x0102_0304,
    };
    let sig = fair_play_control_signals(&cfg);
    assert_eq!(
        sig.control_codes,
        vec![
            XAERO_FAIR_PLAY_CODE.to_string(),
            NO_MINIMAP_CODE.to_string()
        ]
    );
    assert_eq!(
        sig.level_id_payloads,
        vec![
            ChannelPayload {
                channel: XAERO_MINIMAP_CHANNEL.to_string(),
                payload: vec![0x00, 0x01, 0x02, 0x03, 0x04],
            },
            ChannelPayload {
                channel: XAERO_WORLDMAP_CHANNEL.to_string(),
                payload: vec![0x00, 0x01, 0x02, 0x03, 0x04],
            },
        ]
    );

    // Waypoints still allowed: fair code only (the map retains utility).
    let sig = fair_play_control_signals(&MapFairPlayConfig {
        allow_waypoint_teleport: true,
        ..cfg.clone()
    });
    assert_eq!(sig.control_codes, vec![XAERO_FAIR_PLAY_CODE.to_string()]);
    assert_eq!(sig.level_id_payloads.len(), 2);

    // Partial allowance: no per-feature client code exists, so the reset
    // code runs and the server-side enforcer carries the restriction.
    let sig = fair_play_control_signals(&MapFairPlayConfig {
        allow_cave_map: true,
        ..cfg.clone()
    });
    assert_eq!(sig.control_codes, vec![XAERO_RESET_CODE.to_string()]);

    // Fair-play off: reset code; level id still separates maps per world.
    let sig = fair_play_control_signals(&MapFairPlayConfig {
        fair_play: false,
        ..cfg.clone()
    });
    assert_eq!(sig.control_codes, vec![XAERO_RESET_CODE.to_string()]);
    assert_eq!(sig.level_id_payloads.len(), 2);

    // Unassigned level id emits no payload (never a bogus 0 id).
    let sig = fair_play_control_signals(&MapFairPlayConfig {
        world_level_id: 0,
        ..cfg.clone()
    });
    assert!(sig.level_id_payloads.is_empty());

    // The enforcer wrapper produces the identical signal set on both host
    // kinds — dedicated server and friend host share one code path.
    let enforcer = FairPlayEnforcer::new(cfg);
    assert_eq!(enforcer.signals(), fair_play_control_signals(enforcer.config()));
}

#[test]
fn server_side_radar_suppression_blocks_occluded_underground_and_far() {
    let cfg = MapFairPlayConfig {
        fair_play: true,
        allow_cave_map: false,
        allow_entity_radar: false,
        allow_waypoint_teleport: true,
        world_level_id: 7,
    };
    let viewer = [0.0, 70.0, 0.0];

    // Hacked-client case: hostile mob 30 blocks away in a deep cave, no
    // line of sight -> suppressed for a non-operator.
    assert!(!should_transmit_entity(&cfg, viewer, [30.0, 20.0, 0.0], true, false, false));
    // The operator sees everything, always.
    assert!(should_transmit_entity(&cfg, viewer, [30.0, 20.0, 0.0], true, false, true));
    // Visible target: transmitted even underground.
    assert!(should_transmit_entity(&cfg, viewer, [30.0, 20.0, 0.0], true, true, false));
    // Nearby occluded underground (inside the 24-block combat radius):
    // still transmitted so melee stays vanilla-smooth.
    assert!(should_transmit_entity(&cfg, viewer, [10.0, 50.0, 0.0], true, false, false));
    // Mid-range occluded on the surface: still transmitted (vanilla
    // tracking range does not require LOS).
    assert!(should_transmit_entity(&cfg, viewer, [40.0, 70.0, 0.0], true, false, false));
    // Far occluded on the surface: suppressed (wallhack radar range).
    let far = FAIR_PLAY_TRACK_RADIUS + 10.0;
    assert!(!should_transmit_entity(&cfg, viewer, [far, 70.0, 0.0], true, false, false));
    // Non-radar targets (passive mobs, items, …) are never suppressed.
    assert!(should_transmit_entity(&cfg, viewer, [30.0, CAVE_LAYER_Y - 20.0, 0.0], false, false, false));

    // Policy off / radar allowed: everything transmits.
    for relaxed in [
        MapFairPlayConfig { fair_play: false, ..cfg.clone() },
        MapFairPlayConfig { allow_entity_radar: true, ..cfg.clone() },
    ] {
        assert!(should_transmit_entity(&relaxed, viewer, [30.0, 20.0, 0.0], true, false, false));
        assert!(should_transmit_entity(&relaxed, viewer, [far, 70.0, 0.0], true, false, false));
    }
}

// ---------------------------------------------------------------------------
// ② Unified accessory slots
// ---------------------------------------------------------------------------

fn modifier(attribute: &str, operation: ModifierOperation, amount: f64) -> AttributeModifier {
    AttributeModifier {
        attribute: attribute.to_string(),
        operation,
        amount,
    }
}

#[test]
fn standard_slots_cover_aether_backpack_and_goggles_generically() {
    let mut inv = AccessoryInventory::standard();
    assert_eq!(
        inv.slot_types(),
        vec!["back", "belt", "body", "charm", "feet", "hands", "head", "necklace", "ring"]
    );

    // Aether-style rings: two ring indices, tag "ring".
    let ring_a = AccessoryItem::new("aether:iron_ring", ["ring"]);
    let ring_b = AccessoryItem::new("aether:golden_ring", ["ring"]);
    inv.equip("ring", 0, ring_a).unwrap();
    inv.equip("ring", 1, ring_b).unwrap();
    // Third ring slot does not exist.
    assert_eq!(
        inv.equip("ring", 2, AccessoryItem::new("aether:diamond_ring", ["ring"])),
        Err(AccessoryError::SlotIndexOutOfRange {
            slot_type: "ring".into(),
            index: 2,
            count: 2
        })
    );

    // Pendant -> necklace via the "pendant" alias; cape -> back; gloves ->
    // hands; a custom "shield" slot type takes the Aether shield.
    inv.equip("necklace", 0, AccessoryItem::new("aether:iron_pendant", ["pendant"]))
        .unwrap();
    inv.equip("back", 0, AccessoryItem::new("aether:red_cape", ["cape"]))
        .unwrap();
    inv.equip("hands", 0, AccessoryItem::new("aether:leather_gloves", ["gloves"]))
        .unwrap();
    inv.add_slot_type("shield", 1, ["shield", "aether_shield"]).unwrap();
    inv.equip("shield", 0, AccessoryItem::new("aether:iron_shield", ["shield"]))
        .unwrap();

    // Backpack -> back (both "back" and "backpack" aliases), goggles ->
    // head, artifact charm -> charm, boots -> feet.
    inv.unequip("back", 0).unwrap();
    inv.equip(
        "back",
        0,
        AccessoryItem::new("sophisticatedbackpacks:backpack", ["backpack"]),
    )
    .unwrap();
    inv.equip("head", 0, AccessoryItem::new("create:goggles", ["goggles"]))
        .unwrap();
    inv.equip("charm", 0, AccessoryItem::new("artifacts:cloud_in_a_bottle", ["artifact"]))
        .unwrap();
    inv.equip("feet", 0, AccessoryItem::new("artifacts:running_shoes", ["boots"]))
        .unwrap();

    // Tag gating is fail-closed: untagged/dirt never equips, wrong-tag
    // items are rejected, and an occupied slot refuses a second item.
    assert_eq!(
        inv.equip("ring", 0, AccessoryItem::new("minecraft:dirt", Vec::<&str>::new())),
        Err(AccessoryError::ItemNotAllowed {
            slot_type: "ring".into(),
            item_id: "minecraft:dirt".into()
        })
    );
    assert_eq!(
        inv.equip("ring", 0, AccessoryItem::new("aether:nether_ring", ["ring"])),
        Err(AccessoryError::SlotOccupied {
            slot_type: "ring".into(),
            index: 0
        })
    );
    assert!(matches!(
        inv.equip("nosuchslot", 0, AccessoryItem::new("x:y", ["ring"])),
        Err(AccessoryError::UnknownSlotType(_))
    ));
    // unequip returns the item and empties the slot; second unequip fails.
    let out = inv.unequip("ring", 0).unwrap();
    assert_eq!(out.item_id, "aether:iron_ring");
    assert!(matches!(
        inv.unequip("ring", 0),
        Err(AccessoryError::SlotEmpty { .. })
    ));
}

#[test]
fn custom_slot_types_validate_counts_and_names() {
    let mut inv = AccessoryInventory::standard();
    assert_eq!(
        inv.add_slot_type("pet", 0, ["pet"]),
        Err(AccessoryError::InvalidSlotCount {
            slot_type: "pet".into(),
            count: 0
        })
    );
    assert_eq!(
        inv.add_slot_type("pet", 17, ["pet"]),
        Err(AccessoryError::InvalidSlotCount {
            slot_type: "pet".into(),
            count: 17
        })
    );
    assert_eq!(
        inv.add_slot_type("ring", 4, ["ring"]),
        Err(AccessoryError::DuplicateSlotType("ring".into()))
    );
    assert_eq!(
        inv.add_slot_type("Bad Name!", 1, ["x"]),
        Err(AccessoryError::InvalidSlotTypeName("Bad Name!".into()))
    );

    // A 16-slot custom type equips every index; an empty allowlist accepts
    // any item (unrestricted slot group).
    inv.add_slot_type("cobblemon:accessory", 16, Vec::<&str>::new()).unwrap();
    inv.equip(
        "cobblemon:accessory",
        15,
        AccessoryItem::new("cobblemon:friendship_band", Vec::<&str>::new()),
    )
    .unwrap();
    assert_eq!(
        inv.item_at("cobblemon:accessory", 15).unwrap().item_id,
        "cobblemon:friendship_band"
    );
}

#[test]
fn attribute_totals_apply_vanilla_three_stage_math() {
    let mut inv = AccessoryInventory::standard();
    inv.equip(
        "ring",
        0,
        AccessoryItem::new("aether:iron_ring", ["ring"]).with_modifiers(vec![
            modifier("minecraft:generic.armor", ModifierOperation::AddValue, 2.0),
            modifier("minecraft:generic.luck", ModifierOperation::AddValue, 1.0),
        ]),
    )
    .unwrap();
    inv.equip(
        "ring",
        1,
        AccessoryItem::new("aether:golden_ring", ["ring"]).with_modifiers(vec![modifier(
            "minecraft:generic.armor",
            ModifierOperation::AddMultipliedBase,
            0.5,
        )]),
    )
    .unwrap();
    inv.equip(
        "charm",
        0,
        AccessoryItem::new("artifacts:obsidian_skull", ["artifact"]).with_modifiers(vec![
            modifier("minecraft:generic.armor", ModifierOperation::AddMultipliedTotal, 0.1),
            modifier("minecraft:generic.armor", ModifierOperation::AddValue, 1.0),
        ]),
    )
    .unwrap();

    let agg = inv.aggregate_attribute_modifiers();
    assert_eq!(
        agg.keys().cloned().collect::<Vec<_>>(),
        vec!["minecraft:generic.armor", "minecraft:generic.luck"]
    );
    let armor = &agg["minecraft:generic.armor"];
    assert_eq!(
        *armor,
        AttributeTotals {
            add_value: 3.0,
            add_multiplied_base: 0.5,
            add_multiplied_total: 0.1,
        }
    );
    // Vanilla Attribute.getValue: (10 + 3 + 10*0.5) * (1 + 0.1) = 18 * 1.1 = 19.8.
    let applied = armor.apply(10.0);
    assert!((applied - 19.8).abs() < 1e-9, "{applied}");
    // Zero base stays zero under pure multipliers; empty totals no-op.
    assert_eq!(AttributeTotals::default().apply(42.0), 42.0);
    assert!(AttributeTotals::default().is_zero());

    // Unequipping removes that item's contribution only.
    inv.unequip("ring", 1).unwrap();
    let agg = inv.aggregate_attribute_modifiers();
    assert_eq!(agg["minecraft:generic.armor"].add_multiplied_base, 0.0);
}

#[test]
fn acc1_persistence_roundtrips_and_fails_closed_on_corruption() {
    let mut inv = AccessoryInventory::standard();
    inv.add_slot_type("shield", 1, ["shield"]).unwrap();
    inv.equip(
        "ring",
        1,
        AccessoryItem::new("aether:golden_ring", ["ring", "golden"]).with_modifiers(vec![
            modifier("minecraft:generic.armor", ModifierOperation::AddValue, 2.0),
            modifier("minecraft:generic.knockback_resistance", ModifierOperation::AddMultipliedTotal, 0.25),
        ]),
    )
    .unwrap();
    inv.equip("back", 0, AccessoryItem::new("aether:red_cape", ["cape"]))
        .unwrap();
    inv.equip("shield", 0, AccessoryItem::new("aether:iron_shield", ["shield"]))
        .unwrap();

    let bytes = inv.to_bytes();
    assert_eq!(&bytes[..4], b"ACC1");
    let decoded = AccessoryInventory::from_bytes(&bytes).expect("roundtrip");
    assert_eq!(decoded, inv);
    // Deterministic: encode(decode(encode(x))) == encode(x).
    assert_eq!(decoded.to_bytes(), bytes);

    // Corruption classes all fail closed.
    let mut bad_crc = bytes.clone();
    let n = bad_crc.len();
    bad_crc[n - 1] ^= 0xFF;
    assert!(matches!(
        AccessoryInventory::from_bytes(&bad_crc),
        Err(AccessoryError::Persistence(ref m)) if m.contains("CRC-32")
    ));
    assert!(AccessoryInventory::from_bytes(&bytes[..bytes.len() - 4]).is_err());
    assert!(AccessoryInventory::from_bytes(b"ACC1").is_err());
    // A trailing byte changes the body, so the CRC gate fires first —
    // still fail-closed with a named Persistence error.
    let mut trailing = bytes.clone();
    trailing.push(0);
    assert!(matches!(
        AccessoryInventory::from_bytes(&trailing),
        Err(AccessoryError::Persistence(_))
    ));
    let mut bad_magic = bytes.clone();
    bad_magic[3] = b'9';
    assert!(AccessoryInventory::from_bytes(&bad_magic).is_err());
}

// ---------------------------------------------------------------------------
// ③ Dual-loader JAR scanner
// ---------------------------------------------------------------------------

fn crc32(data: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &byte in data {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            let mask = 0u32.wrapping_sub(crc & 1);
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}

/// Pack `entries` (name → bytes) into a real stored-entry ZIP archive.
fn jar(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut central = Vec::new();
    for (name, data) in entries {
        let name = name.as_bytes();
        let crc = crc32(data);
        let size = data.len() as u32;
        let offset = out.len() as u32;

        out.extend(0x0403_4B50u32.to_le_bytes());
        out.extend(10u16.to_le_bytes());
        out.extend(0u16.to_le_bytes());
        out.extend(0u16.to_le_bytes());
        out.extend(0u16.to_le_bytes());
        out.extend(0u16.to_le_bytes());
        out.extend(crc.to_le_bytes());
        out.extend(size.to_le_bytes());
        out.extend(size.to_le_bytes());
        out.extend((name.len() as u16).to_le_bytes());
        out.extend(0u16.to_le_bytes());
        out.extend(name);
        out.extend(*data);

        central.extend(0x0201_4B50u32.to_le_bytes());
        central.extend(20u16.to_le_bytes());
        central.extend(10u16.to_le_bytes());
        central.extend(0u16.to_le_bytes());
        central.extend(0u16.to_le_bytes());
        central.extend(0u16.to_le_bytes());
        central.extend(0u16.to_le_bytes());
        central.extend(crc.to_le_bytes());
        central.extend(size.to_le_bytes());
        central.extend(size.to_le_bytes());
        central.extend((name.len() as u16).to_le_bytes());
        central.extend(0u16.to_le_bytes());
        central.extend(0u16.to_le_bytes());
        central.extend(0u16.to_le_bytes());
        central.extend(0u16.to_be_bytes());
        central.extend(0u32.to_le_bytes());
        central.extend(offset.to_le_bytes());
        central.extend(name);
    }
    let cd_offset = out.len() as u32;
    let cd_size = central.len() as u32;
    out.extend(central);
    out.extend(0x0605_4B50u32.to_le_bytes());
    out.extend(0u16.to_le_bytes());
    out.extend(0u16.to_le_bytes());
    out.extend((entries.len() as u16).to_le_bytes());
    out.extend((entries.len() as u16).to_le_bytes());
    out.extend(cd_size.to_le_bytes());
    out.extend(cd_offset.to_le_bytes());
    out.extend(0u16.to_le_bytes());
    out
}

fn fabric_jar() -> Vec<u8> {
    let manifest = br#"{
        "schemaVersion": 1,
        "id": "fabricharm",
        "version": "1.2.3",
        "environment": "*",
        "entrypoints": {
            "main": ["com.fabricharm.FabricInit"]
        },
        "depends": {
            "fabricloader": ">=0.15.0",
            "minecraft": "~1.21",
            "mylibrary": ">=1.0"
        },
        "mixins": ["fabricharm.mixins.json"]
    }"#;
    let mixin_cfg = br#"{
        "required": true,
        "package": "com.fabricharm.mixin",
        "mixins": ["LivingEntityMixin"]
    }"#;
    jar(&[
        ("fabric.mod.json", manifest.as_slice()),
        ("fabricharm.mixins.json", mixin_cfg.as_slice()),
        ("com/fabricharm/FabricInit.class", b"\xca\xfe\xba\xbe".as_slice()),
    ])
}

fn neoforge_jar() -> Vec<u8> {
    let manifest = br#"modLoader="javafml"
loaderVersion="[4.0.39]"
license="MIT"

[[mods]]
modId="neoharm"
version="2.0.0"
displayName="Neo Harm"

[[dependencies.neoharm]]
modId="minecraft"
type="required"
versionRange="[1.21.1,1.22)"
side="BOTH"

[[dependencies.neoharm]]
modId="neoforge"
type="required"
versionRange="[21.1.172,)"
side="BOTH"

[[mixins]]
config="neoharm.mixins.json"
"#;
    jar(&[
        ("META-INF/neoforge.mods.toml", manifest.as_slice()),
        ("neoharm.mixins.json", br#"{"package":"com.neoharm.mixin","mixins":[]}"#.as_slice()),
        ("com/neoharm/NeoInit.class", b"\xca\xfe\xba\xbe".as_slice()),
    ])
}

fn client_only_fabric_jar() -> Vec<u8> {
    let manifest = br#"{
        "schemaVersion": 1,
        "id": "prettyshader",
        "version": "0.9.0",
        "environment": "client"
    }"#;
    jar(&[("fabric.mod.json", manifest.as_slice())])
}

fn sodium_jar() -> Vec<u8> {
    let manifest = br#"{
        "schemaVersion": 1,
        "id": "sodium",
        "version": "0.6.13",
        "environment": "*"
    }"#;
    jar(&[
        ("fabric.mod.json", manifest.as_slice()),
        ("sodium.mixins.json", b"{}".as_slice()),
    ])
}

#[test]
fn fabric_manifest_extracts_full_metadata() {
    let desc = scan_mod_jar(&fabric_jar()).expect("fabric jar scans");
    assert_eq!(desc.loader, ModLoaderKind::Fabric);
    assert_eq!(desc.mod_id, "fabricharm");
    assert_eq!(desc.declared_mod_ids, vec!["fabricharm"]);
    assert_eq!(desc.version, "1.2.3");
    assert_eq!(desc.environment, ModEnvironment::Both);
    let range = desc.declared_mc_range.expect("mc range parsed");
    assert!(range.contains(McVersion::V1_21_1));
    assert!(!range.contains(McVersion::V1_20_4));
    assert_eq!(desc.mixin_configs, vec!["fabricharm.mixins.json"]);
    assert_eq!(desc.entrypoints, vec!["com.fabricharm.FabricInit"]);
    let lib = desc
        .dependencies
        .iter()
        .find(|d| d.mod_id == "mylibrary")
        .expect("mylibrary dep present");
    assert!(lib.required);
    assert_eq!(lib.version_range.as_deref(), Some(">=1.0"));
}

#[test]
fn neoforge_manifest_extracts_full_metadata() {
    let desc = scan_mod_jar(&neoforge_jar()).expect("neoforge jar scans");
    assert_eq!(desc.loader, ModLoaderKind::NeoForge);
    assert_eq!(desc.mod_id, "neoharm");
    assert_eq!(desc.version, "2.0.0");
    assert_eq!(desc.environment, ModEnvironment::Both);
    let range = desc.declared_mc_range.expect("mc range parsed");
    assert!(range.contains(McVersion::V1_21_1));
    assert!(!range.contains(McVersion::V1_20_4));
    assert_eq!(desc.mixin_configs, vec!["neoharm.mixins.json"]);
    assert!(desc
        .dependencies
        .iter()
        .any(|d| d.mod_id == "neoforge" && d.required));
}

#[test]
fn legacy_forge_and_dual_jars_detect_loader_kind() {
    let legacy_toml = br#"modLoader="javafml"
loaderVersion="[47,48)"
license="MIT"
[[mods]]
modId="legacyharm"
version="1.0.0"
"#;
    let desc = scan_mod_jar(&jar(&[("META-INF/mods.toml", legacy_toml.as_slice())]))
        .expect("legacy forge jar scans");
    assert_eq!(desc.loader, ModLoaderKind::ForgeLegacy);
    assert_eq!(desc.mod_id, "legacyharm");

    // One archive carrying both manifests is a real dual-loader release.
    let fabric_json = br#"{"schemaVersion":1,"id":"botharm","version":"3.1.0","environment":"*"}"#;
    let neo_toml = br#"modLoader="javafml"
loaderVersion="[4.0.39]"
license="MIT"
[[mods]]
modId="botharm"
version="3.1.0"
"#;
    let desc = scan_mod_jar(&jar(&[
        ("fabric.mod.json", fabric_json.as_slice()),
        ("META-INF/neoforge.mods.toml", neo_toml.as_slice()),
    ]))
    .expect("dual jar scans");
    assert_eq!(desc.loader, ModLoaderKind::DualFabricNeoForge);
    assert_eq!(desc.mod_id, "botharm");
}

#[test]
fn datapack_jar_and_bare_jar_classify_fail_closed() {
    // pack.mcmeta + data/ -> DatapackOnly.
    let desc = scan_mod_jar(&jar(&[
        ("pack.mcmeta", br#"{"pack":{"pack_format":48,"description":"dp"}}"#.as_slice()),
        ("data/example/recipes/x.json", b"{}".as_slice()),
    ]))
    .expect("datapack jar scans");
    assert_eq!(desc.loader, ModLoaderKind::DatapackOnly);
    assert!(desc.mod_id.is_empty());

    // A bare archive with neither manifest nor datapack evidence rejects.
    let err = scan_mod_jar(&jar(&[("com/x/Only.class", b"\xca\xfe\xba\xbe".as_slice())]))
        .unwrap_err();
    assert!(matches!(err, JarScanError::Manifest(_)), "{err}");
}

#[test]
fn plan_merges_fabric_and_neoforge_into_one_unified_table() {
    let fab = fabric_jar();
    let neo = neoforge_jar();
    let client = client_only_fabric_jar();
    let sodium = sodium_jar();
    let corrupt = {
        let mut j = jar(&[("fabric.mod.json", b"{}".as_slice())]);
        j.truncate(j.len() - 4); // destroy the EOCD record
        j
    };

    let plan = plan_dual_loader_mods_dir([
        ("sodium-fabric-0.6.13.jar", sodium.as_slice()),
        ("fabricharm-1.2.3.jar", fab.as_slice()),
        ("corrupt.jar", corrupt.as_slice()),
        ("prettyshader-0.9.0.jar", client.as_slice()),
        ("neoharm-2.0.0.jar", neo.as_slice()),
    ]);

    // Fabric + NeoForge coexist in ONE unified table; the client-only mod
    // and the superseded perf mod are split out; the corrupt jar fails
    // closed with a named reason.
    assert_eq!(plan.unified_mod_ids(), vec!["fabricharm", "neoharm"]);
    assert_eq!(plan.unified_mods[0].loader, ModLoaderKind::Fabric);
    assert_eq!(plan.unified_mods[1].loader, ModLoaderKind::NeoForge);
    assert_eq!(plan.skipped_client_only.len(), 1);
    assert_eq!(plan.skipped_client_only[0].mod_id, "prettyshader");
    assert_eq!(plan.skipped_superseded.len(), 1);
    assert_eq!(plan.skipped_superseded[0].mod_id, "sodium");
    assert_eq!(plan.rejected_jars.len(), 1);
    assert_eq!(plan.rejected_jars[0].jar_name, "corrupt.jar");
    assert!(plan.duplicate_conflicts.is_empty());

    // Both loader ecosystems' mixin configs flow into the same lifter
    // pipeline (Intermediary for Fabric, Mojang for NeoForge).
    let mut cfgs = plan.all_mixin_configs();
    cfgs.sort();
    assert_eq!(cfgs, vec!["fabricharm.mixins.json", "neoharm.mixins.json"]);

    // `mylibrary` is required by fabricharm and provided by nothing.
    assert_eq!(plan.missing_dependencies.len(), 1);
    assert_eq!(plan.missing_dependencies[0].mod_id, "fabricharm");
    assert_eq!(plan.missing_dependencies[0].required_mod_id, "mylibrary");
    assert_eq!(
        plan.missing_dependencies[0].version_range.as_deref(),
        Some(">=1.0")
    );
}

#[test]
fn plan_detects_duplicate_mod_id_and_satisfies_present_deps() {
    // Two different jars both declaring "fabricharm"; the fabric one also
    // requires "mylibrary" (which is present, so no missing-dep flag).
    let a = jar(&[(
        "fabric.mod.json",
        br#"{"id":"fabricharm","version":"1.0.0","depends":{"mylibrary":">=1.0"}}"#.as_slice(),
    )]);
    let b_toml = br#"modLoader="javafml"
loaderVersion="[4.0.39]"
license="MIT"
[[mods]]
modId="fabricharm"
version="9.9.9"
"#;
    let b = jar(&[("META-INF/neoforge.mods.toml", b_toml.as_slice())]);

    // Library jar satisfying the dependency.
    let lib_json = br#"{"schemaVersion":1,"id":"mylibrary","version":"1.4.0","environment":"*"}"#;
    let lib = jar(&[("fabric.mod.json", lib_json.as_slice())]);

    let plan = plan_dual_loader_mods_dir([
        ("fabricharm-neoforge-9.9.9.jar", b.as_slice()),
        ("fabricharm-1.0.0.jar", a.as_slice()),
        ("mylibrary-1.4.0.jar", lib.as_slice()),
    ]);

    assert_eq!(plan.duplicate_conflicts.len(), 1);
    assert_eq!(plan.duplicate_conflicts[0].mod_id, "fabricharm");
    assert_eq!(
        plan.duplicate_conflicts[0].jar_names,
        vec!["fabricharm-1.0.0.jar", "fabricharm-neoforge-9.9.9.jar"]
    );
    // First claimant in sorted order stays in the table.
    assert_eq!(
        plan.unified_mod_ids(),
        vec!["fabricharm", "mylibrary"]
    );
    // The dependency fabricharm declares is satisfied by the present jar.
    assert!(plan.missing_dependencies.is_empty());
}

#[test]
fn scanner_rejects_malformed_manifests() {
    // fabric.mod.json missing "id".
    let bad_json = jar(&[("fabric.mod.json", br#"{"version":"1.0"}"#.as_slice())]);
    assert!(matches!(
        scan_mod_jar(&bad_json),
        Err(JarScanError::Manifest(_))
    ));

    // Invalid JSON outright.
    let corrupt_json = jar(&[("fabric.mod.json", b"{not json".as_slice())]);
    assert!(matches!(
        scan_mod_jar(&corrupt_json),
        Err(JarScanError::Json(_))
    ));

    // TOML missing [[mods]].
    let no_mods = jar(&[(
        "META-INF/neoforge.mods.toml",
        br#"modLoader="javafml"
license="MIT"
"#
        .as_slice(),
    )]);
    assert!(matches!(
        scan_mod_jar(&no_mods),
        Err(JarScanError::Manifest(_))
    ));

    // Malformed TOML.
    let bad_toml = jar(&[(
        "META-INF/neoforge.mods.toml",
        b"[[mods]\nmodId=" .as_slice(),
    )]);
    assert!(matches!(
        scan_mod_jar(&bad_toml),
        Err(JarScanError::Toml(_))
    ));
}

#[test]
fn deeply_nested_toml_arrays_and_tables_fail_closed_without_stack_overflow() {
    let deep_array = format!("x = {}{}\n", "[".repeat(10_000), "]".repeat(10_000));
    let array_jar = jar(&[("META-INF/neoforge.mods.toml", deep_array.as_bytes())]);
    assert!(matches!(
        scan_mod_jar(&array_jar),
        Err(JarScanError::Toml(_))
    ));

    let deep_table = format!("x = {}{}\n", "{a=".repeat(10_000), "}".repeat(10_000));
    let table_jar = jar(&[("META-INF/neoforge.mods.toml", deep_table.as_bytes())]);
    assert!(matches!(
        scan_mod_jar(&table_jar),
        Err(JarScanError::Toml(_))
    ));
}
