// SPDX-License-Identifier: LGPL-3.0-or-later
//! Clean-room loader for visual assets not covered by the GUI sprite atlas.
//!
//! Assets are decoded from the player's own client JAR and resource packs
//! directly in memory. Later resource packs override earlier layers. Missing
//! entries use deterministic generated RGBA placeholders; no Mojang bytes are
//! included in this crate.

use std::collections::BTreeMap;
use std::fs;
use std::io::Read;
use std::path::{Component, Path, PathBuf};

use megu_client_mesh::png_decode;
use megu_core::zip_archive::LazyZipIndex;

use crate::title_branding::RgbaImage;

const ASSET_PREFIX: &str = "assets/minecraft/textures/";
const MAX_TEXTURE_BYTES: usize = 8 * 1024 * 1024;
const MAX_TEXTURES: usize = 4096;
const MAX_TOTAL_DECODED_BYTES: usize = 256 * 1024 * 1024;
const MAX_DIR_DEPTH: usize = 32;

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
    "netherite",
    "redstone",
    "copper",
    "gold",
    "emerald",
    "diamond",
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
const EMPTY_SLOTS: &[&str] = &[
    "armor",
    "shield",
    "anvil",
    "smithing_template",
    "loom",
    "brewing_stand",
];

/// Loaded sprite collection. Images are keyed by their path below the
/// `textures/` directory, including the `.png` suffix.
#[derive(Clone, Debug, Default)]
pub struct VanillaAtlasExtras {
    images: BTreeMap<String, RgbaImage>,
    loaded_paths: BTreeMap<String, ()>,
    loaded_bytes: usize,
}

impl VanillaAtlasExtras {
    /// Loads all recognized sprites. Resource-pack paths are in ascending
    /// priority order; unpacked directories and ZIP/JAR archives are accepted.
    pub fn load(jar_path: Option<&Path>, resource_packs: &[PathBuf]) -> Self {
        let mut out = Self::fallback();
        if let Some(path) = jar_path.filter(|p| p.is_file()) {
            out.fold_archive(path);
        }
        for pack in resource_packs {
            if pack.is_dir() {
                out.fold_directory(pack);
            } else if pack.is_file() {
                out.fold_archive(pack);
            }
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
        add("map/map_icons.png".into(), "map_icons");
        for id in [
            "player",
            "frame",
            "red_marker",
            "blue_marker",
            "target_x",
            "target_point",
            "player_off_map",
            "player_off_limits",
            "banner",
            "mansion",
            "monument",
            "trial_chambers",
        ] {
            add(format!("map/decorations/{id}.png"), id);
        }
        for kind in ["humanoid", "leggings"] {
            for id in [
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
                add(format!("trims/models/armor/{id}_{kind}.png"), id);
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
        for id in EMPTY_SLOTS {
            add(format!("gui/sprites/container/slot/{id}.png"), id);
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
    /// Returns a map decoration icon, falling back to the shared icon sheet.
    pub fn map_decoration(&self, id: &str) -> Option<&RgbaImage> {
        self.get(&format!("map/decorations/{}.png", normalize(id)))
            .or_else(|| self.get("map/map_icons.png"))
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
            "trims/models/armor/{}_{}.png",
            normalize(pattern),
            if leggings { "leggings" } else { "humanoid" }
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
        self.get(&format!("gui/sprites/container/slot/{}.png", normalize(id)))
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
    fn fold_archive(&mut self, path: &Path) {
        let Ok(index) = LazyZipIndex::open_filtered(path, recognized) else {
            return;
        };
        for entry in index.entries() {
            let Some(key) = asset_key(&entry.name) else {
                continue;
            };
            if self.loaded_paths.len() >= MAX_TEXTURES && !self.loaded_paths.contains_key(key) {
                break;
            }
            let Ok(bytes) = index.read_entry(&entry.name, MAX_TEXTURE_BYTES) else {
                continue;
            };
            if let Ok(image) = decode(&bytes) {
                self.insert_loaded(key, image);
            }
        }
    }
    fn fold_directory(&mut self, root: &Path) {
        let mut stack = vec![(root.to_path_buf(), 0usize)];
        while let Some((dir, depth)) = stack.pop() {
            if depth > MAX_DIR_DEPTH {
                continue;
            }
            let Ok(items) = fs::read_dir(&dir) else {
                continue;
            };
            for item in items.flatten() {
                let Ok(kind) = item.file_type() else { continue };
                if kind.is_dir() {
                    stack.push((item.path(), depth + 1));
                    continue;
                }
                if !kind.is_file() {
                    continue;
                }
                let path = item.path();
                let Ok(relative) = path.strip_prefix(root) else {
                    continue;
                };
                if !relative
                    .components()
                    .all(|c| matches!(c, Component::Normal(_)))
                {
                    continue;
                }
                let name = relative
                    .to_string_lossy()
                    .replace(std::path::MAIN_SEPARATOR, "/");
                let full = if name.starts_with("assets/") {
                    name
                } else {
                    format!("assets/minecraft/textures/{name}")
                };
                let Some(key) = asset_key(&full) else {
                    continue;
                };
                if self.loaded_paths.len() >= MAX_TEXTURES && !self.loaded_paths.contains_key(key) {
                    continue;
                }
                let Ok(metadata) = fs::metadata(&path) else {
                    continue;
                };
                if metadata.len() > MAX_TEXTURE_BYTES as u64 {
                    continue;
                }
                let Ok(file) = fs::File::open(path) else {
                    continue;
                };
                let mut bytes = Vec::new();
                if file
                    .take(MAX_TEXTURE_BYTES as u64 + 1)
                    .read_to_end(&mut bytes)
                    .is_err()
                    || bytes.len() > MAX_TEXTURE_BYTES
                {
                    continue;
                }
                if let Ok(image) = decode(&bytes) {
                    self.insert_loaded(key, image);
                }
            }
        }
    }
    fn insert_loaded(&mut self, key: &str, image: RgbaImage) {
        let old_size = if self.loaded_paths.contains_key(key) {
            self.images.get(key).map_or(0, |old| old.rgba.len())
        } else {
            0
        };
        let Some(total) = self
            .loaded_bytes
            .checked_sub(old_size)
            .and_then(|used| used.checked_add(image.rgba.len()))
        else {
            return;
        };
        if total > MAX_TOTAL_DECODED_BYTES {
            return;
        }
        self.loaded_bytes = total;
        self.images.insert(key.to_owned(), image);
        self.loaded_paths.insert(key.to_owned(), ());
    }
}

fn recognized(name: &str) -> bool {
    asset_key(name).is_some()
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
        || key.starts_with("gui/sprites/container/slot/");
    recognized.then_some(key)
}
fn decode(bytes: &[u8]) -> Result<RgbaImage, String> {
    if bytes.len() > MAX_TEXTURE_BYTES {
        return Err("PNG exceeds per-entry cap".into());
    }
    let (width, height, rgba) = png_decode::decode_png_rgba(bytes).map_err(|e| e.to_string())?;
    let expected = (width as usize)
        .checked_mul(height as usize)
        .and_then(|n| n.checked_mul(4))
        .ok_or("PNG dimensions overflow")?;
    if rgba.len() != expected {
        return Err("decoded PNG size mismatch".into());
    }
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

/// Replaces each trim pixel with the closest of eight source palette stops,
/// then selects the material palette color at that same stop. This preserves
/// the trim's alpha mask while applying Vanilla's eight-stop permutation.
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
        let stop = (0..8).min_by_key(|stop| {
            let offset = stop * 4;
            (0..3)
                .map(|channel| {
                    let delta =
                        i32::from(pixel[channel]) - i32::from(base_palette[offset + channel]);
                    (delta * delta) as u32
                })
                .sum::<u32>()
        })?;
        let offset = stop * 4;
        let material = &material_palette[offset..offset + 4];
        let alpha = u16::from(pixel[3]);
        out.extend_from_slice(&material[..3]);
        out.push(((alpha * u16::from(material[3]) + 127) / 255) as u8);
    }
    Some(out)
}
