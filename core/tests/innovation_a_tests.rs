// SPDX-License-Identifier: LGPL-3.0-or-later

use std::sync::Arc;

use megu_core::bounded::{BoundedMap, DimBound, LootSeedToken, MustConsume, SpawnerDrawToken};
use megu_core::coords::{ChunkKey, DimIndex};
use megu_core::local_transport::{local_pair, TrySendError};
use megu_core::net_budget::{
    classify_perceptual_band, should_emit_tick, EncodeOnceFrameCache, PerceptualBand,
};
use megu_core::quiescent::{ActivityMask, QuiescentChunkScheduler};
use megu_core::section_rev::SharedSectionRevTable;

const OW: DimIndex = DimIndex::OVERWORLD;

fn key(cx: i32, cz: i32) -> ChunkKey {
    ChunkKey {
        dim: OW,
        cx,
        cz,
    }
}

// ---------------------------------------------------------------------------
// QuiescentChunkScheduler
// ---------------------------------------------------------------------------

#[test]
fn quiescent_scheduler_visits_only_active_chunks() {
    let mut sched = QuiescentChunkScheduler::new();
    // 1,000 loaded chunk columns; only 50 have work. The other 950 must
    // cost O(0) — they are not stored, not iterated, not "skipped".
    for cx in 0..50i32 {
        sched.wake(key(cx, 0), ActivityMask::RANDOM_TICK);
    }
    assert_eq!(sched.active_len(), 50);
    assert_eq!(sched.active_chunks().len(), 50);
    assert_eq!(sched.active_chunks()[0].key, key(0, 0));
    assert_eq!(sched.active_chunks()[0].mask, ActivityMask::RANDOM_TICK);

    let ratio = sched.quiescent_ratio(1_000);
    assert!(
        (ratio - 0.95).abs() < f64::EPSILON,
        "50 active of 1000 loaded should read 95% quiescent, got {ratio}"
    );
    assert_eq!(sched.quiescent_ratio(0), 1.0);

    // An idle chunk never appears in the active slice.
    assert!(!sched.is_active(key(999, 999)));
    assert_eq!(sched.activity_of(key(999, 999)), ActivityMask::NONE);
}

#[test]
fn quiescent_clear_bits_physically_removes_idle_chunks() {
    let mut sched = QuiescentChunkScheduler::new();
    let k = key(3, 3);
    sched.wake(k, ActivityMask::FLUID | ActivityMask::REDSTONE);
    assert_eq!(
        sched.activity_of(k),
        ActivityMask::FLUID | ActivityMask::REDSTONE
    );

    // Partial clear keeps the chunk active with the remaining bit.
    assert_eq!(
        sched.clear_bits(k, ActivityMask::FLUID),
        ActivityMask::REDSTONE
    );
    assert!(sched.is_active(k));

    // Clearing the last bit swap-removes the entry entirely.
    assert_eq!(sched.clear_bits(k, ActivityMask::REDSTONE), ActivityMask::NONE);
    assert!(!sched.is_active(k));
    assert_eq!(sched.active_len(), 0);

    // Slot recycling: a re-woken chunk occupies a dense slot again.
    sched.wake(k, ActivityMask::MOB);
    assert!(sched.is_active(k));
    assert_eq!(sched.active_len(), 1);
}

#[test]
fn quiescent_border_mutation_wakes_neighbour_and_bumps_rev_table() {
    let mut sched = QuiescentChunkScheduler::new();
    let mut rev = SharedSectionRevTable::new();

    // Interior mutation wakes only the owning chunk.
    let (srev, crev) = sched.on_block_mutated(OW, 8, 64, 8, ActivityMask::FLUID, &mut rev);
    assert_eq!((srev, crev), (1, 1));
    assert_eq!(sched.active_len(), 1);
    assert!(sched.is_active(key(0, 0)));

    // Border mutation at (15, 64, 0): local x=15 leaks +X into chunk
    // (1, 0); local z=0 leaks -Z into chunk (0, -1); the corner also
    // touches (1, -1). The rev table must bump every touched section and
    // chunk column too.
    sched.clear();
    sched.on_block_mutated(OW, 15, 64, 0, ActivityMask::REDSTONE, &mut rev);
    assert!(sched.is_active(key(0, 0)), "owning chunk must wake");
    assert!(sched.is_active(key(1, 0)), "+X neighbour must wake");
    assert!(sched.is_active(key(0, -1)), "-Z neighbour must wake");
    assert!(sched.is_active(key(1, -1)), "corner chunk must wake");
    assert_eq!(sched.active_len(), 4);
    assert!(rev.chunk_rev(key(1, 0)) >= 1);
    assert!(rev.chunk_rev(key(0, -1)) >= 1);
    assert!(rev.chunk_rev(key(1, -1)) >= 1);
    // A chunk outside the mutation's footprint stays quiescent.
    assert!(!sched.is_active(key(-1, 0)));
}

#[test]
fn quiescent_delayed_wake_fires_on_exact_tick() {
    let mut sched = QuiescentChunkScheduler::new();
    let target = key(7, -2);
    let _handle = sched.wake_after(target, ActivityMask::BLOCK_ENTITY, 5);
    assert_eq!(sched.pending_delayed_wakes(), 1);
    assert!(!sched.is_active(target));

    let mut woken = Vec::new();
    for _ in 0..4 {
        assert_eq!(sched.advance_tick(&mut woken), 0);
    }
    assert!(woken.is_empty());
    assert!(!sched.is_active(target));

    // Fifth advance fires the timer exactly.
    assert_eq!(sched.advance_tick(&mut woken), 1);
    assert_eq!(woken, vec![target]);
    assert!(sched.is_active(target));
    assert_eq!(sched.activity_of(target), ActivityMask::BLOCK_ENTITY);
    assert_eq!(sched.pending_delayed_wakes(), 0);
    assert_eq!(sched.current_tick(), 5);

    // Bits accumulate across multiple delayed wakes for one key.
    let k2 = key(1, 1);
    sched.wake_after(k2, ActivityMask::FLUID, 2);
    sched.wake_after(k2, ActivityMask::MOB, 2);
    let mut woken = Vec::new();
    sched.advance_tick(&mut woken);
    sched.advance_tick(&mut woken);
    assert!(woken.contains(&k2));
    assert_eq!(
        sched.activity_of(k2),
        ActivityMask::FLUID | ActivityMask::MOB
    );
}

#[test]
fn quiescent_forget_chunk_retires_pending_wakes() {
    let mut sched = QuiescentChunkScheduler::new();
    let k = key(4, 4);
    sched.wake(k, ActivityMask::MOD_MACHINE);
    sched.wake_after(k, ActivityMask::FLUID, 3);
    sched.forget_chunk(k);
    assert!(!sched.is_active(k));
    assert_eq!(sched.active_len(), 0);

    // The in-flight timer retires as a no-op: nothing wakes, nothing is
    // reported.
    let mut woken = Vec::new();
    for _ in 0..5 {
        sched.advance_tick(&mut woken);
    }
    assert!(woken.is_empty());
    assert!(!sched.is_active(k));
}

// ---------------------------------------------------------------------------
// BoundedMap / tokens / DimBound
// ---------------------------------------------------------------------------

#[test]
fn bounded_map_never_exceeds_capacity_and_evicts_oldest() {
    let mut map: BoundedMap<u32, u64, 128> = BoundedMap::new();
    assert_eq!(map.capacity(), 128);
    assert!(map.is_empty());

    // 10,000 inserts: the cap is never exceeded and every full-map insert
    // reports the evicted pair.
    let mut evicted = 0u64;
    for i in 0..10_000u32 {
        if let Some((k, v)) = map.insert(i, u64::from(i) * 7) {
            evicted += 1;
            assert_eq!(u64::from(k) * 7, v);
        }
        assert!(map.len() <= 128);
    }
    assert_eq!(map.len(), 128);
    assert_eq!(evicted, 10_000 - 128);

    // Oldest eviction is deterministic: the surviving window is exactly
    // the last 128 keys written.
    for k in 0..(10_000 - 128) {
        assert!(!map.contains_key(&k), "key {k} should be evicted");
    }
    for k in (10_000 - 128)..10_000 {
        assert_eq!(map.get(&k), Some(&(u64::from(k) * 7)));
    }
    assert_eq!(map.oldest_key(), Some(&(10_000 - 128)));
    assert!(map.is_full());
}

#[test]
fn bounded_map_update_refresh_remove_and_retain() {
    let mut map: BoundedMap<u32, u32, 2> = BoundedMap::new();
    map.insert(1, 10);
    map.insert(2, 20);
    // Re-inserting an existing key refreshes its recency, so `1` survives
    // and `2` (now least-recently-written) is evicted.
    assert_eq!(map.insert(1, 11), None);
    assert_eq!(map.insert(3, 30), Some((2, 20)));
    assert!(map.contains_key(&1));
    assert!(!map.contains_key(&2));
    assert_eq!(map.get(&1), Some(&11));

    *map.get_mut(&1).unwrap() = 12;
    assert_eq!(map.remove(&1), Some(12));
    assert_eq!(map.remove(&1), None);

    map.insert(9, 90);
    map.retain(|_, v| *v > 50);
    assert!(map.contains_key(&9));
    assert!(!map.contains_key(&3));
    assert_eq!(map.len(), 1);
    map.clear();
    assert!(map.is_empty());

    // A zero-capacity map returns the inserted pair immediately.
    let mut zero: BoundedMap<u32, u32, 0> = BoundedMap::new();
    assert_eq!(zero.insert(1, 1), Some((1, 1)));
    assert_eq!(zero.len(), 0);
}

#[test]
fn consumption_tokens_stage_their_draws() {
    let loot = LootSeedToken::from_rng_draw(-123_456_789);
    assert_eq!(loot.stage(), -123_456_789);
    let spawner = SpawnerDrawToken::from_rng_draw(0xdead_beef);
    assert_eq!(spawner.stage(), 0xdead_beef);
    let generic = MustConsume::new(String::from("payload"));
    assert_eq!(generic.consume(), "payload");
}

#[test]
fn dim_bound_rejects_cross_dimension_unwrap() {
    let pos = DimBound::new(DimIndex::NETHER, (1_i32, 2_i32, 3_i32));
    assert_eq!(pos.into_inner_checked(DimIndex::NETHER), Some((1, 2, 3)));

    let pos = DimBound::new(DimIndex::NETHER, (1_i32, 2_i32, 3_i32));
    // The same coordinates must NOT unwrap in the overworld — that is
    // the structural guarantee.
    assert_eq!(pos.into_inner_checked(DimIndex::OVERWORLD), None);

    // map preserves the binding; modded dims behave identically.
    let bound = DimBound::new(DimIndex(7), 20_i32).map(|v| v * 2);
    assert_eq!(bound.dim, DimIndex(7));
    assert_eq!(bound.into_inner_checked(DimIndex(7)), Some(40));
    assert_eq!(DimBound::new(OW, 9).into_inner(), 9);
}

// ---------------------------------------------------------------------------
// local_transport
// ---------------------------------------------------------------------------

#[test]
fn local_transport_exchanges_50k_arcs_zero_copy_in_fifo_order() {
    const COUNT: usize = 50_000;
    let (server, client) = local_pair::<Vec<u8>, Vec<u8>>(1_024);

    let producer = std::thread::spawn(move || {
        let mut sent = Vec::with_capacity(COUNT);
        for i in 0..COUNT {
            let msg = Arc::new(vec![(i & 0xff) as u8, (i >> 8) as u8, (i >> 16) as u8]);
            sent.push(msg.clone());
            let mut msg = msg;
            loop {
                match server.try_send(msg) {
                    Ok(()) => break,
                    Err(TrySendError::Full(back)) => {
                        msg = back;
                        std::hint::spin_loop();
                    }
                    Err(TrySendError::Disconnected(_)) => panic!("client dropped early"),
                }
            }
        }
        (server, sent)
    });

    let consumer = std::thread::spawn(move || {
        let mut received = Vec::with_capacity(COUNT);
        while received.len() < COUNT {
            client.drain_into(&mut received);
        }
        (client, received)
    });

    let (server, sent) = producer.join().expect("producer panicked");
    let (client, received) = consumer.join().expect("consumer panicked");

    // Exact FIFO order and *pointer* identity — the received Arc is the
    // sender's allocation, proving zero serialization and zero copies.
    assert_eq!(received.len(), COUNT);
    for (i, (expected, got)) in sent.iter().zip(received.iter()).enumerate() {
        assert!(
            Arc::ptr_eq(expected, got),
            "message {i} arrived as a different allocation — the transport copied it"
        );
        assert_eq!(got[0], (i & 0xff) as u8);
        assert_eq!(got[1], (i >> 8) as u8);
        assert_eq!(got[2], (i >> 16) as u8);
    }
    assert_eq!(server.sent_count(), COUNT as u64);
    assert_eq!(client.received_count(), COUNT as u64);
    assert_eq!(
        server.bytes_saved_estimate(),
        COUNT as u64 * size_of::<Vec<u8>>() as u64
    );
}

#[test]
fn local_transport_backpressure_and_disconnect() {
    let (server, client) = local_pair::<u64, u64>(4);

    // Four payload slots: the fifth send reports Full with the message.
    for i in 0..4u64 {
        server.try_send(Arc::new(i)).expect("slot should be free");
    }
    match server.try_send(Arc::new(99)) {
        Err(TrySendError::Full(msg)) => assert_eq!(*msg, 99),
        other => panic!("expected Full backpressure, got {other:?}"),
    }
    assert_eq!(client.recv_len(), 4);

    // Receiving frees slots; FIFO order holds through the churn.
    assert_eq!(*client.try_recv().unwrap(), 0);
    server.try_send(Arc::new(4)).expect("freed slot");
    let mut drained = Vec::new();
    assert_eq!(client.drain_into(&mut drained), 4);
    let order: Vec<u64> = drained.iter().map(|a| **a).collect();
    assert_eq!(order, vec![1, 2, 3, 4]);

    // Dropping the peer flips sends to Disconnected immediately.
    drop(client);
    assert!(!server.peer_connected());
    match server.try_send(Arc::new(7)) {
        Err(TrySendError::Disconnected(msg)) => assert_eq!(*msg, 7),
        other => panic!("expected Disconnected, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// net_budget
// ---------------------------------------------------------------------------

#[test]
fn encode_once_cache_shares_one_arc_across_players() {
    const PLAYERS: usize = 50;
    let mut rev = SharedSectionRevTable::new();
    let mut cache: EncodeOnceFrameCache<256> = EncodeOnceFrameCache::new();
    let k = key(12, -30);

    rev.bump_block_local(OW, 12 * 16, 64, -30 * 16);
    let payload: Arc<[u8]> = (0..4096u32).map(|i| (i % 251) as u8).collect();

    // Player 1 misses and encodes; players 2..50 hit the one shared Arc.
    assert!(cache.get(k, &rev).is_none(), "absent frame misses");
    let frame = cache.store_fresh(k, &rev, payload.clone());
    assert!(frame.verify());
    assert!(Arc::ptr_eq(&frame.bytes, &payload));
    for _ in 1..PLAYERS {
        let hit = cache.get(k, &rev).expect("encode-once hit");
        assert!(
            Arc::ptr_eq(&hit.bytes, &frame.bytes),
            "every player must receive the same Arc allocation"
        );
    }
    assert_eq!(cache.hits(), (PLAYERS - 1) as u64);
    assert_eq!(cache.misses(), 1);

    // A single block edit bumps the canonical rev and instantly
    // invalidates the frame for all 50 players — one bump, no per-player
    // bookkeeping.
    rev.bump_block(OW, 12 * 16 + 3, 64, -30 * 16 + 3);
    assert!(cache.get(k, &rev).is_none());
    // Player-1 encode miss + this post-edit stale miss.
    assert_eq!(cache.misses(), 2);
    assert!(cache.cached_rev(k).is_some());

    // Re-encoding at the new revision restores hits on the *new* Arc.
    let new_payload: Arc<[u8]> = vec![7u8; 128].into();
    let new_frame = cache.store_fresh(k, &rev, new_payload.clone());
    assert!(Arc::ptr_eq(&new_frame.bytes, &new_payload));
    assert!(cache.get(k, &rev).is_some());

    // Bounded: len can never exceed MAX even under churn.
    for i in 0..1_000i32 {
        cache.store(key(i, 0), 1, vec![0u8; 8].into());
        assert!(cache.len() <= 256);
    }
    assert_eq!(cache.len(), 256);
}

#[test]
fn perceptual_scheduler_bands_and_rates() {
    let viewer = [0.0f32, 64.0, 0.0];
    let fwd = [1.0f32, 0.0, 0.0]; // facing +X

    // In front + near → 20 Hz; in front + mid → 10 Hz; behind → 4 Hz;
    // out of range / stationary-beyond-near → 0 Hz.
    assert_eq!(
        classify_perceptual_band(viewer, fwd, [10.0, 64.0, 0.0], true, 16.0, 128.0),
        PerceptualBand::InFrustumNear
    );
    assert_eq!(
        classify_perceptual_band(viewer, fwd, [60.0, 64.0, 0.0], true, 16.0, 128.0),
        PerceptualBand::InFrustumMid
    );
    assert_eq!(
        classify_perceptual_band(viewer, fwd, [-30.0, 64.0, 0.0], true, 16.0, 128.0),
        PerceptualBand::RearOrFar
    );
    assert_eq!(
        classify_perceptual_band(viewer, fwd, [200.0, 64.0, 0.0], true, 16.0, 128.0),
        PerceptualBand::Quiescent
    );
    assert_eq!(
        classify_perceptual_band(viewer, fwd, [60.0, 64.0, 0.0], false, 16.0, 128.0),
        PerceptualBand::Quiescent
    );

    // Emission rates over 20 ticks (= 1 second): 20 / 10 / 4 / 0 Hz.
    let count = |band: PerceptualBand, entity: u32| {
        (0..20u64)
            .filter(|&t| should_emit_tick(band, t, entity))
            .count()
    };
    assert_eq!(count(PerceptualBand::InFrustumNear, 0), 20);
    assert_eq!(count(PerceptualBand::InFrustumMid, 0), 10);
    assert_eq!(count(PerceptualBand::RearOrFar, 0), 4);
    assert_eq!(count(PerceptualBand::Quiescent, 0), 0);

    // Staggering spreads same-band entities across the stride window: in
    // any single tick, at most one of entities 0..5 in the 4 Hz band emits.
    for tick in 0..20u64 {
        let emitters = (0..5u32)
            .filter(|&id| should_emit_tick(PerceptualBand::RearOrFar, tick, id))
            .count();
        assert!(emitters <= 1, "tick {tick} burst: {emitters} emitters");
    }
}

#[test]
fn perceptual_scheduler_reduction_exceeds_80_percent() {
    // 100 streamed entities over 100 ticks: 10 near, 10 mid, 10 rear and
    // 70 quiescent, versus the naive "every entity every tick" baseline.
    let bands = [
        (10usize, PerceptualBand::InFrustumNear),
        (10usize, PerceptualBand::InFrustumMid),
        (10usize, PerceptualBand::RearOrFar),
        (70usize, PerceptualBand::Quiescent),
    ];
    let baseline: u64 = 100 * 100;
    let mut sent: u64 = 0;
    let mut entity_id = 0u32;
    for (count, band) in bands {
        for _ in 0..count {
            for tick in 0..100u64 {
                if should_emit_tick(band, tick, entity_id) {
                    sent += 1;
                }
            }
            entity_id += 1;
        }
    }
    let reduction = 1.0 - (sent as f64 / baseline as f64);
    assert!(
        reduction >= 0.80,
        "expected >=80% packet reduction, got {:.1}% ({sent}/{baseline})",
        reduction * 100.0
    );
    // The exact count: 10*100 + 10*50 + 10*20 + 70*0 = 1700 → 83%.
    assert_eq!(sent, 1_700);
}
