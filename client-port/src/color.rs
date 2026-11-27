// SPDX-License-Identifier: LGPL-3.0-or-later

/// RGB bytes authored in sRGB (PNG, biome and registry colors).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SrgbRgb8(pub [u8; 3]);

/// RGB in linear light, suitable for multiplying by light or sampled albedo.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct LinearRgb(pub [f32; 3]);

impl SrgbRgb8 {
    pub const fn from_packed_rgb(rgb: u32) -> Self {
        Self([(rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8])
    }

    pub fn to_linear(self) -> LinearRgb {
        LinearRgb(
            self.0
                .map(|channel| srgb_to_linear(f32::from(channel) / 255.0)),
        )
    }
}

pub fn srgb_to_linear(channel: f32) -> f32 {
    if channel <= 0.04045 {
        channel / 12.92
    } else {
        ((channel + 0.055) / 1.055).powf(2.4)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packed_srgb_decodes_both_transfer_segments_in_rgb_order() {
        let LinearRgb(rgb) = SrgbRgb8::from_packed_rgb(0xff0a80ff).to_linear();
        assert!((rgb[0] - 0.00303527).abs() < 1e-7);
        assert!((rgb[1] - 0.2158605).abs() < 1e-7);
        assert_eq!(rgb[2], 1.0);
        assert_eq!(srgb_to_linear(0.0), 0.0);
    }
}
