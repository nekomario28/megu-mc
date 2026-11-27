// SPDX-License-Identifier: LGPL-3.0-or-later
//! Rich spatial-audio comfort & cost gates.
//!
//! Four properties the rich subsystem has to keep:
//!
//! * **Dry & stereo 2D** — Music/UI/ambient voices never reach the
//!   spatializer, so no cave reverb, no underwater low-pass, no mono squash.
//! * **Vanilla range law** — `attenuation_distance * max(volume, 1)` decides
//!   audibility, so `volume = 4.0` on a 16-block sound carries 64 blocks, and
//!   outside that window nothing leaks back in through the reflection paths.
//! * **No hard chops, no idle cost** — a reverb tail keeps decaying after the
//!   voice that fed it ends, then drops to a zero state below the -72 dBFS
//!   floor and stops stepping entirely.
//! * **Control-block rate** — spatial parameters are resolved and
//!   interpolated at a fixed 64-sample cadence, so the signal does not depend
//!   on how the caller chunked it, an occlusion change is a ramp instead of a
//!   click, and a glided parameter converges on the same steady state a
//!   snapped one would.

use std::sync::{Arc, Mutex};

use megu_client::audio_out::{
    AudioCmd, AudioCmdRing, AudioMixCore, AudioMixTelemetry, AudioMixThreadOptions, AudioSink,
    AUDIO_CMD_RING_CAPACITY, AUDIO_MIX_BLOCK_FRAMES, AUDIO_MIX_BLOCK_SAMPLES,
};
use megu_client::sound_assets::{DecodedPcm, MIX_SAMPLE_RATE};
use megu_client::spatial_audio::{
    tail_silence_linear, AcousticMaterial, AcousticScene, DenseVoxelGrid, ListenerPose,
    MaterialClass, SharedListenerFdnBus, SoundSource, SpatialAudioEngine, SpatialAudioPreset,
    CONTROL_BLOCK_SAMPLES, TAIL_SILENCE_DB,
};

const FS: u32 = MIX_SAMPLE_RATE;

fn sine(freq: f32, n: usize) -> Vec<f32> {
    (0..n)
        .map(|i| (2.0 * std::f32::consts::PI * freq * i as f32 / FS as f32).sin() * 0.5)
        .collect()
}

fn rms(v: &[f32]) -> f32 {
    (v.iter().map(|x| x * x).sum::<f32>() / v.len().max(1) as f32).sqrt()
}

fn peak(v: &[f32]) -> f32 {
    v.iter().fold(0.0_f32, |m, s| m.max(s.abs()))
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

/// Symmetric stone shell with an air interior — a reflective room, so the
/// early-reflection path is genuinely non-empty.
fn stone_room() -> DenseVoxelGrid {
    let mut g = DenseVoxelGrid::new(
        [-12, -8, -12],
        [24, 16, 24],
        AcousticMaterial::from_class(MaterialClass::StoneLike),
    );
    g.fill_box([-7, -3, -7], [6, 3, 6], AcousticMaterial::AIR);
    g
}

/// Sealed stone tunnel 60 blocks long on +Z: reflections exist all the way
/// down, so a range-gated voice has something to leak through if the gate is
/// missing.
fn stone_tunnel() -> DenseVoxelGrid {
    let mut g = DenseVoxelGrid::new(
        [-10, -8, 0],
        [20, 16, 60],
        AcousticMaterial::from_class(MaterialClass::StoneLike),
    );
    g.fill_box([-5, -3, 0], [4, 3, 59], AcousticMaterial::AIR);
    g
}

/// Sink that keeps every block it was handed, so a test can inspect the mix
/// the way the device would hear it.
struct CaptureSink {
    blocks: Arc<Mutex<Vec<i16>>>,
}

impl AudioSink for CaptureSink {
    fn push(&mut self, pcm: &[i16]) -> Result<(), String> {
        let mut guard = self
            .blocks
            .lock()
            .map_err(|_| "capture sink poisoned".to_string())?;
        guard.extend_from_slice(pcm);
        Ok(())
    }

    fn label(&self) -> &'static str {
        "capture"
    }

    fn requires_realtime_cadence(&self) -> bool {
        false
    }
}

#[test]
fn rich_audio_2d_ui_bgm_bypass_stays_dry_and_stereo() {
    // A 2D Music/UI/ambient voice is a stereo asset with hard-panned content,
    // played to a *submerged* listener with maximum acoustic drama available
    // downstream. Both constraints must lose to the bypass: no mono squash, no
    // underwater muffle, no reverb send.
    let frames = AUDIO_MIX_BLOCK_SAMPLES / 2 * 5;
    let stereo: Vec<[f32; 2]> = (0..frames)
        .map(|i| {
            let t = i as f32 / FS as f32;
            // Harness gain 1.0 and |samples| <= 0.8 keep the bus limiter at
            // identity, so any difference at all is the spatializer.
            [
                0.5 * (2.0 * std::f32::consts::PI * 220.0 * t).sin(),
                -0.5 * (2.0 * std::f32::consts::PI * 330.0 * t).sin(),
            ]
        })
        .collect();

    let captured: Arc<Mutex<Vec<i16>>> = Arc::new(Mutex::new(Vec::new()));
    let mut core = Box::new(AudioMixCore::new(
        Box::new(CaptureSink {
            blocks: Arc::clone(&captured),
        }),
        &AudioMixThreadOptions::default(),
    ));
    let ring = AudioCmdRing::new(AUDIO_CMD_RING_CAPACITY);
    let telemetry = AudioMixTelemetry::default();

    assert!(ring.try_push(AudioCmd::UpdateListener {
        pos: [0.0, 64.0, 0.0],
        vel: [0.0, 0.0, 0.0],
        yaw_deg: 37.0,
        pitch_deg: 0.0,
        submerged: true,
    }));
    assert!(ring.try_push(AudioCmd::PlayDecoded {
        voice_id: 7,
        pcm: Arc::new(DecodedPcm::Stereo(stereo.clone())),
        pos: [3.0, 64.0, 0.0],
        entity_id: None,
        volume: 1.0,
        pitch: 1.0,
        attenuation_distance: 16.0,
        is_2d: true,
        category: 1,
    }));
    for _ in 0..5 {
        core.step_one_block(&ring, &telemetry);
    }

    let out = captured
        .lock()
        .expect("capture sink is not poisoned")
        .clone();
    assert!(
        out.len() >= (AUDIO_MIX_BLOCK_FRAMES * 4) * 2,
        "expected four blocks of captured audio, got {} samples",
        out.len()
    );
    // `render_2d` reads frame `i` and `i + 1`, so the very last frame of the
    // clip is a guard: compare the first four blocks.
    let mut saw_channel_split = false;
    for i in 0..AUDIO_MIX_BLOCK_FRAMES * 4 {
        let expect_l = (stereo[i][0] * 32_767.0).round() as i32;
        let expect_r = (stereo[i][1] * 32_767.0).round() as i32;
        let (l, r) = (i32::from(out[2 * i]), i32::from(out[2 * i + 1]));
        assert!(
            (l - expect_l).abs() <= 1 && (r - expect_r).abs() <= 1,
            "2D bypass altered frame {i}: got [{l}, {r}], expected [{expect_l}, {expect_r}]"
        );
        if l != r {
            saw_channel_split = true;
        }
    }
    assert!(
        saw_channel_split,
        "2D bypass must keep L/R apart instead of mono-squashing"
    );
    // Nothing was ever sent into the shared reverb: the cave is not on this
    // path even while the listener is underwater.
    assert!(
        core.reverb_bus_dormant(),
        "a 2D voice must never excite the shared reverb bus"
    );
}

#[test]
fn rich_audio_volume_above_one_widens_audibility_past_sixteen_blocks() {
    let world = stone_tunnel();
    let listener = pose(0.5, 0.5, 0.5, 0.0);
    // 40 blocks down a reflective stone tunnel: well past the 16-block
    // default, and surrounded by walls that would keep a reflection alive if
    // the range gate only covered the direct path.
    let source = src(0.5, 0.5, 40.5);

    let render = |range_m: f32| -> Vec<f32> {
        let mut e = SpatialAudioEngine::new(FS, SpatialAudioPreset::Realistic);
        e.set_max_range(range_m);
        // Traced, not posed: the tunnel really does return early reflections
        // and a reverb estimate, so the range gate has to cover them too.
        e.analyze(&world, &source, &listener);
        let mut out = Vec::new();
        e.process(&sine(300.0, 9_600), &mut out);
        out
    };

    // Vanilla audible range: attenuation_distance(16) * max(volume, 1.0).
    let quiet_range = render(16.0 * 1.0_f32.max(1.0));
    let loud_range = render(16.0 * 4.0_f32.max(1.0));

    assert!(
        peak(&quiet_range) < 1e-9,
        "a 16-block sound 40 blocks away must be silent, heard {}",
        peak(&quiet_range)
    );
    assert!(
        peak(&loud_range) > 1e-3,
        "volume 4.0 must widen the range to 64 blocks and stay audible, peak {}",
        peak(&loud_range)
    );

    let mut probe = SpatialAudioEngine::new(FS, SpatialAudioPreset::Realistic);
    probe.set_max_range(16.0 * 4.0_f32.max(1.0));
    assert_eq!(probe.max_range(), 64.0);
}

#[test]
fn rich_audio_reverb_tail_outlives_the_voice_then_goes_dormant() {
    let mut bus = SharedListenerFdnBus::new(FS);
    bus.set_rt60([0.9, 0.7, 0.5]);
    assert!(
        bus.is_dormant(),
        "a never-excited bus is a zero state, not a running network"
    );

    // 50 ms of send, then silence: the classic "voice ended, room rings".
    for i in 0..2_400 {
        let s = (2.0 * std::f32::consts::PI * 180.0 * i as f32 / FS as f32).sin() * 0.6;
        bus.send(s, 0.8);
        bus.step();
    }
    assert!(!bus.is_dormant(), "a driven bus must not report dormant");

    // The tail must keep sounding and decay smoothly — never chop to zero
    // while it is still above the floor.
    let mut windows: Vec<f32> = Vec::new();
    let mut window_peak = 0.0_f32;
    let mut dormant_after = None;
    for i in 0..FS as usize * 4 {
        let (l, r) = bus.step_input(0.0);
        if bus.is_dormant() {
            dormant_after = Some(i);
            break;
        }
        window_peak = window_peak.max(l.abs()).max(r.abs());
        if (i + 1) % 2_400 == 0 {
            windows.push(window_peak);
            window_peak = 0.0;
        }
    }
    let dormant_after = dormant_after.expect("the tail must go dormant within 4 s");
    assert!(
        dormant_after > 2_400,
        "the tail must outlive the send by at least 50 ms, went dormant at {dormant_after}"
    );
    assert!(
        windows.len() >= 3,
        "expected several decay windows, got {}",
        windows.len()
    );
    for (i, pair) in windows.windows(2).enumerate() {
        assert!(
            pair[1] <= pair[0] * 1.15,
            "reverb tail must decay, not swell, at window {i}: {} -> {}",
            pair[0],
            pair[1]
        );
    }
    // Going dormant is the floor guarantee: the bus only stops when a whole
    // probe window stayed under `TAIL_SILENCE_DB`, so the tail cannot be cut
    // while it is still audible. What the windows add is that the decay was
    // gradual the whole way down rather than a step.
    let last = *windows.last().expect("non-empty windows");
    assert!(
        last < windows[0] * 0.02,
        "the tail must decay far below the send: {} -> {last}",
        windows[0]
    );
    assert!(
        last < 1e-3,
        "final window {last} is not near the {TAIL_SILENCE_DB} dBFS floor"
    );
}

#[test]
fn rich_audio_tail_survives_the_delay_line_before_its_first_echo() {
    // A single-sample send into a network whose RT60 is pinned at the 30 ms
    // floor: the writes stop after one sample and the wet output stays exactly
    // zero until the shortest line turns over ~30 ms later. Declaring
    // dormancy in that gap would clear the echo before it is ever heard —
    // the hard chop this whole path exists to avoid.
    let mut bus = SharedListenerFdnBus::new(FS);
    bus.set_rt60([0.0, 0.0, 0.0]);
    assert_eq!(
        bus.step_input(0.8),
        (0.0, 0.0),
        "no tap can return a sample in its own step"
    );
    let mut heard = 0.0_f32;
    for _ in 0..FS as usize * 2 / 10 {
        let (l, r) = bus.step_input(0.0);
        heard = heard.max(l.abs()).max(r.abs());
    }
    assert!(
        heard > 1e-4,
        "the first echo was cleared before it arrived (peak {heard})"
    );
}

#[test]
fn rich_audio_dormant_bus_is_free_and_wakes_on_the_next_send() {
    let mut bus = SharedListenerFdnBus::new(FS);
    bus.set_rt60([0.4, 0.3, 0.2]);
    for _ in 0..480 {
        bus.step_input(0.7);
    }
    // Let the short room decay away completely, keeping the last probe
    // window's worth of output so the floor claim can be checked directly.
    let mut window: std::collections::VecDeque<f32> = std::collections::VecDeque::new();
    let mut silent_steps = 0;
    while !bus.is_dormant() && silent_steps < FS as usize {
        let (l, r) = bus.step_input(0.0);
        assert!(l.is_finite() && r.is_finite());
        window.push_back(l.abs().max(r.abs()));
        if window.len() > CONTROL_BLOCK_SAMPLES {
            window.pop_front();
        }
        silent_steps += 1;
    }
    assert!(bus.is_dormant(), "the bus never reached dormancy");
    let floor = tail_silence_linear();
    let last_window_peak = window.iter().fold(0.0_f32, |m, s| m.max(*s));
    assert!(
        last_window_peak < floor,
        "dormancy was declared while the wet output was still {last_window_peak} \
         (above the {TAIL_SILENCE_DB} dBFS floor {floor})"
    );

    // Dormant means exactly zero output and no state change: a listener
    // sitting still costs nothing and cannot emit a residue.
    for _ in 0..4_096 {
        assert_eq!(bus.step_input(0.0), (0.0, 0.0));
    }
    assert!(bus.is_dormant());
    // `send`/`step` shares the same fast path.
    assert_eq!(bus.step(), (0.0, 0.0));

    // A new send wakes it and produces a real tail again.
    let mut woke_peak = 0.0_f32;
    for i in 0..4_800 {
        let s = (2.0 * std::f32::consts::PI * 200.0 * i as f32 / FS as f32).sin() * 0.5;
        let (l, r) = bus.step_input(s);
        woke_peak = woke_peak.max(l.abs()).max(r.abs());
    }
    assert!(!bus.is_dormant(), "a driven bus must wake");
    assert!(woke_peak > 1e-3, "woken bus stayed silent ({woke_peak})");
}

#[test]
fn rich_audio_control_blocks_make_output_independent_of_chunking() {
    let world = stone_room();
    let listener = pose(0.5, 0.5, 4.5, 20.0);
    let source = src(0.5, 0.5, -3.5);
    // 2048 frames covers every production cadence: one call, 128-sample
    // real-time blocks, and 960-sample `VCR1` voice frames (960 + 960 + 128).
    let input = sine(440.0, 2_048);

    let render = |chunk: usize| -> Vec<f32> {
        let mut e = SpatialAudioEngine::new(FS, SpatialAudioPreset::Ultra);
        e.analyze(&world, &source, &listener);
        let mut all = Vec::new();
        for slice in input.chunks(chunk) {
            let mut out = Vec::new();
            e.process(slice, &mut out);
            all.extend(out);
        }
        all
    };

    let one_call = render(input.len());
    assert_eq!(one_call.len(), input.len() * 2);
    for chunk in [CONTROL_BLOCK_SAMPLES, 128, 960] {
        let chunked = render(chunk);
        assert_eq!(chunked.len(), one_call.len());
        for (i, (&a, &b)) in one_call.iter().zip(chunked.iter()).enumerate() {
            assert_eq!(
                a.to_bits(),
                b.to_bits(),
                "sample {i} changed with a {chunk}-sample caller block: {a} vs {b}"
            );
        }
    }
}

#[test]
fn rich_audio_occlusion_jump_is_ramped_instead_of_clicking() {
    let world = stone_room();
    // ~1.2 blocks apart: the direct path dominates, so a hard occlusion step
    // would move the waveform by a large fraction of its amplitude in one
    // sample.
    let listener = pose(0.5, 0.5, -0.7, 0.0);
    let source = src(0.5, 0.5, 0.5);
    let mut e = SpatialAudioEngine::new(FS, SpatialAudioPreset::Ultra);
    let scene = e.analyze(&world, &source, &listener).clone();
    assert!(
        scene.occlusion.transmission_loss_db < 1.0,
        "the two ends should start in clear line of sight: {:?}",
        scene.occlusion
    );

    let open = {
        let mut out = Vec::new();
        e.process(&sine(300.0, 4_800), &mut out);
        out
    };
    assert!(rms(&open) > 1e-2, "warm-up should be audible: {}", rms(&open));

    // Slam the occlusion shut between two control blocks — the case that used
    // to step `direct_gain` from ~0.55 to ~0 in one sample.
    let mut blocked_scene = scene.clone();
    blocked_scene.occlusion.transmission_loss_db = 60.0;
    blocked_scene.occlusion.direct_loss_db = 60.0;
    blocked_scene.occlusion.cutoff_hz = 350.0;
    blocked_scene.occlusion.blocked = true;
    e.set_scene(blocked_scene.clone());
    let blocked = {
        let mut out = Vec::new();
        e.process(&sine(300.0, 4_800), &mut out);
        out
    };

    // Join the two runs and measure the largest single-sample step.
    let mut joined: Vec<f32> = open[open.len() - 256..].to_vec();
    joined.extend_from_slice(&blocked[..512]);
    let max_step = joined
        .as_chunks::<2>()
        .0
        .windows(2)
        .map(|w| (w[1][0] - w[0][0]).abs())
        .fold(0.0_f32, f32::max);
    let unrestricted = 2.0 * std::f32::consts::PI * 300.0 / FS as f32 * 0.5;
    assert!(
        max_step < unrestricted + 0.05,
        "occlusion change clicked: max per-sample step {max_step} vs a \
         {unrestricted} smooth-slope bound"
    );
    // The occlusion did land — the direct path is 60 dB down — though the
    // room tail the open run already excited keeps ringing, which is exactly
    // the behaviour a hard chop would destroy.
    assert!(
        rms(&blocked) < rms(&open) * 0.71,
        "occlusion must actually take effect: {} vs {}",
        rms(&blocked),
        rms(&open)
    );
    for (i, s) in blocked.iter().enumerate() {
        assert!(s.is_finite(), "sample {i} non-finite: {s}");
    }
}

#[test]
fn rich_audio_glided_parameters_settle_on_the_snapped_steady_state() {
    // A dry scene (no early taps, zero RT60) isolates the parameters that
    // glide: `direct_gain` and the occlusion filter cutoff. The glided engine
    // must land on the same waveform a fresh engine produces when its
    // parameters were snapped from the first sample.
    let mut blocked = AcousticScene::default();
    blocked.occlusion.transmission_loss_db = 60.0;
    blocked.occlusion.direct_loss_db = 60.0;
    blocked.occlusion.cutoff_hz = 350.0;
    blocked.occlusion.blocked = true;

    let listener = pose(0.5, 0.5, -0.7, 0.0);
    let source = src(0.5, 0.5, 0.5);
    let input = sine(300.0, 4_800);

    let mut glided_engine = SpatialAudioEngine::new(FS, SpatialAudioPreset::Ultra);
    glided_engine.set_scene(AcousticScene::default());
    glided_engine.set_source(source);
    glided_engine.set_listener(listener);
    let mut warm = Vec::new();
    glided_engine.process(&input, &mut warm);
    glided_engine.set_scene(blocked.clone());
    let mut glided = Vec::new();
    glided_engine.process(&input, &mut glided);

    let mut snapped_engine = SpatialAudioEngine::new(FS, SpatialAudioPreset::Ultra);
    snapped_engine.set_scene(blocked);
    snapped_engine.set_source(source);
    snapped_engine.set_listener(listener);
    let mut snapped = Vec::new();
    snapped_engine.process(&input, &mut snapped);

    // 75..100 ms: three-plus slew time constants after the change, so the
    // glide has converged (a one-control-block ramp finishes in 1.3 ms; the
    // cutoff needs ~5 τ = 75 ms).
    let window = 3_600..4_800;
    let a = rms(&glided[window.clone()]);
    let b = rms(&snapped[window]);
    assert!(
        (a - b).abs() <= b.max(1e-6) * 0.25,
        "glided parameters settled at {a}, snapped reference at {b}"
    );
    assert!(b > 1e-4, "reference should still carry the muted direct path");
}

#[test]
fn rich_audio_3d_voice_returns_the_reverb_bus_to_zero_cost_idle() {
    let captured: Arc<Mutex<Vec<i16>>> = Arc::new(Mutex::new(Vec::new()));
    let mut core = Box::new(AudioMixCore::new(
        Box::new(CaptureSink {
            blocks: Arc::clone(&captured),
        }),
        &AudioMixThreadOptions::default(),
    ));
    let ring = AudioCmdRing::new(AUDIO_CMD_RING_CAPACITY);
    let telemetry = AudioMixTelemetry::default();
    assert!(core.reverb_bus_dormant(), "a fresh core idles");

    assert!(ring.try_push(AudioCmd::UpdateListener {
        pos: [0.0, 64.0, 0.0],
        vel: [0.0, 0.0, 0.0],
        yaw_deg: 0.0,
        pitch_deg: 0.0,
        submerged: false,
    }));
    // One second of 3D tone a couple of blocks away.
    assert!(ring.try_push(AudioCmd::PlayDecoded {
        voice_id: 11,
        pcm: Arc::new(DecodedPcm::Mono(sine(440.0, FS as usize))),
        pos: [2.0, 64.0, 0.0],
        entity_id: None,
        volume: 1.0,
        pitch: 1.0,
        attenuation_distance: 16.0,
        is_2d: false,
        category: 3,
    }));
    // Admission is metered, so the voice exists only after the first block —
    // the acoustic result has to arrive once it does, exactly as the Lane B
    // worker's later reply would.
    core.step_one_block(&ring, &telemetry);
    assert!(ring.try_push(AudioCmd::ApplyAcousticResult {
        voice_id: 11,
        occlusion_db: 6.0,
        reverb_send: 0.6,
    }));
    core.step_one_block(&ring, &telemetry);
    assert!(
        !core.reverb_bus_dormant(),
        "a 3D voice with a reverb send must excite the shared bus"
    );

    // Let the clip finish and the tail fall below the floor: the latch has to
    // clear so the next idle block is free. The extra blocks cover the longest
    // FDN delay line (90 ms), which still holds real signal after the last
    // send — a tail that short must not be declared silent early.
    for _ in 0..(FS as usize / AUDIO_MIX_BLOCK_FRAMES) + 64 {
        core.step_one_block(&ring, &telemetry);
    }
    assert_eq!(
        telemetry
            .active_voices
            .load(std::sync::atomic::Ordering::Relaxed),
        0
    );
    assert!(
        core.reverb_bus_dormant(),
        "the bus must return to dormancy once its tail is below the floor"
    );
}
