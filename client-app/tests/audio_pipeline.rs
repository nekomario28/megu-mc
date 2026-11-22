// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! Behavior-pinning tests for the live sound path: 767 sound packet
//! decoding, asset-index resolution, the deterministic mixer (cap/steal,
//! stop, limiter, subtitles), and sink behavior. Synthetic fixtures only —
//! no Mojang audio.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use mc_spine_client::audio_out::{open_sink, AudioSinkKind};
use mc_spine_client::cli::parse_args;
use mc_spine_client::protocol::{decode_play_sound_body, decode_stop_sound_body, write_varint};
use mc_spine_client::session::{PlaySoundEvent, SoundMixer};
use mc_spine_client::sound_assets::{
    placeholder_pcm, sound_name_for_id, SoundLibrary, MIX_SAMPLE_RATE,
};
use mc_spine_client::spatial_audio::{ListenerPose, SpatialAudioPreset, SubtitleDirection};

fn put_string(buf: &mut Vec<u8>, s: &str) {
    write_varint(buf, s.len() as i32);
    buf.extend_from_slice(s.as_bytes());
}

fn sound_event(name: &str, volume: f32, position: Option<[f32; 3]>) -> PlaySoundEvent {
    PlaySoundEvent {
        label: name.to_string(),
        pcm: Arc::new(placeholder_pcm(name, 0, MIX_SAMPLE_RATE)),
        stereo_pcm: None,
        max_range_m: f32::INFINITY,
        is_placeholder: true,
        category: 0,
        position,
        entity_id: None,
        volume,
        pitch: 1.0,
        seed: 42,
    }
}

// ---------------------------------------------------------------------------
// Wire decoding
// ---------------------------------------------------------------------------

#[test]
fn sound_effect_decodes_inline_event() {
    let mut body = Vec::new();
    write_varint(&mut body, 0);
    put_string(&mut body, "minecraft:block.stone.break");
    body.push(0); // has_fixed_range = false
    write_varint(&mut body, 3); // category: blocks
    for v in [80i32, 160, -24] {
        body.extend_from_slice(&v.to_be_bytes());
    }
    body.extend_from_slice(&0.75f32.to_be_bytes());
    body.extend_from_slice(&0.9f32.to_be_bytes());
    body.extend_from_slice(&12345i64.to_be_bytes());
    let ev = decode_play_sound_body(&body, false).unwrap();
    assert_eq!(ev.name.as_deref(), Some("minecraft:block.stone.break"));
    assert_eq!(ev.registry_id, None);
    assert_eq!(ev.category, 3);
    assert_eq!(ev.position, Some([10.0, 20.0, -3.0]));
    assert_eq!(ev.entity_id, None);
    assert_eq!(ev.volume, 0.75);
    assert_eq!(ev.pitch, 0.9);
    assert_eq!(ev.seed, 12345);
}

#[test]
fn sound_effect_decodes_fixed_range_inline() {
    let mut body = Vec::new();
    write_varint(&mut body, 0);
    put_string(&mut body, "entity.test.ping");
    body.push(1);
    body.extend_from_slice(&16.0f32.to_be_bytes());
    write_varint(&mut body, 0);
    for v in [0i32; 3] {
        body.extend_from_slice(&v.to_be_bytes());
    }
    body.extend_from_slice(&1.0f32.to_be_bytes());
    body.extend_from_slice(&1.0f32.to_be_bytes());
    body.extend_from_slice(&0i64.to_be_bytes());
    let ev = decode_play_sound_body(&body, false).unwrap();
    assert_eq!(ev.fixed_range, Some(16.0));
}

#[test]
fn sound_effect_decodes_registry_holder() {
    let mut body = Vec::new();
    write_varint(&mut body, 6 + 1); // registry id 6 (entity.player.levelup)
    write_varint(&mut body, 0);
    for v in [0i32; 3] {
        body.extend_from_slice(&v.to_be_bytes());
    }
    body.extend_from_slice(&1.0f32.to_be_bytes());
    body.extend_from_slice(&1.0f32.to_be_bytes());
    body.extend_from_slice(&7i64.to_be_bytes());
    let ev = decode_play_sound_body(&body, false).unwrap();
    assert_eq!(ev.registry_id, Some(6));
    // Registry ids the mc-spine server emits map to event names; a
    // pinned spot-check plus coverage of unknown ids.
    assert_eq!(sound_name_for_id(1362), Some("block.stone.break"));
    assert_eq!(sound_name_for_id(6), None);
}

#[test]
fn sound_entity_decodes_entity_id() {
    let mut body = Vec::new();
    write_varint(&mut body, 0);
    put_string(&mut body, "entity.cow.milk");
    body.push(0);
    write_varint(&mut body, 7); // category: neutral
    write_varint(&mut body, 9999); // entity id
    body.extend_from_slice(&0.5f32.to_be_bytes());
    body.extend_from_slice(&1.2f32.to_be_bytes());
    body.extend_from_slice(&(-3i64).to_be_bytes());
    let ev = decode_play_sound_body(&body, true).unwrap();
    assert_eq!(ev.entity_id, Some(9999));
    assert_eq!(ev.position, None);
    assert_eq!(ev.category, 7);
}

#[test]
fn stop_sound_decodes_flag_combinations() {
    // flags=0x03: category + name
    let mut body = vec![0x03];
    write_varint(&mut body, 2);
    put_string(&mut body, "block.stone.break");
    let s = decode_stop_sound_body(&body).unwrap();
    assert_eq!(s.category, Some(2));
    assert_eq!(s.name.as_deref(), Some("block.stone.break"));

    // flags=0: stop everything
    let s = decode_stop_sound_body(&[0x00]).unwrap();
    assert_eq!(s.category, None);
    assert_eq!(s.name, None);

    // flags=0x02: name only
    let mut body = vec![0x02];
    put_string(&mut body, "ambient.cave");
    let s = decode_stop_sound_body(&body).unwrap();
    assert_eq!(s.category, None);
    assert_eq!(s.name.as_deref(), Some("ambient.cave"));
}

// ---------------------------------------------------------------------------
// Asset index + sounds.json resolution
// ---------------------------------------------------------------------------

fn fixture_assets() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("mc-audio-fixture-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("indexes")).unwrap();
    let h1 = "aa".repeat(20);
    let h2 = "bb".repeat(20);
    let h3 = "cc".repeat(20);
    std::fs::create_dir_all(dir.join(format!("objects/{}/", &h1[..2]))).unwrap();
    std::fs::create_dir_all(dir.join(format!("objects/{}/", &h2[..2]))).unwrap();
    std::fs::create_dir_all(dir.join(format!("objects/{}/", &h3[..2]))).unwrap();
    // sounds.json itself is an object in the vanilla index.
    std::fs::write(dir.join(format!("objects/{}/{}", &h1[..2], h1)), "").unwrap();
    std::fs::write(dir.join(format!("objects/{}/{}", &h2[..2], h2)), "").unwrap();
    let sounds_json = r#"{
        "test.ping": {"sounds": [
            {"name": "test/ping1", "volume": 0.8},
            {"name": "test/ping2", "volume": 0.4, "pitch": 1.5, "weight": 3}
        ]},
        "test.alias": {"sounds": [{"name": "test.ping", "type": "event"}]}
    }"#;
    std::fs::write(
        dir.join(format!("objects/{}/{}", &h3[..2], h3)),
        sounds_json,
    )
    .unwrap();
    let index = format!(
        r#"{{"objects": {{
            "minecraft/sounds/test/ping1.ogg": {{"hash": "{h1}", "size": 4}},
            "minecraft/sounds/test/ping2.ogg": {{"hash": "{h2}", "size": 4}},
            "minecraft/sounds.json": {{"hash": "{h3}", "size": 300}}
        }}}}"#
    );
    std::fs::write(dir.join("indexes/fixture.json"), index).unwrap();
    dir
}

#[test]
fn library_resolves_variants_deterministically() {
    let dir = fixture_assets();
    let lib = SoundLibrary::load(&dir, "fixture").unwrap();
    assert_eq!(lib.event_count(), 2);
    // Same seed -> same variant, always.
    let a = lib.resolve("test.ping", 0).unwrap();
    let b = lib.resolve("test.ping", 0).unwrap();
    assert_eq!(a.path, b.path);
    assert_eq!(a.volume, b.volume);
    // Weights: ping2 weight 3 of 4 -> seeds picking slot 1..4 get ping2.
    let pick1 = lib.resolve("test.ping", 1).unwrap();
    assert!(pick1.path.ends_with("bb".repeat(20)));
    assert_eq!(pick1.pitch, 1.5);
    // Namespaced + entity.alias event indirection resolves too.
    let alias = lib.resolve("minecraft:test.alias", 1).unwrap();
    assert_eq!(alias.path, pick1.path);
    assert!(lib.resolve("test.missing", 0).is_none());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn library_placeholder_tone_is_deterministic_and_category_tinted() {
    let a = placeholder_pcm("entity.player.levelup", 0, MIX_SAMPLE_RATE);
    let b = placeholder_pcm("entity.player.levelup", 0, MIX_SAMPLE_RATE);
    assert_eq!(a, b, "same name+seed -> identical PCM");
    let c = placeholder_pcm("entity.player.levelup", 5, MIX_SAMPLE_RATE);
    assert_ne!(a, c, "category changes the tone");
    assert!(a.iter().any(|s| s.abs() > 0.01), "tone is non-silent");
    assert!(a.iter().all(|s| s.is_finite() && s.abs() <= 1.0));
}

#[test]
fn decode_ogg_rejects_garbage() {
    let dir = std::env::temp_dir().join(format!("mc-ogg-bad-{}", std::process::id()));
    std::fs::write(&dir, b"not an ogg file").unwrap();
    assert!(mc_spine_client::sound_assets::decode_ogg_mono(&dir, 1 << 20, 1 << 20).is_err());
    let _ = std::fs::remove_file(&dir);
}

/// Real Vorbis decode path — exercised against an ffmpeg-generated OGG
/// (skipped when ffmpeg is absent).
#[test]
fn decode_ogg_vorbis_real_file() {
    if std::process::Command::new("ffmpeg")
        .arg("-version")
        .output()
        .map(|o| !o.status.success())
        .unwrap_or(true)
    {
        eprintln!("skip: ffmpeg not on PATH");
        return;
    }
    let ogg = std::env::temp_dir().join(format!("mc-tone-{}.ogg", std::process::id()));
    let ok = std::process::Command::new("ffmpeg")
        .args([
            "-y",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:duration=0.25:sample_rate=44100",
            "-c:a",
            "libvorbis",
            "-q:a",
            "3",
        ])
        .arg(&ogg)
        .output()
        .unwrap();
    assert!(ok.status.success());
    let (pcm, rate) =
        mc_spine_client::sound_assets::decode_ogg_mono(&ogg, 1 << 20, 4_000_000).unwrap();
    assert_eq!(rate, 44_100);
    assert!(pcm.len() > 8_000, "decoded {} samples", pcm.len());
    let peak = pcm.iter().fold(0.0f32, |m, s| m.max(s.abs()));
    assert!(peak > 0.1, "decoded audio is non-silent (peak {peak})");
    // Resample path: 44.1k -> 48k linear.
    let rs = mc_spine_client::sound_assets::resample_linear(&pcm, 44_100, MIX_SAMPLE_RATE);
    assert!(rs.len() > pcm.len());
    let _ = std::fs::remove_file(&ogg);
}

// ---------------------------------------------------------------------------
// Mixer behavior
// ---------------------------------------------------------------------------

#[test]
fn mixer_pans_directionally_and_subtitles_follow() {
    // Listener at yaw 0 faces +Z (vanilla south): the right ear is -X, so a
    // source at x=-6 pans RIGHT and x=+6 pans LEFT.
    for (x, dir) in [
        (-6.0f32, SubtitleDirection::Right),
        (6.0, SubtitleDirection::Left),
    ] {
        let mut mixer = SoundMixer::new(MIX_SAMPLE_RATE, SpatialAudioPreset::Realistic);
        mixer.set_listener(ListenerPose {
            position: [0.0, 0.0, 0.0],
            yaw_deg: 0.0,
            pitch_deg: 0.0,
            velocity: [0.0; 3],
            submerged: false,
        });
        mixer.submit(sound_event("block.stone.break", 1.0, Some([x, 0.0, 0.0])));
        let mut out = vec![0.0f32; 960 * 2];
        mixer.render(&mut out);
        let (l, r) = (
            out.iter().step_by(2).map(|s| s * s).sum::<f32>(),
            out.iter().skip(1).step_by(2).map(|s| s * s).sum::<f32>(),
        );
        assert!(l > 0.0 && r > 0.0, "both channels energized");
        match dir {
            SubtitleDirection::Left => assert!(l > r, "source at +x should pan left: l={l} r={r}"),
            SubtitleDirection::Right => {
                assert!(r > l, "source at -x should pan right: l={l} r={r}")
            }
            _ => unreachable!(),
        }
        let sub = mixer.drain_subtitle().expect("subtitle cue emitted");
        assert_eq!(sub.name, "block.stone.break");
        assert_eq!(sub.cue.direction, dir);
    }
}

#[test]
fn mixer_caps_voices_and_steals_quietest_then_oldest() {
    let mut mixer = SoundMixer::new(MIX_SAMPLE_RATE, SpatialAudioPreset::Realistic);
    // Voice 0 is quiet; voices 1..=23 are loud; the cap is 24.
    mixer.submit(sound_event("s.quiet", 0.05, Some([5.0, 0.0, 0.0])));
    for i in 0..23 {
        mixer.submit(sound_event(
            &format!("s.loud{i}"),
            1.0,
            Some([5.0, 0.0, 0.0]),
        ));
    }
    let mut out = vec![0.0f32; 64 * 2];
    mixer.render(&mut out);
    assert_eq!(mixer.voice_count(), 24);
    // Admitting voice 25 steals "s.quiet" (lowest loudness).
    mixer.submit(sound_event("s.new", 1.0, Some([5.0, 0.0, 0.0])));
    mixer.render(&mut out);
    assert_eq!(mixer.voice_count(), 24);
    assert_eq!(mixer.stats.voices_stolen, 1);
    assert!(!mixer.voice_labels().iter().any(|l| l == "s.quiet"));
    // Equal loudness -> oldest stolen.
    mixer.submit(sound_event("s.new2", 1.0, Some([5.0, 0.0, 0.0])));
    mixer.render(&mut out);
    assert!(!mixer.voice_labels().iter().any(|l| l == "s.loud0"));
}

#[test]
fn mixer_stop_sound_filters_by_category_and_name() {
    let mut mixer = SoundMixer::new(MIX_SAMPLE_RATE, SpatialAudioPreset::Realistic);
    mixer.submit(sound_event("block.stone.break", 1.0, Some([5.0, 0.0, 0.0])));
    let mut ev = sound_event("entity.cow.milk", 1.0, Some([5.0, 0.0, 0.0]));
    ev.category = 7;
    mixer.submit(ev);
    let mut out = vec![0.0f32; 64 * 2];
    mixer.render(&mut out);
    assert_eq!(mixer.voice_count(), 2);
    mixer.submit_stop(None, Some("block.stone.break"));
    assert_eq!(mixer.voice_count(), 1);
    assert_eq!(mixer.voice_labels(), vec!["entity.cow.milk".to_string()]);
    mixer.submit_stop(Some(7), None);
    assert_eq!(mixer.voice_count(), 0);
    assert_eq!(mixer.stats.voices_stopped, 2);
}

#[test]
fn mixer_output_is_bounded_and_deterministic() {
    let render = || {
        let mut mixer = SoundMixer::new(MIX_SAMPLE_RATE, SpatialAudioPreset::Realistic);
        mixer.set_listener(ListenerPose {
            position: [0.0, 0.0, 0.0],
            yaw_deg: 0.0,
            pitch_deg: 0.0,
            velocity: [0.0; 3],
            submerged: false,
        });
        for i in 0..6 {
            mixer.submit(sound_event(
                &format!("test.sound{i}"),
                1.0,
                Some([i as f32 - 3.0, 0.0, 2.0]),
            ));
        }
        let mut out = vec![0.0f32; 2048 * 2];
        mixer.render(&mut out);
        out
    };
    let a = render();
    let b = render();
    assert_eq!(a, b, "fixed inputs -> identical PCM");
    let peak = a.iter().fold(0.0f32, |m, s| m.max(s.abs()));
    assert!(
        peak <= 1.0,
        "bus limiter caps at max_volume_cap: peak {peak}"
    );
    assert!(peak > 0.01, "mix is non-silent");
}

#[test]
fn mixer_zero_distance_source_is_finite() {
    let mut mixer = SoundMixer::new(MIX_SAMPLE_RATE, SpatialAudioPreset::Realistic);
    mixer.set_listener(ListenerPose {
        position: [4.0, 4.0, 4.0],
        yaw_deg: 0.0,
        pitch_deg: 0.0,
        velocity: [0.0; 3],
        submerged: false,
    });
    mixer.submit(sound_event("test.here", 1.0, Some([4.0, 4.0, 4.0])));
    let mut out = vec![0.0f32; 960 * 2];
    mixer.render(&mut out);
    assert!(
        out.iter().all(|s| s.is_finite()),
        "zero-distance produced non-finite output"
    );
    let sub = mixer.drain_subtitle().unwrap();
    assert!(sub.cue.loudness.is_finite());
}

// ---------------------------------------------------------------------------
// Sink + CLI surface
// ---------------------------------------------------------------------------

#[test]
fn wav_sink_produces_parseable_non_silent_file() {
    let path = std::env::temp_dir().join(format!("mc-sink-test-{}.wav", std::process::id()));
    let mut pcm = Vec::new();
    for i in 0..4800i32 {
        let t = i as f32 / 48000.0;
        let s = (t * 440.0 * 2.0 * std::f32::consts::PI).sin();
        pcm.push((s * 0.5 * 32767.0) as i16);
        pcm.push((s * 0.25 * 32767.0) as i16);
    }
    {
        let (mut sink, warn) = open_sink(&AudioSinkKind::Wav(path.clone()));
        assert!(warn.is_none());
        assert_eq!(sink.label(), "wav");
        sink.push(&pcm).unwrap();
        sink.finish();
    }
    let bytes = std::fs::read(&path).unwrap();
    assert_eq!(&bytes[0..4], b"RIFF");
    assert_eq!(&bytes[8..12], b"WAVE");
    assert_eq!(
        u32::from_le_bytes(bytes[24..28].try_into().unwrap()),
        48_000
    );
    let data = u32::from_le_bytes(bytes[40..44].try_into().unwrap()) as usize;
    assert_eq!(data, pcm.len() * 2);
    assert!(bytes[44..].iter().any(|b| *b != 0), "WAV data non-silent");
    let _ = std::fs::remove_file(&path);
}

#[test]
fn cli_parses_audio_flags() {
    let cfg = parse_args(&["--audio-sink", "null", "--server", "localhost"]).unwrap();
    assert_eq!(cfg.audio_sink, AudioSinkKind::Null);
    let cfg = parse_args(&["--audio-wav=/tmp/out.wav"]).unwrap();
    assert_eq!(
        cfg.audio_sink,
        AudioSinkKind::Wav(PathBuf::from("/tmp/out.wav"))
    );
    assert!(parse_args(&["--audio-sink", "bogus"]).is_err());
    assert_eq!(
        parse_args::<&str>(&[]).unwrap().audio_sink,
        AudioSinkKind::Auto
    );
}

/// Guard: the default WAV writer used by `open_sink` writes via
/// `BufWriter`; ensure `finish` is idempotent (double-finish safe).
#[test]
fn wav_sink_finish_is_idempotent() {
    let path = std::env::temp_dir().join(format!("mc-sink-idem-{}.wav", std::process::id()));
    {
        let (mut sink, _) = open_sink(&AudioSinkKind::Wav(path.clone()));
        sink.push(&[1, -1, 0]).unwrap();
        sink.finish();
        sink.finish();
    }
    assert!(std::fs::metadata(&path).unwrap().len() >= 44 + 6);
    let _ = std::fs::remove_file(&path);
}

/// `Path` import used by fixture helpers — keeps `cargo test` honest when
/// the module set changes.
#[allow(dead_code)]
fn _unused(_: &Path) {}
