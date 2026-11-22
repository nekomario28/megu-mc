// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! Unit tests for `mc_spine_core::tier25_guard_batch` — the Tier-2.5
//! zero-JNI-idle guard pre-filter and O(1)-per-tick `TB25` batch
//! coalescer.

use mc_spine_core::tier25_guard_batch::{
    BatchEventRecord, GuardEvalContext, GuardPredicate, LoaderLane, Tier25Error,
    TickBatchCoalescer, FRAME_MAGIC, FRAME_VERSION, MAX_BATCH_EVENTS, MAX_EVENT_PAYLOAD_BYTES,
    MAX_GUARD_DEPTH, MAX_GUARD_NODES,
};

fn ctx(entity_id: i32) -> GuardEvalContext {
    GuardEvalContext {
        entity_id,
        ..GuardEvalContext::default()
    }
}

fn record(hook_id: u32, entity_id: i32, key: Option<u64>) -> BatchEventRecord {
    BatchEventRecord {
        hook_id,
        entity_id,
        flags: 0xCAFE,
        coalesce_key: key,
        payload: vec![0xAB; 8],
    }
}

// ---------------------------------------------------------------------------
// Required: zero-JNI idle filtering over a realistic entity swarm
// ---------------------------------------------------------------------------

/// Simulates `LivingEntity#travel` firing for 10,000 entities in one tick
/// where exactly one entity is in lava (Create `LavaSwimmingMixin`-style
/// sparse site). Every non-matching entity must be filtered out in pure
/// Rust — and the following idle tick must produce `None`, i.e. exactly
/// 0 JNI crossings.
#[test]
fn guard_filters_9999_idle_entities_with_zero_jni_crossings_and_fires_exact_match() {
    let mut coalescer = TickBatchCoalescer::new();
    let guard = GuardPredicate::EntityInLava;

    let total = 10_000i32;
    let hot_entity = 4_242i32;
    for entity_id in 0..total {
        let mut context = ctx(entity_id);
        context.in_lava = entity_id == hot_entity;
        let queued = coalescer
            .submit_guarded(
                LoaderLane::NeoForge,
                &guard,
                &context,
                record(7, entity_id, None),
            )
            .expect("submit must not fail");
        assert_eq!(queued, entity_id == hot_entity, "entity {entity_id}");
    }

    let metrics = coalescer.metrics();
    assert_eq!(metrics.evaluated, 10_000);
    assert_eq!(metrics.filtered_out, 9_999);
    assert_eq!(metrics.dispatched, 1);
    assert_eq!(metrics.coalesced, 0);
    assert_eq!(coalescer.pending_count(LoaderLane::NeoForge), 1);

    // Busy tick: exactly ONE flat frame crosses into the JVM host.
    let frame = coalescer
        .flush_tick(500, LoaderLane::NeoForge)
        .expect("one survivor must produce a frame");
    assert_eq!(&frame[0..4], &FRAME_MAGIC);
    assert_eq!(frame[4], FRAME_VERSION);
    assert_eq!(frame[5], LoaderLane::NeoForge as u8);
    let decoded = TickBatchCoalescer::decode_batch_frame(&frame).expect("frame must decode");
    assert_eq!(decoded.tick, 500);
    assert_eq!(decoded.loader, LoaderLane::NeoForge);
    assert_eq!(decoded.events.len(), 1);
    assert_eq!(decoded.events[0].entity_id, hot_entity);
    assert_eq!(decoded.events[0].hook_id, 7);
    assert_eq!(coalescer.metrics().flushes, 1);

    // Idle tick: the lane is empty, so flush returns None — 0 JNI calls.
    assert!(coalescer
        .flush_tick(501, LoaderLane::NeoForge)
        .is_none());
    assert_eq!(coalescer.metrics().zero_crossing_ticks, 1);
    // A wholly filtered tick is also zero-crossing.
    for entity_id in 0..total {
        let queued = coalescer
            .submit_guarded(
                LoaderLane::NeoForge,
                &guard,
                &ctx(entity_id),
                record(7, entity_id, None),
            )
            .expect("submit must not fail");
        assert!(!queued);
    }
    assert!(coalescer
        .flush_tick(502, LoaderLane::NeoForge)
        .is_none());
    let metrics = coalescer.metrics();
    assert_eq!(metrics.filtered_out, 9_999 + 10_000);
    assert_eq!(metrics.dispatched, 1);
    assert_eq!(metrics.flushes, 1);
    assert_eq!(metrics.zero_crossing_ticks, 2);
}

// ---------------------------------------------------------------------------
// Required: O(1) coalesced batch in ONE deterministic TB25 frame
// ---------------------------------------------------------------------------

/// 1,000 guard-passing events — including duplicate coalesce keys — must
/// drain as ONE flat frame (flushes == 1, so 1 JNI crossing regardless of
/// count), dedup last-write-wins, round-trip through the strict decoder,
/// and be byte-deterministic.
#[test]
fn o1_batch_coalesces_1000_events_into_single_tb25_frame_roundtrip() {
    let guard = GuardPredicate::And(vec![
        GuardPredicate::EntityOnGround,
        GuardPredicate::Or(vec![
            GuardPredicate::EntityInWater,
            GuardPredicate::Not(Box::new(GuardPredicate::EntityInWater)),
        ]),
    ]);
    guard.validate().expect("guard must be valid");

    let mut coalescer = TickBatchCoalescer::new();
    let mut twin = TickBatchCoalescer::new();
    let mut context = ctx(0);
    context.on_ground = true;

    let submissions = 1_000u32;
    for i in 0..submissions {
        // Keys 0..256 each repeat ~4x -> dedup must collapse to 256 live
        // records, each holding the LAST payload submitted for its key.
        let key = u64::from(i % 256);
        let mut r = record(100 + i % 3, i as i32, Some(key));
        r.payload = vec![(i & 0xFF) as u8; 4];
        assert!(coalescer
            .submit_guarded(LoaderLane::Fabric, &guard, &context, r.clone())
            .expect("submit must not fail"));
        assert!(twin
            .submit_guarded(LoaderLane::Fabric, &guard, &context, r)
            .expect("submit must not fail"));
    }

    let metrics = coalescer.metrics();
    assert_eq!(metrics.evaluated, 1_000);
    assert_eq!(metrics.filtered_out, 0);
    assert_eq!(metrics.dispatched, 1_000);
    assert_eq!(metrics.coalesced, 1_000 - 256);
    assert_eq!(coalescer.pending_count(LoaderLane::Fabric), 256);

    let frame = coalescer
        .flush_tick(9_001, LoaderLane::Fabric)
        .expect("non-empty lane must produce a frame");
    let twin_frame = twin
        .flush_tick(9_001, LoaderLane::Fabric)
        .expect("twin must produce a frame");
    assert_eq!(frame, twin_frame, "framing must be byte-deterministic");
    assert_eq!(coalescer.metrics().flushes, 1);

    // Fixed-size frame: 20B header + 256*(21+4)B records + 4B CRC32.
    assert_eq!(frame.len(), 20 + 256 * 25 + 4);
    assert_eq!(&frame[0..4], &FRAME_MAGIC);
    assert_eq!(frame[5], LoaderLane::Fabric as u8);

    let decoded = TickBatchCoalescer::decode_batch_frame(&frame).expect("frame must decode");
    assert_eq!(decoded.version, FRAME_VERSION);
    assert_eq!(decoded.loader, LoaderLane::Fabric);
    assert_eq!(decoded.tick, 9_001);
    assert_eq!(decoded.events.len(), 256);
    // Records sit in stable first-submission order with last-write-wins
    // payloads: key k last fired at i = k + 768 (k < 232) or k + 512
    // (k >= 232), since i ranges over 0..1000 with key = i % 256.
    for (idx, event) in decoded.events.iter().enumerate() {
        let last_i = idx as u32 + if idx < 232 { 768 } else { 512 };
        assert_eq!(event.coalesce_key, Some(idx as u64));
        assert_eq!(event.entity_id, last_i as i32);
        assert_eq!(event.hook_id, 100 + last_i % 3);
        assert_eq!(event.payload, vec![(last_i & 0xFF) as u8; 4]);
    }
}

// ---------------------------------------------------------------------------
// Required: hostile inputs fail closed
// ---------------------------------------------------------------------------

/// Depth > 8 guards, an over-capacity batch, a flipped CRC bit, a
/// truncated frame, and trailing bytes must ALL fail closed.
#[test]
fn hostile_guard_depth_oversized_batch_and_corrupt_crc_fail_closed() {
    let mut coalescer = TickBatchCoalescer::new();
    let context = ctx(1);

    // --- depth > MAX_GUARD_DEPTH -------------------------------------
    let mut deep = GuardPredicate::EntityInLava;
    for _ in 0..MAX_GUARD_DEPTH {
        deep = GuardPredicate::Not(Box::new(deep));
    }
    // `deep` is now 8 Nots + leaf = depth 9 at the leaf.
    assert_eq!(
        deep.validate(),
        Err(Tier25Error::GuardDepthExceeded {
            depth: MAX_GUARD_DEPTH + 1
        })
    );
    let before = coalescer.metrics();
    assert!(coalescer
        .submit_guarded(LoaderLane::NeoForge, &deep, &context, record(1, 1, None))
        .is_err());
    let after = coalescer.metrics();
    assert_eq!(after.rejected, before.rejected + 1);
    // An invalid guard never reaches evaluation or the queue.
    assert_eq!(after.evaluated, before.evaluated);
    assert_eq!(coalescer.pending_count(LoaderLane::NeoForge), 0);
    // eval on the hostile tree is still bounded and panic-free.
    let _ = deep.eval(&context);

    // --- node count > MAX_GUARD_NODES ---------------------------------
    let wide = GuardPredicate::Or(
        (0..MAX_GUARD_NODES)
            .map(|_| GuardPredicate::EntityOnGround)
            .collect(),
    );
    // MAX_GUARD_NODES leaves + 1 Or root = MAX_GUARD_NODES + 1 nodes.
    assert_eq!(
        wide.validate(),
        Err(Tier25Error::GuardNodeBudgetExceeded {
            nodes: MAX_GUARD_NODES + 1
        })
    );

    // --- other invalid guard shapes ------------------------------------
    assert_eq!(
        GuardPredicate::And(vec![]).validate(),
        Err(Tier25Error::GuardEmptyCombinator)
    );
    assert_eq!(
        GuardPredicate::EntityHasEffect { effect_bit: 64 }.validate(),
        Err(Tier25Error::GuardInvalidEffectBit { effect_bit: 64 })
    );

    // --- payload > MAX_EVENT_PAYLOAD_BYTES -----------------------------
    let mut big = record(1, 1, None);
    big.payload = vec![0; MAX_EVENT_PAYLOAD_BYTES + 1];
    assert_eq!(
        coalescer.submit_guarded(
            LoaderLane::NeoForge,
            &GuardPredicate::EntityOnGround,
            &{
                let mut c = ctx(1);
                c.on_ground = true;
                c
            },
            big,
        ),
        Err(Tier25Error::PayloadTooLarge {
            size: MAX_EVENT_PAYLOAD_BYTES + 1
        })
    );

    // --- batch > MAX_BATCH_EVENTS --------------------------------------
    let pass_guard = GuardPredicate::EntityOnGround;
    let mut on_ground = ctx(0);
    on_ground.on_ground = true;
    for i in 0..MAX_BATCH_EVENTS {
        coalescer
            .submit_guarded(
                LoaderLane::Fabric,
                &pass_guard,
                &on_ground,
                record(1, i as i32, None),
            )
            .expect("in-capacity submits must succeed");
    }
    assert_eq!(
        coalescer.submit_guarded(
            LoaderLane::Fabric,
            &pass_guard,
            &on_ground,
            record(1, 0, None),
        ),
        Err(Tier25Error::BatchFull)
    );
    // ...but coalescing onto an existing key still succeeds at capacity.
    assert!(coalescer
        .submit_guarded(
            LoaderLane::NeoForge,
            &pass_guard,
            &on_ground,
            record(1, 0, Some(9)),
        )
        .unwrap());
    assert!(coalescer
        .submit_guarded(
            LoaderLane::NeoForge,
            &pass_guard,
            &on_ground,
            record(1, 0, Some(9)),
        )
        .unwrap());

    // --- strict decode rejection paths ----------------------------------
    let frame = coalescer
        .flush_tick(1, LoaderLane::Fabric)
        .expect("full lane must produce a frame");

    // Flipped bit in the body -> CRC mismatch.
    let mut corrupt = frame.clone();
    corrupt[24] ^= 0x01;
    assert_eq!(
        TickBatchCoalescer::decode_batch_frame(&corrupt),
        Err(Tier25Error::CrcMismatch)
    );
    // Flipped bit inside the CRC trailer itself -> CRC mismatch.
    let mut corrupt_crc = frame.clone();
    let last = corrupt_crc.len() - 1;
    corrupt_crc[last] ^= 0x01;
    assert_eq!(
        TickBatchCoalescer::decode_batch_frame(&corrupt_crc),
        Err(Tier25Error::CrcMismatch)
    );
    // Truncated mid-record -> Truncated.
    assert_eq!(
        TickBatchCoalescer::decode_batch_frame(&frame[..frame.len() - 10]),
        Err(Tier25Error::FrameTruncated)
    );
    // Under the minimum header+trailer size -> Truncated.
    assert_eq!(
        TickBatchCoalescer::decode_batch_frame(&frame[..20]),
        Err(Tier25Error::FrameTruncated)
    );
    // Trailing bytes after the CRC -> TrailingBytes.
    let mut padded = frame.clone();
    padded.push(0);
    assert_eq!(
        TickBatchCoalescer::decode_batch_frame(&padded),
        Err(Tier25Error::TrailingBytes)
    );
    // Bad magic / version / loader are all rejected.
    let mut bad = frame.clone();
    bad[0] = b'X';
    assert_eq!(
        TickBatchCoalescer::decode_batch_frame(&bad),
        Err(Tier25Error::BadMagic)
    );
    let mut bad = frame.clone();
    bad[4] = 99;
    assert_eq!(
        TickBatchCoalescer::decode_batch_frame(&bad),
        Err(Tier25Error::UnsupportedVersion(99))
    );
    let mut bad = frame.clone();
    bad[5] = 7;
    assert_eq!(
        TickBatchCoalescer::decode_batch_frame(&bad),
        Err(Tier25Error::UnknownLoader(7))
    );
}

// ---------------------------------------------------------------------------
// Supporting: guard semantics
// ---------------------------------------------------------------------------

#[test]
fn guard_leaves_and_combinators_evaluate_correctly() {
    let mut c = ctx(3);
    c.entity_type_id = 17;
    c.dim_id = 2;
    c.in_water = true;
    c.sneaking = true;
    c.active_effects = 1 << 5 | 1 << 40;
    c.held_item_hash = 0xDEAD_BEEF;
    c.slot_index = 3;

    assert!(GuardPredicate::EntityInWater.eval(&c));
    assert!(!GuardPredicate::EntityInLava.eval(&c));
    assert!(GuardPredicate::PlayerSneaking.eval(&c));
    assert!(GuardPredicate::EntityHasEffect { effect_bit: 5 }.eval(&c));
    assert!(GuardPredicate::EntityHasEffect { effect_bit: 40 }.eval(&c));
    assert!(!GuardPredicate::EntityHasEffect { effect_bit: 6 }.eval(&c));
    // An out-of-range bit evaluates false rather than panicking.
    assert!(!GuardPredicate::EntityHasEffect { effect_bit: 99 }.eval(&c));
    assert!(GuardPredicate::EntityTypeMatches { entity_type_id: 17 }.eval(&c));
    assert!(!GuardPredicate::EntityTypeMatches { entity_type_id: 18 }.eval(&c));
    assert!(GuardPredicate::DimensionMatches { dim_id: 2 }.eval(&c));
    assert!(!GuardPredicate::DimensionMatches { dim_id: 3 }.eval(&c));
    assert!(GuardPredicate::HoldingItemId {
        item_hash: 0xDEAD_BEEF
    }
    .eval(&c));
    assert!(!GuardPredicate::HoldingItemId { item_hash: 1 }.eval(&c));
    assert!(GuardPredicate::SlotIndexEquals { slot: 3 }.eval(&c));
    assert!(!GuardPredicate::SlotIndexEquals { slot: 0 }.eval(&c));

    let compound = GuardPredicate::And(vec![
        GuardPredicate::EntityInWater,
        GuardPredicate::Not(Box::new(GuardPredicate::EntityInLava)),
        GuardPredicate::Or(vec![
            GuardPredicate::PlayerSneaking,
            GuardPredicate::EntityOnGround,
        ]),
    ]);
    assert!(compound.eval(&c));
    c.sneaking = false;
    assert!(!compound.eval(&c));
}

#[test]
fn depth_boundary_at_max_guard_depth_is_accepted() {
    // Exactly MAX_GUARD_DEPTH nodes deep must still validate and eval.
    let mut guard = GuardPredicate::EntityInLava;
    for _ in 1..MAX_GUARD_DEPTH {
        guard = GuardPredicate::Not(Box::new(guard));
    }
    guard.validate().expect("depth == MAX_GUARD_DEPTH is legal");
    // 7 Nots -> leaf negated an odd number of times.
    let mut c = ctx(0);
    c.in_lava = true;
    assert!(!guard.eval(&c));
}

#[test]
fn lanes_are_independent() {
    let mut coalescer = TickBatchCoalescer::new();
    let mut c = ctx(0);
    c.in_lava = true;
    let g = GuardPredicate::EntityInLava;
    coalescer
        .submit_guarded(LoaderLane::NeoForge, &g, &c, record(1, 1, None))
        .unwrap();
    coalescer
        .submit_guarded(LoaderLane::Fabric, &g, &c, record(2, 2, None))
        .unwrap();
    // Flushing NeoForge leaves the Fabric queue untouched.
    let nf = coalescer.flush_tick(1, LoaderLane::NeoForge).unwrap();
    assert_eq!(
        TickBatchCoalescer::decode_batch_frame(&nf)
            .unwrap()
            .events
            .len(),
        1
    );
    assert_eq!(coalescer.pending_count(LoaderLane::Fabric), 1);
    assert!(coalescer.flush_tick(1, LoaderLane::NeoForge).is_none());
    let fb = coalescer.flush_tick(1, LoaderLane::Fabric).unwrap();
    let decoded = TickBatchCoalescer::decode_batch_frame(&fb).unwrap();
    assert_eq!(decoded.loader, LoaderLane::Fabric);
    assert_eq!(decoded.events[0].hook_id, 2);
}
