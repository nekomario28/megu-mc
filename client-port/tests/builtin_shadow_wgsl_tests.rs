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
fn presets_have_expected_budgets() {
    for (quality, cascades, pixels, taps, lights) in [
        (BuiltinShadowQuality::Low, 1, 1024, 4, 8),
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
fn low_spec_never_receives_expensive_quality() {
    use BuiltinShadowQuality::{High, Low, Medium, Off};
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
    assert_eq!(size_of::<ShadowUniforms>(), 416);
    assert_eq!(offset_of!(ShadowUniforms, light_view_proj), 0);
    assert_eq!(offset_of!(ShadowUniforms, camera_view_proj), 192);
    assert_eq!(offset_of!(ShadowUniforms, cascade_splits), 256);
    assert_eq!(offset_of!(ShadowUniforms, camera_position), 320);
    assert_eq!(offset_of!(ShadowUniforms, shadow_params), 384);
    assert_eq!(offset_of!(ShadowUniforms, render_params), 400);
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
        (BuiltinShadowQuality::Low, 8),
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
        assert!(src.contains("struct ShadowUniforms {"), "{path}");
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
    assert!(MEGU_TERRAIN_LIT_WGSL.contains("textureSampleCompareLevel"));
    assert!(MEGU_TERRAIN_LIT_WGSL.contains("array<PointLight, 64>"));
    assert!(MEGU_SKY_ATMOSPHERE_WGSL.contains("rayleigh"));
    assert!(MEGU_SKY_ATMOSPHERE_WGSL.contains("mie"));
    assert!(MEGU_WATER_SURFACE_WGSL.contains("fresnel"));
}

#[test]
fn native_toml_loader_accepts_every_quality_manifest() {
    for quality in [
        BuiltinShadowQuality::Off,
        BuiltinShadowQuality::Low,
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
        assert_eq!(pack.passes.len(), 4);
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
