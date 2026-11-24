// SPDX-License-Identifier: LGPL-3.0-or-later
//! Domain-independent mathematical building blocks for spatial simulation.
//!
//! Bounds are conservative and may decline to skip a tile. Integral tables
//! answer inclusive, axis-aligned box queries after O(number of cells) setup.
//! Floating-point sums have normal rounding error, so callers requiring
//! bit-identical accumulation order must check that contract separately.

/// A proof of the sign of every point in a Lipschitz-bounded tile.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TileSignDecision {
    UniformPositive,
    UniformNegative,
    MixedRequiresEvaluation,
}

/// Stateless entry point for conservative tile sign classification.
pub struct LipschitzTileClassifier;

impl LipschitzTileClassifier {
    pub fn classify_subtile_lipschitz(
        center_value: f64,
        lipschitz_bound: f64,
        half_extents: [f64; 3],
    ) -> TileSignDecision {
        classify_subtile_lipschitz(center_value, lipschitz_bound, half_extents)
    }
}

// The next representable doubles are used to round interval endpoints outwards.
// Standard binary64 operations round to nearest; one ULP outward contains the
// exact result of an individual arithmetic operation.
fn next_up(value: f64) -> f64 {
    if value.is_nan() || value == f64::INFINITY {
        value
    } else if value == 0.0 {
        f64::from_bits(1)
    } else if value > 0.0 {
        f64::from_bits(value.to_bits() + 1)
    } else {
        f64::from_bits(value.to_bits() - 1)
    }
}

fn next_down(value: f64) -> f64 {
    if value.is_nan() || value == f64::NEG_INFINITY {
        value
    } else if value == 0.0 {
        -f64::from_bits(1)
    } else if value > 0.0 {
        f64::from_bits(value.to_bits() - 1)
    } else {
        f64::from_bits(value.to_bits() + 1)
    }
}

fn mul_upper(lhs: f64, rhs: f64) -> f64 {
    next_up(lhs * rhs)
}

/// Conservatively classify a box given a global Euclidean Lipschitz bound.
///
/// A finite bound `L >= 0` guarantees `|f(p) - f(center)| <= L * distance`.
/// The largest distance inside the box is the Euclidean norm of its half
/// extents. The radius and its multiplication by `L` are rounded outwards;
/// equality is always treated as mixed because a strictly signed result is
/// required. Non-finite inputs and overflowing bounds also return mixed.
pub fn classify_subtile_lipschitz(
    center_value: f64,
    lipschitz_bound: f64,
    half_extents: [f64; 3],
) -> TileSignDecision {
    use TileSignDecision::{MixedRequiresEvaluation, UniformNegative, UniformPositive};

    if !center_value.is_finite()
        || !lipschitz_bound.is_finite()
        || lipschitz_bound < 0.0
        || half_extents.iter().any(|extent| !extent.is_finite())
    {
        return MixedRequiresEvaluation;
    }

    // Scaling by the largest component avoids squaring into overflow or
    // underflow. Each intermediate bound is rounded upward, including sqrt.
    let extents = half_extents.map(f64::abs);
    let scale = extents.into_iter().fold(0.0_f64, f64::max);
    let radius_upper = if scale == 0.0 {
        0.0
    } else {
        let norm = extents.map(|extent| next_up(extent / scale));
        let squared = norm.map(|component| mul_upper(component, component));
        let square_sum = next_up(next_up(squared[0] + squared[1]) + squared[2]);
        mul_upper(scale, next_up(square_sum.sqrt()))
    };
    let variation_upper = mul_upper(lipschitz_bound, radius_upper);
    if center_value > variation_upper {
        UniformPositive
    } else if center_value < -variation_upper {
        UniformNegative
    } else {
        MixedRequiresEvaluation
    }
}

/// An inclusive floating-point interval. Invalid intervals propagate to the
/// unbounded interval `[-infinity, infinity]` during arithmetic.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct IntervalF64 {
    pub min: f64,
    pub max: f64,
}

impl IntervalF64 {
    const WHOLE: Self = Self {
        min: f64::NEG_INFINITY,
        max: f64::INFINITY,
    };

    fn valid(self) -> bool {
        self.min <= self.max
    }

    /// Outward-rounded interval sum.
    #[allow(clippy::should_implement_trait)] // Public API requires named interval operations.
    pub fn add(self, other: Self) -> Self {
        if !self.valid() || !other.valid() {
            return Self::WHOLE;
        }
        let min = self.min + other.min;
        let max = self.max + other.max;
        if min.is_nan() || max.is_nan() {
            Self::WHOLE
        } else {
            Self {
                min: next_down(min),
                max: next_up(max),
            }
        }
    }

    /// Outward-rounded interval difference.
    #[allow(clippy::should_implement_trait)] // Public API requires named interval operations.
    pub fn sub(self, other: Self) -> Self {
        if !self.valid() || !other.valid() {
            return Self::WHOLE;
        }
        let min = self.min - other.max;
        let max = self.max - other.min;
        if min.is_nan() || max.is_nan() {
            Self::WHOLE
        } else {
            Self {
                min: next_down(min),
                max: next_up(max),
            }
        }
    }

    /// Outward-rounded interval product over all four endpoint combinations.
    #[allow(clippy::should_implement_trait)] // Public API requires named interval operations.
    pub fn mul(self, other: Self) -> Self {
        if !self.valid() || !other.valid() {
            return Self::WHOLE;
        }
        let products = [
            self.min * other.min,
            self.min * other.max,
            self.max * other.min,
            self.max * other.max,
        ];
        if products.iter().any(|product| product.is_nan()) {
            return Self::WHOLE;
        }
        let min = products.iter().copied().fold(f64::INFINITY, f64::min);
        let max = products.iter().copied().fold(f64::NEG_INFINITY, f64::max);
        Self {
            min: next_down(min),
            max: next_up(max),
        }
    }

    /// Outward-rounded multiplication by a scalar.
    pub fn scale(self, factor: f64) -> Self {
        if !self.valid() || factor.is_nan() {
            return Self::WHOLE;
        }
        let a = self.min * factor;
        let b = self.max * factor;
        if a.is_nan() || b.is_nan() {
            Self::WHOLE
        } else {
            Self {
                min: next_down(a.min(b)),
                max: next_up(a.max(b)),
            }
        }
    }

    pub fn contains_zero(self) -> bool {
        !self.valid() || (self.min <= 0.0 && self.max >= 0.0)
    }

    pub fn strictly_positive(self) -> bool {
        self.valid() && self.min > 0.0
    }

    pub fn strictly_negative(self) -> bool {
        self.valid() && self.max < 0.0
    }
}

/// Summed-area table with x as the fastest-changing input coordinate:
/// `values[z * width + x]`. Constructors return `None` for inconsistent
/// dimensions or non-finite data/prefixes.
#[derive(Clone, Debug)]
pub struct IntegralGrid2D {
    width: usize,
    height: usize,
    stride: usize,
    prefix: Vec<f64>,
}

impl IntegralGrid2D {
    pub fn new(width: usize, height: usize, values: &[f64]) -> Option<Self> {
        if width.checked_mul(height)? != values.len() {
            return None;
        }
        let stride = width.checked_add(1)?;
        let len = stride.checked_mul(height.checked_add(1)?)?;
        let mut prefix = vec![0.0; len];
        for z in 0..height {
            for x in 0..width {
                let value = values[z * width + x];
                let i = (z + 1) * stride + x + 1;
                let sum = value + prefix[i - 1] + prefix[i - stride] - prefix[i - stride - 1];
                if !sum.is_finite() {
                    return None;
                }
                prefix[i] = sum;
            }
        }
        Some(Self {
            width,
            height,
            stride,
            prefix,
        })
    }

    /// Inclusive box sum, intersected with the grid. Empty boxes sum to zero.
    pub fn box_sum(&self, x0: usize, z0: usize, x1: usize, z1: usize) -> f64 {
        let Some((x0, z0, x1, z1)) = self.bounds(x0, z0, x1, z1) else {
            return 0.0;
        };
        let a = self.prefix[(z1 + 1) * self.stride + x1 + 1];
        let b = self.prefix[z0 * self.stride + x1 + 1];
        let c = self.prefix[(z1 + 1) * self.stride + x0];
        let d = self.prefix[z0 * self.stride + x0];
        a - b - c + d
    }

    /// Inclusive box mean, returning zero for an empty intersection.
    pub fn box_mean(&self, x0: usize, z0: usize, x1: usize, z1: usize) -> f64 {
        let Some((x0, z0, x1, z1)) = self.bounds(x0, z0, x1, z1) else {
            return 0.0;
        };
        let cells = (x1 - x0 + 1) * (z1 - z0 + 1);
        self.box_sum(x0, z0, x1, z1) / cells as f64
    }

    fn bounds(
        &self,
        x0: usize,
        z0: usize,
        x1: usize,
        z1: usize,
    ) -> Option<(usize, usize, usize, usize)> {
        if x0 > x1 || z0 > z1 || x0 >= self.width || z0 >= self.height {
            None
        } else {
            Some((x0, z0, x1.min(self.width - 1), z1.min(self.height - 1)))
        }
    }
}

/// Three-channel summed-area table using the same x-fastest layout and
/// inclusive clamped rectangles as [`IntegralGrid2D`].
#[derive(Clone, Debug)]
pub struct IntegralRgbGrid2D {
    width: usize,
    height: usize,
    stride: usize,
    prefix: Vec<[f64; 3]>,
}

impl IntegralRgbGrid2D {
    pub fn new(width: usize, height: usize, values: &[[f64; 3]]) -> Option<Self> {
        if width.checked_mul(height)? != values.len() {
            return None;
        }
        let stride = width.checked_add(1)?;
        let len = stride.checked_mul(height.checked_add(1)?)?;
        let mut prefix = vec![[0.0; 3]; len];
        for z in 0..height {
            for x in 0..width {
                let i = (z + 1) * stride + x + 1;
                for channel in 0..3 {
                    let sum = values[z * width + x][channel]
                        + prefix[i - 1][channel]
                        + prefix[i - stride][channel]
                        - prefix[i - stride - 1][channel];
                    if !sum.is_finite() {
                        return None;
                    }
                    prefix[i][channel] = sum;
                }
            }
        }
        Some(Self {
            width,
            height,
            stride,
            prefix,
        })
    }

    pub fn box_sum(&self, x0: usize, z0: usize, x1: usize, z1: usize) -> [f64; 3] {
        let Some((x0, z0, x1, z1)) = self.bounds(x0, z0, x1, z1) else {
            return [0.0; 3];
        };
        let a = self.prefix[(z1 + 1) * self.stride + x1 + 1];
        let b = self.prefix[z0 * self.stride + x1 + 1];
        let c = self.prefix[(z1 + 1) * self.stride + x0];
        let d = self.prefix[z0 * self.stride + x0];
        std::array::from_fn(|channel| a[channel] - b[channel] - c[channel] + d[channel])
    }

    pub fn box_mean(&self, x0: usize, z0: usize, x1: usize, z1: usize) -> [f64; 3] {
        let Some((x0, z0, x1, z1)) = self.bounds(x0, z0, x1, z1) else {
            return [0.0; 3];
        };
        let cells = ((x1 - x0 + 1) * (z1 - z0 + 1)) as f64;
        self.box_sum(x0, z0, x1, z1).map(|sum| sum / cells)
    }

    fn bounds(
        &self,
        x0: usize,
        z0: usize,
        x1: usize,
        z1: usize,
    ) -> Option<(usize, usize, usize, usize)> {
        if x0 > x1 || z0 > z1 || x0 >= self.width || z0 >= self.height {
            None
        } else {
            Some((x0, z0, x1.min(self.width - 1), z1.min(self.height - 1)))
        }
    }
}

/// Integral volume with x-fastest flat input order:
/// `values[(y * depth + z) * width + x]`.
///
/// Prefixes use i128 to avoid intermediate overflow. Construction bounds the
/// total positive and negative magnitudes separately, guaranteeing that every
/// possible subvolume sum can be returned exactly as i64.
#[derive(Clone, Debug)]
pub struct IntegralVolume3D {
    width: usize,
    height: usize,
    depth: usize,
    stride_x: usize,
    stride_z: usize,
    prefix: Vec<i128>,
}

impl IntegralVolume3D {
    pub fn new(width: usize, height: usize, depth: usize, values: &[i64]) -> Option<Self> {
        if width.checked_mul(height)?.checked_mul(depth)? != values.len() {
            return None;
        }
        let (mut positive_magnitude, mut negative_magnitude) = (0_u128, 0_u128);
        for &value in values {
            if value >= 0 {
                positive_magnitude += value as u128;
            } else {
                negative_magnitude += u128::from(value.unsigned_abs());
            }
            if positive_magnitude > i64::MAX as u128 || negative_magnitude > i64::MAX as u128 + 1 {
                return None;
            }
        }
        let stride_x = width.checked_add(1)?;
        let stride_z = depth.checked_add(1)?;
        let len = stride_x
            .checked_mul(stride_z)?
            .checked_mul(height.checked_add(1)?)?;
        let mut volume = Self {
            width,
            height,
            depth,
            stride_x,
            stride_z,
            prefix: vec![0; len],
        };
        for y in 1..=height {
            for z in 1..=depth {
                for x in 1..=width {
                    let i = volume.index(x, y, z);
                    let cell = i128::from(values[((y - 1) * depth + z - 1) * width + x - 1]);
                    volume.prefix[i] = cell
                        + volume.get(x - 1, y, z)
                        + volume.get(x, y - 1, z)
                        + volume.get(x, y, z - 1)
                        - volume.get(x - 1, y - 1, z)
                        - volume.get(x - 1, y, z - 1)
                        - volume.get(x, y - 1, z - 1)
                        + volume.get(x - 1, y - 1, z - 1);
                }
            }
        }
        Some(volume)
    }

    /// Exact i64 sum of an inclusive 3D box, intersected with the volume.
    pub fn box_sum_3d(
        &self,
        x0: usize,
        y0: usize,
        z0: usize,
        x1: usize,
        y1: usize,
        z1: usize,
    ) -> i64 {
        if x0 > x1
            || y0 > y1
            || z0 > z1
            || x0 >= self.width
            || y0 >= self.height
            || z0 >= self.depth
        {
            return 0;
        }
        let (x1, y1, z1) = (
            x1.min(self.width - 1) + 1,
            y1.min(self.height - 1) + 1,
            z1.min(self.depth - 1) + 1,
        );
        let sum = self.get(x1, y1, z1)
            - self.get(x0, y1, z1)
            - self.get(x1, y0, z1)
            - self.get(x1, y1, z0)
            + self.get(x0, y0, z1)
            + self.get(x0, y1, z0)
            + self.get(x1, y0, z0)
            - self.get(x0, y0, z0);
        // The constructor bounds the absolute sum of every possible box.
        i64::try_from(sum).expect("subvolume sum fits i64")
    }

    fn index(&self, x: usize, y: usize, z: usize) -> usize {
        (y * self.stride_z + z) * self.stride_x + x
    }

    fn get(&self, x: usize, y: usize, z: usize) -> i128 {
        self.prefix[self.index(x, y, z)]
    }
}

/// Evaluate the Chebyshev expansion `sum_k coeffs[k] * T_k(u)` using
/// Clenshaw recurrence, where `u = 2 * (x - a) / (b - a) - 1`.
/// Coefficients are ordered from degree zero upwards. `a == b` is invalid.
pub fn chebyshev_eval(x: f64, a: f64, b: f64, coeffs: &[f64]) -> f64 {
    if coeffs.is_empty() {
        return 0.0;
    }
    let u = 2.0 * ((x - a) / (b - a)) - 1.0;
    let mut next = 0.0;
    let mut next_next = 0.0;
    for &coefficient in coeffs[1..].iter().rev() {
        let current = 2.0 * u * next - next_next + coefficient;
        next_next = next;
        next = current;
    }
    coeffs[0] + u * next - next_next
}

// Degree-8 Remez minimax approximation of exp(-x) on [0, 1], transformed
// into Chebyshev coefficients. Maximum absolute error is about 1.284e-11.
const EXP_NEG_UNIT_COEFFS: [f64; 9] = [
    0.645_035_270_449_150_1,
    -0.312_841_606_369_743_4,
    0.038_704_115_419_326_56,
    -0.003_208_683_015_130_921_7,
    0.000_199_919_237_755_498_84,
    -0.000_009_975_211_042_940_17,
    0.000_000_415_016_896_680_143_1,
    -0.000_000_014_805_521_597_488_348,
    0.000_000_000_462_592_354_638_311_7,
];

/// Minimax `exp(-x)` for `x` in `[0, 1]`, with ~1e-11 absolute error.
pub fn fast_exp_neg_unit(x: f64) -> f64 {
    chebyshev_eval(x, 0.0, 1.0, &EXP_NEG_UNIT_COEFFS)
}

/// Explicit name for the unit-interval Chebyshev approximation.
pub fn fast_exp_neg_chebyshev(x: f64) -> f64 {
    fast_exp_neg_unit(x)
}

/// Approximate `exp(-x)` for nonnegative x by splitting off powers of two.
/// NaN, negative arguments, and positive infinity fail closed to zero.
pub fn fast_exp_neg(x: f64) -> f64 {
    if !x.is_finite() || x < 0.0 || x >= 745.0 {
        return 0.0;
    }
    let powers_of_two = (x / std::f64::consts::LN_2).floor() as u32;
    let remainder = (x - f64::from(powers_of_two) * std::f64::consts::LN_2).clamp(0.0, 1.0);
    let scale = if powers_of_two <= 1022 {
        f64::from_bits((1023 - u64::from(powers_of_two)) << 52)
    } else {
        f64::from_bits(1_u64 << (1074 - powers_of_two))
    };
    scale * fast_exp_neg_unit(remainder)
}

/// Cubic Hermite interpolation and its analytic derivative with respect to t.
/// Tangents m0 and m1 are derivatives in the same parameter t.
pub fn hermite_eval_and_deriv(p0: f64, m0: f64, p1: f64, m1: f64, t: f64) -> (f64, f64) {
    let a = 2.0 * p0 + m0 - 2.0 * p1 + m1;
    let b = -3.0 * p0 - 2.0 * m0 + 3.0 * p1 - m1;
    let value = ((a * t + b) * t + m0) * t + p0;
    let derivative = (3.0 * a * t + 2.0 * b) * t + m0;
    (value, derivative)
}

/// Second-order Velocity-Verlet step for acceleration depending on position.
/// The acceleration function is evaluated at the old and new positions.
/// For conservative position-dependent forces, this is symplectic.
pub fn symplectic_verlet_step(
    pos: [f64; 3],
    vel: [f64; 3],
    accel_fn: impl Fn([f64; 3]) -> [f64; 3],
    dt: f64,
) -> ([f64; 3], [f64; 3]) {
    let acceleration = accel_fn(pos);
    let next_pos = std::array::from_fn(|i| pos[i] + dt * vel[i] + 0.5 * dt * dt * acceleration[i]);
    let next_acceleration = accel_fn(next_pos);
    let next_vel =
        std::array::from_fn(|i| vel[i] + 0.5 * dt * (acceleration[i] + next_acceleration[i]));
    (next_pos, next_vel)
}

/// Analytic flow for `dy/dt = -rate * (y - target)` over a fixed time step.
/// Uses expm1 to retain precision for very small time steps.
pub fn exp_decay_exact(current: f64, target: f64, rate: f64, dt: f64) -> f64 {
    current + (target - current) * -(-rate * dt).exp_m1()
}

fn spread_10_bits(mut value: u32) -> u32 {
    value &= 0x3ff;
    value = (value | (value << 16)) & 0x0300_00ff;
    value = (value | (value << 8)) & 0x0300_f00f;
    value = (value | (value << 4)) & 0x030c_30c3;
    (value | (value << 2)) & 0x0924_9249
}

fn compact_10_bits(mut value: u32) -> u32 {
    value &= 0x0924_9249;
    value = (value | (value >> 2)) & 0x030c_30c3;
    value = (value | (value >> 4)) & 0x0300_f00f;
    value = (value | (value >> 8)) & 0x0300_00ff;
    (value | (value >> 16)) & 0x3ff
}

/// Interleave the low 10 bits of each coordinate into a 30-bit Morton code.
pub fn morton_encode_3d(x: u32, y: u32, z: u32) -> u32 {
    spread_10_bits(x) | (spread_10_bits(y) << 1) | (spread_10_bits(z) << 2)
}

/// Decode a 30-bit Morton code into three 10-bit coordinates.
pub fn morton_decode_3d(code: u32) -> (u32, u32, u32) {
    (
        compact_10_bits(code),
        compact_10_bits(code >> 1),
        compact_10_bits(code >> 2),
    )
}

/// Interleave all 16 bits of each coordinate into a 48-bit Morton code.
pub fn morton_encode_3d_u16(x: u16, y: u16, z: u16) -> u64 {
    let mut code = 0_u64;
    for bit in 0..16 {
        code |= u64::from((x >> bit) & 1) << (3 * bit);
        code |= u64::from((y >> bit) & 1) << (3 * bit + 1);
        code |= u64::from((z >> bit) & 1) << (3 * bit + 2);
    }
    code
}

/// Decode a 48-bit Morton code into 16-bit coordinates (high bits ignored).
pub fn morton_decode_3d_u16(code: u64) -> (u16, u16, u16) {
    let mut coordinates = [0_u16; 3];
    for bit in 0..16 {
        for (axis, coordinate) in coordinates.iter_mut().enumerate() {
            *coordinate |= (((code >> (3 * bit + axis)) & 1) as u16) << bit;
        }
    }
    (coordinates[0], coordinates[1], coordinates[2])
}

/// Morton index of a 16^3 section. Every aligned 4^3 cube occupies one
/// contiguous group of 64 entries. Out-of-section coordinates return None.
pub fn morton_section_index(x: usize, y: usize, z: usize) -> Option<usize> {
    if x >= 16 || y >= 16 || z >= 16 {
        None
    } else {
        Some(morton_encode_3d(x as u32, y as u32, z as u32) as usize)
    }
}
