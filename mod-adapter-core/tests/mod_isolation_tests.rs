// SPDX-License-Identifier: LGPL-3.0-or-later
//! Unit tests for `mod_adapter_core::mod_isolation`.

use std::fs;
use std::path::{Path, PathBuf};

use mod_adapter_core::mod_isolation::{
    FaultOutcome, ModFaultKind, ModQuarantineConfig, ModQuarantineSupervisor, QuarantineError,
    RestoreReport, SandboxModState, SkipReason, SnapshotTrigger, TimeMachineConfig,
    TimeMachineError, ToastChoice, WorldTimeMachine,
};

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

// ---------------------------------------------------------------------------
//  ModQuarantineSupervisor
// ---------------------------------------------------------------------------

#[test]
fn registered_mod_is_active_and_running() {
    let mut sup = ModQuarantineSupervisor::default();
    sup.register_mod("create", 1);
    assert_eq!(sup.mod_state("create"), Some(SandboxModState::Active));
    assert!(sup.is_running("create"));
    assert_eq!(sup.tracked_mod_count(), 1);
    // Re-registering is idempotent.
    sup.register_mod("create", 5);
    assert_eq!(sup.mod_state("create"), Some(SandboxModState::Active));
}

#[test]
fn fuel_exhaustion_takes_three_strikes_then_quarantines_only_that_mod() {
    let mut sup = ModQuarantineSupervisor::default();
    sup.register_mod("naughty", 0);
    sup.register_mod("well_behaved", 0);

    for (tick, want) in [(1u64, 1u32), (2, 2)] {
        match sup.record_fault("naughty", ModFaultKind::FuelExhausted, tick, "fuel") {
            FaultOutcome::Degraded {
                strikes,
                max_strikes,
            } => {
                assert_eq!(strikes, want);
                assert_eq!(max_strikes, 3);
            }
            other => panic!("expected Degraded, got {other:?}"),
        }
        assert_eq!(
            sup.mod_state("naughty"),
            Some(SandboxModState::Degraded { strikes: want })
        );
    }

    let outcome = sup.record_fault("naughty", ModFaultKind::FuelExhausted, 3, "fuel exhausted");
    let toast = match outcome {
        FaultOutcome::Quarantined(t) => t,
        other => panic!("expected Quarantined, got {other:?}"),
    };
    match sup.mod_state("naughty") {
        Some(SandboxModState::Quarantined { reason, tick, .. }) => {
            assert_eq!(reason, ModFaultKind::FuelExhausted);
            assert_eq!(tick, 3);
        }
        other => panic!("expected Quarantined state, got {other:?}"),
    }
    // The healthy neighbor is untouched.
    assert_eq!(sup.mod_state("well_behaved"), Some(SandboxModState::Active));
    assert!(sup.is_running("well_behaved"));
    assert!(!sup.is_running("naughty"));

    assert_megu_fingerprint(&toast.fingerprint);
    assert_eq!(toast.mod_id, "naughty");
    assert!(toast.title_en.contains("naughty"));
    assert!(toast.title_ja.contains("naughty"));
    assert!(!toast.summary_en.is_empty());
    assert!(!toast.summary_ja.is_empty());
    assert_eq!(
        toast.choices,
        vec![
            ToastChoice::ViewLog,
            ToastChoice::KeepDisabled,
            ToastChoice::RollbackPreModSnapshot
        ]
    );
    // The same toast is also queued for the UI poller.
    let toasts = sup.drain_toasts();
    assert_eq!(toasts, vec![toast]);
    assert!(sup.drain_toasts().is_empty());
}

#[test]
fn hard_faults_quarantine_immediately() {
    for kind in [
        ModFaultKind::TrapOrPanic,
        ModFaultKind::MemoryLimitExceeded,
        ModFaultKind::DisallowedHostCall,
    ] {
        let mut sup = ModQuarantineSupervisor::default();
        sup.register_mod("m", 0);
        match sup.record_fault("m", kind, 10, "boom") {
            FaultOutcome::Quarantined(t) => assert_eq!(t.reason, kind),
            other => panic!("{kind:?}: expected Quarantined, got {other:?}"),
        }
        assert!(matches!(
            sup.mod_state("m"),
            Some(SandboxModState::Quarantined { .. })
        ));
    }
}

#[test]
fn repeated_timeout_uses_its_own_transient_budget() {
    let mut sup = ModQuarantineSupervisor::default();
    sup.register_mod("slow", 0);
    for tick in 1..=2 {
        assert!(matches!(
            sup.record_fault("slow", ModFaultKind::RepeatedTimeout, tick, "late"),
            FaultOutcome::Degraded { .. }
        ));
    }
    assert!(matches!(
        sup.record_fault("slow", ModFaultKind::RepeatedTimeout, 3, "late"),
        FaultOutcome::Quarantined(_)
    ));
}

#[test]
fn healthy_ticks_decay_transient_strikes_back_to_active() {
    let config = ModQuarantineConfig {
        healthy_ticks_per_decay: 2,
        ..Default::default()
    };
    let mut sup = ModQuarantineSupervisor::new(config);
    sup.register_mod("flaky", 0);
    sup.record_fault("flaky", ModFaultKind::FuelExhausted, 1, "x");
    sup.record_fault("flaky", ModFaultKind::FuelExhausted, 2, "x");
    assert_eq!(
        sup.mod_state("flaky"),
        Some(SandboxModState::Degraded { strikes: 2 })
    );

    sup.record_healthy_tick("flaky", 3); // 1 of 2 — no decay yet
    assert_eq!(
        sup.mod_state("flaky"),
        Some(SandboxModState::Degraded { strikes: 2 })
    );
    sup.record_healthy_tick("flaky", 4); // decay one strike
    assert_eq!(
        sup.mod_state("flaky"),
        Some(SandboxModState::Degraded { strikes: 1 })
    );
    sup.record_healthy_tick("flaky", 5);
    sup.record_healthy_tick("flaky", 6);
    assert_eq!(sup.mod_state("flaky"), Some(SandboxModState::Active));
    // A quarantined mod does not decay out — it stays contained.
    sup.record_fault("flaky", ModFaultKind::TrapOrPanic, 7, "panic");
    for tick in 8..20 {
        sup.record_healthy_tick("flaky", tick);
    }
    assert!(matches!(
        sup.mod_state("flaky"),
        Some(SandboxModState::Quarantined { .. })
    ));
}

#[test]
fn fault_on_contained_mod_is_idempotent_no_duplicate_toast() {
    let mut sup = ModQuarantineSupervisor::default();
    sup.register_mod("m", 0);
    sup.record_fault("m", ModFaultKind::TrapOrPanic, 1, "panic");
    assert_eq!(
        sup.record_fault("m", ModFaultKind::TrapOrPanic, 2, "panic again"),
        FaultOutcome::AlreadyContained
    );
    assert_eq!(sup.drain_toasts().len(), 1);
}

#[test]
fn disable_and_re_enable_round_trip() {
    let mut sup = ModQuarantineSupervisor::default();
    sup.register_mod("m", 0);
    sup.record_fault("m", ModFaultKind::TrapOrPanic, 1, "panic");
    sup.disable_mod("m", 2).unwrap();
    assert_eq!(sup.mod_state("m"), Some(SandboxModState::DisabledByUser));
    assert_eq!(sup.contained_mods(), vec!["m".to_string()]);

    assert_eq!(sup.re_enable_mod("m", 3).unwrap(), SandboxModState::Active);
    assert!(sup.is_running("m"));
    assert!(sup.contained_mods().is_empty());
    assert_eq!(
        sup.re_enable_mod("ghost", 4),
        Err(QuarantineError::UnknownMod("ghost".to_string()))
    );
    assert_eq!(
        sup.disable_mod("ghost", 4),
        Err(QuarantineError::UnknownMod("ghost".to_string()))
    );
}

#[test]
fn toast_detail_is_redacted_single_line_and_capped() {
    let mut sup = ModQuarantineSupervisor::default();
    sup.register_mod("m", 0);
    let scary = format!(
        "panic at /home/alice/secret\nstack: {:?}",
        "x".repeat(2_000)
    );
    let FaultOutcome::Quarantined(toast) =
        sup.record_fault("m", ModFaultKind::TrapOrPanic, 1, &scary)
    else {
        panic!("expected quarantine");
    };
    assert!(!toast.detail.contains('\n'));
    assert!(!toast.detail.contains("/home/alice"));
    assert!(toast.detail.contains("[REDACTED]"));
    assert!(toast.detail.len() < 400);
}

#[test]
fn unregistered_mod_fault_still_quarantines_fail_closed() {
    let mut sup = ModQuarantineSupervisor::default();
    // No register_mod call — a fault report alone must still contain it.
    assert!(matches!(
        sup.record_fault(
            "sneaky",
            ModFaultKind::DisallowedHostCall,
            1,
            "write to host fs"
        ),
        FaultOutcome::Quarantined(_)
    ));
    assert!(!sup.is_running("sneaky"));
}

#[test]
fn tracked_table_is_bounded_and_never_evicts_quarantined() {
    let config = ModQuarantineConfig {
        max_tracked_mods: 4,
        ..Default::default()
    };
    let mut sup = ModQuarantineSupervisor::new(config);
    sup.register_mod("bad", 0);
    sup.record_fault("bad", ModFaultKind::TrapOrPanic, 1, "panic");
    for i in 0..3 {
        sup.register_mod(&format!("healthy{i}"), 10 + i);
    }
    assert_eq!(sup.tracked_mod_count(), 4);
    // One more registration evicts the oldest Active record, never the
    // quarantined one.
    sup.register_mod("newcomer", 20);
    assert_eq!(sup.tracked_mod_count(), 4);
    assert!(matches!(
        sup.mod_state("bad"),
        Some(SandboxModState::Quarantined { .. })
    ));
    assert_eq!(sup.mod_state("healthy0"), None);
}

// ---------------------------------------------------------------------------
//  WorldTimeMachine
// ---------------------------------------------------------------------------

/// Unique scratch root per test so parallel `cargo test` runs never share
/// directories.
fn scratch(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "megu-mi-{}-{}-{}",
        tag,
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn write(path: &Path, bytes: &[u8]) {
    if let Some(p) = path.parent() {
        fs::create_dir_all(p).unwrap();
    }
    fs::write(path, bytes).unwrap();
}

fn tm(root: &Path, config: TimeMachineConfig) -> WorldTimeMachine {
    WorldTimeMachine::new(root.join("store"), config)
}

#[test]
fn snapshot_capture_dedups_unchanged_blobs() {
    let root = scratch("dedup");
    let world = root.join("world");
    write(&world.join("level.dat"), b"level-one");
    write(&world.join("region/r.0.0.mca"), &[7u8; 10_000]);
    let mut m = tm(&root, TimeMachineConfig::default());

    let s1 = m
        .create_snapshot(&world, SnapshotTrigger::ModAdded, 1_000)
        .unwrap();
    assert!(s1.id.starts_with("snap-"));
    assert_eq!(s1.files.len(), 2);
    assert_eq!(s1.total_bytes, 10_000 + 9);
    let objects = |m: &WorldTimeMachine| -> Vec<String> {
        let mut v: Vec<String> = fs::read_dir(m.store_dir().join("objects"))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        v.sort();
        v
    };
    let first_objs = objects(&m);
    assert_eq!(first_objs.len(), 2);
    assert!(first_objs.iter().all(|n| n.len() == 64));

    // Change one file, snapshot again: only one new blob.
    write(&world.join("level.dat"), b"level-two");
    let s2 = m
        .create_snapshot(&world, SnapshotTrigger::ModUpdated, 2_000)
        .unwrap();
    assert_ne!(s1.id, s2.id);
    assert_eq!(objects(&m).len(), 3, "unchanged mca blob must be shared");

    fs::remove_dir_all(&root).unwrap();
}

#[test]
fn denylist_and_skips_keep_secrets_and_jars_out() {
    let root = scratch("deny");
    let world = root.join("world");
    write(&world.join("level.dat"), b"ok");
    write(&world.join("evil.jar"), b"binary");
    write(&world.join("accounts.json"), br#"{"token":"secret"}"#);
    write(&world.join("launcher_accounts.json"), br#"{"x":1}"#);
    write(&world.join("session.token"), b"tok");
    write(&world.join("datapacks/pack.jar"), b"jar-in-subdir");
    let mut m = tm(&root, TimeMachineConfig::default());

    let snap = m
        .create_snapshot(&world, SnapshotTrigger::Manual, 1_000)
        .unwrap();
    assert_eq!(snap.files.len(), 1);
    assert_eq!(snap.files[0].path, "level.dat");
    let denied: Vec<&str> = snap
        .skipped
        .iter()
        .filter(|s| s.reason == SkipReason::DenylistedName)
        .map(|s| s.path.as_str())
        .collect();
    assert_eq!(
        denied.len(),
        5,
        "all denylisted entries skipped: {denied:?}"
    );

    // The store must not contain the secret bytes anywhere.
    let store = root.join("store");
    for entry in fs::read_dir(store.join("objects")).unwrap() {
        let bytes = fs::read(entry.unwrap().path()).unwrap();
        assert_ne!(bytes, b"tok");
        assert!(!bytes.windows(6).any(|w| w == b"secret"));
    }
    fs::remove_dir_all(&root).unwrap();
}

#[cfg(unix)]
#[test]
fn symlinks_are_never_followed_or_stored() {
    let root = scratch("symlink");
    let world = root.join("world");
    write(&world.join("real.dat"), b"real");
    std::os::unix::fs::symlink("/etc/hostname", world.join("link.dat")).unwrap();
    let outside = root.join("outside");
    write(&outside.join("esc.dat"), b"escape");
    std::os::unix::fs::symlink(&outside, world.join("dirlink")).unwrap();

    let mut m = tm(&root, TimeMachineConfig::default());
    let snap = m
        .create_snapshot(&world, SnapshotTrigger::Manual, 1)
        .unwrap();
    assert_eq!(snap.files.len(), 1);
    let syms: Vec<_> = snap
        .skipped
        .iter()
        .filter(|s| s.reason == SkipReason::Symlink)
        .collect();
    assert_eq!(syms.len(), 2);
    fs::remove_dir_all(&root).unwrap();
}

#[test]
fn restore_round_trips_and_keeps_backup() {
    let root = scratch("restore");
    let world = root.join("world");
    write(&world.join("level.dat"), b"before-mods");
    write(&world.join("data/x.dat"), &[1u8; 100]);
    let mut m = tm(&root, TimeMachineConfig::default());
    let snap = m
        .create_snapshot(&world, SnapshotTrigger::ModAdded, 500)
        .unwrap();

    // "Mod install" clobbers the world.
    write(&world.join("level.dat"), b"after-bad-mod");
    write(&world.join("mod_junk.dat"), b"junk");

    let RestoreReport {
        snapshot_id,
        restored_files,
        backup_dir,
        ..
    } = m.restore_snapshot(&snap.id, &world).unwrap();
    assert_eq!(snapshot_id, snap.id);
    assert_eq!(restored_files, 2);
    assert_eq!(fs::read(world.join("level.dat")).unwrap(), b"before-mods");
    assert_eq!(fs::read(world.join("data/x.dat")).unwrap(), vec![1u8; 100]);
    // The pre-restore tree is preserved, not destroyed.
    let backup = backup_dir.expect("existing target must be backed up");
    assert_eq!(
        fs::read(backup.join("level.dat")).unwrap(),
        b"after-bad-mod"
    );
    fs::remove_dir_all(&root).unwrap();
}

#[test]
fn restore_fails_closed_on_corrupted_blob() {
    let root = scratch("corrupt");
    let world = root.join("world");
    write(&world.join("a.dat"), b"aaa");
    write(&world.join("b.dat"), b"bbb");
    let mut m = tm(&root, TimeMachineConfig::default());
    let snap = m
        .create_snapshot(&world, SnapshotTrigger::PreUpgrade, 1)
        .unwrap();

    // Corrupt one blob on disk.
    let victim = &snap.files[0].sha256;
    fs::write(root.join("store/objects").join(victim), b"tampered").unwrap();

    let before = fs::read(world.join("a.dat")).unwrap();
    let err = m.restore_snapshot(&snap.id, &world).unwrap_err();
    assert!(matches!(err, TimeMachineError::BlobChecksumMismatch { .. }));
    // Fail closed: the live world is untouched.
    assert_eq!(fs::read(world.join("a.dat")).unwrap(), before);
    assert!(world.join("a.dat").exists());
    assert!(!root.join(".world.mengu-restore-staging").exists());
    fs::remove_dir_all(&root).unwrap();
}

#[test]
fn restore_rejects_path_traversal_in_manifest() {
    let root = scratch("traversal");
    let world = root.join("world");
    write(&world.join("ok.dat"), b"ok");
    let mut m = tm(&root, TimeMachineConfig::default());
    let snap = m
        .create_snapshot(&world, SnapshotTrigger::Manual, 1)
        .unwrap();

    // Hand-edit the manifest to include a traversal path.
    let mpath = root
        .join("store/snapshots")
        .join(format!("{}.json", snap.id));
    let mut text = fs::read_to_string(&mpath).unwrap();
    text = text.replace("\"path\":\"ok.dat\"", "\"path\":\"../escaped.dat\"");
    fs::write(&mpath, text).unwrap();

    let err = m.restore_snapshot(&snap.id, &world).unwrap_err();
    assert!(matches!(err, TimeMachineError::UnsafeManifestPath { .. }));
    assert!(!root.join("escaped.dat").exists());
    fs::remove_dir_all(&root).unwrap();
}

#[test]
fn missing_snapshot_reports_not_found() {
    let root = scratch("missing");
    let m = tm(&root, TimeMachineConfig::default());
    match m.restore_snapshot("snap-doesnotexist", &root.join("w")) {
        Err(TimeMachineError::SnapshotNotFound { id }) => {
            assert_eq!(id, "snap-doesnotexist")
        }
        other => panic!("expected SnapshotNotFound, got {other:?}"),
    }
    fs::remove_dir_all(&root).unwrap();
}

#[test]
fn retention_prunes_oldest_and_gcs_unreferenced_blobs() {
    let root = scratch("retention");
    let world = root.join("world");
    let config = TimeMachineConfig {
        max_snapshots: 3,
        ..Default::default()
    };
    let mut m = tm(&root, config);
    for i in 0..5u32 {
        write(&world.join("level.dat"), format!("gen-{i}").as_bytes());
        m.create_snapshot(&world, SnapshotTrigger::Manual, i as u64)
            .unwrap();
    }
    let snaps = m.list_snapshots().unwrap();
    assert_eq!(snaps.len(), 3, "only newest 3 kept");
    assert_eq!(snaps[0].seq, 3);
    // Only the 3 referenced blobs remain — the 2 pruned generations were
    // swept by the mark-and-sweep GC.
    assert_eq!(fs::read_dir(root.join("store/objects")).unwrap().count(), 3);
    fs::remove_dir_all(&root).unwrap();
}

#[test]
fn byte_budget_prunes_snapshots_until_live_set_fits() {
    let root = scratch("bytes");
    let world = root.join("world");
    let config = TimeMachineConfig {
        max_snapshots: 100,
        max_total_bytes: 150,
        ..Default::default()
    };
    let mut m = tm(&root, config);
    for i in 0..4u32 {
        write(&world.join("f.dat"), &[i as u8; 100]);
        m.create_snapshot(&world, SnapshotTrigger::Manual, i as u64)
            .unwrap();
    }
    let snaps = m.list_snapshots().unwrap();
    assert_eq!(snaps.len(), 1, "4x100B > 150B keeps only the newest");
    assert_eq!(fs::read_dir(root.join("store/objects")).unwrap().count(), 1);
    fs::remove_dir_all(&root).unwrap();
}

#[test]
fn identical_content_is_idempotent() {
    let root = scratch("idem");
    let world = root.join("world");
    write(&world.join("level.dat"), b"same");
    let mut m = tm(&root, TimeMachineConfig::default());
    let a = m
        .create_snapshot(&world, SnapshotTrigger::Manual, 1_000)
        .unwrap();
    let b = m
        .create_snapshot(&world, SnapshotTrigger::Manual, 1_000)
        .unwrap();
    assert_eq!(a.id, b.id);
    assert_eq!(m.list_snapshots().unwrap().len(), 1);
    fs::remove_dir_all(&root).unwrap();
}

#[test]
fn oversized_file_fails_closed() {
    let root = scratch("toobig");
    let world = root.join("world");
    write(&world.join("huge.dat"), &[9u8; 2_000]);
    let config = TimeMachineConfig {
        max_file_bytes: 100,
        ..Default::default()
    };
    let mut m = tm(&root, config);
    match m.create_snapshot(&world, SnapshotTrigger::Manual, 1) {
        Err(TimeMachineError::FileTooLarge { path, len, cap }) => {
            assert_eq!(path, "huge.dat");
            assert_eq!(len, 2_000);
            assert_eq!(cap, 100);
        }
        other => panic!("expected FileTooLarge, got {other:?}"),
    }
    assert!(m.list_snapshots().unwrap().is_empty());
    fs::remove_dir_all(&root).unwrap();
}
