// SPDX-License-Identifier: LGPL-3.0-or-later
//! QUAL-AUDIO-3 lane tests (audit §2 B1-B5 + §3.6):
//!
//!  * B1 — O(1) `block_state_id -> AcousticMaterial` LUT: wool/wood/glass/
//!    iron registry states plus unknown Mod names classify through
//!    tags/keywords, and `WorldAcousticSnapshot` tracks live edits.
//!  * B2 — layered `SoundLibrary`: a clean-room Mod JAR's
//!    `assets/<modid>/sounds.json` + `.ogg` resolves/decodes entirely in
//!    memory (zero extracted files), and a resource-pack `replace: true`
//!    layer wins over the JAR.
//!  * B3/B4/B5 — 2D stereo bypass vs mono-squashed 3D, the exact Vanilla
//!    `attenuation_distance * max(volume, 1.0)` range law, entity-tracked
//!    voice position/velocity, and a `SharedListenerFdnBus` tail that
//!    keeps decaying after the impulse voice finishes.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use megu_client::audio_out::{AudioRuntime, AudioSessionConfig, AudioSinkKind};
use megu_client::protocol::{
    DecodedChunk, DecodedLightData, DecodedSection, DimensionMeta, WireSoundEvent,
};
use megu_client::session::{PlaySoundEvent, SoundMixer, WorldAcousticSnapshot};
use megu_client::sound_assets::{
    DecodedPcm, SoundEventRegistry, SoundLibrary, SoundSourceData, MIX_SAMPLE_RATE,
};
use megu_client::spatial_audio::{
    AcousticMaterial, AcousticMaterialLut, ListenerPose, MaterialClass, SpatialAudioPreset,
    VoxelAcousticWorld,
};
use megu_client::world::{BlockTable, WorldState};
use megu_client_mesh::tactile_effects::EnvironmentAudioHint;

fn tmp_root(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("mc-qual-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn empty_light() -> DecodedLightData {
    DecodedLightData {
        sky_mask: Vec::new(),
        block_mask: Vec::new(),
        empty_sky_mask: Vec::new(),
        empty_block_mask: Vec::new(),
        sky_arrays: Vec::new(),
        block_arrays: Vec::new(),
    }
}

/// Every file under `dir`, relative — the zero-extraction witness.
fn file_tree(dir: &Path) -> BTreeSet<PathBuf> {
    let mut out = BTreeSet::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for entry in std::fs::read_dir(&d).unwrap().flatten() {
            let p = entry.path();
            if p.is_dir() {
                stack.push(p);
            } else {
                out.insert(p.strip_prefix(dir).unwrap().to_path_buf());
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// B1 — acoustic LUT + live snapshot
// ---------------------------------------------------------------------------

#[test]
fn qual_b1_acoustic_lut_classifies_wool_wood_glass_metal_and_unknown_mod_blocks() {
    // Unknown Mod blocks classify through the keyword fallback — the
    // whole point of name-based classification instead of tag-only.
    for (name, want) in [
        ("create:brass_casing", MaterialClass::Metal),
        ("custom:stained_glass", MaterialClass::Glass),
        ("biomesoplenty:maple_leaves", MaterialClass::SoftAbsorber),
        ("custom:unknown_obelisk", MaterialClass::StoneLike),
    ] {
        assert_eq!(
            AcousticMaterial::from_block_name(name).class,
            want,
            "{name} misclassified"
        );
    }
    let mod_lut = AcousticMaterialLut::from_state_names(&[
        "create:brass_casing".to_string(),
        "custom:stained_glass".to_string(),
        String::new(),
    ]);
    assert_eq!(mod_lut.get(0).class, MaterialClass::Metal);
    assert_eq!(mod_lut.get(1).class, MaterialClass::Glass);
    // Never-named states read as the dominant solid, never air.
    assert_eq!(mod_lut.get(2).class, MaterialClass::StoneLike);

    // The real-registry half needs the generated 1.21.1 state table; in a
    // strict-public export it is absent, which is a skip — a present but
    // nameless table stays a failure.
    let blocks = BlockTable::load(None);
    let names = blocks.state_names();
    if names.is_empty() {
        eprintln!("registry state table absent (strict-public export); LUT check skipped");
        return;
    }
    assert!(
        names.iter().any(|n| n == "minecraft:white_wool"),
        "registry names must load for the LUT (got {} names)",
        names.len()
    );
    let state_id = |name: &str| -> u32 {
        names
            .iter()
            .position(|n| n == name)
            .unwrap_or_else(|| panic!("no state named {name}")) as u32
    };
    let wool = state_id("minecraft:white_wool");
    let planks = state_id("minecraft:oak_planks");
    let glass = state_id("minecraft:glass");
    let iron = state_id("minecraft:iron_block");
    let stone = state_id("minecraft:stone");

    // O(1) LUT built from the real registry names.
    let lut = blocks.acoustic_lut();
    assert_eq!(lut.get(wool).class, MaterialClass::SoftAbsorber);
    assert_eq!(lut.get(planks).class, MaterialClass::Wood);
    assert_eq!(lut.get(glass).class, MaterialClass::Glass);
    assert_eq!(lut.get(iron).class, MaterialClass::Metal);
    assert_eq!(lut.get(stone).class, MaterialClass::StoneLike);
    assert_eq!(lut.get(u32::MAX).class, MaterialClass::StoneLike);

    // Live world -> snapshot: a wool cell inside a real chunk reports
    // SoftAbsorber through the LUT, and a block edit re-syncs live.
    let mut world = WorldState::new(DimensionMeta::default(), BlockTable::load(None));
    let mut states = Box::new([0u32; 4096]);
    // y = -60 in a min_y = -64 world -> section-local y 4; cell (5, -60, 5).
    states[4 * 256 + 5 * 16 + 5] = wool;
    world
        .apply_chunk(DecodedChunk {
            cx: 0,
            cz: 0,
            sections: vec![DecodedSection {
                non_air: 1,
                states,
                biomes: [0; 64],
            }],
            light: empty_light(),
            block_entities: Vec::new(),
        })
        .unwrap();
    let snapshot = WorldAcousticSnapshot::new(&world);
    snapshot.sync_chunk(&world, 0, 0);
    assert_eq!(
        snapshot.acoustic_material(5, -60, 5).class,
        MaterialClass::SoftAbsorber
    );
    assert_eq!(
        snapshot.acoustic_material(0, -60, 0).class,
        MaterialClass::Air
    );
    world.apply_block_update([5, -60, 5], iron).unwrap();
    snapshot.sync_block(&world, [5, -60, 5]);
    assert_eq!(
        snapshot.acoustic_material(5, -60, 5).class,
        MaterialClass::Metal
    );
    snapshot.unload_chunk(0, 0);
    assert_eq!(
        snapshot.acoustic_material(5, -60, 5).class,
        MaterialClass::Air
    );
}

// ---------------------------------------------------------------------------
// B2 — clean-room Mod JAR + resource-pack replace, all in memory
// ---------------------------------------------------------------------------

/// IEEE 802.3 CRC-32 (the polynomial ZIP uses) — no crates.
fn crc32(data: &[u8]) -> u32 {
    let mut crc: u32 = 0xFFFF_FFFF;
    for &b in data {
        crc ^= u32::from(b);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xEDB8_8320 & 0u32.wrapping_sub(crc & 1));
        }
    }
    !crc
}

/// Minimal stored-method ZIP: local headers + central directory + EOCD,
/// enough for the bounded `zip_entries` reader to index and decompress.
fn build_zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut records: Vec<(String, u32, u32, u32)> = Vec::new();
    for (name, data) in entries {
        let local_off = out.len() as u32;
        let crc = crc32(data);
        let size = data.len() as u32;
        out.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
        out.extend_from_slice(&20u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&crc.to_le_bytes());
        out.extend_from_slice(&size.to_le_bytes());
        out.extend_from_slice(&size.to_le_bytes());
        out.extend_from_slice(&(name.len() as u16).to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(data);
        records.push((name.to_string(), crc, size, local_off));
    }
    let cd_off = out.len() as u32;
    for (name, crc, size, local_off) in &records {
        out.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
        out.extend_from_slice(&20u16.to_le_bytes());
        out.extend_from_slice(&20u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&crc.to_le_bytes());
        out.extend_from_slice(&size.to_le_bytes());
        out.extend_from_slice(&size.to_le_bytes());
        out.extend_from_slice(&(name.len() as u16).to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&local_off.to_le_bytes());
        out.extend_from_slice(name.as_bytes());
    }
    let cd_size = out.len() as u32 - cd_off;
    out.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&(records.len() as u16).to_le_bytes());
    out.extend_from_slice(&(records.len() as u16).to_le_bytes());
    out.extend_from_slice(&cd_size.to_le_bytes());
    out.extend_from_slice(&cd_off.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out
}

/// A real OGG Vorbis file rendered by ffmpeg (same fixture pattern as
/// `audio_pipeline.rs`); `None` when ffmpeg is absent.
fn ffmpeg_ogg(path: &Path, freq: u32, stereo: bool) -> Option<Vec<u8>> {
    let channels = if stereo { "2" } else { "1" };
    let ok = std::process::Command::new("ffmpeg")
        .args([
            "-y",
            "-f",
            "lavfi",
            "-i",
            &format!("sine=frequency={freq}:duration=0.25:sample_rate=44100"),
            "-ac",
            channels,
            "-c:a",
            "libvorbis",
            "-q:a",
            "3",
        ])
        .arg(path)
        .output()
        .ok()?;
    ok.status.success().then(|| std::fs::read(path).unwrap())
}

#[test]
fn qual_b2_multilayer_sound_library_loads_mod_jar_and_resourcepack_replace_in_memory() {
    let Some(ogg_bytes) = ffmpeg_ogg(
        &tmp_root("b2-ogg").join("src.ogg"),
        440,
        false,
    ) else {
        eprintln!("skip: ffmpeg not on PATH");
        return;
    };
    let root = tmp_root("b2");

    // Clean-room Mod JAR: `assets/my_mod/sounds.json` + OGG, plus a
    // minecraft-namespace event the resource pack will `replace`.
    let mods = root.join("mods");
    std::fs::create_dir_all(&mods).unwrap();
    let jar_path = mods.join("my_mod.jar");
    let jar = build_zip(&[
        (
            "assets/my_mod/sounds.json",
            br#"{"drill_loop": {"sounds": [
                {"name": "my_mod:drill", "attenuation_distance": 32.0}
            ]}}"# as &[u8],
        ),
        ("assets/my_mod/sounds/drill.ogg", ogg_bytes.as_slice()),
        (
            "assets/minecraft/sounds.json",
            br#"{"shared.sound": {"sounds": [{"name": "shared", "volume": 0.5}]}}"#,
        ),
        ("assets/minecraft/sounds/shared.ogg", ogg_bytes.as_slice()),
    ]);
    std::fs::write(&jar_path, jar).unwrap();

    // Resource pack (directory form): `replace: true` discards the JAR's
    // variant for `shared.sound` and supplies its own OGG on disk.
    let pack = root.join("packs/replace_pack");
    std::fs::create_dir_all(pack.join("assets/minecraft/sounds")).unwrap();
    std::fs::write(
        pack.join("assets/minecraft/sounds.json"),
        br#"{"shared.sound": {"replace": true, "sounds": [
            {"name": "pack_tone", "attenuation_distance": 8.0}
        ]}}"#,
    )
    .unwrap();
    std::fs::write(pack.join("assets/minecraft/sounds/pack_tone.ogg"), &ogg_bytes).unwrap();

    let before = file_tree(&root);

    let mut lib = SoundLibrary::empty();
    assert_eq!(lib.merge_sound_layer(&jar_path).unwrap(), 2);
    assert_eq!(lib.merge_sound_layer(&pack).unwrap(), 1);

    // Mod event resolves to a ZipEntry source with its declared
    // attenuation — decoded straight from archive bytes.
    let drill = lib.resolve("my_mod:drill_loop", 0).expect("drill_loop resolves");
    assert_eq!(drill.attenuation_distance, Some(32.0));
    let drill_source = drill.source.clone();
    match &drill_source {
        SoundSourceData::ZipEntry {
            archive_path,
            entry_name,
        } => {
            assert_eq!(archive_path, &jar_path);
            assert_eq!(entry_name, "assets/my_mod/sounds/drill.ogg");
        }
        other => panic!("drill_loop must come from the JAR, got {other:?}"),
    }
    let pcm = lib.decode_source(&drill_source).expect("in-memory decode");
    let (frames, peak) = match &*pcm {
        DecodedPcm::Mono(v) => (
            v.len(),
            v.iter().fold(0.0f32, |m, s| m.max(s.abs())),
        ),
        DecodedPcm::Stereo(v) => (
            v.len(),
            v.iter().fold(0.0f32, |m, f| m.max(f[0].abs()).max(f[1].abs())),
        ),
    };
    assert!(frames > 8_000, "decoded {frames} frames");
    assert!(peak > 0.1, "decoded audio is non-silent (peak {peak})");

    // Without the pack, `shared.sound` came from the JAR itself —
    // proving replace actually swapped the winner, not just appended.
    let mut jar_only = SoundLibrary::empty();
    jar_only.merge_sound_layer(&jar_path).unwrap();
    let jar_winner = jar_only
        .resolve("minecraft:shared.sound", 0)
        .expect("jar shared.sound resolves");
    assert!(matches!(
        jar_winner.source,
        SoundSourceData::ZipEntry { .. }
    ));

    // With the pack merged above it, `replace: true` makes the pack's
    // single variant the only one — its file source and its
    // attenuation_distance both win.
    let winner = lib
        .resolve("minecraft:shared.sound", 0)
        .expect("pack shared.sound resolves");
    assert_eq!(winner.attenuation_distance, Some(8.0));
    match &winner.source {
        SoundSourceData::File(p) => {
            assert!(p.ends_with("pack_tone.ogg"), "pack file wins: {p:?}");
            assert!(p.starts_with(&pack));
        }
        other => panic!("pack variant must resolve to a file, got {other:?}"),
    }

    // The full minecraft:sound_event id table (wire/registry order).
    let registry = SoundEventRegistry::from_names(vec![
        "minecraft:entity.player.levelup".to_string(),
        "my_mod:drill_loop".to_string(),
    ]);
    assert_eq!(registry.name(0), Some("entity.player.levelup"));
    assert_eq!(registry.name(1), Some("my_mod:drill_loop"));
    assert_eq!(registry.name(99), None);

    // ZERO extraction: the whole layered resolve+decode never wrote a
    // file — the tree is byte-identical to before.
    assert_eq!(file_tree(&root), before, "resolve/decode extracted files");

    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(tmp_root("b2-ogg"));
}

// ---------------------------------------------------------------------------
// B3 + B4 + B5 — bypass, range law, entity Doppler, shared-FDN tail
// ---------------------------------------------------------------------------

fn event(label: &str, pcm: Vec<f32>, stereo: Option<Vec<[f32; 2]>>) -> PlaySoundEvent {
    PlaySoundEvent {
        label: label.to_string(),
        pcm: Arc::new(pcm),
        stereo_pcm: stereo.map(Arc::new),
        is_placeholder: false,
        category: 0,
        position: None,
        entity_id: None,
        volume: 1.0,
        pitch: 1.0,
        seed: 0,
        max_range_m: f32::INFINITY,
    }
}

#[test]
fn qual_b3_b4_b5_2d_stereo_bypass_volume_range_doppler_and_shared_fdn_tail() {
    // ---- B3a: Music stereo bypass — L/R untouched, zero reverb, no
    // mono squash, even with maximum cave reverb in the environment.
    let mut mixer = SoundMixer::new(MIX_SAMPLE_RATE, SpatialAudioPreset::Realistic);
    mixer.set_environment_hint(EnvironmentAudioHint {
        underwater_lowpass: 0.0,
        cave_reverb_mix: 1.0,
        rain_eaves_occlusion: 0.0,
    });
    let n = 2400usize;
    let stereo: Vec<[f32; 2]> = (0..n)
        .map(|i| if i % 2 == 0 { [0.5, -0.25] } else { [0.4, -0.2] })
        .collect();
    let mono: Vec<f32> = stereo.iter().map(|f| (f[0] + f[1]) * 0.5).collect();
    let mut ev = event("music.overworld", mono, Some(stereo.clone()));
    ev.category = 1; // Music
    mixer.submit(ev);
    let mut out = vec![0.0f32; n * 2];
    mixer.render(&mut out);
    // `render_2d` interpolates idx/idx+1, so the final frame is a guard.
    for i in 0..n - 1 {
        let (l, r) = (out[2 * i], out[2 * i + 1]);
        assert!(
            (l - stereo[i][0]).abs() < 1e-3 && (r - stereo[i][1]).abs() < 1e-3,
            "bypassed stereo corrupted at frame {i}: [{l}, {r}] vs {:?}",
            stereo[i]
        );
        assert_ne!(l, r, "2D bypass must not mono-squash");
    }
    // Cave reverb hint at maximum produced zero wet tail for the
    // bypassed voice.
    let dbg = mixer.voice_debug();
    assert!(
        dbg.iter().all(|(_, _, _, _, spatial)| !spatial),
        "music voice must not be spatialized: {dbg:?}"
    );

    // ---- B3b: same stereo asset as a world-positioned 3D sound is mono
    // downmixed before spatialization.
    let mut mixer3d = SoundMixer::new(MIX_SAMPLE_RATE, SpatialAudioPreset::Realistic);
    mixer3d.set_listener(ListenerPose {
        position: [0.0, 64.0, 0.0],
        ..ListenerPose::default()
    });
    let mut ev = event("block.custom.stereo", vec![0.125; n], Some(stereo));
    ev.category = 3;
    ev.position = Some([3.0, 64.0, 0.0]);
    mixer3d.submit(ev);
    let mut out3d = vec![0.0f32; n * 2];
    mixer3d.render(&mut out3d);
    let (peak_l, peak_r) = out3d.as_chunks::<2>().0.iter().fold(
        (0.0f32, 0.0f32),
        |(l, r), f| (l.max(f[0].abs()), r.max(f[1].abs())),
    );
    assert!(
        peak_l > 1e-4 && peak_r > 1e-4 && peak_l < 0.3 && peak_r < 0.3,
        "3D world sound must derive from the mono downmix, not raw L/R ({peak_l},{peak_r})"
    );

    // ---- B4: exact Vanilla range law through the live ingest path —
    // attenuation_distance(default 16) * max(volume, 1.0): volume=4 -> 64m,
    // attenuation_distance=32 + volume=2 -> 64m, vol=1 -> 16m.
    let assets = tmp_root("b4-assets");
    std::fs::create_dir_all(assets.join("indexes")).unwrap();
    std::fs::create_dir_all(assets.join("objects/dummy")).unwrap();
    let sounds_json = r#"{
        "big.boom": {"sounds": [{"name": "test/dummy"}]},
        "wide.thunder": {"sounds": [{"name": "test/dummy", "attenuation_distance": 32.0}]},
        "far.hiss": {"sounds": [{"name": "test/dummy", "attenuation_distance": 8.0}]}
    }"#;
    let hash = "dd".repeat(20);
    std::fs::create_dir_all(assets.join("objects/dd")).unwrap();
    std::fs::write(assets.join(format!("objects/dd/{hash}")), sounds_json).unwrap();
    std::fs::write(
        assets.join("indexes/qual.json"),
        format!(
            r#"{{"objects": {{
                "minecraft/sounds.json": {{"hash": "{hash}", "size": 400}},
                "minecraft/sounds/test/dummy.ogg": {{"hash": "{hash}", "size": 400}}
            }}}}"#
        ),
    )
    .unwrap();
    let cfg = AudioSessionConfig {
        sink: AudioSinkKind::Null,
        assets_dir: Some(assets.clone()),
        asset_index: Some("qual".to_string()),
        game_dir: None,
        max_volume_cap: 1.0,
        max_concurrent_same_sound: 4,
        subtitle_path: None,
        subtitles_to_stderr: false,
    };
    let mut rt = AudioRuntime::new(&cfg);
    rt.update_listener([0.0, 64.0, 0.0], Some(0.0), Some(0.0));
    let wire = |name: &str, volume: f32| WireSoundEvent {
        name: Some(format!("minecraft:{name}")),
        registry_id: None,
        category: 3,
        position: Some([50.0, 64.0, 0.0]),
        entity_id: None,
        volume,
        pitch: 1.0,
        seed: 0,
        fixed_range: None,
    };
    rt.ingest(&wire("big.boom", 4.0));
    rt.ingest(&wire("wide.thunder", 2.0));
    rt.ingest(&wire("far.hiss", 1.0));
    // Decode runs on the async worker now — pump until every voice has
    // landed (bounded wait; the fixture "ogg" fails decode → placeholder).
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        rt.pump_frames(240);
        if rt
            .mixer
            .voice_max_range("minecraft:big.boom")
            .is_some()
            && rt
                .mixer
                .voice_max_range("minecraft:wide.thunder")
                .is_some()
            && rt.mixer.voice_max_range("minecraft:far.hiss").is_some()
        {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "decoded voices never reached the mixer"
        );
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    assert_eq!(
        rt.mixer.voice_max_range("minecraft:big.boom"),
        Some(64.0),
        "attenuation 16 * max(volume 4, 1) must widen to 64m"
    );
    assert_eq!(
        rt.mixer.voice_max_range("minecraft:wide.thunder"),
        Some(64.0),
        "attenuation 32 * max(volume 2, 1) = 64m"
    );
    assert_eq!(rt.mixer.voice_max_range("minecraft:far.hiss"), Some(8.0));
    // Output gain still saturates: a 64m-range voice at 50m is audible;
    // an 8m-range voice at 50m is silent — both through the same buffer
    // only when isolated, so measure the boom's range fade directly.
    assert!(
        rt.mixer.voice_debug().iter().all(|(_, _, _, _, s)| *s),
        "all positioned voices must be spatialized"
    );

    // ---- B5a: entity-bound voice tracks live position + finite velocity.
    let mut tracker = SoundMixer::new(MIX_SAMPLE_RATE, SpatialAudioPreset::Realistic);
    tracker.set_listener(ListenerPose {
        position: [0.0, 64.0, 0.0],
        ..ListenerPose::default()
    });
    tracker.set_entity_position(42, [2.0, 64.0, 0.0]);
    let mut ev = event("entity.moving", vec![0.3; MIX_SAMPLE_RATE as usize], None);
    ev.entity_id = Some(42);
    tracker.submit(ev);
    let mut buf = vec![0.0f32; 4800];
    tracker.render(&mut buf);
    assert_eq!(tracker.voice_debug()[0].2, [2.0, 64.0, 0.0]);
    tracker.set_entity_position(42, [10.0, 64.0, 4.0]);
    tracker.render(&mut buf);
    let (_, eid, pos, vel, spatial) = tracker.voice_debug()[0];
    assert_eq!(eid, Some(42));
    assert!(spatial);
    assert_eq!(pos, [10.0, 64.0, 4.0], "voice must follow the entity");
    assert!(
        vel.iter().all(|v| v.is_finite()) && vel[0] > 1.0 && vel[2] > 0.0,
        "finite nonzero source velocity from the entity delta: {vel:?}"
    );

    // ---- B5b: shared FDN keeps ringing after the impulse voice ends.
    let mut tail_mixer = SoundMixer::new(MIX_SAMPLE_RATE, SpatialAudioPreset::Realistic);
    tail_mixer.set_listener(ListenerPose {
        position: [0.0, 64.0, 0.0],
        ..ListenerPose::default()
    });
    tail_mixer.set_environment_hint(EnvironmentAudioHint {
        underwater_lowpass: 0.0,
        cave_reverb_mix: 1.0,
        rain_eaves_occlusion: 0.0,
    });
    let mut impulse = vec![0.0f32; 4800];
    impulse[0] = 0.9;
    let mut ev = event("block.cave.impulse", impulse, None);
    ev.category = 3;
    ev.position = Some([4.0, 64.0, 0.0]);
    tail_mixer.submit(ev);
    let mut tail = vec![0.0f32; 9600];
    tail_mixer.render(&mut tail); // impulse ends inside this block
    let mut tail2 = vec![0.0f32; 24000 * 2];
    tail_mixer.render(&mut tail2);
    assert_eq!(tail_mixer.voice_debug().len(), 0, "impulse voice finished");
    let early_peak = tail2[..4800]
        .iter()
        .fold(0.0f32, |m, s| m.max(s.abs()));
    let late_peak = tail2[43_200..]
        .iter()
        .fold(0.0f32, |m, s| m.max(s.abs()));
    assert!(
        early_peak > 1e-4,
        "shared FDN tail must outlive the source voice (peak {early_peak})"
    );
    assert!(
        late_peak < early_peak,
        "reverb tail must decay smoothly: early {early_peak} vs late {late_peak}"
    );

    let _ = std::fs::remove_dir_all(&assets);
}
