// SPDX-License-Identifier: LGPL-3.0-or-later
//! One bounded entry point for the local 1.21.1 asset systems.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use crate::vanilla_atlas_extras::VanillaAtlasExtras;
use crate::vanilla_gui_sprites::{load_gui_stack, VanillaGuiTextureAtlas};
use crate::vanilla_i18n_data::{VanillaJarDataCatalog, VanillaLanguageTable, VanillaTextsAndFonts};
use crate::vanilla_jar::{
    load_visual_atlases_from_paths, prioritized_packs, AssetLayer, VanillaEntityTextureAtlas,
    VanillaParticleTextureAtlas, MAX_DECODED_RGBA_BYTES, MAX_VISUAL_ATLAS_DIM,
};
use crate::vanilla_world_assets::VanillaWorldAssets;
use megu_client_mesh::vanilla_block_models::VanillaBlockModelCatalog;

const MAX_MODEL_BYTES: u64 = 64 * 1024 * 1024;
/// Shared source-byte admission budget for every optional resource-pack
/// layer feeding textures, models, colormaps and GUI atlases. The base
/// client JAR is not optional and is admitted separately; all user packs
/// compete for this one aggregate budget in highest-to-lowest priority so
/// a bulky low-priority layer can never starve an overriding pack.
pub const MAX_SHARED_ASSET_PACK_SOURCE_BYTES: u64 = 256 * 1024 * 1024;

/// Load local runtime assets from the player's game directory and resource packs.
pub fn load_runtime_assets(
    jar_path: Option<&Path>,
    assets_dir: Option<&Path>,
    asset_index: Option<&str>,
    resource_packs: &[PathBuf],
    locale: &str,
) -> VanillaRuntimeAssets {
    VanillaRuntimeAssets::load_runtime_assets(
        jar_path,
        assets_dir,
        asset_index,
        resource_packs,
        locale,
    )
}

/// Combined local asset state consumed by the client renderer and UI.
#[derive(Clone, Debug)]
pub struct VanillaRuntimeAssets {
    pub entity_atlas: VanillaEntityTextureAtlas,
    pub particle_atlas: VanillaParticleTextureAtlas,
    pub world_assets: VanillaWorldAssets,
    pub atlas_extras: VanillaAtlasExtras,
    pub i18n: VanillaI18nCatalog,
    pub gui_atlas: megu_ui::draw::GuiSpriteAtlas,
    /// Full named texture atlas backing `gui_atlas`, retained for callers
    /// that need the HUD and container UV table as well.
    pub gui_textures: VanillaGuiTextureAtlas,
    /// `mob_effect/<id>.png` placements in the shared GUI texture.
    pub mob_effect_uvs: BTreeMap<String, [f32; 4]>,
    /// Baked Vanilla 1.21.1 + resource-pack blockstate/block model catalog.
    pub block_models: VanillaBlockModelCatalog,
}

/// Language, credits, recipe, and tag data from the requested local assets.
#[derive(Clone, Debug, Default)]
pub struct VanillaI18nCatalog {
    pub language: VanillaLanguageTable,
    pub texts_and_fonts: VanillaTextsAndFonts,
    pub data: VanillaJarDataCatalog,
    pub locale: String,
    pub notes: Vec<String>,
}

/// Small diagnostics snapshot that does not clone decoded texture buffers.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct VanillaRuntimeAssetsSummary {
    pub entity_atlas_size: [u32; 2],
    pub particle_atlas_size: [u32; 2],
    pub gui_atlas_size: [u32; 2],
    pub recipes: usize,
    pub world_textures: usize,
    pub gui_sprites: usize,
    pub notes: usize,
    pub locale: String,
}

fn path_source_bytes(path: &Path) -> Option<u64> {
    let meta = std::fs::symlink_metadata(path).ok()?;
    if meta.is_file() {
        return Some(meta.len());
    }
    if !meta.is_dir() {
        return None;
    }
    let mut total = 0u64;
    let mut entries = 0usize;
    let mut stack = vec![path.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(dir).ok()?.flatten() {
            entries += 1;
            if entries > 50_000 {
                return None;
            }
            let meta = std::fs::symlink_metadata(entry.path()).ok()?;
            if meta.is_file() {
                total = total.checked_add(meta.len())?;
            } else if meta.is_dir() {
                stack.push(entry.path());
            }
        }
    }
    Some(total)
}

/// Admit resource packs under the shared aggregate source-byte budget.
/// Input order is Vanilla's low-to-high priority order and the returned
/// vector preserves that order. Admission itself walks highest priority
/// first; oversized or unreadable packs fail closed and are omitted.
pub fn admit_resource_packs(resource_packs: &[PathBuf]) -> Vec<PathBuf> {
    admit_resource_packs_with_notes(resource_packs).0
}

fn admit_resource_packs_with_notes(resource_packs: &[PathBuf]) -> (Vec<PathBuf>, Vec<String>) {
    let ordered = prioritized_packs(resource_packs);
    let mut admitted_high_to_low = Vec::new();
    let mut notes = Vec::new();
    let mut remaining = MAX_SHARED_ASSET_PACK_SOURCE_BYTES;
    for path in ordered.iter().rev() {
        let Some(bytes) = path_source_bytes(path) else {
            notes.push(format!(
                "{} ignored: unreadable or unsafe pack layout",
                path.display()
            ));
            continue;
        };
        if bytes > remaining {
            notes.push(format!(
                "{} ignored: shared asset admission budget exceeded ({bytes} bytes requested, {remaining} remaining)",
                path.display()
            ));
            continue;
        }
        remaining -= bytes;
        admitted_high_to_low.push(path.clone());
    }
    admitted_high_to_low.reverse();
    (admitted_high_to_low, notes)
}

impl VanillaRuntimeAssets {
    /// Load local runtime assets. Missing inputs use each subsystem's
    /// deterministic fallback and are recorded in `i18n.notes`.
    pub fn load_runtime_assets(
        jar_path: Option<&Path>,
        assets_dir: Option<&Path>,
        asset_index: Option<&str>,
        resource_packs: &[PathBuf],
        locale: &str,
    ) -> Self {
        let (admitted_packs, pack_notes) = admit_resource_packs_with_notes(resource_packs);
        let resource_packs = admitted_packs.as_slice();
        let (entity_atlas, particle_atlas) =
            load_visual_atlases_from_paths(jar_path, resource_packs);

        let world_assets = VanillaWorldAssets::load_from_jar_and_packs(jar_path, resource_packs);
        let atlas_extras = VanillaAtlasExtras::load(jar_path, resource_packs);
        let (mut gui_textures, gui_notes) = {
            let assets = load_gui_stack(jar_path, resource_packs);
            (assets.build_full_atlas(), assets.notes)
        };
        let mob_effect_uvs = append_mob_effect_icons(&mut gui_textures, &atlas_extras);
        let gui_atlas = gui_textures.to_gui_sprite_atlas();

        let mut i18n = VanillaI18nCatalog {
            locale: locale.to_ascii_lowercase(),
            ..VanillaI18nCatalog::default()
        };
        if let Some(jar) = jar_path {
            match VanillaLanguageTable::load(
                jar,
                assets_dir,
                asset_index,
                locale,
                &[],
                resource_packs,
            ) {
                Ok(language) => i18n.language = language,
                Err(error) => i18n.notes.push(format!("language data: {error}")),
            }
            match VanillaTextsAndFonts::load(jar, assets_dir, asset_index, resource_packs) {
                Ok(texts) => i18n.texts_and_fonts = texts,
                Err(error) => i18n.notes.push(format!("text/font data: {error}")),
            }
            match VanillaJarDataCatalog::load(jar, resource_packs) {
                Ok(data) => i18n.data = data,
                Err(error) => i18n.notes.push(format!("recipes/tags: {error}")),
            }
        } else {
            i18n.notes
                .push("client JAR unavailable; using asset fallbacks".into());
        }
        i18n.notes.extend(pack_notes);
        i18n.notes.extend(gui_notes);
        let block_models = load_block_model_catalog(jar_path, resource_packs);

        Self {
            entity_atlas,
            particle_atlas,
            world_assets,
            atlas_extras,
            i18n,
            gui_atlas,
            gui_textures,
            mob_effect_uvs,
            block_models,
        }
    }

    /// Summarize loaded data without copying its image or translation data.
    pub fn summary(&self) -> VanillaRuntimeAssetsSummary {
        VanillaRuntimeAssetsSummary {
            entity_atlas_size: [self.entity_atlas.width, self.entity_atlas.height],
            particle_atlas_size: [self.particle_atlas.width, self.particle_atlas.height],
            gui_atlas_size: [self.gui_textures.width, self.gui_textures.height],
            recipes: self.i18n.data.recipes.len(),
            world_textures: self.world_assets.env_textures.len()
                + self.world_assets.colormaps.len()
                + self.world_assets.misc_overlays.len()
                + self.world_assets.destroy_stages.len(),
            gui_sprites: self.gui_textures.entries.len() + self.gui_textures.named_entries.len(),
            notes: self.i18n.notes.len() + self.world_assets.notes.len(),
            locale: self.i18n.locale.clone(),
        }
    }
}

fn load_block_model_catalog(
    jar_path: Option<&Path>,
    resource_packs: &[PathBuf],
) -> VanillaBlockModelCatalog {
    let is_block_model_json = |name: &str| {
        name.starts_with("assets/")
            && name.ends_with(".json")
            && (name.contains("/blockstates/") || name.contains("/models/"))
    };
    // `sources` is low-to-high resolution order: the base JAR first, then
    // packs in Vanilla stacking order. Admission walks it backwards so the
    // winning high-priority document is decoded and charged first; lower
    // copies of the same resource id are never opened or charged at all.
    let mut sources = Vec::new();
    if let Some(path) = jar_path {
        sources.push((path, false));
    }
    for path in prioritized_packs(resource_packs) {
        sources.push((path.as_path(), true));
    }
    let mut layers = Vec::new();
    layers.resize_with(sources.len(), Vec::new);
    let mut winners = BTreeSet::new();
    let mut remaining = MAX_MODEL_BYTES;
    for layer_index in (0..sources.len()).rev() {
        let (path, pack) = sources[layer_index];
        let opened = AssetLayer::open(path, pack, |name| {
            is_block_model_json(name) && !winners.contains(name)
        })
        .and_then(|layer| layer.collect(remaining));
        if let Ok((entries, _)) = opened {
            let mut docs = Vec::new();
            for entry in entries {
                let byte_len = entry.bytes.len() as u64;
                if byte_len > remaining {
                    break;
                }
                remaining -= byte_len;
                winners.insert(entry.name.clone());
                if let Ok(text) = String::from_utf8(entry.bytes) {
                    docs.push((entry.name, text));
                }
            }
            layers[layer_index] = docs;
        }
    }
    VanillaBlockModelCatalog::new(layers)
}

fn append_mob_effect_icons(
    atlas: &mut VanillaGuiTextureAtlas,
    extras: &VanillaAtlasExtras,
) -> BTreeMap<String, [f32; 4]> {
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
    let available: Vec<_> = EFFECTS
        .iter()
        .filter_map(|id| extras.mob_effect(id).map(|image| (*id, image)))
        .collect();
    if available.is_empty() {
        return BTreeMap::new();
    }
    let was_empty = atlas.entries.is_empty() && atlas.named_entries.is_empty();
    let canvas_width = if was_empty { 256 } else { atlas.width.max(256) };
    let old_width = atlas.width as usize;
    let old_height = if was_empty { 0 } else { atlas.height as usize };
    let new_width = canvas_width as usize;
    let mut x = 0u32;
    let mut y = old_height as u32;
    let mut row_height = 0u32;
    let mut positions = Vec::with_capacity(available.len());
    let height_limit = MAX_VISUAL_ATLAS_DIM.min((MAX_DECODED_RGBA_BYTES / (new_width * 4)) as u32);
    for (id, image) in available {
        let (needed, height) = (image.width + 2, image.height + 2);
        if needed > canvas_width {
            continue;
        }
        let (next_x, next_y, next_height) = if x + needed > canvas_width {
            (0, y + row_height, 0)
        } else {
            (x, y, row_height)
        };
        if next_y + next_height.max(height) > height_limit {
            continue;
        }
        positions.push((id.to_owned(), image, next_x + 1, next_y + 1));
        (x, y, row_height) = (next_x + needed, next_y, next_height.max(height));
    }
    if positions.is_empty() {
        return BTreeMap::new();
    }
    let new_height = y.saturating_add(row_height).max(old_height as u32).max(1);
    let mut pixels = vec![0; new_width * new_height as usize * 4];
    for row in 0..old_height {
        let src = row * old_width * 4;
        let dst = row * new_width * 4;
        pixels[dst..dst + old_width * 4].copy_from_slice(&atlas.rgba[src..src + old_width * 4]);
    }
    for (_, image, px, py) in &positions {
        let len = image.width as usize * 4;
        for row in 0..image.height + 2 {
            let src = row.saturating_sub(1).min(image.height - 1) as usize * len;
            let dst = (((py - 1 + row) as usize) * new_width + *px as usize) * 4;
            pixels[dst..dst + len].copy_from_slice(&image.rgba[src..src + len]);
            pixels.copy_within(dst..dst + 4, dst - 4);
            pixels.copy_within(dst + len - 4..dst + len, dst + len);
        }
    }
    if old_height > 0 && old_width > 0 {
        let scale_x = old_width as f32 / new_width as f32;
        let scale_y = old_height as f32 / new_height as f32;
        for (_, entry) in &mut atlas.entries {
            entry.uv_min[0] *= scale_x;
            entry.uv_max[0] *= scale_x;
            entry.uv_min[1] *= scale_y;
            entry.uv_max[1] *= scale_y;
        }
        for (_, entry) in &mut atlas.named_entries {
            entry.uv_min[0] *= scale_x;
            entry.uv_max[0] *= scale_x;
            entry.uv_min[1] *= scale_y;
            entry.uv_max[1] *= scale_y;
        }
    }
    atlas.width = canvas_width;
    atlas.height = new_height;
    atlas.rgba = pixels;
    let mut uvs = BTreeMap::new();
    for (id, image, px, py) in positions {
        let uv_min = [
            px as f32 / atlas.width as f32,
            py as f32 / atlas.height as f32,
        ];
        let uv_max = [
            (px + image.width) as f32 / atlas.width as f32,
            (py + image.height) as f32 / atlas.height as f32,
        ];
        atlas.named_entries.push((
            format!("mob_effect/{id}"),
            crate::vanilla_gui_sprites::SpriteAtlasEntry {
                uv_min,
                uv_max,
                width: image.width,
                height: image.height,
                nine_slice_border: None,
            },
        ));
        uvs.insert(id, [uv_min[0], uv_min[1], uv_max[0], uv_max[1]]);
    }
    uvs
}
