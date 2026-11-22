// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! Lane UX-RESILIENCE-1: crash containment (`run_contained` /
//! `run_session_contained`), the deterministic injected-clock circuit
//! breaker, and the zero-restart mod reload planner + transaction
//! rollback.

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use mc_spine_client::crash_reporter::{crash_file_path, ReportEnv};
use mc_spine_client::recovery::{
    install_panic_site_recorder, BreakerDecision, ContainmentOutcome, CrashCircuitBreaker,
    CrashGuard, MissingDep, ModEntry, ModReloadOutcome, ModReloadPlanner, ModReloadTransaction,
    RecoveryAction, ReloadStrategy, ReloadTier, DEFAULT_CRASH_WINDOW_MS,
    DEFAULT_MAX_CRASHES_IN_WINDOW, DEFAULT_STABLE_WINDOW_MS,
};

fn test_env() -> ReportEnv {
    ReportEnv {
        mc_version: "26.3".to_string(),
        engine_version: "mc-spine-client 0.1.0".to_string(),
        os: "linux-x86_64".to_string(),
        culprit: "mc-spine-world".to_string(),
        subsystem: Some("tick".to_string()),
        stage: None,
        gpu_class: None,
        top_frames: Vec::new(),
        mod_count: Some(3),
    }
}

fn scratch_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("mc-spine-recovery-{tag}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Serializes tests that install/depend on the process-global panic hook.
static HOOK_LOCK: Mutex<()> = Mutex::new(());

fn sha(tag: char) -> String {
    tag.to_string().repeat(64)
}

fn entry(id: &str, version: &str, tier: ReloadTier) -> ModEntry {
    ModEntry::new(id, version, sha('a'), tier)
}

// ---------------------------------------------------------------------------
// Crash containment
// ---------------------------------------------------------------------------

#[test]
fn contained_panic_drops_poisoned_session_and_returns_soft_restart() {
    struct Session {
        drops: Arc<AtomicUsize>,
        ticked: bool,
    }
    impl Drop for Session {
        fn drop(&mut self) {
            self.drops.fetch_add(1, Ordering::SeqCst);
        }
    }

    let dir = scratch_dir("contained");
    let mut guard = CrashGuard::new(test_env(), dir.clone());
    let drops = Arc::new(AtomicUsize::new(0));
    let session = Session {
        drops: drops.clone(),
        ticked: false,
    };

    let outcome = guard.run_session_contained(1_000, session, |s| {
        // Half-mutate the session, then blow up: the polluted object must
        // be dropped inside the boundary and never handed back.
        s.ticked = true;
        panic!("simulated tick fault");
    });

    match outcome {
        ContainmentOutcome::Completed(_) => panic!("panic must be contained, not completed"),
        ContainmentOutcome::Recovered { action, report } => {
            match action {
                RecoveryAction::InternalSoftRestart {
                    attempt,
                    crash_id,
                    blame_hint,
                } => {
                    assert_eq!(attempt, 1);
                    assert_eq!(crash_id, report.fingerprint);
                    assert_eq!(blame_hint, "mc-spine-world");
                }
                RecoveryAction::ReturnToCrashScreen { reason, .. } => {
                    panic!("first crash must soft-restart, got crash screen: {reason}")
                }
            }
            assert_eq!(report.context.error_kind, "panic");
            assert!(report
                .context
                .detail_message
                .contains("simulated tick fault"));
            assert!(report.markdown.len() <= 4096);
            // The local crash-<fp>.md was written immediately.
            let path = crash_file_path(&dir, &report.fingerprint);
            assert!(path.exists(), "missing {}", path.display());
            let written = std::fs::read_to_string(&path).unwrap();
            assert_eq!(written, report.markdown);
        }
    }

    // The moved-in session was dropped exactly once, during unwinding.
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    assert!(!guard.breaker().is_tripped());
    assert_eq!(guard.breaker().attempts(), 1);
}

#[test]
fn successful_boundary_returns_value_and_counts_healthy() {
    let dir = scratch_dir("healthy");
    let mut guard = CrashGuard::new(test_env(), dir);
    match guard.run_contained(0, || 40 + 2) {
        ContainmentOutcome::Completed(v) => assert_eq!(v, 42),
        ContainmentOutcome::Recovered { .. } => panic!("healthy boundary recovered"),
    }
}

#[test]
fn panic_site_is_recorded_when_recorder_hook_installed() {
    let _lock = HOOK_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    install_panic_site_recorder();
    let dir = scratch_dir("site");
    let mut guard = CrashGuard::new(test_env(), dir);
    let marker = "site_marker_unique_7f3c";
    let outcome = guard.run_contained(0, || panic!("{marker}"));
    match outcome {
        ContainmentOutcome::Recovered { report, .. } => {
            // The recorder supplies file:line:col, which lands in detail.
            assert!(report.context.detail_message.contains(marker));
            assert!(
                report.context.detail_message.contains("recovery_tests.rs"),
                "expected panic site in detail, got {:?}",
                report.context.detail_message
            );
        }
        ContainmentOutcome::Completed(()) => panic!("panic escaped containment"),
    }
}

#[test]
fn non_string_payload_is_contained() {
    let dir = scratch_dir("nonstr");
    let mut guard = CrashGuard::new(test_env(), dir);
    let outcome = guard.run_contained(0, || std::panic::panic_any(42u32));
    match outcome {
        ContainmentOutcome::Recovered { report, .. } => {
            assert!(
                report
                    .context
                    .detail_message
                    .starts_with("<non-string panic payload>"),
                "{:?}",
                report.context.detail_message
            );
        }
        ContainmentOutcome::Completed(()) => panic!("panic_any escaped containment"),
    }
}

// ---------------------------------------------------------------------------
// Circuit breaker (injected clock)
// ---------------------------------------------------------------------------

#[test]
fn breaker_trips_on_max_crashes_in_window() {
    let mut b = CrashCircuitBreaker::new();
    assert_eq!(b.decide(0, "FP-A"), BreakerDecision::Allow { attempt: 1 });
    assert_eq!(
        b.decide(1_000, "FP-B"),
        BreakerDecision::Allow { attempt: 2 }
    );
    match b.decide(2_000, "FP-C") {
        BreakerDecision::Tripped { reason } => {
            assert!(reason.contains("crashes within"), "{reason}");
        }
        BreakerDecision::Allow { .. } => panic!("3rd crash in window must trip"),
    }
    assert!(b.is_tripped());
    // Tripped is sticky: further crashes stay refused.
    match b.decide(3_000, "FP-D") {
        BreakerDecision::Tripped { reason } => {
            assert!(reason.contains("already tripped"), "{reason}");
        }
        BreakerDecision::Allow { .. } => panic!("tripped breaker must keep refusing"),
    }
    assert_eq!(b.attempts(), 4); // crashes still counted for diagnostics
}

#[test]
fn breaker_trips_on_identical_consecutive_fingerprints() {
    let mut b = CrashCircuitBreaker::new();
    assert_eq!(
        b.decide(0, "FP-SAME"),
        BreakerDecision::Allow { attempt: 1 }
    );
    match b.decide(5_000, "FP-SAME") {
        BreakerDecision::Tripped { reason } => {
            assert!(reason.contains("FP-SAME"), "{reason}");
            assert!(reason.contains("consecutively"), "{reason}");
        }
        BreakerDecision::Allow { .. } => panic!("2nd identical crash must trip"),
    }
}

#[test]
fn breaker_window_evicts_old_crashes() {
    let mut b = CrashCircuitBreaker::new();
    assert_eq!(b.decide(0, "FP-A"), BreakerDecision::Allow { attempt: 1 });
    assert_eq!(
        b.decide(DEFAULT_CRASH_WINDOW_MS / 2, "FP-B"),
        BreakerDecision::Allow { attempt: 2 }
    );
    // First crash fell out of the window → only 2 in window → allowed.
    assert_eq!(
        b.decide(DEFAULT_CRASH_WINDOW_MS + 1, "FP-C"),
        BreakerDecision::Allow { attempt: 3 }
    );
    assert!(!b.is_tripped());
}

#[test]
fn breaker_resets_after_stable_healthy_run() {
    let mut b = CrashCircuitBreaker::new();
    assert_eq!(b.decide(0, "FP-A"), BreakerDecision::Allow { attempt: 1 });
    assert!(!b.note_healthy(1_000)); // streak begins
    assert!(!b.note_healthy(1_000 + DEFAULT_STABLE_WINDOW_MS - 1));
    assert!(b.note_healthy(1_000 + DEFAULT_STABLE_WINDOW_MS)); // streak complete
    assert_eq!(b.attempts(), 0);
    assert!(!b.is_tripped());
    // Next crash is attempt 1 of a new epoch.
    assert_eq!(
        b.decide(1_000 + DEFAULT_STABLE_WINDOW_MS + 1, "FP-A"),
        BreakerDecision::Allow { attempt: 1 }
    );
}

#[test]
fn guard_soft_restarts_then_routes_to_crash_screen() {
    let dir = scratch_dir("guard-loop");
    let mut guard = CrashGuard::new(test_env(), dir.clone());
    for i in 0..DEFAULT_MAX_CRASHES_IN_WINDOW - 1 {
        let t = i as u64 * 100;
        match guard.run_contained(t, || panic!("distinct fault {i}")) {
            ContainmentOutcome::Recovered { action, .. } => {
                assert!(
                    matches!(action, RecoveryAction::InternalSoftRestart { .. }),
                    "crash {i} should soft-restart"
                );
            }
            ContainmentOutcome::Completed(()) => panic!("panic escaped"),
        }
    }
    // The crash that hits the limit routes to the crash screen.
    match guard.run_contained(900, || panic!("distinct fault Z")) {
        ContainmentOutcome::Recovered { action, report } => match action {
            RecoveryAction::ReturnToCrashScreen {
                crash_id,
                preview_markdown,
                reason,
            } => {
                assert_eq!(crash_id, report.fingerprint);
                assert_eq!(preview_markdown, report.markdown);
                assert!(!reason.is_empty());
                assert!(crash_file_path(&dir, &crash_id).exists());
            }
            RecoveryAction::InternalSoftRestart { .. } => {
                panic!("tripped breaker must not soft-restart")
            }
        },
        ContainmentOutcome::Completed(()) => panic!("panic escaped"),
    }
    // No infinite loop: further crashes keep routing to the screen.
    match guard.run_contained(1_000, || panic!("distinct fault W")) {
        ContainmentOutcome::Recovered { action, .. } => {
            assert!(matches!(action, RecoveryAction::ReturnToCrashScreen { .. }));
        }
        ContainmentOutcome::Completed(()) => panic!("panic escaped"),
    }
}

#[test]
fn guard_resets_breaker_after_stable_session() {
    let dir = scratch_dir("guard-reset");
    let mut guard = CrashGuard::new(test_env(), dir);
    // One absorbed crash.
    match guard.run_contained(0, || panic!("early fault")) {
        ContainmentOutcome::Recovered { .. } => {}
        ContainmentOutcome::Completed(()) => panic!("panic escaped"),
    }
    assert_eq!(guard.breaker().attempts(), 1);
    // Healthy run past the stability threshold resets the breaker.
    for t in [1_000u64, 20_000, 31_000, 32_000] {
        match guard.run_contained(t, || ()) {
            ContainmentOutcome::Completed(()) => {}
            ContainmentOutcome::Recovered { .. } => panic!("healthy boundary recovered"),
        }
    }
    assert_eq!(guard.breaker().attempts(), 0);
    // A new crash starts a fresh epoch at attempt 1.
    match guard.run_contained(33_000, || panic!("late fault")) {
        ContainmentOutcome::Recovered { action, .. } => match action {
            RecoveryAction::InternalSoftRestart { attempt, .. } => assert_eq!(attempt, 1),
            RecoveryAction::ReturnToCrashScreen { .. } => panic!("breaker should have reset"),
        },
        ContainmentOutcome::Completed(()) => panic!("panic escaped"),
    }
}

// ---------------------------------------------------------------------------
// Mod reload planner
// ---------------------------------------------------------------------------

#[test]
fn identical_snapshots_plan_no_change() {
    let planner = ModReloadPlanner::default();
    let current = vec![entry("sodium", "1.0", ReloadTier::DeclarativeHotReload)];
    let target = vec![entry("sodium", "1.0", ReloadTier::DeclarativeHotReload)];
    assert_eq!(
        planner.plan(&current, &target).strategy,
        ReloadStrategy::NoChange
    );
}

#[test]
fn declarative_only_changes_hot_reload_in_place() {
    let planner = ModReloadPlanner::default();
    let current = vec![
        entry("sodium", "1.0", ReloadTier::DeclarativeHotReload),
        entry("iris", "1.0", ReloadTier::DeclarativeHotReload),
    ];
    let mut updated_iris = entry("iris", "1.1", ReloadTier::DeclarativeHotReload);
    updated_iris.sha256 = sha('b');
    let target = vec![
        entry("sodium", "1.0", ReloadTier::DeclarativeHotReload),
        updated_iris,
        entry("megu-spells", "0.3", ReloadTier::DeclarativeHotReload),
    ];
    match planner.plan(&current, &target).strategy {
        ReloadStrategy::HotReloadInPlace {
            added,
            updated,
            removed,
        } => {
            assert_eq!(added.len(), 1);
            assert_eq!(added[0].mod_id, "megu-spells");
            assert_eq!(updated.len(), 1);
            assert_eq!(updated[0].mod_id, "iris");
            assert!(removed.is_empty());
        }
        other => panic!("declarative diff must hot-reload, got {other:?}"),
    }
}

#[test]
fn removal_only_declarative_is_hot_reload() {
    let planner = ModReloadPlanner::default();
    let current = vec![
        entry("sodium", "1.0", ReloadTier::DeclarativeHotReload),
        entry("datapack-x", "2.0", ReloadTier::DeclarativeHotReload),
    ];
    let target = vec![entry("sodium", "1.0", ReloadTier::DeclarativeHotReload)];
    match planner.plan(&current, &target).strategy {
        ReloadStrategy::HotReloadInPlace { removed, .. } => {
            assert_eq!(removed.len(), 1);
            assert_eq!(removed[0].mod_id, "datapack-x");
        }
        other => panic!("declarative removal must hot-reload, got {other:?}"),
    }
}

#[test]
fn registry_tier_change_plans_internal_soft_restart() {
    let planner = ModReloadPlanner::default();
    let current = vec![entry("sodium", "1.0", ReloadTier::DeclarativeHotReload)];
    let target = vec![
        entry("sodium", "1.0", ReloadTier::DeclarativeHotReload),
        entry("create", "2.0", ReloadTier::SessionSoftRestart),
    ];
    match planner.plan(&current, &target).strategy {
        ReloadStrategy::InternalSoftRestart {
            added,
            updated,
            removed,
            reason,
        } => {
            assert_eq!(added.len(), 1);
            assert_eq!(added[0].mod_id, "create");
            assert!(updated.is_empty() && removed.is_empty());
            assert!(reason.contains("create"), "{reason}");
            assert!(reason.contains("soft restart"), "{reason}");
        }
        other => panic!("registry-tier add must soft-restart, got {other:?}"),
    }
}

#[test]
fn soft_restart_tier_dominates_declarative_in_mixed_diff() {
    let planner = ModReloadPlanner::default();
    let current = vec![
        entry("sodium", "1.0", ReloadTier::DeclarativeHotReload),
        entry("create", "2.0", ReloadTier::SessionSoftRestart),
    ];
    let target = vec![
        entry("sodium", "1.0", ReloadTier::DeclarativeHotReload),
        entry("create", "2.1", ReloadTier::SessionSoftRestart),
        entry("megu-spells", "0.3", ReloadTier::DeclarativeHotReload),
    ];
    match planner.plan(&current, &target).strategy {
        ReloadStrategy::InternalSoftRestart { added, updated, .. } => {
            assert_eq!(added.len(), 1);
            assert_eq!(updated.len(), 1);
        }
        other => panic!("mixed diff with session-tier must soft-restart, got {other:?}"),
    }
}

#[test]
fn process_restart_tier_is_reserved_for_native_flags() {
    let planner = ModReloadPlanner::default();
    let current = vec![];
    let target = vec![
        entry("gpu-native", "9.9", ReloadTier::ProcessRestart),
        entry("create", "2.0", ReloadTier::SessionSoftRestart),
    ];
    match planner.plan(&current, &target).strategy {
        ReloadStrategy::ProcessRestartRequired { reason } => {
            assert!(reason.contains("gpu-native"), "{reason}");
        }
        other => panic!("native-tier must demand process restart, got {other:?}"),
    }
}

#[test]
fn missing_dependencies_stop_before_applying_with_modrinth_queries() {
    let planner = ModReloadPlanner {
        target_mc: "1.21.1".to_string(),
        loaders: vec!["fabric".to_string(), "neoforge".to_string()],
    };
    let mut needy = entry("create", "2.0", ReloadTier::DeclarativeHotReload);
    needy.missing_dependencies = vec![MissingDep {
        requiring_mod_id: "create".to_string(),
        required_mod_id: "flywheel".to_string(),
        version_range: Some(">=1.21.1-0.6".to_string()),
    }];
    let current = vec![];
    let target = vec![
        needy,
        entry("sodium", "1.0", ReloadTier::DeclarativeHotReload),
    ];
    match planner.plan(&current, &target).strategy {
        ReloadStrategy::MissingDependencies {
            missing,
            modrinth_queries,
        } => {
            assert_eq!(missing.len(), 1);
            assert_eq!(missing[0].required_mod_id, "flywheel");
            assert_eq!(missing[0].requiring_mod_id, "create");
            assert_eq!(modrinth_queries.len(), 1);
            let q = &modrinth_queries[0];
            assert!(
                q.starts_with("https://api.modrinth.com/v2/project/flywheel/version?"),
                "{q}"
            );
            assert!(q.contains("game_versions=%5B%221.21.1%22%5D"), "{q}");
            assert!(
                q.contains("loaders=%5B%22fabric%22%2C%22neoforge%22%5D"),
                "{q}"
            );
        }
        other => panic!("missing dep must stop the plan, got {other:?}"),
    }
}

#[test]
fn plan_is_deterministic_regardless_of_input_order() {
    let planner = ModReloadPlanner::default();
    let current = vec![
        entry("sodium", "1.0", ReloadTier::DeclarativeHotReload),
        entry("iris", "1.0", ReloadTier::DeclarativeHotReload),
    ];
    let forward = vec![
        entry("create", "2.0", ReloadTier::SessionSoftRestart),
        entry("sodium", "1.0", ReloadTier::DeclarativeHotReload),
        entry("megu-spells", "0.3", ReloadTier::DeclarativeHotReload),
    ];
    let mut reversed = forward.clone();
    reversed.reverse();
    assert_eq!(
        planner.plan(&current, &forward),
        planner.plan(&current, &reversed)
    );
}

// ---------------------------------------------------------------------------
// Mod reload transaction
// ---------------------------------------------------------------------------

#[test]
fn transaction_commits_staged_set() {
    let current = vec![
        entry("sodium", "1.0", ReloadTier::DeclarativeHotReload),
        entry("iris", "1.0", ReloadTier::DeclarativeHotReload),
    ];
    let add = entry("megu-spells", "0.3", ReloadTier::DeclarativeHotReload);
    let remove = current[1].clone();
    let mut tx = ModReloadTransaction::begin(&current);
    tx.stage(std::slice::from_ref(&add), &[], &[remove]);
    assert_eq!(
        tx.staged()
            .iter()
            .map(|m| m.mod_id.as_str())
            .collect::<Vec<_>>(),
        vec!["megu-spells", "sodium"]
    );
    match tx.commit(|staged| {
        assert_eq!(staged.len(), 2);
        Ok(())
    }) {
        ModReloadOutcome::Committed { active } => {
            assert_eq!(active[0].mod_id, "megu-spells");
            assert_eq!(active[1].mod_id, "sodium");
        }
        ModReloadOutcome::RolledBack { reason, .. } => {
            panic!("clean apply must commit: {reason}")
        }
    }
}

#[test]
fn transaction_rolls_back_on_apply_error() {
    let current = vec![entry("sodium", "1.0", ReloadTier::DeclarativeHotReload)];
    let add = entry("megu-spells", "0.3", ReloadTier::DeclarativeHotReload);
    let mut tx = ModReloadTransaction::begin(&current);
    tx.stage(&[add], &[], &[]);
    match tx.commit(|_| Err("mods/ is read-only".to_string())) {
        ModReloadOutcome::RolledBack { active, reason } => {
            assert_eq!(reason, "mods/ is read-only");
            assert_eq!(active, current); // exact previous set, untouched
        }
        ModReloadOutcome::Committed { .. } => panic!("failed apply must roll back"),
    }
}

#[test]
fn transaction_rolls_back_on_apply_panic() {
    let current = vec![entry("sodium", "1.0", ReloadTier::DeclarativeHotReload)];
    let add = entry("megu-spells", "0.3", ReloadTier::DeclarativeHotReload);
    let mut tx = ModReloadTransaction::begin(&current);
    tx.stage(&[add], &[], &[]);
    let applied = Arc::new(AtomicUsize::new(0));
    let applied2 = applied.clone();
    match tx.commit(move |_| {
        applied2.fetch_add(1, Ordering::SeqCst);
        panic!("installer exploded");
    }) {
        ModReloadOutcome::RolledBack { active, reason } => {
            assert!(reason.contains("installer exploded"), "{reason}");
            assert_eq!(active, current);
        }
        ModReloadOutcome::Committed { .. } => panic!("panicking apply must roll back"),
    }
    assert_eq!(applied.load(Ordering::SeqCst), 1);
}

#[test]
fn transaction_rejects_dep_incomplete_stage_before_apply() {
    let current = vec![entry("sodium", "1.0", ReloadTier::DeclarativeHotReload)];
    let mut needy = entry("create", "2.0", ReloadTier::SessionSoftRestart);
    needy.missing_dependencies = vec![MissingDep {
        requiring_mod_id: "create".to_string(),
        required_mod_id: "flywheel".to_string(),
        version_range: None,
    }];
    let mut tx = ModReloadTransaction::begin(&current);
    tx.stage(&[needy], &[], &[]);
    let ran = Arc::new(AtomicUsize::new(0));
    let ran2 = ran.clone();
    match tx.commit(move |_| {
        ran2.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }) {
        ModReloadOutcome::RolledBack { active, reason } => {
            assert!(reason.contains("validation failed"), "{reason}");
            assert!(reason.contains("flywheel"), "{reason}");
            assert_eq!(active, current);
        }
        ModReloadOutcome::Committed { .. } => {
            panic!("dep-incomplete stage must never commit")
        }
    }
    // Apply is never invoked for an invalid staged set.
    assert_eq!(ran.load(Ordering::SeqCst), 0);
}

#[test]
fn transaction_rejects_malformed_sha256() {
    let current = vec![entry("sodium", "1.0", ReloadTier::DeclarativeHotReload)];
    let bad = ModEntry::new(
        "megu-spells",
        "0.3",
        "not-hex",
        ReloadTier::DeclarativeHotReload,
    );
    let mut tx = ModReloadTransaction::begin(&current);
    tx.stage(&[bad], &[], &[]);
    match tx.commit(|_| Ok(())) {
        ModReloadOutcome::RolledBack { reason, .. } => {
            assert!(reason.contains("sha256"), "{reason}");
        }
        ModReloadOutcome::Committed { .. } => panic!("bad digest must roll back"),
    }
}
