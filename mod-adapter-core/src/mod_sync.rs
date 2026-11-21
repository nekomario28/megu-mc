// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later

//! Hardened server → client mod sync protocol, manifest, and security
//! verifier (Lane NET-2).
//!
//! A `mc-spine` server advertises the exact set of `.jar` mods a connecting
//! client needs via a Protocol 767 `CustomPacketPayload` on channel
//! [`MOD_SYNC_CHANNEL`] (`mc-spine:mod_sync_v1`) whose body is the compact
//! deterministic `MSY1` frame produced by [`encode_sync_manifest`]. The
//! client decodes it fail-closed with [`decode_sync_manifest`], computes a
//! [`ModSyncPlan`] with [`plan_mod_sync`], downloads each `to_download`
//! entry over HTTPS, and installs the bytes through
//! [`install_verified_bytes`].
//!
//! The security posture is zero-RCE / zero-arbitrary-file-drop /
//! zero-unverified-downloads, enforced fail-closed at every layer:
//!
//! 1. **Strict origin allowlist** — [`validate_download_url`] only accepts
//!    `https://` URLs whose host is exactly one of
//!    [`TRUSTED_MOD_CDN_HOSTS`] or an operator-pinned mirror registered via
//!    [`SyncPolicy::with_operator_mirrors`]. `http:`/`file:`/`ftp:`/
//!    `javascript:` schemes, credentials-in-URL (`user:pass@host` or any
//!    `@`), raw IP literals (dotted, dword, hex, or IPv6), `localhost`/
//!    `*.localhost`/internal-style suffixes, non-443 ports, control
//!    characters, and non-ASCII URLs are all hard errors.
//! 2. **Strict `.jar` filename jail** — [`validate_jar_file_name`] requires
//!    a plain ASCII basename ending in `.jar` (case-insensitive) and
//!    rejects path separators, `..`, `:`, NUL/control bytes, leading `.`
//!    (hidden files), and shell metacharacters (`$`, `` ` ``, `()`, `{}`,
//!    `|&;<>*?!'"~#`), so a manifest can never steer a write outside the
//!    client's `mods/` directory. [`install_verified_bytes`] additionally
//!    refuses to write through a pre-existing symlink at the target or
//!    temp path.
//! 3. **Zero-dependency SHA-256** — [`Sha256`] is a clean pure-`std`
//!    FIPS 180-4 implementation (no crates.io code) paired with the
//!    constant-time [`digest_eq`]. Every [`ModSyncEntry`] must carry a
//!    64-hex digest and an exact `size_bytes <= MAX_MOD_JAR_BYTES`;
//!    downloaded bytes are checked against BOTH before anything is
//!    written, and the installed file is re-hashed after the atomic rename.
//! 4. **Optimization-mod auto-skip + explicit consent** —
//!    [`plan_mod_sync`] auto-skips any entry whose advertised or
//!    filename-inferred mod id is in
//!    [`crate::fallback::BUILTIN_SUPERSEDED_PERF_MODS`], and
//!    [`install_verified_bytes`] writes nothing unless a
//!    [`ConsentToken`] with `user_approved == true` is presented.
//! 5. **Bounded wire format** — `MSY1` decoding rejects bad magic, counts
//!    above [`MAX_SYNC_MODS`], counts the remaining bytes provably cannot
//!    hold (checked *before* allocation), over-cap string lengths,
//!    truncation, trailing bytes, and any entry that fails the structural
//!    checks above.
//!
//! Clean-room note: this module only *syncs* third-party mod JARs the
//! server operator chose to require; matching mod ids against
//! [`crate::fallback::BUILTIN_SUPERSEDED_PERF_MODS`] is strictly factual
//! compatibility matching — no third-party code, assets, or data is copied,
//! linked, bundled, or derived from.

use std::collections::BTreeSet;
use std::fmt;
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use crate::fallback::BUILTIN_SUPERSEDED_PERF_MODS;
use crate::modpack::infer_candidate_mod_id;

/// Protocol 767 `CustomPacketPayload` channel carrying `MSY1` sync bodies.
pub const MOD_SYNC_CHANNEL: &str = "mc-spine:mod_sync_v1";

/// 4-byte magic prefix of every [`encode_sync_manifest`] body. The `1` is
/// the wire version: a future incompatible frame bumps the magic, so old
/// decoders fail closed on the magic check instead of misparsing.
pub const MSY1_MAGIC: [u8; 4] = *b"MSY1";

/// Hard cap on manifest entries per sync body — checked before allocation.
pub const MAX_SYNC_MODS: usize = 512;

/// Maximum `.jar` size a manifest may advertise and the client will ever
/// accept: 256 MiB. Larger declared sizes are rejected, and on-disk files
/// larger than this hash as mismatches.
pub const MAX_MOD_JAR_BYTES: u64 = 256 * 1024 * 1024;

/// Maximum byte length of a [`ModSyncEntry::download_url`].
pub const MAX_SYNC_URL_LEN: usize = 2048;

/// Maximum byte length of a [`ModSyncEntry::file_name`] (one `NAME_MAX`).
pub const MAX_SYNC_FILE_NAME_LEN: usize = 255;

/// Maximum byte length of a [`ModSyncEntry::mod_id`].
pub const MAX_SYNC_MOD_ID_LEN: usize = 128;

/// Minimum wire size of one encoded entry: 1-byte id length + 1-byte name
/// length + 2-byte URL length + 8-byte size + 32-byte digest, with all
/// strings empty. A declared count exceeding what the remaining bytes can
/// hold is provably malformed before any allocation.
const MIN_ENTRY_WIRE_BYTES: usize = 1 + 1 + 2 + 8 + 32;

/// Canonical trusted mod-host CDNs. Matched exactly (after lowercasing and
/// stripping one trailing `.`) — `cdn.modrinth.com.evil.example` does NOT
/// match. Operator mirrors extend this only through
/// [`SyncPolicy::with_operator_mirrors`].
pub const TRUSTED_MOD_CDN_HOSTS: &[&str] = &[
    "cdn.modrinth.com",
    "edge.forgecdn.net",
    "mediafilez.forgecdn.net",
];

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Classification of a [`ModSyncError`] so callers and tests can match on
/// the failure mode without parsing message text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModSyncErrorKind {
    /// Wire body failed a structural check (magic, truncation, trailing
    /// bytes, length prefixes, UTF-8).
    MalformedManifest,
    /// Declared or supplied entry count exceeds [`MAX_SYNC_MODS`].
    TooManyEntries,
    /// Download URL failed the scheme/host/SSRF checks.
    UnsafeUrl,
    /// Target file name failed the `.jar` basename jail.
    UnsafeFileName,
    /// Operator-pinned mirror host is itself unsafe (IP literal, internal
    /// name, or malformed).
    UnsafeMirrorHost,
    /// A non-URL/non-filename field failed validation (mod id charset,
    /// digest hex, size bound, superseded id, duplicate name).
    InvalidEntry,
    /// [`install_verified_bytes`] was called with `user_approved == false`.
    ConsentRequired,
    /// Downloaded byte length differs from the manifest `size_bytes`.
    SizeMismatch,
    /// Downloaded or installed bytes failed SHA-256 verification.
    DigestMismatch,
    /// The install target or temp path is a symlink — refused to write
    /// through it.
    SymlinkEscape,
    /// Underlying filesystem error.
    Io,
}

/// Fail-closed error for every mod-sync check. Carries a [`ModSyncErrorKind`]
/// plus a human-readable reason for operator logs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModSyncError {
    kind: ModSyncErrorKind,
    reason: String,
}

impl ModSyncError {
    pub fn kind(&self) -> ModSyncErrorKind {
        self.kind
    }

    pub fn reason(&self) -> &str {
        &self.reason
    }
}

impl fmt::Display for ModSyncError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.reason)
    }
}

impl std::error::Error for ModSyncError {}

fn err(kind: ModSyncErrorKind, msg: impl Into<String>) -> ModSyncError {
    ModSyncError {
        kind,
        reason: msg.into(),
    }
}

fn io_err(e: io::Error) -> ModSyncError {
    err(ModSyncErrorKind::Io, format!("I/O error: {e}"))
}

// ---------------------------------------------------------------------------
// Pure-std FIPS 180-4 SHA-256
// ---------------------------------------------------------------------------

const SHA256_H0: [u32; 8] = [
    0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19,
];

const SHA256_K: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
    0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
    0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
    0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
    0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
];

fn sha256_compress(h: &mut [u32; 8], block: &[u8]) {
    debug_assert_eq!(block.len(), 64);
    let mut w = [0u32; 64];
    for i in 0..16 {
        w[i] = u32::from_be_bytes([
            block[4 * i],
            block[4 * i + 1],
            block[4 * i + 2],
            block[4 * i + 3],
        ]);
    }
    for i in 16..64 {
        let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
        let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
        w[i] = w[i - 16]
            .wrapping_add(s0)
            .wrapping_add(w[i - 7])
            .wrapping_add(s1);
    }
    let (mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh) =
        (h[0], h[1], h[2], h[3], h[4], h[5], h[6], h[7]);
    for i in 0..64 {
        let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
        let ch = (e & f) ^ ((!e) & g);
        let t1 = hh
            .wrapping_add(s1)
            .wrapping_add(ch)
            .wrapping_add(SHA256_K[i])
            .wrapping_add(w[i]);
        let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
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

/// Streaming pure-`std` FIPS 180-4 SHA-256 hasher — zero crates.io
/// dependencies. Feed bytes with [`Sha256::update`] in any chunking and take
/// the digest with [`Sha256::finalize`].
#[derive(Clone)]
pub struct Sha256 {
    h: [u32; 8],
    buf: [u8; 64],
    buf_len: usize,
    total_len: u64,
}

impl Default for Sha256 {
    fn default() -> Self {
        Self::new()
    }
}

impl Sha256 {
    pub fn new() -> Self {
        Self {
            h: SHA256_H0,
            buf: [0u8; 64],
            buf_len: 0,
            total_len: 0,
        }
    }

    pub fn update(&mut self, mut data: &[u8]) {
        self.total_len = self.total_len.wrapping_add(data.len() as u64);
        if self.buf_len > 0 {
            let take = (64 - self.buf_len).min(data.len());
            self.buf[self.buf_len..self.buf_len + take].copy_from_slice(&data[..take]);
            self.buf_len += take;
            data = &data[take..];
            if self.buf_len == 64 {
                let block = self.buf;
                sha256_compress(&mut self.h, &block);
                self.buf_len = 0;
            }
        }
        while data.len() >= 64 {
            sha256_compress(&mut self.h, &data[..64]);
            data = &data[64..];
        }
        if !data.is_empty() {
            self.buf[..data.len()].copy_from_slice(data);
            self.buf_len = data.len();
        }
    }

    pub fn finalize(mut self) -> [u8; 32] {
        let bit_len = self.total_len.wrapping_mul(8);
        self.update(&[0x80]);
        while self.buf_len != 56 {
            self.update(&[0]);
        }
        self.buf[56..64].copy_from_slice(&bit_len.to_be_bytes());
        let block = self.buf;
        sha256_compress(&mut self.h, &block);
        let mut out = [0u8; 32];
        for (i, word) in self.h.iter().enumerate() {
            out[4 * i..4 * i + 4].copy_from_slice(&word.to_be_bytes());
        }
        out
    }
}

/// One-shot SHA-256 digest of `data`.
pub fn sha256(data: &[u8]) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(data);
    h.finalize()
}

/// Lowercase 64-char hex encoding of a 32-byte digest.
pub fn digest_hex(digest: &[u8; 32]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(64);
    for &b in digest.iter() {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0F) as usize] as char);
    }
    out
}

/// One-shot lowercase hex SHA-256 of `data`.
pub fn sha256_hex(data: &[u8]) -> String {
    digest_hex(&sha256(data))
}

/// Parse a strict 64-hex-char digest (either case). `None` for any other
/// length or non-hex character — fail closed.
pub fn parse_sha256_hex(text: &str) -> Option<[u8; 32]> {
    if text.len() != 64 || !text.is_ascii() {
        return None;
    }
    let mut out = [0u8; 32];
    let bytes = text.as_bytes();
    for i in 0..32 {
        let hi = (bytes[2 * i] as char).to_digit(16)?;
        let lo = (bytes[2 * i + 1] as char).to_digit(16)?;
        out[i] = ((hi << 4) | lo) as u8;
    }
    Some(out)
}

/// Constant-time equality of two 32-byte digests — never short-circuits on
/// a differing byte, so byte position leaks nothing through timing.
pub fn digest_eq(a: &[u8; 32], b: &[u8; 32]) -> bool {
    let mut diff = 0u8;
    for i in 0..32 {
        diff |= a[i] ^ b[i];
    }
    diff == 0
}

/// Stream-hash a file with the [`MAX_MOD_JAR_BYTES`] ceiling enforced while
/// reading. Returns `(digest, byte_len)`; when the file exceeds the cap the
/// returned digest is zeroed — the size check fails first in every caller,
/// so the zero digest can never produce a false verify.
fn hash_file_capped(path: &Path) -> io::Result<([u8; 32], u64)> {
    let mut file = fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 8192];
    let mut total = 0u64;
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        total += n as u64;
        if total > MAX_MOD_JAR_BYTES {
            return Ok(([0u8; 32], total));
        }
        hasher.update(&buf[..n]);
    }
    Ok((hasher.finalize(), total))
}

// ---------------------------------------------------------------------------
// Manifest types & client policy
// ---------------------------------------------------------------------------

/// One server-advertised mod the client must hold in `mods/`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModSyncEntry {
    /// Loader-style mod id (`create`, `jei`, `sodium`). ASCII
    /// `[a-zA-Z0-9._-]`, 1..=[`MAX_SYNC_MOD_ID_LEN`] bytes.
    pub mod_id: String,
    /// Plain `.jar` basename to install under `mods/` — passes the
    /// [`validate_jar_file_name`] jail.
    pub file_name: String,
    /// `https://` download URL on a trusted CDN (or operator mirror).
    pub download_url: String,
    /// 64-hex-char SHA-256 of the exact JAR bytes.
    pub sha256_hex: String,
    /// Exact JAR byte length, 1..=[`MAX_MOD_JAR_BYTES`].
    pub size_bytes: u64,
}

/// The decoded `MSY1` body: the ordered set of mods a client must hold.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SyncManifest {
    pub entries: Vec<ModSyncEntry>,
}

/// Client-side trust policy: the canonical [`TRUSTED_MOD_CDN_HOSTS`]
/// allowlist plus any operator-pinned HTTPS mirror hostnames.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SyncPolicy {
    extra_trusted_hosts: Vec<String>,
}

impl SyncPolicy {
    /// The default policy — canonical CDNs only, no mirrors.
    pub fn canonical() -> Self {
        Self::default()
    }

    /// Canonical CDNs plus operator-pinned mirror hostnames. Each mirror is
    /// validated as a bare DNS host (no scheme, port, path, credentials, IP
    /// literal, or internal name) — an unusable or unsafe mirror is an
    /// error, not a silent skip.
    pub fn with_operator_mirrors(mirrors: &[&str]) -> Result<Self, ModSyncError> {
        let mut extra = Vec::with_capacity(mirrors.len());
        for raw in mirrors {
            let host = normalize_host(raw).map_err(|e| {
                err(
                    ModSyncErrorKind::UnsafeMirrorHost,
                    format!("operator mirror '{raw}': {}", e.reason()),
                )
            })?;
            if is_ip_literal_host(&host) || is_local_or_internal_host(&host) {
                return Err(err(
                    ModSyncErrorKind::UnsafeMirrorHost,
                    format!("operator mirror '{raw}' is an IP literal or internal name"),
                ));
            }
            extra.push(host);
        }
        Ok(Self {
            extra_trusted_hosts: extra,
        })
    }

    /// Normalized operator-pinned mirror hostnames (excludes the canonical
    /// CDN list).
    pub fn extra_trusted_hosts(&self) -> &[String] {
        &self.extra_trusted_hosts
    }
}

/// Explicit end-user consent required before any byte is written to `mods/`.
/// Construct via [`ConsentToken::approved`] / [`ConsentToken::denied`] or
/// set the field directly in tests.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ConsentToken {
    pub user_approved: bool,
}

impl ConsentToken {
    pub const fn approved() -> Self {
        Self {
            user_approved: true,
        }
    }

    pub const fn denied() -> Self {
        Self {
            user_approved: false,
        }
    }
}

// ---------------------------------------------------------------------------
// URL allowlist & filename jail
// ---------------------------------------------------------------------------

/// Lowercase `host`, strip one trailing DNS root `.`, then require a strict
/// DNS name: 1..=253 bytes of `[a-z0-9.-]`, every label non-empty and not
/// starting or ending with `-`. This kills IDN homoglyphs (punycode
/// `xn--` hosts never equal a plain allowlist name), underscores,
/// whitespace, and embedded port/userinfo residue.
fn normalize_host(host: &str) -> Result<String, ModSyncError> {
    if host.is_empty() || host.len() > 253 {
        return Err(err(
            ModSyncErrorKind::UnsafeUrl,
            format!("host length {} out of bounds", host.len()),
        ));
    }
    let mut h = host.to_ascii_lowercase();
    if h.ends_with('.') {
        h.pop();
    }
    if h.is_empty()
        || !h
            .bytes()
            .all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'.' | b'-'))
    {
        return Err(err(
            ModSyncErrorKind::UnsafeUrl,
            format!("host '{host}' is not a strict DNS name"),
        ));
    }
    for label in h.split('.') {
        if label.is_empty() || label.starts_with('-') || label.ends_with('-') {
            return Err(err(
                ModSyncErrorKind::UnsafeUrl,
                format!("host '{host}' has a malformed DNS label"),
            ));
        }
    }
    Ok(h)
}

/// `host` (already normalized) is a raw IP literal in any of the textual
/// forms a URL parser might accept: dotted `127.0.0.1` / truncated `127.1`
/// / octal, dword integer `2130706433`, or hex `0x7f000001`. `[::1]`-style
/// IPv6 literals are rejected earlier by the `[` authority check.
fn is_ip_literal_host(host: &str) -> bool {
    host.parse::<std::net::Ipv4Addr>().is_ok()
        || host.bytes().all(|b| b.is_ascii_digit() || b == b'.')
        || host.starts_with("0x")
}

/// `host` (already normalized) names a loopback/internal SSRF target that
/// must never be contacted even if an operator fumbles a mirror pin.
fn is_local_or_internal_host(host: &str) -> bool {
    host == "localhost"
        || host.ends_with(".localhost")
        || host == "local"
        || host.ends_with(".local")
        || host.ends_with(".internal")
        || host.ends_with(".lan")
        || host.ends_with(".home")
        || host.ends_with(".corp")
        || host.ends_with(".intranet")
}

/// Scheme/host/port/SSRF validation that is independent of the allowlist —
/// shared by the encoder, decoder, and [`validate_download_url`]. Returns
/// the normalized hostname on success.
fn validate_url_syntax(url: &str) -> Result<String, ModSyncError> {
    let url_err = |m: String| err(ModSyncErrorKind::UnsafeUrl, m);
    if url.is_empty() || url.len() > MAX_SYNC_URL_LEN {
        return Err(url_err(format!(
            "URL length {} out of bounds (1..={MAX_SYNC_URL_LEN})",
            url.len()
        )));
    }
    if !url.is_ascii() {
        return Err(url_err("URL must be pure ASCII".to_string()));
    }
    if url.bytes().any(|b| b <= 0x20 || b == 0x7F) {
        return Err(url_err("URL contains a control/space byte".to_string()));
    }
    // HTTPS only — no http:, file:, ftp:, javascript:, or bare `host/path`.
    if url.len() < 9 || !url.as_bytes()[..8].eq_ignore_ascii_case(b"https://") {
        return Err(url_err(format!(
            "URL '{url}' is not an https:// URL"
        )));
    }
    let rest = &url[8..];
    let auth_end = rest
        .find(|c| matches!(c, '/' | '?' | '#'))
        .unwrap_or(rest.len());
    let authority = &rest[..auth_end];
    if authority.is_empty() {
        return Err(url_err(format!("URL '{url}' has an empty authority")));
    }
    // Any '@' is credentials-in-URL or host-spoofing
    // (`cdn.modrinth.com@evil.example`) — always refused.
    if authority.contains('@') {
        return Err(url_err(format!(
            "URL '{url}' embeds credentials or userinfo"
        )));
    }
    let host = if authority.starts_with('[') {
        // IPv6 literal — raw IPs are never trusted download origins.
        return Err(url_err(format!("URL '{url}' uses an IPv6 literal")));
    } else if let Some(colon) = authority.rfind(':') {
        let port = &authority[colon + 1..];
        if port != "443" {
            return Err(url_err(format!(
                "URL '{url}' uses a non-443 port"
            )));
        }
        &authority[..colon]
    } else {
        authority
    };
    let host = normalize_host(host)?;
    if is_ip_literal_host(&host) {
        return Err(url_err(format!(
            "URL '{url}' uses a raw IP literal host"
        )));
    }
    if is_local_or_internal_host(&host) {
        return Err(url_err(format!(
            "URL '{url}' targets a local/internal host"
        )));
    }
    Ok(host)
}

/// Whether `host` is a trusted download origin under `policy`: an exact
/// match to a [`TRUSTED_MOD_CDN_HOSTS`] entry or a normalized
/// operator-pinned mirror. Never a suffix match.
pub fn is_trusted_mod_cdn_host(host: &str, policy: &SyncPolicy) -> bool {
    let Ok(h) = normalize_host(host) else {
        return false;
    };
    TRUSTED_MOD_CDN_HOSTS.contains(&h.as_str())
        || policy.extra_trusted_hosts.iter().any(|m| m == &h)
}

/// Full download-URL policy check: [`validate_url_syntax`] (HTTPS only, no
/// credentials, no IP literals, no local/internal hosts, no control bytes)
/// plus the trusted-host allowlist. Fail closed on every deviation.
pub fn validate_download_url(url: &str, policy: &SyncPolicy) -> Result<(), ModSyncError> {
    let host = validate_url_syntax(url)?;
    if !is_trusted_mod_cdn_host(&host, policy) {
        return Err(err(
            ModSyncErrorKind::UnsafeUrl,
            format!("URL host '{host}' is not a trusted mod CDN"),
        ));
    }
    Ok(())
}

/// The `.jar` basename jail: `name` must be a pure-ASCII plain file name
/// ending in `.jar` (case-insensitive) with a non-empty stem, and may not
/// contain `/`, `\`, `:`, `..`, NUL/control/DEL bytes, a leading `.`, or
/// shell metacharacters (`$` `` ` `` `(){}[]&;|<>*?!'"~#`). Space is
/// allowed — real mod JARs carry it — but nothing that can traverse a
/// path or expand in a shell is.
pub fn validate_jar_file_name(name: &str) -> Result<(), ModSyncError> {
    let name_err = |m: String| err(ModSyncErrorKind::UnsafeFileName, m);
    if name.is_empty() || name.len() > MAX_SYNC_FILE_NAME_LEN {
        return Err(name_err(format!(
            "file name length {} out of bounds (1..={MAX_SYNC_FILE_NAME_LEN})",
            name.len()
        )));
    }
    if !name.is_ascii() {
        return Err(name_err(format!("file name '{name}' is not ASCII")));
    }
    for b in name.bytes() {
        match b {
            0x00..=0x1F | 0x7F => {
                return Err(name_err(format!("file name '{name}' has a control byte")));
            }
            b'/' | b'\\' => {
                return Err(name_err(format!(
                    "file name '{name}' contains a path separator"
                )));
            }
            b':' => {
                return Err(name_err(format!("file name '{name}' contains ':'")));
            }
            b'`' | b'$' | b'(' | b')' | b'{' | b'}' | b'[' | b']' | b'&' | b';' | b'|' | b'<'
            | b'>' | b'*' | b'?' | b'!' | b'\'' | b'"' | b'~' | b'#' => {
                return Err(name_err(format!(
                    "file name '{name}' contains a shell metacharacter"
                )));
            }
            _ => {}
        }
    }
    if name.starts_with('.') {
        return Err(name_err(format!(
            "file name '{name}' is a hidden dotfile"
        )));
    }
    if name.contains("..") {
        return Err(name_err(format!("file name '{name}' contains '..'")));
    }
    if name.len() <= 4 || !name[name.len() - 4..].eq_ignore_ascii_case(".jar") {
        return Err(name_err(format!(
            "file name '{name}' is not a .jar basename"
        )));
    }
    Ok(())
}

/// Structural validation that never touches policy: mod id charset, the
/// filename jail, a parseable 64-hex digest, and a size inside
/// `1..=MAX_MOD_JAR_BYTES`. Shared by encoder, decoder, and
/// [`validate_sync_entry`].
fn validate_entry_structure(entry: &ModSyncEntry) -> Result<(), ModSyncError> {
    if entry.mod_id.is_empty()
        || entry.mod_id.len() > MAX_SYNC_MOD_ID_LEN
        || !entry
            .mod_id
            .bytes()
            .all(|b| matches!(b, b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'_' | b'-' | b'.'))
    {
        return Err(err(
            ModSyncErrorKind::InvalidEntry,
            format!("mod id {:?} is empty, over-cap, or has an illegal byte", entry.mod_id),
        ));
    }
    validate_jar_file_name(&entry.file_name)?;
    if parse_sha256_hex(&entry.sha256_hex).is_none() {
        return Err(err(
            ModSyncErrorKind::InvalidEntry,
            format!(
                "entry '{}' sha256 is not a 64-hex digest",
                entry.file_name
            ),
        ));
    }
    if entry.size_bytes == 0 || entry.size_bytes > MAX_MOD_JAR_BYTES {
        return Err(err(
            ModSyncErrorKind::InvalidEntry,
            format!(
                "entry '{}' size {} out of bounds (1..={MAX_MOD_JAR_BYTES})",
                entry.file_name, entry.size_bytes
            ),
        ));
    }
    Ok(())
}

/// Every per-entry check under `policy`: structure + syntax-checked HTTPS
/// URL on the trusted-host allowlist. Anything this rejects can never be
/// downloaded or installed.
pub fn validate_sync_entry(entry: &ModSyncEntry, policy: &SyncPolicy) -> Result<(), ModSyncError> {
    validate_entry_structure(entry)?;
    validate_download_url(&entry.download_url, policy)
}

// ---------------------------------------------------------------------------
// MSY1 wire codec
// ---------------------------------------------------------------------------

fn read_u8(bytes: &[u8], cursor: &mut usize) -> Result<u8, ModSyncError> {
    let b = *bytes.get(*cursor).ok_or_else(|| {
        err(
            ModSyncErrorKind::MalformedManifest,
            "truncated MSY1 body".to_string(),
        )
    })?;
    *cursor += 1;
    Ok(b)
}

/// Read a length-prefixed UTF-8 string, checking the cap and the byte bound
/// BEFORE any allocation — an over-cap or out-of-body length fails closed
/// with zero bytes allocated.
fn read_capped_str<'a>(
    bytes: &'a [u8],
    cursor: &mut usize,
    len: usize,
    cap: usize,
    what: &str,
) -> Result<&'a str, ModSyncError> {
    if len > cap {
        return Err(err(
            ModSyncErrorKind::MalformedManifest,
            format!("{what} length {len} exceeds cap {cap}"),
        ));
    }
    let end = cursor.checked_add(len).ok_or_else(|| {
        err(
            ModSyncErrorKind::MalformedManifest,
            format!("{what} length overflows body"),
        )
    })?;
    let slice = bytes.get(*cursor..end).ok_or_else(|| {
        err(
            ModSyncErrorKind::MalformedManifest,
            format!("{what} truncated mid-string"),
        )
    })?;
    let text = std::str::from_utf8(slice).map_err(|_| {
        err(
            ModSyncErrorKind::MalformedManifest,
            format!("{what} is not valid UTF-8"),
        )
    })?;
    *cursor = end;
    Ok(text)
}

/// Deterministically encode `manifest` as an `MSY1` body:
/// `"MSY1" | u16be count | per-entry {u8 id_len, id, u8 name_len, name,
/// u16be url_len, url, u64be size, 32-byte digest}`. Entries are emitted in
/// manifest order; the same manifest always produces the same bytes.
///
/// Fail closed: refuses to encode an entry that violates the structural
/// contract (string caps, `.jar` jail, digest, size bound) or the HTTPS
/// URL syntax — a server bug surfaces at encode time, not on the client's
/// decode path. The trusted-host *allowlist* is client policy and is not
/// evaluated here.
pub fn encode_sync_manifest(manifest: &SyncManifest) -> Result<Vec<u8>, ModSyncError> {
    if manifest.entries.len() > MAX_SYNC_MODS {
        return Err(err(
            ModSyncErrorKind::TooManyEntries,
            format!(
                "{} entries exceed MAX_SYNC_MODS={MAX_SYNC_MODS}",
                manifest.entries.len()
            ),
        ));
    }
    let mut out = Vec::with_capacity(6 + manifest.entries.len() * 96);
    out.extend_from_slice(&MSY1_MAGIC);
    out.extend_from_slice(&(manifest.entries.len() as u16).to_be_bytes());
    for entry in &manifest.entries {
        validate_entry_structure(entry)?;
        validate_url_syntax(&entry.download_url)?;
        let digest = parse_sha256_hex(&entry.sha256_hex).expect("structure-checked digest");
        out.push(entry.mod_id.len() as u8);
        out.extend_from_slice(entry.mod_id.as_bytes());
        out.push(entry.file_name.len() as u8);
        out.extend_from_slice(entry.file_name.as_bytes());
        out.extend_from_slice(&(entry.download_url.len() as u16).to_be_bytes());
        out.extend_from_slice(entry.download_url.as_bytes());
        out.extend_from_slice(&entry.size_bytes.to_be_bytes());
        out.extend_from_slice(&digest);
    }
    Ok(out)
}

/// Decode an `MSY1` body produced by [`encode_sync_manifest`]. Fail closed:
/// bad magic, a count over [`MAX_SYNC_MODS`], a count the remaining bytes
/// provably cannot hold, over-cap or truncated strings, trailing bytes,
/// and any entry failing [`validate_entry_structure`] or
/// [`validate_url_syntax`] all yield `Err` — a hostile body is dropped in
/// full, never partially applied.
pub fn decode_sync_manifest(bytes: &[u8]) -> Result<SyncManifest, ModSyncError> {
    let malformed = |m: &str| err(ModSyncErrorKind::MalformedManifest, m.to_string());
    if bytes.len() < 6 {
        return Err(malformed("MSY1 body shorter than header"));
    }
    if bytes[..4] != MSY1_MAGIC {
        return Err(malformed("bad MSY1 magic"));
    }
    let count = u16::from_be_bytes([bytes[4], bytes[5]]) as usize;
    if count > MAX_SYNC_MODS {
        return Err(err(
            ModSyncErrorKind::TooManyEntries,
            format!("declared count {count} exceeds MAX_SYNC_MODS={MAX_SYNC_MODS}"),
        ));
    }
    let mut cursor = 6usize;
    // Pre-allocation bound: each entry costs at least MIN_ENTRY_WIRE_BYTES,
    // so a larger count is provably malformed before Vec::with_capacity.
    if count > (bytes.len() - cursor) / MIN_ENTRY_WIRE_BYTES {
        return Err(malformed("declared count exceeds remaining body"));
    }
    let mut entries = Vec::with_capacity(count);
    for _ in 0..count {
        let id_len = read_u8(bytes, &mut cursor)? as usize;
        let mod_id =
            read_capped_str(bytes, &mut cursor, id_len, MAX_SYNC_MOD_ID_LEN, "mod_id")?.to_owned();
        let name_len = read_u8(bytes, &mut cursor)? as usize;
        let file_name = read_capped_str(
            bytes,
            &mut cursor,
            name_len,
            MAX_SYNC_FILE_NAME_LEN,
            "file_name",
        )?
        .to_owned();
        let url_len = u16::from_be_bytes([read_u8(bytes, &mut cursor)?, read_u8(bytes, &mut cursor)?])
            as usize;
        let download_url =
            read_capped_str(bytes, &mut cursor, url_len, MAX_SYNC_URL_LEN, "url")?.to_owned();
        let end = cursor.checked_add(8).ok_or_else(|| malformed("size overflow"))?;
        let size_slice = bytes
            .get(cursor..end)
            .ok_or_else(|| malformed("truncated size"))?;
        let size_bytes = u64::from_be_bytes(size_slice.try_into().expect("8-byte slice"));
        cursor = end;
        let end = cursor.checked_add(32).ok_or_else(|| malformed("digest overflow"))?;
        let digest_slice = bytes
            .get(cursor..end)
            .ok_or_else(|| malformed("truncated digest"))?;
        let mut digest = [0u8; 32];
        digest.copy_from_slice(digest_slice);
        cursor = end;
        entries.push(ModSyncEntry {
            mod_id,
            file_name,
            download_url,
            sha256_hex: digest_hex(&digest),
            size_bytes,
        });
    }
    if cursor != bytes.len() {
        return Err(malformed("trailing bytes after final entry"));
    }
    for entry in &entries {
        validate_entry_structure(entry)?;
        validate_url_syntax(&entry.download_url)?;
    }
    Ok(SyncManifest { entries })
}

// ---------------------------------------------------------------------------
// Sync planning & verified install
// ---------------------------------------------------------------------------

/// Whether `entry` names a performance mod `mc-spine` already implements
/// natively — checked by the advertised `mod_id` AND the id inferred from
/// the JAR filename, with `-`/`_` folded so `ferrite_core` still matches
/// `ferritecore`. Skipped entries are never downloaded.
pub fn is_superseded_sync_entry(entry: &ModSyncEntry) -> bool {
    is_superseded_id(&entry.mod_id)
        || infer_candidate_mod_id(&entry.file_name).is_some_and(|id| is_superseded_id(&id))
}

fn is_superseded_id(mod_id: &str) -> bool {
    let key = normalized_id_key(mod_id);
    BUILTIN_SUPERSEDED_PERF_MODS
        .iter()
        .any(|b| normalized_id_key(b) == key)
}

/// `-`/`_`-insensitive lowercase comparison key, matching
/// `modpack::normalized_mod_key` semantics.
fn normalized_id_key(id: &str) -> String {
    id.bytes()
        .filter(|b| !matches!(b, b'-' | b'_'))
        .map(|b| b.to_ascii_lowercase() as char)
        .collect()
}

/// A manifest entry the client will not download, with the reason.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RejectedSyncEntry {
    pub mod_id: String,
    pub file_name: String,
    pub reason: String,
}

/// What [`plan_mod_sync`] decided for a manifest against the client's
/// `mods/` directory. Disjoint partitions: every entry lands in exactly
/// one bucket.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ModSyncPlan {
    /// `file_name`s whose on-disk bytes already match the manifest digest
    /// and size — nothing to fetch.
    pub already_installed_verified: Vec<String>,
    /// Entries that passed every check and must be downloaded over HTTPS,
    /// then installed through [`install_verified_bytes`].
    pub to_download: Vec<ModSyncEntry>,
    /// `mod_id`s auto-skipped because
    /// [`BUILTIN_SUPERSEDED_PERF_MODS`] already covers them.
    pub skipped_builtin_perf: Vec<String>,
    /// Entries that failed validation, duplicated a target name, or point
    /// at an unsafe on-disk target — with the fail-closed reason.
    pub rejected_entries: Vec<RejectedSyncEntry>,
}

/// Partition `manifest` against the client's `mods/` directory. Purely
/// observational — reads and hashes existing files but writes nothing.
/// Entries are checked in order: full validation first (a malformed
/// "sodium" entry is *rejected*, not silently skipped), then duplicate
/// `file_name` detection, then [`BUILTIN_SUPERSEDED_PERF_MODS`]
/// auto-skip, then on-disk verification.
pub fn plan_mod_sync(manifest: &SyncManifest, mods_dir: &Path, policy: &SyncPolicy) -> ModSyncPlan {
    let mut plan = ModSyncPlan::default();
    let mut seen_names = BTreeSet::new();
    for entry in &manifest.entries {
        let reject = |plan: &mut ModSyncPlan, reason: String| {
            plan.rejected_entries.push(RejectedSyncEntry {
                mod_id: entry.mod_id.clone(),
                file_name: entry.file_name.clone(),
                reason,
            });
        };
        if let Err(e) = validate_sync_entry(entry, policy) {
            reject(&mut plan, e.to_string());
            continue;
        }
        // `a.jar` and `A.JAR` collide on every case-insensitive client FS —
        // keep the first, reject the rest rather than race the last write.
        if !seen_names.insert(entry.file_name.to_ascii_lowercase()) {
            reject(
                &mut plan,
                format!("duplicate target file name '{}'", entry.file_name),
            );
            continue;
        }
        if is_superseded_sync_entry(entry) {
            plan.skipped_builtin_perf.push(entry.mod_id.clone());
            continue;
        }
        let target = mods_dir.join(&entry.file_name);
        match fs::symlink_metadata(&target) {
            Ok(meta) if meta.file_type().is_symlink() => {
                reject(
                    &mut plan,
                    format!("existing '{}' is a symlink — refusing to touch it", entry.file_name),
                );
            }
            Ok(meta) if meta.is_dir() => {
                reject(
                    &mut plan,
                    format!("existing '{}' is a directory — cannot overwrite", entry.file_name),
                );
            }
            Ok(_) => match hash_file_capped(&target) {
                Ok((digest, len)) => {
                    let expected =
                        parse_sha256_hex(&entry.sha256_hex).expect("structure-checked digest");
                    if len == entry.size_bytes && digest_eq(&digest, &expected) {
                        plan.already_installed_verified.push(entry.file_name.clone());
                    } else {
                        plan.to_download.push(entry.clone());
                    }
                }
                Err(_) => plan.to_download.push(entry.clone()),
            },
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                plan.to_download.push(entry.clone());
            }
            Err(e) => reject(&mut plan, format!("cannot stat target: {e}")),
        }
    }
    plan
}

/// Atomically install already-downloaded `bytes` for `entry` into
/// `mods_dir`, gated on explicit user consent.
///
/// Fail-closed sequence — anything short of full success writes NOTHING
/// (or removes a partially staged temp file):
/// 1. `consent.user_approved` must be `true`.
/// 2. [`validate_sync_entry`] — URL allowlist, filename jail, digest,
///    size — plus the superseded-perf-mod guard.
/// 3. `bytes.len() == entry.size_bytes` and
///    `sha256(bytes) ==` the manifest digest, compared in constant time.
/// 4. Refuse to write through a pre-existing symlink at the target or the
///    staging path (both are unlinked inside `mods/` only).
/// 5. Stage to `.{file_name}.msync-part` inside `mods/` (created
///    `O_EXCL`, fsynced), then rename — atomic on POSIX.
/// 6. Re-hash the installed file; on mismatch the file is removed.
///
/// Returns the installed path. No network I/O happens here — the caller
/// downloads `to_download` URLs itself.
pub fn install_verified_bytes(
    mods_dir: &Path,
    entry: &ModSyncEntry,
    bytes: &[u8],
    consent: &ConsentToken,
    policy: &SyncPolicy,
) -> Result<PathBuf, ModSyncError> {
    if !consent.user_approved {
        return Err(err(
            ModSyncErrorKind::ConsentRequired,
            format!(
                "user consent required before installing '{}'",
                entry.file_name
            ),
        ));
    }
    validate_sync_entry(entry, policy)?;
    if is_superseded_sync_entry(entry) {
        return Err(err(
            ModSyncErrorKind::InvalidEntry,
            format!(
                "'{}' is superseded by built-in optimizations — never installed",
                entry.mod_id
            ),
        ));
    }
    if bytes.len() as u64 != entry.size_bytes {
        return Err(err(
            ModSyncErrorKind::SizeMismatch,
            format!(
                "'{}': downloaded {} bytes, manifest expects {}",
                entry.file_name,
                bytes.len(),
                entry.size_bytes
            ),
        ));
    }
    let expected = parse_sha256_hex(&entry.sha256_hex).expect("structure-checked digest");
    if !digest_eq(&sha256(bytes), &expected) {
        return Err(err(
            ModSyncErrorKind::DigestMismatch,
            format!("'{}' failed SHA-256 verification", entry.file_name),
        ));
    }

    fs::create_dir_all(mods_dir).map_err(io_err)?;
    let target = mods_dir.join(&entry.file_name);
    if let Ok(meta) = fs::symlink_metadata(&target) {
        if meta.file_type().is_symlink() {
            return Err(err(
                ModSyncErrorKind::SymlinkEscape,
                format!(
                    "'{}' exists as a symlink — refusing to write through it",
                    entry.file_name
                ),
            ));
        }
    }

    // Stage inside `mods/` so the rename stays on one filesystem. A stale
    // temp left by a crashed run (or pre-planted by an attacker) is
    // unlinked first — remove_file never follows a symlink.
    let tmp = mods_dir.join(format!(".{}.msync-part", entry.file_name));
    if fs::symlink_metadata(&tmp).is_ok() {
        fs::remove_file(&tmp).map_err(io_err)?;
    }
    let stage = || -> io::Result<()> {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()
    };
    if let Err(e) = stage() {
        let _ = fs::remove_file(&tmp);
        return Err(io_err(e));
    }
    if let Err(first) = fs::rename(&tmp, &target) {
        // Windows refuses rename-over-existing; remove the target (a
        // symlink swap-in only loses the link itself) and retry once.
        let _ = first;
        if fs::symlink_metadata(&target).is_ok() {
            let _ = fs::remove_file(&target);
        }
        if let Err(e) = fs::rename(&tmp, &target) {
            let _ = fs::remove_file(&tmp);
            return Err(io_err(e));
        }
    }
    // Post-install verification: hash what's actually on disk now. A
    // mismatch deletes the planted file rather than leaving corrupt bytes
    // in `mods/`.
    let verified = hash_file_capped(&target)
        .map(|(digest, len)| len == entry.size_bytes && digest_eq(&digest, &expected))
        .unwrap_or(false);
    if !verified {
        let _ = fs::remove_file(&target);
        return Err(err(
            ModSyncErrorKind::DigestMismatch,
            format!("'{}' failed post-install re-verification", entry.file_name),
        ));
    }
    // Best-effort directory fsync so the rename survives a crash; not all
    // filesystems support it, so the result is ignored.
    if let Ok(dir) = fs::File::open(mods_dir) {
        let _ = dir.sync_all();
    }
    Ok(target)
}
