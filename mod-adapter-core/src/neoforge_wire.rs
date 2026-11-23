// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later

//! Universal Protocol 767 VarInt/string codecs and NeoForge 1.21.1
//! Configuration `frozen_registry*` synchronization frames for ANY mod's
//! [`BlockRegistry`].

use crate::blockstates::BlockRegistry;

pub const CHANNEL_FROZEN_REGISTRY_SYNC_START: &str = "neoforge:frozen_registry_sync_start";
pub const CHANNEL_FROZEN_REGISTRY: &str = "neoforge:frozen_registry";
pub const CHANNEL_FROZEN_REGISTRY_SYNC_COMPLETED: &str = "neoforge:frozen_registry_sync_completed";

/// Protocol 767 Configuration clientbound `0x07` Registry Data packet ID.
pub const CONFIG_REGISTRY_DATA_PACKET_ID: u8 = 0x07;

/// Maximum UTF-8 length accepted by the vanilla string codec for registry and
/// entry names (`FriendlyByteBuf` default cap).
pub const MAX_REGISTRY_DATA_NAME_LEN: usize = 32_767;

/// Protocol 767 VarInt encoder.
pub fn write_varint(out: &mut Vec<u8>, value: u32) {
    megu_core::varint::write_varint_i32(value as i32, out);
}

/// Protocol 767 VarInt decoder with strict 5-byte bound.
pub fn read_varint(bytes: &[u8], cursor: &mut usize) -> Option<u32> {
    let tail = bytes.get(*cursor..)?;
    let (value, consumed) = megu_core::varint::read_varint_i32(tail).ok()?;
    *cursor = cursor.checked_add(consumed)?;
    Some(value as u32)
}

/// Protocol 767 UTF-8 string encoder (`VarInt(len) + utf8 bytes`).
pub fn write_mc_string(out: &mut Vec<u8>, text: &str) {
    write_varint(out, text.len() as u32);
    out.extend_from_slice(text.as_bytes());
}

pub fn read_mc_string(bytes: &[u8], cursor: &mut usize, max_len: usize) -> Option<String> {
    let len = read_varint(bytes, cursor)? as usize;
    if len > max_len {
        return None;
    }
    let slice = bytes.get(*cursor..*cursor + len)?;
    *cursor += len;
    std::str::from_utf8(slice).ok().map(str::to_string)
}

/// Build the ordered NeoForge 1.21.1 Configuration `0x01` Custom Payload
/// `(channel, payload)` frames synchronizing any frozen [`BlockRegistry`].
pub fn build_neoforge_frozen_registry_frames<R>(
    registry: &BlockRegistry<R>,
) -> Vec<(String, Vec<u8>)> {
    let mut frames = Vec::with_capacity(3);

    let mut start_body = Vec::new();
    write_varint(&mut start_body, 1);
    write_mc_string(&mut start_body, "minecraft:block");
    frames.push((CHANNEL_FROZEN_REGISTRY_SYNC_START.to_string(), start_body));

    let mut snap_body = Vec::new();
    write_mc_string(&mut snap_body, "minecraft:block");
    write_varint(&mut snap_body, registry.state_count() as u32);
    for entry in registry.states() {
        write_varint(&mut snap_body, entry.state_id);
        write_mc_string(&mut snap_body, &entry.canonical);
    }
    frames.push((CHANNEL_FROZEN_REGISTRY.to_string(), snap_body));

    frames.push((
        CHANNEL_FROZEN_REGISTRY_SYNC_COMPLETED.to_string(),
        Vec::new(),
    ));
    frames
}

/// Build a Protocol 767 Configuration `0x07` Registry Data body — packet ID
/// included, transport length/compression framing excluded — synchronizing one
/// custom block/item registry. `entry_names` must list entry resource
/// locations in ascending runtime-ID order; each encodes with an absent
/// optional NBT payload, the static-registry form used for block/item data.
pub fn build_config_registry_data_0x07(
    registry_id: &str,
    entry_names: &[String],
) -> Result<Vec<u8>, &'static str> {
    if registry_id.len() > MAX_REGISTRY_DATA_NAME_LEN {
        return Err("oversized Configuration Registry Data registry name");
    }
    if entry_names.len() > i32::MAX as usize {
        return Err("too many Configuration Registry Data entries");
    }
    for name in entry_names {
        if name.len() > MAX_REGISTRY_DATA_NAME_LEN {
            return Err("oversized Configuration Registry Data entry name");
        }
    }
    let mut body = Vec::with_capacity(8 + entry_names.len() * 24);
    body.push(CONFIG_REGISTRY_DATA_PACKET_ID);
    write_mc_string(&mut body, registry_id);
    write_varint(&mut body, entry_names.len() as u32);
    for name in entry_names {
        write_mc_string(&mut body, name);
        body.push(0);
    }
    Ok(body)
}

/// Decode a body produced by [`build_config_registry_data_0x07`], returning the
/// registry identifier and ordered entry names. Only the absent-NBT static
/// form is supported; any present entry payload fails closed.
pub fn decode_config_registry_data_0x07(body: &[u8]) -> Option<(String, Vec<String>)> {
    if body.first().copied() != Some(CONFIG_REGISTRY_DATA_PACKET_ID) {
        return None;
    }
    let mut cursor = 1usize;
    let registry = read_mc_string(body, &mut cursor, MAX_REGISTRY_DATA_NAME_LEN)?;
    let count = read_varint(body, &mut cursor)? as usize;
    // Each entry needs at least a VarInt length byte plus the absent-NBT byte,
    // so a count beyond half the remaining body is provably malformed.
    if count > (body.len() - cursor) / 2 {
        return None;
    }
    let mut entries = Vec::with_capacity(count);
    for _ in 0..count {
        let name = read_mc_string(body, &mut cursor, MAX_REGISTRY_DATA_NAME_LEN)?;
        match body.get(cursor) {
            Some(0) => cursor += 1,
            _ => return None,
        }
        entries.push(name);
    }
    (cursor == body.len()).then_some((registry, entries))
}

// The Fabric-family counterpart of the `neoforge:frozen_registry*` frames
// above: `fabric:registry/sync/direct` carries the same
// registry→(entry, raw id) snapshot on Fabric API 1.21.1 and reuses this
// module's VarInt/string primitives (the codec itself lives in
// `crate::loader_extensions` next to the other loader-extension codecs).
pub use crate::loader_extensions::{
    decode_fabric_registry_sync_direct, encode_fabric_registry_sync_direct,
    FabricRegistrySyncMap, CHANNEL_FABRIC_REGISTRY_SYNC_DIRECT,
    MAX_FABRIC_REGISTRY_SYNC_BODY_BYTES, MAX_FABRIC_REGISTRY_SYNC_ENTRIES,
    MAX_FABRIC_REGISTRY_SYNC_REGISTRIES,
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::blockstates::{BlockDef, PropertyDef, UniversalBlockRole, DEFAULT_BASE_STATE_ID};

    fn test_registry() -> BlockRegistry {
        BlockRegistry::build(
            DEFAULT_BASE_STATE_ID,
            vec![
                BlockDef {
                    id: "mekanism:universal_cable".into(),
                    properties: vec![PropertyDef::new("tier", &["basic", "advanced"])],
                    load_factor: 0.0,
                    capacity_factor: 0.0,
                    role: UniversalBlockRole::NetworkCable,
                },
                BlockDef {
                    id: "ae2:fluix_cable".into(),
                    properties: vec![],
                    load_factor: 0.5,
                    capacity_factor: 0.0,
                    role: UniversalBlockRole::NetworkCable,
                },
                BlockDef {
                    id: "pipez:item_pipe".into(),
                    properties: vec![],
                    load_factor: 0.0,
                    capacity_factor: 0.0,
                    role: UniversalBlockRole::TransportPipe,
                },
            ],
        )
        .unwrap()
    }

    #[test]
    fn registry_data_0x07_round_trips_custom_block_and_item_registries() {
        let reg = test_registry();
        let block_names: Vec<String> = reg.blocks().iter().map(|b| b.id.clone()).collect();
        // Blocks sort canonically by (namespace, path) regardless of input order.
        assert_eq!(
            block_names,
            [
                "ae2:fluix_cable",
                "mekanism:universal_cable",
                "pipez:item_pipe"
            ]
        );

        let body = build_config_registry_data_0x07("minecraft:block", &block_names).unwrap();
        assert_eq!(body[0], CONFIG_REGISTRY_DATA_PACKET_ID);
        let (registry, entries) = decode_config_registry_data_0x07(&body).unwrap();
        assert_eq!(registry, "minecraft:block");
        assert_eq!(entries, block_names);

        let item_names = vec![
            "ae2:certus_quartz_crystal".to_string(),
            "mekanism:bio_fuel".to_string(),
        ];
        let item_body = build_config_registry_data_0x07("minecraft:item", &item_names).unwrap();
        assert_eq!(
            decode_config_registry_data_0x07(&item_body),
            Some(("minecraft:item".to_string(), item_names))
        );
    }

    #[test]
    fn registry_data_0x07_rejects_malformed_bodies_and_oversized_names() {
        let names = vec!["mekanism:universal_cable".to_string()];
        let body = build_config_registry_data_0x07("minecraft:block", &names).unwrap();

        // Wrong packet ID.
        let mut wrong_id = body.clone();
        wrong_id[0] = 0x08;
        assert!(decode_config_registry_data_0x07(&wrong_id).is_none());
        // Truncation and trailing garbage both fail closed.
        assert!(decode_config_registry_data_0x07(&body[..body.len() - 1]).is_none());
        let mut extra = body.clone();
        extra.push(0);
        assert!(decode_config_registry_data_0x07(&extra).is_none());
        // A present entry NBT byte is not the static block/item form.
        let mut present_nbt = body.clone();
        *present_nbt.last_mut().unwrap() = 1;
        assert!(decode_config_registry_data_0x07(&present_nbt).is_none());
        // Inflated entry count beyond what the remaining bytes can hold.
        let mut inflated = Vec::new();
        inflated.push(CONFIG_REGISTRY_DATA_PACKET_ID);
        write_mc_string(&mut inflated, "minecraft:block");
        write_varint(&mut inflated, 1_000_000);
        assert!(decode_config_registry_data_0x07(&inflated).is_none());
        // Oversized entry/registry names fail at build time.
        let oversized = vec!["x".repeat(MAX_REGISTRY_DATA_NAME_LEN + 1)];
        assert_eq!(
            build_config_registry_data_0x07("minecraft:block", &oversized),
            Err("oversized Configuration Registry Data entry name")
        );
        assert_eq!(
            build_config_registry_data_0x07(&"x".repeat(MAX_REGISTRY_DATA_NAME_LEN + 1), &names),
            Err("oversized Configuration Registry Data registry name")
        );
        assert!(decode_config_registry_data_0x07(&[]).is_none());
    }
}
