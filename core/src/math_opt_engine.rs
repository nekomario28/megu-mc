// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! `megu_core::math_opt_engine` — four rigorously paper-backed mathematical
//! engines for runtime kernel selection, telemetry, budget regulation and
//! spatial ordering. Everything here is pure `std`, allocation-free in the
//! hot path, clock-free, deterministic and NaN/Inf fail-closed.
//!
//! * [`DiscountedUcbAutotuner`] — a `K`-arm non-stationary multi-armed
//!   bandit implementing Discounted UCB (Garivier & Moulines, ALT 2011,
//!   "On Upper-Confidence Bound Policies for Switching Bandit Problems").
//!   Discounted statistics forget stale measurements geometrically, so the
//!   tuner detects regime switches (scene density changes, thermal
//!   throttling) and converges to the new fastest kernel automatically.
//! * [`P2QuantileEstimator`] + [`WelfordStats`] — the Jain & Chlamtac `P²`
//!   streaming quantile algorithm (CACM 1985, "The `P²` Algorithm for
//!   Dynamic Calculation of Quantiles and Histograms Without Storing
//!   Observations"): exact five-marker P50/P95/P99 tracking in `O(1)`
//!   memory and `O(1)` time per sample, plus textbook Welford online
//!   mean/variance. No `Vec`, no sorting, no windows.
//! * [`RooflineModel`] + [`PidBudgetController`] — the analytic Roofline
//!   performance model (Williams, Waterman & Patterson, CACM 2009,
//!   "Roofline: An Insightful Visual Performance Model for Multicore
//!   Architectures") deriving peak compute, memory bandwidth and the ridge
//!   point from a [`UniversalHardwareProfile`], plus an anti-windup,
//!   critically-damped discrete PID regulating budget scale toward a
//!   target frame time.
//! * [`morton3d_encode`] / [`morton3d_decode`] / [`morton3d_signed`] —
//!   exact 21-bit-per-axis Z-order curve keys (63-bit codes) via SWAR
//!   magic-bits interleaving, plus [`projected_sphere_diameter_px`], the
//!   closed-form Mara & McGuire (2013) analytic screen-space projection
//!   of a bounding sphere for sub-pixel LOD and Hi-Z culling.

use crate::hw_adaptive_engine::{
    CacheDomainTopology, CoreHeterogeneity, ExtendedSimdTier, GpuComputeParadigm,
    MemoryFootprintClass, PowerThermalProfile, UniversalHardwareProfile,
};

// ===========================================================================
// 1. Discounted UCB non-stationary multi-armed bandit
// ===========================================================================

/// Default discount factor: an effective horizon of `1 / (1 - γ) = 50`
/// samples — long enough for stable estimates, short enough to forget a
/// kernel that turned slow a few hundred frames ago.
pub const DUCB_DEFAULT_GAMMA: f64 = 0.98;

/// Default multiplicative exploration weight on the UCB bonus term.
pub const DUCB_DEFAULT_C: f64 = 1.0;

/// Default exploration bonus constant `ξ`. Garivier & Moulines derive
/// regret bounds for `ξ > 1/2`; `0.6` sits just above that threshold and
/// keeps exploitation dominant once a clear winner exists.
pub const DUCB_DEFAULT_XI: f64 = 0.6;

/// Default normalization scale: a 1 ms kernel latency maps to reward `1/2`.
pub const DUCB_DEFAULT_LATENCY_SCALE_NS: f64 = 1_000_000.0;

/// Observations above one minute are instrumentation garbage, not kernel
/// timings — the same fail-closed bound [`KernelAutotuner`](crate::hw_adaptive_engine::KernelAutotuner)
/// uses.
const DUCB_MAX_VALID_NS: u64 = 60_000_000_000;

fn sanitize_unit_interval_open(value: f64, lo: f64, hi: f64, fallback: f64) -> f64 {
    if !value.is_finite() {
        return fallback;
    }
    value.clamp(lo, hi)
}

/// A `K`-arm Discounted-UCB autotuner over an arbitrary kernel enum
/// (`Scalar`, `SwarRecompute`, `SimdWide`, `LutBatch`, `GpuIndirect`,
/// `GpuMeshShader`, ...). Arms are plain indices; callers keep the
/// index → kernel mapping.
///
/// State per arm is the discounted pull count `N_i(γ, t)` and the
/// discounted reward sum `S_i(γ, t)` with reward
/// `r = 1 / (1 + latency / scale) ∈ (0, 1)`, so *lower* latency is
/// *higher* reward. Each [`observe`](Self::observe) decays both
/// statistics by `γ` in `O(K)`, which is exactly the Garivier–Moulines
/// `N_t(γ, i) = γ N_{t-1}(γ, i) + 1[A_t = i]` recursion.
///
/// Selection explores untried arms first (lowest index first), then
/// maximizes `μ̂_i + c · sqrt(ξ · ln n_γ / N_i)` with `n_γ = Σ_i N_i`,
/// breaking ties deterministically toward the lowest arm index. An arm
/// whose discounted mass decayed to zero is re-explored, which is the
/// mechanism that detects a regime switch: after the optimal kernel
/// changes, the stale winner's `μ̂` collapses within `~1/(1-γ)` samples
/// while the forgotten arms re-enter through the exploration bonus.
#[derive(Clone, Debug)]
pub struct DiscountedUcbAutotuner<const K: usize> {
    gamma: f64,
    exploration_c: f64,
    exploration_xi: f64,
    latency_scale_ns: f64,
    counts: [f64; K],
    reward_sums: [f64; K],
    pulls: [u64; K],
}

impl<const K: usize> Default for DiscountedUcbAutotuner<K> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const K: usize> DiscountedUcbAutotuner<K> {
    /// Default tuning: `γ = 0.98`, `c = 1`, `ξ = 0.6`, 1 ms reward scale.
    pub fn new() -> Self {
        Self::with_params(
            DUCB_DEFAULT_GAMMA,
            DUCB_DEFAULT_C,
            DUCB_DEFAULT_XI,
            DUCB_DEFAULT_LATENCY_SCALE_NS,
        )
    }

    /// Custom tuning. Non-finite or out-of-range parameters fall back to
    /// the defaults: `γ` is clamped into `(0.5, 0.9999]`, `c` and `ξ`
    /// must be positive finite, `latency_scale_ns` must be positive
    /// finite.
    pub fn with_params(
        gamma: f64,
        exploration_c: f64,
        exploration_xi: f64,
        latency_scale_ns: f64,
    ) -> Self {
        Self {
            gamma: sanitize_unit_interval_open(gamma, 0.500_000_1, 0.9999, DUCB_DEFAULT_GAMMA),
            exploration_c: if exploration_c.is_finite() && exploration_c > 0.0 {
                exploration_c
            } else {
                DUCB_DEFAULT_C
            },
            exploration_xi: if exploration_xi.is_finite() && exploration_xi > 0.0 {
                exploration_xi
            } else {
                DUCB_DEFAULT_XI
            },
            latency_scale_ns: if latency_scale_ns.is_finite() && latency_scale_ns > 0.0 {
                latency_scale_ns
            } else {
                DUCB_DEFAULT_LATENCY_SCALE_NS
            },
            counts: [0.0; K],
            reward_sums: [0.0; K],
            pulls: [0; K],
        }
    }

    /// Number of arms under management.
    pub const fn arm_count(&self) -> usize {
        K
    }

    /// Total lifetime observations accepted for `arm` (0 when out of range).
    pub fn pulls(&self, arm: usize) -> u64 {
        self.pulls.get(arm).copied().unwrap_or(0)
    }

    /// Discounted pull count `N_i(γ, t)`; `0.0` for out-of-range arms.
    pub fn discounted_count(&self, arm: usize) -> f64 {
        self.counts.get(arm).copied().unwrap_or(0.0)
    }

    /// Total discounted sample mass `n_γ = Σ_i N_i`, the logarithm
    /// argument of the exploration bonus.
    pub fn total_discounted_count(&self) -> f64 {
        self.counts.iter().sum()
    }

    /// Discounted mean reward `S_i / N_i` in `(0, 1)`, or `None` when the
    /// arm carries no discounted mass.
    pub fn mean_reward(&self, arm: usize) -> Option<f64> {
        let n = *self.counts.get(arm)?;
        if n > 0.0 {
            let mean = self.reward_sums[arm] / n;
            if mean.is_finite() {
                return Some(mean);
            }
        }
        None
    }

    /// Pick the next arm to run. Untried arms win in index order first;
    /// afterwards the arm with the largest discounted-UCB index wins,
    /// with ties resolved toward the lowest index. Degenerate `K = 0`
    /// returns `0`.
    pub fn select_arm(&self) -> usize {
        if K == 0 {
            return 0;
        }
        for (arm, &pulls) in self.pulls.iter().enumerate() {
            if pulls == 0 {
                return arm;
            }
        }
        let ln_total = self.total_discounted_count().max(1.0).ln();
        let mut best_arm = 0usize;
        let mut best_score = f64::NEG_INFINITY;
        for arm in 0..K {
            let n_i = self.counts[arm];
            let score = if !n_i.is_finite() || n_i <= f64::MIN_POSITIVE {
                // Fully decayed mass behaves like an untried arm: the
                // exploration bonus is unbounded, so it is re-explored.
                f64::INFINITY
            } else {
                let mean = self.reward_sums[arm] / n_i;
                let mean = if mean.is_finite() { mean } else { 0.0 };
                let bonus = (self.exploration_xi * ln_total / n_i).max(0.0).sqrt();
                mean + self.exploration_c * bonus
            };
            // Strict `>` keeps the earliest arm on exact ties; NaN scores
            // are rejected so a poisoned statistic can never be chosen.
            if !score.is_nan() && score > best_score {
                best_score = score;
                best_arm = arm;
            }
        }
        best_arm
    }

    /// Record one externally measured kernel duration for `arm`.
    /// Fail-closed: out-of-range arms, `0 ns` and samples above 60 s are
    /// discarded. Accepted samples decay every arm's statistics by `γ`
    /// then credit this arm — `O(K)` total work, no allocation.
    pub fn observe(&mut self, arm: usize, elapsed_ns: u64) {
        if arm >= K || !(1..=DUCB_MAX_VALID_NS).contains(&elapsed_ns) {
            return;
        }
        for i in 0..K {
            self.counts[i] *= self.gamma;
            self.reward_sums[i] *= self.gamma;
        }
        let reward = 1.0 / (1.0 + elapsed_ns as f64 / self.latency_scale_ns);
        self.counts[arm] += 1.0;
        self.reward_sums[arm] += reward;
        self.pulls[arm] = self.pulls[arm].saturating_add(1);
    }
}

// ===========================================================================
// 2. P² streaming quantiles + Welford online moments
// ===========================================================================

/// Jain–Chlamtac `P²` streaming quantile estimator. Tracks the `p`-th
/// quantile (P50/P95/P99 — the 1%-low FPS metric) with five markers in
/// `O(1)` memory (136 bytes, no heap) and `O(1)` time per sample.
///
/// The five markers sit at probabilities `{0, p/2, p, (1+p)/2, 1}` —
/// minimum, two bracketing quantiles, the target and maximum. Each new
/// sample shifts the desired marker positions by their probability
/// increments; a marker more than one position off its desired location
/// is nudged with the parabolic (`P²`) prediction, falling back to the
/// linear form whenever parabola would violate monotonicity. Non-finite
/// samples are rejected; until five valid samples exist the estimator
/// reports `None`.
#[derive(Clone, Debug)]
pub struct P2QuantileEstimator {
    p: f64,
    count: u64,
    /// Marker heights `q_i`; doubles as the first-five sample buffer
    /// until `count == 5`, when it is sorted and markers are initialized.
    heights: [f64; 5],
    /// Actual marker positions `n_i` (1-indexed ranks).
    positions: [i64; 5],
    /// Desired marker positions `n'_i`.
    desired: [f64; 5],
}

impl P2QuantileEstimator {
    /// Estimator for the `p`-th quantile. `p` must lie in `(0, 1)`;
    /// non-finite inputs fail closed to the median, finite ones are
    /// clamped into `[1e-4, 1 - 1e-4]` so the edge markers stay distinct.
    pub fn new(p: f64) -> Self {
        let p = if !p.is_finite() {
            0.5
        } else {
            p.clamp(1.0e-4, 1.0 - 1.0e-4)
        };
        Self {
            p,
            count: 0,
            heights: [0.0; 5],
            positions: [0; 5],
            desired: [0.0; 5],
        }
    }

    /// Probability of the target marker.
    pub fn p(&self) -> f64 {
        self.p
    }

    /// Valid samples accepted so far (non-finite ones never count).
    pub fn count(&self) -> u64 {
        self.count
    }

    /// Probability of marker `i` in `{0, p/2, p, (1+p)/2, 1}`.
    fn marker_probability(&self, index: usize) -> f64 {
        match index {
            0 => 0.0,
            1 => self.p / 2.0,
            2 => self.p,
            3 => (1.0 + self.p) / 2.0,
            _ => 1.0,
        }
    }

    /// Current estimate of the `p`-th quantile, or `None` before the
    /// five-marker bootstrap completes.
    pub fn estimate(&self) -> Option<f64> {
        if self.count >= 5 {
            Some(self.heights[2])
        } else {
            None
        }
    }

    /// Minimum observed value once markers exist.
    pub fn minimum(&self) -> Option<f64> {
        if self.count >= 5 {
            Some(self.heights[0])
        } else {
            None
        }
    }

    /// Maximum observed value once markers exist.
    pub fn maximum(&self) -> Option<f64> {
        if self.count >= 5 {
            Some(self.heights[4])
        } else {
            None
        }
    }

    /// Feed one sample. NaN and ±Inf are rejected fail-closed; this is
    /// the entire `P²` update and performs no allocation.
    pub fn observe(&mut self, sample: f64) {
        if !sample.is_finite() {
            return;
        }
        if self.count < 5 {
            self.heights[self.count as usize] = sample;
            self.count += 1;
            if self.count == 5 {
                self.heights.sort_unstable_by(f64::total_cmp);
                self.positions = [1, 2, 3, 4, 5];
                for i in 0..5 {
                    self.desired[i] = 1.0 + 4.0 * self.marker_probability(i);
                }
            }
            return;
        }
        self.count += 1;

        // Cell k holds q[k] <= sample < q[k+1]; extremes update the
        // boundary marker heights and pin k to an interior cell.
        let k = if sample < self.heights[0] {
            self.heights[0] = sample;
            0usize
        } else if sample >= self.heights[4] {
            self.heights[4] = sample;
            3usize
        } else {
            let mut cell = 0usize;
            for i in 0..4 {
                if sample < self.heights[i + 1] {
                    cell = i;
                    break;
                }
            }
            cell
        };

        for position in self.positions.iter_mut().skip(k + 1) {
            *position += 1;
        }
        for i in 0..5 {
            self.desired[i] += self.marker_probability(i);
        }

        for i in 1..4 {
            let d = self.desired[i] - self.positions[i] as f64;
            let forward_ok = d >= 1.0 && self.positions[i + 1] - self.positions[i] > 1;
            let backward_ok = d <= -1.0 && self.positions[i - 1] - self.positions[i] < -1;
            if !(forward_ok || backward_ok) {
                continue;
            }
            let ds = if forward_ok { 1.0 } else { -1.0 };
            let candidate = self.parabolic(i, ds);
            if self.heights[i - 1] < candidate && candidate < self.heights[i + 1] {
                self.heights[i] = candidate;
            } else {
                // Linear fallback: parabola would break monotonicity.
                let j = (i as i64 + ds as i64) as usize;
                self.heights[i] += ds * (self.heights[j] - self.heights[i])
                    / (self.positions[j] - self.positions[i]) as f64;
            }
            self.positions[i] += ds as i64;
        }
    }

    /// `P²` parabolic prediction of marker `i` moved `ds` (`±1`) ranks.
    fn parabolic(&self, i: usize, ds: f64) -> f64 {
        let q = self.heights;
        let n = self.positions;
        let left_span = (n[i] - n[i - 1]) as f64;
        let right_span = (n[i + 1] - n[i]) as f64;
        let outer_span = (n[i + 1] - n[i - 1]) as f64;
        q[i] + ds / outer_span
            * ((n[i] as f64 - n[i - 1] as f64 + ds) * (q[i + 1] - q[i]) / right_span
                + (n[i + 1] as f64 - n[i] as f64 - ds) * (q[i] - q[i - 1]) / left_span)
    }
}

/// Welford online mean and variance: `O(1)` memory, numerically stable,
/// with non-finite samples rejected before they can poison `m2`.
#[derive(Clone, Copy, Debug, Default)]
pub struct WelfordStats {
    count: u64,
    mean: f64,
    m2: f64,
}

impl WelfordStats {
    /// Empty accumulator.
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed one sample; NaN and ±Inf are rejected fail-closed.
    pub fn observe(&mut self, sample: f64) {
        if !sample.is_finite() {
            return;
        }
        self.count += 1;
        let delta = sample - self.mean;
        self.mean += delta / self.count as f64;
        self.m2 += delta * (sample - self.mean);
    }

    /// Number of accepted samples.
    pub fn count(&self) -> u64 {
        self.count
    }

    /// Running mean, `None` before the first accepted sample.
    pub fn mean(&self) -> Option<f64> {
        if self.count > 0 {
            Some(self.mean)
        } else {
            None
        }
    }

    /// Population variance `m2 / n`, `None` for `n = 0`.
    pub fn variance(&self) -> Option<f64> {
        if self.count > 0 {
            Some(self.m2 / self.count as f64)
        } else {
            None
        }
    }

    /// Sample variance `m2 / (n - 1)`, `None` for `n < 2`.
    pub fn sample_variance(&self) -> Option<f64> {
        if self.count > 1 {
            Some(self.m2 / (self.count - 1) as f64)
        } else {
            None
        }
    }

    /// Population standard deviation, `None` for `n = 0`.
    pub fn std_dev(&self) -> Option<f64> {
        self.variance().map(f64::sqrt)
    }
}

// ===========================================================================
// 3. Analytic roofline model + anti-windup PID budget controller
// ===========================================================================

/// Effective clock class per power/thermal profile (GHz). Order-of-
/// magnitude engineering constants, deliberately conservative for
/// battery and handheld parts where sustained boost is unavailable.
fn clock_ghz(power: PowerThermalProfile) -> f64 {
    match power {
        PowerThermalProfile::AcDesktop => 4.2,
        PowerThermalProfile::ServerHeadless => 3.2,
        PowerThermalProfile::BatteryLaptop => 2.4,
        PowerThermalProfile::HandheldTdp => 1.6,
    }
}

/// FLOP per clock per core for single-precision FMA lanes. A 512-bit
/// vector issues 16 lanes × 2 (FMA); smaller widths scale down, scalar
/// FMA counts 2.
fn flops_per_cycle(simd: ExtendedSimdTier) -> f64 {
    match simd {
        ExtendedSimdTier::Avx512 => 32.0,
        ExtendedSimdTier::Avx2 | ExtendedSimdTier::Sve2 => 16.0,
        ExtendedSimdTier::Sse42
        | ExtendedSimdTier::Neon
        | ExtendedSimdTier::Rvv
        | ExtendedSimdTier::WasmSimd128 => 8.0,
        ExtendedSimdTier::Scalar => 2.0,
    }
}

/// Effective memory bandwidth in GB/s for the kernels this plan
/// schedules: PCIe staging cost for discrete GPUs, shared LPDDR for
/// unified/tile-based parts, DRAM channels for headless compute.
fn bandwidth_gbs(gpu: GpuComputeParadigm) -> f64 {
    match gpu {
        GpuComputeParadigm::DiscretePcieHighBw => 25.0,
        GpuComputeParadigm::UnifiedApuSharedRam => 80.0,
        GpuComputeParadigm::TileBasedTbdr => 60.0,
        GpuComputeParadigm::LowPowerIntegratedWgpu => 30.0,
        GpuComputeParadigm::HeadlessNoGpu => 45.0,
    }
}

/// Roofline placement of a kernel given its arithmetic intensity.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum RooflineVerdict {
    /// `I < I*`: attainable throughput is `I × β`, memory-bound.
    MemoryBound {
        /// `min(Π, I·β)` in GFLOP/s.
        attainable_gflops: f64,
        /// `I / I*`, below `1.0` on this side of the ridge.
        ridge_ratio: f64,
    },
    /// `I >= I*`: attainable throughput is the compute ceiling `Π`.
    ComputeBound {
        /// `min(Π, I·β)` in GFLOP/s — `Π` on this side of the ridge.
        attainable_gflops: f64,
        /// `I / I*`, at least `1.0`.
        ridge_ratio: f64,
    },
}

impl RooflineVerdict {
    /// `min(Π, I·β)` regardless of which side of the ridge we are on.
    pub fn attainable_gflops(self) -> f64 {
        match self {
            Self::MemoryBound {
                attainable_gflops, ..
            }
            | Self::ComputeBound {
                attainable_gflops, ..
            } => attainable_gflops,
        }
    }

    /// `I / I*`: `1.0` exactly on the ridge point.
    pub fn ridge_ratio(self) -> f64 {
        match self {
            Self::MemoryBound { ridge_ratio, .. } | Self::ComputeBound { ridge_ratio, .. } => {
                ridge_ratio
            }
        }
    }
}

/// Dispatch advice derived from a [`RooflineVerdict`] plus the offload
/// and LUT-friendliness context of the profile the model was built from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum RooflineAdvice {
    /// Deeply memory-bound with a discrete GPU and enough bytes in
    /// flight to amortize PCIe staging: push the kernel off-host.
    GpuOffload,
    /// Memory-bound without worthwhile offload: FlashAttention-style
    /// recompute beats bandwidth — keep data out of the memory system.
    RecomputeOverBandwidth,
    /// Compute-bound (or LUT-friendly memory-bound): keep tables resident
    /// and batch — the KV-cache-style trade.
    LutCacheBatch,
}

/// Analytic Roofline model (Williams, Waterman & Patterson 2009):
/// attainable GFLOP/s is `min(Π, I·β)` where `Π` is the compute ceiling,
/// `β` the memory bandwidth and `I` the kernel's arithmetic intensity.
/// The ridge point `I* = Π / β` separates memory-bound from
/// compute-bound kernels.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RooflineModel {
    /// Compute ceiling `Π` in GFLOP/s.
    pub peak_gflops: f64,
    /// Memory bandwidth `β` in GB/s.
    pub bandwidth_gbs: f64,
    /// Ridge point `I* = Π / β` in FLOP per byte.
    pub ridge_intensity: f64,
    /// `true` when a discrete GPU is worth staging work to.
    offload_capable: bool,
    /// `true` when the cache topology keeps lookup tables resident.
    lut_friendly: bool,
}

impl RooflineModel {
    /// Derive the model from a hardware profile: `Π` from cores × SIMD
    /// width × clock class, `β` from the memory/GPU class, `I* = Π / β`.
    /// Pure and total — every profile yields a finite model.
    pub fn from_profile(profile: &UniversalHardwareProfile) -> Self {
        let cores = match profile.core_layout {
            CoreHeterogeneity::Uniform { cores } => f64::from(cores.max(1)),
            // E-cores count at half weight: they widen throughput but do
            // not set the achievable per-kernel compute ceiling.
            CoreHeterogeneity::Hybrid { p_cores, e_cores } => {
                f64::from(p_cores) + 0.5 * f64::from(e_cores)
            }
        };
        let peak_gflops =
            cores * flops_per_cycle(profile.extended_simd) * clock_ghz(profile.power_profile);
        let bandwidth_gbs = bandwidth_gbs(profile.gpu_paradigm);
        let ridge_intensity = if peak_gflops > 0.0 && bandwidth_gbs > 0.0 {
            peak_gflops / bandwidth_gbs
        } else {
            f64::INFINITY
        };
        let lut_friendly = matches!(
            profile.cache_domains,
            CacheDomainTopology::AsymmetricDualCcd { .. }
        ) || (profile.base.l3_cache_kib
            >= crate::hw_adaptive_engine::L3_MEDIUM_KIB
            && profile.memory_class != MemoryFootprintClass::Constrained);
        Self {
            peak_gflops,
            bandwidth_gbs,
            ridge_intensity,
            offload_capable: matches!(profile.gpu_paradigm, GpuComputeParadigm::DiscretePcieHighBw),
            lut_friendly,
        }
    }

    /// Classify a kernel by its per-item arithmetic intensity
    /// `I = flops_per_item / bytes_per_item`. Invalid inputs fail closed
    /// toward the compute-bound verdict: a kernel whose intensity cannot
    /// be measured is scheduled for the compute ceiling rather than a
    /// fictitious memory wall.
    pub fn classify_kernel(&self, flops_per_item: f64, bytes_per_item: f64) -> RooflineVerdict {
        if !flops_per_item.is_finite()
            || flops_per_item < 0.0
            || !bytes_per_item.is_finite()
            || bytes_per_item <= 0.0
        {
            return RooflineVerdict::ComputeBound {
                attainable_gflops: self.peak_gflops.max(0.0),
                ridge_ratio: f64::INFINITY,
            };
        }
        let flops = flops_per_item;
        let bytes = bytes_per_item;
        let intensity = flops / bytes;
        let attainable = (intensity * self.bandwidth_gbs).min(self.peak_gflops);
        let ridge_ratio = if self.ridge_intensity > 0.0 && self.ridge_intensity.is_finite() {
            intensity / self.ridge_intensity
        } else {
            0.0
        };
        if intensity < self.ridge_intensity {
            RooflineVerdict::MemoryBound {
                attainable_gflops: attainable.max(0.0),
                ridge_ratio,
            }
        } else {
            RooflineVerdict::ComputeBound {
                attainable_gflops: attainable.max(0.0),
                ridge_ratio,
            }
        }
    }

    /// Advise a concrete dispatch strategy for the kernel. A deeply
    /// memory-bound kernel (`ridge_ratio < 0.25`) on a discrete-GPU host
    /// earns [`RooflineAdvice::GpuOffload`]; other memory-bound kernels
    /// get recompute advice unless the cache topology favors resident
    /// tables; compute-bound kernels always get LUT batching.
    pub fn recommended_strategy(&self, flops_per_item: f64, bytes_per_item: f64) -> RooflineAdvice {
        match self.classify_kernel(flops_per_item, bytes_per_item) {
            RooflineVerdict::MemoryBound { ridge_ratio, .. } => {
                if self.offload_capable
                    && ridge_ratio < 0.25
                    && bytes_per_item.is_finite()
                    && bytes_per_item >= 64.0
                {
                    RooflineAdvice::GpuOffload
                } else if self.lut_friendly && ridge_ratio >= 0.5 {
                    // Near the ridge on a LUT-friendly host, resident
                    // tables hide most of the residual bandwidth cost.
                    RooflineAdvice::LutCacheBatch
                } else {
                    RooflineAdvice::RecomputeOverBandwidth
                }
            }
            RooflineVerdict::ComputeBound { .. } => RooflineAdvice::LutCacheBatch,
        }
    }
}

/// Anti-windup clamped discrete PID controller regulating a dimensionless
/// work-budget scale toward a target frame time (e.g. `4.16 ms` at
/// 240 FPS or `16.67 ms` at 60 FPS).
///
/// Each [`update`](Self::update) consumes an externally measured frame
/// time and returns the new budget scale in `[min_scale, max_scale]`: a
/// consumer multiplies its chunk-upload / shadow / LOD allowance by it.
/// The error is normalized (`e = (measured − target) / target`), so one
/// gain set is valid at any refresh rate. The integral term carries a
/// hard symmetric clamp and conditional integration freezes it while the
/// output is saturated in the push direction, so sustained overload can
/// never wind the integrator into a seconds-long recovery.
///
/// Default gains (`Kp = 0.40`, `Ki = 0.05`, `Kd = 0.05` per normalized
/// error) place the closed loop at the critical-damping edge for the
/// first-order budget plants this engine feeds: the approach is
/// effectively monotone with at most one shallow zero-crossing, even
/// across a 12× step in offered load.
#[derive(Clone, Copy, Debug)]
pub struct PidBudgetController {
    target_ms: f64,
    kp: f64,
    ki: f64,
    kd: f64,
    min_scale: f64,
    max_scale: f64,
    integral_limit: f64,
    scale: f64,
    integral: f64,
    prev_error: f64,
    primed: bool,
}

impl PidBudgetController {
    /// Critically-damped defaults: full budget (`scale = 1.0`) at start,
    /// `[0.05, 1.0]` output range, `±4` integral clamp.
    pub fn new(target_ms: f64) -> Self {
        Self::with_gains(target_ms, 0.40, 0.05, 0.05, 0.05, 1.0)
    }

    /// Full tuning surface. Every parameter is sanitized fail-closed:
    /// non-positive/non-finite `target_ms` falls back to 16.67 ms, gains
    /// are clamped non-negative, and `min_scale`/`max_scale` are ordered
    /// inside `(0, 1]`.
    pub fn with_gains(
        target_ms: f64,
        kp: f64,
        ki: f64,
        kd: f64,
        min_scale: f64,
        max_scale: f64,
    ) -> Self {
        let target_ms = if target_ms.is_finite() && target_ms > 0.0 {
            target_ms
        } else {
            1_000.0 / 60.0
        };
        let gain = |g: f64| {
            if g.is_finite() && g > 0.0 {
                g
            } else {
                0.0
            }
        };
        let min_scale = if min_scale.is_finite() && min_scale > 0.0 {
            min_scale.min(1.0)
        } else {
            0.05
        };
        let max_scale = if max_scale.is_finite() && max_scale >= min_scale {
            max_scale.min(1.0)
        } else {
            1.0
        };
        Self {
            target_ms,
            kp: gain(kp),
            ki: gain(ki),
            kd: gain(kd),
            min_scale,
            max_scale: max_scale.max(min_scale),
            integral_limit: 4.0,
            scale: 1.0,
            integral: 0.0,
            prev_error: 0.0,
            primed: false,
        }
    }

    /// The configured target frame time.
    pub fn target_ms(&self) -> f64 {
        self.target_ms
    }

    /// Current budget scale — multiply per-frame work allowances by this.
    pub fn budget_scale(&self) -> f64 {
        self.scale
    }

    /// The clamped integral accumulator, exposed for windup diagnostics.
    pub fn integral(&self) -> f64 {
        self.integral
    }

    /// Consume one measured frame time, return the new budget scale.
    /// Non-finite or non-positive measurements are ignored (the previous
    /// scale is returned unchanged).
    pub fn update(&mut self, measured_ms: f64) -> f64 {
        if !measured_ms.is_finite() || measured_ms <= 0.0 {
            return self.scale;
        }
        let error = (measured_ms - self.target_ms) / self.target_ms;
        let derivative = if self.primed {
            error - self.prev_error
        } else {
            0.0
        };
        // Conditional integration: freeze the accumulator while the
        // output is saturated in the direction this error pushes, so
        // long transients cannot wind it past the integral clamp.
        let saturated_high = self.scale >= self.max_scale && error < 0.0;
        let saturated_low = self.scale <= self.min_scale && error > 0.0;
        if !(saturated_high || saturated_low) {
            self.integral =
                (self.integral + error).clamp(-self.integral_limit, self.integral_limit);
        }
        let output = self.kp * error + self.ki * self.integral + self.kd * derivative;
        self.scale = (self.scale - output).clamp(self.min_scale, self.max_scale);
        self.prev_error = error;
        self.primed = true;
        self.scale
    }

    /// Return to full budget with a cleared controller memory.
    pub fn reset(&mut self) {
        self.scale = 1.0;
        self.integral = 0.0;
        self.prev_error = 0.0;
        self.primed = false;
    }
}

// ===========================================================================
// 4. 63-bit Morton Z-order + analytic sphere screen projection
// ===========================================================================

/// Bits of each axis a [`morton3d_encode`] key carries; the packed code
/// uses `3 × 21 = 63` bits.
pub const MORTON3D_AXIS_BITS: u32 = 21;

/// Largest coordinate value that round-trips through a 63-bit key.
pub const MORTON3D_AXIS_MAX: u32 = (1 << MORTON3D_AXIS_BITS) - 1;

/// Signed-coordinate bias for [`morton3d_signed`]: chunk coordinates in
/// `[-2^20, 2^20)` map onto the full unsigned `[0, 2^21)` axis range.
pub const MORTON3D_SIGNED_BIAS: i32 = 1 << 20;

/// Scatter the low 21 bits of `value` to every third bit of a `u64`
/// (positions `0, 3, 6, ..., 60`) — the classic SWAR magic-bits ladder.
#[inline]
fn morton3d_spread(value: u32) -> u64 {
    let mut v = u64::from(value) & 0x1f_ffff;
    v = (v | (v << 32)) & 0x001f_0000_0000_ffff;
    v = (v | (v << 16)) & 0x001f_0000_ff00_00ff;
    v = (v | (v << 8)) & 0x100f_00f0_0f00_f00f;
    v = (v | (v << 4)) & 0x10c3_0c30_c30c_30c3;
    (v | (v << 2)) & 0x1249_2492_4924_9249
}

/// Inverse of [`morton3d_spread`]: gather every third bit starting at
/// `value`'s bit 0 into the low 21 bits.
#[inline]
fn morton3d_compact(mut value: u64) -> u32 {
    value &= 0x1249_2492_4924_9249;
    value = (value ^ (value >> 2)) & 0x10c3_0c30_c30c_30c3;
    value = (value ^ (value >> 4)) & 0x100f_00f0_0f00_f00f;
    value = (value ^ (value >> 8)) & 0x001f_0000_ff00_00ff;
    value = (value ^ (value >> 16)) & 0x001f_0000_0000_ffff;
    ((value ^ (value >> 32)) & 0x1f_ffff) as u32
}

/// Interleave the low 21 bits of `x`, `y`, `z` into a 63-bit Z-order
/// (Morton) key. Bits above bit 20 are silently masked off; ordering the
/// keys sorts space so each `2³` block stays contiguous at every level —
/// the cache-coherent traversal order for chunk-section arrays.
/// Branchless `O(1)`.
pub fn morton3d_encode(x: u32, y: u32, z: u32) -> u64 {
    morton3d_spread(x) | (morton3d_spread(y) << 1) | (morton3d_spread(z) << 2)
}

/// Exact inverse of [`morton3d_encode`]: recover the three 21-bit
/// coordinates from a 63-bit key. Bit 63 of `code` is ignored.
pub fn morton3d_decode(code: u64) -> (u32, u32, u32) {
    (
        morton3d_compact(code),
        morton3d_compact(code >> 1),
        morton3d_compact(code >> 2),
    )
}

/// Morton key for signed chunk coordinates. Each axis is clamped to
/// `[-2^20, 2^20 - 1]` and biased by `+2^20`, so the Minecraft chunk
/// grid around the origin sorts in Z-order with no sign bits polluting
/// the key. Out-of-range coordinates saturate at the nearest edge
/// instead of wrapping.
pub fn morton3d_signed(cx: i32, cy: i32, cz: i32) -> u64 {
    let bias = |v: i32| -> u32 {
        v.clamp(-MORTON3D_SIGNED_BIAS, MORTON3D_SIGNED_BIAS - 1)
            .wrapping_add(MORTON3D_SIGNED_BIAS) as u32
    };
    morton3d_encode(bias(cx), bias(cy), bias(cz))
}

/// Closed-form screen-space diameter (pixels) of a perspective-projected
/// bounding sphere — Mara & McGuire, "2D Polyhedral Bounds of a Clipped,
/// Perspective-Projected 3D Sphere" (JCGT 2013), evaluated for the
/// sphere silhouette: the projected radius is `f_y · r / √(d² − r²)`
/// with `d = ‖c_view‖`, so the diameter is twice that.
///
/// `center_view` is in view space with `+z` forward; `focal_y_px` is the
/// vertical focal length in pixels (`f_y = viewport_h / (2·tan(fov/2))`).
/// Fail-closed contract for culling:
///
/// * sphere entirely behind the camera (`z + r ≤ 0`) → `0.0`;
/// * camera inside the sphere (`d ≤ r`) → `f32::INFINITY` (never cull);
/// * any non-finite input, or `focal_y_px ≤ 0` → `f32::INFINITY`;
/// * `radius ≤ 0` → `0.0`.
///
/// This replaces the usual 8-corner box projection (≈24 flops × 8 plus a
/// matrix multiply) with ~10 flops, exact at the silhouette.
pub fn projected_sphere_diameter_px(center_view: [f32; 3], radius: f32, focal_y_px: f32) -> f32 {
    if !center_view.iter().all(|v| v.is_finite()) || !radius.is_finite() || !focal_y_px.is_finite()
    {
        return f32::INFINITY;
    }
    if radius <= 0.0 {
        return 0.0;
    }
    if focal_y_px <= 0.0 {
        return f32::INFINITY;
    }
    if center_view[2] + radius <= 0.0 {
        return 0.0;
    }
    let d = (center_view[0] * center_view[0]
        + center_view[1] * center_view[1]
        + center_view[2] * center_view[2])
        .sqrt();
    if d <= radius {
        return f32::INFINITY;
    }
    2.0 * focal_y_px * radius / (d * d - radius * radius).sqrt()
}
