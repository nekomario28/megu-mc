// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! Smooth biome/dimension atmosphere blending and chunk fade-in polish,
//! plus the geometry bridges that turn client-port's layered voxel clouds
//! and 3D cubic planets/suns into `TerrainVertex` buffers.
//!
//! * [`BiomeAtmosphereBlender`] — exponential `dt_sec`-driven lerp of sky
//!   zenith/horizon, fog color/density, and underwater/cave depth tint so
//!   crossing a biome border or ascending into orbit never hard-cuts.
//! * [`blend_dimension_crossfade`] — cubic-smoothstep crossfade between
//!   two resolved atmospheres for Phase-1 seamless portal transfers, so a
//!   dimension swap eases every channel instead of flashing.
//! * [`ChunkFadeTracker`] — per-`(cx, cz)` arrival ages producing a smooth
//!   `0.0 -> 1.0` fade over a quarter second.
//! * [`voxel_clouds_to_geometry`], [`planet_body_to_geometry`],
//!   [`planet_subworld_transform`] — convert `generate_layered_voxel_clouds`
//!   and `build_3d_planet_mesh` output straight into GPU-ready geometry.

use std::collections::BTreeMap;

use megu_client_mesh::space_sky_bridge::{
    build_3d_planet_mesh, DimensionAdaptationPolicy, PlanetSkyBody, SkyBodyPlacement,
    PLANET_TINT_ATMOSPHERE, PLANET_TINT_RELIEF, PLANET_TINT_RING, PLANET_TINT_SURFACE,
};
use megu_client_mesh::vanilla_plus_3d::ExtrudedMesh;
use megu_client_mesh::vanilla_plus_nature::{
    cloud_deck_spec, CloudLighting, VoxelCloudCell, FACE_NEG_X, FACE_NEG_Y, FACE_NEG_Z,
    FACE_POS_X, FACE_POS_Y, FACE_POS_Z,
};

use crate::subworld_render::{box_face_corners, SubWorldTransform, TerrainVertex};

// ---------------------------------------------------------------------------
// Atmosphere parameters + exponential blender
// ---------------------------------------------------------------------------

/// One fully-resolved atmosphere state: what the sky pass, fog pass and
/// lit-geometry shading consume for one frame. All fields are sanitized on
/// write (non-finite reads as the neutral default for that field).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AtmosphereParams {
    /// Sky color at the zenith.
    pub sky_zenith_rgb: [f32; 3],
    /// Sky color at the horizon (also the clear color).
    pub sky_horizon_rgb: [f32; 3],
    /// Fog color the distance dissolve blends toward.
    pub fog_rgb: [f32; 3],
    /// Exponential fog density (`0.0` = no haze, vacuum keeps this at 0).
    pub fog_density: f32,
    /// Underwater/cave depth tint multiplied into shaded color.
    pub depth_tint_rgb: [f32; 3],
    /// Depth tint strength `0.0..=1.0` (`0` = dry air).
    pub depth_tint_strength: f32,
    /// Hemisphere ambient light color.
    pub ambient_rgb: [f32; 3],
    /// Directional sun intensity multiplier.
    pub sun_intensity: f32,
}

impl Default for AtmosphereParams {
    /// Overworld noon: vanilla-ish blue sky, mild haze, no depth tint.
    fn default() -> Self {
        Self {
            sky_zenith_rgb: [0.30, 0.55, 0.98],
            sky_horizon_rgb: [0.62, 0.75, 0.92],
            fog_rgb: [0.62, 0.75, 0.92],
            fog_density: 0.0022,
            depth_tint_rgb: [0.45, 0.65, 0.85],
            depth_tint_strength: 0.0,
            ambient_rgb: [0.42, 0.46, 0.55],
            sun_intensity: 1.0,
        }
    }
}

fn clean(v: f32, fallback: f32) -> f32 {
    if v.is_finite() {
        v
    } else {
        fallback
    }
}

fn clean_rgb(rgb: [f32; 3], fallback: [f32; 3]) -> [f32; 3] {
    [
        clean(rgb[0], fallback[0]),
        clean(rgb[1], fallback[1]),
        clean(rgb[2], fallback[2]),
    ]
}

/// Bound a sanitized rgb triple: colors are linear-space light values, so
/// anything outside `0.0..=4.0` (negative albedo, absurd HDR) is clamped —
/// matching the `fog_density`/`sun_intensity` bounds below.
fn clamp_rgb(rgb: [f32; 3]) -> [f32; 3] {
    [
        rgb[0].clamp(0.0, 4.0),
        rgb[1].clamp(0.0, 4.0),
        rgb[2].clamp(0.0, 4.0),
    ]
}

fn lerp3(a: [f32; 3], b: [f32; 3], t: f32) -> [f32; 3] {
    [
        a[0] + (b[0] - a[0]) * t,
        a[1] + (b[1] - a[1]) * t,
        a[2] + (b[2] - a[2]) * t,
    ]
}

fn scale3(rgb: [f32; 3], k: f32) -> [f32; 3] {
    [rgb[0] * k, rgb[1] * k, rgb[2] * k]
}

fn sstep(edge0: f32, edge1: f32, x: f32) -> f32 {
    let t = ((x - edge0) / (edge1 - edge0)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

// ---------------------------------------------------------------------------
// Time-of-day color temperature + Beer-Lambert water column
// ---------------------------------------------------------------------------

/// Minecraft day length in ticks (24000): tick 0 is sunrise, 6000 noon,
/// 12000 sunset, 18000 midnight.
const TICKS_PER_DAY: u64 = 24_000;

/// Per-channel Beer-Lambert absorption coefficients for open ocean water
/// (per block): red dies first, then green; blue travels farthest.
pub const WATER_ABSORPTION_RGB: [f32; 3] = [0.30, 0.10, 0.045];

/// Default biome water tint (vanilla-ish clear-ocean teal). Biomes that tint
/// their water pass their own color into [`AtmosphereParams::underwater_from`].
pub const DEFAULT_WATER_TINT_RGB: [f32; 3] = [0.10, 0.30, 0.42];

/// Time-of-day color temperature as a multiplicative `[r, g, b]` tint for
/// sky, fog, cloud and water palettes:
///
/// * dawn (tick ~0): warm amber,
/// * noon (tick ~6000): neutral `~[1, 1, 1]` daylight,
/// * dusk (tick ~12000): warm copper,
/// * night (tick ~18000): cool silver-blue moonlight,
/// * `rain_strength` blends any of the above toward a desaturated, slightly
///   dimmed overcast grey.
///
/// Pure function of `(world_time_ticks, rain_strength)` — deterministic, no
/// clock, always finite and bounded to `0.0..=1.5` per channel.
pub fn time_of_day_color_temperature(world_time_ticks: u64, rain_strength: f32) -> [f32; 3] {
    let t = (world_time_ticks % TICKS_PER_DAY) as f32;
    let e = (std::f32::consts::TAU * t / TICKS_PER_DAY as f32).sin();

    // Day factor and the low-sun warm band (sunrise and sunset alike).
    let day = sstep(-0.10, 0.10, e);
    let warm_w = 1.0 - sstep(0.0, 0.35, e.abs());
    // Dawn leans amber, dusk leans copper — split by which half of the day.
    let dawn = t < 6000.0 || t >= 21_600.0;
    let warm_rgb = if dawn {
        [1.15, 0.95, 0.80]
    } else {
        [1.16, 0.85, 0.66]
    };
    let night_rgb = [0.70, 0.80, 1.10];

    // Night cool first, then the low-sun warm band on top: at the horizon
    // (e ≈ 0, day ≈ 0.5) the warm rim must dominate the dusk palette.
    let mut temp = lerp3([1.0, 1.0, 1.0], night_rgb, (1.0 - day) * 0.85);
    temp = lerp3(temp, warm_rgb, warm_w * 0.85);

    // Storm overcast: desaturate toward luminance and dim a touch.
    let storm = clean(rain_strength, 0.0).clamp(0.0, 1.0);
    if storm > 0.0 {
        let lum = 0.2126 * temp[0] + 0.7152 * temp[1] + 0.0722 * temp[2];
        temp = lerp3(temp, [lum, lum, lum], storm * 0.6);
        temp = scale3(temp, 1.0 - 0.25 * storm);
    }
    [temp[0].clamp(0.0, 1.5), temp[1].clamp(0.0, 1.5), temp[2].clamp(0.0, 1.5)]
}

/// Beer-Lambert transmittance through `depth_blocks` of `water_tint_rgb`
/// water: `exp(-a_c * d)` per channel using [`WATER_ABSORPTION_RGB`],
/// multiplied by the biome water tint. `depth <= 0` returns the tint
/// unchanged — the surface itself is already water-colored.
pub fn water_column_transmittance(depth_blocks: f32, water_tint_rgb: [f32; 3]) -> [f32; 3] {
    let d = clean(depth_blocks, 0.0).max(0.0);
    let tint = clean_rgb(water_tint_rgb, DEFAULT_WATER_TINT_RGB);
    [
        tint[0] * (-WATER_ABSORPTION_RGB[0] * d).exp(),
        tint[1] * (-WATER_ABSORPTION_RGB[1] * d).exp(),
        tint[2] * (-WATER_ABSORPTION_RGB[2] * d).exp(),
    ]
}

/// Smoothly blend a dry-air atmosphere into its water-column counterpart.
/// `submersion_01` is smoothstepped internally so entering or leaving water
/// glides through the transition instead of snapping flat at the surface.
/// Inputs and output are [`AtmosphereParams::sanitized`].
pub fn blend_water_transition(
    above: &AtmosphereParams,
    underwater: &AtmosphereParams,
    submersion_01: f32,
) -> AtmosphereParams {
    let s = sstep(0.0, 1.0, clean(submersion_01, 0.0));
    let (a, u) = (above.sanitized(), underwater.sanitized());
    AtmosphereParams {
        sky_zenith_rgb: lerp3(a.sky_zenith_rgb, u.sky_zenith_rgb, s),
        sky_horizon_rgb: lerp3(a.sky_horizon_rgb, u.sky_horizon_rgb, s),
        fog_rgb: lerp3(a.fog_rgb, u.fog_rgb, s),
        fog_density: a.fog_density + (u.fog_density - a.fog_density) * s,
        depth_tint_rgb: lerp3(a.depth_tint_rgb, u.depth_tint_rgb, s),
        depth_tint_strength: a.depth_tint_strength
            + (u.depth_tint_strength - a.depth_tint_strength) * s,
        ambient_rgb: lerp3(a.ambient_rgb, u.ambient_rgb, s),
        sun_intensity: a.sun_intensity + (u.sun_intensity - a.sun_intensity) * s,
    }
    .sanitized()
}

/// Phase-1 seamless dimension transfer crossfade: cubic-smoothstep
/// (`t = p*p*(3-2p)` on clamped `progress`) linear blend of every
/// [`AtmosphereParams`] channel — zenith/horizon/fog colors, density,
/// depth tint, ambient and sun — so Overworld ↔ Nether ↔ End ↔ custom
/// mod-dimension transfers glide with zero harsh flash. `progress <= 0`
/// reproduces `from`, `>= 1` reproduces `to`; inputs and output are
/// sanitized like [`blend_water_transition`].
pub fn blend_dimension_crossfade(
    from: &AtmosphereParams,
    to: &AtmosphereParams,
    progress: f32,
) -> AtmosphereParams {
    // Pin the endpoints exactly: `a + (b - a) * t` is not bit-identical
    // to `b` at t = 1, and the landing frame must reproduce the
    // destination palette precisely so the fade finishes flash-free.
    // NaN `progress` fails both comparisons and falls through to the
    // blend, where `clean` folds it to `from` anyway.
    if progress <= 0.0 {
        return from.sanitized();
    }
    if progress >= 1.0 {
        return to.sanitized();
    }
    blend_water_transition(from, to, progress)
}

/// Horizon-melt factor in `0.0..=1.0` for one cloud cell at
/// `distance_blocks` from the viewer: `1.0` inside `fade_start`, smoothstep
/// down to `0.0` at `fade_end` so decks dissolve into the horizon fog
/// instead of clipping at the field edge. A degenerate or non-finite range
/// falls back to a hard cutoff at `fade_start` (still bounded).
pub fn cloud_horizon_fade(distance_blocks: f32, fade_start: f32, fade_end: f32) -> f32 {
    let d = clean(distance_blocks, 0.0).max(0.0);
    let start = clean(fade_start, 0.0).max(0.0);
    let end = clean(fade_end, start);
    if end <= start {
        return if d >= start { 0.0 } else { 1.0 };
    }
    1.0 - sstep(start, end, d)
}

impl AtmosphereParams {
    /// Vacuum/orbit: star-black sky, zero haze, stark sun.
    pub fn vacuum_orbit() -> Self {
        Self {
            sky_zenith_rgb: [0.005, 0.008, 0.02],
            sky_horizon_rgb: [0.01, 0.012, 0.03],
            fog_rgb: [0.01, 0.012, 0.03],
            fog_density: 0.0,
            depth_tint_rgb: [1.0, 1.0, 1.0],
            depth_tint_strength: 0.0,
            ambient_rgb: [0.06, 0.07, 0.10],
            sun_intensity: 1.25,
        }
    }

    /// Underwater: foggy teal with a strong depth tint, derived through the
    /// shared Beer-Lambert pipeline at a typical eye depth so it always
    /// stays harmonized with the noon surface palette.
    pub fn underwater() -> Self {
        Self::default().underwater_from(DEFAULT_WATER_TINT_RGB, 8.0)
    }

    /// Underwater state derived from THIS surface atmosphere: every channel
    /// is the surface value filtered through a `depth_blocks` column of
    /// `water_tint_rgb` water via the same [`WATER_ABSORPTION_RGB`]
    /// Beer-Lambert curve as [`water_column_transmittance`], plus the
    /// in-scattered light of the medium itself. Because surface sky, fog,
    /// ambient and sun all flow through the same optical path, crossing the
    /// water surface can never snap to a mismatched flat color — feed the
    /// result through [`blend_water_transition`] for a gradual entry.
    ///
    /// `depth_blocks` is clamped to `0..=32`; deeper water absorbs harder
    /// (reds die first, blues persist — classic Beer-Lambert).
    pub fn underwater_from(&self, water_tint_rgb: [f32; 3], depth_blocks: f32) -> Self {
        let surface = self.sanitized();
        let d = clean(depth_blocks, 8.0).clamp(0.0, 32.0);
        let tint = clean_rgb(water_tint_rgb, DEFAULT_WATER_TINT_RGB);
        // Transmittance without the tint multiplier (tint enters as the
        // in-scatter medium color instead).
        let t = [
            (-WATER_ABSORPTION_RGB[0] * d).exp(),
            (-WATER_ABSORPTION_RGB[1] * d).exp(),
            (-WATER_ABSORPTION_RGB[2] * d).exp(),
        ];
        let t_mean = (t[0] + t[1] + t[2]) / 3.0;
        // transmitted sky light + medium in-scatter per channel.
        let veil = |src: [f32; 3], scatter: f32| {
            [
                src[0] * t[0] * 0.6 + tint[0] * (1.0 - t[0]) * scatter,
                src[1] * t[1] * 0.6 + tint[1] * (1.0 - t[1]) * scatter,
                src[2] * t[2] * 0.6 + tint[2] * (1.0 - t[2]) * scatter,
            ]
        };
        Self {
            sky_zenith_rgb: veil(surface.sky_zenith_rgb, 0.65),
            sky_horizon_rgb: veil(surface.sky_horizon_rgb, 0.85),
            fog_rgb: veil(surface.sky_horizon_rgb, 1.0),
            // Water is ~10x denser than air haze; never thinner than a mist.
            fog_density: (0.008 + 0.03 * (1.0 - t_mean))
                .max(surface.fog_density * 0.5)
                .clamp(0.0, 4.0),
            // The medium's tint multiplied into shaded color: brighten the
            // tint so the modulated albedo reads watery, not black.
            depth_tint_rgb: clamp_rgb(scale3(tint, 1.6)),
            depth_tint_strength: (0.45 + 0.4 * (1.0 - t_mean)).clamp(0.0, 1.0),
            ambient_rgb: scale3(lerp3(tint, surface.ambient_rgb, t_mean), 0.9),
            sun_intensity: (surface.sun_intensity * (0.4 + 0.35 * t_mean)).clamp(0.0, 16.0),
        }
        .sanitized()
    }

    /// Time-of-day tinted copy: multiplies sky, fog and ambient channels by
    /// [`time_of_day_color_temperature`] so the whole palette shares one
    /// warm/cool rhythm; `sun_intensity` tracks the tint luminance (storms
    /// dim the sun). Depth fields are untouched — water tints stay physical.
    pub fn with_time_of_day(&self, world_time_ticks: u64, rain_strength: f32) -> Self {
        let temp = time_of_day_color_temperature(world_time_ticks, rain_strength);
        let p = self.sanitized();
        let lum = 0.2126 * temp[0] + 0.7152 * temp[1] + 0.0722 * temp[2];
        Self {
            sky_zenith_rgb: clamp_rgb([
                p.sky_zenith_rgb[0] * temp[0],
                p.sky_zenith_rgb[1] * temp[1],
                p.sky_zenith_rgb[2] * temp[2],
            ]),
            sky_horizon_rgb: clamp_rgb([
                p.sky_horizon_rgb[0] * temp[0],
                p.sky_horizon_rgb[1] * temp[1],
                p.sky_horizon_rgb[2] * temp[2],
            ]),
            fog_rgb: clamp_rgb([
                p.fog_rgb[0] * temp[0],
                p.fog_rgb[1] * temp[1],
                p.fog_rgb[2] * temp[2],
            ]),
            fog_density: p.fog_density,
            depth_tint_rgb: p.depth_tint_rgb,
            depth_tint_strength: p.depth_tint_strength,
            ambient_rgb: clamp_rgb([
                p.ambient_rgb[0] * temp[0],
                p.ambient_rgb[1] * temp[1],
                p.ambient_rgb[2] * temp[2],
            ]),
            sun_intensity: (p.sun_intensity * lum).clamp(0.0, 16.0),
        }
    }

    /// The atmosphere a [`DimensionAdaptationPolicy`] implies: vacuum
    /// bodies (orbit / no oxygen / no air transmission) get the stark
    /// orbital look, breathable dimensions get the overworld tinted by the
    /// policy's `sky_tint_rgb`.
    pub fn for_policy(policy: &DimensionAdaptationPolicy) -> Self {
        if policy.is_orbit || !policy.has_oxygen || policy.acoustic_air_transmission < 0.05 {
            let mut params = Self::vacuum_orbit();
            let tint = policy.sky_tint_rgb;
            params.sky_zenith_rgb = scale3(
                params.sky_zenith_rgb,
                0.5 + tint[0].max(tint[1]).max(tint[2]),
            );
            params.sky_horizon_rgb = scale3(
                params.sky_horizon_rgb,
                0.5 + tint[0].max(tint[1]).max(tint[2]),
            );
            params.fog_rgb = params.sky_horizon_rgb;
            params.ambient_rgb = [
                params.ambient_rgb[0] * (0.5 + tint[0]),
                params.ambient_rgb[1] * (0.5 + tint[1]),
                params.ambient_rgb[2] * (0.5 + tint[2]),
            ];
            params
        } else {
            let mut params = Self::default();
            let tint = policy.sky_tint_rgb;
            params.sky_zenith_rgb = [
                (params.sky_zenith_rgb[0] * tint[0]).clamp(0.0, 1.0),
                (params.sky_zenith_rgb[1] * tint[1]).clamp(0.0, 1.0),
                (params.sky_zenith_rgb[2] * tint[2]).clamp(0.0, 1.0),
            ];
            params.sky_horizon_rgb = [
                (params.sky_horizon_rgb[0] * tint[0]).clamp(0.0, 1.0),
                (params.sky_horizon_rgb[1] * tint[1]).clamp(0.0, 1.0),
                (params.sky_horizon_rgb[2] * tint[2]).clamp(0.0, 1.0),
            ];
            params.fog_rgb = params.sky_horizon_rgb;
            // Thinner atmospheres see farther; never thinner than vacuum 0.
            params.fog_density *= policy.acoustic_air_transmission.clamp(0.0, 1.0);
            params
        }
    }

    /// Sanitized copy: non-finite fields read as the neutral value and rgb
    /// channels are bounded to `0.0..=4.0`, so a corrupt biome record cannot
    /// paint NaN or absurd light values into a frame uniform.
    pub fn sanitized(&self) -> Self {
        let neutral = Self::default();
        Self {
            sky_zenith_rgb: clamp_rgb(clean_rgb(self.sky_zenith_rgb, neutral.sky_zenith_rgb)),
            sky_horizon_rgb: clamp_rgb(clean_rgb(self.sky_horizon_rgb, neutral.sky_horizon_rgb)),
            fog_rgb: clamp_rgb(clean_rgb(self.fog_rgb, neutral.fog_rgb)),
            fog_density: clean(self.fog_density, neutral.fog_density).clamp(0.0, 4.0),
            depth_tint_rgb: clamp_rgb(clean_rgb(self.depth_tint_rgb, neutral.depth_tint_rgb)),
            depth_tint_strength: clean(self.depth_tint_strength, 0.0).clamp(0.0, 1.0),
            ambient_rgb: clamp_rgb(clean_rgb(self.ambient_rgb, neutral.ambient_rgb)),
            sun_intensity: clean(self.sun_intensity, 1.0).clamp(0.0, 16.0),
        }
    }
}

/// Exponential time-blend between atmosphere states (biome borders,
/// Overworld -> orbit ascents, submersion). `advance(dt_sec)` moves
/// `current` a fraction `1 - exp(-dt/tau)` of the way to `target` — smooth,
/// monotonic, and guaranteed NaN-free.
#[derive(Clone, Debug)]
pub struct BiomeAtmosphereBlender {
    current: AtmosphereParams,
    target: AtmosphereParams,
    /// Time constant in seconds (`DEFAULT_TAU_SECS` unless overridden).
    pub tau_secs: f32,
}

impl BiomeAtmosphereBlender {
    /// Default blend time constant: ~1.2 s feels smooth without lagging
    /// behind sprint-speed biome crossings.
    pub const DEFAULT_TAU_SECS: f32 = 1.2;

    /// Blender already showing `initial` (no transition in flight).
    pub fn new(initial: AtmosphereParams) -> Self {
        let initial = initial.sanitized();
        Self {
            current: initial,
            target: initial,
            tau_secs: Self::DEFAULT_TAU_SECS,
        }
    }

    /// Point the blend at a new biome/dimension atmosphere.
    pub fn set_target(&mut self, target: AtmosphereParams) {
        self.target = target.sanitized();
    }

    /// The blend destination.
    pub fn target(&self) -> AtmosphereParams {
        self.target
    }

    /// The sanitized atmosphere for this frame (post-`advance`).
    pub fn current(&self) -> AtmosphereParams {
        self.current
    }

    /// `true` when `current` is within `eps` of `target` on every channel.
    /// The channel lists live on the stack — no heap allocation.
    pub fn is_settled(&self, eps: f32) -> bool {
        fn channels(p: &AtmosphereParams) -> [f32; 18] {
            [
                p.sky_zenith_rgb[0],
                p.sky_zenith_rgb[1],
                p.sky_zenith_rgb[2],
                p.sky_horizon_rgb[0],
                p.sky_horizon_rgb[1],
                p.sky_horizon_rgb[2],
                p.fog_rgb[0],
                p.fog_rgb[1],
                p.fog_rgb[2],
                p.fog_density,
                p.depth_tint_rgb[0],
                p.depth_tint_rgb[1],
                p.depth_tint_rgb[2],
                p.depth_tint_strength,
                p.ambient_rgb[0],
                p.ambient_rgb[1],
                p.ambient_rgb[2],
                p.sun_intensity,
            ]
        }
        let current = channels(&self.current);
        let target = channels(&self.target);
        current
            .iter()
            .zip(target.iter())
            .all(|(a, b)| (a - b).abs() <= eps)
    }

    /// Jump straight to `target` (dimension load, respawn).
    pub fn snap_to_target(&mut self) {
        self.current = self.target;
    }

    /// Advance the blend by `dt_sec` frame seconds and return the new
    /// current state. `dt_sec <= 0` or non-finite leaves `current` untouched.
    pub fn advance(&mut self, dt_sec: f32) -> AtmosphereParams {
        let tau = if self.tau_secs.is_finite() && self.tau_secs > 0.0 {
            self.tau_secs
        } else {
            Self::DEFAULT_TAU_SECS
        };
        let t = if dt_sec.is_finite() && dt_sec > 0.0 {
            1.0 - (-dt_sec / tau).exp()
        } else {
            0.0
        };
        if t <= 0.0 {
            return self.current;
        }
        let (c, d) = (self.current, self.target);
        self.current = AtmosphereParams {
            sky_zenith_rgb: lerp3(c.sky_zenith_rgb, d.sky_zenith_rgb, t),
            sky_horizon_rgb: lerp3(c.sky_horizon_rgb, d.sky_horizon_rgb, t),
            fog_rgb: lerp3(c.fog_rgb, d.fog_rgb, t),
            fog_density: c.fog_density + (d.fog_density - c.fog_density) * t,
            depth_tint_rgb: lerp3(c.depth_tint_rgb, d.depth_tint_rgb, t),
            depth_tint_strength: c.depth_tint_strength
                + (d.depth_tint_strength - c.depth_tint_strength) * t,
            ambient_rgb: lerp3(c.ambient_rgb, d.ambient_rgb, t),
            sun_intensity: c.sun_intensity + (d.sun_intensity - c.sun_intensity) * t,
        }
        .sanitized();
        self.current
    }
}

// ---------------------------------------------------------------------------
// Chunk fade-in tracker
// ---------------------------------------------------------------------------

/// Tracks when each `(cx, cz)` chunk column first appeared and produces a
/// smooth `0.0 -> 1.0` alpha ramp so newly arrived chunks dissolve in
/// instead of popping. Ages advance only through `tick(dt_sec)` — no wall
/// clock is ever read.
#[derive(Clone, Debug)]
pub struct ChunkFadeTracker {
    ages: BTreeMap<(i32, i32), f32>,
    /// Seconds a chunk takes to fully fade in (`DEFAULT_FADE_SECONDS`).
    pub fade_seconds: f32,
}

impl Default for ChunkFadeTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl ChunkFadeTracker {
    /// Vanilla-feel dissolve: a quarter second per newly arrived chunk.
    pub const DEFAULT_FADE_SECONDS: f32 = 0.25;
    /// Hard bound on simultaneously tracked chunk columns. Generous enough
    /// for the largest sane view distance (~64 chunk radius); chunks noted
    /// while the tracker is saturated simply render fully opaque.
    const MAX_TRACKED_CHUNKS: usize = 8192;

    /// Empty tracker with the default fade duration.
    pub fn new() -> Self {
        Self {
            ages: BTreeMap::new(),
            fade_seconds: Self::DEFAULT_FADE_SECONDS,
        }
    }

    /// Register a chunk as just-arrived. Returns `true` when the key was
    /// newly tracked; re-noting an already-fading chunk keeps its progress
    /// (streaming systems re-announce visible chunks every frame).
    ///
    /// The map is bounded: at [`Self::MAX_TRACKED_CHUNKS`] finished fades
    /// are evicted first (they are observationally identical to untracked
    /// chunks), and a still-saturated tracker refuses the note so memory
    /// can never grow without bound across teleports or missed unloads.
    pub fn note_chunk(&mut self, cx: i32, cz: i32) -> bool {
        if self.ages.contains_key(&(cx, cz)) {
            return false;
        }
        if self.ages.len() >= Self::MAX_TRACKED_CHUNKS {
            self.evict_finished();
            if self.ages.len() >= Self::MAX_TRACKED_CHUNKS {
                return false;
            }
        }
        self.ages.insert((cx, cz), 0.0);
        true
    }

    /// The fade duration `fade_alpha`/`is_fading` currently resolve to.
    fn resolved_fade_seconds(&self) -> f32 {
        if self.fade_seconds.is_finite() && self.fade_seconds > 0.0 {
            self.fade_seconds
        } else {
            Self::DEFAULT_FADE_SECONDS
        }
    }

    /// Drop entries whose fade already completed. A finished entry reports
    /// `fade_alpha == 1.0` and `is_fading == false` — exactly like an
    /// untracked chunk — so eviction cannot change rendered output, only
    /// `len()`.
    fn evict_finished(&mut self) {
        let duration = self.resolved_fade_seconds();
        self.ages.retain(|_, age| *age < duration);
    }

    /// Stop tracking a chunk (unload). Returns `true` when it was tracked.
    pub fn forget(&mut self, cx: i32, cz: i32) -> bool {
        self.ages.remove(&(cx, cz)).is_some()
    }

    /// Advance all tracked ages by `dt_sec` frame seconds.
    pub fn tick(&mut self, dt_sec: f32) {
        if !dt_sec.is_finite() || dt_sec <= 0.0 {
            return;
        }
        for age in self.ages.values_mut() {
            *age += dt_sec;
        }
    }

    /// `0.0 -> 1.0` fade for a chunk column. Untracked chunks report `1.0`
    /// so anything that bypasses `note_chunk` stays fully opaque.
    pub fn fade_alpha(&self, cx: i32, cz: i32) -> f32 {
        let duration = self.resolved_fade_seconds();
        match self.ages.get(&(cx, cz)) {
            Some(age) => (age / duration).clamp(0.0, 1.0),
            None => 1.0,
        }
    }

    /// `true` while a chunk is still fading in.
    pub fn is_fading(&self, cx: i32, cz: i32) -> bool {
        self.ages.contains_key(&(cx, cz)) && self.fade_alpha(cx, cz) < 1.0
    }

    /// `true` when any tracked chunk's fade is still animating — the
    /// opaque-terrain RenderBundle path consults this because fade values
    /// bake into the recorded draw uniforms.
    pub fn any_fading(&self) -> bool {
        let duration = self.resolved_fade_seconds();
        self.ages.values().any(|age| *age < duration)
    }

    /// Drop every chunk not accepted by `keep` (world unloads, view-radius
    /// shrink). `keep` receives `(cx, cz)`.
    pub fn retain<F: FnMut((i32, i32)) -> bool>(&mut self, mut keep: F) {
        self.ages.retain(|key, _| keep(*key));
    }

    /// Tracked chunk count.
    pub fn len(&self) -> usize {
        self.ages.len()
    }

    /// `true` when nothing is tracked.
    pub fn is_empty(&self) -> bool {
        self.ages.is_empty()
    }
}

// ---------------------------------------------------------------------------
// Layered voxel clouds -> TerrainVertex geometry
// ---------------------------------------------------------------------------

/// Global alpha a cloud entry should carry into the translucent lit pass.
pub const CLOUD_RENDER_ALPHA: f32 = 0.8;

const CLOUD_FACE_BITS: [u8; 6] = [
    FACE_POS_X, FACE_NEG_X, FACE_POS_Y, FACE_NEG_Y, FACE_POS_Z, FACE_NEG_Z,
];

/// Sanitize one cloud-lighting channel: `CloudLighting` fields are public,
/// so a hand-built value could carry NaN or negative light into vertex
/// albedo. Non-finite reads as `0.0`, negatives clamp to `0.0`.
fn cloud_channel(v: f32) -> f32 {
    if v.is_finite() {
        v.max(0.0)
    } else {
        0.0
    }
}

/// Emit one cloud cell's exposed quads into `vertices`/`indices`. Shading:
/// top faces take `lighting.top_rgb`, bottoms `bottom_rgb`, sides
/// `side_rgb`; `layer` adds the puffy-cap boost and `extra_boost` applies
/// the styled path's forward-scatter silver lining. Non-finite or
/// non-positive-size cells emit nothing.
fn push_cloud_cell_geometry(
    cell: &VoxelCloudCell,
    lighting: &CloudLighting,
    extra_boost: f32,
    vertices: &mut Vec<TerrainVertex>,
    indices: &mut Vec<u32>,
) {
    let max = [
        cell.min[0] + cell.size[0],
        cell.min[1] + cell.size[1],
        cell.min[2] + cell.size[2],
    ];
    if !cell.min.iter().chain(max.iter()).all(|v| v.is_finite())
        || !cell.size.iter().all(|s| s.is_finite() && *s > 0.0)
    {
        return;
    }
    let cap_boost = (1.0 + 0.04 * f32::from(cell.layer)) * extra_boost.clamp(0.0, 1.25);
    let faces = box_face_corners(cell.min, max);
    for (face, (normal, corners)) in faces.iter().enumerate() {
        if cell.exposed_faces & CLOUD_FACE_BITS[face] == 0 {
            continue;
        }
        let rgb = match face {
            2 => lighting.top_rgb,
            3 => lighting.bottom_rgb,
            _ => lighting.side_rgb,
        };
        let albedo = [
            (cloud_channel(rgb[0]) * cap_boost).min(1.0),
            (cloud_channel(rgb[1]) * cap_boost).min(1.0),
            (cloud_channel(rgb[2]) * cap_boost).min(1.0),
        ];
        let base = vertices.len() as u32;
        for (corner, &position) in corners.iter().enumerate() {
            vertices.push(TerrainVertex::new(
                position,
                *normal,
                albedo,
                [[0.0, 0.0], [0.0, 1.0], [1.0, 1.0], [1.0, 0.0]][corner],
                u32::MAX,
            ));
        }
        indices.extend([base, base + 1, base + 2, base, base + 2, base + 3]);
    }
}

/// Convert [`generate_layered_voxel_clouds`] cells into lit geometry: one
/// quad per exposed face, top faces tinted `lighting.top_rgb`, bottoms
/// `bottom_rgb`, sides `side_rgb` (upper strata slightly brighter for the
/// puffy-cap look). Upload the result as a [`SubWorldRenderEntry`] with
/// `alpha = CLOUD_RENDER_ALPHA` and an identity transform — cell positions
/// are already world-space.
///
/// [`generate_layered_voxel_clouds`]: megu_client_mesh::vanilla_plus_nature::generate_layered_voxel_clouds
pub fn voxel_clouds_to_geometry(
    cells: &[VoxelCloudCell],
    lighting: &CloudLighting,
) -> (Vec<TerrainVertex>, Vec<u32>) {
    let quads: usize = cells.iter().map(|c| c.quad_count() as usize).sum();
    let mut vertices = Vec::with_capacity(quads * 4);
    let mut indices = Vec::with_capacity(quads * 6);
    for cell in cells {
        push_cloud_cell_geometry(cell, lighting, 1.0, &mut vertices, &mut indices);
    }
    (vertices, indices)
}

/// One cloud geometry batch sharing a single draw-entry alpha — cells are
/// grouped by `(layer, horizon band)` so each atmospheric deck gets its own
/// translucency and distant cells melt toward the fog.
pub struct CloudGeometryBatch {
    /// Alpha for this batch's draw entry: `deck opacity * horizon band`.
    /// Multiply by the caller's global cloud pass alpha if one is applied.
    pub alpha: f32,
    /// Lit, shaded `TerrainVertex` stream (4 verts per quad).
    pub vertices: Vec<TerrainVertex>,
    /// Index stream (6 per quad).
    pub indices: Vec<u32>,
    /// The `cell.layer` these quads came from (`0` cumulus, `1..` caps).
    pub layer: u8,
    /// Horizon-melt band `0..=4` (`4` = fully crisp, lower = foggier).
    pub fade_band: u8,
}

/// Styling knobs for [`voxel_clouds_to_geometry_styled`]: which deck count
/// resolves each cell's `layer` into its [`cloud_deck_spec`], where the
/// viewer stands for the horizon melt, and the fade's start/end distances.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CloudGeometryStyle {
    /// Active deck count — pass `config.effective_cloud_layers()` so each
    /// cell's `layer` resolves to the right deck spec (opacity, role).
    pub layers: u8,
    /// Viewer world XZ for the horizon-melt distance.
    pub viewer_xz: [f32; 2],
    /// Distance (blocks) at which clouds begin melting into horizon fog.
    pub horizon_fade_start: f32,
    /// Distance (blocks) at which clouds are fully melted.
    pub horizon_fade_end: f32,
}

impl Default for CloudGeometryStyle {
    /// Three decks around the origin, outer ~40% of a 12-block-cell radius
    /// melting into the fog.
    fn default() -> Self {
        Self {
            layers: 3,
            viewer_xz: [0.0, 0.0],
            horizon_fade_start: 48.0,
            horizon_fade_end: 78.0,
        }
    }
}

/// Styled variant of [`voxel_clouds_to_geometry`]: identical shading, but
/// cells are grouped into [`CloudGeometryBatch`]es by deck layer and a
/// quantized horizon-melt band. Each batch's `alpha` is
/// `deck opacity * band`, so cumulus stays near-opaque, altocumulus and
/// cirrus read genuinely translucent, and cells near `horizon_fade_end`
/// dissolve instead of clipping. Partially melted cells also get a soft
/// forward-scatter "silver lining" brightening on their lit faces.
///
/// Batches are returned in deterministic `(layer, band)` order; fully
/// melted cells are dropped entirely.
pub fn voxel_clouds_to_geometry_styled(
    cells: &[VoxelCloudCell],
    lighting: &CloudLighting,
    style: &CloudGeometryStyle,
) -> Vec<CloudGeometryBatch> {
    const FADE_BANDS: f32 = 4.0;
    let mut batches: BTreeMap<(u8, u8), CloudGeometryBatch> = BTreeMap::new();
    for cell in cells {
        let cx = cell.min[0] + cell.size[0] * 0.5;
        let cz = cell.min[2] + cell.size[2] * 0.5;
        let dist = ((cx - style.viewer_xz[0]).powi(2) + (cz - style.viewer_xz[1]).powi(2)).sqrt();
        let fade = cloud_horizon_fade(dist, style.horizon_fade_start, style.horizon_fade_end);
        // Quantize into FADE_BANDS+1 levels; band 0 is fully melted.
        let band = (fade * FADE_BANDS).round().clamp(0.0, FADE_BANDS) as u8;
        if band == 0 {
            continue;
        }
        let spec = cloud_deck_spec(cell.layer, style.layers);
        // Soft forward-scatter silver lining on partially melted edges.
        let lining = 1.0 + 0.10 * (1.0 - fade);
        let key = (cell.layer, band);
        let batch = batches.entry(key).or_insert_with(|| CloudGeometryBatch {
            alpha: (spec.opacity * (band as f32 / FADE_BANDS)).clamp(0.0, 1.0),
            vertices: Vec::new(),
            indices: Vec::new(),
            layer: cell.layer,
            fade_band: band,
        });
        push_cloud_cell_geometry(cell, lighting, lining, &mut batch.vertices, &mut batch.indices);
    }
    batches.into_values().collect()
}

// ---------------------------------------------------------------------------
// ExtrudedMesh / cubic planets -> TerrainVertex geometry
// ---------------------------------------------------------------------------

/// Convert any [`ExtrudedMesh`] (`positions`/`normal`/`tint_index` quads)
/// into lit geometry, resolving each `tint_index` through `albedo_of`.
/// `tint_index < 0` (untinted) still queries `albedo_of` so the caller
/// controls the fallback color.
pub fn extruded_mesh_to_geometry(
    mesh: &ExtrudedMesh,
    albedo_of: impl Fn(i32) -> [f32; 3],
) -> (Vec<TerrainVertex>, Vec<u32>) {
    let mut vertices = Vec::with_capacity(mesh.quads.len() * 4);
    let mut indices = Vec::with_capacity(mesh.quads.len() * 6);
    for quad in &mesh.quads {
        // `ExtrudedMesh` fields are public: skip quads whose geometry would
        // put non-finite data into a vertex buffer, and bound the resolved
        // albedo to linear `0..=1`.
        if !quad.positions.iter().flatten().all(|c| c.is_finite())
            || !quad.normal.iter().all(|c| c.is_finite())
            || !quad.uv.iter().flatten().all(|c| c.is_finite())
        {
            continue;
        }
        let albedo = albedo_of(quad.tint_index).map(|c| {
            if c.is_finite() {
                c.clamp(0.0, 1.0)
            } else {
                0.0
            }
        });
        let base = vertices.len() as u32;
        for (&position, &uv) in quad.positions.iter().zip(quad.uv.iter()) {
            vertices.push(TerrainVertex::new(
                position,
                quad.normal,
                albedo,
                uv,
                // ExtrudedMesh has UVs but no texture identifier. Callers
                // can select a sprite on the returned vertices.
                u32::MAX,
            ));
        }
        indices.extend([base, base + 1, base + 2, base, base + 2, base + 3]);
    }
    (vertices, indices)
}

/// Convert a [`PlanetSkyBody`] into GPU-ready lit geometry through the real
/// [`build_3d_planet_mesh`]: surface plates take `surface_rgb`, the
/// atmosphere rim shell takes `rim_rgb` (surface when absent), the ring
/// band takes `ring.rgb` dimmed by its alpha, and relief steps take a
/// slightly darkened surface so the voxel-sphere silhouette reads in shade.
/// Geometry is in `0..=1` local space centred on `(0.5, 0.5, 0.5)` —
/// pair it with [`planet_subworld_transform`] for sky placement.
pub fn planet_body_to_geometry(body: &PlanetSkyBody) -> (Vec<TerrainVertex>, Vec<u32>) {
    let mesh = build_3d_planet_mesh(body);
    let surface = body.effective_surface_rgb();
    let rim = body.effective_rim_rgb().unwrap_or(surface);
    let (ring_rgb, ring_alpha) = body
        .ring
        .map(|r| (r.effective_rgb(), r.effective_alpha()))
        .unwrap_or(([0.0; 3], 0.0));
    let relief = scale3(surface, 0.85);
    extruded_mesh_to_geometry(&mesh, |tint| match tint {
        PLANET_TINT_SURFACE => surface,
        PLANET_TINT_ATMOSPHERE => rim,
        PLANET_TINT_RING => scale3(ring_rgb, ring_alpha.max(0.15)),
        PLANET_TINT_RELIEF => relief,
        _ => surface,
    })
}

/// Sky placement for a body's `0..=1` local-space mesh: the mesh centre is
/// placed `distance` blocks along `placement.direction` and scaled so its
/// silhouette subtends `placement.apparent_angular_radius_deg`.
/// `distance` is the far-plane draw distance the sky pass uses (keep it
/// inside `camera.far`).
pub fn planet_subworld_transform(placement: &SkyBodyPlacement, distance: f32) -> SubWorldTransform {
    let distance = if distance.is_finite() && distance > 1.0 {
        distance
    } else {
        256.0
    };
    // `SkyBodyPlacement` fields are public: a hand-built placement could
    // carry a zero/degenerate or non-finite direction, or a NaN radius
    // (`f32::clamp` propagates NaN). Fall back to a sane sky point and a
    // mid-size apparent radius so the transform can never go non-finite.
    let radius_deg = if placement.apparent_angular_radius_deg.is_finite() {
        placement.apparent_angular_radius_deg.clamp(0.5, 90.0)
    } else {
        8.0
    };
    // Unit cube -> world diameter subtending the apparent angular diameter.
    let scale = 2.0 * distance * (radius_deg.to_radians() * 0.5).tan();
    let d = placement.direction;
    let len_sq = d[0] * d[0] + d[1] * d[1] + d[2] * d[2];
    let dir = if len_sq.is_finite() && len_sq > 1.0e-12 {
        let inv = len_sq.sqrt().recip();
        [d[0] * inv, d[1] * inv, d[2] * inv]
    } else {
        [0.0, 0.0, -1.0]
    };
    let center = [dir[0] * distance, dir[1] * distance, dir[2] * distance];
    // Centre of the 0..1 mesh lands exactly on the sky-sphere point.
    let half = scale * 0.5;
    SubWorldTransform {
        position: [center[0] - half, center[1] - half, center[2] - half],
        quaternion: [0.0, 0.0, 0.0, 1.0],
        scale,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use megu_client_mesh::vanilla_plus_nature::{
        compute_cloud_lighting, generate_layered_voxel_clouds, VanillaPlusNatureConfig,
    };

    fn cloud_cfg(layers: u8) -> VanillaPlusNatureConfig {
        VanillaPlusNatureConfig {
            enabled: true,
            layered_voxel_clouds: true,
            cloud_layers: layers,
            ..VanillaPlusNatureConfig::default()
        }
    }

    #[test]
    fn color_temperature_walks_amber_noon_copper_silver_and_storm_grey() {
        let noon = time_of_day_color_temperature(6_000, 0.0);
        // Noon is neutral daylight: all channels near 1, warm/cool balanced.
        for c in noon {
            assert!((0.9..=1.1).contains(&c), "noon channel {c} must stay neutral");
        }

        let dawn = time_of_day_color_temperature(300, 0.0);
        let dusk = time_of_day_color_temperature(12_300, 0.0);
        // Both horizon bands run warm (r > b); dusk is the deeper copper.
        assert!(dawn[0] > dawn[2], "dawn must lean warm amber");
        assert!(dusk[0] > dusk[2], "dusk must lean warm copper");
        assert!(dusk[0] - dusk[1] > dawn[0] - dawn[1],
            "dusk copper must out-warm dawn amber");

        let night = time_of_day_color_temperature(18_000, 0.0);
        // Night: cool silver-blue — blue channel exceeds red, overall dimmer.
        assert!(night[2] > night[0], "night must lean cool silver-blue");
        assert!(night[0] < noon[0]);

        // Storm overcast: channels converge toward luminance and dim.
        let storm = time_of_day_color_temperature(6_000, 1.0);
        let spread_storm = storm[0].max(storm[1]).max(storm[2])
            - storm[0].min(storm[1]).min(storm[2]);
        let spread_dusk = dusk[0].max(dusk[1]).max(dusk[2])
            - dusk[0].min(dusk[1]).min(dusk[2]);
        let storm_dusk = time_of_day_color_temperature(12_300, 1.0);
        let spread_storm_dusk = storm_dusk[0].max(storm_dusk[1]).max(storm_dusk[2])
            - storm_dusk[0].min(storm_dusk[1]).min(storm_dusk[2]);
        assert!(spread_storm_dusk < spread_dusk, "storm must desaturate");
        assert!(storm[0].max(storm[1]).max(storm[2]) <= noon[0].max(noon[1]).max(noon[2]),
            "storm dims every channel");
        assert!(spread_storm < 0.15, "full storm should be near-grey");

        // Deterministic and bounded at extremes.
        let wild = time_of_day_color_temperature(u64::MAX, f32::NAN);
        assert!(wild.iter().all(|c| (0.0..=1.5).contains(c)));
    }

    #[test]
    fn water_column_absorbs_red_first() {
        let shallow = water_column_transmittance(2.0, DEFAULT_WATER_TINT_RGB);
        let deep = water_column_transmittance(16.0, DEFAULT_WATER_TINT_RGB);
        // Beer-Lambert ordering: blue survives, red dies first.
        assert!(deep[2] > deep[1] && deep[1] > deep[0]);
        assert!(shallow.iter().zip(deep.iter()).all(|(s, d)| s > d));
        // Depth 0 returns the raw tint.
        let surface = water_column_transmittance(0.0, DEFAULT_WATER_TINT_RGB);
        assert_eq!(surface, DEFAULT_WATER_TINT_RGB);
        for v in deep {
            assert!(v.is_finite() && v >= 0.0);
        }
    }

    #[test]
    fn underwater_params_derive_from_surface_optics() {
        let surface = AtmosphereParams::default();
        let under = surface.underwater_from(DEFAULT_WATER_TINT_RGB, 10.0);
        // Teal fog denser than air haze, dimmer sun, bounded channels.
        assert!(under.fog_density > surface.fog_density);
        assert!(under.sun_intensity < surface.sun_intensity);
        assert!(under.fog_rgb[2] > under.fog_rgb[0]);
        assert!(under.depth_tint_strength > 0.3);
        for c in under
            .sky_zenith_rgb
            .iter()
            .chain(under.fog_rgb.iter())
            .chain(under.depth_tint_rgb.iter())
        {
            assert!((0.0..=4.0).contains(c));
        }
        // Deeper water: stronger tint, more fog, less sun.
        let deeper = surface.underwater_from(DEFAULT_WATER_TINT_RGB, 24.0);
        assert!(deeper.sun_intensity < under.sun_intensity);
        assert!(deeper.fog_density >= under.fog_density - 1e-6);
        // The zero-arg preset stays a harmonized teal.
        let preset = AtmosphereParams::underwater();
        assert!(preset.fog_rgb[1] > preset.fog_rgb[0]);
        assert!(preset.fog_rgb[2] >= preset.fog_rgb[0]);
    }

    #[test]
    fn blend_water_transition_glides_without_snapping() {
        let above = AtmosphereParams::default();
        let under = above.underwater_from(DEFAULT_WATER_TINT_RGB, 12.0);
        let close = |p: &AtmosphereParams, q: &AtmosphereParams| {
            p.sky_zenith_rgb
                .iter()
                .chain(p.sky_horizon_rgb.iter())
                .chain(p.fog_rgb.iter())
                .chain(p.depth_tint_rgb.iter())
                .chain(p.ambient_rgb.iter())
                .zip(
                    q.sky_zenith_rgb
                        .iter()
                        .chain(q.sky_horizon_rgb.iter())
                        .chain(q.fog_rgb.iter())
                        .chain(q.depth_tint_rgb.iter())
                        .chain(q.ambient_rgb.iter()),
                )
                .all(|(a, b)| (a - b).abs() < 1e-5)
                && (p.fog_density - q.fog_density).abs() < 1e-6
                && (p.depth_tint_strength - q.depth_tint_strength).abs() < 1e-6
                && (p.sun_intensity - q.sun_intensity).abs() < 1e-6
        };
        let dry = blend_water_transition(&above, &under, 0.0);
        let wet = blend_water_transition(&above, &under, 1.0);
        assert!(close(&dry, &above.sanitized()));
        assert!(close(&wet, &under.sanitized()));
        // Midway sits strictly between on a monotone channel.
        let mid = blend_water_transition(&above, &under, 0.5);
        assert!(mid.fog_density > above.fog_density && mid.fog_density < under.fog_density);
        // Smoothstep keeps the centre off the linear midpoint.
        let linear = (above.fog_density + under.fog_density) * 0.5;
        assert!((mid.fog_density - linear).abs() < 0.05);
        // Clamped inputs and hostile values stay finite.
        let over = blend_water_transition(&above, &under, 2.0);
        assert!(close(&over, &under.sanitized()));
        let nan = blend_water_transition(&above, &under, f32::NAN);
        assert!(close(&nan, &above.sanitized()));
    }

    #[test]
    fn with_time_of_day_tints_sky_and_dims_storm_sun() {
        let noonish = AtmosphereParams::default().with_time_of_day(6_000, 0.0);
        // Neutral temperature barely moves the noon palette.
        assert!((noonish.sky_zenith_rgb[0] - 0.30).abs() < 0.06);
        let dusk = AtmosphereParams::default().with_time_of_day(12_000, 0.0);
        assert!(dusk.sky_horizon_rgb[0] > dusk.sky_horizon_rgb[2],
            "dusk horizon must lean warm");
        let storm = AtmosphereParams::default().with_time_of_day(6_000, 1.0);
        assert!(storm.sun_intensity < AtmosphereParams::default().sun_intensity,
            "storm dims the sun through the tint luminance");
    }

    #[test]
    fn cloud_horizon_fade_melts_between_start_and_end() {
        assert_eq!(cloud_horizon_fade(10.0, 48.0, 78.0), 1.0);
        assert_eq!(cloud_horizon_fade(90.0, 48.0, 78.0), 0.0);
        let mid = cloud_horizon_fade(63.0, 48.0, 78.0);
        assert!((0.0..1.0).contains(&mid) && (mid - 0.5).abs() < 1e-4);
        // Monotone decreasing across the band.
        let mut prev = 1.0f32;
        for i in 0..=20 {
            let f = cloud_horizon_fade(48.0 + i as f32 * 1.5, 48.0, 78.0);
            assert!(f <= prev + 1e-6);
            prev = f;
        }
        // Degenerate range: hard cutoff, still bounded.
        assert_eq!(cloud_horizon_fade(50.0, 48.0, 48.0), 0.0);
        // Hostile inputs fail closed into a bounded 0..=1.
        let nan = cloud_horizon_fade(f32::NAN, f32::NAN, f32::NAN);
        assert!((0.0..=1.0).contains(&nan));
        assert_eq!(cloud_horizon_fade(f32::NAN, 10.0, 100.0), 1.0);
    }

    #[test]
    fn styled_cloud_geometry_batches_by_deck_and_melts_at_horizon() {
        let cells = generate_layered_voxel_clouds(&cloud_cfg(3), 0.0, 0.0, 6_000);
        assert!(!cells.is_empty());
        let lighting = compute_cloud_lighting(0.8, 0.0, 0.0);

        let style = CloudGeometryStyle {
            layers: 3,
            viewer_xz: [0.0, 0.0],
            horizon_fade_start: 10.0,
            horizon_fade_end: 80.0,
        };
        let batches = voxel_clouds_to_geometry_styled(&cells, &lighting, &style);
        assert!(!batches.is_empty());
        // Every batch carries valid alpha and indices in range.
        for b in &batches {
            assert!((0.0..=1.0).contains(&b.alpha) && b.alpha > 0.0);
            assert!(!b.vertices.is_empty() && !b.indices.is_empty());
            let vmax = b.vertices.len() as u32;
            assert!(b.indices.iter().all(|&i| i < vmax));
            assert!(b.fade_band >= 1 && b.fade_band <= 4);
            // Albedo stays in the shaded 0..=1 range even with the lining.
            assert!(b.vertices.iter().all(|v| v.albedo().iter().all(|c| (0.0..=1.0).contains(c))));
        }
        // Deck translucency: a cumulus batch out-alphas a cirrus batch when
        // both exist at full band.
        let cum_alpha = batches.iter().filter(|b| b.layer == 0).map(|b| b.alpha).fold(0.0f32, f32::max);
        if let Some(cir) = batches.iter().filter(|b| b.layer == 2).map(|b| b.alpha).reduce(f32::max) {
            assert!(cum_alpha > cir, "cumulus must stay more opaque than cirrus");
        }

        // A viewer very far away melts everything past the fade end.
        let far = CloudGeometryStyle {
            viewer_xz: [10_000.0, 0.0],
            ..style
        };
        assert!(voxel_clouds_to_geometry_styled(&cells, &lighting, &far).is_empty());

        // With a huge fade window nothing melts: the crisp band reproduces
        // the classic geometry quad count.
        let crisp = CloudGeometryStyle {
            horizon_fade_start: 1.0e6,
            horizon_fade_end: 2.0e6,
            ..style
        };
        let crisp_batches = voxel_clouds_to_geometry_styled(&cells, &lighting, &crisp);
        let styled_quads: usize = crisp_batches.iter().map(|b| b.indices.len() / 6).sum();
        let (_, classic_idx) = voxel_clouds_to_geometry(&cells, &lighting);
        assert_eq!(styled_quads, classic_idx.len() / 6);
    }

    /// Nether-ish destination palette for the crossfade tests: crimson
    /// haze, dim ambient — far enough from [`AtmosphereParams::default`]
    /// that a wrong blend endpoint or t direction is immediately visible.
    fn nether_atmosphere() -> AtmosphereParams {
        AtmosphereParams {
            sky_zenith_rgb: [0.35, 0.05, 0.04],
            sky_horizon_rgb: [0.55, 0.10, 0.06],
            fog_rgb: [0.55, 0.10, 0.06],
            fog_density: 0.05,
            depth_tint_rgb: [0.9, 0.4, 0.3],
            depth_tint_strength: 0.2,
            ambient_rgb: [0.30, 0.12, 0.10],
            sun_intensity: 0.4,
        }
    }

    #[test]
    fn blend_dimension_crossfade_hits_exact_endpoints() {
        let from = AtmosphereParams::default();
        let to = nether_atmosphere();
        for progress in [f32::NEG_INFINITY, -1.0, -0.0, 0.0] {
            assert_eq!(
                blend_dimension_crossfade(&from, &to, progress),
                from,
                "progress {progress} must pin `from`"
            );
        }
        for progress in [1.0, 1.5, f32::INFINITY] {
            assert_eq!(
                blend_dimension_crossfade(&from, &to, progress),
                to,
                "progress {progress} must pin `to`"
            );
        }
    }

    #[test]
    fn blend_dimension_crossfade_midpoint_is_strictly_intermediate_and_finite() {
        let from = AtmosphereParams::default();
        let to = nether_atmosphere();
        let mid = blend_dimension_crossfade(&from, &to, 0.5);
        // Smoothstep(0.5) == 0.5 exactly, so each channel sits at the
        // arithmetic midpoint; strict bounds + finiteness still asserted.
        let channels = |p: &AtmosphereParams| {
            [
                p.sky_zenith_rgb[0],
                p.sky_zenith_rgb[1],
                p.sky_zenith_rgb[2],
                p.sky_horizon_rgb[0],
                p.sky_horizon_rgb[1],
                p.sky_horizon_rgb[2],
                p.fog_rgb[0],
                p.fog_rgb[1],
                p.fog_rgb[2],
                p.fog_density,
                p.depth_tint_rgb[0],
                p.depth_tint_rgb[1],
                p.depth_tint_rgb[2],
                p.depth_tint_strength,
                p.ambient_rgb[0],
                p.ambient_rgb[1],
                p.ambient_rgb[2],
                p.sun_intensity,
            ]
        };
        let (a, b, m) = (channels(&from), channels(&to), channels(&mid));
        for i in 0..a.len() {
            assert!(m[i].is_finite(), "mid channel {i} must be finite");
            let (lo, hi) = (a[i].min(b[i]), a[i].max(b[i]));
            assert!(
                m[i] > lo && m[i] < hi,
                "channel {i} must sit strictly between {} and {}, got {}",
                a[i],
                b[i],
                m[i]
            );
            let expected = (a[i] + b[i]) * 0.5;
            assert!(
                (m[i] - expected).abs() < 1e-6,
                "smoothstep(0.5)=0.5 ⇒ midpoint channel {i}: {} vs {expected}",
                m[i]
            );
        }
    }

    #[test]
    fn blend_dimension_crossfade_stays_finite_on_hostile_input() {
        let from = AtmosphereParams::default();
        let to = nether_atmosphere();
        for progress in [f32::NAN, -f32::NAN, f32::EPSILON, 0.999_999] {
            let blended = blend_dimension_crossfade(&from, &to, progress);
            let p = blended.sanitized();
            assert_eq!(blended, p, "blend output must already be sanitized");
        }
    }
}
