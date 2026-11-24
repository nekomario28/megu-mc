// SPDX-License-Identifier: LGPL-3.0-or-later

//! High-speed batched TNT / explosion kernel.
//!
//! The raycast core reproduces the Vanilla `Explosion.collectBlocks` march
//! exactly: a 16x16x16 direction grid of which only the 1,352 surface cells
//! cast rays, each ray carries `power * (0.7 + rand * 0.6)` of remaining
//! power, marches in `0.3`-block steps, pays `(resistance + 0.3) * 0.3` for
//! every non-air cell and an unconditional `0.225` drain per step, and
//! records a cell while power remains after the block cost. All draws come
//! from a seeded [`XorShift64`] consumed in the canonical ray order, so a
//! given `(world, origin, power, seed)` reproduces bit-for-bit on every run.
//!
//! On top of the scalar march this module adds the pieces a modded
//! mega-TNT needs to stay inside a 50 ms tick:
//!
//! * [`BlastVolumeCache`] snapshots the blast bounding box into one flat
//!   array per explosion instead of probing a global map per ray step, and
//!   [`ExplosionBatch`] reuses that allocation (plus a dense `f32`
//!   intensity grid) across every explosion of a tick.
//! * [`BlastWorld`] applies explosions sequentially against an overlay of
//!   already-destroyed cells, so stacked or chained primed TNT in the same
//!   tick deduplicate by position and skip raycasts for cells that are
//!   already air.
//! * [`MegaBlastPlan`] slices a huge [`BlastResult`] into per-tick
//!   concentric rings (ring `r` = block centers `r..r+1` from the origin,
//!   innermost first, ascending coordinates inside a ring) bounded by
//!   `max_blocks_per_tick` — [`MEGA_BLAST_MAX_PER_TICK`] names the 16k
//!   block threshold above which a server should slice — groups block
//!   updates per 16x16x16 [`SectionBlocksUpdate`] packet and
//!   pre-coalesces drops into <= [`MAX_STACK_SIZE`] item stacks via
//!   [`coalesce_item_counts`], conserving every dropped item.
//!
//! Safety: powers are scrubbed through [`scrub_power`] (`NaN`/infinite ->
//! 0, negatives -> 0, huge -> [`MAX_BLAST_POWER`]), coordinates use
//! saturating/checked `i64` arithmetic, and a blast whose bounding volume
//! exceeds [`MAX_CACHE_CELLS`] fails closed with
//! [`ExplosionError::RegionTooLarge`] instead of allocating without bound.
//! Nothing here reads a wall clock or depends on crate-external state.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::error::Error;
use std::fmt;
use std::hash::{BuildHasherDefault, Hasher};

/// Vanilla casts rays only from the surface of a 16x16x16 direction grid.
pub const RAY_GRID: i32 = 16;
/// 16^3 - 14^3 surface cells: the exact Vanilla ray count for one explosion.
pub const RAY_SURFACE_CELLS: usize = 1352;
/// Vanilla ray march step length in blocks.
pub const RAY_STEP: f64 = 0.3;
/// Unconditional per-step power drain applied to every ray step.
pub const STEP_DRAIN: f32 = 0.225;
/// Block-cost base inside `(resistance + BLOCK_COST_BASE) * BLOCK_COST_BASE`.
pub const BLOCK_COST_BASE: f32 = 0.3;
/// Per-ray power factor lower bound (`0.7 + rand * 0.6`).
pub const RAY_POWER_MIN: f32 = 0.7;
/// Per-ray power factor range width.
pub const RAY_POWER_RANGE: f32 = 0.6;
/// Vanilla TNT explosion power.
pub const TNT_POWER: f32 = 4.0;
/// Upper clamp applied by [`scrub_power`]; larger requests fail closed at
/// the cache bound ([`ExplosionError::RegionTooLarge`]) before allocating.
pub const MAX_BLAST_POWER: f32 = 64.0;
/// Conservative multiple of `power` covering the farthest point a ray can
/// still have power at (all-air march: `power * 1.3 / 0.225 * 0.3` ~=
/// `power * 1.734`), plus [`BLAST_BOUNDS_MARGIN`] blocks of slack.
pub const BLAST_REACH_FACTOR: f64 = 1.75;
/// Extra cells of margin around the theoretical ray reach.
pub const BLAST_BOUNDS_MARGIN: i64 = 2;
/// Hard cap on cached blast volume; exceeding it is an error, never an OOM.
pub const MAX_CACHE_CELLS: usize = 6_000_000;
/// Maximum block removals one tick of a [`MegaBlastPlan`] may apply. A
/// blast affecting more cells than this must drain over multiple ticks so
/// a single 50 ms tick never stalls; smaller blasts may be applied whole.
pub const MEGA_BLAST_MAX_PER_TICK: usize = 16_384;
/// Vanilla item stack ceiling used by [`coalesce_item_counts`].
pub const MAX_STACK_SIZE: u32 = 64;
/// Per-ray step cap; unreachable for finite powers (each step drains >=
/// [`STEP_DRAIN`]) and only guards against pathological float states.
pub const MAX_RAY_STEPS: u32 = 1 << 20;
/// Cell flag: never recorded as destroyed regardless of remaining power.
pub const FLAG_INDESTRUCTIBLE: u8 = 1;

/// One block sample inside a [`BlastVolumeCache`]. `block_id == 0` is air.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BlastCell {
    /// Registry-style block identifier; 0 means air.
    pub block_id: u32,
    /// Item entity produced per destroyed block (0 = no drop).
    pub drop_item: u32,
    /// Vanilla-style explosion resistance fed into `(resistance + 0.3) * 0.3`.
    pub blast_resistance: f32,
    /// Copies of `drop_item` produced per destroyed block.
    pub drop_count: u8,
    /// Bitfield; see [`FLAG_INDESTRUCTIBLE`].
    pub flags: u8,
}

impl BlastCell {
    /// The canonical air cell.
    pub const AIR: Self = Self {
        block_id: 0,
        drop_item: 0,
        blast_resistance: 0.0,
        drop_count: 0,
        flags: 0,
    };

    /// A plain destructible solid.
    pub const fn solid(
        block_id: u32,
        blast_resistance: f32,
        drop_item: u32,
        drop_count: u8,
    ) -> Self {
        Self {
            block_id,
            drop_item,
            blast_resistance,
            drop_count,
            flags: 0,
        }
    }

    /// A solid that rays pay resistance for but that is never destroyed
    /// (bedrock-style immunity on top of its huge resistance).
    pub const fn indestructible(
        block_id: u32,
        blast_resistance: f32,
        drop_item: u32,
        drop_count: u8,
    ) -> Self {
        Self {
            block_id,
            drop_item,
            blast_resistance,
            drop_count,
            flags: FLAG_INDESTRUCTIBLE,
        }
    }

    /// Air cells skip the block-cost term entirely.
    #[inline]
    pub fn is_air(&self) -> bool {
        self.block_id == 0
    }

    /// Indestructible cells still attenuate rays but are never recorded.
    #[inline]
    pub fn is_indestructible(&self) -> bool {
        self.flags & FLAG_INDESTRUCTIBLE != 0
    }
}

/// Read-only block source the kernels snapshot through.
pub trait BlastBlockSource {
    /// Cell at a block position; implementations return [`BlastCell::AIR`]
    /// for unknown positions.
    fn cell(&self, x: i32, y: i32, z: i32) -> BlastCell;
}

impl BlastBlockSource for HashMap<(i32, i32, i32), BlastCell> {
    fn cell(&self, x: i32, y: i32, z: i32) -> BlastCell {
        self.get(&(x, y, z)).copied().unwrap_or(BlastCell::AIR)
    }
}

impl<F> BlastBlockSource for F
where
    F: Fn(i32, i32, i32) -> BlastCell,
{
    fn cell(&self, x: i32, y: i32, z: i32) -> BlastCell {
        self(x, y, z)
    }
}

/// Fail-closed errors; the kernels prefer `Err` over unbounded allocation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExplosionError {
    /// The blast bounding volume exceeds [`MAX_CACHE_CELLS`].
    RegionTooLarge { cells: usize, limit: usize },
}

impl fmt::Display for ExplosionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RegionTooLarge { cells, limit } => write!(
                f,
                "blast bounding volume of {cells} cells exceeds limit {limit}"
            ),
        }
    }
}

impl Error for ExplosionError {}

/// Deterministic xorshift64* PRNG. One instance per explosion, consumed in
/// canonical ray order, so reruns are bit-identical.
#[derive(Clone, Copy, Debug)]
pub struct XorShift64 {
    state: u64,
}

impl XorShift64 {
    /// Seeded constructor; the state is folded through a constant so seed 0
    /// still produces a full-period stream.
    pub fn new(seed: u64) -> Self {
        let mut state = seed
            .wrapping_mul(0x9E37_79B9_7F4A_7C15)
            .wrapping_add(0xD1B5_4A32_D192_ED03);
        if state == 0 {
            state = 0x2545_F491_4F6C_DD1D;
        }
        Self { state }
    }

    /// xorshift64* next value.
    #[inline]
    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.state;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.state = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// Uniform f32 in `[0.0, 1.0)` with 24-bit resolution (Vanilla `nextFloat`
    /// granularity).
    #[inline]
    pub fn next_f32(&mut self) -> f32 {
        ((self.next_u64() >> 40) as f32) * (1.0 / 16_777_216.0)
    }
}

/// Scrub a requested blast power fail-closed: non-finite and negative input
/// becomes 0.0 (no destruction), finite input clamps to
/// `[0.0, MAX_BLAST_POWER]`.
#[inline]
pub fn scrub_power(power: f32) -> f32 {
    if power.is_finite() {
        power.clamp(0.0, MAX_BLAST_POWER)
    } else {
        0.0
    }
}

/// Inclusive `[min, max]` block bounds covering every point a ray can reach
/// for `power`, or `None` when the origin is non-finite.
pub fn blast_bounds(origin: [f64; 3], power: f32) -> Option<([i64; 3], [i64; 3])> {
    if !origin.iter().all(|v| v.is_finite()) {
        return None;
    }
    let extent = (f64::from(power) * BLAST_REACH_FACTOR).ceil() as i64 + BLAST_BOUNDS_MARGIN;
    let mut min = [0i64; 3];
    let mut max = [0i64; 3];
    for axis in 0..3 {
        let center = origin[axis].floor() as i64;
        min[axis] = center.saturating_sub(extent);
        max[axis] = center.saturating_add(extent);
    }
    Some((min, max))
}

/// Dense local snapshot of one blast bounding box: one flat [`BlastCell`]
/// per block instead of a hash probe per ray step. [`ExplosionBatch`]
/// reuses the allocation across explosions and ticks.
#[derive(Clone, Debug, Default)]
pub struct BlastVolumeCache {
    min: [i64; 3],
    dims: [usize; 3],
    cells: Vec<BlastCell>,
}

impl BlastVolumeCache {
    /// Empty cache; call [`Self::prepare`] before use.
    pub fn new() -> Self {
        Self::default()
    }

    /// (Re)build the snapshot for `origin`/`power`, reusing the backing
    /// allocation when the new volume fits. Returns
    /// [`ExplosionError::RegionTooLarge`] before allocating when the volume
    /// would exceed [`MAX_CACHE_CELLS`].
    pub fn prepare<S: BlastBlockSource + ?Sized>(
        &mut self,
        source: &S,
        origin: [f64; 3],
        power: f32,
    ) -> Result<(), ExplosionError> {
        let Some((min, max)) = blast_bounds(origin, power) else {
            self.min = [0; 3];
            self.dims = [0; 3];
            self.cells.clear();
            return Ok(());
        };
        let mut dims = [0usize; 3];
        let mut volume: usize = 1;
        for axis in 0..3 {
            dims[axis] = (max[axis] - min[axis] + 1).max(0) as usize;
            volume = volume.saturating_mul(dims[axis]);
        }
        if volume > MAX_CACHE_CELLS {
            return Err(ExplosionError::RegionTooLarge {
                cells: volume,
                limit: MAX_CACHE_CELLS,
            });
        }
        self.min = min;
        self.dims = dims;
        self.cells.clear();
        self.cells.reserve(volume);
        // Fill in x-fastest order so index math below stays a single
        // multiply-add per lookup.
        for lz in 0..dims[2] {
            for ly in 0..dims[1] {
                for lx in 0..dims[0] {
                    let x = min[0].saturating_add(lx as i64) as i32;
                    let y = min[1].saturating_add(ly as i64) as i32;
                    let z = min[2].saturating_add(lz as i64) as i32;
                    self.cells.push(source.cell(x, y, z));
                }
            }
        }
        Ok(())
    }

    /// Cache bounds `[min, max]` (inclusive, `i64` for overflow safety).
    pub fn bounds(&self) -> ([i64; 3], [i64; 3]) {
        let max = std::array::from_fn(|axis| {
            self.min[axis]
                .saturating_add(self.dims[axis] as i64)
                .saturating_sub(1)
        });
        (self.min, max)
    }

    /// Cached volume in cells.
    pub fn volume(&self) -> usize {
        self.cells.len()
    }

    /// Flat index of a block position, or `None` outside the snapshot.
    #[inline]
    fn index_of(&self, x: i32, y: i32, z: i32) -> Option<usize> {
        let lx = x as i64 - self.min[0];
        let ly = y as i64 - self.min[1];
        let lz = z as i64 - self.min[2];
        if lx < 0
            || ly < 0
            || lz < 0
            || lx >= self.dims[0] as i64
            || ly >= self.dims[1] as i64
            || lz >= self.dims[2] as i64
        {
            return None;
        }
        Some((lz * self.dims[1] as i64 + ly) as usize * self.dims[0] + lx as usize)
    }

    /// Cell at a block position; positions outside the snapshot read as air.
    #[inline]
    pub fn get(&self, x: i32, y: i32, z: i32) -> BlastCell {
        self.index_of(x, y, z)
            .map(|idx| self.cells[idx])
            .unwrap_or(BlastCell::AIR)
    }

    /// Cell plus flat index for the dense intensity grid.
    #[inline]
    fn locate(&self, x: i32, y: i32, z: i32) -> (Option<usize>, BlastCell) {
        match self.index_of(x, y, z) {
            Some(idx) => (Some(idx), self.cells[idx]),
            None => (None, BlastCell::AIR),
        }
    }
}

/// One destroyed block recorded by a [`BlastResult`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AffectedCell {
    /// Block position that was destroyed.
    pub pos: [i32; 3],
    /// Remaining ray power (post block-cost, pre step-drain) at the strongest
    /// ray that reached this cell; `> 0.0` by construction.
    pub intensity: f32,
    /// The cell snapshot that was destroyed (for drop resolution).
    pub cell: BlastCell,
}

/// Counters proving which code paths ran (useful for benchmarks and tests).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BlastStats {
    /// Rays cast (1352 for any non-empty blast).
    pub rays_cast: u64,
    /// Total ray march steps.
    pub steps: u64,
    /// Cells recorded as destroyed.
    pub affected: u64,
}

/// The destroyed-block set of one explosion, sorted by position for
/// deterministic downstream processing.
#[derive(Clone, Debug)]
pub struct BlastResult {
    /// Explosion origin the rays were cast from.
    pub origin: [f64; 3],
    /// Scrubbed power actually used.
    pub power: f32,
    /// Snapshot bounds the march was confined to.
    pub bounds: ([i64; 3], [i64; 3]),
    /// Destroyed cells sorted by `(x, y, z)`.
    pub cells: Vec<AffectedCell>,
    /// Per-explosion counters.
    pub stats: BlastStats,
}

impl BlastResult {
    /// Build a [`MegaBlastPlan`] draining this blast across ticks in
    /// concentric rings of at most `max_blocks_per_tick` removals each.
    pub fn plan(&self, max_blocks_per_tick: usize) -> MegaBlastPlan {
        MegaBlastPlan::new(self, max_blocks_per_tick)
    }

    /// Empty result for scrubbed-away inputs (non-finite origin, zero power).
    fn empty(origin: [f64; 3], power: f32) -> Self {
        Self {
            origin,
            power,
            bounds: ([0; 3], [0; 3]),
            cells: Vec::new(),
            stats: BlastStats::default(),
        }
    }
}

/// Reusable per-tick engine: the dense cache plus a parallel `f32`
/// intensity grid, both reallocated only when a larger blast needs them.
#[derive(Clone, Debug, Default)]
pub struct ExplosionBatch {
    cache: BlastVolumeCache,
    density: Vec<f32>,
}

impl ExplosionBatch {
    pub fn new() -> Self {
        Self::default()
    }

    /// Snapshot `source` around `origin`, march all 1,352 Vanilla rays and
    /// return the destroyed-cell set. `seed` drives the per-ray power
    /// factors; identical inputs always produce identical results.
    pub fn blast<S: BlastBlockSource + ?Sized>(
        &mut self,
        source: &S,
        origin: [f64; 3],
        power: f32,
        seed: u64,
    ) -> Result<BlastResult, ExplosionError> {
        let power = scrub_power(power);
        if power <= 0.0 {
            return Ok(BlastResult::empty(origin, power));
        }
        self.cache.prepare(source, origin, power)?;
        if self.cache.volume() == 0 {
            return Ok(BlastResult::empty(origin, power));
        }
        let bounds = self.cache.bounds();
        self.density.clear();
        self.density.resize(self.cache.volume(), 0.0);
        let mut stats = BlastStats::default();
        march_rays(&self.cache, &mut self.density, origin, power, seed, &mut stats);
        // Extract the density grid back into sorted affected cells.
        let mut cells = Vec::with_capacity(stats.affected as usize);
        for (idx, &intensity) in self.density.iter().enumerate() {
            if intensity > 0.0 {
                let (pos, cell) = self.cache.cell_and_pos(idx);
                cells.push(AffectedCell {
                    pos,
                    intensity,
                    cell,
                });
            }
        }
        cells.sort_by_key(|c| c.pos);
        stats.affected = cells.len() as u64;
        Ok(BlastResult {
            origin,
            power,
            bounds,
            cells,
            stats,
        })
    }
}

impl BlastVolumeCache {
    /// Reverse an index produced by [`Self::index_of`]; only valid for
    /// indices below `volume()`.
    fn cell_and_pos(&self, idx: usize) -> ([i32; 3], BlastCell) {
        let lz = idx / (self.dims[0] * self.dims[1]);
        let rem = idx % (self.dims[0] * self.dims[1]);
        let ly = rem / self.dims[0];
        let lx = rem % self.dims[0];
        let pos = [
            self.min[0].saturating_add(lx as i64) as i32,
            self.min[1].saturating_add(ly as i64) as i32,
            self.min[2].saturating_add(lz as i64) as i32,
        ];
        (pos, self.cells[idx])
    }
}

/// The shared Vanilla march. `density` must be parallel to `cache.cells`;
/// out-of-cache positions read as air and are never recorded, matching the
/// bounded blast domain exactly.
fn march_rays(
    cache: &BlastVolumeCache,
    density: &mut [f32],
    origin: [f64; 3],
    power: f32,
    seed: u64,
    stats: &mut BlastStats,
) {
    let mut rng = XorShift64::new(seed);
    let grid = RAY_GRID as usize;
    for i in 0..grid {
        for j in 0..grid {
            for k in 0..grid {
                let (iu, ju, ku) = (i as i32, j as i32, k as i32);
                if iu != 0 && iu != RAY_GRID - 1 && ju != 0 && ju != RAY_GRID - 1 && ku != 0
                    && ku != RAY_GRID - 1
                {
                    continue;
                }
                stats.rays_cast += 1;
                let mut dx = (f64::from(iu) / 15.0) * 2.0 - 1.0;
                let mut dy = (f64::from(ju) / 15.0) * 2.0 - 1.0;
                let mut dz = (f64::from(ku) / 15.0) * 2.0 - 1.0;
                let len = (dx * dx + dy * dy + dz * dz).sqrt();
                dx /= len;
                dy /= len;
                dz /= len;
                let mut remaining = power * (RAY_POWER_MIN + rng.next_f32() * RAY_POWER_RANGE);
                let (mut px, mut py, mut pz) = (origin[0], origin[1], origin[2]);
                let mut steps = 0u32;
                while remaining > 0.0 && steps < MAX_RAY_STEPS {
                    steps += 1;
                    let bx = px.floor() as i32;
                    let by = py.floor() as i32;
                    let bz = pz.floor() as i32;
                    let (idx, cell) = cache.locate(bx, by, bz);
                    if !cell.is_air() {
                        let resistance = if cell.blast_resistance.is_nan() {
                            0.0
                        } else {
                            cell.blast_resistance.max(0.0)
                        };
                        remaining -= (resistance + BLOCK_COST_BASE) * BLOCK_COST_BASE;
                    }
                    if remaining > 0.0 && !cell.is_air() && !cell.is_indestructible() {
                        if let Some(idx) = idx {
                            let slot = &mut density[idx];
                            if remaining > *slot {
                                *slot = remaining;
                            }
                        }
                    }
                    px += dx * RAY_STEP;
                    py += dy * RAY_STEP;
                    pz += dz * RAY_STEP;
                    remaining -= STEP_DRAIN;
                }
                stats.steps += u64::from(steps);
            }
        }
    }
}

/// One coalesced item stack ready to become an item entity.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct ItemStack {
    /// Item registry id.
    pub item_id: u32,
    /// Stack size, always `<= stack_size` and `> 0`.
    pub count: u32,
}

/// Collapse `(item_id, count)` pairs into <= `stack_size` stacks.
///
/// Counts are aggregated per `item_id` in ascending id order and chunked,
/// so the total number of conserved items is identical before and after
/// coalescing while the entity/packet count drops by up to 64x.
pub fn coalesce_item_counts<I>(counts: I, stack_size: u32) -> Vec<ItemStack>
where
    I: IntoIterator<Item = (u32, u32)>,
{
    let stack = stack_size.max(1);
    let mut totals: BTreeMap<u32, u64> = BTreeMap::new();
    for (item_id, count) in counts {
        *totals.entry(item_id).or_insert(0) += u64::from(count);
    }
    let mut out = Vec::new();
    for (item_id, total) in totals {
        let mut left = total;
        while left > 0 {
            let take = left.min(u64::from(stack)) as u32;
            out.push(ItemStack {
                item_id,
                count: take,
            });
            left -= u64::from(take);
        }
    }
    out
}

/// One packed block update inside a [`SectionBlocksUpdate`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SectionBlockUpdate {
    /// Vanilla-style packed section-local position:
    /// `x | z << 4 | y << 8` with each component in `0..16`.
    pub packed_pos: u16,
    /// Block id written into the section (0 = air).
    pub block_id: u32,
}

/// Per-section block update packet payload.
#[derive(Clone, Debug)]
pub struct SectionBlocksUpdate {
    /// Section coordinates `(cx, cy, cz)` the updates belong to.
    pub section: [i32; 3],
    /// Packed updates sorted by `packed_pos` for deterministic encoding.
    pub updates: Vec<SectionBlockUpdate>,
}

/// One scheduled block removal inside a [`MegaBlastPlan`] tick.
#[derive(Clone, Copy, Debug)]
pub struct BlockRemoval {
    /// World block position.
    pub pos: [i32; 3],
    /// Concentric shell index: `floor` of the Euclidean distance from the
    /// explosion origin to the block center. Rings drain innermost first,
    /// so `ring` is non-decreasing across a whole plan.
    pub ring: u32,
    /// Destroyed block id.
    pub block_id: u32,
    /// Ray intensity that destroyed it.
    pub intensity: f32,
}

/// Everything one tick of a [`MegaBlastPlan`] emits.
#[derive(Clone, Debug)]
pub struct BlastTickSlice {
    /// Zero-based tick index this slice belongs to.
    pub tick_index: u64,
    /// Removals applied this tick, in concentric-ring order (ascending
    /// `ring`, then ascending `pos` inside each ring).
    pub removals: Vec<BlockRemoval>,
    /// Innermost ring this slice touches (`removals[0].ring`).
    pub first_ring: u32,
    /// Outermost ring this slice touches (`removals.last().ring`). A ring
    /// larger than the tick budget splits across slices, so the next
    /// slice's `first_ring` may equal this slice's `last_ring` but is
    /// never smaller.
    pub last_ring: u32,
    /// Block updates grouped per section for compact packets.
    pub section_updates: Vec<SectionBlocksUpdate>,
    /// Coalesced item drops spawned this tick.
    pub drops: Vec<ItemStack>,
    /// Total removals applied so far.
    pub done: usize,
    /// Total removals planned.
    pub total: usize,
}

#[derive(Clone, Copy, Debug)]
struct PlannedRemoval {
    pos: [i32; 3],
    ring: u32,
    intensity: f32,
    cell: BlastCell,
}

/// Progressive executor for huge blasts: buckets the destroyed set into
/// discrete concentric rings around the origin — ring `r` holds every
/// block whose center sits `r..r+1` blocks away — sorts each ring by
/// block position, and releases at most `max_blocks_per_tick` removals
/// per [`Self::next_slice`] call, so even a power-64 mega-TNT never
/// freezes a tick.
#[derive(Clone, Debug)]
pub struct MegaBlastPlan {
    center: [f64; 3],
    removals: Vec<PlannedRemoval>,
    cursor: usize,
    budget: usize,
    ring_count: usize,
    stack_size: u32,
}

impl MegaBlastPlan {
    /// Plan `result` into per-tick slices of at most `max_blocks_per_tick`
    /// removals (clamped to >= 1). Item stacks are capped at
    /// [`MAX_STACK_SIZE`].
    pub fn new(result: &BlastResult, max_blocks_per_tick: usize) -> Self {
        let center = result.origin;
        let mut removals: Vec<PlannedRemoval> = result
            .cells
            .iter()
            .map(|c| {
                let dx = f64::from(c.pos[0]) + 0.5 - center[0];
                let dy = f64::from(c.pos[1]) + 0.5 - center[1];
                let dz = f64::from(c.pos[2]) + 0.5 - center[2];
                let dist2 = dx * dx + dy * dy + dz * dz;
                PlannedRemoval {
                    pos: c.pos,
                    // Shell index: floor of the Euclidean distance. `as`
                    // saturates at `u32::MAX` for absurd coordinates, which
                    // still lands the cell in the outermost ring.
                    ring: dist2.sqrt() as u32,
                    intensity: c.intensity,
                    cell: c.cell,
                }
            })
            .collect();
        // Concentric rings innermost first, deterministic coordinate
        // order inside each ring.
        removals.sort_by_key(|r| (r.ring, r.pos));
        // Distinct rings seen, counting only transitions (the vec is
        // already ring-sorted).
        let ring_count = removals
            .windows(2)
            .filter(|pair| pair[0].ring != pair[1].ring)
            .count()
            + usize::from(!removals.is_empty());
        Self {
            center,
            removals,
            cursor: 0,
            budget: max_blocks_per_tick.max(1),
            ring_count,
            stack_size: MAX_STACK_SIZE,
        }
    }

    /// Whether `result` is large enough to require multi-tick draining:
    /// strictly more affected cells than [`MEGA_BLAST_MAX_PER_TICK`].
    pub fn needs_slicing(result: &BlastResult) -> bool {
        result.cells.len() > MEGA_BLAST_MAX_PER_TICK
    }

    /// Number of distinct concentric rings the plan covers.
    pub fn ring_count(&self) -> usize {
        self.ring_count
    }

    /// Per-tick removal bound the plan was built with.
    pub fn budget(&self) -> usize {
        self.budget
    }

    /// Total removals planned.
    pub fn total(&self) -> usize {
        self.removals.len()
    }

    /// Removals already emitted.
    pub fn done(&self) -> usize {
        self.cursor
    }

    /// Whether every removal has been emitted.
    pub fn is_complete(&self) -> bool {
        self.cursor >= self.removals.len()
    }

    /// `(done, total)` progress for monotonic-completion checks.
    pub fn progress(&self) -> (usize, usize) {
        (self.cursor, self.removals.len())
    }

    /// Emit the next tick slice, or `None` once the plan is complete.
    /// Each call removes at most `budget` blocks.
    pub fn next_slice(&mut self) -> Option<BlastTickSlice> {
        if self.cursor >= self.removals.len() {
            return None;
        }
        let end = (self.cursor + self.budget).min(self.removals.len());
        let slice = &self.removals[self.cursor..end];
        let tick_index = (self.cursor / self.budget) as u64;
        let mut removals = Vec::with_capacity(slice.len());
        let mut sections: BTreeMap<[i32; 3], Vec<SectionBlockUpdate>> = BTreeMap::new();
        let mut drops: Vec<(u32, u32)> = Vec::new();
        for r in slice {
            removals.push(BlockRemoval {
                pos: r.pos,
                ring: r.ring,
                block_id: r.cell.block_id,
                intensity: r.intensity,
            });
            let section = [
                r.pos[0].div_euclid(16),
                r.pos[1].div_euclid(16),
                r.pos[2].div_euclid(16),
            ];
            let local = [
                r.pos[0].rem_euclid(16) as u16,
                r.pos[1].rem_euclid(16) as u16,
                r.pos[2].rem_euclid(16) as u16,
            ];
            let packed = local[0] | (local[2] << 4) | (local[1] << 8);
            sections.entry(section).or_default().push(SectionBlockUpdate {
                packed_pos: packed,
                block_id: 0,
            });
            if r.cell.drop_item != 0 && r.cell.drop_count > 0 {
                drops.push((r.cell.drop_item, u32::from(r.cell.drop_count)));
            }
        }
        let mut section_updates: Vec<SectionBlocksUpdate> = sections
            .into_iter()
            .map(|(section, mut updates)| {
                updates.sort_by_key(|u| u.packed_pos);
                SectionBlocksUpdate { section, updates }
            })
            .collect();
        section_updates.sort_by_key(|s| s.section);
        self.cursor = end;
        Some(BlastTickSlice {
            tick_index,
            first_ring: slice.first().map(|r| r.ring).unwrap_or(0),
            last_ring: slice.last().map(|r| r.ring).unwrap_or(0),
            removals,
            section_updates,
            drops: coalesce_item_counts(drops, self.stack_size),
            done: self.cursor,
            total: self.removals.len(),
        })
    }

    /// Drain the whole plan, returning every slice in order.
    pub fn drain(&mut self) -> Vec<BlastTickSlice> {
        let mut out = Vec::new();
        while let Some(slice) = self.next_slice() {
            out.push(slice);
        }
        out
    }

    /// Explosion origin (kept for callers that need it after planning).
    pub fn center(&self) -> [f64; 3] {
        self.center
    }
}

/// Counters for [`BlastWorld`] runs.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BlastWorldStats {
    /// Explosions actually raycast.
    pub blasts_run: u64,
    /// Primed positions skipped because the cell was already air.
    pub skipped_already_air: u64,
    /// Duplicate positions removed before detonation.
    pub deduplicated: u64,
}

/// FxHash-style hasher: a few multiplies per key, no `SipHash` overhead on
/// the per-step hot path's bookkeeping structures.
#[derive(Default)]
pub struct FxHasher {
    hash: u64,
}

impl Hasher for FxHasher {
    #[inline]
    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.hash = (self.hash.rotate_left(5) ^ u64::from(b)).wrapping_mul(0x517C_C1B7_2722_0A95);
        }
    }

    #[inline]
    fn write_u8(&mut self, n: u8) {
        self.hash = (self.hash.rotate_left(5) ^ u64::from(n)).wrapping_mul(0x517C_C1B7_2722_0A95);
    }

    #[inline]
    fn write_u32(&mut self, n: u32) {
        self.hash = (self.hash.rotate_left(5) ^ u64::from(n)).wrapping_mul(0x517C_C1B7_2722_0A95);
    }

    #[inline]
    fn write_u64(&mut self, n: u64) {
        self.hash = (self.hash.rotate_left(5) ^ n).wrapping_mul(0x517C_C1B7_2722_0A95);
    }

    #[inline]
    fn write_usize(&mut self, n: usize) {
        self.write_u64(n as u64);
    }

    #[inline]
    fn write_i32(&mut self, n: i32) {
        self.write_u32(n as u32);
    }

    #[inline]
    fn finish(&self) -> u64 {
        self.hash
    }
}

/// Sequential blast world: an immutable [`BlastBlockSource`] plus an
/// overlay of cells already destroyed this tick. Later explosions see the
/// earlier craters as air (Vanilla sequential semantics) and primed TNT
/// sitting in already-air cells are skipped without a raycast.
pub struct BlastWorld<'a, S: BlastBlockSource + ?Sized> {
    source: &'a S,
    destroyed: HashSet<[i32; 3], BuildHasherDefault<FxHasher>>,
    batch: ExplosionBatch,
    /// Running counters; inspect after each [`Self::detonate_tnt_tick`].
    pub stats: BlastWorldStats,
}

/// Overlay handed to [`ExplosionBatch::blast`]: destroyed cells read as air.
struct OverlaySource<'a, S: BlastBlockSource + ?Sized> {
    inner: &'a S,
    destroyed: &'a HashSet<[i32; 3], BuildHasherDefault<FxHasher>>,
}

impl<S: BlastBlockSource + ?Sized> BlastBlockSource for OverlaySource<'_, S> {
    fn cell(&self, x: i32, y: i32, z: i32) -> BlastCell {
        if self.destroyed.contains(&[x, y, z]) {
            BlastCell::AIR
        } else {
            self.inner.cell(x, y, z)
        }
    }
}

impl<'a, S: BlastBlockSource + ?Sized> BlastWorld<'a, S> {
    pub fn new(source: &'a S) -> Self {
        Self {
            source,
            destroyed: HashSet::default(),
            batch: ExplosionBatch::new(),
            stats: BlastWorldStats::default(),
        }
    }

    /// Effective cell at `pos`: air once destroyed this session.
    pub fn cell(&self, x: i32, y: i32, z: i32) -> BlastCell {
        if self.destroyed.contains(&[x, y, z]) {
            BlastCell::AIR
        } else {
            self.source.cell(x, y, z)
        }
    }

    /// Whether `pos` has been destroyed since this world was created.
    pub fn is_destroyed(&self, pos: [i32; 3]) -> bool {
        self.destroyed.contains(&pos)
    }

    /// Detonate one tick of primed TNT.
    ///
    /// Positions are sorted and deduplicated; any position whose cell is
    /// already air (destroyed by an earlier blast in the same session, or
    /// genuinely empty) is skipped without casting rays. Each real blast
    /// marks its destroyed cells so subsequent TNT see updated terrain.
    /// `power` is scrubbed per blast; `seed` is mixed with the blast ordinal
    /// so every explosion gets a deterministic, distinct RNG stream.
    pub fn detonate_tnt_tick(
        &mut self,
        positions: &[[i32; 3]],
        power: f32,
        seed: u64,
    ) -> Result<Vec<BlastResult>, ExplosionError> {
        let mut sorted: Vec<[i32; 3]> = positions.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        self.stats.deduplicated += (positions.len() - sorted.len()) as u64;
        let mut results = Vec::new();
        for (ordinal, &pos) in sorted.iter().enumerate() {
            if self.cell(pos[0], pos[1], pos[2]).is_air() {
                self.stats.skipped_already_air += 1;
                continue;
            }
            let overlay = OverlaySource {
                inner: self.source,
                destroyed: &self.destroyed,
            };
            // Primed TNT entity center: block center horizontally, block
            // bottom vertically (matches the scalar `explode_at` convention).
            let origin = [
                f64::from(pos[0]) + 0.5,
                f64::from(pos[1]),
                f64::from(pos[2]) + 0.5,
            ];
            let blast_seed = seed
                ^ (ordinal as u64)
                    .wrapping_mul(0x9E37_79B9_7F4A_7C15)
                    .rotate_left(29);
            let result = self.batch.blast(&overlay, origin, power, blast_seed)?;
            self.stats.blasts_run += 1;
            for affected in &result.cells {
                self.destroyed.insert(affected.pos);
            }
            self.destroyed.insert(pos);
            results.push(result);
        }
        Ok(results)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Solid stone cube `[-half, half]^3` with the given blast resistance.
    fn stone_cube(half: i32, resistance: f32) -> HashMap<(i32, i32, i32), BlastCell> {
        let mut map = HashMap::new();
        for x in -half..=half {
            for y in -half..=half {
                for z in -half..=half {
                    map.insert(
                        (x, y, z),
                        BlastCell::solid(7, resistance, 3, 2),
                    );
                }
            }
        }
        map
    }

    #[test]
    fn xor_shift_is_deterministic_and_bounded() {
        let mut a = XorShift64::new(42);
        let mut b = XorShift64::new(42);
        for _ in 0..256 {
            assert_eq!(a.next_u64(), b.next_u64());
        }
        let mut rng = XorShift64::new(0);
        for _ in 0..1024 {
            let v = rng.next_f32();
            assert!((0.0..1.0).contains(&v));
        }
        // Different seeds diverge immediately.
        assert_ne!(XorShift64::new(1).next_u64(), XorShift64::new(2).next_u64());
    }

    #[test]
    fn scrub_power_fails_closed() {
        assert_eq!(scrub_power(f32::NAN), 0.0);
        assert_eq!(scrub_power(f32::INFINITY), 0.0);
        assert_eq!(scrub_power(f32::NEG_INFINITY), 0.0);
        assert_eq!(scrub_power(-1.0), 0.0);
        assert_eq!(scrub_power(0.0), 0.0);
        assert_eq!(scrub_power(TNT_POWER), TNT_POWER);
        assert_eq!(scrub_power(1000.0), MAX_BLAST_POWER);
        assert_eq!(scrub_power(MAX_BLAST_POWER), MAX_BLAST_POWER);
    }

    #[test]
    fn blast_bounds_cover_ray_reach_and_reject_nonfinite() {
        assert_eq!(blast_bounds([f64::NAN, 0.0, 0.0], 4.0), None);
        let (min, max) = blast_bounds([0.5, 0.5, 0.5], 4.0).unwrap();
        // extent = ceil(4 * 1.75) + 2 = 9 around floor(0.5) = 0
        assert_eq!(min, [-9, -9, -9]);
        assert_eq!(max, [9, 9, 9]);
        // Zero power still reserves the margin around the origin cell.
        let (min, max) = blast_bounds([0.5, 0.5, 0.5], 0.0).unwrap();
        assert_eq!((min, max), ([-2, -2, -2], [2, 2, 2]));
    }

    #[test]
    fn volume_cache_snapshots_bounds_and_reads_air_outside() {
        let world = stone_cube(1, 0.5);
        let mut cache = BlastVolumeCache::new();
        cache.prepare(&world, [0.5, 0.5, 0.5], 4.0).unwrap();
        assert_eq!(
            cache.bounds(),
            blast_bounds([0.5, 0.5, 0.5], 4.0).unwrap()
        );
        assert!(cache.volume() > 0);
        assert_eq!(cache.get(0, 0, 0).block_id, 7);
        assert_eq!(cache.get(10_000, 0, 0), BlastCell::AIR);
        // A non-finite origin empties the snapshot instead of failing.
        cache.prepare(&world, [f64::NAN, 0.0, 0.0], 4.0).unwrap();
        assert_eq!(cache.volume(), 0);
        assert_eq!(cache.get(0, 0, 0), BlastCell::AIR);
    }

    #[test]
    fn empty_and_degenerate_blasts_cast_no_rays() {
        let world: HashMap<(i32, i32, i32), BlastCell> = HashMap::new();
        let mut batch = ExplosionBatch::new();
        // All-air world: every ray marches but records nothing.
        let result = batch.blast(&world, [0.5, 0.5, 0.5], TNT_POWER, 1).unwrap();
        assert_eq!(result.stats.rays_cast, RAY_SURFACE_CELLS as u64);
        assert_eq!(result.stats.affected, 0);
        assert!(result.stats.steps > 0);
        assert!(result.cells.is_empty());

        // Zero, negative, NaN power and non-finite origins are empty results.
        for (origin, power) in [
            ([0.5, 0.5, 0.5], 0.0),
            ([0.5, 0.5, 0.5], -4.0),
            ([0.5, 0.5, 0.5], f32::NAN),
            ([f64::NAN, 0.0, 0.0], 4.0),
        ] {
            let result = batch.blast(&world, origin, power, 1).unwrap();
            assert!(result.cells.is_empty());
            assert_eq!(result.stats, BlastStats::default());
            assert_eq!(result.bounds, ([0; 3], [0; 3]));
        }
    }

    #[test]
    fn cube_blast_destroys_cells_in_sorted_order_deterministically() {
        let world = stone_cube(2, 0.5);
        let mut batch = ExplosionBatch::new();
        let result = batch.blast(&world, [0.5, 0.5, 0.5], TNT_POWER, 7).unwrap();
        assert_eq!(result.stats.rays_cast, RAY_SURFACE_CELLS as u64);
        assert!(result.stats.affected > 0);
        assert_eq!(result.stats.affected as usize, result.cells.len());
        assert!(result.cells.windows(2).all(|w| w[0].pos <= w[1].pos));
        assert!(result
            .cells
            .iter()
            .all(|c| c.cell.block_id == 7 && c.intensity > 0.0));
        assert!(result
            .cells
            .iter()
            .all(|c| c.pos.iter().all(|&v| (-2..=2).contains(&v))));

        // Identical inputs reproduce the identical destroyed set.
        let mut again = ExplosionBatch::new();
        let second = again
            .blast(&world, [0.5, 0.5, 0.5], TNT_POWER, 7)
            .unwrap();
        assert_eq!(result.cells, second.cells);
        assert_eq!(result.stats, second.stats);
        assert_eq!(result.bounds, second.bounds);

        // A different seed changes the per-ray power draws.
        let third = again
            .blast(&world, [0.5, 0.5, 0.5], TNT_POWER, 8)
            .unwrap();
        assert_ne!(result.cells, third.cells);
    }

    #[test]
    fn resistance_and_indestructible_flags_block_recording() {
        // Bedrock-class resistance costs more per step than any TNT ray
        // carries, so rays die inside the first cell and nothing is recorded.
        let world = stone_cube(2, 1000.0);
        let mut batch = ExplosionBatch::new();
        let result = batch.blast(&world, [0.5, 0.5, 0.5], TNT_POWER, 3).unwrap();
        assert!(result.cells.is_empty());
        assert_eq!(result.stats.rays_cast, RAY_SURFACE_CELLS as u64);

        // Indestructible cells still attenuate rays but are never destroyed.
        let mut world = stone_cube(2, 0.0);
        for cell in world.values_mut() {
            cell.flags = FLAG_INDESTRUCTIBLE;
        }
        let result = batch
            .blast(&world, [0.5, 0.5, 0.5], TNT_POWER, 3)
            .unwrap();
        assert!(result.cells.is_empty());
        assert!(result.stats.steps > result.stats.rays_cast);
    }

    #[test]
    fn oversized_blast_fails_closed_before_allocating() {
        let world = stone_cube(0, 0.0);
        let mut batch = ExplosionBatch::new();
        let err = batch
            .blast(&world, [0.5, 0.5, 0.5], MAX_BLAST_POWER, 1)
            .unwrap_err();
        assert!(matches!(err, ExplosionError::RegionTooLarge { .. }));
    }

    #[test]
    fn coalesce_item_counts_conserves_and_chunks() {
        let stacks = coalesce_item_counts([(5, 10), (5, 70), (3, 1)], 64);
        assert_eq!(
            stacks,
            vec![
                ItemStack {
                    item_id: 3,
                    count: 1
                },
                ItemStack {
                    item_id: 5,
                    count: 64
                },
                ItemStack {
                    item_id: 5,
                    count: 16
                },
            ],
            "ascending item id, <= stack_size chunks"
        );
        assert_eq!(
            stacks.iter().map(|s| u64::from(s.count)).sum::<u64>(),
            81
        );
        // stack_size 0 clamps to 1 rather than dividing by zero.
        let stacks = coalesce_item_counts([(9, 2)], 0);
        assert_eq!(
            stacks,
            vec![
                ItemStack {
                    item_id: 9,
                    count: 1
                },
                ItemStack {
                    item_id: 9,
                    count: 1
                },
            ]
        );
        assert!(coalesce_item_counts(std::iter::empty::<(u32, u32)>(), 64).is_empty());
    }

    fn planned_result() -> BlastResult {
        BlastResult {
            origin: [0.5, 0.5, 0.5],
            power: TNT_POWER,
            bounds: ([0; 3], [0; 3]),
            cells: vec![
                AffectedCell {
                    pos: [5, 5, 5],
                    intensity: 0.5,
                    cell: BlastCell::solid(2, 0.0, 0, 0),
                },
                AffectedCell {
                    pos: [1, 0, 0],
                    intensity: 2.0,
                    cell: BlastCell::solid(1, 0.0, 5, 2),
                },
                AffectedCell {
                    pos: [0, 0, 0],
                    intensity: 3.0,
                    cell: BlastCell::solid(1, 0.0, 5, 2),
                },
                AffectedCell {
                    pos: [0, 1, 0],
                    intensity: 1.0,
                    cell: BlastCell::solid(1, 0.0, 5, 2),
                },
            ],
            stats: BlastStats::default(),
        }
    }

    #[test]
    fn mega_blast_plan_drains_in_concentric_rings() {
        let result = planned_result();
        assert!(!MegaBlastPlan::needs_slicing(&result));
        let mut plan = MegaBlastPlan::new(&result, 2);
        assert_eq!(plan.budget(), 2);
        assert_eq!(plan.total(), 4);
        // Block centers sit at pos + 0.5: [0,0,0] is ring 0, the unit-axis
        // neighbours are ring 1, and [5,5,5] lands in ring 8.
        assert_eq!(plan.ring_count(), 3);
        assert!(!plan.is_complete());
        assert_eq!(plan.progress(), (0, 4));

        let slices = plan.drain();
        assert_eq!(slices.len(), 2);
        assert_eq!(
            slices[0]
                .removals
                .iter()
                .map(|r| r.pos)
                .collect::<Vec<_>>(),
            vec![[0, 0, 0], [0, 1, 0]]
        );
        assert_eq!((slices[0].first_ring, slices[0].last_ring), (0, 1));
        assert_eq!(
            slices[1]
                .removals
                .iter()
                .map(|r| r.pos)
                .collect::<Vec<_>>(),
            vec![[1, 0, 0], [5, 5, 5]]
        );
        assert_eq!((slices[1].first_ring, slices[1].last_ring), (1, 8));
        assert!(slices[1].last_ring >= slices[0].last_ring);

        // Section updates pack vanilla-style and sort by packed position.
        for slice in &slices {
            assert_eq!(slice.section_updates.len(), 1);
            let section = &slice.section_updates[0];
            assert_eq!(section.section, [0, 0, 0]);
            assert!(section
                .updates
                .windows(2)
                .all(|w| w[0].packed_pos <= w[1].packed_pos));
            assert!(section.updates.iter().all(|u| u.block_id == 0));
        }
        assert_eq!(slices[0].section_updates[0].updates[0].packed_pos, 0);
        assert_eq!(slices[0].section_updates[0].updates[1].packed_pos, 256);
        assert_eq!(slices[1].section_updates[0].updates[0].packed_pos, 1);
        // [5,5,5] local (5,5,5): 5 | 5 << 4 | 5 << 8
        assert_eq!(slices[1].section_updates[0].updates[1].packed_pos, 1365);

        // Drops coalesce per slice and conserve the per-block counts.
        assert_eq!(
            slices[0].drops,
            vec![ItemStack {
                item_id: 5,
                count: 4
            }]
        );
        assert_eq!(
            slices[1].drops,
            vec![ItemStack {
                item_id: 5,
                count: 2
            }]
        );

        assert!(plan.is_complete());
        assert_eq!(plan.done(), 4);
        assert_eq!(plan.progress(), (4, 4));
        assert_eq!(slices[1].done, 4);
        assert_eq!(slices[1].total, 4);
        assert_eq!(slices[0].tick_index, 0);
        assert_eq!(slices[1].tick_index, 1);
        assert!(plan.next_slice().is_none());
        assert_eq!(plan.center(), [0.5, 0.5, 0.5]);
    }

    #[test]
    fn mega_blast_plan_edge_cases() {
        // Budget clamps to at least one removal per tick.
        let mut plan = MegaBlastPlan::new(&planned_result(), 0);
        assert_eq!(plan.budget(), 1);
        assert_eq!(plan.drain().len(), 4);

        // An empty blast plans nothing.
        let empty = BlastResult::empty([0.0; 3], 0.0);
        let mut plan = MegaBlastPlan::new(&empty, 16);
        assert_eq!(plan.total(), 0);
        assert_eq!(plan.ring_count(), 0);
        assert!(plan.is_complete());
        assert!(plan.next_slice().is_none());

        // The slicing threshold is strict.
        let cells = (0..=MEGA_BLAST_MAX_PER_TICK)
            .map(|i| AffectedCell {
                pos: [i as i32, 0, 0],
                intensity: 1.0,
                cell: BlastCell::solid(1, 0.0, 0, 0),
            })
            .collect();
        let big = BlastResult {
            cells,
            ..planned_result()
        };
        assert!(MegaBlastPlan::needs_slicing(&big));
        let mut plan = MegaBlastPlan::new(&big, MEGA_BLAST_MAX_PER_TICK);
        let first = plan.next_slice().unwrap();
        assert_eq!(first.removals.len(), MEGA_BLAST_MAX_PER_TICK);
        assert_eq!(first.done, MEGA_BLAST_MAX_PER_TICK);
        assert_eq!(plan.next_slice().unwrap().removals.len(), 1);
        assert!(plan.next_slice().is_none());
    }

    #[test]
    fn blast_world_dedups_sorts_and_sees_prior_craters() {
        let mut world_map = stone_cube(8, 0.5);
        world_map.insert((0, 0, 0), BlastCell::solid(9, 0.3, 0, 0));
        world_map.insert((6, 0, 6), BlastCell::solid(9, 0.3, 0, 0));
        let mut world = BlastWorld::new(&world_map);

        // Duplicate primed positions collapse; a genuinely air cell skips
        // without casting rays.
        let results = world
            .detonate_tnt_tick(
                &[[0, 0, 0], [0, 0, 0], [6, 0, 6], [40, 0, 40]],
                TNT_POWER,
                11,
            )
            .unwrap();
        assert_eq!(world.stats.deduplicated, 1);
        assert_eq!(world.stats.skipped_already_air, 1);
        assert_eq!(world.stats.blasts_run, 2);
        assert_eq!(results.len(), 2);
        assert!(world.is_destroyed([0, 0, 0]));
        assert!(world.cell(0, 0, 0).is_air());
        // The second charge sits ~8.5 blocks away through stone: the first
        // crater cannot reach it, so its own blast still runs.
        assert!(world.is_destroyed([6, 0, 6]));

        // Re-detonating the same positions now finds air and skips.
        let results = world
            .detonate_tnt_tick(&[[0, 0, 0], [6, 0, 6]], TNT_POWER, 11)
            .unwrap();
        assert!(results.is_empty());
        assert_eq!(world.stats.skipped_already_air, 3);
        assert_eq!(world.stats.blasts_run, 2);

        // Degenerate power still consumes a solid position deterministically.
        let results = world
            .detonate_tnt_tick(&[[-7, 0, -7]], f32::NAN, 5)
            .unwrap();
        assert_eq!(results.len(), 1);
        assert!(results[0].cells.is_empty());
        assert!(world.is_destroyed([-7, 0, -7]));
    }
}
