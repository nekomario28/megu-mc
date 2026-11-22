// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! Asynchronous live Protocol 767/777 session and renderer-facing world state.
//! Socket operations happen on a worker thread; `poll` never performs I/O.

use std::sync::mpsc::{self, Receiver, SyncSender, TryRecvError};
use std::time::{Duration, Instant};

use mc_spine_client::protocol::{
    offline_uuid, ClientCustomChannelEvent, ClientPlayEvent, DecodedMagicCastState,
    DecodedMagicSync, DecodedMotionTrigger, DimensionMeta, PlayEvent, ProtocolClient,
    SeasonWireSync, TeleportSync,
};
use mc_spine_client::render::ClientVfxHost;
use mc_spine_client::render_graph::GpuPipelineKind;
use mc_spine_client::world::{BlockTable, WorldState};
use mc_spine_client_mesh::mesher::{
    BlockVisualEnhancementResult, BlockVisualInput, VanillaPlusPipelineConfig,
};
use mc_spine_client_mesh::tactile_effects::{
    compute_environment_audio_hint, evaluate_night_sky_drama, CameraMicroOffset,
    EnvironmentAudioHint, FluidEntryEvent, GroundSurfaceKind, ImpactMaterial, LeafCanopyKind,
    NightSkyDrama, TactileEffectsConfig, TactileEffectsSim,
};
use mc_spine_client_mesh::vanilla_plus_nature::{
    tick_nature_ambience, ActiveSeasonKind, NatureAmbienceEnv, NatureAmbienceState,
    NatureModRegistry, SeasonAmbienceInput,
};
use mc_spine_ui::magic_hud::{CastBarState, CooldownSlotState, MagicHudState, StatusBadgeState};
use mc_spine_ui::state::{HudState, MiniHudPreset, UiState};

// client-gpu's manifest cannot be changed in this lane. Reuse the pure-std
// core implementations until the crate can declare mc-spine-core directly.
#[allow(dead_code)]
#[path = "../../core/src/hw_adaptive_engine.rs"]
pub mod hw_adaptive_engine;
#[allow(dead_code)]
#[path = "../../core/src/sys_metrics.rs"]
pub mod sys_metrics;

use crate::atmosphere::{AtmosphereParams, BiomeAtmosphereBlender, ChunkFadeTracker};
use crate::subworld_render::SubWorldRenderBank;
use hw_adaptive_engine::{
    detect_host_hardware_caps, select_compute_strategy, ComputeStrategy, HostHardwareCaps,
    PagedSectionPool, SpeculativeChunkPrefetcher,
};
use sys_metrics::{sample_host_metrics, CpuSampler, FrameTimingStats};

const PREFETCH_VIEW_DISTANCE: u32 = 8;
const PREFETCH_MAX_CHUNKS: usize = 16;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SessionStatus {
    Connecting,
    Playing,
    Disconnected(String),
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PlayerPose {
    /// Feet position in Minecraft world coordinates.
    pub feet: [f64; 3],
    pub yaw: f32,
    pub pitch: f32,
}

impl PlayerPose {
    pub fn from_teleport(teleport: &TeleportSync, current: Option<Self>) -> Self {
        let previous = current.unwrap_or(Self {
            feet: [0.0; 3],
            yaw: 0.0,
            pitch: 0.0,
        });
        let mut feet = [teleport.x, teleport.y, teleport.z];
        for (index, component) in feet.iter_mut().enumerate() {
            if teleport.flags & (1 << index) != 0 {
                *component += previous.feet[index];
            }
        }
        Self {
            feet,
            yaw: teleport.yaw
                + if teleport.flags & 8 != 0 {
                    previous.yaw
                } else {
                    0.0
                },
            pitch: teleport.pitch
                + if teleport.flags & 16 != 0 {
                    previous.pitch
                } else {
                    0.0
                },
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct PlayerMovement {
    pub pose: PlayerPose,
    pub strafe: f32,
    pub forward: f32,
    pub jump: bool,
}

enum WorkerMessage {
    Connected,
    Play(ClientPlayEvent),
    Closed(String),
}

enum WorkerCommand {
    Movement(PlayerMovement),
}

/// Owns UI-facing world data on the event-loop thread. The worker owns its socket.
pub struct LiveSession {
    incoming: Receiver<WorkerMessage>,
    outgoing: SyncSender<WorkerCommand>,
    pub status: SessionStatus,
    pub world: WorldState,
    pub hud: HudState,
    pub magic: MagicHudState,
    pub vfx: ClientVfxHost,
    pub motion: Option<DecodedMotionTrigger>,
    pub pose: Option<PlayerPose>,
    pub camera_sync: Option<PlayerPose>,
    pub terrain_dirty: bool,
    pub vfx_admitted: u64,
    pub vfx_rejected: u64,
    pub animation_packets: u64,
    pub animation_rejected: u64,
    pub motion_packets: u64,
    pub chat_message: Option<String>,
    pub hw_caps: HostHardwareCaps,
    pub compute_strategy: ComputeStrategy,
    pub paged_sections: PagedSectionPool,
    pub prefetcher: SpeculativeChunkPrefetcher,
    pub metrics_probe: CpuSampler,
    pub frame_timings: FrameTimingStats,
    pub atmosphere_blender: BiomeAtmosphereBlender,
    pub chunk_fades: ChunkFadeTracker,
    pub subworlds: SubWorldRenderBank,
    pub vanilla_plus_cfg: VanillaPlusPipelineConfig,
    pub nature_mod_registry: NatureModRegistry,
    /// The latest decoded `SSN1` season sync, or `None` before the first
    /// `mc-spine:season_v1` frame (and on seasonless dimensions).
    pub latest_season: Option<SeasonWireSync>,
    /// Live seasonal ambience input derived from `latest_season`; the
    /// neutral seasonless fallback until a Season Mod frame arrives.
    pub season_ambience: SeasonAmbienceInput,
    /// Retained nature-ambience particle simulation, ticked once per world
    /// tick inside [`Self::poll`] from `nature_env` + `season_ambience`.
    pub nature_ambience: NatureAmbienceState,
    /// World-sampled ambience spawn hints. The render loop refreshes the
    /// site lists each frame; [`Self::poll`] feeds in the camera `origin`
    /// and the live `season_ambience`.
    pub nature_env: NatureAmbienceEnv,
    /// Atmosphere target with the season tint stripped: the value the
    /// blender would hold if no `SSN1` frame had ever arrived.
    base_atmosphere_target: AtmosphereParams,
    /// The target last installed by the season path, used to detect an
    /// external `set_target` between two season frames.
    seasoned_atmosphere_target: Option<AtmosphereParams>,
    /// Opt-in tactile & sky micro-effect configuration. The default is
    /// `TactilePreset::Off`, which keeps the engine completely silent
    /// until `set_tactile_config` installs `Subtle`/`Fancy`.
    tactile_config: TactileEffectsConfig,
    /// Retained tactile-effect particle sim seeded with the session's
    /// VFX hardware tier; ticked once per world tick inside
    /// [`Self::poll`] and stepped per frame by `step_tactile_frame`.
    tactile_sim: TactileEffectsSim,
    /// Latest deterministic night-sky layer (shooting stars + aurora
    /// bands) evaluated in `step_tactile_frame` from the live season.
    latest_night_sky_drama: NightSkyDrama,
    /// Latest opt-in camera micro-physics offset; exact zero while the
    /// preset is `Off` or `subtle_camera_motion` is disabled.
    latest_camera_micro_offset: CameraMicroOffset,
    /// Latest environment audio occlusion/reverb/muffle hint.
    latest_audio_hint: EnvironmentAudioHint,
    /// Monotonic event counter seeding deterministic effect hashing for
    /// the `on_*` emit entry points.
    tactile_event_tick: u64,
    /// Pose captured by the previous `step_tactile_frame`, used to derive
    /// camera micro-physics velocities from the live pose stream.
    tactile_last_pose: Option<PlayerPose>,
    /// Landing impulse queued by `on_footstep_or_landing`, consumed by
    /// the next `step_tactile_frame` camera-dip step.
    tactile_landing_impulse: f32,
    last_motion_delta: [f64; 2],
    last_vfx_tick: Instant,
    last_animation_tick: Instant,
}

impl LiveSession {
    pub fn connect(host: String, port: u16, username: String, version: String) -> Self {
        let (tx, incoming) = mpsc::sync_channel(256);
        let (outgoing, rx) = mpsc::sync_channel(8);
        std::thread::spawn(move || run_worker(host, port, username, version, tx, rx));
        Self::with_channels(incoming, outgoing)
    }

    fn with_channels(
        incoming: Receiver<WorkerMessage>,
        outgoing: SyncSender<WorkerCommand>,
    ) -> Self {
        let hw_caps = detect_host_hardware_caps();
        let compute_strategy = select_compute_strategy(&hw_caps);
        let vfx = ClientVfxHost::new(GpuPipelineKind::Classic);
        let tactile_tier = vfx.budget_tier();
        let tactile_config = TactileEffectsConfig::default();
        Self {
            incoming,
            outgoing,
            status: SessionStatus::Connecting,
            world: WorldState::new(DimensionMeta::default(), BlockTable::load(None)),
            hud: HudState::default(),
            magic: MagicHudState::default(),
            vfx,
            motion: None,
            pose: None,
            camera_sync: None,
            terrain_dirty: false,
            vfx_admitted: 0,
            vfx_rejected: 0,
            animation_packets: 0,
            animation_rejected: 0,
            motion_packets: 0,
            chat_message: None,
            hw_caps,
            compute_strategy,
            paged_sections: PagedSectionPool::new(),
            prefetcher: SpeculativeChunkPrefetcher::new(),
            metrics_probe: CpuSampler::new(),
            frame_timings: FrameTimingStats::new(),
            atmosphere_blender: BiomeAtmosphereBlender::new(AtmosphereParams::default()),
            chunk_fades: ChunkFadeTracker::new(),
            subworlds: SubWorldRenderBank::new(),
            vanilla_plus_cfg: VanillaPlusPipelineConfig::default(),
            nature_mod_registry: NatureModRegistry::default(),
            latest_season: None,
            season_ambience: SeasonAmbienceInput::default(),
            nature_ambience: NatureAmbienceState::default(),
            nature_env: NatureAmbienceEnv::default(),
            base_atmosphere_target: AtmosphereParams::default(),
            seasoned_atmosphere_target: None,
            tactile_sim: TactileEffectsSim::new(tactile_config.clone(), tactile_tier),
            tactile_config,
            latest_night_sky_drama: NightSkyDrama::default(),
            latest_camera_micro_offset: CameraMicroOffset::default(),
            latest_audio_hint: EnvironmentAudioHint::default(),
            tactile_event_tick: 0,
            tactile_last_pose: None,
            tactile_landing_impulse: 0.0,
            last_motion_delta: [0.0; 2],
            last_vfx_tick: Instant::now(),
            last_animation_tick: Instant::now(),
        }
    }

    /// Evaluate opt-in block enhancements with the session's active pack
    /// configuration and modded-nature registry.
    pub fn evaluate_block_visuals(&self, input: &BlockVisualInput) -> BlockVisualEnhancementResult {
        mc_spine_client_mesh::mesher::evaluate_block_visual_pipeline(
            input,
            &self.vanilla_plus_cfg,
            &self.nature_mod_registry,
        )
    }

    /// Advance the retained nature-ambience particle sim by `dt_sec` using
    /// `nature_env` plus the live `season_ambience`. The pack's own
    /// `enabled`/`nature_ambience` toggles gate spawning, so a default
    /// (vanilla) config ticks existing particles out silently.
    pub fn step_nature_ambience(&mut self, dt_sec: f32) {
        self.nature_env.season = self.season_ambience;
        tick_nature_ambience(
            &mut self.nature_ambience,
            &self.vanilla_plus_cfg.nature,
            &self.nature_env,
            dt_sec,
        );
    }

    /// Install a new tactile-effects configuration, propagating it into
    /// the retained sim. `TactilePreset::Off` (the default) keeps every
    /// emit path and the camera micro-physics completely silent.
    pub fn set_tactile_config(&mut self, config: TactileEffectsConfig) {
        self.tactile_sim.set_config(config.clone());
        self.tactile_config = config;
    }

    /// The retained tactile-effect particle sim (read-only view for the
    /// renderer's particle pass).
    pub fn tactile_sim(&self) -> &TactileEffectsSim {
        &self.tactile_sim
    }

    /// The active tactile-effects configuration.
    pub fn tactile_config(&self) -> &TactileEffectsConfig {
        &self.tactile_config
    }

    /// Latest deterministic night-sky layer from `step_tactile_frame`.
    pub fn latest_night_sky_drama(&self) -> &NightSkyDrama {
        &self.latest_night_sky_drama
    }

    /// Latest environment audio occlusion/reverb/muffle hint.
    pub fn latest_audio_hint(&self) -> EnvironmentAudioHint {
        self.latest_audio_hint
    }

    /// Latest opt-in camera micro-physics offset.
    pub fn latest_camera_micro_offset(&self) -> CameraMicroOffset {
        self.latest_camera_micro_offset
    }

    /// A body broke a fluid surface: mass/speed-scaled water droplets and
    /// concentric [`WaterRippleRing`]s, or lava sizzle sparks and steam.
    /// Silent while the preset is `Off`.
    pub fn on_fluid_entry(&mut self, event: FluidEntryEvent) {
        self.tactile_sim.emit_fluid_entry(&event);
    }

    /// A trunk punch/chop or gust: a downward-spiralling canopy burst.
    /// `kind == None` picks the canopy family from the live
    /// `season_ambience` (Spring -> `CherrySakura`, Autumn ->
    /// `AutumnMaple`, Winter -> `SpruceNeedle`, Summer/seasonless ->
    /// `OakGreen`). Silent while the preset is `Off`.
    pub fn on_tree_shake(
        &mut self,
        canopy_pos: [f32; 3],
        kind: Option<LeafCanopyKind>,
        intensity: f32,
    ) {
        let kind = kind.unwrap_or_else(|| self.seasonal_canopy_kind());
        let tick = self.next_tactile_event_tick();
        self.tactile_sim
            .emit_tree_shake_burst(canopy_pos, kind, intensity, tick);
    }

    /// Footstep or landing dust for `surface`; heavy landings also feed
    /// the camera micro-physics dip impulse on the next
    /// `step_tactile_frame`. Silent while the preset is `Off`.
    pub fn on_footstep_or_landing(
        &mut self,
        pos: [f32; 3],
        surface: GroundSurfaceKind,
        landing_speed: f32,
    ) {
        if landing_speed.is_finite() && landing_speed > 0.0 {
            self.tactile_landing_impulse = self.tactile_landing_impulse.max(landing_speed);
        }
        let tick = self.next_tactile_event_tick();
        self.tactile_sim
            .emit_footstep_or_landing(pos, surface, landing_speed, tick);
    }

    /// Brief crisp sparks on an ore strike, shield block, crit or arrow
    /// flyby. Silent while the preset is `Off`.
    pub fn on_impact_sparks(
        &mut self,
        pos: [f32; 3],
        normal: [f32; 3],
        material: ImpactMaterial,
    ) {
        let tick = self.next_tactile_event_tick();
        self.tactile_sim.emit_impact_sparks(pos, normal, material, tick);
    }

    /// Advance the whole tactile & sky layer by one frame:
    ///
    /// * steps the particle sim and the wetness tracker (soaked while
    ///   `is_submerged || is_raining`, drip anchor at the live pose),
    /// * steps the opt-in camera micro-physics with velocities derived
    ///   from the live pose stream plus the queued landing impulse,
    /// * evaluates the deterministic night sky with the live
    ///   `season_ambience` (winter season or `snow_override` -> polar
    ///   aurora) and the session's atmosphere density,
    /// * refreshes the environment audio hint.
    ///
    /// Completely silent (zero particles, zero sky drama, zero offsets)
    /// while `tactile_config.preset == TactilePreset::Off`.
    #[allow(clippy::too_many_arguments)]
    pub fn step_tactile_frame(
        &mut self,
        dt_sec: f32,
        world_time_ticks: u64,
        is_clear_sky: bool,
        is_submerged: bool,
        sky_light: u8,
        enclosed_wall_ratio: f32,
        is_raining: bool,
    ) {
        if let Some(pose) = self.pose {
            self.tactile_sim.wetness_mut().set_position([
                pose.feet[0] as f32,
                pose.feet[1] as f32,
                pose.feet[2] as f32,
            ]);
        }
        self.tactile_sim
            .step_with_wetness(dt_sec, is_submerged || is_raining);

        let (strafe_vel, vertical_vel) = self.tactile_camera_velocities(dt_sec);
        let landing = self.tactile_landing_impulse;
        self.tactile_landing_impulse = 0.0;
        self.latest_camera_micro_offset = self.tactile_sim.camera_mut().step(
            strafe_vel,
            vertical_vel,
            landing,
            0.0,
            dt_sec,
            &self.tactile_config,
        );

        let season = self.season_ambience.sanitized();
        let is_cold_or_winter = !season.is_seasonless
            && !season.is_tropical
            && (matches!(season.season, ActiveSeasonKind::Winter) || season.snow_override);
        self.latest_night_sky_drama = evaluate_night_sky_drama(
            world_time_ticks,
            is_clear_sky && !is_raining,
            is_cold_or_winter,
            self.tactile_atmosphere_density(),
            &self.tactile_config,
        );
        self.latest_audio_hint = compute_environment_audio_hint(
            is_submerged,
            sky_light,
            enclosed_wall_ratio,
            is_raining,
        );
    }

    /// Seasonal canopy family for `on_tree_shake` when the caller has no
    /// concrete `LeafCanopyKind`: sakura bloom in spring, crimson maple
    /// in autumn, evergreen spruce through the dormant winter and plain
    /// oak otherwise (including the seasonless fallback).
    fn seasonal_canopy_kind(&self) -> LeafCanopyKind {
        match self.season_ambience.sanitized().season {
            ActiveSeasonKind::Spring => LeafCanopyKind::CherrySakura,
            ActiveSeasonKind::Summer => LeafCanopyKind::OakGreen,
            ActiveSeasonKind::Autumn => LeafCanopyKind::AutumnMaple,
            ActiveSeasonKind::Winter => LeafCanopyKind::SpruceNeedle,
        }
    }

    /// Normalized `0..=1` air density for the night-sky layer: `0` on
    /// skylight-less dimensions (nether/end) and vacuum-thin air, `1` at
    /// the overworld reference. The blender's `fog_density` is the
    /// session's air-transmission proxy — `AtmosphereParams::for_policy`
    /// scales it by the dimension's acoustic air transmission and
    /// `vacuum_orbit` keeps it at `0`.
    fn tactile_atmosphere_density(&self) -> f32 {
        if !self.world.dimension.has_skylight {
            return 0.0;
        }
        let reference = AtmosphereParams::default().fog_density;
        if reference <= 0.0 {
            return 0.0;
        }
        (self.atmosphere_blender.current().fog_density / reference).clamp(0.0, 1.0)
    }

    /// Sideways/vertical camera velocity (blocks/sec) derived from the
    /// live pose stream: sideways is the horizontal delta projected onto
    /// the camera-right axis, vertical is `dy/dt`. `(0, 0)` on the first
    /// frame, a degenerate `dt`, or a teleport-pop delta (> 4 blocks).
    fn tactile_camera_velocities(&mut self, dt_sec: f32) -> (f32, f32) {
        let pose = self.pose;
        let previous = self.tactile_last_pose;
        self.tactile_last_pose = pose;
        let (Some(pose), Some(previous)) = (pose, previous) else {
            return (0.0, 0.0);
        };
        if !dt_sec.is_finite() || dt_sec <= 1.0e-3 {
            return (0.0, 0.0);
        }
        let dt = dt_sec.min(0.25);
        let dx = (pose.feet[0] - previous.feet[0]) as f32;
        let dy = (pose.feet[1] - previous.feet[1]) as f32;
        let dz = (pose.feet[2] - previous.feet[2]) as f32;
        if !dx.is_finite() || !dy.is_finite() || !dz.is_finite() || dx * dx + dy * dy + dz * dz > 16.0
        {
            return (0.0, 0.0);
        }
        // MC yaw 0 faces +Z; the camera-right axis is (-cos(yaw), -sin(yaw)).
        let yaw = pose.yaw.to_radians();
        let strafe_vel = -(dx * yaw.cos() + dz * yaw.sin()) / dt;
        (strafe_vel, dy / dt)
    }

    /// Deterministic event-seed counter for the `on_*` emit paths: same
    /// session event order -> same spread/phase hashing, no RNG, no clock.
    fn next_tactile_event_tick(&mut self) -> u64 {
        self.tactile_event_tick = self.tactile_event_tick.wrapping_add(1);
        self.tactile_event_tick
    }

    /// Limit main-thread packet work per frame. No reads, writes or DNS on this thread.
    pub fn poll(&mut self, camera_position: [f32; 3]) {
        for _ in 0..32 {
            match self.incoming.try_recv() {
                Ok(WorkerMessage::Connected) => self.status = SessionStatus::Playing,
                Ok(WorkerMessage::Play(event)) => {
                    if let Err(error) = self.apply_event(event) {
                        self.status = SessionStatus::Disconnected(error);
                        break;
                    }
                }
                Ok(WorkerMessage::Closed(reason)) => {
                    self.status = SessionStatus::Disconnected(reason);
                    break;
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    if !matches!(self.status, SessionStatus::Disconnected(_)) {
                        self.status = SessionStatus::Disconnected("network worker exited".into());
                    }
                    break;
                }
            }
        }
        // Preserve the last received terrain even if the peer closes immediately
        // after sending chunk data.
        let had_pending = self.world.pending_mesh_sections() > 0;
        match self.world.rebuild_dirty_bounded(24, camera_position) {
            Ok(rebuilt) => self.terrain_dirty |= rebuilt != 0 || had_pending,
            Err(error) if !matches!(self.status, SessionStatus::Disconnected(_)) => {
                self.status = SessionStatus::Disconnected(format!("mesh: {error}"));
            }
            Err(_) => {}
        }
        if !matches!(self.status, SessionStatus::Disconnected(_)) {
            let now = Instant::now();
            let elapsed_ticks = (now.duration_since(self.last_vfx_tick).as_millis() / 50).min(4);
            self.nature_env.origin = camera_position;
            for _ in 0..elapsed_ticks {
                self.vfx.tick();
                self.step_nature_ambience(0.05);
                self.tactile_sim.step(0.05);
            }
            self.last_vfx_tick = now;
            let animation_dt = now.duration_since(self.last_animation_tick).as_secs_f32();
            self.last_animation_tick = now;
            if let Err(error) = self.world.advance_animations(animation_dt.min(0.1)) {
                self.status = SessionStatus::Disconnected(format!("animation: {error:?}"));
            }
        }
    }

    pub fn send_movement(&mut self, movement: PlayerMovement) {
        if !matches!(self.status, SessionStatus::Playing) || self.pose.is_none() {
            return;
        }
        if let Some(previous) = self.pose {
            self.last_motion_delta = [
                movement.pose.feet[0] - previous.feet[0],
                movement.pose.feet[2] - previous.feet[2],
            ];
        }
        self.pose = Some(movement.pose);
        self.refresh_prefetcher();
        let _ = self.outgoing.try_send(WorkerCommand::Movement(movement));
    }

    fn refresh_prefetcher(&mut self) {
        let Some(pose) = self.pose else {
            return;
        };
        let columns = &self.world.columns;
        self.prefetcher.update(
            pose.feet[0],
            pose.feet[2],
            self.last_motion_delta[0],
            self.last_motion_delta[1],
            PREFETCH_VIEW_DISTANCE,
            PREFETCH_MAX_CHUNKS,
            |cx, cz| columns.contains_key(&(cx, cz)),
        );
    }

    /// Record frame timing and advance visual transitions before HUD rendering.
    pub fn record_frame_and_refresh_minihud(&mut self, frame_dt_sec: f32, ui: &mut UiState) {
        let frame_us = f64::from(frame_dt_sec) * 1_000_000.0;
        self.frame_timings.push_frame_ms(frame_us / 1_000.0);
        self.chunk_fades.tick(frame_dt_sec);
        self.atmosphere_blender.advance(frame_dt_sec);

        if ui.settings.qol.mini_hud_preset() == MiniHudPreset::Off {
            return;
        }

        let host = sample_host_metrics(&mut self.metrics_probe);
        let telemetry = sys_metrics::MiniHudData::from_telemetry(&host, &self.frame_timings);
        let hud = &mut ui.hud.mini_hud_data;
        hud.fps = telemetry.fps;
        hud.fps_one_percent_low = telemetry.fps_one_percent_low;
        hud.ram_used_mib = telemetry.ram_used_mib;
        hud.ram_total_mib = telemetry.ram_total_mib;
        hud.cpu_percent = telemetry.cpu_percent;
        hud.gpu_name = telemetry.gpu_name;
        hud.gpu_busy_percent = telemetry.gpu_busy_percent;
        hud.vram_used_mib = telemetry.vram_used_mib;
        hud.vram_total_mib = telemetry.vram_total_mib;
        hud.loaded_chunks = Some(self.world.loaded_chunks().min(u32::MAX as usize) as u32);
        hud.coords = self
            .pose
            .map(|pose| (pose.feet[0], pose.feet[1], pose.feet[2]));
    }

    /// Apply a decoded packet, also used by framed protocol regression tests.
    pub fn apply_event(&mut self, event: ClientPlayEvent) -> Result<(), String> {
        match event {
            ClientPlayEvent::Play(event) => self.apply_play(event),
            ClientPlayEvent::CustomChannel(channel) => self.apply_custom(channel),
        }
    }

    fn apply_play(&mut self, event: PlayEvent) -> Result<(), String> {
        match event {
            PlayEvent::JoinGame { dimension, .. } | PlayEvent::Respawn { dimension } => {
                self.world.reset_dimension(dimension);
                self.terrain_dirty = true;
                self.pose = None;
                self.last_motion_delta = [0.0; 2];
                self.chunk_fades = ChunkFadeTracker::new();
                self.prefetcher = SpeculativeChunkPrefetcher::new();
                self.atmosphere_blender.snap_to_target();
            }
            PlayEvent::Teleport(teleport) => {
                let pose = PlayerPose::from_teleport(&teleport, self.pose);
                self.pose = Some(pose);
                self.camera_sync = Some(pose);
            }
            PlayEvent::ChunkData(chunk) => {
                let (cx, cz) = (chunk.cx, chunk.cz);
                self.world.apply_chunk(*chunk)?;
                self.chunk_fades.note_chunk(cx, cz);
                self.refresh_prefetcher();
                self.terrain_dirty = true;
            }
            PlayEvent::UpdateLight { cx, cz, light } => {
                self.world.apply_light_update(cx, cz, &light)?;
            }
            PlayEvent::BlockUpdate { pos, state_id } => {
                self.world.apply_block_update(pos, state_id)?
            }
            PlayEvent::SectionBlocksUpdate(changes) => {
                for (pos, state_id) in changes {
                    self.world.apply_block_update(pos, state_id)?;
                }
            }
            PlayEvent::UnloadChunk { cx, cz } => {
                self.world.unload_chunk(cx, cz);
                self.chunk_fades.forget(cx, cz);
                self.refresh_prefetcher();
                self.terrain_dirty = true;
            }
            PlayEvent::SetHealth { health, food, .. } => {
                self.hud.health = health;
                self.hud.hunger = food.clamp(0, 20) as u8;
            }
            PlayEvent::SystemChat { text, overlay } => {
                if overlay {
                    self.hud.actionbar = text.clone();
                } else {
                    self.hud.chat_lines.push(text.clone());
                    if self.hud.chat_lines.len() > 100 {
                        self.hud.chat_lines.remove(0);
                    }
                }
                self.chat_message = Some(text);
            }
            PlayEvent::Disconnect(reason) => self.status = SessionStatus::Disconnected(reason),
            _ => {}
        }
        Ok(())
    }

    fn apply_custom(&mut self, channel: ClientCustomChannelEvent) -> Result<(), String> {
        match channel {
            ClientCustomChannelEvent::Magic(sync) => self.magic = magic_hud_from_sync(&sync),
            ClientCustomChannelEvent::Vfx(payload) => {
                match self.vfx.ingest_vfx1_payload(&payload) {
                    Ok(admitted) => self.vfx_admitted += admitted as u64,
                    Err(_) => self.vfx_rejected += 1,
                }
            }
            ClientCustomChannelEvent::Anim(payload) => {
                self.animation_packets += 1;
                if self.world.ingest_anm1_payload(&payload).is_err() {
                    self.animation_rejected += 1;
                }
            }
            ClientCustomChannelEvent::Motion(trigger) => {
                self.motion_packets += 1;
                self.motion = Some(trigger);
            }
            ClientCustomChannelEvent::Season(sync) => self.apply_season_sync(&sync),
            ClientCustomChannelEvent::Unknown { .. } => {}
        }
        Ok(())
    }

    /// Route one decoded `SSN1` frame into the live season state: update
    /// `latest_season`/`season_ambience`/`nature_env.season` and retint the
    /// atmosphere blend target with the seasonal foliage tint. Reapplying a
    /// frame never compounds the tint — it is always recomputed from the
    /// stored untinted base, which is itself refreshed when another path
    /// installed a target since the last season frame.
    fn apply_season_sync(&mut self, sync: &SeasonWireSync) {
        self.latest_season = Some(sync.clone());
        let ambience = SeasonAmbienceInput::from_wire_sync(
            sync.sub_season_idx,
            sync.progress_in_sub_season,
            sync.tropical_dim,
            sync.snow_enabled,
            sync.foliage_rgb,
            sync.grass_rgb,
        );
        self.season_ambience = ambience;
        self.nature_env.season = ambience;

        // If the blender target still equals what the previous season frame
        // installed, the stored untinted base is current; otherwise another
        // path (biome/dimension change) installed a new base to adopt.
        let current_target = self.atmosphere_blender.target();
        if self.seasoned_atmosphere_target != Some(current_target) {
            self.base_atmosphere_target = current_target;
        }
        let tinted = seasonal_atmosphere(self.base_atmosphere_target, &ambience);
        self.atmosphere_blender.set_target(tinted);
        self.seasoned_atmosphere_target = Some(tinted);
    }
}

/// Apply the seasonal ambience state to an untinted atmosphere base: the
/// foliage multiplier leans gently on ambient/horizon/fog, and active snow
/// adds a soft haze plus a slight crisp ambient cool-down. A seasonless
/// input returns the base untouched.
fn seasonal_atmosphere(base: AtmosphereParams, season: &SeasonAmbienceInput) -> AtmosphereParams {
    let s = season.sanitized();
    if s.is_seasonless {
        return base;
    }
    let mut out = base;
    let blend = |v: f32, t: f32, k: f32| (v * (1.0 + (t - 1.0) * k)).clamp(0.0, 4.0);
    for i in 0..3 {
        out.ambient_rgb[i] = blend(out.ambient_rgb[i], s.foliage_tint[i], 0.40);
        out.sky_horizon_rgb[i] = blend(out.sky_horizon_rgb[i], s.foliage_tint[i], 0.18);
        out.fog_rgb[i] = blend(out.fog_rgb[i], s.foliage_tint[i], 0.18);
        out.sky_zenith_rgb[i] = blend(out.sky_zenith_rgb[i], s.foliage_tint[i], 0.10);
        out.depth_tint_rgb[i] = blend(out.depth_tint_rgb[i], s.grass_tint[i], 0.10);
    }
    let snow = s.snow_amount();
    if snow > 0.0 {
        out.fog_density = (out.fog_density * (1.0 + 0.30 * snow)).clamp(0.0, 4.0);
        out.sun_intensity = (out.sun_intensity * (1.0 - 0.12 * snow)).clamp(0.0, 16.0);
        for i in 0..3 {
            out.ambient_rgb[i] += ([0.55, 0.62, 0.75][i] - out.ambient_rgb[i]) * 0.12 * snow;
        }
    }
    out.sanitized()
}

pub fn magic_hud_from_sync(sync: &DecodedMagicSync) -> MagicHudState {
    MagicHudState {
        mana: sync.mana,
        max_mana: sync.max_mana,
        active_cast: sync.cast.as_ref().map(|cast| {
            let (elapsed, total, channeled) = match cast.state {
                DecodedMagicCastState::CastTime {
                    total_ticks,
                    elapsed_ticks,
                } => (elapsed_ticks, total_ticks, false),
                DecodedMagicCastState::Channeled {
                    max_ticks,
                    elapsed_ticks,
                    ..
                } => (elapsed_ticks, max_ticks, true),
            };
            CastBarState {
                spell_label: cast.spell_id.clone(),
                progress: if total == 0 {
                    0.0
                } else {
                    elapsed as f32 / total as f32
                },
                channeled,
                interrupted_flash_ticks: 0,
            }
        }),
        cooldowns: sync
            .cooldowns
            .iter()
            .map(|(label, &ticks)| CooldownSlotState {
                spell_label: label.clone(),
                remaining_ticks: ticks,
                total_ticks: ticks,
            })
            .collect(),
        custom_statuses: sync
            .statuses
            .iter()
            .map(|status| StatusBadgeState {
                label: status.id.clone(),
                remaining_ticks: status.remaining_ticks,
                amplifier: status.amplifier,
                is_harmful: false,
                shield_amount: (status.absorption_remaining > 0.0)
                    .then_some(status.absorption_remaining),
            })
            .collect(),
    }
}

fn run_worker(
    host: String,
    port: u16,
    username: String,
    version: String,
    tx: SyncSender<WorkerMessage>,
    rx: Receiver<WorkerCommand>,
) {
    let result = run_connection(&host, port, &username, &version, &tx, rx);
    if let Err(error) = result {
        let _ = tx.send(WorkerMessage::Closed(error));
    }
}

fn run_connection(
    host: &str,
    port: u16,
    username: &str,
    version: &str,
    tx: &SyncSender<WorkerMessage>,
    rx: Receiver<WorkerCommand>,
) -> Result<(), String> {
    let protocol = mc_spine_client::cli::parse_mc_version(version)?;
    let (mut client, _) = ProtocolClient::connect_for_version(
        host,
        port,
        username,
        offline_uuid(username),
        10,
        Duration::from_secs(8),
        protocol,
    )?;
    if tx.send(WorkerMessage::Connected).is_err() {
        return Ok(());
    }
    let mut last_input = None;
    let mut last_send = Instant::now();
    loop {
        loop {
            match rx.try_recv() {
                Ok(WorkerCommand::Movement(movement)) => last_input = Some(movement),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => return Ok(()),
            }
        }
        if last_send.elapsed() >= Duration::from_millis(50) {
            if let Some(input) = last_input {
                let pose = input.pose;
                client.send_player_position_rotation(
                    pose.feet[0],
                    pose.feet[1],
                    pose.feet[2],
                    pose.yaw,
                    pose.pitch,
                    false,
                )?;
                client.send_player_input(input.strafe, input.forward, input.jump, false)?;
            }
            last_send = Instant::now();
        }
        match client.recv_play_with_custom_channels(Duration::from_millis(20))? {
            Some(ClientPlayEvent::Play(PlayEvent::KeepAlive(id))) => client.send_keep_alive(id)?,
            Some(ClientPlayEvent::Play(PlayEvent::ChunkBatchFinished(count))) => {
                client.send_chunk_batch_received((count.max(1)) as f32)?;
            }
            Some(ClientPlayEvent::Play(PlayEvent::Teleport(ref teleport))) => {
                let previous_pose = last_input.map(|movement: PlayerMovement| movement.pose);
                let pose = PlayerPose::from_teleport(teleport, previous_pose);
                client.send_accept_teleportation(
                    teleport.teleport_id,
                    pose.feet,
                    pose.yaw,
                    pose.pitch,
                    false,
                )?;
                if tx
                    .send(WorkerMessage::Play(ClientPlayEvent::Play(
                        PlayEvent::Teleport(teleport.clone()),
                    )))
                    .is_err()
                {
                    return Ok(());
                }
                last_input = Some(PlayerMovement {
                    pose,
                    strafe: 0.0,
                    forward: 0.0,
                    jump: false,
                });
            }
            Some(ClientPlayEvent::Play(PlayEvent::Disconnect(reason))) => return Err(reason),
            Some(event) => {
                if tx.send(WorkerMessage::Play(event)).is_err() {
                    return Ok(());
                }
            }
            None => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::renderer::{
        probe_adapters, request_gpu_device, section_geometry, GpuRenderer, RendererError,
        OFFSCREEN_FORMAT,
    };
    use crate::Camera;
    use mc_spine_client::protocol::{
        decode_chunk_packet, decode_custom_payload_packet, decode_packet_body, encode_packet,
        DecodedMotionPhase, WireReader, CHANNEL_ANIM_V1, CHANNEL_MAGIC_V1, CHANNEL_MOTION_V1,
        CHANNEL_SEASON_V1, CHANNEL_VFX_V1, SSN1_MAGIC, SSN1_VERSION,
    };
    use mc_spine_client_mesh::tactile_effects::{
        FluidEntryBody, TactileParticleKind, TactilePreset,
    };
    use mc_spine_client_mesh::vanilla_plus_nature::{ActiveSeasonKind, NatureParticleKind};
    use mc_spine_client_mesh::vfx_engine::{DynamicColoredLight, Vfx1Packet, VfxCommand};
    use mc_spine_client_mesh::RenderBackendPreference;
    use mc_spine_kernel::skeletal_anim::{encode_animation_trigger, AnimationTrigger, LoopMode};
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};

    fn session() -> (LiveSession, SyncSender<WorkerMessage>) {
        let (tx, incoming) = mpsc::sync_channel(8);
        let (outgoing, _rx) = mpsc::sync_channel(8);
        (LiveSession::with_channels(incoming, outgoing), tx)
    }

    #[test]
    fn initializes_host_strategy_and_live_resources() {
        let (live, _) = session();
        assert!(live.hw_caps.logical_cores >= 1);
        assert_eq!(
            live.compute_strategy,
            select_compute_strategy(&live.hw_caps)
        );
        assert_eq!(live.paged_sections.stats().logical_sections, 0);
        assert_eq!(live.prefetcher.pending_count(), 0);
        assert_eq!(live.frame_timings.frame_count(), 0);
        assert_eq!(live.subworlds.len(), 0);
    }

    #[test]
    fn chunk_arrival_fades_until_advanced_and_unload_forgets() {
        let (mut live, _) = session();
        let body = decoded_frame(0x27, &stone_chunk());
        let chunk = decode_chunk_packet(&body, 24, 64).unwrap();
        live.apply_event(ClientPlayEvent::Play(PlayEvent::ChunkData(Box::new(chunk))))
            .unwrap();
        assert_eq!(live.chunk_fades.fade_alpha(0, 0), 0.0);
        assert!(live.chunk_fades.is_fading(0, 0));

        let mut ui = UiState::new();
        live.record_frame_and_refresh_minihud(0.125, &mut ui);
        assert!((live.chunk_fades.fade_alpha(0, 0) - 0.5).abs() < 0.001);
        live.record_frame_and_refresh_minihud(0.125, &mut ui);
        assert!(!live.chunk_fades.is_fading(0, 0));

        live.apply_event(ClientPlayEvent::Play(PlayEvent::UnloadChunk {
            cx: 0,
            cz: 0,
        }))
        .unwrap();
        assert_eq!(live.chunk_fades.len(), 0);
    }

    #[test]
    fn basic_and_advanced_minihud_receive_frame_and_host_telemetry() {
        let (mut live, _) = session();
        let mut ui = UiState::new();
        ui.settings.qol.mini_hud.enabled = false;
        live.record_frame_and_refresh_minihud(0.02, &mut ui);
        assert_eq!(live.frame_timings.frame_count(), 1);
        assert_eq!(ui.hud.mini_hud_data.fps, None);

        ui.settings.qol.cycle_mini_hud();
        assert_eq!(ui.settings.qol.mini_hud_preset(), MiniHudPreset::Basic);
        live.record_frame_and_refresh_minihud(0.02, &mut ui);
        assert!((ui.hud.mini_hud_data.fps.unwrap() - 50.0).abs() < 0.01);
        assert!((ui.hud.mini_hud_data.fps_one_percent_low.unwrap() - 50.0).abs() < 0.01);
        assert_eq!(ui.hud.mini_hud_data.loaded_chunks, Some(0));

        ui.settings.qol.cycle_mini_hud();
        assert_eq!(ui.settings.qol.mini_hud_preset(), MiniHudPreset::Advanced);
        live.record_frame_and_refresh_minihud(0.04, &mut ui);
        assert_eq!(live.frame_timings.frame_count(), 3);
        assert!((live.frame_timings.frame_ms_avg() - 26.666_666).abs() < 0.001);
        assert!((ui.hud.mini_hud_data.fps.unwrap() - 37.5).abs() < 0.01);
        assert!((ui.hud.mini_hud_data.fps_one_percent_low.unwrap() - 25.0).abs() < 0.01);
    }

    fn varint(out: &mut Vec<u8>, mut value: u32) {
        loop {
            let part = (value & 0x7f) as u8;
            value >>= 7;
            out.push(part | if value == 0 { 0 } else { 0x80 });
            if value == 0 {
                break;
            }
        }
    }

    fn wire_string(out: &mut Vec<u8>, value: &str) {
        varint(out, value.len() as u32);
        out.extend_from_slice(value.as_bytes());
    }

    fn decoded_frame(packet_id: i32, payload: &[u8]) -> Vec<u8> {
        // Exercise the same VarInt length/pid framing as the TCP receiver.
        let frame = encode_packet(packet_id, payload, None);
        let mut reader = WireReader::new(&frame);
        let length = usize::try_from(reader.read_varint().unwrap()).unwrap();
        assert_eq!(reader.remaining(), length);
        let (decoded_id, body) = decode_packet_body(reader.rest(), None).unwrap();
        assert_eq!(decoded_id, packet_id);
        body
    }

    fn read_loopback_frame(socket: &mut TcpStream) -> (i32, Vec<u8>) {
        let mut length = 0u32;
        for shift in (0..35).step_by(7) {
            let mut byte = [0];
            socket.read_exact(&mut byte).unwrap();
            length |= u32::from(byte[0] & 0x7f) << shift;
            if byte[0] & 0x80 == 0 {
                break;
            }
        }
        assert!(length < 8 * 1024 * 1024);
        let mut packet = vec![0; length as usize];
        socket.read_exact(&mut packet).unwrap();
        decode_packet_body(&packet, None).unwrap()
    }

    fn send_loopback_frame(socket: &mut TcpStream, packet_id: i32, data: &[u8]) {
        socket
            .write_all(&encode_packet(packet_id, data, None))
            .unwrap();
    }

    fn framed(channel: &str, payload: &[u8]) -> ClientPlayEvent {
        let mut wire = Vec::new();
        wire_string(&mut wire, channel);
        wire.extend_from_slice(payload);
        let body = decoded_frame(0x19, &wire);
        ClientPlayEvent::CustomChannel(
            decode_custom_payload_packet(&body).expect("framed custom packet"),
        )
    }

    fn stone_chunk() -> Vec<u8> {
        // Protocol 767 level_chunk_with_light body, dimension -64..320.
        // A single 16x16x16 solid stone section is surrounded by air.
        let mut sections = Vec::new();
        for section in 0..24 {
            sections.extend_from_slice(&(if section == 4 { 4096i16 } else { 0 }).to_be_bytes());
            sections.push(0); // single-valued block palette
            varint(&mut sections, if section == 4 { 1 } else { 0 });
            varint(&mut sections, 0); // no block data array
            sections.push(0); // single-valued biome palette
            varint(&mut sections, 0);
            varint(&mut sections, 0);
        }
        let mut body = Vec::new();
        body.extend_from_slice(&0i32.to_be_bytes()); // chunk X
        body.extend_from_slice(&0i32.to_be_bytes()); // chunk Z
        body.extend_from_slice(&[10, 0]); // empty unnamed compound heightmap
        varint(&mut body, sections.len() as u32);
        body.extend_from_slice(&sections);
        for _ in 0..5 {
            varint(&mut body, 0);
        } // block entities and four light masks
        varint(&mut body, 0); // sky arrays
        varint(&mut body, 0); // block arrays
        body
    }

    fn assert_live_gpu_frame(world: &WorldState) -> Option<GpuRenderer> {
        let reports = probe_adapters(RenderBackendPreference::Auto);
        if reports.is_empty() {
            eprintln!("SKIP live GPU render: probe_adapters found 0 adapters");
            return None;
        }
        let ctx = match request_gpu_device(RenderBackendPreference::Auto, None, None) {
            Ok(ctx) => ctx,
            Err(RendererError::NoAdapter(error)) => {
                panic!("probe found adapters but request failed: {error}")
            }
            Err(error) => panic!("live GPU setup failed: {error}"),
        };
        eprintln!(
            "live GPU test adapter: {:?} {}",
            ctx.report.backend, ctx.report.name
        );
        let mut renderer = GpuRenderer::new(ctx, OFFSCREEN_FORMAT);
        let camera = Camera {
            position: [8.0, 20.0, 30.0],
            yaw_deg: 0.0,
            pitch_deg: -25.0,
            ..Camera::default()
        };
        let blank = renderer
            .render_offscreen_rgba(128, 128, &camera, None)
            .unwrap();
        assert_eq!(renderer.update_live_terrain(world), 1);
        assert_eq!(renderer.live_section_count(), 1);
        let rendered = renderer
            .render_offscreen_rgba(128, 128, &camera, None)
            .unwrap();
        assert!(
            rendered
                .iter()
                .zip(&blank)
                .any(|(pixel, baseline)| pixel != baseline),
            "decoded live chunk must change GPU pixels from an empty world"
        );
        eprintln!(
            "live chunk changed offscreen GPU frame ({} bytes)",
            rendered.len()
        );
        Some(renderer)
    }

    #[test]
    fn loopback_tcp_767_worker_receives_chunks_and_channels() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let (done_tx, done_rx) = mpsc::channel::<()>();

        let server = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            socket
                .set_write_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            assert_eq!(read_loopback_frame(&mut socket).0, 0); // handshake
            assert_eq!(read_loopback_frame(&mut socket).0, 0); // login_start

            let mut login = offline_uuid("Loopback").to_vec();
            wire_string(&mut login, "Loopback");
            varint(&mut login, 0); // profile properties
            login.push(0);
            send_loopback_frame(&mut socket, 0x02, &login);
            assert_eq!(read_loopback_frame(&mut socket).0, 0x03); // login_acknowledged

            send_loopback_frame(&mut socket, 0x03, &[]); // finish_configuration
            assert_eq!(read_loopback_frame(&mut socket).0, 0x00); // client_information
            assert_eq!(read_loopback_frame(&mut socket).0, 0x03); // finish ack

            send_loopback_frame(&mut socket, 0x26, &29i64.to_be_bytes());
            let mut teleport = Vec::new();
            for value in [8.0f64, 64.0, 30.0] {
                teleport.extend_from_slice(&value.to_be_bytes());
            }
            teleport.extend_from_slice(&0.0f32.to_be_bytes()); // yaw
            teleport.extend_from_slice(&0.0f32.to_be_bytes()); // pitch
            teleport.push(0); // absolute position
            varint(&mut teleport, 3); // teleport ID
            send_loopback_frame(&mut socket, 0x40, &teleport);
            send_loopback_frame(&mut socket, 0x27, &stone_chunk());

            let mut magic = b"MAG1".to_vec();
            magic.extend_from_slice(&9.0f32.to_le_bytes());
            magic.extend_from_slice(&20.0f32.to_le_bytes());
            magic.push(0); // no cast
            varint(&mut magic, 0); // no cooldowns
            varint(&mut magic, 0); // no statuses
            let mut payload = Vec::new();
            wire_string(&mut payload, CHANNEL_MAGIC_V1);
            payload.extend_from_slice(&magic);
            send_loopback_frame(&mut socket, 0x19, &payload);

            let vfx = Vfx1Packet {
                commands: vec![VfxCommand::SpawnLight {
                    id: 17,
                    light: DynamicColoredLight {
                        pos: [4.0, 4.0, 4.0],
                        rgb: [1.0, 1.0, 1.0],
                        radius: 3.0,
                        intensity: 1.0,
                        ttl_ticks: 50,
                        priority: 1,
                    },
                }],
            }
            .encode()
            .unwrap();
            payload.clear();
            wire_string(&mut payload, CHANNEL_VFX_V1);
            payload.extend_from_slice(&vfx);
            send_loopback_frame(&mut socket, 0x19, &payload);

            let animation = encode_animation_trigger(AnimationTrigger {
                entity_id: 77,
                layer: 0,
                clip_id: 0,
                speed: 1.0,
                fade_ticks: 0,
                loop_mode: LoopMode::Loop,
                restart: true,
            })
            .unwrap();
            payload.clear();
            wire_string(&mut payload, CHANNEL_ANIM_V1);
            payload.extend_from_slice(&animation);
            send_loopback_frame(&mut socket, 0x19, &payload);

            let mut keep_alive = false;
            let mut accepted_teleport = false;
            for _ in 0..8 {
                let (packet_id, body) = read_loopback_frame(&mut socket);
                if packet_id == 0x18 {
                    assert_eq!(body, 29i64.to_be_bytes());
                    keep_alive = true;
                }
                if packet_id == 0x00 {
                    assert_eq!(WireReader::new(&body).read_varint().unwrap(), 3);
                    accepted_teleport = true;
                }
                if keep_alive && accepted_teleport {
                    break;
                }
            }
            assert!(
                keep_alive && accepted_teleport,
                "server must receive play acknowledgments"
            );
            done_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        });

        let mut live =
            LiveSession::connect("127.0.0.1".into(), port, "Loopback".into(), "767".into());
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            live.poll([8.0, 20.0, 30.0]);
            if live.world.loaded_chunks() == 1
                && live.magic.mana == 9.0
                && live.vfx_admitted == 1
                && live.animation_packets == 1
                && live.camera_sync.is_some()
            {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(
            live.world.loaded_chunks(),
            1,
            "TCP chunk should reach ClientWorld"
        );
        assert_eq!(live.magic.mana, 9.0, "TCP MAG1 should reach the HUD");
        assert_eq!(live.vfx_admitted, 1, "TCP VFX1 should reach the VFX host");
        assert_eq!(
            live.animation_packets, 1,
            "TCP ANM1 should reach animation host"
        );
        assert_eq!(live.camera_sync.unwrap().feet, [8.0, 64.0, 30.0]);
        done_tx.send(()).unwrap();
        server.join().unwrap();
        let mesh = live.world.packed_meshes().next().expect("TCP chunk mesh");
        let geometry =
            section_geometry(mesh.1, live.world.translucent_order(mesh.0).unwrap_or(&[]));
        assert!(!geometry.opaque.is_empty());
        let _ = assert_live_gpu_frame(&live.world);
    }

    #[test]
    fn state_and_relative_teleport() {
        let (_tx, incoming) = mpsc::sync_channel(1);
        let (outgoing, _rx) = mpsc::sync_channel(1);
        let mut session = LiveSession::with_channels(incoming, outgoing);
        assert_eq!(session.status, SessionStatus::Connecting);
        session.status = SessionStatus::Playing;
        let teleport = TeleportSync {
            x: 2.0,
            y: 3.0,
            z: 4.0,
            yaw: 15.0,
            pitch: -5.0,
            flags: 0x1f,
            teleport_id: 7,
        };
        let base = PlayerPose {
            feet: [4.0, 5.0, 6.0],
            yaw: 90.0,
            pitch: 10.0,
        };
        let pose = PlayerPose::from_teleport(&teleport, Some(base));
        assert_eq!(pose.feet, [6.0, 8.0, 10.0]);
        assert_eq!((pose.yaw, pose.pitch), (105.0, 5.0));
        session.pose = Some(base);
        session
            .apply_event(ClientPlayEvent::Play(PlayEvent::Teleport(teleport)))
            .unwrap();
        assert_eq!(session.camera_sync, Some(pose));
    }

    #[test]
    fn worker_messages_and_hud_play_events() {
        let (mut live, tx) = session();
        tx.send(WorkerMessage::Connected).unwrap();
        tx.send(WorkerMessage::Play(ClientPlayEvent::Play(
            PlayEvent::SetHealth {
                health: 14.0,
                food: 12,
                saturation: 2.0,
            },
        )))
        .unwrap();
        tx.send(WorkerMessage::Play(ClientPlayEvent::Play(
            PlayEvent::SystemChat {
                text: "server ready".into(),
                overlay: false,
            },
        )))
        .unwrap();
        live.poll([0.0; 3]);
        assert_eq!(live.status, SessionStatus::Playing);
        assert_eq!(live.hud.health, 14.0);
        assert_eq!(live.hud.hunger, 12);
        assert_eq!(live.hud.chat_lines.last().unwrap(), "server ready");
        tx.send(WorkerMessage::Closed("test disconnect".into()))
            .unwrap();
        live.poll([0.0; 3]);
        assert_eq!(
            live.status,
            SessionStatus::Disconnected("test disconnect".into())
        );
    }

    #[test]
    fn framed_magic_vfx_anim_motion_reach_live_hosts() {
        let (mut live, _tx) = session();
        let mut magic = b"MAG1".to_vec();
        magic.extend_from_slice(&7.5f32.to_le_bytes());
        magic.extend_from_slice(&20f32.to_le_bytes());
        magic.push(1); // cast time
        wire_string(&mut magic, "spell:flame");
        varint(&mut magic, 100);
        varint(&mut magic, 25);
        varint(&mut magic, 1); // cooldowns
        wire_string(&mut magic, "spell:flame");
        varint(&mut magic, 40);
        varint(&mut magic, 0); // statuses
        live.apply_event(framed(CHANNEL_MAGIC_V1, &magic)).unwrap();
        assert_eq!((live.magic.mana, live.magic.max_mana), (7.5, 20.0));
        assert_eq!(live.magic.active_cast.as_ref().unwrap().progress, 0.25);
        assert_eq!(live.magic.cooldowns[0].remaining_ticks, 40);
        assert!(!live.magic.is_empty());

        let vfx = Vfx1Packet {
            commands: vec![VfxCommand::SpawnLight {
                id: 42,
                light: DynamicColoredLight {
                    pos: [3.0, 4.0, 5.0],
                    rgb: [1.0, 0.5, 0.25],
                    radius: 6.0,
                    intensity: 2.0,
                    ttl_ticks: 20,
                    priority: 2,
                },
            }],
        }
        .encode()
        .unwrap();
        live.apply_event(framed(CHANNEL_VFX_V1, &vfx)).unwrap();
        assert_eq!(live.vfx_admitted, 1);
        assert_eq!(live.vfx.frame_output([0.0, 0.0, 1.0]).lights.len(), 1);

        const GEOMETRY: &str = r#"{"format_version":"1.12.0","minecraft:geometry":[{"description":{"identifier":"geometry.test"},"bones":[{"name":"root","pivot":[0,0,0]}]}]}"#;
        const ANIMATIONS: &str = r#"{"format_version":"1.8.0","animations":{"animation.test.move":{"loop":true,"animation_length":1.0,"bones":{"root":{"position":{"0.0":[0,0,0],"1.0":[2,0,0]}}}}}}"#;
        live.world
            .animations
            .register_model_json("test", GEOMETRY, ANIMATIONS)
            .unwrap();
        live.world.animations.bind_entity(77, "test").unwrap();
        let before = live.world.evaluate_entity_pose(77).unwrap().unwrap();
        let trigger = encode_animation_trigger(AnimationTrigger {
            entity_id: 77,
            layer: 0,
            clip_id: live
                .world
                .animations
                .clip_id("test", "animation.test.move")
                .unwrap(),
            speed: 1.0,
            fade_ticks: 0,
            loop_mode: LoopMode::Loop,
            restart: true,
        })
        .unwrap();
        live.apply_event(framed(CHANNEL_ANIM_V1, &trigger)).unwrap();
        assert_eq!(live.animation_packets, 1);
        assert_eq!(live.animation_rejected, 0);
        live.world.advance_animations(0.5).unwrap();
        assert_ne!(
            live.world.evaluate_entity_pose(77).unwrap().unwrap(),
            before
        );

        let mut motion = b"MOT1".to_vec();
        motion.extend_from_slice(&77u64.to_be_bytes());
        motion.extend_from_slice(&123u32.to_be_bytes());
        motion.extend_from_slice(&2u16.to_be_bytes());
        motion.push(0); // main hand
        motion.extend_from_slice(&1.0f32.to_be_bytes());
        motion.extend_from_slice(&5u32.to_be_bytes());
        motion.push(2); // active hit window
        live.apply_event(framed(CHANNEL_MOTION_V1, &motion))
            .unwrap();
        assert_eq!(live.motion_packets, 1);
        assert_eq!(
            live.motion.unwrap().phase,
            DecodedMotionPhase::ActiveHitWindow
        );
    }

    #[test]
    fn decoded_play_chunk_meshes_and_renders_live_gpu_frame() {
        let (mut live, _tx) = session();
        let chunk_body = decoded_frame(0x27, &stone_chunk());
        live.apply_event(ClientPlayEvent::Play(PlayEvent::ChunkData(Box::new(
            decode_chunk_packet(&chunk_body, 24, 64).unwrap(),
        ))))
        .unwrap();
        live.poll([8.0, 8.0, 24.0]);
        assert_eq!(live.world.loaded_chunks(), 1);
        let mesh = live
            .world
            .packed_meshes()
            .next()
            .expect("solid section mesh");
        let geometry =
            section_geometry(mesh.1, live.world.translucent_order(mesh.0).unwrap_or(&[]));
        assert!(!geometry.opaque.is_empty());
        assert!(!geometry.opaque_indices.is_empty());

        let renderer = assert_live_gpu_frame(&live.world);

        live.apply_event(ClientPlayEvent::Play(PlayEvent::UnloadChunk {
            cx: 0,
            cz: 0,
        }))
        .unwrap();
        assert_eq!(live.world.loaded_chunks(), 0);
        if let Some(mut renderer) = renderer {
            renderer.update_live_terrain(&live.world);
            assert_eq!(renderer.live_section_count(), 0);
        }
    }

    fn ssn1_frame(
        sub_season_idx: u8,
        progress_q16: u16,
        foliage: [u8; 3],
        grass: [u8; 3],
        flags: u8,
    ) -> Vec<u8> {
        let mut frame = Vec::with_capacity(19);
        frame.extend_from_slice(SSN1_MAGIC);
        frame.push(SSN1_VERSION);
        frame.push(sub_season_idx);
        frame.extend_from_slice(&progress_q16.to_be_bytes());
        frame.extend_from_slice(&12u16.to_be_bytes()); // day_of_season
        frame.extend_from_slice(&49_151u16.to_be_bytes()); // daylight q16
        frame.extend_from_slice(&foliage);
        frame.extend_from_slice(&grass);
        frame.push(flags);
        frame
    }

    #[test]
    fn ssn1_season_frames_drive_ambience_and_atmosphere() {
        let (mut live, _tx) = session();
        assert!(live.latest_season.is_none());
        assert!(live.season_ambience.is_seasonless);
        let base_target = live.atmosphere_blender.target();

        // Mid-autumn frame with a warm crimson foliage tint.
        let autumn = ssn1_frame(7, 32_767, [255, 120, 40], [200, 160, 80], 0);
        live.apply_event(framed(CHANNEL_SEASON_V1, &autumn)).unwrap();
        assert_eq!(
            live.latest_season.as_ref().unwrap().sub_season_idx,
            7,
            "SSN1 should reach LiveSession"
        );
        assert_eq!(live.season_ambience.season, ActiveSeasonKind::Autumn);
        assert!(!live.season_ambience.is_seasonless);
        assert_eq!(live.season_ambience.sub_season_index, 7);
        assert!(live.season_ambience.autumn_ramp() > 0.5);
        assert_eq!(live.nature_env.season, live.season_ambience);

        // The foliage multiplier tinted the blend target: green/blue drop
        // while the (already 1.0) red channel holds.
        let autumn_target = live.atmosphere_blender.target();
        assert!(autumn_target.ambient_rgb[1] < base_target.ambient_rgb[1]);
        assert!(autumn_target.ambient_rgb[2] < base_target.ambient_rgb[2]);
        assert!((autumn_target.ambient_rgb[0] - base_target.ambient_rgb[0]).abs() < 1e-6);

        // A mid-winter frame with snow enabled: frosty haze + snow motes.
        let winter = ssn1_frame(10, 49_151, [235, 240, 255], [220, 230, 240], 0b100);
        live.apply_event(framed(CHANNEL_SEASON_V1, &winter)).unwrap();
        assert_eq!(live.season_ambience.season, ActiveSeasonKind::Winter);
        assert!(live.season_ambience.snow_override);
        assert!(live.season_ambience.snow_amount() > 0.5);
        let winter_target = live.atmosphere_blender.target();
        assert!(
            winter_target.fog_density > autumn_target.fog_density,
            "winter snow should haze the fog"
        );
        assert!(winter_target.sun_intensity < autumn_target.sun_intensity);

        // The ambience tick picks the live season up: snow drifts, and the
        // pack toggle still gates spawning (default OFF stays quiet).
        for _ in 0..40 {
            live.step_nature_ambience(0.05);
        }
        assert!(
            live.nature_ambience
                .particles()
                .iter()
                .all(|p| !matches!(p.kind, NatureParticleKind::SnowMote { .. })),
            "default-OFF pack must emit no ambience"
        );
        live.vanilla_plus_cfg.nature.enabled = true;
        live.vanilla_plus_cfg.nature.nature_ambience = true;
        for _ in 0..40 {
            live.step_nature_ambience(0.05);
        }
        assert!(
            live.nature_ambience
                .particles()
                .iter()
                .any(|p| matches!(p.kind, NatureParticleKind::SnowMote { .. })),
            "winter SSN1 should drift snow motes through NatureAmbience"
        );

        // A tropical frame keeps the same sub-season but kills the snow.
        let tropical = ssn1_frame(10, 49_151, [200, 255, 200], [180, 240, 180], 0b101);
        live.apply_event(framed(CHANNEL_SEASON_V1, &tropical))
            .unwrap();
        assert!(live.season_ambience.is_tropical);
        assert_eq!(live.season_ambience.snow_amount(), 0.0);
        let tropical_target = live.atmosphere_blender.target();
        assert!(
            tropical_target.fog_density < winter_target.fog_density,
            "tropical dims lose the snowy haze"
        );
    }

    #[test]
    fn tactile_effects_default_off_stays_silent() {
        let (mut live, _tx) = session();
        assert_eq!(live.tactile_config().preset, TactilePreset::Off);
        assert_eq!(live.tactile_sim().len(), 0);
        assert_eq!(live.tactile_sim().tier(), live.vfx.budget_tier());

        live.on_fluid_entry(FluidEntryEvent {
            pos: [0.0, 64.0, 0.0],
            impact_speed: 6.0,
            body: FluidEntryBody::PlayerOrMediumMob,
            is_lava: false,
            seed_tick: 42,
        });
        live.on_tree_shake([0.0, 70.0, 0.0], None, 1.0);
        live.on_footstep_or_landing([0.0, 64.0, 0.0], GroundSurfaceKind::Snow, 5.0);
        live.on_impact_sparks(
            [0.0, 64.0, 0.0],
            [0.0, 1.0, 0.0],
            ImpactMaterial::CriticalHit,
        );
        live.step_tactile_frame(0.05, 18_000, true, true, 0, 0.9, true);
        assert_eq!(live.tactile_sim().len(), 0, "Off preset emits no particles");
        assert!(live.latest_night_sky_drama().is_empty());
        assert_eq!(
            live.latest_camera_micro_offset(),
            CameraMicroOffset::default()
        );

        // Even with the camera opt-in flag set, the Off preset pins the
        // micro-physics output to an exact zero offset.
        live.set_tactile_config(TactileEffectsConfig {
            subtle_camera_motion: true,
            ..TactileEffectsConfig::default()
        });
        live.on_footstep_or_landing([0.0, 64.0, 0.0], GroundSurfaceKind::StoneOrWood, 8.0);
        live.step_tactile_frame(0.05, 18_100, true, false, 15, 0.0, false);
        assert_eq!(
            live.latest_camera_micro_offset(),
            CameraMicroOffset::default(),
            "Off preset must zero the camera offset even when opted in"
        );
        assert_eq!(live.tactile_sim().len(), 0);
    }

    #[test]
    fn tactile_effects_fancy_and_live_seasons_drive_sky_splash_leaves_audio() {
        let (mut live, _tx) = session();

        // A mid-winter SSN1 frame with snow override.
        let winter = ssn1_frame(10, 49_151, [235, 240, 255], [220, 230, 240], 0b100);
        live.apply_event(framed(CHANNEL_SEASON_V1, &winter)).unwrap();
        assert_eq!(live.season_ambience.season, ActiveSeasonKind::Winter);
        assert!(live.season_ambience.snow_override);

        live.set_tactile_config(TactileEffectsConfig {
            preset: TactilePreset::Fancy,
            ..TactileEffectsConfig::default()
        });
        assert_eq!(live.tactile_config().preset, TactilePreset::Fancy);
        assert_eq!(live.tactile_sim().config().preset, TactilePreset::Fancy);

        // Winter clear night -> polar aurora bands in the sky layer.
        live.step_tactile_frame(0.05, 18_000, true, false, 4, 0.0, false);
        assert!(
            !live.latest_night_sky_drama().aurora_bands.is_empty(),
            "winter SSN1 + Fancy should drive polar aurora bands"
        );

        // Water entry -> droplets plus a concentric ripple ring.
        let before = live.tactile_sim().len();
        live.on_fluid_entry(FluidEntryEvent {
            pos: [2.0, 63.0, 2.0],
            impact_speed: 8.0,
            body: FluidEntryBody::PlayerOrMediumMob,
            is_lava: false,
            seed_tick: 7,
        });
        let spawned = &live.tactile_sim().particles()[before..];
        assert!(
            spawned
                .iter()
                .any(|p| matches!(p.kind, TactileParticleKind::SplashDroplet)),
            "water entry must spawn splash droplets"
        );
        assert!(
            spawned
                .iter()
                .any(|p| matches!(p.kind, TactileParticleKind::WaterRippleRing { .. })),
            "water entry must spawn a concentric ripple ring"
        );

        // Spring SSN1 -> a trunk shake sheds sakura-pink petals.
        let spring = ssn1_frame(1, 30_000, [255, 200, 220], [200, 230, 180], 0);
        live.apply_event(framed(CHANNEL_SEASON_V1, &spring)).unwrap();
        assert_eq!(live.season_ambience.season, ActiveSeasonKind::Spring);
        let before = live.tactile_sim().len();
        live.on_tree_shake([4.0, 70.0, 4.0], None, 1.0);
        let petals = &live.tactile_sim().particles()[before..];
        assert!(!petals.is_empty(), "spring shake must shed petals");
        assert!(
            petals
                .iter()
                .all(|p| matches!(p.kind, TactileParticleKind::LeafPetal)
                    && p.rgba[0] > 0.9
                    && p.rgba[2] > p.rgba[1]),
            "spring seasonal pick must tint petals sakura pink"
        );

        // Autumn SSN1 -> maple-amber leaves, and the aurora is gone now
        // that the live season is no longer winter.
        let autumn = ssn1_frame(7, 32_767, [255, 120, 40], [200, 160, 80], 0);
        live.apply_event(framed(CHANNEL_SEASON_V1, &autumn)).unwrap();
        assert_eq!(live.season_ambience.season, ActiveSeasonKind::Autumn);
        let before = live.tactile_sim().len();
        live.on_tree_shake([8.0, 70.0, 8.0], None, 1.0);
        let petals = &live.tactile_sim().particles()[before..];
        assert!(!petals.is_empty(), "autumn shake must shed leaves");
        assert!(
            petals
                .iter()
                .all(|p| matches!(p.kind, TactileParticleKind::LeafPetal)
                    && p.rgba[0] > p.rgba[1]
                    && p.rgba[1] > p.rgba[2] + 0.1),
            "autumn seasonal pick must tint leaves maple amber"
        );
        live.step_tactile_frame(0.05, 18_000, true, false, 4, 0.0, false);
        assert!(
            live.latest_night_sky_drama().aurora_bands.is_empty(),
            "autumn nights get no polar aurora: live seasonal coupling"
        );

        // Submerged in a dark enclosed cave while it rains: underwater
        // low-pass, cave reverb and rain-eaves occlusion all engage.
        live.step_tactile_frame(0.05, 18_100, true, true, 0, 0.9, true);
        let hint = live.latest_audio_hint();
        assert!(hint.underwater_lowpass > 0.0, "submerged -> low-pass");
        assert!(hint.cave_reverb_mix > 0.0, "dark enclosed -> reverb");
        assert!(hint.rain_eaves_occlusion > 0.0, "rain + cover -> occlusion");
    }
}
