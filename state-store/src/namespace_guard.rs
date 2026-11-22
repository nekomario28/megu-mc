// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
#![deny(unsafe_code)]
//! Unknown-namespace non-destructive preservation and safe-rollback routing.
//!
//! Vanilla's modded-world failure mode is silent loss: remove or disable a
//! mod (or walk a save between the `1.21.1` and `26.3` targets) and every
//! `modid:block`, `modid:item` and `modid:entity` the registry no longer
//! knows is *deleted* on load. Re-installing the mod afterwards cannot bring
//! the data back — it was never written anywhere.
//!
//! [`NamespaceTombstoneSidecar`] is the quarantine sidecar that makes the
//! same load survivable. When [`preserve_unknown`] meets an id whose
//! namespace is absent from the active set, the entry's full original
//! payload — block-state properties, block-entity NBT, item components,
//! entity NBT — is parked verbatim in the sidecar keyed by its location,
//! and the caller substitutes a safe vanilla placeholder so the chunk or
//! inventory finishes loading cleanly. When the mod returns,
//! [`restore_for_reloaded_namespaces`] hands every matching entry back with
//! its payload untouched.
//!
//! [`guard_world_load_with_snapshot`] is the operator-facing entry point:
//! before a single placeholder substitution happens it takes a
//! content-addressed [`WorldSnapshotStore`] backup of the world and re-verifies
//! every blob, so even a sidecar-format bug can never lose the world — the
//! pre-change state is sealed and verified before any mutation is written.
//!
//! Everything is pure `std`, deterministic (sidecars serialize through the
//! crate's sorted-key JSON codec), and fail-closed: malformed ids, a corrupt
//! or foreign sidecar document, and the `MAX_PRESERVED_ENTRIES` cap all
//! produce errors rather than silent drops, and no wall clock is read —
//! timestamps come from the caller.

use crate::datafix::DataValue;
use crate::migrate::{parse_data_json, to_data_json, write_file_atomic, JsonError};
use crate::world_snapshot::{SnapshotReason, WorldSnapshotError, WorldSnapshotStore};
use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

/// Sidecar document format tag, checked on every load so a foreign or
/// corrupt document is never mistaken for a tombstone sidecar.
const SIDECAR_FORMAT: &str = "mcspine-namespace-tombstone-v1";

/// Fail-closed cap on parked entries. Far past anything a real modded save
/// produces — it exists to refuse garbage, never to bound legitimate worlds.
pub const MAX_PRESERVED_ENTRIES: usize = 500_000;

/// Byte cap on a sidecar document (256 MiB). Entries carry full NBT payloads,
/// so this sits well above a capped sidecar's realistic size.
const MAX_SIDECAR_BYTES: u64 = 256 * 1_048_576;

/// Fail-closed length caps on caller-supplied strings. Real resource
/// locations and location keys are tens of bytes; the caps refuse garbage.
const MAX_ID_LEN: usize = 1_024;
const MAX_LOCATION_KEY_LEN: usize = 4_096;

/// Placeholder written for a quarantined block: invisible, non-interactive,
/// and drops nothing.
pub const PLACEHOLDER_BLOCK: &str = "minecraft:structure_void";
/// Placeholder written for a quarantined item stack: a plain vanilla item
/// with no behavior.
pub const PLACEHOLDER_ITEM: &str = "minecraft:paper";
/// Placeholder written for a quarantined entity: vanilla's zero-impact
/// marker entity — no AI, no rendering, no drops.
pub const PLACEHOLDER_ENTITY: &str = "minecraft:marker";

/// Which kind of registry object a parked entry was.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum PreservedKind {
    Block,
    Item,
    Entity,
}

impl PreservedKind {
    /// The safe vanilla id substituted while the original is quarantined.
    pub fn placeholder_id(&self) -> &'static str {
        match self {
            PreservedKind::Block => PLACEHOLDER_BLOCK,
            PreservedKind::Item => PLACEHOLDER_ITEM,
            PreservedKind::Entity => PLACEHOLDER_ENTITY,
        }
    }

    fn as_str(&self) -> &'static str {
        match self {
            PreservedKind::Block => "block",
            PreservedKind::Item => "item",
            PreservedKind::Entity => "entity",
        }
    }

    fn from_str(s: &str) -> Option<Self> {
        match s {
            "block" => Some(PreservedKind::Block),
            "item" => Some(PreservedKind::Item),
            "entity" => Some(PreservedKind::Entity),
            _ => None,
        }
    }
}

/// One quarantined registry object with its complete original payload.
#[derive(Clone, Debug, PartialEq)]
pub struct PreservedEntry {
    /// Whether this was a block, item or entity.
    pub kind: PreservedKind,
    /// The exact id the world carried, e.g. `"create:mechanical_press"`.
    pub original_id: String,
    /// `original_id`'s namespace segment, e.g. `"create"`.
    pub namespace: String,
    /// Where the entry lived, e.g. `"minecraft:overworld@12,64,-8"` or
    /// `"player:alex:slot:5"`. Unique per sidecar — it is the map key.
    pub location_key: String,
    /// The vanilla id loaded in its place while quarantined.
    pub placeholder_id: String,
    /// The original data, preserved losslessly: block-state properties,
    /// block-entity NBT, item components or entity NBT.
    pub raw_payload: DataValue,
    /// Caller-supplied stamp recording when the entry was quarantined.
    pub quarantined_at_tick: u64,
}

/// An unknown-id sighting handed to [`NamespaceTombstoneSidecar::preserve_unknown`]
/// or [`guard_world_load_with_snapshot`]. `namespace`, `placeholder_id` and
/// `quarantined_at_tick` are derived by the guard — callers supply only what
/// the world itself carried.
#[derive(Clone, Debug, PartialEq)]
pub struct UnknownEntry {
    /// Whether this is a block, item or entity.
    pub kind: PreservedKind,
    /// The id the world carried, e.g. `"create:brass_funnel"`.
    pub original_id: String,
    /// Where it lives, e.g. `"minecraft:overworld@12,64,-8"`.
    pub location_key: String,
    /// The entry's complete original data.
    pub raw_payload: DataValue,
}

/// Why a namespace-guard operation failed.
#[derive(Debug)]
pub enum NamespaceGuardError {
    /// An underlying filesystem call failed; `op` names what was attempted.
    Io {
        op: &'static str,
        path: PathBuf,
        source: io::Error,
    },
    /// The sidecar text is not valid JSON.
    Json(JsonError),
    /// The sidecar parsed but failed schema verification: wrong format tag,
    /// mistyped fields, a count mismatch, or a duplicate location key.
    Schema(String),
    /// A caller-supplied id or location key is malformed.
    InvalidEntry { reason: String },
    /// The sidecar is full (`MAX_PRESERVED_ENTRIES`) and refused a new
    /// location key rather than dropping an existing record.
    CapExceeded { cap: usize },
    /// The pre-change world snapshot failed or did not verify — the guard
    /// refuses to mutate anything without a proven rollback.
    Snapshot(WorldSnapshotError),
}

impl fmt::Display for NamespaceGuardError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            NamespaceGuardError::Io { op, path, source } => {
                write!(f, "{op} failed on {}: {source}", path.display())
            }
            NamespaceGuardError::Json(e) => write!(f, "sidecar is not valid JSON: {e}"),
            NamespaceGuardError::Schema(reason) => {
                write!(f, "sidecar failed schema verification: {reason}")
            }
            NamespaceGuardError::InvalidEntry { reason } => {
                write!(f, "invalid preserved entry: {reason}")
            }
            NamespaceGuardError::CapExceeded { cap } => {
                write!(f, "sidecar reached the {cap}-entry cap; refusing to drop records")
            }
            NamespaceGuardError::Snapshot(e) => write!(f, "pre-change snapshot failed: {e}"),
        }
    }
}

impl Error for NamespaceGuardError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            NamespaceGuardError::Io { source, .. } => Some(source),
            NamespaceGuardError::Json(e) => Some(e),
            NamespaceGuardError::Snapshot(e) => Some(e),
            _ => None,
        }
    }
}

impl From<JsonError> for NamespaceGuardError {
    fn from(e: JsonError) -> Self {
        NamespaceGuardError::Json(e)
    }
}

impl From<WorldSnapshotError> for NamespaceGuardError {
    fn from(e: WorldSnapshotError) -> Self {
        NamespaceGuardError::Snapshot(e)
    }
}

fn ioe(op: &'static str, path: &Path) -> impl FnOnce(io::Error) -> NamespaceGuardError {
    let path = path.to_path_buf();
    move |source| NamespaceGuardError::Io { op, path, source }
}

/// Split `id` into `(namespace, path)`. Vanilla's default-namespace rule
/// applies: a bare `"stone"` is `"minecraft"`/`"stone"`. Anything without a
/// non-empty namespace and path, or with a second `:` inside the path, is
/// malformed and returns `None` — never guessed at.
fn split_resource_id(id: &str) -> Option<(&str, &str)> {
    match id.split_once(':') {
        Some((ns, path)) if !ns.is_empty() && !path.is_empty() && !path.contains(':') => {
            Some((ns, path))
        }
        None if !id.is_empty() && !id.contains(':') => Some(("minecraft", id)),
        _ => None,
    }
}

/// The namespace segment of a resource location, with the vanilla
/// `minecraft` default for bare paths. `None` when `id` is malformed.
pub fn namespace_of(id: &str) -> Option<&str> {
    split_resource_id(id).map(|(ns, _)| ns)
}

fn validate_id(id: &str) -> Result<(&str, &str), NamespaceGuardError> {
    if id.len() > MAX_ID_LEN {
        return Err(NamespaceGuardError::InvalidEntry {
            reason: format!("id exceeds the {MAX_ID_LEN}-byte cap"),
        });
    }
    split_resource_id(id).ok_or_else(|| NamespaceGuardError::InvalidEntry {
        reason: format!("id {id:?} is not a valid resource location"),
    })
}

fn validate_location_key(location_key: &str) -> Result<(), NamespaceGuardError> {
    if location_key.is_empty() || location_key.len() > MAX_LOCATION_KEY_LEN {
        return Err(NamespaceGuardError::InvalidEntry {
            reason: format!("location key {location_key:?} is empty or over the {MAX_LOCATION_KEY_LEN}-byte cap"),
        });
    }
    Ok(())
}

// ------------------------------------------------------------ the sidecar

/// The deterministic quarantine table: [`PreservedEntry`]s keyed by
/// `location_key` in a `BTreeMap`, so iteration, serialization and restore
/// order are all stable.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct NamespaceTombstoneSidecar {
    entries: BTreeMap<String, PreservedEntry>,
}

impl NamespaceTombstoneSidecar {
    /// An empty sidecar.
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of parked entries.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether nothing is parked.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The parked record at `location_key`, if any.
    pub fn get(&self, location_key: &str) -> Option<&PreservedEntry> {
        self.entries.get(location_key)
    }

    /// Every parked record, ordered by `location_key`.
    pub fn entries(&self) -> &BTreeMap<String, PreservedEntry> {
        &self.entries
    }

    /// Route one discovered registry object.
    ///
    /// `entry.original_id`'s namespace in `active_namespaces` means the id is
    /// live: the entry is left untouched and `Ok(None)` tells the caller to
    /// load it as-is. An absent namespace means the owning mod is gone: the
    /// complete `raw_payload` is parked in the sidecar and `Ok(Some(_))`
    /// returns the safe vanilla placeholder id to substitute so the chunk,
    /// inventory or entity list finishes loading.
    ///
    /// Re-preserving an already-parked `location_key` replaces its record —
    /// that can only happen when the world genuinely rewrote the location
    /// (a placeholder reads as `minecraft:*`, which is never unknown), so
    /// the newest observation is the accurate one. The `MAX_PRESERVED_ENTRIES`
    /// cap refuses a *new* key when full rather than evicting a record.
    pub fn preserve_unknown(
        &mut self,
        entry: UnknownEntry,
        quarantined_at_tick: u64,
        active_namespaces: &BTreeSet<String>,
    ) -> Result<Option<String>, NamespaceGuardError> {
        let (namespace, _path) = validate_id(&entry.original_id)?;
        validate_location_key(&entry.location_key)?;
        if active_namespaces.contains(namespace) {
            return Ok(None);
        }
        if !self.entries.contains_key(&entry.location_key)
            && self.entries.len() >= MAX_PRESERVED_ENTRIES
        {
            return Err(NamespaceGuardError::CapExceeded {
                cap: MAX_PRESERVED_ENTRIES,
            });
        }
        let placeholder_id = entry.kind.placeholder_id().to_string();
        let record = PreservedEntry {
            kind: entry.kind,
            namespace: namespace.to_string(),
            location_key: entry.location_key.clone(),
            original_id: entry.original_id,
            placeholder_id: placeholder_id.clone(),
            raw_payload: entry.raw_payload,
            quarantined_at_tick,
        };
        self.entries.insert(record.location_key.clone(), record);
        Ok(Some(placeholder_id))
    }

    /// Pop and return every parked entry whose namespace is live again in
    /// `active_namespaces` — the mod came back, so its payloads can be
    /// written into the world immediately. Entries for still-missing
    /// namespaces stay parked. Returned entries are ordered by
    /// `location_key` for deterministic restoration.
    pub fn restore_for_reloaded_namespaces(
        &mut self,
        active_namespaces: &BTreeSet<String>,
    ) -> Vec<PreservedEntry> {
        let keys: Vec<String> = self
            .entries
            .values()
            .filter(|e| active_namespaces.contains(e.namespace.as_str()))
            .map(|e| e.location_key.clone())
            .collect();
        let mut restored = Vec::with_capacity(keys.len());
        for key in keys {
            if let Some(entry) = self.entries.remove(&key) {
                restored.push(entry);
            }
        }
        restored
    }

    /// Operator-facing count of parked entries per missing namespace — the
    /// "how much of mod X is safely stored" report.
    pub fn summary_by_namespace(&self) -> BTreeMap<String, usize> {
        let mut out = BTreeMap::new();
        for entry in self.entries.values() {
            *out.entry(entry.namespace.clone()).or_insert(0) += 1;
        }
        out
    }

    /// Serialize the whole sidecar as a schema-tagged JSON document and
    /// install it atomically via [`write_file_atomic`] — a crash mid-save can
    /// never leave a torn sidecar behind.
    pub fn save_atomic(&self, path: &Path) -> Result<(), NamespaceGuardError> {
        let mut root = BTreeMap::new();
        root.insert(
            "format".to_string(),
            DataValue::Str(SIDECAR_FORMAT.to_string()),
        );
        root.insert(
            "entry_count".to_string(),
            DataValue::Int(self.entries.len() as i64),
        );
        root.insert(
            "entries".to_string(),
            DataValue::List(self.entries.values().map(entry_to_json).collect()),
        );
        let text = to_data_json(&DataValue::Map(root));
        write_file_atomic(path, text.as_bytes()).map_err(ioe("write sidecar", path))
    }

    /// Read and fully verify a sidecar document. Fail-closed at every step:
    /// unreadable file, past-cap bytes, non-UTF-8, non-JSON, wrong format
    /// tag, a declared `entry_count` that disagrees with the array, any
    /// mistyped or unexpected field, a `namespace` that disagrees with
    /// `original_id`, a non-vanilla `placeholder_id`, or a duplicate
    /// `location_key` all produce `Err` — a corrupt sidecar is never
    /// silently emptied or partially adopted.
    pub fn load_verified(path: &Path) -> Result<Self, NamespaceGuardError> {
        let bytes = std::fs::read(path).map_err(ioe("read sidecar", path))?;
        if bytes.len() as u64 > MAX_SIDECAR_BYTES {
            return Err(NamespaceGuardError::Schema(format!(
                "sidecar exceeds the {}-byte cap",
                MAX_SIDECAR_BYTES
            )));
        }
        let text = String::from_utf8(bytes).map_err(|_| {
            NamespaceGuardError::Schema("sidecar is not UTF-8".to_string())
        })?;
        let root = parse_data_json(&text)?;
        let map = root
            .as_map()
            .ok_or_else(|| NamespaceGuardError::Schema("root is not an object".to_string()))?;
        match map.get("format").and_then(DataValue::as_str) {
            Some(SIDECAR_FORMAT) => {}
            Some(other) => {
                return Err(NamespaceGuardError::Schema(format!(
                    "format tag {other:?} is not {SIDECAR_FORMAT:?}"
                )))
            }
            None => {
                return Err(NamespaceGuardError::Schema(
                    "missing format tag".to_string(),
                ))
            }
        }
        let list = match map.get("entries") {
            Some(DataValue::List(list)) => list,
            _ => {
                return Err(NamespaceGuardError::Schema(
                    "entries is missing or not an array".to_string(),
                ))
            }
        };
        if let Some(declared) = map.get("entry_count") {
            let declared = declared.as_int().ok_or_else(|| {
                NamespaceGuardError::Schema("entry_count is not an integer".to_string())
            })?;
            if declared < 0 || declared as usize != list.len() {
                return Err(NamespaceGuardError::Schema(format!(
                    "entry_count {declared} disagrees with {} entries",
                    list.len()
                )));
            }
        }
        if list.len() > MAX_PRESERVED_ENTRIES {
            return Err(NamespaceGuardError::CapExceeded {
                cap: MAX_PRESERVED_ENTRIES,
            });
        }
        let mut sidecar = Self::new();
        for item in list {
            let entry = entry_from_json(item)?;
            if sidecar
                .entries
                .insert(entry.location_key.clone(), entry)
                .is_some()
            {
                return Err(NamespaceGuardError::Schema(
                    "duplicate location_key in sidecar".to_string(),
                ));
            }
        }
        Ok(sidecar)
    }
}

/// Every field a serialized entry must carry — checked exactly, so a
/// foreign or drifted schema is refused rather than partially read.
const ENTRY_FIELDS: &[&str] = &[
    "kind",
    "location_key",
    "namespace",
    "original_id",
    "placeholder_id",
    "quarantined_at_tick",
    "raw_payload",
];

fn entry_to_json(entry: &PreservedEntry) -> DataValue {
    let mut m = BTreeMap::new();
    m.insert(
        "kind".to_string(),
        DataValue::Str(entry.kind.as_str().to_string()),
    );
    m.insert(
        "location_key".to_string(),
        DataValue::Str(entry.location_key.clone()),
    );
    m.insert(
        "namespace".to_string(),
        DataValue::Str(entry.namespace.clone()),
    );
    m.insert(
        "original_id".to_string(),
        DataValue::Str(entry.original_id.clone()),
    );
    m.insert(
        "placeholder_id".to_string(),
        DataValue::Str(entry.placeholder_id.clone()),
    );
    m.insert(
        "quarantined_at_tick".to_string(),
        // u64 ticks past i64::MAX saturate rather than wrap into a
        // negative that would fail verification on load.
        DataValue::Int(i64::try_from(entry.quarantined_at_tick).unwrap_or(i64::MAX)),
    );
    m.insert("raw_payload".to_string(), entry.raw_payload.clone());
    DataValue::Map(m)
}

fn required_str<'m>(
    map: &'m BTreeMap<String, DataValue>,
    field: &str,
) -> Result<&'m str, NamespaceGuardError> {
    map.get(field)
        .and_then(DataValue::as_str)
        .ok_or_else(|| NamespaceGuardError::Schema(format!("entry field {field:?} is missing or not a string")))
}

fn entry_from_json(value: &DataValue) -> Result<PreservedEntry, NamespaceGuardError> {
    let map = value
        .as_map()
        .ok_or_else(|| NamespaceGuardError::Schema("entry is not an object".to_string()))?;
    for key in map.keys() {
        if !ENTRY_FIELDS.contains(&key.as_str()) {
            return Err(NamespaceGuardError::Schema(format!(
                "entry carries unexpected field {key:?}"
            )));
        }
    }
    for field in ENTRY_FIELDS {
        if !map.contains_key(*field) {
            return Err(NamespaceGuardError::Schema(format!(
                "entry is missing field {field:?}"
            )));
        }
    }
    let kind = PreservedKind::from_str(required_str(map, "kind")?).ok_or_else(|| {
        NamespaceGuardError::Schema("entry kind is not block/item/entity".to_string())
    })?;
    let original_id = required_str(map, "original_id")?;
    let location_key = required_str(map, "location_key")?;
    let namespace = required_str(map, "namespace")?;
    let placeholder_id = required_str(map, "placeholder_id")?;
    let tick = map
        .get("quarantined_at_tick")
        .and_then(DataValue::as_int)
        .ok_or_else(|| {
            NamespaceGuardError::Schema(
                "quarantined_at_tick is missing or not an integer".to_string(),
            )
        })?;
    let quarantined_at_tick = u64::try_from(tick).map_err(|_| {
        NamespaceGuardError::Schema(format!("quarantined_at_tick {tick} is negative"))
    })?;
    let raw_payload = map
        .get("raw_payload")
        .cloned()
        .ok_or_else(|| NamespaceGuardError::Schema("missing raw_payload".to_string()))?;
    // Cross-checks, all fail-closed: the stored namespace must agree with
    // the id it claims to preserve, the location key must be well-formed,
    // and the placeholder must be a vanilla id — a modded placeholder would
    // itself need quarantining, which the format forbids.
    let (ns, _path) = validate_id(original_id)
        .map_err(|e| NamespaceGuardError::Schema(format!("original_id: {e}")))?;
    if ns != namespace {
        return Err(NamespaceGuardError::Schema(format!(
            "entry namespace {namespace:?} disagrees with original_id {original_id:?}"
        )));
    }
    validate_location_key(location_key)
        .map_err(|e| NamespaceGuardError::Schema(format!("location_key: {e}")))?;
    if namespace_of(placeholder_id) != Some("minecraft") {
        return Err(NamespaceGuardError::Schema(format!(
            "placeholder_id {placeholder_id:?} is not a vanilla id"
        )));
    }
    Ok(PreservedEntry {
        kind,
        original_id: original_id.to_string(),
        namespace: namespace.to_string(),
        location_key: location_key.to_string(),
        placeholder_id: placeholder_id.to_string(),
        raw_payload,
        quarantined_at_tick,
    })
}

// -------------------------------------------------- the load-time router

/// What [`guard_world_load_with_snapshot`] did for one world load.
#[derive(Clone, Debug, PartialEq)]
pub struct NamespaceGuardReport {
    /// The verified pre-change world snapshot. Present only when the whole
    /// guard pass succeeded — it is the rollback handle.
    pub snapshot_id: String,
    /// How many entries each missing namespace had parked during this pass.
    pub preserved_counts_by_namespace: BTreeMap<String, usize>,
    /// `true` only when the snapshot was created *and* verified, every
    /// unknown entry was parked, and the sidecar was written atomically —
    /// the point where placeholder substitution is provably safe.
    pub safe_to_proceed: bool,
}

/// Guard one world load against missing namespaces.
///
/// Order is the safety contract:
///
/// 1. A [`WorldSnapshotStore`] backup of `world_root` is taken into
///    `backup_root` and re-verified blob-by-blob *before* anything else —
///    the rollback exists and is proven intact before any placeholder
///    mutation is contemplated.
/// 2. The sidecar at `sidecar_path` is loaded when it exists (a corrupt or
///    foreign document is an error, never a silent reset) so parked entries
///    accumulate across restarts.
/// 3. Every `discovered` entry is routed through
///    [`NamespaceTombstoneSidecar::preserve_unknown`]; each unknown id's full
///    payload is parked.
/// 4. The updated sidecar is written atomically.
///
/// `timestamp_secs` is caller-supplied — the guard never reads the wall
/// clock — and doubles as the `quarantined_at_tick` stamp for entries parked
/// in this pass. Any failure returns `Err` before step 4, so a failed guard
/// can never leave a verified-snapshot-less mutation half-applied.
pub fn guard_world_load_with_snapshot(
    world_root: &Path,
    backup_root: &Path,
    sidecar_path: &Path,
    active_namespaces: &BTreeSet<String>,
    discovered: &[UnknownEntry],
    mc_version: &str,
    timestamp_secs: u64,
) -> Result<NamespaceGuardReport, NamespaceGuardError> {
    // 1. The rollback is sealed and verified first. `create_snapshot` only
    //    reads the world; nothing under `world_root` is mutated here.
    let store = WorldSnapshotStore::new(world_root, backup_root)?;
    let manifest = store.create_snapshot(
        SnapshotReason::PreModChange {
            summary: "pre-namespace-preserve".to_string(),
        },
        mc_version,
        timestamp_secs,
    )?;
    store.verify_snapshot(&manifest.snapshot_id)?;

    // 2. Merge into the existing sidecar so entries parked by earlier loads
    //    survive; a document that fails verification stops the whole guard.
    let mut sidecar = if sidecar_path.exists() {
        NamespaceTombstoneSidecar::load_verified(sidecar_path)?
    } else {
        NamespaceTombstoneSidecar::new()
    };

    // 3. Park every unknown entry with the load's timestamp as the stamp.
    let mut preserved_counts_by_namespace = BTreeMap::new();
    for entry in discovered {
        if sidecar
            .preserve_unknown(entry.clone(), timestamp_secs, active_namespaces)?
            .is_some()
        {
            if let Some(ns) = namespace_of(&entry.original_id) {
                *preserved_counts_by_namespace.entry(ns.to_string()).or_insert(0) += 1;
            }
        }
    }

    // 4. Persist — atomic, so a crash leaves either the old or the new
    //    sidecar, never a torn one.
    sidecar.save_atomic(sidecar_path)?;

    Ok(NamespaceGuardReport {
        snapshot_id: manifest.snapshot_id,
        preserved_counts_by_namespace,
        safe_to_proceed: true,
    })
}
