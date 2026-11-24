// SPDX-License-Identifier: LGPL-3.0-or-later
//! Unit tests for `megu_core::server_perf_governors`.

use std::cell::Cell;
use std::sync::Arc;

use megu_core::server_perf_governors::{
    ChunkPacketCacheKey, CompressedChunkPacketCache, CrowdTickGuard, EntityTickClass, TickDecision,
    VelocityViewGovernor, DEFAULT_CROWD_THRESHOLD, HIGH_SPEED_THRESHOLD, MAX_VIEW_DISTANCE,
    MIN_VIEW_DISTANCE,
};

// ---------------------------------------------------------------------------
// VelocityViewGovernor
// ---------------------------------------------------------------------------

#[test]
fn stationary_plan_emits_the_full_symmetric_square() {
    let gov = VelocityViewGovernor::new(10);
    let plan = gov.plan_chunk_sends(100, -40, 0.0, 0.0, 1.0, usize::MAX);
    // Chebyshev square of radius 10: (2*10+1)^2 = 441 chunks.
    assert_eq!(plan.len(), 21 * 21);
    // Every edge and corner of the square is present.
    for &(dx, dz) in &[(10, 0), (-10, 0), (0, 10), (0, -10), (10, 10), (-10, -10)] {
        assert!(plan.contains(&(100 + dx, -40 + dz)), "missing {dx},{dz}");
    }
    // Outside the square is absent.
    assert!(!plan.contains(&(111, -40)));
    assert!(!plan.contains(&(100, -51)));
    // Nearest first: the player's own chunk leads the plan.
    assert_eq!(plan[0], (100, -40));
}

#[test]
fn effective_radius_tracks_load_scale_and_clamps() {
    let gov = VelocityViewGovernor::new(10);
    assert_eq!(gov.effective_radius(1.0), 10);
    assert_eq!(gov.effective_radius(0.5), 5);
    // 10 * 0.25 = 2.5 -> rounds (half away from zero) to 3.
    assert_eq!(gov.effective_radius(0.25), 3);
    // A small view distance clamps the scaled radius at the 2-chunk floor.
    let tiny = VelocityViewGovernor::new(2);
    assert_eq!(tiny.effective_radius(0.25), 2);
    // Non-finite scale fails closed to the full distance, never under-sends.
    assert_eq!(gov.effective_radius(f32::NAN), 10);
    // View distance itself is clamped into 2..=32.
    assert_eq!(
        VelocityViewGovernor::new(0).view_distance(),
        MIN_VIEW_DISTANCE
    );
    assert_eq!(
        VelocityViewGovernor::new(200).view_distance(),
        MAX_VIEW_DISTANCE
    );
}

#[test]
fn elytra_flight_keeps_full_forward_reach_and_prunes_the_rear() {
    let gov = VelocityViewGovernor::new(12);
    // Elytra + rocket along +x at 1.8 blocks/tick (way above 0.75).
    let cone = gov.plan_chunk_sends(0, 0, 1.8, 0.0, 1.0, usize::MAX);
    let square = gov.plan_chunk_sends(0, 0, 0.0, 0.0, 1.0, usize::MAX);

    // Forward reach is the full view distance: +12 dead ahead survives.
    assert!(
        cone.contains(&(12, 0)),
        "forward edge must never pop in late"
    );
    // Rear reach is pruned to max(12/2, 3) = 6: -6 survives, -7 and -12 do not.
    assert!(cone.contains(&(-6, 0)));
    assert!(!cone.contains(&(-7, 0)));
    assert!(!cone.contains(&(-12, 0)));

    // The cone sheds at least 40% of the symmetric square's chunk count.
    assert!(
        cone.len() * 5 <= square.len() * 3,
        "cone {} should be <= 60% of square {}",
        cone.len(),
        square.len()
    );

    // Dead-ahead ordering: every +x-row chunk precedes every rear chunk,
    // and nearer forward chunks come before farther ones.
    let pos = |c: (i32, i32)| cone.iter().position(|&p| p == c).unwrap();
    for k in 1..=12 {
        assert!(pos((k, 0)) < pos((-1, 0)), "ahead +{k} must beat rear");
    }
    for k in 1..12 {
        assert!(pos((k, 0)) < pos((k + 1, 0)), "nearer forward first");
    }
    // Own chunk first.
    assert_eq!(cone[0], (0, 0));

    // Side chunks interpolate: directly abeam (alignment 0) reaches past
    // the rear radius but short of full view distance.
    assert!(cone.contains(&(0, 6)));
    assert!(!cone.contains(&(0, 12)));
}

#[test]
fn cone_plan_respects_the_per_tick_budget_cap() {
    let gov = VelocityViewGovernor::new(12);
    let plan = gov.plan_chunk_sends(0, 0, 1.8, 0.0, 1.0, 64);
    assert_eq!(plan.len(), 64);
    assert_eq!(gov.plan_chunk_sends(0, 0, 1.8, 0.0, 1.0, 0).len(), 0);
    // The capped cone keeps the best-ranked chunks: forward row survives.
    for k in 1..=12 {
        assert!(plan.contains(&(k, 0)));
    }
}

#[test]
fn non_finite_velocity_fails_closed_to_the_symmetric_plan() {
    let gov = VelocityViewGovernor::new(8);
    let reference = gov.plan_chunk_sends(7, 9, 0.2, 0.1, 1.0, usize::MAX);
    for &(vx, vz) in &[
        (f32::NAN, 0.0),
        (0.0, f32::NAN),
        (f32::INFINITY, 0.0),
        (0.0, f32::NEG_INFINITY),
        (f32::INFINITY, f32::INFINITY),
    ] {
        let plan = gov.plan_chunk_sends(7, 9, vx, vz, 1.0, usize::MAX);
        assert_eq!(plan, reference, "velocity ({vx}, {vz}) must fail closed");
    }
}

#[test]
fn directional_radius_matches_the_spec_envelope() {
    let gov = VelocityViewGovernor::new(12);
    // Full reach inside the forward hemisphere.
    assert_eq!(gov.directional_radius(1.0), 12);
    assert_eq!(gov.directional_radius(0.15), 12);
    // Rear band prunes to max(12/2, 3) = 6.
    assert_eq!(gov.directional_radius(-0.21), 6);
    assert_eq!(gov.directional_radius(-1.0), 6);
    // Interpolation band stays between rear and full reach, monotonically.
    let mid = gov.directional_radius(0.0);
    assert!((6..12).contains(&mid));
    assert!(gov.directional_radius(-0.1) <= gov.directional_radius(0.1));
    // Non-finite alignment fails closed to the minimum reach.
    assert_eq!(gov.directional_radius(f32::NAN), 6);
    // The rear floor keeps at least 3 chunks of context behind the player.
    let close = VelocityViewGovernor::new(4);
    assert_eq!(close.directional_radius(-1.0), 3);
}

#[test]
fn sub_threshold_speed_stays_symmetric() {
    let gov = VelocityViewGovernor::new(8);
    // Sprint-jump speed (~0.28) is well under the 0.75 cone threshold.
    let slow = gov.plan_chunk_sends(0, 0, 0.28, 0.0, 1.0, usize::MAX);
    assert_eq!(slow.len(), 17 * 17);
    assert!(slow.contains(&(-8, 0)), "rear chunks remain at low speed");
    // Confirm the threshold constant gates the mode switch.
    let just_under = gov.plan_chunk_sends(0, 0, HIGH_SPEED_THRESHOLD - 0.001, 0.0, 1.0, usize::MAX);
    let just_over = gov.plan_chunk_sends(0, 0, HIGH_SPEED_THRESHOLD, 0.0, 1.0, usize::MAX);
    assert_eq!(just_under.len(), 17 * 17);
    assert!(just_over.len() < just_under.len());
}

// ---------------------------------------------------------------------------
// CrowdTickGuard
// ---------------------------------------------------------------------------

#[test]
fn critical_entities_always_run_full_at_any_density() {
    let mut guard = CrowdTickGuard::default();
    guard.set_chunk_entity_count(0, 0, 500);
    for eid in -3..20 {
        for tick in 0..20_u64 {
            assert_eq!(
                guard.decide(EntityTickClass::Critical, eid, 0, 0, tick),
                TickDecision::RunFull,
                "Critical eid {eid} tick {tick} must never throttle"
            );
        }
    }
    // Even an untracked (density 0) or off-map chunk is irrelevant.
    assert_eq!(
        guard.decide(EntityTickClass::Critical, 7, 999, 999, 123),
        TickDecision::RunFull
    );
}

#[test]
fn crowd_mobs_run_full_at_or_below_the_threshold() {
    let mut guard = CrowdTickGuard::new(DEFAULT_CROWD_THRESHOLD);
    guard.set_chunk_entity_count(1, 1, DEFAULT_CROWD_THRESHOLD);
    for eid in 0..10 {
        for tick in 0..10_u64 {
            assert_eq!(
                guard.decide(EntityTickClass::CrowdMob, eid, 1, 1, tick),
                TickDecision::RunFull
            );
        }
    }
    // Untracked chunks read as density 0: also full.
    assert_eq!(
        guard.decide(EntityTickClass::CrowdMob, 0, 5, 5, 0),
        TickDecision::RunFull
    );
}

#[test]
fn crowded_mobs_stride_idle_ai_but_never_physics() {
    let mut guard = CrowdTickGuard::default();
    guard.set_chunk_entity_count(0, 0, 80); // >64 -> prime stride 5
    assert_eq!(CrowdTickGuard::idle_ai_stride(80), 5);

    // For any entity, exactly one tick in every window of five runs idle
    // AI, and physics/damage run on every tick.
    for eid in 0..25 {
        let decisions: Vec<TickDecision> = (0..5_u64)
            .map(|tick| guard.decide(EntityTickClass::CrowdMob, eid, 0, 0, tick))
            .collect();
        assert!(
            decisions.iter().all(|d| d.run_physics_and_damage()),
            "physics/damage must run every tick for eid {eid}"
        );
        let full = decisions.iter().filter(|d| d.run_idle_ai()).count();
        assert_eq!(
            full, 1,
            "eid {eid} must run idle AI exactly once per 5 ticks"
        );
    }

    // Even spread across a cohort at one instant: 10 consecutive ids give
    // exactly two full ticks (phase (eid + tick) % 5 == 0 -> eid 0 and 5).
    let cohort_full = (0..10)
        .filter(|eid| {
            guard
                .decide(EntityTickClass::CrowdMob, *eid, 0, 0, 0)
                .run_idle_ai()
        })
        .count();
    assert_eq!(cohort_full, 2);

    // Deterministic: identical inputs give identical decisions.
    let a = guard.decide(EntityTickClass::CrowdMob, 42, 0, 0, 7);
    let b = guard.decide(EntityTickClass::CrowdMob, 42, 0, 0, 7);
    assert_eq!(a, b);
}

#[test]
fn stride_tiers_match_the_prime_ladder() {
    assert_eq!(CrowdTickGuard::idle_ai_stride(17), 2);
    assert_eq!(CrowdTickGuard::idle_ai_stride(32), 2);
    assert_eq!(CrowdTickGuard::idle_ai_stride(33), 3);
    assert_eq!(CrowdTickGuard::idle_ai_stride(64), 3);
    assert_eq!(CrowdTickGuard::idle_ai_stride(65), 5);
    assert_eq!(CrowdTickGuard::idle_ai_stride(10_000), 5);
}

#[test]
fn stride_boundary_and_density_tracking() {
    let mut guard = CrowdTickGuard::default();
    guard.set_chunk_entity_count(2, 2, 16);
    // Density == threshold: no striding.
    assert_eq!(
        guard.decide(EntityTickClass::CrowdMob, 1, 2, 2, 0),
        TickDecision::RunFull
    );
    // Crossing to 17 starts stride-2: eid 1 at tick 0 -> (1+0) % 2 != 0.
    guard.set_chunk_entity_count(2, 2, 17);
    assert_eq!(
        guard.decide(EntityTickClass::CrowdMob, 1, 2, 2, 0),
        TickDecision::PhysicsOnly
    );
    assert_eq!(
        guard.decide(EntityTickClass::CrowdMob, 2, 2, 2, 0),
        TickDecision::RunFull
    );
    // Density bookkeeping: update, read, remove, clear.
    assert_eq!(guard.chunk_entity_count(2, 2), 17);
    assert_eq!(guard.tracked_chunk_count(), 1);
    assert!(guard.remove_chunk(2, 2));
    assert_eq!(guard.chunk_entity_count(2, 2), 0);
    guard.set_chunk_entity_count(9, 9, 99);
    guard.clear();
    assert_eq!(guard.tracked_chunk_count(), 0);
}

#[test]
fn negative_entity_ids_stride_deterministically() {
    let mut guard = CrowdTickGuard::default();
    guard.set_chunk_entity_count(0, 0, 80); // stride 5
                                            // |eid| is the phase: eid -10 -> phase 10 -> (10 + 0) % 5 == 0.
    assert_eq!(
        guard.decide(EntityTickClass::CrowdMob, -10, 0, 0, 0),
        TickDecision::RunFull
    );
    assert_eq!(
        guard.decide(EntityTickClass::CrowdMob, -9, 0, 0, 0),
        TickDecision::PhysicsOnly
    );
    // i64::MIN must not panic and must be deterministic (wrapping_abs).
    let min_a = guard.decide(EntityTickClass::CrowdMob, i64::MIN, 0, 0, 0);
    let min_b = guard.decide(EntityTickClass::CrowdMob, i64::MIN, 0, 0, 0);
    assert_eq!(min_a, min_b);
}

// ---------------------------------------------------------------------------
// CompressedChunkPacketCache
// ---------------------------------------------------------------------------

fn key(dim: i32, cx: i32, cz: i32, epoch: u64, profile: u8) -> ChunkPacketCacheKey {
    ChunkPacketCacheKey {
        dim,
        cx,
        cz,
        dirty_epoch: epoch,
        antixray_profile: profile,
    }
}

#[test]
fn fifty_players_share_one_encode_of_the_same_chunk() {
    let mut cache = CompressedChunkPacketCache::new(4096);
    let encodes = Cell::new(0_u32);
    let k = key(0, 5, -3, 1, 0);

    let mut packets = Vec::new();
    for _player in 0..50 {
        packets.push(cache.get_or_encode(k, || {
            encodes.set(encodes.get() + 1);
            vec![0x27; 128]
        }));
    }

    // Serialized + compressed exactly once; 49 viewers shared the Arc.
    assert_eq!(encodes.get(), 1);
    assert_eq!(cache.misses(), 1);
    assert_eq!(cache.hits(), 49);
    for p in &packets[1..] {
        assert!(Arc::ptr_eq(&packets[0], p), "every viewer shares one Arc");
    }
    assert_eq!(packets[0].len(), 128);
    assert!(packets[0].iter().all(|&b| b == 0x27));
    // Sharing saved 49 avoided serializations worth of payload bytes.
    assert_eq!(cache.bytes_saved_by_sharing(), 49 * 128);
}

#[test]
fn bumping_dirty_epoch_forces_a_fresh_encode() {
    let mut cache = CompressedChunkPacketCache::new(4096);
    let encodes = Cell::new(0_u32);
    let encode = |tag: u8| {
        encodes.set(encodes.get() + 1);
        vec![tag; 64]
    };

    let epoch1 = key(0, 1, 1, 1, 0);
    let epoch2 = key(0, 1, 1, 2, 0);

    let first = cache.get_or_encode(epoch1, || encode(0x11));
    let second = cache.get_or_encode(epoch1, || encode(0x22));
    assert!(Arc::ptr_eq(&first, &second));
    assert_eq!(encodes.get(), 1);

    // Same chunk, new epoch: a different key, so a fresh encode happens.
    let third = cache.get_or_encode(epoch2, || encode(0x33));
    assert!(!Arc::ptr_eq(&first, &third));
    assert_eq!(encodes.get(), 2);
    assert_eq!(cache.misses(), 2);
    assert_eq!(cache.hits(), 1);
    assert_eq!(third.len(), 64);
    assert!(third.iter().all(|&b| b == 0x33));

    // Distinct anti-xray profiles never share bytes either.
    let profile1 = cache.get_or_encode(key(0, 1, 1, 2, 1), || encode(0x44));
    assert_eq!(encodes.get(), 3);
    assert!(profile1.iter().all(|&b| b == 0x44));
}

#[test]
fn lru_eviction_caps_resident_entries_at_max() {
    let mut cache = CompressedChunkPacketCache::new(4);
    for i in 0..4 {
        let _ = cache.get_or_encode(key(0, i, 0, 1, 0), || vec![i as u8]);
    }
    assert_eq!(cache.len(), 4);

    // Touch chunk 0 so chunk 1 becomes the coldest.
    let hit0 = cache.get_or_encode(key(0, 0, 0, 1, 0), || vec![0xEE]);
    assert!(hit0.iter().all(|&b| b == 0));

    // Inserting a fifth entry evicts chunk 1, not the just-touched chunk 0.
    let _ = cache.get_or_encode(key(0, 9, 0, 1, 0), || vec![9]);
    assert_eq!(cache.len(), 4);
    assert!(cache.get(key(0, 1, 0, 1, 0)).is_none(), "coldest evicted");
    assert!(cache.get(key(0, 0, 0, 1, 0)).is_some(), "hot entry kept");
    assert!(cache.get(key(0, 2, 0, 1, 0)).is_some());
    assert!(cache.get(key(0, 9, 0, 1, 0)).is_some());

    // Flooding with many more keys still never exceeds the cap.
    for i in 10..40 {
        let _ = cache.get_or_encode(key(0, i, 0, 1, 0), || vec![i as u8]);
        assert!(cache.len() <= 4);
    }
}

#[test]
fn invalidate_chunk_drops_every_epoch_and_profile() {
    let mut cache = CompressedChunkPacketCache::new(16);
    for epoch in 1..=3 {
        for profile in 0..2 {
            let _ = cache.get_or_encode(key(0, 4, 4, epoch, profile), || vec![1]);
        }
    }
    let _ = cache.get_or_encode(key(1, 4, 4, 1, 0), || vec![2]); // other dim
    let _ = cache.get_or_encode(key(0, 5, 4, 1, 0), || vec![3]); // other chunk
    assert_eq!(cache.len(), 8);

    // One call removes all six variants of the target chunk only.
    assert_eq!(cache.invalidate_chunk(0, 4, 4), 6);
    assert_eq!(cache.len(), 2);
    assert!(cache.get(key(0, 4, 4, 2, 0)).is_none());
    assert!(cache.get(key(1, 4, 4, 1, 0)).is_some());
    assert!(cache.get(key(0, 5, 4, 1, 0)).is_some());
    // Idempotent on an already-absent chunk.
    assert_eq!(cache.invalidate_chunk(0, 4, 4), 0);
}

#[test]
fn zero_capacity_cache_still_encodes_but_retains_nothing() {
    let mut cache = CompressedChunkPacketCache::new(0);
    let encodes = Cell::new(0_u32);
    let k = key(0, 0, 0, 1, 0);
    for _ in 0..3 {
        let p = cache.get_or_encode(k, || {
            encodes.set(encodes.get() + 1);
            vec![7]
        });
        assert!(p.iter().all(|&b| b == 7));
    }
    assert_eq!(encodes.get(), 3);
    assert_eq!(cache.misses(), 3);
    assert!(cache.is_empty());
    assert_eq!(cache.len(), 0);
    assert_eq!(cache.max_entries(), 0);
}
