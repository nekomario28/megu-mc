// SPDX-License-Identifier: LGPL-3.0-or-later

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

/// Exact NeoForge 21.1.x `RegistrySnapshot` payload carried by the
/// `neoforge:frozen_registry` custom payload. The outer body is
/// `ResourceLocation(registry_id)` followed by a VarInt-length-prefixed byte
/// array. Inside that byte array are exactly two `FriendlyByteBuf.writeMap`
/// maps in order:
///
/// 1. runtime integer id (`VarInt`) -> entry `ResourceLocation`
/// 2. alias `ResourceLocation` -> canonical entry `ResourceLocation`
///
/// `writeMap` itself is `VarInt(size)` followed by each key/value pair.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FrozenRegistryPayload {
    pub registry_id: String,
    /// Numeric runtime id -> canonical entry id. `BTreeMap` keeps encoding
    /// deterministic in ascending runtime-id order.
    pub ids: std::collections::BTreeMap<u32, String>,
    /// Alias entry id -> canonical entry id.
    pub aliases: std::collections::BTreeMap<String, String>,
}

/// Alias used by NeoForge's implementation-facing terminology.
pub type RegistrySnapshot = FrozenRegistryPayload;

impl FrozenRegistryPayload {
    /// Encode this snapshot in NeoForge 21.1.x wire order.
    pub fn encode(&self) -> Result<Vec<u8>, &'static str> {
        encode_frozen_registry_payload(self)
    }

    /// Decode a complete payload body, failing closed on trailing bytes or
    /// malformed nested maps.
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        decode_frozen_registry_payload(bytes)
    }
}

fn write_resource_location_map(
    out: &mut Vec<u8>,
    map: &std::collections::BTreeMap<String, String>,
) -> Result<(), &'static str> {
    if map.len() > i32::MAX as usize {
        return Err("too many FrozenRegistryPayload aliases");
    }
    write_varint(out, map.len() as u32);
    for (alias, canonical) in map {
        if alias.len() > MAX_REGISTRY_DATA_NAME_LEN || canonical.len() > MAX_REGISTRY_DATA_NAME_LEN {
            return Err("oversized FrozenRegistryPayload alias name");
        }
        write_mc_string(out, alias);
        write_mc_string(out, canonical);
    }
    Ok(())
}

fn write_id_map(
    out: &mut Vec<u8>,
    map: &std::collections::BTreeMap<u32, String>,
) -> Result<(), &'static str> {
    if map.len() > i32::MAX as usize {
        return Err("too many FrozenRegistryPayload ids");
    }
    write_varint(out, map.len() as u32);
    for (runtime_id, name) in map {
        if name.len() > MAX_REGISTRY_DATA_NAME_LEN {
            return Err("oversized FrozenRegistryPayload entry name");
        }
        write_varint(out, *runtime_id);
        write_mc_string(out, name);
    }
    Ok(())
}

/// Encode a NeoForge `RegistrySnapshot`/`FrozenRegistryPayload` exactly as
/// `RegistrySnapshot.STREAM_CODEC`: outer registry id plus a VarInt byte-array
/// length, then nested `ids` and `aliases` maps in that order.
pub fn encode_frozen_registry_payload(
    payload: &FrozenRegistryPayload,
) -> Result<Vec<u8>, &'static str> {
    if payload.registry_id.len() > MAX_REGISTRY_DATA_NAME_LEN {
        return Err("oversized FrozenRegistryPayload registry name");
    }
    let mut inner = Vec::new();
    write_id_map(&mut inner, &payload.ids)?;
    write_resource_location_map(&mut inner, &payload.aliases)?;
    if inner.len() > i32::MAX as usize {
        return Err("oversized FrozenRegistryPayload byte array");
    }

    let mut out = Vec::with_capacity(payload.registry_id.len() + inner.len() + 10);
    write_mc_string(&mut out, &payload.registry_id);
    write_varint(&mut out, inner.len() as u32);
    out.extend_from_slice(&inner);
    Ok(out)
}

fn read_id_map(
    bytes: &[u8],
    cursor: &mut usize,
) -> Option<std::collections::BTreeMap<u32, String>> {
    let count = read_varint(bytes, cursor)? as usize;
    // One entry is at least one VarInt key byte plus one string-length byte.
    if count > bytes.len().saturating_sub(*cursor) / 2 {
        return None;
    }
    let mut out = std::collections::BTreeMap::new();
    for _ in 0..count {
        let runtime_id = read_varint(bytes, cursor)?;
        let name = read_mc_string(bytes, cursor, MAX_REGISTRY_DATA_NAME_LEN)?;
        if out.insert(runtime_id, name).is_some() {
            return None;
        }
    }
    Some(out)
}

fn read_resource_location_map(
    bytes: &[u8],
    cursor: &mut usize,
) -> Option<std::collections::BTreeMap<String, String>> {
    let count = read_varint(bytes, cursor)? as usize;
    // One entry is at least two string-length bytes.
    if count > bytes.len().saturating_sub(*cursor) / 2 {
        return None;
    }
    let mut out = std::collections::BTreeMap::new();
    for _ in 0..count {
        let alias = read_mc_string(bytes, cursor, MAX_REGISTRY_DATA_NAME_LEN)?;
        let canonical = read_mc_string(bytes, cursor, MAX_REGISTRY_DATA_NAME_LEN)?;
        if out.insert(alias, canonical).is_some() {
            return None;
        }
    }
    Some(out)
}

/// Decode the exact NeoForge `RegistrySnapshot` custom-payload layout. Both
/// the nested byte array and outer body must end exactly at their declared
/// lengths; duplicate keys fail closed.
pub fn decode_frozen_registry_payload(bytes: &[u8]) -> Option<FrozenRegistryPayload> {
    let mut cursor = 0usize;
    let registry_id = read_mc_string(bytes, &mut cursor, MAX_REGISTRY_DATA_NAME_LEN)?;
    let inner_len = read_varint(bytes, &mut cursor)? as usize;
    let inner_end = cursor.checked_add(inner_len)?;
    let inner = bytes.get(cursor..inner_end)?;
    let mut inner_cursor = 0usize;
    let ids = read_id_map(inner, &mut inner_cursor)?;
    let aliases = read_resource_location_map(inner, &mut inner_cursor)?;
    if inner_cursor != inner.len() {
        return None;
    }
    cursor = inner_end;
    (cursor == bytes.len()).then_some(FrozenRegistryPayload {
        registry_id,
        ids,
        aliases,
    })
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

    let snapshot = FrozenRegistryPayload {
        registry_id: "minecraft:block".to_string(),
        ids: registry
            .states()
            .iter()
            .map(|entry| (entry.state_id, entry.canonical.clone()))
            .collect(),
        aliases: std::collections::BTreeMap::new(),
    };
    let snap_body = snapshot.encode().unwrap_or_default();
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
