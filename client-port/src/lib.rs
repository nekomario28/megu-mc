// SPDX-License-Identifier: LGPL-3.0-or-later

//! CPU section meshing for Minecraft Java 1.21.1 (protocol 767).
//!
//! - [`perceptual_gpu_offload`]: human-perception priority scheduling
//!   (60° frontal cone boost, distance-scaled entity sync with client-side
//!   interpolation) and compact 16-byte GPU instance packing.
#![deny(unsafe_op_in_unsafe_fn)]

pub mod asset_pipeline;
pub mod atlas;
pub mod ber_instancing;
pub mod builtin_shadow_wgsl;
mod cache;
pub mod classic_ores;
mod column;
pub mod compact_vertex;
pub mod crop_archetype;
mod cull;
mod culler;
pub mod dir_basis;
pub mod draw_order;
pub mod dynamic_light_resolver;
pub mod entity_rig;
pub mod ffi;
pub mod gpu_pipeline;
pub mod gpu_residency;
pub mod inflate;
pub mod hiz_culler;
mod lod;
mod mesh;
pub mod mesher;
pub mod model_baker;
mod packed;
pub mod perceptual_gpu_offload;
pub mod png_decode;
mod region;
mod section;
pub mod shaderpack;
pub mod shaderpack_reflect;
pub mod shape_templates;
pub mod space_sky_bridge;
pub mod tactile_effects;
pub mod upload_budget;
pub mod vanilla_plus_3d;
pub mod vanilla_plus_nature;
pub mod vanilla_plus_visuals;
pub mod vanilla_block_models;
pub mod vfx_engine;
pub mod visibility;
pub mod voxel_dda_shadows;
pub mod world;

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
pub use cache::{
    build_and_pack_section_mesh, build_and_pack_section_mesh_with_baked_models,
    build_section_mesh_with_unpacked, BuiltSectionMesh, MeshCache, ReleasedMeshMeta, SectionCoord,
    SectionMeshInput, DEFAULT_RESORT_THRESHOLD, PRIORITY_ZERO_NEAR_RADIUS_BLOCKS,
};
pub use compact_vertex::{
    pack_vertices_compact, try_pack_vertices_compact, CompactVoxelVertex,
    COMPACT_VERTEX_MAX_EDGE, COMPACT_VERTEX_MAX_STATE_ID, COMPACT_VERTEX_MAX_TINT,
    COMPACT_VERTEX_MAX_Y_FIXED, COMPACT_VERTEX_WGSL_UNPACK,
};
pub use cull::{box_facing_mask, box_in_frustum, SectionVis};
pub use culler::{through_bit, CullResult, GraphCuller, SectionGraph, THROUGH_ALL};
pub use dir_basis::{CubeOrientation, DirBasis, DIR_BASIS};
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
pub use gpu_residency::{
    GpuResidencyManager, ResidencyBudget, ResidencyFramePlan, ResidencyStats,
    SectionResidencyState,
};
pub use ffi::PackedVertex;
pub use hiz_culler::{HiZCullStats, HiZError, HiZPyramid, MAX_DEPTH_DIMENSION};
pub use lod::{summarize_column, LodCell, LodSummary, LOD_COARSE, LOD_FINE};
pub use mesh::{
    bucket_draw_mask, mesh_section, mesh_section_with_baked_models, BakedTerrainFace,
    DirectionBuckets, Layer, Quad, DIRECTION_BUCKETS, DIRECTION_BUCKET_UNASSIGNED, QUAD_FALLING,
    QUAD_TINTED,
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
pub use shape_templates::{
    MaterialBinding, MaterialSlotBinding, ShapeArchetype, ShapeTemplate, ShapeTemplateRegistry,
    StairShape, TemplateQuad, TemplateSavingsReport, WallSide, InstantiatedQuad,
};
pub use upload_budget::{
    ChunkUploadBudget, UploadBudget, UploadBudgetProfile, DEFAULT_MAX_UPLOADS_PER_FRAME,
    DEFAULT_MAX_UPLOAD_BYTES_PER_FRAME, DEFAULT_MAX_UPLOAD_WORK_US_PER_FRAME,
    DEFAULT_MIN_UPLOADS_PER_FRAME, DEFAULT_TARGET_FRAME_US,
    HARD_FLOOR_1000FPS_MAX_UPLOADS_PER_FRAME, HARD_FLOOR_1000FPS_MAX_UPLOAD_BYTES_PER_FRAME,
    HARD_FLOOR_1000FPS_MAX_UPLOAD_WORK_US_PER_FRAME, HARD_FLOOR_1000FPS_TARGET_FRAME_US,
};
pub use visibility::{direction_face_dir, face_dir_direction, FaceVisibility};
pub use voxel_dda_shadows::{
    voxel_shadow_factor, voxel_shadow_factor_world, VoxelOccupancyGrid, VoxelShadowParams,
    DEFAULT_LIGHT_RADIUS, MAX_DDA_STEPS, VOXEL_DDA_SHADOW_WGSL, VOXEL_GRID_BYTES, VOXEL_GRID_CELLS,
    VOXEL_GRID_DIM, VOXEL_GRID_WORDS,
};
pub use world::WorldState;

/// Wire protocol version for the simulated target release (1.21.1 = 767),
/// sourced from the canonical [`megu_core::version`] registry.
pub const PROTOCOL_VERSION: u32 = megu_core::version::TargetVersion::V1_21_1.protocol() as u32;
