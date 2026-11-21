// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later

//! Vulkan-ready GPU data layout and backend selection for the client renderer.
//!
//! Three clean-room pieces, all pure `std` and `#[repr(C)]` where the GPU reads
//! the bytes:
//!
//! * [`resolve_render_backend`] picks [`ActiveRenderBackend::MeshShader`]
//!   (`VK_EXT_mesh_shader`), [`ActiveRenderBackend::ComputeIndirect`] (compute
//!   cull + `vkCmdDrawIndexedIndirectCount`), [`ActiveRenderBackend::Classic`]
//!   or [`ActiveRenderBackend::ClassicLowSpec`] (OpenGL 3.3 / Vulkan 1.1
//!   minimal-hardware path for legacy Intel iGPUs) from reported device
//!   capabilities under an [`RenderBackendPreference`].
//! * [`pack_section_meshlets`] folds a meshed section's quads into
//!   direction-pure runs of at most [`MAX_QUADS_PER_MESHLET`] quads described
//!   by [`MeshletDescriptor`], the work unit a mesh/task shader workgroup
//!   emits.
//! * [`KineticInstancePool`] is a hardware-instancing pool for rotating
//!   mod-contraption geometry: dense 48-byte [`KineticInstanceGpu`] records in
//!   an SSBO plus one [`DrawIndexedIndirectCommand`] per mesh kind, matching
//!   `VkDrawIndexedIndirectCommand`.

use std::collections::BTreeMap;

use crate::mesh::{DirectionBuckets, DIRECTION_BUCKETS, DIRECTION_BUCKET_UNASSIGNED};
use crate::section::{Direction, ALL_FACES};
use crate::Quad;

// ---------------------------------------------------------------------------
// Backend selection
// ---------------------------------------------------------------------------

/// Quads per meshlet work unit: 64 quads = 128 triangles = 256 vertices, the
/// `VK_EXT_mesh_shader` output budget every shipping implementation (NVIDIA
/// RTX 20+, AMD RDNA2+, Intel Arc) guarantees.
pub const MAX_QUADS_PER_MESHLET: usize = 64;
/// Mesh-shader vertices the packer needs per workgroup (`maxMeshOutputVertices`).
pub const REQUIRED_MESH_OUTPUT_VERTICES: u32 = (MAX_QUADS_PER_MESHLET * 4) as u32;
/// Mesh-shader primitives the packer needs per workgroup
/// (`maxMeshOutputPrimitives`, one triangle pair per quad).
pub const REQUIRED_MESH_OUTPUT_PRIMITIVES: u32 = (MAX_QUADS_PER_MESHLET * 2) as u32;

/// Renderer path the user or a launcher setting asked for.
#[repr(u8)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RenderBackendPreference {
    /// Negotiate from hardware capabilities: mesh shader, else
    /// compute+indirect, else classic.
    #[default]
    Auto = 0,
    /// Require `VK_EXT_mesh_shader` (task+mesh pipeline).
    MeshShader = 1,
    /// Require compute culling plus `vkCmdDrawIndexedIndirectCount`.
    ComputeIndirect = 2,
    /// Plain vertex pipeline with per-section draws; always available.
    Classic = 3,
    /// Minimal-hardware vertex pipeline for legacy Intel integrated GPUs
    /// (Intel HD 4000-630, UHD 620-630, Gen7-Gen11): assumes only
    /// OpenGL 3.3 / Vulkan 1.1 core — no `VK_EXT_mesh_shader`, no
    /// `vkCmdDrawIndexedIndirectCount`, no compute shaders, no SSBO.
    /// Kinetic instances upload through 16 KiB-safe UBO batches instead
    /// (see [`KineticInstancePool::build_ubo_batches`]). Always available.
    ClassicLowSpec = 4,
}

impl RenderBackendPreference {
    /// Lowercase TOML/CLI token for this preference.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::MeshShader => "mesh-shader",
            Self::ComputeIndirect => "compute-indirect",
            Self::Classic => "classic",
            Self::ClassicLowSpec => "classic-low-spec",
        }
    }

    /// Parse a TOML/CLI token (case-insensitive, `-`/`_` interchangeable);
    /// `None` when the value names no backend, so callers fail closed.
    pub fn parse(value: &str) -> Option<Self> {
        match value.to_ascii_lowercase().replace('_', "-").as_str() {
            "auto" => Some(Self::Auto),
            "mesh-shader" => Some(Self::MeshShader),
            "compute-indirect" => Some(Self::ComputeIndirect),
            "classic" => Some(Self::Classic),
            "classic-low-spec" => Some(Self::ClassicLowSpec),
            _ => None,
        }
    }
}

/// GPU vendor id as reported by `VkPhysicalDeviceProperties::vendorID`, kept
/// coarse because the capability bits — not the vendor — drive selection.
#[repr(u8)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum GpuVendor {
    Nvidia = 0,
    Amd = 1,
    Intel = 2,
    Apple = 3,
    #[default]
    Other = 4,
}

/// Device capabilities the Vulkan layer surfaces before backend selection.
/// Boolean flags mean "feature present and usable"; the mesh output maxima
/// mirror `VkPhysicalDeviceMeshShaderPropertiesEXT`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct GpuCapabilities {
    pub vendor: GpuVendor,
    /// `VK_EXT_mesh_shader` exposed with `meshShader` + `taskShader` enabled.
    pub supports_ext_mesh_shader: bool,
    /// `maxMeshOutputVertices` (256 on every shipping mesh-shader GPU).
    pub max_mesh_output_vertices: u32,
    /// `maxMeshOutputPrimitives` (>= 256 on NVIDIA/AMD/Intel mesh-shader GPUs).
    pub max_mesh_output_primitives: u32,
    /// `VK_KHR_draw_indirect_count` / Vulkan 1.2 `vkCmdDrawIndexedIndirectCount`.
    pub supports_multi_draw_indirect_count: bool,
    /// A usable compute queue for the cull/compact pass.
    pub supports_compute_shaders: bool,
}

impl GpuCapabilities {
    /// Whether [`MAX_QUADS_PER_MESHLET`]-quad meshlets fit the device's mesh
    /// shader output budget.
    pub fn mesh_shader_adequate(&self) -> bool {
        self.supports_ext_mesh_shader
            && self.max_mesh_output_vertices >= REQUIRED_MESH_OUTPUT_VERTICES
            && self.max_mesh_output_primitives >= REQUIRED_MESH_OUTPUT_PRIMITIVES
    }

    /// Whether the compute-cull + indirect-count path can run.
    pub fn compute_indirect_adequate(&self) -> bool {
        self.supports_compute_shaders && self.supports_multi_draw_indirect_count
    }

    /// Legacy Intel integrated profile (HD 4000-630, UHD 620-630,
    /// Gen7-Gen11): the minimal OpenGL 3.3 / Vulkan 1.1 feature floor —
    /// no mesh shader, no indirect count, no compute, no SSBO.
    /// [`RenderBackendPreference::ClassicLowSpec`] is built for exactly
    /// this shape.
    pub fn intel_hd_legacy() -> Self {
        Self {
            vendor: GpuVendor::Intel,
            ..Self::minimal()
        }
    }

    /// Intel UHD 620/630 integrated profile: shares the legacy minimal
    /// feature floor; pair with a shared-RAM [`GpuMemoryProfile`].
    pub fn intel_uhd_630() -> Self {
        Self::intel_hd_legacy()
    }

    /// Intel Iris Xe (Gen12) integrated profile: mesh-shader capable with
    /// a full output budget, like discrete Arc.
    pub fn intel_iris_xe() -> Self {
        Self {
            vendor: GpuVendor::Intel,
            ..Self::full_gpu()
        }
    }

    /// Discrete NVIDIA profile (RTX 20+): full mesh-shader output budget.
    pub fn nvidia_rtx() -> Self {
        Self {
            vendor: GpuVendor::Nvidia,
            ..Self::full_gpu()
        }
    }

    /// Discrete AMD profile (RDNA2+): mesh shader with the 256-primitive
    /// budget RDNA ships.
    pub fn amd_rdna2() -> Self {
        Self {
            vendor: GpuVendor::Amd,
            supports_ext_mesh_shader: true,
            max_mesh_output_vertices: 256,
            max_mesh_output_primitives: 256,
            supports_multi_draw_indirect_count: true,
            supports_compute_shaders: true,
        }
    }

    /// Apple silicon through Metal/MoltenVK: no `VK_EXT_mesh_shader`, but
    /// compute and `vkCmdDrawIndexedIndirectCount` are available.
    pub fn apple_silicon() -> Self {
        Self {
            vendor: GpuVendor::Apple,
            supports_multi_draw_indirect_count: true,
            supports_compute_shaders: true,
            ..Self::minimal()
        }
    }

    /// Software rasterizer profile: no GPU culling features at all.
    pub fn software() -> Self {
        Self::minimal()
    }

    /// Every feature flag clear: the floor legacy Intel iGPUs sit on.
    fn minimal() -> Self {
        Self {
            vendor: GpuVendor::Other,
            supports_ext_mesh_shader: false,
            max_mesh_output_vertices: 0,
            max_mesh_output_primitives: 0,
            supports_multi_draw_indirect_count: false,
            supports_compute_shaders: false,
        }
    }

    /// Mesh-shader-class discrete feature set with a full output budget.
    fn full_gpu() -> Self {
        Self {
            vendor: GpuVendor::Other,
            supports_ext_mesh_shader: true,
            max_mesh_output_vertices: 256,
            max_mesh_output_primitives: 512,
            supports_multi_draw_indirect_count: true,
            supports_compute_shaders: true,
        }
    }
}

/// The backend a frame actually submits with.
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActiveRenderBackend {
    /// Task+mesh shader pipeline over [`MeshletDescriptor`] work units.
    MeshShader = 0,
    /// Compute cull/compact + `vkCmdDrawIndexedIndirectCount`.
    ComputeIndirect = 1,
    /// Baseline vertex pipeline, per-section multidraw on the CPU side.
    Classic = 2,
    /// Minimal-hardware vertex pipeline for legacy Intel iGPUs: per-section
    /// multidraw like [`Self::Classic`], but kinetic contraptions upload
    /// through [`MAX_INSTANCES_PER_UBO_16KB`]-instance UBO batches instead
    /// of an SSBO, and [`recommend_low_spec_budget`] tightens mesh-cache
    /// and rebuild budgets for shared-RAM / low-VRAM devices.
    ClassicLowSpec = 3,
}

impl ActiveRenderBackend {
    /// Lowercase diagnostic token for this backend.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::MeshShader => "mesh-shader",
            Self::ComputeIndirect => "compute-indirect",
            Self::Classic => "classic",
            Self::ClassicLowSpec => "classic-low-spec",
        }
    }
}

/// Outcome of [`resolve_render_backend`]: the selected pipeline plus a
/// human-readable explanation when the selection is weaker than requested.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BackendResolution {
    pub backend: ActiveRenderBackend,
    /// Why a requested or negotiated GPU path was not used. `None` when the
    /// preference was honored exactly (or `Auto` landed on a GPU path); always
    /// `Some` for `Classic`, which only runs when nothing better exists.
    pub fallback_reason: Option<String>,
}

fn resolution(backend: ActiveRenderBackend) -> BackendResolution {
    BackendResolution {
        backend,
        fallback_reason: None,
    }
}

/// Negotiate like [`RenderBackendPreference::Auto`], carrying `reason` through
/// when an explicit request could not be honored.
fn negotiate(caps: &GpuCapabilities, reason: Option<String>) -> BackendResolution {
    if caps.mesh_shader_adequate() {
        BackendResolution {
            backend: ActiveRenderBackend::MeshShader,
            fallback_reason: reason,
        }
    } else if caps.compute_indirect_adequate() {
        BackendResolution {
            backend: ActiveRenderBackend::ComputeIndirect,
            fallback_reason: reason,
        }
    } else {
        BackendResolution {
            backend: ActiveRenderBackend::Classic,
            fallback_reason: Some(match reason {
                Some(reason) => reason,
                None => "device lacks VK_EXT_mesh_shader and compute+indirect-count support; \
                     using the classic vertex pipeline"
                    .to_string(),
            }),
        }
    }
}

/// Select the active backend for `pref` under `caps`. Never panics: any
/// explicit request the hardware cannot honor degrades through the same
/// ordering `Auto` uses, reporting the missing capability in
/// `fallback_reason`.
pub fn resolve_render_backend(
    pref: RenderBackendPreference,
    caps: &GpuCapabilities,
) -> BackendResolution {
    match pref {
        RenderBackendPreference::Auto => negotiate(caps, None),
        RenderBackendPreference::MeshShader => {
            if caps.mesh_shader_adequate() {
                resolution(ActiveRenderBackend::MeshShader)
            } else {
                negotiate(
                    caps,
                    Some(format!(
                        "VK_EXT_mesh_shader requested but unavailable or too small \
                         (extension: {}, max output vertices: {} < {}, max output \
                         primitives: {} < {}); fell back",
                        caps.supports_ext_mesh_shader,
                        caps.max_mesh_output_vertices,
                        REQUIRED_MESH_OUTPUT_VERTICES,
                        caps.max_mesh_output_primitives,
                        REQUIRED_MESH_OUTPUT_PRIMITIVES,
                    )),
                )
            }
        }
        RenderBackendPreference::ComputeIndirect => {
            if caps.compute_indirect_adequate() {
                resolution(ActiveRenderBackend::ComputeIndirect)
            } else {
                negotiate(
                    caps,
                    Some(format!(
                        "compute+indirect-count requested but unavailable (compute: {}, \
                         indirect count: {}); fell back",
                        caps.supports_compute_shaders, caps.supports_multi_draw_indirect_count,
                    )),
                )
            }
        }
        RenderBackendPreference::Classic => resolution(ActiveRenderBackend::Classic),
        // Needs nothing beyond a GL 3.3 / Vulkan 1.1 core context, so it is
        // honored on every device including the legacy Intel iGPUs it was
        // designed for.
        RenderBackendPreference::ClassicLowSpec => {
            resolution(ActiveRenderBackend::ClassicLowSpec)
        }
    }
}

// ---------------------------------------------------------------------------
// Memory-aware low-spec render budgets
// ---------------------------------------------------------------------------

/// Uniform-buffer size every OpenGL 3.3 (`GL_MAX_UNIFORM_BLOCK_SIZE`) and
/// Vulkan 1.1 (`maxUniformBufferRange`) implementation must expose: 16 KiB.
/// [`MAX_INSTANCES_PER_UBO_16KB`] is sized so one UBO batch always fits.
pub const MIN_GUARANTEED_UBO_BYTES: u32 = 16 * 1024;
/// Instances per 16 KiB-safe kinetic UBO batch: `256 * 48 B = 12,288 B`,
/// comfortably inside [`MIN_GUARANTEED_UBO_BYTES`] on every Intel HD 4000+.
pub const MAX_INSTANCES_PER_UBO_16KB: usize = 256;
/// Bytes per [`KineticInstanceGpu`] instance record.
pub const KINETIC_INSTANCE_GPU_BYTES: u32 = std::mem::size_of::<KineticInstanceGpu>() as u32;
/// Dedicated-VRAM ceiling (MiB) at or below which a device counts as
/// low-spec; any shared-system-RAM device is low-spec regardless.
pub const LOW_SPEC_VRAM_CEILING_MIB: u32 = 512;

/// Reported memory environment for [`recommend_low_spec_budget`].
/// `shared_system_ram` marks integrated GPUs whose "VRAM" is carved out of
/// system memory (every Intel HD/UHD iGPU); `max_ubo_bytes` is the device's
/// uniform-buffer limit — use [`MIN_GUARANTEED_UBO_BYTES`] when unknown.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GpuMemoryProfile {
    /// Device-local video memory in MiB (`0` = none / unknown).
    pub vram_mib: u32,
    /// Whether the GPU shares system RAM instead of owning dedicated VRAM.
    pub shared_system_ram: bool,
    /// `GL_MAX_UNIFORM_BLOCK_SIZE` / `maxUniformBufferRange` in bytes.
    pub max_ubo_bytes: u32,
}

impl GpuMemoryProfile {
    /// Discrete GPU with dedicated VRAM and the guaranteed 16 KiB UBO floor.
    pub fn dedicated(vram_mib: u32) -> Self {
        Self {
            vram_mib,
            shared_system_ram: false,
            max_ubo_bytes: MIN_GUARANTEED_UBO_BYTES,
        }
    }

    /// Integrated GPU sharing system RAM (all Intel HD/UHD iGPUs).
    pub fn integrated_shared(vram_mib: u32) -> Self {
        Self {
            vram_mib,
            shared_system_ram: true,
            max_ubo_bytes: MIN_GUARANTEED_UBO_BYTES,
        }
    }

    /// Largest safe instance count for
    /// [`KineticInstancePool::build_ubo_batches`]: how many 48-byte records
    /// fit `max_ubo_bytes`, capped at [`MAX_INSTANCES_PER_UBO_16KB`].
    pub fn max_instances_per_ubo(&self) -> usize {
        ((self.max_ubo_bytes / KINETIC_INSTANCE_GPU_BYTES) as usize)
            .min(MAX_INSTANCES_PER_UBO_16KB)
    }
}

/// Render budgets [`recommend_low_spec_budget`] derives for a device: how
/// much mesh cache it may hold, how many dirty sections may rebuild per
/// frame, and the chunk radii the renderer should aim at.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LowSpecRenderBudget {
    /// Mesh-cache budget in MiB (`u32::MAX` = unbounded).
    pub max_mesh_cache_mib: u32,
    /// Dirty-section rebuilds allowed per frame (`u32::MAX` = uncapped).
    pub max_dirty_rebuilds_per_frame: u32,
    /// Full-detail chunk radius to target (the short near ring on
    /// low-spec hardware).
    pub recommended_full_detail_chunks: u32,
    /// Horizon-LOD chunk radius to target (kept far on low-spec hardware
    /// so the world still looks distant while the near ring stays cheap).
    pub recommended_lod_chunks: u32,
    /// Whether kinetic contraptions should draw through
    /// [`KineticInstancePool::build_ubo_batches`] UBO batches — the path
    /// that needs no SSBO on GL 3.3 / Vulkan 1.1 minimal hardware.
    pub use_ubo_kinetic_batches: bool,
}

/// Budget recommendation for `backend` under memory profile `mem`.
///
/// A device counts as low-spec when it runs [`ActiveRenderBackend::
/// ClassicLowSpec`], shares system RAM, or reports at most
/// [`LOW_SPEC_VRAM_CEILING_MIB`] MiB of VRAM. Low-spec budgets cap dirty
/// rebuilds at 2 per frame, shrink the full-detail ring to 6-8 chunks
/// (scaled by `vram_mib`), push horizon LOD far out (32 chunks), and
/// request UBO kinetic batching. Everything else keeps a generous
/// VRAM-scaled budget.
pub fn recommend_low_spec_budget(
    mem: &GpuMemoryProfile,
    backend: ActiveRenderBackend,
) -> LowSpecRenderBudget {
    let low_spec = backend == ActiveRenderBackend::ClassicLowSpec
        || mem.shared_system_ram
        || mem.vram_mib <= LOW_SPEC_VRAM_CEILING_MIB;
    if low_spec {
        LowSpecRenderBudget {
            max_mesh_cache_mib: if mem.shared_system_ram { 32 } else { 64 },
            max_dirty_rebuilds_per_frame: 2,
            recommended_full_detail_chunks: (mem.vram_mib / 64).clamp(6, 8),
            recommended_lod_chunks: 32,
            use_ubo_kinetic_batches: true,
        }
    } else {
        LowSpecRenderBudget {
            max_mesh_cache_mib: (mem.vram_mib / 4).clamp(128, 1024),
            max_dirty_rebuilds_per_frame: u32::MAX,
            recommended_full_detail_chunks: 12,
            recommended_lod_chunks: 64,
            use_ubo_kinetic_batches: false,
        }
    }
}

// ---------------------------------------------------------------------------
// 64-quad meshlet packer
// ---------------------------------------------------------------------------

/// One task/mesh-shader work unit over a section's grouped quad buffer.
///
/// The AABB is quantized to `u8` section-local coordinates: the packer emits
/// `floor(min)`/`ceil(max)`, so the stored box always covers the float quad
/// extents (a 16-block section needs only the 0..=16 range). `facing_mask`
/// holds one `Direction::bit` for direction-pure meshlets, or [`ALL_FACES`]
/// for the unassigned bucket a directional cull may never drop.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MeshletDescriptor {
    /// Quantized inclusive lower corner of the quad extents.
    pub aabb_min: [u8; 3],
    /// Quantized inclusive upper corner of the quad extents.
    pub aabb_max: [u8; 3],
    /// `Direction::bit`s this meshlet's quads face; [`ALL_FACES`] when
    /// unassigned, so `facing & mask != 0` keeps it whenever any side is lit.
    pub facing_mask: u8,
    /// Reserved, always zero.
    pub flags: u8,
    /// First quad of this meshlet in [`SectionMeshlets::quads`].
    pub quad_offset: u32,
    /// Quad count, 1..=[`MAX_QUADS_PER_MESHLET`].
    pub quad_count: u32,
}

/// A meshed section repacked for mesh-shader dispatch: `quads` regrouped so
/// each meshlet's run is contiguous, and one [`MeshletDescriptor`] per run.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SectionMeshlets {
    /// `quads` reordered into direction-bucket order, then by meshlet.
    /// `meshlets[i]` reads `quads[quad_offset..quad_offset + quad_count]`.
    pub quads: Vec<Quad>,
    /// Meshlet descriptors in emission order; every bucket's meshlets are
    /// contiguous and quad counts sum exactly to `quads.len()`.
    pub meshlets: Vec<MeshletDescriptor>,
}

/// Direction-pure meshlets over a meshed section. Quads are grouped into the
/// seven [`DirectionBuckets`] (stable inside each bucket) and each bucket's
/// run is split into chunks of at most [`MAX_QUADS_PER_MESHLET`], so a task
/// shader can drop a whole workgroup with a single bit-test on
/// `facing_mask`. The sum of `quad_count` always equals `quads.len()`.
/// Panics only when `quads` exceeds `u32::MAX` entries.
pub fn pack_section_meshlets(quads: &[Quad]) -> SectionMeshlets {
    let (grouped, buckets) = DirectionBuckets::group(quads);
    let mut meshlets = Vec::with_capacity(quads.len() / MAX_QUADS_PER_MESHLET + 1);
    for bucket in 0..DIRECTION_BUCKETS {
        let facing_mask = if bucket == DIRECTION_BUCKET_UNASSIGNED {
            ALL_FACES
        } else {
            Direction::ALL[bucket].bit()
        };
        let base = buckets.offset[bucket] as usize;
        let end = base + buckets.count[bucket] as usize;
        let mut quad_offset = base as u32;
        for chunk in grouped[base..end].chunks(MAX_QUADS_PER_MESHLET) {
            let mut min = [u8::MAX; 3];
            let mut max = [0u8; 3];
            for quad in chunk {
                for position in &quad.positions {
                    for axis in 0..3 {
                        let lo = position[axis].floor().clamp(0.0, u8::MAX as f32) as u8;
                        let hi = position[axis].ceil().clamp(0.0, u8::MAX as f32) as u8;
                        min[axis] = min[axis].min(lo);
                        max[axis] = max[axis].max(hi);
                    }
                }
            }
            meshlets.push(MeshletDescriptor {
                aabb_min: min,
                aabb_max: max,
                facing_mask,
                flags: 0,
                quad_offset,
                quad_count: chunk.len() as u32,
            });
            quad_offset += chunk.len() as u32;
        }
    }
    SectionMeshlets {
        quads: grouped,
        meshlets,
    }
}

/// The box sides facing `camera` for an arbitrary `[min, max]` AABB — the
/// generalization of `crate::box_facing_mask` (which hardcodes a 16^3 box):
/// one `Direction::bit` per axis where the camera is strictly outside.
#[inline]
fn aabb_facing_mask(min: [f32; 3], max: [f32; 3], camera: [f32; 3]) -> u8 {
    const SIDES: [(Direction, Direction); 3] = [
        (Direction::West, Direction::East),
        (Direction::Down, Direction::Up),
        (Direction::North, Direction::South),
    ];
    let mut mask = 0;
    for (axis, (low, high)) in SIDES.into_iter().enumerate() {
        if camera[axis] < min[axis] {
            mask |= low.bit();
        } else if camera[axis] > max[axis] {
            mask |= high.bit();
        }
    }
    mask
}

/// Positive-vertex frustum test for an arbitrary `[min, max]` AABB against
/// `crate::box_in_frustum`-form planes (`dot(n, p) + d >= 0` inside).
#[inline]
fn aabb_in_frustum(min: [f32; 3], max: [f32; 3], planes: &[[f32; 4]]) -> bool {
    planes.iter().all(|&[nx, ny, nz, d]| {
        let p = [
            if nx >= 0.0 { max[0] } else { min[0] },
            if ny >= 0.0 { max[1] } else { min[1] },
            if nz >= 0.0 { max[2] } else { min[2] },
        ];
        nx * p[0] + ny * p[1] + nz * p[2] + d >= 0.0
    })
}

/// Bit-exact CPU oracle for the task-shader cull: returns the indices of
/// meshlets that survive, in emission order. A meshlet is dropped when its
/// quantized AABB lies fully outside any `frustum_planes` plane, or when the
/// camera provably sees none of its face directions — the camera is strictly
/// outside the box on some axis and `aabb_facing_mask & facing_mask == 0`.
/// A camera enclosed by the box (mask 0) keeps every direction, matching
/// `bucket_draw_mask`'s enclosed-camera rule.
pub fn cull_meshlets_cpu_reference(
    meshlets: &[MeshletDescriptor],
    frustum_planes: &[[f32; 4]],
    camera_pos: [f32; 3],
) -> Vec<u32> {
    let mut visible = Vec::with_capacity(meshlets.len());
    for (index, meshlet) in meshlets.iter().enumerate() {
        let min = meshlet.aabb_min.map(f32::from);
        let max = meshlet.aabb_max.map(f32::from);
        if !aabb_in_frustum(min, max, frustum_planes) {
            continue;
        }
        let facing = aabb_facing_mask(min, max, camera_pos);
        if facing != 0 && facing & meshlet.facing_mask == 0 {
            continue;
        }
        visible.push(index as u32);
    }
    visible
}

// ---------------------------------------------------------------------------
// Flywheel-equivalent GPU hardware instancing pool
// ---------------------------------------------------------------------------

/// Rotation axis values for [`KineticInstanceGpu::axis_and_flags`]
/// (`axis < 3`; X/Y/Z in section order).
pub const KINETIC_AXIS_MASK: u32 = 0x3;
/// `axis_and_flags` bit: rotation runs around the negative axis direction.
pub const KINETIC_FLAG_REVERSED: u32 = 1 << 2;
/// `axis_and_flags` bit: the kinetic network is overstressed (stopped).
pub const KINETIC_FLAG_OVERSTRESSED: u32 = 1 << 3;
/// `axis_and_flags` shift for the 12-bit sub-world index addressing
/// [`SubWorldTransformGpu`] table slots (0 = static world grid).
pub const KINETIC_SUBWORLD_SHIFT: u32 = 4;
/// Sub-world index capacity inside `axis_and_flags` (bits 4..16, 4096 slots).
pub const KINETIC_SUBWORLD_MAX: u32 = 0xfff;

/// Pack the `axis_and_flags` word: `axis` 0..=2 (X/Y/Z), `reversed` and
/// `overstressed` flag bits, `subworld_id` clamped to
/// [`KINETIC_SUBWORLD_MAX`].
pub const fn pack_axis_flags(
    axis: u32,
    reversed: bool,
    overstressed: bool,
    subworld_id: u32,
) -> u32 {
    (axis & KINETIC_AXIS_MASK)
        | ((reversed as u32) << 2)
        | ((overstressed as u32) << 3)
        | ((subworld_id & KINETIC_SUBWORLD_MAX) << KINETIC_SUBWORLD_SHIFT)
}

/// Dense GPU instance record for one rotating contraption part (gear, shaft,
/// belt segment, airship body). Exactly 48 bytes, `align(4)`, laid out for a
/// `std430` SSBO so the vertex shader rotates `pos`-relative vertices about
/// `axis` by `phase + angular_velocity * t`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct KineticInstanceGpu {
    /// Instance origin in render-world blocks.
    pub pos: [f32; 3],
    /// Signed angular velocity, radians per second.
    pub angular_velocity: f32,
    /// Phase offset in radians at `t = 0`.
    pub phase: f32,
    /// Packed axis/flags word; see [`pack_axis_flags`].
    pub axis_and_flags: u32,
    /// Packed light: block-light nibble | sky-light nibble << 4 per channel.
    pub light_packed: u32,
    /// RGBA tint, one byte per channel.
    pub tint_rgba: u32,
    /// Per-axis nonuniform scale applied before rotation.
    pub scale_xyz: [f32; 3],
    /// Index of the shared mesh template this instance draws.
    pub mesh_kind: u32,
}

impl KineticInstanceGpu {
    /// Rotation axis encoded in `axis_and_flags` (0 = X, 1 = Y, 2 = Z).
    pub fn axis(&self) -> u32 {
        self.axis_and_flags & KINETIC_AXIS_MASK
    }

    /// Whether the instance rotates around the negative axis direction.
    pub fn reversed(&self) -> bool {
        self.axis_and_flags & KINETIC_FLAG_REVERSED != 0
    }

    /// Whether the owning kinetic network is overstressed.
    pub fn overstressed(&self) -> bool {
        self.axis_and_flags & KINETIC_FLAG_OVERSTRESSED != 0
    }

    /// Sub-world transform slot (0 = static world grid).
    pub fn subworld_id(&self) -> u32 {
        (self.axis_and_flags >> KINETIC_SUBWORLD_SHIFT) & KINETIC_SUBWORLD_MAX
    }
}

/// One `VkDrawIndexedIndirectCommand`, verbatim: an instanced indexed draw
/// whose parameters come from a device buffer.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DrawIndexedIndirectCommand {
    pub index_count: u32,
    pub instance_count: u32,
    pub first_index: u32,
    pub vertex_offset: i32,
    pub first_instance: u32,
}

/// Shared mesh template for one `mesh_kind`: where its index run lives in
/// the renderer's merged geometry buffer.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MeshKindTemplate {
    pub index_count: u32,
    pub first_index: u32,
    pub vertex_offset: i32,
}

/// 6-DOF rigid transform for one contraption/airship sub-world, as a 4x4
/// column-major matrix (64 bytes). Slot 0 conventionally stays
/// [`SubWorldTransformGpu::IDENTITY`] for the static world grid; a flying
/// airship's instances carry its `subworld_id` so both kinds render on the
/// same depth buffer in one pass.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SubWorldTransformGpu {
    /// Column-major 4x4: `matrix[col * 4 + row]`.
    pub matrix: [f32; 16],
}

impl SubWorldTransformGpu {
    /// Identity transform (static world grid).
    pub const IDENTITY: Self = Self {
        matrix: [
            1.0, 0.0, 0.0, 0.0, //
            0.0, 1.0, 0.0, 0.0, //
            0.0, 0.0, 1.0, 0.0, //
            0.0, 0.0, 0.0, 1.0,
        ],
    };
}

impl Default for SubWorldTransformGpu {
    fn default() -> Self {
        Self::IDENTITY
    }
}

/// One uniform-buffer-sized draw batch for the
/// [`ActiveRenderBackend::ClassicLowSpec`] path: at most
/// `max_instances_per_ubo` [`KineticInstanceGpu`] records of a single
/// `mesh_kind`, uploaded through one UBO bind instead of the shared SSBO
/// the indirect path uses.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct KineticUboBatch {
    /// The shared mesh template this batch draws.
    pub mesh_kind: u32,
    /// Index run length in the merged geometry buffer.
    pub index_count: u32,
    /// First index of the template's run in the merged geometry buffer.
    pub first_index: u32,
    /// Vertex offset applied when indexing the template's run.
    pub vertex_offset: i32,
    /// This batch's offset in the frame's packed instance stream (the sum
    /// of all earlier batches' instance counts, across all mesh kinds).
    pub first_instance: u32,
    /// The exact instance records this batch uploads, in the same
    /// deterministic order [`KineticInstancePool::build_indirect_batches`]
    /// writes them.
    pub instances: Vec<KineticInstanceGpu>,
}

/// One frame's instancing upload: the dense instance SSBO, the indirect
/// command buffer, and the dense sub-world transform table.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct KineticFrameUpload {
    /// Instance records grouped by `mesh_kind`; command `first_instance`
    /// indexes this array.
    pub instances: Vec<KineticInstanceGpu>,
    /// Exactly one [`DrawIndexedIndirectCommand`] per active `mesh_kind` that
    /// has a registered template, in ascending `mesh_kind` order.
    pub commands: Vec<DrawIndexedIndirectCommand>,
    /// Dense transform table indexed by `subworld_id`: `0..=max_id`, padded
    /// with [`SubWorldTransformGpu::IDENTITY`]. Empty when no sub-world is
    /// registered.
    pub subworlds: Vec<SubWorldTransformGpu>,
}

/// Hardware-instancing pool for kinetic contraption geometry. Upserts and
/// removals keep instances keyed by `instance_id`; each frame
/// [`build_indirect_batches`](Self::build_indirect_batches) packs every active
/// instance into a deterministic SSBO (sorted by `mesh_kind`, then
/// `instance_id`) and emits exactly one indirect command per mesh kind — a
/// 100k-gear scene submits one draw per mesh template.
#[derive(Default)]
pub struct KineticInstancePool {
    /// `instance_id -> record`; BTreeMap keeps a stable id order.
    instances: BTreeMap<u64, KineticInstanceGpu>,
    /// `mesh_kind -> merged-buffer run`.
    templates: BTreeMap<u32, MeshKindTemplate>,
    /// `subworld_id -> transform`; slot 0 is the static grid.
    subworlds: BTreeMap<u32, SubWorldTransformGpu>,
}

impl KineticInstancePool {
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert or overwrite `instance_id`. The explicit `mesh_kind` is
    /// authoritative and is written into `record.mesh_kind` so the SSBO
    /// record always matches its batch grouping.
    pub fn upsert(&mut self, instance_id: u64, mesh_kind: u32, mut record: KineticInstanceGpu) {
        record.mesh_kind = mesh_kind;
        self.instances.insert(instance_id, record);
    }

    /// Remove `instance_id`; returns false when it was not present. Removal
    /// leaves no residue: the next [`Self::build_indirect_batches`] emits
    /// neither the record nor its bytes.
    pub fn remove(&mut self, instance_id: u64) -> bool {
        self.instances.remove(&instance_id).is_some()
    }

    /// The live record for `instance_id`.
    pub fn get(&self, instance_id: u64) -> Option<&KineticInstanceGpu> {
        self.instances.get(&instance_id)
    }

    /// Number of live instances.
    pub fn len(&self) -> usize {
        self.instances.len()
    }

    pub fn is_empty(&self) -> bool {
        self.instances.is_empty()
    }

    /// Register the merged-geometry-buffer run for `mesh_kind`. Instances of
    /// a kind with no template are skipped entirely at batch time — they are
    /// neither uploaded nor drawn.
    pub fn set_mesh_template(
        &mut self,
        mesh_kind: u32,
        index_count: u32,
        first_index: u32,
        vertex_offset: i32,
    ) {
        self.templates.insert(
            mesh_kind,
            MeshKindTemplate {
                index_count,
                first_index,
                vertex_offset,
            },
        );
    }

    /// The registered template for `mesh_kind`, if any.
    pub fn mesh_template(&self, mesh_kind: u32) -> Option<MeshKindTemplate> {
        self.templates.get(&mesh_kind).copied()
    }

    /// Set the 6-DOF transform for `subworld_id` (0 = static grid).
    pub fn set_subworld_transform(&mut self, subworld_id: u32, transform: SubWorldTransformGpu) {
        self.subworlds.insert(subworld_id, transform);
    }

    /// Drop a sub-world transform; returns false when it was not registered.
    pub fn remove_subworld_transform(&mut self, subworld_id: u32) -> bool {
        self.subworlds.remove(&subworld_id).is_some()
    }

    /// The shader-side rotation angle: `phase + angular_velocity * t`.
    /// `reversed` flips the axis direction, not this scalar, so the formula
    /// matches the vertex shader term-for-term.
    pub fn eval_instance_angle(record: &KineticInstanceGpu, time_secs: f32) -> f32 {
        record.phase + record.angular_velocity * time_secs
    }

    /// Pack active instances into the deterministic frame stream — sorted
    /// by (`mesh_kind`, `instance_id`), skipping kinds with no template —
    /// and return it with each active kind's `(template, first_instance,
    /// count)` run. Shared by the SSBO/indirect and UBO batch paths so both
    /// emit identical instance bytes.
    fn pack_instance_runs(
        &self,
    ) -> (Vec<KineticInstanceGpu>, Vec<(u32, MeshKindTemplate, u32, u32)>) {
        let mut counts: BTreeMap<u32, u32> = BTreeMap::new();
        for record in self.instances.values() {
            if self.templates.contains_key(&record.mesh_kind) {
                *counts.entry(record.mesh_kind).or_default() += 1;
            }
        }
        let total: u32 = counts.values().sum();
        let mut instances = vec![KineticInstanceGpu::default(); total as usize];
        let mut runs = Vec::with_capacity(counts.len());
        let mut cursors: BTreeMap<u32, u32> = BTreeMap::new();
        let mut next = 0u32;
        for (&kind, &count) in &counts {
            runs.push((kind, self.templates[&kind], next, count));
            cursors.insert(kind, next);
            next += count;
        }
        for record in self.instances.values() {
            let Some(slot) = cursors.get_mut(&record.mesh_kind) else {
                continue;
            };
            instances[*slot as usize] = *record;
            *slot += 1;
        }
        (instances, runs)
    }

    /// Pack the frame's instancing upload. Deterministic: instances sort by
    /// (`mesh_kind`, `instance_id`), so two builds over identical state are
    /// byte-identical. One [`DrawIndexedIndirectCommand`] per active
    /// `mesh_kind` with a registered template; `subworlds` is a dense table
    /// from slot 0 through the highest registered id, identity-padded.
    pub fn build_indirect_batches(&self) -> KineticFrameUpload {
        let (instances, runs) = self.pack_instance_runs();
        let commands = runs
            .iter()
            .map(|&(_, template, first_instance, count)| DrawIndexedIndirectCommand {
                index_count: template.index_count,
                instance_count: count,
                first_index: template.first_index,
                vertex_offset: template.vertex_offset,
                first_instance,
            })
            .collect();

        let subworlds = match self.subworlds.keys().next_back() {
            Some(&max_id) => {
                let mut table = vec![SubWorldTransformGpu::IDENTITY; max_id as usize + 1];
                for (&id, &transform) in &self.subworlds {
                    table[id as usize] = transform;
                }
                table
            }
            None => Vec::new(),
        };

        KineticFrameUpload {
            instances,
            commands,
            subworlds,
        }
    }

    /// Split the frame's packed instance stream into uniform-buffer-sized
    /// draw batches for [`ActiveRenderBackend::ClassicLowSpec`]: every
    /// active `mesh_kind`'s run is chunked into groups of at most
    /// `max_instances_per_ubo` instances (`0` is clamped to 1), in
    /// ascending `mesh_kind` order. Pass [`MAX_INSTANCES_PER_UBO_16KB`] —
    /// or [`GpuMemoryProfile::max_instances_per_ubo`] — so each batch fits
    /// the 16 KiB UBO floor every GL 3.3 / Vulkan 1.1 device guarantees.
    /// Deterministic and lossless: concatenating every batch's
    /// `instances` reproduces the same byte stream
    /// [`Self::build_indirect_batches`] packs.
    pub fn build_ubo_batches(&self, max_instances_per_ubo: usize) -> Vec<KineticUboBatch> {
        let per_ubo = max_instances_per_ubo.max(1);
        let (instances, runs) = self.pack_instance_runs();
        let mut batches = Vec::new();
        for (kind, template, first, count) in runs {
            let start = first as usize;
            let end = (first + count) as usize;
            for (offset, chunk) in instances[start..end].chunks(per_ubo).enumerate() {
                batches.push(KineticUboBatch {
                    mesh_kind: kind,
                    index_count: template.index_count,
                    first_index: template.first_index,
                    vertex_offset: template.vertex_offset,
                    first_instance: first + (offset * per_ubo) as u32,
                    instances: chunk.to_vec(),
                });
            }
        }
        batches
    }
}

const _: () = {
    assert!(std::mem::size_of::<KineticInstanceGpu>() == 48);
    assert!(MAX_INSTANCES_PER_UBO_16KB * KINETIC_INSTANCE_GPU_BYTES as usize
        <= MIN_GUARANTEED_UBO_BYTES as usize);
    assert!(std::mem::align_of::<KineticInstanceGpu>() == 4);
    assert!(std::mem::size_of::<DrawIndexedIndirectCommand>() == 20);
    assert!(std::mem::size_of::<SubWorldTransformGpu>() == 64);
    assert!(std::mem::size_of::<MeshletDescriptor>() == 16);
};
