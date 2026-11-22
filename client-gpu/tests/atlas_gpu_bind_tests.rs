// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! Atlas GPU upload, tiled terrain and extruded UV readback, and CSM regression.

use megu_client_gpu::atmosphere::{
    extruded_mesh_to_geometry, AtmosphereParams, BiomeAtmosphereBlender, ChunkFadeTracker,
};
use megu_client_gpu::renderer::{
    composed_shadow_wgsl, probe_adapters, request_gpu_device, ComposedFrame, GpuRenderer,
    OFFSCREEN_FORMAT,
};
use megu_client_gpu::subworld_render::{
    box_mesh, SubWorldRenderBank, SubWorldRenderEntry, SubWorldTransform, COMPOSITE_LIT_WGSL,
    COMPOSITE_TERRAIN_WGSL,
};
use megu_client_gpu::{build_demo_scene, mat4_mul_vec4, Camera, DemoScene};
use megu_client_mesh::atlas::{build_mipmaps_linear, AtlasBuilder, PaddingMode};
use megu_client_mesh::builtin_shadow_wgsl::{
    BuiltinShadowQuality, RenderScalePreset, BUILTIN_SHADOW_SHADERS, MEGU_SHADOW_CASTERS_WGSL,
    MEGU_SHADOW_DEPTH_WGSL,
};
use megu_client_mesh::vanilla_plus_3d::{ExtrudedMesh, ExtrudedQuad};
use megu_client_mesh::RenderBackendPreference;
use wgpu::naga::front::wgsl;
use wgpu::naga::valid::{Capabilities, ValidationFlags, Validator};

fn validate_wgsl(label: &str, source: &str) {
    let module = wgsl::parse_str(source)
        .unwrap_or_else(|error| panic!("{label}: {}", error.emit_to_string(source)));
    Validator::new(ValidationFlags::all(), Capabilities::all())
        .validate(&module)
        .unwrap_or_else(|error| panic!("{label}: {}", error.emit_to_string(source)));
}

#[test]
fn atlas_and_shadow_wgsl_modules_validate_with_naga() {
    for (label, source) in BUILTIN_SHADOW_SHADERS {
        validate_wgsl(label, source);
    }
    validate_wgsl("composite terrain", COMPOSITE_TERRAIN_WGSL);
    validate_wgsl("composite lit", COMPOSITE_LIT_WGSL);
    validate_wgsl(
        "composite shadowed terrain",
        &composed_shadow_wgsl(COMPOSITE_TERRAIN_WGSL, true),
    );
    validate_wgsl(
        "composite shadowed lit",
        &composed_shadow_wgsl(COMPOSITE_LIT_WGSL, false),
    );
    validate_wgsl(
        "shadow caster",
        &format!("{MEGU_SHADOW_DEPTH_WGSL}\n{MEGU_SHADOW_CASTERS_WGSL}"),
    );
}

fn composed_rgba(
    renderer: &mut GpuRenderer,
    bank: &mut SubWorldRenderBank,
    camera: &Camera,
    width: u32,
    height: u32,
    revision: u64,
) -> Vec<u8> {
    let atmosphere = BiomeAtmosphereBlender::new(AtmosphereParams::default());
    let fades = ChunkFadeTracker::new();
    renderer
        .render_offscreen_composed_frame(
            width,
            height,
            camera,
            &mut ComposedFrame {
                subworlds: bank,
                lights: &[],
                atmosphere: &atmosphere,
                chunk_fades: &fades,
                ui_rgba: None,
                world_revision: revision,
                world_time_ticks: 4000,
            },
        )
        .expect("real GPU composed frame")
}

fn sample_rgb(pixels: &[u8], width: u32, x: usize, y: usize) -> [u8; 3] {
    let offset = (y * width as usize + x) * 4;
    pixels[offset..offset + 3].try_into().unwrap()
}

#[test]
fn gpu_atlas_uv_tiling_extrusion_and_cascaded_shadow_pixels() {
    let adapters = probe_adapters(RenderBackendPreference::Auto);
    if adapters.is_empty() {
        eprintln!("SKIP atlas readback: no GPU adapter");
        return;
    }
    let ctx = request_gpu_device(RenderBackendPreference::Auto, None, None)
        .expect("adapter was present but device request failed");
    eprintln!(
        "atlas GPU readback on {} (vendor {:04x})",
        ctx.report.name, ctx.report.vendor_id
    );
    let mut renderer = GpuRenderer::new(ctx, OFFSCREEN_FORMAT);
    renderer.set_render_scale(RenderScalePreset::Native100);

    let mut builder = AtlasBuilder::for_blocks();
    builder.set_padding(2).unwrap();
    assert_eq!(builder.padding_mode(), PaddingMode::WrapTile);
    let solid = |rgba: [u8; 4]| rgba.repeat(16 * 16);
    builder
        .add("blue", 16, 16, solid([6, 10, 255, 255]))
        .unwrap();
    builder.add("red", 16, 16, solid([255, 7, 4, 255])).unwrap();
    let atlas = builder.build().unwrap();
    let mips = build_mipmaps_linear(&atlas, atlas.max_mip_levels(), None).unwrap();
    renderer.upload_block_atlas(&atlas, &mips).unwrap();
    let blue = renderer
        .atlas_sprite_index("blue")
        .expect("blue sprite index");
    assert_eq!(renderer.atlas_sprite_index("missing"), None);
    renderer
        .set_block_state_sprites(&[
            (10, "red"),
            (11, "red"),
            (12, "red"),
            (13, "red"),
            (14, "red"),
        ])
        .unwrap();
    assert!(renderer
        .set_block_state_sprites(&[(42, "unknown")])
        .is_err());

    let camera = Camera {
        position: [8.0, 15.0, 25.0],
        yaw_deg: 0.0,
        pitch_deg: -20.0,
        far: 96.0,
        ..Camera::default()
    };
    let scene = build_demo_scene(camera.position);
    assert!(
        scene.opaque_quads.iter().any(|quad| {
            let min_x = quad
                .positions
                .iter()
                .map(|p| p[0])
                .fold(f32::INFINITY, f32::min);
            let max_x = quad
                .positions
                .iter()
                .map(|p| p[0])
                .fold(f32::NEG_INFINITY, f32::max);
            let min_z = quad
                .positions
                .iter()
                .map(|p| p[2])
                .fold(f32::INFINITY, f32::min);
            let max_z = quad
                .positions
                .iter()
                .map(|p| p[2])
                .fold(f32::NEG_INFINITY, f32::max);
            max_x - min_x > 1.0 || max_z - min_z > 1.0
        }),
        "demo must include a multi-block greedy quad"
    );
    renderer.upload_scene(&scene).unwrap();

    let extruded = ExtrudedMesh {
        quads: vec![ExtrudedQuad {
            positions: [
                [5.0, 7.0, 15.0],
                [10.0, 7.0, 15.0],
                [10.0, 12.0, 15.0],
                [5.0, 12.0, 15.0],
            ],
            normal: [0.0, 0.0, 1.0],
            uv: [[0.0, 0.0], [3.0, 0.0], [3.0, 3.0], [0.0, 3.0]],
            tint_index: -1,
        }],
    };
    let (mut vertices, indices) = extruded_mesh_to_geometry(&extruded, |_| [1.0; 3]);
    assert_eq!(
        vertices.iter().map(|v| v.uv).collect::<Vec<_>>(),
        extruded.quads[0].uv
    );
    for vertex in &mut vertices {
        vertex.atlas_index = blue;
    }
    let mut bank = SubWorldRenderBank::new();
    bank.upsert(SubWorldRenderEntry::new(
        99,
        SubWorldTransform::IDENTITY,
        vertices,
        indices,
    ));
    let (width, height) = (384, 256);
    let textured = composed_rgba(&mut renderer, &mut bank, &camera, width, height, 1);
    let mut red_pixels = 0;
    let mut blue_pixels = 0;
    for rgb in textured.as_chunks::<4>().0.iter().map(|pixel| &pixel[..3]) {
        if rgb[0] > 60
            && u16::from(rgb[0]) > u16::from(rgb[1]) * 2
            && u16::from(rgb[0]) > u16::from(rgb[2]) * 2
        {
            red_pixels += 1;
        }
        if rgb[2] > 80
            && u16::from(rgb[2]) > u16::from(rgb[0]) * 2
            && u16::from(rgb[2]) > u16::from(rgb[1]) * 2
        {
            blue_pixels += 1;
        }
    }
    eprintln!("GPU textured readback: red {red_pixels}, blue {blue_pixels}");
    assert!(
        red_pixels > 60,
        "greedy terrain did not sample red atlas sprite"
    );
    assert!(
        blue_pixels > 60,
        "extruded UV geometry did not sample blue atlas sprite"
    );

    renderer.upload_scene(&DemoScene::default()).unwrap();
    let mut shadow_bank = SubWorldRenderBank::new();
    let (vertices, indices) = box_mesh([-8.0, -0.2, -8.0], [8.0, 0.0, 8.0], [0.6; 3]);
    shadow_bank.upsert(SubWorldRenderEntry::new(
        11,
        SubWorldTransform::IDENTITY,
        vertices,
        indices,
    ));
    let (vertices, indices) = box_mesh([0.0, 0.0, -2.0], [2.0, 3.0, 0.0], [0.6; 3]);
    shadow_bank.upsert(SubWorldRenderEntry::new(
        12,
        SubWorldTransform::IDENTITY,
        vertices,
        indices,
    ));
    let camera = Camera {
        position: [-1.0, 8.0, 14.0],
        yaw_deg: 0.0,
        pitch_deg: -30.0,
        far: 96.0,
        ..Camera::default()
    };
    let (width, height) = (320, 240);
    let off = composed_rgba(&mut renderer, &mut shadow_bank, &camera, width, height, 2);
    assert_eq!(
        renderer.set_shadow_quality(BuiltinShadowQuality::High),
        BuiltinShadowQuality::High
    );
    let high = composed_rgba(&mut renderer, &mut shadow_bank, &camera, width, height, 2);
    let clip = mat4_mul_vec4(
        &camera.view_projection(width as f32 / height as f32),
        [-0.65, 0.01, -1.0, 1.0],
    );
    let x = ((clip[0] / clip[3] * 0.5 + 0.5) * width as f32) as usize;
    let y = ((0.5 - clip[1] / clip[3] * 0.5) * height as f32) as usize;
    let brightness = |pixels: &[u8]| {
        let mut brightness = 0u32;
        for dy in -1..=1 {
            for dx in -1..=1 {
                brightness += sample_rgb(
                    pixels,
                    width,
                    x.strict_add_signed(dx),
                    y.strict_add_signed(dy),
                )
                .into_iter()
                .map(u32::from)
                .sum::<u32>();
            }
        }
        brightness as f32 / 27.0
    };
    let off_luma = brightness(&off);
    let high_luma = brightness(&high);
    eprintln!("CSM atlas regression: Off={off_luma:.2}, High={high_luma:.2}");
    assert!(
        high_luma + 4.0 < off_luma,
        "High CSM did not darken the shadowed floor"
    );
}
