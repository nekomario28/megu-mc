// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later

use mod_offload::combat_motion::{
    decode_motion_trigger, encode_motion_trigger, validate_hit, ActionPhase, ActionSpec,
    ActiveAction, ComboChain, ComboState, Hand, HitCandidate, HitShape,
    HitValidationVerdict as Hit, MotionError, MotionTrigger, TickWindow, Vec3, MOT1_MAGIC,
    MOT1_PACKET_LEN, MOTION_CHANNEL,
};

fn slash(id: u32) -> ActionSpec {
    ActionSpec {
        action_id: id,
        anticipation_ticks: 4,
        active_ticks: 3,
        recovery_ticks: 5,
        cancel_after_tick: Some(7),
        invulnerable_window: None,
        movement_speed_factor: 0.5,
        hit_shape: HitShape::SweepArc {
            reach: 3.0,
            half_angle_deg: 60.0,
            vertical_tolerance: 1.0,
        },
    }
}

fn action(spec: ActionSpec) -> ActiveAction {
    ActiveAction::start(spec, 1.0, 100, Hand::MainHand).unwrap()
}

fn candidate(target_id: u64, x: f64, y: f64, z: f64) -> HitCandidate {
    HitCandidate {
        target_id,
        position: Vec3::new(x, y, z),
    }
}

fn hit(action: &mut ActiveAction, target: HitCandidate, dodge: Option<&ActiveAction>) -> Hit {
    validate_hit(
        action,
        Vec3::new(0.0, 0.0, 0.0),
        Vec3::new(0.0, 0.0, 1.0),
        target,
        dodge,
    )
}

#[test]
fn full_action_progression_and_tick_rewind_protection() {
    let mut swing = action(slash(10));
    assert_eq!(swing.phase(), ActionPhase::Anticipation);
    assert_eq!(swing.movement_speed_factor(), 0.5);
    assert_eq!(swing.total_ticks(), 12);
    assert_eq!(swing.advance_to(103), Ok(ActionPhase::Anticipation));
    assert_eq!(swing.advance_to(104), Ok(ActionPhase::ActiveHitWindow));
    assert_eq!(swing.advance_to(106), Ok(ActionPhase::ActiveHitWindow));
    assert_eq!(swing.advance_to(107), Ok(ActionPhase::Recovery));
    assert!(swing.can_cancel());
    assert_eq!(swing.advance_to(111), Ok(ActionPhase::Recovery));
    assert_eq!(swing.advance_to(112), Ok(ActionPhase::Idle));
    assert_eq!(swing.movement_speed_factor(), 1.0);
    assert!(!swing.can_cancel());
    assert_eq!(swing.advance_to(112), Ok(ActionPhase::Idle));
    assert_eq!(swing.advance_to(111), Err(MotionError::TickRewind));
    assert_eq!(swing.advance_to(u64::MAX), Ok(ActionPhase::Idle));
}

#[test]
fn speed_scaling_clamps_and_rounds_each_phase() {
    let mut fast = ActiveAction::start(slash(11), 2.0, 0, Hand::OffHand).unwrap();
    assert_eq!(fast.speed_multiplier(), 2.0);
    assert_eq!(fast.total_ticks(), 7); // ceil(4/2) + ceil(3/2) + ceil(5/2)
    assert_eq!(fast.advance_to(2), Ok(ActionPhase::ActiveHitWindow));
    assert_eq!(fast.advance_to(4), Ok(ActionPhase::Recovery));
    assert!(fast.can_cancel());
    assert_eq!(fast.advance_to(7), Ok(ActionPhase::Idle));

    let slow = ActiveAction::start(slash(12), -10.0, 0, Hand::MainHand).unwrap();
    assert_eq!(slow.speed_multiplier(), 0.25);
    assert_eq!(slow.total_ticks(), 48);

    let ultra = ActiveAction::start(slash(13), 100.0, 0, Hand::MainHand).unwrap();
    assert_eq!(ultra.speed_multiplier(), 4.0);
    assert_eq!(ultra.total_ticks(), 4);
    assert!(matches!(
        ActiveAction::start(slash(14), f32::NAN, 0, Hand::MainHand),
        Err(MotionError::NonFinite)
    ));
    assert!(matches!(
        ActiveAction::start(slash(14), f32::INFINITY, 0, Hand::MainHand),
        Err(MotionError::NonFinite)
    ));
}

#[test]
fn cancels_are_gated_and_dodge_windows_are_half_open() {
    let mut dodge_spec = slash(20);
    dodge_spec.cancel_after_tick = Some(5);
    dodge_spec.invulnerable_window = Some(TickWindow {
        start_tick: 2,
        end_tick: 6,
    });
    let mut dodge = action(dodge_spec);
    assert!(!dodge.can_cancel());
    assert!(!dodge.is_invulnerable());
    dodge.advance_to(102).unwrap();
    assert!(dodge.is_invulnerable());
    assert!(!dodge.can_cancel());
    dodge.advance_to(105).unwrap();
    assert!(dodge.is_invulnerable());
    assert!(dodge.can_cancel());
    dodge.advance_to(106).unwrap();
    assert!(!dodge.is_invulnerable());

    // Narrow windows keep at least one tick even after acceleration.
    let mut quick = slash(21);
    quick.invulnerable_window = Some(TickWindow {
        start_tick: 5,
        end_tick: 6,
    });
    let mut rolled = ActiveAction::start(quick, 4.0, 0, Hand::OffHand).unwrap();
    rolled.advance_to(2).unwrap();
    assert!(rolled.is_invulnerable());
    rolled.advance_to(3).unwrap();
    assert!(!rolled.is_invulnerable());
}

#[test]
fn combo_advances_through_cancels_and_finished_actions_then_resets() {
    let mut thrust = slash(32);
    thrust.hit_shape = HitShape::ThrustCapsule {
        reach: 4.0,
        radius: 0.4,
    };
    let chain = ComboChain::new(vec![slash(30), slash(31), thrust], 3, true).unwrap();
    let mut combo = ComboState::default();

    let first = combo.trigger(&chain, 100, 1.0).unwrap();
    assert_eq!(
        (first.spec().action_id, first.combo_index(), first.hand()),
        (30, 0, Hand::MainHand)
    );
    assert_eq!(
        combo.trigger(&chain, 103, 1.0).unwrap_err(),
        MotionError::ActionLocked
    );
    let second = combo.trigger(&chain, 107, 1.0).unwrap();
    assert_eq!(
        (second.spec().action_id, second.combo_index(), second.hand()),
        (31, 1, Hand::OffHand)
    );
    assert_eq!(combo.advance_to(119), Ok(ActionPhase::Idle));
    let third = combo.trigger(&chain, 122, 1.0).unwrap();
    assert_eq!(
        (third.spec().action_id, third.combo_index(), third.hand()),
        (32, 2, Hand::MainHand)
    );
    assert_eq!(
        third.spec().hit_shape,
        HitShape::ThrustCapsule {
            reach: 4.0,
            radius: 0.4
        }
    );
    assert_eq!(combo.advance_to(134), Ok(ActionPhase::Idle));
    let reset = combo.trigger(&chain, 138, 1.0).unwrap();
    assert_eq!(
        (reset.spec().action_id, reset.combo_index(), reset.hand()),
        (30, 0, Hand::MainHand)
    );
    assert_eq!(combo.advance_to(137), Err(MotionError::TickRewind));
}

#[test]
fn combo_rejected_trigger_does_not_replace_the_active_step() {
    let chain = ComboChain::new(vec![slash(1), slash(2)], 5, false).unwrap();
    let mut state = ComboState::default();
    state.trigger(&chain, 10, 1.0).unwrap();
    assert_eq!(
        state.trigger(&chain, 11, 1.0).unwrap_err(),
        MotionError::ActionLocked
    );
    assert_eq!(state.current().unwrap().spec().action_id, 1);
    assert_eq!(state.trigger(&chain, 17, 1.0).unwrap().spec().action_id, 2);
    assert_eq!(state.current().unwrap().hand(), Hand::MainHand);
}

#[test]
fn combo_guards_against_same_tick_replays_and_chain_resize() {
    let mut immediate = slash(80);
    immediate.cancel_after_tick = Some(0);
    let chain = ComboChain::new(vec![immediate.clone(), slash(81), slash(82)], 2, true).unwrap();
    let mut state = ComboState::default();
    state.trigger(&chain, 5, 1.0).unwrap();
    assert_eq!(
        state.trigger(&chain, 5, 1.0).unwrap_err(),
        MotionError::ActionLocked
    );
    let next = state.trigger(&chain, 6, 1.0).unwrap();
    assert_eq!(next.spec().action_id, 81);
    assert_eq!(next.hand(), Hand::OffHand);

    // Replacing a three-step chain with a one-step chain cannot index past it.
    let smaller = ComboChain::new(vec![immediate], 2, true).unwrap();
    state.advance_to(18).unwrap();
    let reset = state.trigger(&smaller, 19, 1.0).unwrap();
    assert_eq!(reset.spec().action_id, 80);
    assert_eq!(reset.combo_index(), 0);
    assert_eq!(reset.hand(), Hand::MainHand);
}

#[test]
fn sweep_geometry_and_duplicate_early_behind_and_vertical_rejections() {
    let mut swing = action(slash(40));
    assert_eq!(
        hit(&mut swing, candidate(1, 0.0, 0.0, 2.0), None),
        Hit::OutsideActiveWindow
    );
    swing.advance_to(104).unwrap();
    assert_eq!(
        hit(&mut swing, candidate(1, 0.0, 0.0, 2.0), None),
        Hit::Accepted
    );
    assert_eq!(
        hit(&mut swing, candidate(1, 0.0, 0.0, 2.0), None),
        Hit::AlreadyHit
    );
    assert_eq!(
        hit(&mut swing, candidate(2, 0.0, 0.0, -1.0), None),
        Hit::OutsideArc
    );
    assert_eq!(
        hit(&mut swing, candidate(3, 0.0, 0.0, 3.1), None),
        Hit::OutOfRange
    );
    assert_eq!(
        hit(&mut swing, candidate(4, 0.0, 1.1, 2.0), None),
        Hit::OutOfRange
    );
    assert_eq!(
        hit(&mut swing, candidate(5, 2.0, 0.0, 0.0), None),
        Hit::OutsideArc
    );
    assert_eq!(
        hit(&mut swing, candidate(6, 1.0, 0.0, 1.0), None),
        Hit::Accepted
    );
    swing.advance_to(107).unwrap();
    assert_eq!(
        hit(&mut swing, candidate(7, 0.0, 0.0, 2.0), None),
        Hit::OutsideActiveWindow
    );
}

#[test]
fn thrust_capsule_radial_aoe_and_cone_are_authoritative() {
    let mut spec = slash(50);
    spec.hit_shape = HitShape::ThrustCapsule {
        reach: 3.0,
        radius: 0.5,
    };
    let mut thrust = action(spec.clone());
    thrust.advance_to(104).unwrap();
    assert_eq!(
        hit(&mut thrust, candidate(1, 0.4, 0.0, 2.0), None),
        Hit::Accepted
    );
    assert_eq!(
        hit(&mut thrust, candidate(2, 0.6, 0.0, 2.0), None),
        Hit::OutOfRange
    );
    assert_eq!(
        hit(&mut thrust, candidate(3, 0.0, 0.0, -0.2), None),
        Hit::OutsideArc
    );
    assert_eq!(
        hit(&mut thrust, candidate(4, 0.0, 0.0, 3.4), None),
        Hit::Accepted
    );
    assert_eq!(
        hit(&mut thrust, candidate(5, 0.0, 0.0, 3.6), None),
        Hit::OutOfRange
    );

    spec.hit_shape = HitShape::RadialAoe { radius: 2.0 };
    let mut radial = action(spec.clone());
    radial.advance_to(104).unwrap();
    assert_eq!(
        hit(&mut radial, candidate(1, 0.0, 0.0, -1.0), None),
        Hit::Accepted
    );
    assert_eq!(
        hit(&mut radial, candidate(2, 0.0, 0.0, 2.1), None),
        Hit::OutOfRange
    );
    assert_eq!(
        hit(&mut radial, candidate(3, 0.0, 2.0, 0.0), None),
        Hit::Accepted
    );

    spec.hit_shape = HitShape::ForwardCone {
        reach: 4.0,
        half_angle_deg: 30.0,
    };
    let mut cone = action(spec);
    cone.advance_to(104).unwrap();
    assert_eq!(
        hit(&mut cone, candidate(1, 1.0, 0.0, 3.0), None),
        Hit::Accepted
    );
    assert_eq!(
        hit(&mut cone, candidate(2, 2.0, 0.0, 1.0), None),
        Hit::OutsideArc
    );
    assert_eq!(
        hit(&mut cone, candidate(3, 0.0, 0.0, 5.0), None),
        Hit::OutOfRange
    );
    assert_eq!(
        hit(&mut cone, candidate(4, 0.0, 0.0, -1.0), None),
        Hit::OutsideArc
    );
}

#[test]
fn invulnerable_target_prevents_hit_and_does_not_consume_target() {
    let mut swing = action(slash(60));
    swing.advance_to(104).unwrap();
    let mut roll_spec = slash(61);
    roll_spec.invulnerable_window = Some(TickWindow {
        start_tick: 0,
        end_tick: 3,
    });
    let mut roll = action(roll_spec);
    let target = candidate(9, 0.0, 0.0, 1.0);
    assert_eq!(hit(&mut swing, target, Some(&roll)), Hit::Invulnerable);
    assert!(swing.already_hit().is_empty());
    roll.advance_to(103).unwrap();
    assert_eq!(hit(&mut swing, target, Some(&roll)), Hit::Accepted);
    assert_eq!(hit(&mut swing, target, Some(&roll)), Hit::AlreadyHit);
}

#[test]
fn invalid_geometry_and_specs_are_rejected_without_recording_hits() {
    let mut swing = action(slash(70));
    swing.advance_to(104).unwrap();
    assert_eq!(
        hit(&mut swing, candidate(1, f64::NAN, 0.0, 1.0), None),
        Hit::InvalidGeometry
    );
    assert_eq!(
        validate_hit(
            &mut swing,
            Vec3::new(0.0, 0.0, 0.0),
            Vec3::new(0.0, 0.0, 0.0),
            candidate(2, 0.0, 0.0, 1.0),
            None
        ),
        Hit::InvalidGeometry
    );
    assert!(swing.already_hit().is_empty());

    let mut bad = slash(71);
    bad.active_ticks = 0;
    assert!(matches!(
        ActiveAction::start(bad, 1.0, 0, Hand::MainHand),
        Err(MotionError::InvalidSpec)
    ));
    let mut bad = slash(72);
    bad.hit_shape = HitShape::RadialAoe {
        radius: f64::INFINITY,
    };
    assert!(matches!(
        ActiveAction::start(bad, 1.0, 0, Hand::MainHand),
        Err(MotionError::InvalidSpec)
    ));
    let mut bad = slash(73);
    bad.invulnerable_window = Some(TickWindow {
        start_tick: 3,
        end_tick: 3,
    });
    assert!(matches!(
        ActiveAction::start(bad, 1.0, 0, Hand::MainHand),
        Err(MotionError::InvalidSpec)
    ));
    assert_eq!(
        ComboChain::new(vec![], 4, true).unwrap_err(),
        MotionError::InvalidCombo
    );
    assert!(matches!(
        ActiveAction::start(slash(74), 1.0, u64::MAX, Hand::MainHand),
        Err(MotionError::TickOverflow)
    ));
}

#[test]
fn mot1_roundtrip_has_fixed_big_endian_layout() {
    assert_eq!(MOTION_CHANNEL, "megu:motion_v1");
    let mut swing = ActiveAction::start(slash(0x0102_0304), 1.5, 100, Hand::OffHand).unwrap();
    swing.advance_to(104).unwrap();
    let mut snapshot = MotionTrigger::from_action(0x0102_0304_0506_0708, &swing);
    snapshot.combo_index = 0x1122;
    let bytes = encode_motion_trigger(&snapshot).unwrap();
    assert_eq!(bytes.len(), MOT1_PACKET_LEN);
    assert_eq!(bytes[..4], MOT1_MAGIC);
    assert_eq!(&bytes[4..12], &snapshot.entity_id.to_be_bytes());
    assert_eq!(&bytes[12..16], &snapshot.action_id.to_be_bytes());
    assert_eq!(&bytes[16..18], &snapshot.combo_index.to_be_bytes());
    assert_eq!(bytes[18], Hand::OffHand as u8);
    assert_eq!(&bytes[23..27], &snapshot.elapsed_ticks.to_be_bytes());
    assert_eq!(decode_motion_trigger(&bytes), Ok(snapshot));
}

#[test]
fn mot1_hostile_input_is_rejected() {
    let snapshot = MotionTrigger {
        entity_id: 42,
        action_id: 1,
        combo_index: 0,
        hand: Hand::MainHand,
        speed_multiplier: 1.0,
        elapsed_ticks: 5,
        phase: ActionPhase::ActiveHitWindow,
    };
    let valid = encode_motion_trigger(&snapshot).unwrap();
    assert_eq!(
        decode_motion_trigger(&valid[..27]),
        Err(MotionError::InvalidPacket)
    );
    assert_eq!(
        decode_motion_trigger(&[valid.as_slice(), &[0]].concat()),
        Err(MotionError::InvalidPacket)
    );
    let mut bad = valid;
    bad[0] = b'X';
    assert_eq!(decode_motion_trigger(&bad), Err(MotionError::InvalidPacket));
    bad = valid;
    bad[18] = 2;
    assert_eq!(decode_motion_trigger(&bad), Err(MotionError::InvalidPacket));
    bad = valid;
    bad[27] = 4;
    assert_eq!(decode_motion_trigger(&bad), Err(MotionError::InvalidPacket));
    for speed in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
        bad = valid;
        bad[19..23].copy_from_slice(&speed.to_bits().to_be_bytes());
        assert_eq!(decode_motion_trigger(&bad), Err(MotionError::NonFinite));
        assert_eq!(
            encode_motion_trigger(&MotionTrigger {
                speed_multiplier: speed,
                ..snapshot
            }),
            Err(MotionError::NonFinite)
        );
    }
    for speed in [0.0_f32, 0.249, 4.001, -1.0] {
        bad = valid;
        bad[19..23].copy_from_slice(&speed.to_bits().to_be_bytes());
        assert_eq!(decode_motion_trigger(&bad), Err(MotionError::InvalidPacket));
        assert_eq!(
            encode_motion_trigger(&MotionTrigger {
                speed_multiplier: speed,
                ..snapshot
            }),
            Err(MotionError::InvalidPacket)
        );
    }
}
