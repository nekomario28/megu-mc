// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! Lane UX-BACKUP-1 integration tests: the content-addressed
//! `world_snapshot` store — deduplication, verification, retention/GC,
//! fail-closed path safety, and the atomic 1-click rollback — exercised
//! against real directories on disk.

use megu_state_store::world_snapshot::{
    SnapshotReason, WorldSnapshotError, WorldSnapshotStore,
};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static DIR_COUNTER: AtomicU64 = AtomicU64::new(0);

/// A per-test scratch dir under the system temp dir (TMPDIR-honoring).
struct TestDir(PathBuf);

impl TestDir {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "megu-uxbackup1-{}-{}-{tag}",
            std::process::id(),
            DIR_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        Self(std::fs::canonicalize(&dir).unwrap())
    }

    fn path(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Write `bytes` to `base/rel`, creating parent dirs.
fn put(base: &Path, rel: &str, bytes: &[u8]) {
    let path = base.join(rel);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, bytes).unwrap();
}

/// Every regular file under `base` as `rel_path -> bytes`, sorted.
fn world_footprint(base: &Path) -> BTreeMap<String, Vec<u8>> {
    let mut out = BTreeMap::new();
    let mut stack = vec![base.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).unwrap().flatten() {
            let ft = entry.file_type().unwrap();
            if ft.is_dir() {
                stack.push(entry.path());
            } else if ft.is_file() {
                let rel = entry
                    .path()
                    .strip_prefix(base)
                    .unwrap()
                    .to_str()
                    .unwrap()
                    .replace(std::path::MAIN_SEPARATOR, "/");
                out.insert(rel, std::fs::read(entry.path()).unwrap());
            }
        }
    }
    out
}

/// Count object blobs under `<backup_root>/objects`.
fn count_object_blobs(backup_root: &Path) -> usize {
    let objects = backup_root.join("objects");
    let mut count = 0;
    let mut stack = vec![objects];
    while let Some(dir) = stack.pop() {
        if !dir.is_dir() {
            continue;
        }
        for entry in std::fs::read_dir(&dir).unwrap().flatten() {
            let ft = entry.file_type().unwrap();
            if ft.is_dir() {
                stack.push(entry.path());
            } else {
                count += 1;
            }
        }
    }
    count
}

/// A small but representative world: level.dat, two region files, a mod
/// overlay blob and player data spread across subdirectories.
fn seed_world(world: &Path) {
    put(
        world,
        "level.dat",
        b"level-dat-v1: seed=42, DataVersion=5023",
    );
    put(world, "level.dat_old", b"stale level.dat backup");
    put(world, "region/r.0.0.mca", &[b'R'; 4096]);
    put(world, "region/r.-1.0.mca", &[b'Q'; 2048]);
    put(world, "world_overlay.bin", b"overlay:v1\x00\x01\x02");
    put(world, "data/create_trains.dat", b"train-graph-v1");
}

/// A store with `backup_root` as a sibling `.megu-snapshots` dir, the
/// production layout.
fn sibling_store(dir: &TestDir) -> (WorldSnapshotStore, PathBuf, PathBuf) {
    let world = dir.path("world");
    let backup = dir.path(".megu-snapshots");
    std::fs::create_dir_all(&world).unwrap();
    let store = WorldSnapshotStore::new(&world, &backup).unwrap();
    (store, world, backup)
}

// ------------------------------------------------------------- creation

#[test]
fn create_snapshot_manifest_is_deterministic_and_sorted() {
    let dir = TestDir::new("create");
    let (store, world, backup) = sibling_store(&dir);
    seed_world(&world);

    let m = store
        .create_snapshot(
            SnapshotReason::PreModChange {
                summary: "install create-6.0".to_string(),
            },
            "26.3",
            1_700_000_000,
        )
        .unwrap();

    assert_eq!(m.file_count, 6);
    assert_eq!(m.file_count as usize, m.files.len());
    assert_eq!(m.total_bytes, m.files.iter().map(|f| f.size).sum::<u64>());
    assert_eq!(m.timestamp_secs, 1_700_000_000);
    assert_eq!(m.mc_version, "26.3");
    assert_eq!(
        m.reason,
        SnapshotReason::PreModChange {
            summary: "install create-6.0".to_string()
        }
    );
    // Sorted manifest, every entry jailed and content-addressed.
    let paths: Vec<&str> = m.files.iter().map(|f| f.rel_path.as_str()).collect();
    let mut sorted = paths.clone();
    sorted.sort_unstable();
    assert_eq!(paths, sorted);
    for f in &m.files {
        assert_eq!(f.sha256.len(), 64);
        let blob = backup.join("objects").join(&f.sha256[..2]).join(&f.sha256);
        assert!(blob.is_file(), "blob for {} must exist", f.rel_path);
        assert_eq!(std::fs::metadata(&blob).unwrap().len(), f.size);
    }
    // Manifest document on disk, named by snapshot_id.
    let on_disk = backup
        .join("snapshots")
        .join(format!("{}.json", m.snapshot_id));
    let text = std::fs::read_to_string(on_disk).unwrap();
    assert!(text.contains("megu-world-snapshot-v1"));
    assert!(text.contains(&m.snapshot_id));
    // Round-trips through the strict loader byte-identically.
    let loaded = store.load_manifest(&m.snapshot_id).unwrap();
    assert_eq!(loaded, m);
}

#[test]
fn deduplication_stores_each_unique_content_once() {
    let dir = TestDir::new("dedup");
    let (store, world, backup) = sibling_store(&dir);
    seed_world(&world);

    store
        .create_snapshot(SnapshotReason::Manual, "26.3", 100)
        .unwrap();
    // Six files, snapshot one: six blobs.
    assert_eq!(count_object_blobs(&backup), 6);

    // Modify exactly one file; five are unchanged.
    put(
        &world,
        "level.dat",
        b"level-dat-v2: seed=43, DataVersion=5023",
    );
    let m2 = store
        .create_snapshot(SnapshotReason::Manual, "26.3", 200)
        .unwrap();
    assert_eq!(m2.file_count, 6);

    // 5 unchanged blobs are shared; the modified file contributes a second
    // version -> 7 unique blobs, not the 12 an undeduplicated store needs.
    assert_eq!(count_object_blobs(&backup), 7);
    // ...and both manifests still verify: snapshot 1 kept its old blob.
    assert_eq!(store.list_snapshots().unwrap().len(), 2);
    let v1 = &store.list_snapshots().unwrap()[0];
    assert_eq!(v1.timestamp_secs, 100);
    store.verify_snapshot(&v1.snapshot_id).unwrap();
    store.verify_snapshot(&m2.snapshot_id).unwrap();
}

// --------------------------------------------------------------- restore

#[test]
fn restore_round_trips_and_sweeps_post_snapshot_files() {
    let dir = TestDir::new("restore");
    let (store, world, _backup) = sibling_store(&dir);
    seed_world(&world);
    let original = world_footprint(&world);

    let m = store
        .create_snapshot(SnapshotReason::Manual, "26.3", 100)
        .unwrap();

    // Break the world: mutate a file, delete one, add post-snapshot junk.
    put(&world, "level.dat", b"CORRUPTED BY A BAD MOD");
    std::fs::remove_file(world.join("region/r.-1.0.mca")).unwrap();
    put(&world, "data/crash_leftover.tmp", b"junk");
    put(&world, "mods/broken_mod_jar.dat", b"more junk");

    let report = store.restore_snapshot(&m.snapshot_id, 900).unwrap();
    assert_eq!(report.snapshot_id, m.snapshot_id);
    assert_eq!(report.restored_files, 6);
    assert!(report.restored_bytes > 0);
    // The pre-restore live tree carried 7 regular files (6 seeded minus the
    // deleted region file plus 2 junk files); all were swept.
    assert_eq!(report.swept_files, 7);
    let safety = report
        .safety_snapshot_id
        .expect("default restore must take a pre-rollback snapshot");

    // Byte-for-byte: the world is exactly what the snapshot recorded.
    assert_eq!(world_footprint(&world), original);
    // Post-snapshot junk is gone, deleted file is back.
    assert!(!world.join("data/crash_leftover.tmp").exists());
    assert!(!world.join("mods/broken_mod_jar.dat").exists());
    assert_eq!(
        std::fs::read(world.join("region/r.-1.0.mca")).unwrap(),
        vec![b'Q'; 2048]
    );
    // No staging/trash debris left beside the world.
    for entry in std::fs::read_dir(dir.path("")).unwrap().flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        assert!(
            !name.contains(".megu-staging-") && !name.contains(".megu-old-"),
            "leftover restore debris: {name}"
        );
    }

    // The safety snapshot captured the corrupted pre-restore world, so the
    // rollback itself is undoable.
    let safety_manifest = store.load_manifest(&safety).unwrap();
    assert_eq!(safety_manifest.reason, SnapshotReason::PreRollback);
    let corrupted = world_footprint(&world);
    assert_ne!(
        corrupted.get("level.dat").unwrap(),
        b"CORRUPTED BY A BAD MOD"
    );
    store.verify_snapshot(&safety).unwrap();
    // Roll back the rollback: restoring the safety snapshot must bring the
    // corrupted world back byte-for-byte.
    store.restore_snapshot(&safety, 950).unwrap();
    assert_eq!(
        world_footprint(&world).get("level.dat").unwrap(),
        b"CORRUPTED BY A BAD MOD"
    );
    assert!(world.join("data/crash_leftover.tmp").exists());
}

#[test]
fn restore_without_safety_backup_skips_prerollback() {
    let dir = TestDir::new("restore-nosafety");
    let (store, world, _backup) = sibling_store(&dir);
    seed_world(&world);
    let m = store
        .create_snapshot(SnapshotReason::Manual, "26.3", 100)
        .unwrap();
    put(&world, "level.dat", b"mutated");
    let report = store
        .restore_snapshot_opt(&m.snapshot_id, false, 900)
        .unwrap();
    assert_eq!(report.safety_snapshot_id, None);
    assert_eq!(store.list_snapshots().unwrap().len(), 1);
}

#[test]
fn restore_preserves_backup_store_nested_inside_world() {
    let dir = TestDir::new("restore-nested");
    let world = dir.path("world");
    let backup = world.join(".megu-snapshots");
    std::fs::create_dir_all(&world).unwrap();
    seed_world(&world);
    let store = WorldSnapshotStore::new(&world, &backup).unwrap();

    let m = store
        .create_snapshot(SnapshotReason::Manual, "26.3", 100)
        .unwrap();
    // The nested backup dir must never appear in a manifest.
    assert!(!m
        .files
        .iter()
        .any(|f| f.rel_path.contains(".megu-snapshots")));

    put(&world, "level.dat", b"mutated");
    store.restore_snapshot(&m.snapshot_id, 900).unwrap();
    assert_eq!(
        std::fs::read(world.join("level.dat")).unwrap(),
        b"level-dat-v1: seed=42, DataVersion=5023"
    );
    // The backup store (with every snapshot, including the new
    // pre-rollback one) survived the swap at the same nested path.
    assert!(backup.join("objects").is_dir());
    assert!(backup.join("snapshots").is_dir());
    assert_eq!(store.list_snapshots().unwrap().len(), 2);
    store.verify_snapshot(&m.snapshot_id).unwrap();
}

// ------------------------------------------------------------ integrity

#[test]
fn corruption_in_one_blob_fails_verify_and_restore_closed() {
    let dir = TestDir::new("corrupt");
    let (store, world, backup) = sibling_store(&dir);
    seed_world(&world);
    let before = world_footprint(&world);
    let m = store
        .create_snapshot(SnapshotReason::Manual, "26.3", 100)
        .unwrap();

    // Flip one bit inside the blob backing level.dat.
    let entry = m.files.iter().find(|f| f.rel_path == "level.dat").unwrap();
    let blob = backup
        .join("objects")
        .join(&entry.sha256[..2])
        .join(&entry.sha256);
    let mut bytes = std::fs::read(&blob).unwrap();
    bytes[0] ^= 0x01;
    std::fs::write(&blob, &bytes).unwrap();

    let err = store.verify_snapshot(&m.snapshot_id).unwrap_err();
    assert!(
        matches!(err, WorldSnapshotError::Integrity { .. }),
        "expected Integrity, got {err}"
    );
    // Restore must fail closed BEFORE touching the live world.
    put(&world, "level.dat", b"mutated");
    let err = store.restore_snapshot(&m.snapshot_id, 900).unwrap_err();
    assert!(
        matches!(
            err,
            WorldSnapshotError::Integrity { .. } | WorldSnapshotError::SizeMismatch { .. }
        ),
        "expected Integrity/SizeMismatch, got {err}"
    );
    let after = world_footprint(&world);
    assert_eq!(
        after.get("level.dat").unwrap(),
        b"mutated",
        "failed restore must not touch the live world"
    );
    // Every other file is also exactly as it was pre-restore-attempt.
    assert_eq!(after.len(), before.len());
}

#[test]
fn missing_blob_fails_verify_closed() {
    let dir = TestDir::new("missing-blob");
    let (store, world, backup) = sibling_store(&dir);
    seed_world(&world);
    let m = store
        .create_snapshot(SnapshotReason::Manual, "26.3", 100)
        .unwrap();
    let entry = &m.files[0];
    let blob = backup
        .join("objects")
        .join(&entry.sha256[..2])
        .join(&entry.sha256);
    std::fs::remove_file(&blob).unwrap();
    let err = store.verify_snapshot(&m.snapshot_id).unwrap_err();
    assert!(
        matches!(err, WorldSnapshotError::MissingObject { .. }),
        "expected MissingObject, got {err}"
    );
}

#[test]
fn tampered_manifest_fails_closed() {
    let dir = TestDir::new("tamper");
    let (store, world, _backup) = sibling_store(&dir);
    seed_world(&world);
    let m = store
        .create_snapshot(SnapshotReason::Manual, "26.3", 100)
        .unwrap();
    let path = dir
        .path(".megu-snapshots")
        .join("snapshots")
        .join(format!("{}.json", m.snapshot_id));
    let text = std::fs::read_to_string(&path).unwrap();
    // Lie about one file's size: the internal consistency check must catch
    // it. The `"size":` prefix keeps the replace from ever landing inside a
    // hex digest substring.
    let forged = text.replacen("\"size\":2048", "\"size\":2049", 1);
    assert_ne!(forged, text, "forgery must actually change the manifest");
    std::fs::write(&path, forged).unwrap();
    assert!(store.load_manifest(&m.snapshot_id).is_err());
    assert!(store.restore_snapshot(&m.snapshot_id, 900).is_err());
    assert_eq!(
        world_footprint(&world).get("level.dat").unwrap(),
        b"level-dat-v1: seed=42, DataVersion=5023"
    );
}

// ------------------------------------------------------------- retention

#[test]
fn prune_oldest_keeps_newest_and_gcs_orphaned_blobs() {
    let dir = TestDir::new("prune");
    let (store, world, backup) = sibling_store(&dir);
    seed_world(&world);

    // Five snapshots; each rewrites level.dat so every snapshot owns one
    // private blob, while the other five files stay shared across all five.
    for ts in 1..=5u64 {
        put(
            &world,
            "level.dat",
            format!("level-dat-v{ts}: churn").as_bytes(),
        );
        store
            .create_snapshot(SnapshotReason::Scheduled, "26.3", ts * 100)
            .unwrap();
    }
    assert_eq!(store.list_snapshots().unwrap().len(), 5);
    // 5 shared + 5 distinct level.dat versions = 10 blobs.
    assert_eq!(count_object_blobs(&backup), 10);

    let report = store.prune_oldest(2).unwrap();
    assert_eq!(report.kept_snapshots, 2);
    assert_eq!(report.removed_snapshots.len(), 3);
    // Newest two kept, in the manifest dir and in list order.
    let kept = store.list_snapshots().unwrap();
    assert_eq!(kept.len(), 2);
    assert_eq!(kept[0].timestamp_secs, 400);
    assert_eq!(kept[1].timestamp_secs, 500);
    // Three orphaned level.dat blobs collected; shared blobs untouched.
    // Kept snapshots still fully verify.
    assert_eq!(count_object_blobs(&backup), 7);
    assert_eq!(report.removed_objects, 3);
    for m in &kept {
        store.verify_snapshot(&m.snapshot_id).unwrap();
    }

    // Pruning below the floor is a no-op, not an error.
    let noop = store.prune_oldest(10).unwrap();
    assert_eq!(noop.removed_snapshots.len(), 0);
    assert_eq!(store.list_snapshots().unwrap().len(), 2);
    // Prune to zero removes every manifest and every blob.
    let all = store.prune_oldest(0).unwrap();
    assert_eq!(all.kept_snapshots, 0);
    assert_eq!(count_object_blobs(&backup), 0);
}

// --------------------------------------------------------------- security

#[test]
fn symlink_inside_world_is_refused_fail_closed() {
    let dir = TestDir::new("symlink");
    let (store, world, _backup) = sibling_store(&dir);
    seed_world(&world);

    let outside = dir.path("outside_secret.txt");
    std::fs::write(&outside, b"exfiltrate-me").unwrap();
    let link = world.join("region/evil_link.mca");
    #[cfg(unix)]
    std::os::unix::fs::symlink(&outside, &link).unwrap();
    #[cfg(windows)]
    std::os::windows::fs::symlink_file(&outside, &link).unwrap();

    let err = store
        .create_snapshot(SnapshotReason::Manual, "26.3", 100)
        .unwrap_err();
    assert!(
        matches!(err, WorldSnapshotError::UnsafePath { .. }),
        "expected UnsafePath, got {err}"
    );
}

#[test]
fn traversal_in_manifest_and_id_is_refused_fail_closed() {
    let dir = TestDir::new("traversal");
    let (store, world, backup) = sibling_store(&dir);
    seed_world(&world);
    let m = store
        .create_snapshot(SnapshotReason::Manual, "26.3", 100)
        .unwrap();

    // Snapshot ids are used to build paths — traversal spellings die first.
    for bad_id in ["../escape", "..", "a/b", "..\\x"] {
        let err = store.load_manifest(bad_id).unwrap_err();
        assert!(
            matches!(err, WorldSnapshotError::UnsafeSnapshotId(_)),
            "{bad_id:?}: expected UnsafeSnapshotId, got {err}"
        );
    }
    let err = store.restore_snapshot("../escape", 900).unwrap_err();
    assert!(matches!(err, WorldSnapshotError::UnsafeSnapshotId(_)));
    let err = store.load_manifest("does-not-exist").unwrap_err();
    assert!(matches!(err, WorldSnapshotError::SnapshotNotFound(_)));

    // A hand-forged manifest with a ../escape entry must die at load —
    // before any restore planning can touch it.
    let real = m.files.iter().find(|f| f.rel_path == "level.dat").unwrap();
    let forged = format!(
        "{{\"file_count\":1,\"files\":[{{\"path\":\"../escape.txt\",\"sha256\":\"{}\",\"size\":{}}}],\"format\":\"megu-world-snapshot-v1\",\"mc_version\":\"26.3\",\"reason\":{{\"kind\":\"manual\"}},\"snapshot_id\":\"forged\",\"timestamp_secs\":1,\"total_bytes\":{}}}",
        real.sha256, real.size, real.size
    );
    std::fs::write(backup.join("snapshots/forged.json"), forged).unwrap();
    let err = store.load_manifest("forged").unwrap_err();
    assert!(
        matches!(err, WorldSnapshotError::UnsafePath { .. }),
        "expected UnsafePath, got {err}"
    );
    assert!(store.restore_snapshot("forged", 900).is_err());
    assert_eq!(
        world_footprint(&world).get("level.dat").unwrap(),
        b"level-dat-v1: seed=42, DataVersion=5023"
    );
    // The forged manifest must also poison listing, not silently skip —
    // pruning on a partial view could GC still-referenced blobs.
    assert!(store.list_snapshots().is_err());
    assert!(store.prune_oldest(0).is_err());
}

#[test]
fn symlinked_object_blob_is_refused_on_verify() {
    let dir = TestDir::new("symlink-obj");
    let (store, world, backup) = sibling_store(&dir);
    seed_world(&world);
    let m = store
        .create_snapshot(SnapshotReason::Manual, "26.3", 100)
        .unwrap();
    let entry = &m.files[0];
    let blob = backup
        .join("objects")
        .join(&entry.sha256[..2])
        .join(&entry.sha256);
    let payload = dir.path("payload.bin");
    std::fs::write(&payload, b"attacker bytes").unwrap();
    std::fs::remove_file(&blob).unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(&payload, &blob).unwrap();
    #[cfg(windows)]
    std::os::windows::fs::symlink_file(&payload, &blob).unwrap();

    let err = store.verify_snapshot(&m.snapshot_id).unwrap_err();
    assert!(
        matches!(err, WorldSnapshotError::UnsafePath { .. }),
        "expected UnsafePath, got {err}"
    );
}
