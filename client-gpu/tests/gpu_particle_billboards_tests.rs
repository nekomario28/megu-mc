// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! GPU-instanced particle billboards (L29): CPU-side `ParticleInstance`
//! packing plus real-adapter readback comparing the instanced draw to
//! the CPU-staged per-frame quad path it replaces.

use megu_client_gpu::atmosphere::{AtmosphereParams, BiomeAtmosphereBlender, ChunkFadeTracker};
use megu_client_gpu::renderer::{
    probe_adapters, request_gpu_device, ComposedFrame, GpuRenderer, OFFSCREEN_FORMAT,
};
use megu_client_gpu::subworld_render::{
    pack_particle_billboard, ParticleBillboardPack, ParticleInstance, SubWorldRenderBank,
    PARTICLE_BILLBOARD_CORNERS,
};
use megu_client_gpu::Camera;
use megu_client_mesh::ber_instancing::{
    VanillaParticleKind, VanillaParticleQuad, VanillaParticleWorld,
};
use megu_client_mesh::RenderBackendPreference;

fn sample_quad() -> VanillaParticleQuad {
    VanillaParticleQuad {
        kind: VanillaParticleKind::Flame,
        positions: [
            [-1.0, -1.0, 2.0],
            [3.0, -1.0, 2.0],
            [3.0, 1.0, 2.0],
            [-1.0, 1.0, 2.0],
        ],
        uv: [[0.0, 1.0], [1.0, 1.0], [1.0, 0.0], [0.0, 0.0]],
        color: [0.5, 0.25, 0.75, 0.8],
    }
}

fn expand(instance: &ParticleInstance, corner: [f32; 2]) -> [f32; 3] {
    [
        instance.center[0] + corner[0] * instance.axis_u[0] + corner[1] * instance.axis_v[0],
        instance.center[1] + corner[0] * instance.axis_u[1] + corner[1] * instance.axis_v[1],
        instance.center[2] + corner[0] * instance.axis_u[2] + corner[1] * instance.axis_v[2],
    ]
}

fn assert_vec3_close(actual: [f32; 3], expected: [f32; 3], what: &str) {
    for axis in 0..3 {
        assert!(
            (actual[axis] - expected[axis]).abs() < 1e-5,
            "{what}: {actual:?} != {expected:?}"
        );
    }
}

#[test]
fn particle_instance_layout_and_packing() {
    assert_eq!(
        std::mem::size_of::<ParticleInstance>(),
        76,
        "billboard instance must stay 76 bytes"
    );

    // A clean parallelogram packs; the shader's corner expansion must
    // reproduce every original CPU-expanded position.
    let instanced = match pack_particle_billboard(&sample_quad()) {
        ParticleBillboardPack::Instanced(instance) => instance,
        other => panic!("sample quad must pack: {other:?}"),
    };
    assert_vec3_close(instanced.center, [1.0, 0.0, 2.0], "center");
    assert_vec3_close(instanced.axis_u, [2.0, 0.0, 0.0], "axis_u");
    assert_vec3_close(instanced.axis_v, [0.0, 1.0, 0.0], "axis_v");
    assert_eq!(instanced.color, [0.5, 0.25, 0.75, 0.8]);
    for (corner, position) in PARTICLE_BILLBOARD_CORNERS
        .iter()
        .zip(sample_quad().positions.iter())
    {
        assert_vec3_close(expand(&instanced, *corner), *position, "expanded corner");
    }
    // The affine UV basis reproduces each corner's UV exactly:
    // `uv = uv_origin + c.x*uv_du + c.y*uv_dv`, `c = (corner + 1) / 2`.
    for (corner, uv) in PARTICLE_BILLBOARD_CORNERS
        .iter()
        .zip(sample_quad().uv.iter())
    {
        let c = [corner[0] * 0.5 + 0.5, corner[1] * 0.5 + 0.5];
        let computed = [
            instanced.uv_origin[0] + c[0] * instanced.uv_du[0] + c[1] * instanced.uv_dv[0],
            instanced.uv_origin[1] + c[0] * instanced.uv_du[1] + c[1] * instanced.uv_dv[1],
        ];
        assert!(
            (computed[0] - uv[0]).abs() < 1e-6 && (computed[1] - uv[1]).abs() < 1e-6,
            "uv {computed:?} != {uv:?}"
        );
    }

    // Degenerate / non-finite quads are skipped on both paths.
    let mut quad = sample_quad();
    quad.positions = [[1.0; 3]; 4];
    assert_eq!(
        pack_particle_billboard(&quad),
        ParticleBillboardPack::Skip,
        "degenerate quad"
    );
    let mut quad = sample_quad();
    quad.color[0] = f32::NAN;
    assert_eq!(
        pack_particle_billboard(&quad),
        ParticleBillboardPack::Skip,
        "non-finite color"
    );
    let mut quad = sample_quad();
    quad.positions[2][0] = f32::INFINITY;
    assert_eq!(
        pack_particle_billboard(&quad),
        ParticleBillboardPack::Skip,
        "non-finite position"
    );

    // Renderable but outside the bilinear model -> CPU path only.
    let mut quad = sample_quad();
    quad.positions[2][0] += 1.5;
    assert_eq!(
        pack_particle_billboard(&quad),
        ParticleBillboardPack::CpuOnly,
        "non-parallelogram"
    );
    let mut quad = sample_quad();
    quad.uv[2] = [0.5, 0.5];
    assert_eq!(
        pack_particle_billboard(&quad),
        ParticleBillboardPack::CpuOnly,
        "non-affine uv"
    );
}

#[test]
fn billboard_axes_and_half_size_match_quad_expansion() {
    // The client-port helpers the instanced world path packs with must
    // reproduce `billboard_quads`' corner positions exactly.
    let mut world = VanillaParticleWorld::new();
    for (i, z) in [4.0, 6.0, 8.0].iter().enumerate() {
        assert!(world.spawn(
            VanillaParticleKind::Flame,
            [8.0 + i as f32, 3.0 + i as f32 * 0.5, *z],
            [0.0; 3],
            [1.0, 0.25, 0.1, 0.8],
            2.0,
        ));
    }
    world.step(0.25);
    let camera = Camera::default();
    let right = camera.right_vector();
    let up = [0.0, 1.0, 0.0];
    let quads = world.billboard_quads(right, up);
    assert_eq!(quads.len(), 3);

    let (axis_right, axis_up) = VanillaParticleWorld::billboard_axes(right, up);
    for (particle, quad) in world.particles().iter().zip(quads.iter()) {
        let size = particle.billboard_half_size();
        let instance = ParticleInstance {
            center: particle.pos,
            axis_u: axis_right.map(|r| r * size),
            axis_v: axis_up.map(|u| u * size),
            uv_origin: [0.0, 1.0],
            uv_du: [1.0, 0.0],
            uv_dv: [0.0, -1.0],
            color: particle.color,
        };
        for (corner, position) in PARTICLE_BILLBOARD_CORNERS
            .iter()
            .zip(quad.positions.iter())
        {
            assert_vec3_close(expand(&instance, *corner), *position, "world-path corner");
        }
        // And the quad itself packs losslessly for `set_particle_billboards`.
        match pack_particle_billboard(quad) {
            ParticleBillboardPack::Instanced(packed) => {
                assert_vec3_close(packed.center, particle.pos, "packed center");
            }
            other => panic!("billboard quad must pack: {other:?}"),
        }
    }
}

fn try_gpu_renderer() -> Option<GpuRenderer> {
    if probe_adapters(RenderBackendPreference::Auto).is_empty() {
        eprintln!("SKIP billboard GPU test: probe_adapters found 0 adapters");
        return None;
    }
    let ctx = request_gpu_device(RenderBackendPreference::Auto, None, None)
        .expect("probe found adapters but request_gpu_device failed");
    Some(GpuRenderer::new(ctx, OFFSCREEN_FORMAT))
}

fn render(renderer: &mut GpuRenderer, camera: &Camera, revision: u64) -> Vec<u8> {
    let mut bank = SubWorldRenderBank::new();
    let atmosphere = BiomeAtmosphereBlender::new(AtmosphereParams::default());
    let fades = ChunkFadeTracker::new();
    renderer
        .render_offscreen_composed_frame(
            256,
            256,
            camera,
            &mut ComposedFrame {
                subworlds: &mut bank,
                lights: &[],
                atmosphere: &atmosphere,
                chunk_fades: &fades,
                ui_rgba: None,
                world_revision: revision,
                world_time_ticks: 6000,
            },
        )
        .expect("offscreen GPU readback")
}

fn changed_pixels(a: &[u8], b: &[u8], min_delta: u8) -> usize {
    a.as_chunks::<4>()
        .0
        .iter()
        .zip(b.as_chunks::<4>().0.iter())
        .filter(|(pa, pb)| {
            pa[..3]
                .iter()
                .zip(pb[..3].iter())
                .any(|(ca, cb)| ca.abs_diff(*cb) > min_delta)
        })
        .count()
}

#[test]
fn gpu_instanced_billboards_match_cpu_path() {
    let Some(mut renderer) = try_gpu_renderer() else {
        return;
    };
    let camera = Camera {
        position: [8.0, 4.0, 20.0],
        yaw_deg: 0.0,
        pitch_deg: -10.0,
        far: 96.0,
        ..Camera::default()
    };
    let right = camera.right_vector();
    let up = [0.0, 1.0, 0.0];
    let mut world = VanillaParticleWorld::new();
    for i in 0..4u32 {
        let t = i as f32;
        assert!(world.spawn(
            match i % 4 {
                0 => VanillaParticleKind::Flame,
                1 => VanillaParticleKind::Smoke,
                2 => VanillaParticleKind::Crit,
                _ => VanillaParticleKind::Explosion,
            },
            [6.0 + t, 3.0 + 0.5 * t, 8.0 + t],
            [0.0; 3],
            [1.0, 0.3 + 0.15 * t, 0.1, 0.35 + 0.15 * t],
            20.0,
        ));
    }
    let base = render(&mut renderer, &camera, 1);

    assert!(renderer.particle_instancing_enabled());
    renderer.set_particle_world(&world, right, up);
    assert_eq!(renderer.staged_particle_instances().len(), 4);
    let instanced = render(&mut renderer, &camera, 2);
    assert_eq!(
        renderer.particle_instance_uploads, 4,
        "one record per live particle"
    );

    renderer.set_particle_instancing_enabled(false);
    assert!(!renderer.particle_instancing_enabled());
    renderer.set_particle_world(&world, right, up);
    assert!(
        renderer.staged_particle_instances().is_empty(),
        "CPU path leaves the instance scratch empty"
    );
    let fallback = render(&mut renderer, &camera, 3);
    let fallbacks = renderer.particle_cpu_fallbacks;
    assert!(fallbacks >= 1, "fallback call counted");

    // The instanced draw must paint the same quads as the CPU path.
    let inst_changed = changed_pixels(&base, &instanced, 4);
    let cpu_changed = changed_pixels(&base, &fallback, 4);
    assert!(
        inst_changed > 0 && cpu_changed > 0,
        "particles must paint pixels: instanced={inst_changed} cpu={cpu_changed}"
    );
    let mismatch = changed_pixels(&instanced, &fallback, 8);
    assert!(
        mismatch <= 4,
        "instanced vs CPU path diverged on {mismatch} pixels (>8 LSB)"
    );
    let ratio = inst_changed as f64 / cpu_changed.max(1) as f64;
    assert!(
        (0.9..=1.1).contains(&ratio),
        "coverage diverged: instanced={inst_changed} cpu={cpu_changed}"
    );

    // Toggling back re-enables the instanced draw for the same world.
    renderer.set_particle_instancing_enabled(true);
    renderer.set_particle_world(&world, right, up);
    assert_eq!(renderer.staged_particle_instances().len(), 4);
    let instanced2 = render(&mut renderer, &camera, 4);
    assert_eq!(
        changed_pixels(&instanced, &instanced2, 2),
        0,
        "same staged instances must render identically"
    );

    // A quad outside the bilinear model must route the whole call to the
    // CPU path and still paint.
    let cpu_only_quad = VanillaParticleQuad {
        kind: VanillaParticleKind::Crit,
        positions: [
            [6.0, 2.0, 8.0],
            [10.0, 2.0, 8.0],
            [11.0, 5.0, 8.0],
            [6.0, 5.0, 8.0],
        ],
        uv: [[0.0, 1.0], [1.0, 1.0], [1.0, 0.0], [0.0, 0.0]],
        color: [0.1, 1.0, 0.4, 0.9],
    };
    renderer.set_particle_billboards(&[cpu_only_quad]);
    assert_eq!(
        renderer.particle_cpu_fallbacks,
        fallbacks + 1,
        "CpuOnly quad must count as a fallback call"
    );
    let cpu_only = render(&mut renderer, &camera, 5);
    assert!(
        changed_pixels(&base, &cpu_only, 4) > 0,
        "CpuOnly quad still renders through the CPU path"
    );

    renderer.set_particle_billboards(&[]);
}
