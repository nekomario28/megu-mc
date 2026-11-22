// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! Real `wgpu` renderer: packed-vertex terrain pipelines, the `client-ui`
//! RGBA overlay blit, a depth-buffered camera, and a device-validated Megu
//! native WGSL shaderpack hook that never crashes — any rejection falls back
//! to the built-in pipeline with a human-readable notice.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::path::Path;

use bytemuck::{Pod, Zeroable};
use mc_spine_client::shaders::{
    validate_wgsl_module, WgslStage, TERRAIN_OPAQUE_WGSL, TERRAIN_TRANSLUCENT_WGSL,
};
use mc_spine_client::world::WorldState;
use mc_spine_client_mesh::builtin_shadow_wgsl::{
    compute_sun_direction, negotiate_builtin_shadow_quality, pack_vfx1_lights,
    BuiltinShadowQuality, DirtyOnlyShadowCache, RenderScalePreset, ShadowCacheDecision,
};
use mc_spine_client_mesh::ffi::PackedVertex;
use mc_spine_client_mesh::vfx_engine::DynamicColoredLight;
use mc_spine_client_mesh::SectionCoord;
use mc_spine_client_mesh::{
    is_megu_wgsl_pack, BackendResolution, GpuCapabilities, GpuShaderCaps, MeguWgslPack, PassKind,
    PipelineStage, RenderBackendPreference, ShaderPackSource,
};
use mc_spine_kernel::entity_interp::{evaluate_sky_and_fog, SkyFogInput, SkyFogOutput};
use winit::event_loop::OwnedDisplayHandle;

use crate::atmosphere::{BiomeAtmosphereBlender, ChunkFadeTracker};
use crate::mat4_mul;
use crate::subworld_render::{
    CompositeDrawUniforms, CompositeEnvUniforms, SubWorldGpuCache, SubWorldRenderBank,
    VfxLightsGpu, COMPOSITE_LIT_WGSL, COMPOSITE_TERRAIN_WGSL, DRAW_UNIFORM_STRIDE,
    LIT_VERTEX_LAYOUT, PART_INSTANCE_LAYOUT,
};
use crate::{
    build_index_buffer, gpu_capabilities_for, gpu_shader_caps_for, negotiate_pipeline,
    pack_quads_to_vertices, wgpu_backends_for, Camera, DemoScene, Mat4,
};

// ---------------------------------------------------------------------------
// Error surface (everything is a String inside; the type exists for callers)
// ---------------------------------------------------------------------------

/// Every way renderer/device setup can fail.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RendererError {
    /// `wgpu` found no adapter matching the backend preference.
    NoAdapter(String),
    /// The adapter refused a logical device.
    Device(String),
    /// Packed-quad upload failed the client-port ABI contract.
    PackFailed(String),
    /// The surface could not be created or configured.
    Surface(String),
    /// GPU readback failed.
    Readback(String),
}

impl std::fmt::Display for RendererError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoAdapter(msg) => write!(f, "no usable GPU adapter: {msg}"),
            Self::Device(msg) => write!(f, "device request failed: {msg}"),
            Self::PackFailed(msg) => write!(f, "quad packing failed: {msg}"),
            Self::Surface(msg) => write!(f, "surface error: {msg}"),
            Self::Readback(msg) => write!(f, "GPU readback failed: {msg}"),
        }
    }
}

impl std::error::Error for RendererError {}

// ---------------------------------------------------------------------------
// Headless probe (also the `--headless-probe` printer's data source)
// ---------------------------------------------------------------------------

/// Everything the probe reports about one enumerated adapter.
#[derive(Clone, Debug)]
pub struct AdapterReport {
    pub name: String,
    pub backend: wgpu::Backend,
    pub device_type: wgpu::DeviceType,
    pub vendor_id: u32,
    pub device_id: u32,
    pub driver: String,
    pub driver_info: String,
    pub caps: GpuCapabilities,
    pub resolution: BackendResolution,
    pub shader_caps: GpuShaderCaps,
    pub limits: wgpu::Limits,
}

/// Enumerate real adapters for `pref` (no display required). Returns an empty
/// vec on truly headless machines rather than failing — `--headless-probe`
/// treats that as a clean "no GPU" report.
pub fn probe_adapters(pref: RenderBackendPreference) -> Vec<AdapterReport> {
    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
        backends: wgpu_backends_for(pref),
        ..wgpu::InstanceDescriptor::new_without_display_handle()
    });
    let adapters = pollster::block_on(instance.enumerate_adapters(wgpu_backends_for(pref)));
    adapters
        .iter()
        .map(|adapter| adapter_report(adapter, pref))
        .collect()
}

/// Build the full capability/pipeline report for one adapter.
pub fn adapter_report(adapter: &wgpu::Adapter, pref: RenderBackendPreference) -> AdapterReport {
    let info = adapter.get_info();
    let features = adapter.features();
    let limits = adapter.limits();
    let caps = gpu_capabilities_for(features, &limits, info.vendor);
    AdapterReport {
        shader_caps: gpu_shader_caps_for(info.backend, features, &limits),
        resolution: negotiate_pipeline(pref, &caps),
        caps,
        limits,
        name: info.name.clone(),
        backend: info.backend,
        device_type: info.device_type,
        vendor_id: info.vendor,
        device_id: info.device,
        driver: info.driver.clone(),
        driver_info: info.driver_info.clone(),
    }
}

/// The multi-line `--headless-probe` rendering of one report.
pub fn format_adapter_report(report: &AdapterReport, index: usize) -> String {
    let mut out = format!(
        "adapter {index}: {:?} {:?} \"{}\" (vendor {:#06x} device {:#06x})\n\
         \x20   driver: {} {}\n\
         \x20   mesh shader: {} (max out vtx {}, prim {})\n\
         \x20   compute: {} | indirect-count: {}\n\
         \x20   limits: max_tex {}px, bind_groups {}, storage_buf {}B, \
         color_attachments {}, compute_wg/dim {}\n\
         \x20   -> pipeline: {}",
        report.backend,
        report.device_type,
        report.name,
        report.vendor_id,
        report.device_id,
        report.driver,
        report.driver_info,
        report.caps.supports_ext_mesh_shader,
        report.caps.max_mesh_output_vertices,
        report.caps.max_mesh_output_primitives,
        report.caps.supports_compute_shaders,
        report.caps.supports_multi_draw_indirect_count,
        report.limits.max_texture_dimension_2d,
        report.limits.max_bind_groups,
        report.limits.max_storage_buffer_binding_size,
        report.limits.max_color_attachments,
        report.limits.max_compute_workgroups_per_dimension,
        report.resolution.backend.as_str(),
    );
    if let Some(reason) = &report.resolution.fallback_reason {
        out.push_str(&format!("\n\x20   fallback: {reason}"));
    }
    out
}

// ---------------------------------------------------------------------------
// Device context (adapter + device + negotiated pipeline)
// ---------------------------------------------------------------------------

/// A live `wgpu` adapter+device pair plus the negotiated client-port
/// pipeline verdicts. Shared by the offscreen and surface render paths.
pub struct GpuDevice {
    pub instance: wgpu::Instance,
    pub adapter: wgpu::Adapter,
    pub device: wgpu::Device,
    pub queue: wgpu::Queue,
    pub report: AdapterReport,
}

/// Request a real adapter + logical device for `pref`. `display` is the
/// winit `OwnedDisplayHandle` in windowed mode (`None` headless), and
/// `compatible_surface` lets the adapter pick a device that can present to
/// an already-created surface.
pub fn request_gpu_device(
    pref: RenderBackendPreference,
    display: Option<OwnedDisplayHandle>,
    compatible_surface: Option<&wgpu::Surface<'_>>,
) -> Result<GpuDevice, RendererError> {
    let instance_desc = match display {
        Some(display) => wgpu::InstanceDescriptor::new_with_display_handle(Box::new(display)),
        None => wgpu::InstanceDescriptor::new_without_display_handle(),
    };
    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
        backends: wgpu_backends_for(pref),
        ..instance_desc
    });
    let options = wgpu::RequestAdapterOptions {
        power_preference: match pref {
            RenderBackendPreference::ClassicLowSpec => wgpu::PowerPreference::LowPower,
            _ => wgpu::PowerPreference::HighPerformance,
        },
        force_fallback_adapter: false,
        compatible_surface,
        apply_limit_buckets: true,
    };
    let adapter = pollster::block_on(instance.request_adapter(&options)).map_err(|e| {
        RendererError::NoAdapter(format!(
            "request_adapter for backends {:?}: {e}",
            wgpu_backends_for(pref)
        ))
    })?;
    let report = adapter_report(&adapter, pref);
    let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
        label: Some("mc-spine-client-gpu"),
        required_features: wgpu::Features::empty(),
        required_limits: wgpu::Limits::default(),
        ..wgpu::DeviceDescriptor::default()
    }))
    .map_err(|e| RendererError::Device(e.to_string()))?;
    Ok(GpuDevice {
        instance,
        adapter,
        device,
        queue,
        report,
    })
}

// ---------------------------------------------------------------------------
// Uniform + vertex layouts (mirror shaders.rs / packed.rs field-for-field)
// ---------------------------------------------------------------------------

/// `frame` uniform for both terrain WGSL modules. Mirrors `FrameUniforms`
/// in shaders.rs: WGSL rounds the struct to 16 B alignment, so this is
/// 112 B, not the 104 B `repr(C)` field sum.
#[repr(C)]
#[derive(Clone, Copy, Debug, Pod, Zeroable)]
pub struct FrameUniforms {
    pub view_proj: [[f32; 4]; 4],
    pub camera_pos: [f32; 3],
    pub fog_start: f32,
    pub fog_color: [f32; 3],
    pub fog_end: f32,
    pub _pad: [f32; 4],
}

/// `section` uniform for both terrain WGSL modules (32 B).
#[repr(C)]
#[derive(Clone, Copy, Debug, Pod, Zeroable)]
pub struct SectionUniforms {
    pub origin: [f32; 3],
    pub _pad0: f32,
    pub biome_tint: [f32; 3],
    pub biome_tint_strength: f32,
}

const TERRAIN_ATTRS: [wgpu::VertexAttribute; 4] = [
    wgpu::VertexAttribute {
        format: wgpu::VertexFormat::Uint32,
        offset: 0,
        shader_location: 0,
    },
    wgpu::VertexAttribute {
        format: wgpu::VertexFormat::Uint32,
        offset: 4,
        shader_location: 1,
    },
    wgpu::VertexAttribute {
        format: wgpu::VertexFormat::Uint32,
        offset: 8,
        shader_location: 2,
    },
    wgpu::VertexAttribute {
        format: wgpu::VertexFormat::Uint32,
        offset: 12,
        shader_location: 3,
    },
];

/// 16-byte `PackedVertex` stride -> `@location(0..3)` `u32` inputs.
pub const TERRAIN_VERTEX_LAYOUT: wgpu::VertexBufferLayout<'static> = wgpu::VertexBufferLayout {
    array_stride: std::mem::size_of::<PackedVertex>() as wgpu::BufferAddress,
    step_mode: wgpu::VertexStepMode::Vertex,
    attributes: &TERRAIN_ATTRS,
};

const DEPTH_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Depth32Float;
/// Offscreen/UI-framebuffer pixel format.
pub const OFFSCREEN_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8UnormSrgb;

fn depth_state(write: bool) -> wgpu::DepthStencilState {
    wgpu::DepthStencilState {
        format: DEPTH_FORMAT,
        depth_write_enabled: Some(write),
        depth_compare: Some(wgpu::CompareFunction::LessEqual),
        stencil: wgpu::StencilState::default(),
        bias: wgpu::DepthBiasState::default(),
    }
}

const ALPHA_BLEND: wgpu::BlendState = wgpu::BlendState {
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

// ---------------------------------------------------------------------------
// Shaderpack device hook
// ---------------------------------------------------------------------------

/// Outcome of offering a Megu native WGSL pack to the real `wgpu::Device`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ShaderpackAttempt {
    /// At least one pack pass became a usable terrain pipeline.
    Active {
        /// Pass that replaced the built-in terrain pipeline.
        terrain_pass: String,
        /// Total WGSL modules the device accepted.
        modules_compiled: usize,
    },
    /// Built-in pipeline kept; `notice` explains why (compatibility,
    /// structural validation, or a device rejection).
    Fallback { notice: String },
}

// ---------------------------------------------------------------------------
// The renderer
// ---------------------------------------------------------------------------

/// Per-uploaded-scene GPU buffers.
struct TerrainBuffers {
    opaque_vertices: wgpu::Buffer,
    opaque_indices: wgpu::Buffer,
    opaque_index_count: u32,
    translucent_vertices: wgpu::Buffer,
    translucent_indices: wgpu::Buffer,
    translucent_index_count: u32,
}

/// Local mesh vertices and draw indices, keeping translucent quads ordered.
#[derive(Clone, Debug, Default)]
pub struct LiveSectionGeometry {
    pub opaque: Vec<PackedVertex>,
    pub opaque_indices: Vec<u32>,
    pub translucent: Vec<PackedVertex>,
    pub translucent_indices: Vec<u32>,
}

fn append_live_quad(
    vertices: &mut Vec<PackedVertex>,
    indices: &mut Vec<u32>,
    quad: &[PackedVertex],
) {
    let base = vertices.len() as u32;
    vertices.extend_from_slice(quad);
    // PackedVertex.attributes bit 17 is the original greedy-mesh diagonal.
    let corners = if quad[0].attributes & (1 << 17) == 0 {
        [0, 1, 2, 0, 2, 3]
    } else {
        [0, 1, 3, 1, 2, 3]
    };
    indices.extend(corners.map(|corner| base + corner));
}

pub fn section_geometry(packed: &[PackedVertex], translucent_order: &[u32]) -> LiveSectionGeometry {
    let mut out = LiveSectionGeometry::default();
    let mut translucent = Vec::new();
    for quad in packed.as_chunks::<4>().0 {
        let attrs = quad[0].attributes;
        let layer = (attrs >> 13) & 3;
        let fluid = (attrs >> 15) & 3;
        if layer == 2 || (layer == 3 && fluid == 1) {
            translucent.push(quad.as_slice());
        } else {
            append_live_quad(&mut out.opaque, &mut out.opaque_indices, quad);
        }
    }
    // The world sort refers to the original packed quad indices.
    let mut ordered = Vec::new();
    let mut seen = HashSet::new();
    for &idx in translucent_order {
        let index = idx as usize;
        let quad = packed.get(index.saturating_mul(4)..index.saturating_mul(4).saturating_add(4));
        if let Some(quad) = quad {
            let layer = (quad[0].attributes >> 13) & 3;
            let fluid = (quad[0].attributes >> 15) & 3;
            if seen.insert(index) && (layer == 2 || (layer == 3 && fluid == 1)) {
                ordered.push(quad);
                continue;
            }
        }
        ordered.clear();
        break;
    }
    if ordered.len() != translucent.len() {
        ordered = translucent;
    }
    for quad in ordered {
        append_live_quad(&mut out.translucent, &mut out.translucent_indices, quad);
    }
    out
}

struct LiveTerrainSection {
    packed: Vec<PackedVertex>,
    order: Vec<u32>,
    terrain: TerrainBuffers,
    _uniform: wgpu::Buffer,
    bind_group: wgpu::BindGroup,
}

/// The real `wgpu` renderer: terrain (opaque + translucent), UI blit, depth.
///
/// Owns device-independent state for one color format (`target_format`):
/// the swapchain path passes the surface format, the offscreen path passes
/// [`OFFSCREEN_FORMAT`].
pub struct GpuRenderer {
    ctx: GpuDevice,
    target_format: wgpu::TextureFormat,
    frame_layout: wgpu::BindGroupLayout,
    frame_bind_group: wgpu::BindGroup,
    frame_ubo: wgpu::Buffer,
    section_ubo: wgpu::Buffer,
    terrain_pipeline: wgpu::RenderPipeline,
    translucent_pipeline: wgpu::RenderPipeline,
    ui_pipeline: wgpu::RenderPipeline,
    ui_layout: wgpu::BindGroupLayout,
    ui_bind_group: Option<wgpu::BindGroup>,
    ui_texture: Option<wgpu::Texture>,
    ui_view: Option<wgpu::TextureView>,
    ui_sampler: wgpu::Sampler,
    ui_size: (u32, u32),
    depth_texture: Option<wgpu::Texture>,
    depth_view: Option<wgpu::TextureView>,
    depth_size: (u32, u32),
    scene: Option<TerrainBuffers>,
    live_sections: HashMap<SectionCoord, LiveTerrainSection>,
    composite: CompositeGpu,
    subworld_gpu: SubWorldGpuCache,
    scaled_target: Option<ScaledTarget>,
    /// The negotiated built-in shadow quality (`negotiate_builtin_shadow_quality`
    /// output); `Off` unless [`Self::set_shadow_quality`] ran.
    pub shadow_quality: BuiltinShadowQuality,
    /// Render-scale preset for the composed world pass (`Native100` = the
    /// world and the UI overlay both render at target resolution).
    pub render_scale: RenderScalePreset,
    shadow_cache: DirtyOnlyShadowCache,
    /// Decision the [`DirtyOnlyShadowCache`] returned for the most recent
    /// composed frame (`FirstFrame` until one renders).
    pub last_shadow_decision: ShadowCacheDecision,
    /// The active shaderpack hook verdict (or `None` for built-in).
    pub shaderpack: ShaderpackAttempt,
    /// Latest notice line the HUD/diagnostics can display.
    pub notice: Option<String>,
}

/// GPU state for the composed pass: the fade/light-aware terrain pipeline,
/// the lit `TerrainVertex` pipelines (opaque + alpha), the instanced-part
/// pipeline, the dynamic-offset draw-uniform pool, and the shared
/// environment/lights uniforms. All pipelines share group layouts so one
/// pass binds every draw.
struct CompositeGpu {
    terrain_opaque_pipeline: wgpu::RenderPipeline,
    terrain_alpha_pipeline: wgpu::RenderPipeline,
    lit_pipeline: wgpu::RenderPipeline,
    lit_alpha_pipeline: wgpu::RenderPipeline,
    instanced_pipeline: wgpu::RenderPipeline,
    lit_frame_bg: wgpu::BindGroup,
    draw_layout: wgpu::BindGroupLayout,
    draw_bg: wgpu::BindGroup,
    draw_ubo: wgpu::Buffer,
    draw_capacity: u64,
    draw_scratch: Vec<CompositeDrawUniforms>,
    env_ubo: wgpu::Buffer,
    lights_ubo: wgpu::Buffer,
    env_bg: wgpu::BindGroup,
}

/// The reduced-resolution world target + its UI-blit-style bind group used
/// to upscale back to native resolution when `render_scale < 100%`.
struct ScaledTarget {
    _texture: wgpu::Texture,
    view: wgpu::TextureView,
    bind_group: wgpu::BindGroup,
    size: (u32, u32),
}

/// One composed world-pass invocation: the color view to draw into, its
/// pixel size and matching `view_proj`, the atmosphere clear color, and the
/// chunk-fade ages consulted per live section.
struct ComposedWorldTarget<'a> {
    view: &'a wgpu::TextureView,
    size: (u32, u32),
    view_proj: Mat4,
    clear_rgb: [f32; 3],
    chunk_fades: &'a ChunkFadeTracker,
}

/// Everything [`GpuRenderer::render_offscreen_composed_frame`] needs beyond
/// the camera: the authoritative sub-world bank (synced + drawn),
/// atmosphere state for clear/fog/env uniforms, VFX1 dynamic lights, the
/// chunk-fade tracker, the native-resolution UI framebuffer, and the
/// counters the shadow cache consults.
pub struct ComposedFrame<'a> {
    /// Authoritative sub-world scene (GPU mirror refreshes during render).
    pub subworlds: &'a mut SubWorldRenderBank,
    /// VFX1 dynamic colored lights for this frame.
    pub lights: &'a [DynamicColoredLight],
    /// Blended atmosphere for the frame (clear color, fog, env uniforms).
    pub atmosphere: &'a BiomeAtmosphereBlender,
    /// Per-chunk-column fade ages.
    pub chunk_fades: &'a ChunkFadeTracker,
    /// `client-ui` RGBA framebuffer at native `width`×`height`, or `None`.
    pub ui_rgba: Option<&'a [u8]>,
    /// World edit counter for the dirty-only shadow cache.
    pub world_revision: u64,
    /// World time in ticks (sun direction, shadow cache input).
    pub world_time_ticks: u64,
}

impl GpuRenderer {
    /// Device accessor for embedders.
    pub fn gpu(&self) -> &GpuDevice {
        &self.ctx
    }

    /// Build all pipelines against `target_format` on `ctx`.
    pub fn new(ctx: GpuDevice, target_format: wgpu::TextureFormat) -> Self {
        let frame_ubo = ctx.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("frame-uniforms"),
            size: std::mem::size_of::<FrameUniforms>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let section_ubo = ctx.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("section-uniforms"),
            size: std::mem::size_of::<SectionUniforms>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let frame_layout = ctx
            .device
            .create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("terrain-frame-layout"),
                entries: &[uniform_entry(0), uniform_entry(1)],
            });
        let frame_bind_group = ctx.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("terrain-frame-bind-group"),
            layout: &frame_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: frame_ubo.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: section_ubo.as_entire_binding(),
                },
            ],
        });
        let pipeline_layout = ctx
            .device
            .create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("terrain-pipeline-layout"),
                bind_group_layouts: &[Some(&frame_layout)],
                immediate_size: 0,
            });

        let terrain_pipeline =
            Self::build_terrain_pipeline(&ctx.device, &pipeline_layout, target_format, false);
        let translucent_pipeline =
            Self::build_terrain_pipeline(&ctx.device, &pipeline_layout, target_format, true);

        let ui_layout = ctx
            .device
            .create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("ui-layout"),
                entries: &[
                    wgpu::BindGroupLayoutEntry {
                        binding: 0,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Texture {
                            sample_type: wgpu::TextureSampleType::Float { filterable: true },
                            view_dimension: wgpu::TextureViewDimension::D2,
                            multisampled: false,
                        },
                        count: None,
                    },
                    wgpu::BindGroupLayoutEntry {
                        binding: 1,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                        count: None,
                    },
                ],
            });
        let ui_pipeline_layout =
            ctx.device
                .create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                    label: Some("ui-pipeline-layout"),
                    bind_group_layouts: &[Some(&ui_layout)],
                    immediate_size: 0,
                });
        let ui_module = ctx
            .device
            .create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some("ui-blit"),
                source: wgpu::ShaderSource::Wgsl(Cow::Borrowed(crate::UI_BLIT_WGSL)),
            });
        let ui_pipeline = ctx
            .device
            .create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("ui-blit-pipeline"),
                layout: Some(&ui_pipeline_layout),
                vertex: wgpu::VertexState {
                    module: &ui_module,
                    entry_point: Some("ui_blit_vs"),
                    compilation_options: wgpu::PipelineCompilationOptions::default(),
                    buffers: &[],
                },
                primitive: wgpu::PrimitiveState {
                    topology: wgpu::PrimitiveTopology::TriangleList,
                    cull_mode: None,
                    ..wgpu::PrimitiveState::default()
                },
                depth_stencil: None,
                multisample: wgpu::MultisampleState::default(),
                fragment: Some(wgpu::FragmentState {
                    module: &ui_module,
                    entry_point: Some("ui_blit_fs"),
                    compilation_options: wgpu::PipelineCompilationOptions::default(),
                    targets: &[Some(wgpu::ColorTargetState {
                        format: target_format,
                        blend: Some(ALPHA_BLEND),
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                }),
                multiview_mask: None,
                cache: None,
            });
        let ui_sampler = ctx.device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("ui-sampler"),
            mag_filter: wgpu::FilterMode::Nearest,
            min_filter: wgpu::FilterMode::Nearest,
            ..wgpu::SamplerDescriptor::default()
        });
        let composite = Self::build_composite(&ctx, target_format, &frame_layout, &frame_ubo);
        Self {
            ctx,
            target_format,
            frame_layout,
            frame_bind_group,
            frame_ubo,
            section_ubo,
            terrain_pipeline,
            translucent_pipeline,
            ui_pipeline,
            ui_layout,
            ui_bind_group: None,
            ui_texture: None,
            ui_view: None,
            ui_sampler,
            ui_size: (0, 0),
            depth_texture: None,
            depth_view: None,
            depth_size: (0, 0),
            scene: None,
            live_sections: HashMap::new(),
            composite,
            subworld_gpu: SubWorldGpuCache::new(),
            scaled_target: None,
            shadow_quality: BuiltinShadowQuality::Off,
            render_scale: RenderScalePreset::default(),
            shadow_cache: DirtyOnlyShadowCache::new(),
            last_shadow_decision: ShadowCacheDecision::Rerender(
                mc_spine_client_mesh::builtin_shadow_wgsl::ShadowDirtyReason::FirstFrame,
            ),
            shaderpack: ShaderpackAttempt::Fallback {
                notice: "built-in terrain pipeline".to_string(),
            },
            notice: None,
        }
    }

    /// Build the composed-pass pipelines and shared uniforms. Group layout:
    /// `0` = terrain `frame_layout` (composite terrain) or frame-only
    /// (lit/instanced), `1` = dynamic-offset draw uniforms,
    /// `2` = environment + VFX light uniforms.
    fn build_composite(
        ctx: &GpuDevice,
        target_format: wgpu::TextureFormat,
        frame_layout: &wgpu::BindGroupLayout,
        frame_ubo: &wgpu::Buffer,
    ) -> CompositeGpu {
        let device = &ctx.device;
        let draw_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("composite-draw-layout"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: true,
                    min_binding_size: wgpu::BufferSize::new(
                        std::mem::size_of::<CompositeDrawUniforms>() as u64,
                    ),
                },
                count: None,
            }],
        });
        let env_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("composite-env-layout"),
            entries: &[uniform_entry(0), uniform_entry(1)],
        });
        let lit_frame_layout =
            device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("lit-frame-layout"),
                entries: &[uniform_entry(0)],
            });

        let terrain_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("composite-terrain-layout"),
            bind_group_layouts: &[Some(frame_layout), Some(&draw_layout), Some(&env_layout)],
            immediate_size: 0,
        });
        let lit_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("composite-lit-layout"),
            bind_group_layouts: &[Some(&lit_frame_layout), Some(&draw_layout), Some(&env_layout)],
            immediate_size: 0,
        });

        let terrain_module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("composite-terrain"),
            source: wgpu::ShaderSource::Wgsl(Cow::Borrowed(COMPOSITE_TERRAIN_WGSL)),
        });
        let lit_module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("composite-lit"),
            source: wgpu::ShaderSource::Wgsl(Cow::Borrowed(COMPOSITE_LIT_WGSL)),
        });

        let composite_pipeline = |label, layout, module, vs, fs, buffers, blend, depth_write| {
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some(label),
                layout: Some(layout),
                vertex: wgpu::VertexState {
                    module,
                    entry_point: Some(vs),
                    compilation_options: wgpu::PipelineCompilationOptions::default(),
                    buffers,
                },
                primitive: wgpu::PrimitiveState {
                    topology: wgpu::PrimitiveTopology::TriangleList,
                    cull_mode: None,
                    ..wgpu::PrimitiveState::default()
                },
                depth_stencil: Some(depth_state(depth_write)),
                multisample: wgpu::MultisampleState::default(),
                fragment: Some(wgpu::FragmentState {
                    module,
                    entry_point: Some(fs),
                    compilation_options: wgpu::PipelineCompilationOptions::default(),
                    targets: &[Some(wgpu::ColorTargetState {
                        format: target_format,
                        blend,
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                }),
                multiview_mask: None,
                cache: None,
            })
        };
        let terrain_buffers = [Some(TERRAIN_VERTEX_LAYOUT)];
        let lit_buffers = [Some(LIT_VERTEX_LAYOUT)];
        let instanced_buffers = [Some(LIT_VERTEX_LAYOUT), Some(PART_INSTANCE_LAYOUT)];
        let terrain_opaque_pipeline = composite_pipeline(
            "composite-terrain-opaque",
            &terrain_layout,
            &terrain_module,
            "terrain_vs",
            "terrain_fs",
            &terrain_buffers,
            None,
            true,
        );
        let terrain_alpha_pipeline = composite_pipeline(
            "composite-terrain-alpha",
            &terrain_layout,
            &terrain_module,
            "terrain_translucent_vs",
            "terrain_translucent_fs",
            &terrain_buffers,
            Some(ALPHA_BLEND),
            false,
        );
        let lit_pipeline = composite_pipeline(
            "composite-lit-opaque",
            &lit_layout,
            &lit_module,
            "lit_vs",
            "lit_fs",
            &lit_buffers,
            None,
            true,
        );
        let lit_alpha_pipeline = composite_pipeline(
            "composite-lit-alpha",
            &lit_layout,
            &lit_module,
            "lit_vs",
            "lit_fs",
            &lit_buffers,
            Some(ALPHA_BLEND),
            false,
        );
        let instanced_pipeline = composite_pipeline(
            "composite-instanced",
            &lit_layout,
            &lit_module,
            "inst_vs",
            "lit_fs",
            &instanced_buffers,
            None,
            true,
        );

        let lit_frame_bg = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("lit-frame-bind-group"),
            layout: &lit_frame_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: frame_ubo.as_entire_binding(),
            }],
        });
        let env_ubo = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("composite-env"),
            size: std::mem::size_of::<CompositeEnvUniforms>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let lights_ubo = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("composite-lights"),
            size: std::mem::size_of::<VfxLightsGpu>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let env_bg = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("composite-env-bind-group"),
            layout: &env_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: env_ubo.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: lights_ubo.as_entire_binding(),
                },
            ],
        });
        let draw_capacity = u64::from(DRAW_UNIFORM_STRIDE) * 64;
        let draw_ubo = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("composite-draw-uniforms"),
            size: draw_capacity,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let draw_bg = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("composite-draw-bind-group"),
            layout: &draw_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                    buffer: &draw_ubo,
                    offset: 0,
                    size: wgpu::BufferSize::new(u64::from(DRAW_UNIFORM_STRIDE)),
                }),
            }],
        });
        CompositeGpu {
            terrain_opaque_pipeline,
            terrain_alpha_pipeline,
            lit_pipeline,
            lit_alpha_pipeline,
            instanced_pipeline,
            lit_frame_bg,
            draw_layout,
            draw_bg,
            draw_ubo,
            draw_capacity,
            draw_scratch: Vec::with_capacity(64),
            env_ubo,
            lights_ubo,
            env_bg,
        }
    }

    fn build_terrain_pipeline(
        device: &wgpu::Device,
        layout: &wgpu::PipelineLayout,
        target_format: wgpu::TextureFormat,
        translucent: bool,
    ) -> wgpu::RenderPipeline {
        let (source, label) = if translucent {
            (TERRAIN_TRANSLUCENT_WGSL, "terrain-translucent")
        } else {
            (TERRAIN_OPAQUE_WGSL, "terrain-opaque")
        };
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some(label),
            source: wgpu::ShaderSource::Wgsl(Cow::Borrowed(source)),
        });
        let (vs_entry, fs_entry) = if translucent {
            ("terrain_translucent_vs", "terrain_translucent_fs")
        } else {
            ("terrain_opaque_vs", "terrain_opaque_fs")
        };
        Self::terrain_pipeline_from_module(
            device,
            layout,
            target_format,
            &module,
            (vs_entry, fs_entry),
            translucent,
            label,
        )
    }

    fn terrain_pipeline_from_module(
        device: &wgpu::Device,
        layout: &wgpu::PipelineLayout,
        target_format: wgpu::TextureFormat,
        module: &wgpu::ShaderModule,
        (vs_entry, fs_entry): (&str, &str),
        translucent: bool,
        label: &str,
    ) -> wgpu::RenderPipeline {
        device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some(label),
            layout: Some(layout),
            vertex: wgpu::VertexState {
                module,
                entry_point: Some(vs_entry),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                buffers: &[Some(TERRAIN_VERTEX_LAYOUT)],
            },
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                cull_mode: None,
                ..wgpu::PrimitiveState::default()
            },
            depth_stencil: Some(depth_state(!translucent)),
            multisample: wgpu::MultisampleState::default(),
            fragment: Some(wgpu::FragmentState {
                module,
                entry_point: Some(fs_entry),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: target_format,
                    blend: translucent.then_some(ALPHA_BLEND),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            multiview_mask: None,
            cache: None,
        })
    }

    /// Offer a loaded [`MeguWgslPack`] to the real device: structural
    /// validation, then module compilation + a candidate terrain pipeline
    /// inside a `wgpu::ErrorScope`. Any rejection keeps the built-in
    /// pipeline and records a notice — this never crashes.
    pub fn apply_shaderpack(&mut self, pack: &MeguWgslPack) -> ShaderpackAttempt {
        if !pack.is_compatible() {
            let notice = format!(
                "shaderpack {:?} needs capabilities this GPU lacks: {}",
                pack.name,
                pack.compatibility.missing().join("; ")
            );
            self.shaderpack = ShaderpackAttempt::Fallback {
                notice: notice.clone(),
            };
            self.notice = Some(notice);
            return self.shaderpack.clone();
        }
        // Try every gbuffers-stage graphics pass until one yields a pipeline
        // the device accepts with the packed-vertex layout.
        let pack_layout = self
            .ctx
            .device
            .create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("megu-pack-layout"),
                bind_group_layouts: &[Some(&self.frame_layout)],
                immediate_size: 0,
            });
        let mut compiled = 0usize;
        let mut last_rejection = String::new();
        for pass in &pack.passes {
            if pass.stage != PipelineStage::Gbuffers || pass.kind != PassKind::Graphics {
                continue;
            }
            let info = match validate_wgsl_module(
                &pass.wgsl_source,
                &[WgslStage::Vertex, WgslStage::Fragment],
            ) {
                Ok(info) => info,
                Err(err) => {
                    last_rejection = format!("{}: structural validation failed: {err}", pass.name);
                    continue;
                }
            };
            let scope = self
                .ctx
                .device
                .push_error_scope(wgpu::ErrorFilter::Validation);
            let module = self
                .ctx
                .device
                .create_shader_module(wgpu::ShaderModuleDescriptor {
                    label: Some("megu-wgsl-pack"),
                    source: wgpu::ShaderSource::Wgsl(Cow::Borrowed(pass.wgsl_source.as_str())),
                });
            // The pack's own entry-point names drive the pipeline.
            let name_of = |stage| {
                info.entry_points
                    .iter()
                    .find(|(s, _)| *s == stage)
                    .map(|(_, n)| n.clone())
            };
            let candidate = name_of(WgslStage::Vertex)
                .zip(name_of(WgslStage::Fragment))
                .map(|(vs, fs)| {
                    Self::terrain_pipeline_from_module(
                        &self.ctx.device,
                        &pack_layout,
                        self.target_format,
                        &module,
                        (&vs, &fs),
                        false,
                        "megu-pack-terrain",
                    )
                });
            let error = pollster::block_on(scope.pop());
            match (error, candidate) {
                (None, Some(pipeline)) => {
                    compiled += 1;
                    self.terrain_pipeline = pipeline;
                    let attempt = ShaderpackAttempt::Active {
                        terrain_pass: pass.name.clone(),
                        modules_compiled: compiled,
                    };
                    self.shaderpack = attempt.clone();
                    self.notice = Some(format!(
                        "shaderpack {:?} active: pass {:?} drives the terrain pipeline",
                        pack.name, pass.name
                    ));
                    return attempt;
                }
                (Some(err), _) => {
                    last_rejection = format!("{}: device rejected module: {err}", pass.name);
                }
                (None, None) => {
                    last_rejection = format!(
                        "{}: has vertex+fragment stages but no usable entry points",
                        pass.name
                    );
                }
            }
        }
        let notice = if last_rejection.is_empty() {
            format!(
                "shaderpack {:?} has no gbuffers graphics pass; built-in terrain kept",
                pack.name
            )
        } else {
            format!(
                "shaderpack {:?} fell back to built-in terrain ({last_rejection})",
                pack.name
            )
        };
        self.shaderpack = ShaderpackAttempt::Fallback {
            notice: notice.clone(),
        };
        self.notice = Some(notice);
        self.shaderpack.clone()
    }

    /// Load + offer `--shader-pack <path>`: opens the source, detects the
    /// Megu WGSL manifest, validates against the real shader caps, then runs
    /// the device hook. Non-Megu packs and unreadable paths degrade to a
    /// notice, never a crash.
    pub fn load_shaderpack(&mut self, path: &Path) -> ShaderpackAttempt {
        let attempt = (|| -> Result<ShaderpackAttempt, String> {
            let source = ShaderPackSource::open(path)
                .map_err(|e| format!("cannot open shaderpack {}: {e}", path.display()))?;
            if !is_megu_wgsl_pack(&source) {
                return Err(format!(
                    "{} is not a Megu native WGSL pack (no megu-shader.toml); built-in \
                     terrain kept",
                    path.display()
                ));
            }
            let pack =
                mc_spine_client_mesh::load_megu_wgsl_pack(&source, &self.ctx.report.shader_caps)
                    .map_err(|e| format!("cannot load {}: {e}", path.display()))?;
            Ok(self.apply_shaderpack(&pack))
        })();
        match attempt {
            Ok(attempt) => attempt,
            Err(notice) => {
                self.shaderpack = ShaderpackAttempt::Fallback {
                    notice: notice.clone(),
                };
                self.notice = Some(notice);
                self.shaderpack.clone()
            }
        }
    }

    /// Upload a meshed scene: quads -> real `mc_spine_client_mesh` packed
    /// vertices -> GPU vertex/index buffers.
    pub fn upload_scene(&mut self, scene: &DemoScene) -> Result<(), RendererError> {
        let opaque_verts = pack_quads_to_vertices(&scene.opaque_quads)
            .map_err(|e| RendererError::PackFailed(e.to_string()))?;
        let translucent_verts = pack_quads_to_vertices(&scene.translucent_quads)
            .map_err(|e| RendererError::PackFailed(e.to_string()))?;
        let opaque_idx = build_index_buffer(&scene.opaque_quads);
        let translucent_idx = build_index_buffer(&scene.translucent_quads);

        let buffers = TerrainBuffers {
            opaque_index_count: opaque_idx.len() as u32,
            translucent_index_count: translucent_idx.len() as u32,
            opaque_vertices: vertex_buffer(
                &self.ctx.device,
                &self.ctx.queue,
                "opaque-verts",
                &opaque_verts,
            ),
            opaque_indices: index_buffer(
                &self.ctx.device,
                &self.ctx.queue,
                "opaque-idx",
                &opaque_idx,
            ),
            translucent_vertices: vertex_buffer(
                &self.ctx.device,
                &self.ctx.queue,
                "translucent-verts",
                &translucent_verts,
            ),
            translucent_indices: index_buffer(
                &self.ctx.device,
                &self.ctx.queue,
                "translucent-idx",
                &translucent_idx,
            ),
        };
        self.scene = Some(buffers);
        self.set_section_origin(scene.section_origin);
        Ok(())
    }

    /// Synchronize real chunk sections. Unchanged sections reuse their existing buffers.
    /// Unloaded sections are released as soon as the world stops exposing them.
    pub fn update_live_terrain(&mut self, world: &WorldState) -> usize {
        let mut uploaded = 0;
        self.live_sections
            .retain(|coord, _| world.packed_mesh(*coord).is_some());
        for (coord, packed) in world.packed_meshes() {
            let order = world.translucent_order(coord).unwrap_or(&[]);
            if self
                .live_sections
                .get(&coord)
                .is_some_and(|existing| existing.packed == packed && existing.order == order)
            {
                continue;
            }
            let geometry = section_geometry(packed, order);
            let terrain = TerrainBuffers {
                opaque_index_count: geometry.opaque_indices.len() as u32,
                translucent_index_count: geometry.translucent_indices.len() as u32,
                opaque_vertices: vertex_buffer(
                    &self.ctx.device,
                    &self.ctx.queue,
                    "live-opaque-verts",
                    &geometry.opaque,
                ),
                opaque_indices: index_buffer(
                    &self.ctx.device,
                    &self.ctx.queue,
                    "live-opaque-indices",
                    &geometry.opaque_indices,
                ),
                translucent_vertices: vertex_buffer(
                    &self.ctx.device,
                    &self.ctx.queue,
                    "live-translucent-verts",
                    &geometry.translucent,
                ),
                translucent_indices: index_buffer(
                    &self.ctx.device,
                    &self.ctx.queue,
                    "live-translucent-indices",
                    &geometry.translucent_indices,
                ),
            };
            let uniform = self.ctx.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("live-section-uniform"),
                size: std::mem::size_of::<SectionUniforms>() as u64,
                usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            let section_uniform = SectionUniforms {
                origin: [
                    coord.0 as f32 * 16.0,
                    coord.1 as f32 * 16.0,
                    coord.2 as f32 * 16.0,
                ],
                _pad0: 0.0,
                biome_tint: [1.0; 3],
                biome_tint_strength: 0.0,
            };
            self.ctx
                .queue
                .write_buffer(&uniform, 0, bytemuck::bytes_of(&section_uniform));
            let bind_group = self
                .ctx
                .device
                .create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("live-section-bind-group"),
                    layout: &self.frame_layout,
                    entries: &[
                        wgpu::BindGroupEntry {
                            binding: 0,
                            resource: self.frame_ubo.as_entire_binding(),
                        },
                        wgpu::BindGroupEntry {
                            binding: 1,
                            resource: uniform.as_entire_binding(),
                        },
                    ],
                });
            self.live_sections.insert(
                coord,
                LiveTerrainSection {
                    packed: packed.to_vec(),
                    order: order.to_vec(),
                    terrain,
                    _uniform: uniform,
                    bind_group,
                },
            );
            uploaded += 1;
        }
        uploaded
    }

    pub fn live_section_count(&self) -> usize {
        self.live_sections.len()
    }

    /// Update the `section` uniform (world origin + biome tint).
    pub fn set_section_origin(&self, origin: [f32; 3]) {
        let uniforms = SectionUniforms {
            origin,
            _pad0: 0.0,
            biome_tint: [1.0, 1.0, 1.0],
            biome_tint_strength: 0.0,
        };
        self.ctx
            .queue
            .write_buffer(&self.section_ubo, 0, bytemuck::bytes_of(&uniforms));
    }

    /// Write the `frame` uniform (camera view-proj + fog from the real
    /// `mc_spine_kernel::evaluate_sky_and_fog` at noon, clear weather).
    pub fn set_camera(&self, camera: &Camera, aspect: f32) -> SkyFogOutput {
        let sky = evaluate_sky_and_fog(&SkyFogInput {
            day_time: 6000,
            rain_level: 0.0,
            thunder_level: 0.0,
            view_distance: camera.far.min(256.0),
            biome_fog_color: [0.75, 0.85, 1.0],
            biome_fog_color_next: [0.75, 0.85, 1.0],
            biome_blend: 0.0,
            underwater: false,
            in_lava: false,
            powder_snow: false,
            blindness: 0.0,
            darkness: 0.0,
        });
        let uniforms = FrameUniforms {
            view_proj: camera.view_projection(aspect),
            camera_pos: camera.position,
            fog_start: sky.fog_start,
            fog_color: sky.fog_rgb,
            fog_end: sky.fog_end,
            _pad: [0.0; 4],
        };
        self.ctx
            .queue
            .write_buffer(&self.frame_ubo, 0, bytemuck::bytes_of(&uniforms));
        sky
    }

    /// (Re)create the depth attachment for `width`×`height` when needed.
    fn ensure_depth(&mut self, width: u32, height: u32) {
        if self.depth_size == (width, height) && self.depth_view.is_some() {
            return;
        }
        let texture = self.ctx.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("depth"),
            size: wgpu::Extent3d {
                width: width.max(1),
                height: height.max(1),
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: DEPTH_FORMAT,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            view_formats: &[],
        });
        self.depth_view = Some(texture.create_view(&wgpu::TextureViewDescriptor::default()));
        self.depth_texture = Some(texture);
        self.depth_size = (width, height);
    }

    /// Upload/replace the `client-ui` RGBA framebuffer texture. `rgba` must
    /// be `width * height * 4` bytes.
    pub fn update_ui_framebuffer(
        &mut self,
        width: u32,
        height: u32,
        rgba: &[u8],
    ) -> Result<(), RendererError> {
        if width == 0 || height == 0 {
            self.ui_texture = None;
            self.ui_view = None;
            self.ui_bind_group = None;
            self.ui_size = (0, 0);
            return Ok(());
        }
        if rgba.len() != crate::ui_rgba_len(width, height) {
            return Err(RendererError::Surface(format!(
                "UI framebuffer is {} bytes, expected {}",
                rgba.len(),
                crate::ui_rgba_len(width, height)
            )));
        }
        if self.ui_size != (width, height) || self.ui_texture.is_none() {
            let texture = self.ctx.device.create_texture(&wgpu::TextureDescriptor {
                label: Some("ui-framebuffer"),
                size: wgpu::Extent3d {
                    width,
                    height,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: OFFSCREEN_FORMAT,
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            });
            let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
            self.ui_bind_group = Some(self.ctx.device.create_bind_group(
                &wgpu::BindGroupDescriptor {
                    label: Some("ui-bind-group"),
                    layout: &self.ui_layout,
                    entries: &[
                        wgpu::BindGroupEntry {
                            binding: 0,
                            resource: wgpu::BindingResource::TextureView(&view),
                        },
                        wgpu::BindGroupEntry {
                            binding: 1,
                            resource: wgpu::BindingResource::Sampler(&self.ui_sampler),
                        },
                    ],
                },
            ));
            self.ui_texture = Some(texture);
            self.ui_view = Some(view);
            self.ui_size = (width, height);
        }
        self.ctx.queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: self.ui_texture.as_ref().expect("created above"),
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
        Ok(())
    }

    /// Encode one frame into `view`: sky clear + opaque + translucent +
    /// (optional) UI blit. `sky` supplies the clear color.
    pub fn render_to_view(
        &mut self,
        view: &wgpu::TextureView,
        width: u32,
        height: u32,
        sky: &SkyFogOutput,
    ) {
        self.ensure_depth(width, height);
        let mut encoder = self
            .ctx
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("frame"),
            });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("terrain"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: f64::from(sky.fog_rgb[0]),
                            g: f64::from(sky.fog_rgb[1]),
                            b: f64::from(sky.fog_rgb[2]),
                            a: 1.0,
                        }),
                        store: wgpu::StoreOp::Store,
                    },
                    depth_slice: None,
                })],
                depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                    view: self.depth_view.as_ref().expect("depth ensured"),
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
            pass.set_bind_group(0, &self.frame_bind_group, &[]);
            if let Some(scene) = &self.scene {
                if scene.opaque_index_count > 0 {
                    pass.set_pipeline(&self.terrain_pipeline);
                    pass.set_vertex_buffer(0, scene.opaque_vertices.slice(..));
                    pass.set_index_buffer(
                        scene.opaque_indices.slice(..),
                        wgpu::IndexFormat::Uint32,
                    );
                    pass.draw_indexed(0..scene.opaque_index_count, 0, 0..1);
                }
                if scene.translucent_index_count > 0 {
                    pass.set_pipeline(&self.translucent_pipeline);
                    pass.set_vertex_buffer(0, scene.translucent_vertices.slice(..));
                    pass.set_index_buffer(
                        scene.translucent_indices.slice(..),
                        wgpu::IndexFormat::Uint32,
                    );
                    pass.draw_indexed(0..scene.translucent_index_count, 0, 0..1);
                }
            }
            if !self.live_sections.is_empty() {
                pass.set_pipeline(&self.terrain_pipeline);
                for section in self.live_sections.values() {
                    if section.terrain.opaque_index_count == 0 {
                        continue;
                    }
                    pass.set_bind_group(0, &section.bind_group, &[]);
                    pass.set_vertex_buffer(0, section.terrain.opaque_vertices.slice(..));
                    pass.set_index_buffer(
                        section.terrain.opaque_indices.slice(..),
                        wgpu::IndexFormat::Uint32,
                    );
                    pass.draw_indexed(0..section.terrain.opaque_index_count, 0, 0..1);
                }
                pass.set_pipeline(&self.translucent_pipeline);
                for section in self.live_sections.values() {
                    if section.terrain.translucent_index_count == 0 {
                        continue;
                    }
                    pass.set_bind_group(0, &section.bind_group, &[]);
                    pass.set_vertex_buffer(0, section.terrain.translucent_vertices.slice(..));
                    pass.set_index_buffer(
                        section.terrain.translucent_indices.slice(..),
                        wgpu::IndexFormat::Uint32,
                    );
                    pass.draw_indexed(0..section.terrain.translucent_index_count, 0, 0..1);
                }
            }
        }
        // Second pass: the UI blit (no depth attachment; pipeline is depthless).
        if let Some(ui_bind_group) = &self.ui_bind_group {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("ui-overlay"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Load,
                        store: wgpu::StoreOp::Store,
                    },
                    depth_slice: None,
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            pass.set_pipeline(&self.ui_pipeline);
            pass.set_bind_group(0, ui_bind_group, &[]);
            pass.draw(0..3, 0..1);
        }
        self.ctx.queue.submit([encoder.finish()]);
    }

    /// Render a frame to an offscreen RGBA target and read the pixels back.
    /// Works headless wherever `wgpu` finds any adapter (llvmpipe/lavapipe
    /// included). `ui_rgba` is the `client-ui` framebuffer, or `None`.
    pub fn render_offscreen_rgba(
        &mut self,
        width: u32,
        height: u32,
        camera: &Camera,
        ui_rgba: Option<&[u8]>,
    ) -> Result<Vec<u8>, RendererError> {
        if let Some(rgba) = ui_rgba {
            self.update_ui_framebuffer(width, height, rgba)?;
        }
        let sky = self.set_camera(camera, width.max(1) as f32 / height.max(1) as f32);
        let texture = self.ctx.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("offscreen-target"),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: OFFSCREEN_FORMAT,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        self.render_to_view(&view, width, height, &sky);

        // Readback: tight rows would break the 256-byte row-alignment rule,
        // so copy into a padded buffer and un-pad on the CPU.
        let padded_bpr = crate::aligned_copy_bytes_per_row(width);
        let buf_size = u64::from(padded_bpr) * u64::from(height);
        let buffer = self.ctx.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("offscreen-readback"),
            size: buf_size,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut encoder = self
            .ctx
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("readback"),
            });
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture: &texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &buffer,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(padded_bpr),
                    rows_per_image: Some(height),
                },
            },
            wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
        );
        self.ctx.queue.submit([encoder.finish()]);
        self.map_readback_buffer(&buffer, width, padded_bpr, height)
    }

    /// Map a `COPY_DST` readback buffer and un-pad rows into a tight RGBA vec.
    fn map_readback_buffer(
        &self,
        buffer: &wgpu::Buffer,
        width: u32,
        padded_bpr: u32,
        height: u32,
    ) -> Result<Vec<u8>, RendererError> {
        let (tx, rx) = std::sync::mpsc::channel();
        buffer.map_async(wgpu::MapMode::Read, .., move |result| {
            let _ = tx.send(result);
        });
        self.ctx
            .device
            .poll(wgpu::PollType::wait_indefinitely())
            .map_err(|e| RendererError::Readback(e.to_string()))?;
        rx.recv()
            .map_err(|_| RendererError::Readback("map_async channel closed".to_string()))?
            .map_err(|e| RendererError::Readback(format!("map_async: {e}")))?;
        let mapped = buffer
            .get_mapped_range(..)
            .map_err(|e| RendererError::Readback(format!("get_mapped_range: {e}")))?;
        let row_bytes = (width * 4) as usize;
        let mut rgba = Vec::with_capacity(row_bytes * height as usize);
        for y in 0..height as usize {
            let start = y * padded_bpr as usize;
            rgba.extend_from_slice(&mapped[start..start + row_bytes]);
        }
        drop(mapped);
        buffer.unmap();
        Ok(rgba)
    }

    // ---------------------------------------------------------------------
    // Composed frame: terrain + 6-DOF sub-worlds + instanced parts +
    // clouds/planets + VFX lights + atmosphere + UI at native scale
    // ---------------------------------------------------------------------

    /// Negotiate and activate a built-in shadow quality preset. Returns the
    /// quality the GPU actually supports; a change invalidates the
    /// dirty-only shadow cache so the next composed frame re-renders.
    pub fn set_shadow_quality(&mut self, requested: BuiltinShadowQuality) -> BuiltinShadowQuality {
        let negotiated =
            negotiate_builtin_shadow_quality(requested, &self.ctx.report.caps);
        if negotiated != self.shadow_quality {
            self.shadow_cache.invalidate();
        }
        self.shadow_quality = negotiated;
        negotiated
    }

    /// Set the render-scale preset for the composed world pass. The scaled
    /// intermediate target is (re)created lazily on the next frame.
    pub fn set_render_scale(&mut self, preset: RenderScalePreset) {
        if preset != self.render_scale {
            self.render_scale = preset;
            self.scaled_target = None;
        }
    }

    /// Consult the dirty-only shadow cache for this frame's inputs. The
    /// decision is recorded in [`Self::last_shadow_decision`]; zero-cascade
    /// presets (`Off`, `Feather`) always resolve `ReuseCached` once primed.
    pub fn shadow_decision(
        &mut self,
        camera: &Camera,
        world_revision: u64,
        world_time_ticks: u64,
    ) -> ShadowCacheDecision {
        let sun = compute_sun_direction(world_time_ticks);
        let chunk = [
            (camera.position[0] / 16.0).floor() as i32,
            (camera.position[1] / 16.0).floor() as i32,
            (camera.position[2] / 16.0).floor() as i32,
        ];
        self.last_shadow_decision = self.shadow_cache.should_rerender_shadow_map(
            sun,
            chunk,
            world_revision,
            self.shadow_quality,
        );
        self.last_shadow_decision
    }

    /// Mirror a [`SubWorldRenderBank`] onto the device: uploads only dirty
    /// geometry/instance payloads and clears the bank's dirty flags.
    pub fn sync_subworlds(&mut self, bank: &mut SubWorldRenderBank) {
        self.subworld_gpu
            .sync(&self.ctx.device, &self.ctx.queue, bank);
    }

    /// Number of GPU-resident sub-world entries (for diagnostics/tests).
    pub fn subworld_gpu_entry_count(&self) -> usize {
        self.subworld_gpu.entry_count()
    }

    /// Number of GPU-resident instanced batches (for diagnostics/tests).
    pub fn subworld_gpu_batch_count(&self) -> usize {
        self.subworld_gpu.batch_count()
    }

    /// Write the frame uniform from an explicit fog state instead of the
    /// vanilla `evaluate_sky_and_fog` — the composed pass takes its fog
    /// color and distances from the atmosphere blender.
    fn write_frame_uniforms(
        &self,
        camera: &Camera,
        aspect: f32,
        fog_rgb: [f32; 3],
        fog_start: f32,
        fog_end: f32,
    ) {
        let uniforms = FrameUniforms {
            view_proj: camera.view_projection(aspect),
            camera_pos: camera.position,
            fog_start,
            fog_color: fog_rgb,
            fog_end,
            _pad: [0.0; 4],
        };
        self.ctx
            .queue
            .write_buffer(&self.frame_ubo, 0, bytemuck::bytes_of(&uniforms));
    }

    /// Write the shared environment + VFX-light uniforms for one composed
    /// frame from the blender's current atmosphere.
    fn write_env_uniforms(&self, frame: &ComposedFrame<'_>) {
        let atmo = frame.atmosphere.current();
        let sun_dir = compute_sun_direction(frame.world_time_ticks);
        let env = CompositeEnvUniforms {
            sun: [sun_dir[0], sun_dir[1], sun_dir[2], atmo.sun_intensity],
            ambient: [
                atmo.ambient_rgb[0],
                atmo.ambient_rgb[1],
                atmo.ambient_rgb[2],
                1.0,
            ],
            atmo: [atmo.fog_density, 0.0, 0.0, 0.0],
            depth_tint: [
                atmo.depth_tint_rgb[0],
                atmo.depth_tint_rgb[1],
                atmo.depth_tint_rgb[2],
                atmo.depth_tint_strength,
            ],
        };
        self.ctx
            .queue
            .write_buffer(&self.composite.env_ubo, 0, bytemuck::bytes_of(&env));
        let lights = pack_vfx1_lights(frame.lights, self.shadow_quality);
        let packed = VfxLightsGpu::from(&lights);
        self.ctx.queue.write_buffer(
            &self.composite.lights_ubo,
            0,
            bytemuck::bytes_of(&packed),
        );
    }

    /// Grow the dynamic-offset draw-uniform pool past `needed` records
    /// (rebuilds the buffer + bind group; steady frames never hit this).
    fn ensure_draw_capacity(&mut self, needed: usize) {
        let bytes = needed as u64 * u64::from(DRAW_UNIFORM_STRIDE);
        if bytes <= self.composite.draw_capacity {
            return;
        }
        let records = (needed as u64).max(64) + needed as u64 / 2;
        let capacity = records * u64::from(DRAW_UNIFORM_STRIDE);
        let draw_ubo = self.ctx.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("composite-draw-uniforms"),
            size: capacity,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let draw_bg = self
            .ctx
            .device
            .create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("composite-draw-bind-group"),
                layout: &self.composite.draw_layout,
                entries: &[wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                        buffer: &draw_ubo,
                        offset: 0,
                        size: wgpu::BufferSize::new(u64::from(DRAW_UNIFORM_STRIDE)),
                    }),
                }],
            });
        self.composite.draw_ubo = draw_ubo;
        self.composite.draw_bg = draw_bg;
        self.composite.draw_capacity = capacity;
    }

    /// Ensure the reduced-resolution world target exists at `size`
    /// (only used when `render_scale < Native100`).
    fn ensure_scaled_target(&mut self, width: u32, height: u32) {
        if self
            .scaled_target
            .as_ref()
            .is_some_and(|t| t.size == (width, height))
        {
            return;
        }
        let texture = self.ctx.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("scaled-world-target"),
            size: wgpu::Extent3d {
                width: width.max(1),
                height: height.max(1),
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: self.target_format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        });
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let sampler = self.ctx.device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("scaled-upscale-sampler"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..wgpu::SamplerDescriptor::default()
        });
        let bind_group = self
            .ctx
            .device
            .create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("scaled-upscale-bind-group"),
                layout: &self.ui_layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: wgpu::BindingResource::TextureView(&view),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::Sampler(&sampler),
                    },
                ],
            });
        self.scaled_target = Some(ScaledTarget {
            _texture: texture,
            view,
            bind_group,
            size: (width, height),
        });
    }

    /// Encode the whole world pass (terrain + sub-worlds + instanced parts)
    /// into `target.view` at `target.size`, clearing to the atmosphere
    /// horizon. `target.view_proj` must already match the target aspect.
    fn encode_composed_world(
        &mut self,
        encoder: &mut wgpu::CommandEncoder,
        target: &ComposedWorldTarget<'_>,
    ) {
        let ComposedWorldTarget {
            view,
            size: (width, height),
            view_proj,
            clear_rgb,
            chunk_fades,
        } = *target;
        self.ensure_depth(width, height);
        // Split disjoint field borrows so the scratch writer and the GPU
        // handles can coexist inside the render pass.
        let gpu = &mut self.composite;
        let scene = &self.scene;
        let live_sections = &self.live_sections;
        let subworld_gpu = &self.subworld_gpu;
        let frame_bind_group = &self.frame_bind_group;
        let depth_view = self.depth_view.as_ref().expect("depth ensured");
        gpu.draw_scratch.clear();
        let scratch = &mut gpu.draw_scratch;
        let mut slot = |model: Mat4, fade: f32, alpha: f32| -> u32 {
            let index = scratch.len();
            scratch.push(CompositeDrawUniforms {
                mvp: mat4_mul(&view_proj, &model),
                model,
                params: [fade, alpha, 0.0, 0.0],
                params2: [0.0; 4],
                _stride_pad: [0.0; 24],
            });
            index as u32 * DRAW_UNIFORM_STRIDE
        };
        let identity: Mat4 = [
            [1.0, 0.0, 0.0, 0.0],
            [0.0, 1.0, 0.0, 0.0],
            [0.0, 0.0, 1.0, 0.0],
            [0.0, 0.0, 0.0, 1.0],
        ];
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("composed-world"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: f64::from(clear_rgb[0]),
                            g: f64::from(clear_rgb[1]),
                            b: f64::from(clear_rgb[2]),
                            a: 1.0,
                        }),
                        store: wgpu::StoreOp::Store,
                    },
                    depth_slice: None,
                })],
                depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                    view: depth_view,
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
            pass.set_bind_group(2, &gpu.env_bg, &[]);

            // Opaque terrain: the demo scene plus every live section, each
            // with its chunk-column fade.
            pass.set_pipeline(&gpu.terrain_opaque_pipeline);
            if let Some(scene) = scene {
                if scene.opaque_index_count > 0 {
                    let off = slot(identity, 1.0, 1.0);
                    pass.set_bind_group(0, frame_bind_group, &[]);
                    pass.set_bind_group(1, &gpu.draw_bg, &[off]);
                    pass.set_vertex_buffer(0, scene.opaque_vertices.slice(..));
                    pass.set_index_buffer(
                        scene.opaque_indices.slice(..),
                        wgpu::IndexFormat::Uint32,
                    );
                    pass.draw_indexed(0..scene.opaque_index_count, 0, 0..1);
                }
            }
            for (coord, section) in live_sections.iter() {
                if section.terrain.opaque_index_count == 0 {
                    continue;
                }
                let fade = chunk_fades.fade_alpha(coord.0, coord.2);
                let off = slot(identity, fade, 1.0);
                pass.set_bind_group(0, &section.bind_group, &[]);
                pass.set_bind_group(1, &gpu.draw_bg, &[off]);
                pass.set_vertex_buffer(0, section.terrain.opaque_vertices.slice(..));
                pass.set_index_buffer(
                    section.terrain.opaque_indices.slice(..),
                    wgpu::IndexFormat::Uint32,
                );
                pass.draw_indexed(0..section.terrain.opaque_index_count, 0, 0..1);
            }

            // Translucent terrain (water, stained glass) through the
            // alpha-blended composite variant.
            pass.set_pipeline(&gpu.terrain_alpha_pipeline);
            if let Some(scene) = scene {
                if scene.translucent_index_count > 0 {
                    let off = slot(identity, 1.0, 1.0);
                    pass.set_bind_group(0, frame_bind_group, &[]);
                    pass.set_bind_group(1, &gpu.draw_bg, &[off]);
                    pass.set_vertex_buffer(0, scene.translucent_vertices.slice(..));
                    pass.set_index_buffer(
                        scene.translucent_indices.slice(..),
                        wgpu::IndexFormat::Uint32,
                    );
                    pass.draw_indexed(0..scene.translucent_index_count, 0, 0..1);
                }
            }
            for (coord, section) in live_sections.iter() {
                if section.terrain.translucent_index_count == 0 {
                    continue;
                }
                let fade = chunk_fades.fade_alpha(coord.0, coord.2);
                let off = slot(identity, fade, 1.0);
                pass.set_bind_group(0, &section.bind_group, &[]);
                pass.set_bind_group(1, &gpu.draw_bg, &[off]);
                pass.set_vertex_buffer(0, section.terrain.translucent_vertices.slice(..));
                pass.set_index_buffer(
                    section.terrain.translucent_indices.slice(..),
                    wgpu::IndexFormat::Uint32,
                );
                pass.draw_indexed(0..section.terrain.translucent_index_count, 0, 0..1);
            }

            // 6-DOF sub-worlds, opaque first (airship hulls, planets,
            // contraptions), then instanced mechanical parts, then the
            // alpha-blended entries (cloud shells).
            let mut drew_opaque_subworld = false;
            for entry in subworld_gpu.entries() {
                if entry.index_count == 0 || entry.alpha < 0.999 {
                    continue;
                }
                if !drew_opaque_subworld {
                    pass.set_pipeline(&gpu.lit_pipeline);
                    pass.set_bind_group(0, &gpu.lit_frame_bg, &[]);
                    drew_opaque_subworld = true;
                }
                let model = entry.transform.model_matrix();
                let off = slot(model, 1.0, entry.alpha);
                pass.set_bind_group(1, &gpu.draw_bg, &[off]);
                pass.set_vertex_buffer(0, entry.vertex_buffer().slice(..));
                pass.set_index_buffer(entry.index_buffer().slice(..), wgpu::IndexFormat::Uint32);
                pass.draw_indexed(0..entry.index_count, 0, 0..1);
            }
            let mut drew_instanced = false;
            for batch in subworld_gpu.batches() {
                if batch.index_count == 0 || batch.instance_count == 0 {
                    continue;
                }
                if !drew_instanced {
                    pass.set_pipeline(&gpu.instanced_pipeline);
                    pass.set_bind_group(0, &gpu.lit_frame_bg, &[]);
                    drew_instanced = true;
                }
                let model = batch.transform.model_matrix();
                let off = slot(model, 1.0, 1.0);
                pass.set_bind_group(1, &gpu.draw_bg, &[off]);
                pass.set_vertex_buffer(0, batch.template_vertex_buffer().slice(..));
                pass.set_vertex_buffer(1, batch.instance_buffer().slice(..));
                pass.set_index_buffer(
                    batch.template_index_buffer().slice(..),
                    wgpu::IndexFormat::Uint32,
                );
                pass.draw_indexed(0..batch.index_count, 0, 0..batch.instance_count);
            }
            let mut drew_alpha = false;
            for entry in subworld_gpu.entries() {
                if entry.index_count == 0 || entry.alpha >= 0.999 {
                    continue;
                }
                if !drew_alpha {
                    pass.set_pipeline(&gpu.lit_alpha_pipeline);
                    pass.set_bind_group(0, &gpu.lit_frame_bg, &[]);
                    drew_alpha = true;
                }
                let model = entry.transform.model_matrix();
                let off = slot(model, 1.0, entry.alpha);
                pass.set_bind_group(1, &gpu.draw_bg, &[off]);
                pass.set_vertex_buffer(0, entry.vertex_buffer().slice(..));
                pass.set_index_buffer(entry.index_buffer().slice(..), wgpu::IndexFormat::Uint32);
                pass.draw_indexed(0..entry.index_count, 0, 0..1);
            }
        }
        // Upload the recorded draw uniforms ahead of this frame's submit.
        let gpu = &self.composite;
        self.ctx.queue.write_buffer(
            &gpu.draw_ubo,
            0,
            bytemuck::cast_slice(&gpu.draw_scratch),
        );
    }

    /// Render a full composed frame offscreen and read the pixels back:
    /// terrain with smooth chunk fade + atmosphere clear/fog, 6-DOF
    /// sub-worlds, instanced mechanical parts, VFX1 colored lights, and the
    /// UI overlay at native resolution. `render_scale < 100%` renders the
    /// world into a smaller target and upscales before the UI pass.
    pub fn render_offscreen_composed_frame(
        &mut self,
        width: u32,
        height: u32,
        camera: &Camera,
        frame: &mut ComposedFrame<'_>,
    ) -> Result<Vec<u8>, RendererError> {
        let width = width.max(1);
        let height = height.max(1);
        self.subworld_gpu
            .sync(&self.ctx.device, &self.ctx.queue, frame.subworlds);
        if let Some(rgba) = frame.ui_rgba {
            self.update_ui_framebuffer(width, height, rgba)?;
        }
        let (world_w, world_h) = self.render_scale.scaled_dimensions(width, height);
        let scaled = (world_w, world_h) != (width, height);
        if scaled {
            self.ensure_scaled_target(world_w, world_h);
        }
        self.shadow_decision(camera, frame.world_revision, frame.world_time_ticks);

        let atmo = frame.atmosphere.current();
        let view = camera.far.clamp(16.0, 2048.0);
        self.write_frame_uniforms(
            camera,
            world_w as f32 / world_h as f32,
            atmo.fog_rgb,
            view * 0.6,
            view,
        );
        self.write_env_uniforms(frame);
        // Worst case: one slot per live section + scene + subworlds + batches.
        let draw_count = self.live_sections.len() * 2
            + 2
            + self.subworld_gpu.entry_count()
            + self.subworld_gpu.batch_count();
        self.ensure_draw_capacity(draw_count.max(8));

        // The world pass renders into the scaled target (or straight into
        // the readback texture at native scale).
        let texture = self.ctx.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("composed-target"),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: self.target_format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let native_view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let view_proj = camera.view_projection(world_w as f32 / world_h as f32);
        let clear_rgb = [
            atmo.sky_horizon_rgb[0].max(atmo.fog_rgb[0] * 0.8),
            atmo.sky_horizon_rgb[1].max(atmo.fog_rgb[1] * 0.8),
            atmo.sky_horizon_rgb[2].max(atmo.fog_rgb[2] * 0.8),
        ];
        let mut encoder = self
            .ctx
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("composed-frame"),
            });
        if scaled {
            let target_view = self
                .scaled_target
                .as_ref()
                .map(|t| t.view.clone())
                .expect("ensured above");
            self.encode_composed_world(
                &mut encoder,
                &ComposedWorldTarget {
                    view: &target_view,
                    size: (world_w, world_h),
                    view_proj,
                    clear_rgb,
                    chunk_fades: frame.chunk_fades,
                },
            );
            // Upscale the reduced-res world to native before the UI pass.
            {
                let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("scaled-upscale"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: &native_view,
                        resolve_target: None,
                        ops: wgpu::Operations {
                            load: wgpu::LoadOp::Load,
                            store: wgpu::StoreOp::Store,
                        },
                        depth_slice: None,
                    })],
                    depth_stencil_attachment: None,
                    timestamp_writes: None,
                    occlusion_query_set: None,
                    multiview_mask: None,
                });
                pass.set_pipeline(&self.ui_pipeline);
                pass.set_bind_group(0, &self.scaled_target.as_ref().expect("ensured").bind_group, &[]);
                pass.draw(0..3, 0..1);
            }
        } else {
            self.encode_composed_world(
                &mut encoder,
                &ComposedWorldTarget {
                    view: &native_view,
                    size: (width, height),
                    view_proj,
                    clear_rgb,
                    chunk_fades: frame.chunk_fades,
                },
            );
        }
        // UI overlay at native resolution.
        if let Some(ui_bind_group) = &self.ui_bind_group {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("ui-overlay"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &native_view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Load,
                        store: wgpu::StoreOp::Store,
                    },
                    depth_slice: None,
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            pass.set_pipeline(&self.ui_pipeline);
            pass.set_bind_group(0, ui_bind_group, &[]);
            pass.draw(0..3, 0..1);
        }

        let padded_bpr = crate::aligned_copy_bytes_per_row(width);
        let buffer = self.ctx.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("composed-readback"),
            size: u64::from(padded_bpr) * u64::from(height),
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture: &texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &buffer,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(padded_bpr),
                    rows_per_image: Some(height),
                },
            },
            wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
        );
        self.ctx.queue.submit([encoder.finish()]);
        self.map_readback_buffer(&buffer, width, padded_bpr, height)
    }
}

fn uniform_entry(binding: u32) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
        ty: wgpu::BindingType::Buffer {
            ty: wgpu::BufferBindingType::Uniform,
            has_dynamic_offset: false,
            min_binding_size: None,
        },
        count: None,
    }
}

fn vertex_buffer(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    label: &str,
    verts: &[PackedVertex],
) -> wgpu::Buffer {
    let buffer = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some(label),
        size: std::mem::size_of_val(verts).max(16) as u64,
        usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    if !verts.is_empty() {
        // PackedVertex mirrors the C header's four native-endian u32s; it has
        // no Pod impl, so serialize the fields explicitly.
        let mut bytes = Vec::with_capacity(std::mem::size_of_val(verts));
        for v in verts {
            bytes.extend_from_slice(&v.position.to_ne_bytes());
            bytes.extend_from_slice(&v.state_id.to_ne_bytes());
            bytes.extend_from_slice(&v.attributes.to_ne_bytes());
            bytes.extend_from_slice(&v.tint.to_ne_bytes());
        }
        queue.write_buffer(&buffer, 0, &bytes);
    }
    buffer
}

fn index_buffer(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    label: &str,
    indices: &[u32],
) -> wgpu::Buffer {
    let buffer = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some(label),
        size: std::mem::size_of_val(indices).max(4) as u64,
        usage: wgpu::BufferUsages::INDEX | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    if !indices.is_empty() {
        queue.write_buffer(&buffer, 0, bytemuck::cast_slice(indices));
    }
    buffer
}
