// SPDX-License-Identifier: LGPL-3.0-or-later
//! Standalone textured world-environment passes.
//!
//! These passes own their textures, pipelines and GPU buffers independently of
//! [`crate::renderer::GpuRenderer`]. Construction may upload or replace asset
//! pixels; steady-state `encode` calls only write into preallocated buffers.

use bytemuck::{Pod, Zeroable};
use megu_client_mesh::builtin_shadow_wgsl::compute_sun_direction;

use crate::renderer::GameplayScreenOverlay;
use crate::vanilla_world_assets::{
    moon_phase_uv_rect, DecodedRgbaTexture, VanillaEnvTexture, VanillaMiscOverlay,
    VanillaWorldAssets,
};
use crate::Camera;

/// Depth format expected by [`TexturedCelestialWeatherPass`].
pub const WORLD_ENV_PASS_DEPTH_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Depth32Float;

/// Dimension id for the Overworld in [`CelestialWeatherFrame`].
pub const DIMENSION_KIND_OVERWORLD: u32 = 0;
/// Dimension id for the Nether in [`CelestialWeatherFrame`].
pub const DIMENSION_KIND_NETHER: u32 = 1;
/// Dimension id for The End in [`CelestialWeatherFrame`].
pub const DIMENSION_KIND_END: u32 = 2;

const TEXTURE_SUN: f32 = 0.0;
const TEXTURE_MOON: f32 = 1.0;
const TEXTURE_CLOUDS: f32 = 2.0;
const TEXTURE_RAIN: f32 = 3.0;
const TEXTURE_SNOW: f32 = 4.0;
const TEXTURE_END_SKY: f32 = 5.0;

const END_FACE_COUNT: usize = 6;
const CLOUD_QUAD_COUNT: usize = 1;
const PRECIPITATION_CURTAIN_COUNT: usize = 12;
const CELESTIAL_QUAD_COUNT: usize = 2;
const TOTAL_QUAD_COUNT: usize =
    END_FACE_COUNT + CLOUD_QUAD_COUNT + PRECIPITATION_CURTAIN_COUNT + CELESTIAL_QUAD_COUNT;
const TOTAL_VERTEX_COUNT: usize = TOTAL_QUAD_COUNT * 4;
const TOTAL_INDEX_COUNT: usize = TOTAL_QUAD_COUNT * 6;
const END_SKY_TILES: f32 = 3.0;

/// WGSL for the textured celestial/weather pass. Every environment texture is
/// sampled unconditionally and the per-primitive texture index selects the
/// result, keeping derivative-using texture lookups in uniform control flow.
pub const CELESTIAL_WEATHER_WGSL: &str = r#"
struct EnvironmentUniform {
    view_proj: mat4x4<f32>,
};

@group(0) @binding(0) var<uniform> environment: EnvironmentUniform;
@group(0) @binding(1) var sun_texture: texture_2d<f32>;
@group(0) @binding(2) var moon_texture: texture_2d<f32>;
@group(0) @binding(3) var clouds_texture: texture_2d<f32>;
@group(0) @binding(4) var rain_texture: texture_2d<f32>;
@group(0) @binding(5) var snow_texture: texture_2d<f32>;
@group(0) @binding(6) var end_sky_texture: texture_2d<f32>;
@group(0) @binding(7) var environment_sampler: sampler;

struct VertexOutput {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
    @location(1) alpha: f32,
    @location(2) @interpolate(flat) texture_index: u32,
};

@vertex
fn vs_main(
    @location(0) position: vec4<f32>,
    @location(1) data: vec4<f32>,
) -> VertexOutput {
    var out: VertexOutput;
    out.position = environment.view_proj * position;
    out.uv = data.xy;
    out.alpha = clamp(data.w, 0.0, 1.0);
    out.texture_index = u32(round(data.z));
    return out;
}

@fragment
fn fs_main(input: VertexOutput) -> @location(0) vec4<f32> {
    let sun = textureSample(sun_texture, environment_sampler, input.uv);
    let moon = textureSample(moon_texture, environment_sampler, input.uv);
    let clouds = textureSample(clouds_texture, environment_sampler, input.uv);
    let rain = textureSample(rain_texture, environment_sampler, input.uv);
    let snow = textureSample(snow_texture, environment_sampler, input.uv);
    let end_sky = textureSample(end_sky_texture, environment_sampler, input.uv);

    var color = sun;
    if (input.texture_index == 1u) {
        color = moon;
    }
    if (input.texture_index == 2u) {
        color = clouds;
    }
    if (input.texture_index == 3u) {
        color = rain;
    }
    if (input.texture_index == 4u) {
        color = snow;
    }
    if (input.texture_index == 5u) {
        color = end_sky;
    }
    return vec4<f32>(color.rgb, clamp(color.a * input.alpha, 0.0, 1.0));
}
"#;

/// WGSL for the textured full-screen gameplay overlay pass.
pub const TEXTURED_OVERLAY_WGSL: &str = r#"
struct OverlayUniform {
    effects_a: vec4<f32>,
    effects_b: vec4<f32>,
    screen: vec4<f32>,
};

@group(0) @binding(0) var<uniform> overlay: OverlayUniform;
@group(0) @binding(1) var underwater_texture: texture_2d<f32>;
@group(0) @binding(2) var pumpkin_texture: texture_2d<f32>;
@group(0) @binding(3) var powder_snow_texture: texture_2d<f32>;
@group(0) @binding(4) var nausea_texture: texture_2d<f32>;
@group(0) @binding(5) var vignette_texture: texture_2d<f32>;
@group(0) @binding(6) var spyglass_texture: texture_2d<f32>;
@group(0) @binding(7) var overlay_sampler: sampler;

struct VertexOutput {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs_main(@builtin(vertex_index) vertex_index: u32) -> VertexOutput {
    var positions = array<vec2<f32>, 3>(
        vec2<f32>(-1.0, -1.0),
        vec2<f32>(3.0, -1.0),
        vec2<f32>(-1.0, 3.0),
    );
    var uvs = array<vec2<f32>, 3>(
        vec2<f32>(0.0, 1.0),
        vec2<f32>(2.0, 1.0),
        vec2<f32>(0.0, -1.0),
    );
    var out: VertexOutput;
    out.position = vec4<f32>(positions[vertex_index], 0.0, 1.0);
    out.uv = uvs[vertex_index];
    return out;
}

fn overlay_mix(base: vec4<f32>, rgb: vec3<f32>, amount: f32) -> vec4<f32> {
    let src_alpha = clamp(amount, 0.0, 1.0);
    let out_alpha = src_alpha + base.a * (1.0 - src_alpha);
    if (out_alpha <= 0.0) {
        return vec4<f32>(0.0);
    }
    let out_rgb = (rgb * src_alpha + base.rgb * base.a * (1.0 - src_alpha)) / out_alpha;
    return vec4<f32>(out_rgb, out_alpha);
}

@fragment
fn fs_main(input: VertexOutput) -> @location(0) vec4<f32> {
    let uv = clamp(input.uv, vec2<f32>(0.0), vec2<f32>(1.0));
    let centered = uv * 2.0 - vec2<f32>(1.0);
    let time = overlay.screen.z;
    let aspect = overlay.screen.x / max(overlay.screen.y, 1.0);

    let water_uv = vec2<f32>(
        uv.x * aspect * 4.0 + time * 0.018,
        uv.y * 4.0 - time * 0.032,
    );
    let underwater = textureSample(underwater_texture, overlay_sampler, water_uv);
    let pumpkin = textureSample(pumpkin_texture, overlay_sampler, uv);
    let powder_snow = textureSample(powder_snow_texture, overlay_sampler, uv);
    let vignette = textureSample(vignette_texture, overlay_sampler, uv);
    let spyglass = textureSample(spyglass_texture, overlay_sampler, uv);

    let radius = length(centered);
    let angle = atan2(centered.y, centered.x);
    let swirl_radius = radius * (0.88 + 0.08 * sin(time * 0.7));
    let swirl_angle = angle + radius * 4.0 - time * 0.9;
    let portal_uv = vec2<f32>(cos(swirl_angle), sin(swirl_angle)) * swirl_radius * 0.5 + vec2<f32>(0.5);
    let nausea = textureSample(nausea_texture, overlay_sampler, portal_uv);

    var color = vec4<f32>(0.0);
    color = overlay_mix(
        color,
        vignette.rgb,
        vignette.a * overlay.effects_b.z,
    );
    color = overlay_mix(
        color,
        underwater.rgb,
        underwater.a * overlay.effects_a.z * 0.82,
    );

    let fire_band = 1.0 - smoothstep(0.02, 0.46, 1.0 - uv.y);
    let fire_wave = 0.62
        + 0.24 * sin(uv.x * 39.0 + time * 6.2)
        + 0.14 * sin(uv.x * 17.0 - time * 3.7);
    let fire_mask = clamp(fire_band * fire_wave, 0.0, 1.0) * overlay.effects_a.y;
    color = overlay_mix(
        color,
        vec3<f32>(1.0, 0.24 + 0.42 * fire_mask, 0.015),
        fire_mask * 0.86,
    );

    color = overlay_mix(
        color,
        nausea.rgb * vec3<f32>(0.72, 0.42, 1.0),
        nausea.a * overlay.effects_a.w * 0.76,
    );
    color = overlay_mix(
        color,
        powder_snow.rgb,
        powder_snow.a * overlay.effects_b.x,
    );
    color = overlay_mix(
        color,
        pumpkin.rgb,
        pumpkin.a * overlay.effects_b.y,
    );

    let edge = max(abs(centered.x), abs(centered.y));
    let hurt_mask = smoothstep(0.68, 0.73, edge) * overlay.effects_a.x;
    color = overlay_mix(color, vec3<f32>(0.92, 0.015, 0.005), hurt_mask * 0.72);

    let scope_radius = length(vec2<f32>(centered.x * aspect, centered.y));
    let matte = smoothstep(0.82, 0.87, scope_radius) * overlay.effects_b.w;
    color = overlay_mix(color, vec3<f32>(0.0), matte);
    color = overlay_mix(
        color,
        spyglass.rgb,
        spyglass.a * overlay.effects_b.w,
    );

    return vec4<f32>(color.rgb, clamp(color.a, 0.0, 0.95));
}
"#;

#[repr(C)]
#[derive(Clone, Copy, Debug, Pod, Zeroable)]
struct EnvironmentUniform {
    view_proj: [[f32; 4]; 4],
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Pod, Zeroable)]
struct OverlayUniform {
    effects_a: [f32; 4],
    effects_b: [f32; 4],
    screen: [f32; 4],
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Pod, Zeroable)]
struct WeatherVertex {
    position: [f32; 4],
    data: [f32; 4],
}

const ZERO_VERTEX: WeatherVertex = WeatherVertex {
    position: [0.0; 4],
    data: [0.0; 4],
};

const WEATHER_VERTEX_ATTRIBUTES: [wgpu::VertexAttribute; 2] = [
    wgpu::VertexAttribute {
        format: wgpu::VertexFormat::Float32x4,
        offset: 0,
        shader_location: 0,
    },
    wgpu::VertexAttribute {
        format: wgpu::VertexFormat::Float32x4,
        offset: 16,
        shader_location: 1,
    },
];

const WEATHER_VERTEX_LAYOUT: wgpu::VertexBufferLayout<'static> = wgpu::VertexBufferLayout {
    array_stride: std::mem::size_of::<WeatherVertex>() as u64,
    step_mode: wgpu::VertexStepMode::Vertex,
    attributes: &WEATHER_VERTEX_ATTRIBUTES,
};

/// Per-frame camera, celestial and precipitation inputs for the textured
/// environment pass. Effect alphas and precipitation times are sanitized;
/// callers are expected to provide a finite camera pose.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CelestialWeatherFrame {
    pub camera: Camera,
    pub aspect: f32,
    pub world_time_ticks: u64,
    pub moon_phase: u8,
    pub dimension_kind: u32,
    pub rain_level: f32,
    pub snowy: bool,
    pub cloud_alpha: f32,
    pub cloud_height: f32,
    pub time_secs: f32,
}

impl Default for CelestialWeatherFrame {
    fn default() -> Self {
        Self {
            camera: Camera::default(),
            aspect: 16.0 / 9.0,
            world_time_ticks: 6_000,
            moon_phase: 0,
            dimension_kind: DIMENSION_KIND_OVERWORLD,
            rain_level: 0.0,
            snowy: false,
            cloud_alpha: 1.0,
            cloud_height: 192.0,
            time_secs: 0.0,
        }
    }
}

/// Draw counts from one environment encode call.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CelestialWeatherDrawStats {
    pub alpha_quads: u32,
    pub additive_quads: u32,
    pub written_vertices: u32,
}

/// Normalized UV rectangle for one cell of Vanilla's 4×2 moon phase sheet.
pub fn moon_phase_uv(phase: u8) -> [f32; 4] {
    moon_phase_uv_rect(phase)
}

/// Whether the tiled `end_sky.png` cube replaces the ordinary celestial sky.
pub fn end_sky_enabled(dimension_kind: u32) -> bool {
    dimension_kind == DIMENSION_KIND_END
}

/// Scrolling UV offset for precipitation curtains. Rain has a fast vertical
/// scroll; snow drifts gently in both axes. The result is wrapped to `0..1`.
pub fn precipitation_scroll(time_secs: f32, snowy: bool) -> [f32; 2] {
    let t = finite_or_zero(time_secs).max(0.0);
    if snowy {
        [
            (t * 0.045 + (t * 0.31).sin() * 0.08).rem_euclid(1.0),
            (t * 0.11).rem_euclid(1.0),
        ]
    } else {
        [(t * 0.025).rem_euclid(1.0), (t * 1.35).rem_euclid(1.0)]
    }
}

/// Whether any textured gameplay overlay or the spyglass matte is visible.
pub fn overlay_effects_active(overlay: &GameplayScreenOverlay, spyglass_alpha: f32) -> bool {
    [
        overlay.hurt_alpha,
        overlay.fire_alpha,
        overlay.underwater_alpha,
        overlay.portal_alpha,
        overlay.freezing_alpha,
        overlay.pumpkin_alpha,
        overlay.vignette_alpha,
        spyglass_alpha,
    ]
    .iter()
    .any(|&value| clean_alpha(value) > 0.0)
}

fn finite_or_zero(value: f32) -> f32 {
    if value.is_finite() {
        value
    } else {
        0.0
    }
}

fn clean_alpha(value: f32) -> f32 {
    finite_or_zero(value).clamp(0.0, 1.0)
}

fn clean_vec3(value: [f32; 3], fallback: [f32; 3]) -> [f32; 3] {
    if value.iter().all(|component| component.is_finite()) {
        value
    } else {
        fallback
    }
}

fn normalize_or(value: [f32; 3], fallback: [f32; 3]) -> [f32; 3] {
    let value = clean_vec3(value, fallback);
    let length_sq = value[0] * value[0] + value[1] * value[1] + value[2] * value[2];
    if length_sq <= 1.0e-8 || !length_sq.is_finite() {
        fallback
    } else {
        let inv = 1.0 / length_sq.sqrt();
        [value[0] * inv, value[1] * inv, value[2] * inv]
    }
}

fn valid_decoded_texture(device: &wgpu::Device, image: &DecodedRgbaTexture) -> bool {
    let max_dim = device.limits().max_texture_dimension_2d;
    image.width > 0
        && image.height > 0
        && image.width <= max_dim
        && image.height <= max_dim
        && u64::from(image.width)
            .checked_mul(u64::from(image.height))
            .and_then(|pixels| pixels.checked_mul(4))
            .and_then(|bytes| usize::try_from(bytes).ok())
            == Some(image.rgba.len())
}

fn upload_texture_or_fallback(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    label: &str,
    image: Option<&DecodedRgbaTexture>,
    fallback_rgba: [u8; 4],
) -> wgpu::Texture {
    let use_image = image.is_some_and(|image| valid_decoded_texture(device, image));
    let (width, height, rgba) = if use_image {
        let image = image.expect("use_image implies an image");
        (image.width, image.height, image.rgba.as_slice())
    } else {
        (1, 1, fallback_rgba.as_slice())
    };
    let texture = device.create_texture(&wgpu::TextureDescriptor {
        label: Some(label),
        size: wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8Unorm,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    queue.write_texture(
        wgpu::TexelCopyTextureInfo {
            texture: &texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        rgba,
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(width * 4),
            rows_per_image: Some(height),
        },
        wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
    );
    texture
}

fn create_texture_views(textures: &[wgpu::Texture; 6]) -> [wgpu::TextureView; 6] {
    std::array::from_fn(|index| {
        textures[index].create_view(&wgpu::TextureViewDescriptor::default())
    })
}

fn repeating_sampler(device: &wgpu::Device, label: &str) -> wgpu::Sampler {
    device.create_sampler(&wgpu::SamplerDescriptor {
        label: Some(label),
        address_mode_u: wgpu::AddressMode::Repeat,
        address_mode_v: wgpu::AddressMode::Repeat,
        address_mode_w: wgpu::AddressMode::Repeat,
        mag_filter: wgpu::FilterMode::Linear,
        min_filter: wgpu::FilterMode::Linear,
        mipmap_filter: wgpu::MipmapFilterMode::Linear,
        ..wgpu::SamplerDescriptor::default()
    })
}

fn texture_layout_entry(binding: u32) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::FRAGMENT,
        ty: wgpu::BindingType::Texture {
            sample_type: wgpu::TextureSampleType::Float { filterable: true },
            view_dimension: wgpu::TextureViewDimension::D2,
            multisampled: false,
        },
        count: None,
    }
}

fn sampler_layout_entry(binding: u32) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::FRAGMENT,
        ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
        count: None,
    }
}

fn uniform_layout_entry(binding: u32, size: usize) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
        ty: wgpu::BindingType::Buffer {
            ty: wgpu::BufferBindingType::Uniform,
            has_dynamic_offset: false,
            min_binding_size: wgpu::BufferSize::new(size as u64),
        },
        count: None,
    }
}

fn textured_pass_layout_entries(uniform_size: usize) -> [wgpu::BindGroupLayoutEntry; 8] {
    [
        uniform_layout_entry(0, uniform_size),
        texture_layout_entry(1),
        texture_layout_entry(2),
        texture_layout_entry(3),
        texture_layout_entry(4),
        texture_layout_entry(5),
        texture_layout_entry(6),
        sampler_layout_entry(7),
    ]
}

fn textured_pass_bind_entries<'a>(
    uniform_buffer: &'a wgpu::Buffer,
    texture_views: &'a [wgpu::TextureView; 6],
    sampler: &'a wgpu::Sampler,
) -> [wgpu::BindGroupEntry<'a>; 8] {
    [
        wgpu::BindGroupEntry {
            binding: 0,
            resource: uniform_buffer.as_entire_binding(),
        },
        wgpu::BindGroupEntry {
            binding: 1,
            resource: wgpu::BindingResource::TextureView(&texture_views[0]),
        },
        wgpu::BindGroupEntry {
            binding: 2,
            resource: wgpu::BindingResource::TextureView(&texture_views[1]),
        },
        wgpu::BindGroupEntry {
            binding: 3,
            resource: wgpu::BindingResource::TextureView(&texture_views[2]),
        },
        wgpu::BindGroupEntry {
            binding: 4,
            resource: wgpu::BindingResource::TextureView(&texture_views[3]),
        },
        wgpu::BindGroupEntry {
            binding: 5,
            resource: wgpu::BindingResource::TextureView(&texture_views[4]),
        },
        wgpu::BindGroupEntry {
            binding: 6,
            resource: wgpu::BindingResource::TextureView(&texture_views[5]),
        },
        wgpu::BindGroupEntry {
            binding: 7,
            resource: wgpu::BindingResource::Sampler(sampler),
        },
    ]
}

fn standard_alpha_blend() -> wgpu::BlendState {
    wgpu::BlendState {
        color: wgpu::BlendComponent {
            src_factor: wgpu::BlendFactor::SrcAlpha,
            dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha,
            operation: wgpu::BlendOperation::Add,
        },
        alpha: wgpu::BlendComponent {
            src_factor: wgpu::BlendFactor::One,
            dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha,
            operation: wgpu::BlendOperation::Add,
        },
    }
}

fn additive_celestial_blend() -> wgpu::BlendState {
    wgpu::BlendState {
        color: wgpu::BlendComponent {
            src_factor: wgpu::BlendFactor::SrcAlpha,
            dst_factor: wgpu::BlendFactor::One,
            operation: wgpu::BlendOperation::Add,
        },
        alpha: wgpu::BlendComponent {
            src_factor: wgpu::BlendFactor::SrcAlpha,
            dst_factor: wgpu::BlendFactor::One,
            operation: wgpu::BlendOperation::Add,
        },
    }
}

fn create_environment_pipeline(
    device: &wgpu::Device,
    module: &wgpu::ShaderModule,
    layout: &wgpu::PipelineLayout,
    target_format: wgpu::TextureFormat,
    blend: wgpu::BlendState,
    label: &str,
) -> wgpu::RenderPipeline {
    device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some(label),
        layout: Some(layout),
        vertex: wgpu::VertexState {
            module,
            entry_point: Some("vs_main"),
            compilation_options: wgpu::PipelineCompilationOptions::default(),
            buffers: &[Some(WEATHER_VERTEX_LAYOUT)],
        },
        fragment: Some(wgpu::FragmentState {
            module,
            entry_point: Some("fs_main"),
            compilation_options: wgpu::PipelineCompilationOptions::default(),
            targets: &[Some(wgpu::ColorTargetState {
                format: target_format,
                blend: Some(blend),
                write_mask: wgpu::ColorWrites::ALL,
            })],
        }),
        primitive: wgpu::PrimitiveState {
            topology: wgpu::PrimitiveTopology::TriangleList,
            cull_mode: None,
            ..wgpu::PrimitiveState::default()
        },
        depth_stencil: Some(wgpu::DepthStencilState {
            format: WORLD_ENV_PASS_DEPTH_FORMAT,
            depth_write_enabled: Some(false),
            depth_compare: Some(wgpu::CompareFunction::LessEqual),
            stencil: wgpu::StencilState::default(),
            bias: wgpu::DepthBiasState::default(),
        }),
        multisample: wgpu::MultisampleState::default(),
        multiview_mask: None,
        cache: None,
    })
}

fn write_quad(
    vertices: &mut [WeatherVertex; TOTAL_VERTEX_COUNT],
    quad_cursor: &mut usize,
    corners: [[f32; 3]; 4],
    uvs: [[f32; 2]; 4],
    texture_index: f32,
    alpha: f32,
) {
    debug_assert!(*quad_cursor < TOTAL_QUAD_COUNT);
    let base = *quad_cursor * 4;
    for corner in 0..4 {
        vertices[base + corner] = WeatherVertex {
            position: [
                corners[corner][0],
                corners[corner][1],
                corners[corner][2],
                1.0,
            ],
            data: [
                uvs[corner][0],
                uvs[corner][1],
                texture_index,
                clean_alpha(alpha),
            ],
        };
    }
    *quad_cursor += 1;
}

fn add3(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [a[0] + b[0], a[1] + b[1], a[2] + b[2]]
}

fn scale3(value: [f32; 3], scale: f32) -> [f32; 3] {
    [value[0] * scale, value[1] * scale, value[2] * scale]
}

fn cross3(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}

fn camera_billboard_axes(camera: &Camera) -> ([f32; 3], [f32; 3]) {
    let forward = normalize_or(camera.forward_vector(), [0.0, 0.0, -1.0]);
    let mut right = normalize_or(cross3(forward, [0.0, 1.0, 0.0]), [1.0, 0.0, 0.0]);
    let mut up = cross3(right, forward);
    if camera.roll_rad.is_finite() && camera.roll_rad != 0.0 {
        let (sin, cos) = camera.roll_rad.sin_cos();
        let rolled_right = [
            right[0] * cos + up[0] * sin,
            right[1] * cos + up[1] * sin,
            right[2] * cos + up[2] * sin,
        ];
        up = [
            up[0] * cos - right[0] * sin,
            up[1] * cos - right[1] * sin,
            up[2] * cos - right[2] * sin,
        ];
        right = rolled_right;
    }
    (
        normalize_or(right, [1.0, 0.0, 0.0]),
        normalize_or(up, [0.0, 1.0, 0.0]),
    )
}

fn billboard_corners(
    center: [f32; 3],
    right: [f32; 3],
    up: [f32; 3],
    half_size: f32,
) -> [[f32; 3]; 4] {
    let right = scale3(right, half_size);
    let up = scale3(up, half_size);
    [
        [
            center[0] - right[0] - up[0],
            center[1] - right[1] - up[1],
            center[2] - right[2] - up[2],
        ],
        [
            center[0] + right[0] - up[0],
            center[1] + right[1] - up[1],
            center[2] + right[2] - up[2],
        ],
        [
            center[0] + right[0] + up[0],
            center[1] + right[1] + up[1],
            center[2] + right[2] + up[2],
        ],
        [
            center[0] - right[0] + up[0],
            center[1] - right[1] + up[1],
            center[2] - right[2] + up[2],
        ],
    ]
}

fn sky_distance(camera_far: f32) -> f32 {
    if camera_far.is_finite() && camera_far > 1.0 {
        (camera_far * 0.60).min(384.0)
    } else {
        32.0
    }
}

fn write_end_sky_cube(
    vertices: &mut [WeatherVertex; TOTAL_VERTEX_COUNT],
    quad_cursor: &mut usize,
    camera_position: [f32; 3],
    half_size: f32,
) {
    for face in 0..6 {
        let axis = face / 2;
        let positive = face % 2 == 0;
        let side = if positive { half_size } else { -half_size };
        let mut corners = [[0.0; 3]; 4];
        for (corner_index, corner) in corners.iter_mut().enumerate() {
            let a = if corner_index == 1 || corner_index == 2 {
                half_size
            } else {
                -half_size
            };
            let b = if corner_index >= 2 {
                half_size
            } else {
                -half_size
            };
            *corner = match axis {
                0 => [side, a, b],
                1 => [a, side, b],
                _ => [a, b, side],
            };
            *corner = add3(*corner, camera_position);
        }
        write_quad(
            vertices,
            quad_cursor,
            corners,
            [
                [0.0, 0.0],
                [END_SKY_TILES, 0.0],
                [END_SKY_TILES, END_SKY_TILES],
                [0.0, END_SKY_TILES],
            ],
            TEXTURE_END_SKY,
            1.0,
        );
    }
}

fn write_cloud_deck(
    vertices: &mut [WeatherVertex; TOTAL_VERTEX_COUNT],
    quad_cursor: &mut usize,
    frame: &CelestialWeatherFrame,
) {
    let alpha = clean_alpha(frame.cloud_alpha);
    if alpha <= 0.0
        || frame.dimension_kind == DIMENSION_KIND_END
        || frame.dimension_kind == DIMENSION_KIND_NETHER
    {
        return;
    }
    let camera = frame.camera.position;
    let center_x = finite_or_zero(camera[0]);
    let center_z = finite_or_zero(camera[2]);
    let height = if frame.cloud_height.is_finite() {
        frame.cloud_height
    } else {
        192.0
    };
    let extent = 512.0;
    let x0 = center_x - extent;
    let x1 = center_x + extent;
    let z0 = center_z - extent;
    let z1 = center_z + extent;
    let drift = finite_or_zero(frame.time_secs) * 0.010;
    let uv_scale = 1.0 / 48.0;
    write_quad(
        vertices,
        quad_cursor,
        [
            [x0, height, z0],
            [x1, height, z0],
            [x1, height, z1],
            [x0, height, z1],
        ],
        [
            [x0 * uv_scale + drift, z0 * uv_scale],
            [x1 * uv_scale + drift, z0 * uv_scale],
            [x1 * uv_scale + drift, z1 * uv_scale],
            [x0 * uv_scale + drift, z1 * uv_scale],
        ],
        TEXTURE_CLOUDS,
        alpha * 0.82,
    );
}

fn write_precipitation_curtains(
    vertices: &mut [WeatherVertex; TOTAL_VERTEX_COUNT],
    quad_cursor: &mut usize,
    frame: &CelestialWeatherFrame,
) {
    let intensity = clean_alpha(frame.rain_level);
    if intensity <= 0.01 {
        return;
    }
    let camera = frame.camera.position;
    let camera_x = finite_or_zero(camera[0]);
    let camera_y = finite_or_zero(camera[1]);
    let camera_z = finite_or_zero(camera[2]);
    let scroll = precipitation_scroll(frame.time_secs, frame.snowy);
    let texture = if frame.snowy {
        TEXTURE_SNOW
    } else {
        TEXTURE_RAIN
    };
    let half_width = if frame.snowy { 1.55 } else { 1.20 };
    let vertical_tiles = if frame.snowy { 2.0 } else { 4.0 };
    let alpha = intensity * if frame.snowy { 0.72 } else { 0.56 };
    for index in 0..PRECIPITATION_CURTAIN_COUNT {
        let angle = index as f32 * (std::f32::consts::TAU / PRECIPITATION_CURTAIN_COUNT as f32)
            + (index % 3) as f32 * 0.17;
        let radius = 4.0 + (index % 4) as f32 * 1.75;
        let (sin, cos) = angle.sin_cos();
        let center_x = camera_x + cos * radius;
        let center_z = camera_z + sin * radius;
        let tangent_x = -sin;
        let tangent_z = cos;
        let bottom = camera_y - 4.0;
        let top = camera_y + 8.0;
        let u0 = index as f32 * 0.37 + scroll[0];
        let u1 = u0 + 1.0;
        let v0 = scroll[1];
        let v1 = scroll[1] + vertical_tiles;
        write_quad(
            vertices,
            quad_cursor,
            [
                [
                    center_x - tangent_x * half_width,
                    bottom,
                    center_z - tangent_z * half_width,
                ],
                [
                    center_x + tangent_x * half_width,
                    bottom,
                    center_z + tangent_z * half_width,
                ],
                [
                    center_x + tangent_x * half_width,
                    top,
                    center_z + tangent_z * half_width,
                ],
                [
                    center_x - tangent_x * half_width,
                    top,
                    center_z - tangent_z * half_width,
                ],
            ],
            [[u0, v1], [u1, v1], [u1, v0], [u0, v0]],
            texture,
            alpha,
        );
    }
}

fn write_celestial_billboards(
    vertices: &mut [WeatherVertex; TOTAL_VERTEX_COUNT],
    quad_cursor: &mut usize,
    frame: &CelestialWeatherFrame,
) {
    if frame.dimension_kind == DIMENSION_KIND_END || frame.dimension_kind == DIMENSION_KIND_NETHER {
        return;
    }
    let camera = frame.camera;
    let distance = sky_distance(camera.far);
    let sun_direction = normalize_or(
        compute_sun_direction(frame.world_time_ticks),
        [0.0, 1.0, 0.0],
    );
    let moon_direction = scale3(sun_direction, -1.0);
    let (right, up) = camera_billboard_axes(&camera);
    let position = camera.position;
    let sun_center = add3(position, scale3(sun_direction, distance));
    let moon_center = add3(position, scale3(moon_direction, distance));

    write_quad(
        vertices,
        quad_cursor,
        billboard_corners(sun_center, right, up, distance * 0.075),
        [[0.0, 1.0], [1.0, 1.0], [1.0, 0.0], [0.0, 0.0]],
        TEXTURE_SUN,
        1.0,
    );

    let moon_uv = moon_phase_uv(frame.moon_phase);
    write_quad(
        vertices,
        quad_cursor,
        billboard_corners(moon_center, right, up, distance * 0.058),
        [
            [moon_uv[0], moon_uv[3]],
            [moon_uv[2], moon_uv[3]],
            [moon_uv[2], moon_uv[1]],
            [moon_uv[0], moon_uv[1]],
        ],
        TEXTURE_MOON,
        1.0,
    );
}

/// Standalone textured celestial, cloud and precipitation pass.
///
/// Construction uploads six Vanilla environment textures and allocates one
/// fixed-capacity vertex buffer, one fixed-capacity index buffer and one
/// uniform buffer. `encode` writes into those existing buffers and performs no
/// heap allocation in this module.
pub struct TexturedCelestialWeatherPass {
    queue: wgpu::Queue,
    alpha_pipeline: wgpu::RenderPipeline,
    additive_pipeline: wgpu::RenderPipeline,
    vertex_buffer: wgpu::Buffer,
    index_buffer: wgpu::Buffer,
    uniform_buffer: wgpu::Buffer,
    bind_group: wgpu::BindGroup,
    _sampler: wgpu::Sampler,
    _textures: [wgpu::Texture; 6],
    _texture_views: [wgpu::TextureView; 6],
}

impl TexturedCelestialWeatherPass {
    /// Build the pass and upload `sun`, `moon_phases`, `clouds`, `rain`,
    /// `snow` and `end_sky`; invalid or absent entries use 1×1 fallbacks.
    pub fn new(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        target_format: wgpu::TextureFormat,
        assets: &VanillaWorldAssets,
    ) -> Self {
        let environment_textures = [
            (VanillaEnvTexture::Sun, "world-env-sun", [255, 210, 70, 255]),
            (
                VanillaEnvTexture::MoonPhases,
                "world-env-moon-phases",
                [205, 210, 220, 255],
            ),
            (
                VanillaEnvTexture::Clouds,
                "world-env-clouds",
                [255, 255, 255, 96],
            ),
            (
                VanillaEnvTexture::Rain,
                "world-env-rain",
                [140, 170, 220, 96],
            ),
            (
                VanillaEnvTexture::Snow,
                "world-env-snow",
                [240, 248, 255, 128],
            ),
            (
                VanillaEnvTexture::EndSky,
                "world-env-end-sky",
                [10, 4, 24, 255],
            ),
        ]
        .map(|(key, label, fallback)| {
            upload_texture_or_fallback(
                device,
                queue,
                label,
                assets.env_textures.get(&key),
                fallback,
            )
        });
        let texture_views = create_texture_views(&environment_textures);
        let sampler = repeating_sampler(device, "world-env-sampler");
        let uniform_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("world-env-uniform"),
            size: std::mem::size_of::<EnvironmentUniform>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let mut indices = [0u32; TOTAL_INDEX_COUNT];
        for quad in 0..TOTAL_QUAD_COUNT {
            let vertex_base = (quad * 4) as u32;
            let index_base = quad * 6;
            indices[index_base..index_base + 6].copy_from_slice(&[
                vertex_base,
                vertex_base + 1,
                vertex_base + 2,
                vertex_base,
                vertex_base + 2,
                vertex_base + 3,
            ]);
        }
        let vertex_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("world-env-vertices"),
            size: (std::mem::size_of::<WeatherVertex>() * TOTAL_VERTEX_COUNT) as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let index_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("world-env-indices"),
            size: (std::mem::size_of::<u32>() * TOTAL_INDEX_COUNT) as u64,
            usage: wgpu::BufferUsages::INDEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        queue.write_buffer(&index_buffer, 0, bytemuck::cast_slice(&indices));

        let layout_entries =
            textured_pass_layout_entries(std::mem::size_of::<EnvironmentUniform>());
        let bind_group_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("world-env-bind-layout"),
            entries: &layout_entries,
        });
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("world-env-bind-group"),
            layout: &bind_group_layout,
            entries: &textured_pass_bind_entries(&uniform_buffer, &texture_views, &sampler),
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("world-env-pipeline-layout"),
            bind_group_layouts: &[Some(&bind_group_layout)],
            immediate_size: 0,
        });
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("world-env-celestial-weather"),
            source: wgpu::ShaderSource::Wgsl(CELESTIAL_WEATHER_WGSL.into()),
        });
        let alpha_pipeline = create_environment_pipeline(
            device,
            &module,
            &pipeline_layout,
            target_format,
            standard_alpha_blend(),
            "world-env-alpha-pipeline",
        );
        let additive_pipeline = create_environment_pipeline(
            device,
            &module,
            &pipeline_layout,
            target_format,
            additive_celestial_blend(),
            "world-env-additive-pipeline",
        );
        Self {
            queue: queue.clone(),
            alpha_pipeline,
            additive_pipeline,
            vertex_buffer,
            index_buffer,
            uniform_buffer,
            bind_group,
            _sampler: sampler,
            _textures: environment_textures,
            _texture_views: texture_views,
        }
    }

    /// Encode the pass. Additive sun/moon billboards use `SRC_ALPHA, ONE`
    /// color blending first; alpha-blended End sky, clouds and precipitation
    /// then composite over them.
    pub fn encode(
        &self,
        encoder: &mut wgpu::CommandEncoder,
        color_view: &wgpu::TextureView,
        depth_view: &wgpu::TextureView,
        frame: &CelestialWeatherFrame,
    ) -> CelestialWeatherDrawStats {
        let mut vertices = [ZERO_VERTEX; TOTAL_VERTEX_COUNT];
        let mut alpha_quads = 0usize;
        let camera_position = clean_vec3(frame.camera.position, [0.0; 3]);
        if end_sky_enabled(frame.dimension_kind) {
            write_end_sky_cube(
                &mut vertices,
                &mut alpha_quads,
                camera_position,
                sky_distance(frame.camera.far) * 0.42,
            );
        }
        write_cloud_deck(&mut vertices, &mut alpha_quads, frame);
        write_precipitation_curtains(&mut vertices, &mut alpha_quads, frame);

        let mut total_quads = alpha_quads;
        write_celestial_billboards(&mut vertices, &mut total_quads, frame);
        let additive_quads = total_quads - alpha_quads;
        let written_vertices = total_quads * 4;
        let stats = CelestialWeatherDrawStats {
            alpha_quads: alpha_quads as u32,
            additive_quads: additive_quads as u32,
            written_vertices: written_vertices as u32,
        };
        if total_quads == 0 {
            return stats;
        }

        let uniform = EnvironmentUniform {
            view_proj: frame.camera.view_projection(frame.aspect),
        };
        self.queue
            .write_buffer(&self.uniform_buffer, 0, bytemuck::bytes_of(&uniform));
        self.queue.write_buffer(
            &self.vertex_buffer,
            0,
            bytemuck::cast_slice(&vertices[..written_vertices]),
        );

        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("textured-celestial-weather-pass"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: color_view,
                resolve_target: None,
                depth_slice: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Load,
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                view: depth_view,
                depth_ops: Some(wgpu::Operations {
                    load: wgpu::LoadOp::Load,
                    store: wgpu::StoreOp::Store,
                }),
                stencil_ops: None,
            }),
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });
        pass.set_vertex_buffer(0, self.vertex_buffer.slice(..));
        pass.set_index_buffer(self.index_buffer.slice(..), wgpu::IndexFormat::Uint32);
        pass.set_bind_group(0, &self.bind_group, &[]);
        if additive_quads > 0 {
            let start = (alpha_quads * 6) as u32;
            let end = ((alpha_quads + additive_quads) * 6) as u32;
            pass.set_pipeline(&self.additive_pipeline);
            pass.draw_indexed(start..end, 0, 0..1);
        }
        if alpha_quads > 0 {
            pass.set_pipeline(&self.alpha_pipeline);
            pass.draw_indexed(0..(alpha_quads * 6) as u32, 0, 0..1);
        }
        drop(pass);
        stats
    }
}

/// Standalone textured full-screen gameplay overlay pass.
pub struct TexturedGameplayOverlayPass {
    queue: wgpu::Queue,
    pipeline: wgpu::RenderPipeline,
    uniform_buffer: wgpu::Buffer,
    bind_group: wgpu::BindGroup,
    _sampler: wgpu::Sampler,
    _textures: [wgpu::Texture; 6],
    _texture_views: [wgpu::TextureView; 6],
}

impl TexturedGameplayOverlayPass {
    /// Build the overlay pass and upload Vanilla misc overlay textures.
    /// Invalid or absent entries use small deterministic fallback pixels.
    pub fn new(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        target_format: wgpu::TextureFormat,
        assets: &VanillaWorldAssets,
    ) -> Self {
        let overlay_textures = [
            (
                VanillaMiscOverlay::Underwater,
                "overlay-underwater",
                [22, 88, 128, 112],
            ),
            (
                VanillaMiscOverlay::PumpkinBlur,
                "overlay-pumpkinblur",
                [24, 12, 2, 224],
            ),
            (
                VanillaMiscOverlay::PowderSnowOutline,
                "overlay-powder-snow-outline",
                [190, 232, 255, 128],
            ),
            (
                VanillaMiscOverlay::Nausea,
                "overlay-nausea",
                [92, 24, 150, 96],
            ),
            (
                VanillaMiscOverlay::Vignette,
                "overlay-vignette",
                [0, 0, 0, 176],
            ),
            (
                VanillaMiscOverlay::SpyglassScope,
                "overlay-spyglass-scope",
                [0, 0, 0, 208],
            ),
        ]
        .map(|(key, label, fallback)| {
            upload_texture_or_fallback(
                device,
                queue,
                label,
                assets.misc_overlays.get(&key),
                fallback,
            )
        });
        let texture_views = create_texture_views(&overlay_textures);
        let sampler = repeating_sampler(device, "gameplay-overlay-sampler");
        let uniform_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("gameplay-overlay-uniform"),
            size: std::mem::size_of::<OverlayUniform>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let layout_entries = textured_pass_layout_entries(std::mem::size_of::<OverlayUniform>());
        let bind_group_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("gameplay-overlay-bind-layout"),
            entries: &layout_entries,
        });
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("gameplay-overlay-bind-group"),
            layout: &bind_group_layout,
            entries: &textured_pass_bind_entries(&uniform_buffer, &texture_views, &sampler),
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("gameplay-overlay-pipeline-layout"),
            bind_group_layouts: &[Some(&bind_group_layout)],
            immediate_size: 0,
        });
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("gameplay-textured-overlay"),
            source: wgpu::ShaderSource::Wgsl(TEXTURED_OVERLAY_WGSL.into()),
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("gameplay-textured-overlay-pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &module,
                entry_point: Some("vs_main"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                buffers: &[],
            },
            fragment: Some(wgpu::FragmentState {
                module: &module,
                entry_point: Some("fs_main"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: target_format,
                    blend: Some(standard_alpha_blend()),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                cull_mode: None,
                ..wgpu::PrimitiveState::default()
            },
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        });
        Self {
            queue: queue.clone(),
            pipeline,
            uniform_buffer,
            bind_group,
            _sampler: sampler,
            _textures: overlay_textures,
            _texture_views: texture_views,
        }
    }

    /// Fast-path predicate used by `encode` before touching GPU command state.
    pub fn effects_active(overlay: &GameplayScreenOverlay, spyglass_alpha: f32) -> bool {
        overlay_effects_active(overlay, spyglass_alpha)
    }

    /// Encode the overlay when any effect alpha is non-zero. Returns `false`
    /// without opening a render pass when the screen overlay is fully inactive.
    pub fn encode(
        &self,
        encoder: &mut wgpu::CommandEncoder,
        color_view: &wgpu::TextureView,
        width: u32,
        height: u32,
        overlay: &GameplayScreenOverlay,
        spyglass_alpha: f32,
    ) -> bool {
        if !Self::effects_active(overlay, spyglass_alpha) {
            return false;
        }
        let uniform = OverlayUniform {
            effects_a: [
                clean_alpha(overlay.hurt_alpha),
                clean_alpha(overlay.fire_alpha),
                clean_alpha(overlay.underwater_alpha),
                clean_alpha(overlay.portal_alpha),
            ],
            effects_b: [
                clean_alpha(overlay.freezing_alpha),
                clean_alpha(overlay.pumpkin_alpha),
                clean_alpha(overlay.vignette_alpha),
                clean_alpha(spyglass_alpha),
            ],
            screen: [
                width.max(1) as f32,
                height.max(1) as f32,
                finite_or_zero(overlay.time_secs),
                0.0,
            ],
        };
        self.queue
            .write_buffer(&self.uniform_buffer, 0, bytemuck::bytes_of(&uniform));
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("textured-gameplay-overlay-pass"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: color_view,
                resolve_target: None,
                depth_slice: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Load,
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });
        pass.set_pipeline(&self.pipeline);
        pass.set_bind_group(0, &self.bind_group, &[]);
        pass.draw(0..3, 0..1);
        drop(pass);
        true
    }
}
