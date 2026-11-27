// SPDX-License-Identifier: LGPL-3.0-or-later

use megu_client_gpu::renderer::sky_wgsl_f16;
use megu_client_mesh::builtin_shadow_wgsl::MEGU_SKY_ATMOSPHERE_WGSL;
use wgpu::naga::{
    front::wgsl,
    valid::{Capabilities, ValidationFlags, Validator},
};

#[test]
fn shared_sky_validates_with_a_linear_day_palette_and_only_star_hashing() {
    let sky = MEGU_SKY_ATMOSPHERE_WGSL;
    let module = wgsl::parse_str(sky).unwrap_or_else(|err| panic!("{}", err.emit_to_string(sky)));
    Validator::new(ValidationFlags::all(), Capabilities::empty())
        .validate(&module)
        .unwrap_or_else(|err| panic!("{}", err.emit_to_string(sky)));
    for term in [
        "day_zenith = vec3<f32>(0.18782, 0.38643, 1.0)",
        "day_horizon = vec3<f32>(0.52712, 0.68669, 1.0)",
        "night = vec3<f32>(0.006, 0.014, 0.034)",
        "twilight = 1.0 - smoothstep",
        "mix(dusk_glow, dawn_glow, dawn_factor)",
        "shadow.celestial_params.x > 0.001 && view.y > 0.02",
        "floor(view * 42.0)",
        "return vec4<f32>(max(vec3<f32>(color), vec3<f32>(0.0)), 1.0);",
    ] {
        assert!(sky.contains(term), "missing sky term {term}");
    }
    assert_eq!(sky.matches("fract(sin(dot(cell,").count(), 1);
    assert!(!sky.contains("cloud_presence"));
    assert!(!sky.contains("31313.7"));
}

#[test]
fn sky_variants_share_all_shading_and_differ_only_in_accumulator_types() {
    let half = sky_wgsl_f16();
    assert_eq!(
        half.strip_prefix("enable f16;\n")
            .expect("half capability")
            .replace("alias SkyColor = vec3<f16>;", "alias SkyColor = vec3<f32>;")
            .replace("alias SkyWeight = f16;", "alias SkyWeight = f32;"),
        MEGU_SKY_ATMOSPHERE_WGSL,
    );
}

// ---------------------------------------------------------------------------
// JAR-PARITY-15 renderer math: lightmap ramp, vanilla fog, clip planes,
// deterministic star field, sunrise fan and the End sky tiling.
// ---------------------------------------------------------------------------

use megu_client_gpu::lightmap::{
    dimension_ambient_light, dimension_has_constant_ambient_light, gamma_curve, get_brightness,
    light_curve, vanilla_sky_darken, LightTexture, LightTextureParams, END_AMBIENT_LIGHT,
    END_CONSTANT_AMBIENT_BLEND, END_CONSTANT_AMBIENT_TINT, NETHER_AMBIENT_LIGHT,
    OVERWORLD_AMBIENT_LIGHT,
};
use megu_client_gpu::world_env_pass::{
    sunrise_sunset_alpha, vanilla_star_brightness, vanilla_star_field, CELESTIAL_WEATHER_WGSL,
    END_SKY_TILES, END_SKY_TINT, SUNRISE_FAN_DEPTH, SUNRISE_FAN_RADIUS, SUNRISE_FAN_SEGMENTS,
    VANILLA_END_SKY_EXTENT, VANILLA_STAR_COUNT, VANILLA_STAR_RADIUS, VANILLA_STAR_SEED,
    VANILLA_STAR_SIZE_BASE, VANILLA_STAR_SIZE_JITTER,
};
use megu_client_gpu::{
    vanilla_far_plane, vanilla_render_distance_blocks, Camera, VANILLA_DEPTH_FAR_SCALE,
    VANILLA_NEAR_PLANE,
};

#[test]
fn lightmap_ramp_is_the_vanilla_four_minus_three_curve() {
    // `l=0.5` maps to `0.2` — the retired `l / (3 - 2l)` gave `0.25`.
    assert!((light_curve(0.5) - 0.20).abs() < 1.0e-6);
    assert!((light_curve(0.0)).abs() < 1.0e-6);
    assert!((light_curve(1.0) - 1.0).abs() < 1.0e-6);
    // Per-dimension ambient floors.
    assert!((get_brightness(OVERWORLD_AMBIENT_LIGHT, 0)).abs() < 1.0e-6);
    assert!((get_brightness(NETHER_AMBIENT_LIGHT, 0) - 0.1).abs() < 1.0e-6);
    assert!((get_brightness(END_AMBIENT_LIGHT, 0) - 0.25).abs() < 1.0e-6);
    assert!((get_brightness(END_AMBIENT_LIGHT, 15) - 1.0).abs() < 1.0e-6);
    // `c(x) = 1 - (1 - x)^4`.
    assert!((gamma_curve(0.5) - 0.9375).abs() < 1.0e-6);
    assert_eq!(
        [
            dimension_ambient_light(0),
            dimension_ambient_light(1),
            dimension_ambient_light(2)
        ],
        [0.0, 0.1, 0.25]
    );
    assert!(dimension_has_constant_ambient_light(2));
    assert!(!dimension_has_constant_ambient_light(0));
}

#[test]
fn light_texture_is_sixteen_square_and_end_tinted() {
    let noon = LightTexture::new(LightTextureParams::default());
    assert!(noon.brightness(15, 0) > 0.9);
    assert!(noon.brightness(0, 0) > 0.0);
    // `sky_darken` scales the block-light contribution, so a torch-lit texel
    // dims at night while the sky ramp alone is clock independent.
    let night = LightTexture::new(LightTextureParams {
        sky_darken: 0.2,
        ..LightTextureParams::default()
    });
    assert!(night.brightness(15, 0) < noon.brightness(15, 0));
    assert!(noon.brightness(15, 0) > 0.9);
    // The End's constant-ambient matrix keeps its texels inside the unit cube.
    let end = LightTexture::new(LightTextureParams {
        ambient_light: END_AMBIENT_LIGHT,
        constant_ambient_light_end: true,
        ..LightTextureParams::default()
    });
    assert!(end.brightness(0, 0) > 0.0);
    assert!((END_CONSTANT_AMBIENT_BLEND - 0.25).abs() < 1.0e-6);
    assert_eq!(END_CONSTANT_AMBIENT_TINT, [0.99, 1.12, 1.0]);
    // Vanilla sky brightness: 1.0 at noon, 0.2 at midnight.
    assert!((vanilla_sky_darken(6_000, 0.0, 0.0) - 1.0).abs() < 1.0e-6);
    assert!((vanilla_sky_darken(18_000, 0.0, 0.0) - 0.2).abs() < 1.0e-6);
}

#[test]
fn camera_uses_the_vanilla_near_plane_and_four_times_depth_far() {
    assert!((VANILLA_NEAR_PLANE - 0.05).abs() < 1.0e-6);
    assert!((VANILLA_DEPTH_FAR_SCALE - 4.0).abs() < 1.0e-6);
    // `rd = chunks * 16`, far plane = `rd * 4`.
    assert!((vanilla_render_distance_blocks(12) - 192.0).abs() < 1.0e-6);
    assert!((vanilla_far_plane(12) - 768.0).abs() < 1.0e-6);
    assert!((vanilla_far_plane(32) - 2048.0).abs() < 1.0e-6);
    let camera = Camera::default();
    assert!((camera.near - 0.05).abs() < 1.0e-6);
    assert!((camera.far - 768.0).abs() < 1.0e-6);
    // The live window pushes the render distance in blocks; the renderer
    // widens it to the vanilla depth far and pins the near plane.
    let live = Camera {
        far: vanilla_render_distance_blocks(12),
        near: 0.5,
        ..Camera::default()
    };
    let render = live.with_vanilla_clip_planes();
    assert!((render.near - 0.05).abs() < 1.0e-6);
    assert!((render.far - 768.0).abs() < 1.0e-6);
    // A 32-chunk view reaches 2048 blocks, never the retired 512/192 wall.
    let far_view = Camera {
        far: vanilla_render_distance_blocks(32),
        ..Camera::default()
    }
    .with_vanilla_clip_planes();
    assert!((far_view.far - 2048.0).abs() < 1.0e-6);
}

#[test]
fn star_field_is_vanilla_deterministic_geometry() {
    let stars = vanilla_star_field();
    assert_eq!(stars, vanilla_star_field(), "star field must be deterministic");
    // Vanilla draws one quad per accepted candidate, so the drawn count is
    // below the candidate count.
    assert!(
        stars.len() < VANILLA_STAR_COUNT,
        "Vanilla rejects candidates outside the unit sphere"
    );
    assert_eq!(VANILLA_STAR_SEED, 10842);
    assert!((VANILLA_STAR_RADIUS - 100.0).abs() < 1.0e-6);
    for star in &stars {
        let length = (star.direction[0] * star.direction[0]
            + star.direction[1] * star.direction[1]
            + star.direction[2] * star.direction[2])
            .sqrt();
        assert!((length - 1.0).abs() < 1.0e-5, "direction must be unit length");
        assert!(star.size >= VANILLA_STAR_SIZE_BASE);
        assert!(star.size < VANILLA_STAR_SIZE_BASE + VANILLA_STAR_SIZE_JITTER + 1.0e-6);
        assert!((0.0..=std::f32::consts::TAU).contains(&star.roll));
    }
    // Pinned count from the pinned seed: 1500 candidates, 780 accepted.
    assert_eq!(stars.len(), 780);
}

#[test]
fn star_brightness_and_horizon_glow_follow_the_day_clock() {
    assert!((vanilla_star_brightness(6_000)).abs() < 1.0e-6);
    let midnight = vanilla_star_brightness(18_000);
    assert!((midnight - 0.5).abs() < 1.0e-3, "midnight glow {midnight}");
    // The horizon fan peaks when the sun crosses the horizon and dies when
    // the sun is overhead.
    let dawn = sunrise_sunset_alpha(0, 0.0);
    let noon = sunrise_sunset_alpha(6_000, 0.0);
    assert!(dawn > noon);
    assert!((noon).abs() < 1.0e-6);
    // Rain suppresses the glow.
    assert!(sunrise_sunset_alpha(0, 1.0) < dawn);
    assert!((SUNRISE_FAN_RADIUS - 120.0).abs() < 1.0e-6);
    assert!((SUNRISE_FAN_DEPTH - 40.0).abs() < 1.0e-6);
    assert_eq!(SUNRISE_FAN_SEGMENTS, 16);
}

#[test]
fn end_sky_tiles_sixteen_times_with_the_forty_step_tint() {
    assert!((END_SKY_TILES - 16.0).abs() < 1.0e-6);
    assert!((END_SKY_TINT - 40.0 / 255.0).abs() < 1.0e-9);
    assert!((VANILLA_END_SKY_EXTENT - 100.0).abs() < 1.0e-6);
    let sky = CELESTIAL_WEATHER_WGSL;
    assert!(
        sky.contains("end_sky.rgb * environment.sky_params.z"),
        "the End sky tint must ride the sky uniform"
    );
    assert!(sky.contains("texture_index == 6u"), "star branch");
    assert!(sky.contains("texture_index == 7u"), "sunrise branch");
    assert!(sky.contains("environment.sky_params.w"), "star day-phase spin");
}
