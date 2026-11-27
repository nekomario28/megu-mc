// SPDX-License-Identifier: LGPL-3.0-or-later
#![forbid(unsafe_code)]
//! The single source of truth for Vanilla 1.21.1 entity physics constants and
//! pure movement helpers, so the server tick (`src/`) and the client
//! prediction (`client-kernel/`) cannot drift apart again.
//!
//! Every value below was read from the shipped 1.21.1 runtime bytecode, not
//! from a wiki or a previous port:
//!
//! * `LivingEntity.travel(Vec3)` — gravity, Slow Falling, the horizontal air
//!   drag `0.91F`, the vertical drag `0.98F`, and the jump power path.
//! * `LivingEntity.getFrictionInfluencedSpeed(float)` —
//!   `getSpeed() * 0.21600002F / (f * f * f)` using the **raw** block friction
//!   `f`. The per-tick ground drag is the separate `f * 0.91F`, so cubing the
//!   drag instead of the friction is the historical 32.7 %-too-fast bug.
//! * `LivingEntity.calculateFallDamage(float, float)` —
//!   `Mth.ceil((fallDistance - safeFallDistance) * multiplier *
//!   fallDamageMultiplier)`.
//! * `Attributes` defaults — `GRAVITY = 0.08D`,
//!   `SAFE_FALL_DISTANCE = 3.0D`, `FALL_DAMAGE_MULTIPLIER = 1.0D`,
//!   `JUMP_STRENGTH = 0.42F` (stored as the `double` `0.41999998688697815`).
//! * `Blocks` property construction — default friction `0.6F`, ice `0.98F`,
//!   packed ice `0.98F`, blue ice `0.989F`, slime `0.8F`, soul sand
//!   speed factor `0.4F`, honey speed factor `0.4F` and jump factor `0.5F`.
//!
//! The float-valued constants stay `f32` because the game computes them in
//! Java `float` and widens the result; the `f64` constants are the exact
//! doubles the JVM loads. Mixing the two precisions is therefore explicit at
//! every call site instead of being an accident.

pub use crate::crowd_physics::Aabb as PhysicsAabb;

/// `Attributes.GRAVITY` default: downward acceleration in blocks per tick².
pub const VANILLA_GRAVITY: f64 = 0.08;

/// `LivingEntity.travel`: gravity cap while the Slow Falling effect is active.
pub const VANILLA_SLOW_FALLING_GRAVITY: f64 = 0.01;

/// Vertical drag applied after `Entity.move` (`0.98F` widened to `double`).
pub const VANILLA_AIR_DRAG_Y: f64 = 0.980_000_019_073_486_3;

/// Horizontal air drag applied when airborne (`0.91F`).
pub const VANILLA_HORIZONTAL_AIR_DRAG: f32 = 0.91;

/// `BlockBehaviour.Properties` default friction.
pub const VANILLA_GROUND_FRICTION_DEFAULT: f32 = 0.6;

/// Ice and packed ice friction.
pub const VANILLA_ICE_FRICTION: f32 = 0.98;

/// Blue ice friction.
pub const VANILLA_BLUE_ICE_FRICTION: f32 = 0.989;

/// Slime block friction.
pub const VANILLA_SLIME_FRICTION: f32 = 0.8;

/// `SoulSandBlock` speed factor (`Block.getSpeedFactor`).
pub const VANILLA_SOUL_SAND_SPEED_FACTOR: f32 = 0.4;

/// Honey block speed factor (`Block.getSpeedFactor`).
pub const VANILLA_HONEY_SPEED_FACTOR: f32 = 0.4;

/// Honey block jump factor (`Block.getJumpFactor`).
pub const VANILLA_HONEY_JUMP_FACTOR: f32 = 0.5;

/// `Attributes.JUMP_STRENGTH` default (`0.42F` widened to `double`), before
/// the block jump factor and the Jump Boost bonus are applied.
pub const VANILLA_BASE_JUMP_VELOCITY: f64 = 0.419_999_986_886_978_15;

/// `Attributes.SAFE_FALL_DISTANCE` default: fall distance in blocks that deals
/// no damage.
pub const VANILLA_SAFE_FALL_DISTANCE: f32 = 3.0;

/// Numerator of `getFrictionInfluencedSpeed`, divided by `friction³`.
pub const VANILLA_FRICTION_ACCEL_NUMERATOR: f32 = 0.21600002;

/// `getFrictionInfluencedSpeed` without the `getSpeed()` factor:
/// `0.21600002 / friction³`.
///
/// Vanilla cubes the **raw** block friction here and applies `friction * 0.91`
/// separately as the per-tick drag. Do not pass the already-dragged value:
/// `(friction * 0.91)³` is the classic 32.7 % overspeed bug.
///
/// The division is `f32` exactly like the JVM path (`fdiv`). A zero friction
/// yields infinity rather than a panic; vanilla never supplies zero.
#[must_use]
pub fn ground_friction_factor(friction: f32) -> f32 {
    VANILLA_FRICTION_ACCEL_NUMERATOR / (friction * friction * friction)
}

/// The exact vanilla `getFrictionInfluencedSpeed(base_speed, friction)`.
///
/// The operand order matters: the JVM multiplies `getSpeed()` by the numerator
/// first and divides by the cubed friction afterwards, which is not always
/// bit-identical to `base_speed * ground_friction_factor(friction)`. Use this
/// helper when the result feeds physics; use [`ground_friction_factor`] when
/// only the dimensionless factor is wanted.
#[must_use]
pub fn friction_influenced_speed(base_speed: f32, friction: f32) -> f32 {
    (base_speed * VANILLA_FRICTION_ACCEL_NUMERATOR) / (friction * friction * friction)
}

/// Per-tick ground drag for a block friction: `friction * 0.91` in `f32`,
/// exactly as `travel` computes it before widening to `double`.
#[must_use]
pub fn ground_horizontal_drag(friction: f32) -> f32 {
    friction * VANILLA_HORIZONTAL_AIR_DRAG
}

/// Gravity for the current tick, honouring the Slow Falling effect.
///
/// Vanilla: if the tick starts with `deltaMovement.y <= 0` and the entity has
/// Slow Falling, use `Math.min(gravity, 0.01)`; otherwise use the attribute
/// value unchanged. A `NaN` base gravity propagates like `Math.min`, and a
/// base gravity below `0.01` is preserved rather than raised.
#[must_use]
pub fn effective_gravity(base_gravity: f64, vy: f64, slow_falling: bool) -> f64 {
    if !(slow_falling && vy <= 0.0) {
        return base_gravity;
    }
    if base_gravity.is_nan() {
        return f64::NAN;
    }
    if base_gravity <= VANILLA_SLOW_FALLING_GRAVITY {
        base_gravity
    } else {
        VANILLA_SLOW_FALLING_GRAVITY
    }
}

/// Vanilla fall damage in half-hearts:
/// `ceil((fall_distance - safe_fall_distance - jump_boost) * multiplier)`,
/// clamped at zero.
///
/// `safe_fall_distance` is usually [`VANILLA_SAFE_FALL_DISTANCE`], or the
/// entity's `SAFE_FALL_DISTANCE` attribute. `multiplier` is the product the
/// caller already combined from the damage multiplier and the entity's
/// `FALL_DAMAGE_MULTIPLIER` attribute (both are `1.0F` for a vanilla player).
///
/// `jump_boost_amp` is an **opt-in legacy** aid: when `Some(amp)` it subtracts
/// `amp` whole blocks. Vanilla 1.21.1 does **not** reduce fall damage through
/// the Jump Boost effect — `calculateFallDamage` subtracts only the safe fall
/// distance — so pass `None` for exact 1.21.1 parity. The parameter exists for
/// mods and pre-1.21 datapack semantics that deliberately keep the old aid.
///
/// Non-finite inputs fail closed to `0.0` instead of propagating a non-finite
/// damage value: the JVM cast of `NaN` also yields no damage, while an
/// infinite distance is treated as untrusted input rather than as maximum
/// damage.
#[must_use]
pub fn fall_damage_blocks(
    fall_distance: f32,
    safe_fall_distance: f32,
    jump_boost_amp: Option<u8>,
    multiplier: f32,
) -> f32 {
    let jump_boost = jump_boost_amp.map_or(0.0, f32::from);
    let excess = fall_distance - safe_fall_distance - jump_boost;
    if !excess.is_finite() || !multiplier.is_finite() {
        return 0.0;
    }
    let damage = (excess * multiplier).ceil();
    if damage.is_finite() {
        damage.max(0.0)
    } else {
        0.0
    }
}

/// Vanilla `AABB.clip(from, to)`: the point where the segment first enters the
/// box, or `None` when the segment starts inside, misses, or does not reach the
/// box within this tick.
///
/// This is the swept-shape primitive the collision resolver runs per axis; it
/// is `NaN`-safe and treats an invalid (`min > max`) or non-finite box as no
/// hit. A segment that starts exactly on a face and moves inward is treated as
/// starting inside, matching the JVM's strict `0.0 < t` entry test.
#[must_use]
pub fn aabb_clip_segment(aabb: PhysicsAabb, from: [f64; 3], to: [f64; 3]) -> Option<[f64; 3]> {
    // `AABB.clip`'s accepted-entry epsilon.
    const EPSILON: f64 = 1.0e-7;
    let min = [aabb.min_x, aabb.min_y, aabb.min_z];
    let max = [aabb.max_x, aabb.max_y, aabb.max_z];
    let mut enter = 0.0_f64;
    let mut exit = 1.0_f64;
    let axes = from.iter().zip(to.iter()).zip(min.iter().zip(max.iter()));
    for ((&origin, &target), (&lo, &hi)) in axes {
        let delta = target - origin;
        if !origin.is_finite()
            || !delta.is_finite()
            || !lo.is_finite()
            || !hi.is_finite()
            || lo > hi
        {
            return None;
        }
        if delta.abs() <= EPSILON {
            // Parallel to this slab: the segment only hits if it is inside it.
            if origin < lo - EPSILON || origin > hi + EPSILON {
                return None;
            }
            continue;
        }
        let (near, far) = if delta > 0.0 { (lo, hi) } else { (hi, lo) };
        enter = enter.max((near - origin) / delta);
        exit = exit.min((far - origin) / delta);
        if enter > exit {
            return None;
        }
    }
    // Only an entry from outside the box, strictly inside this tick.
    if enter <= 0.0 || enter >= 1.0 || exit < 0.0 {
        return None;
    }
    Some([
        from[0] + (to[0] - from[0]) * enter,
        from[1] + (to[1] - from[1]) * enter,
        from[2] + (to[2] - from[2]) * enter,
    ])
}

#[cfg(test)]
mod tests {
    use super::{
        aabb_clip_segment, effective_gravity, fall_damage_blocks, friction_influenced_speed,
        ground_friction_factor, ground_horizontal_drag, PhysicsAabb, VANILLA_AIR_DRAG_Y,
        VANILLA_BASE_JUMP_VELOCITY, VANILLA_BLUE_ICE_FRICTION, VANILLA_FRICTION_ACCEL_NUMERATOR,
        VANILLA_GRAVITY, VANILLA_GROUND_FRICTION_DEFAULT, VANILLA_HONEY_JUMP_FACTOR,
        VANILLA_HONEY_SPEED_FACTOR, VANILLA_HORIZONTAL_AIR_DRAG, VANILLA_ICE_FRICTION,
        VANILLA_SAFE_FALL_DISTANCE, VANILLA_SLIME_FRICTION, VANILLA_SLOW_FALLING_GRAVITY,
        VANILLA_SOUL_SAND_SPEED_FACTOR,
    };

    fn unit_box() -> PhysicsAabb {
        PhysicsAabb {
            min_x: 0.0,
            min_y: 0.0,
            min_z: 0.0,
            max_x: 1.0,
            max_y: 1.0,
            max_z: 1.0,
        }
    }

    fn assert_close(actual: f64, expected: f64, tolerance: f64) {
        assert!(
            (actual - expected).abs() <= tolerance,
            "expected {expected}, got {actual}"
        );
    }

    #[test]
    fn constants_match_the_jvm_doubles() {
        assert_eq!(VANILLA_GRAVITY, 0.08);
        assert_eq!(VANILLA_SLOW_FALLING_GRAVITY, 0.01);
        // `travel` widens the `0.98F`/`0.42F` float literals to `double`.
        assert_eq!(VANILLA_AIR_DRAG_Y, f64::from(0.98_f32));
        assert_eq!(VANILLA_BASE_JUMP_VELOCITY, f64::from(0.42_f32));
        assert_eq!(VANILLA_HORIZONTAL_AIR_DRAG, 0.91);
        assert_eq!(VANILLA_GROUND_FRICTION_DEFAULT, 0.6);
        assert_eq!(VANILLA_ICE_FRICTION, 0.98);
        assert_eq!(VANILLA_BLUE_ICE_FRICTION, 0.989);
        assert_eq!(VANILLA_SLIME_FRICTION, 0.8);
        assert_eq!(VANILLA_SOUL_SAND_SPEED_FACTOR, 0.4);
        assert_eq!(VANILLA_HONEY_SPEED_FACTOR, 0.4);
        assert_eq!(VANILLA_HONEY_JUMP_FACTOR, 0.5);
        assert_eq!(VANILLA_SAFE_FALL_DISTANCE, 3.0);
        assert_eq!(VANILLA_FRICTION_ACCEL_NUMERATOR, 0.21600002);
    }

    #[test]
    fn default_ground_friction_factor_is_exactly_one() {
        // 0.6F³ rounds to the same f32 as the 0.21600002F numerator.
        assert_eq!(ground_friction_factor(0.6), 1.0);
        assert_eq!(friction_influenced_speed(0.1, 0.6), 0.1);
        assert_eq!(ground_friction_factor(0.98), 0.229_496_21_f32);
        // The buggy "(friction * 0.91)³" factor would be nowhere near one.
        let buggy = ground_friction_factor(0.6 * 0.91);
        assert!((buggy - 1.0).abs() > 0.3);
    }

    #[test]
    fn steady_walk_golden_matches_the_vanilla_fixed_point() {
        let speed = f64::from(friction_influenced_speed(
            0.1,
            VANILLA_GROUND_FRICTION_DEFAULT,
        ));
        let drag = f64::from(ground_horizontal_drag(VANILLA_GROUND_FRICTION_DEFAULT));
        assert_eq!(drag, 0.546_000_063_419_342);
        // u = a / (1 - f * 0.91) = 0.2202643 blocks/tick = 4.405287 m/s.
        let steady = speed / (1.0 - drag);
        assert_close(steady, 0.220_264_351_231_577_87, 1.0e-12);
        assert_close(steady * 20.0, 4.405_287_024_631_558, 1.0e-11);
    }

    #[test]
    fn ice_family_and_slime_reach_the_vanilla_terminal_speeds() {
        for (friction, expected) in [
            (VANILLA_ICE_FRICTION, 0.212_103_796_960_712_7),
            (VANILLA_BLUE_ICE_FRICTION, 0.223_265_476_738_926_1),
            (VANILLA_SLIME_FRICTION, 0.155_101_144_935_291_7),
        ] {
            let speed = f64::from(friction_influenced_speed(0.1, friction));
            let drag = f64::from(ground_horizontal_drag(friction));
            assert_close(speed / (1.0 - drag), expected, 1.0e-12);
        }
    }

    #[test]
    fn soul_sand_and_honey_speed_factor_damps_carried_velocity() {
        // `Entity.move` multiplies the stored x/z velocity by the block's
        // `getSpeedFactor`; the acceleration is unchanged.
        let speed = f64::from(friction_influenced_speed(
            0.1,
            VANILLA_GROUND_FRICTION_DEFAULT,
        ));
        let drag = f64::from(ground_horizontal_drag(VANILLA_GROUND_FRICTION_DEFAULT))
            * f64::from(VANILLA_SOUL_SAND_SPEED_FACTOR);
        assert_close(drag, 0.218_400_028_622_150_8, 1.0e-15);
        assert_close(speed / (1.0 - drag), 0.127_942_688_270_357, 1.0e-12);
        // Honey shares the speed factor and halves the jump impulse.
        assert_eq!(
            f64::from(VANILLA_HONEY_JUMP_FACTOR) * VANILLA_BASE_JUMP_VELOCITY,
            f64::from(0.21_f32)
        );
    }

    #[test]
    fn slow_falling_only_lightens_a_fall_that_is_already_downward() {
        assert_eq!(effective_gravity(0.08, -0.5, true), 0.01);
        assert_eq!(effective_gravity(0.08, 0.0, true), 0.01);
        assert_eq!(effective_gravity(0.08, 0.5, true), 0.08);
        assert_eq!(effective_gravity(0.08, -1.0, false), 0.08);
        // A gravity attribute already below the cap is never raised.
        assert_eq!(effective_gravity(0.005, -1.0, true), 0.005);
        assert!(effective_gravity(f64::NAN, -1.0, true).is_nan());
        // NaN vertical velocity fails the `vy <= 0` test, like the JVM.
        assert_eq!(effective_gravity(0.08, f64::NAN, true), 0.08);
    }

    #[test]
    fn fall_damage_goldens_match_calculate_fall_damage() {
        assert_eq!(fall_damage_blocks(4.0, 3.0, None, 1.0), 1.0);
        assert_eq!(fall_damage_blocks(10.0, 3.0, None, 1.0), 7.0);
        assert_eq!(fall_damage_blocks(3.0, 3.0, None, 1.0), 0.0);
        assert_eq!(fall_damage_blocks(0.5, 3.0, None, 1.0), 0.0);
        assert_eq!(fall_damage_blocks(3.5, 3.0, None, 1.0), 1.0);
        assert_eq!(fall_damage_blocks(3.25, 3.0, None, 2.0), 1.0);
        assert_eq!(fall_damage_blocks(4.0, 3.0, None, 2.0), 2.0);
        // Opt-in legacy jump-boost aid subtracts whole blocks.
        assert_eq!(fall_damage_blocks(4.0, 3.0, Some(1), 1.0), 0.0);
        assert_eq!(fall_damage_blocks(5.0, 3.0, Some(1), 1.0), 1.0);
        // Non-finite input fails closed.
        assert_eq!(fall_damage_blocks(f32::NAN, 3.0, None, 1.0), 0.0);
        assert_eq!(fall_damage_blocks(f32::INFINITY, 3.0, None, 1.0), 0.0);
        assert_eq!(fall_damage_blocks(4.0, 3.0, None, f32::NAN), 0.0);
    }

    #[test]
    fn clip_returns_the_face_the_segment_enters() {
        let hit = aabb_clip_segment(unit_box(), [-1.0, 0.5, 0.5], [2.0, 0.5, 0.5])
            .expect("segment crosses the box");
        assert_close(hit[0], 0.0, 1.0e-12);
        assert_close(hit[1], 0.5, 1.0e-12);
        assert_close(hit[2], 0.5, 1.0e-12);

        let hit = aabb_clip_segment(unit_box(), [0.5, 2.0, 0.5], [0.5, -1.0, 0.5])
            .expect("segment lands on the top face");
        assert_close(hit[0], 0.5, 1.0e-12);
        assert_close(hit[1], 1.0, 1.0e-12);
        assert_close(hit[2], 0.5, 1.0e-12);

        let hit = aabb_clip_segment(unit_box(), [-1.0, -1.0, -1.0], [2.0, 2.0, 2.0])
            .expect("diagonal crosses the corner");
        assert_close(hit[0], 0.0, 1.0e-12);
        assert_close(hit[1], 0.0, 1.0e-12);
        assert_close(hit[2], 0.0, 1.0e-12);
    }

    #[test]
    fn clip_is_empty_for_inside_starts_misses_and_tiny_steps() {
        // Starting inside never clips: the collision resolver must not
        // teleport an overlapping entity back to a face.
        assert_eq!(
            aabb_clip_segment(unit_box(), [0.5, 0.5, 0.5], [2.0, 0.5, 0.5]),
            None
        );
        // Starting exactly on the near face and moving inward is "inside".
        assert_eq!(
            aabb_clip_segment(unit_box(), [0.0, 0.5, 0.5], [2.0, 0.5, 0.5]),
            None
        );
        // Misses above the box and behind it.
        assert_eq!(
            aabb_clip_segment(unit_box(), [-1.0, 2.0, 0.5], [2.0, 2.0, 0.5]),
            None
        );
        assert_eq!(
            aabb_clip_segment(unit_box(), [2.0, 0.5, 0.5], [3.0, 0.5, 0.5]),
            None
        );
        // Ending exactly on the entry face is not a hit within this tick.
        assert_eq!(
            aabb_clip_segment(unit_box(), [-1.0, 0.5, 0.5], [0.0, 0.5, 0.5]),
            None
        );
        // Invalid and non-finite boxes fail closed.
        let mut invalid = unit_box();
        invalid.min_x = 2.0;
        assert_eq!(
            aabb_clip_segment(invalid, [-1.0, 0.5, 0.5], [3.0, 0.5, 0.5]),
            None
        );
        assert_eq!(
            aabb_clip_segment(unit_box(), [f64::NAN, 0.5, 0.5], [2.0, 0.5, 0.5]),
            None
        );
    }

    #[test]
    fn physics_aabb_is_the_shared_copy_box() {
        let aabb = unit_box();
        let copy = aabb;
        assert_eq!(copy, aabb);
    }
}
