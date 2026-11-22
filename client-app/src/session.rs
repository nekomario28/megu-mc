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

use std::collections::BTreeSet;
use std::path::PathBuf;

use crate::crash_reporter::ReportEnv;
use crate::protocol::{decode_megu_custom_payload, MeguCustomPayload, SeasonWireSync};
use crate::recovery::{
    ContainmentOutcome, CrashCircuitBreaker, CrashGuard, RecoveryAction,
};
use crate::spatial_audio::{
    AcousticTracer, ListenerPose, MaterialClass, SpatialAudioEngine, SpatialAudioPreset,
    VoxelAcousticWorld,
};

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
    let right_norm = (right_raw[0] * right_raw[0]
        + right_raw[1] * right_raw[1]
        + right_raw[2] * right_raw[2])
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
    (
        rel_x.atan2(rel_z),
        rel_y.clamp(-1.0, 1.0).asin(),
        dist,
    )
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
        let dir = std::env::temp_dir().join(format!("mc-spine-session-{tag}-{}", std::process::id()));
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
        session.crash_breaker =
            CrashCircuitBreaker::with_thresholds(2, 60_000, 5, 30_000);
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
