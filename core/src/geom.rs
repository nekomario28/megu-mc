// SPDX-License-Identifier: LGPL-3.0-or-later
#![forbid(unsafe_code)]
//! `megu_core::geom` — the canonical view-frustum plane set and the
//! positive-vertex AABB test every culler shares.
//!
//! Two spellings of one contract live here, and nothing else:
//!
//! * [`aabb_in_frustum`] — the p-vertex (positive vertex) six-dot-product
//!   test over an arbitrary-length `&[Plane]` slice, in the same
//!   `dot(n, p) + d >= 0` "inside" form `box_in_frustum` and the section
//!   cull compute shader use. It answers `false` only when the box lies
//!   *entirely* outside at least one plane, so it has zero false negatives
//!   and a culler may never hide geometry a conforming renderer would draw.
//! * [`Frustum`] — the fixed `[Plane; 6]` set with
//!   [`Frustum::from_view_proj`] extraction and
//!   [`Frustum::contains_aabb_pvertex`], for callers that keep one frustum
//!   per frame instead of re-deriving planes per batch.
//!
//! The p-vertex rule is the whole correctness story. For a plane
//! `[nx, ny, nz, d]` the corner of `[min, max]` farthest along the normal is
//! `min` on an axis whose normal component is negative and `max` otherwise
//! ([`aabb_pvertex`]). If even that corner is behind the plane the box is
//! fully behind it; if any nearer corner is in front, the plane cannot
//! decide the box and the next plane is asked. Comparison is sign-based
//! (`>= 0.0`), not an epsilon test — the GPU shader this mirrors has no
//! epsilon either, and an epsilon here would cull boxes the GPU still draws.
//!
//! Both tests fail closed: a `NaN` bound makes every comparison false, so
//! the box is culled rather than admitted with an unknown verdict. A plane
//! with a zero or non-finite normal is kept verbatim by
//! [`normalize_plane`], which degrades it to a constant `d` test instead of
//! producing `NaN` coefficients.
//!
//! Plane extraction is Gribb-Hartmann over a *row-major* `view_proj`
//! (`rows[row][col]`, clip = `rows · [x, y, z, 1]`) with the clip-Z
//! convention `ndc_z` in `[-1, 1]` — the OpenGL/Java one vanilla's
//! `Projection` matrix produces. A near plane that is `clip_z >= 0` alone
//! (the `[0, 1]` wgpu/Direct3D convention) is a *different* airplane and
//! deliberately not offered here: a pipeline that needs it must keep
//! extracting it locally, because swapping one in silently moves the near
//! plane by half the depth range.
//!
//! Pure `std`, zero dependencies, `unsafe`-free.

/// One plane in `box_in_frustum` form: `[nx, ny, nz, d]`, inside half-space
/// `dot(n, p) + d >= 0`.
pub type Plane = [f32; 4];

/// Number of planes a rectangular view frustum has.
pub const FRUSTUM_PLANE_COUNT: usize = 6;

/// Signed distance from `point` to `plane`: `dot(n, p) + d`, positive
/// inside. Units are world units when the plane came from
/// [`Frustum::from_view_proj`] (which normalizes), and arbitrary scale when
/// it did not — the sign is what [`aabb_in_frustum`] reads either way.
#[inline]
pub fn plane_distance(plane: Plane, point: [f32; 3]) -> f32 {
    plane[0] * point[0] + plane[1] * point[1] + plane[2] * point[2] + plane[3]
}

/// Scale `plane` so its normal is unit length, making
/// [`plane_distance`] a true world-unit distance — the form
/// [`Frustum::from_view_proj`] produces.
///
/// A degenerate plane (zero-length or non-finite normal, which extraction
/// can only produce from a degenerate matrix) is returned unchanged: dividing
/// by its length would turn every coefficient into `NaN` and every test into
/// a silent cull.
#[inline]
pub fn normalize_plane(plane: Plane) -> Plane {
    let [nx, ny, nz, d] = plane;
    let len = (nx * nx + ny * ny + nz * nz).sqrt();
    if len > 0.0 && len.is_finite() {
        [nx / len, ny / len, nz / len, d / len]
    } else {
        plane
    }
}

/// The corner of `[min, max]` farthest along `plane`'s normal — the
/// positive vertex (p-vertex) the six-plane test tests.
#[inline]
pub fn aabb_pvertex(min: [f32; 3], max: [f32; 3], plane: Plane) -> [f32; 3] {
    [
        if plane[0] >= 0.0 { max[0] } else { min[0] },
        if plane[1] >= 0.0 { max[1] } else { min[1] },
        if plane[2] >= 0.0 { max[2] } else { min[2] },
    ]
}

/// Whether `[min, max]` survives the positive-vertex test against `planes`:
/// `false` only when the box lies entirely outside at least one of them.
///
/// `planes` is a slice, not a `[Plane; FRUSTUM_PLANE_COUNT]`, so single-plane
/// unit tests and non-rectangular cull volumes (a shadow cascade's tight
/// cone, a portal's slab) can share the exact arithmetic of the six-plane
/// case. An empty slice reports `true` — no plane rejects the box.
#[inline]
pub fn aabb_in_frustum(min: [f32; 3], max: [f32; 3], planes: &[Plane]) -> bool {
    planes
        .iter()
        .all(|&plane| plane_distance(plane, aabb_pvertex(min, max, plane)) >= 0.0)
}

/// Gribb-Hartmann plane extraction from a row-major `view_proj`
/// (`rows[row][col]`), clip-Z in `[-1, 1]`, **unnormalized**.
///
/// Plane order is left, right, bottom, top, near, far — the order
/// [`Frustum`] keeps, and the order vanilla's `Frustum` counters index.
/// Each plane is `row3 ± rowI`, because the inside half-space of a clip-space
/// bound `a·clip >= 0` is `a·(rows · p) >= 0`, i.e. the row combination
/// dotted with the world point.
pub fn planes_from_view_proj(rows: &[[f32; 4]; 4]) -> [Plane; FRUSTUM_PLANE_COUNT] {
    let [r0, r1, r2, r3] = *rows;
    let sum = |a: Plane, b: Plane| [a[0] + b[0], a[1] + b[1], a[2] + b[2], a[3] + b[3]];
    let diff = |a: Plane, b: Plane| [a[0] - b[0], a[1] - b[1], a[2] - b[2], a[3] - b[3]];
    [
        sum(r3, r0),  // left:   clip_x + clip_w >= 0
        diff(r3, r0), // right:  clip_w - clip_x >= 0
        sum(r3, r1),  // bottom: clip_y + clip_w >= 0
        diff(r3, r1), // top:    clip_w - clip_y >= 0
        sum(r3, r2),  // near:   clip_z + clip_w >= 0  (ndc_z >= -1)
        diff(r3, r2), // far:    clip_w - clip_z >= 0  (ndc_z <= 1)
    ]
}

/// A rectangular view frustum as six normalized `dot(n, p) + d >= 0` planes.
///
/// Built once per frame from the camera's `view_proj`
/// ([`Frustum::from_view_proj`]) and asked about every box a culler
/// considers. [`Frustum::planes`] hands the raw sextet to batch kernels that
/// take `&[Plane; 6]`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Frustum {
    planes: [Plane; FRUSTUM_PLANE_COUNT],
}

impl Frustum {
    /// Frustum from an already-normalized plane sextet, in left, right,
    /// bottom, top, near, far order. Callers that only have unnormalized
    /// coefficients use [`Frustum::from_view_proj`] instead.
    pub const fn new(planes: [Plane; FRUSTUM_PLANE_COUNT]) -> Self {
        Self { planes }
    }

    /// Frustum from a row-major `view_proj` with clip-Z in `[-1, 1]`, with
    /// every plane normalized so [`plane_distance`] reads world units.
    pub fn from_view_proj(rows: &[[f32; 4]; 4]) -> Self {
        let mut planes = planes_from_view_proj(rows);
        for plane in &mut planes {
            *plane = normalize_plane(*plane);
        }
        Self { planes }
    }

    /// The six planes, in left, right, bottom, top, near, far order.
    pub const fn planes(&self) -> &[Plane; FRUSTUM_PLANE_COUNT] {
        &self.planes
    }

    /// Whether `point` is inside (or exactly on) all six planes.
    pub fn contains_point(&self, point: [f32; 3]) -> bool {
        self.planes
            .iter()
            .all(|&plane| plane_distance(plane, point) >= 0.0)
    }

    /// Positive-vertex test for `[min, max]` against all six planes — the
    /// fixed-length form of [`aabb_in_frustum`], and the per-plane math
    /// `aabb_in_frustum_pvertex` mirrors on the client.
    pub fn contains_aabb_pvertex(&self, min: [f32; 3], max: [f32; 3]) -> bool {
        aabb_in_frustum(min, max, &self.planes)
    }

    /// Positive-vertex test for an [`Aabb3`] — the box type
    /// [`crate::bvh`] and the physics queries already carry, so a caller
    /// holding one does not unpack it by hand.
    pub fn contains_aabb(&self, aabb: &crate::bvh::Aabb3) -> bool {
        self.contains_aabb_pvertex(aabb.min, aabb.max)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// OpenGL/Java-style perspective matrix, row-major, clip-Z in `[-1, 1]`:
    /// the convention [`Frustum::from_view_proj`] extracts for. The camera
    /// looks down `-z`, so a visible point has a negative `z`.
    fn perspective(fov_y_deg: f32, aspect: f32, near: f32, far: f32) -> [[f32; 4]; 4] {
        let f = 1.0 / (fov_y_deg.to_radians() * 0.5).tan();
        [
            [f / aspect, 0.0, 0.0, 0.0],
            [0.0, f, 0.0, 0.0],
            [
                0.0,
                0.0,
                (far + near) / (near - far),
                2.0 * far * near / (near - far),
            ],
            [0.0, 0.0, -1.0, 0.0],
        ]
    }

    fn unit_frustum() -> Frustum {
        Frustum::from_view_proj(&perspective(70.0, 1.0, 0.1, 1000.0))
    }

    #[test]
    fn normalized_planes_report_world_unit_distances() {
        let frustum = Frustum::from_view_proj(&perspective(70.0, 1.0, 0.5, 100.0));
        for &[nx, ny, nz, d] in frustum.planes() {
            let len = (nx * nx + ny * ny + nz * nz).sqrt();
            assert!((len - 1.0).abs() < 1e-5, "normal not unit length: {len}");
            assert!(d.is_finite(), "plane offset became non-finite");
        }
        // The near plane sits at z = -0.5: dead on it reads 0, one world unit
        // behind it reads +1 — a distance only if the plane is normalized.
        let near = frustum.planes()[4];
        assert!(plane_distance(near, [0.0, 0.0, -0.5]).abs() < 1e-4);
        assert!((plane_distance(near, [0.0, 0.0, -1.5]) - 1.0).abs() < 1e-4);
    }

    #[test]
    fn boxes_in_front_are_kept_and_boxes_behind_the_camera_are_culled() {
        let frustum = unit_frustum();
        assert!(frustum.contains_aabb_pvertex([-0.1, -0.1, -5.0], [0.1, 0.1, -3.0]));
        // Behind the camera: entirely outside the near plane.
        assert!(!frustum.contains_aabb_pvertex([-0.1, -0.1, 1.0], [0.1, 0.1, 2.0]));
        // Inside the near plane, in front of the camera.
        assert!(!frustum.contains_aabb_pvertex([-0.1, -0.1, -0.05], [0.1, 0.1, 0.05]));
        // Beyond the far plane.
        assert!(!frustum.contains_aabb_pvertex([-1.0, -1.0, -20_000.0], [1.0, 1.0, -19_990.0]));
        // Far to the side: the 70° half-angle at z = -5 is ~3.5 blocks.
        assert!(!frustum.contains_aabb_pvertex([10.0, -0.1, -5.0], [12.0, 0.1, -3.0]));
        assert!(frustum.contains_aabb_pvertex([-3.0, -0.1, -5.0], [-2.0, 0.1, -3.0]));
    }

    #[test]
    fn points_on_the_axis_project_inside_the_unit_frustum() {
        let frustum = unit_frustum();
        assert!(frustum.contains_point([0.0, 0.0, -5.0]));
        assert!(frustum.contains_point([0.0, 0.0, -0.2]));
        // The apex is behind the near plane, not inside the frustum.
        assert!(!frustum.contains_point([0.0, 0.0, 0.0]));
        assert!(!frustum.contains_point([0.0, 0.0, 5.0]));
        assert!(!frustum.contains_point([0.0, 0.0, -1_001.0]));
    }

    #[test]
    fn p_vertex_uses_the_far_corner_along_each_normal() {
        // x >= 0 inside, so the p-vertex is `max.x` for the positive normal
        // and `min.x` for the negative one.
        let positive: Plane = [1.0, 0.0, 0.0, 0.0];
        let negative: Plane = [-1.0, 0.0, 0.0, 0.0];
        assert_eq!(
            aabb_pvertex([-2.0, 0.0, 0.0], [0.5, 1.0, 1.0], positive),
            [0.5, 1.0, 1.0]
        );
        assert_eq!(
            aabb_pvertex([-2.0, 0.0, 0.0], [0.5, 1.0, 1.0], negative),
            [-2.0, 1.0, 1.0]
        );
        // Straddling the plane: the farthest corner is inside, so it survives.
        assert!(aabb_in_frustum(
            [-2.0, 0.0, 0.0],
            [0.5, 1.0, 1.0],
            &[positive]
        ));
        // Fully on the outside: the farthest corner is behind the plane, so
        // the box is rejected — and this is the only case that may reject.
        assert!(!aabb_in_frustum(
            [-3.0, 0.0, 0.0],
            [-1.0, 1.0, 1.0],
            &[positive]
        ));
        assert!(!aabb_in_frustum(
            [1.0, 0.0, 0.0],
            [2.0, 1.0, 1.0],
            &[negative]
        ));
    }

    #[test]
    fn an_empty_plane_slice_keeps_every_box() {
        assert!(aabb_in_frustum([-1.0; 3], [1.0; 3], &[]));
    }

    #[test]
    fn nan_bounds_fail_closed_instead_of_being_drawn() {
        assert!(!aabb_in_frustum(
            [f32::NAN; 3],
            [1.0; 3],
            &unit_frustum().planes()[..]
        ));
        assert!(!unit_frustum().contains_aabb_pvertex([-1.0; 3], [f32::NAN; 3]));
    }

    #[test]
    fn degenerate_planes_are_kept_verbatim() {
        assert_eq!(normalize_plane([0.0, 0.0, 0.0, 5.0]), [0.0, 0.0, 0.0, 5.0]);
        let nan_plane: Plane = [f32::NAN, 0.0, 0.0, 0.0];
        // Bit-for-bit, since `NaN != NaN` would make a plain equality a lie.
        assert_eq!(
            normalize_plane(nan_plane).map(f32::to_bits),
            nan_plane.map(f32::to_bits)
        );
        // A zero normal degrades to a constant `d` test rather than `NaN`.
        assert!(aabb_in_frustum(
            [-1.0; 3],
            [1.0; 3],
            &[[0.0, 0.0, 0.0, 5.0]]
        ));
    }

    #[test]
    fn extracted_planes_match_the_matrix_rows() {
        let rows = perspective(90.0, 1.0, 1.0, 3.0);
        let planes = planes_from_view_proj(&rows);
        let [r0, r1, r2, r3] = rows;
        // left = row3 + row0, top = row3 - row1, near = row3 + row2, far =
        // row3 - row2, componentwise.
        for axis in 0..4 {
            assert_eq!(planes[0][axis], r3[axis] + r0[axis]);
            assert_eq!(planes[3][axis], r3[axis] - r1[axis]);
            assert_eq!(planes[4][axis], r3[axis] + r2[axis]);
            assert_eq!(planes[5][axis], r3[axis] - r2[axis]);
        }
    }

    #[test]
    fn aabb_convenience_matches_the_min_max_form() {
        let frustum = unit_frustum();
        let aabb = crate::bvh::Aabb3::new([-0.1, -0.1, -5.0], [0.1, 0.1, -3.0]);
        assert_eq!(
            frustum.contains_aabb(&aabb),
            frustum.contains_aabb_pvertex(aabb.min, aabb.max)
        );
    }

    #[test]
    fn constructed_frustum_round_trips_its_planes() {
        let planes: [Plane; FRUSTUM_PLANE_COUNT] = [
            [1.0, 0.0, 0.0, 0.0],
            [-1.0, 0.0, 0.0, 10.0],
            [0.0, 1.0, 0.0, 0.0],
            [0.0, -1.0, 0.0, 10.0],
            [0.0, 0.0, 1.0, 0.0],
            [0.0, 0.0, -1.0, 10.0],
        ];
        let frustum = Frustum::new(planes);
        assert_eq!(*frustum.planes(), planes);
        assert!(frustum.contains_point([5.0, 5.0, 5.0]));
        assert!(!frustum.contains_point([15.0, 5.0, 5.0]));
    }
}
