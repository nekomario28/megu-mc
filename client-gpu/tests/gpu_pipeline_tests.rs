// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! `client-gpu` tests: pure-logic units plus a real `wgpu` headless
//! probe/offscreen render that skips gracefully when no adapter exists.

use mc_spine_client::shaders::{
    validate_wgsl_module, WgslStage, TERRAIN_OPAQUE_WGSL, TERRAIN_TRANSLUCENT_WGSL,
};
use mc_spine_client_gpu::renderer::{
    probe_adapters, request_gpu_device, GpuRenderer, RendererError, OFFSCREEN_FORMAT,
};
use mc_spine_client_gpu::{
    aligned_copy_bytes_per_row, build_demo_scene, build_index_buffer, gpu_capabilities_for,
    gpu_shader_caps_for, gpu_vendor_for_id, is_translucent_quad, mat4_mul_vec4, negotiate_pipeline,
    pack_quads_to_vertices, parse_gpu_args, present_mode_for, split_opaque_translucent,
    translucent_quad_order, ui_rgba_len, wgpu_backends_for, Camera, GpuCliConfig, MoveInput,
    UI_BLIT_WGSL,
};
use mc_spine_client_mesh::{ActiveRenderBackend, GpuVendor, Layer, RenderBackendPreference};
use mc_spine_ui::draw::rasterize_draw_list;
use mc_spine_ui::state::{HudState, Screen, UiState};

// ---------------------------------------------------------------------------
// Camera math
// ---------------------------------------------------------------------------

#[test]
fn camera_forward_right_vectors() {
    let cam = Camera {
        position: [0.0; 3],
        yaw_deg: 0.0,
        pitch_deg: 0.0,
        ..Camera::default()
    };
    let fwd = cam.forward_vector();
    assert!((fwd[2] + 1.0).abs() < 1e-6, "yaw 0 faces -Z, got {fwd:?}");
    let right = cam.right_vector();
    assert!((right[0] - 1.0).abs() < 1e-6, "right is +X, got {right:?}");

    // Yaw 90° turns the forward vector to +X.
    let cam = Camera {
        yaw_deg: 90.0,
        ..cam
    };
    let fwd = cam.forward_vector();
    assert!((fwd[0] - 1.0).abs() < 1e-6, "yaw 90 faces +X, got {fwd:?}");
}

#[test]
fn camera_view_projection_center_and_depth() {
    let cam = Camera {
        position: [0.0; 3],
        yaw_deg: 0.0,
        pitch_deg: 0.0,
        near: 0.1,
        far: 512.0,
        ..Camera::default()
    };
    let vp = cam.view_projection(16.0 / 9.0);

    // A point 10 blocks ahead lands centered with NDC depth in (0, 1).
    let clip = mat4_mul_vec4(&vp, [0.0, 0.0, -10.0, 1.0]);
    assert!(
        clip[0].abs() < 1e-4 && clip[1].abs() < 1e-4,
        "centered: {clip:?}"
    );
    let ndc_z = clip[2] / clip[3];
    assert!((0.0..1.0).contains(&ndc_z), "depth in wgpu range: {ndc_z}");

    // The near plane maps to NDC z = 0 exactly.
    let clip = mat4_mul_vec4(&vp, [0.0, 0.0, -0.1, 1.0]);
    assert!((clip[2] / clip[3]).abs() < 1e-4, "near -> 0: {clip:?}");

    // A point behind the camera flips w (the sign that rejects it).
    let clip = mat4_mul_vec4(&vp, [0.0, 0.0, 5.0, 1.0]);
    assert!(clip[3] < 0.0, "behind camera -> negative w: {clip:?}");
}

#[test]
fn camera_move_and_look() {
    let mut cam = Camera {
        position: [0.0; 3],
        yaw_deg: 0.0,
        pitch_deg: 0.0,
        ..Camera::default()
    };
    cam.apply_move(
        MoveInput {
            forward: 1.0,
            ..MoveInput::default()
        },
        1.0,
    );
    assert!(
        cam.position[2] < 0.0,
        "forward at yaw 0 moves -Z: {:?}",
        cam.position
    );
    cam.apply_move(
        MoveInput {
            strafe: 1.0,
            ..MoveInput::default()
        },
        1.0,
    );
    assert!(
        cam.position[0] > 0.0,
        "strafe right moves +X: {:?}",
        cam.position
    );

    cam.apply_mouse_look(0.0, 100_000.0);
    assert!(
        cam.pitch_deg >= -89.9,
        "pitch clamps low: {}",
        cam.pitch_deg
    );
    cam.apply_mouse_look(0.0, -200_000.0);
    assert!(
        cam.pitch_deg <= 89.9,
        "pitch clamps high: {}",
        cam.pitch_deg
    );
    cam.apply_mouse_look(100.0, 0.0);
    let yaw = cam.yaw_deg;
    assert!((0.0..360.0).contains(&yaw), "yaw wraps: {yaw}");
}

// ---------------------------------------------------------------------------
// Packed-quad -> GPU buffer conversion (real client-port FFI)
// ---------------------------------------------------------------------------

#[test]
fn demo_scene_meshes_real_quads() {
    let scene = build_demo_scene([8.0, 18.0, 26.0]);
    assert!(
        !scene.opaque_quads.is_empty(),
        "terrain must emit opaque quads"
    );
    assert!(
        !scene.translucent_quads.is_empty(),
        "water + stained glass must emit translucent quads"
    );
    assert!(
        scene
            .translucent_quads
            .iter()
            .any(|q| q.layer == Layer::Fluid),
        "water produces fluid-layer quads"
    );
    assert!(scene.translucent_quads.iter().all(is_translucent_quad));
    assert!(scene.opaque_quads.iter().all(|q| !is_translucent_quad(q)));
}

#[test]
fn pack_and_index_conversion() {
    let scene = build_demo_scene([8.0, 18.0, 26.0]);
    let verts = pack_quads_to_vertices(&scene.opaque_quads).expect("pack opaque quads");
    assert_eq!(verts.len(), scene.opaque_quads.len() * 4);
    let indices = build_index_buffer(&scene.opaque_quads);
    assert_eq!(indices.len(), scene.opaque_quads.len() * 6);
    let max_index = indices.iter().copied().max().unwrap_or(0);
    assert!((max_index as usize) < verts.len());

    // The packed layout matches the C header: X in bits 0..4, Z in 5..9,
    // direction in attributes bits 0..2, corner index in bits 30..31.
    let v0 = verts[0];
    assert!(v0.position & 0x1f < 16, "X is section-local");
    assert!(
        ((v0.attributes >> 30) & 0x3) == 0,
        "first vertex is corner 0"
    );
    let v1 = verts[1];
    assert_eq!(
        ((v1.attributes >> 30) & 0x3),
        1,
        "second vertex is corner 1"
    );
    // All four verts of a quad carry the same state_id as the source quad.
    let quad = scene.opaque_quads[0];
    for v in &verts[..4] {
        assert_eq!(v.state_id, quad.state_id);
    }
}

#[test]
fn translucent_sort_uses_real_ffi() {
    let scene = build_demo_scene([8.0, 18.0, 26.0]);
    let all: Vec<_> = scene
        .opaque_quads
        .iter()
        .chain(scene.translucent_quads.iter())
        .copied()
        .collect();
    let order = translucent_quad_order(&all, [8.0, 18.0, 26.0]).expect("sort accepted");
    assert_eq!(order.len(), scene.translucent_quads.len());
    // Every returned index names a translucent quad of the input.
    for &i in &order {
        assert!(is_translucent_quad(&all[i as usize]));
    }
    // A non-finite camera fails closed to None.
    assert!(translucent_quad_order(&all, [f32::NAN; 3]).is_none());
}

#[test]
fn split_partition_is_exact() {
    let scene = build_demo_scene([8.0, 18.0, 26.0]);
    let all: Vec<_> = scene
        .opaque_quads
        .iter()
        .chain(scene.translucent_quads.iter())
        .copied()
        .collect();
    let (opaque, translucent) = split_opaque_translucent(&all);
    assert_eq!(opaque.len() + translucent.len(), all.len());
}

// ---------------------------------------------------------------------------
// Capability mapping (pure; no adapter needed)
// ---------------------------------------------------------------------------

#[test]
fn limits_map_to_shader_caps() {
    let features = wgpu::Features::empty();
    let limits = wgpu::Limits {
        max_storage_buffers_per_shader_stage: 8,
        max_storage_buffer_binding_size: 128 * 1024 * 1024,
        max_storage_textures_per_shader_stage: 4,
        max_color_attachments: 8,
        max_compute_workgroups_per_dimension: 65535,
        max_compute_invocations_per_workgroup: 1024,
        ..wgpu::Limits::default()
    };
    let caps = gpu_shader_caps_for(wgpu::Backend::Vulkan, features, &limits);
    assert_eq!(caps.max_glsl_version, 460);
    assert!(caps.supports_compute);
    assert!(caps.supports_ssbo);
    assert!(caps.supports_custom_images);
    assert_eq!(caps.max_ssbo_bytes, 128 * 1024 * 1024);
    assert_eq!(caps.max_color_attachments, 8);

    let gl_caps = gpu_shader_caps_for(wgpu::Backend::Gl, features, &limits);
    assert_eq!(
        gl_caps.max_glsl_version, 410,
        "GL stays on the 4.10 ceiling"
    );

    let mut no_compute = limits.clone();
    no_compute.max_compute_workgroups_per_dimension = 0;
    let caps = gpu_shader_caps_for(wgpu::Backend::Gl, features, &no_compute);
    assert!(!caps.supports_compute);
}

#[test]
fn features_and_vendor_map_to_gpu_caps() {
    let limits = wgpu::Limits {
        max_compute_workgroups_per_dimension: 65535,
        max_compute_invocations_per_workgroup: 256,
        ..wgpu::Limits::default()
    };
    let features = wgpu::Features::MULTI_DRAW_INDIRECT_COUNT;
    let caps = gpu_capabilities_for(features, &limits, 0x8086);
    assert_eq!(caps.vendor, GpuVendor::Intel);
    assert!(caps.supports_compute_shaders);
    assert!(caps.supports_multi_draw_indirect_count);
    assert!(!caps.supports_ext_mesh_shader);
    // Compute + indirect-count => ComputeIndirect under Auto.
    let res = negotiate_pipeline(RenderBackendPreference::Auto, &caps);
    assert_eq!(res.backend, ActiveRenderBackend::ComputeIndirect);

    assert_eq!(gpu_vendor_for_id(0x10de), GpuVendor::Nvidia);
    assert_eq!(gpu_vendor_for_id(0x1002), GpuVendor::Amd);
    assert_eq!(gpu_vendor_for_id(0x106b), GpuVendor::Apple);
    assert_eq!(gpu_vendor_for_id(0xbeef), GpuVendor::Other);
}

#[test]
fn minimal_caps_negotiate_classic() {
    let caps = mc_spine_client_mesh::GpuCapabilities::software();
    let res = negotiate_pipeline(RenderBackendPreference::Auto, &caps);
    assert_eq!(res.backend, ActiveRenderBackend::Classic);
    assert!(res.fallback_reason.is_some());
    let low = negotiate_pipeline(RenderBackendPreference::ClassicLowSpec, &caps);
    assert_eq!(low.backend, ActiveRenderBackend::ClassicLowSpec);
}

#[test]
fn backend_flags_and_present_mode() {
    let primary = wgpu_backends_for(RenderBackendPreference::Auto);
    assert!(primary.contains(wgpu::Backends::VULKAN));
    let low = wgpu_backends_for(RenderBackendPreference::ClassicLowSpec);
    assert_eq!(low, wgpu::Backends::GL);

    let modes = [wgpu::PresentMode::Fifo, wgpu::PresentMode::Immediate];
    assert_eq!(present_mode_for(true, &modes), wgpu::PresentMode::Fifo);
    assert_eq!(
        present_mode_for(false, &modes),
        wgpu::PresentMode::Immediate
    );
    let mailbox_only = [wgpu::PresentMode::Mailbox];
    assert_eq!(
        present_mode_for(true, &mailbox_only),
        wgpu::PresentMode::AutoVsync
    );
}

// ---------------------------------------------------------------------------
// UI framebuffer sizing
// ---------------------------------------------------------------------------

#[test]
fn ui_rgba_sizing() {
    assert_eq!(ui_rgba_len(640, 360), 640 * 360 * 4);
    // 256-byte copy alignment only pads when the row isn't already aligned.
    assert_eq!(aligned_copy_bytes_per_row(64), 256);
    assert_eq!(aligned_copy_bytes_per_row(65), 512);
    assert_eq!(aligned_copy_bytes_per_row(1), 256);

    // The real client-ui rasterizer output is the texture payload.
    let mut ui = UiState::new();
    ui.screen = Screen::InGameHud;
    ui.hud = HudState::demo();
    let list = ui.render(320, 180);
    let rgba = rasterize_draw_list(320, 180, &list);
    assert_eq!(rgba.len(), ui_rgba_len(320, 180));
}

// ---------------------------------------------------------------------------
// WGSL modules
// ---------------------------------------------------------------------------

#[test]
fn wgsl_modules_validate() {
    let graphics = &[WgslStage::Vertex, WgslStage::Fragment];
    for (name, source) in [
        ("ui_blit", UI_BLIT_WGSL),
        ("terrain_opaque", TERRAIN_OPAQUE_WGSL),
        ("terrain_translucent", TERRAIN_TRANSLUCENT_WGSL),
    ] {
        let info = validate_wgsl_module(source, graphics)
            .unwrap_or_else(|e| panic!("{name} must validate: {e}"));
        assert!(info
            .entry_points
            .iter()
            .any(|(s, _)| *s == WgslStage::Vertex));
        assert!(info
            .entry_points
            .iter()
            .any(|(s, _)| *s == WgslStage::Fragment));
    }
    // A broken module fails closed.
    assert!(validate_wgsl_module("fn oops() {", graphics).is_err());
    assert!(validate_wgsl_module("struct A { x: f32 };", graphics).is_err());
}

// ---------------------------------------------------------------------------
// CLI parsing
// ---------------------------------------------------------------------------

#[test]
fn cli_parse() {
    let cfg = parse_gpu_args(&[
        "--connect",
        "mc.example.com:25566",
        "--shader-pack",
        "packs/meu.zip",
        "--gpu-backend",
        "classic-low-spec",
        "--offline-demo",
        "--vsync",
        "off",
    ])
    .expect("valid args");
    assert_eq!(cfg.connect, Some(("mc.example.com".to_string(), 25566)));
    assert_eq!(
        cfg.shader_pack.as_deref(),
        Some(std::path::Path::new("packs/meu.zip"))
    );
    assert_eq!(cfg.gpu_backend, RenderBackendPreference::ClassicLowSpec);
    assert!(cfg.offline_demo);
    assert!(!cfg.vsync);

    let probe = parse_gpu_args(&["--headless-probe"]).unwrap();
    assert!(probe.headless_probe);
    assert_eq!(probe.gpu_backend, RenderBackendPreference::Auto);
    assert!(probe.vsync, "vsync defaults on");

    // host without a port defaults to 25565.
    let bare = parse_gpu_args(&["--connect", "localhost"]).unwrap();
    assert_eq!(bare.connect, Some(("localhost".to_string(), 25565)));

    // inline --flag=value form works too.
    let inline = parse_gpu_args(&["--gpu-backend=classic"]).unwrap();
    assert_eq!(inline.gpu_backend, RenderBackendPreference::Classic);

    // Errors fail closed.
    assert!(parse_gpu_args(&["--gpu-backend", "vulkan-please"]).is_err());
    assert!(parse_gpu_args(&["--connect"]).is_err());
    assert!(parse_gpu_args(&["--bogus"]).is_err());
    assert!(parse_gpu_args(&["--vsync", "maybe"]).is_err());

    let default_cfg = GpuCliConfig::default();
    assert!(!default_cfg.headless_probe && !default_cfg.offline_demo);
}

// ---------------------------------------------------------------------------
// Real wgpu probe + offscreen render (graceful skip without an adapter)
// ---------------------------------------------------------------------------

#[test]
fn headless_probe_does_not_panic() {
    // On a GPU-less VM this must return an empty vec, not crash; on a box
    // with llvmpipe/lavapipe (or real hardware) it reports real adapters.
    let reports = probe_adapters(RenderBackendPreference::Auto);
    for report in &reports {
        eprintln!(
            "probe: {:?} {:?} {} -> {}",
            report.backend,
            report.device_type,
            report.name,
            report.resolution.backend.as_str()
        );
    }
}

#[test]
fn offscreen_render_produces_nonblank_frame() {
    let ctx = match request_gpu_device(RenderBackendPreference::Auto, None, None) {
        Ok(ctx) => ctx,
        Err(RendererError::NoAdapter(msg)) => {
            eprintln!("SKIP offscreen render: no wgpu adapter ({msg})");
            return;
        }
        Err(e) => panic!("unexpected device error: {e}"),
    };
    eprintln!(
        "rendering offscreen on {:?} {} -> {}",
        ctx.report.backend,
        ctx.report.name,
        ctx.report.resolution.backend.as_str()
    );
    let mut renderer = GpuRenderer::new(ctx, OFFSCREEN_FORMAT);
    let scene = build_demo_scene([8.0, 18.0, 26.0]);
    renderer.upload_scene(&scene).expect("upload demo scene");

    // UI overlay through the real client-ui rasterizer.
    let mut ui = UiState::new();
    ui.screen = Screen::InGameHud;
    ui.hud = HudState::demo();
    let list = ui.render(128, 128);
    let ui_rgba = rasterize_draw_list(128, 128, &list);

    let camera = Camera {
        position: [8.0, 14.0, 22.0],
        yaw_deg: 180.0,
        pitch_deg: -15.0,
        ..Camera::default()
    };
    let pixels = renderer
        .render_offscreen_rgba(128, 128, &camera, Some(&ui_rgba))
        .expect("offscreen render");
    assert_eq!(pixels.len(), ui_rgba_len(128, 128));
    let (rgba, tail) = pixels.as_chunks::<4>();
    assert!(tail.is_empty());
    let first = rgba[0];
    assert!(
        rgba.iter().any(|px| *px != first),
        "frame must not be a flat color (terrain + sky expected)"
    );
    assert!(
        rgba.iter().all(|px| px[3] == 255),
        "sky clear and UI write opaque alpha everywhere"
    );
}
