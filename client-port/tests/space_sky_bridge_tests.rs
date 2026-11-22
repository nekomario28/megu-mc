// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later

use megu_client_mesh::space_sky_bridge::*;
use megu_client_mesh::vanilla_plus_3d::ExtrudedMesh;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn assert_finite(mesh: &ExtrudedMesh) {
    for (i, q) in mesh.quads.iter().enumerate() {
        for p in &q.positions {
            for c in p {
                assert!(c.is_finite(), "quad {i} has non-finite position {p:?}");
            }
        }
        for c in &q.normal {
            assert!(c.is_finite(), "quad {i} has non-finite normal");
        }
    }
}

fn assert_unit_normals(mesh: &ExtrudedMesh) {
    for (i, q) in mesh.quads.iter().enumerate() {
        let len =
            (q.normal[0] * q.normal[0] + q.normal[1] * q.normal[1] + q.normal[2] * q.normal[2])
                .sqrt();
        assert!(
            (len - 1.0).abs() < 1e-4,
            "quad {i} normal {:?} is not unit (len {len})",
            q.normal
        );
    }
}

fn assert_uvs_in_range(mesh: &ExtrudedMesh) {
    for (i, q) in mesh.quads.iter().enumerate() {
        for uv in &q.uv {
            assert!(
                (-1e-6..=1.0 + 1e-6).contains(&uv[0]) && (-1e-6..=1.0 + 1e-6).contains(&uv[1]),
                "quad {i} uv {uv:?} outside 0..=1"
            );
        }
    }
}

fn assert_unit(v: [f32; 3], what: &str) {
    let len = (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt();
    assert!(
        (len - 1.0).abs() < 1e-4,
        "{what} {v:?} is not unit (len {len})"
    );
}

fn allow() -> ExternalOverrideFlags {
    ExternalOverrideFlags {
        has_custom_sky_shader: false,
        has_custom_sky_mod_pack: false,
        player_allow_auto_adapt: true,
    }
}

fn moon_meta() -> PlanetDimensionMeta {
    load_planet_meta_json(
        r#"{
            "dimension_id": "ad_astra:moon",
            "gravity_scale": 0.165,
            "atmosphere_density": 0.0,
            "has_oxygen": false,
            "has_sky_light": true,
            "is_orbit": false,
            "sky_tint_rgb": [0.55, 0.55, 0.65],
            "star_brightness_boost": 2.5,
            "parent_body": {
                "id": "minecraft:overworld",
                "angular_radius_deg": 24.0,
                "orbit_period_ticks": 48000,
                "orbit_inclination_deg": 70.0,
                "surface_rgb": [0.25, 0.45, 0.9],
                "atmosphere_rim_rgb": [0.5, 0.7, 1.0],
                "is_below_horizon_parent": false
            },
            "sky_bodies": []
        }"#,
    )
    .unwrap()
}

fn mars_meta() -> PlanetDimensionMeta {
    load_planet_meta_json(
        r#"{
            "dimension_id": "ad_astra:mars",
            "gravity_scale": 0.38,
            "atmosphere_density": 0.12,
            "has_oxygen": false,
            "has_sky_light": true,
            "is_orbit": false,
            "sky_tint_rgb": [0.95, 0.65, 0.45],
            "star_brightness_boost": 1.0,
            "sky_bodies": []
        }"#,
    )
    .unwrap()
}

fn orbit_meta() -> PlanetDimensionMeta {
    load_planet_meta_json(
        r#"{
            "dimension_id": "ad_astra:earth_orbit",
            "gravity_scale": 0.0,
            "atmosphere_density": 0.0,
            "has_oxygen": false,
            "has_sky_light": true,
            "is_orbit": true,
            "parent_body": {
                "id": "minecraft:overworld",
                "angular_radius_deg": 30.0,
                "orbit_period_ticks": 96000,
                "surface_rgb": [0.25, 0.45, 0.9],
                "atmosphere_rim_rgb": [0.5, 0.7, 1.0],
                "is_below_horizon_parent": true
            },
            "sky_bodies": []
        }"#,
    )
    .unwrap()
}

fn earthlike_meta() -> PlanetDimensionMeta {
    load_planet_meta_json(
        r#"{
            "dimension_id": "megu:terra_nova",
            "gravity_scale": 0.95,
            "atmosphere_density": 1.0,
            "has_oxygen": true,
            "has_sky_light": true,
            "is_orbit": false,
            "star_brightness_boost": 1.0,
            "sky_bodies": [
                {
                    "id": "megu:ringed_gemini",
                    "angular_radius_deg": 14.0,
                    "orbit_period_ticks": 96000,
                    "orbit_phase_offset": 0.25,
                    "orbit_inclination_deg": 40.0,
                    "surface_rgb": [0.8, 0.7, 0.5],
                    "atmosphere_rim_rgb": [0.9, 0.85, 0.7],
                    "ring": {
                        "inner_scale": 1.4,
                        "outer_scale": 2.2,
                        "tilt_deg": 24.0,
                        "rgb": [0.9, 0.85, 0.7],
                        "alpha": 0.8
                    }
                }
            ]
        }"#,
    )
    .unwrap()
}

// ---------------------------------------------------------------------------
// Rule 1: unknown/fantasy dimensions are never mis-adapted
// ---------------------------------------------------------------------------

#[test]
fn none_metadata_is_exactly_vanilla_safe_default() {
    // Twilight Forest / any unregistered fantasy dimension: no metadata.
    let policy = derive_dimension_policy(None, allow());
    assert_eq!(policy, DimensionAdaptationPolicy::vanilla_safe_default());
    assert!(policy.sky_bodies.is_empty());
    assert!(policy.parent_body.is_none());
    assert!(!policy.render_builtin_planets);
    assert!(policy.enable_voxel_clouds);
    assert_eq!(policy.wind_sway_scale, 1.0);
    assert_eq!(policy.acoustic_air_transmission, 1.0);
    assert!(policy.allow_seasons);
    assert!(!policy.vacuum_suit_muffle);
    assert!(!policy.daytime_stars_visible);
}

#[test]
fn player_opt_out_is_exactly_vanilla_safe_default() {
    let flags = ExternalOverrideFlags {
        player_allow_auto_adapt: false,
        ..allow()
    };
    let policy = derive_dimension_policy(Some(&moon_meta()), flags);
    assert_eq!(policy, DimensionAdaptationPolicy::vanilla_safe_default());
}

// ---------------------------------------------------------------------------
// Rule 2: external sky shaders / sky packs own the sky visuals
// ---------------------------------------------------------------------------

#[test]
fn custom_sky_shader_defers_builtin_planets_but_keeps_physics() {
    let flags = ExternalOverrideFlags {
        has_custom_sky_shader: true,
        ..allow()
    };
    let policy = derive_dimension_policy(Some(&moon_meta()), flags);
    assert!(!policy.render_builtin_planets);
    assert!(policy.sky_bodies.is_empty());
    assert!(policy.parent_body.is_none());
    // Non-visual cues still derive from the verified metadata.
    assert!(policy.vacuum_suit_muffle);
    assert_eq!(policy.wind_sway_scale, 0.0);
    assert_eq!(policy.acoustic_air_transmission, 0.0);
    assert!(!policy.enable_voxel_clouds);
}

#[test]
fn custom_sky_mod_pack_defers_builtin_planets() {
    let flags = ExternalOverrideFlags {
        has_custom_sky_mod_pack: true,
        ..allow()
    };
    let policy = derive_dimension_policy(Some(&earthlike_meta()), flags);
    assert!(!policy.render_builtin_planets);
    assert!(policy.sky_bodies.is_empty());
    assert!(policy.parent_body.is_none());
}

// ---------------------------------------------------------------------------
// Rule 3: per-band physical coherence
// ---------------------------------------------------------------------------

#[test]
fn moon_vacuum_disables_air_features_and_shows_parent() {
    let meta = moon_meta();
    let policy = derive_dimension_policy(Some(&meta), allow());
    assert!(!policy.enable_voxel_clouds);
    assert_eq!(policy.wind_sway_scale, 0.0);
    assert!(!policy.allow_seasons);
    assert_eq!(policy.acoustic_air_transmission, 0.0);
    assert!(policy.vacuum_suit_muffle);
    assert!(policy.daytime_stars_visible);
    assert!(policy.render_builtin_planets);
    // The Overworld hangs in the lunar sky as a visible body.
    let parent = policy.parent_body.as_ref().expect("overworld parent");
    assert_eq!(parent.id, "minecraft:overworld");
    assert!(!parent.is_below_horizon_parent);
    let place = compute_sky_body_placement(parent, 6000, 80.0);
    assert!(place.above_horizon);
    assert_unit(place.direction, "parent direction");
}

#[test]
fn mars_thin_atmosphere_gets_gentle_wind_and_reduced_audio() {
    let meta = mars_meta();
    let policy = derive_dimension_policy(Some(&meta), allow());
    assert!(!policy.enable_voxel_clouds);
    assert!((policy.wind_sway_scale - 0.20).abs() < 1e-4);
    assert!((policy.acoustic_air_transmission - 0.35).abs() < 1e-4);
    assert!(!policy.vacuum_suit_muffle);
    assert!(!policy.daytime_stars_visible);
    assert!(!policy.is_orbit);
    assert_eq!(policy.gravity_scale, 0.38);
    // Dusty sky tint is preserved, not replaced.
    assert_eq!(policy.sky_tint_rgb, [0.95, 0.65, 0.45]);
}

#[test]
fn orbit_renders_parent_below_and_scales_smoothly_with_altitude() {
    let meta = orbit_meta();
    let policy = derive_dimension_policy(Some(&meta), allow());
    assert!(policy.is_orbit);
    assert!(!policy.enable_voxel_clouds);
    assert_eq!(policy.wind_sway_scale, 0.0);
    assert!(!policy.allow_seasons);
    assert!(policy.vacuum_suit_muffle);
    assert!(policy.daytime_stars_visible);

    let parent = policy.parent_body.as_ref().expect("orbit parent");
    assert!(parent.is_below_horizon_parent);

    // Parent hangs below the station.
    let low = compute_sky_body_placement(parent, 12000, 64.0);
    assert!(!low.above_horizon);
    assert!(low.direction[1] < -0.9, "parent direction {low:?}");
    assert_unit(low.direction, "parent direction");

    // Apparent size shrinks smoothly as altitude increases.
    let high = compute_sky_body_placement(parent, 12000, 384.0);
    assert!(low.apparent_angular_radius_deg > high.apparent_angular_radius_deg);
    assert!(high.apparent_angular_radius_deg >= parent.effective_angular_radius_deg());
    // Continuity: midpoint is strictly between, no steps or NaNs.
    let mid = compute_sky_body_placement(parent, 12000, 192.0);
    assert!(mid.apparent_angular_radius_deg < low.apparent_angular_radius_deg);
    assert!(mid.apparent_angular_radius_deg > high.apparent_angular_radius_deg);
}

#[test]
fn earthlike_keeps_clouds_seasons_and_ringed_neighbour() {
    let meta = earthlike_meta();
    let policy = derive_dimension_policy(Some(&meta), allow());
    assert!(policy.enable_voxel_clouds);
    assert!(policy.allow_seasons);
    assert_eq!(policy.wind_sway_scale, 1.0);
    assert_eq!(policy.acoustic_air_transmission, 1.0);
    assert!(!policy.vacuum_suit_muffle);
    assert!(!policy.daytime_stars_visible);
    assert!(policy.render_builtin_planets);
    assert_eq!(policy.sky_bodies.len(), 1);
    let neighbour = &policy.sky_bodies[0];
    assert_eq!(neighbour.id, "megu:ringed_gemini");
    assert!(neighbour.ring.is_some());
}

#[test]
fn breathable_without_sky_light_disables_seasons() {
    let mut meta = earthlike_meta();
    meta.has_sky_light = false;
    let policy = derive_dimension_policy(Some(&meta), allow());
    assert!(policy.enable_voxel_clouds);
    assert!(!policy.allow_seasons);
}

// ---------------------------------------------------------------------------
// Sky-body placement
// ---------------------------------------------------------------------------

#[test]
fn placement_is_finite_unit_and_deterministic() {
    let meta = earthlike_meta();
    let body = &meta.sky_bodies[0];
    for t in [0u64, 1000, 6000, 12000, 23999, 48001, 96000 * 7 + 3] {
        let a = compute_sky_body_placement(body, t, 70.0);
        assert_unit(a.direction, "direction");
        assert_unit(a.sun_direction, "sun direction");
        assert!((0.0..1.0).contains(&a.orbit_fraction));
        assert!(a.apparent_angular_radius_deg > 0.0);
        assert!(a.apparent_angular_radius_deg.is_finite());
        let b = compute_sky_body_placement(body, t, 70.0);
        assert_eq!(a, b, "placement must be deterministic");
    }
}

#[test]
fn zero_period_body_is_fixed_at_phase_offset() {
    let mut body = earthlike_meta().sky_bodies[0].clone();
    body.orbit_period_ticks = 0;
    body.orbit_phase_offset = 0.3;
    let a = compute_sky_body_placement(&body, 0, 64.0);
    let b = compute_sky_body_placement(&body, 999_999, 512.0);
    assert_eq!(a.direction, b.direction);
    assert!((a.orbit_fraction - 0.3).abs() < 1e-6);
}

#[test]
fn non_finite_player_y_falls_back_to_floor() {
    let meta = orbit_meta();
    let parent = meta.parent_body.as_ref().unwrap();
    let nan = compute_sky_body_placement(parent, 0, f32::NAN);
    let floor = compute_sky_body_placement(parent, 0, 0.0);
    assert_eq!(
        nan.apparent_angular_radius_deg,
        floor.apparent_angular_radius_deg
    );
    assert_unit(nan.direction, "direction");
}

// ---------------------------------------------------------------------------
// 3D cubic planet / ring meshes
// ---------------------------------------------------------------------------

#[test]
fn plain_planet_mesh_is_blocky_finite_and_valid() {
    let body = PlanetSkyBody {
        id: "plain".into(),
        angular_radius_deg: 10.0,
        orbit_period_ticks: 48000,
        orbit_phase_offset: 0.0,
        orbit_inclination_deg: 30.0,
        surface_rgb: [0.7, 0.3, 0.2],
        atmosphere_rim_rgb: None,
        ring: None,
        is_below_horizon_parent: false,
    };
    let mesh = build_3d_planet_mesh(&body);
    assert!(mesh.quad_count() > 0);
    assert_finite(&mesh);
    assert_unit_normals(&mesh);
    assert_uvs_in_range(&mesh);
    // All outward-facing quads are axis-aligned: blocky by construction.
    for q in &mesh.quads {
        let n = q.normal;
        let max_axis = n[0].abs().max(n[1].abs()).max(n[2].abs());
        assert!((max_axis - 1.0).abs() < 1e-4, "non-axis normal {n:?}");
    }
}

#[test]
fn ringed_planet_adds_tilted_ring_quads() {
    let mut body = earthlike_meta().sky_bodies[0].clone();
    let ringed = build_3d_planet_mesh(&body);
    assert_finite(&ringed);
    assert_unit_normals(&ringed);
    assert_uvs_in_range(&ringed);
    assert!(ringed
        .quads
        .iter()
        .any(|q| q.tint_index == PLANET_TINT_RING));
    // Tilted ring -> normals leave the axis-aligned set.
    assert!(ringed
        .quads
        .iter()
        .any(|q| q.normal[2].abs() > 0.1 && q.normal[1].abs() > 0.5));

    body.ring.as_mut().unwrap().tilt_deg = 0.0;
    let flat = build_3d_planet_mesh(&body);
    assert_eq!(flat.quad_count(), ringed.quad_count());
    // Untilted ring normals stay axis-aligned.
    for q in &flat.quads {
        let max_axis = q.normal[0]
            .abs()
            .max(q.normal[1].abs())
            .max(q.normal[2].abs());
        assert!((max_axis - 1.0).abs() < 1e-4);
    }

    body.ring = None;
    let plain = build_3d_planet_mesh(&body);
    assert!(ringed.quad_count() > plain.quad_count());
}

#[test]
fn atmosphere_rim_adds_shell_quads() {
    let mut body = earthlike_meta().sky_bodies[0].clone();
    body.ring = None;
    let with_rim = build_3d_planet_mesh(&body);
    body.atmosphere_rim_rgb = None;
    let airless = build_3d_planet_mesh(&body);
    assert_finite(&with_rim);
    assert_unit_normals(&with_rim);
    assert_uvs_in_range(&with_rim);
    assert!(with_rim.quad_count() > airless.quad_count());
    assert!(with_rim
        .quads
        .iter()
        .any(|q| q.tint_index == PLANET_TINT_ATMOSPHERE));
    assert!(!airless
        .quads
        .iter()
        .any(|q| q.tint_index == PLANET_TINT_ATMOSPHERE));
}

// ---------------------------------------------------------------------------
// JSON loading: both schemas, all rejection paths
// ---------------------------------------------------------------------------

#[test]
fn parses_megu_schema() {
    let meta = earthlike_meta();
    assert_eq!(meta.dimension_id, "megu:terra_nova");
    assert_eq!(meta.gravity_scale, 0.95);
    assert_eq!(meta.atmosphere_density, 1.0);
    assert!(meta.has_oxygen);
    assert!(!meta.is_orbit);
    assert_eq!(meta.sky_bodies.len(), 1);
    let ring = meta.sky_bodies[0].ring.as_ref().unwrap();
    assert_eq!(ring.tilt_deg, 24.0);
}

#[test]
fn parses_ad_astra_style_planet() {
    let meta = load_planet_meta_json(
        r#"{
            "dimension": "ad_astra:mars",
            "gravity": 3.7208,
            "oxygen": false,
            "temperature": -63.0,
            "solar_power": 2.0,
            "orbit": "ad_astra:mars_orbit",
            "tier": 2
        }"#,
    )
    .unwrap();
    assert_eq!(meta.dimension_id, "ad_astra:mars");
    // Ad Astra gravity is m/s^2; 3.7208 / 9.81 ~= 0.38.
    assert!((meta.gravity_scale - 0.38).abs() < 0.005);
    assert!(!meta.has_oxygen);
    // Low-gravity body with no oxygen derives a thin atmosphere.
    assert!((meta.atmosphere_density - 0.12).abs() < 1e-6);
    assert!(!meta.is_orbit);
}

#[test]
fn parses_ad_astra_orbit_dimension() {
    let meta = load_planet_meta_json(
        r#"{
            "dimension": "ad_astra:earth_orbit",
            "gravity": 0.0,
            "oxygen": false,
            "temperature": -270.0,
            "tier": 1
        }"#,
    )
    .unwrap();
    assert!(meta.is_orbit);
    assert_eq!(meta.atmosphere_density, 0.0);
    assert_eq!(meta.gravity_scale, 0.0);
}

#[test]
fn ad_astra_moon_derives_vacuum() {
    let meta = load_planet_meta_json(
        r#"{"dimension": "ad_astra:moon", "gravity": 1.62, "oxygen": false}"#,
    )
    .unwrap();
    assert!((meta.gravity_scale - 0.165).abs() < 0.005);
    assert_eq!(meta.atmosphere_density, 0.0);
    let policy = derive_dimension_policy(Some(&meta), allow());
    assert!(policy.vacuum_suit_muffle);
    assert!(!policy.enable_voxel_clouds);
}

#[test]
fn malformed_json_fails_closed() {
    for bad in [
        "",
        "{",
        "{\"dimension_id\":",
        "{\"dimension_id\": \"x\",}",
        "{\"dimension_id\": \"x\"} trailing",
        "[1, 2, 3]",
        "42",
        "\"just a string\"",
        "null",
        "{\"dimension_id\": NaN}",
        "{\"dimension_id\": \"x\", \"gravity_scale\": NaN}",
        "{\"dimension_id\": \"x\", \"gravity_scale\": Infinity}",
    ] {
        assert!(
            load_planet_meta_json(bad).is_err(),
            "expected rejection: {bad:?}"
        );
    }
}

#[test]
fn missing_or_wrong_dimension_id_fails_closed() {
    assert!(load_planet_meta_json("{}").is_err());
    assert!(load_planet_meta_json("{\"gravity_scale\": 0.5}").is_err());
    assert!(load_planet_meta_json("{\"dimension_id\": 42}").is_err());
    assert!(load_planet_meta_json("{\"dimension_id\": \"\"}").is_err());
}

#[test]
fn non_finite_number_fails_closed() {
    assert!(load_planet_meta_json("{\"dimension_id\": \"x\", \"gravity_scale\": 1e999}").is_err());
    assert!(
        load_planet_meta_json("{\"dimension_id\": \"x\", \"star_brightness_boost\": -1e999}")
            .is_err()
    );
}

#[test]
fn negative_orbit_period_fails_closed() {
    let json = r#"{
        "dimension_id": "x:y",
        "sky_bodies": [{"id": "b", "orbit_period_ticks": -10}]
    }"#;
    assert!(load_planet_meta_json(json).is_err());
}

#[test]
fn more_than_sixteen_sky_bodies_fails_closed() {
    let mut json = String::from("{\"dimension_id\": \"x:y\", \"sky_bodies\": [");
    for i in 0..17 {
        if i > 0 {
            json.push(',');
        }
        json.push_str(&format!("{{\"id\": \"b{i}\"}}"));
    }
    json.push_str("]}");
    assert!(load_planet_meta_json(&json).is_err());

    // Exactly 16 is accepted.
    let mut ok = String::from("{\"dimension_id\": \"x:y\", \"sky_bodies\": [");
    for i in 0..16 {
        if i > 0 {
            ok.push(',');
        }
        ok.push_str(&format!("{{\"id\": \"b{i}\"}}"));
    }
    ok.push_str("]}");
    assert_eq!(load_planet_meta_json(&ok).unwrap().sky_bodies.len(), 16);
}

#[test]
fn wrong_typed_fields_fail_closed() {
    assert!(
        load_planet_meta_json("{\"dimension_id\": \"x\", \"gravity_scale\": \"heavy\"}").is_err()
    );
    assert!(load_planet_meta_json("{\"dimension_id\": \"x\", \"has_oxygen\": 1}").is_err());
    assert!(load_planet_meta_json("{\"dimension_id\": \"x\", \"sky_bodies\": {}}").is_err());
    assert!(load_planet_meta_json(
        "{\"dimension_id\": \"x\", \"sky_bodies\": [{\"angular_radius_deg\": 4}]}"
    )
    .is_err());
}

#[test]
fn ring_field_validates_geometry() {
    // outer <= inner is malformed, not clamped silently.
    let bad = r#"{
        "dimension_id": "x:y",
        "sky_bodies": [{"id": "b", "ring": {"inner_scale": 3.0, "outer_scale": 1.0}}]
    }"#;
    assert!(load_planet_meta_json(bad).is_err());
}
