// SPDX-License-Identifier: LGPL-3.0-or-later
//! `megu-installer`: a pure-Rust, zero-Java, single-executable installer that
//! gives players the Fabric `.exe`-installer / NeoForge-installer experience
//! even on Windows boxes with broken `.jar` associations or no Java at all.
//!
//! Three install lanes share the crate's fail-closed discipline:
//!
//! * **Official Launcher** — [`install_client`] writes
//!   `versions/megu-<mc>/megu-<mc>.json` (a `type: "release"` profile that
//!   `inheritsFrom` the vanilla release and pins
//!   `net.megu:bootstrapper:1.0.0`), stages an optional
//!   `--bootstrap-jar` under `libraries/`, and surgically registers a
//!   `"megu-<mc>"` entry in `launcher_profiles.json` plus the Windows Store
//!   `launcher_profiles_microsoft_store.json` when present, leaving an
//!   atomic `.bak` of every file it rewrites and preserving every other
//!   profile. [`uninstall_client`] removes the version directory and the
//!   profile entries only.
//! * **Prism Launcher** — [`install_prism`] creates a `Megu-<mc>` instance
//!   (`instance.cfg`, `mmc-pack.json`, the standard `.minecraft/` client
//!   folder set, optional `patches/` + `libraries/` staging for the
//!   bootstrapper JAR, optional `WrapperCommand` pin for a native client).
//! * **Dedicated server** — [`install_server`] initializes the standard
//!   server layout through [`ServerDirectory`] (which writes `eula.txt`
//!   with `eula=false` — the EULA is never auto-accepted) and drops the
//!   same `run.sh`/`run.bat`/`README-SERVER.txt` launcher scripts the
//!   Java installer emits.
//!
//! Detection of the OS-standard directories is split into pure functions —
//! [`detect_minecraft_dir_for_os`] / [`detect_prism_instances_dir_for_os`]
//! — so the Windows/macOS/Linux matrix is testable on any host, plus
//! [`resolve_minecraft_dir`] / [`resolve_prism_instances_dir`], which add
//! the Linux Flatpak fallbacks behind an injected `dir_exists` probe.
//!
//! The crate keeps no clock: ISO-8601 stamps come from [`iso_utc_now`],
//! which honours `SOURCE_DATE_EPOCH` and otherwise writes the epoch,
//! exactly like `megu-init`'s `created` default. SHA-256 for
//! `--client-bin` pinning is computed by the shared `megu-core`
//! [`Sha256`] — no external crates, no `unsafe`.
//!
//! Fail-closed invariants every lane enforces: `..` components in
//! user-supplied paths are rejected, every existing directory component on
//! the way to a target must be a real directory (a symlink or regular file
//! errors out), a symlink sitting at a write target is never followed, and
//! staged inputs (`--bootstrap-jar`, `--client-bin`, `--server-bin`) must
//! be existing regular files — never symlinks.

#![deny(unsafe_code)]

use crate::atomic::write_atomic;
use crate::directory::{ensure_client_game_dir_layout, ServerDirectory};
use crate::json::{self, Json};
use megu_core::version::TargetVersion;
use std::io;
use std::path::{Component, Path, PathBuf};

pub use megu_core::crypto::Sha256;

/// Maven coordinate the Official Launcher profile and Prism patch share.
pub const BOOTSTRAP_LIBRARY_COORD: &str = "net.megu:bootstrapper:1.0.0";
/// Path of the staged bootstrapper JAR relative to a `libraries/` root.
pub const BOOTSTRAP_LIBRARY_REL: &str = "net/megu/bootstrapper/1.0.0/bootstrapper-1.0.0.jar";
/// Directory holding the staged bootstrapper JAR relative to a `libraries/` root.
pub const BOOTSTRAP_LIBRARY_DIR: &str = "net/megu/bootstrapper/1.0.0";
/// The bootstrapper's entry point the launchers run.
pub const BOOTSTRAP_MAIN_CLASS: &str = "net.megu.launcher.Main";
/// Prism component `uid` for the local bootstrapper patch.
pub const BOOTSTRAP_PATCH_UID: &str = "net.megu.bootstrapper";
/// `patches/` file name carrying the Prism bootstrapper patch.
pub const BOOTSTRAP_PATCH_FILE: &str = "net.megu.bootstrapper.json";
/// The bootstrapper JAR stays a tiny pure-Java bridge; larger inputs are
/// not the artifact this installer signs up to stage.
pub const MAX_BOOTSTRAP_JAR_BYTES: u64 = 32 * 1024;
/// Official Launcher profile document every OS layout knows.
pub const LAUNCHER_PROFILES_FILE: &str = "launcher_profiles.json";
/// Windows Store launcher variant, updated only when it already exists.
pub const LAUNCHER_PROFILES_STORE_FILE: &str = "launcher_profiles_microsoft_store.json";
/// Flatpak Minecraft directory relative to the user's home.
pub const FLATPAK_MINECRAFT_DIR: &str = ".var/app/com.mojang.Minecraft/.minecraft";
/// Flatpak Prism instances directory relative to the user's home.
pub const FLATPAK_PRISM_INSTANCES_DIR: &str =
    ".var/app/org.prismlauncher.PrismLauncher/data/PrismLauncher/instances";

/// `run.sh` staged next to a dedicated server: LF endings, refuses to boot
/// until `eula.txt` says `eula=true` or the operator exports `EULA=TRUE`.
/// Byte-identical to the script the Java installer writes.
pub const RUN_SH: &str = "#!/bin/sh\n\
# megu dedicated server launcher (Linux/macOS).\n\
cd \"$(dirname \"$0\")\" || exit 1\n\
if [ \"$EULA\" != \"TRUE\" ] && ! grep -q '^eula=true' eula.txt 2>/dev/null; then\n\
    echo \"megu: agree to the Minecraft EULA first:\" >&2\n\
    echo \"  put 'eula=true' into eula.txt, or re-run with EULA=TRUE.\" >&2\n\
    exit 1\n\
fi\n\
if [ -x ./megu ]; then\n\
    exec ./megu \"$@\"\n\
fi\n\
exec megu \"$@\"\n";

/// `run.bat` staged next to a dedicated server: CRLF endings, same
/// EULA-before-launch gate, forwards `%*` to `megu.exe`.
pub const RUN_BAT: &str = "@echo off\r\n\
rem megu dedicated server launcher (Windows).\r\n\
cd /d \"%~dp0\"\r\n\
if /i \"%EULA%\"==\"TRUE\" goto :run\r\n\
findstr /b /l /c:\"eula=true\" eula.txt >nul 2>&1\r\n\
if errorlevel 1 (\r\n\
    echo megu: agree to the Minecraft EULA first ^(eula=true in eula.txt, or EULA=TRUE^) 1>&2\r\n\
    exit /b 1\r\n\
)\r\n\
:run\r\n\
megu.exe %*\r\n";

/// Operator-facing note staged beside the launch scripts.
pub const SERVER_README: &str = "megu dedicated server\n\
=========================\n\
\n\
1. Accept the Minecraft EULA before the first launch. This installer\n\
   NEVER sets eula=true for you. Edit eula.txt so it reads:\n\
       eula=true\n\
   or pass the environment variable EULA=TRUE.\n\
\n\
2. Start the server:\n\
       ./run.sh      (Linux / macOS)\n\
       run.bat       (Windows)\n\
   Extra arguments are forwarded to the server binary via \"$@\" / %*.\n\
\n\
3. --server-bin stages the native binary next to these scripts as\n\
   megu / megu.exe. Without it the scripts use megu(.exe)\n\
   from this directory or PATH.\n";

// ------------------------------------------------------------- OS detection

/// The three OS families the installer ships on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OsKind {
    /// Windows (`%APPDATA%` layouts).
    Windows,
    /// macOS (`~/Library/Application Support` layouts).
    MacOs,
    /// Linux and the other Unix-likes (`~/.minecraft`, XDG, Flatpak).
    Linux,
}

/// The OS this binary was built for.
pub fn host_os_kind() -> OsKind {
    match std::env::consts::OS {
        "windows" => OsKind::Windows,
        "macos" => OsKind::MacOs,
        _ => OsKind::Linux,
    }
}

/// The user's home directory without a `dirs`-style dependency: `HOME` on
/// Unix, `USERPROFILE` (then `HOMEDRIVE`+`HOMEPATH`) on Windows.
pub fn home_dir() -> Option<PathBuf> {
    let (primary, fallback) = if host_os_kind() == OsKind::Windows {
        ("USERPROFILE", "HOME")
    } else {
        ("HOME", "USERPROFILE")
    };
    for name in [primary, fallback] {
        if let Some(value) = std::env::var_os(name).filter(|v| !v.is_empty()) {
            return Some(PathBuf::from(value));
        }
    }
    if host_os_kind() == OsKind::Windows {
        if let (Some(drive), Some(path)) =
            (std::env::var_os("HOMEDRIVE"), std::env::var_os("HOMEPATH"))
        {
            let mut home = PathBuf::from(drive);
            home.push(path);
            return Some(home);
        }
    }
    None
}

fn env_dir(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

/// The OS-standard Minecraft directory, pure and testable: `os` chooses
/// the layout, `home` and `appdata` inject what `%USERPROFILE%`/`$HOME`
/// and `%APPDATA%` would resolve to on the target machine.
///
/// * Windows: `appdata/.minecraft`, falling back to
///   `home/AppData/Roaming/.minecraft` when `%APPDATA%` is unset.
/// * macOS: `home/Library/Application Support/minecraft`.
/// * Linux: `home/.minecraft` (the Flatpak fallback lives in
///   [`resolve_minecraft_dir`], which needs a filesystem probe).
pub fn detect_minecraft_dir_for_os(os: OsKind, home: &Path, appdata: Option<&Path>) -> PathBuf {
    match os {
        OsKind::Windows => appdata.map_or_else(
            || home.join("AppData").join("Roaming").join(".minecraft"),
            |dir| dir.join(".minecraft"),
        ),
        OsKind::MacOs => home
            .join("Library")
            .join("Application Support")
            .join("minecraft"),
        OsKind::Linux => home.join(".minecraft"),
    }
}

/// The OS-standard Prism Launcher `instances/` directory, pure and
/// testable like [`detect_minecraft_dir_for_os`].
///
/// * Windows: `appdata/PrismLauncher/instances` (Roaming fallback).
/// * macOS: `home/Library/Application Support/PrismLauncher/instances`.
/// * Linux: `xdg_data_home/PrismLauncher/instances`, or
///   `home/.local/share/PrismLauncher/instances` when XDG is unset.
pub fn detect_prism_instances_dir_for_os(
    os: OsKind,
    home: &Path,
    appdata: Option<&Path>,
    xdg_data_home: Option<&Path>,
) -> PathBuf {
    match os {
        OsKind::Windows => appdata.map_or_else(
            || {
                home.join("AppData")
                    .join("Roaming")
                    .join("PrismLauncher")
                    .join("instances")
            },
            |dir| dir.join("PrismLauncher").join("instances"),
        ),
        OsKind::MacOs => home
            .join("Library")
            .join("Application Support")
            .join("PrismLauncher")
            .join("instances"),
        OsKind::Linux => xdg_data_home
            .map_or_else(|| home.join(".local").join("share"), Path::to_path_buf)
            .join("PrismLauncher")
            .join("instances"),
    }
}

/// [`detect_minecraft_dir_for_os`] plus the Linux Flatpak fallback: a
/// `~/.var/app/com.mojang.Minecraft/.minecraft` install wins only when the
/// standard `~/.minecraft` does not exist. `dir_exists` is injected so
/// tests decide the filesystem state.
pub fn resolve_minecraft_dir(
    os: OsKind,
    home: &Path,
    appdata: Option<&Path>,
    dir_exists: impl Fn(&Path) -> bool,
) -> PathBuf {
    let standard = detect_minecraft_dir_for_os(os, home, appdata);
    if os == OsKind::Linux && !dir_exists(&standard) {
        let flatpak = home.join(FLATPAK_MINECRAFT_DIR);
        if dir_exists(&flatpak) {
            return flatpak;
        }
    }
    standard
}

/// [`detect_prism_instances_dir_for_os`] plus the Linux Flatpak fallback
/// for `org.prismlauncher.PrismLauncher`.
pub fn resolve_prism_instances_dir(
    os: OsKind,
    home: &Path,
    appdata: Option<&Path>,
    xdg_data_home: Option<&Path>,
    dir_exists: impl Fn(&Path) -> bool,
) -> PathBuf {
    let standard = detect_prism_instances_dir_for_os(os, home, appdata, xdg_data_home);
    if os == OsKind::Linux && !dir_exists(&standard) {
        let flatpak = home.join(FLATPAK_PRISM_INSTANCES_DIR);
        if dir_exists(&flatpak) {
            return flatpak;
        }
    }
    standard
}

/// [`resolve_minecraft_dir`] over the live process environment.
pub fn detect_minecraft_dir() -> io::Result<PathBuf> {
    let home = home_dir().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "cannot locate the user home directory (set HOME/USERPROFILE)",
        )
    })?;
    Ok(resolve_minecraft_dir(
        host_os_kind(),
        &home,
        env_dir("APPDATA").as_deref(),
        |dir| dir.is_dir(),
    ))
}

/// [`resolve_prism_instances_dir`] over the live process environment.
pub fn detect_prism_instances_dir() -> io::Result<PathBuf> {
    let home = home_dir().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "cannot locate the user home directory (set HOME/USERPROFILE)",
        )
    })?;
    Ok(resolve_prism_instances_dir(
        host_os_kind(),
        &home,
        env_dir("APPDATA").as_deref(),
        env_dir("XDG_DATA_HOME").as_deref(),
        |dir| dir.is_dir(),
    ))
}

// ------------------------------------------------------------- timestamps

/// Civil-from-days epoch conversion (Howard Hinnant's algorithm): seconds
/// since 1970-01-01T00:00:00Z -> `YYYY-MM-DDTHH:MM:SS+00:00`, the same ISO
/// spelling the Java installer's `yyyy-MM-dd'T'HH:mm:ssxxx` emits. Pure
/// arithmetic — no clock, no libc.
pub fn unix_seconds_to_iso8601(seconds: u64) -> String {
    let days = (seconds / 86_400) as i64;
    let secs = seconds % 86_400;
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { y + 1 } else { y };
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}+00:00",
        secs / 3_600,
        (secs % 3_600) / 60,
        secs % 60
    )
}

/// The stamp written into `releaseTime`/`time`/`created`/`lastUsed`: the
/// `SOURCE_DATE_EPOCH` environment variable when it parses as seconds, the
/// epoch otherwise — the crate keeps no clock, matching `megu-init`'s
/// `created` default.
pub fn iso_utc_now() -> String {
    let seconds = std::env::var("SOURCE_DATE_EPOCH")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(0);
    unix_seconds_to_iso8601(seconds)
}

// ------------------------------------------------------------------ SHA-256

/// SHA-256 of a byte slice as lowercase hex.
pub fn sha256_hex(data: &[u8]) -> String {
    megu_core::crypto::sha256_hex(data)
}

/// SHA-256 of a file's contents, streamed in bounded chunks.
pub fn sha256_file(path: &Path) -> io::Result<String> {
    megu_core::crypto::sha256_file(path)
}

// --------------------------------------------------------- fail-closed I/O

fn invalid_input(message: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

fn unsafe_target_err(path: &Path) -> io::Error {
    invalid_input(format!(
        "{} has an unsafe or incompatible file type",
        path.display()
    ))
}

/// A user-supplied path may not climb out of its own root: a `..`
/// component fails closed (Windows drive prefixes, plain components, `.`
/// and a leading root are fine).
pub fn reject_traversal(path: &Path) -> io::Result<()> {
    for component in path.components() {
        match component {
            Component::Normal(_) | Component::CurDir | Component::RootDir => {}
            Component::Prefix(_) => {}
            Component::ParentDir => {
                return Err(invalid_input(format!(
                    "refusing path traversal in {}",
                    path.display()
                )))
            }
        }
    }
    Ok(())
}

/// Every component of `dir` that already exists must be a real directory —
/// a symlink or regular file anywhere on the chain (including `dir`
/// itself) is an error, so a planted link cannot redirect the install
/// outside the requested location. Missing components are skipped; the
/// caller creates them.
fn ensure_real_ancestors(dir: &Path) -> io::Result<()> {
    for ancestor in dir.ancestors() {
        match std::fs::symlink_metadata(ancestor) {
            Ok(meta) if meta.file_type().is_symlink() || !meta.is_dir() => {
                return Err(unsafe_target_err(ancestor))
            }
            Ok(_) => {}
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => return Err(err),
        }
    }
    Ok(())
}

/// `mkdir -p` for `root/rel` where `rel` is a fixed safe relative path and
/// every existing component under it must be a real directory — same
/// contract as `directory::ensure_folder`. Returns whether anything was
/// created.
fn ensure_subdir(root: &Path, rel: &Path) -> io::Result<bool> {
    let mut cursor = root.to_path_buf();
    for component in rel.components() {
        match component {
            Component::Normal(name) => cursor.push(name),
            _ => {
                return Err(invalid_input(format!(
                    "{} is not a safe relative path",
                    rel.display()
                )))
            }
        }
        match std::fs::symlink_metadata(&cursor) {
            Ok(meta) if meta.file_type().is_symlink() || !meta.is_dir() => {
                return Err(unsafe_target_err(&cursor))
            }
            Ok(_) => {}
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                std::fs::create_dir_all(root.join(rel))?;
                return Ok(true);
            }
            Err(err) => return Err(err),
        }
    }
    Ok(false)
}

/// A symlink at `path` is never overwritten or followed.
fn refuse_symlink(path: &Path) -> io::Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => Err(invalid_input(format!(
            "refusing to overwrite symlink: {}",
            path.display()
        ))),
        _ => Ok(()),
    }
}

/// `bytes` to `path` atomically after proving the target is no symlink.
fn atomic_write(path: &Path, bytes: &[u8]) -> io::Result<()> {
    refuse_symlink(path)?;
    write_atomic(path, bytes)
}

/// `source` must be an existing regular file that is not itself a symlink;
/// the staged copy lands at `dest` atomically.
fn stage_file(source: &Path, dest: &Path) -> io::Result<()> {
    match std::fs::symlink_metadata(source) {
        Ok(meta) if meta.file_type().is_symlink() || !meta.is_file() => {
            return Err(unsafe_target_err(source))
        }
        Ok(_) => {}
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            return Err(invalid_input(format!(
                "staged file does not exist: {}",
                source.display()
            )))
        }
        Err(err) => return Err(err),
    }
    atomic_write(dest, &std::fs::read(source)?)
}

/// Prepare a user-supplied install root: reject traversal, prove every
/// existing component is a real directory, then create what is missing.
fn prepare_install_root(dir: &Path) -> io::Result<()> {
    reject_traversal(dir)?;
    ensure_real_ancestors(dir)?;
    std::fs::create_dir_all(dir)?;
    ensure_real_ancestors(dir)
}

#[cfg(unix)]
fn mark_executable(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut permissions = std::fs::metadata(path)?.permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(path, permissions)
}

#[cfg(not(unix))]
fn mark_executable(_path: &Path) -> io::Result<()> {
    Ok(())
}

/// Recursive delete that never crosses a link: a symlinked entry is
/// unlinked, not descended.
fn remove_tree(path: &Path) -> io::Result<()> {
    let meta = match std::fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err),
    };
    if meta.file_type().is_symlink() || !meta.is_dir() {
        std::fs::remove_file(path)
    } else {
        for entry in std::fs::read_dir(path)? {
            remove_tree(&entry?.path())?;
        }
        std::fs::remove_dir(path)
    }
}

// ------------------------------------------------------------------ report

/// One filesystem action an install performed, for `--json` output.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReportEntry {
    /// The path that was touched.
    pub path: PathBuf,
    /// `created`, `updated`, `staged`, `backed-up` or `removed`.
    pub action: &'static str,
}

/// What one install/uninstall run did, in deterministic order.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct InstallReport {
    /// Human summary line, e.g. `Installed megu-1.21.1 into <dir>`.
    pub summary: String,
    /// Per-path actions in the order they were applied.
    pub actions: Vec<ReportEntry>,
}

// -------------------------------------------------------- Official Launcher

/// The version directory and profile key this installer owns.
pub fn version_id(mc_version: TargetVersion) -> String {
    format!("megu-{}", mc_version.id())
}

/// Options shared by the Official Launcher and Prism client installs.
#[derive(Clone, Debug)]
pub struct ClientInstallOptions {
    /// Vanilla release the profile inherits from.
    pub mc_version: TargetVersion,
    /// `--bootstrap-jar`: staged under `libraries/` when present.
    pub bootstrap_jar: Option<PathBuf>,
    /// `--client-bin`: native client pinned in the profile.
    pub client_bin: Option<PathBuf>,
    /// `--client-sha256`: verified against the pin when both are given.
    pub client_sha256: Option<String>,
}

impl Default for ClientInstallOptions {
    fn default() -> Self {
        Self {
            mc_version: TargetVersion::V1_21_1,
            bootstrap_jar: None,
            client_bin: None,
            client_sha256: None,
        }
    }
}

/// Options for the dedicated-server install.
#[derive(Clone, Debug)]
pub struct ServerInstallOptions {
    /// Vanilla release the server directory pins.
    pub mc_version: TargetVersion,
    /// `--server-bin`: staged as `megu`/`megu.exe` next to `run.sh`.
    pub server_bin: Option<PathBuf>,
}

impl Default for ServerInstallOptions {
    fn default() -> Self {
        Self {
            mc_version: TargetVersion::V1_21_1,
            server_bin: None,
        }
    }
}

/// `--client-sha256` is exactly 64 hexadecimal digits, lowercased.
pub fn normalize_sha256_hex(value: &str) -> io::Result<String> {
    if value.len() == 64 && value.bytes().all(|b| b.is_ascii_hexdigit()) {
        Ok(value.to_ascii_lowercase())
    } else {
        Err(invalid_input(
            "--client-sha256 must be 64 hexadecimal characters".to_owned(),
        ))
    }
}

/// A verified client pin: absolute binary path plus its SHA-256.
struct ClientPin {
    bin: Option<PathBuf>,
    sha256: Option<String>,
}

fn verify_client_pin(options: &ClientInstallOptions) -> io::Result<ClientPin> {
    let bin = match &options.client_bin {
        Some(path) => {
            reject_traversal(path)?;
            let meta = std::fs::symlink_metadata(path).map_err(|err| {
                if err.kind() == io::ErrorKind::NotFound {
                    invalid_input(format!("--client-bin does not exist: {}", path.display()))
                } else {
                    err
                }
            })?;
            if meta.file_type().is_symlink() || !meta.is_file() {
                return Err(unsafe_target_err(path));
            }
            Some(std::fs::canonicalize(path)?)
        }
        None => None,
    };
    // The pin always carries the real digest: `--client-sha256` is an
    // assertion the caller may add, never a substitute for computing it.
    let computed = bin.as_deref().map(sha256_file).transpose()?;
    let expected = options
        .client_sha256
        .as_deref()
        .map(normalize_sha256_hex)
        .transpose()?;
    if let (Some(want), Some(got)) = (&expected, &computed) {
        if want != got {
            return Err(invalid_input("native client SHA-256 mismatch".to_owned()));
        }
    }
    Ok(ClientPin {
        bin,
        sha256: computed.or(expected),
    })
}

/// Fail-closed `--bootstrap-jar` validation shared by the client and Prism
/// lanes: an existing, non-symlink regular file under the size budget.
fn validated_bootstrap_jar(jar: &Path) -> io::Result<()> {
    reject_traversal(jar)?;
    match std::fs::symlink_metadata(jar) {
        Ok(meta) if meta.file_type().is_symlink() || !meta.is_file() => {
            Err(unsafe_target_err(jar))
        }
        Ok(meta) if meta.len() >= MAX_BOOTSTRAP_JAR_BYTES => Err(invalid_input(format!(
            "--bootstrap-jar must stay under {MAX_BOOTSTRAP_JAR_BYTES} bytes: {}",
            jar.display()
        ))),
        Ok(_) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Err(invalid_input(format!(
            "--bootstrap-jar does not exist: {}",
            jar.display()
        ))),
        Err(err) => Err(err),
    }
}

/// The version JSON the Official Launcher drops into
/// `versions/<id>/<id>.json`.
fn client_version_json(version_id: &str, mc_version: TargetVersion, pin: &ClientPin, now: &str) -> Json {
    let mut members = vec![
        ("id".into(), Json::String(version_id.to_owned())),
        (
            "inheritsFrom".into(),
            Json::String(mc_version.id().to_owned()),
        ),
        ("type".into(), Json::String("release".into())),
        (
            "mainClass".into(),
            Json::String(BOOTSTRAP_MAIN_CLASS.to_owned()),
        ),
        ("releaseTime".into(), Json::String(now.to_owned())),
        ("time".into(), Json::String(now.to_owned())),
    ];
    let mut jvm = Vec::new();
    if let Some(bin) = &pin.bin {
        jvm.push(Json::String(format!("-Dmegu.clientBin={}", bin.display())));
    }
    if let Some(sha) = &pin.sha256 {
        jvm.push(Json::String(format!("-Dmegu.clientSha256={sha}")));
    }
    if !jvm.is_empty() {
        members.push((
            "arguments".into(),
            Json::Object(vec![("jvm".into(), Json::Array(jvm))]),
        ));
    }
    members.push((
        "libraries".into(),
        Json::Array(vec![Json::Object(vec![(
            "name".into(),
            Json::String(BOOTSTRAP_LIBRARY_COORD.to_owned()),
        )])]),
    ));
    Json::Object(members)
}

fn profile_entry(version_id: &str, display_name: &str, now: &str) -> Json {
    Json::Object(vec![
        ("name".into(), Json::String(display_name.to_owned())),
        ("type".into(), Json::String("custom".into())),
        ("created".into(), Json::String(now.to_owned())),
        ("lastUsed".into(), Json::String(now.to_owned())),
        ("lastVersionId".into(), Json::String(version_id.to_owned())),
        ("icon".into(), Json::String("Furnace".into())),
    ])
}

fn fresh_profiles_json(version_id: &str, display_name: &str, now: &str) -> Json {
    Json::Object(vec![
        (
            "profiles".into(),
            Json::Object(vec![(
                version_id.to_owned(),
                profile_entry(version_id, display_name, now),
            )]),
        ),
        ("version".into(), Json::Number(3.0)),
    ])
}

/// `<file>.bak` holds the pre-edit bytes; rewritten only when the backup is
/// missing or no longer matches what is about to be replaced.
fn backup_profiles(file: &Path, current: &str, actions: &mut Vec<ReportEntry>) -> io::Result<()> {
    let backup = file.with_file_name(format!(
        "{}.bak",
        file.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("launcher_profiles.json")
    ));
    let keep = std::fs::read_to_string(&backup)
        .map(|existing| existing == current)
        .unwrap_or(false);
    if keep {
        return Ok(());
    }
    atomic_write(&backup, current.as_bytes())?;
    actions.push(ReportEntry {
        path: backup,
        action: "backed-up",
    });
    Ok(())
}

/// The launcher-upsert semantics: an operator-tweaked `name`, `type`,
/// `created` or `icon` survives a re-install, while `lastUsed` and
/// `lastVersionId` always refresh — the same fields the Java installer
/// overwrites.
fn upsert_profile_fields(fields: &mut Vec<(String, Json)>, version_id: &str, name: &str, now: &str) {
    for (key, value, overwrite) in [
        ("name", name, false),
        ("type", "custom", false),
        ("created", now, false),
        ("lastUsed", now, true),
        ("lastVersionId", version_id, true),
        ("icon", "Furnace", false),
    ] {
        match fields.iter_mut().find(|(k, _)| k == key) {
            Some((_, slot)) if overwrite => *slot = Json::String(value.to_owned()),
            Some(_) => {}
            None => fields.push((key.to_owned(), Json::String(value.to_owned()))),
        }
    }
}

/// Add/update/remove our entry in one profiles document. Returns `true`
/// when the document changed.
fn edit_profiles_doc(
    members: &mut Vec<(String, Json)>,
    version_id: &str,
    display_name: &str,
    uninstall: bool,
    now: &str,
) -> bool {
    let position = members.iter().position(|(key, _)| key == "profiles");
    if uninstall {
        return match position {
            Some(index) => match &mut members[index].1 {
                Json::Object(profiles) => {
                    let before = profiles.len();
                    profiles.retain(|(key, _)| key != version_id);
                    profiles.len() != before
                }
                _ => false,
            },
            None => false,
        };
    }
    match position {
        None => {
            members.push((
                "profiles".into(),
                Json::Object(vec![(
                    version_id.to_owned(),
                    profile_entry(version_id, display_name, now),
                )]),
            ));
            true
        }
        Some(index) => match &mut members[index].1 {
            Json::Object(profiles) => {
                match profiles.iter_mut().find(|(key, _)| key == version_id) {
                    Some((_, Json::Object(fields))) => {
                        upsert_profile_fields(fields, version_id, display_name, now)
                    }
                    Some((_, slot)) => *slot = profile_entry(version_id, display_name, now),
                    None => profiles.push((
                        version_id.to_owned(),
                        profile_entry(version_id, display_name, now),
                    )),
                }
                true
            }
            slot => {
                *slot = Json::Object(vec![(
                    version_id.to_owned(),
                    profile_entry(version_id, display_name, now),
                )]);
                true
            }
        },
    }
}

fn edit_profiles_file(
    file: &Path,
    version_id: &str,
    display_name: &str,
    uninstall: bool,
    now: &str,
    actions: &mut Vec<ReportEntry>,
) -> io::Result<()> {
    refuse_symlink(file)?;
    let text = match std::fs::read_to_string(file) {
        Ok(text) => Some(text),
        Err(err) if err.kind() == io::ErrorKind::NotFound => None,
        Err(err) => return Err(err),
    };
    let mut members = match text.as_deref().map(json::parse) {
        Some(Ok(Json::Object(members))) => members,
        // Missing file, malformed JSON or a non-object root: on install the
        // pre-edit bytes are backed up and a fresh document takes over —
        // on uninstall there is nothing to remove, so leave it alone.
        _ if uninstall => return Ok(()),
        _ => match fresh_profiles_json(version_id, display_name, now) {
            Json::Object(members) => members,
            _ => unreachable!("fresh_profiles_json is always an object"),
        },
    };
    let existed = text.is_some();
    let mut changed = !existed;
    if existed {
        changed = edit_profiles_doc(&mut members, version_id, display_name, uninstall, now);
    }
    let updated = format!("{}\n", Json::Object(members).to_json_string());
    if !changed || text.as_deref() == Some(updated.as_str()) {
        return Ok(());
    }
    if let Some(current) = text.as_deref() {
        backup_profiles(file, current, actions)?;
    }
    atomic_write(file, updated.as_bytes())?;
    actions.push(ReportEntry {
        path: file.to_path_buf(),
        action: if existed { "updated" } else { "created" },
    });
    Ok(())
}

/// Surgical add/update/remove of the `megu-<mc>` entry across
/// `launcher_profiles.json` and — only when it already exists — the
/// Windows Store profile document.
fn update_profiles_files(
    mc_dir: &Path,
    version_id: &str,
    display_name: &str,
    uninstall: bool,
    now: &str,
    actions: &mut Vec<ReportEntry>,
) -> io::Result<()> {
    let standard = mc_dir.join(LAUNCHER_PROFILES_FILE);
    let store = mc_dir.join(LAUNCHER_PROFILES_STORE_FILE);
    if uninstall || standard.exists() || store.exists() {
        for file in [standard, store] {
            if file.exists() {
                edit_profiles_file(&file, version_id, display_name, uninstall, now, actions)?;
            }
        }
    } else {
        edit_profiles_file(&standard, version_id, display_name, false, now, actions)?;
    }
    Ok(())
}

/// Fabric/NeoForge-style Official Launcher install: the version JSON, the
/// staged bootstrapper library, and the launcher profile registration.
///
/// `mc_dir` is either detected ([`detect_minecraft_dir`]) or the
/// `--minecraft-dir` override; either way traversal components and
/// symlinked ancestors fail closed.
pub fn install_client(
    mc_dir: &Path,
    options: &ClientInstallOptions,
    write_profile: bool,
    now: &str,
) -> io::Result<InstallReport> {
    prepare_install_root(mc_dir)?;
    let version_id = version_id(options.mc_version);
    let pin = verify_client_pin(options)?;
    let mut actions = Vec::new();

    if let Some(jar) = &options.bootstrap_jar {
        validated_bootstrap_jar(jar)?;
        ensure_subdir(mc_dir, &Path::new("libraries").join(BOOTSTRAP_LIBRARY_DIR))?;
        let dest = mc_dir.join("libraries").join(BOOTSTRAP_LIBRARY_REL);
        stage_file(jar, &dest)?;
        actions.push(ReportEntry {
            path: dest,
            action: "staged",
        });
    }

    let version_rel = Path::new("versions").join(&version_id);
    ensure_subdir(mc_dir, &version_rel)?;
    let version_json = mc_dir
        .join(&version_rel)
        .join(format!("{version_id}.json"));
    atomic_write(
        &version_json,
        format!(
            "{}\n",
            client_version_json(&version_id, options.mc_version, &pin, now).to_json_string()
        )
        .as_bytes(),
    )?;
    actions.push(ReportEntry {
        path: version_json,
        action: "created",
    });

    if write_profile {
        update_profiles_files(
            mc_dir,
            &version_id,
            &format!("Megu ({})", options.mc_version.id()),
            false,
            now,
            &mut actions,
        )?;
    }
    Ok(InstallReport {
        summary: format!("Installed {version_id} into {}", mc_dir.display()),
        actions,
    })
}

/// Remove the version directory and the profile entries, preserving every
/// other profile and file.
pub fn uninstall_client(
    mc_dir: &Path,
    mc_version: TargetVersion,
    update_profile: bool,
    now: &str,
) -> io::Result<InstallReport> {
    reject_traversal(mc_dir)?;
    let version_id = version_id(mc_version);
    let version_dir = mc_dir.join("versions").join(&version_id);
    let mut actions = Vec::new();
    if version_dir.symlink_metadata().is_ok() {
        remove_tree(&version_dir)?;
        actions.push(ReportEntry {
            path: version_dir,
            action: "removed",
        });
    }
    if update_profile {
        update_profiles_files(mc_dir, &version_id, "", true, now, &mut actions)?;
    }
    Ok(InstallReport {
        summary: format!("Uninstalled {version_id} from {}", mc_dir.display()),
        actions,
    })
}

// ------------------------------------------------------------------- Prism

/// The Prism instance directory name: `Megu-<mc_version>`.
pub fn prism_instance_name(mc_version: TargetVersion) -> String {
    format!("Megu-{}", mc_version.id())
}

/// `mmc-pack.json`: the vanilla component plus the local bootstrapper
/// patch when a JAR is staged.
fn mmc_pack_json(mc_version: TargetVersion, with_bootstrapper: bool) -> Json {
    let mut components = vec![Json::Object(vec![
        ("uid".into(), Json::String("net.minecraft".into())),
        ("version".into(), Json::String(mc_version.id().into())),
        ("important".into(), Json::Bool(true)),
    ])];
    if with_bootstrapper {
        components.push(Json::Object(vec![
            ("uid".into(), Json::String(BOOTSTRAP_PATCH_UID.into())),
            ("version".into(), Json::String("1.0.0".into())),
        ]));
    }
    Json::Object(vec![
        ("formatVersion".into(), Json::Number(1.0)),
        ("components".into(), Json::Array(components)),
    ])
}

/// `patches/net.megu.bootstrapper.json`: the local patch that swaps the
/// game `mainClass` for the bootstrapper entry point.
fn bootstrap_patch_json(mc_version: TargetVersion, pin: &ClientPin) -> Json {
    let mut members = vec![
        ("formatVersion".into(), Json::Number(1.0)),
        ("uid".into(), Json::String(BOOTSTRAP_PATCH_UID.into())),
        ("version".into(), Json::String("1.0.0".into())),
        ("name".into(), Json::String("megu bootstrapper".into())),
        ("mainClass".into(), Json::String(BOOTSTRAP_MAIN_CLASS.into())),
        (
            "inheritsFrom".into(),
            Json::String(mc_version.id().into()),
        ),
        (
            "libraries".into(),
            Json::Array(vec![Json::Object(vec![
                ("name".into(), Json::String(BOOTSTRAP_LIBRARY_COORD.into())),
                ("MMC-hint".into(), Json::String("local".into())),
            ])]),
        ),
    ];
    let mut jvm = Vec::new();
    if let Some(bin) = &pin.bin {
        jvm.push(Json::String(format!("-Dmegu.clientBin={}", bin.display())));
    }
    if let Some(sha) = &pin.sha256 {
        jvm.push(Json::String(format!("-Dmegu.clientSha256={sha}")));
    }
    if !jvm.is_empty() {
        members.push(("+jvmArgs".into(), Json::Array(jvm)));
    }
    Json::Object(members)
}

/// One-click Prism instance: `Megu-<mc>` under the instances directory
/// with `instance.cfg`, `mmc-pack.json`, the standard `.minecraft` client
/// folders, and optional bootstrapper patch + client pin.
///
/// Two launch modes, mirroring the Java export lanes: with
/// `--bootstrap-jar` the patch makes the bootstrapper JAR the main class;
/// without it a pinned `--client-bin` becomes the instance
/// `WrapperCommand`.
pub fn install_prism(
    instances_dir: &Path,
    options: &ClientInstallOptions,
) -> io::Result<InstallReport> {
    prepare_install_root(instances_dir)?;
    let pin = verify_client_pin(options)?;
    let instance_name = prism_instance_name(options.mc_version);
    let mut actions = Vec::new();

    ensure_subdir(instances_dir, Path::new(&instance_name))?;
    let instance = instances_dir.join(&instance_name);

    // Staged payloads land first: an unsafe staged target must fail before
    // any manifest is written.
    let staged_bin = match &pin.bin {
        Some(bin) => {
            let bin_dir = if options.bootstrap_jar.is_some() {
                ".minecraft/bin"
            } else {
                "bin"
            };
            ensure_subdir(&instance, Path::new(bin_dir))?;
            let name = match bin.file_name().and_then(|n| n.to_str()) {
                Some(name) if name.to_ascii_lowercase().ends_with(".exe") => {
                    "megu-client.exe".to_owned()
                }
                _ => "megu-client".to_owned(),
            };
            let dest = instance.join(bin_dir).join(&name);
            stage_file(bin, &dest)?;
            mark_executable(&dest)?;
            actions.push(ReportEntry {
                path: dest.clone(),
                action: "staged",
            });
            Some(dest)
        }
        None => None,
    };

    if let Some(jar) = &options.bootstrap_jar {
        validated_bootstrap_jar(jar)?;
        ensure_subdir(&instance, &Path::new("libraries").join(BOOTSTRAP_LIBRARY_DIR))?;
        let dest = instance.join("libraries").join(BOOTSTRAP_LIBRARY_REL);
        stage_file(jar, &dest)?;
        actions.push(ReportEntry {
            path: dest,
            action: "staged",
        });
        ensure_subdir(&instance, Path::new("patches"))?;
        let patch = instance.join("patches").join(BOOTSTRAP_PATCH_FILE);
        atomic_write(
            &patch,
            format!(
                "{}\n",
                bootstrap_patch_json(options.mc_version, &pin).to_json_string()
            )
            .as_bytes(),
        )?;
        actions.push(ReportEntry {
            path: patch,
            action: "created",
        });
    }

    let mmc_pack = instance.join("mmc-pack.json");
    atomic_write(
        &mmc_pack,
        format!(
            "{}\n",
            mmc_pack_json(options.mc_version, options.bootstrap_jar.is_some()).to_json_string()
        )
        .as_bytes(),
    )?;
    actions.push(ReportEntry {
        path: mmc_pack,
        action: "created",
    });

    // The wrapper command only applies in the no-patch lane: when the
    // bootstrapper patch is installed the JVM launches it directly.
    let mut instance_cfg = format!(
        "[General]\nInstanceType=OneSix\nname=Megu {}\n",
        options.mc_version.id()
    );
    match (&staged_bin, options.bootstrap_jar.is_none()) {
        (Some(bin), true) => {
            instance_cfg.push_str(&format!(
                "OverrideCommands=true\nWrapperCommand=\"{}\"\n",
                bin.display()
            ));
        }
        _ => instance_cfg.push_str("OverrideCommands=false\n"),
    }
    let cfg_path = instance.join("instance.cfg");
    atomic_write(&cfg_path, instance_cfg.as_bytes())?;
    actions.push(ReportEntry {
        path: cfg_path,
        action: "created",
    });

    let game_dir = instance.join(".minecraft");
    ensure_client_game_dir_layout(&game_dir)
        .map_err(|err| io::Error::new(err.kind(), format!("{}: {err}", instance.display())))?;
    actions.push(ReportEntry {
        path: game_dir,
        action: "created",
    });

    Ok(InstallReport {
        summary: format!(
            "Installed Prism instance {} into {}",
            instance_name,
            instances_dir.display()
        ),
        actions,
    })
}

// ------------------------------------------------------------------ server

/// NeoForge-style dedicated server install: the standard layout (through
/// [`ServerDirectory::load_or_init`], which writes `eula.txt` with
/// `eula=false` — consent is the operator's edit, never the installer's)
/// plus the launch scripts and an optional staged native binary.
pub fn install_server(dir: &Path, options: &ServerInstallOptions) -> io::Result<InstallReport> {
    prepare_install_root(dir)?;
    let mut actions = Vec::new();

    if let Some(bin) = &options.server_bin {
        reject_traversal(bin)?;
        let exe = bin
            .file_name()
            .and_then(|n| n.to_str())
            .map(|n| n.to_ascii_lowercase().ends_with(".exe"))
            .unwrap_or(false);
        let dest = dir.join(if exe { "megu.exe" } else { "megu" });
        stage_file(bin, &dest)?;
        mark_executable(&dest)?;
        actions.push(ReportEntry {
            path: dest,
            action: "staged",
        });
    }

    // load_or_init creates eula.txt with eula=false when missing and never
    // rewrites an operator's acceptance.
    let eula_existed = dir.join(crate::eula::EULA_FILE).exists();
    let directory = ServerDirectory::load_or_init(dir, options.mc_version)?;
    directory.ensure_standard_folders()?;
    actions.push(ReportEntry {
        path: dir.join(crate::eula::EULA_FILE),
        action: if eula_existed { "kept" } else { "created" },
    });

    let run_sh = dir.join("run.sh");
    atomic_write(&run_sh, RUN_SH.as_bytes())?;
    mark_executable(&run_sh)?;
    actions.push(ReportEntry {
        path: run_sh,
        action: "created",
    });
    let run_bat = dir.join("run.bat");
    atomic_write(&run_bat, RUN_BAT.as_bytes())?;
    actions.push(ReportEntry {
        path: run_bat,
        action: "created",
    });
    let readme = dir.join("README-SERVER.txt");
    atomic_write(&readme, SERVER_README.as_bytes())?;
    actions.push(ReportEntry {
        path: readme,
        action: "created",
    });

    Ok(InstallReport {
        summary: format!("Installed megu server launcher into {}", dir.display()),
        actions,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_matches_the_published_vectors() {
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        // Spans the two-block padding boundary.
        let msg = b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq";
        assert_eq!(
            sha256_hex(msg),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
        let mut hasher = Sha256::new();
        hasher.update(&vec![b'a'; 1_000_000]);
        assert_eq!(
            hasher.finalize_hex(),
            "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0"
        );
    }

    #[test]
    fn iso8601_conversion_covers_epoch_leap_and_far_future() {
        assert_eq!(unix_seconds_to_iso8601(0), "1970-01-01T00:00:00+00:00");
        assert_eq!(unix_seconds_to_iso8601(86_399), "1970-01-01T23:59:59+00:00");
        assert_eq!(unix_seconds_to_iso8601(86_400), "1970-01-02T00:00:00+00:00");
        // 2000-02-29 and 2024-02-29 are leap days.
        assert_eq!(
            unix_seconds_to_iso8601(951_782_400),
            "2000-02-29T00:00:00+00:00"
        );
        assert_eq!(
            unix_seconds_to_iso8601(1_709_164_800),
            "2024-02-29T00:00:00+00:00"
        );
        assert_eq!(
            unix_seconds_to_iso8601(4_102_444_800),
            "2100-01-01T00:00:00+00:00"
        );
    }

    #[test]
    fn os_detection_matches_launcher_layouts() {
        let home = Path::new("/home/u");
        let appdata = Path::new("C:\\Users\\u\\AppData\\Roaming");
        assert_eq!(
            detect_minecraft_dir_for_os(OsKind::Windows, home, Some(appdata)),
            appdata.join(".minecraft")
        );
        assert_eq!(
            detect_minecraft_dir_for_os(OsKind::Windows, home, None),
            home.join("AppData/Roaming/.minecraft")
        );
        assert_eq!(
            detect_minecraft_dir_for_os(OsKind::MacOs, home, None),
            home.join("Library/Application Support/minecraft")
        );
        assert_eq!(
            detect_minecraft_dir_for_os(OsKind::Linux, home, None),
            home.join(".minecraft")
        );
        // Flatpak wins only when the standard dir is absent.
        assert_eq!(
            resolve_minecraft_dir(OsKind::Linux, home, None, |p| p
                .ends_with(Path::new(".var/app/com.mojang.Minecraft/.minecraft"))),
            home.join(".var/app/com.mojang.Minecraft/.minecraft")
        );
        assert_eq!(
            resolve_minecraft_dir(OsKind::Linux, home, None, |_| true),
            home.join(".minecraft")
        );
        let xdg = Path::new("/xdg");
        assert_eq!(
            detect_prism_instances_dir_for_os(OsKind::Linux, home, None, Some(xdg)),
            xdg.join("PrismLauncher/instances")
        );
        assert_eq!(
            detect_prism_instances_dir_for_os(OsKind::Linux, home, None, None),
            home.join(".local/share/PrismLauncher/instances")
        );
        assert_eq!(
            detect_prism_instances_dir_for_os(OsKind::Windows, home, Some(appdata), None),
            appdata.join("PrismLauncher/instances")
        );
        assert_eq!(
            detect_prism_instances_dir_for_os(OsKind::MacOs, home, None, None),
            home.join("Library/Application Support/PrismLauncher/instances")
        );
    }
}
