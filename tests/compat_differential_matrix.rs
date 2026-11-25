// SPDX-License-Identifier: LGPL-3.0-or-later
//! Cross-feature compatibility checks for the root release binary's shared core.

use megu_core::anticheat_v2::{
    BehavioralCusumDetector, BehavioralFlag, HitboxRewindBuffer, PhysicsEnv,
    PhysicsPredictionStep, ProtocolInvariantFsm, ProtocolViolation, TransactionEchoQueue, TxKind,
};
use megu_core::break_progress::{destroy_progress_per_tick, instant_break, ticks_to_break};
use megu_core::bvh::Aabb3;
use megu_core::coords::{ChunkKey, DimIndex};
use megu_core::dst_oracle::run_dst_campaign;
use megu_core::morton::{morton2_decode_u32, morton2_encode_i16};
use megu_core::net_budget::EncodeOnceFrameCache;
use megu_core::quiescent::{ActivityMask, QuiescentChunkScheduler};
use megu_core::section_rev::SharedSectionRevTable;
use std::sync::Arc;
use std::thread;

#[test]
fn break_progress_is_shared_across_tool_families_and_exact_float_boundaries() {
    let cases = [
        ("pickaxe", 8.0, 1.5, true),
        ("shovel", 6.0, 0.5, true),
        ("axe", 11.2, 2.0, true),
        ("hoe", 3.0, 0.5, true),
        ("shears", 5.0, 0.8, true),
        ("sword", 1.5, 0.5, true),
        ("Create drill", 60.0, 2.0, true),
    ];
    for (family, speed, hardness, harvest) in cases {
        let damage = destroy_progress_per_tick(speed, hardness, harvest);
        assert!(damage.is_finite() && damage > 0.0, "{family}");
        assert_eq!(
            instant_break(speed, hardness, harvest),
            damage >= 1.0 - 1e-5,
            "{family} threshold"
        );
        if damage < 1.0 {
            assert_eq!(ticks_to_break(hardness, harvest, speed), (1.0 / damage).ceil() as u32);
        }
    }
    assert_eq!(destroy_progress_per_tick(45.0, 1.5, true), 1.0);
    assert!(instant_break(45.0, 1.5, true));
    assert!(instant_break(f32::from_bits(45.0f32.to_bits() - 1), 1.5, true));
    assert!(!instant_break(45.0, 1.5, false));
}

#[test]
fn anti_cheat_physics_rtt_rewind_cusum_and_protocol_invariants_are_deterministic() {
    let env = PhysicsEnv::default();
    let envelope = PhysicsPredictionStep::envelope(&env);
    assert_eq!(envelope, PhysicsPredictionStep::envelope(&env));
    assert!(PhysicsPredictionStep::evaluate(&env, [0.2, 0.2, 0.2], None).within);
    assert!(!PhysicsPredictionStep::evaluate(&env, [envelope[0] + 2.0, 0.0, 0.0], None).within);

    let mut tx = TransactionEchoQueue::<8>::new();
    let first = tx.issue(TxKind::VelocityGrant, 100);
    let second = tx.issue(TxKind::Teleport, 120);
    assert_eq!(tx.note_echo(second, 180), Some(60));
    assert_eq!(tx.pending(), 0);
    assert_eq!(tx.rtt().samples, 1);
    assert_eq!(tx.note_echo(first, 190), None);

    let bounds = Aabb3::new([-0.5, 0.0, 2.0], [0.5, 2.0, 3.0]);
    let mut rewind = HitboxRewindBuffer::<8>::new();
    rewind.record(10, bounds);
    assert!(rewind.verify_ray_reach([0.0, 1.0, 0.0], [0.0, 0.0, 1.0], 4.0, 10, 0.0).is_some());
    assert!(rewind.verify_ray_reach([5.0, 1.0, 0.0], [0.0, 0.0, 1.0], 4.0, 10, 0.0).is_none());

    let mut bot = BehavioralCusumDetector::new();
    let mut bot_flag = None;
    for _ in 0..100 {
        bot_flag = bot.note_click_interval(0.1).or(bot_flag);
    }
    assert_eq!(bot_flag, Some(BehavioralFlag::AutoClicker));
    let mut human = BehavioralCusumDetector::new();
    for dt in [0.08, 0.13, 0.10, 0.17, 0.09, 0.14, 0.11, 0.19, 0.07, 0.15, 0.12, 0.18] {
        human.note_click_interval(dt);
    }
    assert_eq!(human.suspicion(), 0.0);

    let mut fsm = ProtocolInvariantFsm::new();
    assert_eq!(fsm.note_dig_status(2, false), Err(ProtocolViolation::DigFinishWithoutStart));
    assert_eq!(fsm.note_dig_status(0, false), Ok(()));
    assert_eq!(fsm.note_dig_status(2, false), Ok(()));
    assert_eq!(fsm.violations(), 1);
}

#[test]
fn deterministic_campaigns_match_across_worker_counts_and_seeds() {
    let seeds = [0, 1, 42, 0x5eed];
    for seed in seeds {
        let serial = run_dst_campaign(seed, 1_000).expect("serial DST campaign");
        assert_eq!(serial.merkle_roots_matched, serial.restarts_verified);
        let workers: Vec<_> = (0..4)
            .map(|_| thread::spawn(move || run_dst_campaign(seed, 1_000).expect("parallel DST campaign")))
            .collect();
        for worker in workers {
            assert_eq!(serial, worker.join().expect("worker thread"));
        }
    }
}

#[test]
fn idle_chunks_are_absent_and_chunk_frames_are_encoded_once_until_revision_changes() {
    let key = ChunkKey::from_block_pos(DimIndex::OVERWORLD, -1, 32);
    let mut scheduler = QuiescentChunkScheduler::new();
    for _ in 0..10_000 {
        assert!(!scheduler.is_active(key));
        assert_eq!(scheduler.active_len(), 0);
    }
    scheduler.wake(key, ActivityMask::BLOCK_ENTITY);
    assert!(scheduler.is_active(key));
    assert_eq!(scheduler.clear_bits(key, ActivityMask::BLOCK_ENTITY), ActivityMask::NONE);
    assert!(!scheduler.is_active(key));

    let revs = SharedSectionRevTable::new();
    let mut cache = EncodeOnceFrameCache::<4>::new();
    let encoded: Arc<[u8]> = Arc::from([1_u8, 2, 3, 4]);
    let stored = cache.store_fresh(key, &revs, Arc::clone(&encoded));
    let hit = cache.get(key, &revs).expect("fresh frame cache hit");
    assert!(Arc::ptr_eq(&stored.bytes, &hit.bytes));
    assert_eq!(cache.hits(), 1);
    assert_eq!(cache.misses(), 0);
}

#[test]
fn signed_chunk_coordinates_round_trip_through_morton_order() {
    for cx in [i16::MIN, -30_000, -1, 0, 1, 30_000, i16::MAX] {
        for cz in [i16::MIN, -17, -1, 0, 1, 17, i16::MAX] {
            assert_eq!(morton2_decode_u32(morton2_encode_i16(cx, cz)), (cx, cz));
        }
    }
}

#[cfg(feature = "create-offload")]
#[test]
fn release_feature_adapters_expose_the_expected_create_and_generic_planners() {
    use create_adapter::factorio_sim::{KineticKind, KineticNetworkPool};
    use mod_adapter_core::mine_cut_all::{MineCutAllConfig, MineCutAllWorldData};

    let mut network = KineticNetworkPool::new();
    let source = network.add_node(KineticKind::SteamEngine);
    assert!(network.status(source).is_some());

    let config = MineCutAllConfig::default();
    assert!(config.effective_max_blocks() > 0);
    let world = MineCutAllWorldData::new();
    assert_eq!(world.blocks.len(), 0);
}
