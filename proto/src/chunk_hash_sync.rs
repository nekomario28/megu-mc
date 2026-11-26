// SPDX-License-Identifier: LGPL-3.0-or-later
//! Canonical CKH1 chunk-hash synchronization wire format and sans-IO planner.
//!
//! Integers use network byte order. Chunk coordinates and edits are in strict
//! lexicographic order on the wire, so one logical message has one encoding.

use std::collections::BTreeMap;
use std::fmt;

pub const CHUNK_HASH_SYNC_CHANNEL: &str = "megu:chunk_hash_sync_v1";
pub const CKH1_MAGIC: [u8; 4] = *b"CKH1";
pub const CKH1_SCHEMA_VERSION: u16 = 1;
pub const MAX_CHUNKS_PER_HASH_FRAME: usize = 4096;
pub const MAX_DELTA_EDITS_PER_CHUNK: usize = 65_536;
pub const MAX_CKH1_FRAME_BYTES: usize = 1024 * 1024;
pub const MAX_EPOCH_LEN: usize = 128;

const HEADER_LEN: usize = 7;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Ckh1Error {
    FrameTooLarge,
    Truncated,
    BadMagic,
    UnknownSchemaVersion(u16),
    InvalidPacketKind(u8),
    InvalidValue(&'static str),
    InvalidUtf8,
    TrailingBytes,
    UnsortedOrDuplicate(&'static str),
    PostApplyHashMismatch,
    EpochMismatch,
    BaseHashMismatch,
}

impl fmt::Display for Ckh1Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::FrameTooLarge => f.write_str("CKH1 frame exceeds byte limit"),
            Self::Truncated => f.write_str("truncated CKH1 frame"),
            Self::BadMagic => f.write_str("invalid CKH1 magic"),
            Self::UnknownSchemaVersion(v) => write!(f, "unsupported CKH1 schema version {v}"),
            Self::InvalidPacketKind(v) => write!(f, "invalid CKH1 packet kind {v}"),
            Self::InvalidValue(v) => write!(f, "invalid CKH1 value: {v}"),
            Self::InvalidUtf8 => f.write_str("invalid CKH1 UTF-8"),
            Self::TrailingBytes => f.write_str("trailing bytes after CKH1 packet"),
            Self::UnsortedOrDuplicate(v) => write!(f, "unsorted or duplicate CKH1 {v}"),
            Self::PostApplyHashMismatch => f.write_str("CKH1 post-apply hash mismatch"),
            Self::EpochMismatch => f.write_str("CKH1 worldgen epoch mismatch"),
            Self::BaseHashMismatch => f.write_str("CKH1 base hash mismatch"),
        }
    }
}

impl std::error::Error for Ckh1Error {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChunkHashEntry {
    pub cx: i32,
    pub cz: i32,
    pub hash: [u8; 32],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChunkBlockDelta {
    pub section_y: i16,
    pub local_idx: u16,
    pub new_state: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FullSyncReason {
    EpochMismatch,
    BaseHashUnknown,
    DeltaTooLarge,
    VerificationFailed,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ChunkHashSyncPacket {
    ClientHaveHashes {
        worldgen_epoch: String,
        dim_id: i32,
        entries: Vec<ChunkHashEntry>,
    },
    ServerChunkMatch {
        dim_id: i32,
        chunks: Vec<(i32, i32)>,
    },
    ServerChunkDelta {
        worldgen_epoch: String,
        dim_id: i32,
        cx: i32,
        cz: i32,
        base_hash: [u8; 32],
        new_hash: [u8; 32],
        edits: Vec<ChunkBlockDelta>,
    },
    ServerRequestFull {
        dim_id: i32,
        chunks: Vec<(i32, i32)>,
        reason: FullSyncReason,
    },
}

/// Authoritative server chunk state. Sections are ordered by their signed Y.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServerChunk {
    pub cx: i32,
    pub cz: i32,
    pub sections: BTreeMap<i16, Vec<u32>>,
}

/// Supplies a previously retained delta from a client base to current server
/// state. Implementations must return canonical, sorted edits or `None`.
pub trait ChunkDeltaProvider {
    fn delta_from(
        &self,
        dim_id: i32,
        cx: i32,
        cz: i32,
        base_hash: &[u8; 32],
        new_hash: &[u8; 32],
    ) -> Option<Vec<ChunkBlockDelta>>;
}

fn merkle_root(domain: &[u8], leaves: &[[u8; 32]]) -> [u8; 32] {
    if leaves.is_empty() {
        let mut material = Vec::with_capacity(domain.len() + 1);
        material.extend_from_slice(domain);
        material.push(0);
        return crate::frl_auth::sha256(&material);
    }
    let mut level = leaves.to_vec();
    while level.len() > 1 {
        let mut next = Vec::with_capacity(level.len().div_ceil(2));
        for pair in level.chunks(2) {
            let mut material = Vec::with_capacity(domain.len() + 1 + 64);
            material.extend_from_slice(domain);
            material.push(1);
            material.extend_from_slice(&pair[0]);
            material.extend_from_slice(pair.get(1).unwrap_or(&pair[0]));
            next.push(crate::frl_auth::sha256(&material));
        }
        level = next;
    }
    level[0]
}

/// Domain-separated Merkle root for canonical section block-state ids.
pub fn section_state_hash(states: &[u32]) -> [u8; 32] {
    let leaves: Vec<[u8; 32]> = states
        .iter()
        .enumerate()
        .map(|(index, state)| {
            let mut material = Vec::with_capacity(29);
            material.extend_from_slice(b"CKH1/section-state/leaf/v1");
            material.extend_from_slice(&(index as u64).to_be_bytes());
            material.extend_from_slice(&state.to_be_bytes());
            crate::frl_auth::sha256(&material)
        })
        .collect();
    let root = merkle_root(b"CKH1/section-state/node/v1", &leaves);
    let mut material = Vec::with_capacity(40);
    material.extend_from_slice(b"CKH1/section-state/root/v1");
    material.extend_from_slice(&(states.len() as u64).to_be_bytes());
    material.extend_from_slice(&root);
    crate::frl_auth::sha256(&material)
}

/// Domain-separated Merkle root binding worldgen epoch, dimension and chunk
/// coordinates to the supplied section roots (in increasing section-Y order).
pub fn chunk_merkle_hash(
    worldgen_epoch: &str,
    dim_id: i32,
    cx: i32,
    cz: i32,
    section_hashes: &[[u8; 32]],
) -> [u8; 32] {
    let leaves: Vec<[u8; 32]> = section_hashes
        .iter()
        .enumerate()
        .map(|(index, hash)| {
            let mut material = Vec::with_capacity(55);
            material.extend_from_slice(b"CKH1/chunk-section/leaf/v1");
            material.extend_from_slice(&(index as u64).to_be_bytes());
            material.extend_from_slice(hash);
            crate::frl_auth::sha256(&material)
        })
        .collect();
    let root = merkle_root(b"CKH1/chunk-section/node/v1", &leaves);
    let mut material = Vec::with_capacity(64 + worldgen_epoch.len());
    material.extend_from_slice(b"CKH1/chunk/root/v1");
    material.extend_from_slice(&(worldgen_epoch.len() as u16).to_be_bytes());
    material.extend_from_slice(worldgen_epoch.as_bytes());
    material.extend_from_slice(&dim_id.to_be_bytes());
    material.extend_from_slice(&cx.to_be_bytes());
    material.extend_from_slice(&cz.to_be_bytes());
    material.extend_from_slice(&(section_hashes.len() as u32).to_be_bytes());
    material.extend_from_slice(&root);
    crate::frl_auth::sha256(&material)
}

fn chunk_hash(
    epoch: &str,
    dim_id: i32,
    cx: i32,
    cz: i32,
    sections: &BTreeMap<i16, Vec<u32>>,
) -> [u8; 32] {
    let hashes: Vec<_> = sections
        .values()
        .map(|states| section_state_hash(states))
        .collect();
    chunk_merkle_hash(epoch, dim_id, cx, cz, &hashes)
}

fn check_epoch(epoch: &str) -> Result<(), Ckh1Error> {
    if epoch.is_empty() || epoch.len() > MAX_EPOCH_LEN || epoch.len() > u16::MAX as usize {
        return Err(Ckh1Error::InvalidValue("worldgen epoch length"));
    }
    Ok(())
}

fn check_chunks(chunks: &[(i32, i32)]) -> Result<(), Ckh1Error> {
    if chunks.len() > MAX_CHUNKS_PER_HASH_FRAME {
        return Err(Ckh1Error::InvalidValue("chunk count"));
    }
    if chunks.windows(2).any(|pair| pair[0] >= pair[1]) {
        return Err(Ckh1Error::UnsortedOrDuplicate("chunk coordinates"));
    }
    Ok(())
}

fn check_entries(entries: &[ChunkHashEntry]) -> Result<(), Ckh1Error> {
    if entries.len() > MAX_CHUNKS_PER_HASH_FRAME {
        return Err(Ckh1Error::InvalidValue("chunk count"));
    }
    if entries
        .windows(2)
        .any(|pair| (pair[0].cx, pair[0].cz) >= (pair[1].cx, pair[1].cz))
    {
        return Err(Ckh1Error::UnsortedOrDuplicate("chunk coordinates"));
    }
    Ok(())
}

fn check_edits(edits: &[ChunkBlockDelta]) -> Result<(), Ckh1Error> {
    if edits.len() > MAX_DELTA_EDITS_PER_CHUNK {
        return Err(Ckh1Error::InvalidValue("delta edit count"));
    }
    if edits.iter().any(|edit| edit.local_idx >= 4096) {
        return Err(Ckh1Error::InvalidValue("local block index"));
    }
    if edits.windows(2).any(|pair| {
        (pair[0].section_y, pair[0].local_idx) >= (pair[1].section_y, pair[1].local_idx)
    }) {
        return Err(Ckh1Error::UnsortedOrDuplicate("delta edits"));
    }
    Ok(())
}

fn put_u16(out: &mut Vec<u8>, n: u16) {
    out.extend_from_slice(&n.to_be_bytes());
}
fn put_i16(out: &mut Vec<u8>, n: i16) {
    out.extend_from_slice(&n.to_be_bytes());
}
fn put_i32(out: &mut Vec<u8>, n: i32) {
    out.extend_from_slice(&n.to_be_bytes());
}
fn put_u32(out: &mut Vec<u8>, n: u32) {
    out.extend_from_slice(&n.to_be_bytes());
}
fn put_epoch(out: &mut Vec<u8>, epoch: &str) -> Result<(), Ckh1Error> {
    check_epoch(epoch)?;
    put_u16(out, epoch.len() as u16);
    out.extend_from_slice(epoch.as_bytes());
    Ok(())
}
fn put_coords(out: &mut Vec<u8>, chunks: &[(i32, i32)]) -> Result<(), Ckh1Error> {
    check_chunks(chunks)?;
    put_u16(out, chunks.len() as u16);
    for &(x, z) in chunks {
        put_i32(out, x);
        put_i32(out, z);
    }
    Ok(())
}
fn finish(out: Vec<u8>) -> Result<Vec<u8>, Ckh1Error> {
    if out.len() > MAX_CKH1_FRAME_BYTES {
        Err(Ckh1Error::FrameTooLarge)
    } else {
        Ok(out)
    }
}

/// Encode a canonical CKH1 packet. Every semantic limit is checked before
/// returning bytes; no oversized object can be truncated into a valid frame.
pub fn encode_ckh1(packet: &ChunkHashSyncPacket) -> Result<Vec<u8>, Ckh1Error> {
    let mut out = Vec::new();
    out.extend_from_slice(&CKH1_MAGIC);
    put_u16(&mut out, CKH1_SCHEMA_VERSION);
    match packet {
        ChunkHashSyncPacket::ClientHaveHashes {
            worldgen_epoch,
            dim_id,
            entries,
        } => {
            check_epoch(worldgen_epoch)?;
            check_entries(entries)?;
            out.push(0);
            put_epoch(&mut out, worldgen_epoch)?;
            put_i32(&mut out, *dim_id);
            put_u16(&mut out, entries.len() as u16);
            for entry in entries {
                put_i32(&mut out, entry.cx);
                put_i32(&mut out, entry.cz);
                out.extend_from_slice(&entry.hash);
            }
        }
        ChunkHashSyncPacket::ServerChunkMatch { dim_id, chunks } => {
            out.push(1);
            put_i32(&mut out, *dim_id);
            put_coords(&mut out, chunks)?;
        }
        ChunkHashSyncPacket::ServerChunkDelta {
            worldgen_epoch,
            dim_id,
            cx,
            cz,
            base_hash,
            new_hash,
            edits,
        } => {
            check_epoch(worldgen_epoch)?;
            check_edits(edits)?;
            out.push(2);
            put_epoch(&mut out, worldgen_epoch)?;
            put_i32(&mut out, *dim_id);
            put_i32(&mut out, *cx);
            put_i32(&mut out, *cz);
            out.extend_from_slice(base_hash);
            out.extend_from_slice(new_hash);
            put_u32(&mut out, edits.len() as u32);
            for edit in edits {
                put_i16(&mut out, edit.section_y);
                put_u16(&mut out, edit.local_idx);
                put_u32(&mut out, edit.new_state);
            }
        }
        ChunkHashSyncPacket::ServerRequestFull {
            dim_id,
            chunks,
            reason,
        } => {
            out.push(3);
            put_i32(&mut out, *dim_id);
            put_coords(&mut out, chunks)?;
            out.push(match reason {
                FullSyncReason::EpochMismatch => 0,
                FullSyncReason::BaseHashUnknown => 1,
                FullSyncReason::DeltaTooLarge => 2,
                FullSyncReason::VerificationFailed => 3,
            });
        }
    }
    finish(out)
}

struct Cursor<'a> {
    bytes: &'a [u8],
    pos: usize,
}
impl<'a> Cursor<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], Ckh1Error> {
        let end = self.pos.checked_add(n).ok_or(Ckh1Error::Truncated)?;
        if end > self.bytes.len() {
            return Err(Ckh1Error::Truncated);
        }
        let slice = &self.bytes[self.pos..end];
        self.pos = end;
        Ok(slice)
    }
    fn u8(&mut self) -> Result<u8, Ckh1Error> {
        Ok(self.take(1)?[0])
    }
    fn u16(&mut self) -> Result<u16, Ckh1Error> {
        Ok(u16::from_be_bytes(
            self.take(2)?.try_into().map_err(|_| Ckh1Error::Truncated)?,
        ))
    }
    fn i16(&mut self) -> Result<i16, Ckh1Error> {
        Ok(i16::from_be_bytes(
            self.take(2)?.try_into().map_err(|_| Ckh1Error::Truncated)?,
        ))
    }
    fn i32(&mut self) -> Result<i32, Ckh1Error> {
        Ok(i32::from_be_bytes(
            self.take(4)?.try_into().map_err(|_| Ckh1Error::Truncated)?,
        ))
    }
    fn u32(&mut self) -> Result<u32, Ckh1Error> {
        Ok(u32::from_be_bytes(
            self.take(4)?.try_into().map_err(|_| Ckh1Error::Truncated)?,
        ))
    }
    fn hash(&mut self) -> Result<[u8; 32], Ckh1Error> {
        self.take(32)?.try_into().map_err(|_| Ckh1Error::Truncated)
    }
    fn epoch(&mut self) -> Result<String, Ckh1Error> {
        let len = usize::from(self.u16()?);
        if len == 0 || len > MAX_EPOCH_LEN {
            return Err(Ckh1Error::InvalidValue("worldgen epoch length"));
        }
        String::from_utf8(self.take(len)?.to_vec()).map_err(|_| Ckh1Error::InvalidUtf8)
    }
    fn coords(&mut self) -> Result<Vec<(i32, i32)>, Ckh1Error> {
        let count = usize::from(self.u16()?);
        if count > MAX_CHUNKS_PER_HASH_FRAME {
            return Err(Ckh1Error::InvalidValue("chunk count"));
        }
        let mut chunks = Vec::with_capacity(count);
        for _ in 0..count {
            chunks.push((self.i32()?, self.i32()?));
        }
        check_chunks(&chunks)?;
        Ok(chunks)
    }
}

/// Decode a complete canonical CKH1 packet, rejecting malformed and trailing
/// input before it can influence caller state.
pub fn decode_ckh1(bytes: &[u8]) -> Result<ChunkHashSyncPacket, Ckh1Error> {
    if bytes.len() > MAX_CKH1_FRAME_BYTES {
        return Err(Ckh1Error::FrameTooLarge);
    }
    if bytes.len() < HEADER_LEN {
        return Err(Ckh1Error::Truncated);
    }
    if bytes.get(..4) != Some(&CKH1_MAGIC) {
        return Err(Ckh1Error::BadMagic);
    }
    let version = u16::from_be_bytes([bytes[4], bytes[5]]);
    if version != CKH1_SCHEMA_VERSION {
        return Err(Ckh1Error::UnknownSchemaVersion(version));
    }
    let mut c = Cursor { bytes, pos: 6 };
    let kind = c.u8()?;
    let packet = match kind {
        0 => {
            let worldgen_epoch = c.epoch()?;
            let dim_id = c.i32()?;
            let count = usize::from(c.u16()?);
            if count > MAX_CHUNKS_PER_HASH_FRAME {
                return Err(Ckh1Error::InvalidValue("chunk count"));
            }
            let mut entries = Vec::with_capacity(count);
            for _ in 0..count {
                entries.push(ChunkHashEntry {
                    cx: c.i32()?,
                    cz: c.i32()?,
                    hash: c.hash()?,
                });
            }
            check_entries(&entries)?;
            ChunkHashSyncPacket::ClientHaveHashes {
                worldgen_epoch,
                dim_id,
                entries,
            }
        }
        1 => ChunkHashSyncPacket::ServerChunkMatch {
            dim_id: c.i32()?,
            chunks: c.coords()?,
        },
        2 => {
            let worldgen_epoch = c.epoch()?;
            let dim_id = c.i32()?;
            let cx = c.i32()?;
            let cz = c.i32()?;
            let base_hash = c.hash()?;
            let new_hash = c.hash()?;
            let count = usize::try_from(c.u32()?)
                .map_err(|_| Ckh1Error::InvalidValue("delta edit count"))?;
            if count > MAX_DELTA_EDITS_PER_CHUNK {
                return Err(Ckh1Error::InvalidValue("delta edit count"));
            }
            let mut edits = Vec::with_capacity(count);
            for _ in 0..count {
                edits.push(ChunkBlockDelta {
                    section_y: c.i16()?,
                    local_idx: c.u16()?,
                    new_state: c.u32()?,
                });
            }
            check_edits(&edits)?;
            ChunkHashSyncPacket::ServerChunkDelta {
                worldgen_epoch,
                dim_id,
                cx,
                cz,
                base_hash,
                new_hash,
                edits,
            }
        }
        3 => {
            let dim_id = c.i32()?;
            let chunks = c.coords()?;
            let reason = match c.u8()? {
                0 => FullSyncReason::EpochMismatch,
                1 => FullSyncReason::BaseHashUnknown,
                2 => FullSyncReason::DeltaTooLarge,
                3 => FullSyncReason::VerificationFailed,
                _ => return Err(Ckh1Error::InvalidValue("full-sync reason")),
            };
            ChunkHashSyncPacket::ServerRequestFull {
                dim_id,
                chunks,
                reason,
            }
        }
        other => return Err(Ckh1Error::InvalidPacketKind(other)),
    };
    if c.pos != bytes.len() {
        return Err(Ckh1Error::TrailingBytes);
    }
    Ok(packet)
}

/// Plan one response per advertised client chunk. A full request is emitted
/// whenever the epoch differs or no usable retained delta exists.
pub fn plan_chunk_sync(
    client: &ChunkHashSyncPacket,
    server_worldgen_epoch: &str,
    dim_id: i32,
    server_chunks: &[ServerChunk],
    delta_provider: Option<&dyn ChunkDeltaProvider>,
    max_delta_edits: usize,
) -> Result<Vec<ChunkHashSyncPacket>, Ckh1Error> {
    let (client_epoch, client_dim, entries) = match client {
        ChunkHashSyncPacket::ClientHaveHashes {
            worldgen_epoch,
            dim_id,
            entries,
        } => (worldgen_epoch, *dim_id, entries),
        _ => return Err(Ckh1Error::InvalidValue("planner requires ClientHaveHashes")),
    };
    check_epoch(client_epoch)?;
    check_epoch(server_worldgen_epoch)?;
    check_entries(entries)?;
    if client_dim != dim_id {
        return Err(Ckh1Error::InvalidValue("dimension mismatch"));
    }
    let mut server_coords = server_chunks
        .iter()
        .map(|chunk| (chunk.cx, chunk.cz))
        .collect::<Vec<_>>();
    server_coords.sort_unstable();
    if server_coords.windows(2).any(|pair| pair[0] == pair[1]) {
        return Err(Ckh1Error::UnsortedOrDuplicate("server chunk coordinates"));
    }
    let mut result = Vec::with_capacity(entries.len());
    for entry in entries {
        let coords = vec![(entry.cx, entry.cz)];
        if client_epoch != server_worldgen_epoch {
            result.push(ChunkHashSyncPacket::ServerRequestFull {
                dim_id,
                chunks: coords,
                reason: FullSyncReason::EpochMismatch,
            });
            continue;
        }
        let Some(server) = server_chunks
            .iter()
            .find(|chunk| chunk.cx == entry.cx && chunk.cz == entry.cz)
        else {
            result.push(ChunkHashSyncPacket::ServerRequestFull {
                dim_id,
                chunks: coords,
                reason: FullSyncReason::BaseHashUnknown,
            });
            continue;
        };
        if server.sections.values().any(|states| states.len() != 4096) {
            result.push(ChunkHashSyncPacket::ServerRequestFull {
                dim_id,
                chunks: coords,
                reason: FullSyncReason::VerificationFailed,
            });
            continue;
        }
        let new_hash = chunk_hash(
            server_worldgen_epoch,
            dim_id,
            server.cx,
            server.cz,
            &server.sections,
        );
        if entry.hash == new_hash {
            result.push(ChunkHashSyncPacket::ServerChunkMatch {
                dim_id,
                chunks: coords,
            });
            continue;
        }
        let Some(edits) = delta_provider.and_then(|provider| {
            provider.delta_from(dim_id, entry.cx, entry.cz, &entry.hash, &new_hash)
        }) else {
            result.push(ChunkHashSyncPacket::ServerRequestFull {
                dim_id,
                chunks: coords,
                reason: FullSyncReason::BaseHashUnknown,
            });
            continue;
        };
        if edits.len() > max_delta_edits.min(MAX_DELTA_EDITS_PER_CHUNK) {
            result.push(ChunkHashSyncPacket::ServerRequestFull {
                dim_id,
                chunks: coords,
                reason: FullSyncReason::DeltaTooLarge,
            });
            continue;
        }
        if check_edits(&edits).is_err() {
            result.push(ChunkHashSyncPacket::ServerRequestFull {
                dim_id,
                chunks: coords,
                reason: FullSyncReason::VerificationFailed,
            });
            continue;
        }
        result.push(ChunkHashSyncPacket::ServerChunkDelta {
            worldgen_epoch: server_worldgen_epoch.to_owned(),
            dim_id,
            cx: entry.cx,
            cz: entry.cz,
            base_hash: entry.hash,
            new_hash,
            edits,
        });
    }
    Ok(result)
}

/// Verify then atomically apply a server delta to cached client section states.
/// The input map is replaced only after the post-apply chunk hash is verified.
pub fn apply_verified_chunk_delta(
    local_worldgen_epoch: &str,
    delta: &ChunkHashSyncPacket,
    sections: &mut BTreeMap<i16, Vec<u32>>,
) -> Result<(), Ckh1Error> {
    let (epoch, dim_id, cx, cz, base_hash, new_hash, edits) = match delta {
        ChunkHashSyncPacket::ServerChunkDelta {
            worldgen_epoch,
            dim_id,
            cx,
            cz,
            base_hash,
            new_hash,
            edits,
        } => (
            worldgen_epoch,
            *dim_id,
            *cx,
            *cz,
            base_hash,
            new_hash,
            edits,
        ),
        _ => return Err(Ckh1Error::InvalidValue("expected ServerChunkDelta")),
    };
    if epoch != local_worldgen_epoch {
        return Err(Ckh1Error::EpochMismatch);
    }
    check_epoch(epoch)?;
    check_edits(edits)?;
    if sections.values().any(|states| states.len() != 4096) {
        return Err(Ckh1Error::InvalidValue("section state count"));
    }
    if chunk_hash(epoch, dim_id, cx, cz, sections) != *base_hash {
        return Err(Ckh1Error::BaseHashMismatch);
    }
    let mut next = sections.clone();
    for edit in edits {
        if usize::from(edit.local_idx) >= 4096 {
            return Err(Ckh1Error::InvalidValue("local block index"));
        }
        let Some(states) = next.get_mut(&edit.section_y) else {
            return Err(Ckh1Error::InvalidValue("unknown section"));
        };
        if states.len() != 4096 {
            return Err(Ckh1Error::InvalidValue("section state count"));
        }
        states[usize::from(edit.local_idx)] = edit.new_state;
    }
    if chunk_hash(epoch, dim_id, cx, cz, &next) != *new_hash {
        return Err(Ckh1Error::PostApplyHashMismatch);
    }
    *sections = next;
    Ok(())
}
