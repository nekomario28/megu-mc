// SPDX-License-Identifier: LGPL-3.0-or-later
//! A deterministic, world-independent 20 Hz player movement step.
//!
//! `pos` is the centre of the player's feet. The caller supplies the collision
//! shapes around the full swept path (including 0.6 blocks above it), the block
//! beneath the player, and a fluid-cell lookup for the current AABB and eyes.
//! All collision resolution and fluid-current calculations use the shared kernel.

use crate::physics::{collide, fluid_impulse, Aabb, FluidCell, FluidKind, FluidQuery};
use crate::Error;
use megu_core::pose_ray::{movement_pose, PlayerPoseKind};
use std::sync::LazyLock;

const STEP_HEIGHT: f32 = 0.6;
const GRAVITY: f64 = 0.08;
const JUMP_VELOCITY: f64 = 0.42;
const VERTICAL_DRAG: f64 = 0.98;
const WATER_VERTICAL_DRAG: f64 = 0.800000011920929;
const AIR_MOVE_SPEED: f32 = 0.02;
const BASE_MOVEMENT_SPEED: f32 = 0.1;
const DEG_TO_RAD: f32 = 0.017453292;

// Minecraft 1.21.1's Mth.sin/cos index into a 65,536-entry float table.
// Build the same table once so movement directions use the game's float angles.
static SIN_TABLE: LazyLock<Box<[f32; 65536]>> = LazyLock::new(|| {
    Box::new(std::array::from_fn(|i| {
        ((i as f64) * std::f64::consts::TAU / 65536.0).sin() as f32
    }))
});

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SurfaceKind {
    #[default]
    Normal,
    Ice,
    PackedIce,
    BlueIce,
    Slime,
    Honey,
    SoulSand,
    Cobweb,
    Ladder,
    Vine,
    Scaffolding,
    PowderSnow,
}

impl SurfaceKind {
    fn slipperiness(self) -> f32 {
        match self {
            Self::Ice | Self::PackedIce => 0.98,
            Self::BlueIce => 0.989,
            Self::Slime => 0.8,
            Self::Honey => 0.4,
            _ => 0.6,
        }
    }

    fn climbable(self) -> bool {
        matches!(self, Self::Ladder | Self::Vine | Self::Scaffolding)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct PlayerInputState {
    pub forward: f32,
    pub strafe: f32,
    pub jump: bool,
    pub sneak: bool,
    pub sprint: bool,
    pub yaw_deg: f32,
    pub pitch_deg: f32,
    pub start_elytra: bool,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PlayerPhysicsState {
    pub pose: PlayerPoseKind,
    pub pos: [f64; 3],
    pub vel: [f64; 3],
    pub on_ground: bool,
    pub in_water: bool,
    pub in_lava: bool,
    pub eye_in_water: bool,
    pub swimming: bool,
    pub elytra_flying: bool,
    pub fall_distance: f32,
    pub air_supply: i32,
    pub jump_cooldown: u8,
    pub is_creative_flying: bool,
    pub ultra_warm: bool,
}

impl Default for PlayerPhysicsState {
    fn default() -> Self {
        Self {
            pose: PlayerPoseKind::Standing,
            pos: [0.0; 3],
            vel: [0.0; 3],
            on_ground: false,
            in_water: false,
            in_lava: false,
            eye_in_water: false,
            swimming: false,
            elytra_flying: false,
            fall_distance: 0.0,
            air_supply: 300,
            jump_cooldown: 0,
            is_creative_flying: false,
            ultra_warm: false,
        }
    }
}

impl PlayerPhysicsState {
    pub fn aabb(self) -> Aabb {
        let (min, max) = self.pose.dimensions().bounds(self.pos);
        Aabb { min, max }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PhysicsStepOutcome {
    pub state: PlayerPhysicsState,
    pub collided_horizontal: bool,
    pub collided_vertical: bool,
    pub stepped_up: bool,
    pub landed_fall_distance: f32,
    pub knocked_back: bool,
}

fn yaw_sin_cos(degrees: f32) -> (f64, f64) {
    let angle = degrees * DEG_TO_RAD;
    let index = ((angle * 10430.378) as i32 & 65535) as usize;
    (
        f64::from(SIN_TABLE[index]),
        f64::from(SIN_TABLE[(index + 16384) & 65535]),
    )
}

// Entity.getInputVector: normalize diagonals, then rotate by float yaw.
fn relative_input(strafe: f32, forward: f32, amount: f32, yaw: f32) -> [f64; 3] {
    let length_sq = f64::from(strafe) * f64::from(strafe) + f64::from(forward) * f64::from(forward);
    if length_sq < 1.0e-7 {
        return [0.0; 3];
    }
    let scale = if length_sq > 1.0 {
        f64::from(amount) / length_sq.sqrt()
    } else {
        f64::from(amount)
    };
    let (sin, cos) = yaw_sin_cos(yaw);
    [
        (f64::from(strafe) * cos - f64::from(forward) * sin) * scale,
        0.0,
        (f64::from(forward) * cos + f64::from(strafe) * sin) * scale,
    ]
}

fn moved_aabb(bounds: Aabb, dx: f64, dz: f64) -> Aabb {
    Aabb {
        min: [bounds.min[0] + dx, bounds.min[1], bounds.min[2] + dz],
        max: [bounds.max[0] + dx, bounds.max[1], bounds.max[2] + dz],
    }
}

// Entity.maybeBackOffFromEdge uses 0.05-block decrements while the displaced
// box has no support at step-height below its feet. Probe using shared collide.
fn back_off_from_edge(bounds: Aabb, delta: &mut [f64; 3], shapes: &[Aabb]) -> Result<(), Error> {
    let supported = |dx, dz| -> Result<bool, Error> {
        let probe = moved_aabb(bounds, dx, dz);
        Ok(collide(
            probe,
            [0.0, -f64::from(STEP_HEIGHT), 0.0],
            false,
            0.0,
            shapes,
        )?
        .vertical_collision_below)
    };
    let decrement = |v: f64| {
        if v.abs() <= 0.05 {
            0.0
        } else {
            v - 0.05 * v.signum()
        }
    };
    // Bounded by the maximum number of steps required by a finite velocity;
    // extremely large impulses are safely clipped to no horizontal motion.
    for _ in 0..256 {
        if supported(delta[0], 0.0)? {
            break;
        }
        delta[0] = decrement(delta[0]);
    }
    for _ in 0..256 {
        if supported(0.0, delta[2])? {
            break;
        }
        delta[2] = decrement(delta[2]);
    }
    for _ in 0..256 {
        if supported(delta[0], delta[2])? {
            return Ok(());
        }
        delta[0] = decrement(delta[0]);
        delta[2] = decrement(delta[2]);
    }
    delta[0] = 0.0;
    delta[2] = 0.0;
    Ok(())
}

/// LivingEntity.knockback. Vanilla subtracts the normalized `dx`/`dz` impulse
/// from half of the target's current horizontal velocity. Callers pass the
/// vector from the target toward the attacker to knock the target away.
pub fn apply_knockback(state: &mut PlayerPhysicsState, strength: f64, dx: f64, dz: f64) {
    if !strength.is_finite() || strength <= 0.0 || !dx.is_finite() || !dz.is_finite() {
        return;
    }
    let length = dx.hypot(dz);
    if length < 1.0e-7 || !length.is_finite() {
        return;
    }
    let x = state.vel[0] * 0.5 - strength * dx / length;
    let z = state.vel[2] * 0.5 - strength * dz / length;
    let y = if state.on_ground {
        (state.vel[1] * 0.5 + strength).min(0.4)
    } else {
        state.vel[1]
    };
    if [x, y, z].into_iter().all(f64::is_finite) {
        state.vel = [x, y, z];
    }
}

/// Advance one player by one Minecraft tick, with the supplied immutable world snapshot.
/// Invalid geometry and non-finite inputs return `Error` without modifying the input state.
pub fn step_player_physics(
    mut state: PlayerPhysicsState,
    input: PlayerInputState,
    surface: SurfaceKind,
    shapes: &[Aabb],
    mut fluid_at: impl FnMut(i32, i32, i32) -> Result<FluidCell, Error>,
) -> Result<PhysicsStepOutcome, Error> {
    if !state.pos.into_iter().all(f64::is_finite)
        || !state.vel.into_iter().all(f64::is_finite)
        || !state.fall_distance.is_finite()
        || ![input.forward, input.strafe, input.yaw_deg, input.pitch_deg]
            .into_iter()
            .all(f32::is_finite)
    {
        return Err(Error::InvalidArgument);
    }
    let forward = input.forward.clamp(-1.0, 1.0);
    let strafe = input.strafe.clamp(-1.0, 1.0);
    let yaw = input.yaw_deg.rem_euclid(360.0);
    let pitch = input.pitch_deg.clamp(-90.0, 90.0);
    let fits = |pose: PlayerPoseKind| {
        let (min, max) = pose.dimensions().bounds(state.pos);
        !shapes.iter().any(|shape| {
            (0..3).all(|i| min[i] + 1e-7 < shape.max[i] && max[i] - 1e-7 > shape.min[i])
        })
    };
    let was_swimming = state.swimming;
    let was_flying = state.elytra_flying;
    state.pose = movement_pose(input.sneak, was_swimming, was_flying, fits);
    let mut sample_fluids = |bounds| {
        let mut sample = |kind| {
            fluid_impulse(
                bounds,
                FluidQuery {
                    kind,
                    ultra_warm: state.ultra_warm,
                    pushed_by_fluid: true,
                    is_player: true,
                    velocity: state.vel,
                },
                &mut fluid_at,
            )
        };
        Ok::<_, Error>((sample(FluidKind::Water)?, sample(FluidKind::Lava)?))
    };
    let (mut water, mut lava) = sample_fluids(state.aabb())?;
    state.in_water = water.in_fluid;
    state.in_lava = lava.in_fluid;
    if !state.in_water && !state.in_lava && !state.on_ground && input.start_elytra {
        state.elytra_flying = true;
    }
    if state.on_ground || state.in_water || state.in_lava || state.is_creative_flying {
        state.elytra_flying = false;
    }
    state.swimming = state.in_water && (input.sprint || state.swimming) && !state.on_ground;

    if state.swimming != was_swimming || state.elytra_flying != was_flying {
        state.pose = movement_pose(input.sneak, state.swimming, state.elytra_flying, fits);
        (water, lava) = sample_fluids(state.aabb())?;
        state.in_water = water.in_fluid;
        state.in_lava = lava.in_fluid;
    }
    let bounds = state.aabb();
    let eye_height = f64::from(state.pose.dimensions().eye_height);
    let eye_bounds = Aabb {
        min: [
            state.pos[0] - 0.01,
            state.pos[1] + eye_height - 0.01,
            state.pos[2] - 0.01,
        ],
        max: [
            state.pos[0] + 0.01,
            state.pos[1] + eye_height + 0.01,
            state.pos[2] + 0.01,
        ],
    };
    state.eye_in_water = fluid_impulse(
        eye_bounds,
        FluidQuery {
            kind: FluidKind::Water,
            ultra_warm: state.ultra_warm,
            pushed_by_fluid: false,
            is_player: true,
            velocity: state.vel,
        },
        &mut fluid_at,
    )?
    .in_fluid;
    if state.eye_in_water && !state.is_creative_flying {
        state.air_supply = state.air_supply.saturating_sub(1).max(0);
    } else {
        state.air_supply = state.air_supply.saturating_add(4).min(300);
    }

    state.jump_cooldown = state.jump_cooldown.saturating_sub(1);
    for axis in 0..3 {
        state.vel[axis] += water.push[axis] + lava.push[axis];
    }
    let was_grounded = state.on_ground;
    let was_falling = state.vel[1] <= 0.0;

    if input.jump {
        if state.in_water || state.in_lava {
            state.vel[1] += 0.04;
        } else if state.on_ground && state.jump_cooldown == 0 && !state.is_creative_flying {
            state.vel[1] = if surface == SurfaceKind::Honey {
                JUMP_VELOCITY * 0.4
            } else {
                JUMP_VELOCITY
            };
            if input.sprint {
                let (sin, cos) = yaw_sin_cos(yaw);
                state.vel[0] -= sin * 0.2;
                state.vel[2] += cos * 0.2;
            }
            state.jump_cooldown = 10;
        }
    } else {
        state.jump_cooldown = 0;
    }

    let climbing = surface.climbable() && !state.elytra_flying;
    if climbing {
        state.vel[0] = state.vel[0].clamp(-0.15, 0.15);
        state.vel[2] = state.vel[2].clamp(-0.15, 0.15);
        state.vel[1] = state.vel[1].max(-0.15);
        if input.sneak && surface != SurfaceKind::Scaffolding {
            state.vel[1] = state.vel[1].max(0.0);
        }
        if input.jump {
            state.vel[1] = state.vel[1].max(0.2);
        }
        state.fall_distance = 0.0;
    }

    if surface == SurfaceKind::Cobweb {
        state.vel[0] *= 0.25;
        state.vel[1] *= 0.05;
        state.vel[2] *= 0.25;
        state.fall_distance = 0.0;
    } else if surface == SurfaceKind::PowderSnow {
        state.vel[0] *= 0.9;
        state.vel[1] *= 0.5;
        state.vel[2] *= 0.9;
        state.fall_distance = 0.0;
    } else if surface == SurfaceKind::Honey && state.vel[1] < -0.05 {
        state.vel[1] = -0.05;
    }

    let mut delta = state.vel;
    let (horizontal_drag, vertical_drag, fluid_gravity) = if state.is_creative_flying {
        let fly_speed = if input.sprint { 0.2 } else { 0.1 };
        let horizontal = relative_input(strafe, forward, fly_speed, yaw);
        delta[0] += horizontal[0];
        delta[2] += horizontal[2];
        delta[1] = match (input.jump, input.sneak) {
            (true, false) => 0.15,
            (false, true) => -0.15,
            _ => 0.0,
        };
        (0.91, 0.6, 0.0)
    } else if state.elytra_flying {
        // LivingEntity.travel uses the float pitch in radians, Math.cos for
        // lift and Mth.sin/cos for the direction from Entity.getLookAngle.
        let pitch_radians = pitch * DEG_TO_RAD;
        let (sin_yaw, cos_yaw) = yaw_sin_cos(yaw);
        let (sin_pitch, cos_pitch) = yaw_sin_cos(pitch);
        let look_x = -sin_yaw * cos_pitch;
        let look_z = cos_yaw * cos_pitch;
        let look_length = look_x.hypot(look_z);
        let horizontal_velocity = state.vel[0].hypot(state.vel[2]);
        let lift = f64::from(pitch_radians).cos().powi(2) * (look_length / 0.4).min(1.0);
        delta[1] += GRAVITY * (-1.0 + lift * 0.75);
        if delta[1] < 0.0 && look_length > 0.0 {
            let d = delta[1] * -0.1 * lift;
            delta[0] += look_x * d / look_length;
            delta[1] += d;
            delta[2] += look_z * d / look_length;
        }
        if pitch_radians < 0.0 && look_length > 0.0 {
            let d = horizontal_velocity * -sin_pitch * 0.04;
            delta[0] -= look_x * d / look_length;
            delta[1] += d * 3.2;
            delta[2] -= look_z * d / look_length;
        }
        if look_length > 0.0 {
            delta[0] += (look_x / look_length * horizontal_velocity - delta[0]) * 0.1;
            delta[2] += (look_z / look_length * horizontal_velocity - delta[2]) * 0.1;
        }
        (0.9900000095367432, 0.9800000190734863, 0.0)
    } else if state.in_water || state.in_lava {
        let amount = AIR_MOVE_SPEED;
        let horizontal = relative_input(strafe, forward, amount, yaw);
        delta[0] += horizontal[0];
        delta[2] += horizontal[2];
        if state.in_water {
            (
                if input.sprint { 0.9 } else { 0.8 },
                WATER_VERTICAL_DRAG,
                if input.sprint { 0.0 } else { GRAVITY / 16.0 },
            )
        } else if lava.max_height <= 0.4 {
            (
                0.5,
                WATER_VERTICAL_DRAG,
                if input.sprint { 0.0 } else { GRAVITY / 16.0 },
            )
        } else {
            (0.5, 0.5, GRAVITY / 4.0)
        }
    } else {
        let friction = if state.on_ground {
            surface.slipperiness() * 0.91
        } else {
            0.91
        };
        let mut speed = if state.on_ground {
            BASE_MOVEMENT_SPEED * (0.21600002 / (friction * friction * friction))
        } else {
            AIR_MOVE_SPEED
        };
        if input.sprint {
            speed *= 1.3;
        }
        if input.sneak {
            speed *= 0.3;
        }
        if matches!(surface, SurfaceKind::SoulSand | SurfaceKind::Honey) {
            speed *= 0.4;
        }
        let horizontal = relative_input(strafe, forward, speed, yaw);
        delta[0] += horizontal[0];
        delta[2] += horizontal[2];
        (f64::from(friction), VERTICAL_DRAG, GRAVITY)
    };

    if was_grounded && input.sneak && !state.in_water && !state.in_lava && !state.is_creative_flying
    {
        back_off_from_edge(bounds, &mut delta, shapes)?;
    }

    let col = collide(
        bounds,
        delta,
        was_grounded,
        if state.elytra_flying {
            0.0
        } else {
            STEP_HEIGHT
        },
        shapes,
    )?;
    for axis in 0..3 {
        state.pos[axis] += col.delta[axis];
    }
    // An initial grounded snapshot can have zero vertical velocity. Preserve
    // support on that one tick; the next tick has vanilla's -0.0784 residue.
    let standing_on_support = was_grounded && delta[1] == 0.0 && {
        collide(
            state.aabb(),
            [0.0, -f64::from(STEP_HEIGHT), 0.0],
            false,
            0.0,
            shapes,
        )?
        .vertical_collision_below
    };
    state.on_ground = col.vertical_collision_below || standing_on_support;
    if col.vertical_collision {
        delta[1] = 0.0;
    }
    if climbing && col.horizontal_collision {
        delta[1] = delta[1].max(0.2);
    }
    state.vel[0] = if col.delta[0] != delta[0] {
        0.0
    } else {
        delta[0] * horizontal_drag
    };
    state.vel[2] = if col.delta[2] != delta[2] {
        0.0
    } else {
        delta[2] * horizontal_drag
    };
    state.vel[1] = if state.elytra_flying {
        delta[1] * vertical_drag
    } else if state.in_water || state.in_lava {
        delta[1] * vertical_drag - fluid_gravity
    } else {
        (delta[1] - fluid_gravity) * vertical_drag
    };

    let mut landed_fall_distance = 0.0;
    if state.on_ground && !was_grounded {
        landed_fall_distance = state.fall_distance;
    }
    if state.on_ground
        || col.delta[1] > 0.0
        || state.in_water
        || state.in_lava
        || climbing
        || state.elytra_flying
        || matches!(surface, SurfaceKind::Cobweb | SurfaceKind::PowderSnow)
    {
        state.fall_distance = 0.0;
    } else if col.delta[1] < 0.0 {
        state.fall_distance += (-col.delta[1]) as f32;
    }
    if state.on_ground && state.elytra_flying {
        state.elytra_flying = false;
    }
    if state.on_ground && surface == SurfaceKind::Slime && !input.sneak && was_falling {
        state.vel[1] = (-delta[1]).max(0.0);
    }
    if !state.pos.into_iter().all(f64::is_finite)
        || !state.vel.into_iter().all(f64::is_finite)
        || !state.fall_distance.is_finite()
    {
        return Err(Error::OutOfRange);
    }
    Ok(PhysicsStepOutcome {
        state,
        collided_horizontal: col.horizontal_collision,
        collided_vertical: col.vertical_collision,
        stepped_up: col.stepped_up,
        landed_fall_distance,
        knocked_back: false,
    })
}

#[cfg(test)]
mod pose_tests {
    use super::*;

    #[test]
    fn release_under_slab_keeps_low_pose_until_headroom_opens() {
        let mut state = PlayerPhysicsState {
            on_ground: true,
            ..Default::default()
        };
        for (height, sneak, expected) in [
            (0.75, false, PlayerPoseKind::Swimming),
            (1.5, true, PlayerPoseKind::Crouching),
            (1.5, false, PlayerPoseKind::Crouching),
            (2.0, false, PlayerPoseKind::Standing),
        ] {
            state.pos = [0.0; 3];
            state.vel = [0.0; 3];
            let shapes = [Aabb {
                min: [-1.0, height, -1.0],
                max: [1.0, 3.0, 1.0],
            }];
            state = step_player_physics(
                state,
                PlayerInputState {
                    sneak,
                    ..Default::default()
                },
                SurfaceKind::Normal,
                &shapes,
                |_, _, _| Ok(FluidCell::default()),
            )
            .unwrap()
            .state;
            assert_eq!(state.pose, expected);
            assert_eq!(
                state.aabb().max[1] - state.pos[1],
                f64::from(expected.dimensions().height)
            );
        }
    }

    #[test]
    fn crouching_eye_submerges_below_standing_eye() {
        let water = |_, y, _| {
            Ok(if y == 1 {
                FluidCell {
                    kind: Some(FluidKind::Water),
                    amount: 4,
                    ..Default::default()
                }
            } else {
                FluidCell::default()
            })
        };
        for sneak in [false, true] {
            let state = step_player_physics(
                PlayerPhysicsState::default(),
                PlayerInputState {
                    sneak,
                    ..Default::default()
                },
                SurfaceKind::Normal,
                &[],
                water,
            )
            .unwrap()
            .state;
            assert_eq!(state.eye_in_water, sneak);
        }
    }
}
