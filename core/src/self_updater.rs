// SPDX-License-Identifier: LGPL-3.0-or-later
//! Atomic self-updater and instant-rollback engine shared by `megu update`
//! and the client UI's one-click upgrade path.
//!
//! The module is deliberately split into pure, testable halves:
//!
//! * **Selection & verification (no I/O):** [`SemVer`] implements strict
//!   SemVer 2.0 precedence including prerelease ordering
//!   (`alpha.3 < alpha.10 < rc.1 < 0.1.0`); [`UpdateChannel`] keeps
//!   prereleases opt-in; [`parse_release_list`] reads the GitHub Releases
//!   API response with [`crate::json`]; [`select_update`] maps a
//!   [`TargetPlatform`] + [`UpdateComponent`] onto the exact artifact names
//!   `tools/release_bundle.py` and `.github/workflows/release.yml` publish
//!   (`megu-<tag>-<target>.tar.gz`, `megu-installer-<target>[.exe]`,
//!   `SHA256SUMS-all.txt`); [`is_trusted_download_url`] fail-closes every
//!   URL outside the pinned `nekomario28/megu-mc` HTTPS prefixes; and
//!   [`parse_sha256sums`] / [`verify_bytes_sha256`] /
//!   [`verify_file_sha256`] pin each download to its expected SHA-256 via
//!   [`crate::crypto`] before anything on disk is touched.
//! * **Atomic mutation (I/O):** [`AtomicBinarySwap`] performs the
//!   running-binary-safe `bin → bin.megu-prev`, `bin.megu-new → bin` rename
//!   dance with automatic restore on failure and O(1) [`rollback`];
//!   [`AtomicBundleSwap`] extends the same transaction to a whole bin/
//!   directory plus launcher JSON pin files so `megu-client-gpu`, the
//!   in-game `megu` server and every `-Dmegu.clientSha256=` pin move
//!   all-or-nothing; [`sync_launcher_sha256_pin`] /
//!   [`rollback_launcher_sha256_pin`] keep Official Launcher
//!   (`versions/<id>/<id>.json` `arguments.jvm`) and PrismLauncher
//!   (`patches/*.json` `+jvmArgs`) pins in lock-step with the swapped
//!   client binary; and [`mc_version_compatible`] blocks silent in-place
//!   upgrades across a Minecraft version boundary.
//!
//! [`extract_tar_gz_member`] completes the end-to-end flow in pure `std`:
//! gzip header parse → [`crate::zip_archive::inflate_raw_rfc1951_with_consumed`]
//! → CRC32/ISIZE trailer check → USTAR member scan, so a verified archive
//!   yields its `megu-<tag>-<target>/<binary>` payload with zero external
//!   dependencies.
#![forbid(unsafe_code)]

use std::cmp::Ordering;
use std::fmt;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use crate::crypto::{sha256_file, sha256_hex};
use crate::hash::crc32_ieee;
use crate::json::{self, JsonError, JsonValue};
use crate::zip_archive;

/// Canonical GitHub coordinates for Megu release metadata and assets.
pub const GITHUB_REPO: &str = "nekomario28/megu-mc";
/// Releases API endpoint the updater polls.
pub const RELEASES_API_URL: &str = "https://api.github.com/repos/nekomario28/megu-mc/releases";
/// Download URL prefixes that are allowed to serve update bytes; anything
/// else is refused before a single byte is fetched.
pub const TRUSTED_URL_PREFIXES: &[&str] = &[
    "https://api.github.com/repos/nekomario28/megu-mc/",
    "https://github.com/nekomario28/megu-mc/releases/download/",
    "https://objects.githubusercontent.com/",
];
/// Checksum asset names, in priority order. The release workflow publishes
/// `SHA256SUMS-all.txt`; the bare names are accepted for per-target sums
/// produced by `tools/release_bundle.py`.
pub const CHECKSUM_ASSET_NAMES: &[&str] = &["SHA256SUMS-all.txt", "SHA256SUMS.txt", "SHA256SUMS"];
/// JVM property whose value the Official Launcher and PrismLauncher pin the
/// native client binary's SHA-256 to (`server-files/src/installer.rs`).
pub const CLIENT_SHA256_PROPERTY: &str = "-Dmegu.clientSha256=";
/// File suffix of the staged replacement written next to the live file.
pub const STAGED_SUFFIX: &str = ".megu-new";
/// File suffix of the previous version kept for O(1) rollback.
pub const PREVIOUS_SUFFIX: &str = ".megu-prev";
/// File suffix a replaced binary is parked under during rollback diagnosis.
pub const FAILED_SUFFIX: &str = ".megu-failed";

/// Hard ceiling on the decompressed tarball size so a hostile archive can
/// never exhaust memory (256 MiB is ~10x the real bundle).
pub const MAX_TAR_BYTES: usize = 256 * 1024 * 1024;
/// Hard ceiling on one extracted member (~10x the largest release binary).
pub const MAX_MEMBER_BYTES: usize = 64 * 1024 * 1024;

// ================================ errors ====================================

/// Everything the updater can fail with. Variants are deliberately
/// descriptive: an unattended updater must never guess.
#[derive(Debug)]
pub enum UpdateError {
    /// Filesystem failure (read/write/rename/sync).
    Io(io::Error),
    /// The release list or a launcher JSON was malformed.
    Json(JsonError),
    /// A version string was not SemVer 2.0.
    MalformedSemVer(String),
    /// A `SHA256SUMS` line was not `<64-hex> <name>` / `<64-hex> *<name>`.
    MalformedSums { line: usize, detail: String },
    /// `SHA256SUMS` had no entry for the requested artifact.
    MissingChecksumEntry(String),
    /// Downloaded bytes did not match the pinned digest — nothing was staged.
    ChecksumMismatch {
        name: String,
        expected: String,
        actual: String,
    },
    /// A release pointed at a URL outside the HTTPS allowlist.
    UntrustedUrl(String),
    /// No published asset carries this platform/component name.
    NoMatchingAsset { release: String, asset: String },
    /// The launcher JSON contains no `-Dmegu.clientSha256=` pin.
    PinNotFound(String),
    /// The launcher pin exists but holds a digest other than the expected
    /// one — the file is left untouched rather than guessed at.
    PinMismatch {
        path: String,
        expected: String,
        found: String,
    },
    /// Rollback was requested but no `.megu-prev` backup exists.
    NoPreviousVersion(PathBuf),
    /// In-place update would cross a Minecraft version boundary.
    CrossMcVersion {
        installed: String,
        candidate: String,
    },
    /// A download or staged file was empty.
    EmptyPayload(String),
    /// A `.tar.gz` member could not be located or decoded.
    Archive(String),
}

impl fmt::Display for UpdateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            UpdateError::Io(err) => write!(f, "I/O error: {err}"),
            UpdateError::Json(err) => write!(f, "{err}"),
            UpdateError::MalformedSemVer(v) => write!(f, "not a SemVer 2.0 version: {v:?}"),
            UpdateError::MalformedSums { line, detail } => {
                write!(f, "SHA256SUMS line {line} is malformed: {detail}")
            }
            UpdateError::MissingChecksumEntry(name) => {
                write!(f, "SHA256SUMS has no entry for {name:?}")
            }
            UpdateError::ChecksumMismatch {
                name,
                expected,
                actual,
            } => write!(
                f,
                "SHA-256 mismatch for {name:?}: expected {expected}, got {actual} \
                 (download rejected, nothing was staged)"
            ),
            UpdateError::UntrustedUrl(url) => {
                write!(f, "download URL outside the trusted allowlist: {url:?}")
            }
            UpdateError::NoMatchingAsset { release, asset } => {
                write!(f, "release {release} has no asset named {asset:?}")
            }
            UpdateError::PinNotFound(path) => {
                write!(f, "{path}: no {CLIENT_SHA256_PROPERTY}<sha256> pin found")
            }
            UpdateError::PinMismatch {
                path,
                expected,
                found,
            } => write!(
                f,
                "{path}: {CLIENT_SHA256_PROPERTY} pin is {found}, expected \
                 {expected}; refusing to rewrite"
            ),
            UpdateError::NoPreviousVersion(path) => {
                write!(f, "no .megu-prev backup for {}", path.display())
            }
            UpdateError::CrossMcVersion {
                installed,
                candidate,
            } => write!(
                f,
                "in-place update would cross a Minecraft version boundary \
                 (installed {installed} -> {candidate}); perform a fresh \
                 profile install or pass --force"
            ),
            UpdateError::EmptyPayload(what) => write!(f, "{what} is empty"),
            UpdateError::Archive(detail) => write!(f, "release archive: {detail}"),
        }
    }
}

impl std::error::Error for UpdateError {}

impl From<io::Error> for UpdateError {
    fn from(err: io::Error) -> Self {
        UpdateError::Io(err)
    }
}

impl From<JsonError> for UpdateError {
    fn from(err: JsonError) -> Self {
        UpdateError::Json(err)
    }
}

/// Convenience result alias for updater operations.
pub type UpdateResult<T> = Result<T, UpdateError>;

// ================================ SemVer ====================================

/// One dot-separated prerelease identifier (SemVer 2.0 §9).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PreIdent {
    /// Purely numeric identifier; compares numerically and below any
    /// alphanumeric identifier.
    Numeric(u64),
    /// Identifier containing at least one non-digit; ASCII ordering.
    Alphanumeric(String),
}

impl Ord for PreIdent {
    fn cmp(&self, other: &Self) -> Ordering {
        match (self, other) {
            (PreIdent::Numeric(a), PreIdent::Numeric(b)) => a.cmp(b),
            // SemVer §11.4.3: numeric identifiers sort below alphanumeric.
            (PreIdent::Numeric(_), PreIdent::Alphanumeric(_)) => Ordering::Less,
            (PreIdent::Alphanumeric(_), PreIdent::Numeric(_)) => Ordering::Greater,
            (PreIdent::Alphanumeric(a), PreIdent::Alphanumeric(b)) => a.cmp(b),
        }
    }
}

impl PartialOrd for PreIdent {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// A strict SemVer 2.0 version with an optional leading `v` tolerated on
/// input (the release tags are `v<semver>`). Build metadata (`+…`) is
/// accepted and discarded, per §10 it never affects precedence.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SemVer {
    /// Major component.
    pub major: u64,
    /// Minor component.
    pub minor: u64,
    /// Patch component.
    pub patch: u64,
    /// Prerelease identifiers; empty means a stable release.
    pub prerelease: Vec<PreIdent>,
}

impl SemVer {
    /// Parse `v0.1.0-alpha.4`, `0.1.0-rc.1` or `0.1.0`; `+build` suffixes are
    /// validated and dropped. Fails closed on any malformed input.
    pub fn parse(input: &str) -> UpdateResult<Self> {
        let fail = || UpdateError::MalformedSemVer(input.to_owned());
        let s = input.strip_prefix('v').unwrap_or(input);
        if s.is_empty() {
            return Err(fail());
        }
        let (core_and_pre, _build) = match s.split_once('+') {
            Some((head, build)) => {
                // §10: build metadata is dot-separated [0-9A-Za-z-]+ idents.
                if build.is_empty()
                    || !build.split('.').all(|ident| {
                        !ident.is_empty()
                            && ident
                                .bytes()
                                .all(|b| b.is_ascii_alphanumeric() || b == b'-')
                    })
                {
                    return Err(fail());
                }
                (head, build)
            }
            None => (s, ""),
        };
        let (core, pre) = match core_and_pre.split_once('-') {
            Some((core, pre)) => (core, Some(pre)),
            None => (core_and_pre, None),
        };
        let mut nums = core.split('.');
        let (major, minor, patch) = (
            nums.next().ok_or_else(fail)?,
            nums.next().ok_or_else(fail)?,
            nums.next().ok_or_else(fail)?,
        );
        if nums.next().is_some() {
            return Err(fail());
        }
        let parse_num = |raw: &str| -> UpdateResult<u64> {
            if raw.is_empty()
                || !raw.bytes().all(|b| b.is_ascii_digit())
                || (raw.len() > 1 && raw.starts_with('0'))
            {
                return Err(fail());
            }
            raw.parse::<u64>().map_err(|_| fail())
        };
        let mut prerelease = Vec::new();
        if let Some(pre) = pre {
            if pre.is_empty() {
                return Err(fail());
            }
            for ident in pre.split('.') {
                if ident.is_empty()
                    || !ident
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'-')
                {
                    return Err(fail());
                }
                let all_digits = ident.bytes().all(|b| b.is_ascii_digit());
                if all_digits {
                    // §9: numeric identifiers must not carry leading zeros.
                    if ident.len() > 1 && ident.starts_with('0') {
                        return Err(fail());
                    }
                    prerelease.push(PreIdent::Numeric(ident.parse::<u64>().map_err(|_| fail())?));
                } else {
                    prerelease.push(PreIdent::Alphanumeric(ident.to_owned()));
                }
            }
        }
        Ok(Self {
            major: parse_num(major)?,
            minor: parse_num(minor)?,
            patch: parse_num(patch)?,
            prerelease,
        })
    }

    /// `true` when the version carries a prerelease tag (`-alpha.4`, `-rc.1`).
    pub fn is_prerelease(&self) -> bool {
        !self.prerelease.is_empty()
    }
}

impl fmt::Display for SemVer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)?;
        if !self.prerelease.is_empty() {
            f.write_str("-")?;
            for (i, ident) in self.prerelease.iter().enumerate() {
                if i > 0 {
                    f.write_str(".")?;
                }
                match ident {
                    PreIdent::Numeric(n) => write!(f, "{n}")?,
                    PreIdent::Alphanumeric(s) => f.write_str(s)?,
                }
            }
        }
        Ok(())
    }
}

impl Ord for SemVer {
    fn cmp(&self, other: &Self) -> Ordering {
        (self.major, self.minor, self.patch)
            .cmp(&(other.major, other.minor, other.patch))
            .then_with(
                || match (self.prerelease.is_empty(), other.prerelease.is_empty()) {
                    // §11.4.1: a release outranks every prerelease of itself.
                    (true, true) => Ordering::Equal,
                    (true, false) => Ordering::Greater,
                    (false, true) => Ordering::Less,
                    (false, false) => {
                        // §11.4.4: shorter prerelease list wins when it is a
                        // strict prefix of the longer one.
                        self.prerelease.iter().cmp(other.prerelease.iter())
                    }
                },
            )
    }
}

impl PartialOrd for SemVer {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

// ============================= update channel ===============================

/// Which release stream the user consents to. Nothing is ever installed or
/// overwritten without an explicit `megu update` / UI confirmation; the
/// channel only steers which releases are *offered*.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UpdateChannel {
    /// Stable releases only — unless the installed build is itself a
    /// prerelease, in which case the user is already on the alpha train and
    /// newer prereleases stay eligible.
    Stable,
    /// Every release, including `-alpha`/`-beta`/`-rc` builds.
    Prerelease,
}

impl UpdateChannel {
    /// The channel that actually applies for an install on `current`.
    pub fn effective(self, current: &SemVer) -> UpdateChannel {
        match self {
            UpdateChannel::Stable if current.is_prerelease() => UpdateChannel::Prerelease,
            other => other,
        }
    }

    /// Whether a release at `candidate` may be offered to `current`.
    pub fn permits(self, current: &SemVer, candidate: &SemVer) -> bool {
        match self.effective(current) {
            UpdateChannel::Prerelease => true,
            UpdateChannel::Stable => !candidate.is_prerelease(),
        }
    }
}

// ============================ target platform ===============================

/// Operating system family of a release artifact.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReleaseOs {
    /// Linux (glibc or musl, see [`TargetPlatform::prefer_musl`]).
    Linux,
    /// FreeBSD 14+ (server artifacts only).
    FreeBsd,
    /// macOS 11+.
    Macos,
    /// Windows 10+.
    Windows,
}

/// CPU architecture of a release artifact.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReleaseArch {
    /// `x86_64` / `amd64`.
    X86_64,
    /// `aarch64` / `arm64`.
    Arm64,
    /// `riscv64` / `rv64gc`.
    Riscv64,
}

/// Which Megu component a release artifact supplies.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UpdateComponent {
    /// `megu` — the dedicated/integrated server.
    Server,
    /// `megu-client-gpu` — the GUI game client.
    Client,
    /// `megu-installer` — the standalone first-run installer, published both
    /// inside every archive and as `megu-installer-<target>[.exe]`.
    Installer,
}

impl UpdateComponent {
    /// Basename of the shipped binary (no `.exe` suffix).
    pub fn binary_stem(self) -> &'static str {
        match self {
            UpdateComponent::Server => "megu",
            UpdateComponent::Client => "megu-client-gpu",
            UpdateComponent::Installer => "megu-installer",
        }
    }
}

/// The `(os, arch, musl)` triple a host maps onto release artifact names —
/// mirroring `host_release_target_for` in `megu_installer.rs` and the
/// `release.yml` build matrix exactly.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TargetPlatform {
    /// Operating system family.
    pub os: ReleaseOs,
    /// CPU architecture.
    pub arch: ReleaseArch,
    /// Prefer the statically-linked `-musl` archive on Linux (Alpine/Void).
    pub prefer_musl: bool,
}

impl TargetPlatform {
    /// The platform this process was built for (`cfg!(target_env="musl")`
    /// detects musl hosts exactly like `megu-installer` does), or `None`
    /// when Megu ships no release artifact for this OS/arch — unknown
    /// hosts must never be handed a foreign binary.
    pub fn host() -> Option<Self> {
        let os = match std::env::consts::OS {
            "linux" => ReleaseOs::Linux,
            "macos" => ReleaseOs::Macos,
            "windows" => ReleaseOs::Windows,
            "freebsd" => ReleaseOs::FreeBsd,
            _ => return None,
        };
        let arch = match std::env::consts::ARCH {
            "x86_64" => ReleaseArch::X86_64,
            "aarch64" => ReleaseArch::Arm64,
            "riscv64" => ReleaseArch::Riscv64,
            _ => return None,
        };
        Some(Self {
            os,
            arch,
            prefer_musl: cfg!(target_env = "musl"),
        })
    }

    /// The release `<target>` slug (`linux-x86_64-musl`, `macos-arm64`, …)
    /// for `component`, or `None` when the release matrix does not ship one.
    /// The slim release matrix splits Linux musl into server-only
    /// `*-musl` archives and dynamically linked GPU-client-only
    /// `*-musl-client` archives; FreeBSD and RISC-V ship server/installer
    /// artifacts only. Unsupported combinations fail closed rather than
    /// silently installing a foreign binary.
    pub fn release_target(self, component: UpdateComponent) -> Option<&'static str> {
        let musl = self.prefer_musl;
        match (self.os, self.arch, component) {
            (ReleaseOs::Linux, ReleaseArch::X86_64, UpdateComponent::Client) if musl => {
                Some("linux-x86_64-musl-client")
            }
            (ReleaseOs::Linux, ReleaseArch::Arm64, UpdateComponent::Client) if musl => {
                Some("linux-arm64-musl-client")
            }
            (ReleaseOs::Linux, ReleaseArch::X86_64, _) if musl => Some("linux-x86_64-musl"),
            (ReleaseOs::Linux, ReleaseArch::Arm64, _) if musl => Some("linux-arm64-musl"),
            (ReleaseOs::Linux, ReleaseArch::X86_64, _) => Some("linux-x86_64"),
            (ReleaseOs::Linux, ReleaseArch::Arm64, _) => Some("linux-arm64"),
            (ReleaseOs::Linux, ReleaseArch::Riscv64, _) if musl => None,
            (ReleaseOs::Linux, ReleaseArch::Riscv64, UpdateComponent::Client) => None,
            (ReleaseOs::Linux, ReleaseArch::Riscv64, _) => Some("linux-riscv64"),
            (ReleaseOs::FreeBsd, ReleaseArch::X86_64, UpdateComponent::Client) => None,
            (ReleaseOs::FreeBsd, ReleaseArch::X86_64, _) => Some("freebsd-x86_64"),
            (ReleaseOs::Macos, ReleaseArch::X86_64, _) => Some("macos-x86_64"),
            (ReleaseOs::Macos, ReleaseArch::Arm64, _) => Some("macos-arm64"),
            (ReleaseOs::Windows, ReleaseArch::X86_64, _) => Some("windows-x86_64"),
            (ReleaseOs::Windows, ReleaseArch::Arm64, _) => Some("windows-arm64"),
            _ => None,
        }
    }

    /// Executable suffix on this platform (`.exe` on Windows, else empty).
    pub fn exe_suffix(self) -> &'static str {
        match self.os {
            ReleaseOs::Windows => ".exe",
            _ => "",
        }
    }

    /// Published asset filename for `component` at `tag`
    /// (`megu-v0.1.0-alpha.4-linux-x86_64.tar.gz`,
    /// `megu-installer-windows-x86_64.exe`). `None` when the matrix ships no
    /// such asset.
    pub fn asset_name(self, component: UpdateComponent, tag: &str) -> Option<String> {
        let target = self.release_target(component)?;
        Some(match component {
            UpdateComponent::Installer => {
                format!("megu-installer-{target}{}", self.exe_suffix())
            }
            _ => format!("megu-{tag}-{target}.tar.gz"),
        })
    }

    /// The binary's basename inside the archive / on disk.
    pub fn binary_name(self, component: UpdateComponent) -> String {
        format!("{}{}", component.binary_stem(), self.exe_suffix())
    }

    /// Tar member path of `component`'s binary inside its archive:
    /// `megu-<tag>-<target>/<binary>`. `None` for the standalone installer
    /// download, which is not an archive.
    pub fn archive_member(self, component: UpdateComponent, tag: &str) -> Option<String> {
        if component == UpdateComponent::Installer {
            return None;
        }
        let target = self.release_target(component)?;
        Some(format!(
            "megu-{tag}-{target}/{}",
            self.binary_name(component)
        ))
    }

    /// Name of the aggregated release-manifest asset
    /// (`megu-<tag>-release-manifest.json`) when this component has a target.
    pub fn manifest_name(self, component: UpdateComponent, tag: &str) -> Option<String> {
        self.release_target(component)?;
        Some(format!("megu-{tag}-release-manifest.json"))
    }
}

// ============================ release metadata ==============================

/// One downloadable asset of a GitHub release.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReleaseAsset {
    /// Asset basename (`megu-v0.1.0-alpha.4-linux-x86_64.tar.gz`).
    pub name: String,
    /// `browser_download_url`; must satisfy [`is_trusted_download_url`].
    pub download_url: String,
}

/// One parsed GitHub Releases API entry.
#[derive(Clone, Debug)]
pub struct ReleaseDescriptor {
    /// Raw tag (`v0.1.0-alpha.4`).
    pub tag: String,
    /// `tag` parsed as [`SemVer`].
    pub version: SemVer,
    /// `draft` flag; missing values default to `true` (fail closed).
    pub draft: bool,
    /// `prerelease` flag, defaulting to `version.is_prerelease()`.
    pub prerelease: bool,
    /// RFC 3339 `published_at`, when present.
    pub published_at: Option<String>,
    /// Downloadable assets.
    pub assets: Vec<ReleaseAsset>,
}

/// Parse the `/repos/nekomario28/megu-mc/releases` JSON array.
///
/// Entries whose `tag_name` is not SemVer are skipped (they can never be
/// newer than the running build); a missing `draft` is treated as `true`
/// and missing `assets`/`name`/`browser_download_url` fields drop only the
/// malformed asset, never the whole document. A non-array top level or
/// malformed JSON fails closed.
pub fn parse_release_list(text: &str) -> UpdateResult<Vec<ReleaseDescriptor>> {
    let value = json::parse_json(text)?;
    let entries = value
        .as_array()
        .ok_or_else(|| UpdateError::Archive("release list is not a JSON array".to_owned()))?;
    let mut releases = Vec::with_capacity(entries.len());
    for entry in entries {
        let Some(tag) = entry.get_str("tag_name") else {
            continue;
        };
        let Ok(version) = SemVer::parse(tag) else {
            continue;
        };
        let draft = entry
            .get("draft")
            .and_then(JsonValue::as_bool)
            .unwrap_or(true);
        let prerelease = entry
            .get("prerelease")
            .and_then(JsonValue::as_bool)
            .unwrap_or_else(|| version.is_prerelease());
        let mut assets = Vec::new();
        if let Some(list) = entry.get("assets").and_then(JsonValue::as_array) {
            for asset in list {
                let (Some(name), Some(url)) =
                    (asset.get_str("name"), asset.get_str("browser_download_url"))
                else {
                    continue;
                };
                assets.push(ReleaseAsset {
                    name: name.to_owned(),
                    download_url: url.to_owned(),
                });
            }
        }
        releases.push(ReleaseDescriptor {
            tag: tag.to_owned(),
            version,
            draft,
            prerelease,
            published_at: entry.get_str("published_at").map(str::to_owned),
            assets,
        });
    }
    Ok(releases)
}

/// `true` only when `url` sits under one of [`TRUSTED_URL_PREFIXES`] and
/// carries no userinfo/whitespace tricks. Everything else — `http://`,
/// look-alike hosts, `…@evil` credentials — is refused.
pub fn is_trusted_download_url(url: &str) -> bool {
    if url.is_empty() || url.len() > 4096 {
        return false;
    }
    if url.bytes().any(|b| b <= 0x20 || b == 0x7f) || url.contains('@') {
        return false;
    }
    TRUSTED_URL_PREFIXES
        .iter()
        .any(|p| url.len() > p.len() && url.starts_with(p))
}

/// A fully-resolved, user-presentable update decision — everything the CLI
/// (`megu update --check`) or client UI needs to display "v0.1.0-alpha.4
/// available" and to download exactly the right verified bytes.
#[derive(Clone, Debug)]
pub struct UpdatePlan {
    /// Currently installed version.
    pub current_version: SemVer,
    /// Version the plan upgrades to.
    pub version: SemVer,
    /// Release tag as published (`v0.1.0-alpha.4`).
    pub tag: String,
    /// Channel the selection ran under.
    pub channel: UpdateChannel,
    /// Component being updated.
    pub component: UpdateComponent,
    /// Release `<target>` slug used for the asset.
    pub release_target: String,
    /// Artifact to download (archive or standalone installer name).
    pub asset_name: String,
    /// Trusted `browser_download_url` for [`UpdatePlan::asset_name`].
    pub asset_url: String,
    /// Checksums asset (`SHA256SUMS-all.txt`).
    pub checksums_name: String,
    /// Trusted `browser_download_url` for the checksums asset.
    pub checksums_url: String,
    /// `megu-<tag>-release-manifest.json` when the release ships one — the
    /// machine-readable source of every target's `mc_targets`, file digests
    /// and modes.
    pub manifest_name: Option<String>,
    /// Trusted URL of [`UpdatePlan::manifest_name`], when present.
    pub manifest_url: Option<String>,
    /// Binary basename inside the archive (or the installer file itself).
    pub binary_name: String,
    /// Tar member path for archive components; empty for the standalone
    /// installer download.
    pub archive_member: String,
    /// Minecraft version the new release targets, when already known —
    /// otherwise the CLI fills it from `manifest_url` before calling
    /// [`mc_version_compatible`].
    pub mc_version: Option<String>,
}

impl UpdatePlan {
    /// One-line human summary for `megu update --check` / UI badges.
    pub fn summary(&self) -> String {
        format!(
            "v{} -> {} ({}, {})",
            self.current_version, self.tag, self.release_target, self.binary_name
        )
    }

    /// Guard this plan against an installed profile pinned to `installed_mc`
    /// (see [`mc_version_compatible`]); a plan with unknown `mc_version`
    /// passes only when `installed_mc` is the empty string.
    pub fn check_mc_compat(&self, installed_mc: &str, force: bool) -> UpdateResult<()> {
        let candidate = self.mc_version.as_deref().unwrap_or(installed_mc);
        mc_version_compatible(installed_mc, candidate, force)
    }
}

/// Pick the newest installable release for `(channel, platform, component)`.
///
/// A release is installable when it is not a draft, its version exceeds
/// `current`, the channel permits it, the platform asset exists alongside a
/// checksums asset, and every URL used is inside the allowlist. `Ok(None)`
/// means "already up to date" — never an error.
pub fn select_update(
    releases: &[ReleaseDescriptor],
    current: &SemVer,
    channel: UpdateChannel,
    platform: TargetPlatform,
    component: UpdateComponent,
) -> UpdateResult<Option<UpdatePlan>> {
    let Some(target) = platform.release_target(component) else {
        return Ok(None);
    };
    let mut best: Option<&ReleaseDescriptor> = None;
    for release in releases {
        if release.draft || release.version <= *current {
            continue;
        }
        // A full release manually flagged prerelease on GitHub is still a
        // prerelease for channel purposes — combine both signals.
        let candidate_is_pre = release.prerelease || release.version.is_prerelease();
        let permitted = match channel.effective(current) {
            UpdateChannel::Prerelease => true,
            UpdateChannel::Stable => !candidate_is_pre,
        };
        if !permitted {
            continue;
        }
        if best.is_none_or(|b| release.version > b.version) {
            best = Some(release);
        }
    }
    let Some(release) = best else {
        return Ok(None);
    };
    let Some(asset_name) = platform.asset_name(component, &release.tag) else {
        return Ok(None);
    };
    let find = |name: &str| release.assets.iter().find(|a| a.name == name);
    let asset = find(&asset_name).ok_or_else(|| UpdateError::NoMatchingAsset {
        release: release.tag.clone(),
        asset: asset_name.clone(),
    })?;
    let checksums = CHECKSUM_ASSET_NAMES
        .iter()
        .find_map(|name| find(name).map(|a| (*name, a)))
        .ok_or_else(|| UpdateError::NoMatchingAsset {
            release: release.tag.clone(),
            asset: CHECKSUM_ASSET_NAMES[0].to_owned(),
        })?;
    for url in [&asset.download_url, &checksums.1.download_url] {
        if !is_trusted_download_url(url) {
            return Err(UpdateError::UntrustedUrl(url.clone()));
        }
    }
    let manifest = platform
        .manifest_name(component, &release.tag)
        .and_then(|name| find(&name).map(|a| (name, a.download_url.clone())));
    if let Some((_, url)) = &manifest {
        if !is_trusted_download_url(url) {
            return Err(UpdateError::UntrustedUrl(url.clone()));
        }
    }
    Ok(Some(UpdatePlan {
        current_version: current.clone(),
        version: release.version.clone(),
        tag: release.tag.clone(),
        channel,
        component,
        release_target: target.to_owned(),
        asset_name,
        asset_url: asset.download_url.clone(),
        checksums_name: checksums.0.to_owned(),
        checksums_url: checksums.1.download_url.clone(),
        manifest_name: manifest.as_ref().map(|(n, _)| n.clone()),
        manifest_url: manifest.map(|(_, u)| u),
        binary_name: platform.binary_name(component),
        archive_member: platform
            .archive_member(component, &release.tag)
            .unwrap_or_default(),
        mc_version: None,
    }))
}

// =========================== SHA256SUMS handling ============================

/// `true` for exactly 64 lowercase-or-uppercase hex digits.
pub fn is_sha256_hex(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Validate + lowercase a 64-hex digest.
pub fn normalize_sha256_hex(s: &str) -> UpdateResult<String> {
    if is_sha256_hex(s) {
        Ok(s.to_ascii_lowercase())
    } else {
        Err(UpdateError::MalformedSums {
            line: 0,
            detail: format!("expected 64 hex digits, got {s:?}"),
        })
    }
}

/// Parse `sha256sum`-format text (`<64-hex><space>[ *]<name>` — the exact
/// shape `tools/release_bundle.py` emits) into sorted `(name, sha256)`
/// pairs. Any non-blank malformed line or duplicated name fails closed.
pub fn parse_sha256sums(text: &str) -> UpdateResult<Vec<(String, String)>> {
    let mut out: Vec<(String, String)> = Vec::new();
    for (index, raw) in text.lines().enumerate() {
        let line = raw.trim_end();
        if line.is_empty() {
            continue;
        }
        let bad = |detail: &str| UpdateError::MalformedSums {
            line: index + 1,
            detail: detail.to_owned(),
        };
        if line.len() < 66 || !line[..64].bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(bad("expected a 64-hex digest"));
        }
        // `<hex> <sep><name>`: one space, then either a second space (text
        // mode) or `*` (binary mode), matching `[0-9a-f]{64} [ *]\S+`.
        let rest = &line[64..];
        let name = match rest.as_bytes() {
            [b' ', b' ', ..] => &rest[2..],
            [b' ', b'*', ..] => &rest[2..],
            _ => return Err(bad("expected `<hex>  <name>` or `<hex> *<name>`")),
        };
        if name.is_empty() || name.bytes().any(|b| b <= 0x20) {
            return Err(bad("empty or whitespace-containing filename"));
        }
        let digest = line[..64].to_ascii_lowercase();
        if out.iter().any(|(n, _)| n == name) {
            return Err(bad("duplicate filename entry"));
        }
        out.push((name.to_owned(), digest));
    }
    Ok(out)
}

/// The digest `sums` pins for `name` — a missing entry is an error, never a
/// skipped check.
pub fn expected_sha256(sums: &[(String, String)], name: &str) -> UpdateResult<String> {
    sums.iter()
        .find(|(n, _)| n == name)
        .map(|(_, d)| d.clone())
        .ok_or_else(|| UpdateError::MissingChecksumEntry(name.to_owned()))
}

/// Verify in-memory `data` against the `SHA256SUMS` entry for `name`.
/// Runs before any staging so a single flipped bit aborts with zero
/// filesystem mutation.
pub fn verify_bytes_sha256(data: &[u8], sums_text: &str, name: &str) -> UpdateResult<()> {
    if data.is_empty() {
        return Err(UpdateError::EmptyPayload(name.to_owned()));
    }
    let expected = expected_sha256(&parse_sha256sums(sums_text)?, name)?;
    let actual = sha256_hex(data);
    if actual == expected {
        Ok(())
    } else {
        Err(UpdateError::ChecksumMismatch {
            name: name.to_owned(),
            expected,
            actual,
        })
    }
}

/// Verify a file on disk against its `SHA256SUMS` entry. Zero-length files
/// are rejected even if a degenerate sums line lists the empty-digest.
pub fn verify_file_sha256(path: &Path, sums_text: &str, name: &str) -> UpdateResult<()> {
    let meta = fs::metadata(path)?;
    if meta.len() == 0 {
        return Err(UpdateError::EmptyPayload(path.display().to_string()));
    }
    let expected = expected_sha256(&parse_sha256sums(sums_text)?, name)?;
    let actual = sha256_file(path)?;
    if actual == expected {
        Ok(())
    } else {
        Err(UpdateError::ChecksumMismatch {
            name: name.to_owned(),
            expected,
            actual,
        })
    }
}

// ============================ tar.gz extraction =============================

/// Extract `member_path` (e.g. `megu-v0.1.0-alpha.4-linux-x86_64/megu`)
/// from a verified `.tar.gz` — gzip header parse, pure-std inflate via
/// [`crate::zip_archive`], CRC32 + ISIZE trailer check, then a strict USTAR
/// scan. The archive bytes must already have passed
/// [`verify_bytes_sha256`]; this layer still bounds output and validates
/// framing so a hostile tarball cannot decompress-bomb or smuggle a
/// differently-named member.
pub fn extract_tar_gz_member(
    archive: &[u8],
    member_path: &str,
    max_member_bytes: usize,
    max_tar_bytes: usize,
) -> UpdateResult<Vec<u8>> {
    let tar = gunzip_single_member(archive, max_tar_bytes)?;
    read_ustar_member(&tar, member_path, max_member_bytes)
}

/// Decode the single gzip member at the head of `archive`, verifying the
/// CRC32/ISIZE trailer and rejecting any non-padding trailing bytes.
fn gunzip_single_member(archive: &[u8], max_out: usize) -> UpdateResult<Vec<u8>> {
    let bad = |d: &str| UpdateError::Archive(format!("gzip: {d}"));
    if archive.len() < 18 || archive[0] != 0x1f || archive[1] != 0x8b {
        return Err(bad("missing 1f 8b magic"));
    }
    if archive[2] != 8 {
        return Err(bad("compression method is not deflate"));
    }
    let flg = archive[3];
    let mut pos = 10usize; // magic(2) CM(1) FLG(1) MTIME(4) XFL(1) OS(1)
    if flg & 0x04 != 0 {
        // FEXTRA: little-endian length-prefixed field.
        let xlen = archive
            .get(pos..pos + 2)
            .map(|b| u16::from_le_bytes([b[0], b[1]]) as usize)
            .ok_or_else(|| bad("truncated FEXTRA"))?;
        pos = pos
            .checked_add(2 + xlen)
            .ok_or_else(|| bad("FEXTRA overflow"))?;
        if pos > archive.len() {
            return Err(bad("FEXTRA overruns input"));
        }
    }
    for flag in [0x08u8, 0x10] {
        // FNAME / FCOMMENT: NUL-terminated Latin-1 strings.
        if flg & flag != 0 {
            let end = archive[pos..]
                .iter()
                .position(|&b| b == 0)
                .map(|i| pos + i + 1)
                .ok_or_else(|| bad("unterminated FNAME/FCOMMENT"))?;
            pos = end;
        }
    }
    if flg & 0x02 != 0 {
        // FHCRC: two bytes we may skip (CRC32 below covers everything).
        pos = pos.checked_add(2).ok_or_else(|| bad("FHCRC overflow"))?;
    }
    if pos >= archive.len() {
        return Err(bad("header overruns input"));
    }
    let (out, consumed) = zip_archive::inflate_raw_rfc1951_with_consumed(&archive[pos..], max_out)
        .map_err(|e| UpdateError::Archive(format!("deflate: {e}")))?;
    let trailer = pos + consumed;
    let crc = archive
        .get(trailer..trailer + 4)
        .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        .ok_or_else(|| bad("missing CRC32 trailer"))?;
    let isize_field = archive
        .get(trailer + 4..trailer + 8)
        .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        .ok_or_else(|| bad("missing ISIZE trailer"))?;
    if crc32_ieee(&out) != crc {
        return Err(bad("CRC32 trailer mismatch"));
    }
    if out.len() as u64 % (1u64 << 32) != u64::from(isize_field) {
        return Err(bad("ISIZE trailer mismatch"));
    }
    // Zero padding after the member is tolerated (tools emit it); anything
    // else — including a smuggled second member — is refused.
    if archive[trailer + 8..].iter().any(|&b| b != 0) {
        return Err(bad("trailing bytes after gzip member"));
    }
    Ok(out)
}

/// Strict USTAR scan for `member_path`. Header magic, octal size field and
/// 512-block stepping are all checked; every member is visited in order so
/// a `member_path` match can only return the file's real bytes.
fn read_ustar_member(
    tar: &[u8],
    member_path: &str,
    max_member_bytes: usize,
) -> UpdateResult<Vec<u8>> {
    let bad = |d: &str| UpdateError::Archive(format!("tar: {d}"));
    let mut pos = 0usize;
    while pos + 512 <= tar.len() {
        let header = &tar[pos..pos + 512];
        if header.iter().all(|&b| b == 0) {
            break; // End-of-archive zero block.
        }
        let size = parse_octal(&header[124..136]).ok_or_else(|| bad("bad size field"))?;
        let nul = |raw: &[u8]| {
            let end = raw.iter().position(|&b| b == 0).unwrap_or(raw.len());
            String::from_utf8_lossy(&raw[..end]).into_owned()
        };
        let mut name = nul(&header[0..100]);
        let prefix = nul(&header[345..500]);
        if !prefix.is_empty() {
            name = format!("{prefix}/{name}");
        }
        pos += 512;
        let size_usize = usize::try_from(size).map_err(|_| bad("member size overflow"))?;
        let end = pos
            .checked_add(size_usize)
            .ok_or_else(|| bad("size overflow"))?;
        if end > tar.len() {
            return Err(bad("member overruns archive"));
        }
        let typeflag = header[156];
        if (typeflag == b'0' || typeflag == 0) && name == member_path {
            if size_usize > max_member_bytes {
                return Err(bad("member exceeds size cap"));
            }
            return Ok(tar[pos..end].to_vec());
        }
        // Step over the data region rounded up to whole 512-byte blocks.
        pos = pos
            .checked_add(size_usize.div_ceil(512) * 512)
            .ok_or_else(|| bad("offset overflow"))?;
    }
    Err(UpdateError::Archive(format!(
        "member {member_path:?} not in archive"
    )))
}

/// POSIX octal field: digits up to a space or NUL terminator.
fn parse_octal(field: &[u8]) -> Option<u64> {
    let mut value = 0u64;
    let mut seen = false;
    for &b in field {
        match b {
            b'0'..=b'7' => {
                seen = true;
                value = value.checked_mul(8)?.checked_add(u64::from(b - b'0'))?;
            }
            b' ' | 0 if seen => break,
            b' ' | 0 => continue,
            _ => return None,
        }
    }
    seen.then_some(value)
}

// ==================== atomic file replacement machinery =====================

/// `<bin>.megu-new` — the staged, already-verified replacement.
pub fn staged_path(path: &Path) -> PathBuf {
    sibling_with_suffix(path, STAGED_SUFFIX)
}

/// `<bin>.megu-prev` — the previous version kept for O(1) rollback.
pub fn previous_path(path: &Path) -> PathBuf {
    sibling_with_suffix(path, PREVIOUS_SUFFIX)
}

/// `<bin>.megu-failed` — the binary being replaced during rollback, kept
/// for diagnosis instead of deleted.
pub fn failed_path(path: &Path) -> PathBuf {
    sibling_with_suffix(path, FAILED_SUFFIX)
}

fn sibling_with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(suffix);
    PathBuf::from(name)
}

/// `fsync` the directory holding `path` so rename decisions survive a
/// crash. Unix-only, best-effort: Windows directories cannot be opened
/// this way, and a failed hint never undoes a completed rename.
fn sync_parent_dir(path: &Path) {
    #[cfg(unix)]
    {
        if let Some(dir) = path.parent() {
            let _ = fs::File::open(dir).and_then(|d| d.sync_all());
        }
    }
    let _ = path;
}

/// Write `bytes` to `staged`, `fsync` them, and set the file mode on Unix:
/// the live file at `live`'s mode is preserved when it exists (with exec
/// bits force-added for binaries so an executable is never replaced by an
/// unrunnable file); fresh installs fall back to 0o755 / 0o644.
fn write_staged(staged: &Path, live: &Path, bytes: &[u8], executable: bool) -> UpdateResult<()> {
    let mut file = fs::File::create(staged)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let fallback = if executable { 0o755 } else { 0o644 };
        let mode = match fs::metadata(live)
            .ok()
            .map(|m| m.permissions().mode() & 0o777)
            .filter(|m| *m != 0)
        {
            Some(live_mode) => {
                if executable {
                    live_mode | 0o111
                } else {
                    live_mode
                }
            }
            None => fallback,
        };
        fs::set_permissions(staged, fs::Permissions::from_mode(mode))?;
    }
    let _ = live;
    Ok(())
}

/// Atomically place a staged `.megu-new` at `path`, parking the incumbent
/// at `.megu-prev` first. On any failure the incumbent is restored and the
/// staged file removed — the directory is never left without a working
/// target. Renames work on running executables on every supported OS
/// (Windows allows renaming an in-use image within its directory).
fn swap_staged_into_place(path: &Path) -> UpdateResult<()> {
    let staged = staged_path(path);
    let prev = previous_path(path);
    let had_incumbent = path.exists();
    if had_incumbent {
        if prev.exists() {
            // A stale backup from an earlier update; it is superseded now
            // and Windows rename refuses to overwrite it.
            fs::remove_file(&prev)?;
        }
        fs::rename(path, &prev).map_err(|e| {
            let _ = fs::remove_file(&staged);
            UpdateError::Io(e)
        })?;
    }
    if let Err(err) = fs::rename(&staged, path) {
        if had_incumbent {
            // Best-effort restore of the previous binary so the install is
            // never left without a working target.
            let _ = fs::rename(&prev, path);
        } else {
            let _ = fs::remove_file(path);
        }
        let _ = fs::remove_file(&staged);
        return Err(UpdateError::Io(err));
    }
    sync_parent_dir(path);
    Ok(())
}

/// Undo a swap at `path`: the current file is parked at `.megu-failed` for
/// diagnosis and `.megu-prev` atomically becomes the target again.
fn rollback_one(path: &Path) -> UpdateResult<()> {
    let prev = previous_path(path);
    if !prev.exists() {
        return Err(UpdateError::NoPreviousVersion(path.to_path_buf()));
    }
    let failed = failed_path(path);
    if failed.exists() {
        fs::remove_file(&failed)?;
    }
    if path.exists() {
        fs::rename(path, &failed)?;
    }
    if let Err(err) = fs::rename(&prev, path) {
        let _ = fs::rename(&failed, path);
        return Err(UpdateError::Io(err));
    }
    sync_parent_dir(path);
    Ok(())
}

/// Bundle-apply unwind: restore `path` from `.megu-prev` when there was an
/// incumbent, or remove the freshly-installed file when the swap created a
/// brand-new target (nothing to restore to).
fn unwind_swap(path: &Path) {
    if previous_path(path).exists() {
        let _ = rollback_one(path);
    } else if path.exists() {
        let failed = failed_path(path);
        let _ = fs::remove_file(&failed);
        let _ = fs::rename(path, failed);
    }
}

/// Install `new_bytes` at `path` through the stage→prev→swap sequence after
/// checking them against `expected_sha256` (pass `Some` from
/// [`expected_sha256`] / the pinned sums entry, `None` for content that was
/// verified by other means — launcher JSON pins are integrity-covered by
/// the exact old-pin match). `executable` forces exec bits on Unix.
fn atomic_install(
    path: &Path,
    new_bytes: &[u8],
    expected_sha256: Option<&str>,
    executable: bool,
) -> UpdateResult<()> {
    if new_bytes.is_empty() {
        return Err(UpdateError::EmptyPayload(path.display().to_string()));
    }
    if let Some(expected) = expected_sha256 {
        let expected = normalize_sha256_hex(expected)?;
        let actual = sha256_hex(new_bytes);
        if actual != expected {
            return Err(UpdateError::ChecksumMismatch {
                name: path.display().to_string(),
                expected,
                actual,
            });
        }
    }
    write_staged(&staged_path(path), path, new_bytes, executable)?;
    swap_staged_into_place(path)
}

/// Single-binary atomic replacement with O(1) rollback — the unit
/// `megu update` uses for the `megu` server binary.
pub struct AtomicBinarySwap;

impl AtomicBinarySwap {
    /// Verify `new_bytes` against `expected_sha256` **before** touching the
    /// filesystem, then stage at `<bin>.megu-new` (fsync + exec bits),
    /// move `bin → bin.megu-prev`, and atomically move `.megu-new → bin`.
    /// If the final rename fails the previous binary is restored
    /// automatically — a working executable is never missing.
    pub fn install(bin_path: &Path, new_bytes: &[u8], expected_sha256: &str) -> UpdateResult<()> {
        atomic_install(bin_path, new_bytes, Some(expected_sha256), true)
    }

    /// Same atomic swap for content already verified upstream (for example
    /// bytes extracted from an archive whose SHA-256 was checked).
    pub fn install_verified(bin_path: &Path, new_bytes: &[u8]) -> UpdateResult<()> {
        atomic_install(bin_path, new_bytes, None, true)
    }

    /// `true` when `<bin>.megu-prev` exists and [`rollback`] can run.
    pub fn has_rollback(bin_path: &Path) -> bool {
        previous_path(bin_path).is_file()
    }

    /// Restore `<bin>.megu-prev` → `bin`, parking the broken incumbent at
    /// `<bin>.megu-failed` for diagnosis.
    pub fn rollback(bin_path: &Path) -> UpdateResult<()> {
        rollback_one(bin_path)
    }

    /// Drop stale `.megu-new` / `.megu-failed` remnants next to `bin` — run
    /// once at boot so an interrupted update cannot accumulate garbage.
    /// `.megu-prev` is kept: it *is* the rollback path.
    pub fn cleanup_stale_files(bin_path: &Path) -> UpdateResult<()> {
        for path in [staged_path(bin_path), failed_path(bin_path)] {
            match fs::remove_file(&path) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(UpdateError::Io(e)),
            }
        }
        Ok(())
    }
}

/// Free-function alias for [`AtomicBinarySwap::rollback`] matching the
/// `megu update --rollback` mental model.
pub fn rollback(bin_path: &Path) -> UpdateResult<()> {
    AtomicBinarySwap::rollback(bin_path)
}

/// Free-function alias for [`AtomicBinarySwap::cleanup_stale_files`].
pub fn cleanup_stale_files(bin_path: &Path) -> UpdateResult<()> {
    AtomicBinarySwap::cleanup_stale_files(bin_path)
}

// ====================== launcher pin synchronisation ========================

/// Outcome of [`sync_launcher_sha256_pin`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PinSyncOutcome {
    /// The pin was found, matched `expected_sha256`, and was atomically
    /// replaced — `<file>.megu-prev` holds the pre-edit bytes.
    Updated,
    /// The file is valid JSON but carries no `-Dmegu.clientSha256=` pin;
    /// nothing was written.
    NotPinned,
}

/// Rewrite every `-Dmegu.clientSha256=<sha>` token in `text`, requiring
/// each existing pin to equal `expected_sha256` (case-insensitive) —
/// a profile whose pin drifted is reported, never silently re-pinned.
/// Returns `(new_text, occurrences)`; `occurrences == 0` leaves `text`
/// unchanged and signals [`PinSyncOutcome::NotPinned`].
///
/// The check is textual rather than AST-based so file formatting survives
/// byte-for-byte; callers additionally require the file to parse as JSON
/// before applying the rewrite.
pub fn rewrite_client_sha256_pin(
    text: &str,
    expected_sha256: &str,
    new_sha256: &str,
) -> UpdateResult<(String, usize)> {
    let expected = normalize_sha256_hex(expected_sha256)?;
    let new = normalize_sha256_hex(new_sha256)?;
    if !text.contains(CLIENT_SHA256_PROPERTY) {
        return Ok((text.to_owned(), 0));
    }
    // Every pin occurrence must hold exactly the expected digest, and each
    // is rewritten in place by span so a differently-cased stored pin (or
    // any formatting around it) is normalised rather than missed.
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    let mut count = 0usize;
    while let Some(at) = rest.find(CLIENT_SHA256_PROPERTY) {
        let value_start = at + CLIENT_SHA256_PROPERTY.len();
        let value_end = rest[value_start..]
            .find('"')
            .map(|i| value_start + i)
            .unwrap_or(rest.len());
        let found = rest[value_start..value_end].trim_end();
        if !found.eq_ignore_ascii_case(&expected) {
            return Err(UpdateError::PinMismatch {
                path: String::new(),
                expected: expected.clone(),
                found: found.to_owned(),
            });
        }
        out.push_str(&rest[..value_start]);
        out.push_str(&new);
        rest = &rest[value_end..];
        count += 1;
    }
    out.push_str(rest);
    Ok((out, count))
}

/// Extract the Minecraft version a launcher JSON is pinned to:
/// `inheritsFrom` (Official version JSON and Megu's Prism patch) or the
/// `net.minecraft` entry of `requires`/`components` (Prism `mmc-pack`).
/// `None` when the file carries no MC binding.
pub fn launcher_mc_version(text: &str) -> Option<String> {
    let value = json::parse_json(text).ok()?;
    if let Some(id) = value.get_str("inheritsFrom") {
        return Some(id.to_owned());
    }
    for key in ["requires", "components"] {
        if let Some(list) = value.get(key).and_then(JsonValue::as_array) {
            for entry in list {
                if entry.get_str("uid") == Some("net.minecraft") {
                    if let Some(v) = entry.get_str("version").or_else(|| entry.get_str("equals")) {
                        return Some(v.to_owned());
                    }
                }
            }
        }
    }
    None
}

/// Atomically retarget a launcher JSON's `-Dmegu.clientSha256=` pin.
///
/// `json_path` may be an Official Launcher version file
/// (`versions/megu-1.21.1/megu-1.21.1.json`, `arguments.jvm`) or a Prism
/// patch (`patches/*.json`, `+jvmArgs`). The file must parse as JSON; when
/// the pin is present and equals `expected_sha256` the rewrite is staged
/// to `.megu-new`, fsynced, and swapped in with the incumbent parked at
/// `.megu-prev` — so [`rollback_launcher_sha256_pin`] restores byte-exact
/// pre-edit state. A mismatch fails closed with [`UpdateError::PinMismatch`]
/// and zero writes.
pub fn sync_launcher_sha256_pin(
    json_path: &Path,
    expected_sha256: &str,
    new_sha256: &str,
) -> UpdateResult<PinSyncOutcome> {
    let text = fs::read_to_string(json_path)?;
    json::parse_json(&text)?;
    let (new_text, count) = match rewrite_client_sha256_pin(&text, expected_sha256, new_sha256) {
        Ok(pair) => pair,
        Err(UpdateError::PinMismatch {
            expected, found, ..
        }) => {
            return Err(UpdateError::PinMismatch {
                path: json_path.display().to_string(),
                expected,
                found,
            })
        }
        Err(e) => return Err(e),
    };
    if count == 0 {
        return Ok(PinSyncOutcome::NotPinned);
    }
    atomic_install(json_path, new_text.as_bytes(), None, false)?;
    Ok(PinSyncOutcome::Updated)
}

/// Restore a launcher JSON from its `<file>.megu-prev` — the companion to
/// [`sync_launcher_sha256_pin`] so the pin can never end up pointing at a
/// binary that is no longer on disk.
pub fn rollback_launcher_sha256_pin(json_path: &Path) -> UpdateResult<()> {
    rollback_one(json_path)
}

// ======================== multi-binary bundle swaps =========================

/// One file the bundle transaction will replace: `path` gets `bytes`.
struct BundleEntry {
    path: PathBuf,
    bytes: Vec<u8>,
    executable: bool,
}

/// What [`AtomicBundleSwap::apply`] committed — the data
/// [`AtomicBundleSwap::rollback`] needs to undo it exactly.
#[derive(Clone, Debug)]
pub struct BundleReceipt {
    /// Every swapped target path, in apply order.
    pub paths: Vec<PathBuf>,
}

/// All-or-nothing swap of a whole `bin/` set plus launcher pin files.
///
/// `megu-client-gpu` and the integrated `megu` server share a directory
/// with the launcher JSONs that pin their digests; upgrading one without
/// the others desyncs the Official Launcher / Prism profile. Add every
/// binary and every `-Dmegu.clientSha256=` pin file, then [`Self::apply`]:
/// all `.megu-new` stages happen first, then each `target → .megu-prev` +
/// `.megu-new → target` pair, and if *any* step fails every already-swapped
/// file is restored from its `.megu-prev` before the error returns.
#[derive(Default)]
pub struct AtomicBundleSwap {
    entries: Vec<BundleEntry>,
    /// `(path, expected_old, new)` launcher pin rewrites resolved at apply.
    pins: Vec<(PathBuf, String, String)>,
}

impl AtomicBundleSwap {
    /// An empty bundle transaction.
    pub fn new() -> Self {
        Self::default()
    }

    /// Queue a replacement binary (Unix exec bits applied).
    pub fn add_binary(&mut self, path: PathBuf, verified_bytes: Vec<u8>) {
        self.entries.push(BundleEntry {
            path,
            bytes: verified_bytes,
            executable: true,
        });
    }

    /// Queue a replacement non-executable file (e.g. a rewritten manifest).
    pub fn add_file(&mut self, path: PathBuf, verified_bytes: Vec<u8>) {
        self.entries.push(BundleEntry {
            path,
            bytes: verified_bytes,
            executable: false,
        });
    }

    /// Queue a `-Dmegu.clientSha256=` retarget for a launcher JSON — the
    /// file is read and validated when [`Self::apply`] resolves, inside the
    /// transaction, so a pin drift aborts before anything is staged.
    pub fn add_launcher_pin(
        &mut self,
        json_path: PathBuf,
        expected_sha256: &str,
        new_sha256: &str,
    ) {
        self.pins
            .push((json_path, expected_sha256.to_owned(), new_sha256.to_owned()));
    }

    /// Stage and swap everything, all-or-nothing.
    pub fn apply(self) -> UpdateResult<BundleReceipt> {
        // Phase 0: resolve pin files into concrete byte entries before any
        // write — a drifted or unparseable pin aborts the whole bundle.
        let mut entries = self.entries;
        for (path, expected, new) in &self.pins {
            let text = fs::read_to_string(path)?;
            json::parse_json(&text)?;
            let (new_text, count) =
                rewrite_client_sha256_pin(&text, expected, new).map_err(|e| match e {
                    UpdateError::PinMismatch {
                        expected, found, ..
                    } => UpdateError::PinMismatch {
                        path: path.display().to_string(),
                        expected,
                        found,
                    },
                    other => other,
                })?;
            if count == 0 {
                return Err(UpdateError::PinNotFound(path.display().to_string()));
            }
            entries.push(BundleEntry {
                path: path.clone(),
                bytes: new_text.into_bytes(),
                executable: false,
            });
        }
        if entries.is_empty() {
            return Err(UpdateError::EmptyPayload(
                "bundle swap member list".to_owned(),
            ));
        }
        for entry in &entries {
            if entry.bytes.is_empty() {
                return Err(UpdateError::EmptyPayload(entry.path.display().to_string()));
            }
            if entries.iter().filter(|e| e.path == entry.path).count() != 1 {
                return Err(UpdateError::Archive(format!(
                    "{} listed twice in bundle",
                    entry.path.display()
                )));
            }
        }
        // Phase 1: stage every `.megu-new`; on failure drop what was made.
        let mut staged: Vec<PathBuf> = Vec::with_capacity(entries.len());
        for entry in &entries {
            let stage = staged_path(&entry.path);
            if let Err(e) = write_staged(&stage, &entry.path, &entry.bytes, entry.executable) {
                for path in &staged {
                    let _ = fs::remove_file(path);
                }
                return Err(e);
            }
            staged.push(stage);
        }
        // Phase 2: swap each target; unwind everything on the first failure.
        for (index, entry) in entries.iter().enumerate() {
            if let Err(e) = swap_staged_into_place(&entry.path) {
                for prior in entries[..index].iter().rev() {
                    unwind_swap(&prior.path);
                }
                for entry in &entries {
                    let _ = fs::remove_file(staged_path(&entry.path));
                }
                return Err(e);
            }
        }
        for entry in &entries {
            sync_parent_dir(&entry.path);
        }
        Ok(BundleReceipt {
            paths: entries.iter().map(|e| e.path.clone()).collect(),
        })
    }

    /// Undo an applied bundle: every swapped path rolls back from its
    /// `.megu-prev` in reverse order, binaries and JSON pins alike.
    pub fn rollback(receipt: &BundleReceipt) -> UpdateResult<()> {
        for path in receipt.paths.iter().rev() {
            if previous_path(path).exists() {
                rollback_one(path)?;
            } else if path.exists() {
                let failed = failed_path(path);
                if failed.exists() {
                    fs::remove_file(&failed)?;
                }
                fs::rename(path, &failed)?;
            }
        }
        Ok(())
    }

    /// [`AtomicBinarySwap::cleanup_stale_files`] for every receipt path.
    pub fn cleanup(receipt: &BundleReceipt) -> UpdateResult<()> {
        for path in &receipt.paths {
            AtomicBinarySwap::cleanup_stale_files(path)?;
        }
        Ok(())
    }
}

// ============================ MC version guard ==============================

/// In-place updates may never cross a Minecraft version boundary: a profile
/// pinned `inheritsFrom: "1.21.1"` whose launcher JSONs and mods are 1.21.1
/// would break if `megu` silently became a 1.21.4 build. Same-MC updates
/// (`0.1.0-alpha.3 -> 0.1.0-alpha.4` on `1.21.1`) pass; a different
/// `candidate` requires the operator's explicit `force`.
pub fn mc_version_compatible(
    installed_mc: &str,
    candidate_mc: &str,
    force: bool,
) -> UpdateResult<()> {
    if installed_mc == candidate_mc || force {
        Ok(())
    } else {
        Err(UpdateError::CrossMcVersion {
            installed: installed_mc.to_owned(),
            candidate: candidate_mc.to_owned(),
        })
    }
}
