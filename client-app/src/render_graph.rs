// SPDX-License-Identifier: LGPL-3.0-or-later
//! Deterministic frame render-graph planner for the Vulkan paths.
//!
//! [`plan_frame`] turns a [`GpuPipelineKind`] plus a [`FramePlanInput`] into
//! an ordered [`RenderGraphPlan`]: which passes run, which embedded WGSL
//! module each binds, how geometry is submitted, and a deterministic
//! draw-call budget + VRAM estimate. Given identical inputs the plan is
//! identical — no clocks, no hash iteration, no device probes.
//!
//! Canonical pass order (mirrors `render_frame`'s software pipeline):
//! `DepthPrepass` → `HiZBuild` → `MeshletCull` → `OpaqueTerrain` →
//! `InstancedBlockEntities` → `SkyAndClouds` → `TranslucentTerrain` →
//! `HudOverlay`. `ClassicLowSpec` drops `HiZBuild` and `MeshletCull`
//! (no compute shaders, no SSBO) and batches block entities through 16 KiB
//! UBOs; `Classic` keeps the Hi-Z chain for CPU-side occlusion queries but
//! submits plain per-section draws.

use megu_client_mesh::vfx_engine::{VfxBudget, VfxFrameOutput, VfxHardwareTier};
use megu_client_mesh::{
    ActiveRenderBackend, PassKind, PipelineStage, BLOCK_ENTITY_INSTANCE_GPU_BYTES,
    KINETIC_INSTANCE_GPU_BYTES, MAX_INSTANCES_PER_UBO_16KB,
};

use crate::shaderpack_runtime::{ActiveShaderSession, LoadedPackKind};
use crate::shaders::{
    validate_wgsl_module, WgslBindingKind, WgslStage, BLOCK_ENTITY_INSTANCED_UBO_WGSL,
    BLOCK_ENTITY_INSTANCED_WGSL, HIZ_DOWNSAMPLE_COMPUTE_WGSL, MESHLET_CULL_COMPUTE_WGSL,
    TERRAIN_OPAQUE_WGSL, TERRAIN_TRANSLUCENT_WGSL,
};

// ---------------------------------------------------------------------------
// Pipeline + pass taxonomy
// ---------------------------------------------------------------------------

/// The GPU pipeline family a frame plan is tailored for. Mirrors
/// `megu_client_mesh::ActiveRenderBackend` one-to-one; kept as a
/// distinct type so the graph names stay stable if backend negotiation grows
/// more variants.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum GpuPipelineKind {
    /// `VK_EXT_mesh_shader` task+mesh pipeline over 64-quad meshlets.
    #[default]
    MeshShader,
    /// Compute cull/compact + `vkCmdDrawIndexedIndirectCount`.
    ComputeIndirect,
    /// Plain vertex pipeline, per-section multidraw on the CPU side.
    Classic,
    /// Minimal-hardware pipeline for legacy Intel iGPUs: no compute, no
    /// SSBO, 16 KiB-safe UBO instance batches.
    ClassicLowSpec,
}

impl GpuPipelineKind {
    /// All pipeline kinds, strongest first.
    pub const ALL: [Self; 4] = [
        Self::MeshShader,
        Self::ComputeIndirect,
        Self::Classic,
        Self::ClassicLowSpec,
    ];

    /// Lowercase diagnostic token.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::MeshShader => "mesh-shader",
            Self::ComputeIndirect => "compute-indirect",
            Self::Classic => "classic",
            Self::ClassicLowSpec => "classic-low-spec",
        }
    }

    /// Whether the pipeline runs a GPU meshlet-cull pass
    /// (task-amplification on `MeshShader`, compute dispatch on
    /// `ComputeIndirect`).
    pub const fn runs_meshlet_cull(self) -> bool {
        matches!(self, Self::MeshShader | Self::ComputeIndirect)
    }

    /// Whether the pipeline builds the Hi-Z mip chain this frame. Only
    /// `ClassicLowSpec` skips it — the minimal-hardware profile has no
    /// compute pass and cannot spare the bandwidth.
    pub const fn builds_hiz(self) -> bool {
        !matches!(self, Self::ClassicLowSpec)
    }

    /// How `InstancedBlockEntities` submits on this pipeline.
    pub const fn instanced_submission(self) -> InstanceSubmission {
        if matches!(self, Self::ClassicLowSpec) {
            InstanceSubmission::UboBatches16k
        } else {
            InstanceSubmission::IndirectSsbo
        }
    }
}

impl From<ActiveRenderBackend> for GpuPipelineKind {
    fn from(backend: ActiveRenderBackend) -> Self {
        match backend {
            ActiveRenderBackend::MeshShader => Self::MeshShader,
            ActiveRenderBackend::ComputeIndirect => Self::ComputeIndirect,
            ActiveRenderBackend::Classic => Self::Classic,
            ActiveRenderBackend::ClassicLowSpec => Self::ClassicLowSpec,
        }
    }
}

/// Ordered pass identifiers in the canonical frame sequence.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum RenderPassId {
    /// Depth-only rasterization feeding the Hi-Z chain and early-z.
    DepthPrepass = 0,
    /// Hi-Z mip-chain build (2x2 max downsample per level).
    HiZBuild,
    /// GPU meshlet cull: frustum + facing mask + Hi-Z occlusion.
    MeshletCull,
    /// Opaque chunk geometry.
    OpaqueTerrain,
    /// Instanced block entities + kinetic contraptions.
    InstancedBlockEntities,
    /// Sky gradient, sun/moon, clouds.
    SkyAndClouds,
    /// Water/stained-glass translucency, back-to-front.
    TranslucentTerrain,
    /// HUD, text, crosshair, debug overlays.
    HudOverlay,
}

impl RenderPassId {
    /// The canonical frame order, first to last.
    pub const ORDER: [Self; 8] = [
        Self::DepthPrepass,
        Self::HiZBuild,
        Self::MeshletCull,
        Self::OpaqueTerrain,
        Self::InstancedBlockEntities,
        Self::SkyAndClouds,
        Self::TranslucentTerrain,
        Self::HudOverlay,
    ];
}

/// How `InstancedBlockEntities` uploads instance records.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum InstanceSubmission {
    /// One SSBO instance stream + one indirect command per mesh kind.
    IndirectSsbo,
    /// [`MAX_INSTANCES_PER_UBO_16KB`]-record 16 KiB-safe UBO draw batches.
    UboBatches16k,
}

/// How a pass submits its work to the GPU.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PassSubmission {
    /// Ordinary `vkCmdDraw*` calls recorded on the CPU.
    DirectDraws,
    /// One or more `vkCmdDispatch*` compute dispatches.
    ComputeDispatch,
    /// Mesh-shader task amplification (`vkCmdDrawMeshTasks*`).
    TaskAmplification,
    /// `vkCmdDrawIndexedIndirectCount` over a GPU-built command buffer.
    IndirectDrawCount,
    /// `vkCmdDrawMeshTasksIndirectCountEXT` over GPU-culled meshlets.
    MeshletIndirectDraw,
    /// One `vkCmdDraw` per 16 KiB-safe uniform-buffer instance batch.
    UboBatches16k,
}

/// GPU resource class a pass binds, paired 1:1 with
/// [`WgslBindingKind`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ResourceKind {
    /// Uniform/constant buffer (`var<uniform>`).
    UniformBuffer,
    /// Read-only storage buffer (`var<storage, read>`).
    StorageRead,
    /// Mutable storage buffer (`var<storage, read_write>`).
    StorageReadWrite,
    /// Sampled texture (`texture_*`).
    SampledTexture,
    /// Storage texture (`texture_storage_*`).
    StorageTexture,
    /// Texture sampler.
    Sampler,
}

impl ResourceKind {
    /// The [`WgslBindingKind`] a WGSL declaration must report for a `var`
    /// bound to a resource of this kind.
    pub const fn wgsl_kind(self) -> WgslBindingKind {
        match self {
            Self::UniformBuffer => WgslBindingKind::Uniform,
            Self::StorageRead => WgslBindingKind::StorageRead,
            Self::StorageReadWrite => WgslBindingKind::StorageReadWrite,
            Self::SampledTexture => WgslBindingKind::Texture,
            Self::StorageTexture => WgslBindingKind::StorageTexture,
            Self::Sampler => WgslBindingKind::Sampler,
        }
    }
}

/// One GPU resource a pass binds, mirrored with the WGSL
/// `@group(g) @binding(b)` declaration of the same index.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PassBinding {
    /// `@group` index.
    pub group: u32,
    /// `@binding` index inside the group.
    pub binding: u32,
    /// Stable resource name for diagnostics (`"frame_uniforms"`, ...).
    pub resource: &'static str,
    /// Resource class.
    pub kind: ResourceKind,
}

const fn bind(
    group: u32,
    binding: u32,
    resource: &'static str,
    kind: ResourceKind,
) -> PassBinding {
    PassBinding {
        group,
        binding,
        resource,
        kind,
    }
}

/// Which embedded WGSL module a pass binds (`None` = fixed-function or a
/// module outside this suite, e.g. the HUD blit shader).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ShaderModule {
    /// `TERRAIN_OPAQUE_WGSL`.
    TerrainOpaque,
    /// `TERRAIN_TRANSLUCENT_WGSL`.
    TerrainTranslucent,
    /// `BLOCK_ENTITY_INSTANCED_WGSL` (SSBO/indirect variant).
    BlockEntityInstancedSsbo,
    /// `BLOCK_ENTITY_INSTANCED_UBO_WGSL` (16 KiB UBO variant).
    BlockEntityInstancedUbo,
    /// `HIZ_DOWNSAMPLE_COMPUTE_WGSL`.
    HiZDownsample,
    /// `MESHLET_CULL_COMPUTE_WGSL`.
    MeshletCull,
}

impl ShaderModule {
    /// The embedded WGSL source for this module.
    pub const fn wgsl(self) -> &'static str {
        match self {
            Self::TerrainOpaque => TERRAIN_OPAQUE_WGSL,
            Self::TerrainTranslucent => TERRAIN_TRANSLUCENT_WGSL,
            Self::BlockEntityInstancedSsbo => BLOCK_ENTITY_INSTANCED_WGSL,
            Self::BlockEntityInstancedUbo => BLOCK_ENTITY_INSTANCED_UBO_WGSL,
            Self::HiZDownsample => HIZ_DOWNSAMPLE_COMPUTE_WGSL,
            Self::MeshletCull => MESHLET_CULL_COMPUTE_WGSL,
        }
    }

    /// Entry-point stages [`validate_wgsl_module`] must find.
    pub const fn required_stages(self) -> &'static [WgslStage] {
        match self {
            Self::HiZDownsample | Self::MeshletCull => {
                &[WgslStage::Compute]
            }
            _ => &[WgslStage::Vertex, WgslStage::Fragment],
        }
    }
}

/// One planned pass: stable identity, the WGSL module it binds, the exact
/// resource bindings it will record, and deterministic cost estimates.
#[derive(Clone, Debug)]
pub struct PlannedPass {
    /// Pass identity within [`RenderPassId::ORDER`].
    pub id: RenderPassId,
    /// Human-readable label for diagnostics/profiling.
    pub label: &'static str,
    /// WGSL module bound by the pass, if any.
    pub shader: Option<ShaderModule>,
    /// How the pass submits work.
    pub submission: PassSubmission,
    /// Exact `@group/@binding` resource set the pass records.
    pub bindings: Vec<PassBinding>,
    /// Estimated draw calls (or dispatches) this frame.
    pub estimated_draw_calls: u32,
    /// Estimated bytes of VRAM the pass's outputs occupy.
    pub estimated_vram_bytes: u64,
}

/// Everything [`plan_frame`] needs to size a frame. Counts come from the
/// CPU cull/meshing pipeline and are treated as exact inputs — the plan is
/// a pure function of them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FramePlanInput {
    /// Render target width in pixels.
    pub width: u32,
    /// Render target height in pixels.
    pub height: u32,
    /// Sections surviving graph+frustum culling this frame.
    pub visible_sections: u32,
    /// Meshlet descriptors emitted for those sections (mesh-shader path).
    pub meshlet_count: u32,
    /// Live block-entity + kinetic instances in the pools.
    pub block_entity_instances: u32,
    /// Active block-entity mesh kinds with registered templates.
    pub block_entity_mesh_kinds: u32,
    /// Sections carrying translucent quads this frame.
    pub translucent_sections: u32,
}

/// One shaderpack pass layered over the built-in frame sequence by
/// [`plan_frame_with_shaderpack`]. Unlike [`PlannedPass`] — whose identity
/// is the fixed [`RenderPassId`] set — a pack pass keeps its pack-defined
/// program name, Iris execution stage, and render-target list, so the
/// plan mirrors the pack's `shaders.properties`/`megu-shader.toml`
/// ordering 1:1.
#[derive(Clone, Debug)]
pub struct ShaderpackPlannedPass {
    /// Pack program name (`shadow`, `dh_shadow`, `gbuffers_terrain`,
    /// `dh_terrain`, `dh_water`, `deferred`, `composite`, `final`, or a
    /// `begin`/`prepare`/`shadowcomp` compute pass).
    pub name: String,
    /// Iris execution stage the pass belongs to.
    pub stage: PipelineStage,
    /// Compute dispatch or graphics draw.
    pub kind: PassKind,
    /// Color-attachment target names the pass declares
    /// (`RENDERTARGETS`/`DRAWBUFFERS` or `megu-shader.toml` `targets`).
    pub render_targets: Vec<String>,
    /// Compute workgroup counts for `PassKind::Compute` passes.
    pub work_groups: Option<[u32; 3]>,
    /// Estimated draw calls (or dispatched workgroups) this frame.
    pub estimated_draw_calls: u32,
    /// Estimated bytes of VRAM the pass's outputs occupy.
    pub estimated_vram_bytes: u64,
}

/// Bounded VFX work submitted after translucent terrain and before the HUD.
/// Kept separate from the fixed [`RenderPassId`] taxonomy so render backends
/// compiled against the existing pass IDs remain compatible.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VfxPassSummary {
    pub active_particles: usize,
    pub beam_count: usize,
    pub ribbon_quad_count: usize,
    pub aura_count: usize,
    pub dynamic_light_count: usize,
    pub budget_preset: VfxHardwareTier,
}

/// A translucent VFX pass and its per-frame draw/upload estimates. Dynamic
/// lights are forwarded in `summary` for the lighting stage to consume.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VfxPlannedPass {
    pub label: &'static str,
    pub after_pass: RenderPassId,
    pub submission: PassSubmission,
    pub estimated_draw_calls: u32,
    pub estimated_vram_bytes: u64,
    pub summary: VfxPassSummary,
}

/// A complete deterministic frame plan.
#[derive(Clone, Debug)]
pub struct RenderGraphPlan {
    /// Pipeline the plan was tailored for.
    pub pipeline: GpuPipelineKind,
    /// Passes in [`RenderPassId::ORDER`].
    pub passes: Vec<PlannedPass>,
    /// Shaderpack passes layered over the built-in sequence in pack
    /// execution order (shadow / dh_shadow / gbuffers / dh_terrain /
    /// dh_water / deferred / composite / final / compute). Empty for
    /// vanilla rendering; populated only by [`plan_frame_with_shaderpack`].
    pub shaderpack_passes: Vec<ShaderpackPlannedPass>,
    /// Display name of the active shaderpack when `shaderpack_passes` is
    /// populated; `None` for built-in rendering.
    pub active_shaderpack: Option<String>,
    /// Optional translucent VFX work between terrain transparency and HUD.
    pub vfx_pass: Option<Box<VfxPlannedPass>>,
}

impl RenderGraphPlan {
    /// Pass `id`'s plan, if the pipeline runs it.
    pub fn pass(&self, id: RenderPassId) -> Option<&PlannedPass> {
        self.passes.iter().find(|p| p.id == id)
    }

    /// Sum of every pass's estimated draw calls/dispatches, including the
    /// layered shaderpack passes when present.
    pub fn total_estimated_draw_calls(&self) -> u32 {
        self.passes
            .iter()
            .map(|p| p.estimated_draw_calls)
            .chain(self.shaderpack_passes.iter().map(|p| p.estimated_draw_calls))
            .chain(self.vfx_pass.iter().map(|p| p.estimated_draw_calls))
            .sum()
    }

    /// Sum of every pass's estimated VRAM footprint in bytes, including
    /// the layered shaderpack passes when present.
    pub fn vram_estimate_bytes(&self) -> u64 {
        self.passes
            .iter()
            .map(|p| p.estimated_vram_bytes)
            .chain(self.shaderpack_passes.iter().map(|p| p.estimated_vram_bytes))
            .chain(self.vfx_pass.iter().map(|p| p.estimated_vram_bytes))
            .sum()
    }

    /// The `(group, binding, kind)` resource set declared for `pass`, for
    /// consistency checks against its WGSL module.
    fn binding_set(pass: &PlannedPass) -> Vec<(u32, u32, ResourceKind)> {
        let mut set: Vec<_> = pass
            .bindings
            .iter()
            .map(|b| (b.group, b.binding, b.kind))
            .collect();
        set.sort_unstable();
        set
    }

    /// Cross-check every pass's declared bindings against the
    /// `@group/@binding` declarations in its WGSL module (parsed through
    /// [`validate_wgsl_module`]). A pass with no shader must declare no
    /// bindings. Returns the first mismatch found.
    pub fn validate_binding_layouts(&self) -> Result<(), String> {
        for pass in &self.passes {
            let Some(module) = pass.shader else {
                if !pass.bindings.is_empty() {
                    return Err(format!(
                        "pass {:?} declares bindings but binds no WGSL module",
                        pass.id
                    ));
                }
                continue;
            };
            let info = validate_wgsl_module(module.wgsl(), module.required_stages())
                .map_err(|e| format!("pass {:?} shader failed validation: {e}", pass.id))?;
            let mut shader_set: Vec<(u32, u32, ResourceKind)> = info
                .bindings
                .iter()
                .map(|b| {
                    let kind = match b.kind {
                        WgslBindingKind::Uniform => ResourceKind::UniformBuffer,
                        WgslBindingKind::StorageRead => ResourceKind::StorageRead,
                        WgslBindingKind::StorageReadWrite => ResourceKind::StorageReadWrite,
                        WgslBindingKind::Texture => ResourceKind::SampledTexture,
                        WgslBindingKind::StorageTexture => ResourceKind::StorageTexture,
                        WgslBindingKind::Sampler => ResourceKind::Sampler,
                    };
                    (b.group, b.binding, kind)
                })
                .collect();
            shader_set.sort_unstable();
            let declared = Self::binding_set(pass);
            if shader_set != declared {
                return Err(format!(
                    "pass {:?} bindings {declared:?} do not match WGSL {shader_set:?}",
                    pass.id
                ));
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Deterministic cost model
// ---------------------------------------------------------------------------

/// Bytes per `MeshletDescriptor`/`MeshletRecord`.
const MESHLET_RECORD_BYTES: u64 = 16;
/// Bytes per `DrawIndexedIndirectCommand`.
const INDIRECT_COMMAND_BYTES: u64 = 20;
/// Bytes per `SubWorldTransformGpu` slot.
const SUBWORLD_TRANSFORM_BYTES: u64 = 64;
/// Maximum sub-world slots carried by the 16 KiB-safe UBO table.
const UBO_SUBWORLD_SLOTS: u64 = 64;

fn bytes(w: u32, h: u32, bpp: u64) -> u64 {
    u64::from(w) * u64::from(h) * bpp
}

/// Hi-Z mip-chain size: mip0 depth plus the geometric ~1/4-series of later
/// levels (4/3 of one depth buffer, bounded below by one texel).
fn hiz_bytes(w: u32, h: u32) -> u64 {
    let mip0 = bytes(w, h, 4);
    mip0 + (mip0 / 3).max(4)
}

/// Draw calls a terrain pass submits: one indirect submission on the GPU
/// cull paths, one `vkCmdDraw` per visible section on the classic paths.
fn terrain_draw_calls(pipeline: GpuPipelineKind, visible_sections: u32) -> u32 {
    match pipeline {
        GpuPipelineKind::MeshShader | GpuPipelineKind::ComputeIndirect => {
            u32::from(visible_sections > 0)
        }
        GpuPipelineKind::Classic | GpuPipelineKind::ClassicLowSpec => visible_sections,
    }
}

/// `InstancedBlockEntities` draw calls: one indirect command per active
/// mesh kind on SSBO pipelines, `ceil(instances / 256)` 16 KiB UBO batches
/// (never fewer than the active-kind count) on `ClassicLowSpec`.
fn instanced_draw_calls(
    pipeline: GpuPipelineKind,
    instances: u32,
    mesh_kinds: u32,
) -> u32 {
    if instances == 0 || mesh_kinds == 0 {
        return 0;
    }
    match pipeline.instanced_submission() {
        InstanceSubmission::IndirectSsbo => mesh_kinds,
        InstanceSubmission::UboBatches16k => {
            let per_ubo = MAX_INSTANCES_PER_UBO_16KB as u32;
            instances.div_ceil(per_ubo).max(mesh_kinds)
        }
    }
}

/// Per-frame instance upload bytes for `InstancedBlockEntities`: the shared
/// 48-byte record stream plus the dense sub-world table on SSBO pipelines,
/// or worst-case one full UBO per batch on `ClassicLowSpec`.
fn instanced_vram_bytes(pipeline: GpuPipelineKind, instances: u32, draws: u32) -> u64 {
    let record_bytes = u64::from(BLOCK_ENTITY_INSTANCE_GPU_BYTES)
        .max(u64::from(KINETIC_INSTANCE_GPU_BYTES));
    match pipeline.instanced_submission() {
        InstanceSubmission::IndirectSsbo => {
            u64::from(instances) * record_bytes + UBO_SUBWORLD_SLOTS * SUBWORLD_TRANSFORM_BYTES
        }
        InstanceSubmission::UboBatches16k => {
            u64::from(draws) * u64::from(MAX_INSTANCES_PER_UBO_16KB as u32) * record_bytes
                + UBO_SUBWORLD_SLOTS * SUBWORLD_TRANSFORM_BYTES
        }
    }
}

// ---------------------------------------------------------------------------
// The planner
// ---------------------------------------------------------------------------

fn terrain_bindings() -> Vec<PassBinding> {
    vec![
        bind(0, 0, "frame_uniforms", ResourceKind::UniformBuffer),
        bind(0, 1, "section_uniforms", ResourceKind::UniformBuffer),
    ]
}

fn instanced_bindings(submission: InstanceSubmission) -> Vec<PassBinding> {
    let (be, kin, worlds) = match submission {
        InstanceSubmission::IndirectSsbo => (
            ResourceKind::StorageRead,
            ResourceKind::StorageRead,
            ResourceKind::StorageRead,
        ),
        InstanceSubmission::UboBatches16k => (
            ResourceKind::UniformBuffer,
            ResourceKind::UniformBuffer,
            ResourceKind::UniformBuffer,
        ),
    };
    vec![
        bind(0, 0, "instance_frame_uniforms", ResourceKind::UniformBuffer),
        bind(0, 1, "block_entity_instances", be),
        bind(0, 2, "kinetic_instances", kin),
        bind(0, 3, "subworld_transforms", worlds),
        bind(0, 4, "draw_params_uniforms", ResourceKind::UniformBuffer),
    ]
}

/// Plan one deterministic frame for `pipeline` from `input`.
pub fn plan_frame(pipeline: GpuPipelineKind, input: &FramePlanInput) -> RenderGraphPlan {
    let (w, h) = (input.width, input.height);
    let mut passes = Vec::with_capacity(RenderPassId::ORDER.len());

    // DepthPrepass: the same packed-vertex terrain shader, depth-only
    // targets. It owns the depth buffer + Hi-Z source image.
    passes.push(PlannedPass {
        id: RenderPassId::DepthPrepass,
        label: "depth-prepass",
        shader: Some(ShaderModule::TerrainOpaque),
        submission: match pipeline {
            GpuPipelineKind::MeshShader => PassSubmission::TaskAmplification,
            GpuPipelineKind::ComputeIndirect => PassSubmission::IndirectDrawCount,
            GpuPipelineKind::Classic | GpuPipelineKind::ClassicLowSpec => {
                PassSubmission::DirectDraws
            }
        },
        bindings: terrain_bindings(),
        estimated_draw_calls: terrain_draw_calls(pipeline, input.visible_sections),
        estimated_vram_bytes: bytes(w, h, 4),
    });

    if pipeline.builds_hiz() {
        // HiZBuild: one downsample dispatch; the chain it writes is sized
        // from the full mip series over the depth target.
        passes.push(PlannedPass {
            id: RenderPassId::HiZBuild,
            label: "hiz-build",
            shader: Some(ShaderModule::HiZDownsample),
            submission: PassSubmission::ComputeDispatch,
            bindings: vec![
                bind(0, 0, "src_depth_mip", ResourceKind::SampledTexture),
                bind(0, 1, "dst_depth_mip", ResourceKind::StorageTexture),
                bind(0, 2, "hiz_params", ResourceKind::UniformBuffer),
            ],
            estimated_draw_calls: 1,
            estimated_vram_bytes: hiz_bytes(w, h),
        });
    }

    if pipeline.runs_meshlet_cull() {
        // MeshletCull: amplification on mesh-shader GPUs, a compute
        // dispatch emitting indirect commands on compute+indirect GPUs.
        passes.push(PlannedPass {
            id: RenderPassId::MeshletCull,
            label: "meshlet-cull",
            shader: Some(ShaderModule::MeshletCull),
            submission: match pipeline {
                GpuPipelineKind::MeshShader => PassSubmission::TaskAmplification,
                _ => PassSubmission::ComputeDispatch,
            },
            bindings: vec![
                bind(0, 0, "cull_uniforms", ResourceKind::UniformBuffer),
                bind(0, 1, "meshlet_records", ResourceKind::StorageRead),
                bind(0, 2, "indirect_commands", ResourceKind::StorageReadWrite),
                bind(0, 3, "indirect_command_count", ResourceKind::StorageReadWrite),
                bind(0, 4, "hiz_pyramid", ResourceKind::SampledTexture),
            ],
            estimated_draw_calls: 1,
            estimated_vram_bytes: u64::from(input.meshlet_count)
                * (MESHLET_RECORD_BYTES + INDIRECT_COMMAND_BYTES),
        });
    }

    passes.push(PlannedPass {
        id: RenderPassId::OpaqueTerrain,
        label: "opaque-terrain",
        shader: Some(ShaderModule::TerrainOpaque),
        submission: match pipeline {
            GpuPipelineKind::MeshShader => PassSubmission::MeshletIndirectDraw,
            GpuPipelineKind::ComputeIndirect => PassSubmission::IndirectDrawCount,
            GpuPipelineKind::Classic | GpuPipelineKind::ClassicLowSpec => {
                PassSubmission::DirectDraws
            }
        },
        bindings: terrain_bindings(),
        estimated_draw_calls: terrain_draw_calls(pipeline, input.visible_sections),
        // Color target + per-vertex packed geometry (~1 KiB/section).
        estimated_vram_bytes: bytes(w, h, 4) + u64::from(input.visible_sections) * 1024,
    });

    let instanced_draws = instanced_draw_calls(
        pipeline,
        input.block_entity_instances,
        input.block_entity_mesh_kinds,
    );
    let instanced_submission = pipeline.instanced_submission();
    passes.push(PlannedPass {
        id: RenderPassId::InstancedBlockEntities,
        label: "instanced-block-entities",
        shader: Some(match instanced_submission {
            InstanceSubmission::IndirectSsbo => ShaderModule::BlockEntityInstancedSsbo,
            InstanceSubmission::UboBatches16k => ShaderModule::BlockEntityInstancedUbo,
        }),
        submission: match instanced_submission {
            InstanceSubmission::IndirectSsbo => match pipeline {
                GpuPipelineKind::MeshShader => PassSubmission::IndirectDrawCount,
                GpuPipelineKind::ComputeIndirect => PassSubmission::IndirectDrawCount,
                _ => PassSubmission::DirectDraws,
            },
            InstanceSubmission::UboBatches16k => PassSubmission::UboBatches16k,
        },
        bindings: instanced_bindings(instanced_submission),
        estimated_draw_calls: instanced_draws,
        estimated_vram_bytes: instanced_vram_bytes(
            pipeline,
            input.block_entity_instances,
            instanced_draws,
        ),
    });

    // Sky fills every pixel the terrain left; the fullscreen pass costs
    // one draw and owns the sky gradient LUT.
    passes.push(PlannedPass {
        id: RenderPassId::SkyAndClouds,
        label: "sky-and-clouds",
        shader: None,
        submission: PassSubmission::DirectDraws,
        bindings: Vec::new(),
        estimated_draw_calls: 1,
        estimated_vram_bytes: 4096,
    });

    passes.push(PlannedPass {
        id: RenderPassId::TranslucentTerrain,
        label: "translucent-terrain",
        shader: Some(ShaderModule::TerrainTranslucent),
        submission: PassSubmission::DirectDraws,
        bindings: terrain_bindings(),
        estimated_draw_calls: input.translucent_sections,
        // CPU back-to-front sort indices, 4 B per translucent quad.
        estimated_vram_bytes: u64::from(input.translucent_sections) * 256,
    });

    passes.push(PlannedPass {
        id: RenderPassId::HudOverlay,
        label: "hud-overlay",
        shader: None,
        submission: PassSubmission::DirectDraws,
        bindings: Vec::new(),
        estimated_draw_calls: 1,
        estimated_vram_bytes: 4096,
    });

    debug_assert!(
        passes
            .iter()
            .map(|p| p.id as u8)
            .collect::<Vec<_>>()
            .windows(2)
            .all(|w| w[0] < w[1]),
        "passes must stay in RenderPassId::ORDER"
    );
    RenderGraphPlan {
        pipeline,
        passes,
        shaderpack_passes: Vec::new(),
        active_shaderpack: None,
        vfx_pass: None,
    }
}

/// Map the active GPU pipeline to the VFX simulation and draw cap. The
/// mesh-shader and classic paths share the balanced cap, while the
/// compute-indirect path has the high cap.
pub const fn vfx_tier_for_pipeline(pipeline: GpuPipelineKind) -> VfxHardwareTier {
    match pipeline {
        GpuPipelineKind::ClassicLowSpec => VfxHardwareTier::LowSpec,
        GpuPipelineKind::MeshShader | GpuPipelineKind::Classic => VfxHardwareTier::Balanced,
        GpuPipelineKind::ComputeIndirect => VfxHardwareTier::High,
    }
}

pub const fn vfx_budget_for_pipeline(pipeline: GpuPipelineKind) -> VfxBudget {
    VfxBudget::for_tier(vfx_tier_for_pipeline(pipeline))
}

/// Plan translucent VFX work with the pipeline's default budget.
pub fn plan_frame_with_vfx(
    pipeline: GpuPipelineKind,
    input: &FramePlanInput,
    vfx: &VfxFrameOutput,
) -> RenderGraphPlan {
    plan_frame_with_vfx_tier(pipeline, input, vfx, vfx_tier_for_pipeline(pipeline))
}

/// Plan VFX with a governor-adjusted tier, limiting even caller-supplied
/// frame data to the selected budget before it reaches GPU consumers.
pub fn plan_frame_with_vfx_tier(
    pipeline: GpuPipelineKind,
    input: &FramePlanInput,
    vfx: &VfxFrameOutput,
    tier: VfxHardwareTier,
) -> RenderGraphPlan {
    let budget = VfxBudget::for_tier(tier);
    let ribbon_quad_count = vfx
        .ribbons
        .iter()
        .take(budget.max_ribbons)
        .map(|ribbon| ribbon.vertices.len() / 2)
        .map(|pairs| pairs.saturating_sub(1))
        .sum::<usize>()
        .min(budget.max_ribbon_segments);
    let summary = VfxPassSummary {
        active_particles: vfx.particles.len().min(budget.max_active_particles),
        beam_count: vfx.beams.len().min(budget.max_beams),
        ribbon_quad_count,
        aura_count: vfx.auras.len().min(budget.max_auras),
        dynamic_light_count: vfx.lights.len().min(budget.max_dynamic_lights),
        budget_preset: tier,
    };
    // A particle, ribbon segment, or aura uses one quad, while beams use
    // core and glow quads. 36 bytes per vertex (position, RGBA, UV) and
    // 32 bytes per dynamic colored light (position, RGB, radius, intensity).
    let quads = summary.active_particles as u64
        + summary.beam_count as u64 * 2
        + summary.ribbon_quad_count as u64
        + summary.aura_count as u64;
    let mut plan = plan_frame(pipeline, input);
    plan.vfx_pass = Some(Box::new(VfxPlannedPass {
        label: "vfx_particles_beams_ribbons",
        after_pass: RenderPassId::TranslucentTerrain,
        submission: PassSubmission::DirectDraws,
        estimated_draw_calls: quads as u32,
        estimated_vram_bytes: quads * 4 * 36 + summary.dynamic_light_count as u64 * 32,
        summary,
    }));
    plan
}

/// Deterministic plan entry for one shaderpack pass. Compute passes report
/// their declared workgroup count as dispatch cost; geometry stages
/// (`shadow`/`dh_shadow` casters, `gbuffers` terrain, `dh_terrain`/
/// `dh_water` LOD) re-issue the pipeline's section submission and budget
/// per-section geometry, while fullscreen stages (`begin`/`prepare`/
/// `deferred`/`composite`/`final`) cost a single draw per declared
/// attachment.
fn shaderpack_pass(
    name: String,
    stage: PipelineStage,
    kind: PassKind,
    render_targets: Vec<String>,
    work_groups: Option<[u32; 3]>,
    pipeline: GpuPipelineKind,
    input: &FramePlanInput,
) -> ShaderpackPlannedPass {
    let workgroup_cost = |groups: Option<[u32; 3]>| -> u32 {
        groups
            .map(|g| g[0].max(1) * g[1].max(1) * g[2].max(1))
            .unwrap_or(1)
    };
    let geometry_stage = matches!(
        stage,
        PipelineStage::Shadow | PipelineStage::Gbuffers | PipelineStage::DistantHorizons
    );
    let (estimated_draw_calls, estimated_vram_bytes) = match kind {
        PassKind::Compute => (
            workgroup_cost(work_groups),
            u64::from(workgroup_cost(work_groups)) * 16,
        ),
        PassKind::Graphics => {
            let target_bytes =
                bytes(input.width, input.height, 4) * render_targets.len().max(1) as u64;
            if geometry_stage {
                (
                    terrain_draw_calls(pipeline, input.visible_sections),
                    target_bytes + u64::from(input.visible_sections) * 1024,
                )
            } else {
                (1, target_bytes)
            }
        }
    };
    ShaderpackPlannedPass {
        name,
        stage,
        kind,
        render_targets,
        work_groups,
        estimated_draw_calls,
        estimated_vram_bytes,
    }
}

/// Layer an activated shaderpack's ordered passes over the deterministic
/// built-in frame plan.
///
/// The vanilla [`plan_frame`] sequence stays untouched as the geometry
/// submission and budget base; the pack's programs append to
/// [`RenderGraphPlan::shaderpack_passes`] in Iris execution order
/// (`begin`/`shadow`/`dh_shadow`/`prepare` compute and shadow passes →
/// `gbuffers`/`dh_terrain`/`dh_water` geometry → `deferred`/`composite`/
/// `final` fullscreen), each folded into the plan's total draw-call and
/// VRAM estimates. `plan_frame` itself is unchanged for vanilla rendering.
pub fn plan_frame_with_shaderpack(
    pipeline: GpuPipelineKind,
    input: &FramePlanInput,
    session: &ActiveShaderSession,
) -> RenderGraphPlan {
    let mut plan = plan_frame(pipeline, input);
    plan.shaderpack_passes = match &session.pack {
        LoadedPackKind::IrisOptiFine(pack) => pack
            .passes
            .iter()
            .map(|p| {
                shaderpack_pass(
                    p.name.clone(),
                    p.stage,
                    p.kind,
                    p.render_targets.clone().unwrap_or_default(),
                    p.work_groups,
                    pipeline,
                    input,
                )
            })
            .collect(),
        LoadedPackKind::MeguWgsl(pack) => pack
            .passes
            .iter()
            .map(|p| {
                shaderpack_pass(
                    p.name.clone(),
                    p.stage,
                    p.kind,
                    p.render_targets.clone(),
                    p.work_groups,
                    pipeline,
                    input,
                )
            })
            .collect(),
    };
    plan.active_shaderpack = Some(session.pack_name.clone());
    plan
}
