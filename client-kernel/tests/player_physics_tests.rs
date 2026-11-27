// SPDX-License-Identifier: LGPL-3.0-or-later
use megu_kernel::player_physics::{
    apply_knockback, step_player_physics, PlayerInputState, PlayerPhysicsState, SurfaceKind,
};
use megu_kernel::{Aabb, Error, FluidCell, FluidKind};

fn bounds(min: [f64; 3], max: [f64; 3]) -> Aabb {
    Aabb { min, max }
}

fn floor() -> Vec<Aabb> {
    vec![bounds([-100.0, -1.0, -100.0], [100.0, 0.0, 100.0])]
}

fn grounded() -> PlayerPhysicsState {
    PlayerPhysicsState {
        on_ground: true,
        ..PlayerPhysicsState::default()
    }
}

fn tick(
    state: PlayerPhysicsState,
    input: PlayerInputState,
    surface: SurfaceKind,
    shapes: &[Aabb],
) -> megu_kernel::player_physics::PhysicsStepOutcome {
    step_player_physics(state, input, surface, shapes, |_, _, _| {
        Ok(FluidCell::default())
    })
    .unwrap()
}

#[test]
fn flat_walk_stays_on_floor_and_diagonal_normalizes() {
    let ground = floor();
    let mut walking = grounded();
    let mut diagonal = grounded();
    for _ in 0..20 {
        walking = tick(
            walking,
            PlayerInputState {
                forward: 1.0,
                ..PlayerInputState::default()
            },
            SurfaceKind::Normal,
            &ground,
        )
        .state;
        diagonal = tick(
            diagonal,
            PlayerInputState {
                forward: 1.0,
                strafe: 1.0,
                ..PlayerInputState::default()
            },
            SurfaceKind::Normal,
            &ground,
        )
        .state;
        assert!(walking.on_ground);
        assert_eq!(walking.pos[1], 0.0);
    }
    assert!(walking.pos[2] > 2.0);
    assert!((diagonal.pos[0].hypot(diagonal.pos[2]) - walking.pos[2]).abs() < 1e-5);
}

#[test]
fn sprint_increases_ground_speed_and_jump_adds_forward_impulse() {
    let ground = floor();
    let mut walking = grounded();
    let mut sprinting = grounded();
    for _ in 0..30 {
        walking = tick(
            walking,
            PlayerInputState {
                forward: 1.0,
                ..PlayerInputState::default()
            },
            SurfaceKind::Normal,
            &ground,
        )
        .state;
        sprinting = tick(
            sprinting,
            PlayerInputState {
                forward: 1.0,
                sprint: true,
                ..PlayerInputState::default()
            },
            SurfaceKind::Normal,
            &ground,
        )
        .state;
    }
    let ratio = sprinting.pos[2] / walking.pos[2];
    assert!(ratio > 1.28 && ratio < 1.32, "sprint/walk = {ratio}");

    let walk_jump = tick(
        grounded(),
        PlayerInputState {
            jump: true,
            ..PlayerInputState::default()
        },
        SurfaceKind::Normal,
        &ground,
    )
    .state;
    let sprint_jump = tick(
        grounded(),
        PlayerInputState {
            jump: true,
            sprint: true,
            ..PlayerInputState::default()
        },
        SurfaceKind::Normal,
        &ground,
    )
    .state;
    assert_eq!(walk_jump.pos[1], sprint_jump.pos[1]);
    assert!(sprint_jump.pos[2] > walk_jump.pos[2] + 0.19);
}

#[test]
fn jump_apex_matches_1211_and_landing_tracks_fall() {
    let ground = floor();
    let mut state = grounded();
    let mut apex: f64 = 0.0;
    let mut landed = false;
    for i in 0..40 {
        let outcome = tick(
            state,
            PlayerInputState {
                jump: i == 0,
                ..PlayerInputState::default()
            },
            SurfaceKind::Normal,
            &ground,
        );
        state = outcome.state;
        apex = apex.max(state.pos[1]);
        if i > 1 && state.on_ground {
            landed = true;
            assert!(outcome.landed_fall_distance > 0.0);
            assert_eq!(state.fall_distance, 0.0);
            break;
        }
    }
    assert!((apex - 1.2522).abs() < 0.0002, "apex = {apex}");
    assert!(landed);
}

#[test]
fn climbs_half_slab_without_penetrating_wall() {
    let mut shapes = floor();
    shapes.push(bounds([1.0, 0.0, -1.0], [10.0, 0.5, 1.0]));
    let mut state = grounded();
    let mut stepped = false;
    for _ in 0..25 {
        let outcome = tick(
            state,
            PlayerInputState {
                strafe: 1.0,
                ..PlayerInputState::default()
            },
            SurfaceKind::Normal,
            &shapes,
        );
        stepped |= outcome.stepped_up;
        state = outcome.state;
    }
    assert!(stepped, "never stepped up onto half slab");
    assert!(state.pos[0] > 1.0);
    assert!((state.pos[1] - 0.5).abs() < 1e-7);
}

#[test]
fn sneaking_does_not_walk_off_a_cliff() {
    let cliff = [bounds([-10.0, -1.0, -10.0], [0.5, 0.0, 10.0])];
    let mut state = grounded();
    for _ in 0..100 {
        state = tick(
            state,
            PlayerInputState {
                strafe: 1.0,
                sneak: true,
                ..PlayerInputState::default()
            },
            SurfaceKind::Normal,
            &cliff,
        )
        .state;
        assert!(state.on_ground, "walked off at x {}", state.pos[0]);
        assert_eq!(state.pos[1], 0.0);
    }
    assert!(state.pos[0] > 0.0);
    assert!(state.pos[0] < 0.81);
}

#[test]
fn ice_retains_more_momentum_than_stone() {
    let ground = floor();
    let mut stone = grounded();
    let mut ice = grounded();
    let mut stone_at_release = 0.0;
    let mut ice_at_release = 0.0;
    for i in 0..55 {
        let input = PlayerInputState {
            forward: if i < 20 { 1.0 } else { 0.0 },
            ..PlayerInputState::default()
        };
        stone = tick(stone, input, SurfaceKind::Normal, &ground).state;
        ice = tick(ice, input, SurfaceKind::Ice, &ground).state;
        if i == 19 {
            stone_at_release = stone.pos[2];
            ice_at_release = ice.pos[2];
        }
    }
    assert!(ice.vel[2] > stone.vel[2]);
    assert!(ice.pos[2] - ice_at_release > stone.pos[2] - stone_at_release);
}

#[test]
fn cobweb_damps_velocity_and_resets_fall_distance() {
    let state = PlayerPhysicsState {
        pos: [0.0, 5.0, 0.0],
        vel: [1.0, -0.4, 1.0],
        fall_distance: 3.0,
        ..PlayerPhysicsState::default()
    };
    let result = tick(state, PlayerInputState::default(), SurfaceKind::Cobweb, &[]);
    assert!(result.state.pos[0] < 0.26);
    assert!(result.state.pos[1] > 4.97);
    assert_eq!(result.state.fall_distance, 0.0);
}

#[test]
fn ladder_climb_jump_and_sneak_hold() {
    let state = PlayerPhysicsState {
        pos: [0.0, 1.0, 0.0],
        vel: [0.8, -0.8, 0.7],
        ..PlayerPhysicsState::default()
    };
    let climb = tick(
        state,
        PlayerInputState {
            jump: true,
            ..PlayerInputState::default()
        },
        SurfaceKind::Ladder,
        &[],
    )
    .state;
    assert!(climb.pos[1] >= 1.2);
    assert!(climb.pos[0] <= 0.15 && climb.pos[2] <= 0.15);
    let hold = tick(
        state,
        PlayerInputState {
            sneak: true,
            ..PlayerInputState::default()
        },
        SurfaceKind::Ladder,
        &[],
    )
    .state;
    assert_eq!(hold.pos[1], state.pos[1]);
    assert_eq!(hold.fall_distance, 0.0);
}

#[test]
fn water_push_jump_and_air_supply() {
    let ground = floor();
    let water = |_: i32, y: i32, _: i32| {
        Ok(FluidCell {
            kind: (0..4).contains(&y).then_some(FluidKind::Water),
            amount: if (0..4).contains(&y) { 8 } else { 0 },
            ..FluidCell::default()
        })
    };
    let first = step_player_physics(
        grounded(),
        PlayerInputState {
            jump: true,
            ..PlayerInputState::default()
        },
        SurfaceKind::Normal,
        &ground,
        water,
    )
    .unwrap();
    assert!(first.state.in_water && first.state.eye_in_water);
    assert_eq!(first.state.air_supply, 299);
    assert!(first.state.pos[1] > 0.0);

    let mut state = grounded();
    for _ in 0..310 {
        state = step_player_physics(
            state,
            PlayerInputState::default(),
            SurfaceKind::Normal,
            &ground,
            water,
        )
        .unwrap()
        .state;
    }
    assert_eq!(state.air_supply, 0);
    assert!(state.in_water);
    state.pos[1] = 5.0;
    state = step_player_physics(
        state,
        PlayerInputState::default(),
        SurfaceKind::Normal,
        &[],
        water,
    )
    .unwrap()
    .state;
    assert_eq!(state.air_supply, 4);
}

#[test]
fn elytra_pitch_changes_lift_and_water_ends_glide() {
    let state = PlayerPhysicsState {
        pos: [0.0, 50.0, 0.0],
        vel: [0.0, -0.1, 1.0],
        elytra_flying: true,
        ..PlayerPhysicsState::default()
    };
    let dive = tick(
        state,
        PlayerInputState {
            pitch_deg: 40.0,
            ..PlayerInputState::default()
        },
        SurfaceKind::Normal,
        &[],
    )
    .state;
    let climb = tick(
        state,
        PlayerInputState {
            pitch_deg: -40.0,
            ..PlayerInputState::default()
        },
        SurfaceKind::Normal,
        &[],
    )
    .state;
    assert!((climb.pos[1] - dive.pos[1]).abs() > 0.01);
    assert!(dive.elytra_flying && climb.elytra_flying);
    let wet = step_player_physics(
        state,
        PlayerInputState::default(),
        SurfaceKind::Normal,
        &[],
        |_, y, _| {
            Ok(FluidCell {
                kind: (50..53).contains(&y).then_some(FluidKind::Water),
                amount: if (50..53).contains(&y) { 8 } else { 0 },
                ..FluidCell::default()
            })
        },
    )
    .unwrap();
    assert!(!wet.state.elytra_flying);
}

#[test]
fn level_elytra_glide_matches_1211_lift_and_float_drag() {
    let state = PlayerPhysicsState {
        pos: [0.0, 50.0, 0.0],
        vel: [0.0, 0.0, 1.0],
        elytra_flying: true,
        ..PlayerPhysicsState::default()
    };
    let result = tick(state, PlayerInputState::default(), SurfaceKind::Normal, &[]);
    // LivingEntity.travel at zero pitch: -0.02 gravity, +0.002 sinking lift,
    // +0.002 forward lift, then 10% horizontal look alignment.
    assert!((result.state.pos[1] - 49.982).abs() < 1e-9);
    assert!((result.state.pos[2] - 1.0018).abs() < 1e-9);
    assert!((result.state.vel[1] + 0.018 * 0.9800000190734863).abs() < 1e-9);
    assert!((result.state.vel[2] - 1.0018 * 0.9900000095367432).abs() < 1e-9);
}

#[test]
fn knockback_has_vanilla_direction_and_ground_lift_cap() {
    let mut state = grounded();
    state.vel = [0.3, 0.0, -0.4];
    apply_knockback(&mut state, 1.0, 3.0, 4.0);
    assert!((state.vel[0] + 0.45).abs() < 1e-9);
    assert!((state.vel[2] + 1.0).abs() < 1e-9);
    assert_eq!(state.vel[1], 0.4);
    let expected = state;
    apply_knockback(&mut state, f64::INFINITY, 3.0, 4.0);
    apply_knockback(&mut state, 1.0, 0.0, 0.0);
    assert_eq!(state, expected);
}

#[test]
fn nonfinite_values_and_invalid_shapes_fail_without_panicking() {
    let invalid = [f64::NAN, f64::INFINITY, f64::NEG_INFINITY];
    for value in invalid {
        let state = PlayerPhysicsState {
            pos: [value, 0.0, 0.0],
            ..grounded()
        };
        assert_eq!(
            step_player_physics(
                state,
                PlayerInputState::default(),
                SurfaceKind::Normal,
                &[],
                |_, _, _| Ok(FluidCell::default())
            ),
            Err(Error::InvalidArgument)
        );
        let state = PlayerPhysicsState {
            vel: [0.0, value, 0.0],
            ..grounded()
        };
        assert_eq!(
            step_player_physics(
                state,
                PlayerInputState::default(),
                SurfaceKind::Normal,
                &[],
                |_, _, _| Ok(FluidCell::default())
            ),
            Err(Error::InvalidArgument)
        );
    }
    for value in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
        let input = PlayerInputState {
            forward: value,
            ..PlayerInputState::default()
        };
        assert_eq!(
            step_player_physics(grounded(), input, SurfaceKind::Normal, &[], |_, _, _| Ok(
                FluidCell::default()
            )),
            Err(Error::InvalidArgument)
        );
        let input = PlayerInputState {
            yaw_deg: value,
            ..PlayerInputState::default()
        };
        assert_eq!(
            step_player_physics(grounded(), input, SurfaceKind::Normal, &[], |_, _, _| Ok(
                FluidCell::default()
            )),
            Err(Error::InvalidArgument)
        );
    }
    let input = PlayerInputState {
        forward: 200.0,
        ..PlayerInputState::default()
    };
    let normal = tick(
        grounded(),
        PlayerInputState {
            forward: 1.0,
            ..PlayerInputState::default()
        },
        SurfaceKind::Normal,
        &floor(),
    );
    assert_eq!(
        tick(grounded(), input, SurfaceKind::Normal, &floor())
            .state
            .pos,
        normal.state.pos
    );
    assert_eq!(
        step_player_physics(
            grounded(),
            PlayerInputState::default(),
            SurfaceKind::Normal,
            &[bounds([0.0; 3], [0.0; 3])],
            |_, _, _| Ok(FluidCell::default())
        ),
        Err(Error::InvalidArgument)
    );
}

// ---------------------------------------------------------------------------
// Vanilla 1.21.1 steady-state ground movement.
//
// `LivingEntity.travel` splits the per-tick ground response in two:
//
//   acceleration  a = getSpeed() * (0.21600002 / f³)   with the *raw* friction f
//   drag          g = f * 0.91                          applied after `move`
//
// so a tick advances the position by the pre-drag delta and stores
// `delta * g`. That recurses to the fixed point `u = a / (1 - f * 0.91)`, which on
// normal ground (f = 0.6, a = 0.1) is 0.2202643 blocks/tick, i.e. 4.405287 m/s.
// Cubing the drag instead of the friction made the old code 32.7% too fast
// (0.2923 blocks/tick, 5.845 m/s).
//
// The commonly quoted "vanilla walking is 4.317 m/s / 0.21585 blocks/tick" figure
// is exactly 0.98 of this value: it matches clients that smooth the movement input
// by 0.98 before the tick, which 1.21.1's `KeyboardInput`/`Input.tick` path does
// not do. The kernel models the entity side, so the parity target is the 0.22026
// fixed point above.
// ---------------------------------------------------------------------------

/// Vanilla ground drag for the default 0.6 friction.
const GROUND_DRAG: f64 = 0.6 * 0.91;

fn walk_input() -> PlayerInputState {
    PlayerInputState {
        forward: 1.0,
        ..PlayerInputState::default()
    }
}

/// Mean blocks travelled along +Z per tick once the acceleration ramp has settled.
fn steady_forward_speed(surface: SurfaceKind, input: PlayerInputState) -> f64 {
    const WARMUP: usize = 60;
    const SAMPLES: usize = 240;
    let ground = floor();
    let mut state = grounded();
    let mut travelled = 0.0;
    for index in 0..WARMUP + SAMPLES {
        let before = state.pos[2];
        state = tick(state, input, surface, &ground).state;
        if index >= WARMUP {
            travelled += state.pos[2] - before;
        }
    }
    travelled / SAMPLES as f64
}

#[test]
fn ground_walk_steady_state_matches_vanilla_1211() {
    let walk = steady_forward_speed(SurfaceKind::Normal, walk_input());
    let sprint = steady_forward_speed(
        SurfaceKind::Normal,
        PlayerInputState {
            sprint: true,
            ..walk_input()
        },
    );
    let walk_mps = walk * 20.0;
    let sprint_mps = sprint * 20.0;
    assert!(
        (walk - 0.1 / (1.0 - GROUND_DRAG)).abs() < 1e-6,
        "walk per tick = {walk}"
    );
    assert!((walk - 0.220_264_3).abs() < 1e-6, "walk per tick = {walk}");
    assert!((walk_mps - 4.405_287).abs() < 1e-3, "walk = {walk_mps} m/s");
    assert!(
        (sprint - 0.13 / (1.0 - GROUND_DRAG)).abs() < 1e-6,
        "sprint per tick = {sprint}"
    );
    assert!((sprint_mps - 5.726_872).abs() < 1e-3, "sprint = {sprint_mps} m/s");
    // The drag-cubed regression walked 0.2923 blocks/tick; keep it extinct.
    assert!(walk < 0.23, "walk per tick regressed to {walk}");
}

#[test]
fn ice_and_blue_ice_keep_vanilla_slipperiness() {
    // Raw friction feeds the acceleration, so slipperiness makes ice *faster*, and
    // the following drag is f * 0.91 for the same raw f.
    for (surface, friction) in [
        (SurfaceKind::Normal, 0.6),
        (SurfaceKind::Ice, 0.98),
        (SurfaceKind::PackedIce, 0.98),
        (SurfaceKind::BlueIce, 0.989),
        (SurfaceKind::Slime, 0.8),
    ] {
        let speed = steady_forward_speed(surface, walk_input());
        let expected = 0.1 * (0.21600002 / (friction * friction * friction))
            / (1.0 - friction * 0.91);
        // Blue ice friction is 0.989, so the f32 multiply inside the kernel leaves
        // a ~1.5e-5 residue against the f64 closed form.
        assert!(
            (speed - expected).abs() < 1e-4,
            "{surface:?} per tick = {speed}, expected {expected}"
        );
    }
}

#[test]
fn soul_sand_and_honey_scale_velocity_after_the_move() {
    // `Entity.move` multiplies the horizontal delta by `Block.getSpeedFactor`
    // (0.4) after the collision sweep, so the displacement of the current tick is
    // only damped through the carried velocity, not by shrinking the input speed.
    for surface in [SurfaceKind::SoulSand, SurfaceKind::Honey] {
        let speed = steady_forward_speed(surface, walk_input());
        let expected = 0.1 / (1.0 - 0.4 * GROUND_DRAG);
        assert!(
            (speed - expected).abs() < 1e-6,
            "{surface:?} per tick = {speed}, expected {expected}"
        );
        assert!((speed - 0.127_942_7).abs() < 1e-6, "{surface:?} = {speed}");
    }
    // The two blocks differ only in jump factor and in honey's slide clamp.
    let honey = steady_forward_speed(SurfaceKind::Honey, walk_input());
    let soul_sand = steady_forward_speed(SurfaceKind::SoulSand, walk_input());
    assert!((honey - soul_sand).abs() < 1e-9, "{honey} vs {soul_sand}");
}

#[test]
fn honey_jump_uses_half_jump_factor_and_keeps_default_friction() {
    let honey = tick(
        grounded(),
        PlayerInputState {
            jump: true,
            ..PlayerInputState::default()
        },
        SurfaceKind::Honey,
        &floor(),
    )
    .state;
    // `getJumpPower` = 0.42 * Block.getJumpFactor() = 0.42 * 0.5.
    assert!((honey.pos[1] - 0.21).abs() < 1e-9, "jump = {}", honey.pos[1]);
    assert!((honey.vel[1] - (0.21 - 0.08) * 0.98).abs() < 1e-9);
    let normal = tick(
        grounded(),
        PlayerInputState {
            jump: true,
            ..PlayerInputState::default()
        },
        SurfaceKind::Normal,
        &floor(),
    )
    .state;
    assert!((normal.pos[1] - 0.42).abs() < 1e-9);
}

#[test]
fn slow_falling_caps_falling_gravity_to_a_hundredth() {
    let falling = PlayerPhysicsState {
        pos: [0.0, 50.0, 0.0],
        slow_falling: true,
        ..PlayerPhysicsState::default()
    };
    let capped = tick(falling, PlayerInputState::default(), SurfaceKind::Normal, &[]).state;
    // (0 - min(0.08, 0.01)) * 0.98
    assert!(
        (capped.vel[1] + 0.01 * 0.98).abs() < 1e-12,
        "vel = {}",
        capped.vel[1]
    );
    let uncapped = tick(
        PlayerPhysicsState {
            slow_falling: false,
            ..falling
        },
        PlayerInputState::default(),
        SurfaceKind::Normal,
        &[],
    )
    .state;
    assert!((uncapped.vel[1] + 0.08 * 0.98).abs() < 1e-12);
    // Rising motion is unaffected: the cap keys off `deltaMovement.y <= 0`.
    let rising = tick(
        PlayerPhysicsState {
            vel: [0.0, 0.1, 0.0],
            ..falling
        },
        PlayerInputState::default(),
        SurfaceKind::Normal,
        &[],
    )
    .state;
    assert!((rising.vel[1] - (0.1 - 0.08) * 0.98).abs() < 1e-12);
}

#[test]
fn powder_snow_and_sweet_berry_bush_scale_the_move_then_zero_velocity() {
    // `makeStuckInBlock` multipliers: the displacement shrinks per axis and the
    // stored velocity becomes zero before the tick's drag is applied.
    for (surface, expected) in [
        (SurfaceKind::PowderSnow, [0.9, 1.5, 0.9]),
        (SurfaceKind::SweetBerryBush, [0.8, 0.75, 0.8]),
    ] {
        let state = PlayerPhysicsState {
            pos: [0.0, 5.0, 0.0],
            vel: [0.5, -0.4, 0.5],
            fall_distance: 3.0,
            ..PlayerPhysicsState::default()
        };
        let moved = tick(state, PlayerInputState::default(), surface, &[]).state;
        for (axis, multiplier) in expected.iter().enumerate() {
            let expected_pos = state.pos[axis] + state.vel[axis] * multiplier;
            assert!(
                (moved.pos[axis] - expected_pos).abs() < 1e-12,
                "{surface:?} axis {axis}: {} != {expected_pos}",
                moved.pos[axis]
            );
        }
        assert_eq!(moved.vel[0], 0.0);
        assert_eq!(moved.vel[2], 0.0);
        assert!((moved.vel[1] + 0.08 * 0.98).abs() < 1e-12, "{}", moved.vel[1]);
        assert_eq!(moved.fall_distance, 0.0);
    }
}

#[test]
fn climbable_assigns_vanilla_vertical_velocity_on_horizontal_collision() {
    // Pressing into a ladder is what makes vanilla climb it: the collision assigns
    // 0.2 to the returned delta, which the following drag turns into 0.1176.
    let mut shapes = floor();
    shapes.push(bounds([0.3, -1.0, -5.0], [2.0, 3.0, 5.0]));
    let mut state = grounded();
    for _ in 0..5 {
        state = tick(
            state,
            PlayerInputState {
                strafe: 1.0,
                ..PlayerInputState::default()
            },
            SurfaceKind::Ladder,
            &shapes,
        )
        .state;
        assert!(
            (state.vel[1] - (0.2 - 0.08) * 0.98).abs() < 1e-9,
            "vel_y = {}",
            state.vel[1]
        );
    }
    // Without a wall there is no collision, so a still ladder player keeps sliding.
    let sliding = PlayerPhysicsState {
        pos: [0.0, 5.0, 0.0],
        vel: [0.0, -0.8, 0.0],
        ..PlayerPhysicsState::default()
    };
    let slide = tick(
        sliding,
        PlayerInputState::default(),
        SurfaceKind::Ladder,
        &[],
    )
    .state;
    assert!((slide.vel[1] + (0.15 + 0.08) * 0.98).abs() < 1e-9, "{}", slide.vel[1]);
}

#[test]
fn lava_splits_shallow_and_deep_drag_at_the_fluid_jump_threshold() {
    // `LivingEntity.getFluidJumpThreshold` is 0.4 for every moving pose (the
    // standing eye sits at 1.62), so lava shallower than that keeps the water-like
    // drag while deeper lava uses the heavier lava drag and gravity.
    let lava = |amount: u8| {
        move |_, y: i32, _| {
            Ok(FluidCell {
                kind: (y == 64).then_some(FluidKind::Lava),
                amount: if y == 64 { amount } else { 0 },
                ..FluidCell::default()
            })
        }
    };
    let submerged = |amount| {
        step_player_physics(
            PlayerPhysicsState {
                pos: [0.0, 64.0, 0.0],
                ..PlayerPhysicsState::default()
            },
            PlayerInputState::default(),
            SurfaceKind::Normal,
            &[],
            lava(amount),
        )
        .unwrap()
        .state
    };
    let deep = submerged(8);
    let shallow = submerged(2);
    assert!(deep.in_lava && shallow.in_lava);
    // Deep lava: `(0 - 0.08 / 4) * ...`; shallow: `(0 - 0.08 / 16)` under water drag.
    assert!(deep.vel[1] < shallow.vel[1], "{:?} {:?}", deep.vel, shallow.vel);
}
