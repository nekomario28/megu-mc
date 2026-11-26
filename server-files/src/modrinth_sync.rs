// SPDX-License-Identifier: LGPL-3.0-or-later
#![forbid(unsafe_code)]
//! Modrinth browser + `.mrpack` + server-push mod sync, zero redistribution.
//!
//! This module owns every type megu needs to browse Modrinth and exchange
//! mod sets without ever re-hosting a third-party binary:
//!
//! * **Zero mod redistribution.** [`ServerModSyncManifest`] and
//!   [`MrpackIndex`] carry Modrinth project/version IDs, SHA-256/SHA-512
//!   hashes and download URLs — never jar/wasm bytes. Every download URL is
//!   proven against the official CDN allowlist
//!   ([`validate_download_url`]): only `https://cdn.modrinth.com/...`
//!   passes, everything else fails closed.
//! * **Modrinth API ToS.** [`required_user_agent`] /
//!   [`is_compliant_user_agent`] enforce the mandatory identifying
//!   `User-Agent` (generic strings like `curl/…` are rejected),
//!   [`RateLimitState`] honors `X-Ratelimit-Remaining` /
//!   `X-Ratelimit-Reset` with a local response cache and exponential
//!   backoff, and [`ModrinthSearchHit`] preserves the `author`,
//!   `license_id` and `project_url` attribution every result must carry.
//! * **Standard `.mrpack`.** [`MrpackIndex`] is a strict parser/writer for
//!   `modrinth.index.json` schema v1 (`formatVersion`, `game: "minecraft"`,
//!   `versionId`, `name`, `dependencies`, `files[]` with `path`,
//!   `hashes.{sha1,sha512}`, `env.{client,server}`, `downloads`,
//!   `fileSize`). [`validate_mrpack_path`] fails closed on absolute paths,
//!   `..` traversal, NUL bytes, backslashes *and* a Mojang/secret purity
//!   filter (`*.jar` under `overrides/`, `config_packets.bin`,
//!   `datapacks/vanilla/**`, `world_overlay*.bin*`,
//!   `launcher_accounts.json`, `accounts.json`, `*.token`).
//! * **Server-push sync.** [`diff_installed_vs_server`] turns an installed
//!   set plus a [`ServerModSyncManifest`] into a [`ModSyncPlan`]
//!   (`to_download`/`to_enable`/`to_disable`/`untrusted_rejected`), where
//!   `requires_user_consent` is always set while downloads are pending.
//!   [`resolve_dependencies`] is the deterministic topological resolver
//!   with missing-required, incompatible and duplicate detection, and
//!   [`verify_file_sha256`] / [`verify_bytes_sha256`] gate every byte
//!   before it may be committed into the CAS/`mods` directory.

use crate::installer::{normalize_sha256_hex, sha256_file, sha256_hex};
use crate::json::{parse, Json, JsonError};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::io;
use std::path::Path;

// ---------------------------------------------------------------- ToS: identity

/// The only host megu downloads mod files from — Modrinth's official CDN.
pub const MODRINTH_CDN_HOST: &str = "cdn.modrinth.com";
/// Modrinth's public API host (metadata only, never binaries).
pub const MODRINTH_API_HOST: &str = "api.modrinth.com";
/// Modrinth's web host, the base of every project attribution link.
pub const MODRINTH_WEB_HOST: &str = "modrinth.com";
/// The identifying repository URL the ToS `User-Agent` must carry.
pub const MEGU_REPO_URL: &str = "https://github.com/nekomario28/megu-mc";
/// The `User-Agent` product token prefix: `nekomario28/megu-mc/<version>`.
pub const USER_AGENT_PREFIX: &str = "nekomario28/megu-mc";

/// The mandatory identifying `User-Agent` for Modrinth API traffic:
/// `nekomario28/megu-mc/<version> (https://github.com/nekomario28/megu-mc)`.
///
/// `version` must be a non-empty token without whitespace; an empty or
/// whitespace-bearing version still produces a string, but
/// [`is_compliant_user_agent`] will reject it, so callers should pass the
/// real crate/release version.
pub fn required_user_agent(version: &str) -> String {
    format!("{USER_AGENT_PREFIX}/{version} ({MEGU_REPO_URL})")
}

/// The `User-Agent` this crate sends, pinned to `megu-server-files`' own
/// version (the crate that actually speaks to Modrinth).
pub fn crate_user_agent() -> String {
    required_user_agent(env!("CARGO_PKG_VERSION"))
}

/// Whether `ua` is a compliant Modrinth `User-Agent`: exactly
/// `nekomario28/megu-mc/<version> (https://github.com/nekomario28/megu-mc)`
/// with a non-empty, whitespace-free version. Generic strings (`curl/…`,
/// `reqwest/…`, `megu-mc`, `Java/…`) all fail closed — the ToS requires an
/// identifying agent, and silently sending a generic one risks a ban.
pub fn is_compliant_user_agent(ua: &str) -> bool {
    let suffix = format!(" ({MEGU_REPO_URL})");
    let Some(rest) = ua.strip_suffix(&suffix) else {
        return false;
    };
    let Some(version) = rest.strip_prefix(&format!("{USER_AGENT_PREFIX}/")) else {
        return false;
    };
    !version.is_empty()
        && !version
            .bytes()
            .any(|b| b.is_ascii_whitespace() || b == b'(' || b == b')')
}

/// `https://modrinth.com/mod/<slug>` — the attribution link every browser
/// result must carry. `None` when `slug` is not a Modrinth slug
/// (`[A-Za-z0-9_-]+`, non-empty).
pub fn project_page_url(slug: &str) -> Option<String> {
    let valid = !slug.is_empty()
        && slug
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    valid.then(|| format!("https://{MODRINTH_WEB_HOST}/mod/{slug}"))
}

/// Why [`validate_download_url`] refused a URL.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UrlTrustError {
    /// Not an `https://` URL (plain HTTP, `file:`, scheme-relative, …).
    NotHttps,
    /// HTTPS but not the official `cdn.modrinth.com` host. This includes
    /// lookalike tricks (`cdn.modrinth.com.evil.test`, userinfo `@` games)
    /// — the prefix match is exact and fails closed.
    UntrustedHost,
    /// Well-formed prefix but a malformed remainder: whitespace, control
    /// bytes, NUL, backslashes or `..` path segments.
    Malformed,
}

impl fmt::Display for UrlTrustError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            UrlTrustError::NotHttps => write!(f, "URL is not https://"),
            UrlTrustError::UntrustedHost => {
                write!(f, "URL host is not {MODRINTH_CDN_HOST}")
            }
            UrlTrustError::Malformed => write!(f, "URL is malformed"),
        }
    }
}
impl std::error::Error for UrlTrustError {}

/// Fail-closed trust check for a mod download URL: only
/// `https://cdn.modrinth.com/<path>` with a non-empty path and no
/// whitespace, control bytes, backslashes or `..` segments passes.
///
/// Arbitrary domains — including `*.cdn.modrinth.com`-lookalikes — are
/// always [`UrlTrustError::UntrustedHost`]. This is the zero-redistribution
/// chokepoint: a server manifest can only ever point clients at bytes
/// Modrinth itself hosts.
pub fn validate_download_url(url: &str) -> Result<(), UrlTrustError> {
    let prefix = format!("https://{MODRINTH_CDN_HOST}/");
    if !url.starts_with("https://") {
        return Err(UrlTrustError::NotHttps);
    }
    let Some(rest) = url.strip_prefix(&prefix) else {
        return Err(UrlTrustError::UntrustedHost);
    };
    let path = rest.split(['?', '#']).next().unwrap_or("");
    let malformed = path.is_empty()
        || url.bytes().any(|b| b < 0x21 || b == 0x7f || b == b'\\')
        || path.split('/').any(|seg| seg.is_empty() || seg == "..");
    if malformed {
        return Err(UrlTrustError::Malformed);
    }
    Ok(())
}

// ---------------------------------------------------------------- ToS: rate limit

/// Upper bound on [`RateLimitState`]'s local response cache.
pub const RATE_LIMIT_MAX_CACHE_ENTRIES: usize = 256;
/// First backoff step after a failed request, in seconds.
pub const RATE_LIMIT_BACKOFF_BASE_SECS: u64 = 1;
/// Backoff ceiling, in seconds.
pub const RATE_LIMIT_BACKOFF_MAX_SECS: u64 = 300;

/// One cached API response.
#[derive(Clone, Debug, PartialEq, Eq)]
struct CachedResponse {
    body: String,
    stored_at_unix: u64,
    ttl_secs: u64,
}

/// Client-side honor state for the Modrinth rate-limit contract.
///
/// Tracks `X-Ratelimit-Remaining` / `X-Ratelimit-Reset` (seconds-until-reset
/// values are normalized against `now_unix`; larger values are read as an
/// absolute epoch), keeps a bounded TTL response cache keyed by request
/// URL, and runs exponential backoff after consecutive failures.
/// Everything is deterministic: callers pass `now_unix`, the module never
/// reads a clock.
#[derive(Clone, Debug)]
pub struct RateLimitState {
    remaining: Option<u32>,
    reset_at_unix: Option<u64>,
    consecutive_failures: u32,
    backoff_until_unix: Option<u64>,
    cache: BTreeMap<String, CachedResponse>,
}

impl Default for RateLimitState {
    fn default() -> Self {
        Self::new()
    }
}

impl RateLimitState {
    pub fn new() -> Self {
        Self {
            remaining: None,
            reset_at_unix: None,
            consecutive_failures: 0,
            backoff_until_unix: None,
            cache: BTreeMap::new(),
        }
    }

    /// Fold one response's headers into the state. Header names match
    /// case-insensitively; only `X-Ratelimit-Remaining` and
    /// `X-Ratelimit-Reset` are read.
    pub fn observe_headers(&mut self, headers: &[(String, String)], now_unix: u64) {
        for (name, value) in headers {
            if name.eq_ignore_ascii_case("x-ratelimit-remaining") {
                self.remaining = value.trim().parse::<u32>().ok();
            } else if name.eq_ignore_ascii_case("x-ratelimit-reset") {
                if let Ok(raw) = value.trim().parse::<u64>() {
                    // Small values are durations-until-reset; values past
                    // `now` are absolute epochs. Either reading honors
                    // "do not request before this point".
                    self.reset_at_unix = Some(if raw > now_unix { raw } else { now_unix + raw });
                }
            }
        }
    }

    /// `X-Ratelimit-Remaining` from the last observed response, if any.
    pub fn requests_remaining(&self) -> Option<u32> {
        self.remaining
    }

    /// Unix time at which the current window resets, if known.
    pub fn reset_at_unix(&self) -> Option<u64> {
        self.reset_at_unix
    }

    /// Unix time the backoff window lifts, if a failure is still cooling down.
    pub fn backoff_until_unix(&self) -> Option<u64> {
        self.backoff_until_unix
    }

    /// Whether a request may be sent at `now_unix`: never inside an active
    /// backoff window, and never while the observed remaining budget is 0
    /// and the reset time is still in the future.
    pub fn may_request(&self, now_unix: u64) -> bool {
        self.retry_after_secs(now_unix) == 0
    }

    /// Seconds the caller must wait before the next request is allowed —
    /// the max of the rate-limit reset wait (only when budget is
    /// exhausted) and the failure backoff wait. 0 means "go".
    pub fn retry_after_secs(&self, now_unix: u64) -> u64 {
        let mut wait = 0u64;
        if self.remaining == Some(0) {
            if let Some(reset) = self.reset_at_unix {
                wait = wait.max(reset.saturating_sub(now_unix));
            }
        }
        if let Some(until) = self.backoff_until_unix {
            wait = wait.max(until.saturating_sub(now_unix));
        }
        wait
    }

    /// Record a successful request: clears the failure counter and any
    /// pending backoff. Rate-limit headers still apply separately.
    pub fn record_success(&mut self) {
        self.consecutive_failures = 0;
        self.backoff_until_unix = None;
    }

    /// Record a failed request: exponential backoff
    /// `base * 2^(failures-1)` seconds, capped at
    /// [`RATE_LIMIT_BACKOFF_MAX_SECS`].
    pub fn record_failure(&mut self, now_unix: u64) {
        self.consecutive_failures = self.consecutive_failures.saturating_add(1);
        let shift = self.consecutive_failures.saturating_sub(1).min(20);
        let delay = RATE_LIMIT_BACKOFF_BASE_SECS
            .saturating_mul(1u64 << shift)
            .min(RATE_LIMIT_BACKOFF_MAX_SECS);
        self.backoff_until_unix = Some(now_unix.saturating_add(delay));
    }

    /// Cached response body for `url`, or `None` when absent/expired.
    pub fn cache_get(&self, url: &str, now_unix: u64) -> Option<&str> {
        self.cache.get(url).and_then(|entry| {
            (entry.stored_at_unix.saturating_add(entry.ttl_secs) > now_unix).then_some(&*entry.body)
        })
    }

    /// Store `body` under `url` for `ttl_secs`. The cache is bounded at
    /// [`RATE_LIMIT_MAX_CACHE_ENTRIES`]; the oldest-stored entry is evicted
    /// first. A 0 TTL stores an immediately-expired entry (a no-op cache
    /// write), keeping the call total for bookkeeping.
    pub fn cache_put(&mut self, url: &str, body: String, now_unix: u64, ttl_secs: u64) {
        if self.cache.len() >= RATE_LIMIT_MAX_CACHE_ENTRIES && !self.cache.contains_key(url) {
            if let Some(oldest) = self
                .cache
                .iter()
                .min_by_key(|(key, entry)| (entry.stored_at_unix, (*key).clone()))
                .map(|(key, _)| key.clone())
            {
                self.cache.remove(&oldest);
            }
        }
        self.cache.insert(
            url.to_owned(),
            CachedResponse {
                body,
                stored_at_unix: now_unix,
                ttl_secs,
            },
        );
    }

    /// Drop every entry whose TTL has elapsed. Returns how many were evicted.
    pub fn cache_purge_expired(&mut self, now_unix: u64) -> usize {
        let before = self.cache.len();
        self.cache
            .retain(|_, e| e.stored_at_unix.saturating_add(e.ttl_secs) > now_unix);
        before - self.cache.len()
    }

    pub fn cache_len(&self) -> usize {
        self.cache.len()
    }
}

// ---------------------------------------------------------------- ToS: attribution

/// One Modrinth search/browser result with the attribution the ToS
/// requires megu to preserve: `author`, `license_id` and a
/// `project_url` of the form `https://modrinth.com/mod/<slug>`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModrinthSearchHit {
    pub project_id: String,
    pub slug: String,
    pub title: String,
    pub author: String,
    /// SPDX license id (or `LicenseRef-…`) as Modrinth reports it.
    pub license_id: String,
    /// Always `https://modrinth.com/mod/<slug>`, derived — never taken
    /// from the wire, so a hostile payload cannot rewrite attribution.
    pub project_url: String,
}

impl ModrinthSearchHit {
    /// Build a hit from trusted fields; `project_url` is derived from
    /// `slug` and fails closed on a non-slug value.
    pub fn new(
        project_id: &str,
        slug: &str,
        title: &str,
        author: &str,
        license_id: &str,
    ) -> Result<Self, ModrinthSyncError> {
        let project_url = project_page_url(slug)
            .ok_or_else(|| ModrinthSyncError::InvalidSlug(slug.to_owned()))?;
        Ok(Self {
            project_id: project_id.to_owned(),
            slug: slug.to_owned(),
            title: title.to_owned(),
            author: author.to_owned(),
            license_id: license_id.to_owned(),
            project_url,
        })
    }

    /// Parse one `hits[]` element from `/v2/search`. `author`, `license`
    /// and a usable `slug` are mandatory — a result that cannot be
    /// attributed is an error, not a best-effort row.
    pub fn from_hit_json(json: &Json) -> Result<Self, ModrinthSyncError> {
        let slug = required_str(json, "slug")?;
        Self::new(
            required_str(json, "project_id")?,
            slug,
            json.get_str("title").unwrap_or(slug),
            required_str(json, "author")?,
            required_str(json, "license")?,
        )
    }
}

// ---------------------------------------------------------------- path safety

/// The Mojang/secret purity rule a path tripped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PurityRule {
    /// `*.jar` anywhere under `overrides/` — bundled mod binaries are
    /// third-party redistribution and are never legal in a pack.
    JarInOverrides,
    /// `config_packets.bin` in any directory — megu-internal channel
    /// state, never pack content.
    ConfigPacketsBin,
    /// Anything under a `datapacks/vanilla/` segment pair — Mojang's own
    /// datapack data must not be republished inside a pack.
    VanillaDatapack,
    /// `world_overlay*.bin*` — megu world-overlay binaries.
    WorldOverlayBin,
    /// `launcher_accounts.json` — Mojang/Microsoft credential material.
    LauncherAccountsJson,
    /// `accounts.json` — account/credential material in any directory.
    AccountsJson,
    /// `*.token` — bearer/session token files.
    TokenFile,
}

/// Why [`validate_mrpack_path`] refused a path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PathRejection {
    Empty,
    /// Leading `/`, `X:` drive prefix or `//` UNC.
    AbsolutePath,
    /// A `..` segment anywhere in the path.
    ParentTraversal,
    /// A NUL byte — C-string truncation attacks on `open(2)`.
    NulByte,
    /// A backslash — a Windows separator that would silently re-root the
    /// path on extraction.
    Backslash,
    /// A `.` segment — ambiguous; the mrpack writer never emits one.
    DotSegment,
    /// An empty interior/trailing segment (`a//b`, `a/`).
    EmptySegment,
    /// The Mojang/secret purity filter matched.
    Purity(PurityRule),
}

impl fmt::Display for PathRejection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PathRejection::Empty => write!(f, "empty path"),
            PathRejection::AbsolutePath => write!(f, "absolute path"),
            PathRejection::ParentTraversal => write!(f, "`..` traversal segment"),
            PathRejection::NulByte => write!(f, "NUL byte in path"),
            PathRejection::Backslash => write!(f, "backslash in path"),
            PathRejection::DotSegment => write!(f, "`.` segment in path"),
            PathRejection::EmptySegment => write!(f, "empty path segment"),
            PathRejection::Purity(rule) => write!(f, "purity rule {rule:?}"),
        }
    }
}
impl std::error::Error for PathRejection {}

/// Fail-closed validation for an instance-relative path in
/// `modrinth.index.json` or an `overrides/` zip listing.
///
/// Rejects: empty/NUL/backslash-bearing strings, absolute paths (`/…`,
/// `X:…`, `//…`), `..`/`.`/empty segments, and the Mojang/secret purity
/// filter — `*.jar` under `overrides/`, `config_packets.bin`,
/// `datapacks/vanilla/**`, `world_overlay*.bin*`,
/// `launcher_accounts.json`, `accounts.json`, `*.token` (all
/// case-insensitive, since pack extraction may land on Windows).
pub fn validate_mrpack_path(path: &str) -> Result<(), PathRejection> {
    if path.is_empty() {
        return Err(PathRejection::Empty);
    }
    if path.bytes().any(|b| b == 0) {
        return Err(PathRejection::NulByte);
    }
    if path.contains('\\') {
        return Err(PathRejection::Backslash);
    }
    if path.starts_with('/')
        || (path.len() >= 2
            && path.as_bytes()[1] == b':'
            && path.as_bytes()[0].is_ascii_alphabetic())
    {
        return Err(PathRejection::AbsolutePath);
    }
    let segments: Vec<&str> = path.split('/').collect();
    for seg in &segments {
        match *seg {
            "" => return Err(PathRejection::EmptySegment),
            "." => return Err(PathRejection::DotSegment),
            ".." => return Err(PathRejection::ParentTraversal),
            _ => {}
        }
    }
    if let Some(rule) = purity_violation(&segments) {
        return Err(PathRejection::Purity(rule));
    }
    Ok(())
}

fn purity_violation(segments: &[&str]) -> Option<PurityRule> {
    let file = segments.last()?.to_ascii_lowercase();
    if segments[0].eq_ignore_ascii_case("overrides") && file.ends_with(".jar") {
        return Some(PurityRule::JarInOverrides);
    }
    if file == "config_packets.bin" {
        return Some(PurityRule::ConfigPacketsBin);
    }
    if segments
        .windows(2)
        .any(|w| w[0].eq_ignore_ascii_case("datapacks") && w[1].eq_ignore_ascii_case("vanilla"))
    {
        return Some(PurityRule::VanillaDatapack);
    }
    if file.starts_with("world_overlay") && file.contains(".bin") {
        return Some(PurityRule::WorldOverlayBin);
    }
    if file == "launcher_accounts.json" {
        return Some(PurityRule::LauncherAccountsJson);
    }
    if file == "accounts.json" {
        return Some(PurityRule::AccountsJson);
    }
    if file.ends_with(".token") {
        return Some(PurityRule::TokenFile);
    }
    None
}

// ---------------------------------------------------------------- mrpack index

/// The manifest filename inside every `.mrpack` zip.
pub const MRPACK_INDEX_FILE: &str = "modrinth.index.json";
/// The only `formatVersion` this parser accepts.
pub const MRPACK_FORMAT_VERSION: u64 = 1;

/// `env.client` / `env.server` support tier in an index `files[]` entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EnvSupport {
    Required,
    Optional,
    Unsupported,
}

impl EnvSupport {
    fn as_str(self) -> &'static str {
        match self {
            EnvSupport::Required => "required",
            EnvSupport::Optional => "optional",
            EnvSupport::Unsupported => "unsupported",
        }
    }
    fn parse(text: &str) -> Result<Self, ModrinthSyncError> {
        match text {
            "required" => Ok(EnvSupport::Required),
            "optional" => Ok(EnvSupport::Optional),
            "unsupported" => Ok(EnvSupport::Unsupported),
            other => Err(ModrinthSyncError::InvalidEnvSupport(other.to_owned())),
        }
    }
}

/// `env` block of a `files[]` entry (optional in the schema).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MrpackEnv {
    pub client: EnvSupport,
    pub server: EnvSupport,
}

/// `hashes` block of a `files[]` entry — SHA-1 and SHA-512 as lowercase
/// hex, carried verbatim (the schema's own digests; the sync lane verifies
/// every committed byte with SHA-256 regardless).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MrpackHashes {
    pub sha1: String,
    pub sha512: String,
}

/// One `files[]` entry: a remote artifact the launcher downloads into the
/// instance at `path`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MrpackFile {
    pub path: String,
    pub hashes: MrpackHashes,
    pub env: Option<MrpackEnv>,
    /// Verified `https://cdn.modrinth.com/...` mirrors, in preference order.
    pub downloads: Vec<String>,
    pub file_size: u64,
}

/// A parsed `modrinth.index.json` (schema v1).
#[derive(Clone, Debug, PartialEq)]
pub struct MrpackIndex {
    pub format_version: u64,
    /// Always `"minecraft"` — anything else fails closed.
    pub game: String,
    pub version_id: String,
    pub name: String,
    pub summary: Option<String>,
    /// Loader/game pins, e.g. `minecraft` -> `1.21.1`, `fabric-loader` -> `0.16.x`.
    pub dependencies: BTreeMap<String, String>,
    pub files: Vec<MrpackFile>,
}

impl MrpackIndex {
    /// Parse the index document, validating every field fail-closed:
    /// `formatVersion` must be 1, `game` must be `"minecraft"`, every file
    /// path must pass [`validate_mrpack_path`], every download URL must be
    /// a trusted CDN URL and every hash must be well-formed hex.
    pub fn from_json(json: &Json) -> Result<Self, ModrinthSyncError> {
        let format_version = required_u64(json, "formatVersion")?;
        if format_version != MRPACK_FORMAT_VERSION {
            return Err(ModrinthSyncError::UnsupportedFormatVersion(format_version));
        }
        let game = required_str(json, "game")?;
        if game != "minecraft" {
            return Err(ModrinthSyncError::NotMinecraftGame(game.to_owned()));
        }
        let dependencies = parse_dependencies(json.get("dependencies"))?;
        let files_json = required_array(json, "files")?;
        let mut files = Vec::with_capacity(files_json.len());
        for entry in files_json {
            files.push(parse_mrpack_file(entry)?);
        }
        Ok(Self {
            format_version,
            game: game.to_owned(),
            version_id: required_str(json, "versionId")?.to_owned(),
            name: required_str(json, "name")?.to_owned(),
            summary: json.get_str("summary").map(str::to_owned),
            dependencies,
            files,
        })
    }

    /// Parse `modrinth.index.json` from its raw text form.
    pub fn parse_index(text: &str) -> Result<Self, ModrinthSyncError> {
        Self::from_json(&parse(text)?)
    }

    /// Serialize in the schema's field order; round-trips with
    /// [`Self::from_json`].
    pub fn to_json(&self) -> Json {
        let files = self
            .files
            .iter()
            .map(|f| {
                let hashes = vec![
                    ("sha1".to_owned(), Json::String(f.hashes.sha1.clone())),
                    ("sha512".to_owned(), Json::String(f.hashes.sha512.clone())),
                ];
                let mut members = vec![
                    ("path".to_owned(), Json::String(f.path.clone())),
                    ("hashes".to_owned(), Json::Object(hashes)),
                ];
                if let Some(env) = f.env {
                    members.push((
                        "env".to_owned(),
                        Json::Object(vec![
                            (
                                "client".to_owned(),
                                Json::String(env.client.as_str().to_owned()),
                            ),
                            (
                                "server".to_owned(),
                                Json::String(env.server.as_str().to_owned()),
                            ),
                        ]),
                    ));
                }
                members.push((
                    "downloads".to_owned(),
                    Json::Array(f.downloads.iter().cloned().map(Json::String).collect()),
                ));
                members.push(("fileSize".to_owned(), Json::Number(f.file_size as f64)));
                Json::Object(members)
            })
            .collect();
        let mut members = vec![
            (
                "formatVersion".to_owned(),
                Json::Number(self.format_version as f64),
            ),
            ("game".to_owned(), Json::String(self.game.clone())),
            (
                "versionId".to_owned(),
                Json::String(self.version_id.clone()),
            ),
            ("name".to_owned(), Json::String(self.name.clone())),
        ];
        if let Some(summary) = &self.summary {
            members.push(("summary".to_owned(), Json::String(summary.clone())));
        }
        members.push(("files".to_owned(), Json::Array(files)));
        members.push((
            "dependencies".to_owned(),
            Json::Object(
                self.dependencies
                    .iter()
                    .map(|(k, v)| (k.clone(), Json::String(v.clone())))
                    .collect(),
            ),
        ));
        Json::Object(members)
    }

    /// Compact `modrinth.index.json` text for writing into a `.mrpack`.
    pub fn to_index_string(&self) -> String {
        self.to_json().to_json_string()
    }
}

fn parse_dependencies(json: Option<&Json>) -> Result<BTreeMap<String, String>, ModrinthSyncError> {
    let mut deps = BTreeMap::new();
    let Some(json) = json else {
        return Err(ModrinthSyncError::MissingField("dependencies"));
    };
    let Some(members) = json.as_object() else {
        return Err(ModrinthSyncError::WrongType("dependencies"));
    };
    for (key, value) in members {
        let Some(v) = value.as_str() else {
            return Err(ModrinthSyncError::WrongType("dependencies[]"));
        };
        deps.insert(key.clone(), v.to_owned());
    }
    Ok(deps)
}

fn parse_mrpack_file(json: &Json) -> Result<MrpackFile, ModrinthSyncError> {
    let path = required_str(json, "path")?;
    validate_mrpack_path(path).map_err(|reason| ModrinthSyncError::UnsafePath {
        path: path.to_owned(),
        reason,
    })?;
    let hashes = json
        .get("hashes")
        .ok_or(ModrinthSyncError::MissingField("hashes"))?;
    let env = match json.get("env") {
        None | Some(Json::Null) => None,
        Some(env) => Some(MrpackEnv {
            client: EnvSupport::parse(required_str(env, "client")?)?,
            server: EnvSupport::parse(required_str(env, "server")?)?,
        }),
    };
    let downloads_json = required_array(json, "downloads")?;
    let mut downloads = Vec::with_capacity(downloads_json.len());
    for url in downloads_json {
        let url = url
            .as_str()
            .ok_or(ModrinthSyncError::WrongType("downloads[]"))?;
        validate_download_url(url).map_err(|reason| ModrinthSyncError::UntrustedDownloadUrl {
            url: url.to_owned(),
            reason,
        })?;
        downloads.push(url.to_owned());
    }
    if downloads.is_empty() {
        return Err(ModrinthSyncError::MissingField("downloads[]"));
    }
    Ok(MrpackFile {
        path: path.to_owned(),
        hashes: MrpackHashes {
            sha1: require_hex(hashes, "sha1", 40)?,
            sha512: require_hex(hashes, "sha512", 128)?,
        },
        env,
        downloads,
        file_size: required_u64(json, "fileSize")?,
    })
}

// ---------------------------------------------------------------- server push sync

/// The wire filename for a [`ServerModSyncManifest`] document.
pub const SERVER_MOD_SYNC_FILE: &str = "megu.modsync.json";
/// The only sync-manifest `formatVersion` this parser accepts.
pub const SERVER_MOD_SYNC_FORMAT_VERSION: u64 = 1;

/// Kind of a project-to-project dependency edge (Modrinth's vocabulary).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DependencyKind {
    /// Hard requirement: resolution fails closed when the target is absent.
    Required,
    /// Soft requirement: orders the install when present, ignored when not.
    Optional,
    /// The two projects cannot be enabled together.
    Incompatible,
    /// The target ships inside this project's jar — provided, not installed.
    Embedded,
}

impl DependencyKind {
    pub fn as_str(self) -> &'static str {
        match self {
            DependencyKind::Required => "required",
            DependencyKind::Optional => "optional",
            DependencyKind::Incompatible => "incompatible",
            DependencyKind::Embedded => "embedded",
        }
    }
    pub fn parse(text: &str) -> Result<Self, ModrinthSyncError> {
        match text {
            "required" => Ok(DependencyKind::Required),
            "optional" => Ok(DependencyKind::Optional),
            "incompatible" => Ok(DependencyKind::Incompatible),
            "embedded" => Ok(DependencyKind::Embedded),
            other => Err(ModrinthSyncError::InvalidDependencyKind(other.to_owned())),
        }
    }
}

/// One declared edge: this project relates to `project_id` as `kind`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModDependencyEdge {
    pub project_id: String,
    pub kind: DependencyKind,
    /// Exact Modrinth dependency version id when the API/pack declares one.
    /// This is never collapsed away: it is serialized, enforced against a
    /// directly installed provider, and carried into mismatch diagnostics.
    pub version_id: Option<String>,
    /// Declaring-edge version predicate when a modpack supplies one.
    pub version_range: Option<String>,
}

impl ModDependencyEdge {
    pub fn new(project_id: impl Into<String>, kind: DependencyKind) -> Self {
        Self {
            project_id: project_id.into(),
            kind,
            version_id: None,
            version_range: None,
        }
    }

    pub fn with_version_id(mut self, version_id: impl Into<String>) -> Self {
        self.version_id = Some(version_id.into());
        self
    }

    pub fn with_version_range(mut self, range: impl Into<String>) -> Self {
        self.version_range = Some(range.into());
        self
    }
}

/// One mod the server expects clients to run. Bytes are never carried —
/// only identity, hashes and a trusted CDN URL.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServerModEntry {
    pub project_id: String,
    pub version_id: String,
    /// Bare jar filename (`fabric-api-x.y.z.jar`); never a path.
    pub file_name: String,
    /// Mandatory SHA-256, 64 lowercase hex — the digest the download gate
    /// verifies before commit.
    pub sha256: String,
    /// Optional SHA-512 cross-check, 128 lowercase hex.
    pub sha512: Option<String>,
    /// `https://cdn.modrinth.com/...` only; anything else routes the entry
    /// to `untrusted_rejected`.
    pub download_url: String,
    /// `false` marks an opt-in mod the client may decline.
    pub required: bool,
    pub dependencies: Vec<ModDependencyEdge>,
}

/// The server-pushed manifest: the exact mod set a world requires, as
/// Modrinth references only.
#[derive(Clone, Debug, PartialEq)]
pub struct ServerModSyncManifest {
    pub format_version: u64,
    pub server_name: String,
    pub minecraft_version: String,
    /// Mod loader id (`fabric`, `neoforge`, …) when the server pins one.
    pub loader: Option<String>,
    pub entries: Vec<ServerModEntry>,
}

impl ServerModSyncManifest {
    /// Structural parse: required fields must be present and correctly
    /// typed, but trust properties (URL allowlist, hash shape, filename
    /// safety) are deliberately *not* checked here — they are per-entry
    /// routing decisions made by [`diff_installed_vs_server`] so one bad
    /// entry cannot veto a whole manifest.
    pub fn from_json(json: &Json) -> Result<Self, ModrinthSyncError> {
        let format_version = required_u64(json, "formatVersion")?;
        if format_version != SERVER_MOD_SYNC_FORMAT_VERSION {
            return Err(ModrinthSyncError::UnsupportedFormatVersion(format_version));
        }
        let mods = required_array(json, "mods")?;
        let mut entries = Vec::with_capacity(mods.len());
        for m in mods {
            entries.push(parse_server_mod_entry(m)?);
        }
        let loader = match json.get("loader") {
            None | Some(Json::Null) => None,
            Some(v) => Some(
                v.as_str()
                    .ok_or(ModrinthSyncError::WrongType("loader"))?
                    .to_owned(),
            ),
        };
        Ok(Self {
            format_version,
            server_name: required_str(json, "serverName")?.to_owned(),
            minecraft_version: required_str(json, "minecraftVersion")?.to_owned(),
            loader,
            entries,
        })
    }

    pub fn parse_manifest(text: &str) -> Result<Self, ModrinthSyncError> {
        Self::from_json(&parse(text)?)
    }

    pub fn to_json(&self) -> Json {
        let mods = self
            .entries
            .iter()
            .map(|e| {
                let mut members = vec![
                    ("projectId".to_owned(), Json::String(e.project_id.clone())),
                    ("versionId".to_owned(), Json::String(e.version_id.clone())),
                    ("fileName".to_owned(), Json::String(e.file_name.clone())),
                    ("sha256".to_owned(), Json::String(e.sha256.clone())),
                ];
                if let Some(sha512) = &e.sha512 {
                    members.push(("sha512".to_owned(), Json::String(sha512.clone())));
                }
                members.push((
                    "downloadUrl".to_owned(),
                    Json::String(e.download_url.clone()),
                ));
                members.push(("required".to_owned(), Json::Bool(e.required)));
                members.push((
                    "dependencies".to_owned(),
                    Json::Array(
                        e.dependencies
                            .iter()
                            .map(|d| {
                                {
                                    let mut members = vec![
                                        ("projectId".to_owned(), Json::String(d.project_id.clone())),
                                        ("kind".to_owned(), Json::String(d.kind.as_str().to_owned())),
                                    ];
                                    if let Some(version_id) = &d.version_id {
                                        members.push(("versionId".to_owned(), Json::String(version_id.clone())));
                                    }
                                    if let Some(range) = &d.version_range {
                                        members.push(("versionRange".to_owned(), Json::String(range.clone())));
                                    }
                                    Json::Object(members)
                                }
                            })
                            .collect(),
                    ),
                ));
                Json::Object(members)
            })
            .collect();
        let mut members = vec![
            (
                "formatVersion".to_owned(),
                Json::Number(self.format_version as f64),
            ),
            (
                "serverName".to_owned(),
                Json::String(self.server_name.clone()),
            ),
            (
                "minecraftVersion".to_owned(),
                Json::String(self.minecraft_version.clone()),
            ),
        ];
        match &self.loader {
            Some(loader) => members.push(("loader".to_owned(), Json::String(loader.clone()))),
            None => members.push(("loader".to_owned(), Json::Null)),
        }
        members.push(("mods".to_owned(), Json::Array(mods)));
        Json::Object(members)
    }

    pub fn to_manifest_string(&self) -> String {
        self.to_json().to_json_string()
    }
}

fn parse_server_mod_entry(json: &Json) -> Result<ServerModEntry, ModrinthSyncError> {
    let deps_json = match json.get("dependencies") {
        None | Some(Json::Null) => &[][..],
        Some(v) => v
            .as_array()
            .ok_or(ModrinthSyncError::WrongType("dependencies"))?,
    };
    let mut dependencies = Vec::with_capacity(deps_json.len());
    for d in deps_json {
        let optional_string = |key: &'static str| -> Result<Option<String>, ModrinthSyncError> {
            match d.get(key) {
                None | Some(Json::Null) => Ok(None),
                Some(v) => Ok(Some(v.as_str().ok_or(ModrinthSyncError::WrongType(key))?.to_owned())),
            }
        };
        dependencies.push(ModDependencyEdge {
            project_id: required_str(d, "projectId")?.to_owned(),
            kind: DependencyKind::parse(required_str(d, "kind")?)?,
            version_id: optional_string("versionId")?,
            version_range: optional_string("versionRange")?,
        });
    }
    Ok(ServerModEntry {
        project_id: required_str(json, "projectId")?.to_owned(),
        version_id: required_str(json, "versionId")?.to_owned(),
        file_name: required_str(json, "fileName")?.to_owned(),
        sha256: required_str(json, "sha256")?.to_owned(),
        sha512: match json.get("sha512") {
            None | Some(Json::Null) => None,
            Some(v) => Some(
                v.as_str()
                    .ok_or(ModrinthSyncError::WrongType("sha512"))?
                    .to_owned(),
            ),
        },
        download_url: required_str(json, "downloadUrl")?.to_owned(),
        required: json.get_bool("required", true),
        dependencies,
    })
}

// ---------------------------------------------------------------- diff -> plan

/// A mod as the client's `mods/` folder reports it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InstalledMod {
    pub file_name: String,
    /// Modrinth project id when the installed jar identifies one.
    pub project_id: Option<String>,
    /// SHA-256 of the installed bytes, when already computed.
    pub sha256: Option<String>,
    pub enabled: bool,
}

/// Why a manifest entry was routed to `untrusted_rejected`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SyncRejectReason {
    /// `downloadUrl` is not `https://cdn.modrinth.com/...`.
    UntrustedDownloadUrl,
    /// `sha256` is not 64 hex digits.
    MalformedSha256,
    /// `sha512` is present but not 128 hex digits.
    MalformedSha512,
    /// `fileName` is not a safe bare filename.
    UnsafeFileName,
}

/// A manifest entry the plan refused, with the reason.
#[derive(Clone, Debug, PartialEq)]
pub struct RejectedModEntry {
    pub project_id: String,
    pub version_id: String,
    pub file_name: String,
    pub reason: SyncRejectReason,
}

/// The deterministic one-click sync plan [`diff_installed_vs_server`]
/// produces. Nothing here has side effects — the UI shows the plan, gets
/// `requires_user_consent`, then executes it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ModSyncPlan {
    /// Trusted entries the client must fetch (missing, hash-mismatched, or
    /// a version change under the same project). Manifest order.
    pub to_download: Vec<ServerModEntry>,
    /// Installed-but-disabled filenames that satisfy the manifest and must
    /// be re-enabled. Sorted.
    pub to_enable: Vec<String>,
    /// Enabled installed filenames the manifest does not want: old
    /// versions being replaced plus foreign extras. Sorted.
    pub to_disable: Vec<String>,
    /// Manifest entries that failed trust checks — surfaced for the
    /// consent dialog, never acted on.
    pub untrusted_rejected: Vec<RejectedModEntry>,
    /// Always `true` while `to_download` is non-empty: the UI must block
    /// on explicit user consent before fetching bytes.
    pub requires_user_consent: bool,
}

/// One entry's trust verdict: a malformed hash or unsafe filename is a
/// rejection exactly like an untrusted host — fail closed, keep going.
fn entry_reject_reason(entry: &ServerModEntry) -> Option<SyncRejectReason> {
    if normalize_sha256_hex(&entry.sha256).is_err() {
        return Some(SyncRejectReason::MalformedSha256);
    }
    if let Some(sha512) = &entry.sha512 {
        if !is_hex_of_len(sha512, 128) {
            return Some(SyncRejectReason::MalformedSha512);
        }
    }
    if !is_safe_file_name(&entry.file_name) {
        return Some(SyncRejectReason::UnsafeFileName);
    }
    if validate_download_url(&entry.download_url).is_err() {
        return Some(SyncRejectReason::UntrustedDownloadUrl);
    }
    None
}

/// A bare filename: no separators, NULs or control bytes, not `.`/`..`.
fn is_safe_file_name(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && !name
            .bytes()
            .any(|b| b < 0x20 || b == 0x7f || b == b'/' || b == b'\\')
}

fn sha256_matches(installed: Option<&str>, expected: &str) -> bool {
    match installed {
        Some(actual) => actual.eq_ignore_ascii_case(expected),
        None => false,
    }
}

/// Diff the client's installed set against the server-pushed manifest.
///
/// Matching is by `fileName` first, then `project_id`: an installed jar
/// with the same project id but a different filename is the old version of
/// a mod being upgraded — it is downloaded *and* disabled. Trusted
/// manifest entries are the only source of actions; anything failing the
/// URL/hash/filename checks lands in `untrusted_rejected` untouched.
pub fn diff_installed_vs_server(
    installed: &[InstalledMod],
    manifest: &ServerModSyncManifest,
) -> ModSyncPlan {
    let mut plan = ModSyncPlan::default();
    let mut trusted_names: BTreeSet<&str> = BTreeSet::new();
    let mut claimed_projects: BTreeSet<&str> = BTreeSet::new();
    let mut disable: BTreeSet<String> = BTreeSet::new();
    let mut enable: BTreeSet<String> = BTreeSet::new();

    for entry in &manifest.entries {
        if let Some(reason) = entry_reject_reason(entry) {
            plan.untrusted_rejected.push(RejectedModEntry {
                project_id: entry.project_id.clone(),
                version_id: entry.version_id.clone(),
                file_name: entry.file_name.clone(),
                reason,
            });
            continue;
        }
        trusted_names.insert(entry.file_name.as_str());
        claimed_projects.insert(entry.project_id.as_str());

        let same_name = installed.iter().find(|m| m.file_name == entry.file_name);
        if let Some(inst) = same_name {
            if sha256_matches(inst.sha256.as_deref(), &entry.sha256) {
                // A disabled *required* mod is re-enabled; a disabled
                // optional one is the user's explicit opt-out — respected.
                if !inst.enabled && entry.required {
                    enable.insert(inst.file_name.clone());
                }
                continue;
            }
            // Same filename, wrong bytes: re-download over it.
            plan.to_download.push(entry.clone());
            continue;
        }
        let same_project = installed
            .iter()
            .find(|m| m.project_id.as_deref() == Some(entry.project_id.as_str()));
        if let Some(old) = same_project {
            plan.to_download.push(entry.clone());
            if old.enabled {
                disable.insert(old.file_name.clone());
            }
        } else {
            plan.to_download.push(entry.clone());
        }
    }

    for m in installed {
        if !m.enabled {
            continue;
        }
        let name_claimed = trusted_names.contains(m.file_name.as_str());
        let project_claimed = m
            .project_id
            .as_deref()
            .map(|p| claimed_projects.contains(p))
            .unwrap_or(false);
        if !name_claimed && !project_claimed {
            disable.insert(m.file_name.clone());
        }
    }

    plan.to_enable = enable.into_iter().collect();
    plan.to_disable = disable.into_iter().collect();
    plan.requires_user_consent = !plan.to_download.is_empty();
    plan
}

// ---------------------------------------------------------------- resolver

/// One mod offered to the resolver: identity plus its declared edges.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModNode {
    pub project_id: String,
    pub version_id: String,
    pub dependencies: Vec<ModDependencyEdge>,
}

/// A `required` edge whose target nothing provides.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MissingDependency {
    pub project_id: String,
    pub missing_project_id: String,
    pub version_id: Option<String>,
    pub version_range: Option<String>,
}

/// A dependency whose provider is present but not the exact version the
/// declaring edge pinned.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DependencyVersionMismatch {
    pub project_id: String,
    pub dependency_project_id: String,
    pub required_version_id: String,
    pub actual_version_id: String,
    pub version_range: Option<String>,
}

/// An `incompatible` edge where both ends are present (reported once per
/// unordered pair, sorted).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModConflict {
    pub project_id: String,
    pub incompatible_with: String,
}

/// Deterministic resolution output.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ResolvedModSet {
    /// Topological install order — dependencies before dependents — over
    /// every node that could be ordered. Ties break on `project_id`, so
    /// the same input always yields the same order.
    pub install_order: Vec<String>,
    /// Required deps no installed or embedded project provides.
    pub missing_required: Vec<MissingDependency>,
    /// Exact dependency version pins contradicted by the selected provider.
    pub version_mismatches: Vec<DependencyVersionMismatch>,
    /// SCCs of the dependency graph, one member list per real cycle.
    pub cycle_components: Vec<Vec<String>>,
    /// Incompatible pairs both present in the set.
    pub conflicts: Vec<ModConflict>,
    /// Project ids listed more than once (duplicate installs).
    pub duplicates: Vec<String>,
    /// Nodes that could not be ordered because they sit on a dependency
    /// cycle. Sorted; empty in every acyclic resolution.
    pub cyclic: Vec<String>,
}

fn dependency_cycle_components(dependents: &BTreeMap<&str, BTreeSet<&str>>, node_ids: &BTreeSet<&str>) -> Vec<Vec<String>> {
    #[allow(clippy::too_many_arguments)]
    fn strongconnect(
        node: &str,
        graph: &BTreeMap<String, BTreeSet<String>>,
        index: &mut usize,
        indices: &mut BTreeMap<String, usize>,
        lowlink: &mut BTreeMap<String, usize>,
        stack: &mut Vec<String>,
        on_stack: &mut BTreeSet<String>,
        components: &mut Vec<Vec<String>>,
    ) {
        indices.insert(node.to_string(), *index);
        lowlink.insert(node.to_string(), *index);
        *index += 1;
        stack.push(node.to_string());
        on_stack.insert(node.to_string());
        for next in graph.get(node).into_iter().flatten() {
            if !indices.contains_key(next) {
                strongconnect(next, graph, index, indices, lowlink, stack, on_stack, components);
                let low = lowlink[next].min(lowlink[node]);
                lowlink.insert(node.to_string(), low);
            } else if on_stack.contains(next) {
                let low = indices[next].min(lowlink[node]);
                lowlink.insert(node.to_string(), low);
            }
        }
        if lowlink[node] == indices[node] {
            let mut component = Vec::new();
            while let Some(member) = stack.pop() {
                on_stack.remove(&member);
                component.push(member.clone());
                if member == node {
                    break;
                }
            }
            component.sort();
            if component.len() > 1
                || component
                    .first()
                    .is_some_and(|only| graph.get(only).is_some_and(|n| n.contains(only)))
            {
                components.push(component);
            }
        }
    }

    let mut graph: BTreeMap<String, BTreeSet<String>> = node_ids
        .iter()
        .map(|id| ((*id).to_string(), BTreeSet::new()))
        .collect();
    for (provider, targets) in dependents {
        graph
            .entry((*provider).to_string())
            .or_default()
            .extend(targets.iter().map(|target| (*target).to_string()));
    }
    let mut components = Vec::new();
    let mut index = 0usize;
    let mut indices = BTreeMap::new();
    let mut lowlink = BTreeMap::new();
    let mut stack = Vec::new();
    let mut on_stack = BTreeSet::new();
    for node in graph.keys().cloned().collect::<Vec<_>>() {
        if !indices.contains_key(&node) {
            strongconnect(
                &node,
                &graph,
                &mut index,
                &mut indices,
                &mut lowlink,
                &mut stack,
                &mut on_stack,
                &mut components,
            );
        }
    }
    components.sort();
    components
}

/// Resolve a mod set: topological order, missing-required, incompatible
/// and duplicate detection.
///
/// `embedded` edges register their target as *provided by* the embedder:
/// a `required` dep on an embedded project resolves to the embedder's node
/// for ordering and never reports missing. `optional` edges order the
/// install when the target is present and are ignored otherwise.
pub fn resolve_dependencies(mods: &[ModNode]) -> ResolvedModSet {
    let mut result = ResolvedModSet::default();

    // provider[target] = install node that satisfies a dep on `target`.
    // Real nodes always win over embedded providers.
    let mut provider: BTreeMap<&str, &str> = BTreeMap::new();
    let mut seen: BTreeSet<&str> = BTreeSet::new();
    let mut duplicates: BTreeSet<String> = BTreeSet::new();
    for m in mods {
        if !seen.insert(m.project_id.as_str()) {
            duplicates.insert(m.project_id.clone());
        }
        provider.insert(m.project_id.as_str(), m.project_id.as_str());
    }
    for m in mods {
        for d in &m.dependencies {
            if d.kind == DependencyKind::Embedded {
                provider
                    .entry(d.project_id.as_str())
                    .or_insert(m.project_id.as_str());
            }
        }
    }
    result.duplicates = duplicates.into_iter().collect();

    // Edges provider -> dependent over real install nodes only.
    let node_ids: BTreeSet<&str> = mods.iter().map(|m| m.project_id.as_str()).collect();
    let mut dependents: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
    let mut indegree: BTreeMap<&str, usize> = node_ids.iter().map(|id| (*id, 0usize)).collect();
    let mut missing: BTreeSet<(String, String, Option<String>, Option<String>)> = BTreeSet::new();
    let mut conflicts: BTreeSet<(String, String)> = BTreeSet::new();
    let mut version_mismatches: BTreeSet<(String, String, String, String, Option<String>)> = BTreeSet::new();
    let versions: BTreeMap<&str, &str> = mods
        .iter()
        .map(|m| (m.project_id.as_str(), m.version_id.as_str()))
        .collect();

    for m in mods {
        for d in &m.dependencies {
            match d.kind {
                DependencyKind::Required | DependencyKind::Optional => {
                    match provider.get(d.project_id.as_str()).copied() {
                        Some(prov) if prov != m.project_id.as_str() && node_ids.contains(prov) => {
                            // Duplicate occurrences of the same provider ->
                            // dependent edge are one graph edge. Counting each
                            // occurrence used to leave indegree permanently
                            // elevated and hallucinate a cycle.
                            let inserted = dependents
                                .entry(prov)
                                .or_default()
                                .insert(m.project_id.as_str());
                            if inserted {
                                if let Some(n) = indegree.get_mut(m.project_id.as_str()) {
                                    *n += 1;
                                }
                            }
                            if let (Some(required_version), Some(actual_version)) = (
                                d.version_id.as_deref(),
                                versions.get(prov).copied(),
                            ) {
                                if prov == d.project_id.as_str() && required_version != actual_version {
                                    version_mismatches.insert((
                                        m.project_id.clone(),
                                        d.project_id.clone(),
                                        required_version.to_owned(),
                                        actual_version.to_owned(),
                                        d.version_range.clone(),
                                    ));
                                }
                            }
                        }
                        Some(_) => {}
                        None => {
                            if d.kind == DependencyKind::Required {
                                missing.insert((
                                    m.project_id.clone(),
                                    d.project_id.clone(),
                                    d.version_id.clone(),
                                    d.version_range.clone(),
                                ));
                            }
                        }
                    }
                }
                DependencyKind::Incompatible => {
                    if provider.contains_key(d.project_id.as_str()) {
                        let mut pair = [m.project_id.clone(), d.project_id.clone()];
                        pair.sort();
                        conflicts.insert((pair[0].clone(), pair[1].clone()));
                    }
                }
                DependencyKind::Embedded => {}
            }
        }
    }

    result.missing_required = missing
        .into_iter()
        .map(|(project_id, missing_project_id, version_id, version_range)| MissingDependency {
            project_id,
            missing_project_id,
            version_id,
            version_range,
        })
        .collect();
    result.version_mismatches = version_mismatches
        .into_iter()
        .map(
            |(project_id, dependency_project_id, required_version_id, actual_version_id, version_range)| {
                DependencyVersionMismatch {
                    project_id,
                    dependency_project_id,
                    required_version_id,
                    actual_version_id,
                    version_range,
                }
            },
        )
        .collect();
    result.conflicts = conflicts
        .into_iter()
        .map(|(project_id, incompatible_with)| ModConflict {
            project_id,
            incompatible_with,
        })
        .collect();

    // Deterministic Kahn: always take the smallest ready project_id.
    let mut ready: BTreeSet<&str> = indegree
        .iter()
        .filter(|(_, n)| **n == 0)
        .map(|(id, _)| *id)
        .collect();
    while let Some(id) = ready.iter().next().copied() {
        ready.remove(id);
        result.install_order.push(id.to_owned());
        if let Some(set) = dependents.get(id) {
            for dep in set {
                if let Some(n) = indegree.get_mut(dep) {
                    *n -= 1;
                    if *n == 0 {
                        ready.insert(dep);
                    }
                }
            }
        }
    }
    result.cycle_components = dependency_cycle_components(&dependents, &node_ids);
    result.cyclic = result
        .cycle_components
        .iter()
        .flatten()
        .cloned()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    result
}

// ---------------------------------------------------------------- download gate

/// Outcome of the SHA-256 pre-commit check.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Sha256Verification {
    /// The bytes match the manifest digest — safe to commit into the
    /// CAS/`mods` directory.
    Match,
    /// The bytes do not match: the file must be discarded, never stored.
    Mismatch { expected: String, actual: String },
}

/// Verify `data` against the manifest's expected SHA-256 before commit.
/// A malformed `expected_sha256` is an error, not a mismatch — a broken
/// manifest must fail closed, not silently pass as "wrong bytes".
pub fn verify_bytes_sha256(
    data: &[u8],
    expected_sha256: &str,
) -> Result<Sha256Verification, ModrinthSyncError> {
    let expected =
        normalize_sha256_hex(expected_sha256).map_err(|_| ModrinthSyncError::InvalidHash {
            field: "sha256",
            value: expected_sha256.to_owned(),
        })?;
    let actual = sha256_hex(data);
    Ok(if actual == expected {
        Sha256Verification::Match
    } else {
        Sha256Verification::Mismatch { expected, actual }
    })
}

/// Verify an on-disk download before it may be committed into the
/// CAS/`mods` directory — the last gate between untrusted bytes and the
/// instance.
pub fn verify_file_sha256(
    path: &Path,
    expected_sha256: &str,
) -> Result<Sha256Verification, ModrinthSyncError> {
    let expected =
        normalize_sha256_hex(expected_sha256).map_err(|_| ModrinthSyncError::InvalidHash {
            field: "sha256",
            value: expected_sha256.to_owned(),
        })?;
    let actual = sha256_file(path)?;
    Ok(if actual == expected {
        Sha256Verification::Match
    } else {
        Sha256Verification::Mismatch { expected, actual }
    })
}

// ---------------------------------------------------------------- errors

/// Every failure mode of this module. Validation rejections carry the
/// offending value so callers can surface it in the consent dialog.
#[derive(Debug)]
pub enum ModrinthSyncError {
    Json(JsonError),
    Io(io::Error),
    MissingField(&'static str),
    WrongType(&'static str),
    /// `formatVersion` was present but not the supported v1.
    UnsupportedFormatVersion(u64),
    /// `game` was present but not `"minecraft"`.
    NotMinecraftGame(String),
    /// A hash field was not the required-length lowercase hex.
    InvalidHash {
        field: &'static str,
        value: String,
    },
    InvalidSlug(String),
    /// A `files[].path` failed [`validate_mrpack_path`].
    UnsafePath {
        path: String,
        reason: PathRejection,
    },
    /// A download URL failed [`validate_download_url`].
    UntrustedDownloadUrl {
        url: String,
        reason: UrlTrustError,
    },
    InvalidEnvSupport(String),
    InvalidDependencyKind(String),
}

impl fmt::Display for ModrinthSyncError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ModrinthSyncError::Json(e) => write!(f, "{e}"),
            ModrinthSyncError::Io(e) => write!(f, "{e}"),
            ModrinthSyncError::MissingField(k) => write!(f, "missing field `{k}`"),
            ModrinthSyncError::WrongType(k) => write!(f, "field `{k}` has the wrong type"),
            ModrinthSyncError::UnsupportedFormatVersion(v) => {
                write!(f, "unsupported formatVersion {v}")
            }
            ModrinthSyncError::NotMinecraftGame(g) => {
                write!(f, "game is `{g}`, not `minecraft`")
            }
            ModrinthSyncError::InvalidHash { field, value } => {
                write!(f, "field `{field}` is not valid hex: `{value}`")
            }
            ModrinthSyncError::InvalidSlug(s) => write!(f, "invalid modrinth slug `{s}`"),
            ModrinthSyncError::UnsafePath { path, reason } => {
                write!(f, "unsafe path `{path}`: {reason}")
            }
            ModrinthSyncError::UntrustedDownloadUrl { url, reason } => {
                write!(f, "untrusted download url `{url}`: {reason}")
            }
            ModrinthSyncError::InvalidEnvSupport(v) => {
                write!(f, "invalid env support tier `{v}`")
            }
            ModrinthSyncError::InvalidDependencyKind(v) => {
                write!(f, "invalid dependency kind `{v}`")
            }
        }
    }
}
impl std::error::Error for ModrinthSyncError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ModrinthSyncError::Json(e) => Some(e),
            ModrinthSyncError::Io(e) => Some(e),
            _ => None,
        }
    }
}
impl From<JsonError> for ModrinthSyncError {
    fn from(e: JsonError) -> Self {
        ModrinthSyncError::Json(e)
    }
}
impl From<io::Error> for ModrinthSyncError {
    fn from(e: io::Error) -> Self {
        ModrinthSyncError::Io(e)
    }
}

// ---------------------------------------------------------------- helpers

fn required_str<'a>(json: &'a Json, key: &'static str) -> Result<&'a str, ModrinthSyncError> {
    match json.get(key) {
        None => Err(ModrinthSyncError::MissingField(key)),
        Some(v) => v.as_str().ok_or(ModrinthSyncError::WrongType(key)),
    }
}

fn required_u64(json: &Json, key: &'static str) -> Result<u64, ModrinthSyncError> {
    match json.get(key) {
        None => Err(ModrinthSyncError::MissingField(key)),
        Some(v) => v.as_u64().ok_or(ModrinthSyncError::WrongType(key)),
    }
}

fn required_array<'a>(json: &'a Json, key: &'static str) -> Result<&'a [Json], ModrinthSyncError> {
    match json.get(key) {
        None => Err(ModrinthSyncError::MissingField(key)),
        Some(v) => v.as_array().ok_or(ModrinthSyncError::WrongType(key)),
    }
}

fn is_hex_of_len(value: &str, len: usize) -> bool {
    value.len() == len && value.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Read `key` as exactly `len` hex digits, returned lowercase.
fn require_hex(json: &Json, key: &'static str, len: usize) -> Result<String, ModrinthSyncError> {
    let value = required_str(json, key)?;
    if is_hex_of_len(value, len) {
        Ok(value.to_ascii_lowercase())
    } else {
        Err(ModrinthSyncError::InvalidHash {
            field: key,
            value: value.to_owned(),
        })
    }
}
