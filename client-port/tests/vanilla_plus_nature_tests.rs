// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later

use mc_spine_client_mesh::vanilla_plus_nature::*;
use mc_spine_client_mesh::vfx_engine::VfxHardwareTier;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn enabled_config() -> VanillaPlusNatureConfig {
    VanillaPlusNatureConfig {
        enabled: true,
        ..VanillaPlusNatureConfig::default()
    }
}

fn assert_finite3(v: [f32; 3]) {
    for c in v {
        assert!(c.is_finite(), "non-finite component in {v:?}");
    }
}

fn assert_zero3(v: [f32; 3]) {
    for c in v {
        assert!(c.abs() < 1e-9, "expected zero offset, got {v:?}");
    }
}

fn norm(v: [f32; 3]) -> f32 {
    (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt()
}

fn luminance(rgb: [f32; 3]) -> f32 {
    0.2126 * rgb[0] + 0.7152 * rgb[1] + 0.0722 * rgb[2]
}

/// Warmth metric: positive means red/amber dominates over blue.
fn warmth(rgb: [f32; 3]) -> f32 {
    rgb[0] - rgb[2]
}

fn count_kind(state: &NatureAmbienceState, pred: impl Fn(&NatureParticleKind) -> bool) -> usize {
    state.particles().iter().filter(|p| pred(&p.kind)).count()
}

fn rich_env(weather: WeatherKind, is_night: bool) -> NatureAmbienceEnv {
    let leaf_blocks: Vec<[f32; 3]> = (0..MAX_ENV_SITES)
        .map(|i| [10.0 + i as f32, 70.0, 20.0])
        .collect();
    let rain_exposed_water: Vec<[f32; 3]> = (0..MAX_ENV_SITES)
        .map(|i| [30.0 + i as f32, 62.0, 40.0])
        .collect();
    let waterfall_bases: Vec<([f32; 3], f32)> = (0..MAX_ENV_SITES)
        .map(|i| ([50.0 + i as f32, 63.0, 60.0], 6.0))
        .collect();
    NatureAmbienceEnv {
        weather,
        is_night,
        firefly_biome: true,
        cherry_blossom: false,
        autumn_factor: 0.0,
        origin: [0.0, 65.0, 0.0],
        leaf_blocks,
        rain_exposed_water,
        waterfall_bases,
    }
}

fn run_ticks(
    state: &mut NatureAmbienceState,
    config: &VanillaPlusNatureConfig,
    env: &NatureAmbienceEnv,
    ticks: u32,
) {
    for _ in 0..ticks {
        tick_nature_ambience(state, config, env, 0.05);
    }
}

// ---------------------------------------------------------------------------
// Master opt-in
// ---------------------------------------------------------------------------

#[test]
fn default_config_is_disabled_and_generates_nothing() {
    let config = VanillaPlusNatureConfig::default();
    assert!(!config.enabled);

    let cells = generate_layered_voxel_clouds(&config, 0.0, 0.0, 12_000);
    assert!(cells.is_empty(), "disabled pack must emit no cloud cells");

    let mut state = NatureAmbienceState::default();
    let env = rich_env(WeatherKind::Thunder, true);
    run_ticks(&mut state, &config, &env, 500);
    assert_eq!(state.len(), 0, "disabled pack must emit no ambience");
}

#[test]
fn per_feature_toggles_gate_output() {
    let config = VanillaPlusNatureConfig {
        enabled: true,
        layered_voxel_clouds: false,
        nature_ambience: false,
        ..VanillaPlusNatureConfig::default()
    };
    assert!(generate_layered_voxel_clouds(&config, 0.0, 0.0, 0).is_empty());

    let mut state = NatureAmbienceState::default();
    let env = rich_env(WeatherKind::Rain, true);
    run_ticks(&mut state, &config, &env, 300);
    assert_eq!(state.len(), 0);
}

// ---------------------------------------------------------------------------
// Layered voxel clouds
// ---------------------------------------------------------------------------

#[test]
fn clouds_are_deterministic() {
    let config = enabled_config();
    let a = generate_layered_voxel_clouds(&config, 123.5, -456.25, 6_000);
    let b = generate_layered_voxel_clouds(&config, 123.5, -456.25, 6_000);
    assert_eq!(a, b);
    assert!(!a.is_empty());
}

#[test]
fn clouds_stack_two_to_three_strata() {
    let config = VanillaPlusNatureConfig {
        cloud_layers: 3,
        ..enabled_config()
    };
    let cells = generate_layered_voxel_clouds(&config, 0.0, 0.0, 6_000);
    assert!(!cells.is_empty());
    assert!(cells.iter().all(|c| c.layer < 3));

    // A stepped cap must sit directly above a base voxel in the same column.
    let stacked = cells.iter().filter(|c| c.layer == 1).any(|cap| {
        cells.iter().any(|base| {
            base.layer == 0
                && (base.min[0] - cap.min[0]).abs() < 1e-3
                && (base.min[2] - cap.min[2]).abs() < 1e-3
                && (cap.min[1] - base.min[1] - CLOUD_LAYER_HEIGHT).abs() < 1e-3
        })
    });
    assert!(stacked, "expected at least one puffy cap stacked on a base");

    // Base wider than or equal to caps: every cap has a base beneath it.
    for cap in cells.iter().filter(|c| c.layer > 0) {
        assert!(cells.iter().any(|base| {
            base.layer == cap.layer - 1
                && (base.min[0] - cap.min[0]).abs() < 1e-3
                && (base.min[2] - cap.min[2]).abs() < 1e-3
        }));
    }
}

#[test]
fn clouds_single_layer_config_emits_only_base() {
    let config = VanillaPlusNatureConfig {
        cloud_layers: 1,
        ..enabled_config()
    };
    let cells = generate_layered_voxel_clouds(&config, 0.0, 0.0, 6_000);
    assert!(cells.iter().all(|c| c.layer == 0));
}

#[test]
fn clouds_clamp_layer_count_to_three() {
    let config = VanillaPlusNatureConfig {
        cloud_layers: 9,
        ..enabled_config()
    };
    assert_eq!(config.effective_cloud_layers(), 3);
    let cells = generate_layered_voxel_clouds(&config, 0.0, 0.0, 6_000);
    assert!(cells.iter().all(|c| c.layer <= 2));
}

#[test]
fn clouds_suppress_hidden_faces() {
    let config = VanillaPlusNatureConfig {
        cloud_layers: 3,
        ..enabled_config()
    };
    let cells = generate_layered_voxel_clouds(&config, 0.0, 0.0, 6_000);
    let same_column = |a: &VoxelCloudCell, b: &VoxelCloudCell| {
        (a.min[0] - b.min[0]).abs() < 1e-3 && (a.min[2] - b.min[2]).abs() < 1e-3
    };
    // Vertically adjacent voxels hide the shared top/bottom faces.
    for lower in &cells {
        if let Some(upper) = cells
            .iter()
            .find(|u| u.layer == lower.layer + 1 && same_column(lower, u))
        {
            assert_eq!(lower.exposed_faces & FACE_POS_Y, 0);
            assert_eq!(upper.exposed_faces & FACE_NEG_Y, 0);
        }
    }
    // Horizontally adjacent voxels hide the shared side face.
    for a in &cells {
        for b in &cells {
            if a.layer != b.layer || (a.min[1] - b.min[1]).abs() > 1e-3 {
                continue;
            }
            if (a.min[2] - b.min[2]).abs() < 1e-3
                && (b.min[0] - a.min[0] - CLOUD_CELL_BLOCKS).abs() < 1e-3
            {
                assert_eq!(a.exposed_faces & FACE_POS_X, 0);
                assert_eq!(b.exposed_faces & FACE_NEG_X, 0);
            }
            if (a.min[0] - b.min[0]).abs() < 1e-3
                && (b.min[2] - a.min[2] - CLOUD_CELL_BLOCKS).abs() < 1e-3
            {
                assert_eq!(a.exposed_faces & FACE_POS_Z, 0);
                assert_eq!(b.exposed_faces & FACE_NEG_Z, 0);
            }
        }
    }
    // Every emitted voxel exposes at least one face.
    assert!(cells.iter().all(|c| c.quad_count() >= 1));
}

#[test]
fn clouds_wind_drift_is_smooth() {
    let config = enabled_config();
    let t0 = 100u64; // scroll 40.0 -> cell origin 3 (no boundary crossing at +1 tick)
    let a = generate_layered_voxel_clouds(&config, 0.0, 0.0, t0);
    let b = generate_layered_voxel_clouds(&config, 0.0, 0.0, t0 + 1);
    assert_eq!(a.len(), b.len(), "no cell-boundary crossing expected");
    // Every cell drifts by exactly one tick of scroll along -X; structure and
    // altitude stay identical, so motion is smooth (far below one cell).
    for (before, after) in a.iter().zip(b.iter()) {
        let dx = before.min[0] - after.min[0];
        assert!(
            (dx - CLOUD_SCROLL_BLOCKS_PER_TICK).abs() < 1e-3,
            "cell drifted {dx} blocks in one tick"
        );
        assert!((before.min[1] - after.min[1]).abs() < 1e-9);
        assert!((before.min[2] - after.min[2]).abs() < 1e-9);
        assert_eq!(before.layer, after.layer);
        assert_eq!(before.exposed_faces, after.exposed_faces);
    }
}

#[test]
fn clouds_stay_finite_at_extreme_inputs() {
    let config = enabled_config();
    for (cx, cz, ticks) in [
        (29_999_999.0, -29_999_999.0, u64::MAX),
        (-29_999_999.0, 29_999_999.0, 1_234_567_890),
        (f32::NAN, 0.0, 42),
        (0.0, f32::INFINITY, 7),
        (0.0, 0.0, 0),
    ] {
        let cells = generate_layered_voxel_clouds(&config, cx, cz, ticks);
        assert!(cells.len() <= MAX_CLOUD_CELLS);
        for c in &cells {
            assert_finite3(c.min);
            assert_finite3(c.size);
            assert!(c.quad_count() <= 6);
        }
    }
}

#[test]
fn cloud_lighting_tracks_time_and_weather() {
    let midday = compute_cloud_lighting(0.9, 0.0, 0.0);
    let sunset = compute_cloud_lighting(0.02, 0.0, 0.0);
    let night = compute_cloud_lighting(-0.8, 0.0, 0.0);
    let rain = compute_cloud_lighting(0.9, 1.0, 0.0);
    let thunder = compute_cloud_lighting(0.9, 0.0, 1.0);

    // Daytime top is crisp white; underside is a dimmer cool tone.
    assert!(luminance(midday.top_rgb) > 0.9);
    assert!(luminance(midday.bottom_rgb) < luminance(midday.top_rgb));

    // Sunrise/sunset top is warmer (more red relative to blue) than midday.
    assert!(sunset.rim_warmth > 0.5);
    assert!(warmth(sunset.top_rgb) > warmth(midday.top_rgb));

    // Rain darkens the underside; thunder darkens it further.
    assert!(luminance(rain.bottom_rgb) < luminance(midday.bottom_rgb));
    assert!(luminance(thunder.bottom_rgb) < luminance(rain.bottom_rgb));

    // Night: deep slate, softly moonlit, much darker than day.
    assert!(luminance(night.top_rgb) < 0.2);
    assert!(night.top_rgb[2] > night.top_rgb[0]); // cool, blue-leaning slate
    assert!(night.rim_warmth < 1e-6);

    // Non-finite input stays finite.
    let nan = compute_cloud_lighting(f32::NAN, f32::NAN, f32::INFINITY);
    assert_finite3(nan.top_rgb);
    assert_finite3(nan.side_rgb);
    assert_finite3(nan.bottom_rgb);
}

// ---------------------------------------------------------------------------
// Nature ambience
// ---------------------------------------------------------------------------

#[test]
fn ambience_caps_are_strict_per_tier() {
    let config = enabled_config();
    for tier in [
        VfxBudgetTier::LowSpec,
        VfxBudgetTier::Balanced,
        VfxBudgetTier::High,
    ] {
        let mut state = NatureAmbienceState::new(tier);
        let env = rich_env(WeatherKind::Thunder, true);
        let mut saw_particles = false;
        for _ in 0..3_000 {
            tick_nature_ambience(&mut state, &config, &env, 0.05);
            assert!(
                state.len() <= tier.max_particles(),
                "{tier:?} exceeded cap {}",
                tier.max_particles()
            );
            saw_particles |= !state.is_empty();
        }
        assert!(saw_particles, "{tier:?} never spawned anything");
    }
    assert_eq!(VfxBudgetTier::LowSpec.max_particles(), 32);
    assert_eq!(VfxBudgetTier::Balanced.max_particles(), 96);
    assert_eq!(VfxBudgetTier::High.max_particles(), 192);
    // Interop with the vfx engine's hardware tier.
    assert_eq!(
        VfxBudgetTier::from(VfxHardwareTier::LowSpec),
        VfxBudgetTier::LowSpec
    );
    assert_eq!(
        VfxBudgetTier::from(VfxHardwareTier::High),
        VfxBudgetTier::High
    );
}

#[test]
fn fireflies_only_spawn_at_night_in_warm_biomes() {
    let config = enabled_config();
    let is_firefly = |k: &NatureParticleKind| matches!(k, NatureParticleKind::Firefly { .. });

    // Daytime: zero fireflies even in an eligible biome.
    let mut state = NatureAmbienceState::default();
    let env = rich_env(WeatherKind::Clear, false);
    run_ticks(&mut state, &config, &env, 200);
    assert_eq!(count_kind(&state, is_firefly), 0);

    // Night but freezing/desert biome: still zero.
    let mut state = NatureAmbienceState::default();
    let mut env = rich_env(WeatherKind::Clear, true);
    env.firefly_biome = false;
    run_ticks(&mut state, &config, &env, 200);
    assert_eq!(count_kind(&state, is_firefly), 0);

    // Night in a warm biome: fireflies appear.
    let mut state = NatureAmbienceState::default();
    let env = rich_env(WeatherKind::Clear, true);
    run_ticks(&mut state, &config, &env, 200);
    assert!(count_kind(&state, is_firefly) > 0);
    for p in state.particles() {
        assert_finite3(p.pos);
        assert_finite3(p.vel);
    }
}

#[test]
fn rain_ripples_only_during_precipitation() {
    let config = enabled_config();
    let is_ripple = |k: &NatureParticleKind| matches!(k, NatureParticleKind::RainRipple { .. });
    let sites = vec![[30.0, 62.0, 40.0]; 4];

    // Clear sky: exposed water still produces zero ripples.
    let mut state = NatureAmbienceState::default();
    let env = NatureAmbienceEnv {
        weather: WeatherKind::Clear,
        rain_exposed_water: sites.clone(),
        ..NatureAmbienceEnv::default()
    };
    run_ticks(&mut state, &config, &env, 200);
    assert_eq!(count_kind(&state, is_ripple), 0);

    // Rain: ripples appear and expand.
    let mut state = NatureAmbienceState::default();
    let env = NatureAmbienceEnv {
        weather: WeatherKind::Rain,
        rain_exposed_water: sites.clone(),
        ..NatureAmbienceEnv::default()
    };
    run_ticks(&mut state, &config, &env, 200);
    assert!(count_kind(&state, is_ripple) > 0);
    assert!(state.particles().iter().any(|p| {
        matches!(p.kind, NatureParticleKind::RainRipple { radius, .. } if radius > 0.08)
    }));

    // Thunder also produces ripples.
    let mut state = NatureAmbienceState::default();
    let env = NatureAmbienceEnv {
        weather: WeatherKind::Thunder,
        rain_exposed_water: sites,
        ..NatureAmbienceEnv::default()
    };
    run_ticks(&mut state, &config, &env, 200);
    assert!(count_kind(&state, is_ripple) > 0);
}

#[test]
fn falling_leaves_respond_to_autumn_and_expire() {
    let config = enabled_config();
    let is_leaf = |k: &NatureParticleKind| matches!(k, NatureParticleKind::FallingLeaf { .. });
    let leaf_blocks = vec![[10.0, 70.0, 20.0], [12.0, 71.0, 22.0]];

    let mut spring = NatureAmbienceState::default();
    let env = NatureAmbienceEnv {
        autumn_factor: 0.0,
        leaf_blocks: leaf_blocks.clone(),
        ..NatureAmbienceEnv::default()
    };
    run_ticks(&mut spring, &config, &env, 120);
    assert!(
        count_kind(&spring, is_leaf) > 0,
        "leaves should fall in spring too"
    );
    let spring_warmth: f32 = spring
        .particles()
        .iter()
        .filter_map(|p| match p.kind {
            NatureParticleKind::FallingLeaf { rgb, .. } => Some(warmth(rgb)),
            _ => None,
        })
        .sum::<f32>()
        / count_kind(&spring, is_leaf).max(1) as f32;

    let mut autumn = NatureAmbienceState::default();
    let env = NatureAmbienceEnv {
        autumn_factor: 1.0,
        leaf_blocks,
        ..NatureAmbienceEnv::default()
    };
    run_ticks(&mut autumn, &config, &env, 120);
    let autumn_leaves: Vec<[f32; 3]> = autumn
        .particles()
        .iter()
        .filter_map(|p| match p.kind {
            NatureParticleKind::FallingLeaf { rgb, .. } => Some(rgb),
            _ => None,
        })
        .collect();
    assert!(!autumn_leaves.is_empty());
    for rgb in &autumn_leaves {
        assert!(rgb[0] > rgb[1], "autumn leaf {rgb:?} should be warm amber");
    }
    let autumn_warmth: f32 =
        autumn_leaves.iter().map(|rgb| warmth(*rgb)).sum::<f32>() / autumn_leaves.len() as f32;
    assert!(
        autumn_warmth > spring_warmth,
        "autumn ({autumn_warmth}) should be warmer than spring ({spring_warmth})"
    );

    // Cherry groves shed pink petals instead.
    let mut cherry = NatureAmbienceState::default();
    let env = NatureAmbienceEnv {
        cherry_blossom: true,
        leaf_blocks: vec![[10.0, 70.0, 20.0]],
        ..NatureAmbienceEnv::default()
    };
    run_ticks(&mut cherry, &config, &env, 120);
    let cherry_rgbs: Vec<[f32; 3]> = cherry
        .particles()
        .iter()
        .filter_map(|p| match p.kind {
            NatureParticleKind::FallingLeaf { rgb, .. } => Some(rgb),
            _ => None,
        })
        .collect();
    assert!(!cherry_rgbs.is_empty());
    assert!(cherry_rgbs.iter().all(|rgb| rgb[2] > 0.7 && rgb[0] > 0.9));

    // With no more spawn sites, every leaf expires cleanly.
    let empty_env = NatureAmbienceEnv::default();
    run_ticks(&mut autumn, &config, &empty_env, 400);
    assert_eq!(autumn.len(), 0, "leaves must expire cleanly");
}

#[test]
fn waterfall_mist_requires_a_real_drop() {
    let config = enabled_config();
    let is_mist = |k: &NatureParticleKind| matches!(k, NatureParticleKind::WaterfallMist { .. });

    // A drop of 6 blocks produces mist.
    let mut state = NatureAmbienceState::default();
    let env = NatureAmbienceEnv {
        waterfall_bases: vec![([50.0, 63.0, 60.0], 6.0)],
        ..NatureAmbienceEnv::default()
    };
    run_ticks(&mut state, &config, &env, 60);
    assert!(count_kind(&state, is_mist) > 0);
    for p in state.particles() {
        if let NatureParticleKind::WaterfallMist { alpha } = p.kind {
            assert!((0.0..=0.5).contains(&alpha));
        }
    }

    // A flat pond (drop < 3) produces none.
    let mut state = NatureAmbienceState::default();
    let env = NatureAmbienceEnv {
        waterfall_bases: vec![([50.0, 63.0, 60.0], 2.0), ([60.0, 63.0, 70.0], 0.0)],
        ..NatureAmbienceEnv::default()
    };
    run_ticks(&mut state, &config, &env, 200);
    assert_eq!(count_kind(&state, is_mist), 0);
}

#[test]
fn ambience_is_deterministic_and_handles_nan_dt() {
    let config = enabled_config();
    let env = rich_env(WeatherKind::Rain, true);
    let mut a = NatureAmbienceState::default();
    let mut b = NatureAmbienceState::default();
    for _ in 0..300 {
        tick_nature_ambience(&mut a, &config, &env, 0.05);
        tick_nature_ambience(&mut b, &config, &env, 0.05);
    }
    assert_eq!(a.particles(), b.particles());

    // NaN / absurd dt cannot corrupt state.
    tick_nature_ambience(&mut a, &config, &env, f32::NAN);
    tick_nature_ambience(&mut a, &config, &env, 1.0e9);
    for p in a.particles() {
        assert_finite3(p.pos);
        assert_finite3(p.vel);
        assert!(p.age_sec.is_finite());
    }
}

// ---------------------------------------------------------------------------
// Wind sway
// ---------------------------------------------------------------------------

#[test]
fn wind_sway_pins_grass_and_crop_roots() {
    let config = enabled_config();
    for weather in [WeatherKind::Clear, WeatherKind::Rain, WeatherKind::Thunder] {
        let params = wind_sway_params(&config, weather, 5_000);
        for category in [WindSwayCategory::ShortGrass, WindSwayCategory::Crop] {
            let offset = compute_wind_sway(&params, category, [3, 64, -7], [0.4, 0.0, 0.6]);
            assert_zero3(offset);
        }
    }
}

#[test]
fn wind_sway_scales_with_weather() {
    let config = enabled_config();
    let mut compared = false;
    for tick in 0..400u64 {
        let local = [0.5, 1.0, 0.5];
        let block = [1, 64, 1];
        let clear = compute_wind_sway(
            &wind_sway_params(&config, WeatherKind::Clear, tick),
            WindSwayCategory::ShortGrass,
            block,
            local,
        );
        let thunder = compute_wind_sway(
            &wind_sway_params(&config, WeatherKind::Thunder, tick),
            WindSwayCategory::ShortGrass,
            block,
            local,
        );
        if norm(clear) > 1e-6 {
            compared = true;
            // Same phase => amplitude ratio equals the strength ratio.
            let expected = 0.85 / 0.15;
            let got = norm(thunder) / norm(clear);
            assert!((got - expected).abs() < 1e-4, "ratio {got} != {expected}");
            assert!(norm(thunder) > norm(clear));
        }
    }
    assert!(compared, "no nonzero sway sample found in 400 ticks");
}

#[test]
fn wind_sway_pivots_lantern_from_the_top() {
    let config = enabled_config();
    let params = wind_sway_params(&config, WeatherKind::Thunder, 5_000);
    let block = [0, 70, 0];

    // Top pivot stays pinned.
    assert_zero3(compute_wind_sway(
        &params,
        WindSwayCategory::HangingLantern,
        block,
        [0.5, 1.0, 0.5],
    ));

    // Below the pivot, the lantern body swings.
    let mut moved = false;
    for tick in 0..400u64 {
        let params = wind_sway_params(&config, WeatherKind::Thunder, tick);
        let off = compute_wind_sway(
            &params,
            WindSwayCategory::HangingLantern,
            block,
            [0.5, 0.2, 0.5],
        );
        if norm(off) > 1e-6 {
            moved = true;
        }
        assert_finite3(off);
    }
    assert!(moved);
}

#[test]
fn wind_sway_leaves_rustle_but_stay_tiny() {
    let config = enabled_config();
    let mut max_norm = 0.0f32;
    for tick in 0..300u64 {
        let params = wind_sway_params(&config, WeatherKind::Thunder, tick);
        let off = compute_wind_sway(
            &params,
            WindSwayCategory::Leaves,
            [4, 80, 9],
            [0.5, 0.5, 0.5],
        );
        assert_finite3(off);
        max_norm = max_norm.max(norm(off));
    }
    // Thunder rustle remains far below a tenth of a block.
    assert!(max_norm < 0.1, "leaf rustle {max_norm} too strong");
    assert!(max_norm > 0.0, "leaves should actually rustle");
}

#[test]
fn wind_sway_disabled_pack_yields_zero() {
    let config = VanillaPlusNatureConfig::default();
    let params = wind_sway_params(&config, WeatherKind::Thunder, 5_000);
    assert_eq!(params.strength, 0.0);
    for category in [
        WindSwayCategory::ShortGrass,
        WindSwayCategory::Crop,
        WindSwayCategory::Leaves,
        WindSwayCategory::HangingLantern,
    ] {
        assert_zero3(compute_wind_sway(
            &params,
            category,
            [0, 0, 0],
            [0.5, 0.7, 0.5],
        ));
    }
}

#[test]
fn wind_sway_wgsl_snippet_is_well_formed() {
    let wgsl = MEGU_WIND_SWAY_WGSL_FN;
    assert!(wgsl.contains("fn megu_wind_sway"));
    assert!(wgsl.contains("vec3<f32>"));
    let balance: i32 = wgsl
        .chars()
        .map(|c| match c {
            '{' => 1,
            '}' => -1,
            _ => 0,
        })
        .sum();
    assert_eq!(balance, 0, "unbalanced braces in WGSL snippet");
}

// ---------------------------------------------------------------------------
// Emissive mob eyes
// ---------------------------------------------------------------------------

#[test]
fn emissive_mob_eyes_table() {
    for id in [
        "enderman",
        "spider",
        "cave_spider",
        "phantom",
        "blaze",
        "drowned",
        "warden",
        "glow_squid",
        "allay",
        "magma_cube",
    ] {
        let hint =
            emissive_mob_feature(id).unwrap_or_else(|| panic!("expected emissive hint for {id}"));
        for c in hint.rgb {
            assert!((0.0..=1.0).contains(&c));
        }
        assert!((0.0..=1.0).contains(&hint.intensity));
    }

    // Namespaced ids resolve identically.
    assert_eq!(
        emissive_mob_feature("minecraft:enderman"),
        emissive_mob_feature("enderman")
    );

    for id in ["cow", "villager", "minecraft:sheep", "zombie_piglin"] {
        assert!(emissive_mob_feature(id).is_none(), "{id} must not glow");
    }
}

#[test]
fn emissive_mob_eyes_respect_config() {
    let config = VanillaPlusNatureConfig::default();
    assert!(emissive_mob_hint(&config, "enderman").is_none());

    let config = enabled_config();
    assert!(emissive_mob_hint(&config, "enderman").is_some());

    let config = VanillaPlusNatureConfig {
        emissive_mob_eyes: false,
        ..enabled_config()
    };
    assert!(emissive_mob_hint(&config, "enderman").is_none());
}

// ---------------------------------------------------------------------------
// Mod integration: leaf registry & classifiers
// ---------------------------------------------------------------------------

#[test]
fn mod_leaf_classifier_prefers_registry_then_tags_then_suffix() {
    let mut reg = NatureModRegistry::new();
    reg.register_mod_leaf("biomesoplenty:maple_leaves", [0.85, 0.40, 0.10], false);

    // Explicit registry wins, even over tag/suffix heuristics.
    let style = classify_leaf_block("biomesoplenty:maple_leaves", &["#c:leaves"], &reg)
        .expect("registered maple leaves should classify");
    assert_eq!(style.rgb, [0.85, 0.40, 0.10]);
    assert!(!style.allow_autumn_tint);

    // Bare-path registration also resolves through a namespaced query.
    reg.register_mod_leaf("starlight_leaves", [0.5, 0.7, 1.0], true);
    let style = classify_leaf_block("mymod:starlight_leaves", &[], &reg)
        .expect("path-registered leaf should classify");
    assert_eq!(style.rgb, [0.5, 0.7, 1.0]);

    // Tag match on unregistered modded leaves -> name-derived tint.
    let tagged = classify_leaf_block("twilightforest:rainbow_oak_leaves", &["#c:leaves"], &reg)
        .expect("tagged mod leaves should classify");
    assert_finite3(tagged.rgb);

    // Suffix match with no tags at all -> deterministic derived tint.
    let lavender = classify_leaf_block("quark:lavender_blossom_leaves", &[], &reg)
        .expect("*_leaves suffix should classify");
    assert!(
        lavender.rgb[2] > lavender.rgb[0],
        "lavender leaves should lean violet, got {:?}",
        lavender.rgb
    );

    // Vanilla leaves classify by suffix too.
    assert!(classify_leaf_block("minecraft:oak_leaves", &[], &reg).is_some());

    // Solid blocks never shed leaves — even with a misleading tag.
    for id in [
        "minecraft:stone",
        "minecraft:oak_planks",
        "minecraft:iron_ore",
        "minecraft:dirt",
        "create:brass_casing",
        "mymod:compressed_leaves_block",
    ] {
        assert!(
            classify_leaf_block(id, &["#c:leaves"], &reg).is_none(),
            "{id} must not classify as leaves"
        );
    }
    // An unknown id with no tag and no leaf suffix stays silent.
    assert!(classify_leaf_block("mymod:total_mystery", &[], &reg).is_none());
}

#[test]
fn mod_leaf_registration_sanitizes_colours() {
    let mut reg = NatureModRegistry::new();
    reg.register_mod_leaf("mymod:glitch_leaves", [f32::NAN, 2.0, -1.0], true);
    let style = classify_leaf_block("mymod:glitch_leaves", &[], &reg)
        .expect("registered leaf should classify");
    for c in style.rgb {
        assert!(c.is_finite() && (0.0..=1.0).contains(&c));
    }
    assert_eq!(
        style.rgb[0], FALLBACK_LEAF_RGB[0],
        "NaN -> fallback green channel"
    );
    assert_eq!(style.rgb[1], 1.0, "over-1 clamped");
    assert_eq!(style.rgb[2], 0.0, "negative clamped");

    // Empty ids are refused, and unknown ids stay unclassified.
    reg.register_mod_leaf("   ", [0.1, 0.2, 0.3], true);
    assert!(reg.registered_leaf("   ").is_none());
    assert!(classify_leaf_block("mymod:never_registered", &[], &reg).is_none());
}

// ---------------------------------------------------------------------------
// Mod integration: wind sway classification
// ---------------------------------------------------------------------------

#[test]
fn mod_wind_sway_classifier_maps_plants_and_rejects_solids() {
    let reg = NatureModRegistry::new();
    let cases: &[(&str, &[&str], Option<WindSwayKind>)] = &[
        (
            "mymod:moonwheat",
            &["#c:crops"],
            Some(WindSwayCategory::Crop),
        ),
        (
            "mymod:copper_melon",
            &["minecraft:crops"],
            Some(WindSwayCategory::Crop),
        ),
        (
            "mymod:starlily",
            &["#c:flowers"],
            Some(WindSwayCategory::ShortGrass),
        ),
        (
            "mymod:moonseed",
            &["#minecraft:saplings"],
            Some(WindSwayCategory::ShortGrass),
        ),
        ("mymod:glow_fern", &[], Some(WindSwayCategory::ShortGrass)),
        (
            "mymod:sky_tall_grass",
            &[],
            Some(WindSwayCategory::ShortGrass),
        ),
        (
            "mymod:hanging_sun_lantern",
            &[],
            Some(WindSwayCategory::HangingLantern),
        ),
        (
            "minecraft:lantern",
            &[],
            Some(WindSwayCategory::HangingLantern),
        ),
        (
            "twilightforest:canopy_leaves",
            &["#c:leaves"],
            Some(WindSwayCategory::Leaves),
        ),
        ("mymod:mossy_leaves", &[], Some(WindSwayCategory::Leaves)),
        ("create:brass_casing", &[], None),
        ("minecraft:stone", &["#c:crops"], None), // solid veto beats a wrong tag
        ("minecraft:sea_lantern", &[], None),     // solid cube, not a hanger
        ("minecraft:jack_o_lantern", &[], None),  // pumpkin, not a hanger
        ("minecraft:grass_block", &[], None),     // full cube, not a plant
        ("mymod:unknown_thing", &[], None),
        ("mymod:catalog", &[], None),  // ends in "log" but not "_log"
        ("mymod:seagrass", &[], None), // ends in "grass" but not "_grass"
        ("", &["#c:leaves"], None),
    ];
    for &(id, tags, want) in cases {
        assert_eq!(classify_wind_sway_block(id, tags, &reg), want, "{id}");
    }
}

#[test]
fn registered_mod_leaf_sways_as_leaves_and_classified_kinds_pin() {
    let mut reg = NatureModRegistry::new();
    reg.register_mod_leaf("mymod:whisper_leaves", [0.4, 0.6, 0.5], true);
    assert_eq!(
        classify_wind_sway_block("mymod:whisper_leaves", &[], &reg),
        Some(WindSwayCategory::Leaves)
    );

    // Classified kinds feed compute_wind_sway with the documented pivots:
    // plants/crops pinned at local_y == 0, lanterns pinned at local_y == 1.
    let config = enabled_config();
    let params = wind_sway_params(&config, WeatherKind::Thunder, 5_000);
    let crop = classify_wind_sway_block("mymod:moonwheat", &["#c:crops"], &reg).unwrap();
    assert_zero3(compute_wind_sway(
        &params,
        crop,
        [1, 64, 1],
        [0.4, 0.0, 0.6],
    ));
    let lantern = classify_wind_sway_block("mymod:sun_lantern", &[], &reg).unwrap();
    assert_zero3(compute_wind_sway(
        &params,
        lantern,
        [1, 64, 1],
        [0.5, 1.0, 0.5],
    ));
}

// ---------------------------------------------------------------------------
// Mod integration: biome ambience classifier
// ---------------------------------------------------------------------------

fn profile_for(
    biome_id: &str,
    tags: &[&str],
    temperature: f32,
    has_precipitation: bool,
    atmosphere_density: f32,
) -> BiomeAmbienceProfile {
    classify_mod_biome_ambience(&ModBiomeFacts {
        biome_id,
        tags,
        temperature,
        has_precipitation,
        atmosphere_density,
    })
}

#[test]
fn magical_mod_biome_gets_cyan_violet_fireflies() {
    let p = profile_for(
        "twilightforest:firefly_forest",
        &["#c:is_magical", "#c:is_forest"],
        0.8,
        true,
        1.0,
    );
    assert!(p.allow_fireflies);
    assert!(
        p.firefly_rgb[2] > p.firefly_rgb[0] && p.firefly_rgb[2] > p.firefly_rgb[1],
        "magical fireflies should be cyan-violet, got {:?}",
        p.firefly_rgb
    );
    assert!(p.allow_falling_leaves && p.allow_rain_ripples && p.allow_clouds);
    assert_eq!(p.wind_multiplier, 1.0);

    // An untagged warm humid biome still gets fireflies, tinted warm.
    let p = profile_for("mymod:lush_meadow", &[], 0.8, true, 1.0);
    assert!(p.allow_fireflies);
    assert!(
        p.firefly_rgb[1] > p.firefly_rgb[2],
        "warm fireflies lean yellow-green, got {:?}",
        p.firefly_rgb
    );
}

#[test]
fn swamp_mod_biome_gets_full_warm_ambience() {
    let p = profile_for(
        "biomesoplenty:ominous_woods",
        &["#c:is_swamp"],
        0.7,
        true,
        1.0,
    );
    assert!(p.allow_fireflies);
    assert!(p.allow_falling_leaves);
    assert!(p.allow_rain_ripples);
    assert!(p.allow_clouds);
    assert_eq!(p.wind_multiplier, 1.0);
    assert!(
        p.firefly_rgb[1] > 0.9,
        "swamp fireflies stay warm green: {:?}",
        p.firefly_rgb
    );
}

#[test]
fn hostile_and_alien_biomes_stay_silent() {
    // Snowy: no fireflies or rain ripples at freezing temperatures.
    let p = profile_for("mymod:frost_tundra", &["#c:is_snowy"], 0.0, true, 1.0);
    assert!(!p.allow_fireflies && !p.allow_rain_ripples);

    // Desert: dry tag kills fireflies, no precipitation kills ripples.
    let p = profile_for(
        "mymod:red_dunes",
        &["#c:is_desert", "#c:is_dry"],
        0.9,
        false,
        1.0,
    );
    assert!(!p.allow_fireflies && !p.allow_rain_ripples);

    // Hot and precipitation-free counts as dry even without a tag.
    let p = profile_for("mymod:ash_wastes", &[], 1.2, false, 1.0);
    assert!(!p.allow_fireflies);

    // Nether: nothing at all, and no weather wind.
    let p = profile_for(
        "minecraft:nether_wastes",
        &["#c:is_nether"],
        2.0,
        false,
        1.0,
    );
    assert!(!p.allow_fireflies && !p.allow_falling_leaves);
    assert!(!p.allow_rain_ripples && !p.allow_clouds);
    assert_eq!(p.wind_multiplier, 0.0);

    // End: same, even with a bogus density claim.
    let p = profile_for("minecraft:end_highlands", &["#c:is_end"], 0.7, false, 1.0);
    assert!(!p.allow_fireflies && !p.allow_falling_leaves);
    assert!(!p.allow_rain_ripples && !p.allow_clouds);
    assert_eq!(p.wind_multiplier, 0.0);

    // Hard-vacuum space dimension: everything off purely by density.
    let p = profile_for("mymod:moon_surface", &[], 0.2, false, 0.0);
    assert!(!p.allow_fireflies && !p.allow_falling_leaves);
    assert!(!p.allow_rain_ripples && !p.allow_clouds);
    assert_eq!(p.wind_multiplier, 0.0);

    // Thin atmosphere sits between the two thresholds: no fireflies, still clouds.
    let p = profile_for("mymod:high_peaks", &[], 0.6, true, 0.20);
    assert!(!p.allow_fireflies && !p.allow_rain_ripples);
    assert!(p.allow_clouds && p.allow_falling_leaves);
    assert!(p.wind_multiplier > 0.0 && p.wind_multiplier < 1.0);

    // Underground cave: no sky, no wind, no fireflies — canopy may shed.
    let p = profile_for(
        "minecraft:deep_dark",
        &["#c:is_underground"],
        0.8,
        false,
        1.0,
    );
    assert!(!p.allow_fireflies && !p.allow_clouds);
    assert_eq!(p.wind_multiplier, 0.0);
    assert!(p.allow_falling_leaves);
}

#[test]
fn mod_biome_classifier_is_deterministic_and_nan_safe() {
    let facts = ModBiomeFacts {
        biome_id: "mymod:starlit_grove",
        tags: &["#c:is_magical"],
        temperature: 0.75,
        has_precipitation: true,
        atmosphere_density: 1.0,
    };
    assert_eq!(
        classify_mod_biome_ambience(&facts),
        classify_mod_biome_ambience(&facts)
    );

    // NaN atmosphere -> treated as vacuum -> everything off.
    let p = classify_mod_biome_ambience(&ModBiomeFacts {
        atmosphere_density: f32::NAN,
        ..facts
    });
    assert!(!p.allow_fireflies && !p.allow_falling_leaves);
    assert!(!p.allow_rain_ripples && !p.allow_clouds);
    assert_eq!(p.wind_multiplier, 0.0);

    // NaN temperature -> treated as frozen -> no fireflies or ripples.
    let p = classify_mod_biome_ambience(&ModBiomeFacts {
        temperature: f32::NAN,
        ..facts
    });
    assert!(!p.allow_fireflies && !p.allow_rain_ripples);
}

// ---------------------------------------------------------------------------
// Mod integration: emissive mob registry & atmosphere helpers
// ---------------------------------------------------------------------------

#[test]
fn mod_emissive_registry_extends_overrides_and_respects_config() {
    let mut reg = NatureModRegistry::new();
    let lich = MobEmissiveHint {
        feature: MobEmissiveFeature::Eyes,
        rgb: [0.60, 0.90, 1.00],
        intensity: 0.75,
    };
    reg.register_mod_emissive_mob("twilightforest:lich", lich);
    reg.register_mod_emissive_mob(
        "ars_nouveau:starbuncle",
        MobEmissiveHint {
            feature: MobEmissiveFeature::BodyGlow,
            rgb: [1.00, 0.85, 0.30],
            intensity: 0.50,
        },
    );

    let config = enabled_config();
    assert_eq!(
        emissive_mob_hint_with_registry("twilightforest:lich", &config, &reg),
        Some(lich)
    );
    assert!(emissive_mob_hint_with_registry("ars_nouveau:starbuncle", &config, &reg).is_some());

    // Vanilla fallback still resolves untouched mobs.
    assert_eq!(
        emissive_mob_hint_with_registry("minecraft:enderman", &config, &reg),
        emissive_mob_feature("enderman")
    );

    // A registry entry overrides the vanilla table for the same id.
    let custom = MobEmissiveHint {
        feature: MobEmissiveFeature::BodyGlow,
        rgb: [0.10, 1.00, 0.40],
        intensity: 0.20,
    };
    reg.register_mod_emissive_mob("minecraft:enderman", custom);
    assert_eq!(
        emissive_mob_hint_with_registry("minecraft:enderman", &config, &reg),
        Some(custom)
    );

    // Unknown mobs stay dark.
    assert!(emissive_mob_hint_with_registry("mymod:plain_slime", &config, &reg).is_none());

    // The config gate still rules: registered mobs stay dark when off.
    let off = VanillaPlusNatureConfig {
        emissive_mob_eyes: false,
        ..enabled_config()
    };
    assert!(emissive_mob_hint_with_registry("twilightforest:lich", &off, &reg).is_none());
    let disabled = VanillaPlusNatureConfig::default();
    assert!(emissive_mob_hint_with_registry("twilightforest:lich", &disabled, &reg).is_none());
}

#[test]
fn atmosphere_helpers_gate_clouds_and_wind() {
    let config = enabled_config();

    // Vacuum, a disabled flag, or NaN density -> zero cloud cells.
    for (density, allow) in [
        (0.0, true),
        (0.10, true),
        (f32::NAN, true),
        (f32::NEG_INFINITY, true),
        (1.0, false),
    ] {
        assert!(
            generate_layered_voxel_clouds_with_atmosphere(&config, 0.0, 0.0, 6_000, density, allow)
                .is_empty(),
            "density {density} allow {allow} must emit no clouds"
        );
    }

    // Normal atmosphere matches the plain generator exactly.
    assert_eq!(
        generate_layered_voxel_clouds_with_atmosphere(&config, 123.0, -45.0, 6_000, 1.0, true),
        generate_layered_voxel_clouds(&config, 123.0, -45.0, 6_000)
    );

    // Wind scales to zero in vacuum.
    let vacuum = wind_sway_params_with_atmosphere(&config, WeatherKind::Thunder, 5_000, 0.0);
    assert_eq!(vacuum.strength, 0.0);
    assert_zero3(compute_wind_sway(
        &vacuum,
        WindSwayCategory::ShortGrass,
        [0, 64, 0],
        [0.5, 0.9, 0.5],
    ));

    // Thin atmosphere attenuates; normal atmosphere keeps full strength.
    let thin = wind_sway_params_with_atmosphere(&config, WeatherKind::Clear, 5_000, 0.5);
    let full = wind_sway_params(&config, WeatherKind::Clear, 5_000);
    assert!(thin.strength > 0.0 && thin.strength < full.strength);
    assert_eq!(
        wind_sway_params_with_atmosphere(&config, WeatherKind::Clear, 5_000, 1.0).strength,
        full.strength
    );

    // The pack toggle still gates the atmosphere-aware variant.
    let disabled = VanillaPlusNatureConfig::default();
    assert_eq!(
        wind_sway_params_with_atmosphere(&disabled, WeatherKind::Thunder, 5_000, 1.0).strength,
        0.0
    );
}
