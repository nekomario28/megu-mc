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
            self.grow();
        }
    }

    fn grow(&mut self) {
        let new_len = self.slots.len() * 2;
        let old = std::mem::replace(&mut self.slots, vec![None; new_len]);
        // Fastutil rehash scans the previous table from high to low.
        for (key, due) in old.into_iter().rev().flatten() {
            let i = self.slot(key);
            self.slots[i] = Some((key, due));
        }
    }

    fn remove(&mut self, key: u64) {
        if key == 0 {
            if self.zero.take().is_some() {
                self.len -= 1;
            }
            return;
        }
        let i = self.slot(key);
        if self.slots[i].is_none() {
            return;
        }
        self.len -= 1;
        let mask = self.slots.len() - 1;
        let mut hole = i;
        let mut scan = (i + 1) & mask;
        while let Some((other, due)) = self.slots[scan] {
            let home = fastutil_mix64(other) as usize & mask;
            let move_here = if hole <= scan {
                hole >= home || home > scan
            } else {
                hole >= home && home > scan
            };
            if move_here {
                self.slots[hole] = Some((other, due));
                hole = scan;
            }
            scan = (scan + 1) & mask;
        }
        self.slots[hole] = None;
    }

    fn ordered(&self) -> Vec<(u64, i64)> {
        self.zero
            .map(|due| (0, due))
            .into_iter()
            .chain(self.slots.iter().rev().flatten().copied())
            .collect()
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
    pub(crate) fn unpack_generated(&mut self, pending: Vec<(Pos, u32)>, now: i64) {
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
        let mut containers = Vec::new();
        for (key, due) in self.next.ordered() {
            let chunk = chunk_from_key(key);
            if due > now || !eligible(chunk) {
                continue;
            }
            self.next.remove(key);
            heap_push(&mut containers, chunk, |a, b| {
                within_tick(&self.chunks[a].heap[0], &self.chunks[b].heap[0])
            });
        }
        let mut result = Vec::new();
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
        for chunk in containers {
            self.next
                .put(chunk_key(chunk), self.chunks[&chunk].heap[0].due);
        }
        result
    }
}
