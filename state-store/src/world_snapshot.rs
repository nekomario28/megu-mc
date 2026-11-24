// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! Pre-mod-change world snapshots and 1-click rollback.
//!
//! [`WorldSnapshotStore`] keeps a content-addressed object store under a
//! caller-chosen `backup_root`:
//!
//! ```text
//!   <backup_root>/
//!     objects/<sha256[0..2]>/<sha256>     one blob per unique file content
//!     snapshots/<snapshot_id>.json        SnapshotManifest document
//! ```
//!
//! Every world file (region `.mca`, `level.dat`, `world_overlay.bin`, mod
//! data, anything under the world root) is hashed once; the digest names the
//! blob. A second snapshot re-hashes the world but only writes blobs whose
//! content is new, so unchanged files are stored exactly once across all
//! snapshots — pre-mod-change snapshots are cheap in both time and space.
//!
//! The design fails closed everywhere a hostile or corrupt path could reach:
//!
//! * [`Sha256`] wraps the canonical `megu_core::crypto` FIPS 180-4
//!   implementation — zero external crates, no wall-clock reads
//!   (`timestamp_secs` is always
//!   caller-supplied, so the same world at the same timestamp produces the
//!   same snapshot id).
//! * [`WorldSnapshotStore::create_snapshot`] refuses symlinks, `..`,
//!   absolute components, NUL bytes, non-UTF-8 names and special files
//!   (fifos/sockets/devices) anywhere under the world root.
//! * Manifests are strict JSON (`"format": "megu-world-snapshot-v1"`),
//!   parsed back through the crate's pure-`std` JSON codec with every
//!   relative path re-validated on load — a hand-edited `../escape` entry
//!   can never drive a write outside the staging tree.
//! * [`WorldSnapshotStore::verify_snapshot`] re-hashes every referenced
//!   object and refuses on any missing or corrupted byte.
//! * [`WorldSnapshotStore::restore_snapshot`] verifies the target snapshot
//!   before touching the live world, optionally takes a `pre-rollback`
//!   safety snapshot (so a rollback is itself undoable), builds the
//!   restored tree in a sibling staging directory, fsyncs every file, and
//!   swaps directories — a mid-restore failure can leave the old world
//!   renamed but never half-written. Files added after the snapshot are
//!   swept away with the replaced tree; a `backup_root` nested inside the
//!   world is moved into the staged tree so the backup itself survives.
//! * [`WorldSnapshotStore::prune_oldest`] enforces retention by deleting
//!   the oldest manifests and garbage-collecting only the blobs no kept
//!   manifest still references.

use crate::datafix::DataValue;
use crate::migrate::{parse_data_json, to_data_json, write_file_atomic};
use megu_core::crypto::{hex_encode as hex_lower, sha256_hex};
use std::collections::{BTreeMap, HashSet};
use std::error::Error;
use std::fmt;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// Manifest format tag, checked on every load so a foreign or corrupt
/// document can never be mistaken for a snapshot.
const MANIFEST_FORMAT: &str = "megu-world-snapshot-v1";

/// Fail-closed caps. They sit far past any real save — they exist to refuse
/// garbage, never to bound legitimate worlds.
/// Files under one world root.
const MAX_WORLD_FILES: usize = 4_000_000;
/// One relative path's byte length.
const MAX_REL_PATH_LEN: usize = 4_096;
/// Entries a single manifest may list.
const MAX_MANIFEST_FILES: usize = 4_000_000;
/// Bytes a manifest document may occupy (16 MiB).
const MAX_MANIFEST_BYTES: u64 = 16 * 1_048_576;
/// Stream buffer for hashing and copying.
const IO_BUF: usize = 65_536;

/// Unique temp-dir names within one process: PID separates processes, the
/// counter separates attempts. No clock is involved.
static STAGING_COUNTER: AtomicU64 = AtomicU64::new(0);

// --------------------------------------------------------------- SHA-256

/// Streaming FIPS 180-4 SHA-256 — a thin wrapper over the canonical
/// `megu_core::crypto::Sha256` keeping this crate's historical method names
/// (`finish`/`digest`) stable for callers.
#[derive(Clone)]
pub struct Sha256(megu_core::crypto::Sha256);

impl Sha256 {
    pub fn new() -> Self {
        Self(megu_core::crypto::Sha256::new())
    }

    pub fn update(&mut self, bytes: &[u8]) {
        self.0.update(bytes);
    }

    pub fn finish(self) -> [u8; 32] {
        self.0.finalize()
    }

    /// One-shot digest of `bytes`.
    pub fn digest(bytes: &[u8]) -> [u8; 32] {
        megu_core::crypto::sha256(bytes)
    }
}

impl Default for Sha256 {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------- errors

/// Why a snapshot-store operation failed.
#[derive(Debug)]
pub enum WorldSnapshotError {
    /// An underlying filesystem call failed; `op` names what was attempted.
    Io {
        op: &'static str,
        path: PathBuf,
        source: io::Error,
    },
    /// The world root does not exist or is not a directory.
    WorldNotFound(PathBuf),
    /// A snapshot id contains characters outside `[A-Za-z0-9._-]` or is a
    /// `.`/`..` segment — it can never name a path.
    UnsafeSnapshotId(String),
    /// A relative path is unsafe: symlink, `..`, absolute, empty or `.`
    /// component, NUL byte, backslash, non-UTF-8 name, or a special file.
    UnsafePath {
        rel_path: String,
        reason: &'static str,
    },
    /// A snapshot id names no manifest under `snapshots/`.
    SnapshotNotFound(String),
    /// A manifest parsed but is internally inconsistent or malformed.
    Manifest { snapshot_id: String, reason: String },
    /// An object blob named by a manifest is absent from the store.
    MissingObject { sha256: String },
    /// An object blob's bytes do not match the digest a manifest recorded.
    Integrity {
        snapshot_id: String,
        rel_path: String,
        expected: String,
        actual: String,
    },
    /// An object blob's size disagrees with the manifest entry.
    SizeMismatch {
        snapshot_id: String,
        rel_path: String,
        expected: u64,
        actual: u64,
    },
}

impl PartialEq for WorldSnapshotError {
    fn eq(&self, other: &Self) -> bool {
        use WorldSnapshotError::*;
        match (self, other) {
            (
                Io {
                    op: o1,
                    path: p1,
                    source: s1,
                },
                Io {
                    op: o2,
                    path: p2,
                    source: s2,
                },
            ) => o1 == o2 && p1 == p2 && s1.kind() == s2.kind(),
            (WorldNotFound(a), WorldNotFound(b)) => a == b,
            (UnsafeSnapshotId(a), UnsafeSnapshotId(b)) => a == b,
            (
                UnsafePath {
                    rel_path: r1,
                    reason: s1,
                },
                UnsafePath {
                    rel_path: r2,
                    reason: s2,
                },
            ) => r1 == r2 && s1 == s2,
            (SnapshotNotFound(a), SnapshotNotFound(b)) => a == b,
            (
                Manifest {
                    snapshot_id: a,
                    reason: r1,
                },
                Manifest {
                    snapshot_id: b,
                    reason: r2,
                },
            ) => a == b && r1 == r2,
            (MissingObject { sha256: a }, MissingObject { sha256: b }) => a == b,
            (
                Integrity {
                    snapshot_id: a,
                    rel_path: r1,
                    expected: e1,
                    actual: c1,
                },
                Integrity {
                    snapshot_id: b,
                    rel_path: r2,
                    expected: e2,
                    actual: c2,
                },
            ) => a == b && r1 == r2 && e1 == e2 && c1 == c2,
            (
                SizeMismatch {
                    snapshot_id: a,
                    rel_path: r1,
                    expected: e1,
                    actual: c1,
                },
                SizeMismatch {
                    snapshot_id: b,
                    rel_path: r2,
                    expected: e2,
                    actual: c2,
                },
            ) => a == b && r1 == r2 && e1 == e2 && c1 == c2,
            _ => false,
        }
    }
}

impl fmt::Display for WorldSnapshotError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        use WorldSnapshotError::*;
        match self {
            Io { op, path, source } => write!(f, "{op} failed on {}: {source}", path.display()),
            WorldNotFound(p) => write!(f, "world root {} is not a directory", p.display()),
            UnsafeSnapshotId(id) => write!(f, "snapshot id {id:?} is not a safe file name"),
            UnsafePath { rel_path, reason } => {
                write!(f, "world path {rel_path:?} refused: {reason}")
            }
            SnapshotNotFound(id) => write!(f, "no snapshot manifest for {id:?}"),
            Manifest {
                snapshot_id,
                reason,
            } => {
                write!(f, "snapshot manifest {snapshot_id:?} is invalid: {reason}")
            }
            MissingObject { sha256 } => write!(f, "object blob {sha256} is missing from the store"),
            Integrity {
                snapshot_id,
                rel_path,
                expected,
                actual,
            } => write!(
                f,
                "snapshot {snapshot_id:?}: object for {rel_path:?} failed integrity \
                 (expected sha256 {expected}, computed {actual})"
            ),
            SizeMismatch {
                snapshot_id,
                rel_path,
                expected,
                actual,
            } => write!(
                f,
                "snapshot {snapshot_id:?}: object for {rel_path:?} has size {actual}, \
                 manifest recorded {expected}"
            ),
        }
    }
}

impl Error for WorldSnapshotError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            WorldSnapshotError::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

fn ioe(op: &'static str, path: &Path) -> impl FnOnce(io::Error) -> WorldSnapshotError {
    let path = path.to_path_buf();
    move |source| WorldSnapshotError::Io { op, path, source }
}

// ------------------------------------------------------------- data types

/// Why a snapshot was taken. Serialized into the manifest so an operator can
/// later see whether a snapshot was manual, a pre-mod-change guard, a
/// pre-migration guard, scheduled, or a rollback's own safety net.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SnapshotReason {
    /// Operator- or UI-triggered snapshot.
    Manual,
    /// Taken just before a mod install/update/removal so the change is
    /// undoable in one click. `summary` is a short human-readable label
    /// (e.g. `"install create-6.0"`).
    PreModChange { summary: String },
    /// Taken just before a world-format migration.
    PreWorldMigrate,
    /// Taken by a periodic backup schedule.
    Scheduled,
    /// Safety snapshot taken automatically inside a restore, so a rollback
    /// can itself be rolled back.
    PreRollback,
}

impl SnapshotReason {
    fn kind(&self) -> &'static str {
        match self {
            SnapshotReason::Manual => "manual",
            SnapshotReason::PreModChange { .. } => "pre_mod_change",
            SnapshotReason::PreWorldMigrate => "pre_world_migrate",
            SnapshotReason::Scheduled => "scheduled",
            SnapshotReason::PreRollback => "pre_rollback",
        }
    }

    fn from_kind(kind: &str, summary: Option<String>) -> Result<Self, String> {
        match kind {
            "manual" => Ok(SnapshotReason::Manual),
            "pre_mod_change" => Ok(SnapshotReason::PreModChange {
                summary: summary.unwrap_or_default(),
            }),
            "pre_world_migrate" => Ok(SnapshotReason::PreWorldMigrate),
            "scheduled" => Ok(SnapshotReason::Scheduled),
            "pre_rollback" => Ok(SnapshotReason::PreRollback),
            other => Err(format!("unknown reason kind {other:?}")),
        }
    }
}

/// One file's manifest record: where it lives in the world, how large it is,
/// and which object blob holds its bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SnapshotFileEntry {
    /// `/`-separated path relative to the world root, validated safe.
    pub rel_path: String,
    /// File length in bytes — cross-checked against the object blob.
    pub size: u64,
    /// Lowercase-hex SHA-256 of the file's bytes; also the object name.
    pub sha256: String,
}

/// A stored snapshot's full manifest.
#[derive(Clone, Debug, PartialEq)]
pub struct SnapshotManifest {
    /// Unique snapshot identifier; the manifest lives at
    /// `snapshots/<snapshot_id>.json`.
    pub snapshot_id: String,
    /// Caller-supplied snapshot time, seconds since the Unix epoch.
    pub timestamp_secs: u64,
    /// Why the snapshot was taken.
    pub reason: SnapshotReason,
    /// Minecraft version string the world was running (e.g. `"26.3"`).
    pub mc_version: String,
    /// `files.len()`, stored for quick manifest inspection.
    pub file_count: u64,
    /// Sum of every entry's `size`.
    pub total_bytes: u64,
    /// Every snapshotted file, sorted by `rel_path`.
    pub files: Vec<SnapshotFileEntry>,
}

/// Result of [`WorldSnapshotStore::verify_snapshot`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerifyReport {
    /// Objects re-hashed and matched.
    pub files_verified: usize,
    /// Total bytes re-hashed.
    pub bytes_verified: u64,
}

/// Result of [`WorldSnapshotStore::restore_snapshot`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RestoreReport {
    /// The snapshot that was restored.
    pub snapshot_id: String,
    /// Files written into the staged tree.
    pub restored_files: usize,
    /// Bytes written into the staged tree.
    pub restored_bytes: u64,
    /// Regular files swept away with the replaced world tree (everything
    /// the snapshot did not cover, including post-snapshot additions).
    pub swept_files: usize,
    /// The `pre_rollback` safety snapshot taken before the swap, when
    /// `create_safety_backup` was on.
    pub safety_snapshot_id: Option<String>,
}

/// Result of [`WorldSnapshotStore::prune_oldest`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PruneReport {
    /// Snapshot ids whose manifests were deleted, oldest first.
    pub removed_snapshots: Vec<String>,
    /// Manifests kept.
    pub kept_snapshots: usize,
    /// Unreferenced object blobs deleted.
    pub removed_objects: usize,
}

// ------------------------------------------------------------ path safety

/// The jail every world-relative path must pass, on both backup and
/// restore. `rel` is a `/`-separated relative path: every component must be
/// non-empty and neither `.` nor `..`, no leading `:`/`/` makes it absolute,
/// and NUL/backslash are refused outright (a backslash is a separator on
/// Windows, so allowing it would reopen traversal there).
fn validate_rel_path(rel: &str) -> Result<(), WorldSnapshotError> {
    let reject = |reason: &'static str| WorldSnapshotError::UnsafePath {
        rel_path: rel.to_string(),
        reason,
    };
    if rel.is_empty() {
        return Err(reject("empty path"));
    }
    if rel.len() > MAX_REL_PATH_LEN {
        return Err(reject("path exceeds the 4096-byte cap"));
    }
    if rel.bytes().any(|b| b == 0) {
        return Err(reject("NUL byte in path"));
    }
    if rel.starts_with('/') || rel.starts_with('\\') {
        return Err(reject("absolute path"));
    }
    if rel.bytes().any(|b| b == b'\\') {
        return Err(reject("backslash separator"));
    }
    for component in rel.split('/') {
        if component.is_empty() {
            return Err(reject("empty path component"));
        }
        if component == "." || component == ".." {
            return Err(reject("dot component"));
        }
    }
    Ok(())
}

/// Snapshot ids become file names under `snapshots/`, so they must be a
/// single safe path component.
fn validate_snapshot_id(id: &str) -> Result<(), WorldSnapshotError> {
    let safe = !id.is_empty()
        && id != "."
        && id != ".."
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_'));
    if safe {
        Ok(())
    } else {
        Err(WorldSnapshotError::UnsafeSnapshotId(id.to_string()))
    }
}

/// True when `sha` is exactly 64 lowercase hex digits — the only spelling
/// this store writes and accepts.
fn valid_sha256_hex(sha: &str) -> bool {
    sha.len() == 64
        && sha
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}

// ------------------------------------------------------------------ store

/// A content-addressed world snapshot store rooted at `backup_root`,
/// snapshotting the live world directory `world_root`.
///
/// `backup_root` is normally a `.megu-snapshots/` sibling next to the world
/// (so a restore's swap can never touch it). Nesting it inside the world is
/// also supported: creation skips the backup subtree, and restore moves it
/// into the staged tree so the store — including the `pre_rollback` safety
/// snapshot — survives the swap.
pub struct WorldSnapshotStore {
    world_root: PathBuf,
    backup_root: PathBuf,
}

/// One world file's (relative path, absolute path) pair.
struct CollectedFile {
    rel: String,
    abs: PathBuf,
}

/// Canonicalized pair resolved lazily (the dirs need not exist at `new`):
/// canonical when the path exists, lexical-absolute otherwise.
struct Roots {
    world: PathBuf,
    backup: PathBuf,
}

impl WorldSnapshotStore {
    /// Create a store handle. Nothing is touched on disk until an operation
    /// runs; `world_root` must exist and be a real directory when
    /// [`create_snapshot`](Self::create_snapshot) is called, and both roots
    /// must not be symlinks.
    pub fn new(
        world_root: impl Into<PathBuf>,
        backup_root: impl Into<PathBuf>,
    ) -> Result<Self, WorldSnapshotError> {
        let store = Self {
            world_root: world_root.into(),
            backup_root: backup_root.into(),
        };
        // A symlinked root would let the jail's string checks pass while the
        // filesystem escapes them — refuse both roots up front.
        for (what, path) in [
            ("world root", &store.world_root),
            ("backup root", &store.backup_root),
        ] {
            match std::fs::symlink_metadata(path) {
                Ok(meta) if meta.file_type().is_symlink() => {
                    return Err(WorldSnapshotError::UnsafePath {
                        rel_path: path.display().to_string(),
                        reason: if what == "world root" {
                            "world root is a symlink"
                        } else {
                            "backup root is a symlink"
                        },
                    });
                }
                _ => {}
            }
        }
        Ok(store)
    }

    fn resolve(path: &Path) -> Result<PathBuf, WorldSnapshotError> {
        match std::fs::canonicalize(path) {
            Ok(p) => Ok(p),
            Err(_) => std::path::absolute(path).map_err(ioe("absolutize", path)),
        }
    }

    /// Absolute world/backup roots, canonicalized when present so a `starts_with`
    /// prefix test is meaningful.
    fn roots(&self) -> Result<Roots, WorldSnapshotError> {
        Ok(Roots {
            world: Self::resolve(&self.world_root)?,
            backup: Self::resolve(&self.backup_root)?,
        })
    }

    fn objects_dir(&self) -> PathBuf {
        self.backup_root.join("objects")
    }

    fn snapshots_dir(&self) -> PathBuf {
        self.backup_root.join("snapshots")
    }

    fn object_path(&self, sha256: &str) -> PathBuf {
        self.objects_dir().join(&sha256[..2]).join(sha256)
    }

    fn manifest_path(&self, snapshot_id: &str) -> PathBuf {
        self.snapshots_dir().join(format!("{snapshot_id}.json"))
    }

    /// Walk the world root, refusing anything unsafe. Returns entries sorted
    /// by relative path so manifests are deterministic.
    fn collect_world_files(&self, roots: &Roots) -> Result<Vec<CollectedFile>, WorldSnapshotError> {
        let meta = std::fs::metadata(&self.world_root)
            .map_err(ioe("stat world root", &self.world_root))?;
        if !meta.is_dir() {
            return Err(WorldSnapshotError::WorldNotFound(self.world_root.clone()));
        }

        let mut out = Vec::new();
        let mut stack = vec![self.world_root.clone()];
        while let Some(dir) = stack.pop() {
            let mut entries = Vec::new();
            for entry in std::fs::read_dir(&dir).map_err(ioe("read world dir", &dir))? {
                entries.push(entry.map_err(ioe("read world dir entry", &dir))?);
            }
            // read_dir order is filesystem-defined; sort for determinism.
            entries.sort_by_key(|e| e.file_name());
            for entry in entries {
                let abs = entry.path();
                let rel_os = abs
                    .strip_prefix(&self.world_root)
                    .expect("walked path is under world root")
                    .to_path_buf();
                let rel = rel_os
                    .to_str()
                    .ok_or_else(|| WorldSnapshotError::UnsafePath {
                        rel_path: rel_os.to_string_lossy().into_owned(),
                        reason: "non-UTF-8 file name",
                    })?
                    .replace(std::path::MAIN_SEPARATOR, "/");
                // `file_type` never traverses a symlink (it is the lstat
                // type), so this test is race-safe for our purposes.
                let file_type = entry.file_type().map_err(ioe("stat world entry", &abs))?;
                if file_type.is_symlink() {
                    return Err(WorldSnapshotError::UnsafePath {
                        rel_path: rel,
                        reason: "symlink",
                    });
                }
                if file_type.is_dir() {
                    // Never descend into the backup store when it is nested
                    // inside the world tree.
                    let abs_resolved = Self::resolve(&abs)?;
                    if abs_resolved == roots.backup {
                        continue;
                    }
                    stack.push(abs);
                    continue;
                }
                if !file_type.is_file() {
                    return Err(WorldSnapshotError::UnsafePath {
                        rel_path: rel,
                        reason: "special file (fifo/socket/device)",
                    });
                }
                validate_rel_path(&rel)?;
                if out.len() >= MAX_WORLD_FILES {
                    return Err(WorldSnapshotError::UnsafePath {
                        rel_path: rel,
                        reason: "world file count exceeds cap",
                    });
                }
                out.push(CollectedFile { rel, abs });
            }
        }
        out.sort_by(|a, b| a.rel.cmp(&b.rel));
        Ok(out)
    }

    /// Stream a file through SHA-256, returning (size, hex digest). The
    /// symlink check happens on `symlink_metadata` immediately before the
    /// open, so a planted symlink is refused rather than followed.
    fn hash_file(path: &Path, rel_for_err: &str) -> Result<(u64, String), WorldSnapshotError> {
        let meta = std::fs::symlink_metadata(path).map_err(ioe("stat file", path))?;
        if meta.file_type().is_symlink() {
            return Err(WorldSnapshotError::UnsafePath {
                rel_path: rel_for_err.to_string(),
                reason: "symlink",
            });
        }
        let mut file = std::fs::File::open(path).map_err(ioe("open file", path))?;
        let mut hasher = Sha256::new();
        let mut total = 0u64;
        let mut buf = [0u8; IO_BUF];
        loop {
            let n = file.read(&mut buf).map_err(ioe("read file", path))?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
            total += n as u64;
        }
        Ok((total, hex_lower(&hasher.finish())))
    }

    /// Write `src`'s bytes to `dst` (fsynced), refusing a symlinked source.
    /// `dst` is created with `create_new` so a pre-planted file — including a
    /// symlink — cannot be written through.
    fn copy_file_fsynced(
        &self,
        src: &Path,
        dst: &Path,
        rel_for_err: &str,
    ) -> Result<u64, WorldSnapshotError> {
        let meta = std::fs::symlink_metadata(src).map_err(ioe("stat object", src))?;
        if meta.file_type().is_symlink() {
            return Err(WorldSnapshotError::UnsafePath {
                rel_path: rel_for_err.to_string(),
                reason: "object blob is a symlink",
            });
        }
        let mut input = std::fs::File::open(src).map_err(ioe("open object", src))?;
        if let Some(parent) = dst.parent() {
            std::fs::create_dir_all(parent).map_err(ioe("create restore dir", parent))?;
        }
        let mut output = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(dst)
            .map_err(ioe("create restored file", dst))?;
        let mut written = 0u64;
        let mut buf = [0u8; IO_BUF];
        let copy_result = (|| -> io::Result<()> {
            loop {
                let n = input.read(&mut buf)?;
                if n == 0 {
                    return Ok(());
                }
                output.write_all(&buf[..n])?;
                written += n as u64;
            }
        })();
        let result = copy_result.and_then(|()| output.sync_all());
        drop(output);
        if let Err(e) = result {
            let _ = std::fs::remove_file(dst);
            return Err(ioe("write restored file", dst)(e));
        }
        Ok(written)
    }

    /// Install one world file's content into the object store. An existing
    /// blob with the same name and size is a dedup hit — the bytes are
    /// definitionally identical up to a SHA-256 collision, and
    /// [`verify_snapshot`](Self::verify_snapshot) re-hashes blobs to catch
    /// real corruption.
    fn install_object(
        &self,
        src: &Path,
        size: u64,
        sha256: &str,
    ) -> Result<(), WorldSnapshotError> {
        let dst = self.object_path(sha256);
        if let Ok(meta) = std::fs::symlink_metadata(&dst) {
            if meta.file_type().is_symlink() {
                return Err(WorldSnapshotError::UnsafePath {
                    rel_path: sha256.to_string(),
                    reason: "object blob is a symlink",
                });
            }
            if meta.len() != size {
                return Err(WorldSnapshotError::Manifest {
                    snapshot_id: String::new(),
                    reason: format!(
                        "object {sha256} exists with size {}, world file has {size}",
                        meta.len()
                    ),
                });
            }
            return Ok(()); // dedup hit — nothing to write
        }
        let parent = dst.parent().expect("object path has prefix dir");
        std::fs::create_dir_all(parent).map_err(ioe("create object dir", parent))?;
        // Unique temp name in the object dir (same filesystem for rename).
        let pid = std::process::id();
        let mut tmp = parent.join("unreachable");
        let mut staged = false;
        for _ in 0..64 {
            let seq = STAGING_COUNTER.fetch_add(1, Ordering::Relaxed);
            let candidate = parent.join(format!(".blob-{pid}-{seq}"));
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&candidate)
            {
                Ok(mut f) => {
                    let copy = (|| -> io::Result<()> {
                        let mut input = std::fs::File::open(src)?;
                        io::copy(&mut input, &mut f)?;
                        f.sync_all()
                    })();
                    drop(f);
                    match copy {
                        Ok(()) => {
                            tmp = candidate;
                            staged = true;
                            break;
                        }
                        Err(e) => {
                            let _ = std::fs::remove_file(&candidate);
                            return Err(ioe("write object blob", &candidate)(e));
                        }
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(ioe("create object blob", &candidate)(e)),
            }
        }
        if !staged {
            return Err(ioe("create object blob", parent)(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "could not allocate a unique blob temp name",
            )));
        }
        std::fs::rename(&tmp, &dst).map_err(ioe("commit object blob", &dst))?;
        if let Ok(dir) = std::fs::File::open(parent) {
            let _ = dir.sync_all();
        }
        Ok(())
    }

    /// Deterministic snapshot id: `snap-<timestamp>-<digest16>` where the
    /// digest covers the full manifest content, so identical world states
    /// snapshot to the same id and different states never collide under one.
    fn snapshot_id(
        timestamp_secs: u64,
        reason: &SnapshotReason,
        mc_version: &str,
        files: &[SnapshotFileEntry],
    ) -> String {
        let mut descriptor = String::from(MANIFEST_FORMAT);
        descriptor.push('\0');
        descriptor.push_str(&timestamp_secs.to_string());
        descriptor.push('\0');
        descriptor.push_str(reason.kind());
        descriptor.push('\0');
        if let SnapshotReason::PreModChange { summary } = reason {
            descriptor.push_str(summary);
        }
        descriptor.push('\0');
        descriptor.push_str(mc_version);
        for f in files {
            descriptor.push('\0');
            descriptor.push_str(&f.rel_path);
            descriptor.push('\0');
            descriptor.push_str(&f.size.to_string());
            descriptor.push('\0');
            descriptor.push_str(&f.sha256);
        }
        let digest = sha256_hex(descriptor.as_bytes());
        format!("snap-{timestamp_secs}-{}", &digest[..16])
    }

    /// Snapshot every regular file under the world root.
    ///
    /// Each file is hashed and its content installed into the deduplicated
    /// object store; the manifest is written atomically. `timestamp_secs`
    /// comes from the caller — this crate never reads the wall clock.
    pub fn create_snapshot(
        &self,
        reason: SnapshotReason,
        mc_version: &str,
        timestamp_secs: u64,
    ) -> Result<SnapshotManifest, WorldSnapshotError> {
        std::fs::create_dir_all(self.objects_dir())
            .map_err(ioe("create objects dir", &self.backup_root))?;
        std::fs::create_dir_all(self.snapshots_dir())
            .map_err(ioe("create snapshots dir", &self.backup_root))?;
        let roots = self.roots()?;
        let collected = self.collect_world_files(&roots)?;

        let mut files = Vec::with_capacity(collected.len());
        let mut total_bytes = 0u64;
        for file in collected {
            let (size, sha256) = Self::hash_file(&file.abs, &file.rel)?;
            self.install_object(&file.abs, size, &sha256)?;
            total_bytes += size;
            files.push(SnapshotFileEntry {
                rel_path: file.rel,
                size,
                sha256,
            });
        }
        files.sort_by(|a, b| a.rel_path.cmp(&b.rel_path));
        files.shrink_to_fit();

        let snapshot_id = Self::snapshot_id(timestamp_secs, &reason, mc_version, &files);
        let manifest = SnapshotManifest {
            file_count: files.len() as u64,
            total_bytes,
            snapshot_id,
            timestamp_secs,
            reason,
            mc_version: mc_version.to_string(),
            files,
        };
        let path = self.manifest_path(&manifest.snapshot_id);
        write_file_atomic(&path, manifest_to_json(&manifest).as_bytes())
            .map_err(ioe("write manifest", &path))?;
        Ok(manifest)
    }

    /// Load and fully validate one manifest.
    pub fn load_manifest(&self, snapshot_id: &str) -> Result<SnapshotManifest, WorldSnapshotError> {
        validate_snapshot_id(snapshot_id)?;
        let path = self.manifest_path(snapshot_id);
        let bytes = match std::fs::read(&path) {
            Ok(b) => b,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                return Err(WorldSnapshotError::SnapshotNotFound(
                    snapshot_id.to_string(),
                ));
            }
            Err(e) => return Err(ioe("read manifest", &path)(e)),
        };
        if bytes.len() as u64 > MAX_MANIFEST_BYTES {
            return Err(WorldSnapshotError::Manifest {
                snapshot_id: snapshot_id.to_string(),
                reason: "manifest exceeds the 16 MiB cap".to_string(),
            });
        }
        let text = String::from_utf8(bytes).map_err(|_| WorldSnapshotError::Manifest {
            snapshot_id: snapshot_id.to_string(),
            reason: "manifest is not UTF-8".to_string(),
        })?;
        let manifest = manifest_from_json(snapshot_id, &text)?;
        if manifest.snapshot_id != snapshot_id {
            return Err(WorldSnapshotError::Manifest {
                snapshot_id: snapshot_id.to_string(),
                reason: format!(
                    "manifest names snapshot {:?}, file is {snapshot_id:?}",
                    manifest.snapshot_id
                ),
            });
        }
        Ok(manifest)
    }

    /// Every manifest under `snapshots/`, sorted oldest-first by
    /// `(timestamp_secs, snapshot_id)`.
    pub fn list_snapshots(&self) -> Result<Vec<SnapshotManifest>, WorldSnapshotError> {
        let dir = self.snapshots_dir();
        let mut out = Vec::new();
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(out),
            Err(e) => return Err(ioe("list snapshots", &dir)(e)),
        };
        for entry in entries {
            let entry = entry.map_err(ioe("list snapshots", &dir))?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            if !name.ends_with(".json") {
                continue;
            }
            out.push(self.load_manifest(&name[..name.len() - 5])?);
        }
        out.sort_by(|a, b| {
            a.timestamp_secs
                .cmp(&b.timestamp_secs)
                .then_with(|| a.snapshot_id.cmp(&b.snapshot_id))
        });
        Ok(out)
    }

    /// Re-hash every object a snapshot references and fail closed on any
    /// missing or corrupted byte.
    pub fn verify_snapshot(&self, snapshot_id: &str) -> Result<VerifyReport, WorldSnapshotError> {
        let manifest = self.load_manifest(snapshot_id)?;
        let mut report = VerifyReport {
            files_verified: 0,
            bytes_verified: 0,
        };
        for entry in &manifest.files {
            let obj = self.object_path(&entry.sha256);
            let meta = match std::fs::symlink_metadata(&obj) {
                Ok(m) => m,
                Err(e) if e.kind() == io::ErrorKind::NotFound => {
                    return Err(WorldSnapshotError::MissingObject {
                        sha256: entry.sha256.clone(),
                    });
                }
                Err(e) => return Err(ioe("stat object", &obj)(e)),
            };
            if meta.file_type().is_symlink() {
                return Err(WorldSnapshotError::UnsafePath {
                    rel_path: entry.rel_path.clone(),
                    reason: "object blob is a symlink",
                });
            }
            if meta.len() != entry.size {
                return Err(WorldSnapshotError::SizeMismatch {
                    snapshot_id: manifest.snapshot_id.clone(),
                    rel_path: entry.rel_path.clone(),
                    expected: entry.size,
                    actual: meta.len(),
                });
            }
            let (_, actual) = Self::hash_file(&obj, &entry.rel_path)?;
            if actual != entry.sha256 {
                return Err(WorldSnapshotError::Integrity {
                    snapshot_id: manifest.snapshot_id.clone(),
                    rel_path: entry.rel_path.clone(),
                    expected: entry.sha256.clone(),
                    actual,
                });
            }
            report.files_verified += 1;
            report.bytes_verified += entry.size;
        }
        Ok(report)
    }

    /// Delete the oldest snapshot manifests beyond `max_keep`, then
    /// garbage-collect object blobs no kept manifest still references.
    /// Kept snapshots remain fully verifiable.
    pub fn prune_oldest(&self, max_keep: usize) -> Result<PruneReport, WorldSnapshotError> {
        let all = self.list_snapshots()?;
        if all.len() <= max_keep {
            return Ok(PruneReport {
                removed_snapshots: Vec::new(),
                kept_snapshots: all.len(),
                removed_objects: 0,
            });
        }
        let (removed, kept) = all.split_at(all.len() - max_keep);
        let mut live: HashSet<String> = HashSet::new();
        for m in kept {
            for f in &m.files {
                live.insert(f.sha256.clone());
            }
        }
        let mut report = PruneReport {
            removed_snapshots: removed.iter().map(|m| m.snapshot_id.clone()).collect(),
            kept_snapshots: kept.len(),
            removed_objects: 0,
        };
        for m in removed {
            let path = self.manifest_path(&m.snapshot_id);
            std::fs::remove_file(&path).map_err(ioe("remove manifest", &path))?;
        }
        // GC: delete blobs nothing kept references. A manifest that failed
        // to parse would already have failed `list_snapshots`, so an
        // unreadable manifest can never orphan its blobs into deletion.
        let objects = self.objects_dir();
        let prefixes = match std::fs::read_dir(&objects) {
            Ok(e) => e,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(report),
            Err(e) => return Err(ioe("list objects", &objects)(e)),
        };
        for entry in prefixes {
            let entry = entry.map_err(ioe("list objects", &objects))?;
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if !file_type.is_dir() || file_type.is_symlink() {
                continue;
            }
            let prefix = entry.path();
            for blob in std::fs::read_dir(&prefix).map_err(ioe("list object prefix", &prefix))? {
                let blob = blob.map_err(ioe("list object prefix", &prefix))?;
                let name = blob.file_name();
                let Some(name) = name.to_str() else { continue };
                if !valid_sha256_hex(name) {
                    // Temp debris from an interrupted install; sweep it.
                    let _ = std::fs::remove_file(blob.path());
                    continue;
                }
                if !live.contains(name) {
                    std::fs::remove_file(blob.path())
                        .map_err(ioe("remove orphaned object", &blob.path()))?;
                    report.removed_objects += 1;
                }
            }
            let _ = std::fs::remove_dir(&prefix); // removes only when empty
        }
        Ok(report)
    }

    /// Remove leftover `.megu-staging-*`/`.megu-old-*` siblings of the world
    /// from an interrupted restore. If the world root itself is missing but
    /// a staged or old tree survived, adopt the staged one (it was the
    /// intended post-restore state) or fall back to the old one.
    fn sweep_stale_restore_dirs(&self) -> Result<(), WorldSnapshotError> {
        let parent = self
            .world_root
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));
        let Some(world_name) = self.world_root.file_name().and_then(|n| n.to_str()) else {
            return Ok(());
        };
        let staging_prefix = format!("{world_name}.megu-staging-");
        let trash_prefix = format!("{world_name}.megu-old-");
        let mut staged = Vec::new();
        let mut trashed = Vec::new();
        for entry in std::fs::read_dir(&parent).map_err(ioe("scan world parent", &parent))? {
            let entry = entry.map_err(ioe("scan world parent", &parent))?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            if name.starts_with(&staging_prefix) {
                staged.push(entry.path());
            } else if name.starts_with(&trash_prefix) {
                trashed.push(entry.path());
            }
        }
        if !self.world_root.exists() {
            // A crash between the two swap renames leaves the world absent.
            // Adopt a complete staged tree if one exists, else roll back to
            // the preserved old tree. Nothing here is half-written: staging
            // was fully fsynced before the swap began.
            if let Some(dir) = staged.first() {
                std::fs::rename(dir, &self.world_root).map_err(ioe("adopt staged world", dir))?;
                staged.remove(0);
            } else if let Some(dir) = trashed.first() {
                std::fs::rename(dir, &self.world_root).map_err(ioe("recover old world", dir))?;
                trashed.remove(0);
            }
        }
        for dir in staged.iter().chain(trashed.iter()) {
            let _ = std::fs::remove_dir_all(dir);
        }
        Ok(())
    }

    /// Best-effort count of regular files under the world root (report
    /// telemetry only — read errors stop the count rather than fail).
    fn count_world_files(&self) -> usize {
        let mut count = 0usize;
        let mut stack = vec![self.world_root.clone()];
        let backup = Self::resolve(&self.backup_root).unwrap_or_else(|_| self.backup_root.clone());
        while let Some(dir) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                break;
            };
            for entry in entries.flatten() {
                let Ok(ft) = entry.file_type() else { continue };
                if ft.is_dir() && !ft.is_symlink() {
                    let abs = entry.path();
                    if Self::resolve(&abs).unwrap_or(abs.clone()) == backup {
                        continue;
                    }
                    stack.push(abs);
                } else if ft.is_file() {
                    count += 1;
                }
            }
        }
        count
    }

    /// Restore `snapshot_id` over the live world, keeping a `pre_rollback`
    /// safety snapshot (default) so the rollback is itself undoable.
    /// `timestamp_secs` is caller-supplied for the safety manifest.
    pub fn restore_snapshot(
        &self,
        snapshot_id: &str,
        timestamp_secs: u64,
    ) -> Result<RestoreReport, WorldSnapshotError> {
        self.restore_snapshot_opt(snapshot_id, true, timestamp_secs)
    }

    /// Restore `snapshot_id` over the live world.
    ///
    /// Order of operations is the safety contract:
    ///
    /// 1. Load and validate the manifest (every `rel_path` re-jailed).
    /// 2. Re-hash every referenced object — corrupt or missing blobs abort
    ///    before the live world is touched.
    /// 3. Optionally snapshot the current world as `pre_rollback`.
    /// 4. Build the restored tree in `<world>.megu-staging-<pid>-<seq>` next
    ///    to the world, fsyncing every file.
    /// 5. Move a nested `backup_root` into the staged tree so the store —
    ///    and the safety snapshot — survives.
    /// 6. Rename world aside, rename staging into place, delete the old
    ///    tree: post-snapshot additions are swept away wholesale.
    pub fn restore_snapshot_opt(
        &self,
        snapshot_id: &str,
        create_safety_backup: bool,
        timestamp_secs: u64,
    ) -> Result<RestoreReport, WorldSnapshotError> {
        let manifest = self.load_manifest(snapshot_id)?;
        // Full integrity check before a single byte of the live tree moves.
        self.verify_snapshot(snapshot_id)?;
        self.sweep_stale_restore_dirs()?;

        let world_existed = self.world_root.is_dir();
        let safety_snapshot_id = if create_safety_backup && world_existed {
            Some(
                self.create_snapshot(
                    SnapshotReason::PreRollback,
                    &manifest.mc_version,
                    timestamp_secs,
                )?
                .snapshot_id,
            )
        } else {
            None
        };

        let roots = self.roots()?;
        let parent = self
            .world_root
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));
        let world_name = self
            .world_root
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("world");
        let pid = std::process::id();
        let seq = STAGING_COUNTER.fetch_add(1, Ordering::Relaxed);
        let staging = parent.join(format!("{world_name}.megu-staging-{pid}-{seq}"));
        let trash = parent.join(format!("{world_name}.megu-old-{pid}-{seq}"));

        // Stage the restored tree. Any failure here deletes the staging dir
        // and leaves the live world exactly as it was.
        let staged = || -> Result<(usize, u64), WorldSnapshotError> {
            std::fs::create_dir_all(&staging).map_err(ioe("create staging dir", &staging))?;
            let mut restored_files = 0usize;
            let mut restored_bytes = 0u64;
            for entry in &manifest.files {
                let dst = rel_to_path(&staging, &entry.rel_path);
                let src = self.object_path(&entry.sha256);
                let written = self.copy_file_fsynced(&src, &dst, &entry.rel_path)?;
                if written != entry.size {
                    return Err(WorldSnapshotError::SizeMismatch {
                        snapshot_id: manifest.snapshot_id.clone(),
                        rel_path: entry.rel_path.clone(),
                        expected: entry.size,
                        actual: written,
                    });
                }
                restored_files += 1;
                restored_bytes += written;
            }
            // Preserve a backup store nested inside the world: move it into
            // the staged tree at the same relative location so it rides the
            // swap instead of being swept into the trash.
            if let Ok(rel) = roots.backup.strip_prefix(&roots.world) {
                if !rel.as_os_str().is_empty() && roots.backup.exists() {
                    let dst = staging.join(rel);
                    if let Some(p) = dst.parent() {
                        std::fs::create_dir_all(p).map_err(ioe("preserve backup dir", p))?;
                    }
                    std::fs::rename(&roots.backup, &dst)
                        .map_err(ioe("preserve backup dir", &roots.backup))?;
                }
            }
            // Fsync the staged root so its entries are durable before swap.
            if let Ok(dir) = std::fs::File::open(&staging) {
                let _ = dir.sync_all();
            }
            Ok((restored_files, restored_bytes))
        };

        let (restored_files, restored_bytes) = match staged() {
            Ok(v) => v,
            Err(e) => {
                let _ = std::fs::remove_dir_all(&staging);
                return Err(e);
            }
        };

        let swept_files = if world_existed {
            self.count_world_files()
        } else {
            0
        };

        // The swap. Renames are per-directory atomic; the brief window where
        // the world sits under the trash name is recoverable (see
        // sweep_stale_restore_dirs), never corrupt.
        if world_existed {
            if let Err(e) = std::fs::rename(&self.world_root, &trash) {
                let _ = std::fs::remove_dir_all(&staging);
                return Err(ioe("set aside live world", &self.world_root)(e));
            }
        }
        if let Err(e) = std::fs::rename(&staging, &self.world_root) {
            // Staging is intact under its temp name; roll the old world back.
            if world_existed {
                let _ = std::fs::rename(&trash, &self.world_root);
            }
            let _ = std::fs::remove_dir_all(&staging);
            return Err(ioe("swap restored world", &self.world_root)(e));
        }
        if let Ok(dir) = std::fs::File::open(&parent) {
            let _ = dir.sync_all();
        }
        if world_existed {
            let _ = std::fs::remove_dir_all(&trash);
        }

        Ok(RestoreReport {
            snapshot_id: manifest.snapshot_id,
            restored_files,
            restored_bytes,
            swept_files,
            safety_snapshot_id,
        })
    }
}

/// Join a validated `/`-separated relative path under `base`. Called only on
/// paths that already passed [`validate_rel_path`], so the result cannot
/// escape `base`.
fn rel_to_path(base: &Path, rel: &str) -> PathBuf {
    let mut out = base.to_path_buf();
    for component in rel.split('/') {
        out.push(component);
    }
    out
}

// ------------------------------------------------------------ manifest JSON

fn manifest_to_json(m: &SnapshotManifest) -> String {
    let mut reason = BTreeMap::new();
    reason.insert(
        "kind".to_string(),
        DataValue::Str(m.reason.kind().to_string()),
    );
    if let SnapshotReason::PreModChange { summary } = &m.reason {
        reason.insert("summary".to_string(), DataValue::Str(summary.clone()));
    }
    let files = DataValue::List(
        m.files
            .iter()
            .map(|f| {
                DataValue::Map(BTreeMap::from([
                    ("path".to_string(), DataValue::Str(f.rel_path.clone())),
                    ("sha256".to_string(), DataValue::Str(f.sha256.clone())),
                    ("size".to_string(), DataValue::Int(f.size as i64)),
                ]))
            })
            .collect(),
    );
    to_data_json(&DataValue::Map(BTreeMap::from([
        (
            "file_count".to_string(),
            DataValue::Int(m.file_count as i64),
        ),
        ("files".to_string(), files),
        (
            "format".to_string(),
            DataValue::Str(MANIFEST_FORMAT.to_string()),
        ),
        (
            "mc_version".to_string(),
            DataValue::Str(m.mc_version.clone()),
        ),
        ("reason".to_string(), DataValue::Map(reason)),
        (
            "snapshot_id".to_string(),
            DataValue::Str(m.snapshot_id.clone()),
        ),
        (
            "timestamp_secs".to_string(),
            DataValue::Int(m.timestamp_secs as i64),
        ),
        (
            "total_bytes".to_string(),
            DataValue::Int(m.total_bytes as i64),
        ),
    ])))
}

fn manifest_err(snapshot_id: &str, reason: impl Into<String>) -> WorldSnapshotError {
    WorldSnapshotError::Manifest {
        snapshot_id: snapshot_id.to_string(),
        reason: reason.into(),
    }
}

fn manifest_from_json(
    snapshot_id: &str,
    text: &str,
) -> Result<SnapshotManifest, WorldSnapshotError> {
    let bad = |reason: &str| manifest_err(snapshot_id, reason);
    let root = parse_data_json(text).map_err(|e| bad(&format!("JSON parse: {e}")))?;
    let map = root.as_map().ok_or_else(|| bad("root is not an object"))?;
    let get = |key: &str| {
        map.get(key)
            .ok_or_else(|| bad(&format!("missing field {key:?}")))
    };
    let get_str = |key: &str| {
        get(key)?
            .as_str()
            .map(str::to_string)
            .ok_or_else(|| bad(&format!("field {key:?} is not a string")))
    };
    let get_u64 = |key: &str| {
        get(key)?
            .as_int()
            .and_then(|v| u64::try_from(v).ok())
            .ok_or_else(|| bad(&format!("field {key:?} is not a non-negative integer")))
    };

    if get_str("format")? != MANIFEST_FORMAT {
        return Err(bad("unrecognized format tag"));
    }
    let reason_map = get("reason")?
        .as_map()
        .ok_or_else(|| bad("field \"reason\" is not an object"))?;
    let reason_kind = reason_map
        .get("kind")
        .and_then(DataValue::as_str)
        .ok_or_else(|| bad("field \"reason.kind\" is missing or not a string"))?;
    let reason_summary = reason_map
        .get("summary")
        .and_then(|v| v.as_str().map(str::to_string));
    let reason = SnapshotReason::from_kind(reason_kind, reason_summary).map_err(|e| bad(&e))?;

    let files_value = get("files")?;
    let DataValue::List(items) = files_value else {
        return Err(bad("field \"files\" is not an array"));
    };
    if items.len() > MAX_MANIFEST_FILES {
        return Err(bad("file list exceeds cap"));
    }
    let mut files = Vec::with_capacity(items.len());
    let mut total_bytes = 0u64;
    for item in items {
        let entry = item
            .as_map()
            .ok_or_else(|| bad("file entry is not an object"))?;
        let rel_path = entry
            .get("path")
            .and_then(|v| v.as_str().map(str::to_string))
            .ok_or_else(|| bad("file entry \"path\" is missing or not a string"))?;
        // The jail applies to manifest content too — a hand-written
        // "../escape" entry is refused here, before any restore planning.
        validate_rel_path(&rel_path)?;
        let sha256 = entry
            .get("sha256")
            .and_then(|v| v.as_str().map(str::to_string))
            .ok_or_else(|| bad("file entry \"sha256\" is missing or not a string"))?;
        if !valid_sha256_hex(&sha256) {
            return Err(bad(&format!(
                "file entry {rel_path:?} has malformed sha256"
            )));
        }
        let size = entry
            .get("size")
            .and_then(DataValue::as_int)
            .and_then(|v| u64::try_from(v).ok())
            .ok_or_else(|| bad(&format!("file entry {rel_path:?} has bad \"size\"")))?;
        total_bytes = total_bytes
            .checked_add(size)
            .ok_or_else(|| bad("total_bytes overflow"))?;
        files.push(SnapshotFileEntry {
            rel_path,
            size,
            sha256,
        });
    }
    let file_count = get_u64("file_count")?;
    if file_count != files.len() as u64 {
        return Err(bad("file_count disagrees with files list"));
    }
    if get_u64("total_bytes")? != total_bytes {
        return Err(bad("total_bytes disagrees with files list"));
    }
    validate_snapshot_id(&get_str("snapshot_id")?)?;
    Ok(SnapshotManifest {
        snapshot_id: get_str("snapshot_id")?,
        timestamp_secs: get_u64("timestamp_secs")?,
        reason,
        mc_version: get_str("mc_version")?,
        file_count,
        total_bytes,
        files,
    })
}

// ------------------------------------------------------------------ tests

#[cfg(test)]
mod tests {
    use super::*;

    /// FIPS 180-4 / RFC 6234 known-answer vectors every SHA-256 must match.
    #[test]
    fn sha256_known_answer_vectors() {
        assert_eq!(
            hex_lower(&Sha256::digest(b"")),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            hex_lower(&Sha256::digest(b"abc")),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            hex_lower(&Sha256::digest(
                b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"
            )),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
        // Streaming in odd-sized chunks must equal the one-shot digest, and
        // the million-'a' vector covers the multi-block path.
        let million_a = vec![b'a'; 1_000_000];
        assert_eq!(
            hex_lower(&Sha256::digest(&million_a)),
            "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0"
        );
        let mut streamed = Sha256::new();
        for chunk in million_a.chunks(7) {
            streamed.update(chunk);
        }
        assert_eq!(
            hex_lower(&streamed.finish()),
            "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0"
        );
    }

    #[test]
    fn rel_path_jail() {
        assert!(validate_rel_path("level.dat").is_ok());
        assert!(validate_rel_path("region/r.0.0.mca").is_ok());
        for bad in [
            "",
            "/abs",
            "../escape",
            "a/../b",
            "a//b",
            "a/./b",
            "a\\b",
            "back\\..\\slash",
        ] {
            assert!(validate_rel_path(bad).is_err(), "{bad:?} must be refused");
        }
        let mut nul = "nul".to_string();
        nul.push('\0');
        assert!(validate_rel_path(&nul).is_err());
    }

    #[test]
    fn snapshot_id_jail() {
        assert!(validate_snapshot_id("snap-1234-abcd").is_ok());
        for bad in ["", "..", "a/b", "a\\b", "a b", ".", "../x", "☃"] {
            assert!(
                validate_snapshot_id(bad).is_err(),
                "{bad:?} must be refused"
            );
        }
    }
}
