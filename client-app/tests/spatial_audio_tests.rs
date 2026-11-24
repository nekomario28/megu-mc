// SPDX-License-Identifier: LGPL-3.0-or-later
//! Tests for the raytraced spatial audio engine: preset tiers, material
//! classification, DDA occlusion/diffraction, Eyring reverb estimates,
//! binaural ITD/ILD, underwater muffle, voice-frame plumbing, and subtitle
//! cues.

use megu_client::spatial_audio::{
    AcousticMaterial, AcousticTracer, DenseVoxelGrid, FreqBand, ListenerPose, MaterialClass,
    SoundSource, SpatialAudioEngine, SpatialAudioPreset, SubtitleDirection, OUTPUT_LIMIT,
};

const FS: u32 = 48_000;

fn sine(freq: f32, n: usize) -> Vec<f32> {
    (0..n)
        .map(|i| (2.0 * std::f32::consts::PI * freq * i as f32 / FS as f32).sin() * 0.5)
        .collect()
}

fn impulse(n: usize) -> Vec<f32> {
    let mut v = vec![0.0; n];
    v[0] = 0.8;
    v
}

fn rms(v: &[f32]) -> f32 {
    (v.iter().map(|x| x * x).sum::<f32>() / v.len().max(1) as f32).sqrt()
}

fn stereo_l(out: &[f32]) -> Vec<f32> {
    out.iter().step_by(2).copied().collect()
}

fn stereo_r(out: &[f32]) -> Vec<f32> {
    out.iter().skip(1).step_by(2).copied().collect()
}

fn assert_all_finite_bounded(out: &[f32]) {
    for (i, s) in out.iter().enumerate() {
        assert!(s.is_finite(), "sample {i} non-finite: {s}");
        assert!(
            s.abs() <= OUTPUT_LIMIT + 1e-6,
            "sample {i} = {s} over limit"
        );
    }
}

fn pose(x: f32, y: f32, z: f32, yaw: f32) -> ListenerPose {
    ListenerPose {
        position: [x, y, z],
        yaw_deg: yaw,
        ..ListenerPose::default()
    }
}

fn src(x: f32, y: f32, z: f32) -> SoundSource {
    SoundSource {
        position: [x, y, z],
        ..SoundSource::default()
    }
}

/// Sealed room: `material` shell everywhere, `interior_min..=interior_max`
/// carved back to air. Rays that leave the interior strike the shell.
fn sealed_room(material: AcousticMaterial) -> DenseVoxelGrid {
    let mut g = DenseVoxelGrid::new([-12, -8, -12], [24, 16, 24], material);
    g.fill_box([-7, -3, -7], [6, 3, 6], AcousticMaterial::AIR);
    g
}

/// Open-air world: earth ground slab below, infinite air above.
fn outdoor() -> DenseVoxelGrid {
    let mut g = DenseVoxelGrid::new([-24, -16, -24], [48, 32, 48], AcousticMaterial::AIR);
    g.fill_box(
        [-24, -16, -24],
        [23, -2, 23],
        AcousticMaterial::from_class(MaterialClass::Earth),
    );
    g
}

/// Stone wall slab across x∈[-12..11], y∈[-8..11], z∈[2..5] (4 thick) — wide
/// and tall enough that diffraction probes cannot detour around its ends, so
/// the `door` variant's portal (x∈[2..5], y∈[-1..1]) is the only way through.
fn wall_world(door: bool) -> DenseVoxelGrid {
    let mut g = DenseVoxelGrid::new([-24, -16, -24], [48, 32, 48], AcousticMaterial::AIR);
    g.fill_box(
        [-12, -8, 2],
        [11, 11, 5],
        AcousticMaterial::from_class(MaterialClass::StoneLike),
    );
    if door {
        g.fill_box([2, -1, 2], [5, 1, 5], AcousticMaterial::AIR);
    }
    g
}

#[test]
fn off_preset_is_bit_exact_passthrough_with_zero_raycasts() {
    let world = sealed_room(AcousticMaterial::from_class(MaterialClass::StoneLike));
    let mut engine = SpatialAudioEngine::new(FS, SpatialAudioPreset::Off);
    let scene = engine.analyze(&world, &src(0.5, 0.5, -2.5), &pose(0.5, 0.5, 2.5, 0.0));
    assert_eq!(scene.rays_traced, 0);

    let input: Vec<f32> = (0..960)
        .map(|i| (i as f32 * 0.01).sin() * 0.5 - 0.25)
        .collect();
    let out = engine.process_voice_frame(&input);
    assert_eq!(out.len(), input.len() * 2);
    for (i, &s) in input.iter().enumerate() {
        assert_eq!(out[2 * i], s, "left sample {i} not bit-exact");
        assert_eq!(out[2 * i + 1], s, "right sample {i} not bit-exact");
    }
}

#[test]
fn material_mapper_routes_block_families() {
    let stone = AcousticMaterial::from_block_name("minecraft:deepslate");
    assert_eq!(stone.class, MaterialClass::StoneLike);
    assert!(stone.absorption(FreqBand::Mid) < 0.10);
    assert_eq!(
        AcousticMaterial::from_block_name("minecraft:iron_block").class,
        MaterialClass::Metal
    );
    assert_eq!(
        AcousticMaterial::from_block_name("minecraft:glass").class,
        MaterialClass::Glass
    );
    assert_eq!(
        AcousticMaterial::from_block_name("oak_planks").class,
        MaterialClass::Wood
    );
    assert_eq!(
        AcousticMaterial::from_block_name("minecraft:dirt").class,
        MaterialClass::Earth
    );
    let wool = AcousticMaterial::from_block_name("minecraft:white_wool");
    assert_eq!(wool.class, MaterialClass::SoftAbsorber);
    assert!(wool.absorption(FreqBand::High) > 0.9);
    assert_eq!(
        AcousticMaterial::from_block_name("minecraft:water").class,
        MaterialClass::Liquid
    );
    // Tricky substrings that must not be misrouted.
    assert_eq!(
        AcousticMaterial::from_block_name("minecraft:bedrock").class,
        MaterialClass::StoneLike,
        "bedrock contains 'bed' but is stone"
    );
    assert_eq!(
        AcousticMaterial::from_block_name("minecraft:sandstone").class,
        MaterialClass::StoneLike,
        "sandstone contains 'sand' but is stone"
    );
    assert_eq!(
        AcousticMaterial::from_block_name("minecraft:air").class,
        MaterialClass::Air
    );
}

#[test]
fn outdoor_scene_is_open_dry_and_crisp() {
    let world = outdoor();
    let tracer = AcousticTracer::new(&world, SpatialAudioPreset::Ultra);
    let scene = tracer.compute_scene([0.5, 0.5, 0.5], [0.5, 0.5, 6.5], FS);
    assert!(
        scene.outdoor_openness > 0.7,
        "open field openness {} <= 0.7",
        scene.outdoor_openness
    );
    assert!(
        scene.rt60_mid_s < 0.3,
        "open field rt60 {} should be tiny",
        scene.rt60_mid_s
    );
    assert!(
        scene.wet_gain < 0.15,
        "wet {} should be near zero",
        scene.wet_gain
    );
    assert_eq!(scene.occlusion.transmission_loss_db, 0.0);

    // An impulse must stay a clean impulse: almost no tail energy.
    let mut engine = SpatialAudioEngine::new(FS, SpatialAudioPreset::Ultra);
    engine.analyze(&world, &src(0.5, 0.5, 0.5), &pose(0.5, 0.5, 6.5, 0.0));
    let mut out = Vec::new();
    engine.process(&impulse(4096), &mut out);
    let direct_peak = out.iter().fold(0.0_f32, |a, b| a.max(b.abs()));
    // The direct impulse lands ~840 samples out (6 m at 343 m/s), so the tail
    // check looks at the final quarter, well past direct + reflections.
    let tail = rms(&out[out.len() * 3 / 4..]);
    assert!(direct_peak > 0.05, "direct path too quiet: {direct_peak}");
    assert!(
        tail < direct_peak * 0.05,
        "outdoor tail {tail} should be far under direct {direct_peak}"
    );
}

#[test]
fn stone_cave_out_reverbs_wool_room() {
    let listener = pose(0.5, 0.5, 4.5, 0.0);
    let source = src(0.5, 0.5, -3.5);
    let stone_world = sealed_room(AcousticMaterial::from_class(MaterialClass::StoneLike));
    let wool_world = sealed_room(AcousticMaterial::from_class(MaterialClass::SoftAbsorber));

    let stone_scene = AcousticTracer::new(&stone_world, SpatialAudioPreset::Ultra).compute_scene(
        source.position,
        listener.position,
        FS,
    );
    let wool_scene = AcousticTracer::new(&wool_world, SpatialAudioPreset::Ultra).compute_scene(
        source.position,
        listener.position,
        FS,
    );

    assert!(
        stone_scene.outdoor_openness < 0.05,
        "sealed cave should trap rays: {}",
        stone_scene.outdoor_openness
    );
    assert!(
        stone_scene.rt60_mid_s > 5.0 * wool_scene.rt60_mid_s.max(0.01),
        "stone rt60 {} should dwarf wool rt60 {}",
        stone_scene.rt60_mid_s,
        wool_scene.rt60_mid_s
    );
    assert!(stone_scene.wet_gain > wool_scene.wet_gain * 4.0);

    // Tail energy check: process an impulse through each engine's reverb.
    let tail_rms = |world: &DenseVoxelGrid| {
        let mut e = SpatialAudioEngine::new(FS, SpatialAudioPreset::Ultra);
        e.analyze(world, &source, &listener);
        let mut out = Vec::new();
        e.process(&impulse(8192), &mut out);
        rms(&out[4096 * 2..])
    };
    let cave_tail = tail_rms(&stone_world);
    let wool_tail = tail_rms(&wool_world);
    assert!(
        cave_tail > wool_tail * 5.0,
        "cave tail {cave_tail} should massively exceed wool tail {wool_tail}"
    );
}

#[test]
fn thick_wall_muffles_highs_and_doorway_recovers_via_diffraction() {
    let listener = pose(0.5, 0.5, -4.5, 0.0);
    let source = src(0.5, 0.5, 6.5);
    let solid = wall_world(false);
    let door = wall_world(true);
    let open = DenseVoxelGrid::new([-24, -16, -24], [48, 32, 48], AcousticMaterial::AIR);

    let occ_solid = AcousticTracer::new(&solid, SpatialAudioPreset::Ultra)
        .occlusion_between(listener.position, source.position);
    let occ_door = AcousticTracer::new(&door, SpatialAudioPreset::Ultra)
        .occlusion_between(listener.position, source.position);
    let occ_open = AcousticTracer::new(&open, SpatialAudioPreset::Ultra)
        .occlusion_between(listener.position, source.position);

    assert_eq!(occ_open.transmission_loss_db, 0.0);
    assert!(
        occ_solid.transmission_loss_db >= 40.0,
        "4m of stone: {occ_solid:?}"
    );
    assert!(
        occ_solid.cutoff_hz < 5_000.0,
        "cutoff {}",
        occ_solid.cutoff_hz
    );
    assert!(
        occ_door.diffracted,
        "doorway probe should register a detour"
    );
    assert!(
        occ_door.transmission_loss_db < occ_solid.transmission_loss_db * 0.4,
        "doorway {} dB should be far below wall {} dB",
        occ_door.transmission_loss_db,
        occ_solid.transmission_loss_db
    );

    // High-frequency energy through the wall collapses; the door restores it.
    let hi_tone_rms = |world: &DenseVoxelGrid| {
        let mut e = SpatialAudioEngine::new(FS, SpatialAudioPreset::Ultra);
        e.analyze(world, &source, &listener);
        let mut out = Vec::new();
        e.process(&sine(8_000.0, 4096), &mut out);
        // Skip the first 256 frames (filter slew), measure steady state.
        rms(&out[1024..4096])
    };
    let solid_rms = hi_tone_rms(&solid);
    let door_rms = hi_tone_rms(&door);
    assert!(
        door_rms > solid_rms * 4.0,
        "doorway {door_rms} should recover over solid wall {solid_rms}"
    );
}

#[test]
fn binaural_right_source_arrives_earlier_and_louder_on_right() {
    let world = outdoor();
    let listener = pose(0.5, 0.5, 0.5, 0.0);
    // Facing +Z, the right side is -X: a right-side source lives at (-4, ., 4).
    let source = src(-3.5, 0.5, 4.5);
    let mut e = SpatialAudioEngine::new(FS, SpatialAudioPreset::Ultra);
    e.analyze(&world, &source, &listener);
    let mut out = Vec::new();
    e.process(&impulse(2048), &mut out);
    let l = stereo_l(&out);
    let r = stereo_r(&out);

    let peak_idx = |v: &[f32]| {
        v.iter()
            .enumerate()
            .max_by(|a, b| {
                a.1.abs()
                    .partial_cmp(&b.1.abs())
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .map(|(i, _)| i)
            .unwrap_or(usize::MAX)
    };
    assert!(
        peak_idx(&r) < peak_idx(&l),
        "right ear peak {} should precede left {}",
        peak_idx(&r),
        peak_idx(&l)
    );
    assert!(
        rms(&r) > rms(&l) * 1.15,
        "right RMS {} should exceed left {}",
        rms(&r),
        rms(&l)
    );

    // And mirrored on the left.
    let source_l = src(4.5, 0.5, 4.5);
    e.set_source(source_l);
    let mut out2 = Vec::new();
    e.process(&impulse(2048), &mut out2);
    let l2 = stereo_l(&out2);
    let r2 = stereo_r(&out2);
    assert!(peak_idx(&l2) < peak_idx(&r2), "mirrored ITD wrong");
    assert!(rms(&l2) > rms(&r2) * 1.15, "mirrored ILD wrong");
}

#[test]
fn underwater_muffles_highs_not_bass() {
    let world = outdoor();
    let listener = pose(0.5, 0.5, 0.5, 0.0);
    let source = src(0.5, 0.5, 6.5);

    let run = |freq: f32, submerged: bool| {
        let mut p = listener;
        p.submerged = submerged;
        let mut e = SpatialAudioEngine::new(FS, SpatialAudioPreset::Ultra);
        e.analyze(&world, &source, &p);
        let mut out = Vec::new();
        e.process(&sine(freq, 4096), &mut out);
        rms(&out[2048..])
    };

    let lo_wet = run(250.0, true);
    let lo_dry = run(250.0, false);
    let hi_wet = run(8_000.0, true);
    let hi_dry = run(8_000.0, false);
    assert!(
        hi_wet < hi_dry * 0.2,
        "8 kHz underwater {hi_wet} vs dry {hi_dry}"
    );
    assert!(
        lo_wet > lo_dry * 0.5,
        "250 Hz underwater {lo_wet} vs dry {lo_dry} — bass should survive"
    );
}

#[test]
fn voice_frame_pipeline_is_bounded_finite_and_carries_cave_tail() {
    let world = sealed_room(AcousticMaterial::from_class(MaterialClass::StoneLike));
    let listener = pose(0.5, 0.5, 4.5, 0.0);
    let source = src(0.5, 0.5, -3.5);
    let mut e = SpatialAudioEngine::new(FS, SpatialAudioPreset::Ultra);
    e.analyze(&world, &source, &listener);

    // Two 20 ms bursts of voice-ish tone, then three frames of silence.
    let burst = sine(300.0, 960);
    let silence = vec![0.0_f32; 960];
    let mut all = Vec::new();
    for frame in [&burst, &burst, &silence, &silence, &silence] {
        all.extend(e.process_voice_frame(frame));
    }
    assert_all_finite_bounded(&all);
    let tail = rms(&all[4 * 960 * 2..]);
    assert!(
        tail > 1e-4,
        "cave voice tail should be audible after the burst: {tail}"
    );

    // Pathological input: NaN, ±Inf, and huge values can never blow up.
    let mut dirty = Vec::new();
    let pathological: Vec<f32> = (0..960)
        .map(|i| match i % 5 {
            0 => f32::NAN,
            1 => f32::INFINITY,
            2 => f32::NEG_INFINITY,
            3 => 1e30,
            _ => -1e30,
        })
        .collect();
    for _ in 0..6 {
        dirty.extend(e.process_voice_frame(&pathological));
    }
    assert_all_finite_bounded(&dirty);
    // Recovery: clean audio still flows after the poisoning attempt.
    let clean = e.process_voice_frame(&burst);
    assert_all_finite_bounded(&clean);
    assert!(rms(&clean) > 1e-4, "engine should still produce audio");
}

#[test]
fn subtitle_cue_classifies_direction_and_loudness() {
    let world = outdoor();
    let listener = pose(0.5, 0.5, 0.5, 0.0);
    let mut e = SpatialAudioEngine::new(FS, SpatialAudioPreset::Ultra);

    let mut cue_at = |x: f32, z: f32| {
        e.analyze(&world, &src(x, 0.5, z), &listener);
        e.subtitle_cue()
    };
    // Facing +Z: front is +Z, right is -X.
    assert_eq!(cue_at(0.5, 6.5).direction, SubtitleDirection::Center);
    assert_eq!(cue_at(6.5, 0.5).direction, SubtitleDirection::Left);
    assert_eq!(cue_at(-5.5, 0.5).direction, SubtitleDirection::Right);
    assert_eq!(cue_at(0.5, -5.5).direction, SubtitleDirection::Behind);

    // Loudness falls with distance and collapses behind a wall.
    let near = cue_at(0.5, 3.5);
    let far = cue_at(0.5, 20.5);
    assert!(
        near.loudness > far.loudness,
        "near {} vs far {}",
        near.loudness,
        far.loudness
    );
    let solid = wall_world(false);
    let mut e2 = SpatialAudioEngine::new(FS, SpatialAudioPreset::Ultra);
    e2.analyze(&solid, &src(0.5, 0.5, 6.5), &pose(0.5, 0.5, -4.5, 0.0));
    let behind_wall = e2.subtitle_cue();
    assert!(
        behind_wall.loudness < near.loudness * 0.3,
        "wall should crush perceived loudness: {behind_wall:?}"
    );
}

#[test]
fn scene_tracing_is_deterministic_and_preset_scaled() {
    let world = sealed_room(AcousticMaterial::from_class(MaterialClass::StoneLike));
    let s = [0.5, 0.5, -3.5];
    let l = [0.5, 0.5, 4.5];
    let a = AcousticTracer::new(&world, SpatialAudioPreset::Ultra).compute_scene(s, l, FS);
    let b = AcousticTracer::new(&world, SpatialAudioPreset::Ultra).compute_scene(s, l, FS);
    assert_eq!(a.rays_traced, 64);
    assert_eq!(b.rays_traced, 64);
    assert_eq!(
        a.rt60_mid_s, b.rt60_mid_s,
        "raytracing must be deterministic"
    );
    assert_eq!(a.early_reflections, b.early_reflections);

    let perf = AcousticTracer::new(&world, SpatialAudioPreset::Performance).compute_scene(s, l, FS);
    assert_eq!(perf.rays_traced, 16);
    let real = AcousticTracer::new(&world, SpatialAudioPreset::Realistic).compute_scene(s, l, FS);
    assert_eq!(real.rays_traced, 32);
    // More rays still find the sealed room fully confining.
    assert!(perf.outdoor_openness < 0.05);
    assert!(real.rt60_mid_s > 0.5);
}

#[test]
fn doppler_stays_stable_under_finite_velocities() {
    let world = outdoor();
    let listener = pose(0.5, 0.5, 0.5, 0.0);
    // Approaching at half the speed of sound, then an extreme finite speed.
    for v in [-171.5_f32, -1_000.0, 800.0] {
        let mut s = src(0.5, 0.5, 30.5);
        s.velocity = [0.0, 0.0, v];
        let mut e = SpatialAudioEngine::new(FS, SpatialAudioPreset::Ultra);
        e.analyze(&world, &s, &listener);
        let mut out = Vec::new();
        // Enough frames to fill the history buffer with tone so any clamped
        // Doppler offset still reads real audio.
        for _ in 0..20 {
            e.process(&sine(440.0, 1024), &mut out);
        }
        assert_all_finite_bounded(&out);
        assert!(rms(&out) > 1e-3, "v={v} should still pass audio");
    }
}
