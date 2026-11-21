// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later

//! Universal zero-JVM, zero-dependency JSON & stored-ZIP/JAR loader for ANY
//! mod's recipes, tags, and declarative offload profiles.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use mod_offload::recipe_processor::{Recipe, Resource, ResourceKind, Stack};

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
    let data_dir = root.join("data");
    if data_dir.is_dir() {
        collect_json_files(&data_dir, root, &mut files)?;
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
        if name.starts_with("data/") && name.ends_with(".json") {
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
/// Recognizes standard `ingredients` / `ingredient` / `input` and `results` /
/// `result` / `output` schemas used across Vanilla, Create, Mekanism, Thermal,
/// and AE2.
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
    let Some(recipe_type) = obj.get("type").and_then(JsonValue::as_str) else {
        return Ok(None);
    };

    let mut inputs = Vec::new();
    if let Some(arr) = obj.get("ingredients").and_then(JsonValue::as_array) {
        for item in arr {
            if let Some(stack) = parse_stack(item, ids) {
                merge_stack(&mut inputs, stack);
            }
        }
    }
    for key in ["ingredient", "input", "item_input", "fluid_input"] {
        if let Some(val) = obj.get(key) {
            if let Some(stack) = parse_stack(val, ids) {
                merge_stack(&mut inputs, stack);
            }
        }
    }

    let mut outputs = Vec::new();
    if let Some(arr) = obj.get("results").and_then(JsonValue::as_array) {
        for item in arr {
            if let Some(stack) = parse_stack(item, ids) {
                merge_stack(&mut outputs, stack);
            }
        }
    }
    for key in ["result", "output", "main_output"] {
        if let Some(val) = obj.get(key) {
            if let Some(stack) = parse_stack(val, ids) {
                merge_stack(&mut outputs, stack);
            }
        }
    }
    if inputs.is_empty() || outputs.is_empty() {
        return Ok(None);
    }

    let base_ticks = obj
        .get("processing_time")
        .or_else(|| obj.get("processingTime"))
        .or_else(|| obj.get("cookingtime"))
        .or_else(|| obj.get("energy"))
        .and_then(JsonValue::as_u64)
        .and_then(|v| u32::try_from(v).ok())
        .filter(|&v| v > 0)
        .unwrap_or(DEFAULT_PROCESSING_TICKS);

    Ok(Some(ParsedModRecipe {
        name: recipe_name.to_string(),
        recipe_type: recipe_type.to_string(),
        recipe: Recipe {
            id: recipe_id,
            base_ticks,
            inputs,
            outputs,
        },
    }))
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
    if parts.len() < 4 || parts[0] != "data" {
        return Ok(());
    }
    let ns = parts[1];
    let category = parts[2];
    let stem_path = parts[3..].join("/");
    let stem = stem_path.strip_suffix(".json").unwrap_or(&stem_path);

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
    if let Some(fluid_name) = obj.get("fluid").and_then(JsonValue::as_str) {
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
        .and_then(JsonValue::as_str)?;
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

struct JsonParser<'a> {
    src: &'a [u8],
    pos: usize,
    depth: usize,
}

pub fn parse_json(bytes: &[u8]) -> Result<JsonValue, DataLoadError> {
    if bytes.len() > MAX_JSON_BYTES {
        return Err(DataLoadError::Json("JSON exceeds max size".into()));
    }
    let mut parser = JsonParser {
        src: bytes,
        pos: 0,
        depth: 0,
    };
    let val = parser.parse_value()?;
    parser.skip_ws();
    if parser.pos != parser.src.len() {
        return Err(DataLoadError::Json(
            "trailing bytes after JSON value".into(),
        ));
    }
    Ok(val)
}

impl<'a> JsonParser<'a> {
    fn skip_ws(&mut self) {
        while let Some(&b) = self.src.get(self.pos) {
            if matches!(b, b' ' | b'\t' | b'\r' | b'\n') {
                self.pos += 1;
            } else {
                break;
            }
        }
    }

    fn peek(&self) -> Option<u8> {
        self.src.get(self.pos).copied()
    }

    fn parse_value(&mut self) -> Result<JsonValue, DataLoadError> {
        self.skip_ws();
        if self.depth >= MAX_JSON_DEPTH {
            return Err(DataLoadError::Json("max JSON nesting exceeded".into()));
        }
        match self.peek() {
            Some(b'{') => self.parse_object(),
            Some(b'[') => self.parse_array(),
            Some(b'"') => self.parse_string().map(JsonValue::String),
            Some(b't') => self.expect_literal(b"true", JsonValue::Bool(true)),
            Some(b'f') => self.expect_literal(b"false", JsonValue::Bool(false)),
            Some(b'n') => self.expect_literal(b"null", JsonValue::Null),
            Some(b'-' | b'0'..=b'9') => self.parse_number(),
            _ => Err(DataLoadError::Json(format!(
                "unexpected token at {}",
                self.pos
            ))),
        }
    }

    fn expect_literal(&mut self, lit: &[u8], val: JsonValue) -> Result<JsonValue, DataLoadError> {
        if self.src.get(self.pos..self.pos + lit.len()) == Some(lit) {
            self.pos += lit.len();
            Ok(val)
        } else {
            Err(DataLoadError::Json(format!(
                "invalid literal at {}",
                self.pos
            )))
        }
    }

    fn parse_object(&mut self) -> Result<JsonValue, DataLoadError> {
        self.pos += 1;
        self.depth += 1;
        let mut map = BTreeMap::new();
        self.skip_ws();
        if self.peek() == Some(b'}') {
            self.pos += 1;
            self.depth -= 1;
            return Ok(JsonValue::Object(map));
        }
        loop {
            self.skip_ws();
            let key = self.parse_string()?;
            self.skip_ws();
            if self.peek() != Some(b':') {
                return Err(DataLoadError::Json("expected ':' in object".into()));
            }
            self.pos += 1;
            let value = self.parse_value()?;
            map.insert(key, value);
            self.skip_ws();
            match self.peek() {
                Some(b',') => self.pos += 1,
                Some(b'}') => {
                    self.pos += 1;
                    self.depth -= 1;
                    return Ok(JsonValue::Object(map));
                }
                _ => return Err(DataLoadError::Json("expected ',' or '}' in object".into())),
            }
        }
    }

    fn parse_array(&mut self) -> Result<JsonValue, DataLoadError> {
        self.pos += 1;
        self.depth += 1;
        let mut arr = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b']') {
            self.pos += 1;
            self.depth -= 1;
            return Ok(JsonValue::Array(arr));
        }
        loop {
            let value = self.parse_value()?;
            arr.push(value);
            self.skip_ws();
            match self.peek() {
                Some(b',') => self.pos += 1,
                Some(b']') => {
                    self.pos += 1;
                    self.depth -= 1;
                    return Ok(JsonValue::Array(arr));
                }
                _ => return Err(DataLoadError::Json("expected ',' or ']' in array".into())),
            }
        }
    }

    fn parse_string(&mut self) -> Result<String, DataLoadError> {
        if self.peek() != Some(b'"') {
            return Err(DataLoadError::Json("expected string quote".into()));
        }
        self.pos += 1;
        let mut out = String::new();
        while let Some(&b) = self.src.get(self.pos) {
            self.pos += 1;
            match b {
                b'"' => return Ok(out),
                b'\\' => {
                    let esc = *self
                        .src
                        .get(self.pos)
                        .ok_or_else(|| DataLoadError::Json("unterminated escape".into()))?;
                    self.pos += 1;
                    match esc {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'b' => out.push('\u{0008}'),
                        b'f' => out.push('\u{000C}'),
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        b'u' => {
                            let hex = self.src.get(self.pos..self.pos + 4).ok_or_else(|| {
                                DataLoadError::Json("short unicode escape".into())
                            })?;
                            self.pos += 4;
                            let hex_str = std::str::from_utf8(hex).map_err(|_| {
                                DataLoadError::Json("invalid unicode escape".into())
                            })?;
                            let code = u32::from_str_radix(hex_str, 16)
                                .map_err(|_| DataLoadError::Json("invalid hex escape".into()))?;
                            let ch = char::from_u32(code).unwrap_or('\u{FFFD}');
                            out.push(ch);
                        }
                        _ => return Err(DataLoadError::Json("unknown escape".into())),
                    }
                }
                _ if b < 0x80 => out.push(b as char),
                _ => {
                    let start = self.pos - 1;
                    let rest = std::str::from_utf8(&self.src[start..])
                        .map_err(|_| DataLoadError::Json("invalid UTF-8 in string".into()))?;
                    let ch = rest.chars().next().unwrap();
                    out.push(ch);
                    self.pos = start + ch.len_utf8();
                }
            }
        }
        Err(DataLoadError::Json("unterminated string".into()))
    }

    fn parse_number(&mut self) -> Result<JsonValue, DataLoadError> {
        let start = self.pos;
        while let Some(&b) = self.src.get(self.pos) {
            if matches!(b, b'0'..=b'9' | b'-' | b'+' | b'.' | b'e' | b'E') {
                self.pos += 1;
            } else {
                break;
            }
        }
        let s = std::str::from_utf8(&self.src[start..self.pos])
            .map_err(|_| DataLoadError::Json("invalid number UTF-8".into()))?;
        let n = s
            .parse::<f64>()
            .map_err(|_| DataLoadError::Json(format!("invalid number '{s}'")))?;
        Ok(JsonValue::Number(n))
    }
}
