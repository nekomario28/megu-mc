// SPDX-License-Identifier: LGPL-3.0-or-later
//! Opaque section-local terrain quads drawn directly from a storage buffer.

use std::fmt::Write as _;
use std::mem::{align_of, size_of};

use bytemuck::{Pod, Zeroable};
use megu_client_mesh::compact_vertex::{
    emit_wgsl_quad_pull_unpack, CompactVoxelVertex, PlanarQuadRecord,
};
use megu_client_mesh::DIR_BASIS;
use wgpu::util::DeviceExt;

use crate::renderer::{depth_state, map_rgba_readback, DEPTH_FORMAT, OFFSCREEN_FORMAT};
use crate::{aligned_copy_bytes_per_row, Camera, Mat4};

/// Storage-buffer element with the same WGSL array stride as `PlanarQuadRecord`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Pod, Zeroable)]
pub struct GpuPlanarQuad {
    pub state_id: u32,
    pub payload: [u32; 5],
}

const _: () = assert!(size_of::<GpuPlanarQuad>() == 24);
const _: () = assert!(size_of::<GpuPlanarQuad>() == size_of::<PlanarQuadRecord>());
const _: () = assert!(align_of::<GpuPlanarQuad>() == align_of::<PlanarQuadRecord>());

impl GpuPlanarQuad {
    /// Views the source records in GPU layout without repacking or allocating.
    pub fn from_records(records: &[PlanarQuadRecord]) -> &[Self] {
        // SAFETY: Both repr(C) records contain a u32 followed by five u32s,
        // with the same alignment and no padding. All u32 patterns are valid.
        // The shared slice retains the length and lifetime of the source.
        unsafe { std::slice::from_raw_parts(records.as_ptr().cast(), records.len()) }
    }
}

/// Geometry-buffer payload bytes for the same quads in each pipeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PullingVramReport {
    pub legacy_vertex_bytes: u128,
    pub legacy_index_bytes: u128,
    pub pulling_storage_bytes: u128,
    pub pulling_index_bytes: u128,
}

/// Compare the compact indexed path (48 + 24 B/quad) with pulling (24 B/quad).
/// The wide counters support every `usize` quad count without overflow.
pub fn vram_footprint_report(quad_count: usize) -> PullingVramReport {
    let quads = quad_count as u128;
    PullingVramReport {
        legacy_vertex_bytes: quads * (4 * size_of::<CompactVoxelVertex>()) as u128,
        legacy_index_bytes: quads * (6 * size_of::<u32>()) as u128,
        pulling_storage_bytes: quads * size_of::<GpuPlanarQuad>() as u128,
        pulling_index_bytes: 0,
    }
}

/// Builds WGSL from the mesh crate's field schema and face basis.
pub fn build_planar_quad_pulling_wgsl() -> String {
    let mut shader = emit_wgsl_quad_pull_unpack();
    for (name, axes) in [
        ("FACE_U", DIR_BASIS.map(|basis| basis.u_axis)),
        ("FACE_V", DIR_BASIS.map(|basis| basis.v_axis)),
    ] {
        writeln!(shader, "const {name} = array<vec3<f32>, 6>(").unwrap();
        for [x, y, z] in axes {
            writeln!(shader, "    vec3<f32>({x}.0, {y}.0, {z}.0),").unwrap();
        }
        shader.push_str(");\n");
    }
    shader.push_str(PULLING_WGSL);
    shader
}

const PULLING_WGSL: &str = r#"
@group(0) @binding(0) var<storage, read> quads: array<PlanarQuadRecord>;
@group(0) @binding(1) var<uniform> view_proj: mat4x4<f32>;

// Same outward winding and AO diagonal choices as PlanarQuadRecord::indices.
const DIAGONAL_02 = array<u32, 6>(0u, 1u, 2u, 0u, 2u, 3u);
const DIAGONAL_13 = array<u32, 6>(0u, 1u, 3u, 1u, 2u, 3u);
const FACE_SHADE = array<f32, 6>(0.45, 1.0, 0.75, 0.85, 0.7, 0.95);
const AO_SHADE = array<f32, 4>(0.4, 0.6, 0.8, 1.0);

fn field(quad: PlanarQuadRecord, offset: u32, mask: u32) -> u32 {
    return planar_quad_field(quad.payload, offset, countOneBits(mask));
}

fn rgb24(packed: u32) -> vec3<f32> {
    return vec3<f32>(
        f32(extractBits(packed, 16u, 8u)),
        f32(extractBits(packed, 8u, 8u)),
        f32(extractBits(packed, 0u, 8u)),
    ) / 255.0;
}

struct VertexOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) color: vec3<f32>,
};

@vertex
fn vs_main(@builtin(vertex_index) vi: u32) -> VertexOut {
    let quad_idx = vi / 6u;
    let tri_vert = vi % 6u;
    let quad = quads[quad_idx];
    let flipped = field(quad, PQ_DIAGONAL_OFFSET, PQ_DIAGONAL_MASK) != 0u;
    let corner = select(DIAGONAL_02[tri_vert], DIAGONAL_13[tri_vert], flipped);
    let face = field(quad, PQ_FACE_OFFSET, PQ_FACE_MASK);

    let origin = vec3<f32>(
        f32(field(quad, PQ_ORIGIN_X_FIXED_OFFSET, PQ_ORIGIN_X_FIXED_MASK)),
        f32(field(quad, PQ_ORIGIN_Y_FIXED_OFFSET, PQ_ORIGIN_Y_FIXED_MASK)),
        f32(field(quad, PQ_ORIGIN_Z_FIXED_OFFSET, PQ_ORIGIN_Z_FIXED_MASK)),
    );
    let span_u = f32(field(quad, PQ_SPAN_U_FIXED_OFFSET, PQ_SPAN_U_FIXED_MASK));
    let span_v = f32(field(quad, PQ_SPAN_V_FIXED_OFFSET, PQ_SPAN_V_FIXED_MASK));
    let position = (origin
        + FACE_U[face] * select(0.0, span_u, corner == 1u || corner == 2u)
        + FACE_V[face] * select(0.0, span_v, corner >= 2u)) / 1024.0;

    let ao = extractBits(field(quad, PQ_AO_OFFSET, PQ_AO_MASK), corner * 2u, 2u);
    let lights = field(quad, PQ_CORNER_LIGHT_OFFSET, PQ_CORNER_LIGHT_MASK);
    let block = extractBits(lights, corner * 8u, 4u);
    let sky = extractBits(lights, corner * 8u + 4u, 4u);
    let illumination = 0.25 + 0.75 * (f32(max(block, sky)) / 15.0);
    let shade = FACE_SHADE[face] * AO_SHADE[ao] * illumination;
    let tint = field(quad, PQ_TINT_OFFSET, PQ_TINT_MASK);
    // The standalone opaque pass uses a deterministic palette for untinted
    // faces. Hash in integer space to preserve the full state-id domain.
    let palette = 0.5 + 0.5 * rgb24((quad.state_id * 0x9e3779b9u) >> 8u);
    let albedo = select(palette, rgb24(tint), tint != 0u);
    return VertexOut(view_proj * vec4<f32>(position, 1.0), albedo * shade);
}

@fragment
fn fs_main(in: VertexOut) -> @location(0) vec4<f32> {
    return vec4<f32>(in.color, 1.0);
}
"#;

/// Depth-tested triangle list with all geometry sourced from storage.
pub struct PlanarQuadPullingPipeline {
    pipeline: wgpu::RenderPipeline,
    layout: wgpu::BindGroupLayout,
}

impl PlanarQuadPullingPipeline {
    /// Create the pipeline for the renderer's offscreen color and depth formats.
    pub fn new(device: &wgpu::Device) -> Self {
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("planar-quad-pulling"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::VERTEX,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Storage { read_only: true },
                        has_dynamic_offset: false,
                        min_binding_size: wgpu::BufferSize::new(size_of::<GpuPlanarQuad>() as u64),
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::VERTEX,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: wgpu::BufferSize::new(size_of::<Mat4>() as u64),
                    },
                    count: None,
                },
            ],
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("planar-quad-pulling"),
            bind_group_layouts: &[Some(&layout)],
            immediate_size: 0,
        });
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("planar-quad-pulling"),
            source: wgpu::ShaderSource::Wgsl(build_planar_quad_pulling_wgsl().into()),
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("planar-quad-pulling"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &module,
                entry_point: Some("vs_main"),
                compilation_options: Default::default(),
                buffers: &[],
            },
            primitive: wgpu::PrimitiveState {
                cull_mode: Some(wgpu::Face::Back),
                ..Default::default()
            },
            depth_stencil: Some(depth_state(true)),
            multisample: Default::default(),
            fragment: Some(wgpu::FragmentState {
                module: &module,
                entry_point: Some("fs_main"),
                compilation_options: Default::default(),
                targets: &[Some(OFFSCREEN_FORMAT.into())],
            }),
            multiview_mask: None,
            cache: None,
        });
        Self { pipeline, layout }
    }

    /// Draw section-local quads and return tightly packed sRGB RGBA8 pixels.
    /// Camera coordinates are relative to the section origin.
    pub fn render_offscreen_rgba(
        &self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        quads: &[PlanarQuadRecord],
        camera: &Camera,
        width: u32,
        height: u32,
    ) -> Result<Vec<u8>, String> {
        let view_proj = camera.view_projection(width.max(1) as f32 / height.max(1) as f32);
        self.render_offscreen_rgba_with_view_projection(
            device, queue, quads, &view_proj, width, height,
        )
    }

    /// Render with a caller-provided section-local view-projection matrix.
    /// Uncovered pixels are transparent, including the entire output for an
    /// empty quad slice.
    pub fn render_offscreen_rgba_with_view_projection(
        &self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        quads: &[PlanarQuadRecord],
        view_proj: &Mat4,
        width: u32,
        height: u32,
    ) -> Result<Vec<u8>, String> {
        let limits = device.limits();
        let max_dimension = limits.max_texture_dimension_2d;
        if width == 0 || height == 0 || width > max_dimension || height > max_dimension {
            return Err(format!(
                "invalid planar quad framebuffer size {width}x{height}"
            ));
        }
        let vertex_count = u32::try_from(quads.len())
            .ok()
            .and_then(|count| count.checked_mul(6))
            .ok_or("planar quad draw exceeds the u32 vertex count")?;
        let quad_bytes: &[u8] = bytemuck::cast_slice(GpuPlanarQuad::from_records(quads));
        let max_geometry_bytes = limits
            .max_storage_buffer_binding_size
            .min(limits.max_buffer_size);
        if quad_bytes.len() as u64 > max_geometry_bytes {
            return Err(format!(
                "planar quad buffer exceeds the {max_geometry_bytes}-byte limit"
            ));
        }
        let padded_row = aligned_copy_bytes_per_row(width);
        let readback_size = u64::from(padded_row) * u64::from(height);
        if readback_size > limits.max_buffer_size || usize::try_from(readback_size).is_err() {
            return Err("planar quad framebuffer exceeds the readback buffer limit".into());
        }

        let bind_group = if quads.is_empty() {
            None
        } else {
            let geometry = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("planar-quad-records"),
                contents: quad_bytes,
                usage: wgpu::BufferUsages::STORAGE,
            });
            let uniform = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("planar-quad-camera"),
                contents: bytemuck::bytes_of(view_proj),
                usage: wgpu::BufferUsages::UNIFORM,
            });
            Some(device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("planar-quad-pulling"),
                layout: &self.layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: geometry.as_entire_binding(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: uniform.as_entire_binding(),
                    },
                ],
            }))
        };

        let size = wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        };
        let color_desc = wgpu::TextureDescriptor {
            label: Some("planar-quad-offscreen"),
            size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: OFFSCREEN_FORMAT,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        };
        let color = device.create_texture(&color_desc);
        let depth = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("planar-quad-depth"),
            format: DEPTH_FORMAT,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            ..color_desc
        });
        let color_view = color.create_view(&wgpu::TextureViewDescriptor::default());
        let depth_view = depth.create_view(&wgpu::TextureViewDescriptor::default());
        let readback = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("planar-quad-readback"),
            size: readback_size,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("planar-quad-offscreen"),
        });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("planar-quad-pulling"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &color_view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                    view: &depth_view,
                    depth_ops: Some(wgpu::Operations {
                        load: wgpu::LoadOp::Clear(1.0),
                        store: wgpu::StoreOp::Discard,
                    }),
                    stencil_ops: None,
                }),
                ..Default::default()
            });
            if let Some(bind_group) = &bind_group {
                pass.set_pipeline(&self.pipeline);
                pass.set_bind_group(0, bind_group, &[]);
                pass.draw(0..vertex_count, 0..1);
            }
        }
        encoder.copy_texture_to_buffer(
            color.as_image_copy(),
            wgpu::TexelCopyBufferInfo {
                buffer: &readback,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(padded_row),
                    rows_per_image: None,
                },
            },
            size,
        );
        queue.submit([encoder.finish()]);
        map_rgba_readback(device, &readback, width, padded_row, height).map_err(|e| e.to_string())
    }
}
