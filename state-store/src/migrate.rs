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
use megu_core::json::{parse_json_with_limits, JsonLimits, JsonValue};
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

/// Bounds for [`parse_data_json`]: the historical 64-level nesting cap.
/// The canonical reader counts the root value as depth 0 and rejects any
/// value at `max_depth`, so scalars at depth 64 — which the retired parser
/// allowed — now fail closed at an absurd depth. Node and byte ceilings
/// stay off: the retired parser had none, and world-metadata exports can
/// be large.
const JSON_LIMITS: JsonLimits = JsonLimits {
    max_depth: MAX_JSON_DEPTH,
    max_nodes: usize::MAX,
    max_bytes: usize::MAX,
};

/// Hard cap on JSON nesting depth. A hostile document must not drive the
/// recursive parser (or any downstream tree walk) into unbounded stack use;
/// 64 levels is already far past anything `level.dat` or chunk metadata
/// produces.
const MAX_JSON_DEPTH: usize = 64;

/// Parse a JSON document into the format-agnostic [`DataValue`] tree the
/// [`DataFixer`] operates on.
///
/// Delegates to the canonical `megu_core::json` reader: strict RFC 8259,
/// the whole input must be one document, duplicate object keys keep the
/// historical last-wins `BTreeMap` semantics. The `Int`/`Float` split stays
/// lexical — `64` and `64.0` are different [`DataValue`] shapes — so
/// [`number_lexemes`] re-reads the number spellings in document order for
/// [`json_to_data_value`]. Non-finite results (`1e999`) and integers past
/// `i64` fail closed.
pub fn parse_data_json(text: &str) -> Result<DataValue, JsonError> {
    let value = parse_json_with_limits(text, JSON_LIMITS, false)
        .map_err(|e| JsonError {
            offset: e.offset,
            message: e.message,
        })?;
    let lexemes = number_lexemes(text);
    let mut next = 0usize;
    let data = json_to_data_value(&value, &lexemes, &mut next)?;
    if next != lexemes.len() {
        // The side scan and the canonical tree disagree about how many
        // numbers the document holds — an internal invariant, never a
        // silently mis-typed tree.
        return Err(JsonError {
            offset: 0,
            message: "internal: number lexeme count mismatch".to_string(),
        });
    }
    Ok(data)
}

/// The document's number literals in order, skipping string contents
/// (escapes honored). The input is already validated by
/// [`parse_json_with_limits`], so literal boundaries are unambiguous: a
/// number runs until a byte outside `[0-9+-.eE]`, none of which can legally
/// follow one in a valid document.
fn number_lexemes(text: &str) -> Vec<&str> {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut i = 0usize;
    let mut in_string = false;
    while i < bytes.len() {
        match bytes[i] {
            b'"' => {
                in_string = !in_string;
                i += 1;
            }
            b'\\' if in_string => i += 2,
            b'-' | b'0'..=b'9' if !in_string => {
                let start = i;
                while i < bytes.len()
                    && matches!(bytes[i], b'0'..=b'9' | b'+' | b'-' | b'.' | b'e' | b'E')
                {
                    i += 1;
                }
                out.push(&text[start..i]);
            }
            _ => i += 1,
        }
    }
    out
}

fn json_number_error(message: &str) -> JsonError {
    JsonError {
        offset: 0,
        message: message.to_string(),
    }
}

fn json_to_data_value(
    value: &JsonValue,
    lexemes: &[&str],
    next: &mut usize,
) -> Result<DataValue, JsonError> {
    Ok(match value {
        JsonValue::Null => DataValue::Null,
        JsonValue::Bool(b) => DataValue::Bool(*b),
        JsonValue::Number(n) => {
            let lexeme = lexemes
                .get(*next)
                .copied()
                .ok_or_else(|| json_number_error("internal: missing number lexeme"))?;
            *next += 1;
            if lexeme.bytes().any(|b| matches!(b, b'.' | b'e' | b'E')) {
                if !n.is_finite() {
                    return Err(json_number_error("number is out of range"));
                }
                DataValue::Float(*n)
            } else {
                lexeme
                    .parse::<i64>()
                    .map(DataValue::Int)
                    .map_err(|_| json_number_error("integer is out of range"))?
            }
        }
        JsonValue::String(s) => DataValue::Str(s.clone()),
        JsonValue::Array(items) => DataValue::List(
            items
                .iter()
                .map(|item| json_to_data_value(item, lexemes, next))
                .collect::<Result<_, JsonError>>()?,
        ),
        JsonValue::Object(members) => {
            let mut map = std::collections::BTreeMap::new();
            for (key, member) in members {
                map.insert(key.clone(), json_to_data_value(member, lexemes, next)?);
            }
            DataValue::Map(map)
        }
    })
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
            ("", "unexpected end of input"),
            ("{", "unterminated object"),
            ("{\"a\": }", "expected a value"),
            ("{\"a\" 1}", "expected ':'"),
            ("{\"a\":1", "expected ',' or '}'"),
            ("[1,]", "expected a value"),
            ("tru", "bad literal"),
            ("nul", "bad literal"),
            ("01", "leading zeros"),
            ("+1", "expected a value"),
            ("1.", "bad number"),
            ("1e", "bad number"),
            ("-", "expected a value"),
            (".5", "expected a value"),
            ("\"unterminated", "unterminated string"),
            ("\"bad\\x\"", "bad escape"),
            ("\"\\ud83d\"", "lone surrogate"),
            ("\"\\ud83d\\u0041\"", "lone surrogate"),
            ("\"\\udc00\"", "lone surrogate"),
            ("9223372036854775808", "out of range"),
            ("1e999", "out of range"),
            ("{} []", "trailing characters"),
            ("[1] extra", "trailing characters"),
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
        assert!(parse_data_json(&deep).is_err());
        // Objects count too. The canonical reader counts scalars at the same
        // depth as containers, so a leaf at depth 64 now fails closed where
        // the retired parser allowed it.
        let mut doc = String::new();
        for _ in 0..MAX_JSON_DEPTH - 1 {
            doc.push_str("{\"a\":");
        }
        doc.push('0');
        for _ in 0..MAX_JSON_DEPTH - 1 {
            doc.push('}');
        }
        assert!(parse_data_json(&doc).is_ok());
        let mut too_deep = String::new();
        for _ in 0..MAX_JSON_DEPTH {
            too_deep.push_str("{\"a\":");
        }
        too_deep.push('0');
        for _ in 0..MAX_JSON_DEPTH {
            too_deep.push('}');
        }
        assert!(parse_data_json(&too_deep).is_err());
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
