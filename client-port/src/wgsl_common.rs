// SPDX-License-Identifier: LGPL-3.0-or-later

//! Shared WGSL snippets for shaders that consume color in linear light.

/// Prepend the sRGB decode helpers to a static WGSL module.
#[macro_export]
macro_rules! wgsl_with_srgb_decode {
    ($source:literal) => {
        concat!(
            r#"
fn srgb_to_linear_rgb(rgb: vec3<f32>) -> vec3<f32> {
    let power_base = max((rgb + 0.055) / 1.055, vec3<f32>(0.0));
    return select(pow(power_base, vec3<f32>(2.4)), rgb / 12.92, rgb <= vec3<f32>(0.04045));
}

fn packed_srgb_rgb8_to_linear(tint: u32) -> vec3<f32> {
    let rgb = vec3<f32>(f32((tint >> 16u) & 255u), f32((tint >> 8u) & 255u), f32(tint & 255u));
    return srgb_to_linear_rgb(rgb / 255.0);
}
"#,
            $source
        )
    };
}

pub const SRGB_DECODE_WGSL: &str = wgsl_with_srgb_decode!("");
pub const SRGB_ENCODE_WGSL: &str = r#"
fn linear_to_srgb_rgb(rgb: vec3<f32>) -> vec3<f32> {
    let power_base = max(rgb, vec3<f32>(0.0));
    return select(1.055 * pow(power_base, vec3<f32>(1.0 / 2.4)) - 0.055,
                  rgb * 12.92, rgb <= vec3<f32>(0.0031308));
}
"#;
