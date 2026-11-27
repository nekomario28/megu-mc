// SPDX-License-Identifier: LGPL-3.0-or-later
//! Vanilla 1.21.1 light map: the `LightTexture` 16×16 RGBA table and the
//! `l / (4 - 3l)` brightness curve every lit surface samples.
//!
//! This module is the single authority for light-response math in the
//! renderer. Terrain vertex shading, the entity/block-entity fallback tint
//! and the uploaded light map all derive from the pure functions here, so a
//! change to the ramp cannot drift between them.
//!
//! * [`light_curve`] — the ramp `l / (4 - 3l)` for `l = level / 15`.
//! * [`get_brightness`] — that ramp blended toward `1.0` by the dimension's
//!   ambient light (Overworld `0.0`, Nether `0.1`, End `0.25`).
//! * [`gamma_curve`] — `c(x) = 1 - (1 - x)^4`, the shoulder the gamma
//!   setting lerps against.
//! * [`LightTexture`] — the 16×16 table: `x` is block light, `y` is sky
//!   light, and each texel carries Vanilla's per-channel sky ramp plus the
//!   End constant-ambient matrix.
//!
//! Everything is pure and deterministic; [`LightTextureParams`] is the whole
//! key, so an unchanged parameter set rebuilds nothing.

/// Edge length of the light map texture (16 block × 16 sky levels).
pub const LIGHT_TEXTURE_LEN: usize = 16;

/// `DimensionType.ambientLight()` — Overworld (no floor).
pub const OVERWORLD_AMBIENT_LIGHT: f32 = 0.0;
/// `DimensionType.ambientLight()` — Nether floor.
pub const NETHER_AMBIENT_LIGHT: f32 = 0.1;
/// `DimensionType.ambientLight()` — The End floor.
pub const END_AMBIENT_LIGHT: f32 = 0.25;

/// Per-channel tint a constant-ambient dimension (The End) lerps its whole
/// light map toward.
pub const END_CONSTANT_AMBIENT_TINT: [f32; 3] = [0.99, 1.12, 1.0];
/// Blend weight of [`END_CONSTANT_AMBIENT_TINT`] — the `0.25` in Vanilla's
/// `lerp(vec3(0.99, 1.12, 1.0), 0.25)`.
pub const END_CONSTANT_AMBIENT_BLEND: f32 = 0.25;

/// Grey every texel is pulled toward, and by how much (`0.75` at `0.04`):
/// keeps absolute darkness from collapsing to true black.
const LIGHTMAP_GREY: [f32; 3] = [0.75; 3];
const LIGHTMAP_GREY_BLEND: f32 = 0.04;

/// Vanilla's `getDarkenWorldAmount` tint: warm-dim the whole map.
const DARKEN_WORLD_TINT: [f32; 3] = [0.7, 0.6, 0.6];

/// `l / (4 - 3l)` on the normalized level `l`: `0.5` maps to `0.2`, not the
/// `0.25` the retired `l / (3 - 2l)` ramp produced.
pub fn light_curve(level: f32) -> f32 {
    let l = if level.is_finite() {
        level.clamp(0.0, 1.0)
    } else {
        0.0
    };
    l / (4.0 - 3.0 * l)
}

/// `LightTexture.getBrightness(ambientLight, lightLevel)`: the [`light_curve`]
/// ramp blended toward `1.0` by the dimension's ambient light, so a Nether or
/// End texel never falls below that floor.
pub fn get_brightness(ambient_light: f32, light_level: u8) -> f32 {
    let ambient = if ambient_light.is_finite() {
        ambient_light.clamp(0.0, 1.0)
    } else {
        0.0
    };
    let curve = light_curve(f32::from(light_level.min(15)) / 15.0);
    // Vanilla's `Mth.lerp(ambient, curve, 1.0)` — delta first — is
    // `curve + ambient * (1 - curve)`.
    curve + ambient * (1.0 - curve)
}

/// `c(x) = 1 - (1 - x)^4`: the gamma shoulder, steep at the dark end and
/// flat near white.
pub fn gamma_curve(value: f32) -> f32 {
    let x = if value.is_finite() {
        value.clamp(0.0, 1.0)
    } else {
        0.0
    };
    let inverse = 1.0 - x;
    1.0 - inverse * inverse * inverse * inverse
}

/// `ClientLevel.getSkyDarken`: `1.0` at noon, `0.2` at midnight, scaled down
/// by rain and thunder. Vanilla's name is historical — this is the sky
/// *brightness* every light map texel is scaled by.
pub fn vanilla_sky_darken(world_time_ticks: u64, rain_level: f32, thunder_level: f32) -> f32 {
    let time_of_day = (world_time_ticks.rem_euclid(24_000) as f32 / 24_000.0 - 0.25).rem_euclid(1.0);
    let mut brightness = 1.0 - ((time_of_day * std::f32::consts::TAU).cos() * 2.0 + 0.2);
    brightness = brightness.clamp(0.0, 1.0);
    brightness = 1.0 - brightness;
    let rain = level(rain_level);
    let thunder = level(thunder_level);
    brightness *= 1.0 - rain * 5.0 / 16.0;
    brightness *= 1.0 - thunder * 5.0 / 16.0;
    brightness * 0.8 + 0.2
}

/// `DimensionType.ambientLight()` for a renderer dimension kind
/// (`0` Overworld, `1` Nether, `2` End) — unknown ids read as Overworld.
pub fn dimension_ambient_light(dimension_kind: u8) -> f32 {
    match dimension_kind {
        1 => NETHER_AMBIENT_LIGHT,
        2 => END_AMBIENT_LIGHT,
        _ => OVERWORLD_AMBIENT_LIGHT,
    }
}

/// `DimensionType.hasConstantAmbientLight()` — The End only.
pub fn dimension_has_constant_ambient_light(dimension_kind: u8) -> bool {
    dimension_kind == 2
}

fn level(value: f32) -> f32 {
    if value.is_finite() {
        value.clamp(0.0, 1.0)
    } else {
        0.0
    }
}

fn lerp(from: f32, to: f32, t: f32) -> f32 {
    from + (to - from) * t
}

fn lerp3(from: [f32; 3], to: [f32; 3], t: f32) -> [f32; 3] {
    [
        lerp(from[0], to[0], t),
        lerp(from[1], to[1], t),
        lerp(from[2], to[2], t),
    ]
}

fn clamp3(rgb: [f32; 3]) -> [f32; 3] {
    [
        level(rgb[0]),
        level(rgb[1]),
        level(rgb[2]),
    ]
}

/// Everything one light map build depends on. Two equal parameter sets
/// produce byte-identical tables, so the renderer can rebuild only when this
/// key changes.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LightTextureParams {
    /// `ClientLevel.getSkyDarken` — see [`vanilla_sky_darken`] (`1.0` noon,
    /// `0.2` midnight).
    pub sky_darken: f32,
    /// `ClientLevel.getSkyFlashTime() > 0` — a lightning flash forces full
    /// sky brightness.
    pub sky_flash: bool,
    /// `LightTexture.blockLightRedFlicker` — the torch-flicker accumulator,
    /// normally `-0.1..=0.1` around zero.
    pub block_flicker: f32,
    /// The gamma setting in Vanilla's `0.0..=1.0` range (`0.5` default):
    /// weights the [`gamma_curve`] shoulder.
    pub gamma: f32,
    /// Night Vision scale (`0.0..=1.0`, conduit power folds in here too).
    pub night_vision_factor: f32,
    /// Darkness effect blend (`0.0..=1.0`) before the gamma multiply.
    pub darkness_factor: f32,
    /// `GameRenderer.getDarkenWorldAmount` (`0.0..=1.0`): the boss-fight
    /// world darkening.
    pub darken_world: f32,
    /// `DimensionType.ambientLight()` — see [`dimension_ambient_light`].
    pub ambient_light: f32,
    /// `DimensionType.hasConstantAmbientLight()` — see
    /// [`dimension_has_constant_ambient_light`].
    pub constant_ambient_light_end: bool,
}

impl Default for LightTextureParams {
    /// Clear Overworld noon with the default gamma: the table the renderer
    /// seeds before the world clock and dimension are known.
    fn default() -> Self {
        Self {
            sky_darken: 1.0,
            sky_flash: false,
            block_flicker: 0.0,
            gamma: 0.5,
            night_vision_factor: 0.0,
            darkness_factor: 0.0,
            darken_world: 0.0,
            ambient_light: OVERWORLD_AMBIENT_LIGHT,
            constant_ambient_light_end: false,
        }
    }
}

impl LightTextureParams {
    /// `Options.gamma` as the renderer stores it: a `50..=200` percentage
    /// where `100` is Vanilla's `0.5` default.
    pub fn gamma_from_percent(gamma_percent: u16) -> f32 {
        (f32::from(gamma_percent) / 200.0).clamp(0.0, 1.0)
    }

    /// Sky brightness factor `sky_darken * 0.95 + 0.05`, or `1.0` during a
    /// lightning flash.
    pub fn sky_brightness(&self) -> f32 {
        if self.sky_flash {
            1.0
        } else {
            level(self.sky_darken) * 0.95 + 0.05
        }
    }

    /// `darknessFactor * gamma` — the amount the Darkness effect subtracts
    /// from sky/ambient light.
    pub fn darkness_amount(&self) -> f32 {
        level(self.darkness_factor) * level(self.gamma)
    }

    /// The block-light flicker folded into the sky ramp (`flicker + 1.5`).
    pub fn flicker_scale(&self) -> f32 {
        let flicker = if self.block_flicker.is_finite() {
            self.block_flicker
        } else {
            0.0
        };
        (flicker + 1.5).max(0.0)
    }

    /// Amplitude of the Darkness shade subtracted from every channel
    /// (`0.45 * darkness * gamma`); the per-tick cosine that gate rides is
    /// not part of the table key.
    pub fn night_vision_shade(&self) -> f32 {
        (0.45 * self.darkness_amount() * level(self.gamma)).max(0.0)
    }
}

/// One light map texel as float RGB, before quantization. `block_light` and
/// `sky_light` are `0..=15` nibbles and clamp; this is the only place the
/// per-texel math lives.
pub fn lightmap_rgb(params: &LightTextureParams, block_light: u8, sky_light: u8) -> [f32; 3] {
    let sky_brightness = params.sky_brightness();
    let block = get_brightness(params.ambient_light, block_light) * sky_brightness;
    let sky = get_brightness(params.ambient_light, sky_light) * params.flicker_scale();
    // Vanilla's per-channel sky ramp: red is the raw level, green and blue
    // gain progressively more of it, which is why a dim sky trends blue.
    let red = sky;
    let green = sky * ((sky * 0.6 + 0.4) * 0.6 + 0.4);
    let blue = sky * (sky * sky * 0.6 + 0.4);
    let mut color = [red, green, blue];
    let sky_tint = lerp3([sky_brightness, sky_brightness, 1.0], [1.0; 3], 0.35);
    if params.constant_ambient_light_end {
        // The End replaces block light with a flat constant ambient tint.
        color = clamp3(lerp3(
            color,
            END_CONSTANT_AMBIENT_TINT,
            END_CONSTANT_AMBIENT_BLEND,
        ));
    } else {
        color = [
            color[0] + sky_tint[0] * block,
            color[1] + sky_tint[1] * block,
            color[2] + sky_tint[2] * block,
        ];
        color = lerp3(color, LIGHTMAP_GREY, LIGHTMAP_GREY_BLEND);
        let darken_world = level(params.darken_world);
        if darken_world > 0.0 {
            let dimmed = [
                color[0] * DARKEN_WORLD_TINT[0],
                color[1] * DARKEN_WORLD_TINT[1],
                color[2] * DARKEN_WORLD_TINT[2],
            ];
            color = lerp3(color, dimmed, darken_world);
        }
    }
    let night_vision = level(params.night_vision_factor);
    if night_vision > 0.0 {
        let brightest = color[0].max(color[1]).max(color[2]);
        if brightest < 1.0 {
            let scale = 1.0 / brightest.max(f32::MIN_POSITIVE);
            let normalized = [color[0] * scale, color[1] * scale, color[2] * scale];
            color = lerp3(color, normalized, night_vision);
        }
    }
    if !params.constant_ambient_light_end {
        let shade = params.night_vision_shade();
        if shade > 0.0 {
            color = [color[0] - shade, color[1] - shade, color[2] - shade];
        }
        color = clamp3(color);
    }
    let gamma = level(params.gamma);
    let weight = (gamma - params.darkness_amount()).max(0.0);
    let curved = [
        gamma_curve(color[0]),
        gamma_curve(color[1]),
        gamma_curve(color[2]),
    ];
    color = lerp3(color, curved, weight);
    clamp3(lerp3(color, LIGHTMAP_GREY, LIGHTMAP_GREY_BLEND))
}

/// One light map texel as the RGBA8 the GPU samples. Vanilla packs the
/// channels swapped — texture red carries the blue ramp — so the table is
/// written in that order and read back through [`LightTexture::brightness`].
pub fn lightmap_rgba(params: &LightTextureParams, block_light: u8, sky_light: u8) -> [u8; 4] {
    let rgb = lightmap_rgb(params, block_light, sky_light);
    [
        (rgb[2] * 255.0) as u8,
        (rgb[1] * 255.0) as u8,
        (rgb[0] * 255.0) as u8,
        255,
    ]
}

/// The 16×16 light map: `texels[block * 16 + sky]`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LightTexture {
    params: LightTextureParams,
    texels: [[u8; 4]; LIGHT_TEXTURE_LEN * LIGHT_TEXTURE_LEN],
}

impl Default for LightTexture {
    fn default() -> Self {
        Self::new(LightTextureParams::default())
    }
}

impl LightTexture {
    /// Build the full table for `params`.
    pub fn new(params: LightTextureParams) -> Self {
        let mut texture = Self {
            params,
            texels: [[0; 4]; LIGHT_TEXTURE_LEN * LIGHT_TEXTURE_LEN],
        };
        texture.write_all();
        texture
    }

    /// Rebuild in place when `params` changed. Returns whether any texel was
    /// rewritten, so a steady frame does no work.
    pub fn update(&mut self, params: LightTextureParams) -> bool {
        if self.params == params {
            return false;
        }
        self.params = params;
        self.write_all();
        true
    }

    /// The parameters this table was built from.
    pub fn params(&self) -> LightTextureParams {
        self.params
    }

    fn write_all(&mut self) {
        for block in 0..LIGHT_TEXTURE_LEN {
            for sky in 0..LIGHT_TEXTURE_LEN {
                self.texels[block * LIGHT_TEXTURE_LEN + sky] =
                    lightmap_rgba(&self.params, block as u8, sky as u8);
            }
        }
    }

    /// The RGBA8 texel at `(block_light, sky_light)`; both clamp to `0..=15`.
    pub fn texel(&self, block_light: u8, sky_light: u8) -> [u8; 4] {
        self.texels[usize::from(block_light.min(15)) * LIGHT_TEXTURE_LEN
            + usize::from(sky_light.min(15))]
    }

    /// The texel's strongest channel as `0.0..=1.0` — what a lit surface
    /// samples the light map for when it has one scalar to spend.
    pub fn brightness(&self, block_light: u8, sky_light: u8) -> f32 {
        let texel = self.texel(block_light, sky_light);
        f32::from(texel[0].max(texel[1]).max(texel[2])) / 255.0
    }

}

#[cfg(test)]
mod tests {
    use super::*;

    fn approx(actual: f32, expected: f32) {
        assert!(
            (actual - expected).abs() < 1.0e-5,
            "expected {expected}, got {actual}"
        );
    }

    #[test]
    fn light_curve_uses_the_four_minus_three_ramp() {
        approx(light_curve(0.0), 0.0);
        // 0.5 / (4 - 1.5) = 0.2 — the retired ramp gave 0.25 here.
        approx(light_curve(0.5), 0.2);
        approx(light_curve(1.0), 1.0);
        approx(light_curve(f32::NAN), 0.0);
        approx(light_curve(4.0), 1.0);
    }

    #[test]
    fn brightness_floors_at_the_dimension_ambient_light() {
        approx(get_brightness(OVERWORLD_AMBIENT_LIGHT, 0), 0.0);
        approx(get_brightness(NETHER_AMBIENT_LIGHT, 0), 0.1);
        approx(get_brightness(END_AMBIENT_LIGHT, 0), 0.25);
        for ambient in [
            OVERWORLD_AMBIENT_LIGHT,
            NETHER_AMBIENT_LIGHT,
            END_AMBIENT_LIGHT,
        ] {
            approx(get_brightness(ambient, 15), 1.0);
            let mut previous = -1.0;
            for light in 0..=15u8 {
                let brightness = get_brightness(ambient, light);
                assert!(brightness >= ambient - 1.0e-6);
                assert!(brightness >= previous);
                previous = brightness;
            }
        }
    }

    #[test]
    fn gamma_curve_is_one_minus_the_fourth_power() {
        approx(gamma_curve(0.0), 0.0);
        approx(gamma_curve(1.0), 1.0);
        approx(gamma_curve(0.5), 0.9375);
        approx(gamma_curve(f32::NAN), 0.0);
    }

    #[test]
    fn sky_darken_matches_the_vanilla_day_shape() {
        approx(vanilla_sky_darken(6_000, 0.0, 0.0), 1.0);
        approx(vanilla_sky_darken(18_000, 0.0, 0.0), 0.2);
        // Rain and thunder dim the sky ramp without reaching zero.
        let rainy = vanilla_sky_darken(6_000, 1.0, 1.0);
        assert!(rainy < 1.0 && rainy > 0.2);
        // Every tick of the day stays finite and inside the vanilla range.
        for tick in 0..24_000u64 {
            let value = vanilla_sky_darken(tick, 0.0, 0.0);
            assert!((0.2..=1.0).contains(&value), "tick {tick} -> {value}");
        }
    }

    #[test]
    fn table_is_16x16_and_matches_the_single_texel_lookup() {
        let params = LightTextureParams::default();
        let texture = LightTexture::new(params);
        for block in [0u8, 4, 15] {
            for sky in [0u8, 8, 15] {
                let texel = lightmap_rgba(&params, block, sky);
                assert_eq!(texture.texel(block, sky), texel, "texel ({block}, {sky})");
                assert_eq!(
                    texture.brightness(block, sky),
                    f32::from(texel[0].max(texel[1]).max(texel[2])) / 255.0
                );
            }
        }
        // Out-of-range nibbles clamp to the top corner.
        assert_eq!(texture.texel(200, 200), texture.texel(15, 15));
        // Full block light in clear noon is essentially white.
        assert!(texture.brightness(15, 0) > 0.95);
        // Absolute darkness stays measurable rather than black.
        assert!(texture.brightness(0, 0) > 0.0);
    }

    #[test]
    fn rebuild_is_keyed_on_the_whole_parameter_set() {
        let mut texture = LightTexture::default();
        assert!(!texture.update(LightTextureParams::default()));
        let night = LightTextureParams {
            sky_darken: 0.2,
            ..LightTextureParams::default()
        };
        assert!(texture.update(night));
        assert_eq!(texture.params(), night);
        assert!(!texture.update(night));
        // `sky_darken` scales block light, so a torch-lit texel dims at night.
        assert!(texture.brightness(15, 0) < LightTexture::default().brightness(15, 0));
    }

    #[test]
    fn nether_and_end_floors_reach_the_table() {
        let nether = LightTextureParams {
            ambient_light: NETHER_AMBIENT_LIGHT,
            ..LightTextureParams::default()
        };
        let nether_texture = LightTexture::new(nether);
        let end = LightTextureParams {
            ambient_light: END_AMBIENT_LIGHT,
            constant_ambient_light_end: true,
            ..LightTextureParams::default()
        };
        let end_texture = LightTexture::new(end);
        assert!(nether_texture.brightness(0, 0) > 0.0);
        assert!(end_texture.brightness(0, 0) > nether_texture.brightness(0, 0));
        // The End matrix never leaves the unit cube.
        for block in 0..16u8 {
            for sky in 0..16u8 {
                let rgb = lightmap_rgb(&end, block, sky);
                assert!(rgb.iter().all(|channel| (0.0..=1.0).contains(channel)));
            }
        }
        // The matrix is `lerp(vec3(0.99, 1.12, 1.0), 0.25)` toward white: a
        // `0.75` measure of the tint plus a `0.25` measure of white.
        for channel in 0..3 {
            let blended = lerp(
                END_CONSTANT_AMBIENT_TINT[channel],
                1.0,
                END_CONSTANT_AMBIENT_BLEND,
            );
            approx(blended, [0.9925, 1.09, 1.0][channel]);
        }
    }

    #[test]
    fn night_vision_brightens_while_darkness_dims() {
        let base = LightTextureParams::default();
        let mut night_vision = base;
        night_vision.night_vision_factor = 1.0;
        let mut darkness = base;
        darkness.darkness_factor = 1.0;
        darkness.gamma = 1.0;
        let plain = LightTexture::new(base);
        let lit = LightTexture::new(night_vision);
        let dim = LightTexture::new(darkness);
        for block in [0u8, 8, 15] {
            for sky in [0u8, 8, 15] {
                let plain = plain.brightness(block, sky);
                let lit = lit.brightness(block, sky);
                let dim = dim.brightness(block, sky);
                assert!(lit >= plain - 1.0e-6, "night vision dimmed ({block}, {sky})");
                assert!(dim <= plain + 1.0e-6, "darkness brightened ({block}, {sky})");
            }
        }
    }

    #[test]
    fn gamma_percent_maps_onto_the_vanilla_slider() {
        approx(LightTextureParams::gamma_from_percent(100), 0.5);
        approx(LightTextureParams::gamma_from_percent(0), 0.0);
        approx(LightTextureParams::gamma_from_percent(200), 1.0);
        approx(LightTextureParams::gamma_from_percent(40), 0.2);
    }

    #[test]
    fn dimension_helpers_match_the_three_vanilla_palettes() {
        approx(dimension_ambient_light(0), OVERWORLD_AMBIENT_LIGHT);
        approx(dimension_ambient_light(1), NETHER_AMBIENT_LIGHT);
        approx(dimension_ambient_light(2), END_AMBIENT_LIGHT);
        approx(dimension_ambient_light(9), OVERWORLD_AMBIENT_LIGHT);
        assert!(!dimension_has_constant_ambient_light(0));
        assert!(!dimension_has_constant_ambient_light(1));
        assert!(dimension_has_constant_ambient_light(2));
    }
}
