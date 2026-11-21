// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later

//! Create-specific kinetic delta wire codec layered over
//! [`mod_adapter_core::neoforge_wire`].

pub use mod_adapter_core::neoforge_wire::{
    build_neoforge_frozen_registry_frames, read_mc_string, read_varint, write_mc_string,
    write_varint, CHANNEL_FROZEN_REGISTRY, CHANNEL_FROZEN_REGISTRY_SYNC_COMPLETED,
    CHANNEL_FROZEN_REGISTRY_SYNC_START,
};

use crate::factory::{DimBlockPos, KineticUpdateTag};

pub const CHANNEL_CREATE_KINETIC_SYNC: &str = "create:kinetic_sync_v1";

/// Encode a batch of changed [`KineticUpdateTag`] deltas into a compact binary
/// custom-payload body suitable for Play `0x19` broadcast to tracked observers.
pub fn encode_kinetic_update_batch(tags: &[KineticUpdateTag]) -> Vec<u8> {
    let mut out = Vec::with_capacity(4 + tags.len() * 29);
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
            pos: DimBlockPos::new(dim, x, y, z),
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
    use crate::blockstates::{core_create_registry, DEFAULT_BASE_STATE_ID};

    #[test]
    fn frozen_registry_frames_cover_all_expanded_create_states() {
        let reg = core_create_registry(DEFAULT_BASE_STATE_ID);
        let frames = build_neoforge_frozen_registry_frames(&reg);
        assert_eq!(frames.len(), 3);
        let body = &frames[1].1;
        let mut cursor = 0usize;
        assert_eq!(
            read_mc_string(body, &mut cursor, 128).as_deref(),
            Some("minecraft:block")
        );
        let count = read_varint(body, &mut cursor).unwrap() as usize;
        assert_eq!(count, reg.state_count());
    }

    #[test]
    fn kinetic_update_batch_round_trips_and_rejects_truncation() {
        let tags = vec![KineticUpdateTag {
            pos: DimBlockPos::new(3, -8, 120, 44),
            speed_rpm: 32.0,
            stress_capacity: 512.0,
            stress_load: 128.0,
            overstressed: false,
        }];
        let encoded = encode_kinetic_update_batch(&tags);
        assert_eq!(decode_kinetic_update_batch(&encoded), Some(tags));
        assert!(decode_kinetic_update_batch(&encoded[..encoded.len() - 1]).is_none());
    }
}
