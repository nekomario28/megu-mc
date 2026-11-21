// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later

//! CPU section meshing for Minecraft Java 1.21.1 (protocol 767).
#![deny(unsafe_op_in_unsafe_fn)]

mod cache;
mod column;
mod cull;
mod culler;
pub mod ffi;
pub mod gpu_pipeline;
mod lod;
mod mesh;
pub mod model_baker;
mod packed;
mod region;
mod section;
pub mod visibility;

#[cfg(test)]
mod tests;

pub use cache::{MeshCache, SectionCoord, DEFAULT_RESORT_THRESHOLD};
pub use cull::{box_facing_mask, box_in_frustum, SectionVis};
pub use culler::{through_bit, CullResult, GraphCuller, SectionGraph, THROUGH_ALL};
pub use gpu_pipeline::{
    cull_meshlets_cpu_reference, pack_axis_flags, pack_facing_and_flags,
    pack_section_meshlets, recommend_low_spec_budget, resolve_render_backend,
    ActiveRenderBackend, BackendResolution, BlockEntityBatchStats, BlockEntityFrameUpload,
    BlockEntityInstanceGpu, BlockEntityInstancePool, BlockEntityMeshKind, BlockEntityUboBatch,
    BlockEntityUboFrame, DrawIndexedIndirectCommand, GpuCapabilities, GpuMemoryProfile, GpuVendor,
    KineticFrameUpload, KineticInstanceGpu, KineticInstancePool, KineticUboBatch,
    LowSpecRenderBudget, MeshKindTemplate, MeshletDescriptor, RenderBackendPreference,
    SectionMeshlets, SubWorldTransformGpu, BLOCK_ENTITY_DYE_MASK, BLOCK_ENTITY_DYE_SHIFT,
    BLOCK_ENTITY_FACING_MASK, BLOCK_ENTITY_FLAG_GLOWING, BLOCK_ENTITY_FLAG_WALL_MOUNTED,
    BLOCK_ENTITY_INSTANCE_GPU_BYTES, BLOCK_ENTITY_LOD_FLAG_NO_CULL,
    BLOCK_ENTITY_MESH_KIND_CUSTOM_BASE, KINETIC_AXIS_MASK, KINETIC_FLAG_OVERSTRESSED,
    KINETIC_FLAG_REVERSED, KINETIC_INSTANCE_GPU_BYTES, KINETIC_SUBWORLD_MAX,
    KINETIC_SUBWORLD_SHIFT, LOW_SPEC_VRAM_CEILING_MIB, MAX_INSTANCES_PER_UBO_16KB,
    MAX_QUADS_PER_MESHLET, MIN_GUARANTEED_UBO_BYTES, REQUIRED_MESH_OUTPUT_PRIMITIVES,
    REQUIRED_MESH_OUTPUT_VERTICES,
};
pub use lod::{summarize_column, LodCell, LodSummary, LOD_COARSE, LOD_FINE};
pub use mesh::{
    bucket_draw_mask, mesh_section, DirectionBuckets, Layer, Quad, DIRECTION_BUCKETS,
    DIRECTION_BUCKET_UNASSIGNED, QUAD_FALLING, QUAD_TINTED,
};
pub use model_baker::{
    bake_model, resolve_item_override, Axis, BakedModel, BakedModelQuad, ElementFace,
    ElementRotation, ItemModelOverride, JsonError, ModelElement, ModelError, RawModelJson,
    ResourcePackModelStack, MAX_ELEMENTS_PER_MODEL, MAX_JSON_DEPTH, MAX_MODEL_COORD,
    MAX_PARENT_DEPTH, MIN_MODEL_COORD, MODEL_UNITS_PER_BLOCK,
};
pub use region::{
    DrawElementsCommand, MultiDrawCommand, RegionStats, RenderRegion, RenderRegions, SectionSlab,
    REGION_SECTIONS, REGION_X, REGION_Y, REGION_Z,
};
pub use section::*;
pub use visibility::FaceVisibility;

pub const PROTOCOL_VERSION: u32 = 767;
