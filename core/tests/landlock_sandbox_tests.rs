// SPDX-License-Identifier: LGPL-3.0-or-later
//! Integration tests for `megu_core::landlock_sandbox` — the spec
//! builder, plan resolution and, on Linux, a real isolated child-process
//! proof that the *kernel* denies `EACCES` outside the declared jail.

use megu_core::landlock_sandbox::{
    detect_landlock_abi, enforce_sandbox, fs_access_mask_for_abi, read_only_access_mask,
    resolve_sandbox_plan, SandboxOutcome, ServerSandboxSpec, DISABLE_ENV_VAR,
};
use std::io;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

const CHILD_ENV: &str = "MEGU_LANDLOCK_E2E_CHILD";
const BASE_ENV: &str = "MEGU_LANDLOCK_E2E_BASE";
const CHILD_TEST_NAME: &str = "landlock_e2e_child";

fn temp_dir(tag: &str) -> PathBuf {
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let dir = std::env::temp_dir().join(format!(
        "megu-landlock-test-{}-{}-{tag}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn assert_eacces<T>(res: &io::Result<T>, what: &str) {
    match res {
        Err(e) => {
            assert_eq!(
                e.kind(),
                io::ErrorKind::PermissionDenied,
                "{what}: expected PermissionDenied (EACCES), got {e:?}"
            );
            #[cfg(unix)]
            assert_eq!(e.raw_os_error(), Some(13), "{what}: expected raw EACCES");
        }
        Ok(_) => panic!("{what}: kernel allowed access outside the sandbox grants"),
    }
}

// ---------------------------------------------------------------------
// Spec builder + plan resolution (pure / filesystem-only, no enforcement)
// ---------------------------------------------------------------------

#[test]
fn spec_for_server_root_builds_minimal_layout() {
    let spec = ServerSandboxSpec::for_server_root("/srv/megu");
    assert_eq!(
        spec.read_only_dirs,
        [
            "/srv/megu/datapacks",
            "/srv/megu/mods",
            "/srv/megu/assets",
            "/srv/megu/config",
        ]
        .iter()
        .map(PathBuf::from)
        .collect::<Vec<_>>()
    );
    assert_eq!(
        spec.read_write_dirs,
        [
            "/srv/megu/world",
            "/srv/megu/logs",
            "/srv/megu/backups",
            "/srv/megu/.megu-cache",
        ]
        .iter()
        .map(PathBuf::from)
        .collect::<Vec<_>>()
    );
    assert!(!spec.allow_tmp);
    assert!(spec.best_effort);
}

#[test]
fn abi_masks_step_strictly_by_version() {
    // v1: EXECUTE..MAKE_SYM (bits 0–12).
    assert_eq!(fs_access_mask_for_abi(1), 0x1FFF);
    // v2 adds REFER (bit 13), v3 adds TRUNCATE (bit 14).
    assert_eq!(fs_access_mask_for_abi(2), 0x3FFF);
    assert_eq!(fs_access_mask_for_abi(3), 0x7FFF);
    // v4 changes the ruleset struct, not the fs mask.
    assert_eq!(fs_access_mask_for_abi(4), 0x7FFF);
    // v5 adds IOCTL_DEV (bit 15); newer ABIs keep every earlier bit.
    assert_eq!(fs_access_mask_for_abi(5), 0xFFFF);
    assert_eq!(fs_access_mask_for_abi(6), 0xFFFF);
    assert_eq!(fs_access_mask_for_abi(7), 0xFFFF);
    // Read-only grant is exactly EXECUTE | READ_FILE | READ_DIR.
    assert_eq!(read_only_access_mask(), 0xD);
}

#[test]
fn plan_deduplicates_and_read_write_wins() {
    let base = temp_dir("dedup");
    let dup = base.join("shared");
    std::fs::create_dir_all(&dup).unwrap();
    let canon = std::fs::canonicalize(&dup).unwrap();
    let spec = ServerSandboxSpec {
        read_only_dirs: vec![dup.clone(), dup.clone()],
        read_write_dirs: vec![dup.clone(), dup.clone()],
        allow_tmp: false,
        best_effort: true,
    };
    let plan = resolve_sandbox_plan(&spec);
    // The same path declared read-write wins outright — no redundant
    // read-only rule survives planning.
    assert_eq!(plan.read_write, vec![canon]);
    assert!(plan.read_only.is_empty());
    assert!(plan.skipped_missing.is_empty());
    std::fs::remove_dir_all(&base).unwrap();
}

#[test]
fn plan_creates_missing_rw_and_skips_missing_ro() {
    let base = temp_dir("missing");
    let missing_rw = base.join("will_be_created");
    let missing_ro = base.join("absent_ro");
    let spec = ServerSandboxSpec {
        read_only_dirs: vec![missing_ro.clone()],
        read_write_dirs: vec![missing_rw.clone()],
        allow_tmp: false,
        best_effort: true,
    };
    let plan = resolve_sandbox_plan(&spec);
    // Read-write dirs are materialised so the jailed server can use them
    // later; read-only dirs that never existed are reported, not created.
    assert!(missing_rw.is_dir());
    assert_eq!(
        plan.read_write,
        vec![std::fs::canonicalize(&missing_rw).unwrap()]
    );
    assert!(!missing_ro.exists());
    assert!(plan.read_only.is_empty());
    assert_eq!(plan.skipped_missing, vec![missing_ro]);
    std::fs::remove_dir_all(&base).unwrap();
}

#[test]
fn plan_canonicalises_symlinks_and_honours_allow_tmp() {
    let base = temp_dir("canon");
    let real = base.join("real_world");
    std::fs::create_dir_all(&real).unwrap();
    #[cfg(unix)]
    {
        let link = base.join("world_link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let spec = ServerSandboxSpec {
            read_only_dirs: vec![],
            read_write_dirs: vec![link],
            allow_tmp: true,
            best_effort: true,
        };
        let plan = resolve_sandbox_plan(&spec);
        let tmp = std::fs::canonicalize(std::env::temp_dir()).unwrap();
        // The symlink resolved to its target — the kernel rule lands on
        // the real path, which is what path lookup will hit at runtime.
        assert_eq!(
            plan.read_write,
            vec![std::fs::canonicalize(&real).unwrap(), tmp]
        );
        assert!(plan.skipped_missing.is_empty());
    }
    std::fs::remove_dir_all(&base).unwrap();
}

// ---------------------------------------------------------------------
// Outcome semantics that never touch kernel enforcement
// ---------------------------------------------------------------------

#[test]
fn env_opt_out_returns_disabled() {
    std::env::set_var(DISABLE_ENV_VAR, "1");
    let res = enforce_sandbox(&ServerSandboxSpec::default());
    std::env::remove_var(DISABLE_ENV_VAR);
    assert_eq!(res, SandboxOutcome::Disabled);
}

#[cfg(not(target_os = "linux"))]
#[test]
fn non_linux_reports_unsupported_os() {
    assert_eq!(detect_landlock_abi(), None);
    let relaxed = enforce_sandbox(&ServerSandboxSpec {
        best_effort: true,
        ..ServerSandboxSpec::default()
    });
    assert_eq!(relaxed, SandboxOutcome::UnsupportedOs);
    let strict = enforce_sandbox(&ServerSandboxSpec {
        best_effort: false,
        ..ServerSandboxSpec::default()
    });
    assert!(matches!(strict, SandboxOutcome::Failed(_)));
}

#[cfg(target_os = "linux")]
#[test]
fn abi_probe_is_consistent() {
    // A Linux 5.13+ host with the LSM enabled reports a positive ABI; a
    // pre-5.13/seccomp-filtered host reports None. Either is a clean,
    // non-flaky answer — the child-process test covers real enforcement.
    if let Some(abi) = detect_landlock_abi() {
        assert!(abi >= 1);
    }
}

// ---------------------------------------------------------------------
// Real kernel enforcement, in an isolated child process
// ---------------------------------------------------------------------

/// Child-side body: only does real work when the parent spawns it with
/// `MEGU_LANDLOCK_E2E_CHILD` set — a plain `cargo test` run skips it.
/// Enforcing Landlock in this test's own process would poison the
/// sandboxed thread for the rest of the harness run.
#[test]
fn landlock_e2e_child() {
    if std::env::var_os(CHILD_ENV).is_none() {
        return;
    }
    let base =
        PathBuf::from(std::env::var_os(BASE_ENV).expect("child needs MEGU_LANDLOCK_E2E_BASE"));
    let allowed_rw = base.join("allowed_rw");
    let allowed_ro = base.join("allowed_ro");
    let forbidden = base.join("forbidden");
    for d in [&allowed_rw, &allowed_ro, &forbidden] {
        std::fs::create_dir_all(d).unwrap();
    }
    // Real files inside each dir so the checks open existing objects,
    // not just directory creation rights.
    std::fs::write(allowed_ro.join("can_read.txt"), b"ro").unwrap();
    std::fs::write(allowed_rw.join("seed.txt"), b"rw").unwrap();
    std::fs::write(forbidden.join("secret.txt"), b"forbidden").unwrap();

    let spec = ServerSandboxSpec {
        read_only_dirs: vec![allowed_ro.clone()],
        read_write_dirs: vec![allowed_rw.clone()],
        allow_tmp: false,
        best_effort: true,
    };
    let outcome = enforce_sandbox(&spec);
    println!("LANDLOCK_E2E_OUTCOME={outcome:?}");
    match outcome {
        SandboxOutcome::Enforced { .. } => {
            // Inside the jail: read-write dir serves reads and writes.
            std::fs::write(allowed_rw.join("new.txt"), b"ok")
                .expect("write inside read-write dir must succeed");
            assert_eq!(std::fs::read(allowed_rw.join("seed.txt")).unwrap(), b"rw");
            assert!(std::fs::read_dir(&allowed_rw).unwrap().count() >= 2);

            // Read-only dir serves reads, refuses every write shape.
            assert_eq!(
                std::fs::read(allowed_ro.join("can_read.txt")).unwrap(),
                b"ro"
            );
            assert_eacces(
                &std::fs::write(allowed_ro.join("planted.txt"), b"x"),
                "create file inside read-only dir",
            );
            assert_eacces(
                &std::fs::OpenOptions::new()
                    .write(true)
                    .open(allowed_ro.join("can_read.txt")),
                "open-for-write inside read-only dir",
            );
            assert_eacces(
                &std::fs::create_dir(allowed_ro.join("subdir")),
                "mkdir inside read-only dir",
            );
            assert_eacces(
                &std::fs::remove_file(allowed_ro.join("can_read.txt")),
                "unlink inside read-only dir",
            );

            // Everything not granted — the kernel answers EACCES itself.
            assert_eacces(
                &std::fs::read(forbidden.join("secret.txt")),
                "read file inside forbidden dir",
            );
            assert_eacces(
                &std::fs::write(forbidden.join("new.txt"), b"x"),
                "create file inside forbidden dir",
            );
            assert_eacces(
                &std::fs::read_dir(&forbidden).map(|mut it| it.next()),
                "list forbidden dir",
            );
            // NB: metadata-only calls (`stat`, `canonicalize`) are NOT
            // Landlock-mediated — only open/exec/write-shape hooks are —
            // so asserting EACCES on those would be a false expectation.
            println!("LANDLOCK_E2E_RESULT=enforced");
        }
        // Pre-5.13 kernel, `landlock` out of `lsm=`, or a seccomp filter:
        // report cleanly, never flake.
        SandboxOutcome::UnsupportedKernel => println!("LANDLOCK_E2E_RESULT=unsupported_kernel"),
        other => {
            eprintln!("unexpected enforce_sandbox outcome: {other:?}");
            std::process::exit(2);
        }
    }
    // No child-side cleanup: once Enforced, this thread is forbidden from
    // touching `base`. The parent (unsandboxed) removes it.
}

/// Parent: spawn this same test binary so the Landlock domain lands on a
/// throwaway process, then assert the child's kernel-level findings.
#[cfg(target_os = "linux")]
#[test]
fn landlock_kernel_enforcement_e2e() {
    let exe = std::env::current_exe().unwrap();
    let base = temp_dir("e2e");
    let out = Command::new(exe)
        .args([
            CHILD_TEST_NAME,
            "--exact",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(CHILD_ENV, "1")
        .env(BASE_ENV, &base)
        // The env opt-out test mutates process env; never let it leak
        // into the child and downgrade a real enforcement run.
        .env_remove(DISABLE_ENV_VAR)
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    std::fs::remove_dir_all(&base).ok();
    assert!(
        out.status.success(),
        "child process failed\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    if stdout.contains("LANDLOCK_E2E_RESULT=enforced") {
        eprintln!("landlock E2E: kernel enforced; forbidden/ access blocked with EACCES");
    } else if stdout.contains("LANDLOCK_E2E_RESULT=unsupported_kernel") {
        eprintln!("landlock E2E: host kernel lacks Landlock; enforcement skipped cleanly");
    } else {
        panic!("child reported no result marker\nstdout:\n{stdout}\nstderr:\n{stderr}");
    }
}
