// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! Live client session state: the per-connection home of the standalone
//! subsystems the client wires together at runtime.
//!
//! [`ClientSession`] owns:
//!
//! * a [`SpatialAudioEngine`] (default opt-in `Realistic` preset at 48 kHz)
//!   used by [`ClientSession::evaluate_sound_event`] to turn a positioned
//!   sound event plus a [`VoxelAcousticWorld`] view into binaural mix
//!   parameters;
//! * a [`CrashCircuitBreaker`] driven through [`CrashGuard`] by
//!   [`ClientSession::run_guarded_subsystem`], so a panicking subsystem is
//!   contained, reported, and — once the breaker trips — that subsystem
//!   alone is disabled while the session stays alive;
//! * the latest `megu:season` / `SSN1` [`SeasonWireSync`] applied via
//!   [`ClientSession::handle_megu_custom_payload`], the session-facing
//!   router over [`crate::protocol::decode_megu_custom_payload`].
//!
//! Everything here is deterministic: no wall-clock reads — the breaker is
//! fed caller-injected `tick`/millisecond values exactly like the rest of
//! [`crate::recovery`].

use std::collections::{BTreeSet, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;

use crate::audio_qol::{AudioLimiter, SoundAdmission, SoundKey};
use crate::crash_reporter::ReportEnv;
use crate::protocol::{decode_megu_custom_payload, MeguCustomPayload, SeasonWireSync};
use crate::recovery::{ContainmentOutcome, CrashCircuitBreaker, CrashGuard, RecoveryAction};
use crate::sound_assets::{normalize_sound_name, sound_name_hash};
use crate::spatial_audio::{
    AcousticMaterial, AcousticTracer, ListenerPose, MaterialClass, SoundSource, SpatialAudioEngine,
    SpatialAudioPreset, SpatialSubtitleCue, VoxelAcousticWorld,
};
use crate::world::WorldState;

/// Default spatial-audio sample rate (matches the `VCR1` PCM path).
const DEFAULT_SAMPLE_RATE: u32 = 48_000;

/// A session subsystem that [`ClientSession::run_guarded_subsystem`] can
/// contain individually. Once the circuit breaker trips on a subsystem's
/// crashes, only that subsystem is disabled — every other boundary keeps
/// running.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum GuardedSubsystem {
    /// Raytraced spatial-audio analysis and binaural DSP.
    SpatialAudio,
    /// `SSN1` season-sync application (tints, snow, daylight curve).
    SeasonSync,
    /// `VFX1` custom-payload host ingest.
    Vfx,
    /// `ANM1` skeletal-animation host ingest.
    WorldAnimation,
    /// Zero-restart mod reload transaction apply.
    ModReload,
}

impl GuardedSubsystem {
    /// Stable label baked into captured crash reports.
    pub fn label(self) -> &'static str {
        match self {
            GuardedSubsystem::SpatialAudio => "spatial-audio",
            GuardedSubsystem::SeasonSync => "season-sync",
            GuardedSubsystem::Vfx => "vfx",
            GuardedSubsystem::WorldAnimation => "world-animation",
            GuardedSubsystem::ModReload => "mod-reload",
        }
    }
}

/// Listener-relative mix parameters for one positioned sound event — the
/// subset of [`crate::spatial_audio::AcousticScene`] plus source geometry a
/// mixer needs to spatialize a mono voice without touching engine DSP
/// state. All fields are finite by construction (occlusion/rolloff math
/// clamps, `distance_m >= 0`).
#[derive(Clone, Debug, PartialEq)]
pub struct BinauralMixParams {
    /// Listener-relative azimuth in radians; positive means the source is
    /// to the listener's right.
    pub azimuth_rad: f32,
    /// Listener-relative elevation in radians; positive is up.
    pub elevation_rad: f32,
    /// Source-to-listener distance in blocks.
    pub distance_m: f32,
    /// Linear direct-path gain after distance rolloff and occlusion,
    /// `0.0..=1.0`.
    pub direct_gain: f32,
    /// Occlusion low-pass cutoff for the direct path, in Hz.
    pub occlusion_cutoff_hz: f32,
    /// `true` when the direct path is effectively blocked (>= 60 dB loss)
    /// and only the reverb field remains.
    pub blocked: bool,
    /// Late-field reverb send level, `0.0..=1.0`.
    pub wet_send: f32,
    /// Mid-band (~1 kHz) RT60 estimate in seconds.
    pub rt60_mid_s: f32,
    /// `true` when the listener's ears are submerged — applies the
    /// underwater muffle to the whole mix.
    pub submerged: bool,
}

/// Live session state binding the protocol router, spatial audio engine,
/// and crash containment together.
pub struct ClientSession {
    /// Binaural/spatial audio engine; default `Realistic` preset at
    /// [`DEFAULT_SAMPLE_RATE`]. Each sounding source gets its own engine
    /// upstream, but the session-level engine supplies the preset, sample
    /// rate, and shared scene analysis used by
    /// [`Self::evaluate_sound_event`].
    pub spatial_audio: SpatialAudioEngine,
    /// Crash-loop guard shared by all guarded subsystem boundaries.
    pub crash_breaker: CrashCircuitBreaker,
    /// Latest `SSN1` season sync received on the season channel.
    pub latest_season: Option<SeasonWireSync>,
    /// Subsystems disabled after their breaker trip.
    disabled_subsystems: BTreeSet<GuardedSubsystem>,
    /// Blame metadata for crash reports captured by [`CrashGuard`].
    report_env: ReportEnv,
    /// Directory guarded-boundary crash reports are persisted into.
    crash_dir: PathBuf,
}

impl ClientSession {
    /// A session with default opt-in configuration: `Realistic` spatial
    /// audio at 48 kHz, a fresh default-threshold circuit breaker, and no
    /// received season state. `report_env`/`crash_dir` feed the
    /// [`CrashGuard`] containment boundaries.
    pub fn new(report_env: ReportEnv, crash_dir: PathBuf) -> Self {
        Self {
            spatial_audio: SpatialAudioEngine::new(
                DEFAULT_SAMPLE_RATE,
                SpatialAudioPreset::Realistic,
            ),
            crash_breaker: CrashCircuitBreaker::new(),
            latest_season: None,
            disabled_subsystems: BTreeSet::new(),
            report_env,
            crash_dir,
        }
    }

    /// Route one custom-payload frame through
    /// [`decode_megu_custom_payload`]. Returns `true` when the frame
    /// decoded to a known "megu" payload; `latest_season` is replaced on
    /// [`MeguCustomPayload::Season`]. Malformed frames and unrecognized
    /// channels fail closed with `false` and leave session state untouched.
    pub fn handle_megu_custom_payload(&mut self, channel: &str, data: &[u8]) -> bool {
        match decode_megu_custom_payload(channel, data) {
            Some(MeguCustomPayload::Season(sync)) => {
                self.latest_season = Some(sync);
                true
            }
            Some(_) => true,
            None => false,
        }
    }

    /// Evaluate the binaural mix parameters for one sound event at
    /// `source_pos` heard by `listener` inside `world`. Runs the same
    /// [`AcousticTracer`] the engine's `analyze` half uses — at this
    /// engine's preset and sample rate — without disturbing live DSP
    /// state, so it is safe to call for speculative or off-voice queries.
    pub fn evaluate_sound_event(
        &self,
        source_pos: [f32; 3],
        listener: &ListenerPose,
        world: &dyn VoxelAcousticWorld,
    ) -> BinauralMixParams {
        let scene = AcousticTracer::new(world, self.spatial_audio.preset()).compute_scene(
            source_pos,
            listener.position,
            self.spatial_audio.sample_rate(),
        );
        let (azimuth_rad, elevation_rad, distance_m) = listener_relative(source_pos, listener);
        let occlusion = &scene.occlusion;
        let rolloff = if distance_m > 1.0 {
            1.0 / distance_m
        } else {
            1.0
        };
        let occ_linear = 10.0_f32
            .powf(-occlusion.transmission_loss_db / 20.0)
            .max(0.001);
        let direct_gain = (rolloff * occ_linear).clamp(0.0, 1.0);
        let head = listener.position;
        let submerged = listener.submerged
            || matches!(
                world
                    .acoustic_material(
                        head[0].floor() as i32,
                        head[1].floor() as i32,
                        head[2].floor() as i32,
                    )
                    .class,
                MaterialClass::Liquid
            );
        BinauralMixParams {
            azimuth_rad,
            elevation_rad,
            distance_m,
            direct_gain,
            occlusion_cutoff_hz: occlusion.cutoff_hz,
            blocked: occlusion.blocked,
            wet_send: scene.wet_gain,
            rt60_mid_s: scene.rt60_mid_s,
            submerged,
        }
    }

    /// Run one subsystem boundary inside a [`CrashGuard`] containment
    /// boundary, driven by this session's [`CrashCircuitBreaker`].
    ///
    /// * Returns `Some(R)` when the boundary completed; the breaker notes a
    ///   healthy tick.
    /// * On panic the boundary is caught, a redacted crash report is
    ///   captured and persisted under `crash_dir`, and the crash is
    ///   recorded in `crash_breaker`. While the breaker keeps allowing
    ///   restarts the subsystem stays enabled; once the breaker trips,
    ///   `subsystem` alone is disabled — later calls for it return `None`
    ///   immediately while the session and every other subsystem stay
    ///   alive.
    /// * `tick` is the caller-injected millisecond clock the breaker
    ///   windowing runs on; this method performs no wall-clock reads.
    pub fn run_guarded_subsystem<F, R>(
        &mut self,
        subsystem: GuardedSubsystem,
        tick: u64,
        f: F,
    ) -> Option<R>
    where
        F: FnOnce() -> R,
    {
        if self.disabled_subsystems.contains(&subsystem) {
            return None;
        }
        let mut env = self.report_env.clone();
        env.subsystem = Some(subsystem.label().to_string());
        // Run the boundary with the session's breaker moved into the guard,
        // then take it back so crash history accumulates on the session.
        let mut guard = CrashGuard::with_breaker(
            env,
            self.crash_dir.clone(),
            std::mem::take(&mut self.crash_breaker),
        );
        let outcome = guard.run_contained(tick, f);
        self.crash_breaker = std::mem::take(guard.breaker_mut());
        match outcome {
            ContainmentOutcome::Completed(value) => Some(value),
            ContainmentOutcome::Recovered { action, .. } => {
                if matches!(action, RecoveryAction::ReturnToCrashScreen { .. }) {
                    self.disabled_subsystems.insert(subsystem);
                }
                None
            }
        }
    }

    /// Whether `subsystem` has been disabled after its breaker tripped.
    pub fn is_subsystem_disabled(&self, subsystem: GuardedSubsystem) -> bool {
        self.disabled_subsystems.contains(&subsystem)
    }
}

/// Listener-space spherical geometry (`azimuth`, `elevation`, `distance`),
/// using the same convention as `SpatialAudioEngine`: `yaw_deg = 0` faces
/// +Z, positive yaw rotates toward -X, positive azimuth is to the right.
fn listener_relative(source_pos: [f32; 3], listener: &ListenerPose) -> (f32, f32, f32) {
    let d = [
        source_pos[0] - listener.position[0],
        source_pos[1] - listener.position[1],
        source_pos[2] - listener.position[2],
    ];
    let dist = (d[0] * d[0] + d[1] * d[1] + d[2] * d[2]).sqrt();
    if !dist.is_finite() || dist < 1e-6 {
        return (0.0, 0.0, 0.0);
    }
    let u = [d[0] / dist, d[1] / dist, d[2] / dist];
    let (sy, cy) = listener.yaw_deg.to_radians().sin_cos();
    let (sp, cp) = listener.pitch_deg.to_radians().sin_cos();
    let fwd = [cp * sy, sp, cp * cy];
    let right_raw = [
        fwd[1] * 0.0 - fwd[2] * 1.0,
        fwd[2] * 0.0 - fwd[0] * 0.0,
        fwd[0] * 1.0 - fwd[1] * 0.0,
    ];
    let right_norm =
        (right_raw[0] * right_raw[0] + right_raw[1] * right_raw[1] + right_raw[2] * right_raw[2])
            .sqrt();
    let right = if right_norm < 1e-6 {
        [cy, 0.0, -sy]
    } else {
        [
            right_raw[0] / right_norm,
            right_raw[1] / right_norm,
            right_raw[2] / right_norm,
        ]
    };
    let up = [
        right[1] * fwd[2] - right[2] * fwd[1],
        right[2] * fwd[0] - right[0] * fwd[2],
        right[0] * fwd[1] - right[1] * fwd[0],
    ];
    let rel_x = u[0] * right[0] + u[1] * right[1] + u[2] * right[2];
    let rel_y = u[0] * up[0] + u[1] * up[1] + u[2] * up[2];
    let rel_z = u[0] * fwd[0] + u[1] * fwd[1] + u[2] * fwd[2];
    (rel_x.atan2(rel_z), rel_y.clamp(-1.0, 1.0).asin(), dist)
}

// ---------------------------------------------------------------------------
// Live sound mixer: wire events -> voices -> limited stereo bus
// ---------------------------------------------------------------------------

/// Maximum pending sound events in the mixer's typed queue.
const SOUND_QUEUE_CAP: usize = 128;
/// Maximum simultaneously sounding voices.
const VOICE_CAP: usize = 24;
/// Maximum retained subtitle cues for consumers that drain them late.
const SUBTITLE_LOG_CAP: usize = 256;

/// A decoded, asset-resolved sound event ready to be voiced — the typed
/// queue element [`SoundMixer`] consumes. `pcm` is mono `f32` at the
/// mixer's sample rate.
#[derive(Clone, Debug)]
pub struct PlaySoundEvent {
    /// Display/limiter label — the sound-event name (or `sound.<id>`).
    pub label: String,
    /// Mono PCM at the mixer sample rate (`Arc` so replays share decode).
    pub pcm: Arc<Vec<f32>>,
    /// `true` when `pcm` is the deterministic placeholder tone (no usable
    /// asset or decoder) — the stats keep this honest.
    pub is_placeholder: bool,
    /// Vanilla sound-source category varint (0=master .. 9=voice).
    pub category: i32,
    /// World position in blocks (`sound_effect`), or `None` for
    /// entity-bound events (`sound_entity`) — those play at the listener
    /// because this wire layer does not track entity positions.
    pub position: Option<[f32; 3]>,
    /// The entity the sound is attached to, when entity-bound.
    pub entity_id: Option<i32>,
    /// Packet volume × variant volume (0..=1 after clamping).
    pub volume: f32,
    /// Playback-rate multiplier (0.5..=2.0 after clamping).
    pub pitch: f32,
    /// Packet seed — retained for reporting; variant selection already ran.
    pub seed: i64,
}

/// Mixer counters — the honest accounting of what played, what was
/// dropped, and why.
#[derive(Clone, Debug, Default)]
pub struct AudioStats {
    /// Sound events submitted to the mixer queue.
    pub events_submitted: u64,
    /// Events the limiter suppressed outright (same-sound storm).
    pub events_suppressed: u64,
    /// Events admitted at reduced gain by the limiter.
    pub events_attenuated: u64,
    /// Voices actually started.
    pub voices_started: u64,
    /// Voices removed early by cap stealing.
    pub voices_stolen: u64,
    /// Voices removed early by Stop Sound.
    pub voices_stopped: u64,
    /// Voices that finished naturally.
    pub voices_finished: u64,
    /// `sound_entity` events played at the listener position because no
    /// entity-position tracking exists on this wire layer.
    pub entity_pos_unresolved: u64,
    /// Registry ids that did not map to a known event name.
    pub unknown_sound_id: u64,
    /// Events with no `sounds.json` definition.
    pub missing_event_def: u64,
    /// Events handled with no asset library loaded.
    pub missing_assets: u64,
    /// Voices playing the placeholder tone instead of a real OGG.
    pub placeholder_voices: u64,
    /// OGG decode failures after a successful resolve.
    pub decode_errors: u64,
    /// Events dropped when the submit queue was full.
    pub queue_dropped: u64,
    /// Subtitle cues emitted.
    pub subtitle_cues: u64,
}

/// One subtitle record: label + direction/loudness cue + the mix position
/// (in output samples) when the sound started.
#[derive(Clone, Debug)]
pub struct SubtitleCueRecord {
    /// Sound-event name (e.g. `entity.player.levelup`).
    pub name: String,
    /// Direction + loudness + distance from the voice's engine.
    pub cue: SpatialSubtitleCue,
    /// Absolute output-sample index when the voice started.
    pub at_sample: u64,
}

/// One sounding voice: its own [`SpatialAudioEngine`] (per the engine's
/// one-voice-per-source contract), the mono PCM cursor, and mix gain.
struct Voice {
    engine: SpatialAudioEngine,
    pcm: Arc<Vec<f32>>,
    /// Fractional read position into `pcm` (pitch stepping).
    cursor: f64,
    /// Samples advanced per output sample (= clamped pitch).
    step: f64,
    /// Final mix gain: volume × category × master × admission.
    gain: f32,
    /// Steal metric — the voice's submitted loudness.
    loudness: f32,
    label: String,
    category: i32,
    order: u64,
    done: bool,
}

impl Voice {
    /// Fill `out` with this voice's next mono samples (zeros past the end);
    /// linear interpolation for the fractional `pitch` cursor.
    fn render_mono(&mut self, out: &mut [f32]) {
        let len = self.pcm.len();
        for s in out.iter_mut() {
            let i = self.cursor as usize;
            if i + 1 < len {
                let frac = (self.cursor - i as f64) as f32;
                *s = self.pcm[i] * (1.0 - frac) + self.pcm[i + 1] * frac;
            } else {
                *s = 0.0;
            }
            self.cursor += self.step;
        }
        if self.cursor as usize >= len.saturating_sub(1) {
            self.done = true;
        }
    }
}

/// The live mixer: a bounded typed event queue, per-voice spatialization
/// through [`SpatialAudioEngine`], category/master volumes, [`AudioLimiter`]
/// admission and bus limiting, a voice cap with deterministic
/// quietest-then-oldest stealing, and Stop Sound handling.
///
/// Deterministic: no wall-clock reads and no RNG anywhere in this type —
/// `render` output is a pure function of submitted events, listener poses,
/// the attached world view, and render calls.
pub struct SoundMixer {
    queue: VecDeque<PlaySoundEvent>,
    voices: Vec<Voice>,
    /// Admission limiter (`qol.max_volume_cap`,
    /// `qol.max_concurrent_same_sound`) — `pub` for direct configuration.
    pub limiter: AudioLimiter,
    listener: ListenerPose,
    world: Box<dyn VoxelAcousticWorld>,
    preset: SpatialAudioPreset,
    sample_rate: u32,
    voice_cap: usize,
    master_gain: f32,
    category_gain: [f32; 10],
    order: u64,
    render_calls: u64,
    samples_rendered: u64,
    /// Public counters — see [`AudioStats`].
    pub stats: AudioStats,
    subtitles: VecDeque<SubtitleCueRecord>,
}

impl SoundMixer {
    /// Mixer at `sample_rate` with `preset` quality (the engine voices each
    /// create their own [`SpatialAudioEngine`] at these settings).
    pub fn new(sample_rate: u32, preset: SpatialAudioPreset) -> Self {
        Self {
            queue: VecDeque::new(),
            voices: Vec::new(),
            limiter: AudioLimiter::new(),
            listener: ListenerPose::default(),
            world: Box::new(OpenAirWorld),
            preset,
            sample_rate,
            voice_cap: VOICE_CAP,
            master_gain: 1.0,
            category_gain: [1.0; 10],
            order: 0,
            render_calls: 0,
            samples_rendered: 0,
            stats: AudioStats::default(),
            subtitles: VecDeque::new(),
        }
    }

    /// Mixer output rate.
    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    /// Attach the acoustic world occlusion/reverb is traced against
    /// (default: [`OpenAirWorld`], i.e. clear line of sight everywhere).
    pub fn set_world(&mut self, world: Box<dyn VoxelAcousticWorld>) {
        self.world = world;
    }

    /// Update the listener pose — cheap geometry refresh on every voice.
    pub fn set_listener(&mut self, listener: ListenerPose) {
        self.listener = listener;
        for voice in &mut self.voices {
            voice.engine.set_listener(listener);
        }
    }

    /// Per-category gain (`0..=9` vanilla sound-source ids), clamped 0..=1.
    pub fn set_category_gain(&mut self, category: i32, gain: f32) {
        if let Some(slot) = self.category_gain.get_mut(category as usize) {
            *slot = gain.clamp(0.0, 1.0);
        }
    }

    /// Master gain applied to every voice, clamped 0..=1.
    pub fn set_master_gain(&mut self, gain: f32) {
        self.master_gain = gain.clamp(0.0, 1.0);
    }

    /// Live voice count (test/diagnostic).
    pub fn voice_count(&self) -> usize {
        self.voices.len()
    }

    /// Labels of live voices in insertion order (diagnostics/tests).
    pub fn voice_labels(&self) -> Vec<String> {
        self.voices.iter().map(|v| v.label.clone()).collect()
    }

    /// Queue one event. The queue is bounded; a flooded session drops the
    /// oldest pending event rather than growing without limit.
    pub fn submit(&mut self, ev: PlaySoundEvent) {
        if self.queue.len() >= SOUND_QUEUE_CAP {
            self.queue.pop_front();
            self.stats.queue_dropped += 1;
        }
        self.queue.push_back(ev);
    }

    /// Stop Sound: remove queued and sounding voices matching the filter —
    /// `None` fields match everything (mirrors the packet's flag bits).
    pub fn submit_stop(&mut self, category: Option<i32>, name: Option<&str>) {
        let name = name.map(normalize_sound_name);
        let matches = |v: &Voice| {
            category.is_none_or(|c| c == v.category)
                && name.is_none_or(|n| n == normalize_sound_name(&v.label))
        };
        let before = self.voices.len();
        self.voices.retain(|v| !matches(v));
        self.stats.voices_stopped += (before - self.voices.len()) as u64;
        self.queue.retain(|ev| {
            !(category.is_none_or(|c| c == ev.category)
                && name.is_none_or(|n| n == normalize_sound_name(&ev.label)))
        });
    }

    /// The voice selected to make room for a new one: quietest submitted
    /// loudness, ties broken by oldest start order — deterministic.
    fn steal_index(&self) -> Option<usize> {
        self.voices
            .iter()
            .enumerate()
            .min_by(|(_, a), (_, b)| {
                a.loudness
                    .partial_cmp(&b.loudness)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then(a.order.cmp(&b.order))
            })
            .map(|(i, _)| i)
    }

    /// Admit one queued event into a voice.
    fn admit(&mut self, ev: PlaySoundEvent) {
        let position = ev.position.unwrap_or(self.listener.position);
        let key = SoundKey::at_block(
            sound_name_hash(&ev.label),
            position[0].floor() as i32,
            position[1].floor() as i32,
            position[2].floor() as i32,
        );
        let admission_gain = match self.limiter.register_sound(key, self.render_calls) {
            SoundAdmission::Play => 1.0,
            SoundAdmission::Attenuate { gain } => {
                self.stats.events_attenuated += 1;
                gain
            }
            SoundAdmission::Suppress => {
                self.stats.events_suppressed += 1;
                return;
            }
        };
        if self.voices.len() >= self.voice_cap {
            if let Some(idx) = self.steal_index() {
                self.voices.remove(idx);
                self.stats.voices_stolen += 1;
            }
        }
        let cat_gain = self
            .category_gain
            .get(ev.category as usize)
            .copied()
            .unwrap_or(1.0);
        let gain = (ev.volume * cat_gain * self.master_gain * admission_gain).clamp(0.0, 1.0);
        let mut engine = SpatialAudioEngine::new(self.sample_rate, self.preset);
        engine.analyze(
            &*self.world,
            &SoundSource {
                position,
                velocity: [0.0; 3],
            },
            &self.listener,
        );
        self.subtitles.push_back(SubtitleCueRecord {
            name: ev.label.clone(),
            cue: engine.subtitle_cue(),
            at_sample: self.samples_rendered,
        });
        self.stats.subtitle_cues += 1;
        while self.subtitles.len() > SUBTITLE_LOG_CAP {
            self.subtitles.pop_front();
        }
        self.order += 1;
        self.voices.push(Voice {
            engine,
            pcm: ev.pcm,
            cursor: 0.0,
            step: f64::from(ev.pitch.clamp(0.5, 2.0)),
            gain,
            loudness: ev.volume,
            label: ev.label,
            category: ev.category,
            order: self.order,
            done: gain <= 0.0,
        });
        self.stats.voices_started += 1;
    }

    /// Render `out.len()/2` stereo frames into the interleaved buffer,
    /// applying the bus limiter last so the output never exceeds the cap.
    /// `out` is NOT cleared — it is expected zeroed and is summed into, so
    /// callers render layers; the limiter still runs on the final mix.
    pub fn render(&mut self, out: &mut [f32]) {
        debug_assert!(out.len().is_multiple_of(2));
        let frames = out.len() / 2;
        if frames == 0 {
            return;
        }
        self.render_calls += 1;
        while let Some(ev) = self.queue.pop_front() {
            self.stats.events_submitted += 1;
            self.admit(ev);
        }
        let mut mono = vec![0.0f32; frames];
        for voice in &mut self.voices {
            mono.fill(0.0);
            voice.render_mono(&mut mono);
            let stereo = voice.engine.process_voice_frame(&mono);
            for i in 0..frames {
                out[2 * i] += stereo[2 * i] * voice.gain;
                out[2 * i + 1] += stereo[2 * i + 1] * voice.gain;
            }
        }
        let finished = self.voices.iter().filter(|v| v.done).count() as u64;
        self.stats.voices_finished += finished;
        self.voices.retain(|v| !v.done);
        AudioLimiter::limit_mix_buffer(out, self.limiter.max_volume_cap());
        self.samples_rendered += frames as u64;
    }

    /// Pop the oldest pending subtitle cue.
    pub fn drain_subtitle(&mut self) -> Option<SubtitleCueRecord> {
        self.subtitles.pop_front()
    }
}

/// Acoustic world with air everywhere — the default when no voxel view is
/// attached (clear line of sight, no reverb).
pub struct OpenAirWorld;

impl VoxelAcousticWorld for OpenAirWorld {
    fn acoustic_material(&self, _x: i32, _y: i32, _z: i32) -> AcousticMaterial {
        AcousticMaterial::AIR
    }
}

/// Read-only acoustic view over the live [`WorldState`] section cache.
/// Unloaded cells report air; fluids report [`MaterialClass::Liquid`];
/// other solid states report [`MaterialClass::StoneLike`] — the block
/// registry on this layer cannot name individual blocks, so material
/// detail within "solid" is approximated.
pub struct WorldAcousticView<'a> {
    /// The world being viewed.
    pub world: &'a WorldState,
}

impl VoxelAcousticWorld for WorldAcousticView<'_> {
    fn acoustic_material(&self, x: i32, y: i32, z: i32) -> AcousticMaterial {
        match self.world.get_block_state(x, y, z) {
            Some(state) if self.world.blocks.is_air(state) => AcousticMaterial::AIR,
            Some(state) if self.world.blocks.fluid_cell(state).kind.is_some() => {
                AcousticMaterial::from_class(MaterialClass::Liquid)
            }
            Some(_) => AcousticMaterial::from_class(MaterialClass::StoneLike),
            None => AcousticMaterial::AIR,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{CHANNEL_SEASON_V1, SSN1_MAGIC, SSN1_VERSION};
    use crate::spatial_audio::{AcousticMaterial, DenseVoxelGrid};

    fn test_env() -> ReportEnv {
        ReportEnv {
            mc_version: "26.3".to_string(),
            engine_version: "mc-spine-client 0.1.0".to_string(),
            os: "linux-x86_64".to_string(),
            culprit: "mc-spine-client".to_string(),
            subsystem: None,
            stage: None,
            gpu_class: None,
            top_frames: Vec::new(),
            mod_count: None,
        }
    }

    fn scratch_dir(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("mc-spine-session-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn session(tag: &str) -> ClientSession {
        ClientSession::new(test_env(), scratch_dir(tag))
    }

    fn ssn1_frame(sub_season_idx: u8, flags: u8) -> Vec<u8> {
        let mut frame = Vec::with_capacity(19);
        frame.extend_from_slice(SSN1_MAGIC);
        frame.push(SSN1_VERSION);
        frame.push(sub_season_idx);
        frame.extend_from_slice(&32_767u16.to_be_bytes());
        frame.extend_from_slice(&42u16.to_be_bytes());
        frame.extend_from_slice(&49_151u16.to_be_bytes());
        frame.extend_from_slice(&[255, 200, 150]);
        frame.extend_from_slice(&[90, 200, 90]);
        frame.push(flags);
        frame
    }

    #[test]
    fn ssn1_payload_updates_latest_season_and_corrupt_frames_fail_closed() {
        let mut session = session("season");
        assert!(session.latest_season.is_none());

        assert!(session.handle_megu_custom_payload(CHANNEL_SEASON_V1, &ssn1_frame(9, 0b011)));
        let sync = session.latest_season.clone().unwrap();
        assert_eq!(sync.sub_season_idx, 9);
        assert_eq!(sync.day_of_season, 42);
        assert!(sync.tropical_dim && sync.frozen_season && !sync.snow_enabled);

        // A newer valid frame replaces the state.
        assert!(session.handle_megu_custom_payload(CHANNEL_SEASON_V1, &ssn1_frame(0, 0)));
        assert_eq!(session.latest_season.as_ref().unwrap().sub_season_idx, 0);

        // Corrupt and unrecognized frames are ignored, state untouched.
        for bad in [
            ssn1_frame(12, 0)[..18].to_vec(),
            ssn1_frame(12, 0),
            ssn1_frame(3, 0b1111_0000),
        ] {
            assert!(!session.handle_megu_custom_payload(CHANNEL_SEASON_V1, &bad));
            assert!(!session.handle_megu_custom_payload("megu:season", &bad));
        }
        assert!(!session.handle_megu_custom_payload("example:unknown", b"abc"));
        assert_eq!(session.latest_season.as_ref().unwrap().sub_season_idx, 0);
    }

    #[test]
    fn evaluate_sound_event_returns_finite_binaural_parameters() {
        let session = session("audio");
        let world = DenseVoxelGrid::new([-8, -8, -8], [16, 16, 16], AcousticMaterial::AIR);
        let listener = ListenerPose {
            position: [0.0, 0.0, 0.0],
            yaw_deg: 0.0,
            pitch_deg: 0.0,
            velocity: [0.0; 3],
            submerged: false,
        };
        // Facing +Z (yaw 0), the right ear points -X: a source at -X sits
        // on the listener's right (positive azimuth).
        let params = session.evaluate_sound_event([-4.0, 0.0, 0.0], &listener, &world);
        assert!(params.azimuth_rad.is_finite() && params.azimuth_rad > 0.0);
        assert!(params.elevation_rad.is_finite());
        assert_eq!(params.distance_m, 4.0);
        assert!((0.0..=1.0).contains(&params.direct_gain));
        assert!(params.occlusion_cutoff_hz.is_finite());
        assert!(params.wet_send.is_finite());
        assert!(params.rt60_mid_s.is_finite());
        assert!(!params.submerged);
    }

    #[test]
    fn guarded_subsystem_contains_panic_and_trips_only_that_subsystem() {
        let mut session = session("guard");
        // Trip on the second crash inside the window regardless of panic
        // fingerprinting, so the trip rule under test is deterministic.
        session.crash_breaker = CrashCircuitBreaker::with_thresholds(2, 60_000, 5, 30_000);
        let mut calls = 0u32;

        // First panic: contained, recorded, subsystem still enabled.
        assert_eq!(
            session.run_guarded_subsystem(GuardedSubsystem::SeasonSync, 1_000, || {
                calls += 1;
                panic!("simulated season-sync panic");
            }),
            None::<()>
        );
        assert_eq!(calls, 1);
        assert_eq!(session.crash_breaker.attempts(), 1);
        assert!(!session.crash_breaker.is_tripped());
        assert!(!session.is_subsystem_disabled(GuardedSubsystem::SeasonSync));

        // Identical second panic trips the breaker and disables only
        // SeasonSync.
        assert!(session
            .run_guarded_subsystem(GuardedSubsystem::SeasonSync, 1_100, || {
                panic!("simulated season-sync panic");
            })
            .is_none());
        assert!(session.crash_breaker.is_tripped());
        assert!(session.is_subsystem_disabled(GuardedSubsystem::SeasonSync));

        // The disabled subsystem is never invoked again; the session and
        // every other subsystem stay alive.
        assert!(session
            .run_guarded_subsystem(GuardedSubsystem::SeasonSync, 1_200, || {
                calls += 1;
            })
            .is_none());
        assert_eq!(calls, 1);
        assert_eq!(
            session.run_guarded_subsystem(GuardedSubsystem::SpatialAudio, 1_300, || 42u32),
            Some(42)
        );
    }
}
