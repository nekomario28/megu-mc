// SPDX-License-Identifier: LGPL-3.0-or-later
//! Zero-manual-step vanilla client JAR bootstrap — the Paper/NeoForge-grade
//! first-run path.
//!
//! A fresh Megu launch needs the vanilla client JAR for blockstates, models
//! and textures. Users must never be told to run `megu-installer` in a
//! terminal just to get vanilla textures, so this module does two things:
//!
//! * [`discover_local_vanilla_jar`] scans every place a client JAR for the
//!   requested version already lives — `<gameDir>/versions/`, the Official
//!   Launcher's `.minecraft` (incl. Flatpak), PrismLauncher/MultiMC-style
//!   library caches, the Modrinth App cache and Megu's own cache dir — on
//!   Linux, macOS and Windows. [`candidate_vanilla_jar_paths_for_os`] is the
//!   pure OS/env -> path-list mapping unit tests exercise offline.
//! * [`spawn_vanilla_jar_bootstrap`] runs the Mojang manifest -> version
//!   JSON -> client JAR chain on a background thread: HTTPS-only against an
//!   official-host allowlist, SHA-1 verified against the version manifest,
//!   streamed in bounded chunks to a `.part` file that is atomically
//!   renamed only after the digest and ZIP magic check out. The window
//!   loop polls [`VanillaJarBootstrap::progress`] and hot-reloads asset
//!   layers + title branding when [`BootstrapProgress::Ready`] lands.
//!
//! Auto-fetch is disabled when `MEGU_NO_AUTO_FETCH` or `MEGU_OFFLINE` is
//! set, and under `cfg(test)` unless a test explicitly spawns against a
//! loopback URL — unit tests never touch the real network.
//!
//! The SHA-1 and JSON reader are the canonical `megu_core::crypto::Sha1`
//! and `megu_core::json` implementations every crate shares.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use megu_core::crypto::Sha1;
use megu_core::json::{parse_json_with_limits, JsonLimits, JsonValue};

use crate::live_session::{jar_version_ids, SessionPaths};
use crate::vanilla_jar::{read_bounded_file, validate_client_zip, MAX_ARCHIVE_BYTES};

/// Mojang's version manifest — step 1 of the official download chain.
pub const VANILLA_MANIFEST_URL: &str =
    "https://piston-meta.mojang.com/mc/game/version_manifest_v2.json";
/// `version_manifest_v2.json` is ~1 MiB today; 4 MiB leaves headroom.
const MAX_MANIFEST_BYTES: u64 = 4 * 1024 * 1024;
/// Per-version metadata JSON cap.
const MAX_VERSION_JSON_BYTES: u64 = 2 * 1024 * 1024;
/// Mojang asset indexes are bounded separately from version metadata.
pub const MAX_ASSET_INDEX_BYTES: u64 = 32 * 1024 * 1024;
/// An individual asset is never allowed to exceed this disk/network budget.
pub const MAX_ASSET_OBJECT_BYTES: u64 = 256 * 1024 * 1024;
/// The 1.21.1 client JAR is ~26 MiB; 128 MiB bounds the download.
pub const MAX_CLIENT_JAR_BYTES: u64 = 128 * 1024 * 1024;
/// Download stream chunk size.
const DOWNLOAD_CHUNK: usize = 64 * 1024;
/// HTTPS-only official Mojang hosts the whole chain may resolve to.
const MOJANG_HOST_PREFIXES: [&str; 4] = [
    "https://piston-meta.mojang.com/",
    "https://piston-data.mojang.com/",
    "https://launchermeta.mojang.com/",
    "https://resources.download.minecraft.net/",
];

/// What the background bootstrap thread is doing right now — the UI
/// renders this on the title/startup status line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BootstrapProgress {
    /// Fetching `version_manifest_v2.json` or the per-version metadata.
    FetchingManifest,
    /// Streaming the client JAR; `total` is the manifest-declared size
    /// when known.
    Downloading { downloaded: u64, total: Option<u64> },
    /// Download complete; SHA-1 + ZIP magic verification in progress.
    Verifying,
    /// Client JAR verified and atomically in place.
    Ready(PathBuf),
    /// Terminal failure — the reason is user-presentable.
    Failed(String),
}

impl BootstrapProgress {
    /// `true` once the worker reached a terminal state.
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Ready(_) | Self::Failed(_))
    }
}

/// Human-readable one-liner for an in-flight [`BootstrapProgress`] —
/// e.g. `Downloading Minecraft 1.21.1 assets... 42% (11.2 / 26.8 MB)`.
/// `None` for terminal states (the caller writes those itself).
pub fn bootstrap_progress_label(version_id: &str, progress: &BootstrapProgress) -> Option<String> {
    match progress {
        BootstrapProgress::FetchingManifest => Some(format!(
            "Fetching Minecraft {version_id} download manifest..."
        )),
        BootstrapProgress::Downloading { downloaded, total } => {
            let done = *downloaded as f64 / (1024.0 * 1024.0);
            Some(match total {
                Some(total) if *total > 0 => {
                    let total_mb = *total as f64 / (1024.0 * 1024.0);
                    let pct = (*downloaded as f64 * 100.0 / *total as f64).min(100.0);
                    format!(
                        "Downloading Minecraft {version_id} assets... {pct:.0}% ({done:.1} / {total_mb:.1} MB)"
                    )
                }
                _ => format!("Downloading Minecraft {version_id} assets... {done:.1} MB"),
            })
        }
        BootstrapProgress::Verifying => Some(format!(
            "Verifying Minecraft {version_id} assets..."
        )),
        BootstrapProgress::Ready(_) | BootstrapProgress::Failed(_) => None,
    }
}

/// Pollable handle to the background download worker. The UI loop reads
/// [`Self::progress`] — cheap, non-blocking, lock-poison tolerant.
pub struct VanillaJarBootstrap {
    shared: Arc<Mutex<BootstrapProgress>>,
}

impl VanillaJarBootstrap {
    /// Latest worker state. `Ready`/`Failed` are terminal.
    pub fn progress(&self) -> BootstrapProgress {
        self.shared
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}

fn set_progress(shared: &Mutex<BootstrapProgress>, progress: BootstrapProgress) {
    *shared.lock().unwrap_or_else(|e| e.into_inner()) = progress;
}

/// `false` when the user opted out (`MEGU_NO_AUTO_FETCH`/`MEGU_OFFLINE`),
/// or under `cfg(test)` so unit tests can never hit the real network —
/// hermetic tests spawn [`spawn_vanilla_jar_bootstrap_with_manifest`]
/// against a loopback URL instead.
pub fn auto_fetch_enabled() -> bool {
    if cfg!(test) {
        return false;
    }
    std::env::var_os("MEGU_NO_AUTO_FETCH").is_none()
        && std::env::var_os("MEGU_OFFLINE").is_none()
}

/// Official-host HTTPS allowlist. Loopback `http://` is reachable only in
/// unit tests, where the hermetic `TcpListener` harness serves mock
/// Mojang responses.
fn url_allowed(url: &str) -> bool {
    #[cfg(test)]
    if url.starts_with("http://127.0.0.1:") || url.starts_with("http://localhost:") {
        return true;
    }
    MOJANG_HOST_PREFIXES.iter().any(|p| url.starts_with(p))
}

/// `version_id` is a directory name under `versions/` — refuse anything
/// that could escape it (separators, drive syntax, `.`/`..` components).
fn valid_version_id(version_id: &str) -> bool {
    !version_id.is_empty()
        && !matches!(version_id, "." | "..")
        && !version_id.contains(['/', '\\', ':'])
}

// ---------------------------------------------------------------------------
// Local discovery — scan launchers before ever touching the network
// ---------------------------------------------------------------------------

/// The OS layout family [`candidate_vanilla_jar_paths_for_os`] emits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LauncherOs {
    Linux,
    MacOs,
    Windows,
}

fn host_os() -> LauncherOs {
    match std::env::consts::OS {
        "macos" => LauncherOs::MacOs,
        "windows" => LauncherOs::Windows,
        _ => LauncherOs::Linux,
    }
}

/// Environment roots the candidate mapping reads — passed explicitly so
/// the mapping stays pure and testable on any host.
#[derive(Clone, Debug, Default)]
pub struct OsEnv {
    /// `$HOME` / `%USERPROFILE%`.
    pub home: Option<PathBuf>,
    /// `${XDG_DATA_HOME}` (Linux default `$HOME/.local/share`).
    pub xdg_data_home: Option<PathBuf>,
    /// `${XDG_CACHE_HOME}` (Linux default `$HOME/.cache`).
    pub xdg_cache_home: Option<PathBuf>,
    /// `%APPDATA%` (Windows roaming profile root).
    pub appdata: Option<PathBuf>,
    /// `%LOCALAPPDATA%` (Windows local profile root).
    pub localappdata: Option<PathBuf>,
    /// `%USERPROFILE%` — Windows `APPDATA` fallback root.
    pub userprofile: Option<PathBuf>,
}

impl OsEnv {
    /// Snapshot the real process environment.
    fn from_env() -> Self {
        let get = |key: &str| std::env::var_os(key).map(PathBuf::from);
        Self {
            home: get("HOME").or_else(|| get("USERPROFILE")),
            xdg_data_home: get("XDG_DATA_HOME"),
            xdg_cache_home: get("XDG_CACHE_HOME"),
            appdata: get("APPDATA"),
            localappdata: get("LOCALAPPDATA"),
            userprofile: get("USERPROFILE"),
        }
    }
}

fn join(path: &Path, parts: &[&str]) -> PathBuf {
    parts.iter().fold(path.to_path_buf(), |acc, part| acc.join(part))
}

/// Megu's own vanilla-JAR cache root (`versions/<id>/<id>.jar` under it).
/// Where the bootstrap downloads land when no `--gameDir` was passed.
pub fn megu_cache_dir(os: LauncherOs, env: &OsEnv) -> Option<PathBuf> {
    match os {
        LauncherOs::Linux => env
            .xdg_cache_home
            .clone()
            .or_else(|| env.home.as_ref().map(|h| h.join(".cache")))
            .map(|root| root.join("megu")),
        LauncherOs::MacOs => env
            .home
            .as_ref()
            .map(|h| join(h, &["Library", "Caches", "megu"])),
        LauncherOs::Windows => env
            .localappdata
            .clone()
            .or_else(|| {
                env.userprofile
                    .as_ref()
                    .map(|u| join(u, &["AppData", "Local"]))
            })
            .map(|root| root.join("megu")),
    }
}

/// Megu's own cache dir on this host (used as the bootstrap destination
/// when no `--gameDir` was passed).
pub fn default_vanilla_cache_dir() -> Option<PathBuf> {
    megu_cache_dir(host_os(), &OsEnv::from_env())
}

/// Every on-disk location a client JAR for `version_ids` may already
/// occupy, in priority order — the first readable ZIP wins:
///
/// 1. `<gameDir>/versions/<id>/<id>.jar` (explicit launcher root),
/// 2. the Official Launcher's `.minecraft` (native + Flatpak),
/// 3. PrismLauncher / Modrinth App library caches,
/// 4. Megu's own cache dir (where the background bootstrap writes when
///    no `--gameDir` was passed — always scanned so a previously fetched
///    JAR is picked up regardless of how Megu was launched).
///
/// Pure function: every input is explicit, so Linux/macOS/Windows layouts
/// are all covered by offline unit tests.
pub fn candidate_vanilla_jar_paths_for_os(
    os: LauncherOs,
    env: &OsEnv,
    game_dir: Option<&Path>,
    version_ids: &[String],
) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for id in version_ids.iter().filter(|id| valid_version_id(id)) {
        if let Some(game_dir) = game_dir {
            out.push(join(game_dir, &["versions", id, &format!("{id}.jar")]));
        }
        match os {
            LauncherOs::Linux => {
                if let Some(home) = &env.home {
                    out.push(join(
                        home,
                        &[".minecraft", "versions", id, &format!("{id}.jar")],
                    ));
                    out.push(join(
                        home,
                        &[
                            ".var", "app", "com.mojang.Minecraft", ".minecraft", "versions",
                            id, &format!("{id}.jar"),
                        ],
                    ));
                    let data = env
                        .xdg_data_home
                        .clone()
                        .unwrap_or_else(|| join(home, &[".local", "share"]));
                    out.push(join(
                        &data,
                        &[
                            "PrismLauncher", "libraries", "com", "mojang", "minecraft", id,
                            &format!("minecraft-{id}-client.jar"),
                        ],
                    ));
                    out.push(join(
                        home,
                        &[
                            ".var", "app", "org.prismlauncher.PrismLauncher", "data",
                            "PrismLauncher", "libraries", "com", "mojang", "minecraft", id,
                            &format!("minecraft-{id}-client.jar"),
                        ],
                    ));
                    out.push(join(
                        &data,
                        &["ModrinthApp", "meta", "versions", id, &format!("{id}.jar")],
                    ));
                }
            }
            LauncherOs::MacOs => {
                if let Some(home) = &env.home {
                    let app_support = join(home, &["Library", "Application Support"]);
                    out.push(join(
                        &app_support,
                        &["minecraft", "versions", id, &format!("{id}.jar")],
                    ));
                    out.push(join(
                        &app_support,
                        &[
                            "PrismLauncher", "libraries", "com", "mojang", "minecraft", id,
                            &format!("minecraft-{id}-client.jar"),
                        ],
                    ));
                    out.push(join(
                        &app_support,
                        &["ModrinthApp", "meta", "versions", id, &format!("{id}.jar")],
                    ));
                }
            }
            LauncherOs::Windows => {
                let roaming = env
                    .appdata
                    .clone()
                    .or_else(|| {
                        env.userprofile
                            .as_ref()
                            .map(|u| join(u, &["AppData", "Roaming"]))
                    });
                if let Some(roaming) = &roaming {
                    out.push(join(
                        roaming,
                        &[".minecraft", "versions", id, &format!("{id}.jar")],
                    ));
                    out.push(join(
                        roaming,
                        &[
                            "PrismLauncher", "libraries", "com", "mojang", "minecraft", id,
                            &format!("minecraft-{id}-client.jar"),
                        ],
                    ));
                    out.push(join(
                        roaming,
                        &["ModrinthApp", "meta", "versions", id, &format!("{id}.jar")],
                    ));
                }
            }
        }
        if let Some(cache) = megu_cache_dir(os, env) {
            out.push(join(&cache, &["versions", id, &format!("{id}.jar")]));
        }
    }
    out
}

/// Ordered local candidate list for a live session (real host OS + env).
pub fn local_vanilla_jar_candidates(paths: &SessionPaths) -> Vec<PathBuf> {
    candidate_vanilla_jar_paths_for_os(
        host_os(),
        &OsEnv::from_env(),
        paths.game_dir.as_deref(),
        &jar_version_ids(paths),
    )
}

/// `true` when `path` is a regular readable file within
/// [`MAX_ARCHIVE_BYTES`] that starts with ZIP magic `PK\x03\x04` — the
/// cheap gate every candidate must pass before the reader trusts it.
pub fn is_valid_jar_candidate(path: &Path) -> bool {
    open_regular_file(path).is_ok_and(|mut file| {
        file.metadata()
            .is_ok_and(|meta| meta.len() <= MAX_ARCHIVE_BYTES)
            && validate_client_zip(&mut file)
    })
}

/// First local candidate that is a real, readable client JAR — scans the
/// `<gameDir>` versions dir, Official/Prism/Modrinth launcher caches and
/// Megu's own cache, so a user who already installed the same Minecraft
/// version anywhere gets vanilla textures instantly.
pub fn discover_local_vanilla_jar(paths: &SessionPaths) -> Option<PathBuf> {
    local_vanilla_jar_candidates(paths)
        .into_iter()
        .find(|jar| is_valid_jar_candidate(jar))
}

/// The root a bootstrap download lands under (`versions/<id>/<id>.jar`
/// inside it): the explicit `--gameDir` when passed — mirroring the
/// layout every launcher already uses — else Megu's cache dir. `None`
/// when no cache location resolves (e.g. no `HOME`).
pub fn bootstrap_dest_dir(paths: &SessionPaths) -> Option<PathBuf> {
    paths.game_dir.clone().or_else(default_vanilla_cache_dir)
}

// ---------------------------------------------------------------------------
// Verified-download JSON parsing — canonical megu-core reader
// ---------------------------------------------------------------------------

/// Bounds for [`parse_vanilla_json`]: the retired parser capped container nesting
/// at 32 levels and the document at [`MAX_MANIFEST_BYTES`], with no node
/// ceiling of its own.
const JSON_LIMITS: JsonLimits = JsonLimits {
    max_depth: 33,
    max_nodes: usize::MAX,
    max_bytes: MAX_MANIFEST_BYTES as usize,
};

fn parse_vanilla_json(text: &str) -> Result<JsonValue, String> {
    parse_json_with_limits(text, JSON_LIMITS, false).map_err(|e| e.to_string())
}

/// Strict non-negative integer read for `downloads.client.size` —
/// [`JsonValue::as_u64`] truncates fractions and saturates negatives where
/// the retired parser rejected them; the declared size must stay exact.
fn strict_u64(value: &JsonValue) -> Option<u64> {
    let n = value.as_f64()?;
    (n.is_finite() && n >= 0.0 && n.fract() == 0.0).then_some(n as u64)
}

// ---------------------------------------------------------------------------
// Manifest chain parsing
// ---------------------------------------------------------------------------

/// `version_manifest_v2.json` -> the per-version metadata URL for
/// `version_id`. The extracted URL is re-checked against the Mojang
/// allowlist so a poisoned manifest can never redirect the fetch.
fn manifest_version_url(manifest_text: &str, version_id: &str) -> Result<String, String> {
    let manifest = parse_vanilla_json(manifest_text)?;
    let versions = manifest
        .get("versions")
        .and_then(JsonValue::as_array)
        .ok_or_else(|| "manifest has no versions array".to_owned())?;
    for entry in versions {
        if entry.get("id").and_then(JsonValue::as_str) == Some(version_id) {
            let url = entry
                .get("url")
                .and_then(JsonValue::as_str)
                .ok_or_else(|| format!("version {version_id:?} entry has no url"))?;
            if !url_allowed(url) {
                return Err(format!("version {version_id:?} url is not a Mojang host"));
            }
            return Ok(url.to_owned());
        }
    }
    Err(format!("version {version_id:?} not in Mojang manifest"))
}

/// `downloads.client` fields needed for the verified download.
struct ClientDownload {
    url: String,
    sha1: String,
    size: u64,
}

/// Per-version metadata JSON -> `downloads.client.{url,sha1,size}` with
/// every field range-checked and the URL allowlisted.
fn client_download_info(version_text: &str) -> Result<ClientDownload, String> {
    let json = parse_vanilla_json(version_text)?;
    let client = json
        .get("downloads")
        .and_then(|d| d.get("client"))
        .ok_or_else(|| "version JSON has no downloads.client".to_owned())?;
    let url = client
        .get("url")
        .and_then(JsonValue::as_str)
        .ok_or_else(|| "downloads.client has no url".to_owned())?;
    if !url_allowed(url) {
        return Err("client JAR url is not a Mojang host".to_owned());
    }
    let sha1 = client
        .get("sha1")
        .and_then(JsonValue::as_str)
        .ok_or_else(|| "downloads.client has no sha1".to_owned())?;
    if sha1.len() != 40
        || !sha1
            .bytes()
            .all(|b| b.is_ascii_digit() || matches!(b, b'a'..=b'f'))
    {
        return Err("downloads.client sha1 is not 40 lowercase hex".to_owned());
    }
    let size = client
        .get("size")
        .and_then(strict_u64)
        .ok_or_else(|| "downloads.client has no size".to_owned())?;
    if size == 0 || size > MAX_CLIENT_JAR_BYTES {
        return Err(format!("client JAR size {size} outside 1..={MAX_CLIENT_JAR_BYTES}"));
    }
    Ok(ClientDownload {
        url: url.to_owned(),
        sha1: sha1.to_ascii_lowercase(),
        size,
    })
}

// ---------------------------------------------------------------------------
// Download worker
// ---------------------------------------------------------------------------

/// Open a cache directory and anchor its children to the opened directory
/// on Linux. Reject directory symlinks before traversing cache-managed names.
pub(crate) struct CacheDirectory {
    path: PathBuf,
    #[cfg(target_os = "linux")]
    _directory: File,
}

impl CacheDirectory {
    pub(crate) fn open(root: &Path) -> io::Result<Self> {
        use std::path::Component;
        let absolute = std::path::absolute(root)?;
        let mut anchor = PathBuf::new();
        let mut directory: Option<Self> = None;
        for component in absolute.components() {
            match component {
                Component::Prefix(_) | Component::RootDir => anchor.push(component.as_os_str()),
                Component::Normal(name) => {
                    let parent = match directory.take() {
                        Some(parent) => parent,
                        None => Self::open_or_create(&anchor)?,
                    };
                    directory = Some(Self::open_or_create(&parent.path.join(name))?);
                }
                Component::CurDir => {}
                Component::ParentDir => {
                    return Err(io::Error::other("cache path contains a parent traversal"))
                }
            }
        }
        directory.map_or_else(|| Self::open_or_create(&anchor), Ok)
    }

    fn open_or_create(path: &Path) -> io::Result<Self> {
        match fs::create_dir(path) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e),
        }
        let checked = fs::symlink_metadata(path)?;
        if !checked.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "cache directory is not a real directory",
            ));
        }
        #[cfg(target_os = "linux")]
        {
            use std::os::{fd::AsRawFd, unix::fs::MetadataExt};
            let directory = File::open(path)?;
            let opened = directory.metadata()?;
            if opened.dev() != checked.dev() || opened.ino() != checked.ino() {
                return Err(io::Error::other("cache directory changed during open"));
            }
            Ok(Self {
                path: PathBuf::from(format!("/proc/self/fd/{}", directory.as_raw_fd())),
                _directory: directory,
            })
        }
        #[cfg(not(target_os = "linux"))]
        Ok(Self {
            path: fs::canonicalize(path)?,
        })
    }

    pub(crate) fn child(&self, name: &str) -> io::Result<Self> {
        Self::open_or_create(&self.path.join(name))
    }

    pub(crate) fn file(&self, name: &str) -> PathBuf {
        self.path.join(name)
    }
}

pub(crate) fn open_regular_file(path: &Path) -> io::Result<File> {
    let checked = fs::symlink_metadata(path)?;
    if !checked.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "cache entry is not a regular file",
        ));
    }
    let file = File::open(path)?;
    if !file.metadata()?.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "cache entry is not a regular file",
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let opened = file.metadata()?;
        if opened.dev() != checked.dev() || opened.ino() != checked.ino() {
            return Err(io::Error::other("cache entry changed during open"));
        }
    }
    Ok(file)
}

pub(crate) struct StagedFile {
    file: Option<File>,
    path: PathBuf,
}

impl StagedFile {
    pub(crate) fn new(destination: &Path) -> io::Result<Self> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        for _ in 0..32 {
            let nonce = NEXT.fetch_add(1, Ordering::Relaxed);
            let path = destination.with_file_name(format!(
                ".megu-{}-{stamp:x}-{nonce:x}.part",
                std::process::id()
            ));
            match OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(file) => {
                    return Ok(Self {
                        file: Some(file),
                        path,
                    })
                }
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(e),
            }
        }
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "cannot allocate a unique staging file",
        ))
    }

    pub(crate) fn file(&mut self) -> &mut File {
        self.file.as_mut().expect("staging file is open")
    }

    pub(crate) fn install(mut self, destination: &Path) -> io::Result<()> {
        let file = self.file.take().expect("staging file is open");
        file.sync_all()?;
        drop(file);
        fs::rename(&self.path, destination)?;
        self.path.clear();
        Ok(())
    }
}

impl Drop for StagedFile {
    fn drop(&mut self) {
        drop(self.file.take());
        if !self.path.as_os_str().is_empty() {
            let _ = fs::remove_file(&self.path);
        }
    }
}

fn successful_response(
    agent: &ureq::Agent,
    url: &str,
) -> Result<ureq::http::Response<ureq::Body>, String> {
    let response = agent
        .get(url)
        .call()
        .map_err(|e| format!("GET {url}: {e}"))?;
    if !response.status().is_success() {
        return Err(format!("GET {url}: HTTP {}", response.status()));
    }
    Ok(response)
}

fn http_agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .max_redirects(0)
        .timeout_connect(Some(Duration::from_secs(15)))
        .timeout_recv_response(Some(Duration::from_secs(15)))
        // Body budget is a whole-body bound (ureq semantics): the ~26 MiB
        // JAR on a slow link needs minutes, not the 15 s header budget.
        .timeout_recv_body(Some(Duration::from_secs(600)))
        .build()
        .into()
}

/// Bounded GET -> UTF-8 body. Fails closed on non-allowlisted URLs and
/// bodies past `cap`.
fn fetch_text(agent: &ureq::Agent, url: &str, cap: u64) -> Result<String, String> {
    if !url_allowed(url) {
        return Err(format!("refusing non-Mojang URL {url}"));
    }
    let mut response = successful_response(agent, url)?;
    response
        .body_mut()
        .with_config()
        .limit(cap)
        .read_to_string()
        .map_err(|e| format!("GET {url}: {e}"))
}

fn fetch_asset_version_text(agent: &ureq::Agent, url: &str) -> Result<String, String> {
    if !url_allowed(url) && !loopback_http_url(url) {
        return Err(format!("refusing non-Mojang URL {url}"));
    }
    let mut response = successful_response(agent, url)?;
    response
        .body_mut()
        .with_config()
        .limit(MAX_VERSION_JSON_BYTES)
        .read_to_string()
        .map_err(|e| format!("GET {url}: {e}"))
}

/// Stream and verify the JAR in an exclusive staging file before installing it.
fn download_and_verify(
    agent: &ureq::Agent,
    client: &ClientDownload,
    jar: &Path,
    shared: &Mutex<BootstrapProgress>,
) -> Result<(), String> {
    let mut response = successful_response(agent, &client.url)?;
    let total = response.body().content_length().or(Some(client.size));
    let mut staged = StagedFile::new(jar).map_err(|e| format!("stage client JAR: {e}"))?;
    set_progress(
        shared,
        BootstrapProgress::Downloading {
            downloaded: 0,
            total,
        },
    );
    let actual = transfer_hashed(
        response.body_mut().as_reader(),
        staged.file(),
        client.size,
        |downloaded| {
            set_progress(shared, BootstrapProgress::Downloading { downloaded, total });
        },
    )?;
    set_progress(shared, BootstrapProgress::Verifying);
    if actual != client.sha1 {
        return Err(format!(
            "SHA-1 mismatch: got {actual}, manifest declared {}",
            client.sha1
        ));
    }
    staged.file().rewind().map_err(|e| e.to_string())?;
    if !validate_client_zip(staged.file()) {
        return Err("downloaded client JAR has an invalid ZIP directory".into());
    }
    staged
        .install(jar)
        .map_err(|e| format!("install client JAR: {e}"))
}

fn transfer_hashed(
    mut reader: impl Read,
    mut writer: impl Write,
    size: u64,
    mut progress: impl FnMut(u64),
) -> Result<String, String> {
    let mut hash = Sha1::new();
    let mut count = 0u64;
    let mut buffer = [0; DOWNLOAD_CHUNK];
    loop {
        let n = reader.read(&mut buffer).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        count += n as u64;
        if count > size {
            return Err("input exceeded declared size".into());
        }
        hash.update(&buffer[..n]);
        writer.write_all(&buffer[..n]).map_err(|e| e.to_string())?;
        progress(count);
    }
    if count != size {
        return Err(format!("input is {count} bytes, expected {size}"));
    }
    Ok(hash.finalize_hex())
}

fn bootstrap_run(
    version_id: &str,
    dest_root: &Path,
    manifest_url: &str,
    shared: &Mutex<BootstrapProgress>,
) -> Result<PathBuf, String> {
    let version_dir = dest_root.join("versions").join(version_id);
    let jar = version_dir.join(format!("{version_id}.jar"));
    // A JAR that appeared since discovery ran (another launcher, a
    // previous successful fetch) already satisfies the requirement.
    if is_valid_jar_candidate(&jar) {
        return Ok(jar);
    }
    let agent = http_agent();
    set_progress(shared, BootstrapProgress::FetchingManifest);
    let manifest_text = fetch_text(&agent, manifest_url, MAX_MANIFEST_BYTES)?;
    let version_url = manifest_version_url(&manifest_text, version_id)?;
    let version_text = fetch_text(&agent, &version_url, MAX_VERSION_JSON_BYTES)?;
    let client = client_download_info(&version_text)?;
    let dir = CacheDirectory::open(dest_root)
        .and_then(|dir| dir.child("versions"))
        .and_then(|dir| dir.child(version_id))
        .map_err(|e| format!("create versions cache: {e}"))?;
    download_and_verify(
        &agent,
        &client,
        &dir.file(&format!("{version_id}.jar")),
        shared,
    )?;
    Ok(jar)
}

/// Spawn the manifest -> version JSON -> SHA-1-verified client JAR
/// download chain on a daemon thread. Returns `None` for an unsafe
/// `version_id` or a non-allowlisted manifest URL; progress is polled
/// non-blockingly from the UI loop through the returned handle.
#[doc(hidden)]
pub fn spawn_vanilla_jar_bootstrap_with_manifest(
    version_id: &str,
    dest_game_dir: &Path,
    manifest_url: &str,
) -> Option<VanillaJarBootstrap> {
    if !valid_version_id(version_id) || !url_allowed(manifest_url) {
        return None;
    }
    let shared = Arc::new(Mutex::new(BootstrapProgress::FetchingManifest));
    let worker_shared = Arc::clone(&shared);
    let version = version_id.to_owned();
    let dest = dest_game_dir.to_path_buf();
    let url = manifest_url.to_owned();
    std::thread::Builder::new()
        .name("megu-vanilla-jar-bootstrap".to_owned())
        .spawn(move || {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                bootstrap_run(&version, &dest, &url, &worker_shared)
            }));
            let outcome = match result {
                Ok(Ok(jar)) => BootstrapProgress::Ready(jar),
                Ok(Err(reason)) => BootstrapProgress::Failed(reason),
                Err(_) => BootstrapProgress::Failed("download worker crashed".to_owned()),
            };
            set_progress(&worker_shared, outcome);
        })
        .ok()?;
    Some(VanillaJarBootstrap { shared })
}

/// [`spawn_vanilla_jar_bootstrap_with_manifest`] pinned to the official
/// Mojang version manifest.
pub fn spawn_vanilla_jar_bootstrap(
    version_id: &str,
    dest_game_dir: &Path,
) -> Option<VanillaJarBootstrap> {
    spawn_vanilla_jar_bootstrap_with_manifest(version_id, dest_game_dir, VANILLA_MANIFEST_URL)
}

// ---------------------------------------------------------------------------
// Mojang asset bootstrap
// ---------------------------------------------------------------------------

/// The Mojang `assetIndex` reference embedded in a version JSON document.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AssetIndexRef {
    pub id: String,
    pub sha1: String,
    pub size: u64,
    pub url: String,
}

/// Downloadable object from an asset index, ordered by startup importance.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AssetObjectSpec {
    pub logical_path: String,
    pub hash: String,
    pub size: u64,
    pub priority: AssetPriority,
}

/// Stable asset-fetch priority. Derive order is the transfer order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum AssetPriority {
    Tier0FontsAndLang,
    Tier1UiAndIcons,
    Tier2Sounds,
    Tier3Other,
}

/// Optional bandwidth policy for asset synchronization.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AssetTierFilter {
    All,
    FontsAndLangOnly,
}

/// Observable state of the asset worker.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AssetBootstrapProgress {
    FetchingIndex,
    Downloading {
        completed_objects: usize,
        total_objects: usize,
        downloaded_bytes: u64,
        total_bytes: u64,
        fonts_ready: bool,
    },
    Ready {
        assets_dir: PathBuf,
        index_id: String,
    },
    Failed(String),
}

impl AssetBootstrapProgress {
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Ready { .. } | Self::Failed(_))
    }
}

/// Pollable handle for an asset bootstrap worker.
pub struct VanillaAssetsBootstrap {
    shared: Arc<Mutex<AssetBootstrapProgress>>,
}

impl VanillaAssetsBootstrap {
    pub fn progress(&self) -> AssetBootstrapProgress {
        self.shared
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}

fn set_asset_progress(shared: &Mutex<AssetBootstrapProgress>, progress: AssetBootstrapProgress) {
    *shared.lock().unwrap_or_else(|e| e.into_inner()) = progress;
}

fn valid_sha1(value: &str) -> bool {
    value.len() == 40
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || matches!(b, b'a'..=b'f'))
}

fn safe_asset_index_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// Parse and validate `assetIndex` from Mojang's version metadata.
pub fn parse_asset_index_ref(version_text: &str) -> Result<AssetIndexRef, String> {
    let json = parse_vanilla_json(version_text)?;
    let index = json
        .get("assetIndex")
        .ok_or_else(|| "version JSON has no assetIndex".to_owned())?;
    let id = index
        .get("id")
        .and_then(JsonValue::as_str)
        .ok_or_else(|| "assetIndex has no id".to_owned())?;
    if !safe_asset_index_id(id) {
        return Err("assetIndex id is unsafe".to_owned());
    }
    let sha1 = index
        .get("sha1")
        .and_then(JsonValue::as_str)
        .ok_or_else(|| "assetIndex has no sha1".to_owned())?;
    if !valid_sha1(sha1) {
        return Err("assetIndex sha1 is not 40 lowercase hex".to_owned());
    }
    let size = index
        .get("size")
        .and_then(strict_u64)
        .ok_or_else(|| "assetIndex has no exact non-negative size".to_owned())?;
    if size == 0 || size > MAX_ASSET_INDEX_BYTES {
        return Err(format!(
            "asset index size {size} outside 1..={MAX_ASSET_INDEX_BYTES}"
        ));
    }
    let url = index
        .get("url")
        .and_then(JsonValue::as_str)
        .ok_or_else(|| "assetIndex has no url".to_owned())?;
    if !url_allowed(url) {
        return Err("assetIndex url is not a Mojang HTTPS host".to_owned());
    }
    Ok(AssetIndexRef {
        id: id.to_owned(),
        sha1: sha1.to_owned(),
        size,
        url: url.to_owned(),
    })
}

fn valid_logical_asset_path(path: &str) -> bool {
    !path.is_empty()
        && path.len() <= 1024
        && !path.starts_with('/')
        && !path.contains('\\')
        && path
            .split('/')
            .all(|part| !part.is_empty() && part != "." && part != "..")
}

fn asset_priority(path: &str) -> AssetPriority {
    if path.starts_with("minecraft/font/") || path.starts_with("minecraft/lang/") {
        AssetPriority::Tier0FontsAndLang
    } else if path.starts_with("icons/")
        || path.starts_with("minecraft/texts/")
        || path.starts_with("minecraft/shaders/")
    {
        AssetPriority::Tier1UiAndIcons
    } else if path == "minecraft/sounds.json" || path.starts_with("minecraft/sounds/") {
        AssetPriority::Tier2Sounds
    } else {
        AssetPriority::Tier3Other
    }
}

/// Parse the content-addressed `objects` map and return deterministic priority order.
pub fn parse_asset_objects(index_text: &str) -> Result<Vec<AssetObjectSpec>, String> {
    let limits = JsonLimits {
        max_depth: 8,
        max_nodes: 2_000_000,
        max_bytes: MAX_ASSET_INDEX_BYTES as usize,
    };
    let json = parse_json_with_limits(index_text, limits, true).map_err(|e| e.to_string())?;
    let objects = json
        .get("objects")
        .and_then(JsonValue::as_object)
        .ok_or_else(|| "asset index has no objects object".to_owned())?;
    let mut parsed = Vec::with_capacity(objects.len());
    for (logical_path, value) in objects {
        if !valid_logical_asset_path(logical_path) {
            return Err(format!("unsafe asset logical path {logical_path:?}"));
        }
        let hash = value
            .get("hash")
            .and_then(JsonValue::as_str)
            .ok_or_else(|| format!("asset {logical_path:?} has no hash"))?;
        if !valid_sha1(hash) {
            return Err(format!(
                "asset {logical_path:?} hash is not 40 lowercase hex"
            ));
        }
        let size = value
            .get("size")
            .and_then(strict_u64)
            .ok_or_else(|| format!("asset {logical_path:?} has no exact size"))?;
        if size > MAX_ASSET_OBJECT_BYTES {
            return Err(format!(
                "asset {logical_path:?} exceeds per-object size cap"
            ));
        }
        parsed.push(AssetObjectSpec {
            logical_path: logical_path.clone(),
            hash: hash.to_owned(),
            size,
            priority: asset_priority(logical_path),
        });
    }
    parsed.sort_by(|a, b| {
        let a_music = a.logical_path.starts_with("minecraft/sounds/music/")
            && a.logical_path.ends_with(".ogg");
        let b_music = b.logical_path.starts_with("minecraft/sounds/music/")
            && b.logical_path.ends_with(".ogg");
        (
            a.priority,
            a.logical_path != "minecraft/sounds.json",
            a_music,
            &a.logical_path,
        )
            .cmp(&(
                b.priority,
                b.logical_path != "minecraft/sounds.json",
                b_music,
                &b.logical_path,
            ))
    });
    Ok(parsed)
}

fn asset_dir_candidates(
    os: LauncherOs,
    env: &OsEnv,
    game_dir: Option<&Path>,
    assets_dir: Option<&Path>,
) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Some(dir) = assets_dir {
        out.push(dir.to_path_buf());
    }
    if let Some(dir) = game_dir {
        out.push(dir.join("assets"));
    }
    if let Some(home) = &env.home {
        match os {
            LauncherOs::Linux => {
                out.push(home.join(".minecraft/assets"));
                out.push(home.join(".var/app/com.mojang.Minecraft/.minecraft/assets"));
                let data = env
                    .xdg_data_home
                    .clone()
                    .unwrap_or_else(|| home.join(".local/share"));
                out.push(data.join("PrismLauncher/assets"));
                out.push(
                    home.join(".var/app/org.prismlauncher.PrismLauncher/data/PrismLauncher/assets"),
                );
                out.push(data.join("ModrinthApp/meta/assets"));
            }
            LauncherOs::MacOs => {
                let support = home.join("Library/Application Support");
                out.push(support.join("minecraft/assets"));
                out.push(support.join("PrismLauncher/assets"));
                out.push(support.join("ModrinthApp/meta/assets"));
            }
            LauncherOs::Windows => {
                let roaming = env
                    .appdata
                    .clone()
                    .or_else(|| env.userprofile.as_ref().map(|u| u.join("AppData/Roaming")));
                if let Some(root) = roaming {
                    out.push(root.join(".minecraft/assets"));
                    out.push(root.join("PrismLauncher/assets"));
                    out.push(root.join("ModrinthApp/meta/assets"));
                }
            }
        }
    }
    if let Some(cache) = megu_cache_dir(os, env) {
        out.push(cache.join("assets"));
    }
    out
}

/// Find an existing local `indexes/<id>.json` and `objects/` tree.
pub fn discover_local_assets_dir(index_id: &str, paths: &SessionPaths) -> Option<PathBuf> {
    if !safe_asset_index_id(index_id) {
        return None;
    }
    asset_dir_candidates(
        host_os(),
        &OsEnv::from_env(),
        paths.game_dir.as_deref(),
        paths.assets_dir.as_deref(),
    )
    .into_iter()
    .find(|dir| {
        dir.join("indexes")
            .join(format!("{index_id}.json"))
            .is_file()
            && dir.join("objects").is_dir()
    })
}

fn file_matches_spec(path: &Path, spec: &AssetObjectSpec) -> bool {
    let Ok(file) = open_regular_file(path) else {
        return false;
    };
    file.metadata().is_ok_and(|meta| meta.len() == spec.size)
        && transfer_hashed(file, io::sink(), spec.size, |_| {}).is_ok_and(|hash| hash == spec.hash)
}

fn fetch_asset_index(
    agent: &ureq::Agent,
    reference: &AssetIndexRef,
    url: &str,
    final_path: &Path,
) -> Result<String, String> {
    if !url_allowed(url) && !loopback_http_url(url) {
        return Err("refusing non-Mojang asset index URL".to_owned());
    }
    let mut response = successful_response(agent, url)?;
    let mut body = Vec::new();
    let actual = transfer_hashed(
        response.body_mut().as_reader(),
        &mut body,
        reference.size,
        |_| {},
    )?;
    if actual != reference.sha1 {
        return Err("asset index SHA-1 mismatch".into());
    }
    let text = String::from_utf8(body).map_err(|e| format!("asset index is not UTF-8: {e}"))?;
    install_bytes(final_path, text.as_bytes()).map_err(|e| format!("install asset index: {e}"))?;
    Ok(text)
}

fn install_bytes(destination: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut staged = StagedFile::new(destination)?;
    staged.file().write_all(bytes)?;
    staged.install(destination)
}

fn loopback_http_url(url: &str) -> bool {
    url.starts_with("http://127.0.0.1:") || url.starts_with("http://localhost:")
}

fn download_asset_object(
    agent: &ureq::Agent,
    spec: &AssetObjectSpec,
    url: &str,
    dest: &Path,
) -> Result<(), String> {
    if !url_allowed(url) && !loopback_http_url(url) {
        return Err(format!("refusing non-Mojang asset URL {url}"));
    }
    let mut response = successful_response(agent, url)?;
    install_asset_object(response.body_mut().as_reader(), spec, dest)
}

fn install_asset_object(
    reader: impl Read,
    spec: &AssetObjectSpec,
    dest: &Path,
) -> Result<(), String> {
    let mut staged = StagedFile::new(dest).map_err(|e| format!("stage asset: {e}"))?;
    let actual = transfer_hashed(reader, staged.file(), spec.size, |_| {})?;
    if actual != spec.hash {
        return Err(format!("asset {} SHA-1 mismatch", spec.logical_path));
    }
    staged
        .install(dest)
        .map_err(|e| format!("install asset: {e}"))
}

struct AssetBootstrapEndpoints {
    index_url: Option<String>,
    object_base_url: Option<String>,
    local_candidates: Option<Vec<PathBuf>>,
}

fn assets_run(
    index_id: &str,
    version_json_url: &str,
    dest: &Path,
    tier_filter: AssetTierFilter,
    endpoints: &AssetBootstrapEndpoints,
    shared: &Mutex<AssetBootstrapProgress>,
) -> Result<(), String> {
    let agent = http_agent();
    let root = CacheDirectory::open(dest).map_err(|e| format!("open asset cache: {e}"))?;
    let indexes = root
        .child("indexes")
        .map_err(|e| format!("open index cache: {e}"))?;
    let object_root = root
        .child("objects")
        .map_err(|e| format!("open object cache: {e}"))?;
    let index_name = format!("{index_id}.json");
    let index_destination = indexes.file(&index_name);
    // Discover complete launcher layouts before any network request. A local
    // index is bounded and its objects are still independently SHA-1 checked.
    let local_candidates = endpoints
        .local_candidates
        .clone()
        .unwrap_or_else(|| vec![dest.to_path_buf()]);
    let local = local_candidates.iter().find(|dir| {
        dir.join("indexes")
            .join(format!("{index_id}.json"))
            .is_file()
            && dir.join("objects").is_dir()
    });
    let local_index = local.and_then(|source_dir| {
        let source = if source_dir == dest {
            index_destination.clone()
        } else {
            source_dir.join("indexes").join(&index_name)
        };
        let text =
            String::from_utf8(read_bounded_file(&source, MAX_ASSET_INDEX_BYTES as usize).ok()?)
                .ok()?;
        parse_asset_objects(&text).ok()?;
        Some((source_dir, text))
    });
    let index_text = if let Some((source_dir, text)) = local_index {
        if source_dir != dest {
            install_bytes(&index_destination, text.as_bytes())
                .map_err(|e| format!("install local asset index: {e}"))?;
        }
        text
    } else {
        set_asset_progress(shared, AssetBootstrapProgress::FetchingIndex);
        let version_text = fetch_asset_version_text(&agent, version_json_url)?;
        let reference = parse_asset_index_ref(&version_text)?;
        if reference.id != index_id {
            return Err(format!(
                "requested asset index {index_id:?}, version JSON declares {:?}",
                reference.id
            ));
        }
        let index_url = endpoints.index_url.as_deref().unwrap_or(&reference.url);
        if !url_allowed(index_url) && !loopback_http_url(index_url) {
            return Err("asset index endpoint is not allowlisted".to_owned());
        }
        fetch_asset_index(&agent, &reference, index_url, &index_destination)?
    };
    let mut objects = parse_asset_objects(&index_text)?;
    if tier_filter == AssetTierFilter::FontsAndLangOnly {
        objects.retain(|o| o.priority == AssetPriority::Tier0FontsAndLang);
    }
    let total_objects = objects.len();
    let total_bytes = objects.iter().map(|o| o.size).sum();
    let completed_objects = 0;
    let mut downloaded_bytes = 0u64;
    let mut fonts_ready = objects
        .iter()
        .all(|object| object.priority != AssetPriority::Tier0FontsAndLang);
    set_asset_progress(
        shared,
        AssetBootstrapProgress::Downloading {
            completed_objects,
            total_objects,
            downloaded_bytes,
            total_bytes,
            fonts_ready,
        },
    );
    let object_base = endpoints
        .object_base_url
        .as_deref()
        .unwrap_or("https://resources.download.minecraft.net");
    for (index, spec) in objects.iter().enumerate() {
        let prefix = &spec.hash[..2];
        let object_dir = object_root
            .child(prefix)
            .map_err(|e| format!("open object prefix: {e}"))?;
        let object_path = object_dir.file(&spec.hash);
        if !file_matches_spec(&object_path, spec) {
            let reused = local_candidates
                .iter()
                .filter(|dir| dir.as_path() != dest)
                .any(|source_dir| {
                    let source = source_dir.join("objects").join(prefix).join(&spec.hash);
                    open_regular_file(&source).is_ok_and(|file| {
                        file.metadata().is_ok_and(|meta| meta.len() == spec.size)
                            && install_asset_object(file, spec, &object_path).is_ok()
                    })
                });
            if !reused {
                let url = format!(
                    "{}/{}/{}",
                    object_base.trim_end_matches('/'),
                    prefix,
                    spec.hash
                );
                download_asset_object(&agent, spec, &url, &object_path)?;
                downloaded_bytes = downloaded_bytes.saturating_add(spec.size);
            }
        }
        let completed_objects = index + 1;
        if spec.priority == AssetPriority::Tier0FontsAndLang
            && objects
                .iter()
                .filter(|o| o.priority == AssetPriority::Tier0FontsAndLang)
                .all(|o| {
                    file_matches_spec(&dest.join("objects").join(&o.hash[..2]).join(&o.hash), o)
                })
        {
            fonts_ready = true;
        }
        set_asset_progress(
            shared,
            AssetBootstrapProgress::Downloading {
                completed_objects,
                total_objects,
                downloaded_bytes,
                total_bytes,
                fonts_ready,
            },
        );
    }
    Ok(())
}

fn spawn_assets_worker(
    index_id: &str,
    version_json_url: &str,
    dest: &Path,
    tier_filter: AssetTierFilter,
    index_url_override: Option<String>,
    object_base_override: Option<String>,
    local_candidates: Option<Vec<PathBuf>>,
) -> Option<VanillaAssetsBootstrap> {
    let injected_endpoints = index_url_override.is_some() && object_base_override.is_some();
    if !safe_asset_index_id(index_id)
        || (!url_allowed(version_json_url)
            && !(injected_endpoints && loopback_http_url(version_json_url)))
    {
        return None;
    }
    if let Some(url) = &index_url_override {
        if !url_allowed(url) && !(injected_endpoints && loopback_http_url(url)) {
            return None;
        }
    }
    if let Some(url) = &object_base_override {
        if !url_allowed(url) && !(injected_endpoints && loopback_http_url(url)) {
            return None;
        }
    }
    let shared = Arc::new(Mutex::new(AssetBootstrapProgress::FetchingIndex));
    let worker_shared = Arc::clone(&shared);
    let id = index_id.to_owned();
    let version_url = version_json_url.to_owned();
    let root = dest.to_path_buf();
    let endpoints = AssetBootstrapEndpoints {
        index_url: index_url_override,
        object_base_url: object_base_override,
        local_candidates,
    };
    std::thread::Builder::new()
        .name("megu-vanilla-assets-bootstrap".to_owned())
        .spawn(move || {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                assets_run(
                    &id,
                    &version_url,
                    &root,
                    tier_filter,
                    &endpoints,
                    &worker_shared,
                )
            }));
            let final_state = match result {
                Ok(Ok(())) => AssetBootstrapProgress::Ready {
                    assets_dir: root,
                    index_id: id,
                },
                Ok(Err(reason)) => AssetBootstrapProgress::Failed(reason),
                Err(_) => {
                    AssetBootstrapProgress::Failed("asset download worker crashed".to_owned())
                }
            };
            set_asset_progress(&worker_shared, final_state);
        })
        .ok()?;
    Some(VanillaAssetsBootstrap { shared })
}

/// Spawn a Mojang asset bootstrap using the session's launcher paths and the
/// official resource object endpoint. Existing launcher stores are scanned first.
pub fn spawn_vanilla_assets_bootstrap(
    index_id: &str,
    version_json_url: &str,
    paths: &SessionPaths,
    tier_filter: AssetTierFilter,
) -> Option<VanillaAssetsBootstrap> {
    let dest_assets = paths
        .assets_dir
        .clone()
        .or_else(|| paths.game_dir.as_ref().map(|dir| dir.join("assets")))
        .or_else(|| default_vanilla_cache_dir().map(|dir| dir.join("assets")))?;
    let local_candidates = asset_dir_candidates(
        host_os(),
        &OsEnv::from_env(),
        paths.game_dir.as_deref(),
        paths.assets_dir.as_deref(),
    );
    spawn_assets_worker(
        index_id,
        version_json_url,
        &dest_assets,
        tier_filter,
        None,
        None,
        Some(local_candidates),
    )
}

/// Endpoint-injectable variant for hermetic loopback tests and controlled mirrors.
#[doc(hidden)]
pub fn spawn_vanilla_assets_bootstrap_with_endpoints(
    index_id: &str,
    version_json_url: &str,
    dest_assets: &Path,
    index_url: &str,
    object_base_url: &str,
    tier_filter: AssetTierFilter,
) -> Option<VanillaAssetsBootstrap> {
    spawn_assets_worker(
        index_id,
        version_json_url,
        dest_assets,
        tier_filter,
        Some(index_url.to_owned()),
        Some(object_base_url.to_owned()),
        None,
    )
}

// ---------------------------------------------------------------------------
// Tests — fully offline: candidate mapping is pure, SHA-1 uses published
// vectors, and the download chain is served by a hermetic loopback
// TcpListener. No test ever touches a real network host.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use megu_core::crypto::sha1_hex;
    use std::net::TcpListener;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static NEXT_TEMP_DIR: AtomicUsize = AtomicUsize::new(0);

    fn test_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "megu-bootstrap-{label}-{}-{}",
            std::process::id(),
            NEXT_TEMP_DIR.fetch_add(1, Ordering::Relaxed),
        ));
        std::fs::create_dir_all(&dir).expect("test directory created");
        dir
    }

    fn pb(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    fn ids() -> Vec<String> {
        vec!["1.21.1".to_owned()]
    }

    // -- candidate mapping -------------------------------------------------

    #[test]
    fn linux_candidates_cover_game_dir_official_flatpak_prism_modrinth_cache() {
        let env = OsEnv {
            home: Some(pb("/home/alice")),
            ..OsEnv::default()
        };
        let out = candidate_vanilla_jar_paths_for_os(
            LauncherOs::Linux,
            &env,
            Some(&pb("/games/mc")),
            &ids(),
        );
        let strings: Vec<String> = out.iter().map(|p| p.display().to_string()).collect();
        assert_eq!(strings[0], "/games/mc/versions/1.21.1/1.21.1.jar");
        assert!(strings.contains(&"/home/alice/.minecraft/versions/1.21.1/1.21.1.jar".to_owned()));
        assert!(strings.contains(
            &"/home/alice/.var/app/com.mojang.Minecraft/.minecraft/versions/1.21.1/1.21.1.jar"
                .to_owned()
        ));
        assert!(strings.contains(
            &"/home/alice/.local/share/PrismLauncher/libraries/com/mojang/minecraft/1.21.1/minecraft-1.21.1-client.jar"
                .to_owned()
        ));
        assert!(strings.contains(
            &"/home/alice/.var/app/org.prismlauncher.PrismLauncher/data/PrismLauncher/libraries/com/mojang/minecraft/1.21.1/minecraft-1.21.1-client.jar"
                .to_owned()
        ));
        assert!(strings.contains(
            &"/home/alice/.local/share/ModrinthApp/meta/versions/1.21.1/1.21.1.jar".to_owned()
        ));
        assert!(strings.contains(&"/home/alice/.cache/megu/versions/1.21.1/1.21.1.jar".to_owned()));
    }

    #[test]
    fn linux_candidates_honor_xdg_roots() {
        let env = OsEnv {
            home: Some(pb("/home/alice")),
            xdg_data_home: Some(pb("/xdg/data")),
            xdg_cache_home: Some(pb("/xdg/cache")),
            ..OsEnv::default()
        };
        let out = candidate_vanilla_jar_paths_for_os(LauncherOs::Linux, &env, None, &ids());
        let strings: Vec<String> = out.iter().map(|p| p.display().to_string()).collect();
        assert!(strings.contains(
            &"/xdg/data/PrismLauncher/libraries/com/mojang/minecraft/1.21.1/minecraft-1.21.1-client.jar"
                .to_owned()
        ));
        assert!(strings.contains(
            &"/xdg/data/ModrinthApp/meta/versions/1.21.1/1.21.1.jar".to_owned()
        ));
        assert!(strings.contains(&"/xdg/cache/megu/versions/1.21.1/1.21.1.jar".to_owned()));
    }

    #[test]
    fn macos_candidates_cover_launcher_prism_modrinth_cache() {
        let env = OsEnv {
            home: Some(pb("/Users/alice")),
            ..OsEnv::default()
        };
        let out = candidate_vanilla_jar_paths_for_os(LauncherOs::MacOs, &env, None, &ids());
        let strings: Vec<String> = out.iter().map(|p| p.display().to_string()).collect();
        assert!(strings.contains(
            &"/Users/alice/Library/Application Support/minecraft/versions/1.21.1/1.21.1.jar"
                .to_owned()
        ));
        assert!(strings.contains(
            &"/Users/alice/Library/Application Support/PrismLauncher/libraries/com/mojang/minecraft/1.21.1/minecraft-1.21.1-client.jar"
                .to_owned()
        ));
        assert!(strings.contains(
            &"/Users/alice/Library/Application Support/ModrinthApp/meta/versions/1.21.1/1.21.1.jar"
                .to_owned()
        ));
        assert!(strings.contains(
            &"/Users/alice/Library/Caches/megu/versions/1.21.1/1.21.1.jar".to_owned()
        ));
    }

    #[test]
    fn windows_candidates_cover_appdata_prism_modrinth_localappdata() {
        let roaming = pb(r"C:\Users\alice\AppData\Roaming");
        let local = pb(r"C:\Users\alice\AppData\Local");
        let env = OsEnv {
            appdata: Some(roaming.clone()),
            localappdata: Some(local.clone()),
            userprofile: Some(pb(r"C:\Users\alice")),
            ..OsEnv::default()
        };
        let out = candidate_vanilla_jar_paths_for_os(LauncherOs::Windows, &env, None, &ids());
        // Path comparisons are component-wise, so these hold on every
        // host that runs the suite (the env roots are opaque strings).
        assert!(out.contains(&roaming.join(".minecraft/versions/1.21.1/1.21.1.jar")));
        assert!(out.contains(&roaming.join(
            "PrismLauncher/libraries/com/mojang/minecraft/1.21.1/minecraft-1.21.1-client.jar"
        )));
        assert!(out.contains(&roaming.join("ModrinthApp/meta/versions/1.21.1/1.21.1.jar")));
        assert!(out.contains(&local.join("megu/versions/1.21.1/1.21.1.jar")));
    }

    #[test]
    fn windows_candidates_fall_back_to_userprofile_when_appdata_unset() {
        let profile = pb(r"C:\Users\alice");
        let env = OsEnv {
            userprofile: Some(profile.clone()),
            ..OsEnv::default()
        };
        let out = candidate_vanilla_jar_paths_for_os(LauncherOs::Windows, &env, None, &ids());
        assert!(out.contains(&profile.join(
            "AppData/Roaming/.minecraft/versions/1.21.1/1.21.1.jar"
        )));
        assert!(out.contains(&profile.join("AppData/Local/megu/versions/1.21.1/1.21.1.jar")));
    }

    #[test]
    fn unsafe_version_ids_never_enter_candidate_paths() {
        let env = OsEnv {
            home: Some(pb("/home/alice")),
            ..OsEnv::default()
        };
        let out = candidate_vanilla_jar_paths_for_os(
            LauncherOs::Linux,
            &env,
            Some(&pb("/games/mc")),
            &[
                "..".to_owned(),
                "../escape".to_owned(),
                "a/b".to_owned(),
                String::new(),
            ],
        );
        assert!(out.is_empty(), "unsafe ids produced paths: {out:?}");
    }

    #[test]
    fn jar_candidate_requires_a_zip_directory_within_size_cap() {
        let dir = test_dir("candidate");
        let good = dir.join("good.jar");
        std::fs::write(&good, stored_zip(&[("assets/x/y", b"Y")])).unwrap();
        assert!(is_valid_jar_candidate(&good));
        let bad = dir.join("bad.jar");
        std::fs::write(&bad, b"not a zip").unwrap();
        assert!(!is_valid_jar_candidate(&bad));
        std::fs::write(&bad, b"PK\x03\x04rest").unwrap();
        assert!(!is_valid_jar_candidate(&bad));
        let empty = dir.join("empty.jar");
        std::fs::write(&empty, b"").unwrap();
        assert!(!is_valid_jar_candidate(&empty));
        assert!(!is_valid_jar_candidate(&dir.join("missing.jar")));
        assert!(!is_valid_jar_candidate(&dir)); // a directory, not a file
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn discover_local_finds_game_dir_jar_first() {
        let dir = test_dir("discover");
        let jar_dir = dir.join("versions").join("1.21.1");
        std::fs::create_dir_all(&jar_dir).unwrap();
        let jar = jar_dir.join("1.21.1.jar");
        std::fs::write(&jar, stored_zip(&[("assets/x/y", b"Y")])).unwrap();
        let paths = SessionPaths {
            game_dir: Some(dir.clone()),
            version_id: Some("1.21.1".to_owned()),
            ..SessionPaths::default()
        };
        assert_eq!(discover_local_vanilla_jar(&paths).as_deref(), Some(jar.as_path()));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    // -- SHA-1 -------------------------------------------------------------

    #[test]
    fn sha1_matches_published_fips_vectors() {
        assert_eq!(sha1_hex(b""), "da39a3ee5e6b4b0d3255bfef95601890afd80709");
        assert_eq!(sha1_hex(b"abc"), "a9993e364706816aba3e25717850c26c9cd0d89d");
        assert_eq!(
            sha1_hex(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"),
            "84983e441c3bd26ebaae4aa1f95129e5e54670f1"
        );
        // Multi-block stream boundary: 1,000,000 'a' exercised through
        // incremental updates of varying sizes.
        let input = vec![b'a'; 1_000_000];
        let mut hasher = Sha1::new();
        for chunk in input.chunks(63) {
            hasher.update(chunk);
        }
        assert_eq!(hasher.finalize_hex(), "34aa973cd4c4daa4f61eeb2bdbad27316534016f");
    }

    // -- URL allowlist -----------------------------------------------------

    #[test]
    fn mojang_https_hosts_allowed_everything_else_rejected() {
        assert!(url_allowed(VANILLA_MANIFEST_URL));
        assert!(url_allowed("https://piston-data.mojang.com/v1/objects/ab/cd/client.jar"));
        assert!(url_allowed("https://launchermeta.mojang.com/v1/packages/x/1.21.1.json"));
        assert!(url_allowed(
            "https://resources.download.minecraft.net/ab/cd"
        ));
        assert!(!url_allowed("http://piston-meta.mojang.com/mc/game/version_manifest_v2.json"));
        assert!(!url_allowed("https://piston-meta.mojang.com.evil.example/x"));
        assert!(!url_allowed(
            "https://resources.download.minecraft.net.evil.example/ab/cd"
        ));
        assert!(!url_allowed(
            "https://piston-meta.mojang.com@evil.example/mc/game/version_manifest_v2.json"
        ));
        assert!(!url_allowed("https://evil.example/piston-meta.mojang.com/"));
        assert!(!url_allowed("ftp://piston-meta.mojang.com/x"));
        assert!(!url_allowed(""));
        // Loopback is test-only (cfg(test) active here).
        assert!(url_allowed("http://127.0.0.1:8080/manifest.json"));
        assert!(url_allowed("http://localhost:8080/manifest.json"));
    }

    // -- manifest / version JSON parsing -----------------------------------

    const MANIFEST: &str = r#"{
        "latest": {"release": "1.21.1", "snapshot": "24w33a"},
        "versions": [
            {"id": "24w33a", "type": "snapshot", "url": "https://piston-meta.mojang.com/v1/packages/aaa/24w33a.json"},
            {"id": "1.21.1", "type": "release", "url": "https://piston-meta.mojang.com/v1/packages/bbb/1.21.1.json"},
            {"id": "1.20.4", "type": "release", "url": "https://piston-meta.mojang.com/v1/packages/ccc/1.20.4.json"}
        ]
    }"#;

    #[test]
    fn manifest_version_url_extracts_requested_release() {
        assert_eq!(
            manifest_version_url(MANIFEST, "1.21.1").unwrap(),
            "https://piston-meta.mojang.com/v1/packages/bbb/1.21.1.json"
        );
        assert_eq!(
            manifest_version_url(MANIFEST, "24w33a").unwrap(),
            "https://piston-meta.mojang.com/v1/packages/aaa/24w33a.json"
        );
    }

    #[test]
    fn manifest_version_url_fails_closed() {
        assert!(manifest_version_url(MANIFEST, "9.9.9").is_err());
        assert!(manifest_version_url("not json", "1.21.1").is_err());
        assert!(manifest_version_url("{}", "1.21.1").is_err());
        // A poisoned manifest redirecting off the allowlist is rejected.
        let evil = MANIFEST.replace(
            "https://piston-meta.mojang.com/v1/packages/bbb/1.21.1.json",
            "https://evil.example/1.21.1.json",
        );
        assert!(manifest_version_url(&evil, "1.21.1").is_err());
        let http = MANIFEST.replace(
            "https://piston-meta.mojang.com/v1/packages/bbb/1.21.1.json",
            "http://piston-meta.mojang.com/v1/packages/bbb/1.21.1.json",
        );
        assert!(manifest_version_url(&http, "1.21.1").is_err());
    }

    fn version_json(url: &str, sha1: &str, size: u64) -> String {
        format!(
            r#"{{"id":"1.21.1","downloads":{{"client":{{"sha1":"{sha1}","size":{size},"url":"{url}"}},"server":{{"sha1":"{}","size":1,"url":"{url}"}}}}}}"#,
            "0".repeat(40)
        )
    }

    #[test]
    fn client_download_info_extracts_and_validates() {
        let jar_url = "https://piston-data.mojang.com/v1/objects/aabbcc/client.jar";
        let info = client_download_info(&version_json(jar_url, &"a".repeat(40), 26_000_000))
            .unwrap();
        assert_eq!(info.url, jar_url);
        assert_eq!(info.sha1, "a".repeat(40));
        assert_eq!(info.size, 26_000_000);
    }

    #[test]
    fn client_download_info_fails_closed() {
        let jar_url = "https://piston-data.mojang.com/v1/objects/aabbcc/client.jar";
        // Wrong host.
        assert!(client_download_info(&version_json(
            "https://evil.example/client.jar",
            &"a".repeat(40),
            1_000
        ))
        .is_err());
        // Malformed sha1.
        assert!(client_download_info(&version_json(jar_url, "xyz", 1_000)).is_err());
        assert!(client_download_info(&version_json(jar_url, &"A".repeat(40), 1_000)).is_err());
        // Oversized / zero / missing size.
        assert!(client_download_info(&version_json(
            jar_url,
            &"a".repeat(40),
            MAX_CLIENT_JAR_BYTES + 1
        ))
        .is_err());
        assert!(client_download_info(&version_json(jar_url, &"a".repeat(40), 0)).is_err());
        assert!(client_download_info(r#"{"downloads":{"other":{}}}"#).is_err());
        assert!(client_download_info("{}").is_err());
    }

    // -- hermetic end-to-end ------------------------------------------------

    /// Minimal stored-ZIP writer (same shape vanilla_jar's tests carry).
    fn stored_zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut central = Vec::new();
        for (name, data) in entries {
            let lh_off = out.len() as u32;
            out.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
            out.extend_from_slice(&20u16.to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes());
            out.extend_from_slice(&0u32.to_le_bytes());
            out.extend_from_slice(&(data.len() as u32).to_le_bytes());
            out.extend_from_slice(&(data.len() as u32).to_le_bytes());
            out.extend_from_slice(&(name.len() as u16).to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes());
            out.extend_from_slice(name.as_bytes());
            out.extend_from_slice(data);

            central.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
            central.extend_from_slice(&20u16.to_le_bytes());
            central.extend_from_slice(&20u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u32.to_le_bytes());
            central.extend_from_slice(&(data.len() as u32).to_le_bytes());
            central.extend_from_slice(&(data.len() as u32).to_le_bytes());
            central.extend_from_slice(&(name.len() as u16).to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u32.to_le_bytes()); // external attributes
            central.extend_from_slice(&lh_off.to_le_bytes());
            central.extend_from_slice(name.as_bytes());
        }
        let cd_offset = out.len() as u32;
        out.extend_from_slice(&central);
        out.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
        out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
        out.extend_from_slice(&(central.len() as u32).to_le_bytes());
        out.extend_from_slice(&cd_offset.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out
    }

    /// One loopback HTTP/1.0 exchange per connection: read the request
    /// head, dispatch on the path, write a Content-Length response.
    /// `body_delay` stalls the jar body write so the client observably
    /// sits in `Downloading` for a deterministic window.
    fn serve_once(
        listener: &TcpListener,
        body_delay: Duration,
        respond: impl Fn(&str) -> (u16, Vec<u8>),
    ) {
        for stream in listener.incoming() {
            let mut stream = match stream {
                Ok(s) => s,
                Err(_) => return,
            };
            let mut head = Vec::new();
            let mut byte = [0u8; 1];
            loop {
                match stream.read(&mut byte) {
                    Ok(0) | Err(_) => return,
                    Ok(_) => {
                        head.push(byte[0]);
                        if head.ends_with(b"\r\n\r\n") {
                            break;
                        }
                        if head.len() > 16 * 1024 {
                            return;
                        }
                    }
                }
            }
            let request = String::from_utf8_lossy(&head);
            let path = request
                .split_whitespace()
                .nth(1)
                .unwrap_or("/")
                .to_owned();
            let (status, body) = respond(&path);
            let reason = if status == 200 { "OK" } else { "Not Found" };
            let response = format!(
                "HTTP/1.0 {status} {reason}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            if stream.write_all(response.as_bytes()).is_err() {
                return;
            }
            if !body_delay.is_zero() {
                std::thread::sleep(body_delay);
            }
            if stream.write_all(&body).is_err() {
                return;
            }
        }
    }

    /// Spin up a loopback server serving manifest -> version JSON -> JAR.
    /// Returns the manifest URL.
    fn mock_mojang_server(
        jar_bytes: Vec<u8>,
        served_sha1: String,
        jar_body_delay: Duration,
    ) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let port = listener.local_addr().unwrap().port();
        let base = format!("http://127.0.0.1:{port}");
        let manifest_url = format!("{base}/version_manifest_v2.json");
        let version_url = format!("{base}/1.21.1.json");
        let jar_url = format!("{base}/client.jar");
        let manifest = MANIFEST.replace(
            "https://piston-meta.mojang.com/v1/packages/bbb/1.21.1.json",
            &version_url,
        );
        let version = version_json(&jar_url, &served_sha1, jar_bytes.len() as u64);
        std::thread::spawn(move || {
            serve_once(&listener, jar_body_delay, move |path| match path {
                "/version_manifest_v2.json" => (200, manifest.clone().into_bytes()),
                "/1.21.1.json" => (200, version.clone().into_bytes()),
                "/client.jar" => (200, jar_bytes.clone()),
                _ => (404, b"nope".to_vec()),
            });
        });
        manifest_url
    }

    fn await_terminal(handle: &VanillaJarBootstrap) -> BootstrapProgress {
        for _ in 0..2_000 {
            let progress = handle.progress();
            if progress.is_terminal() {
                return progress;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        panic!("bootstrap did not reach a terminal state")
    }

    #[test]
    fn bootstrap_downloads_verifies_and_installs_jar() {
        let jar_bytes = stored_zip(&[
            (
                "assets/minecraft/textures/block/stone.png",
                b"PNG".as_slice(),
            ),
            ("META-INF/MANIFEST.MF", b"MF".as_slice()),
        ]);
        // The jar body is delayed so the client observably parks in
        // `Downloading` — the transition sequence is part of the contract.
        let manifest_url = mock_mojang_server(
            jar_bytes.clone(),
            sha1_hex(&jar_bytes),
            Duration::from_millis(300),
        );
        let dest = test_dir("e2e-ok");
        let handle =
            spawn_vanilla_jar_bootstrap_with_manifest("1.21.1", &dest, &manifest_url).unwrap();
        let mut saw_fetching = false;
        let mut saw_downloading = false;
        let ready = loop {
            match handle.progress() {
                BootstrapProgress::FetchingManifest => saw_fetching = true,
                BootstrapProgress::Downloading { .. } => saw_downloading = true,
                BootstrapProgress::Verifying => {}
                terminal => break terminal,
            }
            std::thread::sleep(Duration::from_millis(2));
        };
        match ready {
            BootstrapProgress::Ready(jar) => {
                let expected = dest.join("versions/1.21.1/1.21.1.jar");
                assert_eq!(jar, expected);
                assert!(expected.is_file());
                assert!(!dest.join("versions/1.21.1/1.21.1.jar.part").exists());
                assert_eq!(std::fs::read(&expected).unwrap(), jar_bytes);
                assert!(is_valid_jar_candidate(&expected));
                // Discovery now resolves the downloaded JAR.
                let paths = SessionPaths {
                    game_dir: Some(dest.clone()),
                    version_id: Some("1.21.1".to_owned()),
                    ..SessionPaths::default()
                };
                assert_eq!(
                    discover_local_vanilla_jar(&paths).as_deref(),
                    Some(expected.as_path())
                );
            }
            other => panic!("bootstrap failed: {other:?}"),
        }
        assert!(saw_fetching || saw_downloading, "no in-flight progress observed");
        assert!(saw_downloading, "Downloading state never observed");
        std::fs::remove_dir_all(&dest).unwrap();
    }

    #[test]
    fn bootstrap_sha1_mismatch_fails_closed_and_cleans_part() {
        let jar_bytes = stored_zip(&[("assets/x/y.png", b"Y".as_slice())]);
        let manifest_url = mock_mojang_server(jar_bytes, "0".repeat(40), Duration::ZERO);
        let dest = test_dir("e2e-bad-sha");
        let handle =
            spawn_vanilla_jar_bootstrap_with_manifest("1.21.1", &dest, &manifest_url).unwrap();
        match await_terminal(&handle) {
            BootstrapProgress::Failed(reason) => {
                assert!(reason.contains("SHA-1"), "unexpected failure: {reason}");
            }
            other => panic!("expected Failed, got {other:?}"),
        }
        let version_dir = dest.join("versions/1.21.1");
        assert!(!version_dir.join("1.21.1.jar").exists());
        assert!(!version_dir.join("1.21.1.jar.part").exists());
        std::fs::remove_dir_all(&dest).unwrap();
    }

    #[test]
    fn bootstrap_reports_manifest_failures_cleanly() {
        // Loopback server that 404s every path: the failure is a clean
        // status reason, not a hang or panic.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            serve_once(&listener, Duration::ZERO, |_| (404, b"nope".to_vec()));
        });
        let dest = test_dir("e2e-404");
        let handle = spawn_vanilla_jar_bootstrap_with_manifest(
            "1.21.1",
            &dest,
            &format!("http://127.0.0.1:{port}/manifest.json"),
        )
        .unwrap();
        match await_terminal(&handle) {
            BootstrapProgress::Failed(_) => {}
            other => panic!("expected Failed, got {other:?}"),
        }
        std::fs::remove_dir_all(&dest).unwrap();
    }

    #[test]
    fn spawn_rejects_unsafe_inputs() {
        let dest = test_dir("spawn-guard");
        assert!(spawn_vanilla_jar_bootstrap("../x", &dest).is_none());
        assert!(spawn_vanilla_jar_bootstrap("", &dest).is_none());
        assert!(spawn_vanilla_jar_bootstrap_with_manifest(
            "1.21.1",
            &dest,
            "https://evil.example/manifest.json"
        )
        .is_none());
        std::fs::remove_dir_all(&dest).unwrap();
    }

    #[test]
    fn progress_labels_track_the_download() {
        assert_eq!(
            bootstrap_progress_label("1.21.1", &BootstrapProgress::FetchingManifest).unwrap(),
            "Fetching Minecraft 1.21.1 download manifest..."
        );
        let line = bootstrap_progress_label(
            "1.21.1",
            &BootstrapProgress::Downloading {
                downloaded: 11 * 1024 * 1024,
                total: Some(26 * 1024 * 1024),
            },
        )
        .unwrap();
        assert!(line.contains("Downloading Minecraft 1.21.1 assets"), "{line}");
        assert!(line.contains('%'), "{line}");
        assert!(line.contains("MB"), "{line}");
        assert_eq!(
            bootstrap_progress_label("1.21.1", &BootstrapProgress::Verifying).unwrap(),
            "Verifying Minecraft 1.21.1 assets..."
        );
        assert!(bootstrap_progress_label("1.21.1", &BootstrapProgress::Ready(pb("/x"))).is_none());
        assert!(
            bootstrap_progress_label("1.21.1", &BootstrapProgress::Failed("x".to_owned())).is_none()
        );
    }
}
