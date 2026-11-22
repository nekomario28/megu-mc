//! Scheduled block ticks, using the native per-chunk collection contract.

use crate::generation_math::fastutil_mix64;
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};

type Pos = (i32, i32, i32);
type ChunkPos = (i32, i32);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ScheduledTick {
    pub pos: Pos,
    pub block: u32,
    pub due: i64,
    pub priority: i8,
    pub sub_order: i64,
}

fn full_order(a: &ScheduledTick, b: &ScheduledTick) -> Ordering {
    (a.due, a.priority, a.sub_order).cmp(&(b.due, b.priority, b.sub_order))
}

fn within_tick(a: &ScheduledTick, b: &ScheduledTick) -> Ordering {
    (a.priority, a.sub_order).cmp(&(b.priority, b.sub_order))
}

thread_local! {
    /// `collect` runs once per queue per tick and its per-chunk container
    /// heap is pure scratch — it never escapes the drain — so it rides a
    /// bounded thread-local instead of allocating a fresh `Vec` every pass.
    /// The buffer is taken out (not borrowed) for the drain so a nested
    /// `collect` would still see a usable empty `Vec`.
    static COLLECT_CONTAINERS: std::cell::RefCell<Vec<ChunkPos>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// Retained-capacity bound for the shared `collect` container heap: a
/// one-off wide drain shrinks back on return instead of pinning peak memory.
const COLLECT_SCRATCH_KEEP_BYTES: usize = 256 * 1024;

fn bound_collect_scratch(v: &mut Vec<ChunkPos>) {
    let keep = COLLECT_SCRATCH_KEEP_BYTES / std::mem::size_of::<ChunkPos>().max(1);
    if v.capacity() > keep {
        v.shrink_to(keep);
    }
}

/// Retained capacity of the shared `collect` container heap — test-only proof
/// that the scratch buffer is reused (and bounded) across drains.
#[cfg(test)]
fn collect_scratch_capacity() -> usize {
    COLLECT_CONTAINERS.with(|cell| cell.borrow().capacity())
}

// Java PriorityQueue's treatment of equal elements is observable when two
// containers have the same next priority/sub-order.
fn heap_push<T>(heap: &mut Vec<T>, value: T, cmp: impl Fn(&T, &T) -> Ordering) {
    heap.push(value);
    let mut k = heap.len() - 1;
    while k > 0 {
        let parent = (k - 1) / 2;
        if cmp(&heap[k], &heap[parent]) != Ordering::Less {
            break;
        }
        heap.swap(k, parent);
        k = parent;
    }
}

fn heap_pop<T>(heap: &mut Vec<T>, cmp: impl Fn(&T, &T) -> Ordering) -> Option<T> {
    let last = heap.pop()?;
    if heap.is_empty() {
        return Some(last);
    }
    let result = std::mem::replace(&mut heap[0], last);
    let mut k = 0;
    while k < heap.len() / 2 {
        let mut child = k * 2 + 1;
        if child + 1 < heap.len() && cmp(&heap[child], &heap[child + 1]) == Ordering::Greater {
            child += 1;
        }
        if cmp(&heap[k], &heap[child]) != Ordering::Greater {
            break;
        }
        heap.swap(k, child);
        k = child;
    }
    Some(result)
}

fn chunk_key((x, z): ChunkPos) -> u64 {
    u64::from(x as u32) | (u64::from(z as u32) << 32)
}

fn chunk_from_key(key: u64) -> ChunkPos {
    (key as u32 as i32, (key >> 32) as u32 as i32)
}

/// The native next-deadline map is fastutil's Long2LongOpenHashMap. Its
/// descending-table iteration supplies the insertion order of tied chunks.
#[derive(Clone)]
struct NextChunks {
    slots: Vec<Option<(u64, i64)>>,
    zero: Option<i64>,
    len: usize,
}

impl NextChunks {
    fn new() -> Self {
        Self {
            slots: vec![None; 32],
            zero: None,
            len: 0,
        }
    }

    fn slot(&self, key: u64) -> usize {
        let mask = self.slots.len() - 1;
        let mut i = fastutil_mix64(key) as usize & mask;
        while self.slots[i].is_some_and(|(old, _)| old != key) {
            i = (i + 1) & mask;
        }
        i
    }

    fn put(&mut self, key: u64, due: i64) {
        if key == 0 {
            if self.zero.replace(due).is_none() {
                self.len += 1;
            }
        } else {
            let i = self.slot(key);
            if self.slots[i].replace((key, due)).is_none() {
                self.len += 1;
            }
        }
        if self.len > self.slots.len() * 3 / 4 {
            self.resize(self.slots.len() * 2);
        }
    }

    fn resize(&mut self, new_len: usize) {
        let old = std::mem::replace(&mut self.slots, vec![None; new_len]);
        // Fastutil rehash scans the previous table from high to low.
        for (key, due) in old.into_iter().rev().flatten() {
            let i = self.slot(key);
            self.slots[i] = Some((key, due));
        }
    }

    fn remove_slot(&mut self, mut hole: usize, mut on_wrap: impl FnMut(u64)) {
        self.len -= 1;
        let mask = self.slots.len() - 1;
        let mut scan = (hole + 1) & mask;
        while let Some((other, due)) = self.slots[scan] {
            let home = fastutil_mix64(other) as usize & mask;
            let move_here = if hole <= scan {
                hole >= home || home > scan
            } else {
                hole >= home && home > scan
            };
            if move_here {
                if scan < hole {
                    on_wrap(other);
                }
                self.slots[hole] = Some((other, due));
                hole = scan;
            }
            scan = (scan + 1) & mask;
        }
        self.slots[hole] = None;
    }

    fn take_ready(&mut self, mut ready: impl FnMut(u64, i64) -> bool) {
        if self.zero.is_some_and(|due| ready(0, due)) {
            self.zero = None;
            self.len -= 1;
        }
        // The native iterator removes in place. Keys shifted across slot zero
        // are visited in shift order after the descending table scan.
        let mut wrapped = Vec::new();
        for i in (0..self.slots.len()).rev() {
            if self.slots[i].is_some_and(|(key, due)| ready(key, due)) {
                self.remove_slot(i, |key| wrapped.push(key));
            }
        }
        for key in wrapped {
            let i = self.slot(key);
            let (_, due) = self.slots[i].unwrap();
            if ready(key, due) {
                self.remove_slot(i, |_| {});
                // Fastutil's iterator uses regular map removal for wrapped
                // keys, which can shrink a previously grown table.
                let size = self.slots.len();
                if size > 32 && self.len < size * 3 / 16 {
                    self.resize(size / 2);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_collection_order_and_deduplication() {
        let mut cases = 0;
        for row in include_str!("../tests/data/scheduled_tick_1211.tsv").lines() {
            let f: Vec<_> = row.split('\t').collect();
            assert_eq!(f.len(), 6);
            let cold: HashSet<_> = f[2]
                .split(';')
                .filter(|v| !v.is_empty())
                .map(|v| {
                    let (x, z) = v.split_once(',').unwrap();
                    (x.parse::<i32>().unwrap(), z.parse::<i32>().unwrap())
                })
                .collect();
            let limit = f[3].parse().unwrap();
            let mut queue = ScheduledTicks::new();
            for entry in f[4].split(';') {
                let v: Vec<_> = entry.split(',').collect();
                assert_eq!(v.len(), 7);
                queue.schedule(ScheduledTick {
                    block: if v[0] == "A" { 1 } else { 2 },
                    pos: (
                        v[1].parse().unwrap(),
                        v[2].parse().unwrap(),
                        v[3].parse().unwrap(),
                    ),
                    due: v[4].parse().unwrap(),
                    priority: match v[5] {
                        "HIGH" => -1,
                        "NORMAL" => 0,
                        "LOW" => 1,
                        _ => panic!("bad priority"),
                    },
                    sub_order: v[6].parse().unwrap(),
                });
            }
            let first = queue.collect(10, limit, |c| !cold.contains(&c));
            let scenario: usize = f[0].parse().unwrap();
            if scenario.is_multiple_of(5) {
                if let Some(tick) = first.first() {
                    queue.schedule(ScheduledTick {
                        due: 11,
                        priority: 0,
                        sub_order: 200,
                        ..*tick
                    });
                }
            }
            let second = queue.collect(11, limit, |c| !cold.contains(&c));
            let render = |ticks: Vec<ScheduledTick>| -> Vec<String> {
                ticks
                    .into_iter()
                    .map(|t| {
                        format!(
                            "{},{},{},{}",
                            if t.block == 1 { "A" } else { "B" },
                            t.pos.0,
                            t.pos.1,
                            t.pos.2
                        )
                    })
                    .collect()
            };
            let (want_first, want_second) = f[5].split_once('|').unwrap();
            let expected = |s: &str| -> Vec<String> {
                s.split(';')
                    .filter(|v| !v.is_empty())
                    .map(str::to_owned)
                    .collect()
            };
            assert_eq!(
                render(first),
                expected(want_first),
                "first collection case {}",
                f[0]
            );
            assert_eq!(
                render(second),
                expected(want_second),
                "second collection case {}",
                f[0]
            );
            cases += 1;
        }
        assert_eq!(cases, 48);
    }

    #[test]
    fn native_callback_metadata_for_block_and_fluid_delivery() {
        let cases = delivery_fixture::replay(false, |_, _, _| unreachable!());
        assert_eq!(cases, 5);
    }

    /// `LevelChunkTicks.unpack`: deduplication is on the (pos, block) identity,
    /// each chunk's list starts its negative sub-order at its own length, and
    /// every proto tick lands due `now` at normal priority. A mutant that
    /// counts pending entries globally instead of per chunk produces the
    /// wrong sub-orders here.
    #[test]
    fn unpack_generated_restarts_negative_sub_order_per_chunk() {
        let mut queue = ScheduledTicks::new();
        queue.unpack_generated(
            vec![
                ((1, 2, 3), 7),
                ((1, 2, 3), 7), // exact identity: deduplicated before counting
                ((1, 2, 3), 9), // same pos, different block: a second tick
                ((1, 4, 3), 7),
                ((17, 0, 17), 7),   // chunk (1,1)
                ((-16, 0, -16), 7), // chunk (-1,-1)
            ],
            50,
        );
        let out = queue.collect(50, 100, |_| true);
        assert_eq!(out.len(), 5, "the duplicate identity is dropped");
        assert!(
            out.iter().all(|t| t.due == 50 && t.priority == 0),
            "proto ticks are zero-delay, normal priority"
        );
        // Chunk (0,0) holds three ticks with sub_orders -3, -2, -1 in pending
        // order; all of them sort ahead of the single-tick chunks' -1 heads.
        let head: Vec<_> = out[..3]
            .iter()
            .map(|t| (t.pos, t.block, t.sub_order))
            .collect();
        assert_eq!(
            head,
            vec![
                ((1, 2, 3), 7, -3),
                ((1, 2, 3), 9, -2),
                ((1, 4, 3), 7, -1),
            ],
            "per-chunk sub-order starts at minus the chunk's own pending length"
        );
        let mut tail: Vec<_> = out[3..]
            .iter()
            .map(|t| (t.pos, t.block, t.sub_order))
            .collect();
        tail.sort();
        assert_eq!(
            tail,
            vec![((-16, 0, -16), 7, -1), ((17, 0, 17), 7, -1)],
            "every chunk's list restarts its own negative sub-order"
        );
    }

    /// `LevelTicks.schedule` refuses a duplicate (pos, block) and keeps the
    /// entry already queued — rescheduling must not move a tick earlier or
    /// later, and a different block at the same pos is a separate identity.
    #[test]
    fn schedule_keeps_the_first_of_duplicate_identities() {
        let mut queue = ScheduledTicks::new();
        let first = ScheduledTick {
            pos: (1, 0, 1),
            block: 7,
            due: 5,
            priority: 0,
            sub_order: 0,
        };
        assert!(queue.schedule(first));
        assert!(
            !queue.schedule(ScheduledTick {
                pos: (1, 0, 1),
                block: 7,
                due: 1,
                priority: -3,
                sub_order: 2,
            }),
            "the duplicate identity is refused"
        );
        assert!(
            queue.schedule(ScheduledTick {
                pos: (1, 0, 1),
                block: 8,
                due: 1,
                priority: -3,
                sub_order: 2,
            }),
            "a different block at the same position is a different tick"
        );
        let out = queue.collect(10, 10, |_| true);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].block, 8, "the earlier-due distinct block delivers first");
        assert_eq!(out[1], first, "the original due time is kept, not replaced");
    }

    /// Saved ticks inflate their delays at the load game time with a 32-bit
    /// `SavedTick.delay`: a delay wider than i32 wraps to an overdue tick, and
    /// sub-orders restart per chunk in heap order, discarding the saved ones.
    #[test]
    fn unpack_saved_restarts_sub_order_and_wraps_i32_delays() {
        let mut queue = ScheduledTicks::new();
        assert!(queue.schedule(ScheduledTick {
            pos: (1, 0, 1),
            block: 7,
            due: 100,
            priority: 0,
            sub_order: 9,
        }));
        assert!(queue.schedule(ScheduledTick {
            pos: (2, 0, 1),
            block: 8,
            due: 100,
            priority: -1,
            sub_order: 3,
        }));
        assert!(queue.schedule(ScheduledTick {
            pos: (33, 0, 1),
            block: 9,
            due: 50,
            priority: 0,
            sub_order: 0,
        }));
        assert!(queue.schedule(ScheduledTick {
            pos: (49, 0, 1),
            block: 5,
            due: 60 + i64::from(i32::MAX) + 7,
            priority: 0,
            sub_order: 0,
        }));
        let mut loaded = queue.unpack_saved(60);
        let out = loaded.collect(200, 10, |_| true);
        assert_eq!(out.len(), 4);
        // Chunk (0,0) saved in heap order: the priority -1 tick leads, so its
        // reloaded sub_orders are -2 then -1.
        assert_eq!(out[0].pos, (2, 0, 1));
        assert_eq!(out[0].due, 100, "small delays survive the round-trip");
        assert_eq!(out[0].sub_order, -2);
        assert_eq!(out[1].pos, (1, 0, 1));
        assert_eq!(out[1].sub_order, -1);
        let wrapped = out
            .iter()
            .find(|t| t.pos == (49, 0, 1))
            .expect("the wrapped tick survives");
        assert!(
            wrapped.due < 60,
            "a delay wider than i32 wraps to an overdue trigger, got {}",
            wrapped.due
        );
        let lone = out
            .iter()
            .find(|t| t.pos == (33, 0, 1))
            .expect("the single-tick chunk survives");
        assert_eq!(lone.sub_order, -1, "one-tick chunks restart at -1");
    }

    /// The persisted state must round-trip byte for byte (the fastutil table
    /// layout and heap order are part of the contract), and decoding corrupt
    /// input fails closed on every structural check.
    #[test]
    fn tick_state_roundtrip_is_byte_stable_and_decode_fails_closed() {
        let mut queue = ScheduledTicks::new();
        queue.unpack_generated(vec![((-1, 0, -1), 9), ((17, 0, 17), 8)], 40);
        assert!(queue.schedule(ScheduledTick {
            pos: (1, 2, 3),
            block: 7,
            due: 40,
            priority: 0,
            sub_order: 0,
        }));
        assert!(queue.schedule(ScheduledTick {
            pos: (0, 0, 0),
            block: 3,
            due: 41,
            priority: 1,
            sub_order: 4,
        }));
        let mut bytes = Vec::new();
        queue.encode(&mut bytes);
        let mut slice = &bytes[..];
        let decoded = ScheduledTicks::decode(&mut slice).expect("encode output decodes");
        let mut again = Vec::new();
        decoded.encode(&mut again);
        assert_eq!(bytes, again, "decode/encode is byte-stable");

        // Chunks are stored sorted by (x, z): (-1,-1) 1 tick, (0,0) 2 ticks,
        // (1,1) 1 tick. A chunk record is 12 header bytes + 33 bytes per tick;
        // a tick's `due` field starts at offset 16 inside its 33-byte record.
        let chunk2_tick2_due = 4 + (12 + 33) + 12 + 33 + 16;
        let mut bad = bytes.clone();
        bad[4 + 12..4 + 12 + 4].copy_from_slice(&17i32.to_be_bytes()); // first chunk's tick pos.x
        assert!(
            ScheduledTicks::decode(&mut &bad[..]).is_err(),
            "a tick outside its declared chunk is refused"
        );
        let mut bad = bytes.clone();
        bad[chunk2_tick2_due..chunk2_tick2_due + 8]
            .copy_from_slice(&1i64.to_be_bytes()); // second tick of chunk (0,0) before its parent
        assert!(
            ScheduledTicks::decode(&mut &bad[..]).is_err(),
            "a heap child ordering before its parent is refused"
        );
        assert!(
            ScheduledTicks::decode(&mut &bytes[..bytes.len() - 1]).is_err(),
            "truncated state is refused"
        );
    }

    /// XPOLL-REDSTONE-TICKS: a 1,000-drain mixed workload must deliver the
    /// identical `(pos, block, due, priority, sub_order)` stream on two
    /// identically scheduled queues, keep every callback at or before its
    /// `now`, and leave the shared container scratch at a stable bounded
    /// capacity — zero steady-state growth.
    #[test]
    fn xpoll_redstone_ticks_drain_preserves_order_and_bounded_scratch() {
        fn schedule(queue: &mut ScheduledTicks) -> usize {
            let mut state = 0x9E37_79B9_7F4A_7C15u64;
            let mut accepted = 0;
            for i in 0..2_000u64 {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                let chunk = (i % 64) as i32;
                let tick = ScheduledTick {
                    pos: (
                        chunk * 16 + (i % 7) as i32,
                        (i % 12) as i32,
                        ((i * 3) % 16) as i32,
                    ),
                    block: (i % 5) as u32,
                    due: (i % 40) as i64,
                    priority: ((i % 7) as i64 - 3) as i8,
                    sub_order: i as i64,
                };
                accepted += usize::from(queue.schedule(tick));
            }
            accepted
        }

        let mut first = ScheduledTicks::new();
        let mut second = ScheduledTicks::new();
        let accepted = schedule(&mut first);
        assert_eq!(accepted, schedule(&mut second));

        let mut drained_first = Vec::new();
        let mut drained_second = Vec::new();
        let drain_start = std::time::Instant::now();
        for now in 0..1_000i64 {
            // Ticks all come due inside the first 40 drains; the remaining
            // 960 collects exercise the empty fast path.
            let a = first.collect(now, 1_000, |_| true);
            let b = second.collect(now, 1_000, |_| true);
            assert!(a.iter().all(|t| t.due <= now), "tick delivered before due");
            assert_eq!(a, b, "identical queues diverged at now {now}");
            drained_first.extend(a);
            drained_second.extend(b);
        }
        // 40 working drains + 960 O(1) empty fast paths across two queues.
        eprintln!(
            "xpoll_redstone_ticks drain={} ticks={} collects=2000 elapsed={:.3}ms",
            accepted,
            drained_first.len(),
            drain_start.elapsed().as_secs_f64() * 1e3,
        );
        assert_eq!(drained_first, drained_second);
        assert_eq!(drained_first.len(), accepted, "every accepted tick delivered once");
        // Every tick is delivered at the first `now` that reaches its due
        // (the limit never binds here), so the stream's due stamps are
        // nondecreasing across the whole run.
        assert!(
            drained_first.windows(2).all(|w| w[0].due <= w[1].due),
            "a later-due tick overtook an earlier one"
        );

        // The shared scratch reached its working set during the wide drains
        // and did not grow while the remaining collects hit the fast path.
        let capacity = collect_scratch_capacity();
        assert!(capacity > 0, "the drain used the shared container scratch");
        assert!(
            capacity * std::mem::size_of::<ChunkPos>() <= COLLECT_SCRATCH_KEEP_BYTES,
            "scratch stayed under the 256 KiB bound"
        );
        for _ in 0..200 {
            assert!(first.collect(2_000, 1_000, |_| true).is_empty());
        }
        assert_eq!(collect_scratch_capacity(), capacity, "steady-state scratch growth");
    }

    /// XPOLL-REDSTONE-TICKS: empty and future-only queues answer in O(1)
    /// without delivering or dropping pending work; ineligible chunks keep
    /// their deadlines for a later eligible drain.
    #[test]
    fn xpoll_redstone_ticks_empty_and_future_only_fast_path() {
        let mut queue = ScheduledTicks::new();
        assert!(queue.collect(0, 100, |_| true).is_empty());
        assert_eq!(collect_scratch_capacity(), 0, "empty drains never touch scratch");

        for i in 0..32i32 {
            assert!(queue.schedule(ScheduledTick {
                pos: (i * 16, 0, 0),
                block: 1,
                due: 100 + i64::from(i),
                priority: 0,
                sub_order: i64::from(i),
            }));
        }
        assert!(queue.collect(50, 100, |_| true).is_empty(), "nothing is due yet");
        assert!(queue.has_pending_matching(|_| true), "future ticks stay pending");

        let due = queue.collect(200, 100, |_| true);
        assert_eq!(due.len(), 32);
        assert_eq!(
            due.iter().map(|t| t.pos.0).collect::<Vec<_>>(),
            (0..32i32).map(|i| i * 16).collect::<Vec<_>>(),
            "one tick per chunk drains in sub-order"
        );
        assert!(queue.collect(300, 100, |_| true).is_empty());

        // An ineligible chunk is not drained and not forgotten.
        assert!(queue.schedule(ScheduledTick {
            pos: (512, 0, 0),
            block: 2,
            due: 10,
            priority: 0,
            sub_order: 99,
        }));
        assert!(queue.collect(400, 100, |_| false).is_empty());
        assert!(queue.has_pending_matching(|t| t.block == 2));
        let rest = queue.collect(400, 100, |_| true);
        assert_eq!(rest.len(), 1);
        assert_eq!(rest[0].pos, (512, 0, 0));
        assert!(!queue.has_pending_matching(|_| true));
    }

    /// `LevelTicks` drains each ready chunk in its own (triggerTick,
    /// priority, subTickOrder) order and merges the per-chunk heads by
    /// INTRA_TICK_DRAIN_ORDER — priority first, subTickOrder breaking ties —
    /// so equal-priority ticks interleave strictly by insertion order across
    /// chunk boundaries.
    #[test]
    fn intra_tick_merge_orders_priority_then_sub_order_across_chunks() {
        // Four chunks at one due/priority: distinct sub-orders make the
        // expected merge exactly the sub-order sort.
        let mut queue = ScheduledTicks::new();
        let mut expected = Vec::new();
        for sub_order in 0..12i64 {
            let chunk = (sub_order % 4) as i32;
            let pos = (chunk * 16 + (sub_order / 4) as i32, 64, 0);
            expected.push(pos);
            assert!(queue.schedule(ScheduledTick {
                pos,
                block: 1,
                due: 5,
                priority: 0,
                sub_order,
            }));
        }
        let out = queue.collect(5, 100, |_| true);
        assert_eq!(
            out.iter().map(|t| t.pos).collect::<Vec<_>>(),
            expected,
            "equal-priority ticks must interleave by subTickOrder alone"
        );

        // Mixed priorities at one due: the merge is the global
        // (priority, sub_order) sort across chunk boundaries.
        let mut queue = ScheduledTicks::new();
        for (pos, priority, sub_order) in [
            ((0i32, 64, 0), -3i8, 4i64),
            ((16, 64, 0), -3, 1),
            ((1, 64, 0), 0, 2),
            ((17, 64, 0), 0, 0),
            ((32, 64, 0), 3, 3),
        ] {
            assert!(queue.schedule(ScheduledTick {
                pos,
                block: 1,
                due: 5,
                priority,
                sub_order,
            }));
        }
        let out = queue.collect(5, 100, |_| true);
        assert_eq!(
            out.iter().map(|t| t.sub_order).collect::<Vec<_>>(),
            vec![1, 4, 0, 2, 3],
            "priority must lead sub-order in the cross-chunk merge"
        );
    }

    /// The merge is not a flat (priority, sub_order) sort: each chunk's own
    /// heap pops in (triggerTick, priority, subTickOrder), so inside one
    /// chunk an earlier-due lower-priority tick keeps its slot ahead of a
    /// later-due EXTREMELY_HIGH tick — the exact shape the pinned native
    /// wave shows for {VERY_LOW@100, EXTREMELY_LOW@100, EXTREMELY_HIGH@102}
    /// delivered in that order at game time 102.
    #[test]
    fn a_chunk_drains_in_trigger_order_not_flat_priority_order() {
        let mut queue = ScheduledTicks::new();
        for tick in [
            // Chunk (0,0): due 5 LOW then due 7 EXTREMELY_HIGH.
            ScheduledTick {
                pos: (0, 64, 0),
                block: 1,
                due: 5,
                priority: 1,
                sub_order: 1,
            },
            ScheduledTick {
                pos: (1, 64, 0),
                block: 2,
                due: 7,
                priority: -3,
                sub_order: 0,
            },
            // Chunk (1,0): a NORMAL head between them in priority.
            ScheduledTick {
                pos: (16, 64, 0),
                block: 3,
                due: 6,
                priority: 0,
                sub_order: 2,
            },
        ] {
            assert!(queue.schedule(tick));
        }
        let out = queue.collect(10, 10, |_| true);
        assert_eq!(
            out.iter().map(|t| (t.pos, t.priority)).collect::<Vec<_>>(),
            vec![((16, 64, 0), 0), ((0, 64, 0), 1), ((1, 64, 0), -3)],
            "chunk (0,0) must keep its own trigger order under the merge"
        );

        // Due gating is per chunk head: a future-due chunk is skipped whole;
        // once due, its EXTREMELY_HIGH head outranks a lower-priority
        // competitor regardless of the competitor's earlier triggerTick.
        let mut queue = ScheduledTicks::new();
        for tick in [
            ScheduledTick {
                pos: (0, 64, 0),
                block: 1,
                due: 9,
                priority: -3,
                sub_order: 0,
            },
            ScheduledTick {
                pos: (16, 64, 0),
                block: 2,
                due: 5,
                priority: 3,
                sub_order: 1,
            },
        ] {
            assert!(queue.schedule(tick));
        }
        assert_eq!(
            queue.collect(9, 10, |_| true).iter().map(|t| t.pos).collect::<Vec<_>>(),
            vec![(0, 64, 0), (16, 64, 0)],
            "cross-chunk the due EH head must precede the due EL head"
        );
        // The same pair, collected before the EH tick comes due: only the
        // due tick runs, and the retained future tick is neither dropped nor
        // pulled forward.
        let mut queue = ScheduledTicks::new();
        for tick in [
            ScheduledTick {
                pos: (0, 64, 0),
                block: 1,
                due: 9,
                priority: -3,
                sub_order: 0,
            },
            ScheduledTick {
                pos: (16, 64, 0),
                block: 2,
                due: 5,
                priority: 3,
                sub_order: 1,
            },
        ] {
            assert!(queue.schedule(tick));
        }
        assert_eq!(
            queue.collect(7, 10, |_| true).iter().map(|t| t.pos).collect::<Vec<_>>(),
            vec![(16, 64, 0)],
            "the due<=7 pass must skip the future chunk head entirely"
        );
        assert_eq!(
            queue.collect(9, 10, |_| true).iter().map(|t| t.pos).collect::<Vec<_>>(),
            vec![(0, 64, 0)]
        );
    }

    /// `tickCheck` gates collection per game tick: an ineligible chunk is
    /// skipped without being dropped or rescheduled, keeps its original
    /// triggerTick, and drains in its original order the first tick it
    /// becomes eligible — merging against whatever came due in the meantime.
    #[test]
    fn ineligible_chunks_retain_and_deliver_on_eligibility() {
        let mut queue = ScheduledTicks::new();
        for (pos, sub_order) in [
            ((0, 64, 0), 0i64),
            ((1, 64, 0), 1),
            ((2, 64, 0), 2),
            ((16, 64, 0), 3),
            ((17, 64, 0), 4),
        ] {
            assert!(queue.schedule(ScheduledTick {
                pos,
                block: 1,
                due: 10,
                priority: 0,
                sub_order,
            }));
        }
        let cold = HashSet::from([(0, 0)]);
        let out = queue.collect(10, 10, |c| !cold.contains(&c));
        assert_eq!(
            out.iter().map(|t| t.sub_order).collect::<Vec<_>>(),
            vec![3, 4],
            "only the eligible chunk may drain"
        );
        // Repeated ineligible collects do not drop or reschedule the rest.
        assert!(queue.collect(11, 10, |c| !cold.contains(&c)).is_empty());
        assert!(queue.has_pending_matching(|t| t.sub_order <= 2));
        let out = queue.collect(12, 10, |_| true);
        assert_eq!(
            out.iter().map(|t| (t.due, t.sub_order)).collect::<Vec<_>>(),
            vec![(10, 0), (10, 1), (10, 2)],
            "the retained chunk drains in order with its original dues"
        );
        assert!(!queue.has_pending_matching(|_| true));

        // A chunk that becomes eligible merges against ticks that arrived
        // while it was cold: sub-order still decides, not arrival time.
        let mut queue = ScheduledTicks::new();
        for (pos, sub_order) in [((0, 64, 0), 0i64), ((16, 64, 0), 1), ((32, 64, 0), 2)] {
            assert!(queue.schedule(ScheduledTick {
                pos,
                block: 1,
                due: 10,
                priority: 0,
                sub_order,
            }));
        }
        let cold = HashSet::from([(0, 0)]);
        assert_eq!(queue.collect(11, 10, |c| !cold.contains(&c)).len(), 2);
        assert!(queue.schedule(ScheduledTick {
            pos: (33, 64, 0),
            block: 1,
            due: 12,
            priority: 0,
            sub_order: 3,
        }));
        let out = queue.collect(12, 10, |_| true);
        assert_eq!(
            out.iter().map(|t| t.sub_order).collect::<Vec<_>>(),
            vec![0, 3],
            "the held-back chunk's overdue tick still leads by sub-order"
        );
    }

    /// The per-tick callback bound (vanilla's 65,536) truncates the drain
    /// mid-merge; nothing past the bound is dropped or reordered — the next
    /// collect resumes exactly where the bound cut, including inside a
    /// chunk, and a zero bound delivers nothing while losing nothing.
    #[test]
    fn per_tick_cap_spills_over_in_exact_order() {
        // One chunk, one wave: the bound cuts inside the chunk.
        let mut queue = ScheduledTicks::new();
        for sub_order in 0..8i64 {
            assert!(queue.schedule(ScheduledTick {
                pos: (sub_order as i32, 64, 0),
                block: 1,
                due: 10,
                priority: 0,
                sub_order,
            }));
        }
        let first = queue.collect(10, 3, |_| true);
        assert_eq!(
            first.iter().map(|t| t.sub_order).collect::<Vec<_>>(),
            vec![0, 1, 2]
        );
        let second = queue.collect(10, 3, |_| true);
        assert_eq!(
            second.iter().map(|t| t.sub_order).collect::<Vec<_>>(),
            vec![3, 4, 5],
            "spillover must resume the same chunk at the cut point"
        );
        let rest = queue.collect(11, 10, |_| true);
        assert_eq!(
            rest.iter().map(|t| t.sub_order).collect::<Vec<_>>(),
            vec![6, 7]
        );

        // Across chunks the bound cuts mid-merge; limit zero loses nothing.
        let mut queue = ScheduledTicks::new();
        for (x, sub_order) in [(0i32, 0i64), (17, 1), (2, 2), (19, 3), (4, 4)] {
            assert!(queue.schedule(ScheduledTick {
                pos: (x, 64, 0),
                block: 1,
                due: 10,
                priority: 0,
                sub_order,
            }));
        }
        assert!(queue.collect(10, 0, |_| true).is_empty());
        assert!(queue.has_pending_matching(|_| true));
        let first = queue.collect(10, 3, |_| true);
        assert_eq!(
            first.iter().map(|t| t.sub_order).collect::<Vec<_>>(),
            vec![0, 1, 2]
        );
        let second = queue.collect(11, 10, |_| true);
        assert_eq!(
            second.iter().map(|t| t.sub_order).collect::<Vec<_>>(),
            vec![3, 4]
        );
        assert!(!queue.has_pending_matching(|_| true));
    }

    /// Save → decode → unpack at the load game time keeps every tick's
    /// remaining delay, priority and position in the drain: the reloaded
    /// queue delivers the identical (pos, block, due, priority) stream as
    /// the uninterrupted original. Native restart replaces sub-orders per
    /// chunk, so the workload keeps every (due, priority) pair unique to
    /// pin exactly one stream.
    #[test]
    fn save_reload_preserves_relative_delays_and_merge_order() {
        let spec: [(Pos, i64, i8); 7] = [
            ((0, 64, 0), 10, 0),
            ((1, 64, 0), 11, -3),
            ((16, 64, 0), 10, -1),
            ((17, 64, 0), 12, 0),
            ((32, 64, 0), 11, 1),
            ((33, 64, 0), 10, 3),
            ((0, 64, 15), 13, -2),
        ];
        let mut original = ScheduledTicks::new();
        for (i, &(pos, due, priority)) in spec.iter().enumerate() {
            assert!(original.schedule(ScheduledTick {
                pos,
                block: 1 + i as u32,
                due,
                priority,
                sub_order: i as i64,
            }));
        }
        // Deliver part of wave 10, then snapshot the queue mid-flight.
        assert_eq!(original.collect(10, 2, |_| true).len(), 2);
        let mut bytes = Vec::new();
        original.encode(&mut bytes);
        let mut input = &bytes[..];
        let mut loaded = ScheduledTicks::decode(&mut input)
            .expect("encode output decodes")
            .unpack_saved(10);
        assert!(input.is_empty());
        let render = |ticks: Vec<ScheduledTick>| {
            ticks
                .into_iter()
                .map(|t| (t.pos, t.block, t.due, t.priority))
                .collect::<Vec<_>>()
        };
        for now in 10..=14 {
            assert_eq!(
                render(original.collect(now, 3, |_| true)),
                render(loaded.collect(now, 3, |_| true)),
                "reloaded queue diverged at game time {now}"
            );
        }
        assert!(!original.has_pending_matching(|_| true));
        assert!(!loaded.has_pending_matching(|_| true));
    }

    /// Block and fluid ticks live in separate `LevelTicks` namespaces: the
    /// same (pos, id) can pend in both, identity suppression never crosses
    /// queues, and each drains by its own cap, eligibility and merge order.
    #[test]
    fn block_and_fluid_queues_are_independent_namespaces() {
        let mut blocks = ScheduledTicks::new();
        let mut fluids = ScheduledTicks::new();
        let shared = ScheduledTick {
            pos: (1, 64, 1),
            block: 7,
            due: 10,
            priority: 0,
            sub_order: 0,
        };
        assert!(blocks.schedule(shared));
        assert!(
            fluids.schedule(shared),
            "the fluid namespace must not be suppressed by the block queue"
        );
        // A refused duplicate in one queue does not disturb the other.
        assert!(!blocks.schedule(ScheduledTick {
            due: 12,
            sub_order: 9,
            ..shared
        }));
        assert!(fluids.schedule(ScheduledTick {
            pos: (5, 64, 1),
            block: 9,
            due: 10,
            priority: -3,
            sub_order: 1,
        }));
        assert!(blocks.schedule(ScheduledTick {
            pos: (2, 64, 1),
            block: 8,
            due: 10,
            priority: 0,
            sub_order: 2,
        }));
        // Independent caps and merge orders: the fluid queue drains its own
        // HIGH head first; the block queue's cap holds back only its tail.
        assert_eq!(
            fluids.collect(10, 10, |_| true).iter().map(|t| t.pos).collect::<Vec<_>>(),
            vec![(5, 64, 1), (1, 64, 1)]
        );
        assert_eq!(blocks.collect(10, 1, |_| true), vec![shared]);
        assert_eq!(
            blocks.collect(10, 10, |_| true).iter().map(|t| t.pos).collect::<Vec<_>>(),
            vec![(2, 64, 1)]
        );
        // Delivery released only the fluid identity; the block queue's copy
        // was already re-collected above, so both namespaces are free to
        // schedule the identity again.
        assert!(fluids.schedule(ScheduledTick {
            due: 20,
            ..shared
        }));
        assert!(!blocks.has_pending_matching(|_| true));
        assert_eq!(fluids.collect(20, 10, |_| true).len(), 1);
        // Independent eligibility: one queue held back, the other drained.
        for pos in [(30, 64, 0), (31, 64, 0)] {
            assert!(blocks.schedule(ScheduledTick {
                pos,
                block: 1,
                due: 30,
                priority: 0,
                sub_order: 10,
            }));
            assert!(fluids.schedule(ScheduledTick {
                pos,
                block: 2,
                due: 30,
                priority: 0,
                sub_order: 10,
            }));
        }
        let cold = HashSet::from([(1, 0)]);
        assert_eq!(blocks.collect(30, 10, |_| true).len(), 2);
        assert!(
            fluids.collect(30, 10, |c| !cold.contains(&c)).is_empty(),
            "the fluid queue holds its whole wave back"
        );
        assert_eq!(fluids.collect(31, 10, |_| true).len(), 2);
    }

    /// `hasScheduledTick` suppression ends at delivery: a delivered (pos,
    /// id) schedules again, while a still-pending duplicate keeps the
    /// original entry untouched.
    #[test]
    fn delivery_releases_the_identity_for_rescheduling() {
        let mut queue = ScheduledTicks::new();
        let first = ScheduledTick {
            pos: (1, 64, 1),
            block: 7,
            due: 10,
            priority: 0,
            sub_order: 0,
        };
        assert!(queue.schedule(first));
        assert!(!queue.schedule(ScheduledTick {
            due: 12,
            sub_order: 1,
            ..first
        }));
        assert_eq!(queue.collect(10, 10, |_| true), vec![first]);
        let second = ScheduledTick {
            due: 20,
            priority: -3,
            sub_order: 5,
            ..first
        };
        assert!(
            queue.schedule(second),
            "a delivered identity must schedule again"
        );
        assert_eq!(queue.collect(20, 10, |_| true), vec![second]);
        assert!(!queue.has_pending_matching(|_| true));
    }

    /// Cross-check the heap/table emulation against a naive reference merge
    /// on randomized workloads: globally unique sub-orders make the
    /// reference order fully determined, so any divergence in per-chunk
    /// trigger order, cross-chunk merge, cap spillover, deadline-table
    /// tracking or eligibility retention shows up as an exact diff.
    #[test]
    fn randomized_workload_matches_reference_merge() {
        struct Model {
            chunks: HashMap<ChunkPos, std::collections::VecDeque<ScheduledTick>>,
            waiting: HashSet<(Pos, u32)>,
        }
        impl Model {
            fn schedule(&mut self, tick: ScheduledTick) -> bool {
                if !self.waiting.insert((tick.pos, tick.block)) {
                    return false;
                }
                let heap = self
                    .chunks
                    .entry((tick.pos.0 >> 4, tick.pos.2 >> 4))
                    .or_default();
                let i = heap.partition_point(|x| full_order(x, &tick) == Ordering::Less);
                heap.insert(i, tick);
                true
            }
            fn collect(
                &mut self,
                now: i64,
                limit: usize,
                eligible: impl Fn(ChunkPos) -> bool,
            ) -> Vec<ScheduledTick> {
                let mut out = Vec::new();
                while out.len() < limit {
                    let mut best: Option<(ChunkPos, ScheduledTick)> = None;
                    for (&chunk, heap) in &self.chunks {
                        if !eligible(chunk) {
                            continue;
                        }
                        let Some(&head) = heap.front() else {
                            continue;
                        };
                        if head.due > now {
                            continue;
                        }
                        if best.is_none_or(|(_, b)| within_tick(&head, &b) == Ordering::Less) {
                            best = Some((chunk, head));
                        }
                    }
                    let Some((chunk, _)) = best else {
                        break;
                    };
                    let tick = self.chunks.get_mut(&chunk).unwrap().pop_front().unwrap();
                    self.waiting.remove(&(tick.pos, tick.block));
                    out.push(tick);
                }
                out
            }
            fn pending(&self) -> bool {
                self.chunks.values().any(|heap| !heap.is_empty())
            }
        }
        for seed in 0..24u64 {
            let mut state = seed
                .wrapping_mul(0x9E37_79B9_7F4A_7C15)
                .wrapping_add(0x85EB_CA6B_7855_51F5);
            let mut rng = || {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                state
            };
            let mut port = ScheduledTicks::new();
            let mut model = Model {
                chunks: HashMap::new(),
                waiting: HashSet::new(),
            };
            let mut now = 0i64;
            let mut order = 0i64;
            for _ in 0..300 {
                if rng() % 5 < 3 {
                    let tick = ScheduledTick {
                        pos: (
                            (rng() % 80) as i32 - 40,
                            (rng() % 8) as i32,
                            (rng() % 80) as i32 - 40,
                        ),
                        block: (rng() % 6) as u32,
                        due: now + (rng() % 9) as i64 - 2,
                        priority: (rng() % 7) as i8 - 3,
                        sub_order: order,
                    };
                    order += 1;
                    assert_eq!(
                        port.schedule(tick),
                        model.schedule(tick),
                        "dedup mismatch on {tick:?}"
                    );
                }
                now += (rng() % 3) as i64;
                let limit = (rng() % 9) as usize;
                let cold: HashSet<ChunkPos> = (-3..=2i32)
                    .flat_map(|x| (-3..=2i32).map(move |z| (x, z)))
                    .filter(|_| rng() % 10 < 3)
                    .collect();
                let a = port.collect(now, limit, |c| !cold.contains(&c));
                let b = model.collect(now, limit, |c| !cold.contains(&c));
                assert!(a.iter().all(|t| t.due <= now), "early callback");
                assert_eq!(a, b, "drain mismatch at now {now} limit {limit}");
            }
            let a = port.collect(i64::MAX / 4, usize::MAX, |_| true);
            let b = model.collect(i64::MAX / 4, usize::MAX, |_| true);
            assert_eq!(a, b, "final drain mismatch");
            assert_eq!(port.has_pending_matching(|_| true), model.pending());
        }
    }
}

#[cfg(test)]
pub(crate) mod delivery_fixture {
    use super::*;

    fn ticks(text: &str) -> impl Iterator<Item = ScheduledTick> + '_ {
        text.split(';').filter(|&v| v != "-").map(|entry| {
            let fields: Vec<_> = entry.split(',').collect();
            let [block, x, y, z, due, priority, sub_order] = fields.as_slice() else {
                panic!("invalid tick: {entry}");
            };
            ScheduledTick {
                pos: (x.parse().unwrap(), y.parse().unwrap(), z.parse().unwrap()),
                block: block.parse().unwrap(),
                due: due.parse().unwrap(),
                priority: match *priority {
                    "EXTREMELY_HIGH" => -3,
                    "VERY_HIGH" => -2,
                    "HIGH" => -1,
                    "NORMAL" => 0,
                    "LOW" => 1,
                    "VERY_LOW" => 2,
                    "EXTREMELY_LOW" => 3,
                    _ => panic!("invalid priority: {priority}"),
                },
                sub_order: sub_order.parse().unwrap(),
            }
        })
    }

    fn queue<'a>(
        phase: &str,
        blocks: &'a mut ScheduledTicks,
        fluids: &'a mut ScheduledTicks,
    ) -> &'a mut ScheduledTicks {
        match phase {
            "B" => blocks,
            "F" => fluids,
            _ => panic!("invalid phase: {phase}"),
        }
    }

    pub(crate) fn replay(
        with_reload: bool,
        mut reload: impl FnMut(i64, &mut ScheduledTicks, &mut ScheduledTicks),
    ) -> usize {
        let fixture = include_str!("../tests/data/scheduled_tick_delivery_1211.tsv");
        let mut cases = 0;
        for scenario in fixture.strip_prefix("case\t").unwrap().split("\ncase\t") {
            let (name, operations) = scenario.split_once('\n').unwrap();
            if operations.lines().any(|line| line.starts_with("reload\t")) != with_reload {
                continue;
            }
            cases += 1;
            let mut blocks = ScheduledTicks::new();
            let mut fluids = ScheduledTicks::new();
            for operation in operations.lines() {
                let fields: Vec<_> = operation.split('\t').collect();
                match fields.as_slice() {
                    ["unpack", phase, now, input] => {
                        let generated = ticks(input)
                            .map(|t| {
                                assert_eq!((t.due, t.priority), (0, 0));
                                (t.pos, t.block)
                            })
                            .collect();
                        queue(phase, &mut blocks, &mut fluids)
                            .unpack_generated(generated, now.parse().unwrap());
                    }
                    ["schedule", phase, input] => {
                        let queue = queue(phase, &mut blocks, &mut fluids);
                        for tick in ticks(input) {
                            assert!(queue.schedule(tick), "{name}: {tick:?}");
                        }
                    }
                    ["reload", now] => reload(now.parse().unwrap(), &mut blocks, &mut fluids),
                    ["tick", now, limit, cold, expected_blocks, expected_fluids] => {
                        let now = now.parse().unwrap();
                        let limit = limit.parse().unwrap();
                        let cold: HashSet<_> = cold
                            .split(';')
                            .filter(|v| !v.is_empty())
                            .map(|v| {
                                let (x, z) = v.split_once(',').unwrap();
                                (x.parse().unwrap(), z.parse().unwrap())
                            })
                            .collect();
                        for (phase, queue, expected) in [
                            ("block", &mut blocks, *expected_blocks),
                            ("fluid", &mut fluids, *expected_fluids),
                        ] {
                            assert_eq!(
                                queue.collect(now, limit, |chunk| !cold.contains(&chunk)),
                                ticks(expected).collect::<Vec<_>>(),
                                "{name}, {phase} callbacks at game tick {now}",
                            );
                        }
                    }
                    _ => panic!("invalid operation: {operation}"),
                }
            }
            assert!(
                !blocks.has_pending_matching(|_| true),
                "{name}: extra blocks"
            );
            assert!(
                !fluids.has_pending_matching(|_| true),
                "{name}: extra fluids"
            );
        }
        cases
    }
}

#[derive(Clone)]
struct ChunkTicks {
    heap: Vec<ScheduledTick>,
}

#[derive(Clone)]
pub(crate) struct ScheduledTicks {
    chunks: HashMap<ChunkPos, ChunkTicks>,
    waiting: HashSet<(Pos, u32)>,
    next: NextChunks,
}

impl ScheduledTicks {
    pub(crate) fn new() -> Self {
        Self {
            chunks: HashMap::new(),
            waiting: HashSet::new(),
            next: NextChunks::new(),
        }
    }

    /// Test-only inspection of queued identities; the production scheduler
    /// exposes collections only when their eligible game tick is due.
    #[cfg(test)]
    pub(crate) fn has_pending_matching(
        &self,
        pred: impl Fn(&ScheduledTick) -> bool,
    ) -> bool {
        self.chunks.values().any(|chunk| chunk.heap.iter().any(&pred))
    }

    /// Generator queues represent zero-delay, normal-priority proto ticks.
    /// Native unpack starts each chunk at minus its own pending-list length;
    /// ordinary runtime schedules retain their separate nonnegative counter.
    pub(crate) fn unpack_generated(&mut self, mut pending: Vec<(Pos, u32)>, now: i64) {
        // ProtoChunkTicks deduplicates before LevelChunkTicks uses the list
        // length to assign sub-orders.
        let mut seen = HashSet::new();
        pending.retain(|entry| seen.insert(*entry));
        let mut counts: HashMap<ChunkPos, i64> = HashMap::new();
        for (pos, _) in &pending {
            *counts.entry((pos.0 >> 4, pos.2 >> 4)).or_default() += 1;
        }
        for (pos, block) in pending {
            let remaining = counts.get_mut(&(pos.0 >> 4, pos.2 >> 4)).unwrap();
            self.schedule(ScheduledTick {
                pos,
                block,
                due: now,
                priority: 0,
                sub_order: -*remaining,
            });
            *remaining -= 1;
        }
    }

    /// Save lists follow the heap's iteration order. Native unpack discards
    /// the old sub-orders and inflates the saved delays at the load game time.
    pub(crate) fn unpack_saved(self, now: i64) -> Self {
        let mut chunks: Vec<_> = self.chunks.into_iter().collect();
        chunks.sort_unstable_by_key(|(chunk, _)| *chunk);
        let mut loaded = Self::new();
        for (_, chunk) in chunks {
            let start = -(chunk.heap.len() as i64);
            for (index, mut tick) in chunk.heap.into_iter().enumerate() {
                let delay = tick.due.wrapping_sub(now) as i32;
                tick.due = now.wrapping_add(i64::from(delay));
                tick.sub_order = start + index as i64;
                loaded.schedule(tick);
            }
        }
        loaded
    }

    // Keep the native heap and fastutil table layout: reconstructing only the
    // set of entries can change the order of equal-priority chunk ticks.
    pub(crate) fn encode(&self, out: &mut Vec<u8>) {
        let mut chunks: Vec<_> = self
            .chunks
            .iter()
            .filter(|(_, c)| !c.heap.is_empty())
            .collect();
        chunks.sort_by_key(|(p, _)| **p);
        out.extend_from_slice(&(chunks.len() as u32).to_be_bytes());
        for (&(x, z), chunk) in chunks {
            out.extend_from_slice(&x.to_be_bytes());
            out.extend_from_slice(&z.to_be_bytes());
            out.extend_from_slice(&(chunk.heap.len() as u32).to_be_bytes());
            for t in &chunk.heap {
                for n in [t.pos.0, t.pos.1, t.pos.2] {
                    out.extend_from_slice(&n.to_be_bytes());
                }
                out.extend_from_slice(&t.block.to_be_bytes());
                out.extend_from_slice(&t.due.to_be_bytes());
                out.push(t.priority as u8);
                out.extend_from_slice(&t.sub_order.to_be_bytes());
            }
        }
        out.extend_from_slice(&(self.next.slots.len() as u32).to_be_bytes());
        for slot in &self.next.slots {
            out.push(u8::from(slot.is_some()));
            if let Some((key, due)) = slot {
                out.extend_from_slice(&key.to_be_bytes());
                out.extend_from_slice(&due.to_be_bytes());
            }
        }
        out.push(u8::from(self.next.zero.is_some()));
        if let Some(due) = self.next.zero {
            out.extend_from_slice(&due.to_be_bytes());
        }
    }

    pub(crate) fn decode(input: &mut &[u8]) -> Result<Self, &'static str> {
        fn take<const N: usize>(input: &mut &[u8]) -> Result<[u8; N], &'static str> {
            let (value, rest) = input.split_at_checked(N).ok_or("truncated tick state")?;
            *input = rest;
            Ok(value.try_into().unwrap())
        }
        let mut result = Self::new();
        let count = u32::from_be_bytes(take(input)?) as usize;
        if count > input.len() / 12 {
            return Err("invalid tick chunk count");
        }
        for _ in 0..count {
            let x = i32::from_be_bytes(take(input)?);
            let z = i32::from_be_bytes(take(input)?);
            let n = u32::from_be_bytes(take(input)?) as usize;
            if n == 0 || n > input.len() / 33 {
                return Err("invalid tick count");
            }
            let mut heap = Vec::with_capacity(n);
            for _ in 0..n {
                let t = ScheduledTick {
                    pos: (
                        i32::from_be_bytes(take(input)?),
                        i32::from_be_bytes(take(input)?),
                        i32::from_be_bytes(take(input)?),
                    ),
                    block: u32::from_be_bytes(take(input)?),
                    due: i64::from_be_bytes(take(input)?),
                    priority: i8::from_be_bytes(take(input)?),
                    sub_order: i64::from_be_bytes(take(input)?),
                };
                if (t.pos.0 >> 4, t.pos.2 >> 4) != (x, z)
                    || !result.waiting.insert((t.pos, t.block))
                {
                    return Err("invalid or duplicate tick");
                }
                heap.push(t);
            }
            if heap
                .iter()
                .enumerate()
                .skip(1)
                .any(|(i, t)| full_order(t, &heap[(i - 1) / 2]) == Ordering::Less)
                || result.chunks.insert((x, z), ChunkTicks { heap }).is_some()
            {
                return Err("invalid tick heap");
            }
        }
        let size = u32::from_be_bytes(take(input)?) as usize;
        if size < 32 || !size.is_power_of_two() || size > input.len() {
            return Err("invalid tick table size");
        }
        let mut slots = Vec::with_capacity(size);
        for _ in 0..size {
            slots.push(match take::<1>(input)?[0] {
                0 => None,
                1 => Some((
                    u64::from_be_bytes(take(input)?),
                    i64::from_be_bytes(take(input)?),
                )),
                _ => return Err("invalid tick slot"),
            });
        }
        let zero = match take::<1>(input)?[0] {
            0 => None,
            1 => Some(i64::from_be_bytes(take(input)?)),
            _ => return Err("invalid zero tick slot"),
        };
        let len = slots.iter().flatten().count() + usize::from(zero.is_some());
        if len != result.chunks.len() || len > size * 3 / 4 {
            return Err("invalid tick table entries");
        }
        result.next = NextChunks { slots, zero, len };
        for (&chunk, ticks) in &result.chunks {
            let key = chunk_key(chunk);
            let due = if key == 0 {
                result.next.zero
            } else {
                result.next.slots[result.next.slot(key)]
                    .filter(|v| v.0 == key)
                    .map(|v| v.1)
            };
            if due != Some(ticks.heap[0].due) {
                return Err("tick table does not match heap");
            }
        }
        Ok(result)
    }

    pub(crate) fn schedule(&mut self, tick: ScheduledTick) -> bool {
        if !self.waiting.insert((tick.pos, tick.block)) {
            return false;
        }
        let chunk = (tick.pos.0 >> 4, tick.pos.2 >> 4);
        let queue = self
            .chunks
            .entry(chunk)
            .or_insert_with(|| ChunkTicks { heap: Vec::new() });
        let previous = queue.heap.first().copied();
        heap_push(&mut queue.heap, tick, full_order);
        if previous != queue.heap.first().copied() {
            self.next.put(chunk_key(chunk), queue.heap[0].due);
        }
        true
    }

    /// Collects before running callbacks; newly scheduled ticks cannot join the
    /// current collection. Ineligible chunks keep their original deadlines.
    pub(crate) fn collect(
        &mut self,
        now: i64,
        limit: usize,
        eligible: impl Fn(ChunkPos) -> bool,
    ) -> Vec<ScheduledTick> {
        // `next` holds exactly one deadline per chunk that still has a pending
        // tick, so an empty table means every heap is drained: answer in O(1)
        // without touching the allocator, the container heap, or the sort.
        if self.next.len == 0 {
            debug_assert!(self.chunks.values().all(|chunk| chunk.heap.is_empty()));
            return Vec::new();
        }
        let mut containers =
            COLLECT_CONTAINERS.with(|cell| std::mem::take(&mut *cell.borrow_mut()));
        containers.clear();
        self.next.take_ready(|key, due| {
            let chunk = chunk_from_key(key);
            if due > now || !eligible(chunk) {
                return false;
            }
            heap_push(&mut containers, chunk, |a, b| {
                within_tick(&self.chunks[a].heap[0], &self.chunks[b].heap[0])
            });
            true
        });
        // Every popped container yields at least its head tick before the
        // loop can stop, so this is a lower bound on the final length — the
        // reserve never over-allocates past what the drain is guaranteed to
        // return, and the returned `Vec` still owns its own buffer.
        let mut result = Vec::with_capacity(containers.len().min(limit));
        while result.len() < limit && !containers.is_empty() {
            let chunk = heap_pop(&mut containers, |a, b| {
                within_tick(&self.chunks[a].heap[0], &self.chunks[b].heap[0])
            })
            .unwrap();
            let first =
                heap_pop(&mut self.chunks.get_mut(&chunk).unwrap().heap, full_order).unwrap();
            self.waiting.remove(&(first.pos, first.block));
            result.push(first);
            let other = containers.first().map(|c| self.chunks[c].heap[0]);
            while result.len() < limit {
                let Some(tick) = self.chunks[&chunk].heap.first().copied() else {
                    break;
                };
                if tick.due > now
                    || other.is_some_and(|x| within_tick(&tick, &x) == Ordering::Greater)
                {
                    break;
                }
                let tick =
                    heap_pop(&mut self.chunks.get_mut(&chunk).unwrap().heap, full_order).unwrap();
                self.waiting.remove(&(tick.pos, tick.block));
                result.push(tick);
            }
            if let Some(tick) = self.chunks[&chunk].heap.first().copied() {
                if tick.due <= now && result.len() < limit {
                    heap_push(&mut containers, chunk, |a, b| {
                        within_tick(&self.chunks[a].heap[0], &self.chunks[b].heap[0])
                    });
                } else {
                    self.next.put(chunk_key(chunk), tick.due);
                }
            }
        }
        for chunk in containers.drain(..) {
            self.next
                .put(chunk_key(chunk), self.chunks[&chunk].heap[0].due);
        }
        bound_collect_scratch(&mut containers);
        COLLECT_CONTAINERS.with(|cell| *cell.borrow_mut() = containers);
        result
    }
}
