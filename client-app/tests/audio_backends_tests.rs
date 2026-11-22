// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! Cross-platform audio backend tests: deterministic per-OS sink probing
//! (Linux → PipeWire/ALSA, macOS → CoreAudio, Windows → WinMM), exact
//! RIFF/WAVE framing, `AudioLimiter` output bounds, and the headless
//! fail-closed `Null` path. No real devices are required — probing is
//! injected and every native open fails closed.

use mc_spine_client::audio_out::{
    detect_available_sinks_for_os, encode_pcm_le, open_sink, wav_header_bytes,
    AudioSessionConfig, AudioSinkKind, TargetOs,
};
use mc_spine_client::audio_qol::AudioLimiter;
use mc_spine_client::session::SoundMixer;
use mc_spine_client::sound_assets::{placeholder_pcm, MIX_SAMPLE_RATE};
use mc_spine_client::spatial_audio::SpatialAudioPreset;
use std::sync::Arc;

// ---------------------------------------------------------------------------
// Per-OS probe ordering
// ---------------------------------------------------------------------------

/// `detect_available_sinks_for_os` picks the real native backend for each
/// OS — never a silent `Null` while the probe reports availability.
#[test]
fn probe_selects_native_backend_per_os() {
    let all = |_: &str| true;
    assert_eq!(
        detect_available_sinks_for_os(TargetOs::Linux, all),
        vec![AudioSinkKind::PipeWire, AudioSinkKind::Alsa],
        "Linux must probe PipeWire first, then ALSA"
    );
    assert_eq!(
        detect_available_sinks_for_os(TargetOs::MacOs, all),
        vec![AudioSinkKind::CoreAudio],
        "macOS must probe CoreAudio"
    );
    assert_eq!(
        detect_available_sinks_for_os(TargetOs::Windows, all),
        vec![AudioSinkKind::WinMm],
        "Windows must probe WinMM"
    );
}

/// The `available` filter removes missing backends but keeps OS order.
#[test]
fn probe_filters_missing_backends() {
    // Only ALSA present on this Linux box: PipeWire is skipped, not
    // replaced by a wrong-OS backend.
    assert_eq!(
        detect_available_sinks_for_os(TargetOs::Linux, |k| k == "alsa"),
        vec![AudioSinkKind::Alsa]
    );
    assert_eq!(
        detect_available_sinks_for_os(TargetOs::Linux, |k| k == "pipewire"),
        vec![AudioSinkKind::PipeWire]
    );
    // macOS/Windows never see Linux subprocess sinks, and vice versa.
    assert_eq!(
        detect_available_sinks_for_os(TargetOs::MacOs, |k| k == "alsa"),
        vec![AudioSinkKind::Null]
    );
    assert_eq!(
        detect_available_sinks_for_os(TargetOs::Windows, |k| k == "pw"),
        vec![AudioSinkKind::Null]
    );
}

/// Headless / nothing probed → the explicit `Null` sentinel, on every OS.
#[test]
fn probe_falls_back_to_null_when_headless() {
    for os in [
        TargetOs::Linux,
        TargetOs::MacOs,
        TargetOs::Windows,
        TargetOs::Other,
    ] {
        assert_eq!(
            detect_available_sinks_for_os(os, |_| false),
            vec![AudioSinkKind::Null],
            "{os:?} must degrade to Null, not an empty or wrong list"
        );
    }
}

/// `open_sink` on a kind the host OS does not provide fails closed:
/// `null` sink, a warning, and no panic.
#[test]
fn wrong_os_backend_fails_closed_to_null() {
    #[cfg(not(target_os = "macos"))]
    {
        let (sink, warn) = open_sink(&AudioSinkKind::CoreAudio);
        assert_eq!(sink.label(), "null");
        assert!(warn.unwrap().contains("macOS"));
    }
    #[cfg(not(target_os = "windows"))]
    {
        let (sink, warn) = open_sink(&AudioSinkKind::WinMm);
        assert_eq!(sink.label(), "null");
        assert!(warn.unwrap().contains("Windows"));
    }
    #[cfg(target_os = "macos")]
    {
        // A real macOS run either opens AudioQueue or fails closed.
        let (sink, _warn) = open_sink(&AudioSinkKind::CoreAudio);
        assert!(matches!(sink.label(), "coreaudio" | "null"));
    }
    #[cfg(target_os = "windows")]
    {
        let (sink, _warn) = open_sink(&AudioSinkKind::WinMm);
        assert!(matches!(sink.label(), "winmm" | "null"));
    }
}

/// `Auto` on this host never panics and lands on a Linux subprocess sink
/// or the null fallback — and it never selects a wrong-OS backend.
#[test]
fn auto_sink_is_native_or_null_never_wrong_os() {
    let (mut sink, warn) = open_sink(&AudioSinkKind::Auto);
    assert!(matches!(sink.label(), "pipewire" | "alsa" | "null"));
    if sink.label() == "null" {
        assert!(warn.is_some(), "silent Null fallback is forbidden");
    }
    // The sink accepts a bounded push and finishes without panicking.
    sink.push(&[0i16; 480]).expect("sink push failed");
    sink.finish();
}

// ---------------------------------------------------------------------------
// WAV / PCM framing
// ---------------------------------------------------------------------------

/// `wav_header_bytes` emits the exact 44-byte RIFF/WAVE header for 48 kHz
/// s16 stereo, including patched total/data sizes.
#[test]
fn wav_header_bytes_is_exact_riff_wave() {
    let h = wav_header_bytes(12);
    assert_eq!(&h[0..4], b"RIFF");
    assert_eq!(u32::from_le_bytes(h[4..8].try_into().unwrap()), 36 + 12);
    assert_eq!(&h[8..12], b"WAVE");
    assert_eq!(&h[12..16], b"fmt ");
    assert_eq!(u32::from_le_bytes(h[16..20].try_into().unwrap()), 16);
    assert_eq!(u16::from_le_bytes(h[20..22].try_into().unwrap()), 1); // PCM
    assert_eq!(u16::from_le_bytes(h[22..24].try_into().unwrap()), 2); // stereo
    assert_eq!(
        u32::from_le_bytes(h[24..28].try_into().unwrap()),
        MIX_SAMPLE_RATE
    );
    assert_eq!(
        u32::from_le_bytes(h[28..32].try_into().unwrap()),
        MIX_SAMPLE_RATE * 4
    );
    assert_eq!(u16::from_le_bytes(h[32..34].try_into().unwrap()), 4); // align
    assert_eq!(u16::from_le_bytes(h[34..36].try_into().unwrap()), 16); // bits
    assert_eq!(&h[36..40], b"data");
    assert_eq!(u32::from_le_bytes(h[40..44].try_into().unwrap()), 12);

    // Empty stream: RIFF size is 36, data size 0.
    let h0 = wav_header_bytes(0);
    assert_eq!(u32::from_le_bytes(h0[4..8].try_into().unwrap()), 36);
    assert_eq!(u32::from_le_bytes(h0[40..44].try_into().unwrap()), 0);

    // Past the 4 GiB RIFF ceiling sizes saturate instead of wrapping.
    let hmax = wav_header_bytes(u64::from(u32::MAX) + 4096);
    assert_eq!(u32::from_le_bytes(hmax[4..8].try_into().unwrap()), u32::MAX);
    assert_eq!(u32::from_le_bytes(hmax[40..44].try_into().unwrap()), u32::MAX);
}

/// `encode_pcm_le` serializes interleaved s16 samples little-endian.
#[test]
fn encode_pcm_le_is_little_endian() {
    let bytes = encode_pcm_le(&[0x0102, -1, 32767, -32768]);
    assert_eq!(bytes, [0x02, 0x01, 0xFF, 0xFF, 0xFF, 0x7F, 0x00, 0x80]);
    assert!(encode_pcm_le(&[]).is_empty());
}

// ---------------------------------------------------------------------------
// Limiter bounds
// ---------------------------------------------------------------------------

/// `AudioLimiter::limit_mix_buffer` hard-clamps to `cap`, runs the soft
/// knee inside it, and scrubs non-finite samples to silence.
#[test]
fn limiter_bounds_every_sample() {
    let mut buf = [2.0f32, -3.0, f32::NAN, f32::INFINITY, 0.3, -0.95];
    AudioLimiter::limit_mix_buffer(&mut buf, 0.5);
    assert!(buf.iter().all(|s| s.is_finite() && s.abs() <= 0.5));
    assert_eq!(buf[2], 0.0);
    assert_eq!(buf[3], 0.0);
    assert_eq!(buf[4], 0.3, "samples under the knee pass through");

    // Full-scale input at cap 1.0 can never exceed ±1.0.
    let mut loud = vec![12.0f32; 1024];
    AudioLimiter::limit_mix_buffer(&mut loud, 1.0);
    assert!(loud.iter().all(|s| s.abs() <= 1.0));
}

/// End-to-end: a voice mixed through `SoundMixer::render` is limited to
/// the configured cap before it ever reaches a sink.
#[test]
fn mixer_output_stays_bounded_through_limiter() {
    let mut mixer = SoundMixer::new(MIX_SAMPLE_RATE, SpatialAudioPreset::Realistic);
    mixer.limiter.set_max_volume_cap(0.4);
    for _ in 0..8 {
        mixer.submit(mc_spine_client::session::PlaySoundEvent {
            label: "loud".to_string(),
            pcm: Arc::new(placeholder_pcm("entity.explode", 0, MIX_SAMPLE_RATE)),
            is_placeholder: true,
            category: 0,
            position: None,
            entity_id: None,
            volume: 1.0,
            pitch: 1.0,
            seed: 7,
        });
    }
    let mut out = vec![0.0f32; 4800 * 2];
    mixer.render(&mut out);
    let peak = out.iter().fold(0.0f32, |m, s| m.max(s.abs()));
    assert!(peak <= 0.4 + 1e-6, "limiter leak: peak {peak}");
}

/// Headless end-to-end: `AudioRuntime` over an explicit `Null` sink pumps
/// frames without a device and finishes cleanly.
#[test]
fn null_sink_runtime_pumps_without_panic() {
    let cfg = AudioSessionConfig {
        sink: AudioSinkKind::Null,
        assets_dir: None,
        asset_index: None,
        max_volume_cap: 1.0,
        max_concurrent_same_sound: 4,
        subtitle_path: None,
        subtitles_to_stderr: false,
    };
    assert!(mc_spine_client::audio_out::AudioRuntime::enabled(&cfg));
    let mut rt = mc_spine_client::audio_out::AudioRuntime::new(&cfg);
    assert_eq!(rt.sink_label(), "null");
    rt.pump_frames(480);
    rt.finish();
}
