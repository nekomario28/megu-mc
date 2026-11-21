// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later

//! Missing mod-dependency UX engine (Lane UX-DEPS).
//!
//! Two cooperating halves, both pure-`std` and fail-closed:
//!
//! * **Auto-fetch planning** — [`MissingDepQuery`] turns a
//!   [`crate::ecosystem_compat::MissingDependency`] into an exact Modrinth
//!   v2 `/project/{slug}/version` URL; [`parse_modrinth_versions_json`]
//!   reduces the real API response to one verified
//!   [`ResolvedModrinthArtifact`] whose download URL must pass
//!   [`crate::mod_sync::validate_download_url`] (HTTPS + trusted-CDN
//!   allowlist) and whose file name must pass
//!   [`crate::mod_sync::validate_jar_file_name`]. Nothing is ever
//!   downloaded implicitly: [`install_resolved_artifact`] requires a
//!   [`crate::mod_sync::ConsentToken`] with `user_approved == true`,
//!   re-verifies size + SHA-256/SHA-512 in constant time, and materializes
//!   through [`crate::mod_sync::ContentAddressedModCache`] so the JAR lands
//!   content-addressed and atomic.
//! * **Hot-reload planning** — [`assess_hot_reload_after_add`] classifies a
//!   newly dropped/fetched JAR as [`ReloadRequirement::HotReloadable`]
//!   (datapack/recipe/tag/culinary data, `mc-spine-mod-bundle/1` documents,
//!   or `.class` files where *every* method verifies under the bounded
//!   [`crate::bytecode_subset_vm`] subset) or
//!   [`ReloadRequirement::RestartRequired`] (Mixin bytecode, native
//!   libraries, corrupt manifests, or any bytecode outside the verified
//!   subset — deep Tier-3 / registry-mutating code).
//!
//! [`plan_dep_recovery`] merges both halves into an operator-facing
//! EN/JA summary: which missing dependencies have a verified auto-fetch
//! candidate, which need a manual drop-in, and the exact
//! `Drop JAR into mods/ and run /mods reload` instruction.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

use crate::bytecode_subset_vm::{parse_class_methods, verify_subset_method};
use crate::class_mixin_scanner::scan_jar_mixins;
use crate::data_loader::{parse_json, DataLoadError, JsonValue};
use crate::ecosystem_compat::{
    scan_mod_jar_named, DualLoaderPlan, MissingDependency, ModJarDescriptor, ModLoaderKind,
};
use crate::mixin_lifter::MixinLiftVerdict;
use crate::mod_sync::{
    self, ConsentToken, ContentAddressedModCache, ModSyncEntry, ModSyncError, SyncPolicy,
    MAX_MOD_JAR_BYTES,
};
use crate::modpack;
use crate::unified_bundle::BUNDLE_SCHEMA;
use crate::version_compat::{McVersion, VersionRange};

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Fail-closed error for every dep-resolver check. Response-level problems
/// reject the whole parse; per-file/per-version problems simply make that
/// candidate ineligible (skipped, never partially trusted).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DepResolveError {
    /// The response body was not valid JSON, or its root was not a
    /// version array.
    Json(String),
    /// A dependency mod id failed the `[a-z0-9_-]{1,64}` validation.
    InvalidModId(String),
    /// `target_mc` did not parse as a dotted numeric version.
    InvalidTargetVersion(String),
    /// The declared dependency version range did not parse.
    InvalidRange { declared: String, reason: String },
    /// Downloaded byte length differed from the artifact's `size_bytes`.
    SizeMismatch { expected: u64, actual: u64 },
    /// Downloaded bytes failed the recorded digest; names the algorithm.
    DigestMismatch(&'static str),
    /// The artifact carries no usable sha256/sha512 digest (or a stored
    /// digest is malformed) to verify downloaded bytes against.
    DigestUnavailable,
    /// An underlying [`crate::mod_sync`] check failed during install.
    Install(ModSyncError),
    /// [`install_resolved_artifact`] requires `ConsentToken::approved()`.
    ConsentRequired,
}

impl DepResolveError {
    /// Stable operator-facing reason string.
    pub fn reason(&self) -> String {
        match self {
            Self::Json(m) => format!("invalid Modrinth response: {m}"),
            Self::InvalidModId(id) => {
                format!("mod id '{id}' is not a valid [a-z0-9_-]{{1,64}} id")
            }
            Self::InvalidTargetVersion(v) => {
                format!("target Minecraft version '{v}' is not a dotted numeric version")
            }
            Self::InvalidRange { declared, reason } => {
                format!("declared range '{declared}' does not parse: {reason}")
            }
            Self::SizeMismatch { expected, actual } => {
                format!("downloaded {actual} bytes, artifact declares {expected}")
            }
            Self::DigestMismatch(algo) => format!("downloaded bytes failed {algo} verification"),
            Self::DigestUnavailable => {
                "artifact has no usable sha256/sha512 digest to verify against".to_string()
            }
            Self::Install(e) => format!("mod-sync install failed: {}", e.reason()),
            Self::ConsentRequired => {
                "explicit user consent is required before install".to_string()
            }
        }
    }
}

impl fmt::Display for DepResolveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.reason())
    }
}

impl std::error::Error for DepResolveError {}

// ---------------------------------------------------------------------------
// Pure-std FIPS 180-4 SHA-512 — Modrinth publishes sha512 by default while
// the content-addressed cache keys on sha256, so downloaded bytes must be
// verifiable against either before anything is written.
// ---------------------------------------------------------------------------

const SHA512_H0: [u64; 8] = [
    0x6a09e667f3bcc908,
    0xbb67ae8584caa73b,
    0x3c6ef372fe94f82b,
    0xa54ff53a5f1d36f1,
    0x510e527fade682d1,
    0x9b05688c2b3e6c1f,
    0x1f83d9abfb41bd6b,
    0x5be0cd19137e2179,
];

const SHA512_K: [u64; 80] = [
    0x428a2f98d728ae22, 0x7137449123ef65cd, 0xb5c0fbcfec4d3b2f, 0xe9b5dba58189dbbc,
    0x3956c25bf348b538, 0x59f111f1b605d019, 0x923f82a4af194f9b, 0xab1c5ed5da6d8118,
    0xd807aa98a3030242, 0x12835b0145706fbe, 0x243185be4ee4b28c, 0x550c7dc3d5ffb4e2,
    0x72be5d74f27b896f, 0x80deb1fe3b1696b1, 0x9bdc06a725c71235, 0xc19bf174cf692694,
    0xe49b69c19ef14ad2, 0xefbe4786384f25e3, 0x0fc19dc68b8cd5b5, 0x240ca1cc77ac9c65,
    0x2de92c6f592b0275, 0x4a7484aa6ea6e483, 0x5cb0a9dcbd41fbd4, 0x76f988da831153b5,
    0x983e5152ee66dfab, 0xa831c66d2db43210, 0xb00327c898fb213f, 0xbf597fc7beef0ee4,
    0xc6e00bf33da88fc2, 0xd5a79147930aa725, 0x06ca6351e003826f, 0x142929670a0e6e70,
    0x27b70a8546d22ffc, 0x2e1b21385c26c926, 0x4d2c6dfc5ac42aed, 0x53380d139d95b3df,
    0x650a73548baf63de, 0x766a0abb3c77b2a8, 0x81c2c92e47edaee6, 0x92722c851482353b,
    0xa2bfe8a14cf10364, 0xa81a664bbc423001, 0xc24b8b70d0f89791, 0xc76c51a30654be30,
    0xd192e819d6ef5218, 0xd69906245565a910, 0xf40e35855771202a, 0x106aa07032bbd1b8,
    0x19a4c116b8d2d0c8, 0x1e376c085141ab53, 0x2748774cdf8eeb99, 0x34b0bcb5e19b48a8,
    0x391c0cb3c5c95a63, 0x4ed8aa4ae3418acb, 0x5b9cca4f7763e373, 0x682e6ff3d6b2b8a3,
    0x748f82ee5defb2fc, 0x78a5636f43172f60, 0x84c87814a1f0ab72, 0x8cc702081a6439ec,
    0x90befffa23631e28, 0xa4506cebde82bde9, 0xbef9a3f7b2c67915, 0xc67178f2e372532b,
    0xca273eceea26619c, 0xd186b8c721c0c207, 0xeada7dd6cde0eb1e, 0xf57d4f7fee6ed178,
    0x06f067aa72176fba, 0x0a637dc5a2c898a6, 0x113f9804bef90dae, 0x1b710b35131c471b,
    0x28db77f523047d84, 0x32caab7b40c72493, 0x3c9ebe0a15c9bebc, 0x431d67c49c100d4c,
    0x4cc5d4becb3e42b6, 0x597f299cfc657e2a, 0x5fcb6fab3ad6faec, 0x6c44198c4a475817,
];

fn sha512_compress(h: &mut [u64; 8], block: &[u8]) {
    debug_assert_eq!(block.len(), 128);
    let mut w = [0u64; 80];
    for i in 0..16 {
        w[i] = u64::from_be_bytes([
            block[8 * i],
            block[8 * i + 1],
            block[8 * i + 2],
            block[8 * i + 3],
            block[8 * i + 4],
            block[8 * i + 5],
            block[8 * i + 6],
            block[8 * i + 7],
        ]);
    }
    for i in 16..80 {
        let s0 = w[i - 15].rotate_right(1) ^ w[i - 15].rotate_right(8) ^ (w[i - 15] >> 7);
        let s1 = w[i - 2].rotate_right(19) ^ w[i - 2].rotate_right(61) ^ (w[i - 2] >> 6);
        w[i] = w[i - 16]
            .wrapping_add(s0)
            .wrapping_add(w[i - 7])
            .wrapping_add(s1);
    }
    let (mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh) =
        (h[0], h[1], h[2], h[3], h[4], h[5], h[6], h[7]);
    for i in 0..80 {
        let s1 = e.rotate_right(14) ^ e.rotate_right(18) ^ e.rotate_right(41);
        let ch = (e & f) ^ ((!e) & g);
        let t1 = hh
            .wrapping_add(s1)
            .wrapping_add(ch)
            .wrapping_add(SHA512_K[i])
            .wrapping_add(w[i]);
        let s0 = a.rotate_right(28) ^ a.rotate_right(34) ^ a.rotate_right(39);
        let maj = (a & b) ^ (a & c) ^ (b & c);
        let t2 = s0.wrapping_add(maj);
        hh = g;
        g = f;
        f = e;
        e = d.wrapping_add(t1);
        d = c;
        c = b;
        b = a;
        a = t1.wrapping_add(t2);
    }
    h[0] = h[0].wrapping_add(a);
    h[1] = h[1].wrapping_add(b);
    h[2] = h[2].wrapping_add(c);
    h[3] = h[3].wrapping_add(d);
    h[4] = h[4].wrapping_add(e);
    h[5] = h[5].wrapping_add(f);
    h[6] = h[6].wrapping_add(g);
    h[7] = h[7].wrapping_add(hh);
}

/// One-shot SHA-512 digest of `data` (FIPS 180-4). Compresses every full
/// 128-byte block directly from the input, then pads the tail
/// (`0x80`, zeros to the 112-byte boundary, 128-bit big-endian bit length).
fn sha512(data: &[u8]) -> [u8; 64] {
    const BLOCK: usize = 128;
    let mut h = SHA512_H0;
    let mut total_len = 0u64;
    let mut rest = data;

    while rest.len() >= BLOCK {
        total_len = total_len.wrapping_add(BLOCK as u64);
        sha512_compress(&mut h, &rest[..BLOCK]);
        rest = &rest[BLOCK..];
    }
    total_len = total_len.wrapping_add(rest.len() as u64);

    // Tail buffer: rest || 0x80 || zeros || 16-byte bit length. At most two
    // blocks (rest < 128 bytes ⇒ used <= 128, zeros <= 127, +16 <= 256... the
    // exact bound is len_at + 16 <= 256).
    let mut tail = [0u8; BLOCK * 2];
    tail[..rest.len()].copy_from_slice(rest);
    tail[rest.len()] = 0x80;
    let used = rest.len() + 1;
    let zeros = (112 + BLOCK - (used % BLOCK)) % BLOCK;
    let len_at = used + zeros;
    let bit_len = (total_len as u128).wrapping_mul(8);
    tail[len_at..len_at + 16].copy_from_slice(&bit_len.to_be_bytes());
    let tail_len = len_at + 16;
    debug_assert_eq!(tail_len % BLOCK, 0);
    let (chunks, _) = tail[..tail_len].as_chunks::<BLOCK>();
    for chunk in chunks {
        sha512_compress(&mut h, chunk);
    }

    let mut out = [0u8; 64];
    for (i, word) in h.iter().enumerate() {
        out[8 * i..8 * i + 8].copy_from_slice(&word.to_be_bytes());
    }
    out
}

/// Parse a strict 128-hex-char sha512 digest (either case). `None` for any
/// other length or non-hex character — fail closed.
fn parse_sha512_hex(text: &str) -> Option<[u8; 64]> {
    if text.len() != 128 || !text.is_ascii() {
        return None;
    }
    let mut out = [0u8; 64];
    let bytes = text.as_bytes();
    for i in 0..64 {
        let hi = (bytes[2 * i] as char).to_digit(16)?;
        let lo = (bytes[2 * i + 1] as char).to_digit(16)?;
        out[i] = ((hi << 4) | lo) as u8;
    }
    Some(out)
}

/// Constant-time equality of two 64-byte digests — never short-circuits on
/// a differing byte.
fn digest_eq_64(a: &[u8; 64], b: &[u8; 64]) -> bool {
    let mut diff = 0u8;
    for i in 0..64 {
        diff |= a[i] ^ b[i];
    }
    diff == 0
}

// ---------------------------------------------------------------------------
// Modrinth loader mapping
// ---------------------------------------------------------------------------

/// Modrinth `loaders` filter value for one loader ecosystem.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum ModrinthLoader {
    Fabric,
    NeoForge,
    Forge,
}

impl ModrinthLoader {
    /// The exact string Modrinth's `loaders` filter expects.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Fabric => "fabric",
            Self::NeoForge => "neoforge",
            Self::Forge => "forge",
        }
    }
}

impl fmt::Display for ModrinthLoader {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Modrinth API base used by [`MissingDepQuery::query_url`].
pub const MODRINTH_API_BASE: &str = "https://api.modrinth.com/v2";

/// Map the plan's [`ModLoaderKind`] preferences onto Modrinth loader names,
/// deduplicated in first-seen order. `DualFabricNeoForge` contributes both
/// `fabric` and `neoforge`; `DatapackOnly` contributes nothing. An empty
/// result defaults to `[Fabric, NeoForge]` — mc-spine's dual-loader world —
/// so the query always carries an explicit loader filter.
fn modrinth_loaders_for(preferred: &[ModLoaderKind]) -> Vec<ModrinthLoader> {
    let mut out: Vec<ModrinthLoader> = Vec::new();
    for kind in preferred {
        let mapped: &[ModrinthLoader] = match kind {
            ModLoaderKind::Fabric => &[ModrinthLoader::Fabric],
            ModLoaderKind::NeoForge => &[ModrinthLoader::NeoForge],
            ModLoaderKind::ForgeLegacy => &[ModrinthLoader::Forge],
            ModLoaderKind::DualFabricNeoForge => {
                &[ModrinthLoader::Fabric, ModrinthLoader::NeoForge]
            }
            ModLoaderKind::DatapackOnly => &[],
        };
        for &l in mapped {
            if !out.contains(&l) {
                out.push(l);
            }
        }
    }
    if out.is_empty() {
        out.push(ModrinthLoader::Fabric);
        out.push(ModrinthLoader::NeoForge);
    }
    out
}

// ---------------------------------------------------------------------------
// MissingDepQuery — exact Modrinth v2 query URL for one missing dependency
// ---------------------------------------------------------------------------

/// `[a-z0-9_-]{1,64}` — the same charset `ecosystem_compat` enforces on
/// `fabric.mod.json` ids and the charset Modrinth slugs use.
fn is_valid_dep_mod_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id.bytes()
            .all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'_' | b'-'))
}

/// RFC 3986 unreserved characters pass through; every other byte is
/// percent-encoded uppercase. Applied to the slug and to the JSON array
/// parameters so the emitted URL is always safe to hand to an HTTP client.
fn percent_encode(input: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = String::with_capacity(input.len());
    for &b in input.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => {
                out.push('%');
                out.push(HEX[(b >> 4) as usize] as char);
                out.push(HEX[(b & 0x0F) as usize] as char);
            }
        }
    }
    out
}

/// `["a","b"]` JSON array literal with minimal string escaping — the exact
/// shape Modrinth's query filters expect before percent-encoding.
fn json_string_array(items: &[&str]) -> String {
    let mut out = String::from("[");
    for (i, item) in items.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push('"');
        for ch in item.chars() {
            match ch {
                '"' => out.push_str("\\\""),
                '\\' => out.push_str("\\\\"),
                c if (c as u32) < 0x20 => {
                    out.push_str(&format!("\\u{:04x}", c as u32));
                }
                c => out.push(c),
            }
        }
        out.push('"');
    }
    out.push(']');
    out
}

/// A fully validated, ready-to-fetch Modrinth version query for one
/// [`MissingDependency`]. Build via
/// [`MissingDepQuery::from_missing_dependency`]; the mod id, target
/// version, and declared range are all validated up front so
/// [`Self::query_url`] can never emit a malformed or unsafe request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MissingDepQuery {
    /// Mod that declared the dependency (`MissingDependency::mod_id`).
    pub requiring_mod_id: String,
    /// The missing mod id (`MissingDependency::required_mod_id`) — also
    /// the Modrinth project slug path segment.
    pub required_mod_id: String,
    /// The raw declared range string, when the manifest carried one.
    pub declared_range: Option<String>,
    /// The parsed declared range (`*` when undeclared) — matched against
    /// each candidate's `version_number`.
    pub required_range: VersionRange,
    /// Exact `game_versions` filter string (e.g. `"1.21.1"`), matched
    /// verbatim against Modrinth's `game_versions` entries.
    pub target_mc_tag: String,
    /// Preferred loader filters, deduplicated, never empty.
    pub loaders: Vec<ModrinthLoader>,
}

impl MissingDepQuery {
    /// Validate `dep` and bind it to `target_mc` + `preferred_loaders`.
    ///
    /// `target_mc` is the exact Modrinth `game_versions` tag (e.g.
    /// `"1.21.1"`, `"1.21"`) — it must parse as a dotted numeric version but
    /// is matched verbatim, never normalized (`"1.21"` stays `"1.21"`, not
    /// `"1.21.0"`). `preferred_loaders` maps through
    /// [`modrinth_loaders_for`].
    pub fn from_missing_dependency(
        dep: &MissingDependency,
        target_mc: &str,
        preferred_loaders: &[ModLoaderKind],
    ) -> Result<Self, DepResolveError> {
        if !is_valid_dep_mod_id(&dep.required_mod_id) {
            return Err(DepResolveError::InvalidModId(dep.required_mod_id.clone()));
        }
        if dep.mod_id.trim().is_empty() {
            return Err(DepResolveError::InvalidModId(dep.mod_id.clone()));
        }
        let tag = target_mc.trim();
        if tag.is_empty() || McVersion::parse(tag).is_none() {
            return Err(DepResolveError::InvalidTargetVersion(
                target_mc.to_string(),
            ));
        }
        let required_range = match dep.version_range.as_deref() {
            Some(declared) => VersionRange::parse(declared).map_err(|e| {
                DepResolveError::InvalidRange {
                    declared: declared.to_string(),
                    reason: e.reason().to_string(),
                }
            })?,
            None => VersionRange::parse("*").map_err(|e| DepResolveError::InvalidRange {
                declared: "*".to_string(),
                reason: e.reason().to_string(),
            })?,
        };
        Ok(Self {
            requiring_mod_id: dep.mod_id.clone(),
            required_mod_id: dep.required_mod_id.clone(),
            declared_range: dep.version_range.clone(),
            required_range,
            target_mc_tag: tag.to_string(),
            loaders: modrinth_loaders_for(preferred_loaders),
        })
    }

    /// The exact Modrinth v2 versions URL, e.g.
    /// `https://api.modrinth.com/v2/project/sodium/version?game_versions=%5B%221.21.1%22%5D&loaders=%5B%22fabric%22%2C%22neoforge%22%5D`.
    /// The slug and both JSON-array parameters are strictly
    /// percent-encoded — docs show the bare `[`/`"`/`,` form, but a real
    /// HTTP client requires this encoding.
    pub fn query_url(&self) -> String {
        let game_versions = json_string_array(&[self.target_mc_tag.as_str()]);
        let loader_names: Vec<&str> = self.loaders.iter().map(|l| l.as_str()).collect();
        let loaders_json = json_string_array(&loader_names);
        format!(
            "{MODRINTH_API_BASE}/project/{}/version?game_versions={}&loaders={}",
            percent_encode(&self.required_mod_id),
            percent_encode(&game_versions),
            percent_encode(&loaders_json),
        )
    }
}

// ---------------------------------------------------------------------------
// ResolvedModrinthArtifact — one verified auto-fetch candidate
// ---------------------------------------------------------------------------

/// A single Modrinth release selected as the auto-fetch candidate for one
/// missing dependency: exact file identity, trusted download URL, declared
/// size, and at least one usable digest (sha256 preferred, sha512 accepted —
/// Modrinth publishes sha512 by default).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedModrinthArtifact {
    /// Modrinth project slug this artifact was resolved for — the
    /// `/project/<slug>/` path segment. Stamped by [`resolve_for_query`];
    /// empty for bare [`parse_modrinth_versions_json`] results.
    pub project_slug: String,
    /// Modrinth `project_id`, when the response carried one.
    pub project_id: Option<String>,
    /// Version `id` from the response.
    pub version_id: String,
    /// Published `version_number`.
    pub version_number: String,
    /// Published `version_type` (`release`/`beta`/`alpha`/other).
    pub version_type: String,
    /// Chosen file's `filename` — already passed the `.jar` basename jail.
    pub file_name: String,
    /// The chosen file's `primary` flag.
    pub primary: bool,
    /// The chosen file's `file_type` (`required_mod`, `optional_mod`,
    /// `required_resource`, `optional_resource`, or absent).
    pub file_type: String,
    /// `https` download URL on a trusted CDN — already passed
    /// [`mod_sync::validate_download_url`] under the canonical policy.
    pub download_url: String,
    /// Declared JAR byte length, `1..=MAX_MOD_JAR_BYTES`.
    pub size_bytes: u64,
    /// `hashes.sha256` (64-hex), preferred for verification and the
    /// content-addressed cache key.
    pub sha256_hex: Option<String>,
    /// `hashes.sha512` (128-hex) — Modrinth's default published digest.
    pub sha512_hex: Option<String>,
    /// `hashes.sha1` (40-hex), informational only — never sufficient alone.
    pub sha1_hex: Option<String>,
}

impl ResolvedModrinthArtifact {
    /// Re-verify downloaded `bytes` against this artifact: exact size, then
    /// sha256 when present else sha512, constant-time. sha1 is never
    /// accepted as the sole check.
    pub fn verify_downloaded_bytes(&self, bytes: &[u8]) -> Result<(), DepResolveError> {
        if bytes.len() as u64 != self.size_bytes {
            return Err(DepResolveError::SizeMismatch {
                expected: self.size_bytes,
                actual: bytes.len() as u64,
            });
        }
        if let Some(expected_hex) = &self.sha256_hex {
            let expected =
                mod_sync::parse_sha256_hex(expected_hex).ok_or(DepResolveError::DigestUnavailable)?;
            if !mod_sync::digest_eq(&mod_sync::sha256(bytes), &expected) {
                return Err(DepResolveError::DigestMismatch("sha256"));
            }
            return Ok(());
        }
        if let Some(expected_hex) = &self.sha512_hex {
            let expected =
                parse_sha512_hex(expected_hex).ok_or(DepResolveError::DigestUnavailable)?;
            if !digest_eq_64(&sha512(bytes), &expected) {
                return Err(DepResolveError::DigestMismatch("sha512"));
            }
            return Ok(());
        }
        Err(DepResolveError::DigestUnavailable)
    }

    /// A [`ModSyncEntry`] view for callers that want to feed the result
    /// into [`crate::mod_sync::plan_mod_sync`]. Requires `sha256_hex` —
    /// artifacts carrying only sha512 cannot form a sync entry (`None`).
    pub fn to_sync_entry(&self) -> Option<ModSyncEntry> {
        let sha256_hex = self.sha256_hex.clone()?;
        Some(ModSyncEntry {
            mod_id: self.project_slug.clone(),
            file_name: self.file_name.clone(),
            download_url: self.download_url.clone(),
            sha256_hex,
            size_bytes: self.size_bytes,
        })
    }
}

/// Strict `len`-hex check used for the informational sha1 field.
fn is_hex_len(text: &str, len: usize) -> bool {
    text.len() == len && text.bytes().all(|b| b.is_ascii_hexdigit())
}

/// One file entry that survived every fail-closed check.
struct CandidateFile {
    file_name: String,
    download_url: String,
    file_type: String,
    size_bytes: u64,
    sha256_hex: Option<String>,
    sha512_hex: Option<String>,
    sha1_hex: Option<String>,
    primary: bool,
    file_type_rank: u8,
    index: usize,
}

/// Extract and validate one `files[]` element. Any deviation — missing
/// fields, unsafe URL, jail-breaking name, oversized/zero size, no usable
/// sha256/sha512 — makes the file ineligible (`None`), never an error.
fn extract_candidate_file(
    obj: &BTreeMap<String, JsonValue>,
    index: usize,
    policy: &SyncPolicy,
) -> Option<CandidateFile> {
    let file_name = obj.get("filename")?.as_str()?;
    mod_sync::validate_jar_file_name(file_name).ok()?;
    let download_url = obj.get("url")?.as_str()?;
    mod_sync::validate_download_url(download_url, policy).ok()?;
    let size_bytes = obj.get("size")?.as_u64()?;
    if size_bytes == 0 || size_bytes > MAX_MOD_JAR_BYTES {
        return None;
    }
    let hashes = obj.get("hashes")?.as_object()?;
    let sha256_hex = hashes
        .get("sha256")
        .and_then(JsonValue::as_str)
        .filter(|s| mod_sync::parse_sha256_hex(s).is_some())
        .map(|s| s.to_ascii_lowercase());
    let sha512_hex = hashes
        .get("sha512")
        .and_then(JsonValue::as_str)
        .filter(|s| parse_sha512_hex(s).is_some())
        .map(|s| s.to_ascii_lowercase());
    let sha1_hex = hashes
        .get("sha1")
        .and_then(JsonValue::as_str)
        .filter(|s| is_hex_len(s, 40))
        .map(|s| s.to_ascii_lowercase());
    if sha256_hex.is_none() && sha512_hex.is_none() {
        return None;
    }
    let primary = obj
        .get("primary")
        .and_then(JsonValue::as_bool)
        .unwrap_or(false);
    let file_type = obj
        .get("file_type")
        .and_then(JsonValue::as_str)
        .unwrap_or("");
    let file_type_rank = match file_type {
        "required_mod" | "optional_mod" => 2,
        "required_resource" | "optional_resource" => 1,
        _ => 0,
    };
    Some(CandidateFile {
        file_name: file_name.to_string(),
        download_url: download_url.to_string(),
        file_type: file_type.to_string(),
        size_bytes,
        sha256_hex,
        sha512_hex,
        sha1_hex,
        primary,
        file_type_rank,
        index,
    })
}

/// Modrinth `version_type` ranking: lower is better.
fn version_type_rank(version_type: &str) -> u8 {
    match version_type {
        "release" => 0,
        "beta" => 1,
        "alpha" => 2,
        _ => 3,
    }
}

/// Parse a published `version_number` into an [`McVersion`] key plus a
/// `has_suffix` flag (prerelease/build metadata such as `-beta`, `+b1`).
///
/// Accepts `v`-prefixed and two-component numbers (`"1.2"` -> `1.2.0`);
/// the leading dotted-numeric run must be 1..=3 components. Anything else
/// (`"nightly"`, `"1.2.3.4"`, `""`) is `None` — the candidate is skipped
/// rather than guessed at.
fn artifact_version_key(version_number: &str) -> Option<(McVersion, bool)> {
    let s = version_number.trim();
    let s = s.strip_prefix(['v', 'V']).unwrap_or(s);
    let end = s
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(s.len());
    let run = &s[..end];
    let suffix = &s[end..];
    if run.is_empty() {
        return None;
    }
    let parts: Vec<&str> = run.split('.').collect();
    if parts.len() > 3 {
        return None;
    }
    let mut n = [0u16; 3];
    for (i, part) in parts.iter().enumerate() {
        if part.is_empty() || !part.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        n[i] = part.parse().ok()?;
    }
    Some((McVersion::new(n[0], n[1], n[2]), !suffix.is_empty()))
}

/// Loader-name set for matching a version's `loaders` array. Empty
/// `preferred` defaults to fabric+neoforge, mirroring
/// [`modrinth_loaders_for`].
fn preferred_loader_names(preferred: &[ModLoaderKind]) -> Vec<&'static str> {
    modrinth_loaders_for(preferred)
        .iter()
        .map(|l| l.as_str())
        .collect()
}

fn json_err(e: DataLoadError) -> String {
    match e {
        DataLoadError::Io(m) | DataLoadError::Json(m) | DataLoadError::Zip(m) => m,
    }
}

/// Parse a real Modrinth `GET /v2/project/{id}/version` JSON response and
/// select the best eligible release.
///
/// Eligibility (all fail-closed):
/// - `status == "listed"`;
/// - `game_versions` contains `target_mc` verbatim;
/// - `loaders` intersects `preferred_loaders` (empty preference defaults to
///   fabric+neoforge);
/// - `version_number` parses via [`artifact_version_key`] and satisfies
///   `required_range`;
/// - at least one `files[]` entry passes the URL allowlist, `.jar` name
///   jail, size bound, and carries a usable sha256 or sha512.
///
/// Ranking (deterministic): `version_type` release < beta < alpha < other;
/// then higher parsed `version_number`; then un-suffixed over suffixed;
/// then lexicographically greater `version_number`; then lowest `id`.
///
/// Returns `Ok(None)` when no version is eligible — including when every
/// candidate's file failed validation (untrusted URL, non-`.jar` name, no
/// digest). Only malformed JSON / non-array root / invalid `target_mc`
/// produce `Err`.
pub fn parse_modrinth_versions_json(
    json: &[u8],
    target_mc: &str,
    preferred_loaders: &[ModLoaderKind],
    required_range: &VersionRange,
) -> Result<Option<ResolvedModrinthArtifact>, DepResolveError> {
    let tag = target_mc.trim();
    if tag.is_empty() || McVersion::parse(tag).is_none() {
        return Err(DepResolveError::InvalidTargetVersion(
            target_mc.to_string(),
        ));
    }
    let loader_names = preferred_loader_names(preferred_loaders);
    let policy = SyncPolicy::canonical();

    let root = parse_json(json).map_err(|e| DepResolveError::Json(json_err(e)))?;
    let versions = root
        .as_array()
        .ok_or_else(|| DepResolveError::Json("root is not a version array".to_string()))?;

    struct Scored {
        artifact: ResolvedModrinthArtifact,
        type_rank: u8,
        version_key: McVersion,
        has_suffix: bool,
    }

    let mut scored: Vec<Scored> = Vec::new();
    for version in versions {
        let Some(obj) = version.as_object() else {
            continue;
        };
        // Only published releases are fetch candidates.
        if obj.get("status").and_then(JsonValue::as_str) != Some("listed") {
            continue;
        }
        let mc_match = obj
            .get("game_versions")
            .and_then(JsonValue::as_array)
            .is_some_and(|gv| gv.iter().any(|v| v.as_str() == Some(tag)));
        if !mc_match {
            continue;
        }
        let loader_match = obj
            .get("loaders")
            .and_then(JsonValue::as_array)
            .is_some_and(|ls| {
                ls.iter().filter_map(JsonValue::as_str).any(|l| {
                    loader_names.iter().any(|p| p.eq_ignore_ascii_case(l))
                })
            });
        if !loader_match {
            continue;
        }
        let Some(version_number) = obj.get("version_number").and_then(JsonValue::as_str) else {
            continue;
        };
        let Some(version_id) = obj.get("id").and_then(JsonValue::as_str) else {
            continue;
        };
        let Some((version_key, has_suffix)) = artifact_version_key(version_number) else {
            continue;
        };
        if !required_range.contains(version_key) {
            continue;
        }
        let Some(files) = obj.get("files").and_then(JsonValue::as_array) else {
            continue;
        };
        // Best file in this version: primary first, then mod file_type,
        // then earliest index.
        let mut best: Option<CandidateFile> = None;
        for (index, file) in files.iter().enumerate() {
            let Some(fobj) = file.as_object() else {
                continue;
            };
            let Some(candidate) = extract_candidate_file(fobj, index, &policy) else {
                continue;
            };
            let better = match &best {
                None => true,
                Some(b) => {
                    (candidate.primary, candidate.file_type_rank, usize::MAX - candidate.index)
                        > (b.primary, b.file_type_rank, usize::MAX - b.index)
                }
            };
            if better {
                best = Some(candidate);
            }
        }
        let Some(file) = best else {
            continue;
        };
        let version_type = obj
            .get("version_type")
            .and_then(JsonValue::as_str)
            .unwrap_or("");
        scored.push(Scored {
            artifact: ResolvedModrinthArtifact {
                project_slug: String::new(),
                project_id: obj
                    .get("project_id")
                    .and_then(JsonValue::as_str)
                    .map(str::to_string),
                version_id: version_id.to_string(),
                version_number: version_number.to_string(),
                version_type: version_type.to_string(),
                file_name: file.file_name,
                primary: file.primary,
                file_type: file.file_type,
                download_url: file.download_url,
                size_bytes: file.size_bytes,
                sha256_hex: file.sha256_hex,
                sha512_hex: file.sha512_hex,
                sha1_hex: file.sha1_hex,
            },
            type_rank: version_type_rank(version_type),
            version_key,
            has_suffix,
        });
    }

    scored.sort_by(|a, b| {
        a.type_rank
            .cmp(&b.type_rank)
            .then_with(|| b.version_key.cmp(&a.version_key))
            .then_with(|| a.has_suffix.cmp(&b.has_suffix))
            .then_with(|| b.artifact.version_number.cmp(&a.artifact.version_number))
            .then_with(|| a.artifact.version_id.cmp(&b.artifact.version_id))
    });
    Ok(scored.into_iter().next().map(|s| s.artifact))
}

/// [`parse_modrinth_versions_json`] bound to a [`MissingDepQuery`]: parses
/// with the query's target tag, loader set, and required range, then stamps
/// `project_slug` with the queried mod id so [`plan_dep_recovery`] can
/// match artifacts back to dependencies.
pub fn resolve_for_query(
    query: &MissingDepQuery,
    json: &[u8],
) -> Result<Option<ResolvedModrinthArtifact>, DepResolveError> {
    // The query already carries resolved Modrinth loaders; reconstruct the
    // equivalent ModLoaderKind slice losslessly (Fabric/NeoForge/Forge map
    // back 1:1; a dual preference arrives as two entries).
    let preferred: Vec<ModLoaderKind> = query
        .loaders
        .iter()
        .map(|l| match l {
            ModrinthLoader::Fabric => ModLoaderKind::Fabric,
            ModrinthLoader::NeoForge => ModLoaderKind::NeoForge,
            ModrinthLoader::Forge => ModLoaderKind::ForgeLegacy,
        })
        .collect();
    let mut artifact = parse_modrinth_versions_json(
        json,
        &query.target_mc_tag,
        &preferred,
        &query.required_range,
    )?;
    if let Some(a) = &mut artifact {
        a.project_slug = query.required_mod_id.clone();
    }
    Ok(artifact)
}

// ---------------------------------------------------------------------------
// plan_dep_recovery — EN/JA operator summary
// ---------------------------------------------------------------------------

/// Per-dependency recovery state inside a [`DepRecoveryPlan`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DepRecoveryStatus {
    /// A verified auto-fetch candidate exists; install still requires
    /// explicit [`ConsentToken`] approval.
    AutoFetchReady,
    /// The candidate's `file_name` already sits in `mods/` — nothing to
    /// fetch, just reload.
    AlreadyOnDisk,
    /// No verified candidate — manual drop-in required.
    ManualOnly,
    /// The dependency id or declared range failed validation — cannot even
    /// form a query; manual drop-in required.
    UnresolvableQuery,
}

/// One missing dependency plus its recovery verdict.
#[derive(Clone, Debug, PartialEq)]
pub struct DepRecoveryEntry {
    /// Mod that declared the dependency.
    pub requiring_mod_id: String,
    /// The missing mod id.
    pub required_mod_id: String,
    /// Raw declared range string, when present.
    pub declared_range: Option<String>,
    /// Exact Modrinth query URL (`None` when the query could not be built).
    pub query_url: Option<String>,
    /// Verified auto-fetch candidate, when one was resolved for this id.
    pub candidate: Option<ResolvedModrinthArtifact>,
    /// `candidate.file_name` already exists in `mods/`.
    pub already_on_disk: bool,
    pub status: DepRecoveryStatus,
    /// One-line English instruction for this dependency.
    pub instruction_en: String,
    /// One-line Japanese instruction for this dependency.
    pub instruction_ja: String,
}

/// The full recovery plan: structured entries plus ready-to-print EN and
/// JA summaries.
#[derive(Clone, Debug, PartialEq)]
pub struct DepRecoveryPlan {
    pub entries: Vec<DepRecoveryEntry>,
    /// English multi-line summary for operator logs/UI.
    pub summary_en: String,
    /// Japanese multi-line summary.
    pub summary_ja: String,
}

/// Per-status EN/JA instruction lines. `candidate` is only meaningful for
/// `AutoFetchReady`/`AlreadyOnDisk`.
fn recovery_instructions(
    status: DepRecoveryStatus,
    required_mod_id: &str,
    declared_range: &Option<String>,
    candidate: &Option<ResolvedModrinthArtifact>,
) -> (String, String) {
    let range_en = declared_range
        .as_deref()
        .map(|r| format!(" (declared '{r}')"))
        .unwrap_or_default();
    let range_ja = declared_range
        .as_deref()
        .map(|r| format!("（宣言 '{r}'）"))
        .unwrap_or_default();
    match status {
        DepRecoveryStatus::AutoFetchReady => {
            let c = candidate.as_ref().expect("AutoFetchReady carries a candidate");
            (
                format!(
                    "verified Modrinth candidate '{}' v{} ({} bytes) — approve the fetch prompt or drop the JAR into mods/ manually, then run '/mods reload'",
                    c.file_name, c.version_number, c.size_bytes
                ),
                format!(
                    "検証済み Modrinth 候補 '{}' v{}（{} バイト）— 取得プロンプトを承認するか JAR を手動で mods/ に配置し、'/mods reload' を実行してください",
                    c.file_name, c.version_number, c.size_bytes
                ),
            )
        }
        DepRecoveryStatus::AlreadyOnDisk => {
            let file = candidate
                .as_ref()
                .map(|c| c.file_name.as_str())
                .unwrap_or("the candidate JAR");
            (
                format!("'{file}' already present in mods/ — run '/mods reload' to apply it"),
                format!("'{file}' はすでに mods/ にあります — '/mods reload' で適用してください"),
            )
        }
        DepRecoveryStatus::ManualOnly => (
            format!(
                "no verified Modrinth candidate{range_en} — download the '{required_mod_id}' JAR manually, drop it into mods/, then run '/mods reload'"
            ),
            format!(
                "検証済みの Modrinth 候補がありません{range_ja} — '{required_mod_id}' の JAR を手動でダウンロードして mods/ に配置し、'/mods reload' を実行してください"
            ),
        ),
        DepRecoveryStatus::UnresolvableQuery => (
            format!(
                "dependency id/range failed validation{range_en} — cannot auto-fetch '{required_mod_id}'; drop a compatible JAR into mods/ manually, then run '/mods reload'"
            ),
            format!(
                "依存 ID/範囲が検証を通りません{range_ja} — '{required_mod_id}' を自動取得できません。互換性のある JAR を mods/ に手動で配置し、'/mods reload' を実行してください"
            ),
        ),
    }
}

/// Build the recovery plan for `dual_plan.missing_dependencies` against
/// `mods_dir` and the already-resolved `resolved_artifacts` (produced per
/// dependency via [`resolve_for_query`] or
/// [`parse_modrinth_versions_json`] — matching is on
/// `artifact.project_slug`, so bare-parse results without a stamped slug
/// never match).
///
/// The query target is [`McVersion::CURRENT`] with the dual-loader
/// `[Fabric, NeoForge]` preference — the runtime's own defaults. Callers
/// targeting another Minecraft version should build
/// [`MissingDepQuery`]s explicitly instead.
pub fn plan_dep_recovery(
    mods_dir: &Path,
    dual_plan: &DualLoaderPlan,
    resolved_artifacts: &[ResolvedModrinthArtifact],
) -> DepRecoveryPlan {
    let target_tag = McVersion::CURRENT.to_string();
    let loaders = [ModLoaderKind::Fabric, ModLoaderKind::NeoForge];

    let mut entries = Vec::with_capacity(dual_plan.missing_dependencies.len());
    for dep in &dual_plan.missing_dependencies {
        let mut entry = DepRecoveryEntry {
            requiring_mod_id: dep.mod_id.clone(),
            required_mod_id: dep.required_mod_id.clone(),
            declared_range: dep.version_range.clone(),
            query_url: None,
            candidate: None,
            already_on_disk: false,
            status: DepRecoveryStatus::ManualOnly,
            instruction_en: String::new(),
            instruction_ja: String::new(),
        };

        match MissingDepQuery::from_missing_dependency(dep, &target_tag, &loaders) {
            Err(_) => {
                entry.status = DepRecoveryStatus::UnresolvableQuery;
            }
            Ok(query) => {
                entry.query_url = Some(query.query_url());
                if let Some(candidate) = resolved_artifacts.iter().find(|a| {
                    a.project_slug.eq_ignore_ascii_case(&dep.required_mod_id)
                }) {
                    entry.candidate = Some(candidate.clone());
                    let on_disk = std::fs::metadata(mods_dir.join(&candidate.file_name))
                        .map(|m| m.is_file())
                        .unwrap_or(false);
                    entry.already_on_disk = on_disk;
                    entry.status = if on_disk {
                        DepRecoveryStatus::AlreadyOnDisk
                    } else {
                        DepRecoveryStatus::AutoFetchReady
                    };
                }
            }
        }

        let (en, ja) = recovery_instructions(
            entry.status,
            &entry.required_mod_id,
            &entry.declared_range,
            &entry.candidate,
        );
        entry.instruction_en = en;
        entry.instruction_ja = ja;
        entries.push(entry);
    }

    DepRecoveryPlan {
        summary_en: build_summary_en(&entries),
        summary_ja: build_summary_ja(&entries),
        entries,
    }
}

fn status_tag_en(status: DepRecoveryStatus) -> &'static str {
    match status {
        DepRecoveryStatus::AutoFetchReady => "AUTO-FETCH",
        DepRecoveryStatus::AlreadyOnDisk => "ON DISK",
        DepRecoveryStatus::ManualOnly => "MANUAL",
        DepRecoveryStatus::UnresolvableQuery => "MANUAL (invalid id/range)",
    }
}

fn build_summary_en(entries: &[DepRecoveryEntry]) -> String {
    if entries.is_empty() {
        return "No missing mod dependencies — every required mod is provided.".to_string();
    }
    let mut out = format!("Missing mod dependencies: {}\n", entries.len());
    for e in entries {
        let range = e
            .declared_range
            .as_deref()
            .map(|r| format!(" '{r}'"))
            .unwrap_or_default();
        out.push_str(&format!(
            "- '{}' required by '{}' (declared{range}): [{}] {}\n",
            e.required_mod_id,
            e.requiring_mod_id,
            status_tag_en(e.status),
            e.instruction_en
        ));
    }
    out
}

fn build_summary_ja(entries: &[DepRecoveryEntry]) -> String {
    if entries.is_empty() {
        return "不足している mod 依存関係はありません — 必要な mod はすべて提供されています。"
            .to_string();
    }
    let mut out = format!("不足している mod 依存関係: {} 件\n", entries.len());
    for e in entries {
        let range = e
            .declared_range
            .as_deref()
            .map(|r| format!(" '{r}'"))
            .unwrap_or_default();
        let tag = match e.status {
            DepRecoveryStatus::AutoFetchReady => "自動取得",
            DepRecoveryStatus::AlreadyOnDisk => "配置済み",
            DepRecoveryStatus::ManualOnly => "手動",
            DepRecoveryStatus::UnresolvableQuery => "手動（ID/範囲が不正）",
        };
        out.push_str(&format!(
            "- '{}' が必要とする '{}'（宣言{range}）: [{}] {}\n",
            e.requiring_mod_id, e.required_mod_id, tag, e.instruction_ja
        ));
    }
    out
}

// ---------------------------------------------------------------------------
// assess_hot_reload_after_add — restart-free hot-reload classifier
// ---------------------------------------------------------------------------

/// Whether a newly added JAR can be applied without a clean restart.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReloadRequirement {
    /// Safe to hot-apply: datapack/recipe/tag/culinary data,
    /// `mc-spine-mod-bundle/1` documents, and/or `.class` members where every
    /// method verifies under the bounded subset VM. `reasons` lists the
    /// positive evidence found.
    HotReloadable { reasons: Vec<String> },
    /// Needs a clean restart: Mixin bytecode, native libraries, a corrupt
    /// loader manifest, or any `.class` method outside the verified subset
    /// (deep Tier-3 code, registry mutations, reflection, IO, networking).
    /// `reasons` lists the decisive blockers.
    RestartRequired { reasons: Vec<String> },
}

impl ReloadRequirement {
    /// `true` for [`Self::HotReloadable`].
    pub fn is_hot_reloadable(&self) -> bool {
        matches!(self, Self::HotReloadable { .. })
    }

    /// The evidence/decisive reasons.
    pub fn reasons(&self) -> &[String] {
        match self {
            Self::HotReloadable { reasons } | Self::RestartRequired { reasons } => reasons,
        }
    }
}

/// `.class` members one assessment may parse/verify.
const MAX_HOT_SCAN_CLASSES: usize = 4096;
/// Cumulative decompressed `.class` bytes one assessment may read.
const MAX_HOT_SCAN_BYTES: u64 = 256 * 1024 * 1024;
/// `.json` members inspected for `mc-spine-mod-bundle/1` schema tags.
const MAX_BUNDLE_SCAN_MEMBERS: usize = 128;
/// Cumulative `.json` bytes inspected for bundle schema tags.
const MAX_BUNDLE_SCAN_BYTES: u64 = 64 * 1024 * 1024;
/// Schema marker pre-filter — cheaper than parsing every JSON member.
const BUNDLE_SCHEMA_MARKER: &[u8] = BUNDLE_SCHEMA.as_bytes();

fn is_native_library_name(name: &str) -> bool {
    const NATIVE_SUFFIXES: &[&str] = &[".so", ".dll", ".dylib", ".jnilib"];
    NATIVE_SUFFIXES.iter().any(|s| {
        name.len() >= s.len() && name[name.len() - s.len()..].eq_ignore_ascii_case(s)
    })
}

/// Whether a `.json` member name is plausible bundle-document territory:
/// archive-root JSON, or any path containing "bundle". Deep datapack JSON
/// (`data/<ns>/recipes/...`) is skipped — it is datapack evidence anyway.
fn is_bundle_candidate_name(name: &str) -> bool {
    if !name.ends_with(".json") {
        return false;
    }
    if !name.contains('/') {
        return true;
    }
    name.to_ascii_lowercase().contains("bundle")
}

/// Count `mc-spine-mod-bundle/1` documents among plausible `.json` members.
/// Detection is by schema tag only — the runtime fully validates bundle
/// contents at install time; for hot-reload classification the document is
/// pure data either way.
fn count_bundle_members(entries: &[modpack::ZipMemberEntry], jar_bytes: &[u8]) -> usize {
    let mut found = 0usize;
    let mut scanned = 0usize;
    let mut bytes_read = 0u64;
    for entry in entries {
        if !is_bundle_candidate_name(&entry.name) {
            continue;
        }
        scanned += 1;
        if scanned > MAX_BUNDLE_SCAN_MEMBERS || bytes_read > MAX_BUNDLE_SCAN_BYTES {
            break;
        }
        let Ok(bytes) = modpack::read_zip_member(jar_bytes, entry, "added JAR") else {
            continue;
        };
        bytes_read = bytes_read.saturating_add(bytes.len() as u64);
        if bytes_read > MAX_BUNDLE_SCAN_BYTES {
            break;
        }
        // Cheap substring pre-filter before paying for a JSON parse.
        if !bytes
            .windows(BUNDLE_SCHEMA_MARKER.len())
            .any(|w| w == BUNDLE_SCHEMA_MARKER)
        {
            continue;
        }
        let Ok(root) = parse_json(&bytes) else {
            continue;
        };
        let is_bundle = root
            .as_object()
            .and_then(|o| o.get("schema"))
            .and_then(JsonValue::as_str)
            == Some(BUNDLE_SCHEMA);
        if is_bundle {
            found += 1;
        }
    }
    found
}

/// Classify a newly added JAR for restart-free hot reload.
///
/// Decision order (any restart blocker wins):
/// 1. Archive must list as a readable ZIP/JAR, else `RestartRequired`.
/// 2. A present-but-corrupt loader manifest fails the fail-closed
///    [`scan_mod_jar_named`] scan → `RestartRequired`.
/// 3. Mixin evidence — manifest-declared configs, any `*.mixins.json`
///    member, or any site/config found by [`scan_jar_mixins`] (an
///    unscannable mixin surface also fails closed) → `RestartRequired`.
///    Mixin application is startup-time work regardless of lift verdict.
/// 4. Native library members (`.so`/`.dll`/`.dylib`/`.jnilib`) →
///    `RestartRequired`.
/// 5. Every `.class` member (including `META-INF/versions/` multi-release
///    classes) must parse via [`parse_class_methods`] and have *every*
///    method pass [`verify_subset_method`]; any failure →
///    `RestartRequired`. This is what catches deep Tier-3 code and
///    registry-mutating entrypoints — their host calls are outside the
///    allowlist.
/// 6. Positive evidence — datapack `data/`+`pack.mcmeta`, bundle schema
///    documents, verified class count — lands in `HotReloadable.reasons`;
///    an archive with no executable/data surface at all is trivially
///    hot-reloadable (it is inert in `mods/`).
pub fn assess_hot_reload_after_add(added_jar_bytes: &[u8]) -> ReloadRequirement {
    let mut hot: Vec<String> = Vec::new();
    let mut restart: Vec<String> = Vec::new();

    let entries = match modpack::zip_member_list(added_jar_bytes, "added JAR") {
        Ok(e) => e,
        Err(e) => {
            return ReloadRequirement::RestartRequired {
                reasons: vec![format!(
                    "archive is not a readable JAR/ZIP: {}",
                    e.reason()
                )],
            };
        }
    };

    // Loader manifest — best effort. A present manifest that fails the
    // fail-closed scan is itself a restart blocker; a missing manifest is
    // fine (datapack/pure-class archives have none).
    let has_manifest_member = entries.iter().any(|e| {
        matches!(
            e.name.as_str(),
            "fabric.mod.json" | "META-INF/neoforge.mods.toml" | "META-INF/mods.toml"
        )
    });
    let desc: Option<ModJarDescriptor> = match scan_mod_jar_named(added_jar_bytes, "added JAR") {
        Ok(d) => Some(d),
        Err(e) => {
            if has_manifest_member {
                restart.push(format!(
                    "loader manifest failed the fail-closed scan ({}) — cannot prove the mod is hot-safe",
                    e.reason()
                ));
            }
            None
        }
    };

    // --- Mixin surface -------------------------------------------------
    let declared_mixins = desc
        .as_ref()
        .is_some_and(|d| !d.mixin_configs.is_empty());
    let has_mixin_members = entries.iter().any(|e| e.name.ends_with(".mixins.json"));
    let mixin_scan = scan_jar_mixins(added_jar_bytes);
    let mixin_blocked = match &mixin_scan {
        Ok(report) => report.mixin_configs_found > 0 || !report.sites.is_empty(),
        // Unscannable mixin surface — fail closed.
        Err(_) => true,
    };
    if declared_mixins || has_mixin_members || mixin_blocked {
        let detail = match &mixin_scan {
            Ok(report) if !report.sites.is_empty() => match &report.verdict {
                MixinLiftVerdict::FullyLiftable(bindings) => {
                    format!("{} site(s), liftable but still startup-time", bindings.len())
                }
                MixinLiftVerdict::RequiresFallback { unliftable, .. } => {
                    format!("{} unliftable site(s)", unliftable.len())
                }
            },
            Ok(report) => format!("{} config(s) declared", report.mixin_configs_found),
            Err(e) => format!("scan failed closed ({e})"),
        };
        restart.push(format!(
            "Mixin bytecode requires a clean JVM restart ({detail})"
        ));
    }

    // --- Native libraries ----------------------------------------------
    let native_count = entries
        .iter()
        .filter(|e| is_native_library_name(&e.name))
        .count();
    if native_count > 0 {
        restart.push(format!(
            "{native_count} native library member(s) (.so/.dll/.dylib/.jnilib) cannot hot-load"
        ));
    }

    // --- Positive data evidence -----------------------------------------
    let bundle_count = count_bundle_members(&entries, added_jar_bytes);
    if bundle_count > 0 {
        hot.push(format!(
            "{bundle_count} unified-bundle document(s) ({BUNDLE_SCHEMA})"
        ));
    }
    let has_pack_mcmeta = entries.iter().any(|e| e.name == "pack.mcmeta");
    let data_members = entries
        .iter()
        .filter(|e| e.name.starts_with("data/"))
        .count();
    if has_pack_mcmeta || data_members > 0 {
        hot.push(format!(
            "datapack data ({} member(s) under data/{})",
            data_members,
            if has_pack_mcmeta { " + pack.mcmeta" } else { "" }
        ));
    }

    // --- .class subset verification --------------------------------------
    let mut class_count = 0usize;
    let mut extracted = 0u64;
    let mut subset_failure: Option<String> = None;
    for entry in &entries {
        if !entry.name.ends_with(".class") {
            continue;
        }
        class_count += 1;
        if class_count > MAX_HOT_SCAN_CLASSES {
            subset_failure = Some("too many .class members".to_string());
            break;
        }
        let bytes = match modpack::read_zip_member(added_jar_bytes, entry, "added JAR") {
            Ok(b) => b,
            Err(e) => {
                subset_failure = Some(format!("{}: {}", entry.name, e.reason()));
                break;
            }
        };
        extracted = extracted.saturating_add(bytes.len() as u64);
        if extracted > MAX_HOT_SCAN_BYTES {
            subset_failure = Some(".class byte budget exceeded".to_string());
            break;
        }
        let parsed = match parse_class_methods(&bytes) {
            Ok(p) => p,
            Err(e) => {
                subset_failure = Some(format!("{}: {e}", entry.name));
                break;
            }
        };
        for m in &parsed.methods {
            if let Err(e) = verify_subset_method(&parsed, &m.name, &m.descriptor) {
                subset_failure = Some(format!("{}::{}{}: {e}", entry.name, m.name, m.descriptor));
                break;
            }
        }
        if subset_failure.is_some() {
            break;
        }
    }
    match subset_failure {
        Some(why) => restart.push(format!(
            "bytecode outside the verified .class subset ({why}) — deep Tier-3 or unliftable code needs a clean restart"
        )),
        None => {
            if class_count > 0 {
                hot.push(format!(
                    "{class_count} .class member(s) verified under the bounded subset VM"
                ));
            }
        }
    }

    if !restart.is_empty() {
        return ReloadRequirement::RestartRequired { reasons: restart };
    }
    if hot.is_empty() {
        hot.push(
            "no executable, mixin, native, or datapack surface — nothing requires a JVM restart"
                .to_string(),
        );
    }
    ReloadRequirement::HotReloadable { reasons: hot }
}

// ---------------------------------------------------------------------------
// install_resolved_artifact — consent-gated verified materialization
// ---------------------------------------------------------------------------

/// Verify `jar_bytes` against `artifact`, cache them content-addressed, and
/// materialize `artifact.file_name` into `mods_dir` — all behind explicit
/// [`ConsentToken`] approval.
///
/// Sequence (fail-closed, nothing is written on any error):
/// 1. `consent.user_approved` must be `true` → [`DepResolveError::ConsentRequired`].
/// 2. [`ResolvedModrinthArtifact::verify_downloaded_bytes`] — exact size +
///    sha256 (preferred) or sha512, constant-time.
/// 3. [`ContentAddressedModCache::store_verified_bytes`] — atomic,
///    content-addressed blob at `<cache_root>/<sha256>.jar`.
/// 4. [`ContentAddressedModCache::materialize_into_profile`] — re-checks
///    consent, the `.jar` name jail, and symlink escapes; atomic rename.
///
/// No network I/O happens here — the caller downloads `artifact.download_url`
/// itself after this plan is approved.
pub fn install_resolved_artifact(
    cache: &ContentAddressedModCache,
    artifact: &ResolvedModrinthArtifact,
    jar_bytes: &[u8],
    mods_dir: &Path,
    consent: &ConsentToken,
) -> Result<PathBuf, DepResolveError> {
    if !consent.user_approved {
        return Err(DepResolveError::ConsentRequired);
    }
    artifact.verify_downloaded_bytes(jar_bytes)?;
    let digest = mod_sync::sha256(jar_bytes);
    cache
        .store_verified_bytes(&digest, jar_bytes)
        .map_err(DepResolveError::Install)?;
    cache
        .materialize_into_profile(&digest, mods_dir, &artifact.file_name, consent)
        .map_err(DepResolveError::Install)
}

// ---------------------------------------------------------------------------
// Unit tests: SHA-512 FIPS 180-4 known-answer vectors
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(bytes: &[u8]) -> String {
        let mut out = String::with_capacity(bytes.len() * 2);
        for b in bytes {
            out.push_str(&format!("{b:02x}"));
        }
        out
    }

    #[test]
    fn sha512_known_answer_vectors() {
        // FIPS 180-4 published digests.
        assert_eq!(
            hex(&sha512(b"")),
            "cf83e1357eefb8bdf1542850d66d8007d620e4050b5715dc83f4a921d36ce9ce\
             47d0d13c5d85f2b0ff8318d2877eec2f63b931bd47417a81a538327af927da3e"
        );
        assert_eq!(
            hex(&sha512(b"abc")),
            "ddaf35a193617abacc417349ae20413112e6fa4e89a97ea20a9eeee64b55d39a\
             2192992a274fc1a836ba3c23a3feebbd454d4423643ce80e2a9ac94fa54ca49f"
        );
        // Multi-block message.
        assert_eq!(
            hex(&sha512(
                b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"
            )),
            "204a8fc6dda82f0a0ced7beb8e08a41657c16ef468b228a8279be331a703c335\
             96fd15c13b1b07f9aa1d3bea57789ca031ad85c7a71dd70354ec631238ca3445"
        );
    }

    #[test]
    fn sha512_padding_boundary_lengths() {
        // sha512("a" * n) for lengths straddling the 112-byte tail boundary
        // and the 128-byte block edge — exercises every padding path.
        let cases: &[(usize, &str)] = &[
            (55, "b0220c772cbf6c1822e2cb38a437d0e1d58772417a4bbb21c961364f8b6143e05aa6316dca8d1d7b19e16448419076395f6086cb55101fbd6d5497b148e1745f"),
            (56, "962b64aae357d2a4fee3ded8b539bdc9d325081822b0bfc55583133aab44f18bafe11d72a7ae16c79ce2ba620ae2242d5144809161945f1367f41b3972e26e04"),
            (64, "01d35c10c6c38c2dcf48f7eebb3235fb5ad74a65ec4cd016e2354c637a8fb49b695ef3c1d6f7ae4cd74d78cc9c9bcac9d4f23a73019998a7f73038a5c9b2dbde"),
            (119, "130396a75cb483f2eee8c56d8a668bb3d2641f5243212c0bee2bd33da096ad9eb8179fe18f9eaacf76e09fae9de4c3f14ba13341e345be05bf76c182cc3468cb"),
            (120, "f241de612b01aa2fa3cf01531d2a8e5e17fc761dfd48a704a834a47f57d6eade7804ecc39be42fdef16ec6adeaf7c01c2fd0c4cc97d3860907cfa4a3b36d0c05"),
            (127, "828613968b501dc00a97e08c73b118aa8876c26b8aac93df128502ab360f91bab50a51e088769a5c1eff4782ace147dce3642554199876374291f5d921629502"),
            (128, "b73d1929aa615934e61a871596b3f3b33359f42b8175602e89f7e06e5f658a243667807ed300314b95cacdd579f3e33abdfbe351909519a846d465c59582f321"),
        ];
        for (len, expected) in cases {
            let data = vec![b'a'; *len];
            assert_eq!(&hex(&sha512(&data)), expected, "len={len}");
        }
    }

    #[test]
    fn sha512_hex_parse_roundtrip() {
        let digest = sha512(b"roundtrip");
        let parsed = parse_sha512_hex(&hex(&digest)).expect("valid hex");
        assert!(digest_eq_64(&digest, &parsed));
        // Truncated and non-hex inputs fail closed.
        assert!(parse_sha512_hex(&hex(&digest)[..126]).is_none());
        assert!(parse_sha512_hex(&"z".repeat(128)).is_none());
    }
}
