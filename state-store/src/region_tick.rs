// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! Deterministic independent region-tick partitioning for
//! [`crate::sharded::DimensionState`].
//!
//! Active chunks are grouped into 8x8-chunk super-regions
//! ([`RegionCoord`]) and the regions are bucketed into four checkerboard
//! "waves" by `(rx & 1, rz & 1)`. Two regions that share a wave color always
//! differ by an *even* amount on both axes, so distinct same-color regions
//! are separated by at least one whole region — they can never share a chunk
//! border. Ticking the regions of one wave in parallel therefore produces
//! zero cross-region chunk contention, and running the waves in order gives
//! bit-identical results for any worker count.
//!
//! Parallelism runs on the resident [`MeguTaskPool`] workers of
//! `mc_spine_core::task_pool` via `scope_blocking` — scoped, stack-borrowing
//! fan-out with caller help-stealing, so there is no per-wave OS thread
//! spawn/join cost and even a single-worker pool cannot deadlock.

use std::collections::BTreeMap;
use std::sync::OnceLock;

use mc_spine_core::task_pool::{MeguTaskPool, TaskLane};

use crate::sharded::{ChunkCoord, DimensionState};

/// Chunk columns per super-region side: one region is 8x8 chunks.
pub const REGION_SIDE_CHUNKS: i32 = 8;
/// Checkerboard colors — the number of waves a partition can produce.
pub const WAVE_COUNT: usize = 4;

/// An 8x8-chunk super-region address. `rx`/`rz` floor toward negative
/// infinity like vanilla, so chunk `-1` lands in region `-1`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RegionCoord {
    pub rx: i32,
    pub rz: i32,
}

impl RegionCoord {
    pub fn new(rx: i32, rz: i32) -> Self {
        Self { rx, rz }
    }

    /// The region holding `chunk`.
    pub fn for_chunk(chunk: ChunkCoord) -> Self {
        Self {
            rx: chunk.cx.div_euclid(REGION_SIDE_CHUNKS),
            rz: chunk.cz.div_euclid(REGION_SIDE_CHUNKS),
        }
    }

    /// Checkerboard color in `0..WAVE_COUNT`. `& 1` reads the two's-complement
    /// low bit, so negative coordinates color exactly like positive ones.
    fn wave_color(self) -> usize {
        ((self.rx & 1) | ((self.rz & 1) << 1)) as usize
    }
}

/// One region's tickable work: its coordinate and its active chunks, sorted
/// and deduplicated.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RegionWork {
    pub region: RegionCoord,
    pub chunks: Vec<ChunkCoord>,
}

/// Group `chunks` into regions, then bucket the regions into up to
/// [`WAVE_COUNT`] waves by checkerboard color. Regions land in each wave in
/// `(rx, rz)` order and each region's chunks are sorted — the partition is a
/// pure function of the input set, independent of input order or duplicates.
fn partition_waves(chunks: &[ChunkCoord]) -> Vec<Vec<RegionWork>> {
    let mut regions: BTreeMap<RegionCoord, Vec<ChunkCoord>> = BTreeMap::new();
    for &chunk in chunks {
        regions
            .entry(RegionCoord::for_chunk(chunk))
            .or_default()
            .push(chunk);
    }
    let mut waves: [Vec<RegionWork>; WAVE_COUNT] = Default::default();
    for (region, mut region_chunks) in regions {
        region_chunks.sort_unstable();
        region_chunks.dedup();
        waves[region.wave_color()].push(RegionWork {
            region,
            chunks: region_chunks,
        });
    }
    waves.into_iter().filter(|wave| !wave.is_empty()).collect()
}

/// Group the loaded/active `chunks` into spatially independent region waves.
/// Every returned `Vec<ChunkCoord>` is one wave: no two regions inside it
/// share a chunk border, so every wave is safe to tick in parallel. Chunks
/// appear exactly once, in deterministic order.
pub fn partition_independent_regions(chunks: &[ChunkCoord]) -> Vec<Vec<ChunkCoord>> {
    partition_waves(chunks)
        .into_iter()
        .map(|wave| {
            wave.into_iter()
                .flat_map(|work| work.chunks)
                .collect()
        })
        .collect()
}

/// Summary of one [`tick_dimension_regions_parallel`] run.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RegionTickStats {
    /// Worker threads the waves were split across (clamped to `>= 1`).
    pub threads: usize,
    /// Waves that ran (`<= WAVE_COUNT`, zero on empty input).
    pub waves: usize,
    /// Distinct super-regions ticked.
    pub regions_ticked: usize,
    /// Chunk columns ticked (the deduplicated input size).
    pub chunks_ticked: usize,
}

/// Process-wide resident pool backing [`tick_dimension_regions_parallel`]
/// when the caller does not supply one. Sized once from the probed hardware
/// profile; the `OnceLock` intentionally lives for the whole process so
/// waves never pay OS thread spawn/join cost after the first tick.
fn default_pool() -> &'static MeguTaskPool {
    static DEFAULT_POOL: OnceLock<MeguTaskPool> = OnceLock::new();
    DEFAULT_POOL.get_or_init(MeguTaskPool::for_host)
}

/// Tick every active chunk of `dimension`, partitioned into independent
/// waves on the process-wide resident [`MeguTaskPool`]. Within a wave the
/// regions are split into `threads` contiguous slices — each task walks its
/// slice in `(rx, rz)` order — and waves run strictly in order, so the
/// result is bit-identical for any `threads` (same-color regions never
/// share a chunk border, so concurrent region ticks cannot contend).
///
/// `tick_fn` receives the dimension, the region coordinate and that region's
/// sorted active-chunk slice; it is invoked exactly once per region.
pub fn tick_dimension_regions_parallel<F>(
    dimension: &DimensionState,
    active_chunks: &[ChunkCoord],
    threads: usize,
    tick_fn: F,
) -> RegionTickStats
where
    F: Fn(&DimensionState, RegionCoord, &[ChunkCoord]) + Sync,
{
    tick_regions_in_pool(default_pool(), dimension, active_chunks, threads, tick_fn)
}

/// [`tick_dimension_regions_parallel`] on an explicit pool. Tests use this
/// to pin determinism across pool topologies (e.g. 1 vs 8 interactive
/// workers) without touching the process-wide default.
///
/// `threads` is the requested slice granularity per wave, exactly as in
/// [`tick_dimension_regions_parallel`]; slice tasks are stolen across the
/// pool's Lane-A workers and the calling thread helps when the pool is
/// smaller than `threads`, so the partition — and therefore the result — is
/// independent of pool size.
pub fn tick_regions_in_pool<F>(
    pool: &MeguTaskPool,
    dimension: &DimensionState,
    active_chunks: &[ChunkCoord],
    threads: usize,
    tick_fn: F,
) -> RegionTickStats
where
    F: Fn(&DimensionState, RegionCoord, &[ChunkCoord]) + Sync,
{
    let threads = threads.max(1);
    let waves = partition_waves(active_chunks);
    let stats = RegionTickStats {
        threads,
        waves: waves.len(),
        regions_ticked: waves.iter().map(Vec::len).sum(),
        chunks_ticked: waves.iter().flatten().map(|w| w.chunks.len()).sum(),
    };
    for wave in &waves {
        run_wave(pool, dimension, wave, threads, &tick_fn);
    }
    stats
}

/// Run one wave's regions over at most `threads` pool tasks. The sorted
/// region list is cut into contiguous slices so every region is ticked
/// exactly once in a deterministic assignment — scheduling order may
/// interleave, but the regions are non-adjacent by construction so
/// interleaving is unobservable.
fn run_wave<F>(
    pool: &MeguTaskPool,
    dimension: &DimensionState,
    wave: &[RegionWork],
    threads: usize,
    tick_fn: &F,
) where
    F: Fn(&DimensionState, RegionCoord, &[ChunkCoord]) + Sync,
{
    if wave.len() <= 1 || threads <= 1 {
        for work in wave {
            tick_fn(dimension, work.region, &work.chunks);
        }
        return;
    }
    let slice_len = wave.len().div_ceil(threads.min(wave.len()));
    pool.scope_blocking(TaskLane::Interactive, |scope| {
        for slice in wave.chunks(slice_len) {
            scope.spawn(move || {
                for work in slice {
                    tick_fn(dimension, work.region, &work.chunks);
                }
            });
        }
    });
}

/// Bits per [`RegionMobTable::alive_mask`] word.
const ALIVE_WORD_BITS: usize = 64;
/// Fixed integration step for [`RegionMobTable::step`], in seconds.
const MOB_STEP_DT: f64 = 0.05;
/// Per-tick downward acceleration applied to `vel_y` before integration.
const MOB_GRAVITY: f32 = 0.08;
/// Ground plane height; mobs clamp at `y = 0` with a damped bounce.
const MOB_GROUND_Y: f64 = 0.0;
/// Fraction of impact velocity kept after a ground bounce.
const MOB_BOUNCE_DAMP: f32 = 0.4;

/// Region-owned structure-of-arrays mob table.
///
/// One [`RegionCoord`] owns its table exclusively for the duration of a wave
/// tick, so [`RegionMobTable::step`] runs with **zero** lock acquisitions —
/// every field is a plain `Vec` and [`RegionMobTable::alive_mask`] is a
/// bitset scanned word-at-a-time, the shape auto-vectorizers target. The
/// table deliberately holds no `Mutex`/`RwLock`/atomic: cross-region mob
/// movement is *not* handled here, it goes through the deterministic
/// per-region outbox merged between waves, keeping same-wave ticks free of
/// shared mutable state.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RegionMobTable {
    /// Stable entity ids, parallel to every other column.
    pub eid: Vec<i32>,
    /// Packed entity-type id.
    pub etype: Vec<u16>,
    /// World position, one `f64` per axis.
    pub pos_x: Vec<f64>,
    pub pos_y: Vec<f64>,
    pub pos_z: Vec<f64>,
    /// Per-axis velocity.
    pub vel_x: Vec<f32>,
    pub vel_y: Vec<f32>,
    pub vel_z: Vec<f32>,
    /// `[yaw, pitch]` pair per mob.
    pub yaw_pitch: Vec<[f32; 2]>,
    /// Hit points; `<= 0.0` marks the mob dead.
    pub hp: Vec<f32>,
    /// `[fuse_ticks, fire_ticks, air_ticks]` packed per mob.
    pub fuse_fire_air: Vec<[i16; 3]>,
    /// Liveness bitset — dead mobs are skipped without touching their rows.
    /// Padding bits beyond `len()` are always zero.
    pub alive_mask: Vec<u64>,
}

impl RegionMobTable {
    /// Number of mob slots, alive or dead.
    pub fn len(&self) -> usize {
        self.eid.len()
    }

    /// Whether the table holds no mob slots.
    pub fn is_empty(&self) -> bool {
        self.eid.is_empty()
    }

    /// Append one live mob across every column.
    #[allow(clippy::too_many_arguments)]
    pub fn push(
        &mut self,
        eid: i32,
        etype: u16,
        pos: [f64; 3],
        vel: [f32; 3],
        yaw_pitch: [f32; 2],
        hp: f32,
        fuse_fire_air: [i16; 3],
    ) {
        self.eid.push(eid);
        self.etype.push(etype);
        self.pos_x.push(pos[0]);
        self.pos_y.push(pos[1]);
        self.pos_z.push(pos[2]);
        self.vel_x.push(vel[0]);
        self.vel_y.push(vel[1]);
        self.vel_z.push(vel[2]);
        self.yaw_pitch.push(yaw_pitch);
        self.hp.push(hp);
        self.fuse_fire_air.push(fuse_fire_air);
        let index = self.eid.len() - 1;
        if index.is_multiple_of(ALIVE_WORD_BITS) {
            self.alive_mask.push(0);
        }
        self.set_alive(index, true);
    }

    /// Liveness of slot `index`; out-of-range slots read as dead.
    pub fn alive(&self, index: usize) -> bool {
        self.alive_mask
            .get(index / ALIVE_WORD_BITS)
            .is_some_and(|word| word & (1u64 << (index % ALIVE_WORD_BITS)) != 0)
    }

    /// Set or clear slot `index`'s liveness bit; out-of-range is a no-op.
    pub fn set_alive(&mut self, index: usize, alive: bool) {
        let Some(word) = self.alive_mask.get_mut(index / ALIVE_WORD_BITS) else {
            return;
        };
        let bit = 1u64 << (index % ALIVE_WORD_BITS);
        if alive {
            *word |= bit;
        } else {
            *word &= !bit;
        }
    }

    /// Number of live mobs (popcount over the mask words).
    pub fn alive_count(&self) -> usize {
        self.alive_mask
            .iter()
            .map(|word| word.count_ones() as usize)
            .sum()
    }

    /// Advance every live mob by one deterministic tick: gravity, semi-
    /// implicit Euler integration, the `y = 0` ground plane with a damped
    /// bounce, fuse/fire/air countdowns, and death when `hp <= 0` or a lit
    /// fuse reaches zero.
    ///
    /// Runs entirely on `&mut self` with no `Mutex`, atomics or shared
    /// state, so a region can hold its table across a whole wave tick.
    /// `tick` is reserved for scheduled AI phases and does not affect the
    /// integration constants — the gameplay-accurate kernel replaces them
    /// without changing the ownership or lock-free contract.
    pub fn step(&mut self, tick: u64) {
        let _ = tick;
        for index in 0..self.len() {
            if !self.alive(index) {
                continue;
            }
            self.vel_y[index] -= MOB_GRAVITY;
            self.pos_x[index] += f64::from(self.vel_x[index]) * MOB_STEP_DT;
            self.pos_y[index] += f64::from(self.vel_y[index]) * MOB_STEP_DT;
            self.pos_z[index] += f64::from(self.vel_z[index]) * MOB_STEP_DT;
            if self.pos_y[index] < MOB_GROUND_Y {
                self.pos_y[index] = MOB_GROUND_Y;
                if self.vel_y[index] < 0.0 {
                    self.vel_y[index] = -self.vel_y[index] * MOB_BOUNCE_DAMP;
                }
            }
            let mut fuse_fire_air = self.fuse_fire_air[index];
            if fuse_fire_air[0] > 0 {
                fuse_fire_air[0] -= 1;
                if fuse_fire_air[0] == 0 {
                    self.hp[index] = 0.0;
                }
            }
            if fuse_fire_air[1] > 0 {
                fuse_fire_air[1] -= 1;
            }
            if fuse_fire_air[2] > i16::MIN {
                fuse_fire_air[2] -= 1;
            }
            self.fuse_fire_air[index] = fuse_fire_air;
            if self.hp[index] <= 0.0 {
                self.set_alive(index, false);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sharded::BlockCoord;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Mutex;

    fn coord(cx: i32, cz: i32) -> ChunkCoord {
        ChunkCoord::new(cx, cz)
    }

    /// Every chunk of regions `rx/rz` in `-2..=2` on both axes, covering all
    /// four colors on both halves of the coordinate space.
    fn grid_chunks() -> Vec<ChunkCoord> {
        let mut chunks = Vec::new();
        for cx in -17..=17 {
            for cz in -17..=17 {
                chunks.push(coord(cx, cz));
            }
        }
        chunks
    }

    #[test]
    fn region_coord_floors_toward_negative_infinity() {
        assert_eq!(RegionCoord::for_chunk(coord(0, 0)), RegionCoord::new(0, 0));
        assert_eq!(RegionCoord::for_chunk(coord(7, 15)), RegionCoord::new(0, 1));
        assert_eq!(RegionCoord::for_chunk(coord(8, -8)), RegionCoord::new(1, -1));
        assert_eq!(RegionCoord::for_chunk(coord(-1, -1)), RegionCoord::new(-1, -1));
        assert_eq!(RegionCoord::for_chunk(coord(-8, 8)), RegionCoord::new(-1, 1));
        assert_eq!(RegionCoord::for_chunk(coord(-9, -16)), RegionCoord::new(-2, -2));
    }

    /// Rebuild the region sets a wave's chunk list came from.
    fn wave_regions(wave: &[ChunkCoord]) -> Vec<RegionCoord> {
        let mut regions: Vec<RegionCoord> =
            wave.iter().map(|&c| RegionCoord::for_chunk(c)).collect();
        regions.sort_unstable();
        regions.dedup();
        regions
    }

    #[test]
    fn partition_never_packs_border_adjacent_regions_into_one_wave() {
        let waves = partition_independent_regions(&grid_chunks());
        assert_eq!(waves.len(), WAVE_COUNT);
        // Every chunk appears exactly once across the waves.
        let mut all: Vec<ChunkCoord> = waves.iter().flatten().copied().collect();
        all.sort_unstable();
        let mut expected = grid_chunks();
        expected.sort_unstable();
        assert_eq!(all, expected);

        for wave in &waves {
            let regions = wave_regions(wave);
            for (i, a) in regions.iter().enumerate() {
                for b in &regions[i + 1..] {
                    // Chebyshev distance > 1: not even a corner touch.
                    assert!(
                        (a.rx - b.rx).abs() > 1 || (a.rz - b.rz).abs() > 1,
                        "regions {a:?} and {b:?} share a wave but touch"
                    );
                }
            }
        }
    }

    #[test]
    fn partition_is_deterministic_and_dedupes_input() {
        let mut shuffled = grid_chunks();
        shuffled.reverse();
        shuffled.extend_from_slice(&grid_chunks());
        assert_eq!(
            partition_independent_regions(&grid_chunks()),
            partition_independent_regions(&shuffled)
        );
        assert!(partition_independent_regions(&[]).is_empty());
    }

    /// A deterministic chunk payload derived only from its coordinates.
    fn chunk_seed(chunk: ChunkCoord) -> u32 {
        (chunk.cx.wrapping_mul(31).wrapping_add(chunk.cz.wrapping_mul(17))) as u32
    }

    /// Write `chunk_seed` into the `(0, 0)` cell of each active chunk. The
    /// final overlay state is a pure function of the chunk set.
    fn tick_writes_seed(dim: &DimensionState, chunks: &[ChunkCoord]) {
        for &chunk in chunks {
            dim.set_block(
                BlockCoord::new(chunk.cx * 16, 0, chunk.cz * 16),
                chunk_seed(chunk),
            );
        }
    }

    fn snapshot(dim: &DimensionState, chunks: &[ChunkCoord]) -> Vec<(ChunkCoord, Option<u32>, u64)> {
        let mut snap: Vec<(ChunkCoord, Option<u32>, u64)> = chunks
            .iter()
            .map(|&c| {
                (
                    c,
                    dim.block(BlockCoord::new(c.cx * 16, 0, c.cz * 16)),
                    dim.overlay_revision(c),
                )
            })
            .collect();
        snap.sort_unstable();
        snap
    }

    #[test]
    fn parallel_tick_is_bit_identical_to_serial_tick() {
        let chunks = grid_chunks();
        let serial = DimensionState::new("minecraft:overworld");
        let serial_stats = tick_dimension_regions_parallel(&serial, &chunks, 1, |dim, _r, slice| {
            tick_writes_seed(dim, slice);
        });
        let parallel = DimensionState::new("minecraft:overworld");
        let parallel_stats =
            tick_dimension_regions_parallel(&parallel, &chunks, 4, |dim, _r, slice| {
                tick_writes_seed(dim, slice);
            });
        let quad = DimensionState::new("minecraft:overworld");
        let quad_stats = tick_dimension_regions_parallel(&quad, &chunks, 16, |dim, _r, slice| {
            tick_writes_seed(dim, slice);
        });

        assert_eq!(snapshot(&serial, &chunks), snapshot(&parallel, &chunks));
        assert_eq!(snapshot(&serial, &chunks), snapshot(&quad, &chunks));
        // Every chunk was written exactly once.
        assert!(snapshot(&serial, &chunks).iter().all(|(c, block, revision)| {
            *revision == 1 && *block == Some(chunk_seed(*c))
        }));
        let expected = RegionTickStats {
            threads: 0,
            waves: WAVE_COUNT,
            // cx/cz -17..=17 span regions -3..=2 on each axis: 6x6 regions.
            regions_ticked: 36,
            chunks_ticked: chunks.len(),
        };
        assert_eq!(
            serial_stats,
            RegionTickStats {
                threads: 1,
                ..expected
            }
        );
        assert_eq!(
            parallel_stats,
            RegionTickStats {
                threads: 4,
                ..expected
            }
        );
        assert_eq!(
            quad_stats,
            RegionTickStats {
                threads: 16,
                ..expected
            }
        );
    }

    #[test]
    fn serial_tick_visits_regions_in_wave_then_coordinate_order() {
        // Chunks deliberately listed out of order; the visitation record must
        // still be the canonical wave -> (rx, rz) order.
        let mut chunks = grid_chunks();
        chunks.reverse();
        let dim = DimensionState::new("minecraft:overworld");
        let order = Mutex::new(Vec::new());
        tick_dimension_regions_parallel(&dim, &chunks, 1, |_dim, region, slice| {
            order.lock().unwrap().push((region, slice.len()));
        });
        let order = order.into_inner().unwrap();

        let mut expected: Vec<(RegionCoord, usize)> = partition_waves(&chunks)
            .into_iter()
            .flat_map(|wave| {
                wave.into_iter()
                    .map(|work| (work.region, work.chunks.len()))
            })
            .collect();
        expected.dedup();
        assert_eq!(order, expected);
    }

    #[test]
    fn tick_fn_runs_once_per_region_across_any_thread_count() {
        let chunks = grid_chunks();
        let dim = DimensionState::new("minecraft:overworld");
        let calls = AtomicU64::new(0);
        for threads in [1, 2, 3, 4, 8] {
            calls.store(0, Ordering::Relaxed);
            tick_dimension_regions_parallel(&dim, &chunks, threads, |_dim, _r, _slice| {
                calls.fetch_add(1, Ordering::Relaxed);
            });
            // The -17..=17 chunk span covers regions -3..=2 on each axis.
            assert_eq!(calls.load(Ordering::Relaxed), 36, "threads={threads}");
        }
    }

    #[test]
    fn empty_and_single_region_inputs_still_report() {
        let dim = DimensionState::new("minecraft:overworld");
        let stats = tick_dimension_regions_parallel(&dim, &[], 4, |_d, _r, _c| unreachable!());
        assert_eq!(
            stats,
            RegionTickStats {
                threads: 4,
                waves: 0,
                regions_ticked: 0,
                chunks_ticked: 0,
            }
        );
        let stats = tick_dimension_regions_parallel(&dim, &[coord(3, -3), coord(3, -3)], 4, |d, _r, c| {
            tick_writes_seed(d, c);
        });
        assert_eq!(stats.regions_ticked, 1);
        assert_eq!(stats.chunks_ticked, 1);
        assert_eq!(stats.waves, 1);
    }
}
