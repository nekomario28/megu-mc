// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later

//! Unified opt-in **Vanilla+** block visual pipeline.
//!
//! [`evaluate_block_visual_pipeline`] is the single entry-point the mesher
//! calls for one block occurrence. It composes the four opt-in enhancement
//! packs —
//!
//! * [`crate::classic_ores`]: classic unified ore sprite restyle,
//! * [`crate::vanilla_plus_visuals`]: connected textures, Better Grass/Snow
//!   side coating and recessed 3D block relief,
//! * [`crate::vanilla_plus_3d`]: distance-LOD procedural 3D upgrade meshes for
//!   crops, rails, ladders, torches and crystals,
//! * [`crate::vanilla_plus_nature`]: leaf tint profiles and weather wind sway
//!   classification,
//!
//! into one [`BlockVisualEnhancementResult`]. The pipeline is **opt-in and
//! default OFF**: [`VanillaPlusPipelineConfig::default`] disables every pack,
//! so [`VanillaPlusPipelineConfig::is_all_disabled`] is `true` and the
//! evaluation short-circuits to [`BlockVisualEnhancementResult::VANILLA`] in
//! O(1). The same zero-override result is returned when the caller reports an
//! external resource-pack override — authored content always wins over every
//! built-in enhancement.
//!
//! Everything is pure `std`, deterministic, and reads no wall clock. Every
//! fact the caller cannot supply fails closed toward the vanilla look: an
//! empty ore sprite preserves the original texture, a non-finite distance is
//! treated as infinitely far, and an unrecognised block id simply gets no
//! overrides.

use crate::classic_ores::{
    evaluate_ore_adaptation, ClassicOresConfig, OreAdaptationDecision, OreTextureInput,
    SynthesisPlan, CLASSIC_ORE_DIM,
};
use crate::crop_archetype::{self, CropInput};
use crate::section::Direction;
use crate::vanilla_plus_3d::{
    build_3d_crop_mesh, build_3d_cross_mesh, build_3d_crystal_mesh, build_3d_fixture_mesh,
    build_3d_ladder_mesh, build_3d_moon_mesh, build_3d_ore_relief, build_3d_rail_mesh,
    build_3d_sun_mesh, vanilla_plus_decision, BlockModelHint, Crop3dSpec, CrossLayout,
    CrystalStage, ExtrudedMesh, FixtureKind, MeshUpgradeDecision, RailShape, VanillaPlus3dConfig,
    VanillaPlusCategory,
};
use crate::vanilla_plus_nature::{
    classify_leaf_block, classify_wind_sway_block, LeafParticleStyle, NatureModRegistry,
    VanillaPlusNatureConfig, WindSwayCategory,
};
use crate::vanilla_plus_visuals::{
    build_3d_relief_mesh, classify_ctm_block, evaluate_better_grass, evaluate_better_snow,
    resolve_ctm_tile, BetterSnowResult, CtmNeighbors, CtmTile, ReliefSubject, SideFaceStyle,
    VanillaPlusVisualsConfig,
};

/// Full 16x16 opaque alpha mask used when the caller has no sprite data for a
/// cross-plane upgrade: the extrusion then thickens the full sprite quad.
static FULL_ALPHA_MASK: [u8; 256] = [255; 256];

// ---------------------------------------------------------------------------
// Pipeline configuration
// ---------------------------------------------------------------------------

/// Per-user composition of the four opt-in Vanilla+ enhancement packs.
/// `Default` is fully vanilla: every sub-pack is disabled, so
/// [`Self::is_all_disabled`] returns `true`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct VanillaPlusPipelineConfig {
    /// Vanilla+ 3D: procedural 3D crops, crystals, rails, ladders, torches,
    /// lanterns, chains and iron bars (plus the voxel Sun/Moon), gated by the
    /// pack's distance LOD cutoff.
    pub three_d: VanillaPlus3dConfig,
    /// Classic Ores: unified classic ore sprite restyle with the full ladder
    /// of mod-ore preservation guards.
    pub classic_ores: ClassicOresConfig,
    /// Vanilla+ Visuals: connected textures, Better Grass/Snow side coating
    /// and recessed 3D block relief.
    pub visuals: VanillaPlusVisualsConfig,
    /// Vanilla+ Nature: leaf tint/particle profiles and wind sway
    /// classification.
    pub nature: VanillaPlusNatureConfig,
}

impl VanillaPlusPipelineConfig {
    /// `true` when no pack can alter any output — the master switch of each
    /// gated pack plus every `visuals` toggle. When `true`,
    /// [`evaluate_block_visual_pipeline`] returns
    /// [`BlockVisualEnhancementResult::VANILLA`] in O(1).
    pub fn is_all_disabled(&self) -> bool {
        !self.three_d.enabled
            && !self.classic_ores.enabled
            && !self.nature.enabled
            && !self.visuals.connected_textures
            && !self.visuals.better_grass
            && !self.visuals.better_snow
            && !self.visuals.block_relief_3d
            && !self.visuals.skin_3d_layers
    }
}

// ---------------------------------------------------------------------------
// Per-block input
// ---------------------------------------------------------------------------

/// Facts the pipeline evaluates for one block occurrence. Callers fill in
/// what they know; every field defaults to the fail-closed vanilla value.
#[derive(Clone, Debug)]
pub struct BlockVisualInput<'a> {
    /// Namespaced block id, e.g. `"minecraft:wheat"` or
    /// `"biomesoplenty:maple_leaves"`. Only the path component is matched by
    /// the id classifiers.
    pub block_id: &'a str,
    /// Block/item tags the registry reports (`"c:ores"`,
    /// `"minecraft:leaves"`, `"minecraft:crops"`, ...).
    pub tags: &'a [&'a str],
    /// 8-neighbour coplanar same-type connection mask for the face being
    /// meshed (`n`/`e`/`s`/`w` cardinals plus the gated diagonals).
    pub ctm_neighbors: CtmNeighbors,
    /// Block face currently being meshed; drives the Better Grass side logic.
    pub face: Direction,
    /// Squared camera-to-block distance in blocks. Feeds the 3D-upgrade and
    /// relief LOD cutoffs. Non-finite or negative values fail closed as
    /// infinitely far (flat 2D fallback).
    pub distance_sq: f32,
    /// An enabled external resource pack already overrides this block's look.
    /// When `true` the pipeline yields entirely and returns
    /// [`BlockVisualEnhancementResult::VANILLA`] — user content always wins.
    pub has_external_custom_override: bool,
    /// The active resource-pack stack bakes a custom 3D model for this block.
    /// Honoured by the 3D upgrade and the block relief through their
    /// `respect_custom_resourcepack*` toggles.
    pub has_custom_3d_resourcepack_model: bool,
    /// The resolved model is a flat cross ("X" or `#` planes with no
    /// thickness) — the shape the 3D upgrade knows how to thicken. Lets
    /// modded cross blocks classify generically.
    pub is_cross_model: bool,
    /// The block uses a custom / non-full-cube model (an ore-preservation
    /// guard: custom models keep their original texture).
    pub has_custom_model: bool,
    /// World-space block position, used for the deterministic sub-texel crop
    /// jitter and as the wind sway phase seed.
    pub block_pos: [i32; 3],
    /// Horizontal facing of the block: furnace front, door hinge side,
    /// ladder/crystal attachment direction. `Up`/`Down` fall back inside the
    /// sub-builders.
    pub facing: Direction,
    /// Furnace-family block is currently lit — emits the tagged emissive
    /// ember quad in the relief mesh.
    pub furnace_lit: bool,
    /// Rail block-state shape for the 3D rail upgrade.
    pub rail_shape: RailShape,
    /// Powered/detector/activator rail is currently powered (rail tint 1).
    pub rail_powered: bool,
    /// Crystal growth stage override; `None` derives it from the id.
    pub crystal_stage: Option<CrystalStage>,
    /// Cross-plane layout override; `None` picks the crop-field `#` grid for
    /// crop ids and the diagonal flower cross otherwise.
    pub cross_layout: Option<CrossLayout>,
    /// `16x16` sprite alpha mask for cross extrusion; `None` extrudes the
    /// full sprite quad.
    pub cross_alpha: Option<&'a [u8; 256]>,
    /// Procedural 3D crop spec. `Some` routes eligible `CropsAndFlora`
    /// blocks to [`build_3d_crop_mesh`] instead of the generic cross-plane
    /// extrusion; `None` preserves the existing cross fallback.
    pub crop_3d: Option<Crop3dSpec>,
    /// Automatic mod-crop facts for [`crop_archetype::classify`]. When
    /// `crop_3d` is `None` and this is `Some`, eligible `CropsAndFlora`
    /// blocks are classified from registry tags, blockstate and sprite
    /// features: a confident archetype routes to [`build_3d_crop_mesh`],
    /// while non-crops and ambiguous sprites fail closed to the cross-plane
    /// extrusion. `None` (the default) preserves the existing behaviour.
    pub crop_auto_input: Option<CropInput<'a>>,
    /// `16x16` ore-vein occupancy mask for the 0.5-texel outward relief;
    /// `None` (or a non-ore block) emits no relief geometry.
    pub ore_vein_mask: Option<&'a [u8; 256]>,
    /// 6-bit exposed-face mask ([`Direction::bit`] order) limiting which ore
    /// faces get relief. Defaults to `0x3F` (all faces exposed).
    pub exposed_faces: u8,
    /// Lunar phase `0..=7` (vanilla order) for a `moon` celestial id.
    pub moon_phase: u8,
    /// Ore sprite pixel width; only `16` is restylable.
    pub ore_texture_width: u32,
    /// Ore sprite pixel height (taller than wide = animation strip).
    pub ore_texture_height: u32,
    /// An `.mcmeta` animation descriptor accompanies the ore sprite.
    pub ore_has_mcmeta_animation: bool,
    /// Raw RGBA8 ore sprite pixels (`width * height * 4` bytes). Empty or
    /// short data fails closed into preservation for mod ores, while vanilla
    /// ores still restyle from their curated palette.
    pub ore_texture_rgba: &'a [u8],
    /// Better Grass: the lower diagonal block across `face` is the same
    /// grass-family type (one-block slope).
    pub lower_diagonal_same_type: bool,
    /// Better Grass: the same-level block across `face` occludes the side.
    pub same_level_opaque: bool,
    /// Better Grass: this block carries its `snowy` state.
    pub snowy: bool,
    /// Better Grass: the block across `face` is snow.
    pub adjacent_snow: bool,
    /// Better Snow: this block is a bottom slab.
    pub is_bottom_slab: bool,
    /// Better Snow: the block stands on solid ground.
    pub on_solid_ground: bool,
    /// Better Snow: cardinal neighbours holding snow layers or snow blocks.
    pub snowy_cardinal_neighbors: u8,
}

impl<'a> Default for BlockVisualInput<'a> {
    fn default() -> Self {
        Self {
            block_id: "",
            tags: &[],
            ctm_neighbors: CtmNeighbors::NONE,
            face: Direction::South,
            distance_sq: 0.0,
            has_external_custom_override: false,
            has_custom_3d_resourcepack_model: false,
            is_cross_model: false,
            has_custom_model: false,
            block_pos: [0; 3],
            facing: Direction::South,
            furnace_lit: false,
            rail_shape: RailShape::NorthSouth,
            rail_powered: false,
            crystal_stage: None,
            cross_layout: None,
            cross_alpha: None,
            crop_3d: None,
            crop_auto_input: None,
            ore_vein_mask: None,
            exposed_faces: 0x3F,
            moon_phase: 0,
            ore_texture_width: CLASSIC_ORE_DIM,
            ore_texture_height: CLASSIC_ORE_DIM,
            ore_has_mcmeta_animation: false,
            ore_texture_rgba: &[],
            lower_diagonal_same_type: false,
            same_level_opaque: false,
            snowy: false,
            adjacent_snow: false,
            is_bottom_slab: false,
            on_solid_ground: false,
            snowy_cardinal_neighbors: 0,
        }
    }
}

// ---------------------------------------------------------------------------
// Result
// ---------------------------------------------------------------------------

/// The composed enhancement set for one block occurrence. Every field is an
/// override on top of the vanilla mesh; the all-defaults value
/// ([`Self::VANILLA`], also [`Self::default`]) means "mesh this block exactly
/// like vanilla".
#[derive(Clone, Debug, PartialEq)]
pub struct BlockVisualEnhancementResult {
    /// Classic Ores: synthesized unified-ore sprite plan, or `None` when the
    /// original texture is preserved (any [`crate::classic_ores::PreserveReason`]).
    pub classic_ore: Option<SynthesisPlan>,
    /// Connected-texture tile for the meshed face (`0..=46` canonical index
    /// for `Full47`, or a horizontal/vertical trim tile), or `None` for
    /// vanilla UVs.
    pub ctm_tile: Option<CtmTile>,
    /// Better Grass / snowy side-face coating; `SideFaceStyle::VanillaSide`
    /// is no override.
    pub side_face: SideFaceStyle,
    /// Better Snow: thin snow infill layer under a partial block.
    pub better_snow: BetterSnowResult,
    /// Recessed 3D relief overlay mesh (furnace firebox, bookshelf bands,
    /// windowed door bars, jukebox slot), or `None`.
    pub relief_mesh: Option<ExtrudedMesh>,
    /// What the Vanilla+ 3D upgrade decided for this block occurrence —
    /// useful for callers that distinguish `Use2dLodFallback` and
    /// `DeferToResourcePack` from a plain miss.
    pub upgrade_decision: MeshUpgradeDecision,
    /// Procedural 3D upgrade mesh replacing the flat model, present only
    /// when `upgrade_decision` is `Upgrade3d`.
    pub upgrade_mesh: Option<ExtrudedMesh>,
    /// Leaf canopy tint/particle profile for this block, or `None`.
    pub leaf_profile: Option<LeafParticleStyle>,
    /// Wind sway category this block's vertices should use, or `None`.
    pub wind_sway: Option<WindSwayCategory>,
}

impl BlockVisualEnhancementResult {
    /// The clean zero-override result: vanilla meshing, untouched.
    pub const VANILLA: Self = Self {
        classic_ore: None,
        ctm_tile: None,
        side_face: SideFaceStyle::VanillaSide,
        better_snow: BetterSnowResult {
            emit_snow_layer: false,
            layer_height: 0.0,
        },
        relief_mesh: None,
        upgrade_decision: MeshUpgradeDecision::Use2dVanilla,
        upgrade_mesh: None,
        leaf_profile: None,
        wind_sway: None,
    };

    /// `true` when the result carries no override of any kind and the block
    /// meshes exactly like vanilla.
    pub fn is_vanilla(&self) -> bool {
        *self == Self::VANILLA
    }
}

impl Default for BlockVisualEnhancementResult {
    fn default() -> Self {
        Self::VANILLA
    }
}

// ---------------------------------------------------------------------------
// Evaluation
// ---------------------------------------------------------------------------

/// Evaluates every enabled Vanilla+ pack for one block occurrence.
///
/// Returns [`BlockVisualEnhancementResult::VANILLA`] in O(1) when
/// `cfg.is_all_disabled()` or `input.has_external_custom_override` is set.
/// Otherwise it composes, in order:
///
/// 1. **Classic Ores** — texture-level restyle: `classic_ore` holds
///    the synthesis plan for a standard vanilla/mod ore; every odd input
///    (animated, non-16x16, custom model, denylisted, unique art) preserves.
/// 2. **Vanilla+ Visuals** — `ctm_tile` for glass/bookshelf/sandstone
///    families, `side_face` for grass-family slopes, `better_snow` infill,
///    and `relief_mesh` for furnaces, bookshelves, windowed doors/trapdoors
///    and jukeboxes within the relief LOD cutoff.
/// 3. **Vanilla+ 3D** — `upgrade_mesh` (crops, crystals, rails, ladders,
///    torches, lanterns, chains, iron bars, voxel Sun/Moon) gated by the
///    pack's distance LOD cutoff; `upgrade_decision` reports the outcome.
/// 4. **Vanilla+ Nature** — `leaf_profile` canopy tint (under
///    `nature_ambience`) and `wind_sway` classification (under
///    `gentle_wind_sway`).
pub fn evaluate_block_visual_pipeline(
    input: &BlockVisualInput,
    cfg: &VanillaPlusPipelineConfig,
    registry: &NatureModRegistry,
) -> BlockVisualEnhancementResult {
    // O(1) fast path: nothing enabled, or authored content always wins.
    if cfg.is_all_disabled() || input.has_external_custom_override {
        return BlockVisualEnhancementResult::VANILLA;
    }

    // Both LOD cutoffs compare in block units; a bad distance fails far.
    let distance_blocks = if input.distance_sq.is_finite() && input.distance_sq >= 0.0 {
        input.distance_sq.sqrt()
    } else {
        f32::INFINITY
    };

    let mut out = BlockVisualEnhancementResult::VANILLA;

    // 1. Classic Ores: a texture-level restyle, so the mesher only swaps the
    //    sprite. The cheap id/tag prefilter keeps the per-block tag Vec out
    //    of the hot path for the 99% of blocks that can never be ores.
    if cfg.classic_ores.enabled && could_be_ore(input.block_id, input.tags) {
        let ore = OreTextureInput {
            block_id: input.block_id,
            tags: input.tags.iter().map(|t| (*t).to_string()).collect(),
            width: input.ore_texture_width,
            height: input.ore_texture_height,
            has_mcmeta_animation: input.ore_has_mcmeta_animation,
            has_custom_model: input.has_custom_model,
            has_external_pack_override: input.has_external_custom_override,
            rgba: input.ore_texture_rgba,
        };
        if let OreAdaptationDecision::SynthesizeClassic(plan) =
            evaluate_ore_adaptation(&cfg.classic_ores, &ore)
        {
            out.classic_ore = Some(plan);
        }
    }

    // 2. Vanilla+ Visuals: connected textures, side coating, snow infill and
    //    recessed block relief — all zero-vertex swaps except the relief,
    //    which only ever carves quads *inside* the vanilla cube.
    let visuals = &cfg.visuals;
    if visuals.connected_textures {
        if let Some(mode) = classify_ctm_block(input.block_id) {
            out.ctm_tile = Some(resolve_ctm_tile(mode, &input.ctm_neighbors));
        }
    }
    if visuals.better_grass {
        out.side_face = evaluate_better_grass(
            visuals,
            input.block_id,
            input.face,
            input.lower_diagonal_same_type,
            input.same_level_opaque,
            input.snowy,
            input.adjacent_snow,
        );
    }
    if visuals.better_snow {
        out.better_snow = evaluate_better_snow(
            visuals,
            input.block_id,
            input.is_bottom_slab,
            input.on_solid_ground,
            input.snowy_cardinal_neighbors,
        );
    }
    if visuals.block_relief_3d {
        if let Some(subject) = classify_relief_subject(input) {
            out.relief_mesh = build_3d_relief_mesh(
                visuals,
                subject,
                input.has_custom_3d_resourcepack_model,
                distance_blocks,
            );
        }
    }

    // 3. Vanilla+ 3D: the procedural upgrade mesh, gated by the pack's
    //    resource-pack deference and distance LOD.
    let three_d = &cfg.three_d;
    if three_d.enabled {
        let hint = BlockModelHint {
            has_custom_3d_resourcepack_model: input.has_custom_3d_resourcepack_model,
            is_cross_model: input.is_cross_model,
        };
        out.upgrade_decision =
            vanilla_plus_decision(three_d, input.block_id, &hint, distance_blocks);
        if let MeshUpgradeDecision::Upgrade3d(category) = out.upgrade_decision {
            let mesh = build_upgrade_mesh(input, category);
            // An upgrade that emits zero quads (an ore without a vein mask,
            // a fully transparent cross sprite) is indistinguishable from
            // vanilla, so it reports no override and the result stays
            // `is_vanilla()`.
            if mesh.quads.is_empty() {
                out.upgrade_decision = MeshUpgradeDecision::Use2dVanilla;
            } else {
                out.upgrade_mesh = Some(mesh);
            }
        }
    }

    // 4. Vanilla+ Nature: canopy tint profile and sway class, each honouring
    //    its own sub-toggle under the pack's master switch.
    let nature = &cfg.nature;
    if nature.enabled {
        if nature.nature_ambience {
            out.leaf_profile = classify_leaf_block(input.block_id, input.tags, registry);
        }
        if nature.gentle_wind_sway {
            out.wind_sway = classify_wind_sway_block(input.block_id, input.tags, registry);
        }
    }

    out
}

// ---------------------------------------------------------------------------
// Local classification helpers
// ---------------------------------------------------------------------------

fn block_path(block_id: &str) -> &str {
    block_id.rsplit(':').next().unwrap_or(block_id)
}

/// Cheap mirror of `classic_ores`' ore test — `*ore*`/`ores`-tagged ids only.
/// Anything this accepts but `evaluate_ore_adaptation` rejects is still
/// preserved; it only exists to skip the `OreTextureInput` allocation for
/// ordinary blocks.
fn could_be_ore(block_id: &str, tags: &[&str]) -> bool {
    if block_path(block_id).contains("ore") {
        return true;
    }
    tags.iter().any(|t| {
        let path = block_path(t);
        path == "ores" || path.ends_with("_ores")
    })
}

/// Maps a block id to its 3D relief subject, or `None` for blocks with no
/// relief. Only ids the relief builder knows are carved; iron doors and
/// trapdoors (no window pane) are left flat.
fn classify_relief_subject(input: &BlockVisualInput) -> Option<ReliefSubject> {
    let path = block_path(input.block_id);
    match path {
        "furnace" | "blast_furnace" | "smoker" => Some(ReliefSubject::Furnace {
            lit: input.furnace_lit,
            facing: input.facing,
        }),
        "bookshelf" | "chiseled_bookshelf" => Some(ReliefSubject::Bookshelf),
        "jukebox" | "crafting_table" => Some(ReliefSubject::JukeboxTop),
        _ if path.ends_with("_door") && path != "iron_door" => Some(ReliefSubject::DoorWindow {
            facing: input.facing,
        }),
        _ if path.ends_with("_trapdoor") && path != "iron_trapdoor" => {
            Some(ReliefSubject::TrapdoorWindow {
                facing: input.facing,
            })
        }
        _ => None,
    }
}

/// Dispatches an `Upgrade3d` decision to the matching procedural builder.
fn build_upgrade_mesh(input: &BlockVisualInput, category: VanillaPlusCategory) -> ExtrudedMesh {
    let path = block_path(input.block_id);
    match category {
        VanillaPlusCategory::CropsAndFlora => {
            if let Some(spec) = input.crop_3d {
                build_3d_crop_mesh(&spec)
            } else if let Some(spec) = auto_crop_spec(input) {
                build_3d_crop_mesh(&spec)
            } else {
                let alpha = input.cross_alpha.unwrap_or(&FULL_ALPHA_MASK);
                build_3d_cross_mesh(alpha, cross_layout_for(path, input), input.block_pos)
            }
        }
        VanillaPlusCategory::Ores3dRelief => input
            .ore_vein_mask
            .map(|mask| build_3d_ore_relief(mask, input.exposed_faces))
            .unwrap_or_default(),
        VanillaPlusCategory::CrystalsAndClusters => build_3d_crystal_mesh(
            input.crystal_stage.unwrap_or_else(|| crystal_stage_for(path)),
            input.facing,
        ),
        VanillaPlusCategory::RailsAndLadders => {
            if path == "ladder" {
                build_3d_ladder_mesh(input.facing)
            } else {
                build_3d_rail_mesh(input.rail_shape, input.rail_powered)
            }
        }
        VanillaPlusCategory::FixturesAndChains => build_3d_fixture_mesh(fixture_kind_for(path)),
        VanillaPlusCategory::Celestial3d => {
            if path == "moon" {
                build_3d_moon_mesh(input.moon_phase)
            } else {
                build_3d_sun_mesh()
            }
        }
    }
}

/// Derives a [`Crop3dSpec`] from `input.crop_auto_input` via the
/// deterministic [`crop_archetype::classify`] pass. Returns `Some` only when
/// the crop gate passes *and* an archetype clears the confidence bar —
/// non-crops and ambiguous sprites fail closed so the caller falls back to
/// the generic cross extrusion. The per-block seed is a stable FNV-1a of the
/// block id so neighbouring identical crops still vary their pose.
fn auto_crop_spec(input: &BlockVisualInput) -> Option<Crop3dSpec> {
    let crop_in = input.crop_auto_input.as_ref()?;
    let decision = crop_archetype::classify(crop_in);
    if !decision.is_crop {
        return None;
    }
    let archetype = decision.archetype?;
    Some(Crop3dSpec {
        archetype: archetype.into(),
        stage: decision.stage,
        max_stage: decision.max_stage,
        seed: fnv1a64(input.block_id.as_bytes()),
        block_pos: input.block_pos,
    })
}

/// FNV-1a 64-bit hash over raw bytes — the same offset/prime pair as
/// [`crop_archetype::decision_fingerprint`], applied to the block id.
fn fnv1a64(bytes: &[u8]) -> u64 {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut h = OFFSET;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(PRIME);
    }
    h
}

/// Crop-field `#` grid for farmed crops, the diagonal flower cross for
/// everything else; `input.cross_layout` always wins.
fn cross_layout_for(path: &str, input: &BlockVisualInput) -> CrossLayout {
    if let Some(layout) = input.cross_layout {
        return layout;
    }
    const CROPS: &[&str] = &[
        "wheat",
        "carrots",
        "potatoes",
        "beetroots",
        "nether_wart",
        "melon_stem",
        "pumpkin_stem",
        "attached_melon_stem",
        "attached_pumpkin_stem",
        "sweet_berry_bush",
        "torchflower_crop",
        "pitcher_crop",
    ];
    if CROPS.contains(&path) || path.ends_with("_stem") || path.ends_with("_crop") {
        CrossLayout::HashGrid
    } else {
        CrossLayout::Diagonal
    }
}

/// Crystal stage derived from the id when the caller supplies none:
/// `small_*`/`medium_*`/`large_*` names take their bud stage, everything else
/// builds the full cluster.
fn crystal_stage_for(path: &str) -> CrystalStage {
    if path.contains("small") {
        CrystalStage::SmallBud
    } else if path.contains("medium") {
        CrystalStage::MediumBud
    } else if path.contains("large") {
        CrystalStage::LargeBud
    } else {
        CrystalStage::Cluster
    }
}

/// Fixture shape for the `fixtures_and_chains` category; any `*torch*` id
/// falls through to the torch build.
fn fixture_kind_for(path: &str) -> FixtureKind {
    if path == "iron_bars" {
        FixtureKind::IronBars
    } else if path == "chain" {
        FixtureKind::Chain
    } else if path.contains("lantern") {
        FixtureKind::Lantern
    } else {
        FixtureKind::Torch
    }
}
