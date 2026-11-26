// SPDX-License-Identifier: LGPL-3.0-or-later
//! Decoder and lifecycle store for the live Create kinetic sync channel.

use std::collections::BTreeMap;

use crate::protocol::WireReader;

pub const CHANNEL_CREATE_KINETIC_SYNC: &str = "create:kinetic_sync_v1";
pub const CHANNEL_CREATE_KINETIC_UPDATE: &str = "create:kinetic_update_v1";
const MAX_RECORDS: usize = 4096;
const MAX_PENDING_SNAPSHOTS: usize = 64;
pub const KINETIC_BATCH_MAGIC: [u8; 4] = *b"KNT1";
pub const KINETIC_BATCH_VERSION: u8 = 1;

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

    pub fn dimensioned_position(&self) -> (i32, (i32, i32, i32)) {
        (self.dim, self.position())
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct KineticSyncChunk {
    pub dimension_id: i32,
    pub snapshot_generation: u64,
    pub chunk_index: u32,
    pub chunk_total: u32,
    pub records: Vec<KineticSyncRecord>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum KineticSyncPayload {
    /// Backwards-compatible VarInt-counted delta batch.
    LegacyBatch(Vec<KineticSyncRecord>),
    /// Dimension-scoped snapshot chunk carrying generation and chunk metadata.
    SnapshotChunk(KineticSyncChunk),
}

#[derive(Clone, Debug, PartialEq)]
pub struct KineticSnapshot {
    pub dimension_id: i32,
    pub snapshot_generation: u64,
    pub records: Vec<KineticSyncRecord>,
}

fn decode_records(
    reader: &mut WireReader<'_>,
    count: usize,
) -> Result<Vec<KineticSyncRecord>, String> {
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
    Ok(records)
}

fn decode_legacy_batch(bytes: &[u8]) -> Result<Vec<KineticSyncRecord>, String> {
    let mut reader = WireReader::new(bytes);
    let count = usize::try_from(reader.read_varint()?)
        .map_err(|_| "negative kinetic record count".to_string())?;
    if count > MAX_RECORDS {
        return Err(format!(
            "kinetic record count {count} exceeds {MAX_RECORDS}"
        ));
    }
    let records = decode_records(&mut reader, count)?;
    if reader.remaining() != 0 {
        return Err("trailing bytes in kinetic payload".into());
    }
    Ok(records)
}

fn decode_chunked_batch(bytes: &[u8]) -> Result<KineticSyncChunk, String> {
    let mut reader = WireReader::new(bytes);
    if reader.take(4)? != KINETIC_BATCH_MAGIC {
        return Err("invalid kinetic chunk magic".into());
    }
    if reader.read_u8()? != KINETIC_BATCH_VERSION {
        return Err("unsupported kinetic chunk version".into());
    }
    let dimension_id = reader.read_i32()?;
    let snapshot_generation = reader.read_u64()?;
    let chunk_index = reader.read_i32()? as u32;
    let chunk_total = reader.read_i32()? as u32;
    if chunk_total == 0 || chunk_index >= chunk_total || chunk_total > 4096 {
        return Err("invalid kinetic chunk index".into());
    }
    let count = usize::try_from(reader.read_varint()?)
        .map_err(|_| "negative kinetic record count".to_string())?;
    if count > MAX_RECORDS {
        return Err(format!(
            "kinetic record count {count} exceeds {MAX_RECORDS}"
        ));
    }
    let records = decode_records(&mut reader, count)?;
    if reader.remaining() != 0 {
        return Err("trailing bytes in kinetic chunk".into());
    }
    if records.iter().any(|record| record.dim != dimension_id) {
        return Err("kinetic chunk dimension mismatch".into());
    }
    Ok(KineticSyncChunk {
        dimension_id,
        snapshot_generation,
        chunk_index,
        chunk_total,
        records,
    })
}

/// Decode either the legacy counted batch or one dimension-scoped `KNT1`
/// snapshot chunk, preserving chunk metadata for callers that reassemble
/// large joining snapshots.
pub fn decode_kinetic_sync_payload(bytes: &[u8]) -> Result<KineticSyncPayload, String> {
    if bytes.get(..4) == Some(&KINETIC_BATCH_MAGIC) {
        decode_chunked_batch(bytes).map(KineticSyncPayload::SnapshotChunk)
    } else {
        decode_legacy_batch(bytes).map(KineticSyncPayload::LegacyBatch)
    }
}

/// Decode the payload bytes following the custom channel identifier. Both
/// the legacy batch and one chunk of a multi-chunk snapshot return their
/// records here, so existing delta consumers remain compatible; callers that
/// need snapshot completeness use [`KineticSyncReassembler`].
pub fn decode_kinetic_sync_batch(bytes: &[u8]) -> Result<Vec<KineticSyncRecord>, String> {
    match decode_kinetic_sync_payload(bytes)? {
        KineticSyncPayload::LegacyBatch(records) => Ok(records),
        KineticSyncPayload::SnapshotChunk(chunk) => Ok(chunk.records),
    }
}

type PendingChunks = BTreeMap<u32, (u32, Vec<KineticSyncRecord>)>;

/// Reassembles `(snapshot_generation, dimension_id)` chunks without mixing
/// dimensions or generations. Chunks may arrive out of order; duplicates,
/// inconsistent totals and oversized pending state fail closed.
#[derive(Clone, Debug, Default)]
pub struct KineticSyncReassembler {
    pending: BTreeMap<(u64, i32), PendingChunks>,
}

impl KineticSyncReassembler {
    pub fn ingest(&mut self, bytes: &[u8]) -> Result<Option<KineticSnapshot>, String> {
        let KineticSyncPayload::SnapshotChunk(chunk) = decode_kinetic_sync_payload(bytes)? else {
            return Ok(None);
        };
        self.ingest_chunk(chunk)
    }

    pub fn ingest_chunk(
        &mut self,
        chunk: KineticSyncChunk,
    ) -> Result<Option<KineticSnapshot>, String> {
        let key = (chunk.snapshot_generation, chunk.dimension_id);
        if !self.pending.contains_key(&key) && self.pending.len() >= MAX_PENDING_SNAPSHOTS {
            return Err("too many pending kinetic snapshots".into());
        }
        let chunks = self.pending.entry(key).or_default();
        if chunks
            .values()
            .next()
            .is_some_and(|(total, _)| *total != chunk.chunk_total)
        {
            return Err("inconsistent kinetic chunk total".into());
        }
        if chunks.contains_key(&chunk.chunk_index) {
            return Err("duplicate kinetic snapshot chunk".into());
        }
        chunks.insert(chunk.chunk_index, (chunk.chunk_total, chunk.records));
        let chunks = self.pending.get(&key).expect("entry inserted");
        if chunks.len() != chunk.chunk_total as usize {
            return Ok(None);
        }
        let mut records = Vec::new();
        for index in 0..chunk.chunk_total {
            let Some((total, part)) = chunks.get(&index) else {
                return Ok(None);
            };
            if *total != chunk.chunk_total {
                return Err("inconsistent kinetic chunk total".into());
            }
            records.extend(part.iter().cloned());
        }
        self.pending.remove(&key);
        Ok(Some(KineticSnapshot {
            dimension_id: chunk.dimension_id,
            snapshot_generation: chunk.snapshot_generation,
            records,
        }))
    }

    pub fn pending_len(&self) -> usize {
        self.pending.len()
    }
}

type DimensionedPos = (i32, (i32, i32, i32));
type StoreKey = (i32, i32, i32, i32);

fn store_key(dim: i32, pos: (i32, i32, i32)) -> StoreKey {
    (dim, pos.0, pos.1, pos.2)
}

/// Lifecycle store keyed by `(dimension_id, pos)`. The legacy methods that
/// omit a dimension deliberately operate on Overworld dimension `0`; modded
/// dimensions use the explicit `_in_dimension` forms.
#[derive(Clone, Debug, Default)]
pub struct KineticSyncStore {
    live: BTreeMap<StoreKey, KineticSyncRecord>,
    pending_updates: Vec<KineticSyncRecord>,
    pending_removals: Vec<DimensionedPos>,
}

impl KineticSyncStore {
    pub fn apply_batch(&mut self, records: Vec<KineticSyncRecord>) {
        for record in records {
            let key = store_key(record.dim, record.position());
            self.live.insert(key, record.clone());
            self.pending_removals
                .retain(|removed| store_key(removed.0, removed.1) != key);
            self.pending_updates.push(record);
        }
    }

    pub fn apply_snapshot(&mut self, snapshot: KineticSnapshot) {
        self.live
            .retain(|(dim, _, _, _), _| *dim != snapshot.dimension_id);
        self.apply_batch(snapshot.records);
    }

    pub fn at(&self, x: i32, y: i32, z: i32) -> Option<&KineticSyncRecord> {
        self.at_in_dimension(0, x, y, z)
    }

    pub fn at_in_dimension(&self, dim: i32, x: i32, y: i32, z: i32) -> Option<&KineticSyncRecord> {
        self.live.get(&store_key(dim, (x, y, z)))
    }

    pub fn drain_updates(&mut self) -> Vec<KineticSyncRecord> {
        std::mem::take(&mut self.pending_updates)
    }

    /// Drain removals visible to legacy Overworld-only callers. Dimensioned
    /// callers should use [`Self::drain_dimensioned_removals`].
    pub fn drain_removals(&mut self) -> Vec<(i32, i32, i32)> {
        let mut legacy = Vec::new();
        let mut remaining = Vec::new();
        for removal in std::mem::take(&mut self.pending_removals) {
            if removal.0 == 0 {
                legacy.push(removal.1);
            } else {
                remaining.push(removal);
            }
        }
        self.pending_removals = remaining;
        legacy
    }

    pub fn drain_dimensioned_removals(&mut self) -> Vec<DimensionedPos> {
        std::mem::take(&mut self.pending_removals)
    }

    pub fn evict(&mut self, pos: (i32, i32, i32)) {
        self.evict_in_dimension(0, pos);
    }

    pub fn evict_in_dimension(&mut self, dim: i32, pos: (i32, i32, i32)) {
        let key = store_key(dim, pos);
        if self.live.remove(&key).is_some() {
            self.pending_updates
                .retain(|record| store_key(record.dim, record.position()) != key);
            let removal = (dim, pos);
            if !self.pending_removals.contains(&removal) {
                self.pending_removals.push(removal);
            }
        }
    }

    pub fn evict_chunk(&mut self, cx: i32, cz: i32) {
        self.evict_chunk_in_dimension(0, cx, cz);
    }

    pub fn evict_chunk_in_dimension(&mut self, dim: i32, cx: i32, cz: i32) {
        let positions: Vec<_> = self
            .live
            .keys()
            .copied()
            .filter(|(record_dim, x, _, z)| {
                *record_dim == dim && x.div_euclid(16) == cx && z.div_euclid(16) == cz
            })
            .map(|(_, x, y, z)| (x, y, z))
            .collect();
        for pos in positions {
            self.evict_in_dimension(dim, pos);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record() -> KineticSyncRecord {
        KineticSyncRecord {
            dim: 0,
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

    fn encoded_chunk(
        dim: i32,
        generation: u64,
        index: u32,
        total: u32,
        records: &[KineticSyncRecord],
    ) -> Vec<u8> {
        let mut bytes = KINETIC_BATCH_MAGIC.to_vec();
        bytes.push(KINETIC_BATCH_VERSION);
        bytes.extend_from_slice(&dim.to_be_bytes());
        bytes.extend_from_slice(&generation.to_be_bytes());
        bytes.extend_from_slice(&index.to_be_bytes());
        bytes.extend_from_slice(&total.to_be_bytes());
        bytes.push(records.len() as u8);
        for record in records {
            for value in [record.dim, record.x, record.y, record.z] {
                bytes.extend_from_slice(&value.to_be_bytes());
            }
            for value in [record.speed_rpm, record.stress_capacity, record.stress_load] {
                bytes.extend_from_slice(&value.to_be_bytes());
            }
            bytes.push(u8::from(record.overstressed));
        }
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
    fn chunked_snapshot_decodes_and_reassembles_out_of_order() {
        let mut first = record();
        first.dim = 3;
        let mut second = record();
        second.dim = 3;
        second.x = 13;
        let chunk0 = encoded_chunk(3, 77, 0, 2, &[first.clone()]);
        let chunk1 = encoded_chunk(3, 77, 1, 2, &[second.clone()]);
        assert_eq!(
            decode_kinetic_sync_batch(&chunk0).unwrap(),
            vec![first.clone()]
        );

        let mut reassembler = KineticSyncReassembler::default();
        assert!(reassembler.ingest(&chunk1).unwrap().is_none());
        let snapshot = reassembler
            .ingest(&chunk0)
            .unwrap()
            .expect("complete snapshot");
        assert_eq!(snapshot.dimension_id, 3);
        assert_eq!(snapshot.snapshot_generation, 77);
        assert_eq!(snapshot.records, vec![first, second]);
    }

    #[test]
    fn store_keys_same_position_by_dimension_and_legacy_defaults_to_overworld() {
        let overworld = record();
        let mut other = record();
        other.dim = 3;
        other.speed_rpm = 99.0;
        let mut store = KineticSyncStore::default();
        store.apply_batch(vec![overworld.clone(), other.clone()]);
        assert_eq!(store.at(12, 64, -7), Some(&overworld));
        assert_eq!(store.at_in_dimension(3, 12, 64, -7), Some(&other));
        store.evict_in_dimension(3, (12, 64, -7));
        assert!(store.at_in_dimension(3, 12, 64, -7).is_none());
        assert!(store.at(12, 64, -7).is_some());
        assert_eq!(store.drain_dimensioned_removals(), vec![(3, (12, 64, -7))]);
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
