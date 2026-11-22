// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! Concurrency, determinism and low-spec topology tests for
//! `mc_spine_core::task_pool`.

use std::hint::spin_loop;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use mc_spine_core::hw_adaptive_engine::{
    CacheDomainTopology, CoreHeterogeneity, ExtendedSimdTier, GpuComputeParadigm, HostHardwareCaps,
    MemoryFootprintClass, MemoryTopology, PowerThermalProfile, SimdKernelTier, StorageIoClass,
    UniversalHardwareProfile,
};
use mc_spine_core::task_pool::{MeguTaskPool, PoolTopologyConfig, TaskLane};

/// Bounded spin-wait: returns as soon as `condition` holds, `false` after
/// a generous iteration budget so a scheduling bug fails instead of
/// hanging the suite.
fn wait_until(mut condition: impl FnMut() -> bool) -> bool {
    for _ in 0..20_000_000u32 {
        if condition() {
            return true;
        }
        spin_loop();
    }
    condition()
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

// ---------------------------------------------------------------------------
// 1. Dual-lane preemption: Lane A never queues behind a saturated Lane B
// ---------------------------------------------------------------------------

#[test]
fn task_pool_dual_lane_interactive_preempts_background_and_steals_work() {
    let pool = MeguTaskPool::new(PoolTopologyConfig::new(2, 1));
    assert_eq!(pool.interactive_threads(), 2);
    assert_eq!(pool.background_threads(), 1);

    let release = Arc::new(AtomicBool::new(false));
    let interactive_done = Arc::new(AtomicUsize::new(0));

    // Saturate the single background worker: the first job spins on
    // `release`, so every queued job is provably stuck behind it for the
    // duration of the interactive burst.
    for _ in 0..64 {
        let release = Arc::clone(&release);
        pool.spawn(TaskLane::Background, move || {
            while !release.load(Ordering::Acquire) {
                spin_loop();
            }
        });
    }

    // High-priority interactive tasks must complete without waiting for a
    // single background job.
    for _ in 0..8 {
        let done = Arc::clone(&interactive_done);
        pool.spawn(TaskLane::Interactive, move || {
            done.fetch_add(1, Ordering::AcqRel);
        });
    }
    assert!(
        wait_until(|| interactive_done.load(Ordering::Acquire) == 8),
        "interactive lane starved behind a saturated background lane",
    );

    // Free lane B before the owner-blocking scoped burst: a help-stealing
    // caller is allowed to execute foreign-lane work, so an unreleased
    // spinner could legitimately occupy it — the preemption property is
    // already proven above while lane B was saturated.
    release.store(true, Ordering::Release);

    // A scoped interactive burst over the shared injector exercises the
    // work-stealing/help path deterministically.
    let cells: Vec<AtomicU64> = (0..32).map(|i| AtomicU64::new(i as u64)).collect();
    pool.par_for_each(TaskLane::Interactive, &cells, |index, cell| {
        cell.fetch_xor(index as u64, Ordering::Relaxed);
    });
    for cell in &cells {
        assert_eq!(cell.load(Ordering::Relaxed), 0);
    }

    // 64 background + 8 interactive spawns + 8 par_for_each chunk jobs
    // (min(32, 2 workers * 4)).
    let expected = 64 + 8 + 8;
    assert!(
        wait_until(|| pool.tasks_executed() == expected),
        "pool executed {} of {expected} submitted tasks",
        pool.tasks_executed(),
    );
    assert_eq!(pool.tasks_executed(), expected);
    assert!(
        pool.tasks_stolen() >= 1,
        "expected off-deque (injector/steal) executions to be recorded",
    );
}

// ---------------------------------------------------------------------------
// 2. Scoped stack borrows are bit-identical to serial on any pool size
// ---------------------------------------------------------------------------

fn wave_mix(index: usize, wave: u64, mut x: u64) -> u64 {
    x ^= (index as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15) ^ wave;
    x ^= x >> 30;
    x = x.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    x ^= x >> 27;
    x = x.wrapping_mul(0x94d0_49bb_1331_11eb);
    x ^ (x >> 31)
}

const REGION_LEN: usize = 2048;
const CHUNK: usize = 128;
const TICKS: usize = 200;
const WAVES: u64 = 4;

fn serial_tick(data: &mut [u64]) {
    for wave in 0..WAVES {
        for (index, value) in data.iter_mut().enumerate() {
            *value = wave_mix(index, wave, *value);
        }
    }
}

#[test]
fn task_pool_scope_blocking_borrows_stack_and_matches_serial_bit_for_bit() {
    let seed: Vec<u64> = (0..REGION_LEN as u64)
        .map(|i| i.wrapping_mul(0x2545_f491_4f6c_dd1d) ^ 0xdead_beef)
        .collect();

    let mut expected = seed.clone();
    for _ in 0..TICKS {
        serial_tick(&mut expected);
    }

    for cfg in [PoolTopologyConfig::new(1, 0), PoolTopologyConfig::new(8, 0)] {
        let pool = MeguTaskPool::new(cfg);
        let live = pool.live_worker_counter();
        let total = cfg.interactive_threads + cfg.background_threads;
        assert!(
            wait_until(|| live.load(Ordering::SeqCst) == total),
            "workers failed to start",
        );

        // 200 ticks x 4 waves: every wave is a scope borrowing `data`'s
        // stack storage via `chunks_mut`, exactly like a 4-wave region
        // tick fan-out.
        let mut data = seed.clone();
        for _ in 0..TICKS {
            for wave in 0..WAVES {
                pool.scope_blocking(TaskLane::Interactive, |s| {
                    for (chunk_index, chunk) in data.chunks_mut(CHUNK).enumerate() {
                        s.spawn(move || {
                            for (offset, value) in chunk.iter_mut().enumerate() {
                                let index = chunk_index * CHUNK + offset;
                                *value = wave_mix(index, wave, *value);
                            }
                        });
                    }
                });
            }
        }
        assert_eq!(
            data, expected,
            "scope_blocking output diverged from serial on cfg {cfg:?}",
        );

        // par_for_each deterministic index mapping is also bit-identical.
        let out: Vec<AtomicU64> = (0..REGION_LEN).map(|_| AtomicU64::new(0)).collect();
        pool.par_for_each(TaskLane::Interactive, &data, |index, value| {
            out[index].store(wave_mix(index, 7, *value), Ordering::Relaxed);
        });
        for (index, value) in data.iter().enumerate() {
            assert_eq!(
                out[index].load(Ordering::Relaxed),
                wave_mix(index, 7, *value)
            );
        }

        drop(pool);
        assert!(
            wait_until(|| live.load(Ordering::SeqCst) == 0),
            "pool dropped with live worker threads",
        );
    }
}

// ---------------------------------------------------------------------------
// 3. Gate P5: hardware-tier sizing + 2-core (1,1) help-stealing
// ---------------------------------------------------------------------------

fn profile(logical_cores: u32, core_layout: CoreHeterogeneity) -> UniversalHardwareProfile {
    let hybrid_cores = matches!(core_layout, CoreHeterogeneity::Hybrid { .. });
    UniversalHardwareProfile {
        base: HostHardwareCaps {
            logical_cores,
            l3_cache_kib: 0,
            hybrid_cores,
            simd_tier: SimdKernelTier::Scalar,
            memory_topology: MemoryTopology::UnifiedUma,
        },
        l2_per_core_kib: 0,
        cache_line_bytes: 64,
        cache_domains: CacheDomainTopology::SingleSharedLlc,
        core_layout,
        total_ram_mib: 4096,
        memory_class: MemoryFootprintClass::Constrained,
        gpu_paradigm: GpuComputeParadigm::HeadlessNoGpu,
        power_profile: PowerThermalProfile::ServerHeadless,
        storage_io: StorageIoClass::SataSsd,
        extended_simd: ExtendedSimdTier::Scalar,
    }
}

#[test]
fn task_pool_p5_two_core_lowspec_topology_never_overallocates_or_deadlocks() {
    // Tier-0: 2 logical cores must never oversubscribe — one worker per
    // lane so the OS scheduler always has a free context.
    let tier0 = PoolTopologyConfig::from_hardware_profile(&profile(
        2,
        CoreHeterogeneity::Uniform { cores: 2 },
    ));
    assert_eq!(tier0, PoolTopologyConfig::new(1, 1));

    let single = PoolTopologyConfig::from_hardware_profile(&profile(
        1,
        CoreHeterogeneity::Uniform { cores: 1 },
    ));
    assert_eq!(single, PoolTopologyConfig::new(1, 1));

    // Tier-1: 4 cores.
    let tier1 = PoolTopologyConfig::from_hardware_profile(&profile(
        4,
        CoreHeterogeneity::Uniform { cores: 4 },
    ));
    assert_eq!(tier1, PoolTopologyConfig::new(2, 1));

    // Hybrid parts map lanes onto physical clusters: Apple M3-class
    // (4P + 4E) and Intel Raptor-Lake-class (8P + 8E).
    let hybrid_m3 = PoolTopologyConfig::from_hardware_profile(&profile(
        8,
        CoreHeterogeneity::Hybrid {
            p_cores: 4,
            e_cores: 4,
        },
    ));
    assert_eq!(hybrid_m3, PoolTopologyConfig::new(3, 4));

    let hybrid_raptor = PoolTopologyConfig::from_hardware_profile(&profile(
        24,
        CoreHeterogeneity::Hybrid {
            p_cores: 8,
            e_cores: 8,
        },
    ));
    assert_eq!(hybrid_raptor, PoolTopologyConfig::new(7, 8));

    // Tier-4: 16 and 32 uniform cores scale without any worker cap.
    let tier4 = PoolTopologyConfig::from_hardware_profile(&profile(
        16,
        CoreHeterogeneity::Uniform { cores: 16 },
    ));
    assert_eq!(tier4, PoolTopologyConfig::new(12, 4));
    let wide = PoolTopologyConfig::from_hardware_profile(&profile(
        32,
        CoreHeterogeneity::Uniform { cores: 32 },
    ));
    assert_eq!(wide, PoolTopologyConfig::new(24, 8));

    // A real (1, 1) pool must execute nested/recursive scoped fan-out
    // without deadlocking: the only Lane-A worker runs a job that itself
    // blocks on nested Lane-A *and* Lane-B scoped work, so only caller
    // help-stealing can drain it.
    let pool = MeguTaskPool::new(tier0);
    assert_eq!(pool.interactive_threads(), 1);
    assert_eq!(pool.background_threads(), 1);
    let handle = pool.handle();
    let sums = Arc::new(Mutex::new(Vec::new()));

    pool.scope_blocking(TaskLane::Interactive, |s| {
        for _wave in 0..4 {
            let handle = handle.clone();
            let sums = Arc::clone(&sums);
            s.spawn(move || {
                // Cross-lane nested scope: worker A waits on lane-B jobs
                // and can only finish by help-stealing or via worker B.
                let inner_b = AtomicUsize::new(0);
                handle.scope_blocking(TaskLane::Background, |is| {
                    for _ in 0..3 {
                        is.spawn(|| {
                            inner_b.fetch_add(1, Ordering::SeqCst);
                        });
                    }
                });
                // Same-lane nested scope: jobs land on this very worker's
                // private deque and are drained by its own help loop.
                let inner_a = AtomicUsize::new(0);
                handle.scope_blocking(TaskLane::Interactive, |is| {
                    for _ in 0..5 {
                        is.spawn(|| {
                            inner_a.fetch_add(1, Ordering::SeqCst);
                        });
                    }
                });
                lock(&sums).push((
                    inner_b.load(Ordering::SeqCst),
                    inner_a.load(Ordering::SeqCst),
                ));
            });
        }
    });

    let mut results = lock(&sums).clone();
    results.sort_unstable();
    assert_eq!(results, vec![(3, 5); 4]);
    assert!(pool.tasks_executed() >= 4 + 4 * (3 + 5));
}
