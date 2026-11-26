// SPDX-License-Identifier: LGPL-3.0-or-later
//! Standalone block-break crack overlay pass.
//!
//! The pass renders Vanilla's ten `destroy_stage` textures as one horizontal
//! strip over a slightly inflated unit cube. All GPU resources are retained by
//! [`BlockBreakCrackPass`]; once a target is active, encoding a frame performs
//! one 80-byte uniform write and one indexed draw without heap allocation in
//! this crate's code.

use std::sync::OnceLock;

use bytemuck::{Pod, Zeroable};
use wgpu::util::DeviceExt;

use crate::vanilla_world_assets::{DecodedRgbaTexture, VanillaWorldAssets};

/// Number of Vanilla block-destruction stages in the horizontal strip.
pub const CRACK_STAGE_COUNT: u32 = 10;
/// Distance the crack cube is inflated beyond the targeted block on every side.
pub const CRACK_CUBE_INFLATION: f32 = 0.003;
/// Minimum local coordinate of the inflated crack cube.
pub const CRACK_CUBE_MIN: f32 = -CRACK_CUBE_INFLATION;
/// Maximum local coordinate of the inflated crack cube.
pub const CRACK_CUBE_MAX: f32 = 1.0 + CRACK_CUBE_INFLATION;

/// WGSL for the world-space crack box. The strip UV is formed in the vertex
/// shader as `(stage + local_u) / 10`; the stage is clamped defensively even
/// though the host uniform writer already performs the same clamp.
pub const CRACK_OVERLAY_WGSL: &str = r#"
struct CrackUniforms {
    view_proj: mat4x4<f32>,
    block_origin_stage: vec4<f32>,
};

@group(0) @binding(0) var<uniform> uniforms: CrackUniforms;
@group(0) @binding(1) var crack_strip: texture_2d<f32>;
@group(0) @binding(2) var crack_sampler: sampler;

struct VertexOutput {
    @builtin(position) clip_position: vec4<f32>,
    @location(0) strip_uv: vec2<f32>,
};

@vertex
fn vs_crack_overlay(
    @location(0) local_position: vec3<f32>,
    @location(1) local_uv: vec2<f32>,
) -> VertexOutput {
    let world_position = uniforms.block_origin_stage.xyz + local_position;
    let stage = clamp(uniforms.block_origin_stage.w, 0.0, 9.0);
    var out: VertexOutput;
    out.clip_position = uniforms.view_proj * vec4<f32>(world_position, 1.0);
    out.strip_uv = vec2<f32>((stage + local_uv.x) / 10.0, local_uv.y);
    return out;
}

@fragment
fn fs_crack_overlay(in: VertexOutput) -> @location(0) vec4<f32> {
    let texel = textureSample(crack_strip, crack_sampler, in.strip_uv);
    if (texel.a < 0.05) {
        discard;
    }
    return texel;
}
"#;

/// One crack-cube vertex: inflated block-local position plus un-inflated
/// face-local UV coordinates.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Pod, Zeroable)]
pub struct CrackVertex {
    pub position: [f32; 3],
    pub uv: [f32; 2],
}

/// Uniform record written by the hot path. The layout is exactly 80 bytes:
/// one `mat4x4<f32>` followed by `block_origin.xyz` and the clamped stage in
/// `block_origin_stage.w`.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Pod, Zeroable)]
pub struct CrackUniforms {
    pub view_proj: [[f32; 4]; 4],
    pub block_origin_stage: [f32; 4],
}

const fn crack_vertex(position: [f32; 3], uv: [f32; 2]) -> CrackVertex {
    CrackVertex { position, uv }
}

const fn build_crack_cube_vertices() -> [CrackVertex; 24] {
    let m = CRACK_CUBE_MIN;
    let x = CRACK_CUBE_MAX;
    // Four perimeter vertices per face. UVs always cover the full face; the
    // ordering rotates the face-local frame where needed so every quad remains
    // planar and non-self-intersecting.
    let faces = [
        // -Y
        [[m, m, m], [m, m, x], [x, m, x], [x, m, m]],
        // +Y
        [[m, x, m], [x, x, m], [x, x, x], [m, x, x]],
        // -Z
        [[m, m, m], [x, m, m], [x, x, m], [m, x, m]],
        // +Z
        [[m, m, x], [m, x, x], [x, x, x], [x, m, x]],
        // -X
        [[m, m, m], [m, x, m], [m, x, x], [m, m, x]],
        // +X
        [[x, m, m], [x, m, x], [x, x, x], [x, x, m]],
    ];
    let mut out = [crack_vertex([0.0; 3], [0.0; 2]); 24];
    let mut face = 0;
    while face < 6 {
        let mut corner = 0;
        while corner < 4 {
            let uv = match corner {
                0 => [0.0, 1.0],
                1 => [1.0, 1.0],
                2 => [1.0, 0.0],
                _ => [0.0, 0.0],
            };
            out[face * 4 + corner] = crack_vertex(faces[face][corner], uv);
            corner += 1;
        }
        face += 1;
    }
    out
}

const fn build_crack_cube_indices() -> [u32; 36] {
    let mut out = [0u32; 36];
    let mut face = 0;
    while face < 6 {
        let base = (face * 4) as u32;
        let at = face * 6;
        out[at] = base;
        out[at + 1] = base + 2;
        out[at + 2] = base + 1;
        out[at + 3] = base;
        out[at + 4] = base + 3;
        out[at + 5] = base + 2;
        face += 1;
    }
    out
}

/// Twenty-four pre-inflated cube vertices, four per face.
pub const CRACK_CUBE_VERTICES: [CrackVertex; 24] = build_crack_cube_vertices();
/// Thirty-six cube indices, two triangles per face.
pub const CRACK_CUBE_INDICES: [u32; 36] = build_crack_cube_indices();

const CRACK_VERTEX_ATTRS: [wgpu::VertexAttribute; 2] = [
    wgpu::VertexAttribute {
        format: wgpu::VertexFormat::Float32x3,
        offset: 0,
        shader_location: 0,
    },
    wgpu::VertexAttribute {
        format: wgpu::VertexFormat::Float32x2,
        offset: 12,
        shader_location: 1,
    },
];

const CRACK_VERTEX_LAYOUT: wgpu::VertexBufferLayout<'static> = wgpu::VertexBufferLayout {
    array_stride: std::mem::size_of::<CrackVertex>() as wgpu::BufferAddress,
    step_mode: wgpu::VertexStepMode::Vertex,
    attributes: &CRACK_VERTEX_ATTRS,
};

const CRACK_BLEND: wgpu::BlendState = wgpu::BlendState {
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
};

// 64 MiB still admits a full 1024x1024-per-stage resource-pack strip while
// blocking pathological inputs that amplify one admitted decode into an
// unbounded ten-stage allocation.
const MAX_CRACK_STRIP_BYTES: usize = 64 * 1024 * 1024;

/// Packed horizontal destroy-stage strip. `width` is the total strip width
/// (`tile_width * 10`), and `rgba` is tightly packed RGBA8 row-major data.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DestroyStageStrip {
    pub tile_width: u32,
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
    pub fallback_stages: [bool; 10],
}

/// Build the exact uniform value the encode path uploads for a target.
pub fn crack_uniform(view_proj: [[f32; 4]; 4], target: ([i32; 3], u8)) -> CrackUniforms {
    let (block_pos, stage) = target;
    CrackUniforms {
        view_proj,
        block_origin_stage: [
            block_pos[0] as f32,
            block_pos[1] as f32,
            block_pos[2] as f32,
            f32::from(stage.min(9)),
        ],
    }
}

fn texture_is_well_formed(texture: &DecodedRgbaTexture) -> bool {
    texture.width > 0
        && texture.height > 0
        && u64::from(texture.width)
            .checked_mul(u64::from(texture.height))
            .and_then(|pixels| pixels.checked_mul(4))
            == Some(texture.rgba.len() as u64)
}

fn texture_is_usable(texture: &DecodedRgbaTexture) -> bool {
    texture_is_well_formed(texture)
        && texture
            .rgba
            .as_chunks::<4>()
            .0
            .iter()
            .any(|pixel| pixel[3] != 0)
}

fn loader_destroy_fallbacks() -> &'static [DecodedRgbaTexture; 10] {
    static FALLBACKS: OnceLock<[DecodedRgbaTexture; 10]> = OnceLock::new();
    FALLBACKS.get_or_init(|| VanillaWorldAssets::fallback().destroy_stages)
}

// Compare with the loader-owned fallback instead of duplicating its pixel
// formula; exact collisions are indistinguishable from that placeholder anyway.
fn is_builtin_loader_destroy_fallback(texture: &DecodedRgbaTexture, stage: usize) -> bool {
    !texture.from_jar && texture == &loader_destroy_fallbacks()[stage]
}

fn stage_needs_fallback(texture: &DecodedRgbaTexture, stage: usize) -> bool {
    !texture_is_usable(texture) || is_builtin_loader_destroy_fallback(texture, stage)
}

fn copy_stage_into_strip(
    source: &DecodedRgbaTexture,
    tile_width: u32,
    tile_height: u32,
    stage: usize,
    strip_width: u32,
    strip_rgba: &mut [u8],
) {
    let tile_w = tile_width as usize;
    let tile_h = tile_height as usize;
    let source_w = source.width as usize;
    let source_h = source.height as usize;
    let source_rgba = source.rgba.as_slice();
    let stride = strip_width as usize * 4;
    let x0 = stage * tile_w;

    if source.width == tile_width && source.height == tile_height {
        for y in 0..tile_h {
            let src = y * source_w * 4;
            let dst = y * stride + x0 * 4;
            strip_rgba[dst..dst + tile_w * 4].copy_from_slice(&source_rgba[src..src + tile_w * 4]);
        }
        return;
    }

    for y in 0..tile_h {
        let sy = ((y * source_h) / tile_h).min(source_h - 1);
        for x in 0..tile_w {
            let sx = ((x * source_w) / tile_w).min(source_w - 1);
            let src = (sy * source_w + sx) * 4;
            let dst = (y * strip_width as usize + x0 + x) * 4;
            strip_rgba[dst..dst + 4].copy_from_slice(&source_rgba[src..src + 4]);
        }
    }
}

fn write_procedural_stage(
    stage: u8,
    tile_width: u32,
    tile_height: u32,
    strip_width: u32,
    strip_rgba: &mut [u8],
) {
    let generated = DecodedRgbaTexture {
        width: tile_width,
        height: tile_height,
        rgba: procedural_destroy_stage_rgba(stage, tile_width, tile_height),
        from_jar: false,
    };
    copy_stage_into_strip(
        &generated,
        tile_width,
        tile_height,
        usize::from(stage),
        strip_width,
        strip_rgba,
    );
}

fn procedural_strip() -> DestroyStageStrip {
    let tile_width = 16;
    let tile_height = 16;
    let width = tile_width * CRACK_STAGE_COUNT;
    let mut rgba = vec![0; width as usize * tile_height as usize * 4];
    for stage in 0..10u8 {
        write_procedural_stage(stage, tile_width, tile_height, width, &mut rgba);
    }
    DestroyStageStrip {
        tile_width,
        width,
        height: tile_height,
        rgba,
        fallback_stages: [true; 10],
    }
}

/// Pack the ten destroy stages left-to-right into one RGBA strip.
///
/// Well-formed non-empty stages use their source texels (nearest-resampled if
/// a resource pack mixes dimensions). Malformed or fully transparent stages
/// are replaced by this module's deterministic fracture fallback so every
/// stage remains visible even without a usable local Vanilla JAR.
pub fn pack_destroy_stage_strip(assets: &VanillaWorldAssets) -> DestroyStageStrip {
    let Some((tile_width, tile_height)) = assets
        .destroy_stages
        .iter()
        .enumerate()
        .find(|(stage, texture)| !stage_needs_fallback(texture, *stage))
        .map(|(_, texture)| (texture.width, texture.height))
    else {
        return procedural_strip();
    };

    let Some(width) = tile_width.checked_mul(CRACK_STAGE_COUNT) else {
        return procedural_strip();
    };
    let Some(byte_len) = usize::try_from(width)
        .ok()
        .and_then(|w| {
            usize::try_from(tile_height)
                .ok()
                .and_then(|h| w.checked_mul(h))
        })
        .and_then(|pixels| pixels.checked_mul(4))
    else {
        return procedural_strip();
    };
    if byte_len > MAX_CRACK_STRIP_BYTES {
        return procedural_strip();
    }
    let mut rgba = vec![0; byte_len];
    let mut fallback_stages = [false; 10];
    for (stage, texture) in assets.destroy_stages.iter().enumerate() {
        if !stage_needs_fallback(texture, stage) {
            copy_stage_into_strip(texture, tile_width, tile_height, stage, width, &mut rgba);
        } else {
            fallback_stages[stage] = true;
            write_procedural_stage(stage as u8, tile_width, tile_height, width, &mut rgba);
        }
    }
    DestroyStageStrip {
        tile_width,
        width,
        height: tile_height,
        rgba,
        fallback_stages,
    }
}

fn fracture_hash(value: u32) -> u32 {
    let mut x = value;
    x ^= x >> 16;
    x = x.wrapping_mul(0x7feb_352d);
    x ^= x >> 15;
    x = x.wrapping_mul(0x846c_a68b);
    x ^ (x >> 16)
}

fn fracture_order(width: u32, height: u32) -> (Vec<usize>, usize) {
    let pixel_count = width as usize * height as usize;
    let mut visited = vec![false; pixel_count];
    let mut order = Vec::with_capacity(pixel_count);
    let center_x = (width as i32 - 1) / 2;
    let center_y = (height as i32 - 1) / 2;
    let directions = [
        (1, 0),
        (1, 1),
        (0, 1),
        (-1, 1),
        (-1, 0),
        (-1, -1),
        (0, -1),
        (1, -1),
    ];

    for branch in 0..8u32 {
        let mut x = center_x;
        let mut y = center_y;
        let mut direction = branch as usize;
        for step in 0..width.saturating_add(height) {
            if x >= 0 && y >= 0 && x < width as i32 && y < height as i32 {
                let index = y as usize * width as usize + x as usize;
                if !visited[index] {
                    visited[index] = true;
                    order.push(index);
                }
            }
            let noise = fracture_hash(
                (x as u32).wrapping_mul(0x9e37_79b9)
                    ^ (y as u32).wrapping_mul(0x85eb_ca6b)
                    ^ branch.wrapping_mul(0xc2b2_ae35)
                    ^ step.wrapping_mul(0x27d4_eb2f),
            );
            let turn = match noise % 9 {
                0 | 1 => 7,
                2 | 3 => 1,
                _ => 0,
            };
            direction = (direction + turn) % 8;
            x += directions[direction].0;
            y += directions[direction].1;
            if x < 0 || y < 0 || x >= width as i32 || y >= height as i32 {
                break;
            }
        }
    }

    let crack_len = order.len();

    // The radial walkers cover the crack-like pixels. This deterministic tail
    // is activated only at tiny or very elongated sizes where the walkers run
    // out of unique cells before the requested stage coverage.
    for y in 0..height as usize {
        for x in 0..width as usize {
            let index = y * width as usize + x;
            if !visited[index] {
                order.push(index);
            }
        }
    }
    (order, crack_len)
}

/// Generate one deterministic high-contrast fracture stage as RGBA8 pixels.
///
/// Later stages activate a strictly larger prefix of the same stable crack
/// ordering, so coverage increases monotonically from stage 0 through 9 while
/// preserving a continuous branching fracture shape.
pub fn procedural_destroy_stage_rgba(stage: u8, width: u32, height: u32) -> Vec<u8> {
    let Some(pixel_count) = (width as usize).checked_mul(height as usize) else {
        return Vec::new();
    };
    let Some(byte_len) = pixel_count.checked_mul(4) else {
        return Vec::new();
    };
    let mut rgba = vec![0; byte_len];
    if pixel_count == 0 {
        return rgba;
    }
    let stage = usize::from(stage.min(9));
    let (order, crack_len) = fracture_order(width, height);
    let scaled = crack_len * (stage + 1) * 8 / 100;
    let target = (stage + 1).max(scaled).min(pixel_count);
    for index in order.into_iter().take(target) {
        let at = index * 4;
        rgba[at] = 18;
        rgba[at + 1] = 18;
        rgba[at + 2] = 18;
        rgba[at + 3] = 232;
    }
    rgba
}

fn create_strip_texture(device: &wgpu::Device, width: u32, height: u32) -> wgpu::Texture {
    device.create_texture(&wgpu::TextureDescriptor {
        label: Some("block-break-crack-strip"),
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
    })
}

fn create_strip_bind_group(
    device: &wgpu::Device,
    layout: &wgpu::BindGroupLayout,
    uniform_buffer: &wgpu::Buffer,
    strip_view: &wgpu::TextureView,
    sampler: &wgpu::Sampler,
) -> wgpu::BindGroup {
    device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("block-break-crack-bind-group"),
        layout,
        entries: &[
            wgpu::BindGroupEntry {
                binding: 0,
                resource: uniform_buffer.as_entire_binding(),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: wgpu::BindingResource::TextureView(strip_view),
            },
            wgpu::BindGroupEntry {
                binding: 2,
                resource: wgpu::BindingResource::Sampler(sampler),
            },
        ],
    })
}

/// Retained GPU state for the block-break crack overlay.
pub struct BlockBreakCrackPass {
    pipeline: wgpu::RenderPipeline,
    bind_group_layout: wgpu::BindGroupLayout,
    bind_group: wgpu::BindGroup,
    uniform_buffer: wgpu::Buffer,
    vertex_buffer: wgpu::Buffer,
    index_buffer: wgpu::Buffer,
    strip_texture: wgpu::Texture,
    sampler: wgpu::Sampler,
    strip_width: u32,
    strip_height: u32,
}

impl BlockBreakCrackPass {
    /// Allocate every static resource used by the render pass. The initial
    /// strip is transparent until [`upload_destroy_stages`] is called.
    pub fn new(
        device: &wgpu::Device,
        target_format: wgpu::TextureFormat,
        depth_format: wgpu::TextureFormat,
    ) -> Self {
        let vertex_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("block-break-crack-vertices"),
            contents: bytemuck::cast_slice(&CRACK_CUBE_VERTICES),
            usage: wgpu::BufferUsages::VERTEX,
        });
        let index_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("block-break-crack-indices"),
            contents: bytemuck::cast_slice(&CRACK_CUBE_INDICES),
            usage: wgpu::BufferUsages::INDEX,
        });
        let uniform_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("block-break-crack-uniforms"),
            size: std::mem::size_of::<CrackUniforms>() as wgpu::BufferAddress,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let initial_width = 16 * CRACK_STAGE_COUNT;
        let initial_height = 16;
        let strip_texture = create_strip_texture(device, initial_width, initial_height);
        let strip_view = strip_texture.create_view(&wgpu::TextureViewDescriptor::default());
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("block-break-crack-nearest-sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Nearest,
            min_filter: wgpu::FilterMode::Nearest,
            mipmap_filter: wgpu::MipmapFilterMode::Nearest,
            ..Default::default()
        });
        let bind_group_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("block-break-crack-layout"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::VERTEX,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: wgpu::BufferSize::new(
                            std::mem::size_of::<CrackUniforms>() as u64,
                        ),
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });
        let bind_group = create_strip_bind_group(
            device,
            &bind_group_layout,
            &uniform_buffer,
            &strip_view,
            &sampler,
        );
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("block-break-crack-pipeline-layout"),
            bind_group_layouts: &[Some(&bind_group_layout)],
            immediate_size: 0,
        });
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("block-break-crack-wgsl"),
            source: wgpu::ShaderSource::Wgsl(std::borrow::Cow::Borrowed(CRACK_OVERLAY_WGSL)),
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("block-break-crack-pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_crack_overlay"),
                compilation_options: Default::default(),
                buffers: &[Some(CRACK_VERTEX_LAYOUT)],
            },
            primitive: wgpu::PrimitiveState {
                cull_mode: Some(wgpu::Face::Back),
                ..Default::default()
            },
            depth_stencil: Some(wgpu::DepthStencilState {
                format: depth_format,
                depth_write_enabled: Some(false),
                depth_compare: Some(wgpu::CompareFunction::LessEqual),
                stencil: wgpu::StencilState::default(),
                bias: wgpu::DepthBiasState::default(),
            }),
            multisample: wgpu::MultisampleState::default(),
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_crack_overlay"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: target_format,
                    blend: Some(CRACK_BLEND),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            multiview_mask: None,
            cache: None,
        });

        Self {
            pipeline,
            bind_group_layout,
            bind_group,
            uniform_buffer,
            vertex_buffer,
            index_buffer,
            strip_texture,
            sampler,
            strip_width: initial_width,
            strip_height: initial_height,
        }
    }

    /// Upload all ten Vanilla destroy stages as one horizontal GPU strip.
    /// Blank or malformed stages are replaced by the deterministic procedural
    /// fracture pattern before upload.
    pub fn upload_destroy_stages(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        assets: &VanillaWorldAssets,
    ) {
        let packed = pack_destroy_stage_strip(assets);
        let max_dimension = device.limits().max_texture_dimension_2d;
        let packed = if packed.width > max_dimension || packed.height > max_dimension {
            procedural_strip()
        } else {
            packed
        };

        if packed.width != self.strip_width || packed.height != self.strip_height {
            self.strip_texture = create_strip_texture(device, packed.width, packed.height);
            let strip_view = self
                .strip_texture
                .create_view(&wgpu::TextureViewDescriptor::default());
            self.bind_group = create_strip_bind_group(
                device,
                &self.bind_group_layout,
                &self.uniform_buffer,
                &strip_view,
                &self.sampler,
            );
            self.strip_width = packed.width;
            self.strip_height = packed.height;
        }

        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &self.strip_texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            &packed.rgba,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(packed.width * 4),
                rows_per_image: Some(packed.height),
            },
            wgpu::Extent3d {
                width: packed.width,
                height: packed.height,
                depth_or_array_layers: 1,
            },
        );
    }

    /// Encode the crack overlay for one target. `None` records no work. For an
    /// active target this performs one 80-byte uniform write and exactly one
    /// indexed draw; it allocates no heap memory in this crate's code.
    pub fn encode(
        &self,
        pass: &mut wgpu::RenderPass<'_>,
        queue: &wgpu::Queue,
        view_proj: [[f32; 4]; 4],
        target: Option<([i32; 3], u8)>,
    ) {
        let Some(target) = target else {
            return;
        };
        let uniforms = crack_uniform(view_proj, target);
        queue.write_buffer(&self.uniform_buffer, 0, bytemuck::bytes_of(&uniforms));
        pass.set_pipeline(&self.pipeline);
        pass.set_bind_group(0, &self.bind_group, &[]);
        pass.set_vertex_buffer(0, self.vertex_buffer.slice(..));
        pass.set_index_buffer(self.index_buffer.slice(..), wgpu::IndexFormat::Uint32);
        pass.draw_indexed(0..36, 0, 0..1);
    }
}
