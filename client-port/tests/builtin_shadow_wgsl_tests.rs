// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later

use std::mem::{align_of, offset_of, size_of};

use mc_spine_client_mesh::builtin_shadow_wgsl::*;
use mc_spine_client_mesh::gpu_pipeline::GpuCapabilities;
use mc_spine_client_mesh::shaderpack::{
    load_megu_wgsl_pack, GpuShaderCaps, PipelineStage, ShaderPackSource, MEGU_WGSL_MANIFEST,
};
use mc_spine_client_mesh::vfx_engine::DynamicColoredLight;

#[test]
fn default_off_has_zero_shadow_work() {
    assert_eq!(BuiltinShadowQuality::default(), BuiltinShadowQuality::Off);
    let config = BuiltinShadowQuality::default().config();
    assert_eq!(config.cascade_count, 0);
    assert_eq!(config.map_resolution, 0);
    assert_eq!(config.pcf_taps, 0);
    assert_eq!(config.max_dynamic_lights, 0);
    assert_eq!(config.depth_array_bytes(), 0);
    assert_eq!(
        compute_cascade_splits(0.1, 192.0, 0, 0.65),
        Vec::<f32>::new()
    );
}

#[test]
fn preset_indices_stay_backward_compatible() {
    for (quality, index) in [
        (BuiltinShadowQuality::Off, 0),
        (BuiltinShadowQuality::Low, 1),
        (BuiltinShadowQuality::Medium, 2),
        (BuiltinShadowQuality::High, 3),
        (BuiltinShadowQuality::Feather, 4),
        (BuiltinShadowQuality::Natural, 5),
    ] {
        assert_eq!(quality.index(), index);
    }
    for (quality, name) in [
        (BuiltinShadowQuality::Off, "off"),
        (BuiltinShadowQuality::Feather, "feather"),
        (BuiltinShadowQuality::Low, "low"),
        (BuiltinShadowQuality::Natural, "natural"),
        (BuiltinShadowQuality::Medium, "medium"),
        (BuiltinShadowQuality::High, "high"),
    ] {
        assert_eq!(quality.as_str(), name);
    }
}

#[test]
fn presets_have_expected_budgets() {
    for (quality, cascades, pixels, taps, lights) in [
        (BuiltinShadowQuality::Feather, 0, 0, 0, 16),
        (BuiltinShadowQuality::Low, 1, 1024, 4, 8),
        (BuiltinShadowQuality::Natural, 2, 1536, 9, 32),
        (BuiltinShadowQuality::Medium, 2, 2048, 9, 32),
        (BuiltinShadowQuality::High, 3, 2048, 16, 64),
    ] {
        let config = quality.config();
        assert_eq!(config.cascade_count, cascades);
        assert_eq!(config.map_resolution, pixels);
        assert_eq!(config.pcf_taps, taps);
        assert_eq!(config.max_dynamic_lights, lights);
        assert_eq!(
            config.depth_array_bytes(),
            cascades as u64 * pixels as u64 * pixels as u64 * 4
        );
    }
}

#[test]
fn feather_is_shadow_map_free_and_natural_has_no_camera_post_fx() {
    let feather = BuiltinShadowQuality::Feather.config();
    assert_eq!(feather.cascade_count, 0);
    assert_eq!(feather.map_resolution, 0);
    assert_eq!(feather.depth_array_bytes(), 0);
    assert!(feather.max_dynamic_lights > 0, "Feather keeps VFX1 lights");
    assert!(!feather.camera_post_fx);

    let natural = BuiltinShadowQuality::Natural.config();
    assert!(natural.cascade_count > 0, "Natural keeps soft PCF shadows");
    assert!(!natural.camera_post_fx, "Vanilla+ has no camera gimmicks");
    for quality in [
        BuiltinShadowQuality::Off,
        BuiltinShadowQuality::Feather,
        BuiltinShadowQuality::Low,
        BuiltinShadowQuality::Natural,
        BuiltinShadowQuality::Medium,
        BuiltinShadowQuality::High,
    ] {
        assert_eq!(
            quality.config().camera_post_fx,
            quality.config().shader_tier >= 3,
            "{quality:?}"
        );
    }
}

#[test]
fn low_spec_never_receives_expensive_quality() {
    use BuiltinShadowQuality::{Feather, High, Low, Medium, Natural, Off};
    let legacy = GpuCapabilities::intel_hd_legacy();
    let integrated = GpuCapabilities::intel_iris_xe();
    let software = GpuCapabilities::software();
    let discrete = GpuCapabilities::nvidia_rtx();
    assert_eq!(negotiate_builtin_shadow_quality(Off, &discrete), Off);
    assert_eq!(negotiate_builtin_shadow_quality(High, &legacy), Low);
    assert_eq!(negotiate_builtin_shadow_quality(High, &integrated), Low);
    assert_eq!(negotiate_builtin_shadow_quality(Medium, &legacy), Low);
    assert_eq!(negotiate_builtin_shadow_quality(Low, &legacy), Low);
    assert_eq!(negotiate_builtin_shadow_quality(High, &software), Off);
    assert_eq!(negotiate_builtin_shadow_quality(High, &discrete), High);
    // Feather never negotiates up into a shadow-map preset and never off on
    // real iGPUs; only a compute-less software rasterizer drops it to Off.
    assert_eq!(
        negotiate_builtin_shadow_quality(Feather, &discrete),
        Feather
    );
    assert_eq!(
        negotiate_builtin_shadow_quality(Feather, &integrated),
        Feather
    );
    assert_eq!(negotiate_builtin_shadow_quality(Feather, &software), Off);
    // Natural degrades gracefully on weak parts, keeps its rank elsewhere.
    assert_eq!(negotiate_builtin_shadow_quality(Natural, &integrated), Low);
    assert_eq!(
        negotiate_builtin_shadow_quality(Natural, &discrete),
        Natural
    );
    assert_eq!(
        negotiate_builtin_shadow_quality(High, &GpuCapabilities::apple_silicon()),
        Medium
    );
    assert_eq!(
        negotiate_builtin_shadow_quality(High, &GpuCapabilities::amd_rdna2()),
        High
    );
}

#[test]
fn splits_are_monotonic_and_end_at_far_plane() {
    for count in 1..=3 {
        for lambda in [0.0, 0.25, 0.65, 1.0] {
            let splits = compute_cascade_splits(0.1, 256.0, count, lambda);
            assert_eq!(splits.len(), count as usize);
            let mut previous = 0.1;
            for split in &splits {
                assert!(*split > previous);
                assert!(*split <= 256.0);
                previous = *split;
            }
            assert_eq!(splits.last().copied(), Some(256.0));
        }
    }
    assert!(compute_cascade_splits(-1.0, 100.0, 3, 0.5).is_empty());
    assert!(compute_cascade_splits(1.0, 1.0, 3, 0.5).is_empty());
    assert!(compute_cascade_splits(f32::NAN, 100.0, 2, 0.5).is_empty());
}

#[test]
fn sun_and_moon_arc_remains_finite_for_every_tick() {
    let mut previous = compute_sun_direction(0);
    for time in 0..=24_000 {
        let sun = compute_sun_direction(time);
        assert!(sun.iter().all(|value| value.is_finite()));
        let length = sun.iter().map(|value| value * value).sum::<f32>().sqrt();
        assert!((length - 1.0).abs() < 1e-5, "{time} length {length}");
        let jump = sun
            .iter()
            .zip(previous)
            .map(|(a, b)| (a - b).powi(2))
            .sum::<f32>()
            .sqrt();
        assert!(jump < 0.0003, "sun jumped at {time}");
        previous = sun;
    }
    assert!(compute_sun_direction(6000)[1] > 0.999);
    assert!(compute_sun_direction(18000)[1] < -0.999);
    assert_eq!(compute_sun_direction(0), compute_sun_direction(24_000));
    assert!(compute_sun_direction(u64::MAX)
        .iter()
        .all(|value| value.is_finite()));
}

#[test]
fn inei_shadow_balance_layers_eaves_without_losing_playable_ambient() {
    let eaves = compute_inei_shadow_balance(0.0, 0.8);
    assert!(eaves.sky_ambient_scale < 0.55);
    assert!(eaves.warm_bounce_scale > 1.0);
    assert!(eaves.lantern_presence_boost > 1.25);
    assert!((0.15..=0.30).contains(&eaves.shadow_desaturation));
    assert!((eaves.sky_ambient_scale - 0.42).abs() < 0.00001);
    assert!((eaves.warm_bounce_scale - 1.15).abs() < 0.00001);
    assert!((eaves.shadow_desaturation - 0.22).abs() < 0.00001);
    assert!((eaves.lantern_presence_boost - 1.38).abs() < 0.00001);
    // Even an upward surface with no warm bounce retains the WGSL ambient floor.
    assert!((0.11 * eaves.sky_ambient_scale).max(0.065) >= 0.065);

    let day = compute_inei_shadow_balance(1.0, 0.8);
    assert_eq!(day.sky_ambient_scale, 1.0);
    assert_eq!(day.warm_bounce_scale, 1.0);
    assert_eq!(day.shadow_desaturation, 0.0);
    assert!((day.lantern_presence_boost - 1.0).abs() < 0.00001);

    let night = compute_inei_shadow_balance(1.0, -0.8);
    assert!((night.lantern_presence_boost - 1.38).abs() < 0.00001);

    let penumbra = compute_inei_shadow_balance(0.5, 0.5);
    assert!((penumbra.sky_ambient_scale - 0.71).abs() < 0.00001);
    assert!((penumbra.warm_bounce_scale - 1.075).abs() < 0.00001);
    assert!((penumbra.shadow_desaturation - 0.11).abs() < 0.00001);
    assert!((penumbra.lantern_presence_boost - 1.2375).abs() < 0.00001);
}

#[test]
fn inei_shadow_balance_sanitizes_nonfinite_and_out_of_range_inputs() {
    for visibility in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY, -3.0, 3.0] {
        for sun in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY, -3.0, 3.0] {
            let balance = compute_inei_shadow_balance(visibility, sun);
            assert!((0.42..=1.0).contains(&balance.sky_ambient_scale));
            assert!((1.0..=1.15).contains(&balance.warm_bounce_scale));
            assert!((0.0..=0.22).contains(&balance.shadow_desaturation));
            assert!((1.0..=1.38).contains(&balance.lantern_presence_boost));
            assert!(balance.sky_ambient_scale.is_finite());
            assert!(balance.warm_bounce_scale.is_finite());
            assert!(balance.shadow_desaturation.is_finite());
            assert!(balance.lantern_presence_boost.is_finite());
        }
    }
}

#[test]
fn yugen_mist_layers_valleys_and_grows_near_dawn_and_dusk() {
    let river = compute_yugen_valley_mist(62.0, 80.0, 80.0, 0.012, 0.25);
    let ridge = compute_yugen_valley_mist(140.0, 80.0, 80.0, 0.012, 0.25);
    assert!(river > ridge, "river {river}, ridge {ridge}");
    let dawn = compute_yugen_valley_mist(62.0, 80.0, 80.0, 0.012, 0.0);
    let dusk = compute_yugen_valley_mist(62.0, 80.0, 80.0, 0.012, 0.5);
    assert!(dawn > river, "dawn {dawn}, noon {river}");
    assert!(dusk > river, "dusk {dusk}, noon {river}");
    assert!(compute_yugen_valley_mist(62.0, 80.0, 1e9, 100.0, 0.0) <= 0.92);
    assert_eq!(compute_yugen_valley_mist(62.0, 80.0, 0.0, 0.012, 0.0), 0.0);
    assert_eq!(compute_yugen_valley_mist(62.0, 80.0, 80.0, 0.0, 0.0), 0.0);
}

#[test]
fn yugen_mist_sanitizes_nonfinite_inputs() {
    let baseline = [62.0, 80.0, 64.0, 0.012, 0.25];
    for index in 0..5 {
        for invalid in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            let mut inputs = baseline;
            inputs[index] = invalid;
            let result =
                compute_yugen_valley_mist(inputs[0], inputs[1], inputs[2], inputs[3], inputs[4]);
            assert!(result.is_finite(), "index {index}, value {invalid}");
            assert!((0.0..=0.92).contains(&result));
        }
    }
    assert_eq!(
        compute_yugen_valley_mist(62.0, 80.0, -10.0, -0.012, 0.0),
        0.0
    );
    assert_eq!(
        compute_yugen_valley_mist(62.0, 80.0, 0.0, f32::MAX, 0.0),
        0.0
    );
    let saturated = compute_yugen_valley_mist(62.0, 80.0, f32::MAX, f32::MAX, 0.0);
    assert_eq!(saturated, 0.92);
}

#[test]
fn komorebi_dapple_is_bounded_smooth_and_neutral_at_night() {
    let pos = [17.25, 71.0, -22.5];
    let first = compute_komorebi_dapple(pos, 0.25, 0.8);
    let shifted = compute_komorebi_dapple([17.27, 71.0, -22.48], 0.25, 0.8);
    let morning = compute_komorebi_dapple(pos, 0.10, 0.8);
    let later = compute_komorebi_dapple(pos, 0.101, 0.8);
    assert!((first - shifted).abs() < 0.02);
    assert!((morning - later).abs() < 0.02);
    assert!((first - shifted).abs() > 1e-6);
    assert!((morning - later).abs() > 1e-6);
    assert_eq!(compute_komorebi_dapple(pos, 0.25, 0.0), 1.0);
    assert_eq!(compute_komorebi_dapple(pos, 0.25, -0.8), 1.0);
    assert_eq!(compute_komorebi_dapple(pos, 0.25, 0.08), 1.0);
    assert_eq!(
        compute_komorebi_dapple(pos, 0.0, 0.8),
        compute_komorebi_dapple(pos, 1.0, 0.8)
    );

    for x in -10..=10 {
        for phase in [0.0, 0.1, 0.25, 0.49, 0.75, 0.99] {
            let value = compute_komorebi_dapple([x as f32, 63.0, x as f32 * 0.7], phase, 0.8);
            assert!((0.85..=1.10).contains(&value), "dapple {value}");
        }
    }
    assert_eq!(
        compute_komorebi_dapple([f32::NAN, 0.0, 2.0], 0.25, 0.8),
        1.0
    );
    assert_eq!(compute_komorebi_dapple(pos, f32::NAN, 0.8), 1.0);
}

#[test]
fn nure_iro_patina_deepens_wet_colors_and_reveals_grazing_sheen() {
    let albedo = [0.65, 0.40, 0.20];
    assert_eq!(
        compute_nure_iro_patina(albedo, 1.0, 0.0, 0.2),
        (albedo, 0.0)
    );
    let (damp, sheen) = compute_nure_iro_patina(albedo, 1.0, 1.0, 0.1);
    for (wet, dry) in damp.into_iter().zip(albedo) {
        assert!(wet > 0.0 && wet < dry, "wet {wet}, dry {dry}");
    }
    assert!(sheen > 0.0 && sheen <= 0.12);
    let (_, facing_sheen) = compute_nure_iro_patina(albedo, 1.0, 1.0, 0.95);
    assert!(facing_sheen < sheen);
    let (_, downward_sheen) = compute_nure_iro_patina(albedo, -1.0, 1.0, 0.1);
    assert_eq!(downward_sheen, 0.0);
    assert_eq!(
        compute_nure_iro_patina(albedo, 1.0, -2.0, 0.1),
        (albedo, 0.0)
    );
    assert!(compute_nure_iro_patina(albedo, 1.0, f32::NAN, 0.1)
        .1
        .is_finite());
}

#[test]
fn inei_wgsl_matches_cpu_curves_and_preserves_warm_lantern_color() {
    let terrain = MEGU_TERRAIN_LIT_WGSL;
    for expression in [
        "mix(0.42, 1.0, visibility)",
        "mix(1.15, 1.0, visibility)",
        "(1.0 - visibility) * 0.22",
        "visibility * clamp(shadow.sun_direction.y * 1.5, 0.0, 1.0)",
        "vec3<f32>(0.065)",
        "(dot(normal, L_dir) + wrap) / (1.0 + wrap)",
        "pow12(max(dot(normal, half_vec), 0.0)) * 0.08",
        "lit_color += rich_albedo * lantern_light + lacquer_sheen",
    ] {
        assert!(
            terrain.contains(expression),
            "missing terrain curve {expression}"
        );
    }
    assert!(terrain.find("let shibumi =").unwrap() < terrain.find("lit_color +=").unwrap());
    let sky = MEGU_SKY_ATMOSPHERE_WGSL;
    assert!(sky.contains("vec3<f32>(0.025, 0.040, 0.13)"));
    assert!(sky.contains("vec3<f32>(0.42, 0.17, 0.10)"));
}

#[test]
fn japanese_aesthetics_curves_are_mirrored_in_wgsl_without_new_bindings() {
    let terrain = MEGU_TERRAIN_LIT_WGSL;
    for curve in [
        "fn yugen_valley_mist(",
        "smoothstep(63.0, 96.0, world_y)",
        "smoothstep(0.0, 0.28, abs(sin(day_phase * 6.2831853)))",
        "clamp(1.0 - exp(-max(view_distance, 0.0) * density), 0.0, 0.92)",
        "let atmosphere = yugen_valley_mist(",
        "fn komorebi_dapple(",
        "0.11 * (0.72 * broad + 0.28 * fine)",
        "visibility < 0.95 || cascade_count == 0u",
        "let wetness = clamp(shadow.render_params.z * 24.0, 0.0, 1.0)",
        "(1.0 - 0.18 * wetness)",
        "pow4(1.0 - clamp(dot(normal, view_dir), 0.0, 1.0))",
    ] {
        assert!(terrain.contains(curve), "missing terrain curve {curve}");
    }
    let water = MEGU_WATER_SURFACE_WGSL;
    for curve in [
        "if (shadow.sun_direction.y < 0.0)",
        "let lunar_path = pow72(max(dot(reflected, moon_axis), 0.0))",
        "* rise * shadow.moon_color.w",
        "return vec4<f32>(color + suigetsu, 0.88)",
    ] {
        assert!(water.contains(curve), "missing water curve {curve}");
    }
    assert_eq!(terrain.matches("@binding(").count(), 4);
    assert_eq!(water.matches("@binding(").count(), 1);
}

#[test]
fn texel_snapping_resists_sub_texel_translation() {
    let camera = ShadowCamera::default();
    let config = BuiltinShadowQuality::High.config();
    let first = build_shadow_uniforms(camera, config, 6500);
    let mut jittered = camera;
    jittered.position[0] += 0.00001;
    jittered.position[2] -= 0.00001;
    let second = build_shadow_uniforms(jittered, config, 6500);
    for layer in 0..3 {
        assert_eq!(
            first.light_view_proj[layer], second.light_view_proj[layer],
            "layer {layer} moved under sub-texel jitter"
        );
    }
    assert!(first
        .light_view_proj
        .iter()
        .flatten()
        .flatten()
        .all(|v| v.is_finite()));
    assert!(first
        .camera_view_proj
        .iter()
        .flatten()
        .all(|v| v.is_finite()));
    assert!(first.cascade_splits[0] < first.cascade_splits[1]);
    assert!(first.cascade_splits[1] < first.cascade_splits[2]);
    assert_eq!(first.with_active_cascade(99).shadow_params[3], 2.0);
    assert_eq!(first, build_shadow_uniforms(camera, config, 6500));
}

#[test]
fn directional_depth_maps_put_sunward_occluders_in_front() {
    let uniform = build_shadow_uniforms(
        ShadowCamera::default(),
        BuiltinShadowQuality::Low.config(),
        6000,
    );
    let matrix = uniform.light_view_proj[0];
    let depth = |position: [f32; 3]| {
        matrix[0][2] * position[0]
            + matrix[1][2] * position[1]
            + matrix[2][2] * position[2]
            + matrix[3][2]
    };
    assert!(depth([0.0, 10.0, 0.0]) < depth([0.0, 0.0, 0.0]));
    assert!(depth([0.0, 0.0, 0.0]) < depth([0.0, -10.0, 0.0]));
}

#[test]
fn uniforms_match_wgsl_std140_alignment_and_byte_offsets() {
    assert_eq!(align_of::<ShadowUniforms>(), 16);
    assert_eq!(size_of::<ShadowUniforms>() % 16, 0);
    assert_eq!(size_of::<ShadowUniforms>(), 432);
    assert_eq!(offset_of!(ShadowUniforms, light_view_proj), 0);
    assert_eq!(offset_of!(ShadowUniforms, camera_view_proj), 192);
    assert_eq!(offset_of!(ShadowUniforms, cascade_splits), 256);
    assert_eq!(offset_of!(ShadowUniforms, camera_position), 320);
    assert_eq!(offset_of!(ShadowUniforms, shadow_params), 384);
    assert_eq!(offset_of!(ShadowUniforms, render_params), 400);
    assert_eq!(offset_of!(ShadowUniforms, celestial_params), 416);
}

#[test]
fn vfx1_colored_lights_pack_into_bounded_uniform_layout() {
    let source = vec![
        DynamicColoredLight {
            pos: [4.0, 8.0, 12.0],
            rgb: [0.2, 0.4, 0.9],
            radius: 6.5,
            intensity: 2.25,
            ttl_ticks: 20,
            priority: 1,
        };
        80
    ];
    assert_eq!(align_of::<GpuPointLight>(), 16);
    assert_eq!(size_of::<GpuPointLight>(), 32);
    assert_eq!(size_of::<VfxLightUniforms>(), 16 + 64 * 32);
    assert_eq!(
        pack_vfx1_lights(&source, BuiltinShadowQuality::Off).light_count[0],
        0
    );
    for (quality, expected) in [
        (BuiltinShadowQuality::Feather, 16),
        (BuiltinShadowQuality::Low, 8),
        (BuiltinShadowQuality::Natural, 32),
        (BuiltinShadowQuality::Medium, 32),
        (BuiltinShadowQuality::High, 64),
    ] {
        let packed = pack_vfx1_lights(&source, quality);
        assert_eq!(packed.light_count[0], expected);
        assert_eq!(packed.lights[0].position_radius, [4.0, 8.0, 12.0, 6.5]);
        assert_eq!(packed.lights[0].color_intensity, [0.2, 0.4, 0.9, 2.25]);
    }
    let mut invalid = source[0];
    invalid.radius = f32::NAN;
    assert_eq!(
        pack_vfx1_lights(&[invalid, source[0]], BuiltinShadowQuality::Low).light_count[0],
        1
    );
}

#[test]
fn extreme_and_invalid_camera_inputs_do_not_produce_nan() {
    let camera = ShadowCamera {
        forward: [0.0; 3],
        up: [0.0; 3],
        fov_y_radians: f32::NAN,
        aspect_ratio: f32::INFINITY,
        position: [f32::NAN, 0.0, 0.0],
        near: -1.0,
        far: f32::NAN,
    };
    let result = build_shadow_uniforms(camera, BuiltinShadowQuality::High.config(), u64::MAX);
    assert!(result
        .light_view_proj
        .iter()
        .flatten()
        .flatten()
        .all(|v| v.is_finite()));
    assert!(result
        .camera_view_proj
        .iter()
        .flatten()
        .all(|v| v.is_finite()));
}

fn assert_balanced_wgsl(src: &str) {
    let mut brackets = Vec::new();
    for ch in src.chars() {
        match ch {
            '{' | '(' | '[' => brackets.push(ch),
            '}' => assert_eq!(brackets.pop(), Some('{')),
            ')' => assert_eq!(brackets.pop(), Some('(')),
            ']' => assert_eq!(brackets.pop(), Some('[')),
            _ => {}
        }
    }
    assert!(brackets.is_empty(), "unclosed WGSL delimiter");
}

#[test]
fn all_builtin_shaders_have_valid_entrypoint_and_uniform_structure() {
    let expected_fields = [
        "light_view_proj:",
        "camera_view_proj:",
        "cascade_splits:",
        "sun_direction:",
        "sun_color:",
        "moon_color:",
        "camera_position:",
        "camera_forward:",
        "camera_right:",
        "camera_up:",
        "shadow_params:",
        "render_params:",
        "celestial_params:",
    ];
    for (path, src) in BUILTIN_SHADOW_SHADERS {
        assert!(path.ends_with(".wgsl"));
        assert_balanced_wgsl(src);
        assert!(
            src.contains("@vertex") && src.contains("fn vs_main("),
            "{path}"
        );
        assert!(
            src.contains("@fragment") && src.contains("fn fs_main("),
            "{path}"
        );
        if src.contains("struct ShadowUniforms {") {
            // The four scene passes share the ShadowUniforms layout.
            assert!(
                src.contains("@group(0) @binding(0) var<uniform> shadow: ShadowUniforms;"),
                "{path}"
            );
            let fields: Vec<&str> = src
                .split("struct ShadowUniforms {")
                .nth(1)
                .unwrap()
                .split("};")
                .next()
                .unwrap()
                .lines()
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .collect();
            assert_eq!(fields.len(), expected_fields.len(), "{path}");
            for (line, field) in fields.iter().zip(expected_fields) {
                assert!(line.starts_with(field), "{path} field {field} differs");
            }
        } else {
            // The post pass owns its own uniform block instead.
            assert_eq!(path, "spatial_sharpen.wgsl");
            assert!(src.contains("struct SharpenUniforms {"), "{path}");
            assert!(src.contains("texture_2d<f32>"), "{path}");
        }
        let lower = src.to_ascii_lowercase();
        for forbidden in [
            "optifine",
            "iris",
            "complementary",
            "bsl",
            "seus",
            "sildurs",
        ] {
            assert!(
                !lower.contains(forbidden),
                "{path} unexpected third-party marker"
            );
        }
    }
    assert_eq!(BUILTIN_SHADOW_SHADERS.len(), 5);
    assert!(MEGU_TERRAIN_LIT_WGSL.contains("textureSampleCompareLevel"));
    assert!(MEGU_TERRAIN_LIT_WGSL.contains("array<PointLight, 64>"));
    assert!(MEGU_SKY_ATMOSPHERE_WGSL.contains("rayleigh"));
    assert!(MEGU_SKY_ATMOSPHERE_WGSL.contains("mie"));
    assert!(MEGU_WATER_SURFACE_WGSL.contains("fresnel"));
    assert!(MEGU_SPATIAL_SHARPEN_WGSL.contains("textureSampleLevel"));
}

#[test]
fn native_toml_loader_accepts_every_quality_manifest() {
    for quality in [
        BuiltinShadowQuality::Off,
        BuiltinShadowQuality::Feather,
        BuiltinShadowQuality::Low,
        BuiltinShadowQuality::Natural,
        BuiltinShadowQuality::Medium,
        BuiltinShadowQuality::High,
    ] {
        let manifest = builtin_megu_shader_toml(quality);
        let mut files = vec![(MEGU_WGSL_MANIFEST, manifest.as_str())];
        files.extend(BUILTIN_SHADOW_SHADERS);
        let source = ShaderPackSource::from_memory(files).unwrap();
        let pack = load_megu_wgsl_pack(&source, &GpuShaderCaps::legacy_opengl()).unwrap();
        assert_eq!(pack.name, "Megu Enhanced");
        assert_eq!(pack.api_version, 1);
        assert_eq!(pack.passes.len(), 5);
        assert!(pack.is_compatible());
        assert!(!pack.requires_compute);
        assert!(!pack.requires_ssbo);
        assert!(pack.passes.iter().all(|pass| !pass.wgsl_source.is_empty()));
        assert_eq!(
            pack.passes
                .iter()
                .find(|pass| pass.name == "sky_atmosphere")
                .unwrap()
                .stage,
            PipelineStage::Begin
        );
        assert!(manifest.contains(&format!("quality = \"{}\"", quality.as_str())));
    }
}

#[test]
fn dirty_only_cache_skips_clean_frames_and_catches_every_dirty_kind() {
    let mut cache = DirtyOnlyShadowCache::default();
    assert_eq!(
        cache.angle_threshold_deg,
        DirtyOnlyShadowCache::DEFAULT_ANGLE_THRESHOLD_DEG
    );
    let sun = compute_sun_direction(6000);
    let chunk = [0, 4, 0];
    let quality = BuiltinShadowQuality::Natural;

    // First frame must render; afterwards a quiet frame reuses the cache.
    assert_eq!(
        cache.should_rerender_shadow_map(sun, chunk, 7, quality),
        ShadowCacheDecision::Rerender(ShadowDirtyReason::FirstFrame)
    );
    assert_eq!(
        cache.should_rerender_shadow_map(sun, chunk, 7, quality),
        ShadowCacheDecision::ReuseCached
    );

    // A block edit inside the shadow radius forces a rerender.
    assert_eq!(
        cache.should_rerender_shadow_map(sun, chunk, 8, quality),
        ShadowCacheDecision::Rerender(ShadowDirtyReason::WorldEdited)
    );
    // A preset change forces a rerender.
    assert_eq!(
        cache.should_rerender_shadow_map(sun, chunk, 8, BuiltinShadowQuality::High),
        ShadowCacheDecision::Rerender(ShadowDirtyReason::PresetChanged)
    );
    // Crossing a snap boundary forces a rerender.
    assert_eq!(
        cache.should_rerender_shadow_map(sun, [1, 4, 0], 8, BuiltinShadowQuality::High),
        ShadowCacheDecision::Rerender(ShadowDirtyReason::CameraCrossedSnapRegion)
    );

    // Small sun drift stays cached; cumulative drift past 0.75 deg rerenders.
    // 24000 ticks = 360 deg, so 50 ticks = 0.75 deg.
    assert_eq!(
        cache.should_rerender_shadow_map(
            compute_sun_direction(6000 + 49),
            [1, 4, 0],
            8,
            BuiltinShadowQuality::High,
        ),
        ShadowCacheDecision::ReuseCached
    );
    assert_eq!(
        cache.should_rerender_shadow_map(
            compute_sun_direction(6000 + 49 + 60),
            [1, 4, 0],
            8,
            BuiltinShadowQuality::High,
        ),
        ShadowCacheDecision::Rerender(ShadowDirtyReason::SunMoved)
    );

    // Feather and Off own no shadow map: they always reuse once primed.
    assert_eq!(
        cache.should_rerender_shadow_map(sun, [1, 4, 0], 8, BuiltinShadowQuality::Feather),
        ShadowCacheDecision::Rerender(ShadowDirtyReason::PresetChanged)
    );
    assert_eq!(
        cache.should_rerender_shadow_map(
            compute_sun_direction(18_000),
            [9, 9, 9],
            8,
            BuiltinShadowQuality::Feather,
        ),
        ShadowCacheDecision::ReuseCached
    );

    // invalidate() resets to the first-frame path.
    cache.invalidate();
    assert_eq!(
        cache.should_rerender_shadow_map(sun, chunk, 8, quality),
        ShadowCacheDecision::Rerender(ShadowDirtyReason::FirstFrame)
    );

    // A non-finite sun vector fails safe by rerendering.
    assert_eq!(
        cache.should_rerender_shadow_map(sun, chunk, 8, quality),
        ShadowCacheDecision::ReuseCached
    );
    assert_eq!(
        cache.should_rerender_shadow_map([f32::NAN; 3], chunk, 8, quality),
        ShadowCacheDecision::Rerender(ShadowDirtyReason::SunMoved)
    );
}

#[test]
fn render_scale_presets_are_fixed_and_deterministic() {
    assert_eq!(RenderScalePreset::default(), RenderScalePreset::Native100);
    for (preset, percent) in [
        (RenderScalePreset::Native100, 100),
        (RenderScalePreset::Quality85, 85),
        (RenderScalePreset::Balanced75, 75),
        (RenderScalePreset::Performance67, 67),
        (RenderScalePreset::UltraPerf50, 50),
    ] {
        assert_eq!(preset.percent(), percent);
    }
    assert_eq!(
        RenderScalePreset::Balanced75.scaled_dimensions(1920, 1080),
        (1440, 810)
    );
    assert_eq!(
        RenderScalePreset::UltraPerf50.scaled_dimensions(1920, 1080),
        (960, 540)
    );
    assert_eq!(
        RenderScalePreset::Native100.scaled_dimensions(640, 360),
        (640, 360)
    );
    // Never upscales, never zero, deterministic across calls.
    for preset in [
        RenderScalePreset::Native100,
        RenderScalePreset::Quality85,
        RenderScalePreset::Balanced75,
        RenderScalePreset::Performance67,
        RenderScalePreset::UltraPerf50,
    ] {
        let (w, h) = preset.scaled_dimensions(1, 1);
        assert_eq!((w, h), (1, 1));
        let (w, h) = preset.scaled_dimensions(u32::MAX, 3);
        assert!(w > 0 && h >= 1);
        assert_eq!(
            preset.scaled_dimensions(1920, 1080),
            preset.scaled_dimensions(1920, 1080)
        );
    }
}

#[test]
fn eight_phase_moonlight_and_opt_in_eclipses() {
    // Vanilla orbit: no eclipse is the default.
    assert_eq!(EclipseMode::default(), EclipseMode::None);
    // Full moon at midnight: gentle silver moonlight + soft lunar shadow.
    let full = compute_celestial_lighting(18_000, 0, EclipseMode::None);
    assert!(full.moon_illuminance > 0.9);
    assert!(full.lunar_shadow_strength > 0.4);
    assert!(full.star_visibility > 0.9, "stars out at midnight");
    // New moon: near-zero lunar contribution, darker night.
    let new = compute_celestial_lighting(18_000, 4, EclipseMode::None);
    assert!(new.moon_illuminance < 0.1);
    assert!(new.lunar_shadow_strength < 0.05);
    assert!(new.moon_illuminance < full.moon_illuminance);
    // Daytime: no stars.
    let noon = compute_celestial_lighting(6000, 0, EclipseMode::None);
    assert_eq!(noon.star_visibility, 0.0);
    assert_eq!(noon.sun_illuminance, 1.0);

    // Solar eclipse dims daylight, softens shadows, boosts corona, and lets
    // stars show even at noon.
    let totality = compute_celestial_lighting(6000, 0, EclipseMode::SolarEclipse { coverage: 1.0 });
    assert!(totality.sun_illuminance < 0.1);
    assert_eq!(totality.shadow_softness, 1.0);
    assert_eq!(totality.corona_boost, 1.0);
    assert!(totality.star_visibility > 0.8);
    assert!(totality.eclipse_dim > 0.5);
    let partial = compute_celestial_lighting(6000, 0, EclipseMode::SolarEclipse { coverage: 0.4 });
    assert!(partial.sun_illuminance > totality.sun_illuminance);
    assert!(partial.corona_boost < totality.corona_boost);

    // Total lunar eclipse shifts moonlight toward copper and dims it.
    let blood =
        compute_celestial_lighting(18_000, 0, EclipseMode::TotalLunarEclipse { progress: 1.0 });
    assert!(blood.moon_color[0] > full.moon_color[0]);
    assert!(blood.moon_color[2] < full.moon_color[2]);
    assert!(blood.moon_illuminance < full.moon_illuminance);
    let penumbral =
        compute_celestial_lighting(18_000, 0, EclipseMode::TotalLunarEclipse { progress: 0.0 });
    assert_eq!(penumbral.moon_color, full.moon_color);

    // Every combination stays finite and inside the 0..=1 visual range.
    for time in [0, 3000, 6000, 12_000, 18_000, 23_999, u64::MAX] {
        for phase in [0u8, 1, 4, 7, 8, 200, u8::MAX] {
            for eclipse in [
                EclipseMode::None,
                EclipseMode::SolarEclipse { coverage: 0.5 },
                EclipseMode::SolarEclipse { coverage: f32::NAN },
                EclipseMode::TotalLunarEclipse { progress: 0.75 },
                EclipseMode::TotalLunarEclipse {
                    progress: f32::INFINITY,
                },
            ] {
                let s = compute_celestial_lighting(time, phase, eclipse);
                assert!(s.sun_illuminance.is_finite());
                assert!(s.moon_color.iter().all(|v| v.is_finite()));
                assert!(s.moon_illuminance.is_finite());
                assert!(s.lunar_shadow_strength.is_finite());
                assert!(s.shadow_softness.is_finite());
                assert!(s.corona_boost.is_finite());
                assert!((0.0..=1.0).contains(&s.star_visibility));
                assert!(s.eclipse_dim.is_finite());
            }
        }
    }
}

#[test]
fn celestial_state_reaches_shadow_uniforms() {
    let config = BuiltinShadowQuality::Natural.config();
    let camera = ShadowCamera::default();
    // Vanilla path derives the moon phase from the world day (day 4 = new moon).
    let vanilla = build_shadow_uniforms(camera, config, 4 * 24_000 + 18_000);
    assert!(
        vanilla.celestial_params[0] > 0.9,
        "star visibility at midnight"
    );
    // Opt-in solar eclipse: coverage drives corona boost and eclipse dim.
    let eclipsed = build_shadow_uniforms_celestial(
        camera,
        config,
        6000,
        0,
        EclipseMode::SolarEclipse { coverage: 1.0 },
    );
    assert_eq!(eclipsed.celestial_params[1], 1.0, "corona boost");
    assert!(eclipsed.celestial_params[3] > 0.5, "eclipse dim");
    let plain = build_shadow_uniforms(camera, config, 6000);
    assert!(eclipsed.sun_color[0] < plain.sun_color[0] * 0.2);
    // Lunar eclipse tints the moon channel copper.
    let blood = build_shadow_uniforms_celestial(
        camera,
        config,
        18_000,
        0,
        EclipseMode::TotalLunarEclipse { progress: 1.0 },
    );
    assert!(blood.moon_color[0] > vanilla.moon_color[0]);
    assert!(blood.moon_color[2] < vanilla.moon_color[2]);
    for field in [
        vanilla.celestial_params,
        eclipsed.celestial_params,
        blood.celestial_params,
    ] {
        assert!(field.iter().all(|v| v.is_finite()));
    }
}

#[test]
fn twilight_sunlight_favors_muted_vermilion_over_blue() {
    let camera = ShadowCamera::default();
    let config = BuiltinShadowQuality::Natural.config();
    let sunrise = build_shadow_uniforms(camera, config, 0);
    let sunset = build_shadow_uniforms(camera, config, 12_000);
    let noon = build_shadow_uniforms(camera, config, 6_000);

    for dusk in [sunrise, sunset] {
        assert!(dusk.sun_color[0] > dusk.sun_color[1]);
        assert!(dusk.sun_color[1] > dusk.sun_color[2]);
        assert!(dusk.sun_color[0] / dusk.sun_color[2] > 2.0);
        assert!(dusk.sun_color.iter().all(|component| component.is_finite()));
    }
    assert!(noon.sun_color[0] / noon.sun_color[2] < 1.2);
}

#[test]
fn spatial_sharpen_shader_is_compact_fullscreen_post() {
    let lines = MEGU_SPATIAL_SHARPEN_WGSL.lines().count();
    assert!(
        (55..=110).contains(&lines),
        "sharpen pass should stay compact (~60-90 lines), got {lines}"
    );
    assert!(MEGU_SPATIAL_SHARPEN_WGSL.contains("@builtin(vertex_index)"));
    assert!(MEGU_SPATIAL_SHARPEN_WGSL.contains("SharpenUniforms"));
}
