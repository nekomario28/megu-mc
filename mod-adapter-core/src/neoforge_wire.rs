// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later

//! Universal Protocol 767 VarInt/string codecs and NeoForge 1.21.1
//! Configuration `frozen_registry*` synchronization frames for ANY mod's
//! [`BlockRegistry`].

use crate::blockstates::BlockRegistry;

pub const CHANNEL_FROZEN_REGISTRY_SYNC_START: &str = "neoforge:frozen_registry_sync_start";
pub const CHANNEL_FROZEN_REGISTRY: &str = "neoforge:frozen_registry";
pub const CHANNEL_FROZEN_REGISTRY_SYNC_COMPLETED: &str = "neoforge:frozen_registry_sync_completed";

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
