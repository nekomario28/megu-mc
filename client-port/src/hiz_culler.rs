// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later

//! Conservative hierarchical-Z (Hi-Z) occlusion culling, pure `std`.
//!
//! [`HiZPyramid`] downsamples a standard `[0, 1]` depth buffer (`0.0` = near
//! plane, `1.0` = far plane) into a **max**-depth mip chain: every mip texel
//! holds the *farthest* occluder depth of its `2 x 2` footprint, including
//! odd border texels when the parent dimension is odd, so a screen-space
//! rectangle is occluded only when its nearest possible depth is strictly
//! behind *every* base-level texel it covers. The test therefore has **zero
//! false occlusions**: anything that could show a single visible pixel
//! reports visible. A reverse-Z pipeline (`1.0` = near) would need the dual
//! `min`-depth pyramid; this module only implements the standard-direction
//! variant.
//!
//! * [`HiZPyramid::is_screen_rect_occluded`] tests a pixel-space rect plus
//!   the primitive's nearest `z` against one mip level in `O(1)`.
//! * [`HiZPyramid::cull_world_aabb`] projects the 8 corners of a world AABB
//!   through a `view_proj` matrix and runs the rect test; a corner on or
//!   behind the near plane (`w <= 0`) conservatively reports visible.
//! * [`HiZPyramid::cull_meshlet_batch`] folds a batch of world AABBs into
//!   [`HiZCullStats`].

/// Largest depth-buffer edge the pyramid accepts. Mip construction is
/// O(width * height), so a 4096 x 4096 buffer stays under ~22 M texels
/// across the whole chain.
pub const MAX_DEPTH_DIMENSION: usize = 4096;

/// Depth slack for the occlusion test: a rect culls only when its nearest
/// possible depth is strictly farther than the farthest occluder texel it
/// covers by more than this epsilon, so depth ties never cull.
const OCCLUSION_TEST_EPSILON: f32 = 1.0e-6;

/// Why [`HiZPyramid::from_depth_buffer`] rejected its input.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HiZError {
    /// `width` or `height` is outside `1..=MAX_DEPTH_DIMENSION`.
    InvalidDimensions { width: usize, height: usize },
    /// `depth_01.len()` does not equal `width * height`.
    DepthLengthMismatch { expected: usize, actual: usize },
}

impl std::fmt::Display for HiZError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match *self {
            HiZError::InvalidDimensions { width, height } => write!(
                f,
                "depth buffer dimensions {width}x{height} outside 1..={MAX_DEPTH_DIMENSION}"
            ),
            HiZError::DepthLengthMismatch { expected, actual } => write!(
                f,
                "depth buffer has {actual} texels, expected {expected} (width * height)"
            ),
        }
    }
}

impl std::error::Error for HiZError {}

/// One mip level of the pyramid: `width * height` row-major max-depth
/// texels. Level 0 is the (scrubbed) depth buffer itself.
#[derive(Clone, Debug)]
struct Mip {
    width: usize,
    height: usize,
    texels: Vec<f32>,
}

/// Per-AABB outcome counters from [`HiZPyramid::cull_meshlet_batch`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HiZCullStats {
    /// AABBs passed through the occlusion test.
    pub tested: u32,
    /// AABBs proven fully occluded.
    pub culled: u32,
    /// `tested - culled`: everything still drawn (includes all
    /// conservative "cannot prove occluded" results).
    pub visible: u32,
}

/// Conservative max-depth occlusion pyramid over a `[0, 1]` depth buffer.
#[derive(Clone, Debug)]
pub struct HiZPyramid {
    width: usize,
    height: usize,
    /// `mips[0]` is the full-resolution depth buffer; each later level is
    /// the 2x2 max of the previous one. The chain ends at `1 x 1`.
    mips: Vec<Mip>,
}

impl HiZPyramid {
    /// Build the pyramid from a row-major depth buffer holding standard
    /// `[0, 1]` depth (`0.0` near, `1.0` far). Non-finite texels are
    /// scrubbed to `1.0` (farthest), the conservative direction: a
    /// corrupt texel can only *lose* occlusion information, never invent
    /// it.
    pub fn from_depth_buffer(
        width: usize,
        height: usize,
        depth_01: &[f32],
    ) -> Result<Self, HiZError> {
        if width == 0 || height == 0 || width > MAX_DEPTH_DIMENSION || height > MAX_DEPTH_DIMENSION
        {
            return Err(HiZError::InvalidDimensions { width, height });
        }
        if depth_01.len() != width * height {
            return Err(HiZError::DepthLengthMismatch {
                expected: width * height,
                actual: depth_01.len(),
            });
        }

        let mut mips = Vec::new();
        let mut mip = Mip {
            width,
            height,
            texels: depth_01
                .iter()
                .map(|&d| if d.is_finite() { d } else { 1.0 })
                .collect(),
        };
        loop {
            let (next_w, next_h) = (mip.width.div_ceil(2), mip.height.div_ceil(2));
            if next_w == mip.width && next_h == mip.height {
                mips.push(mip);
                break;
            }
            let (w, h) = (mip.width, mip.height);
            let mut texels = Vec::with_capacity(next_w * next_h);
            for y in 0..next_h {
                for x in 0..next_w {
                    // Max over the 2x2 footprint; edge texels duplicate the
                    // last row/column so odd dimensions keep their border
                    // inside the parent max (never dropped, never culled
                    // by accident).
                    let mut m = f32::NEG_INFINITY;
                    for dy in 0..2usize {
                        for dx in 0..2usize {
                            let sx = (2 * x + dx).min(w - 1);
                            let sy = (2 * y + dy).min(h - 1);
                            m = m.max(mip.texels[sy * w + sx]);
                        }
                    }
                    texels.push(m);
                }
            }
            let next = Mip {
                width: next_w,
                height: next_h,
                texels,
            };
            mips.push(mip);
            mip = next;
        }
        Ok(Self {
            width,
            height,
            mips,
        })
    }

    /// Depth-buffer width in pixels.
    pub fn width(&self) -> usize {
        self.width
    }

    /// Depth-buffer height in pixels.
    pub fn height(&self) -> usize {
        self.height
    }

    /// Number of mip levels (1 for a `1 x 1` buffer).
    pub fn mip_count(&self) -> usize {
        self.mips.len()
    }

    /// `(width, height, row-major texels)` of mip `level`, for tests and
    /// diagnostics. Level 0 is the scrubbed input buffer.
    pub fn mip(&self, level: usize) -> Option<(usize, usize, &[f32])> {
        self.mips
            .get(level)
            .map(|m| (m.width, m.height, m.texels.as_slice()))
    }

    /// Covered-texel range `[first, last]` of pixel interval `[a0, a1]`
    /// (`a0 < a1`, both inside `0..=dim * s`) at mip scale `s`, clamped to
    /// the `dim`-texel mip. The range is the exact set of texels the
    /// interval touches; sampling their max can only overcover, never
    /// undercover, so the occlusion test stays conservative.
    fn texel_range(a0: f32, a1: f32, s: f32, dim: usize) -> (usize, usize) {
        let first = ((a0 / s).floor() as isize).max(0) as usize;
        let last = (((a1 / s).ceil() as isize).max(1) - 1).max(first as isize) as usize;
        (first.min(dim - 1), last.min(dim - 1))
    }

    /// Conservative occlusion test for the pixel-space rectangle
    /// `[min_x_px, max_x_px] x [min_y_px, max_y_px]` whose nearest possible
    /// depth is `nearest_z_01`. Returns `true` only when every depth-buffer
    /// texel the rect touches holds a strictly nearer occluder
    /// (`nearest_z_01 > max_occluder_z + epsilon`), i.e. the rect cannot
    /// show a single pixel.
    ///
    /// Never culls (returns `false`) when `nearest_z_01 <= 0.0` (the rect
    /// can touch the near plane), when any bound or the depth is
    /// non-finite, or when the clamped rect is empty/off-screen.
    pub fn is_screen_rect_occluded(
        &self,
        min_x_px: f32,
        min_y_px: f32,
        max_x_px: f32,
        max_y_px: f32,
        nearest_z_01: f32,
    ) -> bool {
        if nearest_z_01 <= 0.0 || !nearest_z_01.is_finite() {
            return false;
        }
        if ![min_x_px, min_y_px, max_x_px, max_y_px]
            .iter()
            .all(|v| v.is_finite())
        {
            return false;
        }
        let (w, h) = (self.width as f32, self.height as f32);
        let x0 = min_x_px.min(max_x_px).clamp(0.0, w);
        let x1 = min_x_px.max(max_x_px).clamp(0.0, w);
        let y0 = min_y_px.min(max_y_px).clamp(0.0, h);
        let y1 = min_y_px.max(max_y_px).clamp(0.0, h);
        if x1 <= x0 || y1 <= y0 {
            return false;
        }

        // Smallest mip where the rect covers at most a 2x2 texel
        // footprint: the sample count stays O(1) while the covered texels
        // still span every base-level texel under the rect.
        let mut level = self.mips.len() - 1;
        for (k, mip) in self.mips.iter().enumerate() {
            let s = (1usize << k) as f32;
            let (fx, lx) = Self::texel_range(x0, x1, s, mip.width);
            let (fy, ly) = Self::texel_range(y0, y1, s, mip.height);
            if lx - fx < 2 && ly - fy < 2 {
                level = k;
                break;
            }
        }
        let mip = &self.mips[level];
        let s = (1usize << level) as f32;
        let (fx, lx) = Self::texel_range(x0, x1, s, mip.width);
        let (fy, ly) = Self::texel_range(y0, y1, s, mip.height);

        let mut max_occluder = f32::NEG_INFINITY;
        for ty in fy..=ly {
            for tx in fx..=lx {
                max_occluder = max_occluder.max(mip.texels[ty * mip.width + tx]);
            }
        }
        nearest_z_01 > max_occluder + OCCLUSION_TEST_EPSILON
    }

    /// Conservative occlusion test for a world-space AABB. All 8 corners
    /// are projected by `view_proj`, a row-major 4x4 matrix applied as
    /// `clip = view_proj * [x, y, z, 1]`; `x`/`y` NDC in `[-1, 1]` map
    /// linearly onto `width x height` pixels and `z` NDC is the standard
    /// `[0, 1]` depth.
    ///
    /// Returns `false` (visible) whenever a corner lands on or behind the
    /// near plane (`w <= 0`) or any projected value is non-finite: a box
    /// the projection cannot bound may still be visible.
    pub fn cull_world_aabb(
        &self,
        view_proj: &[[f32; 4]; 4],
        aabb_min: [f32; 3],
        aabb_max: [f32; 3],
    ) -> bool {
        let (mut x0, mut x1) = (f32::INFINITY, f32::NEG_INFINITY);
        let (mut y0, mut y1) = (f32::INFINITY, f32::NEG_INFINITY);
        let mut nearest_z = f32::INFINITY;
        for corner_bits in 0..8usize {
            let corner = [
                if corner_bits & 1 == 0 {
                    aabb_min[0]
                } else {
                    aabb_max[0]
                },
                if corner_bits & 2 == 0 {
                    aabb_min[1]
                } else {
                    aabb_max[1]
                },
                if corner_bits & 4 == 0 {
                    aabb_min[2]
                } else {
                    aabb_max[2]
                },
                1.0,
            ];
            let clip = [
                view_proj[0][0] * corner[0]
                    + view_proj[0][1] * corner[1]
                    + view_proj[0][2] * corner[2]
                    + view_proj[0][3] * corner[3],
                view_proj[1][0] * corner[0]
                    + view_proj[1][1] * corner[1]
                    + view_proj[1][2] * corner[2]
                    + view_proj[1][3] * corner[3],
                view_proj[2][0] * corner[0]
                    + view_proj[2][1] * corner[1]
                    + view_proj[2][2] * corner[2]
                    + view_proj[2][3] * corner[3],
                view_proj[3][0] * corner[0]
                    + view_proj[3][1] * corner[1]
                    + view_proj[3][2] * corner[2]
                    + view_proj[3][3] * corner[3],
            ];
            let w = clip[3];
            if !w.is_finite() || w <= 0.0 {
                return false;
            }
            let ndc = [clip[0] / w, clip[1] / w, clip[2] / w];
            if !ndc.iter().all(|v| v.is_finite()) {
                return false;
            }
            let px = (ndc[0] * 0.5 + 0.5) * self.width as f32;
            let py = (ndc[1] * 0.5 + 0.5) * self.height as f32;
            x0 = x0.min(px);
            x1 = x1.max(px);
            y0 = y0.min(py);
            y1 = y1.max(py);
            nearest_z = nearest_z.min(ndc[2]);
        }
        self.is_screen_rect_occluded(x0, y0, x1, y1, nearest_z)
    }

    /// Run [`Self::cull_world_aabb`] over a batch of `(aabb_min, aabb_max)`
    /// world AABBs sharing one `view_proj`, returning aggregate counters.
    pub fn cull_meshlet_batch(
        &self,
        view_proj: &[[f32; 4]; 4],
        aabbs: &[([f32; 3], [f32; 3])],
    ) -> HiZCullStats {
        let tested = aabbs.len() as u32;
        let culled = aabbs
            .iter()
            .filter(|&&(aabb_min, aabb_max)| self.cull_world_aabb(view_proj, aabb_min, aabb_max))
            .count() as u32;
        HiZCullStats {
            tested,
            culled,
            visible: tested - culled,
        }
    }
}
