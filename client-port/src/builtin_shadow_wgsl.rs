// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later

//! Optional, clean-room Megu Enhanced shaders and deterministic CPU math.
//! The default is Off, and this module never allocates or submits GPU work.

use crate::gpu_pipeline::{GpuCapabilities, GpuVendor};
use crate::vfx_engine::DynamicColoredLight;
use std::f32::consts::{PI, TAU};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, PartialOrd, Ord)]
pub enum BuiltinShadowQuality {
    #[default]
    Off,
    Low,
    Medium,
    High,
}

impl BuiltinShadowQuality {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Low => "low",
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
        }
    }

    pub const fn config(self) -> CascadeConfig {
        match self {
            Self::Off => CascadeConfig::new(self, 0, 0, 0, 0),
            Self::Low => CascadeConfig::new(self, 1, 1024, 4, 8),
            Self::Medium => CascadeConfig::new(self, 2, 2048, 9, 32),
            Self::High => CascadeConfig::new(self, 3, 2048, 16, 64),
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
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CascadeConfig {
    pub quality: BuiltinShadowQuality,
    pub cascade_count: u32,
    pub map_resolution: u32,
    pub pcf_taps: u32,
    pub max_dynamic_lights: u32,
}

impl CascadeConfig {
    const fn new(
        quality: BuiltinShadowQuality,
        cascade_count: u32,
        map_resolution: u32,
        pcf_taps: u32,
        max_dynamic_lights: u32,
    ) -> Self {
        Self {
            quality,
            cascade_count,
            map_resolution,
            pcf_taps,
            max_dynamic_lights,
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
    /// Quality index, tick-based day phase, fog density, exposure.
    pub render_params: [f32; 4],
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
pub fn build_shadow_uniforms(
    camera: ShadowCamera,
    config: CascadeConfig,
    world_time_ticks: u64,
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

    let daylight = ((sun[1] + 0.12) / 0.24).clamp(0.0, 1.0);
    let twilight = (1.0 - (sun[1].abs() / 0.25).min(1.0)) * 0.35;
    let day_phase = (world_time_ticks % 24_000) as f32 / 24_000.0;
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
            (0.95 + twilight) * daylight,
            (0.92 - twilight * 0.3) * daylight,
            (0.85 - twilight * 0.6) * daylight,
            1.0,
        ],
        moon_color: [0.12, 0.17, 0.31, 1.0 - daylight],
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
        render_params: [config.quality.index() as f32, day_phase, 0.012, 1.0],
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
    let texel = 1.0 / max(shadow.shadow_params.x, 1.0);
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
    var light = sky_ambient + bounce + shadow.sun_color.xyz * sun_power * visibility
                + shadow.moon_color.xyz * moon_power * shadow.moon_color.w;
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
        let diffuse = max(dot(normal, displacement / distance), 0.0);
        light += point.color_intensity.xyz * point.color_intensity.w
                 * diffuse * falloff;
    }
    let atmosphere = 1.0 - exp(-view_distance * shadow.render_params.z);
    let fog = mix(vec3<f32>(0.23, 0.29, 0.41),
                  vec3<f32>(0.58, 0.72, 0.95),
                  clamp(shadow.sun_direction.y * 0.7 + 0.35, 0.0, 1.0));
    var color = mix(input.albedo * light, fog, atmosphere);
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
/// warm twilight, sun disc, moon disc and an inexpensive Low gradient.
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
    let night = vec3<f32>(0.006, 0.014, 0.034);
    let zenith = mix(night, vec3<f32>(0.05, 0.22, 0.52), day);
    let pale = mix(vec3<f32>(0.02, 0.035, 0.08),
                   vec3<f32>(0.46, 0.63, 0.92), day);
    var color = mix(zenith, pale, clamp(horizon * 0.85, 0.0, 1.0));
    if (shadow.render_params.x >= 2.0) {
        let rayleigh = (1.0 + mu * mu) * 0.75;
        let mie_g = 0.72;
        let denom = max(1.0 + mie_g * mie_g - 2.0 * mie_g * mu, 0.01);
        let mie = (1.0 - mie_g * mie_g) / pow(denom, 1.5);
        color += vec3<f32>(0.025, 0.072, 0.15) * rayleigh * day;
        color += vec3<f32>(0.10, 0.075, 0.045) * mie * day * 0.1;
    }
    let twilight = 1.0 - smoothstep(0.0, 0.28, abs(sun.y));
    let toward_sun = pow(max(mu, 0.0), 12.0);
    color += vec3<f32>(0.66, 0.18, 0.045) * twilight * horizon * toward_sun;
    let sun_disc = smoothstep(0.9998, 0.99998, mu);
    let moon_disc = smoothstep(0.99984, 0.99996, dot(view, -sun));
    color += vec3<f32>(3.2, 2.55, 1.65) * day * sun_disc;
    color += vec3<f32>(0.55, 0.67, 0.88) * (1.0 - day) * moon_disc;
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
    if (quality <= 1.0) {
        return vec4<f32>(color, 0.88);
    }
    let phase = shadow.render_params.y * 6.2831853;
    let normal = wave_normal(input.world_position.xz, phase, quality);
    let to_camera = shadow.camera_position.xyz - input.world_position;
    let view = to_camera / max(length(to_camera), 0.0001);
    let fresnel = 0.02 + 0.98 * pow(1.0 - max(dot(normal, view), 0.0), 5.0);
    let reflected = reflect(-view, normal);
    let sky = mix(vec3<f32>(0.02, 0.035, 0.085),
                  vec3<f32>(0.18, 0.44, 0.78), day);
    let sky_reflection = mix(sky * 0.6, sky, clamp(reflected.y, 0.0, 1.0));
    let half_vector = view + shadow.sun_direction.xyz;
    let halfway = half_vector / max(length(half_vector), 0.0001);
    let sparkle = pow(max(dot(normal, halfway), 0.0), 180.0);
    color = mix(color, sky_reflection, fresnel)
            + shadow.sun_color.xyz * sparkle * day * 0.8;
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

/// File names and sources accepted by the native shaderpack loader.
pub const BUILTIN_SHADOW_SHADERS: [(&str, &str); 4] = [
    ("shadow_depth.wgsl", MEGU_SHADOW_DEPTH_WGSL),
    ("terrain_lit.wgsl", MEGU_TERRAIN_LIT_WGSL),
    ("sky_atmosphere.wgsl", MEGU_SKY_ATMOSPHERE_WGSL),
    ("water_surface.wgsl", MEGU_WATER_SURFACE_WGSL),
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
"#,
        quality.as_str()
    )
}
