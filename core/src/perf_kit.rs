// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! `megu_core::perf_kit` — reusable, zero-dependency performance
//! primitives for any block entity, modded machine, transport node, or
//! persistence unit.
//!
//! Everything in this module is pure `std`, allocation-bounded, and fully
//! deterministic: no clocks, no randomness, no unsafe, and nothing keyed to
//! a specific mod or vanilla subsystem. The six primitives compose into the
//! classic "sleep, wake exactly once, batch the result" loop the adapters
//! (`mod-adapter-core`, `create-adapter`, any future machine family) all
//! need:
//!
//! * [`DirtyFingerprintGate`] — 64-bit FNV-1a/splitmix fingerprint table for
//!   O(1) "did anything actually change?" checks. A machine whose state
//!   fingerprint is identical to the last committed one is skipped without
//!   re-running its tick logic; a mutation commits a new fingerprint so the
//!   change wakes it the same tick it happens.
//! * [`BoundedScratchVec`] — reusable scratch storage with a deterministic
//!   retention policy ([`BoundedScratchVec::clear_bounded`]) so burst spikes
//!   shrink back instead of inflating idle RSS, plus a [`growth_events`]
//!   counter that makes "zero churn after warmup" testable.
//! * [`TickBudgetQueue`] — idempotent FIFO work queue whose
//!   [`TickBudgetQueue::drain_budget`] slices an arbitrarily large burst into
//!   a fixed per-tick budget (items and cost), eliminating tick spikes from
//!   auto-saves, mesh uploads, and mass machine completions.
//! * [`EventWakeWheel`] — deterministic tick-indexed wake scheduler:
//!   `O(log n)` schedule/cancel, [`EventWakeWheel::pop_due`] returns exactly
//!   the machines whose wake tick arrived, in sorted order, so sleeping
//!   machines never poll.
//! * [`CoalescedDeltaBatch`] — last-write-wins delta accumulator that
//!   collapses repeated updates to the same key and drains in sorted,
//!   deterministic order for packet emission or disk flushes.
//! * [`AdaptiveLoadGovernor`] — fixed 120-sample sliding-window latency
//!   monitor with P50/P95/P99 percentiles and a hysteresis dead-band that
//!   eases a budget scale between `min_scale` and `1.0`, shedding load
//!   smoothly instead of oscillating.
//!
//! Fingerprints are 64-bit values, so [`DirtyFingerprintGate`] skips are
//! exact when fingerprints match and probabilistic otherwise: two distinct
//! states sharing one 64-bit fingerprint would produce a false skip. The
//! birthday bound over practical machine counts keeps that at the level of
//! hardware bit-flips, and the fingerprint itself is a pure function, so
//! every decision the gate makes is deterministic.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::hash::Hash;
use std::ops::{Deref, DerefMut};

// ---------------------------------------------------------------------------
// Fingerprinting — FNV-1a mixing with a splitmix64 avalanche finish
// ---------------------------------------------------------------------------

/// FNV-1a 64-bit offset basis.
const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
/// FNV-1a 64-bit prime.
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

/// Splitmix64 finalizer constant (golden ratio `2^64 / φ`).
const SPLITMIX_GAMMA: u64 = 0x9e37_79b9_7f4a_7c15;

/// FNV-1a over `bytes`. Merkle–Damgård-weak against extension only when
/// used bare — callers fingerprinting fixed records should prefer
/// [`fingerprint_bytes`], which avalanche-finishes the result.
#[inline]
pub fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut h = FNV_OFFSET;
    for &b in bytes {
        h ^= u64::from(b);
        h = h.wrapping_mul(FNV_PRIME);
    }
    h
}

/// `splitmix64` bijective avalanche — the same finalizer Java's
/// `SplittableRandom` uses. Deterministic, seed-free, and a permutation of
/// `u64`, so two different mixer states never collide here.
#[inline]
pub fn splitmix64(mut x: u64) -> u64 {
    x = x.wrapping_add(SPLITMIX_GAMMA);
    x = (x ^ (x >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    x ^ (x >> 31)
}

/// Canonical 64-bit fingerprint of a byte slice: FNV-1a mixing followed by
/// the splitmix64 avalanche so low-entropy tails distribute uniformly.
#[inline]
pub fn fingerprint_bytes(bytes: &[u8]) -> u64 {
    splitmix64(fnv1a64(bytes))
}

/// Incremental fingerprint builder for structured state.
///
/// Every `write_*` method feeds FNV-1a directly; variable-length fields
/// (`write_bytes`, `write_str`) prepend their length so
/// `("ab", "c")` and `("a", "bc")` never collide by framing. [`finish`]
/// applies the splitmix64 avalanche.
///
/// [`finish`]: Self::finish
#[derive(Clone, Debug)]
pub struct FingerprintBuilder {
    h: u64,
}

impl Default for FingerprintBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl FingerprintBuilder {
    /// Empty accumulator seeded with the FNV offset basis.
    pub const fn new() -> Self {
        Self { h: FNV_OFFSET }
    }

    #[inline]
    fn byte(&mut self, b: u8) {
        self.h ^= u64::from(b);
        self.h = self.h.wrapping_mul(FNV_PRIME);
    }

    /// Feed the little-endian form of `v`.
    pub fn write_u64(&mut self, v: u64) -> &mut Self {
        for b in v.to_le_bytes() {
            self.byte(b);
        }
        self
    }

    /// Feed the little-endian form of `v`.
    pub fn write_u32(&mut self, v: u32) -> &mut Self {
        for b in v.to_le_bytes() {
            self.byte(b);
        }
        self
    }

    /// Feed the little-endian form of `v`.
    pub fn write_u16(&mut self, v: u16) -> &mut Self {
        for b in v.to_le_bytes() {
            self.byte(b);
        }
        self
    }

    /// Feed one byte.
    pub fn write_u8(&mut self, v: u8) -> &mut Self {
        self.byte(v);
        self
    }

    /// Feed `v` widened to `u64` (as `i64`, preserving sign).
    pub fn write_i64(&mut self, v: i64) -> &mut Self {
        self.write_u64(v as u64)
    }

    /// Feed `v` widened to `u32` (as `i32`, preserving sign).
    pub fn write_i32(&mut self, v: i32) -> &mut Self {
        self.write_u32(v as u32)
    }

    /// Feed `v` as `u64`.
    pub fn write_usize(&mut self, v: usize) -> &mut Self {
        self.write_u64(v as u64)
    }

    /// Feed the exact `f64` bit pattern — `0.0`/`-0.0` and distinct NaN
    /// payloads stay distinct, matching `to_bits` semantics rather than `==`.
    pub fn write_f64(&mut self, v: f64) -> &mut Self {
        self.write_u64(v.to_bits())
    }

    /// Feed `0` or `1`.
    pub fn write_bool(&mut self, v: bool) -> &mut Self {
        self.write_u8(u8::from(v))
    }

    /// Feed `len` then the raw bytes — self-delimiting, so concatenated
    /// fields cannot alias.
    pub fn write_bytes(&mut self, bytes: &[u8]) -> &mut Self {
        self.write_u64(bytes.len() as u64);
        for &b in bytes {
            self.byte(b);
        }
        self
    }

    /// Feed a UTF-8 string through [`write_bytes`](Self::write_bytes).
    pub fn write_str(&mut self, s: &str) -> &mut Self {
        self.write_bytes(s.as_bytes())
    }

    /// Avalanche-finish and return the 64-bit fingerprint.
    pub fn finish(&self) -> u64 {
        splitmix64(self.h)
    }
}

// ---------------------------------------------------------------------------
// DirtyFingerprintGate
// ---------------------------------------------------------------------------

/// Counters reported by [`DirtyFingerprintGate::stats`].
///
/// * `skips` — `should_skip` calls that found the presented fingerprint
///   already committed (the cheap path).
/// * `updates` — `commit` calls that actually changed the table (new key or
///   new fingerprint).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FingerprintStats {
    /// Committed-fingerprint hits — work skipped.
    pub skips: u64,
    /// Fingerprint-table writes — state actually changed.
    pub updates: u64,
}

/// O(1) dirty-check table mapping a key to the 64-bit fingerprint of its
/// last committed state.
///
/// The contract is two-phase so work deferral is safe:
///
/// 1. [`should_skip`](Self::should_skip) — a pure check plus a skip counter.
///    `true` means the committed fingerprint already equals `fingerprint`:
///    re-processing would be redundant.
/// 2. [`commit`](Self::commit) — performed only after the work that
///    justified the fingerprint actually ran, so a deferred machine stays
///    dirty instead of being silently marked clean.
///
/// [`observe`](Self::observe) fuses the two for callers that always commit
/// immediately. [`invalidate`](Self::invalidate) drops the committed
/// fingerprint (the next check is dirty), and [`remove`](Self::remove)
/// forgets the key entirely (entity gone).
#[derive(Clone, Debug)]
pub struct DirtyFingerprintGate<K> {
    table: HashMap<K, u64>,
    skips: u64,
    updates: u64,
}

impl<K> Default for DirtyFingerprintGate<K> {
    fn default() -> Self {
        Self {
            table: HashMap::new(),
            skips: 0,
            updates: 0,
        }
    }
}

/// Gates compare on fingerprint state only — the skip/update counters are
/// instrumentation, not identity, so two caches with equal tables are equal
/// regardless of how many skips each has served.
impl<K: Eq + Hash> PartialEq for DirtyFingerprintGate<K> {
    fn eq(&self, other: &Self) -> bool {
        self.table == other.table
    }
}

impl<K: Eq + Hash> Eq for DirtyFingerprintGate<K> {}

impl<K: Eq + Hash> DirtyFingerprintGate<K> {
    /// Empty gate.
    pub fn new() -> Self {
        Self::default()
    }

    /// Empty gate with room for `capacity` entries before rehashing.
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            table: HashMap::with_capacity(capacity),
            skips: 0,
            updates: 0,
        }
    }

    /// Number of tracked keys.
    pub fn len(&self) -> usize {
        self.table.len()
    }

    /// `true` when no key has a committed fingerprint.
    pub fn is_empty(&self) -> bool {
        self.table.is_empty()
    }

    /// `true` — and counts a skip — when `fingerprint` equals the key's
    /// committed fingerprint, meaning re-processing is redundant. `false`
    /// otherwise; the caller is expected to run the work and then
    /// [`commit`](Self::commit).
    #[inline]
    pub fn should_skip(&mut self, key: &K, fingerprint: u64) -> bool {
        if self.table.get(key) == Some(&fingerprint) {
            self.skips += 1;
            return true;
        }
        false
    }

    /// Record `fingerprint` as `key`'s newest committed state. Counts an
    /// update only when the table actually changes (insert or different
    /// fingerprint), so a redundant commit is free in the stats.
    #[inline]
    pub fn commit(&mut self, key: K, fingerprint: u64) {
        match self.table.entry(key) {
            std::collections::hash_map::Entry::Occupied(mut e) => {
                if *e.get() != fingerprint {
                    *e.get_mut() = fingerprint;
                    self.updates += 1;
                }
            }
            std::collections::hash_map::Entry::Vacant(e) => {
                e.insert(fingerprint);
                self.updates += 1;
            }
        }
    }

    /// Fused check-and-commit for callers that commit immediately:
    /// `false` when `fingerprint` is already committed (a skip), `true`
    /// after committing it otherwise.
    #[inline]
    pub fn observe(&mut self, key: K, fingerprint: u64) -> bool {
        if self.should_skip(&key, fingerprint) {
            return false;
        }
        self.commit(key, fingerprint);
        true
    }

    /// Drop the committed fingerprint so the next [`should_skip`] for this
    /// key reports dirty — the "something changed underneath" signal.
    ///
    /// [`should_skip`]: Self::should_skip
    #[inline]
    pub fn invalidate(&mut self, key: &K) {
        self.table.remove(key);
    }

    /// Forget `key` entirely (the entity left the world). Identical
    /// mechanism to [`invalidate`](Self::invalidate); kept as a separate
    /// verb so call sites document intent.
    #[inline]
    pub fn remove(&mut self, key: &K) {
        self.table.remove(key);
    }

    /// Drop every committed fingerprint — every key is dirty afterwards.
    /// Keeps the allocated table for reuse.
    #[inline]
    pub fn clear(&mut self) {
        self.table.clear();
    }

    /// The fingerprint currently committed for `key`, if any.
    pub fn fingerprint_of(&self, key: &K) -> Option<u64> {
        self.table.get(key).copied()
    }

    /// Skip/update counters since construction or [`reset_stats`].
    ///
    /// [`reset_stats`]: Self::reset_stats
    pub fn stats(&self) -> FingerprintStats {
        FingerprintStats {
            skips: self.skips,
            updates: self.updates,
        }
    }

    /// Zero the skip/update counters without touching committed state.
    pub fn reset_stats(&mut self) {
        self.skips = 0;
        self.updates = 0;
    }
}

// ---------------------------------------------------------------------------
// BoundedScratchVec
// ---------------------------------------------------------------------------

/// Reusable scratch buffer with a deterministic retention policy and an
/// observable growth counter.
///
/// `BoundedScratchVec` is a `Vec<T>` stand-in for hot loops that need
/// temporary storage each tick: the buffer keeps its capacity across
/// [`clear`](Self::clear), so steady-state reuse allocates nothing. When a
/// burst did push capacity past a caller-chosen bound,
/// [`clear_bounded`](Self::clear_bounded) shrinks back exactly to that
/// bound — no allocator heuristics — so a spike never becomes permanent
/// idle RSS.
///
/// [`growth_events`](Self::growth_events) counts every operation that had
/// to grow the buffer's capacity (initial `with_capacity` included), which
/// turns "zero churn after warmup" into an assertion instead of a hope.
///
/// Equality compares contents only; the growth counter is instrumentation.
#[derive(Clone, Debug)]
pub struct BoundedScratchVec<T> {
    buf: Vec<T>,
    growth_events: u64,
}

impl<T> Default for BoundedScratchVec<T> {
    fn default() -> Self {
        Self {
            buf: Vec::new(),
            growth_events: 0,
        }
    }
}

impl<T: PartialEq> PartialEq for BoundedScratchVec<T> {
    fn eq(&self, other: &Self) -> bool {
        self.buf == other.buf
    }
}

impl<T: Eq> Eq for BoundedScratchVec<T> {}

impl<T> BoundedScratchVec<T> {
    /// Empty scratch with no allocation.
    pub const fn new() -> Self {
        Self {
            buf: Vec::new(),
            growth_events: 0,
        }
    }

    /// Empty scratch pre-sized to `capacity`; counts one growth event when
    /// `capacity > 0` since the buffer allocates.
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            buf: Vec::with_capacity(capacity),
            growth_events: u64::from(capacity > 0),
        }
    }

    /// Elements currently stored.
    pub fn len(&self) -> usize {
        self.buf.len()
    }

    /// `true` when empty.
    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    /// Current capacity in elements.
    pub fn capacity(&self) -> usize {
        self.buf.capacity()
    }

    /// Number of calls that grew the buffer's capacity since construction.
    /// An `extend`-style call counts once even if `Vec` amortized several
    /// reallocs into it — the metric is "calls that churned", which is what
    /// per-tick budgets care about.
    pub fn growth_events(&self) -> u64 {
        self.growth_events
    }

    #[inline]
    fn note_growth(&self, before: usize, grown: &Self) -> u64 {
        u64::from(grown.buf.capacity() > before)
    }

    /// Append `value`, growing if needed.
    pub fn push(&mut self, value: T) {
        let cap = self.buf.capacity();
        self.buf.push(value);
        self.growth_events += self.note_growth(cap, self);
    }

    /// Ensure room for `additional` more elements.
    pub fn reserve(&mut self, additional: usize) {
        let cap = self.buf.capacity();
        self.buf.reserve(additional);
        self.growth_events += self.note_growth(cap, self);
    }

    /// Append every item from `iter`.
    pub fn extend<I: IntoIterator<Item = T>>(&mut self, iter: I) {
        let cap = self.buf.capacity();
        self.buf.extend(iter);
        self.growth_events += self.note_growth(cap, self);
    }

    /// Truncate to `len` elements (no-op when already shorter).
    pub fn truncate(&mut self, len: usize) {
        self.buf.truncate(len);
    }

    /// Remove and return the last element.
    pub fn pop(&mut self) -> Option<T> {
        self.buf.pop()
    }

    /// Drop all elements but keep the capacity for reuse.
    pub fn clear(&mut self) {
        self.buf.clear();
    }

    /// Drop all elements, then shrink capacity down to at most
    /// `max_retain_elems` when a burst pushed it past that bound. When
    /// capacity is already within the bound this is exactly
    /// [`clear`](Self::clear) — free. The shrink reallocates to a fresh
    /// buffer of precisely `max_retain_elems`, which is a deterministic
    /// release, not an allocator hint.
    pub fn clear_bounded(&mut self, max_retain_elems: usize) {
        if self.buf.capacity() > max_retain_elems {
            self.buf = Vec::with_capacity(max_retain_elems);
        } else {
            self.buf.clear();
        }
    }

    /// Read-only view of the contents.
    pub fn as_slice(&self) -> &[T] {
        &self.buf
    }

    /// Mutable view of the contents (never grows capacity).
    pub fn as_mut_slice(&mut self) -> &mut [T] {
        &mut self.buf
    }

    /// Consume and return the inner `Vec`.
    pub fn into_inner(self) -> Vec<T> {
        self.buf
    }
}

impl<T: Clone> BoundedScratchVec<T> {
    /// Append a copy of every element in `slice`.
    pub fn extend_from_slice(&mut self, slice: &[T]) {
        let cap = self.buf.capacity();
        self.buf.extend_from_slice(slice);
        self.growth_events += self.note_growth(cap, self);
    }

    /// Resize to `new_len`, filling new slots with `value`.
    pub fn resize(&mut self, new_len: usize, value: T) {
        let cap = self.buf.capacity();
        self.buf.resize(new_len, value);
        self.growth_events += self.note_growth(cap, self);
    }
}

impl<T> Deref for BoundedScratchVec<T> {
    type Target = [T];

    fn deref(&self) -> &[T] {
        &self.buf
    }
}

impl<T> DerefMut for BoundedScratchVec<T> {
    fn deref_mut(&mut self) -> &mut [T] {
        &mut self.buf
    }
}

impl<T> From<Vec<T>> for BoundedScratchVec<T> {
    fn from(buf: Vec<T>) -> Self {
        Self {
            buf,
            growth_events: 0,
        }
    }
}

// ---------------------------------------------------------------------------
// TickBudgetQueue
// ---------------------------------------------------------------------------

/// What [`TickBudgetQueue::drain_budget`] actually did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DrainReport {
    /// Items moved to the caller's `out` buffer, in FIFO order.
    pub items: usize,
    /// Sum of `cost_fn` over the drained items.
    pub cost: u64,
}

/// Idempotent FIFO queue that slices arbitrary bursts into a fixed
/// per-tick budget.
///
/// [`enqueue`](Self::enqueue) dedupes: a key already waiting is not
/// queued twice, so producers can signal freely. [`drain_budget`] pops the
/// oldest keys until either `max_items` is reached or the next item's cost
/// would exceed `max_cost` — with one deliberate exception: a non-empty
/// queue always releases at least one item when `max_items > 0`, so a
/// single oversized item can never deadlock the queue. Work the tick
/// couldn't finish goes back through [`requeue_front`](Self::requeue_front)
/// without losing its place.
#[derive(Clone, Debug)]
pub struct TickBudgetQueue<K> {
    queue: VecDeque<K>,
    queued: HashSet<K>,
}

impl<K> Default for TickBudgetQueue<K> {
    fn default() -> Self {
        Self {
            queue: VecDeque::new(),
            queued: HashSet::new(),
        }
    }
}

impl<K: Eq + Hash> PartialEq for TickBudgetQueue<K> {
    fn eq(&self, other: &Self) -> bool {
        self.queue == other.queue
    }
}

impl<K: Eq + Hash> Eq for TickBudgetQueue<K> {}

impl<K: Eq + Hash + Clone> TickBudgetQueue<K> {
    /// Empty queue.
    pub fn new() -> Self {
        Self::default()
    }

    /// Append `key` to the back. `false` — a no-op — when `key` is already
    /// queued, which makes producers idempotent by construction.
    pub fn enqueue(&mut self, key: K) -> bool {
        if !self.queued.insert(key.clone()) {
            return false;
        }
        self.queue.push_back(key);
        true
    }

    /// Push `key` back onto the front (re-arming an item whose processing
    /// was interrupted). `false` when `key` is already queued.
    pub fn requeue_front(&mut self, key: K) -> bool {
        if !self.queued.insert(key.clone()) {
            return false;
        }
        self.queue.push_front(key);
        true
    }

    /// Move up to `max_items` keys (and up to `max_cost` cumulative
    /// `cost_fn` units) from the front into `out`, in FIFO order.
    ///
    /// Stops before the first item that would overflow either bound —
    /// except that a non-empty queue with `max_items > 0` always releases
    /// its head item even when `cost_fn(head) > max_cost`, so one big job
    /// cannot starve the queue forever. `out` is appended to, never
    /// cleared, so callers can reuse it across ticks.
    pub fn drain_budget(
        &mut self,
        max_items: usize,
        max_cost: u64,
        mut cost_fn: impl FnMut(&K) -> u64,
        out: &mut Vec<K>,
    ) -> DrainReport {
        let mut report = DrainReport::default();
        while report.items < max_items {
            let Some(next) = self.queue.front() else {
                break;
            };
            let cost = cost_fn(next);
            if report.items > 0 && report.cost.saturating_add(cost) > max_cost {
                break;
            }
            let key = self.queue.pop_front().expect("front checked");
            self.queued.remove(&key);
            out.push(key);
            report.items += 1;
            report.cost = report.cost.saturating_add(cost);
        }
        report
    }

    /// Move every queued key into `out` in FIFO order.
    pub fn flush_all(&mut self, out: &mut Vec<K>) {
        out.extend(self.queue.drain(..));
        self.queued.clear();
    }

    /// Queued keys waiting to drain.
    pub fn len(&self) -> usize {
        self.queue.len()
    }

    /// `true` when nothing is queued.
    pub fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }

    /// `true` while `key` sits in the queue.
    pub fn contains(&self, key: &K) -> bool {
        self.queued.contains(key)
    }

    /// Remove `key` from wherever it sits in the queue. `false` when
    /// absent. O(n) scan — fine for cancels, not a hot path.
    pub fn remove(&mut self, key: &K) -> bool {
        if !self.queued.remove(key) {
            return false;
        }
        if let Some(pos) = self.queue.iter().position(|k| k == key) {
            self.queue.remove(pos);
        }
        true
    }

    /// Drop every queued key, keeping both allocations for reuse.
    pub fn clear(&mut self) {
        self.queue.clear();
        self.queued.clear();
    }
}

// ---------------------------------------------------------------------------
// EventWakeWheel
// ---------------------------------------------------------------------------

/// Deterministic tick-indexed wake scheduler.
///
/// Keys are bucketed by absolute wake tick in a `BTreeMap` with a
/// `BTreeSet` per tick, so [`pop_due`](Self::pop_due) yields every machine
/// due in strictly increasing `(tick, key)` order — byte-for-byte
/// reproducible regardless of insertion history. `schedule`, `cancel`, and
/// reschedule are `O(log n)`; `pop_due` is `O(log n)` plus the due count.
/// Sleeping machines cost exactly nothing between their wake ticks.
#[derive(Clone, Debug)]
pub struct EventWakeWheel<K> {
    /// `wake_tick -> keys due then`, sorted inside each bucket.
    buckets: BTreeMap<u64, BTreeSet<K>>,
    /// `key -> wake_tick` for O(log n) cancel/reschedule.
    index: HashMap<K, u64>,
}

impl<K> Default for EventWakeWheel<K> {
    fn default() -> Self {
        Self {
            buckets: BTreeMap::new(),
            index: HashMap::new(),
        }
    }
}

impl<K: Ord + Eq + Hash + Clone> PartialEq for EventWakeWheel<K> {
    fn eq(&self, other: &Self) -> bool {
        self.index == other.index
    }
}

impl<K: Ord + Eq + Hash + Clone> Eq for EventWakeWheel<K> {}

impl<K: Ord + Eq + Hash + Clone> EventWakeWheel<K> {
    /// Empty wheel.
    pub fn new() -> Self {
        Self::default()
    }

    /// Arm `key` to wake at `wake_tick`. Rescheduling is atomic: a key
    /// already armed for a different tick moves cleanly, and re-arming for
    /// the same tick is a no-op.
    pub fn schedule(&mut self, key: K, wake_tick: u64) {
        if self.index.get(&key) == Some(&wake_tick) {
            return;
        }
        self.cancel(&key);
        self.buckets
            .entry(wake_tick)
            .or_default()
            .insert(key.clone());
        self.index.insert(key, wake_tick);
    }

    /// Disarm `key`. `false` when it wasn't scheduled.
    pub fn cancel(&mut self, key: &K) -> bool {
        let Some(tick) = self.index.remove(key) else {
            return false;
        };
        if let Some(set) = self.buckets.get_mut(&tick) {
            set.remove(key);
            if set.is_empty() {
                self.buckets.remove(&tick);
            }
        }
        true
    }

    /// `key`'s armed wake tick, if any.
    pub fn wake_of(&self, key: &K) -> Option<u64> {
        self.index.get(key).copied()
    }

    /// The earliest armed wake tick across all keys.
    pub fn next_wake(&self) -> Option<u64> {
        self.buckets.keys().next().copied()
    }

    /// Keys currently armed.
    pub fn len(&self) -> usize {
        self.index.len()
    }

    /// `true` when nothing is armed.
    pub fn is_empty(&self) -> bool {
        self.index.is_empty()
    }

    /// Pop every key whose wake tick is `<= now_tick`, appending them to
    /// `out` in `(wake_tick, key)` sorted order and returning the count.
    /// `out` is appended to, never cleared — pass a reusable
    /// [`BoundedScratchVec`] for allocation-free ticks.
    pub fn pop_due(&mut self, now_tick: u64, out: &mut BoundedScratchVec<K>) -> usize {
        // `now_tick == u64::MAX` cannot `+ 1` without wrapping, so drain
        // the whole map explicitly instead of `split_off`ing at `MAX`
        // (which would leave the `MAX` bucket behind).
        let due = if now_tick == u64::MAX {
            std::mem::take(&mut self.buckets)
        } else {
            let later = self.buckets.split_off(&(now_tick + 1));
            std::mem::replace(&mut self.buckets, later)
        };
        let mut popped = 0;
        for (_tick, keys) in due {
            for key in keys {
                self.index.remove(&key);
                out.push(key);
                popped += 1;
            }
        }
        popped
    }
}

// ---------------------------------------------------------------------------
// CoalescedDeltaBatch
// ---------------------------------------------------------------------------

/// Last-write-wins delta accumulator with deterministic sorted drain.
///
/// Every intermediate state update to a key is collapsed to the newest
/// value — a machine written a hundred times between flushes produces one
/// outbound delta, not a hundred. [`drain_sorted_into`] emits in
/// `Ord` key order regardless of insertion order, so the wire (or the
/// savefile) sees the same bytes every run.
///
/// [`drain_sorted_into`]: Self::drain_sorted_into
#[derive(Clone, Debug)]
pub struct CoalescedDeltaBatch<K, V> {
    map: BTreeMap<K, V>,
}

impl<K, V> Default for CoalescedDeltaBatch<K, V> {
    fn default() -> Self {
        Self {
            map: BTreeMap::new(),
        }
    }
}

impl<K: Ord, V: PartialEq> PartialEq for CoalescedDeltaBatch<K, V> {
    fn eq(&self, other: &Self) -> bool {
        self.map == other.map
    }
}

impl<K: Ord, V: Eq> Eq for CoalescedDeltaBatch<K, V> {}

impl<K: Ord, V> CoalescedDeltaBatch<K, V> {
    /// Empty batch.
    pub fn new() -> Self {
        Self::default()
    }

    /// Store `value` as `key`'s newest delta, replacing any older one.
    /// Returns the evicted value when there was one.
    pub fn record(&mut self, key: K, value: V) -> Option<V> {
        self.map.insert(key, value)
    }

    /// The newest recorded value for `key`.
    pub fn get(&self, key: &K) -> Option<&V> {
        self.map.get(key)
    }

    /// `true` while `key` has a pending delta.
    pub fn contains(&self, key: &K) -> bool {
        self.map.contains_key(key)
    }

    /// Drop `key`'s pending delta. `false` when absent.
    pub fn remove(&mut self, key: &K) -> bool {
        self.map.remove(key).is_some()
    }

    /// Keys with pending deltas.
    pub fn len(&self) -> usize {
        self.map.len()
    }

    /// `true` when no deltas are pending.
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// Iterate pending `(key, value)` pairs in sorted key order without
    /// draining.
    pub fn iter(&self) -> impl Iterator<Item = (&K, &V)> {
        self.map.iter()
    }

    /// Append all pending deltas to `out` in sorted key order, leaving the
    /// batch empty. `out` is appended to, never cleared.
    pub fn drain_sorted_into(&mut self, out: &mut Vec<(K, V)>) {
        out.extend(std::mem::take(&mut self.map));
    }

    /// Drop every pending delta, keeping the allocation for reuse.
    pub fn clear(&mut self) {
        self.map.clear();
    }
}

// ---------------------------------------------------------------------------
// AdaptiveLoadGovernor
// ---------------------------------------------------------------------------

/// Fixed sliding-window depth for [`AdaptiveLoadGovernor`].
pub const GOVERNOR_WINDOW: usize = 120;

/// Lower edge of the hysteresis dead-band (fraction of target).
const DEAD_BAND_LOW: f64 = 0.90;
/// Upper edge of the hysteresis dead-band (fraction of target).
const DEAD_BAND_HIGH: f64 = 1.10;
/// Fraction of the gap to the desired scale closed per evaluation —
/// exponential smoothing, so the budget moves fast but never jumps.
const SCALE_SMOOTHING: f64 = 0.25;

/// Sliding-window P50/P95/P99 latency monitor that turns observed load
/// into a smooth budget scale in `min_scale..=1.0`.
///
/// Samples arrive via [`record`](Self::record); non-finite and negative
/// inputs are scrubbed (and counted) rather than poisoning the window.
/// [`budget_scale`](Self::budget_scale) compares P95 against the target
/// with a `0.90..1.10` hysteresis dead-band: inside the band the scale is
/// held exactly (no oscillation), above it the desired scale becomes
/// `target / p95` (floored at `min_scale`), below it `1.0`; the applied
/// scale then closes a quarter of the gap each evaluation, so load sheds
/// and restores gradually instead of flapping.
#[derive(Clone, Debug)]
pub struct AdaptiveLoadGovernor {
    /// Ring buffer of accepted samples; `head` is the next write slot.
    samples: [f64; GOVERNOR_WINDOW],
    head: usize,
    len: usize,
    /// Latency the budget is tuned to hold (sanitized: finite and > 0).
    target: f64,
    /// Lowest budget scale the governor will emit (clamped to `[0, 1]`).
    min_scale: f64,
    /// Current smoothed scale, starts at `1.0`.
    scale: f64,
    /// Samples rejected by the input scrubber.
    scrubbed: u64,
}

impl AdaptiveLoadGovernor {
    /// A governor holding latency near `target_latency`, never scaling the
    /// budget below `min_scale` (clamped into `[0, 1]`; a non-finite or
    /// non-positive target falls back to `1.0` so the governor stays
    /// well-defined on garbage configuration).
    pub fn new(target_latency: f64, min_scale: f64) -> Self {
        let target = if target_latency.is_finite() && target_latency > 0.0 {
            target_latency
        } else {
            1.0
        };
        let min_scale = if min_scale.is_finite() {
            min_scale.clamp(0.0, 1.0)
        } else {
            0.0
        };
        Self {
            samples: [0.0; GOVERNOR_WINDOW],
            head: 0,
            len: 0,
            target,
            min_scale,
            scale: 1.0,
            scrubbed: 0,
        }
    }

    /// Feed one observed latency. Non-finite (`NaN`, `±inf`) and negative
    /// samples are rejected and counted, never stored.
    pub fn record(&mut self, sample: f64) {
        if !sample.is_finite() || sample < 0.0 {
            self.scrubbed += 1;
            return;
        }
        self.samples[self.head] = sample;
        self.head = (self.head + 1) % GOVERNOR_WINDOW;
        self.len = (self.len + 1).min(GOVERNOR_WINDOW);
    }

    /// Accepted samples currently in the window (≤ [`GOVERNOR_WINDOW`]).
    pub fn len(&self) -> usize {
        self.len
    }

    /// `true` before the first accepted sample.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Samples rejected by the scrubber since construction.
    pub fn scrubbed_count(&self) -> u64 {
        self.scrubbed
    }

    /// Sanitized latency target.
    pub fn target(&self) -> f64 {
        self.target
    }

    /// Lowest scale the governor can emit.
    pub fn min_scale(&self) -> f64 {
        self.min_scale
    }

    /// The scale most recently produced by [`budget_scale`](Self::budget_scale)
    /// (`1.0` until the first evaluation).
    pub fn current_scale(&self) -> f64 {
        self.scale
    }

    /// Sorted copy of the live window (allocation: `len` `f64`s). While
    /// `len < GOVERNOR_WINDOW` the accepted samples occupy exactly
    /// `samples[..len]`; once the ring fills, every slot is live — so the
    /// same slice covers both cases.
    fn sorted(&self) -> Vec<f64> {
        let mut s = self.samples[..self.len].to_vec();
        s.sort_unstable_by(f64::total_cmp);
        s
    }

    /// Nearest-rank percentile over the live window (`p` in `0..=100`,
    /// clamped). `None` while the window is empty.
    pub fn percentile(&self, p: f64) -> Option<f64> {
        if self.len == 0 {
            return None;
        }
        let p = if p.is_finite() { p.clamp(0.0, 100.0) } else { 0.0 };
        let rank = ((p / 100.0) * self.len as f64).ceil() as usize;
        let idx = rank.max(1) - 1;
        Some(self.sorted()[idx.min(self.len - 1)])
    }

    /// Median latency over the live window.
    pub fn p50(&self) -> Option<f64> {
        self.percentile(50.0)
    }

    /// 95th-percentile latency — the governor's load signal.
    pub fn p95(&self) -> Option<f64> {
        self.percentile(95.0)
    }

    /// 99th-percentile latency.
    pub fn p99(&self) -> Option<f64> {
        self.percentile(99.0)
    }

    /// Evaluate the window and update the smoothed budget scale.
    ///
    /// * `p95 > target * 1.10` (overloaded): desired scale is
    ///   `target / p95`, floored at `min_scale` — proportional shedding.
    /// * `p95 < target * 0.90` (headroom): desired scale is `1.0`.
    /// * Inside the dead-band: desired equals the current scale — a pure
    ///   hold, so borderline load cannot oscillate the budget.
    ///
    /// The applied scale then closes 25% of the remaining gap and is
    /// clamped into `min_scale..=1.0`. With an empty window the current
    /// scale (`1.0` initially) is returned unchanged.
    pub fn budget_scale(&mut self) -> f64 {
        let Some(p95) = self.p95() else {
            return self.scale;
        };
        let desired = if p95 > self.target * DEAD_BAND_HIGH {
            (self.target / p95).clamp(self.min_scale, 1.0)
        } else if p95 < self.target * DEAD_BAND_LOW {
            1.0
        } else {
            self.scale
        };
        self.scale = (self.scale + (desired - self.scale) * SCALE_SMOOTHING)
            .clamp(self.min_scale, 1.0);
        self.scale
    }

    /// Drop the window and counters; the smoothed scale resets to `1.0`.
    pub fn reset(&mut self) {
        self.samples = [0.0; GOVERNOR_WINDOW];
        self.head = 0;
        self.len = 0;
        self.scale = 1.0;
        self.scrubbed = 0;
    }
}
