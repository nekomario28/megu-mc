// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! Real `wgpu` renderer: packed-vertex terrain pipelines, the `client-ui`
//! RGBA overlay blit, a depth-buffered camera, and a device-validated Megu
//! native WGSL shaderpack hook that never crashes — any rejection falls back
//! to the built-in pipeline with a human-readable notice.

use std::borrow::Cow;
use std::cell::Cell;
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::path::Path;
use std::time::Instant;

use bytemuck::{Pod, Zeroable};
use mc_spine_client::shaders::{
    validate_wgsl_module, WgslStage, TERRAIN_OPAQUE_WGSL, TERRAIN_TRANSLUCENT_WGSL,
};
use mc_spine_client::world::WorldState;
use mc_spine_client_mesh::atlas::{AtlasMip, PackedAtlas};
use mc_spine_client_mesh::ber_instancing::{VanillaParticleQuad, VanillaParticleWorld};
use mc_spine_client_mesh::builtin_shadow_wgsl::{
    build_shadow_uniforms_celestial, compute_sun_direction, negotiate_builtin_shadow_quality,
    pack_vfx1_lights, BuiltinShadowQuality, CascadeConfig, DirtyOnlyShadowCache, EclipseMode,
    RenderScalePreset, ShadowCacheDecision, ShadowCamera, ShadowUniforms, MEGU_SHADOW_CASTERS_WGSL,
    MEGU_SHADOW_DEPTH_WGSL, MEGU_SHADOW_SAMPLING_WGSL,
};
use mc_spine_client_mesh::ffi::PackedVertex;
use mc_spine_client_mesh::vanilla_plus_3d::ExtrudedMesh;
use mc_spine_client_mesh::vfx_engine::DynamicColoredLight;
use mc_spine_client_mesh::{
    box_in_frustum, ChunkUploadBudget, GraphCuller, HiZPyramid, SectionCoord, SectionGraph, EDGE,
};
use mc_spine_client_mesh::{
    is_megu_wgsl_pack, BackendResolution, GpuCapabilities, GpuShaderCaps, MeguWgslPack, PassKind,
    PipelineStage, RenderBackendPreference, ShaderPackSource,
};
use mc_spine_kernel::debug_chords::DebugLine;
use mc_spine_kernel::entity_interp::{evaluate_sky_and_fog, SkyFogInput, SkyFogOutput};
use winit::event_loop::OwnedDisplayHandle;

use crate::atmosphere::{AtmosphereParams, BiomeAtmosphereBlender, ChunkFadeTracker};
use crate::live_session::hw_adaptive_engine::{
    detect_host_hardware_caps, select_compute_strategy, ComputeStrategy, MemoryTransferStrategy,
};
use crate::mat4_mul;
use crate::subworld_render::{
    CompositeDrawUniforms, CompositeEnvUniforms, DebugLineVertex, SubWorldGpuCache,
    SubWorldRenderBank, TerrainVertex, VfxLightsGpu, COMPOSITE_LIT_WGSL, COMPOSITE_TERRAIN_WGSL,
    DEBUG_LINE_VERTEX_LAYOUT, DEBUG_LINE_WGSL, DRAW_UNIFORM_STRIDE, LIT_VERTEX_LAYOUT,
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

/// CPU-side record of a section mesh queued for GPU upload. The render
/// loop's [`ChunkUploadBudget`] admits a bounded number per frame; the
/// packed payload is cloned once at queue time so the `WorldState` borrow
/// ends before the deferred GPU work runs.
struct PendingSectionUpload {
    packed: Vec<PackedVertex>,
    order: Vec<u32>,
    /// Estimated GPU payload bytes charged against the budget.
    upload_bytes: usize,
    /// Estimated upload-side CPU cost in microseconds.
    cost_us: u64,
}

impl PendingSectionUpload {
    fn new(packed: &[PackedVertex], order: &[u32]) -> Self {
        // `section_geometry` emits at most one output vertex per packed
        // vertex (16 B each) and six u32 indices per quad (~6 B per packed
        // vertex); the slack covers the section uniform/bind-group work.
        let upload_bytes = packed.len() * (std::mem::size_of::<PackedVertex>() + 6) + 256;
        let cost_us = 32 + upload_bytes as u64 / 256;
        Self {
            packed: packed.to_vec(),
            order: order.to_vec(),
            upload_bytes,
            cost_us,
        }
    }

    fn matches(&self, packed: &[PackedVertex], order: &[u32]) -> bool {
        self.packed == packed && self.order == order
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

/// Six view-frustum planes in [`box_in_frustum`] form extracted from the
/// column-major `view_proj`: each entry is `[nx, ny, nz, d]` with the
/// inside half-space `dot(n, p) + d >= 0`. Covers NDC x/y in `[-1, 1]`
/// and z in `[0, 1]` — the wgpu clip convention `Camera::view_projection`
/// produces.
fn frustum_planes_from_view_proj(view_proj: &Mat4) -> [[f32; 4]; 6] {
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
fn view_proj_row_major(view_proj: &Mat4) -> [[f32; 4]; 4] {
    std::array::from_fn(|row| std::array::from_fn(|col| view_proj[col][row]))
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
    /// Optional Hi-Z occlusion pyramid an embedder installs from a previous
    /// frame's depth buffer; consulted after the graph/frustum test in the
    /// composed pass. `None` keeps the conservative graph/frustum path.
    hiz_pyramid: Option<HiZPyramid>,
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
    composite: CompositeGpu,
    debug_line_pipeline: wgpu::RenderPipeline,
    debug_lines: Option<(wgpu::Buffer, u32)>,
    entity_ber: Option<DynamicLitGpu>,
    particles: Option<DynamicLitGpu>,
    particle_alphas: Vec<f32>,
    subworld_gpu: SubWorldGpuCache,
    scaled_target: Option<ScaledTarget>,
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
    last_shadow_casters: Vec<u32>,
    /// Number of shadow depth passes completed (one per cache re-render).
    pub shadow_rerender_count: u64,
    /// Number of composed frames that reused an existing shadow texture.
    pub shadow_reuse_count: u64,
    /// Decision the [`DirtyOnlyShadowCache`] returned for the most recent
    /// composed frame (`FirstFrame` until one renders).
    pub last_shadow_decision: ShadowCacheDecision,
    /// The active shaderpack hook verdict (or `None` for built-in).
    pub shaderpack: ShaderpackAttempt,
    /// Latest notice line the HUD/diagnostics can display.
    pub notice: Option<String>,
}

pub const MAX_DEBUG_LINES: usize = 65_536;
const MAX_DYNAMIC_QUADS: usize = 16_384;

struct DynamicLitGpu {
    vertices: wgpu::Buffer,
    indices: wgpu::Buffer,
    index_count: u32,
}

impl DynamicLitGpu {
    fn upload(ctx: &GpuDevice, vertices: &[TerrainVertex], indices: &[u32]) -> Option<Self> {
        if indices.is_empty() {
            return None;
        }
        let vertices_gpu = ctx.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("dynamic-lit-vertices"),
            size: std::mem::size_of_val(vertices) as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let indices_gpu = ctx.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("dynamic-lit-indices"),
            size: std::mem::size_of_val(indices) as u64,
            usage: wgpu::BufferUsages::INDEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        ctx.queue
            .write_buffer(&vertices_gpu, 0, bytemuck::cast_slice(vertices));
        ctx.queue
            .write_buffer(&indices_gpu, 0, bytemuck::cast_slice(indices));
        Some(Self {
            vertices: vertices_gpu,
            indices: indices_gpu,
            index_count: indices.len() as u32,
        })
    }
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
    lit_frame_layout: wgpu::BindGroupLayout,
    env_layout: wgpu::BindGroupLayout,
    draw_layout: wgpu::BindGroupLayout,
    draw_bg: wgpu::BindGroup,
    draw_ubo: wgpu::Buffer,
    draw_capacity: u64,
    draw_scratch: Vec<CompositeDrawUniforms>,
    env_ubo: wgpu::Buffer,
    lights_ubo: wgpu::Buffer,
    env_bg: wgpu::BindGroup,
    atlas: AtlasGpu,
}

struct AtlasGpu {
    _texture: wgpu::Texture,
    view: wgpu::TextureView,
    sampler: wgpu::Sampler,
    rects: wgpu::Buffer,
    state_sprites: wgpu::Buffer,
    sprite_indices: HashMap<String, u32>,
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
struct ShadowGpu {
    config: CascadeConfig,
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
    lit_depth: wgpu::RenderPipeline,
    instanced_depth: wgpu::RenderPipeline,
    terrain_color: wgpu::RenderPipeline,
    lit_color: wgpu::RenderPipeline,
    lit_alpha_color: wgpu::RenderPipeline,
    instanced_color: wgpu::RenderPipeline,
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

/// Snapshot caster identities, transforms and visibility across composed
/// frames. Geometry and instance uploads also invalidate via their dirty flags
/// before GPU synchronization clears those flags.
fn subworld_caster_signature(bank: &SubWorldRenderBank) -> Vec<u32> {
    let mut signature = Vec::new();
    for entry in bank.entries() {
        signature.extend([entry.id as u32, (entry.id >> 32) as u32]);
        signature.extend(entry.transform.position.map(f32::to_bits));
        signature.extend(entry.transform.quaternion.map(f32::to_bits));
        signature.push(entry.transform.scale.to_bits());
        signature.push(entry.alpha.to_bits());
        signature.extend([entry.vertices.len() as u32, entry.indices.len() as u32]);
    }
    for batch in bank.batches() {
        signature.push(batch.mesh_kind);
        signature.extend(batch.transform.position.map(f32::to_bits));
        signature.extend(batch.transform.quaternion.map(f32::to_bits));
        signature.push(batch.transform.scale.to_bits());
        signature.extend([
            batch.template_vertices.len() as u32,
            batch.template_indices.len() as u32,
            batch.instances.len() as u32,
        ]);
    }
    signature
}

fn subworld_casters_dirty(bank: &SubWorldRenderBank) -> bool {
    bank.entries()
        .any(|entry| entry.dirty && entry.alpha >= 0.999)
        || bank
            .batches()
            .any(|batch| batch.geometry_dirty || batch.instances_dirty)
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
        let shadow_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
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
            layout: &shadow_layout,
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
        let depth_pipeline = |label, entry, buffers: &[Option<wgpu::VertexBufferLayout<'_>>]| {
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some(label),
                layout: Some(&caster_pipeline_layout),
                vertex: wgpu::VertexState {
                    module: &caster_module,
                    entry_point: Some(entry),
                    compilation_options: Default::default(),
                    buffers,
                },
                primitive: wgpu::PrimitiveState {
                    cull_mode: None,
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
            "vs_terrain",
            &[Some(TERRAIN_VERTEX_LAYOUT)],
        );
        let lit_depth = depth_pipeline("shadow-lit", "vs_lit", &[Some(LIT_VERTEX_LAYOUT)]);
        let instanced_depth = depth_pipeline(
            "shadow-instanced",
            "vs_inst",
            &[Some(LIT_VERTEX_LAYOUT), Some(PART_INSTANCE_LAYOUT)],
        );

        let terrain_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("shadowed-composite-terrain"),
            bind_group_layouts: &[
                Some(frame_layout),
                Some(&composite.draw_layout),
                Some(&composite.env_layout),
                Some(&shadow_layout),
            ],
            immediate_size: 0,
        });
        let lit_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("shadowed-composite-lit"),
            bind_group_layouts: &[
                Some(&composite.lit_frame_layout),
                Some(&composite.draw_layout),
                Some(&composite.env_layout),
                Some(&shadow_layout),
            ],
            immediate_size: 0,
        });
        let terrain_module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("shadowed-terrain"),
            source: wgpu::ShaderSource::Wgsl(Cow::Owned(composed_shadow_wgsl(
                COMPOSITE_TERRAIN_WGSL,
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
        let color_pipeline = |label, layout, module, vs, fs, buffers, blend, write| {
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
                    cull_mode: None,
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
        );
        let lit_color = color_pipeline(
            "shadowed-lit-color",
            &lit_layout,
            &lit_module,
            "lit_vs",
            "lit_fs",
            &[Some(LIT_VERTEX_LAYOUT)],
            None,
            true,
        );
        let lit_alpha_color = color_pipeline(
            "shadowed-lit-alpha",
            &lit_layout,
            &lit_module,
            "lit_vs",
            "lit_fs",
            &[Some(LIT_VERTEX_LAYOUT)],
            Some(ALPHA_BLEND),
            false,
        );
        let instanced_color = color_pipeline(
            "shadowed-instanced-color",
            &lit_layout,
            &lit_module,
            "inst_vs",
            "lit_fs",
            &[Some(LIT_VERTEX_LAYOUT), Some(PART_INSTANCE_LAYOUT)],
            None,
            true,
        );
        Self {
            config,
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
            lit_depth,
            instanced_depth,
            terrain_color,
            lit_color,
            lit_alpha_color,
            instanced_color,
        }
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
    }
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
    /// Camera position — seeds the [`GraphCuller`] search's camera section.
    camera_position: [f32; 3],
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

    /// Sorted-by-name sprite index for lit geometry's `atlas_index`.
    pub fn atlas_sprite_index(&self, name: &str) -> Option<u32> {
        self.composite.atlas.sprite_indices.get(name).copied()
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
        let mut sprite_indices = HashMap::new();
        for name in atlas.placements.keys() {
            let tile = atlas
                .tile_rect(name)
                .ok_or_else(|| format!("missing tile: {name}"))?;
            sprite_indices.insert(name.clone(), rects.len() as u32);
            rects.push([tile.u0, tile.v0, tile.u_scale, tile.v_scale]);
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
        };
        let bind_group = gpu_atlas.bind_group(device, &self.composite);
        self.composite.atlas = gpu_atlas;
        self.composite.env_bg = bind_group;
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
            states[id as usize] = *sprite_indices
                .get(sprite)
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
        // `compute_strategy` without a window-loop call site.
        let (upload_budget, mesh_worker_threads) =
            strategy_upload_defaults(&select_compute_strategy(&detect_host_hardware_caps()));
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
            window_camera: Cell::new(None),
            scene: None,
            live_sections: HashMap::new(),
            upload_budget,
            pending_uploads: BTreeMap::new(),
            section_graphs: HashMap::new(),
            hiz_pyramid: None,
            upload_frame_ms: VecDeque::new(),
            last_render_instant: None,
            mesh_worker_threads,
            compute_strategy_apply_count: 0,
            admitted_uploads: 0,
            deferred_uploads: 0,
            last_frame_culled_sections: 0,
            culled_sections: 0,
            composite,
            debug_line_pipeline,
            debug_lines: None,
            entity_ber: None,
            particles: None,
            particle_alphas: Vec::new(),
            subworld_gpu: SubWorldGpuCache::new(),
            scaled_target: None,
            shadow_quality: BuiltinShadowQuality::Off,
            render_scale: RenderScalePreset::default(),
            shadow_cache: DirtyOnlyShadowCache::new(),
            shadow: None,
            shadow_active: false,
            last_shadow_camera: None,
            last_shadow_casters: Vec::new(),
            shadow_rerender_count: 0,
            shadow_reuse_count: 0,
            last_shadow_decision: ShadowCacheDecision::Rerender(
                mc_spine_client_mesh::builtin_shadow_wgsl::ShadowDirtyReason::FirstFrame,
            ),
            shaderpack: ShaderpackAttempt::Fallback {
                notice: "built-in terrain pipeline".to_string(),
            },
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
        self.entity_ber = DynamicLitGpu::upload(&self.ctx, &vertices, &indices);
    }

    /// Upload precomputed camera-facing particle quads with individual alpha.
    pub fn set_particle_billboards(&mut self, quads: &[VanillaParticleQuad]) {
        let mut vertices = Vec::new();
        let mut indices = Vec::new();
        self.particle_alphas.clear();
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
        self.particles = DynamicLitGpu::upload(&self.ctx, &vertices, &indices);
    }

    /// Convert the Vanilla particle simulation to camera-facing quads.
    pub fn set_particle_world(
        &mut self,
        world: &VanillaParticleWorld,
        camera_right: [f32; 3],
        camera_up: [f32; 3],
    ) {
        self.set_particle_billboards(&world.billboard_quads(camera_right, camera_up));
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
                wgpu::BindGroupLayoutEntry {
                    binding: 4,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Storage { read_only: true },
                        has_dynamic_offset: false,
                        min_binding_size: wgpu::BufferSize::new(16),
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 5,
                    visibility: wgpu::ShaderStages::FRAGMENT,
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
            size: 16,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        ctx.queue.write_buffer(
            &atlas_rects,
            0,
            bytemuck::cast_slice(&[[0.0f32, 0.0, 1.0, 1.0]]),
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
            terrain_alpha_pipeline,
            lit_pipeline,
            lit_alpha_pipeline,
            instanced_pipeline,
            lit_frame_bg,
            lit_frame_layout,
            env_layout,
            draw_layout,
            draw_bg,
            draw_ubo,
            draw_capacity,
            draw_scratch: Vec::with_capacity(64),
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
        let previous_count = self.live_sections.len();
        self.live_sections
            .retain(|coord, _| world.packed_mesh(*coord).is_some());
        self.pending_uploads
            .retain(|coord, _| world.packed_mesh(*coord).is_some());
        let removed = self.live_sections.len() != previous_count;
        self.section_graphs.clear();
        self.section_graphs.extend(world.section_graphs());
        for (coord, packed) in world.packed_meshes() {
            let order = world.translucent_order(coord).unwrap_or(&[]);
            let up_to_date = match self.pending_uploads.get(&coord) {
                Some(pending) => pending.matches(packed, order),
                None => self
                    .live_sections
                    .get(&coord)
                    .is_some_and(|existing| existing.packed == packed && existing.order == order),
            };
            if up_to_date {
                continue;
            }
            self.pending_uploads
                .insert(coord, PendingSectionUpload::new(packed, order));
            scheduled += 1;
        }
        if removed {
            self.shadow_cache.invalidate();
        }
        scheduled
    }

    /// Build the GPU buffers, section uniform and bind group for one
    /// section and make it resident, replacing any previous upload for
    /// `coord`.
    fn install_live_section(
        &mut self,
        coord: SectionCoord,
        packed: &[PackedVertex],
        order: &[u32],
    ) {
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
    }

    /// Per-rendered-frame upload boundary: measure the frame interval for
    /// the budget's adaptive controller, open a fresh
    /// [`ChunkUploadBudget`] frame, adapt the per-frame cap to the windowed
    /// P95, then drain queued section uploads through it. Runs once per
    /// composed or plain world pass.
    fn begin_render_upload_frame(&mut self) {
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
        self.upload_budget.adapt(self.upload_frame_p95_us());
        self.drain_pending_uploads();
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

    /// Admit queued section uploads through the open [`ChunkUploadBudget`]
    /// frame and install the ones that fit; the rest stay queued for later
    /// frames. Returns the number of sections installed this frame.
    fn drain_pending_uploads(&mut self) -> usize {
        if self.pending_uploads.is_empty() {
            return 0;
        }
        let mut admitted = 0;
        let coords: Vec<SectionCoord> = self.pending_uploads.keys().copied().collect();
        for coord in coords {
            let Some(pending) = self.pending_uploads.get(&coord) else {
                continue;
            };
            let (bytes, cost_us) = (pending.upload_bytes, pending.cost_us);
            if !self.upload_budget.try_schedule(bytes, cost_us) {
                self.deferred_uploads += 1;
                continue;
            }
            let pending = self
                .pending_uploads
                .remove(&coord)
                .expect("pending checked");
            self.admitted_uploads += 1;
            self.install_live_section(coord, &pending.packed, &pending.order);
            admitted += 1;
        }
        if admitted > 0 {
            self.shadow_cache.invalidate();
        }
        admitted
    }

    /// Sections the composed pass will skip this frame: the
    /// [`GraphCuller`]'s frustum+occlusion result for graph-covered coords,
    /// the plain six-plane frustum test for coords the world never
    /// published a visibility graph for, and — when a Hi-Z pyramid is
    /// installed — the conservative depth test on top of both. Only
    /// sections with drawable indices are considered.
    fn cull_live_sections(
        &self,
        view_proj: &Mat4,
        camera_position: [f32; 3],
    ) -> HashSet<SectionCoord> {
        let mut culled = HashSet::new();
        if self.live_sections.is_empty() {
            return culled;
        }
        let planes = frustum_planes_from_view_proj(view_proj);
        // One graph-culler BFS per frame over the mirrored visibility set.
        // Coords absent from the snapshot (mesh-only worlds) fall back to
        // the frustum test rather than being treated as unreachable.
        let graph_visible = (!self.section_graphs.is_empty()).then(|| {
            let camera_section = SectionCoord(
                (camera_position[0] / EDGE as f32).floor() as i32,
                (camera_position[1] / EDGE as f32).floor() as i32,
                (camera_position[2] / EDGE as f32).floor() as i32,
            );
            GraphCuller::new(camera_section, &planes)
                .search(self.section_graphs.iter().map(|(&c, &g)| (c, g)))
                .visible
                .into_iter()
                .collect::<HashSet<_>>()
        });
        let hiz_rows = self
            .hiz_pyramid
            .as_ref()
            .map(|_| view_proj_row_major(view_proj));
        for (coord, section) in self.live_sections.iter() {
            if section.terrain.opaque_index_count == 0
                && section.terrain.translucent_index_count == 0
            {
                continue;
            }
            let min = [
                coord.0 as f32 * EDGE as f32,
                coord.1 as f32 * EDGE as f32,
                coord.2 as f32 * EDGE as f32,
            ];
            let visible = if self.section_graphs.contains_key(coord) {
                graph_visible
                    .as_ref()
                    .is_some_and(|set| set.contains(coord))
            } else {
                box_in_frustum(min, &planes)
            };
            if !visible {
                culled.insert(*coord);
                continue;
            }
            if let (Some(hiz), Some(rows)) = (&self.hiz_pyramid, &hiz_rows) {
                let max = [
                    min[0] + EDGE as f32,
                    min[1] + EDGE as f32,
                    min[2] + EDGE as f32,
                ];
                if hiz.cull_world_aabb(rows, min, max) {
                    culled.insert(*coord);
                }
            }
        }
        culled
    }

    /// The active per-frame chunk-upload budget.
    pub fn upload_budget(&self) -> &ChunkUploadBudget {
        &self.upload_budget
    }

    /// Replace the upload budget outright (tests/embedders tuning caps).
    pub fn set_upload_budget(&mut self, budget: ChunkUploadBudget) {
        self.upload_budget = budget;
    }

    /// Section uploads currently queued awaiting budget admission.
    pub fn pending_upload_count(&self) -> usize {
        self.pending_uploads.len()
    }

    /// Install or clear the Hi-Z occlusion pyramid consulted by the
    /// composed world pass (`None` keeps the conservative graph/frustum
    /// path).
    pub fn set_hiz_pyramid(&mut self, pyramid: Option<HiZPyramid>) {
        self.hiz_pyramid = pyramid;
    }

    /// The installed Hi-Z pyramid, if any.
    pub fn hiz_pyramid(&self) -> Option<&HiZPyramid> {
        self.hiz_pyramid.as_ref()
    }

    /// Push an adaptive [`ComputeStrategy`] into the renderer: the memory
    /// topology selects the chunk-upload budget preset and mesh-worker
    /// concurrency hint, while the cache/SIMD tiers pick conservative
    /// initial render-scale and shadow-negotiation requests (device caps
    /// still negotiate down inside [`Self::set_shadow_quality`]). Returns
    /// the negotiated shadow quality.
    pub fn apply_compute_strategy(&mut self, strategy: &ComputeStrategy) -> BuiltinShadowQuality {
        let (budget, workers) = strategy_upload_defaults(strategy);
        self.upload_budget = budget;
        self.mesh_worker_threads = workers;
        self.set_render_scale(match strategy.memory {
            MemoryTransferStrategy::UnifiedZeroCopy => RenderScalePreset::Balanced75,
            MemoryTransferStrategy::DiscreteStagedArena => RenderScalePreset::Native100,
        });
        let negotiated = self.set_shadow_quality(match strategy.memory {
            MemoryTransferStrategy::UnifiedZeroCopy => BuiltinShadowQuality::Low,
            MemoryTransferStrategy::DiscreteStagedArena => BuiltinShadowQuality::Natural,
        });
        self.compute_strategy_apply_count += 1;
        negotiated
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
    /// `mc_spine_kernel::evaluate_sky_and_fog` at noon, clear weather).
    pub fn set_camera(&self, camera: &Camera, aspect: f32) -> SkyFogOutput {
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
        );
    }

    /// Render the live window with the current biome, depth tint and chunk
    /// fade progress. Requires a camera set by `set_camera` for world draws.
    pub fn render_to_view_with_env(
        &mut self,
        view: &wgpu::TextureView,
        width: u32,
        height: u32,
        sky: &SkyFogOutput,
        atmosphere: &BiomeAtmosphereBlender,
        chunk_fades: &ChunkFadeTracker,
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

    /// Window camera currently uses the same fixed noon time as `set_camera`.
    /// The composed-frame API also accepts live world time and VFX lights.
    fn encode_window_composed_world(
        &mut self,
        encoder: &mut wgpu::CommandEncoder,
        view: &wgpu::TextureView,
        size: (u32, u32),
        sky: &SkyFogOutput,
        atmosphere: &BiomeAtmosphereBlender,
        chunk_fades: &ChunkFadeTracker,
    ) {
        let (width, height) = size;
        let camera = self.window_camera.get().expect("checked by caller");
        let aspect = width.max(1) as f32 / height.max(1) as f32;
        const NOON: u64 = 6000;
        self.prepare_shadow(&camera, aspect, NOON);
        let decision = self.shadow_decision(&camera, 0, NOON);
        if self.shadow_active {
            if matches!(decision, ShadowCacheDecision::Rerender(_)) {
                self.encode_shadow_map(encoder, &camera, aspect, NOON);
            } else {
                self.shadow_reuse_count += 1;
            }
        }

        let atmo = atmosphere.current();
        let sun = compute_sun_direction(NOON);
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
        self.ctx
            .queue
            .write_buffer(&self.composite.env_ubo, 0, bytemuck::bytes_of(&env));
        let lights = VfxLightsGpu::from(&pack_vfx1_lights(&[], self.shadow_quality));
        self.ctx
            .queue
            .write_buffer(&self.composite.lights_ubo, 0, bytemuck::bytes_of(&lights));
        self.write_frame_uniforms(&camera, aspect, atmo.fog_rgb, sky.fog_start, sky.fog_end);
        // Pending uploads may all be admitted by this frame's drain inside
        // `encode_composed_world`, so reserve draw slots for them too.
        let draws = (self.live_sections.len() + self.pending_uploads.len()) * 2
            + 2
            + self.subworld_gpu.entry_count()
            + self.subworld_gpu.batch_count()
            + usize::from(self.entity_ber.is_some())
            + self.particle_alphas.len();
        self.ensure_draw_capacity(draws.max(8));
        self.encode_composed_world(
            encoder,
            &ComposedWorldTarget {
                view,
                size: (width, height),
                view_proj: camera.view_projection(aspect),
                clear_rgb: [
                    atmo.sky_horizon_rgb[0].max(atmo.fog_rgb[0] * 0.8),
                    atmo.sky_horizon_rgb[1].max(atmo.fog_rgb[1] * 0.8),
                    atmo.sky_horizon_rgb[2].max(atmo.fog_rgb[2] * 0.8),
                ],
                chunk_fades,
                camera_position: camera.position,
            },
        );
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
            self.shadow_active = false;
            self.last_shadow_camera = None;
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
        let enabled = self.shadow_quality.config().cascade_count > 0
            && sun.iter().all(|v| v.is_finite())
            && sun[1] > 0.08
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
            self.shadow_cache.invalidate();
        }
        let signature = [
            camera.position[0],
            camera.position[1],
            camera.position[2],
            camera.yaw_deg,
            camera.pitch_deg,
            camera.fov_deg,
            camera.near,
            camera.far,
            aspect,
        ];
        if let Some(last) = self.last_shadow_camera {
            // The light projection depends on camera orientation, clip planes,
            // aspect, and position. Keep stable small translations cached.
            if signature
                .iter()
                .zip(last)
                .enumerate()
                .any(|(i, (a, b))| (a - b).abs() > if i < 3 { 0.5 } else { 0.05 })
            {
                self.shadow_cache.invalidate();
            }
        }
        self.shadow_active = true;
    }

    fn encode_shadow_map(
        &mut self,
        encoder: &mut wgpu::CommandEncoder,
        camera: &Camera,
        aspect: f32,
        world_time_ticks: u64,
    ) {
        let shadow = self.shadow.as_mut().expect("shadow resources prepared");
        let forward = camera.forward_vector();
        let uniform = build_shadow_uniforms_celestial(
            ShadowCamera {
                position: camera.position,
                forward,
                up: [0.0, 1.0, 0.0],
                fov_y_radians: camera.fov_deg.to_radians(),
                aspect_ratio: aspect,
                near: camera.near,
                far: camera.far,
            },
            shadow.config,
            world_time_ticks,
            ((world_time_ticks / 24_000) % 8) as u8,
            EclipseMode::None,
        );
        self.ctx
            .queue
            .write_buffer(&shadow.receiver_uniform, 0, &shadow_uniform_bytes(&uniform));
        for (layer, buffer) in shadow.caster_uniforms.iter().enumerate() {
            self.ctx.queue.write_buffer(
                buffer,
                0,
                &shadow_uniform_bytes(&uniform.with_active_cascade(layer as u32)),
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
            .filter(|(_, section)| section.terrain.opaque_index_count > 0)
            .map(|(coord, _)| (*coord, push_model(identity)))
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
        for (layer, view) in shadow.layer_views.iter().enumerate() {
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
            pass.set_bind_group(0, &shadow.caster_bgs[layer], &[]);
            pass.set_pipeline(&shadow.terrain_depth);
            if let (Some(scene), Some(offset)) = (&self.scene, scene_offset) {
                pass.set_bind_group(1, &self.frame_bind_group, &[]);
                pass.set_bind_group(2, &shadow.draw_bg, &[offset]);
                pass.set_vertex_buffer(0, scene.opaque_vertices.slice(..));
                pass.set_index_buffer(scene.opaque_indices.slice(..), wgpu::IndexFormat::Uint32);
                pass.draw_indexed(0..scene.opaque_index_count, 0, 0..1);
            }
            for (coord, offset) in &section_offsets {
                let section = &self.live_sections[coord];
                pass.set_bind_group(1, &section.bind_group, &[]);
                pass.set_bind_group(2, &shadow.draw_bg, &[*offset]);
                pass.set_vertex_buffer(0, section.terrain.opaque_vertices.slice(..));
                pass.set_index_buffer(
                    section.terrain.opaque_indices.slice(..),
                    wgpu::IndexFormat::Uint32,
                );
                pass.draw_indexed(0..section.terrain.opaque_index_count, 0, 0..1);
            }
            pass.set_pipeline(&shadow.lit_depth);
            pass.set_bind_group(1, &self.frame_bind_group, &[]);
            for (entry, offset) in self
                .subworld_gpu
                .entries()
                .filter(|entry| entry.index_count > 0 && entry.alpha >= 0.999)
                .zip(&entry_offsets)
            {
                pass.set_bind_group(2, &shadow.draw_bg, &[*offset]);
                pass.set_vertex_buffer(0, entry.vertex_buffer().slice(..));
                pass.set_index_buffer(entry.index_buffer().slice(..), wgpu::IndexFormat::Uint32);
                pass.draw_indexed(0..entry.index_count, 0, 0..1);
            }
            pass.set_pipeline(&shadow.instanced_depth);
            for (batch, offset) in self
                .subworld_gpu
                .batches()
                .filter(|batch| batch.index_count > 0 && batch.instance_count > 0)
                .zip(&batch_offsets)
            {
                pass.set_bind_group(2, &shadow.draw_bg, &[*offset]);
                pass.set_vertex_buffer(0, batch.template_vertex_buffer().slice(..));
                pass.set_vertex_buffer(1, batch.instance_buffer().slice(..));
                pass.set_index_buffer(
                    batch.template_index_buffer().slice(..),
                    wgpu::IndexFormat::Uint32,
                );
                pass.draw_indexed(0..batch.index_count, 0, 0..batch.instance_count);
            }
        }
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
        self.shadow_rerender_count += 1;
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
        if subworld_casters_dirty(bank)
            || subworld_caster_signature(bank) != self.last_shadow_casters
        {
            self.shadow_cache.invalidate();
        }
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
        self.ctx
            .queue
            .write_buffer(&self.composite.lights_ubo, 0, bytemuck::bytes_of(&packed));
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
        } = *target;
        self.ensure_depth(width, height);
        self.begin_render_upload_frame();
        let culled_sections = self.cull_live_sections(&view_proj, camera_position);
        self.culled_sections += culled_sections.len() as u64;
        self.last_frame_culled_sections = culled_sections.len();
        // Split disjoint field borrows so the scratch writer and the GPU
        // handles can coexist inside the render pass.
        let gpu = &mut self.composite;
        let shadow = self.shadow.as_ref().filter(|_| self.shadow_active);
        let scene = &self.scene;
        let live_sections = &self.live_sections;
        let subworld_gpu = &self.subworld_gpu;
        let entity_ber = &self.entity_ber;
        let particles = &self.particles;
        let particle_alphas = &self.particle_alphas;
        let debug_lines = &self.debug_lines;
        let debug_line_pipeline = &self.debug_line_pipeline;
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
            if let Some(shadow) = shadow {
                pass.set_bind_group(3, &shadow.receiver_bg, &[]);
            }

            // Opaque terrain: the demo scene plus every live section, each
            // with its chunk-column fade.
            pass.set_pipeline(shadow.map_or(&gpu.terrain_opaque_pipeline, |s| &s.terrain_color));
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
                if culled_sections.contains(coord) || section.terrain.opaque_index_count == 0 {
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
                if culled_sections.contains(coord) || section.terrain.translucent_index_count == 0 {
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
                    pass.set_pipeline(shadow.map_or(&gpu.lit_pipeline, |s| &s.lit_color));
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
                    pass.set_pipeline(
                        shadow.map_or(&gpu.instanced_pipeline, |s| &s.instanced_color),
                    );
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
                    pass.set_pipeline(
                        shadow.map_or(&gpu.lit_alpha_pipeline, |s| &s.lit_alpha_color),
                    );
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

            if let Some(mesh) = entity_ber {
                pass.set_pipeline(shadow.map_or(&gpu.lit_pipeline, |s| &s.lit_color));
                pass.set_bind_group(0, &gpu.lit_frame_bg, &[]);
                let off = slot(identity, 1.0, 1.0);
                pass.set_bind_group(1, &gpu.draw_bg, &[off]);
                pass.set_vertex_buffer(0, mesh.vertices.slice(..));
                pass.set_index_buffer(mesh.indices.slice(..), wgpu::IndexFormat::Uint32);
                pass.draw_indexed(0..mesh.index_count, 0, 0..1);
            }
            if let Some(mesh) = particles {
                pass.set_pipeline(shadow.map_or(&gpu.lit_alpha_pipeline, |s| &s.lit_alpha_color));
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
        if subworld_casters_dirty(frame.subworlds)
            || subworld_caster_signature(frame.subworlds) != self.last_shadow_casters
        {
            self.shadow_cache.invalidate();
        }
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
        let aspect = world_w as f32 / world_h as f32;
        self.prepare_shadow(camera, aspect, frame.world_time_ticks);
        let decision = self.shadow_decision(camera, frame.world_revision, frame.world_time_ticks);

        let atmo = frame.atmosphere.current();
        let view = camera.far.clamp(16.0, 2048.0);
        self.write_frame_uniforms(camera, aspect, atmo.fog_rgb, view * 0.6, view);
        self.write_env_uniforms(frame);
        // Worst case: one slot per live section + pending uploads admitted
        // by this frame's drain + scene + subworlds + batches.
        let draw_count = (self.live_sections.len() + self.pending_uploads.len()) * 2
            + 2
            + self.subworld_gpu.entry_count()
            + self.subworld_gpu.batch_count()
            + usize::from(self.entity_ber.is_some())
            + self.particle_alphas.len();
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
        if self.shadow_active {
            if matches!(decision, ShadowCacheDecision::Rerender(_)) {
                self.encode_shadow_map(&mut encoder, camera, aspect, frame.world_time_ticks);
                self.last_shadow_casters = subworld_caster_signature(frame.subworlds);
            } else {
                self.shadow_reuse_count += 1;
            }
        }
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
                pass.set_bind_group(
                    0,
                    &self.scaled_target.as_ref().expect("ensured").bind_group,
                    &[],
                );
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
                    camera_position: camera.position,
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
