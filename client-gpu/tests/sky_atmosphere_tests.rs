// SPDX-License-Identifier: LGPL-3.0-or-later

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
        "return vec4<f32>(max(color, vec3<f32>(0.0)), 1.0);",
    ] {
        assert!(sky.contains(term), "missing sky term {term}");
    }
    assert_eq!(sky.matches("fract(sin(dot(cell,").count(), 1);
    assert!(!sky.contains("cloud_presence"));
    assert!(!sky.contains("31313.7"));
}
