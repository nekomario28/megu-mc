// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later

//! Generic modded block-entity tick runtime bridging
//! [`mc_spine_core::perf_kit`] primitives into one sleep/wake/coalesce loop.
//!
//! Any modded machine family — cooking pots, tech processors, magic
//! infusers, dimension miners, collector buses — fits the same contract:
//!
//! * Producers call [`commit`](Self::commit) whenever a machine's state
//!   changes, supplying a 64-bit fingerprint of the post-change state and
//!   the machine's next self-wake tick (recipe completion, fuel burn-down,
//!   charge threshold, …) or `None` when it can sleep forever.
//! * [`is_dirty`](Self::is_dirty) is the O(1) front door for hot loops that
//!   re-evaluate machines opportunistically: an unchanged fingerprint means
//!   the machine's committed state is current and it may be skipped.
//! * [`tick`](Self::tick) pops exactly the machines whose wake tick
//!   arrived — never a poll — and slices the burst through a
//!   [`TickBudgetQueue`] so 10,000 simultaneous completions spread across
//!   ticks instead of spiking one.
//! * [`record_delta`](Self::record_delta) accumulates outbound
//!   state/network updates last-write-wins; [`drain_deltas_into`] emits
//!   them once per flush in sorted, deterministic order.
//!
//! The runtime is fully generic (`K` machine key, `V` delta payload) and
//! holds no mod names — see [`crate::culinary_adapter::CookingPotCluster`]
//! for the culinary instantiation.

use std::hash::Hash;

use mc_spine_core::perf_kit::{
    BoundedScratchVec, CoalescedDeltaBatch, DirtyFingerprintGate, DrainReport, EventWakeWheel,
    FingerprintStats, TickBudgetQueue,
};

/// Sleep/wake/coalesce runtime for one family of modded machines.
///
/// `K` is the machine key (block position, entity id, network node — any
/// `Ord + Eq + Hash + Clone` type). `V` is whatever outbound delta the
/// family syncs (results, snapshots, packets).
#[derive(Clone, Debug)]
pub struct ModTickRuntime<K, V> {
    /// Committed state fingerprints — the O(1) idle-skip front door.
    gate: DirtyFingerprintGate<K>,
    /// Exact-tick wake schedule for armed machines.
    wheel: EventWakeWheel<K>,
    /// Due-but-unprocessed backlog sliced by per-tick budgets.
    backlog: TickBudgetQueue<K>,
    /// Last-write-wins outbound deltas awaiting the next flush.
    deltas: CoalescedDeltaBatch<K, V>,
    /// Reusable buffer for `pop_due` output — no steady-state allocation.
    due_scratch: BoundedScratchVec<K>,
}

impl<K: Ord + Eq + Hash + Clone, V> Default for ModTickRuntime<K, V> {
    fn default() -> Self {
        Self {
            gate: DirtyFingerprintGate::new(),
            wheel: EventWakeWheel::new(),
            backlog: TickBudgetQueue::new(),
            deltas: CoalescedDeltaBatch::new(),
            due_scratch: BoundedScratchVec::new(),
        }
    }
}

impl<K: Ord + Eq + Hash + Clone, V> ModTickRuntime<K, V> {
    /// Empty runtime.
    pub fn new() -> Self {
        Self::default()
    }

    /// O(1) dirty check: `true` when `fingerprint` differs from `key`'s
    /// committed state (or nothing was ever committed) — the caller should
    /// re-evaluate the machine and [`commit`](Self::commit) the result.
    /// `false` is a counted skip: the machine provably hasn't changed.
    #[inline]
    pub fn is_dirty(&mut self, key: &K, fingerprint: u64) -> bool {
        !self.gate.should_skip(key, fingerprint)
    }

    /// Commit `key`'s post-change fingerprint and (re)arm its wake.
    ///
    /// `wake_at: Some(t)` schedules an exact-tick wake — including a tick
    /// at or before the next [`tick`](Self::tick) call's `now`, which is
    /// how a same-tick mutation produces a same-tick wake. `None` parks
    /// the machine: it costs nothing until the next `commit` re-arms it.
    #[inline]
    pub fn commit(&mut self, key: K, fingerprint: u64, wake_at: Option<u64>) {
        self.gate.commit(key.clone(), fingerprint);
        match wake_at {
            Some(tick) => self.wheel.schedule(key, tick),
            None => {
                self.wheel.cancel(&key);
            }
        }
    }

    /// Forget `key` entirely — gate, schedule, backlog, and pending delta.
    pub fn remove(&mut self, key: &K) {
        self.gate.remove(key);
        self.wheel.cancel(key);
        self.backlog.remove(key);
        self.deltas.remove(key);
    }

    /// `key`'s armed wake tick, if any.
    pub fn wake_of(&self, key: &K) -> Option<u64> {
        self.wheel.wake_of(key)
    }

    /// Move every machine due at or before `now` onto the backlog.
    /// Returns how many newly-queued keys this call added — keys already
    /// sitting in the backlog are not double-counted (the queue dedupes).
    pub fn enqueue_due(&mut self, now: u64) -> usize {
        self.due_scratch.clear();
        self.wheel.pop_due(now, &mut self.due_scratch);
        let mut added = 0;
        for i in 0..self.due_scratch.len() {
            if self.backlog.enqueue(self.due_scratch[i].clone()) {
                added += 1;
            }
        }
        added
    }

    /// Move up to `max_items` due keys (and `max_cost` cumulative `cost_fn`
    /// units) off the backlog into `out`, FIFO. Burst leftovers stay queued
    /// for later ticks — the [`DrainReport`] tells the caller how much the
    /// tick actually absorbed.
    pub fn drain_backlog(
        &mut self,
        max_items: usize,
        max_cost: u64,
        cost_fn: impl FnMut(&K) -> u64,
        out: &mut Vec<K>,
    ) -> DrainReport {
        self.backlog.drain_budget(max_items, max_cost, cost_fn, out)
    }

    /// Push a key back to the front of the backlog when its processing was
    /// interrupted mid-tick.
    pub fn requeue_front(&mut self, key: K) -> bool {
        self.backlog.requeue_front(key)
    }

    /// One call of the whole intake half: pop the wheel at `now`, then
    /// drain up to the budget into `out`. Process the returned keys, then
    /// [`commit`](Self::commit) each machine's post-processing state.
    pub fn tick(
        &mut self,
        now: u64,
        max_items: usize,
        max_cost: u64,
        cost_fn: impl FnMut(&K) -> u64,
        out: &mut Vec<K>,
    ) -> DrainReport {
        self.enqueue_due(now);
        self.drain_backlog(max_items, max_cost, cost_fn, out)
    }

    /// Accumulate `value` as `key`'s newest outbound delta, collapsing any
    /// older pending one. One flush emits at most one delta per key no
    /// matter how often the machine was touched.
    pub fn record_delta(&mut self, key: K, value: V) {
        self.deltas.record(key, value);
    }

    /// Append all pending deltas to `out` in sorted key order.
    pub fn drain_deltas_into(&mut self, out: &mut Vec<(K, V)>) {
        self.deltas.drain_sorted_into(out);
    }

    /// Gate skip/update counters — the measurable proof idle machines
    /// skip in O(1).
    pub fn gate_stats(&self) -> FingerprintStats {
        self.gate.stats()
    }

    /// Machines armed on the wake wheel.
    pub fn scheduled_len(&self) -> usize {
        self.wheel.len()
    }

    /// Machines due but not yet processed.
    pub fn backlog_len(&self) -> usize {
        self.backlog.len()
    }

    /// Keys with an unflushed outbound delta.
    pub fn pending_delta_len(&self) -> usize {
        self.deltas.len()
    }

    /// `true` when nothing is armed, queued, or pending.
    pub fn is_quiet(&self) -> bool {
        self.wheel.is_empty() && self.backlog.is_empty()
    }
}
