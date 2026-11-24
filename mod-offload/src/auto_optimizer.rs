// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later

//! Generic automatic optimizers for mod-sourced work that wastes ticks.
//!
//! Nothing here knows a mod by name: a machine is a `u64` key, a recipe lookup
//! is a `(machine_kind, input_fingerprint)` pair, a packet source is a `u64`,
//! and a block write is plain coordinates plus state IDs. Any adapter that can
//! describe its workload in those terms gets the same four eliminations:
//!
//! - [`AutoRecipeMemoizer`]: O(N) per-tick linear recipe scans, including
//!   repeated scans that return "no match".
//! - [`AdaptivePollBackoff`]: polling machines that have not changed in many
//!   consecutive ticks, with 0-tick wakeup on neighbor or transfer events.
//! - [`AutoDeltaCoalescer`]: byte-identical sync packets and sub-interval
//!   churn from sources too far away to matter this tick.
//! - [`AutoBulkSectionBatcher`]: burst `set_block` floods (quarries, breath
//!   attacks, mega-explosions) into one update per section.

use std::collections::{btree_map::Entry, BTreeMap, HashMap, VecDeque};

/// FNV-1a 64-bit offset basis and prime: the deterministic non-cryptographic
/// mixing base used across the workspace for content fingerprints.
const FNV_OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

fn fnv1a64(mut hash: u64, bytes: &[u8]) -> u64 {
    for &byte in bytes {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

/// splitmix64-style finalizer: spreads low-entropy FNV output bits so adjacent
/// inputs never cluster onto one cache slot.
fn mix64(mut z: u64) -> u64 {
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

/// Deterministic fingerprint of a machine's `(item_id, count)` input slots.
///
/// Slot order is significant: `[(a, 1), (b, 2)]` differs from `[(b, 2), (a, 1)]`.
/// The value is a cache key, not a proof of equality - two different slot
/// layouts may collide, so adapters whose matchers are order-insensitive should
/// canonicalize (e.g. sort) their slots before fingerprinting.
#[must_use]
pub fn fingerprint_item_slots(slots: &[(u32, u32)]) -> u64 {
    let mut hash = fnv1a64(FNV_OFFSET_BASIS, &(slots.len() as u64).to_le_bytes());
    for &(item, count) in slots {
        hash = fnv1a64(hash, &item.to_le_bytes());
        hash = fnv1a64(hash, &count.to_le_bytes());
    }
    mix64(hash)
}

/// 128-bit payload fingerprint for duplicate suppression. The halves use
/// independent FNV bases plus a final mix so any changed byte flips both.
#[must_use]
pub fn fingerprint_128(bytes: &[u8]) -> [u64; 2] {
    let len = bytes.len() as u64;
    let low = mix64(fnv1a64(FNV_OFFSET_BASIS, bytes) ^ len);
    let high = mix64(fnv1a64(0x8422_2325_9e37_79b9, bytes).rotate_left(32) ^ !len);
    [low, high]
}

/// Packs a logical machine position into one `u64` backoff key.
///
/// The mix is many-to-one by design: a collision only shares a cadence between
/// two positions, which degrades the optimization rather than breaking it.
/// Callers that need an injective key (e.g. a block-entity index) supply their
/// own.
#[must_use]
pub fn pack_machine_key(dim: i32, x: i32, y: i32, z: i32) -> u64 {
    let low = (u64::from(x as u32) << 32) | u64::from(z as u32);
    let high = (u64::from(dim as u32) << 32) | u64::from(y as u32);
    mix64(mix64(low) ^ mix64(high))
}

/// Default capacity of [`AutoRecipeMemoizer`].
pub const DEFAULT_MEMO_CAPACITY: usize = 8_192;

/// Maximum machines tracked by [`AdaptivePollBackoff`].
pub const MAX_TRACKED_MACHINES: usize = 65_536;

/// Maximum sources tracked by [`AutoDeltaCoalescer`].
pub const MAX_COALESCE_SOURCES: usize = 65_536;

/// Poll cadence ladder in ticks for [`AdaptivePollBackoff`].
pub const BACKOFF_LADDER: [u64; 5] = [1, 2, 4, 8, 20];

/// Consecutive unchanged tick results needed to advance one ladder step.
pub const UNCHANGED_TICKS_PER_STEP: u8 = 4;

/// Cell count of one 16³ section in [`AutoBulkSectionBatcher`].
pub const SECTION_VOLUME: usize = 4096;

/// Counters for [`AutoRecipeMemoizer`]. All are cumulative since construction.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MemoStats {
    /// Lookups served from a cached `Some`.
    pub hits: u64,
    /// Lookups served from a cached `None` (the compute ran once, ever).
    pub negative_hits: u64,
    /// Lookups that ran the compute function.
    pub misses: u64,
    /// Entries evicted by the bounded FIFO table.
    pub evictions: u64,
}

/// Memoizes `(machine_kind, input_fingerprint) -> Option<R>` lookups so a
/// machine whose inputs satisfy no recipe stops rescanning the recipe list,
/// and a machine whose inputs do match keeps its recipe without rescanning.
///
/// Negative results are cached on equal footing with positive ones: an idle
/// machine holding non-matching items runs its matcher exactly once until a
/// fingerprint change or [`invalidate_all`](Self::invalidate_all).
///
/// The table is bounded; when full, the oldest inserted entry is evicted
/// (deterministic FIFO).
pub struct AutoRecipeMemoizer<R> {
    capacity: usize,
    entries: HashMap<(u32, u64), Option<R>>,
    /// Insertion order for FIFO eviction; each key appears exactly once.
    order: VecDeque<(u32, u64)>,
    stats: MemoStats,
}

impl<R: Clone> AutoRecipeMemoizer<R> {
    /// A memoizer with [`DEFAULT_MEMO_CAPACITY`] entries.
    pub fn new() -> Self {
        Self::with_capacity(DEFAULT_MEMO_CAPACITY)
    }

    /// A memoizer with at most `capacity` entries (clamped to at least one).
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            capacity: capacity.max(1),
            entries: HashMap::new(),
            order: VecDeque::new(),
            stats: MemoStats::default(),
        }
    }

    /// Returns the cached result for this key, running `compute` and storing
    /// its result (including `None`) on the first lookup only.
    pub fn lookup_or_compute(
        &mut self,
        machine_kind: u32,
        input_fingerprint: u64,
        compute: impl FnOnce() -> Option<R>,
    ) -> Option<R> {
        let key = (machine_kind, input_fingerprint);
        match self.entries.get(&key) {
            Some(Some(value)) => {
                self.stats.hits += 1;
                Some(value.clone())
            }
            Some(None) => {
                self.stats.negative_hits += 1;
                None
            }
            None => {
                self.stats.misses += 1;
                let value = compute();
                self.insert(key, value.clone());
                value
            }
        }
    }

    fn insert(&mut self, key: (u32, u64), value: Option<R>) {
        if self.entries.len() >= self.capacity {
            while let Some(oldest) = self.order.pop_front() {
                if self.entries.remove(&oldest).is_some() {
                    self.stats.evictions += 1;
                    break;
                }
            }
        }
        self.entries.insert(key, value);
        self.order.push_back(key);
    }

    /// Drops every cached entry. Call on recipe reload or registry changes.
    pub fn invalidate_all(&mut self) {
        self.entries.clear();
        self.order.clear();
    }

    pub fn stats(&self) -> MemoStats {
        self.stats
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

impl<R: Clone> Default for AutoRecipeMemoizer<R> {
    fn default() -> Self {
        Self::new()
    }
}

/// Counters for [`AdaptivePollBackoff`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BackoffStats {
    /// Machines with a tracked cadence entry.
    pub total_tracked: u64,
    /// Tracked machines backed off past the 1-tick cadence.
    pub sleeping_count: u64,
    /// [`should_tick`](AdaptivePollBackoff::should_tick) calls that returned
    /// `false` - ticks the caller skipped entirely.
    pub ticks_saved: u64,
}

struct BackoffEntry {
    /// Tick of the last granted poll; reschedules anchor here.
    last_ticked: u64,
    /// Earliest tick the next poll is granted.
    next_due: u64,
    /// Index into [`BACKOFF_LADDER`].
    level: u8,
    /// Consecutive unchanged results toward the next step.
    unchanged: u8,
}

/// Puts zero-change machines to sleep on a `1 -> 2 -> 4 -> 8 -> 20` tick
/// cadence ladder, and wakes them in the same tick a neighbor update or
/// item/fluid/energy transfer lands.
///
/// Usage per machine per tick: gate the expensive work with
/// [`should_tick`](Self::should_tick); when it grants a poll, report the
/// outcome through [`record_tick_result`](Self::record_tick_result). External
/// events call [`wake`](Self::wake)/[`wake_neighbors`](Self::wake_neighbors),
/// which restore 0-tick latency immediately.
///
/// The table holds at most [`MAX_TRACKED_MACHINES`] entries; an unknown
/// machine beyond the cap always polls, so a full table can never starve a
/// machine.
#[derive(Default)]
pub struct AdaptivePollBackoff {
    entries: HashMap<u64, BackoffEntry>,
    ticks_saved: u64,
}

impl AdaptivePollBackoff {
    pub fn new() -> Self {
        Self::default()
    }

    /// Grants a poll at `current_tick` when the machine's cadence is due.
    /// A granted poll reschedules the next due tick at the current ladder step.
    pub fn should_tick(&mut self, id: u64, current_tick: u64) -> bool {
        let Some(entry) = self.entry_or_insert(id) else {
            return true;
        };
        if current_tick >= entry.next_due {
            entry.last_ticked = current_tick;
            entry.next_due = current_tick.saturating_add(BACKOFF_LADDER[entry.level as usize]);
            true
        } else {
            self.ticks_saved += 1;
            false
        }
    }

    /// Reports the outcome of a granted poll. `false` results accumulate
    /// toward the next ladder step; a `true` result returns the machine to
    /// the 1-tick cadence immediately.
    pub fn record_tick_result(&mut self, id: u64, changed: bool) {
        let Some(entry) = self.entry_or_insert(id) else {
            return;
        };
        if changed {
            entry.level = 0;
            entry.unchanged = 0;
            entry.next_due = entry.last_ticked.saturating_add(BACKOFF_LADDER[0]);
        } else {
            entry.unchanged = entry.unchanged.saturating_add(1);
            if entry.unchanged >= UNCHANGED_TICKS_PER_STEP
                && usize::from(entry.level) + 1 < BACKOFF_LADDER.len()
            {
                entry.unchanged = 0;
                entry.level += 1;
                entry.next_due = entry
                    .last_ticked
                    .saturating_add(BACKOFF_LADDER[entry.level as usize]);
            }
        }
    }

    /// Restores a machine to the 1-tick cadence and clears its due time, so
    /// the next [`should_tick`](Self::should_tick) grants a poll on the same
    /// tick the wake arrives - 0-tick latency.
    pub fn wake(&mut self, id: u64) {
        let Some(entry) = self.entry_or_insert(id) else {
            return;
        };
        entry.level = 0;
        entry.unchanged = 0;
        entry.next_due = 0;
    }

    /// Wakes every listed machine; for neighbor-update fan-out.
    pub fn wake_neighbors(&mut self, ids: &[u64]) {
        for &id in ids {
            self.wake(id);
        }
    }

    /// Drops a machine's tracking state (e.g. when it is removed).
    pub fn remove(&mut self, id: u64) {
        self.entries.remove(&id);
    }

    fn entry_or_insert(&mut self, id: u64) -> Option<&mut BackoffEntry> {
        if !self.entries.contains_key(&id) && self.entries.len() >= MAX_TRACKED_MACHINES {
            return None;
        }
        Some(self.entries.entry(id).or_insert(BackoffEntry {
            last_ticked: 0,
            next_due: 0,
            level: 0,
            unchanged: 0,
        }))
    }

    pub fn stats(&self) -> BackoffStats {
        BackoffStats {
            total_tracked: self.entries.len() as u64,
            sleeping_count: self.entries.values().filter(|e| e.level > 0).count() as u64,
            ticks_saved: self.ticks_saved,
        }
    }
}

/// How far a sync source is from its consumer; sets the minimum resend
/// interval for changed payloads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DistanceTier {
    /// In the same area: 1-tick minimum interval.
    Near,
    /// Moderate distance: 4-tick minimum interval.
    Mid,
    /// Far away: 10-tick minimum interval.
    Far,
    /// Outside interest range: updates are dropped outright.
    OutOfRange,
}

impl DistanceTier {
    fn interval(self) -> Option<u64> {
        match self {
            Self::Near => Some(1),
            Self::Mid => Some(4),
            Self::Far => Some(10),
            Self::OutOfRange => None,
        }
    }
}

/// The verdict of [`AutoDeltaCoalescer::should_emit`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EmitDecision {
    /// Ship this packet now.
    Send,
    /// Byte-identical to the last packet this source sent; drop it.
    DropDuplicate,
    /// Changed too soon for the source's tier, or out of range entirely;
    /// suppress this emission (retry on a later tick).
    DeferThrottle,
}

/// Counters for [`AutoDeltaCoalescer`], cumulative since construction.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CoalesceStats {
    /// Emissions granted.
    pub sent: u64,
    /// Emissions dropped as byte-identical duplicates.
    pub dropped_duplicate: u64,
    /// Emissions suppressed by tier throttling or out-of-range policy.
    pub throttled_distance: u64,
}

struct SourceState {
    fingerprint: [u64; 2],
    last_sent_tick: u64,
}

/// Drops duplicate sync payloads and throttles distant churn.
///
/// A source's first payload always sends. Afterwards, a byte-identical
/// payload drops at any tier (even [`Near`](DistanceTier::Near)), and a
/// changed payload sends only once the tier's minimum interval has elapsed
/// since the last send. [`OutOfRange`](DistanceTier::OutOfRange) always
/// suppresses.
///
/// An unknown source beyond [`MAX_COALESCE_SOURCES`] is sent through rather
/// than tracked, so a full table degrades to passthrough instead of loss.
#[derive(Default)]
pub struct AutoDeltaCoalescer {
    sources: HashMap<u64, SourceState>,
    stats: CoalesceStats,
}

impl AutoDeltaCoalescer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Decides whether `payload` should be emitted for `source_id` now.
    pub fn should_emit(
        &mut self,
        source_id: u64,
        payload: &[u8],
        current_tick: u64,
        tier: DistanceTier,
    ) -> EmitDecision {
        let Some(interval) = tier.interval() else {
            self.stats.throttled_distance += 1;
            return EmitDecision::DeferThrottle;
        };
        let fingerprint = fingerprint_128(payload);
        match self.sources.get(&source_id) {
            Some(state) if state.fingerprint == fingerprint => {
                self.stats.dropped_duplicate += 1;
                EmitDecision::DropDuplicate
            }
            Some(state) if current_tick.saturating_sub(state.last_sent_tick) < interval => {
                self.stats.throttled_distance += 1;
                EmitDecision::DeferThrottle
            }
            _ => {
                self.stats.sent += 1;
                if self.sources.len() < MAX_COALESCE_SOURCES
                    || self.sources.contains_key(&source_id)
                {
                    self.sources.insert(
                        source_id,
                        SourceState {
                            fingerprint,
                            last_sent_tick: current_tick,
                        },
                    );
                }
                EmitDecision::Send
            }
        }
    }

    /// Drops a source's tracked state (e.g. on chunk unload).
    pub fn remove(&mut self, source_id: u64) {
        self.sources.remove(&source_id);
    }

    pub fn stats(&self) -> CoalesceStats {
        self.stats
    }
}

/// Section coordinate of a batched block write: `(dim, chunk_x, section_y,
/// chunk_z)` with each axis divided by 16 under Euclidean (floor) semantics.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct SectionKey {
    pub dim: i32,
    pub cx: i32,
    pub sy: i32,
    pub cz: i32,
}

/// One surviving net change inside a [`BulkSectionBatch`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BulkCellWrite {
    /// Local cell index `0..4096`, packed `(ly << 8) | (lz << 4) | lx`.
    pub index: u16,
    /// The state the cell held before this batch's first write.
    pub old_state: u32,
    /// The state after this batch's last write.
    pub new_state: u32,
}

/// All net changes for one section, sorted by cell index.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BulkSectionBatch {
    pub key: SectionKey,
    pub cells: Vec<BulkCellWrite>,
    /// `true` when any recorded write was flagged light-relevant (the default
    /// for [`record_set_block`](AutoBulkSectionBatcher::record_set_block),
    /// since a generic engine cannot prove a state change preserves light).
    pub dirty_light: bool,
}

/// Counters for [`AutoBulkSectionBatcher`], cumulative since construction.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BulkBatchStats {
    /// Every `record_set_block` call, including no-ops.
    pub raw_writes: u64,
    /// Net cell changes emitted by `drain_section_batches`.
    pub coalesced_writes: u64,
    /// No-op calls plus writes that cancelled a cell back to its initial state.
    pub cancelled_writes: u64,
    /// Sections that received at least one non-no-op write.
    pub sections_touched: u64,
}

#[derive(Default)]
struct SectionAccum {
    /// Local index -> `(initial_old_state, latest_new_state)`.
    cells: BTreeMap<u16, (u32, u32)>,
    light: bool,
}

/// Coalesces burst `set_block` floods into one update per 16³ section.
///
/// Writes are keyed to their section and local cell index. Last-write-wins:
/// re-writing a cell only advances its `new_state`, and when the latest state
/// returns to the cell's initial state the net change cancels entirely - an
/// `A -> B -> A` sequence produces no output. [`drain_section_batches`]
/// (Self::drain_section_batches) returns the survivors sorted by
/// `(dim, cx, sy, cz)`, skipping sections whose writes all cancelled.
#[derive(Default)]
pub struct AutoBulkSectionBatcher {
    sections: BTreeMap<SectionKey, SectionAccum>,
    stats: BulkBatchStats,
}

impl AutoBulkSectionBatcher {
    pub fn new() -> Self {
        Self::default()
    }

    /// Records a block change flagged light-relevant (the safe default for a
    /// mod-agnostic caller). No-ops (`old_state == new_state`) are skipped.
    pub fn record_set_block(
        &mut self,
        dim: i32,
        x: i32,
        y: i32,
        z: i32,
        old_state: u32,
        new_state: u32,
    ) {
        self.record_inner(dim, [x, y, z], old_state, new_state, true);
    }

    /// Records a block change that provably cannot alter lighting, so its
    /// batch can report `dirty_light == false` when nothing else set it.
    pub fn record_set_block_unlit(
        &mut self,
        dim: i32,
        x: i32,
        y: i32,
        z: i32,
        old_state: u32,
        new_state: u32,
    ) {
        self.record_inner(dim, [x, y, z], old_state, new_state, false);
    }

    fn record_inner(
        &mut self,
        dim: i32,
        [x, y, z]: [i32; 3],
        old_state: u32,
        new_state: u32,
        affects_light: bool,
    ) {
        self.stats.raw_writes += 1;
        if old_state == new_state {
            self.stats.cancelled_writes += 1;
            return;
        }
        let key = SectionKey {
            dim,
            cx: x.div_euclid(16),
            sy: y.div_euclid(16),
            cz: z.div_euclid(16),
        };
        let index = ((y.rem_euclid(16) as u16) << 8)
            | ((z.rem_euclid(16) as u16) << 4)
            | (x.rem_euclid(16) as u16);
        if !self.sections.contains_key(&key) {
            self.stats.sections_touched += 1;
        }
        let section = self.sections.entry(key).or_default();
        section.light |= affects_light;
        match section.cells.entry(index) {
            Entry::Vacant(slot) => {
                slot.insert((old_state, new_state));
            }
            Entry::Occupied(mut slot) => {
                let (initial_old, current_new) = slot.get_mut();
                *current_new = new_state;
                if *current_new == *initial_old {
                    slot.remove();
                    self.stats.cancelled_writes += 1;
                }
            }
        }
    }

    /// Drains all accumulated writes into per-section batches sorted by
    /// `(dim, cx, sy, cz)`, with cells sorted by local index. Sections whose
    /// writes cancelled out entirely produce no batch.
    pub fn drain_section_batches(&mut self) -> Vec<BulkSectionBatch> {
        let sections = std::mem::take(&mut self.sections);
        let mut batches = Vec::with_capacity(sections.len());
        for (key, accum) in sections {
            if accum.cells.is_empty() {
                continue;
            }
            self.stats.coalesced_writes += accum.cells.len() as u64;
            batches.push(BulkSectionBatch {
                key,
                dirty_light: accum.light,
                cells: accum
                    .cells
                    .into_iter()
                    .map(|(index, (old_state, new_state))| BulkCellWrite {
                        index,
                        old_state,
                        new_state,
                    })
                    .collect(),
            });
        }
        batches
    }

    /// `true` when no un-drained writes remain.
    pub fn is_empty(&self) -> bool {
        self.sections.is_empty()
    }

    pub fn stats(&self) -> BulkBatchStats {
        self.stats
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    // ---------- AutoRecipeMemoizer ----------

    #[test]
    fn memoizer_caches_positive_matches() {
        let calls = Cell::new(0);
        let mut memo = AutoRecipeMemoizer::<u32>::new();
        let fp = fingerprint_item_slots(&[(5, 3), (7, 1)]);
        let first = memo.lookup_or_compute(2, fp, || {
            calls.set(calls.get() + 1);
            Some(42)
        });
        assert_eq!(first, Some(42));
        for _ in 0..99 {
            let got = memo.lookup_or_compute(2, fp, || {
                calls.set(calls.get() + 1);
                Some(43)
            });
            assert_eq!(got, Some(42));
        }
        assert_eq!(calls.get(), 1);
        assert_eq!(
            memo.stats(),
            MemoStats {
                hits: 99,
                negative_hits: 0,
                misses: 1,
                evictions: 0,
            }
        );
    }

    #[test]
    fn memoizer_negative_cache_runs_compute_once_across_100_idle_ticks() {
        let calls = Cell::new(0);
        let mut memo = AutoRecipeMemoizer::<u32>::new();
        let fp = fingerprint_item_slots(&[(99, 1)]);
        for _ in 0..100 {
            let got = memo.lookup_or_compute(7, fp, || {
                calls.set(calls.get() + 1);
                None
            });
            assert_eq!(got, None);
        }
        assert_eq!(calls.get(), 1);
        assert_eq!(memo.stats().misses, 1);
        assert_eq!(memo.stats().negative_hits, 99);
        // A different machine kind with the same inputs is a different key.
        memo.lookup_or_compute(8, fp, || {
            calls.set(calls.get() + 1);
            None
        });
        assert_eq!(calls.get(), 2);
    }

    #[test]
    fn memoizer_invalidate_all_forces_recompute_on_reload() {
        let mut memo = AutoRecipeMemoizer::<u32>::new();
        let fp = fingerprint_item_slots(&[(1, 1)]);
        assert_eq!(memo.lookup_or_compute(0, fp, || Some(9)), Some(9));
        memo.invalidate_all();
        assert!(memo.is_empty());
        assert_eq!(memo.lookup_or_compute(0, fp, || Some(10)), Some(10));
        assert_eq!(memo.stats().misses, 2);
    }

    #[test]
    fn memoizer_bounded_capacity_evicts_oldest_first() {
        let mut memo = AutoRecipeMemoizer::<u32>::with_capacity(3);
        for fp in [10_u64, 11, 12] {
            memo.lookup_or_compute(0, fp, || Some(fp as u32));
        }
        // Fourth distinct key evicts fingerprint 10.
        memo.lookup_or_compute(0, 13, || Some(13));
        assert_eq!(memo.len(), 3);
        assert_eq!(memo.stats().evictions, 1);
        // 11 still hits; 10 misses again and evicts 11 next.
        let calls = Cell::new(0);
        memo.lookup_or_compute(0, 11, || {
            calls.set(calls.get() + 1);
            None
        });
        assert_eq!(calls.get(), 0);
        memo.lookup_or_compute(0, 10, || {
            calls.set(calls.get() + 1);
            Some(1)
        });
        assert_eq!(calls.get(), 1);
        assert_eq!(memo.stats().evictions, 2);
    }

    #[test]
    fn fingerprint_item_slots_is_deterministic_and_order_sensitive() {
        let a = fingerprint_item_slots(&[(1, 64), (2, 1)]);
        assert_eq!(a, fingerprint_item_slots(&[(1, 64), (2, 1)]));
        assert_ne!(a, fingerprint_item_slots(&[(2, 1), (1, 64)]));
        assert_ne!(a, fingerprint_item_slots(&[(1, 64)]));
        assert_ne!(a, fingerprint_item_slots(&[(1, 63), (2, 1)]));
    }

    // ---------- AdaptivePollBackoff ----------

    #[test]
    fn backoff_climbs_the_full_ladder_and_counts_saved_ticks() {
        let mut backoff = AdaptivePollBackoff::new();
        let mut ticked = Vec::new();
        for tick in 0..80_u64 {
            if backoff.should_tick(1, tick) {
                ticked.push(tick);
                backoff.record_tick_result(1, false);
            }
        }
        // 4 ticks at 1t, then 4 at 2t, 4t, 8t, then 20t (max rung).
        assert_eq!(
            ticked,
            [0, 1, 2, 3, 5, 7, 9, 11, 15, 19, 23, 27, 35, 43, 51, 59, 79]
        );
        let stats = backoff.stats();
        assert_eq!(stats.total_tracked, 1);
        assert_eq!(stats.sleeping_count, 1);
        assert_eq!(stats.ticks_saved, 80 - 17);
    }

    #[test]
    fn backoff_changed_result_returns_to_fastest_cadence() {
        let mut backoff = AdaptivePollBackoff::new();
        for tick in 0..12_u64 {
            if backoff.should_tick(1, tick) {
                backoff.record_tick_result(1, false);
            }
        }
        assert!(backoff.stats().sleeping_count > 0);
        // Machine finally does work: back to 1-tick cadence immediately.
        assert!(backoff.should_tick(1, 15));
        backoff.record_tick_result(1, true);
        assert!(backoff.should_tick(1, 16));
        assert_eq!(backoff.stats().sleeping_count, 0);
    }

    #[test]
    fn wake_gives_zero_tick_latency() {
        let mut backoff = AdaptivePollBackoff::new();
        for tick in 0..60_u64 {
            if backoff.should_tick(9, tick) {
                backoff.record_tick_result(9, false);
            }
        }
        // Deep asleep (20t cadence); a neighbor update must not wait it out.
        backoff.wake(9);
        assert!(backoff.should_tick(9, 60));
        backoff.record_tick_result(9, true);
        assert!(backoff.should_tick(9, 61));
    }

    #[test]
    fn wake_neighbors_wakes_a_whole_fanout() {
        let mut backoff = AdaptivePollBackoff::new();
        for tick in 0..60_u64 {
            for id in [3_u64, 4, 5] {
                if backoff.should_tick(id, tick) {
                    backoff.record_tick_result(id, false);
                }
            }
        }
        assert_eq!(backoff.stats().sleeping_count, 3);
        backoff.wake_neighbors(&[3, 4, 5]);
        for id in [3_u64, 4, 5] {
            assert!(backoff.should_tick(id, 60));
        }
    }

    #[test]
    fn backoff_table_never_starves_beyond_the_cap() {
        let mut backoff = AdaptivePollBackoff::new();
        for id in 0..MAX_TRACKED_MACHINES as u64 {
            backoff.should_tick(id, 0);
        }
        assert_eq!(backoff.stats().total_tracked, MAX_TRACKED_MACHINES as u64);
        // One more unknown machine still polls; it just is not tracked.
        assert!(backoff.should_tick(u64::MAX, 0));
        assert_eq!(backoff.stats().total_tracked, MAX_TRACKED_MACHINES as u64);
    }

    // ---------- AutoDeltaCoalescer ----------

    #[test]
    fn coalescer_sends_first_sight_and_drops_byte_identical() {
        let mut coalescer = AutoDeltaCoalescer::new();
        let payload = b"entity-state-v1";
        assert_eq!(
            coalescer.should_emit(1, payload, 0, DistanceTier::Near),
            EmitDecision::Send
        );
        // Duplicates drop immediately, even at Near and in the same tick.
        assert_eq!(
            coalescer.should_emit(1, payload, 0, DistanceTier::Near),
            EmitDecision::DropDuplicate
        );
        assert_eq!(
            coalescer.should_emit(1, payload, 500, DistanceTier::Near),
            EmitDecision::DropDuplicate
        );
        assert_eq!(coalescer.stats().sent, 1);
        assert_eq!(coalescer.stats().dropped_duplicate, 2);
    }

    #[test]
    fn coalescer_throttles_changed_payloads_by_distance_tier() {
        let mut coalescer = AutoDeltaCoalescer::new();
        assert_eq!(
            coalescer.should_emit(1, b"a", 0, DistanceTier::Mid),
            EmitDecision::Send
        );
        // Mid tier needs a 4-tick gap between sends.
        for tick in [1_u64, 2, 3] {
            assert_eq!(
                coalescer.should_emit(1, b"b", tick, DistanceTier::Mid),
                EmitDecision::DeferThrottle
            );
        }
        assert_eq!(
            coalescer.should_emit(1, b"b", 4, DistanceTier::Mid),
            EmitDecision::Send
        );
        // Far tier needs 10 from the last send (tick 4 -> 14).
        assert_eq!(
            coalescer.should_emit(1, b"c", 13, DistanceTier::Far),
            EmitDecision::DeferThrottle
        );
        assert_eq!(
            coalescer.should_emit(1, b"c", 14, DistanceTier::Far),
            EmitDecision::Send
        );
        // Near tier still drops duplicates but never delays real changes past 1t.
        assert_eq!(
            coalescer.should_emit(1, b"d", 15, DistanceTier::Near),
            EmitDecision::Send
        );
        let stats = coalescer.stats();
        assert_eq!(stats.sent, 4);
        assert_eq!(stats.throttled_distance, 4);
    }

    #[test]
    fn coalescer_out_of_range_drops_without_tracking() {
        let mut coalescer = AutoDeltaCoalescer::new();
        for tick in 0..10_u64 {
            assert_eq!(
                coalescer.should_emit(2, b"far-away", tick, DistanceTier::OutOfRange),
                EmitDecision::DeferThrottle
            );
        }
        assert_eq!(coalescer.stats().throttled_distance, 10);
        assert_eq!(coalescer.stats().sent, 0);
        // Back in range, the source is still a first sight and sends at once.
        assert_eq!(
            coalescer.should_emit(2, b"far-away", 10, DistanceTier::Near),
            EmitDecision::Send
        );
    }

    #[test]
    fn fingerprint_128_separates_payloads_in_both_halves() {
        let a = fingerprint_128(b"machine-state");
        assert_eq!(a, fingerprint_128(b"machine-state"));
        let b = fingerprint_128(b"machine-statf");
        assert_ne!(a, b);
        assert_ne!(a[0], b[0]);
        assert_ne!(a[1], b[1]);
    }

    // ---------- AutoBulkSectionBatcher ----------

    #[test]
    fn batcher_coalesces_overwrites_to_one_net_change() {
        let mut batcher = AutoBulkSectionBatcher::new();
        // Five writes to one cell: only the (first_old -> last_new) survives.
        for (old, new) in [(10, 11), (11, 12), (12, 13), (13, 14), (14, 15)] {
            batcher.record_set_block(0, 5, 70, 5, old, new);
        }
        let batches = batcher.drain_section_batches();
        assert_eq!(batches.len(), 1);
        assert_eq!(
            batches[0].key,
            SectionKey {
                dim: 0,
                cx: 0,
                sy: 4,
                cz: 0
            }
        );
        let index = (6_u16 << 8) | (5 << 4) | 5; // ly=70%16=6, lz=5, lx=5
        assert_eq!(
            batches[0].cells,
            vec![BulkCellWrite {
                index,
                old_state: 10,
                new_state: 15
            }]
        );
        assert!(batches[0].dirty_light);
        assert_eq!(batcher.stats().raw_writes, 5);
        assert_eq!(batcher.stats().coalesced_writes, 1);
        assert!(batcher.is_empty());
        assert!(batcher.drain_section_batches().is_empty());
    }

    #[test]
    fn batcher_cancels_net_zero_and_skips_noops() {
        let mut batcher = AutoBulkSectionBatcher::new();
        batcher.record_set_block(0, 0, 0, 0, 7, 8); // A -> B
        batcher.record_set_block(0, 0, 0, 0, 8, 7); // B -> A: cancels out
        batcher.record_set_block(0, 1, 0, 0, 4, 4); // no-op
        batcher.record_set_block(0, 2, 0, 0, 1, 2); // one real change survives
        let batches = batcher.drain_section_batches();
        assert_eq!(batches.len(), 1);
        assert_eq!(batches[0].cells.len(), 1);
        assert_eq!(
            batches[0].cells[0],
            BulkCellWrite {
                index: 2,
                old_state: 1,
                new_state: 2
            }
        );
        assert_eq!(batcher.stats().raw_writes, 4);
        assert_eq!(batcher.stats().cancelled_writes, 2);
        assert_eq!(batcher.stats().coalesced_writes, 1);
    }

    #[test]
    fn batcher_groups_negative_coords_by_euclidean_section() {
        let mut batcher = AutoBulkSectionBatcher::new();
        batcher.record_set_block(-1, -1, -1, -1, 1, 2);
        batcher.record_set_block(-1, -16, -16, -16, 3, 4);
        batcher.record_set_block(-1, -17, 0, 0, 5, 6); // cx -2, sy 0, cz 0
        let batches = batcher.drain_section_batches();
        let keys: Vec<_> = batches.iter().map(|b| b.key).collect();
        assert_eq!(
            keys,
            [
                SectionKey {
                    dim: -1,
                    cx: -2,
                    sy: 0,
                    cz: 0
                },
                SectionKey {
                    dim: -1,
                    cx: -1,
                    sy: -1,
                    cz: -1
                },
            ]
        );
        // (-16,-16,-16) is the corner cell: local 0. (-1,-1,-1) is local 15³.
        assert_eq!(
            batches[1].cells[0],
            BulkCellWrite {
                index: 0,
                old_state: 3,
                new_state: 4
            }
        );
        assert_eq!(
            batches[1].cells[1],
            BulkCellWrite {
                index: (15 << 8) | (15 << 4) | 15,
                old_state: 1,
                new_state: 2
            }
        );
    }

    #[test]
    fn batcher_handles_a_thousand_write_burst_deterministically() {
        let mut batcher = AutoBulkSectionBatcher::new();
        for i in 0..1000_i32 {
            let x = (i % 41) - 20; // -20..=20 spans cx -2..=1
            let y = 64 + (i % 16);
            let z = (i % 37) - 18; // -18..=18 spans cz -2..=1
            batcher.record_set_block(0, x, y, z, 100, 200);
        }
        assert_eq!(batcher.stats().raw_writes, 1000);
        let batches = batcher.drain_section_batches();
        // Deterministically sorted by (dim, cx, sy, cz).
        let keys: Vec<_> = batches.iter().map(|b| b.key).collect();
        let mut sorted = keys.clone();
        sorted.sort();
        assert_eq!(keys, sorted);
        // Every cell unique, indices in range and ascending.
        let total: usize = batches
            .iter()
            .map(|b| {
                let indices: Vec<u16> = b.cells.iter().map(|c| c.index).collect();
                let mut dedup = indices.clone();
                dedup.dedup();
                assert_eq!(indices, dedup); // sorted + unique
                b.cells.len()
            })
            .sum();
        assert_eq!(total, batcher.stats().coalesced_writes as usize);
        // A second identical burst produces byte-identical output.
        for i in 0..1000_i32 {
            let x = (i % 41) - 20;
            let y = 64 + (i % 16);
            let z = (i % 37) - 18;
            batcher.record_set_block(0, x, y, z, 100, 200);
        }
        assert_eq!(batcher.drain_section_batches(), batches);
    }

    #[test]
    fn batcher_dirty_light_follows_the_light_hint() {
        let mut batcher = AutoBulkSectionBatcher::new();
        batcher.record_set_block_unlit(0, 0, 0, 0, 1, 2);
        let batches = batcher.drain_section_batches();
        assert!(!batches[0].dirty_light);
        batcher.record_set_block_unlit(0, 0, 0, 0, 2, 3);
        batcher.record_set_block(0, 1, 0, 0, 9, 10); // light-relevant write
        let batches = batcher.drain_section_batches();
        assert!(batches[0].dirty_light);
    }
}
