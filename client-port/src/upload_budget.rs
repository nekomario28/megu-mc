// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later

//! Adaptive per-frame chunk-mesh upload budget.
//!
//! [`ChunkUploadBudget`] caps how many chunk-mesh uploads (and how many
//! upload bytes / CPU microseconds) may be admitted into one frame, so a
//! burst of dirty sections — a join, a teleport, a world-edit flood —
//! spreads smoothly across frames instead of spiking a single frame's
//! submit time (the classic chunk-border FPS stutter).
//!
//! The budget is pure `std` and wall-clock free: the render loop calls
//! [`ChunkUploadBudget::begin_frame`] once per frame, asks
//! [`ChunkUploadBudget::try_schedule`] before each upload, then reports the
//! observed windowed frame-time P95 with
//! [`ChunkUploadBudget::adapt`]. When P95 runs over the frame-time target
//! the admitted upload count halves (down to
//! [`ChunkUploadBudget::min_uploads_per_frame`]); when it recovers under
//! 90% of target the count regrows one slot per frame (up to
//! [`ChunkUploadBudget::max_uploads_per_frame`]). Uploads that do not fit
//! are simply not admitted and retry on a later frame.

/// Default upper bound on chunk uploads admitted per frame.
pub const DEFAULT_MAX_UPLOADS_PER_FRAME: u32 = 8;
/// Default floor the adaptive controller never throttles below: one upload
/// per frame keeps the queue draining even on sustained stutter.
pub const DEFAULT_MIN_UPLOADS_PER_FRAME: u32 = 1;
/// Default chunk-upload byte budget per frame: 4 MiB.
pub const DEFAULT_MAX_UPLOAD_BYTES_PER_FRAME: usize = 4 * 1024 * 1024;
/// Default upload-side CPU work budget per frame: 2 ms.
pub const DEFAULT_MAX_UPLOAD_WORK_US_PER_FRAME: u64 = 2_000;
/// Default frame-time target used by [`ChunkUploadBudget::adapt`] (~60 fps).
pub const DEFAULT_TARGET_FRAME_US: u64 = 16_667;

/// Recover when P95 is at or under `target * RECOVER_PERCENT / 100`.
const RECOVER_PERCENT: u64 = 90;
/// Throttle when P95 exceeds `target * THROTTLE_PERCENT / 100`.
const THROTTLE_PERCENT: u64 = 110;

/// Per-frame admission controller for chunk-mesh uploads.
///
/// Three hard allowances gate every frame — upload count, upload bytes,
/// and estimated upload CPU microseconds — and the count allowance is
/// adaptive: [`adapt`](Self::adapt) shrinks it while reported frame times
/// overrun the target and regrows it once the frame pacer reports headroom.
/// All counters are plain integers; no `SystemTime` or `Instant` is
/// consulted.
#[derive(Clone, Debug)]
pub struct ChunkUploadBudget {
    /// Maximum uploads ever admitted per frame.
    pub max_uploads_per_frame: u32,
    /// Adaptive-count floor: uploads admitted even under sustained
    /// throttling, so the dirty queue always drains.
    pub min_uploads_per_frame: u32,
    /// Maximum upload payload bytes admitted per frame.
    pub max_upload_bytes_per_frame: usize,
    /// Maximum estimated upload CPU work admitted per frame, microseconds.
    pub max_upload_work_us_per_frame: u64,
    /// Frame-time target [`adapt`](Self::adapt) throttles against,
    /// microseconds.
    pub target_frame_us: u64,

    uploads_per_frame: u32,
    used_uploads: u32,
    used_bytes: usize,
    used_us: u64,
    frame_open: bool,
}

impl Default for ChunkUploadBudget {
    fn default() -> Self {
        Self {
            max_uploads_per_frame: DEFAULT_MAX_UPLOADS_PER_FRAME,
            min_uploads_per_frame: DEFAULT_MIN_UPLOADS_PER_FRAME,
            max_upload_bytes_per_frame: DEFAULT_MAX_UPLOAD_BYTES_PER_FRAME,
            max_upload_work_us_per_frame: DEFAULT_MAX_UPLOAD_WORK_US_PER_FRAME,
            target_frame_us: DEFAULT_TARGET_FRAME_US,
            uploads_per_frame: DEFAULT_MAX_UPLOADS_PER_FRAME,
            used_uploads: 0,
            used_bytes: 0,
            used_us: 0,
            frame_open: false,
        }
    }
}

impl ChunkUploadBudget {
    /// A budget with the default caps, starting un-throttled.
    pub fn new() -> Self {
        Self::default()
    }

    /// A budget with explicit per-frame caps and the default adaptive
    /// floor/target. `max_uploads_per_frame` of `0` is clamped to `1` —
    /// a budget that admits nothing cannot drain a queue.
    pub fn with_limits(
        max_uploads_per_frame: u32,
        max_upload_bytes_per_frame: usize,
        max_upload_work_us_per_frame: u64,
    ) -> Self {
        let max_uploads_per_frame = max_uploads_per_frame.max(1);
        Self {
            max_uploads_per_frame,
            uploads_per_frame: max_uploads_per_frame,
            max_upload_bytes_per_frame,
            max_upload_work_us_per_frame,
            ..Self::default()
        }
    }

    /// Reduced preset matching the [`crate::LowSpecRenderBudget`] contract
    /// (2 uploads/frame, 1 MiB/frame, 1 ms/frame): shared-RAM and
    /// ≤512 MiB-VRAM devices keep world streaming smooth by trading upload
    /// throughput for frame-time headroom.
    pub fn low_spec() -> Self {
        Self {
            max_uploads_per_frame: 2,
            min_uploads_per_frame: 1,
            max_upload_bytes_per_frame: 1024 * 1024,
            max_upload_work_us_per_frame: 1_000,
            uploads_per_frame: 2,
            ..Self::default()
        }
    }

    /// Override the frame-time target used by [`adapt`](Self::adapt).
    pub fn with_target_frame_us(mut self, target_frame_us: u64) -> Self {
        self.target_frame_us = target_frame_us;
        self
    }

    /// Override the adaptive-count floor (clamped to
    /// `1..=max_uploads_per_frame`).
    pub fn with_min_uploads_per_frame(mut self, min_uploads_per_frame: u32) -> Self {
        self.min_uploads_per_frame = min_uploads_per_frame
            .max(1)
            .min(self.max_uploads_per_frame.max(1));
        self.uploads_per_frame = self.uploads_per_frame.max(self.min_uploads_per_frame);
        self
    }

    /// Open a new frame: the three allowances reset to their effective caps.
    /// Calling `begin_frame` twice simply restarts the frame — unspent
    /// allowances do not carry over.
    pub fn begin_frame(&mut self) {
        self.used_uploads = 0;
        self.used_bytes = 0;
        self.used_us = 0;
        self.frame_open = true;
    }

    /// Admit one chunk upload of `bytes` payload costing `cost_us`
    /// microseconds. Atomic: the upload either fits all three allowances —
    /// count, bytes, work — or consumes nothing and can be retried on a
    /// later frame. Always admits nothing outside [`begin_frame`](Self::begin_frame).
    pub fn try_schedule(&mut self, bytes: usize, cost_us: u64) -> bool {
        if !self.frame_open
            || self.used_uploads >= self.uploads_per_frame
            || bytes > self.remaining_bytes()
            || cost_us > self.remaining_work_us()
        {
            return false;
        }
        self.used_uploads += 1;
        self.used_bytes = self.used_bytes.saturating_add(bytes);
        self.used_us = self.used_us.saturating_add(cost_us);
        true
    }

    /// Adapt the per-frame upload count to the reported frame-time P95 in
    /// microseconds, as computed by the windowed frame stats — call once per
    /// frame after `begin_frame`'s frame closes.
    ///
    /// * `p95_us > target * 1.10` (or a non-finite reading, fail-safe):
    ///   the count halves toward [`min_uploads_per_frame`](Self::min_uploads_per_frame).
    /// * `p95_us <= target * 0.90`: the count regrows by one toward
    ///   [`max_uploads_per_frame`](Self::max_uploads_per_frame).
    /// * Between the two watermarks the count holds — the band keeps the
    ///   controller from oscillating on borderline frames.
    pub fn adapt(&mut self, p95_us: f64) {
        let target = self.target_frame_us as f64;
        if !p95_us.is_finite() || p95_us > target * (THROTTLE_PERCENT as f64 / 100.0) {
            self.uploads_per_frame =
                (self.uploads_per_frame / 2).max(self.min_uploads_per_frame);
        } else if p95_us >= 0.0 && p95_us <= target * (RECOVER_PERCENT as f64 / 100.0) {
            self.uploads_per_frame =
                (self.uploads_per_frame + 1).min(self.max_uploads_per_frame);
        }
    }

    /// Uploads the current frame will still admit.
    pub fn remaining_uploads(&self) -> u32 {
        self.uploads_per_frame.saturating_sub(self.used_uploads)
    }

    /// Upload bytes the current frame will still admit.
    pub fn remaining_bytes(&self) -> usize {
        self.max_upload_bytes_per_frame.saturating_sub(self.used_bytes)
    }

    /// Estimated upload microseconds the current frame will still admit.
    pub fn remaining_work_us(&self) -> u64 {
        self.max_upload_work_us_per_frame.saturating_sub(self.used_us)
    }

    /// Uploads admitted so far in the open frame.
    pub fn used_uploads(&self) -> u32 {
        self.used_uploads
    }

    /// Upload bytes admitted so far in the open frame.
    pub fn used_bytes(&self) -> usize {
        self.used_bytes
    }

    /// Estimated upload microseconds admitted so far in the open frame.
    pub fn used_work_us(&self) -> u64 {
        self.used_us
    }

    /// Current adaptive upload cap
    /// (`min_uploads_per_frame..=max_uploads_per_frame`).
    pub fn uploads_per_frame(&self) -> u32 {
        self.uploads_per_frame
    }

    /// Whether [`adapt`](Self::adapt) is currently throttling below
    /// [`max_uploads_per_frame`](Self::max_uploads_per_frame).
    pub fn is_throttled(&self) -> bool {
        self.uploads_per_frame < self.max_uploads_per_frame
    }
}
