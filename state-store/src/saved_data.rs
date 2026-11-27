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
//! Everything is pure `std`; the JSON front end reuses the crate's canonical
//! [`crate::migrate::parse_data_json`] / [`crate::migrate::to_data_json`] codec
//! so a document round-trips through the same number-lexeme-preserving tree the
//! migration pipeline uses.

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

use crate::datafix::DataValue;
use crate::migrate::{parse_data_json, to_data_json, write_file_atomic};
use crate::vanilla_schema::{unknown_keys_to_preserve, SchemaScope};

/// Most `data/*.dat` documents one [`SavedDataStore`] may hold.
pub const MAX_SAVED_DATA_FILES: usize = 256;
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

/// Vanilla envelope key holding the subclass payload.
pub const SAVED_DATA_PAYLOAD_KEY: &str = "data";
/// Vanilla envelope key holding the save `DataVersion`.
pub const SAVED_DATA_VERSION_KEY: &str = "DataVersion";

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
    /// The envelope is not a JSON object, so it cannot be a `SavedData` root.
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
    /// JSON parse failure for `data/<name>.dat`.
    MalformedDocument { path: PathBuf, message: String },
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
            Self::MissingDataCompound => {
                f.write_str("SavedData document has no `data` compound")
            }
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

    /// Serialize the envelope as one `data/<name>.dat` document.
    pub fn to_json_text(&self) -> String {
        to_data_json(&DataValue::Map(self.to_root()))
    }
}

/// One instance-owned set of `data/*.dat` envelopes, capped globally.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SavedDataStore {
    files: BTreeMap<String, SavedDataEnvelope>,
}

impl SavedDataStore {
    /// Empty store.
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert or replace one envelope after re-checking every store cap.
    pub fn insert(&mut self, envelope: SavedDataEnvelope) -> Result<(), SavedDataError> {
        envelope.validate()?;
        let bytes = envelope.envelope_bytes();
        let replacing = self.files.contains_key(envelope.name());
        if !replacing && self.files.len() >= MAX_SAVED_DATA_FILES {
            return Err(SavedDataError::TooManyFiles {
                files: self.files.len() + 1,
                max: MAX_SAVED_DATA_FILES,
            });
        }
        let previous_bytes = self
            .files
            .get(envelope.name())
            .map_or(0, SavedDataEnvelope::envelope_bytes);
        let total = self.total_bytes() - previous_bytes + bytes;
        if total > MAX_SAVED_DATA_STORE_BYTES {
            return Err(SavedDataError::StoreTooLarge {
                bytes: total,
                max: MAX_SAVED_DATA_STORE_BYTES,
            });
        }
        self.files.insert(envelope.name().to_owned(), envelope);
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

    /// Remove one envelope.
    pub fn remove(&mut self, name: &str) -> Option<SavedDataEnvelope> {
        self.files.remove(name)
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
        self.files.values().map(SavedDataEnvelope::envelope_bytes).sum()
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
    /// Only regular files with a safe stem and a `.dat` suffix are read; every
    /// other entry is ignored. A document that fails to parse or exceeds a cap
    /// aborts the whole load (fail closed) instead of being silently skipped.
    /// A missing directory is an empty store.
    pub fn load_dir(dir: &Path) -> Result<Self, SavedDataError> {
        let entries = match std::fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Self::new()),
            Err(error) => {
                return Err(SavedDataError::Io {
                    path: dir.to_path_buf(),
                    message: error.to_string(),
                });
            }
        };
        let mut store = Self::new();
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
            let bytes = read_bounded(&path, MAX_SAVED_DATA_ENVELOPE_BYTES)?;
            let text = String::from_utf8(bytes).map_err(|_| SavedDataError::MalformedDocument {
                path: path.clone(),
                message: "not UTF-8 JSON".to_owned(),
            })?;
            let envelope = SavedDataEnvelope::from_json_text(stem, &text).map_err(|error| {
                SavedDataError::MalformedDocument {
                    path: path.clone(),
                    message: error.to_string(),
                }
            })?;
            store.insert(envelope)?;
        }
        store.validate()?;
        Ok(store)
    }

    /// Write every envelope as `<dir>/<name>.dat` through the atomic
    /// write-temp-then-rename path. Existing unrelated files are untouched;
    /// an envelope that no longer exists is not removed (a caller that wants
    /// deletion drives it through [`SavedDataStore::remove`] plus its own
    /// cleanup).
    pub fn save_to_dir(&self, dir: &Path) -> Result<(), SavedDataError> {
        self.validate()?;
        std::fs::create_dir_all(dir).map_err(|error| SavedDataError::Io {
            path: dir.to_path_buf(),
            message: error.to_string(),
        })?;
        for (name, envelope) in &self.files {
            let path = dir.join(format!("{name}.dat"));
            let document = envelope.to_json_text();
            write_file_atomic(&path, document.as_bytes()).map_err(|error| SavedDataError::Io {
                path,
                message: error.to_string(),
            })?;
        }
        Ok(())
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

    fn map(entries: impl IntoIterator<Item = (&'static str, DataValue)>) -> BTreeMap<String, DataValue> {
        entries
            .into_iter()
            .map(|(key, value)| (key.to_owned(), value))
            .collect()
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
        let preserved: Vec<&str> = envelope.preserved().iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(preserved, vec!["mymod_meta"]);
        assert_eq!(
            preserved,
            unknown_keys_to_preserve(
                SchemaScope::AuxMap,
                ["data", "DataVersion", "mymod_meta"].into_iter()
            )
        );
        // Namespaced payload keys inside `data` ride along untouched.
        assert!(envelope
            .data()
            .contains_key("mymod:alpaca_scores"));

        // The rebuilt root is byte-for-byte the input document.
        assert_eq!(envelope.to_root(), modded_root());

        // JSON round-trip through the canonical codec.
        let json = envelope.to_json_text();
        let reparsed = SavedDataEnvelope::from_json_text("mymod_scores", &json).unwrap();
        assert_eq!(reparsed, envelope);
        assert_eq!(reparsed.to_root(), modded_root());
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
            envelope.put_field("DataVersion", DataValue::Int(1)).unwrap_err(),
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
        let huge =
            DataValue::Str("x".repeat(MAX_SAVED_DATA_ENVELOPE_BYTES + 1));
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
            store.insert(
                SavedDataEnvelope::new("one_too_many", 3955, BTreeMap::new()).unwrap()
            ),
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
        let expected_bytes = store
            .get("scoreboard")
            .unwrap()
            .envelope_bytes()
            + store.get("map_7").unwrap().envelope_bytes();
        assert_eq!(store.total_bytes(), expected_bytes);
        store.validate().unwrap();

        // Unrelated files are ignored, a non-`.dat` suffix too.
        std::fs::write(dir.join("notes.txt"), "not saved data").unwrap();
        store.save_to_dir(&dir).unwrap();
        assert!(dir.join("scoreboard.dat").is_file());
        assert!(dir.join("map_7.dat").is_file());

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
        assert!(SavedDataStore::load_dir(&dir.join("absent")).unwrap().is_empty());
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
        std::fs::write(
            dir.join("scoreboard.dat"),
            r#"{"data":{},"DataVersion":3955}"#,
        )
        .unwrap();
        let store = SavedDataStore::load_dir(&dir).unwrap();
        assert_eq!(store.len(), 1);
        assert!(store.get("scoreboard").unwrap().preserved().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
