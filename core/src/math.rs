// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! Vanilla `net.minecraft.util.Mth` primitives.
//!
//! These are the arithmetic contracts simulation code inherits from the game:
//! `Mth.sin`/`Mth.cos` are a 65 536-entry `f32` lookup table, not libm, so a
//! port that calls `f32::sin` is off by an ulp exactly where a cell boundary is
//! decided; `floor_div`/`floor_mod` are the Euclidean grid operations behind
//! chunk coordinates; and the `lerp` family and `wrap_degrees` keep the same
//! rounding the bytecode produces.

use std::sync::OnceLock;

/// Integer types [`Mth::floor_div`]/[`Mth::floor_mod`] apply to.
///
/// Implemented for `i32` and `i64`, the two widths vanilla's `Math.floorDiv`
/// overloads cover. The sign rule is the Euclidean one: the quotient rounds
/// toward negative infinity, so `floor_mod` is always in `[0, |b|)` for a
/// positive `b` regardless of the dividend's sign.
pub trait FloorInt: Copy {
    /// `Math.floorDiv(a, b)`.
    fn floor_div(self, b: Self) -> Self;
    /// `Math.floorMod(a, b)` — `a - floor_div(a, b) * b`.
    fn floor_mod(self, b: Self) -> Self;
}

macro_rules! impl_floor_int {
    ($($t:ty),+) => {$(
        impl FloorInt for $t {
            fn floor_div(self, b: $t) -> $t {
                let q = self / b;
                if (self ^ b) < 0 && self % b != 0 { q - 1 } else { q }
            }
            fn floor_mod(self, b: $t) -> $t {
                self - <Self as FloorInt>::floor_div(self, b) * b
            }
        }
    )+};
}
impl_floor_int!(i32, i64);

/// `SIN_STEPS` — `f32` steps per radian: `65536 / (2π)`, truncated by the cast
/// the way the table indexing does.
const SIN_STEPS: f32 = 10430.378;

fn sin_table() -> &'static [f32; 65536] {
    static T: OnceLock<Box<[f32; 65536]>> = OnceLock::new();
    T.get_or_init(|| {
        let mut t = Box::new([0.0f32; 65536]);
        for (i, v) in t.iter_mut().enumerate() {
            *v = (i as f64 * std::f64::consts::PI * 2.0 / 65536.0).sin() as f32;
        }
        t
    })
}

/// `net.minecraft.util.Mth` as a namespace of static functions — the same
/// shape the game's utility class has, so call sites read like the reference.
pub struct Mth;

impl Mth {
    /// `Mth.sin(float)`: indexes the table, wrapping the `f32` index through
    /// the same truncation the bytecode does — **not** libm.
    #[inline]
    pub fn sin(v: f32) -> f32 {
        sin_table()[(v * SIN_STEPS) as i32 as usize & 65535]
    }

    /// `Mth.cos(float)`: the same table read a quarter turn ahead, the
    /// `+ 16384.0` added in `f32` before truncation as in the bytecode.
    #[inline]
    pub fn cos(v: f32) -> f32 {
        sin_table()[(v * SIN_STEPS + 16384.0) as i32 as usize & 65535]
    }

    /// `Math.floorDiv`: quotient rounding toward negative infinity.
    #[inline]
    pub fn floor_div<T: FloorInt>(a: T, b: T) -> T {
        a.floor_div(b)
    }

    /// `Math.floorMod`: remainder with the divisor's sign, so a positive `b`
    /// always yields `0..b` — the chunk-coordinate rule.
    #[inline]
    pub fn floor_mod<T: FloorInt>(a: T, b: T) -> T {
        a.floor_mod(b)
    }

    /// `Mth.clamp`: `value < min ? min : (value > max ? max : value)` — the
    /// ternary form, so a `NaN` value passes through as vanilla's does.
    #[inline]
    pub fn clamp<T: PartialOrd + Copy>(value: T, min: T, max: T) -> T {
        if value < min {
            min
        } else if value > max {
            max
        } else {
            value
        }
    }

    /// `Mth.lerp(double delta, double start, double end)`.
    #[inline]
    pub fn lerp(delta: f64, start: f64, end: f64) -> f64 {
        start + delta * (end - start)
    }

    /// `Mth.lerp(float delta, float start, float end)` — the `f32` overload;
    /// the arithmetic stays in `f32` because it rounds differently.
    #[inline]
    pub fn lerp_f32(delta: f32, start: f32, end: f32) -> f32 {
        start + delta * (end - start)
    }

    /// `Mth.clampedLerp(double...)`: `lerp` with `delta` held inside `[0, 1]`.
    #[inline]
    pub fn clamped_lerp(delta: f64, start: f64, end: f64) -> f64 {
        if delta < 0.0 {
            start
        } else if delta > 1.0 {
            end
        } else {
            Self::lerp(delta, start, end)
        }
    }

    /// `Mth.clampedLerp(float...)`.
    #[inline]
    pub fn clamped_lerp_f32(delta: f32, start: f32, end: f32) -> f32 {
        if delta < 0.0 {
            start
        } else if delta > 1.0 {
            end
        } else {
            Self::lerp_f32(delta, start, end)
        }
    }

    /// `Mth.inverseLerp(double...)`: the unclamped inverse of [`lerp`](Self::lerp).
    #[inline]
    pub fn inverse_lerp(value: f64, start: f64, end: f64) -> f64 {
        (value - start) / (end - start)
    }

    /// The `f32` overload of [`inverse_lerp`](Self::inverse_lerp).
    #[inline]
    pub fn inverse_lerp_f32(value: f32, start: f32, end: f32) -> f32 {
        (value - start) / (end - start)
    }

    /// `Mth.lerp2(deltaX, deltaZ, x0y0, x1y0, x0y1, x1y1)` — bilinear over x
    /// first, then z, in vanilla's argument order.
    #[inline]
    pub fn lerp2(dx: f64, dz: f64, x0y0: f64, x1y0: f64, x0y1: f64, x1y1: f64) -> f64 {
        Self::lerp(
            dz,
            Self::lerp(dx, x0y0, x1y0),
            Self::lerp(dx, x0y1, x1y1),
        )
    }

    /// `Mth.lerp3(a, b, c, v0..v7)` — trilinear over the eight cell corners,
    /// `a` along x, `b` along y, `c` along z, in vanilla's exact staging.
    #[inline]
    #[expect(clippy::too_many_arguments)]
    pub fn lerp3(
        a: f64,
        b: f64,
        c: f64,
        v0: f64,
        v1: f64,
        v2: f64,
        v3: f64,
        v4: f64,
        v5: f64,
        v6: f64,
        v7: f64,
    ) -> f64 {
        let l0 = Self::lerp(a, v0, v1);
        let l1 = Self::lerp(a, v2, v3);
        let l2 = Self::lerp(a, v4, v5);
        let l3 = Self::lerp(a, v6, v7);
        let m0 = Self::lerp(b, l0, l1);
        let m1 = Self::lerp(b, l2, l3);
        Self::lerp(c, m0, m1)
    }

    /// `Mth.smoothstep` — Perlin's quintic fade `t³(6t² - 15t + 10)`, the
    /// interpolation curve the noise samplers apply per cell.
    #[inline]
    pub fn smoothstep(t: f64) -> f64 {
        t * t * t * (t * (t * 6.0 - 15.0) + 10.0)
    }

    /// `Mth.wrapDegrees(double)`: fold into `[-180, 180)` via `fmod`, matching
    /// the bytecode's two conditional fixes — so `-370.0` gives `-10.0`, not
    /// `350.0`, and `180.0` maps to `-180.0` (the upper fix is `>= 180`
    /// applied before the lower, which `180` already satisfies).
    #[inline]
    pub fn wrap_degrees(d: f64) -> f64 {
        let mut e = d % 360.0;
        if e >= 180.0 {
            e -= 360.0;
        }
        if e < -180.0 {
            e += 360.0;
        }
        e
    }

    /// `Mth.wrapDegrees(float)`.
    #[inline]
    pub fn wrap_degrees_f32(f: f32) -> f32 {
        let mut e = f % 360.0;
        if e >= 180.0 {
            e -= 360.0;
        }
        if e < -180.0 {
            e += 360.0;
        }
        e
    }

    /// `Mth.wrapDegrees(int)` — yaw in whole degrees, e.g. the `a[0-255]`
    /// entity-rotation wire encoding's inverse.
    #[inline]
    pub fn wrap_degrees_int(i: i32) -> i32 {
        let mut j = i % 360;
        if j >= 180 {
            j -= 360;
        }
        if j < -180 {
            j += 360;
        }
        j
    }

    /// `Mth.getSeed(int x, int y, int z)` — the position hash positional
    /// random factories key `at(x, y, z)` forks on.
    #[inline]
    pub fn get_seed(x: i32, y: i32, z: i32) -> i64 {
        let l = (x.wrapping_mul(3129871) as i64) ^ (z as i64).wrapping_mul(116129781) ^ (y as i64);
        let l = l
            .wrapping_mul(l)
            .wrapping_mul(42317861)
            .wrapping_add(l.wrapping_mul(11));
        l >> 16
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sin_and_cos_come_from_the_65536_entry_table() {
        // The table itself, built the way vanilla builds it.
        assert_eq!(Mth::sin(0.0), 0.0);
        assert_eq!(Mth::cos(0.0), 1.0);
        // Table granularity bounds the error to one step (~9.6e-5) plus the
        // f32 truncation of the index; 1.5e-4 leaves headroom without libm.
        let mut angle = -16.0f32;
        while angle < 16.0 {
            assert!(
                (Mth::sin(angle) - angle.sin()).abs() < 1.5e-4,
                "sin({angle})"
            );
            assert!(
                (Mth::cos(angle) - angle.cos()).abs() < 1.5e-4,
                "cos({angle})"
            );
            angle += 0.037;
        }
        // Deterministic to the bit: same input, same table cell.
        assert_eq!(Mth::sin(12.345).to_bits(), Mth::sin(12.345).to_bits());
    }

    #[test]
    fn sin_wraps_and_cos_is_sin_a_quarter_turn_ahead() {
        // Index arithmetic wraps mod 65536, so the function is 2π-periodic
        // up to the table's own cell rounding (one step ≈ 9.6e-5).
        assert!((Mth::sin(0.5) - Mth::sin(0.5 + std::f32::consts::TAU)).abs() < 2e-4);
        // A quarter turn (16384 of 65536 entries) relates cos to sin — in the
        // table's own granularity, exact to the cell.
        let v = 0.7234f32;
        let idx_sin = (v * SIN_STEPS) as i32 as usize & 65535;
        let idx_cos = (v * SIN_STEPS + 16384.0) as i32 as usize & 65535;
        assert_eq!(Mth::cos(v), sin_table()[idx_cos]);
        assert_eq!(Mth::sin(v), sin_table()[idx_sin]);
        // sin² + cos² = 1 up to the f32 table's own rounding.
        let sum = Mth::sin(2.5) * Mth::sin(2.5) + Mth::cos(2.5) * Mth::cos(2.5);
        assert!((sum - 1.0).abs() < 1e-6, "{sum}");
    }

    #[test]
    fn floor_div_and_floor_mod_are_euclidean_on_negative_coords() {
        // Chunk-coordinate arithmetic: block -1 lives in chunk -1, slot 15.
        assert_eq!(Mth::floor_div(-1i32, 16), -1);
        assert_eq!(Mth::floor_mod(-1i32, 16), 15);
        assert_eq!(Mth::floor_div(-16i32, 16), -1);
        assert_eq!(Mth::floor_mod(-16i32, 16), 0);
        assert_eq!(Mth::floor_div(-17i32, 16), -2);
        assert_eq!(Mth::floor_mod(-17i32, 16), 15);
        assert_eq!(Mth::floor_div(31i32, 16), 1);
        assert_eq!(Mth::floor_mod(31i32, 16), 15);
        // The identity a = floor_div*b + floor_mod holds on mixed signs.
        for a in [-33i64, -17, -1, 0, 1, 17, 33] {
            for b in [3i64, 16] {
                assert_eq!(Mth::floor_div(a, b) * b + Mth::floor_mod(a, b), a);
                assert!((0..b).contains(&Mth::floor_mod(a, b)));
            }
        }
    }

    #[test]
    fn clamp_uses_vanillas_ternary_semantics() {
        assert_eq!(Mth::clamp(5, 0, 3), 3);
        assert_eq!(Mth::clamp(-1, 0, 3), 0);
        assert_eq!(Mth::clamp(2, 0, 3), 2);
        assert_eq!(Mth::clamp(2.5f64, 0.0, 3.0), 2.5);
        assert_eq!(Mth::clamp(9.0f32, 0.0, 3.0), 3.0);
        // NaN fails both comparisons, so the value is returned — the same
        // leak vanilla's ternary has, pinned on purpose.
        assert!(Mth::clamp(f32::NAN, 0.0, 1.0).is_nan());
    }

    #[test]
    fn the_lerp_family_interpolates_in_vanillas_staging() {
        assert_eq!(Mth::lerp(0.25, 0.0, 8.0), 2.0);
        assert_eq!(Mth::lerp_f32(0.5, -4.0, 4.0), 0.0);
        // clampedLerp clamps the delta, not the value.
        assert_eq!(Mth::clamped_lerp(-0.5, 10.0, 20.0), 10.0);
        assert_eq!(Mth::clamped_lerp(1.5, 10.0, 20.0), 20.0);
        assert_eq!(Mth::clamped_lerp_f32(0.5, 10.0, 20.0), 15.0);
        // inverseLerp undoes lerp.
        assert_eq!(Mth::inverse_lerp(15.0, 10.0, 20.0), 0.5);
        assert_eq!(Mth::inverse_lerp_f32(12.5, 10.0, 20.0), 0.25);
        let d = Mth::inverse_lerp(7.5, 3.0, 9.0);
        assert_eq!(Mth::lerp(d, 3.0, 9.0), 7.5);
        // Bilinear: centre of the unit cell is the mean of the corners.
        assert_eq!(Mth::lerp2(0.5, 0.5, 0.0, 10.0, 20.0, 30.0), 15.0);
        assert_eq!(Mth::lerp2(0.0, 0.0, 1.0, 2.0, 3.0, 4.0), 1.0);
        assert_eq!(Mth::lerp2(1.0, 1.0, 1.0, 2.0, 3.0, 4.0), 4.0);
        // Trilinear: corners and the exact centre.
        assert_eq!(Mth::lerp3(0.0, 0.0, 0.0, 1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0), 1.0);
        assert_eq!(Mth::lerp3(1.0, 1.0, 1.0, 1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0), 8.0);
        assert_eq!(Mth::lerp3(0.5, 0.5, 0.5, 1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0), 4.5);
    }

    #[test]
    fn smoothstep_is_the_quintic_fade() {
        assert_eq!(Mth::smoothstep(0.0), 0.0);
        assert_eq!(Mth::smoothstep(1.0), 1.0);
        assert_eq!(Mth::smoothstep(0.5), 0.5);
        // Symmetric: s(t) = 1 - s(1 - t).
        for t in [0.125f64, 0.25, 0.4, 0.75] {
            assert!((Mth::smoothstep(t) - (1.0 - Mth::smoothstep(1.0 - t))).abs() < 1e-15);
        }
    }

    #[test]
    fn wrap_degrees_folds_into_minus_180_to_180() {
        assert_eq!(Mth::wrap_degrees(0.0), 0.0);
        assert_eq!(Mth::wrap_degrees(190.0), -170.0);
        assert_eq!(Mth::wrap_degrees(-190.0), 170.0);
        assert_eq!(Mth::wrap_degrees(360.0), 0.0);
        assert_eq!(Mth::wrap_degrees(-360.0), 0.0);
        assert_eq!(Mth::wrap_degrees(370.0), 10.0);
        assert_eq!(Mth::wrap_degrees(-370.0), -10.0);
        assert_eq!(Mth::wrap_degrees(180.0), -180.0);
        assert_eq!(Mth::wrap_degrees(-180.0), -180.0);
        assert_eq!(Mth::wrap_degrees_f32(190.0), -170.0);
        assert_eq!(Mth::wrap_degrees_f32(-190.0), 170.0);
        assert_eq!(Mth::wrap_degrees_int(190), -170);
        assert_eq!(Mth::wrap_degrees_int(-190), 170);
        assert_eq!(Mth::wrap_degrees_int(360), 0);
        assert_eq!(Mth::wrap_degrees_int(-370), -10);
    }

    #[test]
    fn get_seed_is_deterministic_and_position_dependent() {
        assert_eq!(Mth::get_seed(1, 2, 3), Mth::get_seed(1, 2, 3));
        assert_ne!(Mth::get_seed(0, 0, 0), Mth::get_seed(1, 0, 0));
        assert_ne!(Mth::get_seed(0, 0, 0), Mth::get_seed(0, 1, 0));
        assert_ne!(Mth::get_seed(0, 0, 0), Mth::get_seed(0, 0, 1));
        // Negative coordinates must not panic or stick to zero.
        let _ = Mth::get_seed(-30000000, -64, 30000000);
    }
}
