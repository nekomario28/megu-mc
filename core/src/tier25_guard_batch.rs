// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! `megu_core::tier25_guard_batch` — the Tier-2.5 zero-JNI-idle,
//! O(1)-per-tick batching layer between native hot paths and bounded JVM
//! loader hosts (NeoForge / Fabric).
//!
//! When a mod carries only a handful of Tier-3 mixin sites — Create's
//! `LavaSwimmingMixin` on `LivingEntity#travel`, `SmithingMenu#onTake`,
//! `MinecartFurnace#fuel`, and the like — the dominant cost is not the
//! mixin itself but the JNI round-trip spent discovering that the site's
//! entry condition does not hold this tick. This module removes that cost
//! with two pure-`std` primitives:
//!
//! * [`GuardPredicate`] + [`GuardEvalContext`] — an entry-guard
//!   pre-filter evaluated entirely in Rust (~ns: a handful of field reads
//!   and compares) *before* any crossing into a JVM loader host. On the
//!   overwhelming majority of ticks the guard is false, the submit short
//!   circuits to `Ok(false)`, and the JNI crossing count stays exactly 0.
//! * [`TickBatchCoalescer`] — per-loader-lane tick batching: every event
//!   that survives its guard during a tick is queued (with last-write-wins
//!   dedup on caller-marked `coalesce_key`s), and [`flush_tick`] serializes
//!   all `1..=MAX_BATCH_EVENTS` survivors into ONE flat big-endian
//!   `TB25` [`FlatBatchFrame`]-compatible buffer, so a non-idle tick costs
//!   at most one JNI call — `O(1)` regardless of entity count.
//!
//! Fail-closed invariants on hostile input:
//!
//! * guards are bounded: [`MAX_GUARD_DEPTH`] nesting and
//!   [`MAX_GUARD_NODES`] total nodes, enforced by
//!   [`GuardPredicate::validate`]; [`GuardPredicate::eval`] itself is
//!   depth-capped so even an unvalidated hostile tree cannot recurse
//!   deeper than the bound or panic;
//! * the batch is bounded: [`MAX_BATCH_EVENTS`] queued events per lane per
//!   tick and [`MAX_EVENT_PAYLOAD_BYTES`] per event payload;
//! * [`TickBatchCoalescer::decode_batch_frame`] verifies the `TB25` magic,
//!   the frame version, the loader lane id, zeroed reserved bytes, the
//!   declared record count, every record bound, a strict trailing-bytes
//!   check, and finally the trailing IEEE CRC32 — any deviation returns
//!   [`Tier25Error`] and yields no partial data.
//!
//! Everything is deterministic (insertion-stable record order, no clocks,
//! no randomness), panic-free on hostile input, `&mut`-driven like every
//! other `core` primitive, and free of interior mutability.
//!
//! [`flush_tick`]: TickBatchCoalescer::flush_tick
//! [`FlatBatchFrame`]: TickBatchCoalescer::decode_batch_frame

use std::collections::HashMap;
use std::error::Error;
use std::fmt;

// ---------------------------------------------------------------------------
// Bounds and wire constants
// ---------------------------------------------------------------------------

/// Maximum boolean-combinator nesting depth accepted by
/// [`GuardPredicate::validate`] and honored by [`GuardPredicate::eval`].
/// The root predicate sits at depth 1, so eight levels of `And`/`Or`/`Not`
/// wrapping a leaf is the deepest legal guard.
pub const MAX_GUARD_DEPTH: u32 = 8;

/// Maximum total nodes (leaves + combinators) a validated guard tree may
/// contain. Bounds both validation and worst-case evaluation work.
pub const MAX_GUARD_NODES: u32 = 32;

/// Maximum events one loader lane may queue in a single tick. The 4 097th
/// submission fails closed with [`Tier25Error::BatchFull`].
pub const MAX_BATCH_EVENTS: usize = 4096;

/// Maximum per-event opaque payload bytes accepted into the batch. Larger
/// payloads must go through the shared-memory side channel, not the flat
/// frame.
pub const MAX_EVENT_PAYLOAD_BYTES: usize = 1024;

/// Flat frame magic: ASCII `TB25` (Tier-2.5 Batch), big-endian.
pub const FRAME_MAGIC: [u8; 4] = *b"TB25";

/// Current flat frame format version.
pub const FRAME_VERSION: u8 = 1;

/// Fixed frame header size: magic(4) + version(1) + loader(1) +
/// reserved(2) + tick(8) + count(4).
const FRAME_HEADER_BYTES: usize = 20;

/// Per-record fixed prefix size: hook_id(4) + entity_id(4) + flags(2) +
/// key_kind(1) + coalesce_key(8) + payload_len(2).
const RECORD_HEADER_BYTES: usize = 21;

/// Trailing CRC32 size.
const FRAME_TRAILER_BYTES: usize = 4;

/// Number of distinct loader lanes (array index = `loader as usize - 1`).
const LANE_COUNT: usize = 2;

// ---------------------------------------------------------------------------
// Guard predicates
// ---------------------------------------------------------------------------

/// A zero-cost hot-path entry guard evaluated entirely in Rust before any
/// JNI crossing. Leaves read one precomputed fact out of
/// [`GuardEvalContext`]; `And`/`Or`/`Not` combine them with bounded
/// nesting (see [`MAX_GUARD_DEPTH`] / [`MAX_GUARD_NODES`]).
///
/// Cheap to evaluate (~a few ns for a typical 1–3 node tree) and
/// allocation-free on every path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GuardPredicate {
    /// `LivingEntity#isInLava` — Create `LavaSwimmingMixin`-style sites.
    EntityInLava,
    /// `LivingEntity#isInWater`.
    EntityInWater,
    /// `Entity#onGround`.
    EntityOnGround,
    /// `Player#isShiftKeyDown` (sneaking).
    PlayerSneaking,
    /// Entity has the status effect at bit `effect_bit` of
    /// [`GuardEvalContext::active_effects`]. `effect_bit` must be `< 64`.
    EntityHasEffect {
        /// Bit index (0..=63) into the active-effects bitmask.
        effect_bit: u32,
    },
    /// Entity's registry type id equals `entity_type_id`.
    EntityTypeMatches {
        /// Expected `entity_type_id`.
        entity_type_id: u32,
    },
    /// Entity's dimension id equals `dim_id`.
    DimensionMatches {
        /// Expected `dim_id`.
        dim_id: u32,
    },
    /// Held-item content hash equals `item_hash` — e.g. a specific fuel
    /// item for `MinecartFurnace#fuel` sites.
    HoldingItemId {
        /// Expected `held_item_hash`.
        item_hash: u64,
    },
    /// Container/slot index equals `slot` — e.g. `SmithingMenu#onTake`
    /// result-slot sites.
    SlotIndexEquals {
        /// Expected `slot_index`.
        slot: u32,
    },
    /// All children must hold. Must be non-empty (an empty `And` is a
    /// spec bug, not a vacuous truth) — [`validate`] rejects it.
    And(Vec<GuardPredicate>),
    /// At least one child must hold. Must be non-empty.
    Or(Vec<GuardPredicate>),
    /// Logical negation of the child.
    Not(Box<GuardPredicate>),
}

impl GuardPredicate {
    /// Fail-closed structural validation: combinator nesting at or below
    /// [`MAX_GUARD_DEPTH`], total node count at or below
    /// [`MAX_GUARD_NODES`], non-empty combinators, and in-range leaf
    /// parameters. Must be called (and must pass) before a guard is
    /// submitted to [`TickBatchCoalescer::submit_guarded`]; submitting an
    /// invalid guard is an error, never a silent pass.
    pub fn validate(&self) -> Result<(), Tier25Error> {
        // Iterative so a hostile tree cannot stack-overflow validation.
        let mut nodes: u32 = 0;
        let mut stack: Vec<(&GuardPredicate, u32)> = Vec::with_capacity(16);
        stack.push((self, 1));
        while let Some((guard, depth)) = stack.pop() {
            if depth > MAX_GUARD_DEPTH {
                return Err(Tier25Error::GuardDepthExceeded { depth });
            }
            nodes += 1;
            if nodes > MAX_GUARD_NODES {
                return Err(Tier25Error::GuardNodeBudgetExceeded { nodes });
            }
            match guard {
                Self::EntityHasEffect { effect_bit } if *effect_bit >= 64 => {
                    return Err(Tier25Error::GuardInvalidEffectBit {
                        effect_bit: *effect_bit,
                    });
                }
                Self::And(children) | Self::Or(children) => {
                    if children.is_empty() {
                        return Err(Tier25Error::GuardEmptyCombinator);
                    }
                    stack.extend(children.iter().map(|c| (c, depth + 1)));
                }
                Self::Not(inner) => stack.push((inner.as_ref(), depth + 1)),
                _ => {}
            }
        }
        Ok(())
    }

    /// Evaluate the guard against one tick's precomputed facts. Pure:
    /// no allocation, no clock, no interior mutability. Depth-capped at
    /// [`MAX_GUARD_DEPTH`] even for unvalidated trees — a node deeper
    /// than the bound evaluates `false` (fail-closed), so `eval` can
    /// never overflow the stack on a hostile predicate.
    #[inline]
    pub fn eval(&self, ctx: &GuardEvalContext) -> bool {
        self.eval_at(ctx, 1)
    }

    fn eval_at(&self, ctx: &GuardEvalContext, depth: u32) -> bool {
        if depth > MAX_GUARD_DEPTH {
            return false;
        }
        match self {
            Self::EntityInLava => ctx.in_lava,
            Self::EntityInWater => ctx.in_water,
            Self::EntityOnGround => ctx.on_ground,
            Self::PlayerSneaking => ctx.sneaking,
            Self::EntityHasEffect { effect_bit } => {
                *effect_bit < 64 && ctx.active_effects & (1u64 << effect_bit) != 0
            }
            Self::EntityTypeMatches { entity_type_id } => {
                ctx.entity_type_id == *entity_type_id
            }
            Self::DimensionMatches { dim_id } => ctx.dim_id == *dim_id,
            Self::HoldingItemId { item_hash } => ctx.held_item_hash == *item_hash,
            Self::SlotIndexEquals { slot } => ctx.slot_index == *slot,
            Self::And(children) => children.iter().all(|c| c.eval_at(ctx, depth + 1)),
            Self::Or(children) => children.iter().any(|c| c.eval_at(ctx, depth + 1)),
            Self::Not(inner) => !inner.eval_at(ctx, depth + 1),
        }
    }
}

/// The per-tick, per-entity facts a [`GuardPredicate`] reads. The mixin
/// site's Rust-side shim populates this from already-materialized state —
/// filling it must never itself touch JNI.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GuardEvalContext {
    /// Entity runtime id (or `0` for non-entity sites such as menus).
    pub entity_id: i32,
    /// Registry id of the entity's type (`EntityType<?>` ordinal).
    pub entity_type_id: u32,
    /// Dimension id (`ResourceLocation` hash or numeric dimension id).
    pub dim_id: u32,
    /// `Entity#isInLava`.
    pub in_lava: bool,
    /// `Entity#isInWater`.
    pub in_water: bool,
    /// `Entity#onGround`.
    pub on_ground: bool,
    /// `Player#isShiftKeyDown`.
    pub sneaking: bool,
    /// Bitmask of active `MobEffect` ids (bit `i` = effect id `i`).
    pub active_effects: u64,
    /// Content hash of the held/trigger item (0 = empty hand).
    pub held_item_hash: u64,
    /// Slot index for menu/container sites.
    pub slot_index: u32,
}

// ---------------------------------------------------------------------------
// Batch records and loader lanes
// ---------------------------------------------------------------------------

/// The bounded JVM loader host a batch lane drains into.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum LoaderLane {
    /// NeoForge loader host.
    NeoForge = 1,
    /// Fabric loader host.
    Fabric = 2,
}

impl LoaderLane {
    fn index(self) -> usize {
        self as usize - 1
    }

    fn from_wire(byte: u8) -> Result<Self, Tier25Error> {
        match byte {
            1 => Ok(Self::NeoForge),
            2 => Ok(Self::Fabric),
            other => Err(Tier25Error::UnknownLoader(other)),
        }
    }
}

/// One triggered mixin-site event queued for the next lane flush.
/// `coalesce_key`, when present, marks the event coalesceable: a later
/// submission in the same tick with the same key last-write-wins replaces
/// it in place, collapsing repeated identical triggers (e.g. per-entity
/// `travel` spam) into one record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BatchEventRecord {
    /// Mixin-site identifier assigned by the lane's site table.
    pub hook_id: u32,
    /// Entity runtime id the event is about (0 for non-entity sites).
    pub entity_id: i32,
    /// Site-defined flag bits (e.g. cancellable, phase).
    pub flags: u16,
    /// Optional dedup key; identical keys within a tick coalesce to the
    /// latest record.
    pub coalesce_key: Option<u64>,
    /// Opaque site payload, at most [`MAX_EVENT_PAYLOAD_BYTES`].
    pub payload: Vec<u8>,
}

/// A decoded `TB25` flat batch frame — the mirror of what
/// [`TickBatchCoalescer::flush_tick`] emits and what the JVM-side lane
/// consumer parses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodedBatchFrame {
    /// Frame format version ([`FRAME_VERSION`]).
    pub version: u8,
    /// Loader lane this frame drains into.
    pub loader: LoaderLane,
    /// Tick the batch was flushed for.
    pub tick: u64,
    /// Coalesced events in submission-stable order.
    pub events: Vec<BatchEventRecord>,
}

/// Deterministic, allocation-free telemetry for the guard + batch layer.
/// Counters are monotone u64s; `saturating` is unnecessary because every
/// increment is driven by a real call (a u64 can never wrap at tick
/// rates).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Tier25Metrics {
    /// `submit_guarded` calls that reached guard evaluation.
    pub evaluated: u64,
    /// Evaluations where the guard was false — the zero-JNI fast path.
    pub filtered_out: u64,
    /// Evaluations where the guard passed and the event entered the lane
    /// queue (includes events later coalesced into an existing key).
    pub dispatched: u64,
    /// Dispatched events that merged into an already-queued coalesce key
    /// instead of appending a new record.
    pub coalesced: u64,
    /// `flush_tick` calls that produced a `TB25` frame (the number of JNI
    /// crossings a compliant host performs).
    pub flushes: u64,
    /// `flush_tick` calls that found an empty lane and crossed JNI zero
    /// times.
    pub zero_crossing_ticks: u64,
    /// Submissions rejected with a [`Tier25Error`] (invalid guard,
    /// oversized payload, or full batch).
    pub rejected: u64,
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Why guard validation, batch submission, or frame decoding failed.
/// Every variant is fail-closed: no partial queue mutation or partial
/// decode escapes on error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Tier25Error {
    /// Guard combinator nesting exceeded [`MAX_GUARD_DEPTH`].
    GuardDepthExceeded {
        /// Depth at which the bound was hit.
        depth: u32,
    },
    /// Guard node count exceeded [`MAX_GUARD_NODES`].
    GuardNodeBudgetExceeded {
        /// Node count at which the bound was hit.
        nodes: u32,
    },
    /// `And`/`Or` with zero children.
    GuardEmptyCombinator,
    /// `EntityHasEffect` bit outside `0..64`.
    GuardInvalidEffectBit {
        /// Offending bit index.
        effect_bit: u32,
    },
    /// Event payload exceeded [`MAX_EVENT_PAYLOAD_BYTES`].
    PayloadTooLarge {
        /// Declared payload size.
        size: usize,
    },
    /// Lane already holds [`MAX_BATCH_EVENTS`] events this tick.
    BatchFull,
    /// Frame ended before a declared field/record completed.
    FrameTruncated,
    /// Leading magic bytes were not `TB25`.
    BadMagic,
    /// Frame version byte is not [`FRAME_VERSION`].
    UnsupportedVersion(u8),
    /// Loader byte names no known lane.
    UnknownLoader(u8),
    /// Structurally invalid header field or record.
    MalformedFrame(&'static str),
    /// Trailing CRC32 did not match the frame body.
    CrcMismatch,
    /// Extra bytes follow the CRC32 trailer.
    TrailingBytes,
}

impl fmt::Display for Tier25Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::GuardDepthExceeded { depth } => {
                write!(f, "guard nesting depth {depth} exceeds max {MAX_GUARD_DEPTH}")
            }
            Self::GuardNodeBudgetExceeded { nodes } => {
                write!(f, "guard node count {nodes} exceeds max {MAX_GUARD_NODES}")
            }
            Self::GuardEmptyCombinator => {
                f.write_str("guard And/Or combinator has no children")
            }
            Self::GuardInvalidEffectBit { effect_bit } => {
                write!(f, "guard effect bit {effect_bit} is outside 0..64")
            }
            Self::PayloadTooLarge { size } => write!(
                f,
                "event payload {size} bytes exceeds max {MAX_EVENT_PAYLOAD_BYTES}"
            ),
            Self::BatchFull => write!(f, "lane batch is at capacity {MAX_BATCH_EVENTS}"),
            Self::FrameTruncated => f.write_str("batch frame truncated"),
            Self::BadMagic => f.write_str("batch frame magic is not TB25"),
            Self::UnsupportedVersion(v) => {
                write!(f, "batch frame version {v} is not supported")
            }
            Self::UnknownLoader(b) => write!(f, "batch frame loader byte {b} is unknown"),
            Self::MalformedFrame(what) => write!(f, "malformed batch frame: {what}"),
            Self::CrcMismatch => f.write_str("batch frame CRC32 mismatch"),
            Self::TrailingBytes => {
                f.write_str("batch frame has trailing bytes after CRC32")
            }
        }
    }
}

impl Error for Tier25Error {}

// ---------------------------------------------------------------------------
// CRC32 (IEEE 802.3 reflected — same table as state-store snapshots)
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// Big-endian cursor helpers (bounds-checked, panic-free on hostile input)
// ---------------------------------------------------------------------------

fn push_u16(out: &mut Vec<u8>, v: u16) {
    out.extend_from_slice(&v.to_be_bytes());
}

fn push_u32(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_be_bytes());
}

fn push_u64(out: &mut Vec<u8>, v: u64) {
    out.extend_from_slice(&v.to_be_bytes());
}

fn take_u16(bytes: &[u8], cursor: &mut usize) -> Result<u16, Tier25Error> {
    let end = cursor.checked_add(2).ok_or(Tier25Error::FrameTruncated)?;
    let raw = bytes
        .get(*cursor..end)
        .ok_or(Tier25Error::FrameTruncated)?;
    *cursor = end;
    Ok(u16::from_be_bytes([raw[0], raw[1]]))
}

fn take_u32(bytes: &[u8], cursor: &mut usize) -> Result<u32, Tier25Error> {
    let end = cursor.checked_add(4).ok_or(Tier25Error::FrameTruncated)?;
    let raw = bytes
        .get(*cursor..end)
        .ok_or(Tier25Error::FrameTruncated)?;
    *cursor = end;
    Ok(u32::from_be_bytes([raw[0], raw[1], raw[2], raw[3]]))
}

fn take_u64(bytes: &[u8], cursor: &mut usize) -> Result<u64, Tier25Error> {
    let end = cursor.checked_add(8).ok_or(Tier25Error::FrameTruncated)?;
    let raw = bytes
        .get(*cursor..end)
        .ok_or(Tier25Error::FrameTruncated)?;
    *cursor = end;
    Ok(u64::from_be_bytes(raw.try_into().expect("8-byte slice")))
}

// ---------------------------------------------------------------------------
// Tick batch coalescer
// ---------------------------------------------------------------------------

/// Per-loader-lane tick event coalescer. All events that survive their
/// [`GuardPredicate`] during one tick accumulate in the lane's queue;
/// [`flush_tick`] then emits them as ONE flat `TB25` frame — so a tick
/// with N surviving events costs exactly 1 JNI crossing and an idle tick
/// costs 0.
///
/// [`flush_tick`]: TickBatchCoalescer::flush_tick
#[derive(Debug, Default)]
pub struct TickBatchCoalescer {
    queues: [Vec<BatchEventRecord>; LANE_COUNT],
    /// coalesce_key -> index into `queues[lane]`, for O(1) dedup.
    key_index: [HashMap<u64, usize>; LANE_COUNT],
    metrics: Tier25Metrics,
}

impl TickBatchCoalescer {
    /// Empty coalescer with zeroed metrics.
    pub fn new() -> Self {
        Self::default()
    }

    /// Hot-path submit: evaluate `guard` against `ctx` in pure Rust, and
    /// only queue `record` for `loader` when the guard holds.
    ///
    /// Returns `Ok(false)` — the zero-JNI fast path — when the guard
    /// filters the event out, `Ok(true)` when it is queued (or coalesced
    /// into an existing key). Fails closed:
    ///
    /// * an invalid guard (see [`GuardPredicate::validate`]) is an error,
    ///   never a silent pass/filter;
    /// * a payload over [`MAX_EVENT_PAYLOAD_BYTES`] is rejected;
    /// * a [`MAX_BATCH_EVENTS`]-deep queue rejects further events rather
    ///   than growing unboundedly.
    ///
    /// On `Err` the lane queue is unchanged.
    pub fn submit_guarded(
        &mut self,
        loader: LoaderLane,
        guard: &GuardPredicate,
        ctx: &GuardEvalContext,
        record: BatchEventRecord,
    ) -> Result<bool, Tier25Error> {
        if let Err(e) = guard.validate() {
            self.metrics.rejected += 1;
            return Err(e);
        }
        self.metrics.evaluated += 1;
        if !guard.eval(ctx) {
            self.metrics.filtered_out += 1;
            return Ok(false);
        }
        if record.payload.len() > MAX_EVENT_PAYLOAD_BYTES {
            self.metrics.rejected += 1;
            return Err(Tier25Error::PayloadTooLarge {
                size: record.payload.len(),
            });
        }
        let lane = loader.index();
        if let Some(key) = record.coalesce_key {
            if let Some(&idx) = self.key_index[lane].get(&key) {
                // Last-write-wins, in place: stable order, no reordering.
                self.queues[lane][idx] = record;
                self.metrics.dispatched += 1;
                self.metrics.coalesced += 1;
                return Ok(true);
            }
        }
        if self.queues[lane].len() >= MAX_BATCH_EVENTS {
            self.metrics.rejected += 1;
            return Err(Tier25Error::BatchFull);
        }
        if let Some(key) = record.coalesce_key {
            self.key_index[lane].insert(key, self.queues[lane].len());
        }
        self.queues[lane].push(record);
        self.metrics.dispatched += 1;
        Ok(true)
    }

    /// Events currently queued for `loader` this tick.
    pub fn pending_count(&self, loader: LoaderLane) -> usize {
        self.queues[loader.index()].len()
    }

    /// End-of-tick drain for `loader`. `None` — the zero-JNI answer —
    /// when nothing was queued this tick; otherwise ONE flat big-endian
    /// `TB25` frame covering every queued event (`O(1)` JNI crossings:
    /// exactly one, regardless of event count).
    ///
    /// The lane queue and its coalesce index are cleared either way, so a
    /// new tick always starts empty.
    pub fn flush_tick(&mut self, tick: u64, loader: LoaderLane) -> Option<Vec<u8>> {
        let lane = loader.index();
        let events = std::mem::take(&mut self.queues[lane]);
        self.key_index[lane].clear();
        if events.is_empty() {
            self.metrics.zero_crossing_ticks += 1;
            return None;
        }
        self.metrics.flushes += 1;
        Some(encode_batch_frame(tick, loader, &events))
    }

    /// Decode and strictly verify a `TB25` flat frame. Fail-closed end to
    /// end: magic, version, loader byte, zeroed reserved bytes, record
    /// count bound, per-record bounds, exact-length trailing check, and
    /// CRC32 are all verified; any deviation is a [`Tier25Error`] and no
    /// partial frame is returned.
    pub fn decode_batch_frame(bytes: &[u8]) -> Result<DecodedBatchFrame, Tier25Error> {
        if bytes.len() < FRAME_HEADER_BYTES + FRAME_TRAILER_BYTES {
            return Err(Tier25Error::FrameTruncated);
        }
        if bytes[0..4] != FRAME_MAGIC {
            return Err(Tier25Error::BadMagic);
        }
        let version = bytes[4];
        if version != FRAME_VERSION {
            return Err(Tier25Error::UnsupportedVersion(version));
        }
        let loader = LoaderLane::from_wire(bytes[5])?;
        if bytes[6] != 0 || bytes[7] != 0 {
            return Err(Tier25Error::MalformedFrame("non-zero reserved header bytes"));
        }
        let mut cursor = 8usize;
        let tick = take_u64(bytes, &mut cursor)?;
        let count = take_u32(bytes, &mut cursor)? as usize;
        if count > MAX_BATCH_EVENTS {
            return Err(Tier25Error::MalformedFrame("record count exceeds batch capacity"));
        }
        let mut events = Vec::with_capacity(count.min(MAX_BATCH_EVENTS));
        for _ in 0..count {
            let hook_id = take_u32(bytes, &mut cursor)?;
            let entity_id = take_u32(bytes, &mut cursor)? as i32;
            let flags = take_u16(bytes, &mut cursor)?;
            let key_kind = bytes.get(cursor).copied().ok_or(Tier25Error::FrameTruncated)?;
            cursor += 1;
            let coalesce_key = match key_kind {
                0 => {
                    if take_u64(bytes, &mut cursor)? != 0 {
                        return Err(Tier25Error::MalformedFrame(
                            "non-zero key field on keyless record",
                        ));
                    }
                    None
                }
                1 => Some(take_u64(bytes, &mut cursor)?),
                _ => {
                    return Err(Tier25Error::MalformedFrame("bad coalesce key kind byte"));
                }
            };
            let payload_len = take_u16(bytes, &mut cursor)? as usize;
            if payload_len > MAX_EVENT_PAYLOAD_BYTES {
                return Err(Tier25Error::MalformedFrame("record payload exceeds byte cap"));
            }
            let end = cursor
                .checked_add(payload_len)
                .ok_or(Tier25Error::FrameTruncated)?;
            let payload = bytes
                .get(cursor..end)
                .ok_or(Tier25Error::FrameTruncated)?
                .to_vec();
            cursor = end;
            events.push(BatchEventRecord {
                hook_id,
                entity_id,
                flags,
                coalesce_key,
                payload,
            });
        }
        let remaining = bytes.len() - cursor;
        if remaining < FRAME_TRAILER_BYTES {
            return Err(Tier25Error::FrameTruncated);
        }
        if remaining > FRAME_TRAILER_BYTES {
            return Err(Tier25Error::TrailingBytes);
        }
        let stored_crc = u32::from_be_bytes(
            bytes[cursor..cursor + 4].try_into().expect("4-byte slice"),
        );
        if crc32(&bytes[..cursor]) != stored_crc {
            return Err(Tier25Error::CrcMismatch);
        }
        Ok(DecodedBatchFrame {
            version,
            loader,
            tick,
            events,
        })
    }

    /// Current telemetry counters.
    pub fn metrics(&self) -> Tier25Metrics {
        self.metrics
    }
}

/// Serialize one lane's tick events into the flat `TB25` frame:
///
/// ```text
/// magic(4) "TB25" | version(1) | loader(1) | reserved(2)=0 |
/// tick(8) | count(4) |
/// records[count] { hook_id(4) | entity_id(4) | flags(2) |
///                  key_kind(1) | coalesce_key(8) | payload_len(2) |
///                  payload(payload_len) } |
/// crc32(4) — IEEE 802.3 over every preceding byte
/// ```
///
/// All integers big-endian; record order is the lane's stable submission
/// order, so identical submission sequences produce byte-identical
/// frames.
fn encode_batch_frame(tick: u64, loader: LoaderLane, events: &[BatchEventRecord]) -> Vec<u8> {
    let payload_bytes: usize = events.iter().map(|e| e.payload.len()).sum();
    let mut out = Vec::with_capacity(
        FRAME_HEADER_BYTES + events.len() * RECORD_HEADER_BYTES + payload_bytes + FRAME_TRAILER_BYTES,
    );
    out.extend_from_slice(&FRAME_MAGIC);
    out.push(FRAME_VERSION);
    out.push(loader as u8);
    out.extend_from_slice(&[0u8; 2]);
    push_u64(&mut out, tick);
    push_u32(&mut out, events.len() as u32);
    for e in events {
        push_u32(&mut out, e.hook_id);
        push_u32(&mut out, e.entity_id as u32);
        push_u16(&mut out, e.flags);
        match e.coalesce_key {
            Some(k) => {
                out.push(1);
                push_u64(&mut out, k);
            }
            None => {
                out.push(0);
                push_u64(&mut out, 0);
            }
        }
        // Callers enforce the cap; cast stays lossless.
        push_u16(&mut out, e.payload.len() as u16);
        out.extend_from_slice(&e.payload);
    }
    let crc = crc32(&out);
    push_u32(&mut out, crc);
    out
}
