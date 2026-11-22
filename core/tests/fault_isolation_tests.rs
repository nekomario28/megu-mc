// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! Unit tests for `mc_spine_core::fault_isolation`.

use mc_spine_core::crash_report::CrashContext;
use mc_spine_core::fault_isolation::{
    CircuitState, ExecutionPermit, FaultIsolationConfig, FaultIsolationRouter, IsolationReason,
    SubsystemScope,
};
use mc_spine_core::version::TargetVersion;

fn mod_scope(name: &str) -> SubsystemScope {
    SubsystemScope::ModNamespace(name.to_string())
}

fn chunk_scope(dim: &str, cx: i32, cz: i32) -> SubsystemScope {
    SubsystemScope::ChunkRegion {
        dimension: dim.to_string(),
        cx,
        cz,
    }
}

/// Every `OpenIsolated` must carry a well-formed `MEGU-E-XXXXXXXX` blame ID.
fn assert_megu_fingerprint(fp: &str) {
    assert!(
        fp.starts_with("MEGU-E-"),
        "fingerprint {fp:?} missing MEGU-E- prefix"
    );
    assert_eq!(fp.len(), "MEGU-E-".len() + 8, "bad fingerprint length");
    assert!(
        fp["MEGU-E-".len()..]
            .chars()
            .all(|c| c.is_ascii_digit() || ('A'..='F').contains(&c)),
        "fingerprint {fp:?} is not 8 uppercase hex digits"
    );
}

fn open_isolated(state: &CircuitState) -> (u64, &IsolationReason, &str) {
    match state {
        CircuitState::OpenIsolated {
            until_tick,
            reason,
            fingerprint,
        } => (*until_tick, reason, fingerprint.as_str()),
        other => panic!("expected OpenIsolated, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// 1. Healthy scopes
// ---------------------------------------------------------------------------

#[test]
fn healthy_scope_stays_closed_and_one_tick_noise_never_trips() {
    let mut router = FaultIsolationRouter::default();
    let scope = mod_scope("farm_helper");

    // Two hundred clean ticks: always allowed, always Closed, never tracked.
    for tick in 0..200 {
        assert_eq!(router.should_execute(&scope, tick), ExecutionPermit::Allow);
        assert_eq!(
            router.record_success(&scope, tick, 3_000),
            CircuitState::Closed
        );
    }
    assert_eq!(
        router.tracked_scope_count(),
        0,
        "always-healthy scopes must not occupy table slots"
    );

    // Two consecutive overruns (< throttle strikes) must not trip, and a
    // clean tick fully resets the streak — a further single overrun is
    // strike 1 again, not strike 4.
    assert_eq!(
        router.record_success(&scope, 200, 12_000),
        CircuitState::Closed
    );
    assert_eq!(
        router.record_success(&scope, 201, 12_500),
        CircuitState::Closed
    );
    assert_eq!(
        router.record_success(&scope, 202, 1_000),
        CircuitState::Closed
    );
    assert_eq!(
        router.record_success(&scope, 203, 12_000),
        CircuitState::Closed,
        "strike counter must reset on a clean tick"
    );
    assert_eq!(router.should_execute(&scope, 204), ExecutionPermit::Allow);
    assert!(router.isolated_scopes().is_empty());
}

// ---------------------------------------------------------------------------
// 2. Sustained overruns: Closed -> Throttled -> OpenIsolated
// ---------------------------------------------------------------------------

#[test]
fn sustained_overruns_escalate_throttle_then_isolate_with_fingerprint() {
    let mut router = FaultIsolationRouter::default();
    let scope = mod_scope("lag_machine");

    // Strikes 1 and 2 stay Closed — the throttle only engages at strike 3.
    assert_eq!(
        router.record_success(&scope, 0, 12_000),
        CircuitState::Closed
    );
    assert_eq!(
        router.record_success(&scope, 1, 12_000),
        CircuitState::Closed
    );

    // Strike 3 -> Throttled with a real stride and a bounded window.
    let (stride, _until) = match router.record_success(&scope, 2, 12_000) {
        CircuitState::Throttled { stride, until_tick } => {
            assert!(stride >= 2, "throttle stride must skip ticks");
            assert!(until_tick > 2, "throttle window must reach forward");
            (stride, until_tick)
        }
        other => panic!("expected Throttled at strike 3, got {other:?}"),
    };

    // Over one full stride cycle exactly one tick is permitted; the rest
    // are SkipThrottled.
    let mut allowed = 0_u32;
    let mut throttled = 0_u32;
    for t in 3..3 + u64::from(stride) {
        match router.should_execute(&scope, t) {
            ExecutionPermit::Allow => allowed += 1,
            ExecutionPermit::SkipThrottled => throttled += 1,
            other => panic!("throttled scope returned {other:?}"),
        }
    }
    assert_eq!(allowed, 1, "stride {stride} must allow exactly 1 tick");
    assert_eq!(throttled, stride - 1);

    // Keep overrunning on the ticks the throttle permits: strikes climb
    // past the throttle threshold until the isolate limit (8) isolates.
    let mut t = 3_u64;
    let isolated_state = loop {
        if router.should_execute(&scope, t) == ExecutionPermit::Allow {
            let st = router.record_success(&scope, t, 12_000);
            if matches!(st, CircuitState::OpenIsolated { .. }) {
                break st;
            }
        }
        t += 1;
        assert!(t < 10_000, "sustained overruns never isolated");
    };

    let (until_tick, reason, fingerprint) = open_isolated(&isolated_state);
    assert_megu_fingerprint(fingerprint);
    assert!(
        until_tick > t,
        "isolation window must reach forward from the fault"
    );
    match reason {
        IsolationReason::SustainedTickBudgetOverrun {
            consecutive_overruns,
            last_elapsed_us,
            budget_us,
        } => {
            assert_eq!(*consecutive_overruns, 8);
            assert_eq!(*last_elapsed_us, 12_000);
            assert_eq!(*budget_us, 8_000);
        }
        other => panic!("expected SustainedTickBudgetOverrun, got {other:?}"),
    }

    // The skip permit surfaces the same blame fingerprint.
    match router.should_execute(&scope, t + 1) {
        ExecutionPermit::SkipIsolated { fingerprint: fp } => {
            assert_eq!(fp, fingerprint)
        }
        other => panic!("isolated scope returned {other:?}"),
    }
}

#[test]
fn two_hard_stalls_isolate_immediately() {
    let mut router = FaultIsolationRouter::default();
    let scope = SubsystemScope::BuiltinSubsystem("lighting".to_string());

    // One 50 ms stall is a bad tick and stays Closed; the second means
    // wedged, long before the 8-strike overrun path could engage.
    assert_eq!(
        router.record_success(&scope, 0, 50_000),
        CircuitState::Closed
    );
    let state = router.record_success(&scope, 1, 55_000);
    let (_, reason, fingerprint) = open_isolated(&state);
    assert_megu_fingerprint(fingerprint);
    match reason {
        IsolationReason::SustainedTickBudgetOverrun {
            consecutive_overruns,
            ..
        } => assert_eq!(*consecutive_overruns, 2),
        other => panic!("expected SustainedTickBudgetOverrun, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// 3. Panic isolates only the offender
// ---------------------------------------------------------------------------

#[test]
fn single_panic_isolates_only_the_offending_scope() {
    let mut router = FaultIsolationRouter::default();
    let bad = mod_scope("create");
    let neighbors = [
        mod_scope("mystic_addon"),
        chunk_scope("minecraft:overworld", 4, -9),
        SubsystemScope::Contraption(77),
        SubsystemScope::BuiltinSubsystem("redstone".to_string()),
    ];

    let state = router.record_panic(&bad, 100, "index out of bounds in tick");
    let (until_tick, reason, fingerprint) = open_isolated(&state);
    assert_megu_fingerprint(fingerprint);
    assert_eq!(until_tick, 100 + 200, "default initial cooldown is 200");
    match reason {
        IsolationReason::PanicCaught { summary } => {
            assert!(summary.contains("index out of bounds"));
        }
        other => panic!("expected PanicCaught, got {other:?}"),
    }

    match router.should_execute(&bad, 101) {
        ExecutionPermit::SkipIsolated { fingerprint: fp } => {
            assert_eq!(fp, fingerprint)
        }
        other => panic!("panicking scope returned {other:?}"),
    }

    // Every other scope — all four scope kinds — is completely unaffected.
    for scope in &neighbors {
        assert_eq!(
            router.should_execute(scope, 101),
            ExecutionPermit::Allow,
            "{scope:?} must be unaffected by another scope's panic"
        );
    }

    let isolated = router.isolated_scopes();
    assert_eq!(isolated.len(), 1);
    assert_eq!(isolated[0].0, bad);
}

#[test]
fn panic_fingerprint_matches_crash_report_blame_id() {
    let mut router = FaultIsolationRouter::default();
    let scope = mod_scope("create");
    let summary = "index out of bounds in tick";

    let state = router.record_panic(&scope, 0, summary);
    let (_, _, fingerprint) = open_isolated(&state);

    // The router must mint the exact CrashContext blame ID for this
    // scope/fault so operator surfaces deduplicate correctly.
    let expected =
        CrashContext::new("panic", "mod:create", TargetVersion::LATEST.id(), summary).fingerprint();
    assert_eq!(fingerprint, expected);
}

// ---------------------------------------------------------------------------
// 4. Half-open probe + exponential backoff
// ---------------------------------------------------------------------------

#[test]
fn half_open_probe_closes_on_success_and_backs_off_on_failure() {
    let mut router = FaultIsolationRouter::new(FaultIsolationConfig {
        initial_cooldown_ticks: 10,
        max_cooldown_ticks: 40,
        ..FaultIsolationConfig::default()
    });
    let scope = mod_scope("flaky_mod");

    router.record_panic(&scope, 0, "boom");

    // Cooldown 10: ticks 1..=9 are hard skips.
    for t in 1..10 {
        match router.should_execute(&scope, t) {
            ExecutionPermit::SkipIsolated { fingerprint } => assert_megu_fingerprint(&fingerprint),
            other => panic!("tick {t}: expected SkipIsolated, got {other:?}"),
        }
    }

    // Tick 10: cooldown expired -> exactly one guarded probe.
    assert_eq!(
        router.should_execute(&scope, 10),
        ExecutionPermit::AllowHalfOpenProbe
    );
    // No second probe is permitted while the first is in flight.
    assert!(matches!(
        router.should_execute(&scope, 11),
        ExecutionPermit::SkipIsolated { .. }
    ));

    // Failing probe -> re-isolated with doubled cooldown (20).
    let state = router.record_error(&scope, 11, "still broken");
    let (until_tick, reason, _) = open_isolated(&state);
    assert_eq!(until_tick, 11 + 20);
    assert!(matches!(
        reason,
        IsolationReason::RepeatedRuntimeErrors { .. }
    ));

    // Probe at 31, fail again -> cooldown 40.
    assert_eq!(
        router.should_execute(&scope, 31),
        ExecutionPermit::AllowHalfOpenProbe
    );
    let state = router.record_panic(&scope, 31, "boom again");
    let (until_tick, _, _) = open_isolated(&state);
    assert_eq!(until_tick, 31 + 40);

    // Probe at 71, fail again -> 80 would exceed the cap; clamps to 40.
    assert_eq!(
        router.should_execute(&scope, 71),
        ExecutionPermit::AllowHalfOpenProbe
    );
    let state = router.record_error(&scope, 71, "nope");
    let (until_tick, _, _) = open_isolated(&state);
    assert_eq!(
        until_tick,
        71 + 40,
        "backoff must cap at max_cooldown_ticks"
    );

    // Probe at 111, clean -> circuit closes and backoff resets.
    assert_eq!(
        router.should_execute(&scope, 111),
        ExecutionPermit::AllowHalfOpenProbe
    );
    assert_eq!(
        router.record_success(&scope, 111, 1_000),
        CircuitState::Closed
    );
    assert_eq!(router.should_execute(&scope, 112), ExecutionPermit::Allow);

    // A fresh isolation after recovery starts from the initial cooldown
    // again, not the grown one.
    let state = router.record_panic(&scope, 120, "new boom");
    let (until_tick, _, _) = open_isolated(&state);
    assert_eq!(until_tick, 120 + 10);
}

#[test]
fn throttle_heals_back_to_closed_on_a_clean_tick() {
    let mut router = FaultIsolationRouter::default();
    let scope = mod_scope("bursty_mod");

    for t in 0..3 {
        router.record_success(&scope, t, 12_000);
    }
    assert!(matches!(
        router.circuit_state(&scope),
        CircuitState::Throttled { .. }
    ));

    // One clean run on a permitted tick heals the circuit fully.
    let permitted = (3..100)
        .find(|&t| router.should_execute(&scope, t) == ExecutionPermit::Allow)
        .expect("throttled scope must still get stride ticks");
    assert_eq!(
        router.record_success(&scope, permitted, 1_000),
        CircuitState::Closed
    );
    assert_eq!(
        router.should_execute(&scope, permitted + 1),
        ExecutionPermit::Allow
    );
}

#[test]
fn lost_probe_rearms_instead_of_stranding_the_scope() {
    let mut router = FaultIsolationRouter::new(FaultIsolationConfig {
        initial_cooldown_ticks: 10,
        ..FaultIsolationConfig::default()
    });
    let scope = mod_scope("died_mid_probe");

    router.record_panic(&scope, 0, "boom");
    assert_eq!(
        router.should_execute(&scope, 10),
        ExecutionPermit::AllowHalfOpenProbe
    );
    // The runner took the probe permit but never reported a result. A
    // second ask inside the cooldown still skips...
    assert!(matches!(
        router.should_execute(&scope, 19),
        ExecutionPermit::SkipIsolated { .. }
    ));
    // ...but once cooldown_ticks elapse the router re-arms one probe
    // rather than leaving the scope in limbo forever.
    assert_eq!(
        router.should_execute(&scope, 20),
        ExecutionPermit::AllowHalfOpenProbe
    );
    assert_eq!(
        router.record_success(&scope, 20, 1_000),
        CircuitState::Closed
    );
}

#[test]
fn errors_isolate_after_configured_strikes() {
    let mut router = FaultIsolationRouter::new(FaultIsolationConfig {
        error_isolate_strikes: 3,
        ..FaultIsolationConfig::default()
    });
    let scope = mod_scope("noisy_mod");

    assert_eq!(
        router.record_error(&scope, 0, "transient 1"),
        CircuitState::Closed
    );
    assert_eq!(
        router.record_error(&scope, 1, "transient 2"),
        CircuitState::Closed
    );
    let state = router.record_error(&scope, 2, "third strike");
    let (_, reason, fingerprint) = open_isolated(&state);
    assert_megu_fingerprint(fingerprint);
    match reason {
        IsolationReason::RepeatedRuntimeErrors {
            error_count,
            last_error,
        } => {
            assert_eq!(*error_count, 3);
            assert_eq!(last_error, "third strike");
        }
        other => panic!("expected RepeatedRuntimeErrors, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// 5. Operator quarantine / release
// ---------------------------------------------------------------------------

#[test]
fn operator_quarantine_and_release_behave_cleanly() {
    let mut router = FaultIsolationRouter::default();
    let scope = mod_scope("suspect_mod");

    let state = router.manual_quarantine(&scope, 50);
    let (until_tick, reason, fingerprint) = open_isolated(&state);
    assert_eq!(until_tick, 50 + 200);
    assert_eq!(*reason, IsolationReason::OperatorManualQuarantine);
    assert_megu_fingerprint(fingerprint);

    assert!(matches!(
        router.should_execute(&scope, 51),
        ExecutionPermit::SkipIsolated { .. }
    ));
    assert_eq!(router.isolated_scopes().len(), 1);

    router.manual_release(&scope);
    assert_eq!(router.circuit_state(&scope), CircuitState::Closed);
    assert_eq!(router.should_execute(&scope, 52), ExecutionPermit::Allow);
    assert!(router.isolated_scopes().is_empty());

    // Releasing a scope the router never tracked is a harmless no-op.
    router.manual_release(&mod_scope("never_seen"));
    assert_eq!(router.tracked_scope_count(), 1);
}

#[test]
fn isolated_scopes_listing_is_sorted_deterministically() {
    let mut router = FaultIsolationRouter::default();
    // Insert in deliberately non-sorted order.
    let scopes = [
        SubsystemScope::BuiltinSubsystem("entity_ai".to_string()),
        mod_scope("zz_mod"),
        SubsystemScope::Contraption(9),
        chunk_scope("minecraft:nether", -3, 7),
        mod_scope("aa_mod"),
    ];
    for (i, scope) in scopes.iter().enumerate() {
        router.record_panic(scope, i as u64, "boom");
    }

    let first = router.isolated_scopes();
    let second = router.isolated_scopes();
    assert_eq!(first, second, "listing must be deterministic");
    let got: Vec<SubsystemScope> = first.iter().map(|(s, _)| s.clone()).collect();
    let mut expected = got.clone();
    expected.sort();
    assert_eq!(got, expected, "scopes must arrive sorted");
    assert_eq!(first.len(), scopes.len());
}

// ---------------------------------------------------------------------------
// 6. Scope flood: bounded memory, isolations never evicted
// ---------------------------------------------------------------------------

#[test]
fn scope_flood_evicts_idle_healthy_but_never_isolated_scopes() {
    const CAP: usize = 16;
    let mut router = FaultIsolationRouter::new(FaultIsolationConfig {
        max_tracked_scopes: CAP,
        ..FaultIsolationConfig::default()
    });

    // Pin three live isolations into the table.
    let pinned: Vec<SubsystemScope> = (0..3)
        .map(|i| mod_scope(&format!("bad_actor_{i}")))
        .collect();
    for scope in &pinned {
        router.record_panic(scope, 0, "boom");
    }

    // Flood far more scopes than the table can hold. Each records a
    // single sub-strike error, so it is tracked but healthy (Closed)
    // and therefore evictable.
    for i in 0..1_000_u32 {
        router.record_error(&mod_scope(&format!("noise_{i}")), 1, "transient");
        assert!(
            router.tracked_scope_count() <= CAP,
            "tracked scopes exceeded the configured bound"
        );
    }
    assert_eq!(router.tracked_scope_count(), CAP);

    // All three isolations survived the flood intact.
    let isolated = router.isolated_scopes();
    assert_eq!(isolated.len(), 3);
    for scope in &pinned {
        assert!(
            isolated.iter().any(|(s, _)| s == scope),
            "isolated scope {scope:?} was evicted by the flood"
        );
        match router.should_execute(scope, 2) {
            ExecutionPermit::SkipIsolated { fingerprint } => assert_megu_fingerprint(&fingerprint),
            other => panic!("pinned isolation returned {other:?}"),
        }
    }

    // Evicted noise scopes are simply untracked again: allowed, and any
    // new record can re-track them.
    assert_eq!(
        router.should_execute(&mod_scope("noise_0"), 2),
        ExecutionPermit::Allow
    );
    assert_eq!(
        router.record_error(&mod_scope("noise_0"), 2, "again"),
        CircuitState::Closed
    );
    assert!(router.tracked_scope_count() <= CAP);
}
