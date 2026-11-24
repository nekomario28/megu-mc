// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later

//! Generic colossal-creature and multi-block forge primitives — pure `std`,
//! zero new dependencies, 100% declarative, and entirely free of
//! mod-specific code.
//!
//! Five families cover the contracts shared by every big multi-part
//! creature mod and multi-block smelter mod — dragons, sea serpents,
//! hydras, raid bosses, and breath-charged alloy forges all plug into the
//! same data-driven surfaces:
//!
//! 1. [`GrowthStageSpec`]/[`GrowthStageCurve`]: five-stage (1..=5)
//!    declarative growth curve with configurable age thresholds, a smooth
//!    hitbox-scale lerp clamped to `0.35..=2.2`, max-health / melee-damage
//!    multipliers, a `flight_unlock_stage` gate, and per-stage breath
//!    reach.
//! 2. [`PartKind`]/[`PartOffsetSpec`]/[`PartAabb`]: multi-part hitboxes —
//!    [`evaluate_multipart_hitboxes`] rotates bone-local offsets through
//!    vanilla yaw/pitch into world-space AABBs scaled by the current
//!    stage, each carrying a `damage_multiplier` (heads `1.25`, armored
//!    torsos `0.9`, limbs `1.0`), and [`hit_test_parts`] resolves the
//!    nearest ray intersection into a [`PartHit`].
//! 3. [`MountableFlyerInput`]/[`MountableFlyerState`]/
//!    [`step_mountable_flyer`]: a generic 3D flying-mount controller —
//!    stage-gated takeoff (from the ground *or* mid-fall), yaw/pitch
//!    steering, strafe, dive boost, stamina-bounded hovering, smooth vs
//!    hard landing detection, a breath-active flag, and fail-closed
//!    NaN/extreme-speed bounds.
//! 4. [`ElementKind`]/[`ElementalBreathSpec`]/[`evaluate_breath_cone`]: a
//!    deterministic cone breath sweep from a mouth origin along the look
//!    direction — bounded to `<= 48` blocks of reach and `<= 256` block
//!    transforms per tick (plus a hard probe budget) so breath can never
//!    lag-bomb a server. Terrain effects are a declarative
//!    [`BlockTransformRule`] table; lightning-style specs chain-arc to
//!    conductive targets inside `chain_radius`.
//! 5. [`MultiBlockForgeSpec`]/[`MultiBlockForgeRuntime`]: a generic 3D
//!    structure validator (`3x3x3` shells, apertures, any shape declared
//!    as offset cells) driving a breath-charged heat reservoir and an
//!    alloy recipe table `(base_ingot, catalyst_item, element_kind,
//!    heat_cost) -> output_item` with deterministic progress, idle heat
//!    decay, and lossless item conservation.
//!
//! Clean-room note: every rule, constant, and pattern here is an original
//! declarative design mirroring generic genre contracts — no mod code,
//! constants, or data tables are ported from any third-party mod.

use std::collections::{BTreeMap, BTreeSet};

use crate::capability_bus::{
    BlockCoord, CapabilityItemStack, FaceDirection, ItemTransferHandler,
};

// ===========================================================================
// Shared bounds & orientation helpers
// ===========================================================================

/// Lowest growth stage index.
pub const MIN_GROWTH_STAGE: u8 = 1;
/// Highest growth stage index.
pub const MAX_GROWTH_STAGE: u8 = 5;

/// Smallest uniform part scale any stage may apply.
pub const MIN_STAGE_HITBOX_SCALE: f32 = 0.35;
/// Largest uniform part scale any stage may apply.
pub const MAX_STAGE_HITBOX_SCALE: f32 = 2.2;

/// Hard cap on breath reach — a breath cone can never exceed 48 blocks.
pub const MAX_BREATH_RANGE_BLOCKS: f32 = 48.0;
/// Hard cap on block transforms one breath tick may emit.
pub const MAX_BREATH_BLOCK_TRANSFORMS_PER_TICK: usize = 256;
/// Hard cap on `block_at` probes one breath tick may issue.
pub const MAX_BREATH_BLOCK_PROBES_PER_TICK: usize = 8_192;
/// Hard cap on cone candidate positions visited per breath tick — bounds
/// the sweep itself, not just the probe/transform output.
pub const MAX_BREATH_CANDIDATE_POSITIONS: usize = 32_768;

/// Largest pattern a single multi-block forge may declare.
pub const MAX_FORGE_PATTERN_CELLS: usize = 512;

/// Vanilla look-vector for `(yaw_rad, pitch_rad)`: yaw `0` faces `+Z`
/// (south) and increasing yaw turns toward `-X` (west); pitch is positive
/// downward, so `pitch = +pi/2` looks straight down.
///
/// Returns `[0, 0, 1]` (south, level) for non-finite input — fail-closed.
pub fn look_direction(yaw_rad: f32, pitch_rad: f32) -> [f64; 3] {
    if !yaw_rad.is_finite() || !pitch_rad.is_finite() {
        return [0.0, 0.0, 1.0];
    }
    let (sy, cy) = (yaw_rad as f64).sin_cos();
    let (sp, cp) = (pitch_rad as f64).sin_cos();
    [-sy * cp, -sp, cy * cp]
}

/// Rotate a bone-local `(left, up, forward)` offset by `pitch_rad` about
/// the local left axis (positive pitch noses the `+z` axis down), then by
/// `yaw_rad` about world up (vanilla convention: `+z` forward maps to
/// `(-sin yaw, 0, cos yaw)`, `+x` left maps to `(cos yaw, 0, sin yaw)`).
fn rotate_local_offset(local: [f64; 3], yaw_rad: f32, pitch_rad: f32) -> [f64; 3] {
    let (sp, cp) = (pitch_rad as f64).sin_cos();
    let (sy, cy) = (yaw_rad as f64).sin_cos();
    let y1 = local[1].mul_add(cp, -local[2] * sp);
    let z1 = local[1].mul_add(sp, local[2] * cp);
    let x1 = local[0];
    [x1.mul_add(cy, -z1 * sy), y1, x1.mul_add(sy, z1 * cy)]
}

/// Sanitize a stage scale into `0.35..=2.2`; non-finite or non-positive
/// input clamps to [`MIN_STAGE_HITBOX_SCALE`] — fail-closed.
fn sanitize_stage_scale(scale: f32) -> f32 {
    if scale.is_finite() && scale > 0.0 {
        scale.clamp(MIN_STAGE_HITBOX_SCALE, MAX_STAGE_HITBOX_SCALE)
    } else {
        MIN_STAGE_HITBOX_SCALE
    }
}

/// `Some(v)` when `v` is finite and non-negative, else `None`.
fn finite_nonneg(v: f32) -> Option<f32> {
    (v.is_finite() && v >= 0.0).then_some(v)
}

/// Clamp `v` into `-1..=1`; non-finite input becomes `0.0`.
fn clamp_intent(v: f32) -> f32 {
    if v.is_finite() {
        v.clamp(-1.0, 1.0)
    } else {
        0.0
    }
}

/// Wrap an angle into `(-pi, pi]`; non-finite input becomes `0.0`.
fn wrap_radians(v: f32) -> f32 {
    if !v.is_finite() {
        return 0.0;
    }
    let w = (v + std::f32::consts::PI).rem_euclid(std::f32::consts::TAU)
        - std::f32::consts::PI;
    if w == -std::f32::consts::PI {
        std::f32::consts::PI
    } else {
        w
    }
}

// ===========================================================================
// 1. Growth stages (1..=5)
// ===========================================================================

/// Declarative stat line for one growth stage. Stage anchors are sorted
/// by `min_age_ticks` inside [`GrowthStageCurve`]; the stage number is the
/// anchor's 1-based position.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GrowthStageSpec {
    /// First `age_ticks` (inclusive) this stage applies from.
    pub min_age_ticks: u64,
    /// Uniform scale applied to every part hitbox at this stage —
    /// normalized into `0.35..=2.2` by [`GrowthStageCurve::new`].
    pub hitbox_scale: f32,
    /// Multiplier applied to the spec's base `max_health`.
    pub max_health_multiplier: f32,
    /// Multiplier applied to base melee damage.
    pub melee_damage_multiplier: f32,
    /// Breath reach in blocks at this stage — additionally bounded by
    /// [`MAX_BREATH_RANGE_BLOCKS`] at evaluation time.
    pub breath_range_blocks: f32,
}

/// A five-anchor growth curve: stage `1` is the first anchor, stage `5`
/// the last. Scalar fields are linearly interpolated between anchors so a
/// growing creature scales smoothly instead of popping between stages.
///
/// Built via [`GrowthStageCurve::new`], which sorts anchors by
/// `min_age_ticks`, truncates to [`MAX_GROWTH_STAGE`] entries, guarantees
/// at least one anchor, and clamps every scale/multiplier into a finite
/// non-negative range (`hitbox_scale` into `0.35..=2.2`).
#[derive(Clone, Debug, PartialEq)]
pub struct GrowthStageCurve {
    /// Stage anchors sorted by `min_age_ticks`; `stages[i]` is stage
    /// `i + 1`. Never empty after construction.
    pub stages: Vec<GrowthStageSpec>,
    /// First stage (1-based) at which mounted flight unlocks.
    pub flight_unlock_stage: u8,
}

impl GrowthStageCurve {
    /// Normalize `stages` into a curve: sorted ascending by
    /// `min_age_ticks`, truncated to five anchors, every `hitbox_scale`
    /// clamped into `0.35..=2.2`, every other scalar clamped finite
    /// non-negative. An empty `stages` falls back to a single stage-1
    /// anchor at age `0` (scale `1.0`, multipliers `1.0`, breath `0`).
    /// `flight_unlock_stage` clamps into `1..=5`.
    pub fn new(mut stages: Vec<GrowthStageSpec>, flight_unlock_stage: u8) -> Self {
        stages.sort_by_key(|s| s.min_age_ticks);
        stages.truncate(MAX_GROWTH_STAGE as usize);
        for s in &mut stages {
            s.hitbox_scale = sanitize_stage_scale(s.hitbox_scale);
            s.max_health_multiplier = finite_nonneg(s.max_health_multiplier).unwrap_or(1.0);
            s.melee_damage_multiplier = finite_nonneg(s.melee_damage_multiplier).unwrap_or(1.0);
            s.breath_range_blocks = finite_nonneg(s.breath_range_blocks).unwrap_or(0.0);
        }
        if stages.is_empty() {
            stages.push(GrowthStageSpec {
                min_age_ticks: 0,
                hitbox_scale: 1.0,
                max_health_multiplier: 1.0,
                melee_damage_multiplier: 1.0,
                breath_range_blocks: 0.0,
            });
        }
        Self {
            stages,
            flight_unlock_stage: flight_unlock_stage.clamp(MIN_GROWTH_STAGE, MAX_GROWTH_STAGE),
        }
    }

    /// The 1-based stage `age_ticks` falls into: `1 +` the index of the
    /// last anchor whose `min_age_ticks <= age_ticks` (`1` before the
    /// first anchor).
    pub fn stage_for_age_ticks(&self, age_ticks: u64) -> u8 {
        let idx = self
            .stages
            .iter()
            .rposition(|s| s.min_age_ticks <= age_ticks)
            .unwrap_or(0);
        (idx + 1) as u8
    }

    /// Anchor for `stage` (clamped into `1..=stage_count`).
    pub fn spec_for_stage(&self, stage: u8) -> &GrowthStageSpec {
        let idx = usize::from(stage.saturating_sub(1)).min(self.stages.len() - 1);
        &self.stages[idx]
    }

    /// Anchor whose stage `age_ticks` currently occupies.
    pub fn spec_for_age_ticks(&self, age_ticks: u64) -> &GrowthStageSpec {
        self.spec_for_stage(self.stage_for_age_ticks(age_ticks))
    }

    /// Linear interpolation across anchors: `age` before the first
    /// anchor returns the first value, after the last the last value.
    /// Equal-threshold anchors resolve to the later anchor's value.
    fn lerp_at(&self, age_ticks: u64, pick: impl Fn(&GrowthStageSpec) -> f32) -> f32 {
        let stages = &self.stages;
        let i = stages
            .iter()
            .rposition(|s| s.min_age_ticks <= age_ticks)
            .unwrap_or(0);
        let cur = &stages[i];
        if i + 1 >= stages.len() {
            return pick(cur);
        }
        let next = &stages[i + 1];
        let span = next.min_age_ticks.saturating_sub(cur.min_age_ticks);
        if span == 0 || age_ticks >= next.min_age_ticks {
            return pick(next);
        }
        let frac = (age_ticks - cur.min_age_ticks) as f32 / span as f32;
        pick(cur) + (pick(next) - pick(cur)) * frac
    }

    /// Smooth hitbox scale in `0.35..=2.2` at `age_ticks`.
    pub fn hitbox_scale_at(&self, age_ticks: u64) -> f32 {
        self.lerp_at(age_ticks, |s| s.hitbox_scale)
    }

    /// Smooth max-health multiplier at `age_ticks`.
    pub fn max_health_multiplier_at(&self, age_ticks: u64) -> f32 {
        self.lerp_at(age_ticks, |s| s.max_health_multiplier)
    }

    /// Smooth melee-damage multiplier at `age_ticks`.
    pub fn melee_damage_multiplier_at(&self, age_ticks: u64) -> f32 {
        self.lerp_at(age_ticks, |s| s.melee_damage_multiplier)
    }

    /// Smooth breath reach (blocks) at `age_ticks`.
    pub fn breath_range_at(&self, age_ticks: u64) -> f32 {
        self.lerp_at(age_ticks, |s| s.breath_range_blocks)
    }

    /// `true` once `age_ticks` reaches `flight_unlock_stage`.
    pub fn flight_unlocked_at(&self, age_ticks: u64) -> bool {
        self.stage_for_age_ticks(age_ticks) >= self.flight_unlock_stage
    }
}

impl Default for GrowthStageCurve {
    /// Generic five-stage colossal curve: hatchling (`0.35` scale) through
    /// ancient (`2.2` scale) over ~14 in-game days, flight at stage 3.
    fn default() -> Self {
        Self::new(
            vec![
                GrowthStageSpec {
                    min_age_ticks: 0,
                    hitbox_scale: 0.35,
                    max_health_multiplier: 0.4,
                    melee_damage_multiplier: 0.4,
                    breath_range_blocks: 0.0,
                },
                GrowthStageSpec {
                    min_age_ticks: 24_000,
                    hitbox_scale: 0.7,
                    max_health_multiplier: 0.6,
                    melee_damage_multiplier: 0.7,
                    breath_range_blocks: 8.0,
                },
                GrowthStageSpec {
                    min_age_ticks: 72_000,
                    hitbox_scale: 1.0,
                    max_health_multiplier: 1.0,
                    melee_damage_multiplier: 1.0,
                    breath_range_blocks: 16.0,
                },
                GrowthStageSpec {
                    min_age_ticks: 168_000,
                    hitbox_scale: 1.6,
                    max_health_multiplier: 1.6,
                    melee_damage_multiplier: 1.6,
                    breath_range_blocks: 32.0,
                },
                GrowthStageSpec {
                    min_age_ticks: 336_000,
                    hitbox_scale: 2.2,
                    max_health_multiplier: 2.4,
                    melee_damage_multiplier: 2.4,
                    breath_range_blocks: 48.0,
                },
            ],
            3,
        )
    }
}

// ===========================================================================
// 2. Multi-part hitboxes
// ===========================================================================

/// Semantic identity of one sub-part — the kind drives the default
/// `damage_multiplier` and lets hit consumers distinguish headshots from
/// body blows without mod-specific names.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum PartKind {
    /// Primary head — weak point (`1.25x` default damage).
    Head,
    /// Neck segment(s).
    Neck,
    /// Central body — typically armored (`0.9x` default damage).
    Torso,
    /// Left wing (`1.0x`).
    LeftWing,
    /// Right wing (`1.0x`).
    RightWing,
    /// Tail segment `n` (`1.0x`) — numbered tip-to-root or root-to-tip at
    /// the spec's discretion; the id is stable within a layout.
    Tail(u8),
}

impl PartKind {
    /// Default per-kind damage multiplier: heads are weak points
    /// (`1.25`), torsos armored (`0.9`), everything else neutral (`1.0`).
    pub fn default_damage_multiplier(self) -> f32 {
        match self {
            Self::Head => 1.25,
            Self::Torso => 0.9,
            Self::Neck | Self::LeftWing | Self::RightWing | Self::Tail(_) => 1.0,
        }
    }
}

/// One bone-local sub-part declaration: where the part hangs off the
/// entity root at scale `1.0`, how big its AABB is, and an optional
/// damage-multiplier override (`None` = the [`PartKind`] default).
///
/// `local_offset` uses the entity frame `(left, up, forward)` — `+x` is
/// the creature's left, `+y` up, `+z` forward (matching vanilla, where a
/// yaw-`0` entity faces `+Z` and its left side is `+X`).
/// `half_extents` are applied per *world* axis — the AABB stays
/// axis-aligned and does not rotate, only the part's position does
/// (mirroring vanilla multi-part entity behavior).
#[derive(Clone, Debug, PartialEq)]
pub struct PartOffsetSpec {
    /// Which semantic part this is.
    pub kind: PartKind,
    /// Bone-local center offset `(left, up, forward)` at scale `1.0`.
    pub local_offset: [f64; 3],
    /// Axis-aligned AABB half extents at scale `1.0`.
    pub half_extents: [f64; 3],
    /// `Some(m)` overrides [`PartKind::default_damage_multiplier`];
    /// non-finite or negative values fall back to the kind default.
    pub damage_multiplier: Option<f32>,
}

/// A world-space sub-part AABB produced by
/// [`evaluate_multipart_hitboxes`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PartAabb {
    /// Semantic part identity.
    pub kind: PartKind,
    /// Index into the `parts` slice this AABB came from.
    pub part_index: usize,
    /// Inclusive minimum corner.
    pub min: [f64; 3],
    /// Inclusive maximum corner.
    pub max: [f64; 3],
    /// Resolved damage multiplier (override or kind default).
    pub damage_multiplier: f32,
}

/// A creature's complete sub-part layout at scale `1.0`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MultiPartHitbox {
    /// Bone-local part declarations.
    pub parts: Vec<PartOffsetSpec>,
}

/// Smallest usable half extent — keeps degenerate declarations from
/// producing zero-area (unhittable) boxes.
const MIN_PART_HALF_EXTENT: f64 = 0.05;

impl MultiPartHitbox {
    /// World-space AABBs at `origin`/`yaw_rad`/`pitch_rad`/`stage_scale` —
    /// delegates to [`evaluate_multipart_hitboxes`].
    pub fn evaluate(
        &self,
        origin: [f64; 3],
        yaw_rad: f32,
        pitch_rad: f32,
        stage_scale: f32,
    ) -> Vec<PartAabb> {
        evaluate_multipart_hitboxes(origin, yaw_rad, pitch_rad, stage_scale, &self.parts)
    }

    /// Nearest part intersected by the ray — delegates to
    /// [`hit_test_parts`] over `aabbs` previously produced by
    /// [`Self::evaluate`].
    pub fn hit_test(
        &self,
        aabbs: &[PartAabb],
        ray_origin: [f64; 3],
        ray_dir: [f64; 3],
        max_dist: f64,
    ) -> Option<PartHit> {
        hit_test_parts(ray_origin, ray_dir, max_dist, aabbs)
    }
}

/// Transform every [`PartOffsetSpec`] into a world-space [`PartAabb`]:
/// bone-local `(left, up, forward)` offsets rotate by `pitch_rad` then
/// `yaw_rad` (vanilla convention), scale by `stage_scale` (clamped into
/// `0.35..=2.2`), and translate onto `origin`. Parts with non-finite
/// local offsets are skipped; degenerate or non-finite half extents are
/// repaired to [`MIN_PART_HALF_EXTENT`] instead of producing unhittable
/// boxes.
pub fn evaluate_multipart_hitboxes(
    origin: [f64; 3],
    yaw_rad: f32,
    pitch_rad: f32,
    stage_scale: f32,
    parts: &[PartOffsetSpec],
) -> Vec<PartAabb> {
    let scale = sanitize_stage_scale(stage_scale) as f64;
    let mut out = Vec::with_capacity(parts.len());
    for (part_index, part) in parts.iter().enumerate() {
        if !part.local_offset.iter().all(|v| v.is_finite()) {
            continue;
        }
        let rotated = rotate_local_offset(part.local_offset, yaw_rad, pitch_rad);
        let mut min = [0.0; 3];
        let mut max = [0.0; 3];
        for axis in 0..3 {
            let center = origin[axis] + rotated[axis] * scale;
            let half = part.half_extents[axis];
            let half = (if half.is_finite() && half > 0.0 {
                half
            } else {
                MIN_PART_HALF_EXTENT
            }) * scale;
            min[axis] = center - half;
            max[axis] = center + half;
        }
        let damage_multiplier = part
            .damage_multiplier
            .and_then(finite_nonneg)
            .unwrap_or_else(|| part.kind.default_damage_multiplier());
        out.push(PartAabb {
            kind: part.kind,
            part_index,
            min,
            max,
            damage_multiplier,
        });
    }
    out
}

/// Nearest sub-part hit resolved by [`hit_test_parts`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PartHit {
    /// Semantic part identity.
    pub kind: PartKind,
    /// Index into the evaluated AABB list (= the declaring
    /// [`PartOffsetSpec`]'s index).
    pub part_index: usize,
    /// Ray parameter `t` of the entry face — in units of `ray_dir`
    /// length (equal to blocks when `ray_dir` is normalized).
    pub t: f64,
    /// World-space hit point on the part's entry face.
    pub point: [f64; 3],
    /// Damage multiplier the part forwards to the parent HP pool.
    pub damage_multiplier: f32,
}

/// Slab-method ray/AABB intersection: smallest `t` in `[0, max_t]` or
/// `None`.
fn ray_aabb(origin: [f64; 3], dir: [f64; 3], min: [f64; 3], max: [f64; 3], max_t: f64) -> Option<f64> {
    let mut t_enter = 0.0f64;
    let mut t_exit = max_t;
    for i in 0..3 {
        if dir[i].abs() < f64::EPSILON {
            if origin[i] < min[i] || origin[i] > max[i] {
                return None;
            }
            continue;
        }
        let inv = 1.0 / dir[i];
        let mut t1 = (min[i] - origin[i]) * inv;
        let mut t2 = (max[i] - origin[i]) * inv;
        if t1 > t2 {
            std::mem::swap(&mut t1, &mut t2);
        }
        t_enter = t_enter.max(t1);
        t_exit = t_exit.min(t2);
        if t_enter > t_exit {
            return None;
        }
    }
    Some(t_enter)
}

/// Nearest [`PartAabb`] intersected by `ray_origin + t * ray_dir` with
/// `t <= max_dist`. `ray_dir` need not be normalized — `t` is in units of
/// its length. Returns `None` for non-finite inputs, a zero direction,
/// `max_dist <= 0`, or a clean miss — fail-closed.
pub fn hit_test_parts(
    ray_origin: [f64; 3],
    ray_dir: [f64; 3],
    max_dist: f64,
    aabbs: &[PartAabb],
) -> Option<PartHit> {
    if !(max_dist.is_finite() && max_dist > 0.0)
        || !ray_origin.iter().all(|v| v.is_finite())
        || !ray_dir.iter().all(|v| v.is_finite())
        || ray_dir.iter().all(|v| v.abs() < f64::EPSILON)
    {
        return None;
    }
    let mut best: Option<usize> = None;
    let mut best_t = f64::INFINITY;
    for (i, aabb) in aabbs.iter().enumerate() {
        if let Some(t) = ray_aabb(ray_origin, ray_dir, aabb.min, aabb.max, max_dist) {
            if t < best_t {
                best_t = t;
                best = Some(i);
            }
        }
    }
    best.map(|i| {
        let aabb = aabbs[i];
        PartHit {
            kind: aabb.kind,
            part_index: aabb.part_index,
            t: best_t,
            point: [
                ray_origin[0] + ray_dir[0] * best_t,
                ray_origin[1] + ray_dir[1] * best_t,
                ray_origin[2] + ray_dir[2] * best_t,
            ],
            damage_multiplier: aabb.damage_multiplier,
        }
    })
}

// ===========================================================================
// 3. Mounted flight
// ===========================================================================

/// Tuning contract for [`step_mountable_flyer`]. All speeds are blocks per
/// tick; angles are radians.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MountableFlyerSpec {
    /// Minimum growth stage allowed to take off.
    pub flight_unlock_stage: u8,
    /// Full-speed forward flight (blocks/tick).
    pub fly_speed: f64,
    /// Strafe speed as a fraction of `fly_speed` (`0..=1`).
    pub strafe_speed_scale: f64,
    /// Upward speed while `ascend` is held.
    pub climb_speed: f64,
    /// Downward speed while `descend` is held.
    pub descend_speed: f64,
    /// Speed multiplier while `dive_boost` is held and pitch is at or
    /// past `dive_min_pitch_rad` downward.
    pub dive_speed_multiplier: f64,
    /// Positive-down pitch (radians) that counts as diving.
    pub dive_min_pitch_rad: f32,
    /// Upward velocity applied on takeoff.
    pub takeoff_impulse: f64,
    /// Maximum yaw change per tick.
    pub yaw_rate_rad: f32,
    /// Maximum pitch change per tick; pitch clamps to `+/-pi/2`.
    pub pitch_rate_rad: f32,
    /// Gravity for an unpowered fall (grounded-state falling before
    /// flight engages).
    pub gravity_per_tick: f64,
    /// Terminal velocity for unpowered falls.
    pub terminal_fall_speed: f64,
    /// Gentle sink while hovering with zero control input (`0` = perfect
    /// hover).
    pub hover_sink_speed: f64,
    /// Forced descent once stamina empties — an exhausted mount sinks
    /// instead of hanging in the air.
    pub fatigue_sink_speed: f64,
    /// `|vel.y|` at touchdown above which the landing counts as hard.
    pub hard_landing_speed: f64,
    /// Hard cap on total velocity magnitude — fail-closed bound against
    /// extreme-speed inputs.
    pub max_speed: f64,
    /// Stamina reservoir size.
    pub max_stamina: f32,
    /// Per-tick drain while airborne.
    pub stamina_drain_per_tick: f32,
    /// Additional per-tick drain while hovering with no steering input.
    pub stamina_hover_extra_drain: f32,
    /// Per-tick regen while grounded.
    pub stamina_regen_per_tick: f32,
    /// Whether the breath flag may raise while grounded.
    pub breath_while_grounded: bool,
    /// Whether the breath flag may raise while airborne.
    pub breath_while_flying: bool,
}

impl Default for MountableFlyerSpec {
    /// Reasonable generic flyer: stage-3 flight unlock, `0.8` blocks/tick
    /// cruise, 2.5-second stamina reservoir, gentle hover.
    fn default() -> Self {
        Self {
            flight_unlock_stage: 3,
            fly_speed: 0.8,
            strafe_speed_scale: 0.6,
            climb_speed: 0.5,
            descend_speed: 0.7,
            dive_speed_multiplier: 1.8,
            dive_min_pitch_rad: 0.35,
            takeoff_impulse: 0.9,
            yaw_rate_rad: 0.12,
            pitch_rate_rad: 0.15,
            gravity_per_tick: 0.08,
            terminal_fall_speed: 3.92,
            hover_sink_speed: 0.0,
            fatigue_sink_speed: 0.35,
            hard_landing_speed: 1.2,
            max_speed: 4.0,
            max_stamina: 50.0,
            stamina_drain_per_tick: 1.0,
            stamina_hover_extra_drain: 0.5,
            stamina_regen_per_tick: 2.5,
            breath_while_grounded: true,
            breath_while_flying: true,
        }
    }
}

/// One tick of rider + world input to [`step_mountable_flyer`].
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct MountableFlyerInput {
    /// Rider's current growth stage (`clamped 1..=5`); takeoff requires
    /// `>= spec.flight_unlock_stage`.
    pub growth_stage: u8,
    /// Forward/back intent `-1..=1`.
    pub forward: f32,
    /// Left/right strafe intent `-1..=1` (positive = left).
    pub strafe: f32,
    /// Desired yaw change this tick, clamped to `+/-yaw_rate_rad`.
    pub yaw_delta_rad: f32,
    /// Desired absolute pitch, approached at `pitch_rate_rad`.
    pub pitch_target_rad: f32,
    /// Jump/ascend — takeoff from ground or mid-fall, climb while flying.
    pub ascend: bool,
    /// Sneak/descend — faster descent while flying.
    pub descend: bool,
    /// Sprint/dive boost.
    pub dive_boost: bool,
    /// Rider is commanding breath this tick.
    pub breath_pressed: bool,
    /// World-reported ground contact this tick.
    pub on_ground: bool,
}

/// Mutable per-mount flight state.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MountableFlyerState {
    /// Current position.
    pub pos: [f64; 3],
    /// Current velocity (blocks/tick).
    pub vel: [f64; 3],
    /// Facing (vanilla yaw, positive-down pitch convention).
    pub yaw_rad: f32,
    /// Look pitch — positive = nose down.
    pub pitch_rad: f32,
    /// `true` once taken off until a smooth/hard landing.
    pub airborne: bool,
    /// Remaining flight stamina `0..=spec.max_stamina`.
    pub stamina: f32,
    /// `true` while the rider's breath command is currently honored —
    /// callers use this to drive [`evaluate_breath_cone`].
    pub breath_active: bool,
}

impl MountableFlyerState {
    /// A grounded mount at `pos` facing `yaw_rad` with full stamina `1.0`.
    pub fn new_grounded(pos: [f64; 3], yaw_rad: f32, max_stamina: f32) -> Self {
        Self {
            pos,
            vel: [0.0; 3],
            yaw_rad,
            pitch_rad: 0.0,
            airborne: false,
            stamina: max_stamina.max(0.0),
            breath_active: false,
        }
    }
}

/// What happened during one [`step_mountable_flyer`] call.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FlyerStepEvents {
    /// The mount left the ground this tick.
    pub took_off: bool,
    /// `ascend` was pressed but `growth_stage < flight_unlock_stage`.
    pub takeoff_refused_low_stage: bool,
    /// The mount touched down this tick.
    pub landed: bool,
    /// Touchdown vertical speed exceeded `hard_landing_speed`.
    pub hard_landing: bool,
    /// Stamina reached `0` this tick — the mount is gliding down.
    pub stamina_empty: bool,
    /// Non-finite/oversized input or state was repaired this tick.
    pub sanitized: bool,
}

/// Advance one mounted flyer by one tick.
///
/// Order of operations:
///
/// 1. **Sanitize** — non-finite `pos`/`vel`/`yaw`/`pitch`/`stamina` and
///    every non-finite input scalar are repaired (`sanitized` flag); the
///    mount never propagates NaN.
/// 2. **Steering** — `yaw_delta` clamps to `+/-yaw_rate_rad`, pitch
///    approaches `pitch_target_rad` at `pitch_rate_rad`; applies on the
///    ground too (a grounded mount can turn).
/// 3. **Landing** — `airborne && on_ground` lands, flagging `hard_landing`
///    when touchdown `|vel.y|` exceeded `hard_landing_speed`.
/// 4. **Grounded** — stamina regens; `ascend` takes off when
///    `growth_stage >= flight_unlock_stage` (otherwise
///    `takeoff_refused_low_stage`). A mount that is off the ground but not
///    yet flying falls under `gravity_per_tick` — pressing `ascend`
///    mid-fall still engages flight.
/// 5. **Airborne** — stamina drains (extra while hovering), the look
///    direction drives forward/strafe velocity, `ascend`/`descend` add
///    vertical authority, `dive_boost` past `dive_min_pitch_rad`
///    multiplies speed, an empty reservoir forces `fatigue_sink_speed`
///    descent, and total speed clamps to `max_speed`.
/// 6. **Integrate** — `pos += vel`; the breath flag tracks
///    `breath_pressed` gated on the airborne/grounded breath policy.
pub fn step_mountable_flyer(
    state: &mut MountableFlyerState,
    spec: &MountableFlyerSpec,
    input: &MountableFlyerInput,
) -> FlyerStepEvents {
    let mut ev = FlyerStepEvents::default();

    // 1. Sanitize state + inputs — fail closed, never propagate NaN.
    for c in &mut state.vel {
        if !c.is_finite() {
            *c = 0.0;
            ev.sanitized = true;
        }
    }
    for c in &mut state.pos {
        if !c.is_finite() {
            *c = 0.0;
            ev.sanitized = true;
        }
    }
    if !state.yaw_rad.is_finite() {
        state.yaw_rad = 0.0;
        ev.sanitized = true;
    }
    if !state.pitch_rad.is_finite() {
        state.pitch_rad = 0.0;
        ev.sanitized = true;
    }
    if !(state.stamina.is_finite() && state.stamina >= 0.0) {
        state.stamina = 0.0;
        ev.sanitized = true;
    }
    state.stamina = state.stamina.min(spec.max_stamina.max(0.0));

    let forward = clamp_intent(input.forward);
    let strafe = clamp_intent(input.strafe);
    if forward != input.forward || strafe != input.strafe {
        ev.sanitized = true;
    }
    let stage = input
        .growth_stage
        .clamp(MIN_GROWTH_STAGE, MAX_GROWTH_STAGE);

    // 2. Steering (works on the ground too — a mount can turn in place).
    let yaw_delta = if input.yaw_delta_rad.is_finite() {
        input.yaw_delta_rad.clamp(-spec.yaw_rate_rad, spec.yaw_rate_rad)
    } else {
        ev.sanitized = true;
        0.0
    };
    state.yaw_rad = wrap_radians(state.yaw_rad + yaw_delta);
    let pitch_target = if input.pitch_target_rad.is_finite() {
        input
            .pitch_target_rad
            .clamp(-std::f32::consts::FRAC_PI_2, std::f32::consts::FRAC_PI_2)
    } else {
        ev.sanitized = true;
        state.pitch_rad
    };
    let dp = pitch_target - state.pitch_rad;
    state.pitch_rad = (state.pitch_rad + dp.clamp(-spec.pitch_rate_rad, spec.pitch_rate_rad))
        .clamp(-std::f32::consts::FRAC_PI_2, std::f32::consts::FRAC_PI_2);

    // 3. Landing — ground contact while flying always lands.
    if state.airborne && input.on_ground {
        ev.landed = true;
        ev.hard_landing = state.vel[1] < -spec.hard_landing_speed;
        state.airborne = false;
        state.vel = [0.0; 3];
    }

    // 4. Grounded — regen, gravity for off-edge walks, stage-gated takeoff.
    if !state.airborne {
        if input.on_ground {
            state.vel = [0.0; 3];
            state.stamina = (state.stamina + spec.stamina_regen_per_tick.max(0.0))
                .min(spec.max_stamina.max(0.0));
        } else {
            state.vel[0] = 0.0;
            state.vel[2] = 0.0;
            state.vel[1] = (state.vel[1] - spec.gravity_per_tick.max(0.0))
                .max(-spec.terminal_fall_speed.max(0.0));
        }
        if input.ascend {
            if stage >= spec.flight_unlock_stage {
                state.airborne = true;
                state.vel = [0.0, spec.takeoff_impulse.max(0.0), 0.0];
                ev.took_off = true;
            } else {
                ev.takeoff_refused_low_stage = true;
            }
        }
    }

    // 5. Airborne steering — skipped on the takeoff tick itself so the
    // impulse is delivered intact.
    if state.airborne && !ev.took_off {
        let hovering = forward == 0.0 && strafe == 0.0 && !input.ascend && !input.descend;
        let drain = spec.stamina_drain_per_tick.max(0.0)
            + if hovering {
                spec.stamina_hover_extra_drain.max(0.0)
            } else {
                0.0
            };
        let was = state.stamina;
        state.stamina = (state.stamina - drain).max(0.0);
        if was > 0.0 && state.stamina == 0.0 {
            ev.stamina_empty = true;
        }

        let look = look_direction(state.yaw_rad, state.pitch_rad);
        let (sy, cy) = state.yaw_rad.sin_cos();
        let left = [cy as f64, 0.0, sy as f64];
        let diving = input.dive_boost
            && forward > 0.0
            && state.pitch_rad >= spec.dive_min_pitch_rad;
        let speed = spec.fly_speed.max(0.0)
            * if diving {
                spec.dive_speed_multiplier.max(1.0)
            } else {
                1.0
            };
        let strafe_scale = spec.strafe_speed_scale.clamp(0.0, 1.0);
        let strafe_v = f64::from(strafe) * strafe_scale * speed;
        let fwd_v = f64::from(forward) * speed;
        let mut vel = [
            look[0] * fwd_v + left[0] * strafe_v,
            look[1] * fwd_v,
            look[2] * fwd_v + left[2] * strafe_v,
        ];
        if input.ascend && state.stamina > 0.0 {
            vel[1] += spec.climb_speed.max(0.0);
        }
        if input.descend {
            vel[1] -= spec.descend_speed.max(0.0);
        }
        if hovering {
            vel[1] = -spec.hover_sink_speed.max(0.0);
        }
        if state.stamina <= 0.0 {
            vel[1] = vel[1].min(-spec.fatigue_sink_speed.max(0.0));
        }
        state.vel = vel;
    }

    // 6. Fail-closed speed cap + integrate + breath flag.
    let speed_sq = state.vel[0] * state.vel[0]
        + state.vel[1] * state.vel[1]
        + state.vel[2] * state.vel[2];
    let max_speed = spec.max_speed.max(0.0);
    if speed_sq.is_finite() && speed_sq > max_speed * max_speed {
        let k = max_speed / speed_sq.sqrt();
        for c in &mut state.vel {
            *c *= k;
        }
        ev.sanitized = true;
    }
    state.pos[0] += state.vel[0];
    state.pos[1] += state.vel[1];
    state.pos[2] += state.vel[2];
    state.breath_active = input.breath_pressed
        && if state.airborne {
            spec.breath_while_flying
        } else {
            spec.breath_while_grounded
        };

    ev
}

// ===========================================================================
// 4. Elemental breath & terrain transform
// ===========================================================================

/// Breath element identity — the three classic kinds plus an open
/// namespace for mods that invent their own.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ElementKind {
    /// Charring/igniting breath.
    Fire,
    /// Freezing breath.
    Ice,
    /// Arcing breath — chains between conductive targets.
    Lightning,
    /// Mod-defined element (`0..=65535`).
    Custom(u16),
}

/// A probed world block: its Save-v2 id plus the tags it carries (for
/// `#tag` rules and expectations).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BlockProbe {
    /// Block id (e.g. `"minecraft:stone"`).
    pub id: String,
    /// Tag ids the block belongs to (`"namespace:tag"`).
    pub tags: BTreeSet<String>,
}

/// Declarative terrain-transform rule: `from` is a literal block id or a
/// `#namespace:tag` reference; `to` is the resulting block id.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlockTransformRule {
    /// `"namespace:block"` literal or `"#namespace:tag"`.
    pub from: String,
    /// Replacement block id.
    pub to: String,
}

impl BlockTransformRule {
    fn matches(&self, probe: &BlockProbe) -> bool {
        match self.from.strip_prefix('#') {
            Some(tag) => probe.tags.contains(tag),
            None => probe.id == self.from,
        }
    }
}

/// Declarative breath profile — the element, reach, aperture, damage, and
/// terrain-transform table one creature's breath uses.
#[derive(Clone, Debug, PartialEq)]
pub struct ElementalBreathSpec {
    /// Element identity (drives presets and forge compatibility).
    pub element: ElementKind,
    /// Reach in blocks — clamped to `0..=48` at evaluation.
    pub range_blocks: f32,
    /// Cone half-angle in radians, clamped to `0..=1.2` (`~68.8 deg`) —
    /// wider angles are physically meaningless for a directed breath.
    pub cone_half_angle_rad: f32,
    /// Damage dealt to each entity in the cone per evaluation.
    pub damage: f32,
    /// Ordered terrain-transform table; the first matching rule wins.
    pub block_rules: Vec<BlockTransformRule>,
    /// Per-tick block-transform budget — clamped to `<= 256`.
    pub max_block_transforms_per_tick: usize,
    /// Chain-arc radius: a hit target arcs to the nearest un-hit
    /// conductive entity within this distance.
    pub chain_radius: f32,
    /// Maximum chained (non-direct) hits per evaluation; `0` disables
    /// chaining.
    pub chain_max_targets: usize,
    /// Damage multiplier applied per chain hop (`0.8` = each arc deals
    /// 80% of the previous hop).
    pub chain_damage_falloff: f32,
}

impl ElementalBreathSpec {
    /// Generic fire breath: chars grass/dirt into ash and stone into a
    /// scorched variant, 16-block reach, no chaining.
    pub fn fire() -> Self {
        Self {
            element: ElementKind::Fire,
            range_blocks: 16.0,
            cone_half_angle_rad: 0.35,
            damage: 2.0,
            block_rules: vec![
                BlockTransformRule {
                    from: "minecraft:grass_block".to_string(),
                    to: "minecraft:dirt".to_string(),
                },
                BlockTransformRule {
                    from: "minecraft:dirt".to_string(),
                    to: "anyforge:charred_dirt".to_string(),
                },
                BlockTransformRule {
                    from: "minecraft:stone".to_string(),
                    to: "anyforge:scorched_stone".to_string(),
                },
            ],
            max_block_transforms_per_tick: MAX_BREATH_BLOCK_TRANSFORMS_PER_TICK,
            chain_radius: 0.0,
            chain_max_targets: 0,
            chain_damage_falloff: 1.0,
        }
    }

    /// Generic ice breath: freezes water into ice and stone into a frozen
    /// variant, 16-block reach, no chaining.
    pub fn ice() -> Self {
        Self {
            element: ElementKind::Ice,
            range_blocks: 16.0,
            cone_half_angle_rad: 0.35,
            damage: 1.5,
            block_rules: vec![
                BlockTransformRule {
                    from: "minecraft:water".to_string(),
                    to: "minecraft:ice".to_string(),
                },
                BlockTransformRule {
                    from: "minecraft:stone".to_string(),
                    to: "anyforge:frozen_stone".to_string(),
                },
            ],
            max_block_transforms_per_tick: MAX_BREATH_BLOCK_TRANSFORMS_PER_TICK,
            chain_radius: 0.0,
            chain_max_targets: 0,
            chain_damage_falloff: 1.0,
        }
    }

    /// Generic lightning breath: no terrain transforms, but each hit arcs
    /// to conductive entities within `4` blocks, up to `4` chained hits
    /// at `80%` falloff per hop.
    pub fn lightning() -> Self {
        Self {
            element: ElementKind::Lightning,
            range_blocks: 20.0,
            cone_half_angle_rad: 0.25,
            damage: 3.0,
            block_rules: Vec::new(),
            max_block_transforms_per_tick: MAX_BREATH_BLOCK_TRANSFORMS_PER_TICK,
            chain_radius: 4.0,
            chain_max_targets: 4,
            chain_damage_falloff: 0.8,
        }
    }
}

/// An entity that may be caught in a breath cone.
#[derive(Clone, Debug, PartialEq)]
pub struct BreathEntity {
    /// Runtime entity id.
    pub entity_id: u64,
    /// World-space center.
    pub center: [f64; 3],
    /// AABB half extents — the largest component is the cone-test radius.
    pub half_extents: [f64; 3],
    /// Whether chain-arc breath can jump to this entity (metal-armored,
    /// wet, or otherwise conductive targets).
    pub conductive: bool,
}

/// One entity struck by a breath evaluation.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BreathEntityHit {
    /// Struck entity.
    pub entity_id: u64,
    /// Damage dealt — `spec.damage` for direct hits,
    /// `damage * falloff^hop` for chained arcs.
    pub damage: f32,
    /// Axial distance from `mouth_origin` for direct cone hits; arc
    /// distance from the previous hop's target for chained hits.
    pub distance: f64,
    /// `true` when the hit arrived via chain arc rather than the cone.
    pub chained: bool,
}

/// One terrain transform emitted by a breath evaluation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BreathBlockTransform {
    /// Block position.
    pub pos: [i32; 3],
    /// Block id found.
    pub from: String,
    /// Block id to place.
    pub to: String,
}

/// Result of one [`evaluate_breath_cone`] call.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct BreathOutcome {
    /// Entities inside the cone or reached by chain arcs, sorted by
    /// `(distance, entity_id)`.
    pub entity_hits: Vec<BreathEntityHit>,
    /// Block transforms to apply, in deterministic sweep order — never
    /// more than `min(spec.max_block_transforms_per_tick, 256)`.
    pub block_transforms: Vec<BreathBlockTransform>,
    /// `true` when a safety bound (transform cap, probe budget, or
    /// candidate budget) stopped the sweep early.
    pub truncated: bool,
}

/// Deterministic cone breath sweep from `mouth_origin` along the
/// `(yaw_rad, pitch_rad)` look direction.
///
/// * `spec.range_blocks` clamps to `0..=48`; non-finite or non-positive
///   range, or a non-finite origin/direction, yields an empty outcome —
///   fail-closed.
/// * The sweep walks the cone axis at half-block steps; candidate block
///   positions are visited in deterministic (shell distance, then
///   lexicographic) order, deduplicated, and each is probed at most once.
/// * Transforms come from the first matching [`BlockTransformRule`];
///   output is capped at `min(spec.max_block_transforms_per_tick, 256)`
///   and the whole sweep halts at hard candidate/probe budgets, so one
///   breath tick can never lag-bomb a server (`truncated` reports an
///   early stop).
/// * Entity hits test each center's perpendicular distance against the
///   cone radius at its axial projection, then chain-arc to conductive
///   entities per `chain_radius`/`chain_max_targets`/
///   `chain_damage_falloff` — chain results are bounded, sorted, and
///   hop-deduped.
pub fn evaluate_breath_cone(
    mouth_origin: [f64; 3],
    yaw_rad: f32,
    pitch_rad: f32,
    spec: &ElementalBreathSpec,
    entities: &[BreathEntity],
    block_at: &mut dyn FnMut([i32; 3]) -> Option<BlockProbe>,
) -> BreathOutcome {
    let mut out = BreathOutcome::default();
    if !mouth_origin.iter().all(|v| v.is_finite())
        || !yaw_rad.is_finite()
        || !pitch_rad.is_finite()
    {
        return out;
    }
    let range = if spec.range_blocks.is_finite() {
        spec.range_blocks.clamp(0.0, MAX_BREATH_RANGE_BLOCKS)
    } else {
        0.0
    };
    if range <= 0.0 {
        return out;
    }
    let dir = look_direction(yaw_rad, pitch_rad);
    let half_angle = if spec.cone_half_angle_rad.is_finite() {
        spec.cone_half_angle_rad.clamp(0.0, 1.2)
    } else {
        0.0
    };
    let tan_a = (half_angle as f64).tan();
    let cap = spec
        .max_block_transforms_per_tick
        .min(MAX_BREATH_BLOCK_TRANSFORMS_PER_TICK);

    // --- Terrain sweep -------------------------------------------------
    let mut probes = 0usize;
    let mut candidates = 0usize;
    let mut cache: BTreeMap<[i32; 3], Option<BlockProbe>> = BTreeMap::new();
    let mut matched = 0usize;
    let step = 0.5;
    let mut d = step;
    'sweep: while d <= f64::from(range) {
        let center = [
            mouth_origin[0] + dir[0] * d,
            mouth_origin[1] + dir[1] * d,
            mouth_origin[2] + dir[2] * d,
        ];
        // Box radius carries a +0.5 margin over the cone radius so no
        // in-cone block can fall between two axis shells.
        let shell_r = tan_a * d + 0.5 + 0.5;
        let lo = [
            (center[0] - shell_r).floor() as i32,
            (center[1] - shell_r).floor() as i32,
            (center[2] - shell_r).floor() as i32,
        ];
        let hi = [
            (center[0] + shell_r).ceil() as i32 - 1,
            (center[1] + shell_r).ceil() as i32 - 1,
            (center[2] + shell_r).ceil() as i32 - 1,
        ];
        for bx in lo[0]..=hi[0] {
            for by in lo[1]..=hi[1] {
                for bz in lo[2]..=hi[2] {
                    if cache.contains_key(&[bx, by, bz]) {
                        continue;
                    }
                    candidates += 1;
                    if candidates > MAX_BREATH_CANDIDATE_POSITIONS {
                        out.truncated = true;
                        break 'sweep;
                    }
                    let q = [bx as f64 + 0.5, by as f64 + 0.5, bz as f64 + 0.5];
                    let v = [q[0] - mouth_origin[0], q[1] - mouth_origin[1], q[2] - mouth_origin[2]];
                    let t = v[0] * dir[0] + v[1] * dir[1] + v[2] * dir[2];
                    if !(0.0..=f64::from(range) + 0.5).contains(&t) {
                        continue;
                    }
                    let perp_sq = (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]) - t * t;
                    let cone_r = tan_a * t.max(0.0) + 0.5;
                    if perp_sq > cone_r * cone_r {
                        continue;
                    }
                    probes += 1;
                    if probes > MAX_BREATH_BLOCK_PROBES_PER_TICK {
                        out.truncated = true;
                        break 'sweep;
                    }
                    let probe = block_at([bx, by, bz]);
                    if let Some(p) = &probe {
                        if let Some(rule) = spec.block_rules.iter().find(|r| r.matches(p)) {
                            matched += 1;
                            if matched > cap {
                                out.truncated = true;
                                break 'sweep;
                            }
                            out.block_transforms.push(BreathBlockTransform {
                                pos: [bx, by, bz],
                                from: p.id.clone(),
                                to: rule.to.clone(),
                            });
                        }
                    }
                    cache.insert([bx, by, bz], probe);
                }
            }
        }
        d += step;
    }

    // --- Entity cone hits ----------------------------------------------
    let mut direct: Vec<(u64, f64)> = Vec::new(); // (entity_id, t)
    let mut seen: BTreeSet<u64> = BTreeSet::new();
    for e in entities {
        if !e.center.iter().all(|v| v.is_finite()) || !seen.insert(e.entity_id) {
            continue;
        }
        let r_ent = e
            .half_extents
            .iter()
            .copied()
            .filter(|v| v.is_finite() && *v > 0.0)
            .fold(0.0f64, f64::max);
        let v = [
            e.center[0] - mouth_origin[0],
            e.center[1] - mouth_origin[1],
            e.center[2] - mouth_origin[2],
        ];
        let len_sq = v[0] * v[0] + v[1] * v[1] + v[2] * v[2];
        let t = v[0] * dir[0] + v[1] * dir[1] + v[2] * dir[2];
        let hit = if t < 0.0 {
            len_sq <= r_ent * r_ent
        } else {
            let reach = f64::from(range) + r_ent;
            if t > reach {
                false
            } else {
                let perp_sq = (len_sq - t * t).max(0.0);
                let cone_r = tan_a * t + 0.5 + r_ent;
                perp_sq <= cone_r * cone_r
            }
        };
        if hit {
            direct.push((e.entity_id, t.max(0.0)));
        }
    }
    direct.sort_by(|a, b| {
        a.1.partial_cmp(&b.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.0.cmp(&b.0))
    });
    let mut hit_ids: BTreeSet<u64> = BTreeSet::new();
    let mut hit_pos: BTreeMap<u64, [f64; 3]> = BTreeMap::new();
    let mut hit_depth: BTreeMap<u64, u32> = BTreeMap::new();
    for (id, t) in direct {
        hit_ids.insert(id);
        hit_depth.insert(id, 0);
        if let Some(e) = entities.iter().find(|e| e.entity_id == id) {
            hit_pos.insert(id, e.center);
        }
        out.entity_hits.push(BreathEntityHit {
            entity_id: id,
            damage: spec.damage.max(0.0),
            distance: t,
            chained: false,
        });
    }

    // --- Chain arcs ----------------------------------------------------
    let chain_radius = if spec.chain_radius.is_finite() {
        spec.chain_radius.max(0.0)
    } else {
        0.0
    };
    let falloff = if spec.chain_damage_falloff.is_finite() {
        spec.chain_damage_falloff.clamp(0.0, 1.0)
    } else {
        0.0
    };
    let mut chained = 0usize;
    let mut depth = 0u32;
    while chain_radius > 0.0 && chained < spec.chain_max_targets {
        depth += 1;
        let mut round: Vec<(u64, f64)> = Vec::new(); // (candidate_id, best_dist)
        for e in entities {
            if !e.conductive || hit_ids.contains(&e.entity_id) {
                continue;
            }
            let best = hit_pos
                .iter()
                .filter(|(id, _)| hit_depth.get(*id) == Some(&(depth - 1)))
                .map(|(_, p)| {
                    let dx = e.center[0] - p[0];
                    let dy = e.center[1] - p[1];
                    let dz = e.center[2] - p[2];
                    dx * dx + dy * dy + dz * dz
                })
                .filter(|d_sq| *d_sq <= (chain_radius as f64) * (chain_radius as f64))
                .fold(None, |acc: Option<f64>, d_sq| {
                    Some(acc.map_or(d_sq, |a| a.min(d_sq)))
                });
            if let Some(d_sq) = best {
                round.push((e.entity_id, d_sq));
            }
        }
        if round.is_empty() {
            break;
        }
        round.sort_by(|a, b| {
            a.1.partial_cmp(&b.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.0.cmp(&b.0))
        });
        for (id, d_sq) in round {
            if chained >= spec.chain_max_targets {
                break;
            }
            if !hit_ids.insert(id) {
                continue;
            }
            chained += 1;
            hit_depth.insert(id, depth);
            if let Some(e) = entities.iter().find(|e| e.entity_id == id) {
                hit_pos.insert(id, e.center);
            }
            out.entity_hits.push(BreathEntityHit {
                entity_id: id,
                damage: spec.damage.max(0.0) * falloff.powi(depth as i32),
                distance: d_sq.sqrt(),
                chained: true,
            });
        }
    }

    out
}

// ===========================================================================
// 5. Multi-block forges
// ===========================================================================

/// What a pattern cell expects at its world position.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ForgeCellExpect {
    /// An exact block id.
    Block(String),
    /// Any block carrying this tag (`"namespace:tag"`, no `#` needed).
    Tag(String),
    /// An empty cell — accepts `None` (air/unloaded probe) or a vanilla
    /// air id.
    Air,
}

/// One cell of a forge pattern, positioned relative to the core at
/// `(0,0,0)`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ForgePatternCell {
    /// Offset from `core_pos`.
    pub offset: [i32; 3],
    /// What the world must contain here.
    pub expect: ForgeCellExpect,
}

/// Build the classic `3x3x3` hollow-core forge pattern: `shell` for the
/// 25 casing cells and `aperture` for the single cell at
/// `aperture_offset` (e.g. `[0, 0, 1]` — a face center where breath and
/// items enter). The center `(0,0,0)` is the core block itself and is
/// never a pattern cell.
pub fn forge_shell_3x3(
    shell: ForgeCellExpect,
    aperture: ForgeCellExpect,
    aperture_offset: [i32; 3],
) -> Vec<ForgePatternCell> {
    let mut cells = Vec::with_capacity(26);
    for dx in -1..=1 {
        for dy in -1..=1 {
            for dz in -1..=1 {
                if dx == 0 && dy == 0 && dz == 0 {
                    continue;
                }
                let offset = [dx, dy, dz];
                let expect = if offset == aperture_offset {
                    aperture.clone()
                } else {
                    shell.clone()
                };
                cells.push(ForgePatternCell { offset, expect });
            }
        }
    }
    cells
}

/// 3D pattern validator: every cell's `block_at(core_pos + offset)` must
/// satisfy its [`ForgeCellExpect`]. Returns `false` when the pattern is
/// empty, exceeds [`MAX_FORGE_PATTERN_CELLS`], or any cell mismatches —
/// one missing brick rejects the whole structure.
pub fn validate_structure(
    core_pos: [i32; 3],
    pattern: &[ForgePatternCell],
    block_at: &mut dyn FnMut([i32; 3]) -> Option<BlockProbe>,
) -> bool {
    if pattern.is_empty() || pattern.len() > MAX_FORGE_PATTERN_CELLS {
        return false;
    }
    pattern.iter().all(|cell| {
        let pos = [
            core_pos[0].wrapping_add(cell.offset[0]),
            core_pos[1].wrapping_add(cell.offset[1]),
            core_pos[2].wrapping_add(cell.offset[2]),
        ];
        match (&cell.expect, block_at(pos)) {
            (ForgeCellExpect::Block(id), Some(p)) => p.id == *id,
            (ForgeCellExpect::Tag(tag), Some(p)) => p.tags.contains(tag),
            (ForgeCellExpect::Air, None) => true,
            (ForgeCellExpect::Air, Some(p)) => matches!(
                p.id.as_str(),
                "minecraft:air" | "minecraft:cave_air" | "minecraft:void_air"
            ),
            _ => false,
        }
    })
}

/// One alloy recipe: `(base_ingot, catalyst_item, element_kind,
/// heat_cost, smelt_ticks) -> (output_item, output_count)`. One of each
/// input is consumed when the smelt starts; the output materializes when
/// `smelt_ticks` of progress complete.
#[derive(Clone, Debug, PartialEq)]
pub struct AlloyRecipe {
    /// Item id of the base ingot consumed.
    pub base_ingot: String,
    /// Item id of the catalyst consumed.
    pub catalyst_item: String,
    /// Breath element the forge must be charged with.
    pub element: ElementKind,
    /// Heat subtracted from the reservoir when the smelt starts.
    pub heat_cost: f32,
    /// Ticks of progress the smelt takes once started.
    pub smelt_ticks: u32,
    /// Produced item id.
    pub output_item: String,
    /// Produced item count.
    pub output_count: u32,
}

/// Declarative forge profile: the structural pattern, which breath
/// elements charge it, the heat reservoir bounds, and the alloy table.
#[derive(Clone, Debug, PartialEq)]
pub struct MultiBlockForgeSpec {
    /// Pattern cells relative to `core_pos` — the core block at `(0,0,0)`
    /// is never itself a cell.
    pub cells: Vec<ForgePatternCell>,
    /// Breath elements [`MultiBlockForgeRuntime::apply_breath`] accepts.
    pub heat_elements: Vec<ElementKind>,
    /// Heat reservoir ceiling.
    pub heat_capacity: f32,
    /// Heat gained per breath tick.
    pub heat_per_breath_tick: f32,
    /// Heat lost per tick while no smelt is running (also while the
    /// structure is invalid).
    pub idle_heat_decay_per_tick: f32,
    /// Alloy recipes, matched in declaration order.
    pub recipes: Vec<AlloyRecipe>,
}

/// A smelt currently in progress — inputs are already consumed and can
/// never be lost, only delayed.
#[derive(Clone, Debug, PartialEq)]
pub struct ActiveSmelt {
    /// Index into `spec.recipes`.
    pub recipe_index: usize,
    /// Ticks of progress so far.
    pub progress_ticks: u32,
    /// Ticks required (copied from the recipe at start).
    pub total_ticks: u32,
    /// Output produced on completion.
    pub output_item: String,
    /// Output count produced on completion.
    pub output_count: u32,
}

/// Events from one [`MultiBlockForgeRuntime::tick`].
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ForgeTickEvents {
    /// A recipe started this tick (`spec.recipes` index).
    pub smelt_started: Option<usize>,
    /// A smelt completed this tick (`spec.recipes` index).
    pub smelt_completed: Option<usize>,
    /// The structure is currently invalid — smelting pauses.
    pub structure_invalid: bool,
}

/// Mutable runtime of one placed forge: cached structure verdict, heat
/// reservoir, buffered item input/output, and at most one in-flight
/// smelt.
///
/// The input/output buffers hold [`CapabilityItemStack`]s — the runtime
/// implements [`ItemTransferHandler`], so a forge can be registered
/// directly onto [`crate::capability_bus::BlockCapabilityBus`] as an item
/// endpoint: recipe inputs flow in via `insert_item`, completed alloys
/// flow out via `extract_item`, and `as_any_mut` recovers the runtime
/// for ticking. Until any spec-taking call (`revalidate`,
/// `apply_breath`, `tick`) binds the recipe table, `insert_item`
/// refuses everything — fail-closed.
#[derive(Clone, Debug)]
pub struct MultiBlockForgeRuntime {
    /// World position of the core block.
    pub core_pos: [i32; 3],
    /// Cached result of [`Self::revalidate`].
    pub structure_valid: bool,
    /// Current reservoir heat `0..=spec.heat_capacity`.
    pub heat: f32,
    /// Breath element the reservoir was last charged with — recipes key
    /// on it, so a fire-charged forge cannot run an ice recipe.
    pub charged_element: Option<ElementKind>,
    /// Buffered input stacks (merged per `can_merge` — item id **and**
    /// component bytes identical).
    pub input: Vec<CapabilityItemStack>,
    /// Completed output stacks awaiting pickup.
    pub output: Vec<CapabilityItemStack>,
    /// In-flight smelt, if any.
    pub active_smelt: Option<ActiveSmelt>,
    /// Item ids any bound recipe accepts (`base_ingot`/`catalyst_item`)
    /// — refreshed by [`Self::note_spec`] on every spec-taking call.
    accepted_inputs: BTreeSet<String>,
}

impl MultiBlockForgeRuntime {
    /// A fresh, unvalidated forge at `core_pos`.
    pub fn new(core_pos: [i32; 3]) -> Self {
        Self {
            core_pos,
            structure_valid: false,
            heat: 0.0,
            charged_element: None,
            input: Vec::new(),
            output: Vec::new(),
            active_smelt: None,
            accepted_inputs: BTreeSet::new(),
        }
    }

    /// Rebind the recipe table: refresh `accepted_inputs` so bus-side
    /// `insert_item` knows which items this forge swallows.
    fn note_spec(&mut self, spec: &MultiBlockForgeSpec) {
        self.accepted_inputs = spec
            .recipes
            .iter()
            .flat_map(|r| [r.base_ingot.clone(), r.catalyst_item.clone()])
            .collect();
    }

    /// A [`BlockCoord`] view of `core_pos` for bus registration.
    pub fn coord(&self, dim_id: &str) -> BlockCoord {
        BlockCoord::new(dim_id, self.core_pos[0], self.core_pos[1], self.core_pos[2])
    }

    /// Re-run [`validate_structure`] and cache the verdict.
    pub fn revalidate(
        &mut self,
        spec: &MultiBlockForgeSpec,
        block_at: &mut dyn FnMut([i32; 3]) -> Option<BlockProbe>,
    ) -> bool {
        self.note_spec(spec);
        self.structure_valid = validate_structure(self.core_pos, &spec.cells, block_at);
        self.structure_valid
    }

    /// Merge `stack` into the input buffer (id + components must match
    /// to merge, per [`CapabilityItemStack::can_merge`]). Empty stacks
    /// are ignored; counts saturate at `u32::MAX`.
    pub fn insert_input(&mut self, stack: &CapabilityItemStack) {
        if stack.is_empty() {
            return;
        }
        if let Some(slot) = self.input.iter_mut().find(|s| s.can_merge(stack)) {
            slot.count = slot.count.saturating_add(stack.count);
        } else {
            self.input.push(stack.clone());
        }
    }

    /// Items of `item` currently buffered in the input.
    pub fn input_count(&self, item: &str) -> u32 {
        self.input
            .iter()
            .filter(|s| s.item_id == item)
            .map(|s| s.count)
            .sum()
    }

    /// Items of `item` waiting in the output.
    pub fn output_count(&self, item: &str) -> u32 {
        self.output
            .iter()
            .filter(|s| s.item_id == item)
            .map(|s| s.count)
            .sum()
    }

    /// Apply `ticks` of `element` breath: charges the reservoir when the
    /// structure is valid and `spec.heat_elements` accepts the element.
    /// Returns whether any heat was accepted.
    pub fn apply_breath(&mut self, spec: &MultiBlockForgeSpec, element: ElementKind, ticks: u32) -> bool {
        self.note_spec(spec);
        if !self.structure_valid || ticks == 0 || !spec.heat_elements.contains(&element) {
            return false;
        }
        let gain = spec.heat_per_breath_tick.max(0.0) * ticks as f32;
        if !gain.is_finite() || gain <= 0.0 {
            return false;
        }
        self.heat = (self.heat + gain).min(spec.heat_capacity.max(0.0));
        self.charged_element = Some(element);
        true
    }

    /// Merge a completed product stack into the output buffer.
    fn insert_output(&mut self, stack: CapabilityItemStack) {
        if stack.is_empty() {
            return;
        }
        if let Some(slot) = self.output.iter_mut().find(|s| s.can_merge(&stack)) {
            slot.count = slot.count.saturating_add(stack.count);
        } else {
            self.output.push(stack);
        }
    }

    /// Consume one `item` from the input buffer.
    fn take_input(&mut self, item: &str) -> bool {
        if let Some(pos) = self.input.iter().position(|s| s.item_id == item && s.count > 0) {
            self.input[pos].count -= 1;
            if self.input[pos].count == 0 {
                self.input.remove(pos);
            }
            true
        } else {
            false
        }
    }

    /// Advance the forge one tick.
    ///
    /// * An invalid structure pauses (never voids) the in-flight smelt
    ///   and decays heat.
    /// * A running smelt gains one progress tick; on `total_ticks` it
    ///   pushes `output_item x output_count` into `output` — lossless:
    ///   consumed inputs always materialize as output.
    /// * With no active smelt the recipe table is scanned in order: the
    ///   first recipe whose `(base_ingot, catalyst_item,
    ///   charged_element)` inputs are present **and** whose `heat_cost`
    ///   the reservoir covers consumes one of each input plus the heat
    ///   and starts.
    /// * Idle ticks (`active_smelt` still `None` after scanning) decay
    ///   `idle_heat_decay_per_tick` of heat.
    pub fn tick(&mut self, spec: &MultiBlockForgeSpec) -> ForgeTickEvents {
        self.note_spec(spec);
        let mut ev = ForgeTickEvents {
            structure_invalid: !self.structure_valid,
            ..ForgeTickEvents::default()
        };

        if self.structure_valid {
            let completed = if let Some(smelt) = &mut self.active_smelt {
                smelt.progress_ticks = smelt.progress_ticks.saturating_add(1);
                smelt.progress_ticks >= smelt.total_ticks
            } else {
                false
            };
            if completed {
                if let Some(smelt) = self.active_smelt.take() {
                    self.insert_output(CapabilityItemStack::new(
                        smelt.output_item,
                        smelt.output_count,
                    ));
                    ev.smelt_completed = Some(smelt.recipe_index);
                }
            }
            if self.active_smelt.is_none() {
                for (i, recipe) in spec.recipes.iter().enumerate() {
                    let base_needed = if recipe.base_ingot == recipe.catalyst_item {
                        2
                    } else {
                        1
                    };
                    let have_base = self.input_count(&recipe.base_ingot) >= base_needed;
                    let have_cat = self.input_count(&recipe.catalyst_item) >= 1;
                    let heat_cost = if recipe.heat_cost.is_finite() {
                        recipe.heat_cost.max(0.0)
                    } else {
                        f32::MAX
                    };
                    if have_base
                        && have_cat
                        && self.charged_element == Some(recipe.element)
                        && self.heat >= heat_cost
                    {
                        self.take_input(&recipe.base_ingot);
                        self.take_input(&recipe.catalyst_item);
                        self.heat = (self.heat - heat_cost).max(0.0);
                        self.active_smelt = Some(ActiveSmelt {
                            recipe_index: i,
                            progress_ticks: 0,
                            total_ticks: recipe.smelt_ticks.max(1),
                            output_item: recipe.output_item.clone(),
                            output_count: recipe.output_count,
                        });
                        ev.smelt_started = Some(i);
                        break;
                    }
                }
            }
        }

        if self.active_smelt.is_none() {
            self.heat = (self.heat - spec.idle_heat_decay_per_tick.max(0.0)).max(0.0);
        }
        ev
    }
}

/// Forge as a [`crate::capability_bus::BlockCapabilityBus`] item
/// endpoint: every face accepts recipe-input items (unbounded buffer —
/// the smelt loop consumes them), every face yields completed products.
/// `insert_item` is all-or-nothing per stack identity: non-recipe items
/// return as a full remainder so `transfer_item_between` rolls them
/// back instead of stranding junk in the forge.
impl ItemTransferHandler for MultiBlockForgeRuntime {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }

    fn insert_item(
        &mut self,
        _face: Option<FaceDirection>,
        stack: &CapabilityItemStack,
        simulate: bool,
    ) -> CapabilityItemStack {
        if stack.is_empty() || !self.accepted_inputs.contains(&stack.item_id) {
            return stack.clone();
        }
        if !simulate {
            self.insert_input(stack);
        }
        // Unbounded input buffer: nothing is refused once the id matches.
        let mut remainder = stack.clone();
        remainder.count = 0;
        remainder
    }

    fn extract_item(
        &mut self,
        _face: Option<FaceDirection>,
        max_count: u32,
        filter: Option<&dyn Fn(&CapabilityItemStack) -> bool>,
        simulate: bool,
    ) -> Option<CapabilityItemStack> {
        if max_count == 0 {
            return None;
        }
        let index = self.output.iter().position(|s| {
            !s.is_empty() && filter.is_none_or(|f| f(s))
        })?;
        if simulate {
            let mut probe = self.output[index].clone();
            Some(probe.take(max_count))
        } else {
            let stack = &mut self.output[index];
            let out = stack.take(max_count);
            if stack.count == 0 {
                self.output.remove(index);
            }
            Some(out)
        }
    }

    fn slots(&self) -> usize {
        self.input.len() + self.output.len()
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(stage_scale: f32) -> GrowthStageSpec {
        GrowthStageSpec {
            min_age_ticks: 0,
            hitbox_scale: stage_scale,
            max_health_multiplier: 1.0,
            melee_damage_multiplier: 1.0,
            breath_range_blocks: 10.0,
        }
    }

    // ---- Growth stages -------------------------------------------------

    #[test]
    fn stage_for_age_ticks_picks_last_anchor_below_age() {
        let curve = GrowthStageCurve::new(
            vec![
                spec(0.35),
                GrowthStageSpec {
                    min_age_ticks: 100,
                    ..spec(0.7)
                },
                GrowthStageSpec {
                    min_age_ticks: 200,
                    ..spec(1.0)
                },
            ],
            2,
        );
        assert_eq!(curve.stage_for_age_ticks(0), 1);
        assert_eq!(curve.stage_for_age_ticks(99), 1);
        assert_eq!(curve.stage_for_age_ticks(100), 2);
        assert_eq!(curve.stage_for_age_ticks(199), 2);
        assert_eq!(curve.stage_for_age_ticks(200), 3);
        assert_eq!(curve.stage_for_age_ticks(u64::MAX), 3);
    }

    #[test]
    fn curve_sorts_anchors_and_truncates_to_five() {
        let curve = GrowthStageCurve::new(
            vec![
                GrowthStageSpec {
                    min_age_ticks: 400,
                    ..spec(1.0)
                },
                spec(0.35),
                GrowthStageSpec {
                    min_age_ticks: 50,
                    ..spec(0.5)
                },
                GrowthStageSpec {
                    min_age_ticks: 600,
                    ..spec(1.5)
                },
                GrowthStageSpec {
                    min_age_ticks: 900,
                    ..spec(2.0)
                },
                GrowthStageSpec {
                    min_age_ticks: 1200,
                    ..spec(2.2)
                },
                GrowthStageSpec {
                    min_age_ticks: 1500,
                    ..spec(9.0)
                },
            ],
            3,
        );
        assert_eq!(curve.stages.len(), 5);
        assert_eq!(curve.stage_for_age_ticks(1500), 5); // truncated tail unreachable
        assert_eq!(curve.stage_for_age_ticks(1200), 5);
        assert_eq!(curve.stage_for_age_ticks(0), 1);
        assert_eq!(curve.stage_for_age_ticks(50), 2);
    }

    #[test]
    fn empty_curve_falls_back_to_single_stage_one_anchor() {
        let curve = GrowthStageCurve::new(Vec::new(), 9);
        assert_eq!(curve.stages.len(), 1);
        assert_eq!(curve.flight_unlock_stage, MAX_GROWTH_STAGE);
        assert_eq!(curve.stage_for_age_ticks(0), 1);
        assert!((curve.hitbox_scale_at(0) - 1.0).abs() < f32::EPSILON);
    }

    #[test]
    fn hitbox_scale_clamps_into_bounds() {
        let curve = GrowthStageCurve::new(
            vec![
                spec(0.01),
                GrowthStageSpec {
                    min_age_ticks: 100,
                    ..spec(99.0)
                },
            ],
            3,
        );
        assert!((curve.hitbox_scale_at(0) - MIN_STAGE_HITBOX_SCALE).abs() < f32::EPSILON);
        assert!((curve.hitbox_scale_at(100) - MAX_STAGE_HITBOX_SCALE).abs() < f32::EPSILON);
    }

    #[test]
    fn curve_lerps_between_anchors() {
        let curve = GrowthStageCurve::new(
            vec![
                spec(1.0),
                GrowthStageSpec {
                    min_age_ticks: 100,
                    ..spec(2.0)
                },
            ],
            3,
        );
        assert!((curve.hitbox_scale_at(50) - 1.5).abs() < 0.001);
        // Stage identity is still discrete — lerp changes scale, not stage.
        assert_eq!(curve.stage_for_age_ticks(50), 1);
        assert!((curve.breath_range_at(50) - 10.0).abs() < f32::EPSILON);
    }

    #[test]
    fn flight_unlock_honors_stage_threshold() {
        let curve = GrowthStageCurve::new(
            vec![
                spec(0.35),
                GrowthStageSpec {
                    min_age_ticks: 100,
                    ..spec(1.0)
                },
                GrowthStageSpec {
                    min_age_ticks: 200,
                    ..spec(1.5)
                },
            ],
            3,
        );
        assert!(!curve.flight_unlocked_at(199));
        assert!(curve.flight_unlocked_at(200));
        let default_curve = GrowthStageCurve::default();
        assert_eq!(default_curve.flight_unlock_stage, 3);
        assert_eq!(default_curve.stages.len(), 5);
        assert!(default_curve.flight_unlocked_at(336_000));
        assert!(!default_curve.flight_unlocked_at(0));
    }

    // ---- Multi-part hitboxes --------------------------------------------

    fn dragon_parts() -> Vec<PartOffsetSpec> {
        vec![
            PartOffsetSpec {
                kind: PartKind::Head,
                local_offset: [0.0, 1.0, 3.0],
                half_extents: [0.5, 0.5, 0.6],
                damage_multiplier: None,
            },
            PartOffsetSpec {
                kind: PartKind::Neck,
                local_offset: [0.0, 0.8, 1.8],
                half_extents: [0.4, 0.4, 0.8],
                damage_multiplier: None,
            },
            PartOffsetSpec {
                kind: PartKind::Torso,
                local_offset: [0.0, 0.5, 0.0],
                half_extents: [0.9, 0.7, 1.4],
                damage_multiplier: None,
            },
            PartOffsetSpec {
                kind: PartKind::LeftWing,
                local_offset: [1.4, 1.0, 0.0],
                half_extents: [1.0, 0.2, 0.8],
                damage_multiplier: None,
            },
            PartOffsetSpec {
                kind: PartKind::RightWing,
                local_offset: [-1.4, 1.0, 0.0],
                half_extents: [1.0, 0.2, 0.8],
                damage_multiplier: None,
            },
            PartOffsetSpec {
                kind: PartKind::Tail(0),
                local_offset: [0.0, 0.4, -2.5],
                half_extents: [0.4, 0.4, 1.2],
                damage_multiplier: None,
            },
        ]
    }

    #[test]
    fn multipart_hitboxes_scale_with_stage() {
        let parts = dragon_parts();
        let curve = GrowthStageCurve::default();
        for (stage, expected_scale) in [(1u8, 0.35f64), (3, 1.0), (5, 2.2)] {
            let scale =
                curve.hitbox_scale_at(curve.stages[usize::from(stage) - 1].min_age_ticks) as f64;
            assert!((scale - expected_scale).abs() < 0.01, "stage {stage}");
            let aabbs =
                evaluate_multipart_hitboxes([0.0, 64.0, 0.0], 0.0, 0.0, scale as f32, &parts);
            let head = &aabbs[0];
            // Head center: origin + offset * scale along +Z (yaw 0 faces +Z).
            assert!((head.min[2] - (3.0 * scale - 0.6 * scale)).abs() < 1e-9);
            assert!((head.max[2] - (3.0 * scale + 0.6 * scale)).abs() < 1e-9);
            assert!((head.min[1] - (64.0 + 1.0 * scale - 0.5 * scale)).abs() < 1e-9);
            assert!((head.damage_multiplier - 1.25).abs() < f32::EPSILON);
            let torso = &aabbs[2];
            assert!((torso.damage_multiplier - 0.9).abs() < f32::EPSILON);
            let wing = &aabbs[3];
            assert!((wing.damage_multiplier - 1.0).abs() < f32::EPSILON);
        }
    }

    #[test]
    fn yaw_rotation_swings_parts_around_origin() {
        let parts = dragon_parts();
        // yaw = +90deg (pi/2): vanilla forward becomes -X (west).
        let aabbs = evaluate_multipart_hitboxes(
            [0.0, 64.0, 0.0],
            std::f32::consts::FRAC_PI_2,
            0.0,
            1.0,
            &parts,
        );
        let head = &aabbs[0];
        // Head local (0,1,3) -> world ~(-3, +1, 0). f32 cos(pi/2) is not
        // exactly 0, so compare with a 1e-6 tolerance.
        assert!((head.min[0] - (-3.0 - 0.5)).abs() < 1e-6);
        assert!((head.max[0] - (-3.0 + 0.5)).abs() < 1e-6);
        assert!((head.min[2] - (-0.6)).abs() < 1e-6);
        assert!((head.max[2] - 0.6).abs() < 1e-6);
        // Left wing (+x local, creature's left) points +Z (south) at yaw 90.
        let lwing = &aabbs[3];
        assert!((lwing.min[2] - (1.4 - 0.8)).abs() < 1e-6);
        assert!((lwing.max[2] - (1.4 + 0.8)).abs() < 1e-6);
    }

    #[test]
    fn pitch_noses_parts_down() {
        let parts = dragon_parts();
        let aabbs = evaluate_multipart_hitboxes(
            [0.0, 64.0, 0.0],
            0.0,
            std::f32::consts::FRAC_PI_2,
            1.0,
            &parts,
        );
        // Full nose-down: the head's forward offset becomes downward.
        let head = &aabbs[0];
        assert!(head.max[1] < 64.0 + 1.0 - 2.0, "head swung below origin");
    }

    #[test]
    fn hit_test_distinguishes_head_torso_tail() {
        let parts = dragon_parts();
        let aabbs = evaluate_multipart_hitboxes([0.0, 64.0, 0.0], 0.0, 0.0, 1.0, &parts);

        // Ray along +Z through the head center — at y=65.4 it clears the
        // torso/neck boxes (tops at 65.2) so the head is the first hit.
        let hit = hit_test_parts([0.0, 65.4, -10.0], [0.0, 0.0, 1.0], 50.0, &aabbs)
            .expect("head hit");
        assert_eq!(hit.kind, PartKind::Head);
        assert!((hit.damage_multiplier - 1.25).abs() < f32::EPSILON);

        // Ray along +Z through the torso center — torso, then head is
        // behind it along this axis but further away... torso is at z=0,
        // head at z=3: the first box along +Z from -10 is the tail (-2.5).
        let hit = hit_test_parts([0.0, 64.5, -10.0], [0.0, 0.0, 1.0], 50.0, &aabbs)
            .expect("tail/torso hit");
        assert_eq!(hit.kind, PartKind::Tail(0));

        // Straight down onto the torso from above — the only part whose
        // AABB contains the x=z=0 column.
        let hit = hit_test_parts([0.0, 70.0, 0.0], [0.0, -1.0, 0.0], 50.0, &aabbs)
            .expect("torso hit");
        assert_eq!(hit.kind, PartKind::Torso);
        assert!(hit.t > 0.0 && hit.t <= 50.0);

        // A miss returns None.
        assert!(hit_test_parts([50.0, 64.0, 0.0], [0.0, 0.0, 1.0], 50.0, &aabbs).is_none());
        // Degenerate rays fail closed.
        assert!(hit_test_parts([0.0; 3], [0.0; 3], 10.0, &aabbs).is_none());
        assert!(
            hit_test_parts([0.0; 3], [f64::NAN, 0.0, 1.0], 10.0, &aabbs).is_none()
        );
        assert!(hit_test_parts([0.0; 3], [0.0, 0.0, 1.0], 0.0, &aabbs).is_none());
    }

    #[test]
    fn hit_test_returns_nearest_part() {
        // Two stacked boxes along +Z; the nearer must win.
        let parts = vec![
            PartOffsetSpec {
                kind: PartKind::Head,
                local_offset: [0.0, 0.0, 5.0],
                half_extents: [0.5, 0.5, 0.5],
                damage_multiplier: None,
            },
            PartOffsetSpec {
                kind: PartKind::Torso,
                local_offset: [0.0, 0.0, 1.5],
                half_extents: [0.5, 0.5, 0.5],
                damage_multiplier: None,
            },
        ];
        let aabbs = evaluate_multipart_hitboxes([0.0; 3], 0.0, 0.0, 1.0, &parts);
        let hit = hit_test_parts([0.0, 0.0, -5.0], [0.0, 0.0, 1.0], 20.0, &aabbs).unwrap();
        assert_eq!(hit.kind, PartKind::Torso);
        assert!((hit.point[2] - 1.0).abs() < 1e-9);
    }

    // ---- Mounted flight --------------------------------------------------

    fn flyer_spec() -> MountableFlyerSpec {
        MountableFlyerSpec {
            max_stamina: 50.0,
            ..MountableFlyerSpec::default()
        }
    }

    fn grounded_state() -> MountableFlyerState {
        MountableFlyerState::new_grounded([0.0, 64.0, 0.0], 0.0, 50.0)
    }

    #[test]
    fn takeoff_refused_below_flight_unlock_stage() {
        let spec = flyer_spec();
        let mut st = grounded_state();
        for stage in [1u8, 2] {
            let ev = step_mountable_flyer(
                &mut st,
                &spec,
                &MountableFlyerInput {
                    growth_stage: stage,
                    ascend: true,
                    on_ground: true,
                    ..MountableFlyerInput::default()
                },
            );
            assert!(!ev.took_off);
            assert!(ev.takeoff_refused_low_stage);
            assert!(!st.airborne);
        }
    }

    #[test]
    fn takeoff_cruise_dive_and_land() {
        let spec = flyer_spec();
        let mut st = grounded_state();
        // Takeoff.
        let ev = step_mountable_flyer(
            &mut st,
            &spec,
            &MountableFlyerInput {
                growth_stage: 4,
                ascend: true,
                on_ground: true,
                ..MountableFlyerInput::default()
            },
        );
        assert!(ev.took_off);
        assert!(st.airborne);
        assert!(st.vel[1] > 0.0);
        assert!(st.pos[1] > 64.0);

        // Cruise forward — look direction drives velocity.
        let ev = step_mountable_flyer(
            &mut st,
            &spec,
            &MountableFlyerInput {
                growth_stage: 4,
                forward: 1.0,
                on_ground: false,
                ..MountableFlyerInput::default()
            },
        );
        assert!(!ev.took_off);
        assert!((st.vel[2] - spec.fly_speed).abs() < 1e-9);
        assert!(st.stamina < spec.max_stamina);

        // Dive: pitch down + boost exceeds cruise speed. Pitch advances
        // at pitch_rate_rad per tick, so warm it up past dive_min_pitch.
        let mut st2 = st;
        for _ in 0..4 {
            step_mountable_flyer(
                &mut st2,
                &spec,
                &MountableFlyerInput {
                    growth_stage: 4,
                    pitch_target_rad: 1.0,
                    ..MountableFlyerInput::default()
                },
            );
        }
        assert!(st2.pitch_rad >= spec.dive_min_pitch_rad);
        let ev = step_mountable_flyer(
            &mut st2,
            &spec,
            &MountableFlyerInput {
                growth_stage: 4,
                forward: 1.0,
                dive_boost: true,
                ..MountableFlyerInput::default()
            },
        );
        let speed = (st2.vel[0] * st2.vel[0] + st2.vel[1] * st2.vel[1] + st2.vel[2] * st2.vel[2]).sqrt();
        assert!(speed > spec.fly_speed * 1.4, "dive boost applied ({speed})");
        assert!(st2.vel[1] < 0.0, "diving descends");
        assert!(!ev.landed);

        // Smooth landing on ground contact.
        let ev = step_mountable_flyer(
            &mut st,
            &spec,
            &MountableFlyerInput {
                growth_stage: 4,
                forward: 0.0,
                on_ground: true,
                ..MountableFlyerInput::default()
            },
        );
        assert!(ev.landed);
        assert!(!ev.hard_landing);
        assert!(!st.airborne);
        assert_eq!(st.vel, [0.0; 3]);
    }

    #[test]
    fn stamina_exhaustion_forces_descent_then_regen_on_ground() {
        let spec = MountableFlyerSpec {
            max_stamina: 2.0,
            stamina_drain_per_tick: 1.0,
            ..flyer_spec()
        };
        let mut st = MountableFlyerState::new_grounded([0.0, 64.0, 0.0], 0.0, 2.0);
        step_mountable_flyer(
            &mut st,
            &spec,
            &MountableFlyerInput {
                growth_stage: 5,
                ascend: true,
                on_ground: true,
                ..MountableFlyerInput::default()
            },
        );
        let mut emptied = false;
        for _ in 0..10 {
            let ev = step_mountable_flyer(
                &mut st,
                &spec,
                &MountableFlyerInput {
                    growth_stage: 5,
                    ascend: true,
                    ..MountableFlyerInput::default()
                },
            );
            emptied |= ev.stamina_empty;
            if st.stamina <= 0.0 {
                break;
            }
        }
        assert!(emptied);
        // Empty reservoir: ascend no longer lifts — forced sink wins.
        let ev = step_mountable_flyer(
            &mut st,
            &spec,
            &MountableFlyerInput {
                growth_stage: 5,
                ascend: true,
                ..MountableFlyerInput::default()
            },
        );
        assert!(st.vel[1] <= -spec.fatigue_sink_speed);
        assert!(!ev.took_off);
        // Regen on the ground.
        st.airborne = false;
        step_mountable_flyer(
            &mut st,
            &spec,
            &MountableFlyerInput {
                growth_stage: 5,
                on_ground: true,
                ..MountableFlyerInput::default()
            },
        );
        assert!(st.stamina > 0.0);
    }

    #[test]
    fn flyer_nan_and_extreme_speed_fail_closed() {
        let spec = flyer_spec();
        let mut st = grounded_state();
        st.vel = [f64::NAN, 1e9, f64::INFINITY];
        st.pos[0] = f64::NAN;
        let ev = step_mountable_flyer(
            &mut st,
            &spec,
            &MountableFlyerInput {
                growth_stage: 5,
                forward: f32::NAN,
                yaw_delta_rad: f32::INFINITY,
                on_ground: true,
                ..MountableFlyerInput::default()
            },
        );
        assert!(ev.sanitized);
        assert!(st.pos.iter().all(|v| v.is_finite()));
        assert!(st.vel.iter().all(|v| v.is_finite()));

        // Extreme computed speed clamps to max_speed (a misconfigured
        // spec asking for fly_speed > max_speed).
        let fast = MountableFlyerSpec {
            fly_speed: 100.0,
            ..spec
        };
        let mut st = MountableFlyerState {
            airborne: true,
            ..grounded_state()
        };
        let ev = step_mountable_flyer(
            &mut st,
            &fast,
            &MountableFlyerInput {
                growth_stage: 5,
                forward: 1.0,
                ..MountableFlyerInput::default()
            },
        );
        let sp = (st.vel[0] * st.vel[0] + st.vel[1] * st.vel[1] + st.vel[2] * st.vel[2]).sqrt();
        assert!(sp <= fast.max_speed + 1e-9);
        assert!(ev.sanitized);
    }

    #[test]
    fn mid_fall_ascend_takes_off() {
        let spec = flyer_spec();
        let mut st = grounded_state();
        // Walked off an edge: grounded -> falling (not airborne yet).
        let ev = step_mountable_flyer(
            &mut st,
            &spec,
            &MountableFlyerInput {
                growth_stage: 3,
                on_ground: false,
                ..MountableFlyerInput::default()
            },
        );
        assert!(!ev.took_off && !st.airborne);
        assert!(st.vel[1] < 0.0, "unpowered fall");
        // Jump mid-fall engages flight.
        let ev = step_mountable_flyer(
            &mut st,
            &spec,
            &MountableFlyerInput {
                growth_stage: 3,
                ascend: true,
                on_ground: false,
                ..MountableFlyerInput::default()
            },
        );
        assert!(ev.took_off && st.airborne);
    }

    #[test]
    fn breath_flag_respects_ground_and_air_policy() {
        let spec = flyer_spec();
        let mut st = grounded_state();
        step_mountable_flyer(
            &mut st,
            &spec,
            &MountableFlyerInput {
                growth_stage: 5,
                breath_pressed: true,
                on_ground: true,
                ..MountableFlyerInput::default()
            },
        );
        assert!(st.breath_active);
        let grounded_only = MountableFlyerSpec {
            breath_while_flying: false,
            ..spec
        };
        let ev = step_mountable_flyer(
            &mut st,
            &grounded_only,
            &MountableFlyerInput {
                growth_stage: 5,
                ascend: true,
                breath_pressed: true,
                on_ground: true,
                ..MountableFlyerInput::default()
            },
        );
        assert!(ev.took_off);
        assert!(!st.breath_active, "breath policy denied while flying");
    }

    // ---- Elemental breath --------------------------------------------------

    fn probe(id: &str) -> BlockProbe {
        BlockProbe {
            id: id.to_string(),
            tags: BTreeSet::new(),
        }
    }

    #[test]
    fn fire_breath_chars_matching_blocks_and_hits_entities() {
        let spec = ElementalBreathSpec::fire();
        // World: grass at (0,64,3), dirt at (0,64,4), stone at (1,64,4).
        let mut blocks = |pos: [i32; 3]| -> Option<BlockProbe> {
            match pos {
                [0, 64, 3] => Some(probe("minecraft:grass_block")),
                [0, 64, 4] => Some(probe("minecraft:dirt")),
                [1, 64, 4] => Some(probe("minecraft:stone")),
                _ => None,
            }
        };
        let target = BreathEntity {
            entity_id: 7,
            center: [0.5, 64.5, 6.0],
            half_extents: [0.4, 0.9, 0.4],
            conductive: false,
        };
        let out = evaluate_breath_cone(
            [0.5, 64.5, 0.5],
            0.0,
            0.0,
            &spec,
            &[target],
            &mut blocks,
        );
        assert_eq!(out.entity_hits.len(), 1);
        assert_eq!(out.entity_hits[0].entity_id, 7);
        assert!(!out.entity_hits[0].chained);
        assert!(!out.truncated);
        // grass -> dirt, dirt -> charred, stone -> scorched.
        let to: Vec<&str> = out
            .block_transforms
            .iter()
            .map(|t| t.to.as_str())
            .collect();
        assert!(to.contains(&"minecraft:dirt"));
        assert!(to.contains(&"anyforge:charred_dirt"));
        assert!(to.contains(&"anyforge:scorched_stone"));
        assert!(out.block_transforms.len() <= MAX_BREATH_BLOCK_TRANSFORMS_PER_TICK);
    }

    #[test]
    fn ice_breath_freezes_water() {
        let spec = ElementalBreathSpec::ice();
        let mut blocks = |pos: [i32; 3]| -> Option<BlockProbe> {
            match pos {
                [0, 64, 2] | [0, 64, 3] => Some(probe("minecraft:water")),
                _ => None,
            }
        };
        let out = evaluate_breath_cone([0.5, 64.5, 0.5], 0.0, 0.0, &spec, &[], &mut blocks);
        assert_eq!(out.block_transforms.len(), 2);
        assert!(
            out.block_transforms
                .iter()
                .all(|t| t.to == "minecraft:ice")
        );
    }

    #[test]
    fn breath_transforms_respect_the_256_cap() {
        let mut spec = ElementalBreathSpec::fire();
        spec.range_blocks = MAX_BREATH_RANGE_BLOCKS;
        spec.cone_half_angle_rad = 1.0;
        let mut blocks = |_: [i32; 3]| -> Option<BlockProbe> {
            Some(probe("minecraft:stone")) // everything transformable
        };
        let out = evaluate_breath_cone([0.5, 64.5, 0.5], 0.0, 0.0, &spec, &[], &mut blocks);
        assert!(out.block_transforms.len() <= MAX_BREATH_BLOCK_TRANSFORMS_PER_TICK);
        assert!(out.truncated, "a saturated cone must flag truncation");
        // Deterministic ordering: sweep order is stable across calls.
        let mut blocks2 = |_: [i32; 3]| -> Option<BlockProbe> {
            Some(probe("minecraft:stone"))
        };
        let out2 = evaluate_breath_cone([0.5, 64.5, 0.5], 0.0, 0.0, &spec, &[], &mut blocks2);
        assert_eq!(out.block_transforms, out2.block_transforms);
    }

    #[test]
    fn breath_range_is_capped_at_48_blocks() {
        let mut spec = ElementalBreathSpec::fire();
        spec.range_blocks = 10_000.0;
        let mut called = false;
        let mut blocks = |pos: [i32; 3]| -> Option<BlockProbe> {
            called = true;
            assert!(pos[2] <= 49, "sweep stayed inside the cap ({pos:?})");
            Some(probe("minecraft:stone"))
        };
        let out = evaluate_breath_cone([0.5, 64.5, 0.5], 0.0, 0.0, &spec, &[], &mut blocks);
        assert!(called);
        assert!(out.truncated); // saturated — probe/candidate budget hit
    }

    #[test]
    fn lightning_chains_to_conductive_targets() {
        let spec = ElementalBreathSpec::lightning();
        // Mouth at (0.5,64.5,0.5) looking +Z; cone half-angle 0.25 keeps
        // e2..e5 outside the beam while staying within chain range of e1.
        let entities = vec![
            BreathEntity {
                entity_id: 1,
                center: [0.5, 64.5, 8.0], // dead-center — direct cone hit
                half_extents: [0.4, 0.9, 0.4],
                conductive: false,
            },
            BreathEntity {
                entity_id: 2,
                center: [4.0, 64.5, 8.0], // outside cone, 3.5 from e1 — hop 1
                half_extents: [0.4, 0.9, 0.4],
                conductive: true,
            },
            BreathEntity {
                entity_id: 3,
                center: [7.5, 64.5, 8.0], // outside cone, 3.5 from e2 — hop 2
                half_extents: [0.4, 0.9, 0.4],
                conductive: true,
            },
            BreathEntity {
                entity_id: 4,
                center: [30.0, 64.5, 8.0], // far outside chain radius
                half_extents: [0.4, 0.9, 0.4],
                conductive: true,
            },
            BreathEntity {
                entity_id: 5,
                center: [4.5, 64.5, 8.0], // in chain range but not conductive
                half_extents: [0.4, 0.9, 0.4],
                conductive: false,
            },
        ];
        let mut no_blocks = |_: [i32; 3]| -> Option<BlockProbe> { None };
        let out = evaluate_breath_cone(
            [0.5, 64.5, 0.5],
            0.0,
            0.0,
            &spec,
            &entities,
            &mut no_blocks,
        );
        let by_id: BTreeMap<u64, &BreathEntityHit> =
            out.entity_hits.iter().map(|h| (h.entity_id, h)).collect();
        // Direct cone hit on e1.
        assert!(by_id.contains_key(&1));
        assert!(!by_id[&1].chained);
        // e2 chained from e1 (hop 1: damage * 0.8).
        assert!(by_id[&2].chained);
        assert!((by_id[&2].damage - spec.damage * 0.8).abs() < 1e-4);
        // e3 chained from e2 (hop 2: damage * 0.64).
        assert!(by_id[&3].chained);
        assert!((by_id[&3].damage - spec.damage * 0.64).abs() < 1e-3);
        // e4 out of chain radius; e5 not conductive.
        assert!(!by_id.contains_key(&4));
        assert!(!by_id.contains_key(&5));
    }

    #[test]
    fn non_finite_breath_is_fail_closed() {
        let spec = ElementalBreathSpec::fire();
        let mut blocks = |_: [i32; 3]| -> Option<BlockProbe> { Some(probe("minecraft:stone")) };
        let out = evaluate_breath_cone(
            [f64::NAN, 0.0, 0.0],
            0.0,
            0.0,
            &spec,
            &[],
            &mut blocks,
        );
        assert!(out.entity_hits.is_empty() && out.block_transforms.is_empty());
        let mut spec2 = spec.clone();
        spec2.range_blocks = f32::NAN;
        let out = evaluate_breath_cone([0.5, 64.5, 0.5], 0.0, 0.0, &spec2, &[], &mut blocks);
        assert!(out.block_transforms.is_empty());
    }

    // ---- Multi-block forge --------------------------------------------------

    fn forge_spec() -> MultiBlockForgeSpec {
        MultiBlockForgeSpec {
            cells: forge_shell_3x3(
                ForgeCellExpect::Block("anyforge:forge_brick".to_string()),
                ForgeCellExpect::Block("anyforge:aperture".to_string()),
                [0, 0, 1],
            ),
            heat_elements: vec![ElementKind::Fire],
            heat_capacity: 100.0,
            heat_per_breath_tick: 5.0,
            idle_heat_decay_per_tick: 1.0,
            recipes: vec![AlloyRecipe {
                base_ingot: "anyforge:iron_ingot".to_string(),
                catalyst_item: "anyforge:fire_catalyst".to_string(),
                element: ElementKind::Fire,
                heat_cost: 30.0,
                smelt_ticks: 5,
                output_item: "anyforge:dragonsteel_ingot".to_string(),
                output_count: 1,
            }],
        }
    }

    fn full_shell(core: [i32; 3]) -> impl FnMut([i32; 3]) -> Option<BlockProbe> {
        move |pos: [i32; 3]| {
            let d = [
                pos[0] - core[0],
                pos[1] - core[1],
                pos[2] - core[2],
            ];
            let surface = d.iter().all(|c| (-1..=1).contains(c)) && d != [0, 0, 0];
            if !surface {
                return None;
            }
            if d == [0, 0, 1] {
                Some(probe("anyforge:aperture"))
            } else {
                Some(probe("anyforge:forge_brick"))
            }
        }
    }

    #[test]
    fn forge_structure_validates_and_rejects_missing_brick() {
        let spec = forge_spec();
        let core = [10, 64, 10];
        let mut good = full_shell(core);
        assert!(validate_structure(core, &spec.cells, &mut good));
        assert!(!validate_structure(core, &[], &mut good)); // empty pattern rejects
        // Remove one brick — a single missing cell rejects everything.
        let mut base = full_shell(core);
        let mut broken = |pos: [i32; 3]| -> Option<BlockProbe> {
            if pos == [11, 64, 10] {
                Some(probe("minecraft:stone"))
            } else {
                base(pos)
            }
        };
        assert!(!validate_structure(core, &spec.cells, &mut broken));
        // Wrong aperture block rejects too.
        let mut base = full_shell(core);
        let mut bad_aperture = |pos: [i32; 3]| -> Option<BlockProbe> {
            if pos == [10, 64, 11] {
                Some(probe("minecraft:stone"))
            } else {
                base(pos)
            }
        };
        assert!(!validate_structure(core, &spec.cells, &mut bad_aperture));
    }

    #[test]
    fn forge_heat_smelt_and_conservation() {
        let spec = forge_spec();
        let core = [10, 64, 10];
        let mut rt = MultiBlockForgeRuntime::new(core);
        // Breath without a valid structure is ignored.
        assert!(!rt.apply_breath(&spec, ElementKind::Fire, 5));
        assert!(rt.revalidate(&spec, &mut full_shell(core)));
        // Charge: 8 ticks * 5 = 40 heat.
        assert!(rt.apply_breath(&spec, ElementKind::Fire, 8));
        assert!((rt.heat - 40.0).abs() < f32::EPSILON);
        // Wrong element rejected.
        assert!(!rt.apply_breath(&spec, ElementKind::Ice, 4));
        assert!((rt.heat - 40.0).abs() < f32::EPSILON);
        // Feed inputs.
        rt.insert_input(&CapabilityItemStack::new("anyforge:iron_ingot", 1));
        rt.insert_input(&CapabilityItemStack::new("anyforge:fire_catalyst", 1));
        // First tick: recipe consumes inputs + 30 heat, leaves 40-30 = 10.
        let ev = rt.tick(&spec);
        assert_eq!(ev.smelt_started, Some(0));
        assert_eq!(rt.input_count("anyforge:iron_ingot"), 0);
        assert_eq!(rt.input_count("anyforge:fire_catalyst"), 0);
        assert!((rt.heat - 10.0).abs() < 1e-4);
        // 5 more ticks complete the 5-tick smelt (the start tick does not
        // advance progress).
        let mut completed = None;
        for _ in 0..5 {
            let ev = rt.tick(&spec);
            completed = ev.smelt_completed.or(completed);
        }
        assert_eq!(completed, Some(0));
        assert_eq!(rt.output_count("anyforge:dragonsteel_ingot"), 1);
        // Lossless conservation: exactly 1 output for 1+1 inputs, nothing
        // stranded — inputs consumed once, output produced once.
        assert_eq!(rt.input.iter().map(|s| s.count).sum::<u32>(), 0);
        assert_eq!(rt.output.iter().map(|s| s.count).sum::<u32>(), 1);
        assert!(rt.active_smelt.is_none());
        // No recipe runnable without heat: an empty-ingredient tick only decays.
        rt.heat = 0.5;
        let ev = rt.tick(&spec);
        assert!(ev.smelt_started.is_none());
        assert!(rt.heat < 0.5, "idle heat decayed");
    }

    #[test]
    fn forge_waits_for_inputs_heat_and_matching_element() {
        let spec = forge_spec();
        let core = [10, 64, 10];
        let mut rt = MultiBlockForgeRuntime::new(core);
        rt.revalidate(&spec, &mut full_shell(core));
        rt.apply_breath(&spec, ElementKind::Fire, 8);
        // Missing catalyst — nothing starts.
        rt.insert_input(&CapabilityItemStack::new("anyforge:iron_ingot", 1));
        let ev = rt.tick(&spec);
        assert!(ev.smelt_started.is_none());
        assert_eq!(rt.input_count("anyforge:iron_ingot"), 1);
        // Heat below the recipe cost — nothing starts.
        rt.insert_input(&CapabilityItemStack::new("anyforge:fire_catalyst", 1));
        rt.heat = 5.0;
        let ev = rt.tick(&spec);
        assert!(ev.smelt_started.is_none());
        assert_eq!(rt.input_count("anyforge:iron_ingot"), 1);
        // Top the heat up, then it runs.
        rt.apply_breath(&spec, ElementKind::Fire, 6);
        let ev = rt.tick(&spec);
        assert_eq!(ev.smelt_started, Some(0));
    }

    #[test]
    fn broken_structure_pauses_but_never_loses_the_smelt() {
        let spec = forge_spec();
        let core = [10, 64, 10];
        let mut rt = MultiBlockForgeRuntime::new(core);
        rt.revalidate(&spec, &mut full_shell(core));
        rt.apply_breath(&spec, ElementKind::Fire, 8);
        rt.insert_input(&CapabilityItemStack::new("anyforge:iron_ingot", 1));
        rt.insert_input(&CapabilityItemStack::new("anyforge:fire_catalyst", 1));
        rt.tick(&spec);
        assert!(rt.active_smelt.is_some());
        // Structure breaks mid-smelt: progress holds, inputs already
        // consumed — nothing is voided.
        rt.structure_valid = false;
        let before = rt.active_smelt.as_ref().unwrap().progress_ticks;
        let ev = rt.tick(&spec);
        assert!(ev.structure_invalid);
        assert_eq!(rt.active_smelt.as_ref().unwrap().progress_ticks, before);
        // Rebuild: the smelt resumes and completes.
        rt.revalidate(&spec, &mut full_shell(core));
        for _ in 0..5 {
            rt.tick(&spec);
        }
        assert_eq!(rt.output_count("anyforge:dragonsteel_ingot"), 1);
    }

    #[test]
    fn forge_registers_as_block_capability_bus_endpoint() {
        use crate::capability_bus::{BlockCapabilityBus, SimpleSlotInventory};

        let spec = forge_spec();
        let core = [10, 64, 10];
        let mut rt = MultiBlockForgeRuntime::new(core);
        rt.revalidate(&spec, &mut full_shell(core));
        rt.apply_breath(&spec, ElementKind::Fire, 8);
        let forge_coord = rt.coord("minecraft:overworld");

        // A chest holding the recipe inputs, wired through the bus.
        let mut chest = SimpleSlotInventory::new(4, 64);
        chest.set_slot(0, Some(CapabilityItemStack::new("anyforge:iron_ingot", 1)));
        chest.set_slot(1, Some(CapabilityItemStack::new("anyforge:fire_catalyst", 1)));
        chest.set_slot(2, Some(CapabilityItemStack::new("minecraft:dirt", 3)));
        let chest_coord = BlockCoord::new("minecraft:overworld", 20, 64, 20);

        let mut bus = BlockCapabilityBus::new();
        bus.register_item_handler(chest_coord.clone(), Box::new(chest));
        bus.register_item_handler(forge_coord.clone(), Box::new(rt));

        // Accepted recipe items move; non-recipe dirt rolls back.
        assert_eq!(
            bus.transfer_item_between(&chest_coord, None, &forge_coord, None, 1, None),
            1
        );
        assert_eq!(
            bus.transfer_item_between(&chest_coord, None, &forge_coord, None, 1, None),
            1
        );
        assert_eq!(
            bus.transfer_item_between(&chest_coord, None, &forge_coord, None, 3, None),
            0
        );

        // Recover the runtime through the bus, run the smelt to output.
        let forge = bus
            .item_endpoint_mut(&forge_coord)
            .and_then(|h| h.as_any_mut().downcast_mut::<MultiBlockForgeRuntime>())
            .expect("forge endpoint");
        assert_eq!(forge.input_count("anyforge:iron_ingot"), 1);
        assert_eq!(forge.input_count("anyforge:fire_catalyst"), 1);
        for _ in 0..6 {
            forge.tick(&spec);
        }
        assert_eq!(forge.output_count("anyforge:dragonsteel_ingot"), 1);

        // A second chest pulls the finished alloy back out of the forge.
        let out_coord = BlockCoord::new("minecraft:overworld", 21, 64, 20);
        bus.register_item_handler(
            out_coord.clone(),
            Box::new(SimpleSlotInventory::new(4, 64)),
        );
        assert_eq!(
            bus.transfer_item_between(&forge_coord, None, &out_coord, None, 1, None),
            1
        );
        let forge = bus
            .item_endpoint(&forge_coord)
            .and_then(|h| h.as_any().downcast_ref::<MultiBlockForgeRuntime>())
            .expect("forge endpoint");
        assert_eq!(forge.output_count("anyforge:dragonsteel_ingot"), 0);
        // Dirt stayed in the source chest the whole time.
        let chest = bus
            .item_endpoint(&chest_coord)
            .and_then(|h| h.as_any().downcast_ref::<SimpleSlotInventory>())
            .expect("chest endpoint");
        assert_eq!(chest.total_items(), 3);
    }
}
