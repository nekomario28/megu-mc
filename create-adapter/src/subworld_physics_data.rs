// SPDX-License-Identifier: LGPL-3.0-or-later

//! Data-driven rigid-body subworld physics loaded from user-supplied datapacks
//! and mod JARs.
//!
//! The schema is deliberately namespace-neutral and resource-location based:
//!
//! * `data/<ns>/physics_block_properties/*.json` — per-block/per-tag rules
//!   `{ "priority"?, "selector": "ns:block" | "#ns:tag", "properties": {...},
//!   "overrides": { "prop=value": {...} } }`. Property keys may be bare or
//!   namespaced; the local key names are `mass`, `volume`, `friction`,
//!   `restitution`, `fragile`, `floating_material`, `floating_scale`, and
//!   `inertia`.
//! * `data/<ns>/floating_materials/*.json` — `{ "lift_strength",
//!   "transition_speed", "prevent_self_lift", "scale_with_gravity",
//!   "scale_with_pressure", "slow/fast_{vertical,horizontal}_friction" }`.
//! * `data/<ns>/tags/block/*.json` — vanilla tag format (`values` may nest
//!   `#other:tag` references and `{ "id", "required" }` objects, plus a
//!   `remove` list). Tag members feed selectors, mass-category fallbacks, and
//!   airtight/envelope barrier detection.
//!
//! [`SubworldPhysicsData`] merges every loaded archive into one registry.
//! Archives are read through [`megu_core::zip_archive::ZipSliceIndex`], which
//! descends one level into `META-INF/jarjar/*.jar` / `META-INF/jars/*.jar`, so
//! bundled JARs work without unpacking anything to disk.
//!
//! Resolution order inside [`crate::aeronautics::RigidBodySubWorld`] is:
//! ship-local mass overrides, then this registry, then the legacy
//! [`crate::aeronautics::MaterialPolicy`] compatibility table. The registry
//! therefore only changes physics for blocks user data actually covers; until
//! physics data is installed through [`install_global`], subworlds keep their
//! historical behaviour bit-for-bit.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, RwLock};

use megu_core::zip_archive::ZipSliceIndex;
use mod_adapter_core::data_loader::{decode_data_json, JsonValue};

/// Engine default mass when the active registry has no matching rule.
pub const DATA_DEFAULT_MASS: f64 = 1.0;
/// Engine default volume when the active registry has no matching rule.
pub const DATA_DEFAULT_VOLUME: f64 = 1.0;
/// Engine default friction when the active registry has no matching rule.
pub const DATA_DEFAULT_FRICTION: f64 = 1.0;
/// Engine default restitution when the active registry has no matching rule.
pub const DATA_DEFAULT_RESTITUTION: f64 = 0.0;
/// Engine default floating-material scale when the active registry omits it.
pub const DATA_DEFAULT_FLOATING_SCALE: f64 = 1.0;
/// Velocity-independent universal drag coefficient used by the subworld solver.
pub const DATA_UNIVERSAL_DRAG: f64 = 0.09;
/// Buoyancy multiplier applied to enclosed gas volume.
pub const HOT_AIR_STRENGTH: f64 = 1.5;

// ---------------------------------------------------------------------------
// Data model
// ---------------------------------------------------------------------------

/// Which kind of selector a `physics_block_properties` rule carries.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Selector {
    /// Exact `"namespace:path"` block id.
    Id(String),
    /// `"namespace:path"` block tag (the `#` prefix is stripped).
    Tag(String),
}

/// One conditional override inside a physics rule: every `key=value` pair in
/// the condition must equal the blockstate's property value.
#[derive(Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
struct StateCondition {
    pairs: Vec<(String, String)>,
}

impl StateCondition {
    fn parse(raw: &str) -> Self {
        let pairs = raw
            .split(',')
            .filter_map(|pair| {
                let (k, v) = pair.split_once('=')?;
                let k = k.trim();
                let v = v.trim();
                (!k.is_empty() && !v.is_empty()).then(|| (k.to_string(), v.to_string()))
            })
            .collect();
        Self { pairs }
    }

    fn matches(&self, state: &[(String, String)]) -> bool {
        self.pairs
            .iter()
            .all(|(k, v)| state.iter().any(|(sk, sv)| sk == k && sv == v))
    }
}

/// Resolved physics properties for one block. Every field is optional:
/// absent properties fall back to the engine defaults when the registry is
/// active, or to the legacy material policy when it is not.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SubworldBlockProps {
    pub mass: Option<f64>,
    pub volume: Option<f64>,
    pub friction: Option<f64>,
    pub restitution: Option<f64>,
    pub fragile: Option<bool>,
    pub floating_material: Option<String>,
    pub floating_scale: Option<f64>,
    pub inertia: Option<[f64; 3]>,
}

impl SubworldBlockProps {
    fn merge(&mut self, other: &SubworldBlockProps) {
        if other.mass.is_some() {
            self.mass = other.mass;
        }
        if other.volume.is_some() {
            self.volume = other.volume;
        }
        if other.friction.is_some() {
            self.friction = other.friction;
        }
        if other.restitution.is_some() {
            self.restitution = other.restitution;
        }
        if other.fragile.is_some() {
            self.fragile = other.fragile;
        }
        if other.floating_material.is_some() {
            self.floating_material = other.floating_material.clone();
        }
        if other.floating_scale.is_some() {
            self.floating_scale = other.floating_scale;
        }
        if other.inertia.is_some() {
            self.inertia = other.inertia;
        }
    }
}

/// A `data/*/floating_materials/*.json` entry.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct FloatingMaterial {
    pub lift_strength: f64,
    pub transition_speed: f64,
    pub prevent_self_lift: bool,
    pub scale_with_gravity: bool,
    pub scale_with_pressure: bool,
    pub slow_vertical_friction: f64,
    pub fast_vertical_friction: f64,
    pub slow_horizontal_friction: f64,
    pub fast_horizontal_friction: f64,
}

#[derive(Clone, Debug)]
struct PhysicsRule {
    priority: i64,
    /// Insertion order — later files win ties deterministically.
    order: u64,
    /// Built-in fallbacks layer below user data and are deduplicated when
    /// independently loaded archives merge.
    builtin: bool,
    selector: Selector,
    properties: SubworldBlockProps,
    /// Sorted by condition for deterministic application order.
    overrides: Vec<(StateCondition, SubworldBlockProps)>,
}

/// The merged runtime subworld physics registry.
#[derive(Clone, Debug, Default)]
pub struct SubworldPhysicsData {
    rules: Vec<PhysicsRule>,
    /// `"namespace:path"` -> raw member value strings (`"ns:id"` or `"#ns:tag"`).
    /// These stay raw after finalization so later archive merges can still
    /// resolve cross-archive nested references.
    tag_members: BTreeMap<String, BTreeSet<String>>,
    /// `"namespace:path"` -> raw `remove` value strings per tag.
    tag_removals: BTreeMap<String, BTreeSet<String>>,
    /// Expanded tag membership produced by [`Self::finalize_tags`].
    resolved_tag_members: BTreeMap<String, BTreeSet<String>>,
    /// `"namespace:path"` -> floating material.
    floating_materials: BTreeMap<String, FloatingMaterial>,
    /// Monotonic rule counter shared across merges so tie-breaks stay
    /// deterministic regardless of load order.
    next_order: u64,
    /// `true` once any JAR contributed physics data; unmatched blocks then
    /// resolve to [`DATA_DEFAULT_MASS`] instead of the legacy policy.
    active: bool,
    /// `true` after [`Self::finalize_tags`] expanded nested `#tag` refs.
    tags_finalized: bool,
}

// ---------------------------------------------------------------------------
// JSON parsing helpers
// ---------------------------------------------------------------------------

fn json_f64(obj: &BTreeMap<String, JsonValue>, key: &str) -> Option<f64> {
    obj.get(key).and_then(JsonValue::as_f64)
}

fn json_bool(obj: &BTreeMap<String, JsonValue>, key: &str) -> Option<bool> {
    obj.get(key).and_then(JsonValue::as_bool)
}

fn json_string(obj: &BTreeMap<String, JsonValue>, key: &str) -> Option<String> {
    obj.get(key).and_then(JsonValue::as_str).map(str::to_string)
}

/// Physics definitions default to priority 1000; malformed/non-integral or
/// negative priorities invalidate the definition rather than truncating it.
fn json_priority(obj: &BTreeMap<String, JsonValue>) -> Option<i64> {
    let Some(raw) = obj.get("priority") else {
        return Some(1000);
    };
    let value = raw.as_f64()?;
    (value.fract() == 0.0 && value >= 0.0 && value <= f64::from(i32::MAX)).then_some(value as i64)
}

/// Parse a bare or namespaced property bag into [`SubworldBlockProps`]. The
/// namespace is metadata for pack authors; the local key name is the contract.
fn parse_block_props(obj: &BTreeMap<String, JsonValue>) -> SubworldBlockProps {
    let mut props = SubworldBlockProps::default();
    for (key, value) in obj {
        let name = key.rsplit_once(':').map(|(_, local)| local).unwrap_or(key);
        match name {
            "mass" => props.mass = value.as_f64(),
            "volume" => props.volume = value.as_f64(),
            "friction" => props.friction = value.as_f64(),
            "restitution" => props.restitution = value.as_f64(),
            "fragile" => props.fragile = value.as_bool(),
            "floating_material" => {
                props.floating_material = value.as_str().map(normalize_resource_location)
            }
            "floating_scale" => props.floating_scale = value.as_f64(),
            "inertia" => {
                if let Some(arr) = value.as_array() {
                    if arr.len() == 3 {
                        let v: Option<Vec<f64>> = arr.iter().map(|v| v.as_f64()).collect();
                        if let Some(v) = v {
                            props.inertia = <[f64; 3]>::try_from(v.as_slice()).ok();
                        }
                    }
                }
            }
            _ => {}
        }
    }
    props
}

fn parse_floating_material(obj: &BTreeMap<String, JsonValue>) -> Option<FloatingMaterial> {
    Some(FloatingMaterial {
        lift_strength: json_f64(obj, "lift_strength")?,
        transition_speed: json_f64(obj, "transition_speed").unwrap_or(0.0),
        prevent_self_lift: json_bool(obj, "prevent_self_lift").unwrap_or(false),
        scale_with_gravity: json_bool(obj, "scale_with_gravity").unwrap_or(false),
        scale_with_pressure: json_bool(obj, "scale_with_pressure").unwrap_or(false),
        slow_vertical_friction: json_f64(obj, "slow_vertical_friction").unwrap_or(0.0),
        fast_vertical_friction: json_f64(obj, "fast_vertical_friction").unwrap_or(0.0),
        slow_horizontal_friction: json_f64(obj, "slow_horizontal_friction").unwrap_or(0.0),
        fast_horizontal_friction: json_f64(obj, "fast_horizontal_friction").unwrap_or(0.0),
    })
}

// ---------------------------------------------------------------------------
// Loading
// ---------------------------------------------------------------------------

/// Built-in engine fallback: bedrock is effectively immovable ballast even
/// when no datapack rule covers it. Tag-category mass fallbacks are evaluated
/// dynamically in [`SubworldPhysicsData::resolve`] so any namespace can use
/// the neutral mass-category tag leaf names.
fn builtin_rules() -> Vec<PhysicsRule> {
    vec![PhysicsRule {
        priority: 0,
        order: 0,
        builtin: true,
        selector: Selector::Id("minecraft:bedrock".to_string()),
        properties: SubworldBlockProps {
            mass: Some(1000.0),
            ..SubworldBlockProps::default()
        },
        overrides: Vec::new(),
    }]
}

/// Last `/`-separated tag path component, with any namespace stripped.
fn tag_leaf(tag: &str) -> &str {
    let path = tag.rsplit_once(':').map(|(_, path)| path).unwrap_or(tag);
    path.rsplit_once('/').map(|(_, leaf)| leaf).unwrap_or(path)
}

/// Whether the tag leaf names `category`, either exactly or as a trailing
/// underscore-separated component (`balloon_envelope` matches `envelope`).
fn tag_leaf_matches(tag: &str, category: &str) -> bool {
    let leaf = tag_leaf(tag);
    leaf == category || leaf.ends_with(&format!("_{category}"))
}

/// `data/<ns>/<kind>/<rest>.json` split; returns `(namespace, kind, rest)`.
fn data_path(name: &str) -> Option<(&str, &str, &str)> {
    let rest = name.strip_prefix("data/")?;
    let (ns, rest) = rest.split_once('/')?;
    let (kind, file) = rest.split_once('/')?;
    let file = file.strip_suffix(".json")?;
    Some((ns, kind, file))
}

/// Minecraft resource-location defaulting: a bare path is in the
/// `minecraft` namespace unless it already carries `namespace:`.
fn normalize_resource_location(id: &str) -> String {
    if id.contains(':') {
        id.to_string()
    } else {
        format!("minecraft:{id}")
    }
}

/// Pull one tag `values`/`remove` entry: plain `"ns:id"`, `"#ns:tag"` or
/// `{ "id", "required": ... }`.
fn collect_tag_value(value: &JsonValue, out: &mut BTreeSet<String>) {
    if let Some(id) = value.as_str() {
        if let Some(tag) = id.strip_prefix('#') {
            out.insert(format!("#{}", normalize_resource_location(tag)));
        } else {
            out.insert(normalize_resource_location(id));
        }
        return;
    }
    if let Some(obj) = value.as_object() {
        if let Some(id) = json_string(obj, "id") {
            if let Some(tag) = id.strip_prefix('#') {
                out.insert(format!("#{}", normalize_resource_location(tag)));
            } else {
                out.insert(normalize_resource_location(&id));
            }
        }
    }
}

impl SubworldPhysicsData {
    /// An empty, inactive registry (no runtime physics data seen yet).
    pub fn empty() -> Self {
        Self::default()
    }

    /// Registry seeded with only the built-in fallback rules; stays inactive
    /// so unmatched blocks still fall through to the legacy material policy.
    pub fn builtin() -> Self {
        let mut data = Self::default();
        let rules = builtin_rules();
        data.next_order = rules.len() as u64;
        data.rules = rules;
        data
    }

    /// Load every `data/` entry of one ZIP/JAR byte image into `self`.
    /// Nested `META-INF/jarjar/*.jar` members are indexed transparently by
    /// [`ZipSliceIndex`]. Entries that fail to parse are skipped individually
    /// — a malformed addon file must never take down physics loading. Tag
    /// `#`-references stay unexpanded until [`Self::finalize_tags`].
    pub fn load_zip(&mut self, bytes: &[u8]) -> Result<(), megu_core::zip_archive::ZipError> {
        let index = ZipSliceIndex::open_filtered(bytes, |name| {
            name.starts_with("data/") && name.ends_with(".json")
        })?;
        // Deterministic order: sort names so duplicate members across nested
        // JARs resolve identically every run.
        let mut names: Vec<String> = index.entries().iter().map(|e| e.name.clone()).collect();
        names.sort();
        names.dedup();
        for name in names {
            let Some((ns, kind, file)) = data_path(&name) else {
                continue;
            };
            let Ok(raw) = index.read_entry(&name, 1 << 20) else {
                continue;
            };
            let Ok(json) = decode_data_json(&raw) else {
                continue;
            };
            match kind {
                "physics_block_properties" => self.load_physics_file(&json),
                "floating_materials" => self.load_floating_material(ns, file, &json),
                "tags" if file.starts_with("block/") => {
                    self.load_block_tag(ns, &file["block/".len()..], &json)
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// Expand nested `#tag` references inside `values`/`remove` lists.
    /// Idempotent; [`Self::from_zip`]/[`Self::load_jars`] call it automatically.
    pub fn finalize_tags(&mut self) {
        if self.tags_finalized {
            return;
        }
        self.tags_finalized = true;
        let raw = self.tag_members.clone();
        fn expand(
            tag: &str,
            raw: &BTreeMap<String, BTreeSet<String>>,
            seen: &mut BTreeSet<String>,
            out: &mut BTreeSet<String>,
        ) {
            if !seen.insert(tag.to_string()) {
                return;
            }
            let Some(values) = raw.get(tag) else {
                return;
            };
            for value in values {
                if let Some(nested) = value.strip_prefix('#') {
                    expand(nested, raw, seen, out);
                } else {
                    out.insert(value.clone());
                }
            }
        }
        let mut expanded: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        for tag in raw.keys() {
            let mut members = BTreeSet::new();
            expand(tag, &raw, &mut BTreeSet::new(), &mut members);
            expanded.insert(tag.clone(), members);
        }
        // Apply removals against the expanded membership.
        for (tag, removed_raw) in &self.tag_removals {
            if let Some(members) = expanded.get_mut(tag) {
                let mut removed = BTreeSet::new();
                for value in removed_raw {
                    if let Some(nested) = value.strip_prefix('#') {
                        let mut seen = BTreeSet::new();
                        expand(nested, &raw, &mut seen, &mut removed);
                    } else {
                        removed.insert(value.clone());
                    }
                }
                for id in removed {
                    members.remove(&id);
                }
            }
        }
        self.resolved_tag_members = expanded;
    }

    /// Convenience: build a fresh [`Self::builtin`] registry, load `bytes`,
    /// and finalize tag expansion.
    pub fn from_zip(bytes: &[u8]) -> Result<Self, megu_core::zip_archive::ZipError> {
        let mut data = Self::builtin();
        data.load_zip(bytes)?;
        data.finalize_tags();
        Ok(data)
    }

    /// Load several JAR images into one registry. Load order only affects
    /// same-name floating materials, where the later load wins.
    pub fn load_jars<I, B>(jars: I) -> Result<Self, megu_core::zip_archive::ZipError>
    where
        I: IntoIterator<Item = B>,
        B: AsRef<[u8]>,
    {
        let mut data = Self::builtin();
        for jar in jars {
            data.load_zip(jar.as_ref())?;
        }
        data.finalize_tags();
        Ok(data)
    }

    fn load_physics_file(&mut self, json: &JsonValue) {
        let Some(obj) = json.as_object() else {
            return;
        };
        let Some(selector_raw) = json_string(obj, "selector") else {
            return;
        };
        let selector = if let Some(tag) = selector_raw.strip_prefix('#') {
            Selector::Tag(tag.to_string())
        } else {
            Selector::Id(selector_raw)
        };
        // Namespace-less resource locations follow Minecraft's usual default
        // namespace rather than the file's own namespace.
        let selector = match &selector {
            Selector::Id(id) if !id.contains(':') => Selector::Id(normalize_resource_location(id)),
            Selector::Tag(t) if !t.contains(':') => Selector::Tag(normalize_resource_location(t)),
            other => other.clone(),
        };
        let Some(properties_obj) = obj.get("properties").and_then(JsonValue::as_object) else {
            return;
        };
        let properties = parse_block_props(properties_obj);
        let mut overrides = Vec::new();
        if let Some(over) = obj.get("overrides").and_then(JsonValue::as_object) {
            for (cond_raw, cond_props) in over {
                let Some(cond_obj) = cond_props.as_object() else {
                    continue;
                };
                overrides.push((StateCondition::parse(cond_raw), parse_block_props(cond_obj)));
            }
        }
        overrides.sort_by(|a, b| a.0.cmp(&b.0));
        let Some(priority) = json_priority(obj) else {
            return;
        };
        let order = self.next_order;
        self.next_order += 1;
        self.rules.push(PhysicsRule {
            priority,
            order,
            builtin: false,
            selector,
            properties,
            overrides,
        });
        self.active = true;
    }

    fn load_floating_material(&mut self, ns: &str, file: &str, json: &JsonValue) {
        let Some(obj) = json.as_object() else {
            return;
        };
        let Some(material) = parse_floating_material(obj) else {
            return;
        };
        self.floating_materials
            .insert(format!("{ns}:{file}"), material);
        self.active = true;
    }

    fn load_block_tag(&mut self, ns: &str, path: &str, json: &JsonValue) {
        let Some(obj) = json.as_object() else {
            return;
        };
        let tag_id = format!("{ns}:{path}");
        if obj.get("replace").and_then(JsonValue::as_bool) == Some(true) {
            self.tag_members.remove(&tag_id);
            self.tag_removals.remove(&tag_id);
        }
        if let Some(values) = obj.get("values").and_then(JsonValue::as_array) {
            let members = self.tag_members.entry(tag_id.clone()).or_default();
            for value in values {
                collect_tag_value(value, members);
            }
        }
        if let Some(removes) = obj.get("remove").and_then(JsonValue::as_array) {
            let removed = self.tag_removals.entry(tag_id).or_default();
            for value in removes {
                collect_tag_value(value, removed);
            }
        }
        // New raw tag input invalidates the previous expansion. Constructors
        // and archive merges finalize after all contributing archives load.
        self.tags_finalized = false;
        // Tag files alone do NOT activate the registry: ordinary mod block
        // tags (`data/<ns>/tags/block/*`) are ubiquitous, and activating on
        // them would replace every block's legacy mass policy with the registry
        // 1.0 default. Activation comes from `physics_block_properties` /
        // `floating_materials` entries, which carry physics semantics rather than ordinary classification metadata.
    }

    // -- resolution ---------------------------------------------------------

    fn effective_tag_table(&self) -> &BTreeMap<String, BTreeSet<String>> {
        if self.tags_finalized {
            &self.resolved_tag_members
        } else {
            &self.tag_members
        }
    }

    /// Does `id` belong to block tag `"namespace:path"` (without `#`)? Raw
    /// `#`-references only resolve after [`Self::finalize_tags`].
    pub fn is_tag_member(&self, tag: &str, id: &str) -> bool {
        self.effective_tag_table()
            .get(tag)
            .is_some_and(|members| members.contains(id))
    }

    /// All member ids of tag `"namespace:path"`.
    pub fn tag_members(&self, tag: &str) -> Option<&BTreeSet<String>> {
        self.effective_tag_table().get(tag)
    }

    /// The floating material registered as `"namespace:path"`.
    pub fn floating_material(&self, id: &str) -> Option<&FloatingMaterial> {
        self.floating_materials.get(id)
    }

    /// Highest-priority neutral mass-category fallback for `id`, derived from
    /// any namespaced tag whose leaf is one of the runtime mass categories.
    /// Category priorities mirror the data files they stand in for: ordinary
    /// categories layer at the default priority, while `super_light` layers
    /// just above them. Scanning the tag table is deterministic because the
    /// map is ordered; ties keep the lexicographically first tag.
    fn builtin_tag_mass(&self, id: &str, state: &[(String, String)]) -> Option<(i64, f64)> {
        let mut best: Option<(i64, f64)> = None;
        for (tag, members) in self.effective_tag_table() {
            if !members.contains(id) {
                continue;
            }
            let candidate = match tag_leaf(tag) {
                "heavy" => Some((1000, 2.0)),
                "light" => Some((1000, 0.5)),
                "normal" => Some((1000, 1.0)),
                "super_heavy" => Some((1000, 4.0)),
                "super_light" => Some((1001, 0.25)),
                "weightless" => Some((1000, 0.0)),
                _ => None,
            };
            if let Some(candidate) = candidate {
                if best.is_none_or(|current| candidate.0 > current.0) {
                    best = Some(candidate);
                }
            }
        }
        if let Some((priority, mass)) = best {
            let double_slab = state
                .iter()
                .any(|(key, value)| key == "type" && value == "double");
            return Some((priority, if double_slab { mass * 2.0 } else { mass }));
        }
        None
    }

    /// Resolved properties for `id` with blockstate `state`
    /// (`[("axis", "x"), ...]` parsed from the canonical string). Returns
    /// `None` when no rule selects the block and the registry is inactive —
    /// callers then fall back to the legacy material policy. When the
    /// registry is active but no rule matches, the engine defaults
    /// (`mass = 1.0`, ...) are filled in.
    pub fn resolve(&self, id: &str, state: &[(String, String)]) -> Option<SubworldBlockProps> {
        let mut matching: Vec<&PhysicsRule> = self
            .rules
            .iter()
            .filter(|rule| match &rule.selector {
                Selector::Id(sel) => sel == id,
                Selector::Tag(tag) => self.is_tag_member(tag, id),
            })
            .collect();
        matching.sort_by_key(|rule| (rule.priority, rule.order, !rule.builtin));

        // Neutral mass-category tags are engine fallback layers rather than
        // user rules: at equal priority user data still applies after them,
        // while the high-priority `super_light` category layers above ordinary
        // default-priority rules.
        let mut category = self.builtin_tag_mass(id, state);
        let mut resolved = SubworldBlockProps::default();
        let mut matched_any = false;
        for rule in matching {
            if let Some((category_priority, mass)) = category {
                if category_priority <= rule.priority {
                    resolved.merge(&SubworldBlockProps {
                        mass: Some(mass),
                        ..SubworldBlockProps::default()
                    });
                    category = None;
                }
            }
            resolved.merge(&rule.properties);
            for (cond, override_props) in &rule.overrides {
                if cond.matches(state) {
                    resolved.merge(override_props);
                }
            }
            matched_any = true;
        }
        if let Some((_, mass)) = category {
            resolved.merge(&SubworldBlockProps {
                mass: Some(mass),
                ..SubworldBlockProps::default()
            });
            matched_any = true;
        }
        if !matched_any && !self.active {
            return None;
        }
        if self.active {
            if resolved.mass.is_none() {
                resolved.mass = Some(DATA_DEFAULT_MASS);
            }
            if resolved.volume.is_none() {
                resolved.volume = Some(DATA_DEFAULT_VOLUME);
            }
            if resolved.friction.is_none() {
                resolved.friction = Some(DATA_DEFAULT_FRICTION);
            }
            if resolved.restitution.is_none() {
                resolved.restitution = Some(DATA_DEFAULT_RESTITUTION);
            }
            if resolved.floating_scale.is_none() {
                resolved.floating_scale = Some(DATA_DEFAULT_FLOATING_SCALE);
            }
        }
        Some(resolved)
    }

    /// Resolved mass for `id`/`state`, or `None` when the registry has no
    /// opinion (inactive and unmatched).
    pub fn mass_for(&self, id: &str, state: &[(String, String)]) -> Option<f64> {
        self.resolve(id, state).and_then(|p| p.mass)
    }

    /// The `(floating_material id, floating_scale)` pair for `id`/`state`.
    pub fn floating_for(&self, id: &str, state: &[(String, String)]) -> Option<(String, f64)> {
        let props = self.resolve(id, state)?;
        let material = props.floating_material?;
        let scale = props.floating_scale.unwrap_or(DATA_DEFAULT_FLOATING_SCALE);
        Some((material, scale))
    }

    /// Whether the block's resolved floating material lifts the block
    /// itself (`lift_strength > 0` and not `prevent_self_lift`).
    pub fn has_self_lift(&self, id: &str, state: &[(String, String)]) -> bool {
        let Some((material_id, _)) = self.floating_for(id, state) else {
            return false;
        };
        self.floating_material(&material_id)
            .is_some_and(|m| m.lift_strength > 0.0 && !m.prevent_self_lift)
    }

    /// Maximum `lift_strength` among the block's floating material — the
    /// enclosed-gas lift multiplier (`0.0` when absent).
    pub fn gas_lift_strength(&self, id: &str, state: &[(String, String)]) -> f64 {
        let Some((material_id, _)) = self.floating_for(id, state) else {
            return 0.0;
        };
        self.floating_material(&material_id)
            .map(|m| m.lift_strength)
            .unwrap_or(0.0)
    }

    /// Whether `id` belongs to any namespaced block tag whose leaf denotes an
    /// airtight or envelope barrier. Matching the leaf rather than a concrete
    /// namespace keeps runtime datapack compatibility fully data-driven.
    pub fn is_airtight_member(&self, id: &str) -> bool {
        self.effective_tag_table().iter().any(|(tag, members)| {
            members.contains(id)
                && (tag_leaf_matches(tag, "airtight") || tag_leaf_matches(tag, "envelope"))
        })
    }

    /// Whether `id`/`state` resolves to a lift-producing block through mod
    /// data: an airtight/envelope member or a floating material with
    /// `lift_strength > 0` (self-buoyant materials).
    pub fn is_lift_block(&self, id: &str, state: &[(String, String)]) -> bool {
        self.is_airtight_member(id) || self.gas_lift_strength(id, state) > 0.0
    }

    /// Whether the registry saw any physics data at all.
    pub fn is_active(&self) -> bool {
        self.active
    }

    /// Merge `other` into `self`: user rules append in discovery order,
    /// built-in fallbacks are deduplicated, raw tag members/removals union,
    /// and floating materials overwrite on name conflict. Raw tags are
    /// re-expanded after the union so nested `#tag` references can resolve
    /// across independent archive/discovery boundaries.
    pub fn absorb(&mut self, other: SubworldPhysicsData) {
        let mut has_builtin = self.rules.iter().any(|rule| rule.builtin);
        for mut rule in other.rules {
            if rule.builtin {
                if has_builtin {
                    continue;
                }
                has_builtin = true;
                rule.order = 0;
                if self.next_order == 0 {
                    self.next_order = 1;
                }
            } else {
                rule.order = self.next_order;
                self.next_order = self.next_order.saturating_add(1);
            }
            self.rules.push(rule);
        }
        for (tag, members) in other.tag_members {
            self.tag_members.entry(tag).or_default().extend(members);
        }
        for (tag, removals) in other.tag_removals {
            self.tag_removals.entry(tag).or_default().extend(removals);
        }
        for (id, material) in other.floating_materials {
            self.floating_materials.insert(id, material);
        }
        self.active |= other.active;
        self.tags_finalized = false;
        self.finalize_tags();
    }
}

// ---------------------------------------------------------------------------
// Process-wide registry
// ---------------------------------------------------------------------------

static GLOBAL_REGISTRY: RwLock<Option<Arc<SubworldPhysicsData>>> = RwLock::new(None);

/// Install (or merge into) the process-wide subworld physics registry. Called by
/// the Subworld bridge after mod-JAR discovery; every subsequently
/// assembled [`crate::aeronautics::RigidBodySubWorld`] picks it up.
/// Merging unions rules/tags/materials so independent datapacks and JARs can
/// arrive in separate discovery passes.
pub fn install_global(mut data: SubworldPhysicsData) {
    data.finalize_tags();
    let mut guard = GLOBAL_REGISTRY.write().unwrap_or_else(|e| e.into_inner());
    match guard.as_mut() {
        Some(existing) => {
            let mut merged = (**existing).clone();
            merged.absorb(data);
            *existing = Arc::new(merged);
        }
        None => *guard = Some(Arc::new(data)),
    }
}

/// The currently installed registry, if any.
pub fn current_global() -> Option<Arc<SubworldPhysicsData>> {
    GLOBAL_REGISTRY
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
}

/// Clear the process-wide registry (test isolation).
pub fn reset_global() {
    *GLOBAL_REGISTRY.write().unwrap_or_else(|e| e.into_inner()) = None;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal Stored-entry ZIP writer for synthetic JAR fixtures.
    fn build_zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut central = Vec::new();
        for (name, body) in entries {
            let name_b = name.as_bytes();
            let body_b = *body;
            let local_off = out.len() as u32;
            let crc = megu_core::hash::crc32_ieee(body_b);
            out.extend_from_slice(&0x0403_4b50u32.to_le_bytes()); // LFH sig
            out.extend_from_slice(&20u16.to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes()); // flags
            out.extend_from_slice(&0u16.to_le_bytes()); // method: stored
            out.extend_from_slice(&0u16.to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes());
            out.extend_from_slice(&crc.to_le_bytes());
            out.extend_from_slice(&(body_b.len() as u32).to_le_bytes());
            out.extend_from_slice(&(body_b.len() as u32).to_le_bytes());
            out.extend_from_slice(&(name_b.len() as u16).to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes());
            out.extend_from_slice(name_b);
            out.extend_from_slice(body_b);

            central.extend_from_slice(&0x0201_4b50u32.to_le_bytes()); // CDE sig
            central.extend_from_slice(&20u16.to_le_bytes());
            central.extend_from_slice(&20u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&crc.to_le_bytes());
            central.extend_from_slice(&(body_b.len() as u32).to_le_bytes());
            central.extend_from_slice(&(body_b.len() as u32).to_le_bytes());
            central.extend_from_slice(&(name_b.len() as u16).to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u32.to_le_bytes());
            central.extend_from_slice(&local_off.to_le_bytes());
            central.extend_from_slice(name_b);
        }
        let cd_off = out.len() as u32;
        let cd_size = central.len() as u32;
        out.extend_from_slice(&central);
        out.extend_from_slice(&0x0605_4b50u32.to_le_bytes()); // EOCD
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
        out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
        out.extend_from_slice(&cd_size.to_le_bytes());
        out.extend_from_slice(&cd_off.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out
    }

    fn zstr(s: &str) -> &[u8] {
        s.as_bytes()
    }

    const PHYSICS_ENTRIES: &[(&str, &str)] = &[
        (
            "data/testphysics/tags/block/heavy.json",
            r##"{"replace":false,"values":["minecraft:obsidian","#c:stones"]}"##,
        ),
        (
            "data/c/tags/block/stones.json",
            r##"{"values":["minecraft:stone"]}"##,
        ),
        (
            "data/testphysics/physics_block_properties/heavy.json",
            r##"{"selector":"#testphysics:heavy","properties":{"testphysics:mass":2.0}}"##,
        ),
        (
            "data/testphysics/physics_block_properties/slab.json",
            r##"{"selector":"minecraft:oak_slab","properties":{"testphysics:mass":0.5},"overrides":{"type=double":{"testphysics:mass":1.0}}}"##,
        ),
        (
            "data/testphysics/floating_materials/end_stone.json",
            r##"{"lift_strength":2,"transition_speed":2,"prevent_self_lift":true,"slow_vertical_friction":0.4}"##,
        ),
    ];

    const AERO_ENTRIES: &[(&str, &str)] = &[
        (
            "data/testair/tags/block/airtight.json",
            r##"{"values":["testair:red_envelope","#minecraft:wool"]}"##,
        ),
        (
            "data/minecraft/tags/block/wool.json",
            r##"{"values":["minecraft:white_wool"]}"##,
        ),
        (
            "data/testair/physics_block_properties/balloon_drag.json",
            r##"{"selector":"#testair:airtight","properties":{"testphysics:floating_material":"testair:simple_drag","testphysics:floating_scale":0.33}}"##,
        ),
        (
            "data/testair/floating_materials/simple_drag.json",
            r##"{"scale_with_pressure":true,"scale_with_gravity":true,"lift_strength":0.0}"##,
        ),
        (
            "data/testair/physics_block_properties/liftstone.json",
            r##"{"selector":"#testair:liftstone","properties":{"testphysics:floating_material":"testair:liftstone"}}"##,
        ),
        (
            "data/testair/tags/block/liftstone.json",
            r##"{"values":["testair:liftstone","testair:bright_liftstone"]}"##,
        ),
        (
            "data/testair/floating_materials/liftstone.json",
            r##"{"lift_strength":10,"prevent_self_lift":true}"##,
        ),
    ];

    fn fixture_zip() -> Vec<u8> {
        let mut entries: Vec<(&str, Vec<u8>)> = PHYSICS_ENTRIES
            .iter()
            .map(|(n, b)| (*n, b.as_bytes().to_vec()))
            .collect();
        entries.extend(
            AERO_ENTRIES
                .iter()
                .map(|(n, b)| (*n, b.as_bytes().to_vec())),
        );
        let refs: Vec<(&str, &[u8])> = entries.iter().map(|(n, b)| (*n, b.as_slice())).collect();
        build_zip(&refs)
    }

    #[test]
    fn loads_physics_tags_and_floating_materials_from_synthetic_jar() {
        let zip = fixture_zip();
        let data = SubworldPhysicsData::from_zip(&zip).unwrap();

        // Tag selector: obsidian is in #testphysics:heavy directly, stone via
        // the nested #c:stones reference.
        assert_eq!(data.mass_for("minecraft:obsidian", &[]), Some(2.0));
        assert_eq!(data.mass_for("minecraft:stone", &[]), Some(2.0));
        // Exact selector with a full property set.
        let bedrock = data.resolve("minecraft:bedrock", &[]).unwrap();
        assert_eq!(bedrock.mass, Some(1000.0));
        assert_eq!(bedrock.friction, Some(1.0));
        assert_eq!(bedrock.volume, Some(1.0));
        // Blockstate override: double slab -> 1.0, single -> 0.5.
        let state = vec![("type".to_string(), "double".to_string())];
        assert_eq!(data.mass_for("minecraft:oak_slab", &state), Some(1.0));
        assert_eq!(data.mass_for("minecraft:oak_slab", &[]), Some(0.5));
        // Unmatched block -> engine default once active.
        let double = vec![("type".to_string(), "double".to_string())];
        assert_eq!(data.mass_for("minecraft:dirt", &double), Some(1.0));
        // Airtight tag expanded through #minecraft:wool.
        assert!(data.is_airtight_member("testair:red_envelope"));
        assert!(data.is_airtight_member("minecraft:white_wool"));
        assert!(!data.is_airtight_member("minecraft:stone"));
        // Floating material wiring + self-lift semantics.
        let (mat, scale) = data
            .floating_for("testair:red_envelope", &[])
            .expect("airtight block has floating material");
        assert_eq!(mat, "testair:simple_drag");
        assert!((scale - 0.33).abs() < 1e-12);
        assert!(data
            .floating_material("testair:simple_drag")
            .is_some_and(|material| material.scale_with_gravity));
        assert!(!data.has_self_lift("testair:red_envelope", &[]));
        assert!(data.is_lift_block("testair:red_envelope", &[]));
        // liftstone: lift_strength 10 but prevent_self_lift -> no direct
        // self-lift; still a lift block via lift_strength > 0.
        assert!(!data.has_self_lift("testair:liftstone", &[]));
        assert!(data.is_lift_block("testair:liftstone", &[]));
        assert_eq!(data.gas_lift_strength("testair:liftstone", &[]), 10.0);
        // Builtin bedrock fallback is present even without physics files.
        let bare = SubworldPhysicsData::builtin();
        assert_eq!(bare.mass_for("minecraft:bedrock", &[]), Some(1000.0));
        assert_eq!(bare.mass_for("minecraft:stone", &[]), None);
        assert!(!bare.is_active());
    }

    #[test]
    fn nested_jarjar_entries_are_indexed() {
        let inner = build_zip(&[(
            "data/testair/physics_block_properties/x.json",
            zstr(r##"{"selector":"testair:red_envelope","properties":{"testphysics:mass":3.0}}"##),
        )]);
        let outer = build_zip(&[("META-INF/jarjar/testair.jar", inner.as_slice())]);
        let data = SubworldPhysicsData::from_zip(&outer).unwrap();
        assert_eq!(
            data.mass_for("testair:red_envelope", &[]),
            Some(3.0),
            "nested jarjar physics data must load"
        );
    }

    #[test]
    fn neutral_mass_category_tags_work_without_physics_rule_files() {
        let zip = build_zip(&[
            (
                "data/pack/tags/block/heavy.json",
                zstr(r#"{"values":["minecraft:obsidian"]}"#),
            ),
            (
                "data/pack/tags/block/super_light.json",
                zstr(r#"{"values":["minecraft:feather"]}"#),
            ),
            (
                "data/pack/tags/block/normal.json",
                zstr(r#"{"values":["minecraft:stone"]}"#),
            ),
            (
                "data/pack/tags/block/weightless.json",
                zstr(r#"{"values":["minecraft:cobblestone"]}"#),
            ),
        ]);
        let data = SubworldPhysicsData::from_zip(&zip).unwrap();
        assert!(!data.is_active());
        assert_eq!(data.mass_for("minecraft:obsidian", &[]), Some(2.0));
        assert_eq!(data.mass_for("minecraft:feather", &[]), Some(0.25));
        assert_eq!(data.mass_for("minecraft:stone", &[]), Some(1.0));
        assert_eq!(data.mass_for("minecraft:cobblestone", &[]), Some(0.0));
        let double = vec![("type".to_string(), "double".to_string())];
        assert_eq!(data.mass_for("minecraft:obsidian", &double), Some(4.0));
        assert_eq!(data.mass_for("minecraft:cobblestone", &double), Some(0.0));
        assert_eq!(data.mass_for("minecraft:dirt", &[]), None);
    }

    #[test]
    fn matching_rules_layer_by_priority_instead_of_winner_take_all() {
        let zip = build_zip(&[
            (
                "data/testphysics/physics_block_properties/base.json",
                zstr(
                    r##"{"priority":500,"selector":"minecraft:stone","properties":{"testphysics:mass":2.0,"testphysics:friction":0.25,"testphysics:volume":0.5}}"##,
                ),
            ),
            (
                "data/testphysics/physics_block_properties/high.json",
                zstr(
                    r##"{"priority":1500,"selector":"minecraft:stone","properties":{"testphysics:mass":3.0,"testphysics:restitution":0.2}}"##,
                ),
            ),
        ]);
        let data = SubworldPhysicsData::from_zip(&zip).unwrap();
        let resolved = data.resolve("minecraft:stone", &[]).unwrap();
        assert_eq!(
            resolved.mass,
            Some(3.0),
            "higher priority wins the same key"
        );
        assert_eq!(
            resolved.friction,
            Some(0.25),
            "lower-priority keys still contribute"
        );
        assert_eq!(resolved.volume, Some(0.5));
        assert_eq!(resolved.restitution, Some(0.2));
    }

    #[test]
    fn super_light_category_layers_above_default_priority_rules() {
        let zip = build_zip(&[
            (
                "data/pack/tags/block/super_light.json",
                zstr(r#"{"values":["minecraft:feather"]}"#),
            ),
            (
                "data/pack/physics_block_properties/feather.json",
                zstr(r##"{"selector":"minecraft:feather","properties":{"pack:mass":3.0}}"##),
            ),
        ]);
        let data = SubworldPhysicsData::from_zip(&zip).unwrap();
        assert_eq!(data.mass_for("minecraft:feather", &[]), Some(0.25));
    }

    #[test]
    fn physics_rule_requires_properties_and_bare_selector_defaults_to_minecraft() {
        let zip = build_zip(&[
            (
                "data/testphysics/physics_block_properties/incomplete.json",
                zstr(
                    r##"{"selector":"minecraft:dirt","overrides":{"type=double":{"testphysics:mass":7.0}}}"##,
                ),
            ),
            (
                "data/testphysics/physics_block_properties/bare.json",
                zstr(r##"{"selector":"stone","properties":{"testphysics:mass":4.0}}"##),
            ),
        ]);
        let data = SubworldPhysicsData::from_zip(&zip).unwrap();
        assert_eq!(data.mass_for("minecraft:stone", &[]), Some(4.0));
        assert_eq!(data.mass_for("testphysics:stone", &[]), Some(1.0));
        let double = vec![("type".to_string(), "double".to_string())];
        assert_eq!(data.mass_for("minecraft:dirt", &double), Some(1.0));
    }

    #[test]
    fn bare_resource_locations_default_to_minecraft_everywhere() {
        let zip = build_zip(&[
            (
                "data/minecraft/tags/block/bare_members.json",
                zstr(r##"{"values":["stone","#wool"]}"##),
            ),
            (
                "data/minecraft/tags/block/wool.json",
                zstr(r#"{"values":["white_wool"]}"#),
            ),
            (
                "data/minecraft/floating_materials/simple.json",
                zstr(r#"{"lift_strength":1.0}"#),
            ),
            (
                "data/testphysics/physics_block_properties/bare_material.json",
                zstr(
                    r##"{"selector":"minecraft:stone","properties":{"testphysics:floating_material":"simple"}}"##,
                ),
            ),
        ]);
        let data = SubworldPhysicsData::from_zip(&zip).unwrap();
        assert!(data.is_tag_member("minecraft:bare_members", "minecraft:stone"));
        assert!(data.is_tag_member("minecraft:bare_members", "minecraft:white_wool"));
        assert_eq!(
            data.floating_for("minecraft:stone", &[]),
            Some(("minecraft:simple".to_string(), 1.0))
        );
    }

    #[test]
    fn floating_material_requires_lift_strength() {
        let zip = build_zip(&[(
            "data/testair/floating_materials/incomplete.json",
            zstr(r#"{"scale_with_pressure":true}"#),
        )]);
        let data = SubworldPhysicsData::from_zip(&zip).unwrap();
        assert!(data.floating_material("testair:incomplete").is_none());
        assert!(!data.is_active());
    }

    #[test]
    fn absorb_preserves_cross_archive_tag_references() {
        let rules = build_zip(&[(
            "data/testphysics/physics_block_properties/heavy.json",
            zstr(r##"{"selector":"#testphysics:heavy","properties":{"testphysics:mass":7.0}}"##),
        )]);
        let nested_ref = build_zip(&[(
            "data/testphysics/tags/block/heavy.json",
            zstr(r##"{"values":["#testphysics:base"]}"##),
        )]);
        let base = build_zip(&[(
            "data/testphysics/tags/block/base.json",
            zstr(r#"{"values":["minecraft:stone"]}"#),
        )]);
        let mut merged = SubworldPhysicsData::from_zip(&rules).unwrap();
        merged.absorb(SubworldPhysicsData::from_zip(&nested_ref).unwrap());
        merged.absorb(SubworldPhysicsData::from_zip(&base).unwrap());
        assert_eq!(merged.mass_for("minecraft:stone", &[]), Some(7.0));
    }

    #[test]
    fn absorb_applies_removals_across_archive_boundaries() {
        let values = build_zip(&[(
            "data/testphysics/tags/block/heavy.json",
            zstr(r#"{"values":["minecraft:stone","minecraft:dirt"]}"#),
        )]);
        let removals = build_zip(&[(
            "data/testphysics/tags/block/heavy.json",
            zstr(r#"{"remove":["minecraft:dirt"]}"#),
        )]);
        let mut merged = SubworldPhysicsData::from_zip(&values).unwrap();
        merged.absorb(SubworldPhysicsData::from_zip(&removals).unwrap());
        assert!(merged.is_tag_member("testphysics:heavy", "minecraft:stone"));
        assert!(!merged.is_tag_member("testphysics:heavy", "minecraft:dirt"));
    }

    #[test]
    fn malformed_entries_are_skipped_not_fatal() {
        let zip = build_zip(&[
            (
                "data/testphysics/physics_block_properties/bad.json",
                zstr("not json"),
            ),
            (
                "data/testphysics/physics_block_properties/good.json",
                zstr(r##"{"selector":"minecraft:stone","properties":{"testphysics:mass":7.0}}"##),
            ),
        ]);
        let data = SubworldPhysicsData::from_zip(&zip).unwrap();
        assert_eq!(data.mass_for("minecraft:stone", &[]), Some(7.0));
    }

    #[test]
    fn tag_remove_and_replace_are_honored() {
        let zip = build_zip(&[
            (
                "data/testmod/tags/block/base.json",
                zstr(r##"{"values":["minecraft:stone","minecraft:dirt","#testmod:extra"]}"##),
            ),
            (
                "data/testmod/tags/block/extra.json",
                zstr(r##"{"values":["minecraft:gravel"]}"##),
            ),
            (
                "data/testmod/tags/block/filtered.json",
                zstr(
                    r##"{"values":["#testmod:base"],"remove":["#testmod:extra","minecraft:dirt"]}"##,
                ),
            ),
        ]);
        let data = SubworldPhysicsData::from_zip(&zip).unwrap();
        assert!(data.is_tag_member("testmod:filtered", "minecraft:stone"));
        assert!(!data.is_tag_member("testmod:filtered", "minecraft:gravel"));
        assert!(!data.is_tag_member("testmod:filtered", "minecraft:dirt"));
    }
}
