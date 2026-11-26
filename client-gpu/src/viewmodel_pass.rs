// SPDX-License-Identifier: LGPL-3.0-or-later

use std::borrow::Cow;

use bytemuck::{Pod, Zeroable};

use crate::renderer::{FirstPersonUseKind, FirstPersonViewmodel};

pub const VIEWMODEL_MAX_VERTICES: usize = 256;
pub const VIEWMODEL_MAX_INDICES: usize = 384;

pub const VIEWMODEL_WGSL: &str = r#"
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
    out.color = in.color;
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
"#;

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
    bind_group: wgpu::BindGroup,
    uniform_buffer: wgpu::Buffer,
    vertex_buffer: wgpu::Buffer,
    index_buffer: wgpu::Buffer,
    surface_texture: wgpu::Texture,
    surface_view: wgpu::TextureView,
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
        let surface_texture = create_rgba_texture(device, "first-person-viewmodel-surface", 1, 1);
        let surface_view = surface_texture.create_view(&wgpu::TextureViewDescriptor::default());
        let glint_texture = create_rgba_texture(device, "first-person-viewmodel-glint", 1, 1);
        let glint_view = glint_texture.create_view(&wgpu::TextureViewDescriptor::default());
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("first-person-viewmodel-sampler"),
            address_mode_u: wgpu::AddressMode::Repeat,
            address_mode_v: wgpu::AddressMode::Repeat,
            address_mode_w: wgpu::AddressMode::Repeat,
            mag_filter: wgpu::FilterMode::Nearest,
            min_filter: wgpu::FilterMode::Nearest,
            mipmap_filter: wgpu::MipmapFilterMode::Nearest,
            ..wgpu::SamplerDescriptor::default()
        });
        let bind_group = make_bind_group(
            device,
            &bind_group_layout,
            &uniform_buffer,
            &surface_view,
            &sampler,
            &glint_view,
        );
        Self {
            pipeline,
            bind_group_layout,
            bind_group,
            uniform_buffer,
            vertex_buffer,
            index_buffer,
            surface_texture,
            surface_view,
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
            glint_fallback_pending: true,
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
        let texture = create_rgba_texture(device, "first-person-viewmodel-surface", width, height);
        write_rgba_texture(queue, &texture, width, height, rgba);
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
        let texture = create_rgba_texture(device, "first-person-viewmodel-glint", width, height);
        write_rgba_texture(queue, &texture, width, height, rgba);
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
        self.upload_pending_fallbacks(queue);
        let Some(info) = build_viewmodel_geometry(
            vm,
            aspect,
            uv_rect,
            time_secs,
            &mut self.vertices,
            &mut self.indices,
        ) else {
            return;
        };
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
            uv_rect,
            enchanted,
            time_secs,
            self.surface_loaded,
            self.glint_loaded,
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
                    load: wgpu::LoadOp::Clear(1.0),
                    store: wgpu::StoreOp::Store,
                }),
                stencil_ops: None,
            }),
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });
        pass.set_pipeline(&self.pipeline);
        pass.set_bind_group(0, &self.bind_group, &[]);
        pass.set_vertex_buffer(0, self.vertex_buffer.slice(..));
        pass.set_index_buffer(self.index_buffer.slice(..), wgpu::IndexFormat::Uint16);
        pass.draw_indexed(0..info.index_count as u32, 0, 0..1);
    }

    fn rebuild_bind_group(&mut self, device: &wgpu::Device) {
        self.bind_group = make_bind_group(
            device,
            &self.bind_group_layout,
            &self.uniform_buffer,
            &self.surface_view,
            &self.sampler,
            &self.glint_view,
        );
    }

    fn upload_pending_fallbacks(&mut self, queue: &wgpu::Queue) {
        if self.surface_fallback_pending {
            write_rgba_texture(queue, &self.surface_texture, 1, 1, &[255, 255, 255, 255]);
            self.surface_fallback_pending = false;
        }
        if self.glint_fallback_pending {
            write_rgba_texture(queue, &self.glint_texture, 1, 1, &[0, 0, 0, 0]);
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
            translation: [0.39 + anchor_adjust, -0.43, -0.68],
            rotation: [radians(66.0), radians(-14.0), radians(-19.0)],
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

#[allow(clippy::too_many_arguments)]
pub fn build_viewmodel_geometry(
    vm: &FirstPersonViewmodel,
    aspect: f32,
    uv_rect: Option<[f32; 4]>,
    time_secs: f32,
    vertices: &mut [ViewmodelVertex],
    indices: &mut [u16],
) -> Option<ViewmodelGeometryInfo> {
    if !vm.visible {
        return None;
    }
    let kind = classify_viewmodel(vm);
    let pose = viewmodel_pose(vm, aspect, time_secs);
    let mut writer = GeometryWriter {
        vertices,
        indices,
        vertex_count: 0,
        index_count: 0,
    };
    let base_color = base_color(vm, kind, uv_rect);
    let written = match kind {
        ViewmodelGeometryKind::BareArm => writer.append_box(
            0.07,
            0.21,
            0.07,
            [0.98, 0.72, 0.88, 0.82, 0.94, 0.86],
            base_color,
        ),
        ViewmodelGeometryKind::HeldBlock => writer.append_box(
            0.15,
            0.15,
            0.15,
            [1.0, 0.50, 0.80, 0.60, 0.86, 0.70],
            base_color,
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
                width,
                height,
                depth,
                [0.92, 0.58, 0.72, 0.66, 1.0, 0.78],
                base_color,
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

fn base_color(
    vm: &FirstPersonViewmodel,
    kind: ViewmodelGeometryKind,
    uv_rect: Option<[f32; 4]>,
) -> [f32; 4] {
    if kind == ViewmodelGeometryKind::BareArm && uv_rect.is_none() {
        return [0.93, 0.68, 0.50, 1.0];
    }
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
        half_x: f32,
        half_y: f32,
        half_z: f32,
        shades: [f32; 6],
        base_color: [f32; 4],
    ) -> bool {
        if self.vertex_count + 24 > self.vertices.len()
            || self.index_count + 36 > self.indices.len()
        {
            return false;
        }
        let x = half_x;
        let y = half_y;
        let z = half_z;
        self.append_quad(
            [[-x, y, -z], [x, y, -z], [x, y, z], [-x, y, z]],
            shades[0],
            base_color,
        );
        self.append_quad(
            [[-x, -y, z], [x, -y, z], [x, -y, -z], [-x, -y, -z]],
            shades[1],
            base_color,
        );
        self.append_quad(
            [[x, -y, -z], [x, -y, z], [x, y, z], [x, y, -z]],
            shades[2],
            base_color,
        );
        self.append_quad(
            [[-x, -y, z], [-x, -y, -z], [-x, y, -z], [-x, y, z]],
            shades[3],
            base_color,
        );
        self.append_quad(
            [[x, -y, z], [-x, -y, z], [-x, y, z], [x, y, z]],
            shades[4],
            base_color,
        );
        self.append_quad(
            [[-x, -y, -z], [x, -y, -z], [x, y, -z], [-x, y, -z]],
            shades[5],
            base_color,
        );
        true
    }

    fn append_quad(&mut self, corners: [[f32; 3]; 4], shade: f32, base_color: [f32; 4]) {
        let uv = [[0.0, 1.0], [1.0, 1.0], [1.0, 0.0], [0.0, 0.0]];
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

fn create_rgba_texture(
    device: &wgpu::Device,
    label: &str,
    width: u32,
    height: u32,
) -> wgpu::Texture {
    device.create_texture(&wgpu::TextureDescriptor {
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
    })
}

fn write_rgba_texture(
    queue: &wgpu::Queue,
    texture: &wgpu::Texture,
    width: u32,
    height: u32,
    rgba: &[u8],
) {
    queue.write_texture(
        wgpu::TexelCopyTextureInfo {
            texture,
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
