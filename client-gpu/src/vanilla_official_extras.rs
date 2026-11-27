// SPDX-License-Identifier: LGPL-3.0-or-later
//! On-demand access to the remaining official Minecraft 1.21.1 assets.
//!
//! No Mojang bytes are shipped by this module. Call [`VanillaOfficialExtras::load`]
//! with the user's client JAR and assets store; missing or invalid inputs yield
//! an empty/partial catalog and never panic.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use megu_core::json::{JsonLimits, JsonValue, parse_json_with_limits};

use crate::vanilla_jar::{
    AssetLayer, MAX_DECODED_RGBA_BYTES, check_png_budget, decode_asset_png, read_bounded_file,
};

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

fn decode(bytes: &[u8], remaining: &mut usize) -> Option<DecodedRgba> {
    check_png_budget(bytes, *remaining).ok()?;
    let (width, height, rgba) = decode_asset_png(bytes).ok()?;
    *remaining -= rgba.len();
    Some(DecodedRgba {
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
    // Vanilla 1.21.1 DefaultPlayerSkin: slots 0..9 are slim alex..zuri,
    // slots 9..18 are the same nine names in the wide model.
    let n = java_uuid_hash(uuid).rem_euclid(18) as usize;
    (SKINS[n % SKINS.len()], n < SKINS.len())
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

/// An item model display transform.
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
    overrides: BTreeMap<String, Vec<ItemOverride>>,
    transforms: BTreeMap<(String, String), ItemDisplayTransform>,
}

#[derive(Clone, Debug)]
struct ItemOverride {
    thresholds: Vec<(String, f64)>,
    model: String,
}

impl ItemModelCatalog {
    /// Resolve an item texture with a named dynamic state (for example `pulling_1`).
    pub fn resolve_item_texture(
        &self,
        item_name: &str,
        predicate_state: Option<&str>,
    ) -> Option<String> {
        let item = item_name.strip_prefix("minecraft:").unwrap_or(item_name);
        let mut properties = [("", 0.0); 2];
        let count = match (item, predicate_state) {
            ("bow" | "crossbow", Some(state @ ("pulling_0" | "pulling_1" | "pulling_2"))) => {
                let pull = match (item, state) {
                    (_, "pulling_0") => 0.0,
                    ("bow", "pulling_1") => 0.65,
                    ("bow", _) => 0.9,
                    (_, "pulling_1") => 0.58,
                    _ => 1.0,
                };
                properties = [("pulling", 1.0), ("pull", pull)];
                2
            }
            ("crossbow", Some("arrow" | "firework")) => {
                properties = [
                    ("charged", 1.0),
                    ("firework", f64::from(predicate_state == Some("firework"))),
                ];
                2
            }
            ("fishing_rod", Some("cast"))
            | ("shield", Some("blocking"))
            | ("elytra", Some("broken")) => {
                properties[0] = (predicate_state.unwrap_or_default(), 1.0);
                1
            }
            ("clock" | "compass", Some(state)) => {
                let index = state
                    .strip_prefix(item)
                    .and_then(|suffix| suffix.strip_prefix('_'))
                    .and_then(|n| n.parse::<u8>().ok());
                let (property, steps) = if item == "clock" {
                    ("time", 64)
                } else {
                    ("angle", 32)
                };
                if let Some(index) = index.filter(|i| *i < steps) {
                    properties[0] = (property, f64::from(index) / f64::from(steps));
                    1
                } else {
                    0
                }
            }
            _ => 0,
        };
        self.resolve_item_texture_with_properties(item, &properties[..count])
    }

    /// Find the last matching Vanilla override. Every threshold in the
    /// override must match, and unspecified predicates have value zero.
    pub fn resolve_item_texture_with_properties(
        &self,
        item: &str,
        properties: &[(&str, f64)],
    ) -> Option<String> {
        let item = item.strip_prefix("minecraft:").unwrap_or(item);
        let property = |key: &str| {
            properties
                .iter()
                .find(|(name, _)| name.strip_prefix("minecraft:").unwrap_or(name) == key)
                .map_or(0.0, |(_, value)| *value)
        };
        self.overrides
            .get(item)
            .and_then(|models| {
                models.iter().rev().find(|model| {
                    model
                        .thresholds
                        .iter()
                        .all(|(name, threshold)| property(name) >= *threshold)
                })
            })
            .and_then(|model| self.model_textures.get(&model.model))
            .or_else(|| self.textures.get(item))
            .cloned()
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
        let mut remaining = MAX_DECODED_RGBA_BYTES;
        if let Some(jar) = jar_path {
            out.load_jar(jar, &mut remaining);
        }
        if let Some(root) = assets_root {
            out.load_indexed_assets(root, asset_index, &mut remaining);
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
        self.gui.get(
            relative_key
                .trim_start_matches("textures/gui/")
                .trim_start_matches("sprites/"),
        )
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

    fn load_jar(&mut self, jar: &Path, remaining: &mut usize) {
        let Ok(mut layer) = AssetLayer::open(jar, false, wanted_jar_path) else {
            return;
        };
        let mut input_left = MAX_TOTAL;
        for name in layer.names().to_vec() {
            let cap = if name.ends_with(".png") {
                MAX_OBJECT
            } else {
                MAX_JSON
            };
            let Ok(bytes) = layer.read(&name, cap.min(input_left)) else {
                continue;
            };
            input_left -= bytes.len();
            let name = name.as_str();
            if name.ends_with(".png") {
                if let Some(image) = decode(&bytes, remaining) {
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
                    if let Some(key) = name.strip_prefix("assets/minecraft/textures/gui/") {
                        self.gui
                            .insert(key.trim_start_matches("sprites/").to_owned(), image);
                    }
                }
            } else if name == "assets/minecraft/texts/credits.json" {
                if let Some(value) = json(&bytes, MAX_JSON) {
                    self.credits = parse_credits(&value);
                }
            } else if let Some((namespace, file)) = item_model_entry(name) {
                if let Some(value) = json(&bytes, MAX_JSON) {
                    // Vanilla keeps its bare item key (`bow`) and model name
                    // (`minecraft:item/bow`) unchanged; a mod namespace is
                    // addressed as `mymod:wrench` / `mymod:item/wrench`.
                    let (item, model_name) = if namespace == "minecraft" {
                        (file.to_owned(), format!("minecraft:item/{file}"))
                    } else {
                        (format!("{namespace}:{file}"), format!("{namespace}:item/{file}"))
                    };
                    parse_item_model(&mut self.item_models, &item, &model_name, &value);
                }
            } else if name.starts_with("data/minecraft/") && name.ends_with(".json") {
                if let Some(value) = json(&bytes, MAX_JSON) {
                    let key = name.trim_start_matches("data/minecraft/").to_owned();
                    self.registries
                        .summaries
                        .insert(key.clone(), summarize_data(&key, &value));
                    self.registries.raw.insert(key, value);
                }
            }
        }
    }

    fn load_indexed_assets(&mut self, root: &Path, index_id: &str, remaining: &mut usize) {
        if !safe_relative(index_id) || index_id.contains('/') || index_id == "." || index_id == ".."
        {
            return;
        }
        let index_path = root.join("indexes").join(format!("{index_id}.json"));
        let Some(bytes) = read_bounded_file(&index_path, MAX_INDEX).ok() else {
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
            let Some(size) = nonnegative_u64(meta.get("size")) else {
                continue;
            };
            if size > MAX_OBJECT as u64 {
                continue;
            }
            let object = root.join("objects").join(&hash[..2]).join(hash);
            let Ok(object_meta) = std::fs::symlink_metadata(&object) else {
                continue;
            };
            if !object_meta.is_file() || object_meta.len() != size {
                continue;
            }
            if key.starts_with("icons/") {
                let Some(b) = read_bounded_file(&object, MAX_OBJECT).ok() else {
                    continue;
                };
                if b.len() as u64 != size {
                    continue;
                }
                let Some(image) = decode(&b, remaining) else {
                    continue;
                };
                self.window_icons.push(WindowIconRgba {
                    width: image.width,
                    height: image.height,
                    rgba: image.rgba,
                });
            } else {
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
    if let Some(face) = p.strip_prefix("gui/title/background/panorama_") {
        return matches!(
            face,
            "0.png" | "1.png" | "2.png" | "3.png" | "4.png" | "5.png" | "overlay.png"
        );
    }
    if let Some(gui) = p.strip_prefix("gui/sprites/") {
        return [
            "advancements/",
            "gamemode_switcher/",
            "icon/",
            "recipe_book/",
            "spectator/",
        ]
        .iter()
        .any(|prefix| gui.starts_with(prefix));
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
                    let mut credits = d.get("credits").map(strings).unwrap_or_default();
                    if let Some(titles) = d.get("titles").and_then(JsonValue::as_array) {
                        for title in titles {
                            if let Some(name) = title.get_str("title") {
                                credits.push(name.to_owned());
                            }
                            if let Some(names) = title.get("names") {
                                credits.extend(strings(names));
                            }
                        }
                    }
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
        wild_texture: value
            .get_str("wild_texture")
            .or_else(|| textures.and_then(|v| v.get_str("wild")))
            .map(str::to_owned),
        tame_texture: value
            .get_str("tame_texture")
            .or_else(|| textures.and_then(|v| v.get_str("tame")))
            .map(str::to_owned),
        angry_texture: value
            .get_str("angry_texture")
            .or_else(|| textures.and_then(|v| v.get_str("angry")))
            .map(str::to_owned),
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
/// Split `assets/<namespace>/models/item/<file>.json` into
/// `(namespace, item file stem)`. Namespace-generic so mod resource packs are
/// scanned alongside Vanilla's `minecraft` default.
fn item_model_entry(name: &str) -> Option<(&str, &str)> {
    let rest = name.strip_prefix("assets/")?;
    let (namespace, rest) = rest.split_once('/')?;
    let rest = rest.strip_prefix("models/item/")?;
    let file = rest.strip_suffix(".json")?.rsplit('/').next()?;
    if namespace.is_empty()
        || file.is_empty()
        || !namespace.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"_.-".contains(&byte)
        })
    {
        return None;
    }
    Some((namespace, file))
}

/// `model_name` is the item's own model resource (`minecraft:item/bow` or
/// `mymod:item/wrench`), keyed so override entries that reference it resolve
/// to the item's `layer0` texture.
fn parse_item_model(
    catalog: &mut ItemModelCatalog,
    item: &str,
    model_name: &str,
    value: &JsonValue,
) {
    if let Some(textures) = value.get("textures").and_then(JsonValue::as_object) {
        if let Some((_, texture)) = textures.iter().find(|(key, _)| key == "layer0") {
            if let Some(s) = texture.as_str() {
                catalog.textures.insert(item.to_owned(), s.to_owned());
                catalog
                    .model_textures
                    .insert(model_name.to_owned(), s.to_owned());
            }
        }
    }
    if let Some(overrides) = value.get("overrides").and_then(JsonValue::as_array) {
        let parsed = overrides
            .iter()
            .filter_map(|value| {
                let thresholds = value
                    .get("predicate")?
                    .as_object()?
                    .iter()
                    .map(|(key, value)| {
                        let key = key.strip_prefix("minecraft:").unwrap_or(key);
                        value
                            .as_f64()
                            .filter(|n| n.is_finite())
                            .map(|n| (key.to_owned(), n))
                    })
                    .collect::<Option<Vec<_>>>()?;
                let model = value.get_str("model")?;
                let model = if model.contains(':') {
                    model.to_owned()
                } else {
                    format!("minecraft:{model}")
                };
                Some(ItemOverride { thresholds, model })
            })
            .collect();
        catalog.overrides.insert(item.to_owned(), parsed);
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

#[cfg(test)]
mod tests {
    use super::{ItemModelCatalog, MAX_JSON, item_model_entry, json, parse_item_model};

    #[test]
    fn bow_overrides_match_the_complete_predicate_and_choose_the_last_match() {
        let mut catalog = ItemModelCatalog::default();
        let bow = br#"{
            "textures":{"layer0":"minecraft:item/bow"},
            "overrides":[
                {"predicate":{"pulling":1},"model":"item/bow_pulling_0"},
                {"predicate":{"pulling":1,"pull":0.65},"model":"item/bow_pulling_1"},
                {"predicate":{"pulling":1,"pull":0.9},"model":"minecraft:item/bow_pulling_2"}
            ]
        }"#;
        parse_item_model(
            &mut catalog,
            "bow",
            "minecraft:item/bow",
            &json(bow, MAX_JSON).unwrap(),
        );
        for index in 0..=2 {
            let item = format!("bow_pulling_{index}");
            let model = format!(r#"{{"textures":{{"layer0":"minecraft:item/{item}"}}}}"#);
            parse_item_model(
                &mut catalog,
                &item,
                &format!("minecraft:item/{item}"),
                &json(model.as_bytes(), MAX_JSON).unwrap(),
            );
        }
        for (pull, expected) in [
            (0.0, "bow_pulling_0"),
            (0.649, "bow_pulling_0"),
            (0.65, "bow_pulling_1"),
            (0.899, "bow_pulling_1"),
            (0.9, "bow_pulling_2"),
        ] {
            let texture = catalog.resolve_item_texture_with_properties(
                "minecraft:bow",
                &[("pulling", 1.0), ("pull", pull)],
            );
            assert_eq!(texture, Some(format!("minecraft:item/{expected}")));
        }
        assert_eq!(
            catalog
                .resolve_item_texture_with_properties("bow", &[("pull", 1.0)])
                .as_deref(),
            Some("minecraft:item/bow")
        );
    }

    #[test]
    fn modded_item_model_namespace_is_scanned_and_resolved() {
        // Scanner: `assets/<namespace>/models/item/<file>.json` for any pack
        // namespace, while Vanilla's bare key stays unchanged.
        assert_eq!(
            item_model_entry("assets/mymod/models/item/wrench.json"),
            Some(("mymod", "wrench"))
        );
        assert_eq!(
            item_model_entry("assets/minecraft/models/item/bow.json"),
            Some(("minecraft", "bow"))
        );
        assert_eq!(item_model_entry("assets/mymod/textures/item/wrench.png"), None);
        assert_eq!(item_model_entry("data/mymod/models/item/wrench.json"), None);

        let mut catalog = ItemModelCatalog::default();
        let wrench = br#"{"textures":{"layer0":"mymod:item/wrench"}}"#;
        parse_item_model(
            &mut catalog,
            "mymod:wrench",
            "mymod:item/wrench",
            &json(wrench, MAX_JSON).unwrap(),
        );
        assert_eq!(
            catalog.resolve_item_texture("mymod:wrench", None).as_deref(),
            Some("mymod:item/wrench")
        );
    }
}
