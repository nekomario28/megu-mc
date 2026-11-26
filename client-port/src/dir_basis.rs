// SPDX-License-Identifier: LGPL-3.0-or-later
//! Direction bases and proper cube rotations shared by mesh encoders.
//!
//! The six-entry [`DIR_BASIS`] table uses the exact face order of
//! `client-port/src/mesh.rs` and `client-port/src/section.rs`:
//! `Down = 0`, `Up = 1`, `North = 2`, `South = 3`, `West = 4`, `East = 5`.
//! Coordinates are Minecraft block coordinates (`X` east, `Y` up, `Z` south),
//! and flat arrays use the mesher's y-major layout
//! `y * dim_x * dim_z + z * dim_x + x`.

/// Geometric basis for one axis-aligned cube face.
///
/// `u_axis × v_axis == n`. `origin` is the `(u, v) = (0, 0)` corner of the
/// outward face plane of the unit cube, so a rectangle corner is
/// `p(u, v) = origin + u * width * u_axis + v * height * v_axis`.
/// `horizontal_cw`/`horizontal_ccw` use the Minecraft map yaw order
/// North → East → South → West; for the vertical faces yaw is undefined and
/// both indices point back at the same face.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DirBasis {
    /// Unit outward normal.
    pub n: [i8; 3],
    /// Positive tangent U axis.
    pub u_axis: [i8; 3],
    /// Positive tangent V axis.
    pub v_axis: [i8; 3],
    /// Face-plane origin corner in unit-cube coordinates.
    pub origin: [u8; 3],
    /// Index of the face with the opposite normal.
    pub opposite: usize,
    /// Horizontal clockwise yaw rotation index (self for Up/Down).
    pub horizontal_cw: usize,
    /// Horizontal counterclockwise yaw rotation index (self for Up/Down).
    pub horizontal_ccw: usize,
}

impl DirBasis {
    /// Positive memory stride for a unit step along `axis` in the y-major
    /// `y * dim_x * dim_z + z * dim_x + x` array layout.
    const fn positive_axis_stride(axis: usize, dim_x: isize, dim_z: isize) -> isize {
        match axis {
            0 => 1,
            1 => dim_x * dim_z,
            2 => dim_x,
            _ => panic!("axis must be 0, 1, or 2"),
        }
    }

    /// Signed memory stride for a unit axis vector in the y-major layout.
    pub const fn vector_stride(vector: [i8; 3], dim_x: isize, dim_z: isize) -> isize {
        vector[0] as isize + vector[1] as isize * dim_x * dim_z + vector[2] as isize * dim_x
    }

    /// Axis index of the face normal (`0 = X`, `1 = Y`, `2 = Z`).
    pub const fn normal_axis(&self) -> usize {
        if self.n[0] != 0 {
            0
        } else if self.n[1] != 0 {
            1
        } else {
            2
        }
    }

    /// Signed outward-normal memory stride for this face in a grid whose
    /// horizontal dimensions are `dim_x × dim_z`. With both dimensions 18
    /// this reproduces the padded mesher's six outward stride values; with
    /// both dimensions 16 it reproduces the unpadded section values.
    pub const fn stride_for(&self, dim_x: isize, dim_z: isize) -> isize {
        Self::vector_stride(self.n, dim_x, dim_z)
    }

    /// Positive memory stride along tangent U.
    pub const fn u_stride_for(&self, dim_x: isize, dim_z: isize) -> isize {
        Self::vector_stride(self.u_axis, dim_x, dim_z)
    }

    /// Positive memory stride along tangent V.
    pub const fn v_stride_for(&self, dim_x: isize, dim_z: isize) -> isize {
        Self::vector_stride(self.v_axis, dim_x, dim_z)
    }

    /// The mesher tuple `(slice_stride, u_stride, v_stride, outward_stride)`.
    /// `slice_stride` is positive because section slice indices always ascend
    /// along the positive normal axis even for negative-facing faces.
    pub const fn grid_strides_for(
        &self,
        dim_x: isize,
        dim_z: isize,
    ) -> (isize, isize, isize, isize) {
        (
            Self::positive_axis_stride(self.normal_axis(), dim_x, dim_z),
            self.u_stride_for(dim_x, dim_z),
            self.v_stride_for(dim_x, dim_z),
            self.stride_for(dim_x, dim_z),
        )
    }
}

/// The six cube-face bases in `Direction` order. The tangent assignment is
/// the same outward-winding order used by `mesh.rs`:
/// Down `(X,Z)`, Up `(Z,X)`, North `(Y,X)`, South `(X,Y)`, West `(Z,Y)`,
/// East `(Y,Z)`.
pub const DIR_BASIS: [DirBasis; 6] = [
    // 0: Down (-Y), face plane y = 0.
    DirBasis {
        n: [0, -1, 0],
        u_axis: [1, 0, 0],
        v_axis: [0, 0, 1],
        origin: [0, 0, 0],
        opposite: 1,
        horizontal_cw: 0,
        horizontal_ccw: 0,
    },
    // 1: Up (+Y), face plane y = 1.
    DirBasis {
        n: [0, 1, 0],
        u_axis: [0, 0, 1],
        v_axis: [1, 0, 0],
        origin: [0, 1, 0],
        opposite: 0,
        horizontal_cw: 1,
        horizontal_ccw: 1,
    },
    // 2: North (-Z), face plane z = 0.
    DirBasis {
        n: [0, 0, -1],
        u_axis: [0, 1, 0],
        v_axis: [1, 0, 0],
        origin: [0, 0, 0],
        opposite: 3,
        horizontal_cw: 5,
        horizontal_ccw: 4,
    },
    // 3: South (+Z), face plane z = 1.
    DirBasis {
        n: [0, 0, 1],
        u_axis: [1, 0, 0],
        v_axis: [0, 1, 0],
        origin: [0, 0, 1],
        opposite: 2,
        horizontal_cw: 4,
        horizontal_ccw: 5,
    },
    // 4: West (-X), face plane x = 0.
    DirBasis {
        n: [-1, 0, 0],
        u_axis: [0, 0, 1],
        v_axis: [0, 1, 0],
        origin: [0, 0, 0],
        opposite: 5,
        horizontal_cw: 2,
        horizontal_ccw: 3,
    },
    // 5: East (+X), face plane x = 1.
    DirBasis {
        n: [1, 0, 0],
        u_axis: [0, 1, 0],
        v_axis: [0, 0, 1],
        origin: [1, 0, 0],
        opposite: 4,
        horizontal_cw: 3,
        horizontal_ccw: 2,
    },
];

const PERMUTATIONS: [[usize; 3]; 6] = [
    [0, 1, 2],
    [0, 2, 1],
    [1, 0, 2],
    [1, 2, 0],
    [2, 0, 1],
    [2, 1, 0],
];

/// A proper cube orientation: a signed axis-permutation matrix with
/// determinant `+1`. Matrices are row-major and act on column vectors, so
/// `a.compose(b)` is the orientation obtained by applying `b` first and then
/// `a` (ordinary matrix multiplication `a * b`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct CubeOrientation {
    pub matrix: [[i8; 3]; 3],
}

impl CubeOrientation {
    pub const IDENTITY: Self = Self {
        matrix: [[1, 0, 0], [0, 1, 0], [0, 0, 1]],
    };

    /// All 24 proper rotations of the cube. Reflections (`det = -1`) are not
    /// orientations and are excluded.
    pub const ALL: [Self; 24] = build_cube_orientations();

    /// Matrix product `self * rhs`.
    pub const fn compose(self, rhs: Self) -> Self {
        let mut out = [[0i8; 3]; 3];
        let mut row = 0usize;
        while row < 3 {
            let mut col = 0usize;
            while col < 3 {
                let mut sum = 0i8;
                let mut k = 0usize;
                while k < 3 {
                    sum += self.matrix[row][k] * rhs.matrix[k][col];
                    k += 1;
                }
                out[row][col] = sum;
                col += 1;
            }
            row += 1;
        }
        Self { matrix: out }
    }

    /// Inverse rotation. Every entry is orthonormal, so inversion is exactly
    /// matrix transposition.
    pub const fn inverse(self) -> Self {
        let mut out = [[0i8; 3]; 3];
        let mut row = 0usize;
        while row < 3 {
            let mut col = 0usize;
            while col < 3 {
                out[row][col] = self.matrix[col][row];
                col += 1;
            }
            row += 1;
        }
        Self { matrix: out }
    }

    /// Rotate a signed unit vector (normally a face normal or axis).
    pub const fn rotate_normal(self, vector: [i8; 3]) -> [i8; 3] {
        [
            self.matrix[0][0] * vector[0]
                + self.matrix[0][1] * vector[1]
                + self.matrix[0][2] * vector[2],
            self.matrix[1][0] * vector[0]
                + self.matrix[1][1] * vector[1]
                + self.matrix[1][2] * vector[2],
            self.matrix[2][0] * vector[0]
                + self.matrix[2][1] * vector[1]
                + self.matrix[2][2] * vector[2],
        ]
    }

    /// Rotate an integer point/vector about the coordinate origin.
    pub const fn rotate_point(self, point: [i32; 3]) -> [i32; 3] {
        let mut out = [0i32; 3];
        let mut row = 0usize;
        while row < 3 {
            let mut sum = 0i32;
            let mut col = 0usize;
            while col < 3 {
                sum += self.matrix[row][col] as i32 * point[col];
                col += 1;
            }
            out[row] = sum;
            row += 1;
        }
        out
    }
}

const fn orientation_determinant(matrix: [[i8; 3]; 3]) -> i32 {
    let a = matrix[0][0] as i32;
    let b = matrix[0][1] as i32;
    let c = matrix[0][2] as i32;
    let d = matrix[1][0] as i32;
    let e = matrix[1][1] as i32;
    let f = matrix[1][2] as i32;
    let g = matrix[2][0] as i32;
    let h = matrix[2][1] as i32;
    let i = matrix[2][2] as i32;
    a * (e * i - f * h) - b * (d * i - f * g) + c * (d * h - e * g)
}

const fn build_cube_orientations() -> [CubeOrientation; 24] {
    let mut out = [CubeOrientation::IDENTITY; 24];
    let mut out_len = 0usize;
    let mut permutation = 0usize;
    while permutation < PERMUTATIONS.len() {
        let columns = PERMUTATIONS[permutation];
        let mut sign_bits = 0usize;
        while sign_bits < 8 {
            let mut matrix = [[0i8; 3]; 3];
            let mut row = 0usize;
            while row < 3 {
                let sign = if sign_bits & (1usize << row) == 0 {
                    1i8
                } else {
                    -1i8
                };
                matrix[row][columns[row]] = sign;
                row += 1;
            }
            if orientation_determinant(matrix) == 1 {
                out[out_len] = CubeOrientation { matrix };
                out_len += 1;
            }
            sign_bits += 1;
        }
        permutation += 1;
    }
    assert!(
        out_len == 24,
        "the proper cube rotation group has 24 elements"
    );
    out
}

#[cfg(test)]
mod dir_basis_tests {
    use super::*;
    use std::collections::BTreeSet;

    fn cross(a: [i8; 3], b: [i8; 3]) -> [i8; 3] {
        [
            a[1] * b[2] - a[2] * b[1],
            a[2] * b[0] - a[0] * b[2],
            a[0] * b[1] - a[1] * b[0],
        ]
    }

    #[test]
    fn dir_basis_faces_are_right_handed_and_mapped() {
        let expected_normals = [
            [0, -1, 0],
            [0, 1, 0],
            [0, 0, -1],
            [0, 0, 1],
            [-1, 0, 0],
            [1, 0, 0],
        ];
        for (index, basis) in DIR_BASIS.iter().enumerate() {
            assert_eq!(basis.n, expected_normals[index], "face {index} normal");
            assert_eq!(cross(basis.u_axis, basis.v_axis), basis.n, "face {index}");
            assert_eq!(basis.opposite, index ^ 1, "face {index} opposite");
            assert_eq!(DIR_BASIS[basis.opposite].n, basis.n.map(|v| -v));
        }
        assert_eq!(DIR_BASIS[2].horizontal_cw, 5, "north clockwise is east");
        assert_eq!(DIR_BASIS[5].horizontal_cw, 3, "east clockwise is south");
        assert_eq!(DIR_BASIS[3].horizontal_cw, 4, "south clockwise is west");
        assert_eq!(DIR_BASIS[4].horizontal_cw, 2, "west clockwise is north");
        for face in [2usize, 3, 4, 5] {
            let cw = DIR_BASIS[face].horizontal_cw;
            assert_eq!(DIR_BASIS[cw].horizontal_ccw, face);
        }
    }

    #[test]
    fn dir_basis_strides_match_padded_and_unpadded_mesh_grids() {
        let padded = [
            (324, 1, 18, -324),
            (324, 18, 1, 324),
            (18, 324, 1, -18),
            (18, 1, 324, 18),
            (1, 18, 324, -1),
            (1, 324, 18, 1),
        ];
        let unpadded = [
            (256, 1, 16, -256),
            (256, 16, 1, 256),
            (16, 256, 1, -16),
            (16, 1, 256, 16),
            (1, 16, 256, -1),
            (1, 256, 16, 1),
        ];
        for (index, basis) in DIR_BASIS.iter().enumerate() {
            assert_eq!(
                basis.grid_strides_for(18, 18),
                padded[index],
                "padded face {index}"
            );
            assert_eq!(
                basis.grid_strides_for(16, 16),
                unpadded[index],
                "unpadded face {index}"
            );
        }
        assert_eq!(
            DIR_BASIS.map(|basis| basis.stride_for(18, 18)),
            [-324, 324, -18, 18, -1, 1]
        );
        assert_eq!(
            DIR_BASIS.map(|basis| basis.stride_for(16, 16)),
            [-256, 256, -16, 16, -1, 1]
        );
    }

    #[test]
    fn cube_orientations_are_the_24_element_proper_rotation_group() {
        assert_eq!(CubeOrientation::ALL.len(), 24);
        let mut unique = BTreeSet::new();
        for orientation in CubeOrientation::ALL {
            assert_eq!(orientation_determinant(orientation.matrix), 1);
            assert!(
                unique.insert(orientation.matrix),
                "duplicate matrix {orientation:?}"
            );
            assert_eq!(
                orientation.compose(orientation.inverse()),
                CubeOrientation::IDENTITY
            );
            assert_eq!(
                orientation.inverse().compose(orientation),
                CubeOrientation::IDENTITY
            );
        }
        assert_eq!(unique.len(), 24);

        for a in CubeOrientation::ALL {
            for b in CubeOrientation::ALL {
                let product = a.compose(b);
                assert_eq!(orientation_determinant(product.matrix), 1);
                assert!(
                    CubeOrientation::ALL.contains(&product),
                    "composition escaped the group: {a:?} * {b:?}"
                );
            }
        }
    }

    #[test]
    fn cube_orientation_rotates_points_and_normals() {
        // Ninety degrees about +Y: X' = Z and Z' = -X.
        let rotation = CubeOrientation {
            matrix: [[0, 0, 1], [0, 1, 0], [-1, 0, 0]],
        };
        assert!(CubeOrientation::ALL.contains(&rotation));
        assert_eq!(rotation.rotate_normal([1, 0, 0]), [0, 0, -1]);
        assert_eq!(rotation.rotate_normal([0, 0, 1]), [1, 0, 0]);
        assert_eq!(rotation.rotate_point([2, 3, 4]), [4, 3, -2]);
        assert_eq!(rotation.inverse().rotate_point([4, 3, -2]), [2, 3, 4]);
    }
}
