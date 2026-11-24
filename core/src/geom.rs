// SPDX-License-Identifier: LGPL-3.0-or-later
//! Small canonical floating-point bounds and view-frustum primitives.

/// A plane represented as `[a, b, c, d]`, with inside points at dot >= 0.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Plane(pub [f32; 4]);

impl Plane {
    /// Normalize the normal and distance together; a zero normal is unchanged.
    pub fn normalized(self) -> Self {
        let length = (self.0[0] * self.0[0] + self.0[1] * self.0[1] + self.0[2] * self.0[2]).sqrt();
        if length == 0.0 || !length.is_finite() {
            return self;
        }
        Self([
            self.0[0] / length,
            self.0[1] / length,
            self.0[2] / length,
            self.0[3] / length,
        ])
    }
}

/// Six normalized clipping planes.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Frustum {
    pub planes: [Plane; 6],
}

impl Frustum {
    /// Extract left/right, bottom/top and near/far planes from a row-major
    /// view-projection matrix using Gribb-Hartmann row combinations.
    pub fn from_view_proj(vp: &[[f32; 4]; 4]) -> Self {
        let combine = |a: usize, sign: f32| {
            Plane([
                vp[3][0] + sign * vp[a][0],
                vp[3][1] + sign * vp[a][1],
                vp[3][2] + sign * vp[a][2],
                vp[3][3] + sign * vp[a][3],
            ])
            .normalized()
        };
        Self {
            planes: [
                combine(0, 1.0),
                combine(0, -1.0),
                combine(1, 1.0),
                combine(1, -1.0),
                combine(2, 1.0),
                combine(2, -1.0),
            ],
        }
    }

    /// Test an AABB using each plane's positive vertex.
    pub fn contains_aabb_pvertex(&self, min: [f32; 3], max: [f32; 3]) -> bool {
        self.planes.iter().all(|plane| {
            let p = plane.0;
            let x = if p[0] >= 0.0 { max[0] } else { min[0] };
            let y = if p[1] >= 0.0 { max[1] } else { min[1] };
            let z = if p[2] >= 0.0 { max[2] } else { min[2] };
            p[0] * x + p[1] * y + p[2] * z + p[3] >= 0.0
        })
    }

    /// Copy the raw plane coefficients.
    pub fn to_raw_planes(&self) -> [[f32; 4]; 6] {
        self.planes.map(|plane| plane.0)
    }
}

/// Three-dimensional `f32` axis-aligned bounding box.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Aabb3f {
    pub min: [f32; 3],
    pub max: [f32; 3],
}

impl Aabb3f {
    /// Inclusive intersection test.
    pub fn intersects(&self, other: &Self) -> bool {
        (0..3).all(|i| self.min[i] <= other.max[i] && self.max[i] >= other.min[i])
    }
    /// Inclusive point containment.
    pub fn contains_point(&self, point: [f32; 3]) -> bool {
        (0..3).all(|i| point[i] >= self.min[i] && point[i] <= self.max[i])
    }
    /// Expand every side by `amount` (negative values contract).
    pub fn expand(&self, amount: f32) -> Self {
        Self {
            min: self.min.map(|v| v - amount),
            max: self.max.map(|v| v + amount),
        }
    }
    /// Midpoint of the bounds.
    pub fn center(&self) -> [f32; 3] {
        std::array::from_fn(|i| (self.min[i] + self.max[i]) * 0.5)
    }
    /// Half-size of the bounds.
    pub fn extents(&self) -> [f32; 3] {
        std::array::from_fn(|i| (self.max[i] - self.min[i]) * 0.5)
    }
}

/// Three-dimensional `f64` axis-aligned bounding box.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Aabb3d {
    pub min: [f64; 3],
    pub max: [f64; 3],
}

impl Aabb3d {
    /// Inclusive intersection test.
    pub fn intersects(&self, other: &Self) -> bool {
        (0..3).all(|i| self.min[i] <= other.max[i] && self.max[i] >= other.min[i])
    }
    /// Translate both corners.
    pub fn translated(&self, delta: [f64; 3]) -> Self {
        Self {
            min: std::array::from_fn(|i| self.min[i] + delta[i]),
            max: std::array::from_fn(|i| self.max[i] + delta[i]),
        }
    }
    /// Expand every side by `amount` (negative values contract).
    pub fn expanded(&self, amount: f64) -> Self {
        Self {
            min: self.min.map(|v| v - amount),
            max: self.max.map(|v| v + amount),
        }
    }
    /// Smallest AABB containing both bounds.
    pub fn union(&self, other: &Self) -> Self {
        Self {
            min: std::array::from_fn(|i| self.min[i].min(other.min[i])),
            max: std::array::from_fn(|i| self.max[i].max(other.max[i])),
        }
    }
}
