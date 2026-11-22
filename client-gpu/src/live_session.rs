// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! Asynchronous live Protocol 767/777 session and renderer-facing world state.
//! Socket operations happen on a worker thread; `poll` never performs I/O.

use std::collections::BTreeMap;
use std::sync::mpsc::{self, Receiver, SyncSender, TryRecvError};
use std::time::{Duration, Instant};

use mc_spine_client::audio_out::{AudioRuntime, AudioSessionConfig, AudioSinkKind};
use mc_spine_client::protocol::{
    decode_item_stack_components, handle_transfer_packet, offline_uuid,
    ClientCustomChannelEvent, ClientPlayEvent, DecodedMagicCastState, DecodedMagicSync,
    DecodedMotionTrigger, DimensionMeta, ItemStackComponents, PlayEvent, PlayerInfoUpdateEntry,
    ProtocolClient, SeasonWireSync, TeleportSync, TransferTarget, WireSoundEvent, WireStopSound,
};
use mc_spine_kernel::CommandDispatcher;
use mc_spine_kernel::player_physics::{
    step_player_physics, PlayerInputState, PlayerPhysicsState, SurfaceKind,
};
use mc_spine_kernel::raycast::raycast_entities;
use mc_spine_kernel::Aabb;
use mc_spine_client::render::ClientVfxHost;
use mc_spine_client::render_graph::GpuPipelineKind;
use mc_spine_client::session::{AudioStats, WorldAcousticSnapshot};
use mc_spine_client::spatial_audio::{SpatialSubtitleCue, SubtitleDirection};
use mc_spine_client::world::{BlockTable, WorldState};
use mc_spine_client_mesh::asset_pipeline::{CompiledAssetBundle, ResourcePackLayer};
use mc_spine_client_mesh::entity_rig::{classify_entity_rig, EntityRigSpec};
use mc_spine_client_mesh::mesher::{
    BlockVisualEnhancementResult, BlockVisualInput, VanillaPlusPipelineConfig,
};
use mc_spine_client_mesh::tactile_effects::{
    compute_environment_audio_hint, evaluate_night_sky_drama, CameraMicroOffset,
    EnvironmentAudioHint, FluidEntryEvent, GroundSurfaceKind, ImpactMaterial, LeafCanopyKind,
    NightSkyDrama, TactileEffectsConfig, TactileEffectsSim,
};
use mc_spine_client_mesh::vanilla_plus_nature::{
    compute_cloud_lighting, generate_layered_voxel_clouds, tick_nature_ambience,
    ActiveSeasonKind, NatureAmbienceEnv, NatureAmbienceState, NatureModRegistry,
    SeasonAmbienceInput,
};
use mc_spine_ui::magic_hud::{CastBarState, CooldownSlotState, MagicHudState, StatusBadgeState};
use mc_spine_ui::state::{HudState, MiniHudPreset, UiState};
use mc_spine_ui::subtitle_hud::{SubtitleArrow, DEFAULT_TTL_SECS};

// client-gpu's manifest cannot be changed in this lane. Reuse the pure-std
// core implementations until the crate can declare mc-spine-core directly.
#[allow(dead_code)]
#[path = "../../core/src/hw_adaptive_engine.rs"]
pub mod hw_adaptive_engine;
#[allow(dead_code)]
#[path = "../../core/src/sys_metrics.rs"]
pub mod sys_metrics;

use crate::atmosphere::{
    voxel_clouds_to_geometry, AtmosphereParams, BiomeAtmosphereBlender, ChunkFadeTracker,
    CLOUD_RENDER_ALPHA,
};
use crate::subworld_render::{SubWorldRenderBank, SubWorldRenderEntry, SubWorldTransform};
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
    Sound(WireSoundEvent),
    StopSound(WireStopSound),
    Closed(String),
}

enum WorkerCommand {
    Movement(PlayerMovement),
    /// `chat`/`chat_command` — the worker splits on a leading `/` exactly
    /// like vanilla's chat screen.
    Chat(String),
    /// `use_item` — right-click in air (no block target).
    UseItem { hand: i32, sequence: i32, yaw: f32, pitch: f32 },
    /// `use_item_on` — right-click on a block face.
    UseItemOn {
        hand: i32,
        pos: [i32; 3],
        face: i32,
        cursor: [f32; 3],
        inside: bool,
        sequence: i32,
    },
    /// `player_action` — dig/drop/swap statuses (see `send_player_action`).
    PlayerAction { status: i32, pos: [i32; 3], face: i8, sequence: i32 },
    /// `swing` — arm-swing animation.
    Swing { hand: i32 },
    /// `container_click` — a slot transaction on the open window; slot
    /// payloads are owned `Slot` wire blobs.
    ContainerClick {
        window_id: u8,
        state_id: i32,
        slot: i16,
        button: i8,
        mode: i32,
        changed: Vec<(i16, Vec<u8>)>,
        carried: Vec<u8>,
    },
    /// `container_button_click` — enchant-table-style button.
    ContainerButtonClick { window_id: u8, button_id: i32 },
    /// `container_close` — client-initiated window close.
    ContainerClose { window_id: u8 },
}

/// Live view of the server's open-container instruction: the decoded
/// `open_screen` fields plus slot contents decoded as they arrive.
#[derive(Clone, Debug)]
pub struct LiveOpenScreen {
    pub window_id: i32,
    /// Protocol `menu_type` id — resolves through `menu_schema` on the UI
    /// side (`client-ui::container_screen`).
    pub menu_type: i32,
    /// Raw anonymous-NBT title (text component) exactly as sent.
    pub title_nbt: Vec<u8>,
    /// `container_set_content` `state_id` sequence, echoed back on clicks.
    pub state_id: i32,
    /// Decoded slot patches keyed by window slot index.
    pub slots: BTreeMap<i32, ItemStackComponents>,
    /// Carried (cursor) item, decoded when the server sets one.
    pub carried: Option<ItemStackComponents>,
}

/// Session-side scoreboard mirror: objectives, scores, and sidebar pinning
/// exactly as the vanilla client tracks them for the sidebar overlay.
#[derive(Clone, Debug, Default)]
pub struct LiveScoreboard {
    /// `set_objective` upserts keyed by objective name: `(display_nbt,
    /// render_type, number_format_kind)`.
    pub objectives: BTreeMap<String, (Option<Vec<u8>>, i32, Option<i32>)>,
    /// `set_score` lines keyed by `(objective, owner)` -> value.
    pub scores: BTreeMap<(String, String), i32>,
    /// `set_display_objective` slot -> objective name.
    pub display_slots: BTreeMap<i32, String>,
}

/// One live boss-bar row keyed by its packet UUID.
#[derive(Clone, Debug)]
pub struct LiveBossBar {
    /// Last `boss_event` action (0 add, 1 remove, else update-kind).
    pub action: i32,
    /// Action-dependent payload (title/health/color/dividers/flags) kept
    /// raw for the boss-bar renderer.
    pub raw_payload: Vec<u8>,
}

/// Owns UI-facing world data on the event-loop thread. The worker owns its socket.
pub struct LiveSession {
    incoming: Receiver<WorkerMessage>,
    outgoing: SyncSender<WorkerCommand>,
    _offline_sender: Option<SyncSender<WorkerMessage>>,
    pub status: SessionStatus,
    pub world: WorldState,
    audio: AudioRuntime,
    acoustic_world: WorldAcousticSnapshot,
    entity_positions: BTreeMap<i32, [f64; 3]>,
    /// Vanilla rig classification per tracked entity id, populated on
    /// `add_entity` through `classify_entity_rig` (all 130 EntityType ids).
    entity_rigs: BTreeMap<i32, EntityRigSpec>,
    /// Decoded equipment patches per `(entity_id, equipment_slot)` from
    /// `set_equipment`, decoded through `decode_item_stack_components`.
    entity_equipment: BTreeMap<(i32, u8), ItemStackComponents>,
    /// Local-movement physics mirror advanced by [`Self::predict_movement`]
    /// (sneak edge prevention runs inside `step_player_physics`).
    physics_state: PlayerPhysicsState,
    /// Entity the crosshair ray currently rests on (see `refresh_target`).
    pub looking_at_entity: Option<i32>,
    /// Server's open-container instruction, if any.
    pub open_screen: Option<LiveOpenScreen>,
    /// Scoreboard mirror fed by `set_objective`/`set_score`/
    /// `set_display_objective`/`reset_score`.
    pub scoreboard: LiveScoreboard,
    /// Tab-list roster fed by `player_info_update`/`player_info_remove`.
    pub player_info: BTreeMap<[u8; 16], PlayerInfoUpdateEntry>,
    /// Live boss bars keyed by packet UUID.
    pub boss_bars: BTreeMap<[u8; 16], LiveBossBar>,
    /// `set_titles_animation` timing (ticks): fade-in, stay, fade-out.
    pub title_times: Option<[i32; 3]>,
    /// `set_title_text` raw anonymous-NBT title.
    pub title_text_nbt: Option<Vec<u8>>,
    /// `set_subtitle_text` raw anonymous-NBT subtitle.
    pub subtitle_text_nbt: Option<Vec<u8>>,
    /// `set_action_bar_text` raw anonymous-NBT action-bar line.
    pub actionbar_nbt: Option<Vec<u8>>,
    /// Validated `transfer` reconnect target, if the server issued one.
    pub pending_transfer: Option<TransferTarget>,
    /// Vanilla use-item/container sequence counter echoed on `use_item`,
    /// `use_item_on`, `player_action`, and `container_click` packets.
    use_item_sequence: i32,
    /// Lazily compiled resource-pack bundle (atlas + mip chain + blockstates)
    /// built once when `MC_SPINE_RESOURCE_PACK_DIR` points at a pack root
    /// containing `assets/`; absent env keeps the procedural fallback.
    pub asset_bundle: Option<CompiledAssetBundle>,
    asset_bundle_tried: bool,
    /// Set when `asset_bundle` landed but `GpuRenderer::upload_block_atlas`
    /// has not consumed it yet (window.rs owns the GPU upload).
    pub asset_bundle_pending_upload: bool,
    /// Client-side command dispatcher seeded with the vanilla command-name
    /// set; `complete_suggestions` feeds chat autocomplete while typing.
    pub commands: CommandDispatcher,
    player_entity_id: Option<i32>,
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
        Self::with_channels_and_sink(incoming, outgoing, AudioSinkKind::Auto)
    }

    fn with_channels(
        incoming: Receiver<WorkerMessage>,
        outgoing: SyncSender<WorkerCommand>,
    ) -> Self {
        Self::with_channels_and_sink(incoming, outgoing, AudioSinkKind::Null)
    }

    /// Offline live-session fixture with deterministic null audio output.
    /// Accepts the same chunk, block and sound events as the network path.
    pub fn offline_with_null_audio() -> Self {
        let (tx, incoming) = mpsc::sync_channel(256);
        let (outgoing, _rx) = mpsc::sync_channel(8);
        let mut session = Self::with_channels(incoming, outgoing);
        session._offline_sender = Some(tx);
        session
    }

    fn with_channels_and_sink(
        incoming: Receiver<WorkerMessage>,
        outgoing: SyncSender<WorkerCommand>,
        sink: AudioSinkKind,
    ) -> Self {
        let hw_caps = detect_host_hardware_caps();
        let compute_strategy = select_compute_strategy(&hw_caps);
        let vfx = ClientVfxHost::new(GpuPipelineKind::Classic);
        let tactile_tier = vfx.budget_tier();
        let tactile_config = TactileEffectsConfig::default();
        let world = WorldState::new(DimensionMeta::default(), BlockTable::load(None));
        let acoustic_world = WorldAcousticSnapshot::new(&world);
        let mut audio = AudioRuntime::new(&AudioSessionConfig {
            sink,
            assets_dir: std::env::var_os("MC_SPINE_ASSETS_DIR").map(Into::into),
            asset_index: std::env::var("MC_SPINE_ASSET_INDEX").ok(),
            max_volume_cap: 1.0,
            max_concurrent_same_sound: 8,
            subtitle_path: None,
            subtitles_to_stderr: false,
        });
        audio.mixer.set_world(Box::new(acoustic_world.clone()));
        Self {
            incoming,
            outgoing,
            _offline_sender: None,
            status: SessionStatus::Connecting,
            world,
            audio,
            acoustic_world,
            entity_positions: BTreeMap::new(),
            entity_rigs: BTreeMap::new(),
            entity_equipment: BTreeMap::new(),
            physics_state: PlayerPhysicsState::default(),
            looking_at_entity: None,
            open_screen: None,
            scoreboard: LiveScoreboard::default(),
            player_info: BTreeMap::new(),
            boss_bars: BTreeMap::new(),
            title_times: None,
            title_text_nbt: None,
            subtitle_text_nbt: None,
            actionbar_nbt: None,
            pending_transfer: None,
            use_item_sequence: 0,
            asset_bundle: None,
            asset_bundle_tried: false,
            asset_bundle_pending_upload: false,
            commands: seeded_command_dispatcher(),
            player_entity_id: None,
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

    /// Environment values currently affecting spatial DSP.
    pub fn audio_environment_hint(&self) -> EnvironmentAudioHint {
        self.audio.mixer.environment_hint()
    }

    /// Drain the UI's spatial cue feed independently from audio-side logging.
    pub fn drain_subtitle_cues(&mut self) -> Vec<SpatialSubtitleCue> {
        self.audio.mixer.drain_spatial_subtitle_cues()
    }

    pub fn audio_stats(&self) -> &AudioStats {
        self.audio.stats()
    }

    pub fn audio_sink_label(&self) -> &'static str {
        self.audio.sink_label()
    }

    /// Deterministic audio pump for embedded sessions and integration tests.
    pub fn pump_audio_frames(&mut self, frames: usize) {
        self.audio.pump_frames(frames);
    }

    /// Update a known entity location for entity-bound sound packets.
    pub fn set_entity_position(&mut self, entity_id: i32, position: [f64; 3]) {
        self.entity_positions.insert(entity_id, position);
    }

    pub fn remove_entity_position(&mut self, entity_id: i32) {
        self.entity_positions.remove(&entity_id);
    }

    /// Accept a decoded 0x68 or 0x67 sound event on the interactive client.
    pub fn ingest_sound_event(&mut self, mut event: WireSoundEvent) {
        if let Some(position) = event
            .entity_id
            .and_then(|id| self.entity_positions.get(&id))
        {
            event.position = Some(position.map(|coord| coord as f32));
            event.entity_id = None;
        }
        self.audio.ingest(&event);
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
    pub fn on_impact_sparks(&mut self, pos: [f32; 3], normal: [f32; 3], material: ImpactMaterial) {
        let tick = self.next_tactile_event_tick();
        self.tactile_sim
            .emit_impact_sparks(pos, normal, material, tick);
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
        self.audio
            .mixer
            .set_environment_hint(self.latest_audio_hint);
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
        if !dx.is_finite()
            || !dy.is_finite()
            || !dz.is_finite()
            || dx * dx + dy * dy + dz * dz > 16.0
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
                Ok(WorkerMessage::Sound(event)) => self.ingest_sound_event(event),
                Ok(WorkerMessage::StopSound(event)) => self.audio.ingest_stop(&event),
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
        self.audio.pump();
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
        self.update_audio_listener();
        self.refresh_prefetcher();
        let _ = self.outgoing.try_send(WorkerCommand::Movement(movement));
    }

    /// `chat`/`chat_command` — send one chat line; a leading `/` rides the
    /// `chat_command` packet per vanilla's sendChat semantics.
    pub fn send_chat(&mut self, message: &str) {
        if !matches!(self.status, SessionStatus::Playing) {
            return;
        }
        let _ = self.outgoing.try_send(WorkerCommand::Chat(message.to_string()));
    }

    /// `use_item` — right-click in air with `hand` (0 main, 1 off), using
    /// the current look angles and the next interaction sequence.
    pub fn use_item(&mut self, hand: i32) {
        let Some(pose) = self.pose else { return };
        self.use_item_sequence = self.use_item_sequence.wrapping_add(1);
        let _ = self.outgoing.try_send(WorkerCommand::UseItem {
            hand,
            sequence: self.use_item_sequence,
            yaw: pose.yaw,
            pitch: pose.pitch,
        });
    }

    /// `use_item_on` — right-click on block `pos`/`face`; `cursor` is the
    /// in-block hit position (each 0..=1), `inside` the inside-block flag.
    pub fn use_item_on(
        &mut self,
        hand: i32,
        pos: [i32; 3],
        face: i32,
        cursor: [f32; 3],
        inside: bool,
    ) {
        self.use_item_sequence = self.use_item_sequence.wrapping_add(1);
        let _ = self.outgoing.try_send(WorkerCommand::UseItemOn {
            hand,
            pos,
            face,
            cursor,
            inside,
            sequence: self.use_item_sequence,
        });
    }

    /// `player_action` drop: `Q` drops one item, `Ctrl+Q` the whole stack
    /// (vanilla statuses `drop_item` = 4 / `drop_item_stack` = 3 at
    /// `BlockPos.ZERO` facing down).
    pub fn drop_item(&mut self, entire_stack: bool) {
        self.use_item_sequence = self.use_item_sequence.wrapping_add(1);
        let _ = self.outgoing.try_send(WorkerCommand::PlayerAction {
            status: if entire_stack { 3 } else { 4 },
            pos: [0; 3],
            face: 0,
            sequence: self.use_item_sequence,
        });
    }

    /// `player_action` swap-hands: vanilla `F` keybind (status 6).
    pub fn swap_offhand(&mut self) {
        self.use_item_sequence = self.use_item_sequence.wrapping_add(1);
        let _ = self.outgoing.try_send(WorkerCommand::PlayerAction {
            status: 6,
            pos: [0; 3],
            face: 0,
            sequence: self.use_item_sequence,
        });
    }

    /// `swing` — arm-swing animation for `hand`.
    pub fn swing(&mut self, hand: i32) {
        let _ = self.outgoing.try_send(WorkerCommand::Swing { hand });
    }

    /// `container_click` — one window transaction; `changed`/`carried` are
    /// raw `Slot` wire blobs (build simple stacks via
    /// `mc_spine_client::protocol::encode_slot_stack`).
    #[allow(clippy::too_many_arguments)]
    pub fn container_click(
        &mut self,
        window_id: u8,
        state_id: i32,
        slot: i16,
        button: i8,
        mode: i32,
        changed: Vec<(i16, Vec<u8>)>,
        carried: Vec<u8>,
    ) {
        let _ = self.outgoing.try_send(WorkerCommand::ContainerClick {
            window_id,
            state_id,
            slot,
            button,
            mode,
            changed,
            carried,
        });
    }

    /// `container_button_click` — enchant-table-style button press.
    pub fn container_button_click(&mut self, window_id: u8, button_id: i32) {
        let _ = self
            .outgoing
            .try_send(WorkerCommand::ContainerButtonClick { window_id, button_id });
    }

    /// `container_close` — client-initiated close; also clears the tracked
    /// open screen when it matches `window_id`.
    pub fn container_close(&mut self, window_id: u8) {
        if self
            .open_screen
            .as_ref()
            .is_some_and(|screen| screen.window_id == i32::from(window_id))
        {
            self.open_screen = None;
        }
        let _ = self
            .outgoing
            .try_send(WorkerCommand::ContainerClose { window_id });
    }

    /// Take the validated `transfer` reconnect target, if one arrived.
    pub fn take_pending_transfer(&mut self) -> Option<TransferTarget> {
        self.pending_transfer.take()
    }

    /// The classified rig for a tracked entity (set on `add_entity`).
    pub fn entity_rig(&self, entity_id: i32) -> Option<&EntityRigSpec> {
        self.entity_rigs.get(&entity_id)
    }

    /// Local-movement prediction against live world collision: mirrors
    /// `sim.rs`'s shared-physics step so the GPU session gets vanilla
    /// collision, step-up, fluid drag, and sneak edge protection
    /// (`step_player_physics` -> `back_off_from_edge`).
    pub fn predict_movement(
        &mut self,
        strafe: f32,
        forward: f32,
        jump: bool,
        sneak: bool,
        sprint: bool,
        yaw: f32,
        pitch: f32,
    ) -> Option<PlayerPose> {
        if !matches!(self.status, SessionStatus::Playing) {
            return None;
        }
        let base = self.pose.unwrap_or(PlayerPose {
            feet: self.physics_state.pos,
            yaw,
            pitch,
        });
        self.physics_state.pos = base.feet;
        let bounds = self.physics_state.aabb();
        let margin = self
            .physics_state
            .vel
            .map(|velocity| velocity.abs() + 1.0);
        let sweep = Aabb {
            min: std::array::from_fn(|axis| bounds.min[axis] - margin[axis]),
            max: std::array::from_fn(|axis| bounds.max[axis] + margin[axis]),
        };
        let shapes = self.world.collect_collision_aabbs(sweep);
        let feet = base.feet.map(|v| v.floor() as i32);
        let inside = surface_kind(&self.world, feet);
        let surface = if matches!(
            inside,
            SurfaceKind::Ladder
                | SurfaceKind::Vine
                | SurfaceKind::Scaffolding
                | SurfaceKind::Cobweb
                | SurfaceKind::PowderSnow
        ) {
            inside
        } else {
            surface_kind(
                &self.world,
                [feet[0], (base.feet[1] - 0.500_000_1).floor() as i32, feet[2]],
            )
        };
        let state = PlayerPhysicsState {
            pos: base.feet,
            ultra_warm: self.world.dimension.name == "minecraft:the_nether",
            ..self.physics_state
        };
        let world = &self.world;
        let result = step_player_physics(
            state,
            PlayerInputState {
                forward,
                strafe,
                jump,
                sneak,
                sprint,
                yaw_deg: yaw,
                pitch_deg: pitch,
                ..PlayerInputState::default()
            },
            surface,
            &shapes,
            |x, y, z| Ok(world.fluid_cell_at(x, y, z)),
        )
        .ok()?;
        self.physics_state = result.state;
        Some(PlayerPose {
            feet: result.state.pos,
            yaw,
            pitch,
        })
    }

    /// Entity under the crosshair ray (vanilla 4.5-block attack reach,
    /// matching `GameRenderer.pick` entity hit testing).
    pub fn crosshair_entity(&self) -> Option<(i32, f64)> {
        let pose = self.pose?;
        let eye = [pose.feet[0], pose.feet[1] + 1.62, pose.feet[2]];
        let yaw = f64::from(pose.yaw).to_radians();
        let pitch = f64::from(pose.pitch).to_radians();
        let direction = [
            -yaw.sin() * pitch.cos(),
            -pitch.sin(),
            yaw.cos() * pitch.cos(),
        ];
        let mut targets: Vec<(i32, Aabb)> = Vec::with_capacity(self.world.entities.len().min(256));
        for (eid, tracked) in self.world.entities.iter().take(4096) {
            if Some(*eid) == self.player_entity_id {
                continue;
            }
            let (w, h) = self
                .entity_rigs
                .get(eid)
                .map(|spec| (f64::from(spec.width), f64::from(spec.height)))
                .unwrap_or((0.6, 1.8));
            targets.push((
                *eid,
                Aabb {
                    min: [
                        tracked.pos[0] - w / 2.0,
                        tracked.pos[1],
                        tracked.pos[2] - w / 2.0,
                    ],
                    max: [
                        tracked.pos[0] + w / 2.0,
                        tracked.pos[1] + h,
                        tracked.pos[2] + w / 2.0,
                    ],
                },
            ));
        }
        raycast_entities(eye, direction, 4.5, &targets)
            .map(|hit| (hit.entity_id, hit.distance))
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

    fn update_audio_listener(&mut self) {
        if let Some(pose) = self.pose {
            self.audio
                .update_listener(pose.feet, Some(pose.yaw), Some(pose.pitch));
            if let Some(entity_id) = self.player_entity_id {
                self.entity_positions.insert(entity_id, pose.feet);
            }
        }
    }

    /// Compile the resource-pack bundle once (`MC_SPINE_RESOURCE_PACK_DIR`
    /// = pack root containing `assets/`). `CompiledAssetBundle::build`
    /// produces the gutter-padded block atlas and its linear-sRGB mip
    /// chain; `window.rs` uploads it via `upload_block_atlas`.
    fn ensure_asset_bundle(&mut self) {
        if self.asset_bundle_tried {
            return;
        }
        self.asset_bundle_tried = true;
        let Some(dir) = std::env::var_os("MC_SPINE_RESOURCE_PACK_DIR") else {
            return;
        };
        match ResourcePackLayer::from_dir(std::path::Path::new(&dir))
            .and_then(|layer| CompiledAssetBundle::build(&[layer]))
        {
            Ok(bundle) => {
                if !bundle.skipped_errors.is_empty() {
                    eprintln!(
                        "mc-spine-client assets: {} pack entries skipped",
                        bundle.skipped_errors.len()
                    );
                }
                self.asset_bundle = Some(bundle);
                self.asset_bundle_pending_upload = true;
            }
            Err(err) => eprintln!("mc-spine-client assets: bundle build failed: {err}"),
        }
        self.register_cem_models(std::path::Path::new(&dir));
    }

    /// Scan `assets/*/optifine/cem/*.jem|*.jpm` in the loaded pack and register
    /// each custom entity model in `world.animations` under its file stem
    /// (`zombie.jem` -> `zombie`), matching OptiFine's CEM lookup. Unreadable
    /// or malformed files are logged and skipped like other pack entries.
    fn register_cem_models(&mut self, pack_root: &std::path::Path) {
        let assets = pack_root.join("assets");
        let Ok(namespaces) = std::fs::read_dir(&assets) else {
            return;
        };
        for namespace in namespaces.flatten() {
            let cem = namespace.path().join("optifine/cem");
            let Ok(entries) = std::fs::read_dir(&cem) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
                    continue;
                };
                let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                    continue;
                };
                let Ok(text) = std::fs::read_to_string(&path) else {
                    continue;
                };
                let result = match ext {
                    "jem" => self.world.animations.register_jem_model(stem, &text),
                    "jpm" => self.world.animations.register_jpm_model(stem, &text),
                    _ => continue,
                };
                if let Err(err) = result {
                    eprintln!("mc-spine-client assets: CEM model {path:?} skipped: {err:?}");
                }
            }
        }
    }

    /// Regenerate the layered voxel-cloud sheet from the live camera/world
    /// time and mirror it into `subworlds` for the translucent lit pass.
    /// Cells come from `generate_layered_voxel_clouds`; the pack's
    /// `enabled`/`layered_voxel_clouds` toggles gate emission (an empty set
    /// removes the sheet instead of leaving a stale frame).
    fn refresh_voxel_clouds(&mut self) {
        let (cam_x, cam_z) = self
            .pose
            .map(|pose| (pose.feet[0] as f32, pose.feet[2] as f32))
            .unwrap_or((0.0, 0.0));
        let time = self.world.time_of_day.max(0) as u64;
        let cells =
            generate_layered_voxel_clouds(&self.vanilla_plus_cfg.nature, cam_x, cam_z, time);
        if cells.is_empty() {
            self.subworlds.remove(CLOUD_SUBWORLD_ID);
            return;
        }
        // Vanilla celestial angle: 0 = sunrise, 6000 = noon, 18000 = midnight.
        let sun_elevation = ((time % 24_000) as f32 * (std::f32::consts::TAU / 24_000.0)).sin();
        let lighting = compute_cloud_lighting(
            sun_elevation,
            self.world.rain_level,
            self.world.thunder_level,
        );
        let (vertices, indices) = voxel_clouds_to_geometry(&cells, &lighting);
        let mut entry = SubWorldRenderEntry::new(
            CLOUD_SUBWORLD_ID,
            SubWorldTransform::IDENTITY,
            vertices,
            indices,
        );
        entry.alpha = CLOUD_RENDER_ALPHA;
        self.subworlds.upsert(entry);
    }

    /// Record frame timing and advance visual transitions before HUD rendering.
    pub fn record_frame_and_refresh_minihud(&mut self, frame_dt_sec: f32, ui: &mut UiState) {
        let frame_us = f64::from(frame_dt_sec) * 1_000_000.0;
        self.frame_timings.push_frame_ms(frame_us / 1_000.0);
        self.chunk_fades.tick(frame_dt_sec);
        self.atmosphere_blender.advance(frame_dt_sec);
        self.refresh_subtitle_hud(frame_dt_sec, &mut ui.hud);
        self.looking_at_entity = self.crosshair_entity().map(|(eid, _)| eid);
        self.ensure_asset_bundle();
        self.refresh_voxel_clouds();

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

    /// Drain the mixer's spatial subtitle feed into the live HUD state.
    ///
    /// Every admitted sound already arrives with the event label stamped by
    /// the mixer; `SubtitleHudState` dedups repeat plays of the same event
    /// onto one row. `self.hud` stays the canonical store — the window
    /// clones `live.hud` into `ui.hud` before this runs each frame — so the
    /// post-clone `hud` gets the updated snapshot mirrored back.
    fn refresh_subtitle_hud(&mut self, frame_dt_sec: f32, hud: &mut HudState) {
        self.hud.subtitle_hud.tick(frame_dt_sec);
        for cue in self.drain_subtitle_cues() {
            let id = if cue.label.is_empty() {
                "sound.event".to_owned()
            } else {
                cue.label
            };
            let arrow = match cue.direction {
                SubtitleDirection::Left => SubtitleArrow::Left,
                SubtitleDirection::Center => SubtitleArrow::Center,
                SubtitleDirection::Right => SubtitleArrow::Right,
                SubtitleDirection::Behind => SubtitleArrow::Behind,
            };
            self.hud
                .subtitle_hud
                .push(id.clone(), subtitle_label(&id), arrow, DEFAULT_TTL_SECS);
        }
        hud.subtitle_hud = self.hud.subtitle_hud.clone();
    }

    /// Apply a decoded packet, also used by framed protocol regression tests.
    pub fn apply_event(&mut self, event: ClientPlayEvent) -> Result<(), String> {
        match event {
            ClientPlayEvent::Play(event) => self.apply_play(event),
            ClientPlayEvent::CustomChannel(channel) => self.apply_custom(channel),
        }
    }

    fn apply_play(&mut self, event: PlayEvent) -> Result<(), String> {
        // World-side tracking (chunks, block/light updates, entities, block
        // entities, time/weather, break overlays) is owned by
        // `WorldState::apply_play_event`; session-side concerns stay below.
        self.world.apply_play_event(&event)?;
        match event {
            PlayEvent::JoinGame { entity_id, .. } => {
                self.player_entity_id = Some(entity_id);
                self.reset_dimension();
            }
            PlayEvent::Respawn { .. } => self.reset_dimension(),
            PlayEvent::Teleport(teleport) => {
                let pose = PlayerPose::from_teleport(&teleport, self.pose);
                self.pose = Some(pose);
                self.camera_sync = Some(pose);
                self.physics_state.pos = pose.feet;
                self.update_audio_listener();
            }
            PlayEvent::ChunkData(chunk) => {
                let (cx, cz) = (chunk.cx, chunk.cz);
                self.acoustic_world.sync_chunk(&self.world, cx, cz);
                self.audio.mixer.refresh_acoustics();
                self.chunk_fades.note_chunk(cx, cz);
                self.refresh_prefetcher();
                self.terrain_dirty = true;
            }
            PlayEvent::BlockUpdate { pos, .. } => {
                self.acoustic_world.sync_block(&self.world, pos);
                self.audio.mixer.refresh_acoustics();
            }
            PlayEvent::SectionBlocksUpdate(changes) => {
                for (pos, _) in &changes {
                    self.acoustic_world.sync_block(&self.world, *pos);
                }
                self.audio.mixer.refresh_acoustics();
            }
            PlayEvent::UnloadChunk { cx, cz } => {
                self.acoustic_world.unload_chunk(cx, cz);
                self.audio.mixer.refresh_acoustics();
                self.chunk_fades.forget(cx, cz);
                self.refresh_prefetcher();
                self.terrain_dirty = true;
            }
            PlayEvent::AddEntity {
                eid,
                entity_type,
                x,
                y,
                z,
                ..
            } => {
                self.entity_positions.insert(eid, [x, y, z]);
                if let Ok(type_id) = u16::try_from(entity_type) {
                    if let Some(spec) = classify_entity_rig(type_id) {
                        // OptiFine CEM: a registered `.jem`/`.jpm` model whose
                        // file stem is the un-namespaced entity name overrides
                        // the baked rig. Ignored when the pack has no match.
                        let cem_name = spec
                            .name
                            .strip_prefix("minecraft:")
                            .unwrap_or(spec.name);
                        let _ = self.world.animations.bind_entity(eid, cem_name);
                        self.entity_rigs.insert(eid, spec);
                    }
                }
            }
            PlayEvent::RemoveEntities { eids } => {
                for eid in &eids {
                    self.entity_positions.remove(eid);
                    self.entity_rigs.remove(eid);
                    self.entity_equipment.retain(|(e, _), _| e != eid);
                }
            }
            PlayEvent::MoveEntityPos { eid, .. }
            | PlayEvent::MoveEntityPosRot { eid, .. }
            | PlayEvent::MoveEntityRot { eid, .. }
            | PlayEvent::TeleportEntity { eid, .. } => {
                if let Some(tracked) = self.world.entities.get(&eid) {
                    self.entity_positions.insert(eid, tracked.pos);
                }
            }
            PlayEvent::SetEquipment { eid, entries } => {
                for entry in &entries {
                    match decode_item_stack_components(&entry.item_wire) {
                        Ok((stack, _)) => {
                            self.entity_equipment.insert((eid, entry.slot), stack);
                        }
                        Err(_) => {
                            self.entity_equipment.remove(&(eid, entry.slot));
                        }
                    }
                }
            }
            PlayEvent::OpenScreen {
                window_id,
                menu_type,
                title_nbt,
            } => {
                self.open_screen = Some(LiveOpenScreen {
                    window_id,
                    menu_type,
                    title_nbt,
                    state_id: 0,
                    slots: BTreeMap::new(),
                    carried: None,
                });
            }
            PlayEvent::ContainerClose { window_id } => {
                if self
                    .open_screen
                    .as_ref()
                    .is_some_and(|screen| screen.window_id == i32::from(window_id))
                {
                    self.open_screen = None;
                }
            }
            PlayEvent::ContainerSetContent {
                window_id,
                state_id,
                slot_count,
                raw_payload,
            } => {
                if let Some(screen) = &mut self.open_screen {
                    if screen.window_id == i32::from(window_id) {
                        screen.state_id = state_id;
                        let mut offset = 0usize;
                        for index in 0..slot_count {
                            let tail = &raw_payload[offset.min(raw_payload.len())..];
                            match decode_item_stack_components(tail) {
                                Ok((stack, consumed)) => {
                                    offset += consumed;
                                    screen.slots.insert(index, stack);
                                }
                                Err(_) => break,
                            }
                        }
                        // The carried item follows the slot array.
                        if offset < raw_payload.len() {
                            if let Ok((carried, _)) =
                                decode_item_stack_components(&raw_payload[offset..])
                            {
                                screen.carried = Some(carried);
                            }
                        }
                    }
                }
            }
            PlayEvent::ContainerSetSlot {
                window_id,
                state_id,
                slot,
                raw_item,
            } => {
                if let Some(screen) = &mut self.open_screen {
                    if screen.window_id == i32::from(window_id) {
                        screen.state_id = state_id;
                        match decode_item_stack_components(&raw_item) {
                            Ok((stack, _)) => {
                                if slot < 0 {
                                    screen.carried = Some(stack);
                                } else {
                                    screen.slots.insert(i32::from(slot), stack);
                                }
                            }
                            Err(_) => {
                                screen.slots.remove(&i32::from(slot));
                            }
                        }
                    }
                }
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
            PlayEvent::SetActionBarText { nbt_bytes } => {
                self.actionbar_nbt = Some(nbt_bytes);
            }
            PlayEvent::SetTitle {
                fade_in,
                stay,
                fade_out,
            } => {
                self.title_times = Some([fade_in, stay, fade_out]);
            }
            PlayEvent::SetTitleText { nbt_bytes } => {
                self.title_text_nbt = Some(nbt_bytes);
            }
            PlayEvent::SetSubtitleText { nbt_bytes } => {
                self.subtitle_text_nbt = Some(nbt_bytes);
            }
            PlayEvent::BossEvent {
                uuid,
                action,
                raw_payload,
            } => {
                if action == 1 {
                    self.boss_bars.remove(&uuid);
                } else {
                    self.boss_bars
                        .insert(uuid, LiveBossBar { action, raw_payload });
                }
            }
            PlayEvent::PlayerInfo { actions, entries } => {
                for update in &entries {
                    let entry =
                        self.player_info.entry(update.uuid).or_insert_with(|| {
                            PlayerInfoUpdateEntry {
                                uuid: update.uuid,
                                name: None,
                                properties: Vec::new(),
                                chat_session: None,
                                game_mode: None,
                                listed: None,
                                latency: None,
                                display_name_nbt: None,
                            }
                        });
                    if actions & 0x01 != 0 {
                        entry.name = update.name.clone();
                        entry.properties = update.properties.clone();
                    }
                    if actions & 0x02 != 0 {
                        entry.chat_session = update.chat_session.clone();
                    }
                    if actions & 0x04 != 0 {
                        entry.game_mode = update.game_mode;
                    }
                    if actions & 0x08 != 0 {
                        entry.listed = update.listed;
                    }
                    if actions & 0x10 != 0 {
                        entry.latency = update.latency;
                    }
                    if actions & 0x20 != 0 {
                        entry.display_name_nbt = update.display_name_nbt.clone();
                    }
                }
            }
            PlayEvent::PlayerInfoRemove { uuids } => {
                for uuid in &uuids {
                    self.player_info.remove(uuid);
                }
            }
            PlayEvent::SetObjective {
                objective,
                mode,
                display_name_nbt,
                render_type,
                number_format_kind,
                ..
            } => {
                if mode == 1 {
                    self.scoreboard.objectives.remove(&objective);
                    self.scoreboard
                        .scores
                        .retain(|(obj, _), _| obj != &objective);
                    self.scoreboard
                        .display_slots
                        .retain(|_, obj| obj != &objective);
                } else {
                    self.scoreboard.objectives.insert(
                        objective,
                        (
                            display_name_nbt,
                            render_type.unwrap_or(0),
                            number_format_kind,
                        ),
                    );
                }
            }
            PlayEvent::SetScore {
                owner,
                objective,
                value,
                ..
            } => {
                self.scoreboard.scores.insert((objective, owner), value);
            }
            PlayEvent::SetDisplayObjective { slot, objective } => {
                self.scoreboard.display_slots.insert(slot, objective);
            }
            PlayEvent::ResetScore { owner, objective } => {
                match objective {
                    Some(objective) => self
                        .scoreboard
                        .scores
                        .retain(|(obj, who), _| obj != &objective || who != &owner),
                    None => self
                        .scoreboard
                        .scores
                        .retain(|(_, who), _| who != &owner),
                }
            }
            PlayEvent::Transfer { host, port } => match handle_transfer_packet(&host, port) {
                Ok(target) => self.pending_transfer = Some(target),
                Err(reason) => {
                    self.status = SessionStatus::Disconnected(reason);
                }
            },
            PlayEvent::Disconnect(reason) => self.status = SessionStatus::Disconnected(reason),
            _ => {}
        }
        Ok(())
    }

    fn reset_dimension(&mut self) {
        self.acoustic_world.reset_dimension(&self.world);
        self.audio.mixer.refresh_acoustics();
        self.entity_positions.clear();
        self.terrain_dirty = true;
        self.pose = None;
        self.last_motion_delta = [0.0; 2];
        self.chunk_fades = ChunkFadeTracker::new();
        self.prefetcher = SpeculativeChunkPrefetcher::new();
        // A dimension change invalidates the previous dimension's
        // season frame: restore the untinted atmosphere base instead
        // of snapping to a possibly season-tinted target, and drop
        // the SSN1 bookkeeping so a stale seasonal grade (or its
        // aurora/snow ambience) cannot leak into the new dimension.
        self.latest_season = None;
        self.season_ambience = SeasonAmbienceInput::default();
        self.nature_env.season = self.season_ambience;
        self.seasoned_atmosphere_target = None;
        self.atmosphere_blender
            .set_target(self.base_atmosphere_target);
        self.atmosphere_blender.snap_to_target();
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

/// Humanize a sound-event name into a subtitle row label: strip the
/// `minecraft:` namespace, drop the `subtitles.` prefix when present, and
/// turn `block.stone.break`/`entity_player_levelup`-style ids into
/// `block stone break` — the clean-room stand-in for localized subtitle
/// text until lang assets land.
fn subtitle_label(name: &str) -> String {
    let tail = name
        .trim()
        .trim_start_matches("minecraft:")
        .trim_start_matches("subtitles.");
    let mut out = String::with_capacity(tail.len());
    for ch in tail.chars() {
        if ch == '.' || ch == '_' || ch == '/' || ch == '-' {
            if !out.ends_with(' ') && !out.is_empty() {
                out.push(' ');
            }
        } else {
            out.push(ch);
        }
    }
    if out.is_empty() {
        out.push_str("sound");
    }
    out
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
    // GPU session owns the sink and spatial world. Prevent any global
    // headless audio configuration from creating a second active playback.
    client.audio = None;
    if tx.send(WorkerMessage::Connected).is_err() {
        return Ok(());
    }
    let mut last_input = None;
    let mut last_send = Instant::now();
    loop {
        loop {
            match rx.try_recv() {
                Ok(WorkerCommand::Movement(movement)) => last_input = Some(movement),
                Ok(WorkerCommand::Chat(message)) => {
                    client.send_chat_message(&message)?;
                }
                Ok(WorkerCommand::UseItem {
                    hand,
                    sequence,
                    yaw,
                    pitch,
                }) => {
                    client.send_use_item(hand, sequence, yaw, pitch)?;
                }
                Ok(WorkerCommand::UseItemOn {
                    hand,
                    pos,
                    face,
                    cursor,
                    inside,
                    sequence,
                }) => {
                    client.send_use_item_on(hand, pos, face, cursor, inside, sequence)?;
                }
                Ok(WorkerCommand::PlayerAction {
                    status,
                    pos,
                    face,
                    sequence,
                }) => {
                    client.send_player_action(status, pos, face, sequence)?;
                }
                Ok(WorkerCommand::Swing { hand }) => {
                    client.send_swing(hand)?;
                }
                Ok(WorkerCommand::ContainerClick {
                    window_id,
                    state_id,
                    slot,
                    button,
                    mode,
                    changed,
                    carried,
                }) => {
                    let borrowed: Vec<(i16, &[u8])> = changed
                        .iter()
                        .map(|(slot, item)| (*slot, item.as_slice()))
                        .collect();
                    client.send_container_click(
                        window_id,
                        state_id,
                        slot,
                        button,
                        mode,
                        &borrowed,
                        &carried,
                    )?;
                }
                Ok(WorkerCommand::ContainerButtonClick {
                    window_id,
                    button_id,
                }) => {
                    client.send_container_button_click(window_id, button_id)?;
                }
                Ok(WorkerCommand::ContainerClose { window_id }) => {
                    client.send_container_close(window_id)?;
                }
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
        for event in client.sound_events.drain(..) {
            if tx.send(WorkerMessage::Sound(event)).is_err() {
                return Ok(());
            }
        }
        for event in client.stop_sound_events.drain(..) {
            if tx.send(WorkerMessage::StopSound(event)).is_err() {
                return Ok(());
            }
        }
    }
}

/// Reserved `subworlds` id for the layered voxel-cloud sheet (kept clear of
/// contraption ids, which allocate from 1 upward).
const CLOUD_SUBWORLD_ID: u64 = u64::MAX;

/// Seed the session command dispatcher with the vanilla command-name set
/// (`zsh_command_bar`'s `COMMAND_SPECS`) so `complete_suggestions` answers
/// the same literals the bar highlights before any `declare_commands`.
fn seeded_command_dispatcher() -> CommandDispatcher {
    let mut dispatcher = CommandDispatcher::new();
    for name in mc_spine_client::zsh_command_bar::vanilla_command_names() {
        let _ = dispatcher.register_literal(name);
    }
    dispatcher
}

/// Resolve the vanilla `SurfaceKind` for the block at `pos`, consulting the
/// version-pack registry name first and falling back to the built-in 1.21.1
/// state-id table — the same two-tier rule `sim.rs` applies.
fn surface_kind(world: &WorldState, pos: [i32; 3]) -> SurfaceKind {
    let Some(state_id) = world.get_block_state(pos[0], pos[1], pos[2]) else {
        return SurfaceKind::Normal;
    };
    let name = world
        .registry
        .as_ref()
        .and_then(|registry| registry.state_name(state_id).ok());
    match name {
        Some("minecraft:ice") => SurfaceKind::Ice,
        Some("minecraft:packed_ice") => SurfaceKind::PackedIce,
        Some("minecraft:blue_ice") => SurfaceKind::BlueIce,
        Some("minecraft:slime_block") => SurfaceKind::Slime,
        Some("minecraft:honey_block") => SurfaceKind::Honey,
        Some("minecraft:soul_sand") => SurfaceKind::SoulSand,
        Some("minecraft:cobweb") => SurfaceKind::Cobweb,
        Some("minecraft:ladder") => SurfaceKind::Ladder,
        Some("minecraft:vine") => SurfaceKind::Vine,
        Some("minecraft:scaffolding") => SurfaceKind::Scaffolding,
        Some("minecraft:powder_snow") => SurfaceKind::PowderSnow,
        _ => match state_id {
            // Vanilla 1.21.1 state ids used by BlockTable::builtin() when the
            // version-pack registry is unavailable.
            5780 => SurfaceKind::Ice,
            10746 => SurfaceKind::PackedIce,
            12941 => SurfaceKind::BlueIce,
            10364 => SurfaceKind::Slime,
            19445 => SurfaceKind::Honey,
            5850 => SurfaceKind::SoulSand,
            2004 => SurfaceKind::Cobweb,
            4654..=4661 => SurfaceKind::Ladder,
            6837..=6868 => SurfaceKind::Vine,
            18372..=18403 => SurfaceKind::Scaffolding,
            22318 => SurfaceKind::PowderSnow,
            _ => SurfaceKind::Normal,
        },
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
        DecodedMotionPhase, EquipmentEntry, WireReader, CHANNEL_ANIM_V1, CHANNEL_MAGIC_V1,
        CHANNEL_MOTION_V1, CHANNEL_SEASON_V1, CHANNEL_VFX_V1, SSN1_MAGIC, SSN1_VERSION,
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
        live.apply_event(framed(CHANNEL_SEASON_V1, &autumn))
            .unwrap();
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
        live.apply_event(framed(CHANNEL_SEASON_V1, &winter))
            .unwrap();
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
    fn respawn_clears_stale_season_tint_and_ambience() {
        let (mut live, _tx) = session();
        // Install a winter SSN1 frame: tinted atmosphere target + winter
        // ambience state.
        let winter = ssn1_frame(10, 49_151, [235, 240, 255], [220, 230, 240], 0b100);
        live.apply_event(framed(CHANNEL_SEASON_V1, &winter))
            .unwrap();
        assert!(!live.season_ambience.is_seasonless);
        assert_ne!(
            live.atmosphere_blender.target(),
            live.base_atmosphere_target
        );

        // Respawning into another dimension must not keep the old
        // dimension's seasonal grade or ambience: the untinted base is
        // restored and the SSN1 bookkeeping resets until the next frame.
        live.apply_event(ClientPlayEvent::Play(PlayEvent::Respawn {
            dimension: DimensionMeta::default(),
        }))
        .unwrap();
        assert!(live.latest_season.is_none());
        assert!(live.season_ambience.is_seasonless);
        assert!(live.nature_env.season.is_seasonless);
        assert_eq!(
            live.atmosphere_blender.target(),
            live.base_atmosphere_target
        );
        assert_eq!(
            live.atmosphere_blender.current(),
            live.base_atmosphere_target
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
        live.apply_event(framed(CHANNEL_SEASON_V1, &winter))
            .unwrap();
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
        live.apply_event(framed(CHANNEL_SEASON_V1, &spring))
            .unwrap();
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
        live.apply_event(framed(CHANNEL_SEASON_V1, &autumn))
            .unwrap();
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

    #[test]
    fn play_events_drive_scoreboard_bossbar_tablist_and_titles() {
        let (mut live, _tx) = session();
        live.apply_event(ClientPlayEvent::Play(PlayEvent::SetObjective {
            objective: "kills".into(),
            mode: 0,
            display_name_nbt: Some(vec![8, 0, 0, 0]),
            render_type: Some(0),
            number_format_kind: None,
            number_format_nbt: None,
        }))
        .unwrap();
        live.apply_event(ClientPlayEvent::Play(PlayEvent::SetScore {
            owner: "Steve".into(),
            objective: "kills".into(),
            value: 42,
            display_nbt: None,
            number_format_kind: None,
        }))
        .unwrap();
        live.apply_event(ClientPlayEvent::Play(PlayEvent::SetDisplayObjective {
            slot: 1,
            objective: "kills".into(),
        }))
        .unwrap();
        assert!(live.scoreboard.objectives.contains_key("kills"));
        assert_eq!(
            live.scoreboard
                .scores
                .get(&("kills".to_string(), "Steve".to_string())),
            Some(&42)
        );
        assert_eq!(live.scoreboard.display_slots.get(&1).map(String::as_str), Some("kills"));

        live.apply_event(ClientPlayEvent::Play(PlayEvent::ResetScore {
            owner: "Steve".into(),
            objective: Some("kills".into()),
        }))
        .unwrap();
        assert!(live.scoreboard.scores.is_empty());
        live.apply_event(ClientPlayEvent::Play(PlayEvent::SetObjective {
            objective: "kills".into(),
            mode: 1,
            display_name_nbt: None,
            render_type: None,
            number_format_kind: None,
            number_format_nbt: None,
        }))
        .unwrap();
        assert!(live.scoreboard.objectives.is_empty());
        assert!(live.scoreboard.display_slots.is_empty());

        let uuid = [7u8; 16];
        live.apply_event(ClientPlayEvent::Play(PlayEvent::BossEvent {
            uuid,
            action: 0,
            raw_payload: vec![1, 2, 3],
        }))
        .unwrap();
        assert_eq!(live.boss_bars[&uuid].action, 0);
        live.apply_event(ClientPlayEvent::Play(PlayEvent::BossEvent {
            uuid,
            action: 1,
            raw_payload: Vec::new(),
        }))
        .unwrap();
        assert!(live.boss_bars.is_empty());

        live.apply_event(ClientPlayEvent::Play(PlayEvent::PlayerInfo {
            actions: 0x19,
            entries: vec![PlayerInfoUpdateEntry {
                uuid,
                name: Some("Alex".into()),
                properties: Vec::new(),
                chat_session: None,
                game_mode: None,
                listed: Some(true),
                latency: Some(17),
                display_name_nbt: None,
            }],
        }))
        .unwrap();
        assert_eq!(live.player_info[&uuid].name.as_deref(), Some("Alex"));
        assert_eq!(live.player_info[&uuid].listed, Some(true));
        assert_eq!(live.player_info[&uuid].latency, Some(17));
        live.apply_event(ClientPlayEvent::Play(PlayEvent::PlayerInfoRemove {
            uuids: vec![uuid],
        }))
        .unwrap();
        assert!(live.player_info.is_empty());

        live.apply_event(ClientPlayEvent::Play(PlayEvent::SetTitle {
            fade_in: 5,
            stay: 30,
            fade_out: 10,
        }))
        .unwrap();
        assert_eq!(live.title_times, Some([5, 30, 10]));
        live.apply_event(ClientPlayEvent::Play(PlayEvent::SetActionBarText {
            nbt_bytes: vec![9, 9],
        }))
        .unwrap();
        assert_eq!(live.actionbar_nbt.as_deref(), Some(&[9, 9][..]));
    }

    #[test]
    fn play_events_drive_screens_equipment_rigs_and_transfers() {
        let (mut live, _tx) = session();
        live.apply_event(ClientPlayEvent::Play(PlayEvent::OpenScreen {
            window_id: 3,
            menu_type: 0,
            title_nbt: Vec::new(),
        }))
        .unwrap();
        // Two empty slots (`0x00` count) plus the carried slot at the tail.
        live.apply_event(ClientPlayEvent::Play(PlayEvent::ContainerSetContent {
            window_id: 3,
            state_id: 11,
            slot_count: 2,
            raw_payload: vec![0, 0, 0],
        }))
        .unwrap();
        let screen = live.open_screen.as_ref().unwrap();
        assert_eq!(screen.state_id, 11);
        assert_eq!(screen.slots.len(), 2);
        assert!(screen.carried.is_some());

        // Slot 1 gains a real stack (count 1, item id 5, no patches).
        live.apply_event(ClientPlayEvent::Play(PlayEvent::ContainerSetSlot {
            window_id: 3,
            state_id: 12,
            slot: 1,
            raw_item: vec![1, 5, 0, 0],
        }))
        .unwrap();
        assert_eq!(live.open_screen.as_ref().unwrap().slots[&1].item_id, 5);
        live.apply_event(ClientPlayEvent::Play(PlayEvent::ContainerClose {
            window_id: 3,
        }))
        .unwrap();
        assert!(live.open_screen.is_none());

        // Equipment: zombie rig (id 124) + one slot decoded from wire bytes.
        live.apply_event(ClientPlayEvent::Play(PlayEvent::AddEntity {
            eid: 9,
            uuid: [1; 16],
            entity_type: 124,
            x: 1.0,
            y: 64.0,
            z: 2.0,
            pitch: 0.0,
            yaw: 0.0,
            head_yaw: 0.0,
            data: 0,
            vx: 0.0,
            vy: 0.0,
            vz: 0.0,
        }))
        .unwrap();
        assert_eq!(live.entity_rigs[&9].name, "minecraft:zombie");
        live.apply_event(ClientPlayEvent::Play(PlayEvent::SetEquipment {
            eid: 9,
            entries: vec![EquipmentEntry {
                slot: 0,
                item_wire: vec![1, 5, 0, 0],
            }],
        }))
        .unwrap();
        assert_eq!(live.entity_equipment[&(9, 0)].item_id, 5);
        live.apply_event(ClientPlayEvent::Play(PlayEvent::RemoveEntities {
            eids: vec![9],
        }))
        .unwrap();
        assert!(live.entity_rigs.is_empty());
        assert!(live.entity_equipment.is_empty());

        live.apply_event(ClientPlayEvent::Play(PlayEvent::Transfer {
            host: "hub.example".into(),
            port: 25565,
        }))
        .unwrap();
        assert_eq!(
            live.pending_transfer.as_ref().map(|t| t.port),
            Some(25565)
        );
        live.apply_event(ClientPlayEvent::Play(PlayEvent::Transfer {
            host: String::new(),
            port: 0,
        }))
        .unwrap();
        assert!(matches!(live.status, SessionStatus::Disconnected(_)));
    }

    #[test]
    fn action_commands_encode_through_the_worker_channel() {
        let (tx, incoming) = mpsc::sync_channel(8);
        let (outgoing, rx) = mpsc::sync_channel(8);
        let _ = tx;
        let mut live = LiveSession::with_channels(incoming, outgoing);
        live.status = SessionStatus::Playing;
        live.pose = Some(PlayerPose {
            feet: [0.0; 3],
            yaw: 90.0,
            pitch: 10.0,
        });

        live.send_chat("hello world");
        live.use_item(0);
        live.drop_item(false);
        live.drop_item(true);
        live.swap_offhand();
        live.swing(1);
        live.container_close(3);

        let cmd = rx.recv().unwrap();
        assert!(matches!(cmd, WorkerCommand::Chat(ref m) if m == "hello world"));
        assert!(matches!(
            rx.recv().unwrap(),
            WorkerCommand::UseItem {
                hand: 0,
                sequence: 1,
                yaw,
                pitch,
            } if yaw == 90.0 && pitch == 10.0
        ));
        assert!(matches!(
            rx.recv().unwrap(),
            WorkerCommand::PlayerAction {
                status: 4,
                sequence: 2,
                ..
            }
        ));
        assert!(matches!(
            rx.recv().unwrap(),
            WorkerCommand::PlayerAction {
                status: 3,
                sequence: 3,
                ..
            }
        ));
        assert!(matches!(
            rx.recv().unwrap(),
            WorkerCommand::PlayerAction {
                status: 6,
                sequence: 4,
                ..
            }
        ));
        assert!(matches!(rx.recv().unwrap(), WorkerCommand::Swing { hand: 1 }));
        assert!(matches!(
            rx.recv().unwrap(),
            WorkerCommand::ContainerClose { window_id: 3 }
        ));
    }

    #[test]
    fn cem_models_bind_entities_by_pack_stem() {
        let (mut live, _tx) = session();
        live.world
            .animations
            .register_jem_model(
                "zombie",
                r#"{"models":[{"part":"body","translate":[0,-12,0],
                   "submodels":[{"id":"head","translate":[0,-6,0]}]}]}"#,
            )
            .unwrap();
        live.world
            .animations
            .register_jpm_model(
                "skeleton",
                r#"{"id":"body","translate":[0,-12,0]}"#,
            )
            .unwrap();
        live.apply_event(ClientPlayEvent::Play(PlayEvent::AddEntity {
            eid: 9,
            uuid: [1; 16],
            entity_type: 124, // minecraft:zombie
            x: 1.0,
            y: 64.0,
            z: 2.0,
            pitch: 0.0,
            yaw: 0.0,
            head_yaw: 0.0,
            data: 0,
            vx: 0.0,
            vy: 0.0,
            vz: 0.0,
        }))
        .unwrap();
        // Bound through `bind_entity` — the pose evaluates over the JEM rig.
        let pose = live.world.animations.evaluate_entity_pose(9).unwrap();
        assert_eq!(pose.unwrap().len(), 2);
        // Entity type 91 (skeleton) matches the JPM stem registration.
        live.apply_event(ClientPlayEvent::Play(PlayEvent::AddEntity {
            eid: 10,
            uuid: [2; 16],
            entity_type: 91,
            x: 0.0,
            y: 64.0,
            z: 0.0,
            pitch: 0.0,
            yaw: 0.0,
            head_yaw: 0.0,
            data: 0,
            vx: 0.0,
            vy: 0.0,
            vz: 0.0,
        }))
        .unwrap();
        assert!(live
            .world
            .animations
            .evaluate_entity_pose(10)
            .unwrap()
            .is_some());
        // An entity with no registered CEM model stays unbound.
        live.apply_event(ClientPlayEvent::Play(PlayEvent::AddEntity {
            eid: 11,
            uuid: [3; 16],
            entity_type: 1,
            x: 0.0,
            y: 64.0,
            z: 0.0,
            pitch: 0.0,
            yaw: 0.0,
            head_yaw: 0.0,
            data: 0,
            vx: 0.0,
            vy: 0.0,
            vz: 0.0,
        }))
        .unwrap();
        assert!(live
            .world
            .animations
            .evaluate_entity_pose(11)
            .unwrap()
            .is_none());
    }
}
