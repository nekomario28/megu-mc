// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later

//! Universal zero-JVM, zero-dependency JSON & stored-ZIP/JAR loader for ANY
//! mod's recipes, tags, and declarative offload profiles.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

use mod_offload::recipe_processor::{Recipe, Resource, ResourceKind, Stack};

use crate::blockstates::PropertyDef;

const MAX_JSON_DEPTH: usize = 64;
const MAX_JSON_BYTES: usize = 4 * 1024 * 1024;
const DEFAULT_PROCESSING_TICKS: u32 = 100;

/// Deterministic item & fluid string-to-u32 interner shared across all mod
/// adapters and `mod_offload` simulation kernels.
#[derive(Clone, Debug, Default)]
pub struct ItemFluidIdTable {
    items: BTreeMap<String, u32>,
    fluids: BTreeMap<String, u32>,
    next_item: u32,
    next_fluid: u32,
}

impl ItemFluidIdTable {
    pub fn new() -> Self {
        Self {
            items: BTreeMap::new(),
            fluids: BTreeMap::new(),
            next_item: 1,
            next_fluid: 1,
        }
    }

    pub fn intern_item(&mut self, id: &str) -> u32 {
        if let Some(&existing) = self.items.get(id) {
            return existing;
        }
        let assigned = self.next_item;
        self.next_item += 1;
        self.items.insert(id.to_string(), assigned);
        assigned
    }

    pub fn intern_fluid(&mut self, id: &str) -> u32 {
        if let Some(&existing) = self.fluids.get(id) {
            return existing;
        }
        let assigned = self.next_fluid;
        self.next_fluid += 1;
        self.fluids.insert(id.to_string(), assigned);
        assigned
    }

    pub fn item_id(&self, id: &str) -> Option<u32> {
        self.items.get(id).copied()
    }

    pub fn fluid_id(&self, id: &str) -> Option<u32> {
        self.fluids.get(id).copied()
    }
}

/// Parsed recipe from ANY mod namespace (`minecraft:smelting`, `create:pressing`,
/// `mekanism:crushing`, `thermal:pulverizer`, etc.).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParsedModRecipe {
    pub name: String,
    pub recipe_type: String,
    pub recipe: Recipe,
}

/// Aggregated zero-JVM data extracted from a datapack directory or stored JAR.
#[derive(Clone, Debug, Default)]
pub struct LoadedModData {
    pub recipes: Vec<ParsedModRecipe>,
    pub tags: BTreeMap<String, Vec<String>>,
    /// Declared blockstate properties per block id, harvested from
    /// `assets/<ns>/blockstates/*.json` `variants` keys and `multipart`
    /// `when` clauses. Presence of an entry (even with no properties)
    /// confirms the block exists in the mod JAR.
    pub blockstates: BTreeMap<String, Vec<PropertyDef>>,
    pub skipped_compressed_zip_entries: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DataLoadError {
    Io(String),
    Json(String),
    Zip(String),
}

/// Load recipes and tags for any mod namespace from an unpacked datapack root.
pub fn load_from_directory(
    root: &Path,
    ids: &mut ItemFluidIdTable,
) -> Result<LoadedModData, DataLoadError> {
    let mut files = Vec::new();
    for top in ["data", "assets"] {
        let dir = root.join(top);
        if dir.is_dir() {
            collect_json_files(&dir, root, &mut files)?;
        }
    }
    files.sort_by(|a, b| a.0.cmp(&b.0));

    let mut out = LoadedModData::default();
    let mut next_recipe_id = 1u32;
    for (rel_path, bytes) in files {
        ingest_entry(&rel_path, &bytes, ids, &mut next_recipe_id, &mut out)?;
    }
    Ok(out)
}

/// Load recipes and tags for any mod namespace from a stored-entry JAR/ZIP archive.
pub fn load_from_stored_zip(
    bytes: &[u8],
    ids: &mut ItemFluidIdTable,
) -> Result<LoadedModData, DataLoadError> {
    let mut entries = Vec::new();
    let mut skipped_compressed = 0usize;
    let mut cursor = 0usize;

    while cursor + 4 <= bytes.len() {
        let sig = u32::from_le_bytes([
            bytes[cursor],
            bytes[cursor + 1],
            bytes[cursor + 2],
            bytes[cursor + 3],
        ]);
        if sig != 0x0403_4b50 {
            break;
        }
        if cursor + 30 > bytes.len() {
            return Err(DataLoadError::Zip("truncated local file header".into()));
        }
        let flags = u16::from_le_bytes([bytes[cursor + 6], bytes[cursor + 7]]);
        if flags & 0x08 != 0 {
            return Err(DataLoadError::Zip(
                "data-descriptor streaming ZIP entries are unsupported in stored scanner".into(),
            ));
        }
        let method = u16::from_le_bytes([bytes[cursor + 8], bytes[cursor + 9]]);
        let comp_size = u32::from_le_bytes([
            bytes[cursor + 18],
            bytes[cursor + 19],
            bytes[cursor + 20],
            bytes[cursor + 21],
        ]) as usize;
        let name_len = u16::from_le_bytes([bytes[cursor + 26], bytes[cursor + 27]]) as usize;
        let extra_len = u16::from_le_bytes([bytes[cursor + 28], bytes[cursor + 29]]) as usize;

        let name_start = cursor + 30;
        let data_start = name_start
            .checked_add(name_len)
            .and_then(|v| v.checked_add(extra_len))
            .ok_or_else(|| DataLoadError::Zip("header length overflow".into()))?;
        let data_end = data_start
            .checked_add(comp_size)
            .ok_or_else(|| DataLoadError::Zip("entry payload overflow".into()))?;
        if data_end > bytes.len() {
            return Err(DataLoadError::Zip("truncated ZIP entry payload".into()));
        }
        let name = std::str::from_utf8(&bytes[name_start..name_start + name_len])
            .map_err(|_| DataLoadError::Zip("non-UTF8 ZIP entry path".into()))?;
        if name.ends_with(".json") && (name.starts_with("data/") || name.starts_with("assets/")) {
            if method == 0 {
                entries.push((name.to_string(), bytes[data_start..data_end].to_vec()));
            } else {
                skipped_compressed += 1;
            }
        }
        cursor = data_end;
    }

    entries.sort_by(|a, b| a.0.cmp(&b.0));
    let mut out = LoadedModData {
        skipped_compressed_zip_entries: skipped_compressed,
        ..LoadedModData::default()
    };
    let mut next_recipe_id = 1u32;
    for (rel_path, payload) in entries {
        ingest_entry(&rel_path, &payload, ids, &mut next_recipe_id, &mut out)?;
    }
    Ok(out)
}

/// Parse a generic Minecraft/NeoForge/Mod recipe JSON into a [`ParsedModRecipe`].
/// Recognizes the standard `ingredients` / `ingredient` / `input` and
/// `results` / `result` / `output` schema families used across Vanilla,
/// Create, Mekanism, Thermal, and AE2 — including Create's data-driven
/// processing recipes (`create:pressing`/`mixing`/`crushing`/`milling`/
/// `compacting`/`splashing`/`haunting`/`deploying`/`sequenced_assembly`,
/// gated by an optional `heat_requirement`/`heatRequirement`), shaped
/// `pattern`+`key` crafting (`minecraft:crafting_shaped`,
/// `create:mechanical_crafting`), smithing `template`/`base`/`addition`
/// slots, and `neoforge:conditional` recipe wrappers.
pub fn parse_mod_recipe_json(
    recipe_name: &str,
    recipe_id: u32,
    json_bytes: &[u8],
    ids: &mut ItemFluidIdTable,
) -> Result<Option<ParsedModRecipe>, DataLoadError> {
    let root = parse_json(json_bytes)?;
    let Some(obj) = root.as_object() else {
        return Ok(None);
    };
    Ok(parse_mod_recipe_value(recipe_name, recipe_id, obj, ids, 0))
}

/// Deepest `neoforge:conditional` `recipe` nesting the parser unwraps —
/// real conditional recipes nest at most one level.
const MAX_CONDITIONAL_RECIPE_DEPTH: usize = 4;

fn parse_mod_recipe_value(
    recipe_name: &str,
    recipe_id: u32,
    obj: &BTreeMap<String, JsonValue>,
    ids: &mut ItemFluidIdTable,
    depth: usize,
) -> Option<ParsedModRecipe> {
    let recipe_type = obj.get("type").and_then(JsonValue::as_str)?;

    // NeoForge conditional recipes carry the real recipe object under
    // `recipe`; the `neoforge:conditions` list only gates whether the
    // recipe loads, so an unwrapped entry remains a faithful recipe.
    if recipe_type == "neoforge:conditional" {
        if depth < MAX_CONDITIONAL_RECIPE_DEPTH {
            if let Some(inner) = obj.get("recipe").and_then(JsonValue::as_object) {
                return parse_mod_recipe_value(recipe_name, recipe_id, inner, ids, depth + 1);
            }
        }
        return None;
    }

    let mut inputs = Vec::new();
    for key in ["ingredients", "item_inputs", "fluid_inputs", "inputs"] {
        if let Some(val) = obj.get(key) {
            match val.as_array() {
                Some(arr) => {
                    for item in arr {
                        if let Some(stack) = parse_stack(item, ids) {
                            merge_stack(&mut inputs, stack);
                        }
                    }
                }
                None => {
                    // Some schemas serialize a lone ingredient object
                    // under the plural key.
                    if let Some(stack) = parse_stack(val, ids) {
                        merge_stack(&mut inputs, stack);
                    }
                }
            }
        }
    }
    for key in [
        "ingredient",
        "input",
        "item_input",
        "fluid_input",
        "input_item",
        "input_fluid",
        "itemInput",
        "fluidInput",
        "base",
        "template",
        "addition",
    ] {
        if let Some(val) = obj.get(key) {
            if let Some(stack) = parse_stack(val, ids) {
                merge_stack(&mut inputs, stack);
            }
        }
    }
    // Shaped crafting (`minecraft:crafting_shaped`,
    // `create:mechanical_crafting`, …): `key` maps pattern symbols to
    // ingredient objects; every symbol cell in `pattern` consumes one.
    if let (Some(pattern), Some(key_map)) = (
        obj.get("pattern").and_then(JsonValue::as_array),
        obj.get("key").and_then(JsonValue::as_object),
    ) {
        for row in pattern {
            let Some(row) = row.as_str() else {
                continue;
            };
            for symbol in row.chars() {
                let key_name = symbol.to_string();
                if let Some(ingredient) = key_map.get(&key_name) {
                    if let Some(stack) = parse_stack(ingredient, ids) {
                        merge_stack(&mut inputs, stack);
                    }
                }
            }
        }
    }

    let mut outputs = Vec::new();
    for key in ["results", "item_outputs", "fluid_outputs", "outputs"] {
        if let Some(val) = obj.get(key) {
            match val.as_array() {
                Some(arr) => {
                    for item in arr {
                        if let Some(stack) = parse_stack(item, ids) {
                            merge_stack(&mut outputs, stack);
                        }
                    }
                }
                None => {
                    if let Some(stack) = parse_stack(val, ids) {
                        merge_stack(&mut outputs, stack);
                    }
                }
            }
        }
    }
    for key in [
        "result",
        "output",
        "main_output",
        "item_output",
        "fluid_output",
        "itemOutput",
        "fluidOutput",
    ] {
        if let Some(val) = obj.get(key) {
            if let Some(stack) = parse_stack(val, ids) {
                merge_stack(&mut outputs, stack);
            }
        }
    }
    if inputs.is_empty() || outputs.is_empty() {
        return None;
    }

    let base_ticks = obj
        .get("processing_time")
        .or_else(|| obj.get("processingTime"))
        .or_else(|| obj.get("cookingtime"))
        .or_else(|| obj.get("energy"))
        .or_else(|| obj.get("time"))
        .and_then(JsonValue::as_u64)
        .and_then(|v| u32::try_from(v).ok())
        .filter(|&v| v > 0)
        .unwrap_or(DEFAULT_PROCESSING_TICKS);

    Some(ParsedModRecipe {
        name: recipe_name.to_string(),
        recipe_type: recipe_type.to_string(),
        recipe: Recipe {
            id: recipe_id,
            base_ticks,
            inputs,
            outputs,
        },
    })
}

fn collect_json_files(
    dir: &Path,
    root: &Path,
    out: &mut Vec<(String, Vec<u8>)>,
) -> Result<(), DataLoadError> {
    let entries = fs::read_dir(dir).map_err(|e| DataLoadError::Io(e.to_string()))?;
    for entry in entries {
        let entry = entry.map_err(|e| DataLoadError::Io(e.to_string()))?;
        let path = entry.path();
        if path.is_dir() {
            collect_json_files(&path, root, out)?;
        } else if path.extension().and_then(|e| e.to_str()) == Some("json") {
            let rel = path
                .strip_prefix(root)
                .map_err(|e| DataLoadError::Io(e.to_string()))?
                .to_string_lossy()
                .replace('\\', "/");
            let bytes = fs::read(&path).map_err(|e| DataLoadError::Io(e.to_string()))?;
            out.push((rel, bytes));
        }
    }
    Ok(())
}

fn ingest_entry(
    rel_path: &str,
    bytes: &[u8],
    ids: &mut ItemFluidIdTable,
    next_recipe_id: &mut u32,
    out: &mut LoadedModData,
) -> Result<(), DataLoadError> {
    let parts: Vec<&str> = rel_path.split('/').collect();
    if parts.len() < 4 {
        return Ok(());
    }
    let ns = parts[1];
    let category = parts[2];
    let stem_path = parts[3..].join("/");
    let stem = stem_path.strip_suffix(".json").unwrap_or(&stem_path);

    if parts[0] == "assets" {
        if category == "blockstates" {
            if let Some(props) = parse_blockstate_properties(bytes) {
                out.blockstates.insert(format!("{ns}:{stem}"), props);
            }
        }
        return Ok(());
    }
    if parts[0] != "data" {
        return Ok(());
    }

    if category == "recipe" || category == "recipes" {
        let name = format!("{ns}:{stem}");
        if let Some(parsed) = parse_mod_recipe_json(&name, *next_recipe_id, bytes, ids)? {
            *next_recipe_id += 1;
            out.recipes.push(parsed);
        }
    } else if category == "tags" && parts.len() >= 5 {
        let tag_key = format!("{ns}:{stem}");
        let root = parse_json(bytes)?;
        if let Some(values) = root
            .as_object()
            .and_then(|o| o.get("values"))
            .and_then(JsonValue::as_array)
        {
            let mut list = Vec::new();
            for v in values {
                if let Some(s) = v.as_str() {
                    list.push(s.to_string());
                } else if let Some(id) = v
                    .as_object()
                    .and_then(|o| o.get("id"))
                    .and_then(JsonValue::as_str)
                {
                    list.push(id.to_string());
                }
            }
            out.tags.insert(tag_key, list);
        }
    }
    Ok(())
}

/// Extract the declared `BlockState` property table from a mod's
/// `blockstates/*.json` file. Reads `variants` selector keys (`"axis=x"`,
/// `"facing=north,waterlogged=false"`, `""`) and `multipart[].when` clauses
/// (including `OR` sub-clauses and `"a|b"` value unions). Returns `None`
/// only when the file is not a blockstate object at all.
fn parse_blockstate_properties(bytes: &[u8]) -> Option<Vec<PropertyDef>> {
    let root = parse_json(bytes).ok()?;
    blockstate_properties(&root)
}

/// [`parse_blockstate_properties`] over an already-parsed [`JsonValue`]
/// root — the shared AST form the auto-adapter's `assets/` extraction
/// reuses after its own bounded read. Returns `None` only when the
/// document is not a blockstate object at all.
pub fn blockstate_properties(root: &JsonValue) -> Option<Vec<PropertyDef>> {
    let obj = root.as_object()?;
    let mut props: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut seen_form = false;
    if let Some(variants) = obj.get("variants").and_then(JsonValue::as_object) {
        seen_form = true;
        for key in variants.keys() {
            record_selector_key(key, &mut props);
        }
    }
    if let Some(multipart) = obj.get("multipart").and_then(JsonValue::as_array) {
        seen_form = true;
        for part in multipart {
            if let Some(when) = part
                .as_object()
                .and_then(|o| o.get("when"))
                .and_then(JsonValue::as_object)
            {
                record_when_clause(when, &mut props);
            }
        }
    }
    if !seen_form {
        return None;
    }
    Some(
        props
            .into_iter()
            .map(|(name, values)| PropertyDef {
                name,
                values: values.into_iter().collect(),
            })
            .collect(),
    )
}

fn record_selector_key(key: &str, props: &mut BTreeMap<String, BTreeSet<String>>) {
    for pair in key.split(',') {
        let Some((name, value)) = pair.split_once('=') else {
            continue;
        };
        record_property_values(name.trim(), value.trim(), props);
    }
}

fn record_when_clause(
    when: &BTreeMap<String, JsonValue>,
    props: &mut BTreeMap<String, BTreeSet<String>>,
) {
    for (name, value) in when {
        if name == "OR" {
            if let Some(sub_clauses) = value.as_array() {
                for sub in sub_clauses {
                    if let Some(sub_obj) = sub.as_object() {
                        record_when_clause(sub_obj, props);
                    }
                }
            }
        } else if let Some(text) = value.as_str() {
            record_property_values(name, text, props);
        }
    }
}

fn record_property_values(
    name: &str,
    value: &str,
    props: &mut BTreeMap<String, BTreeSet<String>>,
) {
    if name.is_empty() {
        return;
    }
    let entry = props.entry(name.to_string()).or_default();
    for v in value.split('|') {
        let v = v.trim();
        if !v.is_empty() {
            entry.insert(v.to_string());
        }
    }
}

fn parse_stack(val: &JsonValue, ids: &mut ItemFluidIdTable) -> Option<Stack> {
    if let Some(item_str) = val.as_str() {
        return Some(Stack {
            resource: Resource {
                kind: ResourceKind::Item,
                id: ids.intern_item(item_str),
            },
            amount: 1,
        });
    }
    let obj = val.as_object()?;
    // `neoforge:compound`/`forge:compound` ingredients are any-of lists —
    // the first parseable child is the representative stack.
    if obj
        .get("type")
        .and_then(JsonValue::as_str)
        .is_some_and(|t| t.contains("compound"))
    {
        if let Some(children) = obj.get("children").and_then(JsonValue::as_array) {
            if let Some(stack) = children.iter().find_map(|c| parse_stack(c, ids)) {
                return Some(stack);
            }
        }
    }
    if let Some(fluid_name) = obj
        .get("fluid")
        .or_else(|| obj.get("fluidTag"))
        .or_else(|| obj.get("fluid_tag"))
        .and_then(JsonValue::as_str)
    {
        let amount = obj
            .get("amount")
            .and_then(JsonValue::as_u64)
            .and_then(|v| u32::try_from(v).ok())
            .filter(|&v| v > 0)
            .unwrap_or(1000);
        return Some(Stack {
            resource: Resource {
                kind: ResourceKind::Fluid,
                id: ids.intern_fluid(fluid_name),
            },
            amount,
        });
    }
    let item_name = obj
        .get("item")
        .or_else(|| obj.get("id"))
        .or_else(|| obj.get("tag"))
        .and_then(JsonValue::as_str)
        .or_else(|| {
            // One level of `{"ingredient": {…}}` unwrapping — Mekanism/
            // Thermal nest the real ingredient under `ingredient`.
            obj.get("ingredient").and_then(|inner| {
                inner.as_str().or_else(|| {
                    inner.as_object().and_then(|o| {
                        o.get("item")
                            .or_else(|| o.get("id"))
                            .or_else(|| o.get("tag"))
                            .and_then(JsonValue::as_str)
                    })
                })
            })
        })?;
    let count = obj
        .get("count")
        .or_else(|| obj.get("amount"))
        .and_then(JsonValue::as_u64)
        .and_then(|v| u32::try_from(v).ok())
        .filter(|&v| v > 0)
        .unwrap_or(1);
    Some(Stack {
        resource: Resource {
            kind: ResourceKind::Item,
            id: ids.intern_item(item_name),
        },
        amount: count,
    })
}

fn merge_stack(list: &mut Vec<Stack>, stack: Stack) {
    if let Some(existing) = list.iter_mut().find(|s| s.resource == stack.resource) {
        existing.amount = existing.amount.saturating_add(stack.amount);
    } else {
        list.push(stack);
    }
}

// ---------------------------------------------------------------------------
// Public bounds-checked JSON AST & parser (reused by profile.rs)
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub enum JsonValue {
    Null,
    Bool(bool),
    Number(f64),
    String(String),
    Array(Vec<JsonValue>),
    Object(BTreeMap<String, JsonValue>),
}

impl JsonValue {
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Self::Bool(b) => Some(*b),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::String(s) => Some(s.as_str()),
            _ => None,
        }
    }

    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Self::Number(n) if n.is_finite() => Some(*n),
            _ => None,
        }
    }

    pub fn as_u64(&self) -> Option<u64> {
        match self {
            Self::Number(n) if *n >= 0.0 && n.is_finite() => Some(*n as u64),
            _ => None,
        }
    }

    pub fn as_array(&self) -> Option<&[JsonValue]> {
        match self {
            Self::Array(a) => Some(a.as_slice()),
            _ => None,
        }
    }

    pub fn as_object(&self) -> Option<&BTreeMap<String, JsonValue>> {
        match self {
            Self::Object(o) => Some(o),
            _ => None,
        }
    }
}

/// Parse a complete JSON document under [`MAX_JSON_BYTES`] /
/// [`MAX_JSON_DEPTH`] via the canonical `megu_core::json` reader, then lower
/// it into this module's [`JsonValue`] shape.
pub fn parse_json(bytes: &[u8]) -> Result<JsonValue, DataLoadError> {
    if bytes.len() > MAX_JSON_BYTES {
        return Err(DataLoadError::Json("JSON exceeds max size".into()));
    }
    let text = std::str::from_utf8(bytes)
        .map_err(|_| DataLoadError::Json("JSON is not valid UTF-8".into()))?;
    let value = megu_core::json::parse_json_with_limits(
        text,
        megu_core::json::JsonLimits {
            max_depth: MAX_JSON_DEPTH,
            max_nodes: MAX_JSON_BYTES,
            max_bytes: MAX_JSON_BYTES,
        },
        false,
    )
    .map_err(|e| DataLoadError::Json(e.to_string()))?;
    Ok(json_value_from_core(value))
}

fn json_value_from_core(value: megu_core::json::JsonValue) -> JsonValue {
    use megu_core::json::JsonValue as Core;
    match value {
        Core::Null => JsonValue::Null,
        Core::Bool(b) => JsonValue::Bool(b),
        Core::Number(n) => JsonValue::Number(n),
        Core::String(s) => JsonValue::String(s),
        Core::Array(items) => {
            JsonValue::Array(items.into_iter().map(json_value_from_core).collect())
        }
        Core::Object(pairs) => JsonValue::Object(
            pairs
                .into_iter()
                .map(|(k, v)| (k, json_value_from_core(v)))
                .collect(),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blockstate_variants_and_multipart_when_feed_property_discovery() {
        let variants = br#"{
          "variants": {
            "axis=x": {"model": "a"},
            "axis=y,waterlogged=false": {"model": "b"},
            "axis=z,waterlogged=true": {"model": "c"}
          }
        }"#;
        let props = parse_blockstate_properties(variants).unwrap();
        assert_eq!(
            props,
            vec![
                PropertyDef::new("axis", &["x", "y", "z"]),
                PropertyDef::new("waterlogged", &["false", "true"]),
            ]
        );

        let multipart = br#"{
          "multipart": [
            {"when": {"north": "true|false"}, "apply": {}},
            {"when": {"OR": [{"up": "true"}, {"down": "true"}]}, "apply": {}}
          ]
        }"#;
        let props = parse_blockstate_properties(multipart).unwrap();
        assert_eq!(
            props,
            vec![
                PropertyDef::new("down", &["true"]),
                PropertyDef::new("north", &["false", "true"]),
                PropertyDef::new("up", &["true"]),
            ]
        );

        // A property-less blockstate still proves the block exists.
        assert_eq!(
            parse_blockstate_properties(br#"{"variants": {"": {"model": "x"}}}"#),
            Some(Vec::new())
        );
        // Non-blockstate JSON is ignored rather than fabricated.
        assert_eq!(parse_blockstate_properties(br#"{"type": "create:pressing"}"#), None);
    }

    /// `(item_name, amount)` projections of a recipe's stacks, for
    /// readable assertions.
    fn stacks_of(stacks: &[Stack], ids: &ItemFluidIdTable) -> Vec<(String, u32)> {
        stacks
            .iter()
            .map(|s| {
                let name = match s.resource.kind {
                    ResourceKind::Item => ids
                        .items
                        .iter()
                        .find(|(_, id)| **id == s.resource.id)
                        .map(|(n, _)| n.clone()),
                    ResourceKind::Fluid => ids
                        .fluids
                        .iter()
                        .find(|(_, id)| **id == s.resource.id)
                        .map(|(n, _)| n.clone()),
                };
                (name.unwrap_or_else(|| "?".to_string()), s.amount)
            })
            .collect()
    }

    #[test]
    fn generic_processing_and_shaped_recipe_schemas_parse() {
        let mut ids = ItemFluidIdTable::new();

        // Real Create 6.x processing recipe: `ingredients` + `results`
        // (with `chance`) + `processing_time` + `heat_requirement`.
        let crushing = parse_mod_recipe_json(
            "create:crushing/iron_ore",
            1,
            br#"{"type":"create:crushing","ingredients":[{"tag":"c:ores/iron"}],
                "processing_time":250,
                "results":[{"id":"create:crushed_raw_iron","chance":1.0},
                           {"id":"minecraft:iron_nugget","count":3,"chance":0.75}]}"#
                .as_slice(),
            &mut ids,
        )
        .unwrap()
        .expect("create:crushing parses");
        assert_eq!(crushing.recipe_type, "create:crushing");
        assert_eq!(crushing.recipe.base_ticks, 250);
        assert_eq!(crushing.recipe.inputs.len(), 1);
        assert_eq!(crushing.recipe.outputs.len(), 2);

        // Heat-gated mixer recipe with a fluid ingredient.
        let mixing = parse_mod_recipe_json(
            "create:mixing/lava",
            2,
            br#"{"type":"create:mixing","heatRequirement":"superheated",
                "ingredients":[{"item":"minecraft:cobblestone"},
                               {"fluid":"minecraft:lava","amount":250}],
                "results":[{"id":"minecraft:obsidian"}]}"#
                .as_slice(),
            &mut ids,
        )
        .unwrap()
        .expect("heated create:mixing parses");
        assert_eq!(mixing.recipe.inputs.len(), 2);

        // Shaped crafting via `pattern`+`key` (`minecraft:crafting_shaped`,
        // `create:mechanical_crafting`): every symbol cell is an input.
        let shaped = parse_mod_recipe_json(
            "create:mechanical_crafting/mechanical_arm",
            3,
            br#"{"type":"create:mechanical_crafting","acceptMirrored":false,
                "key":{"A":{"item":"create:precision_mechanism"},
                       "B":{"item":"create:brass_sheet"}},
                "pattern":["A A","ABA","A A"],
                "result":{"id":"create:mechanical_arm","count":1}}"#
                .as_slice(),
            &mut ids,
        )
        .unwrap()
        .expect("shaped recipe parses");
        let mut inputs = stacks_of(&shaped.recipe.inputs, &ids);
        inputs.sort();
        assert_eq!(
            inputs,
            vec![
                ("create:brass_sheet".to_string(), 1),
                ("create:precision_mechanism".to_string(), 6),
            ]
        );

        // Smithing `template`/`base`/`addition` slots are inputs too.
        let smithing = parse_mod_recipe_json(
            "minecraft:smithing/netherite",
            4,
            br#"{"type":"minecraft:smithing_transform",
                "template":{"item":"minecraft:netherite_upgrade_smithing_template"},
                "base":{"item":"minecraft:diamond_chestplate"},
                "addition":{"item":"minecraft:netherite_ingot"},
                "result":{"id":"minecraft:netherite_chestplate"}}"#
                .as_slice(),
            &mut ids,
        )
        .unwrap()
        .expect("smithing parses");
        assert_eq!(smithing.recipe.inputs.len(), 3);

        // `neoforge:conditional` wraps the real recipe under `recipe`.
        let conditional = parse_mod_recipe_json(
            "example:cond",
            5,
            br#"{"type":"neoforge:conditional",
                "neoforge:conditions":[{"type":"neoforge:mod_loaded","modid":"example"}],
                "recipe":{"type":"example:pressing",
                          "ingredients":[{"item":"example:in"}],
                          "results":[{"id":"example:out"}]}}"#
                .as_slice(),
            &mut ids,
        )
        .unwrap()
        .expect("conditional recipe unwraps");
        assert_eq!(conditional.recipe_type, "example:pressing");

        // `neoforge:compound` ingredient: first parseable child wins.
        let compound = parse_mod_recipe_json(
            "example:compound",
            6,
            br#"{"type":"example:mix",
                "ingredients":[{"type":"neoforge:compound",
                                "children":[{"item":"example:a"},{"item":"example:b"}]}],
                "results":[{"id":"example:out"}]}"#
                .as_slice(),
            &mut ids,
        )
        .unwrap()
        .expect("compound ingredient parses");
        let inputs = stacks_of(&compound.recipe.inputs, &ids);
        assert_eq!(inputs, vec![("example:a".to_string(), 1)]);

        // A lone ingredient object under the plural key still parses.
        let singular = parse_mod_recipe_json(
            "example:single",
            7,
            br#"{"type":"example:press",
                "ingredients":{"item":"example:in"},
                "results":{"id":"example:out","count":2}}"#
                .as_slice(),
            &mut ids,
        )
        .unwrap()
        .expect("non-array ingredients parse");
        assert_eq!(stacks_of(&singular.recipe.outputs, &ids),
            vec![("example:out".to_string(), 2)]);

        // Schemas with no parseable inputs or outputs are still dropped.
        assert!(parse_mod_recipe_json(
            "example:none",
            8,
            br#"{"type":"example:mystery"}"#.as_slice(),
            &mut ids,
        )
        .unwrap()
        .is_none());
    }
}
