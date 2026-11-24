// SPDX-License-Identifier: LGPL-3.0-or-later
//! Unit and stress tests for `megu_core::perf_kit`.

use megu_core::perf_kit::{
    fingerprint_bytes, fnv1a64, nearest_rank_percentile_sorted_u64, splitmix64,
    AdaptiveLoadGovernor, BoundedScratchVec, CoalescedDeltaBatch, DirtyFingerprintGate,
    EventWakeWheel, FingerprintBuilder, TickBudgetQueue, GOVERNOR_WINDOW,
};

// ---------------------------------------------------------------------------
// Fingerprinting
// ---------------------------------------------------------------------------

#[test]
fn fnv1a64_matches_the_published_basis_vector() {
    // Known FNV-1a/64 vector: single byte 0x61 ('a').
    assert_eq!(fnv1a64(&[0x61]), 0xaf63_dc4c_8601_ec8c);
    assert_eq!(fnv1a64(b""), 0xcbf2_9ce4_8422_2325);
}

#[test]
fn fingerprints_are_deterministic_and_sensitive() {
    let a = fingerprint_bytes(b"minecraft:cooking_pot[lit=true]");
    assert_eq!(a, fingerprint_bytes(b"minecraft:cooking_pot[lit=true]"));
    assert_ne!(a, fingerprint_bytes(b"minecraft:cooking_pot[lit=false]"));
    // splitmix64 is a bijection: distinct inputs stay distinct at u64 scale
    // for the small domain.
    let seen: std::collections::BTreeSet<u64> = (0..10_000).map(splitmix64).collect();
    assert_eq!(seen.len(), 10_000);
}

#[test]
fn builder_fields_are_self_delimiting() {
    let mut a = FingerprintBuilder::new();
    a.write_str("ab").write_str("c");
    let mut b = FingerprintBuilder::new();
    b.write_str("a").write_str("bc");
    assert_ne!(a.finish(), b.finish());

    let mut x = FingerprintBuilder::new();
    x.write_u8(1).write_u16(2).write_u32(3).write_u64(4);
    let mut y = FingerprintBuilder::new();
    y.write_u8(1).write_u16(2).write_u32(3).write_u64(4);
    assert_eq!(x.finish(), y.finish());

    let mut p = FingerprintBuilder::new();
    p.write_f64(0.0);
    let mut n = FingerprintBuilder::new();
    n.write_f64(-0.0);
    assert_ne!(p.finish(), n.finish(), "to_bits keeps 0.0/-0.0 distinct");
}

// ---------------------------------------------------------------------------
// DirtyFingerprintGate
// ---------------------------------------------------------------------------

#[test]
fn gate_skips_committed_states_and_wakes_on_change() {
    let mut gate = DirtyFingerprintGate::<u32>::new();
    // Nothing committed: dirty.
    assert!(!gate.should_skip(&7, 0xAAAA));
    gate.commit(7, 0xAAAA);
    // Same fingerprint: clean skip.
    assert!(gate.should_skip(&7, 0xAAAA));
    assert!(gate.should_skip(&7, 0xAAAA));
    // Mutated fingerprint: dirty again.
    assert!(!gate.should_skip(&7, 0xBBBB));
    gate.commit(7, 0xBBBB);
    assert!(gate.should_skip(&7, 0xBBBB));

    let stats = gate.stats();
    assert_eq!(stats.skips, 3);
    assert_eq!(stats.updates, 2);
}

#[test]
fn gate_invalidate_remove_and_observe() {
    let mut gate = DirtyFingerprintGate::<&'static str>::new();
    gate.commit("pot", 1);
    assert!(gate.should_skip(&"pot", 1));
    gate.invalidate(&"pot");
    assert!(!gate.should_skip(&"pot", 1), "invalidated key is dirty");

    // observe() fuses check+commit.
    assert!(gate.observe("pot", 2), "first observe commits");
    assert!(!gate.observe("pot", 2), "repeat observe skips");
    assert!(gate.observe("pot", 3), "changed observe commits");

    gate.remove(&"pot");
    assert_eq!(gate.fingerprint_of(&"pot"), None);
    assert!(!gate.should_skip(&"pot", 3));

    let stats = gate.stats();
    assert_eq!(stats.skips, 2, "committed-hit skips only");
    assert_eq!(stats.updates, 3, "1, 2, 3 commits");
}

#[test]
fn gate_handles_one_thousand_idle_machines_in_o1() {
    // Simulate 1,000 block entities: one commit each, then a thousand
    // same-fingerprint ticks must all skip.
    let mut gate = DirtyFingerprintGate::<u64>::with_capacity(1000);
    for id in 0..1000u64 {
        gate.commit(id, id.wrapping_mul(0x9E37_79B1));
    }
    gate.reset_stats();
    for _tick in 0..10 {
        for id in 0..1000u64 {
            assert!(gate.should_skip(&id, id.wrapping_mul(0x9E37_79B1)));
        }
    }
    let stats = gate.stats();
    assert_eq!(stats.skips, 10_000);
    assert_eq!(stats.updates, 0, "idle machines commit nothing");
    // One machine changes -> exactly one misses the skip.
    assert!(!gate.should_skip(&500, 0xDEAD));
    gate.commit(500, 0xDEAD);
    assert!(gate.should_skip(&500, 0xDEAD));
    let stats = gate.stats();
    assert_eq!(stats.skips, 10_001);
    assert_eq!(stats.updates, 1);
}

// ---------------------------------------------------------------------------
// BoundedScratchVec
// ---------------------------------------------------------------------------

#[test]
fn scratch_vec_reuses_capacity_with_zero_growth_after_warmup() {
    let mut scratch = BoundedScratchVec::<u64>::new();
    // Warmup: first fill grows the buffer.
    for i in 0..512 {
        scratch.push(i);
    }
    let events_after_fill = scratch.growth_events();
    assert!(events_after_fill > 0);
    let cap = scratch.capacity();
    assert!(cap >= 512);

    // Steady state: 1,000 reuse rounds, zero further growth.
    for round in 0..1000 {
        scratch.clear();
        assert_eq!(scratch.len(), 0);
        assert_eq!(scratch.capacity(), cap);
        for i in 0..512 {
            scratch.push(round * i);
        }
    }
    assert_eq!(
        scratch.growth_events(),
        events_after_fill,
        "steady-state reuse must not churn"
    );
}

#[test]
fn scratch_vec_clear_bounded_shrinks_past_cap() {
    let mut scratch = BoundedScratchVec::<u8>::new();
    scratch.resize(100_000, 7);
    assert!(scratch.capacity() >= 100_000);
    scratch.clear_bounded(1024);
    assert_eq!(scratch.len(), 0);
    assert!(
        scratch.capacity() <= 1024,
        "burst capacity must shrink to the bound, got {}",
        scratch.capacity()
    );
    // Inside the bound, clear_bounded is a plain clear.
    scratch.resize(64, 1);
    let cap = scratch.capacity();
    scratch.clear_bounded(1024);
    assert_eq!(scratch.capacity(), cap, "in-bound clear keeps capacity");
}

#[test]
fn scratch_vec_slice_and_iter_semantics() {
    let mut scratch = BoundedScratchVec::from(vec![1, 2, 3]);
    scratch.extend_from_slice(&[4, 5]);
    scratch.extend([6, 7, 8]);
    assert_eq!(scratch.as_slice(), &[1, 2, 3, 4, 5, 6, 7, 8]);
    scratch.as_mut_slice()[0] = 9;
    scratch.truncate(6);
    assert_eq!(scratch.pop(), Some(6));
    let sum: i32 = scratch.iter().sum();
    assert_eq!(sum, 9 + 2 + 3 + 4 + 5);
    scratch.sort();
    assert_eq!(scratch.as_slice(), &[2, 3, 4, 5, 9]);
    let inner: Vec<i32> = scratch.into_inner();
    assert_eq!(inner, vec![2, 3, 4, 5, 9]);
}

// ---------------------------------------------------------------------------
// TickBudgetQueue
// ---------------------------------------------------------------------------

#[test]
fn budget_queue_dedupes_enqueues() {
    let mut q = TickBudgetQueue::<u32>::new();
    assert!(q.enqueue(1));
    assert!(q.enqueue(2));
    assert!(!q.enqueue(1), "already queued -> idempotent no-op");
    assert!(!q.enqueue(2));
    assert_eq!(q.len(), 2);
    assert!(q.contains(&1));
    assert!(q.contains(&2));
    assert!(!q.contains(&3));
}

#[test]
fn budget_queue_slices_bursts_smoothly() {
    let mut q = TickBudgetQueue::<u32>::new();
    for i in 0..100 {
        q.enqueue(i);
    }
    // 10 items per tick -> exactly 10 ticks of 10.
    let mut drained = Vec::new();
    let mut out = Vec::new();
    for _ in 0..10 {
        let r = q.drain_budget(10, u64::MAX, |_| 1, &mut out);
        assert_eq!(r.items, 10);
        assert_eq!(r.cost, 10);
        drained.append(&mut out);
    }
    assert!(q.is_empty());
    assert_eq!(drained, (0..100).collect::<Vec<_>>(), "strict FIFO");
}

#[test]
fn budget_queue_honors_cost_and_guarantees_progress() {
    let mut q = TickBudgetQueue::<u32>::new();
    for i in 0..10 {
        q.enqueue(i);
    }
    let mut out = Vec::new();
    // cost 3 per item, budget 10 -> items at cost 3,3,3 then 4th would hit 12.
    let r = q.drain_budget(10, 10, |_| 3, &mut out);
    assert_eq!(r.items, 3);
    assert_eq!(r.cost, 9);
    assert_eq!(out, vec![0, 1, 2]);

    // Head item costs more than the whole budget: still drains exactly one
    // so the queue can never deadlock.
    let r = q.drain_budget(10, 1, |_| 100, &mut out);
    assert_eq!(r.items, 1);
    assert_eq!(r.cost, 100);
    assert_eq!(out, vec![0, 1, 2, 3]);
}

#[test]
fn budget_queue_requeue_front_and_flush_all() {
    let mut q = TickBudgetQueue::<u32>::new();
    q.enqueue(1);
    q.enqueue(2);
    q.enqueue(3);
    let mut out = Vec::new();
    q.drain_budget(1, u64::MAX, |_| 1, &mut out);
    assert_eq!(out, vec![1]);
    // Item 1 could not finish -> back to the front, before 2 and 3.
    assert!(q.requeue_front(1));
    assert!(!q.requeue_front(2), "still queued");
    out.clear();
    q.flush_all(&mut out);
    assert_eq!(out, vec![1, 2, 3]);
    assert!(q.is_empty());

    // remove() pulls a mid-queue key.
    q.enqueue(9);
    q.enqueue(8);
    q.enqueue(7);
    assert!(q.remove(&8));
    assert!(!q.contains(&8));
    out.clear();
    q.flush_all(&mut out);
    assert_eq!(out, vec![9, 7]);
}

// ---------------------------------------------------------------------------
// EventWakeWheel
// ---------------------------------------------------------------------------

#[test]
fn wake_wheel_schedules_cancels_and_pops_due_in_order() {
    let mut wheel = EventWakeWheel::<u32>::new();
    wheel.schedule(3, 50);
    wheel.schedule(1, 10);
    wheel.schedule(2, 10);
    assert_eq!(wheel.len(), 3);
    assert_eq!(wheel.wake_of(&3), Some(50));
    assert_eq!(wheel.next_wake(), Some(10));

    let mut out = BoundedScratchVec::<u32>::new();
    // Nothing due yet.
    assert_eq!(wheel.pop_due(9, &mut out), 0);
    assert!(out.is_empty());
    // Tick 10: both keys, sorted by key within the tick.
    assert_eq!(wheel.pop_due(10, &mut out), 2);
    assert_eq!(out.as_slice(), &[1, 2]);
    assert_eq!(wheel.len(), 1);
    out.clear();

    // Cancel before due: never pops.
    assert!(wheel.cancel(&3));
    assert!(!wheel.cancel(&3));
    assert_eq!(wheel.pop_due(1000, &mut out), 0);
    assert!(wheel.is_empty());
}

#[test]
fn wake_wheel_reschedules_atomically() {
    let mut wheel = EventWakeWheel::<u32>::new();
    wheel.schedule(1, 10);
    wheel.schedule(1, 20);
    wheel.schedule(1, 20); // idempotent
    assert_eq!(wheel.len(), 1);
    assert_eq!(wheel.wake_of(&1), Some(20));
    assert_eq!(wheel.next_wake(), Some(20));
    let mut out = BoundedScratchVec::<u32>::new();
    assert_eq!(wheel.pop_due(15, &mut out), 0);
    assert_eq!(wheel.pop_due(20, &mut out), 1);
    assert_eq!(out.as_slice(), &[1]);
}

#[test]
fn wake_wheel_orders_mixed_ticks_deterministically() {
    let mut wheel = EventWakeWheel::<u32>::new();
    // Insertion order deliberately scrambled.
    for (k, t) in [(5u32, 30u64), (1, 30), (9, 5), (2, 5), (7, 30), (3, 15)] {
        wheel.schedule(k, t);
    }
    let mut out = BoundedScratchVec::<u32>::new();
    wheel.pop_due(30, &mut out);
    assert_eq!(
        out.as_slice(),
        &[2, 9, 3, 1, 5, 7],
        "ascending tick, then ascending key"
    );
}

#[test]
fn wake_wheel_ten_thousand_machines_wake_on_exact_ticks() {
    const MACHINES: u64 = 10_000;
    let mut wheel = EventWakeWheel::<u64>::new();
    // Machine i wakes at tick 100 + (i % 97) -> 98 distinct waves.
    for i in 0..MACHINES {
        wheel.schedule(i, 100 + i % 97);
    }
    assert_eq!(wheel.len() as u64, MACHINES);

    let mut out = BoundedScratchVec::<u64>::new();
    let mut total_popped = 0u64;
    for tick in 0..200u64 {
        out.clear();
        let n = wheel.pop_due(tick, &mut out);
        if tick < 100 {
            assert_eq!(n, 0, "nothing may wake early (tick {tick})");
        } else if tick < 197 {
            // Exactly the machines with wake == tick (i % 97 == tick - 100).
            let expected: Vec<u64> = (0..MACHINES)
                .filter(|i| i % 97 == tick - 100)
                .collect();
            assert_eq!(out.as_slice(), expected.as_slice(), "tick {tick}");
        }
        total_popped += n as u64;
    }
    assert_eq!(total_popped, MACHINES);
    assert!(wheel.is_empty());
}

#[test]
fn wake_wheel_far_future_and_u64_max_ticks() {
    let mut wheel = EventWakeWheel::<u32>::new();
    wheel.schedule(1, u64::MAX);
    wheel.schedule(2, u64::MAX);
    let mut out = BoundedScratchVec::<u32>::new();
    assert_eq!(wheel.pop_due(u64::MAX - 1, &mut out), 0);
    assert_eq!(wheel.pop_due(u64::MAX, &mut out), 2, "no overflow wrap");
}

// ---------------------------------------------------------------------------
// CoalescedDeltaBatch
// ---------------------------------------------------------------------------

#[test]
fn delta_batch_collapses_a_hundred_writes_to_one() {
    let mut batch = CoalescedDeltaBatch::<u32, u64>::new();
    for v in 0..100 {
        let prev = batch.record(7, v);
        if v == 0 {
            assert_eq!(prev, None);
        } else {
            assert_eq!(prev, Some(v - 1));
        }
    }
    assert_eq!(batch.len(), 1);
    assert_eq!(batch.get(&7), Some(&99));

    let mut out = Vec::new();
    batch.drain_sorted_into(&mut out);
    assert_eq!(out, vec![(7, 99)]);
    assert!(batch.is_empty());
}

#[test]
fn delta_batch_drains_in_sorted_order_not_insert_order() {
    let mut batch = CoalescedDeltaBatch::<u32, &'static str>::new();
    for k in [50, 1, 900, 3, 77] {
        batch.record(k, "d");
    }
    // Rewrite some keys out of order — still one slot each, still sorted.
    batch.record(3, "d2");
    batch.record(1, "d1");
    let mut out = Vec::new();
    batch.drain_sorted_into(&mut out);
    let keys: Vec<u32> = out.iter().map(|(k, _)| *k).collect();
    assert_eq!(keys, vec![1, 3, 50, 77, 900]);
    assert_eq!(out[0].1, "d1");
    assert_eq!(out[1].1, "d2");
}

#[test]
fn delta_batch_remove_get_iter_clear() {
    let mut batch = CoalescedDeltaBatch::<u32, u32>::new();
    batch.record(1, 10);
    batch.record(2, 20);
    assert!(batch.contains(&1));
    assert_eq!(
        batch.iter().map(|(k, v)| (*k, *v)).collect::<Vec<_>>(),
        vec![(1, 10), (2, 20)]
    );
    assert!(batch.remove(&1));
    assert!(!batch.remove(&1));
    assert_eq!(batch.len(), 1);
    batch.clear();
    assert!(batch.is_empty());
}

// ---------------------------------------------------------------------------
// AdaptiveLoadGovernor
// ---------------------------------------------------------------------------

#[test]
fn governor_scrubs_non_finite_and_negative_samples() {
    let mut g = AdaptiveLoadGovernor::new(50.0, 0.25);
    g.record(f64::NAN);
    g.record(f64::INFINITY);
    g.record(f64::NEG_INFINITY);
    g.record(-1.0);
    assert_eq!(g.scrubbed_count(), 4);
    assert_eq!(g.len(), 0, "nothing poisoned the window");
    assert!(g.p95().is_none());
    g.record(40.0);
    assert_eq!(g.len(), 1);
    assert_eq!(g.scrubbed_count(), 4);
}

#[test]
fn governor_reports_exact_nearest_rank_percentiles() {
    let mut g = AdaptiveLoadGovernor::new(50.0, 0.0);
    for i in 1..=100 {
        g.record(f64::from(i));
    }
    assert_eq!(g.p50(), Some(50.0));
    assert_eq!(g.p95(), Some(95.0));
    assert_eq!(g.p99(), Some(99.0));
    assert_eq!(g.percentile(100.0), Some(100.0));
    assert_eq!(g.percentile(0.0), Some(1.0));
}

#[test]
fn sorted_integer_percentiles_match_nearest_rank_boundaries() {
    let samples: Vec<u64> = (1..=100).collect();
    assert_eq!(nearest_rank_percentile_sorted_u64(&[], 50), None);
    assert_eq!(nearest_rank_percentile_sorted_u64(&samples, 0), Some(1));
    assert_eq!(nearest_rank_percentile_sorted_u64(&samples, 50), Some(50));
    assert_eq!(nearest_rank_percentile_sorted_u64(&samples, 99), Some(99));
    assert_eq!(nearest_rank_percentile_sorted_u64(&samples, 100), Some(100));
    assert_eq!(nearest_rank_percentile_sorted_u64(&samples, 101), Some(100));
}

#[test]
fn governor_window_is_exactly_bounded() {
    let mut g = AdaptiveLoadGovernor::new(50.0, 0.0);
    for i in 0..(GOVERNOR_WINDOW * 3) {
        g.record(i as f64);
    }
    assert_eq!(g.len(), GOVERNOR_WINDOW);
    // Oldest samples evicted: window holds the last 120 values
    // (240..=359), so p50 is the 60th of those.
    let expected: Vec<f64> = (240..360).map(|i| i as f64).collect();
    let mut idx = ((0.50 * GOVERNOR_WINDOW as f64).ceil() as usize) - 1;
    assert_eq!(g.p50(), Some(expected[idx]));
    idx = ((0.95 * GOVERNOR_WINDOW as f64).ceil() as usize) - 1;
    assert_eq!(g.p95(), Some(expected[idx]));
}

#[test]
fn governor_dead_band_holds_scale_and_overload_sheds() {
    let mut g = AdaptiveLoadGovernor::new(50.0, 0.25);

    // p95 inside the 0.90..1.10 dead band -> exact hold.
    for _ in 0..120 {
        g.record(52.0); // 1.04 * target
    }
    let s = g.budget_scale();
    assert_eq!(s, 1.0, "inside dead-band the scale must not move");

    // p95 = 100 = 2x target -> desired = 0.5, eased toward by 25% steps.
    for _ in 0..120 {
        g.record(100.0);
    }
    let s1 = g.budget_scale();
    assert!(s1 < 1.0 && s1 > 0.5, "first step closes 25% of the gap");
    let s2 = g.budget_scale();
    assert!(s2 < s1, "continued overload keeps shedding");
    // Converges to 0.5, never below min_scale.
    for _ in 0..60 {
        g.budget_scale();
    }
    assert!((g.current_scale() - 0.5).abs() < 0.01);
}

#[test]
fn governor_recovers_to_full_scale_under_headroom() {
    let mut g = AdaptiveLoadGovernor::new(50.0, 0.25);
    for _ in 0..120 {
        g.record(200.0); // heavy overload
    }
    for _ in 0..20 {
        g.budget_scale();
    }
    let shed = g.current_scale();
    assert!(shed < 1.0);
    // Clear overload, report strong headroom -> scale eases back to 1.0.
    for _ in 0..120 {
        g.record(10.0);
    }
    for _ in 0..100 {
        g.budget_scale();
    }
    assert!((g.current_scale() - 1.0).abs() < 1e-9);
}

#[test]
fn governor_hysteresis_is_deterministic_on_boundary() {
    let mut g = AdaptiveLoadGovernor::new(100.0, 0.5);
    for _ in 0..120 {
        g.record(110.0); // exactly 1.10 * target -> inside dead-band
    }
    assert_eq!(g.budget_scale(), 1.0);
    for _ in 0..120 {
        g.record(111.0); // just above -> sheds
    }
    assert!(g.budget_scale() < 1.0);
}
