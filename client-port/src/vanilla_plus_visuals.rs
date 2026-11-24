// SPDX-License-Identifier: LGPL-3.0-or-later

//! Optional **Vanilla+ Visual Polish Pack** — four universally loved,
//! zero-backlash, ultra-lightweight visual upgrades in one module.
//!
//! The pack is **opt-in and default OFF**: every toggle on
//! [`VanillaPlusVisualsConfig`] defaults to `false`, so nothing about the
//! vanilla look changes unless the user explicitly turns a feature on. All
//! four subsystems automatically defer to custom resource packs
//! (`respect_custom_resourcepack`, default on) and to cheap vanilla paths
//! beyond their distance LOD cutoffs, so FPS stays rock-solid.
//!
//! Subsystems:
//!
//! * **Connected textures (CTM)** — [`resolve_ctm_tile`] resolves the classic
//!   8-neighbor glass topology into a canonical `0..=46` tile index plus a
//!   border-suppress bitmask, and horizontal/vertical trim topologies for
//!   bookshelves and sandstone. Pure UV/tile lookup: zero extra vertices.
//! * **Better Grass / Better Snow** — [`evaluate_better_grass`] promotes a
//!   grass side face to [`SideFaceStyle::FullTopTexture`] on one-block slopes
//!   so terrain flows seamlessly, and [`evaluate_better_snow`] emits a thin
//!   snow layer under fences, flowers and other partial blocks surrounded by
//!   snowfields.
//! * **3D block relief** — [`build_3d_relief_mesh`] carves 1–2 texel recessed
//!   fireboxes into furnaces/smokers/blast furnaces (with a tagged emissive
//!   ember quad when `lit`), recessed book bands into bookshelves, perforated
//!   window bars into doors/trapdoors, and a disc slot into the jukebox top.
//! * **3D skin outer layers** — [`build_3d_skin_layer_part`] extrudes only the
//!   opaque pixels of a standard `64x64` RGBA skin overlay outward half a
//!   texel (greedy-merged front/back plus perimeter walls) for classic and
//!   slim models alike.
//!
//! All geometry is emitted in block-local `0..=1` space (skin parts use limb
//! texel units) with outward counterclockwise quads, unit normals and
//! `0..=1` UVs, reusing the [`crate::vanilla_plus_3d`] [`ExtrudedMesh`]
//! conventions so output can feed the same upload path. Everything is pure
//! `std`, deterministic, reads no wall clock, and references no Mojang or
//! third-party geometry or texture data: all shapes are generated from first
//! principles plus the caller-supplied skin alpha buffer.

use crate::section::Direction;
use crate::vanilla_plus_3d::{
    ExtrudedMesh, ExtrudedQuad, ALPHA_CUTOFF, MAX_UPGRADE_QUADS, TEXEL,
};

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// Default distance in blocks beyond which 3D block relief is skipped.
pub const DEFAULT_RELIEF_LOD_CUTOFF_BLOCKS: f32 = 20.0;
/// Smallest accepted 3D relief LOD cutoff.
pub const MIN_RELIEF_LOD_CUTOFF_BLOCKS: f32 = 6.0;
/// Largest accepted 3D relief LOD cutoff.
pub const MAX_RELIEF_LOD_CUTOFF_BLOCKS: f32 = 48.0;

/// Default distance in blocks beyond which player skin layers render flat.
pub const DEFAULT_SKIN_3D_LOD_CUTOFF_BLOCKS: f32 = 16.0;
/// Smallest accepted skin-layer LOD cutoff.
pub const MIN_SKIN_3D_LOD_CUTOFF_BLOCKS: f32 = 6.0;
/// Largest accepted skin-layer LOD cutoff.
pub const MAX_SKIN_3D_LOD_CUTOFF_BLOCKS: f32 = 32.0;

/// Height of the better-snow infill layer, in blocks (`2/16`, one snow layer).
pub const BETTER_SNOW_LAYER_HEIGHT: f32 = 0.125;

/// `tint_index` tag carried by the recessed ember quad a lit furnace emits,
/// so a dynamic-light hook can give it a warm glow.
pub const RELIEF_TINT_EMISSIVE: i32 = 77;

/// Whether a relief quad is the tagged emissive ember face.
pub fn is_emissive_relief_quad(quad: &ExtrudedQuad) -> bool {
    quad.tint_index == RELIEF_TINT_EMISSIVE
}

/// Per-user configuration for the Vanilla+ visual polish pack. `Default` is
/// fully vanilla: every visual toggle is `false`.
#[derive(Clone, Debug, PartialEq)]
pub struct VanillaPlusVisualsConfig {
    /// Opt-in: connected textures for glass, bookshelves and pillar trims.
    pub connected_textures: bool,
    /// Opt-in: grass/mycelium/podzol/nylium side faces flow over slopes.
    pub better_grass: bool,
    /// Opt-in: thin snow infill under partial blocks inside snowfields.
    pub better_snow: bool,
    /// Opt-in: recessed 3D relief for furnaces, bookshelves, windowed
    /// doors/trapdoors and jukeboxes.
    pub block_relief_3d: bool,
    /// Opt-in: voxel extrusion of player skin outer layers.
    pub skin_3d_layers: bool,
    /// Distance in blocks beyond which 3D relief falls back to the flat
    /// model. Clamped to `6.0..=48.0` via [`Self::effective_relief_lod_cutoff`].
    pub relief_lod_cutoff_blocks: f32,
    /// Distance in blocks beyond which skin layers render as the vanilla flat
    /// overlay. Clamped to `6.0..=32.0` via
    /// [`Self::effective_skin_3d_lod_cutoff`].
    pub skin_3d_lod_cutoff_blocks: f32,
    /// When `true` (default), blocks and models already restyled by the active
    /// resource-pack stack are left alone entirely.
    pub respect_custom_resourcepack: bool,
}

impl Default for VanillaPlusVisualsConfig {
    fn default() -> Self {
        Self {
            connected_textures: false,
            better_grass: false,
            better_snow: false,
            block_relief_3d: false,
            skin_3d_layers: false,
            relief_lod_cutoff_blocks: DEFAULT_RELIEF_LOD_CUTOFF_BLOCKS,
            skin_3d_lod_cutoff_blocks: DEFAULT_SKIN_3D_LOD_CUTOFF_BLOCKS,
            respect_custom_resourcepack: true,
        }
    }
}

impl VanillaPlusVisualsConfig {
    /// Relief LOD cutoff clamped to `6.0..=48.0` blocks.
    pub fn effective_relief_lod_cutoff(&self) -> f32 {
        self.relief_lod_cutoff_blocks
            .clamp(MIN_RELIEF_LOD_CUTOFF_BLOCKS, MAX_RELIEF_LOD_CUTOFF_BLOCKS)
    }

    /// Skin-layer LOD cutoff clamped to `6.0..=32.0` blocks.
    pub fn effective_skin_3d_lod_cutoff(&self) -> f32 {
        self.skin_3d_lod_cutoff_blocks
            .clamp(MIN_SKIN_3D_LOD_CUTOFF_BLOCKS, MAX_SKIN_3D_LOD_CUTOFF_BLOCKS)
    }
}

fn block_path(block_id: &str) -> &str {
    block_id.rsplit(':').next().unwrap_or(block_id)
}

// ---------------------------------------------------------------------------
// Connected textures
// ---------------------------------------------------------------------------

/// CTM topology a block uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CtmMode {
    /// Full 8-neighbor connected texture (glass, stained glass, tinted glass,
    /// glass panes): cardinals plus diagonal corners gated by both adjacent
    /// cardinals, resolving to a canonical `0..=46` tile index.
    Full47,
    /// Horizontal trim connecting left/right only (bookshelves).
    Horizontal,
    /// Vertical trim connecting up/down only (sandstone/pillar trims).
    Vertical,
}

/// Same-type neighbour presence in the face-local frame: `n` is the block
/// sharing the face's top edge, `e`/`s`/`w` run clockwise, and the diagonals
/// sit between them. A diagonal only counts toward corner styling when *both*
/// adjacent cardinals are also same-type.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CtmNeighbors {
    pub n: bool,
    pub e: bool,
    pub s: bool,
    pub w: bool,
    pub ne: bool,
    pub se: bool,
    pub sw: bool,
    pub nw: bool,
}

impl CtmNeighbors {
    /// No same-type neighbours.
    pub const NONE: Self = Self {
        n: false,
        e: false,
        s: false,
        w: false,
        ne: false,
        se: false,
        sw: false,
        nw: false,
    };

    /// All eight neighbours same-type.
    pub const ALL: Self = Self {
        n: true,
        e: true,
        s: true,
        w: true,
        ne: true,
        se: true,
        sw: true,
        nw: true,
    };

    /// Cardinal mask: `N = 1`, `E = 2`, `S = 4`, `W = 8`.
    fn cardinal_mask(&self) -> u8 {
        (self.n as u8) | ((self.e as u8) << 1) | ((self.s as u8) << 2) | ((self.w as u8) << 3)
    }

    /// Diagonals gated by both adjacent cardinals, in `ne, se, sw, nw` order.
    fn effective_corners(&self) -> (bool, bool, bool, bool) {
        (
            self.ne && self.n && self.e,
            self.se && self.s && self.e,
            self.sw && self.s && self.w,
            self.nw && self.n && self.w,
        )
    }
}

/// Which of the four face borders to suppress because a same-type neighbour
/// continues across that edge.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CtmBorderSuppress {
    pub hide_top: bool,
    pub hide_right: bool,
    pub hide_bottom: bool,
    pub hide_left: bool,
}

impl CtmBorderSuppress {
    /// All four borders drawn (isolated tile).
    pub const NONE: Self = Self {
        hide_top: false,
        hide_right: false,
        hide_bottom: false,
        hide_left: false,
    };
}

/// Horizontal trim tile position.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CtmHorizontalTile {
    /// Unconnected on both sides.
    Single,
    /// Left end of a run (same-type neighbour to the right only).
    Left,
    /// Interior of a run (neighbours on both sides).
    Middle,
    /// Right end of a run (same-type neighbour to the left only).
    Right,
}

/// Vertical trim tile position.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CtmVerticalTile {
    /// Unconnected above and below.
    Single,
    /// Top of a run (same-type neighbour below only).
    Top,
    /// Interior of a run (neighbours above and below).
    Middle,
    /// Bottom of a run (same-type neighbour above only).
    Bottom,
}

/// Resolved CTM tile for one face.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CtmTile {
    /// Canonical `0..=46` tile index plus the border-suppress bitmask.
    Full47 { index: u8, suppress: CtmBorderSuppress },
    /// Horizontal trim tile.
    Horizontal(CtmHorizontalTile),
    /// Vertical trim tile.
    Vertical(CtmVerticalTile),
}

/// Classifies a block id into a CTM topology.
///
/// `block_id` may be namespaced; only the path component is matched. Covers
/// `glass`, `glass_pane`, `tinted_glass`, all 16 `*_stained_glass` blocks and
/// panes, `bookshelf`, `chiseled_bookshelf`, the `*_sandstone` family, plus
/// generic `*_glass`, `*_glass_pane` and `*_bookshelf` modded ids.
pub fn classify_ctm_block(block_id: &str) -> Option<CtmMode> {
    let id = block_path(block_id);
    if id == "bookshelf" || id == "chiseled_bookshelf" || id.ends_with("_bookshelf") {
        return Some(CtmMode::Horizontal);
    }
    if id == "sandstone" || id.ends_with("_sandstone") {
        return Some(CtmMode::Vertical);
    }
    if id == "glass" || id == "glass_pane" || id.ends_with("_glass") || id.ends_with("_glass_pane") {
        return Some(CtmMode::Full47);
    }
    None
}

/// Canonical `0..=46` Full47 tile index.
///
/// Layout, from the cardinal mask `c` (`N=1, E=2, S=4, W=8`) and the effective
/// corner bits (a corner counts only when both adjacent cardinals connect):
///
/// * `0` — isolated (`c = 0`)
/// * `1..=4` — single cardinal end (`N, E, S, W`)
/// * `5..=12` — adjacent cardinal pairs (`NE, ES, SW, WN`), two tiles each
///   indexed by the shared corner
/// * `13, 14` — opposite pairs (`NS` column, `EW` row)
/// * `15..=30` — three cardinals (missing `W, N, E, S`), four tiles each
///   indexed by the two free corners
/// * `31..=46` — all four cardinals, indexed by `ne + 2*se + 4*sw + 8*nw`
fn full47_index(c: u8, ne: bool, se: bool, sw: bool, nw: bool) -> u8 {
    let (ne, se, sw, nw) = (ne as u8, se as u8, sw as u8, nw as u8);
    match c {
        0b0000 => 0,
        0b0001 => 1,
        0b0010 => 2,
        0b0100 => 3,
        0b1000 => 4,
        0b0011 => 5 + ne,
        0b0110 => 7 + se,
        0b1100 => 9 + sw,
        0b1001 => 11 + nw,
        0b0101 => 13,
        0b1010 => 14,
        0b0111 => 15 + ne + 2 * se,
        0b1110 => 19 + se + 2 * sw,
        0b1101 => 23 + sw + 2 * nw,
        0b1011 => 27 + nw + 2 * ne,
        _ => 31 + ne + 2 * se + 4 * sw + 8 * nw,
    }
}

/// Resolves the CTM tile for one face. Pure lookup: the mesher only swaps the
/// UV tile, so connected textures add zero vertices.
///
/// For [`CtmMode::Full47`] the result carries the canonical tile index and the
/// border-suppress bitmask (`hide_top = n`, `hide_right = e`, `hide_bottom =
/// s`, `hide_left = w`). For [`CtmMode::Horizontal`] only `e`/`w` matter; for
/// [`CtmMode::Vertical`] only `n`/`s` matter.
pub fn resolve_ctm_tile(mode: CtmMode, neighbors: &CtmNeighbors) -> CtmTile {
    match mode {
        CtmMode::Full47 => {
            let (ne, se, sw, nw) = neighbors.effective_corners();
            CtmTile::Full47 {
                index: full47_index(neighbors.cardinal_mask(), ne, se, sw, nw),
                suppress: CtmBorderSuppress {
                    hide_top: neighbors.n,
                    hide_right: neighbors.e,
                    hide_bottom: neighbors.s,
                    hide_left: neighbors.w,
                },
            }
        }
        CtmMode::Horizontal => CtmTile::Horizontal(match (neighbors.w, neighbors.e) {
            (false, false) => CtmHorizontalTile::Single,
            (false, true) => CtmHorizontalTile::Left,
            (true, false) => CtmHorizontalTile::Right,
            (true, true) => CtmHorizontalTile::Middle,
        }),
        CtmMode::Vertical => CtmTile::Vertical(match (neighbors.n, neighbors.s) {
            (false, false) => CtmVerticalTile::Single,
            (false, true) => CtmVerticalTile::Top,
            (true, false) => CtmVerticalTile::Bottom,
            (true, true) => CtmVerticalTile::Middle,
        }),
    }
}

// ---------------------------------------------------------------------------
// Better Grass & Better Snow
// ---------------------------------------------------------------------------

/// How a grass-family side face should be textured.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SideFaceStyle {
    /// Ordinary side texture (vanilla behaviour).
    VanillaSide,
    /// Full top texture on the side face, so a one-block slope flows
    /// seamlessly into the higher grass (Fast/Fancy slope mode).
    FullTopTexture,
    /// Snow-covered side texture, promoted when snowy grass sits next to snow.
    SnowySide,
}

/// Blocks eligible for the Better Grass slope treatment.
pub const BETTER_GRASS_BLOCKS: &[&str] = &[
    "grass_block",
    "mycelium",
    "podzol",
    "crimson_nylium",
    "warped_nylium",
];

/// Whether `block_id` is a grass-family block Better Grass can restyle.
/// The canonical Vanilla entries stay in [`BETTER_GRASS_BLOCKS`]; any
/// namespace's `*_grass_block`, `*_nylium`, `*_mycelium`, `*_podzol`, or
/// `*_moss_block` joins them so modded terrain gets the same slope flow.
pub fn is_better_grass_block(block_id: &str) -> bool {
    let path = block_path(block_id);
    BETTER_GRASS_BLOCKS.contains(&path)
        || path.ends_with("_grass_block")
        || path.ends_with("_nylium")
        || path.ends_with("_mycelium")
        || path.ends_with("_podzol")
        || path.ends_with("_moss_block")
}

/// Resolves how a grass-family side face should render.
///
/// On a horizontal face, when `better_grass` is on:
///
/// * snowy grass (`snowy == true`) next to snow (`adjacent_snow`) promotes to
///   [`SideFaceStyle::SnowySide`];
/// * otherwise, when the adjacent lower diagonal block is the same grass type
///   (`lower_diagonal_same_type`) and the adjacent same-level block does not
///   occlude (`!same_level_opaque`), promotes to
///   [`SideFaceStyle::FullTopTexture`] so the slope reads seamlessly.
///
/// Everything else — top/bottom faces, unlisted blocks, buried faces — stays
/// [`SideFaceStyle::VanillaSide`].
pub fn evaluate_better_grass(
    config: &VanillaPlusVisualsConfig,
    block_id: &str,
    face: Direction,
    lower_diagonal_same_type: bool,
    same_level_opaque: bool,
    snowy: bool,
    adjacent_snow: bool,
) -> SideFaceStyle {
    if !config.better_grass || !is_better_grass_block(block_id) {
        return SideFaceStyle::VanillaSide;
    }
    match face {
        Direction::North | Direction::South | Direction::East | Direction::West => {}
        _ => return SideFaceStyle::VanillaSide,
    }
    if snowy && adjacent_snow {
        return SideFaceStyle::SnowySide;
    }
    if lower_diagonal_same_type && !same_level_opaque {
        return SideFaceStyle::FullTopTexture;
    }
    SideFaceStyle::VanillaSide
}

/// Result of the Better Snow evaluation for one partial block.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct BetterSnowResult {
    /// Emit a thin snow layer at the base of the block.
    pub emit_snow_layer: bool,
    /// Layer height in blocks ([`BETTER_SNOW_LAYER_HEIGHT`] when emitted).
    pub layer_height: f32,
}

/// Whether `block_id` is a partial/non-full block Better Snow can sit under:
/// short/tall grass, ferns, dead bush, common flowers, `*_fence`,
/// `*_fence_gate`, `*_wall`, torches, levers, buttons and pressure plates.
/// Bottom slabs are passed separately via `is_bottom_slab`.
pub fn is_better_snow_partial_block(block_id: &str) -> bool {
    let id = block_path(block_id);
    const LIST: &[&str] = &[
        "short_grass",
        "tall_grass",
        "fern",
        "large_fern",
        "dead_bush",
        "lever",
        "dandelion",
        "poppy",
        "allium",
        "cornflower",
        "lily_of_the_valley",
        "sunflower",
        "lilac",
        "rose_bush",
        "peony",
        "wither_rose",
        "torchflower",
        "pitcher_plant",
        "pink_petals",
    ];
    LIST.contains(&id)
        || id.ends_with("_tulip")
        || id.ends_with("_orchid")
        || id.ends_with("_daisy")
        || id.ends_with("_bluet")
        || id.ends_with("_fence")
        || id.ends_with("_fence_gate")
        || id.ends_with("_wall")
        || id.ends_with("torch")
        || id.ends_with("_button")
        || id.ends_with("_pressure_plate")
}

/// Resolves whether a thin snow layer should be emitted at the base of a
/// partial block so snowfields look unbroken.
///
/// Requires `better_snow` on, a partial block (`is_bottom_slab` or
/// [`is_better_snow_partial_block`]) standing on solid ground, and at least
/// two cardinal neighbours holding snow layers or snow blocks.
pub fn evaluate_better_snow(
    config: &VanillaPlusVisualsConfig,
    block_id: &str,
    is_bottom_slab: bool,
    on_solid_ground: bool,
    snowy_cardinal_neighbors: u8,
) -> BetterSnowResult {
    const NO: BetterSnowResult = BetterSnowResult {
        emit_snow_layer: false,
        layer_height: 0.0,
    };
    if !config.better_snow || !on_solid_ground {
        return NO;
    }
    if !is_bottom_slab && !is_better_snow_partial_block(block_id) {
        return NO;
    }
    if snowy_cardinal_neighbors < 2 {
        return NO;
    }
    BetterSnowResult {
        emit_snow_layer: true,
        layer_height: BETTER_SNOW_LAYER_HEIGHT,
    }
}

// ---------------------------------------------------------------------------
// Geometry helpers (block-local 0..=1, outward CCW — same as vanilla_plus_3d)
// ---------------------------------------------------------------------------

fn vsub(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}

fn vcross(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}

fn vnorm(a: [f32; 3]) -> f32 {
    (a[0] * a[0] + a[1] * a[1] + a[2] * a[2]).sqrt()
}

fn quad_normal(p: &[[f32; 3]; 4]) -> Option<[f32; 3]> {
    let n = vcross(vsub(p[1], p[0]), vsub(p[2], p[0]));
    let l = vnorm(n);
    (l > 1e-12).then(|| [n[0] / l, n[1] / l, n[2] / l])
}

fn push_quad(mesh: &mut ExtrudedMesh, positions: [[f32; 3]; 4], uv: [[f32; 2]; 4], tint: i32) {
    if mesh.quads.len() >= MAX_UPGRADE_QUADS {
        return;
    }
    let Some(normal) = quad_normal(&positions) else {
        return;
    };
    mesh.quads.push(ExtrudedQuad {
        positions,
        normal,
        uv,
        tint_index: tint,
    });
}

/// A 2D parametrisation of one axis-aligned block/limb face: `P(u, v, d) =
/// o + u*U + v*V + d*N` with `U x V = N` (outward), so the shared winding
/// tables below always face the documented direction.
struct QuadFrame {
    o: [f32; 3],
    u: [f32; 3],
    v: [f32; 3],
    n: [f32; 3],
}

impl QuadFrame {
    fn point(&self, u: f32, v: f32, d: f32) -> [f32; 3] {
        [
            self.o[0] + u * self.u[0] + v * self.v[0] + d * self.n[0],
            self.o[1] + u * self.u[1] + v * self.v[1] + d * self.n[1],
            self.o[2] + u * self.u[2] + v * self.v[2] + d * self.n[2],
        ]
    }
}

/// Block-face frame (`v` = up) for a given outward direction.
fn block_face_frame(dir: Direction) -> QuadFrame {
    match dir {
        Direction::South => QuadFrame {
            o: [0.0, 0.0, 1.0],
            u: [1.0, 0.0, 0.0],
            v: [0.0, 1.0, 0.0],
            n: [0.0, 0.0, 1.0],
        },
        Direction::North => QuadFrame {
            o: [1.0, 0.0, 0.0],
            u: [-1.0, 0.0, 0.0],
            v: [0.0, 1.0, 0.0],
            n: [0.0, 0.0, -1.0],
        },
        Direction::East => QuadFrame {
            o: [1.0, 0.0, 1.0],
            u: [0.0, 0.0, -1.0],
            v: [0.0, 1.0, 0.0],
            n: [1.0, 0.0, 0.0],
        },
        Direction::West => QuadFrame {
            o: [0.0, 0.0, 0.0],
            u: [0.0, 0.0, 1.0],
            v: [0.0, 1.0, 0.0],
            n: [-1.0, 0.0, 0.0],
        },
        Direction::Up => QuadFrame {
            o: [0.0, 1.0, 1.0],
            u: [1.0, 0.0, 0.0],
            v: [0.0, 0.0, -1.0],
            n: [0.0, 1.0, 0.0],
        },
        Direction::Down => QuadFrame {
            o: [0.0, 0.0, 0.0],
            u: [1.0, 0.0, 0.0],
            v: [0.0, 0.0, 1.0],
            n: [0.0, -1.0, 0.0],
        },
    }
}

/// A rectangular recess pocket on a block face: `(u0..u1, v0..v1)` in the
/// face's `v`-up frame, receding `depth` into the block.
#[derive(Clone, Copy)]
struct Recess {
    u0: f32,
    u1: f32,
    v0: f32,
    v1: f32,
    depth: f32,
}

/// Emits a recess: back panel facing outward plus the four pocket walls
/// facing inward, all strictly inside `0..=1`.
fn emit_recess(mesh: &mut ExtrudedMesh, fr: &QuadFrame, r: Recess, tint: i32) {
    let Recess { u0, u1, v0, v1, depth } = r;
    let p = |u: f32, v: f32, d: f32| fr.point(u, v, -d);
    let uv = |u: f32, v: f32| [u.clamp(0.0, 1.0), v.clamp(0.0, 1.0)];
    // Back panel (+N).
    push_quad(
        mesh,
        [
            p(u0, v0, depth),
            p(u1, v0, depth),
            p(u1, v1, depth),
            p(u0, v1, depth),
        ],
        [uv(u0, v0), uv(u1, v0), uv(u1, v1), uv(u0, v1)],
        tint,
    );
    // Left wall at u0 (+U).
    push_quad(
        mesh,
        [
            p(u0, v0, 0.0),
            p(u0, v0, depth),
            p(u0, v1, depth),
            p(u0, v1, 0.0),
        ],
        [uv(v0, 0.0), uv(v0, depth), uv(v1, depth), uv(v1, 0.0)],
        tint,
    );
    // Right wall at u1 (-U).
    push_quad(
        mesh,
        [
            p(u1, v0, depth),
            p(u1, v0, 0.0),
            p(u1, v1, 0.0),
            p(u1, v1, depth),
        ],
        [uv(v0, depth), uv(v0, 0.0), uv(v1, 0.0), uv(v1, depth)],
        tint,
    );
    // Bottom wall at v0 (+V).
    push_quad(
        mesh,
        [
            p(u0, v0, 0.0),
            p(u1, v0, 0.0),
            p(u1, v0, depth),
            p(u0, v0, depth),
        ],
        [uv(u0, 0.0), uv(u1, 0.0), uv(u1, depth), uv(u0, depth)],
        tint,
    );
    // Top wall at v1 (-V).
    push_quad(
        mesh,
        [
            p(u0, v1, depth),
            p(u1, v1, depth),
            p(u1, v1, 0.0),
            p(u0, v1, 0.0),
        ],
        [uv(u0, depth), uv(u1, depth), uv(u1, 0.0), uv(u0, 0.0)],
        tint,
    );
}

/// Emits a recessed outward-facing panel inside an already carved pocket —
/// used for the lit furnace ember face. Same winding as the back panel.
fn emit_recessed_panel(mesh: &mut ExtrudedMesh, fr: &QuadFrame, r: Recess, tint: i32) {
    let Recess { u0, u1, v0, v1, depth } = r;
    let p = |u: f32, v: f32, d: f32| fr.point(u, v, -d);
    let uv = |u: f32, v: f32| [u.clamp(0.0, 1.0), v.clamp(0.0, 1.0)];
    push_quad(
        mesh,
        [
            p(u0, v0, depth),
            p(u1, v0, depth),
            p(u1, v1, depth),
            p(u0, v1, depth),
        ],
        [uv(u0, v0), uv(u1, v0), uv(u1, v1), uv(u0, v1)],
        tint,
    );
}

/// Emits an axis-aligned box (`from`/`to` corners), all six faces outward.
fn emit_box(mesh: &mut ExtrudedMesh, from: [f32; 3], to: [f32; 3], tint: i32) {
    let (x0, y0, z0) = (from[0], from[1], from[2]);
    let (x1, y1, z1) = (to[0], to[1], to[2]);
    const UV: [[f32; 2]; 4] = [[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]];
    // +X, -X, +Y, -Y, +Z, -Z faces, each outward-counterclockwise.
    let quads: [[[f32; 3]; 4]; 6] = [
        [[x1, y0, z1], [x1, y0, z0], [x1, y1, z0], [x1, y1, z1]],
        [[x0, y0, z0], [x0, y0, z1], [x0, y1, z1], [x0, y1, z0]],
        [[x0, y1, z1], [x1, y1, z1], [x1, y1, z0], [x0, y1, z0]],
        [[x0, y0, z0], [x1, y0, z0], [x1, y0, z1], [x0, y0, z1]],
        [[x0, y0, z1], [x1, y0, z1], [x1, y1, z1], [x0, y1, z1]],
        [[x0, y0, z0], [x0, y1, z0], [x1, y1, z0], [x1, y0, z0]],
    ];
    for positions in quads {
        push_quad(mesh, positions, UV, tint);
    }
}

/// `M * v` for `M = R_y(k * 90 degrees)` about the block centre column.
fn yaw_matrix(quarter_turns: i32) -> [[f32; 3]; 3] {
    match quarter_turns.rem_euclid(4) {
        0 => [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]],
        1 => [[0.0, 0.0, 1.0], [0.0, 1.0, 0.0], [-1.0, 0.0, 0.0]],
        2 => [[-1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, -1.0]],
        _ => [[0.0, 0.0, -1.0], [0.0, 1.0, 0.0], [1.0, 0.0, 0.0]],
    }
}

/// Rotates a mesh by `k` quarter turns about the block-centre vertical axis.
fn yaw_mesh(mesh: &mut ExtrudedMesh, quarter_turns: i32) {
    if quarter_turns.rem_euclid(4) == 0 {
        return;
    }
    mesh.affine(
        yaw_matrix(quarter_turns),
        [0.5, 0.0, 0.5],
        [0.5, 0.0, 0.5],
    );
}

/// Quarter turns that carry the canonical `South` build onto `facing`
/// (`South`/`East`/`North`/`West` -> `0/1/2/3`). Non-horizontal facings are
/// treated as `South`.
fn facing_turns(facing: Direction) -> i32 {
    match facing {
        Direction::East => 1,
        Direction::North => 2,
        Direction::West => 3,
        _ => 0,
    }
}

// ---------------------------------------------------------------------------
// 3D block relief
// ---------------------------------------------------------------------------

/// What a `build_3d_relief_mesh` call should carve.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReliefSubject {
    /// Furnace, blast furnace or smoker: recessed 1–2 texel firebox and top
    /// input slot on the front face; `lit` adds a tagged emissive ember quad.
    Furnace { lit: bool, facing: Direction },
    /// Bookshelf: recessed book bands on all four horizontal sides.
    Bookshelf,
    /// Windowed door: 2-texel-thick perforated window bars standing on the
    /// door plane.
    DoorWindow { facing: Direction },
    /// Windowed trapdoor: the same bars lying flat on the trapdoor plane.
    TrapdoorWindow { facing: Direction },
    /// Jukebox (or crafting-table) top: 1-texel recessed disc slot.
    JukeboxTop,
}

/// Builds the procedural 3D relief mesh for one block, or `None` when the
/// relief should not render: `block_relief_3d` off, a custom resource-pack
/// model deferred to (`respect_custom_resourcepack`), or `distance_blocks`
/// beyond `relief_lod_cutoff_blocks`.
///
/// All emitted quads stay strictly inside block-local `0..=1` space: recesses
/// carve inward, never outward, so a disabled or far-away block is always
/// pixel-identical to vanilla.
pub fn build_3d_relief_mesh(
    config: &VanillaPlusVisualsConfig,
    subject: ReliefSubject,
    has_custom_resourcepack_model: bool,
    distance_blocks: f32,
) -> Option<ExtrudedMesh> {
    if !config.block_relief_3d {
        return None;
    }
    if config.respect_custom_resourcepack && has_custom_resourcepack_model {
        return None;
    }
    if distance_blocks > config.effective_relief_lod_cutoff() {
        return None;
    }
    Some(match subject {
        ReliefSubject::Furnace { lit, facing } => build_furnace_relief(lit, facing),
        ReliefSubject::Bookshelf => build_bookshelf_relief(),
        ReliefSubject::DoorWindow { facing } => build_door_window_relief(facing, false),
        ReliefSubject::TrapdoorWindow { facing } => build_door_window_relief(facing, true),
        ReliefSubject::JukeboxTop => build_jukebox_relief(),
    })
}

/// Furnace relief, built facing `South` then yaw-rotated to `facing`.
fn build_furnace_relief(lit: bool, facing: Direction) -> ExtrudedMesh {
    let mut mesh = ExtrudedMesh::new();
    let front = block_face_frame(Direction::South);
    // Firebox: centre 8x6 texels, recessed 2 texels.
    emit_recess(
        &mut mesh,
        &front,
        Recess {
            u0: 4.0 * TEXEL,
            u1: 12.0 * TEXEL,
            v0: 2.0 * TEXEL,
            v1: 8.0 * TEXEL,
            depth: 2.0 * TEXEL,
        },
        -1,
    );
    // Top input slot: centre 8x2 texels, recessed 1 texel.
    emit_recess(
        &mut mesh,
        &front,
        Recess {
            u0: 4.0 * TEXEL,
            u1: 12.0 * TEXEL,
            v0: 11.0 * TEXEL,
            v1: 13.0 * TEXEL,
            depth: TEXEL,
        },
        -1,
    );
    if lit {
        // Ember face: inset inside the firebox, just in front of its back
        // panel so it never z-fights. Tagged for the dynamic-light hook.
        emit_recessed_panel(
            &mut mesh,
            &front,
            Recess {
                u0: 5.0 * TEXEL,
                u1: 11.0 * TEXEL,
                v0: 3.0 * TEXEL,
                v1: 7.0 * TEXEL,
                depth: 1.5 * TEXEL,
            },
            RELIEF_TINT_EMISSIVE,
        );
    }
    yaw_mesh(&mut mesh, facing_turns(facing));
    mesh
}

/// Bookshelf relief: two recessed book bands per horizontal side; the
/// un-recessed strips read as the protruding wooden shelf planks.
fn build_bookshelf_relief() -> ExtrudedMesh {
    let mut mesh = ExtrudedMesh::new();
    for dir in [
        Direction::South,
        Direction::East,
        Direction::North,
        Direction::West,
    ] {
        let f = block_face_frame(dir);
        for (v0, v1) in [(1.0, 7.0), (9.0, 15.0)] {
            emit_recess(
                &mut mesh,
                &f,
                Recess {
                    u0: TEXEL,
                    u1: 15.0 * TEXEL,
                    v0: v0 * TEXEL,
                    v1: v1 * TEXEL,
                    depth: TEXEL,
                },
                -1,
            );
        }
    }
    mesh
}

/// Windowed door/trapdoor frame: a 2-texel-thick perforated bar grille over
/// the window opening. Doors stand the grille in a vertical plane
/// (`horizontal = false`), trapdoors lay it flat.
fn build_door_window_relief(facing: Direction, horizontal: bool) -> ExtrudedMesh {
    let mut mesh = ExtrudedMesh::new();
    let t = TEXEL;
    // Window opening 6x7 texels; frame bars 2 texels wide plus a mid crossbar.
    let bars: [([f32; 2], [f32; 2]); 5] = [
        ([3.0 * t, 5.0 * t], [7.0 * t, 14.0 * t]),   // left
        ([11.0 * t, 13.0 * t], [7.0 * t, 14.0 * t]), // right
        ([3.0 * t, 13.0 * t], [5.0 * t, 7.0 * t]),   // bottom
        ([3.0 * t, 13.0 * t], [14.0 * t, 15.0 * t]), // top
        ([5.0 * t, 11.0 * t], [10.0 * t, 11.0 * t]), // crossbar
    ];
    for ([a0, a1], [b0, b1]) in bars {
        if horizontal {
            emit_box(&mut mesh, [a0, 0.0, b0], [a1, 2.0 * t, b1], -1);
        } else {
            emit_box(&mut mesh, [a0, b0, 0.0], [a1, b1, 2.0 * t], -1);
        }
    }
    yaw_mesh(&mut mesh, facing_turns(facing));
    mesh
}

/// Jukebox relief: a subtle 1-texel recessed disc slot on the top face.
fn build_jukebox_relief() -> ExtrudedMesh {
    let mut mesh = ExtrudedMesh::new();
    let top = block_face_frame(Direction::Up);
    emit_recess(
        &mut mesh,
        &top,
        Recess {
            u0: 4.0 * TEXEL,
            u1: 12.0 * TEXEL,
            v0: 6.0 * TEXEL,
            v1: 10.0 * TEXEL,
            depth: TEXEL,
        },
        -1,
    );
    mesh
}

// ---------------------------------------------------------------------------
// Clean-room 3D player skin outer layers
// ---------------------------------------------------------------------------

/// Standard skin canvas edge, in texels.
pub const SKIN_DIM: usize = 64;
/// Byte length of a standard `64x64` RGBA8 skin.
pub const SKIN_RGBA_BYTES: usize = SKIN_DIM * SKIN_DIM * 4;
/// Outward extrusion depth of an overlay layer, in limb texels.
pub const SKIN_LAYER_EXTRUSION_TEXELS: f32 = 0.5;

/// The six overlay (outer-layer) parts of a standard `64x64` skin.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SkinOverlayPart {
    /// Head outer layer ("hat").
    Hat,
    /// Body outer layer ("jacket").
    Jacket,
    /// Left arm outer layer.
    LeftSleeve,
    /// Right arm outer layer.
    RightSleeve,
    /// Left leg outer layer.
    LeftPants,
    /// Right leg outer layer.
    RightPants,
}

/// Outcome of [`build_3d_skin_layer_part`].
#[derive(Clone, Debug, PartialEq)]
pub enum SkinLayerResult {
    /// Render the vanilla flat overlay: feature off or beyond
    /// `skin_3d_lod_cutoff_blocks` — zero cost for distant players.
    UseFlatOverlay,
    /// The buffer was not exactly [`SKIN_RGBA_BYTES`]; fails closed.
    InvalidSkin,
    /// Extruded overlay mesh in limb-local texel units (origin at the limb's
    /// own box corner; `x`/`z` run `0..=depth_dims`, `y` runs `0..=height`).
    Extruded(ExtrudedMesh),
}

/// UV sub-rectangle of one face inside the `64x64` skin, in pixels.
#[derive(Clone, Copy)]
struct FaceRegion {
    u0: usize,
    v0: usize,
    w: usize,
    h: usize,
}

/// Face UV regions in [`Direction`] order (`Down, Up, North, South, West,
/// East`): bottom, top, back, front, and the two side strips. `dims` is the
/// limb box in texels `(W, H, D)`.
struct PartLayout {
    dims: [f32; 3],
    regions: [FaceRegion; 6],
}

impl SkinOverlayPart {
    fn layout(self, slim_arms: bool) -> PartLayout {
        let r = |u0, v0, w, h| FaceRegion { u0, v0, w, h };
        match self {
            SkinOverlayPart::Hat => PartLayout {
                dims: [8.0, 8.0, 8.0],
                regions: [
                    r(48, 0, 8, 8),
                    r(40, 0, 8, 8),
                    r(56, 8, 8, 8),
                    r(40, 8, 8, 8),
                    r(32, 8, 8, 8),
                    r(48, 8, 8, 8),
                ],
            },
            SkinOverlayPart::Jacket => PartLayout {
                dims: [8.0, 12.0, 4.0],
                regions: [
                    r(28, 32, 8, 4),
                    r(20, 32, 8, 4),
                    r(32, 36, 8, 12),
                    r(20, 36, 8, 12),
                    r(16, 36, 4, 12),
                    r(28, 36, 4, 12),
                ],
            },
            SkinOverlayPart::RightSleeve => {
                let w = if slim_arms { 3 } else { 4 };
                PartLayout {
                    dims: [w as f32, 12.0, 4.0],
                    regions: [
                        r(48, 32, w, 4),
                        r(44, 32, w, 4),
                        r(52, 36, w, 12),
                        r(44, 36, w, 12),
                        r(40, 36, 4, 12),
                        r(48, 36, 4, 12),
                    ],
                }
            }
            SkinOverlayPart::LeftSleeve => {
                let w = if slim_arms { 3 } else { 4 };
                PartLayout {
                    dims: [w as f32, 12.0, 4.0],
                    regions: [
                        r(56, 48, w, 4),
                        r(52, 48, w, 4),
                        r(60, 52, w, 12),
                        r(52, 52, w, 12),
                        r(48, 52, 4, 12),
                        r(56, 52, 4, 12),
                    ],
                }
            }
            SkinOverlayPart::RightPants => PartLayout {
                dims: [4.0, 12.0, 4.0],
                regions: [
                    r(8, 32, 4, 4),
                    r(4, 32, 4, 4),
                    r(12, 36, 4, 12),
                    r(4, 36, 4, 12),
                    r(0, 36, 4, 12),
                    r(8, 36, 4, 12),
                ],
            },
            SkinOverlayPart::LeftPants => PartLayout {
                dims: [4.0, 12.0, 4.0],
                regions: [
                    r(8, 48, 4, 4),
                    r(4, 48, 4, 4),
                    r(12, 52, 4, 12),
                    r(4, 52, 4, 12),
                    r(0, 52, 4, 12),
                    r(8, 52, 4, 12),
                ],
            },
        }
    }
}

/// Limb-box frame for one face. `v` follows the skin's row direction
/// (downward) so `U x V = N` holds for every face; positions are in texels.
fn limb_face_frame(dir: Direction, dims: [f32; 3]) -> QuadFrame {
    let (w, h, d) = (dims[0], dims[1], dims[2]);
    match dir {
        // Front (+Z): texture rows run left-to-right across -X, top-to-bottom
        // across -Y.
        Direction::South => QuadFrame {
            o: [w, h, d],
            u: [-1.0, 0.0, 0.0],
            v: [0.0, -1.0, 0.0],
            n: [0.0, 0.0, 1.0],
        },
        // Back (-Z).
        Direction::North => QuadFrame {
            o: [0.0, h, 0.0],
            u: [1.0, 0.0, 0.0],
            v: [0.0, -1.0, 0.0],
            n: [0.0, 0.0, -1.0],
        },
        // +X side.
        Direction::East => QuadFrame {
            o: [w, h, 0.0],
            u: [0.0, 0.0, 1.0],
            v: [0.0, -1.0, 0.0],
            n: [1.0, 0.0, 0.0],
        },
        // -X side.
        Direction::West => QuadFrame {
            o: [0.0, h, d],
            u: [0.0, 0.0, -1.0],
            v: [0.0, -1.0, 0.0],
            n: [-1.0, 0.0, 0.0],
        },
        // Top (+Y): texture row 0 at the front edge.
        Direction::Up => QuadFrame {
            o: [0.0, h, d],
            u: [1.0, 0.0, 0.0],
            v: [0.0, 0.0, -1.0],
            n: [0.0, 1.0, 0.0],
        },
        // Bottom (-Y).
        Direction::Down => QuadFrame {
            o: [0.0, 0.0, 0.0],
            u: [1.0, 0.0, 0.0],
            v: [0.0, 0.0, 1.0],
            n: [0.0, -1.0, 0.0],
        },
    }
}

/// One face's `w x h` alpha mask plus its pixel offset inside the skin.
struct OverlayMask<'a> {
    mask: &'a [bool],
    w: usize,
    h: usize,
    px0: usize,
    py0: usize,
}

/// Extrudes one face's alpha mask outward `depth` texels: greedy-merged
/// front/back panels plus perimeter side walls merged into runs along every
/// opaque -> transparent boundary. No hidden interior quads exist; a single
/// opaque texel yields exactly six quads.
fn extrude_overlay_face(
    mesh: &mut ExtrudedMesh,
    face: &OverlayMask<'_>,
    fr: &QuadFrame,
    depth: f32,
) {
    let (mask, w, h, px0, py0) = (face.mask, face.w, face.h, face.px0, face.py0);
    let opaque = |x: usize, y: usize| mask[y * w + x];
    let uv = |x: f32, y: f32| {
        [
            ((px0 as f32 + x) / SKIN_DIM as f32).clamp(0.0, 1.0),
            ((py0 as f32 + y) / SKIN_DIM as f32).clamp(0.0, 1.0),
        ]
    };
    let p = |x: f32, y: f32, d: f32| fr.point(x, y, d);

    // Greedy-merge opaque rectangles; front (+N) and back (-N) per rect.
    let mut done = vec![false; w * h];
    for ty in 0..h {
        for tx in 0..w {
            if !opaque(tx, ty) || done[ty * w + tx] {
                continue;
            }
            let mut rw = 1usize;
            while tx + rw < w && opaque(tx + rw, ty) && !done[ty * w + tx + rw] {
                rw += 1;
            }
            let mut rh = 1usize;
            'grow: while ty + rh < h {
                for xx in tx..tx + rw {
                    if !opaque(xx, ty + rh) || done[(ty + rh) * w + xx] {
                        break 'grow;
                    }
                }
                rh += 1;
            }
            for yy in ty..ty + rh {
                for xx in tx..tx + rw {
                    done[yy * w + xx] = true;
                }
            }
            let (x0, x1) = (tx as f32, (tx + rw) as f32);
            let (y0, y1) = (ty as f32, (ty + rh) as f32);
            push_quad(
                mesh,
                [
                    p(x0, y0, depth),
                    p(x1, y0, depth),
                    p(x1, y1, depth),
                    p(x0, y1, depth),
                ],
                [uv(x0, y0), uv(x1, y0), uv(x1, y1), uv(x0, y1)],
                -1,
            );
            push_quad(
                mesh,
                [
                    p(x1, y0, 0.0),
                    p(x0, y0, 0.0),
                    p(x0, y1, 0.0),
                    p(x1, y1, 0.0),
                ],
                [uv(x1, y0), uv(x0, y0), uv(x0, y1), uv(x1, y1)],
                -1,
            );
        }
    }

    // Horizontal rims: per row, runs of cells whose top/bottom neighbour is
    // transparent merge into one wall quad each.
    for ty in 0..h {
        for &top_side in &[true, false] {
            let mut run: Option<usize> = None;
            for tx in 0..=w {
                let edge = tx < w
                    && opaque(tx, ty)
                    && if top_side {
                        ty == 0 || !opaque(tx, ty - 1)
                    } else {
                        ty + 1 == h || !opaque(tx, ty + 1)
                    };
                match (edge, run) {
                    (true, None) => run = Some(tx),
                    (false, Some(start)) => {
                        let (x0, x1) = (start as f32, tx as f32);
                        let y = if top_side { ty as f32 } else { (ty + 1) as f32 };
                        let quad = if top_side {
                            // -V wall.
                            [
                                p(x0, y, 0.0),
                                p(x1, y, 0.0),
                                p(x1, y, depth),
                                p(x0, y, depth),
                            ]
                        } else {
                            // +V wall.
                            [
                                p(x1, y, 0.0),
                                p(x0, y, 0.0),
                                p(x0, y, depth),
                                p(x1, y, depth),
                            ]
                        };
                        push_quad(mesh, quad, [uv(x0, y), uv(x1, y), uv(x1, y), uv(x0, y)], -1);
                        run = None;
                    }
                    _ => {}
                }
            }
        }
    }

    // Vertical rims: per column, runs of cells whose left/right neighbour is
    // transparent merge into one wall quad each.
    for tx in 0..w {
        for &left_side in &[true, false] {
            let mut run: Option<usize> = None;
            for ty in 0..=h {
                let edge = ty < h
                    && opaque(tx, ty)
                    && if left_side {
                        tx == 0 || !opaque(tx - 1, ty)
                    } else {
                        tx + 1 == w || !opaque(tx + 1, ty)
                    };
                match (edge, run) {
                    (true, None) => run = Some(ty),
                    (false, Some(start)) => {
                        let (y0, y1) = (start as f32, ty as f32);
                        let x = if left_side { tx as f32 } else { (tx + 1) as f32 };
                        let quad = if left_side {
                            // -U wall.
                            [
                                p(x, y1, 0.0),
                                p(x, y0, 0.0),
                                p(x, y0, depth),
                                p(x, y1, depth),
                            ]
                        } else {
                            // +U wall.
                            [
                                p(x, y0, 0.0),
                                p(x, y1, 0.0),
                                p(x, y1, depth),
                                p(x, y0, depth),
                            ]
                        };
                        push_quad(mesh, quad, [uv(x, y0), uv(x, y1), uv(x, y1), uv(x, y0)], -1);
                        run = None;
                    }
                    _ => {}
                }
            }
        }
    }
}

/// Extrudes the opaque pixels (`alpha >= 128`) of one `64x64` skin overlay
/// part outward [`SKIN_LAYER_EXTRUSION_TEXELS`] texels, with greedy-merged
/// front/back panels and perimeter walls — a clean-room voxel-3D outer layer
/// that works identically for classic (4 texel arms) and slim (3 texel arms)
/// models.
///
/// Returns [`SkinLayerResult::UseFlatOverlay`] when `skin_3d_layers` is off or
/// `distance_blocks` exceeds `skin_3d_lod_cutoff_blocks`, and
/// [`SkinLayerResult::InvalidSkin`] when `skin_rgba` is not exactly
/// [`SKIN_RGBA_BYTES`] long — both fail closed, never panic.
pub fn build_3d_skin_layer_part(
    config: &VanillaPlusVisualsConfig,
    skin_rgba: &[u8],
    part: SkinOverlayPart,
    slim_arms: bool,
    distance_blocks: f32,
) -> SkinLayerResult {
    if !config.skin_3d_layers {
        return SkinLayerResult::UseFlatOverlay;
    }
    if skin_rgba.len() != SKIN_RGBA_BYTES {
        return SkinLayerResult::InvalidSkin;
    }
    if distance_blocks > config.effective_skin_3d_lod_cutoff() {
        return SkinLayerResult::UseFlatOverlay;
    }
    let layout = part.layout(slim_arms);
    let mut mesh = ExtrudedMesh::new();
    for dir in Direction::ALL {
        let region = layout.regions[dir as usize];
        if region.w == 0 || region.h == 0 {
            continue;
        }
        let mut mask = vec![false; region.w * region.h];
        for ty in 0..region.h {
            for tx in 0..region.w {
                let px = region.u0 + tx;
                let py = region.v0 + ty;
                let alpha = skin_rgba[(py * SKIN_DIM + px) * 4 + 3];
                mask[ty * region.w + tx] = alpha >= ALPHA_CUTOFF;
            }
        }
        extrude_overlay_face(
            &mut mesh,
            &OverlayMask {
                mask: &mask,
                w: region.w,
                h: region.h,
                px0: region.u0,
                py0: region.v0,
            },
            &limb_face_frame(dir, layout.dims),
            SKIN_LAYER_EXTRUSION_TEXELS,
        );
    }
    SkinLayerResult::Extruded(mesh)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn better_grass_accepts_modded_grass_family_suffixes() {
        // The five canonical Vanilla entries are unchanged, with or without
        // the minecraft: namespace prefix.
        for block in [
            "grass_block",
            "mycelium",
            "podzol",
            "crimson_nylium",
            "warped_nylium",
            "minecraft:grass_block",
            "minecraft:mycelium",
        ] {
            assert!(is_better_grass_block(block), "{block}");
        }
        // Any namespace's grass-family suffixes join the list.
        for block in [
            "biomesoplenty:origin_grass_block",
            "mymod:crimson_nylium",
            "mymod:glowing_mycelium",
            "mymod:sandy_podzol",
            "mymod:lush_moss_block",
        ] {
            assert!(is_better_grass_block(block), "{block}");
        }
        // Non-grass blocks and bare suffix-free stems stay ineligible.
        for block in [
            "minecraft:stone",
            "mymod:dirt",
            "mymod:nylium",
            "mymod:grass",
            "mymod:grass_block_slab",
        ] {
            assert!(!is_better_grass_block(block), "{block}");
        }
    }
}
