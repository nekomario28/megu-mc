// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! Decoder and lifecycle store for the live Create kinetic sync channel.

use std::collections::BTreeMap;

use crate::protocol::WireReader;

pub const CHANNEL_CREATE_KINETIC_SYNC: &str = "create:kinetic_sync_v1";
pub const CHANNEL_CREATE_KINETIC_UPDATE: &str = "create:kinetic_update_v1";
const MAX_RECORDS: usize = 4096;

#[derive(Clone, Debug, PartialEq)]
pub struct KineticSyncRecord {
    pub dim: i32,
    pub x: i32,
    pub y: i32,
    pub z: i32,
    pub speed_rpm: f32,
    pub stress_capacity: f32,
    pub stress_load: f32,
    pub overstressed: bool,
}

impl KineticSyncRecord {
    pub fn position(&self) -> (i32, i32, i32) {
        (self.x, self.y, self.z)
    }
}

/// Decode the payload bytes following the custom channel identifier.
pub fn decode_kinetic_sync_batch(bytes: &[u8]) -> Result<Vec<KineticSyncRecord>, String> {
    let mut reader = WireReader::new(bytes);
    let count = usize::try_from(reader.read_varint()?)
        .map_err(|_| "negative kinetic record count".to_string())?;
    if count > MAX_RECORDS {
        return Err(format!(
            "kinetic record count {count} exceeds {MAX_RECORDS}"
        ));
    }
    if reader.remaining() != count.saturating_mul(29) {
        return Err("kinetic payload length does not match record count".into());
    }
    let mut records = Vec::with_capacity(count);
    for _ in 0..count {
        let dim = reader.read_i32()?;
        let x = reader.read_i32()?;
        let y = reader.read_i32()?;
        let z = reader.read_i32()?;
        let speed_rpm = reader.read_f32()?;
        let stress_capacity = reader.read_f32()?;
        let stress_load = reader.read_f32()?;
        let overstressed = match reader.read_u8()? {
            0 => false,
            1 => true,
            _ => return Err("invalid kinetic overstressed flag".into()),
        };
        if !speed_rpm.is_finite() {
            return Err("non-finite kinetic speed".into());
        }
        records.push(KineticSyncRecord {
            dim,
            x,
            y,
            z,
            speed_rpm,
            stress_capacity,
            stress_load,
            overstressed,
        });
    }
    if reader.remaining() != 0 {
        return Err("trailing bytes in kinetic payload".into());
    }
    Ok(records)
}

#[derive(Clone, Debug, Default)]
pub struct KineticSyncStore {
    live: BTreeMap<(i32, i32, i32), KineticSyncRecord>,
    pending_updates: Vec<KineticSyncRecord>,
    pending_removals: Vec<(i32, i32, i32)>,
}

impl KineticSyncStore {
    pub fn apply_batch(&mut self, records: Vec<KineticSyncRecord>) {
        for record in records {
            let pos = record.position();
            self.live.insert(pos, record.clone());
            self.pending_removals.retain(|removed| *removed != pos);
            self.pending_updates.push(record);
        }
    }

    pub fn at(&self, x: i32, y: i32, z: i32) -> Option<&KineticSyncRecord> {
        self.live.get(&(x, y, z))
    }

    pub fn drain_updates(&mut self) -> Vec<KineticSyncRecord> {
        std::mem::take(&mut self.pending_updates)
    }

    pub fn drain_removals(&mut self) -> Vec<(i32, i32, i32)> {
        std::mem::take(&mut self.pending_removals)
    }

    pub fn evict(&mut self, pos: (i32, i32, i32)) {
        if self.live.remove(&pos).is_some() {
            self.pending_updates
                .retain(|record| record.position() != pos);
            if !self.pending_removals.contains(&pos) {
                self.pending_removals.push(pos);
            }
        }
    }

    pub fn evict_chunk(&mut self, cx: i32, cz: i32) {
        let positions: Vec<_> = self
            .live
            .keys()
            .copied()
            .filter(|(x, _, z)| x.div_euclid(16) == cx && z.div_euclid(16) == cz)
            .collect();
        for pos in positions {
            self.evict(pos);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record() -> KineticSyncRecord {
        KineticSyncRecord {
            dim: -1,
            x: 12,
            y: 64,
            z: -7,
            speed_rpm: -32.5,
            stress_capacity: 256.0,
            stress_load: 72.25,
            overstressed: true,
        }
    }

    fn encoded(record: &KineticSyncRecord) -> Vec<u8> {
        let mut bytes = vec![1];
        for value in [record.dim, record.x, record.y, record.z] {
            bytes.extend_from_slice(&value.to_be_bytes());
        }
        for value in [record.speed_rpm, record.stress_capacity, record.stress_load] {
            bytes.extend_from_slice(&value.to_be_bytes());
        }
        bytes.push(u8::from(record.overstressed));
        bytes
    }

    #[test]
    fn decodes_exact_29_byte_record() {
        let expected = record();
        let bytes = encoded(&expected);
        assert_eq!(bytes.len(), 30);
        assert_eq!(decode_kinetic_sync_batch(&bytes).unwrap(), vec![expected]);
    }

    #[test]
    fn rejects_non_finite_truncated_trailing_and_oversized_batches() {
        let mut bytes = encoded(&record());
        bytes[17..21].copy_from_slice(&f32::NAN.to_be_bytes());
        assert!(decode_kinetic_sync_batch(&bytes).is_err());
        let valid = encoded(&record());
        assert!(decode_kinetic_sync_batch(&valid[..valid.len() - 1]).is_err());
        let mut trailing = valid;
        trailing.push(0);
        assert!(decode_kinetic_sync_batch(&trailing).is_err());
        assert!(decode_kinetic_sync_batch(&[0x81, 0x20]).is_err());
    }

    #[test]
    fn evicts_broken_blocks_and_unloaded_chunks() {
        let mut store = KineticSyncStore::default();
        store.apply_batch(vec![record()]);
        assert!(store.at(12, 64, -7).is_some());
        store.evict((12, 64, -7));
        assert!(store.at(12, 64, -7).is_none());
        assert_eq!(store.drain_removals(), vec![(12, 64, -7)]);

        let mut second = record();
        second.x = 33;
        second.z = 17;
        store.apply_batch(vec![record(), second]);
        store.evict_chunk(0, -1);
        assert!(store.at(12, 64, -7).is_none());
        assert!(store.at(33, 64, 17).is_some());
    }
}
