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

/// Vanilla 8-phase lunar cycle: `moon_phase` wraps modulo 8 like the vanilla
/// `level.getMoonPhase()` (0 = full moon, 4 = new moon).
pub fn compute_celestial_lighting(
    world_time_ticks: u64,
    moon_phase: u8,
    eclipse: EclipseMode,
) -> CelestialLightingState {
    const PHASE_LIGHT: [f32; 8] = [1.0, 0.78, 0.55, 0.32, 0.06, 0.32, 0.55, 0.78];
    let phase_strength = PHASE_LIGHT[(moon_phase % 8) as usize];
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
    // The solar arc lies in XY, so Z is a continuous light-space reference
    // even at noon and midnight. Switching axes here would rotate shadows.
    let light_right = normalize(cross([0.0, 0.0, 1.0], sun), [1.0, 0.0, 0.0]);
    let light_up = cross(sun, light_right);
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
        let cz = snap(dot(center, sun));
        let depth_radius = radius * 4.0;
        let inverse_depth = 1.0 / (2.0 * depth_radius);
        matrices[index] = [
            [
                light_right[0] / radius,
                light_up[0] / radius,
                -sun[0] * inverse_depth,
                0.0,
            ],
            [
                light_right[1] / radius,
                light_up[1] / radius,
                -sun[1] * inverse_depth,
                0.0,
            ],
            [
                light_right[2] / radius,
                light_up[2] / radius,
                -sun[2] * inverse_depth,
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
    let moon_strength = (1.0 - daylight) * celestial.moon_illuminance;
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
};
struct TerrainOutput {
    @builtin(position) position: vec4<f32>,
    @location(0) world_position: vec3<f32>,
    @location(1) normal: vec3<f32>,
    @location(2) albedo: vec3<f32>,
};
@vertex
fn vs_main(input: TerrainInput) -> TerrainOutput {
    var out: TerrainOutput;
    out.position = shadow.camera_view_proj * vec4<f32>(input.world_position, 1.0);
    out.world_position = input.world_position;
    out.normal = input.normal;
    out.albedo = input.albedo;
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
fn sample_cascade(position: vec3<f32>, normal: vec3<f32>, cascade: u32) -> f32 {
    let quality = u32(shadow.render_params.x);
    // A solar eclipse softens penumbrae by widening the filter footprint.
    let texel = (1.0 + shadow.celestial_params.w * 1.5)
        / max(shadow.shadow_params.x, 1.0);
    let incidence = abs(dot(normal, normalize(shadow.sun_direction.xyz)));
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
    let rotation = fract(sin(dot(position.xz, vec2<f32>(12.73, 79.41))) * 4341.21)
        * 6.2831853;
    let c = cos(rotation);
    let s = sin(rotation);
    var visibility = 0.0;
    for (var i = 0u; i < taps; i = i + 1u) {
        var offset: vec2<f32>;
        if (quality <= 1u) {
            offset = vec2<f32>(f32(i % 2u), f32(i / 2u)) * 2.0 - vec2<f32>(1.0);
        } else if (quality == 2u) {
            offset = poisson(i);
        } else {
            let angle = f32(i) * 2.3999632 + rotation;
            let radius = sqrt((f32(i) + 0.5) / f32(taps));
            offset = radius * vec2<f32>(cos(angle), sin(angle)) * 1.8;
        }
        if (quality == 2u) {
            offset = vec2<f32>(offset.x * c - offset.y * s,
                               offset.x * s + offset.y * c);
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
    if (cascade_count > 0u) {
        var layer = 0u;
        if (cascade_count > 1u && view_depth > shadow.cascade_splits.x) {
            layer = 1u;
        }
        if (cascade_count > 2u && view_depth > shadow.cascade_splits.y) {
            layer = 2u;
        }
        visibility = sample_cascade(input.world_position, normal, layer);
    }
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
    let ambient = max((sky_ambient * shadow_sky_scale
                       + bounce * warm_bounce_scale) * ambient_dim,
                      vec3<f32>(0.065));
    // Komorebi travels with the daylight across partially shaded block faces.
    var komorebi = 1.0;
    if (visibility < 0.95 || cascade_count == 0u) {
        komorebi = komorebi_dapple(input.world_position.xz, shadow.render_params.y,
                                   shadow.sun_direction.y);
    }
    let light = ambient + shadow.sun_color.xyz * sun_power * visibility * komorebi
                + shadow.moon_color.xyz * moon_power * shadow.moon_color.w
                  * lunar_visibility;
    let view_dir = -view_offset / max(view_distance, 0.0001);
    // Nure-iro: damp upward stone and timber retain a restrained sky glint.
    let wetness = clamp(shadow.render_params.z * 24.0, 0.0, 1.0)
        * smoothstep(0.15, 0.85, normal.y);
    let wet_luma = dot(input.albedo, vec3<f32>(0.2126, 0.7152, 0.0722));
    let rich_albedo = clamp(input.albedo
        * (vec3<f32>(1.0) + (input.albedo - vec3<f32>(wet_luma)) * (0.08 * wetness))
        * (1.0 - 0.18 * wetness), vec3<f32>(0.0), vec3<f32>(1.0));
    let grazing_sheen = pow(1.0 - clamp(dot(normal, view_dir), 0.0, 1.0), 4.0)
        * 0.12 * wetness * max(normal.y, 0.0);
    // Quiet the darkest saturated materials before warm lamplight returns.
    var lit_color = rich_albedo * light;
    let luma = dot(lit_color, vec3<f32>(0.299, 0.587, 0.114));
    let shibumi = (1.0 - visibility) * 0.22;
    lit_color = mix(lit_color, vec3<f32>(luma) * vec3<f32>(0.96, 0.95, 1.02), shibumi);
    var lantern_light = vec3<f32>(0.0);
    var lacquer_sheen = vec3<f32>(0.0);
    let inei_boost = mix(1.38, 1.0,
                         visibility * clamp(shadow.sun_direction.y * 1.5, 0.0, 1.0));
    var limit = 8u;
    if (shadow.render_params.x >= 2.0) { limit = 32u; }
    if (shadow.render_params.x >= 3.0) { limit = 64u; }
    let light_limit = min(dynamic_lights.light_count.x, limit);
    for (var i = 0u; i < light_limit; i = i + 1u) {
        let point = dynamic_lights.lights[i];
        let displacement = point.position_radius.xyz - input.world_position;
        let radius = max(point.position_radius.w, 0.01);
        let distance_sq = max(dot(displacement, displacement), 0.0001);
        let distance = sqrt(distance_sq);
        let falloff = pow(max(1.0 - distance / radius, 0.0), 2.0);
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
                         * pow(max(dot(normal, half_vec), 0.0), 12.0) * 0.08;
    }
    lit_color += rich_albedo * lantern_light + lacquer_sheen
                 + sky_ambient * grazing_sheen;
    let atmosphere = yugen_valley_mist(input.world_position.y, shadow.camera_position.y,
                                       view_distance, shadow.render_params.z,
                                       shadow.render_params.y);
    let daylight = smoothstep(-0.12, 0.12, shadow.sun_direction.y);
    let twilight = 1.0 - smoothstep(0.0, 0.28, abs(shadow.sun_direction.y));
    let gaze = view_offset / max(view_distance, 0.0001);
    let horizon_mist = pow(1.0 - abs(gaze.y), 3.0);
    let toward_sun = pow(max(dot(gaze, to_sun), 0.0), 4.0);
    let indigo_fog = mix(vec3<f32>(0.075, 0.095, 0.17),
                         vec3<f32>(0.50, 0.59, 0.68), daylight);
    let mist = twilight * horizon_mist;
    let fog = mix(indigo_fog, vec3<f32>(0.28, 0.22, 0.25), mist * 0.25)
              + vec3<f32>(0.18, 0.075, 0.04) * mist * toward_sun;
    var color = mix(lit_color, fog, atmosphere);
    if (shadow.render_params.x >= 3.0) {
        // Screen-direction sun-shaft hint, without a volumetric buffer.
        let toward = view_offset / max(view_distance, 0.0001);
        let beam = pow(max(dot(toward, to_sun), 0.0), 24.0);
        color += shadow.sun_color.xyz * beam * atmosphere * 0.12;
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
    let horizon = pow(1.0 - abs(view.y), 3.0);
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
        let mie = (1.0 - mie_g * mie_g) / pow(denom, 1.5);
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
            if (roll > 0.72) {
                let fade = smoothstep(0.04, 0.18, view.y)
                    * (1.0 - smoothstep(0.45, 0.75, view.y));
                let lit = mix(mix(vec3<f32>(0.10, 0.12, 0.19),
                                  vec3<f32>(0.82, 0.82, 0.86), day),
                              vec3<f32>(0.28, 0.24, 0.30), twilight * 0.7);
                color = mix(color, lit, fade * 0.55);
            }
        }
    }
    let toward_sun = pow(max(mu, 0.0), 4.0);
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
        let ring = pow(max(mu, 0.0), 260.0);
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
@fragment
fn fs_main(input: WaterOutput) -> @location(0) vec4<f32> {
    let quality = shadow.render_params.x;
    let day = smoothstep(-0.12, 0.12, shadow.sun_direction.y);
    let depth = clamp(input.depth_hint, 0.0, 1.0);
    let shallow = vec3<f32>(0.04, 0.32, 0.37);
    let deep = vec3<f32>(0.012, 0.065, 0.15);
    var color = mix(shallow, deep, depth);
    let phase = shadow.render_params.y * 6.2831853;
    let normal = wave_normal(input.world_position.xz, phase, quality);
    let to_camera = shadow.camera_position.xyz - input.world_position;
    let view = to_camera / max(length(to_camera), 0.0001);
    let reflected = reflect(-view, normal);
    // Suigetsu: moonlight gathers into a rippling silver-blue path after sunset.
    var suigetsu = vec3<f32>(0.0);
    if (shadow.sun_direction.y < 0.0) {
        let moon_axis = -normalize(shadow.sun_direction.xyz);
        let lunar_path = pow(max(dot(reflected, moon_axis), 0.0), 72.0);
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
    let fresnel = 0.02 + 0.98 * pow(1.0 - max(dot(normal, view), 0.0), 5.0);
    let sky = mix(vec3<f32>(0.02, 0.035, 0.085),
                  vec3<f32>(0.18, 0.44, 0.78), day);
    let sky_reflection = mix(sky * 0.6, sky, clamp(reflected.y, 0.0, 1.0))
        * (1.0 - shadow.celestial_params.w * 0.55);
    let half_vector = view + shadow.sun_direction.xyz;
    let halfway = half_vector / max(length(half_vector), 0.0001);
    let sparkle = pow(max(dot(normal, halfway), 0.0), 180.0);
    color = mix(color, sky_reflection, fresnel)
            + shadow.sun_color.xyz * sparkle * day * 0.8 + suigetsu;
    if (quality >= 3.0) {
        let ripple = sin(input.world_position.x * 1.7 + phase * 3.0)
                     * sin(input.world_position.z * 1.9 - phase * 2.0);
        let caustic = pow(max(ripple, 0.0), 3.0) * (1.0 - depth);
        color += vec3<f32>(0.018, 0.06, 0.075) * caustic;
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
