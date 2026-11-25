// SPDX-License-Identifier: LGPL-3.0-or-later

//! GPU-ready packed voxel occupancy grid + voxel DDA point-light shadows.
//!
//! Torchlight in a block world has a geometrically exact shadow answer: march
//! a ray from the shaded point to the light through the *opaque* voxels —
//! every solid block fully blocks, every air/glass/leaf cell passes. Doing
//! that per fragment against a full chunk mesh is impossible, so this module
//! keeps a compact bit-packed occupancy volume of the player's immediate
//! surroundings and marches it with the classic Amanatides & Woo 3D DDA.
//!
//! ## The occupancy grid
//!
//! [`VoxelOccupancyGrid`] is a fixed `64×64×64` cell volume — one bit per
//! cell, `1` = casts an opaque shadow. That is `2^18` cells = 8,192 `u32`
//! words = **32 KiB**, small enough to sit entirely in GPU L1/shared memory,
//! so a fragment shader can march dozens of light rays per frame without
//! touching main memory. Cells are indexed `x + 64 * (z + 64 * y)` — the
//! same x-fastest/y-major order vanilla uses inside a chunk section — and
//! the linear index splits as `word = idx >> 5`, `bit = idx & 31`. The WGSL
//! kernel in [`VOXEL_DDA_SHADOW_WGSL`] uses the exact same indexing math on
//! the exact same `u32` word layout, so a `bytemuck`/`wgpu` upload of
//! [`VoxelOccupancyGrid::words`] needs zero transcoding.
//!
//! The grid is *anchored*: [`VoxelOccupancyGrid::set_origin`] records the
//! world coordinate of local cell `(0,0,0)` (rebuilt whenever the player
//! crosses ~16 blocks), and `*_block`/`voxel_shadow_factor_world` convert
//! between world and local space.
//!
//! ## The DDA march
//!
//! [`voxel_dda_shadow_factor`] casts a ray from `from` to a point light at
//! `to` (both in voxel-local units, cell `v` covering `[v, v+1)³`). It first
//! clips the segment to the volume with the slab test, then walks
//! cell-to-cell with Amanatides & Woo, accumulating the total chord length
//! spent inside opaque cells and the distance of the first blocker. The
//! shadow factor is
//!
//! ```text
//! penumbra_width w = 2 * light_radius * t_hit / (light_distance - t_hit)
//! coverage         = opaque_chord / w        (clamped 0..=1)
//! shadow           = 1 - coverage
//! ```
//!
//! a similar-triangles penumbra estimate: a blocker at the receiver is a
//! hard silhouette (`w → 0`), a blocker hugging the light gives a wide soft
//! gradient (`w → ∞`), and a corner-grazing ray keeps a short opaque chord
//! so it stays mostly lit. `light_radius <= 0` degenerates to an exact hard
//! shadow — any opaque cell on the path returns `0.0`.
//!
//! Nothing panics: NaN/inf inputs, zero-length rays (`from == to`), rays
//! that never touch the volume and step-budget exhaustion all return `1.0`
//! (unoccluded) rather than fabricating darkness.

/// Edge length of the cubic occupancy volume in voxels.
pub const VOXEL_GRID_DIM: usize = 64;

/// Total cells in the volume: `64³ = 262,144` bits.
pub const VOXEL_GRID_CELLS: usize = VOXEL_GRID_DIM * VOXEL_GRID_DIM * VOXEL_GRID_DIM;

/// `u32` words backing the volume: `262,144 / 32 = 8,192`.
pub const VOXEL_GRID_WORDS: usize = VOXEL_GRID_CELLS / 32;

/// Occupancy volume size in bytes: `8,192 * 4 = 32 KiB`.
pub const VOXEL_GRID_BYTES: usize = VOXEL_GRID_WORDS * 4;

/// Hard cap on DDA cell steps. A straight 64³ traversal crosses at most
/// `3 * 64 = 192` cells; 256 leaves generous margin for boundary-aligned
/// rays that visit extra cells along shared faces.
pub const MAX_DDA_STEPS: u32 = 256;

/// Default point-light radius in voxel units for [`VoxelShadowParams`]:
/// a quarter-voxel emissive blob — small enough to keep torch silhouettes
/// crisp, large enough to soften single-cell edges.
pub const DEFAULT_LIGHT_RADIUS: f32 = 0.25;

/// Ray-length epsilon: shorter segments are treated as coincident.
const DDA_EPS: f32 = 1.0e-6;

/// "Infinity" for axis-parallel ray bookkeeping. `f32::MAX` (not
/// `f32::INFINITY`) so the WGSL mirror can spell the identical constant
/// `3.402823e+38` — WGSL has no infinity literal.
const BIG_T: f32 = f32::MAX;

/// Bit-packed `64³` opaque-voxel occupancy volume anchored in the world.
///
/// `words` always has exactly [`VOXEL_GRID_WORDS`] elements; bit `i & 31` of
/// word `i >> 5` marks cell `(x, y, z)` with `i = x + 64 * (z + 64 * y)`
/// opaque. Only *opaque* bits cast shadows — glass, leaves, water, air and
/// every non-full-cube block simply stay `0`.
#[derive(Clone, Debug, PartialEq)]
pub struct VoxelOccupancyGrid {
    words: Vec<u32>,
    /// World coordinate of local cell `(0, 0, 0)`.
    origin: [i32; 3],
}

impl Default for VoxelOccupancyGrid {
    fn default() -> Self {
        Self::new()
    }
}

impl VoxelOccupancyGrid {
    /// Empty volume anchored at world `(0, 0, 0)`.
    pub fn new() -> Self {
        Self {
            words: vec![0; VOXEL_GRID_WORDS],
            origin: [0; 3],
        }
    }

    /// Empty volume anchored at `origin` (world coordinate of cell 0,0,0).
    pub fn with_origin(origin: [i32; 3]) -> Self {
        Self {
            words: vec![0; VOXEL_GRID_WORDS],
            origin,
        }
    }

    /// Volume anchored at `origin` from a pre-packed word buffer, or `None`
    /// when `words.len() != `[`VOXEL_GRID_WORDS`].
    pub fn from_words(origin: [i32; 3], words: Vec<u32>) -> Option<Self> {
        if words.len() != VOXEL_GRID_WORDS {
            return None;
        }
        Some(Self { words, origin })
    }

    /// World coordinate of local cell `(0, 0, 0)`.
    pub fn origin(&self) -> [i32; 3] {
        self.origin
    }

    /// Re-anchor the volume (caller is responsible for refilling cells).
    pub fn set_origin(&mut self, origin: [i32; 3]) {
        self.origin = origin;
    }

    /// Raw `u32` words, exactly the layout [`VOXEL_DDA_SHADOW_WGSL`] reads.
    pub fn words(&self) -> &[u32] {
        &self.words
    }

    /// Clear every cell to non-opaque.
    pub fn clear(&mut self) {
        self.words.fill(0);
    }

    /// Linear cell index `x + 64 * (z + 64 * y)`; callers bounds-check first.
    #[inline]
    fn voxel_index(x: usize, y: usize, z: usize) -> usize {
        x + VOXEL_GRID_DIM * (z + VOXEL_GRID_DIM * y)
    }

    /// Mark local cell `(x, y, z)` opaque or not. Out-of-bounds writes are
    /// ignored — never a panic.
    #[inline]
    pub fn set_opaque(&mut self, x: usize, y: usize, z: usize, opaque: bool) {
        if x >= VOXEL_GRID_DIM || y >= VOXEL_GRID_DIM || z >= VOXEL_GRID_DIM {
            return;
        }
        let idx = Self::voxel_index(x, y, z);
        let word = &mut self.words[idx >> 5];
        let bit = 1u32 << (idx & 31);
        if opaque {
            *word |= bit;
        } else {
            *word &= !bit;
        }
    }

    /// Whether local cell `(x, y, z)` casts an opaque shadow. Out-of-bounds
    /// reads are `false` — the world outside the volume never occludes.
    #[inline]
    pub fn is_opaque(&self, x: usize, y: usize, z: usize) -> bool {
        if x >= VOXEL_GRID_DIM || y >= VOXEL_GRID_DIM || z >= VOXEL_GRID_DIM {
            return false;
        }
        let idx = Self::voxel_index(x, y, z);
        (self.words[idx >> 5] >> (idx & 31)) & 1 != 0
    }

    /// Same bit test for a signed DDA cell coordinate.
    #[inline]
    fn is_opaque_cell(&self, x: i32, y: i32, z: i32) -> bool {
        if x < 0 || y < 0 || z < 0 {
            return false;
        }
        self.is_opaque(x as usize, y as usize, z as usize)
    }

    /// Mark the cell containing world block `pos` opaque or not. Positions
    /// outside the volume are ignored.
    #[inline]
    pub fn set_opaque_block(&mut self, pos: [i32; 3], opaque: bool) {
        let x = i64::from(pos[0]) - i64::from(self.origin[0]);
        let y = i64::from(pos[1]) - i64::from(self.origin[1]);
        let z = i64::from(pos[2]) - i64::from(self.origin[2]);
        if (0..VOXEL_GRID_DIM as i64).contains(&x)
            && (0..VOXEL_GRID_DIM as i64).contains(&y)
            && (0..VOXEL_GRID_DIM as i64).contains(&z)
        {
            self.set_opaque(x as usize, y as usize, z as usize, opaque);
        }
    }

    /// Whether the cell containing world block `pos` casts an opaque shadow.
    #[inline]
    pub fn is_opaque_block(&self, pos: [i32; 3]) -> bool {
        let x = i64::from(pos[0]) - i64::from(self.origin[0]);
        let y = i64::from(pos[1]) - i64::from(self.origin[1]);
        let z = i64::from(pos[2]) - i64::from(self.origin[2]);
        if (0..VOXEL_GRID_DIM as i64).contains(&x)
            && (0..VOXEL_GRID_DIM as i64).contains(&y)
            && (0..VOXEL_GRID_DIM as i64).contains(&z)
        {
            self.is_opaque(x as usize, y as usize, z as usize)
        } else {
            false
        }
    }
}

/// Tunables for [`voxel_shadow_factor`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct VoxelShadowParams {
    /// Effective radius of the point light in voxel units. `0.0` gives a
    /// mathematically exact hard shadow; larger values widen the penumbra
    /// gradient around silhouettes.
    pub light_radius: f32,
    /// Distance along the ray the march origin is nudged toward the light
    /// before traversal — the classic acne bias so a point resting on an
    /// opaque face does not shadow itself. `0.0` disables.
    pub origin_bias: f32,
    /// Maximum DDA cell steps, capped at [`MAX_DDA_STEPS`].
    pub max_steps: u32,
}

impl Default for VoxelShadowParams {
    fn default() -> Self {
        Self {
            light_radius: DEFAULT_LIGHT_RADIUS,
            origin_bias: 1.0e-3,
            max_steps: MAX_DDA_STEPS,
        }
    }
}

impl VoxelShadowParams {
    /// Mathematically exact hard shadows: zero light radius, zero bias, full
    /// step budget. Useful for parity tests and crisp-silence comparisons.
    pub const HARD: Self = Self {
        light_radius: 0.0,
        origin_bias: 0.0,
        max_steps: MAX_DDA_STEPS,
    };
}

/// Slab-clip the ray segment `from + dir * t, t ∈ [0, len]` against the
/// volume box `[0, 64)³`, returning `(t_enter, t_leave)` or `None` when the
/// segment never touches the box. Kept bit-for-bit identical to the WGSL
/// `clip_segment_to_grid` — including treating `dir == 0` axes by origin
/// range only.
fn clip_segment_to_grid(from: [f32; 3], dir: [f32; 3], len: f32) -> Option<(f32, f32)> {
    let mut t0 = 0.0f32;
    let mut t1 = len;
    for a in 0..3 {
        let d = dir[a];
        let f = from[a];
        if d.abs() < DDA_EPS {
            if f < 0.0 || f > VOXEL_GRID_DIM as f32 {
                return None;
            }
            continue;
        }
        let mut lo = (0.0 - f) / d;
        let mut hi = (VOXEL_GRID_DIM as f32 - f) / d;
        if lo > hi {
            std::mem::swap(&mut lo, &mut hi);
        }
        t0 = t0.max(lo);
        t1 = t1.min(hi);
        if t0 > t1 {
            return None;
        }
    }
    Some((t0, t1))
}

/// CPU reference implementation of the voxel DDA point-light shadow kernel
/// in [`VOXEL_DDA_SHADOW_WGSL`], kept line-for-line equivalent: same clip,
/// same tie-broken axis selection (lowest axis wins), same opaque-chord
/// accumulation and the same similar-triangles penumbra.
///
/// `from`/`to` are voxel-local coordinates (cell `v` spans `[v, v+1)³`).
/// Returns `1.0` for a fully lit point and `0.0` for a fully shadowed one.
/// NaN/inf inputs, a coincident `from == to`, rays missing the volume and
/// step-budget exhaustion all return `1.0` — never a panic.
pub fn voxel_shadow_factor(
    grid: &VoxelOccupancyGrid,
    from: [f32; 3],
    to: [f32; 3],
    params: &VoxelShadowParams,
) -> f32 {
    if !from.iter().chain(to.iter()).all(|v| v.is_finite()) {
        return 1.0;
    }
    let d = [to[0] - from[0], to[1] - from[1], to[2] - from[2]];
    let len = (d[0] * d[0] + d[1] * d[1] + d[2] * d[2]).sqrt();
    if !len.is_finite() || len < DDA_EPS {
        return 1.0;
    }
    let dir = [d[0] / len, d[1] / len, d[2] / len];
    let radius = if params.light_radius.is_finite() {
        params.light_radius.max(0.0)
    } else {
        0.0
    };
    let bias = if params.origin_bias.is_finite() {
        params.origin_bias.clamp(0.0, 1.0)
    } else {
        0.0
    };
    let max_steps = params.max_steps.min(MAX_DDA_STEPS);
    let from_b = [
        from[0] + dir[0] * bias,
        from[1] + dir[1] * bias,
        from[2] + dir[2] * bias,
    ];
    let Some((t_enter, t_leave)) = clip_segment_to_grid(from_b, dir, len) else {
        return 1.0;
    };
    let start = [
        from_b[0] + dir[0] * t_enter,
        from_b[1] + dir[1] * t_enter,
        from_b[2] + dir[2] * t_enter,
    ];
    let mut cell = [
        start[0].floor() as i32,
        start[1].floor() as i32,
        start[2].floor() as i32,
    ];
    let mut step = [0i32; 3];
    let mut t_delta = [BIG_T; 3];
    let mut t_max = [BIG_T; 3];
    for a in 0..3 {
        if dir[a] > DDA_EPS {
            step[a] = 1;
            t_delta[a] = 1.0 / dir[a];
            t_max[a] = (cell[a] as f32 + 1.0 - from_b[a]) / dir[a];
        } else if dir[a] < -DDA_EPS {
            step[a] = -1;
            t_delta[a] = -1.0 / dir[a];
            t_max[a] = (cell[a] as f32 - from_b[a]) / dir[a];
        }
    }
    let mut opaque_path = 0.0f32;
    let mut first_hit = -1.0f32;
    let mut prev_t = t_enter;
    for _ in 0..max_steps {
        let opaque = grid.is_opaque_cell(cell[0], cell[1], cell[2]);
        if opaque && first_hit < 0.0 {
            first_hit = prev_t;
        }
        let mut axis = 0usize;
        let mut t_next = t_max[0];
        if t_max[1] < t_next {
            axis = 1;
            t_next = t_max[1];
        }
        if t_max[2] < t_next {
            axis = 2;
            t_next = t_max[2];
        }
        let span_end = t_next.min(t_leave);
        if opaque {
            opaque_path += span_end - prev_t;
        }
        if t_next >= t_leave {
            break;
        }
        cell[axis] += step[axis];
        t_max[axis] += t_delta[axis];
        prev_t = t_next;
    }
    if first_hit < 0.0 || opaque_path <= 0.0 {
        return 1.0;
    }
    if radius <= 0.0 {
        return 0.0;
    }
    let penumbra = 2.0 * radius * first_hit / (len - first_hit).max(DDA_EPS);
    let coverage = (opaque_path / penumbra.max(DDA_EPS)).min(1.0);
    (1.0 - coverage).clamp(0.0, 1.0)
}

/// World-space convenience wrapper: translates `from_world`/`to_world`
/// (block-coordinate `f64`s, e.g. a face point and a torch position) into
/// the grid's local frame and calls [`voxel_shadow_factor`]. Positions far
/// outside the volume saturate harmlessly through the clip test.
pub fn voxel_shadow_factor_world(
    grid: &VoxelOccupancyGrid,
    from_world: [f64; 3],
    to_world: [f64; 3],
    params: &VoxelShadowParams,
) -> f32 {
    let o = grid.origin();
    let local = |p: [f64; 3]| {
        [
            (p[0] - f64::from(o[0])) as f32,
            (p[1] - f64::from(o[1])) as f32,
            (p[2] - f64::from(o[2])) as f32,
        ]
    };
    voxel_shadow_factor(grid, local(from_world), local(to_world), params)
}

/// WGSL voxel DDA point-light shadow kernel — the GPU twin of
/// [`voxel_shadow_factor`], kept line-for-line equivalent.
///
/// Bindings (group 0):
/// * `0` — `uniform` [`VoxelShadowParams`] packed as `vec4<f32>`:
///   `x = light_radius`, `y = max_steps`, `z = origin_bias`, `w` unused.
/// * `1` — `storage, read` `array<u32>`: the occupancy words, exactly
///   [`VoxelOccupancyGrid::words`] (`word = idx >> 5`, `bit = idx & 31`,
///   `idx = x + 64 * (z + 64 * y)`).
/// * `2` — `storage, read` `array<VoxelShadowQuery>`: `from`/`to` voxel-local
///   `vec4<f32>` pairs (`.xyz` used, `.w` ignored).
/// * `3` — `storage, read_write` `array<f32>`: one shadow factor per query.
///
/// Dispatch `ceil(n_queries / 64)` workgroups of `@workgroup_size(64)`.
pub const VOXEL_DDA_SHADOW_WGSL: &str = r#"
// Megu voxel DDA point-light shadow kernel.
// Bit-packed 64^3 opaque-voxel occupancy grid; one bit per cell, layout
// identical to VoxelOccupancyGrid::words on the CPU side.

struct VoxelShadowParams {
    // x = light_radius (voxels, 0 = hard shadow)
    // y = max_steps, z = origin_bias, w = unused
    values: vec4<f32>,
};

struct VoxelShadowQuery {
    from: vec4<f32>,
    to: vec4<f32>,
};

@group(0) @binding(0) var<uniform> dda_params: VoxelShadowParams;
@group(0) @binding(1) var<storage, read> voxel_bits: array<u32>;
@group(0) @binding(2) var<storage, read> queries: array<VoxelShadowQuery>;
@group(0) @binding(3) var<storage, read_write> shadow_out: array<f32>;

const VOXEL_DIM: i32 = 64;
const VOXEL_WORDS: u32 = 8192u;
const BIG_T: f32 = 3.402823e+38;
const DDA_EPS: f32 = 1e-6;

fn voxel_index(v: vec3<i32>) -> u32 {
    // idx = x + 64 * (z + 64 * y) — identical to VoxelOccupancyGrid::voxel_index.
    return u32(v.x + VOXEL_DIM * (v.z + VOXEL_DIM * v.y));
}

fn voxel_opaque(v: vec3<i32>) -> bool {
    if any(v < vec3<i32>(0)) || any(v >= vec3<i32>(VOXEL_DIM)) {
        return false;
    }
    let idx = voxel_index(v);
    if idx >= VOXEL_WORDS * 32u {
        return false;
    }
    // word = idx >> 5, bit = idx & 31 — identical to the CPU bit test.
    return ((voxel_bits[idx >> 5u] >> (idx & 31u)) & 1u) != 0u;
}

// Slab-clip `from + dir * t` for t in 0..len against the 64^3 volume box.
// Returns (t_enter, t_leave), or (1, 0) when the segment misses the box.
fn clip_segment_to_grid(from: vec3<f32>, dir: vec3<f32>, len: f32) -> vec2<f32> {
    var t0 = 0.0;
    var t1 = len;
    for (var a = 0; a < 3; a++) {
        let d = dir[a];
        let f = from[a];
        if abs(d) < DDA_EPS {
            if f < 0.0 || f > f32(VOXEL_DIM) {
                return vec2<f32>(1.0, 0.0);
            }
            continue;
        }
        var lo = (0.0 - f) / d;
        var hi = (f32(VOXEL_DIM) - f) / d;
        if lo > hi {
            let tmp = lo;
            lo = hi;
            hi = tmp;
        }
        t0 = max(t0, lo);
        t1 = min(t1, hi);
        if t0 > t1 {
            return vec2<f32>(1.0, 0.0);
        }
    }
    return vec2<f32>(t0, t1);
}

// Amanatides & Woo 3D DDA over the occupancy bits — line-for-line identical
// to voxel_shadow_factor(): same tie-broken axis selection (lowest axis
// wins), same opaque-chord accumulation, same similar-triangles penumbra.
fn voxel_dda_shadow(from_in: vec3<f32>, to: vec3<f32>) -> f32 {
    let d = to - from_in;
    let len = length(d);
    // NaN fails the first clause, inf the second: degenerate rays stay lit.
    if !(len > DDA_EPS && len < BIG_T) {
        return 1.0;
    }
    let dir = d / len;
    let radius = max(dda_params.values.x, 0.0);
    let max_steps = min(u32(dda_params.values.y), 256u);
    let bias = clamp(dda_params.values.z, 0.0, 1.0);
    let from = from_in + dir * bias;
    let span = clip_segment_to_grid(from, dir, len);
    if span.x > span.y {
        return 1.0;
    }
    let start = from + dir * span.x;
    var cell = vec3<i32>(floor(start));
    var step = vec3<i32>(0);
    var t_delta = vec3<f32>(BIG_T);
    var t_max = vec3<f32>(BIG_T);
    if dir.x > DDA_EPS {
        step.x = 1;
        t_delta.x = 1.0 / dir.x;
        t_max.x = (f32(cell.x) + 1.0 - from.x) / dir.x;
    } else if dir.x < -DDA_EPS {
        step.x = -1;
        t_delta.x = -1.0 / dir.x;
        t_max.x = (f32(cell.x) - from.x) / dir.x;
    }
    if dir.y > DDA_EPS {
        step.y = 1;
        t_delta.y = 1.0 / dir.y;
        t_max.y = (f32(cell.y) + 1.0 - from.y) / dir.y;
    } else if dir.y < -DDA_EPS {
        step.y = -1;
        t_delta.y = -1.0 / dir.y;
        t_max.y = (f32(cell.y) - from.y) / dir.y;
    }
    if dir.z > DDA_EPS {
        step.z = 1;
        t_delta.z = 1.0 / dir.z;
        t_max.z = (f32(cell.z) + 1.0 - from.z) / dir.z;
    } else if dir.z < -DDA_EPS {
        step.z = -1;
        t_delta.z = -1.0 / dir.z;
        t_max.z = (f32(cell.z) - from.z) / dir.z;
    }
    var opaque_path = 0.0;
    var first_hit = -1.0;
    var prev_t = span.x;
    for (var i = 0u; i < max_steps; i++) {
        let opaque = voxel_opaque(cell);
        if opaque && first_hit < 0.0 {
            first_hit = prev_t;
        }
        var axis = 0;
        var t_next = t_max.x;
        if t_max.y < t_next {
            axis = 1;
            t_next = t_max.y;
        }
        if t_max.z < t_next {
            axis = 2;
            t_next = t_max.z;
        }
        let span_end = min(t_next, span.y);
        if opaque {
            opaque_path += span_end - prev_t;
        }
        if t_next >= span.y {
            break;
        }
        if axis == 0 {
            cell.x += step.x;
            t_max.x += t_delta.x;
        } else if axis == 1 {
            cell.y += step.y;
            t_max.y += t_delta.y;
        } else {
            cell.z += step.z;
            t_max.z += t_delta.z;
        }
        prev_t = t_next;
    }
    if first_hit < 0.0 || opaque_path <= 0.0 {
        return 1.0;
    }
    if radius <= 0.0 {
        return 0.0;
    }
    let penumbra = 2.0 * radius * first_hit / max(len - first_hit, DDA_EPS);
    let coverage = min(opaque_path / max(penumbra, DDA_EPS), 1.0);
    return clamp(1.0 - coverage, 0.0, 1.0);
}

@compute @workgroup_size(64)
fn cs_main(@builtin(global_invocation_id) gid: vec3<u32>) {
    if gid.x >= arrayLength(&queries) {
        return;
    }
    let q = queries[gid.x];
    shadow_out[gid.x] = voxel_dda_shadow(q.from.xyz, q.to.xyz);
}
"#;
