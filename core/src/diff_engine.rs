// SPDX-License-Identifier: LGPL-3.0-or-later
//! `megu_core::diff_engine` — universal, pure-`std` differential-computation
//! ("diff-based lightweighting") primitives: compute, store, and transmit
//! the exact minimal delta `ΔS` of a state `S` instead of rebuilding the
//! full state every frame, tick, or packet.
//!
//! Everything here is pure `std`, allocation-bounded, deterministic, and
//! fail-closed: no clocks, no randomness, no `unsafe`, no external crates.
//! The four primitives are generic over the whole client/server surface:
//!
//! * [`XorRleDiff`] — an exact invertible binary delta codec. `encode`
//!   diffs two equal-length byte buffers into a compact `XDF1` frame
//!   (run-length-skipped XOR literals under canonical LEB128 varints plus a
//!   CRC32 trailer), `Ok(None)` with zero allocation when the buffers are
//!   identical, and `apply`/`apply_in_place` replay the patch bit-exactly
//!   under strict bounds, canonical-varint, and CRC checks. A 4-byte edit
//!   inside a 16 KiB section compresses to ~21 bytes (> 99.8 %).
//! * [`DirtySlotTracker`] — a shadow-copy tracker for flat `T: PartialEq +
//!   Clone` arrays (GPU vertex/index buffers, inventories, registries).
//!   Mutated slots coalesce into minimal contiguous [`DirtySpan`]s so one
//!   upload covers a whole run; a steady frame yields an empty plan and
//!   uploads zero bytes. Telemetry counters make "0 B on idle" measurable.
//! * [`SubtileDiffMask`] — a `u64` bitmask over the 4³ = 64 sub-tiles of a
//!   16³ voxel section. `mark_block`/`mark_block_with_neighbors` preserve
//!   exact dirty locality for single-block edits (boundary blocks also mark
//!   the face-adjacent sub-tile so partial remeshing never misses a
//!   changed cull face), `dirty_bounds` returns the tight voxel AABB, and
//!   `iter_dirty_subtiles` runs in O(popcount) via `trailing_zeros`.
//! * [`FieldDiffBitset`] — a zero-allocation `u64` bitmask for up to 64
//!   typed entity/UI/state fields: `mark`, `take_dirty_mask`, and
//!   `iter_dirty_indices` do zero work on steady ticks.

use std::collections::BTreeSet;
use std::error::Error;
use std::fmt;
use std::iter::Peekable;

// ---------------------------------------------------------------------------
// XorRleDiff — exact invertible binary delta codec
// ---------------------------------------------------------------------------

/// Leading magic of every [`XorRleDiff`] frame: `XDF1`.
pub const XOR_RLE_MAGIC: [u8; 4] = *b"XDF1";

/// Hard input cap for [`XorRleDiff::encode`], [`XorRleDiff::apply`] and
/// [`XorRleDiff::apply_in_place`]: buffers past 16 MiB are rejected
/// fail-closed so a hostile or corrupt size field can never force a giant
/// allocation or scan.
pub const XOR_RLE_MAX_INPUT_BYTES: usize = 16 * 1024 * 1024;

/// Hard cap on the patch byte string handed to [`XorRleDiff::apply`].
/// A legal encoder output costs at most three bytes per input byte (one
/// literal byte plus one byte of skip and literal varints on single-byte
/// alternation); `4 × input + 64` covers the worst-case legal frame while
/// still bounding hostile input.
pub const XOR_RLE_MAX_PATCH_BYTES: usize = XOR_RLE_MAX_INPUT_BYTES * 4 + 64;

/// Trailing IEEE CRC32 size, in bytes.
const CRC32_TRAILER_BYTES: usize = 4;

/// Every [`XorRleDiff`] failure mode. All are deterministic and
/// fail-closed: a malformed, truncated, oversized, or corrupt patch is
/// rejected before any byte of the output buffer is written.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DiffError {
    /// `encode` was given `base`/`next` slices of different lengths, or
    /// `apply`/`apply_in_place` was given a buffer whose length does not
    /// match the length declared in the frame header.
    LengthMismatch {
        /// Length the operation requires (base len for encode, declared
        /// frame len for apply).
        declared: usize,
        /// Length actually supplied.
        actual: usize,
    },
    /// An input buffer or the patch itself exceeds the fail-closed cap
    /// ([`XOR_RLE_MAX_INPUT_BYTES`] / [`XOR_RLE_MAX_PATCH_BYTES`]).
    TooLarge {
        /// Observed size in bytes.
        len: usize,
        /// Cap that was exceeded.
        cap: usize,
    },
    /// Leading magic bytes were not [`XOR_RLE_MAGIC`].
    BadMagic,
    /// The patch ended before a declared field, varint, literal run, or
    /// the CRC32 trailer completed.
    Truncated,
    /// Structurally invalid frame content: a non-canonical or overflowing
    /// varint, or a record that makes no forward progress.
    Malformed(&'static str),
    /// A skip/literal pair walked the output cursor past the declared
    /// original length.
    OutOfBounds,
    /// The trailing CRC32 did not match the frame body.
    CrcMismatch,
    /// Extra bytes follow the last record before the CRC32 trailer.
    TrailingBytes,
}

impl fmt::Display for DiffError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::LengthMismatch { declared, actual } => write!(
                f,
                "diff length mismatch: declared {declared} bytes, got {actual}"
            ),
            Self::TooLarge { len, cap } => {
                write!(f, "diff input of {len} bytes exceeds cap of {cap} bytes")
            }
            Self::BadMagic => f.write_str("diff frame magic is not XDF1"),
            Self::Truncated => f.write_str("diff frame is truncated"),
            Self::Malformed(what) => write!(f, "malformed diff frame: {what}"),
            Self::OutOfBounds => f.write_str("diff record walks output past the declared length"),
            Self::CrcMismatch => f.write_str("diff frame CRC32 mismatch"),
            Self::TrailingBytes => {
                f.write_str("diff frame has trailing bytes before the CRC32 trailer")
            }
        }
    }
}

impl Error for DiffError {}

/// Exact invertible binary delta codec.
///
/// Wire format of an `XDF1` frame:
///
/// ```text
/// magic        4 B   "XDF1"
/// orig_len     var   canonical LEB128 u64 — required output/base length
/// records      var   repeating (skip_len var, lit_len var, lit_bytes…):
///                    skip `skip_len` bytes that stayed identical, then
///                    `lit_len` XOR literal bytes (`base ^ next`). The
///                    final record may carry `lit_len == 0` to encode a
///                    trailing identical run.
/// crc32        4 B   big-endian IEEE 802.3 CRC32 over every byte above
/// ```
///
/// `encode` returns `Ok(None)` — with zero allocation — when `base ==
/// next`; otherwise it emits the smallest alternating skip/literal record
/// stream, so a sparse edit inside a large buffer compresses to a few
/// bytes. Decoding validates the CRC32 and fully dry-runs the record
/// stream before touching the destination, so no failure leaves a
/// partially patched buffer.
pub struct XorRleDiff;

impl XorRleDiff {
    /// Diffs `base` → `next`. Returns `Ok(None)` when the buffers are
    /// identical, `Ok(Some(patch))` with the `XDF1` frame otherwise.
    /// Fails closed on length mismatch or input past
    /// [`XOR_RLE_MAX_INPUT_BYTES`].
    pub fn encode(base: &[u8], next: &[u8]) -> Result<Option<Vec<u8>>, DiffError> {
        if base.len() != next.len() {
            return Err(DiffError::LengthMismatch {
                declared: base.len(),
                actual: next.len(),
            });
        }
        if base.len() > XOR_RLE_MAX_INPUT_BYTES {
            return Err(DiffError::TooLarge {
                len: base.len(),
                cap: XOR_RLE_MAX_INPUT_BYTES,
            });
        }
        if base == next {
            return Ok(None);
        }
        let mut out = Vec::new();
        out.extend_from_slice(&XOR_RLE_MAGIC);
        push_varint(&mut out, base.len() as u64);
        let mut pos = 0usize;
        while pos < base.len() {
            let mut skip = 0usize;
            while pos + skip < base.len() && base[pos + skip] == next[pos + skip] {
                skip += 1;
            }
            let lit_start = pos + skip;
            let mut lit = 0usize;
            while lit_start + lit < base.len() && base[lit_start + lit] != next[lit_start + lit] {
                lit += 1;
            }
            push_varint(&mut out, skip as u64);
            push_varint(&mut out, lit as u64);
            out.extend((0..lit).map(|i| base[lit_start + i] ^ next[lit_start + i]));
            pos = lit_start + lit;
        }
        let crc = crate::hash::crc32_ieee(&out);
        out.extend_from_slice(&crc.to_be_bytes());
        Ok(Some(out))
    }

    /// Returns the buffer `patch` turns `base` into. `base.len()` must
    /// equal the frame's declared length; the patch is fully validated
    /// (magic, size cap, CRC32, canonical varints, bounds) before the
    /// output is allocated or written.
    pub fn apply(base: &[u8], patch: &[u8]) -> Result<Vec<u8>, DiffError> {
        let (declared, cursor, body_end) = Self::verify_frame(patch)?;
        if declared != base.len() {
            return Err(DiffError::LengthMismatch {
                declared,
                actual: base.len(),
            });
        }
        Self::scan(patch, cursor, body_end, declared, None)?;
        let mut out = base.to_vec();
        Self::scan(patch, cursor, body_end, declared, Some(&mut out))?;
        Ok(out)
    }

    /// [`apply`](Self::apply) writing through `buf` in place: each literal
    /// byte is XORed onto `buf`. `buf` is never left partially patched —
    /// the whole frame validates before the first byte is written.
    pub fn apply_in_place(buf: &mut [u8], patch: &[u8]) -> Result<(), DiffError> {
        let (declared, cursor, body_end) = Self::verify_frame(patch)?;
        if declared != buf.len() {
            return Err(DiffError::LengthMismatch {
                declared,
                actual: buf.len(),
            });
        }
        Self::scan(patch, cursor, body_end, declared, None)?;
        Self::scan(patch, cursor, body_end, declared, Some(buf))
    }

    /// Validates everything that does not depend on record contents —
    /// patch size cap, magic, CRC32 trailer, canonical declared-length
    /// varint, declared length cap — and returns `(declared_len,
    /// records_cursor, body_end)` where `body_end` is the offset of the
    /// CRC32 trailer.
    fn verify_frame(patch: &[u8]) -> Result<(usize, usize, usize), DiffError> {
        if patch.len() > XOR_RLE_MAX_PATCH_BYTES {
            return Err(DiffError::TooLarge {
                len: patch.len(),
                cap: XOR_RLE_MAX_PATCH_BYTES,
            });
        }
        if patch.len() < XOR_RLE_MAGIC.len() + 1 + CRC32_TRAILER_BYTES {
            return Err(DiffError::Truncated);
        }
        if patch[..XOR_RLE_MAGIC.len()] != XOR_RLE_MAGIC {
            return Err(DiffError::BadMagic);
        }
        let body_end = patch.len() - CRC32_TRAILER_BYTES;
        let stored_crc = u32::from_be_bytes([
            patch[body_end],
            patch[body_end + 1],
            patch[body_end + 2],
            patch[body_end + 3],
        ]);
        if crate::hash::crc32_ieee(&patch[..body_end]) != stored_crc {
            return Err(DiffError::CrcMismatch);
        }
        let mut cursor = XOR_RLE_MAGIC.len();
        let declared = take_varint(patch, &mut cursor, body_end)?;
        let declared = usize::try_from(declared)
            .map_err(|_| DiffError::Malformed("declared length overflows usize"))?;
        if declared > XOR_RLE_MAX_INPUT_BYTES {
            return Err(DiffError::TooLarge {
                len: declared,
                cap: XOR_RLE_MAX_INPUT_BYTES,
            });
        }
        Ok((declared, cursor, body_end))
    }

    /// Walks the record stream between `cursor` and `body_end`,
    /// bounds-checking every skip/literal against `declared` and the
    /// patch. When `buf` is `Some` the literals are XORed onto it; when
    /// `None` this is a pure structural dry-run so callers can validate a
    /// whole frame before mutating anything.
    fn scan(
        patch: &[u8],
        mut cursor: usize,
        body_end: usize,
        declared: usize,
        mut buf: Option<&mut [u8]>,
    ) -> Result<(), DiffError> {
        let declared64 = declared as u64;
        let mut out = 0u64;
        while out < declared64 {
            let skip = take_varint(patch, &mut cursor, body_end)?;
            let lit = take_varint(patch, &mut cursor, body_end)?;
            out = out.checked_add(skip).ok_or(DiffError::OutOfBounds)?;
            if out > declared64 {
                return Err(DiffError::OutOfBounds);
            }
            if lit == 0 {
                if out != declared64 {
                    return Err(DiffError::Malformed(
                        "zero-length literal before end of frame",
                    ));
                }
                break;
            }
            let lit_end = out.checked_add(lit).ok_or(DiffError::OutOfBounds)?;
            if lit_end > declared64 {
                return Err(DiffError::OutOfBounds);
            }
            let lit = usize::try_from(lit)
                .map_err(|_| DiffError::Malformed("literal length overflows usize"))?;
            let byte_end = cursor.checked_add(lit).ok_or(DiffError::Truncated)?;
            if byte_end > body_end {
                return Err(DiffError::Truncated);
            }
            if let Some(buf) = buf.as_deref_mut() {
                for (dst, &xor) in buf[out as usize..lit_end as usize]
                    .iter_mut()
                    .zip(&patch[cursor..byte_end])
                {
                    *dst ^= xor;
                }
            }
            out = lit_end;
            cursor = byte_end;
        }
        if cursor != body_end {
            return Err(DiffError::TrailingBytes);
        }
        Ok(())
    }
}

/// Appends `value` as canonical LEB128 (the encoding [`take_varint`]
/// requires: minimal length, no zero padding).
fn push_varint(out: &mut Vec<u8>, mut value: u64) {
    loop {
        let mut byte = (value & 0x7f) as u8;
        value >>= 7;
        if value != 0 {
            byte |= 0x80;
        }
        out.push(byte);
        if value == 0 {
            return;
        }
    }
}

/// Reads a canonical LEB128 varint strictly below `body_end`. Fails closed
/// on truncation, on encodings longer than 10 bytes / overflowing `u64`,
/// and on non-canonical zero-padded forms.
fn take_varint(patch: &[u8], cursor: &mut usize, body_end: usize) -> Result<u64, DiffError> {
    let mut value = 0u64;
    let mut shift = 0u32;
    loop {
        if *cursor >= body_end {
            return Err(DiffError::Truncated);
        }
        let byte = patch[*cursor];
        *cursor += 1;
        if shift >= 64 {
            return Err(DiffError::Malformed("varint exceeds 10 bytes"));
        }
        let payload = u64::from(byte & 0x7f);
        if shift == 63 && payload > 1 {
            return Err(DiffError::Malformed("varint overflows u64"));
        }
        if payload == 0 && shift > 0 && byte & 0x80 == 0 {
            return Err(DiffError::Malformed("non-canonical varint encoding"));
        }
        value |= payload << shift;
        if byte & 0x80 == 0 {
            return Ok(value);
        }
        shift += 7;
    }
}

// ---------------------------------------------------------------------------
// DirtySlotTracker — coalesced contiguous-span patcher for flat arrays
// ---------------------------------------------------------------------------

/// One contiguous run of dirty slots in a [`DirtySlotTracker`] plan: upload
/// or rewrite `slots[start .. start + len]` in a single call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DirtySpan {
    /// First dirty slot index, inclusive.
    pub start: usize,
    /// Number of dirty slots in the run (`end - start`).
    pub len: usize,
}

impl DirtySpan {
    /// First slot past the run (`start + len`).
    pub fn end(self) -> usize {
        self.start + self.len
    }
}

/// What [`DirtySlotTracker::sync_from_slice`] decided for one frame.
///
/// `full_realloc == true` means the tracked array changed length (grown or
/// shrunk): the whole `next` contents must be re-uploaded and `spans`
/// holds a single span covering it (empty when `next` is empty). When
/// `full_realloc` is false, `spans` is the exact minimal set of contiguous
/// dirty runs — an empty `spans` means a steady frame that uploads
/// **0 bytes**.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DirtySyncPlan {
    /// `next` changed length: re-upload everything, do not diff.
    pub full_realloc: bool,
    /// Minimal coalesced dirty runs over `next` (`full_realloc == false`),
    /// or a single whole-buffer span (`full_realloc == true`).
    pub spans: Vec<DirtySpan>,
}

impl DirtySyncPlan {
    /// A steady frame: nothing changed, nothing to upload.
    pub fn is_steady(&self) -> bool {
        !self.full_realloc && self.spans.is_empty()
    }

    /// Total slots covered by `spans` — the slot count an upload writes.
    pub fn covered_slots(&self) -> usize {
        self.spans.iter().map(|span| span.len).sum()
    }
}

/// Cumulative [`DirtySlotTracker`] counters, readable via
/// [`DirtySlotTracker::telemetry`]. `slots_uploaded` versus
/// `slots_skipped` is the measured lightweighting ratio.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DirtySlotTelemetry {
    /// [`sync_from_slice`](DirtySlotTracker::sync_from_slice) calls so far.
    pub total_syncs: u64,
    /// Syncs that produced zero dirty spans (steady frames).
    pub zero_diff_syncs: u64,
    /// Slots a plan or drain reported dirty (the bytes-worth of uploads).
    pub slots_uploaded: u64,
    /// Slots a sync confirmed identical (skipped uploads).
    pub slots_skipped: u64,
}

/// Shadow-copy dirty tracker for flat arrays — GPU vertex/index buffers,
/// inventory slots, registries — where only a few entries change between
/// syncs but the upload granularity is a contiguous byte range.
///
/// The tracker keeps a `shadow` copy of the last-synced contents plus a
/// `pending` set of slot indices mutated through
/// [`update_slot`](Self::update_slot). [`sync_from_slice`](Self::sync_from_slice)
/// diffs a new full slice against `shadow`, folds in every pending mark,
/// rewrites `shadow`, and returns the minimal [`DirtySpan`] set; callers
/// that only push updates can instead drain the marks with
/// [`take_dirty_spans`](Self::take_dirty_spans). A steady frame produces
/// an empty span list — **zero bytes uploaded** — which the
/// [`DirtySlotTelemetry`] counters prove.
pub struct DirtySlotTracker<T: PartialEq + Clone> {
    /// Last-synced contents; the baseline every diff is computed against.
    shadow: Vec<T>,
    /// Slot indices mutated through `update_slot` and not yet reported by
    /// a sync or drained by `take_dirty_spans`.
    pending: BTreeSet<usize>,
    telemetry: DirtySlotTelemetry,
}

impl<T: PartialEq + Clone> Default for DirtySlotTracker<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T: PartialEq + Clone> DirtySlotTracker<T> {
    /// An empty tracker; the first [`sync_from_slice`](Self::sync_from_slice)
    /// reports `full_realloc` and uploads the whole slice.
    pub fn new() -> Self {
        Self {
            shadow: Vec::new(),
            pending: BTreeSet::new(),
            telemetry: DirtySlotTelemetry::default(),
        }
    }

    /// A tracker whose `shadow` is already `initial`, so the first sync
    /// diffs instead of re-uploading.
    pub fn from_slice(initial: &[T]) -> Self {
        Self {
            shadow: initial.to_vec(),
            pending: BTreeSet::new(),
            telemetry: DirtySlotTelemetry::default(),
        }
    }

    /// The last-synced contents every diff is computed against.
    pub fn shadow(&self) -> &[T] {
        &self.shadow
    }

    /// Number of slots currently tracked (the shadow length).
    pub fn len(&self) -> usize {
        self.shadow.len()
    }

    /// Whether the tracker holds no slots yet.
    pub fn is_empty(&self) -> bool {
        self.shadow.is_empty()
    }

    /// Marks slot `idx` dirty after writing `val` into the shadow. Returns
    /// `false` — a no-op — when `idx` is out of range or the slot already
    /// holds `val`, so identical rewrites never dirty the buffer.
    pub fn update_slot(&mut self, idx: usize, val: T) -> bool {
        let Some(slot) = self.shadow.get_mut(idx) else {
            return false;
        };
        if *slot == val {
            return false;
        }
        *slot = val;
        self.pending.insert(idx);
        true
    }

    /// Diffs `next` against the shadow plus every pending `update_slot`
    /// mark, commits `next` as the new shadow, and returns the upload
    /// plan. Adjacent dirty slots coalesce into a single [`DirtySpan`];
    /// pending marks keep a slot dirty even when `next` happens to hold
    /// the value already written through `update_slot`.
    pub fn sync_from_slice(&mut self, next: &[T]) -> DirtySyncPlan {
        self.telemetry.total_syncs += 1;
        let len = next.len();
        if len != self.shadow.len() {
            self.shadow.clear();
            self.shadow.extend_from_slice(next);
            self.pending.clear();
            self.telemetry.slots_uploaded += len as u64;
            let spans = if len == 0 {
                Vec::new()
            } else {
                vec![DirtySpan { start: 0, len }]
            };
            return DirtySyncPlan {
                full_realloc: true,
                spans,
            };
        }
        let mut spans = Vec::new();
        let mut covered = 0usize;
        {
            let mut pending = self.pending.iter().copied().peekable();
            let mut i = 0usize;
            while i < len {
                if Self::slot_dirty(&mut pending, i, &self.shadow, next) {
                    let start = i;
                    while i < len && Self::slot_dirty(&mut pending, i, &self.shadow, next) {
                        self.shadow[i] = next[i].clone();
                        i += 1;
                    }
                    covered += i - start;
                    spans.push(DirtySpan {
                        start,
                        len: i - start,
                    });
                } else {
                    i += 1;
                }
            }
        }
        self.pending.clear();
        if covered == 0 {
            self.telemetry.zero_diff_syncs += 1;
        } else {
            self.telemetry.slots_uploaded += covered as u64;
        }
        self.telemetry.slots_skipped += (len - covered) as u64;
        DirtySyncPlan {
            full_realloc: false,
            spans,
        }
    }

    /// Whether slot `i` needs an upload: explicitly marked through
    /// `update_slot` (`pending`, advanced to `i`) or still differing from
    /// `next` in the shadow.
    fn slot_dirty(
        pending: &mut Peekable<impl Iterator<Item = usize>>,
        i: usize,
        shadow: &[T],
        next: &[T],
    ) -> bool {
        while matches!(pending.peek(), Some(&p) if p < i) {
            pending.next();
        }
        pending.peek().copied() == Some(i) || shadow[i] != next[i]
    }

    /// Drains the slots marked by [`update_slot`](Self::update_slot) into
    /// the same minimal coalesced [`DirtySpan`] form a sync returns — the
    /// push-side equivalent for callers that never re-scan a full slice.
    pub fn take_dirty_spans(&mut self) -> Vec<DirtySpan> {
        let mut iter = self.pending.iter().copied();
        let Some(mut start) = iter.next() else {
            return Vec::new();
        };
        let mut end = start + 1;
        let mut spans = Vec::new();
        for idx in iter {
            if idx == end {
                end += 1;
                continue;
            }
            spans.push(DirtySpan {
                start,
                len: end - start,
            });
            start = idx;
            end = idx + 1;
        }
        spans.push(DirtySpan {
            start,
            len: end - start,
        });
        self.telemetry.slots_uploaded += spans.iter().map(|s| s.len as u64).sum::<u64>();
        self.pending.clear();
        spans
    }

    /// Cumulative counters — see [`DirtySlotTelemetry`].
    pub fn telemetry(&self) -> DirtySlotTelemetry {
        self.telemetry
    }
}

// ---------------------------------------------------------------------------
// Shared O(popcount) set-bit iterator
// ---------------------------------------------------------------------------

/// Iterates the set bits of a `u64` in ascending order via
/// `trailing_zeros` — `O(popcount)`, zero allocation.
struct BitIndexIter {
    bits: u64,
}

impl Iterator for BitIndexIter {
    type Item = u8;

    fn next(&mut self) -> Option<u8> {
        if self.bits == 0 {
            return None;
        }
        let index = self.bits.trailing_zeros() as u8;
        self.bits &= self.bits - 1;
        Some(index)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let exact = self.bits.count_ones() as usize;
        (exact, Some(exact))
    }
}

impl ExactSizeIterator for BitIndexIter {}

// ---------------------------------------------------------------------------
// SubtileDiffMask — 64-bit 4×4×4 sub-tile dirty mask for 16³ sections
// ---------------------------------------------------------------------------

/// A `u64` dirty-sub-tile bitmask for one 16³ voxel section: the section
/// is split into `4³ = 64` sub-tiles of `4³` voxels each, and bit
/// `(sy << 4) | (sz << 2) | sx` marks the sub-tile at sub-tile coordinate
/// `(sx, sy, sz)` (`voxel >> 2` per axis).
///
/// `mark_block`/`mark_block_with_neighbors` keep single-block edits at
/// 1–8 dirty sub-tiles instead of the whole section:
/// `mark_block_with_neighbors` also marks the sub-tile across each axis
/// boundary the block touches (a block at `lx % 4 == 3` borders sub-tile
/// `sx + 1`), so partial remeshing re-evaluates every cull face the edit
/// can change — up to `2³ = 8` sub-tiles for a block on a three-axis
/// sub-tile corner, never more.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct SubtileDiffMask(u64);

impl SubtileDiffMask {
    /// No sub-tile dirty.
    pub const EMPTY: Self = Self(0);
    /// All 64 sub-tiles dirty (full-section remesh).
    pub const ALL: Self = Self(u64::MAX);
    /// Voxel edge length of one sub-tile.
    pub const SUBTILE_EDGE: u8 = 4;
    /// Sub-tiles per section axis (`16 / 4`).
    pub const SUBTILES_PER_AXIS: u8 = 4;
    /// Total sub-tiles tracked (`4³`).
    pub const SUBTILE_COUNT: usize = 64;
    /// Voxel edge length of the section the mask covers.
    pub const SECTION_EDGE: u8 = 16;

    /// An empty mask.
    pub const fn new() -> Self {
        Self::EMPTY
    }

    /// Wraps a raw `u64` bit pattern (`subtile_index` bit order).
    pub const fn from_bits(bits: u64) -> Self {
        Self(bits)
    }

    /// The raw `u64` bit pattern.
    pub const fn bits(self) -> u64 {
        self.0
    }

    /// Whether no sub-tile is dirty.
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// Number of dirty sub-tiles (`popcount`).
    pub const fn dirty_count(self) -> u32 {
        self.0.count_ones()
    }

    /// Bit index of sub-tile coordinate `(sx, sy, sz)`, each `0..4`.
    pub const fn subtile_index(sx: u8, sy: u8, sz: u8) -> u8 {
        (sy << 4) | (sz << 2) | sx
    }

    /// Inverse of [`subtile_index`](Self::subtile_index): `(sx, sy, sz)`
    /// for a bit index `0..64`.
    pub const fn subtile_coords(index: u8) -> [u8; 3] {
        [index & 3, index >> 4, (index >> 2) & 3]
    }

    /// Marks the sub-tile containing voxel `(lx, ly, lz)` (each `0..16`).
    /// Returns `false` — a no-op — on out-of-section coordinates.
    pub fn mark_block(&mut self, lx: u8, ly: u8, lz: u8) -> bool {
        if lx >= Self::SECTION_EDGE || ly >= Self::SECTION_EDGE || lz >= Self::SECTION_EDGE {
            return false;
        }
        self.0 |= 1
            << Self::subtile_index(
                lx / Self::SUBTILE_EDGE,
                ly / Self::SUBTILE_EDGE,
                lz / Self::SUBTILE_EDGE,
            );
        true
    }

    /// Marks one sub-tile coordinate directly. Invalid coordinates are
    /// ignored and return `false`.
    pub fn mark_tile(&mut self, sx: u8, sy: u8, sz: u8) -> bool {
        if sx >= Self::SUBTILES_PER_AXIS || sy >= Self::SUBTILES_PER_AXIS || sz >= Self::SUBTILES_PER_AXIS {
            return false;
        }
        self.0 |= 1 << Self::subtile_index(sx, sy, sz);
        true
    }

    /// Whether one sub-tile coordinate is marked.
    pub const fn is_tile_dirty(self, sx: u8, sy: u8, sz: u8) -> bool {
        sx < Self::SUBTILES_PER_AXIS && sy < Self::SUBTILES_PER_AXIS && sz < Self::SUBTILES_PER_AXIS
            && self.0 & (1 << Self::subtile_index(sx, sy, sz)) != 0
    }

    /// [`mark_block`](Self::mark_block) plus every sub-tile the block
    /// touches across a sub-tile boundary: for each axis where the voxel
    /// sits on a sub-tile edge (`l % 4 == 0` reaches the lower neighbor,
    /// `l % 4 == 3` the upper), the range of affected sub-tiles grows by
    /// one — up to `2³ = 8` sub-tiles for a block on a three-axis
    /// sub-tile corner, where diagonal neighbors' ambient-occlusion and
    /// cull sampling still observe the edit. Returns `false` on
    /// out-of-section coordinates.
    pub fn mark_block_with_neighbors(&mut self, lx: u8, ly: u8, lz: u8) -> bool {
        if !self.mark_block(lx, ly, lz) {
            return false;
        }
        let base = [
            lx / Self::SUBTILE_EDGE,
            ly / Self::SUBTILE_EDGE,
            lz / Self::SUBTILE_EDGE,
        ];
        let mut lo = base;
        let mut hi = base;
        for (axis, &l) in [lx, ly, lz].iter().enumerate() {
            if l % Self::SUBTILE_EDGE == 0 {
                lo[axis] = base[axis].saturating_sub(1);
            }
            if l % Self::SUBTILE_EDGE == Self::SUBTILE_EDGE - 1 {
                hi[axis] = (base[axis] + 1).min(Self::SUBTILES_PER_AXIS - 1);
            }
        }
        for sx in lo[0]..=hi[0] {
            for sy in lo[1]..=hi[1] {
                for sz in lo[2]..=hi[2] {
                    self.0 |= 1 << Self::subtile_index(sx, sy, sz);
                }
            }
        }
        true
    }

    /// Tight voxel-space AABB around every dirty sub-tile:
    /// `(min, max)` inclusive corners in `0..16`, or `None` when the mask
    /// is empty.
    pub fn dirty_bounds(self) -> Option<([u8; 3], [u8; 3])> {
        if self.0 == 0 {
            return None;
        }
        let mut min = [u8::MAX; 3];
        let mut max = [0u8; 3];
        for index in self.iter_dirty_subtiles() {
            let [sx, sy, sz] = Self::subtile_coords(index);
            for (axis, &s) in [sx, sy, sz].iter().enumerate() {
                let lo = s * Self::SUBTILE_EDGE;
                min[axis] = min[axis].min(lo);
                max[axis] = max[axis].max(lo + Self::SUBTILE_EDGE - 1);
            }
        }
        Some((min, max))
    }

    /// Dirty sub-tile bit indices in ascending order, `O(popcount)` via
    /// `trailing_zeros`. Pair with [`subtile_coords`](Self::subtile_coords)
    /// for `(sx, sy, sz)`.
    pub fn iter_dirty_subtiles(self) -> impl Iterator<Item = u8> {
        BitIndexIter { bits: self.0 }
    }

    /// Visits and clears each dirty sub-tile in ascending bit order.
    pub(crate) fn drain_dirty_subtiles(&mut self) -> impl Iterator<Item = u8> + '_ {
        let mask = self;
        std::iter::from_fn(move || {
            if mask.0 == 0 { return None; }
            let index = mask.0.trailing_zeros() as u8;
            mask.0 &= mask.0 - 1;
            Some(index)
        })
    }

    /// Clears every mark.
    pub fn clear(&mut self) {
        self.0 = 0;
    }
}

// ---------------------------------------------------------------------------
// FieldDiffBitset — zero-alloc 64-field entity/UI/state diff tracker
// ---------------------------------------------------------------------------

/// Number of fields a [`FieldDiffBitset`] can track (one `u64` bit each).
pub const FIELD_DIFF_MAX_FIELDS: u8 = 64;

/// A `u64` dirty-field bitmask for entity state, UI widgets, or any
/// flat field list of up to [`FIELD_DIFF_MAX_FIELDS`] entries. All
/// operations are `O(1)` bit ops — `iter_dirty_indices` is `O(popcount)` —
/// so a steady tick does literally zero work beyond one compare.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct FieldDiffBitset(u64);

impl FieldDiffBitset {
    /// No field dirty.
    pub const EMPTY: Self = Self(0);
    /// Every one of the 64 fields dirty.
    pub const ALL: Self = Self(u64::MAX);

    /// An empty tracker.
    pub const fn new() -> Self {
        Self::EMPTY
    }

    /// Marks field `field_idx` (`0..64`) dirty. Returns `false` — a
    /// no-op — for indices outside the tracked range.
    pub fn mark(&mut self, field_idx: u8) -> bool {
        if field_idx >= FIELD_DIFF_MAX_FIELDS {
            return false;
        }
        self.0 |= 1 << field_idx;
        true
    }

    /// Whether no field is dirty — the steady-tick fast path.
    pub const fn is_clean(self) -> bool {
        self.0 == 0
    }

    /// Whether field `field_idx` is dirty (out-of-range is never dirty).
    pub const fn is_dirty(self, field_idx: u8) -> bool {
        field_idx < FIELD_DIFF_MAX_FIELDS && self.0 & (1 << field_idx) != 0
    }

    /// Number of dirty fields.
    pub const fn dirty_count(self) -> u32 {
        self.0.count_ones()
    }

    /// The raw dirty-field bitmask, without clearing it.
    pub const fn dirty_mask(self) -> u64 {
        self.0
    }

    /// Returns the raw dirty-field bitmask and clears the tracker — the
    /// one-call "what changed this tick" drain.
    pub fn take_dirty_mask(&mut self) -> u64 {
        let mask = self.0;
        self.0 = 0;
        mask
    }

    /// Dirty field indices in ascending order, `O(popcount)` via
    /// `trailing_zeros`.
    pub fn iter_dirty_indices(self) -> impl Iterator<Item = u8> {
        BitIndexIter { bits: self.0 }
    }

    /// Clears every mark.
    pub fn clear(&mut self) {
        self.0 = 0;
    }
}
