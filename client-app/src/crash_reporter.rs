// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! Panic hook, local `crash-reports/` persistence, and a `megu-report/1`
//! `/api/report` relay client — pure `std`, zero new crate dependencies.
//!
//! Three cooperating pieces:
//!
//! * [`install_panic_hook`] + [`capture_panic_context`] turn a panic into a
//!   redacted, fingerprinted [`CrashReport`] and persist it atomically as
//!   `<crash_dir>/crash-<fingerprint>.md`. The hook never panics: every
//!   fallible step is best-effort and failures are swallowed.
//! * [`submit_crash_report`] applies the [`CrashSubmitPolicy`] consent gate
//!   (`Ask` / `AutoCrashOnly` / `Off`), deduplicates per session through
//!   [`SessionReportDeduper`], sends the `megu-report/1` JSON payload, and
//!   falls back to `<crash_dir>/pending-<fingerprint>.json` when the
//!   endpoint is missing, unreachable, or answers a non-2xx status — so a
//!   report is never lost.
//! * [`ReportTransport`] abstracts the wire. [`StdHttpReportTransport`]
//!   speaks loopback `http://127.0.0.1:<port>/api/report` directly over
//!   [`TcpStream`] (unit-testable against a local mock server) and
//!   `https://` endpoints through an optional system `curl` subprocess
//!   with strict timeouts, no shell, `--proto =https`, `-q` (no curlrc),
//!   and the JSON body piped over stdin — never argv.
//!
//! The exact request/response contract mirrors
//! `website/functions/api/report.js`: schema `megu-report/1`, a hard 4 KiB
//! body cap, `kind` `"crash"|"bug"`, `MEGU-E-XXXXXXXX` fingerprints, and a
//! JSON receipt carrying `issue_url` / `receipt_id` / `duplicate`.
//!
//! `client-app` is pinned to `std`-only manifests, so the canonical
//! `mc-spine-core` crash-report implementation (fingerprinting +
//! redaction) is compiled in-tree via `#[path]` rather than duplicated.
//! The code below is byte-for-byte the same source `mc-spine-core` ships,
//! so fingerprints always match the core definition.

use std::collections::HashSet;
use std::error::Error;
use std::fmt;
use std::fs;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::panic::{self, AssertUnwindSafe, PanicHookInfo};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::Duration;

// The canonical implementation is compiled in place (no Cargo.toml
// change): `#[path]` mounts the core source file as a private submodule.
// Not every helper it exposes is used here, and the module is private, so
// dead_code is allowed rather than letting the shared file go stale.
#[allow(dead_code)]
#[path = "../../core/src/crash_report.rs"]
mod core_crash_report;

pub use core_crash_report::{redact_sensitive, CrashContext};

// --- Contract constants, mirrored from website/functions/api/report.js ---

/// Schema tag the relay endpoint requires.
pub const REPORT_SCHEMA: &str = "megu-report/1";
/// Hard cap on the serialized JSON payload (`MAX_BODY_BYTES` server-side).
pub const MAX_BODY_BYTES: usize = 4096;
/// Hard cap on the local redacted markdown crash file.
pub const CRASH_MARKDOWN_MAX_BYTES: usize = 4096;
const MAX_TOP_FRAMES: usize = 6;
const MAX_FRAME_LEN: usize = 120;
const MAX_SUMMARY_LEN: usize = 240;
const MAX_MC_VERSION_LEN: usize = 16;
const MAX_ENGINE_VERSION_LEN: usize = 32;
const MAX_OS_LEN: usize = 32;
const MAX_GPU_CLASS_LEN: usize = 32;
const MAX_CULPRIT_LEN: usize = 80;
const MAX_SUBSYSTEM_LEN: usize = 40;
/// Largest response header block the loopback transport will parse.
const MAX_RESPONSE_HEAD_BYTES: usize = 64 * 1024;

/// Default per-request timeout for report submission.
pub const DEFAULT_REPORT_TIMEOUT: Duration = Duration::from_secs(10);

/// Report classification, matching the `kind` field of `megu-report/1`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReportKind {
    /// Automatic crash report (always carries a fingerprint).
    Crash,
    /// Manual bug report filed by the user.
    Bug,
}

impl ReportKind {
    /// Wire value of the `kind` field.
    pub fn as_str(self) -> &'static str {
        match self {
            ReportKind::Crash => "crash",
            ReportKind::Bug => "bug",
        }
    }
}

/// Consent gate applied before any network submission.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CrashSubmitPolicy {
    /// Never send automatically; the UI must ask first
    /// (`user_confirmed == true` required).
    Ask,
    /// Automatic send for [`ReportKind::Crash`] only; bug reports still
    /// require `user_confirmed == true`.
    AutoCrashOnly,
    /// Local files only — the transport is never touched.
    Off,
}

/// Environment snapshot baked into every report: versions, OS/arch, the
/// blamed module, and optional diagnostic fields the schema accepts.
#[derive(Clone, Debug)]
pub struct ReportEnv {
    /// Minecraft version string (<= 16 chars on the wire).
    pub mc_version: String,
    /// Engine version string (<= 32 chars on the wire).
    pub engine_version: String,
    /// OS/arch descriptor (<= 32 chars on the wire).
    pub os: String,
    /// Mod id or module blamed for the crash (<= 80 chars on the wire).
    pub culprit: String,
    /// Optional subsystem label (<= 40 chars on the wire).
    pub subsystem: Option<String>,
    /// Optional active stage/screen label baked in when no
    /// [`set_active_stage`] override is installed.
    pub stage: Option<String>,
    /// Optional GPU class label (<= 32 chars on the wire).
    pub gpu_class: Option<String>,
    /// Optional top stack frames (<= 6 entries, <= 120 chars each).
    pub top_frames: Vec<String>,
    /// Optional loaded mod count (0..=4096).
    pub mod_count: Option<u32>,
}

impl Default for ReportEnv {
    fn default() -> Self {
        Self::detect()
    }
}

impl ReportEnv {
    /// Best-effort environment autodetection: canonical latest MC target,
    /// the client crate version, and `std::env::consts` OS/arch.
    pub fn detect() -> Self {
        Self {
            mc_version: mc_spine_kernel::TargetVersion::LATEST.id().to_string(),
            engine_version: format!("mc-spine-client {}", env!("CARGO_PKG_VERSION")),
            os: format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH),
            culprit: "unknown".to_string(),
            subsystem: None,
            stage: None,
            gpu_class: None,
            top_frames: Vec::new(),
            mod_count: None,
        }
    }
}

/// Source location of a panic (`file:line:column`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PanicSite {
    /// Source file path (may be workspace-relative).
    pub file: String,
    /// 1-based line.
    pub line: u32,
    /// 1-based column.
    pub column: u32,
}

/// A fully redacted, fingerprinted, size-capped report ready for local
/// persistence and optional relay submission.
#[derive(Clone, Debug)]
pub struct CrashReport {
    /// `"crash"` or `"bug"` on the wire.
    pub kind: ReportKind,
    /// `MEGU-E-XXXXXXXX`, computed over the redacted fields.
    pub fingerprint: String,
    /// Redacted crash context (fingerprint source of truth).
    pub context: CrashContext,
    /// Redacted markdown body, <= [`CRASH_MARKDOWN_MAX_BYTES`].
    pub markdown: String,
    /// Serialized `megu-report/1` body, <= [`MAX_BODY_BYTES`].
    pub payload_json: String,
}

// --- Active stage/screen tracking --------------------------------------

static ACTIVE_STAGE: Mutex<Option<String>> = Mutex::new(None);

fn lock_active_stage() -> std::sync::MutexGuard<'static, Option<String>> {
    ACTIVE_STAGE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Record the active stage/screen (e.g. `"main-menu"`, `"world-join"`,
/// `"in-game"`) so a later panic report can name it. Cheap and
/// poison-tolerant; `None` clears the label.
pub fn set_active_stage(stage: impl Into<String>) {
    *lock_active_stage() = Some(stage.into());
}

/// Clear the active stage/screen label.
pub fn clear_active_stage() {
    *lock_active_stage() = None;
}

fn current_stage() -> Option<String> {
    lock_active_stage().clone()
}

// --- Capture ------------------------------------------------------------

/// Build a [`CrashReport`] from a panic (or a manually reported bug):
/// redact every free-form field, fingerprint the redacted context, render
/// the capped markdown file body, and serialize the `megu-report/1`
/// payload. `message` is the panic payload or user-written summary;
/// `site` is the optional `file:line:column`; `error_kind` is the coarse
/// class stored in the context (`"panic"`, `"manual-report"`, ...).
///
/// This function performs no I/O and never panics on hostile input: every
/// field is redacted and then truncated to its schema bound.
pub fn capture_panic_context(
    env: &ReportEnv,
    kind: ReportKind,
    error_kind: &str,
    message: &str,
    site: Option<&PanicSite>,
) -> CrashReport {
    let stage = current_stage().or_else(|| env.stage.clone());
    let mut detail = String::from(message);
    if let Some(site) = site {
        detail.push_str(&format!(" at {}:{}:{}", site.file, site.line, site.column));
    }
    if let Some(stage) = &stage {
        detail.push_str(&format!(" [stage: {stage}]"));
    }
    let context = CrashContext::new(
        error_kind,
        env.culprit.as_str(),
        env.mc_version.as_str(),
        detail,
    )
    .redacted();
    let fingerprint = context.fingerprint();
    let markdown = render_markdown(&context, &fingerprint, env, kind, stage.as_deref());
    let payload_json = encode_payload(&context, &fingerprint, env, kind);
    CrashReport {
        kind,
        fingerprint,
        context,
        markdown,
        payload_json,
    }
}

/// Render the local `crash-<fingerprint>.md` body. The detail block is
/// truncated last so the header always survives; the result never exceeds
/// [`CRASH_MARKDOWN_MAX_BYTES`].
fn render_markdown(
    context: &CrashContext,
    fingerprint: &str,
    env: &ReportEnv,
    kind: ReportKind,
    stage: Option<&str>,
) -> String {
    let mut head = format!(
        "## Crash Report `{fingerprint}`\n\
         - **Fingerprint**: `{fingerprint}`\n\
         - **Kind**: {}\n\
         - **Error kind**: {}\n\
         - **Suspected culprit**: `{}`\n\
         - **Minecraft version**: {}\n\
         - **Engine**: {}\n\
         - **OS/arch**: {}\n",
        kind.as_str(),
        context.error_kind,
        context.culprit_mod_or_module,
        context.mc_version,
        redact_sensitive(&env.engine_version),
        redact_sensitive(&env.os),
    );
    if let Some(stage) = stage {
        head.push_str(&format!("- **Stage**: {}\n", redact_sensitive(stage)));
    }
    if let Some(subsystem) = &env.subsystem {
        head.push_str(&format!(
            "- **Subsystem**: {}\n",
            redact_sensitive(subsystem)
        ));
    }
    if let Some(gpu) = &env.gpu_class {
        head.push_str(&format!("- **GPU**: {}\n", redact_sensitive(gpu)));
    }
    let detail_head = "- **Detail** (auto-redacted):\n\n```\n";
    let detail_tail = "```\n";
    let trunc_note = "\n…[truncated]";
    // The join below adds a separator '\n' after the detail body.
    let fixed = head.len() + detail_head.len() + detail_tail.len() + 1;
    let detail_budget = CRASH_MARKDOWN_MAX_BYTES
        .saturating_sub(fixed)
        .saturating_sub(trunc_note.len());
    let detail = &context.detail_message;
    let detail_body = if detail.len() > detail_budget {
        let mut cut = truncate_bytes(detail, detail_budget);
        cut.push_str(trunc_note);
        cut
    } else {
        detail.clone()
    };
    format!("{head}{detail_head}{detail_body}\n{detail_tail}")
}

/// Truncate `s` to at most `budget` bytes on a UTF-8 char boundary.
fn truncate_bytes(s: &str, budget: usize) -> String {
    if s.len() <= budget {
        return s.to_string();
    }
    let mut end = budget;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_string()
}

/// Truncate to at most `max` Unicode scalar values.
fn truncate_chars(s: &str, max: usize) -> String {
    match s.char_indices().nth(max) {
        Some((byte, _)) => s[..byte].to_string(),
        None => s.to_string(),
    }
}

/// First whitespace-free line of `s`, truncated to `max` chars; falls
/// back to `fallback` when nothing non-empty remains.
fn first_line_or(s: &str, max: usize, fallback: &str) -> String {
    let first = s.lines().map(str::trim).find(|l| !l.is_empty());
    match first {
        Some(line) => truncate_chars(line, max),
        None => fallback.to_string(),
    }
}

// --- JSON payload --------------------------------------------------------

fn push_json_str(out: &mut String, first: &mut bool, key: &str, value: &str) {
    if !*first {
        out.push(',');
    }
    *first = false;
    out.push('"');
    out.push_str(key);
    out.push_str("\":");
    json_escape_into(out, value);
}

fn push_json_num(out: &mut String, first: &mut bool, key: &str, value: u32) {
    if !*first {
        out.push(',');
    }
    *first = false;
    out.push('"');
    out.push_str(key);
    out.push_str("\":");
    out.push_str(&value.to_string());
}

fn json_escape_into(out: &mut String, s: &str) {
    out.push('"');
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

/// Serialize the `megu-report/1` payload exactly as
/// `website/functions/api/report.js` validates it: every string clipped
/// to its schema length, `top_frames` capped at 6, and the whole body
/// forced under [`MAX_BODY_BYTES`] by shedding frames then shrinking the
/// summary (never silently dropping required fields).
fn encode_payload(
    context: &CrashContext,
    fingerprint: &str,
    env: &ReportEnv,
    kind: ReportKind,
) -> String {
    let mc_version = truncate_chars(&redact_sensitive(&env.mc_version), MAX_MC_VERSION_LEN);
    let engine_version = truncate_chars(
        &redact_sensitive(&env.engine_version),
        MAX_ENGINE_VERSION_LEN,
    );
    let os = truncate_chars(&redact_sensitive(&env.os), MAX_OS_LEN);
    let culprit = truncate_chars(
        &redact_sensitive(&env.culprit),
        MAX_CULPRIT_LEN,
    );
    let subsystem = env
        .subsystem
        .as_deref()
        .map(|s| truncate_chars(&redact_sensitive(s), MAX_SUBSYSTEM_LEN));
    let gpu_class = env
        .gpu_class
        .as_deref()
        .map(|s| truncate_chars(&redact_sensitive(s), MAX_GPU_CLASS_LEN));
    let summary_fallback = format!("{} report", kind.as_str());
    let mut summary = first_line_or(
        &redact_sensitive(&context.detail_message),
        MAX_SUMMARY_LEN,
        &summary_fallback,
    );
    if summary.trim().is_empty() {
        summary = truncate_chars(&summary_fallback, MAX_SUMMARY_LEN);
    }
    let mut frames: Vec<String> = env
        .top_frames
        .iter()
        .take(MAX_TOP_FRAMES)
        .map(|f| truncate_chars(&redact_sensitive(f), MAX_FRAME_LEN))
        .collect();
    let fingerprint_field = if fingerprint.is_empty() {
        None
    } else {
        Some(fingerprint)
    };

    loop {
        let mut out = String::with_capacity(1024);
        out.push('{');
        let mut first = true;
        push_json_str(&mut out, &mut first, "schema", REPORT_SCHEMA);
        push_json_str(&mut out, &mut first, "kind", kind.as_str());
        if let Some(fp) = fingerprint_field {
            push_json_str(&mut out, &mut first, "fingerprint", fp);
        }
        push_json_str(&mut out, &mut first, "mc_version", &mc_version);
        push_json_str(&mut out, &mut first, "engine_version", &engine_version);
        push_json_str(&mut out, &mut first, "os", &os);
        push_json_str(&mut out, &mut first, "summary", &summary);
        if !culprit.is_empty() {
            push_json_str(&mut out, &mut first, "culprit", &culprit);
        }
        if let Some(s) = &subsystem {
            push_json_str(&mut out, &mut first, "subsystem", s);
        }
        if let Some(g) = &gpu_class {
            push_json_str(&mut out, &mut first, "gpu_class", g);
        }
        if !frames.is_empty() {
            if !first {
                out.push(',');
            }
            first = false;
            out.push_str("\"top_frames\":[");
            for (i, frame) in frames.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                json_escape_into(&mut out, frame);
            }
            out.push(']');
        }
        if let Some(mods) = env.mod_count {
            push_json_num(&mut out, &mut first, "mod_count", mods.min(4096));
        }
        out.push('}');
        if out.len() <= MAX_BODY_BYTES {
            return out;
        }
        if !frames.is_empty() {
            frames.pop();
            continue;
        }
        // No frames left to shed: halve the summary until the body fits.
        let shrink = summary.chars().count() / 2;
        if shrink == 0 {
            // Fixed fields alone cannot exceed the cap, but never loop.
            return out;
        }
        summary = truncate_chars(&summary, shrink);
    }
}

// --- Minimal JSON parser (receipt decoding) -------------------------------

// `Arr` exists so the parser accepts full JSON even though receipt
// decoding only reads object members, strings, bools, and numbers.
#[allow(dead_code)]
#[derive(Debug)]
enum Json {
    Null,
    Bool(bool),
    Num(f64),
    Str(String),
    Arr(Vec<Json>),
    Obj(Vec<(String, Json)>),
}

impl Json {
    fn get(&self, key: &str) -> Option<&Json> {
        match self {
            Json::Obj(fields) => fields.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }
    fn as_str(&self) -> Option<&str> {
        match self {
            Json::Str(s) => Some(s),
            _ => None,
        }
    }
    fn as_bool(&self) -> Option<bool> {
        match self {
            Json::Bool(b) => Some(*b),
            _ => None,
        }
    }
    fn as_f64(&self) -> Option<f64> {
        match self {
            Json::Num(n) => Some(*n),
            _ => None,
        }
    }
}

const JSON_MAX_DEPTH: usize = 64;

fn parse_json(text: &str) -> Result<Json, String> {
    let mut p = JsonParser {
        bytes: text.as_bytes(),
        pos: 0,
    };
    p.skip_ws();
    let value = p.value(0)?;
    p.skip_ws();
    if p.pos != p.bytes.len() {
        return Err("trailing bytes after JSON value".to_string());
    }
    Ok(value)
}

struct JsonParser<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl JsonParser<'_> {
    fn skip_ws(&mut self) {
        while self.pos < self.bytes.len()
            && matches!(self.bytes[self.pos], b' ' | b'\t' | b'\n' | b'\r')
        {
            self.pos += 1;
        }
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn value(&mut self, depth: usize) -> Result<Json, String> {
        if depth > JSON_MAX_DEPTH {
            return Err("JSON nesting too deep".to_string());
        }
        match self.peek() {
            Some(b'{') => self.object(depth),
            Some(b'[') => self.array(depth),
            Some(b'"') => Ok(Json::Str(self.string()?)),
            Some(b't') => self.literal(b"true", Json::Bool(true)),
            Some(b'f') => self.literal(b"false", Json::Bool(false)),
            Some(b'n') => self.literal(b"null", Json::Null),
            Some(b'-') | Some(b'0'..=b'9') => self.number(),
            other => Err(format!("unexpected byte {other:?} in JSON")),
        }
    }

    fn literal(&mut self, lit: &[u8], value: Json) -> Result<Json, String> {
        if self.bytes[self.pos..].starts_with(lit) {
            self.pos += lit.len();
            Ok(value)
        } else {
            Err("invalid JSON literal".to_string())
        }
    }

    fn number(&mut self) -> Result<Json, String> {
        let start = self.pos;
        if self.peek() == Some(b'-') {
            self.pos += 1;
        }
        while matches!(self.peek(), Some(b'0'..=b'9')) {
            self.pos += 1;
        }
        if self.peek() == Some(b'.') {
            self.pos += 1;
            while matches!(self.peek(), Some(b'0'..=b'9')) {
                self.pos += 1;
            }
        }
        if matches!(self.peek(), Some(b'e') | Some(b'E')) {
            self.pos += 1;
            if matches!(self.peek(), Some(b'+') | Some(b'-')) {
                self.pos += 1;
            }
            while matches!(self.peek(), Some(b'0'..=b'9')) {
                self.pos += 1;
            }
        }
        let text = std::str::from_utf8(&self.bytes[start..self.pos])
            .map_err(|_| "invalid UTF-8 in number".to_string())?;
        text.parse::<f64>()
            .map(Json::Num)
            .map_err(|_| format!("invalid JSON number {text:?}"))
    }

    fn hex4(&mut self) -> Result<u32, String> {
        if self.pos + 4 > self.bytes.len() {
            return Err("truncated \\u escape".to_string());
        }
        let mut value = 0u32;
        for _ in 0..4 {
            let b = self.bytes[self.pos];
            self.pos += 1;
            let digit = match b {
                b'0'..=b'9' => u32::from(b - b'0'),
                b'a'..=b'f' => u32::from(b - b'a') + 10,
                b'A'..=b'F' => u32::from(b - b'A') + 10,
                _ => return Err("invalid \\u escape".to_string()),
            };
            value = value * 16 + digit;
        }
        Ok(value)
    }

    fn string(&mut self) -> Result<String, String> {
        if self.peek() != Some(b'"') {
            return Err("expected string".to_string());
        }
        self.pos += 1;
        let mut out = String::new();
        loop {
            let b = self.peek().ok_or_else(|| "unterminated string".to_string())?;
            self.pos += 1;
            match b {
                b'"' => return Ok(out),
                b'\\' => {
                    let esc = self
                        .peek()
                        .ok_or_else(|| "unterminated escape".to_string())?;
                    self.pos += 1;
                    match esc {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'b' => out.push('\u{0008}'),
                        b'f' => out.push('\u{000c}'),
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        b'u' => {
                            let hi = self.hex4()?;
                            let ch = if (0xd800..0xdc00).contains(&hi)
                                && self.bytes[self.pos..].starts_with(b"\\u")
                            {
                                self.pos += 2;
                                let lo = self.hex4()?;
                                let scalar =
                                    0x1_0000 + ((hi - 0xd800) << 10) + (lo - 0xdc00);
                                char::from_u32(scalar).unwrap_or('\u{fffd}')
                            } else {
                                char::from_u32(hi).unwrap_or('\u{fffd}')
                            };
                            out.push(ch);
                        }
                        _ => return Err("invalid string escape".to_string()),
                    }
                }
                _ => {
                    // Copy the full UTF-8 scalar starting at b.
                    let start = self.pos - 1;
                    let len = if b < 0x80 {
                        1
                    } else if b < 0xe0 {
                        2
                    } else if b < 0xf0 {
                        3
                    } else {
                        4
                    };
                    let end = (start + len).min(self.bytes.len());
                    match std::str::from_utf8(&self.bytes[start..end]) {
                        Ok(s) => {
                            out.push_str(s);
                            self.pos = end;
                        }
                        Err(_) => {
                            out.push('\u{fffd}');
                            self.pos = start + 1;
                        }
                    }
                }
            }
        }
    }

    fn array(&mut self, depth: usize) -> Result<Json, String> {
        self.pos += 1; // '['
        let mut items = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b']') {
            self.pos += 1;
            return Ok(Json::Arr(items));
        }
        loop {
            self.skip_ws();
            items.push(self.value(depth + 1)?);
            self.skip_ws();
            match self.peek() {
                Some(b',') => self.pos += 1,
                Some(b']') => {
                    self.pos += 1;
                    return Ok(Json::Arr(items));
                }
                _ => return Err("expected ',' or ']' in array".to_string()),
            }
        }
    }

    fn object(&mut self, depth: usize) -> Result<Json, String> {
        self.pos += 1; // '{'
        let mut fields = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b'}') {
            self.pos += 1;
            return Ok(Json::Obj(fields));
        }
        loop {
            self.skip_ws();
            let key = self.string()?;
            self.skip_ws();
            if self.peek() != Some(b':') {
                return Err("expected ':' in object".to_string());
            }
            self.pos += 1;
            self.skip_ws();
            let value = self.value(depth + 1)?;
            fields.push((key, value));
            self.skip_ws();
            match self.peek() {
                Some(b',') => self.pos += 1,
                Some(b'}') => {
                    self.pos += 1;
                    return Ok(Json::Obj(fields));
                }
                _ => return Err("expected ',' or '}' in object".to_string()),
            }
        }
    }
}

/// Successful acknowledgement returned by `POST /api/report`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ReportReceipt {
    /// GitHub issue URL the relay filed or deduplicated onto.
    pub issue_url: Option<String>,
    /// Optional relay-side receipt identifier.
    pub receipt_id: Option<String>,
    /// True when the relay merged this report into an existing issue
    /// (`duplicate` on the wire; `deduped` accepted as an alias).
    pub deduped: bool,
    /// Total reports the relay has stacked on this fingerprint.
    pub total_reports: Option<u64>,
    /// Fingerprint echoed back by the relay.
    pub fingerprint: Option<String>,
}

/// Parse the JSON ack returned by `report.js` (`{"ok":true,...}`). An
/// `{"ok":false,"error":"..."}` body surfaces the server-provided reason.
pub fn parse_report_receipt(body: &str) -> Result<ReportReceipt, String> {
    let value = parse_json(body)?;
    if !matches!(value, Json::Obj(_)) {
        return Err("receipt is not a JSON object".to_string());
    }
    match value.get("ok").and_then(Json::as_bool) {
        Some(true) => {}
        Some(false) => {
            let reason = value
                .get("error")
                .and_then(Json::as_str)
                .unwrap_or("relay rejected the report");
            return Err(reason.to_string());
        }
        None => return Err("receipt missing \"ok\" flag".to_string()),
    }
    let deduped = value
        .get("deduped")
        .or_else(|| value.get("duplicate"))
        .and_then(Json::as_bool)
        .unwrap_or(false);
    let total_reports = value
        .get("total_reports")
        .and_then(Json::as_f64)
        .filter(|n| *n >= 0.0)
        .map(|n| n as u64);
    Ok(ReportReceipt {
        issue_url: value
            .get("issue_url")
            .and_then(Json::as_str)
            .map(str::to_string),
        receipt_id: value
            .get("receipt_id")
            .and_then(Json::as_str)
            .map(str::to_string),
        deduped,
        total_reports,
        fingerprint: value
            .get("fingerprint")
            .and_then(Json::as_str)
            .map(str::to_string),
    })
}

// --- Local persistence -----------------------------------------------------

/// Canonical name of the markdown crash file for `fingerprint`.
pub fn crash_file_name(fingerprint: &str) -> String {
    format!("crash-{fingerprint}.md")
}

/// Canonical name of the queued offline payload for `fingerprint`.
pub fn pending_file_name(fingerprint: &str) -> String {
    format!("pending-{fingerprint}.json")
}

/// Canonical markdown crash-file path inside `crash_dir`.
pub fn crash_file_path(crash_dir: &Path, fingerprint: &str) -> PathBuf {
    crash_dir.join(crash_file_name(fingerprint))
}

/// Canonical queued-payload path inside `crash_dir`.
pub fn pending_file_path(crash_dir: &Path, fingerprint: &str) -> PathBuf {
    crash_dir.join(pending_file_name(fingerprint))
}

/// Write `bytes` to `dir/name` atomically (temp file + rename). If the
/// final file already exists it is left untouched — crash and pending
/// file names are fingerprint-addressed, so an existing file already
/// carries identical content.
fn write_atomic(dir: &Path, name: &str, bytes: &[u8]) -> std::io::Result<PathBuf> {
    fs::create_dir_all(dir)?;
    let final_path = dir.join(name);
    if final_path.exists() {
        return Ok(final_path);
    }
    let tmp_path = dir.join(format!(".{name}.tmp-{}", std::process::id()));
    {
        let mut file = fs::File::create(&tmp_path)?;
        let written = file.write_all(bytes).and_then(|()| file.sync_all());
        if let Err(err) = written {
            let _ = fs::remove_file(&tmp_path);
            return Err(err);
        }
    }
    if let Err(err) = fs::rename(&tmp_path, &final_path) {
        // Windows refuses rename-over-existing; retry once after removal.
        let _ = fs::remove_file(&final_path);
        match fs::rename(&tmp_path, &final_path) {
            Ok(()) => {}
            Err(_) => {
                let _ = fs::remove_file(&tmp_path);
                return Err(err);
            }
        }
    }
    Ok(final_path)
}

/// Persist the redacted markdown report as
/// `<crash_dir>/crash-<fingerprint>.md`, atomically.
pub fn write_local_report(crash_dir: &Path, report: &CrashReport) -> std::io::Result<PathBuf> {
    write_atomic(
        crash_dir,
        &crash_file_name(&report.fingerprint),
        report.markdown.as_bytes(),
    )
}

/// Persist the exact `megu-report/1` body as
/// `<crash_dir>/pending-<fingerprint>.json` for later retry.
pub fn write_pending_payload(
    crash_dir: &Path,
    report: &CrashReport,
) -> std::io::Result<PathBuf> {
    write_atomic(
        crash_dir,
        &pending_file_name(&report.fingerprint),
        report.payload_json.as_bytes(),
    )
}

// --- Session deduplication ---------------------------------------------------

/// In-memory per-session deduper: a fingerprint is submitted at most once
/// per process lifetime. Poison-tolerant so it stays usable from a panic
/// path.
#[derive(Debug, Default)]
pub struct SessionReportDeduper {
    sent: Mutex<HashSet<String>>,
}

impl SessionReportDeduper {
    /// Fresh, empty deduper.
    pub fn new() -> Self {
        Self::default()
    }

    fn sent_set(&self) -> std::sync::MutexGuard<'_, HashSet<String>> {
        self.sent
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Whether `fingerprint` was already submitted this session.
    pub fn has_sent(&self, fingerprint: &str) -> bool {
        self.sent_set().contains(fingerprint)
    }

    /// Record `fingerprint` as submitted.
    pub fn mark_sent(&self, fingerprint: &str) {
        self.sent_set().insert(fingerprint.to_string());
    }

    /// Atomically check-and-mark: `true` when this is the first claim of
    /// `fingerprint` this session.
    pub fn claim(&self, fingerprint: &str) -> bool {
        self.sent_set().insert(fingerprint.to_string())
    }
}

// --- Transport ---------------------------------------------------------------

/// One completed HTTP exchange: status line code plus the response body.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TransportResponse {
    /// HTTP status code (`200`, `400`, `500`, ...).
    pub status: u16,
    /// Response body (UTF-8 lossily decoded).
    pub body: String,
}

/// Why a submission attempt failed before a receipt could be produced.
#[derive(Debug)]
pub enum TransportError {
    /// URL failed the scheme/host/charset allowlist.
    UnsafeUrl(String),
    /// TCP connect failed or timed out.
    Connect(String),
    /// I/O error mid-request.
    Io(String),
    /// Malformed HTTP response.
    BadResponse(String),
    /// Required helper (`curl`) is not installed.
    ToolUnavailable(String),
    /// Helper exited non-zero or produced an unusable result.
    ToolFailed(String),
}

impl fmt::Display for TransportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TransportError::UnsafeUrl(m) => write!(f, "unsafe report url: {m}"),
            TransportError::Connect(m) => write!(f, "connect failed: {m}"),
            TransportError::Io(m) => write!(f, "io error: {m}"),
            TransportError::BadResponse(m) => write!(f, "bad http response: {m}"),
            TransportError::ToolUnavailable(m) => write!(f, "curl unavailable: {m}"),
            TransportError::ToolFailed(m) => write!(f, "curl failed: {m}"),
        }
    }
}

impl Error for TransportError {}

/// Wire interface for `POST /api/report`. The production transport is
/// [`StdHttpReportTransport`]; tests substitute a mock.
pub trait ReportTransport {
    /// POST `body` (a serialized `megu-report/1` payload) to `url`.
    /// Implementations must reject URLs outside their supported
    /// scheme/host policy rather than attempt the request.
    fn post_json(
        &self,
        url: &str,
        body: &str,
        timeout: Duration,
    ) -> Result<TransportResponse, TransportError>;
}

/// A validated report endpoint: `https://` for any host, or `http://`
/// restricted to literal loopback (`127.0.0.0/8`, `::1`, `localhost`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReportEndpoint {
    /// True for TLS endpoints (delivered through `curl`).
    pub https: bool,
    /// Bare host token (IPv6 without brackets).
    pub host: String,
    /// `Host:` header value (bracketed for IPv6, includes the port when
    /// non-default).
    pub authority: String,
    /// Resolved port.
    pub port: u16,
    /// Path + query, always starting with `/`.
    pub path: String,
}

fn is_loopback_host(host: &str) -> bool {
    host.eq_ignore_ascii_case("localhost")
        || host == "::1"
        || (host.starts_with("127.") && host[4..].bytes().all(|b| b.is_ascii_digit() || b == b'.'))
}

fn validate_host(host: &str) -> Result<(), String> {
    if host.is_empty() {
        return Err("empty host".to_string());
    }
    if host == "::1" {
        return Ok(());
    }
    let bytes = host.as_bytes();
    if bytes.len() > 253
        || !bytes
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || *b == b'-' || *b == b'.')
    {
        return Err(format!("invalid host {host:?}"));
    }
    if host.starts_with('-') || host.ends_with('-') {
        return Err(format!("invalid host {host:?}"));
    }
    Ok(())
}

fn validate_path(path: &str) -> Result<(), String> {
    // Conservative RFC-3986 subset: alphanumerics, unreserved marks, and
    // the query delimiters a fixed API endpoint needs. Quotes, `$`,
    // parens, backslashes, backticks, and `%` (percent-encoding can hide
    // traversal/whitespace from a downstream consumer) are excluded, and
    // whitespace/control bytes were already rejected upstream.
    if !path.starts_with('/') {
        return Err("path must start with '/'".to_string());
    }
    if path.len() > 1024 {
        return Err("path too long".to_string());
    }
    if path.contains("..") {
        return Err("dot-segments are not allowed in the report path".to_string());
    }
    if !path.bytes().all(|b| {
        b.is_ascii_alphanumeric()
            || matches!(
                b,
                b'-' | b'.'
                    | b'_'
                    | b'~'
                    | b'/'
                    | b'!'
                    | b'&'
                    | b'*'
                    | b'+'
                    | b','
                    | b';'
                    | b'='
                    | b':'
                    | b'@'
                    | b'?'
            )
    }) {
        return Err(format!("invalid path {path:?}"));
    }
    Ok(())
}

/// Validate and split a report endpoint URL. Only `https://<host>` and
/// loopback `http://127.x/` `http://[::1]/` `http://localhost/` pass;
/// everything else (`file:`, `ftp:`, public `http://`, credentials,
/// whitespace/control bytes, unicode) is rejected.
pub fn parse_report_url(url: &str) -> Result<ReportEndpoint, TransportError> {
    let unsafe_url = |reason: &str| TransportError::UnsafeUrl(reason.to_string());
    if url.is_empty() {
        return Err(unsafe_url("empty url"));
    }
    if !url.is_ascii() || url.bytes().any(|b| !(0x21..0x7f).contains(&b)) {
        return Err(unsafe_url("url contains control or non-ascii bytes"));
    }
    let (scheme, rest) = url
        .split_once("://")
        .ok_or_else(|| unsafe_url("missing '://' scheme separator"))?;
    let https = match scheme {
        "https" => true,
        "http" => false,
        other => return Err(unsafe_url(&format!("unsupported scheme {other:?}"))),
    };
    let (authority, path) = match rest.find('/') {
        Some(idx) => (&rest[..idx], &rest[idx..]),
        None => (rest, "/"),
    };
    if authority.is_empty() {
        return Err(unsafe_url("empty host"));
    }
    if authority.contains('@') || authority.contains('?') || authority.contains('#') {
        return Err(unsafe_url("credentials/fragment not allowed in authority"));
    }
    if path.contains('#') {
        return Err(unsafe_url("fragments are not sent"));
    }
    let (host, port) = if let Some(ipv6) = authority.strip_prefix('[') {
        let (inner, tail) = ipv6
            .split_once(']')
            .ok_or_else(|| unsafe_url("unterminated IPv6 literal"))?;
        if inner != "::1" {
            return Err(unsafe_url("only the ::1 loopback IPv6 literal is allowed"));
        }
        let port = match tail.strip_prefix(':') {
            Some(p) => p
                .parse::<u16>()
                .map_err(|_| unsafe_url("invalid port"))?,
            None => {
                if tail.is_empty() {
                    if https {
                        443
                    } else {
                        80
                    }
                } else {
                    return Err(unsafe_url("junk after IPv6 literal"));
                }
            }
        };
        (inner.to_string(), port)
    } else {
        match authority.split_once(':') {
            Some((h, p)) => {
                if p.is_empty() || !p.bytes().all(|b| b.is_ascii_digit()) {
                    return Err(unsafe_url("invalid port"));
                }
                let port = p
                    .parse::<u16>()
                    .map_err(|_| unsafe_url("invalid port"))?;
                (h.to_string(), port)
            }
            None => (
                authority.to_string(),
                if https {
                    443
                } else {
                    80
                },
            ),
        }
    };
    if port == 0 {
        return Err(unsafe_url("port must be 1..=65535"));
    }
    validate_host(&host).map_err(|e| unsafe_url(&e))?;
    validate_path(path).map_err(|e| unsafe_url(&e))?;
    if !https && !is_loopback_host(&host) {
        return Err(unsafe_url(
            "plain http is only allowed for literal loopback hosts",
        ));
    }
    let host_header = if host == "::1" {
        format!("[{host}]")
    } else {
        host.clone()
    };
    let default_port = if https { 443 } else { 80 };
    let authority_header = if port == default_port {
        host_header
    } else {
        format!("{host_header}:{port}")
    };
    Ok(ReportEndpoint {
        https,
        host,
        authority: authority_header,
        port,
        path: path.to_string(),
    })
}

/// Production [`ReportTransport`]: loopback `http://` over a raw
/// [`TcpStream`], `https://` through a system `curl` subprocess.
///
/// * `http://` is accepted only for literal loopback hosts; the request
///   is a hand-rolled `HTTP/1.1` POST with `Content-Length` and
///   `Connection: close`.
/// * `https://` shells out to `curl -q` (ignores `~/.curlrc`) with
///   `--proto =https`, strict `--connect-timeout`/`--max-time`, and the
///   JSON body piped over **stdin** (`--data-binary @-`) — never argv, so
///   report contents cannot leak through the process list. Proxy,
///   `HOME`, and `CURL_HOME` variables are stripped from the child
///   environment for determinism.
#[derive(Clone, Copy, Debug, Default)]
pub struct StdHttpReportTransport;

const HTTP_STATUS_MARKER: &str = "__MEGU_HTTP_STATUS__";

impl StdHttpReportTransport {
    /// Shared instance.
    pub fn new() -> Self {
        Self
    }

    fn post_loopback(
        &self,
        endpoint: &ReportEndpoint,
        body: &str,
        timeout: Duration,
    ) -> Result<TransportResponse, TransportError> {
        let addr: SocketAddr = (endpoint.host.as_str(), endpoint.port)
            .to_socket_addrs()
            .map_err(|e| {
                TransportError::Connect(format!("cannot resolve {}: {e}", endpoint.host))
            })?
            .next()
            .ok_or_else(|| {
                TransportError::Connect(format!("no address for {}", endpoint.host))
            })?;
        let mut stream = TcpStream::connect_timeout(&addr, timeout).map_err(|e| {
            TransportError::Connect(format!("{addr}: {e}"))
        })?;
        stream
            .set_read_timeout(Some(timeout))
            .map_err(|e| TransportError::Io(e.to_string()))?;
        stream
            .set_write_timeout(Some(timeout))
            .map_err(|e| TransportError::Io(e.to_string()))?;
        let request = format!(
            "POST {path} HTTP/1.1\r\n\
             Host: {authority}\r\n\
             User-Agent: mc-spine-crash-reporter/{version}\r\n\
             Content-Type: application/json\r\n\
             Accept: application/json\r\n\
             Content-Length: {len}\r\n\
             Connection: close\r\n\
             \r\n",
            path = endpoint.path,
            authority = endpoint.authority,
            version = env!("CARGO_PKG_VERSION"),
            len = body.len(),
        );
        stream
            .write_all(request.as_bytes())
            .and_then(|()| stream.write_all(body.as_bytes()))
            .map_err(|e| TransportError::Io(e.to_string()))?;
        read_http_response(&mut stream)
    }

    fn post_https_curl(
        &self,
        url: &str,
        body: &str,
        timeout: Duration,
    ) -> Result<TransportResponse, TransportError> {
        let secs = timeout.as_secs().max(1);
        let connect_secs = secs.min(10);
        let mut cmd = Command::new("curl");
        cmd.arg("-q") // ignore ~/.curlrc — must be the first flag
            .arg("-sS")
            .arg("-X")
            .arg("POST")
            .arg("-H")
            .arg("Content-Type: application/json")
            .arg("-H")
            .arg("Accept: application/json")
            .arg("--data-binary")
            .arg("@-")
            .arg("--connect-timeout")
            .arg(connect_secs.to_string())
            .arg("--max-time")
            .arg(secs.to_string())
            .arg("-w")
            .arg(format!("\n{HTTP_STATUS_MARKER}:%{{http_code}}"))
            .arg("--proto")
            .arg("=https")
            .arg("--url")
            .arg(url);
        for var in [
            "HOME",
            "CURL_HOME",
            "XDG_CONFIG_HOME",
            "HTTP_PROXY",
            "HTTPS_PROXY",
            "ALL_PROXY",
            "NO_PROXY",
            "http_proxy",
            "https_proxy",
            "all_proxy",
            "no_proxy",
        ] {
            cmd.env_remove(var);
        }
        let mut child = cmd
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| {
                if e.kind() == std::io::ErrorKind::NotFound {
                    TransportError::ToolUnavailable(
                        "curl binary not found; https submission unavailable".to_string(),
                    )
                } else {
                    TransportError::ToolFailed(format!("cannot spawn curl: {e}"))
                }
            })?;
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(body.as_bytes()); // closed on drop → curl EOF
        }
        let output = child
            .wait_with_output()
            .map_err(|e| TransportError::ToolFailed(format!("cannot wait for curl: {e}")))?;
        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        let marker = format!("{HTTP_STATUS_MARKER}:");
        let Some(marker_pos) = stdout.rfind(&marker) else {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            return Err(TransportError::ToolFailed(format!(
                "curl exited {:?} without a status marker: {stderr}",
                output.status.code()
            )));
        };
        let code_text = stdout[marker_pos + marker.len()..]
            .trim()
            .to_string();
        let status = code_text.parse::<u16>().map_err(|_| {
            TransportError::BadResponse(format!("unparseable curl status {code_text:?}"))
        })?;
        if status == 0 || !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            return Err(TransportError::ToolFailed(format!(
                "curl status {status} (exit {:?}): {stderr}",
                output.status.code()
            )));
        }
        Ok(TransportResponse {
            status,
            body: stdout[..marker_pos].trim_end_matches('\n').to_string(),
        })
    }
}

impl ReportTransport for StdHttpReportTransport {
    fn post_json(
        &self,
        url: &str,
        body: &str,
        timeout: Duration,
    ) -> Result<TransportResponse, TransportError> {
        let endpoint = parse_report_url(url)?;
        if endpoint.https {
            self.post_https_curl(url, body, timeout)
        } else {
            self.post_loopback(&endpoint, body, timeout)
        }
    }
}

// --- HTTP/1.1 response decoding (loopback transport) --------------------------

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack
        .windows(needle.len())
        .position(|w| w == needle)
}

fn read_http_response(stream: &mut TcpStream) -> Result<TransportResponse, TransportError> {
    let mut buf: Vec<u8> = Vec::with_capacity(8192);
    let mut chunk = [0u8; 8192];
    let head_end = loop {
        if let Some(pos) = find_subslice(&buf, b"\r\n\r\n") {
            break pos + 4;
        }
        let n = stream
            .read(&mut chunk)
            .map_err(|e| TransportError::Io(format!("read failed: {e}")))?;
        if n == 0 {
            return Err(TransportError::BadResponse(
                "connection closed before response headers".to_string(),
            ));
        }
        buf.extend_from_slice(&chunk[..n]);
        if buf.len() > MAX_RESPONSE_HEAD_BYTES {
            return Err(TransportError::BadResponse(
                "response headers exceed 64 KiB".to_string(),
            ));
        }
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
    let mut lines = head.split("\r\n");
    let status_line = lines.next().unwrap_or_default();
    let mut parts = status_line.split_whitespace();
    let version_ok = parts
        .next()
        .map(|v| v.starts_with("HTTP/"))
        .unwrap_or(false);
    let status = parts.next().and_then(|s| s.parse::<u16>().ok());
    if !version_ok {
        return Err(TransportError::BadResponse(format!(
            "not an HTTP status line: {status_line:?}"
        )));
    }
    let status = status.ok_or_else(|| {
        TransportError::BadResponse(format!("bad status line: {status_line:?}"))
    })?;

    let mut content_length: Option<usize> = None;
    let mut chunked = false;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let name = name.trim().to_ascii_lowercase();
        let value = value.trim();
        match name.as_str() {
            "content-length" => {
                content_length = value.parse::<usize>().ok();
            }
            "transfer-encoding" if value.to_ascii_lowercase().contains("chunked") => {
                chunked = true;
            }
            _ => {}
        }
    }

    let mut body_bytes = buf.split_off(head_end);

    // RFC 9112 §6.3: Transfer-Encoding overrides Content-Length when both
    // are present.
    let decoded: Vec<u8> = if chunked {
        let mut read_more = |body: &mut Vec<u8>| -> Result<usize, TransportError> {
            let n = stream
                .read(&mut chunk)
                .map_err(|e| TransportError::Io(format!("read failed: {e}")))?;
            body.extend_from_slice(&chunk[..n]);
            Ok(n)
        };
        decode_chunked(&mut body_bytes, &mut read_more)?
    } else if let Some(len) = content_length {
        while body_bytes.len() < len {
            let n = stream
                .read(&mut chunk)
                .map_err(|e| TransportError::Io(format!("read failed: {e}")))?;
            if n == 0 {
                break;
            }
            body_bytes.extend_from_slice(&chunk[..n]);
        }
        body_bytes.truncate(len);
        body_bytes
    } else {
        // Connection-close framing: drain until EOF or read timeout.
        loop {
            match stream.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => body_bytes.extend_from_slice(&chunk[..n]),
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) =>
                {
                    break
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => {
                    return Err(TransportError::Io(format!("read failed: {e}")));
                }
            }
        }
        body_bytes
    };
    Ok(TransportResponse {
        status,
        body: String::from_utf8_lossy(&decoded).to_string(),
    })
}

/// Decode a `Transfer-Encoding: chunked` body from `leftover` (bytes
/// already read past the headers). `read_more` appends bytes to the
/// buffer and returns how many were added; `0` means EOF.
fn decode_chunked(
    leftover: &mut Vec<u8>,
    read_more: &mut dyn FnMut(&mut Vec<u8>) -> Result<usize, TransportError>,
) -> Result<Vec<u8>, TransportError> {
    let bad = |m: &str| TransportError::BadResponse(m.to_string());
    let mut out = Vec::new();
    let mut consumed = 0usize;
    loop {
        // Locate the CRLF that terminates the chunk-size line.
        let line_end = loop {
            if let Some(pos) = find_subslice(&leftover[consumed..], b"\r\n") {
                break consumed + pos;
            }
            if leftover.len() - consumed > 16 {
                return Err(bad("oversized chunk-size line"));
            }
            if read_more(leftover)? == 0 {
                return Err(bad("eof inside chunked body"));
            }
        };
        let size_text = std::str::from_utf8(&leftover[consumed..line_end])
            .map_err(|_| bad("non-ascii chunk size"))?;
        let size_text = size_text.split(';').next().unwrap_or("").trim();
        let size = usize::from_str_radix(size_text, 16)
            .map_err(|_| bad(format!("bad chunk size {size_text:?}").as_str()))?;
        consumed = line_end + 2; // skip CRLF after the size
        if size == 0 {
            // Optional trailer lines, then a final CRLF.
            loop {
                match find_subslice(&leftover[consumed..], b"\r\n") {
                    Some(0) => break,
                    Some(pos) => consumed += pos + 2,
                    None => {
                        if read_more(leftover)? == 0 {
                            break; // body complete; tolerate truncated trailers
                        }
                    }
                }
            }
            return Ok(out);
        }
        while leftover.len() - consumed < size + 2 {
            if read_more(leftover)? == 0 {
                return Err(bad("eof inside chunk data"));
            }
        }
        out.extend_from_slice(&leftover[consumed..consumed + size]);
        consumed += size;
        if &leftover[consumed..consumed + 2] != b"\r\n" {
            return Err(bad("missing CRLF after chunk data"));
        }
        consumed += 2;
    }
}

// --- Panic hook ----------------------------------------------------------------

fn panic_payload_message(info: &PanicHookInfo<'_>) -> String {
    if let Some(s) = info.payload().downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = info.payload().downcast_ref::<String>() {
        s.clone()
    } else {
        "<non-string panic payload>".to_string()
    }
}

/// Install the crash-report panic hook: on any panic it captures a
/// redacted [`CrashReport`] and writes `<crash_dir>/crash-<fingerprint>.md`
/// atomically, then chains to the previously installed hook so the
/// default panic output is preserved. Every step is best-effort — the
/// hook itself never panics (a failing write is ignored), and the
/// previous hook is invoked inside `catch_unwind`.
pub fn install_panic_hook(crash_dir: PathBuf, env: ReportEnv) {
    let previous = panic::take_hook();
    panic::set_hook(Box::new(move |info: &PanicHookInfo<'_>| {
        let message = panic_payload_message(info);
        let site = info.location().map(|loc| PanicSite {
            file: loc.file().to_string(),
            line: loc.line(),
            column: loc.column(),
        });
        let report =
            capture_panic_context(&env, ReportKind::Crash, "panic", &message, site.as_ref());
        let _ = write_local_report(&crash_dir, &report);
        let _ = panic::catch_unwind(AssertUnwindSafe(|| previous(info)));
    }));
}

// --- Submission policy ---------------------------------------------------------

/// Parameters for one [`submit_crash_report`] call.
#[derive(Clone, Debug)]
pub struct SubmitRequest<'a> {
    /// Consent gate.
    pub policy: CrashSubmitPolicy,
    /// Configured relay endpoint (`https://...`, or loopback
    /// `http://127.0.0.1:<port>/api/report`); `None`/empty = unconfigured.
    pub report_api_url: Option<&'a str>,
    /// `crash-reports/` directory for local + offline files.
    pub crash_dir: &'a Path,
    /// UI-provided explicit consent for this specific report.
    pub user_confirmed: bool,
    /// Per-request transport timeout.
    pub timeout: Duration,
}

/// What [`submit_crash_report`] decided, and where the report landed.
#[derive(Clone, Debug, PartialEq)]
pub enum SubmitOutcome {
    /// The relay accepted the report; `receipt` carries `issue_url` etc.
    Sent {
        /// Local markdown copy.
        local_path: PathBuf,
        /// Parsed relay acknowledgement.
        receipt: ReportReceipt,
    },
    /// Policy requires explicit consent the caller has not given; the UI
    /// should show `preview_markdown` and retry with
    /// `user_confirmed == true`.
    AwaitingUserConsent {
        /// Local markdown copy.
        local_path: PathBuf,
        /// The exact redacted markdown the user would send for review.
        preview_markdown: String,
    },
    /// The report could not be delivered (no endpoint, unreachable,
    /// non-2xx, or malformed ack); the exact payload is preserved in
    /// `pending_path` for a later retry.
    SavedOffline {
        /// Local markdown copy.
        local_path: PathBuf,
        /// Queued `megu-report/1` payload.
        pending_path: PathBuf,
        /// Why submission failed.
        reason: String,
    },
    /// Policy or session dedup stopped the submission; the local file
    /// still exists.
    Blocked {
        /// Local markdown copy.
        local_path: PathBuf,
        /// Why submission was refused.
        reason: String,
    },
}

/// Apply the submission policy to `report` and attempt delivery through
/// `transport`.
///
/// Order of operations: persist the local markdown copy first (a report
/// is never dropped because of a policy), then evaluate the consent gate,
/// then session dedup, then the endpoint allowlist, then the transport.
/// Any post-consent failure preserves the payload via
/// [`write_pending_payload`] and returns [`SubmitOutcome::SavedOffline`].
pub fn submit_crash_report(
    report: &CrashReport,
    request: &SubmitRequest<'_>,
    transport: &dyn ReportTransport,
    deduper: &SessionReportDeduper,
) -> SubmitOutcome {
    let local_path = crash_file_path(request.crash_dir, &report.fingerprint);
    if let Err(err) = write_local_report(request.crash_dir, report) {
        return SubmitOutcome::Blocked {
            local_path,
            reason: format!("cannot write local crash file: {err}"),
        };
    }

    // Consent gate — evaluated before any endpoint or network handling.
    let needs_consent = match request.policy {
        CrashSubmitPolicy::Off => {
            return SubmitOutcome::Blocked {
                local_path,
                reason: "crash submission disabled by policy".to_string(),
            };
        }
        CrashSubmitPolicy::Ask => true,
        CrashSubmitPolicy::AutoCrashOnly => report.kind != ReportKind::Crash,
    };
    if needs_consent && !request.user_confirmed {
        return SubmitOutcome::AwaitingUserConsent {
            local_path,
            preview_markdown: report.markdown.clone(),
        };
    }

    // Session dedup: never re-send a fingerprint we already delivered.
    if deduper.has_sent(&report.fingerprint) {
        return SubmitOutcome::Blocked {
            local_path,
            reason: format!(
                "fingerprint {} already submitted this session",
                report.fingerprint
            ),
        };
    }

    let url = request.report_api_url.unwrap_or("").trim();
    if let Err(err) = parse_report_url(url) {
        return save_offline(
            request.crash_dir,
            report,
            local_path,
            format!("report endpoint rejected: {err}"),
        );
    }

    match transport.post_json(url, &report.payload_json, request.timeout) {
        Ok(response) if (200..300).contains(&response.status) => {
            match parse_report_receipt(&response.body) {
                Ok(receipt) => {
                    deduper.mark_sent(&report.fingerprint);
                    SubmitOutcome::Sent {
                        local_path,
                        receipt,
                    }
                }
                Err(err) => save_offline(
                    request.crash_dir,
                    report,
                    local_path,
                    format!("relay ack could not be parsed: {err}"),
                ),
            }
        }
        Ok(response) => save_offline(
            request.crash_dir,
            report,
            local_path,
            format!(
                "relay answered HTTP {}: {}",
                response.status,
                first_line_or(&redact_sensitive(&response.body), 120, "<empty body>")
            ),
        ),
        Err(err) => save_offline(request.crash_dir, report, local_path, err.to_string()),
    }
}

fn save_offline(
    crash_dir: &Path,
    report: &CrashReport,
    local_path: PathBuf,
    reason: String,
) -> SubmitOutcome {
    match write_pending_payload(crash_dir, report) {
        Ok(pending_path) => SubmitOutcome::SavedOffline {
            local_path,
            pending_path,
            reason,
        },
        Err(err) => SubmitOutcome::SavedOffline {
            local_path,
            pending_path: pending_file_path(crash_dir, &report.fingerprint),
            reason: format!("{reason} (pending payload also unwritable: {err})"),
        },
    }
}

#[cfg(test)]
mod reporter_tests {
    use super::*;

    fn test_env() -> ReportEnv {
        ReportEnv {
            mc_version: "26.3".to_string(),
            engine_version: "mc-spine-client 0.1.0".to_string(),
            os: "linux-x86_64".to_string(),
            culprit: "create".to_string(),
            subsystem: None,
            stage: None,
            gpu_class: None,
            top_frames: Vec::new(),
            mod_count: None,
        }
    }

    #[test]
    fn url_allowlist_accepts_https_and_loopback_only() {
        assert!(parse_report_url("https://reports.example.com/api/report").is_ok());
        let ep = parse_report_url("http://127.0.0.1:8787/api/report").unwrap();
        assert!(!ep.https);
        assert_eq!(ep.host, "127.0.0.1");
        assert_eq!(ep.port, 8787);
        assert_eq!(ep.path, "/api/report");
        assert!(parse_report_url("http://127.0.0.2:1/x").is_ok());
        assert!(parse_report_url("http://localhost:8787/x").is_ok());
        assert!(parse_report_url("http://[::1]:8787/x").is_ok());
    }

    #[test]
    fn url_allowlist_rejects_unsafe_inputs() {
        for bad in [
            "",
            "file:///etc/passwd",
            "ftp://127.0.0.1/x",
            "http://public.example.com/api/report",
            "https://user:pass@example.com/x",
            "https://example.com/x#frag",
            "https://example.com/x\r\nInjected: y",
            "https://example.com/\tpwn",
            "https://example.com/ -O /tmp/pwn",
            "https://example.com/`id`",
            "https://example.com/$(id)",
            "https://exa mple.com/x",
            "http://127.0.0.1:0/x",
            "http://127.0.0.1:99999/x",
            "https://[2001:db8::1]/x",
            "noscheme.example.com/x",
            "https://example.com/../%2e%2e",
            "https://例え.jp/x",
        ] {
            assert!(
                parse_report_url(bad).is_err(),
                "must reject {bad:?}"
            );
        }
    }

    #[test]
    fn receipt_parser_reads_ok_duplicate_and_error_bodies() {
        let ok = parse_report_receipt(
            r#"{"ok":true,"fingerprint":"MEGU-E-DEADBEEF","issue_url":"https://github.com/o/r/issues/7","duplicate":true,"total_reports":5,"receipt_id":"rc-1"}"#,
        )
        .unwrap();
        assert_eq!(
            ok.issue_url.as_deref(),
            Some("https://github.com/o/r/issues/7")
        );
        assert!(ok.deduped);
        assert_eq!(ok.total_reports, Some(5));
        assert_eq!(ok.receipt_id.as_deref(), Some("rc-1"));

        let err = parse_report_receipt(r#"{"ok":false,"error":"payload exceeds 4 KiB cap"}"#);
        assert_eq!(err.unwrap_err(), "payload exceeds 4 KiB cap");
        assert!(parse_report_receipt("<html>502</html>").is_err());
        assert!(parse_report_receipt("{}").is_err());
    }

    #[test]
    fn payload_respects_schema_caps() {
        let mut env = test_env();
        env.top_frames = (0..10).map(|i| format!("frame-{i} {}", "x".repeat(200))).collect();
        env.mod_count = Some(9000);
        env.subsystem = Some("s".repeat(200));
        let report = capture_panic_context(
            &env,
            ReportKind::Crash,
            "panic",
            &"detail ".repeat(4000),
            None,
        );
        assert!(report.payload_json.len() <= MAX_BODY_BYTES);
        assert!(report.markdown.len() <= CRASH_MARKDOWN_MAX_BYTES);
        assert!(report.payload_json.contains("\"schema\":\"megu-report/1\""));
        assert!(report.payload_json.contains("\"kind\":\"crash\""));
        assert!(!report.payload_json.contains(&"x".repeat(121)));
        assert!(report.payload_json.contains("\"mod_count\":4096"));
    }

    #[test]
    fn capture_redacts_and_fingerprints_like_core() {
        let env = test_env();
        let a = capture_panic_context(
            &env,
            ReportKind::Crash,
            "panic",
            "NPE at /home/alice/mods/x.jar Bearer tok_ABC 192.168.1.7",
            Some(&PanicSite {
                file: "src/world.rs".to_string(),
                line: 42,
                column: 9,
            }),
        );
        let b = capture_panic_context(
            &env,
            ReportKind::Crash,
            "panic",
            "NPE at /home/bob/mods/x.jar Bearer tok_ABC 192.168.1.7",
            Some(&PanicSite {
                file: "src/world.rs".to_string(),
                line: 42,
                column: 9,
            }),
        );
        assert_eq!(a.fingerprint, b.fingerprint);
        assert!(!a.markdown.contains("alice"));
        assert!(!a.markdown.contains("tok_ABC"));
        assert!(!a.markdown.contains("192.168.1.7"));
        assert!(!a.payload_json.contains("alice"));
        assert!(a.markdown.contains("src/world.rs:42:9"));
    }

    #[test]
    fn chunked_decoder_handles_split_and_trailers() {
        let wire = b"5\r\nhello\r\n6\r\n world\r\n0\r\nX-T: v\r\n\r\n".to_vec();
        let mut leftover = wire[..10].to_vec();
        let mut rest = wire[10..].to_vec();
        rest.reverse(); // pop() delivers in order
        let decoded = decode_chunked(&mut leftover, &mut |buf| {
            let mut added = 0usize;
            for _ in 0..3 {
                if let Some(b) = rest.pop() {
                    buf.push(b);
                    added += 1;
                }
            }
            Ok(added)
        })
        .unwrap();
        assert_eq!(decoded, b"hello world");
    }
}
