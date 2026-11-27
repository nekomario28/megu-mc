// SPDX-License-Identifier: LGPL-3.0-or-later

use std::borrow::Cow;

use bytemuck::{Pod, Zeroable};

use crate::gpu_texture::{create_srgb_color_texture, pixel_art_sampler, write_rgba8_subresource};
use crate::renderer::{FirstPersonUseKind, FirstPersonViewmodel};

pub const VIEWMODEL_MAX_VERTICES: usize = 256;
pub const VIEWMODEL_MAX_INDICES: usize = 384;

const ARM_SKIN_SIZE: u32 = 64;
const ARM_HALF_EXTENT: [f32; 3] = [0.0625, 0.1875, 0.0625];
const FULL_FACE_UVS: [[f32; 4]; 6] = [[0.0, 0.0, 1.0, 1.0]; 6];

// Face order matches append_box. Reversed cap axes keep the skin seams
// aligned with the front face along the hand and shoulder edges.
const RIGHT_ARM_UVS: [[f32; 4]; 6] = [
    arm_skin_uv([48, 20, 44, 16]), // +Y
    arm_skin_uv([52, 16, 48, 20]), // -Y
    arm_skin_uv([40, 20, 44, 32]), // +X
    arm_skin_uv([48, 20, 52, 32]), // -X
    arm_skin_uv([44, 20, 48, 32]), // +Z
    arm_skin_uv([52, 20, 56, 32]), // -Z
];

const fn arm_skin_uv([u0, v0, u1, v1]: [u8; 4]) -> [f32; 4] {
    let size = ARM_SKIN_SIZE as f32;
    [
        u0 as f32 / size,
        v0 as f32 / size,
        u1 as f32 / size,
        v1 as f32 / size,
    ]
}

pub const VIEWMODEL_WGSL: &str = megu_client_mesh::wgsl_with_srgb_decode!(
    r#"
struct ViewmodelUniforms {
    projection: mat4x4<f32>,
    uv_rect: vec4<f32>,
    light_color: vec4<f32>,
    glint_time: vec4<f32>,
    texture_state: vec4<f32>,
};

@group(0) @binding(0) var<uniform> uniforms: ViewmodelUniforms;
@group(0) @binding(1) var surface_texture: texture_2d<f32>;
@group(0) @binding(2) var surface_sampler: sampler;
@group(0) @binding(3) var glint_texture: texture_2d<f32>;

struct VertexInput {
    @location(0) position: vec3<f32>,
    @location(1) uv: vec2<f32>,
    @location(2) color: vec4<f32>,
};

struct VertexOutput {
    @builtin(position) clip_position: vec4<f32>,
    @location(0) uv: vec2<f32>,
    @location(1) color: vec4<f32>,
};

@vertex
fn vs_main(in: VertexInput) -> VertexOutput {
    var out: VertexOutput;
    out.clip_position = uniforms.projection * vec4<f32>(in.position, 1.0);
    out.uv = in.uv;
    out.color = vec4<f32>(srgb_to_linear_rgb(in.color.rgb), in.color.a);
    return out;
}

@fragment
fn fs_main(in: VertexOutput) -> @location(0) vec4<f32> {
    let uv_min = uniforms.uv_rect.xy;
    let uv_span = uniforms.uv_rect.zw - uniforms.uv_rect.xy;
    let surface_uv = uv_min + in.uv * uv_span;
    let surface_sample = textureSample(surface_texture, surface_sampler, surface_uv);
    let surface_texel = vec4<f32>(1.0, 1.0, 1.0, 1.0) * (1.0 - uniforms.texture_state.x)
        + surface_sample * uniforms.texture_state.x;
    let light_factor = vec4<f32>(max(uniforms.light_color.rgb, vec3<f32>(0.0, 0.0, 0.0)), 1.0);
    var color = surface_texel * in.color * light_factor;
    if (color.a < 0.05) {
        discard;
    }
    let glint_strength = uniforms.glint_time.x;
    if (glint_strength > 0.0) {
        let time_secs = uniforms.glint_time.y;
        let glint_scroll = vec2<f32>(time_secs * 0.135, time_secs * -0.087);
        let glint_uv = fract(in.uv * vec2<f32>(3.0, 3.0) + glint_scroll);
        let glint_sample = textureSample(glint_texture, surface_sampler, glint_uv);
        let glint_texel = glint_sample * uniforms.texture_state.y;
        let shimmer = 0.72 + 0.28 * sin(time_secs * 7.0 + in.uv.x * 9.0 + in.uv.y * 5.0);
        let glint_alpha = clamp(glint_texel.a * glint_strength * shimmer * 0.78, 0.0, 1.0);
        color = vec4<f32>(
            mix(color.rgb, glint_texel.rgb, vec3<f32>(glint_alpha, glint_alpha, glint_alpha)),
            color.a
        );
    }
    return color;
}
"#
);

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Pod, Zeroable)]
pub struct ViewmodelVertex {
    pub position: [f32; 3],
    pub uv: [f32; 2],
    pub color: [f32; 4],
}

impl ViewmodelVertex {
    const ZERO: Self = Self {
        position: [0.0; 3],
        uv: [0.0; 2],
        color: [0.0; 4],
    };
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ViewmodelGeometryKind {
    BareArm,
    HeldBlock,
    HeldItem,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ViewmodelPose {
    pub translation: [f32; 3],
    pub rotation: [f32; 3],
    pub scale: [f32; 3],
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ViewmodelGeometryInfo {
    pub kind: ViewmodelGeometryKind,
    pub vertex_count: usize,
    pub index_count: usize,
    pub pose: ViewmodelPose,
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct ViewmodelUniforms {
    projection: [[f32; 4]; 4],
    uv_rect: [f32; 4],
    light_color: [f32; 4],
    glint_time: [f32; 4],
    texture_state: [f32; 4],
}

const VIEWMODEL_ATTRS: [wgpu::VertexAttribute; 3] = [
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
    wgpu::VertexAttribute {
        format: wgpu::VertexFormat::Float32x4,
        offset: 20,
        shader_location: 2,
    },
];

const VIEWMODEL_VERTEX_LAYOUT: wgpu::VertexBufferLayout<'static> = wgpu::VertexBufferLayout {
    array_stride: std::mem::size_of::<ViewmodelVertex>() as wgpu::BufferAddress,
    step_mode: wgpu::VertexStepMode::Vertex,
    attributes: &VIEWMODEL_ATTRS,
};

pub struct FirstPersonViewmodelPass {
    pipeline: wgpu::RenderPipeline,
    bind_group_layout: wgpu::BindGroupLayout,
    surface_bind_group: wgpu::BindGroup,
    uniform_buffer: wgpu::Buffer,
    vertex_buffer: wgpu::Buffer,
    index_buffer: wgpu::Buffer,
    surface_texture: wgpu::Texture,
    surface_view: wgpu::TextureView,
    arm_skin_texture: wgpu::Texture,
    arm_skin_bind_group: wgpu::BindGroup,
    glint_texture: wgpu::Texture,
    glint_view: wgpu::TextureView,
    sampler: wgpu::Sampler,
    vertices: [ViewmodelVertex; VIEWMODEL_MAX_VERTICES],
    indices: [u16; VIEWMODEL_MAX_INDICES],
    surface_texture_size: (u32, u32),
    glint_texture_size: (u32, u32),
    surface_loaded: bool,
    glint_loaded: bool,
    surface_fallback_pending: bool,
    arm_skin_fallback_pending: bool,
    glint_fallback_pending: bool,
}

impl FirstPersonViewmodelPass {
    pub fn new(
        device: &wgpu::Device,
        target_format: wgpu::TextureFormat,
        depth_format: wgpu::TextureFormat,
    ) -> Self {
        let bind_group_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("first-person-viewmodel-layout"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
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
                wgpu::BindGroupLayoutEntry {
                    binding: 3,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
            ],
        });
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("first-person-viewmodel-shader"),
            source: wgpu::ShaderSource::Wgsl(Cow::Borrowed(VIEWMODEL_WGSL)),
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("first-person-viewmodel-pipeline-layout"),
            bind_group_layouts: &[Some(&bind_group_layout)],
            immediate_size: 0,
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("first-person-viewmodel-pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &module,
                entry_point: Some("vs_main"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                buffers: &[Some(VIEWMODEL_VERTEX_LAYOUT)],
            },
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                cull_mode: None,
                ..wgpu::PrimitiveState::default()
            },
            depth_stencil: Some(wgpu::DepthStencilState {
                format: depth_format,
                depth_write_enabled: Some(true),
                depth_compare: Some(wgpu::CompareFunction::LessEqual),
                stencil: wgpu::StencilState::default(),
                bias: wgpu::DepthBiasState::default(),
            }),
            multisample: wgpu::MultisampleState::default(),
            fragment: Some(wgpu::FragmentState {
                module: &module,
                entry_point: Some("fs_main"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: target_format,
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            multiview_mask: None,
            cache: None,
        });
        Self::with_pipeline(device, pipeline, bind_group_layout)
    }

    fn with_pipeline(
        device: &wgpu::Device,
        pipeline: wgpu::RenderPipeline,
        bind_group_layout: wgpu::BindGroupLayout,
    ) -> Self {
        let uniform_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("first-person-viewmodel-uniforms"),
            size: std::mem::size_of::<ViewmodelUniforms>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let vertex_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("first-person-viewmodel-vertices"),
            size: (std::mem::size_of::<ViewmodelVertex>() * VIEWMODEL_MAX_VERTICES) as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let index_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("first-person-viewmodel-indices"),
            size: (std::mem::size_of::<u16>() * VIEWMODEL_MAX_INDICES) as u64,
            usage: wgpu::BufferUsages::INDEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let surface_texture =
            create_srgb_color_texture(device, "first-person-viewmodel-surface", 1, 1, 1);
        let surface_view = surface_texture.create_view(&wgpu::TextureViewDescriptor::default());
        let arm_skin_texture = create_srgb_color_texture(
            device,
            "first-person-viewmodel-arm-skin",
            ARM_SKIN_SIZE,
            ARM_SKIN_SIZE,
            1,
        );
        let glint_texture =
            create_srgb_color_texture(device, "first-person-viewmodel-glint", 1, 1, 1);
        let glint_view = glint_texture.create_view(&wgpu::TextureViewDescriptor::default());
        let sampler = device.create_sampler(&pixel_art_sampler(wgpu::AddressMode::Repeat));
        let surface_bind_group = make_bind_group(
            device,
            &bind_group_layout,
            &uniform_buffer,
            &surface_view,
            &sampler,
            &glint_view,
        );
        let arm_skin_bind_group = make_bind_group(
            device,
            &bind_group_layout,
            &uniform_buffer,
            &arm_skin_texture.create_view(&wgpu::TextureViewDescriptor::default()),
            &sampler,
            &glint_view,
        );
        Self {
            pipeline,
            bind_group_layout,
            surface_bind_group,
            uniform_buffer,
            vertex_buffer,
            index_buffer,
            surface_texture,
            surface_view,
            arm_skin_texture,
            arm_skin_bind_group,
            glint_texture,
            glint_view,
            sampler,
            vertices: [ViewmodelVertex::ZERO; VIEWMODEL_MAX_VERTICES],
            indices: [0; VIEWMODEL_MAX_INDICES],
            surface_texture_size: (1, 1),
            glint_texture_size: (1, 1),
            surface_loaded: false,
            glint_loaded: false,
            surface_fallback_pending: true,
            arm_skin_fallback_pending: true,
            glint_fallback_pending: true,
        }
    }

    pub(crate) fn new_offhand(&self, device: &wgpu::Device) -> Self {
        let mut offhand = Self::with_pipeline(
            device,
            self.pipeline.clone(),
            self.bind_group_layout.clone(),
        );
        offhand.use_glint_from(device, self);
        offhand
    }

    pub(crate) fn use_glint_from(&mut self, device: &wgpu::Device, main_hand: &Self) {
        if main_hand.glint_loaded {
            self.glint_texture = main_hand.glint_texture.clone();
            self.glint_view = main_hand.glint_view.clone();
            self.glint_texture_size = main_hand.glint_texture_size;
            self.glint_loaded = true;
            self.glint_fallback_pending = false;
            self.rebuild_bind_group(device);
        }
    }

    pub const fn vertex_capacity(&self) -> usize {
        VIEWMODEL_MAX_VERTICES
    }

    pub const fn index_capacity(&self) -> usize {
        VIEWMODEL_MAX_INDICES
    }

    pub const fn surface_texture_size(&self) -> (u32, u32) {
        self.surface_texture_size
    }

    pub const fn glint_texture_size(&self) -> (u32, u32) {
        self.glint_texture_size
    }

    pub fn upload_surface_texture(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        width: u32,
        height: u32,
        rgba: &[u8],
    ) -> bool {
        if !valid_rgba_texture(device, width, height, rgba) {
            return false;
        }
        let texture =
            create_srgb_color_texture(device, "first-person-viewmodel-surface", width, height, 1);
        write_rgba8_subresource(queue, texture.as_image_copy(), texture.size(), rgba);
        self.surface_texture = texture;
        self.surface_view = self
            .surface_texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        self.surface_texture_size = (width, height);
        self.surface_loaded = true;
        self.surface_fallback_pending = false;
        self.rebuild_bind_group(device);
        true
    }

    /// Replace the arm's 64×64 skin sheet with runtime-loaded RGBA pixels.
    pub fn upload_arm_skin_texture(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        width: u32,
        height: u32,
        rgba: &[u8],
    ) -> bool {
        if width != ARM_SKIN_SIZE
            || height != ARM_SKIN_SIZE
            || !valid_rgba_texture(device, width, height, rgba)
        {
            return false;
        }
        write_rgba8_subresource(
            queue,
            self.arm_skin_texture.as_image_copy(),
            self.arm_skin_texture.size(),
            rgba,
        );
        self.arm_skin_fallback_pending = false;
        true
    }

    pub fn upload_glint_texture(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        width: u32,
        height: u32,
        rgba: &[u8],
    ) -> bool {
        if !valid_rgba_texture(device, width, height, rgba) {
            return false;
        }
        let texture =
            create_srgb_color_texture(device, "first-person-viewmodel-glint", width, height, 1);
        write_rgba8_subresource(queue, texture.as_image_copy(), texture.size(), rgba);
        self.glint_texture = texture;
        self.glint_view = self
            .glint_texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        self.glint_texture_size = (width, height);
        self.glint_loaded = true;
        self.glint_fallback_pending = false;
        self.rebuild_bind_group(device);
        true
    }

    #[allow(clippy::too_many_arguments)]
    pub fn encode(
        &mut self,
        queue: &wgpu::Queue,
        encoder: &mut wgpu::CommandEncoder,
        target: &wgpu::TextureView,
        dedicated_depth_view: &wgpu::TextureView,
        vm: &FirstPersonViewmodel,
        aspect: f32,
        light_rgb: [f32; 3],
        uv_rect: Option<[f32; 4]>,
        enchanted: bool,
        time_secs: f32,
    ) {
        self.encode_hand(
            queue,
            encoder,
            target,
            dedicated_depth_view,
            vm,
            aspect,
            light_rgb,
            uv_rect,
            enchanted,
            time_secs,
            false,
            wgpu::LoadOp::Clear(1.0),
        );
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn encode_hand(
        &mut self,
        queue: &wgpu::Queue,
        encoder: &mut wgpu::CommandEncoder,
        target: &wgpu::TextureView,
        dedicated_depth_view: &wgpu::TextureView,
        vm: &FirstPersonViewmodel,
        aspect: f32,
        light_rgb: [f32; 3],
        uv_rect: Option<[f32; 4]>,
        enchanted: bool,
        time_secs: f32,
        offhand: bool,
        depth_load: wgpu::LoadOp<f32>,
    ) -> bool {
        let Some(info) = build_hand_geometry(
            vm,
            aspect,
            time_secs,
            &mut self.vertices,
            &mut self.indices,
            offhand,
        ) else {
            return false;
        };
        let bare_arm = info.kind == ViewmodelGeometryKind::BareArm;
        self.upload_pending_fallbacks(queue, bare_arm);
        queue.write_buffer(
            &self.vertex_buffer,
            0,
            bytemuck::cast_slice(&self.vertices[..info.vertex_count]),
        );
        let index_write_count = info.index_count + (info.index_count & 1);
        queue.write_buffer(
            &self.index_buffer,
            0,
            bytemuck::cast_slice(&self.indices[..index_write_count]),
        );
        let uniforms = make_uniforms(
            aspect,
            light_rgb,
            uv_rect.filter(|_| !bare_arm),
            enchanted && !bare_arm,
            time_secs,
            bare_arm || self.surface_loaded,
            !bare_arm && self.glint_loaded,
        );
        queue.write_buffer(&self.uniform_buffer, 0, bytemuck::bytes_of(&uniforms));
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("first-person-viewmodel"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: target,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Load,
                    store: wgpu::StoreOp::Store,
                },
                depth_slice: None,
            })],
            depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                view: dedicated_depth_view,
                depth_ops: Some(wgpu::Operations {
                    load: depth_load,
                    store: wgpu::StoreOp::Store,
                }),
                stencil_ops: None,
            }),
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });
        pass.set_pipeline(&self.pipeline);
        let bind_group = if bare_arm {
            &self.arm_skin_bind_group
        } else {
            &self.surface_bind_group
        };
        pass.set_bind_group(0, bind_group, &[]);
        pass.set_vertex_buffer(0, self.vertex_buffer.slice(..));
        pass.set_index_buffer(self.index_buffer.slice(..), wgpu::IndexFormat::Uint16);
        pass.draw_indexed(0..info.index_count as u32, 0, 0..1);
        true
    }

    fn rebuild_bind_group(&mut self, device: &wgpu::Device) {
        self.surface_bind_group = make_bind_group(
            device,
            &self.bind_group_layout,
            &self.uniform_buffer,
            &self.surface_view,
            &self.sampler,
            &self.glint_view,
        );
    }

    fn upload_pending_fallbacks(&mut self, queue: &wgpu::Queue, bare_arm: bool) {
        if self.surface_fallback_pending {
            write_rgba8_subresource(
                queue,
                self.surface_texture.as_image_copy(),
                self.surface_texture.size(),
                &[255, 255, 255, 255],
            );
            self.surface_fallback_pending = false;
        }
        if bare_arm && self.arm_skin_fallback_pending {
            write_rgba8_subresource(
                queue,
                self.arm_skin_texture.as_image_copy(),
                self.arm_skin_texture.size(),
                &fallback_arm_skin(),
            );
            self.arm_skin_fallback_pending = false;
        }
        if self.glint_fallback_pending {
            write_rgba8_subresource(
                queue,
                self.glint_texture.as_image_copy(),
                self.glint_texture.size(),
                &[0, 0, 0, 0],
            );
            self.glint_fallback_pending = false;
        }
    }
}

pub fn classify_viewmodel(vm: &FirstPersonViewmodel) -> ViewmodelGeometryKind {
    if vm.held_item_name.is_none() {
        ViewmodelGeometryKind::BareArm
    } else if vm.is_block {
        ViewmodelGeometryKind::HeldBlock
    } else {
        ViewmodelGeometryKind::HeldItem
    }
}

pub fn viewmodel_pose(vm: &FirstPersonViewmodel, aspect: f32, time_secs: f32) -> ViewmodelPose {
    let kind = classify_viewmodel(vm);
    let aspect = finite_or(aspect, 16.0 / 9.0).clamp(0.25, 8.0);
    let anchor_adjust = (aspect - 16.0 / 9.0).clamp(-0.35, 0.45) * 0.025;
    let mut pose = match kind {
        ViewmodelGeometryKind::BareArm => ViewmodelPose {
            translation: [0.51 * aspect / (16.0 / 9.0), -0.43, -0.64],
            rotation: [radians(132.0), radians(23.0), radians(20.0)],
            scale: [1.0; 3],
        },
        ViewmodelGeometryKind::HeldBlock => ViewmodelPose {
            translation: [0.36 + anchor_adjust, -0.28, -0.72],
            rotation: [radians(24.0), radians(-38.0), radians(7.0)],
            scale: [1.0; 3],
        },
        ViewmodelGeometryKind::HeldItem => ViewmodelPose {
            translation: [0.34 + anchor_adjust, -0.35, -0.66],
            rotation: [radians(13.0), radians(-18.0), radians(-42.0)],
            scale: [1.0; 3],
        },
    };

    let swing = unit_interval(vm.swing_progress);
    if swing > 0.0 && swing < 1.0 {
        let rooted = swing.sqrt();
        let swing_arc = (rooted * std::f32::consts::PI).sin();
        let swing_sweep = (swing * std::f32::consts::PI).sin();
        pose.translation[0] -= 0.105 * swing_sweep;
        pose.translation[1] += 0.038 * (rooted * std::f32::consts::TAU).sin();
        pose.translation[2] -= 0.165 * swing_arc;
        pose.rotation[0] += radians(35.0) * swing_arc;
        pose.rotation[1] -= radians(28.0) * swing_sweep;
        pose.rotation[2] -= radians(18.0) * swing_sweep;
    }

    pose.translation[0] += finite_or(vm.walk_bob_x, 0.0).clamp(-0.12, 0.12) * 0.58;
    pose.translation[1] += finite_or(vm.walk_bob_y, 0.0).clamp(-0.12, 0.12) * 0.58;
    let equip = unit_interval(vm.equip_progress);
    pose.translation[1] -= (1.0 - equip) * 0.44;
    pose.rotation[0] += (1.0 - equip) * radians(18.0);

    let use_progress = unit_interval(vm.use_progress);
    let time_secs = finite_or(time_secs, 0.0);
    match vm.using_kind {
        FirstPersonUseKind::None => {}
        FirstPersonUseKind::Eat | FirstPersonUseKind::Drink => {
            let bring = 0.40 + 0.60 * use_progress;
            let munch_rate = if vm.using_kind == FirstPersonUseKind::Eat {
                4.0
            } else {
                2.5
            };
            let munch = (use_progress * std::f32::consts::TAU * munch_rate).sin();
            pose.translation[0] -= 0.18 * bring;
            pose.translation[1] += 0.13 * bring + 0.018 * munch;
            pose.translation[2] += 0.105 * bring + 0.012 * munch;
            pose.rotation[0] += radians(19.0) * bring + radians(3.0) * munch;
            pose.rotation[2] += radians(9.0) * bring;
        }
        FirstPersonUseKind::BowDraw | FirstPersonUseKind::CrossbowCharge => {
            let pull = use_progress;
            let tremor_scale = if vm.using_kind == FirstPersonUseKind::CrossbowCharge {
                0.006
            } else {
                0.004
            };
            let tremor = (time_secs * 57.0).sin() * tremor_scale * pull * pull;
            pose.translation[0] -= 0.065 * pull + tremor;
            pose.translation[1] += 0.026 * pull + tremor * 0.5;
            pose.translation[2] += 0.17 * pull;
            pose.rotation[1] += radians(17.0) * pull;
            pose.rotation[2] += radians(7.0) * pull;
        }
        FirstPersonUseKind::ShieldBlock => {
            let guard = 0.45 + 0.55 * use_progress;
            pose.translation[0] -= 0.43 * guard;
            pose.translation[1] += 0.105 * guard;
            pose.translation[2] += 0.045 * guard;
            pose.rotation[1] += radians(55.0) * guard;
            pose.rotation[2] += radians(20.0) * guard;
        }
        FirstPersonUseKind::TridentThrow => {
            let pull = use_progress;
            pose.translation[1] += 0.055 * pull;
            pose.translation[2] += 0.165 * pull;
            pose.rotation[0] -= radians(38.0) * pull;
            pose.rotation[2] += radians(12.0) * pull;
        }
    }

    for value in pose
        .translation
        .iter_mut()
        .chain(pose.rotation.iter_mut())
        .chain(pose.scale.iter_mut())
    {
        if !value.is_finite() {
            *value = 0.0;
        }
    }
    pose
}

/// Generate camera-space geometry with face UVs. Atlas subrects are applied
/// by the draw pass. The UV rect argument is retained for existing callers.
#[allow(clippy::too_many_arguments)]
pub fn build_viewmodel_geometry(
    vm: &FirstPersonViewmodel,
    aspect: f32,
    _uv_rect: Option<[f32; 4]>,
    time_secs: f32,
    vertices: &mut [ViewmodelVertex],
    indices: &mut [u16],
) -> Option<ViewmodelGeometryInfo> {
    build_hand_geometry(vm, aspect, time_secs, vertices, indices, false)
}

/// Generate the mirrored left-hand geometry for an equipped item.
pub fn build_offhand_viewmodel_geometry(
    vm: &FirstPersonViewmodel,
    aspect: f32,
    time_secs: f32,
    vertices: &mut [ViewmodelVertex],
    indices: &mut [u16],
) -> Option<ViewmodelGeometryInfo> {
    build_hand_geometry(vm, aspect, time_secs, vertices, indices, true)
}

fn build_hand_geometry(
    vm: &FirstPersonViewmodel,
    aspect: f32,
    time_secs: f32,
    vertices: &mut [ViewmodelVertex],
    indices: &mut [u16],
    offhand: bool,
) -> Option<ViewmodelGeometryInfo> {
    if !vm.visible || (offhand && vm.held_item_name.is_none()) {
        return None;
    }
    let kind = classify_viewmodel(vm);
    let mut pose = viewmodel_pose(vm, aspect, time_secs);
    let mut writer = GeometryWriter {
        vertices,
        indices,
        vertex_count: 0,
        index_count: 0,
    };
    let written = match kind {
        ViewmodelGeometryKind::BareArm => writer.append_box(
            ARM_HALF_EXTENT,
            [1.0, 0.55, 0.82, 0.90, 0.68, 0.78],
            [1.0; 4],
            RIGHT_ARM_UVS,
        ),
        ViewmodelGeometryKind::HeldBlock => writer.append_box(
            [0.15; 3],
            [1.0, 0.50, 0.80, 0.60, 0.86, 0.70],
            held_color(vm),
            FULL_FACE_UVS,
        ),
        ViewmodelGeometryKind::HeldItem => {
            let name = vm.held_item_name.as_deref().unwrap_or("");
            let (width, height, depth) = if name.contains("shield") {
                (0.20, 0.23, 0.018)
            } else if [
                "sword", "pickaxe", "axe", "shovel", "hoe", "trident", "bow", "crossbow",
            ]
            .iter()
            .any(|kind| name.contains(kind))
            {
                (0.14, 0.24, 0.014)
            } else {
                (0.17, 0.17, 0.014)
            };
            writer.append_box(
                [width, height, depth],
                [0.92, 0.58, 0.72, 0.66, 1.0, 0.78],
                held_color(vm),
                FULL_FACE_UVS,
            )
        }
    };
    if !written {
        return None;
    }
    for vertex in &mut writer.vertices[..writer.vertex_count] {
        vertex.position = transform_point(vertex.position, pose);
        for value in vertex.position {
            if !value.is_finite() {
                return None;
            }
        }
    }
    if offhand {
        // Reflect in camera space after applying the use and equip pose.
        for vertex in &mut writer.vertices[..writer.vertex_count] {
            vertex.position[0] = -vertex.position[0];
        }
        for triangle in writer.indices[..writer.index_count].as_chunks_mut::<3>().0 {
            triangle.swap(1, 2);
        }
        pose.translation[0] = -pose.translation[0];
        pose.rotation[1] = -pose.rotation[1];
        pose.rotation[2] = -pose.rotation[2];
        pose.scale[0] = -pose.scale[0];
    }
    Some(ViewmodelGeometryInfo {
        kind,
        vertex_count: writer.vertex_count,
        index_count: writer.index_count,
        pose,
    })
}

pub fn enchanted_glint_uv_offset(time_secs: f32) -> [f32; 2] {
    let time_secs = finite_or(time_secs, 0.0);
    [
        fract_positive(time_secs * 0.135),
        fract_positive(time_secs * -0.087),
    ]
}

fn held_color(vm: &FirstPersonViewmodel) -> [f32; 4] {
    let rgb = vm.held_color_rgb.map(|channel| {
        if channel.is_finite() {
            channel.clamp(0.0, 1.0)
        } else {
            1.0
        }
    });
    [rgb[0], rgb[1], rgb[2], 1.0]
}

struct GeometryWriter<'a> {
    vertices: &'a mut [ViewmodelVertex],
    indices: &'a mut [u16],
    vertex_count: usize,
    index_count: usize,
}

impl GeometryWriter<'_> {
    fn append_box(
        &mut self,
        [x, y, z]: [f32; 3],
        shades: [f32; 6],
        base_color: [f32; 4],
        face_uvs: [[f32; 4]; 6],
    ) -> bool {
        if self.vertex_count + 24 > self.vertices.len()
            || self.index_count + 36 > self.indices.len()
        {
            return false;
        }
        let faces = [
            [[-x, y, -z], [x, y, -z], [x, y, z], [-x, y, z]],
            [[-x, -y, z], [x, -y, z], [x, -y, -z], [-x, -y, -z]],
            [[x, -y, -z], [x, -y, z], [x, y, z], [x, y, -z]],
            [[-x, -y, z], [-x, -y, -z], [-x, y, -z], [-x, y, z]],
            [[x, -y, z], [-x, -y, z], [-x, y, z], [x, y, z]],
            [[-x, -y, -z], [x, -y, -z], [x, y, -z], [-x, y, -z]],
        ];
        for ((corners, shade), uv) in faces.into_iter().zip(shades).zip(face_uvs) {
            self.append_quad(corners, shade, base_color, uv);
        }
        true
    }

    fn append_quad(
        &mut self,
        corners: [[f32; 3]; 4],
        shade: f32,
        base_color: [f32; 4],
        [u0, v0, u1, v1]: [f32; 4],
    ) {
        let uv = [[u0, v1], [u1, v1], [u1, v0], [u0, v0]];
        let base = self.vertex_count as u16;
        for (corner, uv) in corners.into_iter().zip(uv) {
            self.vertices[self.vertex_count] = ViewmodelVertex {
                position: corner,
                uv,
                color: [
                    base_color[0] * shade,
                    base_color[1] * shade,
                    base_color[2] * shade,
                    base_color[3],
                ],
            };
            self.vertex_count += 1;
        }
        for index in [base, base + 1, base + 2, base, base + 2, base + 3] {
            self.indices[self.index_count] = index;
            self.index_count += 1;
        }
    }
}

fn transform_point(point: [f32; 3], pose: ViewmodelPose) -> [f32; 3] {
    let mut p = [
        point[0] * pose.scale[0],
        point[1] * pose.scale[1],
        point[2] * pose.scale[2],
    ];
    let (sx, cx) = pose.rotation[0].sin_cos();
    p = [p[0], p[1] * cx - p[2] * sx, p[1] * sx + p[2] * cx];
    let (sy, cy) = pose.rotation[1].sin_cos();
    p = [p[0] * cy + p[2] * sy, p[1], -p[0] * sy + p[2] * cy];
    let (sz, cz) = pose.rotation[2].sin_cos();
    p = [p[0] * cz - p[1] * sz, p[0] * sz + p[1] * cz, p[2]];
    [
        p[0] + pose.translation[0],
        p[1] + pose.translation[1],
        p[2] + pose.translation[2],
    ]
}

fn make_uniforms(
    aspect: f32,
    light_rgb: [f32; 3],
    uv_rect: Option<[f32; 4]>,
    enchanted: bool,
    time_secs: f32,
    surface_loaded: bool,
    glint_loaded: bool,
) -> ViewmodelUniforms {
    let light = light_rgb.map(|channel| {
        if channel.is_finite() {
            channel.clamp(0.0, 4.0)
        } else {
            1.0
        }
    });
    let uv_rect = uv_rect
        .filter(|rect| rect.iter().all(|v| v.is_finite()))
        .unwrap_or([0.0, 0.0, 1.0, 1.0]);
    ViewmodelUniforms {
        projection: viewmodel_projection(aspect),
        uv_rect,
        light_color: [light[0], light[1], light[2], 1.0],
        glint_time: [
            if enchanted { 1.0 } else { 0.0 },
            finite_or(time_secs, 0.0),
            0.0,
            0.0,
        ],
        texture_state: [f32::from(surface_loaded), f32::from(glint_loaded), 0.0, 0.0],
    }
}

fn viewmodel_projection(aspect: f32) -> [[f32; 4]; 4] {
    let aspect = finite_or(aspect, 16.0 / 9.0).clamp(0.25, 8.0);
    let near = 0.05;
    let far = 4.0;
    let f = 1.0 / (35.0_f32.to_radians()).tan();
    let depth_scale = far / (near - far);
    let depth_bias = far * near / (near - far);
    [
        [f / aspect, 0.0, 0.0, 0.0],
        [0.0, f, 0.0, 0.0],
        [0.0, 0.0, depth_scale, -1.0],
        [0.0, 0.0, depth_bias, 0.0],
    ]
}

fn fallback_arm_skin() -> [u8; (ARM_SKIN_SIZE * ARM_SKIN_SIZE * 4) as usize] {
    let mut rgba = [0; (ARM_SKIN_SIZE * ARM_SKIN_SIZE * 4) as usize];
    // Paint the shoulder and sleeve above the exposed forearm. The unused
    // regions of the sheet stay transparent.
    for y in 16..32 {
        for x in 40..56 {
            let color: [u8; 3] = match (x, y) {
                (44..=47, 16..=19) | (_, 20..=23) => [58, 115, 133],
                (_, 24) => [47, 96, 111],
                (48..=51, 16..=19) | (_, 25..=31) => [184, 134, 105],
                _ => continue,
            };
            let shade = ((x ^ y) & 3) as i8 * 2 - 3;
            let [r, g, b] = color.map(|channel| channel.saturating_add_signed(shade));
            let offset = ((y * ARM_SKIN_SIZE + x) * 4) as usize;
            rgba[offset..offset + 4].copy_from_slice(&[r, g, b, 255]);
        }
    }
    rgba
}

fn valid_rgba_texture(device: &wgpu::Device, width: u32, height: u32, rgba: &[u8]) -> bool {
    let max_dimension = device.limits().max_texture_dimension_2d;
    width > 0
        && height > 0
        && width <= max_dimension
        && height <= max_dimension
        && u64::from(width)
            .checked_mul(u64::from(height))
            .and_then(|pixels| pixels.checked_mul(4))
            .and_then(|bytes| usize::try_from(bytes).ok())
            == Some(rgba.len())
}

fn make_bind_group(
    device: &wgpu::Device,
    layout: &wgpu::BindGroupLayout,
    uniform_buffer: &wgpu::Buffer,
    surface_view: &wgpu::TextureView,
    sampler: &wgpu::Sampler,
    glint_view: &wgpu::TextureView,
) -> wgpu::BindGroup {
    device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("first-person-viewmodel-bind-group"),
        layout,
        entries: &[
            wgpu::BindGroupEntry {
                binding: 0,
                resource: uniform_buffer.as_entire_binding(),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: wgpu::BindingResource::TextureView(surface_view),
            },
            wgpu::BindGroupEntry {
                binding: 2,
                resource: wgpu::BindingResource::Sampler(sampler),
            },
            wgpu::BindGroupEntry {
                binding: 3,
                resource: wgpu::BindingResource::TextureView(glint_view),
            },
        ],
    })
}

fn radians(degrees: f32) -> f32 {
    degrees * std::f32::consts::PI / 180.0
}

fn finite_or(value: f32, fallback: f32) -> f32 {
    if value.is_finite() {
        value
    } else {
        fallback
    }
}

fn unit_interval(value: f32) -> f32 {
    finite_or(value, 0.0).clamp(0.0, 1.0)
}

fn fract_positive(value: f32) -> f32 {
    value - value.floor()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::renderer::{probe_adapters, request_gpu_device, GpuDevice};
    use megu_client_mesh::RenderBackendPreference;

    fn bare_arm() -> FirstPersonViewmodel {
        FirstPersonViewmodel {
            visible: true,
            equip_progress: 1.0,
            ..Default::default()
        }
    }

    #[test]
    fn bare_arm_uses_wide_skin_uvs_and_four_by_twelve_by_four_proportions() {
        let mut vertices = [ViewmodelVertex::ZERO; VIEWMODEL_MAX_VERTICES];
        let mut indices = [0; VIEWMODEL_MAX_INDICES];
        let info = build_viewmodel_geometry(
            &bare_arm(),
            16.0 / 9.0,
            None,
            0.0,
            &mut vertices,
            &mut indices,
        )
        .unwrap();
        assert_eq!(info.kind, ViewmodelGeometryKind::BareArm);
        assert_eq!((info.vertex_count, info.index_count), (24, 36));
        let faces = [
            ([[48, 16], [44, 16], [44, 20], [48, 20]], 1.0),
            ([[52, 20], [48, 20], [48, 16], [52, 16]], 0.55),
            ([[40, 32], [44, 32], [44, 20], [40, 20]], 0.82),
            ([[48, 32], [52, 32], [52, 20], [48, 20]], 0.90),
            ([[44, 32], [48, 32], [48, 20], [44, 20]], 0.68),
            ([[52, 32], [56, 32], [56, 20], [52, 20]], 0.78),
        ];
        for (face, (texels, shade)) in vertices[..24].as_chunks::<4>().0.iter().zip(faces) {
            for (vertex, uv) in face.iter().zip(texels) {
                assert_eq!(vertex.uv, uv.map(|coord| coord as f32 / 64.0));
                assert_eq!(vertex.color, [shade, shade, shade, 1.0]);
            }
        }
        let distance = |a: usize, b: usize| {
            vertices[a]
                .position
                .iter()
                .zip(vertices[b].position)
                .map(|(a, b)| (a - b).powi(2))
                .sum::<f32>()
                .sqrt()
        };
        for (actual, expected) in [
            (distance(0, 1), 0.125),
            (distance(1, 2), 0.125),
            (distance(9, 10), 0.375),
        ] {
            assert!((actual - expected).abs() < 1e-6);
        }

        let wrist = transform_point([0.0, -0.1875, 0.0], info.pose);
        let shoulder = transform_point([0.0, 0.1875, 0.0], info.pose);
        assert!(wrist[0] > 0.0 && wrist[0] < shoulder[0]);
        assert!(wrist[1] > shoulder[1] && wrist[2] < shoulder[2]);
        let fov_y = 2.0 * (1.0 / viewmodel_projection(16.0 / 9.0)[1][1]).atan();
        assert!((fov_y.to_degrees() - 70.0).abs() < 1e-5);
    }

    #[test]
    fn fallback_skin_has_opaque_arm_faces_and_distinct_sleeve_and_wrist_colors() {
        let rgba = fallback_arm_skin();
        let pixel = |x, y| {
            let start = ((y * ARM_SKIN_SIZE + x) * 4) as usize;
            <[u8; 4]>::try_from(&rgba[start..start + 4]).unwrap()
        };
        for y in 20..32 {
            for x in 40..56 {
                assert_eq!(pixel(x, y)[3], 255);
            }
        }
        let top = pixel(44, 16);
        let sleeve = pixel(44, 20);
        let forearm = pixel(44, 28);
        let bottom = pixel(48, 16);
        assert_eq!(top, sleeve);
        assert_eq!(bottom, forearm);
        assert!(sleeve[2] > sleeve[0] && forearm[0] > forearm[2]);
        assert_ne!(pixel(44, 28), pixel(45, 28), "forearm uses pixel shading");
        assert_eq!(pixel(0, 0), [0; 4]);
    }

    #[test]
    fn shader_validates_and_decodes_vertex_srgb() {
        use wgpu::naga::{front::wgsl, valid::*};

        let module = wgsl::parse_str(VIEWMODEL_WGSL).expect("viewmodel WGSL parses");
        Validator::new(ValidationFlags::all(), Capabilities::empty())
            .validate(&module)
            .expect("viewmodel WGSL validates");
        assert!(VIEWMODEL_WGSL.contains("srgb_to_linear_rgb(in.color.rgb)"));
    }

    #[test]
    fn skin_upload_and_draw_keep_the_arm_and_item_atlas_independent() {
        let backend = RenderBackendPreference::Auto;
        if probe_adapters(backend).is_empty() {
            eprintln!("SKIP viewmodel arm texture test: no wgpu adapter available");
            return;
        }
        let ctx = request_gpu_device(backend, None, None).expect("request viewmodel test device");
        let mut pass = FirstPersonViewmodelPass::new(
            &ctx.device,
            wgpu::TextureFormat::Rgba8UnormSrgb,
            wgpu::TextureFormat::Depth32Float,
        );
        let arm = bare_arm();
        let held = FirstPersonViewmodel {
            held_item_name: Some("minecraft:stone".to_owned()),
            is_block: true,
            ..bare_arm()
        };
        let atlas = [160, 0, 0, 255].repeat(64 * 64);
        assert!(pass.upload_surface_texture(&ctx.device, &ctx.queue, 64, 64, &atlas));
        assert_eq!(
            pass.surface_texture.format(),
            wgpu::TextureFormat::Rgba8UnormSrgb
        );
        assert_eq!(
            pass.arm_skin_texture.format(),
            wgpu::TextureFormat::Rgba8UnormSrgb
        );

        let fallback = draw_pixels(&ctx, &mut pass, &arm);
        assert!(fallback.iter().any(|p| p[3] != 0 && p[0] > p[2]));
        assert!(fallback.iter().any(|p| p[3] != 0 && p[2] > p[0]));

        let mut skin = fallback_arm_skin();
        for pixel in skin
            .as_chunks_mut::<4>()
            .0
            .iter_mut()
            .filter(|pixel| pixel[3] != 0)
        {
            pixel.copy_from_slice(&[0, 160, 0, 255]);
        }
        assert!(pass.upload_arm_skin_texture(&ctx.device, &ctx.queue, 64, 64, &skin));
        assert!(!pass.upload_arm_skin_texture(&ctx.device, &ctx.queue, 64, 64, &skin[..4]));
        assert_eq!(pass.surface_texture_size(), (64, 64));
        for (vm, lit_channel) in [(&arm, 1), (&held, 0)] {
            let pixels = draw_pixels(&ctx, &mut pass, vm);
            let visible: Vec<_> = pixels.iter().filter(|p| p[3] != 0).collect();
            assert!(
                visible.len() > 16,
                "viewmodel did not rasterize enough pixels"
            );
            assert!(visible.iter().all(|p| {
                p[lit_channel] > 0 && p[lit_channel] <= 161 && p[1 - lit_channel] == 0 && p[2] == 0
            }));
        }
    }

    fn draw_pixels(
        ctx: &GpuDevice,
        pass: &mut FirstPersonViewmodelPass,
        vm: &FirstPersonViewmodel,
    ) -> Vec<[u8; 4]> {
        // At 64 pixels wide, a row of RGBA is already copy-aligned.
        let size = wgpu::Extent3d {
            width: 64,
            height: 36,
            depth_or_array_layers: 1,
        };
        let texture = |format, usage| {
            ctx.device.create_texture(&wgpu::TextureDescriptor {
                label: None,
                size,
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format,
                usage,
                view_formats: &[],
            })
        };
        let target = texture(
            wgpu::TextureFormat::Rgba8UnormSrgb,
            wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        );
        let depth = texture(
            wgpu::TextureFormat::Depth32Float,
            wgpu::TextureUsages::RENDER_ATTACHMENT,
        );
        let readback = ctx.device.create_buffer(&wgpu::BufferDescriptor {
            label: None,
            size: u64::from(size.width * size.height * 4),
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut encoder = ctx.device.create_command_encoder(&Default::default());
        pass.encode(
            &ctx.queue,
            &mut encoder,
            &target.create_view(&Default::default()),
            &depth.create_view(&Default::default()),
            vm,
            16.0 / 9.0,
            [1.0; 3],
            Some([0.0, 0.0, 0.125, 0.125]),
            false,
            0.0,
        );
        encoder.copy_texture_to_buffer(
            target.as_image_copy(),
            wgpu::TexelCopyBufferInfo {
                buffer: &readback,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(size.width * 4),
                    rows_per_image: Some(size.height),
                },
            },
            size,
        );
        ctx.queue.submit([encoder.finish()]);
        let slice = readback.slice(..);
        let (tx, rx) = std::sync::mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |result| {
            tx.send(result).unwrap();
        });
        ctx.device
            .poll(wgpu::PollType::wait_indefinitely())
            .unwrap();
        rx.recv().unwrap().unwrap();
        let mapped = slice.get_mapped_range().unwrap();
        bytemuck::cast_slice(&mapped).to_vec()
    }
}
