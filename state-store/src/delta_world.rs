// SPDX-License-Identifier: LGPL-3.0-or-later
//! Deterministic delta world saves and content-addressed time-travel
//! generations.
//!
//! The crate does not generate terrain itself: terrain generation is owned by
//! the worldgen lanes. This module stores the *delta contract* those lanes can
//! reproduce deterministically. A chunk whose state equals the deterministic
//! base terrain for `(seed, worldgen_epoch, dim_id, cx, cz)` is represented as
//! zero delta bytes; changed chunks are compact `MDW1` blobs containing only
//! sparse block edits plus opaque tombstones for unknown or removed block IDs.
//!
//! [`DeltaGenerationStore`] keeps those blobs content-addressed on disk and
//! immutable generation records under it. Every generation is a complete sparse
//! overlay against the deterministic base (not a delta against its parent), so
//! checking out a generation never requires replaying its ancestry. The parent
//! edge is history metadata used for branches and operator visibility.
//!
//! All persistent formats are deterministic, schema-versioned, CRC32-sealed,
//! and fail closed on truncation, corruption, unknown schema versions,
//! mismatched chunk metadata, or `worldgen_epoch` mismatches.

use crate::migrate::write_file_atomic;
use megu_core::crypto::{hex_encode, sha256};
use megu_core::merkle_cas::{Digest32, MerkleTree};
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::error::Error;
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

/// File magic for one chunk delta: Megu Delta World, revision 1.
pub const CHUNK_DELTA_MAGIC: &[u8; 4] = b"MDW1";
/// The only chunk-delta schema this implementation reads and writes.
pub const DELTA_WORLD_SCHEMA_VERSION: u16 = 1;
/// Maximum number of `u32` block states a chunk/section array may contain.
/// `local_idx` is a `u16`, so 65_536 is the exact representable ceiling.
pub const MAX_CHUNK_STATES: usize = u16::MAX as usize + 1;
/// Maximum bytes in a `worldgen_epoch` or namespaced block id.
pub const MAX_DELTA_STRING_BYTES: usize = 512;
/// Maximum bytes in one unknown block's properties JSON.
pub const MAX_PROPERTIES_JSON_BYTES: usize = 65_535;
/// Maximum bytes in one unknown block's opaque NBT payload.
pub const MAX_UNKNOWN_NBT_BYTES: usize = 1_048_576;
/// Maximum bytes in one encoded delta blob. The cap is far above a fully
/// edited 65_536-state chunk and exists to reject garbage before allocation.
pub const MAX_DELTA_BLOB_BYTES: usize = 32 * 1_048_576;
/// Maximum chunks in one generation record.
pub const MAX_GENERATION_CHUNKS: usize = 1_000_000;
/// Maximum bytes in one immutable generation record.
pub const MAX_GENERATION_RECORD_BYTES: usize = 128 * 1_048_576;
/// Maximum entries in the mutable generation-index manifest.
pub const MAX_MANIFEST_GENERATIONS: usize = 1_000_000;
/// Maximum bytes in the generation-index manifest.
pub const MAX_MANIFEST_BYTES: usize = 64 * 1_048_576;
/// Maximum bytes in a generation label.
pub const MAX_LABEL_BYTES: usize = 1_024;

const ENCODING_EXPLICIT: u8 = 0;
const ENCODING_RUNS: u8 = 1;
const GENERATION_MAGIC: &[u8; 4] = b"MDG1";
const MANIFEST_MAGIC: &[u8; 4] = b"MGM1";
const STORAGE_BASE: u8 = 0;
const STORAGE_BLOB: u8 = 1;

// ------------------------------------------------------------ byte encoding

fn put_u16(out: &mut Vec<u8>, value: u16) {
    out.extend_from_slice(&value.to_be_bytes());
}

fn put_u32(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_be_bytes());
}

fn put_i32(out: &mut Vec<u8>, value: i32) {
    out.extend_from_slice(&value.to_be_bytes());
}

fn put_u64(out: &mut Vec<u8>, value: u64) {
    out.extend_from_slice(&value.to_be_bytes());
}

fn put_bytes(out: &mut Vec<u8>, bytes: &[u8]) {
    out.extend_from_slice(bytes);
}

#[derive(Clone, Copy)]
struct Reader<'a> {
    bytes: &'a [u8],
}

impl<'a> Reader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes }
    }

    fn take(&mut self, len: usize) -> Result<&'a [u8], ()> {
        let (out, rest) = self.bytes.split_at_checked(len).ok_or(())?;
        self.bytes = rest;
        Ok(out)
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N], ()> {
        let (out, rest) = self.bytes.split_first_chunk().ok_or(())?;
        self.bytes = rest;
        Ok(*out)
    }

    fn u8(&mut self) -> Result<u8, ()> {
        let [byte] = self.array()?;
        Ok(byte)
    }

    fn u16(&mut self) -> Result<u16, ()> {
        Ok(u16::from_be_bytes(self.array()?))
    }

    fn u32(&mut self) -> Result<u32, ()> {
        Ok(u32::from_be_bytes(self.array()?))
    }

    fn i32(&mut self) -> Result<i32, ()> {
        Ok(i32::from_be_bytes(self.array()?))
    }

    fn u64(&mut self) -> Result<u64, ()> {
        Ok(u64::from_be_bytes(self.array()?))
    }

    fn remaining(&self) -> usize {
        self.bytes.len()
    }
}

// ------------------------------------------------------------ chunk deltas

/// One sparse block-state edit against deterministic base terrain.
///
/// `base_state` is the deterministic pre-image state and is authenticated when
/// applying the edit. `new_state` is the state stored at `local_idx` after the
/// edit. The two differ in every canonical edit.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct BlockDeltaEdit {
    pub local_idx: u16,
    pub base_state: u32,
    pub new_state: u32,
}

/// Opaque preservation record for a block this engine cannot or must not
/// resolve to a numeric state.
///
/// Removing a mod or loading another version must never silently erase these
/// blocks to air. The namespaced id, properties and NBT payload are carried
/// byte-for-byte so the owning mod/version can reclaim the exact block later.
/// `properties_json` is opaque to this crate by design: interpreting it would
/// couple the store to registry/schema details it does not own.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnknownBlockEntry {
    pub local_idx: u16,
    pub namespaced_id: String,
    pub properties_json: String,
    pub nbt_bytes: Vec<u8>,
}

/// A compact sparse delta for one chunk column or section-like block-state
/// array.
///
/// `schema_version` is always [`DELTA_WORLD_SCHEMA_VERSION`] in newly computed
/// deltas. `compute_chunk_delta` compares arrays only; use [`Self::with_identity`]
/// to attach the `(worldgen_epoch, dim_id, cx, cz)` identity required by the
/// persistent store.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChunkDelta {
    pub schema_version: u16,
    pub worldgen_epoch: String,
    pub dim_id: i32,
    pub cx: i32,
    pub cz: i32,
    pub edits: Vec<BlockDeltaEdit>,
    pub unknown_blocks: Vec<UnknownBlockEntry>,
}

impl ChunkDelta {
    /// Construct an empty delta with an explicit chunk identity.
    pub fn new(worldgen_epoch: &str, dim_id: i32, cx: i32, cz: i32) -> Self {
        Self {
            schema_version: DELTA_WORLD_SCHEMA_VERSION,
            worldgen_epoch: worldgen_epoch.to_string(),
            dim_id,
            cx,
            cz,
            edits: Vec::new(),
            unknown_blocks: Vec::new(),
        }
    }

    /// Attach the persistent identity to a delta produced by array comparison.
    pub fn with_identity(mut self, worldgen_epoch: &str, dim_id: i32, cx: i32, cz: i32) -> Self {
        self.worldgen_epoch = worldgen_epoch.to_string();
        self.dim_id = dim_id;
        self.cx = cx;
        self.cz = cz;
        self
    }

    /// Whether this chunk is exactly its deterministic base terrain. Such a
    /// chunk occupies zero bytes in the content-addressed delta-blob store.
    pub fn is_zero_bytes(&self) -> bool {
        self.edits.is_empty() && self.unknown_blocks.is_empty()
    }

    /// Bytes this delta occupies in the generation blob store. Zero-byte base
    /// chunks are never serialized as an `MDW1` blob.
    pub fn stored_delta_bytes(&self) -> usize {
        if self.is_zero_bytes() {
            0
        } else {
            encode_chunk_delta(self).len()
        }
    }

    /// Add or replace an opaque unknown-block preservation entry.
    pub fn set_unknown_block(&mut self, entry: UnknownBlockEntry) {
        if let Some(existing) = self
            .unknown_blocks
            .iter_mut()
            .find(|existing| existing.local_idx == entry.local_idx)
        {
            *existing = entry;
        } else {
            self.unknown_blocks.push(entry);
        }
        self.unknown_blocks.sort_by_key(|entry| entry.local_idx);
    }

    /// Sort sparse edits and tombstones into canonical wire order.
    pub fn canonicalize(&mut self) {
        self.edits.sort_unstable();
        self.unknown_blocks
            .sort_unstable_by_key(|entry| entry.local_idx);
    }

    /// Return a canonically ordered clone.
    pub fn canonicalized(&self) -> Self {
        let mut out = self.clone();
        out.canonicalize();
        out
    }
}

/// Why a chunk-delta operation or codec failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DeltaError {
    /// Input state arrays do not have the same length.
    StateLengthMismatch { base_len: usize, current_len: usize },
    /// More states than a `u16` local index can represent.
    TooManyStates { len: usize },
    /// The first four bytes are not `MDW1`.
    BadMagic,
    /// The schema is not the pinned revision 1 format.
    UnsupportedSchemaVersion(u16),
    /// The edit payload encoding discriminator is unknown.
    UnknownEncoding(u8),
    /// The stream ends before the current record completes.
    Truncated,
    /// Extra bytes follow the canonical payload before the CRC trailer.
    TrailingBytes(usize),
    /// CRC32 mismatch; first value is stored, second computed.
    CrcMismatch { expected: u32, actual: u32 },
    /// A string field is not UTF-8.
    InvalidUtf8(&'static str),
    /// A count exceeds the format's fail-closed cap.
    OversizedCount {
        field: &'static str,
        count: u32,
        cap: u32,
    },
    /// A byte/string length exceeds the format's fail-closed cap.
    OversizedLength {
        field: &'static str,
        len: usize,
        cap: usize,
    },
    /// A structurally mandatory field is empty or otherwise invalid.
    InvalidStructure(&'static str),
    /// Indexes in a canonical stream were repeated or not strictly increasing.
    UnsortedOrDuplicateIndex { local_idx: u16 },
    /// A canonical edit may not be a no-op.
    NoOpEdit { local_idx: u16 },
    /// A contiguous-run payload is malformed.
    InvalidRun(&'static str),
    /// Applying an edit outside the state array.
    IndexOutOfBounds { local_idx: u16, state_len: usize },
    /// Applying two edits to the same local index.
    DuplicateIndex { local_idx: u16 },
    /// The base array does not contain the authenticated pre-image state.
    BaseStateMismatch {
        local_idx: u16,
        expected_base: u32,
        actual_base: u32,
    },
}

impl fmt::Display for DeltaError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        use DeltaError::*;
        match self {
            StateLengthMismatch {
                base_len,
                current_len,
            } => write!(
                f,
                "state length mismatch: base has {base_len}, current has {current_len}"
            ),
            TooManyStates { len } => write!(f, "state length {len} exceeds the u16 index space"),
            BadMagic => f.write_str("not an MDW1 chunk delta (bad magic)"),
            UnsupportedSchemaVersion(version) => {
                write!(f, "unsupported chunk-delta schema version {version}")
            }
            UnknownEncoding(encoding) => write!(f, "unknown chunk-delta encoding {encoding}"),
            Truncated => f.write_str("chunk delta ends mid-record"),
            TrailingBytes(len) => write!(f, "chunk delta has {len} trailing payload bytes"),
            CrcMismatch { expected, actual } => write!(
                f,
                "chunk-delta CRC32 mismatch: stored {expected:#010x}, computed {actual:#010x}"
            ),
            InvalidUtf8(field) => write!(f, "chunk-delta field {field} is not valid UTF-8"),
            OversizedCount { field, count, cap } => {
                write!(f, "chunk-delta {field} count {count} exceeds cap {cap}")
            }
            OversizedLength { field, len, cap } => {
                write!(f, "chunk-delta {field} length {len} exceeds cap {cap}")
            }
            InvalidStructure(reason) => write!(f, "invalid chunk-delta structure: {reason}"),
            UnsortedOrDuplicateIndex { local_idx } => write!(
                f,
                "chunk-delta index {local_idx} is repeated or out of canonical order"
            ),
            NoOpEdit { local_idx } => write!(f, "chunk-delta edit at {local_idx} is a no-op"),
            InvalidRun(reason) => {
                write!(f, "chunk-delta contiguous-run payload is invalid: {reason}")
            }
            IndexOutOfBounds {
                local_idx,
                state_len,
            } => write!(
                f,
                "chunk-delta index {local_idx} is outside a {state_len}-state array"
            ),
            DuplicateIndex { local_idx } => write!(f, "duplicate chunk-delta index {local_idx}"),
            BaseStateMismatch {
                local_idx,
                expected_base,
                actual_base,
            } => write!(
                f,
                "base state mismatch at {local_idx}: expected {expected_base}, actual {actual_base}"
            ),
        }
    }
}

impl Error for DeltaError {}

/// Fallible form of [`compute_chunk_delta`].
pub fn try_compute_chunk_delta(
    base_states: &[u32],
    current_states: &[u32],
) -> Result<ChunkDelta, DeltaError> {
    if base_states.len() != current_states.len() {
        return Err(DeltaError::StateLengthMismatch {
            base_len: base_states.len(),
            current_len: current_states.len(),
        });
    }
    if base_states.len() > MAX_CHUNK_STATES {
        return Err(DeltaError::TooManyStates {
            len: base_states.len(),
        });
    }
    let edits = base_states
        .iter()
        .zip(current_states.iter())
        .enumerate()
        .filter_map(|(index, (&base_state, &new_state))| {
            (base_state != new_state).then_some(BlockDeltaEdit {
                local_idx: index as u16,
                base_state,
                new_state,
            })
        })
        .collect();
    Ok(ChunkDelta {
        schema_version: DELTA_WORLD_SCHEMA_VERSION,
        worldgen_epoch: String::new(),
        dim_id: 0,
        cx: 0,
        cz: 0,
        edits,
        unknown_blocks: Vec::new(),
    })
}

/// Compute the sparse edit list transforming `base_states` into
/// `current_states`.
///
/// This pure comparison carries no persistent identity; attach one with
/// [`ChunkDelta::with_identity`] before committing it to a generation store.
///
/// # Panics
///
/// Panics when the arrays differ in length or exceed [`MAX_CHUNK_STATES`].
/// Use [`try_compute_chunk_delta`] when either input is untrusted.
pub fn compute_chunk_delta(base_states: &[u32], current_states: &[u32]) -> ChunkDelta {
    try_compute_chunk_delta(base_states, current_states)
        .unwrap_or_else(|error| panic!("compute_chunk_delta contract violated: {error}"))
}

/// Apply `delta` to a copy of `base_states`, authenticating every recorded
/// pre-image state before replacing it.
pub fn apply_chunk_delta(base_states: &[u32], delta: &ChunkDelta) -> Result<Vec<u32>, DeltaError> {
    let mut out = base_states.to_vec();
    let mut seen = BTreeSet::new();
    for edit in &delta.edits {
        if edit.base_state == edit.new_state {
            return Err(DeltaError::NoOpEdit {
                local_idx: edit.local_idx,
            });
        }
        if !seen.insert(edit.local_idx) {
            return Err(DeltaError::DuplicateIndex {
                local_idx: edit.local_idx,
            });
        }
        let index = usize::from(edit.local_idx);
        let Some(slot) = out.get_mut(index) else {
            return Err(DeltaError::IndexOutOfBounds {
                local_idx: edit.local_idx,
                state_len: base_states.len(),
            });
        };
        if *slot != edit.base_state {
            return Err(DeltaError::BaseStateMismatch {
                local_idx: edit.local_idx,
                expected_base: edit.base_state,
                actual_base: *slot,
            });
        }
        *slot = edit.new_state;
    }
    Ok(out)
}

/// Invert every sparse edit, giving an instant undo/rollback delta.
///
/// Unknown-block tombstones are copied unchanged. They are opaque preservation
/// records rather than numeric-state mutations, so dropping them during an
/// inverse operation would violate the never-erase-to-air contract.
pub fn invert_chunk_delta(delta: &ChunkDelta) -> ChunkDelta {
    let mut inverted = delta.clone();
    for edit in &mut inverted.edits {
        std::mem::swap(&mut edit.base_state, &mut edit.new_state);
    }
    inverted
}

fn canonical_edit_encoding(edits: &[BlockDeltaEdit]) -> u8 {
    let edit_count = edits.len();
    let run_count = contiguous_run_count(edits);
    let explicit_payload = edit_count * 10;
    let run_payload = 4 + run_count * 4 + edit_count * 8;
    if edit_count != 0 && run_payload < explicit_payload {
        ENCODING_RUNS
    } else {
        ENCODING_EXPLICIT
    }
}

fn edit_payload_len(edits: &[BlockDeltaEdit], encoding: u8) -> usize {
    if encoding == ENCODING_RUNS {
        4 + contiguous_run_count(edits) * 4 + edits.len() * 8
    } else {
        edits.len() * 10
    }
}

fn canonical_encoded_len(delta: &ChunkDelta) -> usize {
    let mut edits = delta.edits.clone();
    edits.sort_by_key(|edit| edit.local_idx);
    let encoding = canonical_edit_encoding(&edits);
    let mut len = 34usize + delta.worldgen_epoch.len() + edit_payload_len(&edits, encoding);
    for entry in &delta.unknown_blocks {
        len += 12 + entry.namespaced_id.len() + entry.properties_json.len() + entry.nbt_bytes.len();
    }
    len
}

/// Validate the canonical structural contract of an in-memory delta.
pub fn validate_chunk_delta(delta: &ChunkDelta) -> Result<(), DeltaError> {
    if delta.schema_version != DELTA_WORLD_SCHEMA_VERSION {
        return Err(DeltaError::UnsupportedSchemaVersion(delta.schema_version));
    }
    if delta.worldgen_epoch.len() > MAX_DELTA_STRING_BYTES {
        return Err(DeltaError::OversizedLength {
            field: "worldgen_epoch",
            len: delta.worldgen_epoch.len(),
            cap: MAX_DELTA_STRING_BYTES,
        });
    }
    if delta.edits.len() > MAX_CHUNK_STATES {
        return Err(DeltaError::OversizedCount {
            field: "edit_count",
            count: delta.edits.len() as u32,
            cap: MAX_CHUNK_STATES as u32,
        });
    }
    let mut edit_indexes = BTreeSet::new();
    for edit in &delta.edits {
        if edit.base_state == edit.new_state {
            return Err(DeltaError::NoOpEdit {
                local_idx: edit.local_idx,
            });
        }
        if !edit_indexes.insert(edit.local_idx) {
            return Err(DeltaError::DuplicateIndex {
                local_idx: edit.local_idx,
            });
        }
    }
    if delta.unknown_blocks.len() > MAX_CHUNK_STATES {
        return Err(DeltaError::OversizedCount {
            field: "unknown_count",
            count: delta.unknown_blocks.len() as u32,
            cap: MAX_CHUNK_STATES as u32,
        });
    }
    let mut unknown_indexes = BTreeSet::new();
    for entry in &delta.unknown_blocks {
        if !unknown_indexes.insert(entry.local_idx) {
            return Err(DeltaError::DuplicateIndex {
                local_idx: entry.local_idx,
            });
        }
        if entry.namespaced_id.is_empty() {
            return Err(DeltaError::InvalidStructure("namespaced_id is empty"));
        }
        if entry.namespaced_id.len() > MAX_DELTA_STRING_BYTES {
            return Err(DeltaError::OversizedLength {
                field: "namespaced_id",
                len: entry.namespaced_id.len(),
                cap: MAX_DELTA_STRING_BYTES,
            });
        }
        if entry.properties_json.len() > MAX_PROPERTIES_JSON_BYTES {
            return Err(DeltaError::OversizedLength {
                field: "properties_json",
                len: entry.properties_json.len(),
                cap: MAX_PROPERTIES_JSON_BYTES,
            });
        }
        if entry.nbt_bytes.len() > MAX_UNKNOWN_NBT_BYTES {
            return Err(DeltaError::OversizedLength {
                field: "nbt_bytes",
                len: entry.nbt_bytes.len(),
                cap: MAX_UNKNOWN_NBT_BYTES,
            });
        }
    }
    let encoded_len = canonical_encoded_len(delta);
    if encoded_len > MAX_DELTA_BLOB_BYTES {
        return Err(DeltaError::OversizedLength {
            field: "delta_blob",
            len: encoded_len,
            cap: MAX_DELTA_BLOB_BYTES,
        });
    }
    Ok(())
}

fn contiguous_run_count(edits: &[BlockDeltaEdit]) -> usize {
    let mut runs = 0usize;
    let mut previous = None;
    for edit in edits {
        let contiguous = previous == Some(edit.local_idx.wrapping_sub(1)) && edit.local_idx != 0;
        if !contiguous {
            runs += 1;
        }
        previous = Some(edit.local_idx);
    }
    runs
}

/// Serialize a delta into the deterministic `MDW1` wire format.
///
/// Edits and unknown entries are sorted before serialization. Contiguous edit
/// runs use the run-length representation only when its payload is strictly
/// smaller than explicit `(index, base, new)` triples, making the encoding
/// canonical rather than input-order dependent. A zero-byte base chunk is
/// still encodable for standalone use; the generation store simply never
/// writes that encoding to a blob because the chunk has zero delta bytes.
pub fn encode_chunk_delta(delta: &ChunkDelta) -> Vec<u8> {
    validate_chunk_delta(delta)
        .unwrap_or_else(|error| panic!("encode_chunk_delta contract violated: {error}"));
    let delta = delta.canonicalized();
    let edit_count = delta.edits.len();
    let run_count = contiguous_run_count(&delta.edits);
    let encoding = canonical_edit_encoding(&delta.edits);

    let mut out = Vec::new();
    put_bytes(&mut out, CHUNK_DELTA_MAGIC);
    put_u16(&mut out, delta.schema_version);
    out.push(encoding);
    out.push(0); // reserved, must remain zero for schema 1
    put_u16(&mut out, delta.worldgen_epoch.len() as u16);
    put_bytes(&mut out, delta.worldgen_epoch.as_bytes());
    put_i32(&mut out, delta.dim_id);
    put_i32(&mut out, delta.cx);
    put_i32(&mut out, delta.cz);
    put_u32(&mut out, edit_count as u32);
    put_u32(&mut out, delta.unknown_blocks.len() as u32);

    if encoding == ENCODING_RUNS {
        put_u32(&mut out, run_count as u32);
        let mut index = 0usize;
        while index < delta.edits.len() {
            let start = delta.edits[index].local_idx;
            let mut end = index + 1;
            while end < delta.edits.len()
                && delta.edits[end].local_idx as u32 == delta.edits[end - 1].local_idx as u32 + 1
            {
                end += 1;
            }
            let len = end - index;
            put_u16(&mut out, start);
            // Stored as len-1 so the complete 65_536-state index space is
            // representable without a zero-length-run ambiguity.
            put_u16(&mut out, (len - 1) as u16);
            for edit in &delta.edits[index..end] {
                put_u32(&mut out, edit.base_state);
                put_u32(&mut out, edit.new_state);
            }
            index = end;
        }
    } else {
        for edit in &delta.edits {
            put_u16(&mut out, edit.local_idx);
            put_u32(&mut out, edit.base_state);
            put_u32(&mut out, edit.new_state);
        }
    }

    for entry in &delta.unknown_blocks {
        put_u16(&mut out, entry.local_idx);
        put_u16(&mut out, entry.namespaced_id.len() as u16);
        put_bytes(&mut out, entry.namespaced_id.as_bytes());
        put_u32(&mut out, entry.properties_json.len() as u32);
        put_bytes(&mut out, entry.properties_json.as_bytes());
        put_u32(&mut out, entry.nbt_bytes.len() as u32);
        put_bytes(&mut out, &entry.nbt_bytes);
    }

    let crc = megu_core::hash::crc32_ieee(&out);
    put_u32(&mut out, crc);
    out
}

fn delta_oversize_count(field: &'static str, count: u32, cap: u32) -> DeltaError {
    DeltaError::OversizedCount { field, count, cap }
}

fn delta_oversize_len(field: &'static str, len: usize, cap: usize) -> DeltaError {
    DeltaError::OversizedLength { field, len, cap }
}

/// Decode and validate an `MDW1` chunk delta. CRC32 is verified before any
/// field is trusted, and malformed payloads fail closed rather than producing
/// partial edits.
pub fn decode_chunk_delta(bytes: &[u8]) -> Result<ChunkDelta, DeltaError> {
    if bytes.len() > MAX_DELTA_BLOB_BYTES {
        return Err(delta_oversize_len(
            "delta_blob",
            bytes.len(),
            MAX_DELTA_BLOB_BYTES,
        ));
    }
    let (body, trailer) = bytes.split_last_chunk().ok_or(DeltaError::Truncated)?;
    let (magic, body_fields) = body.split_first_chunk().ok_or(DeltaError::Truncated)?;
    if magic != CHUNK_DELTA_MAGIC {
        return Err(DeltaError::BadMagic);
    }
    let expected = u32::from_be_bytes(*trailer);
    let actual = megu_core::hash::crc32_ieee(body);
    if actual != expected {
        return Err(DeltaError::CrcMismatch { expected, actual });
    }

    let mut reader = Reader::new(body_fields);
    let schema_version = reader.u16().map_err(|_| DeltaError::Truncated)?;
    if schema_version != DELTA_WORLD_SCHEMA_VERSION {
        return Err(DeltaError::UnsupportedSchemaVersion(schema_version));
    }
    let encoding = reader.u8().map_err(|_| DeltaError::Truncated)?;
    if encoding != ENCODING_EXPLICIT && encoding != ENCODING_RUNS {
        return Err(DeltaError::UnknownEncoding(encoding));
    }
    let reserved = reader.u8().map_err(|_| DeltaError::Truncated)?;
    if reserved != 0 {
        return Err(DeltaError::InvalidRun("reserved header byte is non-zero"));
    }
    let epoch_len = usize::from(reader.u16().map_err(|_| DeltaError::Truncated)?);
    if epoch_len > MAX_DELTA_STRING_BYTES {
        return Err(delta_oversize_len(
            "worldgen_epoch",
            epoch_len,
            MAX_DELTA_STRING_BYTES,
        ));
    }
    let epoch_bytes = reader.take(epoch_len).map_err(|_| DeltaError::Truncated)?;
    let worldgen_epoch = std::str::from_utf8(epoch_bytes)
        .map_err(|_| DeltaError::InvalidUtf8("worldgen_epoch"))?
        .to_string();
    let dim_id = reader.i32().map_err(|_| DeltaError::Truncated)?;
    let cx = reader.i32().map_err(|_| DeltaError::Truncated)?;
    let cz = reader.i32().map_err(|_| DeltaError::Truncated)?;
    let edit_count = reader.u32().map_err(|_| DeltaError::Truncated)?;
    if edit_count as usize > MAX_CHUNK_STATES {
        return Err(delta_oversize_count(
            "edit_count",
            edit_count,
            MAX_CHUNK_STATES as u32,
        ));
    }
    let unknown_count = reader.u32().map_err(|_| DeltaError::Truncated)?;
    if unknown_count as usize > MAX_CHUNK_STATES {
        return Err(delta_oversize_count(
            "unknown_count",
            unknown_count,
            MAX_CHUNK_STATES as u32,
        ));
    }

    let mut edits = Vec::with_capacity(edit_count as usize);
    let mut previous_index: Option<u16> = None;
    let mut validate_index = |local_idx: u16| -> Result<(), DeltaError> {
        if previous_index.is_some_and(|previous| local_idx <= previous) {
            return Err(DeltaError::UnsortedOrDuplicateIndex { local_idx });
        }
        previous_index = Some(local_idx);
        Ok(())
    };
    let mut push_edit = |local_idx: u16,
                         base_state: u32,
                         new_state: u32,
                         edits: &mut Vec<BlockDeltaEdit>|
     -> Result<(), DeltaError> {
        validate_index(local_idx)?;
        if base_state == new_state {
            return Err(DeltaError::NoOpEdit { local_idx });
        }
        edits.push(BlockDeltaEdit {
            local_idx,
            base_state,
            new_state,
        });
        Ok(())
    };

    if encoding == ENCODING_EXPLICIT {
        for _ in 0..edit_count {
            let local_idx = reader.u16().map_err(|_| DeltaError::Truncated)?;
            let base_state = reader.u32().map_err(|_| DeltaError::Truncated)?;
            let new_state = reader.u32().map_err(|_| DeltaError::Truncated)?;
            push_edit(local_idx, base_state, new_state, &mut edits)?;
        }
    } else {
        let run_count = reader.u32().map_err(|_| DeltaError::Truncated)?;
        if run_count > edit_count {
            return Err(DeltaError::InvalidRun("more runs than edits"));
        }
        let mut decoded_edits = 0u32;
        let mut previous_run_end: Option<u16> = None;
        for run_index in 0..run_count {
            let start = reader.u16().map_err(|_| DeltaError::Truncated)?;
            if run_index != 0
                && previous_run_end
                    .is_some_and(|previous| u32::from(previous) + 1 == u32::from(start))
            {
                return Err(DeltaError::InvalidRun(
                    "contiguous edits are split across multiple runs",
                ));
            }
            let len = u32::from(reader.u16().map_err(|_| DeltaError::Truncated)?) + 1;
            let end = u32::from(start) + len - 1;
            if end > u32::from(u16::MAX) {
                return Err(DeltaError::InvalidRun("run exceeds u16 index space"));
            }
            if decoded_edits + len > edit_count {
                return Err(DeltaError::InvalidRun(
                    "runs encode more edits than declared",
                ));
            }
            for offset in 0..len {
                let local_idx = (u32::from(start) + offset) as u16;
                let base_state = reader.u32().map_err(|_| DeltaError::Truncated)?;
                let new_state = reader.u32().map_err(|_| DeltaError::Truncated)?;
                push_edit(local_idx, base_state, new_state, &mut edits)?;
            }
            decoded_edits += len;
            previous_run_end = Some(end as u16);
        }
        if decoded_edits != edit_count {
            return Err(DeltaError::InvalidRun(
                "runs encode fewer edits than declared",
            ));
        }
    }

    let mut unknown_blocks = Vec::with_capacity(unknown_count as usize);
    let mut previous_unknown: Option<u16> = None;
    for _ in 0..unknown_count {
        let local_idx = reader.u16().map_err(|_| DeltaError::Truncated)?;
        if previous_unknown.is_some_and(|previous| local_idx <= previous) {
            return Err(DeltaError::UnsortedOrDuplicateIndex { local_idx });
        }
        previous_unknown = Some(local_idx);
        let id_len = usize::from(reader.u16().map_err(|_| DeltaError::Truncated)?);
        if id_len == 0 {
            return Err(DeltaError::InvalidStructure("namespaced_id is empty"));
        }
        if id_len > MAX_DELTA_STRING_BYTES {
            return Err(delta_oversize_len(
                "namespaced_id",
                id_len,
                MAX_DELTA_STRING_BYTES,
            ));
        }
        let id_bytes = reader.take(id_len).map_err(|_| DeltaError::Truncated)?;
        let namespaced_id = std::str::from_utf8(id_bytes)
            .map_err(|_| DeltaError::InvalidUtf8("namespaced_id"))?
            .to_string();
        let properties_len = reader.u32().map_err(|_| DeltaError::Truncated)? as usize;
        if properties_len > MAX_PROPERTIES_JSON_BYTES {
            return Err(delta_oversize_len(
                "properties_json",
                properties_len,
                MAX_PROPERTIES_JSON_BYTES,
            ));
        }
        let properties_bytes = reader
            .take(properties_len)
            .map_err(|_| DeltaError::Truncated)?;
        let properties_json = std::str::from_utf8(properties_bytes)
            .map_err(|_| DeltaError::InvalidUtf8("properties_json"))?
            .to_string();
        let nbt_len = reader.u32().map_err(|_| DeltaError::Truncated)? as usize;
        if nbt_len > MAX_UNKNOWN_NBT_BYTES {
            return Err(delta_oversize_len(
                "nbt_bytes",
                nbt_len,
                MAX_UNKNOWN_NBT_BYTES,
            ));
        }
        let nbt_bytes = reader
            .take(nbt_len)
            .map_err(|_| DeltaError::Truncated)?
            .to_vec();
        unknown_blocks.push(UnknownBlockEntry {
            local_idx,
            namespaced_id,
            properties_json,
            nbt_bytes,
        });
    }

    if reader.remaining() != 0 {
        return Err(DeltaError::TrailingBytes(reader.remaining()));
    }
    let canonical_encoding = canonical_edit_encoding(&edits);
    if encoding != canonical_encoding {
        return Err(DeltaError::InvalidStructure(
            "edit payload does not use the canonical beneficial encoding",
        ));
    }
    Ok(ChunkDelta {
        schema_version,
        worldgen_epoch,
        dim_id,
        cx,
        cz,
        edits,
        unknown_blocks,
    })
}

// ------------------------------------------------------------- store types

/// Whether a chunk generation entry references a blob or is exactly the
/// deterministic base terrain.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChunkStorage {
    /// The chunk has zero delta bytes. `content_hash` is SHA-256 of the empty
    /// byte string and no object exists under `blobs/` for it.
    DeterministicBase,
    /// The chunk references a deduplicated `MDW1` delta blob by content hash.
    DeltaBlob,
}

/// One chunk entry in an immutable generation record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GenerationChunkRef {
    pub dim_id: i32,
    pub cx: i32,
    pub cz: i32,
    pub storage: ChunkStorage,
    /// SHA-256 of the encoded delta blob, or SHA-256 of the empty byte string
    /// for a deterministic-base chunk.
    pub content_hash: Digest32,
    /// Serialized delta bytes represented by this entry; zero for base chunks.
    pub encoded_len: u32,
}

impl GenerationChunkRef {
    pub fn is_zero_bytes(&self) -> bool {
        self.storage == ChunkStorage::DeterministicBase && self.encoded_len == 0
    }
}

/// An immutable generation record. `parent_gen` records ancestry, but the
/// chunk list itself is a complete sparse overlay against deterministic base
/// terrain, so checkout never needs parent replay.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GenerationRecord {
    pub gen_id: u64,
    pub parent_gen: Option<u64>,
    pub worldgen_epoch: String,
    pub seed: u64,
    pub label: String,
    pub timestamp_tick: u64,
    pub merkle_root: Digest32,
    pub chunks: Vec<GenerationChunkRef>,
}

/// Inputs for [`DeltaGenerationStore::commit_generation`].
///
/// `pinned` defaults to `true`: generation data is expensive to recompute and
/// must not be garbage-collected unless the caller explicitly unpins it.
/// `allow_epoch_migration` defaults to `false`; a generation whose
/// `worldgen_epoch` differs from its parent or the store's active head is
/// otherwise refused fail-closed. Seeds never migrate inside one store.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommitGenerationOptions {
    pub parent_gen: Option<u64>,
    pub seed: u64,
    pub worldgen_epoch: String,
    pub label: String,
    pub timestamp_tick: u64,
    pub chunks: Vec<ChunkDelta>,
    pub pinned: bool,
    pub allow_epoch_migration: bool,
}

impl CommitGenerationOptions {
    pub fn new(
        seed: u64,
        worldgen_epoch: &str,
        label: &str,
        timestamp_tick: u64,
        chunks: Vec<ChunkDelta>,
    ) -> Self {
        Self {
            parent_gen: None,
            seed,
            worldgen_epoch: worldgen_epoch.to_string(),
            label: label.to_string(),
            timestamp_tick,
            chunks,
            pinned: true,
            allow_epoch_migration: false,
        }
    }

    pub fn with_parent(mut self, parent_gen: u64) -> Self {
        self.parent_gen = Some(parent_gen);
        self
    }

    pub fn pinned(mut self, pinned: bool) -> Self {
        self.pinned = pinned;
        self
    }

    pub fn allow_epoch_migration(mut self, allow: bool) -> Self {
        self.allow_epoch_migration = allow;
        self
    }
}

/// A materialized generation: the immutable record plus every chunk delta in
/// canonical chunk order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CheckedOutGeneration {
    pub record: GenerationRecord,
    pub chunks: Vec<ChunkDelta>,
}

/// One numeric block-state difference between two generations.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BlockStateDiff {
    pub local_idx: u16,
    pub generation_a_state: u32,
    pub generation_b_state: u32,
}

/// One opaque unknown-block difference between two generations.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnknownBlockDiff {
    pub local_idx: u16,
    pub generation_a: Option<UnknownBlockEntry>,
    pub generation_b: Option<UnknownBlockEntry>,
}

/// Exact differences for one chunk key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChunkGenerationDiff {
    pub dim_id: i32,
    pub cx: i32,
    pub cz: i32,
    pub block_diffs: Vec<BlockStateDiff>,
    pub unknown_diffs: Vec<UnknownBlockDiff>,
}

/// Exact sparse-overlay diff between two same-epoch, same-seed generations.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GenerationDiff {
    pub gen_a: u64,
    pub gen_b: u64,
    pub chunks: Vec<ChunkGenerationDiff>,
}

impl GenerationDiff {
    pub fn is_empty(&self) -> bool {
        self.chunks.is_empty()
    }
}

/// Garbage-collection statistics for [`DeltaGenerationStore::gc_unreferenced_blobs`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DeltaGcReport {
    pub scanned_blobs: usize,
    pub retained_blobs: usize,
    pub removed_blobs: usize,
    pub removed_bytes: u64,
}

/// Why a generation-store operation failed.
#[derive(Debug)]
pub enum DeltaStoreError {
    Io {
        op: &'static str,
        path: PathBuf,
        source: io::Error,
    },
    Delta(DeltaError),
    InvalidInput(String),
    OversizedFile {
        path: PathBuf,
        len: u64,
        cap: u64,
    },
    ManifestMalformed(String),
    GenerationMalformed {
        gen_id: u64,
        reason: String,
    },
    GenerationNotFound(u64),
    ParentGenerationNotFound(u64),
    GenerationIdConflict(u64),
    DuplicateChunkKey {
        dim_id: i32,
        cx: i32,
        cz: i32,
    },
    ChunkKeyCollision {
        dim_id: i32,
        cx: i32,
        cz: i32,
    },
    EpochMismatch {
        expected: String,
        actual: String,
    },
    SeedMismatch {
        expected: u64,
        actual: u64,
    },
    MissingBlob {
        content_hash: String,
    },
    BlobIntegrity {
        expected_hash: String,
        actual_hash: String,
    },
    BlobSizeMismatch {
        content_hash: String,
        expected_len: u64,
        actual_len: u64,
    },
    InconsistentChunkMetadata {
        dim_id: i32,
        cx: i32,
        cz: i32,
        reason: &'static str,
    },
    InconsistentBaseState {
        dim_id: i32,
        cx: i32,
        cz: i32,
        local_idx: u16,
    },
}

impl fmt::Display for DeltaStoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        use DeltaStoreError::*;
        match self {
            Io { op, path, source } => write!(f, "{op} failed on {}: {source}", path.display()),
            Delta(error) => write!(f, "chunk delta error: {error}"),
            InvalidInput(reason) => write!(f, "invalid generation input: {reason}"),
            OversizedFile { path, len, cap } => write!(
                f,
                "stored file {} is {len} bytes, exceeding the {cap}-byte cap",
                path.display()
            ),
            ManifestMalformed(reason) => write!(f, "generation manifest is invalid: {reason}"),
            GenerationMalformed { gen_id, reason } => {
                write!(f, "generation {gen_id:016x} is invalid: {reason}")
            }
            GenerationNotFound(gen_id) => write!(f, "generation {gen_id:016x} does not exist"),
            ParentGenerationNotFound(gen_id) => {
                write!(f, "parent generation {gen_id:016x} does not exist")
            }
            GenerationIdConflict(gen_id) => write!(
                f,
                "generation id {gen_id:016x} already names different immutable content"
            ),
            DuplicateChunkKey { dim_id, cx, cz } => write!(
                f,
                "duplicate chunk key (dim {dim_id}, cx {cx}, cz {cz}) in one generation"
            ),
            ChunkKeyCollision { dim_id, cx, cz } => write!(
                f,
                "Merkle key collision at chunk (dim {dim_id}, cx {cx}, cz {cz})"
            ),
            EpochMismatch { expected, actual } => write!(
                f,
                "worldgen_epoch mismatch: expected {expected:?}, got {actual:?}; explicit migration is required"
            ),
            SeedMismatch { expected, actual } => write!(
                f,
                "world seed mismatch: expected {expected}, got {actual}"
            ),
            MissingBlob { content_hash } => {
                write!(f, "delta blob {content_hash} is missing from the store")
            }
            BlobIntegrity {
                expected_hash,
                actual_hash,
            } => write!(
                f,
                "delta blob integrity failure: expected sha256 {expected_hash}, computed {actual_hash}"
            ),
            BlobSizeMismatch {
                content_hash,
                expected_len,
                actual_len,
            } => write!(
                f,
                "delta blob {content_hash} has length {actual_len}, generation record recorded {expected_len}"
            ),
            InconsistentChunkMetadata { dim_id, cx, cz, reason } => write!(
                f,
                "chunk (dim {dim_id}, cx {cx}, cz {cz}) metadata mismatch: {reason}"
            ),
            InconsistentBaseState {
                dim_id,
                cx,
                cz,
                local_idx,
            } => write!(
                f,
                "two same-epoch generations disagree about the base state at \
                 (dim {dim_id}, cx {cx}, cz {cz}, local {local_idx})"
            ),
        }
    }
}

impl Error for DeltaStoreError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            DeltaStoreError::Io { source, .. } => Some(source),
            DeltaStoreError::Delta(error) => Some(error),
            _ => None,
        }
    }
}

impl From<DeltaError> for DeltaStoreError {
    fn from(error: DeltaError) -> Self {
        DeltaStoreError::Delta(error)
    }
}

fn store_ioe(op: &'static str, path: &Path) -> impl FnOnce(io::Error) -> DeltaStoreError {
    let path = path.to_path_buf();
    move |source| DeltaStoreError::Io { op, path, source }
}

fn read_file_capped(path: &Path, cap: u64, op: &'static str) -> Result<Vec<u8>, DeltaStoreError> {
    let len = std::fs::metadata(path).map_err(store_ioe(op, path))?.len();
    if len > cap {
        return Err(DeltaStoreError::OversizedFile {
            path: path.to_path_buf(),
            len,
            cap,
        });
    }
    let bytes = std::fs::read(path).map_err(store_ioe(op, path))?;
    let len = bytes.len() as u64;
    if len > cap {
        return Err(DeltaStoreError::OversizedFile {
            path: path.to_path_buf(),
            len,
            cap,
        });
    }
    Ok(bytes)
}

fn normalize_store_root(root: &Path) -> PathBuf {
    if root.as_os_str().is_empty() {
        PathBuf::from(".")
    } else {
        root.to_path_buf()
    }
}

fn generation_file_name(gen_id: u64) -> String {
    format!("{gen_id:016x}.mdwg")
}

fn generation_path(root: &Path, gen_id: u64) -> PathBuf {
    root.join("generations").join(generation_file_name(gen_id))
}

fn manifest_path(root: &Path) -> PathBuf {
    root.join("manifest.mgm1")
}

fn blob_path(root: &Path, content_hash: &Digest32) -> PathBuf {
    let hex = hex_encode(content_hash);
    root.join("blobs").join(&hex[..2]).join(hex)
}

fn empty_hash() -> Digest32 {
    sha256(&[])
}

fn chunk_merkle_key(dim_id: i32, cx: i32, cz: i32) -> u64 {
    let mut preimage = Vec::with_capacity(15 + 12);
    put_bytes(&mut preimage, b"MDW1-CHUNK-KEY");
    put_i32(&mut preimage, dim_id);
    put_i32(&mut preimage, cx);
    put_i32(&mut preimage, cz);
    sha256_prefix(&preimage)
}

fn merkle_root_for_chunks(chunks: &[GenerationChunkRef]) -> Result<Digest32, DeltaStoreError> {
    let mut leaves = Vec::with_capacity(chunks.len());
    let mut merkle_keys = BTreeMap::new();
    for chunk in chunks {
        let key = (chunk.dim_id, chunk.cx, chunk.cz);
        let merkle_key = chunk_merkle_key(key.0, key.1, key.2);
        if let Some(previous) = merkle_keys.insert(merkle_key, key) {
            if previous != key {
                return Err(DeltaStoreError::ChunkKeyCollision {
                    dim_id: key.0,
                    cx: key.1,
                    cz: key.2,
                });
            }
        }
        leaves.push((merkle_key, chunk.content_hash));
    }
    Ok(MerkleTree::new(&leaves).root())
}

fn validate_generation_text(worldgen_epoch: &str, label: &str) -> Result<(), DeltaStoreError> {
    if worldgen_epoch.is_empty() {
        return Err(DeltaStoreError::InvalidInput(
            "worldgen_epoch must not be empty".to_string(),
        ));
    }
    if worldgen_epoch.len() > MAX_DELTA_STRING_BYTES {
        return Err(DeltaStoreError::InvalidInput(format!(
            "worldgen_epoch length {} exceeds {MAX_DELTA_STRING_BYTES}",
            worldgen_epoch.len()
        )));
    }
    if label.len() > MAX_LABEL_BYTES {
        return Err(DeltaStoreError::InvalidInput(format!(
            "label length {} exceeds {MAX_LABEL_BYTES}",
            label.len()
        )));
    }
    Ok(())
}

fn generation_preimage(
    parent_gen: Option<u64>,
    seed: u64,
    worldgen_epoch: &str,
    label: &str,
    timestamp_tick: u64,
    merkle_root: &Digest32,
    chunks: &[GenerationChunkRef],
) -> Vec<u8> {
    let mut out = Vec::new();
    put_bytes(&mut out, b"MEGU-DELTA-GENERATION-ID-v1");
    out.push(u8::from(parent_gen.is_some()));
    put_u64(&mut out, parent_gen.unwrap_or(0));
    put_u64(&mut out, seed);
    put_u64(&mut out, timestamp_tick);
    put_u16(&mut out, worldgen_epoch.len() as u16);
    put_bytes(&mut out, worldgen_epoch.as_bytes());
    put_u16(&mut out, label.len() as u16);
    put_bytes(&mut out, label.as_bytes());
    put_bytes(&mut out, merkle_root);
    put_u32(&mut out, chunks.len() as u32);
    for chunk in chunks {
        put_i32(&mut out, chunk.dim_id);
        put_i32(&mut out, chunk.cx);
        put_i32(&mut out, chunk.cz);
        out.push(match chunk.storage {
            ChunkStorage::DeterministicBase => STORAGE_BASE,
            ChunkStorage::DeltaBlob => STORAGE_BLOB,
        });
        put_u32(&mut out, chunk.encoded_len);
        put_bytes(&mut out, &chunk.content_hash);
    }
    out
}

fn sha256_prefix(parts: &[u8]) -> u64 {
    let [a, b, c, d, e, f, g, h, ..] = sha256(parts);
    u64::from_be_bytes([a, b, c, d, e, f, g, h])
}

fn encode_generation_record(record: &GenerationRecord) -> Vec<u8> {
    let mut out = Vec::new();
    put_bytes(&mut out, GENERATION_MAGIC);
    put_u16(&mut out, DELTA_WORLD_SCHEMA_VERSION);
    put_u64(&mut out, record.gen_id);
    out.push(u8::from(record.parent_gen.is_some()));
    put_u64(&mut out, record.parent_gen.unwrap_or(0));
    put_u64(&mut out, record.seed);
    put_u64(&mut out, record.timestamp_tick);
    put_u16(&mut out, record.worldgen_epoch.len() as u16);
    put_bytes(&mut out, record.worldgen_epoch.as_bytes());
    put_u16(&mut out, record.label.len() as u16);
    put_bytes(&mut out, record.label.as_bytes());
    put_bytes(&mut out, &record.merkle_root);
    put_u32(&mut out, record.chunks.len() as u32);
    for chunk in &record.chunks {
        put_i32(&mut out, chunk.dim_id);
        put_i32(&mut out, chunk.cx);
        put_i32(&mut out, chunk.cz);
        out.push(match chunk.storage {
            ChunkStorage::DeterministicBase => STORAGE_BASE,
            ChunkStorage::DeltaBlob => STORAGE_BLOB,
        });
        put_u32(&mut out, chunk.encoded_len);
        put_bytes(&mut out, &chunk.content_hash);
    }
    let crc = megu_core::hash::crc32_ieee(&out);
    put_u32(&mut out, crc);
    out
}

fn decode_generation_record(
    gen_id_hint: u64,
    bytes: &[u8],
) -> Result<GenerationRecord, DeltaStoreError> {
    let malformed = |reason: &str| DeltaStoreError::GenerationMalformed {
        gen_id: gen_id_hint,
        reason: reason.to_string(),
    };
    if bytes.len() > MAX_GENERATION_RECORD_BYTES {
        return Err(malformed("generation record exceeds the 128 MiB cap"));
    }
    let (body, trailer) = bytes
        .split_last_chunk()
        .ok_or_else(|| malformed("truncated before CRC trailer"))?;
    let (magic, body_fields) = body
        .split_first_chunk()
        .ok_or_else(|| malformed("truncated before magic"))?;
    if magic != GENERATION_MAGIC {
        return Err(malformed("bad generation magic"));
    }
    let expected = u32::from_be_bytes(*trailer);
    let actual = megu_core::hash::crc32_ieee(body);
    if expected != actual {
        return Err(malformed(&format!(
            "CRC32 mismatch: stored {expected:#010x}, computed {actual:#010x}"
        )));
    }
    let mut reader = Reader::new(body_fields);
    let schema = reader.u16().map_err(|_| malformed("truncated schema"))?;
    if schema != DELTA_WORLD_SCHEMA_VERSION {
        return Err(malformed(&format!("unsupported schema version {schema}")));
    }
    let gen_id = reader
        .u64()
        .map_err(|_| malformed("truncated generation id"))?;
    let parent_present = reader.u8().map_err(|_| malformed("truncated parent tag"))?;
    if parent_present > 1 {
        return Err(malformed("parent tag is not boolean"));
    }
    let parent_raw = reader.u64().map_err(|_| malformed("truncated parent id"))?;
    if parent_present == 0 && parent_raw != 0 {
        return Err(malformed("absent parent has a non-zero reserved id"));
    }
    let parent_gen = parent_present.eq(&1).then_some(parent_raw);
    let seed = reader.u64().map_err(|_| malformed("truncated seed"))?;
    let timestamp_tick = reader.u64().map_err(|_| malformed("truncated timestamp"))?;
    let epoch_len = usize::from(
        reader
            .u16()
            .map_err(|_| malformed("truncated epoch length"))?,
    );
    if epoch_len == 0 || epoch_len > MAX_DELTA_STRING_BYTES {
        return Err(malformed("worldgen_epoch length outside bounds"));
    }
    let epoch_bytes = reader
        .take(epoch_len)
        .map_err(|_| malformed("truncated epoch"))?;
    let worldgen_epoch = std::str::from_utf8(epoch_bytes)
        .map_err(|_| malformed("worldgen_epoch is not UTF-8"))?
        .to_string();
    let label_len = usize::from(
        reader
            .u16()
            .map_err(|_| malformed("truncated label length"))?,
    );
    if label_len > MAX_LABEL_BYTES {
        return Err(malformed("label length exceeds cap"));
    }
    let label_bytes = reader
        .take(label_len)
        .map_err(|_| malformed("truncated label"))?;
    let label = std::str::from_utf8(label_bytes)
        .map_err(|_| malformed("label is not UTF-8"))?
        .to_string();
    let merkle_root: Digest32 = reader
        .array()
        .map_err(|_| malformed("truncated merkle root"))?;
    let chunk_count = reader
        .u32()
        .map_err(|_| malformed("truncated chunk count"))? as usize;
    if chunk_count > MAX_GENERATION_CHUNKS {
        return Err(malformed("chunk count exceeds cap"));
    }
    const CHUNK_REF_BYTES: usize = 12 + 1 + 4 + 32;
    if chunk_count.checked_mul(CHUNK_REF_BYTES) != Some(reader.remaining()) {
        return Err(malformed("chunk reference table length mismatch"));
    }
    let empty = empty_hash();
    let mut chunks = Vec::with_capacity(chunk_count);
    let mut previous_key: Option<(i32, i32, i32)> = None;
    for _ in 0..chunk_count {
        let dim_id = reader.i32().map_err(|_| malformed("truncated chunk dim"))?;
        let cx = reader.i32().map_err(|_| malformed("truncated chunk cx"))?;
        let cz = reader.i32().map_err(|_| malformed("truncated chunk cz"))?;
        let key = (dim_id, cx, cz);
        if previous_key.is_some_and(|previous| key <= previous) {
            return Err(malformed("chunk entries are not strictly sorted"));
        }
        previous_key = Some(key);
        let storage_tag = reader
            .u8()
            .map_err(|_| malformed("truncated storage tag"))?;
        let storage = match storage_tag {
            STORAGE_BASE => ChunkStorage::DeterministicBase,
            STORAGE_BLOB => ChunkStorage::DeltaBlob,
            _ => return Err(malformed("unknown chunk storage tag")),
        };
        let encoded_len = reader
            .u32()
            .map_err(|_| malformed("truncated blob length"))?;
        let content_hash: Digest32 = reader
            .array()
            .map_err(|_| malformed("truncated content hash"))?;
        match storage {
            ChunkStorage::DeterministicBase => {
                if encoded_len != 0 || content_hash != empty {
                    return Err(malformed(
                        "base chunk does not use canonical empty content identity",
                    ));
                }
            }
            ChunkStorage::DeltaBlob => {
                if encoded_len == 0 {
                    return Err(malformed("blob chunk has zero encoded length"));
                }
                if encoded_len as usize > MAX_DELTA_BLOB_BYTES {
                    return Err(malformed("blob chunk encoded length exceeds the delta cap"));
                }
            }
        }
        chunks.push(GenerationChunkRef {
            dim_id,
            cx,
            cz,
            storage,
            content_hash,
            encoded_len,
        });
    }
    if reader.remaining() != 0 {
        return Err(malformed("trailing bytes before CRC trailer"));
    }
    let recomputed_merkle = merkle_root_for_chunks(&chunks)
        .map_err(|_| malformed("chunk keys collide in the Merkle key space"))?;
    if recomputed_merkle != merkle_root {
        return Err(malformed("merkle root does not match the chunk references"));
    }
    let preimage = generation_preimage(
        parent_gen,
        seed,
        &worldgen_epoch,
        &label,
        timestamp_tick,
        &merkle_root,
        &chunks,
    );
    let recomputed = sha256_prefix(&preimage);
    if recomputed != gen_id {
        return Err(malformed(&format!(
            "embedded generation id {gen_id:016x} does not match content id {recomputed:016x}"
        )));
    }
    if gen_id_hint != gen_id {
        return Err(malformed(&format!(
            "file name id {gen_id_hint:016x} does not match embedded id {gen_id:016x}"
        )));
    }
    Ok(GenerationRecord {
        gen_id,
        parent_gen,
        worldgen_epoch,
        seed,
        label,
        timestamp_tick,
        merkle_root,
        chunks,
    })
}

fn encode_manifest(active_generation: Option<u64>, entries: &BTreeMap<u64, bool>) -> Vec<u8> {
    let mut out = Vec::new();
    put_bytes(&mut out, MANIFEST_MAGIC);
    put_u16(&mut out, DELTA_WORLD_SCHEMA_VERSION);
    out.push(u8::from(active_generation.is_some()));
    put_u64(&mut out, active_generation.unwrap_or(0));
    put_u32(&mut out, entries.len() as u32);
    for (&gen_id, &pinned) in entries {
        put_u64(&mut out, gen_id);
        out.push(u8::from(pinned));
    }
    let crc = megu_core::hash::crc32_ieee(&out);
    put_u32(&mut out, crc);
    out
}

fn decode_manifest(bytes: &[u8]) -> Result<(Option<u64>, BTreeMap<u64, bool>), DeltaStoreError> {
    let malformed = |reason: &str| DeltaStoreError::ManifestMalformed(reason.to_string());
    if bytes.len() > MAX_MANIFEST_BYTES {
        return Err(malformed("manifest exceeds the 64 MiB cap"));
    }
    let (body, trailer) = bytes
        .split_last_chunk()
        .ok_or_else(|| malformed("truncated before CRC trailer"))?;
    let (magic, body_fields) = body
        .split_first_chunk()
        .ok_or_else(|| malformed("truncated before magic"))?;
    if magic != MANIFEST_MAGIC {
        return Err(malformed("bad manifest magic"));
    }
    let expected = u32::from_be_bytes(*trailer);
    let actual = megu_core::hash::crc32_ieee(body);
    if expected != actual {
        return Err(malformed(&format!(
            "CRC32 mismatch: stored {expected:#010x}, computed {actual:#010x}"
        )));
    }
    let mut reader = Reader::new(body_fields);
    let schema = reader.u16().map_err(|_| malformed("truncated schema"))?;
    if schema != DELTA_WORLD_SCHEMA_VERSION {
        return Err(malformed(&format!("unsupported schema version {schema}")));
    }
    let active_present = reader.u8().map_err(|_| malformed("truncated active tag"))?;
    if active_present > 1 {
        return Err(malformed("active tag is not boolean"));
    }
    let active_raw = reader.u64().map_err(|_| malformed("truncated active id"))?;
    if active_present == 0 && active_raw != 0 {
        return Err(malformed(
            "absent active generation has a non-zero reserved id",
        ));
    }
    let active_generation = active_present.eq(&1).then_some(active_raw);
    let count = reader
        .u32()
        .map_err(|_| malformed("truncated generation count"))? as usize;
    if count > MAX_MANIFEST_GENERATIONS {
        return Err(malformed("generation count exceeds cap"));
    }
    let mut entries = BTreeMap::new();
    let mut previous: Option<u64> = None;
    for _ in 0..count {
        let gen_id = reader
            .u64()
            .map_err(|_| malformed("truncated generation id"))?;
        if previous.is_some_and(|old| gen_id <= old) {
            return Err(malformed("generation ids are not strictly sorted"));
        }
        previous = Some(gen_id);
        let pinned = reader.u8().map_err(|_| malformed("truncated pin flag"))?;
        if pinned > 1 {
            return Err(malformed("pin flag is not boolean"));
        }
        entries.insert(gen_id, pinned == 1);
    }
    if reader.remaining() != 0 {
        return Err(malformed("trailing bytes before CRC trailer"));
    }
    if active_generation.is_some_and(|gen_id| !entries.contains_key(&gen_id)) {
        return Err(malformed("active generation is absent from the index"));
    }
    Ok((active_generation, entries))
}

fn valid_hex_digest_name(name: &str) -> bool {
    name.len() == 64
        && name
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

/// Content-addressed immutable generation store rooted at one directory.
///
/// Layout:
///
/// ```text
/// <root>/
///   blobs/<sha256[0..2]>/<sha256>       deduplicated nonzero MDW1 blobs
///   generations/<gen-id>.mdwg           immutable CRC32-sealed generation
///   manifest.mgm1                       atomic active/pin index
/// ```
///
/// The store is synchronous and single-instance; callers that share it across
/// threads should wrap it in their own synchronization, as with the crate's
/// other owned state stores.
pub struct DeltaGenerationStore {
    root: PathBuf,
    records: BTreeMap<u64, GenerationRecord>,
    pinned: BTreeSet<u64>,
    active_generation: Option<u64>,
}

impl DeltaGenerationStore {
    /// Open or create a generation store. `root` may be absolute or a bare
    /// relative directory path; an empty path is normalized to `.` before any
    /// atomic manifest/blob write so the parent-directory fsync never targets
    /// the empty path.
    pub fn open(root: impl AsRef<Path>) -> Result<Self, DeltaStoreError> {
        let root = normalize_store_root(root.as_ref());
        std::fs::create_dir_all(root.join("blobs"))
            .map_err(store_ioe("create blobs dir", &root))?;
        std::fs::create_dir_all(root.join("generations"))
            .map_err(store_ioe("create generations dir", &root))?;
        let path = manifest_path(&root);
        let (active_generation, entries) =
            match read_file_capped(&path, MAX_MANIFEST_BYTES as u64, "read generation manifest") {
                Ok(bytes) => decode_manifest(&bytes)?,
                Err(DeltaStoreError::Io { source, .. })
                    if source.kind() == io::ErrorKind::NotFound =>
                {
                    (None, BTreeMap::new())
                }
                Err(error) => return Err(error),
            };
        let mut records = BTreeMap::new();
        let mut pinned = BTreeSet::new();
        for (&gen_id, &is_pinned) in &entries {
            let path = generation_path(&root, gen_id);
            let bytes = read_file_capped(
                &path,
                MAX_GENERATION_RECORD_BYTES as u64,
                "read generation record",
            )?;
            let record = decode_generation_record(gen_id, &bytes)?;
            records.insert(gen_id, record);
            if is_pinned {
                pinned.insert(gen_id);
            }
        }
        Ok(Self {
            root,
            records,
            pinned,
            active_generation,
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn active_generation(&self) -> Option<u64> {
        self.active_generation
    }

    pub fn is_pinned(&self, gen_id: u64) -> bool {
        self.pinned.contains(&gen_id)
    }

    pub fn generation(&self, gen_id: u64) -> Option<&GenerationRecord> {
        self.records.get(&gen_id)
    }

    /// Immutable generation records in ascending id order.
    pub fn list_generations(&self) -> Vec<&GenerationRecord> {
        self.records.values().collect()
    }

    fn manifest_entries(&self) -> BTreeMap<u64, bool> {
        self.records
            .keys()
            .map(|&gen_id| (gen_id, self.pinned.contains(&gen_id)))
            .collect()
    }

    fn write_manifest_with(
        root: &Path,
        active_generation: Option<u64>,
        entries: &BTreeMap<u64, bool>,
    ) -> Result<(), DeltaStoreError> {
        let bytes = encode_manifest(active_generation, entries);
        let path = manifest_path(root);
        write_file_atomic(&path, &bytes).map_err(store_ioe("write generation manifest", &path))
    }

    fn write_manifest(&self) -> Result<(), DeltaStoreError> {
        Self::write_manifest_with(&self.root, self.active_generation, &self.manifest_entries())
    }

    fn install_blob(&self, encoded: &[u8], content_hash: &Digest32) -> Result<(), DeltaStoreError> {
        let path = blob_path(&self.root, content_hash);
        match std::fs::metadata(&path) {
            Ok(metadata) => {
                let actual_len = metadata.len();
                if actual_len != encoded.len() as u64 {
                    return Err(DeltaStoreError::BlobSizeMismatch {
                        content_hash: hex_encode(content_hash),
                        expected_len: encoded.len() as u64,
                        actual_len,
                    });
                }
                let existing = read_file_capped(
                    &path,
                    MAX_DELTA_BLOB_BYTES as u64,
                    "read existing delta blob",
                )?;
                let actual = sha256(&existing);
                if actual != *content_hash || existing != encoded {
                    return Err(DeltaStoreError::BlobIntegrity {
                        expected_hash: hex_encode(content_hash),
                        actual_hash: hex_encode(&actual),
                    });
                }
                return Ok(());
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(store_ioe("stat delta blob", &path)(error)),
        }
        let parent = path
            .parent()
            .expect("blob path always has a prefix directory");
        std::fs::create_dir_all(parent).map_err(store_ioe("create blob prefix", parent))?;
        write_file_atomic(&path, encoded).map_err(store_ioe("write delta blob", &path))
    }

    /// Commit a complete sparse overlay as a new immutable generation.
    ///
    /// The operation is ordered for crash safety: new content-addressed blobs
    /// are atomically installed first, then the immutable generation record,
    /// and only then the active/index manifest is atomically replaced. A crash
    /// before the final rename can orphan blobs, but never publishes a
    /// generation whose blobs are absent; `gc_unreferenced_blobs` can collect
    /// such orphans safely.
    pub fn commit_generation(
        &mut self,
        options: CommitGenerationOptions,
    ) -> Result<GenerationRecord, DeltaStoreError> {
        validate_generation_text(&options.worldgen_epoch, &options.label)?;
        if options.chunks.len() > MAX_GENERATION_CHUNKS {
            return Err(DeltaStoreError::InvalidInput(format!(
                "chunk count {} exceeds {MAX_GENERATION_CHUNKS}",
                options.chunks.len()
            )));
        }
        if let Some(parent_id) = options.parent_gen {
            let parent = self
                .records
                .get(&parent_id)
                .ok_or(DeltaStoreError::ParentGenerationNotFound(parent_id))?;
            if parent.seed != options.seed {
                return Err(DeltaStoreError::SeedMismatch {
                    expected: parent.seed,
                    actual: options.seed,
                });
            }
            if parent.worldgen_epoch != options.worldgen_epoch && !options.allow_epoch_migration {
                return Err(DeltaStoreError::EpochMismatch {
                    expected: parent.worldgen_epoch.clone(),
                    actual: options.worldgen_epoch.clone(),
                });
            }
        }

        if let Some(active_id) = self.active_generation {
            let active = self
                .records
                .get(&active_id)
                .ok_or(DeltaStoreError::GenerationNotFound(active_id))?;
            if active.seed != options.seed {
                return Err(DeltaStoreError::SeedMismatch {
                    expected: active.seed,
                    actual: options.seed,
                });
            }
            if active.worldgen_epoch != options.worldgen_epoch && !options.allow_epoch_migration {
                return Err(DeltaStoreError::EpochMismatch {
                    expected: active.worldgen_epoch.clone(),
                    actual: options.worldgen_epoch.clone(),
                });
            }
        }

        struct PreparedChunk {
            delta: ChunkDelta,
            encoded: Vec<u8>,
            content_hash: Digest32,
        }

        let mut prepared = Vec::with_capacity(options.chunks.len());
        let mut keys = BTreeSet::new();
        for original in &options.chunks {
            validate_chunk_delta(original)?;
            if original.worldgen_epoch != options.worldgen_epoch {
                return Err(DeltaStoreError::EpochMismatch {
                    expected: options.worldgen_epoch.clone(),
                    actual: original.worldgen_epoch.clone(),
                });
            }
            let key = (original.dim_id, original.cx, original.cz);
            if !keys.insert(key) {
                return Err(DeltaStoreError::DuplicateChunkKey {
                    dim_id: key.0,
                    cx: key.1,
                    cz: key.2,
                });
            }
            let delta = original.canonicalized();
            let encoded = if delta.is_zero_bytes() {
                Vec::new()
            } else {
                encode_chunk_delta(&delta)
            };
            let content_hash = sha256(&encoded);
            prepared.push(PreparedChunk {
                delta,
                encoded,
                content_hash,
            });
        }
        prepared.sort_by_key(|chunk| (chunk.delta.dim_id, chunk.delta.cx, chunk.delta.cz));

        let mut refs = Vec::with_capacity(prepared.len());
        for chunk in &prepared {
            let key = (chunk.delta.dim_id, chunk.delta.cx, chunk.delta.cz);
            let storage = if chunk.encoded.is_empty() {
                ChunkStorage::DeterministicBase
            } else {
                ChunkStorage::DeltaBlob
            };
            let encoded_len = chunk.encoded.len() as u32;
            refs.push(GenerationChunkRef {
                dim_id: key.0,
                cx: key.1,
                cz: key.2,
                storage,
                content_hash: chunk.content_hash,
                encoded_len,
            });
        }
        let merkle_root = merkle_root_for_chunks(&refs)?;
        let preimage = generation_preimage(
            options.parent_gen,
            options.seed,
            &options.worldgen_epoch,
            &options.label,
            options.timestamp_tick,
            &merkle_root,
            &refs,
        );
        let gen_id = sha256_prefix(&preimage);
        let record = GenerationRecord {
            gen_id,
            parent_gen: options.parent_gen,
            worldgen_epoch: options.worldgen_epoch,
            seed: options.seed,
            label: options.label,
            timestamp_tick: options.timestamp_tick,
            merkle_root,
            chunks: refs,
        };

        if let Some(existing) = self.records.get(&gen_id) {
            if existing != &record {
                return Err(DeltaStoreError::GenerationIdConflict(gen_id));
            }
        }

        // Recommitting identical content is a no-op logically, but it still
        // verifies and repairs the content-addressed objects it names. In
        // particular, a generation whose unpinned blobs were collected can be
        // resurrected by committing the same deterministic content again.
        for chunk in &prepared {
            if !chunk.encoded.is_empty() {
                self.install_blob(&chunk.encoded, &chunk.content_hash)?;
            }
        }
        let path = generation_path(&self.root, gen_id);
        let encoded_record = encode_generation_record(&record);
        if path.exists() {
            let existing = read_file_capped(
                &path,
                MAX_GENERATION_RECORD_BYTES as u64,
                "read generation record",
            )?;
            let decoded = decode_generation_record(gen_id, &existing)?;
            if decoded != record {
                return Err(DeltaStoreError::GenerationIdConflict(gen_id));
            }
        } else {
            write_file_atomic(&path, &encoded_record)
                .map_err(store_ioe("write generation record", &path))?;
        }

        let mut new_records = self.records.clone();
        new_records.insert(gen_id, record.clone());
        let mut new_pinned = self.pinned.clone();
        if options.pinned {
            new_pinned.insert(gen_id);
        }
        let entries: BTreeMap<u64, bool> = new_records
            .keys()
            .map(|&id| (id, new_pinned.contains(&id)))
            .collect();
        Self::write_manifest_with(&self.root, Some(gen_id), &entries)?;
        self.records = new_records;
        self.pinned = new_pinned;
        self.active_generation = Some(gen_id);
        Ok(record)
    }

    /// Materialize a generation without changing the active head. If the
    /// active head belongs to another `worldgen_epoch`, the checkout is
    /// refused fail-closed unless `allow_epoch_migration` is true.
    pub fn checkout_generation_with_migration(
        &self,
        target_gen_id: u64,
        allow_epoch_migration: bool,
    ) -> Result<CheckedOutGeneration, DeltaStoreError> {
        let record = self
            .records
            .get(&target_gen_id)
            .ok_or(DeltaStoreError::GenerationNotFound(target_gen_id))?
            .clone();
        if let Some(active_id) = self.active_generation {
            let active = self
                .records
                .get(&active_id)
                .ok_or(DeltaStoreError::GenerationNotFound(active_id))?;
            if active.seed != record.seed {
                return Err(DeltaStoreError::SeedMismatch {
                    expected: active.seed,
                    actual: record.seed,
                });
            }
            if active.worldgen_epoch != record.worldgen_epoch && !allow_epoch_migration {
                return Err(DeltaStoreError::EpochMismatch {
                    expected: active.worldgen_epoch.clone(),
                    actual: record.worldgen_epoch.clone(),
                });
            }
        }
        self.materialize_generation(record)
    }

    /// Materialize a generation, refusing cross-epoch time travel. Use
    /// [`Self::checkout_generation_with_migration`] for an explicit migration.
    pub fn checkout_generation(
        &self,
        target_gen_id: u64,
    ) -> Result<CheckedOutGeneration, DeltaStoreError> {
        self.checkout_generation_with_migration(target_gen_id, false)
    }

    fn materialize_generation(
        &self,
        record: GenerationRecord,
    ) -> Result<CheckedOutGeneration, DeltaStoreError> {
        let mut chunks = Vec::with_capacity(record.chunks.len());
        for reference in &record.chunks {
            let delta = match reference.storage {
                ChunkStorage::DeterministicBase => ChunkDelta::new(
                    &record.worldgen_epoch,
                    reference.dim_id,
                    reference.cx,
                    reference.cz,
                ),
                ChunkStorage::DeltaBlob => {
                    let path = blob_path(&self.root, &reference.content_hash);
                    let metadata = match std::fs::metadata(&path) {
                        Ok(metadata) => metadata,
                        Err(error) if error.kind() == io::ErrorKind::NotFound => {
                            return Err(DeltaStoreError::MissingBlob {
                                content_hash: hex_encode(&reference.content_hash),
                            });
                        }
                        Err(error) => return Err(store_ioe("stat delta blob", &path)(error)),
                    };
                    let actual_len = metadata.len();
                    if actual_len != u64::from(reference.encoded_len) {
                        return Err(DeltaStoreError::BlobSizeMismatch {
                            content_hash: hex_encode(&reference.content_hash),
                            expected_len: u64::from(reference.encoded_len),
                            actual_len,
                        });
                    }
                    let bytes =
                        read_file_capped(&path, MAX_DELTA_BLOB_BYTES as u64, "read delta blob")?;
                    let actual_hash = sha256(&bytes);
                    if actual_hash != reference.content_hash {
                        return Err(DeltaStoreError::BlobIntegrity {
                            expected_hash: hex_encode(&reference.content_hash),
                            actual_hash: hex_encode(&actual_hash),
                        });
                    }
                    let delta = decode_chunk_delta(&bytes)?;
                    if delta.worldgen_epoch != record.worldgen_epoch
                        || delta.dim_id != reference.dim_id
                        || delta.cx != reference.cx
                        || delta.cz != reference.cz
                    {
                        return Err(DeltaStoreError::InconsistentChunkMetadata {
                            dim_id: reference.dim_id,
                            cx: reference.cx,
                            cz: reference.cz,
                            reason: "blob identity does not match generation entry",
                        });
                    }
                    delta
                }
            };
            chunks.push(delta);
        }
        Ok(CheckedOutGeneration { record, chunks })
    }

    /// Compute the exact sparse block and tombstone diff between two
    /// generations. Different seeds always fail closed. Different
    /// `worldgen_epoch`s fail closed unless `allow_epoch_migration` is true.
    /// Because this store does not generate terrain, a cross-epoch comparison
    /// is the comparison of the stored overlays after an explicit migration;
    /// callers remain responsible for migrating deterministic base terrain or
    /// numeric block-state ids when those meanings changed.
    pub fn diff_generations_with_migration(
        &self,
        gen_a: u64,
        gen_b: u64,
        allow_epoch_migration: bool,
    ) -> Result<GenerationDiff, DeltaStoreError> {
        let record_a = self
            .records
            .get(&gen_a)
            .ok_or(DeltaStoreError::GenerationNotFound(gen_a))?
            .clone();
        let record_b = self
            .records
            .get(&gen_b)
            .ok_or(DeltaStoreError::GenerationNotFound(gen_b))?
            .clone();
        if record_a.seed != record_b.seed {
            return Err(DeltaStoreError::SeedMismatch {
                expected: record_a.seed,
                actual: record_b.seed,
            });
        }
        if record_a.worldgen_epoch != record_b.worldgen_epoch && !allow_epoch_migration {
            return Err(DeltaStoreError::EpochMismatch {
                expected: record_a.worldgen_epoch.clone(),
                actual: record_b.worldgen_epoch.clone(),
            });
        }
        let checkout_a = self.materialize_generation(record_a)?;
        let checkout_b = self.materialize_generation(record_b)?;
        diff_checkouts(&checkout_a, &checkout_b)
    }

    /// Compute the exact diff between two generations in the same worldgen
    /// epoch. Use [`Self::diff_generations_with_migration`] for an explicit
    /// migration comparison.
    pub fn diff_generations(
        &self,
        gen_a: u64,
        gen_b: u64,
    ) -> Result<GenerationDiff, DeltaStoreError> {
        self.diff_generations_with_migration(gen_a, gen_b, false)
    }

    /// Change the active head used for cross-epoch checkout protection and GC
    /// retention. The target's referenced blobs are verified fail-closed before
    /// the active-head manifest is updated; generation content itself is never
    /// modified.
    pub fn activate_generation(
        &mut self,
        target_gen_id: u64,
        allow_epoch_migration: bool,
    ) -> Result<(), DeltaStoreError> {
        let target = self
            .records
            .get(&target_gen_id)
            .ok_or(DeltaStoreError::GenerationNotFound(target_gen_id))?
            .clone();
        if let Some(active_id) = self.active_generation {
            let active = self
                .records
                .get(&active_id)
                .ok_or(DeltaStoreError::GenerationNotFound(active_id))?;
            if active.seed != target.seed {
                return Err(DeltaStoreError::SeedMismatch {
                    expected: active.seed,
                    actual: target.seed,
                });
            }
            if active.worldgen_epoch != target.worldgen_epoch && !allow_epoch_migration {
                return Err(DeltaStoreError::EpochMismatch {
                    expected: active.worldgen_epoch.clone(),
                    actual: target.worldgen_epoch.clone(),
                });
            }
        }
        self.materialize_generation(target)?;
        let previous_active = self.active_generation;
        self.active_generation = Some(target_gen_id);
        if let Err(error) = self.write_manifest() {
            self.active_generation = previous_active;
            return Err(error);
        }
        Ok(())
    }

    /// Pin or unpin a generation for garbage collection.
    pub fn pin_generation(&mut self, gen_id: u64, pinned: bool) -> Result<(), DeltaStoreError> {
        if !self.records.contains_key(&gen_id) {
            return Err(DeltaStoreError::GenerationNotFound(gen_id));
        }
        let old_pinned = self.pinned.clone();
        if pinned {
            self.pinned.insert(gen_id);
        } else {
            self.pinned.remove(&gen_id);
        }
        if let Err(error) = self.write_manifest() {
            self.pinned = old_pinned;
            return Err(error);
        }
        Ok(())
    }

    /// Remove blob objects not referenced by any pinned generation or by the
    /// active generation. Orphans from crashed commits are always collectable.
    ///
    /// Unpinned inactive generation records remain visible in the manifest,
    /// but their blob payloads may be pruned; pin every generation that must
    /// remain checkout-able. This explicit-pin rule is what makes GC safe to
    /// automate without guessing which time-travel branches are valuable.
    pub fn gc_unreferenced_blobs(&self) -> Result<DeltaGcReport, DeltaStoreError> {
        let mut live = HashSet::new();
        for record in self.records.values() {
            let retained = self.pinned.contains(&record.gen_id)
                || self.active_generation == Some(record.gen_id);
            if !retained {
                continue;
            }
            for chunk in &record.chunks {
                if chunk.storage == ChunkStorage::DeltaBlob {
                    live.insert(hex_encode(&chunk.content_hash));
                }
            }
        }
        let blobs_root = self.root.join("blobs");
        let mut report = DeltaGcReport::default();
        let prefixes = match std::fs::read_dir(&blobs_root) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(report),
            Err(error) => return Err(store_ioe("list blob store", &blobs_root)(error)),
        };
        for prefix in prefixes {
            let prefix = prefix.map_err(store_ioe("list blob store", &blobs_root))?;
            let file_type = prefix
                .file_type()
                .map_err(store_ioe("stat blob prefix", &prefix.path()))?;
            if !file_type.is_dir() || file_type.is_symlink() {
                continue;
            }
            let prefix_path = prefix.path();
            let entries = std::fs::read_dir(&prefix_path)
                .map_err(store_ioe("list blob prefix", &prefix_path))?;
            for entry in entries {
                let entry = entry.map_err(store_ioe("list blob prefix", &prefix_path))?;
                let Ok(file_type) = entry.file_type() else {
                    continue;
                };
                if !file_type.is_file() || file_type.is_symlink() {
                    continue;
                }
                let name = entry.file_name();
                let Some(name) = name.to_str() else {
                    continue;
                };
                if !valid_hex_digest_name(name) {
                    continue;
                }
                report.scanned_blobs += 1;
                if live.contains(name) {
                    report.retained_blobs += 1;
                    continue;
                }
                let size = entry
                    .metadata()
                    .map_err(store_ioe("stat blob", &entry.path()))?
                    .len();
                std::fs::remove_file(entry.path())
                    .map_err(store_ioe("remove unreferenced blob", &entry.path()))?;
                report.removed_blobs += 1;
                report.removed_bytes += size;
            }
            let _ = std::fs::remove_dir(&prefix_path); // removes only when empty
        }
        Ok(report)
    }
}

fn checkout_overlay(checkout: &CheckedOutGeneration) -> BTreeMap<(i32, i32, i32), &ChunkDelta> {
    checkout
        .chunks
        .iter()
        .map(|delta| ((delta.dim_id, delta.cx, delta.cz), delta))
        .collect()
}

fn diff_checkouts(
    checkout_a: &CheckedOutGeneration,
    checkout_b: &CheckedOutGeneration,
) -> Result<GenerationDiff, DeltaStoreError> {
    let overlay_a = checkout_overlay(checkout_a);
    let overlay_b = checkout_overlay(checkout_b);
    let mut keys = BTreeSet::new();
    keys.extend(overlay_a.keys().copied());
    keys.extend(overlay_b.keys().copied());

    let mut chunks = Vec::new();
    for key in keys {
        let empty_a = ChunkDelta::new("", key.0, key.1, key.2);
        let empty_b = ChunkDelta::new("", key.0, key.1, key.2);
        let delta_a = overlay_a.get(&key).copied().unwrap_or(&empty_a);
        let delta_b = overlay_b.get(&key).copied().unwrap_or(&empty_b);
        let edits_a: BTreeMap<u16, BlockDeltaEdit> = delta_a
            .edits
            .iter()
            .map(|edit| (edit.local_idx, *edit))
            .collect();
        let edits_b: BTreeMap<u16, BlockDeltaEdit> = delta_b
            .edits
            .iter()
            .map(|edit| (edit.local_idx, *edit))
            .collect();
        let mut edit_indexes = BTreeSet::new();
        edit_indexes.extend(edits_a.keys().copied());
        edit_indexes.extend(edits_b.keys().copied());
        let mut block_diffs = Vec::new();
        for local_idx in edit_indexes {
            let (state_a, state_b) = match (edits_a.get(&local_idx), edits_b.get(&local_idx)) {
                (Some(a), Some(b)) if a.base_state != b.base_state => {
                    return Err(DeltaStoreError::InconsistentBaseState {
                        dim_id: key.0,
                        cx: key.1,
                        cz: key.2,
                        local_idx,
                    });
                }
                (Some(a), Some(b)) => (a.new_state, b.new_state),
                (Some(a), None) => (a.new_state, a.base_state),
                (None, Some(b)) => (b.base_state, b.new_state),
                (None, None) => continue, // indexes come from the union of the two maps
            };
            if state_a != state_b {
                block_diffs.push(BlockStateDiff {
                    local_idx,
                    generation_a_state: state_a,
                    generation_b_state: state_b,
                });
            }
        }

        let unknown_a: BTreeMap<u16, &UnknownBlockEntry> = delta_a
            .unknown_blocks
            .iter()
            .map(|entry| (entry.local_idx, entry))
            .collect();
        let unknown_b: BTreeMap<u16, &UnknownBlockEntry> = delta_b
            .unknown_blocks
            .iter()
            .map(|entry| (entry.local_idx, entry))
            .collect();
        let mut unknown_indexes = BTreeSet::new();
        unknown_indexes.extend(unknown_a.keys().copied());
        unknown_indexes.extend(unknown_b.keys().copied());
        let mut unknown_diffs = Vec::new();
        for local_idx in unknown_indexes {
            let a = unknown_a.get(&local_idx).copied().cloned();
            let b = unknown_b.get(&local_idx).copied().cloned();
            if a != b {
                unknown_diffs.push(UnknownBlockDiff {
                    local_idx,
                    generation_a: a,
                    generation_b: b,
                });
            }
        }
        if !block_diffs.is_empty() || !unknown_diffs.is_empty() {
            chunks.push(ChunkGenerationDiff {
                dim_id: key.0,
                cx: key.1,
                cz: key.2,
                block_diffs,
                unknown_diffs,
            });
        }
    }
    Ok(GenerationDiff {
        gen_a: checkout_a.record.gen_id,
        gen_b: checkout_b.record.gen_id,
        chunks,
    })
}
