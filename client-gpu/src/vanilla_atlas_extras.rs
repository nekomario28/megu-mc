// SPDX-License-Identifier: LGPL-3.0-or-later
//! Clean-room loader for visual assets not covered by the GUI sprite atlas.
//!
//! Assets are decoded from the player's own client JAR and resource packs
//! directly in memory. Later resource packs override earlier layers. Missing
//! entries use deterministic generated RGBA placeholders; no Mojang bytes are
//! included in this crate.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::title_branding::RgbaImage;
use crate::vanilla_jar::{
    AssetLayer, MAX_DECODED_RGBA_BYTES, check_png_budget, decode_asset_png, prioritized_packs,
};

const ASSET_PREFIX: &str = "assets/minecraft/textures/";
const MAX_TEXTURE_BYTES: usize = 8 * 1024 * 1024;
const MAX_TEXTURES: usize = 4096;

const EFFECTS: &[&str] = &[
    "speed",
    "slowness",
    "haste",
    "mining_fatigue",
    "strength",
    "instant_health",
    "instant_damage",
    "jump_boost",
    "nausea",
    "regeneration",
    "resistance",
    "fire_resistance",
    "water_breathing",
    "invisibility",
    "blindness",
    "night_vision",
    "hunger",
    "weakness",
    "poison",
    "wither",
    "health_boost",
    "absorption",
    "saturation",
    "glowing",
    "levitation",
    "luck",
    "unluck",
    "slow_falling",
    "conduit_power",
    "dolphins_grace",
    "bad_omen",
    "hero_of_the_village",
    "darkness",
    "trial_omen",
    "raid_omen",
    "wind_charged",
    "weaving",
    "oozing",
    "infested",
];

const TRIM_MATERIALS: &[&str] = &[
    "quartz",
    "iron",
    "iron_darker",
    "netherite",
    "netherite_darker",
    "redstone",
    "copper",
    "gold",
    "gold_darker",
    "emerald",
    "diamond",
    "diamond_darker",
    "lapis",
    "amethyst",
];
const BOSS_COLORS: &[&str] = &["pink", "blue", "red", "green", "yellow", "purple", "white"];
const BOSS_PARTS: &[&str] = &["background", "progress"];
const NOTCHED: &[&str] = &["6", "10", "12", "20"];
const HUD_EXTRAS: &[&str] = &[
    "hud/air",
    "hud/air_bursting",
    "hud/jump_bar_background",
    "hud/jump_bar_cooldown",
    "hud/jump_bar_progress",
    "hud/hotbar_offhand_left",
    "hud/hotbar_offhand_right",
    "hud/crosshair_attack_indicator_background",
    "hud/crosshair_attack_indicator_full",
    "hud/crosshair_attack_indicator_progress",
    "toast/advancement",
    "toast/recipe",
    "toast/system",
    "toast/tutorial",
    "hud/heart/absorbing_full",
    "hud/heart/absorbing_half",
    "hud/heart/poisoned_full",
    "hud/heart/poisoned_half",
    "hud/heart/withered_full",
    "hud/heart/withered_half",
    "hud/heart/frozen_full",
    "hud/heart/frozen_half",
    "hud/heart/vehicle_full",
    "hud/heart/vehicle_half",
    "hud/heart/hardcore_full",
    "hud/heart/hardcore_half",
];
const EMPTY_SLOTS: &[(&str, &str)] = &[
    ("armor", "item/empty_armor_slot_chestplate.png"),
    ("helmet", "item/empty_armor_slot_helmet.png"),
    ("chestplate", "item/empty_armor_slot_chestplate.png"),
    ("leggings", "item/empty_armor_slot_leggings.png"),
    ("boots", "item/empty_armor_slot_boots.png"),
    ("shield", "item/empty_armor_slot_shield.png"),
    (
        "smithing_template",
        "item/empty_slot_smithing_template_armor_trim.png",
    ),
    (
        "smithing_template_netherite_upgrade",
        "item/empty_slot_smithing_template_netherite_upgrade.png",
    ),
    ("loom", "gui/sprites/container/loom/banner_slot.png"),
    ("loom_banner", "gui/sprites/container/loom/banner_slot.png"),
    ("loom_dye", "gui/sprites/container/loom/dye_slot.png"),
    (
        "loom_pattern",
        "gui/sprites/container/loom/pattern_slot.png",
    ),
];

// Brewing stand bottle and fuel silhouettes are drawn into the screen sheet.
const BREWING_SHEET: &str = "gui/container/brewing_stand.png";
const BREWING_SLOTS: &[(&str, u32, u32)] = &[("brewing_stand", 56, 51), ("brewing_fuel", 17, 17)];

/// Loaded sprite collection. Images are keyed by their path below the
/// `textures/` directory, including the `.png` suffix.
#[derive(Clone, Debug, Default)]
pub struct VanillaAtlasExtras {
    images: BTreeMap<String, RgbaImage>,
    loaded_paths: BTreeMap<String, ()>,
}

impl VanillaAtlasExtras {
    /// Loads all recognized sprites. Resource-pack paths are in ascending
    /// priority order; unpacked directories and ZIP/JAR archives are accepted.
    pub fn load(jar_path: Option<&Path>, resource_packs: &[PathBuf]) -> Self {
        let mut out = Self::fallback();
        let mut remaining = MAX_DECODED_RGBA_BYTES;
        for pack in prioritized_packs(resource_packs).iter().rev() {
            out.fold_layer(pack, true, &mut remaining);
        }
        if let Some(path) = jar_path {
            out.fold_layer(path, false, &mut remaining);
        }
        out
    }

    /// Procedural, deterministic defaults for every catalogued family.
    pub fn fallback() -> Self {
        let mut out = Self::default();
        let mut add = |key: String, seed: &str| {
            out.images.insert(key, procedural(seed));
        };
        for id in [
            "kebab",
            "aztec",
            "alban",
            "aztec2",
            "bomb",
            "plant",
            "wasteland",
            "pool",
            "courbet",
            "sea",
            "sunset",
            "creebet",
            "wanderer",
            "graham",
            "match",
            "bust",
            "stage",
            "void",
            "skull_and_roses",
            "wither",
            "fighters",
            "pointer",
            "pigscene",
            "burning_skull",
            "skeleton",
            "donkey_kong",
            "earth",
            "wind",
            "fire",
            "water",
            "baroque",
            "humble",
            "meditative",
            "unpacked",
            "back",
        ] {
            let id = id.trim();
            add(format!("painting/{id}.png"), id);
        }
        for id in EFFECTS {
            add(format!("mob_effect/{id}.png"), id);
        }
        add("map/map_background.png".into(), "map_background");
        add(
            "map/map_background_checkerboard.png".into(),
            "map_checkerboard",
        );
        for id in [
            "player",
            "frame",
            "red_marker",
            "blue_marker",
            "target_x",
            "target_point",
            "player_off_map",
            "player_off_limits",
            "woodland_mansion",
            "ocean_monument",
            "trial_chambers",
            "red_x",
            "desert_village",
            "jungle_temple",
            "snowy_village",
            "plains_village",
            "taiga_village",
            "swamp_hut",
            "savanna_village",
            "white_banner",
            "orange_banner",
            "magenta_banner",
            "light_blue_banner",
            "yellow_banner",
            "lime_banner",
            "pink_banner",
            "gray_banner",
            "light_gray_banner",
            "cyan_banner",
            "purple_banner",
            "blue_banner",
            "brown_banner",
            "green_banner",
            "red_banner",
            "black_banner",
        ] {
            add(format!("map/decorations/{id}.png"), id);
        }
        for suffix in ["", "_leggings"] {
            for id in [
                "bolt",
                "flow",
                "coast",
                "dune",
                "eye",
                "host",
                "raiser",
                "rib",
                "sentry",
                "shaper",
                "silence",
                "snout",
                "spire",
                "tide",
                "vex",
                "ward",
                "wayfinder",
                "wild",
            ] {
                add(format!("trims/models/armor/{id}{suffix}.png"), id);
            }
        }
        add(
            "trims/color_palettes/trim_palette.png".into(),
            "trim_palette",
        );
        for id in TRIM_MATERIALS {
            add(format!("trims/color_palettes/{id}.png"), id);
        }
        for id in [
            "base",
            "border",
            "bricks",
            "circle",
            "creeper",
            "cross",
            "curly_border",
            "diagonal_left",
            "diagonal_right",
            "diagonal_up_left",
            "diagonal_up_right",
            "flower",
            "globe",
            "gradient",
            "gradient_up",
            "half_vertical",
            "half_horizontal",
            "mojang",
            "piglin",
            "rhombus",
            "skull",
            "small_stripes",
            "square_bottom_left",
            "square_bottom_right",
            "square_top_left",
            "square_top_right",
            "straight_cross",
            "stripe_bottom",
            "stripe_center",
            "stripe_downleft",
            "stripe_downright",
            "stripe_left",
            "stripe_middle",
            "stripe_right",
            "stripe_top",
            "triangle_bottom",
            "triangle_top",
            "triangles_bottom",
            "triangles_top",
            "flow",
            "guster",
        ] {
            add(format!("entity/banner/{id}.png"), id);
            add(format!("entity/shield/{id}.png"), id);
        }
        for color in BOSS_COLORS {
            for part in BOSS_PARTS {
                add(
                    format!("gui/sprites/boss_bar/{color}_{part}.png"),
                    &format!("boss_{color}_{part}"),
                );
            }
        }
        for count in NOTCHED {
            for part in BOSS_PARTS {
                add(
                    format!("gui/sprites/boss_bar/notched_{count}_{part}.png"),
                    &format!("boss_notched_{count}_{part}"),
                );
            }
        }
        for id in HUD_EXTRAS {
            add(format!("gui/sprites/{id}.png"), id);
        }
        for &(id, path) in EMPTY_SLOTS {
            add(path.to_owned(), id);
        }
        for &(id, _, _) in BREWING_SLOTS {
            add(id.to_owned(), id);
        }
        out
    }

    /// Returns the painting image by normalized vanilla painting id.
    pub fn painting(&self, id: &str) -> Option<&RgbaImage> {
        self.get(&format!("painting/{}.png", normalize(id)))
    }
    /// Returns a status-effect icon.
    pub fn mob_effect(&self, id: &str) -> Option<&RgbaImage> {
        self.get(&format!("mob_effect/{}.png", normalize(id)))
    }
    /// Returns the ordinary map background.
    pub fn map_background(&self) -> Option<&RgbaImage> {
        self.get("map/map_background.png")
    }
    /// Returns a discrete map decoration sprite.
    pub fn map_decoration(&self, id: &str) -> Option<&RgbaImage> {
        let id = normalize(id);
        let id = match id.as_str() {
            "banner" => "white_banner",
            "mansion" => "woodland_mansion",
            "monument" => "ocean_monument",
            id => id,
        };
        self.get(&format!("map/decorations/{id}.png"))
    }
    /// Returns a banner pattern layer by pattern id.
    pub fn banner_pattern(&self, id: &str) -> Option<&RgbaImage> {
        self.get(&format!("entity/banner/{}.png", normalize(id)))
    }
    /// Returns a shield pattern layer by pattern id.
    pub fn shield_pattern(&self, id: &str) -> Option<&RgbaImage> {
        self.get(&format!("entity/shield/{}.png", normalize(id)))
    }
    /// Returns armor trim overlay (`leggings=true` selects leggings model).
    pub fn armor_trim(&self, pattern: &str, leggings: bool) -> Option<&RgbaImage> {
        self.get(&format!(
            "trims/models/armor/{}{}.png",
            normalize(pattern),
            if leggings { "_leggings" } else { "" }
        ))
    }
    /// Returns the trim base palette or a material palette.
    pub fn trim_palette(&self, material: &str) -> Option<&RgbaImage> {
        self.get(&format!("trims/color_palettes/{}.png", normalize(material)))
    }
    /// Returns boss bar background/progress for a color or notched count.
    pub fn boss_bar(&self, color: &str, progress: bool) -> Option<&RgbaImage> {
        let part = if progress { "progress" } else { "background" };
        let key = if let Some(n) = color.strip_prefix("notched_") {
            format!("gui/sprites/boss_bar/notched_{n}_{part}.png")
        } else {
            format!("gui/sprites/boss_bar/{}_{part}.png", normalize(color))
        };
        self.get(&key)
    }
    /// Returns an extended HUD/heart/toast sprite by its path below `gui/sprites/`.
    pub fn hud_extra(&self, id: &str) -> Option<&RgbaImage> {
        self.get(&format!("gui/sprites/{}.png", normalize_path(id)))
    }
    /// Returns an empty-slot silhouette by id.
    pub fn empty_slot_sprite(&self, id: &str) -> Option<&RgbaImage> {
        let id = normalize(id);
        if BREWING_SLOTS.iter().any(|(key, _, _)| *key == id) {
            return self.get(&id);
        }
        EMPTY_SLOTS
            .iter()
            .find(|(key, _)| *key == id)
            .and_then(|(_, path)| self.get(path))
    }
    /// Count of valid images loaded from archives or directories.
    pub fn from_jar_count(&self) -> usize {
        self.loaded_paths.len()
    }
    /// Count of available procedural fallback images.
    pub fn fallback_count(&self) -> usize {
        self.images.len().saturating_sub(self.loaded_paths.len())
    }
    /// Generic texture lookup using a path below `textures/`.
    pub fn texture(&self, relative_path: &str) -> Option<&RgbaImage> {
        self.get(&format!(
            "{}.png",
            normalize_path(relative_path.trim_end_matches(".png"))
        ))
    }

    fn get(&self, key: &str) -> Option<&RgbaImage> {
        self.images.get(key)
    }
    fn fold_layer(&mut self, path: &Path, pack: bool, remaining: &mut usize) {
        let flat = pack && path.is_dir() && !path.join("assets").is_dir();
        let key = |name: &str| {
            if flat {
                let full = format!("{ASSET_PREFIX}{name}");
                asset_key(&full).map(str::to_owned)
            } else {
                asset_key(name).map(str::to_owned)
            }
        };
        let Ok(mut layer) = AssetLayer::open(path, pack, |name| key(name).is_some()) else {
            return;
        };
        for name in layer.names().to_vec() {
            let Some(key) = key(&name) else { continue };
            if self.loaded_paths.len() >= MAX_TEXTURES || self.loaded_paths.contains_key(&key) {
                continue;
            }
            let Ok(bytes) = layer.read(&name, MAX_TEXTURE_BYTES) else {
                continue;
            };
            if let Ok(image) = decode(&bytes, *remaining) {
                *remaining -= image.rgba.len();
                if key == BREWING_SHEET && !self.insert_brewing_slots(&image, remaining) {
                    continue;
                }
                self.images.insert(key.clone(), image);
                self.loaded_paths.insert(key, ());
            }
        }
    }
    fn insert_brewing_slots(&mut self, image: &RgbaImage, remaining: &mut usize) -> bool {
        if image.width < 256 || !image.width.is_multiple_of(256) || image.width != image.height {
            return false;
        }
        let scale = image.width / 256;
        let size = 16 * scale;
        let row_bytes = size as usize * 4;
        let slot_bytes = row_bytes * size as usize;
        if slot_bytes * BREWING_SLOTS.len() > *remaining {
            return false;
        }
        for &(name, x, y) in BREWING_SLOTS {
            let mut rgba = Vec::with_capacity(slot_bytes);
            for row in y * scale..y * scale + size {
                let start = (row * image.width + x * scale) as usize * 4;
                rgba.extend_from_slice(&image.rgba[start..start + row_bytes]);
            }
            self.images.insert(
                name.to_owned(),
                RgbaImage {
                    width: size,
                    height: size,
                    rgba,
                },
            );
            self.loaded_paths.insert(name.to_owned(), ());
            *remaining -= slot_bytes;
        }
        true
    }
}

fn asset_key(name: &str) -> Option<&str> {
    if !name.starts_with(ASSET_PREFIX)
        || !name.ends_with(".png")
        || name.contains('\\')
        || name
            .split('/')
            .any(|p| p == ".." || p == "." || p.is_empty())
    {
        return None;
    }
    let key = name.strip_prefix(ASSET_PREFIX)?;
    let recognized = key.starts_with("painting/")
        || key.starts_with("mob_effect/")
        || key.starts_with("map/")
        || key.starts_with("trims/models/armor/")
        || key.starts_with("trims/color_palettes/")
        || key.starts_with("entity/banner/")
        || key.starts_with("entity/shield/")
        || key.starts_with("gui/sprites/boss_bar/")
        || key.starts_with("gui/sprites/hud/")
        || key.starts_with("gui/sprites/toast/")
        || key == BREWING_SHEET
        || EMPTY_SLOTS.iter().any(|(_, path)| *path == key);
    recognized.then_some(key)
}
fn decode(bytes: &[u8], remaining: usize) -> Result<RgbaImage, String> {
    check_png_budget(bytes, remaining)?;
    let (width, height, rgba) = decode_asset_png(bytes)?;
    Ok(RgbaImage {
        width,
        height,
        rgba,
    })
}
fn normalize(id: &str) -> String {
    id.trim().to_ascii_lowercase().replace([' ', '-'], "_")
}
fn normalize_path(id: &str) -> String {
    id.trim()
        .trim_start_matches('/')
        .replace('\\', "/")
        .split('/')
        .filter(|part| !part.is_empty() && *part != "." && *part != "..")
        .collect::<Vec<_>>()
        .join("/")
}
fn procedural(seed: &str) -> RgbaImage {
    let mut hash = 0x811c9dc5u32;
    for byte in seed.bytes() {
        hash = (hash ^ u32::from(byte)).wrapping_mul(0x01000193);
    }
    let a = [
        48 + (hash & 0x7f) as u8,
        48 + ((hash >> 8) & 0x7f) as u8,
        48 + ((hash >> 16) & 0x7f) as u8,
        255,
    ];
    let b = [a[0] / 2, a[1] / 2, a[2] / 2, 255];
    let mut rgba = Vec::with_capacity(16 * 16 * 4);
    for y in 0..16 {
        for x in 0..16 {
            rgba.extend_from_slice(if ((x / 4 + y / 4) & 1) == 0 { &a } else { &b });
        }
    }
    RgbaImage {
        width: 16,
        height: 16,
        rgba,
    }
}

/// Replaces each trim pixel exactly matching one of the eight source palette
/// stops with the material color at that stop. Unmatched pixels are preserved
/// byte-for-byte rather than warped to the nearest stop.
pub fn recolor_armor_trim(
    trim_rgba: &[u8],
    base_palette: &[u8],
    material_palette: &[u8],
) -> Option<Vec<u8>> {
    if !trim_rgba.len().is_multiple_of(4) || base_palette.len() < 32 || material_palette.len() < 32
    {
        return None;
    }
    let mut out = Vec::with_capacity(trim_rgba.len());
    for pixel in trim_rgba.as_chunks::<4>().0 {
        if pixel[3] == 0 {
            out.extend_from_slice(&[0, 0, 0, 0]);
            continue;
        }
        // Vanilla trim permutation is an exact palette replacement, not a
        // nearest-color remap. Unknown/custom pixels survive unchanged.
        let Some(stop) = (0..8).find(|stop| {
            let offset = stop * 4;
            pixel[..3] == base_palette[offset..offset + 3]
        }) else {
            out.extend_from_slice(pixel);
            continue;
        };
        let offset = stop * 4;
        let material = &material_palette[offset..offset + 4];
        let alpha = u16::from(pixel[3]);
        out.extend_from_slice(&material[..3]);
        out.push(((alpha * u16::from(material[3]) + 127) / 255) as u8);
    }
    Some(out)
}
