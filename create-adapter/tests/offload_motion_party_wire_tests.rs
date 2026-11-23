// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later

//! Lane WIRE-ADAPTER-OFFLOAD-4: production-wiring proof that the live
//! [`OffloadBridge`] owns both offload surfaces the wiring audit
//! previously flagged as un-consumed:
//!
//! - `mod_offload::combat_motion` — per-entity server-authoritative
//!   action timelines: [`OffloadBridge::trigger_combat_action`]
//!   sequences combo steps, [`OffloadBridge::step_combat_motions`]
//!   advances every tracked entity on the server tick (folded into
//!   [`OffloadBridge::step_tick`], zero cost when empty), hit
//!   validation latches once per swing, and MOT1 snapshots
//!   encode/decode through the bridge.
//! - `mod_offload::party_storage` — companion party + PC-box registries
//!   keyed by owner UUID with canonical `PRTY` encode/decode, fail-closed
//!   install, and deterministic digest folding — again zero cost when
//!   empty.

use create_adapter::offload_bridge::{
    decode_motion_trigger, ActionPhase, ActionSpec, ComboChain, CompanionRecord, Hand, HitCandidate,
    HitShape, HitValidationVerdict, MotionVec3, OffloadBridge, OffloadBudget, OffloadError,
    PlayerCompanionStorage, StorageSlotRef, MOT1_MAGIC, MOT1_PACKET_LEN,
};

const ENTITY_PLAYER: u64 = 0xA1;
const ENTITY_TARGET: u64 = 0xB2;
const OWNER: [u8; 16] = [0x11; 16];
const OTHER_OWNER: [u8; 16] = [0x22; 16];

/// A three-phase melee swing: anticipation 2, active 3, recovery 2 — at
/// speed 1.0 the elapsed ticks line up exactly with the nominal spec.
fn swing_chain() -> ComboChain {
    ComboChain::new(
        vec![ActionSpec {
            action_id: 7,
            anticipation_ticks: 2,
            active_ticks: 3,
            recovery_ticks: 2,
            cancel_after_tick: None,
            invulnerable_window: None,
            movement_speed_factor: 1.0,
            hit_shape: HitShape::SweepArc {
                reach: 3.0,
                half_angle_deg: 90.0,
                vertical_tolerance: 2.0,
            },
        }],
        20,
        false,
    )
    .expect("swing chain validates")
}

fn companion(uid: u8, owner: [u8; 16]) -> CompanionRecord {
    let mut record = CompanionRecord::new([uid; 16], owner, "mymod:ember_cub");
    record.nickname = Some(format!("cub-{uid}"));
    record.moves = vec!["mymod:scratch".to_string()];
    record
}

#[test]
fn combat_motion_timeline_steps_and_mot1_roundtrips() {
    let mut bridge = OffloadBridge::new();
    assert_eq!(bridge.combat_motion_count(), 0);

    // Trigger at tick 10: the spec puts anticipation first.
    let phase = bridge
        .trigger_combat_action(ENTITY_PLAYER, &swing_chain(), 10, 1.0)
        .expect("trigger succeeds");
    assert_eq!(phase, ActionPhase::Anticipation);
    assert_eq!(bridge.combat_motion_count(), 1);

    // Advance to the first active-window tick (elapsed 2 of 7).
    let active = bridge.step_combat_motions(12).expect("step succeeds");
    assert_eq!(active, 1);
    let action = bridge.combat_motion(ENTITY_PLAYER).expect("live action");
    assert_eq!(action.phase(), ActionPhase::ActiveHitWindow);
    assert_eq!(action.elapsed_ticks(), 2);
    assert_eq!(action.hand(), Hand::MainHand);

    // MOT1 snapshot: encode through the bridge, decode through the
    // re-exported codec — every field round-trips.
    let trigger = bridge
        .combat_motion_trigger(ENTITY_PLAYER)
        .expect("trigger snapshot");
    assert_eq!(trigger.entity_id, ENTITY_PLAYER);
    assert_eq!(trigger.action_id, 7);
    assert_eq!(trigger.phase, ActionPhase::ActiveHitWindow);

    let bytes = bridge
        .encode_combat_motion(ENTITY_PLAYER)
        .expect("MOT1 encodes");
    assert_eq!(bytes.len(), MOT1_PACKET_LEN);
    assert_eq!(&bytes[..4], &MOT1_MAGIC);
    let decoded = decode_motion_trigger(&bytes).expect("MOT1 decodes");
    assert_eq!(decoded.entity_id, ENTITY_PLAYER);
    assert_eq!(decoded.action_id, 7);
    assert_eq!(decoded.hand, Hand::MainHand);
    assert_eq!(decoded.elapsed_ticks, 2);
    assert_eq!(decoded.phase, ActionPhase::ActiveHitWindow);

    // After the action's 7 ticks finish the phase idles; the ComboState
    // stays retained for the combo window until explicitly removed.
    assert_eq!(bridge.step_combat_motions(17).unwrap(), 0);
    assert_eq!(
        bridge.combat_motion(ENTITY_PLAYER).unwrap().phase(),
        ActionPhase::Idle
    );
    assert!(bridge.remove_combat_motion(ENTITY_PLAYER));
    assert!(!bridge.remove_combat_motion(ENTITY_PLAYER));
    assert!(bridge.combat_motion(ENTITY_PLAYER).is_none());
    assert!(bridge.encode_combat_motion(ENTITY_PLAYER).is_none());
}

#[test]
fn combat_hit_validation_latches_once_per_swing() {
    let mut bridge = OffloadBridge::new();
    bridge
        .trigger_combat_action(ENTITY_PLAYER, &swing_chain(), 10, 1.0)
        .unwrap();
    assert_eq!(
        bridge.validate_combat_hit(
            ENTITY_PLAYER,
            MotionVec3::new(0.0, 0.0, 0.0),
            MotionVec3::new(1.0, 0.0, 0.0),
            HitCandidate {
                target_id: ENTITY_TARGET,
                position: MotionVec3::new(1.5, 0.0, 0.0),
            },
            None,
        ),
        HitValidationVerdict::OutsideActiveWindow,
        "anticipation is not a hit window"
    );

    bridge.step_combat_motions(12).unwrap();
    let verdict = bridge.validate_combat_hit(
        ENTITY_PLAYER,
        MotionVec3::new(0.0, 0.0, 0.0),
        MotionVec3::new(1.0, 0.0, 0.0),
        HitCandidate {
            target_id: ENTITY_TARGET,
            position: MotionVec3::new(1.5, 0.0, 0.0),
        },
        None,
    );
    assert_eq!(verdict, HitValidationVerdict::Accepted);
    // Same swing, same target: the latch refuses the double-hit.
    assert_eq!(
        bridge.validate_combat_hit(
            ENTITY_PLAYER,
            MotionVec3::new(0.0, 0.0, 0.0),
            MotionVec3::new(1.0, 0.0, 0.0),
            HitCandidate {
                target_id: ENTITY_TARGET,
                position: MotionVec3::new(1.5, 0.0, 0.0),
            },
            None,
        ),
        HitValidationVerdict::AlreadyHit
    );
    // An untracked attacker has no active window at all.
    assert_eq!(
        bridge.validate_combat_hit(
            0xDEAD,
            MotionVec3::new(0.0, 0.0, 0.0),
            MotionVec3::new(1.0, 0.0, 0.0),
            HitCandidate {
                target_id: ENTITY_TARGET,
                position: MotionVec3::new(1.0, 0.0, 0.0),
            },
            None,
        ),
        HitValidationVerdict::OutsideActiveWindow
    );
}

#[test]
fn step_tick_advances_combat_motions_and_reports_activity() {
    let mut bridge = OffloadBridge::new();
    bridge
        .trigger_combat_action(ENTITY_PLAYER, &swing_chain(), 1, 1.0)
        .unwrap();

    let report = bridge
        .step_tick(2, OffloadBudget::new(64))
        .expect("tick steps");
    assert_eq!(report.combat_motions_active, 1);

    // The action lasts until tick 8; by tick 9 everything is idle again.
    let report = bridge.step_tick(9, OffloadBudget::new(64)).unwrap();
    assert_eq!(report.combat_motions_active, 0);
}

#[test]
fn party_storage_registers_encodes_decodes_and_installs() {
    let mut bridge = OffloadBridge::new();
    assert_eq!(bridge.companion_storage_count(), 0);
    assert_eq!(bridge.encode_companion_storage(&OWNER), None);

    let mut storage = PlayerCompanionStorage::new();
    storage
        .add_captured(companion(0x01, OWNER))
        .expect("first capture lands in the party");
    storage
        .add_captured(companion(0x02, OWNER))
        .expect("second capture lands in the party");
    bridge
        .register_companion_storage(OWNER, storage)
        .expect("registration succeeds");
    assert_eq!(bridge.companion_storage_count(), 1);

    let stored = bridge.companion_storage(&OWNER).expect("storage present");
    assert_eq!(stored.total_occupied(), 2);
    assert_eq!(stored.party().iter().flatten().count(), 2);
    assert!(matches!(
        stored.occupied_slots().first(),
        Some(StorageSlotRef::Party(0))
    ));

    // PRTY round-trip through the bridge's canonical codec.
    let bytes = bridge
        .encode_companion_storage(&OWNER)
        .expect("PRTY encodes");
    assert_eq!(&bytes[..4], b"PRTY");
    bridge
        .install_companion_storage(OTHER_OWNER, &bytes)
        .expect("clean PRTY installs");
    let restored = bridge.companion_storage(&OTHER_OWNER).expect("restored");
    assert_eq!(restored.total_occupied(), 2);
    assert_eq!(
        restored.get(StorageSlotRef::Party(0)).unwrap().species_id,
        "mymod:ember_cub"
    );

    // Corrupt/truncated bytes install nothing and the prior state
    // survives — the decode fails closed before the registry is touched.
    let mut corrupt = bytes.clone();
    corrupt[0] ^= 0xFF; // PRTY magic
    assert!(matches!(
        bridge.install_companion_storage(OTHER_OWNER, &corrupt),
        Err(OffloadError::CompanionStorageDecode(_))
    ));
    assert!(matches!(
        bridge.install_companion_storage(OTHER_OWNER, &bytes[..10]),
        Err(OffloadError::CompanionStorageDecode(_))
    ));
    assert_eq!(
        bridge
            .companion_storage(&OTHER_OWNER)
            .unwrap()
            .total_occupied(),
        2
    );

    // take_ hands the storage back out; encode then reports absent.
    let taken = bridge.take_companion_storage(&OTHER_OWNER).expect("taken");
    assert_eq!(taken.total_occupied(), 2);
    assert_eq!(bridge.encode_companion_storage(&OTHER_OWNER), None);
    assert_eq!(bridge.companion_storage_count(), 1);
}

#[test]
fn empty_motion_and_storage_surfaces_cost_nothing() {
    let fresh = OffloadBridge::new();
    assert_eq!(fresh.combat_motion_count(), 0);
    assert_eq!(fresh.companion_storage_count(), 0);
    // Zero-cost contract: an empty bridge's digest is reproducible and
    // identical to another empty bridge.
    assert_eq!(fresh.state_digest(), OffloadBridge::new().state_digest());
}

#[test]
fn digest_folds_motion_and_storage_deterministically() {
    let build = || {
        let mut bridge = OffloadBridge::new();
        bridge
            .trigger_combat_action(ENTITY_PLAYER, &swing_chain(), 10, 1.0)
            .unwrap();
        let mut storage = PlayerCompanionStorage::new();
        storage.add_captured(companion(0x01, OWNER)).unwrap();
        bridge.register_companion_storage(OWNER, storage).unwrap();
        bridge
    };
    let a = build();
    let b = build();
    let empty = OffloadBridge::new();
    assert_eq!(
        a.state_digest(),
        b.state_digest(),
        "identical motion + storage state fingerprints identically"
    );
    assert_ne!(
        a.state_digest(),
        empty.state_digest(),
        "tracked state must reach the digest"
    );

    // Mutating either surface moves the fingerprint.
    let mut c = build();
    c.step_combat_motions(12).unwrap();
    assert_ne!(a.state_digest(), c.state_digest());
    let mut d = build();
    d.companion_storage_mut(&OWNER)
        .unwrap()
        .add_captured(companion(0x03, OWNER))
        .unwrap();
    assert_ne!(a.state_digest(), d.state_digest());
}
