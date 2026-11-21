// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! CoreProtect-style ultra-lightweight binary audit log with atomic rollback.
//!
//! Every world mutation worth undoing — a placed or broken block, an
//! explosion crater, a container insert/take, an entity kill — is one
//! [`AuditRecord`] in an append-only [`AuditLog`]. The log keeps its records
//! in memory for query speed but each record is also framed for a write-ahead
//! file:
//!
//! ```text
//!   frame        : magic "AUD1" | u32 payload_len | payload | u32 crc32
//!   payload      : seq u64 | tick u64 | action u8 | flags u8 |
//!                  pos i32 x3 | old_state u32 | new_state u32 |
//!                  item_count u16 |
//!                  actor u16 len + UTF-8 | dimension u16 len + UTF-8 |
//!                  item_id u16 len + UTF-8   (only when flags bit1 set)
//!   flags        : bit0 = rolled_back, bit1 = item_id present
//!   crc32        : IEEE 802.3 reflected CRC32 over `payload` only
//! ```
//!
//! All integers are big-endian, matching the `STV1` snapshot codec. Because
//! every frame carries its own length and checksum, a crash that tears a
//! write in the middle of a frame leaves every earlier committed frame
//! readable: [`AuditLog::recover`] walks frames until the first incomplete or
//! corrupt one and reports the dropped tail as a byte count
//! ([`AuditLog::recover_from_file`] returns `(AuditLog, truncated_tail_bytes)`).
//! A frame whose CRC is valid but whose payload will not decode — an unknown
//! action tag, a reserved flag bit, non-UTF-8 text — is treated the same way:
//! the WAL is append-only, so nothing after a bad frame can be trusted.
//!
//! Spatial lookups use a chunk-bucket index keyed
//! `(dimension, cx, cz) -> record indices`: a radius query only walks the
//! buckets the radius box can touch instead of scanning the whole log. The
//! index is rebuilt identically on recovery.
//!
//! Rollback is atomic in the CoreProtect sense:
//! [`AuditLog::rollback_blocks`] replays every matching, not-yet-rolled-back
//! block mutation ([`AuditAction::is_block_mutation`]) in strictly reverse
//! chronological order (`seq` descending), restores `old_state`, and flags
//! each record `rolled_back`. [`AuditLog::restore_blocks`] is the inverse —
//! it re-applies `new_state` to rolled-back records in forward `seq` order.
//! Both emit [`BlockRollbackDelta`]s that
//! [`AuditLog::apply_rollback_to_context`] (or the one-call
//! [`AuditLog::rollback_to_context`] / [`AuditLog::restore_to_context`])
//! writes straight into a live [`ServerContext`].

use crate::sharded::{BlockCoord, ChunkCoord, ServerContext};
use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::{self, Write};
use std::path::Path;

/// Frame magic: `AUD`it log, revision 1.
const AUDIT_MAGIC: &[u8; 4] = b"AUD1";
/// Fail-closed payload cap. A real record is two short strings plus fixed
/// fields — anything past a few MiB is a corrupt length, not a big record.
const MAX_FRAME_PAYLOAD: u32 = 4 * 1024 * 1024;
/// `flags` bit: the record has been undone by a rollback.
const FLAG_ROLLED_BACK: u8 = 0b01;
/// `flags` bit: the payload carries an `item_id` string.
const FLAG_HAS_ITEM: u8 = 0b10;

/// What an [`AuditRecord`] did to the world.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum AuditAction {
    /// Actor placed a block: `old_state` -> `new_state`.
    BlockPlace,
    /// Actor broke a block: `old_state` -> `new_state` (usually air).
    BlockBreak,
    /// Non-player source (`#tnt`, `#creeper`, `#fire`) destroyed blocks.
    Explosion,
    /// Actor put `item_count` of `item_id` into a container.
    ContainerInsert,
    /// Actor removed `item_count` of `item_id` from a container.
    ContainerTake,
    /// Actor killed an entity; `item_id` carries the entity kind.
    EntityKill,
}

impl AuditAction {
    /// Block mutations restore `old_state` on rollback; container and entity
    /// records are audit-only — undoing them needs inventory/entity replay,
    /// which this log deliberately does not attempt.
    pub fn is_block_mutation(self) -> bool {
        matches!(
            self,
            AuditAction::BlockPlace | AuditAction::BlockBreak | AuditAction::Explosion
        )
    }

    fn tag(self) -> u8 {
        match self {
            AuditAction::BlockPlace => 0,
            AuditAction::BlockBreak => 1,
            AuditAction::Explosion => 2,
            AuditAction::ContainerInsert => 3,
            AuditAction::ContainerTake => 4,
            AuditAction::EntityKill => 5,
        }
    }

    fn from_tag(tag: u8) -> Option<Self> {
        match tag {
            0 => Some(AuditAction::BlockPlace),
            1 => Some(AuditAction::BlockBreak),
            2 => Some(AuditAction::Explosion),
            3 => Some(AuditAction::ContainerInsert),
            4 => Some(AuditAction::ContainerTake),
            5 => Some(AuditAction::EntityKill),
            _ => None,
        }
    }
}

/// One appended audit line. `seq` is assigned by [`AuditLog::append`]
/// (monotonically from 1) and preserved verbatim through the WAL — callers
/// constructing a record should pass `seq: 0` or use [`AuditRecord::new`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuditRecord {
    /// Log-assigned sequence number; rollback order is `seq` order.
    pub seq: u64,
    /// Game tick the action happened on.
    pub tick: u64,
    /// Player name, or a pseudo-actor like `#tnt` / `#creeper` / `#fire`.
    pub actor: String,
    /// Dimension id, e.g. `minecraft:overworld`.
    pub dimension: String,
    /// Absolute block position `[x, y, z]`.
    pub pos: [i32; 3],
    pub action: AuditAction,
    /// Block state before the action (`0` when not applicable).
    pub old_state: u32,
    /// Block state after the action (`0` when not applicable).
    pub new_state: u32,
    /// Item id (`minecraft:tnt`) or entity kind for non-block actions.
    pub item_id: Option<String>,
    /// Stack size for container actions (`0` when not applicable).
    pub item_count: u16,
    /// Set by rollback; cleared by restore. Persisted in the frame flags.
    pub rolled_back: bool,
}

impl AuditRecord {
    /// A record ready for [`AuditLog::append`]: `seq` is left `0` (the log
    /// overwrites it), `item_id`/`item_count`/`rolled_back` default empty.
    pub fn new(
        tick: u64,
        actor: impl Into<String>,
        dimension: impl Into<String>,
        pos: [i32; 3],
        action: AuditAction,
        old_state: u32,
        new_state: u32,
    ) -> Self {
        Self {
            seq: 0,
            tick,
            actor: actor.into(),
            dimension: dimension.into(),
            pos,
            action,
            old_state,
            new_state,
            item_id: None,
            item_count: 0,
            rolled_back: false,
        }
    }
}

/// Filter for [`AuditLog::query`], `rollback_blocks` and `restore_blocks`.
/// Every `None` field matches everything; set fields must all match.
/// `center` is `(center_pos, radius)`: a record matches when its position is
/// within Euclidean `radius` blocks of `center_pos`.
#[derive(Clone, Debug, Default)]
pub struct AuditQuery {
    /// Exact actor name (`"Griefer"`, `"#tnt"`).
    pub actor: Option<String>,
    /// Exact dimension id.
    pub dimension: Option<String>,
    /// `(center_pos, radius)` Euclidean sphere filter. A negative radius is
    /// clamped to `0`, matching only the center block itself.
    pub center: Option<([i32; 3], i32)>,
    /// Only records with `tick >= since_tick` (inclusive).
    pub since_tick: Option<u64>,
    /// Only these actions (`None` matches every action).
    pub actions: Option<Vec<AuditAction>>,
}

impl AuditQuery {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn actor(mut self, actor: impl Into<String>) -> Self {
        self.actor = Some(actor.into());
        self
    }

    pub fn dimension(mut self, dimension: impl Into<String>) -> Self {
        self.dimension = Some(dimension.into());
        self
    }

    /// Restrict to positions within `radius` blocks of `center_pos`.
    pub fn within(mut self, center_pos: [i32; 3], radius: i32) -> Self {
        self.center = Some((center_pos, radius));
        self
    }

    pub fn since_tick(mut self, tick: u64) -> Self {
        self.since_tick = Some(tick);
        self
    }

    pub fn actions(mut self, actions: impl IntoIterator<Item = AuditAction>) -> Self {
        self.actions = Some(actions.into_iter().collect());
        self
    }
}

/// One block write a rollback or restore produced: set `pos` in `dimension`
/// to `restored_state`. `seq` names the audit record that authored the write,
/// so callers can cross-reference the log.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlockRollbackDelta {
    pub seq: u64,
    pub dimension: String,
    pub pos: [i32; 3],
    pub restored_state: u32,
}

/// Append-only audit log: in-memory records plus a `(dimension, cx, cz)`
/// chunk-bucket index over them. Records are never reordered or renumbered,
/// so `seq` order is history order.
#[derive(Default)]
pub struct AuditLog {
    records: Vec<AuditRecord>,
    /// Next sequence number `append` hands out (starts at 1).
    next_seq: u64,
    /// `dimension -> (cx, cz) -> record indices`. `HashMap<String, _>` keys
    /// the outer map so radius queries pay one string hash per lookup box,
    /// not per key.
    chunk_index: HashMap<String, HashMap<(i32, i32), Vec<usize>>>,
}

impl AuditLog {
    pub fn new() -> Self {
        Self {
            records: Vec::new(),
            next_seq: 1,
            chunk_index: HashMap::new(),
        }
    }

    pub fn len(&self) -> usize {
        self.records.len()
    }

    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    /// All records in append (`seq`) order.
    pub fn records(&self) -> &[AuditRecord] {
        &self.records
    }

    /// The record with sequence number `seq`, wherever it sits in the log.
    pub fn record(&self, seq: u64) -> Option<&AuditRecord> {
        self.records.iter().find(|r| r.seq == seq)
    }

    /// Append a record, assigning its `seq` (the incoming value is
    /// overwritten), indexing its chunk bucket, and returning the `seq`.
    pub fn append(&mut self, mut record: AuditRecord) -> u64 {
        record.seq = self.next_seq;
        self.next_seq += 1;
        let seq = record.seq;
        self.records.push(record);
        self.index_record(self.records.len() - 1);
        seq
    }

    /// Append a record and write its CRC32 frame to `out`. The record is
    /// committed to memory only after the frame is fully written, so a failed
    /// write leaves the log unchanged.
    pub fn append_to<W: Write>(&mut self, out: &mut W, mut record: AuditRecord) -> io::Result<u64> {
        record.seq = self.next_seq;
        let frame = Self::encode_frame(&record);
        out.write_all(&frame)?;
        self.next_seq += 1;
        self.records.push(record);
        self.index_record(self.records.len() - 1);
        Ok(self.next_seq - 1)
    }

    /// Open `path` in append mode and write the record's frame. Existing
    /// bytes are never touched — the file stays a pure WAL.
    pub fn append_to_file<P: AsRef<Path>>(
        &mut self,
        path: P,
        record: AuditRecord,
    ) -> io::Result<u64> {
        let mut file = OpenOptions::new().create(true).append(true).open(path)?;
        self.append_to(&mut file, record)
    }

    /// Every record matching `query`, in chronological (`seq`) order.
    pub fn query(&self, query: &AuditQuery) -> Vec<&AuditRecord> {
        self.matching_indices(query)
            .into_iter()
            .map(|i| &self.records[i])
            .collect()
    }

    /// Roll back every matching, not-yet-rolled-back block mutation in strict
    /// reverse chronological order: the newest record's `old_state` is
    /// restored first, the oldest last, so each position converges to the
    /// state it held before the earliest matched edit. Matched records are
    /// flagged `rolled_back` so a second call is a no-op. Container and
    /// entity records never emit deltas.
    pub fn rollback_blocks(&mut self, query: &AuditQuery) -> Vec<BlockRollbackDelta> {
        let mut indices = self.matching_indices(query);
        indices.sort_by(|&a, &b| self.records[b].seq.cmp(&self.records[a].seq));
        let mut deltas = Vec::new();
        for i in indices {
            let record = &mut self.records[i];
            if record.rolled_back || !record.action.is_block_mutation() {
                continue;
            }
            record.rolled_back = true;
            deltas.push(BlockRollbackDelta {
                seq: record.seq,
                dimension: record.dimension.clone(),
                pos: record.pos,
                restored_state: record.old_state,
            });
        }
        deltas
    }

    /// Undo a rollback: every matching record currently flagged `rolled_back`
    /// gets its `new_state` re-applied in forward `seq` order and the flag is
    /// cleared, leaving the log exactly as it was before `rollback_blocks`.
    pub fn restore_blocks(&mut self, query: &AuditQuery) -> Vec<BlockRollbackDelta> {
        let mut indices = self.matching_indices(query);
        indices.sort_by_key(|&i| self.records[i].seq);
        let mut deltas = Vec::new();
        for i in indices {
            let record = &mut self.records[i];
            if !record.rolled_back || !record.action.is_block_mutation() {
                continue;
            }
            record.rolled_back = false;
            deltas.push(BlockRollbackDelta {
                seq: record.seq,
                dimension: record.dimension.clone(),
                pos: record.pos,
                restored_state: record.new_state,
            });
        }
        deltas
    }

    /// Write rollback/restore deltas into a live [`ServerContext`]. All
    /// deltas of one dimension are applied inside a single
    /// `with_chunks_write` call, so per dimension the write set is atomic and
    /// shard locks are taken in the store's ascending order.
    pub fn apply_rollback_to_context(&self, ctx: &ServerContext, deltas: &[BlockRollbackDelta]) {
        let mut by_dimension: HashMap<&str, Vec<&BlockRollbackDelta>> = HashMap::new();
        for delta in deltas {
            by_dimension
                .entry(delta.dimension.as_str())
                .or_default()
                .push(delta);
        }
        for (name, group) in by_dimension {
            let dimension = ctx.ensure_dimension(name);
            let coords: Vec<ChunkCoord> = group
                .iter()
                .map(|d| BlockCoord::new(d.pos[0], d.pos[1], d.pos[2]).chunk_coord())
                .collect();
            dimension.with_chunks_write(&coords, |set| {
                for delta in group {
                    set.set_block(
                        BlockCoord::new(delta.pos[0], delta.pos[1], delta.pos[2]),
                        delta.restored_state,
                    );
                }
            });
        }
    }

    /// `rollback_blocks` + `apply_rollback_to_context` in one call.
    pub fn rollback_to_context(
        &mut self,
        ctx: &ServerContext,
        query: &AuditQuery,
    ) -> Vec<BlockRollbackDelta> {
        let deltas = self.rollback_blocks(query);
        self.apply_rollback_to_context(ctx, &deltas);
        deltas
    }

    /// `restore_blocks` + `apply_rollback_to_context` in one call.
    pub fn restore_to_context(
        &mut self,
        ctx: &ServerContext,
        query: &AuditQuery,
    ) -> Vec<BlockRollbackDelta> {
        let deltas = self.restore_blocks(query);
        self.apply_rollback_to_context(ctx, &deltas);
        deltas
    }

    /// The CRC32-framed `AUD1` bytes for one record — the exact byte string
    /// `append_to` writes and `recover` reads.
    pub fn encode_frame(record: &AuditRecord) -> Vec<u8> {
        let payload = encode_payload(record);
        let mut frame = Vec::with_capacity(4 + 4 + payload.len() + 4);
        frame.extend_from_slice(AUDIT_MAGIC);
        frame.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        frame.extend_from_slice(&payload);
        frame.extend_from_slice(&crc32(&payload).to_be_bytes());
        frame
    }

    /// Rebuild a log from framed bytes. Returns `(log, truncated_tail_bytes)`:
    /// every complete, CRC-valid frame before the first gap is recovered;
    /// the remaining bytes — a torn write, a corrupt frame, anything — are
    /// reported by count and dropped.
    pub fn recover(bytes: &[u8]) -> (Self, usize) {
        let mut log = Self::new();
        let mut offset = 0usize;
        while offset < bytes.len() {
            match decode_frame(&bytes[offset..]) {
                Some((record, consumed)) => {
                    log.push_recovered(record);
                    offset += consumed;
                }
                None => break,
            }
        }
        (log, bytes.len() - offset)
    }

    /// `recover` over a whole file. Only file I/O can produce `Err`; a
    /// truncated or corrupt tail is data, not an error.
    pub fn recover_from_file<P: AsRef<Path>>(path: P) -> io::Result<(Self, usize)> {
        let bytes = std::fs::read(path)?;
        Ok(Self::recover(&bytes))
    }

    /// Insert a record exactly as stored (its `seq` is trusted, not
    /// re-assigned), index it, and push `next_seq` past it.
    fn push_recovered(&mut self, record: AuditRecord) {
        self.next_seq = self.next_seq.max(record.seq + 1);
        self.records.push(record);
        self.index_record(self.records.len() - 1);
    }

    /// Add record `index` to the `(dimension, cx, cz)` bucket index.
    fn index_record(&mut self, index: usize) {
        let record = &self.records[index];
        let cx = record.pos[0].div_euclid(16);
        let cz = record.pos[2].div_euclid(16);
        self.chunk_index
            .entry(record.dimension.clone())
            .or_default()
            .entry((cx, cz))
            .or_default()
            .push(index);
    }

    /// Indices of every record matching `query`, in chronological order.
    /// Spatial queries consult the chunk index (only buckets intersecting the
    /// radius box are walked); dimension-only queries walk one dimension's
    /// buckets; everything else is a linear scan over the records.
    fn matching_indices(&self, query: &AuditQuery) -> Vec<usize> {
        let mut indices: Vec<usize> = if let Some((center, radius)) = query.center {
            let radius = radius.max(0) as i64;
            // A matching position's x/z must lie in [c-r, c+r]; the chunk
            // box covering that interval bounds every bucket we must read.
            // Real positions are i32, so the box is clamped to the chunk
            // range they can occupy even for absurd radii.
            let chunk_lo = (i32::MIN as i64).div_euclid(16);
            let chunk_hi = (i32::MAX as i64).div_euclid(16);
            let cx_lo = (((center[0] as i64) - radius).div_euclid(16)).clamp(chunk_lo, chunk_hi);
            let cx_hi = (((center[0] as i64) + radius).div_euclid(16)).clamp(chunk_lo, chunk_hi);
            let cz_lo = (((center[2] as i64) - radius).div_euclid(16)).clamp(chunk_lo, chunk_hi);
            let cz_hi = (((center[2] as i64) + radius).div_euclid(16)).clamp(chunk_lo, chunk_hi);
            let mut out = Vec::new();
            let mut collect = |buckets: &HashMap<(i32, i32), Vec<usize>>| {
                for cx in cx_lo..=cx_hi {
                    for cz in cz_lo..=cz_hi {
                        if let Some(bucket) = buckets.get(&(cx as i32, cz as i32)) {
                            out.extend_from_slice(bucket);
                        }
                    }
                }
            };
            match &query.dimension {
                Some(name) => {
                    if let Some(buckets) = self.chunk_index.get(name) {
                        collect(buckets);
                    }
                }
                None => {
                    for buckets in self.chunk_index.values() {
                        collect(buckets);
                    }
                }
            }
            out.sort_unstable();
            out
        } else if let Some(name) = &query.dimension {
            let mut out: Vec<usize> = self
                .chunk_index
                .get(name)
                .map(|buckets| buckets.values().flatten().copied().collect())
                .unwrap_or_default();
            out.sort_unstable();
            out
        } else {
            (0..self.records.len()).collect()
        };
        indices.retain(|&i| Self::matches(&self.records[i], query));
        indices
    }

    fn matches(record: &AuditRecord, query: &AuditQuery) -> bool {
        if let Some(actor) = &query.actor {
            if record.actor != *actor {
                return false;
            }
        }
        if let Some(dimension) = &query.dimension {
            if record.dimension != *dimension {
                return false;
            }
        }
        if let Some(since) = query.since_tick {
            if record.tick < since {
                return false;
            }
        }
        if let Some(actions) = &query.actions {
            if !actions.contains(&record.action) {
                return false;
            }
        }
        if let Some((center, radius)) = query.center {
            let radius = radius.max(0) as i64;
            let dx = record.pos[0] as i64 - center[0] as i64;
            let dy = record.pos[1] as i64 - center[1] as i64;
            let dz = record.pos[2] as i64 - center[2] as i64;
            if dx * dx + dy * dy + dz * dz > radius * radius {
                return false;
            }
        }
        true
    }
}

// ------------------------------------------------------------------ codec

fn put_u16(out: &mut Vec<u8>, v: u16) {
    out.extend_from_slice(&v.to_be_bytes());
}

fn put_u32(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_be_bytes());
}

fn put_i32(out: &mut Vec<u8>, v: i32) {
    out.extend_from_slice(&v.to_be_bytes());
}

fn put_u64(out: &mut Vec<u8>, v: u64) {
    out.extend_from_slice(&v.to_be_bytes());
}

/// `u16` length prefix + UTF-8 bytes. Strings longer than `u16::MAX` are cut
/// back to a char boundary so the length field stays honest.
fn put_str(out: &mut Vec<u8>, s: &str) {
    let max = u16::MAX as usize;
    let mut len = s.len().min(max);
    while !s.is_char_boundary(len) {
        len -= 1;
    }
    put_u16(out, len as u16);
    out.extend_from_slice(&s.as_bytes()[..len]);
}

fn encode_payload(record: &AuditRecord) -> Vec<u8> {
    let extra = record.item_id.as_deref().map_or(0, |s| s.len() + 2);
    let mut out = Vec::with_capacity(
        8 + 8
            + 1
            + 1
            + 12
            + 4
            + 4
            + 2
            + record.actor.len()
            + 2
            + record.dimension.len()
            + 2
            + extra,
    );
    put_u64(&mut out, record.seq);
    put_u64(&mut out, record.tick);
    out.push(record.action.tag());
    let mut flags = 0u8;
    if record.rolled_back {
        flags |= FLAG_ROLLED_BACK;
    }
    if record.item_id.is_some() {
        flags |= FLAG_HAS_ITEM;
    }
    out.push(flags);
    for axis in record.pos {
        put_i32(&mut out, axis);
    }
    put_u32(&mut out, record.old_state);
    put_u32(&mut out, record.new_state);
    put_u16(&mut out, record.item_count);
    put_str(&mut out, &record.actor);
    put_str(&mut out, &record.dimension);
    if let Some(item_id) = &record.item_id {
        put_str(&mut out, item_id);
    }
    out
}

/// Bounds-checked little reader for one payload. Every getter returns `None`
/// on a short or invalid read; the caller maps that to a rejected frame.
struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let end = self.pos.checked_add(n)?;
        let slice = self.buf.get(self.pos..end)?;
        self.pos = end;
        Some(slice)
    }

    fn u8(&mut self) -> Option<u8> {
        Some(self.take(1)?[0])
    }

    fn u16(&mut self) -> Option<u16> {
        Some(u16::from_be_bytes(self.take(2)?.try_into().ok()?))
    }

    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_be_bytes(self.take(4)?.try_into().ok()?))
    }

    fn i32(&mut self) -> Option<i32> {
        Some(i32::from_be_bytes(self.take(4)?.try_into().ok()?))
    }

    fn u64(&mut self) -> Option<u64> {
        Some(u64::from_be_bytes(self.take(8)?.try_into().ok()?))
    }

    /// `u16` length-prefixed UTF-8. The `u16` bound is the cap — a longer
    /// string cannot have been encoded.
    fn string(&mut self) -> Option<String> {
        let len = self.u16()? as usize;
        let bytes = self.take(len)?;
        String::from_utf8(bytes.to_vec()).ok()
    }

    fn is_done(&self) -> bool {
        self.pos == self.buf.len()
    }
}

/// Decode one payload; `None` means the bytes cannot be a record.
fn decode_payload(payload: &[u8]) -> Option<AuditRecord> {
    let mut r = Reader::new(payload);
    let seq = r.u64()?;
    let tick = r.u64()?;
    let action = AuditAction::from_tag(r.u8()?)?;
    let flags = r.u8()?;
    if flags & !(FLAG_ROLLED_BACK | FLAG_HAS_ITEM) != 0 {
        return None;
    }
    let x = r.i32()?;
    let y = r.i32()?;
    let z = r.i32()?;
    let old_state = r.u32()?;
    let new_state = r.u32()?;
    let item_count = r.u16()?;
    let actor = r.string()?;
    let dimension = r.string()?;
    let item_id = if flags & FLAG_HAS_ITEM != 0 {
        Some(r.string()?)
    } else {
        None
    };
    if !r.is_done() {
        return None;
    }
    Some(AuditRecord {
        seq,
        tick,
        actor,
        dimension,
        pos: [x, y, z],
        action,
        old_state,
        new_state,
        item_id,
        item_count,
        rolled_back: flags & FLAG_ROLLED_BACK != 0,
    })
}

/// Decode the frame at the start of `bytes`; returns the record and the frame
/// length. `None` on anything that is not a complete, CRC-valid `AUD1` frame.
fn decode_frame(bytes: &[u8]) -> Option<(AuditRecord, usize)> {
    if bytes.len() < 8 || &bytes[..4] != AUDIT_MAGIC {
        return None;
    }
    let len = u32::from_be_bytes(bytes[4..8].try_into().ok()?);
    if len > MAX_FRAME_PAYLOAD {
        return None;
    }
    let len = len as usize;
    let end = 8usize.checked_add(len)?.checked_add(4)?;
    if bytes.len() < end {
        return None;
    }
    let payload = &bytes[8..8 + len];
    let stored_crc = u32::from_be_bytes(bytes[8 + len..end].try_into().ok()?);
    if crc32(payload) != stored_crc {
        return None;
    }
    Some((decode_payload(payload)?, end))
}

// ------------------------------------------------------------------ CRC32

/// IEEE 802.3 reflected CRC32 (the Ethernet/`zlib` polynomial), built at
/// compile time — the same table `snapshot.rs` and `anvil_inspect.rs` carry.
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

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(seq_ignored: u64, tick: u64, actor: &str, pos: [i32; 3]) -> AuditRecord {
        let mut r = AuditRecord::new(
            tick,
            actor,
            "minecraft:overworld",
            pos,
            AuditAction::BlockPlace,
            10,
            20,
        );
        r.seq = seq_ignored;
        r
    }

    #[test]
    fn frame_roundtrip_preserves_every_field() {
        let mut r = AuditRecord::new(
            77,
            "Griefer",
            "minecraft:the_nether",
            [-33, -64, 128],
            AuditAction::ContainerTake,
            5,
            0,
        );
        r.seq = 42;
        r.item_id = Some("minecraft:diamond_block".to_string());
        r.item_count = 13;
        r.rolled_back = true;
        let frame = AuditLog::encode_frame(&r);
        let (decoded, consumed) = decode_frame(&frame).expect("frame must decode");
        assert_eq!(consumed, frame.len());
        assert_eq!(decoded, r);
    }

    #[test]
    fn recover_stops_at_first_bad_frame_and_counts_tail() {
        let good_a = AuditLog::encode_frame(&rec(1, 1, "a", [0, 0, 0]));
        let good_b = AuditLog::encode_frame(&rec(2, 2, "b", [16, 0, 0]));
        let good_c = AuditLog::encode_frame(&rec(3, 3, "c", [32, 0, 0]));

        // A torn trailing write: full frames survive, tail is counted.
        let mut bytes = good_a.clone();
        bytes.extend_from_slice(&good_b);
        bytes.extend_from_slice(&good_c[..good_c.len() / 2]);
        let tail = bytes.len() - good_a.len() - good_b.len();
        let (log, truncated) = AuditLog::recover(&bytes);
        assert_eq!(log.len(), 2);
        assert_eq!(truncated, tail);

        // A corrupt middle frame: frames after it are untrusted too.
        let mut bytes = good_a.clone();
        let mut bad = good_b.clone();
        let n = bad.len();
        bad[n - 5] ^= 0xff; // flip a payload byte
        bytes.extend_from_slice(&bad);
        bytes.extend_from_slice(&good_c);
        let (log, truncated) = AuditLog::recover(&bytes);
        assert_eq!(log.len(), 1);
        assert_eq!(truncated, bad.len() + good_c.len());
    }

    #[test]
    fn rollback_replays_newest_first_and_converges_to_oldest_state() {
        let mut log = AuditLog::new();
        let pos = [8, 64, 8];
        // seq1: 0 -> 1, seq2: 1 -> 2, seq3: 2 -> 3.
        for (old, new) in [(0, 1), (1, 2), (2, 3)] {
            let mut r = rec(0, 10, "g", pos);
            r.old_state = old;
            r.new_state = new;
            log.append(r);
        }
        let deltas = log.rollback_blocks(&AuditQuery::new());
        assert_eq!(
            deltas.iter().map(|d| d.restored_state).collect::<Vec<_>>(),
            vec![2, 1, 0],
            "reverse seq order restores each record's old_state"
        );
        assert!(log.records().iter().all(|r| r.rolled_back));
        // Second rollback is a no-op.
        assert!(log.rollback_blocks(&AuditQuery::new()).is_empty());
        // Restore replays new_state forward.
        let deltas = log.restore_blocks(&AuditQuery::new());
        assert_eq!(
            deltas.iter().map(|d| d.restored_state).collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
        assert!(log.records().iter().all(|r| !r.rolled_back));
    }

    #[test]
    fn query_filters_compose() {
        let mut log = AuditLog::new();
        log.append(rec(0, 5, "Alice", [0, 64, 0]));
        log.append(rec(0, 20, "Griefer", [4, 64, 0]));
        let mut far = rec(0, 30, "Griefer", [10_000, 64, 0]);
        far.action = AuditAction::Explosion;
        log.append(far);

        let q = AuditQuery::new()
            .actor("Griefer")
            .within([0, 64, 0], 10)
            .since_tick(10);
        let hits = log.query(&q);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].pos, [4, 64, 0]);

        // Actions filter.
        let q = AuditQuery::new().actions([AuditAction::Explosion]);
        assert_eq!(log.query(&q).len(), 1);
    }
}
