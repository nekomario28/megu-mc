// SPDX-License-Identifier: LGPL-3.0-or-later

//! Lane QUAL-INEI-PORT: the "Inei-Raisan" lighting audit
//! (`audit/inei-raisan-visuals-and-faithful-spatial-audio-audit-20261123.md`
//! §1, `[QUAL-1]` A1-A4) — physical `sky_light`/`block_light` cave
//! darkness, real `-sun` moonlight cascades with 8-phase lunar contrast,
//! cutout/stained-glass shadow casters, and authentic geometry komorebi.

use megu_client_mesh::builtin_shadow_wgsl::*;
use megu_client_mesh::vanilla_plus_nature::MEGU_WIND_SWAY_WGSL_FN;

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

/// Light-projection depth of a world point, mirroring the comparison the
/// shadow sampler performs (`clip.z` from the cascade matrix column).
fn light_depth(uniforms: &ShadowUniforms, cascade: usize, p: [f32; 3]) -> f32 {
    let m = uniforms.light_view_proj[cascade];
    m[0][2] * p[0] + m[1][2] * p[1] + m[2][2] * p[2] + m[3][2]
}

#[test]
fn qual_a1_skylight_zero_cave_blocks_direct_sun_and_scales_entrance_bounce() {
    // --- CPU mirror: sealed caves get exactly zero direct gate and the
    // deep-cave ambient floor sits at the audited near-black level. ---
    let sealed = compute_inei_sky_block_light(0.0, 0.0, 0.8, 0.0);
    assert_eq!(sealed.direct_gate, 0.0, "sealed cave must gate direct light");
    assert_eq!(sealed.sky_factor, 0.0);
    assert_eq!(INEI_DEEP_CAVE_AMBIENT, [0.008, 0.009, 0.014]);

    // The strict gate is the Vanilla LightTexture `l / (4 - 3l)` ramp, so a
    // roofed cell leaks exactly what a Vanilla light-map texel leaks: the
    // retired `smoothstep(0, 2, sky_light)` gate gave `sky_light == 1` half of
    // full sunlight through solid rock.
    assert_eq!(compute_direct_light_gate(0.0), 0.0);
    assert_eq!(compute_direct_light_gate(15.0), 1.0);
    assert!((compute_direct_light_gate(1.0) - 1.0 / 57.0).abs() < 1.0e-5);
    assert!(compute_direct_light_gate(1.0) > 0.0);
    assert!(compute_direct_light_gate(1.0) < 0.02);
    assert!(compute_direct_light_gate(2.0) < 0.05);
    assert_eq!(
        compute_direct_light_gate(2.0),
        vanilla_sky_light_response(2.0)
    );
    // Outside the cascades only a column that really sees the sky is lit.
    assert_eq!(compute_sky_openness(0.0), 0.0);
    assert_eq!(compute_sky_openness(12.0), 0.0);
    assert_eq!(compute_sky_openness(15.0), 1.0);
    assert!(compute_sky_openness(14.0) < 1.0);

    // --- The entrance gradient 15 -> 8 -> 4 -> 1 -> 0 falls off
    // monotonically under the non-linear sky curve. ---
    let levels = [15.0f32, 8.0, 4.0, 1.0, 0.0];
    let factors: Vec<f32> = levels.iter().map(|&l| compute_sky_light_factor(l)).collect();
    for pair in factors.windows(2) {
        assert!(pair[0] > pair[1], "sky factor must drop: {factors:?}");
    }
    assert_eq!(factors[0], 1.0);
    // The non-linear curve is steeper than linear at depth: sky 4 is far
    // darker than 4/15 of full sky.
    assert!(factors[2] < 4.0 / 15.0);

    // Cave-entrance bounce follows the same gradient and shifts tint with
    // the time of day: warm/sky scatter by day, silver-blue under the moon.
    let day_bounce = compute_cave_bounce_color(0.8, 0.0);
    let night_bounce = compute_cave_bounce_color(-0.8, 1.0);
    assert!(day_bounce.iter().all(|v| (0.0..=1.0).contains(v)));
    assert!(night_bounce[2] > night_bounce[0], "moonlit bounce is silver-blue");
    assert!(day_bounce.iter().zip(night_bounce.iter()).all(|(d, n)| d > n),
        "day bounce must exceed night bounce: {day_bounce:?} vs {night_bounce:?}");

    // --- Warm blackbody block-light temperature: incandescent gold near a
    // torch, deep amber-orange farther out. ---
    let hot = compute_block_light_color_temp(15.0);
    let warm = compute_block_light_color_temp(8.0);
    let far = compute_block_light_color_temp(3.0);
    for c in [hot, warm, far] {
        assert!(c.iter().all(|v| v.is_finite() && (0.0..=1.0).contains(v)));
        assert!(c[0] >= c[1] && c[1] >= c[2], "blackbody order r>=g>=b: {c:?}");
    }
    assert!(hot[1] > 0.8, "torch core is incandescent gold: {hot:?}");
    assert!(far[1] < 0.55, "distant ember is deep amber: {far:?}");
    assert!(hot[2] > far[2], "blue channel cools with distance");
    // Quadratic falloff keeps block light alive in deep caves but bounded.
    let torch = compute_inei_sky_block_light(0.0, 14.0, 0.8, 0.0);
    assert!(torch.block_rgb[0] > 0.5, "torch burns through cave dark");
    assert!(torch.block_rgb[0] > torch.block_rgb[2]);

    // --- WGSL presence: the vertex attribute word is unpacked exactly like
    // PackedVertex.attributes, the gate is strict, and the floor is the
    // audited deep-cave constant. ---
    let terrain = MEGU_TERRAIN_LIT_WGSL;
    for needle in [
        "@location(3) light_attr: u32",
        "(input.light_attr >> 9u) & 0xfu", // sky_light bits 9..12
        "(input.light_attr >> 5u) & 0xfu", // block_light bits 5..8
        "sky_block_light",
        "fn sky_light_factor(",
        "exp2(1.8 * log2(",               // pow(n, 1.8), no pow() call
        "let direct_gate = sky_light_curve(sky_light)",
        "fn sky_light_curve(",
        "fn sky_openness(",
        "fn caster_shadow_weight(",
        "visibility = sample_cascade(input.world_position, normal, layer, sky_open)",
        "visibility * komorebi",
        "* direct_gate",
        "fn color_temp(",
        "fn cave_bounce_color(",
        "vec3<f32>(0.008, 0.009, 0.014)",
        "let block_glow = color_temp(block_light)",
    ] {
        assert!(terrain.contains(needle), "terrain WGSL missing {needle}");
    }
    assert_eq!(terrain.matches("@binding(").count(), 4,
        "A1 must not add bindings");
    assert_balanced_wgsl(terrain);
}

#[test]
fn qual_a2_nighttime_uses_moon_neg_sun_cascades_and_8_phase_contrast() {
    let camera = ShadowCamera::default();
    let config = BuiltinShadowQuality::Natural.config();

    // --- Noon: cascades orient along the true sun. ---
    let noon = build_shadow_uniforms_celestial(
        camera, config, 6000, 0, EclipseMode::None);
    let sun_noon = compute_sun_direction(6000);
    assert!(sun_noon[1] > 0.9);
    assert!(light_depth(&noon, 0, [0.0, 10.0, 0.0])
        < light_depth(&noon, 0, [0.0, -10.0, 0.0]));
    // A tilted daytime tick proves direction, not just "up".
    let afternoon = build_shadow_uniforms_celestial(
        camera, config, 3000, 0, EclipseMode::None);
    let sun_pm = compute_sun_direction(3000);
    assert!(sun_pm[1] > 0.0 && sun_pm[0] > 0.5);
    // Sunward occluders sit in front (smaller clip z).
    let ahead = [sun_pm[0] * 10.0, sun_pm[1] * 10.0, 0.0];
    let behind = [-sun_pm[0] * 10.0, -sun_pm[1] * 10.0, 0.0];
    assert!(light_depth(&afternoon, 0, ahead) < light_depth(&afternoon, 0, [0.0; 3]));
    assert!(light_depth(&afternoon, 0, [0.0; 3]) < light_depth(&afternoon, 0, behind));

    // --- Twilight handover: |sun.y| < 0.03 fades moonlight to zero and
    // never pops — matrices stay finite through the flip. ---
    let dusk = build_shadow_uniforms_celestial(
        camera, config, 12_000, 0, EclipseMode::None);
    let sun_dusk = compute_sun_direction(12_000);
    assert!(sun_dusk[1].abs() < 0.03, "tick 12000 sits at the horizon");
    assert!(dusk.moon_color[3] < 0.05,
        "horizon fade must crush moon strength, got {}", dusk.moon_color[3]);
    for matrix in &dusk.light_view_proj {
        assert!(matrix.iter().flatten().all(|v| v.is_finite()));
    }
    // Just past dusk the moon ramps in smoothly, never at full strength.
    let moonrise = build_shadow_uniforms_celestial(
        camera, config, 12_300, 0, EclipseMode::None);
    assert!(moonrise.moon_color[3] > dusk.moon_color[3]);

    // --- Midnight: cascades are alive and oriented along -sun (the real
    // moon direction), not disabled. ---
    let midnight = build_shadow_uniforms_celestial(
        camera, config, 21_000, 0, EclipseMode::None);
    let sun_late = compute_sun_direction(21_000);
    assert!(sun_late[1] < 0.0, "tick 21000 is night");
    assert!(midnight.shadow_params[1] > 0.0, "night keeps active cascades");
    for matrix in &midnight.light_view_proj {
        assert!(matrix.iter().flatten().all(|v| v.is_finite()));
    }
    // The moon at -sun is the caster: moonward occluders land in front.
    let moon_dir = [-sun_late[0], -sun_late[1], -sun_late[2]];
    let moonward = [moon_dir[0] * 10.0, moon_dir[1] * 10.0, moon_dir[2] * 10.0];
    let sunward = [sun_late[0] * 10.0, sun_late[1] * 10.0, sun_late[2] * 10.0];
    assert!(light_depth(&midnight, 0, moonward)
        < light_depth(&midnight, 0, [0.0; 3]));
    assert!(light_depth(&midnight, 0, [0.0; 3])
        < light_depth(&midnight, 0, sunward));
    // The night matrix is genuinely different from a day matrix at the
    // mirrored sun position: the depth axis flipped to the moon.
    assert!(afternoon.light_view_proj[0][0][2].signum()
        != midnight.light_view_proj[0][0][2].signum(),
        "night depth axis must point at the moon, not the sun");
    // Full-moon midnight carries strong moonlight through the horizon fade.
    assert!(midnight.moon_color[3] > 0.9, "full moon at night: {}",
        midnight.moon_color[3]);

    // --- 8-phase lunar contrast: full moon crisp, new moon near-black. ---
    assert!(moon_phase_intensity(0) > 0.9, "full moon");
    assert!(moon_phase_intensity(4) < 0.1, "new moon");
    assert!(moon_phase_intensity(0) > 10.0 * moon_phase_intensity(4));
    let new_moon = build_shadow_uniforms_celestial(
        camera, config, 21_000, 4, EclipseMode::None);
    assert!(new_moon.moon_color[3] < 0.1,
        "new moon night is near pitch-black starlight");
    for phase in 0..8u8 {
        assert!(moon_phase_intensity(phase).is_finite());
        assert!(moon_phase_intensity(phase + 8) == moon_phase_intensity(phase));
    }
    // The sampling WGSL shares the same caster/horizon semantics.
    let sampling = MEGU_SHADOW_SAMPLING_WGSL;
    for needle in [
        "fn shadow_caster_direction(",
        "return select(-sun, sun, sun.y >= 0.0)",
        "caster_shadow_weight(shadow.sun_direction.y)",
        "mix(1.0, visibility, horizon_fade)",
    ] {
        assert!(sampling.contains(needle), "sampling WGSL missing {needle}");
    }
    assert_balanced_wgsl(sampling);
}

#[test]
fn qual_a3_a4_cutout_wind_and_stained_glass_wgsl_present_and_fake_komorebi_gated() {
    // --- A3a: the cutout alpha-test depth caster exists, discards
    // transparent texels, and embeds the identical wind sway so leaf
    // shadows move with the visible canopy. ---
    let cutout = MEGU_SHADOW_CUTOUT_DEPTH_WGSL;
    for needle in [
        "fn sample_atlas_alpha(",
        "if (atlas_alpha < 0.5) { discard; }",
        "sway_kind",
        "megu_wind_sway(",
        "light_view_proj",
        "texture_2d<f32>",
        "textureSampleLevel",
        "@interpolate(flat) sprite_id",
    ] {
        assert!(cutout.contains(needle), "cutout WGSL missing {needle}");
    }
    // The embedded sway is byte-identical to MEGU_WIND_SWAY_WGSL_FN so the
    // shadow pass can never drift from the main pass motion.
    assert!(cutout.contains(MEGU_WIND_SWAY_WGSL_FN),
        "cutout must embed the identical wind sway function");
    assert_balanced_wgsl(cutout);

    // --- A3b: the stained-glass color-transmission caster writes a
    // multiplicative RGB output and exposes the dye transmission table. ---
    let stained = MEGU_STAINED_GLASS_SHADOW_COLOR_WGSL;
    for needle in [
        "fn stained_glass_transmission_rgb(",
        "-> @location(0) vec4<f32>",
        "array<vec3<f32>, 16>",
        "min(color_idx, 15u)",
    ] {
        assert!(stained.contains(needle), "stained WGSL missing {needle}");
    }
    assert_balanced_wgsl(stained);

    // Both casters are published under stable file names alongside the
    // five core passes (whose table is unchanged at 5 entries).
    assert_eq!(BUILTIN_SHADOW_SHADERS.len(), 5);
    assert_eq!(BUILTIN_SHADOW_CASTER_SHADERS.len(), 2);
    let names: Vec<&str> = BUILTIN_SHADOW_CASTER_SHADERS.iter().map(|(n, _)| *n).collect();
    assert!(names.contains(&"shadow_cutout_depth.wgsl"));
    assert!(names.contains(&"shadow_stained_glass_color.wgsl"));

    // The CPU transmission table matches vanilla dye ordering: white
    // passes almost everything, black passes almost nothing, and the
    // coloured dyes keep their dominant channel.
    let white = stained_glass_transmission_rgb(0);
    assert!(white.iter().all(|v| *v > 0.9));
    let black = stained_glass_transmission_rgb(15);
    assert!(black.iter().all(|v| *v < 0.15));
    let red = stained_glass_transmission_rgb(14);
    assert!(red[0] > 0.8 && red[1] < 0.3 && red[2] < 0.3);
    // Out-of-range indices clamp instead of guessing.
    assert_eq!(stained_glass_transmission_rgb(200),
        stained_glass_transmission_rgb(15));

    // --- A4: the fake unconditional sin() dapple is OFF whenever a real
    // shadow map is active; it survives only as the map-free fallback. ---
    let terrain = MEGU_TERRAIN_LIT_WGSL;
    assert!(!terrain.contains("visibility < 0.95 || cascade_count == 0u"),
        "dapple must no longer run on every penumbra");
    let gate = terrain.find("if (cascade_count == 0u)")
        .expect("komorebi gate");
    let call = terrain.find("komorebi_dapple(input.world_position.xz")
        .expect("dapple call");
    assert!(gate < call, "dapple must sit behind the map-free gate");
    // A live map leaves komorebi at 1.0: real leaf cutout + wind casters
    // produce the authentic dapple instead.
    assert!(terrain.contains("var komorebi = 1.0;"));
}

// ---------------------------------------------------------------------------
// FIX-RICH-SHADOW-LIGHT-ALIGNMENT: true cave darkness, a continuous sun/moon
// handover and Pure-Vanilla neutrality for every new factor.
// ---------------------------------------------------------------------------

#[test]
fn rich_cave_darkness_leaks_no_sunlight_and_no_sky_ambient() {
    // A sealed cave (`sky_light == 0`) multiplies every direct and ambient
    // sun/moon term by exactly zero: no pale-blue sky fill, no sunlit rim
    // past the shadow cascades.
    let sealed = compute_inei_sky_block_light(0.0, 0.0, 0.8, 1.0);
    assert_eq!(sealed.direct_gate, 0.0);
    assert_eq!(sealed.sky_factor, 0.0);
    assert_eq!(compute_sky_openness(sealed.sky_light), 0.0);
    assert_eq!(compute_direct_light_gate(sealed.sky_light), 0.0);

    // The entrance gradient stays monotone and bounded: no level tricks the
    // gate into over-brightening a roofed cell.
    let mut previous_gate = 0.0f32;
    let mut previous_open = 0.0f32;
    for level in 0..=15u8 {
        let level = f32::from(level);
        let gate = compute_direct_light_gate(level);
        let open = compute_sky_openness(level);
        assert!(gate.is_finite() && (0.0..=1.0).contains(&gate));
        assert!(open.is_finite() && (0.0..=1.0).contains(&open));
        assert!(gate >= previous_gate - 1.0e-6, "gate dipped at {level}");
        assert!(open >= previous_open - 1.0e-6, "openness dipped at {level}");
        previous_gate = gate;
        previous_open = open;
        // One block of roof (level 14) must not read as open sky.
        if level < 15.0 {
            assert!(open < 1.0, "level {level} must not read as open sky");
        }
    }
    // A single roofed block (14) still shades a noticeable part of the light
    // in Vanilla (light-map texel `0.777`), and a deep tunnel is dark.
    assert!(compute_direct_light_gate(14.0) < 0.80);
    assert!(compute_direct_light_gate(8.0) < 0.30);
    // Hostile inputs fail closed.
    for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
        assert_eq!(compute_direct_light_gate(bad), 0.0);
        assert_eq!(compute_sky_openness(bad), 0.0);
        assert_eq!(vanilla_sky_light_response(bad), 0.0);
    }
}

#[test]
fn twilight_handover_is_continuous_and_never_ends_in_a_dead_band() {
    // The light-space flip at `sun.y == 0` is exactly where the map weight is
    // zero, and the weight is back to full by 0.12 above the horizon.
    assert_eq!(compute_twilight_handover(0.0).shadow_weight, 0.0);
    assert_eq!(
        compute_twilight_handover(SHADOW_HANDOVER_ELEVATION).shadow_weight,
        1.0
    );
    assert_eq!(
        compute_twilight_handover(-SHADOW_HANDOVER_ELEVATION).shadow_weight,
        1.0
    );
    // Full daylight and full night both saturate: nothing dims mid-day or
    // mid-night.
    assert_eq!(compute_twilight_handover(1.0).shadow_weight, 1.0);
    assert_eq!(compute_twilight_handover(1.0).sun_scale, 1.0);
    assert_eq!(compute_twilight_handover(-1.0).shadow_weight, 1.0);
    assert_eq!(compute_twilight_handover(-1.0).sun_scale, 0.0);

    // Sweep the horizon: both curves are continuous with no step, the sun
    // keeps a warm residual glow at the crossing, and the *effective* direct
    // light (`sun_scale * shadow_weight`) never jumps — that product is what
    // the eye sees, and it is what used to pop when the map switched off.
    let mut previous: Option<(f32, f32, f32)> = None;
    let mut step = -1.0f32;
    while step <= 1.0 {
        let handover = compute_twilight_handover(step);
        assert!(handover.shadow_weight.is_finite());
        assert!(handover.sun_scale.is_finite());
        assert!((0.0..=1.0).contains(&handover.shadow_weight));
        assert!((0.0..=1.0).contains(&handover.sun_scale));
        let effective = handover.sun_scale * handover.shadow_weight;
        if let Some((prev_weight, prev_scale, prev_effective)) = previous {
            assert!(
                (handover.shadow_weight - prev_weight).abs() < 0.02,
                "shadow weight jumped at {step}"
            );
            assert!(
                (handover.sun_scale - prev_scale).abs() < 0.02,
                "sun scale jumped at {step}"
            );
            assert!(
                (effective - prev_effective).abs() < 0.02,
                "effective sunlight jumped at {step}"
            );
        }
        previous = Some((handover.shadow_weight, handover.sun_scale, effective));
        step += 0.001;
    }

    // No dead band: for every elevation where the sun still delivers light,
    // the map is also doing work — the retired `0.02..0.08` band left a sunlit
    // stripe with no shadows at all.
    let mut checked = false;
    for i in 1..=20 {
        let elevation = i as f32 * 0.005;
        let handover = compute_twilight_handover(elevation);
        if handover.sun_scale > 0.01 {
            assert!(
                handover.shadow_weight > 0.0,
                "sun at {elevation} is lit with the map off"
            );
            checked = true;
        }
    }
    assert!(checked, "the sweep must cover a lit twilight elevation");
    // Twilight keeps a warm residual sun rather than a black cut.
    assert!((0.15..0.30).contains(&compute_twilight_handover(0.0).sun_scale));
    // Hostile input fails to the horizon value instead of NaN.
    let bad = compute_twilight_handover(f32::NAN);
    assert!(bad.shadow_weight.is_finite() && bad.sun_scale.is_finite());
}

#[test]
fn rich_factors_are_identity_for_open_noon_sky_so_vanilla_cannot_drift() {
    // Every new factor is exactly 1.0 for the open, noon, sun-overhead case a
    // Pure Vanilla frame renders: the rich path reproduces its previous output
    // there, and the Vanilla preset renders none of this at all.
    assert_eq!(compute_direct_light_gate(15.0), 1.0);
    assert_eq!(compute_sky_light_factor(15.0), 1.0);
    assert_eq!(compute_sky_openness(15.0), 1.0);
    assert_eq!(compute_twilight_handover(1.0).shadow_weight, 1.0);
    assert_eq!(compute_twilight_handover(1.0).sun_scale, 1.0);

    // Pure Vanilla keeps its own preset: no cascades, no map, no PCF taps and
    // no dynamic lights, so none of the rich math above is ever sampled.
    let off = BuiltinShadowQuality::Off.config();
    assert_eq!(off.cascade_count, 0);
    assert_eq!(off.map_resolution, 0);
    assert_eq!(off.pcf_taps, 0);
    assert_eq!(off.max_dynamic_lights, 0);
    assert_eq!(off.depth_array_bytes(), 0);
    assert!(!off.camera_post_fx);

    // The mist density is derived from the Vanilla fog end instead of a fixed
    // magic constant, so the rich haze saturates where `setupFog` does.
    let camera = ShadowCamera::default();
    let uniforms = build_shadow_uniforms(camera, BuiltinShadowQuality::Natural.config(), 6000);
    let render_distance = camera.far * 0.25;
    let (_, fog_end) = vanilla_air_fog_range(render_distance);
    assert!((fog_end - render_distance).abs() < 1.0e-4);
    let mist = uniforms.render_params[2];
    assert!((mist - rich_mist_density((0.0, fog_end))).abs() < 1.0e-6);
    // The optical depth is the same at every view distance: the haze reaches
    // its `0.92` ceiling exactly at the Vanilla fog end, no sooner.
    assert!(mist * fog_end > 2.5 && mist * fog_end < 2.6);
    assert!(mist > 0.0);
    // A 12-chunk view (`vanilla_far_plane(12) == 768`) lands where the retired
    // fixed `0.012` did; a 32-chunk view must thin out instead of over-fogging.
    let near_view = build_shadow_uniforms(
        ShadowCamera { far: 768.0, ..camera },
        BuiltinShadowQuality::Natural.config(),
        6000,
    );
    assert!((near_view.render_params[2] - 0.012).abs() < 0.002);
    let far_view = build_shadow_uniforms(
        ShadowCamera { far: 2048.0, ..camera },
        BuiltinShadowQuality::Natural.config(),
        6000,
    );
    assert!(far_view.render_params[2] < near_view.render_params[2] * 0.5);
    // A degenerate range contributes no fog rather than dividing by zero.
    assert_eq!(rich_mist_density((0.0, 0.0)), 0.0);
    assert_eq!(rich_mist_density((f32::NAN, f32::NAN)), 0.0);
    // The Vanilla range formula matches `FogRenderer.setupFog` at both ends.
    assert_eq!(vanilla_air_fog_range(128.0), (115.2, 128.0));
    assert_eq!(vanilla_air_fog_range(32.0), (28.0, 32.0));
}
