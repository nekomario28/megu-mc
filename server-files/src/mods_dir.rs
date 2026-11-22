// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
#![deny(unsafe_code)]
//! `mods/` discovery, the way Fabric Loader, NeoForge and the launchers
//! (Prism, Modrinth App, MultiMC) treat the folder: only regular `.jar`
//! files are active, everything else is set aside with an explicit reason —
//! never silently dropped, never a hard failure.
//!
//! The launchers' shared conventions are honored: `*.disabled` is a mod
//! toggled off without deleting the jar, entries starting with `.` are
//! metadata (`.DS_Store`, `.gitkeep`) or macOS AppleDouble sidecars
//! (`._create.jar`) that are not valid ZIP data, symlinks are refused
//! outright, and `<mods>/<mc_version>/` is an optional per-version folder
//! whose jars are active too (Fabric's `mods/1.21.1/` convention). Any
//! other subdirectory — a different version folder or a junk folder like
//! `memory_repo/` — is reported as [`ModsDirSkipReason::OtherSubdirectory`]
//! and not descended into.

use std::ffi::OsStr;
use std::io;
use std::path::{Path, PathBuf};

/// Why [`scan_mods_folder`] set an entry aside instead of activating it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModsDirSkipReason {
    /// `*.jar.disabled` / `*.disabled` — the Prism Launcher / Modrinth App /
    /// MultiMC convention for a mod toggled off.
    DisabledMod,
    /// A name starting with `.`: dotfiles (`.DS_Store`, `.gitkeep`) and
    /// macOS AppleDouble `._*` resource-fork sidecars, whose bytes are not
    /// valid ZIP/jar data and break naive archive readers.
    DotfileOrAppleDouble,
    /// A symlink: always refused, so a `mods/` entry can never redirect the
    /// loader outside the folder.
    SymlinkRefused,
    /// A regular entry that is not a `.jar`: `*.txt`, `*.zip`, `*.toml`,
    /// `*.log` and anything else, including `.jar`-named non-regular files
    /// (FIFOs, sockets, devices).
    NonJarFile,
    /// A subdirectory that is not the matching `<mc_version>` folder —
    /// `mods/1.20.1/` on a 1.21.1 instance, `mods/memory_repo/` and so on.
    OtherSubdirectory,
}

/// One entry under `mods/` that was seen but not activated.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SkippedModEntry {
    /// The entry's path (under the scanned `mods/` root).
    pub path: PathBuf,
    /// Why it was skipped.
    pub reason: ModsDirSkipReason,
}

/// The result of [`scan_mods_folder`]: every directory entry accounted for
/// as either an active jar or a [`SkippedModEntry`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ModsFolderScan {
    /// The `mods/` directory that was scanned.
    pub root: PathBuf,
    /// Loadable jars — regular files ending in `.jar` (case-insensitive),
    /// paths under [`Self::root`], sorted by normalized relative path.
    pub active_mods: Vec<PathBuf>,
    /// Everything else that was seen, sorted the same way.
    pub skipped: Vec<SkippedModEntry>,
}

impl ModsFolderScan {
    /// [`Self::active_mods`] relative to [`Self::root`] with `/`
    /// separators — the deterministic order the scan reports.
    pub fn relative_active_mods(&self) -> Vec<String> {
        self.active_mods
            .iter()
            .map(|p| normalized_rel(p.strip_prefix(&self.root).unwrap_or(p)))
            .collect()
    }

    /// [`Self::skipped`] as `(relative path, reason)` pairs, the same
    /// `/`-separated form as [`Self::relative_active_mods`].
    pub fn relative_skipped(&self) -> Vec<(String, ModsDirSkipReason)> {
        self.skipped
            .iter()
            .map(|s| {
                (
                    normalized_rel(s.path.strip_prefix(&self.root).unwrap_or(&s.path)),
                    s.reason,
                )
            })
            .collect()
    }
}

/// The `mc_version` argument must be a single plain folder name: it is
/// joined verbatim under `mods/`, so separators, `.`/`..` and NUL would
/// otherwise escape the directory or scan somewhere unintended.
fn validate_mc_version(version: &str) -> io::Result<()> {
    let ok = !version.is_empty()
        && !version.starts_with('.')
        && !version.contains('/')
        && !version.contains('\\')
        && !version.contains('\0');
    if ok {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("mc_version {version:?} is not a single safe folder name"),
        ))
    }
}

/// What one directory entry means for the scan.
enum Classification {
    /// A regular `.jar` file.
    Active,
    /// Set aside with this reason.
    Skip(ModsDirSkipReason),
    /// The `<mc_version>` subdirectory: its direct entries are classified
    /// with the same rules.
    VersionSubdir,
}

/// Classify one `read_dir` entry. `version_dir` is `Some(name)` only at the
/// `mods/` top level; inside the version folder it is `None`, so nested
/// directories there classify as [`ModsDirSkipReason::OtherSubdirectory`]
/// like everywhere else.
fn classify(
    name: &OsStr,
    file_type: std::fs::FileType,
    version_dir: Option<&OsStr>,
) -> Classification {
    // Checked before the name rules: `DirEntry::file_type` does not follow
    // links, so a symlink — even one pointing at a perfectly good jar — is
    // always refused.
    if file_type.is_symlink() {
        return Classification::Skip(ModsDirSkipReason::SymlinkRefused);
    }
    let text = name.to_string_lossy();
    if text.starts_with('.') {
        return Classification::Skip(ModsDirSkipReason::DotfileOrAppleDouble);
    }
    if file_type.is_dir() {
        return if version_dir == Some(name) {
            Classification::VersionSubdir
        } else {
            Classification::Skip(ModsDirSkipReason::OtherSubdirectory)
        };
    }
    // `.disabled` wins over `.jar` (`mod.jar.disabled` is a disabled mod,
    // not a jar); both suffix checks are ASCII-case-insensitive for the
    // `.JAR` files Windows tooling produces.
    let lower = text.to_ascii_lowercase();
    if lower.ends_with(".disabled") {
        return Classification::Skip(ModsDirSkipReason::DisabledMod);
    }
    if lower.ends_with(".jar") && file_type.is_file() {
        return Classification::Active;
    }
    Classification::Skip(ModsDirSkipReason::NonJarFile)
}

/// Classify every direct entry of `dir` into `scan`. `version_dir` names
/// the `<mc_version>` folder to descend into; `None` inside it.
fn scan_entries(
    dir: &Path,
    version_dir: Option<&OsStr>,
    scan: &mut ModsFolderScan,
) -> io::Result<()> {
    let read = match std::fs::read_dir(dir) {
        Ok(read) => read,
        // A missing folder is an empty scan, not an error — the launcher
        // creates `mods/` lazily too.
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    for entry in read {
        let entry = entry?;
        match classify(&entry.file_name(), entry.file_type()?, version_dir) {
            Classification::Active => scan.active_mods.push(entry.path()),
            Classification::Skip(reason) => scan.skipped.push(SkippedModEntry {
                path: entry.path(),
                reason,
            }),
            Classification::VersionSubdir => scan_entries(&entry.path(), None, scan)?,
        }
    }
    Ok(())
}

/// A relative path as a forward-slash-separated string — the deterministic
/// sort key, identical regardless of the platform's separator.
fn normalized_rel(rel: &Path) -> String {
    rel.components()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

/// Scan `mods_dir` for active mod jars, launcher-style.
///
/// `mc_version`, when given, also activates the jars directly inside
/// `<mods_dir>/<mc_version>/` (Fabric's per-version folder); it must be a
/// single plain folder name — anything with a path separator, `.`/`..` or
/// NUL is rejected with `InvalidInput` before anything is read.
///
/// The result is deterministic: [`ModsFolderScan::active_mods`] and
/// [`ModsFolderScan::skipped`] are both sorted by their normalized
/// `/`-separated relative path. A missing `mods/` is an empty scan, not an
/// error.
pub fn scan_mods_folder(mods_dir: &Path, mc_version: Option<&str>) -> io::Result<ModsFolderScan> {
    if let Some(version) = mc_version {
        validate_mc_version(version)?;
    }
    let mut scan = ModsFolderScan {
        root: mods_dir.to_path_buf(),
        ..ModsFolderScan::default()
    };
    scan_entries(mods_dir, mc_version.map(OsStr::new), &mut scan)?;
    scan.active_mods
        .sort_by_cached_key(|p| normalized_rel(p.strip_prefix(mods_dir).unwrap_or(p)));
    scan.skipped
        .sort_by_cached_key(|s| normalized_rel(s.path.strip_prefix(mods_dir).unwrap_or(&s.path)));
    Ok(scan)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_dir(tag: &str) -> PathBuf {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join(format!("server-files-mods-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn missing_folder_scans_empty_and_bad_versions_are_refused() {
        let dir = tmp_dir("empty").join("no-mods");
        let scan = scan_mods_folder(&dir, Some("1.21.1")).unwrap();
        assert!(scan.active_mods.is_empty());
        assert!(scan.skipped.is_empty());
        for bad in ["..", ".", "a/b", "a\\b", "", "1.2\0"] {
            assert!(scan_mods_folder(&dir, Some(bad)).is_err(), "{bad:?}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
