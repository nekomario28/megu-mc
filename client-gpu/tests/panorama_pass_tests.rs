// SPDX-License-Identifier: LGPL-3.0-or-later
//! ALPHA5-VIS-PANORAMA-PASS coverage: real naga validation for the title
//! panorama shader plus pure Rust checks for cube-layer permutation, face
//! orientation, input validation and continuous camera-angle progression.

use megu_client_gpu::panorama_pass::{
    minecraft_panorama_face_flip, reorient_minecraft_panorama_face, validate_panorama_face_data,
    validate_panorama_overlay_data, vanilla_panorama_angles, TitlePanoramaPass,
    MINECRAFT_PANORAMA_TO_WGPU_LAYER, TITLE_PANORAMA_WGSL, WGPU_LAYER_TO_MINECRAFT_PANORAMA,
};
use megu_client_gpu::renderer::{request_gpu_device, OFFSCREEN_FORMAT};
use megu_client_mesh::RenderBackendPreference;
use wgpu::naga::front::wgsl;
use wgpu::naga::valid::{Capabilities, ValidationFlags, Validator};
use wgpu::naga::ShaderStage;

fn compile_and_validate(source: &str) -> wgpu::naga::Module {
    let module = wgsl::parse_str(source)
        .unwrap_or_else(|err| panic!("naga parse failed:\n{}", err.emit_to_string(source)));
    Validator::new(ValidationFlags::all(), Capabilities::all())
        .validate(&module)
        .unwrap_or_else(|err| panic!("naga validation failed:\n{}", err.emit_to_string(source)));
    module
}

fn has_entry_point(module: &wgpu::naga::Module, stage: ShaderStage, name: &str) -> bool {
    module
        .entry_points
        .iter()
        .any(|ep| ep.stage == stage && ep.name == name)
}

#[test]
fn title_panorama_wgsl_validates_with_real_naga() {
    let module = compile_and_validate(TITLE_PANORAMA_WGSL);
    assert!(has_entry_point(&module, ShaderStage::Vertex, "vs_main"));
    assert!(has_entry_point(&module, ShaderStage::Fragment, "fs_main"));
    assert!(TITLE_PANORAMA_WGSL.contains("@binding(1) var panorama_cube: texture_cube<f32>"));
    assert!(TITLE_PANORAMA_WGSL.contains("@binding(3) var panorama_overlay: texture_2d<f32>"));
    assert_eq!(
        TITLE_PANORAMA_WGSL
            .matches("textureSample(panorama_cube")
            .count(),
        5,
        "the softening filter must remain exactly a center tap plus four neighbors"
    );
    assert!(TITLE_PANORAMA_WGSL.contains("@builtin(vertex_index)"));
}

#[test]
fn minecraft_faces_map_to_wgpu_layer_order_without_collision() {
    assert_eq!(MINECRAFT_PANORAMA_TO_WGPU_LAYER, [5, 0, 4, 1, 2, 3]);
    assert_eq!(WGPU_LAYER_TO_MINECRAFT_PANORAMA, [1, 3, 4, 5, 2, 0]);
    let mut seen = [false; 6];
    for (minecraft_face, &layer) in MINECRAFT_PANORAMA_TO_WGPU_LAYER.iter().enumerate() {
        assert!(layer < 6);
        assert!(!seen[layer], "layer {layer} is reused");
        seen[layer] = true;
        assert_eq!(WGPU_LAYER_TO_MINECRAFT_PANORAMA[layer], minecraft_face);
    }
    assert!(seen.into_iter().all(|used| used));
}

#[test]
fn face_upload_orientation_matches_camera_capture_axes() {
    assert_eq!(minecraft_panorama_face_flip(0), (true, false));
    assert_eq!(minecraft_panorama_face_flip(1), (true, false));
    assert_eq!(minecraft_panorama_face_flip(2), (true, false));
    assert_eq!(minecraft_panorama_face_flip(3), (true, false));
    assert_eq!(minecraft_panorama_face_flip(4), (false, true));
    assert_eq!(minecraft_panorama_face_flip(5), (false, true));

    // 2×2 face with a distinct byte pattern per pixel:
    // top-left=1, top-right=2, bottom-left=3, bottom-right=4.
    let mut face = Vec::new();
    for pixel in [1_u8, 2, 3, 4] {
        face.extend_from_slice(&[pixel, pixel, pixel, 255]);
    }
    let side = reorient_minecraft_panorama_face(&face, 2, 0).unwrap();
    assert_eq!(
        side[0], 2,
        "side face output left edge starts at source right"
    );
    assert_eq!(side[4], 1);
    assert_eq!(side[8], 4);
    assert_eq!(side[12], 3);
    let up = reorient_minecraft_panorama_face(&face, 2, 4).unwrap();
    assert_eq!(
        up[0], 3,
        "pole face output top edge starts at source bottom"
    );
    assert_eq!(up[4], 4);
    assert_eq!(up[8], 1);
    assert_eq!(up[12], 2);
}

#[test]
fn dimension_validation_rejects_mismatched_or_impossible_payloads() {
    let good = vec![7_u8; 2 * 2 * 4];
    let faces = [
        &good[..],
        &good[..],
        &good[..],
        &good[..],
        &good[..],
        &good[..],
    ];
    validate_panorama_face_data(faces, 2, 2).unwrap();

    assert!(validate_panorama_face_data(faces, 0, 2)
        .unwrap_err()
        .contains("nonzero"));
    assert!(validate_panorama_face_data(faces, 3, 2)
        .unwrap_err()
        .contains("exceeds"));

    let short = [0_u8; 15];
    let bad_faces = [
        &good[..],
        &good[..],
        &short[..],
        &good[..],
        &good[..],
        &good[..],
    ];
    let error = validate_panorama_face_data(bad_faces, 2, 2).unwrap_err();
    assert!(error.contains("face 2"), "{error}");
    assert!(error.contains("expected 16"), "{error}");

    validate_panorama_overlay_data(&good, 2, 2, 2).unwrap();
    assert!(validate_panorama_overlay_data(&good, 0, 2, 2)
        .unwrap_err()
        .contains("nonzero"));
    assert!(validate_panorama_overlay_data(&good[..15], 2, 2, 2)
        .unwrap_err()
        .contains("expected 16"));
    assert!(reorient_minecraft_panorama_face(&good[..15], 2, 0)
        .unwrap_err()
        .contains("expected 16"));
    assert!(reorient_minecraft_panorama_face(&good, 2, 6)
        .unwrap_err()
        .contains("outside"));
}

#[test]
fn vanilla_angles_are_finite_continuous_and_unwrapped() {
    let (yaw0, pitch0) = vanilla_panorama_angles(0.0);
    assert!(yaw0.abs() < 1e-7);
    assert!((pitch0 - 25.0_f32.to_radians()).abs() < 1e-6);

    let (yaw10, pitch10) = vanilla_panorama_angles(10.0);
    assert!((yaw10 - 30.0_f32.to_radians()).abs() < 1e-5);
    let expected_pitch10 = ((10.0_f64 * 0.1).sin() * 5.0 + 25.0).to_radians() as f32;
    assert!((pitch10 - expected_pitch10).abs() < 1e-5);

    // One revolution takes 120 seconds at 3 deg/s. The helper returns a
    // continuous angle rather than wrapping and introducing a 2π jump.
    let (yaw120, _) = vanilla_panorama_angles(120.0);
    assert!((yaw120 - std::f32::consts::TAU).abs() < 1e-3);

    let mut previous = vanilla_panorama_angles(0.0);
    for step in 1..=2_000 {
        let t = step as f32 * 0.01;
        let current = vanilla_panorama_angles(t);
        assert!(current.0.is_finite() && current.1.is_finite());
        let yaw_delta = current.0 - previous.0;
        let pitch_delta = (current.1 - previous.1).abs();
        assert!(
            yaw_delta > 0.0 && yaw_delta < 0.001,
            "yaw jump at {t}: {yaw_delta}"
        );
        assert!(pitch_delta < 0.0002, "pitch jump at {t}: {pitch_delta}");
        previous = current;
    }

    assert_eq!(
        vanilla_panorama_angles(f32::NAN),
        vanilla_panorama_angles(0.0)
    );
    assert_eq!(
        vanilla_panorama_angles(f32::INFINITY),
        vanilla_panorama_angles(0.0)
    );
    let negative = vanilla_panorama_angles(-1.0);
    assert!(negative.0.is_finite() && negative.1.is_finite());
}

#[test]
fn gpu_constructor_upload_and_encode_smoke() {
    let Ok(ctx) = request_gpu_device(RenderBackendPreference::Classic, None, None) else {
        eprintln!("skipping panorama GPU smoke test: no wgpu adapter");
        return;
    };
    let mut pass = TitlePanoramaPass::new(&ctx.device, &ctx.queue, OFFSCREEN_FORMAT);
    assert!(!pass.has_panorama());
    assert_eq!(pass.face_size(), 1);

    let mut faces_vec = Vec::new();
    for face in 0..6_u8 {
        let mut rgba = vec![0_u8; 2 * 2 * 4];
        for pixel in rgba.as_chunks_mut::<4>().0 {
            *pixel = [20 + face * 30, 80, 160, 255];
        }
        faces_vec.push(rgba);
    }
    let faces: [&[u8]; 6] = std::array::from_fn(|i| faces_vec[i].as_slice());
    let overlay = [
        255, 255, 255, 32, 0, 0, 0, 64, 0, 0, 0, 64, 255, 255, 255, 32,
    ];
    pass.upload_from_faces(&ctx.device, &ctx.queue, faces, 2, Some((&overlay, 2, 2)))
        .unwrap();
    assert!(pass.has_panorama());
    assert_eq!(pass.face_size(), 2);
    assert_eq!(pass.overlay_size(), Some((2, 2)));

    let target = ctx.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("panorama-smoke-target"),
        size: wgpu::Extent3d {
            width: 8,
            height: 8,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: OFFSCREEN_FORMAT,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
        view_formats: &[],
    });
    let view = target.create_view(&wgpu::TextureViewDescriptor::default());
    let mut encoder = ctx
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("panorama-smoke-encoder"),
        });
    pass.encode(
        &mut encoder,
        &ctx.queue,
        &view,
        8,
        8,
        0.0,
        25.0_f32.to_radians(),
        1.0,
    );
    ctx.queue.submit([encoder.finish()]);
}
