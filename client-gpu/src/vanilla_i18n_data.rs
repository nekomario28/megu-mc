// SPDX-License-Identifier: LGPL-3.0-or-later
//! Bounded in-memory readers for vanilla language, credits, fonts, recipes,
//! and tags. Mojang asset bytes are always read from the user's installation.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use megu_core::crypto::sha1_hex;
use megu_core::json::{parse_json_with_limits, JsonLimits, JsonValue};
use megu_core::zip_archive::LazyZipIndex;

const MAX_ARCHIVE_BYTES: u64 = 256 * 1024 * 1024;
const MAX_ARCHIVE_ENTRIES: usize = 50_000;
const MAX_ENTRY_BYTES: usize = 8 * 1024 * 1024;
const MAX_TOTAL_BYTES: usize = 64 * 1024 * 1024;
const MAX_INDEX_BYTES: usize = 32 * 1024 * 1024;
const MAX_ENTRIES: usize = 100_000;
const MAX_LAYERS: usize = 256;
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

fn read_bounded_file(path: &Path, limit: usize) -> Option<Vec<u8>> {
    let file = fs::File::open(path).ok()?;
    let mut bytes = Vec::new();
    file.take(limit as u64 + 1).read_to_end(&mut bytes).ok()?;
    (bytes.len() <= limit).then_some(bytes)
}

fn exact_u64(value: &JsonValue) -> Option<u64> {
    let n = value.as_f64()?;
    (n.is_finite() && n >= 0.0 && n.fract() == 0.0 && n <= 9_007_199_254_740_991.0)
        .then_some(n as u64)
}

fn read_archive(
    path: &Path,
    select: impl Fn(&str) -> bool,
) -> Result<Vec<(String, Vec<u8>)>, String> {
    let size = fs::metadata(path).map_err(|e| e.to_string())?.len();
    if size > MAX_ARCHIVE_BYTES {
        return Err("archive byte limit exceeded".into());
    }
    let index = LazyZipIndex::open_all(path).map_err(|e| e.to_string())?;
    if index.entries().len() > MAX_ARCHIVE_ENTRIES {
        return Err("archive entry limit exceeded".into());
    }
    let mut total = 0usize;
    let mut out = Vec::new();
    for meta in index.entries() {
        if !select(&meta.name) || !safe_rel(&meta.name) {
            continue;
        }
        if meta.uncompressed_size > MAX_ENTRY_BYTES as u64 {
            continue;
        }
        let bytes = index
            .read_entry(&meta.name, MAX_ENTRY_BYTES)
            .map_err(|e| e.to_string())?;
        total = total
            .checked_add(bytes.len())
            .ok_or("aggregate entry size overflow")?;
        if total > MAX_TOTAL_BYTES {
            return Err("aggregate entry byte limit exceeded".into());
        }
        out.push((meta.name.clone(), bytes));
    }
    Ok(out)
}

fn read_layer(
    path: &Path,
    select: impl Fn(&str) -> bool + Copy,
) -> Result<Vec<(String, Vec<u8>)>, String> {
    if path.is_dir() {
        let mut out = Vec::new();
        let mut stack = vec![(path.to_path_buf(), 0usize)];
        let mut total = 0usize;
        let mut scanned = 0usize;
        while let Some((dir, depth)) = stack.pop() {
            if depth > 32 {
                return Err("resource directory depth exceeded".into());
            }
            for entry in fs::read_dir(&dir).map_err(|e| e.to_string())? {
                scanned += 1;
                if scanned > MAX_ENTRIES {
                    return Err("resource tree entry limit exceeded".into());
                }
                let entry = entry.map_err(|e| e.to_string())?;
                let ft = entry.file_type().map_err(|e| e.to_string())?;
                if ft.is_symlink() {
                    continue;
                }
                if ft.is_dir() {
                    stack.push((entry.path(), depth + 1));
                    continue;
                }
                if !ft.is_file() {
                    continue;
                }
                let rel = entry
                    .path()
                    .strip_prefix(path)
                    .map_err(|e| e.to_string())?
                    .to_string_lossy()
                    .replace('\\', "/");
                if !safe_rel(&rel) || !select(&rel) {
                    continue;
                }
                if out.len() >= MAX_ENTRIES {
                    return Err("resource entry count limit exceeded".into());
                }
                let len = entry.metadata().map_err(|e| e.to_string())?.len();
                if len > MAX_ENTRY_BYTES as u64 {
                    continue;
                }
                if let Some(bytes) = read_bounded_file(&entry.path(), MAX_ENTRY_BYTES) {
                    total = total
                        .checked_add(bytes.len())
                        .ok_or("aggregate entry size overflow")?;
                    if total > MAX_TOTAL_BYTES {
                        return Err("aggregate entry byte limit exceeded".into());
                    }
                    out.push((rel, bytes));
                }
            }
        }
        out.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(out)
    } else if path.is_file() {
        read_archive(path, select)
    } else {
        Ok(Vec::new())
    }
}

fn normalize_locale(locale: &str) -> Option<String> {
    if locale.len() != 5
        || !locale.as_bytes()[..2].iter().all(u8::is_ascii_alphabetic)
        || locale.as_bytes()[2] != b'_'
        || !locale.as_bytes()[3..].iter().all(u8::is_ascii_alphabetic)
    {
        return None;
    }
    Some(locale.to_ascii_lowercase())
}

fn language_map(bytes: &[u8]) -> Option<BTreeMap<String, String>> {
    let root = parse_bounded_json(bytes)?;
    let mut out = BTreeMap::new();
    for (k, v) in root.as_object()? {
        out.insert(k.clone(), v.as_str()?.to_owned());
    }
    Some(out)
}

fn asset_object(root: &Path, index_id: &str, key: &str) -> Option<Vec<u8>> {
    if !safe_rel(index_id) || index_id.contains('/') || !safe_rel(key) {
        return None;
    }
    let index_path = root.join("indexes").join(format!("{index_id}.json"));
    let bytes = read_bounded_file(&index_path, MAX_INDEX_BYTES)?;
    let json = parse_bounded_json(&bytes)?;
    let entry = json.get("objects")?.get(key)?;
    let hash = entry.get_str("hash")?;
    if hash.len() != 40
        || !hash
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return None;
    }
    let size = exact_u64(entry.get("size")?)?;
    if size > MAX_ENTRY_BYTES as u64 {
        return None;
    }
    let obj = root.join("objects").join(&hash[..2]).join(hash);
    let data = read_bounded_file(&obj, MAX_ENTRY_BYTES)?;
    if data.len() as u64 != size || sha1_hex(&data) != hash {
        return None;
    }
    Some(data)
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
        if mod_jars.len().saturating_add(resource_packs.len()) > MAX_LAYERS {
            return Err("localization layer count limit exceeded".into());
        }
        let locale = normalize_locale(locale).ok_or("invalid locale")?;
        let mut table = Self {
            locale: locale.clone(),
            translations: BTreeMap::new(),
        };
        table.apply_jar_language(jar, "en_us")?;
        if let (Some(root), Some(index)) = (assets_dir, asset_index) {
            if let Some(bytes) = asset_object(root, index, &format!("minecraft/lang/{locale}.json"))
            {
                if let Some(m) = language_map(&bytes) {
                    table.translations.extend(m);
                }
            }
        }
        for layer in mod_jars.iter().chain(resource_packs) {
            table.apply_layer(layer)?;
        }
        Ok(table)
    }

    fn apply_jar_language(&mut self, jar: &Path, locale: &str) -> Result<(), String> {
        let paths = [
            format!("assets/minecraft/lang/{locale}.json"),
            format!("assets/minecraft/lang/{}.json", self.locale),
        ];
        for (_, bytes) in read_archive(jar, |n| paths.iter().any(|p| n == p))? {
            if let Some(m) = language_map(&bytes) {
                self.translations.extend(m);
            }
        }
        Ok(())
    }

    fn apply_layer(&mut self, layer: &Path) -> Result<(), String> {
        let files = read_layer(layer, |n| {
            n.starts_with("assets/") && n.contains("/lang/") && n.ends_with(".json")
        })?;
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
            } else if file == format!("{}.json", self.locale) || file == "en_us.json" {
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
        if resource_packs.len() > MAX_LAYERS {
            return Err("font/text resource layer count limit exceeded".into());
        }
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
        for (n, b) in read_archive(jar, wanted)? {
            map.insert(n, b);
        }
        for p in resource_packs {
            for (n, b) in read_layer(p, wanted)? {
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
            if safe_rel(index) && !index.contains('/') {
                let p = root.join("indexes").join(format!("{index}.json"));
                if let Some(b) = read_bounded_file(&p, MAX_INDEX_BYTES) {
                    if let Some(v) = parse_bounded_json(&b) {
                        out.has_unihex_in_assets_index = v
                            .get("objects")
                            .and_then(JsonValue::as_object)
                            .is_some_and(|o| {
                                o.iter().any(|(k, e)| {
                                    safe_rel(k)
                                        && k.starts_with("minecraft/font/unihex/")
                                        && k.ends_with(".zip")
                                        && e.get_str("hash").is_some_and(valid_hash)
                                })
                            });
                    }
                }
            }
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
    if let Some(obj) = v.get("key").and_then(JsonValue::as_object) {
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
        kind: kind(&ty),
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
        if overlays.len() > MAX_LAYERS {
            return Err("data overlay layer count limit exceeded".into());
        }
        let mut c = Self::default();
        c.apply_layer(jar)?;
        for p in overlays {
            c.apply_layer(p)?;
        }
        Ok(c)
    }
    fn apply_layer(&mut self, p: &Path) -> Result<(), String> {
        let files = read_layer(p, |n| {
            (n.starts_with("data/")
                && (n.contains("/recipe/")
                    || n.contains("/recipes/")
                    || n.contains("/tags/item/")
                    || n.contains("/tags/items/")
                    || n.contains("/tags/block/")
                    || n.contains("/tags/blocks/")))
                && n.ends_with(".json")
        })?;
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
                if !file.contains('/') {
                    let stem = file.strip_suffix(".json").unwrap_or(file);
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

fn parse_tag(bytes: &[u8]) -> Option<(bool, Vec<String>)> {
    let v = parse_bounded_json(bytes)?;
    let replace = v.get_bool("replace", false);
    let vals = v.get("values")?.as_array()?;
    let mut out = Vec::new();
    for x in vals {
        let (id, required) = match x {
            JsonValue::String(s) => (s.as_str(), true),
            JsonValue::Object(_) => (x.get_str("id")?, x.get_bool("required", true)),
            _ => return None,
        };
        if !required {
            continue;
        }
        let checked = id.strip_prefix('!').unwrap_or(id);
        if valid_id(checked.trim_start_matches('#')) {
            out.push(id.to_owned());
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
