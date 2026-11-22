// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! GPU compute Hi-Z pyramid + GPU section culling — clean-room pure-`wgpu`
//! implementations per `audit/ram-vram-role-separation-and-gpu-hiz-design`.
//!
//! * [`HIZ_BUILD_WGSL`] builds an `R32Float` max-depth mip pyramid from a
//!   depth buffer (`cs_depth_to_mip0`) or any `R32Float` source
//!   (`cs_r32_to_mip0`), then max-downsamples level by level
//!   (`cs_downsample`).
//! * [`SECTION_CULL_WGSL`] runs six-plane frustum culling, direction-bucket
//!   culling (same bit layout as `client-port`'s `bucket_draw_mask`) and a
//!   conservative Hi-Z occlusion test per section, emitting
//!   `DrawIndexedIndirectArgs` (plus a per-section bucket mask and cull
//!   counters) so the CPU only ever sees how many draws survived.
//!
//! The occlusion test is conservative by construction: a section is only
//! culled when every corner projects in front of the camera, its screen
//! rect is covered by pyramid texels, and its *nearest* depth is strictly
//! farther than the farthest occluder depth recorded there — so the pass
//! produces zero false negatives (a visible section is never culled).

use bytemuck::{Pod, Zeroable};

/// Hi-Z pyramid builder WGSL: depth-buffer mip0, `R32Float` mip0 copy and
/// 2x2 max downsample entry points.
pub const HIZ_BUILD_WGSL: &str = r#"
// Group 0: binding 0 = source (depth or r32 float), binding 1 = r32float
// storage destination. One bind group is created per dispatch by the host.

@group(0) @binding(0) var depth_src: texture_depth_2d;
@group(0) @binding(1) var mip0_out: texture_storage_2d<r32float, write>;

@compute @workgroup_size(8, 8)
fn cs_depth_to_mip0(@builtin(global_invocation_id) id: vec3<u32>) {
    let dst_dims = vec2<i32>(textureDimensions(mip0_out));
    if (i32(id.x) >= dst_dims.x || i32(id.y) >= dst_dims.y) {
        return;
    }
    let d = textureLoad(depth_src, vec2<i32>(id.xy), 0);
    textureStore(mip0_out, vec2<i32>(id.xy), vec4<f32>(d, 0.0, 0.0, 1.0));
}
"#;

/// `R32Float` source variants of the Hi-Z builder: `cs_r32_to_mip0`
/// copies a plain `R32Float` texture into mip 0, `cs_downsample` writes
/// each level as the 2x2 max of the previous level (edges clamp so odd
/// dimensions stay correct). Separate module so the float/depth binding
/// types never have to unify in one bind group.
pub const HIZ_DOWNSAMPLE_WGSL: &str = r#"
@group(0) @binding(0) var src_level: texture_2d<f32>;
@group(0) @binding(1) var dst_level: texture_storage_2d<r32float, write>;

@compute @workgroup_size(8, 8)
fn cs_r32_to_mip0(@builtin(global_invocation_id) id: vec3<u32>) {
    let dst_dims = vec2<i32>(textureDimensions(dst_level));
    if (i32(id.x) >= dst_dims.x || i32(id.y) >= dst_dims.y) {
        return;
    }
    let d = textureLoad(src_level, vec2<i32>(id.xy), 0).x;
    textureStore(dst_level, vec2<i32>(id.xy), vec4<f32>(d, 0.0, 0.0, 1.0));
}

@compute @workgroup_size(8, 8)
fn cs_downsample(@builtin(global_invocation_id) id: vec3<u32>) {
    let dst_dims = vec2<i32>(textureDimensions(dst_level));
    if (i32(id.x) >= dst_dims.x || i32(id.y) >= dst_dims.y) {
        return;
    }
    let src_dims = vec2<i32>(textureDimensions(src_level));
    let base = vec2<i32>(id.xy) * 2;
    let x1 = min(base.x + 1, src_dims.x - 1);
    let y1 = min(base.y + 1, src_dims.y - 1);
    var m = textureLoad(src_level, vec2<i32>(base.x, base.y), 0).x;
    m = max(m, textureLoad(src_level, vec2<i32>(x1, base.y), 0).x);
    m = max(m, textureLoad(src_level, vec2<i32>(base.x, y1), 0).x);
    m = max(m, textureLoad(src_level, vec2<i32>(x1, y1), 0).x);
    textureStore(dst_level, vec2<i32>(id.xy), vec4<f32>(m, 0.0, 0.0, 1.0));
}
"#;

/// Section culling WGSL: one thread per section runs the six-plane
/// frustum test, the direction-bucket draw mask (same bit order as
/// `client-port`: Down/Up/North/South/West/East bits 0..5, unassigned bit
/// 6) and the conservative Hi-Z occlusion test, then writes
/// `DrawIndexedIndirectArgs` with `instance_count` 1 (keep) or 0 (culled)
/// plus the kept bucket mask and cull-reason counters.
pub const SECTION_CULL_WGSL: &str = r#"
struct CullUniforms {
    view_proj: mat4x4<f32>,
    planes: array<vec4<f32>, 6>,
    camera_pos: vec4<f32>,
    // x = hiz width, y = hiz height, z = mip count, w = section count
    params: vec4<f32>,
};

struct SectionGpuRecord {
    aabb_min: vec4<f32>,
    aabb_max: vec4<f32>,
    index_count: u32,
    first_index: u32,
    base_vertex: i32,
    first_instance: u32,
    _pad: vec4<u32>,
};

struct IndirectArgs {
    index_count: u32,
    instance_count: u32,
    first_index: u32,
    base_vertex: i32,
    first_instance: u32,
};

struct CullStats {
    visible_count: atomic<u32>,
    frustum_culled: atomic<u32>,
    hiz_culled: atomic<u32>,
    _pad: u32,
};

@group(0) @binding(0) var<uniform> u: CullUniforms;
@group(0) @binding(1) var<storage, read> sections: array<SectionGpuRecord>;
@group(0) @binding(2) var<storage, read_write> out_args: array<IndirectArgs>;
@group(0) @binding(3) var<storage, read_write> out_stats: CullStats;
@group(0) @binding(4) var<storage, read_write> out_masks: array<u32>;
@group(0) @binding(5) var hiz: texture_2d<f32>;

// Positive-vertex six-plane test: `false` only when the box lies fully
// outside at least one plane (`dot(n, p) + d < 0` for the corner farthest
// along the plane normal).
fn aabb_in_frustum(lo: vec3<f32>, hi: vec3<f32>) -> bool {
    for (var i = 0u; i < 6u; i = i + 1u) {
        let p = u.planes[i];
        let pos = vec3<f32>(
            select(lo.x, hi.x, p.x >= 0.0),
            select(lo.y, hi.y, p.y >= 0.0),
            select(lo.z, hi.z, p.z >= 0.0));
        if (dot(p.xyz, pos) + p.w < 0.0) {
            return false;
        }
    }
    return true;
}

// Mirrors `section_draw_mask` (client-gpu renderer): Direction bit order
// Down=1, Up=2, North=4, South=8, West=16, East=32, unassigned=64; a
// camera inside a section plane on an axis draws both directions.
fn section_draw_mask(min_b: vec3<f32>, cam: vec3<f32>) -> u32 {
    var facing = 0u;
    if (cam.x < min_b.x) {
        facing = facing | 16u;
    } else if (cam.x > min_b.x + 16.0) {
        facing = facing | 32u;
    }
    if (cam.y < min_b.y) {
        facing = facing | 1u;
    } else if (cam.y > min_b.y + 16.0) {
        facing = facing | 2u;
    }
    if (cam.z < min_b.z) {
        facing = facing | 4u;
    } else if (cam.z > min_b.z + 16.0) {
        facing = facing | 8u;
    }
    var draw = select(facing | 64u, 127u, facing == 0u);
    if (cam.x >= min_b.x && cam.x <= min_b.x + 16.0) {
        draw = draw | 48u;
    }
    if (cam.y >= min_b.y && cam.y <= min_b.y + 16.0) {
        draw = draw | 3u;
    }
    if (cam.z >= min_b.z && cam.z <= min_b.z + 16.0) {
        draw = draw | 12u;
    }
    return draw;
}

// Conservative Hi-Z occlusion: `true` only when every corner projects in
// front of the camera, the screen rect is covered by pyramid texels, and
// the section's *nearest* depth is strictly farther than the farthest
// occluder depth in that region. Any uncertainty (near-plane clipping,
// off-screen rect, empty pyramid texels reading 1.0) keeps the section.
fn hiz_occluded(lo: vec3<f32>, hi: vec3<f32>) -> bool {
    var ndc_min = vec3<f32>(1e30, 1e30, 1e30);
    var ndc_max = vec3<f32>(-1e30, -1e30, -1e30);
    for (var i = 0u; i < 8u; i = i + 1u) {
        let corner = vec4<f32>(
            select(lo.x, hi.x, (i & 1u) != 0u),
            select(lo.y, hi.y, (i & 2u) != 0u),
            select(lo.z, hi.z, (i & 4u) != 0u),
            1.0);
        let clip = u.view_proj * corner;
        if (clip.w <= 0.0) {
            // Corner behind the camera: occlusion unprovable, keep it.
            return false;
        }
        let ndc = clip.xyz / clip.w;
        ndc_min = min(ndc_min, ndc);
        ndc_max = max(ndc_max, ndc);
    }
    if (ndc_max.x < -1.0 || ndc_min.x > 1.0 || ndc_max.y < -1.0 || ndc_min.y > 1.0) {
        // Fully off-screen rect: the frustum test owns that case.
        return false;
    }
    // NDC (+y up) -> texel uv (+y down) of the projected AABB.
    let uv_min = clamp(ndc_min.xy * vec2<f32>(0.5, -0.5) + vec2<f32>(0.5, 0.5),
                       vec2<f32>(0.0, 0.0), vec2<f32>(1.0, 1.0));
    let uv_max = clamp(ndc_max.xy * vec2<f32>(0.5, -0.5) + vec2<f32>(0.5, 0.5),
                       vec2<f32>(0.0, 0.0), vec2<f32>(1.0, 1.0));
    let hiz_w = u.params.x;
    let hiz_h = u.params.y;
    let mip_count = u32(u.params.z);
    let px_min = uv_min * vec2<f32>(hiz_w, hiz_h);
    let px_max = uv_max * vec2<f32>(hiz_w, hiz_h);
    let span = max(max(px_max.x - px_min.x, 1.0), max(px_max.y - px_min.y, 1.0));
    // Pick the mip where the rect fits inside ~2x2 texels, clamped to the
    // built pyramid — coarser texels are max-depth, so this stays
    // conservative at every size.
    let mip = min(u32(ceil(log2(span))) - 1u, mip_count - 1u);
    let scale = exp2(f32(mip));
    let level_dims = vec2<i32>(textureDimensions(hiz, i32(mip)));
    let x0 = clamp(i32(px_min.x / scale), 0, level_dims.x - 1);
    let y0 = clamp(i32(px_min.y / scale), 0, level_dims.y - 1);
    let x1 = clamp(i32(px_max.x / scale), 0, level_dims.x - 1);
    let y1 = clamp(i32(px_max.y / scale), 0, level_dims.y - 1);
    var occluder = textureLoad(hiz, vec2<i32>(x0, y0), i32(mip)).x;
    occluder = max(occluder, textureLoad(hiz, vec2<i32>(x1, y0), i32(mip)).x);
    occluder = max(occluder, textureLoad(hiz, vec2<i32>(x0, y1), i32(mip)).x);
    occluder = max(occluder, textureLoad(hiz, vec2<i32>(x1, y1), i32(mip)).x);
    // Depth convention is forward-z [0, 1] (far = 1.0): a section is
    // occluded only when its nearest corner sits strictly behind the
    // farthest depth recorded by the pyramid. `>` (not `>=`) keeps
    // coplanar geometry — zero false negatives.
    return ndc_min.z > occluder;
}

@compute @workgroup_size(64)
fn cs_cull(@builtin(global_invocation_id) id: vec3<u32>) {
    let index = id.x;
    if (index >= u32(u.params.w)) {
        return;
    }
    let section = sections[index];
    let lo = section.aabb_min.xyz;
    let hi = section.aabb_max.xyz;
    let mask = section_draw_mask(lo, u.camera_pos.xyz);
    out_masks[index] = mask;
    var instance_count = 0u;
    if (!aabb_in_frustum(lo, hi)) {
        atomicAdd(&out_stats.frustum_culled, 1u);
    } else if (hiz_occluded(lo, hi)) {
        atomicAdd(&out_stats.hiz_culled, 1u);
    } else {
        instance_count = 1u;
        atomicAdd(&out_stats.visible_count, 1u);
    }
    out_args[index] = IndirectArgs(
        section.index_count,
        instance_count,
        section.first_index,
        section.base_vertex,
        section.first_instance);
}
"#;

/// POD mirror of the WGSL `CullUniforms` (192 bytes).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Pod, Zeroable)]
pub struct CullUniforms {
    /// `projection * view`, column-major — same layout as [`crate::Mat4`].
    pub view_proj: [[f32; 4]; 4],
    /// Six frustum planes, inside half-space `dot(n, p) + d >= 0`.
    pub planes: [[f32; 4]; 6],
    /// Camera world position (w unused).
    pub camera_pos: [f32; 4],
    /// `hiz width, hiz height, mip count, section count`.
    pub params: [f32; 4],
}

/// POD mirror of the WGSL `SectionGpuRecord` (64 bytes): one culled
/// section's AABB plus the `DrawIndexedIndirectArgs` it writes through.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Pod, Zeroable)]
pub struct SectionGpuRecord {
    /// AABB minimum (w unused).
    pub aabb_min: [f32; 4],
    /// AABB maximum (w unused).
    pub aabb_max: [f32; 4],
    /// Index count of the section's canonical shared-index draw.
    pub index_count: u32,
    /// First index into the shared quad index buffer.
    pub first_index: u32,
    /// Vertex bias for the draw.
    pub base_vertex: i32,
    /// First instance for the draw.
    pub first_instance: u32,
    /// Reserved.
    pub _pad: [u32; 4],
}

/// POD mirror of the WGSL `IndirectArgs` — `wgpu`'s
/// `DrawIndexedIndirectArgs` layout (20 bytes).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Pod, Zeroable, PartialEq, Eq)]
pub struct IndirectArgs {
    /// Indices per surviving draw.
    pub index_count: u32,
    /// `1` when the section survived every cull, `0` when culled.
    pub instance_count: u32,
    /// First index of the draw.
    pub first_index: u32,
    /// Vertex bias of the draw.
    pub base_vertex: i32,
    /// First instance of the draw.
    pub first_instance: u32,
}

/// POD mirror of the WGSL `CullStats` (16 bytes).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Pod, Zeroable)]
pub struct CullStats {
    /// Sections that survived all three culls.
    pub visible_count: u32,
    /// Sections rejected by the six-plane frustum test.
    pub frustum_culled: u32,
    /// Sections rejected by the conservative Hi-Z occlusion test.
    pub hiz_culled: u32,
    /// Reserved.
    pub _pad: u32,
}

/// Whole-pyramid + per-level views, compute pipelines and telemetry for
/// GPU Hi-Z build + section cull dispatches.
pub struct GpuHizCuller {
    /// `R32Float` mip pyramid texture (max depth per texel).
    pyramid: wgpu::Texture,
    /// View over every mip level (the cull shader's sampled texture).
    pyramid_view: wgpu::TextureView,
    /// Per-mip views used as storage write and sampled-read targets.
    mip_views: Vec<wgpu::TextureView>,
    mip_count: u32,
    width: u32,
    height: u32,
    depth_src_layout: wgpu::BindGroupLayout,
    float_src_layout: wgpu::BindGroupLayout,
    cull_layout: wgpu::BindGroupLayout,
    depth_to_mip0: wgpu::ComputePipeline,
    float_to_mip0: wgpu::ComputePipeline,
    downsample: wgpu::ComputePipeline,
    cull: wgpu::ComputePipeline,
    /// `true` once any `build_*` method has written the pyramid — the
    /// renderer's cull path refuses to depth-cull an unbuilt (zeroed =
    /// nearest-depth) pyramid so it can never produce false culls.
    pyramid_ready: bool,
    /// Hi-Z build dispatches issued (mip0 + every downsample step).
    pub hiz_build_dispatches: u64,
    /// Section-cull dispatches issued.
    pub cull_dispatches: u64,
}

impl GpuHizCuller {
    /// Allocate the `width`x`height` `R32Float` pyramid and compile both
    /// compute pipelines. `width`/`height` should match the depth source
    /// for mip 0 to be a faithful copy.
    pub fn new(device: &wgpu::Device, width: u32, height: u32) -> Self {
        let max_dim = width.max(height).max(1);
        let mip_count = 32 - max_dim.leading_zeros();
        let pyramid = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("gpu-hiz-pyramid"),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: mip_count,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::R32Float,
            usage: wgpu::TextureUsages::TEXTURE_BINDING
                | wgpu::TextureUsages::STORAGE_BINDING
                | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let pyramid_view = pyramid.create_view(&wgpu::TextureViewDescriptor::default());
        let mip_views: Vec<wgpu::TextureView> = (0..mip_count)
            .map(|level| {
                pyramid.create_view(&wgpu::TextureViewDescriptor {
                    label: Some("gpu-hiz-pyramid-level"),
                    base_mip_level: level,
                    mip_level_count: Some(1),
                    ..wgpu::TextureViewDescriptor::default()
                })
            })
            .collect();
        let depth_src_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("gpu-hiz-depth-src"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::COMPUTE,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Depth,
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::COMPUTE,
                    ty: wgpu::BindingType::StorageTexture {
                        access: wgpu::StorageTextureAccess::WriteOnly,
                        format: wgpu::TextureFormat::R32Float,
                        view_dimension: wgpu::TextureViewDimension::D2,
                    },
                    count: None,
                },
            ],
        });
        let float_src_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("gpu-hiz-float-src"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::COMPUTE,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: false },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::COMPUTE,
                    ty: wgpu::BindingType::StorageTexture {
                        access: wgpu::StorageTextureAccess::WriteOnly,
                        format: wgpu::TextureFormat::R32Float,
                        view_dimension: wgpu::TextureViewDimension::D2,
                    },
                    count: None,
                },
            ],
        });
        let cull_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("gpu-section-cull"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::COMPUTE,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::COMPUTE,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Storage { read_only: true },
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::COMPUTE,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Storage { read_only: false },
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 3,
                    visibility: wgpu::ShaderStages::COMPUTE,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Storage { read_only: false },
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 4,
                    visibility: wgpu::ShaderStages::COMPUTE,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Storage { read_only: false },
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 5,
                    visibility: wgpu::ShaderStages::COMPUTE,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: false },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
            ],
        });
        let build_module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("gpu-hiz-build"),
            source: wgpu::ShaderSource::Wgsl(HIZ_BUILD_WGSL.into()),
        });
        let downsample_module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("gpu-hiz-downsample"),
            source: wgpu::ShaderSource::Wgsl(HIZ_DOWNSAMPLE_WGSL.into()),
        });
        let cull_module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("gpu-section-cull"),
            source: wgpu::ShaderSource::Wgsl(SECTION_CULL_WGSL.into()),
        });
        let pipeline = |layout: &wgpu::BindGroupLayout, module: &wgpu::ShaderModule, ep: &str| {
            let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("gpu-hiz-pipeline-layout"),
                bind_group_layouts: &[Some(layout)],
                immediate_size: 0,
            });
            device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
                label: Some("gpu-hiz-pipeline"),
                layout: Some(&pipeline_layout),
                module,
                entry_point: Some(ep),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                cache: None,
            })
        };
        let depth_to_mip0 = pipeline(&depth_src_layout, &build_module, "cs_depth_to_mip0");
        let float_to_mip0 = pipeline(&float_src_layout, &downsample_module, "cs_r32_to_mip0");
        let downsample = pipeline(&float_src_layout, &downsample_module, "cs_downsample");
        let cull = pipeline(&cull_layout, &cull_module, "cs_cull");
        Self {
            pyramid,
            pyramid_view,
            mip_views,
            mip_count,
            width,
            height,
            depth_src_layout,
            float_src_layout,
            cull_layout,
            depth_to_mip0,
            float_to_mip0,
            downsample,
            cull,
            pyramid_ready: false,
            hiz_build_dispatches: 0,
            cull_dispatches: 0,
        }
    }

    /// Pyramid level-0 `(width, height)` — the resolution the culler was
    /// built for.
    pub fn resolution(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// Whether the pyramid holds real depth data. Callers must not treat
    /// occlusion results from an unbuilt pyramid as valid — zero-initialised
    /// `R32Float` reads as "everything is occluded".
    pub fn pyramid_ready(&self) -> bool {
        self.pyramid_ready
    }

    /// Pyramid mip count.
    pub fn mip_count(&self) -> u32 {
        self.mip_count
    }

    /// The `R32Float` pyramid texture (readback/inspection).
    pub fn pyramid_texture(&self) -> &wgpu::Texture {
        &self.pyramid
    }

    /// All-mips sampled view (the cull shader's `hiz` binding).
    pub fn pyramid_view(&self) -> &wgpu::TextureView {
        &self.pyramid_view
    }

    /// Record the downsample chain `mip -> mip+1` up to the last level.
    fn encode_downsample_chain(
        &mut self,
        device: &wgpu::Device,
        encoder: &mut wgpu::CommandEncoder,
    ) {
        for level in 1..self.mip_count {
            let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("gpu-hiz-downsample-bg"),
                layout: &self.float_src_layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: wgpu::BindingResource::TextureView(
                            &self.mip_views[level as usize - 1],
                        ),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::TextureView(
                            &self.mip_views[level as usize],
                        ),
                    },
                ],
            });
            let dims = self.mip_dims(level);
            let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
                label: Some("gpu-hiz-downsample"),
                timestamp_writes: None,
            });
            pass.set_pipeline(&self.downsample);
            pass.set_bind_group(0, &bind_group, &[]);
            pass.dispatch_workgroups(dims.0.div_ceil(8), dims.1.div_ceil(8), 1);
            self.hiz_build_dispatches += 1;
        }
    }

    /// `(width, height)` of pyramid `level` (level 0 is full resolution).
    fn mip_dims(&self, level: u32) -> (u32, u32) {
        ((self.width >> level).max(1), (self.height >> level).max(1))
    }

    /// Build the pyramid from a `Depth32Float`/`texture_depth_2d` view:
    /// mip 0 copies the depth buffer 1:1, then each level is the 2x2 max
    /// of the previous one.
    pub fn build_from_depth(
        &mut self,
        device: &wgpu::Device,
        encoder: &mut wgpu::CommandEncoder,
        depth_view: &wgpu::TextureView,
    ) {
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("gpu-hiz-mip0-depth"),
            layout: &self.depth_src_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(depth_view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(&self.mip_views[0]),
                },
            ],
        });
        let dims = self.mip_dims(0);
        {
            let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
                label: Some("gpu-hiz-mip0"),
                timestamp_writes: None,
            });
            pass.set_pipeline(&self.depth_to_mip0);
            pass.set_bind_group(0, &bind_group, &[]);
            pass.dispatch_workgroups(dims.0.div_ceil(8), dims.1.div_ceil(8), 1);
        }
        self.hiz_build_dispatches += 1;
        self.pyramid_ready = true;
        self.encode_downsample_chain(device, encoder);
    }

    /// Build the pyramid from any `R32Float` source view (used by tests
    /// and by readback-driven rebuilds): mip 0 copies the source 1:1,
    /// then each level is the 2x2 max of the previous one.
    pub fn build_from_r32(
        &mut self,
        device: &wgpu::Device,
        encoder: &mut wgpu::CommandEncoder,
        src_view: &wgpu::TextureView,
    ) {
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("gpu-hiz-mip0-float"),
            layout: &self.float_src_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(src_view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(&self.mip_views[0]),
                },
            ],
        });
        let dims = self.mip_dims(0);
        {
            let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
                label: Some("gpu-hiz-mip0"),
                timestamp_writes: None,
            });
            pass.set_pipeline(&self.float_to_mip0);
            pass.set_bind_group(0, &bind_group, &[]);
            pass.dispatch_workgroups(dims.0.div_ceil(8), dims.1.div_ceil(8), 1);
        }
        self.hiz_build_dispatches += 1;
        self.pyramid_ready = true;
        self.encode_downsample_chain(device, encoder);
    }

    /// Dispatch the section-cull compute pass: `count` sections in
    /// `sections` are frustum/bucket/Hi-Z culled into `args`
    /// (`IndirectArgs` per section), `stats` (survival/cull counters) and
    /// `masks` (per-section kept-bucket bits). All buffers must be
    /// STORAGE-capable; `uniforms` carries the camera + pyramid params.
    #[allow(clippy::too_many_arguments)]
    pub fn encode_cull(
        &mut self,
        device: &wgpu::Device,
        encoder: &mut wgpu::CommandEncoder,
        uniforms: &wgpu::Buffer,
        sections: &wgpu::Buffer,
        args: &wgpu::Buffer,
        stats: &wgpu::Buffer,
        masks: &wgpu::Buffer,
        count: u32,
    ) {
        if count == 0 {
            return;
        }
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("gpu-section-cull-bg"),
            layout: &self.cull_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: uniforms.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: sections.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: args.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 3,
                    resource: stats.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 4,
                    resource: masks.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 5,
                    resource: wgpu::BindingResource::TextureView(&self.pyramid_view),
                },
            ],
        });
        {
            let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
                label: Some("gpu-section-cull"),
                timestamp_writes: None,
            });
            pass.set_pipeline(&self.cull);
            pass.set_bind_group(0, &bind_group, &[]);
            pass.dispatch_workgroups(count.div_ceil(64), 1, 1);
        }
        self.cull_dispatches += 1;
    }
}
