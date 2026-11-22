// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! Real adapter readback for debug geometry, articulated rigs, BERs, and particles.

use mc_spine_client_gpu::atmosphere::{AtmosphereParams, BiomeAtmosphereBlender, ChunkFadeTracker};
use mc_spine_client_gpu::renderer::{
    probe_adapters, request_gpu_device, ComposedFrame, GpuRenderer, MAX_DEBUG_LINES,
    OFFSCREEN_FORMAT,
};
use mc_spine_client_gpu::subworld_render::{SubWorldRenderBank, DEBUG_LINE_WGSL};
use mc_spine_client_gpu::Camera;
use mc_spine_client_mesh::ber_instancing::{
    build_ber_mesh, VanillaBerFacing, VanillaBerKind, VanillaBerState, VanillaParticleKind,
    VanillaParticleWorld,
};
use mc_spine_client_mesh::builtin_shadow_wgsl::BuiltinShadowQuality;
use mc_spine_client_mesh::entity_rig::{build_entity_rig_mesh, classify_entity_rig, EntityPose};
use mc_spine_client_mesh::RenderBackendPreference;
use mc_spine_kernel::debug_chords::{
    build_chunk_border_lines, build_entity_hitbox_lines, DebugLine,
};
use mc_spine_kernel::Aabb;
use wgpu::naga::front::wgsl;
use wgpu::naga::valid::{Capabilities, ValidationFlags, Validator};

fn render(renderer: &mut GpuRenderer, camera: &Camera) -> Vec<u8> {
    let mut bank = SubWorldRenderBank::new();
    let atmosphere = BiomeAtmosphereBlender::new(AtmosphereParams::default());
    let fades = ChunkFadeTracker::new();
    renderer
        .render_offscreen_composed_frame(
            512,
            512,
            camera,
            &mut ComposedFrame {
                subworlds: &mut bank,
                lights: &[],
                atmosphere: &atmosphere,
                chunk_fades: &fades,
                ui_rgba: None,
                world_revision: 1,
                world_time_ticks: 6000,
            },
        )
        .expect("offscreen GPU readback")
}

#[test]
fn rx7800_xt_debug_lines_entities_ber_particles_readback() {
    let module = wgsl::parse_str(DEBUG_LINE_WGSL).unwrap_or_else(|error| {
        panic!("debug line WGSL: {}", error.emit_to_string(DEBUG_LINE_WGSL))
    });
    Validator::new(ValidationFlags::all(), Capabilities::all())
        .validate(&module)
        .expect("debug-line naga validation");

    let adapters = probe_adapters(RenderBackendPreference::Auto);
    assert!(
        !adapters.is_empty(),
        "real GPU adapter required for this integration test"
    );
    let ctx = request_gpu_device(RenderBackendPreference::Auto, None, None)
        .expect("RX 7800 XT GPU device");
    eprintln!(
        "GPU wireframe and entities on {} vendor {:04x}",
        ctx.report.name, ctx.report.vendor_id
    );
    assert_eq!(ctx.report.vendor_id, 0x1002, "RX 7800 XT AMD GPU required");
    assert!(ctx.report.name.contains("7800"), "RX 7800 XT GPU required");

    let mut renderer = GpuRenderer::new(ctx, OFFSCREEN_FORMAT);
    assert_eq!(
        renderer.set_shadow_quality(BuiltinShadowQuality::Low),
        BuiltinShadowQuality::Low,
        "RX 7800 XT must accept the CSM shadowed lit pipeline"
    );
    let camera = Camera {
        position: [8.0, 3.3, 20.0],
        yaw_deg: 0.0,
        pitch_deg: -8.0,
        far: 96.0,
        ..Camera::default()
    };
    let baseline = render(&mut renderer, &camera);

    // Zombie (protocol 1.21.1 entity id 124) and chest use their actual CPU geometry.
    let zombie_spec = (0..130)
        .filter_map(classify_entity_rig)
        .find(|spec| spec.name == "minecraft:zombie")
        .expect("zombie rig available");
    let zombie = build_entity_rig_mesh(zombie_spec, EntityPose::default());
    let chest = build_ber_mesh(
        VanillaBerKind::Chest,
        VanillaBerState::Chest {
            lid_open: 0.0,
            double: false,
            facing: VanillaBerFacing::North,
        },
    );
    assert!(!zombie.quads.is_empty() && !chest.quads.is_empty());
    renderer.set_entity_and_ber_meshes(vec![(&zombie, [7.0, 1.0, 9.0], [0.25, 1.0, 0.2])]);
    let zombie_pixels = render(&mut renderer, &camera);
    let changed_zombie = baseline
        .chunks_exact(4)
        .zip(zombie_pixels.chunks_exact(4))
        .filter(|(a, b)| a[0..3] != b[0..3])
        .count();
    assert!(
        changed_zombie > 50,
        "Zombie rig must draw GPU pixels: {changed_zombie}"
    );

    renderer.set_entity_and_ber_meshes(vec![(&chest, [9.0, 1.0, 9.0], [1.0, 0.55, 0.20])]);
    let chest_pixels = render(&mut renderer, &camera);
    let changed_chest = baseline
        .chunks_exact(4)
        .zip(chest_pixels.chunks_exact(4))
        .filter(|(a, b)| a[0..3] != b[0..3])
        .count();
    assert!(
        changed_chest > 50,
        "Chest BER must draw GPU pixels: {changed_chest}"
    );

    renderer.set_entity_and_ber_meshes(vec![
        (&zombie, [7.0, 1.0, 9.0], [0.25, 1.0, 0.2]),
        (&chest, [9.0, 1.0, 9.0], [1.0, 0.55, 0.20]),
    ]);
    let lit = render(&mut renderer, &camera);
    let changed_lit = baseline
        .chunks_exact(4)
        .zip(lit.chunks_exact(4))
        .filter(|(a, b)| a[0..3] != b[0..3])
        .count();
    assert!(
        changed_lit > 100,
        "rig and chest must rasterize, changed {changed_lit} pixels"
    );

    let mut lines = build_chunk_border_lines(0, 0, 0, 16);
    lines.extend(build_entity_hitbox_lines(
        Aabb {
            min: [6.7, 1.0, 8.7],
            max: [7.3, 2.95, 9.3],
        },
        2.65,
        0.0,
        0.0,
        true,
    ));
    assert!(lines.len() < MAX_DEBUG_LINES);
    // Add three unobstructed reference segments to make color readback independent
    // of subpixel chunk borders and the user's GPU line rasterization convention.
    for (y, rgba) in [
        (4.0, [255, 0, 0, 255]),
        (4.25, [255, 255, 0, 255]),
        (4.5, [0, 0, 255, 255]),
    ] {
        lines.push(DebugLine {
            start: [5.5, y, 12.0],
            end: [10.5, y, 12.0],
            rgba,
        });
    }
    lines.push(DebugLine {
        start: [f32::NAN, 1.0, 1.0],
        end: [2.0, 1.0, 1.0],
        rgba: [255; 4],
    });
    renderer.set_debug_lines(&lines);
    let with_lines = render(&mut renderer, &camera);
    let added_pixels = lit
        .chunks_exact(4)
        .zip(with_lines.chunks_exact(4))
        .filter(|(a, b)| a[0..3] != b[0..3])
        .count();
    assert!(
        added_pixels > 50,
        "debug LineList must appear: {added_pixels} pixels"
    );
    let mut red = 0;
    let mut yellow = 0;
    let mut blue = 0;
    for pixel in with_lines.chunks_exact(4) {
        if pixel[0] > 180 && pixel[1] < 95 && pixel[2] < 95 {
            red += 1;
        }
        if pixel[0] > 170 && pixel[1] > 170 && pixel[2] < 110 {
            yellow += 1;
        }
        if pixel[2] > 180 && pixel[0] < 100 && pixel[1] < 120 {
            blue += 1;
        }
    }
    assert!(
        red > 0 && yellow > 0 && blue > 0,
        "debug lines red={red} yellow={yellow} blue={blue}"
    );

    let mut particle_world = VanillaParticleWorld::new();
    assert!(particle_world.spawn(
        VanillaParticleKind::Flame,
        [8.0, 3.0, 11.0],
        [0.0; 3],
        [1.0, 0.25, 0.1, 0.8],
        2.0,
    ));
    renderer.set_particle_world(&particle_world, camera.right_vector(), [0.0, 1.0, 0.0]);
    let with_particle = render(&mut renderer, &camera);
    assert!(
        with_lines
            .chunks_exact(4)
            .zip(with_particle.chunks_exact(4))
            .any(|(a, b)| a[0..3] != b[0..3]),
        "particle quad must change GPU pixels"
    );
    renderer.set_debug_lines(&[]);
    renderer.set_particle_billboards(&[]);
}
