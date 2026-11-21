// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! Chunky-style parallel pregen controller and the ticket-driven chunk LRU
//! unloader.
//!
//! [`PregenJob`] is the bounded work queue a live server ticks to pre-generate
//! terrain without starving players: it enumerates the target chunk set once
//! as a deterministic outward spiral from `(center_cx, center_cz)` — ring `0`
//! is the center, ring `k` the `8k` chunks at Chebyshev distance `k`, walked
//! clockwise from the northwest corner — and hands it out in
//! [`PregenJob::take_batch`] slices the caller completes at its own pace
//! (`mark_done`), so per-tick work stays capped and the loop can be
//! `pause()`d, `resume()`d or `cancel()`lled at any boundary. `Square` covers
//! exactly `(2r+1)^2` chunks; `Circle` the cells with `dx^2 + dz^2 <= r^2`.
//! Either way every chunk is emitted exactly once. The spiral is produced one
//! ring at a time, so an arbitrarily large radius never materializes more
//! than `8r` pending offsets.
//!
//! [`ChunkTicketLruUnloader`] is the other half of bounded memory (closes the
//! `#105` resident-chunk growth): chunks are tracked by `(dimension, cx, cz)`
//! with reference-counted [`TicketKind`]s — player view distance, OpenPAC
//! forceloads, spawn chunks and temporary load tickets — and a
//! `last_used_tick` per chunk. [`ChunkTicketLruUnloader::collect_evictable`]
//! never touches a chunk holding even one active ticket and evicts unticketed
//! chunks older than `grace_ticks` in strict LRU order (oldest
//! `last_used_tick` first, ties broken deterministically by
//! `(dimension, cx, cz)`), just enough to keep the unticketed resident set at
//! `max_resident_unloaded`. [`ChunkTicketLruUnloader::evict_from_context`]
//! does the same against a live [`ServerContext`] and returns the removed
//! [`FlushedChunk`] overlays, so callers can persist them — an eviction never
//! silently drops an edit.

use crate::sharded::{ChunkCoord, ChunkOverlay, ServerContext};
use std::collections::HashMap;

/// The footprint a [`PregenJob`] covers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PregenShape {
    /// Every chunk with `max(|dx|, |dz|) <= radius`: `(2r+1)^2` chunks.
    Square,
    /// Every chunk offset with `dx^2 + dz^2 <= radius^2`.
    Circle,
}

/// Lifecycle of a [`PregenJob`]. `Completed` and `Cancelled` are terminal;
/// `Paused` resumes to `Running`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PregenState {
    Running,
    Paused,
    Completed,
    Cancelled,
}

/// Point-in-time job counters from [`PregenJob::progress`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PregenProgress {
    /// Chunks confirmed done via [`PregenJob::mark_done`].
    pub done: u64,
    /// Total chunks the spiral will emit.
    pub total: u64,
    /// `done / total * 100` (`100.0` when `total == 0`).
    pub percent: f64,
    pub state: PregenState,
}

/// The cells of spiral ring `k` (`k >= 1`): every offset with
/// `max(|dx|, |dz|) == k`, walked clockwise starting at the northwest corner
/// `(-k, -k)` — east along the north edge, south along the east edge, west
/// along the south edge, north along the west edge. `8k` cells, no corner is
/// visited twice.
fn ring_cells(k: i64) -> Vec<(i64, i64)> {
    let mut cells = Vec::with_capacity((8 * k) as usize);
    for dx in -k..=k {
        cells.push((dx, -k));
    }
    for dz in (-k + 1)..=k {
        cells.push((k, dz));
    }
    for dx in (-k..k).rev() {
        cells.push((dx, k));
    }
    for dz in (-k + 1..k).rev() {
        cells.push((-k, dz));
    }
    cells
}

/// Whether offset `(dx, dz)` is inside `shape` at `radius`. i128 keeps the
/// squares exact: `radius_chunks` is a `u32`, so offsets near `u32::MAX`
/// would overflow i64.
fn in_shape(dx: i64, dz: i64, radius: i64, shape: PregenShape) -> bool {
    match shape {
        PregenShape::Square => true,
        PregenShape::Circle => {
            let (dx, dz, r) = (i128::from(dx), i128::from(dz), i128::from(radius));
            dx * dx + dz * dz <= r * r
        }
    }
}

/// The full deterministic spiral order for `shape` at `radius`, center
/// `(center_cx, center_cz)` — what [`PregenJob`] emits across all
/// `take_batch` calls, materialized at once. Intended for checks and small
/// radii; the job itself never holds more than one ring.
pub fn spiral_order(
    center_cx: i32,
    center_cz: i32,
    radius_chunks: u32,
    shape: PregenShape,
) -> Vec<(i32, i32)> {
    let radius = radius_chunks as i64;
    let mut out = Vec::new();
    if in_shape(0, 0, radius, shape) {
        out.push((center_cx, center_cz));
    }
    for k in 1..=radius {
        for (dx, dz) in ring_cells(k) {
            if in_shape(dx, dz, radius, shape) {
                out.push((
                    center_cx.wrapping_add(dx as i32),
                    center_cz.wrapping_add(dz as i32),
                ));
            }
        }
    }
    out
}

/// Chunks inside a `Circle` of `radius`: `sum over dx of 2*floor(sqrt(r^2 -
/// dx^2)) + 1`, counted exactly with integer square roots.
fn circle_total(radius: u32) -> u64 {
    let r = u128::from(radius);
    let r2 = r * r;
    let mut total = 0u128;
    for dx in 0..=r {
        let dz_max = (r2 - dx * dx).isqrt();
        let columns = 2 * dz_max + 1;
        total += if dx == 0 { columns } else { 2 * columns };
    }
    // A circle over the whole u32 chunk range is ~5.8e19 chunks, past u64;
    // saturate rather than overflow (such a job could never finish anyway).
    total.min(u128::from(u64::MAX)) as u64
}

/// A capped, pausable pregen run. Construct with
/// [`PregenJob::new`], then drive it once per tick: `take_batch` hands out the
/// next spiral slice, the caller generates those chunks, and `mark_done`
/// confirms them. `progress()` reports the running totals for a status line.
pub struct PregenJob {
    center: (i32, i32),
    radius: i64,
    shape: PregenShape,
    state: PregenState,
    total: u64,
    /// Emitted to callers but not yet `mark_done`-confirmed.
    issued: u64,
    done: u64,
    /// Spiral cursor: the ring currently being handed out, the next ring to
    /// load once it is exhausted, and the position inside the ring.
    ring: Vec<(i64, i64)>,
    ring_pos: usize,
    next_ring: i64,
}

impl PregenJob {
    /// A job spiralling out from `(center_cx, center_cz)` over `radius_chunks`
    /// in `shape`. The total is counted analytically, so even a huge radius
    /// constructs in `O(r)` arithmetic and `O(1)` memory.
    pub fn new(center_cx: i32, center_cz: i32, radius_chunks: u32, shape: PregenShape) -> Self {
        let total = match shape {
            PregenShape::Square => {
                // `(2r+1)^2` overflows u64 for radius_chunks near u32::MAX;
                // saturate — no real plan reaches that size.
                let side = u128::from(radius_chunks) * 2 + 1;
                (side * side).min(u128::from(u64::MAX)) as u64
            }
            PregenShape::Circle => circle_total(radius_chunks),
        };
        Self {
            center: (center_cx, center_cz),
            radius: radius_chunks as i64,
            shape,
            state: PregenState::Running,
            total,
            issued: 0,
            done: 0,
            ring: Vec::new(),
            ring_pos: 0,
            next_ring: 0,
        }
    }

    pub fn center(&self) -> (i32, i32) {
        self.center
    }

    pub fn radius(&self) -> u32 {
        self.radius as u32
    }

    pub fn shape(&self) -> PregenShape {
        self.shape
    }

    pub fn state(&self) -> PregenState {
        self.state
    }

    /// The next `max_chunks` absolute chunk coordinates in spiral order.
    /// Empty when the job is not `Running` or everything is already issued;
    /// an empty batch after `issued == total` just means the outstanding
    /// chunks are still awaiting `mark_done`.
    pub fn take_batch(&mut self, max_chunks: usize) -> Vec<(i32, i32)> {
        if self.state != PregenState::Running {
            return Vec::new();
        }
        let mut batch = Vec::with_capacity(max_chunks);
        while batch.len() < max_chunks {
            if self.ring_pos >= self.ring.len() {
                if self.next_ring > self.radius {
                    break;
                }
                self.ring = if self.next_ring == 0 {
                    vec![(0, 0)]
                } else {
                    ring_cells(self.next_ring)
                };
                self.next_ring += 1;
                self.ring_pos = 0;
                if self.shape == PregenShape::Circle {
                    let radius = self.radius;
                    self.ring
                        .retain(|&(dx, dz)| in_shape(dx, dz, radius, PregenShape::Circle));
                }
                continue;
            }
            let (dx, dz) = self.ring[self.ring_pos];
            self.ring_pos += 1;
            batch.push((
                self.center.0.wrapping_add(dx as i32),
                self.center.1.wrapping_add(dz as i32),
            ));
        }
        self.issued += batch.len() as u64;
        batch
    }

    /// Confirm `count` of the issued chunks as generated (clamped at the
    /// outstanding count). When `done` reaches `total` the job completes.
    pub fn mark_done(&mut self, count: usize) {
        let confirmed = (count as u64).min(self.issued.saturating_sub(self.done));
        self.done += confirmed;
        if self.done >= self.total
            && matches!(self.state, PregenState::Running | PregenState::Paused)
        {
            self.state = PregenState::Completed;
        }
    }

    /// Issued but not yet confirmed — the in-flight work a driver should keep
    /// below its per-tick budget.
    pub fn pending(&self) -> u64 {
        self.issued.saturating_sub(self.done)
    }

    pub fn progress(&self) -> PregenProgress {
        let percent = if self.total == 0 {
            100.0
        } else {
            self.done as f64 * 100.0 / self.total as f64
        };
        PregenProgress {
            done: self.done,
            total: self.total,
            percent,
            state: self.state,
        }
    }

    /// Freeze the spiral cursor exactly where it is; `take_batch` returns
    /// nothing while `Paused`. No-op unless `Running`.
    pub fn pause(&mut self) {
        if self.state == PregenState::Running {
            self.state = PregenState::Paused;
        }
    }

    /// Continue a `Paused` job from its exact cursor. No-op otherwise —
    /// a cancelled or completed job cannot be revived.
    pub fn resume(&mut self) {
        if self.state == PregenState::Paused {
            self.state = PregenState::Running;
        }
    }

    /// Abandon the job; unissued chunks are never emitted. No-op once
    /// `Completed` — finished work stays finished.
    pub fn cancel(&mut self) {
        if matches!(self.state, PregenState::Running | PregenState::Paused) {
            self.state = PregenState::Cancelled;
        }
    }

    /// One server tick of pregen: take at most `max_chunks` spiral chunks,
    /// materialize their (empty) overlays in `dimension` so the chunk set is
    /// resident and visible to the unloader, run `generate(cx, cz)` for each,
    /// and mark the batch done. Returns how many chunks were processed.
    pub fn drive_tick(
        &mut self,
        ctx: &ServerContext,
        dimension: &str,
        max_chunks: usize,
        mut generate: impl FnMut(i32, i32),
    ) -> usize {
        let batch = self.take_batch(max_chunks);
        if batch.is_empty() {
            return 0;
        }
        let dim = ctx.ensure_dimension(dimension);
        let coords: Vec<ChunkCoord> = batch
            .iter()
            .map(|&(cx, cz)| ChunkCoord::new(cx, cz))
            .collect();
        dim.with_chunks_write(&coords, |set| {
            for &coord in &coords {
                let _ = set.get_mut(coord);
            }
        });
        for &(cx, cz) in &batch {
            generate(cx, cz);
        }
        let n = batch.len();
        self.mark_done(n);
        n
    }
}

/// Why a chunk is pinned in memory. Temporary tickets expire at
/// `expire_tick`: `Temporary(t)` is active iff `current_tick < t`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TicketKind {
    /// Inside a connected player's view distance.
    PlayerView,
    /// Held by an explicit forceload (e.g. an OpenPAC claim).
    Forceload,
    /// Spawn chunk.
    Spawn,
    /// Short-lived load ticket that stops protecting at `expire_tick`.
    Temporary(u64),
}

impl TicketKind {
    /// Whether this ticket still protects its chunk at `current_tick`.
    fn is_active(self, current_tick: u64) -> bool {
        match self {
            TicketKind::Temporary(expire_tick) => current_tick < expire_tick,
            _ => true,
        }
    }
}

/// A resident chunk removed by [`ChunkTicketLruUnloader::evict_from_context`]:
/// the overlay leaves memory whole, edits included, for the caller to flush
/// to disk.
#[derive(Debug)]
pub struct FlushedChunk {
    pub dimension: String,
    pub coord: ChunkCoord,
    pub overlay: ChunkOverlay,
}

/// `(dimension, cx, cz)` — the map key everything here is indexed by.
type ChunkKey = (String, i32, i32);

/// Reference-counted chunk tickets plus per-chunk last-used ticks, driving
/// LRU eviction of unticketed residents.
///
/// `acquire` / `release` adjust the per-`(kind)` reference count (a chunk is
/// protected while any count is non-zero and its ticket is still active);
/// `touch` records the last-used tick. Both `acquire` and `release` also
/// stamp `last_used`, so a chunk that only ever carried tickets re-enters the
/// LRU at the moment it is released rather than becoming invisible.
#[derive(Default)]
pub struct ChunkTicketLruUnloader {
    /// `(dim, cx, cz) -> kind -> outstanding reference count`.
    tickets: HashMap<ChunkKey, HashMap<TicketKind, u32>>,
    /// `(dim, cx, cz) -> last-used game tick`.
    last_used: HashMap<ChunkKey, u64>,
}

impl ChunkTicketLruUnloader {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add one reference to `kind` on the chunk and stamp it used at `tick`.
    pub fn acquire(&mut self, dimension: &str, cx: i32, cz: i32, kind: TicketKind, tick: u64) {
        let key = Self::key(dimension, cx, cz);
        *self
            .tickets
            .entry(key.clone())
            .or_default()
            .entry(kind)
            .or_insert(0) += 1;
        self.last_used.insert(key, tick);
    }

    /// Drop one reference to `kind`. Returns `false` when the chunk held no
    /// such ticket. The chunk is still stamped used at `tick` — releasing is
    /// an access.
    pub fn release(
        &mut self,
        dimension: &str,
        cx: i32,
        cz: i32,
        kind: TicketKind,
        tick: u64,
    ) -> bool {
        let key = Self::key(dimension, cx, cz);
        self.last_used.insert(key.clone(), tick);
        let Some(kinds) = self.tickets.get_mut(&key) else {
            return false;
        };
        let Some(count) = kinds.get_mut(&kind) else {
            return false;
        };
        *count -= 1;
        if *count == 0 {
            kinds.remove(&kind);
        }
        if kinds.is_empty() {
            self.tickets.remove(&key);
        }
        true
    }

    /// Record that the chunk was used at `tick` (loads, block accesses,
    /// player presence — anything that proves residency is still useful).
    pub fn touch(&mut self, dimension: &str, cx: i32, cz: i32, tick: u64) {
        self.last_used.insert(Self::key(dimension, cx, cz), tick);
    }

    /// The chunk's last-used tick, if it was ever touched or ticketed.
    pub fn last_used_tick(&self, dimension: &str, cx: i32, cz: i32) -> Option<u64> {
        self.last_used.get(&Self::key(dimension, cx, cz)).copied()
    }

    /// Outstanding active-ticket references on the chunk at `current_tick`
    /// (expired temporaries do not count).
    pub fn active_tickets(&self, dimension: &str, cx: i32, cz: i32, current_tick: u64) -> usize {
        self.tickets
            .get(&Self::key(dimension, cx, cz))
            .map(|kinds| {
                kinds
                    .iter()
                    .filter(|(kind, &count)| count > 0 && kind.is_active(current_tick))
                    .map(|(_, &count)| count as usize)
                    .sum()
            })
            .unwrap_or(0)
    }

    /// Whether the chunk holds at least one active ticket — i.e. must never
    /// appear in an eviction list.
    pub fn is_protected(&self, dimension: &str, cx: i32, cz: i32, current_tick: u64) -> bool {
        self.active_tickets(dimension, cx, cz, current_tick) > 0
    }

    /// Number of distinct `(dim, cx, cz)` keys the unloader tracks.
    pub fn tracked_count(&self) -> usize {
        let mut keys: Vec<&ChunkKey> = self.last_used.keys().collect();
        keys.extend(self.tickets.keys());
        keys.sort();
        keys.dedup();
        keys.len()
    }

    /// The unticketed, grace-expired tracked chunks to evict, oldest
    /// `last_used_tick` first (ties by `(dim, cx, cz)`), such that after
    /// eviction at most `max_resident_unloaded` unticketed chunks remain.
    /// Chunks with any active ticket are never listed, and chunks touched
    /// within `grace_ticks` of `current_tick` survive regardless of the cap.
    pub fn collect_evictable(
        &self,
        current_tick: u64,
        max_resident_unloaded: usize,
        grace_ticks: u64,
    ) -> Vec<(String, i32, i32)> {
        let mut unticketed = 0usize;
        let mut candidates: Vec<(&ChunkKey, u64)> = Vec::new();
        for (key, &used) in &self.last_used {
            if self.is_protected(&key.0, key.1, key.2, current_tick) {
                continue;
            }
            unticketed += 1;
            if current_tick.saturating_sub(used) >= grace_ticks {
                candidates.push((key, used));
            }
        }
        let over = unticketed.saturating_sub(max_resident_unloaded);
        candidates.sort_by(|a, b| a.1.cmp(&b.1).then_with(|| a.0.cmp(b.0)));
        candidates.truncate(over);
        candidates.into_iter().map(|(key, _)| key.clone()).collect()
    }

    /// `collect_evictable` applied to a live [`ServerContext`]: every
    /// resident overlay is first registered (a chunk the unloader has never
    /// seen is stamped at `current_tick`, so it still gets `grace_ticks`
    /// before eviction), expired temporary tickets are dropped, then victims
    /// are removed from their shards in LRU order. Returns the flushed
    /// overlays — nothing evicted is lost.
    pub fn evict_from_context(
        &mut self,
        ctx: &ServerContext,
        current_tick: u64,
        max_resident_unloaded: usize,
        grace_ticks: u64,
    ) -> Vec<FlushedChunk> {
        for (name, dimension) in ctx.dimensions_sorted() {
            dimension.for_each_overlay(|coord, _| {
                self.last_used
                    .entry((name.clone(), coord.cx, coord.cz))
                    .or_insert(current_tick);
            });
        }
        self.prune_expired(current_tick);
        let victims = self.collect_evictable(current_tick, max_resident_unloaded, grace_ticks);
        let mut flushed = Vec::with_capacity(victims.len());
        for (name, cx, cz) in victims {
            let coord = ChunkCoord::new(cx, cz);
            if let Some(overlay) = ctx
                .dimension(&name)
                .and_then(|dim| dim.remove_overlay(coord))
            {
                flushed.push(FlushedChunk {
                    dimension: name.clone(),
                    coord,
                    overlay,
                });
            }
            let key = (name, cx, cz);
            self.last_used.remove(&key);
            self.tickets.remove(&key);
        }
        flushed
    }

    /// Drop expired `Temporary` tickets and now-empty ticket sets.
    fn prune_expired(&mut self, current_tick: u64) {
        for kinds in self.tickets.values_mut() {
            kinds.retain(|kind, _| kind.is_active(current_tick));
        }
        self.tickets.retain(|_, kinds| !kinds.is_empty());
    }

    fn key(dimension: &str, cx: i32, cz: i32) -> ChunkKey {
        (dimension.to_string(), cx, cz)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn square_spiral_is_outward_and_covers_each_chunk_once() {
        let order = spiral_order(10, -20, 5, PregenShape::Square);
        assert_eq!(order.len(), 11 * 11);
        let unique: HashSet<_> = order.iter().copied().collect();
        assert_eq!(unique.len(), order.len());
        // Every cell of the square, nothing outside it.
        for &(cx, cz) in &order {
            assert!((-5..=5).contains(&(cx - 10)));
            assert!((-5..=5).contains(&(cz + 20)));
        }
        // Strictly outward: ring index (Chebyshev distance) never decreases.
        let mut last_ring = 0i64;
        for &(cx, cz) in &order {
            let ring = ((cx - 10) as i64).abs().max(((cz + 20) as i64).abs());
            assert!(ring >= last_ring);
            last_ring = ring;
        }
        assert_eq!(order[0], (10, -20));
    }

    #[test]
    fn ring_cells_visits_each_ring_cell_exactly_once() {
        for k in 1..=8i64 {
            let cells = ring_cells(k);
            assert_eq!(cells.len(), (8 * k) as usize);
            let unique: HashSet<_> = cells.iter().copied().collect();
            assert_eq!(unique.len(), cells.len());
            assert!(cells.iter().all(|&(dx, dz)| dx.abs().max(dz.abs()) == k));
        }
    }

    #[test]
    fn circle_total_matches_filtered_spiral() {
        for radius in [0u32, 1, 3, 5, 9] {
            let order = spiral_order(0, 0, radius, PregenShape::Circle);
            assert_eq!(order.len() as u64, circle_total(radius));
            let r2 = radius as i64 * radius as i64;
            assert!(order
                .iter()
                .all(|&(cx, cz)| cx as i64 * cx as i64 + cz as i64 * cz as i64 <= r2));
        }
    }

    #[test]
    fn extreme_radius_totals_saturate_instead_of_overflowing() {
        // (2r+1)^2 for r = u32::MAX is ~7.4e19, past u64::MAX — the square
        // total overflowed (panic in debug, wrap in release) before.
        let mut job = PregenJob::new(0, 0, u32::MAX, PregenShape::Square);
        assert_eq!(job.progress().total, u64::MAX);
        // The job still emits its first ring correctly.
        assert_eq!(job.take_batch(1), vec![(0, 0)]);
    }

    #[test]
    fn temporary_ticket_expires() {
        let mut unloader = ChunkTicketLruUnloader::new();
        unloader.acquire("minecraft:overworld", 1, 1, TicketKind::Temporary(100), 50);
        assert!(unloader.is_protected("minecraft:overworld", 1, 1, 99));
        assert!(!unloader.is_protected("minecraft:overworld", 1, 1, 100));
        unloader.prune_expired(100);
        assert_eq!(unloader.active_tickets("minecraft:overworld", 1, 1, 100), 0);
    }
}
