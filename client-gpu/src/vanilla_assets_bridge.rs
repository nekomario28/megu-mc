// SPDX-License-Identifier: LGPL-3.0-or-later
//! One bounded entry point for the local 1.21.1 asset systems.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use crate::vanilla_atlas_extras::VanillaAtlasExtras;
use crate::vanilla_gui_sprites::{load_vanilla_gui_from_path, VanillaGuiTextureAtlas};
use crate::vanilla_i18n_data::{VanillaJarDataCatalog, VanillaLanguageTable, VanillaTextsAndFonts};
use crate::vanilla_jar::{
    load_vanilla_entity_atlas_from_jar_bytes, load_vanilla_particle_atlas_from_jar_bytes,
    VanillaEntityTextureAtlas, VanillaParticleTextureAtlas,
};
use crate::vanilla_world_assets::VanillaWorldAssets;

const MAX_JAR_BYTES: u64 = 256 * 1024 * 1024;

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
        let jar_bytes = jar_path.and_then(read_bounded);
        let pack_bytes: Vec<Vec<u8>> = resource_packs
            .iter()
            .filter(|path| path.is_file())
            .filter_map(|path| read_bounded(path))
            .collect();
        let pack_slices: Vec<&[u8]> = pack_bytes.iter().map(Vec::as_slice).collect();
        let (entity_atlas, particle_atlas) = if let Some(jar) = jar_bytes.as_deref() {
            (
                load_vanilla_entity_atlas_from_jar_bytes(jar, &pack_slices),
                load_vanilla_particle_atlas_from_jar_bytes(jar, &pack_slices),
            )
        } else {
            let empty = [];
            (
                load_vanilla_entity_atlas_from_jar_bytes(&empty, &[]),
                load_vanilla_particle_atlas_from_jar_bytes(&empty, &[]),
            )
        };

        let world_assets = VanillaWorldAssets::load_from_jar_and_packs(jar_path, resource_packs);
        let atlas_extras = VanillaAtlasExtras::load(jar_path, resource_packs);
        let gui_assets = jar_path.map_or_else(
            crate::vanilla_gui_sprites::VanillaGuiAssets::fallback,
            |path| load_vanilla_gui_from_path(path, resource_packs),
        );
        let mut gui_textures = gui_assets.build_full_atlas();
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
        for path in resource_packs.iter().filter(|p| p.is_file()) {
            if read_bounded(path).is_none() {
                i18n.notes
                    .push(format!("resource pack unreadable: {}", path.display()));
            }
        }

        Self {
            entity_atlas,
            particle_atlas,
            world_assets,
            atlas_extras,
            i18n,
            gui_atlas,
            gui_textures,
            mob_effect_uvs,
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
    let canvas_width = if was_empty {
        256
    } else {
        atlas.width.max(256)
    };
    let old_width = atlas.width as usize;
    let old_height = if was_empty { 0 } else { atlas.height as usize };
    let new_width = canvas_width as usize;
    let mut x = 0u32;
    let mut y = old_height as u32;
    let mut row_height = 0u32;
    let mut positions = Vec::with_capacity(available.len());
    for (id, image) in available {
        let Some(needed) = image.width.checked_add(2) else {
            continue;
        };
        if needed > canvas_width {
            continue;
        }
        if x + needed > canvas_width {
            y = y.saturating_add(row_height);
            x = 0;
            row_height = 0;
        }
        positions.push((id.to_owned(), image, x + 1, y + 1));
        x += needed;
        row_height = row_height.max(image.height.saturating_add(2));
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
        for row in 0..image.height as usize {
            let src = row * image.width as usize * 4;
            let dst = ((py + row as u32) as usize * new_width + *px as usize) * 4;
            let len = image.width as usize * 4;
            pixels[dst..dst + len].copy_from_slice(&image.rgba[src..src + len]);
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

fn read_bounded(path: &Path) -> Option<Vec<u8>> {
    let metadata = fs::metadata(path).ok()?;
    if metadata.len() > MAX_JAR_BYTES {
        return None;
    }
    fs::read(path).ok()
}
