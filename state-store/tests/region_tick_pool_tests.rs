// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! Gates P1 & P2 from
//! `audit/full-multithreading-server-client-parallel-architecture-20261123.md`
//! for the [`megu_state_store::region_tick`] resident-pool migration.
//!
//! * **P1** — 16 active regions across the 4 checkerboard waves, ticked for
//!   200 rounds on a 1-interactive-worker [`MeguTaskPool`] vs an 8-worker
//!   one, with per-region `(rx, rz, tick)` deterministic RNG writes plus a
//!   sorted cross-region outbox merge. The final state hash must be
//!   bit-identical: pool size must be unobservable.
//! * **P2** — the resident pool reuses the same workers across ticks
//!   (`tasks_executed()` grows by a clean constant delta, `live_workers()`
//!   never changes) and [`RegionMobTable`] steps 512 mobs lock-free,
//!   bit-identically on two concurrent scoped jobs.

use std::sync::{Mutex, MutexGuard};

use megu_core::task_pool::{MeguTaskPool, PoolTopologyConfig, TaskLane};
use megu_state_store::region_tick::{tick_regions_in_pool, RegionCoord, RegionMobTable};
use megu_state_store::sharded::{BlockCoord, ChunkCoord, DimensionState};

/// Poison-tolerant lock — same contract as `megu_core::task_pool`'s
/// internal helper; a panicked tick must not wedge the merge path.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// 4x4 region grid: `rx, rz in 0..4` puts exactly 4 regions in each of the
/// 4 checkerboard waves.
const GRID: i32 = 4;
const REGION_CHUNKS: usize = 8 * 8;
const TOTAL_CHUNKS: usize = GRID as usize * GRID as usize * REGION_CHUNKS;
const TICKS: u64 = 200;
/// Slice granularity handed to `tick_regions_in_pool`: 4 regions per wave
/// become 4 single-region tasks, so both pool sizes exercise `scope_blocking`.
const TICK_THREADS: usize = 4;
/// Deterministic cross-region events emitted per region per tick.
const OUTBOX_PER_REGION: u64 = 4;

/// splitmix64 finalizer — the whole test RNG, no external crates.
fn mix64(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^ (x >> 31)
}

/// Deterministic per-`(region, tick, seq)` value. `seq` namespaces the
/// primary chunk writes (`0..64`) from the outbox payloads (`>= 1024`).
fn region_value(rx: i32, rz: i32, tick: u64, seq: u64) -> u64 {
    mix64(
        (rx as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15)
            ^ (rz as u64).wrapping_mul(0xC2B2_AE3D_27D4_EB4F)
            ^ tick.wrapping_mul(0x1656_67B1_9E37_79F9)
            ^ seq.wrapping_mul(0x27D4_EB2F_1656_67C5),
    )
}

/// Every active chunk of the 4x4 region grid, sorted for determinism.
fn grid_chunks() -> Vec<ChunkCoord> {
    let mut chunks = Vec::with_capacity(TOTAL_CHUNKS);
    for rx in 0..GRID {
        for rz in 0..GRID {
            for dx in 0..8 {
                for dz in 0..8 {
                    chunks.push(ChunkCoord::new(rx * 8 + dx, rz * 8 + dz));
                }
            }
        }
    }
    chunks.sort_unstable();
    chunks
}

/// Commit-block probe layout: region `(rx, rz)` writes `OUTBOX_PER_REGION`
/// events at `x = rx*1024 + seq*16`, split across `y = 1` (low bits) and
/// `y = 2` (high bits) so the full `u64` payload lands in the state hash.
fn commit_block(rx: i32, rz: i32, seq: u64, high: bool) -> BlockCoord {
    BlockCoord::new(
        rx * 1024 + seq as i32 * 16,
        if high { 2 } else { 1 },
        rz * 1024,
    )
}

/// FNV-1a over every probed block value and owning chunk's overlay
/// revision, sorted so `HashMap` order can never leak in.
fn state_hash(dim: &DimensionState, chunks: &[ChunkCoord]) -> u64 {
    let mut records: Vec<(i32, i32, i32, u32, u64)> = Vec::new();
    for &chunk in chunks {
        let block = BlockCoord::new(chunk.cx * 16, 0, chunk.cz * 16);
        records.push((
            block.x,
            block.y,
            block.z,
            dim.block(block).unwrap_or(u32::MAX),
            dim.overlay_revision(chunk),
        ));
    }
    for rx in 0..GRID {
        for rz in 0..GRID {
            for seq in 0..OUTBOX_PER_REGION {
                for high in [false, true] {
                    let block = commit_block(rx, rz, seq, high);
                    let owner = ChunkCoord::new(block.x.div_euclid(16), block.z.div_euclid(16));
                    records.push((
                        block.x,
                        block.y,
                        block.z,
                        dim.block(block).unwrap_or(u32::MAX),
                        dim.overlay_revision(owner),
                    ));
                }
            }
        }
    }
    records.sort_unstable();
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for record in records {
        for word in [
            record.0 as u32 as u64,
            record.1 as u32 as u64,
            record.2 as u32 as u64,
            record.3 as u64,
            record.4,
        ] {
            for byte in word.to_le_bytes() {
                hash ^= u64::from(byte);
                hash = hash.wrapping_mul(0x0000_0100_0000_01B3);
            }
        }
    }
    hash
}

/// Run `ticks` region ticks on `pool`, merging the deterministic
/// cross-region outbox after every tick, and return the final state hash.
fn run_scenario(pool: &MeguTaskPool, ticks: u64) -> u64 {
    let dim = DimensionState::new("minecraft:overworld");
    let chunks = grid_chunks();
    // (rx, rz, seq, payload) — push order is scheduling-dependent, so the
    // merge sorts before applying; the result is order-independent.
    let outbox: Mutex<Vec<(i32, i32, u64, u64)>> = Mutex::new(Vec::new());
    for tick in 0..ticks {
        let tick_fn = |dim: &DimensionState, region: RegionCoord, slice: &[ChunkCoord]| {
            for (index, &chunk) in slice.iter().enumerate() {
                let value = region_value(region.rx, region.rz, tick, index as u64);
                dim.set_block(
                    BlockCoord::new(chunk.cx * 16, 0, chunk.cz * 16),
                    value as u32,
                );
            }
            let mut events = lock(&outbox);
            for seq in 0..OUTBOX_PER_REGION {
                let payload = region_value(region.rx, region.rz, tick, 0xFFFF + seq);
                events.push((region.rx, region.rz, seq, payload));
            }
        };
        let stats = tick_regions_in_pool(pool, &dim, &chunks, TICK_THREADS, tick_fn);
        assert_eq!(stats.waves, 4, "4x4 grid must fill all checkerboard waves");
        assert_eq!(stats.regions_ticked, 16);
        assert_eq!(stats.chunks_ticked, TOTAL_CHUNKS);

        let mut events = std::mem::take(&mut *lock(&outbox));
        events.sort_unstable();
        for (rx, rz, seq, payload) in events {
            dim.set_block(commit_block(rx, rz, seq, false), payload as u32);
            dim.set_block(commit_block(rx, rz, seq, true), (payload >> 32) as u32);
        }
    }
    state_hash(&dim, &chunks)
}

#[test]
fn p1_four_wave_region_tick_is_bit_identical_1_vs_8_threads_over_200_ticks() {
    let single = MeguTaskPool::new(PoolTopologyConfig::new(1, 0));
    let wide = MeguTaskPool::new(PoolTopologyConfig::new(8, 0));
    assert_eq!(single.interactive_threads(), 1);
    assert_eq!(wide.interactive_threads(), 8);

    let hash_1t = run_scenario(&single, TICKS);
    let hash_8t = run_scenario(&wide, TICKS);

    assert_eq!(
        hash_1t, hash_8t,
        "1-worker and 8-worker pools must produce a bit-identical final state"
    );
}

/// Deterministic mob fixture shared by the P2 lock-free step check.
fn mob_table() -> RegionMobTable {
    let mut table = RegionMobTable::default();
    for i in 0..512usize {
        table.push(
            2000 + i as i32,
            (i % 11) as u16,
            [i as f64 * 0.5, (i % 5) as f64 * 0.3, -(i as f64)],
            [
                ((i % 7) as f32 - 3.0) * 0.1,
                0.0,
                ((i % 5) as f32 - 2.0) * 0.05,
            ],
            [i as f32 * 0.02, (i % 3) as f32 * 0.1],
            20.0,
            // Every 17th mob is a 2-tick fuse: it dies early, proving the
            // alive_mask actually gates `step`.
            [if i % 17 == 0 { 2 } else { 0 }, (i % 11) as i16, 300],
        );
    }
    table
}

#[test]
fn p2_resident_pool_reuses_workers_and_region_mob_table_steps_lockfree() {
    let pool = MeguTaskPool::new(PoolTopologyConfig::new(3, 1));
    assert_eq!(pool.interactive_threads(), 3);
    assert_eq!(pool.background_threads(), 1);
    let residents = pool.interactive_threads() + pool.background_threads();
    // `live_workers` is bumped inside `worker_main`, so it can lag `new()`
    // by a scheduler quantum; it can never exceed the configured count.
    assert!(pool.live_workers() <= residents);
    // Bounded wait for every spawned worker to register — deterministic
    // because `MeguTaskPool::new` already joined nothing and the threads
    // are runnable; this only absorbs scheduler latency.
    for _ in 0..10_000_000 {
        if pool.live_workers() == residents {
            break;
        }
        std::hint::spin_loop();
    }
    assert_eq!(pool.live_workers(), residents);

    // Resident reuse: identical tick workloads must produce an identical
    // `tasks_executed` delta every round — no per-wave thread churn.
    let dim = DimensionState::new("minecraft:overworld");
    let chunks = grid_chunks();
    let mut deltas = Vec::new();
    let mut previous = pool.tasks_executed();
    for _ in 0..8 {
        tick_regions_in_pool(&pool, &dim, &chunks, TICK_THREADS, |_d, _r, _s| {});
        let now = pool.tasks_executed();
        deltas.push(now - previous);
        previous = now;
    }
    assert!(deltas.iter().all(|&delta| delta > 0), "deltas={deltas:?}");
    assert!(
        deltas.iter().all(|&delta| delta == deltas[0]),
        "tasks_executed must grow by a clean constant delta, got {deltas:?}"
    );
    assert_eq!(
        pool.live_workers(),
        residents,
        "workers must be reused, not respawned"
    );

    // RegionMobTable: 512 mobs stepped lock-free, two independent tables on
    // two concurrent scoped jobs must stay bit-identical.
    let mut left = mob_table();
    let mut right = mob_table();
    assert_eq!(left.len(), 512);
    assert_eq!(left.alive_mask.len(), 512 / 64);
    assert_eq!(left.alive_count(), 512);

    pool.scope_blocking(TaskLane::Interactive, |scope| {
        scope.spawn(|| {
            for tick in 0..16u64 {
                left.step(tick);
            }
        });
        scope.spawn(|| {
            for tick in 0..16u64 {
                right.step(tick);
            }
        });
    });

    let expected_alive = (0..512usize).filter(|i| i % 17 != 0).count();
    assert_eq!(left.alive_count(), expected_alive);
    assert_eq!(
        left, right,
        "concurrent lock-free steps must be bit-identical"
    );
    assert!(
        left.pos_y.iter().all(|&y| y >= 0.0),
        "ground plane must hold"
    );
    for index in 0..left.len() {
        assert_eq!(left.alive(index), index % 17 != 0, "slot {index}");
    }
}
