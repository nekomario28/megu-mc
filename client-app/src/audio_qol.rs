// SPDX-License-Identifier: LGPL-3.0-or-later
//! Client-side quality-of-life helpers: a hard max-volume audio limiter with
//! same-sound coalescing, a smooth zoom controller, a low-durability tool
//! guard, and a bounded chat-history ring buffer.
//!
//! Everything here is pure std Rust with no wall-clock access — callers pass
//! ticks, frame deltas, and pre-formatted timestamps so behavior stays
//! deterministic and testable.

use std::collections::{HashMap, VecDeque};

/// Upper bound for [`AudioLimiter::max_volume_cap`] and the
/// `qol.max_volume_cap` setting: mix samples may never exceed full scale.
pub const MAX_VOLUME_CAP: f32 = 1.0;
/// Default [`AudioLimiter::max_volume_cap`]: no attenuation of the mix.
pub const DEFAULT_MAX_VOLUME_CAP: f32 = MAX_VOLUME_CAP;
/// Minimum selectable `max_concurrent_same_sound`.
pub const MIN_MAX_CONCURRENT_SAME_SOUND: u8 = 1;
/// Maximum selectable `max_concurrent_same_sound`.
pub const MAX_MAX_CONCURRENT_SAME_SOUND: u8 = 32;
/// Default `max_concurrent_same_sound`: how many instances of the same
/// `(sound_id, cell)` pair may sound per tick before attenuation kicks in.
pub const DEFAULT_MAX_CONCURRENT_SAME_SOUND: u8 = 4;
/// How many times [`DEFAULT_MAX_CONCURRENT_SAME_SOUND`] redundant instances
/// are still admitted (attenuated) before further instances are suppressed.
const ATTENUATION_RANGE_MULTIPLIER: u32 = 4;
/// Bit shift turning a block coordinate into a quantized sound cell
/// (16-block cells, matching chunk-section granularity).
pub const CELL_SHIFT: u32 = 4;

/// Minimum selectable `zoom_divisor`: mild zoom only.
pub const MIN_ZOOM_DIVISOR: f32 = 1.5;
/// Maximum selectable `zoom_divisor`: heavy telescopic zoom.
pub const MAX_ZOOM_DIVISOR: f32 = 10.0;
/// Default `zoom_divisor`: 4x zoom while the zoom key is held.
pub const DEFAULT_ZOOM_DIVISOR: f32 = 4.0;
/// Exponential approach rate (per second) for [`ZoomState`] FOV/divisor
/// interpolation. Higher values settle faster; the motion stays smooth
/// because the step is proportional to the remaining distance.
pub const DEFAULT_ZOOM_SMOOTHING_RATE: f32 = 12.0;
/// Largest frame delta [`ZoomState::tick`] honors; guards against huge
/// hitches teleporting the zoom instead of gliding to it.
const MAX_ZOOM_FRAME_DT: f32 = 0.25;

/// Default `tool_warn_percent`: warn when <=10% durability remains.
pub const DEFAULT_TOOL_WARN_PERCENT: u8 = 10;
/// Maximum selectable `tool_warn_percent`.
pub const MAX_TOOL_WARN_PERCENT: u8 = 100;

/// Default chat-history capacity in lines.
pub const DEFAULT_CHAT_HISTORY_LINES: usize = 1000;
/// Minimum selectable chat-history capacity.
pub const MIN_CHAT_HISTORY_LINES: usize = 100;
/// Maximum selectable chat-history capacity.
pub const MAX_CHAT_HISTORY_LINES: usize = 5000;

fn sanitize_volume_cap(value: f32) -> f32 {
    if value.is_finite() {
        value.clamp(0.0, MAX_VOLUME_CAP)
    } else {
        DEFAULT_MAX_VOLUME_CAP
    }
}

fn sanitize_positive_f32(value: f32, fallback: f32) -> f32 {
    if value.is_finite() && value > 0.0 {
        value
    } else {
        fallback
    }
}

/// Identity of one playable sound instance: a caller-chosen `sound_id`
/// plus the quantized world cell the sound originates from. Two sounds
/// share a [`SoundKey`] only when both match, so identical sounds in
/// different cells never coalesce.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct SoundKey {
    /// Stable identifier for the sound (e.g. a hashed sound event name or
    /// numeric sound id supplied by the audio backend).
    pub sound_id: u64,
    /// Quantized cell X (`block_x >> CELL_SHIFT`).
    pub cell_x: i32,
    /// Quantized cell Y (`block_y >> CELL_SHIFT`).
    pub cell_y: i32,
    /// Quantized cell Z (`block_z >> CELL_SHIFT`).
    pub cell_z: i32,
}

impl SoundKey {
    /// Build a key directly from already-quantized cell coordinates.
    pub const fn in_cell(sound_id: u64, cell_x: i32, cell_y: i32, cell_z: i32) -> Self {
        Self {
            sound_id,
            cell_x,
            cell_y,
            cell_z,
        }
    }

    /// Build a key from a block position, quantizing it into a
    /// [`CELL_SHIFT`]-sized cell first.
    pub fn at_block(sound_id: u64, block_x: i32, block_y: i32, block_z: i32) -> Self {
        Self::in_cell(
            sound_id,
            block_x >> CELL_SHIFT,
            block_y >> CELL_SHIFT,
            block_z >> CELL_SHIFT,
        )
    }
}

/// Decision returned by [`AudioLimiter::register_sound`] for one sound
/// instance arriving in the current tick.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum SoundAdmission {
    /// Within the concurrent-instance budget: play at full volume.
    Play,
    /// Beyond the concurrent budget but inside the attenuation range:
    /// play with `gain` (< 1.0) applied to the instance volume so a stack
    /// of identical sounds thickens instead of clipping.
    Attenuate { gain: f32 },
    /// Far beyond the budget: drop the instance entirely.
    Suppress,
}

/// Hard max-volume limiter plus same-tick sound coalescing.
///
/// `limit_mix_buffer` guarantees every output sample satisfies
/// `sample.abs() <= max_volume_cap` (with NaN/Inf scrubbed to `0.0`), so a
/// tick that sums fifty thunder strikes can never produce ear-splitting
/// clipping. `register_sound` counts instances per [`SoundKey`] within the
/// current tick and attenuates or suppresses redundant stacking before it
/// ever reaches the mixer.
#[derive(Clone, Debug)]
pub struct AudioLimiter {
    max_volume_cap: f32,
    max_concurrent_same_sound: u8,
    current_tick: u64,
    tick_counts: HashMap<SoundKey, u32>,
}

impl AudioLimiter {
    /// Limiter with default settings (`cap = 1.0`, `4` concurrent).
    pub fn new() -> Self {
        Self::with_limits(DEFAULT_MAX_VOLUME_CAP, DEFAULT_MAX_CONCURRENT_SAME_SOUND)
    }

    /// Limiter with explicit limits; both are clamped into range.
    pub fn with_limits(max_volume_cap: f32, max_concurrent_same_sound: u8) -> Self {
        Self {
            max_volume_cap: sanitize_volume_cap(max_volume_cap),
            max_concurrent_same_sound: max_concurrent_same_sound
                .clamp(MIN_MAX_CONCURRENT_SAME_SOUND, MAX_MAX_CONCURRENT_SAME_SOUND),
            current_tick: 0,
            tick_counts: HashMap::new(),
        }
    }

    /// Current hard volume ceiling (`0.0..=1.0`).
    pub fn max_volume_cap(&self) -> f32 {
        self.max_volume_cap
    }

    /// Set the hard volume ceiling; non-finite values reset to the default.
    pub fn set_max_volume_cap(&mut self, value: f32) {
        self.max_volume_cap = sanitize_volume_cap(value);
    }

    /// Current per-tick concurrent-instance budget (`1..=32`).
    pub fn max_concurrent_same_sound(&self) -> u8 {
        self.max_concurrent_same_sound
    }

    /// Set the per-tick concurrent-instance budget; clamped `1..=32`.
    pub fn set_max_concurrent_same_sound(&mut self, value: u8) {
        self.max_concurrent_same_sound =
            value.clamp(MIN_MAX_CONCURRENT_SAME_SOUND, MAX_MAX_CONCURRENT_SAME_SOUND);
    }

    /// Register one instance of `key` arriving in `tick` and decide how it
    /// should be voiced. Counts reset automatically when `tick` changes, so
    /// callers only need to pass a monotonically increasing tick counter.
    pub fn register_sound(&mut self, key: SoundKey, tick: u64) -> SoundAdmission {
        if tick != self.current_tick {
            self.tick_counts.clear();
            self.current_tick = tick;
        }
        let count = {
            let entry = self.tick_counts.entry(key).or_insert(0);
            *entry += 1;
            *entry
        };
        let allowed = u32::from(self.max_concurrent_same_sound);
        if count <= allowed {
            return SoundAdmission::Play;
        }
        let attenuated_until = allowed.saturating_mul(ATTENUATION_RANGE_MULTIPLIER);
        if count <= attenuated_until {
            SoundAdmission::Attenuate {
                gain: allowed as f32 / count as f32,
            }
        } else {
            SoundAdmission::Suppress
        }
    }

    /// Soft-knee peak limiter + hard clamp over a mixed f32 buffer.
    ///
    /// Samples pass untouched below 80% of `cap`. Between the knee onset and
    /// `cap` a cubic soft-knee compresses smoothly (C1-continuous, value- and
    /// slope-matched at both ends). At and above `cap` the sample is pinned
    /// to `±cap`, so the hard guarantee `sample.abs() <= cap` holds for
    /// every output sample. Non-finite inputs (NaN, ±Inf) become `0.0`.
    ///
    /// `max_volume_cap` is clamped to `0.0..=1.0`; non-finite caps fall back
    /// to [`DEFAULT_MAX_VOLUME_CAP`].
    pub fn limit_mix_buffer(samples: &mut [f32], max_volume_cap: f32) {
        let cap = sanitize_volume_cap(max_volume_cap);
        // Knee onset at 80% of the ceiling; the knee spans the remaining 20%.
        let knee_onset = cap * 0.8;
        let knee_width = cap - knee_onset;
        for sample in samples.iter_mut() {
            if !sample.is_finite() {
                *sample = 0.0;
                continue;
            }
            let magnitude = sample.abs();
            let limited = if magnitude <= knee_onset {
                magnitude
            } else if magnitude < cap && knee_width > 0.0 {
                // Cubic Hermite blend: value and slope match the identity at
                // t=0 and flatten to slope 0 at t=1 (= cap), so it joins the
                // hard clamp above without a discontinuity.
                let t = (magnitude - knee_onset) / knee_width;
                knee_onset + knee_width * (t + t * t * (1.0 - t))
            } else {
                cap
            };
            *sample = sample.signum() * limited.min(cap);
        }
    }

    /// Convenience wrapper applying [`AudioLimiter::limit_mix_buffer`] with
    /// this limiter's configured `max_volume_cap`.
    pub fn limit_mix(&self, samples: &mut [f32]) {
        Self::limit_mix_buffer(samples, self.max_volume_cap);
    }
}

impl Default for AudioLimiter {
    fn default() -> Self {
        Self::new()
    }
}

/// Smooth zoom controller: exponential interpolation of the effective FOV
/// divisor between `1.0` (unzoomed) and `zoom_divisor` (fully zoomed), plus
/// automatic mouse-sensitivity scaling while zoom is active.
///
/// FOV is `base_fov / current_divisor`, so holding zoom divides the field of
/// view by up to [`DEFAULT_ZOOM_DIVISOR`] (4x by default). Because
/// [`ZoomState::tick`] approaches the target exponentially, the zoom eases
/// in and out instead of snapping.
#[derive(Clone, Debug)]
pub struct ZoomState {
    base_fov: f32,
    zoom_divisor: f32,
    current_divisor: f32,
    zoomed: bool,
    smoothing_rate: f32,
}

impl ZoomState {
    /// Zoom controller for `base_fov` degrees with
    /// [`DEFAULT_ZOOM_DIVISOR`] and [`DEFAULT_ZOOM_SMOOTHING_RATE`].
    pub fn new(base_fov: f32) -> Self {
        Self::with_zoom_divisor(base_fov, DEFAULT_ZOOM_DIVISOR)
    }

    /// Zoom controller with an explicit divisor (clamped `1.5..=10.0`).
    pub fn with_zoom_divisor(base_fov: f32, zoom_divisor: f32) -> Self {
        Self {
            base_fov: sanitize_positive_f32(base_fov, 70.0),
            zoom_divisor: clamp_zoom_divisor(zoom_divisor),
            current_divisor: 1.0,
            zoomed: false,
            smoothing_rate: DEFAULT_ZOOM_SMOOTHING_RATE,
        }
    }

    /// Override the exponential approach rate (per second, `> 0`).
    pub fn set_smoothing_rate(&mut self, rate: f32) {
        self.smoothing_rate = sanitize_positive_f32(rate, DEFAULT_ZOOM_SMOOTHING_RATE);
    }

    /// Set the fully-zoomed divisor; clamped `1.5..=10.0`.
    pub fn set_zoom_divisor(&mut self, divisor: f32) {
        self.zoom_divisor = clamp_zoom_divisor(divisor);
    }

    /// Currently configured fully-zoomed divisor.
    pub fn zoom_divisor(&self) -> f32 {
        self.zoom_divisor
    }

    /// Press/release the zoom key.
    pub fn set_zoomed(&mut self, zoomed: bool) {
        self.zoomed = zoomed;
    }

    /// Whether the zoom key is currently held.
    pub fn is_zoomed(&self) -> bool {
        self.zoomed
    }

    /// Advance the interpolation by `dt_seconds` and return the current FOV
    /// in degrees. Non-finite or negative deltas are treated as `0`; deltas
    /// above [`MAX_ZOOM_FRAME_DT`] are clamped so hitches cannot teleport.
    pub fn tick(&mut self, dt_seconds: f32) -> f32 {
        let dt = if dt_seconds.is_finite() {
            dt_seconds.clamp(0.0, MAX_ZOOM_FRAME_DT)
        } else {
            0.0
        };
        let target = if self.zoomed { self.zoom_divisor } else { 1.0 };
        let k = 1.0 - (-self.smoothing_rate * dt).exp();
        self.current_divisor += (target - self.current_divisor) * k;
        self.fov()
    }

    /// Current interpolated field of view in degrees.
    pub fn fov(&self) -> f32 {
        self.base_fov / self.current_divisor
    }

    /// Current effective zoom factor (`1.0` unzoomed, up to `zoom_divisor`).
    pub fn zoom_factor(&self) -> f32 {
        self.current_divisor
    }

    /// Mouse sensitivity scaled for the current zoom: `base / zoom_factor`,
    /// so aiming stays consistent while zoomed in.
    pub fn scaled_sensitivity(&self, base_sensitivity: f32) -> f32 {
        base_sensitivity / self.current_divisor
    }
}

fn clamp_zoom_divisor(value: f32) -> f32 {
    if value.is_finite() {
        value.clamp(MIN_ZOOM_DIVISOR, MAX_ZOOM_DIVISOR)
    } else {
        DEFAULT_ZOOM_DIVISOR
    }
}

/// Outcome of [`check_tool_durability_warning`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToolDurability {
    /// Plenty of durability left (or the tool is already broken): swing
    /// normally, vanilla behavior unchanged.
    Ok,
    /// Remaining durability is at or below `warn_percent` of max: warn the
    /// player but let the swing proceed.
    LowWarning { remaining: u32 },
    /// `prevent_break` is on and this swing would consume the last point of
    /// durability: block the swing so the tool survives.
    BlockLastUse,
}

/// Low-durability tool guard.
///
/// `damage` is the durability already consumed; `remaining =
/// max_damage - damage` is what is left before the tool breaks.
/// `warn_percent` (`0..=100`) sets the warning threshold as a percentage of
/// `max_damage`. With `prevent_break = false` (the default) nothing about
/// vanilla breaking changes — the worst outcome is a [`ToolDurability::Ok`]
/// or [`ToolDurability::LowWarning`]. With `prevent_break = true` the final
/// swing that would reduce the tool to zero durability is blocked via
/// [`ToolDurability::BlockLastUse`].
pub fn check_tool_durability_warning(
    damage: u32,
    max_damage: u32,
    warn_percent: u8,
    prevent_break: bool,
) -> ToolDurability {
    if max_damage == 0 {
        return ToolDurability::Ok;
    }
    let remaining = max_damage.saturating_sub(damage);
    if remaining == 0 {
        // Already broken: nothing left to warn about or protect.
        return ToolDurability::Ok;
    }
    if prevent_break && remaining <= 1 {
        return ToolDurability::BlockLastUse;
    }
    let warn_percent = u64::from(warn_percent.min(MAX_TOOL_WARN_PERCENT));
    if u64::from(remaining) * 100 <= u64::from(max_damage) * warn_percent {
        return ToolDurability::LowWarning { remaining };
    }
    ToolDurability::Ok
}

/// Bounded ring buffer for chat lines.
///
/// Capacity is clamped to `100..=5000` lines (default `1000`). When
/// `retain_across_reconnect` is `true` (the default) lines survive a server
/// disconnect/reconnect cycle; when `false`, [`ChatHistory::on_server_disconnect`]
/// clears the buffer. Timestamps are never read from the wall clock — the
/// caller passes a pre-formatted `[HH:MM]`-style stamp (or a tick counter)
/// to [`ChatHistory::push_with_timestamp`], which only prepends it when
/// `show_timestamps` is enabled.
#[derive(Clone, Debug)]
pub struct ChatHistory {
    lines: VecDeque<String>,
    capacity: usize,
    retain_across_reconnect: bool,
    show_timestamps: bool,
}

impl ChatHistory {
    /// History with [`DEFAULT_CHAT_HISTORY_LINES`] capacity and reconnect
    /// retention enabled.
    pub fn new() -> Self {
        Self::with_capacity(DEFAULT_CHAT_HISTORY_LINES)
    }

    /// History with an explicit capacity, clamped `100..=5000`.
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            lines: VecDeque::new(),
            capacity: clamp_chat_capacity(capacity),
            retain_across_reconnect: true,
            show_timestamps: false,
        }
    }

    /// Maximum number of lines retained.
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Resize the ring; clamped `100..=5000`. Growing keeps all lines,
    /// shrinking drops the oldest first.
    pub fn set_capacity(&mut self, capacity: usize) {
        self.capacity = clamp_chat_capacity(capacity);
        while self.lines.len() > self.capacity {
            self.lines.pop_front();
        }
    }

    /// Whether lines survive [`ChatHistory::on_server_disconnect`].
    pub fn retain_across_reconnect(&self) -> bool {
        self.retain_across_reconnect
    }

    /// Enable/disable reconnect retention.
    pub fn set_retain_across_reconnect(&mut self, retain: bool) {
        self.retain_across_reconnect = retain;
    }

    /// Whether [`ChatHistory::push_with_timestamp`] prepends `[timestamp]`.
    pub fn show_timestamps(&self) -> bool {
        self.show_timestamps
    }

    /// Enable/disable `[HH:MM]`-style timestamp prefixes.
    pub fn set_show_timestamps(&mut self, show: bool) {
        self.show_timestamps = show;
    }

    /// Number of lines currently stored.
    pub fn len(&self) -> usize {
        self.lines.len()
    }

    /// Whether the buffer is empty.
    pub fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }

    /// Append a line verbatim (caller may embed its own prefix).
    pub fn push(&mut self, line: impl Into<String>) {
        self.lines.push_back(line.into());
        while self.lines.len() > self.capacity {
            self.lines.pop_front();
        }
    }

    /// Append a chat line with a caller-supplied timestamp. When
    /// `show_timestamps` is enabled the stored line is
    /// `"[{timestamp}] {line}"`; otherwise the timestamp is dropped and the
    /// line is stored verbatim. `timestamp` is expected to be pre-formatted
    /// (e.g. `"14:32"` or a tick count) — this type never reads the clock.
    pub fn push_with_timestamp(&mut self, timestamp: &str, line: impl Into<String>) {
        let line = line.into();
        if self.show_timestamps && !timestamp.is_empty() {
            self.push(format!("[{timestamp}] {line}"));
        } else {
            self.push(line);
        }
    }

    /// Notify the buffer that the client disconnected from a server.
    /// Clears history unless `retain_across_reconnect` is enabled.
    pub fn on_server_disconnect(&mut self) {
        if !self.retain_across_reconnect {
            self.clear();
        }
    }

    /// Drop every stored line.
    pub fn clear(&mut self) {
        self.lines.clear();
    }

    /// Iterate stored lines oldest-to-newest.
    pub fn iter(&self) -> impl Iterator<Item = &str> {
        self.lines.iter().map(String::as_str)
    }

    /// The `n` most recent lines, oldest-to-newest.
    pub fn recent(&self, n: usize) -> Vec<&str> {
        let skip = self.lines.len().saturating_sub(n);
        self.lines.iter().skip(skip).map(String::as_str).collect()
    }
}

impl Default for ChatHistory {
    fn default() -> Self {
        Self::new()
    }
}

fn clamp_chat_capacity(capacity: usize) -> usize {
    capacity.clamp(MIN_CHAT_HISTORY_LINES, MAX_CHAT_HISTORY_LINES)
}
