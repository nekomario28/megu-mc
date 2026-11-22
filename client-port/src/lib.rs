// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later

//! CPU section meshing for Minecraft Java 1.21.1 (protocol 767).
#![deny(unsafe_op_in_unsafe_fn)]

pub mod atlas;
pub mod ber_instancing;
pub mod builtin_shadow_wgsl;
mod cache;
pub mod classic_ores;
mod column;
mod cull;
mod culler;
pub mod draw_order;
pub mod ffi;
pub mod gpu_pipeline;
pub mod hiz_culler;
mod lod;
mod mesh;
pub mod model_baker;
mod packed;
mod region;
mod section;
pub mod shaderpack;
pub mod shaderpack_reflect;
pub mod upload_budget;
pub mod vanilla_plus_3d;
pub mod vfx_engine;
pub mod visibility;

#[cfg(test)]
mod tests;

pub use atlas::{
    apply_biome_tint, build_mipmaps, remap_baked_quad_uvs, remap_baked_quad_uvs_inset,
    AnimFrame, AnimationError, AtlasBuilder, AtlasError, AtlasMip, PackedAtlas,
    SpriteAnimation, SpritePlacement, DEFAULT_ATLAS_MAX_DIM, MAX_ANIMATION_FRAMES,
    MAX_ATLAS_DIM, MAX_SPRITE_PADDING,
};
pub use ber_instancing::{
    BerAtlasKey, BerCullStats, BerEntityInput, BerFrameBatches, BerFrameView, BerInstanceBatch,
    BerInstanceData, BerInteriorItem, BerLodPolicy, BerModelKey, BerMotionState, BerShellModel,
    BlockEntityInstanceBatcher, BER_INSTANCE_GPU_BYTES, DEFAULT_MAX_DYNAMIC_DIST,
    DEFAULT_NEAR_INTERIOR_DIST, LOW_SPEC_MAX_DYNAMIC_DIST, LOW_SPEC_NEAR_INTERIOR_DIST,
    LOD_SCALE_MAX, LOD_SCALE_MIN,
};
pub use cache::{MeshCache, SectionCoord, DEFAULT_RESORT_THRESHOLD};
pub use cull::{box_facing_mask, box_in_frustum, SectionVis};
pub use culler::{through_bit, CullResult, GraphCuller, SectionGraph, THROUGH_ALL};
pub use draw_order::{
    aabb_distance_sq, depth_key, DrawOrder, DrawOrderScratch, KeyedDraw, MultidrawScratch,
};
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
pub use hiz_culler::{HiZCullStats, HiZError, HiZPyramid, MAX_DEPTH_DIMENSION};
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
pub use shaderpack::{
    is_megu_wgsl_pack, load_megu_wgsl_pack, plan_shader_pipeline,
    plan_shader_pipeline_with_options, preprocess_shader_source, AlphaTest, BlendDirective,
    CustomImage, CustomTexture, GpuShaderCaps, IdMap, IdMatcher, IrisFeature, MeguWgslPack,
    MeguWgslPass, PackDimension, PackOrigin, PassKind, PipelineStage, PreprocessedShader, Profile,
    ShaderCompatibilityReport, ShaderKind, ShaderOption, ShaderOptionOverrides, ShaderPackError,
    ShaderPackFamily, ShaderPackLang, ShaderPackSource, ShaderPass, ShaderProperties,
    ShaderPipelinePlan, ShaderUniformRequirements, ShadowSettings, SsboSpec, MAX_INCLUDE_DEPTH,
    MAX_PACK_FILES, MAX_PACK_UNPACKED_BYTES, MAX_PROPERTIES_BYTES, MAX_SHADER_FILE_BYTES,
    MAX_TOTAL_INCLUDES, MAX_ZIP_FILE_BYTES, MEGU_WGSL_MANIFEST,
};
pub use upload_budget::{
    ChunkUploadBudget, DEFAULT_MAX_UPLOADS_PER_FRAME, DEFAULT_MAX_UPLOAD_BYTES_PER_FRAME,
    DEFAULT_MAX_UPLOAD_WORK_US_PER_FRAME, DEFAULT_MIN_UPLOADS_PER_FRAME, DEFAULT_TARGET_FRAME_US,
};
pub use visibility::FaceVisibility;

pub const PROTOCOL_VERSION: u32 = 767;
