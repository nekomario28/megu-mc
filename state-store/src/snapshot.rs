// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! Deterministic binary snapshot of a [`ServerContext`]'s chunk overlays.
//!
//! The `STV1` codec freezes the per-chunk write-side state (block overrides,
//! block-entity payloads and the overlay `revision`) into a byte string that a
//! later [`decode_snapshot`] restores into a fresh [`ServerContext`]. It is a
//! transport/checkpoint format, not a world format: entities and scheduled
//! ticks are runtime state, so they are not part of the record.
//!
//! Wire layout — every integer is big-endian (the same network order the
//! protocol uses), every count and string length is a `u32`:
//!
//! ```text
//!   magic          : 4 bytes "STV1"
//!   protocol       : i32        (TargetVersion::protocol)
//!   data_version   : i32        (TargetVersion::data_version — decode's key)
//!   mc_version     : u32 len + UTF-8 id ("1.21.1" / "26.3")
//!   dimension_count: u32
//!   per dimension, sorted by name:
//!     name           : u32 len + UTF-8
//!     chunk_count    : u32
//!     per chunk, sorted by (cx, cz):
//!       cx, cz          : i32, i32
//!       revision        : u64
//!       block_count     : u32
//!       per block, sorted by (lx, y, lz)  : lx u8, y i32, lz u8, state u32
//!       block_entities  : u32
//!       per entity, sorted by (lx, y, lz) : lx u8, y i32, lz u8,
//!                                         payload u32 len + bytes
//!   trailer: u32 CRC32 (IEEE 802.3, reflected) over every byte above
//! ```
//!
//! Determinism is a contract: `HashMap` iteration order never reaches the
//! wire — dimensions, chunks, blocks and block entities are all sorted before
//! encoding, so the same `ServerContext` always produces byte-identical
//! output. Decoding fails closed: bad magic, a CRC mismatch, truncation, a
//! `data_version` outside the pinned target set, an inconsistent
//! protocol/data-version pair or any count past the safe caps is an error,
//! never a partial restore or a panic.

use crate::sharded::{ChunkCoord, ChunkOverlay, DimensionState, ServerContext};
use megu_core::version::TargetVersion;
use std::collections::HashMap;
use std::error::Error;
use std::fmt;
use std::sync::Arc;

/// File magic: `S`napshot `T`arget-`V`ersioned, revision 1.
const MAGIC: &[u8; 4] = b"STV1";

// Fail-closed caps. A count larger than its cap is refused before a single
// element is read, so a corrupt stream can never drive an unbounded loop or
// allocation. The caps sit far above any real overlay content — they exist to
// reject garbage, not to bound legitimate saves.
/// More dimensions than a server can plausibly register (vanilla ships 3).
const MAX_DIMENSIONS: u32 = 4_096;
/// Chunk overlays per dimension — far past any real touched-chunk count.
const MAX_CHUNKS: u32 = 4_000_000;
/// Block overrides per chunk: `16 * 16 * 4096` in-chunk positions worth.
const MAX_BLOCK_OVERRIDES: u32 = 1_048_576;
/// Block-entity payloads per chunk.
const MAX_BLOCK_ENTITIES: u32 = 65_536;
/// One block-entity payload (a megabyte is already absurd for a chest NBT).
const MAX_PAYLOAD_LEN: u32 = 1_048_576;
/// Dimension-name byte length.
const MAX_NAME_LEN: u32 = 1_024;
/// `mc_version` string byte length.
const MAX_VERSION_LEN: u32 = 256;

/// Why a byte string is not a valid `STV1` snapshot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SnapshotError {
    /// The first four bytes are not `b"STV1"`.
    BadMagic,
    /// The stream ends before the record being read completes.
    Truncated,
    /// The trailing CRC32 does not match the header and body bytes.
    CrcMismatch { expected: u32, actual: u32 },
    /// `data_version` is not one [`TargetVersion::from_data_version`] knows —
    /// the snapshot names a target this build has no constants for.
    UnknownDataVersion(i32),
    /// `data_version` resolves to a target whose pinned protocol disagrees
    /// with the stream's `protocol_version`: the header contradicts itself.
    InconsistentHeader {
        data_version: i32,
        protocol_version: i32,
    },
    /// A count field exceeds the codec's safe cap.
    OversizedCount {
        field: &'static str,
        count: u32,
        cap: u32,
    },
    /// A length-prefixed byte field exceeds its cap.
    OversizedLength {
        field: &'static str,
        len: u32,
        cap: u32,
    },
    /// A string field is not valid UTF-8.
    InvalidUtf8(&'static str),
}

impl fmt::Display for SnapshotError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SnapshotError::BadMagic => write!(f, "not an STV1 snapshot (bad magic)"),
            SnapshotError::Truncated => write!(f, "snapshot ends mid-record"),
            SnapshotError::CrcMismatch { expected, actual } => write!(
                f,
                "snapshot CRC32 mismatch: stored {expected:#010x}, computed {actual:#010x}"
            ),
            SnapshotError::UnknownDataVersion(dv) => {
                write!(f, "snapshot DataVersion {dv} is not a pinned target")
            }
            SnapshotError::InconsistentHeader {
                data_version,
                protocol_version,
            } => write!(
                f,
                "snapshot DataVersion {data_version} does not match protocol {protocol_version}"
            ),
            SnapshotError::OversizedCount { field, count, cap } => {
                write!(f, "snapshot {field} count {count} exceeds cap {cap}")
            }
            SnapshotError::OversizedLength { field, len, cap } => {
                write!(f, "snapshot {field} length {len} exceeds cap {cap}")
            }
            SnapshotError::InvalidUtf8(field) => {
                write!(f, "snapshot {field} is not valid UTF-8")
            }
        }
    }
}

impl Error for SnapshotError {}

// ------------------------------------------------------------------ CRC32

/// IEEE 802.3 reflected CRC32 (the Ethernet/`zlib` polynomial), built at
/// compile time — pure `std`, no table crate.
const fn crc32_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 {
                0xEDB8_8320 ^ (c >> 1)
            } else {
                c >> 1
            };
            k += 1;
        }
        table[i] = c;
        i += 1;
    }
    table
}

static CRC32_TABLE: [u32; 256] = crc32_table();

fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &b in bytes {
        crc = CRC32_TABLE[((crc ^ u32::from(b)) & 0xff) as usize] ^ (crc >> 8);
    }
    !crc
}

// ----------------------------------------------------------------- encode

fn put_u32(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_be_bytes());
}

fn put_i32(out: &mut Vec<u8>, v: i32) {
    out.extend_from_slice(&v.to_be_bytes());
}

fn put_u64(out: &mut Vec<u8>, v: u64) {
    out.extend_from_slice(&v.to_be_bytes());
}

/// `u32` length prefix + raw bytes.
fn put_bytes(out: &mut Vec<u8>, bytes: &[u8]) {
    put_u32(
        out,
        u32::try_from(bytes.len()).expect("snapshot field longer than u32::MAX"),
    );
    out.extend_from_slice(bytes);
}

fn put_count(out: &mut Vec<u8>, n: usize) {
    put_u32(
        out,
        u32::try_from(n).expect("snapshot record count exceeds u32::MAX"),
    );
}

/// A chunk's sorted overlay record, staged before writing so `HashMap` order
/// cannot reach the wire.
struct ChunkRecord {
    coord: ChunkCoord,
    revision: u64,
    blocks: Vec<((u8, i32, u8), u32)>,
    block_entities: Vec<((u8, i32, u8), Vec<u8>)>,
}

/// Serialize `ctx`'s chunk overlays for `target` into the `STV1` format.
///
/// Deterministic: encoding the same `ServerContext` twice produces
/// byte-identical snapshots, and so does encoding two contexts built from the
/// same writes in any order.
pub fn encode_snapshot(ctx: &ServerContext, target: TargetVersion) -> Vec<u8> {
    let meta = target.meta();
    let mut out = Vec::new();
    out.extend_from_slice(MAGIC);
    put_i32(&mut out, meta.protocol);
    put_i32(&mut out, meta.data_version);
    put_bytes(&mut out, meta.id.as_bytes());

    let dims = ctx.dimensions_sorted();
    put_count(&mut out, dims.len());
    for (name, dim) in &dims {
        put_bytes(&mut out, name.as_bytes());
        let mut chunks = Vec::new();
        dim.for_each_overlay(|coord, overlay| {
            let mut blocks: Vec<((u8, i32, u8), u32)> = overlay
                .blocks()
                .iter()
                .map(|(&local, &state)| (local, state))
                .collect();
            blocks.sort_unstable();
            let mut block_entities: Vec<((u8, i32, u8), Vec<u8>)> = overlay
                .block_entities()
                .iter()
                .map(|(&local, payload)| (local, payload.clone()))
                .collect();
            block_entities.sort_unstable();
            chunks.push(ChunkRecord {
                coord,
                revision: overlay.revision(),
                blocks,
                block_entities,
            });
        });
        chunks.sort_unstable_by_key(|c| c.coord);
        put_count(&mut out, chunks.len());
        for chunk in chunks {
            put_i32(&mut out, chunk.coord.cx);
            put_i32(&mut out, chunk.coord.cz);
            put_u64(&mut out, chunk.revision);
            put_count(&mut out, chunk.blocks.len());
            for ((lx, y, lz), state) in chunk.blocks {
                out.push(lx);
                put_i32(&mut out, y);
                out.push(lz);
                put_u32(&mut out, state);
            }
            put_count(&mut out, chunk.block_entities.len());
            for ((lx, y, lz), payload) in chunk.block_entities {
                out.push(lx);
                put_i32(&mut out, y);
                out.push(lz);
                put_bytes(&mut out, &payload);
            }
        }
    }
    let crc = crc32(&out);
    put_u32(&mut out, crc);
    out
}

// ----------------------------------------------------------------- decode

struct Reader<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], SnapshotError> {
        if self.bytes.len() - self.pos < n {
            return Err(SnapshotError::Truncated);
        }
        let slice = &self.bytes[self.pos..self.pos + n];
        self.pos += n;
        Ok(slice)
    }

    fn u8(&mut self) -> Result<u8, SnapshotError> {
        Ok(self.take(1)?[0])
    }

    fn u32(&mut self) -> Result<u32, SnapshotError> {
        Ok(u32::from_be_bytes(self.take(4)?.try_into().unwrap()))
    }

    fn i32(&mut self) -> Result<i32, SnapshotError> {
        Ok(i32::from_be_bytes(self.take(4)?.try_into().unwrap()))
    }

    fn u64(&mut self) -> Result<u64, SnapshotError> {
        Ok(u64::from_be_bytes(self.take(8)?.try_into().unwrap()))
    }

    /// A `u32` count refused when it exceeds `cap`.
    fn count(&mut self, field: &'static str, cap: u32) -> Result<u32, SnapshotError> {
        let count = self.u32()?;
        if count > cap {
            return Err(SnapshotError::OversizedCount { field, count, cap });
        }
        Ok(count)
    }

    /// A `u32` length + that many bytes, refused when the length exceeds `cap`.
    fn bytes_capped(
        &mut self,
        field: &'static str,
        cap: u32,
    ) -> Result<&'a [u8], SnapshotError> {
        let len = self.u32()?;
        if len > cap {
            return Err(SnapshotError::OversizedLength { field, len, cap });
        }
        self.take(len as usize)
    }

    fn string(&mut self, field: &'static str, cap: u32) -> Result<&'a str, SnapshotError> {
        let bytes = self.bytes_capped(field, cap)?;
        std::str::from_utf8(bytes).map_err(|_| SnapshotError::InvalidUtf8(field))
    }
}

/// Restore a snapshot produced by [`encode_snapshot`].
///
/// The returned [`ServerContext`] holds every dimension and chunk overlay the
/// snapshot carried — block overrides, block-entity payloads and overlay
/// revisions included — plus the [`TargetVersion`] the snapshot's
/// `data_version` identifies. The CRC32 is verified before a single record is
/// read, and every record is bounds-checked, so a malformed or hostile byte
/// string can only produce a [`SnapshotError`], never a panic or a partially
/// restored context returned as valid.
pub fn decode_snapshot(bytes: &[u8]) -> Result<(ServerContext, TargetVersion), SnapshotError> {
    // Magic first, then the trailer CRC over everything before it: a flipped
    // bit anywhere is refused before any field is trusted.
    let body_len = bytes.len().checked_sub(4).ok_or(SnapshotError::Truncated)?;
    if body_len < MAGIC.len() {
        return Err(SnapshotError::Truncated);
    }
    let (body, trailer) = bytes.split_at(body_len);
    if body[..MAGIC.len()] != *MAGIC {
        return Err(SnapshotError::BadMagic);
    }
    let expected = u32::from_be_bytes(trailer.try_into().unwrap());
    let actual = crc32(body);
    if actual != expected {
        return Err(SnapshotError::CrcMismatch { expected, actual });
    }

    let mut r = Reader { bytes: body, pos: MAGIC.len() };
    let protocol_version = r.i32()?;
    let data_version = r.i32()?;
    // The DataVersion is the snapshot's identity: unknown versions are
    // refused, and the protocol field must agree with it, so a corrupt or
    // hand-mixed header cannot smuggle a context under the wrong target.
    let target = TargetVersion::from_data_version(data_version)
        .ok_or(SnapshotError::UnknownDataVersion(data_version))?;
    if protocol_version != target.protocol() {
        return Err(SnapshotError::InconsistentHeader {
            data_version,
            protocol_version,
        });
    }
    // `mc_version` is informational (data_version already pins the target);
    // it is still length-capped and must be real UTF-8.
    let _mc_version = r.string("mc_version", MAX_VERSION_LEN)?;

    let ctx = ServerContext::new();
    let dimension_count = r.count("dimension_count", MAX_DIMENSIONS)?;
    for _ in 0..dimension_count {
        let name = r.string("dimension_name", MAX_NAME_LEN)?.to_owned();
        let dim = Arc::new(DimensionState::new(name.as_str()));
        let chunk_count = r.count("chunk_count", MAX_CHUNKS)?;
        for _ in 0..chunk_count {
            let coord = ChunkCoord::new(r.i32()?, r.i32()?);
            let revision = r.u64()?;
            let block_count = r.count("block_count", MAX_BLOCK_OVERRIDES)?;
            let mut blocks = HashMap::new();
            for _ in 0..block_count {
                let lx = r.u8()?;
                let y = r.i32()?;
                let lz = r.u8()?;
                let state = r.u32()?;
                blocks.insert((lx, y, lz), state);
            }
            let entity_count = r.count("block_entity_count", MAX_BLOCK_ENTITIES)?;
            let mut block_entities = HashMap::new();
            for _ in 0..entity_count {
                let lx = r.u8()?;
                let y = r.i32()?;
                let lz = r.u8()?;
                let payload = r
                    .bytes_capped("block_entity_payload", MAX_PAYLOAD_LEN)?
                    .to_vec();
                block_entities.insert((lx, y, lz), payload);
            }
            dim.install_overlay(
                coord,
                ChunkOverlay::from_snapshot_parts(blocks, block_entities, revision),
            );
        }
        ctx.register_dimension(name, dim);
    }
    Ok((ctx, target))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sharded::BlockCoord;
    use std::collections::BTreeMap;

    /// The IEEE 802.3 check vector every CRC32 implementation must reproduce.
    #[test]
    fn crc32_matches_the_standard_check_vector() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(crc32(b""), 0);
        assert_eq!(crc32(b"STV1"), 0x2E76_03D4);
    }

    /// A context with the vanilla trio's two hot dimensions plus a modded
    /// one, every chunk kind populated.
    fn populated_context() -> ServerContext {
        let ctx = ServerContext::new();

        let overworld = ctx.ensure_dimension("minecraft:overworld");
        overworld.set_block(BlockCoord::new(8, 64, -8), 42);
        overworld.set_block(BlockCoord::new(9, 64, -8), 43); // same chunk, second write
        overworld.set_block(BlockCoord::new(-33, 200, 8), 7); // a second chunk
        let first = ChunkCoord::new(0, -1);
        overworld.with_chunks_write(&[first], |set| {
            set.get_mut(first)
                .unwrap()
                .set_block_entity((8, 64, 8), b"{id:\"minecraft:chest\"}".to_vec());
        });

        let nether = ctx.ensure_dimension("minecraft:the_nether");
        nether.set_block(BlockCoord::new(0, 32, 0), 5);
        nether.set_block(BlockCoord::new(0, 33, 0), 5);
        nether.set_block(BlockCoord::new(0, 34, 0), 5);

        let custom = ctx.ensure_dimension("create:brass_dimension");
        custom.set_block(BlockCoord::new(1, 2, 3), 9001);
        let coord = ChunkCoord::new(0, 0);
        custom.with_chunks_write(&[coord], |set| {
            set.get_mut(coord)
                .unwrap()
                .set_block_entity((1, 2, 3), vec![0xde, 0xad, 0xbe, 0xef]);
        });

        // Entity/scheduled-tick state is runtime-only: present here to prove
        // the snapshot ignores it, not that it carries it.
        nether.add_entity(
            1,
            crate::sharded::EntityRecord {
                kind: "minecraft:ghast".to_string(),
                pos: [0.0; 3],
                vel: [0.0; 3],
                yaw: 0.0,
                pitch: 0.0,
                chunk: ChunkCoord::new(0, 0),
            },
        );
        ctx
    }

    /// Every `(dim, chunk, local) -> state` and `(dim, chunk, local) ->
    /// payload` plus revisions, collected through the public API so the
    /// comparison itself cannot share a bug with the codec.
    fn overlay_footprint(ctx: &ServerContext) -> BTreeMap<String, Vec<String>> {
        let mut out = BTreeMap::new();
        for name in ctx.dimension_names() {
            let dim = ctx.dimension(&name).unwrap();
            let mut rows = Vec::new();
            // Chunk coords are not enumerable through the public API; the
            // test knows which chunks it wrote, but to stay honest it scans
            // the whole -4..5 range and records whatever has a revision.
            for cx in -4..5 {
                for cz in -4..5 {
                    let coord = ChunkCoord::new(cx, cz);
                    let rev = dim.overlay_revision(coord);
                    if rev == 0 {
                        continue;
                    }
                    rows.push(format!("chunk {cx},{cz} rev {rev}"));
                    dim.with_chunks_read(&[coord], |set| {
                        let overlay = set.get(coord).unwrap();
                        for (&(lx, y, lz), &state) in overlay.blocks() {
                            rows.push(format!("  block {lx},{y},{lz} = {state}"));
                        }
                        for (&(lx, y, lz), payload) in overlay.block_entities() {
                            rows.push(format!("  entity {lx},{y},{lz} = {payload:02x?}"));
                        }
                    });
                }
            }
            rows.sort();
            out.insert(name, rows);
        }
        out
    }

    #[test]
    fn roundtrip_restores_every_dimension_and_revision() {
        for target in TargetVersion::ALL {
            let original = populated_context();
            let bytes = encode_snapshot(&original, target);
            assert_eq!(&bytes[..4], b"STV1");

            let (restored, decoded_target) = decode_snapshot(&bytes).unwrap();
            assert_eq!(decoded_target, target);
            assert_eq!(restored.dimension_names(), original.dimension_names());
            assert_eq!(overlay_footprint(&restored), overlay_footprint(&original));

            // Entities do not travel in the snapshot.
            let nether = restored.dimension("minecraft:the_nether").unwrap();
            assert_eq!(nether.entity_count(), 0);

            // Re-encoding the restore is byte-identical to the first pass:
            // the format is a fixed point, not just deterministic.
            assert_eq!(encode_snapshot(&restored, target), bytes);
        }
    }

    #[test]
    fn encoding_is_byte_identical_deterministic() {
        let ctx = populated_context();
        let a = encode_snapshot(&ctx, TargetVersion::V26_3);
        let b = encode_snapshot(&ctx, TargetVersion::V26_3);
        assert_eq!(a, b);

        // A second context reaching the same state through a different write
        // order (and matching revisions, since the write *count* is what the
        // revision records) encodes identically: sorted output, not
        // insertion order.
        let other = ServerContext::new();
        let ow = other.ensure_dimension("minecraft:overworld");
        ow.set_block(BlockCoord::new(-33, 200, 8), 7);
        ow.set_block(BlockCoord::new(9, 64, -8), 43);
        ow.set_block(BlockCoord::new(8, 64, -8), 42);
        let first = ChunkCoord::new(0, -1);
        ow.with_chunks_write(&[first], |set| {
            set.get_mut(first)
                .unwrap()
                .set_block_entity((8, 64, 8), b"{id:\"minecraft:chest\"}".to_vec());
        });
        let nether = other.ensure_dimension("minecraft:the_nether");
        for y in [32, 33, 34] {
            nether.set_block(BlockCoord::new(0, y, 0), 5);
        }
        let custom = other.ensure_dimension("create:brass_dimension");
        custom.set_block(BlockCoord::new(1, 2, 3), 9001);
        custom.with_chunks_write(&[ChunkCoord::new(0, 0)], |set| {
            set.get_mut(ChunkCoord::new(0, 0))
                .unwrap()
                .set_block_entity((1, 2, 3), vec![0xde, 0xad, 0xbe, 0xef]);
        });
        assert_eq!(encode_snapshot(&other, TargetVersion::V26_3), a);
    }

    /// Hand-roll a body so CRC-correct malformed streams can be built.
    fn wrap(body: &[u8]) -> Vec<u8> {
        let mut out = body.to_vec();
        out.extend_from_slice(&crc32(&out).to_be_bytes());
        out
    }

    fn header(target: TargetVersion) -> Vec<u8> {
        let mut h = Vec::new();
        h.extend_from_slice(MAGIC);
        h.extend_from_slice(&target.protocol().to_be_bytes());
        h.extend_from_slice(&target.data_version().to_be_bytes());
        h.extend_from_slice(&(target.id().len() as u32).to_be_bytes());
        h.extend_from_slice(target.id().as_bytes());
        h
    }

    #[test]
    fn rejects_a_flipped_bit_anywhere() {
        let bytes = encode_snapshot(&populated_context(), TargetVersion::V1_21_1);
        for i in [4, 12, bytes.len() / 2, bytes.len() - 5] {
            let mut bad = bytes.clone();
            bad[i] ^= 0x01;
            assert_eq!(
                decode_snapshot(&bad).err(),
                Some(SnapshotError::CrcMismatch {
                    expected: u32::from_be_bytes(bytes[bytes.len() - 4..].try_into().unwrap()),
                    actual: crc32(&bad[..bad.len() - 4]),
                }),
                "flip at byte {i}"
            );
        }
    }

    #[test]
    fn rejects_bad_magic_and_truncation() {
        let bytes = encode_snapshot(&populated_context(), TargetVersion::V26_3);
        for i in 0..4 {
            let mut bad = bytes.clone();
            bad[i] ^= 0xff;
            // Re-seal so the CRC check cannot mask the magic check.
            let sealed = wrap(&bad[..bad.len() - 4]);
            assert_eq!(decode_snapshot(&sealed).err(), Some(SnapshotError::BadMagic));
        }
        // Every truncation point short of the full stream fails closed —
        // either mid-record or as a CRC mismatch on the trailer bytes.
        for cut in [0, 3, 4, 8, bytes.len() - 4, bytes.len() - 1] {
            let err = decode_snapshot(&bytes[..cut]).err().unwrap();
            assert!(
                matches!(
                    err,
                    SnapshotError::Truncated
                        | SnapshotError::BadMagic
                        | SnapshotError::CrcMismatch { .. }
                ),
                "cut {cut} gave {err:?}"
            );
        }
        assert!(decode_snapshot(&[]).is_err());
    }

    #[test]
    fn rejects_unknown_data_version_and_inconsistent_protocol() {
        for dv in [0, 3954, 3956, 5022, 5024, i32::MAX] {
            let mut body = Vec::new();
            body.extend_from_slice(MAGIC);
            body.extend_from_slice(&767i32.to_be_bytes());
            body.extend_from_slice(&dv.to_be_bytes());
            body.extend_from_slice(&1u32.to_be_bytes());
            body.extend_from_slice(b"x");
            body.extend_from_slice(&0u32.to_be_bytes());
            assert_eq!(
                decode_snapshot(&wrap(&body)).err(),
                Some(SnapshotError::UnknownDataVersion(dv)),
            );
        }
        // A valid data_version paired with the other target's protocol is a
        // contradiction, refused fail-closed.
        let mut body = Vec::new();
        body.extend_from_slice(MAGIC);
        body.extend_from_slice(&777i32.to_be_bytes()); // 26.3's protocol
        body.extend_from_slice(&3955i32.to_be_bytes()); // 1.21.1's dv
        body.extend_from_slice(&6u32.to_be_bytes());
        body.extend_from_slice(b"1.21.1");
        body.extend_from_slice(&0u32.to_be_bytes());
        assert_eq!(
            decode_snapshot(&wrap(&body)).err(),
            Some(SnapshotError::InconsistentHeader {
                data_version: 3955,
                protocol_version: 777,
            }),
        );
    }

    #[test]
    fn rejects_oversized_counts_before_reading_records() {
        // dimension_count past the cap, CRC-correct.
        let mut body = header(TargetVersion::V26_3);
        body.extend_from_slice(&(MAX_DIMENSIONS + 1).to_be_bytes());
        assert_eq!(
            decode_snapshot(&wrap(&body)).err(),
            Some(SnapshotError::OversizedCount {
                field: "dimension_count",
                count: MAX_DIMENSIONS + 1,
                cap: MAX_DIMENSIONS,
            }),
        );

        // chunk_count past the cap inside one dimension.
        let mut body = header(TargetVersion::V26_3);
        body.extend_from_slice(&1u32.to_be_bytes());
        body.extend_from_slice(&11u32.to_be_bytes());
        body.extend_from_slice(b"overworld_x");
        body.extend_from_slice(&(MAX_CHUNKS + 1).to_be_bytes());
        assert_eq!(
            decode_snapshot(&wrap(&body)).err(),
            Some(SnapshotError::OversizedCount {
                field: "chunk_count",
                count: MAX_CHUNKS + 1,
                cap: MAX_CHUNKS,
            }),
        );

        // A block-entity payload length past the cap.
        let mut body = header(TargetVersion::V26_3);
        body.extend_from_slice(&1u32.to_be_bytes());
        body.extend_from_slice(&11u32.to_be_bytes());
        body.extend_from_slice(b"overworld_x");
        body.extend_from_slice(&1u32.to_be_bytes()); // 1 chunk
        body.extend_from_slice(&0i32.to_be_bytes()); // cx
        body.extend_from_slice(&0i32.to_be_bytes()); // cz
        body.extend_from_slice(&0u64.to_be_bytes()); // revision
        body.extend_from_slice(&0u32.to_be_bytes()); // 0 blocks
        body.extend_from_slice(&1u32.to_be_bytes()); // 1 block entity
        body.extend_from_slice(&[0u8]); // lx
        body.extend_from_slice(&0i32.to_be_bytes()); // y
        body.extend_from_slice(&[0u8]); // lz
        body.extend_from_slice(&(MAX_PAYLOAD_LEN + 1).to_be_bytes());
        assert_eq!(
            decode_snapshot(&wrap(&body)).err(),
            Some(SnapshotError::OversizedLength {
                field: "block_entity_payload",
                len: MAX_PAYLOAD_LEN + 1,
                cap: MAX_PAYLOAD_LEN,
            }),
        );

        // A dimension name that is not UTF-8.
        let mut body = header(TargetVersion::V26_3);
        body.extend_from_slice(&1u32.to_be_bytes());
        body.extend_from_slice(&1u32.to_be_bytes());
        body.push(0xff);
        assert_eq!(
            decode_snapshot(&wrap(&body)).err(),
            Some(SnapshotError::InvalidUtf8("dimension_name")),
        );
    }

    #[test]
    fn an_empty_context_roundtrips() {
        for target in TargetVersion::ALL {
            let bytes = encode_snapshot(&ServerContext::new(), target);
            let (restored, decoded) = decode_snapshot(&bytes).unwrap();
            assert_eq!(decoded, target);
            assert!(restored.is_empty());
        }
    }

    #[test]
    fn dv_constants_come_from_the_version_registry() {
        // The fixer's DataVersions and the snapshot's identity key are the
        // same constants — a registry bump moves both, never one.
        assert_eq!(crate::datafix::DV_1_21_1, TargetVersion::V1_21_1.data_version());
        assert_eq!(crate::datafix::DV_26_3, TargetVersion::V26_3.data_version());
        assert_eq!(
            TargetVersion::from_data_version(crate::datafix::DV_1_21_1),
            Some(TargetVersion::V1_21_1)
        );
        assert_eq!(
            TargetVersion::from_data_version(crate::datafix::DV_26_3),
            Some(TargetVersion::V26_3)
        );
    }
}
