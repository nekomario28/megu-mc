// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! Deterministic per-frame streaming budget pacer.
//!
//! The pacer is pure `std` Rust and wall-clock free: the render/streaming
//! loop calls [`FrameBudgetPacer::begin_frame`] once per frame, asks
//! [`FrameBudgetPacer::try_consume_mesh`],
//! [`FrameBudgetPacer::try_consume_upload`], and
//! [`FrameBudgetPacer::try_consume_light`] before doing paced work, then
//! reports the observed frame time with [`FrameBudgetPacer::end_frame`].
//! Work that does not fit is simply not consumed and can be retried on a
//! later frame.

/// Default paced CPU work budget per frame: 2 ms for streaming, meshing,
/// and upload bookkeeping.
pub const DEFAULT_MAX_FRAME_WORK_US: u64 = 2_000;
/// Default GPU upload budget per frame: 4 MiB.
pub const DEFAULT_MAX_UPLOAD_BYTES_PER_FRAME: usize = 4 * 1024 * 1024;
/// Default light-rebuild batch budget per frame.
pub const DEFAULT_MAX_LIGHT_BATCHES_PER_FRAME: usize = 8;
/// Default frame-time target used by adaptive overrun backoff (~60 fps).
pub const DEFAULT_TARGET_FRAME_US: u64 = 16_667;
/// Default number of frames in the join/teleport slow-start ramp.
pub const DEFAULT_RAMP_FRAMES: u32 = 30;
/// Frames retained by the sliding telemetry window: two seconds at 60 fps.
pub const FRAME_STATS_WINDOW: usize = 120;

const RAMP_START_RADIUS: u32 = 2;
const RAMP_RADIUS_STEP: u32 = 2;
const RAMP_UPLOAD_START_PERCENT: u64 = 25;
const FULL_BUDGET_PERCENT: u64 = 100;
const BACKOFF_PERCENT: u64 = 50;
const BACKOFF_RECOVER_PERCENT: u64 = 25;

/// The allowances available to one frame, returned by
/// [`FrameBudgetPacer::begin_frame`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FrameBudget {
    /// Remaining CPU work budget for mesh/streaming/upload bookkeeping.
    pub work_us: u64,
    /// Remaining upload byte budget.
    pub upload_bytes: usize,
    /// Remaining light batches.
    pub light_batches: usize,
    /// Streaming radius currently admitted by the join/teleport ramp.
    pub active_radius: u32,
    /// Upload budget percentage after slow-start ramp scaling (25..=100).
    pub upload_percent: u64,
    /// Streaming work/upload budget percentage after adaptive backoff.
    pub backoff_percent: u64,
}

/// Point-in-time view of the frame-time sliding window. All values are
/// microseconds; percentiles are exact nearest-rank picks over the sorted
/// window, so every figure is a duration a real frame actually took.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct FrameStatsSnapshot {
    /// Frames in the window (`0..=FRAME_STATS_WINDOW`).
    pub count: usize,
    /// Shortest frame in the window.
    pub min_us: f64,
    /// Nearest-rank 50th percentile — the median frame.
    pub p50_us: f64,
    /// Nearest-rank 95th percentile — the stutter tail the adaptive
    /// budgets throttle against.
    pub p95_us: f64,
    /// Nearest-rank 99th percentile — the worst consistent spikes.
    pub p99_us: f64,
    /// Slowest frame in the window — the single worst spike.
    pub max_us: f64,
    /// Arithmetic mean of the window.
    pub mean_us: f64,
}

/// Sliding-window frame-time statistics over the last
/// [`FRAME_STATS_WINDOW`] reported frames.
///
/// Recording is a fixed-capacity ring overwrite — no allocation, no wall
/// clock: callers report each observed frame time and
/// [`snapshot`](Self::snapshot) produces the exact P50/P95/P99/max stutter
/// telemetry the adaptive upload budgets consume. Non-finite (`NaN`,
/// `±inf`) and negative readings are scrubbed at the door so a broken
/// timer cannot poison the window.
#[derive(Clone, Debug)]
pub struct FrameStats {
    ring: [f64; FRAME_STATS_WINDOW],
    cursor: usize,
    len: usize,
}

impl Default for FrameStats {
    fn default() -> Self {
        Self {
            ring: [0.0; FRAME_STATS_WINDOW],
            cursor: 0,
            len: 0,
        }
    }
}

impl FrameStats {
    /// An empty window.
    pub fn new() -> Self {
        Self::default()
    }

    /// Record one frame time in microseconds. `NaN`, infinite, and
    /// negative readings are dropped; everything else enters the window,
    /// evicting the oldest frame once it is full.
    pub fn record(&mut self, frame_us: f64) {
        if !frame_us.is_finite() || frame_us < 0.0 {
            return;
        }
        self.ring[self.cursor] = frame_us;
        self.cursor = (self.cursor + 1) % FRAME_STATS_WINDOW;
        self.len = (self.len + 1).min(FRAME_STATS_WINDOW);
    }

    /// Frames currently in the window.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Whether no valid frame has been recorded yet.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Exact percentile statistics over the window: the ring is copied and
    /// fully sorted, then each percentile is the nearest-rank element —
    /// `ceil(p * n / 100)` — so P50/P95/P99 are always real observed frame
    /// times, never interpolated. `count == 0` yields a zeroed snapshot.
    pub fn snapshot(&self) -> FrameStatsSnapshot {
        if self.len == 0 {
            return FrameStatsSnapshot::default();
        }
        let mut sorted = [0.0; FRAME_STATS_WINDOW];
        sorted[..self.len].copy_from_slice(&self.ring[..self.len]);
        sorted[..self.len].sort_unstable_by(f64::total_cmp);
        let window = &sorted[..self.len];
        let mean_us = window.iter().sum::<f64>() / self.len as f64;
        FrameStatsSnapshot {
            count: self.len,
            min_us: window[0],
            p50_us: percentile(window, 50.0),
            p95_us: percentile(window, 95.0),
            p99_us: percentile(window, 99.0),
            max_us: window[self.len - 1],
            mean_us,
        }
    }
}

/// Nearest-rank percentile over a sorted window: `ceil(p/100 * n)`,
/// clamped to `1..=n`, indexes `sorted[rank - 1]`. Always an observed
/// value — interpolation would invent frame times that never happened.
fn percentile(sorted: &[f64], p: f64) -> f64 {
    let n = sorted.len();
    let rank = ((p / 100.0) * n as f64).ceil() as usize;
    sorted[rank.clamp(1, n) - 1]
}

/// Per-frame pacer for chunk streaming, meshing, GPU uploads, and light
/// batches.
///
/// Configure the public caps directly or with the `with_*` builders, then
/// drive one [`FrameBudget`] per rendered frame. All counters are plain
/// integers; no `SystemTime`, `Instant`, or other wall clock is consulted.
#[derive(Clone, Debug)]
pub struct FrameBudgetPacer {
    /// Maximum paced CPU work admitted per frame, in microseconds.
    pub max_frame_work_us: u64,
    /// Maximum upload bytes admitted per frame.
    pub max_upload_bytes_per_frame: usize,
    /// Maximum light batches admitted per frame.
    pub max_light_batches_per_frame: usize,
    /// Frame-time target used by adaptive overrun backoff, in microseconds.
    pub target_frame_us: u64,
    /// Number of frames used by the join/teleport slow-start ramp.
    pub ramp_frames: u32,

    active_radius: u32,
    ramp_target: u32,
    ramp_frame: u32,
    ramp_active: bool,
    backoff_percent: u64,
    last_frame_us: u64,
    frame_open: bool,
    frame_work_budget_us: u64,
    frame_upload_budget_bytes: usize,
    frame_light_batch_budget: usize,
    frame_upload_percent: u64,
    remaining_work_us: u64,
    remaining_upload_bytes: usize,
    remaining_light_batches: usize,
    consumed_work_us: u64,
    consumed_upload_bytes: usize,
    consumed_light_batches: usize,
    frame_stats: FrameStats,
}

impl Default for FrameBudgetPacer {
    fn default() -> Self {
        Self {
            max_frame_work_us: DEFAULT_MAX_FRAME_WORK_US,
            max_upload_bytes_per_frame: DEFAULT_MAX_UPLOAD_BYTES_PER_FRAME,
            max_light_batches_per_frame: DEFAULT_MAX_LIGHT_BATCHES_PER_FRAME,
            target_frame_us: DEFAULT_TARGET_FRAME_US,
            ramp_frames: DEFAULT_RAMP_FRAMES,
            active_radius: 0,
            ramp_target: 0,
            ramp_frame: 0,
            ramp_active: false,
            backoff_percent: FULL_BUDGET_PERCENT,
            last_frame_us: 0,
            frame_open: false,
            frame_work_budget_us: 0,
            frame_upload_budget_bytes: 0,
            frame_light_batch_budget: 0,
            frame_upload_percent: FULL_BUDGET_PERCENT,
            remaining_work_us: 0,
            remaining_upload_bytes: 0,
            remaining_light_batches: 0,
            consumed_work_us: 0,
            consumed_upload_bytes: 0,
            consumed_light_batches: 0,
            frame_stats: FrameStats::new(),
        }
    }
}

impl FrameBudgetPacer {
    /// A pacer with the default budgets and ramp settings.
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the three per-frame caps while keeping the default ramp and
    /// adaptive-backoff settings.
    pub fn with_limits(
        max_frame_work_us: u64,
        max_upload_bytes_per_frame: usize,
        max_light_batches_per_frame: usize,
    ) -> Self {
        Self {
            max_frame_work_us,
            max_upload_bytes_per_frame,
            max_light_batches_per_frame,
            ..Self::default()
        }
    }

    /// Override the frame-time target used by adaptive overrun backoff.
    pub fn with_target_frame_us(mut self, target_frame_us: u64) -> Self {
        self.target_frame_us = target_frame_us;
        self
    }

    /// Override the number of frames in the join/teleport slow-start ramp.
    pub fn with_ramp_frames(mut self, ramp_frames: u32) -> Self {
        self.ramp_frames = ramp_frames;
        self
    }

    /// Start (or restart) the login/teleport slow-start ramp for a target
    /// view distance in chunks.
    ///
    /// The first ramped frame admits `min(2, target)` chunks of streaming
    /// radius and 25% of the upload budget, then both expand smoothly to the
    /// target over [`FrameBudgetPacer::ramp_frames`] frames.
    pub fn on_world_join_or_teleport(&mut self, target_view_distance: u32) {
        self.ramp_target = target_view_distance;
        self.ramp_frame = 0;
        if target_view_distance <= RAMP_START_RADIUS || self.ramp_frames == 0 {
            self.ramp_active = false;
            self.active_radius = target_view_distance;
        } else {
            self.ramp_active = true;
            self.active_radius = RAMP_START_RADIUS.min(target_view_distance);
        }
    }

    /// Begin a new frame, resetting the per-frame allowances. If the caller
    /// began a frame and never reported its duration, the consumed paced-work
    /// total is used as the previous frame's duration estimate before the
    /// next frame's budget is computed.
    pub fn begin_frame(&mut self) -> FrameBudget {
        if self.frame_open {
            let last_frame_us = self.consumed_work_us;
            self.finish_frame(last_frame_us);
        }

        let (active_radius, upload_percent, ramping) = self.ramp_state();
        self.active_radius = active_radius;
        self.frame_upload_percent = upload_percent;
        let work_budget = scale_u64(self.max_frame_work_us, self.backoff_percent);
        let upload_budget = scale_usize(
            scale_usize(self.max_upload_bytes_per_frame, upload_percent),
            self.backoff_percent,
        );
        let light_budget = self.max_light_batches_per_frame;

        self.frame_work_budget_us = work_budget;
        self.frame_upload_budget_bytes = upload_budget;
        self.frame_light_batch_budget = light_budget;
        self.remaining_work_us = work_budget;
        self.remaining_upload_bytes = upload_budget;
        self.remaining_light_batches = light_budget;
        self.consumed_work_us = 0;
        self.consumed_upload_bytes = 0;
        self.consumed_light_batches = 0;
        self.frame_open = true;

        if ramping {
            self.ramp_frame = self.ramp_frame.saturating_add(1);
            if self.ramp_frame >= self.ramp_frames {
                self.ramp_active = false;
                self.active_radius = self.ramp_target;
            }
        } else if self.ramp_active {
            self.ramp_active = false;
            self.active_radius = self.ramp_target;
        }

        FrameBudget {
            work_us: work_budget,
            upload_bytes: upload_budget,
            light_batches: light_budget,
            active_radius,
            upload_percent,
            backoff_percent: self.backoff_percent,
        }
    }

    /// Report the observed duration of the current (or just-finished) frame
    /// in microseconds. Frames over `target_frame_us * 1.25` halve the next
    /// frame's streaming work/upload budget; frames at or under the target
    /// recover the budget smoothly by 25 percentage points per frame.
    pub fn end_frame(&mut self, last_frame_us: u64) {
        self.finish_frame(last_frame_us);
    }

    /// Admit `cost_us` of mesh/streaming CPU work this frame.
    pub fn try_consume_mesh(&mut self, cost_us: u64) -> bool {
        self.try_consume_work(cost_us)
    }

    /// Admit one upload of `bytes` costing `cost_us` this frame. The upload
    /// is atomic: it either fits both the byte and CPU budgets or consumes
    /// nothing and can be retried on a later frame.
    pub fn try_consume_upload(&mut self, bytes: usize, cost_us: u64) -> bool {
        if !self.frame_open
            || bytes > self.remaining_upload_bytes
            || cost_us > self.remaining_work_us
        {
            return false;
        }
        self.remaining_upload_bytes -= bytes;
        self.remaining_work_us -= cost_us;
        self.consumed_upload_bytes = self.consumed_upload_bytes.saturating_add(bytes);
        self.consumed_work_us = self.consumed_work_us.saturating_add(cost_us);
        true
    }

    /// Admit one light batch costing `cost_us` this frame.
    pub fn try_consume_light(&mut self, cost_us: u64) -> bool {
        if !self.frame_open || self.remaining_light_batches == 0 || cost_us > self.remaining_work_us
        {
            return false;
        }
        self.remaining_light_batches -= 1;
        self.remaining_work_us -= cost_us;
        self.consumed_light_batches = self.consumed_light_batches.saturating_add(1);
        self.consumed_work_us = self.consumed_work_us.saturating_add(cost_us);
        true
    }

    /// Remaining paced CPU work budget for the open frame.
    pub fn remaining_work_us(&self) -> u64 {
        self.remaining_work_us
    }

    /// Remaining upload byte budget for the open frame.
    pub fn remaining_upload_bytes(&self) -> usize {
        self.remaining_upload_bytes
    }

    /// Remaining light batch budget for the open frame.
    pub fn remaining_light_batches(&self) -> usize {
        self.remaining_light_batches
    }

    /// Total paced CPU work consumed by the open frame.
    pub fn consumed_work_us(&self) -> u64 {
        self.consumed_work_us
    }

    /// Total upload bytes consumed by the open frame.
    pub fn consumed_upload_bytes(&self) -> usize {
        self.consumed_upload_bytes
    }

    /// Total light batches consumed by the open frame.
    pub fn consumed_light_batches(&self) -> usize {
        self.consumed_light_batches
    }

    /// Effective CPU work budget granted to the open frame.
    pub fn frame_work_budget_us(&self) -> u64 {
        self.frame_work_budget_us
    }

    /// Effective upload byte budget granted to the open frame.
    pub fn frame_upload_budget_bytes(&self) -> usize {
        self.frame_upload_budget_bytes
    }

    /// Effective light batch budget granted to the open frame.
    pub fn frame_light_batch_budget(&self) -> usize {
        self.frame_light_batch_budget
    }

    /// Upload budget percentage applied by the current slow-start ramp.
    pub fn frame_upload_percent(&self) -> u64 {
        self.frame_upload_percent
    }

    /// Streaming radius currently admitted by the slow-start ramp.
    pub fn active_radius(&self) -> u32 {
        self.active_radius
    }

    /// Whether the join/teleport slow-start ramp is still running.
    pub fn is_ramping(&self) -> bool {
        self.ramp_active
    }

    /// Number of ramp frames already issued in the current ramp.
    pub fn ramp_frame(&self) -> u32 {
        self.ramp_frame
    }

    /// Chunk radius the current ramp is targeting.
    pub fn ramp_target(&self) -> u32 {
        self.ramp_target
    }

    /// Current adaptive streaming budget percentage (50..=100).
    pub fn backoff_percent(&self) -> u64 {
        self.backoff_percent
    }

    /// Whether adaptive backoff is currently reducing the streaming budget.
    pub fn is_backoff_active(&self) -> bool {
        self.backoff_percent < FULL_BUDGET_PERCENT
    }

    /// Most recently reported frame duration in microseconds.
    pub fn last_frame_us(&self) -> u64 {
        self.last_frame_us
    }

    /// The sliding-window frame-time telemetry: every reported
    /// [`end_frame`](Self::end_frame) duration enters the last-120-frames
    /// window.
    pub fn frame_stats(&self) -> &FrameStats {
        &self.frame_stats
    }

    /// Exact P50/P95/P99/max over the sliding window; see
    /// [`FrameStats::snapshot`].
    pub fn frame_stats_snapshot(&self) -> FrameStatsSnapshot {
        self.frame_stats.snapshot()
    }

    /// VSync-friendly pacing headroom: `target_frame_us - P95` of the
    /// sliding window, in microseconds. Positive means the worst-consistent
    /// frame still fits inside the target (headroom for heavier uploads);
    /// negative means the P95 tail is overrunning and adaptive budgets
    /// should shrink. `None` until at least one frame has been reported.
    pub fn frame_time_headroom_us(&self) -> Option<i64> {
        let snapshot = self.frame_stats.snapshot();
        if snapshot.count == 0 {
            return None;
        }
        Some(self.target_frame_us as i64 - snapshot.p95_us as i64)
    }

    fn try_consume_work(&mut self, cost_us: u64) -> bool {
        if !self.frame_open || cost_us > self.remaining_work_us {
            return false;
        }
        self.remaining_work_us -= cost_us;
        self.consumed_work_us = self.consumed_work_us.saturating_add(cost_us);
        true
    }

    fn finish_frame(&mut self, last_frame_us: u64) {
        self.last_frame_us = last_frame_us;
        self.frame_stats.record(last_frame_us as f64);
        self.frame_open = false;
        let overrun_threshold = self
            .target_frame_us
            .saturating_add(self.target_frame_us / 4);
        if last_frame_us > overrun_threshold {
            self.backoff_percent = BACKOFF_PERCENT;
        } else if last_frame_us <= self.target_frame_us {
            self.backoff_percent =
                (self.backoff_percent + BACKOFF_RECOVER_PERCENT).min(FULL_BUDGET_PERCENT);
        }
    }

    fn ramp_state(&self) -> (u32, u64, bool) {
        if !self.ramp_active || self.ramp_frames == 0 {
            return (self.active_radius, FULL_BUDGET_PERCENT, false);
        }
        let elapsed = self.ramp_frame.saturating_add(1).min(self.ramp_frames);
        let upload_percent = if self.ramp_frames <= 1 {
            FULL_BUDGET_PERCENT
        } else {
            RAMP_UPLOAD_START_PERCENT
                + (FULL_BUDGET_PERCENT - RAMP_UPLOAD_START_PERCENT) * u64::from(elapsed - 1)
                    / u64::from(self.ramp_frames - 1)
        };
        (
            ramp_radius(elapsed, self.ramp_target, self.ramp_frames),
            upload_percent,
            true,
        )
    }
}

fn ramp_radius(elapsed_frame: u32, target: u32, ramp_frames: u32) -> u32 {
    if target <= RAMP_START_RADIUS {
        return target;
    }
    let span = target - RAMP_START_RADIUS;
    let steps = span.div_ceil(RAMP_RADIUS_STEP);
    if steps == 0 || ramp_frames <= 1 {
        return target;
    }
    let step =
        ((u64::from(elapsed_frame) - 1) * u64::from(steps) / u64::from(ramp_frames - 1)) as u32;
    let step = step.min(steps);
    if step >= steps {
        target
    } else {
        RAMP_START_RADIUS + step * RAMP_RADIUS_STEP
    }
}

fn scale_u64(value: u64, percent: u64) -> u64 {
    ((u128::from(value) * u128::from(percent)) / 100) as u64
}

fn scale_usize(value: usize, percent: u64) -> usize {
    ((value as u128 * u128::from(percent)) / 100) as usize
}
