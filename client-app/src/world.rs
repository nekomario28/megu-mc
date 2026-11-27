// SPDX-License-Identifier: LGPL-3.0-or-later
//! Live Protocol 767 world cache, block-state classification, `client-port`
//! greedy meshing + vertex packing + translucent sorting, and collision lookup.

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::Instant;

use megu_core::coords::CoordMap;
use megu_core::render_cs::SectionRenderMetadata;

use megu_client_mesh::ffi::{
    ABI_OK, PackedVertex, megu_client_mesh_pack_vertices, megu_client_mesh_sort_translucent,
};
use megu_client_mesh::vanilla_block_models::{BakedBlockStateCache, VanillaBlockModelCatalog};
use megu_client_mesh::{
    ALL_FACES, BakedTerrainFace, Block, BlockShape, COMPACT_VERTEX_MAX_STATE_ID, CUTOUT,
    CompressedSection, DirectionBuckets, FLUID_TINTED, FaceVisibility, LAVA, LIGHT_BYTES, Layer,
    Light, MeshCache, MissingNeighbor, NO_FLUID, OPAQUE, Quad, ReleasedMeshMeta, RenderRegions,
    SAME_STATE_CULL, SECTION_CELLS, Section, SectionBlocks, SectionCoord, SectionGraph, TINTED,
    TRANSLUCENT, WATER, section_index,
};
use megu_kernel::occlusion_table::{CoalescedLightQueue, QueuedLightEdit};
use megu_kernel::skeletal_anim::{
    AnimationClip, AnimationController, AnimationError, BoneMask, LoopMode, MAX_CLIPS, SkeletonRig,
    decode_animation_trigger, parse_animation_json, parse_geometry_json, parse_jem_model,
    parse_jpm_model,
};
use megu_kernel::{
    Aabb, FluidCell, FluidKind, LightEdit, LightEngine, LightFact, LightUpdateMultiWord,
    PalettedSection, Registry, STATE_COUNT,
};

use crate::protocol::{
    ChunkBiomeData, DecodedBlockEntity, DecodedBossBarAction, DecodedChunk,
    DecodedEntityMetadataEntry, DecodedLightData, DecodedParticleOptions, DimensionMeta,
    MapItemIcon, NbtValue, PlayEvent, PlayerTeamInfo, VisualLevelEvent, WireAttribute, WireReader,
    bitset_get, decode_boss_bar_action, decode_entity_metadata, decode_paletted_container,
    decode_particle_options, visual_level_event,
};

/// Texture-name to block-atlas sprite lookup used to certify when a baked
/// full-cube model is compatible with the existing greedy terrain stream.
pub type BlockAtlasSpriteLookup = Arc<dyn Fn(&str) -> Option<u32> + Send + Sync>;

// ---------------------------------------------------------------------------
// Safe Rust wrappers over client-port vertex packing and translucent sorting
// ---------------------------------------------------------------------------

pub fn pack_vertices(quads: &[Quad]) -> Result<Vec<PackedVertex>, i32> {
    if quads.is_empty() {
        return Ok(Vec::new());
    }
    let mut required = 0usize;
    let query = unsafe {
        megu_client_mesh_pack_vertices(
            quads.as_ptr(),
            quads.len(),
            std::ptr::null_mut(),
            0,
            &mut required,
        )
    };
    if query != ABI_OK {
        return Err(query);
    }
    let mut out = vec![PackedVertex::default(); required];
    let mut written = 0usize;
    let status = unsafe {
        megu_client_mesh_pack_vertices(
            quads.as_ptr(),
            quads.len(),
            out.as_mut_ptr(),
            out.len(),
            &mut written,
        )
    };
    if status != ABI_OK {
        return Err(status);
    }
    out.truncate(written);
    Ok(out)
}

fn packed_vertices_digest(vertices: &[PackedVertex]) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    for vertex in vertices {
        vertex.position.hash(&mut hasher);
        vertex.state_id.hash(&mut hasher);
        vertex.attributes.hash(&mut hasher);
        vertex.tint.hash(&mut hasher);
    }
    hasher.finish()
}

pub fn sort_translucent(quads: &[Quad], camera: [f32; 3]) -> Result<Vec<u32>, i32> {
    if quads.is_empty() {
        return Ok(Vec::new());
    }
    let mut required = 0usize;
    let query = unsafe {
        megu_client_mesh_sort_translucent(
            quads.as_ptr(),
            quads.len(),
            camera[0],
            camera[1],
            camera[2],
            std::ptr::null_mut(),
            0,
            &mut required,
        )
    };
    if query != ABI_OK {
        return Err(query);
    }
    if required == 0 {
        return Ok(Vec::new());
    }
    let mut out = vec![0u32; required];
    let mut written = 0usize;
    let status = unsafe {
        megu_client_mesh_sort_translucent(
            quads.as_ptr(),
            quads.len(),
            camera[0],
            camera[1],
            camera[2],
            out.as_mut_ptr(),
            out.len(),
            &mut written,
        )
    };
    if status != ABI_OK {
        return Err(status);
    }
    out.truncate(written);
    Ok(out)
}

// ---------------------------------------------------------------------------
// Block-state registry (loads assets/block_states_1211.json or uses fallback)
// ---------------------------------------------------------------------------

const FLAG_AIR: u8 = 1;
const FLAG_SOLID: u8 = 8;

#[derive(Clone, Copy, Debug)]
struct LocalBox {
    min: [f64; 3],
    max: [f64; 3],
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DynamicBlockState {
    pub name: String,
    pub opacity: u8,
    pub luminance: u8,
}

impl DynamicBlockState {
    fn from_facts_byte(name: String, facts_byte: u8) -> Self {
        Self {
            name,
            opacity: facts_byte & 0x0f,
            luminance: facts_byte >> 4,
        }
    }

    fn light_fact(&self) -> LightFact {
        let opaque = self.opacity >= 15;
        LightFact {
            opacity: self.opacity.min(15),
            emission: self.luminance.min(15),
            occlusion: if opaque { [0x0f; 6] } else { [0; 6] },
        }
    }
}

pub struct BlockTable {
    mesh_blocks: Vec<Block>,
    shapes: Vec<LocalBox>,
    collision_shapes: Vec<Vec<u16>>,
    fluid_cells: Vec<FluidCell>,
    air_mask: Vec<bool>,
    /// Per-state registry name (`"minecraft:oak_planks"`) when the table was
    /// built from `block_states_1211.json`; empty in builtin fallback mode.
    state_names: Vec<String>,
    biome_tints: BiomeTintResolver,
    /// Sparse registry for modded dynamic states. Dynamic ids start at
    /// `STATE_COUNT` (`26_684`) on the mod protocol, with Create-band rows
    /// directly above vanilla and generic mod rows at `1_000_000+`; extending
    /// the dense per-state arrays to the maximum id would waste memory.
    dynamic_states: BTreeMap<u32, DynamicBlockState>,
    /// Exclusive state bound including installed dynamic ids.
    dynamic_state_limit: usize,
    /// O(1) state→material table built once alongside `state_names`
    /// (empty → StoneLike fallback in builtin mode).
    acoustic_lut: crate::spatial_audio::AcousticMaterialLut,
    pub loaded_from_json: bool,
    pub source_path: Option<PathBuf>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BiomeTintKind {
    Grass,
    Foliage,
    Water,
}

#[derive(Clone, Debug, Default)]
pub struct BiomeColormap {
    grass_rgba: Option<Vec<u8>>,
    foliage_rgba: Option<Vec<u8>>,
}

impl BiomeColormap {
    pub fn from_rgba(grass: Option<Vec<u8>>, foliage: Option<Vec<u8>>) -> Result<Self, String> {
        const RGBA_BYTES: usize = 256 * 256 * 4;
        for (name, bytes) in [("grass", &grass), ("foliage", &foliage)] {
            if bytes
                .as_ref()
                .is_some_and(|pixels| pixels.len() != RGBA_BYTES)
            {
                return Err(format!(
                    "{name} colormap must contain exactly {RGBA_BYTES} RGBA bytes"
                ));
            }
        }
        Ok(Self {
            grass_rgba: grass,
            foliage_rgba: foliage,
        })
    }

    fn sample(&self, kind: BiomeTintKind, temperature: f32, downfall: f32) -> Option<u32> {
        let pixels = match kind {
            BiomeTintKind::Grass => self.grass_rgba.as_ref()?,
            BiomeTintKind::Foliage => self.foliage_rgba.as_ref()?,
            BiomeTintKind::Water => return None,
        };
        let temperature = temperature.clamp(0.0, 1.0);
        let downfall = downfall.clamp(0.0, 1.0) * temperature;
        let x = ((1.0 - temperature) * 255.0) as usize;
        let y = ((1.0 - downfall) * 255.0) as usize;
        let offset = (y * 256 + x) * 4;
        Some(
            (u32::from(pixels[offset]) << 16)
                | (u32::from(pixels[offset + 1]) << 8)
                | u32::from(pixels[offset + 2]),
        )
    }
}

/// Vanilla `grass_color_modifier` modes that affect colormap-derived grass
/// colors after the temperature/downfall lookup.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BiomeGrassColorModifier {
    /// Dark forest: average the colormap color with `0x28340a`.
    DarkForest,
    /// Swamp family noise selects two fixed greens. The resolver has no
    /// block-position input, so it uses vanilla's dominant `0x6a7039` branch.
    Swamp,
}

/// One biome registry climate/tint record. Ids are the biome palette ids used
/// by the chunk wire format (the canonical vanilla order is alphabetical for
/// the 64 built-in 1.21.1 biomes). Colors are `0xRRGGBB`; `None` means the
/// temperature/downfall colormap lookup is authoritative.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BiomeClimateEntry {
    pub biome_id: u32,
    pub temperature: f32,
    pub downfall: f32,
    pub grass_color: Option<u32>,
    pub foliage_color: Option<u32>,
    pub water_color: Option<u32>,
    pub grass_color_modifier: Option<BiomeGrassColorModifier>,
}

const fn biome_climate(
    biome_id: u32,
    temperature: f32,
    downfall: f32,
    grass_color: Option<u32>,
    foliage_color: Option<u32>,
    water_color: u32,
) -> BiomeClimateEntry {
    BiomeClimateEntry {
        biome_id,
        temperature,
        downfall,
        grass_color,
        foliage_color,
        water_color: Some(water_color),
        grass_color_modifier: None,
    }
}

const fn biome_climate_mod(
    biome_id: u32,
    temperature: f32,
    downfall: f32,
    grass_color: Option<u32>,
    foliage_color: Option<u32>,
    water_color: u32,
    grass_color_modifier: BiomeGrassColorModifier,
) -> BiomeClimateEntry {
    BiomeClimateEntry {
        grass_color_modifier: Some(grass_color_modifier),
        ..biome_climate(
            biome_id,
            temperature,
            downfall,
            grass_color,
            foliage_color,
            water_color,
        )
    }
}

/// Canonical vanilla 1.21.1 biome climates in registry/alphabetical order.
/// Values mirror the vanilla `data/minecraft/worldgen/biome/*.json` records;
/// explicit colors are the fixed `effects.*_color` overrides. Biomes without
/// an override use the grass/foliage colormap and water color 0x3f76e4.
pub const VANILLA_BIOME_CLIMATES_1_21_1: [BiomeClimateEntry; 64] = [
    biome_climate(0, 2.0, 0.0, Some(0x90814d), Some(0x9e814d), 0x3f76e4), // badlands
    biome_climate(1, 0.95, 0.9, None, None, 0x3f76e4),                    // bamboo_jungle
    biome_climate(2, 2.0, 0.0, None, None, 0x3f76e4),                     // basalt_deltas
    biome_climate(3, 0.8, 0.4, None, None, 0x3f76e4),                     // beach
    biome_climate(4, 0.6, 0.6, None, None, 0x3f76e4),                     // birch_forest
    biome_climate(5, 0.5, 0.8, Some(0xb6db61), Some(0xb6db61), 0x5db7ef), // cherry_grove
    biome_climate(6, 0.5, 0.5, None, None, 0x3d57d6),                     // cold_ocean
    biome_climate(7, 2.0, 0.0, None, None, 0x3f76e4),                     // crimson_forest
    biome_climate_mod(
        8,
        0.7,
        0.8,
        None,
        None,
        0x3f76e4,
        BiomeGrassColorModifier::DarkForest,
    ), // dark_forest
    biome_climate(9, 0.5, 0.5, None, None, 0x3d57d6),                     // deep_cold_ocean
    biome_climate(10, 0.8, 0.4, None, None, 0x3f76e4),                    // deep_dark
    biome_climate(11, 0.5, 0.5, None, None, 0x3938c9),                    // deep_frozen_ocean
    biome_climate(12, 0.5, 0.5, None, None, 0x45adf2),                    // deep_lukewarm_ocean
    biome_climate(13, 0.5, 0.5, None, None, 0x3f76e4),                    // deep_ocean
    biome_climate(14, 2.0, 0.0, None, None, 0x3f76e4),                    // desert
    biome_climate(15, 0.8, 0.4, None, None, 0x3f76e4),                    // dripstone_caves
    biome_climate(16, 0.5, 0.5, None, None, 0x3f76e4),                    // end_barrens
    biome_climate(17, 0.5, 0.5, None, None, 0x3f76e4),                    // end_highlands
    biome_climate(18, 0.5, 0.5, None, None, 0x3f76e4),                    // end_midlands
    biome_climate(19, 2.0, 0.0, Some(0x90814d), Some(0x9e814d), 0x3f76e4), // eroded_badlands
    biome_climate(20, 0.7, 0.8, None, None, 0x3f76e4),                    // flower_forest
    biome_climate(21, 0.7, 0.8, None, None, 0x3f76e4),                    // forest
    biome_climate(22, 0.0, 0.5, None, None, 0x3938c9),                    // frozen_ocean
    biome_climate(23, -0.7, 0.9, None, None, 0x3f76e4),                   // frozen_peaks
    biome_climate(24, 0.0, 0.5, None, None, 0x3938c9),                    // frozen_river
    biome_climate(25, -0.2, 0.8, None, None, 0x3f76e4),                   // grove
    biome_climate(26, 0.0, 0.5, None, None, 0x3f76e4),                    // ice_spikes
    biome_climate(27, -0.7, 0.9, None, None, 0x3f76e4),                   // jagged_peaks
    biome_climate(28, 0.95, 0.9, None, None, 0x3f76e4),                   // jungle
    biome_climate(29, 0.5, 0.5, None, None, 0x45adf2),                    // lukewarm_ocean
    biome_climate(30, 0.5, 0.5, None, None, 0x3f76e4),                    // lush_caves
    biome_climate_mod(
        31,
        0.8,
        0.9,
        None,
        Some(0x8db127),
        0x3a7a6a,
        BiomeGrassColorModifier::Swamp,
    ), // mangrove_swamp
    biome_climate(32, 0.5, 0.8, None, None, 0x0e4ecf),                    // meadow
    biome_climate(33, 0.9, 1.0, None, None, 0x3f76e4),                    // mushroom_fields
    biome_climate(34, 2.0, 0.0, None, None, 0x3f76e4),                    // nether_wastes
    biome_climate(35, 0.5, 0.5, None, None, 0x3f76e4),                    // ocean
    biome_climate(36, 0.6, 0.6, None, None, 0x3f76e4),                    // old_growth_birch_forest
    biome_climate(37, 0.3, 0.8, None, None, 0x3f76e4),                    // old_growth_pine_taiga
    biome_climate(38, 0.25, 0.8, None, None, 0x3f76e4),                   // old_growth_spruce_taiga
    biome_climate(39, 0.8, 0.4, None, None, 0x3f76e4),                    // plains
    biome_climate(40, 0.5, 0.5, None, None, 0x3f76e4),                    // river
    biome_climate(41, 2.0, 0.0, None, None, 0x3f76e4),                    // savanna
    biome_climate(42, 2.0, 0.0, None, None, 0x3f76e4),                    // savanna_plateau
    biome_climate(43, 0.5, 0.5, None, None, 0x3f76e4),                    // small_end_islands
    biome_climate(44, 0.05, 0.3, None, None, 0x3d57d6),                   // snowy_beach
    biome_climate(45, 0.0, 0.5, None, None, 0x3f76e4),                    // snowy_plains
    biome_climate(46, -0.3, 0.9, None, None, 0x3f76e4),                   // snowy_slopes
    biome_climate(47, -0.5, 0.4, None, None, 0x3d57d6),                   // snowy_taiga
    biome_climate(48, 2.0, 0.0, None, None, 0x3f76e4),                    // soul_sand_valley
    biome_climate(49, 0.95, 0.8, None, None, 0x3f76e4),                   // sparse_jungle
    biome_climate(50, 1.0, 0.3, None, None, 0x3f76e4),                    // stony_peaks
    biome_climate(51, 0.2, 0.3, None, None, 0x3f76e4),                    // stony_shore
    biome_climate(52, 0.8, 0.4, None, None, 0x3f76e4),                    // sunflower_plains
    biome_climate_mod(
        53,
        0.8,
        0.9,
        None,
        Some(0x6a7039),
        0x617b64,
        BiomeGrassColorModifier::Swamp,
    ), // swamp
    biome_climate(54, 0.25, 0.8, None, None, 0x3f76e4),                   // taiga
    biome_climate(55, 0.5, 0.5, None, None, 0x3f76e4),                    // the_end
    biome_climate(56, 0.5, 0.5, None, None, 0x3f76e4),                    // the_void
    biome_climate(57, 0.5, 0.5, None, None, 0x43d5ee),                    // warm_ocean
    biome_climate(58, 2.0, 0.0, None, None, 0x3f76e4),                    // warped_forest
    biome_climate(59, 0.2, 0.3, None, None, 0x3f76e4),                    // windswept_forest
    biome_climate(60, 0.2, 0.3, None, None, 0x3f76e4), // windswept_gravelly_hills
    biome_climate(61, 0.2, 0.3, None, None, 0x3f76e4), // windswept_hills
    biome_climate(62, 2.0, 0.0, None, None, 0x3f76e4), // windswept_savanna
    biome_climate(63, 2.0, 0.0, Some(0x90814d), Some(0x9e814d), 0x3f76e4), // wooded_badlands
];

fn climate_fallback_tint(kind: BiomeTintKind, temperature: f32, downfall: f32) -> u32 {
    // Approximate the vanilla colormap ramp when the client JAR colormap is
    // unavailable. This keeps canonical climates visually distinct instead
    // of collapsing every biome to the historic flat green fallback.
    if (temperature, downfall) == (0.8, 0.4) {
        return match kind {
            BiomeTintKind::Grass => 0x79c05a,
            BiomeTintKind::Foliage => 0x59ae30,
            BiomeTintKind::Water => 0x3f76e4,
        };
    }
    let t = temperature.clamp(0.0, 1.0);
    let d = downfall.clamp(0.0, 1.0) * t;
    let (cold, hot, wet) = match kind {
        BiomeTintKind::Grass => ((117, 167, 91), (191, 183, 85), (70, 170, 70)),
        BiomeTintKind::Foliage => ((87, 130, 55), (174, 164, 42), (45, 145, 55)),
        BiomeTintKind::Water => return 0x3f76e4,
    };
    let mix = |a: (u8, u8, u8), b: (u8, u8, u8), f: f32| {
        [
            (f32::from(a.0) * (1.0 - f) + f32::from(b.0) * f) as u32,
            (f32::from(a.1) * (1.0 - f) + f32::from(b.1) * f) as u32,
            (f32::from(a.2) * (1.0 - f) + f32::from(b.2) * f) as u32,
        ]
    };
    let dry = mix(cold, hot, t);
    let rgb = mix((dry[0] as u8, dry[1] as u8, dry[2] as u8), wet, d);
    (rgb[0] << 16) | (rgb[1] << 8) | rgb[2]
}

/// Resolves optional asset colormaps using the biome's vanilla temperature
/// and downfall values. The canonical 1.21.1 climate table is installed by
/// default; registry updates can replace or extend it at runtime. Unknown
/// ids preserve the block's established fallback tint.
#[derive(Clone, Debug)]
pub struct BiomeTintResolver {
    colormap: BiomeColormap,
    climate: BTreeMap<u32, (f32, f32)>,
    grass_colors: BTreeMap<u32, u32>,
    grass_modifiers: BTreeMap<u32, BiomeGrassColorModifier>,
    foliage_colors: BTreeMap<u32, u32>,
    water_colors: BTreeMap<u32, u32>,
}

impl Default for BiomeTintResolver {
    fn default() -> Self {
        let mut resolver = Self {
            colormap: BiomeColormap::default(),
            climate: BTreeMap::new(),
            grass_colors: BTreeMap::new(),
            grass_modifiers: BTreeMap::new(),
            foliage_colors: BTreeMap::new(),
            water_colors: BTreeMap::new(),
        };
        resolver.update_biome_climates_from_registry(&VANILLA_BIOME_CLIMATES_1_21_1);
        resolver
    }
}

impl BiomeTintResolver {
    pub fn set_colormap(&mut self, colormap: BiomeColormap) {
        self.colormap = colormap;
    }

    pub fn set_biome_climate(&mut self, biome_id: u32, temperature: f32, downfall: f32) -> bool {
        if !temperature.is_finite() || !downfall.is_finite() {
            return false;
        }
        self.climate.insert(biome_id, (temperature, downfall));
        true
    }

    /// Replace or extend climate records from the live biome registry.
    /// Non-finite climate values are rejected; the return value is the number
    /// of accepted records. Explicit color fields replace existing overrides,
    /// while `None` clears a prior override and restores colormap lookup.
    pub fn update_biome_climates_from_registry(&mut self, entries: &[BiomeClimateEntry]) -> usize {
        let mut accepted = 0;
        for entry in entries {
            if !entry.temperature.is_finite() || !entry.downfall.is_finite() {
                continue;
            }
            self.climate
                .insert(entry.biome_id, (entry.temperature, entry.downfall));
            if let Some(color) = entry.grass_color {
                self.grass_colors
                    .insert(entry.biome_id, color & 0x00ff_ffff);
            } else {
                self.grass_colors.remove(&entry.biome_id);
            }
            if let Some(modifier) = entry.grass_color_modifier {
                self.grass_modifiers.insert(entry.biome_id, modifier);
            } else {
                self.grass_modifiers.remove(&entry.biome_id);
            }
            if let Some(color) = entry.foliage_color {
                self.foliage_colors
                    .insert(entry.biome_id, color & 0x00ff_ffff);
            } else {
                self.foliage_colors.remove(&entry.biome_id);
            }
            if let Some(color) = entry.water_color {
                self.water_colors
                    .insert(entry.biome_id, color & 0x00ff_ffff);
            } else {
                self.water_colors.remove(&entry.biome_id);
            }
            accepted += 1;
        }
        accepted
    }

    pub fn set_water_color(&mut self, biome_id: u32, color: u32) {
        self.water_colors.insert(biome_id, color & 0x00ff_ffff);
    }

    pub fn resolve(&self, biome_id: u32, kind: BiomeTintKind, fallback: u32) -> u32 {
        if kind == BiomeTintKind::Water {
            return self
                .water_colors
                .get(&biome_id)
                .copied()
                .unwrap_or(fallback);
        }
        let explicit = match kind {
            BiomeTintKind::Grass => self.grass_colors.get(&biome_id),
            BiomeTintKind::Foliage => self.foliage_colors.get(&biome_id),
            BiomeTintKind::Water => None,
        };
        if let Some(&color) = explicit {
            return color;
        }
        let base = self
            .climate
            .get(&biome_id)
            .map(|&(temperature, downfall)| {
                self.colormap
                    .sample(kind, temperature, downfall)
                    .unwrap_or_else(|| climate_fallback_tint(kind, temperature, downfall))
            })
            .unwrap_or(fallback);
        if kind != BiomeTintKind::Grass {
            return base;
        }
        match self.grass_modifiers.get(&biome_id) {
            Some(BiomeGrassColorModifier::DarkForest) => {
                ((base & 0x00fe_fefe).wrapping_add(0x0028_340a) >> 1) & 0x00ff_ffff
            }
            Some(BiomeGrassColorModifier::Swamp) => 0x006a_7039,
            None => base,
        }
    }
}

impl BlockTable {
    pub fn set_biome_tint_resolver(&mut self, resolver: BiomeTintResolver) {
        self.biome_tints = resolver;
    }
}

impl BlockTable {
    pub fn load(explicit_path: Option<&Path>) -> Self {
        if let Some(path) = explicit_path {
            if let Ok(table) = Self::from_json_file(path) {
                return table;
            }
        }
        let candidates = [
            PathBuf::from("assets/block_states_1211.json"),
            PathBuf::from("../assets/block_states_1211.json"),
        ];
        for candidate in &candidates {
            if candidate.is_file() {
                if let Ok(table) = Self::from_json_file(candidate) {
                    return table;
                }
            }
        }
        if let Ok(exe) = std::env::current_exe() {
            for ancestor in exe.ancestors().skip(1).take(4) {
                let candidate = ancestor.join("assets/block_states_1211.json");
                if candidate.is_file() {
                    if let Ok(table) = Self::from_json_file(&candidate) {
                        return table;
                    }
                }
            }
        }
        Self::builtin()
    }

    /// Load the launcher's block facts, with a coarse fallback for the
    /// zero-setup runtime that ships only the block registry.
    pub fn load_for_game_dir(game_dir: &Path, blocks_json: Option<&Path>) -> Self {
        let table = Self::load(Some(&game_dir.join("assets/block_states_1211.json")));
        if table.loaded_from_json {
            return table;
        }
        blocks_json
            .and_then(|path| std::fs::read(path).ok())
            .and_then(|raw| Self::from_blocks_json(&raw).ok())
            .unwrap_or(table)
    }

    fn from_blocks_json(raw: &[u8]) -> Result<Self, String> {
        use megu_core::json::JsonValue;

        let mut table = Self::builtin();
        table
            .state_names
            .resize(table.mesh_blocks.len(), String::new());
        decode_block_registry_names(&mut table.state_names, raw, |state, base, name, entry| {
            let short = name.strip_prefix("minecraft:").unwrap_or(name);
            let air = matches!(short, "air" | "cave_air" | "void_air");
            let collides = entry.get("boundingBox").and_then(JsonValue::as_str) == Some("block");
            let solid =
                collides && entry.get("transparent").and_then(JsonValue::as_bool) == Some(false);
            let collision: &[u16] = if collides { &[0] } else { &[] };
            let flag = (if air { FLAG_AIR } else { 0 }) | (if solid { FLAG_SOLID } else { 0 });
            let full_sides = if solid { ALL_FACES } else { 0 };
            let (block, fluid) = classify_parsed_state(
                state as u32,
                base,
                short,
                flag,
                full_sides,
                collision,
                &table.shapes,
            );
            table.mesh_blocks[state] = block;
            table.fluid_cells[state] = fluid;
            table.air_mask[state] = air;
            table.collision_shapes[state] = if fluid.blocks_motion {
                collision.to_vec()
            } else {
                Vec::new()
            };
        })?;
        table.acoustic_lut =
            crate::spatial_audio::AcousticMaterialLut::from_state_names(&table.state_names);
        Ok(table)
    }

    pub fn builtin() -> Self {
        let count = STATE_COUNT as usize;
        let shapes = vec![LocalBox {
            min: [0.0, 0.0, 0.0],
            max: [1.0, 1.0, 1.0],
        }];
        let mut mesh_blocks = Vec::with_capacity(count);
        let mut collision_shapes = Vec::with_capacity(count);
        let mut fluid_cells = Vec::with_capacity(count);
        let mut air_mask = Vec::with_capacity(count);

        for state in 0..count {
            let sid = state as u32;
            let (block, col, fcell, is_air) = classify_builtin(sid);
            mesh_blocks.push(block);
            collision_shapes.push(col);
            fluid_cells.push(fcell);
            air_mask.push(is_air);
        }

        Self {
            mesh_blocks,
            shapes,
            collision_shapes,
            fluid_cells,
            air_mask,
            state_names: Vec::new(),
            dynamic_states: BTreeMap::new(),
            dynamic_state_limit: 0,
            biome_tints: BiomeTintResolver::default(),
            acoustic_lut: crate::spatial_audio::AcousticMaterialLut::from_state_names(&[]),
            loaded_from_json: false,
            source_path: None,
        }
    }

    pub fn from_json_file(path: &Path) -> Result<Self, String> {
        let raw =
            std::fs::read(path).map_err(|e| format!("failed to read {}: {e}", path.display()))?;
        let mut table = Self::from_json_bytes(&raw)?;
        table.source_path = Some(path.to_path_buf());
        if let Some(blocks_path) = path.parent().map(|dir| dir.join("blocks_1211.json")) {
            if blocks_path.is_file() {
                let blocks_raw = std::fs::read(&blocks_path)
                    .map_err(|e| format!("failed to read {}: {e}", blocks_path.display()))?;
                enrich_state_names_from_blocks_json(&mut table.state_names, &blocks_raw)?;
            }
        }
        Ok(table)
    }

    pub fn from_json_bytes(raw: &[u8]) -> Result<Self, String> {
        let parsed = parse_block_states_json(raw)?;
        let count = parsed.flags.len();
        if count == 0 || parsed.full_sides.len() != count || parsed.collision_shapes.len() != count
        {
            return Err("inconsistent column lengths in block_states_1211.json".to_string());
        }

        let mut owned_names: Vec<String> = vec![String::new(); count];
        let mut base_ids: Vec<u32> = vec![0; count];
        for (name, base, len) in &parsed.blocks {
            let start = *base as usize;
            let end = start.saturating_add(*len as usize).min(count);
            for idx in start..end {
                owned_names[idx] = name.clone();
                base_ids[idx] = *base;
            }
        }

        let mut mesh_blocks = Vec::with_capacity(count);
        let mut fluid_cells = Vec::with_capacity(count);
        let mut air_mask = Vec::with_capacity(count);

        for idx in 0..count {
            let name = owned_names[idx].as_str();
            let short = name.strip_prefix("minecraft:").unwrap_or(name);
            let flag = parsed.flags[idx];
            air_mask.push(flag & FLAG_AIR != 0);
            let (block, cell) = classify_parsed_state(
                idx as u32,
                base_ids[idx],
                short,
                flag,
                parsed.full_sides[idx] & ALL_FACES,
                &parsed.collision_shapes[idx],
                &parsed.shapes,
            );
            mesh_blocks.push(block);
            fluid_cells.push(cell);
        }

        Ok(Self {
            mesh_blocks,
            shapes: parsed.shapes,
            collision_shapes: parsed.collision_shapes,
            fluid_cells,
            air_mask,
            acoustic_lut: crate::spatial_audio::AcousticMaterialLut::from_state_names(&owned_names),
            state_names: owned_names,
            dynamic_states: BTreeMap::new(),
            dynamic_state_limit: 0,
            biome_tints: BiomeTintResolver::default(),
            loaded_from_json: true,
            source_path: None,
        })
    }

    /// Install dynamic mod block states keyed by their global state id.
    /// `facts_byte` uses the MRS1 packing `(luminance << 4) | opacity`.
    /// Returns the number of accepted entries. The sparse representation
    /// deliberately does not allocate dense arrays up to the dynamic id.
    pub fn install_dynamic_mod_states(&mut self, states: &[(u32, String, u8)]) -> usize {
        let mut accepted = 0;
        for &(state_id, ref name, facts_byte) in states {
            // Vanilla rows (`0..STATE_COUNT`) are owned by the dense vanilla
            // table and must never be overwritten by a dynamic registry frame.
            // Every id at or above `STATE_COUNT` is mod-owned: Create-band
            // rows start immediately at `26_684`, while generic mod rows use
            // the sparse `1_000_000+` band. Accepting the full non-vanilla
            // space also keeps mixed-band MRS1 frames lossless.
            if state_id < STATE_COUNT || name.is_empty() || name.len() > 512 {
                continue;
            }
            self.dynamic_states.insert(
                state_id,
                DynamicBlockState::from_facts_byte(name.clone(), facts_byte),
            );
            accepted += 1;
        }
        if accepted != 0 {
            self.dynamic_state_limit = self
                .dynamic_states
                .keys()
                .next_back()
                .and_then(|&id| usize::try_from(id).ok()?.checked_add(1))
                .unwrap_or(usize::MAX);
        }
        accepted
    }

    /// Light facts for one installed dynamic block state.
    pub fn dynamic_light_fact(&self, state_id: u32) -> Option<LightFact> {
        self.dynamic_states
            .get(&state_id)
            .map(DynamicBlockState::light_fact)
    }

    pub fn mesh_block(&self, state_id: u32) -> Block {
        if let Some(dynamic) = self.dynamic_states.get(&state_id) {
            if dynamic.opacity >= 15 {
                return Block::opaque_cube(state_id);
            }
            return Block {
                state_id,
                flags: CUTOUT,
                occlusion: 0,
                fluid_kind: NO_FLUID,
                fluid_level: 0,
                tint: 0,
                fluid_tint: 0,
            };
        }
        self.mesh_blocks
            .get(state_id as usize)
            .copied()
            .unwrap_or_else(|| Block::opaque_cube(state_id))
    }

    fn mesh_block_for_biome(&self, state_id: u32, biome_id: u32) -> Block {
        let mut block = self.mesh_block(state_id);
        if block.flags & TINTED != 0 {
            let name = self
                .state_name(state_id)
                .map(|name| block_state_base_name(name).trim_start_matches("minecraft:"));
            if name.is_none_or(|name| block_tint(name).1) {
                let kind = if name.is_some_and(foliage_tint_kind) {
                    BiomeTintKind::Foliage
                } else {
                    BiomeTintKind::Grass
                };
                block.tint = self.biome_tints.resolve(biome_id, kind, block.tint);
            }
        }
        if block.flags & FLUID_TINTED != 0 {
            block.fluid_tint =
                self.biome_tints
                    .resolve(biome_id, BiomeTintKind::Water, block.fluid_tint);
        }
        block
    }

    pub fn is_air(&self, state_id: u32) -> bool {
        if self.dynamic_states.contains_key(&state_id) {
            return false;
        }
        self.air_mask
            .get(state_id as usize)
            .copied()
            .unwrap_or(false)
    }

    /// Registry name for a block state (`"minecraft:oak_planks"`); `None`
    /// for out-of-range states or the builtin fallback table.
    pub fn state_name(&self, state_id: u32) -> Option<&str> {
        if let Some(dynamic) = self.dynamic_states.get(&state_id) {
            return Some(dynamic.name.as_str());
        }
        self.state_names
            .get(state_id as usize)
            .map(String::as_str)
            .filter(|n| !n.is_empty())
    }

    /// Number of global block states this table covers, including the sparse
    /// dynamic-state limit when modded states have been installed.
    pub fn state_count(&self) -> usize {
        self.mesh_blocks.len().max(self.dynamic_state_limit)
    }

    /// Replace or extend biome climate records in this table's tint resolver.
    pub fn update_biome_climates_from_registry(&mut self, entries: &[BiomeClimateEntry]) -> usize {
        self.biome_tints
            .update_biome_climates_from_registry(entries)
    }

    /// Per-state registry names indexed by state id (empty in builtin
    /// fallback mode); bulk source for acoustic/material LUT construction.
    pub fn state_names(&self) -> &[String] {
        &self.state_names
    }

    /// The O(1) acoustic LUT built once when this table loaded — every
    /// ray/snapshot query is a single indexed read, never a name scan.
    pub fn acoustic_lut(&self) -> &crate::spatial_audio::AcousticMaterialLut {
        &self.acoustic_lut
    }

    pub fn fluid_cell(&self, state_id: u32) -> FluidCell {
        if self.dynamic_states.contains_key(&state_id) {
            return FluidCell {
                kind: None,
                amount: 0,
                falling: false,
                blocks_motion: self.dynamic_states[&state_id].opacity != 0,
                is_ice: false,
                sturdy_faces: if self.dynamic_states[&state_id].opacity >= 15 {
                    ALL_FACES
                } else {
                    0
                },
            };
        }
        self.fluid_cells
            .get(state_id as usize)
            .copied()
            .unwrap_or_default()
    }

    pub fn push_collision_aabbs(
        &self,
        state_id: u32,
        bx: i32,
        by: i32,
        bz: i32,
        out: &mut Vec<Aabb>,
    ) {
        if let Some(dynamic) = self.dynamic_states.get(&state_id) {
            if dynamic.opacity != 0 {
                let (ox, oy, oz) = (f64::from(bx), f64::from(by), f64::from(bz));
                out.push(Aabb {
                    min: [ox, oy, oz],
                    max: [ox + 1.0, oy + 1.0, oz + 1.0],
                });
            }
            return;
        }
        let Some(indices) = self.collision_shapes.get(state_id as usize) else {
            return;
        };
        let ox = f64::from(bx);
        let oy = f64::from(by);
        let oz = f64::from(bz);
        for &idx in indices {
            if let Some(shape) = self.shapes.get(usize::from(idx)) {
                let min = [ox + shape.min[0], oy + shape.min[1], oz + shape.min[2]];
                let max = [ox + shape.max[0], oy + shape.max[1], oz + shape.max[2]];
                if min[0] < max[0] && min[1] < max[1] && min[2] < max[2] {
                    out.push(Aabb { min, max });
                }
            }
        }
    }
}

fn classify_render_shape(name: &str, shapes: &[LocalBox], collision_shapes: &[u16]) -> BlockShape {
    if matches!(
        name,
        "short_grass"
            | "tall_grass"
            | "fern"
            | "large_fern"
            | "dead_bush"
            | "cobweb"
            | "sugar_cane"
            | "seagrass"
            | "tall_seagrass"
            | "kelp"
            | "kelp_plant"
            | "sweet_berry_bush"
            | "bamboo_sapling"
            | "vine"
            | "glow_lichen"
            | "brown_mushroom"
            | "red_mushroom"
            | "wither_rose"
            | "torchflower"
            | "pitcher_plant"
            | "poppy"
            | "blue_orchid"
            | "allium"
            | "azure_bluet"
            | "cornflower"
            | "oxeye_daisy"
            | "dandelion"
            | "sunflower"
            | "lilac"
            | "rose_bush"
            | "peony"
            | "wheat"
            | "carrots"
            | "potatoes"
            | "beetroots"
            | "nether_wart"
    ) || name.ends_with("_flower")
        || name.ends_with("_sapling")
    {
        BlockShape::CrossPlant
    } else if name.ends_with("_slab") {
        let boxes = collision_shapes
            .iter()
            .filter_map(|&shape_id| shapes.get(usize::from(shape_id)));
        if boxes
            .clone()
            .any(|shape| shape.min[1] == 0.0 && shape.max[1] == 1.0)
        {
            BlockShape::Cube
        } else if boxes
            .clone()
            .any(|shape| shape.min[1] >= 0.5 && shape.max[1] > 0.5)
        {
            BlockShape::SlabTop
        } else {
            BlockShape::SlabBottom
        }
    } else if name.ends_with("_stairs") {
        BlockShape::Stair
    } else if name.ends_with("_torch")
        || name == "torch"
        || name == "lantern"
        || name == "soul_lantern"
        || name == "candle"
        || name.ends_with("_candle")
    {
        BlockShape::Torch
    } else if name.ends_with("_fence")
        || name.ends_with("_wall")
        || name == "iron_bars"
        || name.ends_with("_pane")
    {
        BlockShape::FencePost
    } else if name.ends_with("_door") || name.ends_with("_trapdoor") {
        BlockShape::DoorPanel
    } else if name.ends_with("_carpet")
        || name == "snow"
        || name.ends_with("_pressure_plate")
        || name.ends_with("_rail")
        || name == "lily_pad"
        || name == "redstone_wire"
    {
        BlockShape::ThinLayer
    } else if name.ends_with("_bed") {
        BlockShape::BedHalf
    } else {
        BlockShape::Cube
    }
}

/// Mesh block + fluid cell for one water/lava flow level (`0..=15`; `>= 8`
/// is the falling column form). Shared by the builtin table and the JSON
/// loader so the two fluid representations cannot drift apart.
fn fluid_block_and_cell(sid: u32, level: u8, water: bool) -> (Block, FluidCell) {
    let falling = level >= 8;
    let amount = if falling { 8 } else { 8 - level };
    let cell = FluidCell {
        kind: Some(if water {
            FluidKind::Water
        } else {
            FluidKind::Lava
        }),
        amount,
        falling,
        blocks_motion: false,
        is_ice: false,
        sturdy_faces: 0,
    };
    let block = Block {
        state_id: sid,
        fluid_kind: if water { WATER } else { LAVA },
        fluid_level: level,
        flags: if water { FLUID_TINTED } else { 0 },
        fluid_tint: if water { 0x3f76e4 } else { 0 },
        ..Block::AIR
    };
    (block, cell)
}

fn foliage_tint_kind(name: &str) -> bool {
    matches!(
        name,
        "oak_leaves"
            | "jungle_leaves"
            | "acacia_leaves"
            | "dark_oak_leaves"
            | "mangrove_leaves"
            | "vine"
    )
}

/// RGB fallback and whether the biome colormap may override it.
fn block_tint(name: &str) -> (u32, bool) {
    match name {
        "birch_leaves" => (0x80a755, false),
        "spruce_leaves" => (0x619961, false),
        "lily_pad" => (0x208030, false),
        "grass_block" | "short_grass" | "tall_grass" | "fern" | "large_fern" | "sugar_cane" => {
            (0x79c05a, true)
        }
        _ if foliage_tint_kind(name) => (0x59ae30, true),
        _ => (0, false),
    }
}

/// Classify one `block_states_1211.json` row into its mesh block and fluid
/// cell. Air rows get the air block and an empty cell; water/lava rows share
/// [`fluid_block_and_cell`] with the builtin table.
fn classify_parsed_state(
    sid: u32,
    base_id: u32,
    short: &str,
    flag: u8,
    full_sides: u8,
    collision_shapes: &[u16],
    shapes: &[LocalBox],
) -> (Block, FluidCell) {
    if flag & FLAG_AIR != 0 {
        return (Block::AIR, FluidCell::default());
    }
    if short == "water" || short == "lava" {
        let level = sid.saturating_sub(base_id).min(15) as u8;
        return fluid_block_and_cell(sid, level, short == "water");
    }
    let has_collision = !collision_shapes.is_empty();
    let is_ice = short == "ice" || short == "packed_ice" || short == "blue_ice";
    let is_translucent = short.contains("stained_glass")
        || short == "ice"
        || short == "frosted_ice"
        || short == "slime_block"
        || short == "honey_block"
        || short == "tinted_glass"
        || short == "nether_portal";
    let (tint, _) = block_tint(short);
    let tint_flag = if tint != 0 { TINTED } else { 0 };

    let block = if is_translucent {
        Block {
            state_id: sid,
            tint,
            flags: TRANSLUCENT | SAME_STATE_CULL | tint_flag,
            occlusion: 0,
            fluid_kind: NO_FLUID,
            fluid_level: 0,
            fluid_tint: 0,
        }
    } else if full_sides == ALL_FACES && flag & FLAG_SOLID != 0 && !short.contains("glass") {
        Block {
            state_id: sid,
            tint,
            flags: OPAQUE | tint_flag,
            occlusion: ALL_FACES,
            fluid_kind: NO_FLUID,
            fluid_level: 0,
            fluid_tint: 0,
        }
    } else {
        Block {
            state_id: sid,
            tint,
            flags: CUTOUT | tint_flag,
            occlusion: if short.contains("glass") || short.ends_with("_leaves") {
                0
            } else {
                full_sides
            },
            fluid_kind: NO_FLUID,
            fluid_level: 0,
            fluid_tint: 0,
        }
    };
    let shape = classify_render_shape(short, shapes, collision_shapes);
    let mut block = block.with_shape(shape);
    if shape == BlockShape::ThinLayer {
        let height = collision_shapes
            .iter()
            .filter_map(|&shape_id| shapes.get(usize::from(shape_id)))
            .map(|shape| shape.max[1])
            .fold(0.0f64, f64::max);
        let parameter = (height * 16.0).round().clamp(1.0, 2.0) as u8;
        block = block.with_shape_parameter(parameter);
    }
    let cell = FluidCell {
        kind: None,
        amount: 0,
        falling: false,
        blocks_motion: has_collision,
        is_ice,
        sturdy_faces: full_sides,
    };
    (block, cell)
}

fn classify_builtin(sid: u32) -> (Block, Vec<u16>, FluidCell, bool) {
    // Canonical 1.21.1 state IDs for key blocks when block_states_1211.json is absent.
    match sid {
        0 => (Block::AIR, Vec::new(), FluidCell::default(), true),
        80..=95 => {
            let (block, cell) = fluid_block_and_cell(sid, (sid - 80) as u8, true);
            (block, Vec::new(), cell, false)
        }
        96..=111 => {
            let (block, cell) = fluid_block_and_cell(sid, (sid - 96) as u8, false);
            (block, Vec::new(), cell, false)
        }
        519 => (
            Block {
                state_id: sid,
                flags: CUTOUT,
                occlusion: 0,
                ..Block::AIR
            },
            vec![0],
            FluidCell {
                blocks_motion: true,
                ..FluidCell::default()
            },
            false,
        ),
        2005 => (
            Block {
                state_id: sid,
                flags: CUTOUT | TINTED,
                tint: 0x7cbd6b,
                occlusion: 0,
                ..Block::AIR
            },
            Vec::new(),
            FluidCell::default(),
            false,
        ),
        _ => (
            Block::opaque_cube(sid),
            vec![0],
            FluidCell {
                blocks_motion: true,
                sturdy_faces: ALL_FACES,
                ..FluidCell::default()
            },
            false,
        ),
    }
}

/// Strip the trailing canonical property list from a block-state name.
///
/// Block-state ids are often compared only by their owning block type
/// (`minecraft:oak_stairs`), while enriched canonical names append vanilla
/// properties (`minecraft:oak_stairs[facing=north,...]`). Every block-type
/// classification in this module must use this base form.
pub fn block_state_base_name(name: &str) -> &str {
    name.split_once('[').map_or(name, |(base, _)| base)
}

fn canonical_state_name(block_name: &str) -> String {
    if block_name.contains(':') {
        block_name.to_owned()
    } else {
        format!("minecraft:{block_name}")
    }
}

/// Expand the one-name-per-state table loaded from `block_states_1211.json`
/// into canonical, property-complete vanilla state strings using the sibling
/// `blocks_1211.json` manifest.
///
/// `blocks_1211.json` lists each block's properties in registry Cartesian
/// order. State zero of a block starts at its first property values; the last
/// property is the fastest-varying digit (`offset % num_values`), matching the
/// pinned wire/oracle decoders. Property keys in the emitted string are sorted
/// lexicographically, exactly like vanilla's canonical `BlockState#toString`
/// form.
///
/// The input asset is depth/size bounded and expansion is capped by the target
/// state table length, so malformed manifests cannot cause unbounded work.
/// Returns the number of state names rewritten. Blocks without properties or
/// outside the target table leave their existing names unchanged.
pub fn enrich_state_names_from_blocks_json(
    state_names: &mut [String],
    blocks_json: &[u8],
) -> Result<usize, String> {
    decode_block_registry_names(state_names, blocks_json, |_, _, _, _| {})
}

fn decode_block_registry_names(
    state_names: &mut [String],
    blocks_json: &[u8],
    mut visit: impl FnMut(usize, u32, &str, &megu_core::json::JsonValue),
) -> Result<usize, String> {
    const MAX_BLOCKS_JSON_BYTES: usize = 16 * 1024 * 1024;
    if blocks_json.len() > MAX_BLOCKS_JSON_BYTES {
        return Err("blocks_1211.json exceeds the 16 MiB input bound".to_string());
    }
    let text = std::str::from_utf8(blocks_json)
        .map_err(|_| "blocks_1211.json is not valid UTF-8".to_string())?;
    let limits = megu_core::json::JsonLimits {
        max_depth: 24,
        max_nodes: 2_000_000,
        max_bytes: MAX_BLOCKS_JSON_BYTES,
    };
    let root = megu_core::json::parse_json_with_limits(text, limits, false)
        .map_err(|e| format!("blocks_1211.json: {e}"))?;
    let blocks = root
        .as_array()
        .ok_or_else(|| "blocks_1211.json: top level is not an array".to_string())?;

    let mut rewritten = 0usize;
    for (block_index, entry) in blocks.iter().enumerate() {
        let object = entry
            .as_object()
            .ok_or_else(|| format!("blocks_1211.json: entry {block_index} is not an object"))?;
        let get_field = |key: &str| {
            object
                .iter()
                .find(|(name, _)| name == key)
                .map(|(_, value)| value)
        };
        let raw_name = get_field("name")
            .and_then(megu_core::json::JsonValue::as_str)
            .ok_or_else(|| format!("blocks_1211.json: entry {block_index} has no string name"))?;
        let min_state = get_field("minStateId")
            .and_then(megu_core::json::JsonValue::as_f64)
            .filter(|n| n.fract() == 0.0 && *n >= 0.0 && *n <= u32::MAX as f64)
            .ok_or_else(|| format!("blocks_1211.json: {raw_name} has a bad minStateId"))?
            as u32;
        let max_state = get_field("maxStateId")
            .and_then(megu_core::json::JsonValue::as_f64)
            .filter(|n| n.fract() == 0.0 && *n >= 0.0 && *n <= u32::MAX as f64)
            .ok_or_else(|| format!("blocks_1211.json: {raw_name} has a bad maxStateId"))?
            as u32;
        if max_state < min_state {
            return Err(format!(
                "blocks_1211.json: {raw_name} maxStateId is below minStateId"
            ));
        }
        let state_len = u64::from(max_state) - u64::from(min_state) + 1;
        let properties = get_field("states")
            .map(|value| {
                value
                    .as_array()
                    .ok_or_else(|| format!("blocks_1211.json: {raw_name} states is not an array"))
            })
            .transpose()?
            .unwrap_or_default();
        let base_name = canonical_state_name(raw_name);
        if properties.is_empty() {
            let upper = (u64::from(max_state) + 1).min(state_names.len() as u64) as usize;
            for (state, name) in state_names[..upper]
                .iter_mut()
                .enumerate()
                .skip(min_state as usize)
            {
                if name.is_empty() {
                    name.clone_from(&base_name);
                }
                visit(state, min_state, &base_name, entry);
            }
            continue;
        }

        let mut decoded = Vec::with_capacity(properties.len());
        let mut expected_len = 1u64;
        for (property_index, property) in properties.iter().enumerate() {
            let property_object = property.as_object().ok_or_else(|| {
                format!("blocks_1211.json: {raw_name} states[{property_index}] is not an object")
            })?;
            let property_field = |key: &str| {
                property_object
                    .iter()
                    .find(|(name, _)| name == key)
                    .map(|(_, value)| value)
            };
            let property_name = property_field("name")
                .and_then(megu_core::json::JsonValue::as_str)
                .ok_or_else(|| {
                    format!("blocks_1211.json: {raw_name} states[{property_index}] has no name")
                })?
                .to_owned();
            if property_field("values").is_none()
                && property_field("type").and_then(megu_core::json::JsonValue::as_str)
                    == Some("bool")
            {
                // Vanilla BooleanProperty enumerates true before false.
                expected_len = expected_len
                    .checked_mul(2)
                    .ok_or_else(|| format!("blocks_1211.json: {raw_name} state count overflows"))?;
                decoded.push((property_name, vec!["true".into(), "false".into()]));
                continue;
            }
            let values = property_field("values")
                .and_then(megu_core::json::JsonValue::as_array)
                .ok_or_else(|| {
                    format!("blocks_1211.json: {raw_name}.{property_name} has no values array")
                })?;
            if values.is_empty() || values.len() > 4096 {
                return Err(format!(
                    "blocks_1211.json: {raw_name}.{property_name} has an unsupported value count"
                ));
            }
            if let Some(num_values) = property_field("num_values")
                .and_then(megu_core::json::JsonValue::as_f64)
                .filter(|n| n.fract() == 0.0 && *n >= 1.0)
            {
                if num_values as usize != values.len() {
                    return Err(format!(
                        "blocks_1211.json: {raw_name}.{property_name} num_values disagrees with values"
                    ));
                }
            }
            let mut names = Vec::with_capacity(values.len());
            for value in values {
                // minecraft-data writes most values as strings, but integer
                // properties may be numeric JSON values depending on export
                // lineage. Canonical state strings use bare scalar text.
                let text = match value {
                    megu_core::json::JsonValue::String(text) => text.clone(),
                    megu_core::json::JsonValue::Bool(value) => value.to_string(),
                    megu_core::json::JsonValue::Number(value) if value.is_finite() => {
                        if value.fract() == 0.0 && value.abs() <= i64::MAX as f64 {
                            (*value as i64).to_string()
                        } else {
                            value.to_string()
                        }
                    }
                    _ => {
                        return Err(format!(
                            "blocks_1211.json: {raw_name}.{property_name} value is not scalar"
                        ));
                    }
                };
                names.push(text);
            }
            expected_len = expected_len
                .checked_mul(names.len() as u64)
                .ok_or_else(|| format!("blocks_1211.json: {raw_name} state count overflows"))?;
            if expected_len > state_len.max(1) && expected_len > state_names.len() as u64 {
                return Err(format!(
                    "blocks_1211.json: {raw_name} state count exceeds its input bounds"
                ));
            }
            decoded.push((property_name, names));
        }
        if expected_len != state_len {
            return Err(format!(
                "blocks_1211.json: {raw_name} property product {expected_len} does not match state span {state_len}"
            ));
        }

        for offset in 0..state_len {
            let state = min_state.saturating_add(offset as u32) as usize;
            if state >= state_names.len() {
                break;
            }
            let mut remaining = offset;
            let mut pairs = Vec::with_capacity(decoded.len());
            for (property_name, values) in decoded.iter().rev() {
                let index = (remaining % values.len() as u64) as usize;
                remaining /= values.len() as u64;
                pairs.push((property_name.as_str(), values[index].as_str()));
            }
            pairs.sort_unstable_by_key(|(name, _)| *name);
            let mut canonical = String::with_capacity(base_name.len() + 2 + decoded.len() * 20);
            canonical.push_str(&base_name);
            canonical.push('[');
            for (index, (property, value)) in pairs.iter().enumerate() {
                if index != 0 {
                    canonical.push(',');
                }
                canonical.push_str(property);
                canonical.push('=');
                canonical.push_str(value);
            }
            canonical.push(']');
            visit(state, min_state, &base_name, entry);
            state_names[state] = canonical;
            rewritten += 1;
        }
    }
    Ok(rewritten)
}

// ---------------------------------------------------------------------------
// assets/block_states_1211.json decoding on the canonical megu_core::json
// reader (the file is machine-generated strict JSON)
// ---------------------------------------------------------------------------

struct ParsedBlockStates {
    blocks: Vec<(String, u32, u32)>,
    flags: Vec<u8>,
    full_sides: Vec<u8>,
    shapes: Vec<LocalBox>,
    collision_shapes: Vec<Vec<u16>>,
}

fn json_u64(value: &megu_core::json::JsonValue, what: &str) -> Result<u64, String> {
    match value {
        megu_core::json::JsonValue::Number(n)
            if n.fract() == 0.0 && *n >= 0.0 && *n <= u64::MAX as f64 =>
        {
            Ok(*n as u64)
        }
        _ => Err(format!(
            "block_states_1211.json: expected integer for {what}"
        )),
    }
}

fn json_string(value: &megu_core::json::JsonValue, what: &str) -> Result<String, String> {
    value
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| format!("block_states_1211.json: expected string for {what}"))
}

fn json_array<'a>(
    value: &'a megu_core::json::JsonValue,
    what: &str,
) -> Result<&'a [megu_core::json::JsonValue], String> {
    value
        .as_array()
        .ok_or_else(|| format!("block_states_1211.json: expected array for {what}"))
}

fn parse_state_block_rows(
    value: &megu_core::json::JsonValue,
) -> Result<Vec<(String, u32, u32)>, String> {
    let items = json_array(value, "blocks")?;
    let mut out = Vec::with_capacity(items.len());
    for (i, item) in items.iter().enumerate() {
        let row = json_array(item, "blocks row")
            .map_err(|_| format!("block_states_1211.json: malformed blocks row {i}"))?;
        if row.len() != 3 {
            return Err(format!(
                "block_states_1211.json: blocks row {i} is not a 3-tuple"
            ));
        }
        out.push((
            json_string(&row[0], "blocks row name")?,
            json_u64(&row[1], "blocks row base")? as u32,
            json_u64(&row[2], "blocks row length")? as u32,
        ));
    }
    Ok(out)
}

fn parse_u8_list(value: &megu_core::json::JsonValue, what: &str) -> Result<Vec<u8>, String> {
    json_array(value, what)?
        .iter()
        .enumerate()
        .map(|(i, v)| {
            json_u64(v, what)
                .map(|n| n as u8)
                .map_err(|_| format!("block_states_1211.json: bad {what} entry {i}"))
        })
        .collect()
}

fn parse_shape_list(value: &megu_core::json::JsonValue) -> Result<Vec<LocalBox>, String> {
    let items = json_array(value, "shapes")?;
    let mut out = Vec::with_capacity(items.len());
    for (i, item) in items.iter().enumerate() {
        if !matches!(item, megu_core::json::JsonValue::Object(_)) {
            return Err(format!(
                "block_states_1211.json: shapes entry {i} is not an object"
            ));
        }
        out.push(LocalBox {
            min: shape_axis(item, "min", i)?,
            max: shape_axis(item, "max", i)?,
        });
    }
    Ok(out)
}

fn shape_axis(
    shape: &megu_core::json::JsonValue,
    key: &str,
    index: usize,
) -> Result<[f64; 3], String> {
    let Some(axis) = shape.get(key) else {
        return Ok([0.0; 3]);
    };
    let items = json_array(axis, "shape axis").map_err(|_| {
        format!("block_states_1211.json: shapes entry {index} has a bad '{key}' axis")
    })?;
    if items.len() != 3 {
        return Err(format!(
            "block_states_1211.json: shapes entry {index} '{key}' is not a triple"
        ));
    }
    let mut out = [0.0; 3];
    for (slot, item) in out.iter_mut().zip(items.iter()) {
        *slot = item.as_f64().ok_or_else(|| {
            format!("block_states_1211.json: shapes entry {index} '{key}' is not numeric")
        })?;
    }
    Ok(out)
}

fn parse_collision_shapes(value: &megu_core::json::JsonValue) -> Result<Vec<Vec<u16>>, String> {
    let items = json_array(value, "collision_shapes")?;
    let mut out = Vec::with_capacity(items.len());
    for (i, item) in items.iter().enumerate() {
        let indices = json_array(item, "collision_shapes row")
            .map_err(|_| format!("block_states_1211.json: malformed collision_shapes row {i}"))?;
        let mut list = Vec::with_capacity(indices.len());
        for (j, index) in indices.iter().enumerate() {
            list.push(
                json_u64(index, "collision_shapes index").map_err(|_| {
                    format!("block_states_1211.json: bad collision_shapes[{i}][{j}]")
                })? as u16,
            );
        }
        out.push(list);
    }
    Ok(out)
}

fn parse_block_states_json(raw: &[u8]) -> Result<ParsedBlockStates, String> {
    let text = std::str::from_utf8(raw)
        .map_err(|_| "block_states_1211.json is not valid UTF-8".to_string())?;
    // The asset holds ~26k states and ~300k values; bound depth and input but
    // leave the node budget effectively unbounded like the old scanner did.
    let limits = megu_core::json::JsonLimits {
        max_depth: 64,
        max_nodes: usize::MAX,
        max_bytes: usize::MAX,
    };
    let root = megu_core::json::parse_json_with_limits(text, limits, false)
        .map_err(|e| format!("block_states_1211.json: {e}"))?;
    let Some(entries) = root.as_object() else {
        return Err("block_states_1211.json: top level is not an object".to_string());
    };
    let mut parsed = ParsedBlockStates {
        blocks: Vec::new(),
        flags: Vec::new(),
        full_sides: Vec::new(),
        shapes: Vec::new(),
        collision_shapes: Vec::new(),
    };
    for (key, value) in entries {
        match key.as_str() {
            "blocks" => parsed.blocks = parse_state_block_rows(value)?,
            "flags" => parsed.flags = parse_u8_list(value, "flags")?,
            "full_sides" => parsed.full_sides = parse_u8_list(value, "full_sides")?,
            "shapes" => parsed.shapes = parse_shape_list(value)?,
            "collision_shapes" => parsed.collision_shapes = parse_collision_shapes(value)?,
            _ => {}
        }
    }
    Ok(parsed)
}

// ---------------------------------------------------------------------------
// Chunk Column + Live MeshCache World State
// ---------------------------------------------------------------------------

/// Shared empty overlay for sections without any dynamic mod states.
static EMPTY_DYNAMIC_STATES: BTreeMap<usize, u32> = BTreeMap::new();

pub struct StoredSection {
    pub palette: PalettedSection,
    pub block_light: [u8; LIGHT_BYTES],
    pub sky_light: [u8; LIGHT_BYTES],
    /// Biome registry ids for the section's 4×4×4 cells (y, z, x order)
    /// — patched in place by `chunks_biomes` and seeded from
    /// `level_chunk_with_light`.
    pub biomes: [u32; 64],
    pub has_non_air: bool,
}

impl StoredSection {
    fn palette_state(state: u32, bound: u32) -> (u32, Option<u32>) {
        if state >= 0x8000 {
            // Values at/above the 15-bit direct-encoding limit are dynamic
            // mod states in this protocol. Store air in the legacy palette
            // and retain the exact state in the sparse overlay.
            (0, Some(state))
        } else {
            (state.min(bound), None)
        }
    }

    fn state_at(&self, index: usize, dynamic: &BTreeMap<usize, u32>) -> u32 {
        dynamic
            .get(&index)
            .copied()
            .or_else(|| self.palette.get(index).ok())
            .unwrap_or(0)
    }

    /// Content identity between a freshly decoded section and the stored one.
    /// Equal cells (including the sparse dynamic overlay), light and biomes
    /// mean a remesh would reproduce the resident mesh byte for byte.
    fn same_content(
        &self,
        decoded: &Self,
        self_dynamic: &BTreeMap<usize, u32>,
        decoded_dynamic: &BTreeMap<usize, u32>,
    ) -> bool {
        self.block_light == decoded.block_light
            && self.sky_light == decoded.sky_light
            && self.biomes == decoded.biomes
            && (0..SECTION_CELLS).all(|idx| {
                self.state_at(idx, self_dynamic) == decoded.state_at(idx, decoded_dynamic)
            })
    }

    fn set_state(
        &mut self,
        index: usize,
        state: u32,
        dynamic: &mut BTreeMap<usize, u32>,
    ) -> Result<(), String> {
        if state >= 0x8000 {
            dynamic.insert(index, state);
            self.palette
                .set(index, 0)
                .map_err(|e| format!("PalettedSection::set failed: {e:?}"))?;
        } else {
            dynamic.remove(&index);
            self.palette
                .set(index, state)
                .map_err(|e| format!("PalettedSection::set failed: {e:?}"))?;
        }
        Ok(())
    }

    fn from_states(
        states: &[u32; SECTION_CELLS],
        block_light: [u8; LIGHT_BYTES],
        sky_light: [u8; LIGHT_BYTES],
        blocks: &BlockTable,
        state_count: usize,
        dynamic_states: &mut BTreeMap<usize, u32>,
    ) -> Result<Self, String> {
        let bound = u32::try_from(state_count.saturating_sub(1)).unwrap_or(u32::MAX);
        let (first, first_dynamic) = Self::palette_state(states[0], bound);
        let mut palette = PalettedSection::new_single_bounded(first, state_count)
            .map_err(|e| format!("PalettedSection::new_single_bounded failed: {e:?}"))?;
        dynamic_states.clear();
        if let Some(state) = first_dynamic {
            dynamic_states.insert(0, state);
        }
        let first_exact = first_dynamic.unwrap_or(first);
        let mut has_non_air = !blocks.is_air(first_exact);
        for (idx, &raw_state) in states.iter().enumerate() {
            let (stored, dynamic) = Self::palette_state(raw_state, bound);
            if idx != 0 && stored != first {
                palette
                    .set(idx, stored)
                    .map_err(|e| format!("PalettedSection::set failed: {e:?}"))?;
            }
            if let Some(state) = dynamic {
                dynamic_states.insert(idx, state);
            }
            let exact = dynamic.unwrap_or(stored);
            if !blocks.is_air(exact) {
                has_non_air = true;
            }
        }
        Ok(Self {
            palette,
            block_light,
            sky_light,
            biomes: [0; 64],
            has_non_air,
        })
    }

    fn to_mesh_section(&self, blocks: &BlockTable, dynamic: &BTreeMap<usize, u32>) -> Section {
        let mut unpacked = [0u32; SECTION_CELLS];
        self.palette.unpack_into(&mut unpacked);
        for (&index, &state) in dynamic {
            unpacked[index] = state;
        }
        let mut mesh_blocks = Box::new([Block::AIR; SECTION_CELLS]);
        for (index, (dst, &sid)) in mesh_blocks.iter_mut().zip(unpacked.iter()).enumerate() {
            let x = index % 16;
            let z = (index / 16) % 16;
            let y = index / 256;
            *dst = blocks.mesh_block_for_biome(sid, self.biome_at(x, y, z));
        }
        Section {
            blocks: mesh_blocks,
            block_light: self.block_light,
            sky_light: self.sky_light,
        }
    }

    fn biome_at(&self, x: usize, y: usize, z: usize) -> u32 {
        self.biomes[(y / 4) * 16 + (z / 4) * 4 + x / 4]
    }

    /// The meshing snapshot in [`CompressedSection`] form. A single-state
    /// palette skips the dense block array entirely, so uniform stone,
    /// deepslate, water and air sections enter `MeshCache` with zero
    /// block-storage heap bytes while staying bit-identical to
    /// [`to_mesh_section`](Self::to_mesh_section) plus the
    /// `CompressedSection::from` collapse.
    fn to_compressed_mesh_section(
        &self,
        blocks: &BlockTable,
        dynamic: &BTreeMap<usize, u32>,
    ) -> CompressedSection {
        let uniform_biome = self.biomes[1..]
            .iter()
            .all(|&biome| biome == self.biomes[0]);
        if dynamic.is_empty() {
            if let Some(state) = self.palette.uniform_state().filter(|_| uniform_biome) {
                return CompressedSection {
                    blocks: SectionBlocks::Uniform(
                        blocks.mesh_block_for_biome(state, self.biomes[0]),
                    ),
                    block_light: self.block_light,
                    sky_light: self.sky_light,
                };
            }
        }
        CompressedSection::from(self.to_mesh_section(blocks, dynamic))
    }
}

/// [`FaceVisibility::compute`] for a [`CompressedSection`] block array: a
/// uniform section resolves in O(1) to `uniform_air`/`sealed_solid` without
/// materializing the dense block array. `blocks_sight` mirrors the
/// client-port predicate — only a full-occlusion opaque cube stops sight.
fn face_visibility_of(blocks: &SectionBlocks) -> FaceVisibility {
    match blocks {
        SectionBlocks::Uniform(block) => {
            if block.flags & OPAQUE != 0 && block.occlusion & ALL_FACES == ALL_FACES {
                FaceVisibility::sealed_solid()
            } else {
                FaceVisibility::uniform_air()
            }
        }
        SectionBlocks::Dense(dense) => FaceVisibility::compute(dense),
        SectionBlocks::Palette { .. } => {
            let expanded = Box::new(std::array::from_fn(|i| blocks.cell(i)));
            FaceVisibility::compute(&expanded)
        }
    }
}

/// Identity key for one translucent quad inside a released mesh: the bit
/// pattern of its four section-local corner positions plus its face
/// direction. The greedy mesher never emits two quads covering the same
/// face of the same cells, so the key is unique per section.
type TranslucentSlotKey = ([u32; 12], u8);

fn translucent_slot_key(quad: &Quad) -> TranslucentSlotKey {
    let mut bits = [0u32; 12];
    for (dst, pos) in bits.as_chunks_mut::<3>().0.iter_mut().zip(quad.positions) {
        *dst = pos.map(f32::to_bits);
    }
    (bits, quad.direction as u8)
}

/// `client-port`'s crate-private `is_translucent`: the quads the
/// camera-driven re-sort keeps and reorders — translucent blocks and water.
fn quad_is_translucent(quad: &Quad) -> bool {
    quad.layer == Layer::Translucent || (quad.layer == Layer::Fluid && quad.fluid_kind == WATER)
}

pub struct ChunkColumn {
    pub cx: i32,
    pub cz: i32,
    pub sections: Vec<StoredSection>,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct MeshMetrics {
    pub sections_rebuilt: u64,
    pub total_mesh_us: u64,
    pub total_pack_us: u64,
    pub total_sort_us: u64,
    pub total_quads: usize,
    pub packed_vertices: usize,
    pub translucent_quads: usize,
}

impl MeshMetrics {
    pub fn avg_us_per_section(&self) -> f64 {
        if self.sections_rebuilt == 0 {
            0.0
        } else {
            self.total_mesh_us as f64 / self.sections_rebuilt as f64
        }
    }
}

/// Visibility-graph node for one loaded section: the 6x6 `FaceVisibility`
/// reachability matrix plus whether the section can emit geometry for
/// `GraphCuller`.
#[derive(Clone, Copy, Debug)]
struct SectionVisibility {
    faces: FaceVisibility,
    renderable: bool,
}

/// Validated model assets and the deterministic mapping from wire clip IDs to
/// animation names. The first clip in `register_model` is wire clip ID 0.
struct RegisteredAnimationModel {
    rig: SkeletonRig,
    clips: Vec<AnimationClip>,
    clip_names: HashMap<(u16, u8), String>,
    clip_ids: HashMap<String, u16>,
}

struct EntityAnimation {
    model_id: String,
    controller: AnimationController,
}

/// Client-side skeletal animation state. ANM1 packets target entities already
/// bound to registered models; the host never creates entities from packets.
#[derive(Default)]
pub struct WorldAnimationHost {
    models: HashMap<String, RegisteredAnimationModel>,
    entities: HashMap<i32, EntityAnimation>,
}

/// Alternative name for the world's model and entity animation registry.
pub type EntityAnimationRegistry = WorldAnimationHost;

fn loop_mode_id(mode: LoopMode) -> u8 {
    match mode {
        LoopMode::Once => 0,
        LoopMode::Loop => 1,
        LoopMode::HoldOnLastFrame => 2,
    }
}

impl WorldAnimationHost {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register clips in their wire-ID order (0 through `clips.len() - 1`).
    pub fn register_model(
        &mut self,
        model_id: impl Into<String>,
        rig: SkeletonRig,
        clips: Vec<AnimationClip>,
    ) -> Result<(), AnimationError> {
        let numbered = clips
            .into_iter()
            .enumerate()
            .map(|(id, clip)| (id as u16, clip))
            .collect();
        self.register_model_with_clip_ids(model_id, rig, numbered)
    }

    /// Register explicit ANM1 clip IDs. Duplicate model, clip or wire IDs fail
    /// before the registry is modified.
    pub fn register_model_with_clip_ids(
        &mut self,
        model_id: impl Into<String>,
        rig: SkeletonRig,
        mut clips: Vec<(u16, AnimationClip)>,
    ) -> Result<(), AnimationError> {
        let model_id = model_id.into();
        if model_id.is_empty() || self.models.contains_key(&model_id) {
            return Err(AnimationError::InvalidRig);
        }
        if clips.len() > MAX_CLIPS {
            return Err(AnimationError::LimitExceeded);
        }
        clips.sort_by_key(|(id, _)| *id);
        let mut native = Vec::with_capacity(clips.len());
        let mut clip_names = HashMap::new();
        let mut clip_ids = HashMap::new();
        for (id, clip) in &clips {
            if *id > 4095
                || clip_ids.insert(clip.name.clone(), *id).is_some()
                || native
                    .last()
                    .is_some_and(|(previous, _): &(u16, AnimationClip)| previous == id)
            {
                return Err(AnimationError::InvalidClip);
            }
            native.push((*id, clip.clone()));
        }
        // Let the kernel validate tracks, bone names and clip contents before
        // constructing wire-facing variants.
        AnimationController::new(rig.clone(), native.iter().map(|(_, c)| c.clone()).collect())?;

        let mut controller_clips = Vec::with_capacity(MAX_CLIPS.min(clips.len() * 3));
        for (id, mut clip) in native {
            let original_mode = loop_mode_id(clip.loop_mode);
            let internal_name = format!("anm1.{id}.{original_mode}");
            clip.name = internal_name.clone();
            clip_names.insert((id, original_mode), internal_name);
            controller_clips.push(clip);
        }
        // ANM1 chooses loop mode per trigger. Populate alternate modes while
        // respecting the kernel's MAX_CLIPS bound; unavailable overrides fail
        // explicitly instead of silently playing with incorrect loop behavior.
        for (id, clip) in &clips {
            for mode in [LoopMode::Once, LoopMode::Loop, LoopMode::HoldOnLastFrame] {
                if controller_clips.len() == MAX_CLIPS {
                    break;
                }
                let mode_id = loop_mode_id(mode);
                if clip_names.contains_key(&(*id, mode_id)) {
                    continue;
                }
                let mut variant = clip.clone();
                variant.name = format!("anm1.{id}.{mode_id}");
                variant.loop_mode = mode;
                clip_names.insert((*id, mode_id), variant.name.clone());
                controller_clips.push(variant);
            }
        }
        self.models.insert(
            model_id,
            RegisteredAnimationModel {
                rig,
                clips: controller_clips,
                clip_names,
                clip_ids,
            },
        );
        Ok(())
    }

    /// Load a single Bedrock/GeckoLib geometry and animation document.
    /// Parsed clips are sorted by name for stable wire clip IDs.
    pub fn register_model_json(
        &mut self,
        model_id: impl Into<String>,
        geometry_json: &str,
        animation_json: &str,
    ) -> Result<(), AnimationError> {
        let rig = parse_geometry_json(geometry_json)?;
        let mut clips = parse_animation_json(animation_json)?;
        clips.sort_by(|a, b| a.name.cmp(&b.name));
        self.register_model(model_id, rig, clips)
    }

    /// Register an OptiFine `.jem` entity model from a resource pack's
    /// `optifine/cem/` directory. JEM files carry geometry only — the model
    /// binds with no clips and drives poses through bind/entity state.
    pub fn register_jem_model(
        &mut self,
        model_id: impl Into<String>,
        jem_json: &str,
    ) -> Result<(), AnimationError> {
        let rig = parse_jem_model(jem_json)?;
        self.register_model(model_id, rig, Vec::new())
    }

    /// Register an OptiFine `.jpm` part model the same way; the document root
    /// is the part tree itself.
    pub fn register_jpm_model(
        &mut self,
        model_id: impl Into<String>,
        jpm_json: &str,
    ) -> Result<(), AnimationError> {
        let rig = parse_jpm_model(jpm_json)?;
        self.register_model(model_id, rig, Vec::new())
    }

    /// Find the ANM1 clip ID for a model's registered animation name.
    pub fn clip_id(&self, model_id: &str, clip_name: &str) -> Option<u16> {
        self.models.get(model_id)?.clip_ids.get(clip_name).copied()
    }

    /// Bind a spawned entity to a model, with a full-body base layer.
    pub fn bind_entity(&mut self, entity_id: i32, model_id: &str) -> Result<(), AnimationError> {
        let model = self
            .models
            .get(model_id)
            .ok_or(AnimationError::InvalidRig)?;
        let mask = BoneMask::all(&model.rig);
        self.bind_entity_with_layers(entity_id, model_id, vec![(mask, 1.0)])
    }

    /// Bind an entity with ordered layers, e.g. a full-body walk layer and an
    /// upper-body-only cast layer. Binding the same entity replaces its pose.
    pub fn bind_entity_with_layers(
        &mut self,
        entity_id: i32,
        model_id: &str,
        layers: Vec<(BoneMask, f32)>,
    ) -> Result<(), AnimationError> {
        if entity_id < 0 {
            return Err(AnimationError::InvalidWire);
        }
        let model = self
            .models
            .get(model_id)
            .ok_or(AnimationError::InvalidRig)?;
        let mut controller = AnimationController::new(model.rig.clone(), model.clips.clone())?;
        for (mask, weight) in layers {
            controller.add_layer(mask, weight)?;
        }
        self.entities.insert(
            entity_id,
            EntityAnimation {
                model_id: model_id.to_owned(),
                controller,
            },
        );
        Ok(())
    }

    /// Add a masked layer without restarting any playing layers.
    pub fn add_entity_layer(
        &mut self,
        entity_id: i32,
        mask: BoneMask,
        weight: f32,
    ) -> Result<usize, AnimationError> {
        self.entities
            .get_mut(&entity_id)
            .ok_or(AnimationError::InvalidWire)?
            .controller
            .add_layer(mask, weight)
    }

    /// Ingest a complete `megu:anim_v1` (ANM1) custom-payload body.
    /// Protocol fade ticks use Minecraft's 20 ticks per second.
    pub fn ingest_anm1_payload(&mut self, bytes: &[u8]) -> Result<(), AnimationError> {
        let trigger = decode_animation_trigger(bytes)?;
        let entity = self
            .entities
            .get_mut(&trigger.entity_id)
            .ok_or(AnimationError::InvalidWire)?;
        let model = self
            .models
            .get(&entity.model_id)
            .ok_or(AnimationError::InvalidRig)?;
        let name = model
            .clip_names
            .get(&(trigger.clip_id, loop_mode_id(trigger.loop_mode)))
            .ok_or(AnimationError::UnknownClip)?;
        while entity.controller.layers().len() <= usize::from(trigger.layer) {
            entity
                .controller
                .add_layer(BoneMask::all(&model.rig), 1.0)?;
        }
        entity.controller.play(
            usize::from(trigger.layer),
            name,
            f32::from(trigger.fade_ticks) / 20.0,
            trigger.speed,
            trigger.restart,
        )
    }

    pub fn advance(&mut self, dt_sec: f32) -> Result<(), AnimationError> {
        if !dt_sec.is_finite() || dt_sec < 0.0 {
            return Err(AnimationError::InvalidTime);
        }
        for entity in self.entities.values_mut() {
            entity.controller.advance(dt_sec)?;
        }
        Ok(())
    }

    /// One column-major model-space matrix per bone, ordered as in the rig.
    /// Despawned and unbound entities return `Ok(None)`.
    pub fn evaluate_entity_pose(
        &self,
        entity_id: i32,
    ) -> Result<Option<Vec<[f32; 16]>>, AnimationError> {
        self.entities
            .get(&entity_id)
            .map(|entity| entity.controller.evaluate_world_matrices())
            .transpose()
    }

    pub fn despawn_entity(&mut self, entity_id: i32) -> bool {
        self.entities.remove(&entity_id).is_some()
    }

    pub fn clear_entities(&mut self) {
        self.entities.clear();
    }

    pub fn entity_count(&self) -> usize {
        self.entities.len()
    }
}

// ---------------------------------------------------------------------------
// Live entity / block-entity / sky / block-break tracking (Protocol 767 S2C)
// ---------------------------------------------------------------------------

/// Upper bound on simultaneously tracked live entities. Spawn packets past
/// the cap are ignored so a hostile server cannot grow the map unboundedly.
pub const MAX_TRACKED_ENTITIES: usize = 4096;
/// Upper bound on tracked block entities across every loaded column.
pub const MAX_TRACKED_BLOCK_ENTITIES: usize = 8192;
/// Upper bound on concurrent block-break crack overlays (one per breaker).
pub const MAX_BLOCK_DESTRUCTIONS: usize = 256;
/// Vanilla hurt-flash duration in ticks (`Entity#animateHurt` /
/// `hurt_animation`); `entity_event` status 2 sets the same timer.
const HURT_FLASH_TICKS: u8 = 10;
/// `VecDeltaCodec` scale: positions quantize to 1/4096 block on the wire.
const POS_Q_PER_BLOCK: f64 = 4096.0;
/// `game_event` ids for the weather family (vanilla `GameEvent` ordinals).
const GAME_EVENT_START_RAINING: u8 = 1;
const GAME_EVENT_STOP_RAINING: u8 = 2;
const GAME_EVENT_RAIN_LEVEL_CHANGE: u8 = 7;
const GAME_EVENT_THUNDER_LEVEL_CHANGE: u8 = 8;

/// Non-finite wire floats read as `0.0` — the same sanitize convention the
/// kernel's sky/fog evaluator uses — so NaN/inf can never poison tracked
/// entity or weather state.
fn clean_f64(v: f64) -> f64 {
    if v.is_finite() { v } else { 0.0 }
}

fn clean_f32(v: f32) -> f32 {
    if v.is_finite() { v } else { 0.0 }
}

/// Weather levels clamp to `0.0..=1.0`; non-finite input reads as `0.0`.
fn clean_weather_level(v: f32) -> f32 {
    clean_f32(v).clamp(0.0, 1.0)
}

/// Round a block-space value (coordinate or delta) to 1/4096-block units —
/// the same round-half-up the server's `VecDeltaCodec` move encoder uses
/// (`floor(s) + (frac >= 0.5)`), so `pos_q` mirrors the wire quantization.
/// Wire move deltas arrive as `raw / 4096.0`, so `quantize_pos_4096(delta)`
/// recovers the exact i16; absolute sets quantize the coordinate itself.
fn quantize_pos_4096(v: f64) -> i64 {
    let scaled = v * POS_Q_PER_BLOCK;
    if !scaled.is_finite() {
        return 0;
    }
    let floor = scaled.floor();
    (floor + f64::from(scaled - floor >= 0.5)) as i64
}

/// Can `map` take `key` without exceeding `cap`? A present key always can
/// (updates replace in place), which is the one bounded-growth rule every
/// tracked-entity family shares.
fn has_capacity<K: Ord, V>(map: &BTreeMap<K, V>, key: &K, cap: usize) -> bool {
    map.contains_key(key) || map.len() < cap
}

/// Bounded upsert: replace a present key, drop a new key once `map` is full.
fn bounded_insert<K: Ord, V>(map: &mut BTreeMap<K, V>, key: K, value: V, cap: usize) {
    if has_capacity(map, &key, cap) {
        map.insert(key, value);
    }
}

/// Bounded push: keep the newest `cap` records of an arrival-ordered queue.
fn push_bounded<T>(queue: &mut VecDeque<T>, value: T, cap: usize) {
    if queue.len() >= cap {
        queue.pop_front();
    }
    queue.push_back(value);
}

pub const MAP_SIDE: usize = 128;
const MAX_TRACKED_MAPS: usize = 128;
const MAX_TRACKED_TEAMS: usize = 1024;
const MAX_TEAM_MEMBERS: usize = 16_384;

/// The 1.21.1 map palette (`MapColor` IDs, then `Brightness` IDs). Map
/// pixels are not block registry IDs. Both unused IDs and NONE are transparent.
fn map_pixel(packed: u8) -> [u8; 4] {
    const COLORS: [u32; 62] = [
        0, 8368696, 16247203, 13092807, 16711680, 10526975, 10987431, 31744, 16777215, 10791096,
        9923917, 7368816, 4210943, 9402184, 16776437, 14188339, 11685080, 6724056, 15066419,
        8375321, 15892389, 5000268, 10066329, 5013401, 8339378, 3361970, 6704179, 6717235,
        10040115, 1644825, 16445005, 6085589, 4882687, 55610, 8476209, 7340544, 13742497, 10441252,
        9787244, 7367818, 12223780, 6780213, 10505550, 3746083, 8874850, 5725276, 8014168, 4996700,
        4993571, 5001770, 9321518, 2430480, 12398641, 9715553, 6035741, 1474182, 3837580, 5647422,
        1356933, 6579300, 14200723, 8365974,
    ];
    let rgb = COLORS.get(usize::from(packed >> 2)).copied().unwrap_or(0);
    if rgb == 0 {
        return [0; 4];
    }
    let shade = [180, 220, 255, 135][usize::from(packed & 3)];
    let channel = |shift: u32| (((rgb >> shift) & 255) * shade / 255) as u8;
    [channel(16), channel(8), channel(0), 255]
}

#[derive(Clone, Debug)]
pub struct TrackedMap {
    pub scale: i8,
    pub locked: bool,
    pub pixels: Arc<[[u8; 4]]>,
    pub icons: Arc<[MapItemIcon]>,
}

#[derive(Clone, Debug)]
pub struct ScoreboardTeam {
    pub info: PlayerTeamInfo,
    pub members: BTreeSet<String>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CombatState {
    pub in_combat: bool,
    /// Elapsed client ticks while active, or the server duration after End.
    pub ticks: u32,
    /// Last causing entity from `damage_event`, not the victim ID from Kill.
    pub last_attacker_id: Option<i32>,
}

impl CombatState {
    pub fn advance_ticks(&mut self, elapsed: u32) {
        if self.in_combat {
            self.ticks = self.ticks.saturating_add(elapsed);
        }
    }
}

fn component_text(bytes: &[u8]) -> String {
    WireReader::new(bytes)
        .read_nbt()
        .map(|v| v.extract_text())
        .unwrap_or_default()
}

/// One live entity tracked from the Protocol 767 S2C entity packets
/// (`add_entity`, the `move_entity_*` family, `teleport_entity`,
/// `rotate_head`, `set_entity_motion`, `hurt_animation`, `entity_event`,
/// `set_equipment`, `remove_entities`). Only `add_entity` creates rows;
/// every other packet updates an already-tracked id or is ignored.
#[derive(Clone, Debug)]
pub struct TrackedEntity {
    /// Server entity id.
    pub eid: i32,
    /// Spawn UUID exactly as received on `add_entity`.
    pub uuid: [u8; 16],
    /// Entity-type registry id from `add_entity`.
    pub entity_type: i32,
    /// Position in 1/4096-block fixed point — the client-side `VecDeltaCodec`
    /// accumulator. Absolute sets quantize the coordinate; relative moves add
    /// the decoded delta, so accumulation is exact integer arithmetic.
    pub pos_q: [i64; 3],
    /// Position in blocks — vanilla keeps full f64 precision here.
    pub pos: [f64; 3],
    /// Velocity in blocks/tick (`set_entity_motion`, wire i16 / 8000).
    pub vel: [f64; 3],
    /// Body yaw in degrees (`byte * 360 / 256` on the wire).
    pub yaw_deg: f32,
    /// Pitch in degrees.
    pub pitch_deg: f32,
    /// Head yaw in degrees (`rotate_head`, spawn, `*_pos_rot`, teleport).
    pub head_yaw_deg: f32,
    pub on_ground: bool,
    /// Remaining hurt-flash ticks; set to [`HURT_FLASH_TICKS`] by
    /// `hurt_animation`/`entity_event` status 2 and counted down by
    /// [`WorldState::tick_entities`].
    pub hurt_ticks: u8,
    /// Most recently received vanilla entity animation id (0 main hand,
    /// 3 off hand, 4 critical, 5 magic critical); consumers can animate it.
    pub last_animation: Option<u8>,
    /// Bitmask of equipment slot IDs seen in `set_equipment` (bits 0..=7).
    pub equipment_slots: u8,
}

/// One block entity tracked from chunk payloads and `block_entity_data`
/// updates, keyed by world `[x, y, z]` in [`WorldState::block_entities`].
#[derive(Clone, Debug)]
pub struct TrackedBlockEntity {
    /// Wire block-entity type id.
    pub kind: i32,
    /// Raw anonymous-NBT payload exactly as received on the wire (empty when
    /// the packet carried the absent-NBT marker).
    pub nbt_bytes: Vec<u8>,
}

/// Crack-overlay state for one block being mined, keyed by breaker entity id
/// in [`WorldState::block_destructions`].
#[derive(Clone, Copy, Debug)]
pub struct BlockBreakProgress {
    /// Block position being mined.
    pub pos: [i32; 3],
    /// Break stage `0..=9` — higher is more cracked. The overlay clears at
    /// stage `>= 10` or the wire `-1` sentinel (255 as `u8`).
    pub stage: u8,
}

/// One `level_particles` spawn request — the vanilla particle emitter
/// parameters (drained by the particle stage, capped at 256 records).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LevelParticleSpawn {
    /// Palette particle id.
    pub particle_id: i32,
    /// `true` = visible beyond the normal 32-block particle range.
    pub long_distance: bool,
    /// Spawn position.
    pub pos: [f64; 3],
    /// Per-axis spread (each `0..=` block radius).
    pub offset: [f32; 3],
    /// Particle velocity scale.
    pub max_speed: f32,
    /// Spawn count (`0` = the vanilla single directional burst).
    pub count: i32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TrackedBossBar {
    pub name_nbt: Vec<u8>,
    pub progress: f32,
    pub color: i32,
    pub divisions: i32,
    pub flags: u8,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TrackedMobEffect {
    pub amplifier: i32,
    pub duration: i32,
    pub flags: u8,
}

/// Renderer-facing entity fields projected from Protocol 767 metadata and timers.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct EntityVisualState {
    pub on_fire: bool,
    pub crouching: bool,
    pub sprinting: bool,
    pub swimming: bool,
    pub invisible: bool,
    pub glowing: bool,
    pub is_baby: bool,
    pub health: Option<f32>,
    pub pose: Option<i32>,
    pub hurt_ticks: u8,
    pub swing_ticks: u8,
    pub sheep_color: Option<u8>,
    pub sheep_sheared: Option<bool>,
    pub creeper_state: Option<i32>,
    pub creeper_charged: Option<bool>,
    pub villager_data: Option<(i32, i32, i32)>,
    pub shulker_peek: Option<u8>,
    pub wither_invul_ticks: Option<i32>,
    pub warden_anger: Option<i32>,
    pub enderman_carried_block: Option<i32>,
    pub dropped_item_stack: Option<Vec<u8>>,
}

/// Decoded visual state for a tracked block entity or its latest `0x55` event.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BlockEntityVisualState {
    Chest {
        open_count: u8,
        trapped: bool,
        ender: bool,
    },
    ShulkerBox {
        open_count: u8,
        color: u8,
    },
    Bell {
        ring_dir: u8,
        ringing_ticks: u16,
    },
    Sign {
        front_lines: [String; 4],
        back_lines: [String; 4],
        front_glowing: bool,
        back_glowing: bool,
    },
    Banner {
        base_color: u8,
        pattern_count: u8,
    },
    Beacon {
        level: i32,
        primary_effect: i32,
        secondary_effect: i32,
    },
    EnchantingTable {
        book_open: bool,
    },
    Campfire {
        item_count: u8,
    },
    TrialSpawner {
        ominous: bool,
        active: bool,
    },
    Vault {
        ominous: bool,
        state: u8,
    },
    Piston {
        extending: bool,
        facing: u8,
    },
    Generic,
}

fn nbt_field<'a>(value: &'a NbtValue, key: &str) -> Option<&'a NbtValue> {
    match value {
        NbtValue::Compound(fields) => fields.get(key),
        _ => None,
    }
}

fn nbt_i32(value: &NbtValue, key: &str) -> Option<i32> {
    nbt_field(value, key).and_then(NbtValue::as_i32)
}

fn nbt_string<'a>(value: &'a NbtValue, key: &str) -> Option<&'a str> {
    match nbt_field(value, key)? {
        NbtValue::String(text) => Some(text),
        _ => None,
    }
}

fn nbt_bool(value: &NbtValue, key: &str) -> bool {
    nbt_i32(value, key).is_some_and(|value| value != 0)
}

fn nbt_u8(value: &NbtValue, key: &str) -> Option<u8> {
    nbt_i32(value, key).and_then(|value| u8::try_from(value).ok())
}

fn nbt_u16(value: &NbtValue, key: &str) -> Option<u16> {
    nbt_i32(value, key).and_then(|value| u16::try_from(value).ok())
}

/// Element count of an NBT list field, clamped to `u8`.
fn nbt_list_len(value: &NbtValue, key: &str) -> Option<u8> {
    match nbt_field(value, key)? {
        NbtValue::List(items) => u8::try_from(items.len()).ok(),
        _ => None,
    }
}

/// Shulker-box dye color from its block-state base name
/// (`minecraft:light_blue_shulker_box` -> `3`).
fn shulker_color_from_name(block_base_name: &str) -> Option<u8> {
    let color = block_base_name
        .rsplit(':')
        .next()?
        .strip_suffix("_shulker_box")?;
    Some(match color {
        "white" => 0,
        "orange" => 1,
        "magenta" => 2,
        "light_blue" => 3,
        "yellow" => 4,
        "lime" => 5,
        "pink" => 6,
        "gray" => 7,
        "light_gray" => 8,
        "cyan" => 9,
        "purple" => 10,
        "blue" => 11,
        "brown" => 12,
        "green" => 13,
        "red" => 14,
        "black" => 15,
        _ => return None,
    })
}

/// Project one tracked block entity's decoded facts (visual kind tail, block
/// name, NBT, newest block event) into the typed visual enum.
fn block_entity_visual_from(
    id_tail: &str,
    block_base_name: &str,
    nbt: Option<&NbtValue>,
    action: Option<u8>,
    parameter: u8,
) -> BlockEntityVisualState {
    match id_tail {
        "chest" | "trapped_chest" | "ender_chest" => BlockEntityVisualState::Chest {
            open_count: match action {
                Some(1) => parameter,
                _ => nbt.and_then(|value| nbt_u8(value, "open")).unwrap_or(0),
            },
            trapped: id_tail == "trapped_chest" || block_base_name.ends_with("trapped_chest"),
            ender: id_tail == "ender_chest" || block_base_name.ends_with("ender_chest"),
        },
        _ if id_tail.ends_with("shulker_box") => BlockEntityVisualState::ShulkerBox {
            open_count: match action {
                Some(1) => parameter,
                _ => nbt
                    .and_then(|value| nbt_u8(value, "Peek").or_else(|| nbt_u8(value, "peek")))
                    .unwrap_or(0),
            },
            color: nbt
                .and_then(|value| nbt_u8(value, "Color"))
                .or_else(|| shulker_color_from_name(block_base_name))
                .unwrap_or(10),
        },
        "bell" => BlockEntityVisualState::Bell {
            ring_dir: parameter,
            ringing_ticks: match action {
                Some(1) => 50,
                _ => nbt.and_then(|value| nbt_u16(value, "ticks")).unwrap_or(0),
            },
        },
        "sign" | "hanging_sign" => BlockEntityVisualState::Sign {
            front_lines: nbt
                .map(|value| nbt_text_lines(value, "front_text"))
                .unwrap_or_default(),
            back_lines: nbt
                .map(|value| nbt_text_lines(value, "back_text"))
                .unwrap_or_default(),
            front_glowing: nbt
                .and_then(|value| nbt_field(value, "front_text"))
                .is_some_and(|side| nbt_bool(side, "has_glowing_text")),
            back_glowing: nbt
                .and_then(|value| nbt_field(value, "back_text"))
                .is_some_and(|side| nbt_bool(side, "has_glowing_text")),
        },
        "banner" | "wall_banner" => BlockEntityVisualState::Banner {
            base_color: nbt.and_then(|value| nbt_u8(value, "Base")).unwrap_or(0),
            pattern_count: nbt
                .and_then(|value| nbt_list_len(value, "Patterns"))
                .unwrap_or(0),
        },
        "beacon" => BlockEntityVisualState::Beacon {
            level: nbt.and_then(|value| nbt_i32(value, "Levels")).unwrap_or(0),
            primary_effect: nbt.and_then(|value| nbt_i32(value, "Primary")).unwrap_or(0),
            secondary_effect: nbt
                .and_then(|value| nbt_i32(value, "Secondary"))
                .unwrap_or(0),
        },
        "enchanting_table" => BlockEntityVisualState::EnchantingTable {
            book_open: nbt.is_some_and(|value| nbt_bool(value, "BookOpen")),
        },
        "campfire" | "soul_campfire" => BlockEntityVisualState::Campfire {
            item_count: nbt
                .and_then(|value| nbt_list_len(value, "Items"))
                .unwrap_or(0),
        },
        "trial_spawner" => BlockEntityVisualState::TrialSpawner {
            ominous: nbt.is_some_and(|value| nbt_bool(value, "ominous")),
            active: nbt.is_some_and(|value| {
                nbt_bool(value, "is_active")
                    || nbt_string(value, "state")
                        .is_some_and(|state| state.eq_ignore_ascii_case("active"))
            }),
        },
        "vault" => BlockEntityVisualState::Vault {
            ominous: nbt.is_some_and(|value| nbt_bool(value, "ominous")),
            state: nbt
                .and_then(|value| {
                    nbt_u8(value, "state").or_else(|| {
                        nbt_string(value, "state").and_then(|state| {
                            match state.to_ascii_uppercase().as_str() {
                                "INACTIVE" => Some(0),
                                "ACTIVE" => Some(1),
                                "UNLOCKING" => Some(2),
                                "EJECTING" => Some(3),
                                _ => None,
                            }
                        })
                    })
                })
                .unwrap_or(0),
        },
        _ if block_base_name.contains("piston") => BlockEntityVisualState::Piston {
            extending: action == Some(0),
            facing: parameter,
        },
        _ => BlockEntityVisualState::Generic,
    }
}

fn nbt_text_lines(value: &NbtValue, key: &str) -> [String; 4] {
    let lines = nbt_field(value, key)
        .and_then(|field| nbt_field(field, "messages"))
        .and_then(|field| match field {
            NbtValue::List(lines) => Some(lines),
            _ => None,
        });
    std::array::from_fn(|index| {
        lines
            .and_then(|items| items.get(index))
            .map(NbtValue::extract_text)
            .unwrap_or_default()
    })
}

/// Visual block-entity types in vanilla 1.21.1 registry order.
fn protocol_767_visual_block_entity(kind: i32) -> Option<&'static str> {
    Some(match kind {
        1 => "chest",
        2 => "trapped_chest",
        3 => "ender_chest",
        7 => "sign",
        8 => "hanging_sign",
        10 => "piston",
        12 => "enchanting_table",
        14 => "beacon",
        19 => "banner",
        23 => "shulker_box",
        30 => "bell",
        32 => "campfire",
        42 => "trial_spawner",
        43 => "vault",
        _ => return None,
    })
}

fn protocol_767_entity_name(entity_type: i32) -> Option<&'static str> {
    static TABLE: OnceLock<megu_proto::ProtocolTable> = OnceLock::new();
    TABLE
        .get_or_init(megu_proto::ProtocolTable::v767_builtin)
        .registry_name("minecraft:entity_type", u32::try_from(entity_type).ok()?)
}

/// Vanilla entity types with a baby variant; metadata index 16 is the baby
/// flag for exactly these types, never for the rest of the registry.
const BABY_ENTITY_TYPES: &[&str] = &[
    "minecraft:armadillo",
    "minecraft:axolotl",
    "minecraft:camel",
    "minecraft:cat",
    "minecraft:chicken",
    "minecraft:cow",
    "minecraft:donkey",
    "minecraft:fox",
    "minecraft:frog",
    "minecraft:goat",
    "minecraft:hoglin",
    "minecraft:horse",
    "minecraft:llama",
    "minecraft:mooshroom",
    "minecraft:mule",
    "minecraft:ocelot",
    "minecraft:panda",
    "minecraft:pig",
    "minecraft:polar_bear",
    "minecraft:rabbit",
    "minecraft:sheep",
    "minecraft:sniffer",
    "minecraft:strider",
    "minecraft:tadpole",
    "minecraft:trader_llama",
    "minecraft:turtle",
    "minecraft:villager",
    "minecraft:wandering_trader",
    "minecraft:wolf",
    "minecraft:zoglin",
    "minecraft:zombie",
    "minecraft:zombie_horse",
    "minecraft:zombie_villager",
];

#[derive(Clone, Debug)]
pub struct MerchantPanelState {
    pub offers: Vec<crate::protocol::MerchantOfferEntry>,
    pub villager_level: i32,
    pub villager_xp: i32,
    pub show_progress: bool,
    pub can_restock: bool,
}

/// Live world-border state assembled from the six vanilla border packets.
/// Diameters are full widths in blocks; lerp is stepped once per simulated
/// client tick, matching the server's real-time millisecond duration.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct WorldBorderState {
    pub center_x: f64,
    pub center_z: f64,
    pub current_diameter: f64,
    pub target_diameter: f64,
    /// Remaining lerp time in client ticks (`lerp_ms / 50`, rounded up).
    pub lerp_ticks_remaining: u32,
    /// Total lerp duration in client ticks; zero means static.
    pub lerp_ticks_total: u32,
    /// Absolute portal boundary (`initialize_border` only).
    pub portal_boundary: i32,
    pub warning_blocks: i32,
    pub warning_time: i32,
}

impl Default for WorldBorderState {
    fn default() -> Self {
        Self {
            center_x: 0.0,
            center_z: 0.0,
            current_diameter: 60_000_000.0,
            target_diameter: 60_000_000.0,
            lerp_ticks_remaining: 0,
            lerp_ticks_total: 0,
            portal_boundary: 29_999_984,
            warning_blocks: 5,
            warning_time: 15,
        }
    }
}

impl WorldBorderState {
    fn begin_lerp(&mut self, old_diameter: f64, new_diameter: f64, lerp_ms: i64) {
        self.current_diameter = old_diameter.max(0.0);
        self.target_diameter = new_diameter.max(0.0);
        if lerp_ms > 0 && self.current_diameter != self.target_diameter {
            let ticks =
                u32::try_from(lerp_ms.saturating_add(49).div_euclid(50)).unwrap_or(u32::MAX);
            self.lerp_ticks_total = ticks;
            self.lerp_ticks_remaining = ticks;
        } else {
            self.current_diameter = self.target_diameter;
            self.lerp_ticks_total = 0;
            self.lerp_ticks_remaining = 0;
        }
    }

    /// Advance the border diameter by one client tick.
    pub fn tick(&mut self) {
        if self.lerp_ticks_remaining == 0 {
            return;
        }
        if self.lerp_ticks_remaining <= 1 {
            self.lerp_ticks_remaining = 0;
            self.current_diameter = self.target_diameter;
            return;
        }
        let step =
            (self.target_diameter - self.current_diameter) / f64::from(self.lerp_ticks_remaining);
        self.current_diameter += step;
        self.lerp_ticks_remaining -= 1;
    }

    /// Positive horizontal distance to the nearest border wall, negative inside.
    /// Outside the square this becomes negative by the penetration depth.
    pub fn horizontal_distance(&self, x: f64, z: f64) -> f64 {
        let half = (self.current_diameter * 0.5).max(0.0);
        half - (x - self.center_x).abs().max((z - self.center_z).abs())
    }

    /// `(signed distance, warning intensity)` for the HUD edge treatment.
    pub fn warning_at(&self, x: f64, z: f64) -> (f32, f32) {
        let distance = self.horizontal_distance(x, z) as f32;
        let warning = self.warning_blocks.max(0) as f32;
        let intensity = if distance < 0.0 {
            1.0
        } else if warning > 0.0 && distance < warning {
            (1.0 - distance / warning).clamp(0.0, 1.0)
        } else {
            0.0
        };
        (distance, intensity)
    }
}

pub struct WorldState {
    pub dimension: DimensionMeta,
    pub blocks: BlockTable,
    /// Pinned Vanilla 1.21.1 block registry (luminance, opacity, fluid and
    /// occlusion facts) when `assets/block_states_1211.json` +
    /// `assets/blocks_1211.json` were available, or the `--version-dir` pack
    /// opened via `Registry::from_version_dir`; `None` in fallback mode.
    pub registry: Option<Registry>,
    /// Number of global block-state ids the active registry declares
    /// (`Registry::state_count()` or pinned `STATE_COUNT`); bounds the
    /// `PalettedSection` global palette and every incoming state id.
    pub state_count: usize,
    /// Incremental light engine fed by local block edits; only present when
    /// the registry loaded and the dimension fits the engine window.
    light_engine: Option<LightEngine>,
    /// Tick-local, last-write-wins coalescer (`occlusion_table`) for the
    /// light edits block changes queue: a `SectionBlocksUpdate` batch
    /// drains as one `apply_edits_multi_word` call per touched chunk
    /// instead of one engine call per edit.
    light_queue: CoalescedLightQueue,
    pub columns: CoordMap<(i32, i32), ChunkColumn>,
    pub mesh_cache: MeshCache,
    /// Sparse full-resolution block states that cannot survive the kernel
    /// palette's 15-bit direct encoding, keyed by section then cell index.
    /// Mod dynamic ids are u32 and start at `STATE_COUNT`; keeping the ones
    /// that exceed the kernel palette's direct encoding in this side table
    /// prevents silent truncation while `StoredSection` retains its stable
    /// public field layout for test helpers across the workspace.
    section_dynamic_states: CoordMap<SectionCoord, BTreeMap<usize, u32>>,
    /// Official vanilla block-model catalog attached for exact shaped-block
    /// terrain meshing. `None` keeps the legacy greedy fast path untouched.
    block_model_catalog: Option<Arc<VanillaBlockModelCatalog>>,
    /// Lazy state-id baker for the synchronous meshing path. World meshing is
    /// single-threaded, so the cache lives directly in `WorldState`.
    block_model_cache: Option<BakedBlockStateCache>,
    /// Floating-point official baked terrain sidecar keyed by section. Legacy
    /// packed terrain cannot represent fractional positions/UVs, so consumers
    /// use this stream for the shaped replacement geometry.
    baked_terrain: CoordMap<SectionCoord, Vec<BakedTerrainFace>>,
    /// Shaped non-cube terrain quads whose fractional coordinates/extents
    /// cannot be encoded by the integer-grid `PackedVertex` C ABI.
    unpacked_terrain: CoordMap<SectionCoord, Vec<Quad>>,
    packed_meshes: HashMap<SectionCoord, Vec<PackedVertex>>,
    packed_mesh_digests: CoordMap<SectionCoord, u64>,
    /// Monotonic per-section mesh-content revision, bumped on every mesh
    /// rebuild or translucent re-sort so `client-gpu` detects changes with
    /// one `u64` compare instead of byte-comparing `Vec<PackedVertex>`
    /// slices. Entries are removed on chunk unload/evict and survive
    /// [`WorldState::release_uploaded_section_cpu_copies`] so unchanged
    /// frames keep matching the VRAM-resident copy.
    section_mesh_revisions: CoordMap<SectionCoord, u64>,
    /// Source of [`section_mesh_revisions`] values. Starts at 1 so `0`
    /// stays a usable "never uploaded" sentinel for consumers, and never
    /// resets — a later section cannot inherit a stale predecessor's
    /// revision across dimension resets.
    next_mesh_revision: u64,
    translucent_orders: CoordMap<SectionCoord, Vec<u32>>,
    /// Per-released-section map from a kept translucent quad's position key
    /// to its index in the grouped (packed) order the GPU uploaded. Lets a
    /// camera-driven re-sort of a released section re-derive a valid
    /// `translucent_order` over the VRAM-resident mesh without the deleted
    /// full CPU copy.
    released_translucent_slots: CoordMap<SectionCoord, HashMap<TranslucentSlotKey, u32>>,
    quad_counts: CoordMap<SectionCoord, usize>,
    /// Per-section 6x6 face-to-face visibility graph (including uniform-air
    /// sections) plus the renderable flag fed to `GraphCuller`.
    visibility: HashMap<SectionCoord, SectionVisibility>,
    /// Per-section direction-bucket ranges over the grouped packed mesh.
    direction_buckets: CoordMap<SectionCoord, DirectionBuckets>,
    /// Shared 8x4x8-section arena producing batched multi-draw submissions.
    render_regions: RenderRegions,
    pub chunks_decoded: u64,
    pub chunks_unloaded: u64,
    pub light_updates: u64,
    pub block_updates: u64,
    /// Block cells relit by the local `LightEngine` after 0x09/0x49 edits.
    pub local_light_edits: u64,
    pub metrics: MeshMetrics,
    /// Skeletal animation models and live entity controllers.
    pub animations: WorldAnimationHost,
    /// Live entities tracked from S2C entity packets, bounded by
    /// [`MAX_TRACKED_ENTITIES`].
    pub entities: BTreeMap<i32, TrackedEntity>,
    pub local_player_entity_id: Option<i32>,
    pub camera_entity_id: Option<i32>,
    pub combat: CombatState,
    pub tab_header: String,
    pub tab_footer: String,
    pub maps: BTreeMap<i32, TrackedMap>,
    map_recency: VecDeque<i32>,
    pub teams: BTreeMap<String, ScoreboardTeam>,
    member_teams: BTreeMap<String, String>,
    /// Last decoded metadata values by entity and metadata index.
    pub entity_metadata: BTreeMap<i32, Vec<DecodedEntityMetadataEntry>>,
    /// Raw item-slot payloads keyed by (entity id, vanilla equipment slot).
    pub entity_equipment: BTreeMap<(i32, u8), Vec<u8>>,
    /// Current entity pose value from the shared metadata pose field.
    pub entity_poses: BTreeMap<i32, i32>,
    /// Remaining swing/animation ticks by entity id.
    pub entity_animation_ticks: BTreeMap<i32, u8>,
    /// Active potion effects keyed by (entity id, registry effect id).
    pub mob_effects: BTreeMap<(i32, i32), TrackedMobEffect>,
    /// Last server-synced attributes per tracked entity.
    pub entity_attributes: BTreeMap<i32, Vec<WireAttribute>>,
    /// Vehicle-to-passenger relationships from `set_passengers`.
    pub passengers: BTreeMap<i32, Vec<i32>>,
    /// Leash/entity attachment target by source entity id.
    pub entity_links: BTreeMap<i32, i32>,
    /// Recent block events retained for a future block-event renderer.
    pub block_events: VecDeque<([i32; 3], u8, u8, i32)>,
    /// Recent `level_event` records `(event, pos, data, global)` — the vanilla
    /// effect channel (block-break particles 2001, sounds, ...) kept
    /// bounded for the effect renderer like [`Self::block_events`].
    pub level_events: VecDeque<(i32, [i32; 3], i32, bool)>,
    /// Recent `level_particles` spawn records kept bounded the
    /// same way; the particle stage drains them when it lands.
    pub level_particles: VecDeque<LevelParticleSpawn>,
    /// Parsed particle options aligned with the arrival order in
    /// `level_particles`; unknown options preserve their original bytes.
    pub level_particle_options: VecDeque<(i32, DecodedParticleOptions)>,
    /// Recent item pickup animations retained for a future entity animation system.
    pub item_pickups: VecDeque<(i32, i32, i32)>,
    /// Block entities retained from chunk payloads and `block_entity_data`
    /// updates, keyed by world `[x, y, z]`, bounded by
    /// [`MAX_TRACKED_BLOCK_ENTITIES`].
    pub block_entities: BTreeMap<[i32; 3], TrackedBlockEntity>,
    /// Parsed anonymous NBT for block entities, including sign sides, item
    /// lists, workstation properties, and trial-spawner/vault state.
    pub block_entity_visual_nbt: BTreeMap<[i32; 3], NbtValue>,
    /// Boss-bar rows keyed by server UUID.
    pub boss_bars: BTreeMap<[u8; 16], TrackedBossBar>,
    /// Workstation/container property values keyed by (window, property).
    pub container_properties: BTreeMap<(u8, i16), i16>,
    /// Merchant UI state keyed by container window id.
    pub merchant_panels: BTreeMap<u8, MerchantPanelState>,
    /// World age in game ticks (`set_time`).
    pub world_age: i64,
    /// Time of day in game ticks (`set_time`); consumers reduce mod 24000.
    pub time_of_day: i64,
    /// Rain level `0.0..=1.0` driven by weather `game_event`s.
    pub rain_level: f32,
    /// Thunder level `0.0..=1.0` driven by weather `game_event`s.
    pub thunder_level: f32,
    /// Whether the server reports rain right now: `START_RAINING` (1) sets
    /// it, `STOP_RAINING` (2) clears it. Vanilla keeps this flag beside the
    /// level, because the level itself ramps through
    /// `RAIN_LEVEL_CHANGE` (7) instead of following the flag.
    pub is_raining: bool,
    /// Current game mode from the game-state-change event, when received.
    pub game_mode: Option<u8>,
    /// Live server world border (`initialize_border` + the five update packets).
    pub world_border: WorldBorderState,
    /// Default spawn position and compass angle from `set_default_spawn_position`.
    pub default_spawn_position: Option<([i32; 3], f32)>,
    /// `(difficulty, locked)` mirrored from `change_difficulty`.
    pub difficulty: Option<(u8, bool)>,
    /// Server tick manager state (`ticking_state`).
    pub server_tick_rate: f32,
    pub server_ticks_frozen: bool,
    /// Most recent frozen-tick step grant (`ticking_step`).
    pub server_tick_steps: i32,
    /// Player statistics keyed by `(category, statistic)` protocol ids.
    pub statistics: BTreeMap<(i32, i32), i32>,
    /// Server MOTD text decoded from the server-data component payload.
    pub server_motd: Option<String>,
    /// Server favicon bytes from `server_data`, when provided.
    pub server_icon: Option<Vec<u8>>,
    /// Whether the connected server advertises secure-chat enforcement.
    /// Protocol 767 does not carry this bit on `server_data`; it stays `None` there.
    pub server_enforces_secure_chat: Option<bool>,
    /// Chunk batch lifecycle counters used by live timing telemetry.
    pub chunk_batch_active: bool,
    pub chunk_batches_started: u64,
    pub chunk_batches_finished: u64,
    pub last_chunk_batch_size: u64,
    /// Block-break crack overlays keyed by breaker entity id, bounded by
    /// [`MAX_BLOCK_DESTRUCTIONS`].
    pub block_destructions: BTreeMap<i32, BlockBreakProgress>,
}

/// Candidate directories holding the pinned `assets/` pair, mirroring
/// `BlockTable::load`'s search order.
fn kernel_asset_dirs() -> Vec<PathBuf> {
    let mut dirs = vec![PathBuf::from("assets"), PathBuf::from("../assets")];
    if let Ok(exe) = std::env::current_exe() {
        for ancestor in exe.ancestors().skip(1).take(6) {
            dirs.push(ancestor.join("assets"));
        }
    }
    dirs
}

fn try_open_registry(states: &Path, blocks: &Path) -> Option<Registry> {
    let states_json = std::fs::read(states).ok()?;
    let blocks_json = std::fs::read(blocks).ok()?;
    Registry::open_json(&states_json, &blocks_json).ok()
}

/// Load the kernel `Registry` from the asset pair that supplied the block
/// table. Returns `None` (clean fallback) when either file is absent or the
/// pair fails validation.
fn load_kernel_registry(states_path: Option<&Path>) -> Option<Registry> {
    if let Some(states) = states_path {
        if let Some(dir) = states.parent() {
            if let Some(registry) = try_open_registry(states, &dir.join("blocks_1211.json")) {
                return Some(registry);
            }
        }
    }
    for dir in kernel_asset_dirs() {
        if let Some(registry) = try_open_registry(
            &dir.join("block_states_1211.json"),
            &dir.join("blocks_1211.json"),
        ) {
            return Some(registry);
        }
    }
    None
}

fn light_engine_for(dimension: &DimensionMeta, enabled: bool) -> Option<LightEngine> {
    if !enabled {
        return None;
    }
    let height = u32::try_from(dimension.height).ok()?;
    // Windows of <=62 payload sections keep the standard u64-mask engine;
    // taller custom dimensions (up to 128 sections, e.g. min_y=-512 with
    // height=1536) need the multi-word engine so changed-section bits past
    // 63 stay representable.
    if height / 16 <= 62 {
        LightEngine::new(dimension.min_y, height, dimension.has_skylight).ok()
    } else {
        LightEngine::new_tall(dimension.min_y, height, dimension.has_skylight).ok()
    }
}

impl WorldState {
    /// Install dynamic mod block states (`state_id`, canonical name, packed
    /// `(luminance << 4) | opacity` facts) into this world's sparse registry.
    /// Existing loaded sections containing dynamic ids are re-materialized so
    /// meshing, collision and lighting pick up the new facts immediately.
    pub fn install_dynamic_mod_registry_states(&mut self, states: &[(u32, String, u8)]) -> usize {
        let accepted = self.blocks.install_dynamic_mod_states(states);
        if accepted == 0 {
            return 0;
        }
        self.state_count = self.state_count.max(self.blocks.state_count());
        if self.light_engine.is_none() {
            self.light_engine = light_engine_for(&self.dimension, true);
        }

        let min_sy = self.dimension.min_section_y();
        let affected: Vec<SectionCoord> = self
            .section_dynamic_states
            .iter()
            .filter(|(_, overlay)| !overlay.is_empty())
            .map(|(&coord, _)| coord)
            .collect();
        for coord in affected {
            let Some(column) = self.columns.get(&(coord.0, coord.2)) else {
                continue;
            };
            let Some(section) = column.sections.get((coord.1 - min_sy) as usize) else {
                continue;
            };
            let mesh_sec =
                section.to_compressed_mesh_section(&self.blocks, self.dynamic_overlay(coord));
            self.set_section_visibility(coord, face_visibility_of(&mesh_sec.blocks), true);
            self.mesh_cache.insert_compressed(coord, mesh_sec);
        }
        let chunks: Vec<(i32, i32)> = self.columns.keys().copied().collect();
        for (cx, cz) in chunks {
            self.load_light_chunk(cx, cz);
        }
        accepted
    }

    /// Apply live biome-registry climate updates and invalidate every loaded
    /// section whose biome cells reference one of the updated biome ids.
    /// Future chunk inserts use the updated resolver automatically.
    pub fn update_biome_climates_from_registry(&mut self, entries: &[BiomeClimateEntry]) -> usize {
        let accepted = self.blocks.update_biome_climates_from_registry(entries);
        if accepted == 0 {
            return 0;
        }
        let ids: BTreeSet<u32> = entries
            .iter()
            .filter(|entry| entry.temperature.is_finite() && entry.downfall.is_finite())
            .map(|entry| entry.biome_id)
            .collect();
        let min_sy = self.dimension.min_section_y();
        let mut affected = Vec::new();
        for (&(cx, cz), column) in &self.columns {
            for (index, section) in column.sections.iter().enumerate() {
                if section.biomes.iter().any(|biome| ids.contains(biome)) {
                    affected.push(SectionCoord(cx, min_sy + index as i32, cz));
                }
            }
        }
        for coord in affected {
            let Some(column) = self.columns.get(&(coord.0, coord.2)) else {
                continue;
            };
            let Some(section) = column.sections.get((coord.1 - min_sy) as usize) else {
                continue;
            };
            if section.has_non_air {
                let mesh_sec =
                    section.to_compressed_mesh_section(&self.blocks, self.dynamic_overlay(coord));
                self.mesh_cache.insert_compressed(coord, mesh_sec);
                self.mesh_cache.mark_dirty(coord);
            }
        }
        accepted
    }

    /// Lighting facts for a state in either the vanilla kernel registry or
    /// the sparse dynamic mod-state registry.
    fn light_fact_for_state(&self, state_id: u32) -> Option<LightFact> {
        if let Some(fact) = self.blocks.dynamic_light_fact(state_id) {
            return Some(fact);
        }
        self.registry
            .as_ref()
            .and_then(|registry| registry.facts(state_id).ok())
            .map(|fact| LightFact {
                opacity: fact.opacity,
                emission: fact.luminance,
                occlusion: fact.face_occlusion,
            })
    }

    pub fn new(dimension: DimensionMeta, blocks: BlockTable) -> Self {
        let registry = load_kernel_registry(blocks.source_path.as_deref());
        Self::with_registry(dimension, blocks, registry)
    }

    /// Construct a `WorldState` backed only by `blocks` without probing ambient
    /// vanilla registry files on disk (for synthetic block-table tests).
    pub fn new_without_registry(dimension: DimensionMeta, blocks: BlockTable) -> Self {
        Self::with_registry(dimension, blocks, None)
    }

    /// `new` with the kernel `Registry` opened from a `--version-dir` pack
    /// (`blocks.json` + `block_state_facts.bin`, optional
    /// `block_states.json`); the pack's `state_count()` bounds every stored
    /// section's global palette.
    pub fn new_from_version_dir(
        dimension: DimensionMeta,
        blocks: BlockTable,
        dir: &std::path::Path,
    ) -> Result<Self, String> {
        let registry = Registry::from_version_dir(dir)
            .map_err(|e| format!("cannot open version pack {}: {e:?}", dir.display()))?;
        Ok(Self::with_registry(dimension, blocks, Some(registry)))
    }

    fn with_registry(
        dimension: DimensionMeta,
        blocks: BlockTable,
        registry: Option<Registry>,
    ) -> Self {
        let state_count = registry
            .as_ref()
            .map_or(STATE_COUNT as usize, Registry::state_count);
        let light_engine = light_engine_for(&dimension, registry.is_some());
        Self {
            dimension,
            blocks,
            registry,
            state_count,
            light_engine,
            light_queue: CoalescedLightQueue::new(),
            columns: CoordMap::default(),
            mesh_cache: MeshCache::new(MissingNeighbor::Air),
            section_dynamic_states: CoordMap::default(),
            block_model_catalog: None,
            block_model_cache: None,
            baked_terrain: CoordMap::default(),
            unpacked_terrain: CoordMap::default(),
            packed_meshes: HashMap::new(),
            packed_mesh_digests: CoordMap::default(),
            section_mesh_revisions: CoordMap::default(),
            next_mesh_revision: 1,
            translucent_orders: CoordMap::default(),
            released_translucent_slots: CoordMap::default(),
            quad_counts: CoordMap::default(),
            visibility: HashMap::new(),
            direction_buckets: CoordMap::default(),
            render_regions: RenderRegions::default(),
            chunks_decoded: 0,
            chunks_unloaded: 0,
            light_updates: 0,
            block_updates: 0,
            local_light_edits: 0,
            metrics: MeshMetrics::default(),
            animations: WorldAnimationHost::new(),
            entities: BTreeMap::new(),
            local_player_entity_id: None,
            camera_entity_id: None,
            combat: CombatState::default(),
            tab_header: String::new(),
            tab_footer: String::new(),
            maps: BTreeMap::new(),
            map_recency: VecDeque::new(),
            teams: BTreeMap::new(),
            member_teams: BTreeMap::new(),
            entity_metadata: BTreeMap::new(),
            entity_equipment: BTreeMap::new(),
            entity_poses: BTreeMap::new(),
            entity_animation_ticks: BTreeMap::new(),
            mob_effects: BTreeMap::new(),
            entity_attributes: BTreeMap::new(),
            passengers: BTreeMap::new(),
            entity_links: BTreeMap::new(),
            block_events: VecDeque::new(),
            level_events: VecDeque::new(),
            level_particles: VecDeque::new(),
            level_particle_options: VecDeque::new(),
            item_pickups: VecDeque::new(),
            block_entities: BTreeMap::new(),
            block_entity_visual_nbt: BTreeMap::new(),
            boss_bars: BTreeMap::new(),
            container_properties: BTreeMap::new(),
            merchant_panels: BTreeMap::new(),
            world_age: 0,
            time_of_day: 0,
            rain_level: 0.0,
            thunder_level: 0.0,
            is_raining: false,
            game_mode: None,
            world_border: WorldBorderState::default(),
            default_spawn_position: None,
            difficulty: None,
            server_tick_rate: 20.0,
            server_ticks_frozen: false,
            server_tick_steps: 0,
            statistics: BTreeMap::new(),
            server_motd: None,
            server_icon: None,
            server_enforces_secure_chat: None,
            chunk_batch_active: false,
            chunk_batches_started: 0,
            chunk_batches_finished: 0,
            last_chunk_batch_size: 0,
            block_destructions: BTreeMap::new(),
        }
    }

    /// Attach the official vanilla baked block-model catalog and the block
    /// atlas texture lookup. State names come from this world's `BlockTable`;
    /// `sprite_lookup` certifies which full-cube baked materials may remain on
    /// the existing greedy terrain path. Attaching a catalog invalidates all
    /// currently loaded section meshes so shaped blocks rebuild through the
    /// official JSON geometry instead of mixing old and new representations.
    pub fn set_block_model_catalog(
        &mut self,
        catalog: Arc<VanillaBlockModelCatalog>,
        sprite_lookup: BlockAtlasSpriteLookup,
    ) {
        let lookup = Arc::clone(&sprite_lookup);
        self.block_model_cache = Some(BakedBlockStateCache::new_with_greedy_material_check(
            (*catalog).clone(),
            self.blocks.state_names().to_vec(),
            move |_state_id, quad| lookup(quad.texture.as_str()).is_some(),
        ));
        self.block_model_catalog = Some(catalog);
        self.baked_terrain.clear();
        self.mesh_cache.mark_all_sections_dirty();
    }

    pub fn has_block_model_catalog(&self) -> bool {
        self.block_model_catalog.is_some()
    }

    pub fn block_model_catalog(&self) -> Option<&Arc<VanillaBlockModelCatalog>> {
        self.block_model_catalog.as_ref()
    }

    /// Official baked terrain faces emitted for one synchronously rebuilt
    /// section. Positions are section-local and preserve fractional model
    /// coordinates; `None` means the section is absent, not yet rebuilt, or
    /// had no non-full-cube baked replacement geometry.
    pub fn baked_terrain_faces(&self, coord: SectionCoord) -> Option<&[BakedTerrainFace]> {
        self.baked_terrain.get(&coord).map(Vec::as_slice)
    }

    /// Non-cube shaped terrain quads whose fractional coordinates cannot be
    /// packed into the integer-grid `PackedVertex` C ABI.
    pub fn unpacked_terrain_quads(&self, coord: SectionCoord) -> Option<&[Quad]> {
        self.unpacked_terrain.get(&coord).map(Vec::as_slice)
    }

    pub fn reset_dimension(&mut self, dimension: DimensionMeta) {
        self.dimension = dimension;
        self.animations.clear_entities();
        // Dimension changes invalidate live entity, block-entity and
        // block-break state; the world clock (age/time/weather) persists
        // across respawns exactly like vanilla's shared `ClientLevel` data.
        self.entities.clear();
        self.camera_entity_id = None;
        self.combat = CombatState::default();
        self.entity_metadata.clear();
        self.entity_equipment.clear();
        self.entity_poses.clear();
        self.entity_animation_ticks.clear();
        self.mob_effects.clear();
        self.entity_attributes.clear();
        self.passengers.clear();
        self.entity_links.clear();
        self.block_events.clear();
        self.level_events.clear();
        self.level_particles.clear();
        self.level_particle_options.clear();
        self.item_pickups.clear();
        self.block_entities.clear();
        self.block_entity_visual_nbt.clear();
        self.merchant_panels.clear();
        self.container_properties.clear();
        self.block_destructions.clear();
        // Border state is re-published for the destination dimension by the
        // server; never let an old wall linger while chunks are empty.
        self.world_border = WorldBorderState::default();
        self.chunk_batch_active = false;
        self.light_engine = light_engine_for(&self.dimension, self.registry.is_some());
        // A dimension change discards any edits queued against the old
        // world — the fresh engine has no columns for them to apply to.
        self.light_queue = CoalescedLightQueue::new();
        self.columns.clear();
        self.section_dynamic_states.clear();
        self.mesh_cache = MeshCache::new(MissingNeighbor::Air);
        self.baked_terrain.clear();
        self.unpacked_terrain.clear();
        self.packed_meshes.clear();
        self.packed_mesh_digests.clear();
        self.section_mesh_revisions.clear();
        self.translucent_orders.clear();
        self.released_translucent_slots.clear();
        self.quad_counts.clear();
        self.visibility.clear();
        self.direction_buckets.clear();
        self.render_regions = RenderRegions::default();
        self.recompute_live_totals();
    }

    pub fn ingest_anm1_payload(&mut self, bytes: &[u8]) -> Result<(), AnimationError> {
        self.animations.ingest_anm1_payload(bytes)
    }

    pub fn advance_animations(&mut self, dt_sec: f32) -> Result<(), AnimationError> {
        self.animations.advance(dt_sec)
    }

    pub fn evaluate_entity_pose(
        &self,
        entity_id: i32,
    ) -> Result<Option<Vec<[f32; 16]>>, AnimationError> {
        self.animations.evaluate_entity_pose(entity_id)
    }

    pub fn despawn_entity(&mut self, entity_id: i32) -> bool {
        self.animations.despawn_entity(entity_id)
    }

    pub fn loaded_chunks(&self) -> usize {
        self.columns.len()
    }

    pub fn pending_mesh_sections(&self) -> usize {
        self.mesh_cache.pending()
    }

    pub fn get_block_state(&self, x: i32, y: i32, z: i32) -> Option<u32> {
        // `min_y`/`height` come from server-sent dimension_type NBT and may
        // be any i32; widen before adding so hostile values cannot overflow.
        let min_y = i64::from(self.dimension.min_y);
        let max_y = min_y + i64::from(self.dimension.height);
        let y = i64::from(y);
        if y < min_y || y >= max_y {
            return Some(0);
        }
        let cx = x.div_euclid(16);
        let cz = z.div_euclid(16);
        let col = self.columns.get(&(cx, cz))?;
        let sec_idx = ((y - min_y) / 16) as usize;
        let sec = col.sections.get(sec_idx)?;
        let idx = section_index(
            x.rem_euclid(16) as usize,
            (y - min_y).rem_euclid(16) as usize,
            z.rem_euclid(16) as usize,
        );
        let coord = SectionCoord(cx, self.dimension.min_section_y() + sec_idx as i32, cz);
        Some(sec.state_at(idx, self.dynamic_overlay(coord)))
    }

    pub fn apply_chunk(&mut self, decoded: DecodedChunk) -> Result<(), String> {
        self.apply_chunk_borrowed(&decoded)
    }

    /// Shared chunk-ingest path behind [`Self::apply_chunk`] and the
    /// `ChunkData` arm of [`Self::apply_play_event`]; borrows the decoded
    /// column so event routing does not clone section payloads.
    fn apply_chunk_borrowed(&mut self, decoded: &DecodedChunk) -> Result<(), String> {
        let section_count = decoded.sections.len();
        let default_sky = if self.dimension.has_skylight {
            [0xffu8; LIGHT_BYTES]
        } else {
            [0u8; LIGHT_BYTES]
        };

        let min_sy = self.dimension.min_section_y();
        let mut stored_sections = Vec::with_capacity(section_count);
        let mut sky_cursor = usize::from(bitset_get(&decoded.light.sky_mask, 0));
        let mut block_cursor = usize::from(bitset_get(&decoded.light.block_mask, 0));
        for (idx, sec) in decoded.sections.iter().enumerate() {
            let (sky_update, block_update) =
                section_light_updates(&decoded.light, idx + 1, &mut sky_cursor, &mut block_cursor);
            let sky = sky_update.unwrap_or(default_sky);
            let blk = block_update.unwrap_or([0u8; LIGHT_BYTES]);
            let mut dynamic = BTreeMap::new();
            let mut stored = StoredSection::from_states(
                &sec.states,
                blk,
                sky,
                &self.blocks,
                self.state_count,
                &mut dynamic,
            )?;
            stored.biomes = sec.biomes;
            let coord = SectionCoord(decoded.cx, min_sy + idx as i32, decoded.cz);
            // Unchanged chunk section: a re-sent column whose cells, light
            // and biomes all match leaves the resident mesh valid, so skip
            // the remesh (and its revision bump) instead of rebuilding
            // byte-identical geometry.
            let unchanged = self
                .columns
                .get(&(decoded.cx, decoded.cz))
                .and_then(|col| col.sections.get(idx))
                .is_some_and(|old| {
                    old.same_content(&stored, self.dynamic_overlay(coord), &dynamic)
                });
            // A re-decoded chunk replaces the column: drop any overlay cells
            // the previous decode installed before meshing the fresh one.
            self.section_dynamic_states.remove(&coord);
            if !unchanged {
                if stored.has_non_air {
                    let mesh_sec = stored.to_compressed_mesh_section(&self.blocks, &dynamic);
                    self.set_section_visibility(coord, face_visibility_of(&mesh_sec.blocks), true);
                    self.mesh_cache.insert_compressed(coord, mesh_sec);
                } else {
                    self.remove_section_mesh(coord);
                    self.set_section_visibility(coord, FaceVisibility::uniform_air(), false);
                }
            }
            if !dynamic.is_empty() {
                self.section_dynamic_states.insert(coord, dynamic);
            }
            stored_sections.push(stored);
        }

        self.columns.insert(
            (decoded.cx, decoded.cz),
            ChunkColumn {
                cx: decoded.cx,
                cz: decoded.cz,
                sections: stored_sections,
            },
        );
        self.refresh_chunk_block_entities(decoded.cx, decoded.cz, &decoded.block_entities);
        self.chunks_decoded += 1;
        self.load_light_chunk(decoded.cx, decoded.cz);
        Ok(())
    }

    /// Feed one decoded column into the local `LightEngine` as `LightFact`
    /// cells ordered y, z, x from the window bottom. Skipped silently when the
    /// engine or registry is absent or the column does not cover the window.
    fn load_light_chunk(&mut self, cx: i32, cz: i32) {
        let registry = self.registry.as_ref();
        if registry.is_none() && self.blocks.dynamic_state_limit == 0 {
            return;
        }
        let (Some(engine), Some(col)) = (self.light_engine.as_mut(), self.columns.get(&(cx, cz)))
        else {
            return;
        };
        let cells_per_chunk = engine.cells_per_chunk();
        let height_blocks = cells_per_chunk / 256;
        if col.sections.len() * SECTION_CELLS != cells_per_chunk {
            return;
        }
        let mut cells = Vec::with_capacity(cells_per_chunk);
        let min_sy = self.dimension.min_section_y();
        for y_off in 0..height_blocks {
            let sy = y_off / 16;
            let ly = y_off % 16;
            let sec = &col.sections[sy];
            let coord = SectionCoord(cx, min_sy + sy as i32, cz);
            for lz in 0..16usize {
                for lx in 0..16usize {
                    let state = sec.state_at(
                        section_index(lx, ly, lz),
                        self.section_dynamic_states
                            .get(&coord)
                            .unwrap_or(&EMPTY_DYNAMIC_STATES),
                    );
                    let fact = self
                        .blocks
                        .dynamic_light_fact(state)
                        .or_else(|| {
                            registry
                                .and_then(|registry| registry.facts(state).ok())
                                .map(|fact| LightFact {
                                    opacity: fact.opacity,
                                    emission: fact.luminance,
                                    occlusion: fact.face_occlusion,
                                })
                        })
                        .unwrap_or_default();
                    cells.push(fact);
                }
            }
        }
        let _ = engine.load_chunk(cx, cz, &cells);
    }

    /// Enqueue one local block edit's light change into the tick-local
    /// [`CoalescedLightQueue`]. Unknown states are dropped here exactly as
    /// the former per-edit path dropped them; the queue drains through
    /// [`Self::flush_light_queue`] into the same engine + write-back path.
    fn queue_local_light_edit(&mut self, x: i32, y: i32, z: i32, state_id: u32) {
        if self.light_fact_for_state(state_id).is_some() {
            self.light_queue.push(QueuedLightEdit { x, y, z, state_id });
        }
    }

    /// Drain the coalesced queue into the incremental light engine and push
    /// the resulting 2048-byte sky/block arrays into the stored sections +
    /// mesh cache — one `apply_edits_multi_word` call per touched chunk via
    /// `ChunkLightBatch::to_light_edits` rather than one call per edit.
    /// No-op when the engine or registry is unavailable.
    fn flush_light_queue(&mut self) {
        // `drain` runs even without an engine so stranded edits cannot
        // accumulate across packets.
        let batches = self.light_queue.drain();
        if batches.is_empty() || self.light_engine.is_none() {
            return;
        }
        for batch in &batches {
            let edits: Vec<LightEdit> = batch
                .edits
                .iter()
                .filter_map(|edit| {
                    self.light_fact_for_state(edit.state_id)
                        .map(|fact| LightEdit {
                            x: edit.x,
                            y: edit.y,
                            z: edit.z,
                            fact,
                        })
                })
                .collect();
            if edits.is_empty() {
                continue;
            }
            let update = {
                let engine = self.light_engine.as_mut().expect("checked above");
                match engine.apply_edits_multi_word(&edits) {
                    Ok(update) => update,
                    Err(_) => continue,
                }
            };
            self.local_light_edits += edits.len() as u64;
            self.apply_local_light_update(update);
        }
    }

    /// Write engine-produced light sections back into `StoredSection`s and
    /// reinsert changed sections into the `MeshCache`. The masks are
    /// `BitSet.toLongArray`-shaped word lists: bit `i` lives at
    /// `words[i / 64] >> (i % 64)`, so tall-dimension sections past bit 63
    /// are covered by iterating every word.
    fn apply_local_light_update(&mut self, update: LightUpdateMultiWord) {
        let min_sy = self.dimension.min_section_y();
        for chunk_update in update.chunks {
            let (cx, cz) = chunk_update.chunk;
            // Merge sky/block payloads and cleared (all-zero) sections per
            // local section index. Light-section index `i` maps to stored
            // section `i - 1` (index 0 is the guard section below min_y).
            type SectionLightChange = (usize, Option<[u8; LIGHT_BYTES]>, Option<[u8; LIGHT_BYTES]>);
            let mut changes: Vec<SectionLightChange> = Vec::new();
            let mut put =
                |sy: usize, sky: Option<[u8; LIGHT_BYTES]>, block: Option<[u8; LIGHT_BYTES]>| {
                    if let Some(entry) = changes.iter_mut().find(|entry| entry.0 == sy) {
                        if sky.is_some() {
                            entry.1 = sky;
                        }
                        if block.is_some() {
                            entry.2 = block;
                        }
                    } else {
                        changes.push((sy, sky, block));
                    }
                };
            for (light_idx, array) in chunk_update.sky_sections {
                if light_idx >= 1 {
                    put(light_idx - 1, Some(array), None);
                }
            }
            for (light_idx, array) in chunk_update.block_sections {
                if light_idx >= 1 {
                    put(light_idx - 1, None, Some(array));
                }
            }
            for (word_idx, word) in chunk_update.empty_sky_mask.iter().enumerate() {
                let mut bits = *word;
                while bits != 0 {
                    let bit = word_idx * 64 + bits.trailing_zeros() as usize;
                    bits &= bits - 1;
                    if bit >= 1 {
                        put(bit - 1, Some([0u8; LIGHT_BYTES]), None);
                    }
                }
            }
            for (word_idx, word) in chunk_update.empty_block_mask.iter().enumerate() {
                let mut bits = *word;
                while bits != 0 {
                    let bit = word_idx * 64 + bits.trailing_zeros() as usize;
                    bits &= bits - 1;
                    if bit >= 1 {
                        put(bit - 1, None, Some([0u8; LIGHT_BYTES]));
                    }
                }
            }

            let mut dirty = Vec::new();
            {
                let Some(col) = self.columns.get_mut(&(cx, cz)) else {
                    continue;
                };
                for (sy, sky, block) in changes {
                    let Some(sec) = col.sections.get_mut(sy) else {
                        continue;
                    };
                    let mut changed = false;
                    if let Some(array) = sky {
                        if sec.sky_light != array {
                            sec.sky_light = array;
                            changed = true;
                        }
                    }
                    if let Some(array) = block {
                        if sec.block_light != array {
                            sec.block_light = array;
                            changed = true;
                        }
                    }
                    if changed && sec.has_non_air {
                        dirty.push(sy);
                    }
                }
            }
            for sy in dirty {
                let coord = SectionCoord(cx, min_sy + sy as i32, cz);
                let mesh_sec = match self.columns.get(&(cx, cz)) {
                    Some(col) => match col.sections.get(sy) {
                        Some(sec) => sec
                            .to_compressed_mesh_section(&self.blocks, self.dynamic_overlay(coord)),
                        None => continue,
                    },
                    None => continue,
                };
                self.mesh_cache.insert_compressed(coord, mesh_sec);
            }
        }
    }

    pub fn apply_light_update(
        &mut self,
        cx: i32,
        cz: i32,
        light: &DecodedLightData,
    ) -> Result<(), String> {
        self.light_updates += 1;
        let Some(col) = self.columns.get_mut(&(cx, cz)) else {
            return Ok(());
        };
        let min_sy = self.dimension.min_section_y();
        let mut sky_cursor = usize::from(bitset_get(&light.sky_mask, 0));
        let mut block_cursor = usize::from(bitset_get(&light.block_mask, 0));

        for (idx, sec) in col.sections.iter_mut().enumerate() {
            let mut changed = false;
            let (sky_update, block_update) =
                section_light_updates(light, idx + 1, &mut sky_cursor, &mut block_cursor);
            if let Some(new_sky) = sky_update {
                if sec.sky_light != new_sky {
                    sec.sky_light = new_sky;
                    changed = true;
                }
            }
            if let Some(new_blk) = block_update {
                if sec.block_light != new_blk {
                    sec.block_light = new_blk;
                    changed = true;
                }
            }
            if changed && sec.has_non_air {
                let coord = SectionCoord(cx, min_sy + idx as i32, cz);
                let dynamic = self
                    .section_dynamic_states
                    .get(&coord)
                    .unwrap_or(&EMPTY_DYNAMIC_STATES);
                let mesh_sec = sec.to_compressed_mesh_section(&self.blocks, dynamic);
                self.mesh_cache.insert_compressed(coord, mesh_sec);
            }
        }
        Ok(())
    }

    /// Batched form of [`Self::apply_block_update`] for
    /// `SectionBlocksUpdate` packets: every edit lands in the coalesced
    /// light queue last-write-wins, then drains once — one engine call
    /// per touched chunk instead of one call per edit.
    fn apply_block_updates(&mut self, updates: &[([i32; 3], u32)]) -> Result<(), String> {
        // Idle no-op: an empty batch has nothing to queue and nothing to
        // drain, so it must not reach the light engine.
        if updates.is_empty() && self.light_queue.is_empty() {
            return Ok(());
        }
        let result = updates
            .iter()
            .try_for_each(|&(pos, state_id)| self.apply_block_update_inner(pos, state_id));
        self.flush_light_queue();
        result
    }

    pub fn apply_block_update(&mut self, pos: [i32; 3], state_id: u32) -> Result<(), String> {
        let result = self.apply_block_update_inner(pos, state_id);
        self.flush_light_queue();
        result
    }

    fn apply_block_update_inner(&mut self, pos: [i32; 3], state_id: u32) -> Result<(), String> {
        self.block_updates += 1;
        let [x, y, z] = pos;
        // Widen before adding: `min_y`/`height` are server-controlled i32s.
        let min_y = i64::from(self.dimension.min_y);
        let max_y = min_y + i64::from(self.dimension.height);
        let y64 = i64::from(y);
        if y64 < min_y || y64 >= max_y {
            return Ok(());
        }
        let cx = x.div_euclid(16);
        let cz = z.div_euclid(16);
        let sec_idx = ((y64 - min_y) / 16) as usize;
        let coord = SectionCoord(cx, self.dimension.min_section_y() + sec_idx as i32, cz);
        let lx = x.rem_euclid(16) as usize;
        let ly = (y64 - min_y).rem_euclid(16) as usize;
        let lz = z.rem_euclid(16) as usize;
        let cell_idx = section_index(lx, ly, lz);
        let clamped = if state_id >= 0x8000 {
            state_id
        } else {
            state_id.min(u32::try_from(self.state_count.saturating_sub(1)).unwrap_or(u32::MAX))
        };
        let Some(previous) = self
            .columns
            .get(&(cx, cz))
            .and_then(|col| col.sections.get(sec_idx))
            .map(|sec| sec.state_at(cell_idx, self.dynamic_overlay(coord)))
        else {
            return Ok(());
        };
        // Idle no-op: receiving the state the cell already holds mutates
        // nothing — no palette write, no block-entity drop, no mesh or light
        // work, and therefore no revision bump to advertise.
        if previous == clamped {
            return Ok(());
        }
        let dynamic = self.section_dynamic_states.entry(coord).or_default();
        let col = self
            .columns
            .get_mut(&(cx, cz))
            .expect("column presence checked above");
        let sec = col
            .sections
            .get_mut(sec_idx)
            .expect("section presence checked above");
        sec.set_state(cell_idx, clamped, dynamic)?;
        // A same-base swap (a `facing` flip, a fluid level) keeps the block
        // entity; any other block change invalidates its cached NBT.
        if !self.blocks.state_name(previous).is_some_and(|name| {
                self.blocks.state_name(clamped).is_some_and(|current| {
                    block_state_base_name(name) == block_state_base_name(current)
                })
        }) {
            self.block_entities.remove(&pos);
            self.block_entity_visual_nbt.remove(&pos);
        }

        let mesh_block = self
            .blocks
            .mesh_block_for_biome(clamped, sec.biome_at(lx, ly, lz));
        let light = Light::new(
            read_nibble(&sec.block_light, cell_idx),
            read_nibble(&sec.sky_light, cell_idx),
        );

        if !self.blocks.is_air(clamped) {
            if !sec.has_non_air {
                sec.has_non_air = true;
                let mesh_sec = sec.to_compressed_mesh_section(&self.blocks, dynamic);
                self.set_section_visibility(coord, face_visibility_of(&mesh_sec.blocks), true);
                self.mesh_cache.insert_compressed(coord, mesh_sec);
            } else if self
                .mesh_cache
                .set_cell(coord, [lx, ly, lz], mesh_block, light)
            {
                let mesh_sec = sec.to_compressed_mesh_section(&self.blocks, dynamic);
                self.set_section_visibility(coord, face_visibility_of(&mesh_sec.blocks), true);
            }
        } else if sec.has_non_air
            && self
                .mesh_cache
                .set_cell(coord, [lx, ly, lz], mesh_block, light)
        {
            let mesh_sec = sec.to_compressed_mesh_section(&self.blocks, dynamic);
            self.set_section_visibility(coord, face_visibility_of(&mesh_sec.blocks), true);
        }
        self.queue_local_light_edit(x, y, z, clamped);
        Ok(())
    }

    pub fn unload_chunk(&mut self, cx: i32, cz: i32) {
        if let Some(col) = self.columns.remove(&(cx, cz)) {
            let min_sy = self.dimension.min_section_y();
            for idx in 0..col.sections.len() {
                self.remove_section_mesh(SectionCoord(cx, min_sy + idx as i32, cz));
            }
            self.chunks_unloaded += 1;
            self.recompute_live_totals();
        }
        self.forget_chunk(cx, cz);
    }

    /// Distance governor: unload every column whose Chebyshev distance from
    /// `(center_cx, center_cz)` exceeds `view_distance_chunks + 2` — the
    /// same two-chunk hysteresis margin vanilla keeps past the render
    /// border so chunks do not thrash while the player hovers near an
    /// edge. Eviction is RAM-only: the column, its section meshes,
    /// visibility/render-region artifacts and tracked block entities are
    /// dropped via [`WorldState::unload_chunk`] with zero disk writes, and
    /// `chunks_unloaded` accounts for each one. Live entities stay tracked
    /// — like vanilla, their lifecycle is server-driven (`remove_entities`),
    /// not chunk-driven. Returns how many columns were evicted; intended
    /// to run on a slow cadence (about once a second), not per frame.
    pub fn evict_far_chunks(
        &mut self,
        center_cx: i32,
        center_cz: i32,
        view_distance_chunks: i32,
    ) -> usize {
        let keep_radius = i64::from(view_distance_chunks).saturating_add(2).max(0);
        let victims: Vec<(i32, i32)> = self
            .columns
            .keys()
            .copied()
            .filter(|&(cx, cz)| {
                (i64::from(cx) - i64::from(center_cx))
                    .abs()
                    .max((i64::from(cz) - i64::from(center_cz)).abs())
                    > keep_radius
            })
            .collect();
        let evicted = victims.len();
        for (cx, cz) in victims {
            self.unload_chunk(cx, cz);
        }
        evicted
    }

    /// Drop every tracked block entity inside chunk column `(cx, cz)`. Runs on
    /// every chunk unload — even when the column itself was absent, since
    /// `block_entity_data` can anchor records to not-yet-loaded chunks — and
    /// internally before a chunk payload replaces its block-entity set, so
    /// block entities never outlive their column.
    pub fn forget_chunk(&mut self, cx: i32, cz: i32) {
        self.block_entities
            .retain(|pos, _| pos[0].div_euclid(16) != cx || pos[2].div_euclid(16) != cz);
        self.block_entity_visual_nbt
            .retain(|pos, _| pos[0].div_euclid(16) != cx || pos[2].div_euclid(16) != cz);
    }

    /// Replace the tracked block-entity set of column `(cx, cz)` with the
    /// records carried by a freshly applied chunk payload. The wire format
    /// packs chunk-local X/Z into one byte (`x = packed >> 4`,
    /// `z = packed & 0x0f`) and carries absolute world Y as `i16`.
    fn refresh_chunk_block_entities(&mut self, cx: i32, cz: i32, entities: &[DecodedBlockEntity]) {
        self.forget_chunk(cx, cz);
        for be in entities {
            if self.block_entities.len() >= MAX_TRACKED_BLOCK_ENTITIES {
                break;
            }
            // Saturating: `cx`/`cz` are server-controlled i32s, so `cx * 16`
            // can overflow for absurd coordinates; saturation keeps the
            // position deterministic instead of panicking in debug builds.
            let x = cx
                .saturating_mul(16)
                .saturating_add(i32::from(be.packed_xz >> 4));
            let z = cz
                .saturating_mul(16)
                .saturating_add(i32::from(be.packed_xz & 0x0f));
            let pos = [x, i32::from(be.y), z];
            self.apply_block_entity_data(pos, be.kind, &be.nbt_bytes);
        }
    }

    /// Sparse dynamic-state overlay for one section, or the shared empty
    /// map when the section holds only palette-representable states.
    fn dynamic_overlay(&self, coord: SectionCoord) -> &BTreeMap<usize, u32> {
        self.section_dynamic_states
            .get(&coord)
            .unwrap_or(&EMPTY_DYNAMIC_STATES)
    }

    fn remove_section_mesh(&mut self, coord: SectionCoord) {
        self.mesh_cache.remove(coord);
        self.section_dynamic_states.remove(&coord);
        self.baked_terrain.remove(&coord);
        self.unpacked_terrain.remove(&coord);
        self.packed_meshes.remove(&coord);
        self.packed_mesh_digests.remove(&coord);
        self.section_mesh_revisions.remove(&coord);
        self.translucent_orders.remove(&coord);
        self.released_translucent_slots.remove(&coord);
        self.quad_counts.remove(&coord);
        self.direction_buckets.remove(&coord);
        self.visibility.remove(&coord);
        self.render_regions.remove(coord);
    }

    /// Record the visibility-graph node for one loaded section.
    /// `renderable` mirrors `StoredSection::has_non_air`: the section can emit
    /// geometry toward `GraphCuller`'s `visible` set.
    fn set_section_visibility(
        &mut self,
        coord: SectionCoord,
        faces: FaceVisibility,
        renderable: bool,
    ) {
        self.visibility
            .insert(coord, SectionVisibility { faces, renderable });
    }

    /// Run a bounded greedy-meshing pass on dirty sections, then pack vertices
    /// (16 bytes/vertex, 4 vertices/quad) and sort translucent quads back-to-front.
    /// Ordering falls back to pure camera distance; see
    /// [`rebuild_dirty_bounded_ordered`](Self::rebuild_dirty_bounded_ordered).
    pub fn rebuild_dirty_bounded(
        &mut self,
        limit: usize,
        camera: [f32; 3],
    ) -> Result<usize, String> {
        self.rebuild_dirty_bounded_ordered(limit, camera, &[])
    }

    /// Same as [`rebuild_dirty_bounded`](Self::rebuild_dirty_bounded) but the
    /// dirty queue is drained in `MeshCache::rebuild_dirty_ordered` order:
    /// sections inside the camera frustum first, then nearest to farthest.
    /// `planes` are the six view planes in `box_in_frustum` form
    /// (`dot(n, p) + d >= 0` inside); an empty slice degrades to pure distance
    /// ordering. Afterwards a threshold-gated `MeshCache::resort_translucent`
    /// re-derives renderer artifacts only for sections whose translucent quad
    /// order actually changed — no full re-sort while the camera stays inside
    /// `MeshCache::resort_threshold` of the last sort origin.
    pub fn rebuild_dirty_bounded_ordered(
        &mut self,
        limit: usize,
        camera: [f32; 3],
        planes: &[[f32; 4]],
    ) -> Result<usize, String> {
        let mut refresh: BTreeSet<SectionCoord> = BTreeSet::new();
        let mut rebuilt_count = 0usize;
        if limit != 0 && self.mesh_cache.pending() != 0 {
            let t_mesh = Instant::now();
            let rebuilt = if let Some(cache) = self.block_model_cache.as_mut() {
                let (rebuilt, baked) = self
                    .mesh_cache
                    .rebuild_dirty_ordered_with_baked_models(limit, camera, planes, cache);
                for coord in &rebuilt {
                    self.baked_terrain.remove(coord);
                }
                for (coord, faces) in baked {
                    self.baked_terrain.insert(coord, faces);
                }
                rebuilt
            } else {
                self.mesh_cache.rebuild_dirty_ordered(limit, camera, planes)
            };
            let mesh_us = t_mesh.elapsed().as_micros() as u64;
            self.metrics.total_mesh_us += mesh_us;
            rebuilt_count = rebuilt.len();
            self.metrics.sections_rebuilt += rebuilt_count as u64;
            refresh.extend(rebuilt);
        }

        // Incremental translucent re-sort, gated on camera displacement.
        // Under the resort threshold this returns an empty list and costs only
        // the distance check; dirty sections are skipped inside the cache.
        let t_sort = Instant::now();
        let resorted = self.mesh_cache.resort_translucent(camera);
        self.metrics.total_sort_us += t_sort.elapsed().as_micros() as u64;
        refresh.extend(resorted);

        for coord in refresh {
            self.derive_section_artifacts(coord, camera)?;
        }

        self.recompute_live_totals();
        Ok(rebuilt_count)
    }

    /// Refresh every renderer-facing artifact for one clean meshed section:
    /// direction-bucket ranges over the grouped quad order, the packed
    /// 16-byte vertices, the shared-arena render-region upload and the
    /// translucent sort order. Idempotent. Every call issues a new
    /// [`WorldState::section_mesh_revisions`] value — rebuilds and
    /// translucent re-sorts both change what the GPU should draw.
    fn derive_section_artifacts(
        &mut self,
        coord: SectionCoord,
        camera: [f32; 3],
    ) -> Result<(), String> {
        let revision = self.next_mesh_revision;
        self.next_mesh_revision = self.next_mesh_revision.wrapping_add(1);
        self.section_mesh_revisions.insert(coord, revision);

        if self.mesh_cache.released_mesh_meta(coord).is_some() {
            // The opaque CPU payloads are released and the VRAM copy is
            // authoritative: `mesh(coord)` now holds only the kept
            // translucent re-sort quads, so re-deriving the full artifacts
            // would resurrect a translucent-only mesh as if it were the
            // whole section. Only the translucent order may move here.
            self.refresh_released_translucent_order(coord);
            return Ok(());
        }
        self.released_translucent_slots.remove(&coord);

        let (grouped, buckets, unpacked) = {
            let quads = self.mesh_cache.mesh(coord).unwrap_or(&[]);
            let mut packable = Vec::with_capacity(quads.len());
            let mut unpacked = Vec::new();
            for &quad in quads {
                // `PackedVertex` carries full u32 state ids, but the
                // renderer's 12-byte `CompactVoxelVertex` stream caps them at
                // 19 bits. Quads past that cap (every dynamic mod block state
                // id >= 1_000_000) would degenerate into four zeroed
                // invisible vertices there, so they take the general
                // `TerrainVertex` unpacked stream exactly like shaped
                // non-cube geometry does.
                if quad.needs_unpacked_terrain() || quad.state_id > COMPACT_VERTEX_MAX_STATE_ID {
                    unpacked.push(quad);
                } else {
                    packable.push(quad);
                }
            }
            let (grouped, buckets) = DirectionBuckets::group(&packable);
            (grouped, buckets, unpacked)
        };
        if unpacked.is_empty() {
            self.unpacked_terrain.remove(&coord);
        } else {
            self.unpacked_terrain.insert(coord, unpacked);
        }
        self.quad_counts.insert(coord, grouped.len());
        self.direction_buckets.insert(coord, buckets);
        self.render_regions.upload(coord, &grouped);

        let t_pack = Instant::now();
        let packed = pack_vertices(&grouped)
            .map_err(|code| format!("pack_vertices failed with ABI code {code}"))?;
        self.metrics.total_pack_us += t_pack.elapsed().as_micros() as u64;
        debug_assert_eq!(packed.len(), grouped.len() * 4);
        self.packed_mesh_digests
            .insert(coord, packed_vertices_digest(&packed));
        self.packed_meshes.insert(coord, packed);

        let local_cam = [
            camera[0] - (coord.0 * 16) as f32,
            camera[1] - (coord.1 * 16) as f32,
            camera[2] - (coord.2 * 16) as f32,
        ];
        let t_sort = Instant::now();
        let order = sort_translucent(&grouped, local_cam)
            .map_err(|code| format!("sort_translucent failed with ABI code {code}"))?;
        self.metrics.total_sort_us += t_sort.elapsed().as_micros() as u64;
        self.translucent_orders.insert(coord, order);
        Ok(())
    }

    /// Re-derive `translucent_orders` for a section whose opaque CPU
    /// payloads were released to VRAM. `mesh_cache.mesh(coord)` already
    /// holds the kept translucent quads in the new back-to-front order; the
    /// [`WorldState::released_translucent_slots`] map translates each kept
    /// quad back to its index in the grouped mesh the GPU uploaded, so the
    /// emitted order still indexes the VRAM-resident packed mesh. When the
    /// release bypassed [`WorldState::release_uploaded_section_cpu_copies`]
    /// no slot map exists and the previous order is kept — it still indexes
    /// real translucent quads, only its ordering is stale.
    fn refresh_released_translucent_order(&mut self, coord: SectionCoord) {
        let Some(slots) = self.released_translucent_slots.get(&coord) else {
            return;
        };
        let kept = self.mesh_cache.mesh(coord).unwrap_or(&[]);
        let order = kept
            .iter()
            .map(|quad| {
                slots
                    .get(&translucent_slot_key(quad))
                    .copied()
                    .unwrap_or(u32::MAX)
            })
            .collect();
        self.translucent_orders.insert(coord, order);
    }

    /// Visibility-graph nodes for `GraphCuller::search`: every loaded
    /// section — including uniform-air sections, which propagate sight but
    /// are never renderable — with its 6x6 face-reachability mask and
    /// renderable flag.
    pub fn section_graphs(&self) -> impl Iterator<Item = (SectionCoord, SectionGraph)> + '_ {
        self.visibility.iter().map(|(&coord, &entry)| {
            (
                coord,
                SectionGraph {
                    through: entry.faces.bits(),
                    renderable: entry.renderable,
                },
            )
        })
    }

    /// Direction-bucket ranges for one section, in grouped packed-vertex
    /// order. `None` until the section's first rebuild.
    pub fn direction_buckets(&self, coord: SectionCoord) -> Option<DirectionBuckets> {
        self.direction_buckets.get(&coord).copied()
    }

    /// The render descriptor the mesh build measured for one section:
    /// protrusion span and non-axis-geometry flag the renderer's cull and
    /// draw-bucket planners consume. `None` until the section's first
    /// rebuild.
    pub fn section_render_meta(&self, coord: SectionCoord) -> Option<SectionRenderMetadata> {
        self.mesh_cache.render_meta(coord)
    }

    /// Shared render-region arena for multi-draw command batching.
    pub fn render_regions(&self) -> &RenderRegions {
        &self.render_regions
    }

    /// Mesh `Section`s of one loaded chunk column, bottom to top, ready for
    /// `summarize_column` LOD ingestion. `None` when the column is not
    /// loaded.
    pub fn column_sections(&self, cx: i32, cz: i32) -> Option<Vec<Section>> {
        let column = self.columns.get(&(cx, cz))?;
        let min_sy = self.dimension.min_section_y();
        Some(
            column
                .sections
                .iter()
                .enumerate()
                .map(|(index, section)| {
                    let coord = SectionCoord(cx, min_sy + index as i32, cz);
                    section.to_mesh_section(&self.blocks, self.dynamic_overlay(coord))
                })
                .collect(),
        )
    }

    /// Packed 16-byte vertices for one section — the per-coordinate
    /// counterpart of [`packed_meshes`](Self::packed_meshes) that avoids
    /// materializing a lookup table per frame.
    pub fn packed_mesh(&self, coord: SectionCoord) -> Option<&[PackedVertex]> {
        self.packed_meshes.get(&coord).map(Vec::as_slice)
    }

    /// Digest computed at pack time for the section's packed vertex bytes.
    pub fn packed_mesh_digest(&self, coord: SectionCoord) -> Option<u64> {
        self.packed_mesh_digests.get(&coord).copied()
    }

    /// Iterate rebuilt sections as `(coord, packed 16-byte vertices)` for the
    /// software rasterizer. Four vertices per quad, corner order preserved,
    /// quads in direction-bucket grouped order.
    pub fn packed_meshes(&self) -> impl Iterator<Item = (SectionCoord, &[PackedVertex])> {
        self.packed_meshes
            .iter()
            .map(|(coord, verts)| (*coord, verts.as_slice()))
    }

    /// `sort_translucent` quad indices for one section, back-to-front over the
    /// grouped packed-vertex order.
    pub fn translucent_order(&self, coord: SectionCoord) -> Option<&[u32]> {
        self.translucent_orders.get(&coord).map(Vec::as_slice)
    }

    /// O(1) mesh-change detection for `client-gpu`: the last mesh-content
    /// revision issued for `coord`, bumped on every rebuild or translucent
    /// re-sort. `None` when the section is not loaded or was never meshed.
    /// Replaces byte-comparing `PackedVertex` slices with one `u64` compare.
    pub fn section_mesh_revision(&self, coord: SectionCoord) -> Option<u64> {
        self.section_mesh_revisions.get(&coord).copied()
    }

    /// Dense block-array bytes still retained by the mesh cache.
    pub fn mesh_cache_dense_bytes(&self) -> usize {
        self.mesh_cache.mesh_cache_dense_bytes()
    }

    /// Number of sections still holding their CPU `Vec<PackedVertex>` copy.
    /// Sections released through
    /// [`release_uploaded_section_cpu_copies`](Self::release_uploaded_section_cpu_copies)
    /// and sections not yet meshed are not counted.
    pub fn packed_mesh_count(&self) -> usize {
        self.packed_meshes.len()
    }

    /// Metadata kept for a section whose opaque CPU quads were released
    /// after VRAM upload; `None` while the full CPU mesh is still held.
    /// See [`MeshCache::released_mesh_meta`].
    pub fn released_mesh_meta(&self, coord: SectionCoord) -> Option<&ReleasedMeshMeta> {
        self.mesh_cache.released_mesh_meta(coord)
    }

    /// Post-upload zero-duplication release: once `coord`'s vertex data is
    /// registered for VRAM upload, drops both CPU copies of its opaque
    /// geometry — the `Vec<PackedVertex>` and the `MeshCache`'s opaque
    /// `Vec<Quad>`, which keeps only translucent/water quads for
    /// camera-driven re-sorts plus a [`ReleasedMeshMeta`].
    /// [`section_mesh_revisions`](Self::section_mesh_revision) stays intact
    /// so unchanged frames keep matching the VRAM-resident copy instead of
    /// re-uploading. A later block edit re-dirties the section and the next
    /// rebuild re-populates `packed_meshes` under a strictly higher
    /// revision. Returns the total CPU bytes freed.
    pub fn release_uploaded_section_cpu_copies(&mut self, coord: SectionCoord) -> usize {
        // Capture the translucent-quad -> grouped-index map only on the
        // first release, while the full mesh is still resident:
        // `mesh(coord)` shrinks to the kept translucent quads afterwards.
        if self.mesh_cache.released_mesh_meta(coord).is_none() {
            if let Some(quads) = self.mesh_cache.mesh(coord) {
                if quads.iter().any(quad_is_translucent) {
                    let (grouped, _) = DirectionBuckets::group(quads);
                    let slots: HashMap<TranslucentSlotKey, u32> = grouped
                        .iter()
                        .enumerate()
                        .filter(|(_, quad)| quad_is_translucent(quad))
                        .map(|(index, quad)| (translucent_slot_key(quad), index as u32))
                        .collect();
                    self.released_translucent_slots.insert(coord, slots);
                }
            }
        }
        let quad_bytes = self.mesh_cache.release_uploaded_opaque_cpu_mesh(coord);
        let packed_bytes = if self.mesh_cache.released_mesh_meta(coord).is_some() {
            self.packed_mesh_digests.remove(&coord);
            self.packed_meshes
                .remove(&coord)
                .map_or(0, |verts| verts.len() * std::mem::size_of::<PackedVertex>())
        } else {
            0
        };
        quad_bytes + packed_bytes
    }

    fn recompute_live_totals(&mut self) {
        self.metrics.total_quads = self.quad_counts.values().sum();
        self.metrics.packed_vertices = self.packed_meshes.values().map(Vec::len).sum();
        self.metrics.translucent_quads = self.translucent_orders.values().map(Vec::len).sum();
    }

    pub fn collect_collision_aabbs(&self, sweep: Aabb) -> Vec<Aabb> {
        // `as i32` saturates at the extremes; saturating ±1 keeps the bound
        // arithmetic from overflowing (and wrapping the range empty in
        // release while panicking in debug).
        let min_x = (sweep.min[0].floor() as i32).saturating_sub(1);
        let max_x = (sweep.max[0].floor() as i32).saturating_add(1);
        let min_y = (sweep.min[1].floor() as i32).saturating_sub(1);
        let max_y = (sweep.max[1].floor() as i32).saturating_add(1);
        let min_z = (sweep.min[2].floor() as i32).saturating_sub(1);
        let max_z = (sweep.max[2].floor() as i32).saturating_add(1);

        let mut shapes = Vec::new();
        for bx in min_x..=max_x {
            for bz in min_z..=max_z {
                for by in min_y..=max_y {
                    if let Some(state) = self.get_block_state(bx, by, bz) {
                        if state != 0 {
                            self.blocks
                                .push_collision_aabbs(state, bx, by, bz, &mut shapes);
                        }
                    }
                }
            }
        }
        shapes
    }

    pub fn fluid_cell_at(&self, x: i32, y: i32, z: i32) -> FluidCell {
        self.get_block_state(x, y, z)
            .map(|s| self.blocks.fluid_cell(s))
            .unwrap_or_default()
    }

    /// Standing eye height in Protocol 767 registry order (the same vanilla
    /// dimensions as `src/entity_registry.rs`), adjusted for tracked poses.
    pub fn entity_eye_height(&self, eid: i32) -> Option<f32> {
        const EYES: [f32; 130] = [
            0.36, 0.425, 0.26, 1.7775, 0.13, 0.2751, 0.45, 0.3, 1.53, 0.0, 0.5625, 1.74, 1.3452,
            0.0, 2.275, 0.35, 0.45, 0.5625, 0.595, 0.644, 0.195, 0.595, 1.3, 1.445, 0.3, 1.425,
            0.85, 1.74, 0.2125, 0.99875, 1.7, 6.8, 0.2125, 2.55, 0.13, 1.6575, 0.68, 0.2125, 0.425,
            0.2125, 0.83300006, 0.2125, 0.4, 0.425, 0.595, 2.6, 10.44, 0.0, 0.4, 1.105, 0.425,
            1.19, 0.595, 1.52, 1.74, 1.6575, 0.0, 2.295, 0.2125, 0.0, 0.0, 0.2125, 0.85, 0.0625,
            0.0, 1.7765, 0.2125, 0.325, 0.0, 0.595, 1.3, 1.52, 0.595, 0.425, 1.0625, 0.54, 0.175,
            0.765, 1.79, 1.79, 1.6575, 1.19, 0.2125, 0.455, 0.425, 1.8700001, 0.26, 1.235, 0.5,
            0.265625, 0.13, 1.74, 1.52, 0.325, 0.265625, 1.05, 1.7, 0.2125, 0.595, 0.13, 0.65, 0.4,
            1.74, 1.445, 0.19500001, 0.0, 0.15, 0.595, 1.7765, 0.13, 0.26, 0.34, 0.51875, 1.62,
            1.6575, 1.62, 2.4650001, 0.0, 1.62, 2.9750001, 2.1, 0.265625, 0.68, 1.19, 1.74, 1.52,
            1.74, 1.79, 1.62, 0.2125,
        ];
        let entity = self.entities.get(&eid)?;
        if protocol_767_entity_name(entity.entity_type) == Some("minecraft:player") {
            use megu_core::pose_ray::PlayerPoseKind;
            let pose = match self.entity_poses.get(&eid) {
                Some(1) => PlayerPoseKind::FallFlying,
                Some(2) => PlayerPoseKind::Sleeping,
                Some(3) => PlayerPoseKind::Swimming,
                Some(4) => PlayerPoseKind::SpinAttack,
                Some(5) => PlayerPoseKind::Crouching,
                Some(7) => PlayerPoseKind::Dying,
                _ => PlayerPoseKind::Standing,
            };
            return Some(pose.dimensions().eye_height);
        }
        let height = EYES
            .get(entity.entity_type as usize)
            .copied()
            .unwrap_or(1.53);
        let scale = if self
            .entity_visual_state(eid)
            .is_some_and(|visual| visual.is_baby)
        {
            0.5
        } else {
            1.0
        };
        Some(height * scale)
    }

    /// Return the typed visual projection of one tracked entity.
    pub fn entity_visual_state(&self, eid: i32) -> Option<EntityVisualState> {
        let entity = self.entities.get(&eid)?;
        let metadata = self.entity_metadata.get(&eid);
        let value = |index: u8| {
            metadata
                .and_then(|entries| entries.iter().find(|entry| entry.index == index))
                .map(|entry| &entry.value)
        };
        let flags = match value(0) {
            Some(crate::protocol::DecodedEntityMetadataValue::Byte(bits)) => *bits as u8,
            _ => 0,
        };
        let as_varint = |index| match value(index) {
            Some(crate::protocol::DecodedEntityMetadataValue::VarInt(value)) => Some(*value),
            _ => None,
        };
        let as_bool = |index| match value(index) {
            Some(crate::protocol::DecodedEntityMetadataValue::Bool(value)) => Some(*value),
            _ => None,
        };
        let entity_name = protocol_767_entity_name(entity.entity_type).unwrap_or("");
        let mut visual = EntityVisualState {
            on_fire: flags & 0x01 != 0,
            crouching: flags & 0x02 != 0,
            sprinting: flags & 0x08 != 0,
            swimming: flags & 0x10 != 0,
            invisible: flags & 0x20 != 0,
            glowing: flags & 0x40 != 0,
            is_baby: BABY_ENTITY_TYPES.contains(&entity_name) && as_bool(16).unwrap_or(false),
            health: match value(9) {
                Some(crate::protocol::DecodedEntityMetadataValue::Float(health))
                    if health.is_finite() =>
                {
                    Some(*health)
                }
                _ => None,
            },
            pose: self.entity_poses.get(&eid).copied(),
            hurt_ticks: entity.hurt_ticks,
            swing_ticks: self.entity_animation_ticks.get(&eid).copied().unwrap_or(0),
            ..EntityVisualState::default()
        };
        if entity_name == "minecraft:sheep" {
            if let Some(crate::protocol::DecodedEntityMetadataValue::Byte(color)) = value(17) {
                visual.sheep_color = Some((*color as u8) & 0x0f);
                visual.sheep_sheared = Some((*color as u8) & 0x10 != 0);
            }
        }
        if entity_name == "minecraft:creeper" {
            visual.creeper_state = as_varint(16);
            visual.creeper_charged = as_bool(17);
        }
        visual.villager_data = if entity_name == "minecraft:villager" {
            match value(17) {
                Some(crate::protocol::DecodedEntityMetadataValue::VillagerData {
                    kind,
                    profession,
                    level,
                }) => Some((*kind, *profession, *level)),
                _ => None,
            }
        } else {
            None
        };
        visual.shulker_peek = if entity_name == "minecraft:shulker" {
            match value(18) {
                Some(crate::protocol::DecodedEntityMetadataValue::Byte(peek)) => Some(*peek as u8),
                _ => None,
            }
        } else {
            None
        };
        visual.wither_invul_ticks = if entity_name == "minecraft:wither" {
            as_varint(16)
        } else {
            None
        };
        visual.warden_anger = if entity_name == "minecraft:warden" {
            as_varint(16)
        } else {
            None
        };
        visual.enderman_carried_block = if entity_name == "minecraft:enderman" {
            match value(16) {
                Some(crate::protocol::DecodedEntityMetadataValue::BlockState(state)) => {
                    Some(*state)
                }
                Some(crate::protocol::DecodedEntityMetadataValue::OptionalBlockState(state)) => {
                    *state
                }
                _ => None,
            }
        } else {
            None
        };
        visual.dropped_item_stack = if entity_name == "minecraft:item" {
            match value(8) {
                Some(crate::protocol::DecodedEntityMetadataValue::ItemStack(stack)) => {
                    Some(stack.clone())
                }
                _ => None,
            }
        } else {
            None
        };
        Some(visual)
    }

    /// Return typed block-entity state, combining parsed NBT with the newest matching block event.
    pub fn block_entity_visual_state(&self, pos: [i32; 3]) -> Option<BlockEntityVisualState> {
        let tracked = self.block_entities.get(&pos);
        let nbt = self.block_entity_visual_nbt.get(&pos);
        let event = self
            .block_events
            .iter()
            .rev()
            .find(|(event_pos, _, _, _)| *event_pos == pos);
        let action = event.map(|(_, action, _, _)| *action);
        let parameter = event.map(|(_, _, parameter, _)| *parameter).unwrap_or(0);
        let block_name = self
            .get_block_state(pos[0], pos[1], pos[2])
            .and_then(|state| self.blocks.state_name(state))
            .or_else(|| {
                event
                    .and_then(|(_, _, _, block_type)| u32::try_from(*block_type).ok())
                    .and_then(|state| self.blocks.state_name(state))
            })
            .unwrap_or("");
        let block_base_name = block_state_base_name(block_name);
        let Some(tracked) = tracked else {
            return block_base_name
                .contains("piston")
                .then(|| BlockEntityVisualState::Piston {
                    extending: action == Some(0),
                    facing: parameter,
                });
        };
        let id_tail = protocol_767_visual_block_entity(tracked.kind)
            .or_else(|| nbt.and_then(|value| nbt_string(value, "id")?.rsplit(':').next()))
            .unwrap_or("");
        Some(block_entity_visual_from(
            id_tail,
            block_base_name,
            nbt,
            action,
            parameter,
        ))
    }

    /// Project a retained `level_event` into the typed visual enum.
    pub fn visual_level_events(
        &self,
    ) -> impl Iterator<Item = (VisualLevelEvent, [i32; 3], bool)> + '_ {
        self.level_events
            .iter()
            .map(|(event, pos, data, global)| (visual_level_event(*event, *data), *pos, *global))
    }

    /// Unified ingestion for decoded Protocol 767 play events. The family
    /// handlers below route chunk/block, entity, container, environment and
    /// combat/effect payloads into the live [`WorldState`] deterministically.
    /// Player/session concerns (teleport, health, chat, HUD, settings) and
    /// unrecognized payloads are acknowledged and ignored; the caller keeps
    /// owning those.
    pub fn apply_play_event(&mut self, event: &PlayEvent) -> Result<(), String> {
        // Families are tried in order and the first owner wins, so no family
        // has to restate the whole envelope to prove it handled the event.
        let _ = self.apply_chunk_play_event(event)?
            || self.apply_entity_play_event(event)?
            || self.apply_container_play_event(event)?
            || self.apply_world_play_event(event)?
            || self.apply_combat_play_event(event)?
            || self.apply_map_and_scoreboard_event(event)?;
        Ok(())
    }

    /// Chunk / block / light: column ingest, light and block edits, chunk
    /// unload, explosion removals and biome re-paints.
    fn apply_chunk_play_event(&mut self, event: &PlayEvent) -> Result<bool, String> {
        match event {
            PlayEvent::ChunkData(chunk) => self.apply_chunk_borrowed(chunk)?,
            PlayEvent::UpdateLight { cx, cz, light } => self.apply_light_update(*cx, *cz, light)?,
            PlayEvent::BlockUpdate { pos, state_id } => self.apply_block_update(*pos, *state_id)?,
            PlayEvent::SectionBlocksUpdate(updates) => self.apply_block_updates(updates)?,
            PlayEvent::UnloadChunk { cx, cz } => self.unload_chunk(*cx, *cz),
            PlayEvent::Explosion {
                x, y, z, records, ..
            } => {
                self.apply_explosion_removals(*x, *y, *z, records)?;
            }
            PlayEvent::ChunksBiomes { chunks } => self.apply_chunks_biomes(chunks),
            _ => return Ok(false),
        }
        Ok(true)
    }

    /// Live entities: spawn/despawn, movement, rotation, metadata, attributes,
    /// passengers, links and equipment.
    fn apply_entity_play_event(&mut self, event: &PlayEvent) -> Result<bool, String> {
        match event {
            PlayEvent::AddEntity {
                eid,
                uuid,
                entity_type,
                x,
                y,
                z,
                pitch,
                yaw,
                head_yaw,
                vx,
                vy,
                vz,
                ..
            } => {
                // Bounded: a full table ignores excess spawns; re-spawning a
                // tracked id replaces its row without growing the map.
                if has_capacity(&self.entities, eid, MAX_TRACKED_ENTITIES) {
                    let pos = [clean_f64(*x), clean_f64(*y), clean_f64(*z)];
                    let entity = TrackedEntity {
                        eid: *eid,
                        uuid: *uuid,
                        entity_type: *entity_type,
                        pos_q: pos.map(quantize_pos_4096),
                        pos,
                        vel: [clean_f64(*vx), clean_f64(*vy), clean_f64(*vz)],
                        yaw_deg: clean_f32(*yaw),
                        pitch_deg: clean_f32(*pitch),
                        head_yaw_deg: clean_f32(*head_yaw),
                        on_ground: false,
                        hurt_ticks: 0,
                        last_animation: None,
                        equipment_slots: 0,
                    };
                    self.entities.insert(*eid, entity);
                }
            }
            PlayEvent::SetCamera { eid } => {
                self.camera_entity_id =
                    Some(*eid).filter(|id| Some(*id) != self.local_player_entity_id);
            }
            PlayEvent::RemoveEntities { eids } => {
                for &eid in eids {
                    self.forget_entity(eid);
                }
            }
            PlayEvent::MoveEntityPos {
                eid,
                dx,
                dy,
                dz,
                on_ground,
            } => {
                self.move_tracked_entity(*eid, [*dx, *dy, *dz], None, *on_ground);
            }
            PlayEvent::MoveEntityPosRot {
                eid,
                dx,
                dy,
                dz,
                yaw,
                pitch,
                on_ground,
            } => {
                self.move_tracked_entity(*eid, [*dx, *dy, *dz], Some((*yaw, *pitch)), *on_ground);
            }
            PlayEvent::MoveEntityRot {
                eid,
                yaw,
                pitch,
                on_ground,
            } => {
                self.move_tracked_entity(*eid, [0.0; 3], Some((*yaw, *pitch)), *on_ground);
            }
            PlayEvent::TeleportEntity {
                eid,
                x,
                y,
                z,
                yaw,
                pitch,
                on_ground,
            } => {
                self.teleport_tracked_entity(*eid, [*x, *y, *z], *yaw, *pitch, *on_ground);
            }
            PlayEvent::RotateHead { eid, head_yaw } => {
                if let Some(entity) = self.entities.get_mut(eid) {
                    entity.head_yaw_deg = clean_f32(*head_yaw);
                }
            }
            PlayEvent::SetEntityMotion { eid, vx, vy, vz } => {
                if let Some(entity) = self.entities.get_mut(eid) {
                    entity.vel = [clean_f64(*vx), clean_f64(*vy), clean_f64(*vz)];
                }
            }
            PlayEvent::SetEntityData { eid, raw_entries } => {
                self.apply_entity_metadata(*eid, raw_entries)?;
            }
            PlayEvent::UpdateAttributes { eid, attributes } => bounded_insert(
                &mut self.entity_attributes,
                *eid,
                attributes.clone(),
                MAX_TRACKED_ENTITIES,
            ),
            PlayEvent::SetPassengers {
                vehicle,
                passengers,
            } => bounded_insert(
                &mut self.passengers,
                *vehicle,
                passengers.clone(),
                MAX_TRACKED_ENTITIES,
            ),
            PlayEvent::SetEntityLink {
                source,
                destination,
            } => bounded_insert(
                &mut self.entity_links,
                *source,
                *destination,
                MAX_TRACKED_ENTITIES,
            ),
            PlayEvent::SetEquipment { eid, entries } => {
                if let Some(entity) = self.entities.get_mut(eid) {
                    for entry in entries {
                        if entry.slot < 8 {
                            entity.equipment_slots |= 1u8 << entry.slot;
                        }
                        if entry.slot <= 6
                            && has_capacity(&self.entity_equipment, &(*eid, entry.slot), 4096)
                        {
                            self.entity_equipment
                                .insert((*eid, entry.slot), entry.item_wire.clone());
                        }
                    }
                }
            }
            _ => return Ok(false),
        }
        Ok(true)
    }

    /// Containers / inventory: screen open-close, property updates and
    /// merchant offers.
    fn apply_container_play_event(&mut self, event: &PlayEvent) -> Result<bool, String> {
        match event {
            PlayEvent::OpenScreen { .. } => {
                self.merchant_panels.clear();
                self.container_properties.retain(|(id, _), _| *id == 0);
            }
            PlayEvent::HorseScreenOpen { .. } => {
                // Mount (horse/donkey/mule/llama) inventories open through
                // the same window machinery as `open_screen`: drop stale
                // merchant panels and non-zero-window properties identically.
                self.merchant_panels.clear();
                self.container_properties.retain(|(id, _), _| *id == 0);
            }
            PlayEvent::ContainerClose { window_id } => {
                self.merchant_panels.remove(window_id);
                self.container_properties
                    .retain(|(id, _), _| id != window_id);
            }
            PlayEvent::ContainerSetData {
                window_id,
                property,
                value,
            } => bounded_insert(
                &mut self.container_properties,
                (*window_id, *property),
                *value,
                1024,
            ),
            PlayEvent::MerchantOffers {
                window_id,
                offers,
                villager_level,
                villager_xp,
                show_progress,
                can_restock,
            } => {
                if let Ok(window_id) = u8::try_from(*window_id) {
                    self.merchant_panels.retain(|id, _| *id == window_id);
                    let panel = MerchantPanelState {
                        offers: offers.clone(),
                        villager_level: *villager_level,
                        villager_xp: *villager_xp,
                        show_progress: *show_progress,
                        can_restock: *can_restock,
                    };
                    self.merchant_panels.insert(window_id, panel);
                }
            }
            _ => return Ok(false),
        }
        Ok(true)
    }

    /// World / environment: dimension resets, time, weather and game events,
    /// block-entity data, block events/destruction and level effects.
    fn apply_world_play_event(&mut self, event: &PlayEvent) -> Result<bool, String> {
        match event {
            PlayEvent::JoinGame {
                dimension,
                entity_id,
                ..
            } => {
                self.reset_dimension(dimension.clone());
                self.local_player_entity_id = Some(*entity_id);
                self.tab_header.clear();
                self.tab_footer.clear();
                self.maps.clear();
                self.map_recency.clear();
                self.teams.clear();
                self.member_teams.clear();
            }
            PlayEvent::Respawn { dimension } => self.reset_dimension(dimension.clone()),
            PlayEvent::SetTime {
                world_age,
                time_of_day,
            } => {
                // Idle no-op: a repeated time sync mutates nothing.
                if self.world_age != *world_age || self.time_of_day != *time_of_day {
                    self.world_age = *world_age;
                    self.time_of_day = *time_of_day;
                }
            }
            PlayEvent::GameEvent { event, value } => self.apply_game_event(*event, *value),
            PlayEvent::BlockDestruction {
                breaker_eid,
                pos,
                stage,
            } => {
                self.apply_block_destruction(*breaker_eid, *pos, *stage);
            }
            PlayEvent::BlockEvent {
                pos,
                action,
                parameter,
                block_type,
            } => push_bounded(
                &mut self.block_events,
                (*pos, *action, *parameter, *block_type),
                64,
            ),
            PlayEvent::BlockEntityData {
                pos,
                kind,
                nbt_bytes,
            } => {
                self.apply_block_entity_data(*pos, *kind, nbt_bytes);
            }
            PlayEvent::LevelEvent {
                event,
                pos,
                data,
                disable_relative_volume,
            } => push_bounded(
                &mut self.level_events,
                (*event, *pos, *data, *disable_relative_volume),
                64,
            ),
            PlayEvent::LevelParticles {
                particle_id,
                long_distance,
                x,
                y,
                z,
                offset_x,
                offset_y,
                offset_z,
                max_speed,
                count,
                particle_data,
            } => {
                let options = decode_particle_options(*particle_id, particle_data)?;
                if self.level_particles.len() == 256 {
                    self.level_particles.pop_front();
                    self.level_particle_options.pop_front();
                }
                let spawn = LevelParticleSpawn {
                    particle_id: *particle_id,
                    long_distance: *long_distance,
                    pos: [*x, *y, *z],
                    offset: [*offset_x, *offset_y, *offset_z],
                    max_speed: *max_speed,
                    count: *count,
                };
                self.level_particles.push_back(spawn);
                self.level_particle_options
                    .push_back((*particle_id, options));
            }
            PlayEvent::InitializeBorder {
                x,
                z,
                old_diameter,
                new_diameter,
                lerp_ms,
                portal_boundary,
                warning_blocks,
                warning_time,
            } => {
                self.world_border.center_x = *x;
                self.world_border.center_z = *z;
                self.world_border.portal_boundary = *portal_boundary;
                self.world_border.warning_blocks = *warning_blocks;
                self.world_border.warning_time = *warning_time;
                self.world_border
                    .begin_lerp(*old_diameter, *new_diameter, *lerp_ms);
            }
            PlayEvent::SetBorderCenter { x, z } => {
                self.world_border.center_x = *x;
                self.world_border.center_z = *z;
            }
            PlayEvent::SetBorderSize { diameter } => {
                self.world_border.current_diameter = diameter.max(0.0);
                self.world_border.target_diameter = diameter.max(0.0);
                self.world_border.lerp_ticks_remaining = 0;
                self.world_border.lerp_ticks_total = 0;
            }
            PlayEvent::SetBorderLerpSize {
                old_diameter,
                new_diameter,
                lerp_ms,
            } => self
                .world_border
                .begin_lerp(*old_diameter, *new_diameter, *lerp_ms),
            PlayEvent::SetBorderWarningDistance { warning_blocks } => {
                self.world_border.warning_blocks = *warning_blocks;
            }
            PlayEvent::SetBorderWarningDelay { warning_time } => {
                self.world_border.warning_time = *warning_time;
            }
            PlayEvent::SetDefaultSpawnPosition { pos, angle } => {
                self.default_spawn_position = Some((*pos, *angle));
            }
            PlayEvent::ChangeDifficulty { difficulty, locked } => {
                self.difficulty = Some((*difficulty, *locked));
            }
            PlayEvent::TickingState { tick_rate, frozen } => {
                self.server_tick_rate = if tick_rate.is_finite() {
                    tick_rate.clamp(0.0, 10_000.0)
                } else {
                    20.0
                };
                self.server_ticks_frozen = *frozen;
            }
            PlayEvent::TickingStep { steps } => {
                self.server_tick_steps = (*steps).max(0);
            }
            PlayEvent::AwardStats { entries } => {
                for entry in entries.iter().take(4096) {
                    if self.statistics.len() >= 8192 {
                        break;
                    }
                    self.statistics
                        .insert((entry.category, entry.statistic), entry.value);
                }
            }
            PlayEvent::ServerData { motd_nbt, icon } => {
                if motd_nbt.len() <= 64 * 1024 {
                    let text = WireReader::new(motd_nbt)
                        .read_nbt()
                        .ok()
                        .map(|nbt| nbt.extract_text())
                        .filter(|text| !text.is_empty())
                        .map(|text| text.chars().take(512).collect::<String>());
                    self.server_motd = text;
                }
                self.server_icon = icon.clone().filter(|bytes| bytes.len() <= 256 * 1024);
                self.server_enforces_secure_chat = None;
            }
            PlayEvent::ChunkBatchStart => {
                self.chunk_batch_active = true;
                self.chunk_batches_started = self.chunk_batches_started.saturating_add(1);
            }
            PlayEvent::ChunkBatchFinished(size) => {
                self.chunk_batch_active = false;
                self.chunk_batches_finished = self.chunk_batches_finished.saturating_add(1);
                self.last_chunk_batch_size = (*size).max(0) as u64;
            }
            _ => return Ok(false),
        }
        Ok(true)
    }

    /// Player / combat / effects: hurt flashes, swing animation, mob effects,
    /// boss bars and item pickups.
    fn apply_combat_play_event(&mut self, event: &PlayEvent) -> Result<bool, String> {
        match event {
            // `entity_event` status 2 is the vanilla hurt flash; other
            // statuses carry no tracked state.
            PlayEvent::EntityEvent { eid, status } if *status == 2 => self.flash_hurt(*eid),
            PlayEvent::EntityEvent { .. } => {}
            // `damage_event` (1.19.4+) reports the same hurt the legacy
            // `hurt_animation` carried — flash the victim identically.
            PlayEvent::HurtAnimation { eid, .. } => self.flash_hurt(*eid),
            PlayEvent::DamageEvent {
                eid, source_cause, ..
            } => {
                self.flash_hurt(*eid);
                if Some(*eid) == self.local_player_entity_id {
                    // Protocol 767 encodes optional damage-source IDs as ID + 1, or 0.
                    self.combat.last_attacker_id = (*source_cause > 0).then(|| source_cause - 1);
                }
            }
            PlayEvent::PlayerCombatEnter => {
                self.combat.in_combat = true;
                self.combat.ticks = 0;
            }
            PlayEvent::PlayerCombatEnd { duration } => {
                self.combat.in_combat = false;
                self.combat.ticks = (*duration).max(0) as u32;
            }
            PlayEvent::PlayerCombatKill { player_id, .. }
                if Some(*player_id) == self.local_player_entity_id =>
            {
                self.combat.in_combat = false;
            }
            PlayEvent::Animate { eid, animation } => {
                if let Some(entity) = self.entities.get_mut(eid) {
                    entity.last_animation = Some(*animation);
                }
                // 0 and 3 are the arm-swing pair (6 ticks); the rest run 5.
                let ticks = if *animation == 0 || *animation == 3 {
                    6
                } else {
                    5
                };
                bounded_insert(
                    &mut self.entity_animation_ticks,
                    *eid,
                    ticks,
                    MAX_TRACKED_ENTITIES,
                );
            }
            PlayEvent::UpdateMobEffect {
                eid,
                effect_id,
                amplifier,
                duration,
                flags,
            } => {
                let effect = TrackedMobEffect {
                    amplifier: *amplifier,
                    duration: *duration,
                    flags: *flags,
                };
                bounded_insert(&mut self.mob_effects, (*eid, *effect_id), effect, 4096);
            }
            PlayEvent::RemoveMobEffect { eid, effect_id } => {
                self.mob_effects.remove(&(*eid, *effect_id));
            }
            PlayEvent::TakeItemEntity {
                item_entity,
                player,
                amount,
            } => push_bounded(&mut self.item_pickups, (*item_entity, *player, *amount), 64),
            PlayEvent::BossEvent {
                uuid,
                action,
                raw_payload,
            } => match decode_boss_bar_action(*action, raw_payload)? {
                DecodedBossBarAction::Add {
                    name_nbt,
                    progress,
                    color,
                    divisions,
                    flags,
                } => {
                    let bar = TrackedBossBar {
                        name_nbt,
                        progress,
                        color,
                        divisions,
                        flags,
                    };
                        bounded_insert(&mut self.boss_bars, *uuid, bar, 128);
                    }
                    DecodedBossBarAction::Remove => {
                        self.boss_bars.remove(uuid);
                    }
                    DecodedBossBarAction::UpdateProgress(progress) => {
                        if let Some(bar) = self.boss_bars.get_mut(uuid) {
                            bar.progress = progress;
                        }
                    }
                    DecodedBossBarAction::UpdateName(name_nbt) => {
                        if let Some(bar) = self.boss_bars.get_mut(uuid) {
                            bar.name_nbt = name_nbt;
                        }
                    }
                    DecodedBossBarAction::UpdateStyle { color, divisions } => {
                        if let Some(bar) = self.boss_bars.get_mut(uuid) {
                            bar.color = color;
                            bar.divisions = divisions;
                        }
                    }
                    DecodedBossBarAction::UpdateFlags(flags) => {
                        if let Some(bar) = self.boss_bars.get_mut(uuid) {
                            bar.flags = flags;
                        }
                    }
            },
            _ => return Ok(false),
        }
        Ok(true)
    }

    fn apply_map_and_scoreboard_event(&mut self, event: &PlayEvent) -> Result<bool, String> {
        match event {
            PlayEvent::TabList {
                header_nbt,
                footer_nbt,
            } => {
                self.tab_header = component_text(header_nbt);
                self.tab_footer = component_text(footer_nbt);
            }
            PlayEvent::SetPlayerTeam {
                name,
                mode,
                info,
                entities,
            } => {
                self.apply_team(name, *mode, info.as_deref(), entities);
            }
            PlayEvent::MapItemData {
                map_id,
                scale,
                locked,
                icons,
                columns,
                rows,
                x,
                y,
                colors,
            } => {
                let [width, height, x, z] = [*columns, *rows, *x, *y].map(usize::from);
                if width != 0
                    && (height == 0
                        || x + width > MAP_SIDE
                        || z + height > MAP_SIDE
                        || colors.len() != width * height)
                {
                    return Err("map patch outside 128x128 bounds".into());
                }
                self.map_recency.retain(|id| id != map_id);
                if self.map_recency.len() == MAX_TRACKED_MAPS {
                    if let Some(oldest) = self.map_recency.pop_front() {
                        self.maps.remove(&oldest);
                    }
                }
                self.map_recency.push_back(*map_id);
                let map = self.maps.entry(*map_id).or_insert_with(|| TrackedMap {
                    scale: *scale,
                    locked: *locked,
                    pixels: vec![[0; 4]; MAP_SIDE * MAP_SIDE].into(),
                    icons: Arc::default(),
                });
                map.scale = *scale;
                map.locked = *locked;
                if let Some(icons) = icons {
                    map.icons = icons.iter().take(256).cloned().collect();
                }
                if width != 0 {
                    let pixels = Arc::make_mut(&mut map.pixels);
                    for (row, colors) in colors.chunks_exact(width).enumerate() {
                        let start = (z + row) * MAP_SIDE + x;
                        for (pixel, &color) in pixels[start..start + width].iter_mut().zip(colors) {
                            *pixel = map_pixel(color);
                        }
                    }
                }
            }
            _ => return Ok(false),
        }
        Ok(true)
    }

    fn apply_team(
        &mut self,
        name: &str,
        mode: u8,
        info: Option<&PlayerTeamInfo>,
        members: &[String],
    ) {
        match mode {
            0 => {
                let Some(info) = info else { return };
                if !self.teams.contains_key(name) && self.teams.len() >= MAX_TRACKED_TEAMS {
                    return;
                }
                self.remove_team(name);
                self.teams.insert(
                    name.to_owned(),
                    ScoreboardTeam {
                        info: info.clone(),
                        members: BTreeSet::new(),
                    },
                );
            }
            1 => self.remove_team(name),
            2 => {
                if let (Some(team), Some(info)) = (self.teams.get_mut(name), info) {
                    team.info = info.clone();
                }
            }
            3 | 4 => {}
            _ => return,
        }
        if !self.teams.contains_key(name) {
            return;
        }
        for member in members {
            match mode {
                0 | 3 if has_capacity(&self.member_teams, member, MAX_TEAM_MEMBERS) => {
                    if let Some(old) = self.member_teams.insert(member.clone(), name.to_owned()) {
                        if let Some(team) = self.teams.get_mut(&old) {
                            team.members.remove(member);
                        }
                    }
                    self.teams
                        .get_mut(name)
                        .expect("team checked above")
                        .members
                        .insert(member.clone());
                }
                4 if self
                    .member_teams
                    .get(member)
                    .is_some_and(|team| team == name) =>
                {
                    self.member_teams.remove(member);
                    self.teams
                        .get_mut(name)
                        .expect("team checked above")
                        .members
                        .remove(member);
                }
                _ => {}
            }
        }
    }

    fn remove_team(&mut self, name: &str) {
        if let Some(team) = self.teams.remove(name) {
            for member in team.members {
                self.member_teams.remove(&member);
            }
        }
    }

    pub fn team_for_member(&self, member: &str) -> Option<&ScoreboardTeam> {
        self.teams.get(self.member_teams.get(member)?)
    }

    /// Scoreboard-only part of vanilla's pushability predicate. Physics may
    /// still reject a pair for other reasons (e.g. spectator, non-pushable mob).
    pub fn team_allows_collision(&self, member: &str, other: &str) -> bool {
        let (a, b) = (self.member_teams.get(member), self.member_teams.get(other));
        let same = a.is_some() && a == b;
        [a, b].into_iter().flatten().all(|name| {
            match self.teams[name].info.collision_rule.as_str() {
                "never" => false,
                "pushOtherTeams" | "push_other_teams" => !same,
                "pushOwnTeam" | "push_own_team" => same,
                _ => true,
            }
        })
    }

    pub fn team_allows_seeing_invisible_member(&self, member: &str, viewer: &str) -> bool {
        let (Some(team), Some(other)) =
            (self.team_for_member(member), self.team_for_member(viewer))
        else {
            return false;
        };
        std::ptr::eq(team, other) && team.info.friendly_flags & 0x02 != 0
    }

    pub fn team_shows_nametag(&self, member: &str, viewer: &str) -> bool {
        let Some(team) = self.team_for_member(member) else {
            return true;
        };
        let viewer_team = self.member_teams.get(viewer);
        let same = self.member_teams.get(member) == viewer_team;
        match team.info.nametag_visibility.as_str() {
            "never" => false,
            "hideForOtherTeams" | "hide_for_other_teams" => viewer_team.is_none() || same,
            "hideForOwnTeam" | "hide_for_own_team" => !same,
            _ => true,
        }
    }

    /// Drop every row keyed by `eid` from the tracked-entity families.
    fn forget_entity(&mut self, eid: i32) {
        if self.camera_entity_id == Some(eid) {
            self.camera_entity_id = None;
        }
        self.entities.remove(&eid);
        self.entity_metadata.remove(&eid);
        self.entity_poses.remove(&eid);
        self.entity_animation_ticks.remove(&eid);
        self.entity_equipment
            .retain(|(entity, _), _| *entity != eid);
        self.mob_effects.retain(|(entity, _), _| *entity != eid);
        self.entity_attributes.remove(&eid);
        self.passengers.remove(&eid);
        self.entity_links.remove(&eid);
        for passengers in self.passengers.values_mut() {
            passengers.retain(|&passenger| passenger != eid);
        }
        // A removed breaker stops cracking blocks.
        self.block_destructions.remove(&eid);
    }

    /// Set the hurt-flash timer on a tracked entity; unknown ids are ignored.
    fn flash_hurt(&mut self, eid: i32) {
        if let Some(entity) = self.entities.get_mut(&eid) {
            entity.hurt_ticks = HURT_FLASH_TICKS;
        }
    }

    /// `set_entity_data`: merge the decoded metadata entries by index and keep
    /// the shared pose field in its own map. Bounded like the rest of the
    /// entity family — a full table drops the packet whole.
    fn apply_entity_metadata(&mut self, eid: i32, raw_entries: &[u8]) -> Result<(), String> {
        let entries = decode_entity_metadata(raw_entries)?;
        if !has_capacity(&self.entity_metadata, &eid, MAX_TRACKED_ENTITIES) {
            return Ok(());
        }
        if let Some(pose) = entries
            .iter()
            .find_map(|entry| (entry.index == 6).then_some(&entry.value))
            .and_then(|value| match value {
                crate::protocol::DecodedEntityMetadataValue::Pose(pose) => Some(*pose),
                _ => None,
            })
        {
            self.entity_poses.insert(eid, pose);
        }
        let current = self.entity_metadata.entry(eid).or_default();
        for update in entries {
            if let Some(existing) = current.iter_mut().find(|entry| entry.index == update.index) {
                *existing = update;
            } else {
                current.push(update);
            }
        }
        current.sort_by_key(|entry| entry.index);
        Ok(())
    }

    /// `chunks_biomes`: decode each column's stream of 64-cell biome
    /// paletted containers (one per stored section, bottom-up) and patch
    /// the matching `StoredSection::biomes` in place. A malformed or
    /// truncated payload stops at the last good section — the packet is
    /// a cosmetic color hint, never worth a desync.
    fn apply_chunks_biomes(&mut self, chunks: &[ChunkBiomeData]) {
        // v767 negotiates a 64-entry vanilla biome registry; the direct
        // palette width follows the same ceil-log2 rule the chunk codec
        // uses (`decode_chunk_packet_profiled`).
        let direct_bits = (usize::BITS - 63usize.leading_zeros()) as u8;
        for chunk in chunks {
            let Some(column) = self.columns.get_mut(&(chunk.cx, chunk.cz)) else {
                continue;
            };
            let mut reader = WireReader::new(&chunk.data);
            let min_sy = self.dimension.min_section_y();
            for (index, section) in column.sections.iter_mut().enumerate() {
                if reader.remaining() == 0 {
                    break;
                }
                let Ok(biomes) = decode_paletted_container(&mut reader, 64, 3, 1, direct_bits)
                else {
                    break;
                };
                if section.biomes == biomes.as_slice() {
                    continue;
                }
                section.biomes.copy_from_slice(&biomes);
                if section.has_non_air {
                    let coord = SectionCoord(chunk.cx, min_sy + index as i32, chunk.cz);
                    let dynamic = self
                        .section_dynamic_states
                        .get(&coord)
                        .unwrap_or(&EMPTY_DYNAMIC_STATES);
                    self.mesh_cache.insert_compressed(
                        coord,
                        section.to_compressed_mesh_section(&self.blocks, dynamic),
                    );
                }
            }
        }
    }

    /// Relative-move family (`move_entity_pos`, `move_entity_pos_rot`,
    /// `move_entity_rot`): accumulate the decoded block delta onto the f64
    /// position and the 1/4096 fixed-point `pos_q` mirror — the client side
    /// of vanilla's `VecDeltaCodec`. Unknown ids are ignored; only
    /// `add_entity` creates rows.
    fn move_tracked_entity(
        &mut self,
        eid: i32,
        delta: [f64; 3],
        rot: Option<(f32, f32)>,
        on_ground: bool,
    ) {
        let Some(entity) = self.entities.get_mut(&eid) else {
            return;
        };
        for ((pos, pos_q), &d) in entity
            .pos
            .iter_mut()
            .zip(entity.pos_q.iter_mut())
            .zip(delta.iter())
        {
            let d = clean_f64(d);
            *pos = clean_f64(*pos + d);
            *pos_q = pos_q.saturating_add(quantize_pos_4096(d));
        }
        if let Some((yaw, pitch)) = rot {
            entity.yaw_deg = clean_f32(yaw);
            entity.pitch_deg = clean_f32(pitch);
        }
        entity.on_ground = on_ground;
    }

    /// `teleport_entity`: absolute overwrite of position (f64 + fixed-point
    /// mirror), yaw/pitch, and on-ground. Unknown ids are ignored.
    fn teleport_tracked_entity(
        &mut self,
        eid: i32,
        pos_in: [f64; 3],
        yaw: f32,
        pitch: f32,
        on_ground: bool,
    ) {
        let Some(entity) = self.entities.get_mut(&eid) else {
            return;
        };
        let pos = [
            clean_f64(pos_in[0]),
            clean_f64(pos_in[1]),
            clean_f64(pos_in[2]),
        ];
        entity.pos_q = pos.map(quantize_pos_4096);
        entity.pos = pos;
        entity.yaw_deg = clean_f32(yaw);
        entity.pitch_deg = clean_f32(pitch);
        entity.on_ground = on_ground;
    }

    /// Weather `game_event`s, mirroring vanilla `ClientPacketListener`:
    /// `START_RAINING` raises the rain flag and zeroes the level, letting
    /// `RAIN_LEVEL_CHANGE` (7) ramp it up one server tick at a time instead
    /// of snapping to `1.0`; `STOP_RAINING` only clears the flag, again
    /// leaving the ramp to event 7. Events 7/8 carry the level as a float
    /// clamped NaN-safe to `0.0..=1.0`; all other game events are ignored.
    fn apply_game_event(&mut self, event: u8, value: f32) {
        match event {
            GAME_EVENT_START_RAINING => {
                self.is_raining = true;
                self.rain_level = 0.0;
            }
            GAME_EVENT_STOP_RAINING => self.is_raining = false,
            GAME_EVENT_RAIN_LEVEL_CHANGE => self.rain_level = clean_weather_level(value),
            GAME_EVENT_THUNDER_LEVEL_CHANGE => self.thunder_level = clean_weather_level(value),
            3 if value.is_finite() && (0.0..=3.0).contains(&value) => {
                self.game_mode = Some(value as u8);
            }
            _ => {}
        }
    }

    /// `block_destruction`: stage `0..=9` records/updates the crack overlay;
    /// `>= 10` or the wire `-1` sentinel (255 as `u8`) clears it. Bounded —
    /// new breakers past [`MAX_BLOCK_DESTRUCTIONS`] are ignored while
    /// updates to already-tracked breakers always land.
    fn apply_block_destruction(&mut self, breaker_eid: i32, pos: [i32; 3], stage: i8) {
        let stage = stage as u8;
        if stage > 9 {
            self.block_destructions.remove(&breaker_eid);
            return;
        }
        if !self.block_destructions.contains_key(&breaker_eid)
            && self.block_destructions.len() >= MAX_BLOCK_DESTRUCTIONS
        {
            return;
        }
        self.block_destructions
            .insert(breaker_eid, BlockBreakProgress { pos, stage });
    }

    /// `explode`: the packet's destruction records are byte offsets from
    /// `floor(center)` — the authoritative client-side block removals,
    /// all written as air (global palette state `0`). The record list is
    /// bounded by the decoder's
    /// [`MAX_EXPLOSION_RECORDS`](crate::protocol::MAX_EXPLOSION_RECORDS)
    /// cap; every
    /// write flows through the incremental light queue like
    /// `block_update` does.
    fn apply_explosion_removals(
        &mut self,
        x: f64,
        y: f64,
        z: f64,
        records: &[[i8; 3]],
    ) -> Result<(), String> {
        if records.is_empty() {
            return Ok(());
        }
        let base = [x.floor() as i32, y.floor() as i32, z.floor() as i32];
        let updates: Vec<([i32; 3], u32)> = records
            .iter()
            .map(|r| {
                (
                    [
                        base[0] + i32::from(r[0]),
                        base[1] + i32::from(r[1]),
                        base[2] + i32::from(r[2]),
                    ],
                    0,
                )
            })
            .collect();
        self.apply_block_updates(&updates)
    }

    /// `block_entity_data`: upsert the block entity at `pos` — the live
    /// update path for signs, chests, spawners, etc., complementing the
    /// chunk-payload population in [`Self::refresh_chunk_block_entities`].
    /// Bounded by [`MAX_TRACKED_BLOCK_ENTITIES`].
    fn apply_block_entity_data(&mut self, pos: [i32; 3], kind: i32, nbt_bytes: &[u8]) {
        // Idle no-op: a repeated payload for the tracked row re-derives the
        // identical NBT projection, so skip the reparse entirely.
        if self
            .block_entities
            .get(&pos)
            .is_some_and(|entry| entry.kind == kind && entry.nbt_bytes == nbt_bytes)
        {
            return;
        }
        if self.block_entities.len() >= MAX_TRACKED_BLOCK_ENTITIES
            && !self.block_entities.contains_key(&pos)
        {
            return;
        }
        let entry = self
            .block_entities
            .entry(pos)
            .or_insert_with(|| TrackedBlockEntity {
                kind,
                nbt_bytes: Vec::new(),
            });
        entry.kind = kind;
        entry.nbt_bytes.clear();
        entry.nbt_bytes.extend_from_slice(nbt_bytes);

        // Direct event callers may supply opaque bytes. Keep them in the
        // raw record and restrict the visual projection to complete NBT.
        let mut reader = WireReader::new(nbt_bytes);
        if let Ok(nbt) = reader.read_nbt() {
            if reader.remaining() == 0 {
                self.block_entity_visual_nbt.insert(pos, nbt);
                return;
            }
        }
        self.block_entity_visual_nbt.remove(&pos);
    }

    /// Advance per-tick entity bookkeeping once per client tick: the
    /// hurt-flash timer counts down on every tracked entity.
    pub fn tick_entities(&mut self) {
        self.combat.advance_ticks(1);
        self.world_border.tick();
        for entity in self.entities.values_mut() {
            entity.hurt_ticks = entity.hurt_ticks.saturating_sub(1);
        }
        self.entity_animation_ticks.retain(|_, ticks| {
            *ticks = ticks.saturating_sub(1);
            *ticks != 0
        });
        self.mob_effects.retain(|_, effect| {
            if effect.duration > 0 {
                effect.duration -= 1;
            }
            effect.duration != 0
        });
    }
}

fn read_nibble(bytes: &[u8; LIGHT_BYTES], idx: usize) -> u8 {
    (bytes[idx / 2] >> ((idx & 1) * 4)) & 0x0f
}

fn section_light_updates(
    light: &DecodedLightData,
    light_section: usize,
    sky_cursor: &mut usize,
    block_cursor: &mut usize,
) -> (Option<[u8; LIGHT_BYTES]>, Option<[u8; LIGHT_BYTES]>) {
    let sky_val = if bitset_get(&light.sky_mask, light_section) {
        let arr = light.sky_arrays.get(*sky_cursor).copied();
        *sky_cursor += 1;
        arr
    } else if bitset_get(&light.empty_sky_mask, light_section) {
        Some([0u8; LIGHT_BYTES])
    } else {
        None
    };

    let block_val = if bitset_get(&light.block_mask, light_section) {
        let arr = light.block_arrays.get(*block_cursor).copied();
        *block_cursor += 1;
        arr
    } else if bitset_get(&light.empty_block_mask, light_section) {
        Some([0u8; LIGHT_BYTES])
    } else {
        None
    };
    (sky_val, block_val)
}

// ---------------------------------------------------------------------------
// Vanilla 1.21.1 name-tag easter eggs (entity visuals)
// ---------------------------------------------------------------------------

/// Visual overrides a 1.21.1 client derives from an entity's exact custom
/// name: `LivingEntityRenderer.isEntityUpsideDown` flips "Dinnerbone"/"Grumm"
/// models, `Sheep` fades a "jeb_" sheep's wool through the dye palette, and a
/// "Toast" rabbit swaps to the memorial black-and-white texture.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct EntityEasterEggVisual {
    /// Render the entity upside down ("Dinnerbone" or "Grumm").
    pub upside_down: bool,
    /// Per-frame wool tint for a "jeb_" sheep, `None` for normal dye color.
    pub rainbow_sheep_rgb: Option<[f32; 3]>,
    /// Use the Toast rabbit texture ("Toast" on a rabbit only).
    pub toast_rabbit: bool,
}

/// `DyeColor` texture-diffuse colors in declaration order — the exact wool
/// palette `Sheep.getFleeceColor` cycles for a jeb_ sheep (0xRRGGBB).
const SHEEP_WOOL_RGB: [[u8; 3]; 16] = [
    [0xf9, 0xff, 0xfe], // white
    [0xf9, 0x80, 0x1d], // orange
    [0xc7, 0x4e, 0xbd], // magenta
    [0x3a, 0xb3, 0xda], // light_blue
    [0xfe, 0xd8, 0x3d], // yellow
    [0x80, 0xc7, 0x1f], // lime
    [0xf3, 0x8b, 0xaa], // pink
    [0x47, 0x4f, 0x52], // gray
    [0x9d, 0x9d, 0x97], // light_gray
    [0x16, 0x9c, 0x9c], // cyan
    [0x89, 0x32, 0xb8], // purple
    [0x3c, 0x44, 0xaa], // blue
    [0x83, 0x54, 0x32], // brown
    [0x5e, 0x7c, 0x16], // green
    [0xb0, 0x2e, 0x26], // red
    [0x1d, 0x1d, 0x21], // black
];

/// One 25-tick color per palette slot, lerping into the next — the vanilla
/// `getFleeceColor` fade `(tickCount % 25 + partialTick) / 25`.
fn jeb_wool_rgb(world_tick: u64) -> [f32; 3] {
    let phase = ((world_tick / 25) % 16) as usize;
    let next = (phase + 1) % SHEEP_WOOL_RGB.len();
    let fade = (world_tick % 25) as f32 / 25.0;
    let from = SHEEP_WOOL_RGB[phase].map(f32::from);
    let to = SHEEP_WOOL_RGB[next].map(f32::from);
    [
        (from[0] * (1.0 - fade) + to[0] * fade) / 255.0,
        (from[1] * (1.0 - fade) + to[1] * fade) / 255.0,
        (from[2] * (1.0 - fade) + to[2] * fade) / 255.0,
    ]
}

/// Resolve the vanilla name-tag easter eggs for one entity. `entity_type`
/// accepts `minecraft:name` or the bare name, and `custom_name` is the exact
/// name-tag string: every vanilla comparison is case-sensitive, so
/// "dinnerbone"/"JEB_"/`None` produce no visual.
pub fn evaluate_entity_easter_egg(
    entity_type: &str,
    custom_name: Option<&str>,
    world_tick: u64,
) -> EntityEasterEggVisual {
    let mut visual = EntityEasterEggVisual::default();
    let Some(name) = custom_name else {
        return visual;
    };
    // isEntityUpsideDown ignores the entity type entirely.
    if name == "Dinnerbone" || name == "Grumm" {
        visual.upside_down = true;
    }
    match (
        entity_type
            .strip_prefix("minecraft:")
            .unwrap_or(entity_type),
        name,
    ) {
        ("sheep", "jeb_") => visual.rainbow_sheep_rgb = Some(jeb_wool_rgb(world_tick)),
        ("rabbit", "Toast") => visual.toast_rabbit = true,
        _ => {}
    }
    visual
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn map_patches_preserve_pixels_and_optional_decorations() {
        let mut world = WorldState::new(DimensionMeta::default(), BlockTable::load(None));
        world
            .apply_play_event(&PlayEvent::MapItemData {
                map_id: 19,
                scale: 2,
                locked: false,
                icons: Some(vec![MapItemIcon {
                    kind: 0,
                    x: -2,
                    z: 4,
                    direction: 12,
                    name_nbt: None,
                }]),
                columns: 2,
                rows: 2,
                x: 126,
                y: 126,
                colors: vec![4, 5, 6, 7],
            })
            .unwrap();
        let before = world.maps[&19].pixels.clone();
        let p = |x, z| z * MAP_SIDE + x;
        assert_eq!(before[p(126, 126)], [89, 125, 39, 255]);
        assert_eq!(before[p(127, 126)], [109, 153, 48, 255]);
        assert_eq!(before[p(126, 127)], [127, 178, 56, 255]);
        assert_eq!(before[p(127, 127)], [67, 94, 29, 255]);
        let patch = |columns, colors, icons| PlayEvent::MapItemData {
            map_id: 19,
            scale: 3,
            locked: true,
            icons,
            columns,
            rows: 1,
            x: 127,
            y: 126,
            colors,
        };
        assert!(
            world
            .apply_play_event(&patch(2, vec![18, 18], Some(vec![])))
                .is_err()
        );
        assert_eq!(
            world.maps[&19].icons.len(),
            1,
            "out-of-bounds patch is atomic"
        );
        world.apply_play_event(&patch(1, vec![18], None)).unwrap();
        let updated = &world.maps[&19];
        assert_eq!(updated.scale, 3);
        assert!(updated.locked);
        assert_eq!(updated.icons[0].direction, 12);
        assert_eq!(updated.pixels[p(127, 126)], [255, 0, 0, 255]);
        assert_eq!(updated.pixels[p(126, 126)], before[p(126, 126)]);
        assert_eq!(updated.pixels[p(127, 127)], before[p(127, 127)]);
        assert_eq!(updated.pixels[0], [0; 4]);
        world
            .apply_play_event(&patch(0, vec![], Some(vec![])))
            .unwrap();
        assert!(world.maps[&19].icons.is_empty());
        assert_eq!(world.maps[&19].pixels[p(127, 126)], [255, 0, 0, 255]);
        assert_eq!(map_pixel(255), [0; 4]);
    }

    #[test]
    fn scoreboard_team_modes_move_members_and_apply_both_collision_rules() {
        let mut world = WorldState::new(DimensionMeta::default(), BlockTable::load(None));
        let info = |collision: &str, visibility: &str, color| {
            Some(Box::new(PlayerTeamInfo {
                display_name_nbt: vec![8, 0, 1, b'A'],
                friendly_flags: 2,
                nametag_visibility: visibility.to_owned(),
                collision_rule: collision.to_owned(),
                color,
                prefix_nbt: vec![8, 0, 1, b'['],
                suffix_nbt: vec![8, 0, 1, b']'],
            }))
        };
        let team = |name: &str, mode, info, members: &[&str]| PlayEvent::SetPlayerTeam {
            name: name.to_owned(),
            mode,
            info,
            entities: members.iter().map(|s| (*s).to_owned()).collect(),
        };
        world
            .apply_play_event(&team(
                "red",
                0,
                info("pushOwnTeam", "hideForOtherTeams", 12),
                &["Steve", "Alex"],
            ))
            .unwrap();
        world
            .apply_play_event(&team("blue", 0, info("always", "never", 9), &["Sam"]))
            .unwrap();
        assert!(world.team_allows_collision("Steve", "Alex"));
        assert!(!world.team_allows_collision("Sam", "Alex"));
        assert!(!world.team_shows_nametag("Alex", "Sam"));
        assert!(world.team_shows_nametag("Alex", "Steve"));
        assert!(world.team_allows_seeing_invisible_member("Alex", "Steve"));
        assert!(!world.team_allows_seeing_invisible_member("Alex", "Sam"));
        assert!(world.team_shows_nametag("Alex", "NoTeam"));
        assert!(!world.team_shows_nametag("Sam", "NoTeam"));
        world
            .apply_play_event(&team(
                "red",
                2,
                info("pushOtherTeams", "hideForOwnTeam", 4),
                &[],
            ))
            .unwrap();
        assert_eq!(world.team_for_member("Alex").unwrap().info.color, 4);
        assert!(!world.team_allows_collision("Steve", "Alex"));
        assert!(world.team_allows_collision("Sam", "Alex"));
        assert!(!world.team_shows_nametag("Alex", "Steve"));
        world
            .apply_play_event(&team("blue", 3, None, &["Alex"]))
            .unwrap();
        world
            .apply_play_event(&team("red", 4, None, &["Alex"]))
            .unwrap();
        assert!(!world.teams["red"].members.contains("Alex"));
        assert!(world.teams["blue"].members.contains("Alex"));
        assert_eq!(world.team_for_member("Alex").unwrap().info.color, 9);
        world
            .apply_play_event(&team("blue", 4, None, &["Alex"]))
            .unwrap();
        assert!(world.team_for_member("Alex").is_none());
        world.apply_play_event(&team("red", 1, None, &[])).unwrap();
        assert!(world.team_for_member("Steve").is_none());
    }

    #[test]
    fn runtime_block_registry_preserves_properties_and_vanilla_tint_classes() {
        let raw = br#"[{
            "name":"grass_block","minStateId":8,"maxStateId":9,
            "boundingBox":"block","transparent":false,
            "states":[{"name":"snowy","type":"bool","num_values":2}]
        },{
            "name":"oak_leaves","minStateId":237,"maxStateId":237,
            "boundingBox":"block","transparent":true,"states":[]
        },{
            "name":"birch_leaves","minStateId":293,"maxStateId":293
        },{
            "name":"spruce_leaves","minStateId":265,"maxStateId":265
        },{
            "name":"cherry_leaves","minStateId":377,"maxStateId":377
        },{
            "name":"azalea_leaves","minStateId":405,"maxStateId":405
        },{
            "name":"lily_pad","minStateId":7271,"maxStateId":7271
        },{
            "name":"vine","minStateId":6837,"maxStateId":6837
        },{
            "name":"dirt","minStateId":10,"maxStateId":10
        }]"#;
        let table = BlockTable::from_blocks_json(raw).unwrap();
        assert_eq!(
            table.state_name(8),
            Some("minecraft:grass_block[snowy=true]")
        );
        assert_eq!(
            table.state_name(9),
            Some("minecraft:grass_block[snowy=false]")
        );
        assert_eq!(table.state_name(237), Some("minecraft:oak_leaves"));
        assert_ne!(table.mesh_block(9).flags & OPAQUE, 0);
        assert_ne!(table.mesh_block(237).flags & CUTOUT, 0);
        let plains_tint = |id| table.mesh_block_for_biome(id, 39).tint;
        assert_eq!(plains_tint(9), 0x79c05a);
        assert_eq!(plains_tint(237), 0x59ae30);
        assert_eq!(plains_tint(6837), 0x59ae30);
        assert_eq!(plains_tint(293), 0x80a755);
        assert_eq!(plains_tint(265), 0x619961);
        assert_eq!(plains_tint(7271), 0x208030);
        assert_eq!(plains_tint(10), 0);
        assert_eq!(plains_tint(377), 0);
        assert_eq!(plains_tint(405), 0);
        assert_eq!(table.mesh_block_for_biome(80, 39).fluid_tint, 0x3f76e4);
        assert_eq!(table.mesh_block_for_biome(80, 53).fluid_tint, 0x617b64);
    }

    #[test]
    fn biome_tint_resolver_samples_optional_rgba_colormap() {
        let mut grass = vec![0; 256 * 256 * 4];
        let mut foliage = vec![0; 256 * 256 * 4];
        // At temperature=0.5/downfall=0.5 the sample is (127,191).
        let offset = (191 * 256 + 127) * 4;
        grass[offset..offset + 4].copy_from_slice(&[0x12, 0x34, 0x56, 0xff]);
        foliage[offset..offset + 4].copy_from_slice(&[0x65, 0x43, 0x21, 0xff]);
        let cmap = BiomeColormap::from_rgba(Some(grass), Some(foliage)).unwrap();
        let mut resolver = BiomeTintResolver::default();
        resolver.set_colormap(cmap);
        assert!(resolver.set_biome_climate(7, 0.5, 0.5));
        assert_eq!(resolver.resolve(7, BiomeTintKind::Grass, 0), 0x123456);
        assert_eq!(resolver.resolve(7, BiomeTintKind::Foliage, 0), 0x654321);
        resolver.set_water_color(7, 0xabcdef);
        assert_eq!(resolver.resolve(7, BiomeTintKind::Water, 0), 0xabcdef);
        assert!(BiomeColormap::from_rgba(Some(vec![0; 4]), None).is_err());
    }

    #[test]
    fn render_shape_classifier_uses_names_and_slab_collision_bounds() {
        let boxes = [
            LocalBox {
                min: [0.0, 0.0, 0.0],
                max: [1.0, 0.5, 1.0],
            },
            LocalBox {
                min: [0.0, 0.5, 0.0],
                max: [1.0, 1.0, 1.0],
            },
            LocalBox {
                min: [0.0; 3],
                max: [1.0; 3],
            },
        ];
        assert_eq!(
            classify_render_shape("oak_slab", &boxes, &[0]),
            BlockShape::SlabBottom
        );
        assert_eq!(
            classify_render_shape("oak_slab", &boxes, &[1]),
            BlockShape::SlabTop
        );
        assert_eq!(
            classify_render_shape("oak_slab", &boxes, &[2]),
            BlockShape::Cube
        );
        assert_eq!(
            classify_render_shape("poppy", &[], &[]),
            BlockShape::CrossPlant
        );
        assert_eq!(
            classify_render_shape("oak_door", &[], &[]),
            BlockShape::DoorPanel
        );
        assert_eq!(
            classify_render_shape("redstone_wire", &[], &[]),
            BlockShape::ThinLayer
        );
    }

    #[test]
    fn stored_section_applies_biome_tints_per_quart_cell() {
        let mut pixels = vec![0; 256 * 256 * 4];
        pixels[..4].copy_from_slice(&[0x11, 0x22, 0x33, 0xff]);
        let last = 256 * 256 * 4 - 4;
        pixels[last..].copy_from_slice(&[0xaa, 0xbb, 0xcc, 0xff]);
        let colormap = BiomeColormap::from_rgba(Some(pixels), None).unwrap();
        let mut resolver = BiomeTintResolver::default();
        resolver.set_colormap(colormap);
        // Temperature 1/downfall 1 samples colormap (0, 0); temperature
        // 0/downfall 0 samples (255, 255).
        assert!(resolver.set_biome_climate(1, 1.0, 1.0));
        assert!(resolver.set_biome_climate(2, 0.0, 0.0));
        let blocks = BlockTable {
            mesh_blocks: vec![Block {
                state_id: 0,
                flags: CUTOUT | TINTED,
                tint: 0x7cbd6b,
                ..Block::AIR
            }],
            shapes: vec![],
            collision_shapes: vec![vec![]],
            fluid_cells: vec![FluidCell::default()],
            air_mask: vec![false],
            state_names: vec!["minecraft:short_grass".to_string()],
            biome_tints: resolver,
            dynamic_states: BTreeMap::new(),
            dynamic_state_limit: 0,
            acoustic_lut: crate::spatial_audio::AcousticMaterialLut::from_state_names(&[]),
            loaded_from_json: false,
            source_path: None,
        };
        let states = [0; SECTION_CELLS];
        let mut dynamic = BTreeMap::new();
        let mut stored = StoredSection::from_states(
            &states,
            [0; LIGHT_BYTES],
            [0; LIGHT_BYTES],
            &blocks,
            1,
            &mut dynamic,
        )
        .unwrap();
        stored.biomes[0] = 1;
        stored.biomes[1] = 2;
        let mesh = stored.to_mesh_section(&blocks, &dynamic);
        assert_eq!(mesh.blocks[0].tint, 0x112233);
        assert_eq!(mesh.blocks[4].tint, 0xaabbcc);
    }

    #[test]
    fn biome_messages_and_incremental_edits_refresh_mesh_tints() {
        let mut blocks = BlockTable::builtin();
        blocks.mesh_blocks[1] = Block {
            flags: OPAQUE | FLUID_TINTED,
            fluid_tint: 0xabcdef,
            ..Block::opaque_cube(1)
        };
        blocks.biome_tints.set_water_color(1, 0x112233);
        blocks.biome_tints.set_water_color(2, 0x445566);
        let states = [1; SECTION_CELLS];
        let mut dynamic = BTreeMap::new();
        let mut section = StoredSection::from_states(
            &states,
            [0; LIGHT_BYTES],
            [0; LIGHT_BYTES],
            &blocks,
            blocks.state_count(),
            &mut dynamic,
        )
        .unwrap();
        section.biomes.fill(1);
        let mut world = WorldState::new(DimensionMeta::default(), blocks);
        let coord = SectionCoord(0, world.dimension.min_section_y(), 0);
        world.mesh_cache.insert_compressed(
            coord,
            section.to_compressed_mesh_section(&world.blocks, &dynamic),
        );
        world.columns.insert(
            (0, 0),
            ChunkColumn {
                cx: 0,
                cz: 0,
                sections: vec![section],
            },
        );
        let pos = [1, world.dimension.min_y, 1];

        world.apply_block_update(pos, 0).unwrap();
        world.apply_block_update(pos, 1).unwrap();
        let tint = |world: &WorldState| {
            world
                .mesh_cache
                .section(coord)
                .unwrap()
                .get(1, 0, 1)
                .block
                .fluid_tint
        };
        assert_eq!(tint(&world), 0x112233);
        assert_eq!(world.mesh_cache.rebuild_dirty(1), vec![coord]);

        world
            .apply_play_event(&PlayEvent::ChunksBiomes {
                chunks: vec![ChunkBiomeData {
                    cx: 0,
                    cz: 0,
                    data: vec![0, 2, 0],
                }],
            })
            .unwrap();
        assert_eq!(tint(&world), 0x445566);
        assert_eq!(world.mesh_cache.rebuild_dirty(1), vec![coord]);
    }

    #[test]
    fn block_entity_raw_and_visual_entries_share_lifecycle() {
        let mut blocks = BlockTable::builtin();
        blocks
            .state_names
            .resize(blocks.state_count(), String::new());
        blocks.state_names[1] = "minecraft:oak_sign".into();
        blocks.state_names[2] = "minecraft:oak_sign".into();
        let mut world = WorldState::new(DimensionMeta::default(), blocks);
        let states = [1; SECTION_CELLS];
        let mut dynamic = BTreeMap::new();
        let section = StoredSection::from_states(
            &states,
            [0; LIGHT_BYTES],
            [0; LIGHT_BYTES],
            &world.blocks,
            world.state_count,
            &mut dynamic,
        )
        .unwrap();
        let coord = SectionCoord(0, world.dimension.min_section_y(), 0);
        world.mesh_cache.insert_compressed(
            coord,
            section.to_compressed_mesh_section(&world.blocks, &dynamic),
        );
        world.columns.insert(
            (0, 0),
            ChunkColumn {
                cx: 0,
                cz: 0,
                sections: vec![section],
            },
        );
        let pos = [1, world.dimension.min_y, 1];

        for n in 0..MAX_TRACKED_BLOCK_ENTITIES - 1 {
            let distant = [16 + n as i32, pos[1], 0];
            world.apply_block_entity_data(distant, 7, &[10, 0]);
        }
        world.apply_block_entity_data(pos, 7, &[10, 0]);
        world.apply_block_entity_data([0, pos[1], 0], 7, &[10, 0]);
        assert_eq!(world.block_entities.len(), MAX_TRACKED_BLOCK_ENTITIES);
        assert_eq!(
            world.block_entity_visual_nbt.len(),
            MAX_TRACKED_BLOCK_ENTITIES
        );
        assert!(!world.block_entity_visual_nbt.contains_key(&[0, pos[1], 0]));

        world.apply_block_update(pos, 2).unwrap(); // same block, different state
        assert!(world.block_entities.contains_key(&pos));
        assert!(world.block_entity_visual_nbt.contains_key(&pos));
        world.apply_block_update(pos, 0).unwrap(); // removed
        assert!(!world.block_entities.contains_key(&pos));
        assert!(!world.block_entity_visual_nbt.contains_key(&pos));
    }

    #[test]
    fn dinnerbone_and_grumm_flip_any_entity_type() {
        for name in ["Dinnerbone", "Grumm"] {
            for ty in [
                "minecraft:sheep",
                "sheep",
                "minecraft:rabbit",
                "minecraft:vindicator",
                "zombie",
                "minecraft:player",
            ] {
                let visual = evaluate_entity_easter_egg(ty, Some(name), 0);
                assert!(visual.upside_down, "{ty} named {name}");
                assert!(visual.rainbow_sheep_rgb.is_none());
                assert!(!visual.toast_rabbit);
            }
        }
    }

    #[test]
    fn jeb_sheep_fades_smoothly_through_the_wool_palette() {
        let first = evaluate_entity_easter_egg("minecraft:sheep", Some("jeb_"), 0)
            .rainbow_sheep_rgb
            .expect("jeb_ sheep tints its wool");
        let white = [
            f32::from(0xf9u8) / 255.0,
            f32::from(0xffu8) / 255.0,
            f32::from(0xfeu8) / 255.0,
        ];
        for (got, want) in first.iter().zip(white) {
            assert!((got - want).abs() < 1e-6);
        }
        // At each 25-tick boundary the fade lands exactly on a dye color.
        let orange = [
            f32::from(0xf9u8) / 255.0,
            f32::from(0x80u8) / 255.0,
            f32::from(0x1du8) / 255.0,
        ];
        let tick25 = evaluate_entity_easter_egg("sheep", Some("jeb_"), 25)
            .rainbow_sheep_rgb
            .unwrap();
        for (got, want) in tick25.iter().zip(orange) {
            assert!((got - want).abs() < 1e-6);
        }
        // Two full 16-color cycles: finite RGB inside [0, 1] at every tick.
        let mut last = first;
        let mut changed = 0u32;
        for tick in 0..800 {
            let rgb = evaluate_entity_easter_egg("minecraft:sheep", Some("jeb_"), tick)
                .rainbow_sheep_rgb
                .unwrap();
            assert!(rgb.iter().all(|v| v.is_finite() && (0.0..=1.0).contains(v)));
            if rgb != last {
                changed += 1;
                last = rgb;
            }
        }
        assert!(changed > 0, "the jeb_ tint cycles over time");
        // Mid-cycle ticks interpolate instead of hard-switching colors.
        let mid = evaluate_entity_easter_egg("sheep", Some("jeb_"), 12)
            .rainbow_sheep_rgb
            .unwrap();
        assert_ne!(mid, first);
        assert_ne!(mid, tick25);
    }

    #[test]
    fn toast_texture_is_rabbit_only() {
        for ty in ["minecraft:rabbit", "rabbit"] {
            let visual = evaluate_entity_easter_egg(ty, Some("Toast"), 0);
            assert!(visual.toast_rabbit);
            assert!(!visual.upside_down);
            assert!(visual.rainbow_sheep_rgb.is_none());
        }
        assert!(!evaluate_entity_easter_egg("minecraft:sheep", Some("Toast"), 0).toast_rabbit);
        assert!(!evaluate_entity_easter_egg("minecraft:chicken", Some("Toast"), 0).toast_rabbit);
    }

    #[test]
    fn case_sensitive_names_and_mismatched_types_give_no_visual() {
        for (ty, name) in [
            ("minecraft:sheep", "JEB_"),
            ("minecraft:sheep", "jeb"),
            ("minecraft:sheep", "dinnerbone"),
            ("minecraft:sheep", " Dinnerbone"),
            ("minecraft:sheep", "Dinnerbone "),
            ("minecraft:rabbit", "toast"),
            ("minecraft:zombie", "jeb_"),
            ("minecraft:rabbit", "jeb_"),
            ("other:sheep", "jeb_"),
            ("minecraft:sheep", ""),
        ] {
            let visual = evaluate_entity_easter_egg(ty, Some(name), 0);
            assert!(!visual.upside_down, "{ty} named {name}");
            assert!(visual.rainbow_sheep_rgb.is_none());
            assert!(!visual.toast_rabbit);
        }
        let visual = evaluate_entity_easter_egg("minecraft:sheep", None, 0);
        assert!(!visual.upside_down);
        assert!(visual.rainbow_sheep_rgb.is_none());
        assert!(!visual.toast_rabbit);
    }

    /// `explode` destruction records are byte offsets from
    /// `floor(center)`: each resolves to an absolute position and is
    /// written as air through the same incremental path `block_update`
    /// uses — a loaded chunk really loses the blocks.
    #[test]
    fn explosion_writes_air_into_loaded_chunks() {
        use crate::protocol::DecodedSection;
        let mut world = WorldState::new(
            DimensionMeta {
                name: "minecraft:overworld".to_string(),
                min_y: 0,
                height: 64,
                has_skylight: true,
            },
            BlockTable::builtin(),
        );
        world
            .apply_chunk(DecodedChunk {
                cx: 0,
                cz: 0,
                sections: (0..4)
                    .map(|_| DecodedSection {
                        non_air: SECTION_CELLS as i16,
                        states: Box::new([1; SECTION_CELLS]),
                        biomes: [0; 64],
                    })
                    .collect(),
                block_entities: Vec::new(),
                light: DecodedLightData {
                    sky_mask: Vec::new(),
                    block_mask: Vec::new(),
                    empty_sky_mask: Vec::new(),
                    empty_block_mask: Vec::new(),
                    sky_arrays: Vec::new(),
                    block_arrays: Vec::new(),
                },
            })
            .unwrap();
        assert_eq!(world.get_block_state(3, 4, 5), Some(1));

        world
            .apply_play_event(&PlayEvent::Explosion {
                x: 4.6,
                y: 4.2,
                z: 4.8,
                power: 4.0,
                // `floor(center)` = (4, 4, 4): records land on
                // (4, 4, 4) and (3, 4, 5).
                records: vec![[0, 0, 0], [-1, 0, 1]],
                knockback: [0.0; 3],
                block_interaction: 1,
                small_particle_id: 0,
                large_particle_id: 0,
                sound_name: None,
                sound_id: None,
            })
            .unwrap();
        assert_eq!(world.get_block_state(4, 4, 4), Some(0));
        assert_eq!(world.get_block_state(3, 4, 5), Some(0));
        assert_eq!(world.get_block_state(1, 1, 1), Some(1));
        assert_eq!(world.block_updates, 2);
    }

    #[test]
    fn twenty_four_section_column_releases_dense_mesh_snapshots_after_meshing() {
        use crate::protocol::DecodedSection;

        let blocks = BlockTable::builtin();
        assert_ne!(blocks.mesh_block(1), blocks.mesh_block(2));
        let mut world = WorldState::new(
            DimensionMeta {
                name: "minecraft:overworld".to_string(),
                min_y: -64,
                height: 384,
                has_skylight: true,
            },
            blocks,
        );
        let sections = (0..24)
            .map(|_| {
                let mut states = Box::new([1; SECTION_CELLS]);
                states[0] = 2;
                DecodedSection {
                    non_air: SECTION_CELLS as i16,
                    states,
                    biomes: [0; 64],
                }
            })
            .collect();
        world
            .apply_chunk(DecodedChunk {
                cx: 0,
                cz: 0,
                sections,
                light: DecodedLightData {
                    sky_mask: Vec::new(),
                    block_mask: Vec::new(),
                    empty_sky_mask: Vec::new(),
                    empty_block_mask: Vec::new(),
                    sky_arrays: Vec::new(),
                    block_arrays: Vec::new(),
                },
                block_entities: Vec::new(),
            })
            .unwrap();

        let before = world.mesh_cache_dense_bytes();
        assert_eq!(before, 24 * 65_536);
        assert_eq!(world.mesh_cache.rebuild_dirty(usize::MAX).len(), 24);
        let after = world.mesh_cache.mesh_cache_blocks_heap_bytes();
        assert_eq!(world.mesh_cache_dense_bytes(), 0);
        assert_eq!(after, 24 * (4096 * 2 + 2 * std::mem::size_of::<Block>()));
        assert_eq!(world.get_block_state(0, -64, 0), Some(2));
        assert_eq!(world.get_block_state(1, -64, 0), Some(1));
    }
    #[test]
    fn dynamic_mod_states_keep_full_u32_identity_and_sparse_light_facts() {
        let dynamic_id = 1_000_042;
        let facts = (14u8 << 4) | 15;
        let mut blocks = BlockTable::builtin();
        assert_eq!(
            blocks.install_dynamic_mod_states(&[(
                dynamic_id,
                "examplemod:lamp".to_string(),
                facts,
            )]),
            1
        );
        assert_eq!(blocks.state_name(dynamic_id), Some("examplemod:lamp"));
        let fact = blocks.dynamic_light_fact(dynamic_id).unwrap();
        assert_eq!((fact.opacity, fact.emission), (15, 14));
        assert!(fact.occlusion.iter().all(|&face| face == 0x0f));

        // Force the kernel palette past its 256-entry indirect path. The
        // dynamic overlay must still preserve the full u32 id even though
        // the legacy direct encoding is only 15 bits wide.
        let mut states = [0u32; SECTION_CELLS];
        for (index, state) in states.iter_mut().enumerate() {
            *state = (index % 300 + 1) as u32;
        }
        states[SECTION_CELLS - 1] = dynamic_id;
        let mut dynamic = BTreeMap::new();
        let section = StoredSection::from_states(
            &states,
            [0; LIGHT_BYTES],
            [0; LIGHT_BYTES],
            &blocks,
            blocks.state_count(),
            &mut dynamic,
        )
        .unwrap();
        assert_eq!(section.state_at(SECTION_CELLS - 1, &dynamic), dynamic_id);
        assert_eq!(
            section.to_mesh_section(&blocks, &dynamic).blocks[SECTION_CELLS - 1].state_id,
            dynamic_id
        );
    }

    #[test]
    fn vanilla_biome_defaults_and_grass_modifiers_are_seeded() {
        let resolver = BiomeTintResolver::default();
        let plains = resolver.resolve(39, BiomeTintKind::Grass, 0);
        let desert = resolver.resolve(14, BiomeTintKind::Grass, 0);
        let jungle = resolver.resolve(28, BiomeTintKind::Grass, 0);
        let swamp = resolver.resolve(53, BiomeTintKind::Grass, 0);
        assert!(plains != 0 && desert != 0 && jungle != 0);
        assert_ne!(plains, desert);
        assert_ne!(plains, jungle);
        assert_ne!(desert, jungle);
        assert_eq!(swamp, 0x6a7039);
        assert_eq!(resolver.resolve(53, BiomeTintKind::Water, 0), 0x617b64);
        let dark_forest = resolver.resolve(8, BiomeTintKind::Grass, 0);
        let forest = resolver.resolve(21, BiomeTintKind::Grass, 0);
        assert_ne!(dark_forest, forest);
    }
}

#[cfg(test)]
mod survival_wiring_tests {
    use super::*;
    use crate::protocol::AwardStatEntry;

    #[test]
    fn survival_packets_update_live_world_state() {
        let mut world =
            WorldState::new_without_registry(DimensionMeta::default(), BlockTable::builtin());
        world
            .apply_play_event(&PlayEvent::InitializeBorder {
                x: 0.0,
                z: 0.0,
                old_diameter: 100.0,
                new_diameter: 200.0,
                lerp_ms: 1000,
                portal_boundary: 29_999_984,
                warning_blocks: 10,
                warning_time: 15,
            })
            .unwrap();
        assert_eq!(world.world_border.warning_blocks, 10);
        assert_eq!(world.world_border.lerp_ticks_total, 20);
        for _ in 0..10 {
            world.tick_entities();
        }
        assert!((world.world_border.current_diameter - 150.0).abs() < 0.001);
        let (distance, intensity) = world.world_border.warning_at(70.0, 0.0);
        assert!((distance - 5.0).abs() < 0.01);
        assert!((intensity - 0.5).abs() < 0.01);
        for _ in 0..10 {
            world.tick_entities();
        }
        assert_eq!(world.world_border.current_diameter, 200.0);

        world
            .apply_play_event(&PlayEvent::SetDefaultSpawnPosition {
                pos: [12, 64, -8],
                angle: 0.5,
            })
            .unwrap();
        assert_eq!(world.default_spawn_position, Some(([12, 64, -8], 0.5)));
        world
            .apply_play_event(&PlayEvent::ChangeDifficulty {
                difficulty: 3,
                locked: true,
            })
            .unwrap();
        assert_eq!(world.difficulty, Some((3, true)));
        world
            .apply_play_event(&PlayEvent::TickingState {
                tick_rate: 12.5,
                frozen: true,
            })
            .unwrap();
        world
            .apply_play_event(&PlayEvent::TickingStep { steps: 4 })
            .unwrap();
        assert_eq!(world.server_tick_rate, 12.5);
        assert!(world.server_ticks_frozen);
        assert_eq!(world.server_tick_steps, 4);
        world
            .apply_play_event(&PlayEvent::AwardStats {
                entries: vec![
                    AwardStatEntry {
                        category: 8,
                        statistic: 25,
                        value: 123,
                    },
                    AwardStatEntry {
                        category: 0,
                        statistic: 7,
                        value: 9,
                    },
                ],
            })
            .unwrap();
        assert_eq!(world.statistics.get(&(8, 25)), Some(&123));
        world.apply_play_event(&PlayEvent::ChunkBatchStart).unwrap();
        assert!(world.chunk_batch_active);
        world
            .apply_play_event(&PlayEvent::ChunkBatchFinished(12))
            .unwrap();
        assert!(!world.chunk_batch_active);
        assert_eq!(world.last_chunk_batch_size, 12);
    }

    #[test]
    fn weather_game_events_ramp_rain_and_thunder_instead_of_snapping() {
        let mut world = WorldState::new(DimensionMeta::default(), BlockTable::load(None));
        assert!(!world.is_raining);
        assert_eq!((world.rain_level, world.thunder_level), (0.0, 0.0));

        // START_RAINING raises the flag and leaves the ramp to event 7;
        // vanilla never snaps the level to 1.0 here.
        world
            .apply_play_event(&PlayEvent::GameEvent {
                event: GAME_EVENT_START_RAINING,
                value: 0.0,
            })
            .unwrap();
        assert!(world.is_raining);
        assert_eq!(world.rain_level, 0.0);

        // RAIN_LEVEL_CHANGE drives the level, clamped NaN-safe to 0.0..=1.0.
        let rain_levels = [
            (0.2, 0.2),
            (0.75, 0.75),
            (1.5, 1.0),
            (-0.4, 0.0),
            (f32::NAN, 0.0),
        ];
        for (value, expected) in rain_levels {
            world
                .apply_play_event(&PlayEvent::GameEvent {
                    event: GAME_EVENT_RAIN_LEVEL_CHANGE,
                    value,
                })
                .unwrap();
            assert_eq!(world.rain_level, expected, "RAIN_LEVEL_CHANGE({value})");
        }

        // THUNDER_LEVEL_CHANGE owns the thunder level, same clamping.
        for (value, expected) in [(0.6, 0.6), (2.0, 1.0), (f32::INFINITY, 0.0)] {
            world
                .apply_play_event(&PlayEvent::GameEvent {
                    event: GAME_EVENT_THUNDER_LEVEL_CHANGE,
                    value,
                })
                .unwrap();
            assert_eq!(world.thunder_level, expected, "THUNDER_LEVEL_CHANGE({value})");
        }

        // STOP_RAINING clears the flag only: the level stays where event 7
        // last left it and thunder keeps its own level until event 8 moves
        // it, exactly like vanilla's separate flag/level pair.
        world
            .apply_play_event(&PlayEvent::GameEvent {
                event: GAME_EVENT_RAIN_LEVEL_CHANGE,
                value: 0.4,
            })
            .unwrap();
        let thunder = world.thunder_level;
        world
            .apply_play_event(&PlayEvent::GameEvent {
                event: GAME_EVENT_STOP_RAINING,
                value: 0.0,
            })
            .unwrap();
        assert!(!world.is_raining);
        assert_eq!(world.rain_level, 0.4);
        assert_eq!(world.thunder_level, thunder);

        // A non-weather event (3 = game mode change) touches no weather.
        world
            .apply_play_event(&PlayEvent::GameEvent {
                event: GAME_EVENT_START_RAINING,
                value: 0.0,
            })
            .unwrap();
        world
            .apply_play_event(&PlayEvent::GameEvent { event: 3, value: 2.0 })
            .unwrap();
        assert_eq!(world.game_mode, Some(2));
        assert!(world.is_raining);
        assert_eq!(world.rain_level, 0.0);
        assert_eq!(world.thunder_level, thunder);
    }
}
