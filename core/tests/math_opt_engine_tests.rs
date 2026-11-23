// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! Accuracy, convergence, determinism and fail-closed tests for
//! `megu_core::math_opt_engine`'s four mathematical engines.

use megu_core::hw_adaptive_engine::{
    build_unified_optimization_plan, CacheDomainTopology, CoreHeterogeneity, CpuVendor,
    ExtendedSimdTier, GpuComputeParadigm, HostHardwareCaps, MemoryFootprintClass, MemoryTopology,
    PowerThermalProfile, SimdKernelTier, StorageIoClass, UniversalHardwareProfile,
};
use megu_core::math_opt_engine::{
    morton3d_decode, morton3d_encode, morton3d_signed, projected_sphere_diameter_px,
    DiscountedUcbAutotuner, P2QuantileEstimator, PidBudgetController, RooflineAdvice,
    RooflineModel, RooflineVerdict, WelfordStats, MORTON3D_AXIS_MAX, MORTON3D_SIGNED_BIAS,
};
use megu_core::perf_kit::splitmix64;
use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::atomic::{AtomicUsize, Ordering};

// Per-thread heap-counting allocator: proves the streaming estimators allocate
// zero bytes per sample even when sibling tests run in parallel on other threads.
static HEAP_ALLOCATIONS: AtomicUsize = AtomicUsize::new(0);

thread_local! {
    static COUNT_THIS_THREAD: Cell<bool> = const { Cell::new(false) };
}

struct CountingAllocator;

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if COUNT_THIS_THREAD.try_with(Cell::get).unwrap_or(false) {
            HEAP_ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        }
        System.alloc(layout)
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        System.dealloc(ptr, layout)
    }
}

#[global_allocator]
static GLOBAL: CountingAllocator = CountingAllocator;

/// Deterministic uniform `f64` in `[0, 1)` from a splitmix64 stream.
fn next_unit(state: &mut u64) -> f64 {
    *state = splitmix64(*state);
    (*state >> 11) as f64 / 9_007_199_254_740_992.0 // 2^53
}

/// Deterministic uniform `f64` in `[lo, hi)`.
fn next_range(state: &mut u64, lo: f64, hi: f64) -> f64 {
    lo + (hi - lo) * next_unit(state)
}

fn profile(
    logical_cores: u32,
    l3_cache_kib: u32,
    extended_simd: ExtendedSimdTier,
    gpu_paradigm: GpuComputeParadigm,
    power_profile: PowerThermalProfile,
) -> UniversalHardwareProfile {
    UniversalHardwareProfile {
        base: HostHardwareCaps {
            logical_cores,
            l3_cache_kib,
            hybrid_cores: false,
            simd_tier: SimdKernelTier::Avx2,
            memory_topology: MemoryTopology::DiscreteVram,
        },
        l2_per_core_kib: 1024,
        cache_line_bytes: 64,
        cache_domains: CacheDomainTopology::SingleSharedLlc,
        core_layout: CoreHeterogeneity::Uniform {
            cores: logical_cores,
        },
        total_ram_mib: 32 * 1024,
        memory_class: MemoryFootprintClass::Abundant,
        gpu_paradigm,
        power_profile,
        storage_io: StorageIoClass::NvmeFastRandom,
        extended_simd,
        cpu_vendor: CpuVendor::Other,
    }
}

// ===========================================================================
// 1. Discounted UCB non-stationary bandit
// ===========================================================================

#[test]
fn ducb_explores_every_untried_arm_in_index_order_first() {
    let mut tuner = DiscountedUcbAutotuner::<6>::new();
    for expected in 0..6 {
        assert_eq!(tuner.select_arm(), expected);
        tuner.observe(expected, 100_000);
    }
}

#[test]
fn ducb_converges_to_the_fastest_arm() {
    let mut tuner = DiscountedUcbAutotuner::<3>::new();
    let latency_ns = [50_000u64, 200_000, 500_000];
    let mut state = 0x5eed_u64;
    for _ in 0..300 {
        let arm = tuner.select_arm();
        let jitter = next_range(&mut state, 0.9, 1.1);
        tuner.observe(arm, (latency_ns[arm] as f64 * jitter) as u64);
    }
    let best = (0..3)
        .max_by(|&a, &b| {
            tuner
                .mean_reward(a)
                .partial_cmp(&tuner.mean_reward(b))
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .unwrap();
    assert_eq!(best, 0, "arm 0 is the fastest kernel and must win");
    // Reward ordering r = 1/(1 + lat/scale): the 50 us arm earns ~0.952.
    let mean = tuner.mean_reward(0).expect("arm 0 has samples");
    assert!((0.90..=0.99).contains(&mean), "mean reward {mean}");
}

#[test]
fn ducb_detects_regime_switch_and_moves_to_new_fastest_arm() {
    // Six kernel arms: Scalar, SwarRecompute, SimdWide, LutBatch,
    // GpuIndirect, GpuMeshShader. Phase A is a sparse scene where Scalar
    // wins; phase B simulates a dense-factory scene change plus thermal
    // throttle that makes Scalar 50x slower and SimdWide the fastest.
    let mut tuner = DiscountedUcbAutotuner::<6>::new();
    let phase_a = [40_000u64, 400_000, 800_000, 300_000, 600_000, 150_000];
    let phase_b = [2_000_000u64, 400_000, 60_000, 300_000, 900_000, 150_000];
    let mut state = 0xba5e_u64;
    for _ in 0..300 {
        let arm = tuner.select_arm();
        tuner.observe(
            arm,
            (phase_a[arm] as f64 * next_range(&mut state, 0.9, 1.1)) as u64,
        );
    }
    let best_a = (0..6).max_by(|&a, &b| {
        tuner
            .mean_reward(a)
            .partial_cmp(&tuner.mean_reward(b))
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    assert_eq!(best_a, Some(0), "pre-switch fastest arm must lead");

    let mut picks = [0usize; 6];
    for _ in 0..400 {
        let arm = tuner.select_arm();
        tuner.observe(
            arm,
            (phase_b[arm] as f64 * next_range(&mut state, 0.9, 1.1)) as u64,
        );
        picks[arm] += 1;
    }
    let best_b = (0..6).max_by(|&a, &b| {
        tuner
            .mean_reward(a)
            .partial_cmp(&tuner.mean_reward(b))
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    assert_eq!(
        best_b,
        Some(2),
        "bandit must rank the post-switch fastest arm first"
    );
    assert!(
        picks[2] > picks[0],
        "new optimum ({} plays) must outplay the stale winner ({} plays)",
        picks[2],
        picks[0]
    );
    assert_eq!(
        picks
            .iter()
            .position(|&n| n == *picks.iter().max().unwrap()),
        Some(2),
        "arm 2 must be the single most-played arm after the switch: {picks:?}"
    );
}

#[test]
fn ducb_rejects_out_of_range_and_garbage_samples() {
    let mut tuner = DiscountedUcbAutotuner::<4>::new();
    tuner.observe(0, 100_000);
    let counts: Vec<f64> = (0..4).map(|a| tuner.discounted_count(a)).collect();
    let pulls: Vec<u64> = (0..4).map(|a| tuner.pulls(a)).collect();

    tuner.observe(4, 100_000); // arm >= K
    tuner.observe(usize::MAX, 100_000);
    tuner.observe(1, 0); // zero nanoseconds is not a real sample
    tuner.observe(1, 60_000_000_001); // above the 60 s sanity bound
    tuner.observe(1, u64::MAX);
    for arm in 0..4 {
        assert_eq!(tuner.discounted_count(arm), counts[arm]);
        assert_eq!(tuner.pulls(arm), pulls[arm]);
    }
}

#[test]
fn ducb_is_fully_deterministic_for_identical_streams() {
    let mut first = DiscountedUcbAutotuner::<4>::new();
    let mut second = DiscountedUcbAutotuner::<4>::new();
    let mut state = 0xdec0de_u64;
    for _ in 0..500 {
        let arm = first.select_arm();
        assert_eq!(arm, second.select_arm());
        let ns = (next_range(&mut state, 10_000.0, 900_000.0)) as u64;
        first.observe(arm, ns);
        second.observe(arm, ns);
    }
    for arm in 0..4 {
        assert_eq!(first.discounted_count(arm), second.discounted_count(arm));
        assert_eq!(first.mean_reward(arm), second.mean_reward(arm));
    }
}

#[test]
fn ducb_decays_stale_statistics_geometrically() {
    let mut tuner = DiscountedUcbAutotuner::<2>::new();
    tuner.observe(0, 100_000);
    let n0 = tuner.discounted_count(0);
    assert!((n0 - 1.0).abs() < 1e-12);
    tuner.observe(1, 100_000);
    let n1 = tuner.discounted_count(0);
    assert!(
        (n1 - n0 * 0.98).abs() < 1e-12,
        "gamma discount must shrink the stale count to gamma*n, got {n1}"
    );
}

// ===========================================================================
// 2. P² streaming quantiles + Welford online moments
// ===========================================================================

/// Heavy-tailed frame-time stream in milliseconds: an exponential tail
/// base (`8 - 8 ln(1-u)`, mean 16 ms) with 1% spike outliers overlapping
/// the tail at 30-45 ms — the classic jitter-spike pattern a 1%-low FPS
/// metric exists to catch.
fn heavy_tailed_frame_times(n: usize, seed: u64) -> Vec<f64> {
    let mut state = seed;
    (0..n)
        .map(|_| {
            if next_unit(&mut state) < 0.99 {
                let u = next_unit(&mut state).min(1.0 - 1e-9);
                8.0 - 8.0 * (1.0 - u).ln()
            } else {
                next_range(&mut state, 30.0, 45.0)
            }
        })
        .collect()
}

fn exact_quantile(sorted: &[f64], p: f64) -> f64 {
    let idx = (p * (sorted.len() - 1) as f64).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

#[test]
fn p2_tracks_p50_p95_p99_within_two_percent_of_sorted_truth() {
    let mut samples = heavy_tailed_frame_times(10_000, 0xc0ffee);
    let mut p50 = P2QuantileEstimator::new(0.50);
    let mut p95 = P2QuantileEstimator::new(0.95);
    let mut p99 = P2QuantileEstimator::new(0.99);
    for &x in &samples {
        p50.observe(x);
        p95.observe(x);
        p99.observe(x);
    }
    samples.sort_by(f64::total_cmp);
    for (est, p) in [(&p50, 0.50), (&p95, 0.95), (&p99, 0.99)] {
        let truth = exact_quantile(&samples, p);
        let got = est.estimate().expect("estimator primed");
        let rel = (got - truth).abs() / truth;
        assert!(
            rel < 0.02,
            "p{} estimate {got} vs exact {truth}: {:.4} relative error",
            (p * 100.0) as u32,
            rel
        );
    }
}

#[test]
fn p2_uses_constant_memory_and_zero_heap_per_sample() {
    assert!(
        std::mem::size_of::<P2QuantileEstimator>() <= 160,
        "five markers must stay O(1): {} bytes",
        std::mem::size_of::<P2QuantileEstimator>()
    );
    let samples = heavy_tailed_frame_times(10_000, 0xf00d);
    let mut p99 = P2QuantileEstimator::new(0.99);
    p99.observe(10.0); // settle bootstrap outside the measured window
    let before = HEAP_ALLOCATIONS.load(Ordering::Relaxed);
    COUNT_THIS_THREAD.with(|flag| flag.set(true));
    for &x in &samples {
        p99.observe(x);
    }
    COUNT_THIS_THREAD.with(|flag| flag.set(false));
    let after = HEAP_ALLOCATIONS.load(Ordering::Relaxed);
    assert_eq!(
        after - before,
        0,
        "P2 observe loop must perform zero heap allocations"
    );
}

#[test]
fn p2_rejects_nonfinite_and_tracks_min_max() {
    let mut est = P2QuantileEstimator::new(0.5);
    for x in [5.0, 1.0, 9.0, 3.0, 7.0] {
        est.observe(x);
    }
    assert_eq!(est.estimate(), Some(5.0));
    assert_eq!(est.minimum(), Some(1.0));
    assert_eq!(est.maximum(), Some(9.0));
    est.observe(f64::NAN);
    est.observe(f64::INFINITY);
    est.observe(f64::NEG_INFINITY);
    assert_eq!(est.count(), 5, "non-finite samples must be rejected");
    assert_eq!(est.minimum(), Some(1.0));
    assert_eq!(est.maximum(), Some(9.0));
    // A new real extreme updates the boundary marker.
    est.observe(42.0);
    assert_eq!(est.maximum(), Some(42.0));
}

#[test]
fn p2_monotone_markers_never_cross_on_adversarial_input() {
    // Sorted, reverse-sorted and constant streams are the stress cases
    // for the parabolic/linear marker updates.
    let mut est = P2QuantileEstimator::new(0.99);
    let mut state = 0x1eaf;
    let feed = |est: &mut P2QuantileEstimator, v: f64| {
        est.observe(v);
        if est.count() >= 5 {
            let q = [
                est.minimum().unwrap(),
                est.estimate().unwrap(),
                est.maximum().unwrap(),
            ];
            assert!(q[0] <= q[1] && q[1] <= q[2], "markers inverted: {q:?}");
        }
    };
    for i in 0..5_000 {
        feed(&mut est, i as f64);
    }
    for i in (0..5_000).rev() {
        feed(&mut est, i as f64);
    }
    for _ in 0..5_000 {
        feed(&mut est, next_range(&mut state, -50.0, 50.0));
    }
}

#[test]
fn welford_matches_naive_mean_and_variance() {
    let mut stats = WelfordStats::new();
    let data: Vec<f64> = (1..=1_000).map(|i| i as f64).collect();
    for &x in &data {
        stats.observe(x);
    }
    let n = data.len() as f64;
    let mean = data.iter().sum::<f64>() / n;
    let pop_var = data.iter().map(|x| (x - mean) * (x - mean)).sum::<f64>() / n;
    let sample_var = data.iter().map(|x| (x - mean) * (x - mean)).sum::<f64>() / (n - 1.0);
    assert!((stats.mean().unwrap() - mean).abs() < 1e-9);
    assert!((stats.variance().unwrap() - pop_var).abs() < 1e-6);
    assert!((stats.sample_variance().unwrap() - sample_var).abs() < 1e-6);
    assert_eq!(stats.count(), 1_000);
    stats.observe(f64::NAN);
    stats.observe(f64::INFINITY);
    assert_eq!(stats.count(), 1_000, "non-finite must be rejected");
    assert!(WelfordStats::new().mean().is_none());
}

// ===========================================================================
// 3. Roofline model + PID budget controller
// ===========================================================================

#[test]
fn roofline_derives_peak_bandwidth_and_ridge_from_profile() {
    // 16-core AVX2 desktop on AC power with a discrete GPU:
    // Pi = 16 cores x 16 FLOP/cycle x 4.2 GHz = 1075.2 GFLOP/s,
    // beta = 25 GB/s PCIe staging, ridge = 43.008 FLOP/byte.
    let desktop = profile(
        16,
        32 * 1024,
        ExtendedSimdTier::Avx2,
        GpuComputeParadigm::DiscretePcieHighBw,
        PowerThermalProfile::AcDesktop,
    );
    let model = RooflineModel::from_profile(&desktop);
    assert!((model.peak_gflops - 16.0 * 16.0 * 4.2).abs() < 1e-9);
    assert!((model.bandwidth_gbs - 25.0).abs() < 1e-12);
    assert!((model.ridge_intensity - model.peak_gflops / 25.0).abs() < 1e-9);

    // The plan's roofline projection is the same model — no drift.
    let plan = build_unified_optimization_plan(&desktop, None);
    assert_eq!(plan.roofline_model(), model);

    // Hybrid layout counts E-cores at half weight.
    let mut hybrid = desktop;
    hybrid.core_layout = CoreHeterogeneity::Hybrid {
        p_cores: 8,
        e_cores: 8,
    };
    let hybrid_model = RooflineModel::from_profile(&hybrid);
    assert!((hybrid_model.peak_gflops - (8.0 + 4.0) * 16.0 * 4.2).abs() < 1e-9);
}

#[test]
fn roofline_classifies_memory_vs_compute_bound() {
    let desktop = profile(
        16,
        32 * 1024,
        ExtendedSimdTier::Avx2,
        GpuComputeParadigm::DiscretePcieHighBw,
        PowerThermalProfile::AcDesktop,
    );
    let model = RooflineModel::from_profile(&desktop);
    let ridge = model.ridge_intensity;

    // Light arithmetic per streamed byte: well left of the ridge.
    let light = model.classify_kernel(10.0, 100.0);
    match light {
        RooflineVerdict::MemoryBound {
            attainable_gflops,
            ridge_ratio,
        } => {
            assert!((attainable_gflops - 0.1 * 25.0).abs() < 1e-9);
            assert!(ridge_ratio < 1.0);
            assert!((ridge_ratio - 0.1 / ridge).abs() < 1e-12);
        }
        other => panic!("expected MemoryBound, got {other:?}"),
    }

    // Dense compute per byte: right of the ridge, capped by Pi.
    let dense = model.classify_kernel(2_000.0, 16.0);
    match dense {
        RooflineVerdict::ComputeBound {
            attainable_gflops,
            ridge_ratio,
        } => {
            assert!((attainable_gflops - model.peak_gflops).abs() < 1e-9);
            assert!(ridge_ratio > 1.0);
        }
        other => panic!("expected ComputeBound, got {other:?}"),
    }

    // Invalid intensity fails closed to ComputeBound.
    assert!(matches!(
        model.classify_kernel(f64::NAN, 8.0),
        RooflineVerdict::ComputeBound { .. }
    ));
    assert!(matches!(
        model.classify_kernel(10.0, 0.0),
        RooflineVerdict::ComputeBound { .. }
    ));
}

#[test]
fn roofline_recommends_offload_recompute_or_lut() {
    let desktop = profile(
        16,
        32 * 1024,
        ExtendedSimdTier::Avx2,
        GpuComputeParadigm::DiscretePcieHighBw,
        PowerThermalProfile::AcDesktop,
    );
    let model = RooflineModel::from_profile(&desktop);

    // Deeply memory-bound bulk-transfer kernel on a discrete host.
    assert_eq!(
        model.recommended_strategy(4.0, 256.0),
        RooflineAdvice::GpuOffload
    );
    // Memory-bound but too small to amortize PCIe staging.
    assert_eq!(
        model.recommended_strategy(4.0, 32.0),
        RooflineAdvice::RecomputeOverBandwidth
    );
    // Near-ridge memory-bound on a large-L3 host keeps tables resident.
    let near_ridge = model.ridge_intensity * 0.75;
    assert_eq!(
        model.recommended_strategy(near_ridge * 32.0, 32.0),
        RooflineAdvice::LutCacheBatch
    );
    // Compute-bound always batches LUTs.
    assert_eq!(
        model.recommended_strategy(4_000.0, 16.0),
        RooflineAdvice::LutCacheBatch
    );

    // Headless profile can never advise GPU offload.
    let headless = profile(
        32,
        8 * 1024,
        ExtendedSimdTier::Avx2,
        GpuComputeParadigm::HeadlessNoGpu,
        PowerThermalProfile::ServerHeadless,
    );
    let headless_model = RooflineModel::from_profile(&headless);
    for (flops, bytes) in [(1.0, 256.0), (4.0, 512.0), (0.5, 128.0)] {
        assert_ne!(
            headless_model.recommended_strategy(flops, bytes),
            RooflineAdvice::GpuOffload
        );
    }
}

#[test]
fn pid_converges_monotonically_to_budget_under_step_load() {
    // Plant: measured_ms = scale * offered_demand — the budget scale
    // linearly shrinks the chunk-upload/shadow/LOD work per frame.
    let target = 16.67_f64;
    let mut pid = PidBudgetController::new(target);
    let mut errors = Vec::with_capacity(300);
    for step in 0..300 {
        let demand = if step < 30 { 8.0 } else { 24.0 };
        let measured = pid.budget_scale() * demand;
        pid.update(measured);
        errors.push((measured - target) / target);
    }
    // Critically damped: at most a single sign change after the step.
    let crossings = errors[31..]
        .windows(2)
        .filter(|w| w[0].signum() != w[1].signum() && w[0] != 0.0)
        .count();
    assert!(
        crossings <= 2,
        "oscillating controller: {crossings} crossings"
    );
    // Converged: last 50 frames inside +-2% of target.
    assert!(
        errors[250..].iter().all(|e| e.abs() < 0.02),
        "did not settle: {:?}",
        &errors[280..]
    );
    // Steady-state scale is the exact budget ratio target/demand.
    let expected_scale = target / 24.0;
    assert!(
        (pid.budget_scale() - expected_scale).abs() < 0.02,
        "scale {} vs expected {expected_scale}",
        pid.budget_scale()
    );
}

#[test]
fn pid_anti_windup_keeps_integral_bounded_and_recovers() {
    // Offered load so light that even full budget cannot reach target:
    // the output saturates high and conditional integration must freeze.
    let target = 16.67_f64;
    let mut pid = PidBudgetController::new(target);
    for _ in 0..200 {
        pid.update(pid.budget_scale() * 8.0);
    }
    assert_eq!(pid.budget_scale(), 1.0, "output must saturate at max");
    assert!(
        pid.integral().abs() < 0.5,
        "integral must not wind while saturated: {}",
        pid.integral()
    );
    // A heavy disturbance arrives: recovery is immediate, not delayed by
    // a deeply-wound integrator.
    for step in 0..80 {
        let measured = pid.budget_scale() * 40.0;
        pid.update(measured);
        if step >= 60 {
            assert!(
                ((measured - target) / target).abs() < 0.02
                    || (pid.budget_scale() * 40.0 - target).abs() / target < 0.02,
                "still off target at step {step}"
            );
        }
    }
    assert!(pid.integral().abs() <= 4.0, "integral clamp exceeded");

    // The low-saturation direction is bounded too: 600 ms of offered
    // load pins the scale at min while demanding still more reduction.
    pid.reset();
    for _ in 0..200 {
        let measured = pid.budget_scale() * 600.0;
        pid.update(measured);
    }
    assert!(pid.integral().abs() <= 4.0);
    // Load falls back into the reachable range; recovery converges.
    for step in 0..80 {
        let measured = pid.budget_scale() * 20.0;
        pid.update(measured);
        if step >= 60 {
            let e = (pid.budget_scale() * 20.0 - target).abs() / target;
            assert!(e < 0.02, "low-side recovery failed, residual {e:.3}");
        }
    }
}

#[test]
fn pid_rejects_invalid_measurements_and_parameters() {
    let mut pid = PidBudgetController::new(f64::NAN);
    assert!((pid.target_ms() - 1_000.0 / 60.0).abs() < 1e-9);
    let before = pid.budget_scale();
    assert_eq!(pid.update(f64::NAN), before);
    assert_eq!(pid.update(f64::INFINITY), before);
    assert_eq!(pid.update(0.0), before);
    assert_eq!(pid.update(-3.0), before);
    // Ordering of scale bounds is sanitized.
    let pid2 = PidBudgetController::with_gains(16.67, -1.0, f64::NAN, 2.0, 0.9, 0.1);
    assert!(pid2.budget_scale() <= 1.0 && pid2.budget_scale() > 0.0);
}

// ===========================================================================
// 4. Morton Z-order + analytic sphere projection
// ===========================================================================

#[test]
fn morton3d_interleaves_single_bits_at_exact_positions() {
    for bit in 0..21 {
        assert_eq!(morton3d_encode(1 << bit, 0, 0), 1u64 << (3 * bit));
        assert_eq!(morton3d_encode(0, 1 << bit, 0), 1u64 << (3 * bit + 1));
        assert_eq!(morton3d_encode(0, 0, 1 << bit), 1u64 << (3 * bit + 2));
    }
    assert_eq!(morton3d_encode(7, 0, 0), 0b100_1001);
    // Bits above bit 20 are masked, never leaked into the key.
    assert_eq!(morton3d_encode(u32::MAX, 0, 0) >> 63, 0);
    assert_eq!(
        morton3d_encode(MORTON3D_AXIS_MAX, MORTON3D_AXIS_MAX, MORTON3D_AXIS_MAX),
        (1u64 << 63) - 1
    );
}

#[test]
fn morton3d_decode_is_the_exact_inverse() {
    let mut state = 0x9e3779b9_u64;
    for _ in 0..50_000 {
        let x = (splitmix64(state) as u32) & MORTON3D_AXIS_MAX;
        state = splitmix64(state);
        let y = (splitmix64(state) as u32) & MORTON3D_AXIS_MAX;
        state = splitmix64(state);
        let z = (splitmix64(state) as u32) & MORTON3D_AXIS_MAX;
        state = splitmix64(state);
        assert_eq!(morton3d_decode(morton3d_encode(x, y, z)), (x, y, z));
    }
    assert_eq!(morton3d_decode(0), (0, 0, 0));
    assert_eq!(
        morton3d_decode((1u64 << 63) - 1),
        (MORTON3D_AXIS_MAX, MORTON3D_AXIS_MAX, MORTON3D_AXIS_MAX)
    );
}

#[test]
fn morton3d_keeps_unit_cubes_contiguous() {
    // The Z-order invariant: all 2^3 points of the unit cube occupy the
    // first 8 consecutive keys, all 4^3 of the next level the first 64.
    let unit: Vec<u64> = (0..2)
        .flat_map(|x| (0..2).flat_map(move |y| (0..2).map(move |z| morton3d_encode(x, y, z))))
        .collect();
    let mut sorted = unit.clone();
    sorted.sort_unstable();
    assert_eq!(sorted, (0..8).collect::<Vec<u64>>());
    let cube4: Vec<u64> = (0..4)
        .flat_map(|x| (0..4).flat_map(move |y| (0..4).map(move |z| morton3d_encode(x, y, z))))
        .collect();
    let mut sorted4 = cube4.clone();
    sorted4.sort_unstable();
    assert_eq!(sorted4, (0..64).collect::<Vec<u64>>());
}

#[test]
fn morton3d_signed_biases_and_orders_negative_coordinates() {
    // The origin lands on the bias point; signed neighbours order like
    // their unsigned biased images, and extremes saturate safely.
    let origin = morton3d_signed(0, 0, 0);
    assert_eq!(
        morton3d_decode(origin),
        (
            MORTON3D_SIGNED_BIAS as u32,
            MORTON3D_SIGNED_BIAS as u32,
            MORTON3D_SIGNED_BIAS as u32
        )
    );
    assert!(morton3d_signed(-1, 0, 0) < morton3d_signed(0, 0, 0));
    assert!(morton3d_signed(0, 0, 0) < morton3d_signed(1, 0, 0));
    assert!(morton3d_signed(-16, -16, -16) < morton3d_signed(16, 16, 16));
    // Out-of-range coordinates clamp to the representable edge.
    assert_eq!(
        morton3d_signed(i32::MIN, i32::MIN, i32::MIN),
        morton3d_signed(
            -MORTON3D_SIGNED_BIAS,
            -MORTON3D_SIGNED_BIAS,
            -MORTON3D_SIGNED_BIAS
        )
    );
    assert_eq!(
        morton3d_decode(morton3d_signed(i32::MAX, i32::MAX, i32::MAX)),
        (MORTON3D_AXIS_MAX, MORTON3D_AXIS_MAX, MORTON3D_AXIS_MAX)
    );
}

#[test]
fn projected_sphere_diameter_matches_analytic_tangent_geometry() {
    // On-axis: the projected diameter is exactly 2*f*tan(alpha) with
    // alpha = asin(r/d) — the formula is exact, not an approximation.
    let focal = 900.0_f32;
    let (d, r) = (10.0_f32, 1.0_f32);
    let exact = 2.0 * focal * (r / d) / (1.0 - (r / d) * (r / d)).sqrt();
    let got = projected_sphere_diameter_px([0.0, 0.0, d], r, focal);
    assert!((got - exact).abs() / exact < 1e-6, "{got} vs {exact}");

    // Near-axis agrees with the two-ray angular extent to <2%.
    let center = [1.0_f32, 0.5, 10.0];
    let dist = (center[0].powi(2) + center[1].powi(2) + center[2].powi(2)).sqrt();
    let theta = (center[0].powi(2) + center[1].powi(2))
        .sqrt()
        .atan2(center[2]);
    let alpha = (r / dist).asin();
    let extent = focal * ((theta + alpha).tan() - (theta - alpha).tan());
    let got = projected_sphere_diameter_px(center, r, focal);
    assert!(
        (got - extent).abs() / extent < 0.02,
        "off-axis {got} vs silhouette extent {extent}"
    );
}

#[test]
fn projected_sphere_diameter_is_fail_closed_for_culling() {
    let f = 700.0_f32;
    // Camera inside the sphere: never cull.
    assert_eq!(
        projected_sphere_diameter_px([0.0, 0.0, 0.5], 1.0, f),
        f32::INFINITY
    );
    // Sphere fully behind the camera plane: culled.
    assert_eq!(projected_sphere_diameter_px([0.0, 0.0, -5.0], 1.0, f), 0.0);
    assert_eq!(projected_sphere_diameter_px([0.0, 0.0, -1.0], 1.0, f), 0.0);
    // Degenerate radius draws nothing.
    assert_eq!(projected_sphere_diameter_px([0.0, 0.0, 10.0], 0.0, f), 0.0);
    // Every non-finite input refuses to cull.
    for bad in [
        projected_sphere_diameter_px([f32::NAN, 0.0, 10.0], 1.0, f),
        projected_sphere_diameter_px([0.0, 0.0, f32::INFINITY], 1.0, f),
        projected_sphere_diameter_px([0.0, 0.0, 10.0], f32::NAN, f),
        projected_sphere_diameter_px([0.0, 0.0, 10.0], 1.0, f32::NAN),
        projected_sphere_diameter_px([0.0, 0.0, 10.0], 1.0, 0.0),
        projected_sphere_diameter_px([0.0, 0.0, 10.0], 1.0, -2.0),
    ] {
        assert_eq!(bad, f32::INFINITY);
    }
}
