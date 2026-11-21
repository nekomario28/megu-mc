// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! Free FPS cap, slider presets, Vanilla options compatibility, and the
//! deterministic [`FrameBudgetPacer`] slow-start/backoff contract.

use std::path::PathBuf;

use mc_spine_client::cli::parse_args;
use mc_spine_client::frame_pacer::FrameBudgetPacer;
use mc_spine_client::settings::{
    ClientSettings, FPS_SLIDER_PRESETS, MAX_MAX_FPS, MIN_MAX_FPS, VANILLA_OPTIONS_UNLIMITED_FPS,
};

fn scratch_dir(tag: &str) -> PathBuf {
    let dir =
        std::env::temp_dir().join(format!("mc-spine-frame-pacer-{tag}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn fps_bounds_and_slider_presets_are_canonical() {
    assert_eq!(MIN_MAX_FPS, 10);
    assert_eq!(MAX_MAX_FPS, 2000);
    assert_eq!(VANILLA_OPTIONS_UNLIMITED_FPS, 260);

    let expected: &[Option<u32>] = &[
        Some(30),
        Some(60),
        Some(120),
        Some(144),
        Some(165),
        Some(240),
        Some(280),
        Some(320),
        Some(360),
        Some(480),
        Some(600),
        None,
    ];
    assert_eq!(FPS_SLIDER_PRESETS, expected);
}

#[test]
fn cli_and_toml_accept_free_fps_values_and_unlimited_spellings() {
    let dir = scratch_dir("fps-values");

    for value in ["280", "320", "600", "1000"] {
        let mut cfg = parse_args(&["--max-fps", value]).unwrap();
        cfg.resolve_client_settings_from(&dir).unwrap();
        assert_eq!(
            cfg.client_settings.max_fps,
            Some(value.parse::<u32>().unwrap()),
            "--max-fps {value}"
        );
    }

    // Boundary values and the new free-cap range endpoints.
    for (value, want) in [("10", 10), ("2000", 2000), ("260", 260)] {
        let mut cfg = parse_args(&["--max-fps", value]).unwrap();
        cfg.resolve_client_settings_from(&dir).unwrap();
        assert_eq!(cfg.client_settings.max_fps, Some(want), "--max-fps {value}");
    }

    // 0 and the named unlimited spellings all resolve to None.
    for value in ["0", "unlimited", "none"] {
        let mut cfg = parse_args(&["--max-fps", value]).unwrap();
        cfg.resolve_client_settings_from(&dir).unwrap();
        assert_eq!(cfg.client_settings.max_fps, None, "--max-fps {value}");
    }

    // The background cap stays an independent setting.
    let mut cfg = parse_args(&["--max-fps", "600", "--background-fps", "20"]).unwrap();
    cfg.resolve_client_settings_from(&dir).unwrap();
    assert_eq!(cfg.client_settings.max_fps, Some(600));
    assert_eq!(cfg.client_settings.background_fps_limit, 20);

    // Invalid CLI values fail closed instead of clamping.
    for value in ["9", "2001", "-1", "fast", "1.5"] {
        assert!(
            parse_args(&["--max-fps", value]).is_err(),
            "--max-fps {value} must fail"
        );
    }

    // TOML accepts the same numeric range plus 0/unlimited/none.
    for value in ["280", "320", "600", "1000", "2000"] {
        let (settings, warnings) = ClientSettings::from_toml(&format!("max_fps = {value}\n"));
        assert_eq!(
            settings.max_fps,
            Some(value.parse::<u32>().unwrap()),
            "max_fps = {value}"
        );
        assert!(warnings.is_empty(), "{value}: {warnings:?}");
    }
    for line in [
        "max_fps = 0\n",
        "max_fps = \"unlimited\"\n",
        "max_fps = \"none\"\n",
    ] {
        let (settings, warnings) = ClientSettings::from_toml(line);
        assert_eq!(settings.max_fps, None, "{line}");
        assert!(warnings.is_empty(), "{line}: {warnings:?}");
    }

    // TOML 260 is an ordinary capped value here; only Vanilla's options.txt
    // treats 260 as Unlimited.
    let (settings, _) = ClientSettings::from_toml("max_fps = 260\n");
    assert_eq!(settings.max_fps, Some(260));

    // A malformed TOML value warns and leaves an existing cap untouched.
    let mut settings = ClientSettings {
        max_fps: Some(120),
        ..ClientSettings::default()
    };
    let warnings = settings.apply_toml("max_fps = \"fast\"\n");
    assert_eq!(settings.max_fps, Some(120));
    assert!(warnings.iter().any(|w| w.contains("max_fps")));

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn vanilla_options_txt_260_still_imports_as_unlimited() {
    let settings = ClientSettings::import_vanilla_options_txt("maxFps:260\n");
    assert_eq!(settings.max_fps, None);

    // Values below the Vanilla unlimited sentinel remain capped imports.
    let settings = ClientSettings::import_vanilla_options_txt("maxFps:144\n");
    assert_eq!(settings.max_fps, Some(144));

    // The dedicated sentinel, not the new 2000 FPS maximum, drives the rule.
    assert!(MAX_MAX_FPS > VANILLA_OPTIONS_UNLIMITED_FPS as u32);
}

#[test]
fn pacer_spreads_200_chunk_join_burst_without_exceeding_frame_caps() {
    let mut pacer =
        FrameBudgetPacer::with_limits(1_000, 1024 * 1024, 8).with_target_frame_us(10_000);
    pacer.on_world_join_or_teleport(16);

    let mut pending: Vec<usize> = (0..200).collect();
    let mut accepted = 0usize;
    let mut frames = 0u32;
    while !pending.is_empty() {
        frames += 1;
        assert!(frames <= 100, "join burst did not drain in time");
        let budget = pacer.begin_frame();
        assert!(budget.work_us <= pacer.max_frame_work_us);
        assert!(budget.upload_bytes <= pacer.max_upload_bytes_per_frame);
        assert!(budget.light_batches <= pacer.max_light_batches_per_frame);

        let mut still_pending = Vec::new();
        for chunk in pending {
            // 20 KiB / 10 us per chunk: the 1 MiB upload cap is the pacing
            // bottleneck while the 1 ms work cap still admits ~100 chunks.
            if pacer.try_consume_upload(20 * 1024, 10) {
                accepted += 1;
            } else {
                still_pending.push(chunk);
            }
        }
        pending = still_pending;

        assert!(pacer.consumed_work_us() <= budget.work_us);
        assert!(pacer.consumed_upload_bytes() <= budget.upload_bytes);
        assert!(pacer.remaining_work_us() + pacer.consumed_work_us() == budget.work_us);
        assert_eq!(
            pacer.remaining_upload_bytes() + pacer.consumed_upload_bytes(),
            budget.upload_bytes
        );
        pacer.end_frame(1_000);
    }

    assert_eq!(accepted, 200);
    assert!(frames > 1, "a 200-chunk burst must span multiple frames");
}

#[test]
fn pacer_slow_start_ramp_is_monotonic_from_radius_2_to_target_16() {
    let mut pacer = FrameBudgetPacer::default().with_ramp_frames(30);
    pacer.on_world_join_or_teleport(16);

    let mut radii = Vec::new();
    let mut upload_percents = Vec::new();
    for _ in 0..30 {
        let budget = pacer.begin_frame();
        radii.push(budget.active_radius);
        upload_percents.push(budget.upload_percent);
        pacer.end_frame(1_000);
    }

    assert_eq!(radii.first(), Some(&2));
    assert_eq!(radii.last(), Some(&16));
    assert!(radii.windows(2).all(|pair| pair[1] >= pair[0]));
    assert_eq!(upload_percents.first(), Some(&25));
    assert_eq!(upload_percents.last(), Some(&100));
    assert!(upload_percents.windows(2).all(|pair| pair[1] >= pair[0]));
    assert!(!pacer.is_ramping());
    assert_eq!(pacer.active_radius(), 16);

    let budget = pacer.begin_frame();
    assert_eq!(budget.active_radius, 16);
    assert_eq!(budget.upload_percent, 100);
    assert_eq!(budget.upload_bytes, pacer.max_upload_bytes_per_frame);
    pacer.end_frame(1_000);
}

#[test]
fn pacer_overrun_backoff_halves_then_recovers_smoothly() {
    let mut pacer =
        FrameBudgetPacer::with_limits(2_000, 4 * 1024 * 1024, 8).with_target_frame_us(1_000);

    pacer.begin_frame();
    pacer.end_frame(2_000); // 200% of target: over the 125% backoff threshold.
    assert_eq!(pacer.last_frame_us(), 2_000);

    let spiked_next = pacer.begin_frame();
    assert_eq!(spiked_next.backoff_percent, 50);
    assert_eq!(spiked_next.work_us, 1_000);
    assert_eq!(spiked_next.upload_bytes, 2 * 1024 * 1024);
    assert!(pacer.is_backoff_active());
    pacer.end_frame(500);

    let recovering = pacer.begin_frame();
    assert_eq!(recovering.backoff_percent, 75);
    assert_eq!(recovering.work_us, 1_500);
    assert_eq!(recovering.upload_bytes, 3 * 1024 * 1024);
    pacer.end_frame(500);

    let recovered = pacer.begin_frame();
    assert_eq!(recovered.backoff_percent, 100);
    assert_eq!(recovered.work_us, 2_000);
    assert_eq!(recovered.upload_bytes, 4 * 1024 * 1024);
    assert!(!pacer.is_backoff_active());
    pacer.end_frame(500);
}

#[test]
fn pacer_consumption_is_atomic_and_leaves_remainders_for_later_frames() {
    let mut pacer = FrameBudgetPacer::with_limits(100, 1_000, 2);

    // Before begin_frame there is no open frame budget.
    assert!(!pacer.try_consume_upload(1, 1));

    let budget = pacer.begin_frame();
    assert_eq!(budget.work_us, 100);
    assert_eq!(budget.upload_bytes, 1_000);
    assert_eq!(budget.light_batches, 2);

    assert!(pacer.try_consume_upload(600, 40));
    assert_eq!(pacer.remaining_upload_bytes(), 400);
    assert_eq!(pacer.remaining_work_us(), 60);

    // An upload that cannot fit both allowances consumes nothing.
    assert!(!pacer.try_consume_upload(500, 10));
    assert_eq!(pacer.remaining_upload_bytes(), 400);
    assert_eq!(pacer.remaining_work_us(), 60);
    assert!(!pacer.try_consume_mesh(70));
    assert_eq!(pacer.remaining_work_us(), 60);

    assert!(pacer.try_consume_light(20));
    assert!(pacer.try_consume_mesh(30));
    assert_eq!(pacer.remaining_light_batches(), 1);
    assert_eq!(pacer.remaining_work_us(), 10);
    assert_eq!(pacer.consumed_light_batches(), 1);
    assert_eq!(pacer.consumed_work_us(), 90);

    pacer.end_frame(100);
    let next = pacer.begin_frame();
    assert_eq!(next.work_us, 100);
    assert_eq!(next.upload_bytes, 1_000);
    assert_eq!(next.light_batches, 2);
}
