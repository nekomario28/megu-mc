// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later

//! Optional **Vanilla+** pixel-consistent procedural 3D mesh upgrade.
//!
//! This module is **opt-in and default OFF**: [`VanillaPlus3dConfig::default`]
//! has `enabled == false`, so nothing about the vanilla look changes unless the
//! user explicitly turns the feature on. When enabled it procedurally upgrades
//! flat 2D cross-plane blocks (crops, flowers, amethyst crystals, pointed
//! dripstone, rails, ladders, chains, iron bars, torches, lanterns) and the
//! celestial bodies (3D voxel Sun and 8-phase 3D voxel Moon) into low-quad 3D
//! meshes that keep the unmistakable Minecraft pixel-art identity: every face
//! is a flat texel-grid facet with axis-aligned or 45-degree geometry, never a
//! smooth "realistic" shape.
//!
//! Three fail-safes keep the upgrade invisible when it should be:
//!
//! * **Resource-pack priority** — when a block model hint reports a custom 3D
//!   resource-pack model and `respect_custom_resourcepack_models` is set
//!   (default), [`vanilla_plus_decision`] returns
//!   [`MeshUpgradeDecision::DeferToResourcePack`] so authored 3D packs always
//!   win with zero conflicts.
//! * **Distance LOD** — beyond `lod_cutoff_blocks` (default 24, clamped to
//!   `8.0..=64.0`) the decision is
//!   [`MeshUpgradeDecision::Use2dLodFallback`], so far-away chunks stay
//!   zero-overhead flat cross planes and FPS stays rock-solid.
//! * **Per-category toggles** — celestial, crops/flora, crystals, rails/ladders
//!   and fixtures/chains are independently selectable; a disabled category
//!   falls back to [`MeshUpgradeDecision::Use2dVanilla`].
//!
//! All geometry is emitted in block-local `0..=1` space with outward
//! counterclockwise quads (the same winding convention as
//! [`crate::model_baker::BakedModelQuad`]), unit normals and `0..=1` UVs, so
//! the output can feed the same upload path as baked resource-pack models.
//! Everything is pure `std`, deterministic, and reads no wall clock: the only
//! "randomness" is a position hash used for sub-texel crop jitter.
//!
//! No Mojang or third-party geometry or texture data is referenced anywhere in
//! this file: all shapes are generated from first principles and an optional
//! caller-supplied 16x16 alpha mask (which works identically for vanilla and
//! modded cross sprites).

use std::f32::consts::{FRAC_1_SQRT_2, PI, SQRT_2};

use crate::section::Direction;

// ---------------------------------------------------------------------------
// Limits and constants (fail-closed, shared spirit with model_baker)
// ---------------------------------------------------------------------------

/// One sprite texel in block units (`1/16`).
pub const TEXEL: f32 = 1.0 / 16.0;

/// Alpha value at or above which a sprite texel counts as solid. Matches the
/// classic alpha-test threshold so cutout sprites extrude exactly the pixels
/// the vanilla shader would draw.
pub const ALPHA_CUTOFF: u8 = 128;

/// Hard cap on quads emitted for one upgraded mesh. Natural bounds are far
/// lower (a full 16x16 extrusion is 6 quads); the cap exists so a future
/// caller cannot accidentally build an unbounded mesh.
pub const MAX_UPGRADE_QUADS: usize = 4096;

/// Default distance in blocks beyond which upgraded blocks fall back to the
/// flat 2D model.
pub const DEFAULT_LOD_CUTOFF_BLOCKS: f32 = 24.0;
/// Smallest accepted LOD cutoff.
pub const MIN_LOD_CUTOFF_BLOCKS: f32 = 8.0;
/// Largest accepted LOD cutoff.
pub const MAX_LOD_CUTOFF_BLOCKS: f32 = 64.0;

/// Maximum absolute micro-offset applied to cross blades, in block units.
const JITTER_AMP: f32 = 1.0 / 64.0;

// ---------------------------------------------------------------------------
// Configuration & decision
// ---------------------------------------------------------------------------

/// Feature category an upgraded block belongs to. Each maps to one opt-in
/// toggle on [`VanillaPlus3dConfig`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VanillaPlusCategory {
    /// 3D voxel Sun and 8-phase 3D voxel Moon.
    Celestial3d,
    /// Wheat/carrots/potatoes/beetroots/nether wart, flowers, saplings, tall
    /// grass — and any modded `is_cross_model` block generically.
    CropsAndFlora,
    /// Amethyst cluster + buds, pointed dripstone, sea pickle, and crystal-like
    /// modded sprites.
    CrystalsAndClusters,
    /// Rail, powered/detector/activator rail, ladder.
    RailsAndLadders,
    /// Chain, iron bars, torches, lanterns.
    FixturesAndChains,
    /// Subtle 0.5-texel outward relief on exposed ore vein texels for
    /// `*_ore`/`ancient_debris` blocks, driven by a caller-supplied vein mask.
    Ores3dRelief,
}

/// Per-user configuration for the Vanilla+ 3D upgrade. `Default` is fully
/// vanilla: `enabled == false`.
#[derive(Clone, Debug, PartialEq)]
pub struct VanillaPlus3dConfig {
    /// Master opt-in switch. **`false` by default** — the upgrade never
    /// changes the vanilla look unless explicitly enabled.
    pub enabled: bool,
    /// Upgrade the Sun and Moon to voxel 3D bodies.
    pub celestial_3d: bool,
    /// Upgrade cross-plane crops, flowers, saplings and grass sprites.
    pub crops_and_flora: bool,
    /// Upgrade amethyst buds/clusters, pointed dripstone, sea pickles and
    /// similar crystal sprites.
    pub crystals_and_clusters: bool,
    /// Upgrade rails and ladders.
    pub rails_and_ladders: bool,
    /// Upgrade chains, iron bars, torches and lanterns.
    pub fixtures_and_chains: bool,
    /// Raise exposed ore vein texels by half a texel for a subtle 3D
    /// mineral-fleck relief.
    pub ores_3d_relief: bool,
    /// Distance in blocks beyond which the flat 2D model is used. Clamped to
    /// `8.0..=64.0` when read via [`Self::effective_lod_cutoff`].
    pub lod_cutoff_blocks: f32,
    /// When `true` (default), blocks whose resource-pack stack already
    /// provides a custom 3D model are left alone entirely.
    pub respect_custom_resourcepack_models: bool,
}

impl Default for VanillaPlus3dConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            celestial_3d: true,
            crops_and_flora: true,
            crystals_and_clusters: true,
            rails_and_ladders: true,
            fixtures_and_chains: true,
            ores_3d_relief: true,
            lod_cutoff_blocks: DEFAULT_LOD_CUTOFF_BLOCKS,
            respect_custom_resourcepack_models: true,
        }
    }
}

impl VanillaPlus3dConfig {
    /// LOD cutoff clamped to `8.0..=64.0` blocks.
    pub fn effective_lod_cutoff(&self) -> f32 {
        self.lod_cutoff_blocks
            .clamp(MIN_LOD_CUTOFF_BLOCKS, MAX_LOD_CUTOFF_BLOCKS)
    }

    /// Whether the per-category toggle for `category` is on.
    pub fn category_enabled(&self, category: VanillaPlusCategory) -> bool {
        match category {
            VanillaPlusCategory::Celestial3d => self.celestial_3d,
            VanillaPlusCategory::CropsAndFlora => self.crops_and_flora,
            VanillaPlusCategory::CrystalsAndClusters => self.crystals_and_clusters,
            VanillaPlusCategory::RailsAndLadders => self.rails_and_ladders,
            VanillaPlusCategory::FixturesAndChains => self.fixtures_and_chains,
            VanillaPlusCategory::Ores3dRelief => self.ores_3d_relief,
        }
    }
}

/// What the mesher should do for one block occurrence.
#[derive(Clone, Debug, PartialEq)]
pub enum MeshUpgradeDecision {
    /// Keep the vanilla flat model (feature off, unclassified block, or a
    /// disabled category).
    Use2dVanilla,
    /// A custom 3D resource-pack model exists; defer to it untouched.
    DeferToResourcePack,
    /// Inside an upgrade-eligible category but beyond `lod_cutoff_blocks`:
    /// keep the cheap 2D model for this far block.
    Use2dLodFallback,
    /// Build a procedural 3D upgrade mesh for the given category.
    Upgrade3d(VanillaPlusCategory),
}

/// Caller-supplied facts about the resolved model for one block occurrence.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct BlockModelHint {
    /// The active resource-pack stack bakes a custom 3D model for this block.
    pub has_custom_3d_resourcepack_model: bool,
    /// The resolved model is a flat cross ("X" or "#" planes with no
    /// thickness), i.e. the shape this upgrade knows how to thicken. Set for
    /// modded blocks with no exact id match.
    pub is_cross_model: bool,
}

/// Combines config, block identity and distance into a mesh decision.
///
/// Evaluation order: master switch -> resource-pack deference ->
/// classification -> per-category toggle -> distance LOD.
pub fn vanilla_plus_decision(
    config: &VanillaPlus3dConfig,
    block_id: &str,
    hint: &BlockModelHint,
    distance_blocks: f32,
) -> MeshUpgradeDecision {
    if !config.enabled {
        return MeshUpgradeDecision::Use2dVanilla;
    }
    if config.respect_custom_resourcepack_models && hint.has_custom_3d_resourcepack_model {
        return MeshUpgradeDecision::DeferToResourcePack;
    }
    let Some(category) = classify_vanilla_plus_block(block_id, hint.is_cross_model) else {
        return MeshUpgradeDecision::Use2dVanilla;
    };
    if !config.category_enabled(category) {
        return MeshUpgradeDecision::Use2dVanilla;
    }
    if distance_blocks > config.effective_lod_cutoff() {
        return MeshUpgradeDecision::Use2dLodFallback;
    }
    MeshUpgradeDecision::Upgrade3d(category)
}

/// Classifies a block id into a Vanilla+ category.
///
/// `block_id` may be namespaced (`"minecraft:wheat"` or `"mymod:foo"`); only
/// the path component is matched. Vanilla ids map to their category; a modded
/// `is_cross_model` block with no exact or pattern match falls back
/// generically to [`VanillaPlusCategory::CropsAndFlora`]. The strings `"sun"`
/// and `"moon"` classify as [`VanillaPlusCategory::Celestial3d`]. Any id
/// ending in `_ore` (plus `ancient_debris`) classifies as
/// [`VanillaPlusCategory::Ores3dRelief`].
pub fn classify_vanilla_plus_block(
    block_id: &str,
    is_cross_model: bool,
) -> Option<VanillaPlusCategory> {
    use VanillaPlusCategory::*;
    let id = block_id.rsplit(':').next().unwrap_or(block_id);

    match id {
        "sun" | "moon" => return Some(Celestial3d),
        "rail" | "powered_rail" | "detector_rail" | "activator_rail" | "ladder" => {
            return Some(RailsAndLadders)
        }
        "amethyst_cluster" | "small_amethyst_bud" | "medium_amethyst_bud"
        | "large_amethyst_bud" | "pointed_dripstone" | "sea_pickle" => {
            return Some(CrystalsAndClusters)
        }
        "chain" | "iron_bars" | "torch" | "soul_torch" | "redstone_torch" | "wall_torch"
        | "soul_wall_torch" | "redstone_wall_torch" | "lantern" | "soul_lantern" => {
            return Some(FixturesAndChains)
        }
        _ => {}
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
    const FLORA: &[&str] = &[
        "dandelion",
        "poppy",
        "blue_orchid",
        "allium",
        "azure_bluet",
        "red_tulip",
        "orange_tulip",
        "white_tulip",
        "pink_tulip",
        "oxeye_daisy",
        "cornflower",
        "lily_of_the_valley",
        "torchflower",
        "pink_petals",
        "wither_rose",
        "sunflower",
        "lilac",
        "rose_bush",
        "peony",
        "pitcher_plant",
        "grass",
        "short_grass",
        "tall_grass",
        "fern",
        "large_fern",
        "dead_bush",
        "crimson_fungus",
        "warped_fungus",
        "crimson_roots",
        "warped_roots",
        "nether_sprouts",
        "brown_mushroom",
        "red_mushroom",
        "hanging_roots",
        "spore_blossom",
        "small_dripleaf",
        "big_dripleaf",
    ];

    if CROPS.contains(&id)
        || FLORA.contains(&id)
        || id.ends_with("_sapling")
        || id.ends_with("_stem")
        || id.ends_with("_fungus")
        || id.ends_with("_roots")
    {
        return Some(CropsAndFlora);
    }
    if id.contains("crystal") || id.ends_with("_cluster") || id.ends_with("_bud") {
        return Some(CrystalsAndClusters);
    }
    if id.ends_with("_ore") || id == "ancient_debris" {
        return Some(Ores3dRelief);
    }
    if is_cross_model {
        return Some(CropsAndFlora);
    }
    None
}

// ---------------------------------------------------------------------------
// Geometry primitives
// ---------------------------------------------------------------------------

/// One upgraded quad: four outward-counterclockwise vertex positions in
/// block-local `0..=1` space, a unit outward normal, `0..=1` UVs, and an
/// optional tint tag (`-1` = none; rails use `0`/`1` for unpowered/powered).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ExtrudedQuad {
    pub positions: [[f32; 3]; 4],
    pub normal: [f32; 3],
    pub uv: [[f32; 2]; 4],
    pub tint_index: i32,
}

/// An upgraded mesh: a bounded list of [`ExtrudedQuad`]s.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ExtrudedMesh {
    pub quads: Vec<ExtrudedQuad>,
}

impl ExtrudedMesh {
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of emitted quads.
    pub fn quad_count(&self) -> usize {
        self.quads.len()
    }

    /// Push one quad, computing its normal from the winding. Degenerate
    /// (zero-area) quads are dropped; emission stops at
    /// [`MAX_UPGRADE_QUADS`].
    fn push(&mut self, positions: [[f32; 3]; 4], uv: [[f32; 2]; 4], tint_index: i32) {
        if self.quads.len() >= MAX_UPGRADE_QUADS {
            return;
        }
        let Some(normal) = quad_normal(&positions) else {
            return;
        };
        self.quads.push(ExtrudedQuad {
            positions,
            normal,
            uv,
            tint_index,
        });
    }

    /// Append a copy of `other`'s quads (bounded by [`MAX_UPGRADE_QUADS`]).
    pub fn append(&mut self, other: &ExtrudedMesh) {
        for q in &other.quads {
            if self.quads.len() >= MAX_UPGRADE_QUADS {
                return;
            }
            self.quads.push(*q);
        }
    }

    /// Translate the whole mesh by `off`.
    pub fn translate(&mut self, off: [f32; 3]) {
        for q in &mut self.quads {
            for p in &mut q.positions {
                p[0] += off[0];
                p[1] += off[1];
                p[2] += off[2];
            }
        }
    }

    /// Apply `p' = M * (p - pivot) + target` to positions and `n' = M * n`
    /// (renormalized) to normals. `M` should be a rotation or a
    /// rotation-times-uniform-scale product so winding and outwardness are
    /// preserved.
    pub fn affine(&mut self, m: [[f32; 3]; 3], pivot: [f32; 3], target: [f32; 3]) {
        for q in &mut self.quads {
            for p in &mut q.positions {
                let u = [p[0] - pivot[0], p[1] - pivot[1], p[2] - pivot[2]];
                *p = [
                    target[0] + m[0][0] * u[0] + m[0][1] * u[1] + m[0][2] * u[2],
                    target[1] + m[1][0] * u[0] + m[1][1] * u[1] + m[1][2] * u[2],
                    target[2] + m[2][0] * u[0] + m[2][1] * u[1] + m[2][2] * u[2],
                ];
            }
            let n = [
                m[0][0] * q.normal[0] + m[0][1] * q.normal[1] + m[0][2] * q.normal[2],
                m[1][0] * q.normal[0] + m[1][1] * q.normal[1] + m[1][2] * q.normal[2],
                m[2][0] * q.normal[0] + m[2][1] * q.normal[1] + m[2][2] * q.normal[2],
            ];
            q.normal = normalize_or(n, q.normal);
        }
    }
}

fn sub(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}

fn add_scaled(a: [f32; 3], u: [f32; 3], s: f32) -> [f32; 3] {
    [a[0] + u[0] * s, a[1] + u[1] * s, a[2] + u[2] * s]
}

fn cross(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}

fn dot(a: [f32; 3], b: [f32; 3]) -> f32 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

fn norm(a: [f32; 3]) -> f32 {
    dot(a, a).sqrt()
}

fn normalize_or(a: [f32; 3], fallback: [f32; 3]) -> [f32; 3] {
    let l = norm(a);
    if l > 1e-12 {
        [a[0] / l, a[1] / l, a[2] / l]
    } else {
        fallback
    }
}

/// Outward normal of a CCW-wound convex quad, or `None` when degenerate.
fn quad_normal(p: &[[f32; 3]; 4]) -> Option<[f32; 3]> {
    let n = cross(sub(p[1], p[0]), sub(p[2], p[0]));
    let l = norm(n);
    (l > 1e-12).then(|| [n[0] / l, n[1] / l, n[2] / l])
}

/// A beam/frustum between two rectangular cross-sections. `u*`/`v*` are the
/// half-extent edge vectors of each end ring; `c1 - c0` is the beam axis.
struct Frustum {
    c0: [f32; 3],
    c1: [f32; 3],
    u0: [f32; 3],
    v0: [f32; 3],
    u1: [f32; 3],
    v1: [f32; 3],
}

/// Model-projected UVs for one frustum face: each vertex's UV is its actual
/// block-local position projected onto the face's dominant axis plane and
/// clamped to `0..=1`, the same projection a block-model baker applies to
/// element faces. Side faces map `(u, v) = (horizontal tangent, 1 - y)` —
/// `u` increases toward the viewer's right and `v = 0` sits at the texture
/// top — while horizontal faces map `(u, v) = (x, z)`. A centred fixture
/// therefore samples the centre of its texture instead of anchoring every
/// face at the `(0, 0)` corner.
fn projected_face_uvs(p: &[[f32; 3]; 4]) -> [[f32; 2]; 4] {
    let n = quad_normal(p).unwrap_or([0.0, 0.0, 1.0]);
    let (ax, ay, az) = (n[0].abs(), n[1].abs(), n[2].abs());
    let uv_at = |pt: [f32; 3]| -> [f32; 2] {
        let uv = if ay >= ax && ay >= az {
            [pt[0], pt[2]]
        } else if az >= ax {
            // Normal mostly +/-z: `u` along x, mirrored on the north (-z)
            // side so the texture is never seen backwards.
            [
                if n[2] < 0.0 { 1.0 - pt[0] } else { pt[0] },
                1.0 - pt[1],
            ]
        } else {
            // Normal mostly +/-x: `u` along z, mirrored on the east (+x)
            // side so the texture is never seen backwards.
            [
                if n[0] > 0.0 { 1.0 - pt[2] } else { pt[2] },
                1.0 - pt[1],
            ]
        };
        [uv[0].clamp(0.0, 1.0), uv[1].clamp(0.0, 1.0)]
    };
    let mut uv = [[0.0; 2]; 4];
    for (i, pt) in p.iter().enumerate() {
        uv[i] = uv_at(*pt);
    }
    uv
}

/// Emits one frustum: 4 side quads + 2 end caps, all outward-CCW. Equal end
/// widths produce an ordinary (possibly sloped) box; shrunken ends produce a
/// tapered pixel-art facet.
fn emit_frustum(mesh: &mut ExtrudedMesh, f: &Frustum, tint_index: i32) {
    const SIGNS: [(f32, f32); 4] = [(-1.0, -1.0), (1.0, -1.0), (1.0, 1.0), (-1.0, 1.0)];
    let axis = sub(f.c1, f.c0);
    // Keep (u, v, axis) right-handed so the ring order below faces outward.
    let flip = dot(cross(f.u0, f.v0), axis) < 0.0;
    let neg = |v: [f32; 3]| [-v[0], -v[1], -v[2]];
    let (u0, u1) = if flip { (neg(f.u0), neg(f.u1)) } else { (f.u0, f.u1) };

    let ring = |c: [f32; 3], u: [f32; 3], v: [f32; 3]| -> [[f32; 3]; 4] {
        let mut r = [[0.0; 3]; 4];
        for (i, &(a, b)) in SIGNS.iter().enumerate() {
            r[i] = add_scaled(add_scaled(c, u, a), v, b);
        }
        r
    };
    let r0 = ring(f.c0, u0, f.v0);
    let r1 = ring(f.c1, u1, f.v1);

    for i in 0..4 {
        let j = (i + 1) % 4;
        let positions = [r0[i], r0[j], r1[j], r1[i]];
        mesh.push(positions, projected_face_uvs(&positions), tint_index);
    }
    let cap1 = [r1[0], r1[1], r1[2], r1[3]];
    mesh.push(cap1, projected_face_uvs(&cap1), tint_index);
    let cap0 = [r0[3], r0[2], r0[1], r0[0]];
    mesh.push(cap0, projected_face_uvs(&cap0), tint_index);
}

/// Emits an axis-aligned box (`from`/`to` corners), all six faces outward.
/// Every face's UVs are projected from its actual `[from, to]` coordinates
/// (clamped to `0..=1`) via [`projected_face_uvs`], so a small centred box
/// samples the centre of its texture rather than the `(0, 0)` corner.
fn push_box(mesh: &mut ExtrudedMesh, from: [f32; 3], to: [f32; 3], tint_index: i32) {
    let cx = (from[0] + to[0]) * 0.5;
    let cz = (from[2] + to[2]) * 0.5;
    let (y0, y1) = (from[1].min(to[1]), from[1].max(to[1]));
    let hx = (to[0] - from[0]).abs() * 0.5;
    let hz = (to[2] - from[2]).abs() * 0.5;
    emit_frustum(
        mesh,
        &Frustum {
            c0: [cx, y0, cz],
            c1: [cx, y1, cz],
            u0: [hx, 0.0, 0.0],
            v0: [0.0, 0.0, -hz],
            u1: [hx, 0.0, 0.0],
            v1: [0.0, 0.0, -hz],
        },
        tint_index,
    );
}

/// Emits a tapered "pixel crystal" spike: a frustum from `base` along `dir`
/// for `len` blocks, shrinking from width `w0` to `w1`.
fn emit_spike(
    mesh: &mut ExtrudedMesh,
    base: [f32; 3],
    dir: [f32; 3],
    len: f32,
    w0: f32,
    w1: f32,
    tint_index: i32,
) {
    let d = normalize_or(dir, [0.0, 1.0, 0.0]);
    let seed = if d[2].abs() < 0.9 { [0.0, 0.0, 1.0] } else { [1.0, 0.0, 0.0] };
    let u = normalize_or(cross(d, seed), [1.0, 0.0, 0.0]);
    // u x v = d holds exactly for v = d x u with unit u perpendicular to d.
    let v = normalize_or(cross(d, u), [0.0, 0.0, 1.0]);
    // Tilted spikes have base rings that are not flush with the floor; lift
    // the base just enough that no ring vertex dips below y = 0.
    let lift = w0 * 0.5 * (u[1].abs() + v[1].abs());
    let base = [base[0], base[1].max(lift), base[2]];
    let s = |v: [f32; 3], k: f32| [v[0] * k, v[1] * k, v[2] * k];
    emit_frustum(
        mesh,
        &Frustum {
            c0: base,
            c1: add_scaled(base, d, len),
            u0: s(u, w0 * 0.5),
            v0: s(v, w0 * 0.5),
            u1: s(u, w1 * 0.5),
            v1: s(v, w1 * 0.5),
        },
        tint_index,
    );
}

/// Emits a horizontal beam with a rectangular cross-section (`w` across the
/// track plane, `h` tall) between two points — used for rail bars and ties.
fn emit_beam(
    mesh: &mut ExtrudedMesh,
    c0: [f32; 3],
    c1: [f32; 3],
    w: f32,
    h: f32,
    tint_index: i32,
) {
    let d = sub(c1, c0);
    let perp = normalize_or(cross(d, [0.0, 1.0, 0.0]), [0.0, 0.0, 1.0]);
    let s = |v: [f32; 3], k: f32| [v[0] * k, v[1] * k, v[2] * k];
    emit_frustum(
        mesh,
        &Frustum {
            c0,
            c1,
            u0: s(perp, w * 0.5),
            v0: [0.0, h * 0.5, 0.0],
            u1: s(perp, w * 0.5),
            v1: [0.0, h * 0.5, 0.0],
        },
        tint_index,
    );
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

/// Rotate a mesh by `k` quarter turns about the block-centre vertical axis.
fn yaw(mesh: &mut ExtrudedMesh, quarter_turns: i32) {
    if quarter_turns.rem_euclid(4) == 0 {
        return;
    }
    mesh.affine(
        yaw_matrix(quarter_turns),
        [0.5, 0.0, 0.5],
        [0.5, 0.0, 0.5],
    );
}

/// Rotation matrix mapping local up-space (`+Y` = growth direction, base at
/// the `y = 0` face) onto the attachment direction of a wall/floor/ceiling
/// fixture. Only 90-degree rotations, so facets stay axis-aligned.
fn direction_rotation(dir: Direction) -> [[f32; 3]; 3] {
    match dir {
        Direction::Up => [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]],
        Direction::Down => [[1.0, 0.0, 0.0], [0.0, -1.0, 0.0], [0.0, 0.0, -1.0]],
        Direction::North => [[1.0, 0.0, 0.0], [0.0, 0.0, 1.0], [0.0, -1.0, 0.0]],
        Direction::South => [[1.0, 0.0, 0.0], [0.0, 0.0, -1.0], [0.0, 1.0, 0.0]],
        Direction::East => [[0.0, 1.0, 0.0], [-1.0, 0.0, 0.0], [0.0, 0.0, 1.0]],
        Direction::West => [[0.0, -1.0, 0.0], [1.0, 0.0, 0.0], [0.0, 0.0, 1.0]],
    }
}

/// Deterministic per-block micro-offset (in `+/- JITTER_AMP`) from an integer
/// position hash — no RNG, no clock, stable across rebuilds.
fn block_jitter(block: [i32; 3]) -> [f32; 2] {
    let mut h = (block[0] as u32).wrapping_mul(0x8da6_b343)
        ^ (block[1] as u32).wrapping_mul(0xd816_3841)
        ^ (block[2] as u32).wrapping_mul(0xcb1a_b31f);
    h ^= h >> 15;
    h = h.wrapping_mul(0x9e37_79b9);
    h ^= h >> 13;
    let jx = ((h & 0xff) as f32 / 255.0 - 0.5) * 2.0 * JITTER_AMP;
    let jz = (((h >> 8) & 0xff) as f32 / 255.0 - 0.5) * 2.0 * JITTER_AMP;
    [jx, jz]
}

// ---------------------------------------------------------------------------
// Generic texel-grid sprite extruder
// ---------------------------------------------------------------------------

/// Largest sprite dimension [`extrude_alpha_mask`] accepts: covers the
/// classic 16x16 packs and 32/64-texel HD packs while staying bounded.
pub const MAX_MASK_DIMENSION: usize = 128;

/// Extrudes a `width x height` alpha/opacity mask (`alpha[y * width + x]`,
/// row 0 = sprite top) into a plate `t` blocks thick lying in `z in [0, t]`.
///
/// Front and back faces are greedy-merged over the opaque mask; side rim
/// quads are emitted only along opaque->transparent boundaries and merged
/// into runs, so hidden interior faces never exist. A single opaque texel
/// yields exactly 6 quads; a solid sprite yields exactly 6 quads. Works for
/// any vanilla or modded sprite mask — no hand-authored JSON needed.
///
/// `width`/`height` must each be in `1..=128` and `alpha.len()` must equal
/// `width * height`; anything else fails closed to an empty mesh. `t` is
/// clamped to `0..=1`; `TEXEL`-scale thickness remains the caller's choice.
pub fn extrude_alpha_mask(width: usize, height: usize, alpha: &[u8], t: f32) -> ExtrudedMesh {
    let mut mesh = ExtrudedMesh::new();
    let t = t.clamp(0.0, 1.0);
    if t <= 0.0
        || width == 0
        || height == 0
        || width > MAX_MASK_DIMENSION
        || height > MAX_MASK_DIMENSION
        || alpha.len() != width * height
    {
        return mesh;
    }
    let texel_w = 1.0 / width as f32;
    let texel_h = 1.0 / height as f32;
    let opaque = |x: usize, y: usize| alpha[y * width + x] >= ALPHA_CUTOFF;

    // Greedy-merge opaque rectangles; emit front + back faces per rectangle.
    let mut done = vec![false; width * height];
    for ty in 0..height {
        for tx in 0..width {
            if !opaque(tx, ty) || done[ty * width + tx] {
                continue;
            }
            let mut w = 1usize;
            while tx + w < width && opaque(tx + w, ty) && !done[ty * width + tx + w] {
                w += 1;
            }
            let mut h = 1usize;
            'grow: while ty + h < height {
                for xx in tx..tx + w {
                    if !opaque(xx, ty + h) || done[(ty + h) * width + xx] {
                        break 'grow;
                    }
                }
                h += 1;
            }
            for yy in ty..ty + h {
                for xx in tx..tx + w {
                    done[yy * width + xx] = true;
                }
            }
            let x0 = tx as f32 * texel_w;
            let x1 = (tx + w) as f32 * texel_w;
            let y_hi = 1.0 - ty as f32 * texel_h;
            let y_lo = 1.0 - (ty + h) as f32 * texel_h;
            let (u0, u1) = (x0, x1);
            let (v0, v1) = (ty as f32 * texel_h, (ty + h) as f32 * texel_h);
            // Front (z = 0, normal -Z); UVs mirrored like vanilla's far side.
            mesh.push(
                [
                    [x0, y_lo, 0.0],
                    [x0, y_hi, 0.0],
                    [x1, y_hi, 0.0],
                    [x1, y_lo, 0.0],
                ],
                [[u0, v1], [u0, v0], [u1, v0], [u1, v1]],
                -1,
            );
            // Back (z = t, normal +Z).
            mesh.push(
                [
                    [x0, y_lo, t],
                    [x1, y_lo, t],
                    [x1, y_hi, t],
                    [x0, y_hi, t],
                ],
                [[u0, v1], [u1, v1], [u1, v0], [u0, v0]],
                -1,
            );
        }
    }

    // Vertical rims: per column, runs of cells whose left/right neighbour is
    // transparent merge into one quad each. UVs sample inside column `tx`
    // (`tx * texel_w .. (tx + 1) * texel_w`) so the rim shows the texels it
    // bounds instead of the sprite's transparent left margin.
    for tx in 0..width {
        for &left_side in &[true, false] {
            let mut run: Option<usize> = None;
            for ty in 0..=height {
                let edge = ty < height
                    && opaque(tx, ty)
                    && if left_side {
                        tx == 0 || !opaque(tx - 1, ty)
                    } else {
                        tx == width - 1 || !opaque(tx + 1, ty)
                    };
                match (edge, run) {
                    (true, None) => run = Some(ty),
                    (false, Some(start)) => {
                        let x_e = if left_side {
                            tx as f32 * texel_w
                        } else {
                            (tx + 1) as f32 * texel_w
                        };
                        let y_hi = 1.0 - start as f32 * texel_h;
                        let y_lo = 1.0 - ty as f32 * texel_h;
                        let (v0, v1) = (start as f32 * texel_h, ty as f32 * texel_h);
                        let (u0, u1) = (tx as f32 * texel_w, (tx + 1) as f32 * texel_w);
                        let quad = if left_side {
                            [
                                [x_e, y_lo, t],
                                [x_e, y_hi, t],
                                [x_e, y_hi, 0.0],
                                [x_e, y_lo, 0.0],
                            ]
                        } else {
                            [
                                [x_e, y_lo, 0.0],
                                [x_e, y_hi, 0.0],
                                [x_e, y_hi, t],
                                [x_e, y_lo, t],
                            ]
                        };
                        mesh.push(quad, [[u0, v1], [u0, v0], [u1, v0], [u1, v1]], -1);
                        run = None;
                    }
                    _ => {}
                }
            }
        }
    }

    // Horizontal rims: per row, runs of cells whose top/bottom neighbour is
    // transparent merge into one quad each. UVs sample inside row `ty`
    // (`ty * texel_h .. (ty + 1) * texel_h`) so the rim shows the texels it
    // bounds instead of the sprite's transparent top margin.
    for ty in 0..height {
        for &top_side in &[true, false] {
            let mut run: Option<usize> = None;
            for tx in 0..=width {
                let edge = tx < width
                    && opaque(tx, ty)
                    && if top_side {
                        ty == 0 || !opaque(tx, ty - 1)
                    } else {
                        ty == height - 1 || !opaque(tx, ty + 1)
                    };
                match (edge, run) {
                    (true, None) => run = Some(tx),
                    (false, Some(start)) => {
                        let y_e = if top_side {
                            1.0 - ty as f32 * texel_h
                        } else {
                            1.0 - (ty + 1) as f32 * texel_h
                        };
                        let x0 = start as f32 * texel_w;
                        let x1 = tx as f32 * texel_w;
                        let (v0, v1) = (ty as f32 * texel_h, (ty + 1) as f32 * texel_h);
                        let quad = if top_side {
                            [
                                [x0, y_e, 0.0],
                                [x0, y_e, t],
                                [x1, y_e, t],
                                [x1, y_e, 0.0],
                            ]
                        } else {
                            [
                                [x0, y_e, 0.0],
                                [x1, y_e, 0.0],
                                [x1, y_e, t],
                                [x0, y_e, t],
                            ]
                        };
                        mesh.push(
                            quad,
                            [[x0, v0], [x0, v1], [x1, v1], [x1, v0]],
                            -1,
                        );
                        run = None;
                    }
                    _ => {}
                }
            }
        }
    }

    mesh
}

/// Extrudes a `16x16` alpha/opacity mask (`alpha[y * 16 + x]`, row 0 =
/// sprite top) into a plate `t` blocks thick lying in `z in [0, t]`.
///
/// Thin wrapper over [`extrude_alpha_mask`] for the classic sprite size;
/// see it for the full contract.
pub fn extrude_alpha_mask_16x16(alpha: &[u8; 256], t: f32) -> ExtrudedMesh {
    extrude_alpha_mask(16, 16, alpha, t)
}

// ---------------------------------------------------------------------------
// Cross-plane crops / flora
// ---------------------------------------------------------------------------

/// Blade arrangement for a cross-plane plant.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CrossLayout {
    /// Two intersecting diagonal blades (45 degrees / 135 degrees), the
    /// classic flower/grass cross.
    Diagonal,
    /// Four axis-parallel blades in a `#` grid, the classic wheat/carrot
    /// crop field layout.
    HashGrid,
}

/// Builds a 3D cross mesh from any `16x16` sprite alpha mask: the mask is
/// extruded one texel thick and arranged into blades per `layout`, with a
/// deterministic sub-texel offset derived from `block` so neighbouring plants
/// do not z-fight identically. All geometry stays strictly inside `0..=1`.
pub fn build_3d_cross_mesh(
    alpha: &[u8; 256],
    layout: CrossLayout,
    block: [i32; 3],
) -> ExtrudedMesh {
    let sprite = extrude_alpha_mask_16x16(alpha, TEXEL);
    let mut mesh = ExtrudedMesh::new();
    let [jx, jz] = block_jitter(block);
    match layout {
        CrossLayout::Diagonal => {
            let c = FRAC_1_SQRT_2;
            // Blade length chosen so rotated corners plus the jitter margin
            // still land strictly inside the block.
            let s = 2.0 * SQRT_2 * (0.5 - JITTER_AMP - 0.01) - TEXEL;
            for (bi, &sgn) in [1.0f32, -1.0].iter().enumerate() {
                // The two diagonal blades intersect at the block centre;
                // lift the second one by `+0.0005` and shrink its height by
                // `0.001` (y -> 0.0005 + 0.999 * y) so their faces never sit
                // coplanar while every vertex stays inside `0..=1`.
                let (y_row, y_target) = if bi == 1 {
                    ([0.0, 0.999, 0.0], 0.0005)
                } else {
                    ([0.0, 1.0, 0.0], 0.0)
                };
                let m = [
                    [s * c, 0.0, sgn * c],
                    y_row,
                    [-sgn * s * c, 0.0, c],
                ];
                let mut blade = sprite.clone();
                blade.affine(m, [0.5, 0.0, TEXEL * 0.5], [0.5 + jx, y_target, 0.5 + jz]);
                mesh.append(&blade);
            }
        }
        CrossLayout::HashGrid => {
            let d = 1.0 / 6.0;
            // Blades span slightly less than the full block so the jitter
            // shift can never push a vertex outside `0..=1`.
            let s = 1.0 - 2.0 * JITTER_AMP - 0.02;
            let rot90_scaled = [[0.0, 0.0, 1.0], [0.0, 1.0, 0.0], [-s, 0.0, 0.0]];
            for &sgn in &[1.0f32, -1.0] {
                let mut a = sprite.clone();
                a.affine(
                    [[s, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]],
                    [0.5, 0.0, TEXEL * 0.5],
                    [0.5 + jx, 0.0, 0.5 + sgn * d + jz],
                );
                mesh.append(&a);
                let mut b = sprite.clone();
                b.affine(
                    rot90_scaled,
                    [0.5, 0.0, TEXEL * 0.5],
                    [0.5 + sgn * d + jx, 0.0, 0.5 + jz],
                );
                mesh.append(&b);
            }
        }
    }
    mesh
}

// ---------------------------------------------------------------------------
// Crystals: amethyst buds / clusters, dripstone, sea pickles
// ---------------------------------------------------------------------------

/// Growth stage of a crystal formation; scales height, width and spike count.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CrystalStage {
    SmallBud,
    MediumBud,
    LargeBud,
    Cluster,
}

/// Builds a stepped pixel-art crystal prism cluster whose base sits on the
/// face opposite `facing` and points along `facing` (matching the attachment
/// convention of buds/dripstone). Facets are flat, axis-aligned or 45-ish
/// degree surfaces — blocky, never smooth.
pub fn build_3d_crystal_mesh(stage: CrystalStage, facing: Direction) -> ExtrudedMesh {
    let mut mesh = ExtrudedMesh::new();
    let centre = [0.5, 0.0, 0.5];
    match stage {
        CrystalStage::SmallBud => emit_spike(&mut mesh, centre, [0.0, 1.0, 0.0], 0.22, 0.20, 0.06, -1),
        CrystalStage::MediumBud => {
            emit_spike(&mut mesh, centre, [0.0, 1.0, 0.0], 0.34, 0.26, 0.08, -1)
        }
        CrystalStage::LargeBud => {
            emit_spike(&mut mesh, centre, [0.0, 1.0, 0.0], 0.48, 0.34, 0.10, -1)
        }
        CrystalStage::Cluster => {
            emit_spike(&mut mesh, centre, [0.0, 1.0, 0.0], 0.80, 0.42, 0.14, -1);
            // Four shorter tilted side spikes — the iconic cluster silhouette.
            for &(sx, sz) in &[(1.0f32, 0.0f32), (-1.0, 0.0), (0.0, 1.0), (0.0, -1.0)] {
                emit_spike(
                    &mut mesh,
                    [0.5 + sx * 0.13, 0.0, 0.5 + sz * 0.13],
                    [sx * 0.55, 1.0, sz * 0.55],
                    0.40,
                    0.18,
                    0.05,
                    -1,
                );
            }
        }
    }
    if facing != Direction::Up {
        mesh.affine(direction_rotation(facing), [0.5, 0.5, 0.5], [0.5, 0.5, 0.5]);
    }
    mesh
}

// ---------------------------------------------------------------------------
// Rails & ladders
// ---------------------------------------------------------------------------

/// Vanilla rail shapes the 3D upgrade knows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RailShape {
    NorthSouth,
    EastWest,
    AscendingNorth,
    AscendingSouth,
    AscendingEast,
    AscendingWest,
    CurveSouthEast,
    CurveSouthWest,
    CurveNorthWest,
    CurveNorthEast,
}

const RAIL_GAUGE: f32 = 0.21875;
const RAIL_BAR_W: f32 = 0.125;
const RAIL_BAR_H: f32 = 0.125;
const RAIL_BAR_Y: f32 = 0.10;
const RAIL_TIE_W: f32 = 0.15;
const RAIL_TIE_H: f32 = 0.07;
const RAIL_TIE_Y: f32 = 0.035;
/// Tiny lift applied to curved-rail chord beams so their horizontal faces
/// never share a plane with the straight segments they join.
const RAIL_CHORD_LIFT: f32 = 0.0005;

/// Straight east-west track: two raised bars plus five sleepers.
fn rail_straight_ew(mesh: &mut ExtrudedMesh, tint: i32) {
    for &sgn in &[1.0f32, -1.0] {
        let z = 0.5 + sgn * RAIL_GAUGE;
        emit_beam(mesh, [0.01, RAIL_BAR_Y, z], [0.99, RAIL_BAR_Y, z], RAIL_BAR_W, RAIL_BAR_H, tint);
    }
    for i in 0..5 {
        let x = 0.10 + i as f32 * 0.20;
        emit_beam(
            mesh,
            [x, RAIL_TIE_Y, 0.06],
            [x, RAIL_TIE_Y, 0.94],
            RAIL_TIE_W,
            RAIL_TIE_H,
            -1,
        );
    }
}

/// Ascending track rising toward +X; stepped sleepers keep it pixel-art.
fn rail_ascending_east(mesh: &mut ExtrudedMesh, tint: i32) {
    for &sgn in &[1.0f32, -1.0] {
        let z = 0.5 + sgn * RAIL_GAUGE;
        emit_beam(mesh, [0.01, 0.12, z], [0.99, 0.92, z], RAIL_BAR_W, RAIL_BAR_H, tint);
    }
    for i in 0..6 {
        let x = 0.08 + i as f32 * 0.164;
        let y = RAIL_TIE_Y + x * 0.82;
        emit_beam(mesh, [x, y, 0.10], [x, y, 0.90], RAIL_TIE_W, RAIL_TIE_H, -1);
    }
}

/// 90-degree bend joining the south edge midpoint to the east edge midpoint:
/// each rail is a straight segment, a 45-degree chord, then another straight.
fn rail_curve_se(mesh: &mut ExtrudedMesh, tint: i32) {
    let (inner, outer) = (0.5 - RAIL_GAUGE, 0.5 + RAIL_GAUGE);
    // The chord beams cross the straight segments' end caps; lift them by
    // `RAIL_CHORD_LIFT` so their top/bottom faces never share a plane with
    // the straights' (coplanar overlap would z-fight).
    let chord_y = RAIL_BAR_Y + RAIL_CHORD_LIFT;
    // Inner rail hugs the block centre.
    emit_beam(mesh, [inner, RAIL_BAR_Y, 0.99], [inner, RAIL_BAR_Y, 0.60], RAIL_BAR_W, RAIL_BAR_H, tint);
    emit_beam(mesh, [inner, chord_y, 0.60], [0.60, chord_y, inner], RAIL_BAR_W, RAIL_BAR_H, tint);
    emit_beam(mesh, [0.60, RAIL_BAR_Y, inner], [0.99, RAIL_BAR_Y, inner], RAIL_BAR_W, RAIL_BAR_H, tint);
    // Outer rail bends tighter, hugging the block corner: short legs plus a
    // small chord. The previous wide chord forced both legs to span the
    // `(outer, outer)` corner square, where they overlapped volumetrically
    // and shared coplanar top/bottom faces (z-fighting).
    emit_beam(mesh, [outer, RAIL_BAR_Y, 0.99], [outer, RAIL_BAR_Y, 0.80], RAIL_BAR_W, RAIL_BAR_H, tint);
    emit_beam(mesh, [outer, chord_y, 0.80], [0.80, chord_y, outer], RAIL_BAR_W, RAIL_BAR_H, tint);
    emit_beam(mesh, [0.80, RAIL_BAR_Y, outer], [0.99, RAIL_BAR_Y, outer], RAIL_BAR_W, RAIL_BAR_H, tint);
    // Ties: one per straight leg plus a diagonal tie under the bend. Both
    // straight ties stop at `0.86 - RAIL_TIE_W` so their half-widths never
    // overlap near the `(0.86, 0.86)` corner — overlapping sleepers would
    // share coplanar top/bottom faces and z-fight.
    let tie_end = 0.86 - RAIL_TIE_W;
    emit_beam(mesh, [0.15, RAIL_TIE_Y, 0.86], [tie_end, RAIL_TIE_Y, 0.86], RAIL_TIE_W, RAIL_TIE_H, -1);
    emit_beam(mesh, [0.86, RAIL_TIE_Y, 0.15], [0.86, RAIL_TIE_Y, tie_end], RAIL_TIE_W, RAIL_TIE_H, -1);
    emit_beam(mesh, [0.30, RAIL_TIE_Y, 0.30], [0.70, RAIL_TIE_Y, 0.70], RAIL_TIE_W, RAIL_TIE_H, -1);
}

/// Builds a raised-bar 3D rail (metal bars tinted `1` when `powered`,
/// wooden sleepers untinted) for `shape`.
pub fn build_3d_rail_mesh(shape: RailShape, powered: bool) -> ExtrudedMesh {
    let tint = i32::from(powered);
    let mut mesh = ExtrudedMesh::new();
    let turns = match shape {
        RailShape::EastWest => {
            rail_straight_ew(&mut mesh, tint);
            0
        }
        RailShape::NorthSouth => {
            rail_straight_ew(&mut mesh, tint);
            1
        }
        RailShape::AscendingEast => {
            rail_ascending_east(&mut mesh, tint);
            0
        }
        RailShape::AscendingNorth => {
            rail_ascending_east(&mut mesh, tint);
            1
        }
        RailShape::AscendingWest => {
            rail_ascending_east(&mut mesh, tint);
            2
        }
        RailShape::AscendingSouth => {
            rail_ascending_east(&mut mesh, tint);
            3
        }
        RailShape::CurveSouthEast => {
            rail_curve_se(&mut mesh, tint);
            0
        }
        RailShape::CurveNorthEast => {
            rail_curve_se(&mut mesh, tint);
            1
        }
        RailShape::CurveNorthWest => {
            rail_curve_se(&mut mesh, tint);
            2
        }
        RailShape::CurveSouthWest => {
            rail_curve_se(&mut mesh, tint);
            3
        }
    };
    yaw(&mut mesh, turns);
    mesh
}

/// Builds a 3D ladder: two stiles plus four rungs as a thin plate against
/// the wall *opposite* `facing` — vanilla `facing` names the direction the
/// ladder faces (and the direction a player faces while climbing), so
/// `North` attaches to the south wall (`z = 1.0 - depth .. 1.0`) and fronts
/// north, `South` attaches to the north wall, and so on. `Up`/`Down` are
/// not valid wall facings and fall back to `North`/`South`.
pub fn build_3d_ladder_mesh(facing: Direction) -> ExtrudedMesh {
    let mut mesh = ExtrudedMesh::new();
    let depth = TEXEL;
    // Stiles: plate hugging the z = 1 (south) wall in local space.
    push_box(&mut mesh, [0.09, 0.0, 1.0 - depth], [0.19, 1.0, 1.0], -1);
    push_box(&mut mesh, [0.81, 0.0, 1.0 - depth], [0.91, 1.0, 1.0], -1);
    // Rungs: recessed behind the front face exactly like the stiles' depth.
    for i in 0..4 {
        let y0 = 0.11 + i as f32 * 0.25;
        push_box(
            &mut mesh,
            [0.19, y0, 1.0 - depth * 0.8],
            [0.81, y0 + 0.07, 1.0],
            -1,
        );
    }
    // Quarter turns about the block centre swing the south-wall plate onto
    // the wall opposite each facing: `North` keeps it on the south wall,
    // `South` flips it to the north wall, `West` swings it onto the east
    // wall and `East` onto the west wall.
    let turns = match facing {
        Direction::East => 3,
        Direction::South => 2,
        Direction::West => 1,
        _ => 0,
    };
    yaw(&mut mesh, turns);
    mesh
}

// ---------------------------------------------------------------------------
// Fixtures: chains, iron bars, torches, lanterns
// ---------------------------------------------------------------------------

/// Fixture shapes covered by the `fixtures_and_chains` toggle.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FixtureKind {
    Chain,
    IronBars,
    Torch,
    Lantern,
}

/// Small overlap inset (block units) applied wherever two fixture boxes
/// cross or stack: intersecting boxes keep a volumetric joint but their
/// faces never share a plane, so no coplanar pair can z-fight. `0.001` is
/// far below one texel (`1/16`) — invisible at any watchable distance.
const FIXTURE_JOINT_INSET: f32 = 0.001;

/// Builds a generic 3D fixture centred in the block. Wall-mounted variants
/// are produced by the caller yawing/orienting the returned mesh.
pub fn build_3d_fixture_mesh(kind: FixtureKind) -> ExtrudedMesh {
    let mut mesh = ExtrudedMesh::new();
    let i = FIXTURE_JOINT_INSET;
    match kind {
        FixtureKind::Chain => {
            // Central link column plus three alternating cross plates. Each
            // plate's thin dimension is inset by `i` on both sides so its
            // faces stay strictly inside the column — the joint is volumetric
            // but no two faces are ever coplanar.
            push_box(&mut mesh, [0.44, 0.0, 0.44], [0.56, 1.0, 0.56], -1);
            for (k, &along_x) in [true, false, true].iter().enumerate() {
                let y0 = 0.17 + k as f32 * 0.33;
                if along_x {
                    push_box(
                        &mut mesh,
                        [0.31, y0, 0.44 + i],
                        [0.69, y0 + 0.07, 0.56 - i],
                        -1,
                    );
                } else {
                    push_box(
                        &mut mesh,
                        [0.44 + i, y0, 0.31],
                        [0.56 - i, y0 + 0.07, 0.69],
                        -1,
                    );
                }
            }
        }
        FixtureKind::IronBars => {
            // Pane: two horizontal cross bars plus four vertical posts. The
            // vertical bars' height and the posts' cross-section are inset
            // by `i` so crossing boxes never share a face plane.
            push_box(&mut mesh, [0.0, 0.30, 0.47], [1.0, 0.40, 0.53], -1);
            push_box(&mut mesh, [0.0, 0.70, 0.47], [1.0, 0.80, 0.53], -1);
            push_box(&mut mesh, [0.47, 0.30 + i, 0.0], [0.53, 0.40 - i, 1.0], -1);
            push_box(&mut mesh, [0.47, 0.70 + i, 0.0], [0.53, 0.80 - i, 1.0], -1);
            for &(px, pz) in &[(0.22f32, 0.5f32), (0.78, 0.5), (0.5, 0.22), (0.5, 0.78)] {
                push_box(
                    &mut mesh,
                    [px - 0.03 + i, 0.0, pz - 0.03 + i],
                    [px + 0.03 - i, 1.0, pz + 0.03 - i],
                    -1,
                );
            }
        }
        FixtureKind::Torch => {
            // Handle plus a slightly wider flame head; the head's base dips
            // `i` into the handle so the joint faces never coincide.
            push_box(&mut mesh, [0.44, 0.0, 0.44], [0.56, 0.62, 0.56], -1);
            push_box(&mut mesh, [0.37, 0.62 - i, 0.37], [0.63, 0.76, 0.63], 0);
        }
        FixtureKind::Lantern => {
            // Each stacked section overlaps the one below by `i`, and the
            // hanging ring's bars are inset the same way, so no two boxes
            // share a face plane anywhere in the fixture.
            push_box(&mut mesh, [0.34, 0.0, 0.34], [0.66, 0.09, 0.66], -1);
            push_box(&mut mesh, [0.37, 0.09 - i, 0.37], [0.63, 0.50 + i, 0.63], -1);
            push_box(&mut mesh, [0.34, 0.50, 0.34], [0.66, 0.60, 0.66], -1);
            // Hanging ring: two side bars and a top bar.
            push_box(&mut mesh, [0.41, 0.60 - i, 0.47], [0.47, 0.78, 0.53], -1);
            push_box(&mut mesh, [0.53, 0.60 - i, 0.47], [0.59, 0.78, 0.53], -1);
            push_box(
                &mut mesh,
                [0.41 + i, 0.78 - i, 0.47 + i],
                [0.59 - i, 0.84, 0.53 - i],
                -1,
            );
        }
    }
    mesh
}

// ---------------------------------------------------------------------------
// Celestial: 3D voxel Sun and 8-phase Moon
// ---------------------------------------------------------------------------

/// Builds the voxel Sun in a `0..=1` sky-space cube centred on `(0.5, 0.5,
/// 0.5)`: a cubic core plus stepped square corona plates on all six axes.
/// Deliberately square and layered — unmistakably Minecraft, never a smooth
/// sphere.
pub fn build_3d_sun_mesh() -> ExtrudedMesh {
    let mut mesh = ExtrudedMesh::new();
    push_box(&mut mesh, [0.24, 0.24, 0.24], [0.76, 0.76, 0.76], -1);
    // (centre offset from block centre, plate size, plate thickness)
    const CORONA: [(f32, f32, f32); 3] = [
        (0.28, 0.62, 0.09),
        (0.38, 0.40, 0.09),
        (0.455, 0.20, 0.07),
    ];
    for dir in Direction::ALL {
        let axis = match dir {
            Direction::Down => [0.0, -1.0, 0.0],
            Direction::Up => [0.0, 1.0, 0.0],
            Direction::North => [0.0, 0.0, -1.0],
            Direction::South => [0.0, 0.0, 1.0],
            Direction::West => [-1.0, 0.0, 0.0],
            Direction::East => [1.0, 0.0, 0.0],
        };
        for &(off, size, thick) in &CORONA {
            let c = [
                0.5 + axis[0] * off,
                0.5 + axis[1] * off,
                0.5 + axis[2] * off,
            ];
            let (hx, hy, hz) = (
                if axis[0] != 0.0 { thick } else { size } * 0.5,
                if axis[1] != 0.0 { thick } else { size } * 0.5,
                if axis[2] != 0.0 { thick } else { size } * 0.5,
            );
            push_box(
                &mut mesh,
                [c[0] - hx, c[1] - hy, c[2] - hz],
                [c[0] + hx, c[1] + hy, c[2] + hz],
                -1,
            );
        }
    }
    mesh
}

/// The `16x16` alpha mask of the Moon's disc (radius 7.5 texels), shared by
/// every phase so the silhouette never wobbles.
pub fn moon_disc_mask() -> [u8; 256] {
    let mut mask = [0u8; 256];
    for y in 0..16 {
        for x in 0..16 {
            let nx = (x as f32 + 0.5 - 8.0) / 7.5;
            let ny = (y as f32 + 0.5 - 8.0) / 7.5;
            if nx * nx + ny * ny <= 1.0 {
                mask[y * 16 + x] = 255;
            }
        }
    }
    mask
}

/// The `16x16` lit mask for lunar `phase` (`0..=7`: full, waning gibbous,
/// third quarter, waning crescent, new, waxing crescent, first quarter,
/// waxing gibbous — the vanilla order). The terminator follows an elliptical
/// arc, so gibbous and crescent silhouettes read correctly.
pub fn moon_lit_mask(phase: u8) -> [u8; 256] {
    let mut mask = [0u8; 256];
    let p = phase & 7;
    let c = (PI * p as f32 / 4.0).cos();
    for y in 0..16 {
        for x in 0..16 {
            let nx = (x as f32 + 0.5 - 8.0) / 7.5;
            let ny = (y as f32 + 0.5 - 8.0) / 7.5;
            if nx * nx + ny * ny > 1.0 {
                continue;
            }
            let s = (1.0 - ny * ny).max(0.0).sqrt();
            let lit = match p {
                0 => true,
                4 => false,
                1..=3 => nx <= c * s - 0.001,
                _ => nx >= -c * s + 0.001,
            };
            if lit {
                mask[y * 16 + x] = 255;
            }
        }
    }
    mask
}

/// Fraction of the lunar disc lit in `phase`, in `0.0..=1.0`.
pub fn moon_lit_fraction(phase: u8) -> f32 {
    let lit = moon_lit_mask(phase).iter().filter(|&&a| a >= ALPHA_CUTOFF).count();
    let disc = moon_disc_mask().iter().filter(|&&a| a >= ALPHA_CUTOFF).count();
    if disc == 0 {
        0.0
    } else {
        lit as f32 / disc as f32
    }
}

/// Builds the 3D voxel Moon for `phase` (`0..=7`, vanilla order): a thin
/// extruded disc whose sunlit texels stand in raised relief over the
/// shadowed base — the phases read as real geometry, not a texture swap.
pub fn build_3d_moon_mesh(phase: u8) -> ExtrudedMesh {
    let mut mesh = extrude_alpha_mask_16x16(&moon_disc_mask(), TEXEL);
    let mut lit = extrude_alpha_mask_16x16(&moon_lit_mask(phase), TEXEL * 0.5);
    lit.translate([0.0, 0.0, TEXEL]);
    mesh.append(&lit);
    mesh
}

// ---------------------------------------------------------------------------
// Procedural 3D crops: three clean-room archetypes
// ---------------------------------------------------------------------------

/// Broad crop silhouette families for the procedural 3D crop upgrade.
///
/// The archetype is a *shape grammar*, not a texture lookup: every vanilla
/// and modded crop maps onto exactly one family, so no hand-authored
/// per-block geometry is ever needed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Crop3dArchetype {
    /// Wheat/rice/corn style: a cluster of tapered stalks that lean
    /// outward, each topped by a grain head once growth passes the
    /// halfway point.
    SheafStalk,
    /// Carrot/potato/beetroot/onion/cabbage style: a ground-level bulb or
    /// root mass plus a rosette of angled leaf blades.
    BulbLeaf,
    /// Tomato/berry/pepper style: a supporting vine stem with foliage and
    /// distinct 3D fruit pods that emerge in the upper growth stages.
    VineFruit,
}

/// Facts the procedural crop builder needs for one block occurrence.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Crop3dSpec {
    /// Which silhouette family to build.
    pub archetype: Crop3dArchetype,
    /// Current growth stage (`0` = freshly planted).
    pub stage: u8,
    /// Maximum growth stage. `0` fails closed to the smallest silhouette.
    pub max_stage: u8,
    /// Extra caller seed mixed into the deterministic lean/jitter so two
    /// adjacent identical crops still vary their pose.
    pub seed: u64,
    /// World-space block position, hashed into the pose jitter.
    pub block_pos: [i32; 3],
}

/// Growth fraction in `0.0..=1.0`; `max_stage == 0` fails closed to `0.0`.
fn growth_fraction(spec: &Crop3dSpec) -> f32 {
    let max = spec.max_stage.max(1) as f32;
    (spec.stage.min(spec.max_stage) as f32) / max
}

/// Deterministic `0.0..1.0` value from the spec seed mixed with a salt and
/// the block position — pure integer mixing, identical on every rebuild.
fn spec_jitter(spec: &Crop3dSpec, salt: u64) -> f32 {
    let mut h = spec.seed ^ salt.wrapping_mul(0x9e37_79b9_7f4a_7c15);
    h ^= (spec.block_pos[0] as u32 as u64).wrapping_mul(0x8da6_b343);
    h ^= (spec.block_pos[1] as u32 as u64).wrapping_mul(0xd816_3841);
    h ^= (spec.block_pos[2] as u32 as u64).wrapping_mul(0xcb1a_b31f);
    h ^= h >> 30;
    h = h.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    h ^= h >> 27;
    h = h.wrapping_mul(0x94d0_49bb_1331_11eb);
    h ^= h >> 31;
    // Top 24 bits -> [0, 1); exact and monotone in the integer hash.
    ((h >> 40) & 0x00ff_ffff) as f32 / 16_777_216.0
}

/// `(lean_x, lean_z)` unit-ish direction scaled to `amount` blocks for `i`.
fn lean_dir(spec: &Crop3dSpec, salt: u64, amount: f32) -> [f32; 3] {
    // Angle snapped to eighth-turns so lean stays pixel-art friendly.
    let a = spec_jitter(spec, salt) * 8.0;
    let eighth = (a as u32 & 7) as f32 * (PI * 0.25);
    [eighth.cos() * amount, 0.0, eighth.sin() * amount]
}

/// Builds a deterministic procedural 3D crop for `spec`.
///
/// All geometry lives in block-local `0..=1` space, grows monotonically with
/// `stage / max_stage`, and depends only on `spec` — no clock, no RNG, no
/// external assets. Degenerate specs (`max_stage == 0`) fail closed to the
/// stage-0 silhouette rather than dividing by zero.
pub fn build_3d_crop_mesh(spec: &Crop3dSpec) -> ExtrudedMesh {
    match spec.archetype {
        Crop3dArchetype::SheafStalk => build_sheaf_stalk(spec),
        Crop3dArchetype::BulbLeaf => build_bulb_leaf(spec),
        Crop3dArchetype::VineFruit => build_vine_fruit(spec),
    }
}

/// Wheat/rice/corn: `N` tapered stalks leaning outward from the block
/// centre; a grain head box crowns each stalk once `t >= 0.5`.
fn build_sheaf_stalk(spec: &Crop3dSpec) -> ExtrudedMesh {
    let mut mesh = ExtrudedMesh::new();
    let t = growth_fraction(spec);
    // Cluster positions snapped to the texel grid (4/16 and 12/16 lanes).
    const LANES: [(f32, f32); 5] = [
        (0.50, 0.50),
        (0.25, 0.31),
        (0.75, 0.31),
        (0.31, 0.75),
        (0.69, 0.69),
    ];
    for (i, &(px, pz)) in LANES.iter().enumerate() {
        let jx = spec_jitter(spec, 0x100 + i as u64);
        let lean = lean_dir(spec, 0x200 + i as u64, 0.02 + 0.06 * jx);
        // Stalk height grows monotonically: 3.5..13 texels.
        let h = 0.22 + 0.60 * t;
        // `emit_beam` centres its cross-section on the endpoint; seat the
        // base ring half a width up so the stalk bottom lands on y = 0.
        let w = 0.055 - 0.020 * t; // stalks slim as they stretch.
        let base = [px, w * 0.5, pz];
        let top = [px + lean[0], h, pz + lean[2]];
        emit_beam(&mut mesh, base, top, w, w, -1);
        // Grain head: appears at t >= 0.5, swells with growth.
        if t >= 0.5 {
            let hs = 0.06 + 0.10 * (t - 0.5);
            push_box(
                &mut mesh,
                [top[0] - hs, top[1] - hs * 0.5, top[2] - hs],
                [top[0] + hs, (top[1] + hs).min(0.98), top[2] + hs],
                0,
            );
        }
    }
    mesh
}

/// Carrot/onion/cabbage: a ground bulb plus a rosette of leaf blades angled
/// outward; both grow monotonically with `t`.
fn build_bulb_leaf(spec: &Crop3dSpec) -> ExtrudedMesh {
    let mut mesh = ExtrudedMesh::new();
    let t = growth_fraction(spec);
    // Bulb: half-buried root mass, 1..4.5 texels wide, peeking above soil.
    let r = 0.06 + 0.16 * t;
    let crown = 0.04 + 0.10 * t;
    push_box(
        &mut mesh,
        [0.5 - r, 0.0, 0.5 - r],
        [0.5 + r, crown, 0.5 + r],
        0,
    );
    // Six blades ring the bulb; lean and length grow with `t`.
    for i in 0..6u32 {
        let a = (i as f32 + spec_jitter(spec, 0x300) * 0.5) * (PI / 3.0);
        let (ca, sa) = (a.cos(), a.sin());
        let inner = r * 0.6;
        let reach = (r + 0.10 + 0.10 * t).min(0.46);
        let lift = crown + 0.05 + 0.30 * t;
        // Two-segment blade: up from the bulb, then tipped outward.
        let elbow = [0.5 + ca * inner, lift * 0.55, 0.5 + sa * inner];
        let tip = [0.5 + ca * reach, lift, 0.5 + sa * reach];
        let w = 0.045 - 0.015 * t;
        let root = [0.5 + ca * inner * 0.3, w * 0.5, 0.5 + sa * inner * 0.3];
        emit_beam(&mut mesh, root, elbow, w, w, -1);
        emit_beam(&mut mesh, elbow, tip, w, w * 0.6, -1);
    }
    mesh
}

/// Tomato/berry/pepper: a wiry vine stem, two foliage blades, and fruit
/// pods that pop in above `t = 0.55` (a discrete monotonic step).
fn build_vine_fruit(spec: &Crop3dSpec) -> ExtrudedMesh {
    let mut mesh = ExtrudedMesh::new();
    let t = growth_fraction(spec);
    let lean = lean_dir(spec, 0x400, 0.05);
    let h = 0.30 + 0.55 * t;
    let stem_top = [0.5 + lean[0], h, 0.5 + lean[2]];
    // Wiry stem: slight S-curve via a mid-joint; the base ring sits half a
    // width up so the stem bottom lands on y = 0.
    let mid = [0.5 - lean[2] * 0.5, h * 0.5, 0.5 + lean[0] * 0.5];
    emit_beam(&mut mesh, [0.5, 0.02, 0.5], mid, 0.04, 0.04, -1);
    emit_beam(&mut mesh, mid, stem_top, 0.04, 0.04, -1);
    // Two foliage blades at the mid joint and crown.
    for (i, at) in [mid, stem_top].iter().enumerate() {
        let a = spec_jitter(spec, 0x500 + i as u64) * 8.0;
        let eighth = (a as u32 & 7) as f32 * (PI * 0.25);
        let tip = [
            at[0] + eighth.cos() * 0.16,
            at[1] + 0.06 + 0.08 * t,
            at[2] + eighth.sin() * 0.16,
        ];
        emit_beam(&mut mesh, *at, tip, 0.06, 0.02, -1);
    }
    // Fruit pods: three distinct berries emerge in the upper stages.
    if t > 0.55 {
        for i in 0..3u64 {
            let a = spec_jitter(spec, 0x600 + i) * 8.0;
            let eighth = (a as u32 & 7) as f32 * (PI * 0.25);
            let pod_r = 0.05 + 0.04 * (t - 0.55);
            let cy = h * (0.55 + 0.15 * i as f32) - pod_r;
            let cx = stem_top[0] + eighth.cos() * 0.14;
            let cz = stem_top[2] + eighth.sin() * 0.14;
            push_box(
                &mut mesh,
                [cx - pod_r, cy.max(0.02), cz - pod_r],
                [
                    (cx + pod_r).min(0.98),
                    (cy + pod_r * 2.0).min(0.98),
                    (cz + pod_r).min(0.98),
                ],
                1,
            );
        }
    }
    mesh
}

// ---------------------------------------------------------------------------
// 0.5-texel outward ore vein relief
// ---------------------------------------------------------------------------

/// Outward relief depth for ore vein texels: half a sprite texel
/// (`0.5 / 16`), subtle enough to keep the vanilla silhouette while making
/// the mineral flecks read as real geometry.
pub const ORE_RELIEF_DEPTH: f32 = 0.5 / 16.0;

/// Maps a face-local relief plate (`x = u`, `y = 1 - v`, `z` outward from
/// the block surface) onto block space for `dir`, returning the
/// `(rotation, translation)` pair for [`ExtrudedMesh::affine`]. Every
/// rotation is a proper 90-degree grid rotation (determinant `+1`), so
/// winding and outwardness survive untouched.
fn ore_relief_transform(dir: Direction) -> ([[f32; 3]; 3], [f32; 3]) {
    match dir {
        // Face y=1: u -> +x, v -> +z, outward +y.
        Direction::Up => (
            [[1.0, 0.0, 0.0], [0.0, 0.0, 1.0], [0.0, -1.0, 0.0]],
            [0.0, 1.0, 1.0],
        ),
        // Face y=0: u -> 1-x, v -> +z, outward -y.
        Direction::Down => (
            [[-1.0, 0.0, 0.0], [0.0, 0.0, -1.0], [0.0, -1.0, 0.0]],
            [1.0, 0.0, 1.0],
        ),
        // Face z=0: u -> 1-x, v -> 1-y, outward -z.
        Direction::North => (
            [[-1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, -1.0]],
            [1.0, 0.0, 0.0],
        ),
        // Face z=1: u -> +x, v -> 1-y, outward +z.
        Direction::South => (
            [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]],
            [0.0, 0.0, 1.0],
        ),
        // Face x=0: u -> +z, v -> 1-y, outward -x.
        Direction::West => (
            [[0.0, 0.0, -1.0], [0.0, 1.0, 0.0], [1.0, 0.0, 0.0]],
            [0.0; 3],
        ),
        // Face x=1: u -> 1-z, v -> 1-y, outward +x.
        Direction::East => (
            [[0.0, 0.0, 1.0], [0.0, 1.0, 0.0], [-1.0, 0.0, 0.0]],
            [1.0, 0.0, 1.0],
        ),
    }
}

/// Builds the 0.5-texel outward relief mesh for an ore block.
///
/// `vein_mask` is a `16x16` face-space occupancy grid (`vein_mask[v * 16 +
/// u]`, row 0 = texture top) describing which texels carry mineral vein art.
/// Every non-zero texel is extruded outward by [`ORE_RELIEF_DEPTH`] on each
/// face whose bit is set in `exposed_faces` (a [`Direction::bit`] mask, so
/// `Down = 1 << 0 .. East = 1 << 5`). Faces not set emit zero quads — strict
/// hidden-face culling. An empty mask or an `exposed_faces` of `0` fails
/// closed to an empty mesh.
///
/// Per face the plate is greedy-merged exactly like the sprite extruder:
/// one front quad per merged vein span, one rim quad per boundary run, and
/// no inner face (the base cube already owns that plane, so nothing
/// z-fights). UVs stay in sprite space so the raised pixels keep their
/// exact texture footprint.
pub fn build_3d_ore_relief(vein_mask: &[u8; 256], exposed_faces: u8) -> ExtrudedMesh {
    let mut mesh = ExtrudedMesh::new();
    if exposed_faces & 0x3f == 0 {
        return mesh;
    }
    // Occupancy follows the caller's non-zero convention, not the sprite
    // alpha cutoff: any vein intensity raises a fleck.
    let mut mask = [0u8; 256];
    for i in 0..256 {
        mask[i] = u8::from(vein_mask[i] != 0) * 255;
    }
    if !mask.iter().any(|&a| a != 0) {
        return mesh;
    }
    for dir in Direction::ALL {
        if exposed_faces & dir.bit() == 0 {
            continue;
        }
        let mut plate = extrude_alpha_mask_16x16(&mask, ORE_RELIEF_DEPTH);
        // Drop the inner face quads: they sit coplanar with the base cube
        // face and only cause depth fighting.
        plate.quads.retain(|q| q.normal[2] > -0.5);
        let (m, t) = ore_relief_transform(dir);
        plate.affine(m, [0.0; 3], t);
        mesh.append(&plate);
    }
    mesh
}

// ---------------------------------------------------------------------------
// Mesh fingerprinting
// ---------------------------------------------------------------------------

/// Deterministic FNV-1a fingerprint over every quad's positions, UVs and
/// `tint_index` — the stable pin used by golden-hash tests to detect any
/// drift in generated crop or relief geometry.
pub fn mesh_fingerprint(mesh: &ExtrudedMesh) -> u64 {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut h = OFFSET;
    let mut feed = |bytes: &[u8]| {
        for &b in bytes {
            h ^= b as u64;
            h = h.wrapping_mul(PRIME);
        }
    };
    for q in &mesh.quads {
        for p in &q.positions {
            for &c in p {
                feed(&c.to_bits().to_ne_bytes());
            }
        }
        for uv in &q.uv {
            for &c in uv {
                feed(&c.to_bits().to_ne_bytes());
            }
        }
        feed(&q.tint_index.to_ne_bytes());
    }
    h
}
