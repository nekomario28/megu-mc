// SPDX-License-Identifier: LGPL-3.0-or-later
//! Create Comfort Phase 2D — external `MeguWgslPack` coexistence with
//! Create kinetic/sub-world rendering:
//!
//! * `contraption_helper_rotates_and_tracks_caster_digest` — the
//!   `upsert_contraption_subworld` helper produces a real axis-angle
//!   rotation, advancing either the contraption angle or a water wheel's
//!   per-instance spin moves the `subworld_caster_digest` (the live
//!   shadow-invalidation signal).
//! * `create_contraption_shaderpack_coexistence_real_gpu` — a rotating
//!   contraption sub-world + instanced water wheel + translucent water +
//!   active shadow cascades + an active `gbuffers_subworld` pack pass all
//!   render through `render_offscreen_composed_frame` on a real `wgpu`
//!   device with zero uncaptured validation errors — skipped only when
//!   no adapter exists at all.

use std::sync::{Arc, Mutex};

use megu_client::protocol::{DecodedChunk, DecodedLightData, DecodedSection, DimensionMeta};
use megu_client::world::{BlockTable, WorldState};
use megu_client_gpu::atmosphere::{AtmosphereParams, BiomeAtmosphereBlender, ChunkFadeTracker};
use megu_client_gpu::renderer::{
    probe_adapters, request_gpu_device, subworld_caster_digest, ComposedFrame, GpuRenderer,
    RendererError, ShaderpackAttempt, OFFSCREEN_FORMAT,
};
use megu_client_gpu::subworld_render::{
    box_mesh, InstancedPartBatch, InstancedPartRecord, SubWorldRenderBank, SubWorldTransform,
};
use megu_client_gpu::{build_demo_scene, mat4_mul_vec4, Camera};
use megu_client_mesh::builtin_shadow_wgsl::{BuiltinShadowQuality, RenderScalePreset};
use megu_client_mesh::{
    MeguWgslPack, MeguWgslPass, PassKind, PipelineStage, RenderBackendPreference,
    ShaderCompatibilityReport,
};

const STONE: u32 = 1;
const WATER: u32 = 80;
const CONTRAPTION_ID: u64 = 41;
const WHEEL_MESH_KIND: u32 = 42;

// ---------------------------------------------------------------------------
// Pack fixtures: a minimal `gbuffers_terrain` (drives the terrain pipeline)
// and a `gbuffers_subworld` authored against the composed lit contract —
// group 0 frame / group 1 draw / group 2 env uniforms, `TerrainVertex`
// inputs at locations 0..2+7..8 and the instance stream at 3..6.
// ---------------------------------------------------------------------------

const PACK_TERRAIN_WGSL: &str = r#"
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

struct PackVertex {
    @location(0) packed_pos: u32,
    @location(1) state_id: u32,
    @location(2) attributes: u32,
    @location(3) tint: u32,
};
struct PackFragment {
    @builtin(position) clip: vec4<f32>,
    @location(0) color: vec3<f32>,
};

@vertex
fn pack_terrain_vs(in: PackVertex) -> PackFragment {
    let lx = f32(in.packed_pos & 0x1fu);
    let lz = f32((in.packed_pos >> 5u) & 0x1fu);
    let ly = f32(in.packed_pos >> 10u) / 1024.0;
    let world = section.origin + vec3<f32>(lx, ly, lz);
    var out: PackFragment;
    out.clip = frame.view_proj * vec4<f32>(world, 1.0);
    out.color = vec3<f32>(0.45, 0.62, 0.35);
    return out;
}

@fragment
fn pack_terrain_fs(in: PackFragment) -> @location(0) vec4<f32> {
    return vec4<f32>(in.color, 1.0);
}
"#;

const PACK_SUBWORLD_WGSL: &str = r#"
struct FrameUniforms {
    view_proj: mat4x4<f32>,
    camera_pos: vec3<f32>,
    fog_start: f32,
    fog_color: vec3<f32>,
    fog_end: f32,
    _pad: vec2<f32>,
};
struct DrawUniforms {
    mvp: mat4x4<f32>,
    model: mat4x4<f32>,
    params: vec4<f32>,
    params2: vec4<f32>,
};
struct EnvUniforms {
    sun: vec4<f32>,
    ambient: vec4<f32>,
    atmo: vec4<f32>,
    depth_tint: vec4<f32>,
};
@group(0) @binding(0) var<uniform> frame: FrameUniforms;
@group(1) @binding(0) var<uniform> draw: DrawUniforms;
@group(2) @binding(0) var<uniform> env: EnvUniforms;

struct LitVertex {
    @location(0) position: vec3<f32>,
    @location(1) packed_normal: u32,
    @location(2) packed_albedo: u32,
    @location(7) uv: vec2<f32>,
    @location(8) atlas_index: u32,
};
struct PartInstance {
    @location(3) pos_scale: vec4<f32>,
    @location(4) rot: vec4<f32>,
    @location(5) light_tint: vec2<u32>,
    @location(6) kind_sub: vec2<u32>,
};
struct PackFrag {
    @builtin(position) clip: vec4<f32>,
    @location(0) world: vec3<f32>,
    @location(1) normal: vec3<f32>,
    @location(2) albedo: vec3<f32>,
};

fn rotate_euler(p: vec3<f32>, rot: vec3<f32>) -> vec3<f32> {
    let cx = cos(rot.x);
    let sx = sin(rot.x);
    let cy = cos(rot.y);
    let sy = sin(rot.y);
    let cz = cos(rot.z);
    let sz = sin(rot.z);
    var v = vec3<f32>(p.x, p.y * cx - p.z * sx, p.y * sx + p.z * cx);
    v = vec3<f32>(v.x * cy + v.z * sy, v.y, v.z * cy - v.x * sy);
    v = vec3<f32>(v.x * cz - v.y * sz, v.x * sz + v.y * cz, v.z);
    return v;
}

fn unpack_oct_normal(packed: u32) -> vec3<f32> {
    let ex = f32(packed & 0xffffu) * (2.0 / 65535.0) - 1.0;
    let ey = f32(packed >> 16u) * (2.0 / 65535.0) - 1.0;
    var n = vec3<f32>(ex, ey, 1.0 - abs(ex) - abs(ey));
    let t = max(-n.z, 0.0);
    n.x += select(t, -t, n.x >= 0.0);
    n.y += select(t, -t, n.y >= 0.0);
    return normalize(n);
}

fn unpack_unorm8_albedo(packed: u32) -> vec3<f32> {
    return vec3<f32>(
        f32(packed & 0xffu) / 255.0,
        f32((packed >> 8u) & 0xffu) / 255.0,
        f32((packed >> 16u) & 0xffu) / 255.0,
    );
}

@vertex
fn pack_subworld_vs(in: LitVertex) -> PackFrag {
    var out: PackFrag;
    out.clip = draw.mvp * vec4<f32>(in.position, 1.0);
    out.world = (draw.model * vec4<f32>(in.position, 1.0)).xyz;
    out.normal = normalize((draw.model * vec4<f32>(unpack_oct_normal(in.packed_normal), 0.0)).xyz);
    out.albedo = unpack_unorm8_albedo(in.packed_albedo);
    return out;
}

@vertex
fn pack_subworld_inst_vs(in: LitVertex, inst: PartInstance) -> PackFrag {
    let local = rotate_euler(in.position * inst.pos_scale.w, inst.rot.xyz);
    let nrm = rotate_euler(unpack_oct_normal(in.packed_normal), inst.rot.xyz);
    let base = inst.pos_scale.xyz + local;
    var out: PackFrag;
    out.clip = draw.mvp * vec4<f32>(base, 1.0);
    out.world = (draw.model * vec4<f32>(base, 1.0)).xyz;
    out.normal = normalize((draw.model * vec4<f32>(nrm, 0.0)).xyz);
    out.albedo = unpack_unorm8_albedo(in.packed_albedo);
    return out;
}

@fragment
fn pack_subworld_fs(in: PackFrag) -> @location(0) vec4<f32> {
    let n = normalize(in.normal);
    // Pack-authored lighting that reads the live environment uniforms:
    // hemisphere ambient scaled by the pack-visible exposure, sun
    // diffuse, distance fog — so Create geometry shares the active
    // shaderpack's sun/fog environment.
    var light = env.ambient.rgb * mix(0.6, 1.05, n.y * 0.5 + 0.5) * env.ambient.w;
    light += vec3<f32>(env.sun.w) * max(dot(n, normalize(env.sun.xyz)), 0.0);
    var color = in.albedo * light;
    let dist = distance(in.world, frame.camera_pos);
    let fog = clamp(
        (dist - frame.fog_start) / max(frame.fog_end - frame.fog_start, 0.001),
        0.0,
        1.0,
    );
    color = mix(color, frame.fog_color, fog);
    let fade = clamp(draw.params.x, 0.0, 1.0);
    color = mix(frame.fog_color, color, fade);
    return vec4<f32>(color, draw.params.y * fade);
}
"#;

fn test_pack() -> MeguWgslPack {
    MeguWgslPack {
        name: "create-coexistence-test".to_string(),
        author: "client-gpu-tests".to_string(),
        version: "0.1.0".to_string(),
        api_version: 1,
        requires_compute: false,
        requires_ssbo: false,
        supports_lod: false,
        passes: vec![
            MeguWgslPass {
                name: "gbuffers_terrain".to_string(),
                stage: PipelineStage::Gbuffers,
                kind: PassKind::Graphics,
                file: "gbuffers_terrain.wgsl".to_string(),
                wgsl_source: PACK_TERRAIN_WGSL.to_string(),
                render_targets: Vec::new(),
                work_groups: None,
            },
            MeguWgslPass {
                name: "gbuffers_subworld".to_string(),
                stage: PipelineStage::Gbuffers,
                kind: PassKind::Graphics,
                file: "gbuffers_subworld.wgsl".to_string(),
                wgsl_source: PACK_SUBWORLD_WGSL.to_string(),
                render_targets: Vec::new(),
                work_groups: None,
            },
        ],
        compatibility: ShaderCompatibilityReport::Compatible,
    }
}

// ---------------------------------------------------------------------------
// Contraption helper + caster digest (no GPU needed)
// ---------------------------------------------------------------------------

fn wheel_mesh() -> (Vec<megu_client_gpu::subworld_render::TerrainVertex>, Vec<u32>) {
    box_mesh([-0.5, -0.5, -0.25], [0.5, 0.5, 0.25], [0.55, 0.4, 0.2])
}

fn wheel_instances(angle: f32) -> Vec<InstancedPartRecord> {
    (0..4)
        .map(|i| {
            let a = i as f32 * 1.6 + angle;
            InstancedPartRecord {
                pos_scale: [a.cos() * 3.0, 0.0, a.sin() * 3.0, 1.0],
                rot_uv_frame: [0.0, angle, 0.0, 0.0],
                packed_light: 0xFF,
                tint_rgba: 0xFF_FF_FF_FF,
                mesh_kind: WHEEL_MESH_KIND,
                subworld_id: 0,
            }
        })
        .collect()
}

#[test]
fn contraption_helper_rotates_and_tracks_caster_digest() {
    let mut bank = SubWorldRenderBank::new();
    let mesh = wheel_mesh();
    bank.upsert_contraption_subworld(CONTRAPTION_ID, [8.0, 20.0, 8.0], [0.0, 1.0, 0.0], 0.0, &mesh);
    let entry = bank.get(CONTRAPTION_ID).expect("contraption registered");
    assert!(entry.dirty, "new contraption uploads its geometry");
    assert_eq!(entry.transform.position, [8.0, 20.0, 8.0]);

    // Spinning the bearing by a quarter turn rotates the model-space +X
    // arm toward -Z around the origin pivot.
    bank.upsert_contraption_subworld(
        CONTRAPTION_ID,
        [8.0, 20.0, 8.0],
        [0.0, 1.0, 0.0],
        std::f32::consts::FRAC_PI_2,
        &mesh,
    );
    let entry = bank.get(CONTRAPTION_ID).expect("contraption registered");
    let rotated = mat4_mul_vec4(&entry.transform.model_matrix(), [1.0, 0.0, 0.0, 1.0]);
    assert!(
        (rotated[0] - 8.0).abs() < 1e-5
            && (rotated[1] - 20.0).abs() < 1e-5
            && (rotated[2] - 7.0).abs() < 1e-5,
        "90° about +Y must send the +X arm to -Z: {rotated:?}"
    );

    // Same mesh re-upsert: transform changes alone never re-dirty.
    let digest_still = subworld_caster_digest(&bank);
    bank.upsert_contraption_subworld(
        CONTRAPTION_ID,
        [8.0, 20.0, 8.0],
        [0.0, 1.0, 0.0],
        0.4,
        &mesh,
    );
    let digest_rotated = subworld_caster_digest(&bank);
    assert_ne!(
        digest_still, digest_rotated,
        "advancing the contraption angle must move the caster digest"
    );

    // Instanced water wheel: advancing per-instance spin also moves the
    // digest — live shadow updates cannot freeze mid-turn.
    let mut batch = InstancedPartBatch::new(WHEEL_MESH_KIND, mesh.0.clone(), mesh.1.clone());
    batch.instances = wheel_instances(0.0);
    bank.upsert_batch(batch);
    let digest_wheel_a = subworld_caster_digest(&bank);
    assert!(bank.set_batch_instances(WHEEL_MESH_KIND, wheel_instances(0.5)));
    let digest_wheel_b = subworld_caster_digest(&bank);
    assert_ne!(
        digest_wheel_a, digest_wheel_b,
        "advancing the water wheel's per-instance spin must move the caster digest"
    );
}

// ---------------------------------------------------------------------------
// Real wgpu coexistence render (skips only with zero adapters)
// ---------------------------------------------------------------------------

fn test_dimension() -> DimensionMeta {
    DimensionMeta {
        name: "minecraft:overworld".to_string(),
        min_y: 0,
        height: 64,
        has_skylight: true,
    }
}

fn uniform_section(state: u32) -> DecodedSection {
    DecodedSection {
        non_air: if state == 0 {
            0
        } else {
            megu_client_mesh::SECTION_CELLS as i16
        },
        states: Box::new([state; megu_client_mesh::SECTION_CELLS]),
        biomes: [0; 64],
    }
}

fn chunk(cx: i32, cz: i32, sections: Vec<DecodedSection>) -> DecodedChunk {
    DecodedChunk {
        cx,
        cz,
        sections,
        light: DecodedLightData {
            sky_mask: Vec::new(),
            block_mask: Vec::new(),
            empty_sky_mask: Vec::new(),
            empty_block_mask: Vec::new(),
            sky_arrays: Vec::new(),
            block_arrays: Vec::new(),
        },
        block_entities: Vec::new(),
    }
}

#[test]
fn create_contraption_shaderpack_coexistence_real_gpu() {
    if probe_adapters(RenderBackendPreference::Auto).is_empty() {
        eprintln!("SKIP coexistence render: probe_adapters found no adapters");
        return;
    }
    let ctx = match request_gpu_device(RenderBackendPreference::Auto, None, None) {
        Ok(ctx) => ctx,
        Err(RendererError::NoAdapter(msg)) => {
            eprintln!("SKIP coexistence render: no wgpu adapter ({msg})");
            return;
        }
        Err(e) => panic!("unexpected device error: {e}"),
    };
    eprintln!("coexistence render on {:?} {}", ctx.report.backend, ctx.report.name);
    // Capture every uncaptured wgpu error; validation errors land here.
    let errors: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    {
        let errors = Arc::clone(&errors);
        ctx.device.on_uncaptured_error(Arc::new(move |e| {
            errors.lock().expect("errors").push(e.to_string());
        }));
    }

    let mut renderer = GpuRenderer::new(ctx, OFFSCREEN_FORMAT);
    renderer.set_render_scale(RenderScalePreset::Native100);
    // Active shadow map is part of the contract; an adapter that cannot
    // run cascades cannot prove coexistence.
    let negotiated = renderer.set_shadow_quality(BuiltinShadowQuality::High);
    assert_ne!(
        negotiated,
        BuiltinShadowQuality::Off,
        "adapter must run shadow cascades for the coexistence contract"
    );

    // The pack: `gbuffers_terrain` takes over terrain while
    // `gbuffers_subworld` takes over the lit sub-world + instanced
    // kinetic-part pipelines against the composed env/shadow uniforms.
    let attempt = renderer.apply_shaderpack(&test_pack());
    match &attempt {
        ShaderpackAttempt::Active { terrain_pass, .. } => {
            assert_eq!(terrain_pass, "gbuffers_terrain");
        }
        ShaderpackAttempt::Fallback { notice } => {
            panic!("test pack must activate, got fallback: {notice}")
        }
    }
    assert_eq!(
        renderer.pack_subworld_pass(),
        Some("gbuffers_subworld"),
        "the pack's gbuffers_subworld pass must drive Create draws"
    );

    // Opaque terrain base.
    renderer
        .upload_scene(&build_demo_scene([8.0, 14.0, 30.0]))
        .expect("demo scene upload");

    // Translucent water: a live section carrying decoded water geometry
    // feeds the dedicated `MEGU_WATER_SURFACE_WGSL` pass.
    let mut world = WorldState::new(test_dimension(), BlockTable::builtin());
    world
        .apply_chunk(chunk(
            0,
            0,
            vec![uniform_section(STONE), uniform_section(WATER), uniform_section(0)],
        ))
        .unwrap();
    world
        .rebuild_dirty_bounded(usize::MAX, [8.0, 26.0, 32.0])
        .unwrap();
    renderer.update_live_terrain(&world);

    // Rotating Create contraption sub-world (mechanical bearing) plus a
    // GPU-instanced water wheel submerged in the water column.
    let mut bank = SubWorldRenderBank::new();
    let mesh = wheel_mesh();
    let mut angle = 0.0f32;
    bank.upsert_contraption_subworld(
        CONTRAPTION_ID,
        [8.0, 20.0, 8.0],
        [0.0, 1.0, 0.0],
        angle,
        &mesh,
    );
    let mut batch = InstancedPartBatch::new(WHEEL_MESH_KIND, mesh.0.clone(), mesh.1.clone());
    batch.transform = SubWorldTransform {
        position: [8.0, 20.0, 8.0],
        ..SubWorldTransform::IDENTITY
    };
    batch.instances = wheel_instances(angle);
    bank.upsert_batch(batch);

    let blender = BiomeAtmosphereBlender::new(AtmosphereParams::default());
    let tracker = ChunkFadeTracker::new();
    let camera = Camera {
        position: [8.0, 28.0, 36.0],
        yaw_deg: 0.0,
        pitch_deg: -20.0,
        ..Camera::default()
    };
    let render = |renderer: &mut GpuRenderer, bank: &mut SubWorldRenderBank, ticks: u64| {
        renderer
            .render_offscreen_composed_frame(
                160,
                120,
                &camera,
                &mut ComposedFrame {
                    subworlds: bank,
                    lights: &[],
                    atmosphere: &blender,
                    chunk_fades: &tracker,
                    ui_rgba: None,
                    world_revision: 1,
                    world_time_ticks: ticks,
                },
            )
            .expect("coexistence composed frame")
    };

    let frame_a = render(&mut renderer, &mut bank, 6_000);
    // (c) frame stats: sub-world geometry resident + instanced draws.
    assert!(
        renderer.subworld_gpu_entry_count() > 0,
        "contraption sub-world must be GPU-resident"
    );
    assert!(
        renderer.instanced_kinetic_draws > 0,
        "the instanced water wheel must draw"
    );
    assert!(renderer.shadow_rerender_count >= 1, "shadow map must be live");
    let first = frame_a[0..4].to_vec();
    assert!(
        frame_a.chunks(4).any(|px| px != first.as_slice()),
        "frame is a flat color — nothing rendered"
    );

    // (b) advancing both spins moves the caster digest and re-renders
    // the shadow depth pass.
    let digest_a = renderer.shadow_caster_digest();
    angle += 0.6;
    bank.upsert_contraption_subworld(
        CONTRAPTION_ID,
        [8.0, 20.0, 8.0],
        [0.0, 1.0, 0.0],
        angle,
        &mesh,
    );
    assert!(bank.set_batch_instances(WHEEL_MESH_KIND, wheel_instances(angle)));
    assert_ne!(
        subworld_caster_digest(&bank),
        digest_a,
        "contraption + wheel spin must advance the caster digest"
    );
    let rerenders = renderer.shadow_rerender_count;
    render(&mut renderer, &mut bank, 6_000);
    assert!(
        renderer.shadow_rerender_count > rerenders,
        "live contraption rotation must re-render the shadow map"
    );

    // One more steady frame after the pack + shadows + water all ran:
    // still zero validation errors, still a live scene.
    render(&mut renderer, &mut bank, 6_000);
    assert!(renderer.instanced_kinetic_draws > 0);
    assert!(renderer.subworld_gpu_entry_count() > 0);

    let captured = errors.lock().expect("errors").clone();
    assert!(
        captured.is_empty(),
        "wgpu validation errors during coexistence render: {captured:#?}"
    );
}
