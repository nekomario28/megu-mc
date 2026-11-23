// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! Real `wgpu` renderer: packed-vertex terrain pipelines, the `client-ui`
//! RGBA overlay blit, a depth-buffered camera, and a device-validated Megu
//! native WGSL shaderpack hook that never crashes — any rejection falls back
//! to the built-in pipeline with a human-readable notice.

use std::borrow::Cow;
use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use bytemuck::{Pod, Zeroable};
use megu_client::shaders::{
    validate_wgsl_module, WgslStage, TERRAIN_OPAQUE_WGSL, TERRAIN_TRANSLUCENT_WGSL,
};
use megu_client::world::WorldState;
use megu_client_mesh::atlas::{AtlasMip, AtlasTileRect, PackedAtlas, SpriteAnimation};
use megu_client_mesh::ber_instancing::{VanillaParticleQuad, VanillaParticleWorld};
use megu_client_mesh::builtin_shadow_wgsl::{
    build_shadow_uniforms_celestial, compute_cascade_splits, compute_sun_direction,
    negotiate_builtin_shadow_quality, pack_vfx1_lights, BuiltinShadowQuality, CascadeConfig,
    DirtyOnlyShadowCache, EclipseMode, RenderScalePreset, ShadowCacheDecision, ShadowCamera,
    ShadowDirtyReason, ShadowUniforms, MEGU_SHADOW_CASTERS_WGSL, MEGU_SHADOW_CUTOUT_DEPTH_WGSL,
    MEGU_SHADOW_DEPTH_WGSL, MEGU_SHADOW_SAMPLING_WGSL, MEGU_SKY_ATMOSPHERE_WGSL,
    MEGU_SPATIAL_SHARPEN_WGSL, MEGU_STAINED_GLASS_SHADOW_COLOR_WGSL, MEGU_TERRAIN_LIT_WGSL,
    MEGU_WATER_SURFACE_WGSL,
};
use megu_client_mesh::ffi::PackedVertex;
use megu_client_mesh::vanilla_plus_3d::ExtrudedMesh;
use megu_client_mesh::vfx_engine::DynamicColoredLight;
use megu_client_mesh::{
    box_facing_mask, box_in_frustum, bucket_draw_mask, ChunkUploadBudget, Direction,
    DirectionBuckets, GraphCuller, HiZPyramid, RenderRegions, SectionCoord, SectionGraph,
    DIRECTION_BUCKETS, EDGE, REGION_X, REGION_Y, REGION_Z,
};
use megu_client_mesh::{
    is_megu_wgsl_pack, BackendResolution, GpuCapabilities, GpuShaderCaps, MeguWgslPack, PassKind,
    PipelineStage, RenderBackendPreference, ShaderPackSource,
};
use megu_core::math_opt_engine::{
    morton3d_encode, morton3d_signed, projected_sphere_diameter_px,
};
use megu_core::math_opt_foundation::{LipschitzTileClassifier, TileSignDecision};
use megu_kernel::debug_chords::DebugLine;
use megu_kernel::entity_interp::{evaluate_sky_and_fog, SkyFogInput, SkyFogOutput};
use megu_ui::draw::{font_atlas_rgba, UiMesh, FONT_ATLAS_HEIGHT, FONT_ATLAS_WIDTH};
use winit::event_loop::OwnedDisplayHandle;

use crate::atmosphere::{AtmosphereParams, BiomeAtmosphereBlender, ChunkFadeTracker};
use crate::gpu_hiz::{CullStats, CullUniforms, GpuHizCuller, IndirectArgs, SectionGpuRecord};
use crate::live_session::hw_adaptive_engine::{
    detect_universal_hardware_profile, select_compute_strategy, ComputeStrategy,
    GpuComputeParadigm, MemoryFootprintClass, MemoryTransferStrategy,
};
use crate::mat4_mul;
use crate::mesh_pool::{
    compute_opaque_bucket_ranges, HiZSplatSection, MeshJob, MeshJobPool, MeshResult,
};
use crate::subworld_render::{
    pack_particle_billboard, CompositeDrawUniforms, CompositeEnvUniforms, DebugLineVertex,
    ParticleBillboardPack, ParticleInstance, SubWorldGpuCache, SubWorldRenderBank, TerrainVertex,
    VfxLightsGpu, COMPOSITE_LIT_WGSL, COMPOSITE_TERRAIN_WGSL, DEBUG_LINE_VERTEX_LAYOUT,
    DEBUG_LINE_WGSL, DRAW_UNIFORM_STRIDE, LIT_VERTEX_LAYOUT, PARTICLE_BILLBOARD_CORNERS,
    PARTICLE_BILLBOARD_INDICES, PARTICLE_INSTANCE_LAYOUT, PARTICLE_QUAD_LAYOUT,
    PART_INSTANCE_LAYOUT,
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
        label: Some("megu-client-gpu"),
        required_features: wgpu::Features::empty(),
        // The adapter's own limits, clamped to sane engine ceilings:
        // guaranteed grantable, and low-spec adapters keep their clamped
        // ceilings instead of the inflated `Limits::default()` a
        // discrete-GPU bucket would request.
        required_limits: negotiated_device_limits(&adapter),
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

/// Clamp a `wgpu::Limits` request to the ceilings the renderer actually
/// uses. Every clamp only *lowers* the request below what the adapter
/// advertised — `request_device` can never over-ask — while pathological
/// driver reports (u32::MAX binding sizes on old Vulkan 1.1 / DX12 /
/// Metal iGPUs) come down to values the engine can actually exploit.
/// High-end GPUs keep their full ceilings wherever they matter.
pub fn clamp_requested_limits(limits: wgpu::Limits) -> wgpu::Limits {
    let mut l = limits;
    l.max_texture_dimension_1d = l.max_texture_dimension_1d.min(16384);
    l.max_texture_dimension_2d = l.max_texture_dimension_2d.min(16384);
    l.max_texture_dimension_3d = l.max_texture_dimension_3d.min(4096);
    l.max_texture_array_layers = l.max_texture_array_layers.min(2048);
    l.max_bind_groups = l.max_bind_groups.min(8);
    l.max_bind_groups_plus_vertex_buffers = l.max_bind_groups_plus_vertex_buffers.min(32);
    l.max_bindings_per_bind_group = l.max_bindings_per_bind_group.min(1000);
    l.max_dynamic_uniform_buffers_per_pipeline_layout =
        l.max_dynamic_uniform_buffers_per_pipeline_layout.min(16);
    l.max_dynamic_storage_buffers_per_pipeline_layout =
        l.max_dynamic_storage_buffers_per_pipeline_layout.min(8);
    l.max_sampled_textures_per_shader_stage = l.max_sampled_textures_per_shader_stage.min(64);
    l.max_samplers_per_shader_stage = l.max_samplers_per_shader_stage.min(32);
    l.max_storage_buffers_per_shader_stage = l.max_storage_buffers_per_shader_stage.min(32);
    l.max_storage_textures_per_shader_stage = l.max_storage_textures_per_shader_stage.min(16);
    l.max_uniform_buffers_per_shader_stage = l.max_uniform_buffers_per_shader_stage.min(24);
    l.max_binding_array_elements_per_shader_stage =
        l.max_binding_array_elements_per_shader_stage.min(1_000_000);
    l.max_binding_array_acceleration_structure_elements_per_shader_stage = l
        .max_binding_array_acceleration_structure_elements_per_shader_stage
        .min(1_000_000);
    l.max_binding_array_sampler_elements_per_shader_stage = l
        .max_binding_array_sampler_elements_per_shader_stage
        .min(1_000_000);
    l.max_uniform_buffer_binding_size = l.max_uniform_buffer_binding_size.min(1 << 30);
    l.max_storage_buffer_binding_size = l.max_storage_buffer_binding_size.min(1 << 31);
    l.max_vertex_buffers = l.max_vertex_buffers.min(16);
    l.max_buffer_size = l.max_buffer_size.min(4 << 30);
    l.max_vertex_attributes = l.max_vertex_attributes.min(32);
    l.max_vertex_buffer_array_stride = l.max_vertex_buffer_array_stride.min(4096);
    l.max_inter_stage_shader_variables = l.max_inter_stage_shader_variables.min(32);
    l.max_color_attachments = l.max_color_attachments.min(8);
    l.max_color_attachment_bytes_per_sample = l.max_color_attachment_bytes_per_sample.min(64);
    l.max_compute_workgroup_storage_size = l.max_compute_workgroup_storage_size.min(32768);
    l.max_compute_invocations_per_workgroup = l.max_compute_invocations_per_workgroup.min(1024);
    l.max_compute_workgroup_size_x = l.max_compute_workgroup_size_x.min(1024);
    l.max_compute_workgroup_size_y = l.max_compute_workgroup_size_y.min(1024);
    l.max_compute_workgroup_size_z = l.max_compute_workgroup_size_z.min(64);
    l.max_compute_workgroups_per_dimension = l.max_compute_workgroups_per_dimension.min(65535);
    l
}

/// The device limits to request for `adapter`: the adapter's own report
/// (always grantable, so no Vulkan 1.1 / DX12 / Metal iGPU can fail
/// device creation), clamped to sane engine ceilings.
pub fn negotiated_device_limits(adapter: &wgpu::Adapter) -> wgpu::Limits {
    clamp_requested_limits(adapter.limits())
}

/// Depth-store policy for the final world pass. Tile-based deferred
/// renderers keep the depth attachment in on-chip tile memory: when no
/// later pass or readback consumes it, `Discard` skips the depth
/// writeback entirely. Everywhere else (and whenever a consumer exists)
/// the attachment is stored as before.
pub fn select_depth_store_op(
    paradigm: GpuComputeParadigm,
    needs_depth_readback: bool,
) -> wgpu::StoreOp {
    if needs_depth_readback {
        return wgpu::StoreOp::Store;
    }
    match paradigm {
        GpuComputeParadigm::TileBasedTbdr => wgpu::StoreOp::Discard,
        _ => wgpu::StoreOp::Store,
    }
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

/// Section-in-region offset packed into `PackedVertex::attributes` bits
/// 24-31 for the shared 8x4x8-section region arena: `sx | (sy << 3) |
/// (sz << 5)` where `sx, sz in 0..8` and `sy in 0..4`. Every WGSL decode
/// and post-pack consumer reads only attribute bits 0-17, so the top
/// byte (quad extents/corner, dead on the GPU) is cleared and reused —
/// and the section-local `position` word keeps all 25 coordinate bits
/// (including the 1/1024 `ly` precision) 100% untouched.
fn region_section_word(origin: [i32; 3], coord: SectionCoord) -> u32 {
    let sx = (coord.0 - origin[0]) as u32;
    let sy = (coord.1 - origin[1]) as u32;
    let sz = (coord.2 - origin[2]) as u32;
    (sx | (sy << 3) | (sz << 5)) << 24
}

const DEPTH_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Depth32Float;
/// Offscreen/UI-framebuffer pixel format.
pub const OFFSCREEN_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8UnormSrgb;

/// Fixed demo time for the legacy camera-free entry points
/// (`render_to_view`, scene-only tests): vanilla noon. Live paths pass the
/// authoritative `world_time_ticks` through
/// [`GpuRenderer::render_to_view_with_env`] instead.
const DEMO_WORLD_TIME_TICKS: u64 = 6000;

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

/// Pipelines an external `MeguWgslPack` `gbuffers_subworld`/
/// `gbuffers_entities`-style pass supplies for Create sub-worlds,
/// kinetic parts and entities — built against
/// [`CompositeGpu::pack_lit_layout`] so the module reads the exact
/// frame/draw/environment(+atlas)/shadow-receiver uniforms the built-in
/// `COMPOSITE_LIT_WGSL` draws see. Every slot is independently
/// optional: whichever pipeline the device rejected keeps its built-in
/// counterpart (`gpu.lit_pipeline`/`gpu.instanced_pipeline`/
/// `gpu.lit_alpha_pipeline`, or `shadow.lit_color`/
/// `shadow.instanced_color`/`shadow.lit_alpha_color` while cascades are
/// active). `needs_shadow` records that the module declared group-3
/// bindings — such pipelines only draw on shadow-active frames, since a
/// shadow-free frame never binds the receiver group.
struct PackSubworldGpu {
    /// Pass that produced the module (`gbuffers_subworld`, ...).
    pass_name: String,
    /// `TerrainVertex` draws (opaque sub-worlds, entities/BER).
    lit: Option<wgpu::RenderPipeline>,
    /// Alpha-blended `TerrainVertex` draws (translucent sub-worlds,
    /// particles).
    lit_alpha: Option<wgpu::RenderPipeline>,
    /// Shared-template instanced draws (cogwheels, water wheels,
    /// shafts).
    instanced: Option<wgpu::RenderPipeline>,
    /// Module binds `@group(3)` — draw only while the shadow receiver
    /// bind group is live.
    needs_shadow: bool,
}

/// `gbuffers_*` program names a [`MeguWgslPack`] may dedicate to
/// sub-worlds, kinetic parts and entities. The first Naga-clean pass the
/// device accepts replaces the matching built-in lit pipelines; these
/// names are excluded from terrain candidacy since they shade a
/// different geometry class.
const PACK_SUBWORLD_PASS_NAMES: &[&str] = &[
    "gbuffers_subworld",
    "gbuffers_contraption",
    "gbuffers_entities",
    "gbuffers_block",
];

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
    /// Cutout-quad indices into `opaque_vertices` (shadow-pass only).
    cutout_indices: wgpu::Buffer,
    cutout_index_count: u32,
    /// Decoded `MEGU_WATER_SURFACE_WGSL` stream (world pos + depth hint).
    water_vertices: wgpu::Buffer,
    water_indices: wgpu::Buffer,
    water_index_count: u32,
}

/// Decoded water-surface vertex for `MEGU_WATER_SURFACE_WGSL`
/// (`@location(0)` world blocks, `@location(1)` depth hint `0..=1`).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct WaterVertex {
    /// World-space position in blocks.
    pub position: [f32; 3],
    /// Approximate depth `0.0..=1.0` (deeper = darker blue).
    pub depth_hint: f32,
}

// bytemuck Pod/Zeroable for direct buffer upload.
unsafe impl bytemuck::Pod for WaterVertex {}
unsafe impl bytemuck::Zeroable for WaterVertex {}

/// World-space vertex for `MEGU_TERRAIN_LIT_WGSL`: position, normal,
/// albedo and the packed light word (`block_light` bits 5..8,
/// `sky_light` bits 9..12 — the shader decodes both).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct LitTerrainVertex {
    /// World-space position in blocks.
    pub position: [f32; 3],
    /// Unit outward normal.
    pub normal: [f32; 3],
    /// Linear-ish albedo `0.0..=1.0` per channel.
    pub albedo: [f32; 3],
    /// `PackedVertex::attributes`-compatible light word.
    pub light_attr: u32,
}

// bytemuck Pod/Zeroable for direct buffer upload.
unsafe impl bytemuck::Pod for LitTerrainVertex {}
unsafe impl bytemuck::Zeroable for LitTerrainVertex {}

/// World-space stained-glass caster for
/// `MEGU_STAINED_GLASS_SHADOW_COLOR_WGSL` (`@location(1)` carries the
/// vanilla `DyeColor` index `0..=15`).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct StainedCasterVertex {
    /// World-space position in blocks.
    pub position: [f32; 3],
    /// Vanilla dye index `0..=15` (white .. black).
    pub color_idx: u32,
}

// bytemuck Pod/Zeroable for direct buffer upload.
unsafe impl bytemuck::Pod for StainedCasterVertex {}
unsafe impl bytemuck::Zeroable for StainedCasterVertex {}

/// Local mesh vertices and draw indices, keeping translucent quads ordered.
#[derive(Clone, Debug, Default)]
pub struct LiveSectionGeometry {
    pub opaque: Vec<PackedVertex>,
    pub opaque_indices: Vec<u32>,
    /// Indices into `opaque` covering every `Layer::Cutout` quad — the
    /// alpha-test shadow caster pass draws these without any extra
    /// vertex copy.
    pub cutout_indices: Vec<u32>,
    pub translucent: Vec<PackedVertex>,
    pub translucent_indices: Vec<u32>,
    /// Water quads decoded to world space for the dedicated
    /// `MEGU_WATER_SURFACE_WGSL` pass.
    pub water: Vec<WaterVertex>,
    pub water_indices: Vec<u32>,
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

/// Canonical `[0,1,2,0,2,3]` winding one shared static quad index buffer
/// repeats for every quad. Opaque draws index it positionally: quad `q`
/// of a vertex buffer is always drawn by indices `6*q .. 6*q + 6`, so no
/// per-section or per-region index stream is ever built or uploaded.
pub const QUAD_INDEX_PATTERN: [u32; 6] = [0, 1, 2, 0, 2, 3];

/// `PackedVertex.attributes` bit 17: the greedy mesher's diagonal-flip
/// mark. Quads carrying it were historically drawn with the alternate
/// `[0,1,3,1,2,3]` corner set.
pub const QUAD_DIAGONAL_FLIP_BIT: u32 = 1 << 17;

/// Rotate every flip-marked opaque quad `[v0,v1,v2,v3] -> [v1,v2,v3,v0]`
/// once at upload time. Applied before GPU upload, the canonical
/// `[0,1,2,0,2,3]` index pattern then reproduces the alternate pattern's
/// exact two triangles (same `(v1,v3)` diagonal, same winding), so 100%
/// of opaque quads share one static index buffer with identical output.
pub fn rotate_opaque_quads_for_shared_index(vertices: &mut [PackedVertex]) {
    for quad in vertices.as_chunks_mut::<4>().0 {
        if quad[0].attributes & QUAD_DIAGONAL_FLIP_BIT != 0 {
            quad.rotate_left(1);
        }
    }
}

/// The shared index buffer's contents: [`QUAD_INDEX_PATTERN`] repeated
/// with a +4 stride per quad so index `i` always selects vertex
/// `i / 6 * 4 + pat[i % 6]` of the bound vertex buffer. `count` is
/// rounded up to a whole quad group.
pub fn quad_index_pattern(count: u32) -> Vec<u32> {
    let quads = count.div_ceil(6);
    let mut out = Vec::with_capacity(quads as usize * 6);
    for quad in 0..quads {
        let base = quad * 4;
        out.extend(QUAD_INDEX_PATTERN.map(|corner| base + corner));
    }
    out
}

/// Packed-attribute decode shared by `section_geometry` and callers:
/// `(layer, fluid_kind)` from `PackedVertex::attributes` bits 13..16.
#[inline(always)]
fn packed_layer_fluid(attributes: u32) -> (u32, u32) {
    ((attributes >> 13) & 3, (attributes >> 15) & 3)
}

/// `PackedVertex.position` bit decode — identical to every WGSL caster:
/// `x | (z << 5) | (y * 1024 << 10)` in section-local 1/1024-block steps.
#[inline(always)]
fn decode_packed_local(position: u32) -> [f32; 3] {
    [
        (position & 31) as f32,
        (position >> 10) as f32 / 1024.0,
        ((position >> 5) & 31) as f32,
    ]
}

/// Split one packed section mesh into the streams the live passes draw:
/// opaque (+ a shared-buffer cutout index stream for the shadow pass),
/// ordered translucent, and water quads decoded to world-space
/// [`WaterVertex`]s for `MEGU_WATER_SURFACE_WGSL`. `origin_blocks` is the
/// section's world origin (`coord * 16`).
///
/// `translucent_order` indexes the original packed quad array and covers
/// the union of `Layer::Translucent` + water quads (the sort the world
/// produced); a non-matching entry falls both streams back to mesh
/// order, preserving the pre-change semantics.
pub fn section_geometry(
    packed: &[PackedVertex],
    translucent_order: &[u32],
    origin_blocks: [f32; 3],
) -> LiveSectionGeometry {
    let mut out = LiveSectionGeometry::default();
    let mut translucent = Vec::new();
    let mut water_quads = Vec::new();
    for quad in packed.as_chunks::<4>().0 {
        let (layer, fluid) = packed_layer_fluid(quad[0].attributes);
        if layer == 3 && fluid == 1 {
            water_quads.push(quad.as_slice());
        } else if layer == 2 {
            translucent.push(quad.as_slice());
        } else {
            let base = out.opaque.len() as u32;
            append_live_quad(&mut out.opaque, &mut out.opaque_indices, quad);
            if layer == 1 {
                // Cutout stays in the opaque stream for the color pass;
                // a parallel index list feeds the alpha-test shadow
                // caster without duplicating vertex data.
                out.cutout_indices
                    .extend([0u32, 1, 2, 0, 2, 3].map(|c| base + c));
            }
        }
    }
    // The world sort refers to the original packed quad indices and
    // interleaves translucent and water quads — re-apply it to both
    // streams so back-to-front ordering survives the split.
    let mut ordered_translucent = Vec::new();
    let mut ordered_water = Vec::new();
    let mut seen = HashSet::new();
    for &idx in translucent_order {
        let index = idx as usize;
        let quad = packed.get(index.saturating_mul(4)..index.saturating_mul(4).saturating_add(4));
        let Some(quad) = quad else {
            ordered_translucent.clear();
            ordered_water.clear();
            break;
        };
        let (layer, fluid) = packed_layer_fluid(quad[0].attributes);
        if !seen.insert(index) {
            ordered_translucent.clear();
            ordered_water.clear();
            break;
        }
        if layer == 3 && fluid == 1 {
            ordered_water.push(quad);
        } else if layer == 2 {
            ordered_translucent.push(quad);
        } else {
            ordered_translucent.clear();
            ordered_water.clear();
            break;
        }
    }
    if ordered_translucent.len() != translucent.len() || ordered_water.len() != water_quads.len() {
        ordered_translucent = translucent;
        ordered_water = water_quads;
    }
    for quad in ordered_translucent {
        append_live_quad(&mut out.translucent, &mut out.translucent_indices, quad);
    }
    for quad in ordered_water {
        let base = out.water.len() as u32;
        for vertex in quad {
            let local = decode_packed_local(vertex.position);
            // Skylight at the surface vertex is a free depth proxy:
            // open water reads 15 (shallow, bright), shaded/deep water
            // dims toward the deep-sea colour — no packed field needed.
            let sky_light = ((vertex.attributes >> 9) & 0xf) as f32 / 15.0;
            out.water.push(WaterVertex {
                position: [
                    origin_blocks[0] + local[0],
                    origin_blocks[1] + local[1],
                    origin_blocks[2] + local[2],
                ],
                depth_hint: (1.0 - sky_light).clamp(0.0, 1.0),
            });
        }
        let corners = if quad[0].attributes & (1 << 17) == 0 {
            [0, 1, 2, 0, 2, 3]
        } else {
            [0, 1, 3, 1, 2, 3]
        };
        out.water_indices.extend(corners.map(|c| base + c));
    }
    out
}

struct LiveTerrainSection {
    /// Grouped packed quads, shared with speculative worker jobs and the
    /// Hi-Z splat inputs (`Arc` avoids re-cloning per job).
    packed: Arc<[PackedVertex]>,
    order: Arc<[u32]>,
    /// Per-section geometry streams. Opaque buffers are the shared dummy
    /// pair when `arena` routes the section into a region arena.
    terrain: TerrainBuffers,
    /// Logical opaque index count regardless of where the indices live —
    /// the visibility/cull gate needs this even for arena-resident
    /// sections whose per-section opaque buffers are empty.
    opaque_index_total: u32,
    /// Per-bucket `(first, count)` spans of the section-local opaque index
    /// stream; `Some` whenever the world published [`DirectionBuckets`],
    /// so the per-section fallback path can bucket-cull too.
    opaque_bucket_ranges: Option<[(u32, u32); DIRECTION_BUCKETS]>,
    /// Quad-level direction buckets of `packed`, for the Hi-Z splat job.
    buckets: Option<DirectionBuckets>,
    /// Containing region origin when the opaque stream lives in
    /// [`GpuRenderer::region_arenas`].
    arena: Option<[i32; 3]>,
    _uniform: wgpu::Buffer,
    bind_group: wgpu::BindGroup,
}

/// CPU-side record of a section mesh queued for GPU upload. The render
/// loop's [`ChunkUploadBudget`] admits a bounded number per frame; the
/// packed payload is cloned once at queue time so the `WorldState` borrow
/// ends before the deferred GPU work runs.
struct PendingSectionUpload {
    packed: Arc<[PackedVertex]>,
    order: Arc<[u32]>,
    /// Direction buckets mirrored at queue time — they decide whether the
    /// admitted section routes into a region arena or per-section buffers.
    buckets: Option<DirectionBuckets>,
    /// Estimated GPU payload bytes charged against the budget.
    upload_bytes: usize,
    /// Estimated upload-side CPU cost in microseconds.
    cost_us: u64,
}

impl PendingSectionUpload {
    fn new(
        packed: Arc<[PackedVertex]>,
        order: Arc<[u32]>,
        buckets: Option<DirectionBuckets>,
    ) -> Self {
        // `section_geometry` emits at most one output vertex per packed
        // vertex (16 B each) and six u32 indices per quad (~6 B per packed
        // vertex); the slack covers the section uniform/bind-group work.
        let upload_bytes = packed.len() * (std::mem::size_of::<PackedVertex>() + 6) + 256;
        let cost_us = 32 + upload_bytes as u64 / 256;
        Self {
            packed,
            order,
            buckets,
            upload_bytes,
            cost_us,
        }
    }

    fn matches(&self, packed: &[PackedVertex], order: &[u32]) -> bool {
        self.packed.as_ref() == packed && self.order.as_ref() == order
    }
}

/// Speculative `section_geometry` output produced by the mesh worker pool
/// for a still-pending upload. Matched back to the queue entry by
/// `Arc` pointer identity so a superseded mesh never installs stale
/// geometry.
struct CompletedSectionGeometry {
    packed: Arc<[PackedVertex]>,
    geometry: LiveSectionGeometry,
    opaque_bucket_ranges: Option<[(u32, u32); DIRECTION_BUCKETS]>,
}

/// One section's contribution to a [`RegionArena`]: where its repacked
/// vertices sit in the shared vertex arena plus the section-local opaque
/// index spans per direction bucket. No index data is retained — the
/// canonical [`QUAD_INDEX_PATTERN`] stream maps every quad positionally.
struct ArenaSlot {
    /// First vertex of the section inside the arena's vertex buffer.
    vertex_offset: u32,
    /// Vertices this slot occupies in the vertex buffer.
    vertex_count: u32,
    /// `(first, count)` of each bucket inside the section's local opaque
    /// index stream; the shared index buffer keeps these exact ranges —
    /// a slot's global span is `vertex_offset / 4 * 6 + (first, count)`.
    bucket_ranges: [(u32, u32); DIRECTION_BUCKETS],
}

/// A contiguous `(coord, index range)` record in one direction bucket of
/// a [`RegionArena`], expressed in *shared* quad-index space
/// ([`GpuRenderer::shared_quad_index_buffer`]); kept records sharing a
/// fade value merge into a single `draw_indexed` call at encode time.
struct ArenaDrawRecord {
    coord: SectionCoord,
    /// First index inside the shared quad index buffer.
    start: u32,
    /// End index inside the shared quad index buffer (exclusive).
    end: u32,
}

/// Shared vertex arena over one 8x4x8-section region (the
/// [`RenderRegions`] grid): 16-byte `PackedVertex` streams upload into
/// one vertex buffer through the persistent [`UploadStagingRing`] as
/// slot-range DMA copies (no CPU-side vertex copy is retained), and the
/// single global shared quad index buffer serves every direction-bucket
/// draw positionally — no per-region index buffer, index rebuild, or
/// index upload exists at all.
struct RegionArena {
    /// Vertices appended past the free list (the arena's high-water
    /// vertex count; holes live in `vertex_free`).
    vertex_high_water: u32,
    /// First-fit free list of `(offset, len)` vertex holes, coalesced —
    /// the same discipline `client-port`'s arena uses.
    vertex_free: Vec<(u32, u32)>,
    /// Resident sections in deterministic order.
    slots: BTreeMap<SectionCoord, ArenaSlot>,
    /// Serialized `(byte_offset, bytes)` vertex payloads staged by
    /// [`Self::insert`], keyed by coord so a superseded or removed slot
    /// drops its staged write. Entries present here are *not yet on the
    /// GPU*: their slots emit no draw records until the per-frame DMA cap
    /// admits the payload.
    pending_vertex_writes: BTreeMap<SectionCoord, (u64, Vec<u8>)>,
    /// Highest vertex offset whose bytes have reached `vertex_buffer`
    /// (the growth copy preserves exactly this committed prefix).
    flushed_vertex_len: u32,
    /// Per-bucket `(coord, start, end)` draw records in shared-index
    /// space, covering only slots whose vertex bytes are GPU-resident.
    bucket_records: [Vec<ArenaDrawRecord>; DIRECTION_BUCKETS],
    /// Vertex buffer contents changed since the last GPU write.
    vertex_dirty: bool,
    /// Slots or staged writes changed since the last record rebuild.
    index_dirty: bool,
    vertex_buffer: wgpu::Buffer,
    vertex_capacity: usize,
    /// Frames the arena stayed empty; the idle-slab reclaim grace in the
    /// active [`TransferTopologyBudget`] retires it.
    empty_frames: u32,
    /// `section` uniform holding the region-minimum world origin.
    _uniform: wgpu::Buffer,
    /// `{frame_ubo, region uniform}` on the shared frame layout — serves
    /// the simple, composite, and shadow-caster pipelines alike.
    bind_group: wgpu::BindGroup,
}

impl RegionArena {
    fn new(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        frame_layout: &wgpu::BindGroupLayout,
        frame_ubo: &wgpu::Buffer,
        origin: [i32; 3],
    ) -> Self {
        let vertex_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("region-arena-vertices"),
            size: 64,
            usage: wgpu::BufferUsages::VERTEX
                | wgpu::BufferUsages::COPY_DST
                | wgpu::BufferUsages::COPY_SRC,
            mapped_at_creation: false,
        });
        let uniform = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("region-arena-uniform"),
            size: std::mem::size_of::<SectionUniforms>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        queue.write_buffer(
            &uniform,
            0,
            bytemuck::bytes_of(&SectionUniforms {
                origin: [
                    origin[0] as f32 * EDGE as f32,
                    origin[1] as f32 * EDGE as f32,
                    origin[2] as f32 * EDGE as f32,
                ],
                _pad0: 0.0,
                biome_tint: [1.0; 3],
                biome_tint_strength: 0.0,
            }),
        );
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("region-arena-bind-group"),
            layout: frame_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: frame_ubo.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: uniform.as_entire_binding(),
                },
            ],
        });
        Self {
            vertex_high_water: 0,
            vertex_free: Vec::new(),
            slots: BTreeMap::new(),
            pending_vertex_writes: BTreeMap::new(),
            flushed_vertex_len: 0,
            bucket_records: Default::default(),
            vertex_dirty: false,
            index_dirty: false,
            vertex_buffer,
            vertex_capacity: 0,
            empty_frames: 0,
            _uniform: uniform,
            bind_group,
        }
    }

    /// Shared-index offset where the arena's quad `0` lives: the shared
    /// buffer's index `i` references vertex `i / 6 * 4 + pat[i % 6]`, so
    /// the arena occupies `0 .. index_extent()` of the shared stream.
    fn index_extent(&self) -> u32 {
        self.vertex_high_water / 4 * 6
    }

    /// Whether the committed record set is empty — every slot either
    /// absent or still waiting on the DMA cap.
    fn draw_empty(&self) -> bool {
        self.bucket_records.iter().all(|records| records.is_empty())
    }

    /// Free `offset..offset+len` back into the first-fit list.
    fn release_vertices(&mut self, offset: u32, len: u32) {
        if len == 0 {
            return;
        }
        self.vertex_free.push((offset, len));
        self.vertex_free.sort_unstable();
        // Coalesce adjacent holes so large slabs stay findable.
        let mut merged: Vec<(u32, u32)> = Vec::with_capacity(self.vertex_free.len());
        for &(off, l) in &self.vertex_free {
            match merged.last_mut() {
                Some((mo, ml)) if *mo + *ml == off => *ml += l,
                _ => merged.push((off, l)),
            }
        }
        self.vertex_free = merged;
    }

    /// First-fit allocation of `len` vertices, appending at the end when no
    /// hole is big enough. Returns the slot's vertex offset.
    fn alloc_vertices(&mut self, len: u32) -> u32 {
        if let Some(index) = self
            .vertex_free
            .iter()
            .position(|&(_, hole_len)| hole_len >= len)
        {
            let (offset, hole_len) = self.vertex_free.remove(index);
            if hole_len > len {
                self.vertex_free.push((offset + len, hole_len - len));
            }
            return offset;
        }
        let offset = self.vertex_high_water;
        self.vertex_high_water += len;
        offset
    }

    /// Insert or replace `coord`'s opaque stream: the 16-byte
    /// `PackedVertex` payload serializes straight into a staged partial
    /// write at the slot's byte offset (each vertex's `attributes` gains
    /// the section-in-region word in its free top byte). The slot draws
    /// through the shared quad index buffer — no local index stream is
    /// retained anywhere.
    fn insert(
        &mut self,
        coord: SectionCoord,
        vertices: &[PackedVertex],
        section_word: u32,
        bucket_ranges: [(u32, u32); DIRECTION_BUCKETS],
    ) {
        if let Some(old) = self.slots.remove(&coord) {
            self.release_vertices(old.vertex_offset, old.vertex_count);
        }
        let vertex_count = vertices.len() as u32;
        let vertex_offset = self.alloc_vertices(vertex_count);
        let mut bytes = Vec::with_capacity(std::mem::size_of_val(vertices));
        for v in vertices {
            bytes.extend_from_slice(&v.position.to_ne_bytes());
            bytes.extend_from_slice(&v.state_id.to_ne_bytes());
            // Clear — not OR — the top attribute byte: quad extents and
            // the corner index live there but no shader or post-pack
            // consumer reads them; the slot needs all 8 bits for the
            // section-in-region word.
            bytes.extend_from_slice(&((v.attributes & 0x00ff_ffff) | section_word).to_ne_bytes());
            bytes.extend_from_slice(&v.tint.to_ne_bytes());
        }
        self.pending_vertex_writes
            .insert(coord, (u64::from(vertex_offset) * 16, bytes));
        self.slots.insert(
            coord,
            ArenaSlot {
                vertex_offset,
                vertex_count,
                bucket_ranges,
            },
        );
        self.vertex_dirty = true;
        self.index_dirty = true;
        self.empty_frames = 0;
    }

    /// Drop `coord`'s slab if present; empties retire their region after
    /// the idle-slab reclaim grace.
    fn remove(&mut self, coord: SectionCoord) {
        if let Some(old) = self.slots.remove(&coord) {
            self.release_vertices(old.vertex_offset, old.vertex_count);
            self.index_dirty = true;
        }
        self.pending_vertex_writes.remove(&coord);
    }

    /// Recompute the per-bucket draw records from slot state only: a
    /// slot's bucket span in shared-index space is its local index range
    /// shifted by `vertex_offset / 4 * 6`. Slots whose vertex payload is
    /// still queued behind the DMA cap emit no records yet — drawing them
    /// would sample unwritten vertex data.
    fn rebuild_records(&mut self) {
        for records in &mut self.bucket_records {
            records.clear();
        }
        for (&coord, slot) in &self.slots {
            if self.pending_vertex_writes.contains_key(&coord) {
                continue;
            }
            let global = slot.vertex_offset / 4 * 6;
            for (bucket, &(first, count)) in slot.bucket_ranges.iter().enumerate() {
                if count == 0 {
                    continue;
                }
                self.bucket_records[bucket].push(ArenaDrawRecord {
                    coord,
                    start: global + first,
                    end: global + first + count,
                });
            }
        }
    }

    /// Visit the merged shared-index ranges covering every committed slot
    /// across all buckets — used by passes that draw the whole arena at
    /// once (shadow casters, the no-camera plain path). Consecutive
    /// records inside a bucket collapse into one range.
    fn for_each_live_range(&self, mut emit: impl FnMut(u32, u32)) {
        for records in &self.bucket_records {
            let mut run: Option<(u32, u32)> = None;
            for record in records {
                match run {
                    Some((start, end)) if end == record.start => {
                        run = Some((start, record.end));
                    }
                    _ => {
                        if let Some((start, end)) = run.take() {
                            emit(start, end);
                        }
                        run = Some((record.start, record.end));
                    }
                }
            }
            if let Some((start, end)) = run {
                emit(start, end);
            }
        }
    }

    /// Flush staged vertex inserts to the GPU through the persistent
    /// staging ring, honoring this frame's remaining DMA byte budget
    /// (`dma_remaining` is shared across all arenas so the per-frame cap
    /// is global). When the arena outgrows its buffer the committed slab
    /// is preserved by a `copy_buffer_to_buffer` in the caller's encoder —
    /// the CPU copies are long gone — and the buffer grows in
    /// `slab_vertex_bytes` granularity so reallocations amortize.
    /// Returns `(staged partial writes, buffers replaced)` — a replaced
    /// vertex buffer invalidates recorded `RenderBundle`s referencing it.
    fn flush_gpu(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        encoder: &mut wgpu::CommandEncoder,
        ring: &mut UploadStagingRing,
        dma_remaining: &mut u64,
        slab_vertex_bytes: usize,
    ) -> (u32, bool) {
        let mut buffers_replaced = false;
        let mut staged = 0u32;
        if self.vertex_dirty {
            let needed = self.vertex_high_water as usize;
            if needed > self.vertex_capacity {
                let live_bytes = u64::from(self.flushed_vertex_len) * 16;
                let slab = slab_vertex_bytes.max(64);
                let cap_bytes = (needed * 16).max(slab).next_multiple_of(slab);
                self.vertex_capacity = cap_bytes / 16;
                let new_buffer = device.create_buffer(&wgpu::BufferDescriptor {
                    label: Some("region-arena-vertices"),
                    size: cap_bytes as u64,
                    usage: wgpu::BufferUsages::VERTEX
                        | wgpu::BufferUsages::COPY_DST
                        | wgpu::BufferUsages::COPY_SRC,
                    mapped_at_creation: false,
                });
                if live_bytes > 0 {
                    // Preserve committed slot payloads before the staged
                    // writes land (the CPU copies are long gone).
                    encoder.copy_buffer_to_buffer(
                        &self.vertex_buffer,
                        0,
                        &new_buffer,
                        0,
                        live_bytes,
                    );
                }
                self.vertex_buffer = new_buffer;
                buffers_replaced = true;
            }
            let mut committed = Vec::new();
            for (&coord, (byte_offset, bytes)) in &self.pending_vertex_writes {
                let len = bytes.len() as u64;
                if len > *dma_remaining {
                    // Over this frame's DMA cap: the write (and every
                    // later one) defers to the next frame. The slot's
                    // draw records stay withheld — no partial garbage.
                    ring.staging_ring_dma_defers += 1;
                    break;
                }
                *dma_remaining -= len;
                ring.stage_copy(
                    device,
                    queue,
                    encoder,
                    &self.vertex_buffer,
                    *byte_offset,
                    bytes,
                );
                self.flushed_vertex_len =
                    self.flushed_vertex_len.max((byte_offset + len) as u32 / 16);
                committed.push(coord);
                staged += 1;
            }
            for coord in committed {
                self.pending_vertex_writes.remove(&coord);
            }
            if self.pending_vertex_writes.is_empty() {
                self.flushed_vertex_len = self.flushed_vertex_len.max(self.vertex_high_water);
                self.vertex_dirty = false;
            }
        }
        if self.index_dirty || staged > 0 {
            self.rebuild_records();
            // Uncommitted inserts still owe records — keep the flag set.
            self.index_dirty = !self.pending_vertex_writes.is_empty();
        }
        (staged, buffers_replaced)
    }
}

/// Rendered-frame intervals kept for the upload budget's P95 adaptation.
const UPLOAD_P95_WINDOW: usize = 120;

/// The non-visual halves of a [`ComputeStrategy`]: which
/// [`ChunkUploadBudget`] preset the memory topology warrants and how many
/// mesh-worker threads the worker split reserves for the foreground path.
fn strategy_upload_defaults(strategy: &ComputeStrategy) -> (ChunkUploadBudget, u32) {
    let budget = match strategy.memory {
        // UMA/shared-RAM hosts stream world data through the low-spec
        // budget: less PCIe headroom, smoother frames.
        MemoryTransferStrategy::UnifiedZeroCopy => ChunkUploadBudget::low_spec(),
        // Discrete VRAM behind a staged arena keeps the full default budget.
        MemoryTransferStrategy::DiscreteStagedArena => ChunkUploadBudget::new(),
    };
    (budget, strategy.workers.foreground_threads)
}

/// Transfer-layer sizing the memory topology warrants — staging-ring
/// slot geometry, arena slab growth granularity, empty-arena reclaim
/// grace and the per-frame DMA byte cap. Quality settings (render scale,
/// shadow quality) NEVER derive from this: the memory topology tunes how
/// bytes cross the bus, not how the frame looks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TransferTopologyBudget {
    /// Persistent staging-ring slots in flight.
    pub staging_ring_slots: u32,
    /// Bytes per staging slot.
    pub staging_slot_bytes: u64,
    /// Minimum growth unit for arena vertex buffers, in bytes.
    pub arena_slab_bytes: u64,
    /// Frames an emptied region arena is kept resident before reclaim.
    pub idle_slab_reclaim_frames: u32,
    /// Max staged bytes copied to device-local buffers per frame.
    pub frame_dma_cap_bytes: u64,
}

impl TransferTopologyBudget {
    /// Budget for `memory` on a host of `class`, following the design's
    /// sizing table: discrete VRAM streams 3x8 MiB staging slots with
    /// 32 MiB arena slabs, a 256-frame empty-arena grace and an 8 MiB
    /// per-frame DMA cap; unified memory drops to 3x2 MiB / 8 MiB /
    /// 60 / 3 MiB; a `Constrained` unified host drops further to
    /// 2x1 MiB / 4 MiB / 15 / 1 MiB.
    pub fn for_topology(memory: MemoryTransferStrategy, class: MemoryFootprintClass) -> Self {
        const MIB: u64 = 1024 * 1024;
        match (memory, class) {
            (MemoryTransferStrategy::UnifiedZeroCopy, MemoryFootprintClass::Constrained) => Self {
                staging_ring_slots: 2,
                staging_slot_bytes: MIB,
                arena_slab_bytes: 4 * MIB,
                idle_slab_reclaim_frames: 15,
                frame_dma_cap_bytes: MIB,
            },
            (MemoryTransferStrategy::UnifiedZeroCopy, _) => Self {
                staging_ring_slots: 3,
                staging_slot_bytes: 2 * MIB,
                arena_slab_bytes: 8 * MIB,
                idle_slab_reclaim_frames: 60,
                frame_dma_cap_bytes: 3 * MIB,
            },
            (MemoryTransferStrategy::DiscreteStagedArena, _) => Self {
                staging_ring_slots: 3,
                staging_slot_bytes: 8 * MIB,
                arena_slab_bytes: 32 * MIB,
                idle_slab_reclaim_frames: 256,
                frame_dma_cap_bytes: 8 * MIB,
            },
        }
    }
}

/// Persistent `MAP_WRITE | COPY_SRC` staging ring backing chunk uploads:
/// `slot_count` fixed-size staging buffers rotate through staged copies
/// so steady-state uploads issue zero `create_buffer` calls. Each
/// [`Self::stage_copy`] maps the next slot, memcpy's the payload, unmaps
/// and records a `copy_buffer_to_buffer` in the caller's encoder, so
/// device-local destinations only ever see DMA traffic.
pub struct UploadStagingRing {
    slots: Vec<wgpu::Buffer>,
    /// Slots already carrying a recorded copy in the caller's current
    /// submission batch — re-mapping one before its copy executes would
    /// overwrite the payload the pending copy still has to transfer.
    slot_used: Vec<bool>,
    next: usize,
    slot_bytes: u64,
    /// Slot buffers the ring has allocated (construction +
    /// reconfiguration only — constant in steady state).
    pub staging_ring_allocs: u64,
    /// Payloads staged through the ring, cumulative.
    pub staging_ring_uploads: u64,
    /// Staged writes deferred by the per-frame DMA cap, cumulative.
    pub staging_ring_dma_defers: u64,
    /// Map failures and same-batch slot wraps served by the direct
    /// `write_buffer` fallback, cumulative.
    pub staging_ring_fallbacks: u64,
}

impl UploadStagingRing {
    /// Build the ring with `budget` slot geometry.
    pub fn new(device: &wgpu::Device, budget: &TransferTopologyBudget) -> Self {
        let mut ring = Self {
            slots: Vec::new(),
            slot_used: Vec::new(),
            next: 0,
            slot_bytes: 0,
            staging_ring_allocs: 0,
            staging_ring_uploads: 0,
            staging_ring_dma_defers: 0,
            staging_ring_fallbacks: 0,
        };
        ring.rebuild_slots(
            device,
            budget.staging_ring_slots.max(1),
            budget.staging_slot_bytes.max(256),
        );
        ring
    }

    /// (Re)allocate every slot at `slot_count` x `slot_bytes`; resets the
    /// rotation. Called at construction, on topology-budget changes and
    /// on the rare oversize-payload growth.
    fn rebuild_slots(&mut self, device: &wgpu::Device, slot_count: u32, slot_bytes: u64) {
        self.slots = (0..slot_count)
            .map(|_| {
                device.create_buffer(&wgpu::BufferDescriptor {
                    label: Some("upload-staging-ring"),
                    size: slot_bytes,
                    usage: wgpu::BufferUsages::MAP_WRITE | wgpu::BufferUsages::COPY_SRC,
                    mapped_at_creation: false,
                })
            })
            .collect();
        self.slot_used = vec![false; slot_count as usize];
        self.next = 0;
        self.slot_bytes = slot_bytes;
        self.staging_ring_allocs += u64::from(slot_count);
    }

    /// Open a fresh submission batch: every slot's recorded copy from the
    /// previous batch has executed (its encoder was submitted), so all
    /// slots are reusable. Called once before the staged writes of a
    /// shared `CommandEncoder` — NOT between writes, since every copy
    /// recorded into that encoder still reads its slot at submit time.
    pub fn begin_batch(&mut self) {
        self.slot_used.fill(false);
        self.next = 0;
    }

    /// Re-size the ring when the topology budget changes; identical
    /// geometry keeps the existing slots (no reallocations).
    pub fn reconfigure(&mut self, device: &wgpu::Device, budget: &TransferTopologyBudget) {
        let slots = budget.staging_ring_slots.max(1);
        let bytes = budget.staging_slot_bytes.max(256);
        if slots as usize == self.slots.len() && bytes == self.slot_bytes {
            return;
        }
        self.rebuild_slots(device, slots, bytes);
    }

    /// Stage `bytes` for `dst` at `dst_offset` through the next free ring
    /// slot and record the DMA copy in `encoder`. Payloads larger than a
    /// slot grow the ring's slot size once; a failed map or a batch that
    /// out-numbers the ring falls back to `queue.write_buffer` (counted,
    /// never panics). A slot with a recorded-but-unsubmitted copy is
    /// never re-mapped: the pending copy still has to transfer the old
    /// payload, so bursts past `slot_count` writes take the queue path.
    pub fn stage_copy(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        encoder: &mut wgpu::CommandEncoder,
        dst: &wgpu::Buffer,
        dst_offset: u64,
        bytes: &[u8],
    ) {
        if bytes.is_empty() {
            return;
        }
        let len = bytes.len() as u64;
        if len > self.slot_bytes {
            let slots = self.slots.len().max(1) as u32;
            self.rebuild_slots(device, slots, len.next_power_of_two());
        }
        let index = (self.next..self.next + self.slots.len()).find_map(|i| {
            let index = i % self.slots.len();
            (!self.slot_used[index]).then_some(index)
        });
        let Some(index) = index else {
            // Every slot already carries an unsubmitted copy this batch:
            // the queue's own staging preserves each payload exactly.
            self.staging_ring_fallbacks += 1;
            queue.write_buffer(dst, dst_offset, bytes);
            return;
        };
        self.slot_used[index] = true;
        self.next = (index + 1) % self.slots.len();
        let slot = &self.slots[index];
        let slice = slot.slice(..len);
        let (tx, rx) = std::sync::mpsc::channel();
        slice.map_async(wgpu::MapMode::Write, move |result| {
            let _ = tx.send(result);
        });
        let mapped = device
            .poll(wgpu::PollType::wait_indefinitely())
            .map_err(|e| e.to_string())
            .and_then(|_| {
                rx.recv()
                    .map_err(|_| "map_async channel closed".to_string())
                    .and_then(|r| r.map_err(|e| format!("map_async: {e}")))
            });
        if mapped.is_err() {
            // A staging slot that will not map must never stall uploads:
            // fall back to the queue's own staging path and count it.
            self.staging_ring_fallbacks += 1;
            queue.write_buffer(dst, dst_offset, bytes);
            return;
        }
        if let Ok(mut view) = slice.get_mapped_range_mut() {
            view.copy_from_slice(bytes);
            drop(view);
            slot.unmap();
            encoder.copy_buffer_to_buffer(slot, 0, dst, dst_offset, len);
            self.staging_ring_uploads += 1;
        } else {
            self.staging_ring_fallbacks += 1;
            slot.unmap();
            queue.write_buffer(dst, dst_offset, bytes);
        }
    }
}

/// The `HardFloor1000Fps` upload-drain preset: at a 1000 fps pace a
/// frame's upload work must stay under ~150 µs and 128 KiB, with at
/// most two section installs — the rest of the queue spreads across
/// following frames so a rebuild burst never spikes a 1 ms frame.
pub fn hard_floor_upload_budget() -> ChunkUploadBudget {
    ChunkUploadBudget::with_limits(2, 128 * 1024, 150)
        .with_target_frame_us(1_000)
        .with_min_uploads_per_frame(1)
}

/// Six view-frustum planes in [`box_in_frustum`] form extracted from the
/// column-major `view_proj`: each entry is `[nx, ny, nz, d]` with the
/// inside half-space `dot(n, p) + d >= 0`. Covers NDC x/y in `[-1, 1]`
/// and z in `[0, 1]` — the wgpu clip convention `Camera::view_projection`
/// produces.
pub fn frustum_planes_from_view_proj(view_proj: &Mat4) -> [[f32; 4]; 6] {
    // clip = view_proj * [x, y, z, 1]; with `m[col][row]` storage the
    // coefficients of clip component `i` over world (x, y, z, 1) are
    // `[m[0][i], m[1][i], m[2][i], m[3][i]]`.
    let clip = |component: usize| -> [f32; 4] {
        [
            view_proj[0][component],
            view_proj[1][component],
            view_proj[2][component],
            view_proj[3][component],
        ]
    };
    let combine = |a: [f32; 4], b: [f32; 4], sa: f32, sb: f32| -> [f32; 4] {
        [
            sa * a[0] + sb * b[0],
            sa * a[1] + sb * b[1],
            sa * a[2] + sb * b[2],
            sa * a[3] + sb * b[3],
        ]
    };
    let (cx, cy, cz, cw) = (clip(0), clip(1), clip(2), clip(3));
    [
        combine(cx, cw, 1.0, 1.0),  // left:   clip_x + clip_w >= 0
        combine(cx, cw, -1.0, 1.0), // right:  clip_w - clip_x >= 0
        combine(cy, cw, 1.0, 1.0),  // bottom: clip_y + clip_w >= 0
        combine(cy, cw, -1.0, 1.0), // top:    clip_w - clip_y >= 0
        cz,                         // near:   clip_z >= 0
        combine(cz, cw, -1.0, 1.0), // far:    clip_w - clip_z >= 0
    ]
}

/// Transpose the column-major `Mat4` into the row-major `[[f32; 4]; 4]`
/// layout [`HiZPyramid::cull_world_aabb`] expects (`clip = vp * corner`).
pub(crate) fn view_proj_row_major(view_proj: &Mat4) -> [[f32; 4]; 4] {
    std::array::from_fn(|row| std::array::from_fn(|col| view_proj[col][row]))
}

/// Per-axis boundary sides in `box_facing_mask` order: `(axis, low, high)`
/// mapped from the same (West/East, Down/Up, North/South) pairing.
const FACING_AXIS_SIDES: [(usize, Direction, Direction); 3] = [
    (0, Direction::West, Direction::East),
    (1, Direction::Down, Direction::Up),
    (2, Direction::North, Direction::South),
];

/// The buckets a section may draw this frame. Starts from
/// [`bucket_draw_mask`] of the boundary sides facing the camera (which
/// always keeps the unassigned bucket), then re-adds both directions on
/// every axis the camera sits *inside* — an interior face can front-face
/// a camera embedded in the section's span, so only the strict far-side
/// opposite buckets drop. The skipped set is therefore exactly the
/// back-facing direction buckets the S3 audit targets (a camera strictly
/// west skips every `+X`/East bucket), never a face that could show.
fn section_draw_mask(min: [f32; 3], camera: [f32; 3]) -> u8 {
    let facing = box_facing_mask(min, camera);
    let mut draw = bucket_draw_mask(facing);
    for &(axis, low, high) in &FACING_AXIS_SIDES {
        if camera[axis] >= min[axis] && camera[axis] <= min[axis] + EDGE as f32 {
            draw |= low.bit() | high.bit();
        }
    }
    draw
}

/// Rewrite the section-local opaque vertex decode of `source` (one of
/// the built-in terrain WGSL modules) so the same 16-byte `PackedVertex`
/// stream draws region-arena geometry: the section-in-region offset
/// lives in `attributes` bits 24-31 (`sx | sy<<3 | sz<<5`, written by
/// [`RegionArena::insert`] via [`region_section_word`]) and is folded
/// into the integer part of the coordinate decode — `lx`/`lz` gain
/// `sec*16` block offsets and `ly` gains `sec*16384` fixed-point units,
/// keeping every `position` bit (including the 1/1024 `y` precision)
/// bit-exact. Only the first `terrain_*_vs` decode changes; the
/// translucent entry keeps its own section-local decode, so fragment
/// stages and every other entry stay byte-identical. Panics on anchor
/// drift: a silently wrong layout is worse than a loud build failure.
fn region_opaque_wgsl(source: &str) -> String {
    let out = source.replacen(
        "let lx = f32(in.packed_pos & 0x1fu);\n    let lz = f32((in.packed_pos >> 5u) & 0x1fu);\n    let ly = f32(in.packed_pos >> 10u) / 1024.0;",
        "let sec = in.attributes >> 24u;\n    let lx = f32(in.packed_pos & 0x1fu) + f32((sec & 0x7u) * 16u);\n    let lz = f32((in.packed_pos >> 5u) & 0x1fu) + f32((sec >> 5u) * 16u);\n    let ly = f32((in.packed_pos >> 10u) + ((sec >> 3u) & 0x3u) * 16384u) / 1024.0;",
        1,
    );
    for anchor in [
        "let sec = in.attributes >> 24u;",
        "section.origin + vec3<f32>(lx, ly, lz)",
    ] {
        assert!(out.contains(anchor), "region WGSL anchor missing: {anchor}");
    }
    out
}

/// Same transform for the shadow-caster module's `vs_terrain` entry:
/// `attributes` joins `packed_pos` so the section-in-region offset in
/// its top byte can shift the region-local position by whole sections.
fn region_caster_wgsl(caster_source: &str) -> String {
    let out = caster_source
        .replacen(
            "fn vs_terrain(@location(0) packed_pos: u32)",
            "fn vs_terrain(@location(0) packed_pos: u32, @location(2) attributes: u32)",
            1,
        )
        .replacen(
            "let local = vec3<f32>(f32(packed_pos & 31u),\n        f32(packed_pos >> 10u) / 1024.0, f32((packed_pos >> 5u) & 31u));",
            "let sec = attributes >> 24u;\n    let local = vec3<f32>(f32(packed_pos & 31u) + f32((sec & 7u) * 16u),\n        f32((packed_pos >> 10u) + ((sec >> 3u) & 3u) * 16384u) / 1024.0, f32((packed_pos >> 5u) & 31u) + f32(sec >> 5u) * 16.0);",
            1,
        );
    assert!(
        out.contains("fn vs_terrain(@location(0) packed_pos: u32, @location(2) attributes: u32)"),
        "region caster anchor missing"
    );
    out
}

/// Inputs that decide [`GpuRenderer::cull_live_sections`]: the camera's
/// containing section (the [`GraphCuller`] BFS root), the view-projection
/// bit pattern (frustum planes + Hi-Z view-match), the live-section
/// residency/graph revision, and the installed Hi-Z pyramid revision.
/// Identical inputs reuse the retained culled set verbatim.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct CullCacheKey {
    camera_section: (i32, i32, i32),
    view_proj_bits: [u32; 16],
    /// Sub-pixel rejection depends on the vertical focal length in
    /// pixels — an identical `view_proj` at a different render height
    /// culls differently, so the key carries the bit pattern.
    focal_y_bits: u32,
    world_revision: u64,
    hiz_revision: u64,
}

/// Inputs that decide whether the pre-recorded opaque-terrain
/// `RenderBundle` still applies: the camera section (facing-cull masks +
/// BFS root), the view-projection bits (frustum + Hi-Z view-match), the
/// residency/graph and Hi-Z revisions, the demo-scene revision, whether
/// any chunk fade is still animating (fade values bake into the recorded
/// draw uniforms), and the active shadow pipeline selection.
#[derive(Clone, Copy, Debug, PartialEq)]
struct TerrainBundleKey {
    camera_section: (i32, i32, i32),
    view_proj_bits: [u32; 16],
    world_revision: u64,
    hiz_revision: u64,
    scene_revision: u64,
    fades_active: bool,
    shadow_active: bool,
}

/// Inputs that decide whether the pre-recorded UI-quad `RenderBundle`
/// still applies: the draw's index count plus the vertex/index buffer
/// identities (capacities only change on reallocation).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct UiBundleKey {
    index_count: u32,
    vertex_capacity: u64,
    index_capacity: u64,
}

/// Encode the opaque-terrain portion of the composed world pass into any
/// [`wgpu::util::RenderEncoder`] — the live `RenderPass` or a
/// `RenderBundleEncoder` when the frame's opaque terrain is being cached
/// for steady-state replay. Writes its draw-uniform slots into `scratch`
/// (the persistent terrain slice at `draw_ubo` offsets `0..len`).
///
/// Returns `(skipped_buckets, arena_draws, arena_sections, arena_regions)`
/// for the renderer counters. Pipelines arrive pre-resolved for shadow
/// state; `scratch` is deliberately not `'a`-bound — pushing CPU-side
/// draw-uniform records must end when the call returns.
#[allow(clippy::too_many_arguments)]
fn encode_opaque_terrain<'a>(
    pass: &mut impl wgpu::util::RenderEncoder<'a>,
    opaque_pipeline: &'a wgpu::RenderPipeline,
    region_pipeline: &'a wgpu::RenderPipeline,
    draw_bg: &'a wgpu::BindGroup,
    scratch: &mut Vec<CompositeDrawUniforms>,
    scene: Option<&'a TerrainBuffers>,
    live_sections: &'a HashMap<SectionCoord, LiveTerrainSection>,
    region_arenas: &'a BTreeMap<[i32; 3], RegionArena>,
    shared_index: &'a wgpu::Buffer,
    culled_sections: &HashSet<SectionCoord>,
    camera_position: [f32; 3],
    chunk_fades: &ChunkFadeTracker,
    frame_bind_group: &'a wgpu::BindGroup,
    env_bind_group: &'a wgpu::BindGroup,
    shadow_receiver_bg: Option<&'a wgpu::BindGroup>,
    view_proj: &Mat4,
    // `include_arenas == false` when the GPU-driven indirect path
    // submits the arena sections — per-section fallback draws still
    // emit here.
    include_arenas: bool,
) -> (u64, u64, usize, usize) {
    // Bundle-recorded passes start with empty bind-group state, so the
    // environment (group 2) and optional shadow receiver (group 3) the
    // composite pipeline layouts expect are bound here once — the same
    // bind the inline pass relied on persisting from before the loop.
    pass.set_bind_group(2, Some(env_bind_group), &[]);
    if let Some(receiver) = shadow_receiver_bg {
        pass.set_bind_group(3, Some(receiver), &[]);
    }
    let mut skipped_buckets = 0u64;
    let mut arena_draws = 0u64;
    let mut arena_sections = 0usize;
    let mut arena_regions = 0usize;
    let identity: Mat4 = [
        [1.0, 0.0, 0.0, 0.0],
        [0.0, 1.0, 0.0, 0.0],
        [0.0, 0.0, 1.0, 0.0],
        [0.0, 0.0, 0.0, 1.0],
    ];
    let mut slot = |model: Mat4, fade: f32, alpha: f32| -> u32 {
        let index = scratch.len();
        scratch.push(CompositeDrawUniforms {
            mvp: mat4_mul(view_proj, &model),
            model,
            params: [fade, alpha, 0.0, 0.0],
            params2: [0.0; 4],
            _stride_pad: [0.0; 24],
        });
        index as u32 * DRAW_UNIFORM_STRIDE
    };
    // Opaque terrain: the demo scene, then every live section —
    // arena-resident sections as merged per-region bucket draws,
    // the rest through per-section buffers with the same facing
    // cull applied per bucket.
    pass.set_pipeline(opaque_pipeline);
    if let Some(scene) = scene {
        if scene.opaque_index_count > 0 {
            let off = slot(identity, 1.0, 1.0);
            pass.set_bind_group(0, Some(frame_bind_group), &[]);
            pass.set_bind_group(1, Some(draw_bg), &[off]);
            pass.set_vertex_buffer(0, Some(scene.opaque_vertices.slice(..)));
            pass.set_index_buffer(scene.opaque_indices.slice(..), wgpu::IndexFormat::Uint32);
            pass.draw_indexed(0..scene.opaque_index_count, 0, 0..1);
        }
    }
    // S4 shared region arena: one bind group + buffer pair per
    // 8x4x8 region; seven direction-bucket record sets merge into a few
    // draws per region, all indexed positionally into the one shared
    // canonical quad index buffer.
    if include_arenas && !region_arenas.is_empty() {
        pass.set_pipeline(region_pipeline);
        pass.set_index_buffer(shared_index.slice(..), wgpu::IndexFormat::Uint32);
        // Front-to-back: nearest regions rasterize first so hardware
        // Early-Z/Hi-Z rejects occluded far terrain before it shades.
        let mut arena_order: Vec<[i32; 3]> = region_arenas.keys().copied().collect();
        sort_regions_front_to_back(&mut arena_order, camera_position);
        for origin in arena_order {
            let arena = &region_arenas[&origin];
            if arena.draw_empty() {
                continue;
            }
            let mut bound = false;
            let mut emitted: HashSet<SectionCoord> = HashSet::new();
            for (bucket, records) in arena.bucket_records.iter().enumerate() {
                // Merged run of consecutive kept records sharing
                // one fade value: (start, end, fade_bits).
                let mut run: Option<(u32, u32, u32)> = None;
                for record in records {
                    let coord = &record.coord;
                    let culled = culled_sections.contains(coord);
                    let min = [
                        coord.0 as f32 * EDGE as f32,
                        coord.1 as f32 * EDGE as f32,
                        coord.2 as f32 * EDGE as f32,
                    ];
                    let kept =
                        !culled && section_draw_mask(min, camera_position) >> bucket & 1 != 0;
                    if !kept {
                        if !culled {
                            skipped_buckets += 1;
                        }
                        if let Some((start, end, fade_bits)) = run.take() {
                            let off = slot(identity, f32::from_bits(fade_bits), 1.0);
                            pass.set_bind_group(1, Some(draw_bg), &[off]);
                            pass.draw_indexed(start..end, 0, 0..1);
                            arena_draws += 1;
                        }
                        continue;
                    }
                    if !bound {
                        pass.set_bind_group(0, Some(&arena.bind_group), &[]);
                        pass.set_vertex_buffer(0, Some(arena.vertex_buffer.slice(..)));
                        bound = true;
                    }
                    emitted.insert(*coord);
                    let fade = chunk_fades.fade_alpha(coord.0, coord.2);
                    match run {
                        Some((start, end, bits))
                            if end == record.start && bits == fade.to_bits() =>
                        {
                            run = Some((start, record.end, bits));
                        }
                        _ => {
                            if let Some((start, end, bits)) = run.take() {
                                let off = slot(identity, f32::from_bits(bits), 1.0);
                                pass.set_bind_group(1, Some(draw_bg), &[off]);
                                pass.draw_indexed(start..end, 0, 0..1);
                                arena_draws += 1;
                            }
                            run = Some((record.start, record.end, fade.to_bits()));
                        }
                    }
                }
                if let Some((start, end, bits)) = run.take() {
                    let off = slot(identity, f32::from_bits(bits), 1.0);
                    pass.set_bind_group(1, Some(draw_bg), &[off]);
                    pass.draw_indexed(start..end, 0, 0..1);
                    arena_draws += 1;
                }
            }
            arena_sections += emitted.len();
            arena_regions += usize::from(bound);
        }
        // Restore the per-section pipeline for the fallback path.
        pass.set_pipeline(opaque_pipeline);
    }
    let mut per_section_index_bound = false;
    for (coord, section) in live_sections.iter() {
        if section.arena.is_some()
            || culled_sections.contains(coord)
            || section.opaque_index_total == 0
        {
            continue;
        }
        if !per_section_index_bound {
            // The fallback path draws the same canonical pattern — the
            // shared buffer serves local index space identically.
            pass.set_index_buffer(shared_index.slice(..), wgpu::IndexFormat::Uint32);
            per_section_index_bound = true;
        }
        let fade = chunk_fades.fade_alpha(coord.0, coord.2);
        let min = [
            coord.0 as f32 * EDGE as f32,
            coord.1 as f32 * EDGE as f32,
            coord.2 as f32 * EDGE as f32,
        ];
        match section.opaque_bucket_ranges {
            Some(ranges) => {
                let draw_mask = section_draw_mask(min, camera_position);
                for (bucket, &(start, count)) in ranges.iter().enumerate() {
                    if count == 0 {
                        continue;
                    }
                    if draw_mask >> bucket & 1 == 0 {
                        skipped_buckets += 1;
                        continue;
                    }
                    let off = slot(identity, fade, 1.0);
                    pass.set_bind_group(0, Some(&section.bind_group), &[]);
                    pass.set_bind_group(1, Some(draw_bg), &[off]);
                    pass.set_vertex_buffer(0, Some(section.terrain.opaque_vertices.slice(..)));
                    pass.draw_indexed(start..start + count, 0, 0..1);
                }
            }
            None => {
                let off = slot(identity, fade, 1.0);
                pass.set_bind_group(0, Some(&section.bind_group), &[]);
                pass.set_bind_group(1, Some(draw_bg), &[off]);
                pass.set_vertex_buffer(0, Some(section.terrain.opaque_vertices.slice(..)));
                pass.draw_indexed(0..section.terrain.opaque_index_count, 0, 0..1);
            }
        }
    }
    (skipped_buckets, arena_draws, arena_sections, arena_regions)
}

/// GPU state for the tessellated-quad UI overlay ([`UI_QUAD_WGSL`]):
/// screen-size uniform + static font atlas bound at group 0, grow-only
/// vertex/index buffers carrying `tessellate_draw_list` output.
struct UiQuadGpu {
    pipeline: wgpu::RenderPipeline,
    bind_group: wgpu::BindGroup,
    uniform: wgpu::Buffer,
    _font_texture: wgpu::Texture,
    vertices: wgpu::Buffer,
    indices: wgpu::Buffer,
    vertex_capacity: u64,
    index_capacity: u64,
    index_count: u32,
}

/// One uploaded title-branding texture (wordmark or patched
/// `MEGU EDITION` strip) plus its `UI_QUAD_WGSL`-compatible bind group:
/// the shared screen-size uniform, the texture view and the shared
/// nearest-neighbor UI sampler.
struct BrandingQuadGpu {
    /// Kept alive for its view; the bind group borrows no CPU pixels.
    _texture: wgpu::Texture,
    bind_group: wgpu::BindGroup,
}

/// GPU side of the runtime `MEGU EDITION` title branding: up to two
/// textured quads (wordmark + patched edition strip) drawn through the
/// `ui_quad` pipeline with their own bind groups, plus the yellow splash
/// tessellated against the shared font atlas. Everything is uploaded
/// once at boot — steady frames only rewrite the tiny vertex buffers
/// when the window size actually changes.
struct TitleBrandingGpu {
    /// Clone of the `ui_quad` layout (`0` uniform, `1` texture,
    /// `2` sampler) so each banner texture can own a bind group.
    layout: wgpu::BindGroupLayout,
    logo: Option<BrandingQuadGpu>,
    edition: Option<BrandingQuadGpu>,
    /// Fixed 8-vertex buffer: logo quad at vertices 0..4, edition at
    /// 4..8, both uv 0..1 with white vertex color.
    vertices: wgpu::Buffer,
    /// Fixed `[0,1,2,0,2,3, 4,5,6,4,6,7]` — logo draws `0..6`, edition
    /// `6..12`.
    indices: wgpu::Buffer,
    /// Draw gates set by [`GpuRenderer::update_title_branding`].
    logo_draw: bool,
    edition_draw: bool,
    /// Grow-only splash glyph buffers (tessellated `UiMesh` vertices —
    /// sampled against the font atlas through `ui_quad`'s bind group).
    splash_vertices: wgpu::Buffer,
    splash_indices: wgpu::Buffer,
    splash_vertex_capacity: u64,
    splash_index_capacity: u64,
    splash_index_count: u32,
    /// Last uploaded `(logo_rect, edition_rect, splash hash)` — skips
    /// buffer rewrites when the title layout did not move.
    mesh_key: Option<TitleBrandMeshKey>,
    /// Scratch converting `UiVertex` into the Pod vertex layout.
    vertex_scratch: Vec<UiQuadVertex>,
}

/// Layout key for [`TitleBrandingGpu::mesh_key`]: the two banner
/// destination rects plus a content hash of the splash glyph mesh.
type TitleBrandMeshKey = (Option<[f32; 4]>, Option<[f32; 4]>, u64);

/// Pod mirror of `megu_ui::draw::UiVertex` (pixel-space position,
/// atlas uv, straight-alpha color): 32 bytes, bound at locations 0-2.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Pod, Zeroable)]
struct UiQuadVertex {
    position: [f32; 2],
    uv: [f32; 2],
    color: [f32; 4],
}

const UI_QUAD_VERTEX_ATTRS: [wgpu::VertexAttribute; 3] = [
    wgpu::VertexAttribute {
        format: wgpu::VertexFormat::Float32x2,
        offset: 0,
        shader_location: 0,
    },
    wgpu::VertexAttribute {
        format: wgpu::VertexFormat::Float32x2,
        offset: 8,
        shader_location: 1,
    },
    wgpu::VertexAttribute {
        format: wgpu::VertexFormat::Float32x4,
        offset: 16,
        shader_location: 2,
    },
];

const UI_QUAD_VERTEX_LAYOUT: wgpu::VertexBufferLayout<'static> = wgpu::VertexBufferLayout {
    array_stride: std::mem::size_of::<UiQuadVertex>() as wgpu::BufferAddress,
    step_mode: wgpu::VertexStepMode::Vertex,
    attributes: &UI_QUAD_VERTEX_ATTRS,
};

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
    /// Region-vertex variant of `terrain_pipeline` for the no-camera
    /// plain path's arena draws.
    terrain_region_pipeline: wgpu::RenderPipeline,
    ui_pipeline: wgpu::RenderPipeline,
    ui_layout: wgpu::BindGroupLayout,
    ui_bind_group: Option<wgpu::BindGroup>,
    ui_texture: Option<wgpu::Texture>,
    ui_view: Option<wgpu::TextureView>,
    ui_sampler: wgpu::Sampler,
    ui_size: (u32, u32),
    /// GPU tessellated-quad UI overlay: `tessellate_draw_list` output
    /// uploaded as small vertex/index buffers instead of a per-frame
    /// `w*h*4` CPU raster + texture write.
    ui_quad: UiQuadGpu,
    /// Runtime `MEGU EDITION` title branding quads + splash, drawn in
    /// the UI overlay pass only on the title screen.
    title_branding: TitleBrandingGpu,
    /// Scratch converting `UiVertex` into the Pod vertex layout — retained
    /// so identical-size frames never reallocate.
    ui_quad_vertex_scratch: Vec<UiQuadVertex>,
    /// Composed frames that drew the UI via the GPU quad pipeline.
    pub ui_gpu_quad_frames: u64,
    /// Composed frames that also drew the `MEGU EDITION` title branding
    /// quads or splash — zero off the title screen.
    pub title_branding_frames: u64,
    /// Frames that wrote a CPU-rasterized RGBA framebuffer texture
    /// ([`Self::update_ui_framebuffer`]); the live window path keeps this
    /// at zero once the quad pipeline carries the UI.
    pub ui_cpu_raster_uploads: u64,
    depth_texture: Option<wgpu::Texture>,
    depth_view: Option<wgpu::TextureView>,
    depth_size: (u32, u32),
    /// Camera last submitted by the regular window/offscreen entry point.
    window_camera: Cell<Option<Camera>>,
    scene: Option<TerrainBuffers>,
    live_sections: HashMap<SectionCoord, LiveTerrainSection>,
    /// Per-frame admission controller for live-section GPU uploads, seeded
    /// from the adaptive compute strategy in [`Self::new`] and replaceable
    /// via [`Self::apply_compute_strategy`]/[`Self::set_upload_budget`].
    upload_budget: ChunkUploadBudget,
    /// Section meshes queued by [`Self::update_live_terrain`] awaiting
    /// budget admission on a rendered frame. `BTreeMap` keeps the drain
    /// order deterministic.
    pending_uploads: BTreeMap<SectionCoord, PendingSectionUpload>,
    /// Visibility graphs mirrored from the world on the last
    /// [`Self::update_live_terrain`] pass; feeds the composed frame's
    /// [`GraphCuller`] search. Empty until the first terrain sync.
    section_graphs: HashMap<SectionCoord, SectionGraph>,
    /// Hi-Z occlusion pyramid consulted after the graph/frustum test in
    /// the composed pass. The background worker pool rebuilds it from each
    /// frame's drawn occluder depth (`hiz_source_vp` records the
    /// view-projection it was splatted with); an embedder may also install
    /// one manually via [`Self::set_hiz_pyramid`], which pauses automatic
    /// updates until cleared.
    hiz_pyramid: Option<HiZPyramid>,
    /// View-projection the auto-built pyramid was splatted with. `Some`
    /// only for worker-produced pyramids — the pyramid is applied to
    /// frames with the identical matrix so a stale camera never
    /// false-culls. `None` marks an embedder-installed pyramid, which is
    /// applied unconditionally.
    hiz_source_vp: Option<Mat4>,
    /// An embedder-installed (`set_hiz_pyramid(Some)`) pyramid is present:
    /// automatic rebuild jobs pause and installs skip until `set_hiz_pyramid(None)`.
    hiz_manual: bool,
    /// Terrain residency changed since the last Hi-Z job — occluders moved.
    hiz_dirty: bool,
    /// Revision counter bumped whenever the installed-pyramid inputs to the
    /// cull pass change (`hiz_pyramid`/`hiz_source_vp`/`hiz_manual`).
    hiz_revision: u64,
    /// Revision counter bumped whenever live-section residency or the
    /// mirrored visibility-graph set changes; part of the cull cache key.
    cull_world_revision: u64,
    /// Order-independent stamp of `section_graphs` keys from the last
    /// [`Self::update_live_terrain`] — detects graph-only world changes.
    cull_graphs_stamp: u64,
    /// Retained culled-section set: the cached result while
    /// `cull_cache_key` is `Some`, and the zero-alloc scratch the next
    /// recompute refills. [`Self::cull_live_sections`] hands it to the
    /// caller via `mem::take`; `encode_composed_world` returns it.
    cull_result: HashSet<SectionCoord>,
    /// Retained scratch for the graph-culler visible set — cleared and
    /// refilled on cache misses instead of a fresh `HashSet` each frame.
    cull_graph_scratch: HashSet<SectionCoord>,
    /// Inputs that produced `cull_result`: unchanged inputs reuse the
    /// cached set directly (`cull_cache_hits += 1`, ~0 µs).
    cull_cache_key: Option<CullCacheKey>,
    /// Hi-Z culled count associated with `cull_result`.
    cull_hiz_count: u64,
    /// Sub-pixel fast-reject count associated with `cull_result`.
    cull_subpixel_count: u64,
    /// Composed frames that reused the cached cull result verbatim.
    pub cull_cache_hits: u64,
    /// Instanced `draw_indexed` calls the most recent composed pass
    /// emitted for `InstancedPartBatch` streams (per-pass value, not
    /// cumulative): 2,000 same-kind parts still cost 1 draw.
    pub instanced_kinetic_draws: u64,
    /// Shared vertex arenas over `RenderRegions`' 8x4x8 grid feeding
    /// the composed opaque path's merged bucket draws.
    region_arenas: BTreeMap<[i32; 3], RegionArena>,
    /// Region-arena opaque path toggle; `false` falls back to per-section
    /// opaque buffers (A/B pixel comparison and bring-up).
    arena_enabled: bool,
    /// The one global opaque index stream: [`QUAD_INDEX_PATTERN`]
    /// repeated with a +4 stride per quad. Every opaque quad draw —
    /// per-section and arena-resident alike — indexes it positionally
    /// (`first_index = first_quad * 6`), so no per-section or per-region
    /// index buffers exist and 100% of opaque quads share this stream.
    shared_quad_index_buffer: wgpu::Buffer,
    /// Indices currently resident in `shared_quad_index_buffer`; grows
    /// geometrically to cover the largest referenced extent.
    shared_quad_index_capacity: u32,
    /// Persistent `MAP_WRITE | COPY_SRC` staging ring all chunk/arena
    /// uploads copy through — zero `create_buffer` calls in steady state.
    staging_ring: UploadStagingRing,
    /// GPU compute Hi-Z pyramid + section culler (audit V4), lazily
    /// created at the frame depth resolution.
    gpu_hiz: Option<GpuHizCuller>,
    /// GPU section-cull dispatches run through [`GpuHizCuller`].
    pub gpu_hiz_cull_calls: u64,
    /// Sections the GPU culler proved invisible, cumulative.
    pub gpu_hiz_culled: u64,
    /// Opt-in GPU-driven opaque arena submission (audit
    /// `GPU-INDIRECT-AND-OPT-WIRE`): [`LiveSession`](crate::live_session::LiveSession)
    /// enables it once the adapter proves `INDIRECT_EXECUTION`; the
    /// composed pass then culls arena sections into an indirect args
    /// buffer and submits `*_indexed_indirect` with zero CPU readback.
    pub gpu_indirect_enabled: bool,
    /// Whether the adapter reports `DownlevelFlags::INDIRECT_EXECUTION`
    /// — required for `draw_indexed_indirect`/`multi_draw_indexed_indirect`.
    indirect_execution_supported: bool,
    /// Grow-only indirect buffers, allocated on the first enabled frame.
    gpu_indirect: Option<GpuIndirectBuffers>,
    /// Morton Z-order tie-break inside each camera-distance ring (the
    /// `chunk_mesh` domain plan's `use_morton_layout` projection).
    pub morton_order_enabled: bool,
    /// View-projection the live GPU pyramid was last built under: Hi-Z
    /// rejects stay armed only while it matches the active camera, so a
    /// camera cut can never false-cull through stale depth.
    gpu_hiz_frame_vp: Option<Mat4>,
    /// Composed frames whose opaque pass submitted GPU indirect draws.
    pub gpu_indirect_draw_passes: u64,
    /// Section records submitted to the GPU cull, cumulative.
    pub gpu_indirect_sections_submitted: u64,
    /// Retained pack scratch for [`Self::encode_gpu_indirect_cull`]'s
    /// `SectionGpuRecord` list — `mem::take`n, refilled and returned each
    /// indirect frame so steady frames allocate nothing.
    gpu_indirect_record_scratch: Vec<SectionGpuRecord>,
    /// Retained pack scratch for the matching record coords.
    gpu_indirect_coord_scratch: Vec<SectionCoord>,
    /// Retained scratch for the front-to-back region origin list.
    gpu_indirect_origin_scratch: Vec<[i32; 3]>,
    /// `queue.write_buffer` calls against the indirect
    /// `SectionGpuRecord` buffer, cumulative — one per coalesced dirty
    /// span, zero on frames whose record pack is unchanged.
    pub gpu_indirect_record_uploads: u64,
    /// `SectionGpuRecord` bytes uploaded to the indirect buffer,
    /// cumulative.
    pub gpu_indirect_record_bytes_uploaded: u64,
    /// Indirect frames whose record diff was empty — the record buffer
    /// upload was skipped entirely (0 bytes, 0 `write_buffer` calls).
    pub gpu_indirect_record_uploads_skipped: u64,
    /// Frame-depth pyramid builds recorded after composed world passes.
    pub gpu_hiz_pyramid_builds: u64,
    /// Real frame-P95 observations fed to the upload budget through
    /// [`Self::observe_frame_p95_ms`], cumulative.
    pub upload_budget_p95_observations: u64,
    /// Transfer-layer sizing from the memory topology
    /// ([`MemoryTransferStrategy`] x [`MemoryFootprintClass`]) — never a
    /// quality dial.
    transfer_budget: TransferTopologyBudget,
    /// Host RAM class feeding [`TransferTopologyBudget::for_topology`];
    /// seeded from the detected profile at construction.
    memory_footprint_class: MemoryFootprintClass,
    /// Payloads staged through [`Self::staging_ring`], cumulative.
    pub staging_ring_uploads: u64,
    /// Staging slot buffers allocated, cumulative (warm-up +
    /// reconfiguration only — constant in steady state).
    pub staging_ring_allocs: u64,
    /// Staged writes the per-frame DMA cap deferred, cumulative.
    pub staging_ring_dma_defers: u64,
    /// Shared 16-byte vertex buffer for arena-resident sections (their
    /// opaque geometry lives in the region arena).
    empty_vertex_buffer: wgpu::Buffer,
    /// Shared 16-byte index buffer for arena-resident sections.
    empty_index_buffer: wgpu::Buffer,
    /// Bounded worker pool speculatively building section geometry and
    /// the live Hi-Z pyramid (`mesh_worker_threads` sized).
    mesh_pool: MeshJobPool,
    /// Worker-built geometry for uploads still pending admission, keyed
    /// by section; stale entries (superseded packed payloads) are dropped
    /// at drain time.
    completed_geometry: HashMap<SectionCoord, CompletedSectionGeometry>,
    /// Rendered-frame intervals (ms) feeding [`ChunkUploadBudget::adapt`].
    upload_frame_ms: VecDeque<f64>,
    /// Instant of the previous rendered frame for interval measurement.
    last_render_instant: Option<Instant>,
    /// Mesh-worker concurrency hint from the last applied compute
    /// strategy: foreground threads reserved for the meshing critical path.
    pub mesh_worker_threads: u32,
    /// Times [`Self::apply_compute_strategy`] configured the renderer.
    compute_strategy_apply_count: u64,
    /// Live-section uploads admitted through the budget, cumulative.
    pub admitted_uploads: u64,
    /// Upload admissions the budget rejected and deferred, cumulative.
    pub deferred_uploads: u64,
    /// Sections skipped by the composed pass's visibility checks on the
    /// most recent frame.
    pub last_frame_culled_sections: usize,
    /// Lifetime total of sections skipped before GPU draw submission.
    pub culled_sections: u64,
    /// Section-geometry jobs handed to the background pool, cumulative.
    pub bg_mesh_jobs_submitted: u64,
    /// Worker results drained by the render thread, cumulative — proves
    /// the background mesh path actually executed.
    pub bg_mesh_jobs_completed: u64,
    /// Live Hi-Z pyramids installed from worker splats, cumulative.
    pub hiz_frames_updated: u64,
    /// Sections culled by the Hi-Z pyramid, cumulative.
    pub hiz_culled_sections: u64,
    /// Sections dropped by the O(1) sub-pixel fast reject
    /// (`is_subpixel_section` → `projected_sphere_diameter_px`),
    /// cumulative — proves the closed-form projection runs live.
    pub subpixel_culled_sections: u64,
    /// Non-empty (section, direction-bucket) draws skipped by the facing
    /// mask before GPU submission, cumulative.
    pub direction_buckets_skipped: u64,
    /// Merged `draw_indexed` calls the region-arena opaque path emitted,
    /// cumulative.
    pub region_arena_draws: u64,
    /// Group-0 bind switches the arena path saved relative to one bind
    /// per drawn section, cumulative.
    pub bind_group_switches_saved: u64,
    /// Pre-recorded opaque-terrain bundle and the inputs that produced
    /// it — identical camera/cull/fade/shadow state replays it in a
    /// single `execute_bundles` call instead of the Rust draw loop.
    terrain_bundle: Option<wgpu::RenderBundle>,
    terrain_bundle_key: Option<TerrainBundleKey>,
    /// Counter deltas captured while recording `terrain_bundle`, re-applied
    /// on each replay so the totals keep reflecting what the pass drew.
    terrain_bundle_stats: (u64, u64, usize, usize),
    /// Composed frames that replayed the cached opaque-terrain bundle.
    pub terrain_bundle_hits: u64,
    /// Composed frames that re-recorded the opaque-terrain bundle.
    pub terrain_bundle_records: u64,
    /// Pre-recorded UI-quad bundle and the buffer/count key it was
    /// recorded with — unchanged UI content replays it verbatim.
    ui_bundle: Option<wgpu::RenderBundle>,
    ui_bundle_key: Option<UiBundleKey>,
    /// Frames the UI overlay replayed the cached quad bundle.
    pub ui_bundle_hits: u64,
    /// `queue.write_buffer` calls skipped because the byte payload was
    /// identical to what the buffer already holds (frame/env/lights
    /// UBOs and the steady-state draw UBO).
    pub ubo_writes_skipped: u64,
    /// Slot-range partial vertex uploads through the region arenas,
    /// cumulative — replaces the old whole-buffer re-uploads.
    pub region_partial_uploads: u64,
    /// CPU-side opaque vertex bytes no longer retained after upload
    /// (region slots serialize straight into staged partial writes, and
    /// per-section opaque geometry drops after buffer creation).
    pub cpu_opaque_bytes_freed: u64,
    /// Byte mirrors of the last frame/env/lights uniform uploads for
    /// the skip-on-unchanged dedup.
    last_frame_ubo_bytes: Vec<u8>,
    last_env_ubo_bytes: Vec<u8>,
    last_lights_ubo_bytes: Vec<u8>,
    /// Section uniform + bind group shared by sections the composed pass
    /// never reaches (arena-resident opaque with no translucent stream):
    /// installing them costs zero GPU objects.
    empty_section_uniform: wgpu::Buffer,
    empty_bind_group: wgpu::BindGroup,
    /// Bumped by [`Self::set_terrain`]; part of the terrain bundle key.
    scene_revision: u64,
    /// Player world position for the upload drain's Priority-0 path.
    player_position: [f32; 3],
    composite: CompositeGpu,
    debug_line_pipeline: wgpu::RenderPipeline,
    debug_lines: Option<(wgpu::Buffer, u32)>,
    entity_ber: Option<DynamicGeometryRing>,
    particles: Option<DynamicGeometryRing>,
    particle_alphas: Vec<f32>,
    /// Instanced billboard path: per-frame `ParticleInstance` records
    /// staged into `particle_instances` — replaces the CPU quad staging
    /// whenever `particle_instancing` is on.
    particle_instances: Option<ParticleInstanceRing>,
    particle_instance_scratch: Vec<ParticleInstance>,
    /// `true` when the billboard pipeline exists and instancing is
    /// enabled — [`Self::set_particle_instancing_enabled`] can force the
    /// CPU quad path for parity testing or pathological adapters.
    particle_instancing: bool,
    /// `ParticleInstance` records written to the billboard ring
    /// (cumulative — the per-frame bound is `MAX_DYNAMIC_QUADS`).
    pub particle_instance_uploads: u64,
    /// Input quads/particles dropped by the `MAX_DYNAMIC_QUADS`
    /// per-frame upload bound (cumulative, both paths).
    pub particle_instances_capped: u64,
    /// `set_particle_*` calls routed through the CPU quad path — forced
    /// fallback plus quads outside the instanced bilinear model.
    pub particle_cpu_fallbacks: u64,
    /// Steady frames served from already-allocated dynamic-geometry
    /// ring buffers (`stage_dynamic_geometry` reuse hits).
    pub dynamic_ring_reuses: u64,
    /// Vertex/index buffer objects the dynamic rings have created
    /// (warm-up + capacity growth only — constant in steady state).
    pub dynamic_buffer_allocs: u64,
    subworld_gpu: SubWorldGpuCache,
    scaled_target: Option<ScaledTarget>,
    /// Edge-adaptive 5-tap spatial sharpen pass used for the
    /// `render_scale < Native100` upscale (created lazily on first use).
    sharpen: Option<SharpenGpu>,
    /// Dedicated `MEGU_WATER_SURFACE_WGSL` pass — created the first frame
    /// any live section carries decoded water geometry, `None` otherwise.
    water: Option<WaterGpu>,
    /// `MEGU_TERRAIN_LIT_WGSL` receiver path: created lazily on the first
    /// [`Self::upload_lit_terrain`] call (the decoded-vertex stream is for
    /// explicit lit uploads, not the packed section path).
    lit_terrain: Option<LitTerrainGpu>,
    /// Epoch of the last `ShadowGpu` creation — `lit_terrain`'s group-0
    /// bind group holds depth-texture refs that go stale on rebuild.
    shadow_gpu_epoch: u64,
    /// Pending stained-glass casters the lazy `MEGU_STAINED_GLASS_SHADOW_COLOR_WGSL`
    /// pass uploads on the next shadow encode (`None`/`empty` = zero cost).
    pending_stained_casters: Vec<StainedCasterVertex>,
    /// Sections whose latest mesh data completed VRAM upload — the window
    /// loop drains this and calls
    /// `WorldState::release_uploaded_section_cpu_copies` on each.
    uploaded_section_coords: Vec<SectionCoord>,
    /// Bumped by `upload_block_atlas` — the cutout caster's atlas bind
    /// group holds texture/storage-buffer refs that go stale on reload.
    atlas_epoch: u64,
    /// The negotiated built-in shadow quality (`negotiate_builtin_shadow_quality`
    /// output); `Off` unless [`Self::set_shadow_quality`] ran.
    pub shadow_quality: BuiltinShadowQuality,
    /// Render-scale preset for the composed world pass (`Native100` = the
    /// world and the UI overlay both render at target resolution).
    pub render_scale: RenderScalePreset,
    shadow_cache: DirtyOnlyShadowCache,
    shadow: Option<ShadowGpu>,
    shadow_active: bool,
    last_shadow_camera: Option<[f32; 9]>,
    /// Zero-alloc FNV-1a digest of the caster set the shadow pass last saw
    /// ([`subworld_caster_digest`]); `EMPTY_CASTER_DIGEST` until a sync
    /// records real caster state.
    last_shadow_casters: u64,
    /// Epoch bumped whenever `subworld_gpu.sync` lands real geometry or
    /// instance changes — recorded cascade bundles fold it into their
    /// key because buffer replacement leaves recorded slices stale even
    /// when counts/transforms match.
    subworld_gpu_epoch: u64,
    /// Number of shadow depth passes completed (one per cache re-render).
    pub shadow_rerender_count: u64,
    /// Number of composed frames that reused an existing shadow texture.
    pub shadow_reuse_count: u64,
    /// Decision the [`DirtyOnlyShadowCache`] returned for the most recent
    /// composed frame (`FirstFrame` until one renders).
    pub last_shadow_decision: ShadowCacheDecision,
    /// Rotation-invariant shadow state built for the current frame
    /// ([`build_stable_shadow_uniforms`] — light orthos depend on camera
    /// position, sun, and clip planes only, never yaw/pitch).
    frame_stable_shadow: Option<StableShadow>,
    /// Light-space ortho volumes each cascade's depth map was last
    /// rendered with — sampling matrices and the per-cascade caster AABB
    /// cull both come from these, not the live frame's values.
    rendered_orthos: [CascadeOrtho; 3],
    /// Light view-proj matrices matching the rendered depth maps, per
    /// cascade; written into the receiver uniform every frame so spread
    /// updates keep unrendered cascades sampling their own maps.
    rendered_lvp: [[[f32; 4]; 4]; 3],
    /// Cascades whose ortho drifted since their last render — bit per
    /// layer, drained one per frame on non-emergency updates.
    pending_cascade_layers: u8,
    /// Composed frames where a pure yaw/pitch camera rotation (static
    /// position, sun, world) reused the shadow map — the
    /// rotation-invariant orthos make the reuse bit-exact.
    pub shadow_rotation_reuses: u64,
    /// Caster draws skipped by the per-cascade light-space AABB test
    /// (region arenas and loose sections outside the cascade volume).
    pub shadow_regions_culled: u64,
    /// Fullscreen depth-equal sky pass drawn after opaque terrain.
    sky: SkyGpu,
    /// GPU paradigm from host hardware detection — drives depth-store
    /// policy (TBDR discard) for the composed pass.
    gpu_paradigm: GpuComputeParadigm,
    /// Composed frames whose depth attachment was `Discard`ed after use
    /// (TBDR on-chip tile depth, no writeback).
    pub tbdr_depth_discards: u64,
    /// The active shaderpack hook verdict (or `None` for built-in).
    pub shaderpack: ShaderpackAttempt,
    /// Pack-supplied `gbuffers_*` pipelines for Create sub-worlds and
    /// kinetic parts (`None` = built-in `COMPOSITE_LIT_WGSL` path).
    pack_subworld: Option<PackSubworldGpu>,
    /// Latest notice line the HUD/diagnostics can display.
    pub notice: Option<String>,
}

pub const MAX_DEBUG_LINES: usize = 65_536;
const MAX_DYNAMIC_QUADS: usize = 16_384;

/// Grow-only persistent vertex/index buffer pair for per-frame dynamic
/// geometry (entities, block-entity renders, particles). After the
/// warm-up allocation every steady update is a `queue.write_buffer` into
/// the same buffers — zero `create_buffer` calls in steady state.
/// Capacity only ever grows; an empty upload keeps the buffers and just
/// zeroes the draw count so the next non-empty frame still reuses them.
struct DynamicGeometryRing {
    vertices: wgpu::Buffer,
    indices: wgpu::Buffer,
    vertex_capacity: usize,
    index_capacity: usize,
    /// Index count of the currently staged frame (`0` = nothing drawn).
    index_count: u32,
}

fn dynamic_geometry_buffers(
    ctx: &GpuDevice,
    vertex_capacity: usize,
    index_capacity: usize,
) -> (wgpu::Buffer, wgpu::Buffer) {
    let vertices = ctx.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("dynamic-geometry-vertices"),
        size: (vertex_capacity * std::mem::size_of::<TerrainVertex>()).max(64) as u64,
        usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    let indices = ctx.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("dynamic-geometry-indices"),
        size: (index_capacity * std::mem::size_of::<u32>()).max(64) as u64,
        usage: wgpu::BufferUsages::INDEX | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    (vertices, indices)
}

/// Upload `vertices`/`indices` into `ring`, allocating on first use and
/// re-allocating only when the payload outgrows capacity. Returns
/// `(buffer_allocs, ring_reuses)` for the renderer counters: warm-up and
/// growth are allocs; a same-buffer upload is a reuse.
fn stage_dynamic_geometry(
    ctx: &GpuDevice,
    ring: &mut Option<DynamicGeometryRing>,
    vertices: &[TerrainVertex],
    indices: &[u32],
) -> (u64, u64) {
    if indices.is_empty() {
        if let Some(r) = ring.as_mut() {
            r.index_count = 0;
        }
        return (0, 0);
    }
    let allocated = match ring.as_mut() {
        Some(r) if r.vertex_capacity >= vertices.len() && r.index_capacity >= indices.len() => {
            false
        }
        slot => {
            let vertex_capacity = vertices
                .len()
                .max(slot.as_ref().map_or(256, |r| r.vertex_capacity * 2));
            let index_capacity = indices
                .len()
                .max(slot.map_or(384, |r| r.index_capacity * 2));
            let (vertex_buffer, index_buffer) =
                dynamic_geometry_buffers(ctx, vertex_capacity, index_capacity);
            *ring = Some(DynamicGeometryRing {
                vertices: vertex_buffer,
                indices: index_buffer,
                vertex_capacity,
                index_capacity,
                index_count: 0,
            });
            true
        }
    };
    let r = ring.as_mut().expect("ring ensured above");
    ctx.queue
        .write_buffer(&r.vertices, 0, bytemuck::cast_slice(vertices));
    ctx.queue
        .write_buffer(&r.indices, 0, bytemuck::cast_slice(indices));
    r.index_count = indices.len() as u32;
    if allocated {
        (1, 0)
    } else {
        (0, 1)
    }
}

/// FNV-1a digest of the staged dynamic geometry draw state — part of the
/// per-cascade shadow bundle key. Capacities track buffer replacement:
/// a recorded bundle replays the live buffer handle, so a reallocated
/// ring must re-record.
fn dynamic_geometry_digest(ring: &Option<DynamicGeometryRing>) -> u64 {
    let mut hash = 0u64;
    if let Some(r) = ring {
        hash ^= u64::from(r.index_count) << 1;
        hash = hash
            .wrapping_mul(0x100_0000_01b3)
            .wrapping_add(r.vertex_capacity as u64);
        hash = hash
            .wrapping_mul(0x100_0000_01b3)
            .wrapping_add(r.index_capacity as u64);
    }
    hash
}

/// Grow-only per-instance buffer for GPU-expanded particle billboards —
/// the instanced counterpart of [`DynamicGeometryRing`]. One
/// `queue.write_buffer` of [`ParticleInstance`] records per frame; a
/// single indexed draw expands the static unit quad per instance.
struct ParticleInstanceRing {
    buffer: wgpu::Buffer,
    capacity: usize,
    /// Instance count staged for the current frame (`0` = nothing drawn).
    count: u32,
}

/// Upload `instances` into the billboard ring under the same
/// allocate-once/write-in-place contract as [`stage_dynamic_geometry`].
/// Returns `(buffer_allocs, ring_reuses)` for the shared dynamic-geometry
/// counters.
fn stage_particle_instances(
    ctx: &GpuDevice,
    ring: &mut Option<ParticleInstanceRing>,
    instances: &[ParticleInstance],
) -> (u64, u64) {
    if instances.is_empty() {
        if let Some(r) = ring.as_mut() {
            r.count = 0;
        }
        return (0, 0);
    }
    let allocated = match ring.as_mut() {
        Some(r) if r.capacity >= instances.len() => false,
        slot => {
            let capacity = instances
                .len()
                .max(slot.as_ref().map_or(256, |r| r.capacity * 2));
            let buffer = ctx.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("particle-billboard-instances"),
                size: (capacity * std::mem::size_of::<ParticleInstance>()) as u64,
                usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            *ring = Some(ParticleInstanceRing {
                buffer,
                capacity,
                count: 0,
            });
            true
        }
    };
    let r = ring.as_mut().expect("ring ensured above");
    ctx.queue
        .write_buffer(&r.buffer, 0, bytemuck::cast_slice(instances));
    r.count = instances.len() as u32;
    if allocated {
        (1, 0)
    } else {
        (0, 1)
    }
}

/// Digest of the staged billboard instance ring — the instanced twin of
/// [`dynamic_geometry_digest`] for the per-cascade shadow bundle key.
fn particle_instance_digest(ring: &Option<ParticleInstanceRing>) -> u64 {
    let mut hash = 0u64;
    if let Some(r) = ring {
        hash ^= u64::from(r.count) << 1;
        hash = hash
            .wrapping_mul(0x100_0000_01b3)
            .wrapping_add(r.capacity as u64);
    }
    hash
}

fn append_dynamic_quad(
    vertices: &mut Vec<TerrainVertex>,
    indices: &mut Vec<u32>,
    positions: [[f32; 3]; 4],
    normal: [f32; 3],
    uv: [[f32; 2]; 4],
    tint: [f32; 3],
) -> bool {
    if vertices.len() / 4 >= MAX_DYNAMIC_QUADS
        || !positions
            .iter()
            .flatten()
            .chain(normal.iter())
            .chain(uv.iter().flatten())
            .chain(tint.iter())
            .all(|v| v.is_finite())
    {
        return false;
    }
    let base = vertices.len() as u32;
    for corner in 0..4 {
        vertices.push(TerrainVertex {
            position: positions[corner],
            normal,
            albedo: tint.map(|v| v.clamp(0.0, 1.0)),
            uv: uv[corner],
            atlas_index: u32::MAX,
        });
    }
    indices.extend_from_slice(&[base, base + 1, base + 2, base, base + 2, base + 3]);
    true
}

/// GPU state for instanced particle billboards: the alpha-blended lit
/// pipeline (same blend/depth contract as `lit_alpha_pipeline`), the
/// static unit-quad vertex/index pair the vertex shader expands with
/// each instance's billboard axes, and nothing else — per-particle data
/// lives in the renderer's grow-only [`ParticleInstanceRing`].
struct ParticleBillboardGpu {
    pipeline: wgpu::RenderPipeline,
    quad_vertices: wgpu::Buffer,
    quad_indices: wgpu::Buffer,
}

/// Minimum device limits the instanced billboard path needs beyond what
/// every wgpu backend already guarantees: two vertex streams (unit quad
/// + instance ring), eight attribute slots, and a 76-byte stride.
fn particle_instancing_supported(device: &wgpu::Device) -> bool {
    let limits = device.limits();
    limits.max_vertex_buffers >= 2
        && limits.max_vertex_attributes >= 8
        && limits.max_vertex_buffer_array_stride
            >= std::mem::size_of::<ParticleInstance>() as u32
}

/// GPU state for the composed pass: the fade/light-aware terrain pipeline,
/// the lit `TerrainVertex` pipelines (opaque + alpha), the instanced-part
/// pipeline, the dynamic-offset draw-uniform pool, and the shared
/// environment/lights uniforms. All pipelines share group layouts so one
/// pass binds every draw.
struct CompositeGpu {
    terrain_opaque_pipeline: wgpu::RenderPipeline,
    /// `terrain_opaque_pipeline`'s region-vertex variant for arena draws.
    terrain_region_pipeline: wgpu::RenderPipeline,
    terrain_alpha_pipeline: wgpu::RenderPipeline,
    lit_pipeline: wgpu::RenderPipeline,
    lit_alpha_pipeline: wgpu::RenderPipeline,
    instanced_pipeline: wgpu::RenderPipeline,
    /// Instanced particle billboard state — `None` when the device's
    /// limits cannot run the two-stream draw (`set_particle_billboards`
    /// then keeps the CPU-staged quad path).
    billboard: Option<ParticleBillboardGpu>,
    lit_frame_bg: wgpu::BindGroup,
    lit_frame_layout: wgpu::BindGroupLayout,
    /// Shadow-receiver bind group layout (`{receiver uniform, depth
    /// array, compare sampler}`), shared by [`ShadowGpu`] and pack
    /// `gbuffers_*` sub-world pipelines so a pack shader binding group 3
    /// accepts the live `shadow.receiver_bg`.
    shadow_receiver_layout: wgpu::BindGroupLayout,
    /// Four-group lit pipeline layout (`lit_frame`/`draw`/`env`/
    /// `shadow_receiver`) pack `gbuffers_entities`/`gbuffers_subworld`
    /// pipelines build against: group 3 stays unbound on shadow-free
    /// frames and carries the live cascade receiver when active.
    pack_lit_layout: wgpu::PipelineLayout,
    env_layout: wgpu::BindGroupLayout,
    draw_layout: wgpu::BindGroupLayout,
    draw_bg: wgpu::BindGroup,
    draw_ubo: wgpu::Buffer,
    draw_capacity: u64,
    draw_scratch: Vec<CompositeDrawUniforms>,
    /// Draw uniforms baked into the recorded opaque-terrain bundle; they
    /// persist at `draw_ubo` offsets `0..len` while the bundle is live so
    /// replays need no re-upload.
    terrain_draw_scratch: Vec<CompositeDrawUniforms>,
    env_ubo: wgpu::Buffer,
    lights_ubo: wgpu::Buffer,
    env_bg: wgpu::BindGroup,
    atlas: AtlasGpu,
}

struct AtlasGpu {
    _texture: wgpu::Texture,
    view: wgpu::TextureView,
    sampler: wgpu::Sampler,
    /// `SpriteFrame` table: 32 bytes per sprite — `rect` is the sampling
    /// window currently bound for the sprite, `anim` the `(next v0,
    /// blend)` pair `.mcmeta` playback rewrites each animation tick.
    rects: wgpu::Buffer,
    state_sprites: wgpu::Buffer,
    sprite_indices: HashMap<String, u32>,
    /// Whole-strip tile rect per sprite index — the base each animated
    /// frame window is cut from.
    tiles: Vec<AtlasTileRect>,
    /// Sprites carrying a `.mcmeta` playback table.
    animated: Vec<AnimatedSprite>,
    /// The `.mcmeta` clock value last written into `rects`, so steady
    /// frames cost one compare and no buffer writes.
    anim_tick: Option<u64>,
}

/// One atlas sprite with a `.mcmeta` playback table — the unit
/// [`GpuRenderer::tick_atlas_animations`] rewrites per tick.
struct AnimatedSprite {
    /// Sprite index: the pair lives at `rects[index]` in `SpriteFrame`
    /// units (byte offset `index * 32`).
    index: u32,
    /// Whole-strip rect the frame windows are cut from.
    tile: AtlasTileRect,
    animation: SpriteAnimation,
}

/// Byte stride of one sprite's `SpriteFrame` slot in the `atlas_rects`
/// table (a `rect` vec4 plus the `anim` aux vec4).
const ATLAS_SPRITE_FRAME_BYTES: u64 = 32;

/// Sprite-index resolution shared by `atlas_sprite_index`,
/// `set_block_state_sprites` and `set_atlas_animations`, mirroring
/// `PackedAtlas::tile_rect`'s lookup: exact name, then the path part of a
/// `namespace:path` id, then a `minecraft:`-prefixed guess.
fn sprite_index_of(indices: &HashMap<String, u32>, name: &str) -> Option<u32> {
    if let Some(&index) = indices.get(name) {
        return Some(index);
    }
    if let Some((_, path)) = name.split_once(':') {
        if let Some(&index) = indices.get(path) {
            return Some(index);
        }
    }
    if !name.contains(':') {
        return indices.get(&format!("minecraft:{name}")).copied();
    }
    None
}

impl AtlasGpu {
    fn bind_group(&self, device: &wgpu::Device, composite: &CompositeGpu) -> wgpu::BindGroup {
        device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("composite-env-atlas-bind-group"),
            layout: &composite.env_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: composite.env_ubo.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: composite.lights_ubo.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::TextureView(&self.view),
                },
                wgpu::BindGroupEntry {
                    binding: 3,
                    resource: wgpu::BindingResource::Sampler(&self.sampler),
                },
                wgpu::BindGroupEntry {
                    binding: 4,
                    resource: self.rects.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 5,
                    resource: self.state_sprites.as_entire_binding(),
                },
            ],
        })
    }
}

/// Shadow resources exist only while a nonzero-cascade preset is active.
/// Depth layers and the comparison sampler have matching lifetimes, while
/// each cascade owns an independent uniform buffer to avoid queue write
/// aliasing between passes in the same command submission.
/// Vertex layout for [`StainedCasterVertex`] (`MEGU_STAINED_GLASS_SHADOW_COLOR_WGSL`).
const STAINED_CASTER_LAYOUT: wgpu::VertexBufferLayout<'static> = wgpu::VertexBufferLayout {
    array_stride: std::mem::size_of::<StainedCasterVertex>() as u64,
    step_mode: wgpu::VertexStepMode::Vertex,
    attributes: &[
        wgpu::VertexAttribute {
            format: wgpu::VertexFormat::Float32x3,
            offset: 0,
            shader_location: 0,
        },
        wgpu::VertexAttribute {
            format: wgpu::VertexFormat::Uint32,
            offset: 12,
            shader_location: 1,
        },
    ],
};

/// Vertex layout for [`WaterVertex`] (`MEGU_WATER_SURFACE_WGSL`).
const WATER_VERTEX_LAYOUT: wgpu::VertexBufferLayout<'static> = wgpu::VertexBufferLayout {
    array_stride: std::mem::size_of::<WaterVertex>() as u64,
    step_mode: wgpu::VertexStepMode::Vertex,
    attributes: &[
        wgpu::VertexAttribute {
            format: wgpu::VertexFormat::Float32x3,
            offset: 0,
            shader_location: 0,
        },
        wgpu::VertexAttribute {
            format: wgpu::VertexFormat::Float32,
            offset: 12,
            shader_location: 1,
        },
    ],
};

/// Vertex layout for [`LitTerrainVertex`] (`MEGU_TERRAIN_LIT_WGSL`).
const LIT_TERRAIN_VERTEX_LAYOUT: wgpu::VertexBufferLayout<'static> = wgpu::VertexBufferLayout {
    array_stride: std::mem::size_of::<LitTerrainVertex>() as u64,
    step_mode: wgpu::VertexStepMode::Vertex,
    attributes: &[
        wgpu::VertexAttribute {
            format: wgpu::VertexFormat::Float32x3,
            offset: 0,
            shader_location: 0,
        },
        wgpu::VertexAttribute {
            format: wgpu::VertexFormat::Float32x3,
            offset: 12,
            shader_location: 1,
        },
        wgpu::VertexAttribute {
            format: wgpu::VertexFormat::Float32x3,
            offset: 24,
            shader_location: 2,
        },
        wgpu::VertexAttribute {
            format: wgpu::VertexFormat::Uint32,
            offset: 36,
            shader_location: 3,
        },
    ],
};

/// `MEGU_SHADOW_CUTOUT_DEPTH_WGSL` caster resources — lazily created the
/// first time a section carries `Layer::Cutout` quads, so worlds without
/// leaves/panes/glass pay zero.
struct CutoutGpu {
    pipeline: wgpu::RenderPipeline,
    /// `{atlas_tex, atlas_sampler, atlas_rects, atlas_states}` —
    /// group 3 of the cutout shader.
    bind_group: wgpu::BindGroup,
    /// `GpuRenderer::atlas_epoch` at creation — rebuilt on atlas reload.
    atlas_epoch: u64,
}

/// `MEGU_STAINED_GLASS_SHADOW_COLOR_WGSL` caster resources: pipeline,
/// per-cascade RGBA color targets (cleared white = full transmission)
/// and the staged caster vertex buffer.
struct StainedGpu {
    pipeline: wgpu::RenderPipeline,
    color_view: Vec<wgpu::TextureView>,
    vertices: wgpu::Buffer,
    vertex_count: u32,
}

/// `MEGU_WATER_SURFACE_WGSL` pass: own uniform (`view_proj`, `time`,
/// `camera_pos`) and single-bindgroup pipeline layout.
struct WaterGpu {
    pipeline: wgpu::RenderPipeline,
    uniform: wgpu::Buffer,
    bind_group: wgpu::BindGroup,
}

/// `MEGU_TERRAIN_LIT_WGSL` receiver path for explicitly uploaded
/// decoded-vertex terrain (`LitTerrainVertex` stream with
/// sky/block light in `light_attr`).
struct LitTerrainGpu {
    pipeline: wgpu::RenderPipeline,
    bind_group: wgpu::BindGroup,
    vertices: wgpu::Buffer,
    indices: wgpu::Buffer,
    index_count: u32,
    /// `GpuRenderer::shadow_gpu_epoch` the group-0 bind group was built
    /// against — rebuilt when the shadow depth texture is recreated.
    shadow_epoch: u64,
}

/// `MEGU_SPATIAL_SHARPEN_WGSL` upscale pass for `render_scale < 100%`:
/// 5-tap edge-adaptive sharpening of the world color target.
struct SharpenGpu {
    pipeline: wgpu::RenderPipeline,
    bind_group_layout: wgpu::BindGroupLayout,
    uniform: wgpu::Buffer,
}

struct ShadowGpu {
    config: CascadeConfig,
    /// `{shadow_uniform}` caster layout — shared by the opaque, cutout
    /// and stained-glass caster pipelines.
    caster_layout: wgpu::BindGroupLayout,
    /// Whole-array depth view + comparison sampler (also used by the
    /// `MEGU_TERRAIN_LIT_WGSL` receiver bind group).
    array_view: wgpu::TextureView,
    compare_sampler: wgpu::Sampler,
    cutout: Option<CutoutGpu>,
    stained: Option<StainedGpu>,
    _texture: wgpu::Texture,
    layer_views: Vec<wgpu::TextureView>,
    receiver_uniform: wgpu::Buffer,
    receiver_bg: wgpu::BindGroup,
    caster_uniforms: Vec<wgpu::Buffer>,
    caster_bgs: Vec<wgpu::BindGroup>,
    draw_ubo: wgpu::Buffer,
    draw_bg: wgpu::BindGroup,
    draw_capacity: usize,
    terrain_depth: wgpu::RenderPipeline,
    /// `vs_terrain` variant reading the region-vertex layout for
    /// arena-resident shadow casters.
    terrain_region_depth: wgpu::RenderPipeline,
    lit_depth: wgpu::RenderPipeline,
    instanced_depth: wgpu::RenderPipeline,
    terrain_color: wgpu::RenderPipeline,
    /// Shadow-sampling `terrain_vs` variant for region-arena draws.
    terrain_region_color: wgpu::RenderPipeline,
    lit_color: wgpu::RenderPipeline,
    lit_alpha_color: wgpu::RenderPipeline,
    instanced_color: wgpu::RenderPipeline,
    /// Shadow-sampling `billboard_vs` variant — `Some` exactly when the
    /// composite billboard pipeline exists.
    billboard_color: Option<wgpu::RenderPipeline>,
    /// Epoch bumped every time `draw_ubo`/`draw_bg` are rebuilt —
    /// recorded cascade bundles bake dynamic offsets into the pool and
    /// must re-record after a resize.
    draw_pool_epoch: u64,
    /// Recorded per-cascade caster passes, one per depth-array layer:
    /// replayed while the [`CascadeBundleKey`] inputs hold.
    cascade_bundles: [Option<CascadeBundle>; 3],
}

fn shadow_uniform_bytes(value: &ShadowUniforms) -> Vec<u8> {
    // ShadowUniforms is repr(C), but does not implement Pod in client-port.
    // Write its documented WGSL fields explicitly, with no unsafe casting.
    let mut floats = Vec::with_capacity(std::mem::size_of::<ShadowUniforms>() / 4);
    for matrix in value
        .light_view_proj
        .iter()
        .chain([&value.camera_view_proj])
    {
        for column in matrix {
            floats.extend_from_slice(column);
        }
    }
    for field in [
        value.cascade_splits,
        value.sun_direction,
        value.sun_color,
        value.moon_color,
        value.camera_position,
        value.camera_forward,
        value.camera_right,
        value.camera_up,
        value.shadow_params,
        value.render_params,
        value.celestial_params,
    ] {
        floats.extend_from_slice(&field);
    }
    assert_eq!(floats.len() * 4, std::mem::size_of::<ShadowUniforms>());
    bytemuck::cast_slice(&floats).to_vec()
}

/// Minimal `ShadowUniforms` for the water pass when no shadow preset is
/// active: camera transform plus the celestial fields
/// `MEGU_WATER_SURFACE_WGSL` actually reads (`sun_direction`,
/// `render_params` quality/day-phase, `sun_color`, `moon_color`,
/// `celestial_params`). Derived from the live server clock so water
/// still tracks day/night with shadows off.
fn water_fallback_uniforms(
    view_proj: Mat4,
    camera_position: [f32; 3],
    world_time_ticks: u64,
) -> ShadowUniforms {
    let sun = compute_sun_direction(world_time_ticks);
    let day_phase = (world_time_ticks % 24_000) as f32 / 24_000.0;
    let identity = [
        [1.0, 0.0, 0.0, 0.0],
        [0.0, 1.0, 0.0, 0.0],
        [0.0, 0.0, 1.0, 0.0],
        [0.0, 0.0, 0.0, 1.0],
    ];
    ShadowUniforms {
        light_view_proj: [identity, identity, identity],
        camera_view_proj: view_proj,
        cascade_splits: [0.0; 4],
        sun_direction: [sun[0], sun[1], sun[2], 0.0],
        sun_color: [1.0, 0.98, 0.92, 1.0],
        moon_color: [0.55, 0.62, 0.78, 0.35],
        camera_position: [
            camera_position[0],
            camera_position[1],
            camera_position[2],
            1.0,
        ],
        camera_forward: [0.0, 0.0, 1.0, 0.0],
        camera_right: [1.0, 0.0, 0.0, 0.0],
        camera_up: [0.0, 1.0, 0.0, 0.0],
        shadow_params: [0.0; 4],
        // quality tier + day phase (+fog/exposure unused by water).
        render_params: [1.0, day_phase, 0.0, 1.0],
        celestial_params: [0.0; 4],
    }
}

/// Composite opaque geometry receives a distinct shader entry when shadows
/// are active. The original shaders and layouts remain usable for Off and
/// Feather without any shadow texture, binding, or sampling instructions.
pub fn composed_shadow_wgsl(source: &str, terrain: bool) -> String {
    let shader = if terrain {
        source.replacen(
            "var color = in.albedo;",
            "var color = in.albedo * (0.38 + 0.62 * shadow_visibility(in.world, n));",
            1,
        )
    } else {
        source.replace(
            "light += vec3<f32>(env.sun.w) * max(dot(n, normalize(env.sun.xyz)), 0.0);",
            "light += vec3<f32>(env.sun.w) * max(dot(n, normalize(env.sun.xyz)), 0.0) * shadow_visibility(in.world, n);",
        )
    };
    format!("{shader}\n{MEGU_SHADOW_SAMPLING_WGSL}")
}

const FNV1A_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV1A_PRIME: u64 = 0x0000_0100_0000_01b3;

fn fnv1a_u64(hash: &mut u64, value: u64) {
    *hash ^= value;
    *hash = hash.wrapping_mul(FNV1A_PRIME);
}

fn fnv1a_u32(hash: &mut u64, value: u32) {
    fnv1a_u64(hash, u64::from(value));
}

/// Digest of the empty bank — the `last_shadow_casters` seed, so a bank
/// that never gains casters produces no spurious invalidation.
const EMPTY_CASTER_DIGEST: u64 = FNV1A_OFFSET;

/// Snapshot caster identities, transforms and visibility across composed
/// frames as a deterministic `u64` FNV-1a digest — zero allocation, same
/// field set the old `Vec<u32>` signature hashed. Geometry and instance
/// uploads also invalidate via their dirty flags before GPU
/// synchronization clears those flags.
pub fn subworld_caster_digest(bank: &SubWorldRenderBank) -> u64 {
    let mut hash = FNV1A_OFFSET;
    for entry in bank.entries() {
        // Translucent sheets (voxel clouds, water planes) never draw into
        // the depth map — excluding them keeps their transform-only drift
        // from churning the shadow-cache key.
        if entry.alpha < 0.999 {
            continue;
        }
        fnv1a_u64(&mut hash, entry.id);
        for v in entry.transform.position {
            fnv1a_u32(&mut hash, v.to_bits());
        }
        for v in entry.transform.quaternion {
            fnv1a_u32(&mut hash, v.to_bits());
        }
        fnv1a_u32(&mut hash, entry.transform.scale.to_bits());
        fnv1a_u32(&mut hash, entry.alpha.to_bits());
        fnv1a_u64(&mut hash, entry.vertices.len() as u64);
        fnv1a_u64(&mut hash, entry.indices.len() as u64);
    }
    for batch in bank.batches() {
        fnv1a_u32(&mut hash, batch.mesh_kind);
        for v in batch.transform.position {
            fnv1a_u32(&mut hash, v.to_bits());
        }
        for v in batch.transform.quaternion {
            fnv1a_u32(&mut hash, v.to_bits());
        }
        fnv1a_u32(&mut hash, batch.transform.scale.to_bits());
        fnv1a_u64(&mut hash, batch.template_vertices.len() as u64);
        fnv1a_u64(&mut hash, batch.template_indices.len() as u64);
        fnv1a_u64(&mut hash, batch.instances.len() as u64);
        // Per-instance placement + spin is caster geometry: a water
        // wheel advancing `rot_uv_frame` without it would leave the
        // shadow map frozen mid-turn.
        for inst in &batch.instances {
            for v in inst.pos_scale {
                fnv1a_u32(&mut hash, v.to_bits());
            }
            for v in inst.rot_uv_frame {
                fnv1a_u32(&mut hash, v.to_bits());
            }
            fnv1a_u32(&mut hash, inst.mesh_kind);
            fnv1a_u32(&mut hash, inst.subworld_id);
        }
    }
    hash
}

fn subworld_casters_dirty(bank: &SubWorldRenderBank) -> bool {
    bank.entries()
        .any(|entry| entry.dirty && entry.alpha >= 0.999)
        || bank
            .batches()
            .any(|batch| batch.geometry_dirty || batch.instances_dirty)
}

// ---------------------------------------------------------------------------
// Rotation-invariant CSM ortho + sky uniform machinery
// ---------------------------------------------------------------------------

fn v3_dot(a: [f32; 3], b: [f32; 3]) -> f32 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

fn v3_cross(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}

fn v3_scale(a: [f32; 3], k: f32) -> [f32; 3] {
    [a[0] * k, a[1] * k, a[2] * k]
}

fn v3_add(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [a[0] + b[0], a[1] + b[1], a[2] + b[2]]
}

fn v3_norm(a: [f32; 3], fallback: [f32; 3]) -> [f32; 3] {
    let sq = v3_dot(a, a);
    if sq.is_finite() && sq > 1.0e-12 {
        v3_scale(a, sq.sqrt().recip())
    } else {
        fallback
    }
}

/// World-space volume one shadow cascade covers: a texel-snapped
/// light-space ortho box expressed back in world axes.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CascadeOrtho {
    /// World-space center of the ortho box (texel-snapped light center).
    pub center: [f32; 3],
    /// Half extent along `light_right`/`light_up` (shared radius).
    pub radius: f32,
    /// Half extent along the sun axis.
    pub depth_radius: f32,
    /// Light basis `(light_right, light_up, sun)`.
    pub axes: [[f32; 3]; 3],
}

/// Conservative world-space AABB of one cascade's ortho volume — used to
/// cull caster regions/sections that cannot touch the cascade.
pub fn cascade_world_aabb(ortho: &CascadeOrtho) -> ([f32; 3], [f32; 3]) {
    let [right, up, sun] = ortho.axes;
    let mut min = ortho.center;
    let mut max = ortho.center;
    for i in 0..3 {
        let half =
            ortho.radius * (right[i].abs() + up[i].abs()) + ortho.depth_radius * sun[i].abs();
        min[i] -= half;
        max[i] += half;
    }
    (min, max)
}

/// `true` when the world AABB `[min, max]` intersects the cascade's
/// conservative world AABB — the per-cascade caster cull test.
pub fn aabb_in_cascade(min: [f32; 3], max: [f32; 3], ortho: &CascadeOrtho) -> bool {
    let (cmin, cmax) = cascade_world_aabb(ortho);
    min[0] <= cmax[0]
        && max[0] >= cmin[0]
        && min[1] <= cmax[1]
        && max[1] >= cmin[1]
        && min[2] <= cmax[2]
        && max[2] >= cmin[2]
}

/// [`ShadowUniforms`] plus the per-cascade light-space ortho volumes the
/// matrices were built from — the culler needs the volumes, the depth
/// passes need the matrices.
#[derive(Clone, Copy)]
struct StableShadow {
    uniforms: ShadowUniforms,
    orthos: [CascadeOrtho; 3],
}

/// Rotation-invariant shadow uniforms: identical to
/// `build_shadow_uniforms_celestial` except each cascade's light ortho
/// centers on a bounding sphere around the camera *position* covering
/// every point the frustum can see within the split distance — yaw and
/// pitch cannot move it, so a pure look rotation never changes the
/// rendered depth map (no shimmer, full cache reuse).
fn build_stable_shadow_uniforms(
    camera: &Camera,
    aspect: f32,
    config: CascadeConfig,
    world_time_ticks: u64,
    moon_phase: u8,
) -> StableShadow {
    let shadow_camera = ShadowCamera {
        position: camera.position,
        forward: camera.forward_vector(),
        up: [0.0, 1.0, 0.0],
        fov_y_radians: camera.fov_deg.to_radians(),
        aspect_ratio: aspect,
        near: camera.near,
        far: camera.far,
    };
    let mut uniforms = build_shadow_uniforms_celestial(
        shadow_camera,
        config,
        world_time_ticks,
        moon_phase,
        EclipseMode::None,
    );
    let position = if camera.position.iter().all(|v| v.is_finite()) {
        camera.position
    } else {
        [0.0; 3]
    };
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
    let fov = if camera.fov_deg.is_finite() {
        camera
            .fov_deg
            .to_radians()
            .clamp(0.1, std::f32::consts::PI - 0.1)
    } else {
        std::f32::consts::PI / 3.0
    };
    let aspect = if aspect.is_finite() {
        aspect.clamp(0.25, 8.0)
    } else {
        16.0 / 9.0
    };
    let tan_half_fov = (fov * 0.5).tan();
    let sun = compute_sun_direction(world_time_ticks);
    // Sun by day, real moon at `-sun` by night — identical caster rule to
    // `build_shadow_uniforms_celestial` and `MEGU_SHADOW_SAMPLING_WGSL`'s
    // `shadow_caster_direction`, so light space stays overhead through the
    // whole day cycle instead of pointing into the earth after sunset.
    let caster = if sun[1] >= 0.0 {
        sun
    } else {
        v3_scale(sun, -1.0)
    };
    let light_right = v3_norm(v3_cross([0.0, 0.0, 1.0], caster), [1.0, 0.0, 0.0]);
    let light_up = v3_cross(caster, light_right);
    let count = config.cascade_count.min(3);
    let splits = compute_cascade_splits(near, far, count, 0.65);
    let mut orthos = [CascadeOrtho {
        center: [0.0; 3],
        radius: 0.0,
        depth_radius: 0.0,
        axes: [light_right, light_up, caster],
    }; 3];
    for (index, &end) in splits.iter().enumerate() {
        // Bounding sphere centered on the camera covering the whole
        // frustum slice out to `end` — the far-face corner is the
        // farthest reachable point and carries no forward-axis term,
        // so the volume is identical under any camera rotation.
        let h = tan_half_fov * end;
        let w = h * aspect;
        let reach = (end * end + w * w + h * h).sqrt();
        // Stable extent quantized to a sixteenth of a world block.
        let radius = (reach * 16.0).ceil().max(16.0) / 16.0;
        let texel = 2.0 * radius / config.map_resolution.max(1) as f32;
        let snap = |coordinate: f32| (coordinate / texel).round() * texel;
        let cx = snap(v3_dot(position, light_right));
        let cy = snap(v3_dot(position, light_up));
        let cz = snap(v3_dot(position, caster));
        let depth_radius = radius * 4.0;
        let inverse_depth = 1.0 / (2.0 * depth_radius);
        uniforms.light_view_proj[index] = [
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
        orthos[index] = CascadeOrtho {
            center: v3_add(
                v3_add(v3_scale(light_right, cx), v3_scale(light_up, cy)),
                v3_scale(caster, cz),
            ),
            radius,
            depth_radius,
            axes: [light_right, light_up, caster],
        };
    }
    StableShadow { uniforms, orthos }
}

/// Squared distance from `camera` to the center of the 8x4x8-section
/// region rooted at `origin` (region-local section coords × 16 blocks).
pub fn region_center_dist2(origin: [i32; 3], camera: [f32; 3]) -> f32 {
    let center = [
        origin[0] as f32 * EDGE as f32 + (EDGE * 4) as f32,
        origin[1] as f32 * EDGE as f32 + (EDGE * 2) as f32,
        origin[2] as f32 * EDGE as f32 + (EDGE * 4) as f32,
    ];
    (0..3)
        .map(|i| (center[i] - camera[i]) * (center[i] - camera[i]))
        .sum()
}

/// Sort region origins ascending by squared camera distance —
/// front-to-back, so hardware Early-Z/Hi-Z rejects occluded far terrain
/// instead of shading it first.
pub fn sort_regions_front_to_back(origins: &mut [[i32; 3]], camera: [f32; 3]) {
    origins.sort_by_key(|origin| region_center_dist2(*origin, camera).to_bits());
}

/// Positive-vertex (p-vertex) six-plane AABB frustum test, branchless:
/// each plane is tested against the box corner farthest along its
/// normal, so a box is rejected only when it lies *entirely* outside at
/// least one plane — six dot products, zero false negatives. This is the
/// exact contract `SECTION_CULL_WGSL`'s `aabb_in_frustum` implements on
/// the GPU.
pub fn aabb_in_frustum_pvertex(min: [f32; 3], max: [f32; 3], planes: &[[f32; 4]]) -> bool {
    let mut inside = true;
    for &[nx, ny, nz, d] in planes {
        let pos = [
            if nx >= 0.0 { max[0] } else { min[0] },
            if ny >= 0.0 { max[1] } else { min[1] },
            if nz >= 0.0 { max[2] } else { min[2] },
        ];
        inside &= nx * pos[0] + ny * pos[1] + nz * pos[2] + d >= 0.0;
    }
    inside
}

/// 21-bits-per-lane 3D Morton (Z-order) code — a thin re-export of the
/// canonical `megu_core::math_opt_engine::morton3d_encode` SWAR spread,
/// kept under the historical name for embedders. Interleaving keeps
/// spatial neighbours adjacent — the locality-preserving tie-breaker
/// inside one camera-distance ring.
pub fn morton3d(x: u32, y: u32, z: u32) -> u64 {
    morton3d_encode(x, y, z)
}

/// Morton key for a signed section coordinate via the canonical
/// `megu_core::math_opt_engine::morton3d_signed`: each axis clamps to
/// `[-2^20, 2^20 - 1]` and biases by `+2^20`, so every section coord in
/// ±2^20 (±16.7M blocks) maps distinctly and out-of-range coords
/// saturate at the edge instead of wrapping (Z-order is a locality
/// hash, not a comparator).
pub fn section_morton_key(coord: SectionCoord) -> u64 {
    morton3d_signed(coord.0, coord.1, coord.2)
}

/// Vertical focal length in pixels for `camera` over a `height_px`-tall
/// viewport: `f_y = h / (2·tan(fov_y/2))` — the exact `focal_y_px`
/// argument [`projected_sphere_diameter_px`] expects, matching
/// `Camera::projection_matrix`'s `cot(fov_y/2)` scaled to pixels.
/// Non-finite inputs yield a non-finite focal, which the projection
/// helper fails closed to `INFINITY` (never culls).
fn camera_focal_y_px(camera: &crate::Camera, height_px: u32) -> f32 {
    let f = 1.0 / (camera.fov_deg.to_radians() * 0.5).tan();
    height_px as f32 * 0.5 * f
}

/// O(1) fast reject for a `EDGE`-sized section whose perspective
/// silhouette falls under half a pixel — it cannot cover any sample
/// point, so skipping it loses no visible fragment. Uses the
/// Mara–McGuire 2013 closed-form [`projected_sphere_diameter_px`]:
/// the silhouette diameter depends only on `‖c_view‖`, so the
/// world-space distance is passed head-on (the helper's
/// camera-inside → `INFINITY` contract still prevents a false reject).
pub fn is_subpixel_section(
    camera_position: [f32; 3],
    aabb_min: [f32; 3],
    focal_y_px: f32,
) -> bool {
    const SUBPIXEL_THRESHOLD_PX: f32 = 0.5;
    let half = EDGE as f32 * 0.5;
    let center = [
        aabb_min[0] + half,
        aabb_min[1] + half,
        aabb_min[2] + half,
    ];
    let distance = (0..3)
        .map(|i| (center[i] - camera_position[i]) * (center[i] - camera_position[i]))
        .sum::<f32>()
        .sqrt();
    // Bounding sphere of the EDGE³ box: half the space diagonal.
    let radius = half * 3.0_f32.sqrt();
    projected_sphere_diameter_px([0.0, 0.0, distance], radius, focal_y_px)
        < SUBPIXEL_THRESHOLD_PX
}

/// Conservative whole-tile frustum decision driven by
/// [`LipschitzTileClassifier`]: each plane's signed distance
/// `n·p + d` is a linear field with Lipschitz constant `‖n‖`, so one
/// centre sample proves a box "strictly outside" (cull) or "strictly
/// inside" (that plane can never reject it). The Euclidean Lipschitz
/// radius is looser than the exact per-axis support, so a uniform
/// verdict is always safe — only a mixed plane falls through to the
/// exact p-vertex sweep over the un-decided subset.
pub fn section_frustum_visible(min: [f32; 3], planes: &[[f32; 4]]) -> bool {
    let half = EDGE as f64 * 0.5;
    let center = [
        f64::from(min[0]) + half,
        f64::from(min[1]) + half,
        f64::from(min[2]) + half,
    ];
    let half_extents = [half; 3];
    let mut mixed = [[0.0f32; 4]; 8];
    let mut mixed_count = 0usize;
    for &[nx, ny, nz, d] in planes {
        if mixed_count >= mixed.len() {
            break;
        }
        let center_value = f64::from(nx) * center[0]
            + f64::from(ny) * center[1]
            + f64::from(nz) * center[2]
            + f64::from(d);
        let bound = f64::from(nx.mul_add(nx, ny.mul_add(ny, nz * nz)).sqrt());
        match LipschitzTileClassifier::classify_subtile_lipschitz(
            center_value,
            bound,
            half_extents,
        ) {
            // Strictly outside this plane — provably invisible.
            TileSignDecision::UniformNegative => return false,
            // Strictly inside — this plane can never cull the box.
            TileSignDecision::UniformPositive => {}
            TileSignDecision::MixedRequiresEvaluation => {
                mixed[mixed_count] = [nx, ny, nz, d];
                mixed_count += 1;
            }
        }
    }
    mixed_count == 0 || box_in_frustum(min, &mixed[..mixed_count])
}

/// Opaque submission order for one section: the quantized radial
/// distance ring (16-block bands, nearest first) then the Morton Z-order
/// tie-breaker — front-to-back maximizes hardware Early-Z/Hi-Z rejection
/// while Z-order keeps GPU L2 spatial locality inside each ring.
pub fn opaque_section_sort_key(coord: SectionCoord, camera: [f32; 3]) -> (u32, u64) {
    let half = EDGE as f32 * 0.5;
    let center = [
        coord.0 as f32 * EDGE as f32 + half,
        coord.1 as f32 * EDGE as f32 + half,
        coord.2 as f32 * EDGE as f32 + half,
    ];
    let dist = (0..3)
        .map(|i| (center[i] - camera[i]) * (center[i] - camera[i]))
        .sum::<f32>()
        .sqrt();
    ((dist / EDGE as f32) as u32, section_morton_key(coord))
}

/// Grow-only GPU-driven indirect opaque buffers (audit
/// `GPU-INDIRECT-AND-OPT-WIRE`): `SectionGpuRecord` inputs plus the
/// GPU-written `IndirectArgs` the render pass consumes — reallocated
/// only when residency outgrows capacity, so steady-state frames cost
/// zero `create_buffer` calls.
struct GpuIndirectBuffers {
    /// `SectionGpuRecord` array the cull pass reads.
    sections: wgpu::Buffer,
    /// `IndirectArgs` array the cull pass writes and the opaque pass
    /// draws from (`STORAGE` for the shader, `INDIRECT` for the draw).
    args: wgpu::Buffer,
    /// `CullStats` atomics, zeroed before each dispatch.
    stats: wgpu::Buffer,
    /// Per-section kept bucket masks (reserved for masked draws).
    masks: wgpu::Buffer,
    /// `CullUniforms` for the frame's cull dispatch.
    uniforms: wgpu::Buffer,
    /// Section-record capacity every buffer was sized for.
    capacity: usize,
    /// Record coords in args order — the test/debug readback mapping.
    record_coords: Vec<SectionCoord>,
    /// CPU shadow of `sections` — the baseline the next frame's record
    /// pack diffs against, so steady frames upload zero bytes and a
    /// changed residency uploads only the coalesced dirty spans.
    record_shadow: Vec<SectionGpuRecord>,
    /// Cached pack key & runs so steady frames skip region sorting, per-region
    /// slot Vec allocs, repacking, and shadow diffing completely.
    packed_world_revision: u64,
    packed_camera_section: [i32; 3],
    packed_morton: bool,
    cached_runs: Vec<GpuIndirectRun>,
    cached_count: u32,
}

/// One region's contiguous run inside the frame's indirect args buffer.
#[derive(Clone, Copy)]
struct GpuIndirectRun {
    /// `region_arenas` key — picks the vertex buffer and bind group.
    origin: [i32; 3],
    /// First arg slot of the run.
    first: u32,
    /// Consecutive args in the run.
    count: u32,
}

/// Everything the opaque pass needs to submit the GPU-culled arena
/// draws: the args buffer plus one contiguous run per region, ordered
/// front-to-back by region.
struct GpuIndirectFrame {
    /// GPU-written `IndirectArgs` (a clone of the persistent buffer's
    /// handle — wgpu refcounts keep it alive for the pass).
    args: wgpu::Buffer,
    /// Per-region contiguous runs, front-to-back.
    runs: Vec<GpuIndirectRun>,
}

/// Differential `SectionGpuRecord` upload: bitwise-diff `records`
/// against `shadow` (the bytes already in `buffer`) and `write_buffer`
/// only the dirty indices, coalescing adjacent ones into one minimal
/// contiguous span per call. A grown `records` dirties its tail (an
/// empty shadow — fresh or just-reallocated buffer — collapses to a
/// single full-slice write); a shrunk one needs no tail write because
/// the shader reads only `count` records. Returns `(write_buffer calls,
/// bytes uploaded)` — `(0, 0)` on a steady frame — and resyncs `shadow`
/// to `records` either way.
fn write_indirect_record_diff(
    queue: &wgpu::Queue,
    buffer: &wgpu::Buffer,
    shadow: &mut Vec<SectionGpuRecord>,
    records: &[SectionGpuRecord],
) -> (u64, u64) {
    let stride = std::mem::size_of::<SectionGpuRecord>() as u64;
    let mut calls = 0u64;
    let mut bytes = 0u64;
    let mut span_start: Option<usize> = None;
    for (i, record) in records.iter().enumerate() {
        let dirty = shadow
            .get(i)
            .is_none_or(|s| bytemuck::bytes_of(s) != bytemuck::bytes_of(record));
        if dirty {
            if span_start.is_none() {
                span_start = Some(i);
            }
        } else if let Some(start) = span_start.take() {
            let data = bytemuck::cast_slice(&records[start..i]);
            queue.write_buffer(buffer, start as u64 * stride, data);
            calls += 1;
            bytes += data.len() as u64;
        }
    }
    if let Some(start) = span_start {
        let data = bytemuck::cast_slice(&records[start..]);
        queue.write_buffer(buffer, start as u64 * stride, data);
        calls += 1;
        bytes += data.len() as u64;
    }
    shadow.clear();
    shadow.extend_from_slice(records);
    (calls, bytes)
}

/// Inputs that decide whether a recorded per-cascade shadow `RenderBundle`
/// still applies: the caster residency/scene/subworld revisions, the
/// shadow draw-pool epoch (buffer replacement invalidates recorded
/// offsets), and a digest of the snapped ortho parameters the recorded
/// cull set depends on.
#[derive(Clone, Copy, PartialEq)]
struct CascadeBundleKey {
    layer: u32,
    world_rev: u64,
    scene_rev: u64,
    draw_pool: u64,
    casters: u64,
    ortho_hash: u64,
}

/// One recorded cascade pass plus the key it was captured under.
struct CascadeBundle {
    key: CascadeBundleKey,
    bundle: wgpu::RenderBundle,
}

/// FNV-1a digest of every field of a [`CascadeOrtho`] — the recorded
/// caster cull set changes exactly when this changes.
fn cascade_ortho_hash(ortho: &CascadeOrtho) -> u64 {
    let mut hash = FNV1A_OFFSET;
    for v in ortho.center {
        fnv1a_u32(&mut hash, v.to_bits());
    }
    fnv1a_u32(&mut hash, ortho.radius.to_bits());
    fnv1a_u32(&mut hash, ortho.depth_radius.to_bits());
    for axis in ortho.axes {
        for v in axis {
            fnv1a_u32(&mut hash, v.to_bits());
        }
    }
    hash
}

/// World-space AABB of the 8x4x8-section region rooted at `origin`
/// (region-local section coords × 16 blocks).
pub fn region_world_aabb(origin: [i32; 3]) -> ([f32; 3], [f32; 3]) {
    let min = [
        origin[0] as f32 * EDGE as f32,
        origin[1] as f32 * EDGE as f32,
        origin[2] as f32 * EDGE as f32,
    ];
    let max = [
        min[0] + (REGION_X * EDGE) as f32,
        min[1] + (REGION_Y * EDGE) as f32,
        min[2] + (REGION_Z * EDGE) as f32,
    ];
    (min, max)
}

/// Record one cascade's caster draws into `pass` (a live `RenderPass` or
/// a `RenderBundleEncoder` — identical streams). Sections and region
/// arenas outside the cascade's texel-snapped light-space AABB are
/// skipped; the return value is the number of skipped casters for
/// `shadow_regions_culled`. Offsets index the shared shadow draw pool —
/// culled casters keep their slot so the pool layout stays stable.
#[allow(clippy::too_many_arguments)]
fn record_shadow_cascade<'a>(
    pass: &mut impl wgpu::util::RenderEncoder<'a>,
    shadow: &'a ShadowGpu,
    frame_bind_group: &'a wgpu::BindGroup,
    ortho: &CascadeOrtho,
    scene: Option<(&'a TerrainBuffers, u32)>,
    section_offsets: &[(SectionCoord, u32)],
    cutout_sections: &[SectionCoord],
    live_sections: &'a HashMap<SectionCoord, LiveTerrainSection>,
    region_offsets: &[([i32; 3], u32)],
    region_arenas: &'a BTreeMap<[i32; 3], RegionArena>,
    shared_index: &'a wgpu::Buffer,
    entry_offsets: &[u32],
    batch_offsets: &[u32],
    subworld_gpu: &'a SubWorldGpuCache,
    layer: u32,
) -> u64 {
    let mut culled = 0u64;
    pass.set_bind_group(0, Some(&shadow.caster_bgs[layer as usize]), &[]);
    pass.set_pipeline(&shadow.terrain_depth);
    if let Some((scene, offset)) = scene {
        pass.set_bind_group(1, Some(frame_bind_group), &[]);
        pass.set_bind_group(2, Some(&shadow.draw_bg), &[offset]);
        pass.set_vertex_buffer(0, Some(scene.opaque_vertices.slice(..)));
        pass.set_index_buffer(scene.opaque_indices.slice(..), wgpu::IndexFormat::Uint32);
        pass.draw_indexed(0..scene.opaque_index_count, 0, 0..1);
    }
    for (coord, offset) in section_offsets {
        let min = [
            coord.0 as f32 * EDGE as f32,
            coord.1 as f32 * EDGE as f32,
            coord.2 as f32 * EDGE as f32,
        ];
        let max = [
            min[0] + EDGE as f32,
            min[1] + EDGE as f32,
            min[2] + EDGE as f32,
        ];
        if !aabb_in_cascade(min, max, ortho) {
            culled += 1;
            continue;
        }
        let section = &live_sections[coord];
        pass.set_bind_group(1, Some(&section.bind_group), &[]);
        pass.set_bind_group(2, Some(&shadow.draw_bg), &[*offset]);
        pass.set_vertex_buffer(0, Some(section.terrain.opaque_vertices.slice(..)));
        pass.set_index_buffer(shared_index.slice(..), wgpu::IndexFormat::Uint32);
        pass.draw_indexed(0..section.terrain.opaque_index_count, 0, 0..1);
    }
    // Alpha-tested casters (leaves, panes, bars, flowers) draw a second
    // time through `MEGU_SHADOW_CUTOUT_DEPTH_WGSL` — same packed vertex
    // stream, dedicated index list, atlas-tested fragment discard.
    if !cutout_sections.is_empty() {
        let cutout = shadow.cutout.as_ref().expect("cutout ensured");
        pass.set_pipeline(&cutout.pipeline);
        pass.set_bind_group(3, Some(&cutout.bind_group), &[]);
        for coord in cutout_sections {
            let min = [
                coord.0 as f32 * EDGE as f32,
                coord.1 as f32 * EDGE as f32,
                coord.2 as f32 * EDGE as f32,
            ];
            let max = [
                min[0] + EDGE as f32,
                min[1] + EDGE as f32,
                min[2] + EDGE as f32,
            ];
            if !aabb_in_cascade(min, max, ortho) {
                culled += 1;
                continue;
            }
            let section = &live_sections[coord];
            pass.set_bind_group(0, Some(&shadow.caster_bgs[layer as usize]), &[]);
            pass.set_bind_group(1, Some(&section.bind_group), &[]);
            pass.set_bind_group(2, Some(&shadow.draw_bg), &[0]);
            pass.set_vertex_buffer(0, Some(section.terrain.opaque_vertices.slice(..)));
            pass.set_index_buffer(
                section.terrain.cutout_indices.slice(..),
                wgpu::IndexFormat::Uint32,
            );
            pass.draw_indexed(0..section.terrain.cutout_index_count, 0, 0..1);
        }
    }
    // Arena-resident sections cast through the region depth pipeline:
    // the light needs every bucket, so the arena's committed live ranges
    // draw positionally through the shared index buffer.
    if !region_offsets.is_empty() {
        pass.set_pipeline(&shadow.terrain_region_depth);
        pass.set_index_buffer(shared_index.slice(..), wgpu::IndexFormat::Uint32);
        for (origin, offset) in region_offsets {
            let (min, max) = region_world_aabb(*origin);
            if !aabb_in_cascade(min, max, ortho) {
                culled += 1;
                continue;
            }
            let arena = &region_arenas[origin];
            pass.set_bind_group(1, Some(&arena.bind_group), &[]);
            pass.set_bind_group(2, Some(&shadow.draw_bg), &[*offset]);
            pass.set_vertex_buffer(0, Some(arena.vertex_buffer.slice(..)));
            arena.for_each_live_range(|start, end| {
                pass.draw_indexed(start..end, 0, 0..1);
            });
        }
    }
    pass.set_pipeline(&shadow.lit_depth);
    pass.set_bind_group(1, Some(frame_bind_group), &[]);
    for (entry, offset) in subworld_gpu
        .entries()
        .filter(|entry| entry.index_count > 0 && entry.alpha >= 0.999)
        .zip(entry_offsets)
    {
        pass.set_bind_group(2, Some(&shadow.draw_bg), &[*offset]);
        pass.set_vertex_buffer(0, Some(entry.vertex_buffer().slice(..)));
        pass.set_index_buffer(entry.index_buffer().slice(..), wgpu::IndexFormat::Uint32);
        pass.draw_indexed(0..entry.index_count, 0, 0..1);
    }
    pass.set_pipeline(&shadow.instanced_depth);
    for (batch, offset) in subworld_gpu
        .batches()
        .filter(|batch| batch.index_count > 0 && batch.instance_count > 0)
        .zip(batch_offsets)
    {
        pass.set_bind_group(2, Some(&shadow.draw_bg), &[*offset]);
        pass.set_vertex_buffer(0, Some(batch.template_vertex_buffer().slice(..)));
        pass.set_vertex_buffer(1, Some(batch.instance_buffer().slice(..)));
        pass.set_index_buffer(
            batch.template_index_buffer().slice(..),
            wgpu::IndexFormat::Uint32,
        );
        pass.draw_indexed(0..batch.index_count, 0, 0..batch.instance_count);
    }
    culled
}

impl ShadowGpu {
    fn new(
        ctx: &GpuDevice,
        composite: &CompositeGpu,
        frame_layout: &wgpu::BindGroupLayout,
        target_format: wgpu::TextureFormat,
        config: CascadeConfig,
    ) -> Self {
        let device = &ctx.device;
        // Shared with `CompositeGpu`: pack `gbuffers_*` sub-world
        // pipelines build against the same layout object, so their
        // group-3 bindings accept `receiver_bg` directly.
        let shadow_layout = &composite.shadow_receiver_layout;
        let caster_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("shadow-caster-layout"),
            entries: &[uniform_entry(0)],
        });
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("shadow-depth-array"),
            size: wgpu::Extent3d {
                width: config.map_resolution,
                height: config.map_resolution,
                depth_or_array_layers: config.cascade_count,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: DEPTH_FORMAT,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        });
        let array_view = texture.create_view(&wgpu::TextureViewDescriptor {
            dimension: Some(wgpu::TextureViewDimension::D2Array),
            ..Default::default()
        });
        let layer_views = (0..config.cascade_count)
            .map(|layer| {
                texture.create_view(&wgpu::TextureViewDescriptor {
                    dimension: Some(wgpu::TextureViewDimension::D2),
                    base_array_layer: layer,
                    array_layer_count: Some(1),
                    ..Default::default()
                })
            })
            .collect();
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("shadow-compare-sampler"),
            compare: Some(wgpu::CompareFunction::LessEqual),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        let make_uniform = |label| {
            device.create_buffer(&wgpu::BufferDescriptor {
                label: Some(label),
                size: std::mem::size_of::<ShadowUniforms>() as u64,
                usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            })
        };
        let receiver_uniform = make_uniform("shadow-receiver-uniform");
        let receiver_bg = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("shadow-receiver"),
            layout: shadow_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: receiver_uniform.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(&array_view),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::Sampler(&sampler),
                },
            ],
        });
        let mut caster_uniforms = Vec::new();
        let mut caster_bgs = Vec::new();
        for _ in 0..config.cascade_count {
            let buffer = make_uniform("shadow-caster-uniform");
            let bg = device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("shadow-caster"),
                layout: &caster_layout,
                entries: &[wgpu::BindGroupEntry {
                    binding: 0,
                    resource: buffer.as_entire_binding(),
                }],
            });
            caster_uniforms.push(buffer);
            caster_bgs.push(bg);
        }
        let draw_capacity = 64;
        let draw_ubo = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("shadow-draw-pool"),
            size: draw_capacity as u64 * u64::from(DRAW_UNIFORM_STRIDE),
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let draw_bg = Self::draw_bind_group(device, &composite.draw_layout, &draw_ubo);
        let caster_pipeline_layout =
            device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("shadow-depth-pipeline-layout"),
                bind_group_layouts: &[
                    Some(&caster_layout),
                    Some(frame_layout),
                    Some(&composite.draw_layout),
                ],
                immediate_size: 0,
            });
        let caster_source = format!("{MEGU_SHADOW_DEPTH_WGSL}\n{MEGU_SHADOW_CASTERS_WGSL}");
        let caster_module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("shadow-caster-module"),
            source: wgpu::ShaderSource::Wgsl(Cow::Owned(caster_source)),
        });
        let caster_region_module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("shadow-caster-region-module"),
            source: wgpu::ShaderSource::Wgsl(Cow::Owned(region_caster_wgsl(&format!(
                "{MEGU_SHADOW_DEPTH_WGSL}\n{MEGU_SHADOW_CASTERS_WGSL}"
            )))),
        });
        let depth_pipeline =
            |label,
             module: &wgpu::ShaderModule,
             entry,
             buffers: &[Option<wgpu::VertexBufferLayout<'_>>]| {
                device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                    label: Some(label),
                    layout: Some(&caster_pipeline_layout),
                    vertex: wgpu::VertexState {
                        module,
                        entry_point: Some(entry),
                        compilation_options: Default::default(),
                        buffers,
                    },
                    primitive: wgpu::PrimitiveState {
                        // Back-face rasterization for casters: front
                        // faces cull in hardware so the depth map holds
                        // rear-surface depth — cheaper and less acne.
                        cull_mode: Some(wgpu::Face::Front),
                        ..Default::default()
                    },
                    depth_stencil: Some(wgpu::DepthStencilState {
                        bias: wgpu::DepthBiasState {
                            constant: 1,
                            slope_scale: 1.5,
                            clamp: 0.0,
                        },
                        ..depth_state(true)
                    }),
                    multisample: Default::default(),
                    fragment: None,
                    multiview_mask: None,
                    cache: None,
                })
            };
        let terrain_depth = depth_pipeline(
            "shadow-packed-terrain",
            &caster_module,
            "vs_terrain",
            &[Some(TERRAIN_VERTEX_LAYOUT)],
        );
        let terrain_region_depth = depth_pipeline(
            "shadow-region-terrain",
            &caster_region_module,
            "vs_terrain",
            &[Some(TERRAIN_VERTEX_LAYOUT)],
        );
        let lit_depth = depth_pipeline(
            "shadow-lit",
            &caster_module,
            "vs_lit",
            &[Some(LIT_VERTEX_LAYOUT)],
        );
        let instanced_depth = depth_pipeline(
            "shadow-instanced",
            &caster_module,
            "vs_inst",
            &[Some(LIT_VERTEX_LAYOUT), Some(PART_INSTANCE_LAYOUT)],
        );

        let terrain_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("shadowed-composite-terrain"),
            bind_group_layouts: &[
                Some(frame_layout),
                Some(&composite.draw_layout),
                Some(&composite.env_layout),
                Some(shadow_layout),
            ],
            immediate_size: 0,
        });
        // Same four bind-group set `CompositeGpu::pack_lit_layout`
        // declares — reuse the shared object so pack sub-world
        // pipelines and the built-in shadow receivers are layout-equal.
        let lit_layout = &composite.pack_lit_layout;
        let terrain_module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("shadowed-terrain"),
            source: wgpu::ShaderSource::Wgsl(Cow::Owned(composed_shadow_wgsl(
                COMPOSITE_TERRAIN_WGSL,
                true,
            ))),
        });
        let terrain_region_module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("shadowed-terrain-region"),
            source: wgpu::ShaderSource::Wgsl(Cow::Owned(composed_shadow_wgsl(
                &region_opaque_wgsl(COMPOSITE_TERRAIN_WGSL),
                true,
            ))),
        });
        let lit_module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("shadowed-lit"),
            source: wgpu::ShaderSource::Wgsl(Cow::Owned(composed_shadow_wgsl(
                COMPOSITE_LIT_WGSL,
                false,
            ))),
        });
        let color_pipeline = |label, layout, module, vs, fs, buffers, blend, write, cull| {
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some(label),
                layout: Some(layout),
                vertex: wgpu::VertexState {
                    module,
                    entry_point: Some(vs),
                    compilation_options: Default::default(),
                    buffers,
                },
                primitive: wgpu::PrimitiveState {
                    cull_mode: cull,
                    ..Default::default()
                },
                depth_stencil: Some(depth_state(write)),
                multisample: Default::default(),
                fragment: Some(wgpu::FragmentState {
                    module,
                    entry_point: Some(fs),
                    compilation_options: Default::default(),
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
        let terrain_color = color_pipeline(
            "shadowed-terrain-color",
            &terrain_layout,
            &terrain_module,
            "terrain_vs",
            "terrain_fs",
            &[Some(TERRAIN_VERTEX_LAYOUT)],
            None,
            true,
            Some(wgpu::Face::Back),
        );
        let terrain_region_color = color_pipeline(
            "shadowed-terrain-region-color",
            &terrain_layout,
            &terrain_region_module,
            "terrain_vs",
            "terrain_fs",
            &[Some(TERRAIN_VERTEX_LAYOUT)],
            None,
            true,
            Some(wgpu::Face::Back),
        );
        let lit_color = color_pipeline(
            "shadowed-lit-color",
            lit_layout,
            &lit_module,
            "lit_vs",
            "lit_fs",
            &[Some(LIT_VERTEX_LAYOUT)],
            None,
            true,
            None,
        );
        let lit_alpha_color = color_pipeline(
            "shadowed-lit-alpha",
            lit_layout,
            &lit_module,
            "lit_vs",
            "lit_fs",
            &[Some(LIT_VERTEX_LAYOUT)],
            Some(ALPHA_BLEND),
            false,
            None,
        );
        let instanced_color = color_pipeline(
            "shadowed-instanced-color",
            lit_layout,
            &lit_module,
            "inst_vs",
            "lit_fs",
            &[Some(LIT_VERTEX_LAYOUT), Some(PART_INSTANCE_LAYOUT)],
            None,
            true,
            None,
        );
        let billboard_color = composite.billboard.as_ref().map(|_| {
            color_pipeline(
                "shadowed-billboard",
                lit_layout,
                &lit_module,
                "billboard_vs",
                "lit_fs",
                &[
                    Some(PARTICLE_QUAD_LAYOUT),
                    Some(PARTICLE_INSTANCE_LAYOUT),
                ],
                Some(ALPHA_BLEND),
                false,
                None,
            )
        });
        Self {
            config,
            caster_layout,
            array_view,
            compare_sampler: sampler,
            _texture: texture,
            layer_views,
            receiver_uniform,
            receiver_bg,
            caster_uniforms,
            caster_bgs,
            draw_ubo,
            draw_bg,
            draw_capacity,
            terrain_depth,
            terrain_region_depth,
            lit_depth,
            instanced_depth,
            terrain_color,
            terrain_region_color,
            lit_color,
            lit_alpha_color,
            instanced_color,
            billboard_color,
            draw_pool_epoch: 0,
            cascade_bundles: [None, None, None],
            cutout: None,
            stained: None,
        }
    }

    /// `MEGU_SHADOW_CUTOUT_DEPTH_WGSL` caster state, built on first use —
    /// sections with no `Layer::Cutout` quads never pay for it. Rebuilt
    /// automatically when the block atlas (its bind-group resources) is
    /// replaced.
    fn ensure_cutout(
        &mut self,
        device: &wgpu::Device,
        frame_layout: &wgpu::BindGroupLayout,
        draw_layout: &wgpu::BindGroupLayout,
        atlas: &AtlasGpu,
        atlas_epoch: u64,
    ) -> &CutoutGpu {
        if self
            .cutout
            .as_ref()
            .is_some_and(|c| c.atlas_epoch != atlas_epoch)
        {
            self.cutout = None;
        }
        if self.cutout.is_none() {
            // `@group(3)`: `{atlas_tex, atlas_sampler, atlas_rects,
            // atlas_states}` — the fragment alpha-tests the sprite each
            // `state_id` maps to through the states table.
            let atlas_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("cutout-atlas-layout"),
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
                    wgpu::BindGroupLayoutEntry {
                        binding: 2,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Buffer {
                            ty: wgpu::BufferBindingType::Storage { read_only: true },
                            has_dynamic_offset: false,
                            min_binding_size: None,
                        },
                        count: None,
                    },
                    wgpu::BindGroupLayoutEntry {
                        binding: 3,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Buffer {
                            ty: wgpu::BufferBindingType::Storage { read_only: true },
                            has_dynamic_offset: false,
                            min_binding_size: None,
                        },
                        count: None,
                    },
                ],
            });
            let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("cutout-atlas"),
                layout: &atlas_layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: wgpu::BindingResource::TextureView(&atlas.view),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::Sampler(&atlas.sampler),
                    },
                    wgpu::BindGroupEntry {
                        binding: 2,
                        resource: atlas.rects.as_entire_binding(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 3,
                        resource: atlas.state_sprites.as_entire_binding(),
                    },
                ],
            });
            // Groups: 0 = caster shadow uniform, 1 = the section's
            // `frame_layout` bind group (`ShadowSection.origin`), 2 =
            // unused draw pool slot, 3 = atlas resources.
            let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("cutout-caster-layout"),
                bind_group_layouts: &[
                    Some(&self.caster_layout),
                    Some(frame_layout),
                    Some(draw_layout),
                    Some(&atlas_layout),
                ],
                immediate_size: 0,
            });
            let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some("shadow-cutout-depth"),
                source: wgpu::ShaderSource::Wgsl(MEGU_SHADOW_CUTOUT_DEPTH_WGSL.into()),
            });
            let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("shadow-cutout-depth"),
                layout: Some(&pipeline_layout),
                vertex: wgpu::VertexState {
                    module: &module,
                    entry_point: Some("vs_cutout"),
                    compilation_options: wgpu::PipelineCompilationOptions::default(),
                    buffers: &[Some(TERRAIN_VERTEX_LAYOUT)],
                },
                primitive: wgpu::PrimitiveState {
                    topology: wgpu::PrimitiveTopology::TriangleList,
                    cull_mode: Some(wgpu::Face::Front),
                    ..Default::default()
                },
                depth_stencil: Some(wgpu::DepthStencilState {
                    format: DEPTH_FORMAT,
                    depth_write_enabled: Some(true),
                    depth_compare: Some(wgpu::CompareFunction::LessEqual),
                    stencil: wgpu::StencilState::default(),
                    bias: wgpu::DepthBiasState {
                        constant: 1,
                        slope_scale: 1.5,
                        clamp: 0.0,
                    },
                }),
                multisample: wgpu::MultisampleState::default(),
                fragment: Some(wgpu::FragmentState {
                    module: &module,
                    entry_point: Some("fs_cutout"),
                    compilation_options: wgpu::PipelineCompilationOptions::default(),
                    targets: &[],
                }),
                multiview_mask: None,
                cache: None,
            });
            self.cutout = Some(CutoutGpu {
                pipeline,
                bind_group,
                atlas_epoch,
            });
        }
        self.cutout.as_ref().expect("cutout ensured")
    }

    /// Lazy `MEGU_STAINED_GLASS_SHADOW_COLOR_WGSL` caster state — the
    /// RGBA shadow-color pass exists only while live stained casters
    /// exist, keeping the zero-glass case at zero cost.
    fn ensure_stained(&mut self, device: &wgpu::Device) -> &mut StainedGpu {
        if self.stained.is_none() {
            let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("stained-caster-layout"),
                bind_group_layouts: &[Some(&self.caster_layout)],
                immediate_size: 0,
            });
            let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some("stained-shadow-color"),
                source: wgpu::ShaderSource::Wgsl(MEGU_STAINED_GLASS_SHADOW_COLOR_WGSL.into()),
            });
            let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("stained-shadow-color"),
                layout: Some(&layout),
                vertex: wgpu::VertexState {
                    module: &module,
                    entry_point: Some("vs_stained"),
                    compilation_options: wgpu::PipelineCompilationOptions::default(),
                    buffers: &[Some(STAINED_CASTER_LAYOUT)],
                },
                primitive: wgpu::PrimitiveState {
                    topology: wgpu::PrimitiveTopology::TriangleList,
                    ..Default::default()
                },
                depth_stencil: Some(wgpu::DepthStencilState {
                    format: DEPTH_FORMAT,
                    depth_write_enabled: Some(false),
                    depth_compare: Some(wgpu::CompareFunction::LessEqual),
                    stencil: wgpu::StencilState::default(),
                    bias: wgpu::DepthBiasState {
                        constant: 2,
                        slope_scale: 1.5,
                        clamp: 0.0,
                    },
                }),
                multisample: wgpu::MultisampleState::default(),
                fragment: Some(wgpu::FragmentState {
                    module: &module,
                    entry_point: Some("fs_stained"),
                    compilation_options: wgpu::PipelineCompilationOptions::default(),
                    targets: &[Some(wgpu::ColorTargetState {
                        format: wgpu::TextureFormat::Rgba8Unorm,
                        // Multiplicative transmission: dst *= src.
                        blend: Some(wgpu::BlendState {
                            color: wgpu::BlendComponent {
                                src_factor: wgpu::BlendFactor::Zero,
                                dst_factor: wgpu::BlendFactor::Src,
                                operation: wgpu::BlendOperation::Add,
                            },
                            alpha: wgpu::BlendComponent {
                                src_factor: wgpu::BlendFactor::One,
                                dst_factor: wgpu::BlendFactor::Zero,
                                operation: wgpu::BlendOperation::Add,
                            },
                        }),
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                }),
                multiview_mask: None,
                cache: None,
            });
            let color_view = (0..self.config.cascade_count)
                .map(|_| {
                    let tex = device.create_texture(&wgpu::TextureDescriptor {
                        label: Some("stained-shadow-color"),
                        size: wgpu::Extent3d {
                            width: self.config.map_resolution,
                            height: self.config.map_resolution,
                            depth_or_array_layers: 1,
                        },
                        mip_level_count: 1,
                        sample_count: 1,
                        dimension: wgpu::TextureDimension::D2,
                        format: wgpu::TextureFormat::Rgba8Unorm,
                        usage: wgpu::TextureUsages::RENDER_ATTACHMENT
                            | wgpu::TextureUsages::TEXTURE_BINDING,
                        view_formats: &[],
                    });
                    tex.create_view(&wgpu::TextureViewDescriptor::default())
                })
                .collect();
            let vertices = device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("stained-casters"),
                size: (16 * std::mem::size_of::<StainedCasterVertex>()) as u64,
                usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            self.stained = Some(StainedGpu {
                pipeline,
                color_view,
                vertices,
                vertex_count: 0,
            });
        }
        self.stained.as_mut().expect("stained ensured")
    }

    /// Replace the staged stained-glass caster buffer (non-indexed
    /// world-space triangles — the caller triangulates quads).
    fn write_stained_casters(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        casters: &[StainedCasterVertex],
    ) {
        let stained = self.ensure_stained(device);
        if casters.is_empty() {
            stained.vertex_count = 0;
            return;
        }
        if (stained.vertex_count as usize) < casters.len() {
            stained.vertices = device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("stained-casters"),
                size: ((casters.len().next_power_of_two().max(16))
                    * std::mem::size_of::<StainedCasterVertex>()) as u64,
                usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
        }
        queue.write_buffer(&stained.vertices, 0, bytemuck::cast_slice(casters));
        stained.vertex_count = casters.len() as u32;
    }

    fn draw_bind_group(
        device: &wgpu::Device,
        layout: &wgpu::BindGroupLayout,
        buffer: &wgpu::Buffer,
    ) -> wgpu::BindGroup {
        device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("shadow-draw-bind-group"),
            layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                    buffer,
                    offset: 0,
                    size: wgpu::BufferSize::new(u64::from(DRAW_UNIFORM_STRIDE)),
                }),
            }],
        })
    }

    fn ensure_draw_capacity(
        &mut self,
        device: &wgpu::Device,
        layout: &wgpu::BindGroupLayout,
        needed: usize,
    ) {
        if needed <= self.draw_capacity {
            return;
        }
        self.draw_capacity = needed.next_power_of_two();
        self.draw_ubo = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("shadow-draw-pool"),
            size: self.draw_capacity as u64 * u64::from(DRAW_UNIFORM_STRIDE),
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        self.draw_bg = Self::draw_bind_group(device, layout, &self.draw_ubo);
        self.draw_pool_epoch += 1;
        // The recorded dynamic offsets address the old pool — every
        // cached cascade bundle must re-record against `draw_bg`.
        self.cascade_bundles = [None, None, None];
    }
}

/// Fullscreen sky pass: `MEGU_SKY_ATMOSPHERE_WGSL` draws one depth-1.0
/// triangle after all opaque geometry. `DepthCompare::Equal` (depth ==
/// 1.0 only, no depth write) means terrain-occluded pixels skip the
/// fragment stage entirely while untouched sky pixels get the full
/// atmospheric dome — the final image is unchanged versus a clear color.
/// Kept as a constant so the pipeline descriptor and the diagnostics
/// accessor can never drift apart.
pub const SKY_DEPTH_COMPARE: wgpu::CompareFunction = wgpu::CompareFunction::Equal;

struct SkyGpu {
    pipeline: wgpu::RenderPipeline,
    uniform: wgpu::Buffer,
    bind_group: wgpu::BindGroup,
}

impl SkyGpu {
    fn new(ctx: &GpuDevice, format: wgpu::TextureFormat) -> Self {
        let module = ctx
            .device
            .create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some("sky-atmosphere"),
                source: wgpu::ShaderSource::Wgsl(MEGU_SKY_ATMOSPHERE_WGSL.into()),
            });
        let layout = ctx
            .device
            .create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("sky-shadow-layout"),
                entries: &[wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: wgpu::BufferSize::new(
                            std::mem::size_of::<ShadowUniforms>() as u64,
                        ),
                    },
                    count: None,
                }],
            });
        let pipeline_layout = ctx
            .device
            .create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("sky-pipeline-layout"),
                bind_group_layouts: &[Some(&layout)],
                immediate_size: 0,
            });
        let pipeline = ctx
            .device
            .create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("sky-atmosphere-pipeline"),
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
                        format,
                        blend: None,
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                }),
                primitive: wgpu::PrimitiveState {
                    topology: wgpu::PrimitiveTopology::TriangleList,
                    cull_mode: None,
                    ..wgpu::PrimitiveState::default()
                },
                // Sky pixels sit at depth 1.0 and only fill fragments the
                // opaque world left untouched — occluded pixels never
                // reach the fragment stage.
                depth_stencil: Some(wgpu::DepthStencilState {
                    format: DEPTH_FORMAT,
                    depth_write_enabled: Some(false),
                    depth_compare: Some(SKY_DEPTH_COMPARE),
                    stencil: wgpu::StencilState::default(),
                    bias: wgpu::DepthBiasState::default(),
                }),
                multisample: wgpu::MultisampleState::default(),
                multiview_mask: None,
                cache: None,
            });
        let uniform = ctx.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("sky-shadow-uniform"),
            size: std::mem::size_of::<ShadowUniforms>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let bind_group = ctx.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("sky-shadow-bind-group"),
            layout: &layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: uniform.as_entire_binding(),
            }],
        });
        Self {
            pipeline,
            uniform,
            bind_group,
        }
    }
}

/// The reduced-resolution world target + its UI-blit-style bind group used
/// to upscale back to native resolution when `render_scale < 100%`.
struct ScaledTarget {
    _texture: wgpu::Texture,
    view: wgpu::TextureView,
    sampler: wgpu::Sampler,
    /// `MEGU_SPATIAL_SHARPEN_WGSL` group-0 for this target (built once
    /// the sharpen pipeline exists).
    sharpen_bg: Option<wgpu::BindGroup>,
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
    /// Camera position — seeds the [`GraphCuller`] search's camera section.
    camera_position: [f32; 3],
    /// Vertical focal length in pixels over `size` — the sub-pixel
    /// fast reject's `projected_sphere_diameter_px` argument.
    focal_y_px: f32,
    /// Authoritative server clock — the water pass derives its
    /// `ShadowUniforms` fallback (sun/moon direction, day phase) from it.
    world_time_ticks: u64,
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

    /// Sprite index for lit geometry's `atlas_index` and the
    /// state→sprite table. Resolves like `PackedAtlas::tile_rect`: exact
    /// name first, then the path part of a `namespace:path` id, then a
    /// `minecraft:`-prefixed guess.
    pub fn atlas_sprite_index(&self, name: &str) -> Option<u32> {
        sprite_index_of(&self.composite.atlas.sprite_indices, name)
    }

    /// Replace the GPU atlas and its sprite rectangle table atomically.
    /// The caller supplies consecutive mip levels built from `PackedAtlas`.
    pub fn upload_block_atlas(
        &mut self,
        atlas: &PackedAtlas,
        mips: &[AtlasMip],
    ) -> Result<(), String> {
        let limit = self.ctx.device.limits().max_texture_dimension_2d;
        if atlas.width == 0 || atlas.height == 0 || atlas.width > limit || atlas.height > limit {
            return Err(format!("atlas dimensions must be within 1..={limit}"));
        }
        let checked_len = |w: u32, h: u32| {
            usize::try_from(u64::from(w) * u64::from(h) * 4)
                .map_err(|_| "atlas byte length exceeds usize".to_string())
        };
        if atlas.rgba.len() != checked_len(atlas.width, atlas.height)? {
            return Err("atlas base RGBA byte length mismatch".into());
        }
        let mut expected_w = atlas.width;
        let mut expected_h = atlas.height;
        for (i, mip) in mips.iter().enumerate() {
            expected_w = (expected_w / 2).max(1);
            expected_h = (expected_h / 2).max(1);
            if mip.level != (i + 1) as u32
                || mip.width != expected_w
                || mip.height != expected_h
                || mip.rgba.len() != checked_len(expected_w, expected_h)?
            {
                return Err(format!("invalid atlas mip level {}", i + 1));
            }
            if expected_w == 1 && expected_h == 1 && i + 1 < mips.len() {
                return Err("atlas mip chain exceeds 1x1".into());
            }
        }
        let mut rects = Vec::<[f32; 4]>::new();
        let mut tiles = Vec::new();
        let mut sprite_indices = HashMap::new();
        for name in atlas.placements.keys() {
            let tile = atlas
                .tile_rect(name)
                .ok_or_else(|| format!("missing tile: {name}"))?;
            sprite_indices.insert(name.clone(), tiles.len() as u32);
            tiles.push(tile);
            // `SpriteFrame` slot: whole-strip rect plus a zeroed `anim`
            // aux — `set_atlas_animations` later narrows animated sprites
            // to their current frame window.
            rects.push([tile.u0, tile.v0, tile.u_scale, tile.v_scale]);
            rects.push([0.0; 4]);
        }
        if rects.is_empty() {
            return Err("atlas must contain at least one sprite".into());
        }
        if (rects.len() as u64) * 16 > self.ctx.device.limits().max_storage_buffer_binding_size {
            return Err("atlas rectangle table exceeds GPU storage buffer limit".into());
        }
        let device = &self.ctx.device;
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("block-atlas"),
            size: wgpu::Extent3d {
                width: atlas.width,
                height: atlas.height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1 + mips.len() as u32,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8UnormSrgb,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        for (level, (w, h, rgba)) in
            std::iter::once((atlas.width, atlas.height, atlas.rgba.as_slice()))
                .chain(mips.iter().map(|m| (m.width, m.height, m.rgba.as_slice())))
                .enumerate()
        {
            self.ctx.queue.write_texture(
                wgpu::TexelCopyTextureInfo {
                    texture: &texture,
                    mip_level: level as u32,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                rgba,
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(w * 4),
                    rows_per_image: Some(h),
                },
                wgpu::Extent3d {
                    width: w,
                    height: h,
                    depth_or_array_layers: 1,
                },
            );
        }
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("block-atlas-sampler"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::MipmapFilterMode::Linear,
            ..wgpu::SamplerDescriptor::default()
        });
        let rects_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("block-atlas-rects"),
            size: (rects.len() * 16) as u64,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        self.ctx
            .queue
            .write_buffer(&rects_buffer, 0, bytemuck::cast_slice(&rects));
        // A replacement atlas may assign different indices to the same states.
        // Reset mappings until the caller installs a matching state table.
        let state_sprites = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("block-state-atlas-lookup"),
            size: 4,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        self.ctx
            .queue
            .write_buffer(&state_sprites, 0, bytemuck::bytes_of(&u32::MAX));
        let gpu_atlas = AtlasGpu {
            _texture: texture,
            view,
            sampler,
            rects: rects_buffer,
            state_sprites,
            sprite_indices,
            tiles,
            animated: Vec::new(),
            anim_tick: None,
        };
        let bind_group = gpu_atlas.bind_group(device, &self.composite);
        self.composite.atlas = gpu_atlas;
        self.composite.env_bg = bind_group;
        self.atlas_epoch += 1;
        Ok(())
    }

    /// Bind packed terrain state IDs to atlas sprites. Unknown states remain
    /// untextured, so existing worlds render with their original vertex tint.
    /// Call again after changing atlases to refresh the state mapping.
    pub fn set_block_state_sprites(&mut self, assignments: &[(u32, &str)]) -> Result<(), String> {
        let sprite_indices = &self.composite.atlas.sprite_indices;
        let max_id = assignments.iter().map(|(id, _)| *id).max().unwrap_or(0);
        let bytes = (u64::from(max_id) + 1) * 4;
        if bytes > self.ctx.device.limits().max_storage_buffer_binding_size {
            return Err("block state texture table exceeds GPU storage buffer limit".into());
        }
        let mut states = vec![u32::MAX; (max_id as usize) + 1];
        for &(id, sprite) in assignments {
            states[id as usize] = sprite_index_of(sprite_indices, sprite)
                .ok_or_else(|| format!("unknown atlas sprite: {sprite}"))?;
        }
        let state_sprites = self.ctx.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("block-state-atlas-lookup"),
            size: bytes,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        self.ctx
            .queue
            .write_buffer(&state_sprites, 0, bytemuck::cast_slice(&states));
        self.composite.atlas.state_sprites = state_sprites;
        self.composite.env_bg = self
            .composite
            .atlas
            .bind_group(&self.ctx.device, &self.composite);
        Ok(())
    }

    /// Register `.mcmeta` playback tables for the uploaded atlas and write
    /// each animated sprite's `SpriteFrame` pair at `tick`. Sprites with no
    /// table keep the static whole-strip rect written at upload. Call again
    /// after `upload_block_atlas` — a new atlas resets the table. Returns
    /// the number of sprites that animate.
    pub fn set_atlas_animations(
        &mut self,
        animations: &BTreeMap<String, SpriteAnimation>,
        tick: u64,
    ) -> usize {
        let mut animated = Vec::new();
        for (name, animation) in animations {
            let Some(index) = sprite_index_of(&self.composite.atlas.sprite_indices, name) else {
                continue;
            };
            let entry = AnimatedSprite {
                index,
                tile: self.composite.atlas.tiles[index as usize],
                animation: animation.clone(),
            };
            let (rect, aux) = entry.animation.rect_pair_at_tick(&entry.tile, tick);
            self.ctx.queue.write_buffer(
                &self.composite.atlas.rects,
                u64::from(index) * ATLAS_SPRITE_FRAME_BYTES,
                bytemuck::cast_slice(&[rect, aux]),
            );
            animated.push(entry);
        }
        animated.sort_by_key(|entry| entry.index);
        let count = animated.len();
        self.composite.atlas.animated = animated;
        self.composite.atlas.anim_tick = Some(tick);
        count
    }

    /// Advance every registered `.mcmeta` animation to `tick`: one 32-byte
    /// `SpriteFrame` rewrite per animated sprite. Costs a single compare
    /// while `tick` repeats the value already in the table, so calling at
    /// render rate is free when the animation clock has not advanced.
    pub fn tick_atlas_animations(&mut self, tick: u64) {
        if self.composite.atlas.anim_tick == Some(tick) {
            return;
        }
        self.composite.atlas.anim_tick = Some(tick);
        let atlas = &self.composite.atlas;
        for entry in &atlas.animated {
            let (rect, aux) = entry.animation.rect_pair_at_tick(&entry.tile, tick);
            self.ctx.queue.write_buffer(
                &atlas.rects,
                u64::from(entry.index) * ATLAS_SPRITE_FRAME_BYTES,
                bytemuck::cast_slice(&[rect, aux]),
            );
        }
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
        let terrain_region_pipeline =
            Self::build_terrain_region_pipeline(&ctx.device, &pipeline_layout, target_format);
        let empty_vertex_buffer = ctx.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("empty-vertex"),
            size: 16,
            usage: wgpu::BufferUsages::VERTEX,
            mapped_at_creation: false,
        });
        let empty_index_buffer = ctx.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("empty-index"),
            size: 16,
            usage: wgpu::BufferUsages::INDEX,
            mapped_at_creation: false,
        });

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
        // GPU UI quad path: `UI_QUAD_WGSL` + the static built-in font atlas
        // uploaded once — `update_ui_mesh` only rewrites the small dynamic
        // vertex/index buffers and the screen-size uniform.
        let ui_quad_layout =
            ctx.device
                .create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                    label: Some("ui-quad-layout"),
                    entries: &[
                        uniform_entry(0),
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
        let ui_quad_pipeline_layout =
            ctx.device
                .create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                    label: Some("ui-quad-pipeline-layout"),
                    bind_group_layouts: &[Some(&ui_quad_layout)],
                    immediate_size: 0,
                });
        let ui_quad_module = ctx
            .device
            .create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some("ui-quad"),
                source: wgpu::ShaderSource::Wgsl(Cow::Borrowed(megu_ui::draw::UI_QUAD_WGSL)),
            });
        let ui_quad_pipeline = ctx
            .device
            .create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("ui-quad-pipeline"),
                layout: Some(&ui_quad_pipeline_layout),
                vertex: wgpu::VertexState {
                    module: &ui_quad_module,
                    entry_point: Some("vs_main"),
                    compilation_options: wgpu::PipelineCompilationOptions::default(),
                    buffers: &[Some(UI_QUAD_VERTEX_LAYOUT)],
                },
                primitive: wgpu::PrimitiveState {
                    topology: wgpu::PrimitiveTopology::TriangleList,
                    cull_mode: None,
                    ..wgpu::PrimitiveState::default()
                },
                depth_stencil: None,
                multisample: wgpu::MultisampleState::default(),
                fragment: Some(wgpu::FragmentState {
                    module: &ui_quad_module,
                    entry_point: Some("fs_main"),
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
        let ui_quad_uniform = ctx.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("ui-quad-uniform"),
            size: 16,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let ui_font_texture = ctx.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("ui-font-atlas"),
            size: wgpu::Extent3d {
                width: FONT_ATLAS_WIDTH,
                height: FONT_ATLAS_HEIGHT,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        ctx.queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &ui_font_texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            &font_atlas_rgba(),
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(FONT_ATLAS_WIDTH * 4),
                rows_per_image: Some(FONT_ATLAS_HEIGHT),
            },
            wgpu::Extent3d {
                width: FONT_ATLAS_WIDTH,
                height: FONT_ATLAS_HEIGHT,
                depth_or_array_layers: 1,
            },
        );
        let ui_font_view = ui_font_texture.create_view(&wgpu::TextureViewDescriptor::default());
        let ui_font_sampler = ctx.device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("ui-font-sampler"),
            mag_filter: wgpu::FilterMode::Nearest,
            min_filter: wgpu::FilterMode::Nearest,
            ..wgpu::SamplerDescriptor::default()
        });
        let ui_quad_bind_group = ctx.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("ui-quad-bind-group"),
            layout: &ui_quad_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: ui_quad_uniform.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(&ui_font_view),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::Sampler(&ui_font_sampler),
                },
            ],
        });
        let ui_quad_vertices = ctx.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("ui-quad-vertices"),
            size: 4096,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let ui_quad_indices = ctx.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("ui-quad-indices"),
            size: 4096,
            usage: wgpu::BufferUsages::INDEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let ui_quad = UiQuadGpu {
            pipeline: ui_quad_pipeline,
            bind_group: ui_quad_bind_group,
            uniform: ui_quad_uniform,
            _font_texture: ui_font_texture,
            vertices: ui_quad_vertices,
            indices: ui_quad_indices,
            vertex_capacity: 4096,
            index_capacity: 4096,
            index_count: 0,
        };
        // Runtime `MEGU EDITION` title branding: two quads (wordmark +
        // edition strip) reusing the `ui_quad` pipeline with their own
        // texture bind groups, plus grow-only splash glyph buffers drawn
        // against the shared font atlas.
        let title_branding_vertices = ctx.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("title-branding-vertices"),
            size: 8 * 32,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let title_branding_indices = ctx.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("title-branding-indices"),
            size: 48,
            usage: wgpu::BufferUsages::INDEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        ctx.queue.write_buffer(
            &title_branding_indices,
            0,
            bytemuck::cast_slice(&[0u32, 1, 2, 0, 2, 3, 4, 5, 6, 4, 6, 7]),
        );
        let title_branding = TitleBrandingGpu {
            layout: ui_quad_layout.clone(),
            logo: None,
            edition: None,
            vertices: title_branding_vertices,
            indices: title_branding_indices,
            logo_draw: false,
            edition_draw: false,
            splash_vertices: ctx.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("title-splash-vertices"),
                size: 4096,
                usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            }),
            splash_indices: ctx.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("title-splash-indices"),
                size: 4096,
                usage: wgpu::BufferUsages::INDEX | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            }),
            splash_vertex_capacity: 4096,
            splash_index_capacity: 4096,
            splash_index_count: 0,
            mesh_key: None,
            vertex_scratch: Vec::new(),
        };
        let composite = Self::build_composite(&ctx, target_format, &frame_layout, &frame_ubo);
        let particle_instancing = composite.billboard.is_some();
        // Shared stand-in uniform + bind group for sections the composed
        // pass never draws directly (arena-resident opaque with no
        // translucent stream) — one allocation total, not one per section.
        let empty_section_uniform = ctx.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("empty-section-uniform"),
            size: std::mem::size_of::<SectionUniforms>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        ctx.queue.write_buffer(
            &empty_section_uniform,
            0,
            bytemuck::bytes_of(&SectionUniforms {
                origin: [0.0; 3],
                _pad0: 0.0,
                biome_tint: [1.0; 3],
                biome_tint_strength: 0.0,
            }),
        );
        let empty_bind_group = ctx.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("empty-section-bind-group"),
            layout: &frame_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: frame_ubo.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: empty_section_uniform.as_entire_binding(),
                },
            ],
        });
        let debug_line_module = ctx
            .device
            .create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some("debug-line-shader"),
                source: wgpu::ShaderSource::Wgsl(Cow::Borrowed(DEBUG_LINE_WGSL)),
            });
        let debug_line_layout =
            ctx.device
                .create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                    label: Some("debug-line-pipeline-layout"),
                    bind_group_layouts: &[Some(&composite.lit_frame_layout)],
                    immediate_size: 0,
                });
        let debug_line_pipeline =
            ctx.device
                .create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                    label: Some("debug-3d-lines"),
                    layout: Some(&debug_line_layout),
                    vertex: wgpu::VertexState {
                        module: &debug_line_module,
                        entry_point: Some("debug_line_vs"),
                        compilation_options: wgpu::PipelineCompilationOptions::default(),
                        buffers: &[Some(DEBUG_LINE_VERTEX_LAYOUT)],
                    },
                    primitive: wgpu::PrimitiveState {
                        topology: wgpu::PrimitiveTopology::LineList,
                        cull_mode: None,
                        ..wgpu::PrimitiveState::default()
                    },
                    depth_stencil: Some(depth_state(false)),
                    multisample: wgpu::MultisampleState::default(),
                    fragment: Some(wgpu::FragmentState {
                        module: &debug_line_module,
                        entry_point: Some("debug_line_fs"),
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
        // Seed the non-visual strategy outputs at construction: the same
        // pure `select_compute_strategy` the live session runs, so the
        // upload-budget preset and mesh-worker hint match the session's
        // `compute_strategy` without a window-loop call site. The
        // universal profile additionally carries the GPU paradigm the
        // depth-store policy consults (`TileBasedTbdr` → depth discard).
        let profile = detect_universal_hardware_profile();
        let caps = profile.base;
        let seeded_strategy = select_compute_strategy(&caps);
        let (upload_budget, mesh_worker_threads) = strategy_upload_defaults(&seeded_strategy);
        // Transfer-layer sizing keyed by the detected memory topology x
        // RAM class — strictly bus parameters, never visual quality.
        let memory_footprint_class = profile.memory_class;
        let transfer_budget =
            TransferTopologyBudget::for_topology(seeded_strategy.memory, memory_footprint_class);
        let indirect_execution_supported = ctx
            .adapter
            .get_downlevel_capabilities()
            .flags
            .contains(wgpu::DownlevelFlags::INDIRECT_EXECUTION);
        let staging_ring = UploadStagingRing::new(&ctx.device, &transfer_budget);
        // The shared static quad index buffer starts at one slab of the
        // canonical `[0,1,2,0,2,3]` pattern; `flush_region_arenas` grows
        // it geometrically past the largest referenced extent.
        let shared_quad_index_capacity = 384u32;
        let shared_quad_index_buffer = ctx.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("shared-quad-indices"),
            size: u64::from(shared_quad_index_capacity) * 4,
            usage: wgpu::BufferUsages::INDEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        ctx.queue.write_buffer(
            &shared_quad_index_buffer,
            0,
            bytemuck::cast_slice(&quad_index_pattern(shared_quad_index_capacity)),
        );
        // Low-core / cache-poor hosts: one background worker keeps
        // far-mesh generation off the render thread without letting the
        // pool compete for the few cores the render thread needs.
        let mesh_worker_threads = if caps.logical_cores <= 4
            || (caps.l3_cache_kib > 0 && caps.l3_cache_kib <= 4 * 1024)
        {
            mesh_worker_threads.min(1)
        } else {
            mesh_worker_threads
        };
        let gpu_paradigm = profile.gpu_paradigm;
        let sky = SkyGpu::new(&ctx, target_format);
        Self {
            ctx,
            target_format,
            frame_layout,
            frame_bind_group,
            frame_ubo,
            section_ubo,
            terrain_pipeline,
            translucent_pipeline,
            terrain_region_pipeline,
            ui_pipeline,
            ui_layout,
            ui_bind_group: None,
            ui_texture: None,
            ui_view: None,
            ui_sampler,
            ui_size: (0, 0),
            ui_quad,
            title_branding,
            ui_quad_vertex_scratch: Vec::new(),
            ui_gpu_quad_frames: 0,
            title_branding_frames: 0,
            ui_cpu_raster_uploads: 0,
            depth_texture: None,
            depth_view: None,
            depth_size: (0, 0),
            window_camera: Cell::new(None),
            scene: None,
            live_sections: HashMap::new(),
            upload_budget,
            pending_uploads: BTreeMap::new(),
            section_graphs: HashMap::new(),
            hiz_pyramid: None,
            hiz_source_vp: None,
            hiz_manual: false,
            hiz_dirty: true,
            hiz_revision: 0,
            cull_world_revision: 0,
            cull_graphs_stamp: 0,
            cull_result: HashSet::new(),
            cull_graph_scratch: HashSet::new(),
            cull_cache_key: None,
            cull_hiz_count: 0,
            cull_subpixel_count: 0,
            cull_cache_hits: 0,
            instanced_kinetic_draws: 0,
            region_arenas: BTreeMap::new(),
            arena_enabled: true,
            shared_quad_index_buffer,
            shared_quad_index_capacity,
            staging_ring,
            gpu_hiz: None,
            gpu_hiz_cull_calls: 0,
            gpu_hiz_culled: 0,
            gpu_indirect_enabled: false,
            indirect_execution_supported,
            gpu_indirect: None,
            morton_order_enabled: true,
            gpu_hiz_frame_vp: None,
            gpu_indirect_draw_passes: 0,
            gpu_indirect_sections_submitted: 0,
            gpu_indirect_record_scratch: Vec::new(),
            gpu_indirect_coord_scratch: Vec::new(),
            gpu_indirect_origin_scratch: Vec::new(),
            gpu_indirect_record_uploads: 0,
            gpu_indirect_record_bytes_uploaded: 0,
            gpu_indirect_record_uploads_skipped: 0,
            gpu_hiz_pyramid_builds: 0,
            upload_budget_p95_observations: 0,
            transfer_budget,
            memory_footprint_class,
            staging_ring_uploads: 0,
            staging_ring_allocs: 0,
            staging_ring_dma_defers: 0,
            empty_vertex_buffer,
            empty_index_buffer,
            mesh_pool: MeshJobPool::new(mesh_worker_threads),
            completed_geometry: HashMap::new(),
            upload_frame_ms: VecDeque::new(),
            last_render_instant: None,
            mesh_worker_threads,
            compute_strategy_apply_count: 0,
            admitted_uploads: 0,
            deferred_uploads: 0,
            last_frame_culled_sections: 0,
            culled_sections: 0,
            bg_mesh_jobs_submitted: 0,
            bg_mesh_jobs_completed: 0,
            hiz_frames_updated: 0,
            hiz_culled_sections: 0,
            subpixel_culled_sections: 0,
            direction_buckets_skipped: 0,
            region_arena_draws: 0,
            bind_group_switches_saved: 0,
            terrain_bundle: None,
            terrain_bundle_key: None,
            terrain_bundle_stats: (0, 0, 0, 0),
            terrain_bundle_hits: 0,
            terrain_bundle_records: 0,
            ui_bundle: None,
            ui_bundle_key: None,
            ui_bundle_hits: 0,
            ubo_writes_skipped: 0,
            region_partial_uploads: 0,
            cpu_opaque_bytes_freed: 0,
            last_frame_ubo_bytes: Vec::new(),
            last_env_ubo_bytes: Vec::new(),
            last_lights_ubo_bytes: Vec::new(),
            empty_section_uniform,
            empty_bind_group,
            scene_revision: 0,
            player_position: [0.0; 3],
            composite,
            debug_line_pipeline,
            debug_lines: None,
            entity_ber: None,
            particles: None,
            particle_alphas: Vec::new(),
            particle_instances: None,
            particle_instance_scratch: Vec::new(),
            particle_instancing,
            particle_instance_uploads: 0,
            particle_instances_capped: 0,
            particle_cpu_fallbacks: 0,
            dynamic_ring_reuses: 0,
            dynamic_buffer_allocs: 0,
            subworld_gpu: SubWorldGpuCache::new(),
            scaled_target: None,
            sharpen: None,
            water: None,
            lit_terrain: None,
            shadow_gpu_epoch: 0,
            pending_stained_casters: Vec::new(),
            uploaded_section_coords: Vec::new(),
            atlas_epoch: 0,
            shadow_quality: BuiltinShadowQuality::Off,
            render_scale: RenderScalePreset::default(),
            shadow_cache: DirtyOnlyShadowCache::new(),
            shadow: None,
            shadow_active: false,
            last_shadow_camera: None,
            last_shadow_casters: EMPTY_CASTER_DIGEST,
            subworld_gpu_epoch: 0,
            shadow_rerender_count: 0,
            shadow_reuse_count: 0,
            frame_stable_shadow: None,
            rendered_orthos: [CascadeOrtho {
                center: [0.0; 3],
                radius: 0.0,
                depth_radius: 0.0,
                axes: [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]],
            }; 3],
            rendered_lvp: [[[0.0; 4]; 4]; 3],
            pending_cascade_layers: 0,
            shadow_rotation_reuses: 0,
            shadow_regions_culled: 0,
            sky,
            gpu_paradigm,
            tbdr_depth_discards: 0,
            last_shadow_decision: ShadowCacheDecision::Rerender(
                megu_client_mesh::builtin_shadow_wgsl::ShadowDirtyReason::FirstFrame,
            ),
            shaderpack: ShaderpackAttempt::Fallback {
                notice: "built-in terrain pipeline".to_string(),
            },
            pack_subworld: None,
            notice: None,
        }
    }

    /// Upload world-space F3+G/F3+B segments. Invalid segments are omitted,
    /// and uploads are capped to keep remote diagnostics bounded.
    pub fn set_debug_lines(&mut self, lines: &[DebugLine]) {
        let mut vertices = Vec::with_capacity(lines.len().min(MAX_DEBUG_LINES) * 2);
        for line in lines {
            if vertices.len() >= MAX_DEBUG_LINES * 2 {
                break;
            }
            if !line
                .start
                .iter()
                .chain(line.end.iter())
                .all(|v| v.is_finite())
            {
                continue;
            }
            let color = line.rgba.map(|v| f32::from(v) / 255.0);
            vertices.push(DebugLineVertex {
                position: line.start,
                color,
            });
            vertices.push(DebugLineVertex {
                position: line.end,
                color,
            });
        }
        self.debug_lines = if vertices.is_empty() {
            None
        } else {
            let buffer = self.ctx.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("debug-line-vertices"),
                size: std::mem::size_of_val(vertices.as_slice()) as u64,
                usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            self.ctx
                .queue
                .write_buffer(&buffer, 0, bytemuck::cast_slice(&vertices));
            Some((buffer, vertices.len() as u32))
        };
    }

    /// Batch articulated entity rigs and Vanilla block entity meshes into
    /// one GPU vertex/index stream, using the existing lit shader and atlas.
    pub fn set_entity_and_ber_meshes(&mut self, meshes: Vec<(&ExtrudedMesh, [f32; 3], [f32; 3])>) {
        let mut vertices = Vec::new();
        let mut indices = Vec::new();
        'meshes: for (mesh, origin, tint) in meshes {
            if !origin.iter().chain(tint.iter()).all(|v| v.is_finite()) {
                continue;
            }
            for quad in &mesh.quads {
                if vertices.len() / 4 >= MAX_DYNAMIC_QUADS {
                    break 'meshes;
                }
                let positions = quad.positions.map(|point| {
                    [
                        point[0] + origin[0],
                        point[1] + origin[1],
                        point[2] + origin[2],
                    ]
                });
                append_dynamic_quad(
                    &mut vertices,
                    &mut indices,
                    positions,
                    quad.normal,
                    quad.uv,
                    tint,
                );
            }
        }
        let (allocs, reuses) =
            stage_dynamic_geometry(&self.ctx, &mut self.entity_ber, &vertices, &indices);
        self.dynamic_buffer_allocs += allocs;
        self.dynamic_ring_reuses += reuses;
    }

    /// Upload precomputed camera-facing particle quads with individual alpha.
    ///
    /// When [`Self::particle_instancing`] is on, each quad packs into one
    /// 76-byte [`ParticleInstance`] — center, half-edge axes (the
    /// billboard right/up scaled by half extent), affine UV basis and
    /// tint+alpha — staged into the grow-only instance ring, so the
    /// vertex shader expands the static unit quad instead of the CPU
    /// uploading four corners + six indices per particle. Quads outside
    /// the bilinear model (`ParticleBillboardPack::CpuOnly`), or every
    /// quad when instancing is off, take the original CPU staging path
    /// with identical output, ordering and blending.
    pub fn set_particle_billboards(&mut self, quads: &[VanillaParticleQuad]) {
        self.particle_alphas.clear();
        self.particle_instance_scratch.clear();
        self.particle_instances_capped +=
            quads.len().saturating_sub(MAX_DYNAMIC_QUADS) as u64;
        if self.particle_instancing {
            let mut cpu_only = false;
            for quad in quads.iter().take(MAX_DYNAMIC_QUADS) {
                match pack_particle_billboard(quad) {
                    ParticleBillboardPack::Skip => {}
                    ParticleBillboardPack::Instanced(instance) => {
                        self.particle_instance_scratch.push(instance);
                    }
                    ParticleBillboardPack::CpuOnly => cpu_only = true,
                }
            }
            if !cpu_only {
                self.stage_billboard_scratch();
                return;
            }
            self.particle_cpu_fallbacks += 1;
            self.particle_instance_scratch.clear();
        } else {
            self.particle_cpu_fallbacks += 1;
        }
        if let Some(r) = self.particle_instances.as_mut() {
            r.count = 0;
        }
        let mut vertices = Vec::new();
        let mut indices = Vec::new();
        for quad in quads.iter().take(MAX_DYNAMIC_QUADS) {
            if !quad.color.iter().all(|v| v.is_finite()) {
                continue;
            }
            let u = [
                quad.positions[1][0] - quad.positions[0][0],
                quad.positions[1][1] - quad.positions[0][1],
                quad.positions[1][2] - quad.positions[0][2],
            ];
            let v = [
                quad.positions[3][0] - quad.positions[0][0],
                quad.positions[3][1] - quad.positions[0][1],
                quad.positions[3][2] - quad.positions[0][2],
            ];
            let n = [
                u[1] * v[2] - u[2] * v[1],
                u[2] * v[0] - u[0] * v[2],
                u[0] * v[1] - u[1] * v[0],
            ];
            let length = (n[0] * n[0] + n[1] * n[1] + n[2] * n[2]).sqrt();
            if !length.is_finite() || length <= f32::EPSILON {
                continue;
            }
            let normal = n.map(|component| component / length);
            if append_dynamic_quad(
                &mut vertices,
                &mut indices,
                quad.positions,
                normal,
                quad.uv,
                [quad.color[0], quad.color[1], quad.color[2]],
            ) {
                self.particle_alphas.push(quad.color[3].clamp(0.0, 1.0));
            }
        }
        let (allocs, reuses) =
            stage_dynamic_geometry(&self.ctx, &mut self.particles, &vertices, &indices);
        self.dynamic_buffer_allocs += allocs;
        self.dynamic_ring_reuses += reuses;
    }

    /// Convert the Vanilla particle simulation to camera-facing quads.
    ///
    /// The instanced branch packs each live particle's center +
    /// [`VanillaParticle::billboard_half_size`] ×
    /// [`VanillaParticleWorld::billboard_axes`] straight into
    /// [`ParticleInstance`] records — no intermediate
    /// `VanillaParticleQuad` expansion at all.
    pub fn set_particle_world(
        &mut self,
        world: &VanillaParticleWorld,
        camera_right: [f32; 3],
        camera_up: [f32; 3],
    ) {
        if self.particle_instancing {
            self.particle_alphas.clear();
            self.particle_instance_scratch.clear();
            let (right, up) = VanillaParticleWorld::billboard_axes(camera_right, camera_up);
            let particles = world.particles();
            self.particle_instances_capped +=
                particles.len().saturating_sub(MAX_DYNAMIC_QUADS) as u64;
            for particle in particles.iter().take(MAX_DYNAMIC_QUADS) {
                let size = particle.billboard_half_size();
                if !particle.pos.iter().all(|v| v.is_finite())
                    || !particle.color.iter().all(|v| v.is_finite())
                    || !size.is_finite()
                {
                    continue;
                }
                self.particle_instance_scratch.push(ParticleInstance {
                    center: particle.pos,
                    axis_u: right.map(|r| r * size),
                    axis_v: up.map(|u| u * size),
                    // `billboard_quads`' unit-square UV basis.
                    uv_origin: [0.0, 1.0],
                    uv_du: [1.0, 0.0],
                    uv_dv: [0.0, -1.0],
                    color: particle.color.map(|c| c.clamp(0.0, 1.0)),
                });
            }
            self.stage_billboard_scratch();
            return;
        }
        self.set_particle_billboards(&world.billboard_quads(camera_right, camera_up));
    }

    /// Enable/disable the instanced billboard path (default: on when the
    /// device limits allow it — see `particle_instancing_supported`).
    /// `false` forces the CPU-staged quad path; output, ordering and
    /// blending are identical either way. Enabling on a device whose
    /// limits could not build the billboard pipeline is a no-op.
    pub fn set_particle_instancing_enabled(&mut self, enabled: bool) {
        self.particle_instancing = enabled && self.composite.billboard.is_some();
    }

    /// Finish an instanced `set_particle_*` call: park the CPU ring's
    /// draw count, upload the scratch into the billboard instance ring,
    /// and fold the ring stats into the shared dynamic-geometry counters.
    fn stage_billboard_scratch(&mut self) {
        if let Some(r) = self.particles.as_mut() {
            r.index_count = 0;
        }
        self.particle_instance_uploads += self.particle_instance_scratch.len() as u64;
        let (allocs, reuses) = stage_particle_instances(
            &self.ctx,
            &mut self.particle_instances,
            &self.particle_instance_scratch,
        );
        self.dynamic_buffer_allocs += allocs;
        self.dynamic_ring_reuses += reuses;
    }

    /// The [`ParticleInstance`] records staged for the current frame —
    /// non-empty only on the instanced path (the CPU quad path leaves it
    /// cleared). Exposed for tests and diagnostics.
    pub fn staged_particle_instances(&self) -> &[ParticleInstance] {
        &self.particle_instance_scratch
    }

    /// Whether the instanced billboard path is currently active —
    /// `false` means `set_particle_*` calls stage CPU quads instead.
    pub fn particle_instancing_enabled(&self) -> bool {
        self.particle_instancing
    }

    /// Draw-uniform slots the particle stream takes this frame — one
    /// slot for a live instanced billboard draw, or one per staged quad
    /// on the CPU path.
    fn particle_draw_slots(&self) -> usize {
        if let Some(r) = self.particle_instances.as_ref() {
            if r.count > 0 {
                return 1;
            }
        }
        self.particle_alphas.len()
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
            entries: &[
                uniform_entry(0),
                uniform_entry(1),
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 3,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
                // Bindings 4-5 are visible to the vertex stage too: the
                // composed terrain shaders hoist the per-quad atlas
                // state/rect lookup into `terrain_vs`/`terrain_translucent_vs`.
                // Binding 4 elements are `SpriteFrame` pairs (32 B), not
                // bare rects — the layout min size must match the WGSL.
                wgpu::BindGroupLayoutEntry {
                    binding: 4,
                    visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Storage { read_only: true },
                        has_dynamic_offset: false,
                        min_binding_size: wgpu::BufferSize::new(ATLAS_SPRITE_FRAME_BYTES),
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 5,
                    visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Storage { read_only: true },
                        has_dynamic_offset: false,
                        min_binding_size: wgpu::BufferSize::new(4),
                    },
                    count: None,
                },
            ],
        });
        let lit_frame_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("lit-frame-layout"),
            entries: &[uniform_entry(0)],
        });
        // The receiver layout lives on `CompositeGpu` (not `ShadowGpu`)
        // so pack `gbuffers_*` sub-world pipelines can share the exact
        // layout object — a pack shader declaring group 3 then binds the
        // live `shadow.receiver_bg` with zero re-creation.
        let shadow_receiver_layout =
            device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("shadow-receiver-layout"),
                entries: &[
                    uniform_entry(0),
                    wgpu::BindGroupLayoutEntry {
                        binding: 1,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Texture {
                            sample_type: wgpu::TextureSampleType::Depth,
                            view_dimension: wgpu::TextureViewDimension::D2Array,
                            multisampled: false,
                        },
                        count: None,
                    },
                    wgpu::BindGroupLayoutEntry {
                        binding: 2,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Comparison),
                        count: None,
                    },
                ],
            });
        let pack_lit_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("pack-lit-layout"),
            bind_group_layouts: &[
                Some(&lit_frame_layout),
                Some(&draw_layout),
                Some(&env_layout),
                Some(&shadow_receiver_layout),
            ],
            immediate_size: 0,
        });

        let terrain_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("composite-terrain-layout"),
            bind_group_layouts: &[Some(frame_layout), Some(&draw_layout), Some(&env_layout)],
            immediate_size: 0,
        });
        let lit_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("composite-lit-layout"),
            bind_group_layouts: &[
                Some(&lit_frame_layout),
                Some(&draw_layout),
                Some(&env_layout),
            ],
            immediate_size: 0,
        });

        let terrain_module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("composite-terrain"),
            source: wgpu::ShaderSource::Wgsl(Cow::Borrowed(COMPOSITE_TERRAIN_WGSL)),
        });
        let terrain_region_module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("composite-terrain-region"),
            source: wgpu::ShaderSource::Wgsl(Cow::Owned(region_opaque_wgsl(
                COMPOSITE_TERRAIN_WGSL,
            ))),
        });
        let lit_module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("composite-lit"),
            source: wgpu::ShaderSource::Wgsl(Cow::Borrowed(COMPOSITE_LIT_WGSL)),
        });

        let composite_pipeline =
            |label, layout, module, vs, fs, buffers, blend, depth_write, cull| {
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
                        cull_mode: cull,
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
            Some(wgpu::Face::Back),
        );
        let terrain_region_pipeline = composite_pipeline(
            "composite-terrain-region",
            &terrain_layout,
            &terrain_region_module,
            "terrain_vs",
            "terrain_fs",
            &[Some(TERRAIN_VERTEX_LAYOUT)],
            None,
            true,
            Some(wgpu::Face::Back),
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
            None,
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
            None,
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
            None,
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
            None,
        );
        let billboard = if particle_instancing_supported(device) {
            let pipeline = composite_pipeline(
                "composite-billboard",
                &lit_layout,
                &lit_module,
                "billboard_vs",
                "lit_fs",
                &[
                    Some(PARTICLE_QUAD_LAYOUT),
                    Some(PARTICLE_INSTANCE_LAYOUT),
                ],
                Some(ALPHA_BLEND),
                false,
                None,
            );
            let quad_vertices = device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("particle-billboard-quad"),
                size: std::mem::size_of_val(&PARTICLE_BILLBOARD_CORNERS) as u64,
                usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            ctx.queue.write_buffer(
                &quad_vertices,
                0,
                bytemuck::cast_slice(&PARTICLE_BILLBOARD_CORNERS),
            );
            let quad_indices = device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("particle-billboard-quad-indices"),
                size: std::mem::size_of_val(&PARTICLE_BILLBOARD_INDICES) as u64,
                usage: wgpu::BufferUsages::INDEX | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            ctx.queue.write_buffer(
                &quad_indices,
                0,
                bytemuck::cast_slice(&PARTICLE_BILLBOARD_INDICES),
            );
            Some(ParticleBillboardGpu {
                pipeline,
                quad_vertices,
                quad_indices,
            })
        } else {
            None
        };

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
        let atlas_texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("block-atlas-white-fallback"),
            size: wgpu::Extent3d {
                width: 1,
                height: 1,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8UnormSrgb,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        ctx.queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &atlas_texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            &[255, 255, 255, 255],
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(4),
                rows_per_image: Some(1),
            },
            wgpu::Extent3d {
                width: 1,
                height: 1,
                depth_or_array_layers: 1,
            },
        );
        let atlas_rects = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("block-atlas-fallback-rect"),
            size: ATLAS_SPRITE_FRAME_BYTES,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        ctx.queue.write_buffer(
            &atlas_rects,
            0,
            bytemuck::cast_slice(&[[0.0f32, 0.0, 1.0, 1.0], [0.0; 4]]),
        );
        let state_sprites = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("block-atlas-empty-state-lookup"),
            size: 4,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        ctx.queue
            .write_buffer(&state_sprites, 0, bytemuck::bytes_of(&u32::MAX));
        let atlas = AtlasGpu {
            view: atlas_texture.create_view(&wgpu::TextureViewDescriptor::default()),
            sampler: device.create_sampler(&wgpu::SamplerDescriptor {
                label: Some("block-atlas-fallback-sampler"),
                mag_filter: wgpu::FilterMode::Linear,
                min_filter: wgpu::FilterMode::Linear,
                ..wgpu::SamplerDescriptor::default()
            }),
            _texture: atlas_texture,
            rects: atlas_rects,
            state_sprites,
            sprite_indices: HashMap::new(),
            tiles: Vec::new(),
            animated: Vec::new(),
            anim_tick: None,
        };
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
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::TextureView(&atlas.view),
                },
                wgpu::BindGroupEntry {
                    binding: 3,
                    resource: wgpu::BindingResource::Sampler(&atlas.sampler),
                },
                wgpu::BindGroupEntry {
                    binding: 4,
                    resource: atlas.rects.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 5,
                    resource: atlas.state_sprites.as_entire_binding(),
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
            terrain_region_pipeline,
            terrain_alpha_pipeline,
            lit_pipeline,
            lit_alpha_pipeline,
            instanced_pipeline,
            billboard,
            lit_frame_bg,
            lit_frame_layout,
            shadow_receiver_layout,
            pack_lit_layout,
            env_layout,
            draw_layout,
            draw_bg,
            draw_ubo,
            draw_capacity,
            draw_scratch: Vec::with_capacity(64),
            terrain_draw_scratch: Vec::with_capacity(64),
            env_ubo,
            lights_ubo,
            env_bg,
            atlas,
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
            &[Some(TERRAIN_VERTEX_LAYOUT)],
            translucent,
            label,
        )
    }

    /// The simple-path region variant: `TERRAIN_OPAQUE_WGSL` with the
    /// section-in-region offset decode substituted in (same 16-byte
    /// [`TERRAIN_VERTEX_LAYOUT`] stream as the per-section path).
    fn build_terrain_region_pipeline(
        device: &wgpu::Device,
        layout: &wgpu::PipelineLayout,
        target_format: wgpu::TextureFormat,
    ) -> wgpu::RenderPipeline {
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("terrain-region-opaque"),
            source: wgpu::ShaderSource::Wgsl(Cow::Owned(region_opaque_wgsl(TERRAIN_OPAQUE_WGSL))),
        });
        Self::terrain_pipeline_from_module(
            device,
            layout,
            target_format,
            &module,
            ("terrain_opaque_vs", "terrain_opaque_fs"),
            &[Some(TERRAIN_VERTEX_LAYOUT)],
            false,
            "terrain-region-opaque",
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn terrain_pipeline_from_module(
        device: &wgpu::Device,
        layout: &wgpu::PipelineLayout,
        target_format: wgpu::TextureFormat,
        module: &wgpu::ShaderModule,
        (vs_entry, fs_entry): (&str, &str),
        buffers: &[Option<wgpu::VertexBufferLayout<'_>>],
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
                buffers,
            },
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                // Solid opaque sections carry outward-facing winding, so
                // the rasterizer can drop back faces; cutout/translucent
                // geometry stays double-sided.
                cull_mode: (!translucent).then_some(wgpu::Face::Back),
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

    /// One `wgpu::ErrorScope`-guarded pack pipeline build for the lit
    /// sub-world path, mirroring [`Self::terrain_pipeline_from_module`]
    /// but on the four-group [`CompositeGpu::pack_lit_layout`]: `None`
    /// on any device rejection (bad entry points, vertex-layout
    /// mismatch, unboundable resources) so the caller can fall back to
    /// the built-in `COMPOSITE_LIT_WGSL` pipelines.
    fn try_pack_lit_pipeline(
        &self,
        module: &wgpu::ShaderModule,
        (vs_entry, fs_entry): (&str, &str),
        buffers: &[Option<wgpu::VertexBufferLayout<'_>>],
        blend: Option<wgpu::BlendState>,
        depth_write: bool,
        label: &str,
    ) -> Option<wgpu::RenderPipeline> {
        let scope = self
            .ctx
            .device
            .push_error_scope(wgpu::ErrorFilter::Validation);
        let pipeline = self
            .ctx
            .device
            .create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some(label),
                layout: Some(&self.composite.pack_lit_layout),
                vertex: wgpu::VertexState {
                    module,
                    entry_point: Some(vs_entry),
                    compilation_options: wgpu::PipelineCompilationOptions::default(),
                    buffers,
                },
                primitive: wgpu::PrimitiveState {
                    topology: wgpu::PrimitiveTopology::TriangleList,
                    // Lit sub-world geometry is double-sided — Create
                    // blades and contraption shells have no guaranteed
                    // outward winding.
                    cull_mode: None,
                    ..wgpu::PrimitiveState::default()
                },
                depth_stencil: Some(depth_state(depth_write)),
                multisample: wgpu::MultisampleState::default(),
                fragment: Some(wgpu::FragmentState {
                    module,
                    entry_point: Some(fs_entry),
                    compilation_options: wgpu::PipelineCompilationOptions::default(),
                    targets: &[Some(wgpu::ColorTargetState {
                        format: self.target_format,
                        blend,
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                }),
                multiview_mask: None,
                cache: None,
            });
        match pollster::block_on(scope.pop()) {
            None => Some(pipeline),
            Some(_) => None,
        }
    }

    /// Offer `pack`'s `gbuffers_subworld`/`gbuffers_entities`-style
    /// passes to the device for the lit sub-world path: structural
    /// validation, module creation and the opaque/alpha/instanced
    /// pipeline builds each run inside a `wgpu::ErrorScope`. Accepted
    /// pipelines carry the composed frame's real environment —
    /// `CompositeEnvUniforms` sun/ambient/exposure/fog/depth-tint plus
    /// the atlas table — and, when the module binds group 3, the live
    /// shadow receiver (map resolution, cascade count, PCF taps, sun
    /// color) so Create cogwheels, water wheels and contraptions shade
    /// under the active shaderpack's sun/fog/exposure/shadow
    /// environment instead of the flat built-in. Every rejection falls
    /// back cleanly to `COMPOSITE_LIT_WGSL` (+ `shadow.lit_color`/
    /// `shadow.instanced_color` while cascades are active). Returns a
    /// `"; ..."` notice fragment for the caller's verdict line.
    fn install_pack_subworld_passes(&mut self, pack: &MeguWgslPack) -> String {
        self.pack_subworld = None;
        let mut last_rejection = String::new();
        for pass in &pack.passes {
            if pass.stage != PipelineStage::Gbuffers
                || pass.kind != PassKind::Graphics
                || !PACK_SUBWORLD_PASS_NAMES.contains(&pass.name.as_str())
            {
                continue;
            }
            let info = match validate_wgsl_module(
                &pass.wgsl_source,
                &[WgslStage::Vertex, WgslStage::Fragment],
            ) {
                Ok(info) => info,
                Err(err) => {
                    last_rejection =
                        format!("{}: structural validation failed: {err}", pass.name);
                    continue;
                }
            };
            let vertex_eps: Vec<&str> = info
                .entry_points
                .iter()
                .filter(|(s, _)| *s == WgslStage::Vertex)
                .map(|(_, n)| n.as_str())
                .collect();
            let Some(fs_entry) = info
                .entry_points
                .iter()
                .find(|(s, _)| *s == WgslStage::Fragment)
                .map(|(_, n)| n.clone())
            else {
                last_rejection = format!("{}: no fragment entry point", pass.name);
                continue;
            };
            // `@group(3)` bindings require the live shadow receiver —
            // gate these pipelines to shadow-active frames.
            let needs_shadow = info.bindings.iter().any(|b| b.group >= 3);
            let scope = self
                .ctx
                .device
                .push_error_scope(wgpu::ErrorFilter::Validation);
            let module = self
                .ctx
                .device
                .create_shader_module(wgpu::ShaderModuleDescriptor {
                    label: Some("megu-pack-subworld"),
                    source: wgpu::ShaderSource::Wgsl(Cow::Borrowed(
                        pass.wgsl_source.as_str(),
                    )),
                });
            if pollster::block_on(scope.pop()).is_some() {
                last_rejection = format!("{}: device rejected module", pass.name);
                continue;
            }
            let lit_buffers = [Some(LIT_VERTEX_LAYOUT)];
            let inst_buffers = [Some(LIT_VERTEX_LAYOUT), Some(PART_INSTANCE_LAYOUT)];
            // Convention: the first vertex entry shades whole
            // sub-worlds; a second vertex entry (when the module has
            // one) is the instanced variant. A single-entry module
            // serves both — the instance stream is just vertex data the
            // shader ignores.
            let mut lit = None;
            for vs in &vertex_eps {
                lit = self.try_pack_lit_pipeline(
                    &module,
                    (vs, &fs_entry),
                    &lit_buffers,
                    None,
                    true,
                    "megu-pack-lit",
                );
                if lit.is_some() {
                    break;
                }
            }
            let inst_eps: Vec<&str> = if vertex_eps.len() > 1 {
                vertex_eps[1..]
                    .iter()
                    .chain(vertex_eps[..1].iter())
                    .copied()
                    .collect()
            } else {
                vertex_eps.clone()
            };
            let mut instanced = None;
            for vs in &inst_eps {
                instanced = self.try_pack_lit_pipeline(
                    &module,
                    (vs, &fs_entry),
                    &inst_buffers,
                    None,
                    true,
                    "megu-pack-instanced",
                );
                if instanced.is_some() {
                    break;
                }
            }
            let mut lit_alpha = None;
            for vs in &vertex_eps {
                lit_alpha = self.try_pack_lit_pipeline(
                    &module,
                    (vs, &fs_entry),
                    &lit_buffers,
                    Some(ALPHA_BLEND),
                    false,
                    "megu-pack-lit-alpha",
                );
                if lit_alpha.is_some() {
                    break;
                }
            }
            if lit.is_some() || lit_alpha.is_some() || instanced.is_some() {
                let name = pass.name.clone();
                self.pack_subworld = Some(PackSubworldGpu {
                    pass_name: name.clone(),
                    lit,
                    lit_alpha,
                    instanced,
                    needs_shadow,
                });
                return format!(
                    "; pass {name:?} drives Create sub-worlds + kinetic parts"
                );
            }
            last_rejection = format!("{}: device rejected every lit pipeline", pass.name);
        }
        if last_rejection.is_empty() {
            String::new()
        } else {
            format!("; sub-world passes kept built-in lit ({last_rejection})")
        }
    }

    /// The pack `gbuffers_*` pass currently driving Create sub-worlds,
    /// kinetic parts and entities (`gbuffers_subworld`,
    /// `gbuffers_entities`, ...), or `None` while the built-in
    /// `COMPOSITE_LIT_WGSL` pipelines draw them.
    pub fn pack_subworld_pass(&self) -> Option<&str> {
        self.pack_subworld.as_ref().map(|p| p.pass_name.as_str())
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
        // Sub-world/entity passes install first: they share the lit
        // contract, and their names stay out of terrain candidacy below.
        let subworld_note = self.install_pack_subworld_passes(pack);
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
            if pass.stage != PipelineStage::Gbuffers
                || pass.kind != PassKind::Graphics
                || PACK_SUBWORLD_PASS_NAMES.contains(&pass.name.as_str())
            {
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
                        &[Some(TERRAIN_VERTEX_LAYOUT)],
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
                        "shaderpack {:?} active: pass {:?} drives the terrain \
                         pipeline{subworld_note}",
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
                "shaderpack {:?} has no gbuffers graphics pass; built-in terrain \
                 kept{subworld_note}",
                pack.name
            )
        } else {
            format!(
                "shaderpack {:?} fell back to built-in terrain \
                 ({last_rejection}){subworld_note}",
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
                megu_client_mesh::load_megu_wgsl_pack(&source, &self.ctx.report.shader_caps)
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

    /// Upload a meshed scene: quads -> real `megu_client_mesh` packed
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
            cutout_indices: self.empty_index_buffer.clone(),
            cutout_index_count: 0,
            water_vertices: self.empty_vertex_buffer.clone(),
            water_indices: self.empty_index_buffer.clone(),
            water_index_count: 0,
        };
        self.scene = Some(buffers);
        self.scene_revision += 1;
        self.set_section_origin(scene.section_origin);
        self.shadow_cache.invalidate();
        Ok(())
    }

    /// Reconcile the resident section set with `world` and queue changed
    /// sections for upload. GPU buffer creation is deferred to the render
    /// loop's [`ChunkUploadBudget`]-gated drain so a chunk burst spreads
    /// across frames instead of spiking one submit. Also refreshes the
    /// visibility-graph snapshot the composed pass culls with — this
    /// method only runs on terrain-dirty frames, so the collect stays off
    /// the steady-state render path. Returns the number of sections whose
    /// queued mesh data changed.
    pub fn update_live_terrain(&mut self, world: &WorldState) -> usize {
        let mut scheduled = 0;
        let mut removed: Vec<(SectionCoord, Option<[i32; 3]>)> = Vec::new();
        self.live_sections.retain(|coord, section| {
            let keep = world.packed_mesh(*coord).is_some();
            if !keep {
                removed.push((*coord, section.arena));
            }
            keep
        });
        self.pending_uploads
            .retain(|coord, _| world.packed_mesh(*coord).is_some());
        self.section_graphs.clear();
        self.section_graphs.extend(world.section_graphs());
        // Order-independent stamp of the mirrored graph set — a graph-only
        // world change must still invalidate the composed-frame cull cache.
        let graphs_stamp = self.compute_graphs_stamp();
        let removed_any = !removed.is_empty();
        for (coord, origin) in removed {
            if let Some(origin) = origin {
                if let Some(arena) = self.region_arenas.get_mut(&origin) {
                    arena.remove(coord);
                }
            }
            self.invalidate_hiz();
        }
        let grace = self.transfer_budget.idle_slab_reclaim_frames;
        self.region_arenas
            .retain(|_, arena| !arena.slots.is_empty() || arena.empty_frames < grace);
        for (coord, packed) in world.packed_meshes() {
            let order = world.translucent_order(coord).unwrap_or(&[]);
            scheduled += usize::from(self.queue_live_section_upload(world, coord, packed, order));
        }
        if scheduled > 0 {
            self.shadow_cache.invalidate();
        }
        if scheduled > 0 || removed_any || graphs_stamp != self.cull_graphs_stamp {
            self.cull_world_revision += 1;
            self.cull_graphs_stamp = graphs_stamp;
        }
        scheduled
    }

    /// Order-independent stamp of the mirrored `section_graphs` set —
    /// a graph-only world change still invalidates the cull cache.
    fn compute_graphs_stamp(&self) -> u64 {
        let mut stamp = self.section_graphs.len() as u64;
        for coord in self.section_graphs.keys() {
            stamp = stamp.wrapping_add(
                (coord.0 as u64)
                    .wrapping_mul(0x9e37_79b9)
                    .wrapping_add((coord.1 as u64).wrapping_mul(0x85eb_ca6b))
                    .wrapping_add((coord.2 as u64).wrapping_mul(0xc2b2_ae35)),
            );
        }
        stamp
    }

    /// Queue `coord` for upload when its world data differs from both the
    /// pending queue entry and the resident copy. Shared by the full
    /// reconcile and the O(1) dirty-coordinate path; returns whether a new
    /// payload was queued (and speculatively submitted to the worker pool).
    fn queue_live_section_upload(
        &mut self,
        world: &WorldState,
        coord: SectionCoord,
        packed: &[PackedVertex],
        order: &[u32],
    ) -> bool {
        let up_to_date = match self.pending_uploads.get(&coord) {
            Some(pending) => pending.matches(packed, order),
            None => self.live_sections.get(&coord).is_some_and(|existing| {
                existing.packed.as_ref() == packed && existing.order.as_ref() == order
            }),
        };
        if up_to_date {
            return false;
        }
        let packed: Arc<[PackedVertex]> = packed.into();
        let order: Arc<[u32]> = order.into();
        let buckets = world.direction_buckets(coord);
        self.pending_uploads.insert(
            coord,
            PendingSectionUpload::new(packed.clone(), order.clone(), buckets),
        );
        // Speculative geometry build off the render thread: the drain
        // prefers this result when the worker finishes before the
        // budget admits the section; a full queue falls back inline.
        if self.mesh_pool.try_submit(MeshJob::SectionGeometry {
            coord,
            packed,
            order,
            buckets,
        }) {
            self.bg_mesh_jobs_submitted += 1;
        }
        true
    }

    /// O(1) dirty-coordinate reconcile: the live session marks candidate
    /// sections per play event (column events mark the column and its
    /// four lateral neighbors, block edits mark the section and its six
    /// face neighbors — supersets of every coord `MeshCache` can
    /// invalidate), and this pass touches exactly that set. `order_sweep`
    /// additionally compares resident translucent orders — the
    /// camera-driven re-sort can reorder sections outside the marked set.
    /// Same queueing and revision semantics as the full reconcile; steady
    /// frames with an empty set and no sweep cost one branch.
    pub fn update_live_terrain_tracked(
        &mut self,
        world: &WorldState,
        dirty: &BTreeSet<SectionCoord>,
        order_sweep: bool,
    ) -> usize {
        if dirty.is_empty() && !order_sweep {
            return 0;
        }
        let mut scheduled = 0;
        let mut removed_any = false;
        for &coord in dirty {
            match world.packed_mesh(coord) {
                None => {
                    self.pending_uploads.remove(&coord);
                    self.completed_geometry.remove(&coord);
                    if let Some(section) = self.live_sections.remove(&coord) {
                        removed_any = true;
                        if let Some(origin) = section.arena {
                            if let Some(arena) = self.region_arenas.get_mut(&origin) {
                                arena.remove(coord);
                            }
                        }
                        self.invalidate_hiz();
                    }
                    self.section_graphs.remove(&coord);
                }
                Some(packed) => {
                    let order = world.translucent_order(coord).unwrap_or(&[]);
                    scheduled +=
                        usize::from(self.queue_live_section_upload(world, coord, packed, order));
                }
            }
        }
        // The camera-driven translucent re-sort can rewrite orders for
        // sections no event marked; sweep residents cheaply (opaque-only
        // sections have empty orders and skip on length alone).
        if order_sweep {
            let mut stale: Vec<SectionCoord> = Vec::new();
            for (coord, section) in self.live_sections.iter() {
                if dirty.contains(coord) {
                    continue;
                }
                let order = world.translucent_order(*coord).unwrap_or(&[]);
                if section.order.as_ref() != order {
                    stale.push(*coord);
                }
            }
            for coord in stale {
                let Some(packed) = world.packed_mesh(coord) else {
                    if let Some(section) = self.live_sections.remove(&coord) {
                        removed_any = true;
                        if let Some(origin) = section.arena {
                            if let Some(arena) = self.region_arenas.get_mut(&origin) {
                                arena.remove(coord);
                            }
                        }
                        self.invalidate_hiz();
                    }
                    self.section_graphs.remove(&coord);
                    continue;
                };
                let order = world.translucent_order(coord).unwrap_or(&[]);
                scheduled +=
                    usize::from(self.queue_live_section_upload(world, coord, packed, order));
            }
        }
        if !dirty.is_empty() || removed_any {
            let grace = self.transfer_budget.idle_slab_reclaim_frames;
            self.region_arenas
                .retain(|_, arena| !arena.slots.is_empty() || arena.empty_frames < grace);
            self.section_graphs.clear();
            self.section_graphs.extend(world.section_graphs());
            self.cull_graphs_stamp = self.compute_graphs_stamp();
        }
        if scheduled > 0 {
            self.shadow_cache.invalidate();
        }
        if scheduled > 0 || removed_any || !dirty.is_empty() || order_sweep {
            self.cull_world_revision += 1;
        }
        scheduled
    }

    /// Build the GPU buffers, section uniform and bind group for one
    /// section and make it resident, replacing any previous upload for
    /// `coord`. When the world published [`DirectionBuckets`] and the
    /// region-arena path is enabled, the opaque stream is repacked into
    /// the containing 8x4x8 [`RegionArena`] instead of per-section
    /// buffers; translucent geometry and the section uniform stay
    /// per-section either way.
    fn install_live_section(
        &mut self,
        coord: SectionCoord,
        packed: &Arc<[PackedVertex]>,
        order: &Arc<[u32]>,
        buckets: Option<DirectionBuckets>,
    ) {
        let origin = [
            coord.0 as f32 * 16.0,
            coord.1 as f32 * 16.0,
            coord.2 as f32 * 16.0,
        ];
        let geometry = section_geometry(packed, order, origin);
        let opaque_bucket_ranges = buckets.map(|b| compute_opaque_bucket_ranges(packed, b));
        self.install_live_section_geometry(
            coord,
            packed.clone(),
            order.clone(),
            buckets,
            opaque_bucket_ranges,
            geometry,
        );
    }

    /// The install body shared by the inline path and worker-built
    /// geometry results.
    #[allow(clippy::too_many_arguments)]
    fn install_live_section_geometry(
        &mut self,
        coord: SectionCoord,
        packed: Arc<[PackedVertex]>,
        order: Arc<[u32]>,
        buckets: Option<DirectionBuckets>,
        opaque_bucket_ranges: Option<[(u32, u32); DIRECTION_BUCKETS]>,
        mut geometry: LiveSectionGeometry,
    ) {
        // Retire the previous residency first: a re-upload may switch a
        // section between the arena and per-section storage.
        if let Some(origin) = self
            .live_sections
            .get(&coord)
            .and_then(|section| section.arena)
        {
            if let Some(arena) = self.region_arenas.get_mut(&origin) {
                arena.remove(coord);
            }
        }
        let opaque_index_total = geometry.opaque_indices.len() as u32;
        // Pre-upload diagonal rotation: flip-marked quads become
        // canonical `[0,1,2,0,2,3]` draws, so every opaque quad —
        // arena-resident or per-section — indexes the one shared static
        // index buffer positionally.
        rotate_opaque_quads_for_shared_index(&mut geometry.opaque);
        let arena_origin = (self.arena_enabled && opaque_index_total > 0 && buckets.is_some())
            .then(|| {
                let origin = RenderRegions::origin_of(coord);
                let ranges = opaque_bucket_ranges.unwrap_or([(0, 0); DIRECTION_BUCKETS]);
                let section_word = region_section_word(origin, coord);
                if !self.region_arenas.contains_key(&origin) {
                    let arena = RegionArena::new(
                        &self.ctx.device,
                        &self.ctx.queue,
                        &self.frame_layout,
                        &self.frame_ubo,
                        origin,
                    );
                    self.region_arenas.insert(origin, arena);
                }
                if let Some(arena) = self.region_arenas.get_mut(&origin) {
                    // The 16-byte stream serializes straight into the staged
                    // partial write — no CPU vertex copy is retained per
                    // section (previously a 24-byte `RegionVertex` Vec sat
                    // resident in the arena for its whole lifetime).
                    arena.insert(coord, &geometry.opaque, section_word, ranges);
                    self.cpu_opaque_bytes_freed += (geometry.opaque.len() * 16) as u64;
                }
                origin
            });
        let (opaque_vertices, opaque_indices, opaque_index_count) = if arena_origin.is_some() {
            (
                self.empty_vertex_buffer.clone(),
                self.empty_index_buffer.clone(),
                0,
            )
        } else {
            // Per-section opaque draws share the global canonical index
            // buffer too — the local index stream was byte-identical to
            // the shared pattern, so only the vertex buffer stays
            // per-section and the index slot keeps the dummy handle
            // (draw sites bind `self.shared_quad_index_buffer` live so a
            // growth realloc never leaves a stale slice bound).
            (
                vertex_buffer(
                    &self.ctx.device,
                    &self.ctx.queue,
                    "live-opaque-verts",
                    &geometry.opaque,
                ),
                self.empty_index_buffer.clone(),
                opaque_index_total,
            )
        };
        // Opaque-only sections share the empty translucent buffers and —
        // when the arena already owns their opaque stream — the shared
        // dummy bind group too, so an opaque-only section costs zero
        // `create_buffer`/`create_bind_group` calls.
        let translucent_index_count = geometry.translucent_indices.len() as u32;
        let (translucent_vertices, translucent_indices) = if translucent_index_count == 0 {
            (
                self.empty_vertex_buffer.clone(),
                self.empty_index_buffer.clone(),
            )
        } else {
            (
                vertex_buffer(
                    &self.ctx.device,
                    &self.ctx.queue,
                    "live-translucent-verts",
                    &geometry.translucent,
                ),
                index_buffer(
                    &self.ctx.device,
                    &self.ctx.queue,
                    "live-translucent-indices",
                    &geometry.translucent_indices,
                ),
            )
        };
        let cutout_index_count = geometry.cutout_indices.len() as u32;
        let cutout_indices = if cutout_index_count == 0 || arena_origin.is_some() {
            // Arena-resident opaque streams keep section-local indices
            // meaningless — cutout shadow draws stay per-section only.
            self.empty_index_buffer.clone()
        } else {
            index_buffer(
                &self.ctx.device,
                &self.ctx.queue,
                "live-cutout-indices",
                &geometry.cutout_indices,
            )
        };
        let water_index_count = geometry.water_indices.len() as u32;
        let (water_vertices, water_indices) = if water_index_count == 0 {
            (
                self.empty_vertex_buffer.clone(),
                self.empty_index_buffer.clone(),
            )
        } else {
            (
                typed_vertex_buffer(
                    &self.ctx.device,
                    &self.ctx.queue,
                    "live-water-verts",
                    &geometry.water,
                ),
                index_buffer(
                    &self.ctx.device,
                    &self.ctx.queue,
                    "live-water-indices",
                    &geometry.water_indices,
                ),
            )
        };
        let terrain = TerrainBuffers {
            opaque_index_count,
            translucent_index_count,
            opaque_vertices,
            opaque_indices,
            translucent_vertices,
            translucent_indices,
            cutout_indices,
            cutout_index_count: if arena_origin.is_some() {
                0
            } else {
                cutout_index_count
            },
            water_vertices,
            water_indices,
            water_index_count,
        };
        let shared_group =
            translucent_index_count == 0 && (arena_origin.is_some() || opaque_index_total == 0);
        let (uniform, bind_group) = if shared_group {
            (
                self.empty_section_uniform.clone(),
                self.empty_bind_group.clone(),
            )
        } else {
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
            (uniform, bind_group)
        };
        self.live_sections.insert(
            coord,
            LiveTerrainSection {
                packed,
                order,
                terrain,
                opaque_index_total,
                opaque_bucket_ranges,
                buckets,
                arena: arena_origin,
                _uniform: uniform,
                bind_group,
            },
        );
        // Occluders changed: the installed pyramid (if any) is stale.
        self.invalidate_hiz();
        self.cull_world_revision += 1;
        self.uploaded_section_coords.push(coord);
    }

    /// Coords whose latest mesh finished the VRAM upload this session —
    /// drained by the window loop so `WorldState` can release the matching
    /// CPU copies (`release_uploaded_section_cpu_copies`).
    pub fn take_uploaded_section_coords(&mut self) -> Vec<SectionCoord> {
        std::mem::take(&mut self.uploaded_section_coords)
    }

    /// Drop the worker-built pyramid when terrain residency shifts; an
    /// embedder-installed pyramid (no `hiz_source_vp`) is the caller's
    /// responsibility and stays.
    fn invalidate_hiz(&mut self) {
        if self.hiz_source_vp.is_some() {
            self.hiz_pyramid = None;
            self.hiz_source_vp = None;
            self.hiz_revision += 1;
        }
        self.hiz_dirty = true;
    }

    /// Pull finished worker results: geometry completions are parked for
    /// the pending-upload drain, Hi-Z pyramids install directly.
    fn drain_mesh_results(&mut self) {
        let mut results = Vec::new();
        self.mesh_pool.drain(|result| results.push(result));
        for result in results {
            match result {
                MeshResult::SectionGeometry {
                    coord,
                    packed,
                    geometry,
                    opaque_bucket_ranges,
                } => {
                    self.bg_mesh_jobs_completed += 1;
                    self.completed_geometry.insert(
                        coord,
                        CompletedSectionGeometry {
                            packed,
                            geometry,
                            opaque_bucket_ranges,
                        },
                    );
                }
                MeshResult::HiZDepth { view_proj, pyramid } => {
                    // `hiz_dirty` means residency moved after the job was
                    // submitted — its pyramid may still hold occluders that
                    // no longer exist, so drop it and let the next frame
                    // rebuild from current sections.
                    if let (Some(pyramid), false, false) =
                        (pyramid, self.hiz_manual, self.hiz_dirty)
                    {
                        self.hiz_pyramid = Some(pyramid);
                        self.hiz_source_vp = Some(view_proj);
                        self.hiz_frames_updated += 1;
                        self.hiz_revision += 1;
                    }
                }
            }
        }
        // Drop completions whose pending upload was superseded (a newer
        // packed payload re-queued the section).
        let pending = &self.pending_uploads;
        self.completed_geometry.retain(|coord, done| {
            pending
                .get(coord)
                .is_some_and(|p| Arc::ptr_eq(&p.packed, &done.packed))
        });
    }

    /// Queue the next Hi-Z splat job over this frame's drawn opaque
    /// quads (`self.cull_result` supplies the skip set). Skipped while an
    /// embedder-installed pyramid is active or the installed pyramid
    /// already matches this view and residency.
    fn submit_hiz_job(&mut self, view_proj: &Mat4, camera_position: [f32; 3]) {
        if self.hiz_manual {
            return;
        }
        if !self.hiz_dirty
            && self
                .hiz_source_vp
                .is_some_and(|source| source == *view_proj)
        {
            return;
        }
        let mut sections = Vec::with_capacity(self.live_sections.len());
        for (coord, section) in &self.live_sections {
            if self.cull_result.contains(coord) || section.opaque_index_total == 0 {
                continue;
            }
            let min = [
                coord.0 as f32 * EDGE as f32,
                coord.1 as f32 * EDGE as f32,
                coord.2 as f32 * EDGE as f32,
            ];
            sections.push(HiZSplatSection {
                origin: min,
                packed: section.packed.clone(),
                buckets: section.buckets,
                draw_mask: section_draw_mask(min, camera_position),
            });
        }
        if self.mesh_pool.try_submit(MeshJob::HiZDepth {
            view_proj: *view_proj,
            sections,
        }) {
            self.hiz_dirty = false;
        }
    }

    /// Grow the shared quad index buffer past the largest referenced
    /// extent (arena high-water marks and non-arena sections' index
    /// counts). The contents are the pure [`QUAD_INDEX_PATTERN`] stream,
    /// so growth is one geometric re-create + pattern upload. Returns
    /// whether the buffer was replaced (recorded bundles referencing it
    /// must re-record).
    fn ensure_shared_quad_index_capacity(&mut self) -> bool {
        let mut needed = self
            .live_sections
            .values()
            .map(|section| section.opaque_index_total)
            .max()
            .unwrap_or(0);
        for arena in self.region_arenas.values() {
            needed = needed.max(arena.index_extent());
        }
        if needed <= self.shared_quad_index_capacity {
            return false;
        }
        let quads = needed.div_ceil(6).next_power_of_two().max(64);
        let capacity = quads * 6;
        let buffer = self.ctx.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("shared-quad-indices"),
            size: u64::from(capacity) * 4,
            usage: wgpu::BufferUsages::INDEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        self.ctx.queue.write_buffer(
            &buffer,
            0,
            bytemuck::cast_slice(&quad_index_pattern(capacity)),
        );
        self.shared_quad_index_buffer = buffer;
        self.shared_quad_index_capacity = capacity;
        true
    }

    /// Write dirty region-arena vertex streams to their GPU buffers
    /// through the persistent staging ring, under this frame's DMA byte
    /// cap. Runs at the upload boundary so the render pass reads
    /// immutable arena state. Staged inserts land as slot-range DMA
    /// copies — counted in `region_partial_uploads` and
    /// `staging_ring_uploads`. Also ticks the idle-slab reclaim clock:
    /// an arena empty for `idle_slab_reclaim_frames` retires its
    /// buffers, and the shared index buffer grows to cover whatever
    /// committed.
    fn flush_region_arenas(&mut self) {
        let mut replaced = false;
        let mut encoder = self
            .ctx
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("region-arena-flush"),
            });
        let mut dma_remaining = self.transfer_budget.frame_dma_cap_bytes;
        let slab = self.transfer_budget.arena_slab_bytes as usize;
        // One encoder spans every staged copy below: slots used by an
        // unsubmitted copy stay out of rotation until the submit.
        let mut any_partial = false;
        self.staging_ring.begin_batch();
        for arena in self.region_arenas.values_mut() {
            let (partial, buffers_replaced) = arena.flush_gpu(
                &self.ctx.device,
                &self.ctx.queue,
                &mut encoder,
                &mut self.staging_ring,
                &mut dma_remaining,
                slab,
            );
            self.region_partial_uploads += u64::from(partial);
            any_partial |= partial > 0;
            replaced |= buffers_replaced;
        }
        self.ctx.queue.submit([encoder.finish()]);
        self.staging_ring_uploads = self.staging_ring.staging_ring_uploads;
        self.staging_ring_allocs = self.staging_ring.staging_ring_allocs;
        self.staging_ring_dma_defers = self.staging_ring.staging_ring_dma_defers;
        // Idle-slab reclaim: an arena whose slots all left keeps its
        // buffers for the grace period (a chunk re-entering the region
        // then reuses them), then retires.
        let grace = self.transfer_budget.idle_slab_reclaim_frames;
        for arena in self.region_arenas.values_mut() {
            if arena.slots.is_empty() {
                arena.empty_frames += 1;
            } else {
                arena.empty_frames = 0;
            }
        }
        self.region_arenas
            .retain(|_, arena| !arena.slots.is_empty() || arena.empty_frames < grace);
        if self.ensure_shared_quad_index_capacity() {
            replaced = true;
        }
        if replaced || any_partial {
            // Either a retired arena buffer was replaced or a pending slot
            // finished staging onto the GPU — bump the residency revision so
            // render bundles and the GPU-indirect pack cache observe it.
            self.cull_world_revision += 1;
        }
    }

    /// Enable/disable the shared region-arena opaque path; `false` keeps
    /// per-section opaque buffers (still bucket-culled when ranges
    /// exist). Sections already installed keep their storage — the flag
    /// applies to new installs.
    pub fn set_region_arena_enabled(&mut self, enabled: bool) {
        self.arena_enabled = enabled;
    }

    /// Per-rendered-frame upload boundary: measure the frame interval for
    /// the budget's adaptive controller, open a fresh
    /// [`ChunkUploadBudget`] frame, adapt the per-frame cap to the windowed
    /// P95, then drain queued section uploads through it. Runs once per
    /// composed or plain world pass.
    fn begin_render_upload_frame(&mut self) {
        self.drain_mesh_results();
        let now = Instant::now();
        if let Some(last) = self.last_render_instant.replace(now) {
            let ms = now.duration_since(last).as_secs_f64() * 1_000.0;
            if ms.is_finite() && ms > 0.0 {
                self.upload_frame_ms.push_back(ms);
                while self.upload_frame_ms.len() > UPLOAD_P95_WINDOW {
                    self.upload_frame_ms.pop_front();
                }
            }
        }
        self.upload_budget.begin_frame();
        if self.upload_frame_ms.is_empty() {
            // No measured interval yet — `adapt(0)` reads as "no
            // evidence of stress" and lets the adaptive cap regrow.
            self.upload_budget.adapt(0.0);
        } else {
            // The counted live feedback edge: real measured frame P95.
            self.observe_frame_p95_ms(self.upload_frame_p95_us() / 1_000.0);
        }
        self.drain_pending_uploads();
        self.flush_region_arenas();
    }

    /// Windowed P95 of recent rendered-frame intervals, in microseconds —
    /// the input [`ChunkUploadBudget::adapt`] expects. `0.0` on an empty
    /// window reads as "no evidence of stress" and lets the cap regrow.
    fn upload_frame_p95_us(&self) -> f64 {
        if self.upload_frame_ms.is_empty() {
            return 0.0;
        }
        let mut sorted: Vec<f64> = self.upload_frame_ms.iter().copied().collect();
        sorted.sort_by(f64::total_cmp);
        let rank = (sorted.len() * 95).div_ceil(100).max(1) - 1;
        sorted[rank.min(sorted.len() - 1)] * 1_000.0
    }

    /// The last player world position reported through
    /// [`Self::set_player_position`]; the upload drain's Priority-0 path
    /// admits sections within one section (16 m) of it ahead of the
    /// budget.
    pub fn set_player_position(&mut self, position: [f32; 3]) {
        self.player_position = position;
    }

    /// Admit queued section uploads through the open [`ChunkUploadBudget`]
    /// frame and install the ones that fit; the rest stay queued for later
    /// frames. Priority-0 first: sections within one section (16 m) of the
    /// player bypass the budget and install immediately — a near edit
    /// must swap its slot atomically this frame, never X-ray-flicker
    /// behind a far upload. Returns the number installed this frame.
    fn drain_pending_uploads(&mut self) -> usize {
        if self.pending_uploads.is_empty() {
            return 0;
        }
        let mut admitted = 0;
        let player_section = SectionCoord(
            (self.player_position[0] / 16.0).floor() as i32,
            (self.player_position[1] / 16.0).floor() as i32,
            (self.player_position[2] / 16.0).floor() as i32,
        );
        let near = |coord: &SectionCoord| {
            (coord.0 - player_section.0).abs() <= 1
                && (coord.1 - player_section.1).abs() <= 1
                && (coord.2 - player_section.2).abs() <= 1
        };
        // Priority-0 is the *edit* guarantee: a section that is already
        // resident and within 16 m of the player bypasses the budget so
        // its slot swaps atomically this frame (no X-ray flicker). New
        // installs — even near ones — still flow through the per-frame
        // budget so a chunk burst cannot stall the render thread.
        let coords: Vec<SectionCoord> = {
            let (p0, rest): (Vec<SectionCoord>, Vec<SectionCoord>) = self
                .pending_uploads
                .keys()
                .copied()
                .partition(|c| near(c) && self.live_sections.contains_key(c));
            p0.into_iter().chain(rest).collect()
        };
        for coord in coords {
            let Some(pending) = self.pending_uploads.get(&coord) else {
                continue;
            };
            let (bytes, cost_us) = (pending.upload_bytes, pending.cost_us);
            let priority0 = near(&coord) && self.live_sections.contains_key(&coord);
            if !priority0 && !self.upload_budget.try_schedule(bytes, cost_us) {
                self.deferred_uploads += 1;
                continue;
            }
            let pending = self
                .pending_uploads
                .remove(&coord)
                .expect("pending checked");
            self.admitted_uploads += 1;
            // Prefer the worker-built geometry when it is still for this
            // exact packed payload; otherwise convert inline.
            let completed = self
                .completed_geometry
                .remove(&coord)
                .filter(|done| Arc::ptr_eq(&done.packed, &pending.packed));
            match completed {
                Some(done) => self.install_live_section_geometry(
                    coord,
                    pending.packed,
                    pending.order,
                    pending.buckets,
                    done.opaque_bucket_ranges,
                    done.geometry,
                ),
                None => self.install_live_section(
                    coord,
                    &pending.packed,
                    &pending.order,
                    pending.buckets,
                ),
            }
            admitted += 1;
        }
        if admitted > 0 {
            self.shadow_cache.invalidate();
        }
        admitted
    }

    /// Refresh `self.cull_result`: sections the composed pass will skip
    /// this frame — the [`GraphCuller`]'s frustum+occlusion result for
    /// graph-covered coords, the plain six-plane frustum test for coords
    /// the world never published a visibility graph for, and — when a Hi-Z
    /// pyramid is installed — the conservative depth test on top of both.
    /// Only sections with drawable indices are considered.
    ///
    /// §4 F3: identical `(camera_section, view_proj bits, world revision,
    /// hiz revision)` inputs replay the cached result in ~0 µs
    /// (`cull_cache_hits += 1`); a miss refills the retained
    /// `cull_result`/`cull_graph_scratch` buffers instead of allocating
    /// fresh `HashSet`s every frame.
    fn cull_live_sections(
        &mut self,
        view_proj: &Mat4,
        camera_position: [f32; 3],
        focal_y_px: f32,
    ) {
        let camera_section = SectionCoord(
            (camera_position[0] / EDGE as f32).floor() as i32,
            (camera_position[1] / EDGE as f32).floor() as i32,
            (camera_position[2] / EDGE as f32).floor() as i32,
        );
        let mut view_proj_bits = [0u32; 16];
        for (dst, src) in view_proj_bits.iter_mut().zip(view_proj.iter().flatten()) {
            *dst = src.to_bits();
        }
        let key = CullCacheKey {
            camera_section: (camera_section.0, camera_section.1, camera_section.2),
            view_proj_bits,
            focal_y_bits: focal_y_px.to_bits(),
            world_revision: self.cull_world_revision,
            hiz_revision: self.hiz_revision,
        };
        if self.cull_cache_key == Some(key) {
            self.cull_cache_hits += 1;
            self.hiz_culled_sections += self.cull_hiz_count;
            self.subpixel_culled_sections += self.cull_subpixel_count;
            return;
        }
        // Miss: refill the retained sets (steady frames keep their
        // capacity, so the recompute allocates nothing).
        let mut culled = std::mem::take(&mut self.cull_result);
        culled.clear();
        let mut hiz_culled = 0u64;
        let mut subpixel_culled = 0u64;
        if !self.live_sections.is_empty() {
            let planes = frustum_planes_from_view_proj(view_proj);
            // One graph-culler BFS per frame over the mirrored visibility
            // set. Coords absent from the snapshot (mesh-only worlds) fall
            // back to the frustum test rather than being treated as
            // unreachable.
            let has_graphs = !self.section_graphs.is_empty();
            let mut graph_scratch = std::mem::take(&mut self.cull_graph_scratch);
            graph_scratch.clear();
            if has_graphs {
                graph_scratch.extend(
                    GraphCuller::new(camera_section, &planes)
                        .search(self.section_graphs.iter().map(|(&c, &g)| (c, g)))
                        .visible,
                );
            }
            let graph_visible = has_graphs.then_some(&graph_scratch);
            // A worker-built pyramid only applies to the exact view it was
            // splatted with — a stale camera can't false-cull. Embedder
            // pyramids (`hiz_source_vp == None`) always apply.
            let hiz_rows = match self.hiz_source_vp {
                Some(source) if source == *view_proj => Some(view_proj_row_major(view_proj)),
                Some(_) => None,
                None => self
                    .hiz_pyramid
                    .as_ref()
                    .map(|_| view_proj_row_major(view_proj)),
            };
            for (coord, section) in self.live_sections.iter() {
                if section.opaque_index_total == 0 && section.terrain.translucent_index_count == 0 {
                    continue;
                }
                let min = [
                    coord.0 as f32 * EDGE as f32,
                    coord.1 as f32 * EDGE as f32,
                    coord.2 as f32 * EDGE as f32,
                ];
                // O(1) sub-pixel reject ahead of every other test: a
                // section whose bounding sphere projects under half a
                // pixel covers no sample point — cheaper than Hi-Z or
                // frustum work, and a distance-only check can never
                // false-cull a near or camera-enclosing section.
                if is_subpixel_section(camera_position, min, focal_y_px) {
                    culled.insert(*coord);
                    subpixel_culled += 1;
                    continue;
                }
                // Hi-Z runs before graph/frustum culling: a view-matched
                // pyramid only reports occlusion the previous frame's
                // drawn occluders prove, so a depth-culled section stays
                // culled regardless of which conservative test fires first.
                if let (Some(hiz), Some(rows)) = (&self.hiz_pyramid, &hiz_rows) {
                    let max = [
                        min[0] + EDGE as f32,
                        min[1] + EDGE as f32,
                        min[2] + EDGE as f32,
                    ];
                    if hiz.cull_world_aabb(rows, min, max) {
                        culled.insert(*coord);
                        hiz_culled += 1;
                        continue;
                    }
                }
                let visible = if self.section_graphs.contains_key(coord) {
                    graph_visible.is_some_and(|set| set.contains(coord))
                } else {
                    // The Lipschitz uniform-tile sweep proves strictly
                    // inside/outside planes from one centre sample and
                    // leaves only mixed planes to the exact p-vertex
                    // test — same verdict as `box_in_frustum`, fewer
                    // support-point evaluations away from boundaries.
                    section_frustum_visible(min, &planes)
                };
                if !visible {
                    culled.insert(*coord);
                }
            }
            self.cull_graph_scratch = graph_scratch;
        }
        self.hiz_culled_sections += hiz_culled;
        self.subpixel_culled_sections += subpixel_culled;
        self.cull_hiz_count = hiz_culled;
        self.cull_subpixel_count = subpixel_culled;
        self.cull_cache_key = Some(key);
        self.cull_result = culled;
    }

    /// The lazily-created GPU Hi-Z pyramid + section culler (audit V4),
    /// (re)built when `width`x`height` changes. Callers record a pyramid
    /// build (`build_from_depth` against [`Self::frame_depth_view`] or
    /// `build_from_r32`) through the returned handle before
    /// [`Self::gpu_cull_live_sections`] dispatches the cull pass.
    pub fn gpu_hiz_culler(&mut self, width: u32, height: u32) -> &mut GpuHizCuller {
        let stale = self
            .gpu_hiz
            .as_ref()
            .is_some_and(|hiz| hiz.resolution() != (width, height));
        if stale || self.gpu_hiz.is_none() {
            self.gpu_hiz = Some(GpuHizCuller::new(&self.ctx.device, width, height));
        }
        self.gpu_hiz.as_mut().expect("gpu hiz created")
    }

    /// The frame depth view, when a depth attachment exists — the
    /// production source for [`GpuHizCuller::build_from_depth`].
    pub fn frame_depth_view(&self) -> Option<&wgpu::TextureView> {
        self.depth_view.as_ref()
    }

    /// Rebuild the GPU Hi-Z pyramid from the frame depth buffer at its
    /// current resolution — the per-frame step between the opaque pass
    /// and [`Self::gpu_cull_live_sections`]. Returns `false` until a
    /// depth attachment exists.
    pub fn gpu_hiz_build_from_frame_depth(&mut self) -> bool {
        if self.depth_view.is_none() {
            return false;
        }
        let device = &self.ctx.device;
        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("gpu-hiz-frame-build"),
        });
        let built = self.encode_gpu_hiz_build(&mut encoder);
        self.ctx.queue.submit([encoder.finish()]);
        built
    }

    /// GPU compute section cull over every drawable live section: packs
    /// `SectionGpuRecord`s from `live_sections`, dispatches
    /// `SECTION_CULL_WGSL` against the already-built pyramid and returns
    /// the coords the GPU proves invisible — the same contract
    /// [`Self::cull_result`] carries. With no built pyramid this returns
    /// empty (a zero-initialised pyramid can never produce a false cull).
    pub fn gpu_cull_live_sections(
        &mut self,
        view_proj: &Mat4,
        camera_position: [f32; 3],
    ) -> Vec<SectionCoord> {
        let (mip_count, (hiz_w, hiz_h)) = match &self.gpu_hiz {
            Some(hiz) if hiz.pyramid_ready() => (hiz.mip_count(), hiz.resolution()),
            _ => return Vec::new(),
        };
        let edge = EDGE as f32;
        let mut coords = Vec::new();
        let mut records = Vec::new();
        for (coord, section) in self.live_sections.iter() {
            if section.opaque_index_total == 0 && section.terrain.translucent_index_count == 0 {
                continue;
            }
            let min = [
                coord.0 as f32 * edge,
                coord.1 as f32 * edge,
                coord.2 as f32 * edge,
            ];
            coords.push(*coord);
            records.push(SectionGpuRecord {
                aabb_min: [min[0], min[1], min[2], 0.0],
                aabb_max: [min[0] + edge, min[1] + edge, min[2] + edge, 0.0],
                index_count: section.opaque_index_total,
                first_index: 0,
                base_vertex: 0,
                first_instance: 0,
                _pad: [0; 4],
            });
        }
        if records.is_empty() {
            return Vec::new();
        }
        let count = records.len() as u32;
        let uniforms = CullUniforms {
            view_proj: *view_proj,
            planes: frustum_planes_from_view_proj(view_proj),
            camera_pos: [
                camera_position[0],
                camera_position[1],
                camera_position[2],
                0.0,
            ],
            params: [hiz_w as f32, hiz_h as f32, mip_count as f32, count as f32],
        };
        let device = &self.ctx.device;
        let uniforms_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("gpu-cull-uniforms"),
            size: std::mem::size_of::<CullUniforms>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        self.ctx
            .queue
            .write_buffer(&uniforms_buf, 0, bytemuck::bytes_of(&uniforms));
        let sections_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("gpu-cull-sections"),
            size: (records.len() * std::mem::size_of::<SectionGpuRecord>()) as u64,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        self.ctx
            .queue
            .write_buffer(&sections_buf, 0, bytemuck::cast_slice(&records));
        let args_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("gpu-cull-args"),
            size: (records.len() * std::mem::size_of::<IndirectArgs>()) as u64,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC,
            mapped_at_creation: false,
        });
        let stats_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("gpu-cull-stats"),
            size: std::mem::size_of::<CullStats>() as u64,
            usage: wgpu::BufferUsages::STORAGE
                | wgpu::BufferUsages::COPY_SRC
                | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        self.ctx
            .queue
            .write_buffer(&stats_buf, 0, bytemuck::bytes_of(&CullStats::default()));
        let masks_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("gpu-cull-masks"),
            size: (records.len() * 4) as u64,
            usage: wgpu::BufferUsages::STORAGE,
            mapped_at_creation: false,
        });
        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("gpu-section-cull"),
        });
        let culler = self.gpu_hiz.as_mut().expect("gpu hiz ready");
        culler.encode_cull(
            device,
            &mut encoder,
            &uniforms_buf,
            &sections_buf,
            &args_buf,
            &stats_buf,
            &masks_buf,
            count,
        );
        self.ctx.queue.submit([encoder.finish()]);
        // Blocking readback: the render path consumes culled coords
        // one frame after the dispatch (same latency the CPU Hi-Z path
        // tolerates), so a synchronous map is acceptable here.
        let size = (records.len() * std::mem::size_of::<IndirectArgs>()) as u64;
        let readback = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("gpu-cull-readback"),
            size,
            usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("gpu-cull-readback"),
        });
        encoder.copy_buffer_to_buffer(&args_buf, 0, &readback, 0, size);
        self.ctx.queue.submit([encoder.finish()]);
        let slice = readback.slice(..);
        let (tx, rx) = std::sync::mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |result| {
            let _ = tx.send(result);
        });
        let _ = device.poll(wgpu::PollType::wait_indefinitely());
        let mapped = rx
            .recv()
            .ok()
            .and_then(|r| r.ok())
            .and_then(|_| slice.get_mapped_range().ok())
            .map(|view| view.to_vec());
        readback.unmap();
        let Some(bytes) = mapped else {
            return Vec::new();
        };
        let args: &[IndirectArgs] = bytemuck::cast_slice(&bytes);
        let culled: Vec<SectionCoord> = coords
            .iter()
            .zip(args.iter())
            .filter_map(|(coord, arg)| (arg.instance_count == 0).then_some(*coord))
            .collect();
        self.gpu_hiz_cull_calls += 1;
        self.gpu_hiz_culled += culled.len() as u64;
        culled
    }

    /// Whether the adapter proves `DownlevelFlags::INDIRECT_EXECUTION` —
    /// required for `draw_indexed_indirect`/`multi_draw_indexed_indirect`.
    pub fn indirect_execution_supported(&self) -> bool {
        self.indirect_execution_supported
    }

    /// Enable/disable the GPU-driven opaque arena path. When enabled and
    /// the adapter proves `INDIRECT_EXECUTION`, the composed pass lazily
    /// builds `gpu_hiz`, culls every committed arena section into an
    /// indirect args buffer, and submits the draws straight from GPU
    /// memory — the CPU never reads the args back. Chunk fades keep the
    /// CPU path (they need per-section uniforms).
    pub fn set_gpu_indirect_enabled(&mut self, enabled: bool) {
        self.gpu_indirect_enabled = enabled;
    }

    /// Feed one real frame-time P95 observation (milliseconds) into the
    /// upload budget's adaptive controller — the render/governor loop's
    /// live feedback edge. [`Self::begin_render_upload_frame`] calls this
    /// with the renderer's own measured window; `GpuApp` additionally
    /// feeds the session's `FrameTimingStats` P95 on windowed frames.
    pub fn observe_frame_p95_ms(&mut self, p95_ms: f64) {
        self.upload_budget.adapt(p95_ms * 1_000.0);
        self.upload_budget_p95_observations += 1;
    }

    /// Real P95 observations fed through [`Self::observe_frame_p95_ms`].
    pub fn upload_budget_p95_observations(&self) -> u64 {
        self.upload_budget_p95_observations
    }

    /// Record coords in args-buffer order for the last GPU indirect
    /// frame — the readback mapping for verification tests.
    pub fn gpu_indirect_record_coords(&self) -> &[SectionCoord] {
        self.gpu_indirect
            .as_ref()
            .map_or(&[][..], |b| b.record_coords.as_slice())
    }

    /// The persistent GPU-written indirect args buffer (`None` until the
    /// first indirect frame) — exposed for test/verification readback.
    pub fn gpu_indirect_args_buffer(&self) -> Option<&wgpu::Buffer> {
        self.gpu_indirect.as_ref().map(|b| &b.args)
    }

    /// Test/verification readback of one region arena's vertex buffer
    /// plus `(vertex_high_water, flushed_vertex_len, vertex_capacity)`.
    #[doc(hidden)]
    pub fn debug_region_arena_vertex_buffer(
        &self,
        origin: [i32; 3],
    ) -> Option<(&wgpu::Buffer, u32, u32, usize)> {
        self.region_arenas.get(&origin).map(|a| {
            (
                &a.vertex_buffer,
                a.vertex_high_water,
                a.flushed_vertex_len,
                a.vertex_capacity,
            )
        })
    }

    /// Grow the persistent indirect buffers to `capacity` section
    /// records (powers of two; steady-state frames never reallocate).
    fn ensure_gpu_indirect_capacity(&mut self, capacity: usize) {
        let needs = self
            .gpu_indirect
            .as_ref()
            .is_none_or(|b| b.capacity < capacity);
        if !needs {
            return;
        }
        let capacity = capacity.next_power_of_two().max(16);
        let device = &self.ctx.device;
        self.gpu_indirect = Some(GpuIndirectBuffers {
            sections: device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("gpu-indirect-sections"),
                size: (capacity * std::mem::size_of::<SectionGpuRecord>()) as u64,
                usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            }),
            args: device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("gpu-indirect-args"),
                size: (capacity * std::mem::size_of::<IndirectArgs>()) as u64,
                usage: wgpu::BufferUsages::STORAGE
                    | wgpu::BufferUsages::INDIRECT
                    | wgpu::BufferUsages::COPY_SRC,
                mapped_at_creation: false,
            }),
            stats: device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("gpu-indirect-stats"),
                size: std::mem::size_of::<CullStats>() as u64,
                usage: wgpu::BufferUsages::STORAGE
                    | wgpu::BufferUsages::COPY_SRC
                    | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            }),
            masks: device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("gpu-indirect-masks"),
                size: (capacity * 4) as u64,
                usage: wgpu::BufferUsages::STORAGE,
                mapped_at_creation: false,
            }),
            uniforms: device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("gpu-indirect-uniforms"),
                size: std::mem::size_of::<CullUniforms>() as u64,
                usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            }),
            capacity,
            record_coords: Vec::new(),
            // A fresh (empty) shadow diffs fully dirty, so a
            // grow-realloc uploads the whole record pack in one span.
            record_shadow: Vec::new(),
            packed_world_revision: u64::MAX,
            packed_camera_section: [i32::MIN; 3],
            packed_morton: false,
            cached_runs: Vec::new(),
            cached_count: 0,
        });
    }

    /// Prepare this frame's GPU-driven opaque submission into `encoder`:
    /// pack every committed arena slot into `SectionGpuRecord`s —
    /// regions front-to-back, each region's sections ordered by
    /// camera-distance ring + Morton Z-order — upload them, then
    /// dispatch `SECTION_CULL_WGSL` so the GPU writes `instance_count`
    /// 0/1 straight into the args buffer the opaque pass consumes (zero
    /// CPU readback). Returns `None` when the path is unavailable: flag
    /// off, no `INDIRECT_EXECUTION`, or no committed arena geometry.
    /// Hi-Z occlusion arms only while the pyramid was built under this
    /// exact view-projection — a camera cut or frame 0 dispatches with
    /// `params.z == 0` and never false-culls through stale depth.
    fn encode_gpu_indirect_cull(
        &mut self,
        encoder: &mut wgpu::CommandEncoder,
        view_proj: &Mat4,
        camera_position: [f32; 3],
    ) -> Option<GpuIndirectFrame> {
        if !self.gpu_indirect_enabled || !self.indirect_execution_supported {
            return None;
        }
        let edge = EDGE as f32;
        let cam_section = [
            (camera_position[0] / edge).floor() as i32,
            (camera_position[1] / edge).floor() as i32,
            (camera_position[2] / edge).floor() as i32,
        ];
        // Steady-residency fast path: when neither arena residency nor camera
        // section nor sort policy changed, reuse the already-uploaded GPU
        // `SectionGpuRecord` buffer & runs directly — 0 Vec allocs, 0 sorts,
        // 0 record-shadow memcmp, and 0 `sections` write_buffer bytes!
        let fast_hit = self.gpu_indirect.as_ref().is_some_and(|b| {
            b.cached_count > 0
                && b.packed_world_revision == self.cull_world_revision
                && b.packed_camera_section == cam_section
                && b.packed_morton == self.morton_order_enabled
        });
        if fast_hit {
            let count = self.gpu_indirect.as_ref().expect("fast hit").cached_count;
            let occlusion_armed = self
                .gpu_hiz
                .as_ref()
                .is_some_and(|hiz| hiz.pyramid_ready())
                && self.gpu_hiz_frame_vp == Some(*view_proj);
            let (depth_w, depth_h) = self.depth_size;
            let (hiz_w, hiz_h, mip_count) = {
                let culler = self.gpu_hiz_culler(depth_w, depth_h);
                (
                    culler.resolution().0,
                    culler.resolution().1,
                    culler.mip_count(),
                )
            };
            let uniforms = CullUniforms {
                view_proj: *view_proj,
                planes: frustum_planes_from_view_proj(view_proj),
                camera_pos: [
                    camera_position[0],
                    camera_position[1],
                    camera_position[2],
                    0.0,
                ],
                params: [
                    hiz_w as f32,
                    hiz_h as f32,
                    if occlusion_armed {
                        mip_count as f32
                    } else {
                        0.0
                    },
                    count as f32,
                ],
            };
            let buffers = self.gpu_indirect.as_mut().expect("fast hit");
            self.ctx
                .queue
                .write_buffer(&buffers.uniforms, 0, bytemuck::bytes_of(&uniforms));
            self.ctx
                .queue
                .write_buffer(&buffers.stats, 0, bytemuck::bytes_of(&CullStats::default()));
            self.gpu_indirect_record_uploads_skipped += 1;
            let culler = self.gpu_hiz.as_mut().expect("created above");
            culler.cull_sections_into_indirect_buffer(
                &self.ctx.device,
                encoder,
                &buffers.uniforms,
                &buffers.sections,
                &buffers.args,
                &buffers.stats,
                &buffers.masks,
                count,
            );
            self.gpu_indirect_sections_submitted += u64::from(count);
            return Some(GpuIndirectFrame {
                args: buffers.args.clone(),
                runs: buffers.cached_runs.clone(),
            });
        }
        // Retained scratch: `mem::take` moves the capacity out of `self`
        // for the duration of the pack (the loops below borrow
        // `self.region_arenas` immutably) and the tail of the function
        // returns it — steady frames never allocate.
        let mut records = std::mem::take(&mut self.gpu_indirect_record_scratch);
        let mut record_coords = std::mem::take(&mut self.gpu_indirect_coord_scratch);
        let mut origins = std::mem::take(&mut self.gpu_indirect_origin_scratch);
        records.clear();
        record_coords.clear();
        origins.clear();
        origins.extend(self.region_arenas.keys().copied());
        let mut runs: Vec<GpuIndirectRun> = Vec::new();
        sort_regions_front_to_back(&mut origins, camera_position);
        for origin in origins.iter().copied() {
            let arena = &self.region_arenas[&origin];
            if arena.draw_empty() {
                continue;
            }
            // `pending_vertex_writes` keys are staged but not yet on the
            // GPU — their args would index unwritten vertex memory.
            let mut slots: Vec<(SectionCoord, &ArenaSlot)> = arena
                .slots
                .iter()
                .filter(|(coord, _)| !arena.pending_vertex_writes.contains_key(coord))
                .map(|(&coord, slot)| (coord, slot))
                .collect();
            if slots.is_empty() {
                continue;
            }
            if self.morton_order_enabled {
                slots.sort_by_key(|&(coord, _)| {
                    opaque_section_sort_key(coord, camera_position)
                });
            } else {
                // Deterministic Morton order avoids spurious HashMap-order
                // diffs when `morton_order_enabled` is off.
                slots.sort_by_key(|&(coord, _)| section_morton_key(coord));
            }
            let first = records.len() as u32;
            for (coord, slot) in slots {
                let min = [
                    coord.0 as f32 * edge,
                    coord.1 as f32 * edge,
                    coord.2 as f32 * edge,
                ];
                record_coords.push(coord);
                records.push(SectionGpuRecord {
                    aabb_min: [min[0], min[1], min[2], 0.0],
                    aabb_max: [min[0] + edge, min[1] + edge, min[2] + edge, 0.0],
                    index_count: slot.vertex_count / 4 * 6,
                    first_index: slot.vertex_offset / 4 * 6,
                    base_vertex: 0,
                    first_instance: 0,
                    _pad: [0; 4],
                });
            }
            runs.push(GpuIndirectRun {
                origin,
                first,
                count: records.len() as u32 - first,
            });
        }
        if records.is_empty() {
            self.gpu_indirect_record_scratch = records;
            self.gpu_indirect_coord_scratch = record_coords;
            self.gpu_indirect_origin_scratch = origins;
            return None;
        }
        let count = records.len() as u32;
        self.ensure_gpu_indirect_capacity(records.len());
        // Arm occlusion only when the pyramid is fresh for this exact
        // view; the WGSL early-outs `hiz_occluded` on `params.z == 0`.
        let occlusion_armed = self
            .gpu_hiz
            .as_ref()
            .is_some_and(|hiz| hiz.pyramid_ready())
            && self.gpu_hiz_frame_vp == Some(*view_proj);
        let (depth_w, depth_h) = self.depth_size;
        let (hiz_w, hiz_h, mip_count) = {
            let culler = self.gpu_hiz_culler(depth_w, depth_h);
            (
                culler.resolution().0,
                culler.resolution().1,
                culler.mip_count(),
            )
        };
        let uniforms = CullUniforms {
            view_proj: *view_proj,
            planes: frustum_planes_from_view_proj(view_proj),
            camera_pos: [
                camera_position[0],
                camera_position[1],
                camera_position[2],
                0.0,
            ],
            params: [
                hiz_w as f32,
                hiz_h as f32,
                if occlusion_armed { mip_count as f32 } else { 0.0 },
                count as f32,
            ],
        };
        let buffers = self.gpu_indirect.as_mut().expect("capacity ensured");
        buffers.record_coords.clone_from(&record_coords);
        self.ctx
            .queue
            .write_buffer(&buffers.uniforms, 0, bytemuck::bytes_of(&uniforms));
        // Differential record upload: diff the pack against the shadow
        // of the last uploaded copy — an identical frame costs zero
        // `write_buffer` calls, a residency change costs only the
        // coalesced dirty spans, a grow-realloc (fresh shadow) costs one
        // full-slice write.
        let (span_writes, span_bytes) = write_indirect_record_diff(
            &self.ctx.queue,
            &buffers.sections,
            &mut buffers.record_shadow,
            &records,
        );
        if span_writes == 0 {
            self.gpu_indirect_record_uploads_skipped += 1;
        }
        self.gpu_indirect_record_uploads += span_writes;
        self.gpu_indirect_record_bytes_uploaded += span_bytes;
        self.ctx
            .queue
            .write_buffer(&buffers.stats, 0, bytemuck::bytes_of(&CullStats::default()));
        let culler = self.gpu_hiz.as_mut().expect("created above");
        culler.cull_sections_into_indirect_buffer(
            &self.ctx.device,
            encoder,
            &buffers.uniforms,
            &buffers.sections,
            &buffers.args,
            &buffers.stats,
            &buffers.masks,
            count,
        );
        self.gpu_indirect_sections_submitted += u64::from(count);
        let buffers = self.gpu_indirect.as_mut().expect("capacity ensured");
        buffers.packed_world_revision = self.cull_world_revision;
        buffers.packed_camera_section = cam_section;
        buffers.packed_morton = self.morton_order_enabled;
        buffers.cached_runs.clone_from(&runs);
        buffers.cached_count = count;
        self.gpu_indirect_record_scratch = records;
        self.gpu_indirect_coord_scratch = record_coords;
        self.gpu_indirect_origin_scratch = origins;
        Some(GpuIndirectFrame {
            args: buffers.args.clone(),
            runs,
        })
    }

    /// Record the Hi-Z pyramid build into `encoder` — the caller orders
    /// it after the pass that wrote `depth_view`. Returns `false` until
    /// a depth attachment exists.
    fn encode_gpu_hiz_build(&mut self, encoder: &mut wgpu::CommandEncoder) -> bool {
        if self.depth_view.is_none() {
            return false;
        }
        let (width, height) = self.depth_size;
        {
            let _ = self.gpu_hiz_culler(width, height);
        }
        let culler = self.gpu_hiz.as_mut().expect("created above");
        let view = self.depth_view.as_ref().expect("checked above");
        culler.build_from_depth(&self.ctx.device, encoder, view);
        self.gpu_hiz_pyramid_builds += 1;
        true
    }

    /// The active per-frame chunk-upload budget.
    pub fn upload_budget(&self) -> &ChunkUploadBudget {
        &self.upload_budget
    }

    /// Replace the upload budget outright (tests/embedders tuning caps).
    pub fn set_upload_budget(&mut self, budget: ChunkUploadBudget) {
        self.upload_budget = budget;
    }

    /// The allocated shadow-map resolution, or `None` while shadow
    /// quality is `Off` — the whole `ShadowGpu` (depth array, cascade
    /// views, pipelines, bind groups) stays lazily unallocated until
    /// shadows are first enabled, saving 16–64 MiB on UMA hosts.
    pub fn shadow_map_resolution(&self) -> Option<u32> {
        self.shadow.as_ref().map(|gpu| gpu.config.map_resolution)
    }

    /// Section uploads currently queued awaiting budget admission.
    pub fn pending_upload_count(&self) -> usize {
        self.pending_uploads.len()
    }

    /// Install or clear the Hi-Z occlusion pyramid consulted by the
    /// composed world pass (`None` keeps the conservative graph/frustum
    /// path). An embedder-installed pyramid pauses the automatic worker
    /// rebuild until `set_hiz_pyramid(None)` hands control back.
    pub fn set_hiz_pyramid(&mut self, pyramid: Option<HiZPyramid>) {
        self.hiz_manual = pyramid.is_some();
        self.hiz_source_vp = None;
        self.hiz_pyramid = pyramid;
        self.hiz_revision += 1;
    }

    /// The installed Hi-Z pyramid, if any.
    pub fn hiz_pyramid(&self) -> Option<&HiZPyramid> {
        self.hiz_pyramid.as_ref()
    }

    /// Push an adaptive [`ComputeStrategy`] into the renderer: the memory
    /// topology selects the chunk-upload budget preset, mesh-worker
    /// concurrency hint and [`TransferTopologyBudget`] (staging-ring
    /// slots/bytes, arena slab size, idle-slab reclaim grace, per-frame
    /// DMA cap). Render scale and shadow quality are deliberately NOT
    /// touched — memory topology is a transfer-layer decision, so UMA
    /// keeps the user's `Native100` and chosen shadow preset exactly like
    /// a discrete GPU does. Returns the current negotiated shadow
    /// quality for callers that still consume it.
    pub fn apply_compute_strategy(&mut self, strategy: &ComputeStrategy) -> BuiltinShadowQuality {
        let (budget, workers) = strategy_upload_defaults(strategy);
        self.upload_budget = budget;
        self.mesh_worker_threads = workers;
        self.mesh_pool.set_threads(workers);
        self.transfer_budget =
            TransferTopologyBudget::for_topology(strategy.memory, self.memory_footprint_class);
        self.staging_ring
            .reconfigure(&self.ctx.device, &self.transfer_budget);
        self.compute_strategy_apply_count += 1;
        self.staging_ring_uploads = self.staging_ring.staging_ring_uploads;
        self.staging_ring_allocs = self.staging_ring.staging_ring_allocs;
        self.staging_ring_dma_defers = self.staging_ring.staging_ring_dma_defers;
        self.shadow_quality
    }

    /// The active [`TransferTopologyBudget`] — staging-ring geometry,
    /// arena slab size, idle-slab grace and per-frame DMA cap derived
    /// from the last applied memory topology (never visual settings).
    pub fn transfer_topology_budget(&self) -> TransferTopologyBudget {
        self.transfer_budget
    }

    /// Override the host RAM class used when a strategy's memory topology
    /// re-derives [`Self::transfer_budget`] (embedders that know the
    /// profile better than the detector — e.g. tests — set this before
    /// [`Self::apply_compute_strategy`]).
    pub fn set_memory_footprint_class(&mut self, class: MemoryFootprintClass) {
        self.memory_footprint_class = class;
    }

    /// Times [`Self::apply_compute_strategy`] has configured the renderer.
    pub fn compute_strategy_apply_count(&self) -> u64 {
        self.compute_strategy_apply_count
    }

    /// Whether a full compute strategy has been applied at least once.
    /// Construction seeds only the non-visual halves (upload budget,
    /// mesh-worker hint); this reports explicit full applications.
    pub fn compute_strategy_applied(&self) -> bool {
        self.compute_strategy_apply_count > 0
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
    /// `megu_kernel::evaluate_sky_and_fog` at noon, clear weather).
    /// Store `camera` and upload the frame UBO — with a byte-compare dedup
    /// so an identical camera costs zero `write_buffer` calls. This is the
    /// single frame-UBO write per render call; `render_frame` no longer
    /// uploads a duplicate.
    pub fn set_camera(&mut self, camera: &Camera, aspect: f32) -> SkyFogOutput {
        self.window_camera.set(Some(*camera));
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
        self.write_frame_ubo_dedup(&uniforms);
        sky
    }

    /// Byte-compared upload of `frame_ubo`: unchanged payload ⇒ no
    /// `write_buffer` at all (`ubo_writes_skipped += 1`).
    fn write_frame_ubo_dedup(&mut self, uniforms: &FrameUniforms) {
        let bytes = bytemuck::bytes_of(uniforms);
        if self.last_frame_ubo_bytes.as_slice() == bytes {
            self.ubo_writes_skipped += 1;
            return;
        }
        self.ctx.queue.write_buffer(&self.frame_ubo, 0, bytes);
        self.last_frame_ubo_bytes.clear();
        self.last_frame_ubo_bytes.extend_from_slice(bytes);
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
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
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
        self.ui_cpu_raster_uploads += 1;
        Ok(())
    }

    /// Upload a [`tessellate_draw_list`](megu_ui::draw::tessellate_draw_list)
    /// mesh for the GPU-quad UI overlay: rewrites only the 16-byte
    /// screen-size uniform plus the dynamic vertex/index buffers (grown,
    /// never shrunk). The static font atlas uploaded at construction is
    /// bound every draw. Empty meshes disable the overlay draw.
    pub fn update_ui_mesh(&mut self, width: u32, height: u32, mesh: &UiMesh) {
        let uniforms = [width.max(1) as f32, height.max(1) as f32, 0.0, 0.0];
        self.ctx
            .queue
            .write_buffer(&self.ui_quad.uniform, 0, bytemuck::cast_slice(&uniforms));
        self.ui_quad.index_count = mesh.indices.len() as u32;
        if self.ui_quad.index_count == 0 {
            return;
        }
        self.ui_quad_vertex_scratch.clear();
        self.ui_quad_vertex_scratch
            .extend(mesh.vertices.iter().map(|v| UiQuadVertex {
                position: v.position,
                uv: v.uv,
                color: v.color,
            }));
        let vbytes = (self.ui_quad_vertex_scratch.len() * 32) as u64;
        if vbytes > self.ui_quad.vertex_capacity {
            self.ui_quad.vertex_capacity = vbytes * 2;
            self.ui_quad.vertices = self.ctx.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("ui-quad-vertices"),
                size: self.ui_quad.vertex_capacity,
                usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
        }
        let ibytes = (mesh.indices.len() * 4) as u64;
        if ibytes > self.ui_quad.index_capacity {
            self.ui_quad.index_capacity = ibytes * 2;
            self.ui_quad.indices = self.ctx.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("ui-quad-indices"),
                size: self.ui_quad.index_capacity,
                usage: wgpu::BufferUsages::INDEX | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
        }
        let quad = &self.ui_quad;
        self.ctx.queue.write_buffer(
            &quad.vertices,
            0,
            bytemuck::cast_slice(&self.ui_quad_vertex_scratch),
        );
        self.ctx
            .queue
            .write_buffer(&quad.indices, 0, bytemuck::cast_slice(&mesh.indices));
    }

    /// Draw the UI-quad overlay into `pass`, replaying the pre-recorded
    /// `RenderBundle` while the quad buffer identities and index count
    /// match. Vertex content flows through `write_buffer` on the same
    /// buffers, so identical counts replay fresh content without
    /// re-encoding — only a grow-realloc or count change re-records.
    fn draw_ui_quads(&mut self, pass: &mut wgpu::RenderPass<'_>) {
        let key = UiBundleKey {
            index_count: self.ui_quad.index_count,
            vertex_capacity: self.ui_quad.vertex_capacity,
            index_capacity: self.ui_quad.index_capacity,
        };
        if self.ui_bundle_key == Some(key) {
            if let Some(bundle) = &self.ui_bundle {
                pass.execute_bundles(std::iter::once(bundle));
                self.ui_bundle_hits += 1;
                return;
            }
        }
        let mut bundle_encoder =
            self.ctx
                .device
                .create_render_bundle_encoder(&wgpu::RenderBundleEncoderDescriptor {
                    label: Some("ui-quad-bundle"),
                    color_formats: &[Some(self.target_format)],
                    depth_stencil: None,
                    sample_count: 1,
                    multiview: None,
                });
        bundle_encoder.set_pipeline(&self.ui_quad.pipeline);
        bundle_encoder.set_bind_group(0, Some(&self.ui_quad.bind_group), &[]);
        bundle_encoder.set_vertex_buffer(0, Some(self.ui_quad.vertices.slice(..)));
        bundle_encoder.set_index_buffer(self.ui_quad.indices.slice(..), wgpu::IndexFormat::Uint32);
        bundle_encoder.draw_indexed(0..self.ui_quad.index_count, 0, 0..1);
        let bundle = bundle_encoder.finish(&wgpu::RenderBundleDescriptor {
            label: Some("ui-quad-bundle"),
        });
        pass.execute_bundles(std::iter::once(&bundle));
        self.ui_bundle = Some(bundle);
        self.ui_bundle_key = Some(key);
    }

    // -------------------------------------------------------------------
    // Runtime `MEGU EDITION` title branding
    // -------------------------------------------------------------------

    /// Upload one banner RGBA image as a `UI_QUAD_WGSL`-bound texture:
    /// the shared screen-size uniform, the new texture view and the
    /// shared nearest UI sampler. Malformed CPU payloads (dimension or
    /// byte-count mismatch) fail closed to `None` — no GPU state.
    fn upload_brand_texture(
        &self,
        image: &crate::title_branding::RgbaImage,
        label: &str,
    ) -> Option<BrandingQuadGpu> {
        let expected = image.width as usize * image.height as usize * 4;
        if image.width == 0 || image.height == 0 || image.rgba.len() != expected {
            return None;
        }
        let texture = self.ctx.device.create_texture(&wgpu::TextureDescriptor {
            label: Some(label),
            size: wgpu::Extent3d {
                width: image.width,
                height: image.height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        self.ctx.queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            &image.rgba,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(image.width * 4),
                rows_per_image: Some(image.height),
            },
            wgpu::Extent3d {
                width: image.width,
                height: image.height,
                depth_or_array_layers: 1,
            },
        );
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let bind_group = self.ctx.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some(label),
            layout: &self.title_branding.layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: self.ui_quad.uniform.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(&view),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::Sampler(&self.ui_sampler),
                },
            ],
        });
        Some(BrandingQuadGpu {
            _texture: texture,
            bind_group,
        })
    }

    /// Install (or replace) the title-screen banner textures: the JAR's
    /// `minecraft.png` wordmark and the `edition.png` strip already
    /// patched in memory to `MEGU EDITION`. `None` disables that quad.
    /// One-time PCIe upload at boot — steady frames touch nothing here.
    pub fn set_title_branding(
        &mut self,
        logo: Option<&crate::title_branding::RgbaImage>,
        edition: Option<&crate::title_branding::RgbaImage>,
    ) {
        self.title_branding.logo =
            logo.and_then(|img| self.upload_brand_texture(img, "title-branding-logo"));
        self.title_branding.edition =
            edition.and_then(|img| self.upload_brand_texture(img, "title-branding-edition"));
        // Texture identity changed: re-check the draw gates on the next
        // `update_title_branding` even if the rects did not move.
        self.title_branding.mesh_key = None;
    }

    /// Per-frame title layout: destination rects in UI pixel space for
    /// the logo and edition quads, plus the tessellated splash glyph
    /// mesh (sampled against the font atlas). `None` hides a quad; all
    /// `None` keeps the branding pass fully silent off the title screen.
    /// Rewrites the small vertex/index buffers only when the layout key
    /// moved — steady title frames rewrite nothing.
    pub fn update_title_branding(
        &mut self,
        logo_rect: Option<[f32; 4]>,
        edition_rect: Option<[f32; 4]>,
        splash: Option<&UiMesh>,
    ) {
        // Splash identity = index count + a cheap FNV-1a over its vertex
        // positions/uvs, so a changed line or moved anchor re-uploads.
        let splash_key = splash.map_or(0u64, |mesh| {
            let mut hash = 0xcbf2_9ce4_8422_2325u64;
            for v in &mesh.vertices {
                for f in [v.position[0], v.position[1], v.uv[0], v.uv[1]] {
                    for b in f.to_bits().to_le_bytes() {
                        hash ^= u64::from(b);
                        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
                    }
                }
            }
            hash = hash.wrapping_add(mesh.indices.len() as u64);
            hash
        });
        let key = (logo_rect, edition_rect, splash_key);
        if self.title_branding.mesh_key == Some(key) {
            return;
        }
        self.title_branding.mesh_key = Some(key);
        self.title_branding.logo_draw =
            logo_rect.is_some() && self.title_branding.logo.is_some();
        self.title_branding.edition_draw =
            edition_rect.is_some() && self.title_branding.edition.is_some();

        // Fixed 8-vertex buffer: logo quad at 0..4, edition at 4..8.
        // Absent quads write degenerate (never-drawn) zero verts.
        let mut verts = [UiQuadVertex::default(); 8];
        for (slot, rect) in [logo_rect, edition_rect].into_iter().enumerate() {
            let Some([x, y, w, h]) = rect else {
                continue;
            };
            let (l, t, r, b) = (x, y, x + w, y + h);
            verts[slot * 4] = UiQuadVertex {
                position: [l, t],
                uv: [0.0, 0.0],
                color: [1.0; 4],
            };
            verts[slot * 4 + 1] = UiQuadVertex {
                position: [l, b],
                uv: [0.0, 1.0],
                color: [1.0; 4],
            };
            verts[slot * 4 + 2] = UiQuadVertex {
                position: [r, b],
                uv: [1.0, 1.0],
                color: [1.0; 4],
            };
            verts[slot * 4 + 3] = UiQuadVertex {
                position: [r, t],
                uv: [1.0, 0.0],
                color: [1.0; 4],
            };
        }
        self.ctx.queue.write_buffer(
            &self.title_branding.vertices,
            0,
            bytemuck::cast_slice(&verts),
        );

        // Splash glyph mesh into the grow-only buffers (same pattern as
        // `update_ui_mesh`).
        let branding = &mut self.title_branding;
        branding.splash_index_count = splash.map_or(0, |m| m.indices.len() as u32);
        branding.vertex_scratch.clear();
        if let Some(mesh) = splash {
            branding
                .vertex_scratch
                .extend(mesh.vertices.iter().map(|v| UiQuadVertex {
                    position: v.position,
                    uv: v.uv,
                    color: v.color,
                }));
            let vbytes = (branding.vertex_scratch.len() * 32) as u64;
            if vbytes > branding.splash_vertex_capacity {
                branding.splash_vertex_capacity = vbytes * 2;
                branding.splash_vertices =
                    self.ctx.device.create_buffer(&wgpu::BufferDescriptor {
                        label: Some("title-splash-vertices"),
                        size: branding.splash_vertex_capacity,
                        usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                        mapped_at_creation: false,
                    });
            }
            let ibytes = (mesh.indices.len() * 4) as u64;
            if ibytes > branding.splash_index_capacity {
                branding.splash_index_capacity = ibytes * 2;
                branding.splash_indices =
                    self.ctx.device.create_buffer(&wgpu::BufferDescriptor {
                        label: Some("title-splash-indices"),
                        size: branding.splash_index_capacity,
                        usage: wgpu::BufferUsages::INDEX | wgpu::BufferUsages::COPY_DST,
                        mapped_at_creation: false,
                    });
            }
            self.ctx.queue.write_buffer(
                &branding.splash_vertices,
                0,
                bytemuck::cast_slice(&branding.vertex_scratch),
            );
            self.ctx
                .queue
                .write_buffer(&branding.splash_indices, 0, bytemuck::cast_slice(&mesh.indices));
        }
    }

    /// `true` when a title-branding draw is armed (banner quads or the
    /// splash mesh) — the UI overlay pass opens for either UI quads or
    /// this.
    fn title_branding_pending(&self) -> bool {
        let b = &self.title_branding;
        b.logo_draw || b.edition_draw || b.splash_index_count > 0
    }

    /// Draw the title branding into `pass` after the UI quads: the
    /// wordmark and `MEGU EDITION` strips with their own texture bind
    /// groups, then the splash glyphs through `ui_quad`'s font-atlas
    /// bind group — all through the same `UI_QUAD_WGSL` pipeline.
    fn draw_title_branding(&self, pass: &mut wgpu::RenderPass<'_>) {
        let b = &self.title_branding;
        if !self.title_branding_pending() {
            return;
        }
        pass.set_pipeline(&self.ui_quad.pipeline);
        pass.set_vertex_buffer(0, b.vertices.slice(..));
        pass.set_index_buffer(b.indices.slice(..), wgpu::IndexFormat::Uint32);
        if b.logo_draw {
            if let Some(quad) = &b.logo {
                pass.set_bind_group(0, Some(&quad.bind_group), &[]);
                pass.draw_indexed(0..6, 0, 0..1);
            }
        }
        if b.edition_draw {
            if let Some(quad) = &b.edition {
                pass.set_bind_group(0, Some(&quad.bind_group), &[]);
                pass.draw_indexed(6..12, 0, 0..1);
            }
        }
        if b.splash_index_count > 0 {
            pass.set_bind_group(0, Some(&self.ui_quad.bind_group), &[]);
            pass.set_vertex_buffer(0, b.splash_vertices.slice(..));
            pass.set_index_buffer(b.splash_indices.slice(..), wgpu::IndexFormat::Uint32);
            pass.draw_indexed(0..b.splash_index_count, 0, 0..1);
        }
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
        let params = AtmosphereParams {
            fog_rgb: sky.fog_rgb,
            sky_horizon_rgb: sky.fog_rgb,
            ..AtmosphereParams::default()
        };
        self.render_to_view_with_env(
            view,
            width,
            height,
            sky,
            &BiomeAtmosphereBlender::new(params),
            &ChunkFadeTracker::new(),
            DEMO_WORLD_TIME_TICKS,
            &[],
        );
    }

    /// Render the live window with the current biome, depth tint, chunk
    /// fade progress, live world time and dynamic colored lights.
    /// Requires a camera set by `set_camera` for world draws.
    ///
    /// `world_time_ticks` is the authoritative server clock
    /// (`set_time` day-time); `lights` carries this frame's VFX1 dynamic
    /// point lights (empty slice renders the ambient-only path).
    #[allow(clippy::too_many_arguments)]
    pub fn render_to_view_with_env(
        &mut self,
        view: &wgpu::TextureView,
        width: u32,
        height: u32,
        sky: &SkyFogOutput,
        atmosphere: &BiomeAtmosphereBlender,
        chunk_fades: &ChunkFadeTracker,
        world_time_ticks: u64,
        lights: &[DynamicColoredLight],
    ) {
        self.ensure_depth(width, height);
        let mut encoder = self
            .ctx
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("frame"),
            });
        if self.window_camera.get().is_some() {
            self.encode_window_composed_world(
                &mut encoder,
                view,
                (width, height),
                sky,
                atmosphere,
                chunk_fades,
                world_time_ticks,
                lights,
            );
        } else {
            // No camera => no composed pass; still run the per-frame upload
            // boundary so queued section uploads keep draining.
            self.begin_render_upload_frame();
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
                    if section.arena.is_some() || section.terrain.opaque_index_count == 0 {
                        continue;
                    }
                    pass.set_bind_group(0, &section.bind_group, &[]);
                    pass.set_vertex_buffer(0, section.terrain.opaque_vertices.slice(..));
                    pass.set_index_buffer(
                        self.shared_quad_index_buffer.slice(..),
                        wgpu::IndexFormat::Uint32,
                    );
                    pass.draw_indexed(0..section.terrain.opaque_index_count, 0, 0..1);
                }
                // No camera in this path => no facing cull; every
                // committed live range of each region draws positionally
                // through the shared index buffer.
                if !self.region_arenas.is_empty() {
                    pass.set_pipeline(&self.terrain_region_pipeline);
                    pass.set_index_buffer(
                        self.shared_quad_index_buffer.slice(..),
                        wgpu::IndexFormat::Uint32,
                    );
                    for arena in self.region_arenas.values() {
                        if arena.draw_empty() {
                            continue;
                        }
                        pass.set_bind_group(0, &arena.bind_group, &[]);
                        pass.set_vertex_buffer(0, arena.vertex_buffer.slice(..));
                        arena.for_each_live_range(|start, end| {
                            pass.draw_indexed(start..end, 0, 0..1);
                        });
                    }
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
        // Second pass: the UI overlay — tessellated GPU quads when
        // `update_ui_mesh` installed a mesh, else the legacy CPU-blit
        // framebuffer (no depth attachment; both pipelines are depthless).
        // The `MEGU EDITION` title branding rides the same pass after the
        // UI quads so banners + the splash sit above the screen widgets.
        let mut drew_quad_ui = false;
        if self.ui_quad.index_count > 0 || self.title_branding_pending() {
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
            if self.ui_quad.index_count > 0 {
                self.draw_ui_quads(&mut pass);
                drew_quad_ui = true;
            }
            if self.title_branding_pending() {
                self.draw_title_branding(&mut pass);
                self.title_branding_frames += 1;
            }
        } else if let Some(ui_bind_group) = &self.ui_bind_group {
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
        if drew_quad_ui {
            self.ui_gpu_quad_frames += 1;
        }
        self.ctx.queue.submit([encoder.finish()]);
    }

    /// Window camera renders the live world at its true server time —
    /// day/night cycle, moon-phase cascades and VFX1 dynamic lights all
    /// follow `world_time_ticks`/`lights` exactly like the offscreen
    /// composed-frame path.
    #[allow(clippy::too_many_arguments)]
    fn encode_window_composed_world(
        &mut self,
        encoder: &mut wgpu::CommandEncoder,
        view: &wgpu::TextureView,
        size: (u32, u32),
        sky: &SkyFogOutput,
        atmosphere: &BiomeAtmosphereBlender,
        chunk_fades: &ChunkFadeTracker,
        world_time_ticks: u64,
        lights: &[DynamicColoredLight],
    ) {
        let (width, height) = size;
        let camera = self.window_camera.get().expect("checked by caller");
        let aspect = width.max(1) as f32 / height.max(1) as f32;
        self.prepare_shadow(&camera, aspect, world_time_ticks);
        let decision = self.shadow_decision(&camera, 0, world_time_ticks);
        self.dispatch_shadow_work(encoder, &camera, aspect, decision);

        let atmo = atmosphere.current();
        let sun = compute_sun_direction(world_time_ticks);
        let env = CompositeEnvUniforms {
            sun: [sun[0], sun[1], sun[2], atmo.sun_intensity],
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
        self.write_env_ubo_dedup(&env);
        let lights = VfxLightsGpu::from(&pack_vfx1_lights(lights, self.shadow_quality));
        self.write_lights_ubo_dedup(&lights);
        self.write_frame_uniforms(&camera, aspect, atmo.fog_rgb, sky.fog_start, sky.fog_end);
        // Pending uploads may all be admitted by this frame's drain inside
        // `encode_composed_world`, so reserve draw slots for them too.
        // A bucketed section submits up to `DIRECTION_BUCKETS` opaque
        // draws on the fallback path (arena records merge into fewer).
        let draws = (self.live_sections.len() + self.pending_uploads.len())
            * (DIRECTION_BUCKETS + 1)
            // scene + translucent + one GPU-indirect arena slot
            + 3
            + self.subworld_gpu.entry_count()
            + self.subworld_gpu.batch_count()
            + usize::from(self.entity_ber.is_some())
            + self.particle_draw_slots();
        self.ensure_draw_capacity(draws.max(8));
        let clear_rgb = [
            atmo.sky_horizon_rgb[0].max(atmo.fog_rgb[0] * 0.8),
            atmo.sky_horizon_rgb[1].max(atmo.fog_rgb[1] * 0.8),
            atmo.sky_horizon_rgb[2].max(atmo.fog_rgb[2] * 0.8),
        ];
        // `render_scale < 100%`: the world pass draws into the reduced-res
        // target and the spatial-sharpen pass upscales it onto `view`
        // before the caller's UI overlay (matching the offscreen path).
        let (world_w, world_h) = self.render_scale.scaled_dimensions(width, height);
        if (world_w, world_h) != (width, height) {
            self.ensure_scaled_target(world_w, world_h);
            let target_view = self
                .scaled_target
                .as_ref()
                .map(|t| t.view.clone())
                .expect("ensured above");
            self.encode_composed_world(
                encoder,
                &ComposedWorldTarget {
                    view: &target_view,
                    size: (world_w, world_h),
                    view_proj: camera.view_projection(world_w as f32 / world_h as f32),
                    clear_rgb,
                    chunk_fades,
                    camera_position: camera.position,
                    focal_y_px: camera_focal_y_px(&camera, world_h),
                    world_time_ticks,
                },
            );
            self.encode_sharpen_upscale(encoder, view, (world_w, world_h));
        } else {
            self.encode_composed_world(
                encoder,
                &ComposedWorldTarget {
                    view,
                    size: (width, height),
                    view_proj: camera.view_projection(aspect),
                    clear_rgb,
                    chunk_fades,
                    camera_position: camera.position,
                    focal_y_px: camera_focal_y_px(&camera, height),
                    world_time_ticks,
                },
            );
        }
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
        let mut negotiated = negotiate_builtin_shadow_quality(requested, &self.ctx.report.caps);
        // The adapter report is not a device allocation guarantee. Check the
        // actual logical-device limits before offering a map-backed preset.
        let limits = self.ctx.device.limits();
        while negotiated.config().cascade_count > 0
            && (negotiated.config().map_resolution > limits.max_texture_dimension_2d
                || negotiated.config().cascade_count > limits.max_texture_array_layers
                || limits.max_bind_groups < 4)
        {
            negotiated = match negotiated {
                BuiltinShadowQuality::High => BuiltinShadowQuality::Medium,
                BuiltinShadowQuality::Medium | BuiltinShadowQuality::Natural => {
                    BuiltinShadowQuality::Low
                }
                _ => BuiltinShadowQuality::Off,
            };
        }
        if negotiated != self.shadow_quality {
            self.shadow_cache.invalidate();
            self.shadow = None;
            self.shadow_gpu_epoch += 1;
            self.shadow_active = false;
            self.last_shadow_camera = None;
            self.pending_cascade_layers = 0;
            self.rendered_orthos = [CascadeOrtho {
                center: [0.0; 3],
                radius: 0.0,
                depth_radius: 0.0,
                axes: [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]],
            }; 3];
            self.rendered_lvp = [[[0.0; 4]; 4]; 3];
        }
        self.shadow_quality = negotiated;
        negotiated
    }

    /// Allocated depth-map bytes, including each depth-array layer. This
    /// matches `CascadeConfig::depth_array_bytes()` for the active preset.
    pub fn shadow_allocated_bytes(&self) -> u64 {
        self.shadow
            .as_ref()
            .map_or(0, |s| s.config.depth_array_bytes())
    }

    fn prepare_shadow(&mut self, camera: &Camera, aspect: f32, world_time_ticks: u64) {
        let sun = compute_sun_direction(world_time_ticks);
        // The rotation-invariant state is rebuilt every frame: its camera
        // vectors feed the sky dome even while cascades are off, and the
        // texel-snapped orthos drive the pending-cascade drain.
        self.frame_stable_shadow = Some(build_stable_shadow_uniforms(
            camera,
            aspect,
            self.shadow_quality.config(),
            world_time_ticks,
            ((world_time_ticks / 24_000) % 8) as u8,
        ));
        let valid_camera = camera.position.iter().all(|v| v.is_finite())
            && [
                camera.yaw_deg,
                camera.pitch_deg,
                camera.fov_deg,
                camera.near,
                camera.far,
                aspect,
            ]
            .iter()
            .all(|v| v.is_finite())
            && camera.near > 0.0
            && camera.far > camera.near
            && camera.fov_deg > 1.0
            && camera.fov_deg < 179.0;
        // Cascades stay live across the whole day cycle: below the horizon
        // the caster is the moon at `-sun`, and the sampling shader fades
        // the handover over `|sun.y|` in `0.02..0.08`, so the map only
        // idles inside the fade floor where it reports fully lit anyway.
        let enabled = self.shadow_quality.config().cascade_count > 0
            && sun.iter().all(|v| v.is_finite())
            && sun[1].abs() > 0.02
            && valid_camera;
        if !enabled {
            if self.shadow_active {
                self.shadow_cache.invalidate();
            }
            self.shadow_active = false;
            return;
        }
        if self.shadow.is_none() {
            self.shadow = Some(ShadowGpu::new(
                &self.ctx,
                &self.composite,
                &self.frame_layout,
                self.target_format,
                self.shadow_quality.config(),
            ));
            self.shadow_gpu_epoch += 1;
            self.shadow_cache.invalidate();
        }
        // Yaw and pitch deliberately absent: the bounding-sphere orthos
        // cannot see them, so a pure look rotation marks nothing dirty.
        if let Some(stable) = &self.frame_stable_shadow {
            for (layer, ortho) in stable.orthos.iter().enumerate() {
                if layer < 3 && self.rendered_orthos[layer] != *ortho {
                    self.pending_cascade_layers |= 1 << layer;
                }
            }
        }
        self.shadow_active = true;
    }

    /// Run this frame's shadow depth work. Emergency invalidations
    /// (first frame, preset, world edit, sun move) render every stale
    /// cascade at once; position-only movement (`CameraCrossedSnapRegion`
    /// and pending texel drift) drains ONE cascade per frame so walking
    /// never pays the whole array in a single frame. Pure rotation falls
    /// through to reuse and bumps `shadow_rotation_reuses`.
    fn dispatch_shadow_work(
        &mut self,
        encoder: &mut wgpu::CommandEncoder,
        camera: &Camera,
        aspect: f32,
        decision: ShadowCacheDecision,
    ) {
        if !self.shadow_active {
            return;
        }
        let count = self.shadow_quality.config().cascade_count.min(3);
        let all = (1u8 << count).wrapping_sub(1);
        let mask = match decision {
            ShadowCacheDecision::Rerender(ShadowDirtyReason::CameraCrossedSnapRegion) => {
                self.pending_cascade_layers |= all;
                self.pending_cascade_layers & self.pending_cascade_layers.wrapping_neg()
            }
            ShadowCacheDecision::Rerender(_) => {
                self.pending_cascade_layers |= all;
                all
            }
            ShadowCacheDecision::ReuseCached => {
                self.pending_cascade_layers & self.pending_cascade_layers.wrapping_neg()
            }
        };
        if mask != 0 {
            self.encode_shadow_map(encoder, mask);
        } else {
            self.shadow_reuse_count += 1;
            if let Some(last) = self.last_shadow_camera {
                if (camera.yaw_deg - last[3]).abs() > f32::EPSILON
                    || (camera.pitch_deg - last[4]).abs() > f32::EPSILON
                {
                    self.shadow_rotation_reuses += 1;
                }
            }
        }
        self.write_shadow_receiver_uniform();
        self.last_shadow_camera = Some([
            camera.position[0],
            camera.position[1],
            camera.position[2],
            camera.yaw_deg,
            camera.pitch_deg,
            camera.fov_deg,
            camera.near,
            camera.far,
            aspect,
        ]);
    }

    /// Refresh the receiver uniform the composed pass samples with:
    /// camera fields come from this frame's build (rotation-current
    /// cascade selection) while each cascade's light matrix stays pinned
    /// to the depth map it actually rendered — spread updates and pure
    /// rotation reuse can never desync the pair.
    fn write_shadow_receiver_uniform(&mut self) {
        let (Some(shadow), Some(stable)) =
            (self.shadow.as_ref(), self.frame_stable_shadow.as_ref())
        else {
            return;
        };
        let mut uniforms = stable.uniforms;
        for (index, lvp) in self.rendered_lvp.iter().enumerate() {
            uniforms.light_view_proj[index] = *lvp;
        }
        self.ctx.queue.write_buffer(
            &shadow.receiver_uniform,
            0,
            &shadow_uniform_bytes(&uniforms),
        );
    }

    /// Record + execute shadow caster depth for every cascade in `mask`
    /// (a bitmask over array layers). Each rendered layer refreshes its
    /// entry in `rendered_orthos`/`rendered_lvp` and clears its pending
    /// bit; unrendered layers keep sampling the maps they were rendered
    /// with. Per-cascade AABB culling skips casters outside the snapped
    /// light volume and counted draws go through cached `RenderBundle`s
    /// keyed on the true recorded inputs.
    fn encode_shadow_map(&mut self, encoder: &mut wgpu::CommandEncoder, mask: u8) {
        let stable = *self
            .frame_stable_shadow
            .as_ref()
            .expect("prepare_shadow builds the frame state");
        let count = self
            .shadow
            .as_ref()
            .map_or(0, |s| s.config.cascade_count.min(3) as usize);
        let shadow = self.shadow.as_mut().expect("shadow resources prepared");
        for (layer, buffer) in shadow.caster_uniforms.iter().enumerate() {
            if mask & (1 << layer) == 0 {
                continue;
            }
            self.ctx.queue.write_buffer(
                buffer,
                0,
                &shadow_uniform_bytes(&stable.uniforms.with_active_cascade(layer as u32)),
            );
        }
        let identity: Mat4 = [
            [1.0, 0.0, 0.0, 0.0],
            [0.0, 1.0, 0.0, 0.0],
            [0.0, 0.0, 1.0, 0.0],
            [0.0, 0.0, 0.0, 1.0],
        ];
        let mut draws = Vec::<CompositeDrawUniforms>::new();
        let mut push_model = |model: Mat4| {
            let index = draws.len() as u32;
            draws.push(CompositeDrawUniforms {
                model,
                mvp: model,
                params: [1.0, 1.0, 0.0, 0.0],
                ..Default::default()
            });
            index * DRAW_UNIFORM_STRIDE
        };
        let scene_offset = self
            .scene
            .as_ref()
            .filter(|scene| scene.opaque_index_count > 0)
            .map(|_| push_model(identity));
        let section_offsets: Vec<_> = self
            .live_sections
            .iter()
            .filter(|(_, section)| section.arena.is_none() && section.opaque_index_total > 0)
            .map(|(coord, _)| (*coord, push_model(identity)))
            .collect();
        // Cutout casters: sorted for stable pool slots across recorded
        // bundle replays (a reorder would silently swap section offsets).
        let mut cutout_coords: Vec<SectionCoord> = self
            .live_sections
            .iter()
            .filter(|(_, section)| {
                section.arena.is_none() && section.terrain.cutout_index_count > 0
            })
            .map(|(coord, _)| *coord)
            .collect();
        cutout_coords.sort_unstable();
        if !cutout_coords.is_empty() {
            let atlas_epoch = self.atlas_epoch;
            shadow.ensure_cutout(
                &self.ctx.device,
                &self.frame_layout,
                &self.composite.draw_layout,
                &self.composite.atlas,
                atlas_epoch,
            );
        }
        let cutout_sections: &[SectionCoord] = &cutout_coords;
        let region_offsets: Vec<_> = self
            .region_arenas
            .iter()
            .filter(|(_, arena)| !arena.draw_empty())
            .map(|(origin, _)| (*origin, push_model(identity)))
            .collect();
        let entry_offsets: Vec<_> = self
            .subworld_gpu
            .entries()
            .filter(|entry| entry.index_count > 0 && entry.alpha >= 0.999)
            .map(|entry| push_model(entry.transform.model_matrix()))
            .collect();
        let batch_offsets: Vec<_> = self
            .subworld_gpu
            .batches()
            .filter(|batch| batch.index_count > 0 && batch.instance_count > 0)
            .map(|batch| push_model(batch.transform.model_matrix()))
            .collect();
        shadow.ensure_draw_capacity(
            &self.ctx.device,
            &self.composite.draw_layout,
            draws.len().max(1),
        );
        self.ctx
            .queue
            .write_buffer(&shadow.draw_ubo, 0, bytemuck::cast_slice(&draws));
        // Digest the full candidate caster set: the recorded bundle's
        // draws are a pure function of this set plus the ortho (which
        // selects the AABB-culled subset), both pinned in the key.
        let mut casters = FNV1A_OFFSET;
        if scene_offset.is_some() {
            if let Some(scene) = &self.scene {
                fnv1a_u64(&mut casters, u64::from(scene.opaque_index_count));
            }
        }
        for (coord, _) in &section_offsets {
            fnv1a_u32(&mut casters, coord.0 as u32);
            fnv1a_u32(&mut casters, coord.1 as u32);
            fnv1a_u32(&mut casters, coord.2 as u32);
            fnv1a_u64(
                &mut casters,
                u64::from(self.live_sections[coord].opaque_index_total),
            );
        }
        for coord in &cutout_coords {
            fnv1a_u32(&mut casters, coord.0 as u32);
            fnv1a_u32(&mut casters, coord.1 as u32);
            fnv1a_u32(&mut casters, coord.2 as u32);
            fnv1a_u64(
                &mut casters,
                u64::from(self.live_sections[coord].terrain.cutout_index_count),
            );
        }
        // Atlas replacement rebuilds the cutout bind group — re-record
        // the bundle so it samples the new texture.
        fnv1a_u64(&mut casters, self.atlas_epoch);
        // The shared index buffer's identity is part of the caster set —
        // a growth realloc invalidates recorded draws referencing it.
        fnv1a_u64(&mut casters, u64::from(self.shared_quad_index_capacity));
        for (origin, _) in &region_offsets {
            let arena = &self.region_arenas[origin];
            for v in origin {
                fnv1a_u32(&mut casters, *v as u32);
            }
            fnv1a_u64(&mut casters, u64::from(arena.index_extent()));
            fnv1a_u64(&mut casters, arena.vertex_capacity as u64);
        }
        for entry in self
            .subworld_gpu
            .entries()
            .filter(|entry| entry.index_count > 0 && entry.alpha >= 0.999)
        {
            fnv1a_u64(&mut casters, u64::from(entry.index_count));
            fnv1a_u32(&mut casters, entry.alpha.to_bits());
            for v in entry.transform.position {
                fnv1a_u32(&mut casters, v.to_bits());
            }
            for v in entry.transform.quaternion {
                fnv1a_u32(&mut casters, v.to_bits());
            }
            fnv1a_u32(&mut casters, entry.transform.scale.to_bits());
        }
        for batch in self
            .subworld_gpu
            .batches()
            .filter(|batch| batch.index_count > 0 && batch.instance_count > 0)
        {
            fnv1a_u64(&mut casters, u64::from(batch.mesh_kind));
            fnv1a_u64(&mut casters, u64::from(batch.index_count));
            fnv1a_u64(&mut casters, u64::from(batch.instance_count));
            for v in batch.transform.position {
                fnv1a_u32(&mut casters, v.to_bits());
            }
        }
        fnv1a_u64(&mut casters, dynamic_geometry_digest(&self.entity_ber));
        fnv1a_u64(&mut casters, dynamic_geometry_digest(&self.particles));
        fnv1a_u64(&mut casters, particle_instance_digest(&self.particle_instances));
        // Buffer replacement under identical content — the epoch covers
        // every sync that touched real GPU objects.
        fnv1a_u64(&mut casters, self.subworld_gpu_epoch);
        let frame_bind_group = &self.frame_bind_group;
        let mut culled_total = 0u64;
        for (layer, view) in shadow.layer_views.iter().enumerate() {
            if layer >= count || mask & (1 << layer) == 0 {
                continue;
            }
            let ortho = &stable.orthos[layer];
            let key = CascadeBundleKey {
                layer: layer as u32,
                world_rev: self.cull_world_revision,
                scene_rev: self.scene_revision,
                draw_pool: shadow.draw_pool_epoch,
                casters,
                ortho_hash: cascade_ortho_hash(ortho),
            };
            let bundle = match &shadow.cascade_bundles[layer] {
                Some(cached) if cached.key == key => None,
                _ => {
                    let mut bundle_encoder = self.ctx.device.create_render_bundle_encoder(
                        &wgpu::RenderBundleEncoderDescriptor {
                            label: Some("shadow-cascade-bundle"),
                            color_formats: &[],
                            depth_stencil: Some(wgpu::RenderBundleDepthStencil {
                                format: DEPTH_FORMAT,
                                depth_read_only: false,
                                stencil_read_only: true,
                            }),
                            sample_count: 1,
                            multiview: None,
                        },
                    );
                    let (bundle_entry_offsets, bundle_batch_offsets) =
                        (entry_offsets.as_slice(), batch_offsets.as_slice());
                    culled_total += record_shadow_cascade(
                        &mut bundle_encoder,
                        shadow,
                        frame_bind_group,
                        ortho,
                        self.scene.as_ref().zip(scene_offset),
                        &section_offsets,
                        cutout_sections,
                        &self.live_sections,
                        &region_offsets,
                        &self.region_arenas,
                        &self.shared_quad_index_buffer,
                        bundle_entry_offsets,
                        bundle_batch_offsets,
                        &self.subworld_gpu,
                        layer as u32,
                    );
                    Some(CascadeBundle {
                        key,
                        bundle: bundle_encoder.finish(&wgpu::RenderBundleDescriptor {
                            label: Some("shadow-cascade-bundle"),
                        }),
                    })
                }
            };
            if let Some(recorded) = bundle {
                shadow.cascade_bundles[layer] = Some(recorded);
            }
            let bundle_ref = shadow.cascade_bundles[layer]
                .as_ref()
                .expect("bundle recorded or hit");
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("cascaded-shadow-depth"),
                color_attachments: &[],
                depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                    view,
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
            pass.execute_bundles(std::iter::once(&bundle_ref.bundle));
            drop(pass);
            self.rendered_orthos[layer] = stable.orthos[layer];
            self.rendered_lvp[layer] = stable.uniforms.light_view_proj[layer];
            self.pending_cascade_layers &= !(1 << layer);
        }
        // Lazy stained-glass transmission: the RGBA shadow-color pass
        // only runs while casters were staged — zero cost otherwise.
        if !self.pending_stained_casters.is_empty() {
            let casters = std::mem::take(&mut self.pending_stained_casters);
            shadow.write_stained_casters(&self.ctx.device, &self.ctx.queue, &casters);
        }
        if shadow.stained.as_ref().is_some_and(|s| s.vertex_count > 0) {
            for layer in 0..count {
                if mask & (1 << layer) == 0 {
                    continue;
                }
                let (depth_view, color_view) = {
                    let stained = shadow.stained.as_ref().expect("checked");
                    (&shadow.layer_views[layer], &stained.color_view[layer])
                };
                let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("stained-glass-shadow-color"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: color_view,
                        resolve_target: None,
                        ops: wgpu::Operations {
                            // White = full transmission; casters multiply.
                            load: wgpu::LoadOp::Clear(wgpu::Color::WHITE),
                            store: wgpu::StoreOp::Store,
                        },
                        depth_slice: None,
                    })],
                    depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                        view: depth_view,
                        depth_ops: Some(wgpu::Operations {
                            load: wgpu::LoadOp::Load,
                            store: wgpu::StoreOp::Discard,
                        }),
                        stencil_ops: None,
                    }),
                    timestamp_writes: None,
                    occlusion_query_set: None,
                    multiview_mask: None,
                });
                let stained = shadow.stained.as_ref().expect("checked");
                pass.set_pipeline(&stained.pipeline);
                pass.set_bind_group(0, &shadow.caster_bgs[layer], &[]);
                pass.set_vertex_buffer(0, stained.vertices.slice(..));
                pass.draw(0..stained.vertex_count, 0..1);
            }
        }
        self.shadow_regions_culled += culled_total;
        self.shadow_rerender_count += 1;
    }

    /// The active depth-store policy for the composed world pass —
    /// `TileBasedTbdr` adapters drop the on-chip depth tile after the
    /// pass instead of writing it back.
    fn depth_store_op(&self) -> wgpu::StoreOp {
        // An active GPU Hi-Z culler reads the depth attachment after the
        // pass to build next frame's pyramid — that counts as a real
        // depth consumer, forcing `Store` even on `TileBasedTbdr`.
        select_depth_store_op(self.gpu_paradigm, self.gpu_hiz.is_some())
    }

    /// Report the negotiated [`GpuComputeParadigm`] (host detection at
    /// construction; `LiveSession` re-pushes its caps).
    pub fn gpu_paradigm(&self) -> GpuComputeParadigm {
        self.gpu_paradigm
    }

    /// The depth comparison the post-opaque sky pipeline runs with —
    /// always [`SKY_DEPTH_COMPARE`]; exposed so tests pin the policy to
    /// the live pipeline descriptor rather than a re-declared literal.
    pub fn sky_depth_compare(&self) -> wgpu::CompareFunction {
        SKY_DEPTH_COMPARE
    }

    /// Override the depth-store driver's paradigm — `LiveSession` pushes
    /// its authoritative detection result here.
    pub fn set_gpu_paradigm(&mut self, paradigm: GpuComputeParadigm) {
        self.gpu_paradigm = paradigm;
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
        // The dirty tracker watches the active caster (sun by day, moon at
        // `-sun` by night) so the dusk/dawn handover is a real direction
        // change and re-renders the map exactly once.
        let caster = if sun[1] >= 0.0 {
            sun
        } else {
            v3_scale(sun, -1.0)
        };
        let chunk = [
            (camera.position[0] / 16.0).floor() as i32,
            (camera.position[1] / 16.0).floor() as i32,
            (camera.position[2] / 16.0).floor() as i32,
        ];
        self.last_shadow_decision = self.shadow_cache.should_rerender_shadow_map(
            caster,
            chunk,
            world_revision,
            self.shadow_quality,
        );
        self.last_shadow_decision
    }

    /// Mirror a [`SubWorldRenderBank`] onto the device: uploads only dirty
    /// geometry/instance payloads and clears the bank's dirty flags.
    pub fn sync_subworlds(&mut self, bank: &mut SubWorldRenderBank) {
        let digest = subworld_caster_digest(bank);
        if subworld_casters_dirty(bank) || digest != self.last_shadow_casters {
            self.shadow_cache.invalidate();
            self.subworld_gpu_epoch += 1;
        }
        self.subworld_gpu
            .sync(&self.ctx.device, &self.ctx.queue, bank);
        self.last_shadow_casters = digest;
    }

    /// The `u64` caster digest recorded by the last sub-world sync
    /// ([`subworld_caster_digest`]) — changes only when caster geometry,
    /// transforms, or instance streams actually change.
    pub fn shadow_caster_digest(&self) -> u64 {
        self.last_shadow_casters
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
        &mut self,
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
        self.write_frame_ubo_dedup(&uniforms);
    }

    /// Byte-compared upload of `env_ubo` — steady frames upload zero bytes.
    fn write_env_ubo_dedup(&mut self, env: &CompositeEnvUniforms) {
        let bytes = bytemuck::bytes_of(env);
        if self.last_env_ubo_bytes.as_slice() == bytes {
            self.ubo_writes_skipped += 1;
            return;
        }
        self.ctx
            .queue
            .write_buffer(&self.composite.env_ubo, 0, bytes);
        self.last_env_ubo_bytes.clear();
        self.last_env_ubo_bytes.extend_from_slice(bytes);
    }

    /// Byte-compared upload of `lights_ubo` — steady frames upload zero bytes.
    fn write_lights_ubo_dedup(&mut self, lights: &VfxLightsGpu) {
        let bytes = bytemuck::bytes_of(lights);
        if self.last_lights_ubo_bytes.as_slice() == bytes {
            self.ubo_writes_skipped += 1;
            return;
        }
        self.ctx
            .queue
            .write_buffer(&self.composite.lights_ubo, 0, bytes);
        self.last_lights_ubo_bytes.clear();
        self.last_lights_ubo_bytes.extend_from_slice(bytes);
    }

    /// Write the shared environment + VFX-light uniforms for one composed
    /// frame from the blender's current atmosphere.
    fn write_env_uniforms(&mut self, frame: &ComposedFrame<'_>) {
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
        self.write_env_ubo_dedup(&env);
        let packed = VfxLightsGpu::from(&pack_vfx1_lights(frame.lights, self.shadow_quality));
        self.write_lights_ubo_dedup(&packed);
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
        self.scaled_target = Some(ScaledTarget {
            _texture: texture,
            view,
            sampler,
            sharpen_bg: None,
            size: (width, height),
        });
    }

    /// `MEGU_SPATIAL_SHARPEN_WGSL` resources, created on first sub-native
    /// upscale (`render_scale < Native100`).
    fn ensure_sharpen(&mut self) -> &SharpenGpu {
        if self.sharpen.is_none() {
            let device = &self.ctx.device;
            let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("sharpen-layout"),
                entries: &[
                    wgpu::BindGroupLayoutEntry {
                        binding: 0,
                        visibility: wgpu::ShaderStages::FRAGMENT,
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
                ],
            });
            let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("sharpen-pipeline-layout"),
                bind_group_layouts: &[Some(&layout)],
                immediate_size: 0,
            });
            let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some("spatial-sharpen"),
                source: wgpu::ShaderSource::Wgsl(MEGU_SPATIAL_SHARPEN_WGSL.into()),
            });
            let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("spatial-sharpen"),
                layout: Some(&pipeline_layout),
                vertex: wgpu::VertexState {
                    module: &module,
                    entry_point: Some("vs_main"),
                    compilation_options: wgpu::PipelineCompilationOptions::default(),
                    buffers: &[],
                },
                primitive: wgpu::PrimitiveState {
                    topology: wgpu::PrimitiveTopology::TriangleList,
                    ..Default::default()
                },
                depth_stencil: None,
                multisample: wgpu::MultisampleState::default(),
                fragment: Some(wgpu::FragmentState {
                    module: &module,
                    entry_point: Some("fs_main"),
                    compilation_options: wgpu::PipelineCompilationOptions::default(),
                    targets: &[Some(wgpu::ColorTargetState {
                        format: self.target_format,
                        blend: None,
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                }),
                multiview_mask: None,
                cache: None,
            });
            let uniform = device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("sharpen-uniform"),
                // SharpenUniforms.texel_strength: vec4<f32>.
                size: 16,
                usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            self.sharpen = Some(SharpenGpu {
                pipeline,
                bind_group_layout: layout,
                uniform,
            });
        }
        self.sharpen.as_ref().expect("sharpen ensured")
    }

    /// Fullscreen `MEGU_SPATIAL_SHARPEN_WGSL` upscale: 5-tap edge-adaptive
    /// sharpen of `scaled_target` onto `view` at native size.
    fn encode_sharpen_upscale(
        &mut self,
        encoder: &mut wgpu::CommandEncoder,
        view: &wgpu::TextureView,
        scaled_size: (u32, u32),
    ) {
        self.ensure_sharpen();
        let sharpen = self.sharpen.as_ref().expect("ensured");
        let target = self.scaled_target.as_mut().expect("scaled target ensured");
        if target.sharpen_bg.is_none() {
            target.sharpen_bg = Some(self.ctx.device.create_bind_group(
                &wgpu::BindGroupDescriptor {
                    label: Some("sharpen-bind-group"),
                    layout: &sharpen.bind_group_layout,
                    entries: &[
                        wgpu::BindGroupEntry {
                            binding: 0,
                            resource: sharpen.uniform.as_entire_binding(),
                        },
                        wgpu::BindGroupEntry {
                            binding: 1,
                            resource: wgpu::BindingResource::TextureView(&target.view),
                        },
                        wgpu::BindGroupEntry {
                            binding: 2,
                            resource: wgpu::BindingResource::Sampler(&target.sampler),
                        },
                    ],
                },
            ));
        }
        let texel = [
            1.0 / scaled_size.0.max(1) as f32,
            1.0 / scaled_size.1.max(1) as f32,
            0.6,
            0.25,
        ];
        self.ctx
            .queue
            .write_buffer(&sharpen.uniform, 0, bytemuck::cast_slice(&texel));
        let sharpen = self.sharpen.as_ref().expect("ensured");
        let bg = target.sharpen_bg.as_ref().expect("created");
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("spatial-sharpen-upscale"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                    store: wgpu::StoreOp::Store,
                },
                depth_slice: None,
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });
        pass.set_pipeline(&sharpen.pipeline);
        pass.set_bind_group(0, bg, &[]);
        pass.draw(0..3, 0..1);
    }

    /// `MEGU_WATER_SURFACE_WGSL` resources — the pass binds the frame's
    /// shadow-receiver uniform (camera + celestial state) so water picks
    /// up day/night shading for free. Created on first decoded water.
    fn ensure_water(&mut self) -> &WaterGpu {
        if self.water.is_none() {
            let device = &self.ctx.device;
            let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("water-layout"),
                entries: &[wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                }],
            });
            let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("water-pipeline-layout"),
                bind_group_layouts: &[Some(&layout)],
                immediate_size: 0,
            });
            let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some("water-surface"),
                source: wgpu::ShaderSource::Wgsl(MEGU_WATER_SURFACE_WGSL.into()),
            });
            let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("water-surface"),
                layout: Some(&pipeline_layout),
                vertex: wgpu::VertexState {
                    module: &module,
                    entry_point: Some("vs_main"),
                    compilation_options: wgpu::PipelineCompilationOptions::default(),
                    buffers: &[Some(WATER_VERTEX_LAYOUT)],
                },
                primitive: wgpu::PrimitiveState {
                    topology: wgpu::PrimitiveTopology::TriangleList,
                    cull_mode: None,
                    ..Default::default()
                },
                depth_stencil: Some(wgpu::DepthStencilState {
                    format: DEPTH_FORMAT,
                    depth_write_enabled: Some(false),
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
                        format: self.target_format,
                        blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                }),
                multiview_mask: None,
                cache: None,
            });
            let uniform = device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("water-uniform"),
                size: std::mem::size_of::<ShadowUniforms>() as u64,
                usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("water-bind-group"),
                layout: &layout,
                entries: &[wgpu::BindGroupEntry {
                    binding: 0,
                    resource: uniform.as_entire_binding(),
                }],
            });
            self.water = Some(WaterGpu {
                pipeline,
                uniform,
                bind_group,
            });
        }
        self.water.as_ref().expect("water ensured")
    }

    /// Stage stained-glass casters for the next shadow encode. The lazy
    /// `MEGU_STAINED_GLASS_SHADOW_COLOR_WGSL` pass runs only while this is
    /// non-empty — clear it to drop the glass cost back to zero.
    pub fn upload_stained_glass_casters(&mut self, casters: &[StainedCasterVertex]) {
        self.pending_stained_casters.clear();
        self.pending_stained_casters.extend_from_slice(casters);
    }

    /// Upload an explicitly decoded `LitTerrainVertex` mesh for the
    /// `MEGU_TERRAIN_LIT_WGSL` receiver path (sky/block light decoded
    /// from `light_attr`). Requires active shadow resources.
    pub fn upload_lit_terrain(
        &mut self,
        vertices: &[LitTerrainVertex],
        indices: &[u32],
    ) -> Result<(), RendererError> {
        if vertices.is_empty() || indices.is_empty() {
            self.lit_terrain = None;
            return Ok(());
        }
        let Some(shadow) = self.shadow.as_ref() else {
            return Err(RendererError::Device(
                "upload_lit_terrain requires an active shadow preset".into(),
            ));
        };
        let device = &self.ctx.device;
        if self
            .lit_terrain
            .as_ref()
            .is_some_and(|l| l.shadow_epoch != self.shadow_gpu_epoch)
        {
            self.lit_terrain = None;
        }
        if self.lit_terrain.is_none() {
            let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("lit-terrain-layout"),
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
                            sample_type: wgpu::TextureSampleType::Depth,
                            view_dimension: wgpu::TextureViewDimension::D2Array,
                            multisampled: false,
                        },
                        count: None,
                    },
                    wgpu::BindGroupLayoutEntry {
                        binding: 2,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Comparison),
                        count: None,
                    },
                    wgpu::BindGroupLayoutEntry {
                        binding: 3,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Buffer {
                            ty: wgpu::BufferBindingType::Uniform,
                            has_dynamic_offset: false,
                            min_binding_size: None,
                        },
                        count: None,
                    },
                ],
            });
            let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("lit-terrain-bind-group"),
                layout: &layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: shadow.receiver_uniform.as_entire_binding(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::TextureView(&shadow.array_view),
                    },
                    wgpu::BindGroupEntry {
                        binding: 2,
                        resource: wgpu::BindingResource::Sampler(&shadow.compare_sampler),
                    },
                    wgpu::BindGroupEntry {
                        binding: 3,
                        resource: self.composite.lights_ubo.as_entire_binding(),
                    },
                ],
            });
            let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("lit-terrain-pipeline-layout"),
                bind_group_layouts: &[Some(&layout)],
                immediate_size: 0,
            });
            let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some("terrain-lit"),
                source: wgpu::ShaderSource::Wgsl(MEGU_TERRAIN_LIT_WGSL.into()),
            });
            let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("terrain-lit"),
                layout: Some(&pipeline_layout),
                vertex: wgpu::VertexState {
                    module: &module,
                    entry_point: Some("vs_main"),
                    compilation_options: wgpu::PipelineCompilationOptions::default(),
                    buffers: &[Some(LIT_TERRAIN_VERTEX_LAYOUT)],
                },
                primitive: wgpu::PrimitiveState {
                    topology: wgpu::PrimitiveTopology::TriangleList,
                    ..Default::default()
                },
                depth_stencil: Some(wgpu::DepthStencilState {
                    format: DEPTH_FORMAT,
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
                        format: self.target_format,
                        blend: None,
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                }),
                multiview_mask: None,
                cache: None,
            });
            self.lit_terrain = Some(LitTerrainGpu {
                pipeline,
                bind_group,
                vertices: self.empty_vertex_buffer.clone(),
                indices: self.empty_index_buffer.clone(),
                index_count: 0,
                shadow_epoch: self.shadow_gpu_epoch,
            });
        }
        let lit = self.lit_terrain.as_mut().expect("created");
        lit.vertices = typed_vertex_buffer(device, &self.ctx.queue, "lit-terrain-verts", vertices);
        lit.indices = index_buffer(device, &self.ctx.queue, "lit-terrain-indices", indices);
        lit.index_count = indices.len() as u32;
        Ok(())
    }

    /// Encode the whole world pass (terrain + sub-worlds + instanced parts)
    /// into `target.view` at `target.size`, clearing to the atmosphere
    /// horizon. `target.view_proj` must already match the target aspect.
    ///
    /// `MEGU_SKY_ATMOSPHERE_WGSL` needs a separate fullscreen pass with its
    /// own group-0 shadow uniform; this pass currently clears the sky and uses
    /// group 0 for terrain frame/section data. `MEGU_WATER_SURFACE_WGSL`
    /// requires float3 world-position and per-vertex `depth_hint` inputs,
    /// whereas translucent terrain uploads packed block vertices without a
    /// water-depth attribute. Both require dedicated integration work.
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
            camera_position,
            focal_y_px,
            world_time_ticks,
        } = *target;
        self.ensure_depth(width, height);
        // Lazily stand up the dedicated water pass the first frame any
        // resident section carries decoded water geometry, and refresh
        // its shadow-uniform (stable frame state when shadow resources
        // are active, else a local fallback from the live world clock).
        if self
            .live_sections
            .values()
            .any(|s| s.terrain.water_index_count > 0)
        {
            self.ensure_water();
            let uniforms = self
                .frame_stable_shadow
                .map(|s| s.uniforms)
                .unwrap_or_else(|| {
                    water_fallback_uniforms(view_proj, camera_position, world_time_ticks)
                });
            let water = self.water.as_ref().expect("ensured");
            self.ctx
                .queue
                .write_buffer(&water.uniform, 0, &shadow_uniform_bytes(&uniforms));
        }
        self.begin_render_upload_frame();
        self.cull_live_sections(&view_proj, camera_position, focal_y_px);
        self.culled_sections += self.cull_result.len() as u64;
        self.last_frame_culled_sections = self.cull_result.len();
        // Hand the drawn occluder set to the worker pool while the GPU
        // encodes: the finished Hi-Z pyramid lands on a later frame's
        // upload boundary.
        self.submit_hiz_job(&view_proj, camera_position);
        // GPU-driven opaque path (audit GPU-INDIRECT-AND-OPT-WIRE): cull
        // the committed arena sections straight into the indirect args
        // buffer before the pass encodes draws — the compute dispatch
        // orders ahead of the render pass inside this same encoder, and
        // the pass below consumes the GPU-written args with zero CPU
        // readback. `None` keeps the CPU arena path (flag off, no
        // INDIRECT_EXECUTION, or no committed geometry yet).
        let gpu_indirect = self.encode_gpu_indirect_cull(encoder, &view_proj, camera_position);
        // Opaque-terrain bundle key: every input the recorded commands
        // depend on. `world_revision` covers residency/graph/cull-set
        // changes (and arena buffer replacement bumps it too), the
        // view-proj bits pin the camera and culled set, `fades_active`
        // gates caching entirely — animated fades bake into draw
        // uniforms and must re-encode every frame.
        let camera_section = SectionCoord(
            (camera_position[0] / EDGE as f32).floor() as i32,
            (camera_position[1] / EDGE as f32).floor() as i32,
            (camera_position[2] / EDGE as f32).floor() as i32,
        );
        let mut view_proj_bits = [0u32; 16];
        for (dst, src) in view_proj_bits.iter_mut().zip(view_proj.iter().flatten()) {
            *dst = src.to_bits();
        }
        // Indirect draws carry no per-section uniforms, so bundles are
        // only eligible when the CPU path owns the frame — recording an
        // indirect-args submission would snapshot a buffer handle the
        // next frame's record rebuild may have grown past.
        let bundle_eligible = !chunk_fades.any_fading() && gpu_indirect.is_none();
        let terrain_bundle_key = bundle_eligible.then_some(TerrainBundleKey {
            camera_section: (camera_section.0, camera_section.1, camera_section.2),
            view_proj_bits,
            world_revision: self.cull_world_revision,
            hiz_revision: self.hiz_revision,
            scene_revision: self.scene_revision,
            fades_active: false,
            shadow_active: self.shadow_active,
        });
        let bundle_hit = terrain_bundle_key.is_some()
            && self.terrain_bundle_key == terrain_bundle_key
            && self.terrain_bundle.is_some();
        let terrain_bundle = if bundle_hit {
            self.terrain_bundle_hits += 1;
            self.terrain_bundle.as_ref()
        } else {
            if bundle_eligible && self.terrain_bundle.is_some() {
                // Key rotated: the recorded commands no longer apply.
                self.terrain_bundle = None;
                self.terrain_bundle_key = None;
            }
            None
        };
        let bundle_stats = self.terrain_bundle_stats;
        let depth_store = self.depth_store_op();
        if depth_store == wgpu::StoreOp::Discard {
            self.tbdr_depth_discards += 1;
        }
        // Split disjoint field borrows so the scratch writer and the GPU
        // handles can coexist inside the render pass.
        let culled_sections = &self.cull_result;
        let gpu = &mut self.composite;
        let shadow = self.shadow.as_ref().filter(|_| self.shadow_active);
        let scene = &self.scene;
        let live_sections = &self.live_sections;
        let region_arenas = &self.region_arenas;
        let shared_index = &self.shared_quad_index_buffer;
        let mut skipped_buckets = 0u64;
        let mut arena_draws = 0u64;
        let mut arena_sections = 0usize;
        let mut arena_regions = 0usize;
        let mut instanced_draws = 0u64;
        let subworld_gpu = &self.subworld_gpu;
        let entity_ber = &self.entity_ber;
        let particles = &self.particles;
        let particle_instances = &self.particle_instances;
        let particle_alphas = &self.particle_alphas;
        let debug_lines = &self.debug_lines;
        let debug_line_pipeline = &self.debug_line_pipeline;
        let frame_bind_group = &self.frame_bind_group;
        let depth_view = self.depth_view.as_ref().expect("depth ensured");
        let sky = &self.sky;
        let water = &self.water;
        let lit_terrain = &self.lit_terrain;
        // Pack `gbuffers_*` sub-world pipelines take over the
        // lit/instanced draws when a `MeguWgslPack` installed them;
        // modules binding group 3 (the shadow receiver) only run while
        // cascades are live, so shadow-free frames keep the built-in
        // pairs. Handles are cheap `Arc` clones — copying them here
        // keeps the pass scope free of `self` borrows.
        let pack_subworld = self
            .pack_subworld
            .as_ref()
            .filter(|p| !p.needs_shadow || shadow.is_some());
        let lit_opaque_pipeline = pack_subworld
            .and_then(|p| p.lit.clone())
            .unwrap_or_else(|| shadow.map_or(&gpu.lit_pipeline, |s| &s.lit_color).clone());
        let instanced_pipeline = pack_subworld
            .and_then(|p| p.instanced.clone())
            .unwrap_or_else(|| {
                shadow
                    .map_or(&gpu.instanced_pipeline, |s| &s.instanced_color)
                    .clone()
            });
        let lit_alpha_pipeline = pack_subworld
            .and_then(|p| p.lit_alpha.clone())
            .unwrap_or_else(|| {
                shadow
                    .map_or(&gpu.lit_alpha_pipeline, |s| &s.lit_alpha_color)
                    .clone()
            });
        if let Some(stable) = &self.frame_stable_shadow {
            // The dome reads only camera vectors, sun, render/celestial
            // params — all carried by the frame's stable uniform build.
            self.ctx
                .queue
                .write_buffer(&sky.uniform, 0, &shadow_uniform_bytes(&stable.uniforms));
        }
        gpu.draw_scratch.clear();
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
                        store: depth_store,
                    }),
                    stencil_ops: None,
                }),
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            pass.set_bind_group(2, &gpu.env_bg, &[]);
            if let Some(shadow) = shadow {
                pass.set_bind_group(3, &shadow.receiver_bg, &[]);
            }

            // Opaque terrain: replayed from the pre-recorded RenderBundle
            // when (camera section, view-proj bits, world/hi-z/scene
            // revisions, fade-activity, shadow pipeline) all match the
            // record key — one `execute_bundles` call, zero Rust draw
            // loop, zero `HashSet` allocs. Chunk-fade animation bakes its
            // values into the recorded draw uniforms, so while any fade
            // is live the pass encodes directly instead. Terrain slots
            // always occupy `draw_scratch[0..T]` — seeded from the
            // recorded `terrain_draw_scratch` slice on replay so the
            // bundle's dynamic offsets still resolve.
            if let Some(bundle) = terrain_bundle {
                pass.execute_bundles(std::iter::once(bundle));
                gpu.draw_scratch
                    .extend_from_slice(&gpu.terrain_draw_scratch);
                skipped_buckets += bundle_stats.0;
                arena_draws += bundle_stats.1;
                arena_sections += bundle_stats.2;
                arena_regions += bundle_stats.3;
            } else {
                let stats = if bundle_eligible {
                    let mut bundle_encoder = self.ctx.device.create_render_bundle_encoder(
                        &wgpu::RenderBundleEncoderDescriptor {
                            label: Some("terrain-opaque-bundle"),
                            color_formats: &[Some(self.target_format)],
                            depth_stencil: Some(wgpu::RenderBundleDepthStencil {
                                format: DEPTH_FORMAT,
                                depth_read_only: false,
                                stencil_read_only: true,
                            }),
                            sample_count: 1,
                            multiview: None,
                        },
                    );
                    let stats = encode_opaque_terrain(
                        &mut bundle_encoder,
                        shadow.map_or(&gpu.terrain_opaque_pipeline, |s| &s.terrain_color),
                        shadow.map_or(&gpu.terrain_region_pipeline, |s| &s.terrain_region_color),
                        &gpu.draw_bg,
                        &mut gpu.draw_scratch,
                        scene.as_ref(),
                        live_sections,
                        region_arenas,
                        shared_index,
                        culled_sections,
                        camera_position,
                        chunk_fades,
                        frame_bind_group,
                        &gpu.env_bg,
                        shadow.map(|s| &s.receiver_bg),
                        &view_proj,
                        true,
                    );
                    let bundle = bundle_encoder.finish(&wgpu::RenderBundleDescriptor {
                        label: Some("terrain-opaque-bundle"),
                    });
                    pass.execute_bundles(std::iter::once(&bundle));
                    gpu.terrain_draw_scratch.clone_from(&gpu.draw_scratch);
                    self.terrain_bundle = Some(bundle);
                    self.terrain_bundle_key = terrain_bundle_key;
                    self.terrain_bundle_stats = stats;
                    self.terrain_bundle_records += 1;
                    stats
                } else {
                    self.terrain_bundle = None;
                    self.terrain_bundle_key = None;
                    encode_opaque_terrain(
                        &mut pass,
                        shadow.map_or(&gpu.terrain_opaque_pipeline, |s| &s.terrain_color),
                        shadow.map_or(&gpu.terrain_region_pipeline, |s| &s.terrain_region_color),
                        &gpu.draw_bg,
                        &mut gpu.draw_scratch,
                        scene.as_ref(),
                        live_sections,
                        region_arenas,
                        shared_index,
                        culled_sections,
                        camera_position,
                        chunk_fades,
                        frame_bind_group,
                        &gpu.env_bg,
                        shadow.map(|s| &s.receiver_bg),
                        &view_proj,
                        gpu_indirect.is_none(),
                    )
                };
                skipped_buckets += stats.0;
                arena_draws += stats.1;
                arena_sections += stats.2;
                arena_regions += stats.3;
            }
            // `execute_bundles` clears the pass's bind-group state, so
            // re-bind env (group 2) and the shadow receiver (group 3)
            // for the translucent/entity/UI draws that follow.
            pass.set_bind_group(2, &gpu.env_bg, &[]);
            if let Some(shadow) = shadow {
                pass.set_bind_group(3, &shadow.receiver_bg, &[]);
            }
            // GPU-driven arena draws: one contiguous args run per
            // region in front-to-back order — `multi_draw_indexed_indirect`
            // for multi-section runs, the portable per-slot
            // `draw_indexed_indirect` for single-record runs. Culled
            // sections carry `instance_count == 0` and cost no draws.
            if let Some(indirect) = gpu_indirect.as_ref() {
                pass.set_pipeline(
                    shadow.map_or(&gpu.terrain_region_pipeline, |s| &s.terrain_region_color),
                );
                pass.set_index_buffer(shared_index.slice(..), wgpu::IndexFormat::Uint32);
                let index = gpu.draw_scratch.len();
                gpu.draw_scratch.push(CompositeDrawUniforms {
                    mvp: view_proj,
                    model: [
                        [1.0, 0.0, 0.0, 0.0],
                        [0.0, 1.0, 0.0, 0.0],
                        [0.0, 0.0, 1.0, 0.0],
                        [0.0, 0.0, 0.0, 1.0],
                    ],
                    params: [1.0, 1.0, 0.0, 0.0],
                    params2: [0.0; 4],
                    _stride_pad: [0.0; 24],
                });
                let off = index as u32 * DRAW_UNIFORM_STRIDE;
                for run in &indirect.runs {
                    let arena = &region_arenas[&run.origin];
                    pass.set_bind_group(0, Some(&arena.bind_group), &[]);
                    pass.set_bind_group(1, Some(&gpu.draw_bg), &[off]);
                    pass.set_vertex_buffer(0, Some(arena.vertex_buffer.slice(..)));
                    if run.count > 1 {
                        pass.multi_draw_indexed_indirect(
                            &indirect.args,
                            u64::from(run.first) * 20,
                            run.count,
                        );
                    } else {
                        pass.draw_indexed_indirect(&indirect.args, u64::from(run.first) * 20);
                    }
                    arena_draws += 1;
                    arena_sections += run.count as usize;
                    arena_regions += 1;
                }
                self.gpu_indirect_draw_passes += 1;
            }
            // `MEGU_TERRAIN_LIT_WGSL` receiver draws — explicitly
            // uploaded decoded-vertex terrain with sky/block light in
            // the attribute word (the packed section path stays on the
            // atlas composite pipelines).
            if let Some(lit) = lit_terrain {
                if lit.index_count > 0 {
                    pass.set_pipeline(&lit.pipeline);
                    pass.set_bind_group(0, &lit.bind_group, &[]);
                    pass.set_vertex_buffer(0, lit.vertices.slice(..));
                    pass.set_index_buffer(lit.indices.slice(..), wgpu::IndexFormat::Uint32);
                    pass.draw_indexed(0..lit.index_count, 0, 0..1);
                }
            }
            // Sky dome behind all opaque geometry: the depth-1.0
            // fullscreen triangle passes `CompareFunction::Equal` only on
            // untouched pixels, so terrain-occluded sky never runs a
            // fragment — identical final image, minimal sky shading.
            pass.set_pipeline(&sky.pipeline);
            pass.set_bind_group(0, &sky.bind_group, &[]);
            pass.draw(0..3, 0..1);
            // Non-terrain draws continue in `draw_scratch` past the
            // terrain slice so one `write_buffer` covers the whole frame.
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

            // Draw-order contract: every depth-writing opaque draw —
            // terrain (above), opaque 6-DOF sub-worlds, instanced
            // mechanical parts, entities/BER — lands before the first
            // alpha-blended one, so translucent terrain water
            // depth-tests against and blends over submerged Create
            // water wheels, cogwheels, shafts and contraptions.
            // Alpha-blended sub-worlds and particles follow water;
            // debug lines draw last.

            // 6-DOF sub-worlds, opaque entries (airship hulls, planets,
            // contraptions).
            let mut drew_opaque_subworld = false;
            for entry in subworld_gpu.entries() {
                if entry.index_count == 0 || entry.alpha < 0.999 {
                    continue;
                }
                if !drew_opaque_subworld {
                    pass.set_pipeline(&lit_opaque_pipeline);
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
            // Opaque GPU-instanced mechanical parts (Create water
            // wheels, cogwheels, shafts, presses).
            let mut drew_instanced = false;
            for batch in subworld_gpu.batches() {
                if batch.index_count == 0 || batch.instance_count == 0 {
                    continue;
                }
                if !drew_instanced {
                    pass.set_pipeline(&instanced_pipeline);
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
                instanced_draws += 1;
            }
            // Opaque entities and block entities.
            if let Some(mesh) = entity_ber {
                pass.set_pipeline(&lit_opaque_pipeline);
                pass.set_bind_group(0, &gpu.lit_frame_bg, &[]);
                let off = slot(identity, 1.0, 1.0);
                pass.set_bind_group(1, &gpu.draw_bg, &[off]);
                pass.set_vertex_buffer(0, mesh.vertices.slice(..));
                pass.set_index_buffer(mesh.indices.slice(..), wgpu::IndexFormat::Uint32);
                pass.draw_indexed(0..mesh.index_count, 0, 0..1);
            }

            // Translucent terrain (stained glass) through the
            // alpha-blended composite variant. Short-circuit before
            // `set_pipeline` when nothing in the world can draw here.
            let any_translucent = scene
                .as_ref()
                .is_some_and(|s| s.translucent_index_count > 0)
                || live_sections
                    .values()
                    .any(|s| s.terrain.translucent_index_count > 0);
            if any_translucent {
                pass.set_pipeline(&gpu.terrain_alpha_pipeline);
            }
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
            if any_translucent {
                for (coord, section) in live_sections.iter() {
                    if culled_sections.contains(coord)
                        || section.terrain.translucent_index_count == 0
                    {
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
            }

            // Dedicated water pass: decoded world-space stream through
            // `MEGU_WATER_SURFACE_WGSL` (waves, depth tint, sun/moon
            // specular via the shadow uniform written above). Encoded
            // after every opaque stream so the surface depth-tests
            // against and alpha-blends over submerged water wheels,
            // shafts, contraptions and entities.
            if let Some(water) = water {
                let mut pipeline_set = false;
                for (coord, section) in live_sections.iter() {
                    if culled_sections.contains(coord) || section.terrain.water_index_count == 0 {
                        continue;
                    }
                    if !pipeline_set {
                        pass.set_pipeline(&water.pipeline);
                        pass.set_bind_group(0, &water.bind_group, &[]);
                        pipeline_set = true;
                    }
                    pass.set_vertex_buffer(0, section.terrain.water_vertices.slice(..));
                    pass.set_index_buffer(
                        section.terrain.water_indices.slice(..),
                        wgpu::IndexFormat::Uint32,
                    );
                    pass.draw_indexed(0..section.terrain.water_index_count, 0, 0..1);
                }
            }

            // Alpha-blended sub-world entries (cloud shells, glass
            // contraptions) — the last world geometry, over water.
            let mut drew_alpha = false;
            for entry in subworld_gpu.entries() {
                if entry.index_count == 0 || entry.alpha >= 0.999 {
                    continue;
                }
                if !drew_alpha {
                    pass.set_pipeline(&lit_alpha_pipeline);
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

            // GPU-instanced billboards: one indexed draw expands the
            // static unit quad per staged `ParticleInstance` — the VS
            // rebuilds each quad's corners from its center + billboard
            // axes and `lit_fs` blends `color.a` exactly like the old
            // per-draw uniform alpha did.
            if let Some(ring) = particle_instances {
                if ring.count > 0 {
                    if let Some(billboards) = gpu.billboard.as_ref() {
                        pass.set_pipeline(
                            shadow
                                .and_then(|s| s.billboard_color.as_ref())
                                .unwrap_or(&billboards.pipeline),
                        );
                        pass.set_bind_group(0, &gpu.lit_frame_bg, &[]);
                        let off = slot(identity, 1.0, 1.0);
                        pass.set_bind_group(1, &gpu.draw_bg, &[off]);
                        pass.set_vertex_buffer(0, billboards.quad_vertices.slice(..));
                        pass.set_vertex_buffer(1, ring.buffer.slice(..));
                        pass.set_index_buffer(
                            billboards.quad_indices.slice(..),
                            wgpu::IndexFormat::Uint32,
                        );
                        pass.draw_indexed(0..6, 0, 0..ring.count);
                    }
                }
            }
            if let Some(mesh) = particles {
                pass.set_pipeline(&lit_alpha_pipeline);
                pass.set_bind_group(0, &gpu.lit_frame_bg, &[]);
                pass.set_vertex_buffer(0, mesh.vertices.slice(..));
                pass.set_index_buffer(mesh.indices.slice(..), wgpu::IndexFormat::Uint32);
                for (i, &alpha) in particle_alphas.iter().enumerate() {
                    let off = slot(identity, 1.0, alpha);
                    pass.set_bind_group(1, &gpu.draw_bg, &[off]);
                    pass.draw_indexed((i as u32 * 6)..((i as u32 + 1) * 6), 0, 0..1);
                }
            }

            if let Some((buffer, count)) = debug_lines {
                pass.set_pipeline(debug_line_pipeline);
                pass.set_bind_group(0, &gpu.lit_frame_bg, &[]);
                pass.set_vertex_buffer(0, buffer.slice(..));
                pass.draw(0..*count, 0..1);
            }
        }
        // GPU Hi-Z: build next frame's occlusion pyramid from this
        // frame's depth in the same encoder — the build's compute
        // dispatches order after the render pass on the queue (an active
        // culler already forced `depth_store_op` to Store, so the
        // attachment is intact to read).
        if self.gpu_hiz.is_some() {
            self.encode_gpu_hiz_build(encoder);
            self.gpu_hiz_frame_vp = Some(view_proj);
        }
        self.direction_buckets_skipped += skipped_buckets;
        self.region_arena_draws += arena_draws;
        self.instanced_kinetic_draws = instanced_draws;
        // One bind group per emitting region replaces one per emitting
        // section; the difference is real `set_bind_group` traffic saved.
        self.bind_group_switches_saved += arena_sections.saturating_sub(arena_regions) as u64;
        // Upload the recorded draw uniforms ahead of this frame's submit.
        let gpu = &self.composite;
        self.ctx
            .queue
            .write_buffer(&gpu.draw_ubo, 0, bytemuck::cast_slice(&gpu.draw_scratch));
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
        let caster_digest = subworld_caster_digest(frame.subworlds);
        if subworld_casters_dirty(frame.subworlds) || caster_digest != self.last_shadow_casters {
            self.shadow_cache.invalidate();
            self.subworld_gpu_epoch += 1;
        }
        self.subworld_gpu
            .sync(&self.ctx.device, &self.ctx.queue, frame.subworlds);
        self.last_shadow_casters = caster_digest;
        if let Some(rgba) = frame.ui_rgba {
            self.update_ui_framebuffer(width, height, rgba)?;
        }
        let (world_w, world_h) = self.render_scale.scaled_dimensions(width, height);
        let scaled = (world_w, world_h) != (width, height);
        if scaled {
            self.ensure_scaled_target(world_w, world_h);
        }
        let aspect = world_w as f32 / world_h as f32;
        self.prepare_shadow(camera, aspect, frame.world_time_ticks);
        let decision = self.shadow_decision(camera, frame.world_revision, frame.world_time_ticks);

        let atmo = frame.atmosphere.current();
        let view = camera.far.clamp(16.0, 2048.0);
        self.write_frame_uniforms(camera, aspect, atmo.fog_rgb, view * 0.6, view);
        self.write_env_uniforms(frame);
        // Worst case: `DIRECTION_BUCKETS` opaque buckets + one translucent
        // draw per live section + pending uploads admitted by this frame's
        // drain + scene + subworlds + batches.
        let draw_count = (self.live_sections.len() + self.pending_uploads.len())
            * (DIRECTION_BUCKETS + 1)
            // scene + translucent + one GPU-indirect arena slot
            + 3
            + self.subworld_gpu.entry_count()
            + self.subworld_gpu.batch_count()
            + usize::from(self.entity_ber.is_some())
            + self.particle_draw_slots();
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
        self.dispatch_shadow_work(&mut encoder, camera, aspect, decision);
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
                    camera_position: camera.position,
                    focal_y_px: camera_focal_y_px(camera, world_h),
                    world_time_ticks: frame.world_time_ticks,
                },
            );
            // Upscale the reduced-res world to native before the UI pass.
            self.encode_sharpen_upscale(&mut encoder, &native_view, (world_w, world_h));
        } else {
            self.encode_composed_world(
                &mut encoder,
                &ComposedWorldTarget {
                    view: &native_view,
                    size: (width, height),
                    view_proj,
                    clear_rgb,
                    chunk_fades: frame.chunk_fades,
                    camera_position: camera.position,
                    focal_y_px: camera_focal_y_px(camera, height),
                    world_time_ticks: frame.world_time_ticks,
                },
            );
        }
        // UI overlay at native resolution: tessellated GPU quads when
        // `update_ui_mesh` installed a mesh, else the legacy CPU-blit.
        // The `MEGU EDITION` title branding draws in the same pass.
        let mut drew_quad_ui = false;
        if self.ui_quad.index_count > 0 || self.title_branding_pending() {
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
            if self.ui_quad.index_count > 0 {
                self.draw_ui_quads(&mut pass);
                drew_quad_ui = true;
            }
            if self.title_branding_pending() {
                self.draw_title_branding(&mut pass);
                self.title_branding_frames += 1;
            }
        } else if let Some(ui_bind_group) = &self.ui_bind_group {
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
        if drew_quad_ui {
            self.ui_gpu_quad_frames += 1;
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

/// Generic `bytemuck::Pod` vertex upload — used by the decoded
/// `WaterVertex`/`LitTerrainVertex`/`StainedCasterVertex` streams
/// (`PackedVertex` keeps its manual u32 serialization above).
fn typed_vertex_buffer<T: bytemuck::Pod>(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    label: &str,
    verts: &[T],
) -> wgpu::Buffer {
    let buffer = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some(label),
        size: std::mem::size_of_val(verts).max(16) as u64,
        usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    if !verts.is_empty() {
        queue.write_buffer(&buffer, 0, bytemuck::cast_slice(verts));
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
