// SPDX-License-Identifier: LGPL-3.0-or-later

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

use crate::ber_instancing::{build_ber_mesh, VanillaBerKind, VanillaBerState};
use crate::classic_ores::{
    evaluate_ore_adaptation, ClassicOresConfig, OreAdaptationDecision, OreTextureInput,
    SynthesisPlan, CLASSIC_ORE_DIM,
};
use crate::crop_archetype::{self, CropInput};
use crate::mesh::Layer;
use crate::section::{Block, Direction, SectionView, CUTOUT, OPAQUE};
use crate::vanilla_plus_3d::{
    build_3d_crop_mesh, build_3d_cross_mesh, build_3d_crystal_mesh, build_3d_fixture_mesh,
    build_3d_ladder_mesh, build_3d_moon_mesh, build_3d_ore_relief, build_3d_rail_mesh,
    build_3d_sun_mesh, vanilla_plus_decision, BlockModelHint, Crop3dSpec, CrossLayout,
    CrystalStage, ExtrudedMesh, ExtrudedQuad, FixtureKind, MeshUpgradeDecision, RailShape,
    VanillaPlus3dConfig, VanillaPlusCategory,
};
use crate::vanilla_plus_nature::{
    classify_leaf_block, classify_wind_sway_block, LeafParticleStyle, NatureModRegistry,
    VanillaPlusNatureConfig, WindSwayCategory,
};
use crate::vanilla_plus_visuals::{
    build_3d_relief_mesh, build_3d_skin_layer_part, classify_ctm_block, evaluate_better_grass,
    evaluate_better_snow, resolve_ctm_tile, BetterSnowResult, CtmNeighbors, CtmTile,
    ReliefSubject, SideFaceStyle, SkinLayerResult, SkinOverlayPart, VanillaPlusVisualsConfig,
};
use crate::Quad;

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
        VanillaPlusCategory::FixturesAndChains => {
            let kind = fixture_kind_for(path);
            let mut mesh = build_3d_fixture_mesh(kind);
            // Torch-family ids get the flame-tip emissive split: only the
            // glowing head is tagged, the wooden shaft stays shaded.
            if kind == FixtureKind::Torch {
                if let Some(flame) = torch_flame_kind(path) {
                    tag_torch_flame_tip_quads(&mut mesh, flame);
                }
            }
            mesh
        }
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
        seed: megu_core::perf_kit::fnv1a64(input.block_id.as_bytes()),
        block_pos: input.block_pos,
    })
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

// ---------------------------------------------------------------------------
// Torch flame-tip emissive split
// ---------------------------------------------------------------------------

/// `tint_index` marker stamped on the emissive flame-tip quads of a warm
/// torch upgrade mesh, so a dynamic-light hook can drive the glowing head
/// at full `block_light` while the wooden shaft stays ordinarily shaded.
/// Distinct from [`crate::vanilla_plus_visuals::RELIEF_TINT_EMISSIVE`] so
/// consumers can pick the per-family flame tint.
pub const TORCH_FLAME_WARM_TINT_INDEX: i32 = 80;
/// `tint_index` marker for soul-torch cyan flame-tip quads.
pub const TORCH_FLAME_SOUL_TINT_INDEX: i32 = 81;
/// `tint_index` marker for redstone-torch ember flame-tip quads.
pub const TORCH_FLAME_REDSTONE_TINT_INDEX: i32 = 82;

/// Full vanilla emissive brightness carried by the tagged flame-tip quads:
/// only the glowing head radiates; the shaft keeps ambient shading.
pub const TORCH_FLAME_BLOCK_LIGHT: u8 = 15;

/// Torch flame family for the emissive tip marking — warm coal flame,
/// soul cyan, or redstone ember.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TorchFlameKind {
    /// Ordinary torch / wall torch: warm incandescent flame.
    Warm,
    /// Soul torch: quiet cyan-blue flame.
    Soul,
    /// Redstone torch: deep ember-red tip.
    Redstone,
}

impl TorchFlameKind {
    /// `tint_index` marker stamped on the flame-tip quads by
    /// [`tag_torch_flame_tip_quads`].
    pub const fn tint_index(self) -> i32 {
        match self {
            Self::Warm => TORCH_FLAME_WARM_TINT_INDEX,
            Self::Soul => TORCH_FLAME_SOUL_TINT_INDEX,
            Self::Redstone => TORCH_FLAME_REDSTONE_TINT_INDEX,
        }
    }

    /// Linear-space RGB highlight tint of the flame tip in `0..=1`.
    pub const fn flame_rgb(self) -> [f32; 3] {
        match self {
            Self::Warm => [1.0, 0.58, 0.20],
            Self::Soul => [0.35, 0.85, 0.90],
            Self::Redstone => [1.0, 0.22, 0.10],
        }
    }
}

/// Inverse of [`TorchFlameKind::tint_index`]: the flame family a stamped
/// `tint_index` marks, or `None` for ordinary tags.
pub fn torch_flame_tint_kind(tint_index: i32) -> Option<TorchFlameKind> {
    [
        TorchFlameKind::Warm,
        TorchFlameKind::Soul,
        TorchFlameKind::Redstone,
    ]
    .into_iter()
    .find(|kind| kind.tint_index() == tint_index)
}

/// Classifies a torch-family block id (path only, namespace stripped) into
/// its flame family: `soul*torch*` → [`TorchFlameKind::Soul`],
/// `redstone*torch*` → [`TorchFlameKind::Redstone`], every other `*torch`
/// → [`TorchFlameKind::Warm`]. Non-torch ids return `None` — lanterns are
/// meshed as full glowing bodies and need no tip split.
pub fn torch_flame_kind(block_id: &str) -> Option<TorchFlameKind> {
    let path = block_path(block_id);
    if path != "torch" && !path.ends_with("_torch") {
        return None;
    }
    Some(if path.contains("soul") {
        TorchFlameKind::Soul
    } else if path.contains("redstone") {
        TorchFlameKind::Redstone
    } else {
        TorchFlameKind::Warm
    })
}

/// Whether one quad of a torch [`ExtrudedMesh`] belongs to the glowing
/// flame head rather than the wooden stick shaft. The fixture builder
/// tags the slightly wider head box `tint_index = 0` (shaft `-1`), and an
/// already-stamped flame marker also counts; for untagged meshes a
/// geometric fallback detects the head: every vertex sits above the
/// `10/16` stick top region and the quad's footprint reaches wider than
/// the `2/16` stick cross-section.
pub fn is_torch_flame_tip_quad(quad: &ExtrudedQuad) -> bool {
    if quad.tint_index == 0 || torch_flame_tint_kind(quad.tint_index).is_some() {
        return true;
    }
    let mut above_shaft = true;
    let mut wider_than_shaft = false;
    for p in quad.positions {
        above_shaft &= p[1] > 0.60;
        wider_than_shaft |= (p[0] - 0.5).abs() > 0.09 || (p[2] - 0.5).abs() > 0.09;
    }
    above_shaft && wider_than_shaft
}

/// Restamps every flame-tip quad of a torch upgrade mesh with `kind`'s
/// emissive `tint_index` marker (see [`TorchFlameKind::tint_index`]). Only
/// the glowing head is tagged — the handle quads keep their untinted `-1`
/// tag so they render shaded by the surrounding ambient and block light
/// instead of glowing like a neon stick.
pub fn tag_torch_flame_tip_quads(mesh: &mut ExtrudedMesh, kind: TorchFlameKind) {
    for quad in &mut mesh.quads {
        if is_torch_flame_tip_quad(quad) {
            quad.tint_index = kind.tint_index();
        }
    }
}

/// World-space offset within a `[0, 1]^3` block cell to the glowing flame
/// tip of a standing (`wall_facing = None`) or wall-mounted torch
/// (`Some(0)=north, 1=south, 2=west, 3=east`, matching vanilla facing).
///
/// A standing torch burns at the exact centre of its flame head — `[0.5,
/// 0.625, 0.5]`, `10/16` up the stick. A wall torch leans into its mounted
/// wall, so the tip offsets `0.27` blocks toward the wall (the side
/// opposite `facing`, exactly where vanilla spawns the flame particles)
/// and sits at the tilted tip height `y = 0.72`. Horizontal axes are
/// clamped into `[0.1, 0.9]`; an unrecognised facing code falls back to a
/// centred wall-torch tip.
pub fn torch_flame_world_offset(wall_facing: Option<u8>) -> [f32; 3] {
    let Some(facing) = wall_facing else {
        return [0.5, 0.625, 0.5];
    };
    // 0.27 toward the mounted wall = opposite of the vanilla facing.
    let (x, z): (f32, f32) = match facing {
        0 => (0.5, 0.5 + 0.27), // faces north (-z); wall to the south
        1 => (0.5, 0.5 - 0.27), // faces south (+z); wall to the north
        2 => (0.5 + 0.27, 0.5), // faces west (-x); wall to the east
        3 => (0.5 - 0.27, 0.5), // faces east (+x); wall to the west
        _ => (0.5, 0.5),
    };
    [x.clamp(0.1, 0.9), 0.72, z.clamp(0.1, 0.9)]
}

// ---------------------------------------------------------------------------
// Cutout layer side-list
// ---------------------------------------------------------------------------

/// Mesh output for one section: the quad stream plus the cutout side-list —
/// indices into `quads` whose material belongs to the vanilla alpha-tested
/// (cutout) layer, in emission order. The cutout pass draws exactly these
/// indices with alpha testing enabled; everything else renders in the
/// opaque/translucent passes.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SectionMeshOutput {
    /// Every quad the mesher emitted, in stable emission order.
    pub quads: Vec<Quad>,
    /// Indices into `quads` tagged [`Layer::Cutout`] — leaves, glass panes,
    /// farm crops and every other `CUTOUT`-flagged material.
    pub cutout_indices: Vec<u32>,
}

/// The render layer a section [`Block`] meshes into, mirroring the mesher's
/// precedence: `OPAQUE` wins over `CUTOUT`, which wins over `TRANSLUCENT`.
pub fn block_render_layer(block: &Block) -> Layer {
    if block.flags & OPAQUE != 0 {
        Layer::Opaque
    } else if block.flags & CUTOUT != 0 {
        Layer::Cutout
    } else {
        Layer::Translucent
    }
}

/// Indices of every [`Layer::Cutout`] quad in `quads`, in order.
pub fn cutout_quad_indices(quads: &[Quad]) -> Vec<u32> {
    quads
        .iter()
        .enumerate()
        .filter(|(_, quad)| quad.layer == Layer::Cutout)
        .map(|(index, _)| index as u32)
        .collect()
}

/// Mesh `view` like [`crate::mesh_section`] and additionally report which
/// quads belong to the vanilla cutout layer so the renderer can submit them
/// in a separate alpha-tested pass.
pub fn mesh_section_with_cutout(view: &SectionView<'_>) -> SectionMeshOutput {
    let quads = crate::mesh::mesh_section(view);
    let cutout_indices = cutout_quad_indices(&quads);
    SectionMeshOutput {
        quads,
        cutout_indices,
    }
}

// ---------------------------------------------------------------------------
// Vanilla block-entity instanced meshing
// ---------------------------------------------------------------------------

/// All twelve vanilla BER families in stable dispatch order — the
/// `VanillaBerKind` match arms [`ber_kind_slot`] uses to order batches.
pub const VANILLA_BER_KINDS: [VanillaBerKind; 12] = [
    VanillaBerKind::Chest,
    VanillaBerKind::Sign,
    VanillaBerKind::Banner,
    VanillaBerKind::Beacon,
    VanillaBerKind::Bed,
    VanillaBerKind::Skull,
    VanillaBerKind::ItemFrame,
    VanillaBerKind::Campfire,
    VanillaBerKind::BrewingStand,
    VanillaBerKind::Lectern,
    VanillaBerKind::DecoratedPot,
    VanillaBerKind::ShulkerBox,
];

/// Stable dispatch slot for one [`VanillaBerKind`] — the real 12-arm match
/// every BER batch key derives from.
fn ber_kind_slot(kind: VanillaBerKind) -> usize {
    match kind {
        VanillaBerKind::Chest => 0,
        VanillaBerKind::Sign => 1,
        VanillaBerKind::Banner => 2,
        VanillaBerKind::Beacon => 3,
        VanillaBerKind::Bed => 4,
        VanillaBerKind::Skull => 5,
        VanillaBerKind::ItemFrame => 6,
        VanillaBerKind::Campfire => 7,
        VanillaBerKind::BrewingStand => 8,
        VanillaBerKind::Lectern => 9,
        VanillaBerKind::DecoratedPot => 10,
        VanillaBerKind::ShulkerBox => 11,
    }
}

/// Map a vanilla block-entity type id (`"minecraft:chest"`,
/// `"minecraft:wall_sign"`, ...) to its [`VanillaBerKind`]. Every alias the
/// vanilla registry reports for each renderer family is covered; unknown or
/// modded types fail closed to `None` and are skipped by the collector.
pub fn classify_vanilla_ber(be_type: &str) -> Option<VanillaBerKind> {
    match block_path(be_type) {
        "chest" | "trapped_chest" | "ender_chest" => Some(VanillaBerKind::Chest),
        "sign" | "wall_sign" | "hanging_sign" | "wall_hanging_sign" => {
            Some(VanillaBerKind::Sign)
        }
        "banner" | "standing_banner" | "wall_banner" => Some(VanillaBerKind::Banner),
        "beacon" => Some(VanillaBerKind::Beacon),
        "bed" => Some(VanillaBerKind::Bed),
        "skull" | "wall_skull" | "player_head" | "player_wall_head" => {
            Some(VanillaBerKind::Skull)
        }
        "item_frame" | "glow_item_frame" => Some(VanillaBerKind::ItemFrame),
        "campfire" | "soul_campfire" => Some(VanillaBerKind::Campfire),
        "brewing_stand" => Some(VanillaBerKind::BrewingStand),
        "lectern" => Some(VanillaBerKind::Lectern),
        "decorated_pot" => Some(VanillaBerKind::DecoratedPot),
        "shulker_box" => Some(VanillaBerKind::ShulkerBox),
        _ => None,
    }
}

/// One block entity submitted for instanced BER meshing: its world block
/// position, the [`VanillaBerState`] that fully describes its geometry
/// (facing, lid angle, ...), and the packed light/tint the instance record
/// carries to the GPU.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SectionBerInstance {
    /// Stable id used as the deterministic tiebreak inside a batch.
    pub entity_id: u64,
    /// World block position (the block's min corner) in render-world blocks.
    pub pos: [i32; 3],
    /// Geometry state; `state.kind()` selects the [`VanillaBerKind`].
    pub state: VanillaBerState,
    /// Packed light: block-light nibble | sky-light nibble << 4.
    pub packed_light: u32,
    /// RGBA tint, one byte per channel.
    pub tint_rgba: u32,
}

impl SectionBerInstance {
    /// World-space instance origin in render-world blocks.
    pub fn mesh_position(&self) -> [f32; 3] {
        [
            self.pos[0] as f32,
            self.pos[1] as f32,
            self.pos[2] as f32,
        ]
    }
}

/// One placed instance inside a [`BerKindBatch`]: position plus the light
/// and tint payload the GPU instance record carries.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct BerMeshInstance {
    /// World-space origin in render-world blocks.
    pub pos: [f32; 3],
    /// Packed light at the instance.
    pub packed_light: u32,
    /// RGBA tint, one byte per channel.
    pub tint_rgba: u32,
}

/// One instanced draw batch: every [`BerMeshInstance`] shares the same
/// `(kind, state)`, so one [`ExtrudedMesh`] template built by
/// [`build_ber_mesh`] serves the whole run — the client-port analog of
/// `client_gpu::subworld_render::InstancedPartBatch`.
#[derive(Clone, Debug, PartialEq)]
pub struct BerKindBatch {
    /// Vanilla BER family this batch draws.
    pub kind: VanillaBerKind,
    /// The exact [`VanillaBerState`] `template` was built from; all
    /// instances in the batch share it.
    pub state: VanillaBerState,
    /// Shared template geometry in block-local `0..=1` space.
    pub template: ExtrudedMesh,
    /// World-space placements, in collection order.
    pub instances: Vec<BerMeshInstance>,
}

/// Collects block-entity instances into per-`(kind, state)` instanced
/// batches. Instances with identical [`VanillaBerState`] share one
/// [`build_ber_mesh`] template and one draw call; differing states (a lit
/// and an unlit campfire, say) split into separate batches of the same
/// kind. [`Self::into_batches`] orders the result by
/// [`ber_kind_slot`] so output is deterministic.
#[derive(Default)]
pub struct BerInstanceCollector {
    batches: Vec<BerKindBatch>,
}

impl BerInstanceCollector {
    pub fn new() -> Self {
        Self::default()
    }

    /// Collect `instances` into kind/state batches. An instance whose
    /// `build_ber_mesh` template comes out empty is skipped.
    pub fn collect(&mut self, instances: &[SectionBerInstance]) {
        for instance in instances {
            let kind = instance.state.kind();
            let placement = BerMeshInstance {
                pos: instance.mesh_position(),
                packed_light: instance.packed_light,
                tint_rgba: instance.tint_rgba,
            };
            if let Some(batch) = self
                .batches
                .iter_mut()
                .find(|batch| batch.kind == kind && batch.state == instance.state)
            {
                batch.instances.push(placement);
                continue;
            }
            let template = build_ber_mesh(kind, instance.state);
            if template.quads.is_empty() {
                continue;
            }
            self.batches.push(BerKindBatch {
                kind,
                state: instance.state,
                template,
                instances: vec![placement],
            });
        }
    }

    /// Collect only the instances whose `pos` lies inside the 16³ section
    /// starting at `section_min` (world block coordinates), then mesh them.
    pub fn collect_section(
        &mut self,
        section_min: [i32; 3],
        instances: &[SectionBerInstance],
    ) {
        for instance in instances {
            let inside = (0..3).all(|axis| {
                let delta = instance.pos[axis] as i64 - section_min[axis] as i64;
                (0..16).contains(&delta)
            });
            if inside {
                self.collect(std::slice::from_ref(instance));
            }
        }
    }

    /// The collected batches so far, in collection order.
    pub fn batches(&self) -> &[BerKindBatch] {
        &self.batches
    }

    /// Consume the collector and return the batches ordered by
    /// [`ber_kind_slot`] (stable within a kind), so two collectors fed the
    /// same instances emit byte-identical batch lists.
    pub fn into_batches(mut self) -> Vec<BerKindBatch> {
        self.batches.sort_by_key(|batch| ber_kind_slot(batch.kind));
        self.batches
    }
}

// ---------------------------------------------------------------------------
// 3D player skin outer layers
// ---------------------------------------------------------------------------

/// The six overlay ("second layer") parts of a standard `64x64` player skin,
/// in hat -> jacket -> sleeves -> pants order.
pub const HUMANOID_SKIN_OVERLAY_PARTS: [SkinOverlayPart; 6] = [
    SkinOverlayPart::Hat,
    SkinOverlayPart::Jacket,
    SkinOverlayPart::LeftSleeve,
    SkinOverlayPart::RightSleeve,
    SkinOverlayPart::LeftPants,
    SkinOverlayPart::RightPants,
];

/// Build the extruded 3D outer-layer geometry for every overlay part of a
/// humanoid skin rig. Each part goes through
/// [`build_3d_skin_layer_part`], so the pack toggle and the LOD cutoff gate
/// every part identically: a disabled config, a beyond-cutoff distance, or
/// a malformed skin buffer yields an empty list (the caller keeps the
/// vanilla flat overlay). Slim (3-texel) arms are honoured per part.
pub fn humanoid_skin_layer_parts(
    config: &VanillaPlusVisualsConfig,
    skin_rgba: &[u8],
    slim_arms: bool,
    distance_blocks: f32,
) -> Vec<(SkinOverlayPart, ExtrudedMesh)> {
    HUMANOID_SKIN_OVERLAY_PARTS
        .iter()
        .filter_map(|&part| {
            match build_3d_skin_layer_part(config, skin_rgba, part, slim_arms, distance_blocks)
            {
                SkinLayerResult::Extruded(mesh) => Some((part, mesh)),
                SkinLayerResult::UseFlatOverlay | SkinLayerResult::InvalidSkin => None,
            }
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn enabled_cfg() -> VanillaPlusPipelineConfig {
        let mut cfg = VanillaPlusPipelineConfig::default();
        cfg.three_d.enabled = true;
        cfg
    }

    fn torch_upgrade_mesh(block_id: &str) -> ExtrudedMesh {
        evaluate_block_visual_pipeline(
            &BlockVisualInput {
                block_id,
                distance_sq: 9.0,
                ..Default::default()
            },
            &enabled_cfg(),
            &NatureModRegistry::new(),
        )
        .upgrade_mesh
        .expect("torch upgrade mesh")
    }

    #[test]
    fn standing_torch_flame_tip_sits_at_the_ten_sixteenth_centre() {
        assert_eq!(torch_flame_world_offset(None), [0.5, 0.625, 0.5]);
    }

    #[test]
    fn wall_torch_flame_tip_leans_toward_the_mounted_wall() {
        let standing = torch_flame_world_offset(None);
        let mut seen = Vec::new();
        for facing in 0u8..4 {
            let tip = torch_flame_world_offset(Some(facing));
            seen.push(tip);
            // Tilted tip height, inside the block cell, and distinct from
            // the standing flame origin.
            assert!((tip[1] - 0.72).abs() < 1e-6);
            assert!(tip[0] != 0.5 || tip[2] != 0.5);
            assert_ne!(tip, standing);
            for &c in &[tip[0], tip[1], tip[2]] {
                assert!((0.0..=1.0).contains(&c), "tip escapes the cell: {tip:?}");
            }
            assert!((0.1..=0.9).contains(&tip[0]) && (0.1..=0.9).contains(&tip[2]));
        }
        // Each mounted wall produces a distinct lean direction.
        for (i, a) in seen.iter().enumerate() {
            for b in &seen[i + 1..] {
                assert_ne!(a, b);
            }
        }
        // Vanilla semantics: the tip moves toward the wall, i.e. opposite
        // the torch's facing direction.
        assert!(seen[0][2] > 0.5, "north-facing leans south, toward the wall");
        assert!(seen[1][2] < 0.5, "south-facing leans north, toward the wall");
        assert!(seen[2][0] > 0.5, "west-facing leans east, toward the wall");
        assert!(seen[3][0] < 0.5, "east-facing leans west, toward the wall");
        // An unknown facing code stays inside the cell at the wall height.
        let fallback = torch_flame_world_offset(Some(9));
        assert!((0.1..=0.9).contains(&fallback[0]));
        assert!((0.1..=0.9).contains(&fallback[2]));
        assert!((fallback[1] - 0.72).abs() < 1e-6);
    }

    #[test]
    fn torch_fixture_mesh_separates_six_flame_quads_from_the_shaft() {
        let mesh = build_3d_fixture_mesh(FixtureKind::Torch);
        assert_eq!(mesh.quads.len(), 12);
        let flame: Vec<&ExtrudedQuad> = mesh
            .quads
            .iter()
            .filter(|q| is_torch_flame_tip_quad(q))
            .collect();
        // Exactly one box worth of faces (six) is the glowing head.
        assert_eq!(flame.len(), 6);
        // Every flame vertex floats above the shaft top; every shaft quad
        // has at least one vertex at or below it.
        for quad in flame {
            assert!(quad.positions.iter().all(|p| p[1] > 0.60));
        }
        let shaft: Vec<&ExtrudedQuad> = mesh
            .quads
            .iter()
            .filter(|q| !is_torch_flame_tip_quad(q))
            .collect();
        assert_eq!(shaft.len(), 6);
        for quad in shaft {
            assert!(quad.positions.iter().all(|p| p[0] >= 0.44 - 1e-6
                && p[0] <= 0.56 + 1e-6
                && p[2] >= 0.44 - 1e-6
                && p[2] <= 0.56 + 1e-6));
        }
        // The standing tip offset sits inside the flame head bounds.
        let tip = torch_flame_world_offset(None);
        let (mut lo, mut hi) = ([f32::MAX; 3], [f32::MIN; 3]);
        for quad in mesh.quads.iter().filter(|q| is_torch_flame_tip_quad(q)) {
            for p in quad.positions {
                for axis in 0..3 {
                    lo[axis] = lo[axis].min(p[axis]);
                    hi[axis] = hi[axis].max(p[axis]);
                }
            }
        }
        for axis in 0..3 {
            assert!(tip[axis] >= lo[axis] && tip[axis] <= hi[axis],
                "tip {tip:?} outside head bounds {lo:?}..{hi:?}");
        }
    }

    #[test]
    fn torch_upgrade_mesh_tags_only_the_flame_tip_as_emissive() {
        for (block_id, kind) in [
            ("minecraft:torch", TorchFlameKind::Warm),
            ("minecraft:wall_torch", TorchFlameKind::Warm),
            ("minecraft:soul_torch", TorchFlameKind::Soul),
            ("minecraft:soul_wall_torch", TorchFlameKind::Soul),
            ("minecraft:redstone_torch", TorchFlameKind::Redstone),
            ("minecraft:redstone_wall_torch", TorchFlameKind::Redstone),
        ] {
            let mesh = torch_upgrade_mesh(block_id);
            let tagged = mesh
                .quads
                .iter()
                .filter(|q| q.tint_index == kind.tint_index())
                .count();
            assert_eq!(tagged, 6, "{block_id} must tag exactly the head box");
            // The wooden shaft is never emissive-tagged: it stays shaded.
            let other = mesh
                .quads
                .iter()
                .filter(|q| q.tint_index != kind.tint_index())
                .count();
            assert_eq!(other, 6);
            assert!(mesh
                .quads
                .iter()
                .filter(|q| q.tint_index != kind.tint_index())
                .all(|q| q.tint_index == -1));
            // Every flame quad is recognisable to the light hook, and the
            // family tint round-trips through the marker.
            for quad in &mesh.quads {
                if quad.tint_index == kind.tint_index() {
                    assert!(is_torch_flame_tip_quad(quad));
                    assert_eq!(
                        torch_flame_tint_kind(quad.tint_index),
                        Some(kind)
                    );
                }
            }
        }
        assert_eq!(TORCH_FLAME_BLOCK_LIGHT, 15);
        for kind in [
            TorchFlameKind::Warm,
            TorchFlameKind::Soul,
            TorchFlameKind::Redstone,
        ] {
            assert!(kind.flame_rgb().iter().all(|c| (0.0..=1.0).contains(c)));
        }
        assert!(TorchFlameKind::Soul.flame_rgb()[2] > TorchFlameKind::Warm.flame_rgb()[2]);
    }

    #[test]
    fn torch_flame_kind_only_claims_torch_ids() {
        assert_eq!(
            torch_flame_kind("minecraft:torch"),
            Some(TorchFlameKind::Warm)
        );
        assert_eq!(
            torch_flame_kind("minecraft:lantern"),
            None,
            "lanterns glow as full bodies, not tips"
        );
        assert_eq!(torch_flame_kind("minecraft:torchflower"), None);
        assert_eq!(torch_flame_kind("minecraft:torchflower_crop"), None);
        assert_eq!(torch_flame_kind("minecraft:chain"), None);
        assert_eq!(torch_flame_kind(""), None);
        assert_eq!(
            torch_flame_kind("somemod:netherite_torch"),
            Some(TorchFlameKind::Warm)
        );
    }
}
