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
//!
//! The shape follows the design of Pumpkin's chunk tick scheduler. Pumpkin is
//! GPL-3.0, so only the design is referenced (see issue #3); this is an
//! independent implementation.

use std::collections::HashMap;

/// Four seconds, in ticks. Derived from the cadence rather than written as a
/// literal so it stays four seconds if the cadence changes.
pub(crate) const MAX_DELAY: usize = super::MAX_DELAY_TICKS;

/// A cell that wants to be visited again later.
///
/// `schedule` deduplicates: a cell already waiting keeps its earlier deadline, so
/// repeated neighbour updates cannot push it further out. `reschedule` is for the
/// deliberate case where a cell that just ran wants to back off, and moves it.
pub(crate) struct TickQueue {
    buckets: Vec<Vec<(i32, i32, i32)>>,
    /// Where a waiting cell sits, so it can be moved or removed without a scan.
    waiting: HashMap<(i32, i32, i32), usize>,
    cursor: usize,
}

impl TickQueue {
    pub(crate) fn new() -> Self {
        Self { buckets: (0..MAX_DELAY).map(|_| Vec::new()).collect(), waiting: HashMap::new(), cursor: 0 }
    }

    pub(crate) fn is_empty(&self) -> bool { self.waiting.is_empty() }
    pub(crate) fn len(&self) -> usize { self.waiting.len() }

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
        if self.waiting.contains_key(&pos) { return false }
        let b = self.bucket_of(delay);
        self.buckets[b].push(pos);
        self.waiting.insert(pos, b);
        true
    }

    /// Move `pos` to a new deadline, inserting it if it is not waiting. A cell
    /// that just did work and wants to back off uses this instead of `schedule`.
    pub(crate) fn reschedule(&mut self, pos: (i32, i32, i32), delay: usize) {
        if let Some(previous) = self.waiting.remove(&pos) {
            if let Some(i) = self.buckets[previous].iter().position(|&p| p == pos) {
                self.buckets[previous].swap_remove(i);
            }
        }
        let b = self.bucket_of(delay);
        self.buckets[b].push(pos);
        self.waiting.insert(pos, b);
    }

    /// Drop `pos` from the queue if present.
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
        for pos in &out { self.waiting.remove(pos); }
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
        assert_eq!(ticks[0], vec![(1, 2, 3)], "delay 1 must be due on the next tick");
        assert!(ticks[1..].iter().all(|t| t.is_empty()), "it must not be due again");
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
        q.reschedule((0, 0, 0), 5);
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
        assert!(run(4, &mut q).iter().all(|t| t.is_empty()), "a cancelled cell must not fire");
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
        assert_eq!(ticks[0], vec![(1, 1, 1)], "delay 0 must still be queued, due next tick");
        let late: Vec<_> = ticks.iter().flatten().copied().collect();
        assert!(late.contains(&(2, 2, 2)), "an over-long delay must be clamped, not lost");
        assert!(q.is_empty());
    }

    #[test]
    fn the_ring_wraps_without_losing_entries() {
        let mut q = TickQueue::new();
        for d in 1..=MAX_DELAY { q.schedule((d as i32, 0, 0), d); }
        let ticks = run(MAX_DELAY + 1, &mut q);
        let fired: usize = ticks.iter().map(|t| t.len()).sum();
        assert_eq!(fired, MAX_DELAY, "every entry must fire exactly once");
        assert!(q.is_empty());
    }

    #[test]
    fn an_empty_queue_costs_nothing_to_poll() {
        let mut q = TickQueue::new();
        assert!(q.is_empty());
        for _ in 0..MAX_DELAY * 2 { assert!(q.due().is_empty()); }
        assert_eq!(q.len(), 0);
    }
}
