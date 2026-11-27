// SPDX-License-Identifier: LGPL-3.0-or-later

use std::sync::{Arc, Mutex};

use megu_client_gpu::aligned_copy_bytes_per_row;
use megu_client_gpu::renderer::{request_gpu_device, GameplayScreenOverlay};
use megu_client_gpu::vanilla_world_assets::VanillaWorldAssets;
use megu_client_gpu::world_env_pass::{
    end_sky_enabled, moon_phase_uv, overlay_effects_active, precipitation_scroll,
    CelestialWeatherFrame, TexturedCelestialWeatherPass, TexturedGameplayOverlayPass,
    CELESTIAL_WEATHER_WGSL, DIMENSION_KIND_END, DIMENSION_KIND_NETHER, DIMENSION_KIND_OVERWORLD,
    TEXTURED_OVERLAY_WGSL, WORLD_ENV_PASS_DEPTH_FORMAT,
};
use megu_client_mesh::RenderBackendPreference;
use wgpu::naga::front::wgsl;
use wgpu::naga::valid::{Capabilities, ValidationFlags, Validator};

fn validate_wgsl(name: &str, source: &str) {
    let module = wgsl::parse_str(source).unwrap_or_else(|error| {
        panic!("{name}: naga WGSL parse failed:\n{error}");
    });
    Validator::new(ValidationFlags::all(), Capabilities::all())
        .validate(&module)
        .unwrap_or_else(|error| {
            panic!("{name}: naga WGSL validation failed:\n{error}");
        });
}

#[test]
fn celestial_weather_wgsl_parses_and_validates_with_naga() {
    validate_wgsl("CELESTIAL_WEATHER_WGSL", CELESTIAL_WEATHER_WGSL);
}

#[test]
fn textured_overlay_wgsl_parses_and_validates_with_naga() {
    validate_wgsl("TEXTURED_OVERLAY_WGSL", TEXTURED_OVERLAY_WGSL);
}

#[test]
fn moon_phase_uv_covers_the_vanilla_four_by_two_sheet() {
    for phase in 0..8u8 {
        let col = f32::from(phase % 4);
        let row = f32::from((phase / 4) % 2);
        assert_eq!(
            moon_phase_uv(phase),
            [col * 0.25, row * 0.5, (col + 1.0) * 0.25, (row + 1.0) * 0.5],
            "phase {phase} must select its exact moon_phases.png cell",
        );
    }
    assert_eq!(moon_phase_uv(8), moon_phase_uv(0));
    assert_eq!(moon_phase_uv(31), moon_phase_uv(7));
}

#[test]
fn precipitation_scroll_is_fast_vertical_rain_or_drifting_snow() {
    let rain = precipitation_scroll(0.25, false);
    let snow = precipitation_scroll(0.25, true);
    assert!(
        rain[1] > snow[1] * 8.0,
        "rain must scroll vertically much faster than snow: rain={rain:?}, snow={snow:?}",
    );
    assert!(
        snow[0] > 0.0,
        "snow must have a gentle horizontal drift component: {snow:?}",
    );
    for value in rain.into_iter().chain(snow) {
        assert!((0.0..1.0).contains(&value));
    }

    let wrapped = precipitation_scroll(1.0, false);
    assert!((wrapped[1] - 0.35).abs() < 1.0e-6);
}

#[test]
fn end_sky_is_triggered_only_by_the_end_dimension() {
    assert!(!end_sky_enabled(DIMENSION_KIND_OVERWORLD));
    assert!(!end_sky_enabled(DIMENSION_KIND_NETHER));
    assert!(end_sky_enabled(DIMENSION_KIND_END));
    assert!(!end_sky_enabled(DIMENSION_KIND_END + 1));
}

#[test]
fn overlay_fast_path_skips_only_when_every_alpha_is_zero() {
    let mut overlay = GameplayScreenOverlay {
        time_secs: 123.0,
        ..GameplayScreenOverlay::default()
    };
    assert!(!overlay_effects_active(&overlay, 0.0));
    assert!(!TexturedGameplayOverlayPass::effects_active(&overlay, 0.0));
    assert!(overlay_effects_active(&overlay, 0.01));

    overlay.hurt_alpha = f32::NAN;
    overlay.fire_alpha = f32::NEG_INFINITY;
    assert!(!overlay_effects_active(&overlay, 0.0));

    overlay.hurt_alpha = 0.01;
    assert!(overlay_effects_active(&overlay, 0.0));
}

#[test]
fn gpu_passes_construct_and_encode_without_validation_errors() {
    let ctx = match request_gpu_device(RenderBackendPreference::Auto, None, None) {
        Ok(ctx) => ctx,
        Err(error) => {
            eprintln!("SKIP world-env GPU encode test: no wgpu adapter available ({error})");
            return;
        }
    };
    let errors: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    {
        let errors = Arc::clone(&errors);
        ctx.device.on_uncaptured_error(Arc::new(move |error| {
            errors
                .lock()
                .expect("error capture")
                .push(error.to_string());
        }));
    }

    let format = wgpu::TextureFormat::Rgba8Unorm;
    let assets = VanillaWorldAssets::fallback();
    let environment_pass =
        TexturedCelestialWeatherPass::new(&ctx.device, &ctx.queue, format, &assets);
    let overlay_pass = TexturedGameplayOverlayPass::new(&ctx.device, &ctx.queue, format, &assets);
    let color = ctx.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("world-env-test-color"),
        size: wgpu::Extent3d {
            width: 64,
            height: 64,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let depth = ctx.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("world-env-test-depth"),
        size: color.size(),
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: WORLD_ENV_PASS_DEPTH_FORMAT,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
        view_formats: &[],
    });
    let color_view = color.create_view(&wgpu::TextureViewDescriptor::default());
    let depth_view = depth.create_view(&wgpu::TextureViewDescriptor::default());
    let mut encoder = ctx
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("world-env-test-encoder"),
        });
    {
        let clear_pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("world-env-test-clear"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &color_view,
                resolve_target: None,
                depth_slice: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                view: &depth_view,
                depth_ops: Some(wgpu::Operations {
                    load: wgpu::LoadOp::Clear(1.0),
                    store: wgpu::StoreOp::Store,
                }),
                stencil_ops: None,
            }),
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });
        drop(clear_pass);
    }

    let frame = CelestialWeatherFrame {
        rain_level: 1.0,
        cloud_alpha: 1.0,
        time_secs: 2.0,
        ..CelestialWeatherFrame::default()
    };
    let stats = environment_pass.encode(&mut encoder, &color_view, &depth_view, &frame);
    assert_eq!(stats.alpha_quads, 13);
    assert_eq!(stats.additive_quads, 2);
    assert_eq!(stats.written_vertices, 60);

    let end_stats = environment_pass.encode(
        &mut encoder,
        &color_view,
        &depth_view,
        &CelestialWeatherFrame {
            dimension_kind: DIMENSION_KIND_END,
            ..CelestialWeatherFrame::default()
        },
    );
    assert_eq!(end_stats.alpha_quads, 6);
    assert_eq!(end_stats.additive_quads, 0);
    assert_eq!(end_stats.written_vertices, 24);

    let overlay = GameplayScreenOverlay {
        fire_alpha: 0.8,
        underwater_alpha: 0.2,
        vignette_alpha: 0.2,
        time_secs: 2.0,
        ..GameplayScreenOverlay::default()
    };
    assert!(overlay_pass.encode(&mut encoder, &color_view, 64, 64, &overlay, 0.25));
    assert!(!overlay_pass.encode(
        &mut encoder,
        &color_view,
        64,
        64,
        &GameplayScreenOverlay::default(),
        0.0,
    ));
    let padded_row = aligned_copy_bytes_per_row(64);
    let readback = ctx.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("world-env-test-readback"),
        size: u64::from(padded_row) * 64,
        usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    encoder.copy_texture_to_buffer(
        wgpu::TexelCopyTextureInfo {
            texture: &color,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        wgpu::TexelCopyBufferInfo {
            buffer: &readback,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(padded_row),
                rows_per_image: Some(64),
            },
        },
        color.size(),
    );
    ctx.queue.submit([encoder.finish()]);
    ctx.device
        .poll(wgpu::PollType::wait_indefinitely())
        .expect("poll world-env test device");

    let slice = readback.slice(..);
    let (tx, rx) = std::sync::mpsc::channel();
    slice.map_async(wgpu::MapMode::Read, move |result| {
        let _ = tx.send(result);
    });
    ctx.device
        .poll(wgpu::PollType::wait_indefinitely())
        .expect("poll world-env readback");
    rx.recv().expect("readback channel").expect("map readback");
    let pixels = slice.get_mapped_range().expect("readback range").to_vec();
    readback.unmap();
    let pixel = |x: usize, y: usize| {
        let at = y * padded_row as usize + x * 4;
        [pixels[at], pixels[at + 1], pixels[at + 2], pixels[at + 3]]
    };
    let top_center = pixel(32, 4);
    let bottom_center = pixel(32, 60);
    assert!(
        bottom_center[0] > top_center[0] + 16,
        "bottom fire overlay must redden the lower screen: top={top_center:?}, bottom={bottom_center:?}",
    );
    assert!(
        pixels.iter().step_by(4).any(|&red| red != 0),
        "textured passes must rasterize non-black output",
    );
    let captured_errors = errors.lock().expect("error capture");
    assert!(
        captured_errors.is_empty(),
        "wgpu validation errors: {:?}",
        captured_errors.as_slice(),
    );
}

// ---------------------------------------------------------------------------
// FIX-RICH-SHADOW-LIGHT-ALIGNMENT: the sunrise/sunset fan dissolves instead of
// ending on a crease.
// ---------------------------------------------------------------------------

#[test]
fn sunrise_fan_dissolves_without_a_crease_at_the_band_edge() {
    use megu_client_gpu::world_env_pass::sunrise_sunset_alpha;

    // Peaks on the crossing, dies by the band edge, and never exceeds one.
    assert_eq!(sunrise_sunset_alpha(0, 0.0), 1.0);
    assert_eq!(sunrise_sunset_alpha(6_000, 0.0), 0.0);
    // The fan is a function of |elevation| only: it fades monotonically from
    // sunrise to noon and mirrors dawn against dusk.
    let mut previous = 1.0f32;
    for tick in 0..=6_000u64 {
        let alpha = sunrise_sunset_alpha(tick, 0.0);
        assert!(alpha.is_finite() && (0.0..=1.0).contains(&alpha));
        assert!(alpha <= previous + 1.0e-6, "fan brightened at tick {tick}");
        previous = alpha;
    }
    for tick in [0u64, 1_500, 3_000, 5_000, 6_000, 9_000] {
        let mirrored = (24_000 - tick) % 24_000;
        assert!(
            (sunrise_sunset_alpha(tick, 0.0) - sunrise_sunset_alpha(mirrored, 0.0)).abs()
                < 1.0e-6,
            "dawn and dusk must mirror at tick {tick}"
        );
    }
    // The shoulder is C1 at the edge: the last steps toward zero are tiny,
    // where the retired linear ramp fell at a constant rate and creased.
    let near_edge = sunrise_sunset_alpha(10_500, 0.0);
    assert!(near_edge < 0.05, "edge should fade out, got {near_edge}");
    // Rain suppresses the glow multiplicatively and clamps at zero.
    assert_eq!(sunrise_sunset_alpha(0, 1.0), 0.0);
    assert!(sunrise_sunset_alpha(0, 0.5) > 0.0);
    assert!(sunrise_sunset_alpha(0, 2.0) >= 0.0);
}
