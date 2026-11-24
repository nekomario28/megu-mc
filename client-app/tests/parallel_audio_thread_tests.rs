// SPDX-License-Identifier: LGPL-3.0-or-later
//! Lane MT-CLIENT-AUDIO-2 tests (audit §3.3 / Gate P3):
//!
//!  * P3 — a dedicated real-time `AudioMixThread` decouples all mixing from
//!    the Render Thread: 64 concurrent voices are admitted through the
//!    lock-free SPSC command ring with a sub-0.5 ms caller-side submission,
//!    the 128-sample (2.67 ms @ 48 kHz) cadence mixes 10+ blocks with zero
//!    underruns and zero dropped commands, and the thread joins cleanly on
//!    drop.
//!  * SPSC ring — FIFO ordering preserved across wrap-around (2,000 items
//!    through a 512-slot ring), and a full ring fails closed (`false`,
//!    counted, never a panic or a block).

use std::sync::Arc;
use std::time::{Duration, Instant};

use megu_client::audio_out::{
    AudioCmd, AudioCmdRing, AudioMixThread, AudioMixThreadOptions, NullSink,
    AUDIO_CMD_RING_CAPACITY, AUDIO_MIX_VOICE_SLOTS,
};
use megu_client::sound_assets::{DecodedPcm, MIX_SAMPLE_RATE};

/// One second of a constant tone — long enough that no voice finishes
/// inside the test window.
fn mono_clip() -> DecodedPcm {
    DecodedPcm::Mono(vec![0.25; MIX_SAMPLE_RATE as usize])
}

fn stereo_clip() -> DecodedPcm {
    DecodedPcm::Stereo(vec![[0.2, -0.2]; MIX_SAMPLE_RATE as usize])
}

fn play_cmd(voice_id: u64, mono: &Arc<DecodedPcm>, stereo: &Arc<DecodedPcm>) -> AudioCmd {
    let is_2d = voice_id.is_multiple_of(3);
    AudioCmd::PlayDecoded {
        voice_id,
        // `Arc::clone` is O(1) — the decode worker's payload is shared, not
        // rebuilt, matching the production Lane B handoff.
        pcm: if is_2d { stereo.clone() } else { mono.clone() },
        pos: [voice_id as f32, 64.0, 0.0],
        entity_id: voice_id.is_multiple_of(5).then_some(100 + voice_id as i32),
        volume: 1.0,
        pitch: 1.0,
        attenuation_distance: 16.0,
        is_2d,
        category: 0,
    }
}

#[test]
fn p3_dedicated_audio_mix_thread_64_voices_nonblocking_submit_zero_underruns() {
    let thread = AudioMixThread::spawn(
        Box::new(NullSink::default()),
        &AudioMixThreadOptions::default(),
    );
    let ring = thread.ring();

    // The caller (Render Thread) submits the whole scene — 64 concurrent
    // 3D + 2D voices plus listener/entity updates — through the lock-free
    // ring. Gate P3's bound is < 0.5 ms of caller-side CPU. PCM payloads
    // are shared `Arc`s here exactly as a Lane B decode worker hands them
    // over — building them is not part of the submission path.
    let mono = Arc::new(mono_clip());
    let stereo = Arc::new(stereo_clip());
    let submit_start = Instant::now();
    for voice_id in 1..=AUDIO_MIX_VOICE_SLOTS as u64 {
        assert!(
            ring.try_push(play_cmd(voice_id, &mono, &stereo)),
            "ring must accept voice {voice_id}"
        );
    }
    assert!(ring.try_push(AudioCmd::UpdateListener {
        pos: [0.0, 64.0, 0.0],
        vel: [0.0, 0.0, 0.0],
        yaw_deg: 0.0,
        pitch_deg: 0.0,
        submerged: false,
    }));
    assert!(ring.try_push(AudioCmd::UpdateEntityPos {
        entity_id: 100,
        pos: [4.0, 64.0, 4.0],
        vel: [0.5, 0.0, 0.0],
    }));
    assert!(ring.try_push(AudioCmd::ApplyAcousticResult {
        voice_id: 2,
        occlusion_db: 6.0,
        reverb_send: 0.4,
    }));
    let submit_elapsed = submit_start.elapsed();
    eprintln!("P3 submission latency (67 cmds): {submit_elapsed:?}");
    assert!(
        submit_elapsed < Duration::from_micros(500),
        "caller-side submission of 67 commands took {submit_elapsed:?}; Gate P3 bound is < 0.5 ms"
    );
    assert_eq!(ring.dropped_commands(), 0, "512-slot ring held all submissions");

    // Let the real-time loop run ~15 blocks (40 ms > 10 × 2.67 ms).
    std::thread::sleep(Duration::from_millis(50));
    assert!(
        thread.blocks_mixed() >= 10,
        "expected >= 10 blocks in 50 ms, got {}",
        thread.blocks_mixed()
    );
    assert_eq!(
        thread.underrun_count(),
        0,
        "Gate P3: zero underruns while the render thread is busy"
    );
    assert_eq!(thread.dropped_commands(), 0);
    assert_eq!(
        thread.commands_processed(),
        67,
        "every submitted command drained"
    );
    assert_eq!(
        thread.active_voice_count(),
        AUDIO_MIX_VOICE_SLOTS,
        "all 64 voice slots sounding"
    );
    drop(thread); // pushes Shutdown and joins the OS thread cleanly
}

#[test]
fn spsc_ring_is_lockfree_fifo_and_fails_closed_when_full() {
    let ring = AudioCmdRing::new(AUDIO_CMD_RING_CAPACITY);
    assert_eq!(ring.capacity(), AUDIO_CMD_RING_CAPACITY);
    assert!(ring.is_empty());

    // FIFO ordering.
    assert!(ring.try_push(AudioCmd::StopAll));
    assert!(ring.try_push(AudioCmd::UpdateEntityPos {
        entity_id: 7,
        pos: [1.0, 2.0, 3.0],
        vel: [0.0; 3],
    }));
    assert!(matches!(ring.try_pop(), Some(AudioCmd::StopAll)));
    match ring.try_pop() {
        Some(AudioCmd::UpdateEntityPos { entity_id, .. }) => assert_eq!(entity_id, 7),
        other => panic!("expected UpdateEntityPos, got {other:?}"),
    }
    assert!(ring.try_pop().is_none());

    // Wrap-around: 2,000 items through a 512-slot ring, interleaved so
    // indices wrap the mask several times.
    for i in 0..2_000i32 {
        assert!(ring.try_push(AudioCmd::UpdateEntityPos {
            entity_id: i,
            pos: [0.0; 3],
            vel: [0.0; 3],
        }));
        match ring.try_pop() {
            Some(AudioCmd::UpdateEntityPos { entity_id, .. }) => {
                assert_eq!(entity_id, i, "FIFO broke at item {i}")
            }
            other => panic!("expected UpdateEntityPos at {i}, got {other:?}"),
        }
    }
    assert!(ring.is_empty());

    // Half-full drain pattern across the wrap boundary.
    for i in 0..1_000i32 {
        assert!(ring.try_push(AudioCmd::UpdateEntityPos {
            entity_id: i,
            pos: [0.0; 3],
            vel: [0.0; 3],
        }));
        if i % 2 == 0 {
            assert!(ring.try_pop().is_some());
        }
    }
    let mut last = -1i32;
    while let Some(AudioCmd::UpdateEntityPos { entity_id, .. }) = ring.try_pop() {
        assert!(entity_id > last, "FIFO order broken after wrap");
        last = entity_id;
    }

    // Full ring fails closed: `false`, counted, never a panic or a wait.
    for _ in 0..AUDIO_CMD_RING_CAPACITY {
        assert!(ring.try_push(AudioCmd::StopAll));
    }
    assert!(!ring.try_push(AudioCmd::StopAll), "full ring must fail closed");
    assert_eq!(ring.dropped_commands(), 1);
    assert_eq!(ring.len(), AUDIO_CMD_RING_CAPACITY);
    // Drained FIFO after the full fill.
    for _ in 0..AUDIO_CMD_RING_CAPACITY {
        assert!(matches!(ring.try_pop(), Some(AudioCmd::StopAll)));
    }
    assert!(ring.try_pop().is_none());
}

#[test]
fn audio_mix_thread_manual_step_is_deterministic_and_shutdown_is_clean() {
    // Manual mode drives the identical 128-sample pipeline without the OS
    // thread — the timing-verification surface Gate P3 asks for.
    let mut thread = AudioMixThread::manual(
        Box::new(NullSink::default()),
        &AudioMixThreadOptions::default(),
    );
    assert!(!thread.is_running());
    let mono = Arc::new(mono_clip());
    let stereo = Arc::new(stereo_clip());
    assert!(thread.submit(play_cmd(1, &mono, &stereo)));
    assert!(thread.submit(AudioCmd::UpdateListener {
        pos: [0.0, 64.0, 0.0],
        vel: [0.0; 3],
        yaw_deg: 0.0,
        pitch_deg: 0.0,
        submerged: false,
    }));
    assert!(thread.step_one_block());
    assert_eq!(thread.blocks_mixed(), 1);
    assert_eq!(thread.commands_processed(), 2);
    assert_eq!(thread.active_voice_count(), 1);
    assert_eq!(thread.underrun_count(), 0);
    assert!(thread.step_one_block());
    assert_eq!(thread.blocks_mixed(), 2);

    // A stopped scene drains to zero active voices.
    assert!(thread.submit(AudioCmd::StopAll));
    assert!(thread.step_one_block());
    assert_eq!(thread.active_voice_count(), 0);

    // Promoting the manual core onto the real thread, then shutting down.
    thread.start().expect("manual core promotes to the OS thread");
    std::thread::sleep(Duration::from_millis(10));
    thread.shutdown();
    assert!(!thread.is_running());
    drop(thread);
}
