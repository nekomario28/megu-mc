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

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use crate::audio_qol::{AudioLimiter, SoundAdmission, SoundKey};
use crate::cli::CliConfig;
use crate::crash_reporter::ReportEnv;
use crate::online_auth::OnlineAuthOptions;
use crate::protocol::{decode_megu_custom_payload, MeguCustomPayload, SeasonWireSync};
use crate::recovery::{ContainmentOutcome, CrashCircuitBreaker, CrashGuard, RecoveryAction};
use crate::sound_assets::{normalize_sound_name, sound_name_hash};
use crate::spatial_audio::{
    AcousticMaterial, AcousticTracer, ListenerPose, MaterialClass, SharedListenerFdnBus,
    SoundSource, SpatialAudioEngine, SpatialAudioPreset, SpatialSubtitleCue, VoxelAcousticWorld,
};
use crate::world::WorldState;
use mc_spine_client_mesh::tactile_effects::EnvironmentAudioHint;

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

/// Resolve the connect-time online-mode credentials for a live session: the
/// launcher's `--accessToken` (only consumed when the server's Encryption
/// Request carries `should_authenticate`), the default Mojang session-join
/// client, and the OS CSPRNG. The token is copied into connect state here —
/// the one place CLI secrets become wire material — and is never logged.
pub fn online_auth_options(cfg: &CliConfig) -> OnlineAuthOptions {
    OnlineAuthOptions {
        access_token: cfg.access_token().map(str::to_string),
        profile_uuid: cfg
            .uuid
            .as_deref()
            .and_then(|raw| crate::protocol::parse_uuid(raw).ok()),
        session_join: None,
        rng: None,
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

/// Debug snapshot of one live [`Voice`]:
/// `(label, entity_id, position, velocity, spatialized)`.
pub type VoiceDebugInfo = (String, Option<i32>, [f32; 3], [f32; 3], bool);

/// A decoded, asset-resolved sound event ready to be voiced — the typed
/// queue element [`SoundMixer`] consumes. `pcm` is mono `f32` at the
/// mixer's sample rate.
#[derive(Clone, Debug)]
pub struct PlaySoundEvent {
    /// Display/limiter label — the sound-event name (or `sound.<id>`).
    pub label: String,
    /// Mono PCM at the mixer sample rate (`Arc` so replays share decode).
    pub pcm: Arc<Vec<f32>>,
    /// Original stereo frames when the asset shipped stereo — used only by
    /// 2D-bypassed voices (Music/UI/global); 3D sounds always spatialize
    /// the mono downmix so a stereo Mod `.ogg` never leaks raw L/R into the
    /// world mix.
    pub stereo_pcm: Option<Arc<Vec<[f32; 2]>>>,
    /// `true` when `pcm` is the deterministic placeholder tone (no usable
    /// asset or decoder) — the stats keep this honest.
    pub is_placeholder: bool,
    /// Vanilla sound-source category varint (0=master .. 9=voice).
    pub category: i32,
    /// World position in blocks (`sound_effect`), or `None` for
    /// entity-bound events (`sound_entity`) — those follow the live entity
    /// position map, falling back to the listener until it resolves.
    pub position: Option<[f32; 3]>,
    /// The entity the sound is attached to, when entity-bound.
    pub entity_id: Option<i32>,
    /// Packet volume × variant volume. May exceed `1.0` — Vanilla widens the
    /// audible range (`attenuation_distance * max(volume, 1)`) while output
    /// gain still clamps at `min(volume, 1)`.
    pub volume: f32,
    /// Playback-rate multiplier (0.5..=2.0 after clamping).
    pub pitch: f32,
    /// Packet seed — retained for reporting; variant selection already ran.
    pub seed: i64,
    /// Vanilla audible range in blocks
    /// (`attenuation_distance * max(volume, 1.0)`, or the server's explicit
    /// `range` override); `f32::INFINITY` disables range fading.
    pub max_range_m: f32,
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

/// One sounding voice: either a [`SpatialAudioEngine`] spatial voice
/// (per the engine's one-voice-per-source contract) or a 2D stereo bypass
/// (Music/UI/global), the PCM cursor, and mix gain.
struct Voice {
    /// Spatializer — `Some` for world-positioned/entity voices, `None` for
    /// bypassed 2D sounds so they can never pick up cave reverb, underwater
    /// LP, or mono squash.
    engine: Option<SpatialAudioEngine>,
    /// Original stereo frames for bypassed voices.
    stereo: Option<Arc<Vec<[f32; 2]>>>,
    position: [f32; 3],
    /// Finite per-second source velocity tracked from entity positions —
    /// feeds the engine's Doppler term.
    source_velocity: [f32; 3],
    /// Entity this voice follows (`sound_entity`), if any.
    entity_id: Option<i32>,
    /// `samples_rendered` when `position` was last updated — the dt of the
    /// next finite velocity estimate.
    track_at_sample: u64,
    /// Block cell the source currently sits in; a crossing triggers a
    /// re-trace on `retrace` voices.
    cell: [i32; 3],
    /// `true` when the clip is long enough (>0.5 s) to benefit from
    /// cell-crossing re-traces.
    retrace: bool,
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

    /// 2D bypass render: stereo stays stereo, mono duplicates to both ears —
    /// no engine, no reverb send, no underwater filter. `out` is summed into
    /// like every other voice path.
    fn render_2d(&mut self, out: &mut [f32]) {
        let gain = self.gain;
        if let Some(stereo) = self.stereo.clone() {
            let len = stereo.len();
            for i in 0..out.len() / 2 {
                let idx = self.cursor as usize;
                if idx + 1 < len {
                    let frac = (self.cursor - idx as f64) as f32;
                    let a = stereo[idx];
                    let b = stereo[idx + 1];
                    out[2 * i] += (a[0] + (b[0] - a[0]) * frac) * gain;
                    out[2 * i + 1] += (a[1] + (b[1] - a[1]) * frac) * gain;
                }
                self.cursor += self.step;
            }
            if self.cursor as usize >= len.saturating_sub(1) {
                self.done = true;
            }
        } else {
            let len = self.pcm.len();
            for i in 0..out.len() / 2 {
                let idx = self.cursor as usize;
                if idx + 1 < len {
                    let frac = (self.cursor - idx as f64) as f32;
                    let s = self.pcm[idx] * (1.0 - frac) + self.pcm[idx + 1] * frac;
                    out[2 * i] += s * gain;
                    out[2 * i + 1] += s * gain;
                }
                self.cursor += self.step;
            }
            if self.cursor as usize >= len.saturating_sub(1) {
                self.done = true;
            }
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
    spatial_subtitles: VecDeque<SpatialSubtitleCue>,
    environment_hint: EnvironmentAudioHint,
    /// Live entity positions (blocks) fed by the world/entity-tracking
    /// layer — `sound_entity` voices follow these and derive their Doppler
    /// velocity from deltas.
    entity_positions: BTreeMap<i32, [f32; 3]>,
    /// Listener's last block cell — a crossing re-traces long voices.
    listener_cell: [i32; 3],
    /// One shared FDN reverb every active spatial voice feeds; the tail
    /// keeps decaying after short voices finish.
    reverb_bus: SharedListenerFdnBus,
    /// RT60 last applied to `reverb_bus` — elementwise max across active
    /// voice scenes, retained after the last voice ends so the tail decays
    /// with the room's real time constant.
    bus_rt60: [f32; 3],
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
            spatial_subtitles: VecDeque::new(),
            environment_hint: EnvironmentAudioHint::default(),
            entity_positions: BTreeMap::new(),
            listener_cell: [i32::MIN; 3],
            reverb_bus: SharedListenerFdnBus::new(sample_rate),
            bus_rt60: [0.0; 3],
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
        self.refresh_acoustics();
    }

    /// Update the listener pose — cheap geometry refresh on every voice.
    pub fn set_listener(&mut self, listener: ListenerPose) {
        self.listener = listener;
        self.refresh_acoustics();
    }

    /// Apply the tactile environment's cave, submersion and rain values to
    /// both new and currently playing spatial voices.
    pub fn set_environment_hint(&mut self, hint: EnvironmentAudioHint) {
        let sanitize = |value: f32| {
            if value.is_finite() {
                value.clamp(0.0, 1.0)
            } else {
                0.0
            }
        };
        let hint = EnvironmentAudioHint {
            underwater_lowpass: sanitize(hint.underwater_lowpass),
            cave_reverb_mix: sanitize(hint.cave_reverb_mix),
            rain_eaves_occlusion: sanitize(hint.rain_eaves_occlusion),
        };
        if self.environment_hint != hint {
            self.environment_hint = hint;
            self.refresh_acoustics();
        }
    }

    /// Last environment hint actually applied to the mix.
    pub fn environment_hint(&self) -> EnvironmentAudioHint {
        self.environment_hint
    }

    /// Recompute traced acoustic paths after a chunk/block or listener change.
    pub fn refresh_acoustics(&mut self) {
        let listener = self.acoustic_listener();
        let hint = self.environment_hint;
        for voice in &mut self.voices {
            if let Some(engine) = &mut voice.engine {
                Self::analyze_voice(
                    &*self.world,
                    listener,
                    hint,
                    engine,
                    voice.position,
                    voice.source_velocity,
                );
            }
        }
        self.update_bus_rt60();
    }

    fn acoustic_listener(&self) -> ListenerPose {
        let mut listener = self.listener;
        let head = listener.position.map(|value| value.floor() as i32);
        listener.submerged |= self.environment_hint.underwater_lowpass > 0.0
            || self
                .world
                .acoustic_material(head[0], head[1], head[2])
                .class
                == MaterialClass::Liquid;
        listener
    }

    fn analyze_voice(
        world: &dyn VoxelAcousticWorld,
        listener: ListenerPose,
        hint: EnvironmentAudioHint,
        engine: &mut SpatialAudioEngine,
        position: [f32; 3],
        velocity: [f32; 3],
    ) {
        engine.analyze(
            world,
            &SoundSource { position, velocity },
            &listener,
        );
        let mut scene = engine.scene().clone();
        scene.wet_gain =
            (scene.wet_gain + hint.cave_reverb_mix * (1.0 - scene.wet_gain)).clamp(0.0, 1.0);
        scene.rt60_low_s = scene.rt60_low_s.max(hint.cave_reverb_mix * 1.5);
        scene.rt60_mid_s = scene.rt60_mid_s.max(hint.cave_reverb_mix * 1.2);
        scene.rt60_high_s = scene.rt60_high_s.max(hint.cave_reverb_mix * 0.8);
        scene.occlusion.transmission_loss_db += hint.rain_eaves_occlusion * 6.0;
        engine.set_scene(scene);
        engine.set_listener(listener);
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

    /// Feed one entity's current position (blocks). `sound_entity` voices
    /// attached to `entity_id` follow it each render and derive a finite
    /// source velocity (and thus Doppler) from the position delta.
    pub fn set_entity_position(&mut self, entity_id: i32, pos: [f64; 3]) {
        let pos = [pos[0] as f32, pos[1] as f32, pos[2] as f32];
        if pos.iter().all(|v| v.is_finite()) {
            self.entity_positions.insert(entity_id, pos);
        }
    }

    /// Drop a tracked entity (despawn/remove_entities).
    pub fn remove_entity_position(&mut self, entity_id: i32) {
        self.entity_positions.remove(&entity_id);
    }

    /// Last reported position of `entity_id` (diagnostics/tests).
    pub fn entity_position(&self, entity_id: i32) -> Option<[f32; 3]> {
        self.entity_positions.get(&entity_id).copied()
    }

    /// First live voice labelled `label`'s Vanilla audible range (`None`
    /// when no such voice exists or it is a 2D-bypass voice).
    pub fn voice_max_range(&self, label: &str) -> Option<f32> {
        self.voices
            .iter()
            .find(|v| v.label == label)
            .and_then(|v| v.engine.as_ref().map(SpatialAudioEngine::max_range))
    }

    /// `(label, entity_id, position, velocity, spatialized)` per live voice —
    /// the entity-tracking test surface.
    pub fn voice_debug(&self) -> Vec<VoiceDebugInfo> {
        self.voices
            .iter()
            .map(|v| {
                (
                    v.label.clone(),
                    v.entity_id,
                    v.position,
                    v.source_velocity,
                    v.engine.is_some(),
                )
            })
            .collect()
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

    /// `true` when the event plays as a 2D stereo voice — Music, UI/global
    /// sounds, and non-positioned non-entity events never see the
    /// spatializer (no cave reverb, no underwater LP, no mono squash).
    fn is_2d_bypass(ev: &PlaySoundEvent) -> bool {
        ev.category == 1 // Music
            || normalize_sound_name(&ev.label).starts_with("ui.")
            || (ev.position.is_none() && ev.entity_id.is_none())
            || (ev.volume.is_finite() && ev.volume >= 100.0)
    }

    /// Update the shared listener FDN's RT60: the elementwise max across
    /// live voice scenes — every voice reverberates through the same room,
    /// and the last RT60 sticks after the final voice ends so the tail
    /// decays at the room's real time constant.
    fn update_bus_rt60(&mut self) {
        let mut rt = [0.0_f32; 3];
        for v in &self.voices {
            if let Some(engine) = &v.engine {
                let s = engine.scene();
                rt[0] = rt[0].max(s.rt60_low_s);
                rt[1] = rt[1].max(s.rt60_mid_s);
                rt[2] = rt[2].max(s.rt60_high_s);
            }
        }
        if rt.iter().any(|&r| r > 0.0) {
            self.bus_rt60 = rt;
        }
        self.reverb_bus.set_rt60(self.bus_rt60);
    }

    /// Follow live entity positions for attached voices: finite per-second
    /// velocity from the position delta (never NaN — non-finite or extreme
    /// estimates are clamped), then feed the engine so Doppler is live.
    fn track_entity_voices(&mut self) {
        let now = self.samples_rendered;
        let sr = f64::from(self.sample_rate);
        for voice in &mut self.voices {
            let Some(eid) = voice.entity_id else {
                continue;
            };
            let Some(&pos) = self.entity_positions.get(&eid) else {
                continue;
            };
            if pos == voice.position {
                continue;
            }
            let dt = now.saturating_sub(voice.track_at_sample).max(1) as f64;
            let mut vel = [0.0_f32; 3];
            for i in 0..3 {
                let v = ((f64::from(pos[i]) - f64::from(voice.position[i])) * sr / dt) as f32;
                vel[i] = if v.is_finite() {
                    v.clamp(-256.0, 256.0)
                } else {
                    0.0
                };
            }
            voice.source_velocity = vel;
            voice.position = pos;
            voice.track_at_sample = now;
            if let Some(engine) = &mut voice.engine {
                engine.set_source(SoundSource {
                    position: pos,
                    velocity: vel,
                });
            }
        }
    }

    /// Re-trace long voices when the source or the listener crossed a block
    /// cell boundary — cheap voices keep their original trace.
    fn retrace_moved_voices(&mut self) {
        let cell = self.listener.position.map(|v| v.floor() as i32);
        let listener_crossed = cell != self.listener_cell;
        self.listener_cell = cell;
        if !listener_crossed && !self.voices.iter().any(|v| v.retrace) {
            return;
        }
        let listener = self.acoustic_listener();
        let hint = self.environment_hint;
        for voice in &mut self.voices {
            let cell = voice.position.map(|v| v.floor() as i32);
            let crossed = cell != voice.cell;
            voice.cell = cell;
            if voice.retrace && (crossed || listener_crossed) {
                if let Some(engine) = &mut voice.engine {
                    Self::analyze_voice(
                        &*self.world,
                        listener,
                        hint,
                        engine,
                        voice.position,
                        voice.source_velocity,
                    );
                }
            }
        }
    }

    /// Admit one queued event into a voice.
    fn admit(&mut self, ev: PlaySoundEvent) {
        let position = ev
            .position
            .or_else(|| {
                ev.entity_id
                    .and_then(|eid| self.entity_positions.get(&eid).copied())
            })
            .unwrap_or(self.listener.position);
        let bypass_2d = Self::is_2d_bypass(&ev);
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
        // Vanilla: `volume > 1.0` widens the audible range but output gain
        // still saturates at 1.0 — `min`, not `clamp(0,1)` up front.
        let volume_out = if ev.volume.is_finite() {
            ev.volume.clamp(0.0, 1.0)
        } else {
            0.0
        };
        let gain = (volume_out * cat_gain * self.master_gain * admission_gain).clamp(0.0, 1.0);
        let engine = if bypass_2d {
            None
        } else {
            let mut engine = SpatialAudioEngine::new(self.sample_rate, self.preset);
            engine.set_max_range(ev.max_range_m);
            Self::analyze_voice(
                &*self.world,
                self.acoustic_listener(),
                self.environment_hint,
                &mut engine,
                position,
                [0.0; 3],
            );
            Some(engine)
        };
        let mut cue = engine
            .as_ref()
            .map(SpatialAudioEngine::subtitle_cue)
            .unwrap_or(SpatialSubtitleCue {
                direction: crate::spatial_audio::SubtitleDirection::Center,
                loudness: gain,
                distance_m: 0.0,
                label: String::new(),
            });
        cue.label = ev.label.clone();
        self.subtitles.push_back(SubtitleCueRecord {
            name: ev.label.clone(),
            cue: cue.clone(),
            at_sample: self.samples_rendered,
        });
        self.spatial_subtitles.push_back(cue);
        self.stats.subtitle_cues += 1;
        while self.subtitles.len() > SUBTITLE_LOG_CAP {
            self.subtitles.pop_front();
        }
        while self.spatial_subtitles.len() > SUBTITLE_LOG_CAP {
            self.spatial_subtitles.pop_front();
        }
        self.order += 1;
        self.voices.push(Voice {
            engine,
            stereo: if bypass_2d { ev.stereo_pcm } else { None },
            position,
            source_velocity: [0.0; 3],
            entity_id: ev.entity_id,
            track_at_sample: self.samples_rendered,
            cell: position.map(|v| v.floor() as i32),
            retrace: ev.pcm.len() > (self.sample_rate / 2) as usize,
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
        self.update_bus_rt60();
        self.track_entity_voices();
        self.retrace_moved_voices();
        let mut mono = vec![0.0f32; frames];
        // Per-sample summed reverb send fed by every spatial voice's
        // `s * wet_send` (scaled by voice gain, matching the old per-voice
        // FDN semantics where the wet output was gain-scaled downstream).
        let mut send = vec![0.0f32; frames];
        for voice in &mut self.voices {
            if voice.engine.is_some() {
                mono.fill(0.0);
                voice.render_mono(&mut mono);
                let mut stereo = Vec::with_capacity(frames * 2);
                let gain = voice.gain;
                if let Some(engine) = &mut voice.engine {
                    engine.process_shared(&mono, &mut stereo, &mut send, gain);
                }
                for i in 0..frames {
                    out[2 * i] += stereo[2 * i] * gain;
                    out[2 * i + 1] += stereo[2 * i + 1] * gain;
                }
            } else {
                voice.render_2d(out);
            }
        }
        // The shared bus runs even with zero active voices so an ending
        // tail finishes decaying instead of cutting off.
        for i in 0..frames {
            let (wet_l, wet_r) = self.reverb_bus.step_input(send[i]);
            out[2 * i] += wet_l;
            out[2 * i + 1] += wet_r;
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

    /// Drain the independent spatial cue feed for interactive UI clients.
    pub fn drain_spatial_subtitle_cues(&mut self) -> Vec<SpatialSubtitleCue> {
        self.spatial_subtitles.drain(..).collect()
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
/// every other state resolves through the registry's
/// [`crate::spatial_audio::AcousticMaterialLut`], so wool/wood/glass/metal
/// and unknown Mod blocks keep their real acoustic behaviour.
pub struct WorldAcousticView<'a> {
    /// The world being viewed.
    pub world: &'a WorldState,
}

impl VoxelAcousticWorld for WorldAcousticView<'_> {
    fn acoustic_material(&self, x: i32, y: i32, z: i32) -> AcousticMaterial {
        match self.world.get_block_state(x, y, z) {
            Some(state) => acoustic_material_for_state(self.world, state),
            None => AcousticMaterial::AIR,
        }
    }
}

/// O(1) material lookup for one block state: air and fluids short-circuit
/// (`Liquid` carries its own acoustic coefficients), every other state reads
/// the registry-name LUT the [`crate::world::BlockTable`] built at load — so
/// wool, planks, glass, iron, and unknown Mod blocks
/// (`create:brass_casing`, `custom:stained_glass`) all report their real
/// acoustic class instead of collapsing to `StoneLike`.
fn acoustic_material_for_state(world: &WorldState, state: u32) -> AcousticMaterial {
    if world.blocks.is_air(state) {
        AcousticMaterial::AIR
    } else if world.blocks.fluid_cell(state).kind.is_some() {
        AcousticMaterial::from_class(MaterialClass::Liquid)
    } else {
        world.blocks.acoustic_lut().get(state)
    }
}

/// Owned, shared acoustic cache of decoded real-world sections. One byte per
/// occupied voxel keeps raytracing independent of mutable mesh/world state.
#[derive(Clone)]
pub struct WorldAcousticSnapshot {
    inner: Arc<RwLock<AcousticSnapshotInner>>,
}

type AcousticColumns = BTreeMap<(i32, i32), Vec<Option<Box<[u8]>>>>;

struct AcousticSnapshotInner {
    min_y: i32,
    height: i32,
    columns: AcousticColumns,
}

impl WorldAcousticSnapshot {
    pub fn new(world: &WorldState) -> Self {
        Self {
            inner: Arc::new(RwLock::new(AcousticSnapshotInner {
                min_y: world.dimension.min_y,
                height: world.dimension.height,
                columns: BTreeMap::new(),
            })),
        }
    }

    /// Clear stale chunk materials when a dimension changes.
    pub fn reset_dimension(&self, world: &WorldState) {
        let mut inner = self.inner.write().unwrap_or_else(|e| e.into_inner());
        inner.min_y = world.dimension.min_y;
        inner.height = world.dimension.height;
        inner.columns.clear();
    }

    /// Replace a chunk's acoustic material cells after successful decoding.
    pub fn sync_chunk(&self, world: &WorldState, cx: i32, cz: i32) {
        let sections = world.columns.get(&(cx, cz)).map(|column| {
            column
                .sections
                .iter()
                .map(|section| {
                    if !section.has_non_air {
                        return None;
                    }
                    let cells: Box<[u8]> = (0..4096)
                        .map(|idx| {
                            let state = section.palette.get(idx).unwrap_or(0);
                            acoustic_material_for_state(world, state).class.code()
                        })
                        .collect::<Vec<_>>()
                        .into_boxed_slice();
                    Some(cells)
                })
                .collect::<Vec<_>>()
        });
        let mut inner = self.inner.write().unwrap_or_else(|e| e.into_inner());
        if let Some(sections) = sections {
            inner.columns.insert((cx, cz), sections);
        } else {
            inner.columns.remove(&(cx, cz));
        }
    }

    /// Update one block without re-copying the entire chunk on block edits.
    pub fn sync_block(&self, world: &WorldState, pos: [i32; 3]) {
        let Some(state) = world.get_block_state(pos[0], pos[1], pos[2]) else {
            return;
        };
        let class = acoustic_material_for_state(world, state).class.code();
        let mut inner = self.inner.write().unwrap_or_else(|e| e.into_inner());
        let sy = i64::from(pos[1]) - i64::from(inner.min_y);
        if sy < 0 || sy >= i64::from(inner.height) {
            return;
        }
        let Some(sections) = inner
            .columns
            .get_mut(&(pos[0].div_euclid(16), pos[2].div_euclid(16)))
        else {
            return;
        };
        if let Some(section) = sections.get_mut((sy / 16) as usize) {
            if section.is_none() && class != 0 {
                *section = Some(vec![0; 4096].into_boxed_slice());
            }
            if let Some(cells) = section {
                let idx = (sy as usize % 16) * 256
                    + pos[2].rem_euclid(16) as usize * 16
                    + pos[0].rem_euclid(16) as usize;
                cells[idx] = class;
            }
        }
    }

    pub fn unload_chunk(&self, cx: i32, cz: i32) {
        self.inner
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .columns
            .remove(&(cx, cz));
    }
}

impl VoxelAcousticWorld for WorldAcousticSnapshot {
    fn acoustic_material(&self, x: i32, y: i32, z: i32) -> AcousticMaterial {
        let inner = self.inner.read().unwrap_or_else(|e| e.into_inner());
        let sy = i64::from(y) - i64::from(inner.min_y);
        if sy < 0 || sy >= i64::from(inner.height) {
            return AcousticMaterial::AIR;
        }
        let class = inner
            .columns
            .get(&(x.div_euclid(16), z.div_euclid(16)))
            .and_then(|sections| sections.get((sy / 16) as usize))
            .and_then(Option::as_ref)
            .map_or(0, |cells| {
                cells[(sy as usize % 16) * 256
                    + z.rem_euclid(16) as usize * 16
                    + x.rem_euclid(16) as usize]
            });
        AcousticMaterial::from_class(MaterialClass::from_code(class))
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
