// SPDX-License-Identifier: LGPL-3.0-or-later
//! Bounded in-memory readers for vanilla language, credits, fonts, recipes,
//! and tags. Mojang asset bytes are always read from the user's installation.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::{Path, PathBuf};

use crate::vanilla_jar::{AssetLayer, MAX_PACK_LAYERS, prioritized_packs, read_bounded_file};
use megu_core::crypto::sha1_hex;
use megu_core::json::{JsonLimits, JsonValue, parse_json_with_limits};

const MAX_ENTRY_BYTES: usize = 8 * 1024 * 1024;
const MAX_TOTAL_BYTES: usize = 64 * 1024 * 1024;
const MAX_INDEX_BYTES: usize = 32 * 1024 * 1024;
const JSON_LIMITS: JsonLimits = JsonLimits {
    max_depth: 64,
    max_nodes: 500_000,
    max_bytes: MAX_ENTRY_BYTES,
};

fn safe_rel(s: &str) -> bool {
    !s.is_empty()
        && !s.starts_with('/')
        && !s.contains(['\\', ':', '\0'])
        && s.split('/').all(|p| !p.is_empty() && p != "." && p != "..")
}

fn parse_bounded_json(bytes: &[u8]) -> Option<JsonValue> {
    let s = std::str::from_utf8(bytes).ok()?;
    parse_json_with_limits(s, JSON_LIMITS, true).ok()
}

fn exact_u64(value: &JsonValue) -> Option<u64> {
    let n = value.as_f64()?;
    (n.is_finite() && n >= 0.0 && n.fract() == 0.0 && n <= 9_007_199_254_740_991.0)
        .then_some(n as u64)
}

type TextEntries = Vec<(String, Vec<u8>)>;

fn read_layer(
    path: &Path,
    pack: bool,
    select: impl Fn(&str) -> bool,
    remaining: &mut u64,
) -> Result<TextEntries, String> {
    if pack && !path.exists() {
        return Ok(Vec::new());
    }
    let (files, _) =
        match AssetLayer::open(path, pack, select).and_then(|layer| layer.collect(*remaining)) {
            Ok(files) => files,
            Err(_) if pack => return Ok(Vec::new()),
            Err(error) => return Err(error.to_string()),
        };
    *remaining -= files.iter().map(|e| e.bytes.len() as u64).sum::<u64>();
    Ok(files.into_iter().map(|e| (e.name, e.bytes)).collect())
}

fn normalize_locale(locale: &str) -> Option<String> {
    (!locale.is_empty()
        && locale.len() <= 64
        && locale
            .split('_')
            .all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_alphanumeric())))
    .then(|| locale.to_ascii_lowercase())
}

fn language_map(bytes: &[u8]) -> Option<BTreeMap<String, String>> {
    let root = parse_bounded_json(bytes)?;
    let mut out = BTreeMap::new();
    for (k, v) in root.as_object()? {
        out.insert(k.clone(), v.as_str()?.to_owned());
    }
    Some(out)
}

fn asset_index(root: &Path, index_id: &str) -> Option<JsonValue> {
    if !safe_rel(index_id) || index_id.contains('/') {
        return None;
    }
    let index_path = root.join("indexes").join(format!("{index_id}.json"));
    let bytes = read_bounded_file(&index_path, MAX_INDEX_BYTES).ok()?;
    parse_json_with_limits(
        std::str::from_utf8(&bytes).ok()?,
        JsonLimits {
            max_depth: 8,
            max_nodes: 2_000_000,
            max_bytes: MAX_INDEX_BYTES,
        },
        true,
    )
    .ok()
}

fn object_spec<'a>(objects: &'a JsonValue, key: &str) -> Option<(&'a str, u64)> {
    let entry = objects.get(key)?;
    let hash = entry.get_str("hash").filter(|hash| valid_hash(hash))?;
    Some((hash, exact_u64(entry.get("size")?)?))
}

fn read_object(root: &Path, objects: &JsonValue, key: &str) -> Option<Vec<u8>> {
    let (hash, size) = object_spec(objects, key)?;
    if size > MAX_ENTRY_BYTES as u64 {
        return None;
    }
    let obj = root.join("objects").join(&hash[..2]).join(hash);
    let data = read_bounded_file(&obj, size as usize).ok()?;
    (data.len() as u64 == size && sha1_hex(&data) == hash).then_some(data)
}

fn asset_object(root: &Path, index_id: &str, key: &str) -> Option<Vec<u8>> {
    let index = asset_index(root, index_id)?;
    read_object(root, index.get("objects")?, key)
}

// Vanilla accepts trailing commas in its font provider JSON. Remove them
// only outside quoted strings, then apply the normal bounded parser.
fn parse_font_provider(bytes: &[u8]) -> Option<JsonValue> {
    let mut normalized = Vec::with_capacity(bytes.len());
    let mut quoted = false;
    let mut escaped = false;
    for (i, &b) in bytes.iter().enumerate() {
        if quoted {
            if escaped {
                escaped = false;
            } else if b == b'\\' {
                escaped = true;
            } else if b == b'"' {
                quoted = false;
            }
        } else if b == b'"' {
            quoted = true;
        } else if b == b','
            && matches!(
                bytes[i + 1..]
                    .iter()
                    .copied()
                    .find(|b| !b.is_ascii_whitespace()),
                Some(b']' | b'}')
            )
        {
            continue;
        }
        normalized.push(b);
    }
    parse_bounded_json(&normalized)
}

fn has_unihex(root: &Path, index_id: &str) -> bool {
    let Some(index) = asset_index(root, index_id) else {
        return false;
    };
    let Some(objects) = index.get("objects") else {
        return false;
    };
    let Some(provider) = read_object(root, objects, "minecraft/font/include/unifont.json")
        .and_then(|bytes| parse_font_provider(&bytes))
    else {
        return false;
    };
    let Some(providers) = provider.get("providers").and_then(JsonValue::as_array) else {
        return false;
    };
    ["unifont.zip", "unifont_jp.zip"].into_iter().all(|file| {
        let resource = format!("font/{file}");
        providers.iter().any(|provider| {
            provider.get_str("type") == Some("unihex")
                && provider
                    .get_str("hex_file")
                    .is_some_and(|id| id.strip_prefix("minecraft:").unwrap_or(id) == resource)
        }) && object_spec(objects, &format!("minecraft/{resource}")).is_some()
    })
}

/// Vanilla translation table with requested locale, then English fallback.
#[derive(Clone, Debug, Default)]
pub struct VanillaLanguageTable {
    locale: String,
    translations: BTreeMap<String, String>,
}

impl VanillaLanguageTable {
    /// Load the game JAR, optional Mojang assets store, mod JARs and resource
    /// packs. Layers are applied in argument order, last layer wins.
    pub fn load(
        jar: &Path,
        assets_dir: Option<&Path>,
        asset_index: Option<&str>,
        locale: &str,
        mod_jars: &[PathBuf],
        resource_packs: &[PathBuf],
    ) -> Result<Self, String> {
        let extra_layers = mod_jars
            .len()
            .saturating_add(resource_packs.len())
            .saturating_sub(MAX_PACK_LAYERS);
        let locale = normalize_locale(locale).ok_or("invalid locale")?;
        let mut table = Self {
            locale: locale.clone(),
            translations: BTreeMap::new(),
        };
        let mut remaining = MAX_TOTAL_BYTES as u64;
        table.apply_jar_language(jar, &mut remaining)?;
        if let (Some(root), Some(index)) = (assets_dir, asset_index) {
            if let Some(bytes) = asset_object(root, index, &format!("minecraft/lang/{locale}.json"))
            {
                if let Some(m) = language_map(&bytes) {
                    table.translations.extend(m);
                }
            }
        }
        for layer in mod_jars.iter().chain(resource_packs).skip(extra_layers) {
            table.apply_layer(layer, &mut remaining)?;
        }
        Ok(table)
    }

    fn apply_jar_language(&mut self, jar: &Path, remaining: &mut u64) -> Result<(), String> {
        let english = "assets/minecraft/lang/en_us.json";
        let localized = format!("assets/minecraft/lang/{}.json", self.locale);
        let mut files = read_layer(jar, false, |n| n == english || n == localized, remaining)?;
        files.sort_by_key(|(path, _)| path != english);
        for (_, bytes) in files {
            if let Some(m) = language_map(&bytes) {
                self.translations.extend(m);
            }
        }
        Ok(())
    }

    fn apply_layer(&mut self, layer: &Path, remaining: &mut u64) -> Result<(), String> {
        let locale = format!("{}.json", self.locale);
        let files = read_layer(
            layer,
            true,
            |n| {
                n.starts_with("assets/")
                    && n.split_once("/lang/")
                        .is_some_and(|(_, file)| file == "en_us.json" || file == locale)
            },
            remaining,
        )?;
        let mut english = Vec::new();
        let mut localized = Vec::new();
        for (name, bytes) in files {
            let Some((ns, file)) = name
                .strip_prefix("assets/")
                .and_then(|s| s.split_once("/lang/"))
            else {
                continue;
            };
            if !safe_rel(ns) || ns.contains('/') {
                continue;
            }
            if file == "en_us.json" && self.locale != "en_us" {
                english.push((ns.to_owned(), bytes));
            } else if file == locale || file == "en_us.json" {
                localized.push((ns.to_owned(), bytes));
            }
        }
        for (_, b) in english.into_iter().chain(localized) {
            if let Some(m) = language_map(&b) {
                self.translations.extend(m);
            }
        }
        Ok(())
    }

    /// Requested locale (or English fallback) translation.
    pub fn translate(&self, key: &str) -> Option<&str> {
        self.translations.get(key).map(String::as_str)
    }

    /// Format Vanilla `%s`, indexed `%1$s`, and escaped `%%` placeholders.
    pub fn format(&self, key: &str, args: &[&str]) -> String {
        let Some(src) = self.translate(key) else {
            return key.to_owned();
        };
        let mut out = String::with_capacity(src.len());
        let b = src.as_bytes();
        let mut i = 0;
        let mut sequential = 0;
        while i < b.len() {
            if b[i] != b'%' {
                let c = src[i..].chars().next().unwrap_or('\u{fffd}');
                out.push(c);
                i += c.len_utf8();
                continue;
            }
            if b.get(i + 1) == Some(&b'%') {
                out.push('%');
                i += 2;
                continue;
            }
            let mut j = i + 1;
            let mut idx = None;
            let digits = j;
            while j < b.len() && b[j].is_ascii_digit() {
                j += 1;
            }
            if j > digits && b.get(j) == Some(&b'$') {
                idx = std::str::from_utf8(&b[digits..j])
                    .ok()
                    .and_then(|s| s.parse::<usize>().ok())
                    .and_then(|v| v.checked_sub(1));
                j += 1;
            }
            if b.get(j) == Some(&b's') {
                let a = idx.unwrap_or_else(|| {
                    let x = sequential;
                    sequential += 1;
                    x
                });
                if let Some(value) = args.get(a) {
                    out.push_str(value);
                } else {
                    out.push_str(&src[i..=j]);
                }
                i = j + 1;
            } else {
                out.push('%');
                i += 1;
            }
        }
        out
    }

    fn human_fallback(id: &str) -> String {
        let path = id.rsplit_once(':').map(|(_, p)| p).unwrap_or(id);
        path.split(['_', '/', '.'])
            .filter(|s| !s.is_empty())
            .map(|s| {
                let mut c = s.chars();
                match c.next() {
                    Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
                    None => String::new(),
                }
            })
            .collect::<Vec<_>>()
            .join(" ")
    }
    pub fn item_display_name(&self, id: &str) -> String {
        self.translate(&format!("item.{}.{}", ns(id), path(id)))
            .or_else(|| self.translate(&format!("block.{}.{}", ns(id), path(id))))
            .map(str::to_owned)
            .unwrap_or_else(|| Self::human_fallback(id))
    }
    pub fn block_display_name(&self, id: &str) -> String {
        self.translate(&format!("block.{}.{}", ns(id), path(id)))
            .map(str::to_owned)
            .unwrap_or_else(|| Self::human_fallback(id))
    }
    pub fn menu_title(&self, key: &str) -> String {
        self.translate(key)
            .map(str::to_owned)
            .unwrap_or_else(|| Self::human_fallback(key))
    }
}

fn ns(id: &str) -> &str {
    id.split_once(':').map(|p| p.0).unwrap_or("minecraft")
}
fn path(id: &str) -> &str {
    id.split_once(':').map(|p| p.1).unwrap_or(id)
}

/// Vanilla credits and font resources. PNGs remain compressed bytes owned by
/// the caller; no Mojang asset is copied to the repository.
#[derive(Clone, Debug, Default)]
pub struct VanillaTextsAndFonts {
    pub end_text: Option<String>,
    pub postcredits_text: Option<String>,
    pub font_default_json: Option<String>,
    pub nonlatin_european_png: Option<Vec<u8>>,
    pub accented_png: Option<Vec<u8>>,
    pub font_provider_count: usize,
    pub has_unihex_in_assets_index: bool,
}

impl VanillaTextsAndFonts {
    pub fn load(
        jar: &Path,
        assets_dir: Option<&Path>,
        asset_index: Option<&str>,
        resource_packs: &[PathBuf],
    ) -> Result<Self, String> {
        let wanted = |n: &str| {
            matches!(
                n,
                "assets/minecraft/texts/end.txt"
                    | "assets/minecraft/texts/postcredits.txt"
                    | "assets/minecraft/font/default.json"
                    | "assets/minecraft/textures/font/nonlatin_european.png"
                    | "assets/minecraft/textures/font/accented.png"
            )
        };
        let mut map = BTreeMap::new();
        let mut remaining = MAX_TOTAL_BYTES as u64;
        for (n, b) in read_layer(jar, false, wanted, &mut remaining)? {
            map.insert(n, b);
        }
        for p in prioritized_packs(resource_packs) {
            for (n, b) in read_layer(p, true, wanted, &mut remaining)? {
                map.insert(n, b);
            }
        }
        let end_text = map
            .get("assets/minecraft/texts/end.txt")
            .and_then(|b| String::from_utf8(b.clone()).ok());
        let postcredits_text = map
            .get("assets/minecraft/texts/postcredits.txt")
            .and_then(|b| String::from_utf8(b.clone()).ok());
        let font_default_json = map
            .get("assets/minecraft/font/default.json")
            .and_then(|b| parse_bounded_json(b).map(|_| String::from_utf8_lossy(b).into_owned()));
        let font_provider_count = font_default_json
            .as_deref()
            .and_then(|s| parse_bounded_json(s.as_bytes()))
            .and_then(|s| {
                s.get("providers")
                    .and_then(JsonValue::as_array)
                    .map(|a| a.len())
            })
            .unwrap_or(0);
        let nonlatin_european_png =
            map.remove("assets/minecraft/textures/font/nonlatin_european.png");
        let accented_png = map.remove("assets/minecraft/textures/font/accented.png");
        let mut out = Self {
            end_text,
            postcredits_text,
            font_default_json,
            nonlatin_european_png,
            accented_png,
            font_provider_count,
            ..Self::default()
        };
        if let (Some(root), Some(index)) = (assets_dir, asset_index) {
            out.has_unihex_in_assets_index = has_unihex(root, index);
        }
        Ok(out)
    }
}
fn valid_hash(h: &str) -> bool {
    h.len() == 40
        && h.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Recipe variant recognized from the Vanilla recipe type identifier.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VanillaRecipeKind {
    CraftingShaped,
    CraftingShapeless,
    Smelting,
    Blasting,
    Smoking,
    CampfireCooking,
    Stonecutting,
    SmithingTransform,
    SmithingTrim,
    Special,
    Other,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VanillaIngredient {
    pub items: Vec<String>,
    pub tags: Vec<String>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VanillaRecipeResult {
    pub item: String,
    pub count: u32,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VanillaRecipeEntry {
    pub id: String,
    pub recipe_type: String,
    pub kind: VanillaRecipeKind,
    pub group: Option<String>,
    pub category: Option<String>,
    pub ingredients: Vec<VanillaIngredient>,
    pub result: Option<VanillaRecipeResult>,
}

fn ingredient(v: &JsonValue) -> VanillaIngredient {
    let mut out = VanillaIngredient {
        items: Vec::new(),
        tags: Vec::new(),
    };
    let vals = v.as_array().unwrap_or(std::slice::from_ref(v));
    for x in vals {
        if let Some(id) = x.get_str("item") {
            out.items.push(id.to_owned());
        }
        if let Some(tag) = x.get_str("tag") {
            out.tags.push(tag.to_owned());
        }
    }
    out
}
fn kind(ty: &str) -> VanillaRecipeKind {
    match ty.rsplit(':').next().unwrap_or(ty) {
        "crafting_shaped" => VanillaRecipeKind::CraftingShaped,
        "crafting_shapeless" => VanillaRecipeKind::CraftingShapeless,
        "smelting" => VanillaRecipeKind::Smelting,
        "blasting" => VanillaRecipeKind::Blasting,
        "smoking" => VanillaRecipeKind::Smoking,
        "campfire_cooking" => VanillaRecipeKind::CampfireCooking,
        "stonecutting" => VanillaRecipeKind::Stonecutting,
        "smithing_transform" => VanillaRecipeKind::SmithingTransform,
        "smithing_trim" => VanillaRecipeKind::SmithingTrim,
        s if s.starts_with("crafting_special_") => VanillaRecipeKind::Special,
        _ => VanillaRecipeKind::Other,
    }
}
fn parse_recipe(id: String, b: &[u8]) -> Option<VanillaRecipeEntry> {
    let v = parse_bounded_json(b)?;
    let ty = v.get_str("type")?.to_owned();
    let mut ingredients = Vec::new();
    if let Some(arr) = v.get("ingredients").and_then(JsonValue::as_array) {
        ingredients.extend(arr.iter().map(ingredient));
    }
    let recipe_kind = kind(&ty);
    if recipe_kind == VanillaRecipeKind::CraftingShaped {
        if let (Some(pattern), Some(key_map)) = (
            v.get("pattern").and_then(JsonValue::as_array),
            v.get("key").and_then(JsonValue::as_object),
        ) {
            for row in pattern.iter().filter_map(JsonValue::as_str) {
                for symbol in row.chars() {
                    if symbol.is_whitespace() {
                        continue;
                    }
                    let mut symbol_key = [0u8; 4];
                    let symbol = symbol.encode_utf8(&mut symbol_key).as_bytes();
                    let Ok(symbol) = std::str::from_utf8(symbol) else {
                        continue;
                    };
                    if let Some((_, x)) = key_map.iter().find(|(key, _)| key == symbol) {
                        ingredients.push(ingredient(x));
                    }
                }
            }
        }
    } else if let Some(obj) = v.get("key").and_then(JsonValue::as_object) {
        for (_, x) in obj {
            ingredients.push(ingredient(x));
        }
    }
    for key in ["ingredient", "template", "base", "addition"] {
        if let Some(x) = v.get(key) {
            ingredients.push(ingredient(x));
        }
    }
    let result = v.get("result").and_then(|r| {
        if let Some(item) = r.as_str() {
            return Some(VanillaRecipeResult {
                item: item.to_owned(),
                count: 1,
            });
        }
        let item = r.get_str("id").or_else(|| r.get_str("item"))?.to_owned();
        let count = r
            .get("count")
            .and_then(exact_u64)
            .unwrap_or(1)
            .min(u32::MAX as u64) as u32;
        Some(VanillaRecipeResult { item, count })
    });
    Some(VanillaRecipeEntry {
        id,
        kind: recipe_kind,
        recipe_type: ty,
        group: v.get_str("group").map(str::to_owned),
        category: v.get_str("category").map(str::to_owned),
        ingredients,
        result,
    })
}

/// In-memory recipes plus recursively resolvable item and block tags.
#[derive(Clone, Debug, Default)]
pub struct VanillaJarDataCatalog {
    pub recipes: BTreeMap<String, VanillaRecipeEntry>,
    item_tags: BTreeMap<String, Vec<String>>,
    block_tags: BTreeMap<String, Vec<String>>,
}

impl VanillaJarDataCatalog {
    /// Read vanilla JAR first, then overlay archives/directories in order.
    pub fn load(jar: &Path, overlays: &[PathBuf]) -> Result<Self, String> {
        let mut c = Self::default();
        let mut remaining = MAX_TOTAL_BYTES as u64;
        c.apply_layer(jar, false, &mut remaining)?;
        for p in prioritized_packs(overlays) {
            c.apply_layer(p, true, &mut remaining)?;
        }
        Ok(c)
    }
    fn apply_layer(&mut self, p: &Path, pack: bool, remaining: &mut u64) -> Result<(), String> {
        let files = read_layer(
            p,
            pack,
            |n| {
                (n.starts_with("data/")
                    && (n.contains("/recipe/")
                        || n.contains("/recipes/")
                        || n.contains("/tags/item/")
                        || n.contains("/tags/items/")
                        || n.contains("/tags/block/")
                        || n.contains("/tags/blocks/")))
                    && n.ends_with(".json")
            },
            remaining,
        )?;
        for (name, bytes) in files {
            let Some(rest) = name.strip_prefix("data/") else {
                continue;
            };
            let Some((namespace, tail)) = rest.split_once('/') else {
                continue;
            };
            if !safe_rel(namespace) || namespace.contains('/') {
                continue;
            }
            if let Some(file) = tail
                .strip_prefix("recipe/")
                .or_else(|| tail.strip_prefix("recipes/"))
            {
                let stem = file.strip_suffix(".json").unwrap_or(file);
                if safe_rel(stem) {
                    let id = format!("{namespace}:{stem}");
                    if let Some(r) = parse_recipe(id.clone(), &bytes) {
                        self.recipes.insert(id, r);
                    }
                }
            } else if let Some(tag_tail) = tail.strip_prefix("tags/") {
                if let Some((dir, file)) = tag_tail.split_once('/') {
                    if !["item", "items", "block", "blocks"].contains(&dir) {
                        continue;
                    }
                    let is_item = dir == "item" || dir == "items";
                    let Some(rel) = file.strip_suffix(".json") else {
                        continue;
                    };
                    if !safe_rel(rel) {
                        continue;
                    }
                    let id = format!("{namespace}:{rel}");
                    if let Some((replace, values)) = parse_tag(&bytes) {
                        let tags = if is_item {
                            &mut self.item_tags
                        } else {
                            &mut self.block_tags
                        };
                        let vals = tags.entry(id).or_default();
                        if replace {
                            vals.clear();
                        }
                        for value in values {
                            if let Some(remove) = value.strip_prefix('!') {
                                vals.retain(|existing| existing != remove);
                            } else if !vals.contains(&value) {
                                vals.push(value);
                            }
                        }
                    }
                }
            }
        }
        Ok(())
    }
    pub fn resolve_item_tag(&self, tag: &str) -> Vec<String> {
        resolve_tag(tag, &self.item_tags)
    }
    pub fn resolve_block_tag(&self, tag: &str) -> Vec<String> {
        resolve_tag(tag, &self.block_tags)
    }
    pub fn recipes_for_result(&self, item_id: &str) -> Vec<&VanillaRecipeEntry> {
        self.recipes
            .values()
            .filter(|r| r.result.as_ref().is_some_and(|x| x.item == item_id))
            .collect()
    }
    pub fn recipes_using_item(&self, item_id: &str) -> Vec<&VanillaRecipeEntry> {
        self.recipes
            .values()
            .filter(|r| {
                r.ingredients.iter().any(|i| {
                    i.items.iter().any(|x| x == item_id)
                        || i.tags
                            .iter()
                            .any(|t| self.resolve_item_tag(t).iter().any(|x| x == item_id))
                })
            })
            .collect()
    }
}

fn normalize_tag_entry_id(id: &str) -> Option<String> {
    let (negative, rest) = id
        .strip_prefix('!')
        .map_or((false, id), |rest| (true, rest));
    let (tag, rest) = rest
        .strip_prefix('#')
        .map_or((false, rest), |rest| (true, rest));
    if !valid_id(rest) {
        return None;
    }
    let normalized = if rest.contains(':') {
        rest.to_owned()
    } else {
        format!("minecraft:{rest}")
    };
    Some(format!(
        "{}{}{}",
        if negative { "!" } else { "" },
        if tag { "#" } else { "" },
        normalized
    ))
}

fn parse_tag(bytes: &[u8]) -> Option<(bool, Vec<String>)> {
    let v = parse_bounded_json(bytes)?;
    let replace = v.get_bool("replace", false);
    let vals = v.get("values")?.as_array()?;
    let mut out = Vec::new();
    for x in vals {
        let id = match x {
            JsonValue::String(s) => s.as_str(),
            JsonValue::Object(_) => x.get_str("id")?,
            _ => return None,
        };
        // `required:false` controls whether a missing optional reference is
        // tolerated; it is not permission to erase a present entry. Preserve
        // every syntactically valid entry and canonicalize namespaceless ids.
        if let Some(normalized) = normalize_tag_entry_id(id) {
            out.push(normalized);
        }
    }
    Some((replace, out))
}
fn valid_id(s: &str) -> bool {
    let (n, p) = s.split_once(':').unwrap_or(("minecraft", s));
    safe_rel(n) && !n.contains('/') && safe_rel(p)
}
fn resolve_tag(tag: &str, tags: &BTreeMap<String, Vec<String>>) -> Vec<String> {
    let root = tag.strip_prefix('#').unwrap_or(tag);
    let root = if root.contains(':') {
        root.to_owned()
    } else {
        format!("minecraft:{root}")
    };
    let mut out = BTreeSet::new();
    let mut active = HashSet::new();
    let mut done = HashSet::new();
    let mut stack = vec![(root, false)];
    while let Some((key, exit)) = stack.pop() {
        if exit {
            active.remove(&key);
            done.insert(key);
            continue;
        }
        if done.contains(&key) || !active.insert(key.clone()) {
            continue;
        }
        stack.push((key.clone(), true));
        if let Some(vals) = tags.get(&key) {
            for val in vals.iter().rev() {
                if let Some(next) = val.strip_prefix('#') {
                    let id = if next.contains(':') {
                        next.to_owned()
                    } else {
                        format!("minecraft:{next}")
                    };
                    if !active.contains(&id) {
                        stack.push((id, false));
                    }
                } else {
                    out.insert(val.clone());
                }
            }
        }
    }
    out.into_iter().collect()
}
