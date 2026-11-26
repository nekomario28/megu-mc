// SPDX-License-Identifier: LGPL-3.0-or-later

//! Universal mod loot-table overlay and NeoForge/Forge global loot modifier
//! engine.
//!
//! Everything in this module operates on in-memory datapack/JAR members. It
//! never performs file-system or network I/O, never invokes a JVM, and never
//! seeds randomness from wall-clock time. Malformed bytes fail the ingest
//! closed; unsupported modifier types or conditions simply never fire and are
//! surfaced through diagnostics instead of being treated as permissive wildcards.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use crate::data_loader::{parse_json, DataLoadError, JsonValue};
use crate::provenance_diff::{DiffDomain, ProvenanceAction, ProvenanceDiffLedger};

/// Maximum accepted bytes for one loot-table or loot-modifier JSON document.
pub const MAX_LOOT_JSON_BYTES: usize = 2 * 1024 * 1024;
/// Maximum number of loot tables accepted from one layer.
pub const MAX_LOOT_TABLES_PER_MOD: usize = 4_096;
/// Maximum recursive `add_table` depth. Depth eight is generous for real
/// mods while still making cyclic table graphs fail closed.
pub const MAX_ADD_TABLE_DEPTH: usize = 8;
/// Maximum number of modifier JSON documents accepted from one layer.
pub const MAX_LOOT_MODIFIERS_PER_MOD: usize = 4_096;

const NEOFORGE_GLOBAL_LIST: &str = "data/neoforge/loot_modifiers/global_loot_modifiers.json";
const FORGE_GLOBAL_LIST: &str = "data/forge/loot_modifiers/global_loot_modifiers.json";

/// In-memory member of a mod or datapack layer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LootDataEntry {
    pub path: String,
    pub bytes: Vec<u8>,
}

impl LootDataEntry {
    pub fn new(path: impl Into<String>, bytes: impl Into<Vec<u8>>) -> Self {
        Self {
            path: path.into(),
            bytes: bytes.into(),
        }
    }
}

/// Layer priority class. Vanilla is lowest, datapacks highest; mods retain
/// their input load order within the middle band.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum LootLayerKind {
    Vanilla,
    Mod,
    Datapack,
}

/// One ordered in-memory mod/datapack layer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LootDataLayer {
    pub name: String,
    pub kind: LootLayerKind,
    pub entries: Vec<LootDataEntry>,
}

impl LootDataLayer {
    pub fn new(name: impl Into<String>, kind: LootLayerKind, entries: Vec<LootDataEntry>) -> Self {
        Self {
            name: name.into(),
            kind,
            entries,
        }
    }
}

/// Error produced by fail-closed loot ingest.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LootEngineError {
    InvalidPath(String),
    Json(String),
    TooLarge { path: String, bytes: usize },
    TooManyTables { layer: String, count: usize },
    TooManyModifiers { layer: String, count: usize },
    Malformed(String),
}

impl fmt::Display for LootEngineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidPath(path) => write!(f, "invalid loot data path '{path}'"),
            Self::Json(msg) | Self::Malformed(msg) => f.write_str(msg),
            Self::TooLarge { path, bytes } => {
                write!(
                    f,
                    "'{path}' is {bytes} bytes, over the 2 MiB loot JSON bound"
                )
            }
            Self::TooManyTables { layer, count } => {
                write!(
                    f,
                    "layer '{layer}' has {count} loot tables, over the 4096 bound"
                )
            }
            Self::TooManyModifiers { layer, count } => {
                write!(
                    f,
                    "layer '{layer}' has {count} loot modifiers, over the 4096 bound"
                )
            }
        }
    }
}

impl std::error::Error for LootEngineError {}

impl From<DataLoadError> for LootEngineError {
    fn from(value: DataLoadError) -> Self {
        match value {
            DataLoadError::Io(msg) | DataLoadError::Json(msg) | DataLoadError::Zip(msg) => {
                Self::Json(msg)
            }
        }
    }
}

fn malformed(message: impl Into<String>) -> LootEngineError {
    LootEngineError::Malformed(message.into())
}

fn normalize_id(raw: &str) -> String {
    let raw = raw.trim();
    if raw.contains(':') {
        raw.to_string()
    } else {
        format!("minecraft:{raw}")
    }
}

fn type_path(type_name: &str) -> &str {
    type_name.rsplit(':').next().unwrap_or(type_name)
}

fn valid_resource_part(part: &str, allow_slash: bool) -> bool {
    !part.is_empty()
        && part.split('/').all(|segment| {
            !segment.is_empty()
                && segment != "."
                && segment != ".."
                && segment.bytes().all(|b| {
                    b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'_' | b'-' | b'.')
                })
        })
        && (allow_slash || !part.contains('/'))
}

fn classify_loot_table_path(path: &str) -> Result<Option<(String, String)>, LootEngineError> {
    if path.contains('\\') {
        return Err(LootEngineError::InvalidPath(path.to_string()));
    }
    let parts: Vec<&str> = path.split('/').collect();
    if parts.len() < 4 || parts[0] != "data" {
        return Ok(None);
    }
    if parts[2] != "loot_table" && parts[2] != "loot_tables" {
        return Ok(None);
    }
    let namespace = parts[1];
    let stem_with_ext = parts[3..].join("/");
    let Some(stem) = stem_with_ext.strip_suffix(".json") else {
        return Ok(None);
    };
    if !valid_resource_part(namespace, false) || !valid_resource_part(stem, true) {
        return Err(LootEngineError::InvalidPath(path.to_string()));
    }
    Ok(Some((namespace.to_string(), stem.to_string())))
}

fn classify_modifier_path(path: &str) -> Option<(String, String)> {
    if path.contains('\\') {
        return None;
    }
    let parts: Vec<&str> = path.split('/').collect();
    if parts.len() < 4 || parts[0] != "data" {
        return None;
    }
    if parts[2] != "loot_modifier" && parts[2] != "loot_modifiers" {
        return None;
    }
    let namespace = parts[1];
    let stem_with_ext = parts[3..].join("/");
    let stem = stem_with_ext.strip_suffix(".json")?;
    if stem.is_empty()
        || ((namespace == "forge" || namespace == "neoforge") && stem == "global_loot_modifiers")
        || !valid_resource_part(namespace, false)
        || !valid_resource_part(stem, true)
    {
        return None;
    }
    Some((namespace.to_string(), stem.to_string()))
}

fn ordered_layer_indices(layers: &[LootDataLayer]) -> Vec<usize> {
    fn rank(kind: LootLayerKind) -> usize {
        match kind {
            LootLayerKind::Vanilla => 0,
            LootLayerKind::Mod => 1,
            LootLayerKind::Datapack => 2,
        }
    }
    let mut indices: Vec<usize> = (0..layers.len()).collect();
    indices.sort_by_key(|&index| rank(layers[index].kind));
    indices
}

fn checked_json_bytes(path: &str, bytes: &[u8]) -> Result<(), LootEngineError> {
    if bytes.len() > MAX_LOOT_JSON_BYTES {
        return Err(LootEngineError::TooLarge {
            path: path.to_string(),
            bytes: bytes.len(),
        });
    }
    Ok(())
}

/// Winning loot-table JSON for one canonical loot-table id.
#[derive(Clone, Debug)]
pub struct LootTableOverlayEntry {
    pub id: String,
    pub path: String,
    pub source_layer: String,
    pub json: JsonValue,
    pub previous_owner: Option<String>,
}

/// Deterministic Vanilla → mods → datapacks loot-table overlay index.
#[derive(Clone, Debug, Default)]
pub struct ModLootOverlayIndex {
    tables: BTreeMap<String, LootTableOverlayEntry>,
    provenance: ProvenanceDiffLedger,
    diagnostics: Vec<String>,
}

impl ModLootOverlayIndex {
    pub fn new() -> Self {
        Self::default()
    }

    /// Build an index from layers. Input order inside the mod band is the
    /// mod load order; vanilla entries are always evaluated first and
    /// datapacks last, regardless of where the caller placed those bands.
    pub fn ingest_layers(layers: &[LootDataLayer]) -> Result<Self, LootEngineError> {
        let mut index = Self::new();
        for layer_index in ordered_layer_indices(layers) {
            index.ingest_layer(&layers[layer_index])?;
        }
        Ok(index)
    }

    /// Ingest one additional layer at its current point in the load sequence.
    /// The whole layer is validated before the first visible mutation, so a
    /// malformed member cannot leave a half-ingested index behind.
    pub fn ingest_layer(&mut self, layer: &LootDataLayer) -> Result<(), LootEngineError> {
        let mut sorted_entries: Vec<&LootDataEntry> = layer.entries.iter().collect();
        sorted_entries.sort_by(|a, b| a.path.cmp(&b.path));
        let mut table_members = Vec::new();
        for entry in sorted_entries {
            let Some((namespace, stem)) = classify_loot_table_path(&entry.path)? else {
                continue;
            };
            table_members.push((entry, format!("{namespace}:{stem}")));
        }
        if table_members.len() > MAX_LOOT_TABLES_PER_MOD {
            return Err(LootEngineError::TooManyTables {
                layer: layer.name.clone(),
                count: table_members.len(),
            });
        }

        let mut parsed_tables = Vec::new();
        for (entry, id) in table_members {
            checked_json_bytes(&entry.path, &entry.bytes)?;
            let json = parse_json(&entry.bytes)
                .map_err(|error| LootEngineError::Json(format!("{}: {error:?}", entry.path)))?;
            parsed_tables.push((entry, id, json));
        }

        for (entry, id, json) in parsed_tables {
            let previous_owner = self
                .tables
                .get(&id)
                .map(|existing| existing.source_layer.clone());
            let action = if previous_owner.is_some() {
                ProvenanceAction::Overridden
            } else {
                ProvenanceAction::Added
            };
            let summary = match action {
                ProvenanceAction::Added => format!("{} adds {}", layer.name, entry.path),
                _ => format!("{} overrides loot table with {}", layer.name, entry.path),
            };
            self.provenance.record_action(
                DiffDomain::LootTable,
                id.clone(),
                layer.name.clone(),
                action,
                summary,
                previous_owner.clone(),
            );
            if previous_owner.is_some() {
                self.diagnostics.push(format!(
                    "loot table {id} overridden by '{}' (previous owner: {})",
                    layer.name,
                    previous_owner
                        .clone()
                        .unwrap_or_else(|| "unknown".to_string())
                ));
            }
            self.tables.insert(
                id.clone(),
                LootTableOverlayEntry {
                    id,
                    path: entry.path.clone(),
                    source_layer: layer.name.clone(),
                    json,
                    previous_owner,
                },
            );
        }
        Ok(())
    }

    pub fn table(&self, id: &str) -> Option<&LootTableOverlayEntry> {
        self.tables.get(&normalize_id(id))
    }

    pub fn tables(&self) -> &BTreeMap<String, LootTableOverlayEntry> {
        &self.tables
    }

    pub fn len(&self) -> usize {
        self.tables.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tables.is_empty()
    }

    pub fn provenance(&self) -> &ProvenanceDiffLedger {
        &self.provenance
    }

    pub fn diagnostics(&self) -> &[String] {
        &self.diagnostics
    }
}

/// A rolled item stack mutated by the loot-modifier engine.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LootDropItem {
    pub item: String,
    pub count: u32,
}

impl LootDropItem {
    pub fn new(item: impl Into<String>, count: u32) -> Self {
        Self {
            item: normalize_id(&item.into()),
            count,
        }
    }
}

/// Deterministic count roll.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LootCount {
    Constant(u32),
    Uniform { min: u32, max: u32 },
}

impl LootCount {
    fn roll(self, rng: &mut SplitMix64) -> u32 {
        match self {
            Self::Constant(count) => count,
            Self::Uniform { min, max } => {
                if max <= min {
                    min
                } else {
                    let span = u64::from(max) - u64::from(min) + 1;
                    min + (rng.next_u64() % span) as u32
                }
            }
        }
    }
}

/// Deterministic splitmix64 generator used for every chance and count roll.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SplitMix64 {
    state: u64,
}

impl SplitMix64 {
    pub fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    pub fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform `[0, 1)` double with 53 bits of entropy.
    pub fn next_f64(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }

    /// Uniform `0..bound`; `bound == 0` yields zero rather than panicking.
    pub fn next_bounded_u32(&mut self, bound: u32) -> u32 {
        if bound == 0 {
            return 0;
        }
        (self.next_u64() % u64::from(bound)) as u32
    }
}

/// Item or tag matcher used by `match_tool`, `replace_item`, and
/// `remove_item`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ItemMatcher {
    Item(String),
    Tag(String),
    AnyOf(Vec<ItemMatcher>),
}

/// Runtime context supplied by the caller for one loot roll.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct LootModifierContext {
    pub held_tool: Option<String>,
    pub item_tags: BTreeMap<String, BTreeSet<String>>,
    pub killed_by_player: bool,
    /// Enchantment level used by `random_chance_with_enchanted_bonus`.
    pub enchanted_bonus_level: u32,
}

impl ItemMatcher {
    fn matches(&self, item: &str, tags: &BTreeMap<String, BTreeSet<String>>) -> bool {
        let item = normalize_id(item);
        match self {
            Self::Item(expected) => *expected == item,
            Self::Tag(tag) => tag_contains(tags, tag, &item, 0),
            Self::AnyOf(matchers) => matchers.iter().any(|matcher| matcher.matches(&item, tags)),
        }
    }
}

fn tag_contains(
    tags: &BTreeMap<String, BTreeSet<String>>,
    tag: &str,
    item: &str,
    depth: usize,
) -> bool {
    if depth > 16 {
        return false;
    }
    let Some(members) = tags.get(tag.strip_prefix('#').unwrap_or(tag)) else {
        return false;
    };
    for member in members {
        if let Some(nested) = member.strip_prefix('#') {
            if tag_contains(tags, &normalize_id(nested), item, depth + 1) {
                return true;
            }
        } else if normalize_id(member) == item {
            return true;
        }
    }
    false
}

/// Recursive loot condition tree. Unknown conditions are retained so they
/// evaluate to false explicitly — never as a permissive wildcard.
#[derive(Clone, Debug, PartialEq)]
pub enum LootCondition {
    LootTableId(String),
    RandomChance(f64),
    RandomChanceWithEnchantedBonus {
        base_chance: f64,
        bonus_per_level: f64,
        /// Vanilla linear-provider base chance at enchantment level one.
        /// `None` selects the simplified continuous `base + level * bonus`
        /// form used by some NeoForge modifier JSONs.
        enchanted_base_chance: Option<f64>,
    },
    MatchTool(ItemMatcher),
    AnyOf(Vec<LootCondition>),
    AllOf(Vec<LootCondition>),
    Inverted(Box<LootCondition>),
    KilledByPlayer,
    Unknown(String),
}

impl LootCondition {
    fn evaluate(
        &self,
        loot_table_id: &str,
        context: &LootModifierContext,
        rng: &mut SplitMix64,
    ) -> bool {
        match self {
            Self::LootTableId(pattern) => loot_table_matches(pattern, loot_table_id),
            Self::RandomChance(chance) => rng.next_f64() < chance.clamp(0.0, 1.0),
            Self::RandomChanceWithEnchantedBonus {
                base_chance,
                bonus_per_level,
                enchanted_base_chance,
            } => {
                let level = context.enchanted_bonus_level;
                let chance = match (enchanted_base_chance, level) {
                    (Some(_), 0) => *base_chance,
                    (Some(enchanted_base), level) => {
                        enchanted_base + bonus_per_level * f64::from(level.saturating_sub(1))
                    }
                    (None, _) => base_chance + bonus_per_level * f64::from(level),
                };
                rng.next_f64() < chance.clamp(0.0, 1.0)
            }
            Self::MatchTool(matcher) => context
                .held_tool
                .as_deref()
                .is_some_and(|tool| matcher.matches(tool, &context.item_tags)),
            Self::AnyOf(terms) => terms
                .iter()
                .any(|term| term.evaluate(loot_table_id, context, rng)),
            Self::AllOf(terms) => terms
                .iter()
                .all(|term| term.evaluate(loot_table_id, context, rng)),
            Self::Inverted(term) => !term.evaluate(loot_table_id, context, rng),
            Self::KilledByPlayer => context.killed_by_player,
            Self::Unknown(_) => false,
        }
    }
}

fn loot_table_matches(pattern: &str, loot_table_id: &str) -> bool {
    if pattern.trim() == "*" {
        return true;
    }
    let pattern = normalize_id(pattern);
    let loot_table_id = normalize_id(loot_table_id);
    if let Some(prefix) = pattern.strip_suffix('*') {
        return loot_table_id.starts_with(prefix);
    }
    pattern == loot_table_id
}

/// Action performed by one global loot modifier.
#[derive(Clone, Debug, PartialEq)]
pub enum LootModifierAction {
    AddItem {
        item: String,
        count: LootCount,
        chance: Option<f64>,
    },
    AddTable {
        table: String,
    },
    ReplaceItem {
        target: ItemMatcher,
        item: String,
        count: Option<LootCount>,
        chance: Option<f64>,
    },
    RemoveItem {
        target: ItemMatcher,
    },
    Unknown {
        type_name: String,
    },
}

/// One parsed modifier JSON document.
#[derive(Clone, Debug, PartialEq)]
pub struct LootModifier {
    pub id: String,
    pub source_layer: String,
    pub type_name: String,
    pub conditions: Vec<LootCondition>,
    pub action: LootModifierAction,
}

/// Deterministic report for one modifier application pass.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LootModifierApplyReport {
    pub fired_modifiers: Vec<String>,
    pub condition_failed_modifiers: Vec<String>,
    pub chance_failed_modifiers: Vec<String>,
    pub fail_closed_modifiers: Vec<String>,
    pub added_drops: usize,
    pub replaced_drops: usize,
    pub removed_drops: usize,
    pub diagnostics: Vec<String>,
}

impl LootModifierApplyReport {
    fn diagnostic(&mut self, message: impl Into<String>) {
        self.diagnostics.push(message.into());
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum RegistrationKind {
    Explicit,
    Fallback,
}

struct ApplyState<'a, F>
where
    F: FnMut(&str) -> Vec<LootDropItem>,
{
    context: &'a LootModifierContext,
    table_resolver: &'a mut F,
    rng: &'a mut SplitMix64,
    visiting: &'a mut BTreeSet<String>,
    report: &'a mut LootModifierApplyReport,
}

/// Parsed and activation-ordered global loot modifier engine.
#[derive(Clone, Debug, Default)]
pub struct GlobalLootModifierEngine {
    modifiers: BTreeMap<String, LootModifier>,
    active_order: Vec<String>,
    fallback_order: Vec<String>,
    diagnostics: Vec<String>,
    provenance: ProvenanceDiffLedger,
}

impl GlobalLootModifierEngine {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn ingest_layers(layers: &[LootDataLayer]) -> Result<Self, LootEngineError> {
        let mut engine = Self::new();
        engine.ingest_ordered_layers(layers)?;
        Ok(engine)
    }

    /// Ingest modifier definitions and global activation lists. The method
    /// is intentionally two-phase inside one call: definitions are parsed
    /// before activation so a list can reference a modifier file anywhere in
    /// the same deterministic layer set.
    pub fn ingest_ordered_layers(
        &mut self,
        layers: &[LootDataLayer],
    ) -> Result<(), LootEngineError> {
        let mut working = self.clone();
        working.ingest_ordered_layers_inner(layers)?;
        *self = working;
        Ok(())
    }

    fn ingest_ordered_layers_inner(
        &mut self,
        layers: &[LootDataLayer],
    ) -> Result<(), LootEngineError> {
        let ordered = ordered_layer_indices(layers);
        let mut scans = Vec::new();
        for layer_index in ordered {
            scans.push(scan_layer_for_modifiers(&layers[layer_index])?);
        }

        // Definitions first. Later layers can override an earlier modifier
        // id exactly like any other datapack resource.
        for scan in &scans {
            for file in &scan.modifier_files {
                checked_json_bytes(file.path, file.bytes)?;
                let (modifier, mut diagnostics) =
                    parse_modifier_file(&file.id, scan.layer_name, file.bytes, file.path)?;
                self.diagnostics.append(&mut diagnostics);
                let previous_owner = self
                    .modifiers
                    .get(&modifier.id)
                    .map(|existing| existing.source_layer.clone());
                let action = if previous_owner.is_some() {
                    ProvenanceAction::Overridden
                } else {
                    ProvenanceAction::Added
                };
                self.provenance.record_action(
                    DiffDomain::LootModifier,
                    modifier.id.clone(),
                    scan.layer_name,
                    action,
                    format!("{} defines {}", scan.layer_name, modifier.id),
                    previous_owner,
                );
                self.modifiers.insert(modifier.id.clone(), modifier);
            }
        }

        // Then activation. Fallback discovery is represented in the layer
        // sequence so `replace: true` can clear it, but is stable-partitioned
        // behind explicitly listed entries in the final application order.
        let mut registrations: Vec<(String, RegistrationKind)> = Vec::new();
        for scan in &scans {
            let mut parsed_lists = Vec::new();
            for global in &scan.global_lists {
                checked_json_bytes(&global.path, &global.bytes)?;
                parsed_lists.push(parse_global_modifier_list(&global.bytes, &global.path)?);
            }
            if parsed_lists.iter().any(|list| list.replace) {
                registrations.clear();
            }
            for list in parsed_lists {
                for id in list.entries {
                    upsert_registration(&mut registrations, id, RegistrationKind::Explicit);
                }
            }
            if scan.global_lists.is_empty() && !scan.modifier_files.is_empty() {
                for file in &scan.modifier_files {
                    self.diagnostics.push(format!(
                        "fallback discovery: '{}' ships {} without global_loot_modifiers.json; registering {} at lower priority",
                        scan.layer_name, file.path, file.id
                    ));
                    upsert_registration(
                        &mut registrations,
                        file.id.clone(),
                        RegistrationKind::Fallback,
                    );
                }
            }
        }

        let mut explicit = Vec::new();
        let mut fallback = Vec::new();
        for (id, kind) in registrations {
            if !self.modifiers.contains_key(&id) {
                self.diagnostics.push(format!(
                    "global loot modifier '{id}' is listed but no modifier JSON was loaded; skipping it fail-closed"
                ));
                continue;
            }
            match kind {
                RegistrationKind::Explicit => explicit.push(id),
                RegistrationKind::Fallback => fallback.push(id),
            }
        }
        self.active_order = explicit;
        self.fallback_order = fallback;
        self.active_order
            .extend(self.fallback_order.iter().cloned());
        Ok(())
    }

    pub fn modifier(&self, id: &str) -> Option<&LootModifier> {
        self.modifiers.get(&normalize_id(id))
    }

    pub fn modifiers(&self) -> &BTreeMap<String, LootModifier> {
        &self.modifiers
    }

    pub fn active_order(&self) -> &[String] {
        &self.active_order
    }

    pub fn fallback_order(&self) -> &[String] {
        &self.fallback_order
    }

    pub fn diagnostics(&self) -> &[String] {
        &self.diagnostics
    }

    pub fn provenance(&self) -> &ProvenanceDiffLedger {
        &self.provenance
    }

    /// Apply every active modifier that targets `loot_table_id` to `drops`.
    ///
    /// `table_resolver` is the host's mod+vanilla loot-table callback: given
    /// a canonical table id it returns that table's base rolled drops. Nested
    /// `add_table` calls recursively apply GLMs to the resolved table under
    /// an eight-deep, cycle-guarded traversal. `seed` fully determines every
    /// chance and count roll.
    pub fn apply_loot_modifiers<F>(
        &mut self,
        loot_table_id: &str,
        context: &LootModifierContext,
        drops: &mut Vec<LootDropItem>,
        mut table_resolver: F,
        seed: u64,
    ) -> LootModifierApplyReport
    where
        F: FnMut(&str) -> Vec<LootDropItem>,
    {
        let normalized_id = normalize_id(loot_table_id);
        let mut report = LootModifierApplyReport::default();
        let mut rng = SplitMix64::new(seed);
        let mut visiting = BTreeSet::new();
        visiting.insert(normalized_id.clone());
        let mut state = ApplyState {
            context,
            table_resolver: &mut table_resolver,
            rng: &mut rng,
            visiting: &mut visiting,
            report: &mut report,
        };
        self.apply_inner(&normalized_id, drops, 0, &mut state);
        report
    }

    fn apply_inner<F>(
        &mut self,
        loot_table_id: &str,
        drops: &mut Vec<LootDropItem>,
        depth: usize,
        state: &mut ApplyState<'_, F>,
    ) where
        F: FnMut(&str) -> Vec<LootDropItem>,
    {
        for index in 0..self.active_order.len() {
            let Some(modifier) = self
                .active_order
                .get(index)
                .and_then(|modifier_id| self.modifiers.get(modifier_id))
                .cloned()
            else {
                continue;
            };
            if matches!(modifier.action, LootModifierAction::Unknown { .. }) {
                state.report.fail_closed_modifiers.push(modifier.id.clone());
                state.report.diagnostic(format!(
                    "modifier '{}' has unsupported type '{}'; not applied",
                    modifier.id, modifier.type_name
                ));
                continue;
            }

            let mut conditions_pass = true;
            for condition in &modifier.conditions {
                if !condition.evaluate(loot_table_id, state.context, state.rng) {
                    conditions_pass = false;
                    break;
                }
            }
            if !conditions_pass {
                state
                    .report
                    .condition_failed_modifiers
                    .push(modifier.id.clone());
                continue;
            }

            if !action_chance_passes(&modifier.action, state.rng) {
                state
                    .report
                    .chance_failed_modifiers
                    .push(modifier.id.clone());
                continue;
            }

            let mut fired = true;
            match &modifier.action {
                LootModifierAction::AddItem { item, count, .. } => {
                    let rolled = count.roll(state.rng);
                    if rolled > 0 {
                        drops.push(LootDropItem::new(item.clone(), rolled));
                        state.report.added_drops += 1;
                    }
                }
                LootModifierAction::AddTable { table } => {
                    let table = normalize_id(table);
                    if depth >= MAX_ADD_TABLE_DEPTH || state.visiting.contains(&table) {
                        fired = false;
                        state.report.fail_closed_modifiers.push(modifier.id.clone());
                        state.report.diagnostic(format!(
                            "modifier '{}' hit the add_table depth/cycle guard targeting '{}'; not applied",
                            modifier.id, table
                        ));
                    } else {
                        let mut nested = (state.table_resolver)(&table);
                        state.visiting.insert(table.clone());
                        self.apply_inner(&table, &mut nested, depth + 1, state);
                        state.visiting.remove(&table);
                        state.report.added_drops += nested.len();
                        drops.append(&mut nested);
                    }
                }
                LootModifierAction::ReplaceItem {
                    target,
                    item,
                    count,
                    ..
                } => {
                    let replacement_count = count.map(|count| count.roll(state.rng));
                    for drop in drops.iter_mut() {
                        if target.matches(&drop.item, &state.context.item_tags) {
                            drop.item = normalize_id(item);
                            if let Some(count) = replacement_count {
                                drop.count = count;
                            }
                            state.report.replaced_drops += 1;
                        }
                    }
                }
                LootModifierAction::RemoveItem { target } => {
                    let before = drops.len();
                    drops.retain(|drop| !target.matches(&drop.item, &state.context.item_tags));
                    state.report.removed_drops += before - drops.len();
                }
                LootModifierAction::Unknown { .. } => unreachable!(),
            }

            if fired {
                state.report.fired_modifiers.push(modifier.id.clone());
                self.provenance.record_action(
                    DiffDomain::LootTable,
                    loot_table_id.to_string(),
                    modifier.source_layer.clone(),
                    ProvenanceAction::ModifiedByGlm,
                    format!("{} applied to {}", modifier.id, loot_table_id),
                    None,
                );
            }
        }
    }
}

fn upsert_registration(
    registrations: &mut Vec<(String, RegistrationKind)>,
    id: String,
    kind: RegistrationKind,
) {
    let id = normalize_id(&id);
    if let Some((_, existing_kind)) = registrations
        .iter_mut()
        .find(|(existing, _)| *existing == id)
    {
        if kind == RegistrationKind::Explicit {
            *existing_kind = RegistrationKind::Explicit;
        }
    } else {
        registrations.push((id, kind));
    }
}

fn action_chance_passes(action: &LootModifierAction, rng: &mut SplitMix64) -> bool {
    let chance = match action {
        LootModifierAction::AddItem { chance, .. } => *chance,
        LootModifierAction::ReplaceItem { chance, .. } => *chance,
        _ => None,
    };
    chance.is_none_or(|chance| rng.next_f64() < chance.clamp(0.0, 1.0))
}

struct LayerModifierScan<'a> {
    layer_name: &'a str,
    modifier_files: Vec<ModifierFile<'a>>,
    global_lists: Vec<&'a LootDataEntry>,
}

struct ModifierFile<'a> {
    id: String,
    path: &'a str,
    bytes: &'a [u8],
}

fn scan_layer_for_modifiers(
    layer: &LootDataLayer,
) -> Result<LayerModifierScan<'_>, LootEngineError> {
    let mut sorted_entries: Vec<&LootDataEntry> = layer.entries.iter().collect();
    sorted_entries.sort_by(|a, b| a.path.cmp(&b.path));
    let mut modifier_files = Vec::new();
    let mut global_lists = Vec::new();
    for entry in sorted_entries {
        if entry.path == NEOFORGE_GLOBAL_LIST || entry.path == FORGE_GLOBAL_LIST {
            global_lists.push(entry);
            continue;
        }
        if let Some((namespace, stem)) = classify_modifier_path(&entry.path) {
            modifier_files.push(ModifierFile {
                id: format!("{namespace}:{stem}"),
                path: entry.path.as_str(),
                bytes: entry.bytes.as_slice(),
            });
        }
    }
    if modifier_files.len() > MAX_LOOT_MODIFIERS_PER_MOD {
        return Err(LootEngineError::TooManyModifiers {
            layer: layer.name.clone(),
            count: modifier_files.len(),
        });
    }
    Ok(LayerModifierScan {
        layer_name: layer.name.as_str(),
        modifier_files,
        global_lists,
    })
}

struct GlobalModifierList {
    replace: bool,
    entries: Vec<String>,
}

fn parse_global_modifier_list(
    bytes: &[u8],
    path: &str,
) -> Result<GlobalModifierList, LootEngineError> {
    let root =
        parse_json(bytes).map_err(|error| LootEngineError::Json(format!("{path}: {error:?}")))?;
    let object = root.as_object().ok_or_else(|| {
        malformed(format!(
            "{path}: global loot modifier list must be an object"
        ))
    })?;
    let replace = match object.get("replace") {
        Some(value) => value
            .as_bool()
            .ok_or_else(|| malformed(format!("{path}: 'replace' must be a boolean")))?,
        None => false,
    };
    let mut entries = Vec::new();
    if let Some(entries_value) = object.get("entries") {
        let Some(array) = entries_value.as_array() else {
            return Err(malformed(format!("{path}: 'entries' must be an array")));
        };
        for value in array {
            let Some(id) = value.as_str() else {
                return Err(malformed(format!("{path}: entries must be strings")));
            };
            let id = normalize_id(id);
            if !entries.contains(&id) {
                entries.push(id);
            }
        }
    }
    Ok(GlobalModifierList { replace, entries })
}

fn parse_modifier_file(
    id: &str,
    source_layer: &str,
    bytes: &[u8],
    path: &str,
) -> Result<(LootModifier, Vec<String>), LootEngineError> {
    let root =
        parse_json(bytes).map_err(|error| LootEngineError::Json(format!("{path}: {error:?}")))?;
    let object = root
        .as_object()
        .ok_or_else(|| malformed(format!("{path}: loot modifier must be a JSON object")))?;
    let type_name = object
        .get("type")
        .and_then(JsonValue::as_str)
        .ok_or_else(|| malformed(format!("{path}: loot modifier is missing string 'type'")))?
        .to_string();
    let mut diagnostics = Vec::new();
    let mut conditions = Vec::new();
    if let Some(conditions_value) = object.get("conditions") {
        let Some(array) = conditions_value.as_array() else {
            return Err(malformed(format!(
                "{path}: loot modifier 'conditions' must be an array"
            )));
        };
        for condition in array {
            conditions.push(parse_condition(condition, 0, id, &mut diagnostics));
        }
    }
    let action = parse_modifier_action(&type_name, object, id, &mut diagnostics);
    Ok((
        LootModifier {
            id: normalize_id(id),
            source_layer: source_layer.to_string(),
            type_name,
            conditions,
            action,
        },
        diagnostics,
    ))
}

fn parse_modifier_action(
    type_name: &str,
    object: &BTreeMap<String, JsonValue>,
    modifier_id: &str,
    diagnostics: &mut Vec<String>,
) -> LootModifierAction {
    match type_path(type_name) {
        "add_item" => {
            let Some(item) = object.get("item").and_then(JsonValue::as_str) else {
                diagnostics.push(format!(
                    "modifier '{modifier_id}' add_item is missing 'item'; disabled fail-closed"
                ));
                return LootModifierAction::Unknown {
                    type_name: type_name.to_string(),
                };
            };
            let count = match object.get("count") {
                Some(value) => match parse_count(value) {
                    Some(count) => count,
                    None => {
                        diagnostics.push(format!(
                            "modifier '{modifier_id}' add_item has malformed 'count'; disabled fail-closed"
                        ));
                        return LootModifierAction::Unknown {
                            type_name: type_name.to_string(),
                        };
                    }
                },
                None => LootCount::Constant(1),
            };
            let chance = match object.get("chance") {
                Some(value) => match value.as_f64() {
                    Some(chance) => Some(chance),
                    None => {
                        diagnostics.push(format!(
                            "modifier '{modifier_id}' add_item has malformed 'chance'; disabled fail-closed"
                        ));
                        return LootModifierAction::Unknown {
                            type_name: type_name.to_string(),
                        };
                    }
                },
                None => None,
            };
            LootModifierAction::AddItem {
                item: normalize_id(item),
                count,
                chance,
            }
        }
        "add_table" => {
            let Some(table) = object.get("table").and_then(JsonValue::as_str) else {
                diagnostics.push(format!(
                    "modifier '{modifier_id}' add_table is missing 'table'; disabled fail-closed"
                ));
                return LootModifierAction::Unknown {
                    type_name: type_name.to_string(),
                };
            };
            LootModifierAction::AddTable {
                table: normalize_id(table),
            }
        }
        "replace_item" => {
            let target_value = object.get("replace").or_else(|| object.get("target_item"));
            let target = target_value.and_then(parse_item_matcher);
            let item = object.get("item").and_then(JsonValue::as_str);
            let (Some(target), Some(item)) = (target, item) else {
                diagnostics.push(format!(
                    "modifier '{modifier_id}' replace_item is missing target/item; disabled fail-closed"
                ));
                return LootModifierAction::Unknown {
                    type_name: type_name.to_string(),
                };
            };
            let count = match object.get("count") {
                Some(value) => match parse_count(value) {
                    Some(count) => Some(count),
                    None => {
                        diagnostics.push(format!(
                            "modifier '{modifier_id}' replace_item has malformed 'count'; disabled fail-closed"
                        ));
                        return LootModifierAction::Unknown {
                            type_name: type_name.to_string(),
                        };
                    }
                },
                None => None,
            };
            let chance = match object.get("chance") {
                Some(value) => match value.as_f64() {
                    Some(chance) => Some(chance),
                    None => {
                        diagnostics.push(format!(
                            "modifier '{modifier_id}' replace_item has malformed 'chance'; disabled fail-closed"
                        ));
                        return LootModifierAction::Unknown {
                            type_name: type_name.to_string(),
                        };
                    }
                },
                None => None,
            };
            LootModifierAction::ReplaceItem {
                target,
                item: normalize_id(item),
                count,
                chance,
            }
        }
        "remove_item" => {
            let target_value = object
                .get("items")
                .or_else(|| object.get("item"))
                .or_else(|| object.get("target_item"));
            let Some(target) = target_value.and_then(parse_item_matcher) else {
                diagnostics.push(format!(
                    "modifier '{modifier_id}' remove_item is missing item/items; disabled fail-closed"
                ));
                return LootModifierAction::Unknown {
                    type_name: type_name.to_string(),
                };
            };
            LootModifierAction::RemoveItem { target }
        }
        _ => {
            diagnostics.push(format!(
                "modifier '{modifier_id}' uses unsupported type '{type_name}'; disabled fail-closed"
            ));
            LootModifierAction::Unknown {
                type_name: type_name.to_string(),
            }
        }
    }
}

fn parse_count(value: &JsonValue) -> Option<LootCount> {
    match value {
        JsonValue::Number(number) if number.is_finite() && *number >= 0.0 => {
            Some(LootCount::Constant(*number as u32))
        }
        JsonValue::Object(object) => {
            if let Some(constant) = object
                .get("constant")
                .or_else(|| object.get("value"))
                .or_else(|| object.get("count"))
                .and_then(JsonValue::as_u64)
            {
                return Some(LootCount::Constant(u32::try_from(constant).ok()?));
            }
            let min = object.get("min").and_then(JsonValue::as_u64)?;
            let max = object.get("max").and_then(JsonValue::as_u64)?;
            Some(LootCount::Uniform {
                min: u32::try_from(min).ok()?,
                max: u32::try_from(max).ok()?,
            })
        }
        _ => None,
    }
}

fn parse_item_matcher(value: &JsonValue) -> Option<ItemMatcher> {
    match value {
        JsonValue::String(raw) => {
            let raw = raw.trim();
            if let Some(tag) = raw.strip_prefix('#') {
                Some(ItemMatcher::Tag(normalize_id(tag)))
            } else {
                Some(ItemMatcher::Item(normalize_id(raw)))
            }
        }
        JsonValue::Array(values) => {
            let matchers: Vec<ItemMatcher> = values
                .iter()
                .map(parse_item_matcher)
                .collect::<Option<_>>()?;
            (!matchers.is_empty()).then_some(ItemMatcher::AnyOf(matchers))
        }
        JsonValue::Object(object) => {
            if let Some(tag) = object.get("tag").and_then(JsonValue::as_str) {
                return Some(ItemMatcher::Tag(normalize_id(tag.trim_start_matches('#'))));
            }
            if let Some(item) = object
                .get("item")
                .or_else(|| object.get("id"))
                .and_then(JsonValue::as_str)
            {
                return Some(ItemMatcher::Item(normalize_id(item)));
            }
            if let Some(items) = object.get("items") {
                return parse_item_matcher(items);
            }
            None
        }
        _ => None,
    }
}

fn parse_condition(
    value: &JsonValue,
    depth: usize,
    modifier_id: &str,
    diagnostics: &mut Vec<String>,
) -> LootCondition {
    if depth > 16 {
        diagnostics.push(format!(
            "modifier '{modifier_id}' has conditions nested deeper than 16; disabled fail-closed"
        ));
        return LootCondition::Unknown("condition_depth_limit".to_string());
    }
    let Some(object) = value.as_object() else {
        diagnostics.push(format!(
            "modifier '{modifier_id}' has a non-object condition; disabled fail-closed"
        ));
        return LootCondition::Unknown("non_object_condition".to_string());
    };
    let type_name = object
        .get("condition")
        .or_else(|| object.get("type"))
        .and_then(JsonValue::as_str)
        .unwrap_or("");
    match type_path(type_name) {
        "loot_table_id" | "loot_table" => {
            let id = object
                .get("loot_table_id")
                .or_else(|| object.get("loot_table"))
                .or_else(|| object.get("id"))
                .and_then(JsonValue::as_str);
            id.map(|id| LootCondition::LootTableId(normalize_id(id)))
                .unwrap_or_else(|| unknown_condition(modifier_id, type_name, diagnostics))
        }
        "random_chance" => object
            .get("chance")
            .and_then(JsonValue::as_f64)
            .map(LootCondition::RandomChance)
            .unwrap_or_else(|| unknown_condition(modifier_id, type_name, diagnostics)),
        "random_chance_with_enchanted_bonus" => parse_enchanted_bonus_condition(object)
            .unwrap_or_else(|| unknown_condition(modifier_id, type_name, diagnostics)),
        "match_tool" => parse_match_tool_condition(object)
            .map(LootCondition::MatchTool)
            .unwrap_or_else(|| unknown_condition(modifier_id, type_name, diagnostics)),
        "any_of" => {
            let Some(terms) = object.get("terms").and_then(JsonValue::as_array) else {
                return unknown_condition(modifier_id, type_name, diagnostics);
            };
            LootCondition::AnyOf(
                terms
                    .iter()
                    .map(|term| parse_condition(term, depth + 1, modifier_id, diagnostics))
                    .collect(),
            )
        }
        "all_of" => {
            let Some(terms) = object.get("terms").and_then(JsonValue::as_array) else {
                return unknown_condition(modifier_id, type_name, diagnostics);
            };
            LootCondition::AllOf(
                terms
                    .iter()
                    .map(|term| parse_condition(term, depth + 1, modifier_id, diagnostics))
                    .collect(),
            )
        }
        "inverted" => {
            let Some(term) = object.get("term") else {
                return unknown_condition(modifier_id, type_name, diagnostics);
            };
            LootCondition::Inverted(Box::new(parse_condition(
                term,
                depth + 1,
                modifier_id,
                diagnostics,
            )))
        }
        "killed_by_player" => LootCondition::KilledByPlayer,
        _ => unknown_condition(modifier_id, type_name, diagnostics),
    }
}

fn unknown_condition(
    modifier_id: &str,
    type_name: &str,
    diagnostics: &mut Vec<String>,
) -> LootCondition {
    diagnostics.push(format!(
        "modifier '{modifier_id}' uses unknown or malformed condition '{type_name}'; it will never fire"
    ));
    LootCondition::Unknown(type_name.to_string())
}

fn parse_enchanted_bonus_condition(object: &BTreeMap<String, JsonValue>) -> Option<LootCondition> {
    let base_chance = object
        .get("unenchanted_chance")
        .or_else(|| object.get("base_chance"))
        .or_else(|| object.get("chance"))
        .and_then(JsonValue::as_f64)?;
    if let Some(bonus_per_level) = object
        .get("enchanted_bonus")
        .or_else(|| object.get("bonus_per_level"))
        .or_else(|| object.get("enchanted_bonus_per_level"))
        .and_then(JsonValue::as_f64)
    {
        return Some(LootCondition::RandomChanceWithEnchantedBonus {
            base_chance,
            bonus_per_level,
            enchanted_base_chance: None,
        });
    }

    let enchanted = object.get("enchanted_chance")?;
    let (enchanted_base_chance, bonus_per_level) = match enchanted {
        JsonValue::Number(chance) if chance.is_finite() => (Some(*chance), 0.0),
        JsonValue::Object(levels) => {
            if let Some(kind) = levels.get("type").and_then(JsonValue::as_str) {
                if normalize_id(kind) != "minecraft:linear" {
                    return None;
                }
            }
            let enchanted_base = levels.get("base").and_then(JsonValue::as_f64)?;
            let per_level = levels
                .get("per_level_above_first")
                .or_else(|| levels.get("per_level"))
                .or_else(|| levels.get("bonus"))
                .and_then(JsonValue::as_f64)
                .unwrap_or(0.0);
            (Some(enchanted_base), per_level)
        }
        _ => return None,
    };
    Some(LootCondition::RandomChanceWithEnchantedBonus {
        base_chance,
        bonus_per_level,
        enchanted_base_chance,
    })
}

fn parse_match_tool_condition(object: &BTreeMap<String, JsonValue>) -> Option<ItemMatcher> {
    if let Some(predicate) = object.get("predicate") {
        if let Some(predicate_object) = predicate.as_object() {
            if let Some(tag) = predicate_object.get("tag").and_then(JsonValue::as_str) {
                return Some(ItemMatcher::Tag(normalize_id(tag.trim_start_matches('#'))));
            }
            if let Some(items) = predicate_object
                .get("items")
                .or_else(|| predicate_object.get("item"))
            {
                return parse_item_matcher(match_json_tool_value(items));
            }
        }
        return parse_item_matcher(predicate);
    }
    if let Some(tag) = object.get("tag").and_then(JsonValue::as_str) {
        return Some(ItemMatcher::Tag(normalize_id(tag.trim_start_matches('#'))));
    }
    for key in ["items", "item", "tool"] {
        if let Some(value) = object.get(key) {
            return parse_item_matcher(match_json_tool_value(value));
        }
    }
    None
}

fn match_json_tool_value(value: &JsonValue) -> &JsonValue {
    match value {
        JsonValue::Array(array) if array.len() == 1 => &array[0],
        _ => value,
    }
}

/// One Java bytecode hook marker found in a class member.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct BytecodeLootHook {
    pub class_path: String,
    pub marker: String,
    pub loader_family: String,
}

/// Honest bytecode-hook report: when this is non-empty Megu cannot reproduce
/// the Java lambda's loot mutation from JSON and must say JVM offload is
/// required instead of silently dropping the modification.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BytecodeLootHookReport {
    pub mod_id: String,
    pub hooks_detected: Vec<BytecodeLootHook>,
    pub requires_jvm_offload_for_loot: bool,
}

/// Scan class-entry bytes for Fabric `LootTableEvents` and Forge/NeoForge
/// `LootTableLoadEvent` references. The scan is byte-safe rather than a full
/// class-file parse: constant-pool UTF-8 payloads are part of the member
/// bytes, and malformed/truncated class files simply contribute no decoded
/// structure rather than causing a panic. Non-`.class` entries are ignored.
pub fn detect_bytecode_loot_hooks<P, B>(
    mod_id: impl Into<String>,
    entries: &[(P, B)],
) -> BytecodeLootHookReport
where
    P: AsRef<str>,
    B: AsRef<[u8]>,
{
    const MARKERS: &[(&str, &str)] = &[
        (
            "net/fabricmc/fabric/api/loot/v2/LootTableEvents",
            "fabric_loot_table_events_v2",
        ),
        (
            "net/fabricmc/fabric/api/loot/v3/LootTableEvents",
            "fabric_loot_table_events_v3",
        ),
        ("LootTableLoadEvent", "forge_loot_table_load_event"),
    ];
    let mut hooks = BTreeSet::new();
    for (path, bytes) in entries {
        let path = path.as_ref();
        if !path.ends_with(".class") {
            continue;
        }
        let bytes = bytes.as_ref();
        for (needle, family) in MARKERS {
            if contains_subslice(bytes, needle.as_bytes()) {
                hooks.insert(BytecodeLootHook {
                    class_path: path.to_string(),
                    marker: (*needle).to_string(),
                    loader_family: (*family).to_string(),
                });
            }
        }
    }
    let hooks_detected: Vec<BytecodeLootHook> = hooks.into_iter().collect();
    BytecodeLootHookReport {
        mod_id: mod_id.into(),
        requires_jvm_offload_for_loot: !hooks_detected.is_empty(),
        hooks_detected,
    }
}

fn contains_subslice(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty()
        && haystack.len() >= needle.len()
        && haystack
            .windows(needle.len())
            .any(|window| window == needle)
}
