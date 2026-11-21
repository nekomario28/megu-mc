// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later

//! Pure-Rust NeoForge 1.21.1 Configuration registry sync payloads and Create
//! BlockEntity kinetic UpdateTag wire encoders, ready to route through
//! `mc-spine`'s Protocol 767 Configuration (`0x01`/`0x02`) and Play (`0x07`/`0x19`)
//! handlers without JVM execution.

use crate::blockstates::CreateBlockRegistry;
use crate::factory::KineticUpdateTag;

/// NeoForge 1.21.1 Configuration custom payload channels.
pub const CHANNEL_FROZEN_REGISTRY_SYNC_START: &str = "neoforge:frozen_registry_sync_start";
pub const CHANNEL_FROZEN_REGISTRY: &str = "neoforge:frozen_registry";
pub const CHANNEL_FROZEN_REGISTRY_SYNC_COMPLETED: &str = "neoforge:frozen_registry_sync_completed";
pub const CHANNEL_CREATE_KINETIC_SYNC: &str = "create:kinetic_sync_v1";

/// Protocol 767 VarInt encoder.
pub fn write_varint(out: &mut Vec<u8>, mut value: u32) {
    loop {
        let mut byte = (value & 0x7F) as u8;
        value >>= 7;
        if value != 0 {
            byte |= 0x80;
        }
        out.push(byte);
        if value == 0 {
            break;
        }
    }
}

/// Protocol 767 VarInt decoder with strict 5-byte bound.
pub fn read_varint(bytes: &[u8], cursor: &mut usize) -> Option<u32> {
    let mut result = 0u32;
    let mut shift = 0u32;
    for _ in 0..5 {
        let byte = *bytes.get(*cursor)?;
        *cursor += 1;
        result |= u32::from(byte & 0x7F) << shift;
        if byte & 0x80 == 0 {
            return Some(result);
        }
        shift += 7;
    }
    None
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

/// Build the ordered list of NeoForge Configuration `0x01` Custom Payload
/// `(channel, payload)` frames required to synchronize a frozen
/// [`CreateBlockRegistry`] with an unmodified NeoForge 1.21.1 client.
pub fn build_neoforge_frozen_registry_frames(
    registry: &CreateBlockRegistry,
) -> Vec<(String, Vec<u8>)> {
    let mut frames = Vec::with_capacity(3);

    // 1. neoforge:frozen_registry_sync_start -> list of registry names to sync.
    let mut start_body = Vec::new();
    write_varint(&mut start_body, 1);
    write_mc_string(&mut start_body, "minecraft:block");
    frames.push((CHANNEL_FROZEN_REGISTRY_SYNC_START.to_string(), start_body));

    // 2. neoforge:frozen_registry -> snapshot of canonical block IDs + base/state count.
    let mut snap_body = Vec::new();
    write_mc_string(&mut snap_body, "minecraft:block");
    write_varint(&mut snap_body, registry.state_count() as u32);
    for offset in 0..registry.state_count() {
        let state_id = registry.base_state_id() + offset as u32;
        let entry = registry.state(state_id).expect("contiguous state table");
        write_varint(&mut snap_body, state_id);
        write_mc_string(&mut snap_body, &entry.canonical);
    }
    frames.push((CHANNEL_FROZEN_REGISTRY.to_string(), snap_body));

    // 3. neoforge:frozen_registry_sync_completed -> empty completion marker.
    frames.push((
        CHANNEL_FROZEN_REGISTRY_SYNC_COMPLETED.to_string(),
        Vec::new(),
    ));
    frames
}

/// Encode a batch of changed [`KineticUpdateTag`] deltas into a compact binary
/// custom-payload body suitable for Play `0x19` broadcast to tracked observers.
pub fn encode_kinetic_update_batch(tags: &[KineticUpdateTag]) -> Vec<u8> {
    let mut out = Vec::with_capacity(4 + tags.len() * 41);
    write_varint(&mut out, tags.len() as u32);
    for tag in tags {
        out.extend_from_slice(&tag.pos.dim.to_be_bytes());
        out.extend_from_slice(&tag.pos.x.to_be_bytes());
        out.extend_from_slice(&tag.pos.y.to_be_bytes());
        out.extend_from_slice(&tag.pos.z.to_be_bytes());
        out.extend_from_slice(&(tag.speed_rpm as f32).to_be_bytes());
        out.extend_from_slice(&(tag.stress_capacity as f32).to_be_bytes());
        out.extend_from_slice(&(tag.stress_load as f32).to_be_bytes());
        out.push(u8::from(tag.overstressed));
    }
    out
}

/// Decode a batch written by [`encode_kinetic_update_batch`].
pub fn decode_kinetic_update_batch(bytes: &[u8]) -> Option<Vec<KineticUpdateTag>> {
    let mut cursor = 0usize;
    let count = read_varint(bytes, &mut cursor)? as usize;
    if count > 4096 {
        return None;
    }
    let mut out = Vec::with_capacity(count);
    for _ in 0..count {
        if cursor + 29 > bytes.len() {
            return None;
        }
        let dim = i32::from_be_bytes(bytes[cursor..cursor + 4].try_into().ok()?);
        let x = i32::from_be_bytes(bytes[cursor + 4..cursor + 8].try_into().ok()?);
        let y = i32::from_be_bytes(bytes[cursor + 8..cursor + 12].try_into().ok()?);
        let z = i32::from_be_bytes(bytes[cursor + 12..cursor + 16].try_into().ok()?);
        let speed = f32::from_be_bytes(bytes[cursor + 16..cursor + 20].try_into().ok()?);
        let cap = f32::from_be_bytes(bytes[cursor + 20..cursor + 24].try_into().ok()?);
        let load = f32::from_be_bytes(bytes[cursor + 24..cursor + 28].try_into().ok()?);
        let overstressed = match bytes[cursor + 28] {
            0 => false,
            1 => true,
            _ => return None,
        };
        cursor += 29;
        out.push(KineticUpdateTag {
            pos: crate::factory::DimBlockPos::new(dim, x, y, z),
            speed_rpm: f64::from(speed),
            stress_capacity: f64::from(cap),
            stress_load: f64::from(load),
            overstressed,
        });
    }
    (cursor == bytes.len()).then_some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::blockstates::DEFAULT_BASE_STATE_ID;
    use crate::factory::DimBlockPos;

    #[test]
    fn frozen_registry_frames_cover_all_expanded_create_states() {
        let reg = CreateBlockRegistry::core_create(DEFAULT_BASE_STATE_ID);
        let frames = build_neoforge_frozen_registry_frames(&reg);
        assert_eq!(frames.len(), 3);
        assert_eq!(frames[0].0, CHANNEL_FROZEN_REGISTRY_SYNC_START);
        assert_eq!(frames[1].0, CHANNEL_FROZEN_REGISTRY);
        assert_eq!(frames[2].0, CHANNEL_FROZEN_REGISTRY_SYNC_COMPLETED);

        let body = &frames[1].1;
        let mut cursor = 0usize;
        assert_eq!(
            read_mc_string(body, &mut cursor, 128).as_deref(),
            Some("minecraft:block")
        );
        let count = read_varint(body, &mut cursor).unwrap() as usize;
        assert_eq!(count, reg.state_count());
        let first_id = read_varint(body, &mut cursor).unwrap();
        assert_eq!(first_id, DEFAULT_BASE_STATE_ID);
        let first_name = read_mc_string(body, &mut cursor, 256).unwrap();
        assert_eq!(reg.decode_canonical(&first_name), Some(first_id));
    }

    #[test]
    fn kinetic_update_batch_round_trips_and_rejects_truncation() {
        let tags = vec![
            KineticUpdateTag {
                pos: DimBlockPos::new(0, 12, 64, -5),
                speed_rpm: 32.0,
                stress_capacity: 512.0,
                stress_load: 128.0,
                overstressed: false,
            },
            KineticUpdateTag {
                pos: DimBlockPos::new(3, -8, 120, 44),
                speed_rpm: 0.0,
                stress_capacity: 256.0,
                stress_load: 384.0,
                overstressed: true,
            },
        ];
        let encoded = encode_kinetic_update_batch(&tags);
        let decoded = decode_kinetic_update_batch(&encoded).unwrap();
        assert_eq!(decoded, tags);
        assert!(decode_kinetic_update_batch(&encoded[..encoded.len() - 1]).is_none());
    }
}
