// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! Lane GPU-3 coverage: 6-DOF sub-world transforms, the atmosphere blender
//! and chunk-fade tracker, cloud/planet geometry bridges, WGSL validation of
//! the composed-pass modules, and a real headless `wgpu` render of the full
//! composed frame (terrain + sub-worlds + clouds + planet + VFX1 lights +
//! UI) with uncaptured-error accounting — skipped only when no adapter
//! exists at all.

use std::sync::{Arc, Mutex};

use mc_spine_client_gpu::atmosphere::{
    extruded_mesh_to_geometry, planet_body_to_geometry, planet_subworld_transform,
    voxel_clouds_to_geometry, AtmosphereParams, BiomeAtmosphereBlender, ChunkFadeTracker,
    CLOUD_RENDER_ALPHA,
};
use mc_spine_client_gpu::renderer::{
    probe_adapters, request_gpu_device, ComposedFrame, GpuRenderer, RendererError,
    OFFSCREEN_FORMAT,
};
use mc_spine_client_gpu::subworld_render::{
    box_mesh, quat_from_axis_angle, CompositeDrawUniforms, InstancedPartBatch,
    InstancedPartRecord, SubWorldRenderBank, SubWorldRenderEntry, SubWorldTransform,
    TerrainVertex, COMPOSITE_LIT_WGSL, COMPOSITE_TERRAIN_WGSL, DRAW_UNIFORM_STRIDE,
};
use mc_spine_client_gpu::{build_demo_scene, mat4_mul_vec4, Camera, Mat4};
use mc_spine_client_mesh::builtin_shadow_wgsl::{
    BuiltinShadowQuality, RenderScalePreset, ShadowCacheDecision,
};
use mc_spine_client_mesh::space_sky_bridge::{
    compute_sky_body_placement, derive_dimension_policy, ExternalOverrideFlags,
    PlanetDimensionMeta, PlanetSkyBody, SkyBodyPlacement,
};
use mc_spine_client_mesh::vanilla_plus_nature::{
    compute_cloud_lighting, generate_layered_voxel_clouds, VanillaPlusNatureConfig,
};
use mc_spine_client_mesh::vfx_engine::DynamicColoredLight;
use mc_spine_client_mesh::RenderBackendPreference;
use mc_spine_ui::draw::rasterize_draw_list;
use mc_spine_ui::state::{HudState, Screen, UiState};
use wgpu::naga::front::wgsl;
use wgpu::naga::valid::{Capabilities, ValidationFlags, Validator};
use wgpu::naga::ShaderStage;

// ---------------------------------------------------------------------------
// SubWorldTransform: quaternion -> column-major model matrix
// ---------------------------------------------------------------------------

fn quat_mul(a: [f32; 4], b: [f32; 4]) -> [f32; 4] {
    let [ax, ay, az, aw] = a;
    let [bx, by, bz, bw] = b;
    [
        aw * bx + ax * bw + ay * bz - az * by,
        aw * by - ax * bz + ay * bw + az * bx,
        aw * bz + ax * by - ay * bx + az * bw,
        aw * bw - ax * bx - ay * by - az * bz,
    ]
}

fn rotate_point(m: &Mat4, p: [f32; 3]) -> [f32; 3] {
    let v = mat4_mul_vec4(m, [p[0], p[1], p[2], 1.0]);
    [v[0], v[1], v[2]]
}

fn assert_vec3_near(a: [f32; 3], b: [f32; 3], eps: f32, ctx: &str) {
    for i in 0..3 {
        assert!(
            (a[i] - b[i]).abs() <= eps,
            "{ctx}: component {i} — {a:?} != {b:?}"
        );
    }
}

#[test]
fn subworld_matrix_orthogonal_basis() {
    // An arbitrary 6-DOF orientation: 37° pitch + 51° yaw + 23° roll.
    let q = quat_mul(
        quat_mul(
            quat_from_axis_angle([0.0, 1.0, 0.0], 51.0f32.to_radians()),
            quat_from_axis_angle([1.0, 0.0, 0.0], 37.0f32.to_radians()),
        ),
        quat_from_axis_angle([0.0, 0.0, 1.0], 23.0f32.to_radians()),
    );
    let transform = SubWorldTransform {
        position: [3.0, -7.5, 11.25],
        quaternion: q,
        scale: 1.0,
    };
    let m = transform.model_matrix();
    // The 3x3 basis columns must be orthonormal (pure rotation, no shear).
    for col in 0..3 {
        let len_sq: f32 = (0..3).map(|r| m[col][r] * m[col][r]).sum();
        assert!(
            (len_sq - 1.0).abs() < 1e-5,
            "column {col} not unit length: {len_sq}"
        );
        for other in (col + 1)..3 {
            let d: f32 = (0..3).map(|r| m[col][r] * m[other][r]).sum();
            assert!(d.abs() < 1e-5, "columns {col}/{other} not orthogonal: {d}");
        }
    }
    // Translation lands in the matrix's last column.
    assert_vec3_near(
        [m[3][0], m[3][1], m[3][2]],
        transform.position,
        1e-6,
        "translation column",
    );
}

#[test]
fn subworld_rotation_cardinal_and_arbitrary() {
    let origin_free = SubWorldTransform {
        position: [0.0; 3],
        scale: 1.0,
        quaternion: [0.0; 4],
    };
    // 90° about +Z sends +X to +Y.
    let t = SubWorldTransform {
        quaternion: quat_from_axis_angle([0.0, 0.0, 1.0], std::f32::consts::FRAC_PI_2),
        ..origin_free
    };
    assert_vec3_near(
        rotate_point(&t.model_matrix(), [1.0, 0.0, 0.0]),
        [0.0, 1.0, 0.0],
        1e-6,
        "Z+90 X->Y",
    );
    // 90° about +Y sends +X to -Z.
    let t = SubWorldTransform {
        quaternion: quat_from_axis_angle([0.0, 1.0, 0.0], std::f32::consts::FRAC_PI_2),
        ..origin_free
    };
    assert_vec3_near(
        rotate_point(&t.model_matrix(), [1.0, 0.0, 0.0]),
        [0.0, 0.0, -1.0],
        1e-6,
        "Y+90 X->-Z",
    );
    // 180° about +X flips Y and Z.
    let t = SubWorldTransform {
        quaternion: quat_from_axis_angle([1.0, 0.0, 0.0], std::f32::consts::PI),
        ..origin_free
    };
    assert_vec3_near(
        rotate_point(&t.model_matrix(), [0.0, 1.0, 0.5]),
        [0.0, -1.0, -0.5],
        1e-6,
        "X+180 flips YZ",
    );
    // 120° about the (1,1,1) diagonal cycles the axes: X->Y->Z->X.
    let t = SubWorldTransform {
        quaternion: quat_from_axis_angle(
            [1.0, 1.0, 1.0],
            120.0f32.to_radians(),
        ),
        ..origin_free
    };
    assert_vec3_near(
        rotate_point(&t.model_matrix(), [1.0, 0.0, 0.0]),
        [0.0, 1.0, 0.0],
        1e-5,
        "120 diagonal X->Y",
    );
    assert_vec3_near(
        rotate_point(&t.model_matrix(), [0.0, 0.0, 1.0]),
        [1.0, 0.0, 0.0],
        1e-5,
        "120 diagonal Z->X",
    );
    // Combined pitch+roll preserves lengths (rigid body).
    let q = quat_mul(
        quat_from_axis_angle([0.0, 0.0, 1.0], 30.0f32.to_radians()),
        quat_from_axis_angle([1.0, 0.0, 0.0], 45.0f32.to_radians()),
    );
    let t = SubWorldTransform {
        quaternion: q,
        scale: 2.5,
        position: [1.0, 2.0, 3.0],
    };
    let out = rotate_point(&t.model_matrix(), [0.4, -0.3, 0.8]);
    let d = [
        out[0] - 1.0,
        out[1] - 2.0,
        out[2] - 3.0,
    ];
    let len: f32 = d.iter().map(|v| v * v).sum::<f32>().sqrt();
    let src: f32 = [0.4f32, -0.3, 0.8].iter().map(|v| v * v).sum::<f32>().sqrt();
    assert!(
        (len - src * 2.5).abs() < 1e-4,
        "scaled rigid rotation preserves length: {len} vs {}",
        src * 2.5
    );
}

#[test]
fn subworld_invalid_quaternion_falls_back_to_identity() {
    for bad in [
        [0.0, 0.0, 0.0, 0.0],
        [f32::NAN, 0.0, 0.0, 1.0],
        [0.0, f32::INFINITY, 0.0, 1.0],
    ] {
        let t = SubWorldTransform {
            position: [0.0; 3],
            quaternion: bad,
            scale: 1.0,
        };
        let out = rotate_point(&t.model_matrix(), [1.0, 2.0, 3.0]);
        assert_vec3_near(out, [1.0, 2.0, 3.0], 1e-6, "invalid quat -> identity");
        assert!(t
            .model_matrix()
            .iter()
            .flatten()
            .all(|v| v.is_finite()));
    }
    // Non-finite scale/position fail closed too.
    let t = SubWorldTransform {
        position: [f32::NAN; 3],
        quaternion: [0.0; 4],
        scale: f32::NAN,
    };
    assert_vec3_near(
        rotate_point(&t.model_matrix(), [1.0, 2.0, 3.0]),
        [1.0, 2.0, 3.0],
        1e-6,
        "NaN transform -> identity at origin",
    );
}

// ---------------------------------------------------------------------------
// SubWorldRenderBank bookkeeping
// ---------------------------------------------------------------------------

#[test]
fn subworld_bank_dirty_and_transform_semantics() {
    let (verts, idx) = box_mesh([0.0, 0.0, 0.0], [1.0, 1.0, 1.0], [0.5; 3]);
    assert_eq!(verts.len(), 24);
    assert_eq!(idx.len(), 36);
    let mut bank = SubWorldRenderBank::new();
    bank.upsert(SubWorldRenderEntry::new(
        7,
        SubWorldTransform::IDENTITY,
        verts.clone(),
        idx.clone(),
    ));
    assert_eq!(bank.len(), 1);
    assert!(bank.get(7).expect("entry").dirty, "new entries upload");
    // A transform update alone never re-dirties a synced entry.
    let mut clean = SubWorldRenderEntry::new(8, SubWorldTransform::IDENTITY, verts.clone(), idx.clone());
    clean.dirty = false;
    bank.upsert(clean);
    assert!(bank.set_transform(8, SubWorldTransform {
        position: [1.0, 2.0, 3.0],
        ..SubWorldTransform::IDENTITY
    }));
    assert!(!bank.get(8).expect("entry").dirty);
    assert_eq!(bank.get(8).expect("entry").transform.position, [1.0, 2.0, 3.0]);
    assert!(!bank.set_transform(999, SubWorldTransform::default()));
    // Geometry replacement dirties again.
    assert!(bank.set_geometry(7, verts, idx));
    assert!(bank.get(7).expect("entry").dirty);
    // Instanced batch bookkeeping.
    let mut batch = InstancedPartBatch::new(
        42,
        vec![TerrainVertex::default()],
        vec![0, 0, 0],
    );
    batch.instances.push(InstancedPartRecord {
        pos_scale: [0.0, 0.0, 0.0, 1.0],
        rot_uv_frame: [0.0; 4],
        packed_light: 0xFF,
        tint_rgba: 0xFF_FF_FF_FF,
        mesh_kind: 42,
        subworld_id: 7,
    });
    bank.upsert_batch(batch);
    assert_eq!(bank.batches().count(), 1);
    assert!(bank.set_batch_instances(42, Vec::new()));
    assert!(bank.remove(7));
    assert_eq!(std::mem::size_of::<InstancedPartRecord>(), 48);
    assert_eq!(
        std::mem::size_of::<CompositeDrawUniforms>(),
        DRAW_UNIFORM_STRIDE as usize
    );
}

// ---------------------------------------------------------------------------
// BiomeAtmosphereBlender
// ---------------------------------------------------------------------------

fn assert_finite(params: &AtmosphereParams, ctx: &str) {
    let fields: [&[f32]; 6] = [
        &params.sky_zenith_rgb,
        &params.sky_horizon_rgb,
        &params.fog_rgb,
        &params.depth_tint_rgb,
        &params.ambient_rgb,
        &[params.fog_density, params.depth_tint_strength, params.sun_intensity],
    ];
    for (i, f) in fields.iter().enumerate() {
        assert!(
            f.iter().all(|v| v.is_finite()),
            "{ctx}: field {i} has non-finite value: {f:?}"
        );
    }
}

fn assert_transition_is_smooth(from: AtmosphereParams, to: AtmosphereParams, ctx: &str) {
    let mut blender = BiomeAtmosphereBlender::new(from);
    blender.set_target(to);
    let mut previous = blender.current();
    let frames = 600; // 10 simulated seconds at 60 fps
    for frame in 0..frames {
        let now = blender.advance(1.0 / 60.0);
        assert_finite(&now, ctx);
        // Every channel must move monotonically toward the target — no
        // overshoot, no reversal, and every step strictly smaller than the
        // remaining gap (no abrupt jumps).
        let channels = |p: &AtmosphereParams| -> Vec<f32> {
            let mut v = Vec::new();
            v.extend(p.sky_zenith_rgb);
            v.extend(p.sky_horizon_rgb);
            v.extend(p.fog_rgb);
            v.push(p.fog_density);
            v.extend(p.depth_tint_rgb);
            v.push(p.depth_tint_strength);
            v.extend(p.ambient_rgb);
            v.push(p.sun_intensity);
            v
        };
        let (pc, nc, tc) = (channels(&previous), channels(&now), channels(&to));
        for (i, ((p, n), t)) in pc.iter().zip(nc.iter()).zip(tc.iter()).enumerate() {
            let step = n - p;
            let gap_before = t - p;
            if gap_before.abs() < 1e-9 {
                assert!(
                    step.abs() < 1e-6,
                    "{ctx} frame {frame} channel {i}: moved after settling"
                );
                continue;
            }
            assert!(
                step.signum() == gap_before.signum() || step.abs() < 1e-9,
                "{ctx} frame {frame} channel {i}: reversed direction ({p} -> {n}, target {t})"
            );
            let gap_after = t - n;
            assert!(
                gap_after.abs() <= gap_before.abs() + 1e-6,
                "{ctx} frame {frame} channel {i}: overshot target"
            );
        }
        previous = now;
    }
    assert!(
        blender.is_settled(1e-3),
        "{ctx}: blend never converged after 10 simulated seconds"
    );
}

#[test]
fn atmosphere_blender_forest_desert_orbit_is_smooth() {
    // Forest (overworld default) -> hot desert (dimmer horizon, warmer tint)
    // -> lunar vacuum orbit.
    let forest = AtmosphereParams::default();
    let mut desert = AtmosphereParams::default();
    desert.sky_horizon_rgb = [0.92, 0.78, 0.55];
    desert.fog_rgb = desert.sky_horizon_rgb;
    desert.fog_density = 0.0012;
    desert.ambient_rgb = [0.50, 0.44, 0.34];
    let orbit = AtmosphereParams::vacuum_orbit();
    assert_transition_is_smooth(forest, desert, "forest->desert");
    assert_transition_is_smooth(desert, orbit, "desert->orbit");
    assert_transition_is_smooth(forest, orbit, "forest->orbit");
    // NaN targets sanitize instead of poisoning the blend.
    let mut blender = BiomeAtmosphereBlender::new(forest);
    let mut bad = forest;
    bad.fog_rgb = [f32::NAN; 3];
    bad.fog_density = f32::INFINITY;
    blender.set_target(bad);
    let now = blender.advance(0.5);
    assert_finite(&now, "sanitized target");
}

// ---------------------------------------------------------------------------
// ChunkFadeTracker
// ---------------------------------------------------------------------------

#[test]
fn chunk_fade_is_monotonic_and_cleans_up() {
    let mut tracker = ChunkFadeTracker::new();
    assert!((tracker.fade_alpha(3, -5) - 1.0).abs() < 1e-9);
    assert!(tracker.note_chunk(3, -5));
    assert!(!tracker.note_chunk(3, -5), "re-noting keeps progress");
    assert_eq!(tracker.fade_alpha(3, -5), 0.0);
    let mut prev = 0.0f32;
    // 0.25 s fade in 10 ms steps: strictly non-decreasing, ends at 1.0.
    for i in 1..=25 {
        tracker.tick(0.01);
        let a = tracker.fade_alpha(3, -5);
        assert!(
            (0.0..=1.0).contains(&a) && a.is_finite(),
            "step {i}: alpha {a}"
        );
        assert!(a >= prev, "step {i}: regressed {prev} -> {a}");
        prev = a;
    }
    assert_eq!(prev, 1.0, "fade completes at 0.25 s");
    assert!(!tracker.is_fading(3, -5));
    // Unload cleanup: the key frees and re-noting restarts at 0.
    assert!(tracker.forget(3, -5));
    assert_eq!(tracker.fade_alpha(3, -5), 1.0, "untracked = opaque");
    assert!(tracker.note_chunk(3, -5));
    assert_eq!(tracker.fade_alpha(3, -5), 0.0);
    // retain() prunes vanished chunks; non-finite dt is a no-op.
    tracker.tick(f32::NAN);
    tracker.retain(|_| false);
    assert!(tracker.is_empty());
}

// ---------------------------------------------------------------------------
// Cloud / planet geometry bridges
// ---------------------------------------------------------------------------

fn assert_geometry_finite(verts: &[TerrainVertex], idx: &[u32], ctx: &str) {
    assert!(!verts.is_empty(), "{ctx}: no vertices");
    assert!(
        !idx.is_empty() && idx.len().is_multiple_of(3),
        "{ctx}: no triangles"
    );
    for (i, v) in verts.iter().enumerate() {
        assert!(
            v.position
                .iter()
                .chain(v.normal.iter())
                .chain(v.albedo.iter())
                .all(|c| c.is_finite()),
            "{ctx}: vertex {i} not finite: {v:?}"
        );
    }
    let max = *idx.iter().max().unwrap() as usize;
    assert!(max < verts.len(), "{ctx}: index {max} out of range");
}

fn cloud_config() -> VanillaPlusNatureConfig {
    VanillaPlusNatureConfig {
        enabled: true,
        layered_voxel_clouds: true,
        cloud_layers: 3,
        cloud_height: 192.0,
        ..VanillaPlusNatureConfig::default()
    }
}

#[test]
fn cloud_and_planet_geometry_is_finite_and_non_empty() {
    let cells = generate_layered_voxel_clouds(&cloud_config(), 8.0, 30.0, 6000);
    assert!(!cells.is_empty(), "cloud field should emit cells");
    let lighting = compute_cloud_lighting(0.8, 0.0, 0.0);
    let (verts, idx) = voxel_clouds_to_geometry(&cells, &lighting);
    assert_geometry_finite(&verts, &idx, "clouds");
    // Every emitted vertex albedo comes from the cloud lighting table.
    assert!(verts.iter().all(|v| v.albedo.iter().all(|c| (0.0..=1.05).contains(c))));

    let planet = PlanetSkyBody {
        id: "ad_astra:earth".to_string(),
        angular_radius_deg: 12.0,
        orbit_period_ticks: 0,
        orbit_phase_offset: 0.0,
        orbit_inclination_deg: 20.0,
        surface_rgb: [0.2, 0.5, 0.9],
        atmosphere_rim_rgb: Some([0.5, 0.75, 1.0]),
        ring: None,
        is_below_horizon_parent: false,
    };
    let (verts, idx) = planet_body_to_geometry(&planet);
    assert_geometry_finite(&verts, &idx, "planet");
    let placement = compute_sky_body_placement(&planet, 6000, 64.0);
    let transform = planet_subworld_transform(&placement, 256.0);
    assert!(transform.scale.is_finite() && transform.scale > 0.0);
    assert!(transform.position.iter().all(|v| v.is_finite()));
}

#[test]
fn vacuum_policy_suppresses_clouds_but_keeps_parent_planet() {
    let parent = PlanetSkyBody {
        id: "ad_astra:earth".to_string(),
        angular_radius_deg: 30.0,
        orbit_period_ticks: 0,
        orbit_phase_offset: 0.0,
        orbit_inclination_deg: 0.0,
        surface_rgb: [0.2, 0.45, 0.85],
        atmosphere_rim_rgb: Some([0.4, 0.6, 0.95]),
        ring: None,
        is_below_horizon_parent: true,
    };
    let meta = PlanetDimensionMeta {
        dimension_id: "ad_astra:earth_orbit".to_string(),
        gravity_scale: 0.0,
        atmosphere_density: 0.0,
        has_oxygen: false,
        has_sky_light: true,
        is_orbit: true,
        sky_tint_rgb: [1.0; 3],
        star_brightness_boost: 2.0,
        parent_body: Some(parent),
        sky_bodies: Vec::new(),
    };
    let policy = derive_dimension_policy(Some(&meta), ExternalOverrideFlags::default());
    assert!(policy.is_orbit);
    assert!(!policy.enable_voxel_clouds, "vacuum disables voxel clouds");
    assert!(policy.render_builtin_planets);
    let parent = policy
        .parent_body
        .as_ref()
        .expect("orbit keeps its parent planet");

    // The generator agrees: a disabled config emits zero cells.
    let config = VanillaPlusNatureConfig {
        enabled: true,
        layered_voxel_clouds: policy.enable_voxel_clouds,
        ..VanillaPlusNatureConfig::default()
    };
    let cells = generate_layered_voxel_clouds(&config, 0.0, 0.0, 0);
    let (cloud_verts, cloud_idx) =
        voxel_clouds_to_geometry(&cells, &compute_cloud_lighting(1.0, 0.0, 0.0));
    assert_eq!(cells.len(), 0);
    assert!(cloud_verts.is_empty() && cloud_idx.is_empty());

    // ...while the parent planet still emits solid 3D geometry.
    let (verts, idx) = planet_body_to_geometry(parent);
    assert_geometry_finite(&verts, &idx, "vacuum parent planet");

    // Vanilla-safe default leaves clouds enabled and no planets.
    let vanilla = derive_dimension_policy(None, ExternalOverrideFlags::default());
    assert!(vanilla.enable_voxel_clouds);
    assert!(!vanilla.render_builtin_planets);
}

// ---------------------------------------------------------------------------
// WGSL validation through the real naga frontend
// ---------------------------------------------------------------------------

fn compile_and_validate(name: &str, source: &str) -> wgpu::naga::Module {
    let module = wgsl::parse_str(source).unwrap_or_else(|err| {
        panic!("{name}: naga WGSL parse failed:\n{}", err.emit_to_string(source))
    });
    Validator::new(ValidationFlags::all(), Capabilities::all())
        .validate(&module)
        .unwrap_or_else(|err| {
            panic!("{name}: naga validation failed:\n{}", err.emit_to_string(source))
        });
    module
}

#[test]
fn composite_wgsl_modules_validate_with_naga() {
    let terrain = compile_and_validate("COMPOSITE_TERRAIN_WGSL", COMPOSITE_TERRAIN_WGSL);
    let lit = compile_and_validate("COMPOSITE_LIT_WGSL", COMPOSITE_LIT_WGSL);
    let has_entry_point = |m: &wgpu::naga::Module, stage: ShaderStage, name: &str| {
        m.entry_points
            .iter()
            .any(|ep| ep.stage == stage && ep.name == name)
    };
    for (stage, name) in [
        (ShaderStage::Vertex, "terrain_vs"),
        (ShaderStage::Fragment, "terrain_fs"),
        (ShaderStage::Vertex, "terrain_translucent_vs"),
        (ShaderStage::Fragment, "terrain_translucent_fs"),
    ] {
        assert!(
            has_entry_point(&terrain, stage, name),
            "terrain module lost {name}"
        );
    }
    for (stage, name) in [
        (ShaderStage::Vertex, "lit_vs"),
        (ShaderStage::Vertex, "inst_vs"),
        (ShaderStage::Fragment, "lit_fs"),
    ] {
        assert!(
            has_entry_point(&lit, stage, name),
            "lit module lost {name}"
        );
    }
}

// ---------------------------------------------------------------------------
// Real wgpu composed-frame render (skips only with zero adapters)
// ---------------------------------------------------------------------------

fn count_diff_pixels(a: &[u8], b: &[u8]) -> usize {
    a.as_chunks::<4>()
        .0
        .iter()
        .zip(b.as_chunks::<4>().0.iter())
        .filter(|(pa, pb)| {
            (0..3)
                .map(|c| (pa[c] as i32 - pb[c] as i32).abs())
                .sum::<i32>()
                > 12
        })
        .count()
}

fn assert_nonblank_frame(pixels: &[u8], ctx: &str) {
    let (rgba, tail) = pixels.as_chunks::<4>();
    assert!(tail.is_empty(), "{ctx}: readback length not multiple of 4");
    let first = rgba[0];
    assert!(
        rgba.iter().any(|px| *px != first),
        "{ctx}: frame is a flat color — nothing rendered"
    );
    assert!(
        rgba.iter().all(|px| px[3] == 255),
        "{ctx}: every pixel must end opaque"
    );
}

#[test]
fn composed_frame_renders_on_real_gpu() {
    let adapters = probe_adapters(RenderBackendPreference::Auto);
    if adapters.is_empty() {
        eprintln!("SKIP composed render: probe_adapters found no adapters");
        return;
    }
    let ctx = match request_gpu_device(RenderBackendPreference::Auto, None, None) {
        Ok(ctx) => ctx,
        Err(RendererError::NoAdapter(msg)) => {
            eprintln!("SKIP composed render: no wgpu adapter ({msg})");
            return;
        }
        Err(e) => panic!("unexpected device error: {e}"),
    };
    eprintln!(
        "composed render on {:?} {} ({})",
        ctx.report.backend, ctx.report.name, ctx.report.resolution.backend.as_str()
    );
    // Capture every uncaptured wgpu error; validation errors land here.
    let errors: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    {
        let errors = Arc::clone(&errors);
        ctx.device.on_uncaptured_error(Arc::new(move |e| {
            errors.lock().expect("errors").push(e.to_string());
        }));
    }

    let mut renderer = GpuRenderer::new(ctx, OFFSCREEN_FORMAT);
    let negotiated = renderer.set_shadow_quality(BuiltinShadowQuality::High);
    assert!(negotiated <= BuiltinShadowQuality::High);
    renderer.set_render_scale(RenderScalePreset::Native100);
    // Dirty-only shadow cache: the first frame re-renders, unchanged inputs
    // on the next frame reuse the cached depth pass.
    let camera_for_cache = Camera::default();
    assert_eq!(
        renderer.shadow_decision(&camera_for_cache, 7, 6000),
        ShadowCacheDecision::Rerender(
            mc_spine_client_mesh::builtin_shadow_wgsl::ShadowDirtyReason::FirstFrame
        )
    );
    assert_eq!(
        renderer.shadow_decision(&camera_for_cache, 7, 6000),
        ShadowCacheDecision::ReuseCached
    );

    let scene = build_demo_scene([8.0, 14.0, 30.0]);
    renderer.upload_scene(&scene).expect("upload demo scene");

    // 6-DOF Aeronautics airship: pitched + rolled hull floating in view.
    let mut bank = SubWorldRenderBank::new();
    let (hull_v, hull_i) = box_mesh([-3.0, -1.0, -2.0], [3.0, 1.0, 2.0], [0.72, 0.55, 0.32]);
    let (deck_v, deck_i) = box_mesh([-2.0, 1.0, -1.0], [2.0, 2.4, 1.0], [0.85, 0.8, 0.7]);
    let mut hull_verts = hull_v;
    hull_verts.extend_from_slice(&deck_v);
    let mut hull_indices = hull_i;
    hull_indices.extend(deck_i.iter().map(|i| i + 24));
    let pitched_rolled = quat_mul(
        quat_from_axis_angle([0.0, 0.0, 1.0], 12.0f32.to_radians()),
        quat_from_axis_angle([1.0, 0.0, 0.0], -15.0f32.to_radians()),
    );
    bank.upsert(SubWorldRenderEntry::new(
        1,
        SubWorldTransform {
            position: [8.0, 24.0, 12.0],
            quaternion: pitched_rolled,
            scale: 1.0,
        },
        hull_verts,
        hull_indices,
    ));

    // Layered voxel clouds, low enough to sit in the top of the frame.
    let cloud_cells = generate_layered_voxel_clouds(
        &VanillaPlusNatureConfig {
            enabled: true,
            layered_voxel_clouds: true,
            cloud_layers: 2,
            cloud_height: 52.0,
            ..VanillaPlusNatureConfig::default()
        },
        8.0,
        30.0,
        6000,
    );
    assert!(!cloud_cells.is_empty(), "cloud field must emit cells");
    let (cloud_v, cloud_i) =
        voxel_clouds_to_geometry(&cloud_cells, &compute_cloud_lighting(0.9, 0.0, 0.0));
    let mut cloud_entry =
        SubWorldRenderEntry::new(2, SubWorldTransform::IDENTITY, cloud_v, cloud_i);
    cloud_entry.alpha = CLOUD_RENDER_ALPHA;
    bank.upsert(cloud_entry);

    // Cubic parent planet hanging in the sky at a shallow elevation.
    let planet = PlanetSkyBody {
        id: "lane-gpu3:parent".to_string(),
        angular_radius_deg: 10.0,
        orbit_period_ticks: 0,
        orbit_phase_offset: 0.0,
        orbit_inclination_deg: 45.0,
        surface_rgb: [0.25, 0.5, 0.9],
        atmosphere_rim_rgb: Some([0.5, 0.75, 1.0]),
        ring: None,
        is_below_horizon_parent: false,
    };
    let (planet_v, planet_i) = planet_body_to_geometry(&planet);
    let placement = mc_spine_client_mesh::space_sky_bridge::SkyBodyPlacement {
        direction: [0.0, 0.35, -1.0],
        apparent_angular_radius_deg: 10.0,
        sun_direction: [0.0, 1.0, 0.0],
        orbit_fraction: 0.0,
        above_horizon: true,
    };
    bank.upsert(SubWorldRenderEntry::new(
        3,
        planet_subworld_transform(&placement, 300.0),
        planet_v,
        planet_i,
    ));

    // GPU-instanced mechanical parts (cogwheel teeth) riding the airship.
    let (cog_v, cog_i) = box_mesh([-0.5, -0.5, -0.25], [0.5, 0.5, 0.25], [0.9, 0.7, 0.2]);
    let mut batch = InstancedPartBatch::new(9, cog_v, cog_i);
    batch.transform = SubWorldTransform {
        position: [8.0, 24.0, 12.0],
        quaternion: pitched_rolled,
        scale: 1.0,
    };
    for i in 0..8 {
        let a = i as f32 * 0.8;
        batch.instances.push(InstancedPartRecord {
            pos_scale: [a.cos() * 4.5, 0.0, a.sin() * 4.5, 1.0],
            rot_uv_frame: [0.0, a, 0.0, 0.0],
            packed_light: 0xFF,
            tint_rgba: 0xFF_FF_FF_FF,
            mesh_kind: 9,
            subworld_id: 1,
        });
    }
    bank.upsert_batch(batch);

    // VFX1 dynamic colored light hovering near the airship.
    let lights = [DynamicColoredLight {
        pos: [8.0, 26.0, 14.0],
        rgb: [1.0, 0.45, 0.15],
        radius: 48.0,
        intensity: 6.0,
        ttl_ticks: 20,
        priority: 0,
    }];

    let blender = BiomeAtmosphereBlender::new(AtmosphereParams::default());
    let tracker = ChunkFadeTracker::new();
    let mut ui = UiState::new();
    ui.screen = Screen::InGameHud;
    ui.hud = HudState::demo();
    let ui_rgba = rasterize_draw_list(160, 120, &ui.render(160, 120));

    let camera = Camera {
        position: [8.0, 26.0, 32.0],
        yaw_deg: 0.0,
        pitch_deg: -10.0,
        ..Camera::default()
    };

    let frame_a = {
        let mut frame = ComposedFrame {
            subworlds: &mut bank,
            lights: &lights,
            atmosphere: &blender,
            chunk_fades: &tracker,
            ui_rgba: Some(&ui_rgba),
            world_revision: 7,
            world_time_ticks: 6000,
        };
        renderer
            .render_offscreen_composed_frame(160, 120, &camera, &mut frame)
            .expect("composed frame A")
    };
    assert_eq!(renderer.subworld_gpu_entry_count(), 3);
    assert_eq!(renderer.subworld_gpu_batch_count(), 1);
    assert_nonblank_frame(&frame_a, "composed frame A");

    // Rotate the airship 45° about Y and shift it: the frame must differ.
    assert!(bank.set_transform(
        1,
        SubWorldTransform {
            position: [9.5, 24.5, 12.0],
            quaternion: quat_mul(
                quat_from_axis_angle([0.0, 1.0, 0.0], 45.0f32.to_radians()),
                pitched_rolled,
            ),
            scale: 1.0,
        },
    ));
    let frame_b = {
        let mut frame = ComposedFrame {
            subworlds: &mut bank,
            lights: &lights,
            atmosphere: &blender,
            chunk_fades: &tracker,
            ui_rgba: Some(&ui_rgba),
            world_revision: 7,
            world_time_ticks: 6000,
        };
        renderer
            .render_offscreen_composed_frame(160, 120, &camera, &mut frame)
            .expect("composed frame B")
    };
    assert_nonblank_frame(&frame_b, "composed frame B");
    let diff = count_diff_pixels(&frame_a, &frame_b);
    assert!(
        diff >= 32,
        "rotating the airship 45° must move pixels (got {diff})"
    );

    // Reduced render scale still composites (world at 75%, UI at native).
    renderer.set_render_scale(RenderScalePreset::Balanced75);
    let frame_c = {
        let mut frame = ComposedFrame {
            subworlds: &mut bank,
            lights: &lights,
            atmosphere: &blender,
            chunk_fades: &tracker,
            ui_rgba: Some(&ui_rgba),
            world_revision: 7,
            world_time_ticks: 6000,
        };
        renderer
            .render_offscreen_composed_frame(160, 120, &camera, &mut frame)
            .expect("composed frame C (75% world scale)")
    };
    assert_nonblank_frame(&frame_c, "scaled composed frame");

    let captured = errors.lock().expect("errors").clone();
    assert!(
        captured.is_empty(),
        "wgpu/Vulkan validation errors during composed render: {captured:#?}"
    );
}

#[test]
fn atmosphere_params_sanitized_bounds_rgb_and_fields() {
    let hostile = AtmosphereParams {
        sky_zenith_rgb: [-1.0, 100.0, f32::NAN],
        sky_horizon_rgb: [f32::INFINITY, -5.0, 2.0],
        fog_rgb: [0.5, 0.5, 0.5],
        fog_density: f32::NAN,
        depth_tint_rgb: [9.0, -9.0, 1.0],
        depth_tint_strength: 7.0,
        ambient_rgb: [f32::NEG_INFINITY, 0.5, 0.5],
        sun_intensity: -3.0,
    };
    let s = hostile.sanitized();
    // Non-finite channels fall back per-channel to the neutral default,
    // then every rgb channel is bounded to the linear 0..=4 range.
    assert_eq!(s.sky_zenith_rgb, [0.0, 4.0, 0.98]);
    assert_eq!(s.sky_horizon_rgb, [0.62, 0.0, 2.0]);
    assert_eq!(s.fog_rgb, [0.5, 0.5, 0.5]);
    assert_eq!(s.depth_tint_rgb, [4.0, 0.0, 1.0]);
    assert_eq!(s.depth_tint_strength, 1.0);
    assert_eq!(s.ambient_rgb[0], 0.42);
    assert_eq!(s.sun_intensity, 0.0);
    assert!(s.fog_density.is_finite() && s.fog_density >= 0.0);
    for channel in s
        .sky_zenith_rgb
        .iter()
        .chain(s.sky_horizon_rgb.iter())
        .chain(s.fog_rgb.iter())
        .chain(s.depth_tint_rgb.iter())
        .chain(s.ambient_rgb.iter())
    {
        assert!(channel.is_finite() && (0.0..=4.0).contains(channel));
    }
}

#[test]
fn chunk_fade_tracker_is_bounded_and_recycles_finished_fades() {
    let mut tracker = ChunkFadeTracker::new();
    // Saturate the hard cap with in-flight fades: further notes are refused
    // so the map can never grow without bound.
    let mut noted = 0usize;
    for i in 0..20000i32 {
        if tracker.note_chunk(i, 0) {
            noted += 1;
        }
    }
    assert!(noted <= 8192, "tracker exceeded its hard cap: {noted}");
    assert_eq!(tracker.len(), noted);
    assert!(!tracker.note_chunk(-1, -1), "saturated tracker must refuse");

    // Once every fade has finished, the next note recycles the dead entries
    // instead of refusing — finished fades are observationally identical to
    // untracked chunks.
    tracker.tick(1.0);
    assert!(tracker.note_chunk(-1, -1));
    assert_eq!(tracker.len(), 1);
    assert_eq!(tracker.fade_alpha(-1, -1), 0.0);
}

#[test]
fn voxel_cloud_geometry_sanitizes_lighting_and_degenerate_cells() {
    use mc_spine_client_mesh::vanilla_plus_nature::{CloudLighting, VoxelCloudCell, ALL_FACES};
    let lighting = CloudLighting {
        top_rgb: [f32::NAN, -0.5, 0.8],
        side_rgb: [1.0, 1.0, 1.0],
        bottom_rgb: [0.2, 0.2, 0.2],
        rim_warmth: 0.0,
    };
    let good = VoxelCloudCell {
        min: [0.0, 0.0, 0.0],
        size: [12.0, 4.0, 12.0],
        layer: 0,
        exposed_faces: ALL_FACES,
    };
    let zero_size = VoxelCloudCell {
        size: [0.0, 4.0, 12.0],
        ..good
    };
    let negative_size = VoxelCloudCell {
        size: [-1.0, 4.0, 12.0],
        ..good
    };
    let nan_min = VoxelCloudCell {
        min: [f32::NAN, 0.0, 0.0],
        ..good
    };
    let cells = [good, zero_size, negative_size, nan_min];
    let (verts, indices) = voxel_clouds_to_geometry(&cells, &lighting);
    // Only the well-formed cell contributes: 6 exposed faces x 4 vertices.
    assert_eq!(verts.len(), 24);
    assert_eq!(indices.len(), 36);
    for v in &verts {
        assert!(
            v.albedo
                .iter()
                .all(|c| c.is_finite() && (0.0..=1.0).contains(c)),
            "cloud albedo escaped bounds: {:?}",
            v.albedo
        );
    }
}

#[test]
fn extruded_mesh_to_geometry_skips_non_finite_quads_and_bounds_albedo() {
    use mc_spine_client_mesh::vanilla_plus_3d::{ExtrudedMesh, ExtrudedQuad};
    let quad = |positions: [[f32; 3]; 4]| ExtrudedQuad {
        positions,
        normal: [0.0, 0.0, 1.0],
        uv: [[0.0, 0.0]; 4],
        tint_index: 0,
    };
    let mut mesh = ExtrudedMesh::new();
    mesh.quads.push(quad([
        [0.0, 0.0, 0.0],
        [1.0, 0.0, 0.0],
        [1.0, 1.0, 0.0],
        [0.0, 1.0, 0.0],
    ]));
    mesh.quads.push(quad([[f32::NAN, 0.0, 0.0]; 4]));
    let (verts, indices) = extruded_mesh_to_geometry(&mesh, |_| [2.0, -1.0, f32::NAN]);
    assert_eq!(verts.len(), 4, "non-finite quad must be skipped");
    assert_eq!(indices.len(), 6);
    assert_eq!(verts[0].albedo, [1.0, 0.0, 0.0]);
}

#[test]
fn planet_subworld_transform_survives_hostile_placement() {
    let hostile = SkyBodyPlacement {
        direction: [0.0, 0.0, 0.0],
        apparent_angular_radius_deg: f32::NAN,
        sun_direction: [0.0, 1.0, 0.0],
        orbit_fraction: 0.0,
        above_horizon: true,
    };
    let t = planet_subworld_transform(&hostile, f32::NAN);
    assert!(t.scale.is_finite() && t.scale > 0.0);
    assert!(t
        .position
        .iter()
        .chain(t.quaternion.iter())
        .all(|c| c.is_finite()));

    // A non-unit direction must be normalized: the body centre lands
    // `distance` blocks along the unit direction, not 5x farther out.
    let mut placement = hostile;
    placement.direction = [3.0, 0.0, 4.0];
    placement.apparent_angular_radius_deg = 10.0;
    let t = planet_subworld_transform(&placement, 100.0);
    let half = t.scale * 0.5;
    let center = [
        t.position[0] + half,
        t.position[1] + half,
        t.position[2] + half,
    ];
    let len = (center[0] * center[0] + center[1] * center[1] + center[2] * center[2]).sqrt();
    assert!(
        (len - 100.0).abs() < 1e-3,
        "center not on the sky sphere: {center:?}"
    );
    assert!((center[0] / len - 0.6).abs() < 1e-4);
    assert!((center[2] / len - 0.8).abs() < 1e-4);
}
