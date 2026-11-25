// SPDX-License-Identifier: LGPL-3.0-or-later
#![allow(unsafe_code)]
//! `megu_core::landlock_sandbox` — OpenBSD `unveil`-grade kernel
//! filesystem sandboxing for dedicated servers via the Linux Landlock LSM
//! (ABI v1–v7, kernels 5.13+).
//!
//! Once [`enforce_sandbox`] reports [`SandboxOutcome::Enforced`], every
//! path *outside* the declared directories is denied by the kernel itself
//! with `EACCES`: a rogue datapack, mod or compromised plugin cannot read
//! `~/.ssh`, `/etc/shadow` or a sibling server's `world/` even if it gains
//! arbitrary `std::fs` execution inside the process. Landlock domains are
//! one-way — nothing user-space does afterwards can widen them.
//!
//! Zero external crates: the three Landlock syscalls
//! (`landlock_create_ruleset`, `landlock_add_rule`,
//! `landlock_restrict_self`) plus `prctl(PR_SET_NO_NEW_PRIVS)` go through a
//! small audited `unsafe` block calling `syscall(2)`/`prctl(2)` on the C
//! library the Rust runtime already links. Every raw descriptor sits in an
//! RAII guard (`OwnedFd`/`File`), so the ruleset and probe fds can never
//! leak.
//!
//! * [`ServerSandboxSpec`] — the declarative rule set.
//!   [`ServerSandboxSpec::for_server_root`] builds the minimal Megu
//!   dedicated-server profile: read-only `datapacks/`, `mods/`, `assets/`,
//!   `config/`; read-write `world/`, `logs/`, `backups/`, `.megu-cache/`.
//! * [`resolve_sandbox_plan`] — the pure planning stage: canonicalisation,
//!   deduplication (a read-write grant wins over a read-only one for the
//!   same path) and missing-directory handling, factored out so it is
//!   unit-testable without touching the kernel.
//! * [`enforce_sandbox`] — applies the spec to the calling thread.
//!   [`SandboxOutcome`] reports exactly what happened:
//!   `Enforced { abi_version, ro_rules, rw_rules }` on Landlock-capable
//!   kernels, `UnsupportedKernel` below 5.13 / with the LSM disabled,
//!   `UnsupportedOs` off Linux, `Disabled` under the
//!   `MEGU_DISABLE_LANDLOCK` escape hatch, and `Failed(reason)` whenever a
//!   real enforcement step errors or `best_effort: false` refuses to
//!   degrade.
//!
//! Landlock is per-thread and inherited: the domain covers the enforcing
//! thread plus every thread/process spawned afterwards, so call
//! [`enforce_sandbox`] on the server bootstrap thread *before* starting
//! workers. On non-Linux targets this module compiles to a clean stub
//! reporting `UnsupportedOs` (or `Failed` when `best_effort` is false).

use std::path::{Path, PathBuf};

/// Escape-hatch environment variable: when set to a truthy value
/// (anything except `""`, `"0"` or `"false"`),
/// [`enforce_sandbox`] returns [`SandboxOutcome::Disabled`] without
/// touching the kernel — for debugging, nested-sandbox CI containers and
/// kernels that misreport Landlock support.
pub const DISABLE_ENV_VAR: &str = "MEGU_DISABLE_LANDLOCK";

/// Declarative filesystem sandbox description for one dedicated server.
///
/// Landlock is deny-by-default: once enforced, every path not covered by
/// `read_only_dirs`/`read_write_dirs` is unreachable (`EACCES`). The
/// server root itself is *not* implicitly granted — grant only the
/// directories the server legitimately touches.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ServerSandboxSpec {
    /// Directories (recursively) readable — list, read and execute —
    /// but never writable: content the server loads that a compromised
    /// plugin must not plant or overwrite (`datapacks/`, `mods/`).
    pub read_only_dirs: Vec<PathBuf>,
    /// Directories with the full filesystem right set beneath them:
    /// create/remove/rename/truncate (`world/`, `logs/`, `backups/`).
    pub read_write_dirs: Vec<PathBuf>,
    /// Also grant read-write access beneath [`std::env::temp_dir`], for
    /// launchers that stage downloads and datapack extraction there.
    pub allow_tmp: bool,
    /// `true` (the dedicated-server default) degrades cleanly when the
    /// kernel cannot enforce Landlock (pre-5.13, LSM disabled, non-Linux
    /// host); `false` fails closed: [`enforce_sandbox`] returns
    /// [`SandboxOutcome::Failed`] and the caller must refuse to boot.
    pub best_effort: bool,
}

impl ServerSandboxSpec {
    /// The minimal Megu dedicated-server ruleset rooted at `server_dir`.
    ///
    /// Read-only: `datapacks/`, `mods/`, `assets/`, `config/` (the
    /// `config_packets.bin` parents a server reads but never writes).
    /// Read-write: `world/`, `logs/`, `backups/`, `.megu-cache/`.
    /// `best_effort` so non-Linux dev machines and pre-5.13 kernels still
    /// boot; dedicated deployments wanting a hard guarantee set
    /// `best_effort: false` afterwards.
    pub fn for_server_root(server_dir: impl AsRef<Path>) -> Self {
        let root = server_dir.as_ref();
        Self {
            read_only_dirs: ["datapacks", "mods", "assets", "config"]
                .iter()
                .map(|d| root.join(d))
                .collect(),
            read_write_dirs: ["world", "logs", "backups", ".megu-cache"]
                .iter()
                .map(|d| root.join(d))
                .collect(),
            allow_tmp: false,
            best_effort: true,
        }
    }

    /// The outcome this spec yields when the host cannot enforce at all:
    /// `degraded` under `best_effort`, otherwise fail-closed `Failed`.
    /// This is the single place the best-effort/fail-closed policy lives,
    /// so every degradation path (unsupported kernel, unsupported OS)
    /// answers identically.
    fn unenforceable_outcome(&self, degraded: SandboxOutcome, reason: &str) -> SandboxOutcome {
        if self.best_effort {
            degraded
        } else {
            SandboxOutcome::Failed(reason.to_owned())
        }
    }
}

/// The resolved rule set [`enforce_sandbox`] applies — the pure,
/// inspectable output of [`resolve_sandbox_plan`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SandboxPlan {
    /// Canonicalised read-only roots that exist on disk.
    pub read_only: Vec<PathBuf>,
    /// Canonicalised read-write roots. Missing read-write directories are
    /// materialised during resolution: Landlock only grants *beneath*
    /// listed roots, so a `world/` the jail forbids the server to create
    /// would wedge it at first save.
    pub read_write: Vec<PathBuf>,
    /// Declared directories dropped because they did not exist and could
    /// not be materialised. Surfaced explicitly: a silently skipped rule
    /// is indistinguishable from a denied one in enforcement output.
    pub skipped_missing: Vec<PathBuf>,
}

/// What [`enforce_sandbox`] actually did — the caller (and the E2E test)
/// branches on this, never on hope.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SandboxOutcome {
    /// A kernel Landlock domain is now active on this thread.
    /// `abi_version` is the running kernel's reported ABI (1 = Linux
    /// 5.13, 2 = 5.19, 3 = 6.2, 4 = 6.7, 5 = 6.10; newer kernels report
    /// newer ABIs and enforce the same ruleset). `ro_rules`/`rw_rules`
    /// count the granted path-beneath rules.
    Enforced {
        /// Running kernel's Landlock ABI version.
        abi_version: u32,
        /// Read-only path-beneath rules granted.
        ro_rules: usize,
        /// Read-write path-beneath rules granted.
        rw_rules: usize,
    },
    /// Linux kernel without Landlock: pre-5.13, `landlock` absent from
    /// the `lsm=` boot list, or the syscalls seccomp-filtered (containers).
    /// Only reachable under `best_effort`.
    UnsupportedKernel,
    /// Non-Linux platform (macOS, Windows, *BSD): Landlock is a Linux
    /// LSM. Only reachable under `best_effort`.
    UnsupportedOs,
    /// [`DISABLE_ENV_VAR`] opted the process out of enforcement.
    Disabled,
    /// A real enforcement step errored, or `best_effort: false` refused
    /// a degraded outcome — the caller must not start the server.
    Failed(String),
}

/// Resolve `spec` into the concrete rule set: canonicalise every path
/// (symlinks resolve before the kernel sees them), deduplicate, and let a
/// read-write grant subsume a read-only one for the same path — Landlock
/// unions grants within a domain, so a redundant read-only rule would
/// only clutter the ruleset.
///
/// Missing read-write directories are created; missing read-only
/// directories (and anything that fails canonicalisation) land in
/// [`SandboxPlan::skipped_missing`]. This function performs filesystem
/// mutations only for `read_write_dirs` and [`ServerSandboxSpec::allow_tmp`].
pub fn resolve_sandbox_plan(spec: &ServerSandboxSpec) -> SandboxPlan {
    let mut read_write: Vec<PathBuf> = Vec::new();
    let mut skipped: Vec<PathBuf> = Vec::new();
    for dir in &spec.read_write_dirs {
        // A missing read-write dir is created *now* (pre-enforcement) so
        // the server's own create_dir_all later cannot hit EACCES on its
        // ungranted parent. create_dir_all on an existing dir is a no-op.
        if std::fs::create_dir_all(dir).is_err() {
            skipped.push(dir.clone());
            continue;
        }
        match std::fs::canonicalize(dir) {
            Ok(canon) => push_unique(&mut read_write, canon),
            Err(_) => skipped.push(dir.clone()),
        }
    }
    if spec.allow_tmp {
        let tmp = std::env::temp_dir();
        match std::fs::canonicalize(&tmp) {
            Ok(canon) => push_unique(&mut read_write, canon),
            Err(_) => skipped.push(tmp),
        }
    }
    let mut read_only: Vec<PathBuf> = Vec::new();
    for dir in &spec.read_only_dirs {
        match std::fs::canonicalize(dir) {
            // A path already granted read-write needs no read-only rule:
            // Landlock unions allowed_access across a domain's rules.
            Ok(canon) if read_write.contains(&canon) => {}
            Ok(canon) => push_unique(&mut read_only, canon),
            Err(_) => skipped.push(dir.clone()),
        }
    }
    SandboxPlan {
        read_only,
        read_write,
        skipped_missing: skipped,
    }
}

fn push_unique(list: &mut Vec<PathBuf>, path: PathBuf) {
    if !list.contains(&path) {
        list.push(path);
    }
}

/// The running kernel's Landlock ABI version, `None` when the LSM is
/// absent (pre-5.13 kernel, `landlock` missing from `lsm=`, or the
/// syscall seccomp-filtered). Always `None` off Linux.
pub fn detect_landlock_abi() -> Option<u32> {
    #[cfg(target_os = "linux")]
    {
        sys_linux::kernel_abi_version()
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

/// linux/landlock.h filesystem access-rights mask valid on kernel ABI
/// `abi`. Requesting a bit the running ABI predates fails
/// `create_ruleset` with `EINVAL`, so the mask is stepped strictly by
/// version: `REFER` joins at v2, `TRUNCATE` at v3, `IOCTL_DEV` at v5.
/// ABI v6 (scopes) and v7 (audit-logging flags) add no filesystem bit.
#[doc(hidden)]
pub fn fs_access_mask_for_abi(abi: u32) -> u64 {
    const V1_MASK: u64 = (1 << 13) - 1; // EXECUTE..MAKE_SYM, bits 0–12
    let mut mask = V1_MASK;
    if abi >= 2 {
        mask |= 1 << 13; // LANDLOCK_ACCESS_FS_REFER (Linux 5.19)
    }
    if abi >= 3 {
        mask |= 1 << 14; // LANDLOCK_ACCESS_FS_TRUNCATE (Linux 6.2)
    }
    if abi >= 5 {
        mask |= 1 << 15; // LANDLOCK_ACCESS_FS_IOCTL_DEV (Linux 6.10)
    }
    mask
}

/// The read-only grant set beneath a root: list, read and execute, and
/// nothing else. `REFER` and `TRUNCATE` stay *handled* (the ruleset still
/// governs them globally) but are never granted to a read-only path —
/// re-linking or truncating inside it is write-flavoured.
#[doc(hidden)]
pub fn read_only_access_mask() -> u64 {
    // EXECUTE | READ_FILE | READ_DIR
    (1 << 0) | (1 << 2) | (1 << 3)
}

/// Apply `spec` to the calling thread. See the module docs for the
/// [`SandboxOutcome`] contract; the function itself never panics and
/// never leaks a descriptor.
///
/// Set [`DISABLE_ENV_VAR`] to skip enforcement entirely, e.g. inside a
/// nested-sandbox CI container where `restrict_self` would fail
/// confusingly.
pub fn enforce_sandbox(spec: &ServerSandboxSpec) -> SandboxOutcome {
    if sandbox_disabled_by_env() {
        return SandboxOutcome::Disabled;
    }
    sys::enforce(spec)
}

fn sandbox_disabled_by_env() -> bool {
    std::env::var_os(DISABLE_ENV_VAR).is_some_and(|v| {
        let v = v.to_string_lossy();
        !(v.is_empty() || v == "0" || v.eq_ignore_ascii_case("false"))
    })
}

#[cfg(target_os = "linux")]
use sys_linux as sys;
#[cfg(not(target_os = "linux"))]
use sys_other as sys;

#[cfg(target_os = "linux")]
mod sys_linux {
    //! The audited syscall surface — the only `unsafe` in the crate:
    //! three asm-generic Landlock syscalls (numbers 444–446, identical on
    //! every Linux architecture) reached through the C library's
    //! `syscall(2)` trampoline, plus `prctl(2)` for `PR_SET_NO_NEW_PRIVS`.
    //! glibc and musl both export these symbols; declaring the prototypes
    //! here keeps the crate on its zero-external-crates contract. All
    //! variadic arguments are passed at full `c_ulong`/`usize` width so
    //! the callee's `long`-sized varargs fetches read defined bits on
    //! every ABI, including 32-bit targets.
    use super::{fs_access_mask_for_abi, read_only_access_mask, SandboxOutcome, ServerSandboxSpec};
    use std::fs::File;
    use std::io;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
    use std::os::raw::{c_int, c_long, c_uint, c_ulong, c_void};
    use std::path::Path;

    unsafe extern "C" {
        fn syscall(num: c_long, ...) -> c_long;
        fn prctl(option: c_int, ...) -> c_int;
    }

    const SYS_LANDLOCK_CREATE_RULESET: c_long = 444;
    const SYS_LANDLOCK_ADD_RULE: c_long = 445;
    const SYS_LANDLOCK_RESTRICT_SELF: c_long = 446;
    const PR_SET_NO_NEW_PRIVS: c_int = 38;

    const LANDLOCK_CREATE_RULESET_VERSION: c_uint = 1;
    const LANDLOCK_RULE_TYPE_PATH_BENEATH: c_ulong = 1;

    /// `struct landlock_ruleset_attr` — `handled_access_fs` exists since
    /// ABI v1; `handled_access_net` was appended by ABI v4 (Linux 6.7),
    /// and `scoped` by ABI v6 (Linux 6.12). ABI v7 (Linux 6.15) adds
    /// audit-logging flags to `landlock_restrict_self`, whose zero/default
    /// logging behaviour is exactly what this self-sandboxing server wants.
    /// Pass the longest prefix the running ABI knows: 8 bytes on v1–v3,
    /// 16 on v4–v5 and 24 on v6+.
    #[repr(C)]
    struct RulesetAttr {
        handled_access_fs: u64,
        handled_access_net: u64,
        scoped: u64,
    }

    /// `struct landlock_path_beneath_attr` — unchanged since ABI v1.
    #[repr(C)]
    struct PathBeneathAttr {
        allowed_access: u64,
        parent_fd: c_int,
    }

    /// Kernel-reported Landlock ABI version (`>= 1`), `None` when absent.
    /// The version query is `create_ruleset(NULL, 0, VERSION)`.
    pub(super) fn kernel_abi_version() -> Option<u32> {
        // SAFETY: null attr + size 0 + the VERSION flag is the documented
        // probe; it touches no user memory and returns an int, not an fd.
        let rc = unsafe {
            syscall(
                SYS_LANDLOCK_CREATE_RULESET,
                std::ptr::null::<c_void>(),
                0usize,
                LANDLOCK_CREATE_RULESET_VERSION as c_ulong,
            )
        };
        if rc > 0 {
            Some(rc as u32)
        } else {
            None
        }
    }

    /// One `landlock_add_rule(PATH_BENEATH)` call for `dir` beneath
    /// `ruleset_fd`, granting `allowed_access`. `Ok(false)` when `dir`
    /// cannot be opened — its rule is simply absent, which denies the
    /// path, strictly safer than granting an unverifiable one.
    fn add_path_beneath(ruleset_fd: RawFd, dir: &Path, allowed_access: u64) -> io::Result<bool> {
        let file = match File::open(dir) {
            Ok(f) => f,
            Err(_) => return Ok(false),
        };
        let attr = PathBeneathAttr {
            allowed_access,
            parent_fd: file.as_raw_fd(),
        };
        // SAFETY: `ruleset_fd` and `file` are live descriptors; `attr`
        // outlives the call and the kernel copies it synchronously.
        let rc = unsafe {
            syscall(
                SYS_LANDLOCK_ADD_RULE,
                ruleset_fd as c_ulong,
                LANDLOCK_RULE_TYPE_PATH_BENEATH,
                &attr as *const PathBeneathAttr,
                0 as c_ulong,
            )
        };
        if rc == 0 {
            Ok(true)
        } else {
            Err(io::Error::last_os_error())
        }
    }

    pub(super) fn enforce(spec: &ServerSandboxSpec) -> SandboxOutcome {
        let Some(abi) = kernel_abi_version() else {
            return spec.unenforceable_outcome(
                SandboxOutcome::UnsupportedKernel,
                "kernel does not support Landlock (< 5.13, LSM disabled, or syscall filtered)",
            );
        };
        let plan = super::resolve_sandbox_plan(spec);
        let handled_fs = fs_access_mask_for_abi(abi);
        let attr = RulesetAttr {
            handled_access_fs: handled_fs,
            // Zero network rights handled — Landlock leaves TCP alone.
            handled_access_net: 0,
            // This lane jails filesystem access, not IPC. Keep ABI v6
            // scope bits unhandled while still using the correct v6+
            // structure width; ABI v7 audit logging needs no opt-in flags.
            scoped: 0,
        };
        // ABI v4+ kernels know `handled_access_net`, and ABI v6+ kernels
        // know the additional `scoped` word; older ones must get their
        // exact prefix length or they answer EINVAL.
        let attr_size = if abi >= 6 {
            std::mem::size_of::<RulesetAttr>()
        } else if abi >= 4 {
            std::mem::size_of::<u64>() * 2
        } else {
            std::mem::size_of::<u64>()
        };
        // SAFETY: `attr` is a valid ruleset_attr of `attr_size` bytes.
        // On success the kernel returns a fresh fd we wrap immediately.
        let fd = unsafe {
            syscall(
                SYS_LANDLOCK_CREATE_RULESET,
                &attr as *const RulesetAttr,
                attr_size,
                0 as c_ulong,
            )
        };
        if fd < 0 {
            return SandboxOutcome::Failed(format!(
                "landlock_create_ruleset failed: {}",
                io::Error::last_os_error()
            ));
        }
        // SAFETY: `fd` is a fresh descriptor owned by this thread; OwnedFd
        // closes it exactly once on drop — including every early return.
        let ruleset_fd = unsafe { OwnedFd::from_raw_fd(fd as RawFd) };

        let ro_mask = read_only_access_mask();
        let mut ro_rules = 0usize;
        for dir in &plan.read_only {
            match add_path_beneath(ruleset_fd.as_raw_fd(), dir, ro_mask) {
                Ok(true) => ro_rules += 1,
                Ok(false) => {}
                Err(e) => {
                    return SandboxOutcome::Failed(format!(
                        "landlock_add_rule(read-only, {}) failed: {e}",
                        dir.display()
                    ))
                }
            }
        }
        let mut rw_rules = 0usize;
        for dir in &plan.read_write {
            match add_path_beneath(ruleset_fd.as_raw_fd(), dir, handled_fs) {
                Ok(true) => rw_rules += 1,
                Ok(false) => {}
                Err(e) => {
                    return SandboxOutcome::Failed(format!(
                        "landlock_add_rule(read-write, {}) failed: {e}",
                        dir.display()
                    ))
                }
            }
        }

        // restrict_self refuses a process that could gain privilege; the
        // no_new_privs bit is the documented precondition (or CAP_SYS_ADMIN,
        // which a dedicated server must not need).
        // SAFETY: plain flag-setting call, no pointers involved.
        if unsafe {
            prctl(
                PR_SET_NO_NEW_PRIVS,
                1 as c_ulong,
                0 as c_ulong,
                0 as c_ulong,
                0 as c_ulong,
            )
        } != 0
        {
            return SandboxOutcome::Failed(format!(
                "prctl(PR_SET_NO_NEW_PRIVS) failed: {}",
                io::Error::last_os_error()
            ));
        }
        // SAFETY: `ruleset_fd` is live; flags 0 requests no scoping.
        if unsafe {
            syscall(
                SYS_LANDLOCK_RESTRICT_SELF,
                ruleset_fd.as_raw_fd() as c_ulong,
                0 as c_ulong,
            )
        } != 0
        {
            return SandboxOutcome::Failed(format!(
                "landlock_restrict_self failed: {}",
                io::Error::last_os_error()
            ));
        }
        SandboxOutcome::Enforced {
            abi_version: abi,
            ro_rules,
            rw_rules,
        }
    }
}

#[cfg(not(target_os = "linux"))]
mod sys_other {
    use super::{SandboxOutcome, ServerSandboxSpec};

    /// Landlock is a Linux LSM; other platforms carry their own sandbox
    /// (`unveil` on OpenBSD, `sandbox-exec` on macOS) which this lane does
    /// not implement — degrade cleanly or fail closed per `best_effort`.
    pub(super) fn enforce(spec: &ServerSandboxSpec) -> SandboxOutcome {
        spec.unenforceable_outcome(
            SandboxOutcome::UnsupportedOs,
            "Landlock is a Linux LSM; this platform cannot enforce it",
        )
    }
}
