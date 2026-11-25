//! A delay-indexed queue for block positions that want to be visited later.
//!
//! The systems that need this used to iterate every registered cell each tick and
//! skip the ones whose backoff had not elapsed. That cost is proportional to the
//! number of registered cells, not to the number that actually come due, so ten
//! thousand water cells cost ten thousand iterations and a Vec per tick even when
//! a handful are due. The time-contract states that a scheduled tick's delay must
//! not be read as a permanent sweep of every cell; the sweep was that reading.
//!
//! Indexing the queue by delay makes the per-tick cost proportional to the cells
//! that come due, and the dedup map means a cell can be waiting only once.

use megu_core::coords::CoordMap;
use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap};
use std::hash::Hash;

/// Four seconds, in ticks. Derived from the cadence rather than written as a
/// literal so it stays four seconds if the cadence changes.
pub(crate) const MAX_DELAY: usize = super::MAX_DELAY_TICKS;

/// A deadline queue for machines that should sleep until an event or scheduled
/// update makes them due. The map is authoritative; superseded heap entries are
/// lazy tombstones and are discarded when they reach the front or during compaction.
#[allow(dead_code)]
pub(crate) struct DueMachineQueue<K: Eq + Hash + Ord + Copy> {
    deadlines: HashMap<K, u64>,
    due: BinaryHeap<Reverse<(u64, K)>>,
}

#[allow(dead_code)]
impl<K: Eq + Hash + Ord + Copy> DueMachineQueue<K> {
    const COMPACTION_SLACK: usize = 64;

    pub(crate) fn new() -> Self {
        Self {
            deadlines: HashMap::new(),
            due: BinaryHeap::new(),
        }
    }

    /// Schedule a machine once, retaining the earliest requested deadline.
    pub(crate) fn schedule_due(&mut self, key: K, due_tick: u64) {
        if let Some(&current) = self.deadlines.get(&key) {
            if current <= due_tick {
                return;
            }
        }

        self.deadlines.insert(key, due_tick);
        self.due.push(Reverse((due_tick, key)));
        self.compact_if_needed();
    }

    /// Make a machine due immediately after a mutation or other wake event.
    pub(crate) fn wake(&mut self, key: K, now_tick: u64) {
        self.schedule_due(key, now_tick);
    }

    /// Cancel a machine's pending update. Heap removal is lazy and amortized;
    /// stale entries are bounded by periodic compaction.
    pub(crate) fn remove(&mut self, key: K) {
        self.deadlines.remove(&key);
        if self.deadlines.is_empty() {
            // The compaction threshold bounds the number of tombstones here.
            self.due.clear();
        } else {
            self.compact_if_needed();
        }
    }

    /// Append all due keys in `(due_tick, key)` order.
    pub(crate) fn drain_due_into(&mut self, now_tick: u64, out: &mut Vec<K>) {
        if self
            .due
            .peek()
            .is_none_or(|Reverse((next, _))| *next > now_tick)
        {
            return;
        }

        while let Some(&Reverse((due_tick, key))) = self.due.peek() {
            if due_tick > now_tick {
                break;
            }
            self.due.pop();
            if self.deadlines.get(&key).copied() == Some(due_tick) {
                self.deadlines.remove(&key);
                out.push(key);
            }
        }

        if self.deadlines.is_empty() {
            self.due.clear();
        } else {
            self.compact_if_needed();
        }
    }

    pub(crate) fn clear(&mut self) {
        self.deadlines.clear();
        self.due.clear();
    }

    pub(crate) fn len(&self) -> usize {
        self.deadlines.len()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.deadlines.is_empty()
    }

    pub(crate) fn next_due_tick(&self) -> Option<u64> {
        match self.due.peek() {
            Some(Reverse((tick, key))) if self.deadlines.get(key).copied() == Some(*tick) => {
                Some(*tick)
            }
            Some(_) => self.deadlines.values().copied().min(),
            None => None,
        }
    }

    fn compact_if_needed(&mut self) {
        let limit = self
            .deadlines
            .len()
            .saturating_mul(2)
            .saturating_add(Self::COMPACTION_SLACK);
        if self.due.len() > limit {
            self.due = self
                .deadlines
                .iter()
                .map(|(&key, &tick)| Reverse((tick, key)))
                .collect();
        }
    }
}

/// A cell that wants to be visited again later.
///
/// `schedule` deduplicates: a cell already waiting keeps its earlier deadline, so
/// repeated neighbour updates cannot push it further out. `requeue` is for the
/// deliberate case where a cell that just ran wants to back off, and moves it.
pub(crate) struct TickQueue {
    buckets: Vec<Vec<(i32, i32, i32)>>,
    /// Where a waiting cell sits, so it can be moved or removed without a scan.
    waiting: CoordMap<(i32, i32, i32), usize>,
    cursor: usize,
}

impl TickQueue {
    pub(crate) fn new() -> Self {
        Self {
            buckets: (0..MAX_DELAY).map(|_| Vec::new()).collect(),
            waiting: CoordMap::default(),
            cursor: 0,
        }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.waiting.is_empty()
    }
    /// Test-only: nothing in the tick path needs the count, only whether the queue
    /// has anything to do.
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.waiting.len()
    }

    /// Deadline bucket for `delay` ticks from the current one. A delay of zero or
    /// more than `MAX_DELAY` is clamped into range rather than dropped, because a
    /// dropped schedule is a cell that silently stops updating.
    fn bucket_of(&self, delay: usize) -> usize {
        // `cursor` is the bucket the next `due()` returns, so a delay of one must
        // land there: the cell is due on the tick immediately after scheduling.
        let d = delay.clamp(1, MAX_DELAY) - 1;
        (self.cursor + d) % MAX_DELAY
    }

    /// Queue `pos` if it is not already waiting. Returns whether it was added.
    pub(crate) fn schedule(&mut self, pos: (i32, i32, i32), delay: usize) -> bool {
        if self.waiting.contains_key(&pos) {
            return false;
        }
        let b = self.bucket_of(delay);
        self.buckets[b].push(pos);
        self.waiting.insert(pos, b);
        true
    }

    /// Move `pos` to a new deadline, inserting it if it is not waiting. A cell
    /// that just did work and wants to back off uses this instead of `schedule`.
    pub(crate) fn requeue(&mut self, pos: (i32, i32, i32), delay: usize) {
        if let Some(previous) = self.waiting.remove(&pos) {
            if let Some(i) = self.buckets[previous].iter().position(|&p| p == pos) {
                self.buckets[previous].swap_remove(i);
            }
        }
        let b = self.bucket_of(delay);
        self.buckets[b].push(pos);
        self.waiting.insert(pos, b);
    }

    /// Move `pos` to the next tick's bucket if it is already waiting, and report
    /// whether it was. This is what a wake condition needs: a neighbour change
    /// must make a backed-off cell due promptly. It deliberately does not insert,
    /// because invalidation runs for every block change and would otherwise fill
    /// the queue with cells that are not registered with this system at all.
    pub(crate) fn promote(&mut self, pos: (i32, i32, i32)) -> bool {
        if !self.waiting.contains_key(&pos) {
            return false;
        }
        self.requeue(pos, 1);
        true
    }

    /// Drop `pos` from the queue if present.
    ///
    /// Production never needs this: a cell whose block is gone is still visited once
    /// and removes itself from the registry instead of re-queueing, so the entry
    /// leaves the queue on its own.
    #[cfg(test)]
    pub(crate) fn cancel(&mut self, pos: (i32, i32, i32)) {
        if let Some(previous) = self.waiting.remove(&pos) {
            if let Some(i) = self.buckets[previous].iter().position(|&p| p == pos) {
                self.buckets[previous].swap_remove(i);
            }
        }
    }

    /// Take the cells due this tick and advance. Cost is proportional to the
    /// number of due cells, and an empty bucket costs one swap of an empty Vec.
    pub(crate) fn due(&mut self) -> Vec<(i32, i32, i32)> {
        let out = std::mem::take(&mut self.buckets[self.cursor]);
        for pos in &out {
            self.waiting.remove(pos);
        }
        self.cursor = (self.cursor + 1) % MAX_DELAY;
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(ticks: usize, q: &mut TickQueue) -> Vec<Vec<(i32, i32, i32)>> {
        (0..ticks).map(|_| q.due()).collect()
    }

    #[test]
    fn a_cell_becomes_due_after_exactly_its_delay() {
        let mut q = TickQueue::new();
        q.schedule((1, 2, 3), 1);
        let ticks = run(4, &mut q);
        assert_eq!(
            ticks[0],
            vec![(1, 2, 3)],
            "delay 1 must be due on the next tick"
        );
        assert!(
            ticks[1..].iter().all(|t| t.is_empty()),
            "it must not be due again"
        );
        assert!(q.is_empty());
    }

    #[test]
    fn a_repeated_schedule_keeps_the_earlier_deadline() {
        // A neighbour update must not push an already-waiting cell further out,
        // or a cell under continuous neighbour churn would never run.
        let mut q = TickQueue::new();
        q.schedule((0, 0, 0), 3);
        assert!(!q.schedule((0, 0, 0), 30), "a duplicate must be refused");
        assert_eq!(q.len(), 1);
        assert_eq!(run(3, &mut q)[2], vec![(0, 0, 0)]);
    }

    #[test]
    fn rescheduling_moves_the_deadline() {
        let mut q = TickQueue::new();
        q.schedule((0, 0, 0), 2);
        q.requeue((0, 0, 0), 5);
        let ticks = run(6, &mut q);
        assert!(ticks[1].is_empty(), "the old deadline must not fire");
        assert_eq!(ticks[4], vec![(0, 0, 0)]);
        assert_eq!(q.len(), 0);
    }

    #[test]
    fn cancelling_removes_it_from_its_bucket() {
        let mut q = TickQueue::new();
        q.schedule((7, 7, 7), 2);
        q.cancel((7, 7, 7));
        assert!(q.is_empty());
        assert!(
            run(4, &mut q).iter().all(|t| t.is_empty()),
            "a cancelled cell must not fire"
        );
    }

    #[test]
    fn delays_are_clamped_rather_than_dropped() {
        // A dropped schedule is a cell that silently stops updating, which is the
        // failure mode the wake-condition contract calls unacceptable.
        let mut q = TickQueue::new();
        q.schedule((1, 1, 1), 0);
        q.schedule((2, 2, 2), MAX_DELAY * 4);
        assert_eq!(q.len(), 2);
        let ticks = run(MAX_DELAY + 2, &mut q);
        assert_eq!(
            ticks[0],
            vec![(1, 1, 1)],
            "delay 0 must still be queued, due next tick"
        );
        let late: Vec<_> = ticks.iter().flatten().copied().collect();
        assert!(
            late.contains(&(2, 2, 2)),
            "an over-long delay must be clamped, not lost"
        );
        assert!(q.is_empty());
    }

    #[test]
    fn the_ring_wraps_without_losing_entries() {
        let mut q = TickQueue::new();
        for d in 1..=MAX_DELAY {
            q.schedule((d as i32, 0, 0), d);
        }
        let ticks = run(MAX_DELAY + 1, &mut q);
        let fired: usize = ticks.iter().map(|t| t.len()).sum();
        assert_eq!(fired, MAX_DELAY, "every entry must fire exactly once");
        assert!(q.is_empty());
    }

    #[test]
    fn promote_pulls_a_waiting_cell_forward_and_ignores_a_stranger() {
        // A neighbour change must make a backed-off cell due promptly, and must not
        // enqueue a cell that belongs to some other system.
        let mut q = TickQueue::new();
        q.schedule((1, 1, 1), MAX_DELAY);
        assert!(q.promote((1, 1, 1)), "a waiting cell must be promoted");
        assert!(
            !q.promote((9, 9, 9)),
            "an unknown cell must not be inserted"
        );
        assert_eq!(q.len(), 1, "promote must not add entries");
        assert_eq!(
            run(2, &mut q)[0],
            vec![(1, 1, 1)],
            "it must now be due next tick"
        );
        assert!(q.is_empty());
    }

    #[test]
    fn an_empty_queue_costs_nothing_to_poll() {
        let mut q = TickQueue::new();
        assert!(q.is_empty());
        for _ in 0..MAX_DELAY * 2 {
            assert!(q.due().is_empty());
        }
        assert_eq!(q.len(), 0);
    }
}

#[cfg(test)]
mod due_machine_tests {
    use super::DueMachineQueue;

    #[test]
    fn idle_drains_do_not_pop_or_allocate_after_all_machines_are_removed() {
        let mut queue = DueMachineQueue::new();
        for key in 0..1_000_u32 {
            queue.schedule_due(key, 10_000 + u64::from(key));
        }
        for key in 0..1_000_u32 {
            queue.remove(key);
        }
        assert!(queue.is_empty());
        assert!(queue.due.is_empty());

        let mut out = Vec::new();
        for tick in 0..1_000 {
            queue.drain_due_into(tick, &mut out);
            assert!(out.is_empty());
            assert!(queue.due.is_empty(), "idle drain must not pop heap entries");
        }
    }

    #[test]
    fn an_earlier_wakeup_promotes_and_fires_only_once() {
        let mut queue = DueMachineQueue::new();
        queue.schedule_due(7_u32, 200);
        queue.wake(7, 10);
        assert_eq!(queue.len(), 1);
        assert_eq!(queue.next_due_tick(), Some(10));

        let mut out = Vec::new();
        queue.drain_due_into(10, &mut out);
        assert_eq!(out, vec![7]);
        queue.drain_due_into(200, &mut out);
        assert_eq!(out, vec![7], "superseded deadline must not fire again");
        assert!(queue.is_empty());
    }

    #[test]
    fn due_keys_are_ordered_by_tick_then_key() {
        let mut queue = DueMachineQueue::new();
        queue.schedule_due(9_u32, 20);
        queue.schedule_due(4, 10);
        queue.schedule_due(2, 20);
        queue.schedule_due(1, 10);

        let mut out = Vec::new();
        queue.drain_due_into(20, &mut out);
        assert_eq!(out, vec![1, 4, 2, 9]);
    }

    #[test]
    fn removal_suppresses_firing_and_churn_keeps_heap_bounded() {
        let mut queue = DueMachineQueue::new();
        queue.schedule_due(0_u32, 1_000);
        queue.schedule_due(1, 2_000);
        queue.remove(0);
        assert_eq!(queue.next_due_tick(), Some(2_000));
        let mut out = Vec::new();
        queue.drain_due_into(1_000, &mut out);
        assert!(out.is_empty());

        queue.schedule_due(0, 10_000);
        for key in 1..10_001_u32 {
            queue.schedule_due(key, u64::from(key));
            queue.remove(key);
            assert!(queue.due.len() <= queue.len() * 2 + DueMachineQueue::<u32>::COMPACTION_SLACK);
        }
        queue.remove(0);
        queue.remove(1);
        assert!(queue.is_empty());
        assert!(queue.due.is_empty());
        queue.drain_due_into(u64::MAX, &mut out);
        assert!(out.is_empty());
    }

    #[test]
    fn clear_discards_scheduled_and_stale_entries() {
        let mut queue = DueMachineQueue::new();
        queue.schedule_due(1_u32, 10);
        queue.wake(1, 2);
        queue.schedule_due(2, 20);
        queue.clear();
        assert!(queue.is_empty());
        assert_eq!(queue.len(), 0);
        assert_eq!(queue.next_due_tick(), None);
    }
}
