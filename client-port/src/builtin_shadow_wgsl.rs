// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later

//! Optional, clean-room Megu Enhanced shaders and deterministic CPU math.
//! The default is Off, and this module never allocates or submits GPU work.

use crate::gpu_pipeline::{GpuCapabilities, GpuVendor};
use crate::vfx_engine::DynamicColoredLight;
use std::f32::consts::{PI, TAU};

/// Ordering is a capability/cost rank for `negotiate_builtin_shadow_quality`:
/// `Feather` sits below `Low` so it never negotiates up to a shadow-map preset,
/// while `Natural` sits between `Low` and `Medium`. The serialized `index()`
/// values of `Off`/`Low`/`Medium`/`High` (0, 1, 2, 3) are unchanged; the two
/// new presets append at 4 and 5 for backward compatibility.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, PartialOrd, Ord)]
pub enum BuiltinShadowQuality {
    #[default]
    Off,
    /// Zero shadow-map cascades and zero shadow draw calls: purely analytical
    /// block-face directional shading, hemisphere sky/ground bounce, crisp
    /// natural water, horizon aerial fog, and VFX1 dynamic colored lights.
    Feather,
    Low,
    /// Recommended Vanilla+ preset: block-faithful soft PCF shadows, voxel
    /// cloud shading, natural clear water, hemisphere bounce, dynamic lights,
    /// subtle vanilla-faithful pixel stars, and zero camera post-FX gimmicks.
    Natural,
    Medium,
    High,
}

impl BuiltinShadowQuality {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Feather => "feather",
            Self::Low => "low",
            Self::Natural => "natural",
            Self::Medium => "medium",
            Self::High => "high",
        }
    }

    pub const fn index(self) -> u32 {
        match self {
            Self::Off => 0,
            Self::Low => 1,
            Self::Medium => 2,
            Self::High => 3,
            Self::Feather => 4,
            Self::Natural => 5,
        }
    }

    pub const fn config(self) -> CascadeConfig {
        match self {
            Self::Off => CascadeConfig::new(self, 0, 0, 0, 0, 0, false),
            Self::Feather => CascadeConfig::new(self, 0, 0, 0, 16, 2, false),
            Self::Low => CascadeConfig::new(self, 1, 1024, 4, 8, 1, false),
            Self::Natural => CascadeConfig::new(self, 2, 1536, 9, 32, 2, false),
            Self::Medium => CascadeConfig::new(self, 2, 2048, 9, 32, 2, false),
            Self::High => CascadeConfig::new(self, 3, 2048, 16, 64, 3, true),
        }
    }
}

/// Conservative quality ceiling derived from existing GPU capabilities.
/// Device memory and texture-array limits are not present in these flags.
/// The GPU resource allocator must also validate those limits and fall back.
pub fn negotiate_builtin_shadow_quality(
    requested: BuiltinShadowQuality,
    caps: &GpuCapabilities,
) -> BuiltinShadowQuality {
    if requested == BuiltinShadowQuality::Off {
        return BuiltinShadowQuality::Off;
    }
    let supported = match caps.vendor {
        GpuVendor::Nvidia | GpuVendor::Amd if caps.mesh_shader_adequate() => {
            BuiltinShadowQuality::High
        }
        GpuVendor::Nvidia | GpuVendor::Amd | GpuVendor::Apple if caps.supports_compute_shaders => {
            BuiltinShadowQuality::Medium
        }
        GpuVendor::Intel => BuiltinShadowQuality::Low,
        GpuVendor::Other if !caps.supports_compute_shaders => BuiltinShadowQuality::Off,
        _ => BuiltinShadowQuality::Low,
    };
    requested.min(supported)
}

/// Per-preset static budgets. A 32-bit depth array is assumed.
/// `cascade_count == 0` (Off and Feather) produces no shadow map or draw call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CascadeConfig {
    pub quality: BuiltinShadowQuality,
    pub cascade_count: u32,
    pub map_resolution: u32,
    pub pcf_taps: u32,
    pub max_dynamic_lights: u32,
    /// Feature tier written to `render_params.x`: PCF kernel selection,
    /// water/sky detail, and the dynamic-light cap. Matches `index()` for
    /// Off/Low/Medium/High; Feather and Natural reuse established tiers.
    pub shader_tier: u32,
    /// Cinematic post effects (tonemap grade, sun-shaft hint). Always false
    /// for the Vanilla+ presets: no lens flare, motion blur, DoF, or bloom.
    pub camera_post_fx: bool,
}

impl CascadeConfig {
    const fn new(
        quality: BuiltinShadowQuality,
        cascade_count: u32,
        map_resolution: u32,
        pcf_taps: u32,
        max_dynamic_lights: u32,
        shader_tier: u32,
        camera_post_fx: bool,
    ) -> Self {
        Self {
            quality,
            cascade_count,
            map_resolution,
            pcf_taps,
            max_dynamic_lights,
            shader_tier,
            camera_post_fx,
        }
    }

    pub const fn depth_array_bytes(self) -> u64 {
        self.cascade_count as u64 * self.map_resolution as u64 * self.map_resolution as u64 * 4
    }
}

/// Practical logarithmic-linear splits for 1 to 3 cascades.
/// Invalid clip planes and a disabled preset produce an empty vector.
pub fn compute_cascade_splits(near: f32, far: f32, count: u32, lambda: f32) -> Vec<f32> {
    if !near.is_finite() || !far.is_finite() || near <= 0.0 || far <= near || count == 0 {
        return Vec::new();
    }
    let blend = if lambda.is_finite() {
        lambda.clamp(0.0, 1.0)
    } else {
        0.5
    };
    let count = count.min(3);
    let mut splits = Vec::with_capacity(count as usize);
    for i in 1..=count {
        let t = i as f32 / count as f32;
        let logarithmic = ((1.0 - t) * near.ln() + t * far.ln()).exp();
        let linear = near * (1.0 - t) + far * t;
        splits.push((blend * logarithmic + (1.0 - blend) * linear).clamp(near, far));
    }
    if let Some(end) = splits.last_mut() {
        *end = far;
    }
    splits
}

/// A normalized sun vector, with noon at tick 6000 and midnight at 18000.
/// The moon is visible along the opposite ray.
pub fn compute_sun_direction(world_time_ticks: u64) -> [f32; 3] {
    let phase = (world_time_ticks % 24_000) as f32 * (TAU / 24_000.0);
    [phase.cos(), phase.sin(), 0.0]
}

/// Deterministic In'ei (layered shadow) balance, mirrored in terrain WGSL.
/// Visibility is the shadow-map fraction and elevation is the sun's Y component.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct IneiShadowBalance {
    pub sky_ambient_scale: f32,
    pub warm_bounce_scale: f32,
    pub shadow_desaturation: f32,
    pub lantern_presence_boost: f32,
}

pub fn compute_inei_shadow_balance(visibility: f32, sun_elevation: f32) -> IneiShadowBalance {
    let visibility = if visibility.is_finite() {
        visibility.clamp(0.0, 1.0)
    } else {
        0.0
    };
    let daylight = if sun_elevation.is_finite() {
        (sun_elevation * 1.5).clamp(0.0, 1.0)
    } else {
        0.0
    };
    IneiShadowBalance {
        sky_ambient_scale: 0.42 + 0.58 * visibility,
        warm_bounce_scale: 1.15 - 0.15 * visibility,
        shadow_desaturation: (1.0 - visibility) * 0.22,
        lantern_presence_boost: 1.38 - 0.38 * visibility * daylight,
    }
}

/// Yūgen: distance mist gathers in low valleys and thickens at dawn and dusk.
/// The same height and phase curves are used by the terrain shader.
pub fn compute_yugen_valley_mist(
    world_y: f32,
    camera_y: f32,
    view_distance: f32,
    base_fog_density: f32,
    day_phase: f32,
) -> f32 {
    let world_y = if world_y.is_finite() { world_y } else { 96.0 };
    let camera_y = if camera_y.is_finite() { camera_y } else { 96.0 };
    let distance = if view_distance.is_finite() {
        view_distance.max(0.0)
    } else {
        0.0
    };
    let base_density = if base_fog_density.is_finite() {
        base_fog_density.max(0.0)
    } else {
        0.0
    };
    if distance <= 0.0 || base_density <= 0.0 {
        return 0.0;
    }
    let phase = if day_phase.is_finite() {
        day_phase.rem_euclid(1.0)
    } else {
        0.25
    };
    let valley = 1.0 - smooth01((world_y - 63.0) / 33.0);
    let camera_proximity = 1.0 - smooth01(((camera_y - world_y).abs() / 128.0).min(1.0));
    let twilight = 1.0 - smooth01((phase * TAU).sin().abs() / 0.28);
    let density = base_density
        * (0.32 + valley * (0.78 + 0.22 * camera_proximity) * (0.80 + 0.50 * twilight));
    (1.0 - (-distance * density).exp()).clamp(0.0, 0.92)
}

/// Komorebi: two smooth, world-block-scaled waves suggest moving leaf shade.
/// The caller applies the factor only in penumbra or the Feather tier.
pub fn compute_komorebi_dapple(world_pos: [f32; 3], day_phase: f32, sun_elevation: f32) -> f32 {
    if !world_pos[0].is_finite()
        || !world_pos[2].is_finite()
        || !day_phase.is_finite()
        || !sun_elevation.is_finite()
        || sun_elevation <= 0.08
    {
        return 1.0;
    }
    let drift = (day_phase.rem_euclid(1.0) * TAU).sin() * 0.35;
    let x = world_pos[0];
    let z = world_pos[2];
    let broad = (x * 0.31 + z * 0.21 + drift).sin() * (z * 0.27 - x * 0.17 - drift * 0.7).sin();
    let fine =
        (x * 0.62 - z * 0.42 + drift * 1.7).sin() * (z * 0.54 + x * 0.34 - drift * 1.3).sin();
    let daylight = smooth01((sun_elevation - 0.08) / 0.12);
    1.0 + daylight * (-0.03 + 0.11 * (0.72 * broad + 0.28 * fine))
}

/// Nure-iro: porous surfaces darken slightly when wet, with a grazing sky sheen.
/// Moisture is normalized to 0..=1 and does not depend on wall-clock time.
pub fn compute_nure_iro_patina(
    albedo: [f32; 3],
    normal_y: f32,
    wetness: f32,
    view_dot_normal: f32,
) -> ([f32; 3], f32) {
    let wetness = if wetness.is_finite() {
        wetness.clamp(0.0, 1.0)
    } else {
        0.0
    };
    if wetness <= 0.0 {
        return (albedo, 0.0);
    }
    let albedo = albedo.map(|channel| {
        if channel.is_finite() {
            channel.clamp(0.0, 1.0)
        } else {
            0.0
        }
    });
    let luma = albedo[0] * 0.2126 + albedo[1] * 0.7152 + albedo[2] * 0.0722;
    let scale = 1.0 - 0.18 * wetness;
    let rich_albedo = albedo.map(|channel| {
        (channel * (1.0 + (channel - luma) * 0.08 * wetness) * scale).clamp(0.0, 1.0)
    });
    let normal_y = if normal_y.is_finite() {
        normal_y.clamp(0.0, 1.0)
    } else {
        0.0
    };
    let facing = if view_dot_normal.is_finite() {
        view_dot_normal.clamp(0.0, 1.0)
    } else {
        1.0
    };
    let sheen = (1.0 - facing).powi(4) * 0.12 * wetness * normal_y;
    (rich_albedo, sheen)
}

/// Rain wetness is restricted to upward faces with a nearly full sky view.
/// `sky_exposure` and `rain` are normalized to `0..=1` independently of the
/// packed vanilla skylight range.
pub fn compute_sky_rain_wetness(normal_y: f32, sky_exposure: f32, rain: f32) -> f32 {
    let normal_y = if normal_y.is_finite() {
        normal_y.clamp(-1.0, 1.0)
    } else {
        -1.0
    };
    let sky = if sky_exposure.is_finite() {
        sky_exposure.clamp(0.0, 1.0)
    } else {
        0.0
    };
    let rain = if rain.is_finite() {
        rain.clamp(0.0, 1.0)
    } else {
        0.0
    };
    smooth01((normal_y - 0.7) / 0.3) * smooth01((sky - 0.85) / 0.15) * rain
}

/// Two-octave drifting cloud attenuation mirrored by the terrain WGSL.
/// Closed interiors return neutral light. The explicit coverage parameter is
/// available to callers that have explicit weather data; the live shader
/// scales its procedural cloud coverage with the rain lane in the shadow UBO.
pub fn compute_cloud_shadow_attenuation(
    world_xz: [f32; 2],
    time: f32,
    sky_exposure: f32,
    coverage: f32,
) -> f32 {
    if !world_xz.iter().all(|v| v.is_finite()) || !time.is_finite() {
        return 1.0;
    }
    let sky = if sky_exposure.is_finite() {
        smooth01((sky_exposure.clamp(0.0, 1.0) - 0.72) / 0.28)
    } else {
        0.0
    };
    let coverage = if coverage.is_finite() {
        coverage.clamp(0.0, 1.0)
    } else {
        0.0
    };
    if sky <= 0.0 || coverage <= 0.0 {
        return 1.0;
    }
    let drift = time * 0.013;
    let p0 = [
        world_xz[0] * 0.018 + drift * 0.72,
        world_xz[1] * 0.018 + drift * 0.31,
    ];
    let broad = (p0[0] * 1.31 + p0[1] * 0.73).sin()
        * (p0[0] * -0.49 + p0[1] * 1.17).cos();
    let p1 = [
        world_xz[0] * 0.044 - drift * 0.53,
        world_xz[1] * 0.044 + drift * 0.37,
    ];
    let fine = (p1[0] * 2.47 - p1[1] * 1.81).sin()
        * (p1[0] * 1.63 + p1[1] * 2.09).cos();
    let field = 0.68 * broad + 0.32 * fine;
    let cloud = smooth01((field + 0.20) / 0.82);
    (1.0 - 0.65 * sky * cloud * coverage).clamp(0.35, 1.0)
}

/// Animated concentric ring energy for rain ripples, gated by open-sky
/// wetness. Mirrors the inexpensive procedural pattern in the terrain WGSL.
pub fn compute_sky_rain_ripple(
    world_xz: [f32; 2],
    time: f32,
    normal_y: f32,
    sky_exposure: f32,
    rain: f32,
) -> f32 {
    if !world_xz.iter().all(|v| v.is_finite()) || !time.is_finite() {
        return 0.0;
    }
    let wetness = compute_sky_rain_wetness(normal_y, sky_exposure, rain);
    if wetness <= 0.0 {
        return 0.0;
    }
    let p = [
        world_xz[0] * 0.72 + time * 0.031,
        world_xz[1] * 0.72 - time * 0.021,
    ];
    let cell = [p[0].floor(), p[1].floor()];
    let seed = fract((cell[0] * 17.13 + cell[1] * 91.77).sin() * 31313.7);
    let center = [fract(seed * 17.0), fract(seed * 71.0)];
    let local = [fract(p[0]) - center[0], fract(p[1]) - center[1]];
    let age = fract(time * 0.05 + seed);
    let radius = age * 0.58;
    let distance = (local[0] * local[0] + local[1] * local[1]).sqrt();
    let ring = 1.0 - smooth01(((distance - radius).abs() - 0.025) / 0.045);
    wetness * ring * (1.0 - age)
}

fn fract(value: f32) -> f32 {
    value - value.floor()
}

#[derive(Clone, Copy, Debug)]
pub struct ShadowCamera {
    pub position: [f32; 3],
    pub forward: [f32; 3],
    pub up: [f32; 3],
    pub fov_y_radians: f32,
    pub aspect_ratio: f32,
    pub near: f32,
    pub far: f32,
}

impl Default for ShadowCamera {
    fn default() -> Self {
        Self {
            position: [0.0; 3],
            forward: [0.0, 0.0, 1.0],
            up: [0.0, 1.0, 0.0],
            fov_y_radians: PI / 3.0,
            aspect_ratio: 16.0 / 9.0,
            near: 0.1,
            far: 192.0,
        }
    }
}

/// Column-major matrices and vec4 fields correspond exactly to WGSL layout.
#[repr(C, align(16))]
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ShadowUniforms {
    pub light_view_proj: [[[f32; 4]; 4]; 3],
    pub camera_view_proj: [[f32; 4]; 4],
    pub cascade_splits: [f32; 4],
    /// xyz = direction toward sun; w = live normalized rain wetness.
    pub sun_direction: [f32; 4],
    pub sun_color: [f32; 4],
    pub moon_color: [f32; 4],
    pub camera_position: [f32; 4],
    pub camera_forward: [f32; 4],
    pub camera_right: [f32; 4],
    pub camera_up: [f32; 4],
    /// Depth-map resolution, cascade count, PCF samples, active depth layer.
    pub shadow_params: [f32; 4],
    /// Shader feature tier, tick-based day phase, fog density, exposure.
    pub render_params: [f32; 4],
    /// Star visibility, eclipse corona boost, lunar shadow strength, eclipse dim.
    pub celestial_params: [f32; 4],
}

impl ShadowUniforms {
    pub fn with_active_cascade(mut self, layer: u32) -> Self {
        let count = self.shadow_params[1] as u32;
        self.shadow_params[3] = layer.min(count.saturating_sub(1)) as f32;
        self
    }
}

/// The terrain shader's std140-compatible 32-byte point-light record.
#[repr(C, align(16))]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct GpuPointLight {
    pub position_radius: [f32; 4],
    pub color_intensity: [f32; 4],
}

/// The terrain shader's fixed 64-light uniform buffer.
#[repr(C, align(16))]
#[derive(Clone, Debug, PartialEq)]
pub struct VfxLightUniforms {
    pub light_count: [u32; 4],
    pub lights: [GpuPointLight; 64],
}

impl Default for VfxLightUniforms {
    fn default() -> Self {
        Self {
            light_count: [0; 4],
            lights: [GpuPointLight::default(); 64],
        }
    }
}

impl VfxLightUniforms {
    /// Stamp a [`compute_region_light_mask`] result into `light_count[1..=2]`
    /// (lo/hi u32) so the WGSL walk visits only lights intersecting the
    /// meshed region. The fixed binding layout is unchanged: `yz` was
    /// previously always zero, and a zero `yz` is the shader's "no culling"
    /// fallback, so packing an empty mask is a perf no-op, never a
    /// correctness change.
    pub fn with_region_light_mask(mut self, mask: u64) -> Self {
        self.light_count[1] = mask as u32;
        self.light_count[2] = (mask >> 32) as u32;
        self
    }
}

/// Packs already-validated VFX1 dynamic colored lights into the WGSL binding.
/// Invalid external values are skipped, and the selected quality caps uploads.
pub fn pack_vfx1_lights(
    source: &[DynamicColoredLight],
    quality: BuiltinShadowQuality,
) -> VfxLightUniforms {
    let mut result = VfxLightUniforms::default();
    let limit = quality.config().max_dynamic_lights as usize;
    for light in source {
        if result.light_count[0] as usize == limit {
            break;
        }
        if !light
            .pos
            .iter()
            .chain(light.rgb.iter())
            .all(|v| v.is_finite())
            || !light.radius.is_finite()
            || light.radius <= 0.0
            || !light.intensity.is_finite()
            || light.intensity < 0.0
        {
            continue;
        }
        let index = result.light_count[0] as usize;
        result.lights[index] = GpuPointLight {
            position_radius: [light.pos[0], light.pos[1], light.pos[2], light.radius],
            color_intensity: [light.rgb[0], light.rgb[1], light.rgb[2], light.intensity],
        };
        result.light_count[0] += 1;
    }
    result
}

/// Region→light culling bit mask for the terrain shader's u64 capability
/// mask. Bit `i` is set exactly when the `i`-th packed light's bounding
/// sphere (`position_radius.xyz`, `.w` radius) intersects the
/// `region_aabb_min`/`region_aabb_max` AABB. The mask is meant to be packed
/// into `VfxLightUniforms::light_count[1..=2]` (lo/hi u32) so the WGSL
/// `light_mask_take`/`countTrailingZeros` walk touches only intersecting
/// lights. At most 64 lights are considered; non-finite or inverted bounds
/// and non-finite/negative radii fail closed to that light's bit cleared.
pub fn compute_region_light_mask(
    region_aabb_min: [f32; 3],
    region_aabb_max: [f32; 3],
    lights: &[GpuPointLight],
) -> u64 {
    if !region_aabb_min
        .iter()
        .chain(region_aabb_max.iter())
        .all(|v| v.is_finite())
    {
        return 0;
    }
    let mut mask = 0u64;
    for (index, light) in lights.iter().take(64).enumerate() {
        let pos = light.position_radius;
        let radius = pos[3];
        if !radius.is_finite() || radius < 0.0 {
            continue;
        }
        let mut distance_sq = 0.0f32;
        for axis in 0..3 {
            let lo = region_aabb_min[axis].min(region_aabb_max[axis]);
            let hi = region_aabb_min[axis].max(region_aabb_max[axis]);
            let delta = pos[axis] - pos[axis].clamp(lo, hi);
            distance_sq += delta * delta;
        }
        if distance_sq.is_finite() && distance_sq <= radius * radius {
            mask |= 1u64 << index;
        }
    }
    mask
}

fn dot(a: [f32; 3], b: [f32; 3]) -> f32 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

fn cross(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}

fn add(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [a[0] + b[0], a[1] + b[1], a[2] + b[2]]
}

fn scale(a: [f32; 3], amount: f32) -> [f32; 3] {
    [a[0] * amount, a[1] * amount, a[2] * amount]
}

fn normalize(a: [f32; 3], fallback: [f32; 3]) -> [f32; 3] {
    let square = dot(a, a);
    if square.is_finite() && square > 1.0e-12 {
        scale(a, square.sqrt().recip())
    } else {
        fallback
    }
}

fn smooth01(t: f32) -> f32 {
    let t = if t.is_finite() {
        t.clamp(0.0, 1.0)
    } else {
        0.0
    };
    t * t * (3.0 - 2.0 * t)
}

/// Opt-in eclipse lighting. `None` preserves the 100% vanilla sun/moon orbit.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub enum EclipseMode {
    /// Vanilla orbit: eclipses never occur.
    #[default]
    None,
    /// The moon occludes the sun. `coverage` is 0 (no eclipse) to 1 (total).
    SolarEclipse { coverage: f32 },
    /// The moon passes through Earth's shadow. `progress` is 0 to 1 over the
    /// event; mid-progress is totality (the blood-moon tint).
    TotalLunarEclipse { progress: f32 },
}

/// CPU-side celestial light state shared by every builtin pass.
/// All fields are finite multipliers/tints in the 0.0..=1.0 visual range.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CelestialLightingState {
    /// Daylight multiplier (1.0 normally; dips during a solar eclipse).
    pub sun_illuminance: f32,
    /// Cool-silver moonlight tint; shifts copper during a lunar eclipse.
    pub moon_color: [f32; 3],
    /// Moonlight strength from the 8-phase lunar cycle (1.0 full, ~0 new).
    pub moon_illuminance: f32,
    /// How strongly the shared shadow map darkens moonlit surfaces.
    pub lunar_shadow_strength: f32,
    /// Extra PCF softening while a solar eclipse dims the disc.
    pub shadow_softness: f32,
    /// Solar-eclipse corona ring intensity, peaking at totality.
    pub corona_boost: f32,
    /// Crisp pixel-star intensity: 0 by day, rising at night and during a
    /// total solar eclipse.
    pub star_visibility: f32,
    /// Overall eclipse dimming of sky and ambient light (0..=1).
    pub eclipse_dim: f32,
}

/// Vanilla 8-phase lunar contrast shared by the CPU uniforms and the WGSL
/// lunar terms: `moon_phase` wraps modulo 8 like the vanilla
/// `level.getMoonPhase()`/`day % 8` cycle — phase 0 (full moon) returns
/// `1.0`, phase 4 (new moon) returns near-zero, so a full moon keeps crisp
/// silver-blue moon shadows while a new moon leaves only starlight and
/// torches.
pub fn moon_phase_intensity(moon_phase: u8) -> f32 {
    const PHASE_LIGHT: [f32; 8] = [1.0, 0.78, 0.55, 0.32, 0.06, 0.32, 0.55, 0.78];
    PHASE_LIGHT[(moon_phase % 8) as usize]
}

/// Vanilla 8-phase lunar cycle: `moon_phase` wraps modulo 8 like the vanilla
/// `level.getMoonPhase()` (0 = full moon, 4 = new moon).
pub fn compute_celestial_lighting(
    world_time_ticks: u64,
    moon_phase: u8,
    eclipse: EclipseMode,
) -> CelestialLightingState {
    let phase_strength = moon_phase_intensity(moon_phase);
    let sun = compute_sun_direction(world_time_ticks);
    let mut state = CelestialLightingState {
        sun_illuminance: 1.0,
        moon_color: [0.62, 0.70, 0.92],
        moon_illuminance: phase_strength,
        lunar_shadow_strength: 0.55 * phase_strength,
        shadow_softness: 0.0,
        corona_boost: 0.0,
        star_visibility: smooth01((0.06 - sun[1]) / 0.24),
        eclipse_dim: 0.0,
    };
    match eclipse {
        EclipseMode::None => {}
        EclipseMode::SolarEclipse { coverage } => {
            let occlusion = smooth01((coverage.clamp(0.0, 1.0) - 0.5) / 0.5);
            state.sun_illuminance = 1.0 - 0.94 * occlusion;
            state.shadow_softness = occlusion;
            state.corona_boost = smooth01((coverage.clamp(0.0, 1.0) - 0.9) / 0.1);
            state.eclipse_dim = occlusion * 0.75;
            state.star_visibility = state.star_visibility.max(occlusion * 0.85);
        }
        EclipseMode::TotalLunarEclipse { progress } => {
            let p = progress.clamp(0.0, 1.0);
            let copper = smooth01(p);
            state.moon_color = [
                0.62 + (0.85 - 0.62) * copper,
                0.70 + (0.30 - 0.70) * copper,
                0.92 + (0.13 - 0.92) * copper,
            ];
            state.moon_illuminance *= 1.0 - 0.55 * copper;
            state.lunar_shadow_strength *= 1.0 - 0.5 * copper;
        }
    }
    state
}

/// Deep-cave floor ambient mirrored by the `MEGU_TERRAIN_LIT_WGSL` ambient
/// floor: a sealed-off space (`sky_light == 0`) rests just above a crushed
/// black, so torch and moon light read as real pools of colour.
pub const INEI_DEEP_CAVE_AMBIENT: [f32; 3] = [0.008, 0.009, 0.014];

/// Non-linear sky-light falloff `pow(sky_light / 15, 1.8)` over the vanilla
/// `0..=15` range. The WGSL `sky_light_factor` mirrors the same curve and
/// gates sky ambient plus the cave-entrance indirect bounce.
pub fn compute_sky_light_factor(sky_light: f32) -> f32 {
    let n = if sky_light.is_finite() {
        (sky_light / 15.0).clamp(0.0, 1.0)
    } else {
        0.0
    };
    if n <= 0.0 {
        0.0
    } else {
        n.powf(1.8)
    }
}

/// Strict underground direct-light gate `smoothstep(0, 2, sky_light)`.
/// A sealed cave (`sky_light == 0`) returns exactly `0.0`, so sun and moon
/// direct light can never bleed through rock even where the shadow map or
/// a missing cascade still reports `visibility == 1.0`.
pub fn compute_direct_light_gate(sky_light: f32) -> f32 {
    if !sky_light.is_finite() {
        return 0.0;
    }
    smooth01(sky_light / 2.0)
}

/// Warm blackbody-style block-light temperature, mirrored by the WGSL
/// `color_temp`: incandescent gold beside a torch or lantern, deepening to
/// amber-orange as the light level falls off.
pub fn compute_block_light_color_temp(block_light: f32) -> [f32; 3] {
    let n = if block_light.is_finite() {
        (block_light / 15.0).clamp(0.0, 1.0)
    } else {
        0.0
    };
    [1.0, 0.32 + 0.55 * n, 0.08 + 0.42 * n * n]
}

/// Cave-entrance indirect bounce tint, mirrored by the WGSL
/// `cave_bounce_color`: warm daylight scatter by day, akane-iro depth at
/// dusk, and a quiet silver-blue under moonlight. `sun_elevation` is
/// `sun_direction.y`; `moon_strength` is `moon_color.w`.
pub fn compute_cave_bounce_color(sun_elevation: f32, moon_strength: f32) -> [f32; 3] {
    let elevation = if sun_elevation.is_finite() {
        sun_elevation
    } else {
        0.0
    };
    let moon = if moon_strength.is_finite() {
        moon_strength.clamp(0.0, 4.0)
    } else {
        0.0
    };
    let mix = |a: f32, b: f32, t: f32| a + (b - a) * t;
    let day = smooth01((elevation + 0.12) / 0.24);
    let twilight = 1.0 - smooth01(elevation.abs() / 0.28);
    let mut out = [0.0; 3];
    for (index, channel) in out.iter_mut().enumerate() {
        let day_bounce = mix(
            mix([0.26, 0.18, 0.11][index], [0.16, 0.20, 0.30][index], 0.55),
            [0.30, 0.13, 0.06][index],
            twilight * 0.6,
        );
        let moon_bounce = [0.035, 0.05, 0.10][index] * (0.3 + moon);
        *channel = mix(moon_bounce, day_bounce, day);
    }
    out
}

/// Sanitized `sky_light`/`block_light` pair with every A1 term the
/// `MEGU_TERRAIN_LIT_WGSL` fragment stage derives — a CPU mirror used to
/// verify the shader math without a GPU.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct IneiSkyBlockLight {
    /// `sky_light` clamped into the vanilla `0..=15` range.
    pub sky_light: f32,
    /// `block_light` clamped into the vanilla `0..=15` range.
    pub block_light: f32,
    /// `pow(sky_light / 15, 1.8)`: gates sky ambient and entrance bounce.
    pub sky_factor: f32,
    /// `smoothstep(0, 2, sky_light)`: zero direct sun/moon in sealed caves.
    pub direct_gate: f32,
    /// Time-of-day tinted cave-entrance bounce colour.
    pub cave_bounce: [f32; 3],
    /// Warm blackbody block-light colour under quadratic falloff.
    pub block_rgb: [f32; 3],
}

/// Mirror of the light fields the packed terrain vertex already carries
/// (`block_light << 5 | sky_light << 9` inside `attributes`): unpacks both
/// channels and derives every sky/block lighting term the terrain shader
/// consumes.
pub fn compute_inei_sky_block_light(
    sky_light: f32,
    block_light: f32,
    sun_elevation: f32,
    moon_strength: f32,
) -> IneiSkyBlockLight {
    let sanitize = |value: f32| {
        if value.is_finite() {
            value.clamp(0.0, 15.0)
        } else {
            0.0
        }
    };
    let sky = sanitize(sky_light);
    let block = sanitize(block_light);
    let block_n = block / 15.0;
    IneiSkyBlockLight {
        sky_light: sky,
        block_light: block,
        sky_factor: compute_sky_light_factor(sky),
        direct_gate: compute_direct_light_gate(sky),
        cave_bounce: compute_cave_bounce_color(sun_elevation, moon_strength),
        block_rgb: scale(
            compute_block_light_color_temp(block),
            block_n * block_n,
        ),
    }
}

/// Multiplicative transmission RGB for one stained-glass colour index —
/// the CPU mirror of the WGSL `stained_glass_transmission_rgb` table in
/// [`MEGU_STAINED_GLASS_SHADOW_COLOR_WGSL`]. Indices follow the vanilla
/// `DyeColor` order (0 = white, 15 = black); anything else clamps to the
/// table ends instead of guessing a tint.
pub fn stained_glass_transmission_rgb(color_idx: u8) -> [f32; 3] {
    const TABLE: [[f32; 3]; 16] = [
        [0.97, 0.97, 0.97], // white
        [0.95, 0.52, 0.14], // orange
        [0.86, 0.30, 0.80], // magenta
        [0.36, 0.68, 0.92], // light blue
        [0.98, 0.84, 0.16], // yellow
        [0.52, 0.86, 0.14], // lime
        [0.95, 0.60, 0.72], // pink
        [0.34, 0.36, 0.38], // gray
        [0.64, 0.66, 0.68], // light gray
        [0.13, 0.60, 0.62], // cyan
        [0.56, 0.26, 0.76], // purple
        [0.22, 0.32, 0.86], // blue
        [0.46, 0.29, 0.17], // brown
        [0.36, 0.56, 0.13], // green
        [0.86, 0.14, 0.13], // red
        [0.09, 0.09, 0.10], // black
    ];
    TABLE[usize::from(color_idx.min(15))]
}

/// Why a cached shadow map can no longer be reused this frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShadowDirtyReason {
    /// No frame has rendered into the cache yet.
    FirstFrame,
    /// The active quality preset changed since the last shadow render.
    PresetChanged,
    /// `world_revision` advanced: blocks inside the shadow radius changed.
    WorldEdited,
    /// The sun moved at least `angle_threshold_deg` from the cached pass.
    SunMoved,
    /// The camera crossed into a different snapped chunk region.
    CameraCrossedSnapRegion,
}

/// Result of [`DirtyOnlyShadowCache::should_rerender_shadow_map`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShadowCacheDecision {
    /// The previous depth pass still covers this frame; skip it entirely.
    ReuseCached,
    /// The depth pass must run again for the recorded reason.
    Rerender(ShadowDirtyReason),
}

/// Dirty-only shadow map cache. Skipping the depth pass whenever the sun,
/// camera snap region, and world edits allow it structurally cuts shadow
/// cost by 4x-8x on a typical play session. Sun drift accumulates against
/// the *rendered* direction, so slow motion still refreshes at the angular
/// threshold; nothing in here reads a wall clock.
#[derive(Clone, Debug)]
pub struct DirtyOnlyShadowCache {
    /// Normalized sun direction used by the last rendered depth pass.
    pub last_sun_dir: [f32; 3],
    /// Snapped camera cell used by the last rendered depth pass.
    pub last_camera_chunk: [i32; 3],
    /// World revision counter used by the last rendered depth pass.
    pub last_world_revision: u64,
    /// Angular movement required before the sun invalidates the cache.
    pub angle_threshold_deg: f32,
    last_quality: BuiltinShadowQuality,
    primed: bool,
}

impl Default for DirtyOnlyShadowCache {
    fn default() -> Self {
        Self::new()
    }
}

impl DirtyOnlyShadowCache {
    /// Default angular threshold: 0.75 degrees of sun travel.
    pub const DEFAULT_ANGLE_THRESHOLD_DEG: f32 = 0.75;

    pub fn new() -> Self {
        Self {
            last_sun_dir: [0.0; 3],
            last_camera_chunk: [0; 3],
            last_world_revision: 0,
            angle_threshold_deg: Self::DEFAULT_ANGLE_THRESHOLD_DEG,
            last_quality: BuiltinShadowQuality::Off,
            primed: false,
        }
    }

    /// Force the next call to return `Rerender(FirstFrame)`, e.g. after the
    /// depth array was resized or re-created.
    pub fn invalidate(&mut self) {
        self.primed = false;
    }

    /// Decide whether the shadow depth pass must run this frame.
    /// `camera_chunk` is the camera position already snapped by the caller
    /// (e.g. block position divided by the snap radius). `world_revision`
    /// increments whenever blocks inside the shadow radius change. On
    /// `Rerender` the supplied values become the new cached frame.
    pub fn should_rerender_shadow_map(
        &mut self,
        sun_dir: [f32; 3],
        camera_chunk: [i32; 3],
        world_revision: u64,
        quality: BuiltinShadowQuality,
    ) -> ShadowCacheDecision {
        let decision = self.decide(sun_dir, camera_chunk, world_revision, quality);
        if let ShadowCacheDecision::Rerender(_) = decision {
            self.last_sun_dir = sun_dir;
            self.last_camera_chunk = camera_chunk;
            self.last_world_revision = world_revision;
            self.last_quality = quality;
            self.primed = true;
        }
        decision
    }

    fn decide(
        &self,
        sun_dir: [f32; 3],
        camera_chunk: [i32; 3],
        world_revision: u64,
        quality: BuiltinShadowQuality,
    ) -> ShadowCacheDecision {
        if !self.primed {
            return ShadowCacheDecision::Rerender(ShadowDirtyReason::FirstFrame);
        }
        if quality != self.last_quality {
            return ShadowCacheDecision::Rerender(ShadowDirtyReason::PresetChanged);
        }
        if world_revision != self.last_world_revision {
            return ShadowCacheDecision::Rerender(ShadowDirtyReason::WorldEdited);
        }
        // Zero-cascade presets own no shadow map, so drift is irrelevant.
        if quality.config().cascade_count == 0 {
            return ShadowCacheDecision::ReuseCached;
        }
        let threshold = if self.angle_threshold_deg.is_finite() && self.angle_threshold_deg > 0.0 {
            self.angle_threshold_deg
        } else {
            Self::DEFAULT_ANGLE_THRESHOLD_DEG
        };
        if !sun_dir.iter().all(|v| v.is_finite()) {
            return ShadowCacheDecision::Rerender(ShadowDirtyReason::SunMoved);
        }
        let a = normalize(self.last_sun_dir, [1.0, 0.0, 0.0]);
        let b = normalize(sun_dir, [1.0, 0.0, 0.0]);
        if dot(a, b).clamp(-1.0, 1.0).acos().to_degrees() >= threshold {
            return ShadowCacheDecision::Rerender(ShadowDirtyReason::SunMoved);
        }
        if camera_chunk != self.last_camera_chunk {
            return ShadowCacheDecision::Rerender(ShadowDirtyReason::CameraCrossedSnapRegion);
        }
        ShadowCacheDecision::ReuseCached
    }
}

/// Fixed render-scale presets. Deterministic by design: there is no dynamic
/// resolution feedback loop, only these five steps.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum RenderScalePreset {
    /// 100% - default, UI and world both at native resolution.
    #[default]
    Native100,
    /// 85%
    Quality85,
    /// 75%
    Balanced75,
    /// 67%
    Performance67,
    /// 50%
    UltraPerf50,
}

impl RenderScalePreset {
    pub const fn percent(self) -> u32 {
        match self {
            Self::Native100 => 100,
            Self::Quality85 => 85,
            Self::Balanced75 => 75,
            Self::Performance67 => 67,
            Self::UltraPerf50 => 50,
        }
    }

    /// Deterministic floor-scaled render target size, clamped to at least
    /// 1x1. Never upscales because every preset is at or under 100%.
    pub fn scaled_dimensions(self, width: u32, height: u32) -> (u32, u32) {
        let pct = self.percent() as u64;
        let w = (width as u64 * pct / 100).clamp(1, u32::MAX as u64);
        let h = (height as u64 * pct / 100).clamp(1, u32::MAX as u64);
        (w as u32, h as u32)
    }
}

fn camera_axes(camera: &ShadowCamera) -> ([f32; 3], [f32; 3], [f32; 3]) {
    let forward = normalize(camera.forward, [0.0, 0.0, 1.0]);
    let desired_up = normalize(camera.up, [0.0, 1.0, 0.0]);
    let chosen_up = if dot(forward, desired_up).abs() > 0.99 {
        if forward[1].abs() < 0.99 {
            [0.0, 1.0, 0.0]
        } else {
            [0.0, 0.0, 1.0]
        }
    } else {
        desired_up
    };
    let right = normalize(cross(forward, chosen_up), [1.0, 0.0, 0.0]);
    let up = normalize(cross(right, forward), [0.0, 1.0, 0.0]);
    (forward, right, up)
}

fn camera_matrix(
    position: [f32; 3],
    axes: ([f32; 3], [f32; 3], [f32; 3]),
    near: f32,
    far: f32,
    tan_half_fov: f32,
    aspect: f32,
) -> [[f32; 4]; 4] {
    let (forward, right, up) = axes;
    let x_scale = 1.0 / (tan_half_fov * aspect);
    let y_scale = 1.0 / tan_half_fov;
    let z_scale = far / (far - near);
    [
        [
            right[0] * x_scale,
            up[0] * y_scale,
            forward[0] * z_scale,
            forward[0],
        ],
        [
            right[1] * x_scale,
            up[1] * y_scale,
            forward[1] * z_scale,
            forward[1],
        ],
        [
            right[2] * x_scale,
            up[2] * y_scale,
            forward[2] * z_scale,
            forward[2],
        ],
        [
            -dot(position, right) * x_scale,
            -dot(position, up) * y_scale,
            -dot(position, forward) * z_scale - near * z_scale,
            -dot(position, forward),
        ],
    ]
}

/// Stable frustum-fitted light projections snapped to whole shadow texels.
/// No wall-clock input, allocation of GPU resources, or unsafe type casting.
/// The moon phase follows the vanilla `day % 8` cycle; eclipses stay opt-in
/// through [`build_shadow_uniforms_celestial`].
pub fn build_shadow_uniforms(
    camera: ShadowCamera,
    config: CascadeConfig,
    world_time_ticks: u64,
) -> ShadowUniforms {
    let moon_phase = ((world_time_ticks / 24_000) % 8) as u8;
    build_shadow_uniforms_celestial(
        camera,
        config,
        world_time_ticks,
        moon_phase,
        EclipseMode::None,
    )
}

/// [`build_shadow_uniforms`] with an explicit 8-phase moon and an opt-in
/// eclipse mode. Everything [`compute_celestial_lighting`] derives is folded
/// into the uniform block here, so the shaders stay branch-light.
pub fn build_shadow_uniforms_celestial(
    camera: ShadowCamera,
    config: CascadeConfig,
    world_time_ticks: u64,
    moon_phase: u8,
    eclipse: EclipseMode,
) -> ShadowUniforms {
    let position = if camera.position.iter().all(|v| v.is_finite()) {
        camera.position
    } else {
        [0.0; 3]
    };
    let (forward, right, up) = camera_axes(&camera);
    let near = if camera.near.is_finite() {
        camera.near.max(0.01)
    } else {
        0.1
    };
    let far = if camera.far.is_finite() {
        camera.far.max(near + 1.0)
    } else {
        192.0
    };
    let fov = if camera.fov_y_radians.is_finite() {
        camera.fov_y_radians.clamp(0.1, PI - 0.1)
    } else {
        PI / 3.0
    };
    let aspect = if camera.aspect_ratio.is_finite() {
        camera.aspect_ratio.clamp(0.25, 8.0)
    } else {
        16.0 / 9.0
    };
    let tan_half_fov = (fov * 0.5).tan();
    let sun = compute_sun_direction(world_time_ticks);
    // Sun by day, moon by night: below the horizon the shadow caster is the
    // real moon at -sun instead of a disabled map. The solar arc lies in
    // XY, so Z stays a continuous light-space reference at noon and
    // midnight alike; switching axes would rotate shadows.
    let caster = if sun[1] >= 0.0 {
        sun
    } else {
        scale(sun, -1.0)
    };
    let light_right = normalize(cross([0.0, 0.0, 1.0], caster), [1.0, 0.0, 0.0]);
    let light_up = cross(caster, light_right);
    let count = config.cascade_count.min(3);
    let splits = compute_cascade_splits(near, far, count, 0.65);
    let mut matrices = [[[0.0; 4]; 4]; 3];
    let mut split_uniform = [far; 4];
    for (index, &end) in splits.iter().enumerate() {
        let start = if index == 0 { near } else { splits[index - 1] };
        let center = add(position, scale(forward, (start + end) * 0.5));
        let mut radius_squared = 0.0f32;
        for distance in [start, end] {
            let h = tan_half_fov * distance;
            let w = h * aspect;
            let face_center = add(position, scale(forward, distance));
            for x in [-1.0, 1.0] {
                for y in [-1.0, 1.0] {
                    let corner = add(add(face_center, scale(right, x * w)), scale(up, y * h));
                    let delta = add(corner, scale(center, -1.0));
                    radius_squared = radius_squared.max(dot(delta, delta));
                }
            }
        }
        // Stable extent quantized to a sixteenth of a world block.
        let radius = (radius_squared.sqrt() * 16.0).ceil().max(16.0) / 16.0;
        let texel = 2.0 * radius / config.map_resolution.max(1) as f32;
        let snap = |coordinate: f32| (coordinate / texel).round() * texel;
        let cx = snap(dot(center, light_right));
        let cy = snap(dot(center, light_up));
        let cz = snap(dot(center, caster));
        let depth_radius = radius * 4.0;
        let inverse_depth = 1.0 / (2.0 * depth_radius);
        matrices[index] = [
            [
                light_right[0] / radius,
                light_up[0] / radius,
                -caster[0] * inverse_depth,
                0.0,
            ],
            [
                light_right[1] / radius,
                light_up[1] / radius,
                -caster[1] * inverse_depth,
                0.0,
            ],
            [
                light_right[2] / radius,
                light_up[2] / radius,
                -caster[2] * inverse_depth,
                0.0,
            ],
            [
                -cx / radius,
                -cy / radius,
                (depth_radius + cz) * inverse_depth,
                1.0,
            ],
        ];
        split_uniform[index] = end;
    }

    let celestial = compute_celestial_lighting(world_time_ticks, moon_phase, eclipse);
    let daylight = ((sun[1] + 0.12) / 0.24).clamp(0.0, 1.0);
    // Muted akane-iro sunlight near the horizon, without a sudden color step.
    let twilight = 1.0 - smooth01(sun[1].abs() / 0.28);
    let day_phase = (world_time_ticks % 24_000) as f32 / 24_000.0;
    // smoothstep(0.02, 0.08, |sun.y|): moonlight fades in across the horizon
    // so the sun->moon caster handover never pops a hard shadow edge.
    let horizon_fade = smooth01((sun[1].abs() - 0.02) / 0.06);
    let moon_strength = (1.0 - daylight) * celestial.moon_illuminance * horizon_fade;
    ShadowUniforms {
        light_view_proj: matrices,
        camera_view_proj: camera_matrix(
            position,
            (forward, right, up),
            near,
            far,
            tan_half_fov,
            aspect,
        ),
        cascade_splits: split_uniform,
        sun_direction: [sun[0], sun[1], sun[2], 0.0],
        sun_color: [
            (0.95 + 0.16 * twilight) * daylight * celestial.sun_illuminance,
            (0.92 - 0.18 * twilight) * daylight * celestial.sun_illuminance,
            (0.85 - 0.40 * twilight) * daylight * celestial.sun_illuminance,
            1.0,
        ],
        moon_color: [
            celestial.moon_color[0] * 0.19,
            celestial.moon_color[1] * 0.24,
            celestial.moon_color[2] * 0.34,
            moon_strength,
        ],
        camera_position: [position[0], position[1], position[2], 1.0],
        camera_forward: [forward[0], forward[1], forward[2], 0.0],
        camera_right: [right[0], right[1], right[2], tan_half_fov * aspect],
        camera_up: [up[0], up[1], up[2], tan_half_fov],
        shadow_params: [
            config.map_resolution as f32,
            count as f32,
            config.pcf_taps as f32,
            0.0,
        ],
        render_params: [config.shader_tier as f32, day_phase, 0.012, 1.0],
        celestial_params: [
            celestial.star_visibility,
            celestial.corona_boost,
            celestial.lunar_shadow_strength,
            celestial.eclipse_dim,
        ],
    }
}

/// Depth-only shadow caster. The renderer chooses the array layer via w.
pub const MEGU_SHADOW_DEPTH_WGSL: &str = r#"
struct ShadowUniforms {
    light_view_proj: array<mat4x4<f32>, 3>,
    camera_view_proj: mat4x4<f32>,
    cascade_splits: vec4<f32>,
    sun_direction: vec4<f32>,
    sun_color: vec4<f32>,
    moon_color: vec4<f32>,
    camera_position: vec4<f32>,
    camera_forward: vec4<f32>,
    camera_right: vec4<f32>,
    camera_up: vec4<f32>,
    shadow_params: vec4<f32>,
    render_params: vec4<f32>,
    celestial_params: vec4<f32>,
};
@group(0) @binding(0) var<uniform> shadow: ShadowUniforms;

struct DepthInput {
    @location(0) world_position: vec3<f32>,
};
@vertex
fn vs_main(input: DepthInput) -> @builtin(position) vec4<f32> {
    let cascade = min(u32(shadow.shadow_params.w), 2u);
    return shadow.light_view_proj[cascade] * vec4<f32>(input.world_position, 1.0);
}
@fragment
fn fs_main() {}
"#;

/// Additional depth-only vertex inputs for packed terrain, transformed
/// sub-world meshes and instanced parts. Appended to the built-in depth WGSL
/// by the GPU renderer so all three draw paths share the cascade matrices.
pub const MEGU_SHADOW_CASTERS_WGSL: &str = r#"
struct ShadowSection {
    origin: vec3<f32>,
    _pad0: f32,
    biome_tint: vec3<f32>,
    biome_tint_strength: f32,
};
struct ShadowDraw {
    mvp: mat4x4<f32>,
    model: mat4x4<f32>,
    params: vec4<f32>,
    params2: vec4<f32>,
};
@group(1) @binding(1) var<uniform> section: ShadowSection;
@group(2) @binding(0) var<uniform> draw: ShadowDraw;

@vertex
fn vs_terrain(@location(0) packed_pos: u32) -> @builtin(position) vec4<f32> {
    let local = vec3<f32>(f32(packed_pos & 31u),
        f32(packed_pos >> 10u) / 1024.0, f32((packed_pos >> 5u) & 31u));
    let world = section.origin + local;
    return shadow.light_view_proj[u32(shadow.shadow_params.w)] * vec4<f32>(world, 1.0);
}

@vertex
fn vs_lit(@location(0) position: vec3<f32>) -> @builtin(position) vec4<f32> {
    return shadow.light_view_proj[u32(shadow.shadow_params.w)]
        * draw.model * vec4<f32>(position, 1.0);
}

fn shadow_rotate_euler(p: vec3<f32>, rot: vec3<f32>) -> vec3<f32> {
    let cx = cos(rot.x);
    let sx = sin(rot.x);
    let cy = cos(rot.y);
    let sy = sin(rot.y);
    let cz = cos(rot.z);
    let sz = sin(rot.z);
    var v = vec3<f32>(p.x, p.y * cx - p.z * sx, p.y * sx + p.z * cx);
    v = vec3<f32>(v.x * cy + v.z * sy, v.y, v.z * cy - v.x * sy);
    v = vec3<f32>(v.x * cz - v.y * sz, v.x * sz + v.y * cz, v.z);
    return v;
}

@vertex
fn vs_inst(
    @location(0) position: vec3<f32>,
    @location(3) pos_scale: vec4<f32>,
    @location(4) rot: vec4<f32>,
) -> @builtin(position) vec4<f32> {
    let local = shadow_rotate_euler(position * pos_scale.w, rot.xyz) + pos_scale.xyz;
    return shadow.light_view_proj[u32(shadow.shadow_params.w)]
        * draw.model * vec4<f32>(local, 1.0);
}
"#;

/// Added to the composed opaque terrain and lit shaders when shadow maps
/// are enabled. Shadow bindings occupy group 3; the zero-cascade pipelines
/// contain neither these bindings nor texture sampling instructions.
pub const MEGU_SHADOW_SAMPLING_WGSL: &str = r#"
struct ShadowUniforms {
    light_view_proj: array<mat4x4<f32>, 3>,
    camera_view_proj: mat4x4<f32>,
    cascade_splits: vec4<f32>,
    sun_direction: vec4<f32>,
    sun_color: vec4<f32>,
    moon_color: vec4<f32>,
    camera_position: vec4<f32>,
    camera_forward: vec4<f32>,
    camera_right: vec4<f32>,
    camera_up: vec4<f32>,
    shadow_params: vec4<f32>,
    render_params: vec4<f32>,
    celestial_params: vec4<f32>,
};
@group(3) @binding(0) var<uniform> shadow: ShadowUniforms;
@group(3) @binding(1) var shadow_depth: texture_depth_2d_array;
@group(3) @binding(2) var shadow_compare: sampler_comparison;

// Compile-time Vogel disk: point i sits at radius sqrt((i + 0.5) / 16)
// along golden angle i * 2.39996323. Rescaling by 4 * inverseSqrt(taps)
// reproduces the exact sqrt((i + 0.5) / taps) footprint for any tap count
// <= 16, so the loop below needs no per-tap sqrt/sin/cos.
const VOGEL_16 = array<vec2<f32>, 16>(
    vec2<f32>(0.1767766922712326, 0.0),
    vec2<f32>(-0.2257721722126007, 0.206825852394104),
    vec2<f32>(0.03455798327922821, -0.3937712013721466),
    vec2<f32>(0.28457140922546387, 0.37117263674736023),
    vec2<f32>(-0.522223174571991, -0.09237373620271683),
    vec2<f32>(0.4946953356266022, -0.31468483805656433),
    vec2<f32>(-0.16546531021595, 0.6155251860618591),
    vec2<f32>(-0.31556200981140137, -0.6075941324234009),
    vec2<f32>(0.6846423149108887, 0.25002971291542053),
    vec2<f32>(-0.7122558951377869, 0.29400935769081116),
    vec2<f32>(0.343354195356369, -0.7337287664413452),
    vec2<f32>(0.253730446100235, 0.8089319467544556),
    vec2<f32>(-0.7647467851638794, -0.4431843161582947),
    vec2<f32>(0.8971335887908936, -0.19723407924175262),
    vec2<f32>(-0.5475055575370789, 0.7787731885910034),
    vec2<f32>(-0.1264883130788803, -0.9760894775390625)
);

// Eight precomputed rotation directions keep this shared shader free of
// per-fragment trigonometry while varying the disk orientation by position.
const SHADOW_ROTATIONS = array<vec2<f32>, 8>(
    vec2<f32>(1.0, 0.0),
    vec2<f32>(0.70710677, 0.70710677),
    vec2<f32>(0.0, 1.0),
    vec2<f32>(-0.70710677, 0.70710677),
    vec2<f32>(-1.0, 0.0),
    vec2<f32>(-0.70710677, -0.70710677),
    vec2<f32>(0.0, -1.0),
    vec2<f32>(0.70710677, -0.70710677)
);

// Day casts from the sun, night casts from the real moon at -sun — the
// shadow map stays live through the whole day cycle instead of disabling
// at dusk. The caller fades the horizon handover via |sun.y|.
fn shadow_caster_direction() -> vec3<f32> {
    let sun = normalize(shadow.sun_direction.xyz);
    return select(-sun, sun, sun.y >= 0.0);
}

fn shadow_cascade_visibility(world: vec3<f32>, n: vec3<f32>, layer: u32,
                             disk_rot: mat2x2<f32>) -> f32 {
    let caster = shadow_caster_direction();
    let incidence = clamp(dot(n, caster), 0.0, 1.0);
    let receiver = world + n * (0.015 + 0.05 * (1.0 - incidence));
    let clip = shadow.light_view_proj[layer] * vec4<f32>(receiver, 1.0);
    let uv = clip.xy * vec2<f32>(0.5, -0.5) + vec2<f32>(0.5);
    if (any(uv < vec2<f32>(0.0)) || any(uv > vec2<f32>(1.0))
        || clip.z <= 0.0 || clip.z >= 1.0) {
        return 1.0;
    }
    let resolution = max(shadow.shadow_params.x, 1.0);
    let taps = max(1u, min(u32(shadow.shadow_params.z), 16u));
    let bias = 0.00035 + (1.0 - incidence) * 0.0015;
    // sqrt((i + 0.5) / taps) == VOGEL_16[i].xy * 4 / sqrt(taps): a single
    // inverseSqrt per pixel replaces per-tap radius/trigonometric work.
    let disk_scale = 4.0 * inverseSqrt(f32(taps)) * 1.7 / resolution;
    var visibility = 0.0;
    for (var i = 0u; i < taps; i = i + 1u) {
        let offset = disk_rot * (VOGEL_16[i] * disk_scale);
        visibility += textureSampleCompareLevel(shadow_depth, shadow_compare,
            uv + offset, i32(layer), clip.z - bias);
    }
    return visibility / f32(taps);
}

fn shadow_visibility(world: vec3<f32>, n: vec3<f32>) -> f32 {
    // Horizon handover: |sun.y| < 0.02 fades to fully lit so the sun->moon
    // caster swap never pops a hard edge at dusk or dawn.
    let horizon_fade = smoothstep(0.02, 0.08, abs(shadow.sun_direction.y));
    if (horizon_fade <= 0.0) { return 1.0; }
    let count = u32(shadow.shadow_params.y);
    if (count == 0u) { return 1.0; }
    // Caster-backface early-out: a face strictly turned away from the
    // active caster — the sun by day, the moon at -sun by night — is
    // self-shadowed by its own block, so no shadow tap is ever needed.
    if (dot(n, shadow_caster_direction()) <= 0.0) { return 0.0; }
    // Interleaved-gradient hash chooses a fixed 45-degree rotation once
    // per fragment, even when a cascade blend needs two PCF evaluations.
    let hash = fract(52.9829189 * fract(dot(world.xz,
                                          vec2<f32>(0.06711056, 0.00583715))));
    let axis = SHADOW_ROTATIONS[u32(hash * 8.0)];
    let disk_rot = mat2x2<f32>(axis, vec2<f32>(-axis.y, axis.x));
    let distance_along_view = dot(world - shadow.camera_position.xyz,
                                    shadow.camera_forward.xyz);
    var layer = 0u;
    if (count > 1u && distance_along_view > shadow.cascade_splits.x) { layer = 1u; }
    if (count > 2u && distance_along_view > shadow.cascade_splits.y) { layer = 2u; }
    var visibility = shadow_cascade_visibility(world, n, layer, disk_rot);
    if (layer + 1u < count) {
        var start = 0.1;
        if (layer > 0u) {
            start = shadow.cascade_splits[layer - 1u];
        }
        let end = shadow.cascade_splits[layer];
        let band = max((end - start) * 0.08, 0.01);
        let blend = smoothstep(end - band, end, distance_along_view);
        // Next-cascade guard: blend == 0 contributes nothing, so the second
        // cascade PCF only runs inside the ~8% blend band.
        if (blend > 0.001) {
            visibility = mix(visibility,
                shadow_cascade_visibility(world, n, layer + 1u, disk_rot), blend);
        }
    }
    return mix(1.0, visibility, horizon_fade);
}
"#;

/// Lit terrain with cascaded comparison samplers, quality-dependent PCF,
/// hemisphere and warm bounce ambient, atmospheric fog and colored VFX lights.
pub const MEGU_TERRAIN_LIT_WGSL: &str = r#"
struct ShadowUniforms {
    light_view_proj: array<mat4x4<f32>, 3>,
    camera_view_proj: mat4x4<f32>,
    cascade_splits: vec4<f32>,
    sun_direction: vec4<f32>,
    sun_color: vec4<f32>,
    moon_color: vec4<f32>,
    camera_position: vec4<f32>,
    camera_forward: vec4<f32>,
    camera_right: vec4<f32>,
    camera_up: vec4<f32>,
    shadow_params: vec4<f32>,
    render_params: vec4<f32>,
    celestial_params: vec4<f32>,
};
struct PointLight {
    position_radius: vec4<f32>,
    color_intensity: vec4<f32>,
};
struct DynamicLights {
    light_count: vec4<u32>,
    lights: array<PointLight, 64>,
};
@group(0) @binding(0) var<uniform> shadow: ShadowUniforms;
@group(0) @binding(1) var shadow_depth: texture_depth_2d_array;
@group(0) @binding(2) var depth_comparison: sampler_comparison;
@group(0) @binding(3) var<uniform> dynamic_lights: DynamicLights;

struct TerrainInput {
    @location(0) world_position: vec3<f32>,
    @location(1) normal: vec3<f32>,
    @location(2) albedo: vec3<f32>,
    // Packed terrain attribute word matching PackedVertex.attributes:
    // block_light at bits 5..8, sky_light at bits 9..12.
    @location(3) light_attr: u32,
};
struct TerrainOutput {
    @builtin(position) position: vec4<f32>,
    @location(0) world_position: vec3<f32>,
    @location(1) normal: vec3<f32>,
    @location(2) albedo: vec3<f32>,
    // x = sky_light (0..15), y = block_light (0..15).
    @location(3) sky_block_light: vec2<f32>,
};
@vertex
fn vs_main(input: TerrainInput) -> TerrainOutput {
    var out: TerrainOutput;
    out.position = shadow.camera_view_proj * vec4<f32>(input.world_position, 1.0);
    out.world_position = input.world_position;
    out.normal = input.normal;
    out.albedo = input.albedo;
    out.sky_block_light = vec2<f32>(f32((input.light_attr >> 9u) & 0xfu),
                                    f32((input.light_attr >> 5u) & 0xfu));
    return out;
}
fn poisson(i: u32) -> vec2<f32> {
    let points = array<vec2<f32>, 9>(
        vec2<f32>(-0.71, 0.18), vec2<f32>(0.23, 0.66),
        vec2<f32>(0.81, -0.19), vec2<f32>(-0.25, -0.78),
        vec2<f32>(0.09, 0.03), vec2<f32>(-0.87, -0.52),
        vec2<f32>(0.61, 0.73), vec2<f32>(0.39, -0.91),
        vec2<f32>(-0.40, 0.85)
    );
    return points[i];
}
// Compile-time Vogel disk: point i sits at radius sqrt((i + 0.5) / 16)
// along golden angle i * 2.3999632. Rescaling by 4 * inverseSqrt(taps)
// reproduces the exact sqrt((i + 0.5) / taps) footprint for any tap count
// <= 16, so the loop below needs no per-tap sqrt/sin/cos.
const VOGEL_16 = array<vec2<f32>, 16>(
    vec2<f32>(0.1767766922712326, 0.0),
    vec2<f32>(-0.2257721722126007, 0.206825852394104),
    vec2<f32>(0.03455798327922821, -0.3937712013721466),
    vec2<f32>(0.28457140922546387, 0.37117263674736023),
    vec2<f32>(-0.522223174571991, -0.09237373620271683),
    vec2<f32>(0.4946953356266022, -0.31468483805656433),
    vec2<f32>(-0.16546531021595, 0.6155251860618591),
    vec2<f32>(-0.31556200981140137, -0.6075941324234009),
    vec2<f32>(0.6846423149108887, 0.25002971291542053),
    vec2<f32>(-0.7122558951377869, 0.29400935769081116),
    vec2<f32>(0.343354195356369, -0.7337287664413452),
    vec2<f32>(0.253730446100235, 0.8089319467544556),
    vec2<f32>(-0.7647467851638794, -0.4431843161582947),
    vec2<f32>(0.8971335887908936, -0.19723407924175262),
    vec2<f32>(-0.5475055575370789, 0.7787731885910034),
    vec2<f32>(-0.1264883130788803, -0.9760894775390625)
);

// Per-pixel interleaved-gradient rotation seed for the disk.
fn ign(p: vec2<f32>) -> f32 {
    return fract(52.9829189 * fract(dot(p, vec2<f32>(0.06711056, 0.00583715))));
}

fn pow2(x: f32) -> f32 { return x * x; }
fn pow3(x: f32) -> f32 { return x * x * x; }
fn pow4(x: f32) -> f32 { let x2 = x * x; return x2 * x2; }
fn pow12(x: f32) -> f32 { let x4 = pow4(x); return x4 * x4 * x4; }
fn pow24(x: f32) -> f32 { let x12 = pow12(x); return x12 * x12; }

// Prefix mask over the first `count` of 64 dynamic lights (u64 as lo/hi).
fn light_mask_prefix(count: u32) -> vec2<u32> {
    if (count >= 64u) {
        return vec2<u32>(0xFFFFFFFFu, 0xFFFFFFFFu);
    }
    if (count == 32u) {
        return vec2<u32>(0xFFFFFFFFu, 0u);
    }
    if (count > 32u) {
        return vec2<u32>(0xFFFFFFFFu, (1u << (count - 32u)) - 1u);
    }
    if (count == 0u) {
        return vec2<u32>(0u, 0u);
    }
    return vec2<u32>((1u << count) - 1u, 0u);
}

// Removes and returns the lowest set bit index of the u64 mask
// (lo = x, hi = y), or 64 when empty — one countTrailingZeros per light.
fn light_mask_take(mask: ptr<function, vec2<u32>>) -> u32 {
    if ((*mask).x != 0u) {
        let i = countTrailingZeros((*mask).x);
        (*mask).x = (*mask).x & ((*mask).x - 1u);
        return i;
    }
    if ((*mask).y != 0u) {
        let i = countTrailingZeros((*mask).y);
        (*mask).y = (*mask).y & ((*mask).y - 1u);
        return 32u + i;
    }
    return 64u;
}

// Day casts from the sun, night casts from the real moon at -sun.
fn shadow_caster_direction() -> vec3<f32> {
    let sun = normalize(shadow.sun_direction.xyz);
    return select(-sun, sun, sun.y >= 0.0);
}

fn sample_cascade(position: vec3<f32>, normal: vec3<f32>, cascade: u32) -> f32 {
    // Caster-backface early-out: a face strictly turned away from the
    // active caster — the sun by day, the moon at -sun by night — is
    // self-shadowed, so no tap is needed.
    let caster = shadow_caster_direction();
    if (dot(normal, caster) <= 0.0) {
        return 0.0;
    }
    let quality = u32(shadow.render_params.x);
    // A solar eclipse softens penumbrae by widening the filter footprint.
    let texel = (1.0 + shadow.celestial_params.w * 1.5)
        / max(shadow.shadow_params.x, 1.0);
    let incidence = abs(dot(normal, caster));
    let receiver = position + normal * (0.012 + 0.06 * (1.0 - incidence));
    let clip = shadow.light_view_proj[cascade] * vec4<f32>(receiver, 1.0);
    let uv = clip.xy * vec2<f32>(0.5, -0.5) + vec2<f32>(0.5, 0.5);
    if (any(uv < vec2<f32>(0.0)) || any(uv > vec2<f32>(1.0))
        || clip.z <= 0.0 || clip.z >= 1.0) {
        return 1.0;
    }
    let taps = min(u32(shadow.shadow_params.z), 16u);
    if (taps == 0u) {
        return 1.0;
    }
    // Interleaved-gradient per-pixel rotation, applied as one mat2x2:
    // a single sin/cos pair per pixel replaces per-tap trigonometry.
    let rotation = ign(position.xz) * 6.2831853;
    let c = cos(rotation);
    let s = sin(rotation);
    let disk_rot = mat2x2<f32>(c, s, -s, c);
    // sqrt((i + 0.5) / taps) == VOGEL_16[i].xy * 4 / sqrt(taps).
    let disk_scale = 4.0 * inverseSqrt(f32(taps));
    var visibility = 0.0;
    for (var i = 0u; i < taps; i = i + 1u) {
        var offset: vec2<f32>;
        if (quality <= 1u) {
            offset = vec2<f32>(f32(i % 2u), f32(i / 2u)) * 2.0 - vec2<f32>(1.0);
        } else if (quality == 2u) {
            offset = disk_rot * poisson(i);
        } else {
            offset = disk_rot * VOGEL_16[i] * disk_scale * 1.8;
        }
        visibility += textureSampleCompareLevel(shadow_depth, depth_comparison,
                                                uv + offset * texel, i32(cascade),
                                                clip.z - 0.0004);
    }
    visibility = visibility / f32(taps);
    if (quality >= 3u) {
        // Extra sub-texel depth check hints at short contact occlusion.
        let contact = textureSampleCompareLevel(shadow_depth, depth_comparison,
                                                uv + normal.xz * texel * 0.35,
                                                i32(cascade), clip.z - 0.00015);
        visibility = mix(visibility, min(visibility, contact), 0.18);
    }
    return visibility;
}
fn aces(color: vec3<f32>) -> vec3<f32> {
    let a = color * (2.51 * color + vec3<f32>(0.03));
    let b = color * (2.43 * color + vec3<f32>(0.59)) + vec3<f32>(0.14);
    return clamp(a / b, vec3<f32>(0.0), vec3<f32>(1.0));
}
fn yugen_valley_mist(world_y: f32, camera_y: f32, view_distance: f32,
                     base_fog_density: f32, day_phase: f32) -> f32 {
    let valley = 1.0 - smoothstep(63.0, 96.0, world_y);
    let camera_proximity = 1.0 - smoothstep(0.0, 128.0, abs(camera_y - world_y));
    let twilight = 1.0 - smoothstep(0.0, 0.28, abs(sin(day_phase * 6.2831853)));
    let density = max(base_fog_density, 0.0)
        * (0.32 + valley * (0.78 + 0.22 * camera_proximity) * (0.80 + 0.50 * twilight));
    return clamp(1.0 - exp(-max(view_distance, 0.0) * density), 0.0, 0.92);
}
fn komorebi_dapple(world_xz: vec2<f32>, day_phase: f32, sun_elevation: f32) -> f32 {
    let drift = sin(day_phase * 6.2831853) * 0.35;
    let x = world_xz.x;
    let z = world_xz.y;
    let broad = sin(x * 0.31 + z * 0.21 + drift)
        * sin(z * 0.27 - x * 0.17 - drift * 0.7);
    let fine = sin(x * 0.62 - z * 0.42 + drift * 1.7)
        * sin(z * 0.54 + x * 0.34 - drift * 1.3);
    let daylight = smoothstep(0.08, 0.20, sun_elevation);
    return 1.0 + daylight * (-0.03 + 0.11 * (0.72 * broad + 0.28 * fine));
}
// Two low-cost cloud layers drift with the same day clock as the sky dome.
// Their soft coverage both shades exposed terrain and leaves luminous gaps
// for the atmospheric shaft hint below.
fn cloud_coverage(world_xz: vec2<f32>, day_phase: f32) -> f32 {
    let phase = day_phase * 6.2831853;
    let drift = phase * 0.013;
    let p0 = world_xz * 0.018 + vec2<f32>(drift * 0.72, drift * 0.31);
    let broad = sin(dot(p0, vec2<f32>(1.31, 0.73)))
        * cos(dot(p0, vec2<f32>(-0.49, 1.17)));
    let p1 = world_xz * 0.044 + vec2<f32>(drift * -0.53, drift * 0.37);
    let fine = sin(dot(p1, vec2<f32>(2.47, -1.81)))
        * cos(dot(p1, vec2<f32>(1.63, 2.09)));
    let field = 0.68 * broad + 0.32 * fine;
    return smoothstep(-0.20, 0.62, field);
}
fn cloud_shadow_visibility(world_xz: vec2<f32>, day_phase: f32,
                           sky_light: f32) -> f32 {
    let sky_open = smoothstep(10.8, 14.25, sky_light);
    let rain = clamp(shadow.sun_direction.w, 0.0, 1.0);
    let coverage = cloud_coverage(world_xz, day_phase) * (0.45 + 0.55 * rain);
    return mix(1.0, 1.0 - 0.65 * coverage, sky_open);
}
fn rain_ripple(world_xz: vec2<f32>, time: f32, wetness: f32) -> f32 {
    let p = world_xz * 0.72 + vec2<f32>(time * 0.031, -time * 0.021);
    let cell = floor(p);
    let seed = fract(sin(dot(cell, vec2<f32>(17.13, 91.77))) * 31313.7);
    let center = vec2<f32>(fract(seed * 17.0), fract(seed * 71.0));
    let local = fract(p) - center;
    let age = fract(time * 0.05 + seed);
    let radius = age * 0.58;
    let ring = 1.0 - smoothstep(0.025, 0.07, abs(length(local) - radius));
    return wetness * ring * (1.0 - age);
}
// A1: non-linear sky-light falloff n^1.8 as a single exp2/log2 pair
// — the dark-vision curve that lets cave depth swallow skylight fast.
fn sky_light_factor(sky_light: f32) -> f32 {
    let n = clamp(sky_light / 15.0, 0.0, 1.0);
    return exp2(1.8 * log2(max(n, 0.00001)));
}
// A1: warm blackbody-style temperature for baked block light —
// incandescent gold beside a torch, deep amber-orange farther out.
fn color_temp(block_light: f32) -> vec3<f32> {
    let n = clamp(block_light / 15.0, 0.0, 1.0);
    return vec3<f32>(1.0, 0.32 + 0.55 * n, 0.08 + 0.42 * pow2(n));
}
// A1: cave-entrance indirect bounce tint — warm daylight scatter by day,
// akane-iro depth at dusk, quiet silver-blue under moonlight.
fn cave_bounce_color() -> vec3<f32> {
    let day = smoothstep(-0.12, 0.12, shadow.sun_direction.y);
    let twilight = 1.0 - smoothstep(0.0, 0.28, abs(shadow.sun_direction.y));
    var day_bounce = mix(vec3<f32>(0.26, 0.18, 0.11),
                         vec3<f32>(0.16, 0.20, 0.30), 0.55);
    day_bounce = mix(day_bounce, vec3<f32>(0.30, 0.13, 0.06), twilight * 0.6);
    let moon_bounce = vec3<f32>(0.035, 0.05, 0.10) * (0.3 + shadow.moon_color.w);
    return mix(moon_bounce, day_bounce, day);
}
@fragment
fn fs_main(input: TerrainOutput) -> @location(0) vec4<f32> {
    let normal = normalize(input.normal);
    let to_sun = normalize(shadow.sun_direction.xyz);
    let sun_power = max(dot(normal, to_sun), 0.0);
    let moon_power = max(dot(normal, -to_sun), 0.0);
    var visibility = 1.0;
    let view_offset = input.world_position - shadow.camera_position.xyz;
    let view_distance = length(view_offset);
    let view_depth = dot(view_offset, shadow.camera_forward.xyz);
    let cascade_count = u32(shadow.shadow_params.y);
    // At the caster handover the blend is zero, so skip all PCF work.
    let shadow_strength = smoothstep(0.02, 0.08, abs(shadow.sun_direction.y));
    if (cascade_count > 0u && shadow_strength > 0.0) {
        var layer = 0u;
        if (cascade_count > 1u && view_depth > shadow.cascade_splits.x) {
            layer = 1u;
        }
        if (cascade_count > 2u && view_depth > shadow.cascade_splits.y) {
            layer = 2u;
        }
        visibility = sample_cascade(input.world_position, normal, layer);
        // The sun->moon caster handover fades shadow strength across the
        // horizon so the flip never pops a hard edge.
        visibility = mix(1.0, visibility, shadow_strength);
    }
    // A1: vertex sky/block light drive the physical cave light model.
    let sky_light = clamp(input.sky_block_light.x, 0.0, 15.0);
    let block_light = clamp(input.sky_block_light.y, 0.0, 15.0);
    let sky_factor = sky_light_factor(sky_light);
    // Strict underground gate: sealed caves receive exactly zero direct
    // sun/moon even where the shadow map still reports visibility = 1.
    let direct_gate = smoothstep(0.0, 2.0, sky_light);
    let sky_ambient = mix(vec3<f32>(0.11, 0.14, 0.23),
                          vec3<f32>(0.31, 0.37, 0.50), max(normal.y, 0.0));
    let bounce = mix(vec3<f32>(0.19, 0.12, 0.09), vec3<f32>(0.0),
                      max(normal.y, 0.0));
    // Noki: let deep eaves absorb cool sky while retaining warm indirect light.
    let shadow_sky_scale = mix(0.42, 1.0, visibility);
    let warm_bounce_scale = mix(1.15, 1.0, visibility);
    // An eclipse dims hemisphere ambient too, not just the disc light.
    let ambient_dim = 1.0 - shadow.celestial_params.w * 0.55;
    // Lunar shadow strength lets moonlight share the shadow map softly.
    let lunar_visibility = mix(1.0, visibility, shadow.celestial_params.z);
    // The sky_light gradient carries the outside sky into enclosed spaces
    // as entrance bounce before dissolving into deep cave darkness.
    let enclosure = 1.0 - smoothstep(12.0, 15.0, sky_light);
    let entrance_bounce = cave_bounce_color() * sky_factor * enclosure
                          * (0.6 + 0.4 * max(normal.y, 0.0));
    let ambient = max(((sky_ambient * shadow_sky_scale
                        + bounce * warm_bounce_scale) * sky_factor
                       + entrance_bounce) * ambient_dim,
                      vec3<f32>(0.008, 0.009, 0.014));
    let quality = shadow.render_params.x;
    var cloud_shadow = 1.0;
    if (quality >= 2.0) {
        cloud_shadow = cloud_shadow_visibility(
            input.world_position.xz,
            shadow.render_params.y,
            sky_light,
        );
    }
    // Komorebi is real leaf cutout + wind caster geometry whenever the
    // shadow map is live; the analytic dapple survives only as the
    // map-free fallback so walls never show fake wave artifacts.
    var komorebi = 1.0;
    if (cascade_count == 0u) {
        komorebi = komorebi_dapple(input.world_position.xz, shadow.render_params.y,
                                   shadow.sun_direction.y);
    }
    // Baked block light burns warm through the deepest dark.
    let block_glow = color_temp(block_light) * pow2(block_light / 15.0) * 0.85;
    let sun_warmth = 1.0 - smoothstep(0.06, 0.46, max(shadow.sun_direction.y, 0.0));
    let sun_temperature = mix(vec3<f32>(1.0, 0.99, 0.96),
                              vec3<f32>(1.0, 0.64, 0.38), sun_warmth);
    let light = ambient + shadow.sun_color.xyz * sun_temperature * sun_power
                          * visibility * komorebi * cloud_shadow
                          * direct_gate
                + shadow.moon_color.xyz * moon_power * shadow.moon_color.w
                  * lunar_visibility * direct_gate
                + block_glow;
    let view_dir = -view_offset / max(view_distance, 0.0001);
    // The unused sun-direction w lane carries live precipitation. Both sky
    // exposure and upward normal gate it, so roofs and cave ceilings stay dry.
    // render_params.z is fog density (let wetness = clamp(shadow.render_params.z * 24.0, 0.0, 1.0));
    // precipitation instead comes from sun_direction.w below.
    var wetness = 0.0;
    if (quality >= 2.0) {
        wetness = clamp(shadow.sun_direction.w, 0.0, 1.0)
            * smoothstep(0.7, 1.0, normal.y)
            * smoothstep(12.75, 15.0, sky_light);
    }
    let wet_luma = dot(input.albedo, vec3<f32>(0.2126, 0.7152, 0.0722));
    let rich_albedo = clamp(input.albedo
        * (vec3<f32>(1.0) + (input.albedo - vec3<f32>(wet_luma)) * (0.08 * wetness))
        * (1.0 - 0.18 * wetness), vec3<f32>(0.0), vec3<f32>(1.0));
    var rain_rings = 0.0;
    if (quality >= 2.0) {
        rain_rings = rain_ripple(input.world_position.xz,
                                 shadow.render_params.y * 24000.0, wetness);
    }
    let grazing_sheen = pow4(1.0 - clamp(dot(normal, view_dir), 0.0, 1.0))
        * (0.12 + 0.035 * rain_rings) * wetness * max(normal.y, 0.0);
    // Quiet the darkest saturated materials before warm lamplight returns.
    var lit_color = rich_albedo * light;
    let luma = dot(lit_color, vec3<f32>(0.299, 0.587, 0.114));
    let shibumi = (1.0 - visibility) * 0.22;
    lit_color = mix(lit_color, vec3<f32>(luma) * vec3<f32>(0.96, 0.95, 1.02), shibumi);
    lit_color += vec3<f32>(0.10, 0.14, 0.17) * (grazing_sheen + rain_rings * 0.018);
    var lantern_light = vec3<f32>(0.0);
    var lacquer_sheen = vec3<f32>(0.0);
    let inei_boost = mix(1.38, 1.0,
                         visibility * clamp(shadow.sun_direction.y * 1.5, 0.0, 1.0));
    var limit = 8u;
    if (shadow.render_params.x >= 2.0) { limit = 32u; }
    if (shadow.render_params.x >= 3.0) { limit = 64u; }
    // The loop walks a u64 capability mask (lo = x, hi = y) over the first
    // `light_limit` admitted lights; an optional region mask packed into
    // light_count.yz by `compute_region_light_mask` narrows the walk to the
    // lights intersecting the meshed region. The fixed 64-slot binding
    // layout is unchanged.
    let light_limit = min(dynamic_lights.light_count.x, limit);
    var light_mask = light_mask_prefix(light_limit);
    let region_mask = dynamic_lights.light_count.yz;
    if (region_mask.x != 0u || region_mask.y != 0u) {
        light_mask = light_mask & region_mask;
    }
    loop {
        let i = light_mask_take(&light_mask);
        if (i >= 64u) {
            break;
        }
        let point = dynamic_lights.lights[i];
        let displacement = point.position_radius.xyz - input.world_position;
        let radius = max(point.position_radius.w, 0.01);
        let distance_sq = max(dot(displacement, displacement), 0.0001);
        let distance = sqrt(distance_sq);
        let falloff = pow2(max(1.0 - distance / radius, 0.0));
        let L_dir = displacement / distance;
        let wrap = 0.28;
        let wrapped_diffuse = clamp((dot(normal, L_dir) + wrap) / (1.0 + wrap), 0.0, 1.0);
        let radiance = point.color_intensity.xyz * point.color_intensity.w
                       * falloff * inei_boost;
        lantern_light += radiance * wrapped_diffuse;
        // A small muted lacquer/aged-wood glint around nearby warm light.
        let half_vector = L_dir + view_dir;
        let half_vec = half_vector / max(length(half_vector), 0.0001);
        lacquer_sheen += radiance * vec3<f32>(1.0, 0.78, 0.52)
                         * pow12(max(dot(normal, half_vec), 0.0)) * 0.08;
    }
    lit_color += rich_albedo * lantern_light + lacquer_sheen
                 + sky_ambient * grazing_sheen;
    let atmosphere = yugen_valley_mist(input.world_position.y, shadow.camera_position.y,
                                       view_distance, shadow.render_params.z,
                                       shadow.render_params.y);
    let daylight = smoothstep(-0.12, 0.12, shadow.sun_direction.y);
    let twilight = 1.0 - smoothstep(0.0, 0.28, abs(shadow.sun_direction.y));
    let gaze = view_offset / max(view_distance, 0.0001);
    let horizon_mist = pow3(1.0 - abs(gaze.y));
    let toward_sun = pow4(max(dot(gaze, to_sun), 0.0));
    let indigo_fog = mix(vec3<f32>(0.075, 0.095, 0.17),
                         vec3<f32>(0.50, 0.59, 0.68), daylight);
    let mist = twilight * horizon_mist;
    let fog = mix(indigo_fog, vec3<f32>(0.28, 0.22, 0.25), mist * 0.25)
              + vec3<f32>(0.18, 0.075, 0.04) * mist * toward_sun;
    var color = mix(lit_color, fog, atmosphere);
    if (shadow.render_params.x >= 2.0) {
        // Screen-direction sun-shaft hint, without a volumetric buffer.
        let toward = view_offset / max(view_distance, 0.0001);
        let beam = pow24(max(dot(toward, to_sun), 0.0));
        let opening = 1.0 - cloud_coverage(input.world_position.xz, shadow.render_params.y);
        let shafts = beam * atmosphere * opening * direct_gate * 0.055;
        color += shadow.sun_color.xyz * sun_temperature * shafts;
    }
    if (shadow.render_params.x >= 3.0) {
        color = aces(color);
    }
    return vec4<f32>(pow(max(color, vec3<f32>(0.0)),
                         vec3<f32>(1.0 / 2.2)), 1.0);
}
"#;

/// Screen-filling atmospheric dome with Rayleigh/Mie phase approximations,
/// warm twilight, sun disc, moon disc, blocky voxel clouds, crisp pixel
/// stars, opt-in eclipse dimming/corona and an inexpensive Low gradient.
pub const MEGU_SKY_ATMOSPHERE_WGSL: &str = r#"
struct ShadowUniforms {
    light_view_proj: array<mat4x4<f32>, 3>,
    camera_view_proj: mat4x4<f32>,
    cascade_splits: vec4<f32>,
    sun_direction: vec4<f32>,
    sun_color: vec4<f32>,
    moon_color: vec4<f32>,
    camera_position: vec4<f32>,
    camera_forward: vec4<f32>,
    camera_right: vec4<f32>,
    camera_up: vec4<f32>,
    shadow_params: vec4<f32>,
    render_params: vec4<f32>,
    celestial_params: vec4<f32>,
};
@group(0) @binding(0) var<uniform> shadow: ShadowUniforms;

fn pow3(x: f32) -> f32 { return x * x * x; }
fn pow4(x: f32) -> f32 { let x2 = x * x; return x2 * x2; }
fn pow260(x: f32) -> f32 {
    // x^260 = x^256 * x^4 through a pure doubling chain.
    let x4 = pow4(x);
    return pow4(pow4(pow4(x4))) * x4;
}

struct SkyOutput {
    @builtin(position) position: vec4<f32>,
    @location(0) ndc: vec2<f32>,
};
@vertex
fn vs_main(@builtin(vertex_index) vertex_index: u32) -> SkyOutput {
    let points = array<vec2<f32>, 3>(
        vec2<f32>(-1.0, -1.0), vec2<f32>(3.0, -1.0), vec2<f32>(-1.0, 3.0)
    );
    var out: SkyOutput;
    out.ndc = points[vertex_index];
    out.position = vec4<f32>(out.ndc, 1.0, 1.0);
    return out;
}
@fragment
fn fs_main(input: SkyOutput) -> @location(0) vec4<f32> {
    let view = normalize(shadow.camera_forward.xyz
                         + shadow.camera_right.xyz * input.ndc.x * shadow.camera_right.w
                         + shadow.camera_up.xyz * input.ndc.y * shadow.camera_up.w);
    let sun = normalize(shadow.sun_direction.xyz);
    let mu = clamp(dot(view, sun), -1.0, 1.0);
    let horizon = pow3(1.0 - abs(view.y));
    let day = smoothstep(-0.12, 0.12, sun.y);
    // Tasogare: ai-iro above, restrained akane by the horizon.
    let twilight = 1.0 - smoothstep(0.0, 0.28, abs(sun.y));
    let night = vec3<f32>(0.006, 0.014, 0.034);
    let zenith = mix(mix(night, vec3<f32>(0.06, 0.20, 0.46), day),
                     vec3<f32>(0.025, 0.040, 0.13), twilight * 0.85);
    let pale = mix(mix(vec3<f32>(0.02, 0.035, 0.08),
                       vec3<f32>(0.39, 0.55, 0.74), day),
                   vec3<f32>(0.19, 0.18, 0.27), twilight * 0.72);
    var color = mix(zenith, pale, clamp(horizon * 0.85, 0.0, 1.0));
    if (shadow.render_params.x >= 2.0) {
        let rayleigh = (1.0 + mu * mu) * 0.75;
        let mie_g = 0.72;
        let denom = max(1.0 + mie_g * mie_g - 2.0 * mie_g * mu, 0.01);
        let mie = (1.0 - mie_g * mie_g) / (denom * sqrt(denom));
        color += vec3<f32>(0.025, 0.072, 0.15) * rayleigh * day
                 * (1.0 - twilight * 0.65);
        color += vec3<f32>(0.10, 0.075, 0.045) * mie * day * 0.1;
        // Blocky voxel clouds drifting on the dome stay faithful to the
        // voxel world instead of adding a smooth billboard layer.
        if (view.y > 0.04 && view.y < 0.75) {
            let drift = shadow.render_params.y * 6.2831853 * 0.006;
            let uv = view.xz / (view.y + 0.28) * 0.35 + vec2<f32>(drift, 0.0);
            let cell = floor(uv * 7.0);
            let roll = fract(sin(dot(cell, vec2<f32>(17.13, 91.77))) * 31313.7);
            let cloud_presence = smoothstep(0.70, 0.88, roll);
            if (cloud_presence > 0.001) {
                let local = fract(uv * 7.0);
                let edge_distance = min(min(local.x, 1.0 - local.x),
                                        min(local.y, 1.0 - local.y));
                let edge = 1.0 - smoothstep(0.02, 0.15, edge_distance);
                let fade = smoothstep(0.045, 0.20, view.y)
                    * (1.0 - smoothstep(0.43, 0.75, view.y));
                let warm_sun = mix(vec3<f32>(1.0, 0.99, 0.96),
                                   vec3<f32>(1.0, 0.65, 0.40),
                                   1.0 - smoothstep(0.06, 0.46, max(sun.y, 0.0)));
                let day_cloud = mix(vec3<f32>(0.10, 0.12, 0.19),
                                    vec3<f32>(0.82, 0.84, 0.88), day)
                                    * warm_sun;
                let lit = mix(day_cloud, vec3<f32>(0.22, 0.25, 0.34), 1.0 - day);
                let deck_alpha = cloud_presence * fade * 0.46;
                let rim = cloud_presence * edge * fade * pow4(max(mu, 0.0)) * 0.11;
                color = mix(color, lit, deck_alpha);
                color += warm_sun * rim;
            }
        }
    }
    let toward_sun = pow4(max(mu, 0.0));
    // Asa-moya lends the twilight horizon a moist, low-contrast veil.
    color = mix(color, vec3<f32>(0.24, 0.21, 0.23), twilight * horizon * 0.16);
    color += vec3<f32>(0.42, 0.17, 0.10) * twilight * horizon * toward_sun;
    // A solar eclipse dims the whole dome with the occluded disc.
    color *= 1.0 - shadow.celestial_params.w * 0.55;
    // Crisp pixel stars: chunky direction cells, no smooth sparkle noise.
    if (shadow.celestial_params.x > 0.001 && view.y > 0.02) {
        let cell = floor(view * 42.0);
        let roll = fract(sin(dot(cell, vec3<f32>(12.99, 78.23, 37.72)))
                         * 43758.55);
        if (roll > 0.996) {
            let bright = (roll - 0.996) / 0.004;
            color += vec3<f32>(0.90, 0.93, 1.0) * bright
                     * shadow.celestial_params.x * 0.85;
        }
    }
    let sun_disc = smoothstep(0.9998, 0.99998, mu);
    let moon_disc = smoothstep(0.99984, 0.99996, dot(view, -sun));
    color += vec3<f32>(3.2, 2.55, 1.65) * day * sun_disc
             * (1.0 - shadow.celestial_params.w);
    // The eclipse corona appears as a pale ring once the disc is covered.
    if (shadow.celestial_params.y > 0.001) {
        let ring = pow260(max(mu, 0.0));
        color += vec3<f32>(1.0, 0.92, 0.78) * ring
                 * shadow.celestial_params.y * 1.6;
    }
    color += vec3<f32>(0.55, 0.67, 0.88) * (1.0 - day) * moon_disc
             * (1.0 - shadow.celestial_params.w * 0.4);
    return vec4<f32>(pow(max(color, vec3<f32>(0.0)),
                         vec3<f32>(1.0 / 2.2)), 1.0);
}
"#;

/// Animated wave normals, Schlick Fresnel sky reflection, sun glitter,
/// depth-based absorption and high-quality shallow-water caustic detail.
pub const MEGU_WATER_SURFACE_WGSL: &str = r#"
struct ShadowUniforms {
    light_view_proj: array<mat4x4<f32>, 3>,
    camera_view_proj: mat4x4<f32>,
    cascade_splits: vec4<f32>,
    sun_direction: vec4<f32>,
    sun_color: vec4<f32>,
    moon_color: vec4<f32>,
    camera_position: vec4<f32>,
    camera_forward: vec4<f32>,
    camera_right: vec4<f32>,
    camera_up: vec4<f32>,
    shadow_params: vec4<f32>,
    render_params: vec4<f32>,
    celestial_params: vec4<f32>,
};
@group(0) @binding(0) var<uniform> shadow: ShadowUniforms;

fn pow3(x: f32) -> f32 { return x * x * x; }
fn pow4(x: f32) -> f32 { let x2 = x * x; return x2 * x2; }
fn pow5(x: f32) -> f32 { return pow4(x) * x; }
fn pow72(x: f32) -> f32 {
    // x^72 = x^48 * x^24 through a pure doubling chain.
    let x3 = x * x * x;
    let x6 = x3 * x3;
    let x12 = x6 * x6;
    let x24 = x12 * x12;
    let x48 = x24 * x24;
    return x48 * x24;
}
fn pow180(x: f32) -> f32 {
    // x^180 = x^144 * x^36 through a pure doubling chain.
    let x2 = x * x;
    let x4 = x2 * x2;
    let x8 = x4 * x4;
    let x9 = x8 * x;
    let x18 = x9 * x9;
    let x36 = x18 * x18;
    let x72 = x36 * x36;
    let x144 = x72 * x72;
    return x144 * x36;
}

struct WaterInput {
    @location(0) world_position: vec3<f32>,
    @location(1) depth_hint: f32,
};
struct WaterOutput {
    @builtin(position) position: vec4<f32>,
    @location(0) world_position: vec3<f32>,
    @location(1) depth_hint: f32,
};
@vertex
fn vs_main(input: WaterInput) -> WaterOutput {
    var out: WaterOutput;
    out.position = shadow.camera_view_proj * vec4<f32>(input.world_position, 1.0);
    out.world_position = input.world_position;
    out.depth_hint = input.depth_hint;
    return out;
}
fn wave_normal(position: vec2<f32>, phase: f32, quality: f32) -> vec3<f32> {
    let w0 = position.x * 0.42 + position.y * 0.23 + phase * 2.0;
    let w1 = position.x * -0.19 + position.y * 0.56 - phase;
    var slope = vec2<f32>(cos(w0) * 0.10, cos(w1) * 0.08);
    if (quality >= 3.0) {
        let w2 = position.x * 1.72 - position.y * 1.12 + phase * 3.0;
        let w3 = position.x * -2.30 + position.y * 1.91 - phase * 4.0;
        slope += vec2<f32>(cos(w2), sin(w3)) * 0.035;
    }
    return normalize(vec3<f32>(-slope.x, 1.0, -slope.y));
}
fn water_body_tint(day: f32, twilight: f32) -> vec3<f32> {
    // Matches the built-in underwater atmosphere's overworld teal at noon,
    // shifting smoothly toward a cool silver-blue body tint at night.
    let underwater_teal = vec3<f32>(0.10, 0.30, 0.42);
    let moon_blue = vec3<f32>(0.055, 0.105, 0.20);
    let dusk_teal = vec3<f32>(0.16, 0.24, 0.34);
    let time_tint = mix(mix(moon_blue, underwater_teal, day), dusk_teal, twilight * 0.55);
    // camera_right is unused by this water pass; the renderer places the
    // same biome depth tint/filter here that underwater fog applies.
    let biome_filter = mix(vec3<f32>(1.0), shadow.camera_right.xyz,
                           clamp(shadow.camera_right.w, 0.0, 1.0));
    return time_tint * biome_filter;
}
fn water_cloud_openness(world_xz: vec2<f32>, phase: f32) -> f32 {
    let p = world_xz * 0.018 + vec2<f32>(phase * 0.009, phase * 0.004);
    let a = sin(dot(p, vec2<f32>(1.31, 0.73))) * cos(dot(p, vec2<f32>(-0.49, 1.17)));
    let q = world_xz * 0.044 + vec2<f32>(-phase * 0.005, phase * 0.003);
    let b = sin(dot(q, vec2<f32>(2.47, -1.81))) * cos(dot(q, vec2<f32>(1.63, 2.09)));
    return 1.0 - smoothstep(-0.20, 0.62, 0.68 * a + 0.32 * b);
}
fn water_rain_ripple(world_xz: vec2<f32>, time: f32, wetness: f32) -> f32 {
    let p = world_xz * 0.72 + vec2<f32>(time * 0.031, -time * 0.021);
    let cell = floor(p);
    let seed = fract(sin(dot(cell, vec2<f32>(17.13, 91.77))) * 31313.7);
    let center = vec2<f32>(fract(seed * 17.0), fract(seed * 71.0));
    let local = fract(p) - center;
    let age = fract(time * 0.05 + seed);
    let radius = age * 0.58;
    let ring = 1.0 - smoothstep(0.025, 0.07, abs(length(local) - radius));
    return wetness * ring * (1.0 - age);
}
@fragment
fn fs_main(input: WaterOutput) -> @location(0) vec4<f32> {
    let quality = shadow.render_params.x;
    let day = smoothstep(-0.12, 0.12, shadow.sun_direction.y);
    let twilight = 1.0 - smoothstep(0.0, 0.28, abs(shadow.sun_direction.y));
    let depth = clamp(input.depth_hint, 0.0, 1.0);
    let phase = shadow.render_params.y * 6.2831853;
    let flow = vec2<f32>(0.78, 0.31);
    let flowed_position = input.world_position.xz + flow * phase * 1.7;
    let normal = wave_normal(flowed_position, phase, quality);
    var rain_rings = 0.0;
    if (quality >= 2.0) {
        let sky_exposure = smoothstep(0.85, 1.0, 1.0 - depth);
        rain_rings = water_rain_ripple(input.world_position.xz,
            shadow.render_params.y * 24000.0,
            clamp(shadow.sun_direction.w, 0.0, 1.0) * sky_exposure);
    }
    let to_camera = shadow.camera_position.xyz - input.world_position;
    let view = to_camera / max(length(to_camera), 0.0001);
    let reflected = reflect(-view, normal);
    let absorption = exp(-depth * vec3<f32>(2.25, 1.18, 0.62));
    let body_tint = water_body_tint(day, twilight);
    let seabed = vec3<f32>(0.12, 0.36, 0.31)
        * (0.42 + 0.58 * max(shadow.sun_direction.y, 0.0));
    var color = seabed * absorption + body_tint * (vec3<f32>(1.0) - absorption);
    // Suigetsu: moonlight gathers into a rippling silver-blue path after sunset.
    var suigetsu = vec3<f32>(0.0);
    if (shadow.sun_direction.y < 0.0) {
        let moon_axis = -normalize(shadow.sun_direction.xyz);
        let lunar_path = pow72(max(dot(reflected, moon_axis), 0.0));
        let ripple = 0.70 + 0.30
            * sin(input.world_position.x * 0.83 + phase * 1.8)
            * sin(input.world_position.z * 0.91 - phase * 1.4);
        let rise = smoothstep(0.0, 0.16, -shadow.sun_direction.y);
        suigetsu = vec3<f32>(0.36, 0.45, 0.62) * lunar_path * ripple
            * rise * shadow.moon_color.w;
    }
    if (quality <= 1.0) {
        return vec4<f32>(color + suigetsu, 0.88);
    }
    // Schlick dielectric Fresnel retains low reflectance face-on and a
    // strong sky reflection at grazing angles.
    let fresnel = 0.02 + 0.98 * pow5(1.0 - clamp(dot(normal, view), 0.0, 1.0));
    color += vec3<f32>(0.10, 0.15, 0.18) * rain_rings * 0.045;
    let sun_warmth = (1.0 - smoothstep(0.06, 0.46, max(shadow.sun_direction.y, 0.0))) * day;
    let sun_temperature = mix(vec3<f32>(1.0, 0.99, 0.96),
                              vec3<f32>(1.0, 0.64, 0.38), sun_warmth);
    let cloud_open = water_cloud_openness(input.world_position.xz, phase);
    let day_sky = mix(vec3<f32>(0.18, 0.38, 0.62),
                      vec3<f32>(0.42, 0.56, 0.70), twilight);
    let night_sky = vec3<f32>(0.035, 0.065, 0.13) + shadow.moon_color.xyz * 0.22;
    let sky = mix(night_sky, day_sky * sun_temperature, day);
    let sky_reflection = mix(sky * 0.58, sky, clamp(reflected.y, 0.0, 1.0))
        * mix(0.72, 1.0, cloud_open) * (1.0 - shadow.celestial_params.w * 0.55);
    let half_vector = view + normalize(shadow.sun_direction.xyz);
    let halfway = half_vector / max(length(half_vector), 0.0001);
    let sparkle = pow180(max(dot(normal, halfway), 0.0));
    let moon_reflection = shadow.moon_color.xyz * pow72(max(dot(reflected,
        -normalize(shadow.sun_direction.xyz)), 0.0)) * (1.0 - day);
    color = mix(color, sky_reflection, fresnel)
            + shadow.sun_color.xyz * sun_temperature * sparkle * day * 0.8
            + moon_reflection + suigetsu;
    if (quality >= 3.0) {
        let caustic_a = sin(dot(flowed_position, vec2<f32>(1.7, 1.15)) + phase * 2.2)
            * sin(dot(flowed_position, vec2<f32>(-1.1, 1.9)) - phase * 1.4);
        let caustic_b = sin(dot(flowed_position * 1.83, vec2<f32>(1.2, -1.7)) - phase * 2.8)
            * sin(dot(flowed_position * 1.83, vec2<f32>(1.9, 0.8)) + phase * 1.9);
        let web_a = pow3(max(caustic_a, 0.0));
        let web_b = pow3(max(caustic_b, 0.0));
        let caustic = (0.62 * web_a + 0.38 * web_b) * (1.0 - depth)
            * max(shadow.sun_direction.y, 0.0) * cloud_open;
        color += vec3<f32>(0.05, 0.15, 0.13) * caustic;
        // The water stream exposes sky-light depth rather than a scene-depth
        // texture. This narrow, soft band uses that existing shallow-depth
        // proxy for restrained edge foam without adding a binding.
        let shallow_edge = exp(-abs(depth - 0.12) * 28.0);
        let foam_breakup = smoothstep(0.05, 0.72,
            sin(dot(flowed_position, vec2<f32>(3.1, 2.7)))
            * sin(dot(flowed_position, vec2<f32>(-2.4, 3.6))));
        let foam = shallow_edge * foam_breakup * 0.12;
        color = mix(color, vec3<f32>(0.84, 0.91, 0.94), foam);
    }
    return vec4<f32>(pow(max(color, vec3<f32>(0.0)),
                         vec3<f32>(1.0 / 2.2)), 0.93);
}
"#;

/// Edge-adaptive 5-tap spatial sharpening for a fixed render-scale preset.
/// A fullscreen triangle upscales the sub-native scene target; sharpening
/// is gated by local luma contrast and halo-clamped so it crisps block
/// edges without ringing. The UI stays at native 100% on top.
pub const MEGU_SPATIAL_SHARPEN_WGSL: &str = r#"
struct SharpenUniforms {
    // xy: 1.0 / scene target size, z: strength, w: halo clamp.
    texel_strength: vec4<f32>,
};
@group(0) @binding(0) var<uniform> sharpen: SharpenUniforms;
@group(0) @binding(1) var scene_tex: texture_2d<f32>;
@group(0) @binding(2) var scene_sampler: sampler;

struct SharpenOutput {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs_main(@builtin(vertex_index) vertex_index: u32) -> SharpenOutput {
    let points = array<vec2<f32>, 3>(
        vec2<f32>(-1.0, -1.0), vec2<f32>(3.0, -1.0), vec2<f32>(-1.0, 3.0)
    );
    var out: SharpenOutput;
    let clip = points[vertex_index];
    out.position = vec4<f32>(clip, 0.0, 1.0);
    out.uv = clip * vec2<f32>(0.5, -0.5) + vec2<f32>(0.5, 0.5);
    return out;
}

fn luma(c: vec3<f32>) -> f32 {
    return dot(c, vec3<f32>(0.2126, 0.7152, 0.0722));
}

@fragment
fn fs_main(input: SharpenOutput) -> @location(0) vec4<f32> {
    let texel = sharpen.texel_strength.xy;
    let strength = sharpen.texel_strength.z;
    let halo = sharpen.texel_strength.w;
    let center = textureSampleLevel(scene_tex, scene_sampler,
                                    input.uv, 0.0).rgb;
    let north = textureSampleLevel(scene_tex, scene_sampler,
        input.uv + vec2<f32>(0.0, -texel.y), 0.0).rgb;
    let south = textureSampleLevel(scene_tex, scene_sampler,
        input.uv + vec2<f32>(0.0, texel.y), 0.0).rgb;
    let west = textureSampleLevel(scene_tex, scene_sampler,
        input.uv + vec2<f32>(-texel.x, 0.0), 0.0).rgb;
    let east = textureSampleLevel(scene_tex, scene_sampler,
        input.uv + vec2<f32>(texel.x, 0.0), 0.0).rgb;
    let cross_avg = (north + south + west + east) * 0.25;
    // High-contrast neighborhoods already look sharp; backing off there
    // removes halos while flat areas still recover upscaled detail.
    let contrast = abs(luma(cross_avg) - luma(center));
    let adaptive = max(strength, 0.0) / (1.0 + contrast * 12.0);
    var detail = (center - cross_avg) * adaptive;
    let cap = max(halo, 0.0);
    detail = clamp(detail, vec3<f32>(-cap), vec3<f32>(cap));
    let sharpened = center + detail;
    // Clamping into the 5-tap neighborhood range is a second halo guard:
    // the output can never overshoot past a color the scene already had.
    let lo = min(center, min(min(north, south), min(west, east)));
    let hi = max(center, max(max(north, south), max(west, east)));
    return vec4<f32>(clamp(sharpened, lo, hi), 1.0);
}
"#;

/// Lightweight alpha-test shadow depth caster for cutout blocks — window
/// glass panes, iron bars, trapdoors, flowers, and leaves. Transparent
/// texels `discard`, so sun- and moonlight pass through real openings
/// while frames, mullions and leaf texels still write depth. The optional
/// `sway_kind` vertex input (`0` = rigid) applies the same
/// [`crate::vanilla_plus_nature::MEGU_WIND_SWAY_WGSL_FN`] motion the main
/// pass uses, so leaf shadows sway in sync with the visible canopy — the
/// real-geometry komorebi source.
///
/// Bindings: `0/0` shared [`ShadowUniforms`], `1/1` section origin,
/// `3/0..3` atlas texture + sampler + rect/state storage buffers. The
/// renderer chooses the cascade layer via `shadow_params.w` exactly like
/// [`MEGU_SHADOW_DEPTH_WGSL`].
pub const MEGU_SHADOW_CUTOUT_DEPTH_WGSL: &str = r#"
struct ShadowUniforms {
    light_view_proj: array<mat4x4<f32>, 3>,
    camera_view_proj: mat4x4<f32>,
    cascade_splits: vec4<f32>,
    sun_direction: vec4<f32>,
    sun_color: vec4<f32>,
    moon_color: vec4<f32>,
    camera_position: vec4<f32>,
    camera_forward: vec4<f32>,
    camera_right: vec4<f32>,
    camera_up: vec4<f32>,
    shadow_params: vec4<f32>,
    render_params: vec4<f32>,
    celestial_params: vec4<f32>,
};
struct ShadowSection {
    origin: vec3<f32>,
    _pad0: f32,
    biome_tint: vec3<f32>,
    biome_tint_strength: f32,
};
// One slot per atlas sprite: `rect` is the active `.mcmeta` frame window
// (whole-strip rect for static sprites) and `anim` carries the next
// slot's top edge plus the `interpolate` blend factor.
struct SpriteFrame {
    rect: vec4<f32>,
    anim: vec4<f32>,
};
struct AtlasRects { rects: array<SpriteFrame> };
struct AtlasStates { sprites: array<u32> };
@group(0) @binding(0) var<uniform> shadow: ShadowUniforms;
@group(1) @binding(1) var<uniform> section: ShadowSection;
@group(3) @binding(0) var atlas_tex: texture_2d<f32>;
@group(3) @binding(1) var atlas_sampler: sampler;
@group(3) @binding(2) var<storage, read> atlas_rects: AtlasRects;
@group(3) @binding(3) var<storage, read> atlas_states: AtlasStates;

fn megu_wind_sway(local_pos: vec3<f32>, block_phase: f32, time_sec: f32,
                  strength: f32, kind: u32) -> vec3<f32> {
    let phase = time_sec * 1.4 + block_phase;
    let s0 = sin(phase);
    let s1 = sin(phase * 0.63 + 1.3);
    var attach = clamp(local_pos.y, 0.0, 1.0);
    var amp = strength * 0.12;
    if (kind == 2u) {
        attach = 1.0;
        amp = strength * 0.05;
    } else if (kind == 3u) {
        attach = clamp(1.0 - local_pos.y, 0.0, 1.0);
        amp = strength * 0.10;
    }
    if (kind == 2u) {
        return vec3<f32>(amp * sin(phase * 1.7 + local_pos.x * 3.1),
                         amp * 0.4 * cos(phase * 1.9 + local_pos.y * 2.3),
                         amp * sin(phase * 1.6 + local_pos.z * 2.7));
    }
    return vec3<f32>(amp * attach * s0, 0.0, amp * attach * s1 * 0.6);
}

// Per-block sway phase so neighbouring leaves never move in lockstep.
fn cutout_block_phase(block: vec3<f32>) -> f32 {
    return fract(sin(dot(floor(block), vec3<f32>(31.0, 17.0, 13.0)))
                 * 43758.55) * 6.2831853;
}

fn cutout_tiled_uv(p: vec3<f32>, direction: u32) -> vec2<f32> {
    if direction < 2u { return p.xz; }
    if direction < 4u { return p.xy; }
    return p.zy;
}

struct CutoutInput {
    @location(0) packed_pos: u32,
    @location(1) state_id: u32,
    @location(2) attributes: u32,
    // 0 = rigid (panes, bars); 1/2/3 = the WindSwayCategory discriminants
    // (grass / leaves / hanging) shared with the main wind pass.
    @location(3) sway_kind: u32,
};
struct CutoutOutput {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
    @location(1) @interpolate(flat) sprite_id: u32,
};

@vertex
fn vs_cutout(input: CutoutInput) -> CutoutOutput {
    let local = vec3<f32>(f32(input.packed_pos & 31u),
        f32(input.packed_pos >> 10u) / 1024.0,
        f32((input.packed_pos >> 5u) & 31u));
    var world = section.origin + local;
    // Optional wind sway: rigid casters (kind 0) skip the offset entirely,
    // so window glass stays fixed while leaves rustle the sun's shadow.
    if (input.sway_kind != 0u) {
        let wind_time = shadow.render_params.y * 1200.0;
        world = world + megu_wind_sway(local, cutout_block_phase(world),
                                       wind_time, 0.35, input.sway_kind);
    }
    var out: CutoutOutput;
    out.position = shadow.light_view_proj[
        min(u32(shadow.shadow_params.w), 2u)] * vec4<f32>(world, 1.0);
    out.uv = cutout_tiled_uv(local, input.attributes & 0x7u);
    out.sprite_id = input.state_id;
    return out;
}

fn sample_atlas_alpha(sprite_id: u32, uv: vec2<f32>) -> f32 {
    if (sprite_id >= arrayLength(&atlas_states.sprites)) {
        return 1.0;
    }
    let mapped = atlas_states.sprites[sprite_id];
    if (mapped >= arrayLength(&atlas_rects.rects)) {
        return 1.0;
    }
    let frame = atlas_rects.rects[mapped];
    let rect = frame.rect;
    var alpha = textureSampleLevel(atlas_tex, atlas_sampler,
        rect.xy + fract(uv) * rect.zw, 0.0).a;
    // Interpolated `.mcmeta` sprites blend the next strip slot's alpha so
    // the cutout mask matches the frame the colour pass renders.
    if (frame.anim.y > 0.0) {
        let next = textureSampleLevel(atlas_tex, atlas_sampler,
            vec2<f32>(rect.x + fract(uv.x) * rect.z,
                      frame.anim.x + fract(uv.y) * rect.w), 0.0).a;
        alpha = mix(alpha, next, frame.anim.y);
    }
    return alpha;
}

@fragment
fn fs_cutout(input: CutoutOutput) {
    let atlas_alpha = sample_atlas_alpha(input.sprite_id, input.uv);
    if (atlas_alpha < 0.5) { discard; }
}
"#;

/// Lightweight color-transmission shadow caster for stained glass. It
/// renders into a small `Rgba8Unorm` shadow-color map under multiplicative
/// blending (`Dst` factor), so layered panes compose physically and sun-
/// or moonlight arrives tinted on interior floors and walls. The pass
/// runs only when a stained-glass caster exists in the covered cascades —
/// with none present its cost is exactly zero.
///
/// Bindings: `0/0` shared [`ShadowUniforms`]; the color index arrives as a
/// flat vertex attribute following the vanilla `DyeColor` order.
pub const MEGU_STAINED_GLASS_SHADOW_COLOR_WGSL: &str = r#"
struct ShadowUniforms {
    light_view_proj: array<mat4x4<f32>, 3>,
    camera_view_proj: mat4x4<f32>,
    cascade_splits: vec4<f32>,
    sun_direction: vec4<f32>,
    sun_color: vec4<f32>,
    moon_color: vec4<f32>,
    camera_position: vec4<f32>,
    camera_forward: vec4<f32>,
    camera_right: vec4<f32>,
    camera_up: vec4<f32>,
    shadow_params: vec4<f32>,
    render_params: vec4<f32>,
    celestial_params: vec4<f32>,
};
@group(0) @binding(0) var<uniform> shadow: ShadowUniforms;

// Multiplicative light transmission per vanilla dye index (0 = white ..
// 15 = black); the CPU mirror is `stained_glass_transmission_rgb`.
fn stained_glass_transmission_rgb(color_idx: u32) -> vec3<f32> {
    let table = array<vec3<f32>, 16>(
        vec3<f32>(0.97, 0.97, 0.97), // white
        vec3<f32>(0.95, 0.52, 0.14), // orange
        vec3<f32>(0.86, 0.30, 0.80), // magenta
        vec3<f32>(0.36, 0.68, 0.92), // light blue
        vec3<f32>(0.98, 0.84, 0.16), // yellow
        vec3<f32>(0.52, 0.86, 0.14), // lime
        vec3<f32>(0.95, 0.60, 0.72), // pink
        vec3<f32>(0.34, 0.36, 0.38), // gray
        vec3<f32>(0.64, 0.66, 0.68), // light gray
        vec3<f32>(0.13, 0.60, 0.62), // cyan
        vec3<f32>(0.56, 0.26, 0.76), // purple
        vec3<f32>(0.22, 0.32, 0.86), // blue
        vec3<f32>(0.46, 0.29, 0.17), // brown
        vec3<f32>(0.36, 0.56, 0.13), // green
        vec3<f32>(0.86, 0.14, 0.13), // red
        vec3<f32>(0.09, 0.09, 0.10)  // black
    );
    return table[min(color_idx, 15u)];
}

struct StainedInput {
    @location(0) world_position: vec3<f32>,
    @location(1) color_idx: u32,
};
struct StainedOutput {
    @builtin(position) position: vec4<f32>,
    @location(0) @interpolate(flat) color_idx: u32,
};

@vertex
fn vs_stained(input: StainedInput) -> StainedOutput {
    var out: StainedOutput;
    // The same light-space projection as the depth casters, so the
    // transmission tint lands exactly where the depth map says light was
    // blocked.
    out.position = shadow.light_view_proj[
        min(u32(shadow.shadow_params.w), 2u)]
        * vec4<f32>(input.world_position, 1.0);
    out.color_idx = input.color_idx;
    return out;
}

@fragment
fn fs_stained(input: StainedOutput) -> @location(0) vec4<f32> {
    // Rgba8Unorm target under multiplicative blending: each fragment
    // multiplies the running transmission colour.
    return vec4<f32>(stained_glass_transmission_rgb(input.color_idx), 1.0);
}
"#;

/// Optional shadow casters published alongside the builtin pack. The five
/// [`BUILTIN_SHADOW_SHADERS`] passes are always present; these two are
/// wired only when the scene actually has cutout or stained-glass casters,
/// so they cost nothing otherwise.
pub const BUILTIN_SHADOW_CASTER_SHADERS: [(&str, &str); 2] = [
    ("shadow_cutout_depth.wgsl", MEGU_SHADOW_CUTOUT_DEPTH_WGSL),
    (
        "shadow_stained_glass_color.wgsl",
        MEGU_STAINED_GLASS_SHADOW_COLOR_WGSL,
    ),
];

/// File names and sources accepted by the native shaderpack loader.
pub const BUILTIN_SHADOW_SHADERS: [(&str, &str); 5] = [
    ("shadow_depth.wgsl", MEGU_SHADOW_DEPTH_WGSL),
    ("terrain_lit.wgsl", MEGU_TERRAIN_LIT_WGSL),
    ("sky_atmosphere.wgsl", MEGU_SKY_ATMOSPHERE_WGSL),
    ("water_surface.wgsl", MEGU_WATER_SURFACE_WGSL),
    ("spatial_sharpen.wgsl", MEGU_SPATIAL_SHARPEN_WGSL),
];

/// Existing native loader schema, with metadata for the selected preset.
/// Manifest generation alone performs no GPU work, including for Off.
pub fn builtin_megu_shader_toml(quality: BuiltinShadowQuality) -> String {
    format!(
        r#"[pack]
name = "Megu Enhanced"
author = "Megu"
version = "0.1.0"
api_version = 1
requires_compute = false
requires_ssbo = false
supports_lod = false
quality = "{}"

[pass.shadow_depth]
stage = "shadow"
kind = "graphics"
file = "shadow_depth.wgsl"

[pass.terrain_lit]
stage = "gbuffers"
kind = "graphics"
file = "terrain_lit.wgsl"
targets = ["colortex0"]

[pass.sky_atmosphere]
stage = "begin"
kind = "graphics"
file = "sky_atmosphere.wgsl"
targets = ["colortex0"]

[pass.water_surface]
stage = "translucent"
kind = "graphics"
file = "water_surface.wgsl"
targets = ["colortex0"]

[pass.spatial_sharpen]
stage = "final"
kind = "graphics"
file = "spatial_sharpen.wgsl"
targets = ["colortex0"]
"#,
        quality.as_str()
    )
}

#[cfg(test)]
mod realism_wgsl_tests {
    use super::{
        compute_cloud_shadow_attenuation, compute_sky_rain_ripple, compute_sky_rain_wetness,
    };

    #[test]
    fn rain_wetness_and_ripples_require_open_sky_and_rain() {
        assert_eq!(compute_sky_rain_wetness(1.0, 0.0, 1.0), 0.0);
        assert_eq!(
            compute_sky_rain_ripple([12.5, -4.0], 3.0, 1.0, 0.0, 1.0),
            0.0
        );
        assert_eq!(compute_sky_rain_wetness(0.69, 1.0, 1.0), 0.0);
        assert_eq!(compute_sky_rain_wetness(1.0, 1.0, 0.0), 0.0);
        assert!(compute_sky_rain_wetness(1.0, 1.0, 1.0) > 0.0);
        assert!(compute_sky_rain_ripple([2.0, 5.0], 3.0, 1.0, 1.0, 1.0) > 0.0);
    }

    #[test]
    fn cloud_shadow_is_bounded_weather_responsive_and_smooth() {
        let position = [317.0, -81.0];
        let clear = compute_cloud_shadow_attenuation(position, 0.4, 1.0, 0.0);
        let cloudy = compute_cloud_shadow_attenuation(position, 0.4, 1.0, 1.0);
        let nearby = compute_cloud_shadow_attenuation(
            [position[0] + 0.01, position[1] - 0.01],
            0.4,
            1.0,
            1.0,
        );
        assert_eq!(clear, 1.0);
        assert!(cloudy < clear);
        assert!((0.35..=1.0).contains(&cloudy));
        assert!((cloudy - nearby).abs() < 0.01);
        assert_eq!(
            compute_cloud_shadow_attenuation(position, 0.4, 0.0, 1.0),
            1.0
        );
        for x in -32..32 {
            let factor = compute_cloud_shadow_attenuation(
                [x as f32 * 7.3, x as f32 * -3.1],
                x as f32 * 0.037,
                1.0,
                1.0,
            );
            assert!((0.35..=1.0).contains(&factor));
        }
    }
}
