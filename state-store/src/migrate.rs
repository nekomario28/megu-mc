// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! Operator-facing world-migration engine between the `1.21.1` (`3955`) and
//! `26.3` (`5023`) targets, built on the [`snapshot`] codec and the
//! [`datafix`] pipeline.
//!
//! Three services, one shared output path:
//!
//! * [`inspect_snapshot`] — validate an `STV1` blob end to end (magic, CRC32
//!   and every record, via [`decode_snapshot`]) and report what it carries.
//!   Validation is the same fail-closed path a restore takes, so `inspect`
//!   can never report a snapshot the build could not actually load.
//! * [`migrate_snapshot`] — decode fail-closed, then re-encode under the
//!   requested [`TargetVersion`]. `1.21.1` -> `26.3` is freely allowed; the
//!   `26.3` -> `1.21.1` direction exists only behind `allow_downgrade`,
//!   mirroring the explicit opt-in contract of
//!   [`datafix::DataFixer::v26_3_to_1_21_1_downgrade`]. Every output blob is
//!   round-trip decoded before it is returned: a migration can never emit a
//!   snapshot this build cannot read back.
//! * [`migrate_json_document`] — run a JSON world-metadata document (an
//!   exported `level.dat`, chunk metadata, datapack manifest) through the
//!   [`datafix::DataFixer`] using the minimal pure-`std` [`DataValue`] JSON
//!   front end ([`parse_data_json`] / [`to_data_json`]). Output is
//!   deterministic: `DataValue::Map` is a `BTreeMap`, so keys are emitted
//!   sorted, and numbers keep their parsed `Int`/`Float` shape.
//!
//! [`write_file_atomic`] is the shared output path for the CLI: write to a
//!   uniquely named temp file in the destination's own directory, `sync_all`,
//!   then rename. A failed migration therefore never leaves a torn output
//!   behind, and the input file is never opened for writing at all.

use crate::datafix::{DataFixError, DataFixReport, DataFixer, DataValue, DV_1_21_1};
use crate::sharded::ServerContext;
use crate::snapshot::{decode_snapshot, encode_snapshot, SnapshotError};
use megu_core::version::TargetVersion;
use std::error::Error;
use std::fmt;
use std::io::{self, Write};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

// ------------------------------------------------------------ summary types

/// What an `STV1` snapshot carries, in operator terms.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SnapshotSummary {
    /// Canonical Minecraft release id the snapshot's `data_version` pins
    /// (`"1.21.1"` or `"26.3"`).
    pub mc_version: String,
    /// Wire protocol the pinned target speaks.
    pub protocol_version: i32,
    /// `DataVersion` the snapshot was sealed under — its version identity.
    pub data_version: i32,
    pub dimension_count: usize,
    /// Total chunk overlays across every dimension.
    pub chunk_count: usize,
    /// Total block overrides across every chunk overlay.
    pub block_override_count: usize,
    /// Total block-entity payloads across every chunk overlay.
    pub block_entity_count: usize,
    /// Dimension names in sorted (wire) order.
    pub dimension_names: Vec<String>,
}

/// What one [`migrate_snapshot`] run did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SnapshotMigrationReport {
    /// Target the input snapshot was sealed under.
    pub from: TargetVersion,
    /// Target the output snapshot is sealed under.
    pub to: TargetVersion,
    /// Input and output share a target: the result is the canonical
    /// re-encode of the same payload (byte-identical for encoder-produced
    /// input), not a version change.
    pub already_at_target: bool,
    /// Content counts of the migrated snapshot — the same numbers
    /// [`inspect_snapshot`] reports on the output bytes.
    pub summary: SnapshotSummary,
    /// Input size in bytes.
    pub input_len: usize,
    /// Output size in bytes.
    pub output_len: usize,
}

/// Where a JSON document stopped being JSON.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JsonError {
    /// Byte offset into the input where parsing failed.
    pub offset: usize,
    /// What the parser expected or rejected there.
    pub message: String,
}

impl fmt::Display for JsonError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid JSON at byte {}: {}", self.offset, self.message)
    }
}

impl Error for JsonError {}

/// Why a migration (binary or JSON) did not happen.
#[derive(Clone, Debug, PartialEq)]
pub enum MigrationError {
    /// The input bytes are not a clean `STV1` snapshot: bad magic, CRC
    /// mismatch, truncation, a version the registry does not pin, or a
    /// record count past the codec caps.
    CorruptSnapshot(SnapshotError),
    /// A `26.3` -> `1.21.1` migration was requested without the explicit
    /// `allow_downgrade` opt-in. Refusing is the default: downgrades can
    /// lose data the older format cannot represent.
    DowngradeNotAllowed {
        /// `DataVersion` the input carries.
        from_dv: i32,
        /// `DataVersion` the caller asked for.
        to_dv: i32,
    },
    /// The [`DataFixer`] refused the JSON document (no `DataVersion`, a
    /// malformed one, or a document newer than the pipeline's ceiling).
    Fix(DataFixError),
    /// The input text is not valid JSON.
    Json(JsonError),
    /// A freshly encoded snapshot failed its own decode check. This is an
    /// internal invariant — it should be unreachable — and it is checked so
    /// a bug can never ship an unreadable output as a successful migration.
    OutputInvalid(SnapshotError),
}

impl fmt::Display for MigrationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MigrationError::CorruptSnapshot(e) => write!(f, "corrupt snapshot: {e}"),
            MigrationError::DowngradeNotAllowed { from_dv, to_dv } => write!(
                f,
                "refusing to downgrade data version {from_dv} -> {to_dv} without explicit opt-in"
            ),
            MigrationError::Fix(e) => write!(f, "document migration refused: {e}"),
            MigrationError::Json(e) => write!(f, "{e}"),
            MigrationError::OutputInvalid(e) => {
                write!(f, "internal error: migrated snapshot failed decode: {e}")
            }
        }
    }
}

impl Error for MigrationError {}

impl From<SnapshotError> for MigrationError {
    fn from(e: SnapshotError) -> Self {
        MigrationError::CorruptSnapshot(e)
    }
}

impl From<DataFixError> for MigrationError {
    fn from(e: DataFixError) -> Self {
        MigrationError::Fix(e)
    }
}

impl From<JsonError> for MigrationError {
    fn from(e: JsonError) -> Self {
        MigrationError::Json(e)
    }
}

// ------------------------------------------------------------------ inspect

/// Content counts of a decoded context, computed through the same sorted,
/// shard-locked iteration the encoder uses.
fn summarize(ctx: &ServerContext, target: TargetVersion) -> SnapshotSummary {
    let dimension_names = ctx.dimension_names();
    let mut chunk_count = 0usize;
    let mut block_override_count = 0usize;
    let mut block_entity_count = 0usize;
    for name in &dimension_names {
        if let Some(dim) = ctx.dimension(name) {
            dim.for_each_overlay(|_, overlay| {
                chunk_count += 1;
                block_override_count += overlay.blocks().len();
                block_entity_count += overlay.block_entities().len();
            });
        }
    }
    SnapshotSummary {
        mc_version: target.id().to_string(),
        protocol_version: target.protocol(),
        data_version: target.data_version(),
        dimension_count: dimension_names.len(),
        chunk_count,
        block_override_count,
        block_entity_count,
        dimension_names,
    }
}

/// Validate an `STV1` snapshot and report what it carries.
///
/// The validation is [`decode_snapshot`] itself — CRC32 first, then header,
/// then every capped record — so a [`SnapshotSummary`] is only ever produced
/// for bytes this build can restore verbatim.
pub fn inspect_snapshot(bytes: &[u8]) -> Result<SnapshotSummary, SnapshotError> {
    let (ctx, target) = decode_snapshot(bytes)?;
    Ok(summarize(&ctx, target))
}

// ------------------------------------------------------------------ migrate

/// Migrate an `STV1` snapshot to `to`.
///
/// `1.21.1` -> `26.3` re-encodes the decoded context under the new target
/// (the `data_version`/`protocol`/`mc_version` header is the snapshot's
/// whole version identity; chunk overlays are version-agnostic `u32` state
/// ids and opaque block-entity payloads). `26.3` -> `1.21.1` is the same
/// re-encode but only behind `allow_downgrade == true`. `from == to`
/// returns the canonical re-encode — byte-identical when the input was
/// produced by [`encode_snapshot`] — flagged with `already_at_target`.
///
/// Whatever the direction, the output is decoded once more before being
/// returned: the CRC32 seal and every record are re-verified, so the
/// function's contract is "valid `STV1` in, valid `STV1` out, or an error".
pub fn migrate_snapshot(
    bytes: &[u8],
    to: TargetVersion,
    allow_downgrade: bool,
) -> Result<(Vec<u8>, SnapshotMigrationReport), MigrationError> {
    let (ctx, from) = decode_snapshot(bytes).map_err(MigrationError::CorruptSnapshot)?;
    if from == TargetVersion::V26_3 && to == TargetVersion::V1_21_1 && !allow_downgrade {
        return Err(MigrationError::DowngradeNotAllowed {
            from_dv: from.data_version(),
            to_dv: to.data_version(),
        });
    }
    let out = encode_snapshot(&ctx, to);
    // Round-trip check on the bytes about to leave: the output must decode
    // cleanly at exactly the requested target. `encode_snapshot` is
    // deterministic, so this can only fail on an internal codec bug — which
    // is precisely what it exists to catch.
    let (_, sealed) = decode_snapshot(&out).map_err(MigrationError::OutputInvalid)?;
    debug_assert_eq!(sealed, to);
    let report = SnapshotMigrationReport {
        from,
        to,
        already_at_target: from == to,
        summary: summarize(&ctx, to),
        input_len: bytes.len(),
        output_len: out.len(),
    };
    Ok((out, report))
}

// ------------------------------------------------------------------ JSON in

/// Hard cap on JSON nesting depth. A hostile document must not drive the
/// recursive parser (or any downstream tree walk) into unbounded stack use;
/// 64 levels is already far past anything `level.dat` or chunk metadata
/// produces.
const MAX_JSON_DEPTH: usize = 64;

struct JsonParser<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> JsonParser<'a> {
    fn err(&self, message: impl Into<String>) -> JsonError {
        JsonError {
            offset: self.pos,
            message: message.into(),
        }
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn skip_ws(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.pos += 1;
        }
    }

    /// Expect `literal` at the cursor ("true", "false", "null").
    fn literal(&mut self, literal: &'static [u8], value: DataValue) -> Result<DataValue, JsonError> {
        if self.bytes[self.pos..].starts_with(literal) {
            self.pos += literal.len();
            Ok(value)
        } else {
            Err(self.err("expected a JSON value"))
        }
    }

    fn value(&mut self, depth: usize) -> Result<DataValue, JsonError> {
        self.skip_ws();
        match self.peek() {
            None => Err(self.err("expected a JSON value")),
            Some(b'{') => self.object(depth),
            Some(b'[') => self.array(depth),
            Some(b'"') => Ok(DataValue::Str(self.string()?)),
            Some(b't') => self.literal(b"true", DataValue::Bool(true)),
            Some(b'f') => self.literal(b"false", DataValue::Bool(false)),
            Some(b'n') => self.literal(b"null", DataValue::Null),
            Some(b'-' | b'0'..=b'9') => self.number(),
            Some(_) => Err(self.err("expected a JSON value")),
        }
    }

    /// Entering a container at `depth` enclosing containers: the depth cap
    /// is checked here so scalars at maximum depth still parse.
    fn container_open(&self, depth: usize) -> Result<(), JsonError> {
        if depth >= MAX_JSON_DEPTH {
            return Err(self.err(format!(
                "JSON nesting deeper than {MAX_JSON_DEPTH} levels"
            )));
        }
        Ok(())
    }

    fn object(&mut self, depth: usize) -> Result<DataValue, JsonError> {
        self.container_open(depth)?;
        self.pos += 1; // '{'
        let mut map = std::collections::BTreeMap::new();
        self.skip_ws();
        if self.peek() == Some(b'}') {
            self.pos += 1;
            return Ok(DataValue::Map(map));
        }
        loop {
            self.skip_ws();
            if self.peek() != Some(b'"') {
                return Err(self.err("expected an object key"));
            }
            let key = self.string()?;
            self.skip_ws();
            if self.peek() != Some(b':') {
                return Err(self.err("expected ':' after object key"));
            }
            self.pos += 1;
            let value = self.value(depth + 1)?;
            map.insert(key, value);
            self.skip_ws();
            match self.peek() {
                Some(b',') => self.pos += 1,
                Some(b'}') => {
                    self.pos += 1;
                    return Ok(DataValue::Map(map));
                }
                _ => return Err(self.err("expected ',' or '}' in object")),
            }
        }
    }

    fn array(&mut self, depth: usize) -> Result<DataValue, JsonError> {
        self.container_open(depth)?;
        self.pos += 1; // '['
        let mut items = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b']') {
            self.pos += 1;
            return Ok(DataValue::List(items));
        }
        loop {
            items.push(self.value(depth + 1)?);
            self.skip_ws();
            match self.peek() {
                Some(b',') => self.pos += 1,
                Some(b']') => {
                    self.pos += 1;
                    return Ok(DataValue::List(items));
                }
                _ => return Err(self.err("expected ',' or ']' in array")),
            }
        }
    }

    /// A JSON string starting at the opening quote. Byte-level scanning is
    /// safe: the input is a `&str` (already UTF-8), and the only bytes with
    /// syntactic meaning — `"`, `\` and control bytes below 0x20 — can never
    /// appear inside a multi-byte UTF-8 sequence, so runs copied between
    /// them stay valid UTF-8.
    fn string(&mut self) -> Result<String, JsonError> {
        self.pos += 1; // opening '"'
        let mut out = String::new();
        loop {
            let run_start = self.pos;
            while let Some(b) = self.peek() {
                if b == b'"' || b == b'\\' || b < 0x20 {
                    break;
                }
                self.pos += 1;
            }
            // The slice is valid UTF-8 by the argument above; a failure here
            // would mean the scanner split a multi-byte sequence, which is
            // an internal bug — still reported, never unwrapped.
            let run = std::str::from_utf8(&self.bytes[run_start..self.pos])
                .map_err(|_| self.err("invalid UTF-8 in string"))?;
            out.push_str(run);
            match self.peek() {
                None => return Err(self.err("unterminated string")),
                Some(b'"') => {
                    self.pos += 1;
                    return Ok(out);
                }
                Some(b'\\') => {
                    self.pos += 1;
                    self.escape(&mut out)?;
                }
                Some(_) => return Err(self.err("unescaped control character in string")),
            }
        }
    }

    /// The byte after `\`. `\uXXXX` sequences are combined into scalar
    /// values; a lone surrogate is malformed JSON for this parser's purposes
    /// and refused rather than lossily replaced.
    fn escape(&mut self, out: &mut String) -> Result<(), JsonError> {
        match self.peek() {
            Some(b'"') => out.push('"'),
            Some(b'\\') => out.push('\\'),
            Some(b'/') => out.push('/'),
            Some(b'b') => out.push('\u{8}'),
            Some(b'f') => out.push('\u{c}'),
            Some(b'n') => out.push('\n'),
            Some(b'r') => out.push('\r'),
            Some(b't') => out.push('\t'),
            Some(b'u') => {
                self.pos += 1;
                let hi = self.hex4()?;
                let cp = if (0xD800..=0xDBFF).contains(&hi) {
                    // A high surrogate must be followed by \uDC00-\uDFFF.
                    if self.peek() == Some(b'\\') {
                        self.pos += 1;
                    }
                    if self.peek() != Some(b'u') {
                        return Err(self.err("lone high surrogate in \\u escape"));
                    }
                    self.pos += 1;
                    let lo = self.hex4()?;
                    if !(0xDC00..=0xDFFF).contains(&lo) {
                        return Err(self.err("expected low surrogate after \\uXXXX"));
                    }
                    0x1_0000 + ((hi - 0xD800) << 10) + (lo - 0xDC00)
                } else if (0xDC00..=0xDFFF).contains(&hi) {
                    return Err(self.err("lone low surrogate in \\u escape"));
                } else {
                    hi
                };
                // Every value outside the surrogate range (already handled)
                // and within u16 is a valid scalar.
                out.push(char::from_u32(cp).ok_or_else(|| self.err("invalid \\u escape"))?);
                return Ok(());
            }
            _ => return Err(self.err("invalid escape sequence")),
        }
        self.pos += 1;
        Ok(())
    }

    fn hex4(&mut self) -> Result<u32, JsonError> {
        let mut value = 0u32;
        for _ in 0..4 {
            let b = self.peek().ok_or_else(|| self.err("truncated \\u escape"))?;
            let digit = match b {
                b'0'..=b'9' => b - b'0',
                b'a'..=b'f' => b - b'a' + 10,
                b'A'..=b'F' => b - b'A' + 10,
                _ => return Err(self.err("non-hex digit in \\u escape")),
            };
            value = value * 16 + u32::from(digit);
            self.pos += 1;
        }
        Ok(value)
    }

    /// `-?(0|[1-9][0-9]*)(\.[0-9]+)?([eE][+-]?[0-9]+)?` — the JSON number
    /// grammar, kept strict (no `+1`, no `.5`, no leading zeros). Integer
    /// literals become `Int` and must fit `i64`; fraction/exponent literals
    /// become `Float` and must stay finite. Both overflow cases fail closed
    /// rather than silently changing the value.
    fn number(&mut self) -> Result<DataValue, JsonError> {
        let start = self.pos;
        if self.peek() == Some(b'-') {
            self.pos += 1;
        }
        match self.peek() {
            Some(b'0') => {
                self.pos += 1;
                if matches!(self.peek(), Some(b'0'..=b'9')) {
                    return Err(self.err("leading zeros are not valid JSON"));
                }
            }
            Some(b'1'..=b'9') => {
                while matches!(self.peek(), Some(b'0'..=b'9')) {
                    self.pos += 1;
                }
            }
            _ => return Err(self.err("expected a digit")),
        }
        let mut is_float = false;
        if self.peek() == Some(b'.') {
            is_float = true;
            self.pos += 1;
            if !matches!(self.peek(), Some(b'0'..=b'9')) {
                return Err(self.err("expected a digit after '.'"));
            }
            while matches!(self.peek(), Some(b'0'..=b'9')) {
                self.pos += 1;
            }
        }
        if matches!(self.peek(), Some(b'e') | Some(b'E')) {
            is_float = true;
            self.pos += 1;
            if matches!(self.peek(), Some(b'+') | Some(b'-')) {
                self.pos += 1;
            }
            if !matches!(self.peek(), Some(b'0'..=b'9')) {
                return Err(self.err("expected a digit in the exponent"));
            }
            while matches!(self.peek(), Some(b'0'..=b'9')) {
                self.pos += 1;
            }
        }
        // The accepted grammar is pure ASCII, so this slice is always &str.
        let text = std::str::from_utf8(&self.bytes[start..self.pos])
            .map_err(|_| self.err("invalid number"))?;
        if is_float {
            let v: f64 = text
                .parse()
                .map_err(|_| self.err("invalid number"))?;
            if !v.is_finite() {
                return Err(self.err("number is out of range"));
            }
            Ok(DataValue::Float(v))
        } else {
            text.parse::<i64>()
                .map(DataValue::Int)
                .map_err(|_| self.err("integer is out of range"))
        }
    }
}

/// Parse a JSON document into the format-agnostic [`DataValue`] tree the
/// [`DataFixer`] operates on.
///
/// Strict and fail-closed: the full JSON grammar (strings with `\uXXXX`
/// surrogate pairs, the exact number grammar), a 64-level nesting cap, no
/// trailing bytes after the document, integer literals bounded by `i64` and
/// floats bounded by `is_finite`.
pub fn parse_data_json(text: &str) -> Result<DataValue, JsonError> {
    let mut parser = JsonParser {
        bytes: text.as_bytes(),
        pos: 0,
    };
    let value = parser.value(0)?;
    parser.skip_ws();
    if parser.pos != parser.bytes.len() {
        return Err(parser.err("trailing bytes after the JSON document"));
    }
    Ok(value)
}

// ----------------------------------------------------------------- JSON out

fn write_json_string(text: &str, out: &mut String) {
    out.push('"');
    for ch in text.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

fn write_json_value(value: &DataValue, out: &mut String) {
    match value {
        DataValue::Null => out.push_str("null"),
        DataValue::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        DataValue::Int(v) => out.push_str(&v.to_string()),
        // JSON has no spelling for NaN or the infinities — and `{:?}` would
        // emit the non-JSON tokens `NaN`/`inf` — so non-finite floats become
        // `null`. Finite floats use `{:?}`: shortest round-trip form, and
        // always a valid JSON number (`1.0`, `1e300`, `-3.5e-7`).
        DataValue::Float(v) if v.is_finite() => out.push_str(&format!("{v:?}")),
        DataValue::Float(_) => out.push_str("null"),
        DataValue::Str(s) => write_json_string(s, out),
        DataValue::List(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_json_value(item, out);
            }
            out.push(']');
        }
        DataValue::Map(map) => {
            out.push('{');
            // BTreeMap iteration is sorted-key order: the output document is
            // byte-deterministic for a given tree.
            for (i, (key, item)) in map.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_json_string(key, out);
                out.push(':');
                write_json_value(item, out);
            }
            out.push('}');
        }
    }
}

/// Serialize a [`DataValue`] tree to deterministic JSON: sorted object keys
/// (the `BTreeMap` guarantees it), minimal escaping, no insignificant
/// whitespace.
pub fn to_data_json(value: &DataValue) -> String {
    let mut out = String::new();
    write_json_value(value, &mut out);
    out
}

// ------------------------------------------------------------- migrate JSON

/// The `DataVersion` the document's root map declares, with the same
/// fail-closed reading the fixer performs.
fn document_data_version(root: &DataValue) -> Result<i32, DataFixError> {
    match root.as_map() {
        None => Err(DataFixError::RootNotMap),
        Some(map) => match map.get("DataVersion") {
            None => Err(DataFixError::MissingDataVersion),
            Some(v) => v
                .as_int()
                .map(|dv| i32::try_from(dv).unwrap_or(i32::MAX))
                .ok_or(DataFixError::MalformedDataVersion),
        },
    }
}

/// Migrate a JSON world-metadata document to `to`.
///
/// The document's `DataVersion` picks the direction: toward `26.3` runs
/// [`DataFixer::v1_21_1_to_26_3`]; toward `1.21.1` runs the explicit
/// downgrade pipeline, which any `file_dv` above `3955` reaches only when
/// `allow_downgrade` is set — a document already at `3955` passes through as
/// a clean no-op without the flag. Documents carrying a `DataVersion` no
/// pipeline covers keep the fixer's own semantics (warn and leave
/// unchanged, or [`DataFixError::NewerThanTarget`] when newer than the
/// ceiling).
pub fn migrate_json_document(
    text: &str,
    to: TargetVersion,
    allow_downgrade: bool,
) -> Result<(String, DataFixReport), MigrationError> {
    let mut root = parse_data_json(text)?;
    let file_dv = document_data_version(&root)?;
    let fixer = match to {
        TargetVersion::V26_3 => DataFixer::v1_21_1_to_26_3(),
        TargetVersion::V1_21_1 => {
            if file_dv > DV_1_21_1 && !allow_downgrade {
                return Err(MigrationError::DowngradeNotAllowed {
                    from_dv: file_dv,
                    to_dv: DV_1_21_1,
                });
            }
            DataFixer::v26_3_to_1_21_1_downgrade()
        }
    };
    let report = fixer.fix(&mut root)?;
    Ok((to_data_json(&root), report))
}

// ------------------------------------------------------------ atomic write

/// Temp-file name uniqueness within one process: PID distinguishes
/// processes, the counter distinguishes writes. No clock involved — a
/// filename never needs to be a timestamp, only unique.
static TEMP_FILE_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Atomically replace `path` with `bytes`.
///
/// The bytes land in a uniquely named temp file in the destination's own
/// directory (so the rename stays on one filesystem), are `sync_all`ed, and
/// only then renamed over `path`. Any failure before the rename removes the
/// temp file and leaves `path` — input or a previous output — exactly as it
/// was; the rename itself is the only operation that can touch it, and a
/// rename either happens fully or not at all.
pub fn write_file_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let parent = match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    };
    let name = path.file_name().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{} has no file name", path.display()),
        )
    })?;
    let pid = std::process::id();
    let mut file = None;
    let mut tmp = parent.join("unreachable");
    for _ in 0..64 {
        let seq = TEMP_FILE_COUNTER.fetch_add(1, Ordering::Relaxed);
        let candidate = parent.join(format!(".{}.migrate-tmp-{pid}-{seq}", name.to_string_lossy()));
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
        {
            Ok(f) => {
                file = Some(f);
                tmp = candidate;
                break;
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    let mut file = file.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::AlreadyExists,
            "could not allocate a unique temp file name",
        )
    })?;
    let result = file.write_all(bytes).and_then(|()| file.sync_all());
    drop(file);
    if let Err(e) = result {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    // The rename's own directory entry must reach disk too, or a crash can
    // resurrect the pre-write file even though every staged byte fsynced.
    // Best-effort: where opening a directory as a file fails (Windows) the
    // rename is already durable, so the failure is a safe no-op.
    if let Ok(dir) = std::fs::File::open(parent) {
        let _ = dir.sync_all();
    }
    Ok(())
}

/// Remove orphaned `.migrate-tmp-*` staging files from `dir`, returning how
/// many were deleted.
///
/// [`write_file_atomic`] names its temp files `.<name>.migrate-tmp-<pid>-<seq>`
/// in the destination's own directory; a crash between the `sync_all` and the
/// rename leaves one behind. Orphans are never adopted — a torn staging file
/// is indistinguishable from a complete one — they are debris the next
/// migration would have replaced anyway, so reopen-time cleanup just deletes
/// them. A non-directory `dir` reports `Ok(0)`; an unreadable directory
/// reports the listing error.
pub fn cleanup_stale_migrate_temps(dir: &Path) -> io::Result<usize> {
    if !dir.is_dir() {
        return Ok(0);
    }
    let mut removed = 0usize;
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let is_stale_tmp = entry
            .file_name()
            .to_str()
            .is_some_and(|name| name.starts_with('.') && name.contains(".migrate-tmp-"));
        if is_stale_tmp && entry.file_type().is_ok_and(|t| t.is_file()) {
            std::fs::remove_file(entry.path())?;
            removed += 1;
        }
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::datafix::{DV_1_21_1, DV_26_3};
    use crate::sharded::BlockCoord;
    use crate::snapshot::encode_snapshot;
    use crate::ChunkCoord;

    fn map(entries: &[(&str, DataValue)]) -> DataValue {
        DataValue::Map(
            entries
                .iter()
                .map(|(k, v)| (k.to_string(), v.clone()))
                .collect(),
        )
    }

    /// A two-dimension context with blocks and block entities in both.
    fn populated_context() -> ServerContext {
        let ctx = ServerContext::new();
        let overworld = ctx.ensure_dimension("minecraft:overworld");
        overworld.set_block(BlockCoord::new(8, 64, -8), 42);
        overworld.set_block(BlockCoord::new(9, 64, -8), 43);
        overworld.set_block(BlockCoord::new(-33, 200, 8), 7);
        overworld.with_chunks_write(&[ChunkCoord::new(0, -1)], |set| {
            set.get_mut(ChunkCoord::new(0, -1))
                .unwrap()
                .set_block_entity((8, 64, 8), b"{id:\"minecraft:chest\"}".to_vec());
        });
        let nether = ctx.ensure_dimension("minecraft:the_nether");
        nether.set_block(BlockCoord::new(0, 32, 0), 5);
        nether.set_block(BlockCoord::new(0, 33, 0), 5);
        ctx
    }

    // --------------------------------------------------------- inspection

    #[test]
    fn inspect_reports_version_and_content_counts() {
        for target in TargetVersion::ALL {
            let bytes = encode_snapshot(&populated_context(), target);
            let s = inspect_snapshot(&bytes).unwrap();
            assert_eq!(s.mc_version, target.id());
            assert_eq!(s.protocol_version, target.protocol());
            assert_eq!(s.data_version, target.data_version());
            assert_eq!(s.dimension_count, 2);
            assert_eq!(
                s.dimension_names,
                vec!["minecraft:overworld", "minecraft:the_nether"]
            );
            assert_eq!(s.chunk_count, 3); // overworld: 0,-1 and -3,0 ; nether: 0,0
            assert_eq!(s.block_override_count, 5);
            assert_eq!(s.block_entity_count, 1);
        }
    }

    #[test]
    fn inspect_fails_closed_on_a_flipped_crc_bit() {
        let bytes = encode_snapshot(&populated_context(), TargetVersion::V1_21_1);
        for i in [4, 20, bytes.len() - 5] {
            let mut bad = bytes.clone();
            bad[i] ^= 0x01;
            assert!(
                matches!(
                    inspect_snapshot(&bad),
                    Err(SnapshotError::CrcMismatch { .. })
                ),
                "flip at byte {i}"
            );
        }
        assert_eq!(
            inspect_snapshot(b"not a snapshot"),
            Err(SnapshotError::BadMagic)
        );
        assert_eq!(inspect_snapshot(b"STV"), Err(SnapshotError::Truncated));
    }

    // --------------------------------------------------------- migration

    #[test]
    fn snapshot_upgrade_1_21_1_to_26_3_reseals_at_the_new_target() {
        let input = encode_snapshot(&populated_context(), TargetVersion::V1_21_1);
        let (out, report) = migrate_snapshot(&input, TargetVersion::V26_3, false).unwrap();

        assert_eq!(report.from, TargetVersion::V1_21_1);
        assert_eq!(report.to, TargetVersion::V26_3);
        assert!(!report.already_at_target);
        assert_eq!(report.input_len, input.len());
        assert_eq!(report.output_len, out.len());
        assert_eq!(report.summary.data_version, DV_26_3);
        assert_eq!(report.summary.chunk_count, 3);

        // The output decodes at 26.3 with every record intact — and is
        // exactly what encode_snapshot produces for the same context.
        let (ctx, target) = decode_snapshot(&out).unwrap();
        assert_eq!(target, TargetVersion::V26_3);
        assert_eq!(ctx.dimension_names(), report.summary.dimension_names);
        assert_eq!(out, encode_snapshot(&ctx, TargetVersion::V26_3));
        // Version identity lives in the header: payload sections differ only
        // there, so the migrated snapshot is not the input's bytes.
        assert_ne!(out, input);
    }

    #[test]
    fn snapshot_same_target_returns_byte_identical_canonical_bytes() {
        for target in TargetVersion::ALL {
            let input = encode_snapshot(&populated_context(), target);
            let (out, report) = migrate_snapshot(&input, target, false).unwrap();
            assert!(report.already_at_target);
            assert_eq!(out, input, "canonical re-encode must be a fixed point");
            // The no-op path needs no opt-in even when the target is 1.21.1.
        }
    }

    #[test]
    fn snapshot_downgrade_requires_explicit_opt_in() {
        let input = encode_snapshot(&populated_context(), TargetVersion::V26_3);
        assert_eq!(
            migrate_snapshot(&input, TargetVersion::V1_21_1, false).unwrap_err(),
            MigrationError::DowngradeNotAllowed {
                from_dv: DV_26_3,
                to_dv: DV_1_21_1,
            }
        );

        let (out, report) =
            migrate_snapshot(&input, TargetVersion::V1_21_1, true).unwrap();
        assert!(!report.already_at_target);
        assert_eq!(report.summary.data_version, DV_1_21_1);
        let (_, target) = decode_snapshot(&out).unwrap();
        assert_eq!(target, TargetVersion::V1_21_1);

        // Content survives the round trip: upgrade the result back and the
        // canonical 26.3 bytes are recovered.
        let (back, _) = migrate_snapshot(&out, TargetVersion::V26_3, false).unwrap();
        assert_eq!(back, input);
    }

    #[test]
    fn snapshot_migration_fails_closed_on_corrupt_input() {
        let input = encode_snapshot(&populated_context(), TargetVersion::V1_21_1);
        let mut bad = input.clone();
        bad[10] ^= 0x01;
        assert!(matches!(
            migrate_snapshot(&bad, TargetVersion::V26_3, false).unwrap_err(),
            MigrationError::CorruptSnapshot(SnapshotError::CrcMismatch { .. })
        ));
    }

    // --------------------------------------------------------- JSON codec

    #[test]
    fn json_parses_every_value_kind_and_escapes() {
        let doc = r#"{
            "int": -42,
            "big": 9223372036854775807,
            "float": 3.5,
            "exp": -1.25e2,
            "str": "a\"b\\c\nd€😀",
            "esc": "\u0041\u00e9\ud83d\ude00",
            "list": [1, "two", null, true, false],
            "obj": {"nested": {"deep": []}},
            "nil": null
        }"#;
        let root = parse_data_json(doc).unwrap();
        let m = root.as_map().unwrap();
        assert_eq!(m["int"], DataValue::Int(-42));
        assert_eq!(m["big"], DataValue::Int(i64::MAX));
        assert_eq!(m["float"], DataValue::Float(3.5));
        assert_eq!(m["exp"], DataValue::Float(-125.0));
        assert_eq!(
            m["str"],
            DataValue::Str("a\"b\\c\nd€\u{1F600}".to_string())
        );
        assert_eq!(m["esc"], DataValue::Str("Aé\u{1F600}".to_string()));
        assert_eq!(
            m["list"],
            DataValue::List(vec![
                DataValue::Int(1),
                DataValue::Str("two".to_string()),
                DataValue::Null,
                DataValue::Bool(true),
                DataValue::Bool(false),
            ])
        );
        assert_eq!(m["nil"], DataValue::Null);
        assert!(matches!(m["obj"], DataValue::Map(_)));
    }

    #[test]
    fn json_rejects_malformed_documents() {
        for (doc, needle) in [
            ("", "expected a JSON value"),
            ("{", "expected an object key"),
            ("{\"a\": }", "expected a JSON value"),
            ("{\"a\" 1}", "expected ':'"),
            ("{\"a\":1", "expected ',' or '}'"),
            ("[1,]", "expected a JSON value"),
            ("tru", "expected a JSON value"),
            ("nul", "expected a JSON value"),
            ("01", "leading zeros"),
            ("+1", "expected a JSON value"),
            ("1.", "expected a digit after '.'"),
            ("1e", "expected a digit in the exponent"),
            ("-", "expected a digit"),
            (".5", "expected a JSON value"),
            ("\"unterminated", "unterminated string"),
            ("\"bad\\x\"", "invalid escape"),
            ("\"\\ud83d\"", "lone high surrogate"),
            ("\"\\ud83d\\u0041\"", "expected low surrogate"),
            ("\"\\udc00\"", "lone low surrogate"),
            ("9223372036854775808", "out of range"),
            ("1e999", "out of range"),
            ("{} []", "trailing bytes"),
            ("[1] extra", "trailing bytes"),
        ] {
            let err = parse_data_json(doc).unwrap_err();
            assert!(
                err.message.contains(needle),
                "{doc:?} -> {err:?}, wanted {needle:?}"
            );
        }
        // An unescaped literal control character inside a string.
        assert!(parse_data_json("\"a\tb\"").is_err());
    }

    #[test]
    fn json_nesting_is_capped_at_64() {
        let ok = "[".repeat(MAX_JSON_DEPTH) + &"]".repeat(MAX_JSON_DEPTH);
        assert!(parse_data_json(&ok).is_ok());
        let deep = "[".repeat(MAX_JSON_DEPTH + 1) + &"]".repeat(MAX_JSON_DEPTH + 1);
        let err = parse_data_json(&deep).unwrap_err();
        assert!(err.message.contains("deeper than 64"), "{err:?}");
        // Objects count too.
        let mut doc = String::new();
        for _ in 0..MAX_JSON_DEPTH {
            doc.push_str("{\"a\":");
        }
        doc.push('0');
        for _ in 0..MAX_JSON_DEPTH {
            doc.push('}');
        }
        assert!(parse_data_json(&doc).is_ok());
    }

    #[test]
    fn json_roundtrip_is_deterministic_and_sorted() {
        let doc = r#"{"z":1,"a":{"y":[true,null,2.5],"x":"s"},"b":"\u0041"}"#;
        let parsed = parse_data_json(doc).unwrap();
        let out = to_data_json(&parsed);
        // Sorted keys, unescaped non-ASCII passthrough, minimal spacing.
        assert_eq!(out, r#"{"a":{"x":"s","y":[true,null,2.5]},"b":"A","z":1}"#);
        // And the serializer's output parses back to the same tree.
        assert_eq!(parse_data_json(&out).unwrap(), parsed);
    }

    #[test]
    fn json_serializer_escapes_and_covers_non_finite() {
        let value = map(&[
            ("s", DataValue::Str("q\"\\\u{7}\n".to_string())),
            ("nan", DataValue::Float(f64::NAN)),
            ("inf", DataValue::Float(f64::INFINITY)),
            ("ninf", DataValue::Float(f64::NEG_INFINITY)),
            ("f", DataValue::Float(0.5)),
        ]);
        let out = to_data_json(&value);
        assert_eq!(
            out,
            r#"{"f":0.5,"inf":null,"nan":null,"ninf":null,"s":"q\"\\\u0007\n"}"#
        );
        // Everything emitted still parses as JSON.
        let back = parse_data_json(&out).unwrap();
        let m = back.as_map().unwrap();
        assert_eq!(m["nan"], DataValue::Null);
        assert_eq!(m["f"], DataValue::Float(0.5));
    }

    // ------------------------------------------------- migrate_json_document

    /// A 1.21.1-era JSON export mixing level.dat metadata, a chunk palette
    /// and item components — the three shapes the CLI migrates for real.
    fn world_metadata_json_3955() -> String {
        r#"{
            "DataVersion": 3955,
            "Data": {
                "DataVersion": 3955,
                "Version": {"Id": 3955, "Name": "1.21.1", "Snapshot": false},
                "RandomSeed": -42
            },
            "sections": [
                {
                    "y": 0,
                    "block_states": {
                        "palette": [
                            {"id": "minecraft:grass", "properties": {"snowy": "false"}},
                            {"Name": "minecraft:snow"},
                            {"id": "create:brass_casing"}
                        ]
                    },
                    "biomes": {"palette": ["minecraft:plains", "aether:skyroot_meadow"]}
                }
            ],
            "inventory": [
                {
                    "id": "minecraft:boat",
                    "count": 1,
                    "components": {
                        "minecraft:enchantments": {"minecraft:sharpness": 5},
                        "create:schematic": {"progress": 4}
                    }
                }
            ],
            "pack": {"pack_format": 48, "description": "world pack"},
            "mod_id": "create:shaft"
        }"#
        .to_string()
    }

    #[test]
    fn json_document_3955_to_5023_full_migration() {
        let (out, report) =
            migrate_json_document(&world_metadata_json_3955(), TargetVersion::V26_3, false)
                .unwrap();
        assert_eq!(report.original_dv, DV_1_21_1);
        assert_eq!(report.final_dv, DV_26_3);
        assert_eq!(report.renamed_entries, 3); // grass, snow, boat
        assert_eq!(
            report.applied_steps,
            vec![
                "block_state_key_normalize",
                "level_dat_version_metadata",
                "chunk_section_layout",
                "item_components_upgrade",
                "pack_format_min_max",
                "stamp_data_version_26_3",
            ]
        );

        let doc = parse_data_json(&out).unwrap();
        let m = doc.as_map().unwrap();
        assert_eq!(m["DataVersion"], DataValue::Int(DV_26_3 as i64));
        let data = m["Data"].as_map().unwrap();
        assert_eq!(data["DataVersion"], DataValue::Int(DV_26_3 as i64));
        assert_eq!(
            data["Version"].as_map().unwrap()["Name"],
            DataValue::Str("26.3".to_string())
        );
        assert_eq!(data["RandomSeed"], DataValue::Int(-42));
        let section = match &m["sections"] {
            DataValue::List(v) => v[0].as_map().unwrap(),
            _ => panic!(),
        };
        assert!(!section.contains_key("y"));
        assert_eq!(section["Y"], DataValue::Int(0));
        let palette = match &section["block_states"].as_map().unwrap()["palette"] {
            DataValue::List(v) => v,
            _ => panic!(),
        };
        assert_eq!(
            palette[0].as_map().unwrap()["Name"],
            DataValue::Str("minecraft:short_grass".to_string())
        );
        assert_eq!(
            palette[1].as_map().unwrap()["Name"],
            DataValue::Str("minecraft:snow_layer".to_string())
        );
        // Modded entries keep their namespace; only keys normalized.
        assert_eq!(
            palette[2].as_map().unwrap()["Name"],
            DataValue::Str("create:brass_casing".to_string())
        );
        let item = match &m["inventory"] {
            DataValue::List(v) => v[0].as_map().unwrap(),
            _ => panic!(),
        };
        assert_eq!(item["id"], DataValue::Str("minecraft:oak_boat".to_string()));
        let ench = item["components"].as_map().unwrap()["minecraft:enchantments"]
            .as_map()
            .unwrap();
        assert!(matches!(ench["levels"], DataValue::Map(_)));
        assert_eq!(
            item["components"].as_map().unwrap()["create:schematic"],
            map(&[("progress", DataValue::Int(4))])
        );
        assert_eq!(
            m["pack"].as_map().unwrap()["min_format"],
            DataValue::List(vec![DataValue::Int(121), DataValue::Int(0)])
        );
        assert_eq!(m["mod_id"], DataValue::Str("create:shaft".to_string()));
        // Output is deterministic: sorted keys, byte-identical re-run.
        let (out2, _) =
            migrate_json_document(&world_metadata_json_3955(), TargetVersion::V26_3, false)
                .unwrap();
        assert_eq!(out, out2);
    }

    #[test]
    fn json_document_already_at_target_is_a_clean_noop() {
        // At 5023 toward 26.3: no steps, no warnings, flag not needed.
        let (up, _) =
            migrate_json_document(&world_metadata_json_3955(), TargetVersion::V26_3, false)
                .unwrap();
        let (out, report) = migrate_json_document(&up, TargetVersion::V26_3, false).unwrap();
        assert!(report.applied_steps.is_empty());
        assert!(report.warnings.is_empty());
        assert_eq!(out, up);
        // At 3955 toward 1.21.1: same — no downgrade flag required for a no-op.
        let src = world_metadata_json_3955();
        let (out, report) =
            migrate_json_document(&src, TargetVersion::V1_21_1, false).unwrap();
        assert!(report.applied_steps.is_empty());
        assert!(report.warnings.is_empty());
        assert_eq!(out, to_data_json(&parse_data_json(&src).unwrap()));
    }

    #[test]
    fn json_document_downgrade_needs_the_flag_and_roundtrips() {
        let src = world_metadata_json_3955();
        let (up, _) = migrate_json_document(&src, TargetVersion::V26_3, false).unwrap();

        assert_eq!(
            migrate_json_document(&up, TargetVersion::V1_21_1, false).unwrap_err(),
            MigrationError::DowngradeNotAllowed {
                from_dv: DV_26_3,
                to_dv: DV_1_21_1,
            }
        );

        let (down, report) =
            migrate_json_document(&up, TargetVersion::V1_21_1, true).unwrap();
        assert_eq!(report.final_dv, DV_1_21_1);
        assert_eq!(report.renamed_entries, 3); // short_grass, snow_layer, oak_boat
        let doc = parse_data_json(&down).unwrap();
        let m = doc.as_map().unwrap();
        assert_eq!(m["DataVersion"], DataValue::Int(DV_1_21_1 as i64));
        assert_eq!(
            m["Data"].as_map().unwrap()["Version"].as_map().unwrap()["Name"],
            DataValue::Str("1.21.1".to_string())
        );
        assert_eq!(
            m["pack"].as_map().unwrap()["pack_format"],
            DataValue::Int(48)
        );
        assert!(!m["pack"].as_map().unwrap().contains_key("min_format"));
        let item = match &m["inventory"] {
            DataValue::List(v) => v[0].as_map().unwrap(),
            _ => panic!(),
        };
        assert_eq!(item["id"], DataValue::Str("minecraft:boat".to_string()));
        // Flat enchantment map restored, modded component preserved.
        assert_eq!(
            item["components"].as_map().unwrap()["minecraft:enchantments"],
            map(&[("minecraft:sharpness", DataValue::Int(5))])
        );
        // Re-upgrading the downgrade reproduces the canonical 26.3 document
        // byte-for-byte: the pipeline pair is a fixed point on this shape.
        let (up_again, _) =
            migrate_json_document(&down, TargetVersion::V26_3, false).unwrap();
        assert_eq!(up_again, up);
    }

    #[test]
    fn json_document_fail_closed_on_malformed_and_versionless_input() {
        // Malformed JSON: the fixer is never reached.
        assert!(matches!(
            migrate_json_document("{nope", TargetVersion::V26_3, false).unwrap_err(),
            MigrationError::Json(_)
        ));
        // Valid JSON without a DataVersion the fixer can key on.
        assert_eq!(
            migrate_json_document("{\"foo\": 1}", TargetVersion::V26_3, false).unwrap_err(),
            MigrationError::Fix(DataFixError::MissingDataVersion)
        );
        assert_eq!(
            migrate_json_document("[1, 2]", TargetVersion::V26_3, false).unwrap_err(),
            MigrationError::Fix(DataFixError::RootNotMap)
        );
        // A document newer than everything the pipeline knows.
        assert_eq!(
            migrate_json_document("{\"DataVersion\": 6000}", TargetVersion::V26_3, false)
                .unwrap_err(),
            MigrationError::Fix(DataFixError::NewerThanTarget {
                file_dv: 6000,
                target_dv: DV_26_3,
            })
        );
        // ...and even the opted-in downgrade pipeline cannot represent it.
        assert_eq!(
            migrate_json_document("{\"DataVersion\": 6000}", TargetVersion::V1_21_1, true)
                .unwrap_err(),
            MigrationError::Fix(DataFixError::NewerThanTarget {
                file_dv: 6000,
                target_dv: DV_26_3,
            })
        );
    }

    #[test]
    fn json_document_preserves_modded_namespaces() {
        let src = r#"{
            "DataVersion": 3955,
            "blocks": ["create:brass_casing", "aether:skyroot_planks", "minecraft:snow"],
            "custom": {"deep": {"id": "create:shaft"}}
        }"#;
        let (out, report) = migrate_json_document(src, TargetVersion::V26_3, false).unwrap();
        assert_eq!(report.renamed_entries, 1); // only minecraft:snow
        let doc = parse_data_json(&out).unwrap();
        assert_eq!(
            doc.as_map().unwrap()["blocks"],
            DataValue::List(vec![
                DataValue::Str("create:brass_casing".to_string()),
                DataValue::Str("aether:skyroot_planks".to_string()),
                DataValue::Str("minecraft:snow_layer".to_string()),
            ])
        );
    }

    // --------------------------------------------------------- atomic write

    fn test_dir(tag: &str) -> std::path::PathBuf {
        static DIR_COUNTER: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "megu-migrate-test-{}-{}-{tag}",
            std::process::id(),
            DIR_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn atomic_write_installs_bytes_and_survives_overwrite() {
        let dir = test_dir("atomic");
        let path = dir.join("out.bin");
        write_file_atomic(&path, b"first").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"first");
        // Replacing an existing output is atomic too.
        write_file_atomic(&path, b"second").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"second");
        // No temp litter is left behind.
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(leftovers, vec![std::ffi::OsString::from("out.bin")]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn atomic_write_failure_never_touches_the_input() {
        let dir = test_dir("atomic-fail");
        // Writing into a directory that does not exist fails cleanly.
        let missing = dir.join("no-such-dir").join("out.bin");
        assert!(write_file_atomic(&missing, b"x").is_err());
        assert!(!dir.join("no-such-dir").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn stale_migrate_temps_are_swept_without_touching_committed_files() {
        let dir = test_dir("stale-temps");
        // Two staged-write orphans from crashed runs plus a committed file and
        // a look-alike name that must survive the sweep.
        std::fs::write(dir.join(".out.bin.migrate-tmp-1-0"), b"torn").unwrap();
        std::fs::write(dir.join(".snap.stv.migrate-tmp-9-3"), b"also torn").unwrap();
        std::fs::write(dir.join("out.bin"), b"committed").unwrap();
        std::fs::write(dir.join("notes.migrate-tmp"), b"not ours").unwrap();
        assert_eq!(cleanup_stale_migrate_temps(&dir).unwrap(), 2);
        assert_eq!(std::fs::read(dir.join("out.bin")).unwrap(), b"committed");
        assert_eq!(
            std::fs::read(dir.join("notes.migrate-tmp")).unwrap(),
            b"not ours"
        );
        assert!(!dir.join(".out.bin.migrate-tmp-1-0").exists());
        // A missing directory is an empty answer, not an error.
        assert_eq!(cleanup_stale_migrate_temps(&dir.join("nope")).unwrap(), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
