// SPDX-License-Identifier: LGPL-3.0-or-later
//! Clean-room WGSL shader suite for the Vulkan render paths, plus a static
//! structural validator.
//!
//! Every module embeds its source as a `&'static str` so the software and
//! hardware frontends share one copy of the shader text; nothing here links
//! a GPU API — the strings are data until a Vulkan backend compiles them.
//!
//! Struct layouts mirror the `#[repr(C)]` records in `megu_client_mesh`
//! field-for-field so a single CPU-side writer feeds both paths:
//!
//! * `PackedVertex` — four u32 words (16 B) bound as vertex attributes
//!   `@location(0..3)`; bit layout matches `client-port/src/packed.rs` and
//!   the software decode in `client-app/src/render.rs`.
//! * `MeshletRecord` — the 16 B `MeshletDescriptor` re-encoded as two
//!   little-endian u32 bound words plus `quad_offset`/`quad_count`.
//! * `BlockEntityInstance` / `KineticInstance` — the 48 B
//!   `BlockEntityInstanceGpu` / `KineticInstanceGpu` strides so both pools
//!   share the 16 KiB UBO budget on `ClassicLowSpec` hardware.
//! * `SubWorldTransform` — the 64 B column-major `SubWorldTransformGpu`.

// ---------------------------------------------------------------------------
// Terrain shaders
// ---------------------------------------------------------------------------

/// Opaque terrain: unpacks the 16-byte `PackedVertex` (`position` =
/// `x | z<<5 | (y*1024)<<10`, `attributes` = direction | AO | sky/block
/// light | layer | fluid | diagonal | quad extents | corner), shades with
/// face-normal x AO x max(sky, block) lightmap, applies biome tint, and
/// blends toward `fog_color` over the linear `fog_start..fog_end` ramp —
/// the same math `decode_vertex` + the software rasterizer run.
pub const TERRAIN_OPAQUE_WGSL: &str = "\
// Opaque terrain: packed-vertex unpack + AO/lightmap + biome tint + fog.
struct FrameUniforms {
    view_proj: mat4x4<f32>,
    camera_pos: vec3<f32>,
    fog_start: f32,
    fog_color: vec3<f32>,
    fog_end: f32,
    _pad: vec2<f32>,
};

struct SectionUniforms {
    // World origin of the 16^3 section this draw covers.
    origin: vec3<f32>,
    _pad0: f32,
    // Biome tint multiplier for untinted (palette-fallback) quads.
    biome_tint: vec3<f32>,
    biome_tint_strength: f32,
};

@group(0) @binding(0) var<uniform> frame: FrameUniforms;
@group(0) @binding(1) var<uniform> section: SectionUniforms;

// PackedVertex: four little-endian u32 words, 16-byte stride.
struct TerrainVertex {
    @location(0) packed_pos: u32,
    @location(1) state_id: u32,
    @location(2) attributes: u32,
    @location(3) tint: u32,
};

const FACE_SHADE = array<f32, 6>(0.45, 1.0, 0.75, 0.85, 0.7, 0.95);
const AO_SHADE = array<f32, 4>(0.4, 0.6, 0.8, 1.0);

fn hsv_to_rgb(h: f32, s: f32, v: f32) -> vec3<f32> {
    let i = floor(h * 6.0);
    let f = h * 6.0 - i;
    let p = v * (1.0 - s);
    let q = v * (1.0 - f * s);
    let t = v * (1.0 - (1.0 - f) * s);
    let sector = i32(i) % 6;
    if sector == 0 { return vec3<f32>(v, t, p); }
    if sector == 1 { return vec3<f32>(q, v, p); }
    if sector == 2 { return vec3<f32>(p, v, t); }
    if sector == 3 { return vec3<f32>(p, q, v); }
    if sector == 4 { return vec3<f32>(t, p, v); }
    return vec3<f32>(v, p, q);
}

// Deterministic block-state palette: golden-ratio hue per state id.
fn palette_color(state_id: u32) -> vec3<f32> {
    return hsv_to_rgb(fract(f32(state_id) * 0.618033988749895), 0.5, 0.85);
}

fn tint_rgb(tint: u32) -> vec3<f32> {
    return vec3<f32>(
        f32((tint >> 16u) & 0xffu) / 255.0,
        f32((tint >> 8u) & 0xffu) / 255.0,
        f32(tint & 0xffu) / 255.0,
    );
}

struct TerrainFragment {
    @builtin(position) clip: vec4<f32>,
    @location(0) color: vec3<f32>,
    @location(1) fog_factor: f32,
};

@vertex
fn terrain_opaque_vs(in: TerrainVertex) -> TerrainFragment {
    let lx = f32(in.packed_pos & 0x1fu);
    let lz = f32((in.packed_pos >> 5u) & 0x1fu);
    let ly = f32(in.packed_pos >> 10u) / 1024.0;
    let attr = in.attributes;
    let direction = attr & 0x7u;
    let ao = (attr >> 3u) & 0x3u;
    let block_light = f32((attr >> 5u) & 0xfu);
    let sky_light = f32((attr >> 9u) & 0xfu);
    let world = section.origin + vec3<f32>(lx, ly, lz);

    var albedo = palette_color(in.state_id);
    if in.tint != 0u {
        albedo = tint_rgb(in.tint);
    } else {
        albedo = albedo * mix(
            vec3<f32>(1.0),
            section.biome_tint,
            section.biome_tint_strength,
        );
    }

    let light = clamp(max(block_light, sky_light) / 15.0, 0.0, 1.0);
    let illum = 0.25 + 0.75 * light;
    let shade = FACE_SHADE[min(direction, 5u)] * AO_SHADE[min(ao, 3u)] * illum;

    var out: TerrainFragment;
    out.clip = frame.view_proj * vec4<f32>(world, 1.0);
    out.color = albedo * shade;
    let dist = distance(world, frame.camera_pos);
    out.fog_factor = clamp(
        (dist - frame.fog_start) / max(frame.fog_end - frame.fog_start, 0.001),
        0.0,
        1.0,
    );
    return out;
}

@fragment
fn terrain_opaque_fs(in: TerrainFragment) -> @location(0) vec4<f32> {
    let rgb = mix(in.color, frame.fog_color, in.fog_factor);
    return vec4<f32>(rgb, 1.0);
}
";

/// Translucent terrain (water, stained glass, ice): same packed-vertex
/// unpack and lighting as [`TERRAIN_OPAQUE_WGSL`], plus layer/fluid alpha
/// selection (matching `translucent_alpha` in the software rasterizer) and a
/// smoothstep fog fade so far translucency dissolves instead of popping.
pub const TERRAIN_TRANSLUCENT_WGSL: &str = "\
// Translucent terrain: alpha blending + smooth fog fade.
struct FrameUniforms {
    view_proj: mat4x4<f32>,
    camera_pos: vec3<f32>,
    fog_start: f32,
    fog_color: vec3<f32>,
    fog_end: f32,
    _pad: vec2<f32>,
};

struct SectionUniforms {
    origin: vec3<f32>,
    _pad0: f32,
    biome_tint: vec3<f32>,
    biome_tint_strength: f32,
};

@group(0) @binding(0) var<uniform> frame: FrameUniforms;
@group(0) @binding(1) var<uniform> section: SectionUniforms;

struct TerrainVertex {
    @location(0) packed_pos: u32,
    @location(1) state_id: u32,
    @location(2) attributes: u32,
    @location(3) tint: u32,
};

const FACE_SHADE = array<f32, 6>(0.45, 1.0, 0.75, 0.85, 0.7, 0.95);
const AO_SHADE = array<f32, 4>(0.4, 0.6, 0.8, 1.0);

fn hsv_to_rgb(h: f32, s: f32, v: f32) -> vec3<f32> {
    let i = floor(h * 6.0);
    let f = h * 6.0 - i;
    let p = v * (1.0 - s);
    let q = v * (1.0 - f * s);
    let t = v * (1.0 - (1.0 - f) * s);
    let sector = i32(i) % 6;
    if sector == 0 { return vec3<f32>(v, t, p); }
    if sector == 1 { return vec3<f32>(q, v, p); }
    if sector == 2 { return vec3<f32>(p, v, t); }
    if sector == 3 { return vec3<f32>(p, q, v); }
    if sector == 4 { return vec3<f32>(t, p, v); }
    return vec3<f32>(v, p, q);
}

fn palette_color(state_id: u32) -> vec3<f32> {
    return hsv_to_rgb(fract(f32(state_id) * 0.618033988749895), 0.5, 0.85);
}

fn tint_rgb(tint: u32) -> vec3<f32> {
    return vec3<f32>(
        f32((tint >> 16u) & 0xffu) / 255.0,
        f32((tint >> 8u) & 0xffu) / 255.0,
        f32(tint & 0xffu) / 255.0,
    );
}

struct TranslucentFragment {
    @builtin(position) clip: vec4<f32>,
    @location(0) color: vec3<f32>,
    @location(1) alpha: f32,
};

@vertex
fn terrain_translucent_vs(in: TerrainVertex) -> TranslucentFragment {
    let lx = f32(in.packed_pos & 0x1fu);
    let lz = f32((in.packed_pos >> 5u) & 0x1fu);
    let ly = f32(in.packed_pos >> 10u) / 1024.0;
    let attr = in.attributes;
    let direction = attr & 0x7u;
    let ao = (attr >> 3u) & 0x3u;
    let block_light = f32((attr >> 5u) & 0xfu);
    let sky_light = f32((attr >> 9u) & 0xfu);
    let layer = (attr >> 13u) & 0x3u;
    let fluid_kind = (attr >> 15u) & 0x3u;
    let world = section.origin + vec3<f32>(lx, ly, lz);

    var albedo = palette_color(in.state_id);
    if in.tint != 0u {
        albedo = tint_rgb(in.tint);
    } else {
        albedo = albedo * mix(
            vec3<f32>(1.0),
            section.biome_tint,
            section.biome_tint_strength,
        );
    }

    // LAYER_FLUID=3 with FLUID_LAVA=2 is nearly opaque; other fluids and
    // stained glass stay translucent.
    var alpha = 0.55;
    if layer == 3u {
        alpha = select(0.75, 0.95, fluid_kind == 2u);
    }

    let light = clamp(max(block_light, sky_light) / 15.0, 0.0, 1.0);
    let illum = 0.25 + 0.75 * light;
    let shade = FACE_SHADE[min(direction, 5u)] * AO_SHADE[min(ao, 3u)] * illum;

    var out: TranslucentFragment;
    out.clip = frame.view_proj * vec4<f32>(world, 1.0);
    out.color = albedo * shade;
    let dist = distance(world, frame.camera_pos);
    let fog = clamp(
        (dist - frame.fog_start) / max(frame.fog_end - frame.fog_start, 0.001),
        0.0,
        1.0,
    );
    // Smooth fade: smoothstep fog on the color, slight opacity bleed at
    // the far edge so distant water never hard-clips against the fog.
    let fog_smooth = fog * fog * (3.0 - 2.0 * fog);
    out.color = mix(out.color, frame.fog_color, fog_smooth);
    out.alpha = alpha * (1.0 - 0.35 * fog_smooth);
    return out;
}

@fragment
fn terrain_translucent_fs(in: TranslucentFragment) -> @location(0) vec4<f32> {
    return vec4<f32>(in.color, in.alpha);
}
";

// ---------------------------------------------------------------------------
// Instanced block-entity / kinetic shaders
// ---------------------------------------------------------------------------

/// Flywheel-equivalent Vulkan-native instancing — SSBO variant for the
/// `MeshShader`/`ComputeIndirect`/`Classic` backends. Vertex-shader
/// rotation uses the tick-exact formula
/// `angle = base_angle + speed * (tick - base_tick)` so rotating shafts,
/// gears, belts and animated block entities (chest lids, bell swing) never
/// rebuild their chunk mesh; `overstressed` freezes at `base_angle` and
/// `reversed` flips the axis. Sub-world transforms place airship/contraption
/// space on the same depth buffer as the static grid.
///
/// Record layouts mirror the `#[repr(C)]` CPU records:
/// `BlockEntityInstance` = `BlockEntityInstanceGpu` (48 B),
/// `KineticInstance` = `KineticInstanceGpu` (48 B, `angular_velocity` as
/// `speed`, `phase` as `base_angle`), `SubWorldTransform` =
/// `SubWorldTransformGpu` (64 B column-major).
pub const BLOCK_ENTITY_INSTANCED_WGSL: &str = "\
// Instanced block entities + kinetics: SSBO instance/subworld buffers.
struct BlockEntityInstance {
    pos: vec3<f32>,
    lid_or_anim_rad: f32,
    facing_and_flags: u32,
    light_packed: u32,
    tint_rgba: u32,
    scale_xyz: vec3<f32>,
    mesh_kind: u32,
    lod_flags: u32,
};

struct KineticInstance {
    pos: vec3<f32>,
    // Radians per tick; KineticInstanceGpu::angular_velocity.
    speed: f32,
    // Angle at base_tick; KineticInstanceGpu::phase.
    base_angle: f32,
    axis_and_flags: u32,
    light_packed: u32,
    tint_rgba: u32,
    scale_xyz: vec3<f32>,
    mesh_kind: u32,
};

struct SubWorldTransform {
    // Column-major 4x4; slot 0 is the static world grid.
    matrix: mat4x4<f32>,
};

struct InstanceFrame {
    view_proj: mat4x4<f32>,
    camera_pos: vec3<f32>,
    tick: f32,
    base_tick: f32,
    fog_start: f32,
    fog_end: f32,
    _pad0: f32,
    fog_color: vec3<f32>,
};

struct DrawParams {
    // 0 = block_entities pool, 1 = kinetic pool.
    use_kinetic: u32,
    // First mesh-kind id rendered by this draw (template select).
    mesh_kind: u32,
    _pad: vec2<u32>,
};

@group(0) @binding(0) var<uniform> frame: InstanceFrame;
@group(0) @binding(1) var<storage, read> block_entities: array<BlockEntityInstance>;
@group(0) @binding(2) var<storage, read> kinetic: array<KineticInstance>;
@group(0) @binding(3) var<storage, read> subworlds: array<SubWorldTransform>;
@group(0) @binding(4) var<uniform> params: DrawParams;

struct MeshTemplateVertex {
    // Mesh-template space: one block (0..1) or contraption-local units.
    @location(0) local_pos: vec3<f32>,
    @location(1) normal: vec3<f32>,
    @location(2) uv: vec2<f32>,
};

struct InstancedFragment {
    @builtin(position) clip: vec4<f32>,
    @location(0) color: vec3<f32>,
    @location(1) uv: vec2<f32>,
    @location(2) fog_factor: f32,
};

fn rotate_about_axis(p: vec3<f32>, axis: u32, angle: f32) -> vec3<f32> {
    let s = sin(angle);
    let c = cos(angle);
    if axis == 0u {
        return vec3<f32>(p.x, p.y * c - p.z * s, p.y * s + p.z * c);
    }
    if axis == 1u {
        return vec3<f32>(p.x * c + p.z * s, p.y, p.z * c - p.x * s);
    }
    return vec3<f32>(p.x * c - p.y * s, p.x * s + p.y * c, p.z);
}

fn unpack_light(light_packed: u32) -> vec2<f32> {
    return vec2<f32>(
        f32(light_packed & 0xfu) / 15.0,
        f32((light_packed >> 4u) & 0xfu) / 15.0,
    );
}

fn unpack_tint(tint_rgba: u32) -> vec3<f32> {
    return vec3<f32>(
        f32(tint_rgba & 0xffu) / 255.0,
        f32((tint_rgba >> 8u) & 0xffu) / 255.0,
        f32((tint_rgba >> 16u) & 0xffu) / 255.0,
    );
}

const FACE_SHADE = array<f32, 6>(0.45, 1.0, 0.75, 0.85, 0.7, 0.95);

fn normal_face_shade(n: vec3<f32>) -> f32 {
    let a = abs(n);
    if a.y >= a.x && a.y >= a.z {
        return select(FACE_SHADE[0], FACE_SHADE[1], n.y > 0.0);
    }
    if a.z >= a.x {
        return select(FACE_SHADE[2], FACE_SHADE[3], n.z > 0.0);
    }
    return select(FACE_SHADE[4], FACE_SHADE[5], n.x > 0.0);
}

@vertex
fn instanced_vs(
    in: MeshTemplateVertex,
    @builtin(instance_index) instance_index: u32,
) -> InstancedFragment {
    var local = in.local_pos;
    var normal = in.normal;
    var origin = vec3<f32>(0.0);
    var light_packed = 0u;
    var tint = 0xffffffu;
    var glowing = false;

    if params.use_kinetic != 0u {
        let kin = kinetic[min(instance_index, arrayLength(&kinetic) - 1u)];
        let axis = kin.axis_and_flags & 0x3u;
        let reversed = (kin.axis_and_flags & 0x4u) != 0u;
        let overstressed = (kin.axis_and_flags & 0x8u) != 0u;
        let subworld_id = (kin.axis_and_flags >> 4u) & 0xfffu;
        // Tick-exact rotation: angle = base_angle + speed * (tick - base_tick).
        var angle = kin.base_angle + kin.speed * (frame.tick - frame.base_tick);
        if overstressed {
            angle = kin.base_angle;
        }
        if reversed {
            angle = -angle;
        }
        local = rotate_about_axis(local * kin.scale_xyz, axis, angle);
        normal = rotate_about_axis(normal, axis, angle);
        origin = kin.pos;
        light_packed = kin.light_packed;
        tint = kin.tint_rgba;
        if arrayLength(&subworlds) > 0u {
            let t = subworlds[min(subworld_id, arrayLength(&subworlds) - 1u)];
            let world4 = t.matrix * vec4<f32>(origin + local, 1.0);
            origin = world4.xyz;
            local = vec3<f32>(0.0);
        }
    } else {
        let be = block_entities[min(instance_index, arrayLength(&block_entities) - 1u)];
        let facing_steps = be.facing_and_flags & 0x1fu;
        let yaw = f32(facing_steps) * 0.392699081698724;
        // Chest/shulker lids and bell swings hinge about +X on the upper
        // half of the template; the rest only yaws with the block facing.
        if in.local_pos.y > 0.5 {
            local = rotate_about_axis(local, 0u, be.lid_or_anim_rad);
            normal = rotate_about_axis(normal, 0u, be.lid_or_anim_rad);
        }
        local = rotate_about_axis(local * be.scale_xyz, 1u, yaw);
        normal = rotate_about_axis(normal, 1u, yaw);
        origin = be.pos;
        light_packed = be.light_packed;
        tint = be.tint_rgba;
        glowing = (be.facing_and_flags & 0x200u) != 0u;
    }

    let world = origin + local;
    let light = max(unpack_light(light_packed), vec2<f32>(0.25));
    let illum = select(max(light.x, light.y), 1.0, glowing);
    let shade = normal_face_shade(normal) * illum;

    var out: InstancedFragment;
    out.clip = frame.view_proj * vec4<f32>(world, 1.0);
    out.color = unpack_tint(tint) * shade;
    out.uv = in.uv;
    let dist = distance(world, frame.camera_pos);
    out.fog_factor = clamp(
        (dist - frame.fog_start) / max(frame.fog_end - frame.fog_start, 0.001),
        0.0,
        1.0,
    );
    return out;
}

@fragment
fn instanced_fs(in: InstancedFragment) -> @location(0) vec4<f32> {
    let rgb = mix(in.color, frame.fog_color, in.fog_factor);
    return vec4<f32>(rgb, 1.0);
}
";

/// [`BLOCK_ENTITY_INSTANCED_WGSL`] re-bound for
/// `ActiveRenderBackend::ClassicLowSpec` (pure wgpu Vulkan 1.1 / DX12 / Metal, no SSBO):
/// each 16 KiB-safe UBO batch carries at most
/// `MAX_INSTANCES_PER_UBO_16KB` (256) 48-byte records — 12,288 B, inside the
/// 16 KiB floor every Intel HD 4000+/UHD guarantees — so the same
/// tick-exact vertex rotation runs on legacy integrated GPUs.
pub const BLOCK_ENTITY_INSTANCED_UBO_WGSL: &str = "\
// Instanced block entities + kinetics: 16 KiB UBO batches (ClassicLowSpec).
struct BlockEntityInstance {
    pos: vec3<f32>,
    lid_or_anim_rad: f32,
    facing_and_flags: u32,
    light_packed: u32,
    tint_rgba: u32,
    scale_xyz: vec3<f32>,
    mesh_kind: u32,
    lod_flags: u32,
};

struct KineticInstance {
    pos: vec3<f32>,
    speed: f32,
    base_angle: f32,
    axis_and_flags: u32,
    light_packed: u32,
    tint_rgba: u32,
    scale_xyz: vec3<f32>,
    mesh_kind: u32,
};

struct SubWorldTransform {
    matrix: mat4x4<f32>,
};

struct InstanceFrame {
    view_proj: mat4x4<f32>,
    camera_pos: vec3<f32>,
    tick: f32,
    base_tick: f32,
    fog_start: f32,
    fog_end: f32,
    _pad0: f32,
    fog_color: vec3<f32>,
};

struct DrawParams {
    use_kinetic: u32,
    mesh_kind: u32,
    _pad: vec2<u32>,
};

// 256 * 48 B = 12,288 B <= 16 KiB guaranteed UBO range.
struct BlockEntityUboBatch {
    instances: array<BlockEntityInstance, 256>,
};

struct KineticUboBatch {
    instances: array<KineticInstance, 256>,
};

// 64 * 64 B = 4,096 B <= 16 KiB guaranteed UBO range.
struct SubWorldTableUbo {
    transforms: array<SubWorldTransform, 64>,
};

@group(0) @binding(0) var<uniform> frame: InstanceFrame;
@group(0) @binding(1) var<uniform> block_entities: BlockEntityUboBatch;
@group(0) @binding(2) var<uniform> kinetic: KineticUboBatch;
@group(0) @binding(3) var<uniform> subworlds: SubWorldTableUbo;
@group(0) @binding(4) var<uniform> params: DrawParams;

struct MeshTemplateVertex {
    @location(0) local_pos: vec3<f32>,
    @location(1) normal: vec3<f32>,
    @location(2) uv: vec2<f32>,
};

struct InstancedFragment {
    @builtin(position) clip: vec4<f32>,
    @location(0) color: vec3<f32>,
    @location(1) uv: vec2<f32>,
    @location(2) fog_factor: f32,
};

fn rotate_about_axis(p: vec3<f32>, axis: u32, angle: f32) -> vec3<f32> {
    let s = sin(angle);
    let c = cos(angle);
    if axis == 0u {
        return vec3<f32>(p.x, p.y * c - p.z * s, p.y * s + p.z * c);
    }
    if axis == 1u {
        return vec3<f32>(p.x * c + p.z * s, p.y, p.z * c - p.x * s);
    }
    return vec3<f32>(p.x * c - p.y * s, p.x * s + p.y * c, p.z);
}

fn unpack_light(light_packed: u32) -> vec2<f32> {
    return vec2<f32>(
        f32(light_packed & 0xfu) / 15.0,
        f32((light_packed >> 4u) & 0xfu) / 15.0,
    );
}

fn unpack_tint(tint_rgba: u32) -> vec3<f32> {
    return vec3<f32>(
        f32(tint_rgba & 0xffu) / 255.0,
        f32((tint_rgba >> 8u) & 0xffu) / 255.0,
        f32((tint_rgba >> 16u) & 0xffu) / 255.0,
    );
}

const FACE_SHADE = array<f32, 6>(0.45, 1.0, 0.75, 0.85, 0.7, 0.95);

fn normal_face_shade(n: vec3<f32>) -> f32 {
    let a = abs(n);
    if a.y >= a.x && a.y >= a.z {
        return select(FACE_SHADE[0], FACE_SHADE[1], n.y > 0.0);
    }
    if a.z >= a.x {
        return select(FACE_SHADE[2], FACE_SHADE[3], n.z > 0.0);
    }
    return select(FACE_SHADE[4], FACE_SHADE[5], n.x > 0.0);
}

@vertex
fn instanced_ubo_vs(
    in: MeshTemplateVertex,
    @builtin(instance_index) instance_index: u32,
) -> InstancedFragment {
    let idx = min(instance_index, 255u);
    var local = in.local_pos;
    var normal = in.normal;
    var origin = vec3<f32>(0.0);
    var light_packed = 0u;
    var tint = 0xffffffu;
    var glowing = false;

    if params.use_kinetic != 0u {
        let kin = kinetic.instances[idx];
        let axis = kin.axis_and_flags & 0x3u;
        let reversed = (kin.axis_and_flags & 0x4u) != 0u;
        let overstressed = (kin.axis_and_flags & 0x8u) != 0u;
        let subworld_id = (kin.axis_and_flags >> 4u) & 0xfffu;
        // Tick-exact rotation: angle = base_angle + speed * (tick - base_tick).
        var angle = kin.base_angle + kin.speed * (frame.tick - frame.base_tick);
        if overstressed {
            angle = kin.base_angle;
        }
        if reversed {
            angle = -angle;
        }
        local = rotate_about_axis(local * kin.scale_xyz, axis, angle);
        normal = rotate_about_axis(normal, axis, angle);
        origin = kin.pos;
        light_packed = kin.light_packed;
        tint = kin.tint_rgba;
        let t = subworlds.transforms[min(subworld_id, 63u)];
        let world4 = t.matrix * vec4<f32>(origin + local, 1.0);
        origin = world4.xyz;
        local = vec3<f32>(0.0);
    } else {
        let be = block_entities.instances[idx];
        let facing_steps = be.facing_and_flags & 0x1fu;
        let yaw = f32(facing_steps) * 0.392699081698724;
        if in.local_pos.y > 0.5 {
            local = rotate_about_axis(local, 0u, be.lid_or_anim_rad);
            normal = rotate_about_axis(normal, 0u, be.lid_or_anim_rad);
        }
        local = rotate_about_axis(local * be.scale_xyz, 1u, yaw);
        normal = rotate_about_axis(normal, 1u, yaw);
        origin = be.pos;
        light_packed = be.light_packed;
        tint = be.tint_rgba;
        glowing = (be.facing_and_flags & 0x200u) != 0u;
    }

    let world = origin + local;
    let light = max(unpack_light(light_packed), vec2<f32>(0.25));
    let illum = select(max(light.x, light.y), 1.0, glowing);
    let shade = normal_face_shade(normal) * illum;

    var out: InstancedFragment;
    out.clip = frame.view_proj * vec4<f32>(world, 1.0);
    out.color = unpack_tint(tint) * shade;
    out.uv = in.uv;
    let dist = distance(world, frame.camera_pos);
    out.fog_factor = clamp(
        (dist - frame.fog_start) / max(frame.fog_end - frame.fog_start, 0.001),
        0.0,
        1.0,
    );
    return out;
}

@fragment
fn instanced_ubo_fs(in: InstancedFragment) -> @location(0) vec4<f32> {
    let rgb = mix(in.color, frame.fog_color, in.fog_factor);
    return vec4<f32>(rgb, 1.0);
}
";

// ---------------------------------------------------------------------------
// Compute shaders
// ---------------------------------------------------------------------------

/// Conservative Hi-Z mip builder: one workgroup thread per destination
/// texel emits the max of its 2x2 source footprint. Odd source edges clamp
/// to the border texel so the parent max is never dropped — matching
/// `HiZPyramid::from_depth_buffer` exactly (a dropped border could falsely
/// occlude geometry along the right/bottom edge).
pub const HIZ_DOWNSAMPLE_COMPUTE_WGSL: &str = "\
// Hi-Z mip builder: 2x2 max-depth downsample with odd-border clamping.
struct HiZParams {
    src_size: vec2<u32>,
    dst_size: vec2<u32>,
};

@group(0) @binding(0) var src_depth: texture_2d<f32>;
@group(0) @binding(1) var dst_depth: texture_storage_2d<r32float, write>;
@group(0) @binding(2) var<uniform> params: HiZParams;

@compute @workgroup_size(8, 8, 1)
fn hiz_downsample(@builtin(global_invocation_id) gid: vec3<u32>) {
    if gid.x >= params.dst_size.x || gid.y >= params.dst_size.y {
        return;
    }
    var max_depth = 0.0;
    for (var dy = 0u; dy < 2u; dy += 1u) {
        for (var dx = 0u; dx < 2u; dx += 1u) {
            // Odd border: clamp taps into the source edge so border texels
            // stay inside the parent max (never dropped, never misculled).
            let sx = min(gid.x * 2u + dx, params.src_size.x - 1u);
            let sy = min(gid.y * 2u + dy, params.src_size.y - 1u);
            let d = textureLoad(src_depth, vec2<i32>(i32(sx), i32(sy)), 0).x;
            max_depth = max(max_depth, d);
        }
    }
    textureStore(
        dst_depth,
        vec2<i32>(i32(gid.x), i32(gid.y)),
        vec4<f32>(max_depth, 0.0, 0.0, 0.0),
    );
}
";

/// Meshlet cull + indirect compaction for the `ComputeIndirect` backend
/// (and the amplification step the `MeshShader` backend mirrors in its task
/// shader): frustum positive-vertex test, camera-facing mask test
/// (`Direction::bit` order Down/Up/North/South/West/East = bits 0..5), then
/// Hi-Z occlusion against the mip chain — survivors atomically append one
/// `DrawIndexedIndirectCommand` each.
pub const MESHLET_CULL_COMPUTE_WGSL: &str = "\
// Meshlet cull: frustum + facing-mask + Hi-Z occlusion -> indirect draws.
struct MeshletRecord {
    // MeshletDescriptor repr(C) bytes, little-endian:
    //   bounds0 = aabb_min.x | aabb_min.y<<8 | aabb_min.z<<16 | aabb_max.x<<24
    //   bounds1 = aabb_max.y | aabb_max.z<<8 | facing_mask<<16 | flags<<24
    bounds0: u32,
    bounds1: u32,
    quad_offset: u32,
    quad_count: u32,
};

struct DrawIndexedIndirectCommand {
    index_count: u32,
    instance_count: u32,
    first_index: u32,
    vertex_offset: i32,
    first_instance: u32,
};

struct CullUniforms {
    view_proj: mat4x4<f32>,
    frustum_planes: array<vec4<f32>, 6>,
    camera_pos: vec3<f32>,
    meshlet_count: u32,
    section_origin: vec3<f32>,
    hiz_max_mip: u32,
    screen_size: vec2<f32>,
    base_vertex: i32,
    indices_per_quad: u32,
};

@group(0) @binding(0) var<uniform> cull: CullUniforms;
@group(0) @binding(1) var<storage, read> meshlets: array<MeshletRecord>;
@group(0) @binding(2) var<storage, read_write> commands: array<DrawIndexedIndirectCommand>;
@group(0) @binding(3) var<storage, read_write> command_count: atomic<u32>;
@group(0) @binding(4) var hiz: texture_2d<f32>;

fn aabb_in_frustum(min_p: vec3<f32>, max_p: vec3<f32>) -> bool {
    for (var i = 0u; i < 6u; i += 1u) {
        let plane = cull.frustum_planes[i];
        let p = vec3<f32>(
            select(min_p.x, max_p.x, plane.x >= 0.0),
            select(min_p.y, max_p.y, plane.y >= 0.0),
            select(min_p.z, max_p.z, plane.z >= 0.0),
        );
        if dot(plane.xyz, p) + plane.w < 0.0 {
            return false;
        }
    }
    return true;
}

// Bits the camera can see of [min_p, max_p]: West=16/East=32, Down=1/Up=2,
// North=4/South=8 — the Direction::bit order the CPU facing mask uses.
fn meshlet_facing_bits(min_p: vec3<f32>, max_p: vec3<f32>) -> u32 {
    var mask = 0u;
    if cull.camera_pos.x < min_p.x {
        mask |= 16u;
    } else if cull.camera_pos.x > max_p.x {
        mask |= 32u;
    }
    if cull.camera_pos.y < min_p.y {
        mask |= 1u;
    } else if cull.camera_pos.y > max_p.y {
        mask |= 2u;
    }
    if cull.camera_pos.z < min_p.z {
        mask |= 4u;
    } else if cull.camera_pos.z > max_p.z {
        mask |= 8u;
    }
    return mask;
}

fn hiz_occluded(min_p: vec3<f32>, max_p: vec3<f32>) -> bool {
    var r0 = vec2<f32>(1.0e30);
    var r1 = vec2<f32>(-1.0e30);
    var nearest = 1.0e30;
    for (var b = 0u; b < 8u; b += 1u) {
        let corner = vec4<f32>(
            select(min_p.x, max_p.x, (b & 1u) != 0u),
            select(min_p.y, max_p.y, (b & 2u) != 0u),
            select(min_p.z, max_p.z, (b & 4u) != 0u),
            1.0,
        );
        let clip = cull.view_proj * corner;
        // A corner on or behind the near plane can still be visible.
        if clip.w <= 0.0 {
            return false;
        }
        let ndc = clip.xyz / clip.w;
        let px = (ndc.xy * 0.5 + vec2<f32>(0.5)) * cull.screen_size;
        r0 = min(r0, px);
        r1 = max(r1, px);
        nearest = min(nearest, ndc.z);
    }
    if nearest <= 0.0 {
        return false;
    }
    r0 = clamp(r0, vec2<f32>(0.0), cull.screen_size);
    r1 = clamp(r1, vec2<f32>(0.0), cull.screen_size);
    if r1.x <= r0.x || r1.y <= r0.y {
        return false;
    }
    // Smallest mip where the rect covers at most a 2x2 texel footprint.
    var level = cull.hiz_max_mip;
    for (var k = 0u; k <= cull.hiz_max_mip; k += 1u) {
        let s = f32(1u << k);
        let lo = vec2<i32>(floor(r0 / s));
        let hi = vec2<i32>(ceil(r1 / s)) - vec2<i32>(1);
        if hi.x - lo.x < 2 && hi.y - lo.y < 2 {
            level = k;
            break;
        }
    }
    let s = f32(1u << level);
    let mip_dim = vec2<i32>(textureDimensions(hiz, i32(level)));
    let lo = max(vec2<i32>(floor(r0 / s)), vec2<i32>(0));
    let hi = min(
        vec2<i32>(ceil(r1 / s)) - vec2<i32>(1),
        mip_dim - vec2<i32>(1),
    );
    var max_occluder = 0.0;
    for (var ty = lo.y; ty <= hi.y; ty += 1) {
        for (var tx = lo.x; tx <= hi.x; tx += 1) {
            let d = textureLoad(hiz, vec2<i32>(tx, ty), i32(level)).x;
            max_occluder = max(max_occluder, d);
        }
    }
    return nearest > max_occluder + 0.000001;
}

@compute @workgroup_size(64, 1, 1)
fn meshlet_cull(@builtin(global_invocation_id) gid: vec3<u32>) {
    if gid.x >= cull.meshlet_count {
        return;
    }
    let m = meshlets[gid.x];
    let aabb_min = vec3<f32>(
        f32(m.bounds0 & 0xffu),
        f32((m.bounds0 >> 8u) & 0xffu),
        f32((m.bounds0 >> 16u) & 0xffu),
    ) + cull.section_origin;
    let aabb_max = vec3<f32>(
        f32(m.bounds0 >> 24u),
        f32(m.bounds1 & 0xffu),
        f32((m.bounds1 >> 8u) & 0xffu),
    ) + cull.section_origin;
    if !aabb_in_frustum(aabb_min, aabb_max) {
        return;
    }
    let facing = meshlet_facing_bits(aabb_min, aabb_max);
    let meshlet_mask = (m.bounds1 >> 16u) & 0xffu;
    if facing != 0u && (facing & meshlet_mask) == 0u {
        return;
    }
    if hiz_occluded(aabb_min, aabb_max) {
        return;
    }
    let slot = atomicAdd(&command_count, 1u);
    commands[slot] = DrawIndexedIndirectCommand(
        m.quad_count * cull.indices_per_quad,
        1u,
        m.quad_offset * cull.indices_per_quad,
        cull.base_vertex,
        gid.x,
    );
}
";

// ---------------------------------------------------------------------------
// Structural validator
// ---------------------------------------------------------------------------

/// GPU pipeline stage a `@`-attributed `fn` entry point belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum WgslStage {
    /// `@vertex` entry point.
    Vertex,
    /// `@fragment` entry point.
    Fragment,
    /// `@compute` entry point.
    Compute,
}

impl WgslStage {
    fn attribute(self) -> &'static str {
        match self {
            Self::Vertex => "vertex",
            Self::Fragment => "fragment",
            Self::Compute => "compute",
        }
    }
}

/// Address-space/access class of a `@group/@binding` resource declaration,
/// matching how a `RenderGraph` pass binds the resource.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum WgslBindingKind {
    /// `var<uniform>` — uniform/constant buffer.
    Uniform,
    /// `var<storage, read>` — read-only storage buffer.
    StorageRead,
    /// `var<storage, read_write>` — mutable storage buffer.
    StorageReadWrite,
    /// `var` of `texture_*` (non-storage) — sampled texture.
    Texture,
    /// `var` of `texture_storage_*` — storage texture.
    StorageTexture,
    /// `var` of `sampler*` — texture sampler.
    Sampler,
}

/// One `@group(g) @binding(b)` resource declaration discovered in a module.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WgslBinding {
    /// `@group` index.
    pub group: u32,
    /// `@binding` index inside the group.
    pub binding: u32,
    /// Declared variable name.
    pub name: String,
    /// Address-space/access class parsed from the `var` declaration.
    pub kind: WgslBindingKind,
}

/// Why [`validate_wgsl_module`] rejected a module.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WgslValidationError {
    /// A `(`/`[`/`{` was left open, or a closer did not match the innermost
    /// opener. `position` is the byte offset of the offending closer, or
    /// `source.len()` when the source ended with open delimiters. `found`
    /// is `'\0'` at end-of-source.
    UnbalancedDelimiter {
        /// Byte offset of the mismatched closer (or `source.len()`).
        position: usize,
        /// The opener left unmatched.
        open: char,
        /// The closer that did not match.
        found: char,
    },
    /// A `/*` comment ran to end-of-source without `*/`.
    UnterminatedComment {
        /// Byte offset of the `/*`.
        position: usize,
    },
    /// A stage listed in `required` has no entry point in the module.
    MissingEntryPoint {
        /// The stage with no entry point.
        stage: WgslStage,
    },
    /// A `@group`/`@binding` attribute was malformed (non-numeric or
    /// missing argument, unbalanced argument parens, dangling without a
    /// `var`, or a `var` missing one of the pair).
    MalformedAttribute {
        /// Byte offset where the malformed attribute starts.
        position: usize,
        /// What was wrong, in words.
        detail: String,
    },
    /// Two `var` declarations share one `(group, binding)` pair.
    DuplicateBinding {
        /// Shared group index.
        group: u32,
        /// Shared binding index.
        binding: u32,
    },
}

impl std::fmt::Display for WgslValidationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnbalancedDelimiter {
                position,
                open,
                found,
            } => {
                if *found == '\0' {
                    write!(f, "unclosed '{open}' at end of WGSL source")
                } else {
                    write!(
                        f,
                        "delimiter mismatch at byte {position}: '{open}' closed by '{found}'"
                    )
                }
            }
            Self::UnterminatedComment { position } => {
                write!(f, "unterminated block comment at byte {position}")
            }
            Self::MissingEntryPoint { stage } => {
                write!(f, "no @{stage_attr} entry point", stage_attr = stage.attribute())
            }
            Self::MalformedAttribute { position, detail } => {
                write!(f, "malformed attribute at byte {position}: {detail}")
            }
            Self::DuplicateBinding { group, binding } => {
                write!(f, "duplicate @group({group}) @binding({binding})")
            }
        }
    }
}

impl std::error::Error for WgslValidationError {}

/// Structural summary of one WGSL module: its entry points and declared
/// resource bindings, for cross-checking against a `RenderGraph` pass.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WgslModuleInfo {
    /// `(stage, fn name)` per `@vertex`/`@fragment`/`@compute` entry point.
    pub entry_points: Vec<(WgslStage, String)>,
    /// `@group/@binding` resource declarations in source order.
    pub bindings: Vec<WgslBinding>,
}

/// Replace `//` line comments and `/* */` block comments with whitespace,
/// preserving byte offsets so diagnostics point at the original source.
fn scrub_comments(source: &str) -> Result<String, WgslValidationError> {
    let bytes = source.as_bytes();
    let mut out = bytes.to_vec();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'/' && i + 1 < bytes.len() {
            match bytes[i + 1] {
                b'/' => {
                    out[i] = b' ';
                    out[i + 1] = b' ';
                    i += 2;
                    while i < bytes.len() && bytes[i] != b'\n' {
                        out[i] = b' ';
                        i += 1;
                    }
                }
                b'*' => {
                    let start = i;
                    out[i] = b' ';
                    out[i + 1] = b' ';
                    i += 2;
                    let mut closed = false;
                    while i + 1 < bytes.len() {
                        if bytes[i] == b'*' && bytes[i + 1] == b'/' {
                            out[i] = b' ';
                            out[i + 1] = b' ';
                            i += 2;
                            closed = true;
                            break;
                        }
                        if bytes[i] != b'\n' {
                            out[i] = b' ';
                        }
                        i += 1;
                    }
                    if !closed {
                        return Err(WgslValidationError::UnterminatedComment { position: start });
                    }
                }
                _ => i += 1,
            }
        } else {
            i += 1;
        }
    }
    String::from_utf8(out).map_err(|_| WgslValidationError::MalformedAttribute {
        position: 0,
        detail: "source is not valid UTF-8 after comment scrub".to_string(),
    })
}

/// Check `()[]{}' balance over the scrubbed source.
fn check_delimiters(scrubbed: &str) -> Result<(), WgslValidationError> {
    let bytes = scrubbed.as_bytes();
    let mut stack: Vec<(u8, usize)> = Vec::new();
    for (i, &b) in bytes.iter().enumerate() {
        match b {
            b'(' | b'[' | b'{' => stack.push((b, i)),
            b')' | b']' | b'}' => {
                let want = match b {
                    b')' => b'(',
                    b']' => b'[',
                    _ => b'{',
                };
                match stack.pop() {
                    Some((open, _)) if open == want => {}
                    Some((open, _)) => {
                        return Err(WgslValidationError::UnbalancedDelimiter {
                            position: i,
                            open: open as char,
                            found: b as char,
                        });
                    }
                    None => {
                        return Err(WgslValidationError::UnbalancedDelimiter {
                            position: i,
                            open: '\0',
                            found: b as char,
                        });
                    }
                }
            }
            _ => {}
        }
    }
    if let Some((open, _)) = stack.pop() {
        return Err(WgslValidationError::UnbalancedDelimiter {
            position: scrubbed.len(),
            open: open as char,
            found: '\0',
        });
    }
    Ok(())
}

fn is_ident_start(b: u8) -> bool {
    b.is_ascii_alphabetic() || b == b'_'
}

fn is_ident_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// Whether `word` starts at `i` as a whole WGSL token.
fn word_at(bytes: &[u8], i: usize, word: &str) -> bool {
    let before_ok = i == 0 || !is_ident_char(bytes[i - 1]);
    let end = i + word.len();
    if end > bytes.len() || !before_ok {
        return false;
    }
    &bytes[i..end] == word.as_bytes() && (end == bytes.len() || !is_ident_char(bytes[end]))
}

fn skip_ws(bytes: &[u8], mut i: usize) -> usize {
    while i < bytes.len() && bytes[i].is_ascii_whitespace() {
        i += 1;
    }
    i
}

/// Read an identifier starting at `i`; returns `(name, end)`.
fn read_ident(bytes: &[u8], i: usize) -> Option<(String, usize)> {
    if i >= bytes.len() || !is_ident_start(bytes[i]) {
        return None;
    }
    let mut end = i + 1;
    while end < bytes.len() && is_ident_char(bytes[end]) {
        end += 1;
    }
    Some((
        std::str::from_utf8(&bytes[i..end]).ok()?.to_string(),
        end,
    ))
}

/// Skip one `@name` attribute plus its optional balanced `(...)` argument;
/// returns the attribute name, argument text, and the offset just past it.
fn read_attribute(
    bytes: &[u8],
    at: usize,
) -> Result<(String, Option<String>, usize), WgslValidationError> {
    let (name, mut i) = read_ident(bytes, at + 1).ok_or_else(|| {
        WgslValidationError::MalformedAttribute {
            position: at,
            detail: "'@' is not followed by an attribute name".to_string(),
        }
    })?;
    i = skip_ws(bytes, i);
    if i >= bytes.len() || bytes[i] != b'(' {
        return Ok((name, None, i));
    }
    let arg_start = i;
    let mut depth = 0usize;
    while i < bytes.len() {
        match bytes[i] {
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    let args = std::str::from_utf8(&bytes[arg_start + 1..i])
                        .unwrap_or("")
                        .to_string();
                    return Ok((name, Some(args), i + 1));
                }
            }
            _ => {}
        }
        i += 1;
    }
    Err(WgslValidationError::MalformedAttribute {
        position: at,
        detail: format!("unbalanced '(' in @{name} attribute"),
    })
}

fn parse_u32_arg(
    args: Option<String>,
    at: usize,
    name: &str,
) -> Result<u32, WgslValidationError> {
    let detail = || WgslValidationError::MalformedAttribute {
        position: at,
        detail: format!("@{name} expects one u32 argument"),
    };
    let text = args.ok_or_else(detail)?;
    text.trim().parse::<u32>().map_err(|_| detail())
}

/// Classify a `var` declaration from its address space and store type.
fn classify_var(
    address_space: &str,
    store_type: &str,
) -> Result<WgslBindingKind, String> {
    let ty = store_type.trim();
    match address_space.trim() {
        "uniform" => Ok(WgslBindingKind::Uniform),
        s if s.starts_with("storage") => {
            if s.contains("read_write") {
                Ok(WgslBindingKind::StorageReadWrite)
            } else {
                Ok(WgslBindingKind::StorageRead)
            }
        }
        "" => {
            if ty.starts_with("texture_storage") {
                Ok(WgslBindingKind::StorageTexture)
            } else if ty.starts_with("texture") {
                Ok(WgslBindingKind::Texture)
            } else if ty.starts_with("sampler") {
                Ok(WgslBindingKind::Sampler)
            } else {
                Err(format!("unrecognized resource type '{ty}'"))
            }
        }
        other => Err(format!("@{other} vars cannot carry @group/@binding")),
    }
}

/// Parse the `var` declaration starting at `i` (pointing at `var`):
/// `var[<addr, access>] name : Type [= init]`, stopping after `name`/`Type`.
/// Returns `(name, kind, end-of-decl-type)`.
fn parse_var_decl(
    bytes: &[u8],
    mut i: usize,
    at: usize,
) -> Result<(String, WgslBindingKind, usize), WgslValidationError> {
    i += 3; // 'var'
    i = skip_ws(bytes, i);
    let mut address_space = String::new();
    if i < bytes.len() && bytes[i] == b'<' {
        let start = i + 1;
        let mut depth = 1usize;
        i += 1;
        while i < bytes.len() && depth > 0 {
            match bytes[i] {
                b'<' => depth += 1,
                b'>' => depth -= 1,
                _ => {}
            }
            i += 1;
        }
        if depth != 0 {
            return Err(WgslValidationError::MalformedAttribute {
                position: at,
                detail: "unbalanced '<' in var address space".to_string(),
            });
        }
        address_space = std::str::from_utf8(&bytes[start..i - 1])
            .unwrap_or("")
            .to_string();
        i = skip_ws(bytes, i);
    }
    let (name, name_end) =
        read_ident(bytes, i).ok_or_else(|| WgslValidationError::MalformedAttribute {
            position: at,
            detail: "var declaration is missing a name".to_string(),
        })?;
    i = skip_ws(bytes, name_end);
    if i >= bytes.len() || bytes[i] != b':' {
        return Err(WgslValidationError::MalformedAttribute {
            position: at,
            detail: format!("var '{name}' is missing a ': Type' store type"),
        });
    }
    let type_start = skip_ws(bytes, i + 1);
    // The store type ends at the next '=' or ';' at angle depth 0.
    let mut j = type_start;
    let mut depth = 0usize;
    while j < bytes.len() {
        match bytes[j] {
            b'<' => depth += 1,
            b'>' => depth = depth.saturating_sub(1),
            b'=' | b';' if depth == 0 => break,
            _ => {}
        }
        j += 1;
    }
    let store_type = std::str::from_utf8(&bytes[type_start..j])
        .unwrap_or("")
        .to_string();
    let kind = classify_var(&address_space, &store_type).map_err(|detail| {
        WgslValidationError::MalformedAttribute {
            position: at,
            detail,
        }
    })?;
    Ok((name, kind, j))
}

/// Static structural check of one WGSL module: balanced delimiters, no
/// unterminated comments, an entry point for every `required` stage, and a
/// well-formed unique `@group/@binding` declaration on each bound `var`.
/// This is deliberately *not* a WGSL compiler — it pins the layout contract
/// the `RenderGraph` planner binds against without pulling in a GPU crate.
pub fn validate_wgsl_module(
    source: &str,
    required: &[WgslStage],
) -> Result<WgslModuleInfo, WgslValidationError> {
    let scrubbed = scrub_comments(source)?;
    check_delimiters(&scrubbed)?;
    let bytes = scrubbed.as_bytes();
    let mut info = WgslModuleInfo::default();
    let mut pending_stage: Option<(WgslStage, usize)> = None;
    let mut pending_group: Option<(u32, usize)> = None;
    let mut pending_binding: Option<(u32, usize)> = None;

    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'@' {
            let (name, args, end) = read_attribute(bytes, i)?;
            match name.as_str() {
                "vertex" | "fragment" | "compute" => {
                    let stage = match name.as_str() {
                        "vertex" => WgslStage::Vertex,
                        "fragment" => WgslStage::Fragment,
                        _ => WgslStage::Compute,
                    };
                    if pending_stage.replace((stage, i)).is_some() {
                        return Err(WgslValidationError::MalformedAttribute {
                            position: i,
                            detail: "conflicting stage attributes on one entry point"
                                .to_string(),
                        });
                    }
                }
                "group" => {
                    let group = parse_u32_arg(args, i, &name)?;
                    if pending_group.replace((group, i)).is_some() {
                        return Err(WgslValidationError::MalformedAttribute {
                            position: i,
                            detail: "second @group before the first bound a var".to_string(),
                        });
                    }
                }
                "binding" => {
                    let binding = parse_u32_arg(args, i, &name)?;
                    if pending_binding.replace((binding, i)).is_some() {
                        return Err(WgslValidationError::MalformedAttribute {
                            position: i,
                            detail: "second @binding before the first bound a var"
                                .to_string(),
                        });
                    }
                }
                _ => {}
            }
            i = end;
            continue;
        }
        if word_at(bytes, i, "fn") {
            if pending_group.is_some() || pending_binding.is_some() {
                let position = pending_group
                    .or(pending_binding)
                    .map(|(_, position)| position)
                    .ok_or_else(|| WgslValidationError::MalformedAttribute {
                        position: i,
                        detail: "@group/@binding position state missing".to_string(),
                    })?;
                return Err(WgslValidationError::MalformedAttribute {
                    position,
                    detail: "@group/@binding must precede a var, not a fn".to_string(),
                });
            }
            let name_start = skip_ws(bytes, i + 2);
            if let Some((stage, _)) = pending_stage.take() {
                let (fname, _) =
                    read_ident(bytes, name_start).ok_or_else(|| {
                        WgslValidationError::MalformedAttribute {
                            position: name_start,
                            detail: format!(
                                "@{} is not followed by an entry-point fn name",
                                stage.attribute()
                            ),
                        }
                    })?;
                info.entry_points.push((stage, fname));
            }
            i += 2;
            continue;
        }
        if word_at(bytes, i, "var") {
            match (pending_group.take(), pending_binding.take()) {
                (Some((group, _)), Some((binding, _))) => {
                    let (name, kind, _) = parse_var_decl(bytes, i, i)?;
                    if info
                        .bindings
                        .iter()
                        .any(|b| b.group == group && b.binding == binding)
                    {
                        return Err(WgslValidationError::DuplicateBinding { group, binding });
                    }
                    info.bindings.push(WgslBinding {
                        group,
                        binding,
                        name,
                        kind,
                    });
                }
                (Some((_, at)), None) | (None, Some((_, at))) => {
                    return Err(WgslValidationError::MalformedAttribute {
                        position: at,
                        detail: "@group and @binding must appear as a pair".to_string(),
                    });
                }
                (None, None) => {}
            }
            i += 3;
            continue;
        }
        // Any statement/expression boundary drops a dangling pair.
        if matches!(b, b';' | b'{' | b'}') {
            pending_group = None;
            pending_binding = None;
        }
        i += 1;
    }

    if let Some((stage, at)) = pending_stage {
        return Err(WgslValidationError::MalformedAttribute {
            position: at,
            detail: format!("@{} is not followed by a fn", stage.attribute()),
        });
    }
    if let Some((_, at)) = pending_group.or(pending_binding) {
        return Err(WgslValidationError::MalformedAttribute {
            position: at,
            detail: "@group/@binding attribute never bound a var".to_string(),
        });
    }
    for &stage in required {
        if !info.entry_points.iter().any(|(s, _)| *s == stage) {
            return Err(WgslValidationError::MissingEntryPoint { stage });
        }
    }
    Ok(info)
}

/// Static description of one embedded WGSL module: where it lives in this
/// suite and which entry-point stages a validation pass must find.
#[derive(Clone, Copy, Debug)]
pub struct ShaderSpec {
    /// Stable snake_case module name (matches the `*_WGSL` constant).
    pub name: &'static str,
    /// The WGSL source text.
    pub source: &'static str,
    /// Entry-point stages the module must declare.
    pub required_stages: &'static [WgslStage],
}

const STAGES_GRAPHICS: &[WgslStage] = &[WgslStage::Vertex, WgslStage::Fragment];
const STAGES_COMPUTE: &[WgslStage] = &[WgslStage::Compute];

/// Every WGSL module in this suite, for table-driven validation tests.
pub const SHADER_SUITE: &[ShaderSpec] = &[
    ShaderSpec {
        name: "terrain_opaque",
        source: TERRAIN_OPAQUE_WGSL,
        required_stages: STAGES_GRAPHICS,
    },
    ShaderSpec {
        name: "terrain_translucent",
        source: TERRAIN_TRANSLUCENT_WGSL,
        required_stages: STAGES_GRAPHICS,
    },
    ShaderSpec {
        name: "block_entity_instanced",
        source: BLOCK_ENTITY_INSTANCED_WGSL,
        required_stages: STAGES_GRAPHICS,
    },
    ShaderSpec {
        name: "block_entity_instanced_ubo",
        source: BLOCK_ENTITY_INSTANCED_UBO_WGSL,
        required_stages: STAGES_GRAPHICS,
    },
    ShaderSpec {
        name: "hiz_downsample_compute",
        source: HIZ_DOWNSAMPLE_COMPUTE_WGSL,
        required_stages: STAGES_COMPUTE,
    },
    ShaderSpec {
        name: "meshlet_cull_compute",
        source: MESHLET_CULL_COMPUTE_WGSL,
        required_stages: STAGES_COMPUTE,
    },
];
