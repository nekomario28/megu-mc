// SPDX-License-Identifier: LGPL-3.0-or-later

//! Human-perception priority scheduling and compact GPU instance packing.
//!
//! Three cooperating pieces cut client CPU/GPU work by exploiting what the
//! player can actually perceive in a frame:
//!
//! * [`FrontalConePriorityQueue`] scores chunk-mesh, chunk-gen, and particle
//!   tasks against the camera's 60° frontal vision cone. Work inside the
//!   cone receives a 10× priority multiplier over work behind the player,
//!   while urgent nearby block edits always preempt. Selection is
//!   deterministic: identical candidate sets always yield identical batches.
//! * [`DistanceEntitySyncPlanner`] + [`EntityMotionInterpolator`] scale
//!   entity update rate by perceived need — every tick under 32 m (or when
//!   combat/player/falling-critical), every 2 ticks to 64 m, every 4 ticks
//!   beyond — while the interpolator keeps distant mobs visually smooth
//!   between sparse snapshots using shortest-angle yaw/pitch wrapping.
//! * [`GpuOffloadInstancePacker`] packs particles and weather columns into
//!   `#[repr(C)]` 16-byte [`GpuParticleInstance`] /
//!   [`GpuWeatherColumnInstance`] records (four `u32` words each), so the
//!   CPU uploads only compact dirty instance slices while GPU shaders drive
//!   rainfall, foliage sway, and chunk fade-in.
#![forbid(unsafe_code)]

/// Cosine of the 30° half-angle: a target direction with
/// `dot(dir, forward) >= FRONTAL_CONE_COS_HALF` lies inside the 60° frontal
/// cone.
pub const FRONTAL_CONE_COS_HALF: f32 = 0.866_025_4;
/// Priority multiplier applied to tasks inside the frontal vision cone.
pub const FRONTAL_CONE_MULTIPLIER: u32 = 10;
/// Priority multiplier for the peripheral band: in front of the camera
/// plane (`dot >= 0`) but outside the 60° frontal cone.
pub const PERIPHERAL_CONE_MULTIPLIER: u32 = 4;
/// Priority multiplier for tasks behind the player (`dot < 0`).
pub const REAR_CONE_MULTIPLIER: u32 = 1;
/// Directions shorter than this are treated as "at the camera" and always
/// land in the frontal band.
pub const AT_CAMERA_EPSILON: f32 = 1.0e-6;

/// Near-entity sync radius: full 20 Hz updates inside 32 m.
pub const NEAR_SYNC_RADIUS_BLOCKS: f32 = 32.0;
/// Mid-entity sync radius: 10 Hz updates out to 64 m.
pub const MID_SYNC_RADIUS_BLOCKS: f32 = 64.0;
/// Sync period (in ticks) for entities inside [`NEAR_SYNC_RADIUS_BLOCKS`]
/// or flagged critical: every tick, 20 Hz.
pub const NEAR_SYNC_PERIOD_TICKS: u64 = 1;
/// Sync period for entities in the 32–64 m band: every 2 ticks, 10 Hz.
pub const MID_SYNC_PERIOD_TICKS: u64 = 2;
/// Sync period for entities past 64 m: every 4 ticks, 5 Hz.
pub const FAR_SYNC_PERIOD_TICKS: u64 = 4;

/// GPU instance stride: every packed instance record is exactly 16 bytes,
/// 4-byte aligned — four `u32` shader words.
pub const GPU_INSTANCE_BYTES: usize = 16;
/// Fixed-point scale for packed particle/world offsets: i16 words in
/// 1/256-block units, ±128 block range.
pub const PARTICLE_POS_SCALE: f32 = 256.0;
/// Fixed-point scale for packed particle velocities: i8 words in
/// 1/16-block-per-tick units, ±8 blocks/tick range.
pub const PARTICLE_VEL_SCALE: f32 = 16.0;
/// Fixed-point scale for packed weather fall speeds: i8 words in
/// 1/16-block-per-tick units.
pub const WEATHER_FALL_SPEED_SCALE: f32 = 16.0;
/// Maximum absolute packed particle offset in blocks (±128).
pub const PARTICLE_POS_LIMIT_BLOCKS: f32 = 128.0;
/// Maximum absolute packed velocity in blocks per tick (±8).
pub const PARTICLE_VEL_LIMIT: f32 = 8.0;

fn vec_sub(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}

fn vec_dot(a: [f32; 3], b: [f32; 3]) -> f32 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

fn vec_len_sq(a: [f32; 3]) -> f32 {
    vec_dot(a, a)
}

/// Kind of per-frame offload work scored by [`FrontalConePriorityQueue`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum OffloadTaskKind {
    /// Rebuild a chunk-section mesh (highest base priority: geometry is
    /// what the player is looking at).
    ChunkMesh,
    /// Generate/light a chunk section that has no meshable data yet.
    ChunkGen,
    /// Particle or other cosmetic simulation tick.
    Particle,
}

impl OffloadTaskKind {
    /// Base priority before the cone multiplier: chunk meshes outrank
    /// chunk generation, which outranks cosmetic particles.
    pub fn base_priority(self) -> u32 {
        match self {
            Self::ChunkMesh => 3,
            Self::ChunkGen => 2,
            Self::Particle => 1,
        }
    }
}

/// One candidate job for the frame's offload budget.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct OffloadTask {
    /// Caller-chosen unique identifier; used as the final deterministic
    /// tiebreak, so equal-priority tasks order stably frame to frame.
    pub job_id: u64,
    /// What kind of work this is.
    pub kind: OffloadTaskKind,
    /// World-space position the work is centred on (section centre for
    /// chunk tasks, emitter position for particles).
    pub target_pos: [f32; 3],
    /// Set for immediate player block edits: these always preempt every
    /// non-urgent task regardless of direction.
    pub is_urgent_edit: bool,
}

/// Which perceptual band a task's direction fell into.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConeBand {
    /// Inside the 60° frontal vision cone.
    Frontal,
    /// In front of the camera plane but outside the frontal cone.
    Peripheral,
    /// Behind the player (`dot < 0`).
    Rear,
}

impl ConeBand {
    /// Priority multiplier for this band.
    pub fn multiplier(self) -> u32 {
        match self {
            Self::Frontal => FRONTAL_CONE_MULTIPLIER,
            Self::Peripheral => PERIPHERAL_CONE_MULTIPLIER,
            Self::Rear => REAR_CONE_MULTIPLIER,
        }
    }
}

/// An [`OffloadTask`] plus its resolved perceptual score.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ScoredOffloadTask {
    /// The scored task.
    pub task: OffloadTask,
    /// Cone band the task direction fell into.
    pub band: ConeBand,
    /// Final score: `kind.base_priority() * band.multiplier()`.
    pub score: u32,
    /// Squared distance from the camera; used as the nearer-first
    /// tiebreak inside equal scores.
    pub distance_sq: f32,
}

/// Scores offload tasks by how strongly they land inside the player's
/// 60° frontal vision cone.
///
/// Urgent edits always sort first; everything else orders by descending
/// score, then nearer distance, then `job_id` — so
/// [`select_budgeted_batch`](Self::select_budgeted_batch) is fully
/// deterministic for a given candidate set.
#[derive(Clone, Copy, Debug)]
pub struct FrontalConePriorityQueue {
    /// Camera world position.
    pub camera_pos: [f32; 3],
    /// Camera look direction (normalised in [`new`](Self::new)).
    pub camera_forward_unit: [f32; 3],
    forward_valid: bool,
}

impl FrontalConePriorityQueue {
    /// Build a scorer for `camera_pos` looking along `camera_forward`.
    ///
    /// `camera_forward` is normalised internally; a degenerate
    /// (near-zero) forward vector makes every direction frontal, since
    /// "behind the player" is then meaningless.
    pub fn new(camera_pos: [f32; 3], camera_forward: [f32; 3]) -> Self {
        let len = vec_len_sq(camera_forward).sqrt();
        let (unit, valid) = if len > AT_CAMERA_EPSILON {
            (
                [
                    camera_forward[0] / len,
                    camera_forward[1] / len,
                    camera_forward[2] / len,
                ],
                true,
            )
        } else {
            ([0.0, 0.0, 1.0], false)
        };
        Self {
            camera_pos,
            camera_forward_unit: unit,
            forward_valid: valid,
        }
    }

    /// Score one task against the frontal cone.
    pub fn score(&self, task: &OffloadTask) -> ScoredOffloadTask {
        let dir = vec_sub(task.target_pos, self.camera_pos);
        let dist_sq = vec_len_sq(dir);
        let band = if !self.forward_valid || dist_sq < AT_CAMERA_EPSILON * AT_CAMERA_EPSILON {
            ConeBand::Frontal
        } else {
            let cos = vec_dot(dir, self.camera_forward_unit) / dist_sq.sqrt();
            if cos >= FRONTAL_CONE_COS_HALF {
                ConeBand::Frontal
            } else if cos >= 0.0 {
                ConeBand::Peripheral
            } else {
                ConeBand::Rear
            }
        };
        ScoredOffloadTask {
            task: *task,
            band,
            score: task.kind.base_priority() * band.multiplier(),
            distance_sq: dist_sq,
        }
    }

    /// Deterministically select the top `max_jobs` tasks for this frame.
    ///
    /// Ordering: urgent edits first, then descending score, then nearer
    /// distance, then ascending `job_id`. Returns at most `max_jobs`
    /// entries (fewer when the candidate set is smaller).
    pub fn select_budgeted_batch(
        &self,
        candidates: &[OffloadTask],
        max_jobs: usize,
    ) -> Vec<ScoredOffloadTask> {
        let mut scored: Vec<ScoredOffloadTask> = candidates.iter().map(|t| self.score(t)).collect();
        scored.sort_by(|a, b| {
            b.task
                .is_urgent_edit
                .cmp(&a.task.is_urgent_edit)
                .then_with(|| b.score.cmp(&a.score))
                .then_with(|| a.distance_sq.total_cmp(&b.distance_sq))
                .then_with(|| a.task.job_id.cmp(&b.task.job_id))
        });
        scored.truncate(max_jobs);
        scored
    }
}

/// Perceptual class of a synced entity. Anything the player is directly
/// interacting with bypasses distance scaling entirely.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum EntitySyncClass {
    /// Ordinary ambient entity: distance-scaled rate.
    Passive,
    /// Falling block / gravity-driven entity: critical, always full rate.
    Falling,
    /// Entity in combat with the player: critical, always full rate.
    Combat,
    /// A player entity: critical, always full rate.
    Player,
}

impl EntitySyncClass {
    /// Critical classes never get rate-scaled, even past 64 m.
    pub fn is_critical(self) -> bool {
        !matches!(self, Self::Passive)
    }
}

/// Plans per-entity network sync rates from perceived distance.
///
/// * ≤ 32 m (or critical class): every tick, 20 Hz.
/// * 32–64 m: every 2 ticks, 10 Hz — a 50% bandwidth cut.
/// * > 64 m: every 4 ticks, 5 Hz — a 75% bandwidth cut.
///
/// Ticks are phase-staggered by `entity_id` so same-band entities spread
/// their updates across the period instead of bursting on one tick.
#[derive(Clone, Copy, Debug)]
pub struct DistanceEntitySyncPlanner {
    /// Radius of the full-rate band.
    pub near_radius_blocks: f32,
    /// Outer radius of the half-rate band (beyond it is quarter-rate).
    pub mid_radius_blocks: f32,
}

impl Default for DistanceEntitySyncPlanner {
    fn default() -> Self {
        Self::new()
    }
}

impl DistanceEntitySyncPlanner {
    /// Planner with the vanilla-perception defaults (32 m / 64 m).
    pub fn new() -> Self {
        Self {
            near_radius_blocks: NEAR_SYNC_RADIUS_BLOCKS,
            mid_radius_blocks: MID_SYNC_RADIUS_BLOCKS,
        }
    }

    /// Ticks between syncs for an entity at `distance_blocks`.
    pub fn sync_period_ticks(&self, distance_blocks: f32, class: EntitySyncClass) -> u64 {
        if class.is_critical() || distance_blocks <= self.near_radius_blocks {
            NEAR_SYNC_PERIOD_TICKS
        } else if distance_blocks <= self.mid_radius_blocks {
            MID_SYNC_PERIOD_TICKS
        } else {
            FAR_SYNC_PERIOD_TICKS
        }
    }

    /// Whether `entity_id` should be synced on `tick`, phase-staggered so
    /// entities in the same band spread across the period.
    pub fn should_sync(
        &self,
        tick: u64,
        entity_id: u32,
        distance_blocks: f32,
        class: EntitySyncClass,
    ) -> bool {
        let period = self.sync_period_ticks(distance_blocks, class);
        (tick + u64::from(entity_id)).is_multiple_of(period)
    }

    /// Count how many of the `tick_count` ticks starting at `tick_start`
    /// would carry a sync for this entity — the bandwidth accounting
    /// counterpart of [`should_sync`](Self::should_sync).
    pub fn count_syncs(
        &self,
        tick_start: u64,
        tick_count: u64,
        entity_id: u32,
        distance_blocks: f32,
        class: EntitySyncClass,
    ) -> u64 {
        (tick_start..tick_start + tick_count)
            .filter(|&t| self.should_sync(t, entity_id, distance_blocks, class))
            .count() as u64
    }
}

/// One authoritative entity pose received at `tick`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct EntityPoseSnapshot {
    /// Server tick this pose belongs to.
    pub tick: u64,
    /// World position.
    pub pos: [f32; 3],
    /// Yaw in degrees (any range; wrapped shortest-path at sample time).
    pub yaw_deg: f32,
    /// Pitch in degrees.
    pub pitch_deg: f32,
}

/// A pose interpolated for rendering between two snapshots.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SampledEntityPose {
    /// Interpolated world position.
    pub pos: [f32; 3],
    /// Interpolated yaw in degrees, normalised to `[-180, 180)`.
    pub yaw_deg: f32,
    /// Interpolated pitch in degrees, normalised to `[-180, 180)`.
    pub pitch_deg: f32,
}

/// Interpolates an angle from `a` to `b` (degrees) along the shortest
/// arc — a yaw crossing the ±180° seam swings 20° through 180° rather
/// than 340° the long way around. The result is normalised to
/// `[-180, 180)`.
pub fn lerp_angle_deg(a: f32, b: f32, t: f32) -> f32 {
    let mut delta = (b - a).rem_euclid(360.0);
    if delta > 180.0 {
        delta -= 360.0;
    }
    (a + delta * t + 180.0).rem_euclid(360.0) - 180.0
}

/// Client-side smoother for sparsely-synced entity poses.
///
/// Keeps the two most recent snapshots and linearly interpolates position
/// plus shortest-angle yaw/pitch between them, clamped to the snapshot
/// window (no extrapolation — a stale entity freezes instead of
/// rubber-banding past its last known pose).
#[derive(Clone, Debug, Default)]
pub struct EntityMotionInterpolator {
    prev: Option<EntityPoseSnapshot>,
    latest: Option<EntityPoseSnapshot>,
}

impl EntityMotionInterpolator {
    /// Empty interpolator; [`sample`](Self::sample) returns `None` until
    /// the first snapshot arrives.
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed the newest authoritative pose. Snapshots are expected in
    /// non-decreasing tick order; an out-of-order or same-tick snapshot
    /// simply replaces `latest` without advancing `prev`.
    pub fn push_snapshot(&mut self, snapshot: EntityPoseSnapshot) {
        match self.latest {
            Some(latest) if snapshot.tick > latest.tick => {
                self.prev = Some(latest);
                self.latest = Some(snapshot);
            }
            _ => self.latest = Some(snapshot),
        }
    }

    /// The most recently pushed snapshot, if any.
    pub fn latest(&self) -> Option<EntityPoseSnapshot> {
        self.latest
    }

    /// Pose for `render_tick` (may be fractional, e.g. tick + partial
    /// frame alpha). With two snapshots the sample is clamped inside
    /// `[prev.tick, latest.tick]`; with one it is that snapshot's pose.
    pub fn sample(&self, render_tick: f32) -> Option<SampledEntityPose> {
        let latest = self.latest?;
        let Some(prev) = self.prev else {
            return Some(SampledEntityPose {
                pos: latest.pos,
                yaw_deg: lerp_angle_deg(latest.yaw_deg, latest.yaw_deg, 0.0),
                pitch_deg: lerp_angle_deg(latest.pitch_deg, latest.pitch_deg, 0.0),
            });
        };
        let span = latest.tick.saturating_sub(prev.tick);
        let alpha = if span == 0 {
            1.0
        } else {
            ((f64::from(render_tick) - prev.tick as f64) / span as f64).clamp(0.0, 1.0) as f32
        };
        Some(SampledEntityPose {
            pos: [
                prev.pos[0] + (latest.pos[0] - prev.pos[0]) * alpha,
                prev.pos[1] + (latest.pos[1] - prev.pos[1]) * alpha,
                prev.pos[2] + (latest.pos[2] - prev.pos[2]) * alpha,
            ],
            yaw_deg: lerp_angle_deg(prev.yaw_deg, latest.yaw_deg, alpha),
            pitch_deg: lerp_angle_deg(prev.pitch_deg, latest.pitch_deg, alpha),
        })
    }
}

/// Quantise `v` to an i16 fixed-point word at `scale`, clamped to
/// `±limit` (in unscaled units) first so the result always fits.
fn quant_i16(v: f32, scale: f32, limit: f32) -> i16 {
    (v.clamp(-limit, limit) * scale).round() as i16
}

/// Quantise `v` to an i8 fixed-point word at `scale`, clamped to
/// `±limit` first so the result always fits.
fn quant_i8(v: f32, scale: f32, limit: f32) -> i8 {
    (v.clamp(-limit, limit) * scale).round() as i8
}

fn pack_i16_pair(lo: i16, hi: i16) -> u32 {
    (lo as u16 as u32) | ((hi as u16 as u32) << 16)
}

fn unpack_i16_lo(w: u32) -> i16 {
    w as u16 as i16
}

fn unpack_i16_hi(w: u32) -> i16 {
    (w >> 16) as u16 as i16
}

fn pack_rgba(rgba: [u8; 4]) -> u32 {
    u32::from(rgba[0])
        | (u32::from(rgba[1]) << 8)
        | (u32::from(rgba[2]) << 16)
        | (u32::from(rgba[3]) << 24)
}

fn unpack_rgba(w: u32) -> [u8; 4] {
    [w as u8, (w >> 8) as u8, (w >> 16) as u8, (w >> 24) as u8]
}

/// Compact GPU particle/foliage/chunk-fade instance: 16 bytes, four
/// `u32` words, 4-byte aligned — small enough that thousands of dirty
/// instances cost the CPU a single sub-64 KiB upload while the vertex
/// shader drives motion, sway, and fade.
///
/// Bit layout:
/// * `pos_xy`: `pos_x` (lo i16) | `pos_y` (hi i16), 1/256-block units.
/// * `pos_z_fade_flags`: `pos_z` (lo i16) | `fade_alpha` (u8) | `flags` (u8).
/// * `vel_phase`: `vel_x` (i8) | `vel_y` (i8) | `vel_z` (i8), all in
///   1/16-block-per-tick units | `phase` (u8) for sway/drift seeding.
/// * `tint_rgba`: tint r|g|b|a bytes.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct GpuParticleInstance {
    /// `pos_x` (lo i16) | `pos_y` (hi i16).
    pub pos_xy: u32,
    /// `pos_z` (lo i16) | `fade_alpha` (u8, bits 16–23) | `flags` (u8,
    /// bits 24–31).
    pub pos_z_fade_flags: u32,
    /// `vel_x|vel_y|vel_z` (i8 bytes) | `phase` (u8, bits 24–31).
    pub vel_phase: u32,
    /// Tint r|g|b|a, one byte each.
    pub tint_rgba: u32,
}

impl GpuParticleInstance {
    /// Packed size in bytes — always [`GPU_INSTANCE_BYTES`].
    pub const SIZE_BYTES: usize = GPU_INSTANCE_BYTES;

    /// Pack a particle instance. `pos` is the shader-local world offset
    /// in blocks (clamped to ±128 at 1/256 resolution), `vel` the
    /// per-tick velocity (clamped to ±8 at 1/16 resolution).
    pub fn pack(
        pos: [f32; 3],
        vel: [f32; 3],
        tint_rgba: [u8; 4],
        fade_alpha: u8,
        phase: u8,
        flags: u8,
    ) -> Self {
        let qx = quant_i16(pos[0], PARTICLE_POS_SCALE, PARTICLE_POS_LIMIT_BLOCKS);
        let qy = quant_i16(pos[1], PARTICLE_POS_SCALE, PARTICLE_POS_LIMIT_BLOCKS);
        let qz = quant_i16(pos[2], PARTICLE_POS_SCALE, PARTICLE_POS_LIMIT_BLOCKS);
        let vx = quant_i8(vel[0], PARTICLE_VEL_SCALE, PARTICLE_VEL_LIMIT);
        let vy = quant_i8(vel[1], PARTICLE_VEL_SCALE, PARTICLE_VEL_LIMIT);
        let vz = quant_i8(vel[2], PARTICLE_VEL_SCALE, PARTICLE_VEL_LIMIT);
        Self {
            pos_xy: pack_i16_pair(qx, qy),
            pos_z_fade_flags: (qz as u16 as u32)
                | (u32::from(fade_alpha) << 16)
                | (u32::from(flags) << 24),
            vel_phase: (vx as u8 as u32)
                | ((vy as u8 as u32) << 8)
                | ((vz as u8 as u32) << 16)
                | (u32::from(phase) << 24),
            tint_rgba: pack_rgba(tint_rgba),
        }
    }

    /// Packed world offset in blocks.
    pub fn position(&self) -> [f32; 3] {
        [
            f32::from(unpack_i16_lo(self.pos_xy)) / PARTICLE_POS_SCALE,
            f32::from(unpack_i16_hi(self.pos_xy)) / PARTICLE_POS_SCALE,
            f32::from(unpack_i16_lo(self.pos_z_fade_flags)) / PARTICLE_POS_SCALE,
        ]
    }

    /// Packed velocity in blocks per tick.
    pub fn velocity(&self) -> [f32; 3] {
        [
            f32::from(self.vel_phase as u8 as i8) / PARTICLE_VEL_SCALE,
            f32::from((self.vel_phase >> 8) as u8 as i8) / PARTICLE_VEL_SCALE,
            f32::from((self.vel_phase >> 16) as u8 as i8) / PARTICLE_VEL_SCALE,
        ]
    }

    /// Fade alpha (0–255) blended by the shader during chunk fade-in.
    pub fn fade_alpha(&self) -> u8 {
        (self.pos_z_fade_flags >> 16) as u8
    }

    /// Per-instance flag byte (e.g. lit/unlit, sway enable).
    pub fn flags(&self) -> u8 {
        (self.pos_z_fade_flags >> 24) as u8
    }

    /// Phase byte seeding shader-side sway/drift.
    pub fn phase(&self) -> u8 {
        (self.vel_phase >> 24) as u8
    }

    /// Tint RGBA bytes.
    pub fn tint(&self) -> [u8; 4] {
        unpack_rgba(self.tint_rgba)
    }

    /// The four `u32` shader words, in struct order.
    pub fn as_words(&self) -> [u32; 4] {
        [
            self.pos_xy,
            self.pos_z_fade_flags,
            self.vel_phase,
            self.tint_rgba,
        ]
    }
}

/// Compact GPU weather-column instance (rain/snow streak column):
/// 16 bytes, four `u32` words, 4-byte aligned.
///
/// Bit layout:
/// * `column_xz`: column `x` (lo i16) | `z` (hi i16), whole blocks
///   relative to the weather region origin.
/// * `base_y_height`: `base_y` (lo i16 blocks) | column `height`
///   (hi u16 blocks).
/// * `params`: `intensity` (u8) | `phase` (u8) | `fall_speed` (i8,
///   1/16-block-per-tick units) | `kind` (u8: rain/snow/etc.).
/// * `tint_rgba`: tint r|g|b|a bytes.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct GpuWeatherColumnInstance {
    /// `column_x` (lo i16) | `column_z` (hi i16).
    pub column_xz: u32,
    /// `base_y` (lo i16) | `height` (hi u16).
    pub base_y_height: u32,
    /// `intensity` (u8) | `phase` (u8) | `fall_speed` (i8) | `kind` (u8).
    pub params: u32,
    /// Tint r|g|b|a, one byte each.
    pub tint_rgba: u32,
}

impl GpuWeatherColumnInstance {
    /// Packed size in bytes — always [`GPU_INSTANCE_BYTES`].
    pub const SIZE_BYTES: usize = GPU_INSTANCE_BYTES;

    /// Pack a weather column. `fall_speed` is in blocks per tick and
    /// clamps to ±8 at 1/16 resolution.
    #[allow(clippy::too_many_arguments)] // Flat fields mirror the on-GPU word layout.
    pub fn pack(
        column_xz: [i16; 2],
        base_y: i16,
        height: u16,
        intensity: u8,
        phase: u8,
        fall_speed: f32,
        kind: u8,
        tint_rgba: [u8; 4],
    ) -> Self {
        let speed = quant_i8(fall_speed, WEATHER_FALL_SPEED_SCALE, PARTICLE_VEL_LIMIT);
        Self {
            column_xz: pack_i16_pair(column_xz[0], column_xz[1]),
            base_y_height: (base_y as u16 as u32) | (u32::from(height) << 16),
            params: u32::from(intensity)
                | (u32::from(phase) << 8)
                | ((speed as u8 as u32) << 16)
                | (u32::from(kind) << 24),
            tint_rgba: pack_rgba(tint_rgba),
        }
    }

    /// Column x/z in blocks.
    pub fn column_xz(&self) -> [i16; 2] {
        [unpack_i16_lo(self.column_xz), unpack_i16_hi(self.column_xz)]
    }

    /// Column base y in blocks.
    pub fn base_y(&self) -> i16 {
        unpack_i16_lo(self.base_y_height)
    }

    /// Column height in blocks.
    pub fn height(&self) -> u16 {
        (self.base_y_height >> 16) as u16
    }

    /// Precipitation intensity, 0–255.
    pub fn intensity(&self) -> u8 {
        self.params as u8
    }

    /// Phase byte seeding shader-side streak offsets.
    pub fn phase(&self) -> u8 {
        (self.params >> 8) as u8
    }

    /// Fall speed in blocks per tick (negative = falling).
    pub fn fall_speed(&self) -> f32 {
        f32::from((self.params >> 16) as u8 as i8) / WEATHER_FALL_SPEED_SCALE
    }

    /// Precipitation kind tag (caller-defined, e.g. 0 = rain, 1 = snow).
    pub fn kind(&self) -> u8 {
        (self.params >> 24) as u8
    }

    /// Tint RGBA bytes.
    pub fn tint(&self) -> [u8; 4] {
        unpack_rgba(self.tint_rgba)
    }

    /// The four `u32` shader words, in struct order.
    pub fn as_words(&self) -> [u32; 4] {
        [
            self.column_xz,
            self.base_y_height,
            self.params,
            self.tint_rgba,
        ]
    }
}

/// Accumulates packed GPU instances and tracks the dirty tail so the
/// upload path only ships the newly-written slice each frame.
///
/// Usage per frame: [`reset`](Self::reset) (or reuse with
/// [`clear_dirty`](Self::clear_dirty) after upload), `push_*` the live
/// instances, upload [`dirty_particles`](Self::dirty_particles) /
/// [`dirty_weather_columns`](Self::dirty_weather_columns), then
/// `clear_dirty`.
#[derive(Clone, Debug, Default)]
pub struct GpuOffloadInstancePacker {
    particles: Vec<GpuParticleInstance>,
    weather: Vec<GpuWeatherColumnInstance>,
    particles_dirty_from: Option<usize>,
    weather_dirty_from: Option<usize>,
}

impl GpuOffloadInstancePacker {
    /// Empty packer with no dirty ranges.
    pub fn new() -> Self {
        Self::default()
    }

    /// Append a particle instance; returns its index.
    pub fn push_particle(&mut self, instance: GpuParticleInstance) -> usize {
        let index = self.particles.len();
        self.particles_dirty_from.get_or_insert(index);
        self.particles.push(instance);
        index
    }

    /// Append a weather column instance; returns its index.
    pub fn push_weather_column(&mut self, instance: GpuWeatherColumnInstance) -> usize {
        let index = self.weather.len();
        self.weather_dirty_from.get_or_insert(index);
        self.weather.push(instance);
        index
    }

    /// All packed particle instances.
    pub fn particles(&self) -> &[GpuParticleInstance] {
        &self.particles
    }

    /// All packed weather column instances.
    pub fn weather_columns(&self) -> &[GpuWeatherColumnInstance] {
        &self.weather
    }

    /// The still-unuploaded particle tail — `&particles[dirty_from..]`.
    /// Empty when nothing was pushed since the last `clear_dirty`.
    pub fn dirty_particles(&self) -> &[GpuParticleInstance] {
        match self.particles_dirty_from {
            Some(from) => &self.particles[from..],
            None => &[],
        }
    }

    /// The still-unuploaded weather-column tail.
    pub fn dirty_weather_columns(&self) -> &[GpuWeatherColumnInstance] {
        match self.weather_dirty_from {
            Some(from) => &self.weather[from..],
            None => &[],
        }
    }

    /// Dirty particle instances flattened to `u32` words for upload.
    pub fn dirty_particle_words(&self) -> Vec<u32> {
        self.dirty_particles()
            .iter()
            .flat_map(GpuParticleInstance::as_words)
            .collect()
    }

    /// Dirty weather-column instances flattened to `u32` words.
    pub fn dirty_weather_words(&self) -> Vec<u32> {
        self.dirty_weather_columns()
            .iter()
            .flat_map(GpuWeatherColumnInstance::as_words)
            .collect()
    }

    /// Mark the current dirty tails as uploaded; the next pushes start a
    /// fresh dirty range.
    pub fn clear_dirty(&mut self) {
        self.particles_dirty_from = None;
        self.weather_dirty_from = None;
    }

    /// Drop all instances and dirty ranges (e.g. on world change).
    pub fn reset(&mut self) {
        self.particles.clear();
        self.weather.clear();
        self.clear_dirty();
    }
}
