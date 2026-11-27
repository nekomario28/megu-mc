// SPDX-License-Identifier: LGPL-3.0-or-later
#![deny(unsafe_code)]
//! Universal `data/<name>.dat` `SavedData` store — vanilla + Mod auxiliary data.
//!
//! Vanilla saves its non-chunk world state as small `data/` documents:
//! `scoreboard.dat`, `raids.dat`, `map_<id>.dat`, and whatever a Mod adds
//! (`data/<mod>_<name>.dat`). Every one of them is the same envelope —
//! `SavedData`'s `save(CompoundTag)` writes `data` (a compound owned by the
//! subclass) plus `DataVersion`, and a Mod's subclass is free to add its own
//! root-level keys next to them.
//!
//! [`SavedDataEnvelope`] models exactly that envelope over
//! [`crate::datafix::DataValue`] and classifies its root keys with
//! [`unknown_keys_to_preserve`]: the declarative
//! [`crate::vanilla_schema::SchemaScope::AuxScoreboard`] /
//! [`crate::vanilla_schema::SchemaScope::AuxRaids`] /
//! [`crate::vanilla_schema::SchemaScope::AuxMap`] rows declare `data` and
//! `DataVersion`, so anything else a document carries is parked in
//! [`SavedDataEnvelope::preserved`] and written back verbatim —
//! a removed Mod's payload is not silently dropped by a save cycle, and a
//! newer vanilla's extra envelope field survives an older loader.
//!
//! [`SavedDataStore`] owns a bounded set of envelopes, one per file, and is
//! **fail-closed** at every cap: an oversized payload, an oversized store, too
//! many files, too many keys, an over-deep tree, an unsafe file name, or a
//! malformed document is an error, never a truncated write. Loading and saving
//! a `data/` directory is a whole-store operation so a hostile `data/` cannot
//! smuggle a path outside it (only `[A-Za-z0-9_.-]` names are accepted and the
//! caller supplies the directory).
//!
//! A `data/` directory is persisted as one *generation*: a save stages every
//! document, installs each staged file with an atomic rename, and publishes the
//! sealed [`SAVED_DATA_MANIFEST_FILE`] last. The manifest carries a monotonic
//! generation number plus the store's **deletion tombstones**, so a removed
//! `data/<name>.dat` is not resurrected by a stale copy of it, and a failed or
//! truncated write (ENOSPC, quota, unreadable target) leaves the previous
//! generation's documents *and* its manifest untouched instead of publishing a
//! half-updated directory. The manifest is not a `.dat` document, so vanilla
//! ignores it.
//!
//! The generation/tombstone rules here and the ones in [`crate::delta_world`]
//! (`mdwg` records plus the `mgm1` index) and in the binary world-snapshot
//! generations (`MWS1`, `src/save_v2.rs`) are the same four rules — a
//! deterministic CRC32-sealed container, a monotonic generation id, an atomic
//! rename publish, and explicit absence records — over different payloads, so
//! each keeps its own container and schema. Collapsing them into one manifest
//! means one commit point spanning `data/`, the world directory's overlays and
//! the delta store, i.e. a save-orchestration decision owned by the tick loop in
//! `src/main.rs`; [`SavedDataStore::save_to_dir_with`] is the writer seam that
//! commit would drive for the `data/` leg.
//!
//! Everything is pure `std`; the on-disk form is the real Vanilla one —
//! RFC 1952 gzip-compressed binary NBT (or raw binary NBT) as written by
//! `SavedData::save(CompoundTag)` into `<world>/data/<name>.dat`. The NBT
//! bridge is the crate's own [`crate::anvil_inspect`] codec (`gzip_decompress`,
//! `decode_nbt_to_data_value`, `encode_data_value_to_nbt`,
//! `gzip_encode_stored`), so a document written here is readable by Vanilla and
//! a Vanilla/Mod document is readable here.
//!
//! The JSON front end ([`SavedDataEnvelope::from_json_text`] /
//! [`SavedDataEnvelope::to_json_text`]) is **internal debug/inspection only** —
//! it is never used for a `.dat` file on disk. It reuses the crate's canonical
//! [`crate::migrate::parse_data_json`] / [`crate::migrate::to_data_json`] codec
//! so a document round-trips through the same number-lexeme-preserving tree the
//! migration pipeline uses.

use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use megu_core::hash::crc32_ieee;

use crate::anvil_inspect::{
    decode_nbt_to_data_value, encode_data_value_to_nbt, gzip_decompress, gzip_encode_stored,
};
use crate::datafix::DataValue;
use crate::migrate::{parse_data_json, to_data_json, write_file_atomic};
use crate::vanilla_schema::{unknown_keys_to_preserve, SchemaScope};

/// Most `data/*.dat` documents one [`SavedDataStore`] may hold.
pub const MAX_SAVED_DATA_FILES: usize = 256;
/// Most deletion tombstones one store may record. Tombstones outlive the files
/// they name, so this is a separate budget from [`MAX_SAVED_DATA_FILES`].
pub const MAX_SAVED_DATA_TOMBSTONES: usize = 1024;
/// Longest accepted file stem (`map_123456789` is 13 bytes).
pub const MAX_SAVED_DATA_NAME_BYTES: usize = 96;
/// Most keys one envelope (its `data` compound plus preserved root keys) may
/// carry.
pub const MAX_SAVED_DATA_KEYS: usize = 4096;
/// Largest single envelope, measured by the deterministic
/// [`SavedDataEnvelope::envelope_bytes`] estimate.
pub const MAX_SAVED_DATA_ENVELOPE_BYTES: usize = 16 * 1024 * 1024;
/// Largest aggregate of every envelope in one store.
pub const MAX_SAVED_DATA_STORE_BYTES: usize = 64 * 1024 * 1024;
/// Deepest nested `DataValue` tree one envelope may carry.
pub const MAX_SAVED_DATA_DEPTH: usize = 32;
/// First two bytes of an RFC 1952 gzip member; a `.dat` file starting with
/// them is inflated before its NBT is decoded, everything else is raw NBT.
pub const GZIP_MAGIC: [u8; 2] = [0x1f, 0x8b];

/// Vanilla envelope key holding the subclass payload.
pub const SAVED_DATA_PAYLOAD_KEY: &str = "data";
/// Vanilla envelope key holding the save `DataVersion`.
pub const SAVED_DATA_VERSION_KEY: &str = "DataVersion";

/// Sealed generation manifest inside a `data/` directory. Deliberately *not*
/// a `.dat` document: vanilla's `DimensionDataStorage` only reads
/// `data/<name>.dat`, so both vanilla and [`SavedDataStore::load_dir`] ignore
/// this file when enumerating documents.
pub const SAVED_DATA_MANIFEST_FILE: &str = "saved_data_manifest.msd1";
/// Magic of the sealed generation manifest.
pub const SAVED_DATA_MANIFEST_MAGIC: [u8; 4] = *b"MSD1";
/// The only manifest schema this implementation reads and writes.
pub const SAVED_DATA_MANIFEST_SCHEMA: u16 = 1;
/// Largest accepted manifest document. It holds at most
/// [`MAX_SAVED_DATA_TOMBSTONES`] tombstones of [`MAX_SAVED_DATA_NAME_BYTES`]
/// each.
pub const MAX_SAVED_DATA_MANIFEST_BYTES: usize = 256 * 1024;
/// Suffix appended to a document or manifest this crate refuses, so the
/// refused bytes stay on disk for recovery instead of being overwritten.
pub const QUARANTINE_SUFFIX: &str = ".corrupt";
/// Prefix of the transient staging directory one save writes through.
const STAGING_PREFIX: &str = ".saved-data-staging-";

/// Process-wide staging-directory discriminator. Two stores saving the same
/// directory in one process must never share a staging directory, or one
/// save's cleanup could delete another save's staged documents.
static STAGING_SEQ: AtomicU64 = AtomicU64::new(0);

/// Why a [`SavedDataEnvelope`] / [`SavedDataStore`] operation refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SavedDataError {
    /// The file stem was empty (`data/.dat`).
    EmptyName,
    /// The file stem exceeds [`MAX_SAVED_DATA_NAME_BYTES`].
    NameTooLong { bytes: usize, max: usize },
    /// The file stem carries anything but `[A-Za-z0-9_.-]`, so it cannot name
    /// a path inside `data/`.
    InvalidName(String),
    /// The document root is not a compound/object (a JSON array, an NBT
    /// scalar or list root), so it cannot be a `SavedData` envelope.
    NotAnObject,
    /// The required `data` compound is absent or not a compound.
    MissingDataCompound,
    /// The required `DataVersion` integer is absent or not an integer.
    MissingDataVersion,
    /// `DataVersion` is negative — vanilla never writes one, so it is a
    /// corruption signal rather than something to preserve.
    NegativeDataVersion(i32),
    /// `DataVersion` does not fit the `i32` vanilla writes (a 64-bit JSON
    /// integer that no `SavedData` document can carry).
    DataVersionOutOfRange(i64),
    /// More keys than [`MAX_SAVED_DATA_KEYS`].
    TooManyKeys { keys: usize, max: usize },
    /// One envelope above [`MAX_SAVED_DATA_ENVELOPE_BYTES`].
    PayloadTooLarge { bytes: usize, max: usize },
    /// The store's total above [`MAX_SAVED_DATA_STORE_BYTES`].
    StoreTooLarge { bytes: usize, max: usize },
    /// More files than [`MAX_SAVED_DATA_FILES`].
    TooManyFiles { files: usize, max: usize },
    /// A `DataValue` tree deeper than [`MAX_SAVED_DATA_DEPTH`].
    TooDeep { max: usize },
    /// The caller tried to write a reserved envelope key through
    /// [`SavedDataEnvelope::put_field`].
    ReservedKey(String),
    /// A preserved key collides with a vanilla envelope key.
    ReservedPreservedKey(String),
    /// The schema declares a root key this envelope does not model, so the
    /// document cannot be loaded without risking a silent drop.
    UnmodeledEnvelopeKey(String),
    /// A `data/<name>.dat` document is not a valid gzip/NBT (or debug JSON)
    /// document.
    MalformedDocument { path: PathBuf, message: String },
    /// The sealed generation manifest exists but cannot be trusted: bad magic,
    /// unsupported schema, truncated body, trailing bytes, unsorted or unsafe
    /// tombstone names, or a CRC32 mismatch.
    MalformedManifest { path: PathBuf, message: String },
    /// More deletion tombstones than [`MAX_SAVED_DATA_TOMBSTONES`].
    TooManyTombstones { count: usize, max: usize },
    /// The envelope tree cannot be written as binary NBT: a `Null` value, a
    /// heterogeneous list, or a document past the NBT size cap.
    NbtEncode { message: String },
    /// Filesystem failure while loading or saving.
    Io { path: PathBuf, message: String },
}

impl fmt::Display for SavedDataError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyName => f.write_str("SavedData file name is empty"),
            Self::NameTooLong { bytes, max } => write!(
                f,
                "SavedData file name is {bytes} bytes, over the {max}-byte cap"
            ),
            Self::InvalidName(name) => write!(
                f,
                "SavedData file name {name:?} must be [A-Za-z0-9_.-] only"
            ),
            Self::NotAnObject => f.write_str("SavedData document root is not an object"),
            Self::MissingDataCompound => f.write_str("SavedData document has no `data` compound"),
            Self::MissingDataVersion => {
                f.write_str("SavedData document has no integer `DataVersion`")
            }
            Self::NegativeDataVersion(version) => {
                write!(f, "SavedData document has negative DataVersion {version}")
            }
            Self::DataVersionOutOfRange(version) => write!(
                f,
                "SavedData document has DataVersion {version}, outside the int range"
            ),
            Self::TooManyKeys { keys, max } => {
                write!(f, "SavedData envelope has {keys} keys, over the {max} cap")
            }
            Self::PayloadTooLarge { bytes, max } => write!(
                f,
                "SavedData envelope is {bytes} bytes, over the {max}-byte cap"
            ),
            Self::StoreTooLarge { bytes, max } => write!(
                f,
                "SavedData store holds {bytes} bytes, over the {max}-byte cap"
            ),
            Self::TooManyFiles { files, max } => {
                write!(f, "SavedData store holds {files} files, over the {max} cap")
            }
            Self::TooDeep { max } => {
                write!(f, "SavedData value tree is deeper than {max} levels")
            }
            Self::ReservedKey(key) => {
                write!(f, "SavedData `{key}` is a reserved envelope key")
            }
            Self::ReservedPreservedKey(key) => write!(
                f,
                "SavedData preserved key `{key}` collides with a vanilla envelope key"
            ),
            Self::UnmodeledEnvelopeKey(key) => write!(
                f,
                "SavedData schema key `{key}` is declared but not modelled by this envelope"
            ),
            Self::MalformedDocument { path, message } => {
                write!(f, "{}: {message}", path.display())
            }
            Self::MalformedManifest { path, message } => {
                write!(
                    f,
                    "{}: generation manifest is invalid: {message}",
                    path.display()
                )
            }
            Self::TooManyTombstones { count, max } => write!(
                f,
                "SavedData store records {count} deletion tombstones, over the {max} cap"
            ),
            Self::NbtEncode { message } => {
                write!(f, "SavedData document cannot be encoded as NBT: {message}")
            }
            Self::Io { path, message } => write!(f, "{}: {message}", path.display()),
        }
    }
}

impl Error for SavedDataError {}

/// `true` when `name` is a safe `data/<name>.dat` stem.
fn valid_file_stem(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_SAVED_DATA_NAME_BYTES
        && name != "."
        && name != ".."
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
}

fn validate_file_stem(name: &str) -> Result<(), SavedDataError> {
    if name.is_empty() {
        return Err(SavedDataError::EmptyName);
    }
    if name.len() > MAX_SAVED_DATA_NAME_BYTES {
        return Err(SavedDataError::NameTooLong {
            bytes: name.len(),
            max: MAX_SAVED_DATA_NAME_BYTES,
        });
    }
    if !valid_file_stem(name) {
        return Err(SavedDataError::InvalidName(name.to_owned()));
    }
    Ok(())
}

/// The sealed generation manifest of one `data/` directory: the monotonic
/// generation id plus every deletion tombstone the store has recorded.
///
/// A tombstone is durable intent: the named `data/<name>.dat` was deleted, so a
/// stale copy of that file (a restored backup, a half-applied save, a copy from
/// another world) is ignored on load instead of resurrecting removed state.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct SavedDataManifest {
    /// `0` only for an absent manifest — a vanilla or legacy `data/` directory.
    generation: u64,
    /// Tombstoned file stems, sorted so the encoded bytes are deterministic.
    tombstones: BTreeSet<String>,
}

fn manifest_take<'a>(rest: &mut &'a [u8], len: usize) -> Result<&'a [u8], String> {
    let slice: &'a [u8] = *rest;
    if slice.len() < len {
        return Err("truncated manifest".to_owned());
    }
    let (head, tail) = slice.split_at(len);
    *rest = tail;
    Ok(head)
}

fn manifest_u16(rest: &mut &[u8]) -> Result<u16, String> {
    let bytes = manifest_take(rest, 2)?;
    Ok(u16::from_be_bytes([bytes[0], bytes[1]]))
}

fn manifest_u32(rest: &mut &[u8]) -> Result<u32, String> {
    let bytes = manifest_take(rest, 4)?;
    Ok(u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

fn manifest_u64(rest: &mut &[u8]) -> Result<u64, String> {
    let bytes = manifest_take(rest, 8)?;
    let mut raw = [0u8; 8];
    raw.copy_from_slice(bytes);
    Ok(u64::from_be_bytes(raw))
}

impl SavedDataManifest {
    /// Encode the manifest: magic, schema, generation, tombstone table, then a
    /// CRC32 trailer over everything before it.
    fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(18 + self.tombstones.len() * 40);
        out.extend_from_slice(&SAVED_DATA_MANIFEST_MAGIC);
        out.extend_from_slice(&SAVED_DATA_MANIFEST_SCHEMA.to_be_bytes());
        out.extend_from_slice(&self.generation.to_be_bytes());
        out.extend_from_slice(&(self.tombstones.len() as u32).to_be_bytes());
        for name in &self.tombstones {
            out.extend_from_slice(&(name.len() as u16).to_be_bytes());
            out.extend_from_slice(name.as_bytes());
        }
        let crc = crc32_ieee(&out);
        out.extend_from_slice(&crc.to_be_bytes());
        out
    }

    /// Decode and fully verify a manifest. Fail-closed on every defect: a
    /// trailer/body truncation, a CRC32 mismatch, foreign magic, an unknown
    /// schema, an unsafe or unsorted tombstone name, a tombstone count past the
    /// store cap, generation zero, or any trailing byte.
    fn decode(bytes: &[u8]) -> Result<Self, String> {
        if bytes.len() > MAX_SAVED_DATA_MANIFEST_BYTES {
            return Err(format!(
                "manifest is {} bytes, over the {MAX_SAVED_DATA_MANIFEST_BYTES}-byte cap",
                bytes.len()
            ));
        }
        let (body, trailer) = bytes
            .split_last_chunk()
            .ok_or_else(|| "truncated before the CRC32 trailer".to_owned())?;
        let expected = u32::from_be_bytes(*trailer);
        let actual = crc32_ieee(body);
        if expected != actual {
            return Err(format!(
                "CRC32 mismatch: stored {expected:#010x}, computed {actual:#010x}"
            ));
        }
        let (magic, body) = body
            .split_first_chunk()
            .ok_or_else(|| "truncated before the magic".to_owned())?;
        if magic != &SAVED_DATA_MANIFEST_MAGIC {
            return Err("bad manifest magic".to_owned());
        }
        let mut rest = body;
        let schema = manifest_u16(&mut rest)?;
        if schema != SAVED_DATA_MANIFEST_SCHEMA {
            return Err(format!("unsupported manifest schema version {schema}"));
        }
        let generation = manifest_u64(&mut rest)?;
        if generation == 0 {
            return Err("generation 0 is reserved for an absent manifest".to_owned());
        }
        let count = usize::try_from(manifest_u32(&mut rest)?)
            .map_err(|_| "tombstone count is out of range".to_owned())?;
        if count > MAX_SAVED_DATA_TOMBSTONES {
            return Err(format!(
                "tombstone count {count} exceeds the {MAX_SAVED_DATA_TOMBSTONES} cap"
            ));
        }
        let mut tombstones = BTreeSet::new();
        for _ in 0..count {
            let len = usize::from(manifest_u16(&mut rest)?);
            let raw = manifest_take(&mut rest, len)?;
            let name =
                std::str::from_utf8(raw).map_err(|_| "tombstone name is not UTF-8".to_owned())?;
            validate_file_stem(name).map_err(|error| format!("unsafe tombstone name: {error}"))?;
            if tombstones
                .iter()
                .next_back()
                .is_some_and(|last| last.as_str() >= name)
            {
                return Err("tombstone names are not strictly sorted".to_owned());
            }
            tombstones.insert(name.to_owned());
        }
        if !rest.is_empty() {
            return Err("trailing bytes before the CRC32 trailer".to_owned());
        }
        Ok(Self {
            generation,
            tombstones,
        })
    }
}

/// Move a refused document or manifest aside to `<path>.corrupt[.n]`, keeping
/// its exact bytes for recovery, and return where they went. The first free
/// suffix wins, so a second refused document never overwrites the first
/// preserved copy.
fn quarantine_file(path: &Path) -> Result<PathBuf, SavedDataError> {
    let mut base = path.as_os_str().to_owned();
    base.push(QUARANTINE_SUFFIX);
    let base = PathBuf::from(base);
    let mut target = base.clone();
    let mut index = 0usize;
    while target.exists() {
        if index >= MAX_SAVED_DATA_FILES {
            return Err(SavedDataError::Io {
                path: path.to_path_buf(),
                message: format!(
                    "no free quarantine name beside {} after {MAX_SAVED_DATA_FILES} attempts",
                    base.display()
                ),
            });
        }
        index += 1;
        let mut name = base.as_os_str().to_owned();
        name.push(format!(".{index}"));
        target = PathBuf::from(name);
    }
    std::fs::rename(path, &target).map_err(|error| SavedDataError::Io {
        path: path.to_path_buf(),
        message: format!("cannot quarantine as {}: {error}", target.display()),
    })?;
    Ok(target)
}

/// A staging directory that removes itself, so no code path — including an
/// early `?` return — can leave staged documents behind.
struct StagingDir(PathBuf);

impl Drop for StagingDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Envelope scope for one file stem.
///
/// `scoreboard`/`raids`/`map_<id>` map to their declarative auxiliary scope.
/// Every other file (a Mod's `data/<mod>_<name>.dat`, or a future vanilla
/// file) uses [`SchemaScope::AuxMap`] as the generic `SavedData` envelope
/// scope — the three auxiliary scopes declare the identical `data` +
/// `DataVersion` rows, so classification and preservation are unaffected.
pub fn saved_data_scope_for_name(name: &str) -> SchemaScope {
    match name {
        "scoreboard" => SchemaScope::AuxScoreboard,
        "raids" => SchemaScope::AuxRaids,
        _ => SchemaScope::AuxMap,
    }
}

/// Deterministic size estimate of one value tree, used for the fail-closed
/// envelope/store caps. Charges key bytes, a tag byte per value, and a
/// fixed width per scalar — deterministic across platforms and never
/// under-counts a payload's keys.
fn value_bytes(key: &str, value: &DataValue, depth: usize) -> Result<usize, SavedDataError> {
    if depth > MAX_SAVED_DATA_DEPTH {
        return Err(SavedDataError::TooDeep {
            max: MAX_SAVED_DATA_DEPTH,
        });
    }
    let own = key.len() + 1;
    let children = match value {
        DataValue::Null | DataValue::Bool(_) => 1,
        DataValue::Int(_) | DataValue::Float(_) => 8,
        DataValue::Str(text) => 4 + text.len(),
        DataValue::List(items) => {
            let mut total = 4;
            for item in items {
                total += value_bytes("", item, depth + 1)?;
            }
            total
        }
        DataValue::Map(map) => {
            let mut total = 4;
            for (child_key, child) in map {
                total += value_bytes(child_key, child, depth + 1)?;
            }
            total
        }
    };
    Ok(own + children)
}

fn map_bytes(map: &BTreeMap<String, DataValue>) -> Result<usize, SavedDataError> {
    let mut total = 0usize;
    for (key, value) in map {
        total += value_bytes(key, value, 0)?;
    }
    Ok(total)
}

/// One `data/<name>.dat` `SavedData` envelope: the subclass `data` compound,
/// its `DataVersion`, and every root key the vanilla schema does not declare
/// preserved verbatim.
#[derive(Clone, Debug, PartialEq)]
pub struct SavedDataEnvelope {
    name: String,
    scope: SchemaScope,
    data_version: i32,
    data: BTreeMap<String, DataValue>,
    /// Mod/newer-vanilla root keys, in document order, written back verbatim.
    preserved: Vec<(String, DataValue)>,
}

impl SavedDataEnvelope {
    /// Build an envelope from a payload and `DataVersion`, with no preserved
    /// root keys. Caps are enforced immediately.
    pub fn new(
        name: impl Into<String>,
        data_version: i32,
        data: BTreeMap<String, DataValue>,
    ) -> Result<Self, SavedDataError> {
        let name = name.into();
        validate_file_stem(&name)?;
        if data_version < 0 {
            return Err(SavedDataError::NegativeDataVersion(data_version));
        }
        let envelope = Self {
            scope: saved_data_scope_for_name(&name),
            name,
            data_version,
            data,
            preserved: Vec::new(),
        };
        envelope.validate()?;
        Ok(envelope)
    }

    /// Parse a decoded `data/<name>.dat` root compound.
    ///
    /// Fail-closed on a missing/!compound `data`, a missing/!integer
    /// `DataVersion`, or a negative `DataVersion`; every other root key is
    /// routed through [`unknown_keys_to_preserve`] and kept verbatim, so a Mod
    /// envelope's own metadata survives a load/save cycle.
    pub fn from_root(
        name: impl Into<String>,
        root: BTreeMap<String, DataValue>,
    ) -> Result<Self, SavedDataError> {
        let name = name.into();
        validate_file_stem(&name)?;
        let scope = saved_data_scope_for_name(&name);
        // The declarative table is the single source of truth for "a Mod (or
        // newer vanilla) root key": everything it does not declare for this
        // auxiliary scope is preserved verbatim, in document order.
        let mut preserved_keys: Vec<String> =
            unknown_keys_to_preserve(scope, root.keys().map(String::as_str))
                .into_iter()
                .map(str::to_owned)
                .collect();
        preserved_keys.sort_unstable();
        let mut data = None;
        let mut data_version = None;
        let mut preserved = Vec::with_capacity(preserved_keys.len());
        for (key, value) in root {
            match key.as_str() {
                SAVED_DATA_PAYLOAD_KEY => match value {
                    DataValue::Map(map) => data = Some(map),
                    _ => return Err(SavedDataError::MissingDataCompound),
                },
                SAVED_DATA_VERSION_KEY => match value {
                    DataValue::Int(version) => {
                        let version = i32::try_from(version)
                            .map_err(|_| SavedDataError::DataVersionOutOfRange(version))?;
                        if version < 0 {
                            return Err(SavedDataError::NegativeDataVersion(version));
                        }
                        data_version = Some(version);
                    }
                    _ => return Err(SavedDataError::MissingDataVersion),
                },
                _ if preserved_keys.binary_search(&key).is_ok() => preserved.push((key, value)),
                _ => return Err(SavedDataError::UnmodeledEnvelopeKey(key)),
            }
        }
        let data = data.ok_or(SavedDataError::MissingDataCompound)?;
        let data_version = data_version.ok_or(SavedDataError::MissingDataVersion)?;
        let envelope = Self {
            name,
            scope,
            data_version,
            data,
            preserved,
        };
        envelope.validate()?;
        Ok(envelope)
    }

    /// Parse one `data/<name>.dat` JSON document.
    ///
    /// **Debug/inspection only** — never the on-disk `.dat` form, which is
    /// gzip-compressed binary NBT ([`SavedDataEnvelope::from_nbt_bytes`] /
    /// [`SavedDataEnvelope::to_nbt_bytes`]). Rejects the same envelope shapes
    /// as [`SavedDataEnvelope::from_root`].
    pub fn from_json_text(name: impl Into<String>, text: &str) -> Result<Self, SavedDataError> {
        let name = name.into();
        let value = parse_data_json(text).map_err(|error| SavedDataError::MalformedDocument {
            path: PathBuf::from(format!("data/{name}.dat")),
            message: error.to_string(),
        })?;
        let DataValue::Map(root) = value else {
            return Err(SavedDataError::NotAnObject);
        };
        Self::from_root(name, root)
    }

    /// Parse one real `data/<name>.dat` document from its on-disk bytes.
    ///
    /// Accepts both carriage forms Vanilla and Mods produce: an RFC 1952 gzip
    /// member ([`GZIP_MAGIC`]) wrapping binary NBT, and raw uncompressed binary
    /// NBT. Inflation is capped at [`MAX_SAVED_DATA_ENVELOPE_BYTES`] and the
    /// NBT decode goes through the crate's bounded
    /// [`crate::anvil_inspect::decode_nbt_to_data_value`] bridge, so a hostile
    /// document cannot allocate past the envelope cap. The root compound is
    /// then classified by [`SavedDataEnvelope::from_root`]: `data` and
    /// `DataVersion` supply the envelope, every unknown top-level key is
    /// preserved verbatim.
    ///
    /// Vanilla's `SavedData` files carry an unnamed root tag, but the name is
    /// not semantic here — a named root compound is accepted and ignored.
    /// Anything that is not a compound root, or that fails NBT validation,
    /// fails closed.
    pub fn from_nbt_bytes(name: impl Into<String>, bytes: &[u8]) -> Result<Self, SavedDataError> {
        let name = name.into();
        let path = PathBuf::from(format!("data/{name}.dat"));
        let inflated;
        let document: &[u8] = if bytes.starts_with(&GZIP_MAGIC) {
            inflated = gzip_decompress(bytes, MAX_SAVED_DATA_ENVELOPE_BYTES).map_err(|error| {
                SavedDataError::MalformedDocument {
                    path: path.clone(),
                    message: error.to_string(),
                }
            })?;
            &inflated
        } else {
            bytes
        };
        if document.len() > MAX_SAVED_DATA_ENVELOPE_BYTES {
            return Err(SavedDataError::PayloadTooLarge {
                bytes: document.len(),
                max: MAX_SAVED_DATA_ENVELOPE_BYTES,
            });
        }
        let (_, root) = decode_nbt_to_data_value(document).map_err(|error| {
            SavedDataError::MalformedDocument {
                path: path.clone(),
                message: error.to_string(),
            }
        })?;
        let DataValue::Map(root) = root else {
            return Err(SavedDataError::NotAnObject);
        };
        Self::from_root(name, root)
    }

    /// File stem (`scoreboard`, `map_12`, `mymod_custom`).
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Envelope scope used for unknown-key classification.
    pub fn scope(&self) -> SchemaScope {
        self.scope
    }

    /// `DataVersion` the document declares.
    pub fn data_version(&self) -> i32 {
        self.data_version
    }

    /// Replace `DataVersion` (a datafixer stamps the target).
    pub fn set_data_version(&mut self, data_version: i32) -> Result<(), SavedDataError> {
        if data_version < 0 {
            return Err(SavedDataError::NegativeDataVersion(data_version));
        }
        self.data_version = data_version;
        Ok(())
    }

    /// The subclass `data` compound.
    pub fn data(&self) -> &BTreeMap<String, DataValue> {
        &self.data
    }

    /// Read one `data` key.
    pub fn field(&self, key: &str) -> Option<&DataValue> {
        self.data.get(key)
    }

    /// Insert/replace one `data` key, re-checking the envelope caps.
    pub fn put_field(
        &mut self,
        key: impl Into<String>,
        value: DataValue,
    ) -> Result<(), SavedDataError> {
        let key = key.into();
        if matches!(
            key.as_str(),
            SAVED_DATA_PAYLOAD_KEY | SAVED_DATA_VERSION_KEY
        ) {
            return Err(SavedDataError::ReservedKey(key));
        }
        let previous = self.data.insert(key.clone(), value);
        if let Err(error) = self.validate() {
            match previous {
                Some(previous) => {
                    self.data.insert(key, previous);
                }
                None => {
                    self.data.remove(&key);
                }
            }
            return Err(error);
        }
        Ok(())
    }

    /// Remove one `data` key, returning the previous value.
    pub fn remove_field(&mut self, key: &str) -> Option<DataValue> {
        self.data.remove(key)
    }

    /// Root keys the vanilla schema does not declare, in document order. These
    /// are exactly what [`SavedDataEnvelope::to_root`] writes back verbatim.
    pub fn preserved(&self) -> &[(String, DataValue)] {
        &self.preserved
    }

    /// Deterministic serialized-size estimate charged against the caps.
    pub fn envelope_bytes(&self) -> usize {
        self.measure().unwrap_or(MAX_SAVED_DATA_ENVELOPE_BYTES)
    }

    /// Fallible form of [`SavedDataEnvelope::envelope_bytes`]: an over-deep or
    /// over-cap tree is an error, never a truncated number.
    fn measure(&self) -> Result<usize, SavedDataError> {
        let mut total = map_bytes(&self.data)?;
        for (key, value) in &self.preserved {
            total += value_bytes(key, value, 0)?;
        }
        Ok(total + self.name.len() + 8)
    }

    /// Total key count (`data` keys plus preserved root keys).
    pub fn key_count(&self) -> usize {
        self.data.len() + self.preserved.len()
    }

    /// Re-check every envelope invariant — name, version, key/depth/byte caps,
    /// and that preserved keys never shadow a vanilla envelope key.
    ///
    /// Callers that mutate [`SavedDataEnvelope::data`] through a path other
    /// than [`SavedDataEnvelope::put_field`] run this before persisting.
    pub fn validate(&self) -> Result<(), SavedDataError> {
        validate_file_stem(&self.name)?;
        if self.data_version < 0 {
            return Err(SavedDataError::NegativeDataVersion(self.data_version));
        }
        let keys = self.key_count();
        if keys > MAX_SAVED_DATA_KEYS {
            return Err(SavedDataError::TooManyKeys {
                keys,
                max: MAX_SAVED_DATA_KEYS,
            });
        }
        for (key, _) in &self.preserved {
            if matches!(
                key.as_str(),
                SAVED_DATA_PAYLOAD_KEY | SAVED_DATA_VERSION_KEY
            ) {
                return Err(SavedDataError::ReservedPreservedKey(key.clone()));
            }
        }
        for name in &self.deleted {
            validate_file_stem(name)?;
        }
        if self.deleted.len() > MAX_SAVED_DATA_TOMBSTONES {
            return Err(SavedDataError::TooManyTombstones {
                count: self.deleted.len(),
                max: MAX_SAVED_DATA_TOMBSTONES,
            });
        }
        // Depth is checked while measuring, so a pathological tree fails as
        // `TooDeep` instead of a byte-cap trip that hid the real problem.
        let bytes = self.measure()?;
        if bytes > MAX_SAVED_DATA_ENVELOPE_BYTES {
            return Err(SavedDataError::PayloadTooLarge {
                bytes,
                max: MAX_SAVED_DATA_ENVELOPE_BYTES,
            });
        }
        Ok(())
    }

    /// Rebuild the `data/<name>.dat` root compound: `data`, `DataVersion`,
    /// then every preserved root key verbatim.
    pub fn to_root(&self) -> BTreeMap<String, DataValue> {
        let mut root = BTreeMap::new();
        root.insert(
            SAVED_DATA_PAYLOAD_KEY.to_owned(),
            DataValue::Map(self.data.clone()),
        );
        root.insert(
            SAVED_DATA_VERSION_KEY.to_owned(),
            DataValue::Int(i64::from(self.data_version)),
        );
        for (key, value) in &self.preserved {
            root.insert(key.clone(), value.clone());
        }
        root
    }

    /// Serialize the envelope as a real `data/<name>.dat` document: a gzip
    /// member (RFC 1952, stored deflate blocks) wrapping the binary NBT root
    /// compound `data`, `DataVersion` and every preserved root key.
    ///
    /// Vanilla's `GZIPInputStream`/`NbtIo` pair reads this form directly, so a
    /// world saved here stays usable by a vanilla or Modded server. Fails
    /// closed ([`SavedDataError::NbtEncode`]) on a tree binary NBT cannot
    /// express — `Null` anywhere, a heterogeneous list, or a document past the
    /// NBT size cap.
    pub fn to_nbt_bytes(&self) -> Result<Vec<u8>, SavedDataError> {
        self.validate()?;
        let root = DataValue::Map(self.to_root());
        let nbt =
            encode_data_value_to_nbt("", &root).map_err(|error| SavedDataError::NbtEncode {
                message: error.to_string(),
            })?;
        Ok(gzip_encode_stored(&nbt))
    }

    /// Serialize the envelope as debug/inspection JSON.
    ///
    /// **Never write this to `data/<name>.dat`** — the on-disk form is binary
    /// NBT ([`SavedDataEnvelope::to_nbt_bytes`]).
    pub fn to_json_text(&self) -> String {
        to_data_json(&DataValue::Map(self.to_root()))
    }
}

/// One instance-owned set of `data/*.dat` envelopes, capped globally, plus the
/// durable generation and deletion tombstones of the directory it publishes to.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SavedDataStore {
    files: BTreeMap<String, SavedDataEnvelope>,
    /// File stems deleted from this store. Monotonic: a tombstone only leaves
    /// the set through [`SavedDataStore::clear_tombstone`], or when the same
    /// name is explicitly re-inserted — that is what makes a deletion durable
    /// across a stale copy of the removed document.
    deleted: BTreeSet<String>,
    /// Last generation this store published or loaded, `0` before either.
    generation: u64,
}

impl SavedDataStore {
    /// Empty store.
    pub fn new() -> Self {
        Self::default()
    }

    /// Generation this store last published or loaded; `0` for a store that
    /// has never touched a directory and for a legacy directory with no
    /// manifest.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// File stems deleted from this store, in deterministic order.
    pub fn tombstones(&self) -> impl Iterator<Item = &str> {
        self.deleted.iter().map(String::as_str)
    }

    /// `true` when `name` is tombstoned, so its document is ignored on load.
    pub fn is_tombstoned(&self, name: &str) -> bool {
        self.deleted.contains(name)
    }

    /// Drop the tombstone for `name` so a later `data/<name>.dat` is loaded
    /// again. Returns whether a tombstone was removed; deleting nothing is not
    /// an error, but resurrecting nothing is a deliberate, explicit act.
    pub fn clear_tombstone(&mut self, name: &str) -> bool {
        self.deleted.remove(name)
    }

    /// Insert or replace one envelope after re-checking every store cap.
    ///
    /// Re-inserting a tombstoned name is an explicit resurrection: the
    /// tombstone is dropped, so the document is written and loaded again.
    pub fn insert(&mut self, envelope: SavedDataEnvelope) -> Result<(), SavedDataError> {
        envelope.validate()?;
        let name = envelope.name().to_owned();
        let bytes = envelope.envelope_bytes();
        let replacing = self.files.contains_key(&name);
        if !replacing && self.files.len() >= MAX_SAVED_DATA_FILES {
            return Err(SavedDataError::TooManyFiles {
                files: self.files.len() + 1,
                max: MAX_SAVED_DATA_FILES,
            });
        }
        let previous_bytes = self
            .files
            .get(&name)
            .map_or(0, SavedDataEnvelope::envelope_bytes);
        let total = self.total_bytes() - previous_bytes + bytes;
        if total > MAX_SAVED_DATA_STORE_BYTES {
            return Err(SavedDataError::StoreTooLarge {
                bytes: total,
                max: MAX_SAVED_DATA_STORE_BYTES,
            });
        }
        self.files.insert(name.clone(), envelope);
        // Re-inserting a deleted name is the explicit resurrection path.
        self.deleted.remove(&name);
        Ok(())
    }

    /// Borrow one envelope by file stem.
    pub fn get(&self, name: &str) -> Option<&SavedDataEnvelope> {
        self.files.get(name)
    }

    /// Mutably borrow one envelope. Run [`SavedDataStore::validate`] after
    /// mutating it outside [`SavedDataEnvelope::put_field`].
    pub fn get_mut(&mut self, name: &str) -> Option<&mut SavedDataEnvelope> {
        self.files.get_mut(name)
    }

    /// Remove one envelope and record its deletion tombstone.
    ///
    /// The tombstone is what makes the removal durable: the next
    /// [`SavedDataStore::save_to_dir`] unlinks `data/<name>.dat` *and* records
    /// the name in the generation manifest, so a stale copy of the document
    /// cannot bring removed state back on the following load.
    pub fn remove(&mut self, name: &str) -> Option<SavedDataEnvelope> {
        let removed = self.files.remove(name);
        // An unusable stem never reaches the filesystem, so it cannot own a
        // document to tombstone. A tombstone past the cap is refused by
        // `validate`/`save_to_dir` instead of being dropped silently here.
        if validate_file_stem(name).is_ok() {
            self.deleted.insert(name.to_owned());
        }
        removed
    }

    /// Number of envelopes held.
    pub fn len(&self) -> usize {
        self.files.len()
    }

    /// `true` when no envelope is held.
    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    /// File stems in deterministic order.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.files.keys().map(String::as_str)
    }

    /// Aggregate [`SavedDataEnvelope::envelope_bytes`] across the store.
    /// Recomputed on demand (at most [`MAX_SAVED_DATA_FILES`] envelopes), so a
    /// `get_mut` edit can never leave stale accounting behind.
    pub fn total_bytes(&self) -> usize {
        self.files
            .values()
            .map(SavedDataEnvelope::envelope_bytes)
            .sum()
    }

    /// Re-check every envelope and the aggregate byte total; fail-closed.
    pub fn validate(&self) -> Result<(), SavedDataError> {
        let mut total = 0usize;
        for (name, envelope) in &self.files {
            if envelope.name() != name {
                return Err(SavedDataError::InvalidName(name.clone()));
            }
            envelope.validate()?;
            total += envelope.envelope_bytes();
        }
        if total > MAX_SAVED_DATA_STORE_BYTES {
            return Err(SavedDataError::StoreTooLarge {
                bytes: total,
                max: MAX_SAVED_DATA_STORE_BYTES,
            });
        }
        Ok(())
    }

    /// Load every `data/*.dat` document from `dir`.
    ///
    /// Files are read as the real Vanilla form — RFC 1952 gzip-compressed
    /// binary NBT, or raw binary NBT — through
    /// [`SavedDataEnvelope::from_nbt_bytes`]; JSON documents are not accepted
    /// (export them with [`SavedDataEnvelope::to_json_text`] instead). Only
    /// regular files with a safe stem and a `.dat` suffix are read; every other
    /// entry, [`SAVED_DATA_MANIFEST_FILE`] included, is ignored.
    ///
    /// The generation manifest, when one exists, supplies the store's
    /// generation and its deletion tombstones: a tombstoned stem is never
    /// loaded, even when its document is back on disk. A document that fails to
    /// parse or exceeds a cap aborts the whole load (fail closed) instead of
    /// being silently skipped; [`SavedDataStore::load_dir_quarantining`] is the
    /// recovery path that preserves such a document. A missing directory is an
    /// empty store, and a directory without a manifest — a vanilla or
    /// pre-manifest world — loads exactly as before.
    pub fn load_dir(dir: &Path) -> Result<Self, SavedDataError> {
        Self::load_dir_inner(dir, false).map(|(store, _)| store)
    }

    /// Recovery variant of [`SavedDataStore::load_dir`] for an operator holding
    /// a damaged `data/`: every document the strict loader refuses, and the
    /// manifest too if it is corrupt, is moved aside to `<file>.corrupt` with
    /// its bytes intact, and the rest of the directory loads normally.
    ///
    /// Returns the store and the original paths that were quarantined, in
    /// directory order. This is the only load path that writes to `dir`, and it
    /// only renames refused files inside it — it never deletes or overwrites a
    /// document — so a later [`SavedDataStore::save_to_dir`] cannot bury
    /// recoverable bytes.
    pub fn load_dir_quarantining(dir: &Path) -> Result<(Self, Vec<PathBuf>), SavedDataError> {
        Self::load_dir_inner(dir, true)
    }

    fn load_dir_inner(
        dir: &Path,
        quarantining: bool,
    ) -> Result<(Self, Vec<PathBuf>), SavedDataError> {
        let mut store = Self::new();
        let mut quarantined = Vec::new();
        let manifest_path = dir.join(SAVED_DATA_MANIFEST_FILE);
        match std::fs::read(&manifest_path) {
            Ok(bytes) => match SavedDataManifest::decode(&bytes) {
                Ok(manifest) => {
                    store.generation = manifest.generation;
                    store.deleted = manifest.tombstones;
                }
                Err(_) if quarantining => {
                    quarantine_file(&manifest_path)?;
                    quarantined.push(manifest_path.clone());
                }
                Err(message) => {
                    return Err(SavedDataError::MalformedManifest {
                        path: manifest_path,
                        message,
                    })
                }
            },
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(SavedDataError::Io {
                    path: manifest_path,
                    message: error.to_string(),
                })
            }
        }
        let entries = match std::fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok((store, quarantined))
            }
            Err(error) => {
                return Err(SavedDataError::Io {
                    path: dir.to_path_buf(),
                    message: error.to_string(),
                });
            }
        };
        for entry in entries {
            let entry = entry.map_err(|error| SavedDataError::Io {
                path: dir.to_path_buf(),
                message: error.to_string(),
            })?;
            let path = entry.path();
            let file_type = entry.file_type().map_err(|error| SavedDataError::Io {
                path: path.clone(),
                message: error.to_string(),
            })?;
            if !file_type.is_file() {
                continue;
            }
            let Some(file_name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            let Some(stem) = file_name.strip_suffix(".dat") else {
                continue;
            };
            validate_file_stem(stem)?;
            // A tombstone outranks whatever is on disk: a stale copy of a
            // deleted document must not resurrect removed state.
            if store.deleted.contains(stem) {
                continue;
            }
            let bytes = match read_bounded(&path, MAX_SAVED_DATA_ENVELOPE_BYTES) {
                Ok(bytes) => bytes,
                Err(_) if quarantining => {
                    quarantine_file(&path)?;
                    quarantined.push(path);
                    continue;
                }
                Err(error) => return Err(error),
            };
            let parsed = SavedDataEnvelope::from_nbt_bytes(stem, &bytes).map_err(|error| {
                match error {
                    // Point the message at the real file, not the synthetic
                    // `data/<name>.dat` path the envelope builder reports.
                    SavedDataError::MalformedDocument { message, .. } => {
                        SavedDataError::MalformedDocument {
                            path: path.clone(),
                            message,
                        }
                    }
                    other => other,
                }
            });
            let envelope = match parsed {
                Ok(envelope) => envelope,
                Err(_) if quarantining => {
                    quarantine_file(&path)?;
                    quarantined.push(path);
                    continue;
                }
                Err(error) => return Err(error),
            };
            store.insert(envelope)?;
        }
        store.validate()?;
        Ok((store, quarantined))
    }

    /// Write every envelope as `<dir>/<name>.dat` in the real Vanilla form
    /// (gzip-compressed binary NBT) and publish the generation manifest.
    ///
    /// Ordering, from the outside in:
    ///
    /// 1. every document is encoded and written into a throwaway staging
    ///    directory inside `dir` (write-then-rename, fsynced), so a failed
    ///    encode or an exhausted disk never touches the published generation;
    /// 2. each staged document is installed with one rename over its canonical
    ///    path, keeping `data/<name>.dat` the compatibility surface vanilla
    ///    reads;
    /// 3. the sealed manifest carrying generation `n + 1` and the union of this
    ///    store's tombstones with the ones already on disk — minus every name
    ///    this store now holds live, which is an explicit resurrection — is
    ///    published atomically; the generation advances only when every write
    ///    above succeeded;
    /// 4. tombstoned `data/<name>.dat` files are unlinked. Publishing the
    ///    tombstone before the unlink means a crash between the two leaves the
    ///    document on disk *and* ignored, which the next save finishes off.
    ///
    /// Existing unrelated files are untouched: a `.dat` this store never held
    /// and never tombstoned stays where it is.
    pub fn save_to_dir(&mut self, dir: &Path) -> Result<(), SavedDataError> {
        self.save_to_dir_with(dir, write_file_atomic)
    }

    /// [`SavedDataStore::save_to_dir`] with an injectable writer.
    ///
    /// `write` receives each staged path and that document's complete bytes, and
    /// must leave the file durable before returning (the default is
    /// [`write_file_atomic`]). A caller staging somewhere else, or refusing a
    /// write while a quota is exhausted, gets exactly the same publish ordering
    /// and therefore the same "the previous generation is untouched" guarantee.
    pub fn save_to_dir_with<F>(&mut self, dir: &Path, mut write: F) -> Result<(), SavedDataError>
    where
        F: FnMut(&Path, &[u8]) -> io::Result<()>,
    {
        self.validate()?;
        std::fs::create_dir_all(dir).map_err(|error| SavedDataError::Io {
            path: dir.to_path_buf(),
            message: error.to_string(),
        })?;
        let manifest_path = dir.join(SAVED_DATA_MANIFEST_FILE);
        let prior = Self::read_manifest_before_save(&manifest_path)?;
        // Deletion is monotonic: a tombstone another writer already published is
        // kept, never dropped by this save — except for a name this store holds
        // live, which is the explicit resurrection that outranks the previous
        // generation's tombstone.
        let mut tombstones = prior.tombstones;
        tombstones.extend(self.deleted.iter().cloned());
        for name in self.files.keys() {
            tombstones.remove(name);
        }
        if tombstones.len() > MAX_SAVED_DATA_TOMBSTONES {
            return Err(SavedDataError::TooManyTombstones {
                count: tombstones.len(),
                max: MAX_SAVED_DATA_TOMBSTONES,
            });
        }
        let generation = prior.generation.max(self.generation) + 1;
        let staging = StagingDir(dir.join(format!(
            "{STAGING_PREFIX}{}-{}",
            std::process::id(),
            STAGING_SEQ.fetch_add(1, Ordering::Relaxed),
        )));
        std::fs::create_dir_all(&staging.0).map_err(|error| SavedDataError::Io {
            path: staging.0.clone(),
            message: error.to_string(),
        })?;
        for (name, envelope) in &self.files {
            let document = envelope.to_nbt_bytes()?;
            let staged = staging.0.join(format!("{name}.dat"));
            if let Err(error) = write(&staged, &document) {
                return Err(SavedDataError::Io {
                    path: staged,
                    message: error.to_string(),
                });
            }
        }
        for name in self.files.keys() {
            let staged = staging.0.join(format!("{name}.dat"));
            let canonical = dir.join(format!("{name}.dat"));
            if let Err(error) = std::fs::rename(&staged, &canonical) {
                return Err(SavedDataError::Io {
                    path: canonical,
                    message: format!("cannot install the staged document: {error}"),
                });
            }
        }
        let manifest = SavedDataManifest {
            generation,
            tombstones: tombstones.clone(),
        };
        let encoded = manifest.encode();
        if let Err(error) = write(&manifest_path, &encoded) {
            return Err(SavedDataError::Io {
                path: manifest_path,
                message: error.to_string(),
            });
        }
        for name in &tombstones {
            let path = dir.join(format!("{name}.dat"));
            match std::fs::remove_file(&path) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(SavedDataError::Io {
                        path,
                        message: error.to_string(),
                    })
                }
            }
        }
        self.generation = generation;
        self.deleted = tombstones;
        Ok(())
    }

    /// Read the manifest a save is about to replace: its generation and
    /// tombstones. A corrupt manifest is preserved as `<manifest>.corrupt` and
    /// treated as absent, so one damaged index can never wedge every later save.
    fn read_manifest_before_save(path: &Path) -> Result<SavedDataManifest, SavedDataError> {
        match std::fs::read(path) {
            Ok(bytes) => match SavedDataManifest::decode(&bytes) {
                Ok(manifest) => Ok(manifest),
                Err(_) => {
                    quarantine_file(path)?;
                    Ok(SavedDataManifest::default())
                }
            },
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                Ok(SavedDataManifest::default())
            }
            Err(error) => Err(SavedDataError::Io {
                path: path.to_path_buf(),
                message: error.to_string(),
            }),
        }
    }
}

/// Read at most `cap` bytes of one file, refusing anything larger instead of
/// truncating it.
fn read_bounded(path: &Path, cap: usize) -> Result<Vec<u8>, SavedDataError> {
    use std::io::Read;

    let metadata = std::fs::metadata(path).map_err(|error| SavedDataError::Io {
        path: path.to_path_buf(),
        message: error.to_string(),
    })?;
    if !metadata.is_file() {
        return Err(SavedDataError::Io {
            path: path.to_path_buf(),
            message: "not a regular file".to_owned(),
        });
    }
    if metadata.len() > cap as u64 {
        return Err(SavedDataError::PayloadTooLarge {
            bytes: metadata.len() as usize,
            max: cap,
        });
    }
    let file = std::fs::File::open(path).map_err(|error| SavedDataError::Io {
        path: path.to_path_buf(),
        message: error.to_string(),
    })?;
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take(cap as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| SavedDataError::Io {
            path: path.to_path_buf(),
            message: error.to_string(),
        })?;
    if bytes.len() > cap {
        return Err(SavedDataError::PayloadTooLarge {
            bytes: bytes.len(),
            max: cap,
        });
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    static NEXT_TEMP_DIR: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

    fn test_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "megu-saved-data-{label}-{}-{}",
            std::process::id(),
            NEXT_TEMP_DIR.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
        ));
        std::fs::create_dir_all(&dir).expect("test dir");
        dir
    }

    fn map(
        entries: impl IntoIterator<Item = (&'static str, DataValue)>,
    ) -> BTreeMap<String, DataValue> {
        entries
            .into_iter()
            .map(|(key, value)| (key.to_owned(), value))
            .collect()
    }

    /// Encode a root compound as raw (uncompressed) binary NBT, the carriage
    /// form a Mod or an older vanilla writer may leave on disk.
    fn nbt_document(root: &BTreeMap<String, DataValue>) -> Vec<u8> {
        encode_data_value_to_nbt("", &DataValue::Map(root.clone())).expect("encode NBT")
    }

    /// A Mod scoreboard-ish document: vanilla payload keys plus a Mod's own
    /// root metadata and a namespaced payload key.
    fn modded_root() -> BTreeMap<String, DataValue> {
        let mut root = BTreeMap::new();
        root.insert(
            SAVED_DATA_PAYLOAD_KEY.to_owned(),
            DataValue::Map(map([
                ("Objectives", DataValue::List(Vec::new())),
                (
                    "mymod:alpaca_scores",
                    DataValue::Map(map([("alpaca", DataValue::Int(7))])),
                ),
            ])),
        );
        root.insert(SAVED_DATA_VERSION_KEY.to_owned(), DataValue::Int(3955));
        // Mod envelope metadata the vanilla schema does not declare.
        root.insert(
            "mymod_meta".to_owned(),
            DataValue::Map(map([("owner", DataValue::Str("mymod".into()))])),
        );
        root
    }

    #[test]
    fn round_trip_preserves_unknown_mod_root_keys() {
        let envelope = SavedDataEnvelope::from_root("mymod_scores", modded_root()).unwrap();
        assert_eq!(envelope.name(), "mymod_scores");
        assert_eq!(envelope.scope(), SchemaScope::AuxMap);
        assert_eq!(envelope.data_version(), 3955);
        // Exactly the keys the schema does not declare, in document order.
        let preserved: Vec<&str> = envelope
            .preserved()
            .iter()
            .map(|(k, _)| k.as_str())
            .collect();
        assert_eq!(preserved, vec!["mymod_meta"]);
        assert_eq!(
            preserved,
            unknown_keys_to_preserve(
                SchemaScope::AuxMap,
                ["data", "DataVersion", "mymod_meta"].into_iter()
            )
        );
        // Namespaced payload keys inside `data` ride along untouched.
        assert!(envelope.data().contains_key("mymod:alpaca_scores"));

        // The rebuilt root is byte-for-byte the input document.
        assert_eq!(envelope.to_root(), modded_root());

        // JSON round-trip through the canonical codec (debug/inspection only).
        let json = envelope.to_json_text();
        let reparsed = SavedDataEnvelope::from_json_text("mymod_scores", &json).unwrap();
        assert_eq!(reparsed, envelope);
        assert_eq!(reparsed.to_root(), modded_root());

        // Binary NBT round-trip — the real on-disk form, gzip and raw.
        let nbt = envelope.to_nbt_bytes().unwrap();
        assert_eq!(&nbt[..2], &GZIP_MAGIC);
        let reparsed = SavedDataEnvelope::from_nbt_bytes("mymod_scores", &nbt).unwrap();
        assert_eq!(reparsed, envelope);
        assert_eq!(reparsed.to_root(), modded_root());
        let raw = nbt_document(&modded_root());
        assert_ne!(&raw[..2], &GZIP_MAGIC);
        let reparsed = SavedDataEnvelope::from_nbt_bytes("mymod_scores", &raw).unwrap();
        assert_eq!(reparsed, envelope);
    }

    #[test]
    fn vanilla_files_use_their_declarative_scope() {
        let scoreboard = SavedDataEnvelope::new(
            "scoreboard",
            3955,
            map([("Objectives", DataValue::List(Vec::new()))]),
        )
        .unwrap();
        assert_eq!(scoreboard.scope(), SchemaScope::AuxScoreboard);
        assert_eq!(saved_data_scope_for_name("raids"), SchemaScope::AuxRaids);
        assert_eq!(saved_data_scope_for_name("map_12"), SchemaScope::AuxMap);
        assert_eq!(saved_data_scope_for_name("mymod_x"), SchemaScope::AuxMap);
        assert!(scoreboard.preserved().is_empty());
        let root = scoreboard.to_root();
        assert_eq!(root.get("DataVersion"), Some(&DataValue::Int(3955)));
        assert!(matches!(root.get("data"), Some(DataValue::Map(_))));
    }

    #[test]
    fn malformed_documents_fail_closed() {
        // Missing `data`.
        let mut root = modded_root();
        root.remove("data");
        assert_eq!(
            SavedDataEnvelope::from_root("mymod_scores", root).unwrap_err(),
            SavedDataError::MissingDataCompound
        );
        // `DataVersion` of the wrong type.
        let mut root = modded_root();
        root.insert("DataVersion".into(), DataValue::Str("3955".into()));
        assert_eq!(
            SavedDataEnvelope::from_root("mymod_scores", root).unwrap_err(),
            SavedDataError::MissingDataVersion
        );
        // Negative `DataVersion`.
        let mut root = modded_root();
        root.insert("DataVersion".into(), DataValue::Int(-1));
        assert_eq!(
            SavedDataEnvelope::from_root("mymod_scores", root).unwrap_err(),
            SavedDataError::NegativeDataVersion(-1)
        );
        // Non-object JSON.
        assert_eq!(
            SavedDataEnvelope::from_json_text("mymod_scores", "[1,2,3]").unwrap_err(),
            SavedDataError::NotAnObject
        );
        // Unsafe file names never reach the filesystem.
        for name in ["", "..", "../evil", "a/b", "a\\b", "score board"] {
            assert!(
                SavedDataEnvelope::new(name, 3955, BTreeMap::new()).is_err(),
                "{name:?}"
            );
        }
        // Reserved keys cannot be injected through `put_field`.
        let mut envelope = SavedDataEnvelope::new("scoreboard", 3955, BTreeMap::new()).unwrap();
        assert_eq!(
            envelope
                .put_field("DataVersion", DataValue::Int(1))
                .unwrap_err(),
            SavedDataError::ReservedKey("DataVersion".into())
        );
    }

    #[test]
    fn caps_refuse_instead_of_truncating() {
        // Depth cap.
        let mut deep = DataValue::Int(1);
        for _ in 0..(MAX_SAVED_DATA_DEPTH + 2) {
            deep = DataValue::Map(map([("child", deep)]));
        }
        let deep_envelope = SavedDataEnvelope {
            name: "deep".into(),
            scope: SchemaScope::AuxMap,
            data_version: 3955,
            data: map([("root", deep)]),
            preserved: Vec::new(),
        };
        assert_eq!(
            deep_envelope.validate().unwrap_err(),
            SavedDataError::TooDeep {
                max: MAX_SAVED_DATA_DEPTH
            }
        );

        // Key cap.
        let mut big = BTreeMap::new();
        for index in 0..=MAX_SAVED_DATA_KEYS {
            big.insert(format!("key_{index}"), DataValue::Int(index as i64));
        }
        let mut envelope = SavedDataEnvelope::new("big", 3955, BTreeMap::new()).unwrap();
        envelope.data = big;
        assert!(matches!(
            envelope.validate(),
            Err(SavedDataError::TooManyKeys { .. })
        ));

        // Payload byte cap.
        let huge = DataValue::Str("x".repeat(MAX_SAVED_DATA_ENVELOPE_BYTES + 1));
        let mut envelope = SavedDataEnvelope::new("huge", 3955, BTreeMap::new()).unwrap();
        envelope.data.insert("blob".into(), huge);
        assert!(matches!(
            envelope.validate(),
            Err(SavedDataError::PayloadTooLarge { .. })
        ));

        // File-count cap, and the same name replacing itself is not growth.
        let mut store = SavedDataStore::new();
        for index in 0..MAX_SAVED_DATA_FILES {
            store
                .insert(
                    SavedDataEnvelope::new(
                        format!("file_{index}"),
                        3955,
                        map([("value", DataValue::Int(index as i64))]),
                    )
                    .unwrap(),
                )
                .unwrap();
        }
        store.validate().unwrap();
        assert_eq!(store.len(), MAX_SAVED_DATA_FILES);
        let replacement =
            SavedDataEnvelope::new("file_0", 5023, map([("value", DataValue::Int(9))])).unwrap();
        store.insert(replacement).unwrap();
        assert_eq!(store.len(), MAX_SAVED_DATA_FILES);
        assert_eq!(store.get("file_0").unwrap().data_version(), 5023);
        assert!(matches!(
            store.insert(SavedDataEnvelope::new("one_too_many", 3955, BTreeMap::new()).unwrap()),
            Err(SavedDataError::TooManyFiles { .. })
        ));
    }

    #[test]
    fn store_accounts_bytes_and_reloads_from_disk() {
        let dir = test_dir("disk");
        let mut store = SavedDataStore::new();
        store
            .insert(SavedDataEnvelope::from_root("scoreboard", modded_root()).unwrap())
            .unwrap();
        store
            .insert(
                SavedDataEnvelope::new(
                    "map_7",
                    3955,
                    map([("dimension", DataValue::Str("minecraft:overworld".into()))]),
                )
                .unwrap(),
            )
            .unwrap();
        let expected_bytes = store.get("scoreboard").unwrap().envelope_bytes()
            + store.get("map_7").unwrap().envelope_bytes();
        assert_eq!(store.total_bytes(), expected_bytes);
        store.validate().unwrap();

        // Unrelated files are ignored, a non-`.dat` suffix too.
        std::fs::write(dir.join("notes.txt"), "not saved data").unwrap();
        store.save_to_dir(&dir).unwrap();
        assert!(dir.join("scoreboard.dat").is_file());
        assert!(dir.join("map_7.dat").is_file());
        // Every saved file is a real gzip binary NBT document, not JSON.
        for name in ["scoreboard", "map_7"] {
            let bytes = std::fs::read(dir.join(format!("{name}.dat"))).unwrap();
            assert_eq!(&bytes[..2], &GZIP_MAGIC, "{name}");
            let inflated = gzip_decompress(&bytes, MAX_SAVED_DATA_ENVELOPE_BYTES).unwrap();
            assert!(decode_nbt_to_data_value(&inflated).is_ok(), "{name}");
        }

        let reloaded = SavedDataStore::load_dir(&dir).unwrap();
        reloaded.validate().unwrap();
        assert_eq!(reloaded, store);
        assert_eq!(
            reloaded.get("scoreboard").unwrap().preserved()[0].0,
            "mymod_meta"
        );

        // Removing an envelope corrects the accounting.
        let mut store = reloaded;
        let removed = store.remove("map_7").unwrap();
        assert_eq!(removed.name(), "map_7");
        assert_eq!(
            store.total_bytes(),
            expected_bytes - removed.envelope_bytes()
        );
        assert!(store.get("map_7").is_none());

        // A missing directory is an empty store, not an error.
        assert!(SavedDataStore::load_dir(&dir.join("absent"))
            .unwrap()
            .is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_dir_rejects_traversal_file_names() {
        let dir = test_dir("hostile");
        // Not reachable through a real `read_dir` on every platform, but the
        // stem gate must still refuse it if one ever appears.
        assert_eq!(
            validate_file_stem("../scoreboard").unwrap_err(),
            SavedDataError::InvalidName("../scoreboard".into())
        );
        let root = SavedDataEnvelope::new("scoreboard", 3955, BTreeMap::new())
            .unwrap()
            .to_root();
        std::fs::write(dir.join("scoreboard.dat"), nbt_document(&root)).unwrap();
        let store = SavedDataStore::load_dir(&dir).unwrap();
        assert_eq!(store.len(), 1);
        assert!(store.get("scoreboard").unwrap().preserved().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn gzip_binary_nbt_dat_round_trip() {
        let dir = test_dir("nbt-gzip");
        let envelope = SavedDataEnvelope::from_root("mymod_scores", modded_root()).unwrap();
        let mut store = SavedDataStore::new();
        store.insert(envelope.clone()).unwrap();
        store.save_to_dir(&dir).unwrap();

        // The file on disk is an RFC 1952 gzip member wrapping binary NBT, not
        // a UTF-8 JSON document.
        let bytes = std::fs::read(dir.join("mymod_scores.dat")).unwrap();
        assert_eq!(&bytes[..2], &GZIP_MAGIC);
        assert_ne!(bytes.first().copied(), Some(b'{'));
        let inflated = gzip_decompress(&bytes, MAX_SAVED_DATA_ENVELOPE_BYTES).unwrap();
        let (root_name, root) = decode_nbt_to_data_value(&inflated).unwrap();
        assert!(root_name.is_empty(), "vanilla SavedData roots are unnamed");
        assert_eq!(root, DataValue::Map(modded_root()));

        let reloaded = SavedDataStore::load_dir(&dir).unwrap();
        assert_eq!(reloaded, store);
        assert_eq!(reloaded.get("mymod_scores"), Some(&envelope));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn uncompressed_binary_nbt_loads_and_resaves_as_gzip() {
        let dir = test_dir("nbt-raw");
        let raw = nbt_document(&modded_root());
        assert_ne!(&raw[..2], &GZIP_MAGIC);
        std::fs::write(dir.join("mymod_scores.dat"), &raw).unwrap();

        let mut store = SavedDataStore::load_dir(&dir).unwrap();
        let envelope = store.get("mymod_scores").unwrap();
        assert_eq!(envelope.data_version(), 3955);
        assert_eq!(envelope.to_root(), modded_root());

        store.save_to_dir(&dir).unwrap();
        let bytes = std::fs::read(dir.join("mymod_scores.dat")).unwrap();
        assert_eq!(&bytes[..2], &GZIP_MAGIC);
        assert_eq!(SavedDataStore::load_dir(&dir).unwrap(), store);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unknown_root_keys_and_nested_mod_fields_survive_disk_cycles() {
        let dir = test_dir("nbt-preserve");
        let mut store = SavedDataStore::new();
        store
            .insert(SavedDataEnvelope::from_root("mymod_scores", modded_root()).unwrap())
            .unwrap();
        store.save_to_dir(&dir).unwrap();
        let first_bytes = std::fs::read(dir.join("mymod_scores.dat")).unwrap();

        let mut first = SavedDataStore::load_dir(&dir).unwrap();
        first.save_to_dir(&dir).unwrap();
        let second_bytes = std::fs::read(dir.join("mymod_scores.dat")).unwrap();
        let second = SavedDataStore::load_dir(&dir).unwrap();
        assert_eq!(first, second);
        // Re-encoding a decoded envelope is byte-deterministic.
        assert_eq!(first_bytes, second_bytes);

        let envelope = second.get("mymod_scores").unwrap();
        assert_eq!(envelope.data_version(), 3955);
        assert_eq!(envelope.to_root(), modded_root());
        assert_eq!(
            envelope.preserved(),
            &[(
                "mymod_meta".to_owned(),
                DataValue::Map(map([("owner", DataValue::Str("mymod".into()))])),
            )]
        );
        assert_eq!(
            envelope.data().get("mymod:alpaca_scores"),
            Some(&DataValue::Map(map([("alpaca", DataValue::Int(7))])))
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn non_nbt_dat_files_are_rejected_fail_closed() {
        let json = br#"{"data":{},"DataVersion":3955}"#.to_vec();
        let cases: [(&str, Vec<u8>); 5] = [
            ("json", json.clone()),
            ("gzip-json", gzip_encode_stored(&json)),
            ("empty", Vec::new()),
            ("truncated-nbt", vec![10, 0, 0, 8]),
            (
                "corrupt-gzip",
                vec![0x1f, 0x8b, 8, 0, 0, 0, 0, 0, 0, 0xff, 0x00],
            ),
        ];
        for (label, bytes) in cases {
            let dir = test_dir(&format!("reject-{label}"));
            std::fs::write(dir.join("scoreboard.dat"), &bytes).unwrap();
            let error = SavedDataStore::load_dir(&dir).unwrap_err();
            assert!(
                matches!(
                    error,
                    SavedDataError::MalformedDocument { .. }
                        | SavedDataError::NotAnObject
                        | SavedDataError::Io { .. }
                ),
                "{label}: {error:?}"
            );
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    /// Path of the sealed generation manifest inside `dir`.
    fn manifest_path(dir: &Path) -> PathBuf {
        dir.join(SAVED_DATA_MANIFEST_FILE)
    }

    /// `true` when `name` is one of the transient files a save must clean up.
    fn is_transient(name: &str) -> bool {
        name.starts_with(STAGING_PREFIX) || name.contains("migrate-tmp")
    }

    #[test]
    fn save_publishes_one_generation_per_successful_save() {
        let dir = test_dir("generations");
        let mut store = SavedDataStore::new();
        store
            .insert(
                SavedDataEnvelope::new("scoreboard", 3955, map([("value", DataValue::Int(1))]))
                    .unwrap(),
            )
            .unwrap();
        assert_eq!(
            store.generation(),
            0,
            "a store that never saved has no generation"
        );
        store.save_to_dir(&dir).unwrap();
        assert_eq!(store.generation(), 1);

        // The manifest is sealed, lives beside the documents and is not a `.dat`
        // document, so neither vanilla nor `load_dir` mistakes it for one.
        let raw = std::fs::read(manifest_path(&dir)).unwrap();
        assert_eq!(&raw[..4], &SAVED_DATA_MANIFEST_MAGIC);
        assert!(dir.join("scoreboard.dat").is_file());

        let mut reloaded = SavedDataStore::load_dir(&dir).unwrap();
        assert_eq!(reloaded.generation(), 1);
        assert_eq!(reloaded, store);
        reloaded
            .insert(
                SavedDataEnvelope::new("scoreboard", 3955, map([("value", DataValue::Int(2))]))
                    .unwrap(),
            )
            .unwrap();
        reloaded.save_to_dir(&dir).unwrap();
        assert_eq!(reloaded.generation(), 2);
        let after = SavedDataStore::load_dir(&dir).unwrap();
        assert_eq!(after.generation(), 2);
        assert_eq!(after, reloaded);
        // A save leaves no staging directory or temp file behind.
        for entry in std::fs::read_dir(&dir).unwrap() {
            let name = entry.unwrap().file_name();
            assert!(!is_transient(&name.to_string_lossy()), "{name:?} leaked");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn deletion_tombstone_survives_save_load_and_a_stale_copy() {
        let dir = test_dir("tombstone");
        let mut store = SavedDataStore::new();
        for name in ["scoreboard", "map_7"] {
            store
                .insert(
                    SavedDataEnvelope::new(name, 3955, map([("value", DataValue::Int(7))]))
                        .unwrap(),
                )
                .unwrap();
        }
        store.save_to_dir(&dir).unwrap();
        // A copy of the document that is about to be deleted — exactly what a
        // backup restore or a half-applied save would put back.
        let stale = std::fs::read(dir.join("map_7.dat")).unwrap();

        let removed = store.remove("map_7").unwrap();
        assert_eq!(removed.name(), "map_7");
        assert!(store.is_tombstoned("map_7"));
        assert_eq!(store.tombstones().collect::<Vec<_>>(), vec!["map_7"]);
        store.save_to_dir(&dir).unwrap();
        assert_eq!(store.generation(), 2);
        assert!(
            !dir.join("map_7.dat").exists(),
            "a deletion unlinks the document"
        );

        let mut reloaded = SavedDataStore::load_dir(&dir).unwrap();
        assert!(reloaded.get("map_7").is_none());
        assert!(reloaded.is_tombstoned("map_7"));
        assert_eq!(reloaded.generation(), 2);
        assert_eq!(reloaded, store);

        // A stale copy cannot resurrect deleted state...
        std::fs::write(dir.join("map_7.dat"), &stale).unwrap();
        reloaded = SavedDataStore::load_dir(&dir).unwrap();
        assert!(reloaded.get("map_7").is_none());
        assert!(reloaded.is_tombstoned("map_7"));
        // ...and the next save unlinks it again.
        reloaded.save_to_dir(&dir).unwrap();
        assert!(!dir.join("map_7.dat").exists());
        assert_eq!(SavedDataStore::load_dir(&dir).unwrap(), reloaded);

        // Explicit resurrection is the only way back, and it is recorded.
        assert!(reloaded.clear_tombstone("map_7"));
        assert!(!reloaded.clear_tombstone("map_7"));
        reloaded
            .insert(
                SavedDataEnvelope::new("map_7", 3955, map([("value", DataValue::Int(9))])).unwrap(),
            )
            .unwrap();
        reloaded.save_to_dir(&dir).unwrap();
        let resurrected = SavedDataStore::load_dir(&dir).unwrap();
        assert_eq!(resurrected, reloaded);
        assert!(resurrected.get("map_7").is_some());
        assert!(!resurrected.is_tombstoned("map_7"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unknown_root_keys_and_tombstones_survive_a_manifest_round_trip() {
        let dir = test_dir("manifest-preserve");
        let mut store = SavedDataStore::new();
        store
            .insert(SavedDataEnvelope::from_root("mymod_scores", modded_root()).unwrap())
            .unwrap();
        store
            .insert(
                SavedDataEnvelope::new(
                    "mymod_gone",
                    3955,
                    map([("mymod:owned", DataValue::Int(1))]),
                )
                .unwrap(),
            )
            .unwrap();
        store.save_to_dir(&dir).unwrap();
        store.remove("mymod_gone");
        store.save_to_dir(&dir).unwrap();

        let reloaded = SavedDataStore::load_dir(&dir).unwrap();
        assert_eq!(reloaded, store);
        // The removed mod document is a tombstone, not a silent gap, and the
        // surviving mod envelope is byte-identical including its undocumented
        // root key and namespaced payload.
        assert_eq!(
            reloaded.tombstones().collect::<Vec<_>>(),
            vec!["mymod_gone"]
        );
        assert!(reloaded.is_tombstoned("mymod_gone"));
        assert_eq!(
            reloaded.get("mymod_scores").unwrap().to_root(),
            modded_root()
        );
        assert_eq!(
            reloaded.get("mymod_scores").unwrap().preserved()[0].0,
            "mymod_meta"
        );
        assert!(reloaded
            .get("mymod_scores")
            .unwrap()
            .data()
            .contains_key("mymod:alpaca_scores"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_failed_staged_write_leaves_the_prior_generation_intact() {
        let dir = test_dir("storage-full");
        let mut store = SavedDataStore::new();
        for name in ["alpha", "beta"] {
            store
                .insert(
                    SavedDataEnvelope::new(name, 3955, map([("value", DataValue::Int(1))]))
                        .unwrap(),
                )
                .unwrap();
        }
        store.save_to_dir(&dir).unwrap();
        let before = store.clone();
        let published = [
            ("alpha", std::fs::read(dir.join("alpha.dat")).unwrap()),
            ("beta", std::fs::read(dir.join("beta.dat")).unwrap()),
        ];
        let manifest_before = std::fs::read(manifest_path(&dir)).unwrap();

        // Both documents change, then the second staged write fails the way an
        // exhausted filesystem does. Nothing canonical may move.
        for name in ["alpha", "beta"] {
            store
                .insert(
                    SavedDataEnvelope::new(name, 3955, map([("value", DataValue::Int(2))]))
                        .unwrap(),
                )
                .unwrap();
        }
        let mut writes = 0usize;
        let error = store
            .save_to_dir_with(&dir, |path, bytes| {
                writes += 1;
                if writes == 2 {
                    return Err(io::Error::from(io::ErrorKind::StorageFull));
                }
                write_file_atomic(path, bytes)
            })
            .unwrap_err();
        assert_eq!(writes, 2, "staging stops at the first failed write");
        match &error {
            SavedDataError::Io { path, .. } => assert!(
                path.to_string_lossy().contains(STAGING_PREFIX),
                "the failure must happen before a canonical file is touched: {path:?}"
            ),
            other => panic!("unexpected error {other:?}"),
        }

        // The published generation, every document and the manifest are intact,
        // and no staging directory survives the failure.
        assert_eq!(
            store.generation(),
            1,
            "a failed save never advances the generation"
        );
        for (name, bytes) in published {
            assert_eq!(
                std::fs::read(dir.join(format!("{name}.dat"))).unwrap(),
                bytes,
                "{name}"
            );
        }
        assert_eq!(std::fs::read(manifest_path(&dir)).unwrap(), manifest_before);
        assert_eq!(SavedDataStore::load_dir(&dir).unwrap(), before);
        for entry in std::fs::read_dir(&dir).unwrap() {
            let name = entry.unwrap().file_name();
            assert!(!is_transient(&name.to_string_lossy()), "{name:?} leaked");
        }

        // A retry publishes both new values as the next generation.
        store.save_to_dir(&dir).unwrap();
        assert_eq!(store.generation(), 2);
        let after = SavedDataStore::load_dir(&dir).unwrap();
        assert_eq!(after, store);
        assert_eq!(
            after.get("beta").unwrap().field("value"),
            Some(&DataValue::Int(2))
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_corrupt_manifest_is_rejected_and_a_later_save_repairs_it() {
        let dir = test_dir("bad-manifest");
        let mut store = SavedDataStore::new();
        store
            .insert(
                SavedDataEnvelope::new("scoreboard", 3955, map([("value", DataValue::Int(1))]))
                    .unwrap(),
            )
            .unwrap();
        store.save_to_dir(&dir).unwrap();
        let path = manifest_path(&dir);
        let mut corrupt = std::fs::read(&path).unwrap();
        let last = corrupt.len() - 1;
        corrupt[last] ^= 0x01;
        std::fs::write(&path, &corrupt).unwrap();

        // Rejected fail-closed rather than trusted or silently ignored.
        let error = SavedDataStore::load_dir(&dir).unwrap_err();
        assert!(
            matches!(error, SavedDataError::MalformedManifest { .. }),
            "{error:?}"
        );
        // The recovery path preserves the refused bytes and loads the rest.
        let (mut recovered, quarantined) = SavedDataStore::load_dir_quarantining(&dir).unwrap();
        assert_eq!(quarantined, vec![path.clone()]);
        let preserved =
            path.with_file_name(format!("{SAVED_DATA_MANIFEST_FILE}{QUARANTINE_SUFFIX}"));
        assert_eq!(std::fs::read(&preserved).unwrap(), corrupt);
        assert_eq!(
            recovered.generation(),
            0,
            "a quarantined manifest supplies no trusted generation"
        );
        assert!(recovered.get("scoreboard").is_some());

        // One corrupt index does not poison later saves: the next one publishes
        // a fresh manifest and the documents load again.
        recovered.save_to_dir(&dir).unwrap();
        assert_eq!(recovered.generation(), 1);
        assert_eq!(SavedDataStore::load_dir(&dir).unwrap(), recovered);
        assert_eq!(
            std::fs::read(&preserved).unwrap(),
            corrupt,
            "the quarantined bytes are never overwritten"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_corrupt_document_is_quarantined_without_poisoning_later_saves() {
        let dir = test_dir("bad-nbt");
        let mut store = SavedDataStore::new();
        for name in ["scoreboard", "map_7"] {
            store
                .insert(
                    SavedDataEnvelope::new(name, 3955, map([("value", DataValue::Int(3))]))
                        .unwrap(),
                )
                .unwrap();
        }
        store.save_to_dir(&dir).unwrap();
        let corrupt_bytes = b"not an nbt document".to_vec();
        std::fs::write(dir.join("map_7.dat"), &corrupt_bytes).unwrap();

        // The strict loader refuses the whole directory (fail closed) ...
        assert!(matches!(
            SavedDataStore::load_dir(&dir).unwrap_err(),
            SavedDataError::MalformedDocument { .. }
        ));
        // ... and the recovery path keeps the refused bytes plus the survivors.
        let (mut recovered, quarantined) = SavedDataStore::load_dir_quarantining(&dir).unwrap();
        assert_eq!(quarantined, vec![dir.join("map_7.dat")]);
        assert_eq!(
            std::fs::read(dir.join(format!("map_7.dat{QUARANTINE_SUFFIX}"))).unwrap(),
            corrupt_bytes
        );
        assert!(!dir.join("map_7.dat").exists());
        assert_eq!(recovered.generation(), 1);
        assert!(recovered.get("map_7").is_none());
        assert!(recovered.get("scoreboard").is_some());

        // Later saves work and never overwrite the quarantined document.
        recovered
            .insert(SavedDataEnvelope::new("map_8", 3955, BTreeMap::new()).unwrap())
            .unwrap();
        recovered.save_to_dir(&dir).unwrap();
        assert_eq!(SavedDataStore::load_dir(&dir).unwrap(), recovered);
        assert_eq!(
            std::fs::read(dir.join(format!("map_7.dat{QUARANTINE_SUFFIX}"))).unwrap(),
            corrupt_bytes
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn concurrent_writers_never_publish_a_partial_document() {
        let dir = test_dir("concurrent");
        let writers = 4usize;
        let rounds = 8usize;
        // Each writer publishes a distinct, self-describing payload, so a torn
        // or interleaved install is visible in the bytes themselves.
        let expected: Vec<Vec<u8>> = (0..writers)
            .map(|writer| vec![b'0' + writer as u8; 4096])
            .collect();
        let mut handles = Vec::with_capacity(writers);
        for payload in expected.iter().cloned() {
            let dir = dir.clone();
            handles.push(std::thread::spawn(move || {
                let mut store = SavedDataStore::new();
                let text = String::from_utf8(payload).expect("ASCII payload");
                for _ in 0..rounds {
                    store
                        .insert(
                            SavedDataEnvelope::new(
                                "shared",
                                3955,
                                map([("blob", DataValue::Str(text.clone()))]),
                            )
                            .unwrap(),
                        )
                        .unwrap();
                    store.save_to_dir(&dir).unwrap();
                }
            }));
        }

        // Read the directory while those writers replace the same document:
        // every load either has no document yet or exactly one writer's payload.
        let mut observations = 0usize;
        let mut spins = 0usize;
        loop {
            let store = match SavedDataStore::load_dir(&dir) {
                Ok(store) => store,
                Err(error) => {
                    panic!("a concurrent save published an unreadable directory: {error}")
                }
            };
            if let Some(envelope) = store.get("shared") {
                let text = match envelope.field("blob") {
                    Some(DataValue::Str(text)) => text.clone(),
                    other => panic!("published an unexpected payload: {other:?}"),
                };
                assert!(
                    expected
                        .iter()
                        .any(|payload| payload.as_slice() == text.as_bytes()),
                    "a torn or interleaved document was published"
                );
                observations += 1;
            }
            if handles.iter().all(|handle| handle.is_finished()) && observations > 0 {
                break;
            }
            spins += 1;
            assert!(spins < 1_000_000, "no writer ever published a document");
        }
        for handle in handles {
            handle.join().expect("writer thread");
        }

        let published = SavedDataStore::load_dir(&dir).unwrap();
        let text = match published.get("shared").unwrap().field("blob") {
            Some(DataValue::Str(text)) => text.clone(),
            other => panic!("unexpected final payload: {other:?}"),
        };
        assert!(expected
            .iter()
            .any(|payload| payload.as_slice() == text.as_bytes()));
        assert!(published.generation() >= 1);
        for entry in std::fs::read_dir(&dir).unwrap() {
            let name = entry.unwrap().file_name();
            assert!(!is_transient(&name.to_string_lossy()), "{name:?} leaked");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
