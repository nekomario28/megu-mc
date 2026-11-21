// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! Integration tests for `mod_tick_runtime` and its culinary instantiation,
//! [`culinary_adapter::CookingPotCluster`]: 1,000 modded cooking pots sleep
//! in O(1), wake on the exact recipe-completion tick, slice completion
//! bursts through a per-tick budget, and coalesce outbound sync deltas.

use mod_adapter_core::culinary_adapter::{
    CookingPotCluster, CookingRecipe, CulinaryRegistry, IngredientMatcher, PotSyncSnapshot,
};
use mod_adapter_core::mod_tick_runtime::ModTickRuntime;

/// One-recipe registry: `test:meal` = 1 `test:grain` -> `test:meal` in
/// `cooking_time` ticks.
fn stew_registry(cooking_time: u32) -> CulinaryRegistry {
    let mut reg = CulinaryRegistry::new();
    reg.cooking_recipes.push(CookingRecipe {
        id: "testmod:meal".to_string(),
        recipe_type: "testmod:cooking".to_string(),
        ingredients: vec![IngredientMatcher::Item("testmod:grain".to_string())],
        container: None,
        result_item: "testmod:meal".to_string(),
        result_count: 1,
        cooking_time,
        experience: 0.35,
    });
    reg
}

// ---------------------------------------------------------------------------
// ModTickRuntime — the generic contract
// ---------------------------------------------------------------------------

#[test]
fn runtime_commits_wakes_and_coalesces_generically() {
    // The runtime is generic — prove it with a non-culinary machine key and
    // payload, the same way any tech/magic/dimension mod would plug in.
    let mut rt = ModTickRuntime::<String, u64>::new();
    let key = "anymod:charger[0,64,0]".to_string();

    // is_dirty is the O(1) idle front door.
    assert!(rt.is_dirty(&key, 0xF00D));
    rt.commit(key.clone(), 0xF00D, Some(50));
    assert!(!rt.is_dirty(&key, 0xF00D), "unchanged fingerprint skips");
    assert_eq!(rt.wake_of(&key), Some(50));
    assert_eq!(rt.gate_stats().skips, 1);

    // State mutation -> dirty again -> same-tick re-arm.
    assert!(rt.is_dirty(&key, 0xBEEF));
    rt.commit(key.clone(), 0xBEEF, Some(10));
    assert_eq!(rt.wake_of(&key), Some(10));

    // Exact-tick wake: nothing before 10, the key at 10.
    let mut due = Vec::new();
    assert_eq!(rt.enqueue_due(9), 0);
    assert_eq!(rt.enqueue_due(10), 1);
    assert_eq!(rt.drain_backlog(usize::MAX, u64::MAX, |_| 1, &mut due).items, 1);
    assert_eq!(due, vec![key.clone()]);

    // Delta coalescing: 3 writes -> 1 outbound delta.
    rt.record_delta(key.clone(), 1);
    rt.record_delta(key.clone(), 2);
    rt.record_delta(key.clone(), 3);
    assert_eq!(rt.pending_delta_len(), 1);
    let mut deltas = Vec::new();
    rt.drain_deltas_into(&mut deltas);
    assert_eq!(deltas, vec![(key.clone(), 3)]);

    // remove() erases everything.
    rt.commit(key.clone(), 0x1, Some(99));
    rt.record_delta(key.clone(), 9);
    rt.remove(&key);
    assert_eq!(rt.wake_of(&key), None);
    assert_eq!(rt.pending_delta_len(), 0);
    assert!(rt.is_quiet());
}

#[test]
fn runtime_same_tick_wake_after_mutation() {
    // A machine committed mid-tick with `wake_at <= now` wakes on that
    // very same `tick(now)` call — the "instant same-tick wake" contract.
    let mut rt = ModTickRuntime::<u64, ()>::new();
    rt.commit(1, 0xA, Some(100));
    // Between ticks the machine is re-evaluated and re-armed for "now".
    rt.commit(1, 0xB, Some(42));
    let mut due = Vec::new();
    let drain = rt.tick(42, 64, u64::MAX, |_| 1, &mut due);
    assert_eq!(drain.items, 1);
    assert_eq!(due, vec![1]);
}

// ---------------------------------------------------------------------------
// CookingPotCluster — real culinary adoption
// ---------------------------------------------------------------------------

#[test]
fn cluster_arms_exact_tick_and_still_reports_completed_tick() {
    let reg = stew_registry(40);
    let mut cluster = CookingPotCluster::new();
    assert!(cluster.add_pot(7));
    assert!(!cluster.add_pot(7), "duplicate pot rejected");

    // Sleeping, unheated pot: no schedule.
    assert_eq!(cluster.wake_of(7), None);

    // Item change arms the recipe on the same tick it happens.
    assert!(cluster.insert_input(7, "testmod:grain", 1, &reg, 10));
    cluster.set_heated(7, true, &reg, 10);
    assert_eq!(cluster.wake_of(7), Some(50), "10 + 40 cooking ticks");

    // Before the wake tick: zero processing.
    for t in 11..50 {
        let report = cluster.tick(t, &reg, 64);
        assert_eq!(report.processed, 0, "pot sleeps until its tick ({t})");
        assert!(report.results.is_empty());
    }

    // Exact tick: completion, deterministic completed_tick.
    let report = cluster.tick(50, &reg, 64);
    assert_eq!(report.processed, 1);
    assert_eq!(report.results.len(), 1);
    let (id, result) = &report.results[0];
    assert_eq!(*id, 7);
    assert_eq!(result.completed_tick, 50);
    assert_eq!(result.result_item, "testmod:meal");
    // Single grain consumed -> pot re-sleeps.
    assert_eq!(cluster.wake_of(7), None);
    assert_eq!(cluster.scheduled_len(), 0);
}

#[test]
fn cluster_rearms_repeating_completions_on_exact_ticks() {
    let reg = stew_registry(20);
    let mut cluster = CookingPotCluster::new();
    cluster.add_pot(3);
    // Three grains -> completions at 20, 40, 60.
    assert!(cluster.insert_input(3, "testmod:grain", 3, &reg, 0));
    cluster.set_heated(3, true, &reg, 0);
    assert_eq!(cluster.wake_of(3), Some(20));

    for (tick, expected_wake) in [(20u64, 40u64), (40, 60), (60, 0)] {
        let report = cluster.tick(tick, &reg, 8);
        assert_eq!(report.results.len(), 1, "completion at {tick}");
        assert_eq!(report.results[0].1.completed_tick, tick);
        if expected_wake == 0 {
            assert_eq!(cluster.wake_of(3), None, "ingredients exhausted");
        } else {
            assert_eq!(
                cluster.wake_of(3),
                Some(expected_wake),
                "re-armed for the next cycle after {tick}"
            );
        }
    }
}

#[test]
fn cluster_thousand_idle_pots_skip_in_o1_and_wake_on_exact_tick() {
    let reg = stew_registry(40);
    let mut cluster = CookingPotCluster::new();
    for id in 0..1000u64 {
        cluster.add_pot(id);
    }

    // 1,000 no-op mutations must each be fingerprint-skipped (O(1) each,
    // zero recipe re-matches).
    let before = cluster.gate_stats();
    for id in 0..1000u64 {
        cluster.set_heated(id, false, &reg, 1); // already false -> no-op
    }
    let stats = cluster.gate_stats();
    assert_eq!(
        stats.skips - before.skips,
        1000,
        "every no-op mutation skips via the gate"
    );
    assert_eq!(stats.updates - before.updates, 0);

    // Arm all 1,000 pots for completion at tick 41 (insert at tick 1).
    for id in 0..1000u64 {
        assert!(cluster.insert_input(id, "testmod:grain", 1, &reg, 1));
        cluster.set_heated(id, true, &reg, 1);
    }
    assert_eq!(cluster.scheduled_len(), 1000);

    // Ticks 2..=40: the entire cluster sleeps — nothing processed.
    let mut total_processed = 0usize;
    for t in 2..=40 {
        total_processed += cluster.tick(t, &reg, usize::MAX).processed;
    }
    assert_eq!(total_processed, 0, "1,000 idle pots cost nothing");

    // Tick 41: all 1,000 wake on the exact recipe-completion tick.
    let report = cluster.tick(41, &reg, usize::MAX);
    assert_eq!(report.processed, 1000);
    assert_eq!(report.results.len(), 1000);
    assert!(
        report
            .results
            .iter()
            .all(|(_, r)| r.completed_tick == 41 && r.result_item == "testmod:meal"),
        "every pot completed on its scheduled tick"
    );
    assert_eq!(cluster.scheduled_len(), 0);
}

#[test]
fn cluster_budget_slices_a_completion_burst_without_jitter() {
    let reg = stew_registry(10);
    let mut cluster = CookingPotCluster::new();
    for id in 0..500u64 {
        cluster.add_pot(id);
        cluster.insert_input(id, "testmod:grain", 1, &reg, 0);
        cluster.set_heated(id, true, &reg, 0);
    }

    // 500 completions due at tick 10, budget 200/tick -> smooth slicing:
    // 200 + 200 + 100, and every result keeps its deterministic tick 10.
    let mut done = 0usize;
    let mut results = 0usize;
    for t in 10..=12 {
        let report = cluster.tick(t, &reg, 200);
        done += report.processed;
        results += report.results.len();
        for (_, r) in &report.results {
            assert_eq!(r.completed_tick, 10, "sliced work keeps due tick");
        }
    }
    assert_eq!(done, 500);
    assert_eq!(results, 500);
    assert_eq!(cluster.backlog_len(), 0, "burst fully absorbed");
}

#[test]
fn cluster_coalesces_outbound_deltas() {
    let reg = stew_registry(5);
    let mut cluster = CookingPotCluster::new();
    cluster.add_pot(1);
    cluster.add_pot(2);

    // Pot 1: several mutations between flushes -> exactly one delta.
    cluster.insert_input(1, "testmod:grain", 2, &reg, 0);
    cluster.set_heated(1, true, &reg, 0);
    cluster.set_heated(1, true, &reg, 0); // no-op: fingerprint-skipped
    // Pot 2: untouched -> still has its add_pot baseline? No deltas are
    // recorded on add; only state-changing observations record.
    cluster.set_heated(2, false, &reg, 0); // no-op: skipped, no delta

    assert_eq!(cluster.pending_delta_len(), 1, "coalesced to one delta");
    let mut deltas = Vec::new();
    cluster.drain_sync_deltas(&mut deltas);
    assert_eq!(deltas.len(), 1);
    let (id, snap) = &deltas[0];
    assert_eq!(*id, 1);
    assert_eq!(
        *snap,
        PotSyncSnapshot {
            output: None,
            progress_ticks: 0,
            next_completion_tick: Some(5),
            heated: true,
        }
    );

    // A completion also records a fresh snapshot (last-write-wins).
    let report = cluster.tick(5, &reg, 8);
    assert_eq!(report.results.len(), 1);
    let mut deltas = Vec::new();
    cluster.drain_sync_deltas(&mut deltas);
    assert_eq!(deltas.len(), 1);
    assert_eq!(deltas[0].0, 1);
    assert_eq!(
        deltas[0].1.output,
        Some(("testmod:meal".to_string(), 1)),
        "snapshot carries the new output"
    );
    assert_eq!(deltas[0].1.next_completion_tick, Some(10));
}

#[test]
fn cluster_mutation_mid_sleep_wakes_immediately() {
    let reg = stew_registry(30);
    let mut cluster = CookingPotCluster::new();
    cluster.add_pot(9);
    cluster.set_heated(9, true, &reg, 0);

    // Pot sleeps with no recipe match — no wake armed.
    for t in 1..20 {
        assert_eq!(cluster.tick(t, &reg, 8).processed, 0);
    }
    assert_eq!(cluster.wake_of(9), None);

    // Item lands at tick 20 -> armed at 20 + 30 = 50, not the next poll.
    assert!(cluster.insert_input(9, "testmod:grain", 1, &reg, 20));
    assert_eq!(cluster.wake_of(9), Some(50), "same-tick wake scheduling");
    let report = cluster.tick(50, &reg, 8);
    assert_eq!(report.results.len(), 1);
    assert_eq!(report.results[0].1.completed_tick, 50);
}

#[test]
fn cluster_remove_pot_forgets_everything() {
    let reg = stew_registry(30);
    let mut cluster = CookingPotCluster::new();
    cluster.add_pot(5);
    cluster.insert_input(5, "testmod:grain", 1, &reg, 0);
    cluster.set_heated(5, true, &reg, 0);
    assert_eq!(cluster.wake_of(5), Some(30));

    assert!(cluster.remove_pot(5));
    assert!(!cluster.remove_pot(5));
    assert_eq!(cluster.wake_of(5), None);
    assert_eq!(cluster.scheduled_len(), 0);
    assert_eq!(cluster.pending_delta_len(), 0);
    let report = cluster.tick(30, &reg, 8);
    assert_eq!(report.processed, 0, "removed pot never wakes");
}
