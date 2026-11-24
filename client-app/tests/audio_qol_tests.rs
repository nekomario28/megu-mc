// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! Tests for the client QoL lane: hard max-volume limiter + same-sound
//! coalescing, smooth zoom, low-durability tool guard, extended chat
//! history, and `[qol]` settings persistence.

use megu_client::audio_qol::{
    check_tool_durability_warning, AudioLimiter, ChatHistory, SoundAdmission, SoundKey,
    ToolDurability, ZoomState, DEFAULT_CHAT_HISTORY_LINES, DEFAULT_TOOL_WARN_PERCENT,
    DEFAULT_ZOOM_DIVISOR, MAX_CHAT_HISTORY_LINES, MAX_TOOL_WARN_PERCENT, MAX_VOLUME_CAP,
    MIN_CHAT_HISTORY_LINES, MIN_ZOOM_DIVISOR,
};
use megu_client::settings::{ClientQolSettings, ClientSettings};

fn assert_within_cap(samples: &[f32], cap: f32) {
    for (i, s) in samples.iter().enumerate() {
        assert!(
            s.is_finite(),
            "sample {i} is non-finite after limiting: {s}"
        );
        assert!(
            s.abs() <= cap,
            "sample {i} = {s} exceeds cap {cap}"
        );
    }
}

#[test]
fn peak_limiter_guarantees_cap_on_50x_spikes_and_nan() {
    let cap = 0.5_f32;
    // 50 thunder/Wither-style spikes summed into one mix buffer, plus NaN
    // and +/-Inf contamination.
    let mut samples = Vec::with_capacity(512);
    for i in 0..500 {
        samples.push(if i % 97 == 0 {
            f32::NAN
        } else if i % 89 == 0 {
            f32::INFINITY
        } else if i % 83 == 0 {
            f32::NEG_INFINITY
        } else {
            // Alternating huge positive/negative spikes that sum far past 1.0.
            let spike = 40.0 + (i % 7) as f32;
            if i % 2 == 0 { spike } else { -spike }
        });
    }
    samples.extend_from_slice(&[f32::NAN, f32::INFINITY, f32::NEG_INFINITY, 0.25]);

    AudioLimiter::limit_mix_buffer(&mut samples, cap);
    assert_within_cap(&samples, cap);
    // Quiet samples below the knee survive untouched.
    assert_eq!(samples[samples.len() - 1], 0.25);

    // cap = 0 mutes everything, including non-finite input.
    let mut muted = vec![f32::NAN, 0.5, -0.5, f32::INFINITY];
    AudioLimiter::limit_mix_buffer(&mut muted, 0.0);
    assert_within_cap(&muted, 0.0);
    assert!(muted.iter().all(|s| *s == 0.0));

    // cap = 1.0 still scrubs NaN/Inf; samples below the 0.8 knee pass through.
    let mut dirty = vec![f32::NAN, f32::INFINITY, 0.75, -0.75];
    AudioLimiter::limit_mix_buffer(&mut dirty, 1.0);
    assert_eq!(dirty[0], 0.0);
    assert_eq!(dirty[1], 0.0);
    assert_eq!(dirty[2], 0.75);
    assert_eq!(dirty[3], -0.75);

    // Instance-level limiter uses its configured cap.
    let limiter = AudioLimiter::with_limits(0.5, 4);
    let mut mix = vec![10.0_f32; 64];
    limiter.limit_mix(&mut mix);
    assert_within_cap(&mix, 0.5);

    // Out-of-range / non-finite caps clamp sanely.
    assert_eq!(AudioLimiter::with_limits(2.5, 4).max_volume_cap(), MAX_VOLUME_CAP);
    assert_eq!(AudioLimiter::with_limits(-1.0, 4).max_volume_cap(), 0.0);
    assert_eq!(AudioLimiter::with_limits(f32::NAN, 4).max_volume_cap(), 1.0);
    assert_eq!(AudioLimiter::with_limits(1.0, 0).max_concurrent_same_sound(), 1);
    assert_eq!(AudioLimiter::with_limits(1.0, 200).max_concurrent_same_sound(), 32);
}

#[test]
fn same_sound_coalescing_caps_50_identical_sounds_per_tick() {
    let mut limiter = AudioLimiter::with_limits(1.0, 4);
    let key = SoundKey::at_block(0xC0FFEE, 10, 64, -3);

    let mut plays = 0u32;
    let mut attenuated = 0u32;
    let mut suppressed = 0u32;
    for _ in 0..50 {
        match limiter.register_sound(key, 100) {
            SoundAdmission::Play => plays += 1,
            SoundAdmission::Attenuate { gain } => {
                attenuated += 1;
                assert!(gain > 0.0 && gain < 1.0);
            }
            SoundAdmission::Suppress => suppressed += 1,
        }
    }
    assert_eq!(plays, 4, "only the concurrent budget plays at full volume");
    assert_eq!(attenuated + suppressed, 46);
    assert!(suppressed > 0, "redundant stacking is eventually dropped");

    // A different cell or sound id is an independent stream.
    assert_eq!(
        limiter.register_sound(SoundKey::at_block(0xC0FFEE, 200, 64, -3), 100),
        SoundAdmission::Play
    );
    assert_eq!(
        limiter.register_sound(SoundKey::at_block(0xBEEF, 10, 64, -3), 100),
        SoundAdmission::Play
    );

    // Next tick resets the counters: the same sound plays again at full volume.
    assert_eq!(limiter.register_sound(key, 101), SoundAdmission::Play);
}

#[test]
fn smooth_zoom_interpolates_monotonically_and_scales_sensitivity() {
    let mut zoom = ZoomState::with_zoom_divisor(70.0, 4.0);
    assert!((zoom.fov() - 70.0).abs() < f32::EPSILON);
    assert_eq!(zoom.zoom_factor(), 1.0);

    zoom.set_zoomed(true);
    let mut prev = f32::INFINITY;
    let mut final_fov = 0.0;
    for _ in 0..600 {
        let fov = zoom.tick(1.0 / 60.0);
        assert!(fov <= prev + 1e-4, "zoom-in must be monotonic: {fov} > {prev}");
        prev = fov;
        final_fov = fov;
    }
    // Settles at base / divisor without overshooting.
    assert!((final_fov - 17.5).abs() < 0.01, "final fov {final_fov}");
    assert!(final_fov > 17.5, "exponential approach never undershoots");

    // Sensitivity scales down by the current zoom factor.
    let sens = zoom.scaled_sensitivity(0.5);
    assert!((sens - 0.125).abs() < 0.01, "zoomed sensitivity {sens}");
    assert!(zoom.zoom_factor() > 3.9);

    // Releasing zoom glides back to the base FOV monotonically upward.
    zoom.set_zoomed(false);
    let mut prev = 0.0;
    for _ in 0..600 {
        let fov = zoom.tick(1.0 / 60.0);
        assert!(fov >= prev - 1e-4, "zoom-out must be monotonic: {fov} < {prev}");
        prev = fov;
    }
    assert!((prev - 70.0).abs() < 0.01);
    assert!((zoom.scaled_sensitivity(0.5) - 0.5).abs() < 0.01);

    // Divisor clamps and non-finite dt are safe.
    zoom.set_zoom_divisor(100.0);
    assert_eq!(zoom.zoom_divisor(), 10.0);
    zoom.set_zoom_divisor(f32::NAN);
    assert_eq!(zoom.zoom_divisor(), DEFAULT_ZOOM_DIVISOR);
    zoom.set_zoomed(true);
    zoom.tick(f32::NAN);
    zoom.tick(-5.0);
}

#[test]
fn tool_durability_warns_at_threshold_and_blocks_only_when_opted_in() {
    // max 1000, warn 10% -> remaining <= 100 warns.
    assert_eq!(check_tool_durability_warning(0, 1000, 10, false), ToolDurability::Ok);
    assert_eq!(
        check_tool_durability_warning(899, 1000, 10, false),
        ToolDurability::Ok
    );
    assert_eq!(
        check_tool_durability_warning(900, 1000, 10, false),
        ToolDurability::LowWarning { remaining: 100 }
    );
    assert_eq!(
        check_tool_durability_warning(990, 1000, 10, false),
        ToolDurability::LowWarning { remaining: 10 }
    );

    // Final use: without prevent_tool_break the swing proceeds (warning only);
    // with it, the last swing is blocked.
    assert_eq!(
        check_tool_durability_warning(999, 1000, 10, false),
        ToolDurability::LowWarning { remaining: 1 }
    );
    assert_eq!(
        check_tool_durability_warning(999, 1000, 10, true),
        ToolDurability::BlockLastUse
    );
    assert_eq!(
        check_tool_durability_warning(1000, 1000, 10, true),
        ToolDurability::Ok,
        "already-broken tool has nothing left to protect"
    );
    assert_eq!(
        check_tool_durability_warning(500, 1000, 0, false),
        ToolDurability::Ok,
        "warn_percent 0 disables warnings"
    );
    assert_eq!(check_tool_durability_warning(0, 0, 10, true), ToolDurability::Ok);
    // warn_percent clamps at 100.
    assert_eq!(
        check_tool_durability_warning(1, 1000, 200, false),
        ToolDurability::LowWarning { remaining: 999 }
    );
    let _ = DEFAULT_TOOL_WARN_PERCENT;
    let _ = MAX_TOOL_WARN_PERCENT;
}

#[test]
fn chat_history_ring_cap_reconnect_and_timestamps() {
    // Ring cap: smallest allowed capacity is 100.
    let mut history = ChatHistory::with_capacity(10);
    assert_eq!(history.capacity(), MIN_CHAT_HISTORY_LINES);
    for i in 0..150 {
        history.push(format!("line {i}"));
    }
    assert_eq!(history.len(), MIN_CHAT_HISTORY_LINES);
    assert_eq!(history.iter().next(), Some("line 50"));
    assert_eq!(history.recent(1), vec!["line 149"]);

    // set_capacity grows and shrinks with clamping.
    history.set_capacity(MAX_CHAT_HISTORY_LINES + 10);
    assert_eq!(history.capacity(), MAX_CHAT_HISTORY_LINES);
    history.set_capacity(120);
    history.push("extra");
    assert_eq!(history.len(), 101, "capacity grows without inventing lines");

    // Reconnect retention on by default; disconnect keeps lines.
    history.on_server_disconnect();
    assert_eq!(history.len(), 101);
    history.set_retain_across_reconnect(false);
    history.on_server_disconnect();
    assert!(history.is_empty());

    // Timestamp prefix only when enabled; caller supplies the stamp.
    let mut stamped = ChatHistory::new();
    stamped.push_with_timestamp("14:32", "hello");
    assert_eq!(stamped.recent(1), vec!["hello"], "timestamps off by default");
    stamped.set_show_timestamps(true);
    stamped.push_with_timestamp("14:33", "world");
    assert_eq!(stamped.recent(1), vec!["[14:33] world"]);
    // A tick counter works as the "timestamp" too.
    stamped.push_with_timestamp("t0042", "tick");
    assert_eq!(stamped.recent(1), vec!["[t0042] tick"]);

    assert_eq!(ChatHistory::new().capacity(), DEFAULT_CHAT_HISTORY_LINES);
}

#[test]
fn qol_settings_toml_roundtrip_and_omitted_defaults() {
    let settings = ClientSettings {
        qol: ClientQolSettings {
            max_volume_cap: 0.5,
            max_concurrent_same_sound: 8,
            zoom_divisor: 6.0,
            tool_warn_percent: 25,
            prevent_tool_break: true,
            chat_history_lines: 250,
            retain_chat_on_reconnect: false,
        },
        ..ClientSettings::default()
    };
    let text = settings.to_toml();
    assert!(text.contains("[qol]"));
    assert!(text.contains("max_volume_cap = 0.5"));

    let (back, warnings) = ClientSettings::from_toml(&text);
    assert_eq!(back.qol, settings.qol);
    assert_eq!(back, settings);
    assert!(warnings.is_empty(), "unexpected warnings: {warnings:?}");

    // apply_toml overlays only the qol keys a file defines.
    let mut s = ClientSettings::default();
    s.apply_toml("[qol]\nmax_volume_cap = 0.25\nzoom_divisor = 2.0\n");
    assert_eq!(s.qol.max_volume_cap, 0.25);
    assert_eq!(s.qol.zoom_divisor, 2.0);
    assert_eq!(s.qol.max_concurrent_same_sound, 4, "undefined keys keep defaults");

    // Files written before [qol] existed parse with all defaults.
    let (legacy, warnings) = ClientSettings::from_toml("view_distance = 20\n");
    assert_eq!(legacy.qol, ClientQolSettings::default());
    assert!(warnings.is_empty());

    // Out-of-range values clamp with a warning instead of failing.
    let (clamped, warnings) = ClientSettings::from_toml(
        "[qol]\nmax_volume_cap = 9.0\nzoom_divisor = 0.1\nmax_concurrent_same_sound = 99\nchat_history_lines = 5\n",
    );
    assert_eq!(clamped.qol.max_volume_cap, MAX_VOLUME_CAP);
    assert_eq!(clamped.qol.zoom_divisor, MIN_ZOOM_DIVISOR);
    assert_eq!(clamped.qol.max_concurrent_same_sound, 32);
    assert_eq!(clamped.qol.chat_history_lines, MIN_CHAT_HISTORY_LINES);
    assert!(warnings.iter().any(|w| w.contains("max_volume_cap")));
    assert!(warnings.iter().any(|w| w.contains("zoom_divisor")));
    assert!(warnings.iter().any(|w| w.contains("chat_history_lines")));

    // Invalid values warn and keep the default.
    let (bad, warnings) = ClientSettings::from_toml("[qol]\nmax_volume_cap = \"loud\"\n");
    assert_eq!(
        bad.qol.max_volume_cap,
        megu_client::settings::DEFAULT_MAX_VOLUME_CAP
    );
    assert!(warnings.iter().any(|w| w.contains("max_volume_cap")));
}

#[test]
fn first_launch_audio_defaults_are_ear_safe_and_explicit_values_win() {
    let fresh = ClientSettings::default();
    assert!(
        fresh.volume_master <= 60,
        "first-launch master volume {} must stay ear-safe",
        fresh.volume_master
    );
    assert!(
        fresh.volume_music <= 60,
        "first-launch music volume {} must stay ear-safe",
        fresh.volume_music
    );
    assert!(
        fresh.qol.max_volume_cap <= 0.90,
        "first-launch max_volume_cap {} must stay ear-safe",
        fresh.qol.max_volume_cap
    );
    assert_eq!(fresh.qol.max_concurrent_same_sound, 4);

    // A TOML file that explicitly asks for full volume is honored verbatim.
    let (explicit, warnings) = ClientSettings::from_toml(
        "volume_master = 100\nvolume_music = 100\n[qol]\nmax_volume_cap = 1.0\n",
    );
    assert!(warnings.is_empty(), "unexpected warnings: {warnings:?}");
    assert_eq!(explicit.volume_master, 100);
    assert_eq!(explicit.volume_music, 100);
    assert_eq!(explicit.qol.max_volume_cap, 1.0);
}
