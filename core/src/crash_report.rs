// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! NotEnoughCrashes-style crash reporting: a deterministic fingerprint plus a
//! privacy-redacted bilingual operator blame.
//!
//! [`CrashContext`] captures the four fields a crash handler needs — what
//! failed, which mod/module is blamed, which Minecraft version was running,
//! and the raw detail — and derives from them:
//!
//! * a stable fingerprint `MEGU-E-XXXXXXXX` so duplicate reports from
//!   different players stack onto the same ID (the fingerprint is computed
//!   over the *redacted* fields, so per-player paths and secrets cannot
//!   fragment or leak into it),
//! * a two-line bilingual EN/JA operator blame, and
//! * a copy-paste Markdown summary.
//!
//! Redaction is automatic and dependency-free (`std` only): personal paths
//! (`C:\Users\<name>\`, `/home/<name>/`, `/Users/<name>/`), IPv4 addresses,
//! `Bearer <token>` credentials, and `rcon.password=`/`rcon.password:`
//! values are replaced with `[REDACTED]` before anything is rendered or
//! hashed.

/// All text fields are stored raw; every public render/hash path redacts
/// them first, so callers can pass through whatever the crash site had.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CrashContext {
    /// Coarse failure class, e.g. `"panic"`, `"oom"`, `"protocol_error"`.
    pub error_kind: String,
    /// Mod id or module blamed for the crash, e.g. `"create"`, `"megu-proto"`.
    pub culprit_mod_or_module: String,
    /// Minecraft version string, e.g. `"26.3"` or `"1.21.1"`.
    pub mc_version: String,
    /// Free-form detail (error message, stack hint, packet id, ...).
    pub detail_message: String,
}

impl CrashContext {
    pub fn new(
        error_kind: impl Into<String>,
        culprit_mod_or_module: impl Into<String>,
        mc_version: impl Into<String>,
        detail_message: impl Into<String>,
    ) -> Self {
        Self {
            error_kind: error_kind.into(),
            culprit_mod_or_module: culprit_mod_or_module.into(),
            mc_version: mc_version.into(),
            detail_message: detail_message.into(),
        }
    }

    /// The four fields after [`redact_sensitive`] — safe to log or share.
    pub fn redacted(&self) -> Self {
        Self {
            error_kind: redact_sensitive(&self.error_kind),
            culprit_mod_or_module: redact_sensitive(&self.culprit_mod_or_module),
            mc_version: redact_sensitive(&self.mc_version),
            detail_message: redact_sensitive(&self.detail_message),
        }
    }

    /// Deterministic `MEGU-E-XXXXXXXX` fingerprint over the redacted fields.
    ///
    /// Two crashes from different players produce the same fingerprint when
    /// their normalized (redacted, trimmed) context matches, so reports stack
    /// cleanly. FNV-1a over length-prefixed fields keeps the hash stable
    /// across runs and platforms.
    pub fn fingerprint(&self) -> String {
        let r = self.redacted();
        let mut hash = crate::hash::Fnv1a64::new();
        for field in [
            r.error_kind.as_str(),
            r.culprit_mod_or_module.as_str(),
            r.mc_version.as_str(),
            r.detail_message.as_str(),
        ] {
            let norm = field.trim();
            hash.update(norm.as_bytes());
            hash.update_u8(0xff); // field separator
        }
        format!("MEGU-E-{:08X}", (hash.finish() as u32))
    }

    /// Two-line operator blame: line 1 English, line 2 Japanese.
    pub fn blame_lines(&self) -> (String, String) {
        let fp = self.fingerprint();
        let r = self.redacted();
        let en = format!(
            "Crash {fp} blamed on `{}` (MC {}): {} — {}",
            r.culprit_mod_or_module, r.mc_version, r.error_kind, r.detail_message
        );
        let ja = format!(
            "クラッシュ {fp} の原因候補: `{}` (MC {}) — {}: {}",
            r.culprit_mod_or_module, r.mc_version, r.error_kind, r.detail_message
        );
        (en, ja)
    }

    /// Copy-paste Markdown crash summary with the fingerprint up front.
    pub fn markdown_summary(&self) -> String {
        let r = self.redacted();
        let fp = self.fingerprint();
        format!(
            "## Crash Report `{fp}`\n\
             - **Fingerprint**: `{fp}`\n\
             - **Error kind**: {}\n\
             - **Suspected culprit**: `{}`\n\
             - **Minecraft version**: {}\n\
             - **Detail** (auto-redacted):\n\n```\n{}\n```\n",
            r.error_kind,
            r.culprit_mod_or_module,
            r.mc_version,
            r.detail_message,
        )
    }
}

/// Replace personal paths, IP addresses, and inline credentials in `input`
/// with `[REDACTED]`. Over-redaction is preferred to under-redaction: any
/// token that merely looks sensitive is masked.
pub fn redact_sensitive(input: &str) -> String {
    let mut out = redact_user_paths(input);
    out = redact_bearer_tokens(&out);
    out = redact_rcon_password(&out);
    out = redact_ipv4(&out);
    out
}

const REDACTED: &str = "[REDACTED]";

/// Personal home-directory prefixes whose next path segment is a username.
/// Matched ASCII-case-insensitively; the username runs to the next `/` or
/// `\` (or end of input) and is replaced, keeping the surrounding path.
fn redact_user_paths(input: &str) -> String {
    const PREFIXES: [&str; 3] = ["c:\\users\\", "/home/", "/users/"];
    let bytes = input.as_bytes();
    let mut out = String::with_capacity(input.len());
    let mut i = 0;
    while i < bytes.len() {
        let mut matched = None;
        for prefix in PREFIXES {
            let end = i + prefix.len();
            if end <= bytes.len() && bytes[i..end].eq_ignore_ascii_case(prefix.as_bytes()) {
                matched = Some(end);
                break;
            }
        }
        if let Some(name_start) = matched {
            // Copy the prefix verbatim (preserves original casing).
            out.push_str(&input[i..name_start]);
            // Consume the username segment.
            let mut j = name_start;
            while j < bytes.len() && bytes[j] != b'/' && bytes[j] != b'\\' {
                j += 1;
            }
            if j == name_start {
                // Empty segment (`/home//x`): nothing to redact, skip the
                // prefix so we don't loop on it.
                i = name_start;
                continue;
            }
            out.push_str(REDACTED);
            i = j;
        } else {
            // Copy one full UTF-8 char.
            let ch_len = utf8_len(bytes[i]);
            out.push_str(&input[i..i + ch_len]);
            i += ch_len;
        }
    }
    out
}

fn utf8_len(first: u8) -> usize {
    if first < 0x80 {
        1
    } else if first < 0xe0 {
        2
    } else if first < 0xf0 {
        3
    } else {
        4
    }
}

/// `Bearer <token>` → `Bearer [REDACTED]` (case-insensitive keyword).
fn redact_bearer_tokens(input: &str) -> String {
    const KEYWORD: &str = "bearer";
    let lower = input.to_ascii_lowercase();
    let bytes = lower.as_bytes();
    let mut out = String::with_capacity(input.len());
    let mut i = 0;
    let mut search_from = 0;
    while let Some(pos) = lower[search_from..].find(KEYWORD) {
        let kw_start = search_from + pos;
        let kw_end = kw_start + KEYWORD.len();
        // Require a word boundary before and whitespace after.
        let before_ok = kw_start == 0 || !is_token_byte(bytes[kw_start - 1]);
        let after_ws = kw_end < bytes.len() && bytes[kw_end].is_ascii_whitespace();
        if !before_ok || !after_ws {
            search_from = kw_start + 1;
            continue;
        }
        // Token starts after the whitespace run.
        let mut tok_start = kw_end;
        while tok_start < bytes.len() && bytes[tok_start].is_ascii_whitespace() {
            tok_start += 1;
        }
        if tok_start >= bytes.len() {
            break;
        }
        let mut tok_end = tok_start;
        while tok_end < bytes.len() && !bytes[tok_end].is_ascii_whitespace() {
            tok_end += 1;
        }
        out.push_str(&input[i..tok_start]);
        out.push_str(REDACTED);
        i = tok_end;
        search_from = tok_end;
    }
    out.push_str(&input[i..]);
    out
}

/// `rcon.password=<value>` / `rcon.password:<value>` → key kept, value masked.
fn redact_rcon_password(input: &str) -> String {
    const KEY: &str = "rcon.password";
    let lower = input.to_ascii_lowercase();
    let bytes = lower.as_bytes();
    let mut out = String::with_capacity(input.len());
    let mut i = 0;
    let mut search_from = 0;
    while let Some(pos) = lower[search_from..].find(KEY) {
        let key_start = search_from + pos;
        let key_end = key_start + KEY.len();
        // Word boundary after the key, then optional spaces, then = or :.
        let after_ok = key_end == bytes.len() || !is_token_byte(bytes[key_end]);
        if !after_ok {
            search_from = key_start + 1;
            continue;
        }
        let mut p = key_end;
        while p < bytes.len() && (bytes[p] == b' ' || bytes[p] == b'\t') {
            p += 1;
        }
        if p >= bytes.len() || (bytes[p] != b'=' && bytes[p] != b':') {
            search_from = key_start + 1;
            continue;
        }
        p += 1;
        while p < bytes.len() && (bytes[p] == b' ' || bytes[p] == b'\t') {
            p += 1;
        }
        // Value runs to whitespace, a quote, or a common kv delimiter.
        let mut v_end = p;
        while v_end < bytes.len()
            && !bytes[v_end].is_ascii_whitespace()
            && bytes[v_end] != b'"'
            && bytes[v_end] != b'\''
            && bytes[v_end] != b'&'
            && bytes[v_end] != b';'
            && bytes[v_end] != b','
        {
            v_end += 1;
        }
        out.push_str(&input[i..p]);
        out.push_str(REDACTED);
        i = v_end;
        search_from = v_end;
    }
    out.push_str(&input[i..]);
    out
}

/// Dotted-quad IPv4 literals → `[REDACTED]`.
fn redact_ipv4(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = String::with_capacity(input.len());
    let mut i = 0;
    while i < bytes.len() {
        if let Some(end) = ipv4_quad_end(bytes, i) {
            out.push_str(REDACTED);
            i = end;
        } else {
            let ch_len = utf8_len(bytes[i]);
            out.push_str(&input[i..i + ch_len]);
            i += ch_len;
        }
    }
    out
}

/// If an IPv4 dotted quad starts at `start`, return the index just past it.
/// Each octet is 1–3 ASCII digits; boundaries must not be digits or dots.
fn ipv4_quad_end(bytes: &[u8], start: usize) -> Option<usize> {
    if start > 0 && (bytes[start - 1].is_ascii_digit() || bytes[start - 1] == b'.') {
        return None;
    }
    let mut pos = start;
    for part in 0..4 {
        let part_start = pos;
        while pos < bytes.len() && bytes[pos].is_ascii_digit() && pos - part_start < 3 {
            pos += 1;
        }
        if pos == part_start {
            return None;
        }
        if part < 3 {
            if pos >= bytes.len() || bytes[pos] != b'.' {
                return None;
            }
            pos += 1;
        }
    }
    if pos < bytes.len() && (bytes[pos].is_ascii_digit() || bytes[pos] == b'.') {
        return None;
    }
    Some(pos)
}

fn is_token_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'-' || b == b'.'
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprint_is_deterministic_and_stable_across_players() {
        let a = CrashContext::new(
            "panic",
            "create",
            "26.3",
            "NPE in contraption at /home/alice/mods/create.jar",
        );
        let b = CrashContext::new(
            "panic",
            "create",
            "26.3",
            "NPE in contraption at /home/bob/mods/create.jar",
        );
        assert_eq!(a.fingerprint(), b.fingerprint());
        assert!(a.fingerprint().starts_with("MEGU-E-"));
        assert_eq!(a.fingerprint().len(), "MEGU-E-".len() + 8);
        // Different context → different fingerprint.
        let c = CrashContext::new("oom", "create", "26.3", "heap");
        assert_ne!(a.fingerprint(), c.fingerprint());
    }

    #[test]
    fn redacts_paths_ips_and_credentials() {
        let input = "load failed for C:\\Users\\Alice\\mods\\x.jar and \
                     /home/bob/config.json plus /Users/carol/lib.jar; \
                     peer 192.168.1.10 auth Bearer abc123XYZ and \
                     rcon.password=hunter2, rcon.password: s3cret end";
        let out = redact_sensitive(input);
        assert!(!out.contains("Alice") && !out.contains("bob") && !out.contains("carol"));
        assert!(!out.contains("192.168.1.10"));
        assert!(!out.contains("abc123XYZ"));
        assert!(!out.contains("hunter2") && !out.contains("s3cret"));
        assert!(out.contains("[REDACTED]"));
        assert!(out.contains("Bearer [REDACTED]"));
        assert!(out.contains("rcon.password=[REDACTED]"));
    }

    #[test]
    fn does_not_redact_version_numbers_or_non_ip_dots() {
        let out = redact_sensitive("MC 1.21.1 / protocol 767.5 ok");
        assert_eq!(out, "MC 1.21.1 / protocol 767.5 ok");
    }

    #[test]
    fn blame_and_markdown_are_redacted_and_bilingual() {
        let ctx = CrashContext::new(
            "protocol_error",
            "megu-proto",
            "1.21.1",
            "frame from 10.0.0.5 exceeded cap; Bearer tok_live",
        );
        let (en, ja) = ctx.blame_lines();
        assert!(en.contains("megu-proto"));
        assert!(ja.contains("原因候補"));
        assert!(!en.contains("10.0.0.5") && !ja.contains("tok_live"));
        let md = ctx.markdown_summary();
        assert!(md.contains("## Crash Report `MEGU-E-"));
        assert!(md.contains("Suspected culprit"));
        assert!(!md.contains("10.0.0.5") && !md.contains("tok_live"));
    }
}
