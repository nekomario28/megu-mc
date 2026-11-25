// SPDX-License-Identifier: LGPL-3.0-or-later
//! On-demand access to the remaining official Minecraft 1.21.1 assets.
//!
//! No Mojang bytes are shipped by this module. Call [`VanillaOfficialExtras::load`]
//! with the user's client JAR and assets store; missing or invalid inputs yield
//! an empty/partial catalog and never panic.

use std::collections::BTreeMap;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use megu_client_mesh::png_decode::decode_png_rgba;
use megu_core::json::{parse_json_with_limits, JsonLimits, JsonValue};

use crate::vanilla_jar::read_selected_entries;

const MAX_JSON: usize = 8 * 1024 * 1024;
const MAX_INDEX: usize = 32 * 1024 * 1024;
const MAX_OBJECT: usize = 16 * 1024 * 1024;
const MAX_TOTAL: usize = 96 * 1024 * 1024;
const JSON_LIMITS: JsonLimits = JsonLimits {
    max_depth: 64,
    max_nodes: 500_000,
    max_bytes: MAX_JSON,
};

/// Decoded straight RGBA pixels for an official PNG.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DecodedRgba {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

fn decode(bytes: &[u8]) -> Option<DecodedRgba> {
    if bytes.len() > MAX_OBJECT {
        return None;
    }
    let (width, height, rgba) = decode_png_rgba(bytes).ok()?;
    let expected = (width as usize)
        .checked_mul(height as usize)?
        .checked_mul(4)?;
    (expected == rgba.len() && expected <= MAX_OBJECT * 16).then_some(DecodedRgba {
        width,
        height,
        rgba,
    })
}

fn json(bytes: &[u8], limit: usize) -> Option<JsonValue> {
    if bytes.len() > limit {
        return None;
    }
    let text = std::str::from_utf8(bytes).ok()?;
    parse_json_with_limits(text, JSON_LIMITS, true).ok()
}

fn safe_relative(s: &str) -> bool {
    !s.is_empty()
        && !s.starts_with('/')
        && !s.contains(['\\', ':', '\0'])
        && s.split('/').all(|p| !p.is_empty() && p != "." && p != "..")
}

fn java_uuid_hash(uuid: [u8; 16]) -> i32 {
    let most = u64::from_be_bytes(uuid[..8].try_into().unwrap_or([0; 8]));
    let least = u64::from_be_bytes(uuid[8..].try_into().unwrap_or([0; 8]));
    let v = most ^ least;
    (v ^ (v >> 32)) as u32 as i32
}

const SKINS: [&str; 9] = [
    "alex", "ari", "efe", "kai", "makena", "noor", "steve", "sunny", "zuri",
];

/// Vanilla's stable UUID-derived default skin choice and arm model.
pub fn default_skin_for_uuid(uuid: [u8; 16]) -> (&'static str, bool) {
    let n = java_uuid_hash(uuid).rem_euclid(18) as usize;
    (SKINS[n / 2], n % 2 == 1)
}

/// A decoded window icon in the format accepted by `winit::window::Icon::from_rgba`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WindowIconRgba {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

/// Official built-in resource pack object resolved through the asset index.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BuiltinResourcePack {
    pub id: String,
    pub display_name: String,
    pub object_path: PathBuf,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CreditDiscipline {
    pub name: String,
    pub credits: Vec<String>,
}
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CreditSection {
    pub section: String,
    pub disciplines: Vec<CreditDiscipline>,
}

/// Compact item model lookup. `overrides` maps predicate labels to texture ids.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ItemDisplayTransform {
    pub translation: [f64; 3],
    pub rotation: [f64; 3],
    pub scale: [f64; 3],
}
#[derive(Clone, Debug, Default)]
pub struct ItemModelCatalog {
    textures: BTreeMap<String, String>,
    model_textures: BTreeMap<String, String>,
    overrides: BTreeMap<String, Vec<(String, String)>>,
    transforms: BTreeMap<(String, String), ItemDisplayTransform>,
}

impl ItemModelCatalog {
    /// Resolve an item texture with a named dynamic state (for example `pulling_1`).
    pub fn resolve_item_texture(
        &self,
        item_name: &str,
        predicate_state: Option<&str>,
    ) -> Option<String> {
        let item = item_name.strip_prefix("minecraft:").unwrap_or(item_name);
        if let Some(state) = predicate_state {
            if let Some((_, model)) = self
                .overrides
                .get(item)?
                .iter()
                .find(|(key, _)| key == state)
            {
                if let Some(texture) = self.model_textures.get(model) {
                    return Some(texture.clone());
                }
            }
        }
        self.textures.get(item).cloned()
    }

    /// Return a parsed display transform, defaulting to vanilla's identity transform.
    pub fn display_transform(&self, item_name: &str, context: &str) -> ItemDisplayTransform {
        let item = item_name.strip_prefix("minecraft:").unwrap_or(item_name);
        self.transforms
            .get(&(item.to_owned(), context.to_owned()))
            .cloned()
            .unwrap_or_else(|| ItemDisplayTransform {
                scale: [1.0; 3],
                ..Default::default()
            })
    }
}

/// Lightweight, indexed summaries of the official data-driven registries.
#[derive(Clone, Debug, Default)]
pub struct VanillaRegistryCatalog {
    /// Archive path to parsed JSON for registry and loot preview consumers.
    pub raw: BTreeMap<String, JsonValue>,
    /// Extracted tooltip, screen, and registry fields, keyed by archive path.
    pub summaries: BTreeMap<String, VanillaDataSummary>,
}

/// Common lightweight summary fields used across the 1.21.1 registries.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct VanillaDataSummary {
    pub id: String,
    pub parent: Option<String>,
    pub title_key: Option<String>,
    pub description_key: Option<String>,
    pub icon_item: Option<String>,
    pub frame_type: Option<String>,
    pub sound_event: Option<String>,
    pub length_seconds: Option<f64>,
    pub comparator_output: Option<u64>,
    pub asset_id: Option<String>,
    pub width: Option<u64>,
    pub height: Option<u64>,
    pub wild_texture: Option<String>,
    pub tame_texture: Option<String>,
    pub angry_texture: Option<String>,
    pub biomes: Vec<String>,
    pub translation_key: Option<String>,
    pub asset_name: Option<String>,
    pub ingredient: Option<String>,
    pub template_item: Option<String>,
    pub max_level: Option<u64>,
    pub weight: Option<u64>,
    pub supported_items: Vec<String>,
    pub message_id: Option<String>,
    pub exhaustion: Option<f64>,
    pub scaling: Option<String>,
    pub loot_entry_count: usize,
}

#[derive(Clone, Debug, Default)]
pub struct VanillaOfficialExtras {
    skins: BTreeMap<(String, bool), DecodedRgba>,
    armor: BTreeMap<(String, u8, bool), DecodedRgba>,
    entities: BTreeMap<String, DecodedRgba>,
    gui: BTreeMap<String, DecodedRgba>,
    pub window_icons: Vec<WindowIconRgba>,
    pub builtin_resource_packs: Vec<BuiltinResourcePack>,
    pub credits: Vec<CreditSection>,
    pub item_models: ItemModelCatalog,
    pub registries: VanillaRegistryCatalog,
}

impl VanillaOfficialExtras {
    /// Read a user's installed JAR and assets store. `asset_index` is commonly `17`.
    pub fn load(jar_path: Option<&Path>, assets_root: Option<&Path>, asset_index: &str) -> Self {
        let mut out = Self::default();
        if let Some(jar) = jar_path {
            out.load_jar(jar);
        }
        if let Some(root) = assets_root {
            out.load_indexed_assets(root, asset_index);
        }
        out
    }

    pub fn skin_rgba(&self, name: &str, slim: bool) -> Option<&DecodedRgba> {
        self.skins.get(&(name.to_owned(), slim))
    }
    pub fn armor_layer_rgba(
        &self,
        material: &str,
        layer: u8,
        overlay: bool,
    ) -> Option<&DecodedRgba> {
        self.armor.get(&(material.to_owned(), layer, overlay))
    }
    pub fn entity_variant_rgba(&self, relative_key: &str) -> Option<&DecodedRgba> {
        self.entities
            .get(relative_key.trim_start_matches("textures/entity/"))
    }
    pub fn gui_sprite_rgba(&self, relative_key: &str) -> Option<&DecodedRgba> {
        self.gui
            .get(relative_key.trim_start_matches("textures/gui/sprites/"))
    }

    pub fn resolve_entity_variant_key(entity_name: &str, variant: &str) -> String {
        let entity = entity_name
            .trim_start_matches("textures/entity/")
            .trim_matches('/');
        let variant = variant.trim_matches('/');
        let file = if variant.is_empty() {
            format!("{entity}.png")
        } else if variant.ends_with(".png") {
            variant.to_owned()
        } else if matches!(entity, "horse" | "wolf") && !variant.starts_with(&format!("{entity}_"))
        {
            format!("{entity}_{variant}.png")
        } else {
            format!("{variant}.png")
        };
        if entity.contains('/') || entity.is_empty() {
            file
        } else {
            format!("{entity}/{file}")
        }
    }

    pub fn credits_lines(&self) -> Vec<String> {
        let mut lines = Vec::new();
        for section in &self.credits {
            lines.push(section.section.clone());
            for discipline in &section.disciplines {
                lines.push(discipline.name.clone());
                lines.extend(discipline.credits.iter().cloned());
            }
        }
        lines
    }

    fn load_jar(&mut self, jar: &Path) {
        let (entries, _) = match read_selected_entries(jar, |n| wanted_jar_path(n)) {
            Ok(v) => v,
            Err(_) => return,
        };
        let mut total = 0usize;
        for entry in entries {
            total = match total.checked_add(entry.bytes.len()) {
                Some(v) if v <= MAX_TOTAL => v,
                _ => break,
            };
            let name = entry.name.as_str();
            if name.ends_with(".png") {
                if let Some(image) = decode(&entry.bytes) {
                    if let Some(key) = name.strip_prefix("assets/minecraft/textures/entity/player/")
                    {
                        if let Some((skin, slim)) = skin_key(key) {
                            self.skins.insert((skin.to_owned(), slim), image);
                            continue;
                        }
                    }
                    if let Some(key) = name.strip_prefix("assets/minecraft/textures/models/armor/")
                    {
                        if let Some((material, layer, overlay)) = armor_key(key) {
                            self.armor
                                .insert((material.to_owned(), layer, overlay), image);
                            continue;
                        }
                    }
                    if let Some(key) = name.strip_prefix("assets/minecraft/textures/entity/") {
                        self.entities.insert(key.to_owned(), image);
                        continue;
                    }
                    if let Some(key) = name.strip_prefix("assets/minecraft/textures/gui/sprites/") {
                        self.gui.insert(key.to_owned(), image);
                    }
                }
            } else if name == "assets/minecraft/texts/credits.json" {
                if let Some(value) = json(&entry.bytes, MAX_JSON) {
                    self.credits = parse_credits(&value);
                }
            } else if name.starts_with("assets/minecraft/models/item/") && name.ends_with(".json") {
                if let (Some(file), Some(value)) =
                    (name.rsplit('/').next(), json(&entry.bytes, MAX_JSON))
                {
                    let item = file.trim_end_matches(".json");
                    parse_item_model(&mut self.item_models, item, &value);
                }
            } else if name.starts_with("data/minecraft/") && name.ends_with(".json") {
                if let Some(value) = json(&entry.bytes, MAX_JSON) {
                    let key = name.trim_start_matches("data/minecraft/").to_owned();
                    self.registries
                        .summaries
                        .insert(key.clone(), summarize_data(&key, &value));
                    self.registries.raw.insert(key, value);
                }
            }
        }
    }

    fn load_indexed_assets(&mut self, root: &Path, index_id: &str) {
        if !safe_relative(index_id) || index_id.contains('/') {
            return;
        }
        let index_path = root.join("indexes").join(format!("{index_id}.json"));
        let Some(bytes) = read_bounded(&index_path, MAX_INDEX) else {
            return;
        };
        let Some(index) = json(&bytes, MAX_INDEX) else {
            return;
        };
        let Some(objects) = index.get("objects").and_then(JsonValue::as_object) else {
            return;
        };
        let wanted = [
            "icons/icon_16x16.png",
            "icons/icon_32x32.png",
            "icons/icon_48x48.png",
            "icons/icon_128x128.png",
            "icons/icon_256x256.png",
            "minecraft/resourcepacks/programmer_art.zip",
            "minecraft/resourcepacks/high_contrast.zip",
        ];
        for key in wanted {
            let Some(meta) = objects.iter().find(|(name, _)| name == key).map(|(_, v)| v) else {
                continue;
            };
            let Some(hash) = meta.get_str("hash") else {
                continue;
            };
            if hash.len() != 40 || !hash.bytes().all(|b| b.is_ascii_hexdigit()) {
                continue;
            }
            let object = root.join("objects").join(&hash[..2]).join(hash);
            if key.starts_with("icons/") {
                let Some(b) = read_bounded(&object, MAX_OBJECT) else {
                    continue;
                };
                let Some(image) = decode(&b) else { continue };
                self.window_icons.push(WindowIconRgba {
                    width: image.width,
                    height: image.height,
                    rgba: image.rgba,
                });
            } else if object.is_file() {
                let (id, display_name) = if key.ends_with("programmer_art.zip") {
                    ("programmer_art", "Programmer Art")
                } else {
                    ("high_contrast", "High Contrast")
                };
                self.builtin_resource_packs.push(BuiltinResourcePack {
                    id: id.into(),
                    display_name: display_name.into(),
                    object_path: object,
                });
            }
        }
    }
}

fn read_bounded(path: &Path, limit: usize) -> Option<Vec<u8>> {
    let file = fs::File::open(path).ok()?;
    let mut bytes = Vec::new();
    file.take(limit as u64 + 1).read_to_end(&mut bytes).ok()?;
    (bytes.len() <= limit).then_some(bytes)
}

fn wanted_jar_path(name: &str) -> bool {
    if name.starts_with("data/minecraft/") && name.ends_with(".json") {
        return true;
    }
    if name == "assets/minecraft/texts/credits.json"
        || (name.starts_with("assets/minecraft/models/item/") && name.ends_with(".json"))
    {
        return true;
    }
    if !name.starts_with("assets/minecraft/textures/") || !name.ends_with(".png") {
        return false;
    }
    let p = name.trim_start_matches("assets/minecraft/textures/");
    if p.starts_with("entity/") || p.starts_with("models/armor/") {
        return true;
    }
    if p.starts_with("gui/title/background/panorama_") && p.ends_with(".png") {
        return true;
    }
    if let Some(gui) = p.strip_prefix("gui/sprites/") {
        return [
            "advancements/",
            "gamemodes/",
            "icon/",
            "recipe_book/",
            "spectator/",
        ]
        .iter()
        .any(|prefix| gui.starts_with(prefix))
            || gui == "title/background/panorama_overlay.png"
            || (gui.starts_with("title/background/panorama_") && gui.ends_with(".png"));
    }
    false
}

fn skin_key(key: &str) -> Option<(&str, bool)> {
    let (model, filename) = key.split_once('/')?;
    let name = filename.strip_suffix(".png")?;
    if !SKINS.contains(&name) {
        return None;
    }
    match model {
        "wide" => Some((name, false)),
        "slim" => Some((name, true)),
        _ => None,
    }
}
fn armor_key(key: &str) -> Option<(&str, u8, bool)> {
    let overlay = key.ends_with("_overlay.png");
    let stem = key.strip_suffix(".png")?;
    if overlay {
        let material = stem
            .strip_suffix("_layer_1_overlay")
            .or_else(|| stem.strip_suffix("_layer_2_overlay"))?;
        let layer = if stem.ends_with("_layer_1_overlay") {
            1
        } else {
            2
        };
        return (material == "leather").then_some((material, layer, true));
    }
    let layer = if stem.ends_with("_layer_1") {
        1
    } else if stem.ends_with("_layer_2") {
        2
    } else {
        return None;
    };
    let material = stem
        .strip_suffix("_layer_1")
        .or_else(|| stem.strip_suffix("_layer_2"))?;
    [
        "leather",
        "chainmail",
        "iron",
        "gold",
        "diamond",
        "netherite",
        "turtle",
    ]
    .contains(&material)
    .then_some((material, layer, false))
}

fn strings(value: &JsonValue) -> Vec<String> {
    match value {
        JsonValue::String(s) => vec![s.clone()],
        JsonValue::Array(a) => a.iter().flat_map(strings).collect(),
        _ => Vec::new(),
    }
}
fn parse_credits(value: &JsonValue) -> Vec<CreditSection> {
    let Some(sections) = value.as_array() else {
        return Vec::new();
    };
    sections
        .iter()
        .filter_map(|s| {
            let name = s
                .get_str("section")
                .or_else(|| s.get_str("name"))?
                .to_owned();
            let disciplines = s
                .get("disciplines")
                .and_then(JsonValue::as_array)
                .unwrap_or(&[])
                .iter()
                .filter_map(|d| {
                    let discipline = d
                        .get_str("name")
                        .or_else(|| d.get_str("discipline"))?
                        .to_owned();
                    let credits = d.get("credits").map(strings).unwrap_or_default();
                    Some(CreditDiscipline {
                        name: discipline,
                        credits,
                    })
                })
                .collect();
            Some(CreditSection {
                section: name,
                disciplines,
            })
        })
        .collect()
}
fn translation_field(value: &JsonValue) -> Option<String> {
    value
        .as_str()
        .or_else(|| value.get_str("translate"))
        .map(str::to_owned)
}

fn recursive_entry_count(value: &JsonValue) -> usize {
    match value {
        JsonValue::Object(members) => members
            .iter()
            .map(|(key, value)| usize::from(key == "type") + recursive_entry_count(value))
            .sum(),
        JsonValue::Array(items) => items.iter().map(recursive_entry_count).sum(),
        _ => 0,
    }
}

fn nonnegative_u64(value: Option<&JsonValue>) -> Option<u64> {
    let n = value?.as_f64()?;
    (n.is_finite() && n >= 0.0 && n.fract() == 0.0 && n <= u64::MAX as f64).then_some(n as u64)
}

fn summarize_data(path: &str, value: &JsonValue) -> VanillaDataSummary {
    let display = value.get("display");
    let textures = value.get("assets").or_else(|| value.get("textures"));
    let sound = value.get("sound_event");
    VanillaDataSummary {
        id: path.trim_end_matches(".json").replace('/', ":"),
        parent: value.get_str("parent").map(str::to_owned),
        title_key: value.get("title").and_then(translation_field),
        description_key: value
            .get("description")
            .and_then(translation_field)
            .or_else(|| value.get_str("description_key").map(str::to_owned)),
        icon_item: display
            .and_then(|v| v.get("icon"))
            .and_then(|v| v.get_str("id"))
            .or_else(|| value.get_str("icon"))
            .map(str::to_owned),
        frame_type: display
            .and_then(|v| v.get_str("frame"))
            .or_else(|| value.get_str("frame"))
            .map(str::to_owned),
        sound_event: sound
            .and_then(|v| v.get_str("sound_id").or_else(|| v.get_str("id")))
            .or_else(|| sound.and_then(JsonValue::as_str))
            .map(str::to_owned),
        length_seconds: value.get("length_in_seconds").and_then(JsonValue::as_f64),
        comparator_output: nonnegative_u64(value.get("comparator_output")),
        asset_id: value.get_str("asset_id").map(str::to_owned),
        width: nonnegative_u64(value.get("width")),
        height: nonnegative_u64(value.get("height")),
        wild_texture: textures.and_then(|v| v.get_str("wild")).map(str::to_owned),
        tame_texture: textures.and_then(|v| v.get_str("tame")).map(str::to_owned),
        angry_texture: textures.and_then(|v| v.get_str("angry")).map(str::to_owned),
        biomes: value.get("biomes").map(strings).unwrap_or_default(),
        translation_key: value.get_str("translation_key").map(str::to_owned),
        asset_name: value.get_str("asset_name").map(str::to_owned),
        ingredient: value
            .get("ingredient")
            .and_then(JsonValue::as_str)
            .map(str::to_owned),
        template_item: value
            .get("template_item")
            .and_then(JsonValue::as_str)
            .map(str::to_owned),
        max_level: nonnegative_u64(value.get("max_level")),
        weight: nonnegative_u64(value.get("weight")),
        supported_items: value
            .get("supported_items")
            .map(strings)
            .unwrap_or_default(),
        message_id: value.get_str("message_id").map(str::to_owned),
        exhaustion: value.get("exhaustion").and_then(JsonValue::as_f64),
        scaling: value.get_str("scaling").map(str::to_owned),
        loot_entry_count: recursive_entry_count(value),
    }
}

fn vector3(value: Option<&JsonValue>, default: [f64; 3]) -> [f64; 3] {
    let Some(a) = value.and_then(JsonValue::as_array) else {
        return default;
    };
    if a.len() != 3 {
        return default;
    }
    let mut out = [0.0; 3];
    for (i, v) in a.iter().enumerate() {
        out[i] = v.as_f64().filter(|n| n.is_finite()).unwrap_or(default[i]);
    }
    out
}
fn parse_item_model(catalog: &mut ItemModelCatalog, item: &str, value: &JsonValue) {
    if let Some(textures) = value.get("textures").and_then(JsonValue::as_object) {
        if let Some((_, texture)) = textures.iter().find(|(key, _)| key == "layer0") {
            if let Some(s) = texture.as_str() {
                catalog.textures.insert(item.to_owned(), s.to_owned());
                catalog
                    .model_textures
                    .insert(format!("minecraft:item/{item}"), s.to_owned());
            }
        }
    }
    if let Some(overrides) = value.get("overrides").and_then(JsonValue::as_array) {
        let mut parsed = Vec::new();
        for over in overrides {
            let Some(predicates) = over.get("predicate").and_then(JsonValue::as_object) else {
                continue;
            };
            let Some(model) = over.get_str("model") else {
                continue;
            };
            for (predicate, value) in predicates {
                if let Some(n) = value.as_f64() {
                    let state = match (item, predicate.as_str(), n) {
                        ("bow", "pull", n) if n >= 1.0 => {
                            Some(format!("pulling_{}", (n as usize - 1).min(2)))
                        }
                        ("crossbow", "pull", n) if n > 0.0 => {
                            Some(format!("pulling_{}", if n >= 1.0 { 2 } else { 1 }))
                        }
                        ("crossbow", "charged", _) => Some("arrow".into()),
                        ("crossbow", "firework", _) => Some("firework".into()),
                        ("clock", "time", n) => {
                            Some(format!("clock_{:02}", ((n * 64.0) as usize).min(63)))
                        }
                        ("compass", "angle", n) => {
                            Some(format!("compass_{:02}", ((n * 32.0) as usize).min(31)))
                        }
                        ("fishing_rod", "cast", _) => Some("cast".into()),
                        ("shield", "blocking", _) => Some("blocking".into()),
                        ("elytra", "broken", _) => Some("broken".into()),
                        _ => None,
                    };
                    if let Some(state) = state {
                        parsed.push((state, model.to_owned()));
                    }
                }
            }
        }
        if !parsed.is_empty() {
            catalog.overrides.insert(item.to_owned(), parsed);
        }
    }
    if let Some(display) = value.get("display").and_then(JsonValue::as_object) {
        for (context, transform) in display {
            let t = ItemDisplayTransform {
                translation: vector3(transform.get("translation"), [0.0; 3]),
                rotation: vector3(transform.get("rotation"), [0.0; 3]),
                scale: vector3(transform.get("scale"), [1.0; 3]),
            };
            catalog
                .transforms
                .insert((item.to_owned(), context.clone()), t);
        }
    }
}
