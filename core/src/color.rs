// SPDX-License-Identifier: LGPL-3.0-or-later
#![forbid(unsafe_code)]
//! `megu_core::color` — the canonical HSV hexcone, sRGB transfer function and
//! packed-colour conversions.
//!
//! Every client, tool and shader-facing host in this tree needs the same four
//! answers: "what RGB is this hue", "what linear-light value is this sRGB
//! byte", "what sRGB byte encodes this linear value", and "what `ARGB32`
//! integer is this quad". They live here once so a palette, a texture decode
//! and a packet tint cannot disagree by a channel or a gamma segment.
//!
//! # What "canonical" pins down
//!
//! * [`hsv_to_rgb`] is the HSV **hexcone** model, not the HSL bicone: `h` is a
//!   fraction of a turn in `0.0..=1.0`, `s` and `v` are `0.0..=1.0`, and the
//!   result is `v` scaled by the sector's edge. Sector selection uses
//!   `rem_euclid(6)`, so `h = 1.0` maps to the same colour as `h = 0.0`
//!   instead of falling off the end of the match.
//! * [`hsv_to_rgb_f64`] is the same formula at `f64` width, for the palette
//!   paths that already hold `f64` state-derived hue fractions. The two
//!   widths are *not* interchangeable: a caller that switches width changes
//!   the last ulp of the sector fraction, and a palette is compared
//!   byte-for-byte against a pinned GPU frame in this tree.
//! * [`srgb_to_linear`]/[`linear_to_srgb`] are the IEC 61966-2-1 curves
//!   vanilla's `ARGB` path and every mip/decode path in the clients use, with
//!   the piecewise linear segment below `0.04045`/`0.0031308` — a plain
//!   `powf(2.2)` loses the dark end, which is exactly where a night-lit
//!   texture differs from the jar's.
//! * [`pack_argb`]/[`unpack_argb`] are vanilla `FastColor.ARGB32` order:
//!   alpha in the high byte, then red, green, blue. `0xFF000000 | rgb` — the
//!   spelling `DyeColor.textureDiffuseColor` uses — is
//!   `pack_argb([0xFF, r, g, b])`.
//!
//! Pure `std`, zero dependencies, `unsafe`-free.

/// HSV hexcone to RGB, all channels in `0.0..=1.0`.
///
/// `h` is a hue fraction of a full turn (wrapped with `rem_euclid`, so `1.0`
/// equals `0.0`), `s` the saturation and `v` the value. Out-of-range values
/// are *not* clamped: callers that can produce them (a hue accumulated past
/// one turn, a synthesized saturation) choose the wrap or the clamp
/// themselves, because clamping and wrapping a hue give different colours and
/// silently picking one would change a pinned palette.
pub fn hsv_to_rgb(h: f32, s: f32, v: f32) -> [f32; 3] {
    let sector = (h * 6.0).floor();
    let fraction = h * 6.0 - sector;
    let sector = sector as i32;
    let p = v * (1.0 - s);
    let q = v * (1.0 - fraction * s);
    let t = v * (1.0 - (1.0 - fraction) * s);
    match sector.rem_euclid(6) {
        0 => [v, t, p],
        1 => [q, v, p],
        2 => [p, v, t],
        3 => [p, q, v],
        4 => [t, p, v],
        _ => [v, p, q],
    }
}

/// [`hsv_to_rgb`] at `f64` width, returning the same `f32` RGB triple.
///
/// Width-specialized on purpose: it is the same three-line formula, not a
/// second implementation of a different colour model. It exists so a palette
/// whose hue comes from `f64` golden-ratio math keeps its `f64` sector and
/// fraction arithmetic bit-identical when it is routed here, and it returns
/// `f32` because that is what every vertex and uniform buffer stores.
pub fn hsv_to_rgb_f64(h: f64, s: f64, v: f64) -> [f32; 3] {
    let sector = (h * 6.0).floor();
    let fraction = h * 6.0 - sector;
    let sector = sector as i32;
    let p = v * (1.0 - s);
    let q = v * (1.0 - fraction * s);
    let t = v * (1.0 - (1.0 - fraction) * s);
    let [r, g, b] = match sector.rem_euclid(6) {
        0 => [v, t, p],
        1 => [q, v, p],
        2 => [p, v, t],
        3 => [p, q, v],
        4 => [t, p, v],
        _ => [v, p, q],
    };
    [r as f32, g as f32, b as f32]
}

/// sRGB-encoded channel in `0.0..=1.0` to linear light — the IEC 61966-2-1
/// EOTF, including the linear segment below `0.04045`.
///
/// Inputs are not clamped; a caller decoding a byte does
/// `channel / 255.0`, which cannot leave the range.
pub fn srgb_to_linear(channel: f32) -> f32 {
    if channel <= 0.040_45 {
        channel / 12.92
    } else {
        ((channel + 0.055) / 1.055).powf(2.4)
    }
}

/// Linear-light channel in `0.0..=1.0` to its sRGB encoding — the inverse
/// of [`srgb_to_linear`], including the linear segment below `0.003_130_8`.
///
/// Inputs are not clamped; a caller quantizing to a byte rounds and clamps
/// afterwards (`* 255.0` then `round().clamp(0.0, 255.0)`), which is the
/// order that keeps a half-value byte from landing one step low.
pub fn linear_to_srgb(channel: f32) -> f32 {
    if channel <= 0.003_130_8 {
        channel * 12.92
    } else {
        1.055 * channel.powf(1.0 / 2.4) - 0.055
    }
}

/// Pack `[alpha, red, green, blue]` bytes into vanilla `FastColor.ARGB32`:
/// `a << 24 | r << 16 | g << 8 | b`.
pub const fn pack_argb(channels: [u8; 4]) -> u32 {
    (channels[0] as u32) << 24
        | (channels[1] as u32) << 16
        | (channels[2] as u32) << 8
        | (channels[3] as u32)
}

/// Split a vanilla `FastColor.ARGB32` into `[alpha, red, green, blue]` — the
/// exact inverse of [`pack_argb`].
pub const fn unpack_argb(argb: u32) -> [u8; 4] {
    [
        (argb >> 24) as u8,
        (argb >> 16) as u8,
        (argb >> 8) as u8,
        argb as u8,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_rgb(actual: [f32; 3], expected: [f32; 3]) {
        for (a, e) in actual.iter().zip(expected.iter()) {
            assert!((a - e).abs() < 1e-5, "{actual:?} != {expected:?}");
        }
    }

    #[test]
    fn hue_primaries_are_the_hexcone_corners() {
        assert_rgb(hsv_to_rgb(0.0, 1.0, 1.0), [1.0, 0.0, 0.0]);
        assert_rgb(hsv_to_rgb(1.0 / 3.0, 1.0, 1.0), [0.0, 1.0, 0.0]);
        assert_rgb(hsv_to_rgb(2.0 / 3.0, 1.0, 1.0), [0.0, 0.0, 1.0]);
        // The secondaries sit at the sector boundaries.
        assert_rgb(hsv_to_rgb(1.0 / 6.0, 1.0, 1.0), [1.0, 1.0, 0.0]);
        assert_rgb(hsv_to_rgb(0.5, 1.0, 1.0), [0.0, 1.0, 1.0]);
        assert_rgb(hsv_to_rgb(5.0 / 6.0, 1.0, 1.0), [1.0, 0.0, 1.0]);
    }

    #[test]
    fn zero_saturation_is_grey_at_the_value() {
        assert_rgb(hsv_to_rgb(0.0, 0.0, 0.6), [0.6, 0.6, 0.6]);
        assert_rgb(hsv_to_rgb(0.37, 0.0, 0.25), [0.25, 0.25, 0.25]);
        assert_rgb(hsv_to_rgb(0.99, 1.0, 0.0), [0.0, 0.0, 0.0]);
    }

    #[test]
    fn a_full_turn_wraps_to_the_start() {
        assert_rgb(hsv_to_rgb(1.0, 0.5, 0.85), hsv_to_rgb(0.0, 0.5, 0.85));
        assert_rgb(hsv_to_rgb(2.0, 0.25, 0.4), hsv_to_rgb(0.0, 0.25, 0.4));
    }

    #[test]
    fn the_block_palette_hue_is_the_golden_ratio_conjugate() {
        // `palette_color(state_id)` in the clients: hue = fract(id * 0.618...)
        // at s = 0.5, v = 0.85. Pinned here so a palette change is visible.
        // Sector 3 (fraction 0.7082039...) => [p, q, v].
        let hue = 0.618_033_988_749_895_f64;
        assert_rgb(hsv_to_rgb_f64(hue, 0.5, 0.85), [0.425, 0.549_013, 0.85]);
        let narrow = hsv_to_rgb(hue as f32, 0.5, 0.85);
        assert_rgb(narrow, hsv_to_rgb_f64(hue, 0.5, 0.85));

        // A later state id keeps the hue inside one turn after `fract`.
        let fraction = f64::from(7_u32) * 0.618_033_988_749_895;
        let state_hue = fraction.fract();
        assert!((0.0..1.0).contains(&state_hue));
        assert_rgb(hsv_to_rgb_f64(state_hue, 0.5, 0.85), {
            hsv_to_rgb(state_hue as f32, 0.5, 0.85)
        });
    }

    #[test]
    fn both_widths_agree_across_the_hue_circle() {
        // Same formula, different arithmetic width: they may differ in the
        // last ulp, never in the sector they pick.
        for step in 0..64 {
            let h = f64::from(step) / 64.0;
            let wide = hsv_to_rgb_f64(h, 0.6, 0.9);
            let narrow = hsv_to_rgb(h as f32, 0.6, 0.9);
            for (a, b) in wide.iter().zip(narrow.iter()) {
                assert!(
                    (a - b).abs() < 1e-5,
                    "hue {h} drifted: {wide:?} vs {narrow:?}"
                );
            }
        }
    }

    #[test]
    fn srgb_transfer_function_round_trips_every_byte() {
        for byte in 0..=255u32 {
            let encoded = byte as f32 / 255.0;
            let decoded = srgb_to_linear(encoded);
            assert!((0.0..=1.0).contains(&decoded), "byte {byte} left 0..=1");
            let back = linear_to_srgb(decoded);
            assert!(
                (back - encoded).abs() < 1e-6,
                "byte {byte} round-tripped to {back}"
            );
        }
    }

    #[test]
    fn srgb_decode_uses_the_linear_segment_below_the_knee() {
        // Linear segment: c / 12.92 exactly, not powf(2.2)-ish.
        assert!((srgb_to_linear(0.04) - 0.04 / 12.92).abs() < 1e-9);
        assert_eq!(srgb_to_linear(0.0), 0.0);
        // Power segment: 1.0 is a fixed point of the curve.
        assert_eq!(srgb_to_linear(1.0), 1.0);
        assert_eq!(linear_to_srgb(0.0), 0.0);
        assert!((linear_to_srgb(1.0) - 1.0).abs() < 1e-6);
        // The encode knee is the linear one, not the decode knee.
        assert!((linear_to_srgb(0.003) - 0.003 * 12.92).abs() < 1e-9);
    }

    #[test]
    fn argb_packing_matches_fastcolor_order() {
        assert_eq!(pack_argb([0xFF, 0x12, 0x34, 0x56]), 0xFF12_3456);
        assert_eq!(pack_argb([0x00, 0x00, 0x00, 0x00]), 0);
        // `FastColor.ARGB32.opaque(rgb)` — the `DyeColor` spelling.
        assert_eq!(pack_argb([0xFF, 0x00, 0xF9, 0xFF]), 0xFF00_F9FF);
        assert_eq!(unpack_argb(0xFF12_3456), [0xFF, 0x12, 0x34, 0x56]);
        for argb in [0, 0x00FF_FFFF, 0xFF00_0000, 0x80AB_CDEF, u32::MAX] {
            assert_eq!(pack_argb(unpack_argb(argb)), argb);
        }
    }
}
