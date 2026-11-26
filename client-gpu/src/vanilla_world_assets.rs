// SPDX-License-Identifier: LGPL-3.0-or-later
//! Clean-room loader for Vanilla world, biome, overlay and mining textures.
//!
//! Images are decoded directly from the user's local client JAR and resource
//! packs. No archive contents are extracted to disk or included in this crate.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use crate::vanilla_jar::{
    check_png_budget, decode_asset_png, prioritized_packs, AssetLayer, MAX_DECODED_RGBA_BYTES,
    MAX_PACK_LAYERS,
};

const MAX_TEXTURE_PNG_BYTES: usize = 8 * 1024 * 1024;

/// A decoded RGBA8 texture and whether its current pixels came from the
/// client's Vanilla JAR. Resource-pack overrides and procedural fallbacks
/// have `from_jar == false`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DecodedRgbaTexture {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
    pub from_jar: bool,
}

/// Vanilla world-environment texture categories.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum VanillaEnvTexture {
    Sun,
    MoonPhases,
    Clouds,
    Rain,
    Snow,
    EndSky,
}

impl VanillaEnvTexture {
    pub const ALL: [Self; 6] = [
        Self::Sun,
        Self::MoonPhases,
        Self::Clouds,
        Self::Rain,
        Self::Snow,
        Self::EndSky,
    ];

    pub const fn path(self) -> &'static str {
        match self {
            Self::Sun => "assets/minecraft/textures/environment/sun.png",
            Self::MoonPhases => "assets/minecraft/textures/environment/moon_phases.png",
            Self::Clouds => "assets/minecraft/textures/environment/clouds.png",
            Self::Rain => "assets/minecraft/textures/environment/rain.png",
            Self::Snow => "assets/minecraft/textures/environment/snow.png",
            Self::EndSky => "assets/minecraft/textures/environment/end_sky.png",
        }
    }
}

/// Vanilla temperature/downfall biome colormap categories.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum VanillaColormap {
    Grass,
    Foliage,
}

impl VanillaColormap {
    pub const ALL: [Self; 2] = [Self::Grass, Self::Foliage];

    pub const fn path(self) -> &'static str {
        match self {
            Self::Grass => "assets/minecraft/textures/colormap/grass.png",
            Self::Foliage => "assets/minecraft/textures/colormap/foliage.png",
        }
    }
}

/// Vanilla full-screen and enchantment overlay categories.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum VanillaMiscOverlay {
    EnchantedGlintItem,
    EnchantedGlintEntity,
    Vignette,
    PumpkinBlur,
    PowderSnowOutline,
    SpyglassScope,
    Nausea,
    Underwater,
    Forcefield,
}

impl VanillaMiscOverlay {
    pub const ALL: [Self; 9] = [
        Self::EnchantedGlintItem,
        Self::EnchantedGlintEntity,
        Self::Vignette,
        Self::PumpkinBlur,
        Self::PowderSnowOutline,
        Self::SpyglassScope,
        Self::Nausea,
        Self::Underwater,
        Self::Forcefield,
    ];

    pub const fn path(self) -> &'static str {
        match self {
            Self::EnchantedGlintItem => "assets/minecraft/textures/misc/enchanted_glint_item.png",
            Self::EnchantedGlintEntity => {
                "assets/minecraft/textures/misc/enchanted_glint_entity.png"
            }
            Self::Vignette => "assets/minecraft/textures/misc/vignette.png",
            Self::PumpkinBlur => "assets/minecraft/textures/misc/pumpkinblur.png",
            Self::PowderSnowOutline => "assets/minecraft/textures/misc/powder_snow_outline.png",
            Self::SpyglassScope => "assets/minecraft/textures/misc/spyglass_scope.png",
            Self::Nausea => "assets/minecraft/textures/misc/nausea.png",
            Self::Underwater => "assets/minecraft/textures/misc/underwater.png",
            Self::Forcefield => "assets/minecraft/textures/misc/forcefield.png",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum AssetKey {
    Environment(VanillaEnvTexture),
    Colormap(VanillaColormap),
    Overlay(VanillaMiscOverlay),
    DestroyStage(u8),
}

impl AssetKey {
    const fn path(self) -> &'static str {
        match self {
            Self::Environment(key) => key.path(),
            Self::Colormap(key) => key.path(),
            Self::Overlay(key) => key.path(),
            Self::DestroyStage(stage) => match stage {
                0 => "assets/minecraft/textures/block/destroy_stage_0.png",
                1 => "assets/minecraft/textures/block/destroy_stage_1.png",
                2 => "assets/minecraft/textures/block/destroy_stage_2.png",
                3 => "assets/minecraft/textures/block/destroy_stage_3.png",
                4 => "assets/minecraft/textures/block/destroy_stage_4.png",
                5 => "assets/minecraft/textures/block/destroy_stage_5.png",
                6 => "assets/minecraft/textures/block/destroy_stage_6.png",
                7 => "assets/minecraft/textures/block/destroy_stage_7.png",
                8 => "assets/minecraft/textures/block/destroy_stage_8.png",
                _ => "assets/minecraft/textures/block/destroy_stage_9.png",
            },
        }
    }
}

const ALL_ASSETS: [AssetKey; 27] = [
    AssetKey::Environment(VanillaEnvTexture::Sun),
    AssetKey::Environment(VanillaEnvTexture::MoonPhases),
    AssetKey::Environment(VanillaEnvTexture::Clouds),
    AssetKey::Environment(VanillaEnvTexture::Rain),
    AssetKey::Environment(VanillaEnvTexture::Snow),
    AssetKey::Environment(VanillaEnvTexture::EndSky),
    AssetKey::Colormap(VanillaColormap::Grass),
    AssetKey::Colormap(VanillaColormap::Foliage),
    AssetKey::Overlay(VanillaMiscOverlay::EnchantedGlintItem),
    AssetKey::Overlay(VanillaMiscOverlay::EnchantedGlintEntity),
    AssetKey::Overlay(VanillaMiscOverlay::Vignette),
    AssetKey::Overlay(VanillaMiscOverlay::PumpkinBlur),
    AssetKey::Overlay(VanillaMiscOverlay::PowderSnowOutline),
    AssetKey::Overlay(VanillaMiscOverlay::SpyglassScope),
    AssetKey::Overlay(VanillaMiscOverlay::Nausea),
    AssetKey::Overlay(VanillaMiscOverlay::Underwater),
    AssetKey::Overlay(VanillaMiscOverlay::Forcefield),
    AssetKey::DestroyStage(0),
    AssetKey::DestroyStage(1),
    AssetKey::DestroyStage(2),
    AssetKey::DestroyStage(3),
    AssetKey::DestroyStage(4),
    AssetKey::DestroyStage(5),
    AssetKey::DestroyStage(6),
    AssetKey::DestroyStage(7),
    AssetKey::DestroyStage(8),
    AssetKey::DestroyStage(9),
];

/// Loaded world-environment and overlay textures, with generated fallback
/// pixels populated for every catalogued asset.
#[derive(Clone, Debug)]
pub struct VanillaWorldAssets {
    pub env_textures: BTreeMap<VanillaEnvTexture, DecodedRgbaTexture>,
    pub colormaps: BTreeMap<VanillaColormap, DecodedRgbaTexture>,
    pub misc_overlays: BTreeMap<VanillaMiscOverlay, DecodedRgbaTexture>,
    pub destroy_stages: [DecodedRgbaTexture; 10],
    pub notes: Vec<String>,
}

impl Default for VanillaWorldAssets {
    fn default() -> Self {
        Self::fallback()
    }
}

impl VanillaWorldAssets {
    /// Build deterministic procedural RGBA textures without reading files.
    pub fn fallback() -> Self {
        let mut assets = Self {
            env_textures: BTreeMap::new(),
            colormaps: BTreeMap::new(),
            misc_overlays: BTreeMap::new(),
            destroy_stages: std::array::from_fn(|stage| {
                procedural_texture(AssetKey::DestroyStage(stage as u8))
            }),
            notes: Vec::new(),
        };
        for key in ALL_ASSETS {
            let texture = procedural_texture(key);
            match key {
                AssetKey::Environment(id) => {
                    assets.env_textures.insert(id, texture);
                }
                AssetKey::Colormap(id) => {
                    assets.colormaps.insert(id, texture);
                }
                AssetKey::Overlay(id) => {
                    assets.misc_overlays.insert(id, texture);
                }
                AssetKey::DestroyStage(_) => {}
            }
        }
        assets
    }

    /// Load the catalog from the optional base client JAR and ordered
    /// resource packs. Later packs have higher priority. Pack paths may be
    /// ZIP archives or unpacked resource-pack directories.
    pub fn load_from_jar_and_packs(jar_path: Option<&Path>, resource_packs: &[PathBuf]) -> Self {
        let mut assets = Self::fallback();
        let mut remaining = MAX_DECODED_RGBA_BYTES;
        let mut loaded = BTreeSet::new();
        for path in prioritized_packs(resource_packs).iter().rev() {
            load_layer(path, false, &mut assets, &mut remaining, &mut loaded);
        }
        if let Some(path) = jar_path {
            load_layer(path, true, &mut assets, &mut remaining, &mut loaded);
        }
        if resource_packs.len() > MAX_PACK_LAYERS {
            assets.notes.push(format!(
                "{} resource packs ignored (limit {MAX_PACK_LAYERS})",
                resource_packs.len() - MAX_PACK_LAYERS
            ));
        }
        assets
    }

    pub fn env_texture(&self, texture: VanillaEnvTexture) -> &DecodedRgbaTexture {
        &self.env_textures[&texture]
    }

    pub fn colormap(&self, colormap: VanillaColormap) -> &DecodedRgbaTexture {
        &self.colormaps[&colormap]
    }

    pub fn misc_overlay(&self, overlay: VanillaMiscOverlay) -> &DecodedRgbaTexture {
        &self.misc_overlays[&overlay]
    }

    /// Return a mining crack texture, clamping the stage to `0..=9`.
    pub fn destroy_stage(&self, stage: u8) -> &DecodedRgbaTexture {
        &self.destroy_stages[usize::from(stage.min(9))]
    }

    /// Sample the grass colormap using Vanilla's temperature/downfall lookup.
    pub fn sample_biome_tint(&self, temperature: f32, downfall: f32) -> [u8; 3] {
        sample_colormap_tint(self.colormap(VanillaColormap::Grass), temperature, downfall)
    }
}

/// Normalized UV rectangle for a phase in the 4 by 2 moon sheet.
pub fn moon_phase_uv_rect(phase: u8) -> [f32; 4] {
    let cell = phase % 8;
    let u = f32::from(cell % 4) * 0.25;
    let v = f32::from(cell / 4) * 0.5;
    [u, v, u + 0.25, v + 0.5]
}

/// Sample the Vanilla grass or foliage colormap at temperature/downfall.
/// Coordinates follow Vanilla's `(1 - temperature, 1 - downfall * temperature)`
/// lookup convention, with each normalized coordinate scaled to 0..255.
pub fn sample_colormap_tint(
    colormap: &DecodedRgbaTexture,
    temperature: f32,
    downfall: f32,
) -> [u8; 3] {
    let Some(expected) = (colormap.width as usize)
        .checked_mul(colormap.height as usize)
        .and_then(|pixels| pixels.checked_mul(4))
    else {
        return [0, 0, 0];
    };
    if colormap.width == 0 || colormap.height == 0 || colormap.rgba.len() != expected {
        return [0, 0, 0];
    }
    let t = temperature.clamp(0.0, 1.0);
    let adj_d = downfall.clamp(0.0, 1.0) * t;
    let x = ((1.0 - t) * 255.0) as u32;
    let y = ((1.0 - adj_d) * 255.0) as u32;
    let x = x.min(colormap.width - 1);
    let y = y.min(colormap.height - 1);
    let at = (y as usize * colormap.width as usize + x as usize) * 4;
    [
        colormap.rgba[at],
        colormap.rgba[at + 1],
        colormap.rgba[at + 2],
    ]
}

fn dimensions(key: AssetKey) -> (u32, u32) {
    match key {
        AssetKey::Environment(VanillaEnvTexture::Sun) => (32, 32),
        AssetKey::Environment(VanillaEnvTexture::MoonPhases) => (128, 64),
        AssetKey::Environment(_) => (64, 64),
        AssetKey::Colormap(_) => (256, 256),
        AssetKey::Overlay(_) => (64, 64),
        AssetKey::DestroyStage(_) => (16, 16),
    }
}

fn procedural_texture(key: AssetKey) -> DecodedRgbaTexture {
    let (width, height) = dimensions(key);
    let mut rgba = vec![0; width as usize * height as usize * 4];
    let seed = match key {
        AssetKey::Environment(id) => id as u8,
        AssetKey::Colormap(id) => 16 + id as u8,
        AssetKey::Overlay(id) => 32 + id as u8,
        AssetKey::DestroyStage(stage) => 64 + stage,
    };
    for y in 0..height {
        for x in 0..width {
            let at = (y as usize * width as usize + x as usize) * 4;
            let edge = x == 0 || y == 0 || x + 1 == width || y + 1 == height;
            let checker = ((x / 4) + (y / 4) + u32::from(seed)) % 2 == 0;
            let (rgb, alpha) = match key {
                AssetKey::Environment(VanillaEnvTexture::Sun) => (
                    [255, 210, 70],
                    if (x as i32 - 15).abs() < 12 && (y as i32 - 15).abs() < 12 {
                        255
                    } else {
                        0
                    },
                ),
                AssetKey::Environment(VanillaEnvTexture::MoonPhases) => (
                    [205, 210, 220],
                    if x % 32 < 24 && y % 32 < 24 { 255 } else { 0 },
                ),
                AssetKey::Environment(VanillaEnvTexture::EndSky) => (
                    [
                        24 + (x % 48) as u8,
                        8 + (y % 32) as u8,
                        44 + ((x + y) % 80) as u8,
                    ],
                    255,
                ),
                AssetKey::Environment(_) => ([105, 125, 150], if checker { 180 } else { 90 }),
                AssetKey::Colormap(VanillaColormap::Grass) => {
                    ([55 + (x % 40) as u8, 120 + (y % 75) as u8, 40], 255)
                }
                AssetKey::Colormap(VanillaColormap::Foliage) => {
                    ([45 + (x % 45) as u8, 105 + (y % 90) as u8, 35], 255)
                }
                AssetKey::Overlay(VanillaMiscOverlay::Vignette) => (
                    [0, 0, 0],
                    if edge {
                        220
                    } else {
                        ((x.abs_diff(width / 2) + y.abs_diff(height / 2)) * 3).min(190) as u8
                    },
                ),
                AssetKey::Overlay(
                    VanillaMiscOverlay::EnchantedGlintItem
                    | VanillaMiscOverlay::EnchantedGlintEntity,
                ) => ([155, 80, 210], if checker { 100 } else { 0 }),
                AssetKey::Overlay(_) => ([110, 145, 170], if checker { 110 } else { 30 }),
                AssetKey::DestroyStage(stage) => (
                    [180, 180, 180],
                    if ((x + y + u32::from(stage)) % 7) < u32::from(stage + 1) {
                        200
                    } else {
                        0
                    },
                ),
            };
            rgba[at..at + 3].copy_from_slice(&rgb);
            rgba[at + 3] = alpha;
        }
    }
    DecodedRgbaTexture {
        width,
        height,
        rgba,
        from_jar: false,
    }
}

fn decode_texture(
    bytes: &[u8],
    from_jar: bool,
    key: AssetKey,
    remaining: usize,
) -> Result<DecodedRgbaTexture, String> {
    let (width, height) = check_png_budget(bytes, remaining)?;
    if matches!(key, AssetKey::Colormap(_)) && (width, height) != (256, 256) {
        return Err(format!(
            "colormap is {width}x{height}; Vanilla colormaps must be 256x256"
        ));
    }
    let (_, _, rgba) = decode_asset_png(bytes)?;
    Ok(DecodedRgbaTexture {
        width,
        height,
        rgba,
        from_jar,
    })
}

fn set_texture(assets: &mut VanillaWorldAssets, key: AssetKey, texture: DecodedRgbaTexture) {
    match key {
        AssetKey::Environment(id) => {
            assets.env_textures.insert(id, texture);
        }
        AssetKey::Colormap(id) => {
            assets.colormaps.insert(id, texture);
        }
        AssetKey::Overlay(id) => {
            assets.misc_overlays.insert(id, texture);
        }
        AssetKey::DestroyStage(stage) => assets.destroy_stages[usize::from(stage)] = texture,
    }
}

fn load_layer(
    path: &Path,
    from_jar: bool,
    assets: &mut VanillaWorldAssets,
    remaining: &mut usize,
    loaded: &mut BTreeSet<&'static str>,
) {
    let names: BTreeMap<&str, AssetKey> = ALL_ASSETS
        .into_iter()
        .map(|key| (key.path(), key))
        .collect();
    let mut layer = match AssetLayer::open(path, !from_jar, |name| {
        names.contains_key(name) && !loaded.contains(name)
    }) {
        Ok(layer) => layer,
        Err(error) => {
            assets
                .notes
                .push(format!("{} unreadable: {error}", path.display()));
            return;
        }
    };
    for name in layer.names().to_vec() {
        let key = names[name.as_str()];
        match layer
            .read(&name, MAX_TEXTURE_PNG_BYTES)
            .map_err(|error| error.to_string())
            .and_then(|bytes| decode_texture(&bytes, from_jar, key, *remaining))
        {
            Ok(texture) => {
                *remaining -= texture.rgba.len();
                set_texture(assets, key, texture);
                loaded.insert(key.path());
            }
            Err(error) => assets
                .notes
                .push(format!("{}:{name} rejected: {error}", path.display())),
        }
    }
}
