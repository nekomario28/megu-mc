// SPDX-License-Identifier: LGPL-3.0-or-later

use std::path::PathBuf;

use megu_client_gpu::atmosphere::{AtmosphereParams, BiomeAtmosphereBlender, ChunkFadeTracker};
use megu_client_gpu::renderer::{
    block_selection_outline_lines, request_gpu_device, screenshot_output_path, write_rgba_png,
    ComposedFrame, FirstPersonViewmodel, GameplayScreenOverlay, GpuRenderer, OFFSCREEN_FORMAT,
};
use megu_client_gpu::subworld_render::SubWorldRenderBank;
use megu_client_gpu::vanilla_assets_bridge::VanillaRuntimeAssets;
use megu_client_gpu::vanilla_world_assets::VanillaWorldAssets;
use megu_client_gpu::Camera;
use megu_client_mesh::RenderBackendPreference;

#[test]
fn selection_outline_is_a_twelve_edge_inflated_box() {
    let lines = block_selection_outline_lines([2, 3, 4]);
    assert_eq!(lines.len(), 12);
    assert!(lines.iter().all(|line| line.rgba == [0, 0, 0, 115]));
    assert!(lines.iter().any(|line| line.start[0] < 2.0));
    assert!(lines.iter().any(|line| line.end[1] > 4.0));
}

#[test]
fn screenshot_path_is_stable_and_scoped_to_game_dir() {
    assert_eq!(
        screenshot_output_path(std::path::Path::new("/game"), 42),
        PathBuf::from("/game/screenshots/Megu_42.png")
    );
}

#[test]
fn png_helper_encodes_rgba_and_rejects_wrong_buffer_size() {
    let path = std::env::temp_dir().join(format!("megu-visuals-{}.png", std::process::id()));
    let pixels = [255, 0, 0, 255, 0, 255, 0, 128];
    write_rgba_png(&path, 2, 1, &pixels).unwrap();
    let encoded = std::fs::read(&path).unwrap();
    let decoded = megu_client_mesh::png_decode::decode_png_rgba(&encoded).unwrap();
    assert_eq!((decoded.0, decoded.1), (2, 1));
    assert_eq!(decoded.2, pixels);
    std::fs::remove_file(path).unwrap();
    assert!(write_rgba_png(std::path::Path::new("unused.png"), 2, 1, &[0; 4]).is_err());
}

#[test]
fn missing_local_assets_build_a_diagnostic_fallback_bundle() {
    let bundle = VanillaRuntimeAssets::load_runtime_assets(None, None, None, &[], "ja_jp");
    let summary = bundle.summary();
    assert_eq!(summary.locale, "ja_jp");
    assert_eq!(summary.world_textures, 27);
    assert!(!bundle.mob_effect_uvs.is_empty());
    assert!(bundle.gui_textures.width >= 256);
    assert!(summary.notes > 0);
}

fn visual_renderer() -> Option<GpuRenderer> {
    match request_gpu_device(
        RenderBackendPreference::Auto,
        None,
        None,
    ) {
        Ok(ctx) => Some(GpuRenderer::new(
            ctx,
            OFFSCREEN_FORMAT,
        )),
        Err(error) => {
            eprintln!("SKIP renderer visual GPU test: no wgpu adapter available ({error})");
            None
        }
    }
}

fn render_visual_frame(
    renderer: &mut GpuRenderer,
    camera: &Camera,
) -> Vec<u8> {
    let mut bank = SubWorldRenderBank::new();
    let atmosphere = BiomeAtmosphereBlender::new(
        AtmosphereParams::default(),
    );
    let fades = ChunkFadeTracker::new();
    renderer
        .render_offscreen_composed_frame(
            96,
            96,
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
        .expect("composed visual frame")
}

fn visual_camera() -> Camera {
    Camera {
        position: [0.5, 0.5, 2.5],
        yaw_deg: 0.0,
        pitch_deg: 0.0,
        far: 128.0,
        ..Camera::default()
    }
}

fn changed_pixels(baseline: &[u8], changed: &[u8]) -> usize {
    assert_eq!(baseline.len(), changed.len());
    let (baseline, baseline_tail) = baseline.as_chunks::<4>();
    let (changed, changed_tail) = changed.as_chunks::<4>();
    assert!(baseline_tail.is_empty() && changed_tail.is_empty());
    baseline
        .iter()
        .zip(changed.iter())
        .filter(|(a, b)| a != b)
        .count()
}

fn nonzero_rgb_pixels(pixels: &[u8]) -> usize {
    let (pixels, tail) = pixels.as_chunks::<4>();
    assert!(tail.is_empty());
    pixels
        .iter()
        .filter(|px| px[..3].iter().any(|&channel| channel != 0))
        .count()
}

fn upload_fallback_world_assets(renderer: &mut GpuRenderer) {
    renderer.upload_world_env_assets(&VanillaWorldAssets::fallback());
}

#[test]
fn gpu_block_break_overlay_renders_changed_crack_pixels() {
    let Some(mut renderer) = visual_renderer() else {
        return;
    };
    upload_fallback_world_assets(&mut renderer);
    let camera = visual_camera();
    let baseline = render_visual_frame(&mut renderer, &camera);
    renderer.set_block_break_overlay(Some(([0, 0, -2], 9)));
    let cracked = render_visual_frame(&mut renderer, &camera);
    let changed = changed_pixels(&baseline, &cracked);
    assert!(
        changed > 16,
        "block-break overlay must draw non-zero changed crack pixels, got {changed}"
    );
}

#[test]
fn gpu_first_person_viewmodel_renders_changed_hand_pixels() {
    let Some(mut renderer) = visual_renderer() else {
        return;
    };
    let camera = visual_camera();
    let baseline = render_visual_frame(&mut renderer, &camera);
    renderer.upload_viewmodel_surface_texture(1, 1, &[220, 160, 120, 255]);
    renderer.set_viewmodel_extras(None, false, [1.0, 1.0, 1.0], 1.25);
    renderer.set_first_person_viewmodel(FirstPersonViewmodel {
        visible: true,
        held_item_name: Some("minecraft:stone".to_string()),
        held_color_rgb: [0.45, 0.48, 0.50],
        is_block: true,
        equip_progress: 1.0,
        ..FirstPersonViewmodel::default()
    });
    let with_hand = render_visual_frame(&mut renderer, &camera);
    let changed = changed_pixels(&baseline, &with_hand);
    assert!(
        changed > 16,
        "first-person viewmodel must draw non-zero changed hand pixels, got {changed}"
    );
}

#[test]
fn gpu_title_panorama_renders_nonzero_cubemap_pixels() {
    use megu_client_gpu::title_branding::RgbaImage;
    use megu_client_gpu::vanilla_gui_sprites::{VanillaGuiAssets, VanillaGuiSpriteId};

    let Some(mut renderer) = visual_renderer() else {
        return;
    };
    let mut assets = VanillaGuiAssets::default();
    for (index, id) in [
        VanillaGuiSpriteId::Panorama0,
        VanillaGuiSpriteId::Panorama1,
        VanillaGuiSpriteId::Panorama2,
        VanillaGuiSpriteId::Panorama3,
        VanillaGuiSpriteId::Panorama4,
        VanillaGuiSpriteId::Panorama5,
    ]
    .into_iter()
    .enumerate()
    {
        let mut rgba = Vec::with_capacity(4 * 4 * 4);
        for _ in 0..16 {
            rgba.extend_from_slice(&[40 + index as u8 * 30, 90, 180, 255]);
        }
        assets.sprites.insert(
            id,
            RgbaImage {
                width: 4,
                height: 4,
                rgba,
            },
        );
    }
    assert!(renderer.upload_title_panorama(&assets, None));
    assert!(renderer.has_title_panorama());
    renderer.set_title_panorama_state(true, 0.3, -0.1, 1.0);
    let pixels = render_visual_frame(&mut renderer, &visual_camera());
    let nonzero = nonzero_rgb_pixels(&pixels);
    assert!(
        nonzero > 1_000,
        "active title panorama must fill the frame with non-zero cubemap pixels, got {nonzero}"
    );
}

#[test]
fn gpu_textured_gameplay_overlay_renders_changed_screen_pixels() {
    let Some(mut renderer) = visual_renderer() else {
        return;
    };
    upload_fallback_world_assets(&mut renderer);
    let camera = visual_camera();
    let baseline = render_visual_frame(&mut renderer, &camera);
    renderer.set_spyglass_alpha(0.35);
    renderer.set_gameplay_screen_overlay(GameplayScreenOverlay {
        hurt_alpha: 0.45,
        fire_alpha: 0.35,
        underwater_alpha: 0.25,
        vignette_alpha: 0.30,
        time_secs: 2.0,
        ..GameplayScreenOverlay::default()
    });
    let overlaid = render_visual_frame(&mut renderer, &camera);
    let changed = changed_pixels(&baseline, &overlaid);
    assert!(
        changed > 16,
        "textured gameplay overlay must draw non-zero changed screen pixels, got {changed}"
    );
}

#[test]
fn srgb_ui_vertex_color_round_trips_through_the_offscreen_target() {
    use megu_ui::draw::{tessellate_draw_list, UiColor, UiDrawList, UiRect};

    let Some(mut renderer) = visual_renderer() else {
        return;
    };
    let mut draw = UiDrawList::default();
    let rgb = [54, 77, 97, 255];
    draw.fill_rect(
        UiRect {
            x: 0,
            y: 0,
            w: 96,
            h: 96,
        },
        UiColor::rgba(rgb[0], rgb[1], rgb[2], rgb[3]),
    );
    renderer.update_ui_mesh(96, 96, &tessellate_draw_list(96, 96, &draw));
    let rgba = render_visual_frame(&mut renderer, &visual_camera());
    let center = (48 * 96 + 48) * 4;
    for (actual, expected) in rgba[center..center + 4].iter().zip(rgb) {
        assert!(
            actual.abs_diff(expected) <= 1,
            "got {:?}",
            &rgba[center..center + 4]
        );
    }
}

#[test]
fn textured_cloud_deck_is_visible_above_a_ground_level_camera() {
    use megu_client_gpu::vanilla_world_assets::{DecodedRgbaTexture, VanillaEnvTexture};

    let Some(mut renderer) = visual_renderer() else {
        return;
    };
    let camera = Camera {
        position: [0.0, 72.0, 0.0],
        pitch_deg: 75.0,
        ..Camera::default()
    };
    let mut assets = VanillaWorldAssets::fallback();
    assets.env_textures.remove(&VanillaEnvTexture::Clouds);
    renderer.upload_world_env_assets(&assets);
    let clear = render_visual_frame(&mut renderer, &camera);
    assets.env_textures.insert(
        VanillaEnvTexture::Clouds,
        DecodedRgbaTexture {
            width: 256,
            height: 256,
            rgba: vec![255; 256 * 256 * 4],
            from_jar: false,
        },
    );
    renderer.upload_world_env_assets(&assets);
    let cloudy = render_visual_frame(&mut renderer, &camera);
    let center = (48 * 96 + 48) * 4;
    assert!(
        cloudy[center].saturating_sub(clear[center]) > 40,
        "{:?} vs {:?}",
        &cloudy[center..center + 3],
        &clear[center..center + 3]
    );
    assert!(cloudy[center..center + 3]
        .iter()
        .all(|channel| *channel > 200));
}

#[test]
fn gpu_world_env_frame_renders_changed_environment_pixels() {
    let Some(mut renderer) = visual_renderer() else {
        return;
    };
    upload_fallback_world_assets(&mut renderer);
    let camera = visual_camera();
    let baseline = render_visual_frame(&mut renderer, &camera);
    renderer.set_world_env_frame(2, 1.0, true, 3, [0.0, 1.0, 0.0], 2.0);
    let environment = render_visual_frame(&mut renderer, &camera);
    let changed = changed_pixels(&baseline, &environment);
    assert!(
        changed > 16,
        "world environment frame must draw non-zero changed environment pixels, got {changed}"
    );
}

#[test]
fn gpu_render_to_view_path_encodes_first_person_viewmodel() {
    let Some(mut renderer) = visual_renderer() else {
        return;
    };
    let camera = visual_camera();
    let baseline = renderer
        .render_offscreen_rgba(96, 96, &camera, None)
        .expect("baseline render_to_view frame");
    renderer.set_first_person_viewmodel(FirstPersonViewmodel {
        visible: true,
        equip_progress: 1.0,
        ..FirstPersonViewmodel::default()
    });
    let with_hand = renderer
        .render_offscreen_rgba(96, 96, &camera, None)
        .expect("viewmodel render_to_view frame");
    let changed = changed_pixels(&baseline, &with_hand);
    assert!(
        changed > 16,
        "render_to_view must encode the first-person viewmodel, got {changed} changed pixels"
    );
}
