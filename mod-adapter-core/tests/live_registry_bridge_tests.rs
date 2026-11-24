// SPDX-License-Identifier: LGPL-3.0-or-later

use std::fs;
use std::path::Path;

use mod_adapter_core::auto_adapter::{auto_adapt_mods_dir, AutoAdaptOptions};
use mod_adapter_core::live_registry_bridge::{
    decode_registry_sync_frame, RegistryPlanError, RegistrySyncError, UnifiedModRegistryPlan,
};

use megu_core::hash::crc32_ieee as crc32;

fn jar(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut central = Vec::new();
    for (name, data) in entries {
        let name = name.as_bytes();
        let crc = crc32(data);
        let size = data.len() as u32;
        let offset = out.len() as u32;
        out.extend(0x0403_4B50u32.to_le_bytes());
        out.extend(20u16.to_le_bytes());
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
        central.extend(20u16.to_le_bytes());
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
        central.extend(0u16.to_le_bytes());
        central.extend(0u32.to_le_bytes());
        central.extend(offset.to_le_bytes());
        central.extend(name);
    }
    let central_offset = out.len() as u32;
    out.extend(central);
    let central_size = out.len() as u32 - central_offset;
    out.extend(0x0605_4B50u32.to_le_bytes());
    out.extend(0u16.to_le_bytes());
    out.extend(0u16.to_le_bytes());
    out.extend((entries.len() as u16).to_le_bytes());
    out.extend((entries.len() as u16).to_le_bytes());
    out.extend(central_size.to_le_bytes());
    out.extend(central_offset.to_le_bytes());
    out.extend(0u16.to_le_bytes());
    out
}

fn write_mod(dir: &Path, filename: &str, id: &str, content: &[(&str, &[u8])]) {
    let manifest = format!(r#"{{"schemaVersion":1,"id":"{id}","version":"1.0"}}"#);
    let mut entries = vec![("fabric.mod.json", manifest.as_bytes())];
    entries.extend_from_slice(content);
    fs::write(dir.join(filename), jar(&entries)).unwrap();
}

fn fixture(root: &Path) {
    let mods = root.join("mods");
    fs::create_dir_all(&mods).unwrap();
    write_mod(
        &mods,
        "alpha_ores.jar",
        "alpha_ores",
        &[
            (
                "assets/alpha_ores/blockstates/copper_ore.json",
                br#"{"variants":{"facing=north,lit=false":{},"facing=south,lit=true":{}}}"#,
            ),
            ("assets/alpha_ores/items/raw_copper.json", br#"{"model":"x"}"#),
            (
                "data/alpha_ores/recipes/smelt.json",
                br#"{"type":"minecraft:smelting","ingredient":{"item":"alpha_ores:raw_copper"},"result":{"item":"minecraft:copper_ingot"}}"#,
            ),
            (
                "data/alpha_ores/tags/items/ores.json",
                br#"{"values":["alpha_ores:raw_copper"]}"#,
            ),
            (
                "data/alpha_ores/loot_tables/ore.json",
                br#"{"type":"minecraft:block"}"#,
            ),
        ],
    );
    write_mod(
        &mods,
        "beta_kitchen.jar",
        "beta_kitchen",
        &[
            (
                "assets/beta_kitchen/blockstates/cutting_board.json",
                br#"{"variants":{"axis=x,facing=north":{},"axis=y,facing=south":{},"axis=z,facing=west":{}}}"#,
            ),
            ("assets/beta_kitchen/items/pie.json", br#"{"model":"x"}"#),
            (
                "data/beta_kitchen/recipes/pie.json",
                br#"{"type":"beta_kitchen:baking","input":{"item":"minecraft:wheat"},"output":{"item":"beta_kitchen:pie"}}"#,
            ),
        ],
    );
    write_mod(
        &mods,
        "create_mini.jar",
        "create_mini",
        &[
            (
                "assets/create_mini/blockstates/shaft.json",
                br#"{"variants":{"axis=x":{},"axis=y":{},"axis=z":{}}}"#,
            ),
            ("assets/create_mini/items/shaft.json", br#"{"model":"x"}"#),
        ],
    );
}

fn plan(root: &Path) -> UnifiedModRegistryPlan {
    let report = auto_adapt_mods_dir(&root.join("mods"), &AutoAdaptOptions::default()).unwrap();
    UnifiedModRegistryPlan::from_auto_adapt(50_000, 80_000, &report).unwrap()
}

#[test]
fn multi_mod_plan_expands_deterministically_and_syncs() {
    let first = std::env::temp_dir().join(format!("mrs1-a-{}", std::process::id()));
    let second = std::env::temp_dir().join(format!("mrs1-b-{}", std::process::id()));
    let _ = fs::remove_dir_all(&first);
    let _ = fs::remove_dir_all(&second);
    fixture(&first);
    fixture(&second);

    let registry = plan(&first);
    let repeated = plan(&second);
    assert_eq!(registry, repeated);
    assert_eq!(registry.block_states.len(), 16); // 2*2 + 3*3 + 3
    assert_eq!(registry.block_states[0].block_id, "alpha_ores:copper_ore");
    assert_eq!(registry.block_states[0].properties.len(), 2);
    assert_eq!(
        registry.block_states[0].properties[0],
        ("facing".into(), "north".into())
    );
    assert_eq!(
        registry.block_states[0].properties[1],
        ("lit".into(), "false".into())
    );
    assert_eq!(
        registry.resolve_block_state(50_000),
        Some(&registry.block_states[0])
    );
    assert_eq!(
        registry.block_state_id(
            "alpha_ores:copper_ore",
            &[
                ("lit".into(), "true".into()),
                ("facing".into(), "south".into())
            ]
        ),
        Some(50_003)
    );
    assert!(registry.content.recipes.contains_key("alpha_ores:smelt"));
    assert!(registry.content.tags["alpha_ores:items/ores"].contains("alpha_ores:raw_copper"));
    assert!(registry.content.loot_tables.contains("alpha_ores:ore"));
    assert!(registry
        .items
        .values()
        .any(|name| name == "beta_kitchen:pie"));
    assert!(registry
        .items
        .values()
        .any(|name| name == "create_mini:shaft"));

    let frame = registry.encode_registry_sync_frame();
    let decoded = decode_registry_sync_frame(&frame).unwrap();
    assert_eq!(decoded.base_block_state_id, registry.base_block_state_id);
    assert_eq!(decoded.base_item_id, registry.base_item_id);
    assert_eq!(decoded.block_states, registry.block_states);
    assert_eq!(decoded.items, registry.items);
    assert!(frame.starts_with(b"MRS1"));
    assert!(registry.resolve_item(80_000).is_some());

    fs::remove_dir_all(first).unwrap();
    fs::remove_dir_all(second).unwrap();
}

#[test]
fn sync_decoder_fails_closed_and_plan_caps_property_explosion() {
    let root = std::env::temp_dir().join(format!("mrs1-bad-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fixture(&root);
    let registry = plan(&root);
    let frame = registry.encode_registry_sync_frame();

    assert_eq!(
        decode_registry_sync_frame(&frame[..frame.len() - 1]),
        Err(RegistrySyncError::ChecksumMismatch)
    );
    let mut corrupt = frame.clone();
    corrupt[6] ^= 0x80;
    assert_eq!(
        decode_registry_sync_frame(&corrupt),
        Err(RegistrySyncError::ChecksumMismatch)
    );
    let mut trailing = frame.clone();
    trailing.insert(trailing.len() - 4, 0);
    let checksum = crc32(&trailing[..trailing.len() - 4]);
    let end = trailing.len();
    trailing[end - 4..].copy_from_slice(&checksum.to_be_bytes());
    assert_eq!(
        decode_registry_sync_frame(&trailing),
        Err(RegistrySyncError::TrailingBytes)
    );
    let mut oversized = frame.clone();
    oversized[14..18].copy_from_slice(&u32::MAX.to_be_bytes());
    let checksum = crc32(&oversized[..oversized.len() - 4]);
    let end = oversized.len();
    oversized[end - 4..].copy_from_slice(&checksum.to_be_bytes());
    assert_eq!(
        decode_registry_sync_frame(&oversized),
        Err(RegistrySyncError::TooManyBlockStates)
    );

    let mut properties = String::from("{\"variants\":{\"");
    for index in 0..17 {
        if index > 0 {
            properties.push(',');
        }
        properties.push_str(&format!("p{index}=a|b"));
    }
    properties.push_str("\":{}}}");
    let mods = root.join("explosive");
    fs::create_dir_all(&mods).unwrap();
    let blockstate = properties.into_bytes();
    write_mod(
        &mods,
        "hostile.jar",
        "hostile",
        &[("assets/hostile/blockstates/explosive.json", &blockstate)],
    );
    let hostile_report = auto_adapt_mods_dir(&mods, &AutoAdaptOptions::default()).unwrap();
    assert!(matches!(
        UnifiedModRegistryPlan::from_auto_adapt(1, 1, &hostile_report),
        Err(RegistryPlanError::PropertyExpansionTooLarge { .. })
    ));
    fs::remove_dir_all(root).unwrap();
}
