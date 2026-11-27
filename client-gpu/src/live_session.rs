// SPDX-License-Identifier: LGPL-3.0-or-later
//! Asynchronous live Protocol 767/777 session and renderer-facing world state.
//! Socket operations happen on a worker thread; `poll` never performs I/O.

use std::borrow::Cow;
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, SyncSender, TryRecvError, TrySendError};
use std::time::{Duration, Instant};

use megu_client::audio_out::{
    AudioCmd, AudioCmdRing, AudioRuntime, AudioSessionConfig, AudioSinkKind,
};
use megu_client::kinetic_sync::{
    CHANNEL_CREATE_KINETIC_SYNC, CHANNEL_CREATE_KINETIC_UPDATE, KineticSyncPayload,
    KineticSyncReassembler, KineticSyncRecord, KineticSyncStore, decode_kinetic_sync_payload,
};
use megu_client::mod_assets_meta::{
    CHANNEL_MOD_REGISTRY_SYNC, ClientModRegistry, ModMetaReport, ModTranslationTable,
    decode_mrs1_frame,
};
use megu_client::online_auth::OnlineAuthOptions;
use megu_client::protocol::{
    ClientCustomChannelEvent, ClientPlayEvent, CustomPayloadInbox, DecodedBossBarAction,
    DecodedMagicCastState, DecodedMagicSync, DecodedMotionTrigger, DimensionMeta, EntityMetaValue,
    ItemStackComponents, PlayEvent, PlayerInfoUpdateEntry, ProtocolClient, SeasonWireSync,
    TeleportSync, TransferTarget, WireReader, WireSoundEvent, WireStopSound, decode_boss_bar_action,
    decode_item_stack_components, handle_transfer_packet, offline_uuid, pack_position,
    parse_entity_metadata, parse_uuid, write_string,
};
use megu_client::render::ClientVfxHost;
use megu_client::render_graph::GpuPipelineKind;
use megu_client::session::{AudioStats, DeathLoopState, WorldAcousticSnapshot};
use megu_client::spatial_audio::{SpatialAudioPreset, SpatialSubtitleCue, SubtitleDirection};
use megu_client::world::{
    BlockAtlasSpriteLookup, BlockTable, WorldState, block_state_base_name, pack_vertices,
    sort_translucent,
};
use megu_client_mesh::asset_pipeline::{CompiledAssetBundle, ResourcePackLayer};
use megu_client_mesh::atlas::{AtlasMip, PackedAtlas, SpriteAnimation, SpritePlacement};
use megu_client_mesh::builtin_shadow_wgsl::{BuiltinShadowQuality, enchantment_glint_rgb};
use megu_client_mesh::dynamic_light_resolver::{
    BlockLightInput, LightModulation, ResolvedLightEmitter, evaluate_modulated_intensity,
    resolve_block_light_emitter, resolve_entity_light_emitter, resolve_item_light_emitter,
};
use megu_client_mesh::entity_rig::{EntityRigArchetype, EntityRigSpec, classify_entity_rig};
use megu_client_mesh::mesher::{
    BlockVisualEnhancementResult, BlockVisualInput, VanillaPlusPipelineConfig, torch_flame_kind,
    torch_flame_world_offset,
};
use megu_client_mesh::model_baker::blockstate::{self, BlockStateDef, BlockStateResolution};
use megu_client_mesh::model_baker::{ResourcePackModelStack, bake_model};
use megu_client_mesh::tactile_effects::{
    CameraMicroOffset, EnvironmentAudioHint, FluidEntryEvent, GroundSurfaceKind, ImpactMaterial,
    LeafCanopyKind, NightSkyDrama, TactileEffectsConfig, TactileEffectsSim,
    compute_environment_audio_hint, evaluate_night_sky_drama,
};
use megu_client_mesh::vanilla_block_models::{VanillaBlockModelCatalog, parse_block_state_name};
use megu_client_mesh::vanilla_plus_nature::{
    ActiveSeasonKind, CLOUD_CELL_BLOCKS, CLOUD_SCROLL_BLOCKS_PER_TICK, CloudLighting,
    NatureAmbienceEnv, NatureAmbienceState, NatureModRegistry, SeasonAmbienceInput,
    compute_cloud_lighting, generate_layered_voxel_clouds_with_atmosphere, tick_nature_ambience,
};
use megu_client_mesh::{
    BakedTerrainFace, BuiltSectionMesh, DIRECTION_BUCKETS, Direction, DirectionBuckets, SECTION_CELLS,
    SectionCoord, box_in_frustum, section_index,
};
use megu_kernel::Aabb;
use megu_kernel::CommandDispatcher;
use megu_kernel::player_physics::{
    PlayerInputState, PlayerPhysicsState, SurfaceKind, step_player_physics,
};
use megu_kernel::qol_gameplay::{
    HarvestTool, LookingAtBlock, LookingAtEntity, LookingAtHudInfo, LookingAtTarget,
    evaluate_looking_at_hud,
};
use megu_kernel::raycast::{
    EntityRayHit, FluidClipMode, RayBlockShapes, RayHit, raycast_blocks, raycast_entities,
};
use megu_proto::mod_wire as proto_mod_wire;
use megu_proto::seamless_portal::SEAMLESS_PORTAL_CHANNEL;
use megu_ui::block_inspector_hud::BlockInspectorTelemetry;
use megu_ui::death_screen::DeathScreenState;
use megu_ui::magic_hud::{CastBarState, CooldownSlotState, MagicHudState, StatusBadgeState};
use megu_ui::state::{
    CompactDebugHudData, CompactDebugHudPreset, HudCombat, HudMap, HudMapIcon, HudState, HudSubtitle, UiState,
};
use megu_ui::subtitle_hud::{DEFAULT_TTL_SECS, SubtitleArrow};

// Re-exported so embedders and tests keep the historical
// `live_session::{hw_adaptive_engine,sys_metrics}` paths while the crate
// compiles the canonical `megu-core` sources exactly once.
pub use megu_core::{hw_adaptive_engine, sys_metrics};

use megu_core::math_opt_engine::{
    DiscountedUcbAutotuner, P2QuantileEstimator, PidBudgetController,
};

use crate::SecretString;
use crate::atmosphere::{
    AtmosphereParams, BiomeAtmosphereBlender, CLOUD_RENDER_ALPHA, ChunkFadeTracker,
    voxel_clouds_to_geometry_flagged,
};
use crate::subworld_render::{
    InstancedPartBatch, InstancedPartRecord, MAT_FLAG_CLOUD, SubWorldRenderBank,
    SubWorldRenderEntry, SubWorldTransform, SubworldOwner, TerrainVertex, box_mesh,
    quat_from_axis_angle,
};
use crate::subworld_sync::{
    SUBWORLD_HELM_CHANNEL, SUBWORLD_SYNC_CHANNEL, SUBWORLD_SYNC_MAGIC, SubWorldHelmCmd,
    decode_sync_frame, encode_helm_cmd,
};
use hw_adaptive_engine::{
    ComputeKernelMode, ComputeStrategy, DomainKind, DomainSpecializationPlan, HostHardwareCaps,
    KernelAutotuner, PagedSectionPool, SpeculativeChunkPrefetcher, SpeculativeState,
    UnifiedOptimizationPlan, build_unified_optimization_plan, detect_host_hardware_caps,
    detect_universal_hardware_profile, select_compute_strategy,
};
use sys_metrics::{CpuSampler, FrameTimingStats, sample_host_metrics};

use crate::mesh_pool::{BakedSectionMeshResult, MeshJob, MeshJobPool, MeshResult, SectionMeshLane};
use crate::renderer::{
    ExternalSectionMesh, FirstPersonUseKind, FirstPersonViewmodel, GameplayScreenOverlay,
    GpuRenderer, external_packed_digest, translucent_order_digest,
};

/// Result of one local-player physics frame, including camera and locomotion animation inputs.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LocalPhysicsOutput {
    pub pose: PlayerPose,
    pub eye_height: f32,
    pub walk_bob: (f32, f32),
    pub on_ground: bool,
    pub submerged: bool,
}

#[derive(Clone, Copy, Debug, Default)]
struct MiningState {
    target: Option<([i32; 3], i8)>,
    progress: f32,
    duration: f32,
    cooldown: f32,
    swing_timer: f32,
    /// `STOP_DESTROY_BLOCK` has been sent; keep the same attempt alive until
    /// the authoritative block update, retarget, or explicit release.
    complete: bool,
    /// Block state id the attempt started against; any authoritative mutation
    /// closes that attempt even when the coordinates stay the same.
    state_id: Option<u32>,
}

#[derive(Clone, Copy, Debug, Default)]
struct ItemUseState {
    active: bool,
    on_block: bool,
    elapsed: f32,
    place_timer: f32,
    kind: FirstPersonUseKind,
}

/// Sections per poll the priority-0 lane may rebuild inline, nearest
/// first — bounded so digging/placing reflects this same frame while a
/// near-field burst still defers the rest to the normal lanes.
const PRIORITY_ZERO_INLINE_MAX: usize = 2;

/// Degenerate frustum the priority-0 lane passes to the ordered rebuild:
/// `box_in_frustum` tests `nx·p + ny·p + nz·p + d >= 0`, so a single
/// zero-normal plane with `d = 1` passes every section box and the
/// queue key degenerates to pure camera distance.
const ALL_INCLUSIVE_PLANES: [[f32; 4]; 1] = [[0.0, 0.0, 0.0, 1.0]];

/// `MeshJob::SectionMesh` submissions per poll. Bounded so a reconnect
/// or stream-in burst cannot flood the worker queue ahead of the inline
/// lane; unsent candidates retry next poll.
const OFFTHREAD_SUBMIT_MAX: usize = 8;

const PREFETCH_VIEW_DISTANCE: u32 = 8;
const PREFETCH_MAX_CHUNKS: usize = 16;

/// Horizontal displacement in one movement update that is treated as a
/// teleport/jump rather than a velocity sample for the forward cone.
const PREFETCH_TELEPORT_JUMP_BLOCKS: f64 = 32.0;
const PREFETCH_MIN_VIEW_DISTANCE: u32 = 2;
const PREFETCH_MIN_CHUNKS: usize = 4;

/// Smoothly scale the speculative cone with horizontal speed while never
/// exceeding the memory/DMA caps baked into `PREFETCH_VIEW_DISTANCE` and
/// `PREFETCH_MAX_CHUNKS`. `last_motion_delta` is blocks per client update;
/// at 60 Hz roughly 0.07 is walking, 0.1 sprinting, 0.5+ ice-boat/elytra,
/// and 1.0 is treated as full lookahead.
fn velocity_scaled_prefetch_budget(vel_xz: [f64; 2]) -> (u32, usize) {
    let speed = vel_xz[0].hypot(vel_xz[1]);
    if !speed.is_finite() || speed <= 0.0 {
        return (PREFETCH_MIN_VIEW_DISTANCE, PREFETCH_MIN_CHUNKS);
    }
    let scale = (speed / 1.0).clamp(0.0, 1.0).sqrt();
    let view_span = PREFETCH_VIEW_DISTANCE - PREFETCH_MIN_VIEW_DISTANCE;
    let chunk_span = PREFETCH_MAX_CHUNKS - PREFETCH_MIN_CHUNKS;
    let view_distance = PREFETCH_MIN_VIEW_DISTANCE + (view_span as f64 * scale).round() as u32;
    let max_chunks = PREFETCH_MIN_CHUNKS + (chunk_span as f64 * scale).round() as usize;
    (
        view_distance.min(PREFETCH_VIEW_DISTANCE),
        max_chunks.min(PREFETCH_MAX_CHUNKS),
    )
}

fn horizontal_teleport_jump(delta: [f64; 2]) -> bool {
    delta[0].is_finite()
        && delta[1].is_finite()
        && delta[0].hypot(delta[1]) > PREFETCH_TELEPORT_JUMP_BLOCKS
}

fn transform_position_jump(prev: SubWorldTransform, curr: SubWorldTransform) -> bool {
    prev.position
        .iter()
        .zip(curr.position.iter())
        .map(|(a, b)| f64::from(b - a))
        .map(|d| d * d)
        .sum::<f64>()
        .sqrt()
        > PREFETCH_TELEPORT_JUMP_BLOCKS
}

/// Camera-distance subtraction (in blocks) applied per prefetcher
/// priority rank when the off-thread dirty-section drain orders
/// candidates: the best forward-cone bet (rank 0) reads `4 * 16 = 64`
/// blocks closer, the weakest a quarter-block further per rank, so the
/// speculative heading can reorder the queue but can never starve a
/// frustum hit behind it. Bounded — the boost cannot exceed
/// `PREFETCH_MAX_CHUNKS * PREFETCH_BOOST_BLOCKS_PER_RANK`.
const PREFETCH_BOOST_BLOCKS_PER_RANK: f32 = 4.0;

/// Kernel challenger probe cadence: one poll in
/// [`KERNEL_PROBE_INTERVAL_POLLS`] dispatches under the alternate
/// [`ComputeKernelMode`], so [`KernelAutotuner::observe_sample`]
/// measures a real challenger timing instead of resampling the
/// incumbent's own clock forever — the 61-poll period is coprime with
/// the 60 Hz frame cadence and the 50 ms tick cadence so the probe
/// lands on a different pipeline phase each round.
const KERNEL_PROBE_INTERVAL_POLLS: u64 = 61;

/// Minimum accumulated frame time between `sample_host_metrics` calls while
/// the compact debug HUD is enabled. The `/proc` + DRM sysfs reads are throttled to
/// 2 Hz; frame-time statistics still update every frame and the latest
/// [`sys_metrics::HostMetricsSnapshot`] is reused between samples.
pub const COMPACT_DEBUG_HUD_SYS_SAMPLE_INTERVAL_MS: u64 = 500;

/// The [`UnifiedOptimizationPlan`] domain projections `client-gpu`
/// applies on startup and every settings/strategy push:
/// `chunk_mesh` (`DomainKind::Meshing` — `use_morton_layout` selects the
/// distance-ring + Z-order arena record ordering), `gpu_cull`
/// (`DomainKind::Lighting` — `prefer_single_pass_gpu` gates the
/// GPU-driven indirect path on TBDR/iGPU plans), and `shadow`
/// (`DomainKind::ShadowRendering` — shadow scheduling hints).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ClientDomainHints {
    /// `DomainKind::Meshing` projection — mesh/arena ordering hints.
    pub chunk_mesh: DomainSpecializationPlan,
    /// `DomainKind::Lighting` projection — visibility/occlusion dispatch.
    pub gpu_cull: DomainSpecializationPlan,
    /// `DomainKind::ShadowRendering` projection — shadow scheduling hints.
    pub shadow: DomainSpecializationPlan,
}

impl ClientDomainHints {
    /// Project the three client-gpu domains out of a unified plan.
    fn for_plan(plan: &UnifiedOptimizationPlan) -> Self {
        Self {
            chunk_mesh: plan.for_domain(DomainKind::Meshing),
            gpu_cull: plan.for_domain(DomainKind::Lighting),
            shadow: plan.for_domain(DomainKind::ShadowRendering),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SessionStatus {
    Connecting,
    Playing,
    Disconnected(String),
    /// Unrecoverable local world/session processing error, not a server kick.
    Failed(String),
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

/// The one eye/look pose a rendered frame draws and picks with: the
/// interpolated feet position, the interpolated pose eye height, and the
/// frame's look basis. The window computes it once after this frame's
/// physics/look update and installs it through
/// [`LiveSession::set_render_pose`], so the camera, both crosshair raycasts,
/// the mining target, and the block outline can never disagree about where
/// the eye is or what it is looking at.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RenderFramePose {
    /// Interpolated feet position (vanilla `Entity.getPosition(partialTick)`).
    pub feet: [f64; 3],
    /// Interpolated pose eye height: `1.62` standing, `1.27` sneaking,
    /// `0.4` swimming/elytra (`Player.getEyeHeight(Pose)`).
    pub eye_height: f32,
    /// Eye origin `feet + eye_height` — the camera and raycast origin.
    pub eye: [f64; 3],
    /// Vanilla body yaw in degrees (`0` south, `90` west).
    pub yaw: f32,
    /// Vanilla pitch in degrees (`-90` straight up).
    pub pitch: f32,
    /// Unit look vector derived from `yaw`/`pitch`.
    pub look: [f64; 3],
}

impl RenderFramePose {
    /// Build one frame pose, deriving the eye origin and look vector so
    /// every consumer reads the same numbers.
    pub fn new(feet: [f64; 3], eye_height: f32, yaw: f32, pitch: f32) -> Self {
        Self {
            feet,
            eye_height,
            eye: [feet[0], feet[1] + f64::from(eye_height), feet[2]],
            yaw,
            pitch,
            look: Self::look_vector(yaw, pitch),
        }
    }

    /// Vanilla `Entity.getViewVector(1.0)`: yaw `0` faces `+Z`, `90` west
    /// (`-X`), and positive pitch looks down.
    pub fn look_vector(yaw: f32, pitch: f32) -> [f64; 3] {
        let (sin_yaw, cos_yaw) = f64::from(yaw).to_radians().sin_cos();
        let (sin_pitch, cos_pitch) = f64::from(pitch).to_radians().sin_cos();
        [-sin_yaw * cos_pitch, -sin_pitch, cos_yaw * cos_pitch]
    }
}

#[derive(Clone, Copy, Debug)]
pub struct PlayerMovement {
    pub pose: PlayerPose,
    /// Window lateral input, positive to the right.
    pub strafe: f32,
    pub forward: f32,
    pub jump: bool,
    /// Sneak key state — rides `player_input`'s shift bit (which doubles
    /// as the dismount flag while riding) and `interact`'s
    /// secondary-action flag.
    pub sneak: bool,
}

/// One frame of local survival locomotion input for
/// [`LiveSession::step_live_survival_movement`]. `forward`/`strafe` use the
/// vanilla sign convention (positive forward and positive left).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct SurvivalMovementInput {
    pub forward: f32,
    pub strafe: f32,
    pub jump: bool,
    pub sneak: bool,
    pub sprint: bool,
    pub yaw: f32,
    pub pitch: f32,
}

/// HUD-facing component-preserving hotbar/offhand stack summary. The legacy
/// [`HudState`] hotbar strings stay in place for the current renderer while
/// this richer feed preserves the authoritative `Slot` component metadata.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LiveHotbarStack {
    pub item_name: String,
    pub count: i32,
    pub damage: Option<i32>,
    pub max_damage: Option<i32>,
    pub custom_name: Option<String>,
    pub lore: Vec<String>,
    pub enchanted: bool,
}

/// Typed boss-bar metadata mirrored from `boss_event` add/update actions.
#[derive(Clone, Debug, PartialEq)]
pub struct LiveBossBarView {
    pub title: String,
    pub health: f32,
    /// Protocol boss-bar color id (0 pink, 1 blue, 2 red, 3 green, 4 yellow,
    /// 5 purple, 6 white).
    pub color: i32,
    /// Division/overlay id (0 none, 1 six, 2 ten, 3 twelve, 4 twenty).
    pub divisions: i32,
    /// Vanilla flags byte: darken screen 0x01, fog 0x02, darken sky 0x04.
    pub flags: u8,
}

/// Component-preserving player status effect metadata for HUD consumers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LiveStatusEffectView {
    pub effect_id: i32,
    pub name: String,
    pub amplifier: i32,
    /// Remaining ticks; `-1` is the protocol's infinite sentinel.
    pub duration_ticks: i32,
    pub ambient: bool,
    pub show_particles: bool,
    pub show_icon: bool,
    pub beneficial: bool,
}

impl LiveStatusEffectView {
    /// Vanilla HUD icons appear for effects explicitly flagged with an
    /// icon, plus the legacy zero-flag form older servers still send.
    pub(crate) fn visible_in_hud(&self) -> bool {
        self.show_icon || (!self.ambient && !self.show_particles && !self.show_icon)
    }

    /// Convert the typed protocol view into the renderer's legacy HUD row.
    pub(crate) fn as_hud_state(&self) -> megu_ui::state::LiveStatusEffect {
        megu_ui::state::LiveStatusEffect {
            effect_id: self.effect_id,
            name: self.name.clone(),
            amplifier: self.amplifier.clamp(0, u8::MAX as i32) as u8,
            duration_ticks: if self.duration_ticks < 0 {
                u32::MAX
            } else {
                self.duration_ticks as u32
            },
            beneficial: self.beneficial,
        }
    }
}

// `Play` is the hot variant; boxing it would put an allocation on every frame.
#[allow(clippy::large_enum_variant)]
pub(crate) enum WorkerMessage {
    Connected,
    Play(ClientPlayEvent),
    Sound(WireSoundEvent),
    StopSound(WireStopSound),
    Closed(String),
}

/// Launcher-supplied filesystem locations: the parsed `--gameDir`,
/// `--assetsDir`, `--assetIndex`, `--version` and `--resourcePackDir`
/// values a `GpuCliConfig` carries into the session. `None` fields fall
/// back to the historical `MEGU_*` environment lookups.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SessionPaths {
    /// `--gameDir` — `.minecraft` root; client JARs, `resourcepacks/`,
    /// `saves/` and `options.txt` resolve under it.
    pub game_dir: Option<PathBuf>,
    /// `--assetsDir` — sound asset store root (`indexes/`, `objects/`).
    pub assets_dir: Option<PathBuf>,
    /// `--assetIndex` — index id (`17` for 1.21.1).
    pub asset_index: Option<String>,
    /// Launcher `--version` id — `<gameDir>/versions/<id>/<id>.jar`.
    pub version_id: Option<String>,
    /// `--resourcePackDir` — the launcher's pack folder; the env var
    /// `MEGU_RESOURCE_PACK_DIR` keeps its higher priority.
    pub resource_pack_dir: Option<PathBuf>,
    /// Per-server verified mod JARs; kept outside the global `mods/` folder.
    pub server_mods_dir: Option<PathBuf>,
}

/// `megu:mod_sync_v1` (`MSY1`) channel shared with the integrated server.
pub const CHANNEL_MOD_SYNC_V1: &str = proto_mod_wire::MOD_SYNC_CHANNEL;
const MSH1_MAGIC: &[u8; 4] = b"MSH1";

/// One validated entry in a server `MSY1` manifest.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LiveModSyncEntry {
    pub mod_id: String,
    pub file_name: String,
    pub download_url: String,
    pub sha256_hex: String,
    pub size_bytes: u64,
}

/// Structured client/server mod-profile mismatch. All vectors are stable in
/// wire order (`removed` sorted) so UI and tests never need prose parsing.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LiveModSyncDiff {
    pub unchanged: Vec<LiveModSyncEntry>,
    pub added: Vec<LiveModSyncEntry>,
    pub updated: Vec<LiveModSyncEntry>,
    pub removed: Vec<String>,
}

impl LiveModSyncDiff {
    pub fn matches(&self) -> bool {
        self.added.is_empty() && self.updated.is_empty() && self.removed.is_empty()
    }
}

/// Decode and validate the bounded `MSY1` manifest format through the
/// canonical wire codec, then apply this client's trusted-CDN download policy.
pub fn decode_msy1_manifest(bytes: &[u8]) -> Result<Vec<LiveModSyncEntry>, String> {
    let manifest = proto_mod_wire::decode_msy1_manifest(bytes)?;
    let mut entries = Vec::with_capacity(manifest.entries.len());
    for entry in manifest.entries {
        proto_mod_wire::validate_msy1_canonical_download_url(&entry.download_url)
            .map_err(|_| "unsafe MSY1 download URL".to_owned())?;
        entries.push(LiveModSyncEntry {
            mod_id: entry.mod_id,
            file_name: entry.file_name,
            download_url: entry.download_url,
            sha256_hex: entry.sha256_hex,
            size_bytes: entry.size_bytes,
        });
    }
    Ok(entries)
}

fn infer_mod_id_from_file_name(file_name: &str) -> String {
    file_name
        .trim_end_matches(".jar")
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '_' | '-') {
                c.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect()
}

/// Inventory the local `mods/` directory in the same content-addressed shape
/// as the wire manifest. Oversized/unreadable jars are skipped rather than
/// being allowed to masquerade as a match.
pub fn scan_local_mod_sync_inventory(game_dir: &std::path::Path) -> Vec<LiveModSyncEntry> {
    let mods_dir = game_dir.join("mods");
    let mut files = Vec::new();
    if let Ok(read_dir) = std::fs::read_dir(mods_dir) {
        for entry in read_dir.flatten() {
            let path = entry.path();
            if path.extension().is_some_and(|ext| ext == "jar") && path.is_file() {
                files.push(path);
            }
        }
    }
    files.sort();
    let mut out = Vec::new();
    for path in files.into_iter().take(proto_mod_wire::MAX_MSY1_MODS) {
        let Ok(meta) = std::fs::metadata(&path) else {
            continue;
        };
        if meta.len() == 0 || meta.len() > proto_mod_wire::MAX_MSY1_JAR_BYTES {
            continue;
        }
        let Ok(bytes) = std::fs::read(&path) else {
            continue;
        };
        let Some(file_name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        out.push(LiveModSyncEntry {
            mod_id: infer_mod_id_from_file_name(file_name),
            file_name: file_name.to_owned(),
            download_url: String::new(),
            sha256_hex: megu_core::crypto::sha256_hex(&bytes),
            size_bytes: bytes.len() as u64,
        });
    }
    out
}

/// Content-aware manifest diff: exact file hash matches are unchanged, same
/// mod-id/file-name with different bytes is an update, unmatched server rows
/// are additions and unmatched local filenames are removals.
pub fn compute_live_mod_sync_diff(
    local_entries: &[LiveModSyncEntry],
    server_entries: &[LiveModSyncEntry],
) -> LiveModSyncDiff {
    let mut diff = LiveModSyncDiff::default();
    for server in server_entries {
        if local_entries.iter().any(|local| {
            local.file_name == server.file_name && local.sha256_hex == server.sha256_hex
        }) {
            diff.unchanged.push(server.clone());
        } else if local_entries
            .iter()
            .any(|local| local.mod_id == server.mod_id || local.file_name == server.file_name)
        {
            diff.updated.push(server.clone());
        } else {
            diff.added.push(server.clone());
        }
    }
    let mut removed: Vec<String> = local_entries
        .iter()
        .filter(|local| {
            !server_entries
                .iter()
                .any(|server| server.file_name == local.file_name || server.mod_id == local.mod_id)
        })
        .map(|entry| entry.file_name.clone())
        .collect();
    removed.sort();
    removed.dedup();
    diff.removed = removed;
    diff
}

/// Canonical manifest root hash used by the 36-byte `MSH1` fast-path probe.
pub fn live_mod_sync_root_hash(entries: &[LiveModSyncEntry]) -> [u8; 32] {
    let mut rows: Vec<_> = entries
        .iter()
        .map(|entry| {
            (
                &entry.mod_id,
                &entry.file_name,
                entry.size_bytes,
                entry.sha256_hex.to_ascii_lowercase(),
            )
        })
        .collect();
    rows.sort();
    let mut hasher = megu_core::crypto::Sha256::new();
    hasher.update(&(rows.len() as u32).to_be_bytes());
    for (mod_id, file_name, size, digest) in rows {
        for field in [mod_id.as_bytes(), file_name.as_bytes()] {
            hasher.update(&(field.len() as u64).to_be_bytes());
            hasher.update(field);
        }
        hasher.update(&size.to_be_bytes());
        hasher.update(&(digest.len() as u64).to_be_bytes());
        hasher.update(digest.as_bytes());
    }
    hasher.finalize()
}

fn encode_msh1_probe(root: &[u8; 32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(36);
    out.extend_from_slice(MSH1_MAGIC);
    out.extend_from_slice(root);
    out
}

/// One advancement tree entry mirrored for the `L`-key overlay: display
/// text already extracted from the wire's anonymous-NBT components.
#[derive(Clone, Debug)]
pub struct LiveAdvancement {
    /// Resource-location key (`minecraft:adventure/root`).
    pub key: String,
    /// Parent key in the advancement tree.
    pub parent_id: Option<String>,
    /// Plain-text title (empty when the entry has no display block).
    pub title: String,
    /// Plain-text description.
    pub description: String,
    /// 0 = task, 1 = challenge, 2 = goal.
    pub frame_type: i32,
    /// `flags & 0x04` — hidden until unlocked.
    pub hidden: bool,
    /// Tab-grid coordinates from the display block.
    pub x: f32,
    /// Tab-grid coordinates from the display block.
    pub y: f32,
    /// Criteria required to complete (flattened OR-of-ANDs count).
    pub criteria_total: usize,
}

/// Per-advancement progress mirror keyed by advancement id.
#[derive(Clone, Debug, Default)]
pub struct LiveAdvancementProgress {
    /// `(criterion_id, achieved)` — `true` entries are done.
    pub criteria: Vec<(String, bool)>,
    /// Criteria with an achieved timestamp (done count).
    pub done: usize,
}

/// What the asset discovery pass resolved — every source tried, what it
/// yielded, and every skip, so a dropped source never reads as a clean
/// negative result.
#[derive(Clone, Debug, Default)]
pub struct AssetDiscovery {
    /// Client JAR actually read (`<gameDir>/versions/<id>/<id>.jar`).
    pub jar_path: Option<PathBuf>,
    /// Decoded block/item sprites contributed by the JAR.
    pub jar_textures: usize,
    /// Resource-pack layers contributed (mod JAR, dir or zip), in priority order.
    pub pack_sources: Vec<String>,
    /// Per-entry/per-source failures recorded instead of silently dropped.
    pub notes: Vec<String>,
    /// Total `assets/`-shaped ZIP entries inspected.
    pub zip_entries_seen: usize,
}

pub(crate) enum WorkerCommand {
    Movement {
        movement: PlayerMovement,
        on_ground: bool,
    },
    /// `chat`/`chat_command` — the worker splits on a leading `/` exactly
    /// like vanilla's chat screen.
    Chat(String),
    /// `command_suggestion` — play-phase tab-completion request; the
    /// server's `command_suggestions` reply echoes `transaction_id`.
    CommandSuggestion {
        transaction_id: i32,
        text: String,
    },
    /// `use_item` — right-click in air (no block target).
    UseItem {
        hand: i32,
        sequence: i32,
        yaw: f32,
        pitch: f32,
    },
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
    PlayerAction {
        status: i32,
        pos: [i32; 3],
        face: i8,
        sequence: i32,
    },
    /// `swing` — arm-swing animation.
    Swing {
        hand: i32,
    },
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
    ContainerButtonClick {
        container_id: i32,
        button_id: i32,
    },
    /// `container_close` — client-initiated window close.
    ContainerClose {
        window_id: u8,
    },
    RenameItem(String),
    SelectTrade {
        index: i32,
    },
    SetBeacon {
        primary: Option<i32>,
        secondary: Option<i32>,
    },
    ContainerSlotStateChanged {
        slot_id: i32,
        window_id: i32,
        state: bool,
    },
    /// `seen_advancements` — the advancements screen opened (`Some(tab)`)
    /// or closed (`None`), per vanilla's screen-notification packet.
    SeenAdvancements {
        tab: Option<String>,
    },
    /// `place_recipe` — the recipe-book/recipe browser `+` fill request: window id,
    /// recipe id, and the shift-click fill-all flag.
    PlaceRecipe {
        window_id: u8,
        recipe: String,
        make_all: bool,
    },
    /// `client_command` — the play-phase command packet the death
    /// screen's Respawn button sends: action `0` (`PERFORM_RESPAWN`).
    ClientCommand {
        action: i32,
    },
    /// `interact` — entity interaction: `action` 0 = interact, 1 =
    /// attack, 2 = interact-at (`at` = absolute crosshair hit position).
    Interact {
        target: i32,
        action: i32,
        at: Option<[f32; 3]>,
        hand: i32,
        sneaking: bool,
    },
    /// `set_carried_item` — held hotbar slot echo after a slot change.
    SetCarriedItem {
        slot: i32,
    },
    /// `player_command` — sneak/sprint/elytra/vehicle entity action for
    /// the local player's entity id (`jump_boost` for horse jumps).
    PlayerCommand {
        entity_id: i32,
        action: i32,
        jump_boost: i32,
    },
    /// `player_abilities` — creative-flight flag echo after a local
    /// double-jump toggle (bit 0x02 flying; the server's `may_fly` bit
    /// 0x04 is preserved verbatim from the last sync).
    PlayerAbilities {
        flags: u8,
    },
    ReleaseUseItem,
    MoveVehicle {
        x: f64,
        y: f64,
        z: f64,
        yaw: f32,
        pitch: f32,
    },
    PaddleBoat {
        left: bool,
        right: bool,
    },
    PickItem {
        slot: i32,
    },
    CreativeModeSlot {
        slot: i16,
        item_id: i32,
        count: i32,
    },
    EditBook {
        slot: i32,
        pages: Vec<String>,
        title: Option<String>,
    },
    /// `sign_update` — the sign-edit screen's Done/Escape packet: packed
    /// block position, front/back face flag, and the four text lines.
    UpdateSign {
        pos: (i32, i32, i32),
        is_front: bool,
        lines: [String; 4],
    },
    /// `resource_pack` — status answer for a `resource_pack_push`/`pop`:
    /// the pack UUID plus the vanilla action code (accepted, loaded,
    /// discarded, ...).
    ResourcePackResponse {
        uuid: [u8; 16],
        status: i32,
    },
    /// `client_information` — play-phase re-announce after the settings
    /// screen changed render distance while connected.
    ClientInformation {
        view_distance: i8,
    },
    /// `ping_request` — client-initiated play-phase latency probe whose
    /// server `pong_response` echo supplies real RTT telemetry.
    PingRequest(i64),
    /// Versioned Play C2S plugin/custom payload.
    SendCustomPayload {
        channel: String,
        payload: Vec<u8>,
    },
}

/// Live view of the server's open-container instruction: the decoded
/// `open_screen` fields plus slot contents decoded as they arrive.
#[derive(Clone, Debug)]
pub struct LiveOpenScreen {
    pub window_id: i32,
    /// Protocol `menu_type` id — resolves through `menu_schema` on the UI
    /// side (`client-ui::container_screen`), with a generic 9×N fallback for
    /// modded ids the vanilla table does not model.
    pub menu_type: i32,
    /// Total wire slot count from the server's `container_set_content`
    /// (`None` until content arrives). Sizes the generic fallback grid for a
    /// modded `menu_type`.
    pub content_slot_count: Option<i32>,
    /// Raw anonymous-NBT title (text component) exactly as sent.
    pub title_nbt: Vec<u8>,
    /// Decoded plain text of the authoritative custom container title.
    pub title_text: Option<String>,
    /// `container_set_content` `state_id` sequence, echoed back on clicks.
    pub state_id: i32,
    /// Decoded slot patches keyed by window slot index.
    pub slots: BTreeMap<i32, ItemStackComponents>,
    /// Carried (cursor) item, decoded when the server sets one.
    pub carried: Option<ItemStackComponents>,
    /// `container_set_data` properties for this window — furnace burn
    /// time/cook progress, brewing progress, enchant hints — keyed by
    /// the vanilla property index.
    pub properties: BTreeMap<i16, i16>,
}

/// `(display_nbt, render_type, number_format_kind)` carried by `set_objective`.
pub type ScoreboardObjectiveMeta = (Option<Vec<u8>>, i32, Option<i32>);

/// Client-side mirror of the server's `player_abilities` sync (vanilla
/// `Abilities`): the flags byte plus fly/walk speeds. `toggle_flight`
/// flips bit 0x02 only while the server granted bit 0x04, matching
/// vanilla's double-jump creative flight.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct LivePlayerAbilities {
    /// Raw vanilla flags byte: 0x01 invulnerable, 0x02 flying,
    /// 0x04 may-fly, 0x08 instabuild.
    pub flags: u8,
    /// `flying_speed` field of the last `player_abilities` packet
    /// (vanilla default 0.05).
    pub flying_speed: f32,
    /// `walking_speed` field of the last `player_abilities` packet
    /// (vanilla default 0.1).
    pub walking_speed: f32,
}

impl LivePlayerAbilities {
    /// Vanilla `Abilities::flying` (flag 0x02).
    pub fn flying(self) -> bool {
        self.flags & 0x02 != 0
    }

    /// Vanilla `Abilities::mayfly` (flag 0x04) — double-jump toggling is
    /// legal only while the server keeps this set.
    pub fn may_fly(self) -> bool {
        self.flags & 0x04 != 0
    }

    /// Vanilla `Abilities::instabuild` (flag 0x08) — creative mode.
    pub fn instabuild(self) -> bool {
        self.flags & 0x08 != 0
    }

    /// Vanilla `Abilities::invulnerable` (flag 0x01).
    pub fn invulnerable(self) -> bool {
        self.flags & 0x01 != 0
    }
}

/// Local player's live `hurt_animation` pose: vanilla
/// `LivingEntity.animateHurt` sets `hurtTime = hurtDuration = 10` ticks and
/// stores the attack direction, which the window's `GameRenderer.bobHurt`
/// camera tilt consumes.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LocalHurtAnimation {
    /// Attack direction, the play event's vanilla (server) yaw in degrees.
    pub yaw_deg: f32,
    /// Seconds since the hit landed.
    pub elapsed_secs: f32,
}

impl LocalHurtAnimation {
    /// Vanilla `hurtDuration` — 10 ticks at 20 Hz.
    pub const DURATION_SECS: f32 = 0.5;

    /// Vanilla `hurtTime / hurtDuration`, clamped to `0..=1` where `1.0` is
    /// the instant of the hit.
    pub fn progress(&self) -> f32 {
        if !self.elapsed_secs.is_finite() {
            return 0.0;
        }
        (1.0 - self.elapsed_secs / Self::DURATION_SECS).clamp(0.0, 1.0)
    }
}

/// Session-side scoreboard mirror: objectives, scores, and sidebar pinning
/// exactly as the vanilla client tracks them for the sidebar overlay.
#[derive(Clone, Debug, Default)]
pub struct LiveScoreboard {
    /// `set_objective` upserts keyed by objective name: `(display_nbt,
    /// render_type, number_format_kind)`.
    pub objectives: BTreeMap<String, ScoreboardObjectiveMeta>,
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
    /// Decoded plain-text title (actions 0 add / 3 update-name carry the
    /// NBT component).
    pub title: String,
    /// Fill fraction 0..=1 (actions 0 add / 2 update-health).
    pub health: f32,
    /// Protocol color id (add/update-style actions).
    pub color: i32,
    /// Division/overlay id (add/update-style actions).
    pub divisions: i32,
    /// Vanilla boss-bar flags byte (darken screen/fog/sky bits).
    pub flags: u8,
}

/// Per-entity scalar metadata surfaced from `set_entity_data` via
/// [`parse_entity_metadata`] — shared flags, air, custom name, pose,
/// living health, item-entity stacks and player absorption hearts.
#[derive(Clone, Debug, Default)]
pub struct LiveEntityMeta {
    /// Shared flags byte at index 0 (0x01 on-fire, 0x02 sneaking, 0x08
    /// sprinting, 0x40 glowing, 0x80 fall-flying).
    pub flags: Option<u8>,
    /// Index-1 air supply (vanilla default 300 = full).
    pub air: Option<i32>,
    /// Index-2 custom name — plain text of the optional component.
    pub custom_name: Option<String>,
    /// Index-3 custom-name-visible flag.
    pub name_visible: Option<bool>,
    /// Index-6 pose id.
    pub pose: Option<i32>,
    /// Index-9 living-entity health float when a float record lands
    /// there.
    pub living_health: Option<f32>,
    /// Index-14 player additional-hearts float — the HUD absorption row.
    pub absorption: Option<f32>,
    /// Index-8 item-entity stack decoded through
    /// [`decode_item_stack_components`].
    pub item_stack: Option<ItemStackComponents>,
}

/// One active mob-effect instance on a tracked entity
/// (`update_mob_effect` / `remove_mob_effect`).
#[derive(Clone, Copy, Debug)]
pub struct LiveMobEffect {
    pub amplifier: i32,
    /// Remaining ticks (`-1` = infinite until removed).
    pub duration: i32,
    /// Vanilla flag byte (ambient 0x01, particles 0x02, icon 0x04).
    pub flags: u8,
}

/// The vanilla crosshair pick (`GameRenderer.pick`): entity hits win
/// only when strictly nearer than the traced block.
#[derive(Clone, Copy, Debug)]
pub enum CrosshairPick {
    /// Entity within attack/interact reach (ray hit + distance).
    Entity(EntityRayHit),
    /// Block face under the crosshair (dig/use hit record).
    Block(RayHit),
}

/// Owned snapshot of the server-driven HUD channels handed to the window's
/// UI sync each frame (`record_frame_and_refresh_compact_debug_hud` refreshes the
/// `looking_at_*` picks first).
#[derive(Clone, Debug)]
pub struct LiveHudFeed {
    pub scoreboard: LiveScoreboard,
    pub title_revision: u64,
    pub title_times: Option<[i32; 3]>,
    pub title_text: Option<String>,
    pub subtitle_text: Option<String>,
    pub looking_at_block: Option<([i32; 3], String)>,
    /// Component-preserving hotbar/offhand metadata parallel to the legacy
    /// string-only `HudState` feed.
    pub hotbar_stacks: [Option<LiveHotbarStack>; 9],
    pub offhand_stack: Option<LiveHotbarStack>,
    pub bossbars: Vec<LiveBossBarView>,
    pub status_effects: Vec<LiveStatusEffectView>,
}

/// Block cell adjacent to `pos` on `face` (vanilla `Direction` ordinals
/// 0..=5: down, up, north, south, west, east — anything else leaves the
/// position unchanged).
fn adjacent_block_pos(pos: [i32; 3], face: i32) -> [i32; 3] {
    let (dx, dy, dz) = match face {
        0 => (0, -1, 0),
        1 => (0, 1, 0),
        2 => (0, 0, -1),
        3 => (0, 0, 1),
        4 => (-1, 0, 0),
        5 => (1, 0, 0),
        _ => (0, 0, 0),
    };
    [pos[0] + dx, pos[1] + dy, pos[2] + dz]
}

fn ground_surface_for_name(name: &str) -> GroundSurfaceKind {
    let name = block_state_base_name(name)
        .rsplit(':')
        .next()
        .unwrap_or(name);
    if name.contains("sand") || name.contains("terracotta") {
        GroundSurfaceKind::Sand
    } else if name.contains("snow") {
        GroundSurfaceKind::Snow
    } else if name.contains("gravel") {
        GroundSurfaceKind::Gravel
    } else if name.contains("mud") || name.contains("clay") {
        GroundSurfaceKind::MudOrShallowWater
    } else if name.contains("leaves") || name.contains("leaf_litter") {
        GroundSurfaceKind::FallenLeaves
    } else {
        GroundSurfaceKind::StoneOrWood
    }
}

fn status_effect_name(id: i32) -> &'static str {
    // Protocol 767 (Minecraft 1.21.1) 0-based `minecraft:mob_effect` registry IDs.
    match id {
        0 => "speed",
        1 => "slowness",
        2 => "haste",
        3 => "mining_fatigue",
        4 => "strength",
        5 => "instant_health",
        6 => "instant_damage",
        7 => "jump_boost",
        8 => "nausea",
        9 => "regeneration",
        10 => "resistance",
        11 => "fire_resistance",
        12 => "water_breathing",
        13 => "invisibility",
        14 => "blindness",
        15 => "night_vision",
        16 => "hunger",
        17 => "weakness",
        18 => "poison",
        19 => "wither",
        20 => "health_boost",
        21 => "absorption",
        22 => "saturation",
        23 => "glowing",
        24 => "levitation",
        25 => "luck",
        26 => "unluck",
        27 => "slow_falling",
        28 => "conduit_power",
        29 => "dolphins_grace",
        30 => "bad_omen",
        31 => "hero_of_the_village",
        32 => "darkness",
        33 => "trial_omen",
        34 => "raid_omen",
        35 => "wind_charged",
        36 => "weaving",
        37 => "oozing",
        38 => "infested",
        _ => "unknown",
    }
}

/// `cooldown` S2C state for one item id: `remaining` ticks left of
/// `total` at the 20 Hz client tick — `hud.item_cooldowns` mirrors the
/// fraction for the hotbar wipe.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ItemCooldown {
    pub remaining_ticks: i32,
    pub total_ticks: i32,
}

/// Stored `command_suggestions` reply: the C2S `transaction_id` plus the
/// buffer range (`start`/`length`, char indices) the server completed
/// over and the candidate texts (tooltips already decoded away —
/// `WireSuggestion::tooltip_nbt` stays packet-side for now).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ServerCommandSuggestions {
    pub transaction_id: i32,
    pub start: i32,
    pub length: i32,
    pub matches: Vec<String>,
}

/// Deletion key for one chat line: `player_chat`'s global message index
/// and 256-byte signature, either of which `delete_chat` can name.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ChatMessageKey {
    pub index: i32,
    pub signature: Option<[u8; 256]>,
}

/// One registered Create-style kinetic part (shaft / cog / belt) resolved
/// to a [`InstancedPartRecord`] each [`LiveSession::sync_kinetic_network_rpm`].
/// The rotation phase advances analytically — `theta = phase0 + omega * t`
/// with `omega = network_rpm * ratio * TAU/60` — so the shared template
/// mesh is never rebuilt on the CPU; only the 48-byte instance record
/// changes.
#[derive(Clone, Debug, PartialEq)]
pub struct KineticPartSpec {
    /// Shared mesh template id the part's [`InstancedPartBatch`] batches
    /// under (see [`LiveSession::register_kinetic_template`]).
    pub mesh_kind: u32,
    /// Instance origin in the batch's model space (blocks).
    pub position: [f32; 3],
    /// Unit rotation axis in model space (`x`/`y`/`z` cardinals for
    /// shafts; arbitrary axes decompose through the same math).
    pub axis: [f32; 3],
    /// Speed ratio against the shared network RPM — gear reductions and
    /// sign flips (counter-rotating cog pairs) live here.
    pub ratio: f32,
    /// Phase offset in radians at network time `t = 0`.
    pub phase0: f32,
    /// Uniform model-space scale.
    pub scale: f32,
    /// Packed block|sky light nibbles for the instance record.
    pub packed_light: u32,
    /// RGBA instance tint, one byte per channel (R in bits 0..8).
    pub tint_rgba: u32,
    /// Sub-world slot the instance draws into; `0` is the static world.
    pub subworld_id: u32,
}

impl KineticPartSpec {
    /// A unit-ratio shaft spinning about `axis` at `position`.
    pub fn shaft(mesh_kind: u32, position: [f32; 3], axis: [f32; 3]) -> Self {
        Self {
            mesh_kind,
            position,
            axis,
            ratio: 1.0,
            phase0: 0.0,
            scale: 1.0,
            packed_light: 0xf0,
            tint_rgba: 0xffff_ffff,
            subworld_id: 0,
        }
    }
}

/// Create kinetic block families that get their own procedural 3D
/// template instead of the generic rotor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum KineticMeshFamily {
    /// `create:shaft` — narrow 4×16×4 axle rod.
    Shaft,
    /// `create:cogwheel` — axle plus toothed gear rim and spokes.
    Cogwheel,
    /// `create:large_cogwheel` — wider 24-unit toothed ring and spokes.
    LargeCogwheel,
    /// `create:water_wheel` — hub plus eight radial wooden paddles.
    WaterWheel,
    /// `create:large_water_wheel` — same paddled build at a wider reach.
    LargeWaterWheel,
    /// `create:mechanical_press` — axle, piston shaft and head plate.
    MechanicalPress,
    /// Unknown Create addon kinetic blocks — clean shaft+hub rotor.
    GenericRotor,
}

impl KineticMeshFamily {
    /// Map a block name (`create:shaft`, optionally `[state]`-suffixed)
    /// to its template family; unknown names fall back to the rotor.
    fn from_block_name(name: &str) -> Self {
        let tail = name.rsplit(':').next().unwrap_or(name);
        let base = tail.split('[').next().unwrap_or(tail);
        match base {
            "shaft" => Self::Shaft,
            "cogwheel" => Self::Cogwheel,
            "large_cogwheel" => Self::LargeCogwheel,
            "water_wheel" => Self::WaterWheel,
            "large_water_wheel" => Self::LargeWaterWheel,
            "mechanical_press" => Self::MechanicalPress,
            _ => Self::GenericRotor,
        }
    }

    /// Stable family index folded into the template `mesh_kind`.
    fn index(self) -> u32 {
        match self {
            Self::Shaft => 0,
            Self::Cogwheel => 1,
            Self::LargeCogwheel => 2,
            Self::WaterWheel => 3,
            Self::LargeWaterWheel => 4,
            Self::MechanicalPress => 5,
            Self::GenericRotor => 6,
        }
    }
}

/// Dominant cardinal the part spins around — the axis the template is
/// reoriented onto after being authored along +Y.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum KineticAxis {
    X,
    Y,
    Z,
}

impl KineticAxis {
    fn dominant(axis: [f32; 3]) -> Self {
        if axis[0].abs() > axis[1].abs() && axis[0].abs() > axis[2].abs() {
            Self::X
        } else if axis[2].abs() > axis[1].abs() {
            Self::Z
        } else {
            Self::Y
        }
    }

    /// Axis slot folded into the `mesh_kind` (`family * 4 + slot`) so a
    /// shaft along X and the same shaft along Z keep separate templates.
    fn slot(self) -> u32 {
        match self {
            Self::X => 0,
            Self::Y => 1,
            Self::Z => 2,
        }
    }
}

/// One pixel of Create's 16px-per-block model space, in blocks.
const KPX: f32 = 1.0 / 16.0;
/// Andesite-alloy metal tone used by shafts, cogs and press parts.
const KINETIC_METAL: [f32; 3] = [0.55, 0.52, 0.48];
/// Darker steel tone for piston shafts and press heads.
const KINETIC_DARK_METAL: [f32; 3] = [0.38, 0.36, 0.40];
/// Spruce-plank wood tone for water-wheel paddles and hubs.
const KINETIC_WOOD: [f32; 3] = [0.56, 0.40, 0.22];

/// Append an axis-aligned box (pixel units, pivot-centered) rotated
/// `yaw` radians about +Y to a template under construction.
fn kinetic_push_box(
    vertices: &mut Vec<TerrainVertex>,
    indices: &mut Vec<u32>,
    min: [f32; 3],
    max: [f32; 3],
    yaw: f32,
    albedo: [f32; 3],
) {
    let (mut part_vertices, part_indices) = box_mesh(
        [min[0] * KPX, min[1] * KPX, min[2] * KPX],
        [max[0] * KPX, max[1] * KPX, max[2] * KPX],
        albedo,
    );
    if yaw != 0.0 {
        let (sin, cos) = yaw.sin_cos();
        for vertex in &mut part_vertices {
            let [x, y, z] = vertex.position;
            vertex.position = [x * cos + z * sin, y, z * cos - x * sin];
            let [nx, ny, nz] = vertex.normal();
            vertex.set_normal([nx * cos + nz * sin, ny, nz * cos - nx * sin]);
        }
    }
    let base = vertices.len() as u32;
    vertices.extend(part_vertices);
    indices.extend(part_indices.into_iter().map(|index| index + base));
}

/// Reorient a canonical (+Y-axis) template so its spin axis lands on
/// `axis`: +Y maps onto +X or +Z while the pivot stays at the origin.
fn kinetic_orient_to_axis(vertices: &mut [TerrainVertex], axis: KineticAxis) {
    let orient: fn([f32; 3]) -> [f32; 3] = match axis {
        KineticAxis::Y => return,
        // Rotation -90° about Z sends +Y to +X.
        KineticAxis::X => |[x, y, z]| [y, -x, z],
        // Rotation +90° about X sends +Y to +Z.
        KineticAxis::Z => |[x, y, z]| [x, -z, y],
    };
    for vertex in vertices {
        vertex.position = orient(vertex.position);
        vertex.set_normal(orient(vertex.normal()));
    }
}

/// Procedural 3D template for one Create kinetic family, authored
/// pivot-centered (block center = origin) along the +Y spin axis and
/// then reoriented onto `axis`. Sizes follow Create's 16px-per-block
/// model space: a `4×16×4` shaft is a ¼-block rod spanning the block.
fn kinetic_template(
    family: KineticMeshFamily,
    axis: KineticAxis,
) -> (Vec<TerrainVertex>, Vec<u32>) {
    let mut vertices = Vec::new();
    let mut indices = Vec::new();
    // The narrow 4×16×4 axle rod every family shares.
    kinetic_push_box(
        &mut vertices,
        &mut indices,
        [-2.0, -8.0, -2.0],
        [2.0, 8.0, 2.0],
        0.0,
        KINETIC_METAL,
    );
    match family {
        KineticMeshFamily::Shaft => {}
        KineticMeshFamily::Cogwheel | KineticMeshFamily::LargeCogwheel => {
            let (steps, rim, tooth) = if family == KineticMeshFamily::Cogwheel {
                (8_u32, 5.5_f32, 8.0_f32)
            } else {
                (12_u32, 9.5_f32, 12.0_f32)
            };
            for step in 0..steps {
                let yaw = step as f32 * std::f32::consts::TAU / steps as f32;
                // Rim segment hugging the wheel's circumference.
                kinetic_push_box(
                    &mut vertices,
                    &mut indices,
                    [rim - 1.5, -1.0, -2.0],
                    [rim + 1.5, 1.0, 2.0],
                    yaw,
                    KINETIC_METAL,
                );
                // Tooth protruding past the rim face.
                kinetic_push_box(
                    &mut vertices,
                    &mut indices,
                    [tooth - 1.5, -1.5, -1.0],
                    [tooth + 1.5, 1.5, 1.0],
                    yaw,
                    KINETIC_METAL,
                );
            }
            // Four spokes tying the axle to the rim.
            for step in 0..4 {
                let yaw = step as f32 * std::f32::consts::FRAC_PI_2;
                kinetic_push_box(
                    &mut vertices,
                    &mut indices,
                    [1.0, -0.75, -0.75],
                    [rim - 0.5, 0.75, 0.75],
                    yaw,
                    KINETIC_METAL,
                );
            }
        }
        KineticMeshFamily::WaterWheel | KineticMeshFamily::LargeWaterWheel => {
            let (reach, half_width) = if family == KineticMeshFamily::WaterWheel {
                (7.5_f32, 2.0_f32)
            } else {
                (13.5, 3.0)
            };
            // Squared hub the paddles bolt onto.
            kinetic_push_box(
                &mut vertices,
                &mut indices,
                [-3.0, -2.0, -3.0],
                [3.0, 2.0, 3.0],
                0.0,
                KINETIC_WOOD,
            );
            for step in 0..8 {
                let yaw = step as f32 * std::f32::consts::FRAC_PI_4;
                kinetic_push_box(
                    &mut vertices,
                    &mut indices,
                    [2.5, -0.75, -half_width],
                    [reach, 0.75, half_width],
                    yaw,
                    KINETIC_WOOD,
                );
            }
        }
        KineticMeshFamily::MechanicalPress => {
            // Vertical piston shaft below the spinning axle.
            kinetic_push_box(
                &mut vertices,
                &mut indices,
                [-1.5, -6.0, -1.5],
                [1.5, -2.0, 1.5],
                0.0,
                KINETIC_DARK_METAL,
            );
            // Press head plate at the block's bottom face.
            kinetic_push_box(
                &mut vertices,
                &mut indices,
                [-5.0, -8.0, -5.0],
                [5.0, -6.0, 5.0],
                0.0,
                KINETIC_DARK_METAL,
            );
        }
        KineticMeshFamily::GenericRotor => {
            // Squared hub plus a single marker blade so rotation reads.
            kinetic_push_box(
                &mut vertices,
                &mut indices,
                [-3.0, -2.0, -3.0],
                [3.0, 2.0, 3.0],
                0.0,
                KINETIC_METAL,
            );
            kinetic_push_box(
                &mut vertices,
                &mut indices,
                [2.0, -0.75, -0.5],
                [6.0, 0.75, 0.5],
                0.0,
                KINETIC_METAL,
            );
        }
    }
    kinetic_orient_to_axis(&mut vertices, axis);
    (vertices, indices)
}

/// Axis-angle `(axis, radians)` → XYZ-Euler triplet matching the instanced
/// vertex shader's "applied X then Y then Z" order (`R = Rz·Ry·Rx`).
fn axis_angle_to_euler_xyz(axis: [f32; 3], radians: f32) -> [f32; 3] {
    let [x, y, z, w] = quat_from_axis_angle(axis, radians);
    // Column-major rotation matrix from the quaternion (unit or zero).
    let m00 = 1.0 - 2.0 * (y * y + z * z);
    let m10 = 2.0 * (x * y + z * w);
    let m20 = 2.0 * (x * z - y * w);
    let m21 = 2.0 * (y * z + x * w);
    let m22 = 1.0 - 2.0 * (x * x + y * y);
    let beta = (-m20).clamp(-1.0, 1.0).asin();
    let (alpha, gamma) = if beta.cos().abs() > 1e-6 {
        (m21.atan2(m22), m10.atan2(m00))
    } else {
        // Gimbal lock: collapse roll into yaw.
        let m01 = 2.0 * (x * y - z * w);
        let m11 = 1.0 - 2.0 * (x * x + z * z);
        (0.0, (-m01).atan2(m11))
    };
    [alpha, beta, gamma]
}

/// Resident metadata half of a [`CompiledAssetBundle`]: sprite placements,
/// blockstate defs, the model overlay stack, animations and the skip log —
/// everything [`GpuRenderer::upload_block_atlas`](crate::GpuRenderer::upload_block_atlas)
/// does not need. The multi-MiB RGBA payloads live in
/// [`PendingAtlasPixels`] only until the GPU upload completes, so steady
/// state holds zero CPU atlas bytes and VRAM is the single source of truth.
#[derive(Clone, Debug, Default)]
pub struct CompiledAssetMeta {
    /// Atlas width in texels (matches the uploaded [`PackedAtlas`]).
    pub atlas_width: u32,
    /// Atlas height in texels.
    pub atlas_height: u32,
    /// Per-sprite padding border the atlas was built with.
    pub atlas_padding: u32,
    /// Sprite name -> pixel rect inside the atlas.
    pub placements: BTreeMap<String, SpritePlacement>,
    /// Merged `namespace:block` -> [`BlockStateDef`], highest-priority-wins.
    pub blockstates: BTreeMap<String, BlockStateDef>,
    /// Per-layer model documents pushed lowest-to-highest priority.
    pub model_stack: ResourcePackModelStack,
    /// Merged `namespace:path` -> [`SpriteAnimation`], highest-priority-wins.
    pub animations: BTreeMap<String, SpriteAnimation>,
    /// `(entry id, exact reason)` for every pack entry that failed to parse
    /// or pack — the load never panics on a single broken file.
    pub skipped_errors: Vec<(String, String)>,
}

/// CPU-side pixel payloads awaiting
/// [`GpuRenderer::upload_block_atlas`](crate::GpuRenderer::upload_block_atlas):
/// the atlas base image plus its linear-sRGB mip chain. Held only between
/// bundle build and upload — after the upload the GPU texture owns the
/// sole copy and this whole struct is dropped.
#[derive(Debug)]
pub struct PendingAtlasPixels {
    /// Packed block atlas (`rgba` is `width * height * 4` bytes).
    pub atlas: PackedAtlas,
    /// Gamma-correct linear-sRGB mip chain down to 1x1.
    pub mipmaps: Vec<AtlasMip>,
}

/// Splits a compiled bundle into the resident [`CompiledAssetMeta`] and the
/// [`PendingAtlasPixels`] CPU payloads that exist only until the GPU upload
/// drops them. Placements are kept in both halves because the upload path
/// resolves sprite rect indices through [`PackedAtlas::tile_rect`].
pub fn split_compiled_bundle(
    bundle: CompiledAssetBundle,
) -> (CompiledAssetMeta, PendingAtlasPixels) {
    let CompiledAssetBundle {
        atlas,
        mipmaps,
        blockstates,
        model_stack,
        animations,
        skipped_errors,
    } = bundle;
    let meta = CompiledAssetMeta {
        atlas_width: atlas.width,
        atlas_height: atlas.height,
        atlas_padding: atlas.padding,
        placements: atlas.placements.clone(),
        blockstates,
        model_stack,
        animations,
        skipped_errors,
    };
    (meta, PendingAtlasPixels { atlas, mipmaps })
}

/// Convert the session asset pipeline's normalized layer maps back into
/// vanilla asset paths and load the official blockstate/model catalog.
/// `ResourcePackLayer` keys are `namespace:path`; the catalog parser owns the
/// `assets/<ns>/{blockstates,models}/<path>.json` contract.
fn block_model_catalog_from_layers(layers: &[ResourcePackLayer]) -> VanillaBlockModelCatalog {
    let mut converted = Vec::with_capacity(layers.len());
    for layer in layers {
        let mut entries = Vec::with_capacity(layer.blockstates.len() + layer.models.len());
        for (id, text) in &layer.blockstates {
            if let Some((namespace, path)) = id.split_once(':') {
                entries.push((
                    format!("assets/{namespace}/blockstates/{path}.json"),
                    text.clone(),
                ));
            }
        }
        for (id, text) in &layer.models {
            if let Some((namespace, path)) = id.split_once(':') {
                entries.push((
                    format!("assets/{namespace}/models/{path}.json"),
                    text.clone(),
                ));
            }
        }
        converted.push(entries);
    }
    VanillaBlockModelCatalog::new(converted)
}

fn sprite_lookup_closure(lookup: Arc<BTreeMap<String, u32>>) -> BlockAtlasSpriteLookup {
    Arc::new(move |name: &str| {
        lookup.get(name).copied().or_else(|| {
            name.split_once(':')
                .and_then(|(_, path)| lookup.get(path).copied())
        })
    })
}

/// Namespaced block name of a block-state id — the kernel registry when
/// loaded, else the `BlockTable`'s per-state names (empty in builtin
/// fallback mode, so no names resolve there). Dynamic mod block-state
/// ids at or past the vanilla table resolve through the synced `MRS1`
/// `ClientModRegistry` when one is present.
fn state_block_name<'a>(
    world: &'a WorldState,
    mod_registry: Option<&'a ClientModRegistry>,
    state: u32,
) -> Option<&'a str> {
    if let Some(registry) = &world.registry {
        if let Ok(name) = registry.state_name(state) {
            return Some(name);
        }
    } else if let Some(name) = world.blocks.state_name(state) {
        return Some(name);
    }
    mod_registry.and_then(|registry| registry.block_state_name(state))
}

// Vanilla `game_event` ids beyond the weather quad `world.rs` already
// mirrors (1/2/7/8). The session layer owns the mode flip, the win and
// demo events, the respawn-screen flag, and the player-source stingers.
const GAME_EVENT_NO_RESPAWN_BLOCK: u8 = 0;
const GAME_EVENT_CHANGE_GAME_MODE: u8 = 3;
const GAME_EVENT_WIN_GAME: u8 = 4;
const GAME_EVENT_DEMO: u8 = 5;
const GAME_EVENT_ARROW_HIT_PLAYER: u8 = 6;
const GAME_EVENT_PUFFERFISH_STING: u8 = 9;
const GAME_EVENT_ELDER_GUARDIAN_APPEARANCE: u8 = 10;
const GAME_EVENT_ENABLE_RESPAWN_SCREEN: u8 = 11;

/// Chat hint text for the `game_event` id-5 demo event values (vanilla
/// `DemoMode` steps; the intro value `0` opens a screen this client has
/// no surface for, so it gets a line instead).
fn demo_hint_line(value: i32) -> Option<&'static str> {
    Some(match value {
        0 => "Demo mode: this world is a trial",
        101 => "Demo: use W, A, S and D to move around",
        102 => "Demo: move the mouse to look around",
        103 => "Demo: press Space to jump",
        104 => "Demo: press E to open the inventory",
        _ => return None,
    })
}

/// `level_event` id → `(sound name, source category, volume, pitch)` —
/// the sound half of vanilla `LevelEventHandler`. Category follows the
/// vanilla source table (4 blocks, 5 hostile, 6 neutral, 7 players);
/// the block-break id `2001` is handled separately by the caller since
/// its `data` field carries a block-state id.
fn level_event_sound(event: i32, _data: i32) -> Option<(&'static str, i32, f32, f32)> {
    Some(match event {
        1000 => ("minecraft:block.dispenser.dispense", 4, 1.0, 1.0),
        1001 => ("minecraft:block.dispenser.fail", 4, 1.0, 1.0),
        1002 => ("minecraft:block.dispenser.launch", 4, 1.0, 1.0),
        1003 => ("minecraft:entity.endereye.launch", 6, 1.0, 1.0),
        1004 => ("minecraft:entity.firework_rocket.launch", 8, 1.0, 1.0),
        1005 => ("minecraft:block.iron_door.open", 4, 1.0, 1.0),
        1006 => ("minecraft:block.wooden_door.open", 4, 1.0, 1.0),
        1007 => ("minecraft:block.wooden_trapdoor.open", 4, 1.0, 1.0),
        1008 => ("minecraft:block.fence_gate.open", 4, 1.0, 1.0),
        1009 => ("minecraft:block.fire.extinguish", 4, 1.0, 1.0),
        1011 => ("minecraft:block.iron_door.close", 4, 1.0, 1.0),
        1012 => ("minecraft:block.wooden_door.close", 4, 1.0, 1.0),
        1013 => ("minecraft:block.wooden_trapdoor.close", 4, 1.0, 1.0),
        1014 => ("minecraft:block.fence_gate.close", 4, 1.0, 1.0),
        1015 => ("minecraft:entity.ghast.warn", 5, 1.0, 1.0),
        1016 => ("minecraft:entity.ghast.shoot", 5, 1.0, 1.0),
        1017 => ("minecraft:entity.ender_dragon.shoot", 5, 1.0, 1.0),
        1018 => ("minecraft:entity.blaze.shoot", 5, 1.0, 1.0),
        1019 => ("minecraft:entity.zombie.attack_wooden_door", 5, 1.0, 1.0),
        1020 => ("minecraft:entity.zombie.attack_iron_door", 5, 1.0, 1.0),
        1021 => ("minecraft:entity.zombie.break_wooden_door", 5, 1.0, 1.0),
        1022 => ("minecraft:entity.wither.break_block", 5, 1.0, 1.0),
        1023 => ("minecraft:entity.wither.shoot", 5, 1.0, 1.0),
        1024 => ("minecraft:entity.bat.takeoff", 6, 1.0, 1.0),
        1025 => ("minecraft:entity.zombie.infect", 5, 1.0, 1.0),
        1026 => ("minecraft:entity.zombie_villager.converted", 6, 1.0, 1.0),
        1027 => ("minecraft:entity.ender_dragon.death", 5, 1.0, 1.0),
        1028 => ("minecraft:block.anvil.destroy", 4, 1.0, 1.0),
        1029 => ("minecraft:block.anvil.use", 4, 1.0, 1.0),
        1030 => ("minecraft:block.anvil.land", 4, 1.0, 1.0),
        1031 => ("minecraft:block.portal.travel", 7, 1.0, 1.0),
        1032 => ("minecraft:block.chorus_flower.grow", 4, 1.0, 1.0),
        1033 => ("minecraft:block.chorus_flower.death", 4, 1.0, 1.0),
        1034 => ("minecraft:block.brewing_stand.brew", 4, 1.0, 1.0),
        1035 => ("minecraft:block.iron_trapdoor.open", 4, 1.0, 1.0),
        1036 => ("minecraft:block.iron_trapdoor.close", 4, 1.0, 1.0),
        1039 => ("minecraft:entity.phantom.bite", 5, 1.0, 1.0),
        1041 => ("minecraft:entity.zombie.converted_to_drowned", 5, 1.0, 1.0),
        1042 => ("minecraft:block.grindstone.use", 4, 1.0, 1.0),
        1043 => ("minecraft:item.book.page_turn", 6, 1.0, 1.0),
        1044 => ("minecraft:block.smithing_table.use", 4, 1.0, 1.0),
        1047 => ("minecraft:item.bone_meal.use", 4, 1.0, 1.0),
        1501 => ("minecraft:block.lava.extinguish", 4, 1.0, 1.0),
        1502 => ("minecraft:block.redstone_torch.burnout", 4, 1.0, 1.0),
        1503 => ("minecraft:block.end_portal_frame.fill", 4, 1.0, 1.0),
        2002 => ("minecraft:entity.splash_potion.break", 6, 1.0, 1.0),
        2003 => ("minecraft:entity.endereye.death", 6, 1.0, 1.0),
        2008 => ("minecraft:entity.dragon_fireball.explode", 5, 1.0, 1.0),
        _ => return None,
    })
}

/// `block_event` → block-family sound resolved against the *live*
/// block name at the event position (the wire's raw `block_type` id is
/// registry-dependent, so the placed block wins). Vanilla actions:
/// piston extend/retract (0/1), note block play (0), bell ring (1),
/// container open/close (action 1, `parameter` = viewer count).
fn block_event_sound(name: &str, action: u8, parameter: u8) -> Option<&'static str> {
    let short = block_state_base_name(name)
        .rsplit(':')
        .next()
        .unwrap_or(name);
    let open = parameter > 0;
    match (short, action) {
        ("piston" | "sticky_piston", 0) => Some("minecraft:block.piston.extend"),
        ("piston" | "sticky_piston", 1) => Some("minecraft:block.piston.contract"),
        ("note_block", 0) => Some("minecraft:block.note_block.harp"),
        ("bell", 1) => Some("minecraft:block.bell.use"),
        ("barrel", 1) => Some(if open {
            "minecraft:block.barrel.open"
        } else {
            "minecraft:block.barrel.close"
        }),
        ("ender_chest", 1) => Some(if open {
            "minecraft:block.ender_chest.open"
        } else {
            "minecraft:block.ender_chest.close"
        }),
        ("chest" | "trapped_chest", 1) => Some(if open {
            "minecraft:block.chest.open"
        } else {
            "minecraft:block.chest.close"
        }),
        (short, 1) if short.ends_with("shulker_box") => Some(if open {
            "minecraft:block.shulker_box.open"
        } else {
            "minecraft:block.shulker_box.close"
        }),
        _ => None,
    }
}

/// Best-effort block-family `break` sound for `level_event` 2001 — the
/// packet carries only a block-state id, so the family is inferred from
/// the registry name like the data-driven `SoundType` table does.
fn break_sound_for_block(name: &str) -> &'static str {
    let short = block_state_base_name(name)
        .rsplit(':')
        .next()
        .unwrap_or(name);
    if short.contains("glass")
        || short.contains("amethyst")
        || short == "ice"
        || short.ends_with("_ice")
    {
        "minecraft:block.glass.break"
    } else if short.contains("leaves") || short.contains("vine") || short.contains("sapling") {
        "minecraft:block.grass.break"
    } else if short.contains("wool") || short.contains("carpet") {
        "minecraft:block.wool.break"
    } else if short.contains("gravel") {
        "minecraft:block.gravel.break"
    } else if short.contains("snow") {
        "minecraft:block.snow.break"
    } else if short.contains("slime") || short.contains("honeycomb") {
        "minecraft:block.slime_block.break"
    } else if short.contains("sand") && !short.contains("sandstone") {
        "minecraft:block.sand.break"
    } else if short.contains("log")
        || short.contains("stem")
        || short.contains("planks")
        || short.contains("wood")
        || short.contains("fence")
        || short.contains("door")
        || short.contains("trapdoor")
        || short.contains("chest")
        || short.contains("barrel")
        || short.contains("sign")
        || short.contains("ladder")
        || short.contains("torch")
    {
        "minecraft:block.wood.break"
    } else if short.contains("dirt")
        || short.contains("grass_block")
        || short.contains("farmland")
        || short.contains("mud")
        || short.contains("soil")
    {
        "minecraft:block.gravel.break"
    } else {
        "minecraft:block.stone.break"
    }
}

type FaceSprites = [Option<String>; 6];

/// Packed cubes use the first model layer facing each direction. The
/// baked-geometry path draws additional layers such as the grass fringe.
fn block_face_sprites(
    meta: &CompiledAssetMeta,
    state_name: &str,
    has_sprite: impl Fn(&str) -> bool,
) -> Option<FaceSprites> {
    let (namespace, block, properties) = parse_block_state_name(state_name).ok()?;
    let def = meta.blockstates.get(&format!("{namespace}:{block}"))?;
    let mut props = blockstate::representative_props(def);
    // Prefer the default upright and snow-free models for registries that
    // only supply block names. Real properties always take precedence.
    props.extend(properties);
    let preferred = if namespace == "minecraft" && state_name == block_state_base_name(state_name) {
        let mut preferred = props.clone();
        for (key, value) in [("axis", "y"), ("snowy", "false")] {
            if let Some(prop) = preferred.get_mut(key) {
                *prop = value.into();
            }
        }
        blockstate::resolve_blockstate(def, &preferred, [0, 0, 0]).ok()
    } else {
        None
    };
    let resolution =
        preferred.or_else(|| blockstate::resolve_blockstate(def, &props, [0, 0, 0]).ok())?;
    let specs = match resolution {
        BlockStateResolution::Variant(variant) => vec![variant],
        BlockStateResolution::Multipart(variants) => variants,
    };
    let mut faces = FaceSprites::default();
    let mut texture_maps = Vec::new();
    for spec in &specs {
        if let Ok(model) = bake_model(&meta.model_stack, &spec.model, spec.x_deg, spec.y_deg) {
            for quad in model.quads {
                let direction = quad.cullface.or_else(|| {
                    let normals = [
                        [0.0, -1.0, 0.0],
                        [0.0, 1.0, 0.0],
                        [0.0, 0.0, -1.0],
                        [0.0, 0.0, 1.0],
                        [-1.0, 0.0, 0.0],
                        [1.0, 0.0, 0.0],
                    ];
                    normals
                        .iter()
                        .position(|normal| *normal == quad.normal)
                        .map(|index| Direction::ALL[index])
                });
                if let Some(direction) = direction {
                    let face = &mut faces[direction as usize];
                    if face.is_none() && has_sprite(&quad.texture) {
                        *face = Some(quad.texture);
                    }
                }
            }
        }
        if faces.iter().all(Option::is_some) {
            return Some(faces);
        }
        if let Ok(textures) = meta.model_stack.resolved_textures(&spec.model) {
            texture_maps.push(textures);
        }
    }
    let face_keys: [&[&str]; 6] = [
        &["down", "bottom", "end", "side"],
        &["up", "top", "end", "side"],
        &["north", "front", "side"],
        &["south", "back", "side"],
        &["west", "side"],
        &["east", "side"],
    ];
    for (face, keys) in faces.iter_mut().zip(face_keys) {
        if face.is_some() {
            continue;
        }
        let texture = keys
            .iter()
            .copied()
            .chain(["all", "texture", "cross"])
            .find_map(|key| {
                texture_maps
                    .iter()
                    .filter_map(|textures| textures.get(key))
                    .find(|texture| has_sprite(texture))
            })
            .or_else(|| {
                texture_maps
                    .iter()
                    .flat_map(|textures| textures.iter())
                    .find(|(key, texture)| key.as_str() != "particle" && has_sprite(texture))
                    .map(|(_, texture)| texture)
            })
            .or_else(|| {
                texture_maps
                    .iter()
                    .filter_map(|textures| textures.get("particle"))
                    .find(|texture| has_sprite(texture))
            });
        *face = texture.cloned();
    }
    faces.iter().any(Option::is_some).then_some(faces)
}

fn state_sprite_assignments(
    world: &WorldState,
    mod_registry: Option<&ClientModRegistry>,
    meta: &CompiledAssetMeta,
    renderer: &GpuRenderer,
) -> Vec<(u32, FaceSprites, bool)> {
    let mut per_state_name: HashMap<&str, Option<FaceSprites>> = HashMap::new();
    let mut uses_properties = HashMap::new();
    let named_states = world.blocks.state_names().len();
    let dense_count = world.registry.as_ref().map_or(named_states, |registry| {
        named_states.max(registry.state_count())
    });
    let vanilla = (0..dense_count as u32).filter_map(|state| {
        world
            .blocks
            .state_name(state)
            .or_else(|| world.registry.as_ref()?.state_name(state).ok())
            .map(|name| (state, name))
    });
    let dynamic = mod_registry.into_iter().flat_map(|registry| {
        registry
            .block_states()
            .map(|(state, entry)| (state, entry.name.as_str()))
    });
    vanilla
        .chain(dynamic)
        .filter_map(|(state, name)| {
            let base = block_state_base_name(name);
            let grass = matches!(base, "grass_block" | "minecraft:grass_block");
            // Properties unused by the model (such as leaf distance) share
            // the baked result. Blocks with state-dependent models retain theirs.
            let uses_properties = *uses_properties.entry(base).or_insert_with(|| {
                meta.blockstates
                    .get(base)
                    .or_else(|| meta.blockstates.get(&format!("minecraft:{base}")))
                    .is_some_and(|def| !blockstate::representative_props(def).is_empty())
            });
            let model_name = if uses_properties { name } else { base };
            let faces = per_state_name.entry(model_name).or_insert_with(|| {
                block_face_sprites(meta, model_name, |texture| {
                    renderer.atlas_sprite_index(texture).is_some()
                })
            });
            if grass || faces.is_some() {
                Some((state, faces.clone().unwrap_or_default(), grass))
            } else {
                None
            }
        })
        .collect()
}

/// Per-coord `external_sections` changes pending renderer delivery —
/// the delta queue that lets `window.rs` mirror the map without
/// cloning it every frame. Apply order is `cleared` first (a wholesale
/// reset, e.g. dimension change), then `removals`, then `upserts`
/// re-read from the live map so the mirror always lands the freshest
/// payload regardless of how many times a coord churned between drains.
#[derive(Default)]
pub struct ExternalSectionsDelta {
    /// The map was cleared since the last drain — wipe the mirror first.
    pub cleared: bool,
    /// Coords inserted or replaced since the last drain.
    pub upserts: BTreeSet<SectionCoord>,
    /// Coords removed since the last drain.
    pub removals: BTreeSet<SectionCoord>,
}

impl ExternalSectionsDelta {
    /// `true` when the drain carries no change — the renderer skips it.
    pub fn is_empty(&self) -> bool {
        !self.cleared && self.upserts.is_empty() && self.removals.is_empty()
    }
}

// ---------------------------------------------------------------------------
// Gameplay dynamic lights (held items, glowing mobs, dropped glow items)
// ---------------------------------------------------------------------------

/// Point-light capacity of the WGSL `DynamicLights` UBO — the hard
/// ceiling every frame's gameplay light list is truncated to before it
/// merges with the server VFX1 lights on the same binding.
pub const MAX_DYNAMIC_LIGHTS: usize = 64;

/// One resolved dynamic-light material: linear color temperature, reach
/// and peak intensity, plus the depth of the organic sinusoidal flicker
/// applied per frame (`0.0` = steady emitters — lanterns, crystals).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DynamicLightSpec {
    /// Linear-space color temperature of the emitter.
    pub rgb: [f32; 3],
    /// Falloff reach in blocks (the UBO's `position_radius.w`).
    pub radius: f32,
    /// Peak brightness multiplier (the UBO's `color_intensity.w`).
    pub intensity: f32,
    /// `±` fraction of `intensity` the per-tick sine flicker sweeps.
    pub flicker: f32,
}

/// Map a resolved universal emitter back to the compact
/// [`DynamicLightSpec`] material summary [`item_dynamic_light_spec`]
/// returns: the `flicker` field carries the modulation depth
/// (`OpenFlame`/`SignalPulse` amplitude, `0.0` for steady emitters).
fn spec_from_emitter(emitter: &ResolvedLightEmitter) -> DynamicLightSpec {
    DynamicLightSpec {
        rgb: emitter.rgb,
        radius: emitter.radius,
        intensity: emitter.intensity,
        flicker: match emitter.modulation {
            LightModulation::Steady => 0.0,
            LightModulation::OpenFlame { flicker_amplitude } => flicker_amplitude,
            LightModulation::SignalPulse { amplitude, .. } => amplitude,
        },
    }
}

/// Vanilla item-id path -> declared light level for held/dropped stacks
/// whose luminance cannot come from a placed-block registry row (the
/// bucket, rods and item-only emitters) or is needed before any block
/// registry is available. Levels are the vanilla 1.21.1 block
/// luminances — the resolver still derives every color, radius,
/// modulation and quench decision from the shared material vocabulary,
/// so this table is data, not per-item shader logic.
const VANILLA_ITEM_LIGHT_LEVELS: &[(&str, u8)] = &[
    ("torch", 14),
    ("wall_torch", 14),
    ("soul_torch", 10),
    ("soul_wall_torch", 10),
    ("redstone_torch", 7),
    ("redstone_wall_torch", 7),
    ("lantern", 15),
    ("soul_lantern", 10),
    ("campfire", 15),
    ("soul_campfire", 10),
    ("jack_o_lantern", 15),
    ("sea_lantern", 15),
    ("glowstone", 15),
    ("shroomlight", 15),
    ("ochre_froglight", 15),
    ("pearlescent_froglight", 15),
    ("verdant_froglight", 15),
    ("end_rod", 14),
    ("beacon", 15),
    ("conduit", 15),
    ("amethyst_cluster", 5),
    ("large_amethyst_bud", 4),
    ("medium_amethyst_bud", 2),
    ("glow_berries", 14),
    ("magma_block", 3),
    ("lava_bucket", 15),
    ("blaze_rod", 12),
    ("blaze_powder", 8),
    ("glow_ink_sac", 10),
    ("redstone_block", 7),
];

/// Registry-declared luminance of a namespaced item id, or `None` when
/// the id carries no `minecraft:`/mod-namespace light entry.
fn vanilla_item_light_level(item_name: &str) -> Option<u8> {
    let path = item_name.rsplit(':').next().unwrap_or(item_name);
    VANILLA_ITEM_LIGHT_LEVELS
        .iter()
        .find(|(name, _)| *name == path)
        .map(|(_, level)| *level)
}

/// Vanilla entity-type path -> declared light level for luminous mobs
/// and projectiles the shared resolver's vocabulary does not already
/// classify (blazes, magma cubes, glow squids and allays resolve
/// through generic `blaze`/`magma`/`glow`/`allay` tokens on their own).
const VANILLA_ENTITY_LIGHT_LEVELS: &[(&str, u8)] = &[
    ("blaze", 14),
    ("magma_cube", 9),
    ("fireball", 15),
    ("dragon_fireball", 15),
    ("small_fireball", 10),
    ("end_crystal", 11),
    ("spectral_arrow", 9),
    ("glow_squid", 10),
];

fn vanilla_entity_light_level(rig_name: &str) -> Option<u8> {
    let path = rig_name.rsplit(':').next().unwrap_or(rig_name);
    VANILLA_ENTITY_LIGHT_LEVELS
        .iter()
        .find(|(name, _)| *name == path)
        .map(|(_, level)| *level)
}

/// Universal held/dropped item -> dynamic point light: every vanilla,
/// redstone and modded item resolves through
/// [`resolve_item_light_emitter`]'s data-driven spectrum/material
/// inference (zero per-mod tables). `item_name` accepts
/// `minecraft:torch` and bare `torch` alike; `submerged` quenches open
/// flames (torches and campfires drown — enclosed lanterns and carved
/// pumpkins keep burning underwater, matching vanilla block behavior).
/// Returns `None` for non-glowing items and for emitters that quench.
pub fn item_dynamic_light_spec(item_name: &str, submerged: bool) -> Option<DynamicLightSpec> {
    let emitter = resolve_item_light_emitter(
        item_name,
        vanilla_item_light_level(item_name),
        None,
        submerged,
    )?;
    if emitter.quenches_underwater {
        return None;
    }
    Some(spec_from_emitter(&emitter))
}

/// `radius * intensity` reach-power heuristic used to pick the single
/// brightest candidate when one anchor offers several light sources
/// (mainhand vs offhand, mob glow vs held torch).
fn brightest_emitter(
    a: Option<ResolvedLightEmitter>,
    b: Option<ResolvedLightEmitter>,
) -> Option<ResolvedLightEmitter> {
    let power = |e: &ResolvedLightEmitter| e.radius * e.intensity;
    match (a, b) {
        (Some(x), Some(y)) => Some(if power(&x) >= power(&y) { x } else { y }),
        (x, None) => x,
        (None, y) => y,
    }
}

/// Half-extent (blocks) of the placed luminous-block scan box —
/// `(2*8+1)^3` = 4,913 `get_block_state` reads per call, each a cheap
/// palette lookup that short-circuits before any resolver work.
const PLACED_LIGHT_SCAN_RADIUS: i32 = 8;

/// Names of the redstone signal components whose registry luminance is
/// `0` in every state — wire, repeater, comparator and observer have no
/// `lit` luminance variants, so the scan cannot gate them on
/// `facts.luminance` the way lamps/torches/ores/bulbs are gated (their
/// lit states all carry `luminance > 0`). These are the only names the
/// scan feeds a synthesized `powered` property to; every other
/// luminance-0 cell is skipped before resolver work. Token-matched so
/// modded `*_wire`/`*_repeater`/`*_comparator`/`*_observer` names join
/// automatically.
fn is_always_dark_signal_block(block_id: &str) -> bool {
    let path = block_state_base_name(block_id)
        .rsplit(':')
        .next()
        .unwrap_or(block_id);
    path.split('_')
        .any(|token| matches!(token, "wire" | "repeater" | "comparator" | "observer"))
}

// ---------------------------------------------------------------------------
// Placed-block dynamic-light scan cache
// ---------------------------------------------------------------------------

/// One fully-resolved placed-block light inside the ±8 scan box. The
/// block-state-derived fields (world position, emitter, flicker phase)
/// freeze at scan time; only [`evaluate_modulated_intensity`] and the
/// camera distance re-run on the cached path, so steady-state frames
/// cost zero block/registry lookups.
#[derive(Clone, Copy, Debug)]
struct CachedPlacedLight {
    pos: [f32; 3],
    emitter: ResolvedLightEmitter,
    phase: f32,
}

/// Everything besides `tick_time_sec` (flicker re-evaluates every call)
/// and the sub-block camera position (distance re-measures every call)
/// that can change [`LiveSession::placed_block_light_candidates`]'s
/// output. `WorldState::section_mesh_revisions` alone cannot key the
/// cache — they bump only when the mesh lane re-derives artifacts,
/// which a bounded rebuild budget defers past the frame an edit landed
/// on — so the world mutation counters ride along: they bump
/// synchronously on every `apply_block_update*`, `apply_chunk` and
/// column unload, and the per-section `Option<u64>` revisions keep a
/// loaded-rev-0 section distinct from an unloaded (`None`) one.
#[derive(Clone, Default, PartialEq)]
struct PlacedLightScanKey {
    /// Camera's integer block coordinate — the ±8 window anchor.
    center_block: [i32; 3],
    /// Address of the `Registry` inside `WorldState::registry` —
    /// catches attach/detach of the whole `Option`.
    registry_slot_ptr: usize,
    /// Heap pointer into the registry's interned name storage —
    /// catches in-place `Option<Registry>` replacement, which reuses
    /// the same slot address.
    registry_name_ptr: usize,
    registry_state_count: usize,
    /// `WorldState::state_count` palette bound (session/test reassigns
    /// it next to `registry`).
    world_state_count: usize,
    block_updates: u64,
    chunks_decoded: u64,
    chunks_unloaded: u64,
    /// `columns.len()` — catches direct `columns` inserts/removals and
    /// `reset_dimension`'s wholesale clear.
    loaded_columns: usize,
    /// Per-section mesh revisions of the (always exactly eight)
    /// sections the ±8 box overlaps, in fixed `sy/sz/sx` scan order.
    section_revs: [((i32, i32, i32), Option<u64>); 8],
}

/// Per-frame reused result of the ±8 placed-light scan.
#[derive(Default)]
struct PlacedLightScanCache {
    key: PlacedLightScanKey,
    valid: bool,
    entries: Vec<CachedPlacedLight>,
    hits: u64,
    misses: u64,
}

#[derive(Clone, Default, PartialEq)]
struct NatureAmbienceScanKey {
    center_block: [i32; 3],
    block_table_ptr: usize,
    nature_registry_ptr: usize,
    state_count: usize,
    block_updates: u64,
    chunks_decoded: u64,
    chunks_unloaded: u64,
    loaded_columns: usize,
    section_revs: [((i32, i32, i32), Option<u64>); 8],
}

#[derive(Default)]
struct NatureAmbienceScanCache {
    key: NatureAmbienceScanKey,
    valid: bool,
    leaf_blocks: Vec<[f32; 3]>,
    blossom_blocks: Vec<[f32; 3]>,
    evergreen_blocks: Vec<[f32; 3]>,
    rain_exposed_water: Vec<[f32; 3]>,
    waterfall_bases: Vec<([f32; 3], f32)>,
    hits: u64,
    misses: u64,
    block_reads: u64,
}

/// Per-state resolution memo for one scan pass: `facts`, the
/// always-dark signal classification and the resolver output are all
/// pure functions of `(state, registry)`, so a state repeated across
/// thousands of cells resolves exactly once.
#[derive(Clone, Copy)]
struct PlacedStateEval {
    emitter: ResolvedLightEmitter,
    torch: bool,
    wall_torch: bool,
}

/// How one overlapping section supplies block states to the scan.
/// `Skip` covers cells `get_block_state` would answer `None` for
/// (missing column or missing section payload); `Uniform`/`Dense`
/// cover real palettes. Sections wholly outside the dimension's
/// height range fold into `Uniform(0)` upstream, matching
/// `get_block_state`'s out-of-range `Some(0)` air sentinel.
#[derive(Clone, Copy)]
enum PlacedSectionStates<'a> {
    Skip,
    Uniform(u32),
    Dense(&'a megu_kernel::PalettedSection),
}

/// The (always exactly two) section coordinates and section-local cell
/// ranges a ±[`PLACED_LIGHT_SCAN_RADIUS`] window covers along one
/// axis: the span `c-8..=c+8` is exactly 17 cells, so the
/// `div_euclid(16)` of its endpoints can only ever be adjacent
/// sections — `(lo_s, lo.rem..=15)` then `(lo_s+1, 0..=hi.rem)`.
fn placed_scan_axis(c: i32) -> [(i32, std::ops::RangeInclusive<i32>); 2] {
    let lo = c.wrapping_sub(PLACED_LIGHT_SCAN_RADIUS);
    let hi = c.wrapping_add(PLACED_LIGHT_SCAN_RADIUS);
    [
        (lo.div_euclid(16), lo.rem_euclid(16)..=15),
        (lo.div_euclid(16).wrapping_add(1), 0..=hi.rem_euclid(16)),
    ]
}

/// Resolve one block state into its cacheable placed-light evaluation
/// (base emitter before the per-cell wall-torch mount adjustment), or
/// `None` when the state cannot emit — the exact gate sequence the
/// uncached per-cell path applied.
fn placed_state_eval(state: u32, registry: &megu_kernel::Registry) -> Option<PlacedStateEval> {
    let facts = registry.facts(state).ok()?;
    if facts.is_air != 0 {
        return None;
    }
    let name = registry.state_name(state).unwrap_or_default();
    let signal = is_always_dark_signal_block(name);
    if facts.luminance == 0 && !signal {
        return None;
    }
    // `properties` carries only what the per-state facts table can
    // prove: waterlog status and — for the always-luminance-0 signal
    // components — the `powered` cue the resolver needs (the registry
    // keeps no per-state property strings, so an unpowered wire reads
    // as its faint powered glow).
    let mut props: [(&str, &str); 2] = [("", ""); 2];
    let mut prop_len = 0usize;
    if facts.is_waterlogged != 0 {
        props[prop_len] = ("waterlogged", "true");
        prop_len += 1;
    }
    if signal {
        props[prop_len] = ("powered", "true");
        prop_len += 1;
    }
    let input = BlockLightInput {
        block_id: name,
        properties: &props[..prop_len],
        registry_luminance: facts.luminance,
        texture_rgba: None,
        submerged: facts.is_waterlogged != 0,
    };
    let emitter = resolve_block_light_emitter(&input)?;
    if emitter.quenches_underwater {
        return None;
    }
    Some(PlacedStateEval {
        emitter,
        torch: torch_flame_kind(name).is_some(),
        wall_torch: name.contains("wall"),
    })
}

/// Emit `(dist², light)` pairs for cached placed-light entries with the
/// live `tick_time_sec` and sub-block camera — the per-frame work both
/// the hit and miss paths share.
fn push_cached_placed_lights(
    entries: &[CachedPlacedLight],
    camera: [f32; 3],
    tick_time_sec: f32,
    out: &mut Vec<(f32, megu_client_mesh::vfx_engine::DynamicColoredLight)>,
) {
    use megu_client_mesh::vfx_engine::DynamicColoredLight;
    out.reserve(entries.len());
    for entry in entries {
        let intensity = evaluate_modulated_intensity(&entry.emitter, tick_time_sec, entry.phase);
        if !intensity.is_finite() || intensity <= 0.0 {
            continue;
        }
        let delta = [
            entry.pos[0] - camera[0],
            entry.pos[1] - camera[1],
            entry.pos[2] - camera[2],
        ];
        out.push((
            delta[0] * delta[0] + delta[1] * delta[1] + delta[2] * delta[2],
            DynamicColoredLight {
                pos: entry.pos,
                rgb: entry.emitter.rgb,
                radius: entry.emitter.radius,
                intensity,
                ttl_ticks: 2,
                priority: 1,
            },
        ));
    }
}

// ---------------------------------------------------------------------------
// Grounded contact shadows beneath standing mobs
// ---------------------------------------------------------------------------

/// Cap on contact-shadow quads collected per call — the same 64-entry
/// budget the dynamic-light UBO uses, bounded against pathological
/// entity counts.
pub const MAX_CONTACT_SHADOW_QUADS: usize = 64;

/// Maximum drop (blocks) the floor probe below an entity searches — an
/// entity with no collision surface inside this range is airborne past
/// the shadow's usefulness and emits no quad.
const CONTACT_SHADOW_PROBE_DEPTH: f64 = 4.0;

/// Height above the floor (blocks) at which the contact shadow has
/// fully faded; entities airborne beyond this emit no quad.
const CONTACT_SHADOW_FADE_BLOCKS: f32 = 2.5;

/// One soft, flat contact-shadow quad a mob/creature rig casts onto the
/// solid floor beneath it — the lit-entity staging feed that keeps mobs
/// visually grounded indoors, under cloud shade and when the cascade
/// shadow pass is Off/Feather. `positions` winds counter-clockwise seen
/// from above in the `[0,1,0]` normal plane; `rgba` is a near-black
/// shadow tint whose alpha fades linearly with `height_above_floor`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct EntityContactShadowQuad {
    /// Tracked entity id the shadow belongs to.
    pub entity_id: i32,
    /// World-space quad corners, all at `floor_top + 0.015`.
    pub positions: [[f32; 3]; 4],
    /// Face normal — always `[0.0, 1.0, 0.0]`.
    pub normal: [f32; 3],
    /// Shadow tint `[r, g, b, a]`; alpha `0.30` at ground level, fading
    /// to `0` at [`CONTACT_SHADOW_FADE_BLOCKS`] height.
    pub rgba: [f32; 4],
    /// Vertical distance (blocks) between the entity's feet and the
    /// shadow plane — `0` when standing, `<= 2.5` while hopping.
    pub height_above_floor: f32,
}

// ---------------------------------------------------------------------------
// 3D enchantment-glint shimmer on enchanted stacks
// ---------------------------------------------------------------------------

/// 1.21.1 data-component type ids whose presence (with a non-empty
/// entry list) grants the enchantment glint: `minecraft:enchantments`
/// (`9`) and `minecraft:stored_enchantments` (`23`, enchanted books).
const ITEM_GLINT_COMPONENT_IDS: [i32; 2] = [9, 23];
const COMP_MAX_DAMAGE: i32 = 2;
const COMP_DAMAGE: i32 = 3;
const COMP_CUSTOM_NAME: i32 = 5;
const COMP_LORE: i32 = 7;
const COMP_ENCHANTMENTS: i32 = 9;
const COMP_STORED_ENCHANTMENTS: i32 = 23;
const EFFICIENCY_ENCHANTMENT_ID: i32 = 8;
const AQUA_AFFINITY_ENCHANTMENT_ID: i32 = 0;

/// `true` when the decoded stack carries an enchantment component with
/// at least one entry — vanilla's `ItemStack.hasGlint` proxy driving
/// the violet shimmer layer on 3D dropped/held item quads and armor.
pub fn item_stack_has_glint(stack: &ItemStackComponents) -> bool {
    stack.item_id > 0
        && stack.count > 0
        && stack.added.iter().any(|patch| {
            ITEM_GLINT_COMPONENT_IDS.contains(&patch.type_id)
                // First payload byte is the entry-count varint: any
                // non-zero value means at least one enchantment.
                && patch.data.first().is_some_and(|&count| count != 0)
        })
}

/// Owns UI-facing world data on the event-loop thread. The worker owns its socket.
pub struct LiveSession {
    incoming: Receiver<WorkerMessage>,
    outgoing: SyncSender<WorkerCommand>,
    _offline_sender: Option<SyncSender<WorkerMessage>>,
    /// Drained on demand in offline fixtures so packet-producing public
    /// methods obey bounded queue semantics without a socket worker.
    offline_command_receiver: Option<Receiver<WorkerCommand>>,
    /// Ground bit attached to the newest accepted movement command. This is
    /// the same value the worker writes into `move_player_pos_rot` (`0x1b`).
    pub last_movement_on_ground: Option<bool>,
    pub status: SessionStatus,
    /// Bounded packet history reserved for instant replay capture.
    pub replay_ring: crate::replay_ring::PacketReplayRing,
    replay_started: Instant,
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
    /// Scalar metadata surfaced per entity from `set_entity_data`
    /// (`parse_entity_metadata`) — flags, air, custom name, pose, health,
    /// absorption and item-entity stacks.
    pub entity_metadata: BTreeMap<i32, LiveEntityMeta>,
    /// Active mob effects per entity from `update_mob_effect` /
    /// `remove_mob_effect` (bounded per entity).
    pub mob_effects: BTreeMap<i32, BTreeMap<i32, LiveMobEffect>>,
    /// Latest sneak key state mirrored from [`PlayerMovement::sneak`] —
    /// feeds `interact`'s secondary-action flag.
    pub sneaking: bool,
    /// Local-movement physics mirror advanced by [`Self::predict_movement`]
    /// (sneak edge prevention runs inside `step_player_physics`).
    physics_state: PlayerPhysicsState,
    physics_accumulator: f32,
    prev_pose: Option<PlayerPose>,
    prev_eye_height: f32,
    prev_walk_bob_phase: f32,
    was_on_ground: bool,
    was_submerged: bool,
    walk_dist: f32,
    next_step_dist: f32,
    fall_velocity_y: f32,
    walk_bob_phase: f32,
    eye_height: f32,
    status_effect_accumulator: f32,
    mining: MiningState,
    item_use: ItemUseState,
    swing_progress: f32,
    attack_cooldown: f32,
    attack_speed: f32,
    hurt_flash_alpha: f32,
    /// Local player's `hurt_animation` play event — vanilla
    /// `LivingEntity.hurtTime`/`hurtDuration` plus the attack direction the
    /// window's `GameRenderer.bobHurt` tilt reads.
    local_hurt: Option<LocalHurtAnimation>,
    thunder_flash: f32,
    ambient_bgm_cooldown: f32,
    ambient_bgm_clock: f32,
    riding_vehicle: Option<i32>,
    remote_block_destruction: BTreeMap<i32, ([i32; 3], u8)>,
    sleeping: bool,
    /// Entity the crosshair ray currently rests on (see `refresh_target`).
    pub looking_at_entity: Option<i32>,
    /// Server's open-container instruction, if any.
    pub open_screen: Option<LiveOpenScreen>,
    /// Namespace-generic clientbound `custom_payload` inbox: every valid
    /// `namespace:path` channel outside megu's typed `megu:*` family is
    /// validated, tracked and surfaced here (see
    /// [`megu_client::protocol::CustomPayloadInbox`]).
    pub custom_payload_inbox: CustomPayloadInbox,
    /// Locally toggled crafter grid state, keyed by `(window, slot)`.
    crafter_disabled_slots: BTreeMap<(u8, i32), bool>,
    /// `container_set_data` properties keyed by `(window_id, property)` —
    /// persisted even when no `open_screen` matches (late opens, window-0
    /// sync) so a screen opened after the updates still shows them.
    pub container_properties: BTreeMap<(i32, i16), i16>,
    /// `player_abilities` mirror — creative flight/invulnerability and
    /// fly/walk speeds synced from the server (see [`LivePlayerAbilities`]).
    pub player_abilities: LivePlayerAbilities,
    /// Server-pushed resource-pack UUIDs, most recent last; a `resource_pack_pop`
    /// with no UUID clears the whole stack (bounded at 32 like vanilla's
    /// pack list which practically never exceeds a handful).
    pub server_resource_packs: VecDeque<[u8; 16]>,
    /// The latest play-phase `ping` id the server sent (answered by the
    /// worker at read time — the field records that the event arrived so
    /// tests/debug tooling can observe it).
    pub last_server_ping: Option<i32>,
    /// Smoothed forced camera orientation target from `player_look_at`
    /// `(pose yaw, pitch)` in protocol degrees.
    player_look_at_target: Option<(f32, f32)>,
    /// In-flight play-phase ping probe `(nonce, send time)`.
    pending_ping_probe: Option<(i64, Instant)>,
    last_ping_probe_at: Instant,
    ping_probe_nonce: i64,
    /// Latest `pong_response` round-trip latency in milliseconds.
    pub pong_rtt_ms: Option<u32>,
    /// Wall-clock start of the active server chunk batch.
    chunk_batch_started_at: Option<Instant>,
    /// Duration of the most recent completed chunk batch.
    pub chunk_batch_last_ms: Option<u32>,
    /// The view distance last announced via `client_information`; the
    /// settings screen resends on change through
    /// [`Self::resend_client_information`].
    pub client_view_distance: i8,
    /// Maximum number of chat lines retained in the HUD scrollback.
    chat_history_limit: usize,
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
    /// Decoded plain text of the latest title/subtitle packets.
    pub title_text: Option<String>,
    pub subtitle_text: Option<String>,
    /// Bumped on every title-related packet so the UI can `show()` once on
    /// the event edge instead of re-arming per frame.
    pub title_revision: u64,
    /// Block under the crosshair ray as `(block_pos, registry_name)` —
    /// the Jade-style looking-at overlay's live feed.
    pub looking_at_block: Option<([i32; 3], String)>,
    /// The kernel `qol_gameplay` projection of the live crosshair
    /// target — name, mod badge and detail lines refreshed every frame
    /// alongside `looking_at_block`/`looking_at_entity`.
    pub looking_at_hud: Option<LookingAtHudInfo>,
    /// `set_action_bar_text` raw anonymous-NBT action-bar line.
    pub actionbar_nbt: Option<Vec<u8>>,
    /// Validated `transfer` reconnect target, if the server issued one.
    pub pending_transfer: Option<TransferTarget>,
    /// `open_sign_editor` instruction waiting for the window to lift it
    /// into `UiState::sign_editor`: `(block_pos, is_front)`.
    pending_sign_editor: Option<((i32, i32, i32), bool)>,
    /// `open_book` instructions waiting for the window's book-screen host,
    /// in arrival order. Bounded because only one book screen can be open.
    pending_open_book_hands: VecDeque<u8>,
    /// Vanilla use-item/container sequence counter echoed on `use_item`,
    /// `use_item_on`, `player_action`, and `container_click` packets.
    use_item_sequence: i32,
    /// Block positions each in-flight interaction sequence may have
    /// predicted: `use_item_on` records the clicked block plus the
    /// face-adjacent placement cell, `player_action` (dig) records the
    /// dig target. `block_changed_ack` drains every entry at or below
    /// the acknowledged sequence and re-dirties the touched sections so
    /// stale predictions reconcile against the authoritative world —
    /// vanilla's ghost-block fix under latency and protected regions.
    pending_interaction_seqs: BTreeMap<i32, Vec<[i32; 3]>>,
    /// Highest `block_changed_ack` sequence seen (diagnostics/tests).
    pub highest_acknowledged_sequence: i32,
    /// Server `set_center_chunk` view anchor — the prefetch cone anchors
    /// here instead of at the last pose when the server moves the view
    /// center (vehicles, forced spectate, teleport lag).
    pub view_center_chunk: Option<(i32, i32)>,
    /// Per-item-registry-id cooldown state from `cooldown` S2C packets:
    /// `remaining_ticks`/`total_ticks` at 20 Hz, mirrored into
    /// `hud.item_cooldowns` as the hotbar wipe fraction.
    item_cooldowns: BTreeMap<i32, ItemCooldown>,
    /// Count of `container_click` commands the worker queue refused because
    /// it was full. A refused click never reaches the server, so the UI's
    /// prediction is unconfirmed: the window watches this counter and
    /// restores its slots from [`Self::open_screen`] /
    /// [`Self::player_inventory`] instead of leaving ghost items behind.
    pub dropped_container_clicks: u64,
    /// Latest `command_suggestions` reply awaiting chat-bar consumption
    /// (`transaction_id` matches [`Self::request_command_suggestion`]).
    pub command_suggestions: Option<ServerCommandSuggestions>,
    /// Revision bumped per stored reply — the window polls it once per
    /// frame instead of diffing the payload.
    pub command_suggestions_rev: u64,
    /// Monotonic C2S `command_suggestion` transaction counter.
    command_suggestion_tx: i32,
    /// Server-declared custom chat-completion words
    /// (`custom_chat_completions` S2C actions add/remove/set).
    pub custom_chat_completions: BTreeSet<String>,
    /// Revision for [`Self::custom_chat_completions`] mirroring into the
    /// chat bar's word set.
    pub custom_chat_completions_rev: u64,
    /// Deletion keys parallel to `hud.chat_lines`: `Some(index,
    /// signature)` for `player_chat` lines, `None` for unsigned/system
    /// lines. `delete_chat` retracts by signature (or index fallback);
    /// `reset_chat` clears both. Kept in sync by
    /// [`Self::reconcile_chat_keys`] even when host code appends to
    /// `hud.chat_lines` directly.
    chat_log_keys: std::collections::VecDeque<Option<ChatMessageKey>>,
    /// Launcher filesystem paths this session resolves assets against
    /// (`--gameDir`/`--assetsDir`/`--assetIndex`/`--version`/
    /// `--resourcePackDir`). Empty means env-lookup defaults only.
    pub session_paths: SessionPaths,
    /// Asset store root actually handed to the audio runtime (CLI wins over
    /// `MEGU_ASSETS_DIR`).
    pub resolved_assets_dir: Option<PathBuf>,
    /// Asset index id actually handed to the audio runtime (CLI wins over
    /// `MEGU_ASSET_INDEX`).
    pub resolved_asset_index: Option<String>,
    /// Window-0 player-inventory slot mirror: `container_set_content` /
    /// `container_set_slot` targeting window 0 lands here even without an
    /// `open_screen` — the `E`-key inventory renders from it.
    pub player_inventory: BTreeMap<i32, ItemStackComponents>,
    /// Window-0 carried (cursor) stack, when the server set one.
    pub player_carried: Option<ItemStackComponents>,
    /// `state_id` of the latest window-0 content sync (echoed on clicks).
    pub player_inventory_state_id: i32,
    /// `update_advancements` tree mirror keyed by advancement id.
    pub advancements: BTreeMap<String, LiveAdvancement>,
    /// `update_advancements` progress mirror keyed by advancement id.
    pub advancement_progress: BTreeMap<String, LiveAdvancementProgress>,
    /// Tab the server last selected via `select_advancements_tab`, or the
    /// tab we announced when opening the screen.
    pub advancements_tab: Option<String>,
    /// Resident metadata of the lazily compiled resource-pack bundle
    /// (atlas placements + blockstates + model stack + animations) built
    /// once when `MEGU_RESOURCE_PACK_DIR` points at a pack root
    /// containing `assets/`; absent env keeps the procedural fallback.
    /// The multi-MiB RGBA payloads never live here — they exist only in
    /// [`Self::pending_atlas_pixels`] until `upload_block_atlas` drops
    /// them, so steady state holds zero CPU atlas bytes and VRAM is the
    /// sole owner of atlas texels.
    pub atlas_meta: Option<CompiledAssetMeta>,
    /// CPU-side atlas RGBA + mip chain awaiting
    /// `GpuRenderer::upload_block_atlas`. Cleared to `None` the moment the
    /// upload is consumed — the GPU texture is then the only copy. A
    /// `SurfaceError::Lost` re-derives the payloads on demand through
    /// [`Self::request_atlas_reload`].
    pub pending_atlas_pixels: Option<PendingAtlasPixels>,
    asset_bundle_tried: bool,
    /// UI item-icon atlas (`item_id → uv` + RGBA) packed from the same
    /// decoded `assets/*/textures/{item,block}` sprites the block atlas
    /// pass consumed — `window.rs` uploads `rgba` once and keeps the uv
    /// map for `UiCommand::ItemIcon` tessellation.
    pub item_icon_atlas: Option<megu_ui::draw::ItemIconAtlas>,
    /// Set when a bundle landed but `GpuRenderer::upload_block_atlas`
    /// has not consumed it yet (window.rs owns the GPU upload).
    pub asset_bundle_pending_upload: bool,
    /// Source-by-source record of the last asset discovery pass — which
    /// JAR/packs loaded, what they contributed, and what was skipped.
    pub asset_discovery: AssetDiscovery,
    /// Client-side command dispatcher seeded with the vanilla command-name
    /// set; `complete_suggestions` feeds chat autocomplete while typing.
    pub commands: CommandDispatcher,
    player_entity_id: Option<i32>,
    /// Permission level reported by the server for the local player.
    pub permission_level: u8,
    /// Death & respawn loop state — `player_combat_kill` /
    /// `set_health(<=0)` mark death, `respawn`/`set_health(>0)` clear it,
    /// and the window reads this to drive `Screen::Death` plus the
    /// input lock.
    pub death: DeathLoopState,
    /// Local player game mode mirrored from `game_event` id 3
    /// (`change_game_mode`): 0 survival, 1 creative, 2 adventure,
    /// 3 spectator. Drives the HUD vitals gate (`hud.game_mode`).
    pub game_mode: i32,
    /// `game_event` id 11 (`enable_respawn_screen`): `false` is
    /// vanilla's immediate respawn — the death screen never opens and
    /// `PERFORM_RESPAWN` re-requests automatically on death.
    pub respawn_screen_enabled: bool,
    /// `game_event` id 4 (`win_game`) with `value != 0` asks the client
    /// to roll credits before the respawn. Recorded for the future
    /// credits surface; cleared on `respawn`/`login` via
    /// [`Self::reset_dimension`].
    pub pending_end_credits: bool,
    pub hud: HudState,
    pub magic: MagicHudState,
    pub vfx: ClientVfxHost,
    pub motion: Option<DecodedMotionTrigger>,
    pub pose: Option<PlayerPose>,
    /// Frame pose derived by the window after this frame's physics/look
    /// update ([`RenderFramePose`]). Both crosshair raycasts, the mining
    /// target, and the block outline read it so a pick always matches the
    /// frame that is on screen; `None` falls back to the raw pose.
    render_frame_pose: Option<RenderFramePose>,
    pub camera_sync: Option<PlayerPose>,
    pub terrain_dirty: bool,
    /// Sections play events flagged since the last renderer sync —
    /// `GpuRenderer::update_live_terrain_tracked` reconciles exactly this
    /// set instead of scanning every world section. Column-level events
    /// (chunk data/unload, light updates) mark the column plus the four
    /// lateral neighbor columns so `MeshCache`'s boundary invalidation can
    /// never escape the tracked set.
    pub dirty_terrain_coords: BTreeSet<SectionCoord>,
    /// A non-incremental world transition (dimension reset, first sync)
    /// demands one legacy full reconcile before dirty tracking is trusted.
    pub terrain_full_sync: bool,
    /// The last poll crossed the translucent re-sort threshold, so
    /// resident-section orders may have changed outside the dirty set —
    /// the renderer runs its cheap order sweep on the next sync.
    pub terrain_order_sweep: bool,
    /// Upload payloads the off-thread `MeshJob::SectionMesh` lane and the
    /// same-frame priority-0 lane produced for sections `WorldState` has
    /// not re-derived yet. `window.rs` snapshots this into
    /// `GpuRenderer::external_sections` ahead of every terrain reconcile;
    /// entries are pruned each poll once their `content_epoch` or
    /// `world_revision` moves on.
    pub external_sections: BTreeMap<SectionCoord, ExternalSectionMesh>,
    /// Revision bumped on every `external_sections` mutation — the
    /// renderer mirror compares it against its synced revision and
    /// skips the delivery outright on steady frames (zero map clones).
    pub external_sections_rev: u64,
    /// Renderer-bound per-coord changes accumulated since the last
    /// [`Self::take_external_sections_delta`] drain — latest write wins
    /// per coord, so the drain carries each touched section exactly once
    /// no matter how many mutations landed between frames.
    external_section_delta: ExternalSectionsDelta,
    /// Worker-emitted official baked model faces by section. Entries are
    /// installed only after the matching packed mesh result passes the
    /// lane revision check, then shared into `ExternalSectionMesh` for the
    /// renderer's shaped-terrain upload lane.
    pub baked_sections: HashMap<SectionCoord, Arc<[BakedTerrainFace]>>,
    /// Bumped whenever shaped terrain provenance changes (worker face
    /// sidecar install/removal or external-section clear). Render-side
    /// caches key rebuilds off their section mesh revisions; this counter
    /// is the session-visible proof that the sidecar domain moved.
    pub shaped_terrain_revision: u64,
    /// Catalog/sprite table captured when the runtime asset bundle was
    /// built. Retained so the lazily-spawned section mesh pool receives
    /// the same configuration as the synchronous world mesher.
    block_model_catalog: Option<Arc<VanillaBlockModelCatalog>>,
    block_model_sprite_lookup: Option<Arc<BTreeMap<String, u32>>>,
    /// World-side coords whose CPU mesh copies were released after VRAM
    /// upload. A fresh renderer (device loss / surface re-create) owns no
    /// GPU copy of them, so [`Self::requeue_cpu_released_sections`]
    /// re-dirties the set and the mesh lanes rebuild the payloads.
    cpu_released_world_coords: BTreeSet<SectionCoord>,
    /// Lazily-spawned worker pool for `MeshJob::SectionMesh` bulk
    /// meshing — created only when a dirty tail actually exceeds the
    /// inline rebuild budget, so quiet or headless sessions never pay a
    /// thread.
    section_mesh_workers: Option<MeshJobPool>,
    /// `(coord, epoch)` bookkeeping deduplicating in-flight `SectionMesh`
    /// jobs and dropping results a newer dirty mark superseded.
    section_mesh_lane: SectionMeshLane,
    /// Off-thread lane candidates: coords the play events marked dirty
    /// that are still unmeshed and loaded. Pruned by
    /// [`Self::submit_offthread_sections`] each poll.
    offthread_candidates: BTreeSet<SectionCoord>,
    /// `MeshJob::SectionMesh` jobs accepted by the worker queue,
    /// cumulative — proves the off-thread lane submits live.
    pub offthread_mesh_submitted: u64,
    /// `SectionMesh` results the drain accepted and published into
    /// [`Self::external_sections`], cumulative.
    pub offthread_mesh_completed: u64,
    /// `SectionMesh` results dropped because their section re-dirtied
    /// after the snapshot (stale revision), cumulative.
    pub offthread_mesh_stale: u64,
    /// Sections the priority-0 near-player lane rebuilt inline this
    /// session — the 0-frame-latency block-edit path.
    pub priority_zero_meshes: u64,
    /// Frame-budget governor override on [`Self::mesh_rebuild_budget`]
    /// (`usize::MAX` = unlimited): window.rs clamps it live per
    /// [`ThrottleStage`].
    pub governor_rebuild_cap: usize,
    /// Lifetime count of sections meshed by the camera-frustum +
    /// distance-ordered dirty drain in [`Self::poll`] — proves the
    /// ordered rebuild path executes live.
    pub ordered_rebuilds_count: u64,
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
    /// Latest cached `/proc` + DRM host snapshot; refreshed at most every
    /// [`COMPACT_DEBUG_HUD_SYS_SAMPLE_INTERVAL_MS`] of accumulated frame time while
    /// the compact debug HUD is enabled.
    compact_debug_hud_host: Option<sys_metrics::HostMetricsSnapshot>,
    /// Frame milliseconds accumulated since the last host-metric sample.
    compact_debug_hud_sys_accum_ms: f32,
    /// Lifetime count of `sample_host_metrics` calls — the throttled
    /// `/proc`/DRM reads the compact debug HUD would otherwise do every frame.
    pub sys_sample_count: u64,
    /// Adaptive packed-nibble kernel autotuner seeded from
    /// `compute_strategy` and fed real frame times by
    /// `record_frame_and_refresh_compact_debug_hud` and real mesh-batch durations
    /// by [`Self::poll`].
    kernel_autotuner: KernelAutotuner,
    /// Counted `kernel_autotuner.observe_sample` feeds — proves the
    /// feedback loop actually executes live, not just exists.
    autotuner_sample_count: u64,
    /// Polls elapsed since the last kernel challenger probe; reaching
    /// [`KERNEL_PROBE_INTERVAL_POLLS`] arms one poll under the
    /// alternate [`ComputeKernelMode`].
    kernel_probe_polls: u64,
    /// The kernel mode this poll actually dispatches under —
    /// `kernel_autotuner.selected_mode()` on ordinary polls, the
    /// challenger mode during a probe. `observe_sample` credits the
    /// executed mode, never the incumbent by default, so challenger
    /// observations can actually flip `selected`.
    executed_kernel_mode: ComputeKernelMode,
    /// Counted challenger-probe dispatches — proves the probe loop
    /// executes live, not just exists.
    kernel_probe_count: u64,
    /// Jain–Chlamtac `P²` streaming frame-time quantiles (136 bytes
    /// each, zero heap): P95 feeds the anti-windup budget PID and the
    /// window's upload-budget edge; P99 is the live 1%-low telemetry.
    frame_p2_p95: P2QuantileEstimator,
    frame_p2_p99: P2QuantileEstimator,
    /// Anti-windup PID regulating the per-poll mesh-rebuild budget
    /// scale toward a 60 FPS-class frame time from the streaming P95.
    pid_budget: PidBudgetController,
    /// Latest PID output in `[0.05, 1.0]`; `1.0` until the P² P95
    /// bootstraps its five markers. `mesh_rebuild_budget` multiplies
    /// by this.
    budget_scale: f64,
    /// Garivier–Moulines discounted-UCB bandit over four background
    /// mesh-drain profiles (`ducb_submit_cap` arm → off-thread
    /// submission cap). [`Self::poll`] consumes `select_arm` live each
    /// poll, executes that profile's drain bound, and credits the arm
    /// with the real frame/mesh timings it produced — so the bandit
    /// prices profiles by measured outcome and re-explores them when a
    /// regime switch (scene density, thermal) decays the stale winner.
    ducb: DiscountedUcbAutotuner<4>,
    /// The drain-profile arm [`Self::poll`] is executing this poll —
    /// `ducb.select_arm()` consumed live, then fed the timings that
    /// profile produced.
    ducb_arm_selected: usize,
    /// Counted `ducb.observe` feeds alongside `autotuner_sample_count`.
    ducb_sample_count: u64,
    /// Unified optimization policy resolved from the detected profile
    /// (refreshed by [`Self::apply_compute_strategy`]); the renderer
    /// consumes its `chunk_mesh`/`gpu_cull`/`shadow` projections.
    unified_plan: UnifiedOptimizationPlan,
    /// The three domain projections client-gpu applies: `chunk_mesh`
    /// (`Meshing` — arena record ordering), `gpu_cull` (`Lighting` —
    /// visibility/occlusion dispatch), `shadow` (`ShadowRendering`).
    domain_hints: ClientDomainHints,
    /// Times [`Self::apply_compute_strategy`] pushed the session strategy
    /// into a renderer.
    compute_strategy_apply_count: u64,
    /// Bounded `(window_id, recipe, make_all)` log of accepted
    /// `place_recipe` requests — the observable edge between the UI's
    /// `+` dispatch and the worker channel.
    pub place_recipe_requests: Vec<(u8, String, bool)>,
    /// Per-deck integer-cell key each layered voxel-cloud mesh was last
    /// generated for: `(cloud_cx, cloud_cz, scroll_cell, cloud_layers,
    /// cloud_height_bits)`. Between key hits only the sub-cell X offset
    /// changes, which slides through the existing `SubWorld` model
    /// matrix instead of rebuilding the mesh. The
    /// [`generate_layered_voxel_clouds_with_atmosphere`] gate
    /// (`has_skylight` + `density >= MIN_SKY_ATMOSPHERE`) is evaluated
    /// before the deck loop — a closed gate drops every deck, so a
    /// dimension swap or orbit-ascent blend cannot reuse a stale sheet.
    cloud_deck_cache: [CloudDeckCache; 3],
    /// How many cloud decks `refresh_voxel_clouds` keeps resident and how
    /// they're shaded — set by `window.rs` from `CloudMode` x
    /// `VisualStylePreset`. `Vanilla` preserves the classic single-deck
    /// behaviour (greedy-meshed, no parallax/bob/hybrid optics).
    pub cloud_deck_mode: CloudDeckMode,
    has_vanilla_cloud_texture: bool,
    /// Composed frames that reused the cached cloud mesh with a
    /// matrix-only sub-cell scroll slide.
    pub cloud_mesh_reuses: u64,
    pub atmosphere_blender: BiomeAtmosphereBlender,
    pub chunk_fades: ChunkFadeTracker,
    pub subworlds: SubWorldRenderBank,
    /// Raw `megu:portal_seamless_v1` (`MSP1`) frames captured by
    /// [`Self::apply_custom`], drained each frame by the window into the
    /// seamless dimension-transfer state machine. Bounded so a burst
    /// cannot grow without limit.
    seamless_portal_frames: VecDeque<Vec<u8>>,
    subworld_sync_received_at: Option<Instant>,
    /// SW6D-flagged sub-world the local player pilots (pose `flags` bit
    /// [`SUBWORLD_POSE_PLAYER_ABOARD`]); `None` while the latest snapshot
    /// marks no sub-world as player-controlled.
    player_subworld_id: Option<u64>,
    /// Monotone revision bumped whenever any HUD-visible field mutates
    /// (chat, scoreboard, actionbar, title, health/hunger/hotbar,
    /// subtitle, magic HUD). The window's sync skips the `HudState`
    /// clone while the revision is unchanged.
    pub hud_revision: u64,
    /// The last `hud_revision` the window consumed via
    /// [`Self::sync_hud_to`]; `u64::MAX` until the first sync.
    pub last_synced_hud_revision: u64,
    /// Lifetime count of skipped HUD clones — proves the revision gate
    /// fires on idle frames.
    pub hud_sync_skips: u64,
    /// Registered kinetic parts keyed by stable part id; resolved into
    /// [`InstancedPartRecord`]s by [`Self::sync_kinetic_network_rpm`].
    kinetic_parts: BTreeMap<u32, KineticPartSpec>,
    /// Live wire positions to their render-part ids.
    kinetic_pos_to_id: HashMap<(i32, (i32, i32, i32)), u32>,
    /// Latest wire telemetry for the active dimension, keyed by world position.
    kinetic_telemetry: HashMap<(i32, i32, i32), KineticSyncRecord>,
    /// Dimension-scoped telemetry mirror keyed `(dimension_id, pos)`; the
    /// legacy render map above contains only the active dimension.
    kinetic_telemetry_dimensioned: HashMap<(i32, (i32, i32, i32)), KineticSyncRecord>,
    /// Dimension/generation-aware chunked snapshot reassembler (`KNT1`).
    kinetic_reassembler: KineticSyncReassembler,
    /// Lifecycle store keyed by `(dimension_id, pos)` shared with mesh consumers.
    kinetic_sync_store: KineticSyncStore,
    /// Live angle accumulator, independent for each wire-synchronized part.
    kinetic_live_angles: HashMap<u32, f32>,
    /// Per-`mesh_kind` scratch record lists retained between syncs — the
    /// swap in `sync_kinetic_network_rpm` trades the fresh list for the
    /// batch's stale one, so steady-state frames allocate nothing.
    kinetic_scratch: BTreeMap<u32, Vec<InstancedPartRecord>>,
    /// Current shared kinetic network speed in RPM.
    pub kinetic_network_rpm: f32,
    /// Seconds of accumulated network run time; `phase = phase0 +
    /// rpm * ratio * TAU/60 * t` reads from it.
    kinetic_time_secs: f32,
    /// Allocating counter for `register_kinetic_part` return ids.
    next_kinetic_part_id: u32,
    pub vanilla_plus_cfg: VanillaPlusPipelineConfig,
    pub nature_mod_registry: NatureModRegistry,
    /// Latest `megu:mod_registry_sync_v1` (`MRS1`) frame — maps dynamic
    /// block-state ids (at/past the vanilla table) and item ids to their
    /// real `namespace:name`s for the goggles HUD and looking-at overlay.
    pub mod_registry: Option<ClientModRegistry>,
    /// `MRS1` frames that failed to decode (diagnostics parity with
    /// `vfx_rejected`/`animation_rejected`).
    pub mod_registry_rejected: u64,
    /// Latest structured server/local mod-sync mismatch (`MSY1` consumer).
    pub mod_sync_diff: Option<LiveModSyncDiff>,
    /// `MSY1` frames rejected by the bounded consumer.
    pub mod_sync_rejected: u64,
    /// Root hash already acknowledged through `MSH1`; repeat matching
    /// manifests do not spam the bounded worker queue.
    mod_sync_ack_root: Option<[u8; 32]>,
    /// Mod-JAR `assets/*/lang` translation table loaded for `ui_locale`;
    /// `None` until first use so connect stays lazy.
    mod_translations: Option<ModTranslationTable>,
    /// Generated `resourcepacks/megu_auto_ja_jp` overlay, active only when
    /// the pack is present in the configured bottom-to-top pack stack.
    auto_translation_overlay: Option<BTreeMap<String, String>>,
    /// UI locale code the window pushes (`settings.locale.code()`),
    /// normalized the way `ModTranslationTable::load` expects.
    ui_locale: String,
    /// The latest decoded `SSN1` season sync, or `None` before the first
    /// `megu:season_v1` frame (and on seasonless dimensions).
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
    /// Listener-underwater flag streamed to the dedicated mix thread's
    /// `AudioCmd::UpdateListener`; the window layer derives it from
    /// [`Self::latest_audio_hint`]'s `underwater_lowpass`.
    listener_submerged: bool,
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
    /// One-refresh 360-degree prefetch boost ring emitted at a teleport
    /// arrival anchor before velocity-cone lookahead resumes.
    prefetch_arrival_ring: BTreeMap<(i32, i32), f32>,
    /// Jump delta already consumed as a teleport, so repeated refreshes of
    /// the same movement sample do not keep resetting the forward cone.
    prefetch_last_jump_delta: Option<[f64; 2]>,
    last_vfx_tick: Instant,
    last_animation_tick: Instant,
    /// Client-side wall-clock seconds driving `.mcmeta` sprite animation.
    /// Vanilla reticks `TextureManager` once per client tick (50 ms)
    /// regardless of server `time_of_day`, so this runs off `poll`'s own
    /// heartbeat rather than `world_time_ticks` (which wraps at 24 000
    /// and follows server `set_time` jumps).
    atlas_anim_clock: f64,
    last_atlas_anim_tick: Instant,
    /// Section-revision + mutation-counter-keyed cache for the ±8
    /// placed-block dynamic-light scan — `RefCell` so the
    /// `&self`-only [`Self::collect_dynamic_lights`] signature stays
    /// unchanged while steady-state frames skip the 4,913-cell sweep
    /// entirely.
    placed_light_scan: RefCell<PlacedLightScanCache>,
    nature_ambience_scan: RefCell<NatureAmbienceScanCache>,
}

/// `client_information` view distance announced when a connect site has
/// no settings screen to ask — offline fixtures and the bare
/// [`LiveSession::connect`] entry point. Windowed connects pass the
/// settings value through `connect_with_paths_and_auth` instead.
pub const CONNECT_VIEW_DISTANCE: i8 = 10;

/// Fixed stack encoder for replay event bodies. Large collections are reduced
/// to bounded counts or prefixes before their data enters the 4 MiB ring.
struct ReplayEventEncoder {
    bytes: [u8; 512],
    len: usize,
}

impl ReplayEventEncoder {
    fn new() -> Self {
        Self {
            bytes: [0; 512],
            len: 0,
        }
    }
    fn put(&mut self, value: &[u8]) {
        let take = value.len().min(self.bytes.len().saturating_sub(self.len));
        self.bytes[self.len..self.len + take].copy_from_slice(&value[..take]);
        self.len += take;
    }
    fn i32(&mut self, value: i32) {
        self.put(&value.to_le_bytes());
    }
    fn u32(&mut self, value: u32) {
        self.put(&value.to_le_bytes());
    }
    fn i64(&mut self, value: i64) {
        self.put(&value.to_le_bytes());
    }
    fn f32(&mut self, value: f32) {
        self.put(&value.to_le_bytes());
    }
    fn f64(&mut self, value: f64) {
        self.put(&value.to_le_bytes());
    }
    fn flag(&mut self, value: bool) {
        self.put(&[u8::from(value)]);
    }
    fn bytes(&mut self, value: &[u8]) {
        let len = value
            .len()
            .min(u16::MAX as usize)
            .min(self.bytes.len().saturating_sub(self.len + 2));
        self.put(&(len as u16).to_le_bytes());
        self.put(&value[..len]);
    }
    fn text(&mut self, value: &str) {
        self.bytes(value.as_bytes());
    }
    fn finish(self, tag: u16) -> (i32, [u8; 512], usize) {
        (0x4d52_0000 | i32::from(tag), self.bytes, self.len)
    }
}

/// Stable compact event tags (0x4d52_0000 | tag) and little-endian bodies.
/// This records decoded gameplay events without changing the packet decoder
/// or retaining its potentially large temporary packet allocations.
fn compact_replay_event(event: &PlayEvent) -> Option<(i32, [u8; 512], usize)> {
    use PlayEvent as E;
    let mut out = ReplayEventEncoder::new();
    let tag = match event {
        E::JoinGame {
            entity_id,
            dimension,
            hardcore,
        } => {
            out.i32(*entity_id);
            out.text(&dimension.name);
            out.i32(dimension.min_y);
            out.i32(dimension.height);
            out.flag(dimension.has_skylight);
            out.flag(*hardcore);
            1
        }
        E::Respawn { dimension } => {
            out.text(&dimension.name);
            out.i32(dimension.min_y);
            out.i32(dimension.height);
            out.flag(dimension.has_skylight);
            2
        }
        E::KeepAlive(id) => {
            out.i64(*id);
            3
        }
        E::Teleport(t) => {
            out.f64(t.x);
            out.f64(t.y);
            out.f64(t.z);
            out.f32(t.yaw);
            out.f32(t.pitch);
            out.i32(t.flags);
            out.i32(t.teleport_id);
            4
        }
        E::ChunkBatchStart => 5,
        E::ChunkData(chunk) => {
            out.i32(chunk.cx);
            out.i32(chunk.cz);
            out.u32(chunk.sections.len().min(u32::MAX as usize) as u32);
            let mut digest = 0xcbf2_9ce4_8422_2325u64;
            for section in &chunk.sections {
                out.i32(i32::from(section.non_air));
                for state in section.states.iter() {
                    digest ^= u64::from(*state);
                    digest = digest.wrapping_mul(0x100_0000_01b3);
                }
            }
            out.put(&digest.to_le_bytes());
            6
        }
        E::ChunkBatchFinished(count) => {
            out.i32(*count);
            7
        }
        E::UpdateLight { cx, cz, light } => {
            out.i32(*cx);
            out.i32(*cz);
            out.u32(light.sky_arrays.len() as u32);
            out.u32(light.block_arrays.len() as u32);
            8
        }
        E::BlockUpdate { pos, state_id } => {
            for v in pos {
                out.i32(*v);
            }
            out.u32(*state_id);
            9
        }
        E::SectionBlocksUpdate(entries) => {
            let n = entries.len().min(24);
            out.put(&[n as u8]);
            for (pos, state) in entries.iter().take(n) {
                for v in pos {
                    out.i32(*v);
                }
                out.u32(*state);
            }
            10
        }
        E::UnloadChunk { cx, cz } => {
            out.i32(*cx);
            out.i32(*cz);
            11
        }
        E::SetCenterChunk { cx, cz } => {
            out.i32(*cx);
            out.i32(*cz);
            12
        }
        E::SetHealth {
            health,
            food,
            saturation,
        } => {
            out.f32(*health);
            out.i32(*food);
            out.f32(*saturation);
            13
        }
        E::SystemChat { text, overlay } => {
            out.text(text);
            out.flag(*overlay);
            14
        }
        E::AddEntity {
            eid,
            uuid,
            entity_type,
            x,
            y,
            z,
            pitch,
            yaw,
            head_yaw,
            data,
            vx,
            vy,
            vz,
        } => {
            out.i32(*eid);
            out.put(uuid);
            out.i32(*entity_type);
            out.f64(*x);
            out.f64(*y);
            out.f64(*z);
            out.f32(*pitch);
            out.f32(*yaw);
            out.f32(*head_yaw);
            out.i32(*data);
            out.f64(*vx);
            out.f64(*vy);
            out.f64(*vz);
            15
        }
        E::RemoveEntities { eids } => {
            let n = eids.len().min(80);
            out.put(&[n as u8]);
            for eid in eids.iter().take(n) {
                out.i32(*eid);
            }
            16
        }
        E::MoveEntityPos {
            eid,
            dx,
            dy,
            dz,
            on_ground,
        } => {
            out.i32(*eid);
            out.f64(*dx);
            out.f64(*dy);
            out.f64(*dz);
            out.flag(*on_ground);
            17
        }
        E::MoveEntityPosRot {
            eid,
            dx,
            dy,
            dz,
            yaw,
            pitch,
            on_ground,
        } => {
            out.i32(*eid);
            out.f64(*dx);
            out.f64(*dy);
            out.f64(*dz);
            out.f32(*yaw);
            out.f32(*pitch);
            out.flag(*on_ground);
            18
        }
        E::MoveEntityRot {
            eid,
            yaw,
            pitch,
            on_ground,
        } => {
            out.i32(*eid);
            out.f32(*yaw);
            out.f32(*pitch);
            out.flag(*on_ground);
            19
        }
        E::TeleportEntity {
            eid,
            x,
            y,
            z,
            yaw,
            pitch,
            on_ground,
        } => {
            out.i32(*eid);
            out.f64(*x);
            out.f64(*y);
            out.f64(*z);
            out.f32(*yaw);
            out.f32(*pitch);
            out.flag(*on_ground);
            20
        }
        E::RotateHead { eid, head_yaw } => {
            out.i32(*eid);
            out.f32(*head_yaw);
            21
        }
        E::SetEntityMotion { eid, vx, vy, vz } => {
            out.i32(*eid);
            out.f64(*vx);
            out.f64(*vy);
            out.f64(*vz);
            22
        }
        E::EntityEvent { eid, status } => {
            out.i32(*eid);
            out.put(&[*status as u8]);
            23
        }
        E::HurtAnimation { eid, yaw } => {
            out.i32(*eid);
            out.f32(*yaw);
            24
        }
        E::SetEntityData { eid, raw_entries } => {
            out.i32(*eid);
            out.bytes(raw_entries);
            25
        }
        E::SetTime {
            world_age,
            time_of_day,
        } => {
            out.i64(*world_age);
            out.i64(*time_of_day);
            26
        }
        E::GameEvent { event, value } => {
            out.put(&[*event]);
            out.f32(*value);
            27
        }
        E::LevelEvent {
            event,
            pos,
            data,
            disable_relative_volume,
        } => {
            out.i32(*event);
            for v in pos {
                out.i32(*v);
            }
            out.i32(*data);
            out.flag(*disable_relative_volume);
            28
        }
        E::BlockDestruction {
            breaker_eid,
            pos,
            stage,
        } => {
            out.i32(*breaker_eid);
            for v in pos {
                out.i32(*v);
            }
            out.put(&[*stage as u8]);
            29
        }
        E::Explosion {
            x,
            y,
            z,
            power,
            records,
            knockback,
            block_interaction,
            small_particle_id,
            large_particle_id,
            ..
        } => {
            out.f64(*x);
            out.f64(*y);
            out.f64(*z);
            out.f32(*power);
            out.u32(records.len().min(u32::MAX as usize) as u32);
            for v in knockback {
                out.f32(*v);
            }
            out.i32(*block_interaction);
            out.i32(*small_particle_id);
            out.i32(*large_particle_id);
            30
        }
        E::BlockEntityData {
            pos,
            kind,
            nbt_bytes,
        } => {
            for v in pos {
                out.i32(*v);
            }
            out.i32(*kind);
            out.bytes(nbt_bytes);
            31
        }
        E::ContainerSetContent {
            window_id,
            state_id,
            slot_count,
            raw_payload,
        } => {
            out.put(&[*window_id]);
            out.i32(*state_id);
            out.i32(*slot_count);
            out.bytes(raw_payload);
            32
        }
        E::ContainerSetSlot {
            window_id,
            state_id,
            slot,
            raw_item,
        } => {
            out.put(&[*window_id]);
            out.i32(*state_id);
            out.put(&slot.to_le_bytes());
            out.bytes(raw_item);
            33
        }
        E::SetExperience { bar, level, total } => {
            out.f32(*bar);
            out.i32(*level);
            out.i32(*total);
            34
        }
        E::UpdateMobEffect {
            eid,
            effect_id,
            amplifier,
            duration,
            flags,
        } => {
            out.i32(*eid);
            out.i32(*effect_id);
            out.i32(*amplifier);
            out.i32(*duration);
            out.put(&[*flags]);
            35
        }
        E::RemoveMobEffect { eid, effect_id } => {
            out.i32(*eid);
            out.i32(*effect_id);
            36
        }
        E::SetActionBarText { nbt_bytes } => {
            out.bytes(nbt_bytes);
            37
        }
        E::SetTitleText { nbt_bytes } => {
            out.bytes(nbt_bytes);
            38
        }
        E::SetSubtitleText { nbt_bytes } => {
            out.bytes(nbt_bytes);
            39
        }
        E::BossEvent {
            uuid,
            action,
            raw_payload,
        } => {
            out.put(uuid);
            out.i32(*action);
            out.bytes(raw_payload);
            40
        }
        E::SetPassengers {
            vehicle,
            passengers,
        } => {
            out.i32(*vehicle);
            let n = passengers.len().min(80);
            out.put(&[n as u8]);
            for id in passengers.iter().take(n) {
                out.i32(*id);
            }
            41
        }
        E::TakeItemEntity {
            item_entity,
            player,
            amount,
        } => {
            out.i32(*item_entity);
            out.i32(*player);
            out.i32(*amount);
            42
        }
        E::SetCamera { eid } => {
            out.i32(*eid);
            43
        }
        E::PlayerChat {
            sender,
            index,
            text,
            nbt_bytes,
            ..
        } => {
            out.put(sender);
            out.i32(*index);
            out.text(text);
            out.bytes(nbt_bytes);
            44
        }
        E::DisguisedChat { text, nbt_bytes } => {
            out.text(text);
            out.bytes(nbt_bytes);
            45
        }
        E::Ping(id) => {
            out.i32(*id);
            46
        }
        E::BlockChangedAck(id) => {
            out.i32(*id);
            47
        }
        E::PlayerAbilities {
            flags,
            flying_speed,
            walking_speed,
        } => {
            out.put(&[*flags]);
            out.f32(*flying_speed);
            out.f32(*walking_speed);
            48
        }
        E::PlayerCombatKill {
            player_id,
            message_nbt,
        } => {
            out.i32(*player_id);
            out.bytes(message_nbt);
            49
        }
        E::Disconnect(reason) => {
            out.text(reason);
            50
        }
        E::Other(packet_id) => {
            out.i32(*packet_id);
            51
        }
        _ => return None,
    };
    Some(out.finish(tag))
}

impl LiveSession {
    /// Sample cached chunk light at a world-space position. Y is clamped to
    /// the active dimension's section range before indexing; unavailable
    /// positions use the sky-lit fallback.
    pub fn sample_block_and_sky_light(&self, wx: f32, wy: f32, wz: f32) -> (u8, u8) {
        fn floor_i32(value: f32) -> Option<i32> {
            if !value.is_finite() {
                return None;
            }
            let value = f64::from(value).floor();
            (value >= f64::from(i32::MIN) && value <= f64::from(i32::MAX)).then_some(value as i32)
        }

        let (Some(x), Some(y), Some(z)) = (floor_i32(wx), floor_i32(wy), floor_i32(wz)) else {
            return (0, 15);
        };
        let min_section_y = self.world.dimension.min_section_y();
        let Ok(section_count) = i32::try_from(self.world.dimension.section_count()) else {
            return (0, 15);
        };
        if section_count <= 0 {
            return (0, 15);
        }
        let min_y = min_section_y.saturating_mul(16);
        let max_y = min_y
            .saturating_add(section_count.saturating_mul(16))
            .saturating_sub(1);
        let y = y.clamp(min_y, max_y);
        let Some(column) = self
            .world
            .columns
            .get(&(x.div_euclid(16), z.div_euclid(16)))
        else {
            return (0, 15);
        };
        let section_index = y.div_euclid(16).saturating_sub(min_section_y) as usize;
        let Some(section) = column.sections.get(section_index) else {
            return (0, 15);
        };
        let index = megu_client_mesh::section_index(
            x.rem_euclid(16) as usize,
            y.rem_euclid(16) as usize,
            z.rem_euclid(16) as usize,
        );
        let nibble = |plane: &[u8; megu_client_mesh::LIGHT_BYTES]| {
            (plane[index / 2] >> ((index & 1) * 4)) & 0x0f
        };
        (nibble(&section.block_light), nibble(&section.sky_light))
    }

    pub fn connect(host: String, port: u16, username: String, version: String) -> Self {
        Self::connect_with_paths(host, port, username, version, SessionPaths::default())
    }

    /// Connect like [`Self::connect`], carrying the launcher's asset paths
    /// (`--gameDir`/`--assetsDir`/`--assetIndex`/`--version`/
    /// `--resourcePackDir`) into the sound library and texture discovery.
    pub fn connect_with_paths(
        host: String,
        port: u16,
        username: String,
        version: String,
        paths: SessionPaths,
    ) -> Self {
        Self::connect_with_paths_and_auth(
            host,
            port,
            username,
            version,
            paths,
            None,
            None,
            CONNECT_VIEW_DISTANCE,
        )
    }

    /// [`Self::connect_with_paths`] with the launcher's online-mode
    /// credentials forwarded to the worker's encryption-response path:
    /// `access_token` (`--accessToken`, [`SecretString`]-wrapped at the
    /// CLI) plus the account `--uuid` used both as the session-join
    /// `selectedProfile` and — only when a token is actually present —
    /// the login-start UUID. Offline joins keep `offline_uuid(username)`
    /// exactly as before. The token is moved straight into
    /// [`OnlineAuthOptions`]; it is never logged or exposed on `self`.
    /// `view_distance` is the `client_information` value announced at
    /// login — pass the live settings-screen distance so
    /// [`Self::resend_client_information`] compares against what the
    /// server actually heard.
    #[allow(clippy::too_many_arguments)]
    pub fn connect_with_paths_and_auth(
        host: String,
        port: u16,
        username: String,
        version: String,
        paths: SessionPaths,
        access_token: Option<SecretString>,
        account_uuid: Option<String>,
        view_distance: i8,
    ) -> Self {
        let auth = OnlineAuthOptions {
            access_token: access_token
                .filter(|token| token.is_present())
                .map(|token| token.reveal().to_owned()),
            profile_uuid: account_uuid.as_deref().and_then(|raw| parse_uuid(raw).ok()),
            ..OnlineAuthOptions::default()
        };
        let login_uuid = match (auth.access_token.is_some(), auth.profile_uuid) {
            (true, Some(uuid)) => uuid,
            _ => offline_uuid(&username),
        };
        let (tx, incoming) = mpsc::sync_channel(1024);
        let (outgoing, rx) = mpsc::sync_channel(8);
        std::thread::spawn(move || {
            run_worker(
                host,
                port,
                username,
                version,
                login_uuid,
                auth,
                tx,
                rx,
                view_distance,
            )
        });
        let mut session =
            Self::with_channels_and_sink(incoming, outgoing, AudioSinkKind::Auto, paths);
        session.client_view_distance = view_distance;
        session
    }

    #[cfg(test)]
    pub(crate) fn with_channels(
        incoming: Receiver<WorkerMessage>,
        outgoing: SyncSender<WorkerCommand>,
    ) -> Self {
        Self::with_channels_and_sink(
            incoming,
            outgoing,
            AudioSinkKind::Null,
            SessionPaths::default(),
        )
    }

    /// Offline live-session fixture with deterministic null audio output.
    /// Accepts the same chunk, block and sound events as the network path.
    pub fn offline_with_null_audio() -> Self {
        Self::offline_with_paths(SessionPaths::default())
    }

    /// [`Self::offline_with_null_audio`] carrying launcher asset paths —
    /// the display-free way tests prove `--assetsDir`/`--assetIndex` and
    /// `--gameDir` JAR discovery reach the session.
    pub fn offline_with_paths(paths: SessionPaths) -> Self {
        let (tx, incoming) = mpsc::sync_channel(256);
        let (outgoing, command_rx) = mpsc::sync_channel(8);
        let mut session =
            Self::with_channels_and_sink(incoming, outgoing, AudioSinkKind::Null, paths);
        session._offline_sender = Some(tx);
        session.offline_command_receiver = Some(command_rx);
        session
    }

    fn with_channels_and_sink(
        incoming: Receiver<WorkerMessage>,
        outgoing: SyncSender<WorkerCommand>,
        sink: AudioSinkKind,
        paths: SessionPaths,
    ) -> Self {
        let hw_caps = detect_host_hardware_caps();
        let compute_strategy = select_compute_strategy(&hw_caps);
        // Seed the autotuner with the strategy's static kernel preference;
        // `record_frame_and_refresh_compact_debug_hud` feeds it real frame times.
        let kernel_autotuner =
            KernelAutotuner::new(if compute_strategy.prefer_lut_over_recompute() {
                ComputeKernelMode::LutBatch
            } else {
                ComputeKernelMode::RecomputeSwar
            });
        // The session dispatches under the seeded mode until the first
        // challenger probe flips `executed_kernel_mode` for one poll.
        let seeded_kernel_mode = kernel_autotuner.selected_mode();
        // The unified plan + client domain projections are resolved once
        // at connect and refreshed on every `apply_compute_strategy` push.
        let unified_plan = build_unified_optimization_plan(
            &detect_universal_hardware_profile(),
            Some(kernel_autotuner.selected_mode()),
        );
        let domain_hints = ClientDomainHints::for_plan(&unified_plan);
        let vfx = ClientVfxHost::new(GpuPipelineKind::Classic);
        let tactile_tier = vfx.budget_tier();
        let tactile_config = TactileEffectsConfig::default();
        let blocks = if let Some(game_dir) = paths.game_dir.as_deref() {
            let blocks_json = crate::vanilla_bootstrap::discover_local_server_blocks(game_dir);
            BlockTable::load_for_game_dir(game_dir, blocks_json.as_deref())
        } else {
            BlockTable::load(None)
        };
        let world = WorldState::new(DimensionMeta::default(), blocks);
        let acoustic_world = WorldAcousticSnapshot::new(&world);
        // Launcher `--assetsDir`/`--assetIndex` win over the env fallbacks;
        // the resolved pair is what `SoundLibrary::load` actually opens.
        let resolved_assets_dir = paths
            .assets_dir
            .clone()
            .or_else(|| std::env::var_os("MEGU_ASSETS_DIR").map(Into::into));
        let resolved_asset_index = paths
            .asset_index
            .clone()
            .or_else(|| std::env::var("MEGU_ASSET_INDEX").ok());
        let mut audio = AudioRuntime::new(&AudioSessionConfig {
            sink,
            assets_dir: resolved_assets_dir.clone(),
            asset_index: resolved_asset_index.clone(),
            game_dir: paths.game_dir.clone(),
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
            offline_command_receiver: None,
            last_movement_on_ground: None,
            status: SessionStatus::Connecting,
            replay_ring: crate::replay_ring::PacketReplayRing::default(),
            replay_started: Instant::now(),
            world,
            audio,
            acoustic_world,
            entity_positions: BTreeMap::new(),
            entity_rigs: BTreeMap::new(),
            entity_equipment: BTreeMap::new(),
            entity_metadata: BTreeMap::new(),
            mob_effects: BTreeMap::new(),
            sneaking: false,
            physics_state: PlayerPhysicsState::default(),
            physics_accumulator: 0.0,
            prev_pose: None,
            prev_eye_height: 1.62,
            prev_walk_bob_phase: 0.0,
            was_on_ground: false,
            was_submerged: false,
            walk_dist: 0.0,
            next_step_dist: 0.0,
            fall_velocity_y: 0.0,
            walk_bob_phase: 0.0,
            eye_height: 1.62,
            status_effect_accumulator: 0.0,
            mining: MiningState::default(),
            item_use: ItemUseState::default(),
            swing_progress: 0.0,
            attack_cooldown: 1.0,
            attack_speed: 4.0,
            hurt_flash_alpha: 0.0,
            local_hurt: None,
            thunder_flash: 0.0,
            ambient_bgm_cooldown: 0.0,
            ambient_bgm_clock: 600.0,
            riding_vehicle: None,
            remote_block_destruction: BTreeMap::new(),
            sleeping: false,
            looking_at_entity: None,
            open_screen: None,
            custom_payload_inbox: CustomPayloadInbox::default(),
            crafter_disabled_slots: BTreeMap::new(),
            container_properties: BTreeMap::new(),
            player_abilities: LivePlayerAbilities::default(),
            server_resource_packs: VecDeque::new(),
            last_server_ping: None,
            player_look_at_target: None,
            pending_ping_probe: None,
            last_ping_probe_at: Instant::now(),
            ping_probe_nonce: 0,
            pong_rtt_ms: None,
            chunk_batch_started_at: None,
            chunk_batch_last_ms: None,
            client_view_distance: 10,
            chat_history_limit: 100,
            scoreboard: LiveScoreboard::default(),
            player_info: BTreeMap::new(),
            boss_bars: BTreeMap::new(),
            title_times: None,
            title_text_nbt: None,
            subtitle_text_nbt: None,
            title_text: None,
            subtitle_text: None,
            title_revision: 0,
            looking_at_block: None,
            looking_at_hud: None,
            actionbar_nbt: None,
            pending_transfer: None,
            pending_sign_editor: None,
            pending_open_book_hands: VecDeque::new(),
            use_item_sequence: 0,
            pending_interaction_seqs: BTreeMap::new(),
            highest_acknowledged_sequence: 0,
            view_center_chunk: None,
            item_cooldowns: BTreeMap::new(),
            dropped_container_clicks: 0,
            command_suggestions: None,
            command_suggestions_rev: 0,
            command_suggestion_tx: 0,
            custom_chat_completions: BTreeSet::new(),
            custom_chat_completions_rev: 0,
            chat_log_keys: std::collections::VecDeque::new(),
            session_paths: paths,
            resolved_assets_dir,
            resolved_asset_index,
            player_inventory: BTreeMap::new(),
            player_carried: None,
            player_inventory_state_id: 0,
            advancements: BTreeMap::new(),
            advancement_progress: BTreeMap::new(),
            advancements_tab: None,
            atlas_meta: None,
            pending_atlas_pixels: None,
            asset_bundle_tried: false,
            item_icon_atlas: None,
            asset_bundle_pending_upload: false,
            asset_discovery: AssetDiscovery::default(),
            commands: seeded_command_dispatcher(),
            player_entity_id: None,
            permission_level: 0,
            death: DeathLoopState::default(),
            game_mode: 0,
            respawn_screen_enabled: true,
            pending_end_credits: false,
            hud: HudState::default(),
            magic: MagicHudState::default(),
            vfx,
            motion: None,
            pose: None,
            render_frame_pose: None,
            camera_sync: None,
            terrain_dirty: false,
            dirty_terrain_coords: BTreeSet::new(),
            // Until the first sync finishes, residency is unknown — the
            // renderer needs one legacy full reconcile to establish it.
            terrain_full_sync: true,
            terrain_order_sweep: false,
            external_sections: BTreeMap::new(),
            baked_sections: HashMap::new(),
            shaped_terrain_revision: 0,
            block_model_catalog: None,
            block_model_sprite_lookup: None,
            external_sections_rev: 0,
            external_section_delta: ExternalSectionsDelta::default(),
            cpu_released_world_coords: BTreeSet::new(),
            section_mesh_workers: None,
            section_mesh_lane: SectionMeshLane::default(),
            offthread_candidates: BTreeSet::new(),
            offthread_mesh_submitted: 0,
            offthread_mesh_completed: 0,
            offthread_mesh_stale: 0,
            priority_zero_meshes: 0,
            governor_rebuild_cap: usize::MAX,
            ordered_rebuilds_count: 0,
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
            compact_debug_hud_host: None,
            compact_debug_hud_sys_accum_ms: 0.0,
            sys_sample_count: 0,
            kernel_autotuner,
            autotuner_sample_count: 0,
            kernel_probe_polls: 0,
            executed_kernel_mode: seeded_kernel_mode,
            kernel_probe_count: 0,
            frame_p2_p95: P2QuantileEstimator::new(0.95),
            frame_p2_p99: P2QuantileEstimator::new(0.99),
            pid_budget: PidBudgetController::new(1_000.0 / 60.0),
            budget_scale: 1.0,
            ducb: DiscountedUcbAutotuner::new(),
            ducb_arm_selected: 0,
            ducb_sample_count: 0,
            unified_plan,
            domain_hints,
            compute_strategy_apply_count: 0,
            place_recipe_requests: Vec::new(),
            cloud_deck_cache: [CloudDeckCache::default(); 3],
            cloud_deck_mode: CloudDeckMode::default(),
            has_vanilla_cloud_texture: false,
            cloud_mesh_reuses: 0,
            atmosphere_blender: BiomeAtmosphereBlender::new(AtmosphereParams::default()),
            chunk_fades: ChunkFadeTracker::new(),
            subworlds: SubWorldRenderBank::new(),
            seamless_portal_frames: VecDeque::new(),
            subworld_sync_received_at: None,
            player_subworld_id: None,
            hud_revision: 0,
            last_synced_hud_revision: u64::MAX,
            hud_sync_skips: 0,
            kinetic_parts: BTreeMap::new(),
            kinetic_pos_to_id: HashMap::new(),
            kinetic_telemetry: HashMap::new(),
            kinetic_telemetry_dimensioned: HashMap::new(),
            kinetic_reassembler: KineticSyncReassembler::default(),
            kinetic_sync_store: KineticSyncStore::default(),
            kinetic_live_angles: HashMap::new(),
            kinetic_scratch: BTreeMap::new(),
            kinetic_network_rpm: 0.0,
            kinetic_time_secs: 0.0,
            next_kinetic_part_id: 0,
            vanilla_plus_cfg: VanillaPlusPipelineConfig::default(),
            nature_mod_registry: NatureModRegistry::default(),
            mod_registry: None,
            mod_registry_rejected: 0,
            mod_sync_diff: None,
            mod_sync_rejected: 0,
            mod_sync_ack_root: None,
            mod_translations: None,
            auto_translation_overlay: None,
            ui_locale: "en_us".to_owned(),
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
            listener_submerged: false,
            tactile_event_tick: 0,
            tactile_last_pose: None,
            tactile_landing_impulse: 0.0,
            last_motion_delta: [0.0; 2],
            prefetch_arrival_ring: BTreeMap::new(),
            prefetch_last_jump_delta: None,
            last_vfx_tick: Instant::now(),
            last_animation_tick: Instant::now(),
            atlas_anim_clock: 0.0,
            last_atlas_anim_tick: Instant::now(),
            placed_light_scan: RefCell::new(PlacedLightScanCache::default()),
            nature_ambience_scan: RefCell::new(NatureAmbienceScanCache::default()),
        }
    }

    /// Evaluate opt-in block enhancements with the session's active pack
    /// configuration and modded-nature registry.
    pub fn evaluate_block_visuals(&self, input: &BlockVisualInput) -> BlockVisualEnhancementResult {
        megu_client_mesh::mesher::evaluate_block_visual_pipeline(
            input,
            &self.vanilla_plus_cfg,
            &self.nature_mod_registry,
        )
    }

    fn update_nature_ambience_sites(&mut self) {
        let center = self.nature_env.origin.map(|v| v.floor() as i32);
        let mut section_revs = [((0, 0, 0), None); 8];
        let mut slot = 0;
        for (sy, _) in placed_scan_axis(center[1]) {
            for (sz, _) in placed_scan_axis(center[2]) {
                for (sx, _) in placed_scan_axis(center[0]) {
                    section_revs[slot] = (
                        (sx, sy, sz),
                        self.world.section_mesh_revision(SectionCoord(sx, sy, sz)),
                    );
                    slot += 1;
                }
            }
        }
        let key = NatureAmbienceScanKey {
            center_block: center,
            block_table_ptr: &self.world.blocks as *const BlockTable as usize,
            nature_registry_ptr: &self.nature_mod_registry as *const NatureModRegistry as usize,
            state_count: self.world.state_count,
            block_updates: self.world.block_updates,
            chunks_decoded: self.world.chunks_decoded,
            chunks_unloaded: self.world.chunks_unloaded,
            loaded_columns: self.world.columns.len(),
            section_revs,
        };
        let mut cache = self.nature_ambience_scan.borrow_mut();
        let manual_sites = |current: &[[f32; 3]], scanned: &[[f32; 3]]| {
            current
                .iter()
                .copied()
                .filter(|site| !scanned.contains(site))
                .collect::<Vec<_>>()
        };
        let manual_leaves = manual_sites(&self.nature_env.leaf_blocks, &cache.leaf_blocks);
        let manual_blossoms = manual_sites(&self.nature_env.blossom_blocks, &cache.blossom_blocks);
        let manual_evergreens =
            manual_sites(&self.nature_env.evergreen_blocks, &cache.evergreen_blocks);
        let manual_water = manual_sites(
            &self.nature_env.rain_exposed_water,
            &cache.rain_exposed_water,
        );
        let manual_waterfalls: Vec<_> = self
            .nature_env
            .waterfall_bases
            .iter()
            .copied()
            .filter(|site| !cache.waterfall_bases.contains(site))
            .collect();
        if cache.valid && cache.key == key {
            cache.hits += 1;
        } else {
            cache.misses += 1;
            let [cx, cy, cz] = center;
            let min_y = i64::from(self.world.dimension.min_y);
            let max_y = min_y + i64::from(self.world.dimension.height);
            let min_sy = self.world.dimension.min_section_y();
            let max_sy = min_sy + self.world.dimension.section_count() as i32;
            let mut leaves = Vec::new();
            let mut blossoms = Vec::new();
            let mut evergreens = Vec::new();
            let mut water = Vec::new();
            let waterfalls = Vec::new();
            let mut reads = 0u64;
            for (sy, ys) in placed_scan_axis(cy) {
                for (sz, zs) in placed_scan_axis(cz) {
                    for (sx, xs) in placed_scan_axis(cx) {
                        let Some(section) = (sy >= min_sy && sy < max_sy)
                            .then(|| {
                                self.world
                                    .columns
                                    .get(&(sx, sz))
                                    .and_then(|c| c.sections.get((sy - min_sy) as usize))
                            })
                            .flatten()
                        else {
                            continue;
                        };
                        let uniform = section.palette.uniform_state();
                        if let Some(state) = uniform {
                            let Some(name) = self.world.blocks.state_name(state) else {
                                continue;
                            };
                            let path = block_state_base_name(name)
                                .rsplit(':')
                                .next()
                                .unwrap_or(name);
                            let block_center = |s: i32, r: &std::ops::RangeInclusive<i32>| {
                                s.wrapping_mul(16)
                                    .wrapping_add(8.clamp(*r.start(), *r.end()))
                            };
                            let pos = [
                                block_center(sx, &xs) as f32 + 0.5,
                                block_center(sy, &ys) as f32 + 0.5,
                                block_center(sz, &zs) as f32 + 0.5,
                            ];
                            if path.contains("leaves") {
                                if path.contains("blossom")
                                    || path.contains("sakura")
                                    || path.contains("cherry")
                                {
                                    if blossoms.len() < 256 {
                                        blossoms.push(pos);
                                    }
                                } else if path.contains("spruce")
                                    || path.contains("pine")
                                    || path.contains("conifer")
                                {
                                    if evergreens.len() < 256 {
                                        evergreens.push(pos);
                                    }
                                } else if leaves.len() < 256 {
                                    leaves.push(pos);
                                }
                            }
                            if (path.contains("water")
                                || self.world.blocks.fluid_cell(state).kind.is_some())
                                && water.len() < 256
                            {
                                water.push([pos[0], pos[1] + 0.5, pos[2]]);
                            }
                            // Uniform air/stone and other irrelevant sections
                            // terminate here; uniform emitters contribute one
                            // representative anchor without expanding 4096 cells.
                            continue;
                        }
                        let mut dense = [0u32; SECTION_CELLS];
                        if uniform.is_none() {
                            section.palette.unpack_into(&mut dense);
                        }
                        for ly in ys.clone() {
                            let y = sy.wrapping_mul(16).wrapping_add(ly);
                            if i64::from(y) < min_y || i64::from(y) >= max_y {
                                continue;
                            }
                            for lz in zs.clone() {
                                let z = sz.wrapping_mul(16).wrapping_add(lz);
                                for lx in xs.clone() {
                                    let x = sx.wrapping_mul(16).wrapping_add(lx);
                                    let state = match uniform {
                                        Some(state) => state,
                                        None => {
                                            reads += 1;
                                            dense[section_index(
                                                lx as usize,
                                                ly as usize,
                                                lz as usize,
                                            )]
                                        }
                                    };
                                    let Some(name) = self.world.blocks.state_name(state) else {
                                        continue;
                                    };
                                    let path = block_state_base_name(name)
                                        .rsplit(':')
                                        .next()
                                        .unwrap_or(name);
                                    let pos = [x as f32 + 0.5, y as f32 + 0.5, z as f32 + 0.5];
                                    if path.contains("leaves") {
                                        if path.contains("blossom")
                                            || path.contains("sakura")
                                            || path.contains("cherry")
                                        {
                                            if blossoms.len() < 256 {
                                                blossoms.push(pos);
                                            }
                                        } else if path.contains("spruce")
                                            || path.contains("pine")
                                            || path.contains("conifer")
                                        {
                                            if evergreens.len() < 256 {
                                                evergreens.push(pos);
                                            }
                                        } else if leaves.len() < 256 {
                                            leaves.push(pos);
                                        }
                                    }
                                    if (path.contains("water")
                                        || self.world.blocks.fluid_cell(state).kind.is_some())
                                        && water.len() < 256
                                    {
                                        reads += 1;
                                        let above =
                                            self.world.get_block_state(x, y.saturating_add(1), z);
                                        if above.is_none_or(|sid| self.world.blocks.is_air(sid)) {
                                            water.push([
                                                x as f32 + 0.5,
                                                y as f32 + 1.0,
                                                z as f32 + 0.5,
                                            ]);
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
            cache.block_reads = cache.block_reads.wrapping_add(reads);
            cache.leaf_blocks = leaves;
            cache.blossom_blocks = blossoms;
            cache.evergreen_blocks = evergreens;
            cache.rain_exposed_water = water;
            cache.waterfall_bases = waterfalls;
            cache.key = key;
            cache.valid = true;
        }
        self.nature_env.leaf_blocks = manual_leaves;
        self.nature_env
            .leaf_blocks
            .extend_from_slice(&cache.leaf_blocks);
        self.nature_env.blossom_blocks = manual_blossoms;
        self.nature_env
            .blossom_blocks
            .extend_from_slice(&cache.blossom_blocks);
        self.nature_env.evergreen_blocks = manual_evergreens;
        self.nature_env
            .evergreen_blocks
            .extend_from_slice(&cache.evergreen_blocks);
        self.nature_env.rain_exposed_water = manual_water;
        self.nature_env
            .rain_exposed_water
            .extend_from_slice(&cache.rain_exposed_water);
        self.nature_env.waterfall_bases = manual_waterfalls;
        self.nature_env
            .waterfall_bases
            .extend_from_slice(&cache.waterfall_bases);
    }

    /// Nature site scan cache telemetry: `(hits, misses, dense state reads)`.
    pub fn nature_ambience_scan_stats(&self) -> (u64, u64, u64) {
        let cache = self.nature_ambience_scan.borrow();
        (cache.hits, cache.misses, cache.block_reads)
    }

    /// Advance the retained nature-ambience particle sim by `dt_sec` using
    /// `nature_env` plus the live `season_ambience`. The pack's own
    /// `enabled`/`nature_ambience` toggles gate spawning, so a default
    /// (vanilla) config ticks existing particles out silently.
    pub fn step_nature_ambience(&mut self, dt_sec: f32) {
        self.update_nature_ambience_sites();
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

    /// Set the listener-underwater flag sent on the next
    /// `AudioCmd::UpdateListener` to the dedicated mix thread. The
    /// window layer derives it from [`Self::latest_audio_hint`]'s
    /// `underwater_lowpass`, so the real-time audio path applies the
    /// same submersion muffle the synchronous mixer already had.
    pub fn set_listener_submerged(&mut self, submerged: bool) {
        self.listener_submerged = submerged;
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

    /// Authoritative server world clock (`set_time` time_of_day,
    /// reduced to the 0..24000 day cycle) — the renderer's
    /// `world_time_ticks` and the tactile/eclipse systems all key off it.
    pub fn world_time_ticks(&self) -> u64 {
        self.world.time_of_day.rem_euclid(24_000) as u64
    }

    /// Whole game ticks elapsed on the client-side `.mcmeta` animation
    /// clock (20 ticks per wall-clock second) — the value
    /// `GpuRenderer::tick_atlas_animations` consumes each frame.
    pub fn atlas_anim_tick(&self) -> u64 {
        (self.atlas_anim_clock * 20.0) as u64
    }

    /// Current VFX1 dynamic colored lights for the frame's light UBO.
    pub fn dynamic_colored_lights(&self) -> Vec<megu_client_mesh::vfx_engine::DynamicColoredLight> {
        let forward = self.pose.map_or([0.0, 0.0, 1.0], |p| {
            let (yaw, pitch) = (p.yaw.to_radians(), p.pitch.to_radians());
            [
                -yaw.sin() * pitch.cos(),
                -pitch.sin(),
                yaw.cos() * pitch.cos(),
            ]
        });
        self.vfx.frame_output(forward).lights
    }

    /// `true` while the local player's eye block contains fluid — quenches
    /// held open-flame dynamic lights and feeds the tactile wetness/drip
    /// tracker (vanilla eye height 1.62 above the feet pose).
    pub fn player_eye_submerged(&self) -> bool {
        self.pose.is_some_and(|pose| {
            self.world
                .fluid_cell_at(
                    pose.feet[0].floor() as i32,
                    (pose.feet[1] + 1.62).floor() as i32,
                    pose.feet[2].floor() as i32,
                )
                .kind
                .is_some()
        })
    }

    /// This frame's gameplay-driven dynamic lights for the 64-light WGSL
    /// UBO: the local player's brightest held emitter (mainhand/offhand,
    /// open flames quenched while the eyes are submerged), remote-entity
    /// held glow from `entity_equipment`, naturally luminous mobs,
    /// burning entities (metadata flag bit `0x01`), spectral-glow
    /// entities (bit `0x40`), dropped glowing item stacks and every
    /// placed luminous block inside an 8-block scan box — all resolved
    /// through the universal `client-port` resolver, intensity-modulated
    /// via [`evaluate_modulated_intensity`], sorted nearest-first to
    /// `camera` and truncated to [`MAX_DYNAMIC_LIGHTS`].
    /// The Video tab `Dynamic Lights` toggle gates this at the call
    /// site (`video.dynamic_lights`), not here — World Effects owns the
    /// nature ambience (leaves/ripples/splashes), never the torch and
    /// entity lights.
    pub fn collect_dynamic_lights(
        &self,
        camera: [f32; 3],
    ) -> Vec<megu_client_mesh::vfx_engine::DynamicColoredLight> {
        use megu_client_mesh::vfx_engine::DynamicColoredLight;
        let tick_time_sec = self.world_time_ticks() as f32 / 20.0;
        let mut candidates: Vec<(f32, DynamicColoredLight)> = Vec::new();
        let mut push = |pos: [f32; 3], emitter: ResolvedLightEmitter, phase: f32| {
            let intensity = evaluate_modulated_intensity(&emitter, tick_time_sec, phase);
            if !intensity.is_finite() || intensity <= 0.0 {
                return;
            }
            let delta = [pos[0] - camera[0], pos[1] - camera[1], pos[2] - camera[2]];
            candidates.push((
                delta[0] * delta[0] + delta[1] * delta[1] + delta[2] * delta[2],
                DynamicColoredLight {
                    pos,
                    rgb: emitter.rgb,
                    radius: emitter.radius,
                    intensity,
                    // Rebuilt every frame — a two-tick TTL only matters if
                    // a consumer ever feeds these through the VFX engine.
                    ttl_ticks: 2,
                    priority: 1,
                },
            ));
        };
        // Local player: brightest of mainhand (window-0 container slot
        // 36 + selected hotbar index) and offhand (slot 45), anchored at
        // the held-item flame tip — a hand's width forward and right of
        // the eye at `feet + 1.48`, so a carried torch lights the wall
        // the player faces rather than their own head.
        if let Some(pose) = self.pose {
            let submerged = self.player_eye_submerged();
            let held = [36 + self.hud.selected_slot.min(8) as i32, 45]
                .into_iter()
                .filter_map(|slot| self.player_inventory.get(&slot))
                .filter(|stack| stack.item_id > 0 && stack.count > 0)
                .fold(None, |best, stack| {
                    brightest_emitter(best, self.held_item_emitter(stack, submerged))
                });
            if let Some(emitter) = held {
                let yaw = pose.yaw.to_radians();
                // Vanilla convention: yaw 0 faces +z; the horizontal
                // forward/right pair puts the tip slightly ahead and to
                // the player's right hand.
                let forward = [-yaw.sin(), yaw.cos()];
                let right = [-yaw.cos(), -yaw.sin()];
                push(
                    [
                        (pose.feet[0] + f64::from(0.35 * forward[0] + 0.25 * right[0])) as f32,
                        (pose.feet[1] + 1.48) as f32,
                        (pose.feet[2] + f64::from(0.35 * forward[1] + 0.25 * right[1])) as f32,
                    ],
                    emitter,
                    0.0,
                );
            }
        }
        // Remote entities: luminous mobs, burning entities, dropped glow
        // items and equipment-held emitters — one light per entity, the
        // brightest candidate winning. The entity's own center-cell fluid
        // check quenches held open flames underwater just like the local
        // player's eye check.
        for (eid, rig, pos, _tracked) in self.entity_render_entries() {
            if Some(eid) == self.player_entity_id {
                continue;
            }
            let center = [
                pos[0] as f32,
                (pos[1] + f64::from(rig.height) * 0.5) as f32,
                pos[2] as f32,
            ];
            let submerged = self
                .world
                .fluid_cell_at(
                    center[0].floor() as i32,
                    center[1].floor() as i32,
                    center[2].floor() as i32,
                )
                .kind
                .is_some();
            let flags = self
                .entity_metadata
                .get(&eid)
                .and_then(|meta| meta.flags)
                .unwrap_or(0);
            let mut emitter = resolve_entity_light_emitter(
                rig.name,
                flags & 0x01 != 0,
                flags & 0x40 != 0,
                vanilla_entity_light_level(rig.name),
                None,
            );
            // `minecraft:item` drops carry their stack in metadata index 8.
            if rig.name == "minecraft:item" {
                emitter = brightest_emitter(
                    emitter,
                    self.entity_metadata
                        .get(&eid)
                        .and_then(|meta| meta.item_stack.as_ref())
                        .and_then(|stack| self.held_item_emitter(stack, submerged)),
                );
            }
            // Vanilla equipment slots 0/1 = mainhand/offhand — remote
            // players and item-carrying mobs glow with what they hold.
            for slot in 0u8..=1 {
                emitter = brightest_emitter(
                    emitter,
                    self.entity_equipment
                        .get(&(eid, slot))
                        .and_then(|stack| self.held_item_emitter(stack, submerged)),
                );
            }
            if let Some(emitter) = emitter {
                // Golden-ratio phase decorrelates the flicker between
                // nearby flames so they never pulse in lockstep.
                push(center, emitter, eid as f32 * 0.618_034);
            }
        }
        // Placed luminous blocks near the camera: every cell reporting
        // `facts.luminance > 0` (vanilla or modded — the registry name
        // feeds the universal token inference) plus powered-looking
        // signal components, resolved through the shared block emitter
        // resolver so modded lamps/conduits glow without a mod table.
        candidates.extend(self.placed_block_light_candidates(camera, tick_time_sec));
        candidates.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
        candidates.truncate(MAX_DYNAMIC_LIGHTS);
        candidates.into_iter().map(|(_, light)| light).collect()
    }

    /// Resolve a held/dropped stack into a universal light emitter:
    /// the item's declared luminance comes from the block registry when
    /// the stack is a placeable light block (modded blocks included),
    /// falling back to [`VANILLA_ITEM_LIGHT_LEVELS`] for item-only
    /// emitters and to the resolver's generic vocabulary for modded
    /// non-block items. Quenched open flames report `None`.
    fn held_item_emitter(
        &self,
        stack: &ItemStackComponents,
        submerged: bool,
    ) -> Option<ResolvedLightEmitter> {
        if stack.item_id <= 0 || stack.count <= 0 {
            return None;
        }
        let name = crate::recipe_ingest::item_name_for_id(stack.item_id as u32);
        let declared = self.declared_item_light_level(&name);
        resolve_item_light_emitter(&name, declared, None, submerged)
            .filter(|emitter| !emitter.quenches_underwater)
    }

    /// Declared light level for a held/dropped item id: the live block
    /// registry's own luminance when it knows the name (covers every
    /// modded block item), then [`vanilla_item_light_level`].
    fn declared_item_light_level(&self, item_name: &str) -> Option<u8> {
        if let Some(registry) = self.world.registry.as_ref() {
            if let Some(level) = registry
                .default_state(item_name)
                .ok()
                .and_then(|state| registry.facts(state).ok())
                .map(|facts| facts.luminance)
                .filter(|level| *level > 0)
            {
                return Some(level);
            }
        }
        vanilla_item_light_level(item_name)
    }

    /// The 8-block Manhattan-free box scan of placed luminous blocks,
    /// served from [`PlacedLightScanCache`]: while the camera stays in
    /// the same block and no overlapping section/block mutation lands,
    /// the frame's only work is re-modulating cached emitters — zero
    /// `get_block_state` palette reads, zero registry lookups, zero
    /// resolver calls. A miss re-scans section-by-section over the
    /// (always exactly eight) overlapping sections: uniform/all-air
    /// sections collapse to a single state evaluation and every
    /// repeated state resolves once through a per-scan memo, so even
    /// the miss path never re-runs `is_always_dark_signal_block` per
    /// cell. Torch families anchor at the exact
    /// [`torch_flame_world_offset`] flame tip (wall torches infer their
    /// mount side from solid neighbors); every other emitter anchors at
    /// the resolver's block-local emission origin.
    fn placed_block_light_candidates(
        &self,
        camera: [f32; 3],
        tick_time_sec: f32,
    ) -> Vec<(f32, megu_client_mesh::vfx_engine::DynamicColoredLight)> {
        let mut out = Vec::new();
        if self.world.registry.is_none() {
            return out;
        }
        let center = [
            camera[0].floor() as i32,
            camera[1].floor() as i32,
            camera[2].floor() as i32,
        ];
        let key = self.placed_light_scan_key(center);
        let mut cache = self.placed_light_scan.borrow_mut();
        if cache.valid && cache.key == key {
            cache.hits += 1;
            push_cached_placed_lights(&cache.entries, camera, tick_time_sec, &mut out);
            return out;
        }
        cache.misses += 1;
        let entries = self.scan_placed_lights(center);
        push_cached_placed_lights(&entries, camera, tick_time_sec, &mut out);
        cache.key = key;
        cache.entries = entries;
        cache.valid = true;
        out
    }

    /// `(hits, misses)` for the placed-block light scan cache — proves
    /// the steady-state frame path serves from
    /// [`PlacedLightScanCache`] without touching the world.
    pub fn placed_light_cache_stats(&self) -> (u64, u64) {
        let cache = self.placed_light_scan.borrow();
        (cache.hits, cache.misses)
    }

    /// Build the [`PlacedLightScanKey`] for `center`: the registry
    /// identity, the synchronous world mutation counters and the eight
    /// overlapping sections' mesh revisions.
    fn placed_light_scan_key(&self, center: [i32; 3]) -> PlacedLightScanKey {
        let (slot_ptr, name_ptr, state_count) =
            self.world.registry.as_ref().map_or((0, 0, 0), |registry| {
                (
                    registry as *const megu_kernel::Registry as usize,
                    registry
                        .state_name(0)
                        .ok()
                        .map_or(0usize, |name| name.as_ptr() as usize),
                    registry.state_count(),
                )
            });
        let mut section_revs = [((0, 0, 0), None); 8];
        let mut slot = 0usize;
        for (sy, _) in placed_scan_axis(center[1]) {
            for (sz, _) in placed_scan_axis(center[2]) {
                for (sx, _) in placed_scan_axis(center[0]) {
                    section_revs[slot] = (
                        (sx, sy, sz),
                        self.world.section_mesh_revision(SectionCoord(sx, sy, sz)),
                    );
                    slot += 1;
                }
            }
        }
        PlacedLightScanKey {
            center_block: center,
            registry_slot_ptr: slot_ptr,
            registry_name_ptr: name_ptr,
            registry_state_count: state_count,
            world_state_count: self.world.state_count,
            block_updates: self.world.block_updates,
            chunks_decoded: self.world.chunks_decoded,
            chunks_unloaded: self.world.chunks_unloaded,
            loaded_columns: self.world.columns.len(),
            section_revs,
        }
    }

    /// Re-scan the ±8 box around `center` into frozen
    /// [`CachedPlacedLight`] entries. Iterates sections in `sy/sz/sx`
    /// then cells `ly/lz/lx` order — the same global y-then-z-then-x
    /// sequence the uncached triple loop produced, so candidate order
    /// (and therefore the stable nearest-first sort's tie-breaking in
    /// [`Self::collect_dynamic_lights`]) is bit-identical.
    fn scan_placed_lights(&self, center: [i32; 3]) -> Vec<CachedPlacedLight> {
        let mut entries = Vec::new();
        let Some(registry) = self.world.registry.as_ref() else {
            return entries;
        };
        let [cx, cy, cz] = center;
        // `min_y`/`height` are server-controlled i32s; widen before
        // adding so hostile values cannot overflow.
        let min_y = i64::from(self.world.dimension.min_y);
        let max_y = min_y + i64::from(self.world.dimension.height);
        let min_sy = self.world.dimension.min_section_y();
        let max_sy = min_sy + self.world.dimension.section_count() as i32;
        let mut evals: HashMap<u32, Option<PlacedStateEval>> = HashMap::new();
        for (sy, ys) in placed_scan_axis(cy) {
            for (sz, zs) in placed_scan_axis(cz) {
                for (sx, xs) in placed_scan_axis(cx) {
                    let source = if sy < min_sy || sy >= max_sy {
                        // Wholly outside the dimension's height window:
                        // `get_block_state` answers `Some(0)` for every
                        // cell before any column lookup.
                        PlacedSectionStates::Uniform(0)
                    } else {
                        match self
                            .world
                            .columns
                            .get(&(sx, sz))
                            .and_then(|col| col.sections.get((sy - min_sy) as usize))
                        {
                            Some(sec) => match sec.palette.uniform_state() {
                                Some(state) => PlacedSectionStates::Uniform(state),
                                None => PlacedSectionStates::Dense(&sec.palette),
                            },
                            None => PlacedSectionStates::Skip,
                        }
                    };
                    let mut dense = [0u32; SECTION_CELLS];
                    match source {
                        PlacedSectionStates::Skip => continue,
                        PlacedSectionStates::Uniform(_) => {}
                        PlacedSectionStates::Dense(palette) => {
                            palette.unpack_into(&mut dense);
                        }
                    }
                    for ly in ys.clone() {
                        let y = sy.wrapping_mul(16).wrapping_add(ly);
                        // Mirrors `get_block_state`'s y-window check:
                        // inside it a missing section reads `None`
                        // (skipped upstream), outside it reads the
                        // `Some(0)` air sentinel.
                        let in_dim = i64::from(y) >= min_y && i64::from(y) < max_y;
                        for lz in zs.clone() {
                            let z = sz.wrapping_mul(16).wrapping_add(lz);
                            for lx in xs.clone() {
                                let x = sx.wrapping_mul(16).wrapping_add(lx);
                                let state = if in_dim {
                                    match source {
                                        PlacedSectionStates::Uniform(state) => state,
                                        PlacedSectionStates::Dense(_) => {
                                            dense[section_index(
                                                lx as usize,
                                                ly as usize,
                                                lz as usize,
                                            )]
                                        }
                                        PlacedSectionStates::Skip => unreachable!(),
                                    }
                                } else {
                                    0
                                };
                                let eval = evals
                                    .entry(state)
                                    .or_insert_with(|| placed_state_eval(state, registry));
                                let Some(eval) = eval else {
                                    continue;
                                };
                                let eval = *eval;
                                let mut emitter = eval.emitter;
                                // Torch flames burn at the tip, not the
                                // block origin — the wall mount is a
                                // per-cell neighbor query, so only the
                                // base emitter is memoized.
                                if eval.torch {
                                    let wall_facing = if eval.wall_torch {
                                        self.torch_wall_facing(x, y, z, registry)
                                    } else {
                                        None
                                    };
                                    emitter.local_origin = torch_flame_world_offset(wall_facing);
                                }
                                let pos = [
                                    x as f32 + emitter.local_origin[0],
                                    y as f32 + emitter.local_origin[1],
                                    z as f32 + emitter.local_origin[2],
                                ];
                                // Position-keyed phase keeps neighbours
                                // out of sync.
                                let phase =
                                    (x.wrapping_mul(31) ^ y.wrapping_mul(57) ^ z.wrapping_mul(13))
                                        as f32
                                        * 0.618_034;
                                entries.push(CachedPlacedLight {
                                    pos,
                                    emitter,
                                    phase,
                                });
                            }
                        }
                    }
                }
            }
        }
        entries
    }

    /// Wall-mount inference for `*_wall_torch` cells: the first solid
    /// horizontal neighbour is the wall, and the facing code handed to
    /// [`torch_flame_world_offset`] points the opposite way (`0` north,
    /// `1` south, `2` west, `3` east — the wall torch leans away from
    /// its mount).
    fn torch_wall_facing(
        &self,
        x: i32,
        y: i32,
        z: i32,
        registry: &megu_kernel::Registry,
    ) -> Option<u8> {
        // (offset, facing): wall at +x -> faces west(2), -x -> east(3),
        // +z -> north(0), -z -> south(1).
        const MOUNTS: [([i32; 3], u8); 4] = [
            ([1, 0, 0], 2),
            ([-1, 0, 0], 3),
            ([0, 0, 1], 0),
            ([0, 0, -1], 1),
        ];
        MOUNTS.iter().find_map(|(offset, facing)| {
            let solid = self
                .world
                .get_block_state(x + offset[0], y + offset[1], z + offset[2])
                .and_then(|state| registry.facts(state).ok())
                .is_some_and(|facts| facts.blocks_motion != 0 || facts.is_solid_render != 0);
            solid.then_some(*facing)
        })
    }

    /// This frame's grounded contact-shadow quads, one beneath each
    /// non-item mob/creature rig close enough to a collision surface —
    /// a soft dark quad at `floor_top + 0.015` sized `rig.width * 0.85`
    /// with the `[0, 1, 0]` normal, whose alpha fades linearly to zero
    /// at [`CONTACT_SHADOW_FADE_BLOCKS`] so jumping mobs shed their
    /// shadow smoothly and airborne entities (`> 2.5` blocks up or no
    /// floor inside the probe depth) emit nothing. It complements the
    /// sun/moon cascade pass so mobs stay grounded indoors, under
    /// clouds and on `Shadows: Off/Feather`. Empty while the
    /// `entity_shadows` gate is off — the Video tab `Shadows` toggle
    /// owns it at the call site, never World Effects.
    pub fn entity_contact_shadow_quads(&self) -> Vec<EntityContactShadowQuad> {
        let mut out = Vec::new();
        for (eid, rig, pos, _tracked) in self.entity_render_entries() {
            // Item drops, projectiles and vehicles/boats/frame hooks
            // stay shadow-free — only standing creature rigs ground.
            if matches!(
                rig.archetype,
                EntityRigArchetype::ProjectileOrItem | EntityRigArchetype::VehicleOrHanging
            ) {
                continue;
            }
            let half = rig.width * 0.85 * 0.5;
            if !half.is_finite() || half <= 0.0 || out.len() >= MAX_CONTACT_SHADOW_QUADS {
                continue;
            }
            // Probe for the highest collision surface under the feet:
            // the sweep spans a slim column from `PROBE_DEPTH` below up
            // to the soles so slabs, stairs and farmland report their
            // exact top instead of a full-block guess.
            let floor_top = self
                .world
                .collect_collision_aabbs(Aabb {
                    min: [
                        pos[0] - f64::from(half),
                        pos[1] - CONTACT_SHADOW_PROBE_DEPTH,
                        pos[2] - f64::from(half),
                    ],
                    max: [
                        pos[0] + f64::from(half),
                        pos[1] + 0.001,
                        pos[2] + f64::from(half),
                    ],
                })
                .iter()
                .map(|aabb| aabb.max[1])
                .fold(f64::NEG_INFINITY, f64::max);
            if !floor_top.is_finite() {
                continue;
            }
            let height = (pos[1] - floor_top).max(0.0) as f32;
            if height > CONTACT_SHADOW_FADE_BLOCKS {
                continue;
            }
            let alpha = 0.30 * (1.0 - height / CONTACT_SHADOW_FADE_BLOCKS);
            if alpha <= 0.001 {
                continue;
            }
            let (cx, y, cz) = (pos[0] as f32, (floor_top + 0.015) as f32, pos[2] as f32);
            out.push(EntityContactShadowQuad {
                entity_id: eid,
                positions: [
                    [cx - half, y, cz - half],
                    [cx + half, y, cz - half],
                    [cx + half, y, cz + half],
                    [cx - half, y, cz + half],
                ],
                normal: [0.0, 1.0, 0.0],
                rgba: [0.0, 0.0, 0.0, alpha],
                height_above_floor: height,
            });
        }
        out
    }

    /// The session's glint shimmer clock in seconds — the wall-clock
    /// `atlas_anim_clock` the texture animations run on, so the 3D glint
    /// keeps scrolling even while server time stands still.
    fn glint_time_seconds(&self) -> f32 {
        self.atlas_anim_clock.max(0.0) as f32
    }

    /// `base` quad tint with the scrolling violet-lavender enchantment
    /// glint blended in for an enchanted stack — the 3D counterpart of
    /// the 2D icon shimmer [`enchantment_glint_rgb`] evaluates. The
    /// glint peaks under ~0.32 so enchanted items shimmer without
    /// washing out their colors; non-enchanted stacks pass `base`
    /// through untouched.
    pub fn enchanted_item_quad_tint(
        &self,
        stack: &ItemStackComponents,
        base: [f32; 3],
        uv: [f32; 2],
    ) -> [f32; 3] {
        if !item_stack_has_glint(stack) {
            return base;
        }
        let glint = enchantment_glint_rgb(uv, self.glint_time_seconds());
        [
            (base[0] + glint[0]).clamp(0.0, 1.0),
            (base[1] + glint[1]).clamp(0.0, 1.0),
            (base[2] + glint[2]).clamp(0.0, 1.0),
        ]
    }

    /// `true` when a tracked entity should carry the glint on its 3D
    /// item quads — a dropped enchanted stack (item-entity metadata
    /// index 8) or any enchanted equipment piece in the vanilla hand /
    /// armor slots `0..=5`.
    pub fn entity_enchanted_item_glint(&self, eid: i32) -> bool {
        if self
            .entity_metadata
            .get(&eid)
            .and_then(|meta| meta.item_stack.as_ref())
            .is_some_and(item_stack_has_glint)
        {
            return true;
        }
        (0u8..=5).any(|slot| {
            self.entity_equipment
                .get(&(eid, slot))
                .is_some_and(item_stack_has_glint)
        })
    }

    /// Drain the sections whose meshes finished VRAM upload and release
    /// their CPU copies — keeps `WorldState::packed_mesh_count()` at the
    /// working-set bound (V2) instead of growing with residency. The same
    /// drain drops session-side `ExternalSectionMesh` payloads and
    /// records released world coords so a renderer recreation can
    /// requeue them for re-meshing.
    pub fn release_uploaded_cpu_copies(&mut self, renderer: &mut GpuRenderer) {
        for coord in renderer.take_uploaded_section_coords() {
            self.world.release_uploaded_section_cpu_copies(coord);
            if self.world.released_mesh_meta(coord).is_some() {
                self.cpu_released_world_coords.insert(coord);
            } else {
                self.cpu_released_world_coords.remove(&coord);
            }
            if let Some(external) = self.external_sections.get_mut(&coord) {
                external.release_cpu_payload();
            }
        }
    }

    /// A fresh `GpuRenderer` (device loss / surface re-create) holds none
    /// of the VRAM copies: every section whose CPU payload was released
    /// post-upload must be re-meshed from chunk data. Re-dirty the
    /// world-side released coords through `MeshCache` (which makes
    /// `mesh()` report `None` again, so the mesh lanes treat the section
    /// as unmeshed) and drop released `ExternalSectionMesh` payloads so
    /// the lane rebuilds them instead of mirroring empty buffers.
    /// Returns the number of sections requeued.
    pub fn requeue_cpu_released_sections(&mut self) -> usize {
        let mut requeued = 0usize;
        for coord in std::mem::take(&mut self.cpu_released_world_coords) {
            if self.world.mesh_cache.mark_dirty(coord) {
                self.mark_offthread_coord(coord);
                requeued += 1;
            }
        }
        let released_external: Vec<SectionCoord> = self
            .external_sections
            .iter()
            .filter(|(_, external)| external.cpu_released)
            .map(|(coord, _)| *coord)
            .collect();
        for coord in released_external {
            self.external_sections.remove(&coord);
            self.mark_external_removed(coord);
            if self.world.mesh_cache.mark_dirty(coord) {
                self.mark_offthread_coord(coord);
                requeued += 1;
            }
        }
        if requeued > 0 {
            self.terrain_dirty = true;
        }
        requeued
    }

    /// Rebuild currently loaded section meshes after the vertex-AO policy
    /// changes. The renderer consumes the same setting while staging the
    /// replacement buffers, so old GPU vertices cannot retain stale AO.
    pub fn rebuild_loaded_sections_for_lighting(
        &mut self,
        camera_position: [f32; 3],
        view_distance_chunks: u8,
    ) -> usize {
        let center_x = (camera_position[0] / 16.0).floor() as i32;
        let center_z = (camera_position[2] / 16.0).floor() as i32;
        let radius = i32::from(view_distance_chunks.clamp(6, 32));
        let min_y = self.world.dimension.min_section_y();
        let end_y = min_y + self.world.dimension.section_count() as i32;
        let mut dirtied = 0;
        for cx in center_x - radius..=center_x + radius {
            for cz in center_z - radius..=center_z + radius {
                for sy in min_y..end_y {
                    let coord = SectionCoord(cx, sy, cz);
                    if self.world.mesh_cache.section(coord).is_some()
                        && self.world.mesh_cache.mark_dirty(coord)
                    {
                        self.mark_offthread_coord(coord);
                        dirtied += 1;
                    }
                }
            }
        }
        if dirtied > 0 {
            self.terrain_dirty = true;
        }
        dirtied
    }

    /// Install a new spatial-audio quality tier on the caller-side mixer,
    /// preserving the live acoustic-world binding (`Off` is the bit-exact
    /// vanilla passthrough, `Realistic`/`Ultra` the ray-traced tiers).
    /// With the dedicated real-time mix thread running, the caller-side
    /// mixer is a parked fallback — the thread's DSP keeps the preset it
    /// was spawned with.
    pub fn set_spatial_audio_preset(&mut self, preset: SpatialAudioPreset) {
        let mut mixer =
            megu_client::session::SoundMixer::new(self.audio.mixer.sample_rate(), preset);
        mixer.set_world(Box::new(self.acoustic_world.clone()));
        self.audio.mixer = mixer;
    }

    /// Hand the audio sink to the dedicated real-time `AudioMixThread`
    /// and return the lock-free command ring. Windowed sessions call this
    /// once at startup; headless/unit-test sessions skip it and keep the
    /// synchronous `pump()` path.
    pub fn enable_dedicated_audio_mix(&mut self) -> std::sync::Arc<AudioCmdRing> {
        self.audio.enable_dedicated_mix_thread()
    }

    /// `true` once [`Self::enable_dedicated_audio_mix`] handed the sink
    /// to the real-time mix thread.
    pub fn dedicated_audio_mix(&self) -> bool {
        self.audio.mix_thread().is_some()
    }

    /// O(1) lock-free audio command submission for the render thread —
    /// `false` when no dedicated mix thread runs (callers keep the
    /// synchronous `ingest`/`pump` path).
    pub fn submit_audio_cmd_nonblocking(&self, cmd: AudioCmd) -> bool {
        self.audio.submit_cmd_nonblocking(cmd)
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
        self.emit_player_sound(
            if event.is_lava {
                "minecraft:entity.generic.extinguish_fire"
            } else {
                "minecraft:entity.player.splash"
            },
            0.8,
            1.0,
        );
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
        let sound = match surface {
            GroundSurfaceKind::Sand => "minecraft:block.sand.step",
            GroundSurfaceKind::Snow => "minecraft:block.snow.step",
            GroundSurfaceKind::Gravel => "minecraft:block.gravel.step",
            GroundSurfaceKind::MudOrShallowWater => "minecraft:block.mud.step",
            GroundSurfaceKind::FallenLeaves => "minecraft:block.azalea_leaves.step",
            GroundSurfaceKind::StoneOrWood => "minecraft:block.stone.step",
        };
        let pos = [pos[0], pos[1], pos[2]];
        self.emit_effect_sound(
            sound,
            4,
            if landing_speed > 0.0 { 0.8 } else { 0.35 },
            0.8,
            pos,
        );
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
        // Wetness tracking only feeds drip particles: skip it (and the
        // particle-aging it performs) once the tactile preset is off and
        // no particles remain to drain — camera/audio evaluation below
        // is independent and keeps running.
        if self.tactile_sim.config().preset.is_active() || !self.tactile_sim.is_empty() {
            if let Some(pose) = self.pose {
                self.tactile_sim.wetness_mut().set_position([
                    pose.feet[0] as f32,
                    pose.feet[1] as f32,
                    pose.feet[2] as f32,
                ]);
            }
            self.tactile_sim
                .step_with_wetness(dt_sec, is_submerged || is_raining);
        }

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

    /// Limit main-thread packet work per frame. No reads, writes or DNS on
    /// this thread. `frustum_planes` are the six view planes in
    /// `box_in_frustum` form (`dot(n, p) + d >= 0` inside); they order the
    /// dirty-mesh drain so visible nearby sections mesh first — an empty
    /// slice degrades to pure distance ordering.
    pub fn poll(&mut self, camera_position: [f32; 3], frustum_planes: &[[f32; 4]]) {
        if matches!(self.status, SessionStatus::Failed(_)) {
            return;
        }
        // Arm the adaptive dispatch decisions before any timed work this
        // poll: the D-UCB bandit picks the background drain profile it
        // will be scored on, and the probe cadence may dispatch the
        // challenger kernel mode so `observe_sample` measures a real
        // challenger timing — not another sample of the incumbent.
        self.ducb_arm_selected = self.ducb.select_arm();
        let kernel_mode = self.dispatch_kernel_mode();
        for _ in 0..32 {
            match self.incoming.try_recv() {
                Ok(WorkerMessage::Connected) => self.status = SessionStatus::Playing,
                Ok(WorkerMessage::Play(event)) => {
                    if let Err(error) = self.apply_event(event) {
                        self.status = SessionStatus::Failed(format!("play event: {error}"));
                        return;
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
        if let Some(received_at) = self.subworld_sync_received_at {
            let alpha = (received_at.elapsed().as_secs_f32() / 0.05).clamp(0.0, 1.0);
            self.subworlds.interpolate_poses(alpha);
            if let Some(first_id) = self.subworlds.entries().next().map(|entry| entry.id) {
                let _ = self.live_contraption_interpolated_transform(first_id, alpha);
            }
        }
        // Synchronous pump only when no dedicated mix thread owns the
        // sink — `pump` would write to the null sink otherwise.
        if self.audio.mix_thread().is_none() {
            self.audio.pump();
        }
        // Preserve the last received terrain even if the peer closes immediately
        // after sending chunk data.
        let had_pending = self.world.pending_mesh_sections() > 0;
        // A committed translucent re-sort can rewrite resident orders far
        // outside the event-marked dirty set — the renderer runs its cheap
        // order sweep on this sync instead of a full section scan.
        if self
            .world
            .mesh_cache
            .translucent_resort_due(camera_position)
        {
            self.terrain_order_sweep = true;
        }
        // Off-thread SectionMesh completions from previous polls: accepted
        // results publish into `external_sections`, stale ones (the
        // section re-dirtied after its snapshot) drop.
        self.drain_offthread_sections(camera_position);
        // Priority-0 lane: a degenerate all-inclusive frustum plane set
        // collapses the ordered rebuild's queue key to pure distance, so
        // the nearest one or two dirty sections rebuild inline — and get
        // their world artifacts derived — this same frame, even when a
        // block edit landed behind the camera.
        match self.world.rebuild_dirty_bounded_ordered(
            PRIORITY_ZERO_INLINE_MAX,
            camera_position,
            &ALL_INCLUSIVE_PLANES,
        ) {
            Ok(rebuilt) => self.priority_zero_meshes += rebuilt as u64,
            Err(error) if !matches!(self.status, SessionStatus::Disconnected(_)) => {
                self.status = SessionStatus::Failed(format!("mesh: {error}"));
                return;
            }
            Err(_) => {}
        }
        let rebuild_start = Instant::now();
        match self.world.rebuild_dirty_bounded_ordered(
            self.mesh_rebuild_budget(),
            camera_position,
            frustum_planes,
        ) {
            Ok(rebuilt) => {
                self.terrain_dirty |= rebuilt != 0 || had_pending;
                self.ordered_rebuilds_count += rebuilt as u64;
                if rebuilt != 0 {
                    // Feed the measured per-batch mesh-build duration
                    // into the kernel autotuner — a real timed edge, not
                    // a synthetic sample (`max(1)` guards a sub-ns clock
                    // so `observe_sample` never discards it as empty).
                    // The sample credits the mode the poll actually
                    // dispatched under — incumbent or probed challenger —
                    // and the D-UCB arm whose drain profile ran it.
                    let elapsed_ns =
                        (rebuild_start.elapsed().as_nanos().min(u64::MAX as u128) as u64).max(1);
                    self.kernel_autotuner
                        .observe_sample(kernel_mode, elapsed_ns);
                    self.ducb.observe(self.ducb_arm_selected, elapsed_ns);
                    self.ducb_sample_count += 1;
                    self.autotuner_sample_count += 1;
                }
            }
            Err(error) if !matches!(self.status, SessionStatus::Disconnected(_)) => {
                self.status = SessionStatus::Failed(format!("mesh: {error}"));
                return;
            }
            Err(_) => {}
        }
        // Bulk/streaming tail: sections still dirty after the inline
        // budget get `Arc`-snapshotted into `MeshJob::SectionMesh`
        // workers — bounded non-blocking submissions, so a saturated
        // queue simply leaves the rest to the inline lane next poll.
        self.submit_offthread_sections(camera_position, frustum_planes);
        if !matches!(self.status, SessionStatus::Disconnected(_)) {
            let now = Instant::now();
            let elapsed_ticks = now.duration_since(self.last_vfx_tick).as_millis() as u64 / 50;
            // Preserve sub-tick time at high frame rates. Cap expensive sim
            // catch-up separately from the elapsed combat clock.
            self.last_vfx_tick += Duration::from_millis(elapsed_ticks * 50);
            self.nature_env.origin = camera_position;
            // Idle-sim gating: with the ambience/tactile presets off and no
            // live particles left to age out, the 20 Hz sim steps do nothing
            // observable — skip them so idle sessions burn zero CPU. While a
            // preset is on, or particles are still draining, the step runs
            // every tick exactly as before.
            let ambience_active = (self.vanilla_plus_cfg.nature.enabled
                && self.vanilla_plus_cfg.nature.nature_ambience)
                || !self.nature_ambience.particles().is_empty();
            let tactile_active =
                self.tactile_sim.config().preset.is_active() || !self.tactile_sim.is_empty();
            for _ in 0..elapsed_ticks.min(4) {
                // Time each client sim-tick step so the Compact Debug HUD's MSPT/TPS
                // segments get a live reading — `FrameTimingStats::push_tick`
                // is the only producer of the tick ring it samples.
                let tick_start = Instant::now();
                self.vfx.tick();
                if ambience_active {
                    self.step_nature_ambience(0.05);
                }
                if tactile_active {
                    self.tactile_sim.step(0.05);
                }
                self.world.tick_entities();
                self.frame_timings.push_tick(tick_start.elapsed());
            }
            self.world
                .combat
                .advance_ticks(elapsed_ticks.saturating_sub(4).min(u64::from(u32::MAX)) as u32);
            self.sync_hud_combat();
            let animation_dt = now.duration_since(self.last_animation_tick).as_secs_f32();
            self.last_animation_tick = now;
            if let Err(error) = self.world.advance_animations(animation_dt.min(0.1)) {
                self.status = SessionStatus::Failed(format!("animation: {error:?}"));
            }
            self.atlas_anim_clock += now.duration_since(self.last_atlas_anim_tick).as_secs_f64();
            self.last_atlas_anim_tick = now;
        }
    }

    /// Record an `external_sections` insert/replace for the renderer
    /// delta stream and bump the revision the sync gates on.
    fn mark_external_upsert(&mut self, coord: SectionCoord) {
        self.external_sections_rev = self.external_sections_rev.wrapping_add(1);
        self.external_section_delta.removals.remove(&coord);
        self.external_section_delta.upserts.insert(coord);
    }

    /// Record an `external_sections` removal — latest write wins, so a
    /// previously-queued upsert for the same coord is superseded.
    fn mark_external_removed(&mut self, coord: SectionCoord) {
        self.external_sections_rev = self.external_sections_rev.wrapping_add(1);
        self.external_section_delta.upserts.remove(&coord);
        self.external_section_delta.removals.insert(coord);
    }

    /// Record a wholesale `external_sections` clear — supersedes every
    /// queued per-coord change before it.
    fn mark_external_cleared(&mut self) {
        self.external_sections_rev = self.external_sections_rev.wrapping_add(1);
        self.external_section_delta.upserts.clear();
        self.external_section_delta.removals.clear();
        self.external_section_delta.cleared = true;
    }

    /// Insert or replace an external section payload, recording the
    /// renderer-bound delta in the same step — the single write path
    /// every producer (worker drain, priority-0 lane, tests) takes.
    pub fn insert_external_section(
        &mut self,
        coord: SectionCoord,
        mesh: ExternalSectionMesh,
    ) -> Option<ExternalSectionMesh> {
        self.mark_external_upsert(coord);
        self.external_sections.insert(coord, mesh)
    }

    /// Remove an external section payload, recording the renderer-bound
    /// removal only when an entry actually existed.
    pub fn remove_external_section(&mut self, coord: SectionCoord) -> Option<ExternalSectionMesh> {
        let removed = self.external_sections.remove(&coord);
        if self.baked_sections.remove(&coord).is_some() {
            self.shaped_terrain_revision = self.shaped_terrain_revision.wrapping_add(1);
        }
        if removed.is_some() {
            self.mark_external_removed(coord);
        }
        removed
    }

    /// Drop every external section payload and mark the delta stream
    /// cleared — dimension resets route here so the renderer mirror
    /// wipes in the same order the map did.
    pub fn clear_external_sections(&mut self) {
        self.external_sections.clear();
        if !self.baked_sections.is_empty() {
            self.baked_sections.clear();
            self.shaped_terrain_revision = self.shaped_terrain_revision.wrapping_add(1);
        }
        self.mark_external_cleared();
    }

    /// Drain the accumulated renderer-bound delta — `None` when nothing
    /// changed since the last drain (the steady-frame case the renderer
    /// short-circuits on: no clone, no walk, no alloc).
    pub fn take_external_sections_delta(&mut self) -> Option<ExternalSectionsDelta> {
        if self.external_section_delta.is_empty() {
            return None;
        }
        Some(std::mem::take(&mut self.external_section_delta))
    }

    /// Drop the pending delta stream — the caller mirrored the full map
    /// instead (fresh renderer attach), so replaying stale per-coord
    /// changes afterwards would be a no-op anyway.
    pub fn discard_external_sections_delta(&mut self) {
        self.external_section_delta = ExternalSectionsDelta::default();
    }

    /// Prune [`Self::external_sections`]: an entry stays authoritative
    /// only while its section is still loaded, no newer dirty mark
    /// landed since the payload was produced (`content_epoch`), and the
    /// world has not derived its own artifacts since (`world_revision`).
    /// Dropped entries hand residency back to the world payload — the
    /// byte-identical priority-0 payloads dedup through
    /// `PendingSectionUpload::matches` without a re-upload.
    fn prune_external_sections(&mut self) {
        let world = &self.world;
        let lane = &self.section_mesh_lane;
        let mut pruned = Vec::new();
        self.external_sections.retain(|coord, external| {
            let keep = world.mesh_cache.section(*coord).is_some()
                && lane.epoch_of(*coord) == external.content_epoch
                && world.section_mesh_revision(*coord).unwrap_or(0) == external.world_revision;
            if !keep {
                pruned.push(*coord);
            }
            keep
        });
        for coord in pruned {
            // `retain` already dropped the entry — record the removal.
            if self.baked_sections.remove(&coord).is_some() {
                self.shaped_terrain_revision = self.shaped_terrain_revision.wrapping_add(1);
            }
            self.mark_external_removed(coord);
        }
    }

    /// Drain `MeshJob::SectionMesh` completions without blocking: each
    /// accepted result (still the in-flight submission, content epoch
    /// current, world payload not yet published) becomes an
    /// [`ExternalSectionMesh`] the next renderer reconcile uploads.
    /// Anything else — re-dirtied sections, superseded submissions,
    /// raced inline rebuilds, unloads — is dropped as stale.
    fn drain_offthread_sections(&mut self, camera_position: [f32; 3]) {
        self.prune_external_sections();
        let mut results = Vec::new();
        let mut baked_results: HashMap<SectionCoord, BakedSectionMeshResult> = HashMap::new();
        if let Some(workers) = &self.section_mesh_workers {
            workers.drain(|result| results.push(result));
            workers.drain_baked(|result| {
                baked_results.insert(result.coord, result);
            });
        }
        for result in results {
            let MeshResult::SectionMesh { built } = result else {
                continue;
            };
            if !self.section_mesh_lane.accept(&built) {
                self.offthread_mesh_stale += 1;
                continue;
            }
            // Installed or not, this snapshot is consumed: only a later
            // dirty mark makes the coord a submission candidate again.
            self.offthread_candidates.remove(&built.coord);
            // An inline rebuild or an unload raced the worker — the
            // authoritative paths already covered the coord.
            if self.world.packed_mesh(built.coord).is_some()
                || self.world.mesh_cache.section(built.coord).is_none()
            {
                continue;
            }
            self.offthread_mesh_completed += 1;
            let baked = baked_results
                .remove(&built.coord)
                .filter(|result| result.revision == built.revision)
                .map(|result| result.faces);
            self.install_built_external(built, camera_position, baked);
        }
    }

    /// Snapshot still-dirty candidate sections beyond this poll's inline
    /// reach and submit bounded `MeshJob::SectionMesh` work. Ordering
    /// mirrors `MeshCache::rebuild_dirty_ordered` (in-frustum first, then
    /// nearest) so the most valuable tail lands earliest; `try_send`
    /// backpressure leaves saturated leftovers to the inline lane.
    fn submit_offthread_sections(
        &mut self,
        camera_position: [f32; 3],
        frustum_planes: &[[f32; 4]],
    ) {
        if self.offthread_candidates.is_empty() {
            return;
        }
        // Drop coords already meshed inline (priority-0 or the budgeted
        // drain) or unloaded; what remains mirrors the cache's dirty set.
        let world = &self.world;
        self.offthread_candidates.retain(|coord| {
            world.mesh_cache.section(*coord).is_some() && world.mesh_cache.mesh(*coord).is_none()
        });
        if self.offthread_candidates.is_empty() {
            return;
        }
        let threads = self.compute_strategy.workers.background_threads;
        let mut order: Vec<SectionCoord> = self.offthread_candidates.iter().copied().collect();
        // Speculative prefetcher consumption: pending forward-cone bets
        // subtract a bounded, rank-scaled distance from the queue key so
        // sections along the predicted heading drain earlier. Stale
        // (`CanceledStale`) generations and out-of-cone sections get
        // nothing — the sort stays `(in-frustum, distance)` otherwise.
        let prefetch_boosts = self.prefetch_boost_map();
        order.sort_by_key(|&coord| {
            let (outside, dist2_bits) = offthread_queue_key(coord, camera_position, frustum_planes);
            let dist2 = f32::from_bits(dist2_bits as u32);
            let boost = prefetch_boosts
                .get(&(coord.0, coord.2))
                .copied()
                .unwrap_or(0.0);
            let boosted = (dist2.sqrt() - boost).max(0.0);
            (outside, (boosted * boosted).to_bits() as u64)
        });
        // The D-UCB bandit's selected drain profile bounds this poll's
        // submissions: conservative through burst, all bounded by
        // `ducb_submit_cap` so a saturated queue still leaves the tail
        // to the inline lane next poll.
        let submit_cap = ducb_submit_cap(self.ducb_arm_selected);
        let mut submitted = 0usize;
        for coord in order {
            if submitted >= submit_cap {
                break;
            }
            if !self.section_mesh_lane.can_submit(coord) {
                continue;
            }
            let revision = self.section_mesh_lane.pending_revision(coord);
            let Some(input) = self
                .world
                .mesh_cache
                .snapshot_section_input(coord, revision)
            else {
                continue;
            };
            if self.section_mesh_workers.is_none() {
                let mut pool = MeshJobPool::new(threads);
                if let (Some(catalog), Some(sprite_lookup)) = (
                    self.block_model_catalog.clone(),
                    self.block_model_sprite_lookup.clone(),
                ) {
                    pool.set_block_model_catalog_with_sprite_lookup(
                        Some(catalog),
                        Arc::from(self.world.blocks.state_names().to_vec()),
                        Some(sprite_lookup_closure(sprite_lookup)),
                    );
                }
                self.section_mesh_workers = Some(pool);
            }
            let workers = self
                .section_mesh_workers
                .as_mut()
                .expect("worker pool initialized above");
            if workers.try_submit(MeshJob::SectionMesh { input }) {
                debug_assert_eq!(self.section_mesh_lane.note_submitted(coord), revision);
                self.offthread_mesh_submitted += 1;
                submitted += 1;
            }
        }
    }

    /// Publish a worker-built mesh as an [`ExternalSectionMesh`]: the
    /// opaque packed head plus the translucent tail re-packed and sorted
    /// back-to-front — the same stream `section_geometry` consumes from
    /// the world path, with opaque-only direction buckets recovered from
    /// the worker's vertex ranges.
    fn install_built_external(
        &mut self,
        built: BuiltSectionMesh,
        camera_position: [f32; 3],
        baked_faces: Option<Vec<BakedTerrainFace>>,
    ) {
        let coord = built.coord;
        let opaque_quads = built.opaque_packed.len() as u32 / 4;
        let mut packed = built.opaque_packed;
        if let Ok(tail) = pack_vertices(&built.translucent_quads) {
            packed.extend_from_slice(&tail);
        }
        let local_cam = [
            camera_position[0] - (coord.0 * 16) as f32,
            camera_position[1] - (coord.1 * 16) as f32,
            camera_position[2] - (coord.2 * 16) as f32,
        ];
        // The sort returns indices into the translucent tail; offset by
        // the opaque quad count so they index the combined `packed`.
        let order = sort_translucent(&built.translucent_quads, local_cam)
            .map(|sorted| {
                sorted
                    .into_iter()
                    .map(|index| index + opaque_quads)
                    .collect::<Vec<u32>>()
            })
            .unwrap_or_default();
        // `opaque_bucket_ranges` are four-vertex spans over the opaque
        // head — divide by the vertex stride to recover the quad buckets
        // `DirectionBuckets` consumers index with.
        let mut offset = [0u32; DIRECTION_BUCKETS];
        let mut count = [0u32; DIRECTION_BUCKETS];
        for (bucket, range) in built.opaque_bucket_ranges.iter().enumerate() {
            offset[bucket] = range.0 / 4;
            count[bucket] = range.1 / 4;
        }
        let buckets = DirectionBuckets { offset, count };
        let content_epoch = self.section_mesh_lane.epoch_of(coord);
        let digest = external_packed_digest(&packed);
        let order_digest = translucent_order_digest(&order);
        let baked_faces: Arc<[BakedTerrainFace]> =
            baked_faces.map(Arc::from).unwrap_or_else(|| Arc::from([]));
        if !baked_faces.is_empty() {
            self.baked_sections.insert(coord, Arc::clone(&baked_faces));
            self.shaped_terrain_revision = self.shaped_terrain_revision.wrapping_add(1);
        } else if self.baked_sections.remove(&coord).is_some() {
            self.shaped_terrain_revision = self.shaped_terrain_revision.wrapping_add(1);
        }
        self.insert_external_section(
            coord,
            ExternalSectionMesh {
                packed_quads: packed.len() as u32 / 4,
                order_digest,
                order_len: order.len() as u32,
                cpu_released: false,
                packed: packed.into(),
                order: order.into(),
                buckets: Some(buckets),
                digest,
                content_epoch,
                world_revision: self.world.section_mesh_revision(coord).unwrap_or(0),
            },
        );
        self.dirty_terrain_coords.insert(coord);
        self.terrain_dirty = true;
    }

    fn drain_offline_commands(&mut self) {
        if let Some(rx) = &self.offline_command_receiver {
            while rx.try_recv().is_ok() {}
        }
    }

    pub fn send_movement(&mut self, movement: PlayerMovement) {
        if !matches!(self.status, SessionStatus::Playing)
            || self.pose.is_none()
            || self.death.is_dead()
        {
            return;
        }
        self.apply_local_movement(movement);
        self.prev_pose = self.pose;
        self.queue_movement(movement);
    }

    fn apply_local_movement(&mut self, movement: PlayerMovement) {
        if let Some(previous) = self.pose {
            self.last_motion_delta = [
                movement.pose.feet[0] - previous.feet[0],
                movement.pose.feet[2] - previous.feet[2],
            ];
        }
        self.sneaking = movement.sneak;
        self.pose = Some(movement.pose);
        self.update_audio_listener();
        self.refresh_prefetcher();
    }

    fn queue_movement(&mut self, movement: PlayerMovement) {
        self.drain_offline_commands();
        let on_ground = self.physics_state.on_ground;
        if self
            .outgoing
            .try_send(WorkerCommand::Movement {
                movement,
                on_ground,
            })
            .is_ok()
        {
            self.last_movement_on_ground = Some(on_ground);
        }
    }

    /// Advance one frame of server-authoritative survival movement with the
    /// canonical 20 Hz physics step. This is the stable API the window/input
    /// lane drives; creative flight and offline camera motion remain available
    /// through the existing lower-level methods.
    pub fn step_live_survival_movement(
        &mut self,
        dt_secs: f32,
        input: SurvivalMovementInput,
    ) -> Option<LocalPhysicsOutput> {
        self.step_local_player_physics(
            dt_secs,
            input.forward,
            -input.strafe,
            input.jump,
            input.sneak,
            input.sprint,
            input.yaw,
            input.pitch,
        )
    }

    /// Advance local walking or creative flight at a capped 20 Hz simulation rate.
    /// The window's lateral input is positive to the right.
    #[allow(clippy::too_many_arguments)]
    pub fn step_local_player_physics(
        &mut self,
        dt_secs: f32,
        forward: f32,
        strafe: f32,
        jump: bool,
        sneak: bool,
        sprint: bool,
        yaw: f32,
        pitch: f32,
    ) -> Option<LocalPhysicsOutput> {
        if !dt_secs.is_finite()
            || dt_secs <= 0.0
            || !matches!(self.status, SessionStatus::Playing)
            || self.death.is_dead()
        {
            return None;
        }
        // Minecraft physics uses positive values for left.
        let minecraft_strafe = -strafe;
        const STEP: f32 = 0.05;
        let dt = dt_secs.min(4.0 * STEP);
        let base = self.pose?;
        self.physics_accumulator = (self.physics_accumulator + dt).min(4.0 * STEP);
        if self.player_abilities.flying() {
            let yaw_rad = f64::from(yaw.to_radians());
            let speed = if sprint { 10.92 } else { 7.0 } * f64::from(dt);
            let f = f64::from(forward.clamp(-1.0, 1.0));
            let s = f64::from(minecraft_strafe.clamp(-1.0, 1.0));
            let mut feet = base.feet;
            feet[0] += -yaw_rad.sin() * f * speed + yaw_rad.cos() * s * speed;
            feet[2] += yaw_rad.cos() * f * speed + yaw_rad.sin() * s * speed;
            feet[1] += f64::from(jump as i8 - sneak as i8) * speed;
            self.physics_state.pos = feet;
            self.physics_state.vel = [0.0; 3];
            self.physics_state.is_creative_flying = true;
            self.physics_state.on_ground = false;
            self.physics_state.in_water = false;
            self.physics_state.in_lava = false;
            let pose = PlayerPose { feet, yaw, pitch };
            let movement = PlayerMovement {
                pose,
                strafe,
                forward,
                jump,
                sneak,
            };
            self.eye_height = 1.62;
            self.apply_local_movement(movement);
            self.snapshot_physics_pose();
            if self.physics_accumulator >= STEP {
                self.physics_accumulator %= STEP;
                self.queue_movement(movement);
            }
            return Some(LocalPhysicsOutput {
                pose,
                eye_height: self.eye_height,
                walk_bob: (0.0, 0.0),
                on_ground: false,
                submerged: false,
            });
        }
        self.physics_state.is_creative_flying = false;
        self.sneaking = sneak;
        while self.physics_accumulator >= STEP {
            self.physics_accumulator -= STEP;
            self.snapshot_physics_pose();
            let was_ground = self.was_on_ground;
            let was_wet = self.was_submerged;
            let previous_fall_velocity_y = self.physics_state.vel[1] as f32;
            let old = self.pose.unwrap_or(base);
            if let Some(pose) = self.predict_movement(
                minecraft_strafe, forward, jump, sneak, sprint, yaw, pitch,
            ) {
                let dx = pose.feet[0] - old.feet[0];
                let dz = pose.feet[2] - old.feet[2];
                let distance = (dx * dx + dz * dz).sqrt() as f32;
                self.walk_dist += distance;
                self.walk_bob_phase = self.walk_dist * 7.0;
                self.fall_velocity_y = previous_fall_velocity_y;
                let wet = self.physics_state.in_water || self.physics_state.in_lava;
                if !was_wet && wet {
                    let seed_tick = self.next_tactile_event_tick();
                    self.on_fluid_entry(FluidEntryEvent {
                        pos: pose.feet.map(|v| v as f32),
                        impact_speed: (-self.physics_state.vel[1] as f32).max(0.0),
                        body: megu_client_mesh::tactile_effects::FluidEntryBody::PlayerOrMediumMob,
                        is_lava: self.physics_state.in_lava,
                        seed_tick,
                    });
                }
                self.was_submerged = wet;
                if !was_ground && self.physics_state.on_ground {
                    let speed = (-self.fall_velocity_y).max(0.0);
                    let under = [
                        pose.feet[0].floor() as i32,
                        (pose.feet[1] - 0.05).floor() as i32,
                        pose.feet[2].floor() as i32,
                    ];
                    let kind = ground_surface_for_name(&self.block_name_at(under));
                    self.on_footstep_or_landing(pose.feet.map(|v| v as f32), kind, speed);
                } else if distance > 0.0 && self.physics_state.on_ground {
                    self.next_step_dist += distance;
                    if self.next_step_dist >= 0.7 {
                        self.next_step_dist = 0.0;
                        let under = [
                            pose.feet[0].floor() as i32,
                            (pose.feet[1] - 0.05).floor() as i32,
                            pose.feet[2].floor() as i32,
                        ];
                        self.on_footstep_or_landing(
                            pose.feet.map(|v| v as f32),
                            ground_surface_for_name(&self.block_name_at(under)),
                            0.0,
                        );
                    }
                }
                self.was_on_ground = self.physics_state.on_ground;
                self.eye_height = if self.physics_state.swimming || self.physics_state.elytra_flying
                {
                    0.4
                } else if sneak {
                    1.27
                } else {
                    1.62
                };
                let movement = PlayerMovement {
                    pose,
                    strafe,
                    forward,
                    jump,
                    sneak,
                };
                self.apply_local_movement(movement);
                self.queue_movement(movement);
            }
        }
        let mut pose = self.pose?;
        pose.yaw = yaw;
        pose.pitch = pitch;
        self.pose = Some(pose);
        let alpha = (self.physics_accumulator / STEP).clamp(0.0, 1.0);
        let previous = self.prev_pose.unwrap_or(pose);
        for (feet, before) in pose.feet.iter_mut().zip(previous.feet) {
            *feet = before + (*feet - before) * f64::from(alpha);
        }
        let eye_height = self.prev_eye_height + (self.eye_height - self.prev_eye_height) * alpha;
        let bob_phase =
            self.prev_walk_bob_phase + (self.walk_bob_phase - self.prev_walk_bob_phase) * alpha;
        let bob = if self.physics_state.on_ground {
            (bob_phase.sin() * 0.035, (bob_phase * 2.0).cos() * 0.025)
        } else {
            (0.0, 0.0)
        };
        Some(LocalPhysicsOutput {
            pose,
            eye_height,
            walk_bob: bob,
            on_ground: self.physics_state.on_ground,
            submerged: self.physics_state.in_water || self.physics_state.in_lava,
        })
    }

    fn snapshot_physics_pose(&mut self) {
        self.prev_pose = self.pose;
        self.prev_eye_height = self.eye_height;
        self.prev_walk_bob_phase = self.walk_bob_phase;
    }

    fn reset_physics_interpolation(&mut self) {
        self.physics_accumulator = 0.0;
        self.eye_height = 1.62;
        self.walk_bob_phase = 0.0;
        self.walk_dist = 0.0;
        self.next_step_dist = 0.0;
        self.snapshot_physics_pose();
    }

    /// `chat`/`chat_command` — send one chat line; a leading `/` rides the
    /// `chat_command` packet per vanilla's sendChat semantics.
    pub fn send_chat(&mut self, message: &str) {
        if !matches!(self.status, SessionStatus::Playing) {
            return;
        }
        let _ = self
            .outgoing
            .try_send(WorkerCommand::Chat(message.to_string()));
    }

    /// `command_suggestion` C2S — ask the server to tab-complete `text`
    /// (the full chat-bar buffer, `/` included). Returns the
    /// `transaction_id` the [`PlayEvent::CommandSuggestions`] reply will
    /// echo — `None` while not playing or when the outgoing queue is
    /// full (the caller falls back to local completion).
    pub fn request_command_suggestion(&mut self, text: &str) -> Option<i32> {
        if !matches!(self.status, SessionStatus::Playing) {
            return None;
        }
        self.command_suggestion_tx = self.command_suggestion_tx.wrapping_add(1);
        let transaction_id = self.command_suggestion_tx;
        self.outgoing
            .try_send(WorkerCommand::CommandSuggestion {
                transaction_id,
                text: text.to_string(),
            })
            .ok()?;
        Some(transaction_id)
    }

    /// One 20 Hz client tick of item cooldowns — the window drives this
    /// from the frame clock; expired entries drop out of
    /// `hud.item_cooldowns` so the hotbar wipe ends cleanly.
    pub fn tick_item_cooldowns(&mut self) {
        if self.item_cooldowns.is_empty() {
            return;
        }
        self.item_cooldowns.retain(|_, cd| {
            cd.remaining_ticks -= 1;
            cd.remaining_ticks > 0
        });
        self.sync_hud_item_cooldowns();
    }

    /// Apply the audio settings sliders (vanilla sound categories):
    /// `master` → mixer master gain, `music` → sources 1+2 (music +
    /// jukebox records), `blocks` → 4, `hostile` → 5, `friendly` →
    /// 6+7 (neutral + players); sources 0/3/8/9 keep full gain under
    /// master like vanilla. `max_volume_cap`/`max_concurrent_same_sound`
    /// retune the admission limiter. Applies immediately to both the
    /// synchronous mixer and, via `SetVolumes`, the dedicated mix thread.
    pub fn set_audio_volumes(&mut self, audio: &megu_ui::state::AudioSettings) {
        let pct = |value: u8| f32::from(value.min(100)) / 100.0;
        let mut categories = [1.0f32; 10];
        categories[1] = pct(audio.music);
        categories[2] = pct(audio.music);
        categories[4] = pct(audio.blocks);
        categories[5] = pct(audio.hostile);
        categories[6] = pct(audio.friendly);
        categories[7] = pct(audio.friendly);
        self.audio
            .set_volume_profile(pct(audio.master), categories, pct(audio.max_volume_cap));
        self.audio
            .mixer
            .limiter
            .set_max_concurrent_same_sound(audio.max_concurrent_same_sound);
    }

    /// Vanilla `InteractionHand` ordinals, as they ride the wire.
    pub const MAIN_HAND: i32 = 0;
    pub const OFF_HAND: i32 = 1;
    /// The hand walk order vanilla `Minecraft.startUseItem` uses: the main
    /// hand first, then the off hand.
    pub const INTERACTION_HANDS: [i32; 2] = [Self::MAIN_HAND, Self::OFF_HAND];

    /// The stack carried in `hand`, or `None` for an empty slot (vanilla
    /// `LocalPlayer.getItemInHand(hand).isEmpty()`).
    fn item_in_hand(&self, hand: i32) -> Option<&str> {
        let name = if hand == Self::OFF_HAND {
            self.hud.offhand.as_deref()
        } else {
            self.hud
                .hotbar
                .get(self.hud.selected_slot)
                .and_then(|slot| slot.as_deref())
        }?;
        (!name.is_empty()).then_some(name)
    }

    /// Vanilla `LocalPlayer.isHandsEmpty()`.
    pub fn hands_empty(&self) -> bool {
        Self::INTERACTION_HANDS
            .iter()
            .all(|hand| self.item_in_hand(*hand).is_none())
    }

    /// `MultiPlayerGameMode.performUseItemOn`'s `skip_block_use`:
    /// `player.isSecondaryUseActive() && !player.isHandsEmpty()`. Sneaking
    /// with a stack in either hand keeps the block's own
    /// `useItemOn`/`useWithoutItem` from running, so the click is the held
    /// item's `useOn` (sneak-place instead of chest/door use). Spectator's
    /// container peek is decided before this rule, in
    /// [`Self::block_use_suppressed`].
    pub fn skips_block_use(&self) -> bool {
        self.sneaking && !self.hands_empty()
    }

    /// The hand `startUseItem` resolves for this click: the `[0, 1]`
    /// fallback — the main hand, or the off hand when only the off hand
    /// carries a stack (vanilla picks the off hand for an empty main hand).
    pub fn use_hand(&self) -> i32 {
        let main = self.item_in_hand(Self::MAIN_HAND);
        let off = self.item_in_hand(Self::OFF_HAND);
        match (main, off) {
            (None, Some(_)) => Self::OFF_HAND,
            _ => Self::MAIN_HAND,
        }
    }

    /// `true` when the block's own interaction is suppressed for this
    /// click. Spectator container peeks happen before `skip_block_use` in
    /// `performUseItemOn`, so a spectator's right-click still reaches the
    /// server, which opens container menus for it.
    pub fn block_use_suppressed(&self) -> bool {
        self.game_mode != 3 && self.skips_block_use()
    }

    /// `use_item` — right-click in air with `hand` (0 main, 1 off), using
    /// the frame's look angles and the next interaction sequence.
    pub fn use_item(&mut self, hand: i32) {
        let Some(pose) = self.render_frame_pose() else {
            return;
        };
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
        // Vanilla `MultiPlayerGameMode.performUseItemOn` predicts the
        // clicked cell only while the block's own `useItemOn` runs; with
        // `skip_block_use` the click is the held item's `useOn`, which can
        // only change the face-adjacent cell (sneak-place).
        let predicted: &[[i32; 3]] = if self.block_use_suppressed() {
            &[adjacent_block_pos(pos, face)]
        } else {
            &[pos, adjacent_block_pos(pos, face)]
        };
        let sequence = self.use_item_sequence;
        self.track_pending_interaction(sequence, predicted);
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

    /// `interact` attack — left-click hit on `entity_id` (vanilla action
    /// 1); pairs with [`Self::swing`] exactly like vanilla's
    /// `attack()` dispatch.
    pub fn attack_entity(&mut self, entity_id: i32) {
        let _ = self.outgoing.try_send(WorkerCommand::Interact {
            target: entity_id,
            action: 1,
            at: None,
            hand: 0,
            sneaking: self.sneaking,
        });
    }

    /// `interact` use — right-click on `entity_id`; `at` is the absolute
    /// crosshair hit position (vanilla `interact_at`, action 2 — falls
    /// back to plain `interact` without a hit point). `hand` is the hand
    /// `startUseItem` resolved for this click.
    pub fn interact_entity(&mut self, entity_id: i32, at: Option<[f32; 3]>, hand: i32) {
        let _ = self.outgoing.try_send(WorkerCommand::Interact {
            target: entity_id,
            action: if at.is_some() { 2 } else { 0 },
            at,
            hand,
            sneaking: self.sneaking,
        });
    }

    /// `player_action` dig statuses at `pos`/`face` — 0 start, 1 abort,
    /// 2 finish (the `face` id is the raycast `BlockHitFace` ordinal,
    /// matching vanilla `Direction` 0..=5).
    fn player_action(&mut self, status: i32, pos: [i32; 3], face: i8) {
        self.use_item_sequence = self.use_item_sequence.wrapping_add(1);
        let _ = self.outgoing.try_send(WorkerCommand::PlayerAction {
            status,
            pos,
            face,
            sequence: self.use_item_sequence,
        });
        // Dig statuses (0 start, 1 abort, 2 finish) predict the target
        // block may change; drop/swap statuses carry `pos = [0; 3]` and
        // touch no blocks, so they stay untracked.
        if (0..=2).contains(&status) {
            self.track_pending_interaction(self.use_item_sequence, &[pos]);
        }
    }

    /// `START_DESTROY_BLOCK` — left-click on a diggable block.
    pub fn dig_start(&mut self, pos: [i32; 3], face: i8) {
        self.player_action(0, pos, face);
    }

    /// `ABORT_DESTROY_BLOCK` — release/retarget mid-dig.
    pub fn dig_abort(&mut self, pos: [i32; 3], face: i8) {
        self.player_action(1, pos, face);
    }

    /// `STOP_DESTROY_BLOCK` — the dig completed (server broke the block).
    pub fn dig_finish(&mut self, pos: [i32; 3], face: i8) {
        self.player_action(2, pos, face);
    }

    pub fn begin_mining(&mut self, pos: [i32; 3], face: i8) {
        if self.mining.target == Some((pos, face)) && !self.mining.complete {
            return;
        }
        self.cancel_mining();
        self.dig_start(pos, face);
        if self.hud.game_mode == 1 {
            self.dig_finish(pos, face);
            self.mining.cooldown = 0.25;
            self.trigger_arm_swing();
            return;
        }
        let name = self.block_name_at(pos);
        let mining_meta = vanilla_block_mining_meta(&name);
        let hardness = mining_meta.hardness;
        if hardness == 0.0 && mining_meta.diggable {
            // Instantly fragile blocks (torches, flowers, grass) are broken by
            // the server on START; vanilla does not emit a redundant STOP.
            self.mining = MiningState {
                target: Some((pos, face)),
                progress: 1.0,
                duration: 0.0,
                cooldown: 0.0,
                swing_timer: 0.0,
                complete: true,
                state_id: self.world.get_block_state(pos[0], pos[1], pos[2]),
            };
            self.trigger_arm_swing();
            return;
        }
        if hardness < 0.0 || !hardness.is_finite() || !mining_meta.diggable {
            self.mining = MiningState {
                target: Some((pos, face)),
                duration: f32::INFINITY,
                state_id: self.world.get_block_state(pos[0], pos[1], pos[2]),
                ..MiningState::default()
            };
            return;
        }
        let hotbar_slot = 36 + self.hud.selected_slot as i32;
        let stack = self.player_inventory.get(&hotbar_slot);
        let tool = stack
            .map(|s| crate::recipe_ingest::item_name_for_id(s.item_id as u32))
            .unwrap_or_default();
        let (tool_kind, tier_speed) = mining_tool_facts(&tool);
        let required_tool = mining_meta.preferred_tool;
        // `mineable/<tool>` controls only the speed multiplier; the progress
        // divisor uses `hasCorrectToolForDrops`. Vanilla blocks without a
        // harvestTools set are correct bare-handed; otherwise the exact item
        // bitset generated from the pinned registry decides.
        let held_bit = stack
            .map(|stack| vanilla_tool_harvest_bit(stack.item_id))
            .unwrap_or(0);
        let correct_tool =
            mining_meta.harvest_mask == 0 || mining_meta.harvest_mask & held_bit != 0;
        let effective_tool = required_tool.is_some_and(|required| tool_kind == Some(required));
        let can_harvest = correct_tool;
        let mut speed = mining_item_destroy_speed(
            &tool,
            &name,
            required_tool,
            tool_kind,
            tier_speed,
            effective_tool,
        );
        if speed > 1.0 {
            let efficiency = stack
                .map(|stack| item_enchantment_level(stack, EFFICIENCY_ENCHANTMENT_ID))
                .unwrap_or(0);
            if efficiency > 0 {
                speed += (efficiency * efficiency + 1) as f32;
            }
        }
        let mut effect_multiplier = 1.0f32;
        if let Some(eid) = self.player_entity_id {
            if let Some(effects) = self.mob_effects.get(&eid) {
                let haste = effects.get(&2).map(|effect| effect.amplifier).unwrap_or(-1);
                let conduit = effects
                    .get(&28)
                    .map(|effect| effect.amplifier)
                    .unwrap_or(-1);
                let best_haste = haste.max(conduit);
                if best_haste >= 0 {
                    effect_multiplier *= 1.0 + 0.2 * (best_haste + 1) as f32;
                }
                if let Some(fatigue) = effects.get(&3) {
                    effect_multiplier *= [0.3, 0.09, 0.0027, 0.00081]
                        .get(usize::try_from(fatigue.amplifier).unwrap_or(3).min(3))
                        .copied()
                        .unwrap_or(0.00081);
                }
            }
        }
        speed *= effect_multiplier;
        if self.physics_state.eye_in_water {
            let helmet = self.player_inventory.get(&5);
            let aqua = helmet
                .map(|stack| item_enchantment_level(stack, AQUA_AFFINITY_ENCHANTMENT_ID))
                .unwrap_or(0);
            if aqua == 0 {
                speed *= 0.2;
            }
        }
        if !self.physics_state.on_ground {
            speed *= 0.2;
        }
        // Vanilla accumulates progress in whole 20 Hz ticks and therefore
        // rounds fractional break times up (`ceil` in `ticks_to_break`).
        // Obsidian with a diamond pickaxe is 188 ticks / 9.4 seconds, not
        // the continuous 187.5-tick ratio.
        let ticks = megu_core::break_progress::ticks_to_break(hardness, can_harvest, speed);
        let duration = if ticks == u32::MAX {
            f32::INFINITY
        } else {
            ticks as f32 / 20.0
        };
        self.mining = MiningState {
            target: Some((pos, face)),
            progress: 0.0,
            duration,
            cooldown: 0.0,
            swing_timer: 0.0,
            complete: false,
            state_id: self.world.get_block_state(pos[0], pos[1], pos[2]),
        };
        // Vanilla instant-break blocks complete inside the first tick and do
        // not produce a client STOP packet; the server accepts START as the
        // break action itself.
        if megu_core::break_progress::instant_break(speed, hardness, can_harvest) {
            self.mining.progress = 1.0;
            self.mining.complete = true;
        }
    }

    pub fn tick_mining(&mut self, dt_secs: f32, current_target: Option<([i32; 3], i8)>) -> bool {
        if !dt_secs.is_finite() || dt_secs <= 0.0 {
            return false;
        }
        self.mining.cooldown = (self.mining.cooldown - dt_secs).max(0.0);
        if self.mining.cooldown > 0.0 {
            return false;
        }
        if let Some(target) = self.mining.target {
            if current_target != Some(target) {
                if !self.mining.complete {
                    self.dig_abort(target.0, target.1);
                }
                self.mining = MiningState::default();
                if let Some((pos, face)) = current_target {
                    self.begin_mining(pos, face);
                }
                return false;
            }
            // Any authoritative block mutation closes this exact attempt, even
            // when the coordinates stay constant. In particular a completed
            // dig waits for that mutation instead of restarting early.
            let state = self
                .world
                .get_block_state(target.0[0], target.0[1], target.0[2]);
            if state != self.mining.state_id || state.is_none() {
                self.mining = MiningState::default();
                return false;
            }
            if state == Some(0) {
                self.mining = MiningState::default();
                return false;
            }
        } else if let Some((pos, face)) = current_target {
            self.begin_mining(pos, face);
            return false;
        }
        if self.mining.target.is_none() || self.mining.complete || !self.mining.duration.is_finite()
        {
            return false;
        }
        self.mining.progress += dt_secs / self.mining.duration.max(0.05);
        self.mining.swing_timer += dt_secs;
        if self.mining.swing_timer >= 0.3 {
            self.mining.swing_timer = 0.0;
            self.trigger_arm_swing();
        }
        if self.mining.progress >= 1.0 {
            let Some((pos, face)) = self.mining.target else {
                return false;
            };
            self.dig_finish(pos, face);
            self.mining.progress = 1.0;
            self.mining.complete = true;
            return true;
        }
        false
    }

    pub fn cancel_mining(&mut self) {
        if let Some((pos, face)) = self.mining.target {
            if !self.mining.complete {
                self.dig_abort(pos, face);
            }
        }
        self.mining = MiningState::default();
    }

    pub fn active_block_break_overlay(&self) -> Option<([i32; 3], u8)> {
        if let Some((pos, _)) = self.mining.target {
            if !self.mining.complete {
                return Some((pos, (self.mining.progress.clamp(0.0, 0.999) * 10.0) as u8));
            }
        }
        self.remote_block_destruction.values().next_back().copied()
    }

    pub fn begin_item_use(&mut self, hand: i32, on_block: bool) {
        let name = self.item_in_hand(hand).unwrap_or_default().to_owned();
        let kind = if [
            "apple",
            "bread",
            "beef",
            "porkchop",
            "chicken",
            "carrot",
            "potato",
            "cookie",
            "melon",
            "berry",
            "chorus_fruit",
            "stew",
            "pie",
        ]
        .iter()
        .any(|s| name.contains(s))
        {
            FirstPersonUseKind::Eat
        } else if name.contains("potion") || name.contains("milk") || name.contains("honey_bottle")
        {
            FirstPersonUseKind::Drink
        } else if name.ends_with(":bow") {
            FirstPersonUseKind::BowDraw
        } else if name.ends_with(":crossbow") {
            FirstPersonUseKind::CrossbowCharge
        } else if name.ends_with(":shield") {
            FirstPersonUseKind::ShieldBlock
        } else if name.ends_with(":trident") {
            FirstPersonUseKind::TridentThrow
        } else {
            FirstPersonUseKind::None
        };
        self.item_use = ItemUseState {
            active: true,
            on_block,
            elapsed: 0.0,
            place_timer: 0.0,
            kind,
        };
        if on_block {
            self.trigger_arm_swing_for(hand);
        } else {
            self.use_item(hand);
        }
    }

    pub fn tick_item_use(&mut self, dt_secs: f32) -> bool {
        if !self.item_use.active || !dt_secs.is_finite() || dt_secs <= 0.0 {
            return false;
        }
        self.item_use.elapsed += dt_secs;
        self.item_use.place_timer += dt_secs;
        if self.item_use.on_block && self.item_use.place_timer >= 0.2 {
            self.item_use.place_timer %= 0.2;
            return true;
        }
        false
    }

    pub fn release_item_use(&mut self) {
        if !self.item_use.active {
            return;
        }
        let _ = self.outgoing.try_send(WorkerCommand::ReleaseUseItem);
        self.item_use.active = false;
        self.item_use.kind = FirstPersonUseKind::None;
    }

    pub fn trigger_arm_swing(&mut self) {
        self.trigger_arm_swing_for(Self::MAIN_HAND);
    }

    /// Arm-swing for the hand that actually acted; both the animation clock
    /// and the `swing` packet must carry the same hand.
    pub fn trigger_arm_swing_for(&mut self, hand: i32) {
        self.swing_progress = 0.0;
        self.attack_cooldown = 0.0;
        self.swing(hand);
    }

    pub fn first_person_viewmodel(
        &self,
        visible: bool,
        walk_bob: (f32, f32),
    ) -> FirstPersonViewmodel {
        let name = self
            .hud
            .hotbar
            .get(self.hud.selected_slot)
            .cloned()
            .flatten();
        FirstPersonViewmodel {
            visible,
            held_item_name: name.clone(),
            held_color_rgb: [1.0; 3],
            is_block: name.as_deref().is_some_and(|n| {
                ![
                    "sword", "pickaxe", "axe", "shovel", "hoe", "bow", "crossbow", "trident",
                    "shield", "potion", "apple", "bread", "bucket", "elytra",
                ]
                .iter()
                .any(|kind| n.contains(kind))
            }),
            swing_progress: self.swing_progress,
            use_progress: (self.item_use.elapsed / 1.6).clamp(0.0, 1.0),
            using_kind: self.item_use.kind,
            walk_bob_x: walk_bob.0,
            walk_bob_y: walk_bob.1,
            equip_progress: 1.0,
        }
    }

    /// `set_carried_item` — tell the server which hotbar slot (0..=8) is
    /// held after a local selection change.
    pub fn set_carried_item(&mut self, slot: i32) {
        let _ = self
            .outgoing
            .try_send(WorkerCommand::SetCarriedItem { slot });
    }

    pub fn send_move_vehicle(&mut self, x: f64, y: f64, z: f64, yaw: f32, pitch: f32) {
        let _ = self.outgoing.try_send(WorkerCommand::MoveVehicle {
            x,
            y,
            z,
            yaw,
            pitch,
        });
    }

    pub fn send_paddle_boat(&mut self, left: bool, right: bool) {
        let _ = self
            .outgoing
            .try_send(WorkerCommand::PaddleBoat { left, right });
    }

    pub fn send_pick_item_for_block(&mut self, block_name: &str) -> bool {
        let found = self
            .player_inventory
            .iter()
            .filter_map(|(&slot, stack)| {
                ((9..=44).contains(&slot) && stack.count > 0).then_some((slot, stack))
            })
            .find(|(_, stack)| {
                crate::recipe_ingest::item_name_for_id(stack.item_id as u32) == block_name
            })
            .map(|(slot, _)| slot);
        let Some(slot) = found else { return false };
        if (36..=44).contains(&slot) {
            self.set_carried_item(slot - 36);
        } else {
            let _ = self.outgoing.try_send(WorkerCommand::PickItem { slot });
        }
        true
    }

    pub fn send_creative_mode_slot(&mut self, slot: i16, item_id: i32, count: i32) {
        let _ = self.outgoing.try_send(WorkerCommand::CreativeModeSlot {
            slot,
            item_id,
            count,
        });
    }

    pub fn send_stop_sleeping(&mut self) {
        self.sleeping = false;
        self.player_command(2);
    }

    /// Queue one `edit_book` packet. Returns `true` only when this is a live
    /// play session and the bounded worker queue accepted the packet; callers
    /// keep the user's draft open on `false` so a full/closed queue never
    /// silently discards text.
    pub fn send_edit_book(&mut self, slot: i32, pages: Vec<String>, title: Option<String>) -> bool {
        matches!(self.status, SessionStatus::Playing)
            && self
                .outgoing
                .try_send(WorkerCommand::EditBook { slot, pages, title })
                .is_ok()
    }

    pub fn riding_vehicle_id(&self) -> Option<i32> {
        self.riding_vehicle
    }

    pub fn riding_vehicle_position(&self) -> Option<[f64; 3]> {
        self.riding_vehicle.and_then(|id| {
            self.entity_positions
                .get(&id)
                .copied()
                .or_else(|| self.world.entities.get(&id).map(|entity| entity.pos))
        })
    }

    pub fn is_sleeping(&self) -> bool {
        self.sleeping
            || self
                .player_entity_id
                .and_then(|id| self.entity_metadata.get(&id))
                .and_then(|meta| meta.pose)
                .is_some_and(|pose| pose == 2)
    }

    pub fn set_sleeping(&mut self, sleeping: bool) {
        self.sleeping = sleeping;
    }

    /// `player_command` — entity action id (0 start-sneak, 1 stop-sneak,
    /// 3 start-sprint, 4 stop-sprint, 8 start-flying-elytra). Needs the
    /// player entity id from `login` — no-ops until that lands.
    pub fn player_command(&mut self, action: i32) {
        let Some(entity_id) = self.player_entity_id else {
            return;
        };
        let _ = self.outgoing.try_send(WorkerCommand::PlayerCommand {
            entity_id,
            action,
            jump_boost: 0,
        });
    }

    /// `START_FLYING_WITH_ELYTRA` — the vanilla mid-air jump press: sent
    /// only while airborne, not already gliding, and with an elytra in
    /// the chest armor slot (window-0 container slot 6).
    pub fn try_start_elytra(&mut self) {
        if !matches!(self.status, SessionStatus::Playing)
            || self.death.is_dead()
            || self.physics_state.on_ground
            || self.physics_state.elytra_flying
        {
            return;
        }
        let wearing_elytra = self
            .player_inventory
            .get(&6)
            .filter(|stack| stack.item_id > 0 && stack.count > 0)
            .is_some_and(|stack| {
                crate::recipe_ingest::item_name_for_id(stack.item_id as u32).ends_with("elytra")
            });
        if wearing_elytra {
            self.player_command(8);
        }
    }

    /// `player_abilities` — the vanilla double-jump creative-flight
    /// toggle: flips flag 0x02 only while the server keeps `may_fly`
    /// (0x04) set, mirrors the result into the physics prediction, and
    /// echoes the flags byte to the server. Returns whether the flying
    /// state actually changed (callers skip the normal jump impulse on
    /// a toggle).
    pub fn toggle_flight(&mut self) -> bool {
        if !matches!(self.status, SessionStatus::Playing) || !self.player_abilities.may_fly() {
            return false;
        }
        self.player_abilities.flags ^= 0x02;
        let flying = self.player_abilities.flying();
        self.physics_state.is_creative_flying = flying;
        // Vanilla zeroes the downward velocity the tick flight starts so
        // the player stops falling immediately rather than gliding down.
        if flying && self.physics_state.vel[1] < 0.0 {
            self.physics_state.vel[1] = 0.0;
        }
        let _ = self.outgoing.try_send(WorkerCommand::PlayerAbilities {
            flags: self.player_abilities.flags,
        });
        true
    }

    /// `client_information` — play-phase re-announce when the settings
    /// screen changes render distance mid-session. No-ops when nothing
    /// changed or the session is not in `Playing` yet.
    pub fn resend_client_information(&mut self, view_distance: i8) {
        if view_distance == self.client_view_distance
            || !matches!(self.status, SessionStatus::Playing)
        {
            return;
        }
        self.client_view_distance = view_distance;
        let _ = self
            .outgoing
            .try_send(WorkerCommand::ClientInformation { view_distance });
    }

    /// The sub-world the latest SW6D snapshot flags as player-controlled,
    /// while its render entry is still live — the helm target
    /// [`Self::send_subworld_helm`] commands should address.
    pub fn player_subworld(&self) -> Option<u64> {
        self.player_subworld_id
            .filter(|id| self.subworlds.get(*id).is_some())
    }

    /// Queue one 6-DOF subworld helm command for the connection worker.
    /// Returns false when the bounded worker queue cannot accept it.
    pub fn send_subworld_helm(&self, cmd: &SubWorldHelmCmd) -> bool {
        self.outgoing
            .try_send(WorkerCommand::SendCustomPayload {
                channel: SUBWORLD_HELM_CHANNEL.to_owned(),
                payload: encode_helm_cmd(cmd),
            })
            .is_ok()
    }

    /// One chat line into the bounded HUD scrollback (vanilla's 100-line
    /// cap — oldest evicted first).
    pub fn push_chat_line(&mut self, line: String) {
        self.push_chat_line_keyed(line, None);
    }

    /// Wipe the HUD scrollback plus its retraction keys (debug clear /
    /// reconnect reset paths that host code owns).
    pub fn clear_chat_log(&mut self) {
        self.hud.chat_lines.clear();
        self.chat_log_keys.clear();
        self.hud_revision += 1;
    }

    /// Apply the configured chat history capacity immediately, evicting
    /// oldest entries when the setting is reduced.
    pub fn set_chat_history_limit(&mut self, limit: usize) {
        self.chat_history_limit = limit.clamp(100, 5000);
        let excess = self
            .hud
            .chat_lines
            .len()
            .saturating_sub(self.chat_history_limit);
        if excess > 0 {
            self.hud.chat_lines.drain(..excess);
            self.chat_log_keys
                .drain(..excess.min(self.chat_log_keys.len()));
            self.hud_revision += 1;
        }
    }

    /// [`push_chat_line`](Self::push_chat_line) carrying the `player_chat`
    /// deletion key (`index` + `signature`) so `delete_chat` can retract
    /// the line later.
    fn push_chat_line_keyed(&mut self, line: String, key: Option<ChatMessageKey>) {
        self.hud.chat_lines.push(line);
        self.chat_log_keys.push_back(key);
        if self.hud.chat_lines.len() > self.chat_history_limit {
            let excess = self.hud.chat_lines.len() - self.chat_history_limit;
            self.hud.chat_lines.drain(..excess);
            self.chat_log_keys
                .drain(..excess.min(self.chat_log_keys.len()));
        }
        self.hud_revision += 1;
    }

    /// Keep `chat_log_keys` exactly parallel to `hud.chat_lines`: host
    /// code may append status lines to `hud.chat_lines` directly, so any
    /// surplus gets unsigned `None` keys on the right side.
    fn reconcile_chat_keys(&mut self) {
        while self.chat_log_keys.len() < self.hud.chat_lines.len() {
            self.chat_log_keys.push_back(None);
        }
        while self.chat_log_keys.len() > self.hud.chat_lines.len() {
            self.chat_log_keys.pop_front();
        }
    }

    /// Mirror the tab-list roster into `hud.players` (name + ping rows
    /// the hold-Tab overlay renders; `listed = false` entries hide like
    /// vanilla).
    fn sync_hud_players(&mut self) {
        let mut players: Vec<_> = self
            .player_info
            .values()
            .filter(|entry| entry.listed != Some(false))
            .map(|entry| {
                let (label, color) =
                    if let Some(label) = entry.display_name_nbt.as_deref().and_then(nbt_text) {
                        // A server-supplied display name overrides the team-formatted profile name.
                        (label, None)
                    } else if let Some(name) = entry.name.as_deref() {
                        let (label, color) = self.team_label(name, name);
                        (label.into_owned(), color)
                    } else {
                        (uuid_label(&entry.uuid), None)
                    };
                (
                    (
                        label,
                        entry.latency.unwrap_or(0).clamp(0, i32::from(u16::MAX)) as u16,
                    ),
                    color,
                )
            })
            .collect();
        players.sort_by_cached_key(|((label, _), _)| label.to_lowercase());
        let (rows, colors) = players.into_iter().unzip();
        self.hud.players = rows;
        self.hud.player_colors = colors;
    }

    fn sync_hud_tab_list(&mut self) {
        self.hud.tab_header.clone_from(&self.world.tab_header);
        self.hud.tab_footer.clone_from(&self.world.tab_footer);
    }

    fn sync_hud_combat(&mut self) {
        let combat = HudCombat {
            in_combat: self.world.combat.in_combat,
            ticks: self.world.combat.ticks,
            last_attacker_id: self.world.combat.last_attacker_id,
        };
        if self.hud.combat != combat {
            self.hud.combat = combat;
            self.bump_hud_revision();
        }
    }

    fn team_label<'a>(&'a self, member: &str, label: &'a str) -> (Cow<'a, str>, Option<[u8; 3]>) {
        let Some(team) = self.world.team_for_member(member) else {
            return (Cow::Borrowed(label), None);
        };
        let prefix = nbt_text(&team.info.prefix_nbt).unwrap_or_default();
        let suffix = nbt_text(&team.info.suffix_nbt).unwrap_or_default();
        let color = u32::try_from(team.info.color)
            .ok()
            .and_then(vanilla_team_color);
        (Cow::Owned(format!("{prefix}{label}{suffix}")), color)
    }

    /// Resolve the team and display metadata for a remote, visible entity.
    ///
    /// The label stays borrowed whenever the entity carries no team prefix or
    /// suffix, so the per-frame nametag pass only allocates for the labels it
    /// actually paints.
    pub fn entity_nametag(
        &self,
        eid: i32,
        viewer: &str,
    ) -> Option<(Cow<'_, str>, Option<[u8; 3]>)> {
        if Some(eid) == self.player_entity_id || self.world.camera_entity_id == Some(eid) {
            return None;
        }
        let entity = self.world.entities.get(&eid)?;
        let meta = self.entity_metadata.get(&eid);
        let player = self
            .player_info
            .get(&entity.uuid)
            .and_then(|p| p.name.as_deref());
        let custom = meta
            .filter(|m| m.name_visible == Some(true))
            .and_then(|m| m.custom_name.as_deref());
        let label = player.or(custom)?;
        let uuid;
        let member = if let Some(player) = player {
            player
        } else {
            uuid = scoreboard_uuid(entity.uuid);
            &uuid
        };
        let invisible = meta
            .and_then(|m| m.flags)
            .is_some_and(|flags| flags & 0x20 != 0);
        if invisible
            && !self
                .world
                .team_allows_seeing_invisible_member(member, viewer)
        {
            return None;
        }
        self.world
            .team_shows_nametag(member, viewer)
            .then(|| self.team_label(member, label))
    }

    /// Mirror the decoded boss bars into `hud.bossbars` (`(title,
    /// fraction)` rows the HUD draws top-of-screen).
    fn sync_hud_bossbars(&mut self) {
        self.hud.bossbars = self
            .boss_bars
            .values()
            .map(|bar| (bar.title.clone(), bar.health.clamp(0.0, 1.0)))
            .collect();
    }

    /// Mirror the window-0 player inventory into `hud.hotbar` /
    /// `hud.offhand` / `hud.armor` (container slots 36..45 hotbar, 45
    /// offhand, 5..9 armor — vanilla `InventoryMenu` layout).
    fn sync_hud_inventory(&mut self) {
        let name_of = |inventory: &BTreeMap<i32, ItemStackComponents>, slot: i32| {
            inventory
                .get(&slot)
                .filter(|stack| stack.item_id > 0 && stack.count > 0)
                .map(|stack| crate::recipe_ingest::item_name_for_id(stack.item_id as u32))
        };
        let hotbar =
            std::array::from_fn(|index| name_of(&self.player_inventory, 36 + index as i32));
        let offhand = name_of(&self.player_inventory, 45);
        let armor = (5..9)
            .map(|slot| armor_points_for(name_of(&self.player_inventory, slot).as_deref()))
            .sum::<u32>()
            .min(255) as u8;
        self.hud.hotbar = hotbar;
        self.hud.offhand = offhand;
        self.hud.armor = armor;
        self.sync_hud_map();
    }

    fn held_map_id(&self) -> Option<i32> {
        const COMP_MAP_ID: i32 = 26;
        [36 + self.hud.selected_slot.min(8) as i32, 45]
            .into_iter()
            .filter_map(|slot| self.player_inventory.get(&slot))
            .filter(|stack| stack.count > 0 && stack.item_id > 0)
            .filter(|stack| {
                crate::recipe_ingest::item_name_for_id(stack.item_id as u32)
                    == "minecraft:filled_map"
            })
            .find_map(|stack| component_varint_value(stack, COMP_MAP_ID))
    }

    pub fn sync_hud_map(&mut self) {
        let Some((id, map)) = self
            .held_map_id()
            .and_then(|id| Some((id, self.world.maps.get(&id)?)))
        else {
            self.hud.held_map = None;
            return;
        };
        let icons = map
            .icons
            .iter()
            .map(|icon| HudMapIcon {
                kind: icon.kind,
                x: icon.x,
                z: icon.z,
                direction: icon.direction,
                label: icon.name_nbt.as_deref().and_then(nbt_text),
            })
            .collect();
        if self.hud.held_map.as_ref().is_some_and(|old| {
            old.id == id
                && old.scale == map.scale
                && old.locked == map.locked
                && Arc::ptr_eq(&old.pixels, &map.pixels)
                && old.icons == icons
        }) {
            return;
        }
        self.hud.held_map = Some(Arc::new(HudMap {
            id,
            scale: map.scale,
            locked: map.locked,
            pixels: map.pixels.clone(),
            icons,
        }));
    }

    /// The frame pose every pick path consumes: the pose the window
    /// installed for the frame being drawn, else one derived from the raw
    /// simulation pose so headless callers and the first frame still work.
    pub fn render_frame_pose(&self) -> Option<RenderFramePose> {
        self.render_frame_pose.or_else(|| {
            let pose = self.pose?;
            Some(RenderFramePose::new(
                pose.feet,
                self.eye_height,
                pose.yaw,
                pose.pitch,
            ))
        })
    }

    /// Install the frame's single [`RenderFramePose`] — the eye origin and
    /// look vector the render camera uses, so the crosshair picks against
    /// exactly what the player sees this frame.
    pub fn set_render_pose(&mut self, pose: RenderFramePose) {
        self.render_frame_pose = Some(pose);
    }

    /// The vanilla crosshair pick — entity hits win only when strictly
    /// nearer than the traced block (`GameRenderer.pick` order).
    pub fn crosshair_pick(&self) -> Option<CrosshairPick> {
        match (self.crosshair_entity_hit(), self.crosshair_block_hit()) {
            (Some(entity), Some(block)) => Some(if entity.distance < block.distance {
                CrosshairPick::Entity(entity)
            } else {
                CrosshairPick::Block(block)
            }),
            (Some(entity), None) => Some(CrosshairPick::Entity(entity)),
            (None, Some(block)) => Some(CrosshairPick::Block(block)),
            (None, None) => None,
        }
    }

    /// `true` while the local player is dead — the window locks gameplay
    /// input and shows `Screen::Death` on this.
    pub fn is_dead(&self) -> bool {
        self.death.is_dead()
    }

    /// Active local `hurt_animation` pose (`None` once the 10-tick animation
    /// has played out) — the input to `GameRenderer.bobHurt`.
    pub fn local_hurt_animation(&self) -> Option<LocalHurtAnimation> {
        self.local_hurt
    }

    /// Whether the active `JoinGame` session is in Hardcore mode.
    pub fn is_hardcore(&self) -> bool {
        self.death.hardcore()
    }

    /// Snapshot for `Screen::Death` while dead (`None` while alive) —
    /// cause text, hardcore flag, and the XP-total "Score" line straight
    /// from the [`DeathLoopState`].
    pub fn death_screen_state(&self) -> Option<DeathScreenState> {
        self.death.is_dead().then(|| DeathScreenState {
            cause_message: self.death.cause_message().map(str::to_owned).or_else(|| {
                let eid = self.world.combat.last_attacker_id?;
                let attacker = self.world.entities.get(&eid)?;
                self.player_info
                    .get(&attacker.uuid)
                    .and_then(|entry| entry.name.as_ref())
                    .or_else(|| self.entity_metadata.get(&eid)?.custom_name.as_ref())
                    .map(|name| format!("Last attacker: {name}"))
            }),
            score: self.death.score(),
            hardcore: self.death.hardcore(),
            // The server does not gate respawn; the tracker's own
            // pending flag prevents request spam instead.
            respawn_cooldown_ticks: 0,
        })
    }

    /// Death-screen `Respawn` click: queue `client_command(0)` —
    /// `PERFORM_RESPAWN` on the wire — when the tracker says a request
    /// is legitimate (dead, non-hardcore, none pending). Returns `true`
    /// only when the worker queue accepted it; the screen stays up until
    /// the server's `respawn`/`set_health(>0)` clears `death`.
    pub fn request_respawn(&mut self) -> bool {
        if !matches!(self.status, SessionStatus::Playing) || !self.death.can_request_respawn() {
            return false;
        }
        match self
            .outgoing
            .try_send(WorkerCommand::ClientCommand { action: 0 })
        {
            Ok(()) => {
                self.death.note_respawn_sent();
                true
            }
            Err(_) => false,
        }
    }

    /// End-credits completion: Protocol 767 uses the same `client_command`
    /// action (`PERFORM_RESPAWN = 0`) as death respawn, but vanilla sends it
    /// while the player is still alive when the credits close or when the
    /// server emits `WIN_GAME` with value `0`. It is intentionally independent
    /// of [`DeathLoopState`]'s death guard and remains retryable.
    pub fn send_end_credits_complete(&mut self) -> bool {
        if !matches!(self.status, SessionStatus::Playing) {
            return false;
        }
        let accepted = self
            .outgoing
            .try_send(WorkerCommand::ClientCommand { action: 0 })
            .is_ok();
        if accepted {
            self.pending_end_credits = false;
        }
        accepted
    }

    /// The authoritative decoded stack for `slot` in `window_id`, if the
    /// server ever synced it: window `0` is the player's own inventory
    /// (`container_set_content` without an `open_screen`), every other id
    /// the matching open container. Container clicks forward this stack's
    /// component patch instead of a bare item/count pair, so enchantments,
    /// custom names, and durability survive a click round trip.
    pub fn authoritative_container_stack(
        &self,
        window_id: u8,
        slot: i16,
    ) -> Option<&ItemStackComponents> {
        let slot = i32::from(slot);
        if window_id == 0 {
            return self.player_inventory.get(&slot);
        }
        self.open_screen
            .as_ref()
            .filter(|screen| screen.window_id == i32::from(window_id))
            .and_then(|screen| screen.slots.get(&slot))
    }

    /// The authoritative carried (cursor) stack for `window_id`, mirroring
    /// [`Self::authoritative_container_stack`].
    pub fn authoritative_carried_stack(&self, window_id: u8) -> Option<&ItemStackComponents> {
        if window_id == 0 {
            return self.player_carried.as_ref();
        }
        self.open_screen
            .as_ref()
            .filter(|screen| screen.window_id == i32::from(window_id))
            .and_then(|screen| screen.carried.as_ref())
    }

    /// `container_click` — one window transaction; `changed`/`carried` are
    /// raw `Slot` wire blobs (build simple stacks via
    /// `megu_client::protocol::encode_slot_stack`). Returns `false` when the
    /// worker queue refused the command: the caller painted a prediction the
    /// server will never see, so it must restore its slots from
    /// [`Self::open_screen`] / [`Self::player_inventory`] instead of leaving
    /// ghost items on screen.
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
    ) -> bool {
        let queued = self
            .outgoing
            .try_send(WorkerCommand::ContainerClick {
                window_id,
                state_id,
                slot,
                button,
                mode,
                changed,
                carried,
            })
            .is_ok();
        if !queued {
            self.dropped_container_clicks = self.dropped_container_clicks.wrapping_add(1);
        }
        queued
    }

    /// `container_button_click` — enchant-table-style button press.
    /// `container_id` is the menu's `int` id (VarInt on the wire).
    pub fn container_button_click(&mut self, container_id: i32, button_id: i32) -> bool {
        self.outgoing
            .try_send(WorkerCommand::ContainerButtonClick {
                container_id,
                button_id,
            })
            .is_ok()
    }

    /// `seen_advancements` — announce the advancements screen opened on
    /// `tab` (`Some`) or closed (`None`), like vanilla's `L` key handler.
    /// The tab we announce is also remembered locally so the overlay can
    /// highlight it even before the server echoes a select packet.
    pub fn seen_advancements(&mut self, tab: Option<&str>) {
        if !matches!(self.status, SessionStatus::Playing) {
            return;
        }
        if let Some(tab) = tab {
            self.advancements_tab = Some(tab.to_owned());
        }
        let _ = self.outgoing.try_send(WorkerCommand::SeenAdvancements {
            tab: tab.map(str::to_owned),
        });
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
        self.crafter_disabled_slots
            .retain(|(id, _), _| *id != window_id);
        let _ = self
            .outgoing
            .try_send(WorkerCommand::ContainerClose { window_id });
    }

    /// Queue the anvil rename text (`0x2a`); protocol 767 accepts at most
    /// 150 Unicode scalar values.
    pub fn rename_item(&self, name: &str) -> bool {
        let bounded: String = name.chars().take(150).collect();
        self.outgoing
            .try_send(WorkerCommand::RenameItem(bounded))
            .is_ok()
    }

    /// Queue a merchant offer selection (`0x2f`). Negative indices are
    /// rejected because they cannot identify a row in the open offer list.
    pub fn select_trade(&self, index: i32) -> bool {
        index >= 0
            && self
                .outgoing
                .try_send(WorkerCommand::SelectTrade { index })
                .is_ok()
    }

    /// Queue beacon effect IDs (`0x2e` in Protocol 767), preserving the
    /// protocol's optional primary and secondary effect representation.
    pub fn set_beacon(&self, primary: Option<i32>, secondary: Option<i32>) -> bool {
        self.outgoing
            .try_send(WorkerCommand::SetBeacon { primary, secondary })
            .is_ok()
    }

    /// Dispatch one typed workstation action to its Protocol 767 packet
    /// emitter. The UI remains responsible for producing a valid action;
    /// each packet method still enforces its own wire bounds.
    pub fn dispatch_workstation_action(
        &self,
        action: &megu_ui::container_screen::WorkstationAction,
    ) -> bool {
        use megu_ui::container_screen::WorkstationAction;
        match action {
            WorkstationAction::RenameItem(text) => self.rename_item(text),
            WorkstationAction::SetBeacon { primary, secondary } => {
                self.set_beacon(*primary, *secondary)
            }
            WorkstationAction::SelectTrade(index) => self.select_trade(*index),
            WorkstationAction::ClickButton(button_id) => {
                let Some(container_id) = self.open_screen.as_ref().map(|screen| screen.window_id)
                else {
                    return false;
                };
                self.outgoing
                    .try_send(WorkerCommand::ContainerButtonClick {
                        container_id,
                        button_id: *button_id,
                    })
                    .is_ok()
            }
            WorkstationAction::ToggleCrafterSlot { slot, disabled } => {
                if !(0..9).contains(slot) {
                    return false;
                }
                let Some(window_id) = self.open_screen.as_ref().map(|screen| screen.window_id)
                else {
                    return false;
                };
                // Protocol 767 `container_slot_state_changed` (`0x10`): `state = true` enables the slot.
                self.outgoing
                    .try_send(WorkerCommand::ContainerSlotStateChanged {
                        slot_id: *slot,
                        window_id,
                        state: !*disabled,
                    })
                    .is_ok()
            }
        }
    }

    /// Queue one crafter disabled-state transition (`0x10`). Only grid
    /// slots 0..=8 are valid in the 1.21.1 menu.
    pub fn set_crafter_slot_disabled(
        &mut self,
        window_id: u8,
        slot_id: i32,
        disabled: bool,
    ) -> bool {
        if !(0..9).contains(&slot_id) {
            return false;
        }
        if self
            .outgoing
            .try_send(WorkerCommand::ContainerSlotStateChanged {
                slot_id,
                window_id: i32::from(window_id),
                state: !disabled,
            })
            .is_err()
        {
            return false;
        }
        self.crafter_disabled_slots
            .insert((window_id, slot_id), disabled);
        true
    }

    /// Toggle and send one crafter grid slot; returns the new disabled state
    /// only when the worker accepted the packet.
    pub fn toggle_crafter_slot(&mut self, window_id: u8, slot_id: i32) -> Option<bool> {
        if !(0..9).contains(&slot_id) {
            return None;
        }
        let next = !self
            .crafter_disabled_slots
            .get(&(window_id, slot_id))
            .copied()
            .unwrap_or(false);
        self.set_crafter_slot_disabled(window_id, slot_id, next)
            .then_some(next)
    }

    /// `place_recipe` — the recipe browser `+` / recipe-book grid-fill request for
    /// `recipe` in window `window_id` (`0` is the window-0 player
    /// inventory's 2×2 grid). `make_all` is vanilla's shift-click flag
    /// (fill as many times as ingredients allow).
    ///
    /// Every request is recorded in [`Self::place_recipe_requests`] (the
    /// bounded UI-visible log that proves the dispatch path executed);
    /// the return value is whether the worker queue actually accepted it.
    /// Requests while not `Playing` are refused with `false` and not logged.
    pub fn send_place_recipe(&mut self, window_id: u8, recipe: &str, make_all: bool) -> bool {
        if !matches!(self.status, SessionStatus::Playing) {
            return false;
        }
        if self.place_recipe_requests.len() < 64 {
            self.place_recipe_requests
                .push((window_id, recipe.to_owned(), make_all));
        }
        self.outgoing
            .try_send(WorkerCommand::PlaceRecipe {
                window_id,
                recipe: recipe.to_owned(),
                make_all,
            })
            .is_ok()
    }

    /// Take the validated `transfer` reconnect target, if one arrived.
    pub fn take_pending_transfer(&mut self) -> Option<TransferTarget> {
        self.pending_transfer.take()
    }

    /// Take the pending `open_sign_editor` request the window turns into
    /// a `UiState::sign_editor` session: `(block_pos, is_front)`.
    pub fn take_pending_sign_editor(&mut self) -> Option<((i32, i32, i32), bool)> {
        self.pending_sign_editor.take()
    }

    /// Drain pending `open_book` hand instructions in arrival order. The
    /// window resolves each against the live inventory because the packet
    /// itself carries only the hand enum.
    pub fn drain_open_book_hands(&mut self) -> Vec<u8> {
        self.pending_open_book_hands.drain(..).collect()
    }

    /// Queue the sign-edit screen's `sign_update` packet: packed block
    /// position + front/back flag + the four lines, encoded by the
    /// worker and sent via `send_raw`. Returns whether the worker queue
    /// accepted the command.
    pub fn send_update_sign(
        &self,
        pos: (i32, i32, i32),
        is_front: bool,
        lines: [String; 4],
    ) -> bool {
        self.outgoing
            .try_send(WorkerCommand::UpdateSign {
                pos,
                is_front,
                lines,
            })
            .is_ok()
    }

    /// The classified rig for a tracked entity (set on `add_entity`).
    pub fn entity_rig(&self, entity_id: i32) -> Option<&EntityRigSpec> {
        self.entity_rigs.get(&entity_id)
    }

    /// Main-hand item id a tracked entity holds (equipment slot 0) —
    /// `Some` lets rigged holders (the fox's mouth slot) rebuild their
    /// mesh on pick-up/drop without a pose change.
    pub fn entity_mainhand_item_id(&self, eid: i32) -> Option<i32> {
        self.entity_equipment
            .get(&(eid, 0))
            .map(|stack| stack.item_id)
    }

    /// Iterate renderable live entities — every tracked entity that has a
    /// rig classification — as `(entity_id, rig spec, world feet position,
    /// tracked packet state)`. Positions prefer the `entity_positions`
    /// mirror the audio listener uses, falling back to the entity
    /// tracker's own accumulated `pos`. `tracked` is `None` only for a
    /// rigged entity the tracker has not mirrored yet (or already
    /// dropped); callers render it with the default pose.
    pub fn entity_render_entries(
        &self,
    ) -> impl Iterator<
        Item = (
            i32,
            EntityRigSpec,
            [f64; 3],
            Option<&megu_client::world::TrackedEntity>,
        ),
    > + '_ {
        self.entity_rigs.iter().filter_map(|(eid, spec)| {
            let pos = self
                .entity_positions
                .get(eid)
                .copied()
                .or_else(|| self.world.entities.get(eid).map(|tracked| tracked.pos))?;
            Some((*eid, *spec, pos, self.world.entities.get(eid)))
        })
    }

    /// Local-movement prediction against live world collision: mirrors
    /// `sim.rs`'s shared-physics step so the GPU session gets vanilla
    /// collision, step-up, fluid drag, and sneak edge protection
    /// (`step_player_physics` -> `back_off_from_edge`).
    /// Uses Minecraft's left-positive strafe convention.
    #[allow(clippy::too_many_arguments)]
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
        if !matches!(self.status, SessionStatus::Playing) || self.death.is_dead() {
            return None;
        }
        let base = self.pose.unwrap_or(PlayerPose {
            feet: self.physics_state.pos,
            yaw,
            pitch,
        });
        self.physics_state.pos = base.feet;
        let bounds = self.physics_state.aabb();
        let margin = self.physics_state.vel.map(|velocity| velocity.abs() + 1.0);
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
                [
                    feet[0],
                    (base.feet[1] - 0.500_000_1).floor() as i32,
                    feet[2],
                ],
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
        self.crosshair_entity_hit()
            .map(|hit| (hit.entity_id, hit.distance))
    }

    /// Full [`EntityRayHit`] behind [`Self::crosshair_entity`] — the
    /// `qol_gameplay` looking-at projection needs the hit record, not
    /// just the id/distance pair.
    fn crosshair_entity_hit(&self) -> Option<EntityRayHit> {
        let pose = self.render_frame_pose()?;
        let eye = pose.eye;
        let direction = pose.look;
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
    }

    /// Block under the crosshair ray (`raycast_blocks`) using the live pose's
    /// eye height, per-state selection geometry from the block-shape table,
    /// and bucket-aware source-fluid clipping. Shaped blocks therefore expose
    /// their empty space correctly — for example a ray through the empty upper
    /// half of a bottom slab continues to the block behind it.
    pub fn crosshair_block(&self) -> Option<([i32; 3], f64)> {
        self.crosshair_block_hit()
            .map(|hit| (hit.block_pos, hit.distance))
    }

    /// Full [`RayHit`] behind [`Self::crosshair_block`] — the
    /// `qol_gameplay` looking-at projection consumes the face and
    /// cursor-in-block detail too.
    fn crosshair_block_hit(&self) -> Option<RayHit> {
        let pose = self.render_frame_pose()?;
        let eye = pose.eye;
        let direction = pose.look;
        let reach = if self.hud.game_mode == 1 { 5.0 } else { 4.5 };
        let fluid_mode = self.selection_fluid_clip_mode();
        raycast_blocks(eye, direction, reach, fluid_mode, |x, y, z| {
            match self.world.get_block_state(x, y, z) {
                Some(0) | None => RayBlockShapes::default(),
                Some(state) => {
                    let mut world_boxes = Vec::new();
                    self.world
                        .blocks
                        .push_collision_aabbs(state, x, y, z, &mut world_boxes);
                    let mut boxes: Vec<Aabb> = world_boxes
                        .into_iter()
                        .filter_map(|bounds| {
                            // Collision bounds can exceed the voxel vertically
                            // (notably fences/walls). Selection outlines remain
                            // clipped to the voxel cell even when the collision
                            // shape extends upward for entity movement.
                            let min = [
                                bounds.min[0] - f64::from(x),
                                bounds.min[1] - f64::from(y),
                                bounds.min[2] - f64::from(z),
                            ];
                            let max = [
                                bounds.max[0] - f64::from(x),
                                bounds.max[1] - f64::from(y),
                                bounds.max[2] - f64::from(z),
                            ];
                            let min = min.map(|v| v.clamp(0.0, 1.0));
                            let max = max.map(|v| v.clamp(0.0, 1.0));
                            (min[0] < max[0] && min[1] < max[1] && min[2] < max[2])
                                .then_some(Aabb { min, max })
                        })
                        .collect();
                    if boxes.is_empty() {
                        boxes = selection_fallback_boxes(
                            state_block_name(&self.world, self.mod_registry.as_ref(), state)
                                .unwrap_or(""),
                        );
                    }
                    let fluid = self.world.fluid_cell_at(x, y, z);
                    RayBlockShapes {
                        boxes,
                        is_fluid: fluid.kind.is_some(),
                        is_fluid_source: fluid.kind.is_some()
                            && fluid.amount == 8
                            && !fluid.falling,
                    }
                }
            }
        })
    }

    /// Vanilla fluid clipping for the held item: empty buckets target liquid
    /// source cells; ordinary blocks/tools ignore fluids. Filled buckets and
    /// milk do not re-pick their liquid.
    fn selection_fluid_clip_mode(&self) -> FluidClipMode {
        let hotbar_slot = 36 + self.hud.selected_slot as i32;
        let Some(stack) = self.player_inventory.get(&hotbar_slot) else {
            return FluidClipMode::None;
        };
        let item = crate::recipe_ingest::item_name_for_id(stack.item_id as u32);
        let path = item.rsplit(':').next().unwrap_or(&item);
        if path == "bucket" {
            FluidClipMode::SourceOnly
        } else {
            FluidClipMode::None
        }
    }

    /// Refresh the crosshair targets plus the `qol_gameplay`
    /// looking-at HUD projection for this frame.
    fn refresh_looking_at(&mut self) {
        let entity_hit = self.crosshair_entity_hit();
        let block_hit = self.crosshair_block_hit();
        self.looking_at_entity = entity_hit.map(|hit| hit.entity_id);
        self.looking_at_block =
            block_hit.map(|hit| (hit.block_pos, self.block_name_at(hit.block_pos)));
        self.looking_at_hud = self.evaluate_looking_at(entity_hit, block_hit);
    }

    /// Run `megu_kernel::qol_gameplay::evaluate_looking_at_hud` over the
    /// live crosshair target — the entity wins only when strictly nearer,
    /// matching the vanilla pick order. Block detail lines carry the real
    /// break-progress fraction when a tracked destruction covers the
    /// block; harvest-tool hints come from the registry id's material
    /// family, and crop/container/furnace lines stay absent while the
    /// session does not mirror those properties.
    fn evaluate_looking_at(
        &self,
        entity_hit: Option<EntityRayHit>,
        block_hit: Option<RayHit>,
    ) -> Option<LookingAtHudInfo> {
        let entity_first = match (entity_hit, block_hit) {
            (Some(entity), Some(block)) => entity.distance < block.distance,
            (Some(_), None) => true,
            _ => false,
        };
        if entity_first {
            let hit = entity_hit?;
            let tracked = self.world.entities.get(&hit.entity_id)?;
            let type_id = format!("minecraft:entity_{}", tracked.entity_type);
            return evaluate_looking_at_hud(
                LookingAtTarget::Entity(LookingAtEntity {
                    hit,
                    entity_type_id: &type_id,
                    display_name: None,
                    health: None,
                    max_health: None,
                    armor: None,
                    owner: None,
                    tamed: None,
                    mod_namespace: None,
                }),
                None,
            );
        }
        let hit = block_hit?;
        let name = self.block_name_at(hit.block_pos);
        let break_progress = self
            .world
            .block_destructions
            .values()
            .find(|progress| progress.pos == hit.block_pos)
            .map(|progress| f32::from(progress.stage.min(10)) / 10.0);
        evaluate_looking_at_hud(
            LookingAtTarget::Block(LookingAtBlock {
                hit,
                block_id: &name,
                display_name: None,
                properties: &[],
                harvestable: None,
                preferred_tool: preferred_harvest_tool(&name),
                break_progress,
                crop_growth: None,
                container_fill: None,
                furnace_progress: None,
            }),
            None,
        )
    }

    /// Registry name for the block state at `pos` (e.g. `minecraft:oak_log`),
    /// falling back to the raw state id when the pack registry is absent.
    /// Dynamic mod state ids resolve their real `namespace:block` through
    /// the synced `MRS1` registry.
    fn block_name_at(&self, pos: [i32; 3]) -> String {
        self.world
            .get_block_state(pos[0], pos[1], pos[2])
            .map(|state| {
                state_block_name(&self.world, self.mod_registry.as_ref(), state)
                    .map(str::to_string)
                    .unwrap_or_else(|| format!("minecraft:state_{state}"))
            })
            .unwrap_or_else(|| "minecraft:air".to_string())
    }

    fn refresh_prefetcher(&mut self) {
        let Some(pose) = self.pose else {
            return;
        };
        // `set_center_chunk` wins over the pose: the server's view
        // center is the authoritative anchor while it differs from the
        // local position (vehicle seats, forced spectate, teleport lag).
        let (anchor_x, anchor_z) = self
            .view_center_chunk
            .map(|(cx, cz)| (f64::from(cx) * 16.0 + 8.0, f64::from(cz) * 16.0 + 8.0))
            .unwrap_or((pose.feet[0], pose.feet[2]));
        self.prefetch_arrival_ring.clear();
        let delta = self.last_motion_delta;
        let jump = horizontal_teleport_jump(delta);
        let fresh_jump = jump && self.prefetch_last_jump_delta != Some(delta);
        if fresh_jump {
            // A teleport invalidates every forward-cone bet in one step. Emit
            // a full 360-degree arrival ring for this refresh only; subsequent
            // refreshes resume the speed-scaled directional lookahead below.
            self.prefetcher = SpeculativeChunkPrefetcher::new();
            self.prefetch_last_jump_delta = Some(delta);
            let base_cx = (anchor_x / 16.0)
                .floor()
                .clamp(i32::MIN as f64, i32::MAX as f64) as i32;
            let base_cz = (anchor_z / 16.0)
                .floor()
                .clamp(i32::MIN as f64, i32::MAX as f64) as i32;
            let boost = PREFETCH_MAX_CHUNKS as f32 * PREFETCH_BOOST_BLOCKS_PER_RANK;
            let columns = &self.world.columns;
            for dz in -1..=1 {
                for dx in -1..=1 {
                    if dx == 0 && dz == 0 {
                        continue;
                    }
                    let cx = base_cx.saturating_add(dx);
                    let cz = base_cz.saturating_add(dz);
                    if !columns.contains_key(&(cx, cz)) {
                        self.prefetch_arrival_ring.insert((cx, cz), boost);
                    }
                }
            }
            return;
        }
        if !jump {
            self.prefetch_last_jump_delta = None;
        }
        let (view_distance, max_chunks) = velocity_scaled_prefetch_budget(delta);
        let columns = &self.world.columns;
        self.prefetcher.update(
            anchor_x,
            anchor_z,
            delta[0],
            delta[1],
            view_distance,
            max_chunks,
            |cx, cz| columns.contains_key(&(cx, cz)),
        );
    }

    /// Pick the kernel mode this poll dispatches under: the autotuner's
    /// `selected` on ordinary polls, the alternate mode once every
    /// [`KERNEL_PROBE_INTERVAL_POLLS`] polls. The challenger probe is
    /// what lets `observe_sample` enter its challenger-evaluation block
    /// at all — feeding it only `selected_mode()` kept every challenger
    /// statistic at zero samples forever. The dispatched mode is
    /// recorded in `executed_kernel_mode` so the frame-time feed in
    /// `record_frame_and_refresh_compact_debug_hud` credits the same arm the poll
    /// ran.
    fn dispatch_kernel_mode(&mut self) -> ComputeKernelMode {
        self.kernel_probe_polls = self.kernel_probe_polls.wrapping_add(1);
        let mode = if self.kernel_probe_polls >= KERNEL_PROBE_INTERVAL_POLLS {
            self.kernel_probe_polls = 0;
            self.kernel_probe_count = self.kernel_probe_count.wrapping_add(1);
            match self.kernel_autotuner.selected_mode() {
                ComputeKernelMode::LutBatch => ComputeKernelMode::RecomputeSwar,
                ComputeKernelMode::RecomputeSwar => ComputeKernelMode::LutBatch,
            }
        } else {
            self.kernel_autotuner.selected_mode()
        };
        self.executed_kernel_mode = mode;
        mode
    }

    /// Pending [`SpeculativeChunkPrefetcher`] bets as
    /// `(chunk_x, chunk_z) -> camera-distance boost in blocks`, the
    /// consumption side of `prefetcher.entries()` the dirty-section
    /// drain applies: sections on the predicted movement heading read
    /// up to `PREFETCH_MAX_CHUNKS * PREFETCH_BOOST_BLOCKS_PER_RANK`
    /// blocks closer in the queue ordering, scaled by the entry's own
    /// priority rank. `CanceledStale` entries and duplicate bets resolve
    /// to the strongest surviving boost.
    fn prefetch_boost_map(&self) -> BTreeMap<(i32, i32), f32> {
        let mut boosts: BTreeMap<(i32, i32), f32> = BTreeMap::new();
        for entry in self.prefetcher.entries() {
            if !matches!(entry.state, SpeculativeState::Pending) {
                continue;
            }
            let rank = entry.priority.min(PREFETCH_MAX_CHUNKS as u32) as usize;
            let boost = (PREFETCH_MAX_CHUNKS - rank) as f32 * PREFETCH_BOOST_BLOCKS_PER_RANK;
            boosts
                .entry((entry.cx, entry.cz))
                .and_modify(|b| *b = b.max(boost))
                .or_insert(boost);
        }
        for (&coord, &boost) in &self.prefetch_arrival_ring {
            boosts
                .entry(coord)
                .and_modify(|b| *b = b.max(boost))
                .or_insert(boost);
        }
        boosts
    }

    fn update_audio_listener(&mut self) {
        if let Some(pose) = self.pose {
            // Dedicated mix thread: stream the pose over the lock-free
            // ring instead of touching the caller-side mixer. The
            // synchronous call stays the headless/unit-test fallback.
            let sent = self.audio.submit_cmd_nonblocking(AudioCmd::UpdateListener {
                pos: [
                    pose.feet[0] as f32,
                    (pose.feet[1] + 1.62) as f32,
                    pose.feet[2] as f32,
                ],
                vel: [0.0; 3],
                yaw_deg: pose.yaw,
                pitch_deg: pose.pitch,
                submerged: self.listener_submerged,
            });
            if !sent {
                self.audio
                    .update_listener(pose.feet, Some(pose.yaw), Some(pose.pitch));
            }
            if let Some(entity_id) = self.player_entity_id {
                self.entity_positions.insert(entity_id, pose.feet);
            }
        }
    }

    /// Compile the asset bundle once. Layer order is lowest-to-highest
    /// priority (last layer wins): the vanilla client JAR at
    /// `<gameDir>/versions/<version>/<version>.jar` first, then `mods/*.jar`, then every
    /// `options.txt`-configured pack from `resourcepacks/` (dir or `.zip`),
    /// then the explicit `MEGU_RESOURCE_PACK_DIR` override.
    /// `CompiledAssetBundle::build` produces the gutter-padded block atlas
    /// and its linear-sRGB mip chain; `window.rs` uploads it via
    /// `upload_block_atlas`. What was resolved (and skipped) lands in
    /// [`Self::asset_discovery`].
    /// Attach the official baked block-model catalog to both mesh lanes.
    /// `sprite_lookup` is the atlas texture-name table used only to certify
    /// greedy-compatible full-cube materials; exact shaped-face texture indices
    /// are resolved by the renderer from the same atlas names at upload time.
    pub fn attach_block_model_catalog(
        &mut self,
        catalog: Arc<VanillaBlockModelCatalog>,
        sprite_lookup: Arc<BTreeMap<String, u32>>,
    ) {
        self.world.set_block_model_catalog(
            Arc::clone(&catalog),
            sprite_lookup_closure(Arc::clone(&sprite_lookup)),
        );
        let state_names: Arc<[String]> = Arc::from(self.world.blocks.state_names().to_vec());
        if let Some(workers) = self.section_mesh_workers.as_mut() {
            workers.set_block_model_catalog_with_sprite_lookup(
                Some(Arc::clone(&catalog)),
                state_names,
                Some(sprite_lookup_closure(Arc::clone(&sprite_lookup))),
            );
        }
        self.block_model_catalog = Some(catalog);
        self.block_model_sprite_lookup = Some(sprite_lookup);
    }

    fn ensure_asset_bundle(&mut self) {
        if self.asset_bundle_tried {
            return;
        }
        self.asset_bundle_tried = true;
        let (layers, discovery, cem_dirs) = discover_asset_layers(&self.session_paths);
        if discovery.jar_path.is_none()
            && discovery.pack_sources.is_empty()
            && discovery.notes.is_empty()
        {
            // Nothing configured at all — historical env-var-only behaviour.
        } else {
            for note in &discovery.notes {
                eprintln!("megu-client assets: {note}");
            }
        }
        self.asset_discovery = discovery;
        if layers.is_empty() {
            return;
        }
        // The `layers` sprites are dropped after `split_compiled_bundle`
        // (VRAM owns the texels) — the UI icon atlas is packed here while
        // the decoded `textures/{item,block}` payloads are still alive.
        self.item_icon_atlas = Some(crate::recipe_ingest::build_item_icon_atlas(&layers));
        for dir in &cem_dirs {
            self.register_cem_models(dir);
        }
        let block_model_catalog = block_model_catalog_from_layers(&layers);
        match CompiledAssetBundle::build(&layers) {
            Ok(bundle) => {
                if !bundle.skipped_errors.is_empty() {
                    eprintln!(
                        "megu-client assets: {} pack entries skipped",
                        bundle.skipped_errors.len()
                    );
                }
                let (meta, pixels) = split_compiled_bundle(bundle);
                if block_model_catalog.loaded_blockstates() != 0 {
                    let sprite_lookup = Arc::new(
                        meta.placements
                            .keys()
                            .enumerate()
                            .map(|(index, name)| (name.clone(), index as u32))
                            .collect::<BTreeMap<_, _>>(),
                    );
                    self.attach_block_model_catalog(Arc::new(block_model_catalog), sprite_lookup);
                }
                self.atlas_meta = Some(meta);
                self.pending_atlas_pixels = Some(pixels);
                self.asset_bundle_pending_upload = true;
            }
            Err(err) => eprintln!("megu-client assets: bundle build failed: {err}"),
        }
    }

    /// Consumes any pending atlas pixel payloads by uploading them through
    /// `renderer`, then drops the CPU copies so VRAM holds the sole copy of
    /// the atlas texels (RAM/VRAM role separation: no resident duplicate).
    /// [`Self::atlas_meta`] stays intact in both success and failure — a
    /// failed upload keeps no pixels either and would re-derive them only
    /// through [`Self::request_atlas_reload`]. Returns `true` once no CPU
    /// atlas bytes remain resident.
    pub fn upload_pending_atlas(&mut self, renderer: &mut GpuRenderer) -> bool {
        let Some(pixels) = self.pending_atlas_pixels.take() else {
            self.asset_bundle_pending_upload = false;
            return true;
        };
        match renderer.upload_block_atlas(&pixels.atlas, &pixels.mipmaps) {
            Ok(()) => {
                if let Some(meta) = &self.atlas_meta {
                    let assignments = state_sprite_assignments(
                        &self.world,
                        self.mod_registry.as_ref(),
                        meta,
                        renderer,
                    );
                    let refs: Vec<_> = assignments
                        .iter()
                        .map(|(id, sprites, grass)| {
                            (*id, sprites.each_ref().map(Option::as_deref), *grass)
                        })
                        .collect();
                    if let Err(err) = renderer.set_block_state_face_sprites(&refs) {
                        eprintln!("megu-client assets: state sprite table failed: {err}");
                    }
                    let animated =
                        renderer.set_atlas_animations(&meta.animations, self.atlas_anim_tick());
                    eprintln!(
                        "megu-client assets: {} state sprites bound, {animated} animated",
                        assignments.len()
                    );
                }
            }
            Err(err) => eprintln!("megu-client assets: atlas upload failed: {err}"),
        }
        self.asset_bundle_pending_upload = false;
        self.pending_atlas_pixels.is_none()
    }

    /// Arms an on-demand atlas rebuild + upload: a `SurfaceError::Lost`
    /// calls this so the next [`Self::ensure_asset_bundle`] pass
    /// recompiles the bundle and re-uploads the pixel payloads, instead of
    /// keeping them RAM-resident for a device loss that may never come.
    pub fn request_atlas_reload(&mut self) {
        if self.atlas_meta.is_none() {
            return;
        }
        self.asset_bundle_tried = false;
        self.asset_bundle_pending_upload = true;
    }

    /// Re-arm asset discovery after an external change — e.g. the vanilla
    /// JAR bootstrap finishing its download into Megu's cache. Unlike
    /// [`Self::request_atlas_reload`] this also fires when no bundle was
    /// ever built (`atlas_meta` still `None` because discovery ran before
    /// the JAR existed), so the next `ensure_asset_bundle` pass re-resolves
    /// every layer and newly discovered textures upload seamlessly.
    pub fn refresh_asset_layers(&mut self) {
        self.asset_bundle_tried = false;
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
                    eprintln!("megu-client assets: CEM model {path:?} skipped: {err:?}");
                }
            }
        }
    }

    /// Component-preserving hotbar snapshot parallel to the legacy
    /// string-only [`HudState::hotbar`]. Slots follow window-0 indices 36..45.
    pub fn live_hotbar_snapshot(&self) -> [Option<LiveHotbarStack>; 9] {
        std::array::from_fn(|index| {
            self.player_inventory
                .get(&(36 + index as i32))
                .and_then(live_stack_summary)
        })
    }

    /// Component-preserving offhand stack summary (window-0 slot 45).
    pub fn live_offhand_snapshot(&self) -> Option<LiveHotbarStack> {
        self.player_inventory.get(&45).and_then(live_stack_summary)
    }

    /// Typed boss-bar rows with title/fill plus color, division and flags.
    pub fn live_bossbar_snapshot(&self) -> Vec<LiveBossBarView> {
        self.boss_bars
            .values()
            .map(|bar| LiveBossBarView {
                title: bar.title.clone(),
                health: bar.health.clamp(0.0, 1.0),
                color: bar.color,
                divisions: bar.divisions,
                flags: bar.flags,
            })
            .collect()
    }

    /// Typed local-player status effects, preserving duration, amplifier,
    /// ambient/particle/icon flags, and beneficial classification.
    pub fn live_status_effect_snapshot(&self) -> Vec<LiveStatusEffectView> {
        let Some(eid) = self.player_entity_id else {
            return Vec::new();
        };
        self.mob_effects
            .get(&eid)
            .into_iter()
            .flat_map(|effects| effects.iter())
            .map(|(id, effect)| LiveStatusEffectView {
                effect_id: *id,
                name: status_effect_name(*id).to_string(),
                amplifier: effect.amplifier,
                duration_ticks: effect.duration,
                ambient: effect.flags & 0x01 != 0,
                show_particles: effect.flags & 0x02 != 0,
                show_icon: effect.flags & 0x04 != 0,
                beneficial: matches!(
                    *id,
                    0 | 2
                        | 4
                        | 5
                        | 7
                        | 9
                        | 10
                        | 11
                        | 12
                        | 13
                        | 15
                        | 20
                        | 21
                        | 22
                        | 25
                        | 27
                        | 28
                        | 29
                        | 31
                ),
            })
            .collect()
    }

    /// Snapshot the server-driven HUD channels for `GpuApp::sync_live_hud`.
    pub fn hud_feed(&self) -> LiveHudFeed {
        LiveHudFeed {
            hotbar_stacks: self.live_hotbar_snapshot(),
            offhand_stack: self.live_offhand_snapshot(),
            bossbars: self.live_bossbar_snapshot(),
            status_effects: self.live_status_effect_snapshot(),
            scoreboard: self.scoreboard.clone(),
            title_revision: self.title_revision,
            title_times: self.title_times,
            title_text: self.title_text.clone(),
            subtitle_text: self.subtitle_text.clone(),
            looking_at_block: self.looking_at_block.clone(),
        }
    }

    /// Regenerate the layered voxel-cloud sheet from the live camera/world
    /// time and mirror it into `subworlds` for the translucent lit pass.
    /// Cells come from [`generate_layered_voxel_clouds_with_atmosphere`]:
    /// skylight-bearing dimensions with a non-vacuum atmosphere (the
    /// `DimensionAdaptationPolicy::enable_voxel_clouds` intent, resolved
    /// here through `dimension.has_skylight` + the blender's air-density
    /// proxy) emit cells, while Nether/End/vacuum-orbit produce an empty
    /// set — the pack's `enabled`/`layered_voxel_clouds` toggles gate on
    /// top of that (an empty set removes the sheet instead of leaving a
    /// stale frame).
    /// The mesh is cached per deck on the integer cell grid `(cloud_cx,
    /// cloud_cz, scroll_cell, cloud_layers, cloud_height)`:
    /// `generate` bakes `cx*cell - scroll` into `min.x`, so feeding the
    /// cell-aligned canonical tick produces a mesh whose only drift
    /// inside one cell is the sub-cell X remainder — applied to the
    /// existing `SubWorld` model matrix via `set_transform` instead of a
    /// geometry rebuild. `cloud_mesh_reuses` counts the matrix-only
    /// slides. `CloudDeckMode::Hybrid` keeps three decks alive (each
    /// with its own parallax scroll, gentle bob and `MAT_FLAG_CLOUD`
    /// hybrid optics). The cubic deck is the fallback when Vanilla cloud
    /// pixels are unavailable. Cloudless dimensions and `Off` use no deck.
    pub fn refresh_voxel_clouds(&mut self) {
        // The gate `generate_layered_voxel_clouds_with_atmosphere`
        // applies (`allow_clouds` + `density >= MIN_SKY_ATMOSPHERE`) plus
        // the deck-mode switch: a closed gate or `Off` removes every
        // deck — Nether/End/vacuum and orbit-ascent density dips can
        // never matrix-slide a stale sheet.
        if self.cloud_deck_mode == CloudDeckMode::Off
            || !self.clouds_open()
            || self.uses_textured_cloud_deck()
        {
            self.remove_cloud_decks();
            return;
        }
        let (cam_x, cam_z) = self
            .pose
            .map(|pose| (pose.feet[0] as f32, pose.feet[2] as f32))
            .unwrap_or((0.0, 0.0));
        let time = self.world.time_of_day.max(0) as u64;
        let cell = f64::from(CLOUD_CELL_BLOCKS);
        let scroll_rate = f64::from(CLOUD_SCROLL_BLOCKS_PER_TICK);
        let scroll = time as f64 * scroll_rate;
        let cam_x = if cam_x.is_finite() { cam_x } else { 0.0 };
        let cam_z = if cam_z.is_finite() { cam_z } else { 0.0 };
        let cloud_cz = (f64::from(cam_z) / cell).floor() as i64;
        // Cloned so per-deck `self` mutation inside the loop can borrow
        // mutably while generation re-reads the config.
        let nature = self.vanilla_plus_cfg.nature.clone();
        let cloud_layers = u32::from(nature.effective_cloud_layers());
        let cloud_height = nature.effective_cloud_height().to_bits();
        let density = self.tactile_atmosphere_density();
        let has_skylight = self.world.dimension.has_skylight;
        // Vanilla celestial angle: 0 = sunrise, 6000 = noon, 18000 = midnight.
        let sun_elevation = ((time % 24_000) as f32 * (std::f32::consts::TAU / 24_000.0)).sin();
        let lighting = compute_cloud_lighting(
            sun_elevation,
            self.world.rain_level,
            self.world.thunder_level,
        );
        let lighting_key = cloud_lighting_key(&lighting);
        let hybrid = self.cloud_deck_mode == CloudDeckMode::Hybrid;
        // World-time seconds drive the gentle bob/sway phases.
        let time_sec = time as f32 / 20.0;
        for deck in 0..CLOUD_DECK_IDS.len() {
            let spec = cloud_deck_spec(deck);
            if !hybrid && deck > 0 {
                self.deactivate_cloud_deck(deck);
                continue;
            }
            // Per-deck parallax: each hybrid deck scrolls at its own
            // wind multiplier, so its canonical cell alignment (and mesh
            // cache key) drifts independently — zero mesh rebuilds on
            // sub-cell ticks.
            let deck_scroll = scroll * f64::from(spec.wind_multiplier);
            let scroll_cell = (deck_scroll / cell).floor() as i64;
            // Canonical tick whose baked scroll lands on the cell grid:
            // `generate` computes `min.x = cx*cell - ticks*rate`, so this
            // tick makes the cached mesh carry only whole-cell offsets.
            let canon_ticks = ((scroll_cell as f64 * cell) / scroll_rate)
                .round()
                .clamp(0.0, u64::MAX as f64) as u64;
            let canon_scroll = canon_ticks as f64 * scroll_rate;
            let cloud_cx = ((f64::from(cam_x) + canon_scroll) / cell).floor() as i64;
            let key = (cloud_cx, cloud_cz, scroll_cell, cloud_layers, cloud_height);
            // Sub-cell drift this frame: the model matrix slides the
            // cached mesh by the fraction of a cell scrolled since the
            // last boundary. Hybrid decks add a bounded sine bob + sway.
            let slide_x = -((deck_scroll - canon_scroll) as f32);
            let (lift_y, sway_z) = if hybrid {
                (
                    spec.y_offset
                        + spec.bob_amplitude * (time_sec * spec.bob_freq + spec.phase).sin(),
                    spec.sway_amplitude * (time_sec * spec.sway_freq + spec.phase * 1.7).sin(),
                )
            } else {
                (0.0, 0.0)
            };
            let transform = SubWorldTransform {
                position: [slide_x, lift_y, sway_z],
                ..SubWorldTransform::IDENTITY
            };
            let cache = &self.cloud_deck_cache[deck];
            if cache.mesh_key == Some(key)
                && cache.lighting_key == Some(lighting_key)
                && self.subworlds.get(spec.id).is_some()
            {
                self.subworlds.set_transform(spec.id, transform);
                self.cloud_mesh_reuses += 1;
                continue;
            }
            let cells = generate_layered_voxel_clouds_with_atmosphere(
                &nature,
                cam_x,
                cam_z,
                canon_ticks,
                density,
                has_skylight,
            );
            if cells.is_empty() {
                self.subworlds.remove(spec.id);
                self.cloud_deck_cache[deck] = CloudDeckCache::default();
                continue;
            }
            let material_flags = if hybrid { MAT_FLAG_CLOUD } else { 0 };
            let (vertices, indices) =
                voxel_clouds_to_geometry_flagged(&cells, &lighting, material_flags);
            let mut entry = SubWorldRenderEntry::new(spec.id, transform, vertices, indices);
            entry.alpha = if hybrid {
                spec.alpha
            } else {
                CLOUD_RENDER_ALPHA
            };
            self.subworlds.upsert(entry);
            self.cloud_deck_cache[deck] = CloudDeckCache {
                mesh_key: Some(key),
                lighting_key: Some(lighting_key),
            };
        }
    }

    fn clouds_open(&self) -> bool {
        self.world.dimension.has_skylight && self.tactile_atmosphere_density() >= 0.15
    }

    pub(crate) fn uses_textured_cloud_deck(&self) -> bool {
        self.cloud_deck_mode == CloudDeckMode::Vanilla
            && self.has_vanilla_cloud_texture
            && self.clouds_open()
    }

    /// Drop one cloud deck's subworld and invalidate its mesh cache.
    fn deactivate_cloud_deck(&mut self, deck: usize) {
        self.subworlds.remove(CLOUD_DECK_IDS[deck]);
        self.cloud_deck_cache[deck] = CloudDeckCache::default();
    }

    /// Remove every cloud deck subworld and clear the per-deck mesh
    /// caches — `CloudDeckMode::Off`, skylight-less dimensions and
    /// dimension resets all route through here.
    fn remove_cloud_decks(&mut self) {
        for deck in 0..CLOUD_DECK_IDS.len() {
            self.deactivate_cloud_deck(deck);
        }
    }

    /// Mirror `self.hud` into `hud` only when it changed since the last
    /// call — server packets and local edits bump `hud_revision`, idle
    /// frames hit the revision gate and cost one counter increment
    /// (`hud_sync_skips`) instead of a full `HudState` clone.
    /// Returns `true` when the clone ran.
    pub fn sync_hud_to(&mut self, hud: &mut HudState) -> bool {
        if self.hud_revision == self.last_synced_hud_revision {
            self.hud_sync_skips += 1;
            return false;
        }
        let hud_hidden = hud.hud_hidden;
        *hud = self.hud.clone();
        hud.hud_hidden = hud_hidden;
        self.last_synced_hud_revision = self.hud_revision;
        true
    }

    /// Upload-key the shared mesh template for a kinetic `mesh_kind`:
    /// creates the [`InstancedPartBatch`] every [`KineticPartSpec`] with
    /// that kind instances against. Re-registering an existing kind is a
    /// no-op — the template geometry stays resident and in-flight
    /// instances are preserved.
    pub fn register_kinetic_template(
        &mut self,
        mesh_kind: u32,
        template_vertices: Vec<TerrainVertex>,
        template_indices: Vec<u32>,
    ) {
        if self
            .subworlds
            .batches()
            .any(|batch| batch.mesh_kind == mesh_kind)
        {
            return;
        }
        self.subworlds.upsert_batch(InstancedPartBatch::new(
            mesh_kind,
            template_vertices,
            template_indices,
        ));
    }

    /// Register one rotating kinetic part; returns the stable part id
    /// [`Self::remove_kinetic_part`] accepts. Records resolve on the next
    /// [`Self::sync_kinetic_network_rpm`].
    pub fn register_kinetic_part(&mut self, spec: KineticPartSpec) -> u32 {
        let id = self.next_kinetic_part_id;
        self.next_kinetic_part_id += 1;
        self.kinetic_parts.insert(id, spec);
        id
    }

    /// Drop a registered kinetic part. `false` for an unknown id.
    pub fn remove_kinetic_part(&mut self, id: u32) -> bool {
        self.kinetic_parts.remove(&id).is_some()
    }

    /// Registered kinetic part count (diagnostics/tests).
    pub fn kinetic_part_count(&self) -> usize {
        self.kinetic_parts.len()
    }

    /// Advance wire-synchronized parts by their individual RPM values.
    /// The shared-network API above retains its historical behavior.
    pub fn sync_kinetic_live_parts(&mut self, dt_secs: f32) {
        let dt = if dt_secs.is_finite() {
            dt_secs.max(0.0)
        } else {
            0.0
        };
        let active_dimension = self.current_kinetic_dimension();
        for ((dimension, pos), id) in &self.kinetic_pos_to_id {
            if *dimension != active_dimension {
                continue;
            }
            let Some(record) = self.kinetic_telemetry.get(pos) else {
                continue;
            };
            let angle = self.kinetic_live_angles.entry(*id).or_insert(0.0);
            *angle += record.speed_rpm * std::f32::consts::TAU / 60.0 * dt;
        }
        for records in self.kinetic_scratch.values_mut() {
            records.clear();
        }
        for (id, spec) in &self.kinetic_parts {
            let theta = self
                .kinetic_live_angles
                .get(id)
                .copied()
                .unwrap_or(spec.phase0);
            let euler = axis_angle_to_euler_xyz(spec.axis, theta);
            self.kinetic_scratch
                .entry(spec.mesh_kind)
                .or_default()
                .push(InstancedPartRecord {
                    pos_scale: [
                        spec.position[0],
                        spec.position[1],
                        spec.position[2],
                        spec.scale,
                    ],
                    rot_uv_frame: [euler[0], euler[1], euler[2], 0.0],
                    packed_light: spec.packed_light,
                    tint_rgba: spec.tint_rgba,
                    mesh_kind: spec.mesh_kind,
                    subworld_id: spec.subworld_id,
                });
        }
        let scratch = &mut self.kinetic_scratch;
        for batch in self.subworlds.batches_mut() {
            match scratch.get_mut(&batch.mesh_kind) {
                Some(records) if batch.instances != *records => {
                    std::mem::swap(&mut batch.instances, records);
                    batch.instances_dirty = true;
                }
                Some(_) => {}
                None if !batch.instances.is_empty() => {
                    batch.instances.clear();
                    batch.instances_dirty = true;
                }
                None => {}
            }
        }
    }

    /// Push the active UI locale (`settings.locale.code()`) into the
    /// session so mod display names resolve in the player's language.
    /// A change marks the translation table stale; the next lookup
    /// reloads it for the new locale.
    pub fn set_ui_locale(&mut self, locale: &str) {
        let normalized = locale.trim().replace('-', "_").to_ascii_lowercase();
        if normalized.is_empty() || normalized == self.ui_locale {
            return;
        }
        self.ui_locale = normalized;
        self.mod_translations = None;
        self.auto_translation_overlay = None;
    }

    /// Lazily load `ModTranslationTable` for `ui_locale` from the session
    /// `game_dir`'s mod JARs (bounded: `MAX_ARCHIVES`/`MAX_LANG_BYTES`
    /// inside the loader). Skips silently when no `game_dir` is set.
    fn ensure_mod_translations(&mut self) {
        if self
            .mod_translations
            .as_ref()
            .is_some_and(|table| table.locale() == self.ui_locale)
        {
            return;
        }
        let Some(game_dir) = self.session_paths.game_dir.clone() else {
            return;
        };
        let mut report = ModMetaReport::default();
        let table = ModTranslationTable::load(&game_dir, &self.ui_locale, &mut report);
        for warning in report.warnings {
            eprintln!("megu-client mod meta: {warning}");
        }
        let pack_enabled = configured_resource_pack_entries(&game_dir)
            .iter()
            .any(|pack| pack == crate::mod_translator::AUTO_TRANSLATION_PACK_DIR);
        let pack_root = self
            .session_paths
            .resource_pack_dir
            .clone()
            .unwrap_or_else(|| game_dir.join("resourcepacks"));
        self.auto_translation_overlay = if pack_enabled && self.ui_locale == "ja_jp" {
            crate::mod_translator::load_auto_translation_pack_ja_jp(&pack_root).ok()
        } else {
            None
        };
        self.mod_translations = Some(table);
    }

    /// Live block-inspector telemetry for the kinetic/machine part at `pos`.
    /// Dynamic mod block-state ids resolve their real `namespace:block`
    /// through the `MRS1` registry, and their `display_name` comes from
    /// the loaded mod lang table (`ui_locale`, English fallback).
    pub fn block_inspector_telemetry_at(
        &mut self,
        pos: (i32, i32, i32),
    ) -> Option<BlockInspectorTelemetry> {
        let record = self.kinetic_telemetry.get(&pos)?.clone();
        let state = self.world.get_block_state(pos.0, pos.1, pos.2);
        let resolved = state.and_then(|state| {
            state_block_name(&self.world, self.mod_registry.as_ref(), state)
                .filter(|name| !name.is_empty())
                .map(|name| (state, name))
        });
        let (block_id, dynamic) = resolved
            .map(|(state, name)| {
                let dynamic = self.world.blocks.dynamic_light_fact(state).is_some();
                (name.to_owned(), dynamic)
            })
            .unwrap_or_else(|| ("megu:kinetic_part".to_owned(), false));
        let display_name = if dynamic {
            self.ensure_mod_translations();
            let overlay_key = format!("block.{}", block_id.replace(':', "."));
            self.auto_translation_overlay
                .as_ref()
                .and_then(|overlay| overlay.get(&overlay_key).cloned())
                .or_else(|| {
                    self.mod_translations
                        .as_ref()
                        .map(|table| table.translate_block(&block_id))
                })
                .filter(|name| !name.is_empty())
        } else {
            None
        };
        Some(BlockInspectorTelemetry {
            block_id,
            display_name,
            rpm: record.speed_rpm,
            stress_impact: 0.0,
            network_stress_used: record.stress_load,
            network_stress_capacity: record.stress_capacity,
            overstressed: record.overstressed,
            processing_item: None,
            processing_progress: None,
            fluid_mb: None,
            extra_metrics: Vec::new(),
        })
    }

    fn current_kinetic_dimension(&self) -> i32 {
        match self.world.dimension.name.as_str() {
            "minecraft:the_nether" => 1,
            "minecraft:the_end" => 2,
            _ => 0,
        }
    }

    /// Read-only dimensioned telemetry accessor for mesh reconciliation,
    /// goggles adapters and integration tests.
    pub fn kinetic_telemetry_at_in_dimension(
        &self,
        dimension_id: i32,
        pos: (i32, i32, i32),
    ) -> Option<&KineticSyncRecord> {
        self.kinetic_telemetry_dimensioned.get(&(dimension_id, pos))
    }

    /// Number of pending dimension/generation snapshot groups in the `KNT1`
    /// reassembler (diagnostics and tests).
    pub fn kinetic_pending_snapshot_groups(&self) -> usize {
        self.kinetic_reassembler.pending_len()
    }

    /// Dimensioned store mirror backing the telemetry map.
    pub fn kinetic_sync_store(&self) -> &KineticSyncStore {
        &self.kinetic_sync_store
    }

    fn ingest_kinetic_payload(&mut self, payload: &[u8]) {
        match decode_kinetic_sync_payload(payload) {
            Ok(KineticSyncPayload::LegacyBatch(records)) => {
                self.kinetic_sync_store.apply_batch(records.clone());
                for record in records {
                    self.upsert_kinetic_record(record);
                }
            }
            Ok(KineticSyncPayload::SnapshotChunk(chunk)) => {
                match self.kinetic_reassembler.ingest_chunk(chunk) {
                    Ok(Some(snapshot)) => {
                        self.kinetic_sync_store.apply_snapshot(snapshot.clone());
                        for record in snapshot.records {
                            self.upsert_kinetic_record(record);
                        }
                    }
                    Ok(None) => {}
                    Err(_) => {
                        // Fail closed: malformed chunks never mutate either store.
                    }
                }
            }
            Err(_) => {}
        }
    }

    /// Re-resolve one active-dimension kinetic visual after its chunk/block
    /// state arrived. Telemetry can legitimately precede the chunk; the first
    /// registration then uses the generic rotor and is upgraded here once
    /// MRS1/block-state data names the real part family and axis.
    fn reconcile_kinetic_telemetry_at(&mut self, pos: (i32, i32, i32)) {
        let dim = self.current_kinetic_dimension();
        let Some(record) = self.kinetic_telemetry_dimensioned.get(&(dim, pos)).cloned() else {
            return;
        };
        let Some(state) = self.world.get_block_state(pos.0, pos.1, pos.2) else {
            return;
        };
        let Some(name) = state_block_name(&self.world, self.mod_registry.as_ref(), state) else {
            return;
        };
        let family = KineticMeshFamily::from_block_name(name);
        if family == KineticMeshFamily::GenericRotor {
            return;
        }
        let stale_generic = self
            .kinetic_pos_to_id
            .get(&(dim, pos))
            .and_then(|id| self.kinetic_parts.get(id))
            .is_some_and(|spec| spec.mesh_kind / 4 == KineticMeshFamily::GenericRotor.index());
        if stale_generic {
            if let Some(id) = self.kinetic_pos_to_id.remove(&(dim, pos)) {
                self.kinetic_live_angles.remove(&id);
                self.remove_kinetic_part(id);
            }
            self.kinetic_telemetry.remove(&pos);
        }
        if !self.kinetic_pos_to_id.contains_key(&(dim, pos)) {
            self.upsert_kinetic_record(record);
        }
    }

    fn reconcile_kinetic_telemetry_chunk(&mut self, cx: i32, cz: i32) {
        let dim = self.current_kinetic_dimension();
        let positions: Vec<_> = self
            .kinetic_telemetry_dimensioned
            .keys()
            .copied()
            .filter(|(record_dim, (x, _, z))| {
                *record_dim == dim && x.div_euclid(16) == cx && z.div_euclid(16) == cz
            })
            .map(|(_, pos)| pos)
            .collect();
        for pos in positions {
            self.reconcile_kinetic_telemetry_at(pos);
        }
    }

    /// Consume a server `MSY1` manifest into a typed content diff. Exact
    /// matches emit the bounded `MSH1` root probe once per root; mismatches
    /// stay structured for the window's consent/recovery UI.
    fn apply_mod_sync_manifest(&mut self, payload: &[u8]) {
        let entries = match decode_msy1_manifest(payload) {
            Ok(entries) => entries,
            Err(_) => {
                self.mod_sync_rejected += 1;
                return;
            }
        };
        let local = self
            .session_paths
            .game_dir
            .as_deref()
            .map(scan_local_mod_sync_inventory)
            .unwrap_or_default();
        let diff = compute_live_mod_sync_diff(&local, &entries);
        if diff.matches() {
            let root = live_mod_sync_root_hash(&entries);
            if self.mod_sync_ack_root != Some(root)
                && self
                    .outgoing
                    .try_send(WorkerCommand::SendCustomPayload {
                        channel: CHANNEL_MOD_SYNC_V1.to_owned(),
                        payload: encode_msh1_probe(&root),
                    })
                    .is_ok()
            {
                self.mod_sync_ack_root = Some(root);
            }
        } else {
            self.mod_sync_ack_root = None;
        }
        self.mod_sync_diff = Some(diff);
    }

    /// Install dynamic `MRS1` states into the world before any subsequent
    /// chunk/block packet can be meshed. Iterate the decoded sparse rows
    /// directly: one frame may mix the Create band (`26_684..`) with generic
    /// mod states (`1_000_000..`), so dense probing from the base id would
    /// silently drop every row outside its bounded window.
    fn install_mrs1_dynamic_registry(&mut self, registry: ClientModRegistry) {
        let states: Vec<_> = registry
            .block_states()
            .map(|(state_id, block)| (state_id, block.name.clone(), block.facts_byte))
            .collect();
        self.world.install_dynamic_mod_registry_states(&states);
        self.mod_registry = Some(registry);
    }

    fn upsert_kinetic_record(&mut self, record: KineticSyncRecord) {
        let pos = record.position();
        let dimension = record.dim;
        self.kinetic_telemetry_dimensioned
            .insert((dimension, pos), record.clone());
        if dimension != self.current_kinetic_dimension() {
            return;
        }
        if self.kinetic_pos_to_id.contains_key(&(dimension, pos)) {
            self.kinetic_telemetry.insert(pos, record);
            return;
        }
        let state = self.world.get_block_state(pos.0, pos.1, pos.2);
        let block_name = state
            .and_then(|state| state_block_name(&self.world, self.mod_registry.as_ref(), state));
        // Axis: the MRS1 property map is authoritative for dynamic ids;
        // `axis=` in a property-suffixed state name is the vanilla cue.
        let axis = state
            .and_then(|state| {
                self.mod_registry
                    .as_ref()
                    .and_then(|registry| registry.block_state(state))
                    .and_then(|block| block.properties.get("axis"))
            })
            .map(|axis| {
                if axis == "x" {
                    [1.0, 0.0, 0.0]
                } else if axis == "z" {
                    [0.0, 0.0, 1.0]
                } else {
                    [0.0, 1.0, 0.0]
                }
            })
            .or_else(|| {
                block_name.map(|name| {
                    if name.contains("axis=x") {
                        [1.0, 0.0, 0.0]
                    } else if name.contains("axis=z") {
                        [0.0, 0.0, 1.0]
                    } else {
                        [0.0, 1.0, 0.0]
                    }
                })
            })
            .unwrap_or([0.0, 1.0, 0.0]);
        let family = KineticMeshFamily::from_block_name(block_name.unwrap_or_default());
        let dominant = KineticAxis::dominant(axis);
        let mesh_kind = family.index() * 4 + dominant.slot();
        let (vertices, indices) = kinetic_template(family, dominant);
        self.register_kinetic_template(mesh_kind, vertices, indices);
        // Pivot-centered templates spin about the block center, so the
        // instance origin is `pos + 0.5`, not the block corner.
        let id = self.register_kinetic_part(KineticPartSpec::shaft(
            mesh_kind,
            [pos.0 as f32 + 0.5, pos.1 as f32 + 0.5, pos.2 as f32 + 0.5],
            axis,
        ));
        self.kinetic_pos_to_id.insert((dimension, pos), id);
        self.kinetic_live_angles.insert(id, 0.0);
        self.kinetic_telemetry.insert(pos, record);
    }

    fn evict_kinetic_part(&mut self, pos: (i32, i32, i32)) {
        self.kinetic_telemetry.remove(&pos);
        let dimension = self.current_kinetic_dimension();
        if let Some(id) = self.kinetic_pos_to_id.remove(&(dimension, pos)) {
            self.kinetic_live_angles.remove(&id);
            self.remove_kinetic_part(id);
        }
    }

    fn evict_kinetic_chunk(&mut self, cx: i32, cz: i32) {
        let positions: Vec<_> = self
            .kinetic_pos_to_id
            .keys()
            .copied()
            .filter(|(_, (x, _, z))| x.div_euclid(16) == cx && z.div_euclid(16) == cz)
            .map(|(_, pos)| pos)
            .collect();
        for pos in positions {
            self.evict_kinetic_part(pos);
        }
    }

    /// Set the shared network speed and advance it by `dt_secs`, then
    /// re-resolve every part's [`InstancedPartRecord`]. Rotation is
    /// analytic — `theta = phase0 + rpm * ratio * TAU/60 * t` — so the
    /// CPU writes only the 48-byte records (and only when they actually
    /// changed); template geometry is never rebuilt.
    pub fn sync_kinetic_network_rpm(&mut self, rpm: f32, dt_secs: f32) {
        self.kinetic_time_secs += dt_secs.max(0.0);
        self.kinetic_network_rpm = rpm;
        for records in self.kinetic_scratch.values_mut() {
            records.clear();
        }
        let omega_base = rpm * std::f32::consts::TAU / 60.0;
        let t = self.kinetic_time_secs;
        for spec in self.kinetic_parts.values() {
            let theta = spec.phase0 + omega_base * spec.ratio * t;
            let euler = axis_angle_to_euler_xyz(spec.axis, theta);
            self.kinetic_scratch
                .entry(spec.mesh_kind)
                .or_default()
                .push(InstancedPartRecord {
                    pos_scale: [
                        spec.position[0],
                        spec.position[1],
                        spec.position[2],
                        spec.scale,
                    ],
                    rot_uv_frame: [euler[0], euler[1], euler[2], 0.0],
                    packed_light: spec.packed_light,
                    tint_rgba: spec.tint_rgba,
                    mesh_kind: spec.mesh_kind,
                    subworld_id: spec.subworld_id,
                });
        }
        // Swap each batch's stale record list with the fresh scratch copy;
        // the replaced list keeps its capacity for next frame's write.
        let scratch = &mut self.kinetic_scratch;
        for batch in self.subworlds.batches_mut() {
            match scratch.get_mut(&batch.mesh_kind) {
                Some(records) if batch.instances != *records => {
                    std::mem::swap(&mut batch.instances, records);
                    batch.instances_dirty = true;
                }
                Some(_) => {}
                None => {
                    if !batch.instances.is_empty() {
                        batch.instances.clear();
                        batch.instances_dirty = true;
                    }
                }
            }
        }
    }

    /// Register or replace a live 6-DOF contraption / airship sub-world.
    /// The mesh uploads once on the next `sync_subworlds`; re-upserting
    /// identical geometry refreshes the transform only, so an existing
    /// entry's GPU buffers stay resident.
    pub fn upsert_live_contraption_subworld(
        &mut self,
        contraption_id: u64,
        transform: SubWorldTransform,
        vertices: Vec<TerrainVertex>,
        indices: Vec<u32>,
    ) {
        match self.subworlds.get(contraption_id) {
            Some(existing) if existing.vertices == vertices && existing.indices == indices => {
                self.set_live_contraption_transform(contraption_id, transform);
            }
            Some(_) => {
                self.subworlds
                    .set_geometry(contraption_id, vertices, indices);
                self.set_live_contraption_transform(contraption_id, transform);
            }
            None => self.subworlds.upsert(SubWorldRenderEntry::new(
                contraption_id,
                transform,
                vertices,
                indices,
            )),
        }
    }

    /// Pose-only contraption update with tick-endpoint tracking. Ordinary
    /// motion preserves `prev_transform` for high-Hz interpolation; jumps
    /// over 32 blocks snap both endpoints so teleports never smear across
    /// the world. `false` for an unknown `contraption_id`.
    pub fn set_live_contraption_transform(
        &mut self,
        contraption_id: u64,
        transform: SubWorldTransform,
    ) -> bool {
        let teleport = self
            .subworlds
            .get(contraption_id)
            .map(|entry| transform_position_jump(entry.transform, transform))
            .unwrap_or(false);
        if teleport {
            self.subworlds.snap_transform(contraption_id, transform)
        } else {
            self.subworlds.set_transform(contraption_id, transform)
        }
    }

    /// Pose-only contraption update: writes the new 64-byte model
    /// transform for the next composed frame without touching geometry.
    /// `false` for an unknown `contraption_id`.
    pub fn update_contraption_pose(
        &mut self,
        contraption_id: u64,
        transform: SubWorldTransform,
    ) -> bool {
        self.set_live_contraption_transform(contraption_id, transform)
    }

    /// High-display-rate transform for a live contraption, interpolated
    /// between the tick endpoints retained by `set_live_contraption_transform`.
    pub fn live_contraption_interpolated_transform(
        &self,
        contraption_id: u64,
        alpha: f32,
    ) -> Option<SubWorldTransform> {
        self.subworlds.interpolated_transform(contraption_id, alpha)
    }

    /// Per-poll dirty-mesh rebuild budget: the base slice scaled by the
    /// adaptive strategy's foreground worker count — the meshing critical
    /// path — clamped so a many-core host cannot spike one poll. The live
    /// frame-budget governor can further cap it via
    /// [`Self::governor_rebuild_cap`].
    pub fn mesh_rebuild_budget(&self) -> usize {
        const BASE_POLL_MESH_BUDGET: usize = 24;
        const MAX_FOREGROUND_SCALE: u32 = 2;
        let base = BASE_POLL_MESH_BUDGET
            * self
                .compute_strategy
                .workers
                .foreground_threads
                .clamp(1, MAX_FOREGROUND_SCALE) as usize;
        // The anti-windup PID's live scale (1.0 until the P² P95
        // bootstraps, shrinking under sustained frame-time pressure)
        // throttles the foreground rebuild allowance.
        let scaled = (base as f64 * self.budget_scale).ceil() as usize;
        // The dispatched kernel mode is a real scheduling knob, not just
        // a label: `LutBatch` is the coalesced-batch profile — the full
        // scaled budget rebuilds inline this poll — while
        // `RecomputeSwar` is the low-latency streaming profile — a
        // bounded inline slice with the tail handed to the off-thread
        // lane, the same trade the recompute-vs-table choice makes in
        // the kernel itself.
        let scaled = match self.executed_kernel_mode {
            ComputeKernelMode::LutBatch => scaled,
            ComputeKernelMode::RecomputeSwar => (scaled / 4).max(1),
        };
        scaled.max(1).min(self.governor_rebuild_cap)
    }

    /// Push this session's connect-time [`ComputeStrategy`] into
    /// `renderer`: upload-budget preset, mesh-worker hint, the
    /// negotiated render-scale/shadow defaults, and the resolved
    /// [`UnifiedOptimizationPlan`] domain hints (`chunk_mesh`, `gpu_cull`,
    /// `shadow`). Returns the negotiated shadow quality.
    pub fn apply_compute_strategy(&mut self, renderer: &mut GpuRenderer) -> BuiltinShadowQuality {
        // Re-resolve the unified plan on every push (startup and
        // settings): the profile plus the autotuner's live kernel choice
        // project into the domain hints below.
        let profile = detect_universal_hardware_profile();
        self.unified_plan =
            build_unified_optimization_plan(&profile, Some(self.kernel_autotuner.selected_mode()));
        self.domain_hints = ClientDomainHints::for_plan(&self.unified_plan);
        // The universal profile's GPU paradigm drives the renderer's
        // depth-store policy (TBDR adapters drop the depth tile).
        renderer.set_gpu_paradigm(profile.gpu_paradigm);
        // gpu_cull domain (Lighting): TBDR and low-power iGPU plans
        // prefer one fused pass — depth never leaves the tile — so the
        // GPU-driven indirect arena path stays off there. Discrete
        // adapters that prove `INDIRECT_EXECUTION` take it.
        renderer.set_gpu_indirect_enabled(
            renderer.indirect_execution_supported()
                && !self.domain_hints.gpu_cull.prefer_single_pass_gpu,
        );
        // chunk_mesh domain (Meshing): `use_morton_layout` selects the
        // distance-ring + Z-order tie-break for arena record ordering.
        renderer.morton_order_enabled = self.domain_hints.chunk_mesh.use_morton_layout;
        // shadow domain (ShadowRendering): shadow maps stay lazy on
        // every plan — the dirty-only cache already minimizes pass
        // count, so the negotiated quality below is untouched.
        let negotiated = renderer.apply_compute_strategy(&self.compute_strategy);
        self.compute_strategy_apply_count += 1;
        negotiated
    }

    /// How many times the adaptive strategy has been pushed into a
    /// renderer via [`Self::apply_compute_strategy`].
    pub fn active_compute_strategy_applied(&self) -> u64 {
        self.compute_strategy_apply_count
    }

    /// The kernel mode the adaptive autotuner currently dispatches,
    /// seeded from `compute_strategy` and updated by frame-time feedback.
    pub fn active_kernel_mode(&self) -> ComputeKernelMode {
        self.kernel_autotuner.selected_mode()
    }

    /// The kernel mode the current/last poll actually dispatched under —
    /// `selected_mode()` on ordinary polls, the challenger mode on
    /// probe polls. Differs from [`Self::active_kernel_mode`] exactly
    /// when a challenger measurement is in flight.
    pub fn executed_kernel_mode(&self) -> ComputeKernelMode {
        self.executed_kernel_mode
    }

    /// Counted challenger-probe dispatches — how many polls have run
    /// under the alternate kernel mode for real measurement.
    pub fn kernel_probe_count(&self) -> u64 {
        self.kernel_probe_count
    }

    /// The `MeshJob::SectionMesh` submission cap the selected drain
    /// profile applies this poll — the runtime knob `select_arm`
    /// actually steers.
    pub fn offthread_submit_cap(&self) -> usize {
        ducb_submit_cap(self.ducb_arm_selected)
    }

    /// Autotuner state for diagnostics/tests.
    pub fn kernel_autotuner(&self) -> &KernelAutotuner {
        &self.kernel_autotuner
    }

    /// Counted `kernel_autotuner.observe_sample` feeds — frame-time and
    /// measured mesh-batch samples combined. `> 0` proves the feedback
    /// loop executes live.
    pub fn autotuner_samples(&self) -> u64 {
        self.autotuner_sample_count
    }

    /// Streaming P² P95 of live frame times (ms) — `None` until five
    /// valid samples bootstrap the five-marker estimate.
    pub fn p95_frame_ms(&self) -> Option<f64> {
        self.frame_p2_p95.estimate()
    }

    /// Streaming P² P99 of live frame times (ms) — the 1%-low edge.
    pub fn p99_frame_ms(&self) -> Option<f64> {
        self.frame_p2_p99.estimate()
    }

    /// The anti-windup PID's current budget scale multiplying
    /// [`Self::mesh_rebuild_budget`] (`1.0` pre-bootstrap).
    pub fn frame_budget_scale(&self) -> f64 {
        self.budget_scale
    }

    /// The discounted-UCB drain-profile arm the current/last poll
    /// executed — `ducb.select_arm()` consumed live on the scheduling
    /// loop, then fed the timings that profile produced.
    pub fn ducb_selected_arm(&self) -> usize {
        self.ducb_arm_selected
    }

    /// Counted `ducb.observe` feeds — frame-time and mesh-batch samples.
    pub fn ducb_samples(&self) -> u64 {
        self.ducb_sample_count
    }

    /// The current [`UnifiedOptimizationPlan`] (refreshed by
    /// [`Self::apply_compute_strategy`]).
    pub fn unified_optimization_plan(&self) -> &UnifiedOptimizationPlan {
        &self.unified_plan
    }

    /// The `chunk_mesh`/`gpu_cull`/`shadow` domain projections applied
    /// to the renderer on the last strategy push.
    pub fn domain_hints(&self) -> &ClientDomainHints {
        &self.domain_hints
    }

    /// Record frame timing and advance visual transitions before HUD rendering.
    pub fn record_frame_and_refresh_compact_debug_hud(
        &mut self,
        frame_dt_sec: f32,
        ui: &mut UiState,
    ) {
        self.sync_kinetic_live_parts(frame_dt_sec);
        let frame_us = f64::from(frame_dt_sec) * 1_000_000.0;
        let frame_ms = frame_us / 1_000.0;
        self.frame_timings.push_frame_ms(frame_ms);
        // P² streaming quantiles over the live frame stream: P95 prices
        // the budget PID and the window's upload edge, P99 is the
        // 1%-low telemetry — O(1) memory, no heap per sample.
        self.frame_p2_p95.observe(frame_ms);
        self.frame_p2_p99.observe(frame_ms);
        // Feed the adaptive kernel autotuner real frame-time feedback; the
        // controller discards out-of-range samples itself. The sample
        // credits the mode the poll actually dispatched under — during a
        // challenger probe that is the probed arm, not the incumbent.
        let frame_ns = (f64::from(frame_dt_sec) * 1_000_000_000.0) as u64;
        let kernel_mode = self.executed_kernel_mode;
        self.kernel_autotuner.observe_sample(kernel_mode, frame_ns);
        self.autotuner_sample_count += 1;
        // The discounted-UCB bandit prices the drain profile this poll
        // actually ran — `select_arm` in `poll` picked it, this frame's
        // measured cost scores it.
        self.ducb.observe(self.ducb_arm_selected, frame_ns);
        self.ducb_sample_count += 1;
        // Anti-windup PID consumes the streaming P95 once the five-marker
        // bootstrap lands; `mesh_rebuild_budget` multiplies by the scale.
        if let Some(p95_ms) = self.frame_p2_p95.estimate() {
            self.budget_scale = self.pid_budget.update(p95_ms);
        }
        if frame_dt_sec.is_finite() && frame_dt_sec > 0.0 {
            self.compact_debug_hud_sys_accum_ms += frame_dt_sec * 1_000.0;
        }
        self.chunk_fades.tick(frame_dt_sec);
        self.atmosphere_blender.advance(frame_dt_sec);
        self.refresh_subtitle_hud(frame_dt_sec, &mut ui.hud);
        self.refresh_looking_at();
        self.ensure_asset_bundle();
        self.refresh_voxel_clouds();

        if ui.settings.qol.compact_debug_hud_preset() == CompactDebugHudPreset::Off {
            return;
        }

        // `/proc` + DRM sysfs reads are throttled to
        // `COMPACT_DEBUG_HUD_SYS_SAMPLE_INTERVAL_MS` of accumulated frame time; the
        // cached snapshot feeds every frame's telemetry below.
        if self.compact_debug_hud_host.is_none()
            || self.compact_debug_hud_sys_accum_ms
                >= COMPACT_DEBUG_HUD_SYS_SAMPLE_INTERVAL_MS as f32
        {
            self.compact_debug_hud_host = Some(sample_host_metrics(&mut self.metrics_probe));
            self.sys_sample_count += 1;
            self.compact_debug_hud_sys_accum_ms = 0.0;
        }
        let host = self
            .compact_debug_hud_host
            .as_ref()
            .expect("host metrics sampled above");
        let telemetry = CompactDebugHudData::from_telemetry(host, &self.frame_timings);
        let hud = &mut ui.hud.compact_debug_hud_data;
        hud.fps = telemetry.fps;
        hud.fps_one_percent_low = telemetry.fps_one_percent_low;
        hud.mspt = telemetry.mspt;
        hud.tps = telemetry.tps;
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
        let mut pushed = false;
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
            pushed = true;
        }
        if pushed {
            self.hud_revision += 1;
        }
        // Keep the legacy row mirror fed for embedders/tests that still read
        // `HudState::subtitles`; layout suppresses those rows whenever the
        // richer spatial panel is populated, so this never double-draws.
        self.hud.subtitles = self
            .hud
            .subtitle_hud
            .entries()
            .iter()
            .map(|entry| HudSubtitle {
                label: entry.label.clone(),
                direction: match entry.arrow {
                    SubtitleArrow::Left => megu_ui::state::SubtitleDirection::Left,
                    SubtitleArrow::Center => megu_ui::state::SubtitleDirection::Center,
                    SubtitleArrow::Right => megu_ui::state::SubtitleDirection::Right,
                    SubtitleArrow::Behind => megu_ui::state::SubtitleDirection::Behind,
                },
            })
            .collect();
        hud.subtitle_hud = self.hud.subtitle_hud.clone();
        hud.subtitles = self.hud.subtitles.clone();
    }

    /// Apply a decoded packet, also used by framed protocol regression tests.
    pub fn apply_event(&mut self, event: ClientPlayEvent) -> Result<(), String> {
        if let ClientPlayEvent::Play(play) = &event {
            if let Some((tag, payload, len)) = compact_replay_event(play) {
                self.replay_ring.push(
                    self.replay_started
                        .elapsed()
                        .as_millis()
                        .min(u128::from(u64::MAX)) as u64,
                    tag,
                    &payload[..len],
                );
            }
        }
        match event {
            ClientPlayEvent::Play(event) => self.apply_play(event),
            ClientPlayEvent::CustomChannel(channel) => self.apply_custom(channel),
        }
    }

    /// Record a terrain-dirty mark for `coord` in the reconcile set, the
    /// off-thread lane's content epoch (invalidating any in-flight
    /// `SectionMesh` snapshot) and the submission candidate set.
    fn mark_offthread_coord(&mut self, coord: SectionCoord) {
        self.dirty_terrain_coords.insert(coord);
        self.section_mesh_lane.mark_dirty(coord);
        self.offthread_candidates.insert(coord);
    }

    /// Mark every section of chunk column `(cx, cz)` — plus the four
    /// lateral neighbor columns — for renderer re-evaluation. `MeshCache`
    /// only invalidates a neighbor when a shared boundary actually
    /// changed; over-marking is harmless because the renderer compares
    /// mesh contents per coord, but under-marking would strand a changed
    /// neighbor forever.
    fn mark_chunk_column_dirty(&mut self, cx: i32, cz: i32) {
        let min_sy = self.world.dimension.min_section_y();
        let section_count = self.world.dimension.section_count() as i32;
        for (dx, dz) in [(0, 0), (1, 0), (-1, 0), (0, 1), (0, -1)] {
            for sy in min_sy..min_sy + section_count {
                self.mark_offthread_coord(SectionCoord(cx + dx, sy, cz + dz));
            }
        }
    }

    /// Mark the section containing block `pos` plus its six face-adjacent
    /// sections — `MeshCache::set_cell` can dirty any of them when the
    /// edit touches a shared boundary.
    fn mark_block_dirty(&mut self, pos: [i32; 3]) {
        let section = SectionCoord(
            pos[0].div_euclid(16),
            pos[1].div_euclid(16),
            pos[2].div_euclid(16),
        );
        self.mark_offthread_coord(section);
        for (dx, dy, dz) in [
            (1, 0, 0),
            (-1, 0, 0),
            (0, 1, 0),
            (0, -1, 0),
            (0, 0, 1),
            (0, 0, -1),
        ] {
            self.mark_offthread_coord(SectionCoord(section.0 + dx, section.1 + dy, section.2 + dz));
        }
    }

    /// Record block positions a just-sent interaction sequence may have
    /// predicted — see [`Self::pending_interaction_seqs`]. Bounded: the
    /// map never exceeds 256 in-flight sequences (oldest evicted, same
    /// bound vanilla's pending-prediction ledger keeps).
    fn track_pending_interaction(&mut self, sequence: i32, positions: &[[i32; 3]]) {
        const MAX_PENDING_INTERACTIONS: usize = 256;
        self.pending_interaction_seqs
            .entry(sequence)
            .or_default()
            .extend_from_slice(positions);
        while self.pending_interaction_seqs.len() > MAX_PENDING_INTERACTIONS {
            self.pending_interaction_seqs.pop_first();
        }
    }

    /// `block_changed_ack`: the server confirmed every interaction
    /// sequence at or below `sequence` is processed — the blocks those
    /// interactions touched now match the authoritative world. Drain
    /// the recorded positions and re-dirty their sections so the mesh
    /// picks up any reconciled state (ghost blocks under latency or
    /// protected regions disappear instead of lingering forever).
    fn apply_block_changed_ack(&mut self, sequence: i32) {
        self.highest_acknowledged_sequence = sequence;
        // Wrapping subtraction keeps `k <= sequence` true across the
        // i32 wrap point for any sane in-flight window (< 2^31 deep).
        let drained: Vec<i32> = self
            .pending_interaction_seqs
            .keys()
            .filter(|seq_key| sequence.wrapping_sub(**seq_key) >= 0)
            .copied()
            .collect();
        for key in drained {
            if let Some(positions) = self.pending_interaction_seqs.remove(&key) {
                for pos in positions {
                    self.mark_block_dirty(pos);
                }
                self.terrain_dirty = true;
            }
        }
    }

    /// Re-anchor the speculative prefetch cone on the server-issued view
    /// center (`set_center_chunk`) when one is active, else on the pose.
    fn apply_set_center_chunk(&mut self, cx: i32, cz: i32) {
        self.view_center_chunk = Some((cx, cz));
        self.refresh_prefetcher();
    }

    /// `command_suggestions` reply for a C2S request — stored for the
    /// window to fold into the chat bar's completion menu (matching
    /// `transaction_id` checked there; stale replies stay ignored).
    fn apply_command_suggestions(&mut self, reply: ServerCommandSuggestions) {
        self.command_suggestions = Some(reply);
        self.command_suggestions_rev = self.command_suggestions_rev.wrapping_add(1);
    }

    /// `custom_chat_completions` (aka `chat_completions`) actions:
    /// 0 add, 1 remove, 2 set — the vanilla `CustomChatCompletions`
    /// word set merged into plain-chat tab completion.
    fn apply_chat_completions(&mut self, action: i32, entries: &[String]) {
        match action {
            0 => {
                for entry in entries {
                    self.custom_chat_completions.insert(entry.clone());
                }
            }
            1 => {
                for entry in entries {
                    self.custom_chat_completions.remove(entry);
                }
            }
            _ => {
                self.custom_chat_completions = entries.iter().cloned().collect();
            }
        }
        self.custom_chat_completions_rev = self.custom_chat_completions_rev.wrapping_add(1);
    }

    /// `cooldown` S2C: start (or replace) the cooldown on item registry
    /// `item` for `ticks` ticks at the 20 Hz client tick. `ticks <= 0`
    /// clears — the packet only ever carries positive durations, so a
    /// non-positive value is treated as a removal guard.
    fn apply_cooldown(&mut self, item: i32, ticks: i32) {
        if ticks > 0 {
            self.item_cooldowns.insert(
                item,
                ItemCooldown {
                    remaining_ticks: ticks,
                    total_ticks: ticks,
                },
            );
        } else {
            self.item_cooldowns.remove(&item);
        }
        self.sync_hud_item_cooldowns();
    }

    /// Mirror `item_cooldowns` into `hud.item_cooldowns` keyed by the
    /// same registry name `sync_hud_inventory` writes into `hud.hotbar`
    /// — the layout's slot wipe matches on that name.
    fn sync_hud_item_cooldowns(&mut self) {
        self.hud.item_cooldowns = self
            .item_cooldowns
            .iter()
            .filter(|(_, cd)| cd.remaining_ticks > 0)
            .map(|(item, cd)| {
                (
                    crate::recipe_ingest::item_name_for_id(*item as u32),
                    cd.remaining_ticks as f32 / i32::max(cd.total_ticks, 1) as f32,
                )
            })
            .collect();
        self.hud_revision += 1;
    }

    /// `delete_chat` S2C: retract the message the packet identifies —
    /// signature match when present, global chat index otherwise.
    fn apply_delete_chat(&mut self, message_index: Option<i32>, signature: Option<[u8; 256]>) {
        self.reconcile_chat_keys();
        let position = self.chat_log_keys.iter().position(|key| {
            key.is_some_and(|key| match (signature, message_index) {
                (Some(sig), _) => key.signature == Some(sig),
                (None, Some(index)) => key.index == index,
                (None, None) => false,
            })
        });
        if let Some(position) = position {
            self.hud.chat_lines.remove(position);
            self.chat_log_keys.remove(position);
            self.hud_revision += 1;
        }
    }

    /// `reset_chat` S2C: the server cleared the whole chat history —
    /// drop every line plus its deletion key (vanilla wipes the log).
    fn apply_reset_chat(&mut self) {
        self.hud.chat_lines.clear();
        self.chat_log_keys.clear();
        self.hud_revision += 1;
    }

    fn apply_play(&mut self, event: PlayEvent) -> Result<(), String> {
        // World-side tracking (chunks, block/light updates, entities, block
        // entities, time/weather, break overlays) is owned by
        // `WorldState::apply_play_event`; session-side concerns stay below.
        self.world.apply_play_event(&event)?;
        // The death tracker sees every play event: `player_combat_kill` /
        // `set_health(<=0)` mark death, `respawn`/`set_health(>0)` and the
        // next `login` clear it.
        self.death.apply(&event);
        // `game_event` id 11 with value 0 is vanilla's immediate
        // respawn: the moment death lands the client re-requests
        // `PERFORM_RESPAWN` — no death screen, no click.
        if self.death.is_dead() && !self.respawn_screen_enabled {
            self.request_respawn();
        }
        match event {
            PlayEvent::EntityEvent { eid, status } if Some(eid) == self.player_entity_id && (24..=28).contains(&status) => {
                self.permission_level = (status - 24) as u8;
            }
            PlayEvent::TabList { .. } => {
                self.sync_hud_tab_list();
                self.bump_hud_revision();
            }
            PlayEvent::SetPlayerTeam { .. } => {
                self.sync_hud_players();
                self.bump_hud_revision();
            }
            PlayEvent::MapItemData { map_id, .. } => {
                if self.held_map_id() == Some(map_id)
                    || self
                        .hud
                        .held_map
                        .as_ref()
                        .is_some_and(|map| !self.world.maps.contains_key(&map.id))
                {
                    self.sync_hud_map();
                    self.bump_hud_revision();
                }
            }
            PlayEvent::PlayerCombatEnter
            | PlayEvent::PlayerCombatEnd { .. }
            | PlayEvent::DamageEvent { .. } => self.sync_hud_combat(),
            event @ (PlayEvent::JoinGame { .. }
            | PlayEvent::Respawn { .. }
            | PlayEvent::Teleport(..)
            | PlayEvent::ChunkData(..)
            | PlayEvent::BlockUpdate { .. }
            | PlayEvent::SectionBlocksUpdate(..)
            | PlayEvent::UpdateLight { .. }
            | PlayEvent::UnloadChunk { .. }
            | PlayEvent::GameEvent { .. }
            | PlayEvent::BlockChangedAck(..)
            | PlayEvent::SetCenterChunk { .. }) => self.apply_play_world_state_event(event),
            event @ (PlayEvent::AddEntity { .. }
            | PlayEvent::RemoveEntities { .. }
            | PlayEvent::MoveEntityPos { .. }
            | PlayEvent::MoveEntityPosRot { .. }
            | PlayEvent::MoveEntityRot { .. }
            | PlayEvent::TeleportEntity { .. }
            | PlayEvent::SetEquipment { .. }
            | PlayEvent::SetEntityData { .. }
            | PlayEvent::UpdateMobEffect { .. }
            | PlayEvent::RemoveMobEffect { .. }
            | PlayEvent::HurtAnimation { .. }
            | PlayEvent::SetPassengers { .. }
            | PlayEvent::MoveVehicle { .. }
            | PlayEvent::TakeItemEntity { .. }) => self.apply_play_entity_state_event(event),
            event @ (PlayEvent::OpenScreen { .. }
            | PlayEvent::HorseScreenOpen { .. }
            | PlayEvent::ContainerClose { .. }
            | PlayEvent::OpenSignEditor { .. }
            | PlayEvent::OpenBook { .. }
            | PlayEvent::ContainerSetContent { .. }
            | PlayEvent::ContainerSetSlot { .. }
            | PlayEvent::ContainerSetData { .. }
            | PlayEvent::SetCarriedItem(..)) => self.apply_play_inventory_screen_event(event),
            event @ (PlayEvent::SetHealth { .. }
            | PlayEvent::SetExperience { .. }
            | PlayEvent::SystemChat { .. }
            | PlayEvent::SetActionBarText { .. }
            | PlayEvent::PlayerChat { .. }
            | PlayEvent::DisguisedChat { .. }
            | PlayEvent::CommandSuggestions { .. }
            | PlayEvent::ChatCompletions { .. }
            | PlayEvent::Cooldown { .. }
            | PlayEvent::DeleteChat { .. }
            | PlayEvent::ResetChat) => self.apply_play_vitals_chat_event(event),
            event @ (PlayEvent::UpdateAdvancements { .. }
            | PlayEvent::SelectAdvancementsTab { .. }) => self.apply_play_progression_event(event),
            event @ (PlayEvent::SetTitle { .. }
            | PlayEvent::SetTitleText { .. }
            | PlayEvent::SetSubtitleText { .. }
            | PlayEvent::ClearTitles
            | PlayEvent::BossEvent { .. }
            | PlayEvent::PlayerInfo { .. }
            | PlayEvent::PlayerInfoRemove { .. }
            | PlayEvent::SetObjective { .. }
            | PlayEvent::SetScore { .. }
            | PlayEvent::SetDisplayObjective { .. }
            | PlayEvent::ResetScore { .. }) => self.apply_play_overlay_scoreboard_event(event),
            event @ (PlayEvent::BlockDestruction { .. }
            | PlayEvent::Explosion { .. }
            | PlayEvent::LevelEvent { .. }
            | PlayEvent::BlockEvent { .. }
            | PlayEvent::LevelParticles { .. }) => self.apply_play_combat_effect_event(event),
            event @ (PlayEvent::PlayerCombatKill { .. }
            | PlayEvent::Transfer { .. }
            | PlayEvent::Ping(..)
            | PlayEvent::PlayerAbilities { .. }
            | PlayEvent::ResourcePackPush { .. }
            | PlayEvent::ResourcePackPop { .. }
            | PlayEvent::Disconnect(..)) => self.apply_play_session_service_event(event),
            event @ (PlayEvent::InitializeBorder { .. }
            | PlayEvent::SetBorderCenter { .. }
            | PlayEvent::SetBorderSize { .. }
            | PlayEvent::SetBorderLerpSize { .. }
            | PlayEvent::SetBorderWarningDistance { .. }
            | PlayEvent::SetBorderWarningDelay { .. }
            | PlayEvent::SetDefaultSpawnPosition { .. }
            | PlayEvent::AwardStats { .. }
            | PlayEvent::ChangeDifficulty { .. }
            | PlayEvent::PlayerLookAt { .. }
            | PlayEvent::TickingState { .. }
            | PlayEvent::TickingStep { .. }
            | PlayEvent::ServerData { .. }
            | PlayEvent::ChunkBatchStart
            | PlayEvent::ChunkBatchFinished(..)
            | PlayEvent::PongResponse { .. }) => self.apply_play_survival_state_event(event),
            _ => {}
        }
        Ok(())
    }

    /// Session-side effects for survival state packets whose persistent
    /// world mirror is maintained by `WorldState::apply_play_event` above.
    fn apply_play_survival_state_event(&mut self, event: PlayEvent) {
        match event {
            PlayEvent::PlayerLookAt {
                anchor,
                target,
                entity,
            } => self.apply_player_look_at(anchor, target, entity),
            PlayEvent::PongResponse { id } => {
                if self
                    .pending_ping_probe
                    .is_some_and(|(pending_id, _)| pending_id == id)
                {
                    let (_, sent_at) = self.pending_ping_probe.take().expect("checked above");
                    let rtt = sent_at.elapsed().as_millis().min(u128::from(u32::MAX)) as u32;
                    self.pong_rtt_ms = Some(rtt);
                }
            }
            PlayEvent::ChunkBatchStart => {
                self.chunk_batch_started_at = Some(Instant::now());
            }
            PlayEvent::ChunkBatchFinished(_) => {
                if let Some(started) = self.chunk_batch_started_at.take() {
                    self.chunk_batch_last_ms =
                        Some(started.elapsed().as_millis().min(u128::from(u32::MAX)) as u32);
                }
            }
            _ => {}
        }
        self.sync_survival_hud();
        self.bump_hud_revision();
    }

    /// Compute the protocol look-at orientation and arm the smooth camera
    /// turn consumed by `window.rs` once per frame.
    fn apply_player_look_at(
        &mut self,
        anchor: i32,
        mut target: [f64; 3],
        entity: Option<(i32, i32)>,
    ) {
        let Some(pose) = self.pose else {
            return;
        };
        if let Some((entity_id, entity_anchor)) = entity {
            let Some(tracked) = self.world.entities.get(&entity_id) else {
                return;
            };
            let mut base = tracked.pos;
            if entity_anchor == 1 {
                base[1] += f64::from(self.world.entity_eye_height(entity_id).unwrap_or(1.62));
            }
            // Entity-target packets resolve against the entity's current
            // feet/eye position client-side; the serialized XYZ is the
            // sender-time position, not an offset to add onto it.
            target = base;
        }
        let eye_y = if anchor == 1 {
            pose.feet[1] + f64::from(self.eye_height)
        } else {
            pose.feet[1]
        };
        let dx = target[0] - pose.feet[0];
        let dy = target[1] - eye_y;
        let dz = target[2] - pose.feet[2];
        let horizontal = (dx * dx + dz * dz).sqrt();
        if horizontal < 1.0e-9 && dy.abs() < 1.0e-9 {
            return;
        }
        let yaw = (-dx).atan2(dz).to_degrees() as f32;
        let pitch = (-dy).atan2(horizontal).to_degrees() as f32;
        if yaw.is_finite() && pitch.is_finite() {
            self.player_look_at_target = Some((yaw.rem_euclid(360.0), pitch.clamp(-90.0, 90.0)));
        }
    }

    /// Advance the server-directed look-at turn from the current camera
    /// orientation. Returns the replacement `(pose yaw, pitch)` while active.
    pub fn poll_player_look_at(
        &mut self,
        current_yaw: f32,
        current_pitch: f32,
        dt_secs: f32,
    ) -> Option<(f32, f32)> {
        let (target_yaw, target_pitch) = self.player_look_at_target?;
        let dt = if dt_secs.is_finite() {
            dt_secs.clamp(0.0, 0.25)
        } else {
            0.0
        };
        let alpha = (dt * 8.0).clamp(0.0, 1.0);
        let yaw_delta = (target_yaw - current_yaw + 540.0).rem_euclid(360.0) - 180.0;
        let pitch_delta = target_pitch - current_pitch;
        if yaw_delta.abs() < 0.05 && pitch_delta.abs() < 0.05 {
            self.player_look_at_target = None;
            return Some((target_yaw, target_pitch));
        }
        let next_yaw = (current_yaw + yaw_delta * alpha).rem_euclid(360.0);
        let next_pitch = (current_pitch + pitch_delta * alpha).clamp(-90.0, 90.0);
        Some((next_yaw, next_pitch))
    }

    /// Mirror survival/session facts into the canonical HUD state every
    /// client frame; the world packet handlers own the underlying data.
    fn sync_survival_hud(&mut self) {
        let before = (
            self.hud.world_border_warning_intensity,
            self.hud.world_border_distance_blocks,
            self.hud.spawn_compass_bearing_deg,
            self.hud.server_difficulty,
            self.hud.server_tick_rate,
            self.hud.server_ticks_frozen,
            self.hud.server_tick_steps,
            self.hud.pong_rtt_ms,
            self.hud.chunk_batch_active,
            self.hud.chunk_batch_last_ms,
            self.hud.server_motd.clone(),
            self.hud.server_enforces_secure_chat,
        );
        let magic_changed = self.hud.magic != self.magic;
        let pose = self.pose;
        if let Some(pose) = pose {
            let (distance, intensity) = self
                .world
                .world_border
                .warning_at(pose.feet[0], pose.feet[2]);
            self.hud.world_border_distance_blocks = Some(distance);
            self.hud.world_border_warning_intensity = intensity;
        } else {
            self.hud.world_border_distance_blocks = None;
            self.hud.world_border_warning_intensity = 0.0;
        }
        self.hud.spawn_compass_bearing_deg = pose.and_then(|pose| {
            self.world
                .default_spawn_position
                .map(|([x, _y, z], _angle)| {
                    let dx = f64::from(x) + 0.5 - pose.feet[0];
                    let dz = f64::from(z) + 0.5 - pose.feet[2];
                    (-dx).atan2(dz).to_degrees().rem_euclid(360.0) as f32
                })
        });
        self.hud.server_difficulty = self.world.difficulty;
        self.hud.server_tick_rate = self.world.server_tick_rate;
        self.hud.server_ticks_frozen = self.world.server_ticks_frozen;
        self.hud.server_tick_steps = self.world.server_tick_steps;
        self.hud.pong_rtt_ms = self.pong_rtt_ms;
        self.hud.compact_debug_hud_data.ping_ms =
            self.pong_rtt_ms.map(|ms| ms.min(u32::from(u16::MAX)));
        self.hud.chunk_batch_active = self.world.chunk_batch_active;
        self.hud.chunk_batch_last_ms = self.chunk_batch_last_ms;
        self.hud.server_motd = self.world.server_motd.clone();
        self.hud.server_enforces_secure_chat = self.world.server_enforces_secure_chat;
        self.hud.magic = self.magic.clone();
        let after = (
            self.hud.world_border_warning_intensity,
            self.hud.world_border_distance_blocks,
            self.hud.spawn_compass_bearing_deg,
            self.hud.server_difficulty,
            self.hud.server_tick_rate,
            self.hud.server_ticks_frozen,
            self.hud.server_tick_steps,
            self.hud.pong_rtt_ms,
            self.hud.chunk_batch_active,
            self.hud.chunk_batch_last_ms,
            self.hud.server_motd.clone(),
            self.hud.server_enforces_secure_chat,
        );
        if before != after || magic_changed {
            self.bump_hud_revision();
        }
    }

    /// Real protocol-statistic summary for the Statistics action. Protocol
    /// ids are retained verbatim because `registries.json` does not carry a
    /// runtime statistic-name table for every server revision.
    pub fn statistics_summary(&self) -> String {
        if self.world.statistics.is_empty() {
            return "Statistics: no server statistics received yet".to_owned();
        }
        const CATEGORIES: [&str; 9] = [
            "mined",
            "crafted",
            "used",
            "broken",
            "picked up",
            "dropped",
            "killed",
            "killed by",
            "custom",
        ];
        let mut per_category = [0usize; 9];
        let mut other = 0usize;
        let mut top: ((i32, i32), i32) = ((0, 0), 0);
        for (&key, &value) in &self.world.statistics {
            if usize::try_from(key.0).is_ok_and(|idx| idx < per_category.len()) {
                per_category[key.0 as usize] += 1;
            } else {
                other += 1;
            }
            if value.abs() > top.1.abs() {
                top = (key, value);
            }
        }
        let mut parts: Vec<String> = CATEGORIES
            .iter()
            .zip(per_category)
            .filter(|(_, count)| *count > 0)
            .map(|(name, count)| format!("{name} {count}"))
            .collect();
        if other > 0 {
            parts.push(format!("other {other}"));
        }
        format!(
            "Statistics: {} entries ({}); largest {}#{}={}",
            self.world.statistics.len(),
            parts.join(", "),
            top.0.0,
            top.0.1,
            top.1
        )
    }

    fn maybe_send_ping_probe(&mut self) {
        if !matches!(self.status, SessionStatus::Playing)
            || self.pending_ping_probe.is_some()
            || self.last_ping_probe_at.elapsed() < Duration::from_secs(5)
        {
            return;
        }
        self.ping_probe_nonce = self.ping_probe_nonce.wrapping_add(1);
        let id = self.ping_probe_nonce;
        if self
            .outgoing
            .try_send(WorkerCommand::PingRequest(id))
            .is_ok()
        {
            self.pending_ping_probe = Some((id, Instant::now()));
            self.last_ping_probe_at = Instant::now();
        }
    }

    fn sync_game_mode_from_world(&mut self) {
        if let Some(mode) = self.world.game_mode {
            self.game_mode = i32::from(mode);
            self.hud.game_mode = i32::from(mode);
            self.hud_revision = self.hud_revision.wrapping_add(1);
        }
    }

    fn bump_hud_revision(&mut self) {
        self.hud_revision += 1;
    }

    fn send_resource_pack_response(&mut self, uuid: [u8; 16], status: i32) {
        let _ = self
            .outgoing
            .try_send(WorkerCommand::ResourcePackResponse { uuid, status });
    }

    fn bump_title_revisions(&mut self) {
        self.title_revision += 1;
        self.bump_hud_revision();
    }

    fn decode_container_slot_payload(
        raw_payload: &[u8],
        slot_count: i32,
        mut insert_slot: impl FnMut(i32, ItemStackComponents),
    ) -> Option<ItemStackComponents> {
        let mut offset = 0usize;
        for index in 0..slot_count {
            let tail = &raw_payload[offset.min(raw_payload.len())..];
            match decode_item_stack_components(tail) {
                Ok((stack, consumed)) => {
                    offset += consumed;
                    insert_slot(index, stack);
                }
                Err(_) => break,
            }
        }
        // The carried item follows the slot array.
        if offset < raw_payload.len() {
            decode_item_stack_components(&raw_payload[offset..])
                .ok()
                .map(|(carried, _)| carried)
        } else {
            None
        }
    }

    fn apply_play_world_state_event(&mut self, event: PlayEvent) {
        match event {
            PlayEvent::JoinGame { entity_id, .. } => {
                self.player_entity_id = Some(entity_id);
                self.player_info.clear();
                self.sync_hud_tab_list();
                self.sync_hud_players();
                self.sync_hud_map();
                self.bump_hud_revision();
                self.reset_dimension();
                self.sync_game_mode_from_world();
            }
            PlayEvent::Respawn { .. } => {
                // `ClientPacketListener.handleRespawn` only rebuilds the
                // level — and calls `levelRenderer.allChanged()` — when the
                // packet's dimension differs from the current one; a
                // same-dimension respawn keeps every resident section and
                // resets just the local player.
                if self.world.take_respawn_changed_dimension() {
                    self.reset_dimension();
                } else {
                    self.reset_local_player_for_respawn();
                }
                self.sync_game_mode_from_world();
            }
            PlayEvent::Teleport(teleport) => {
                let previous = self.pose;
                let pose = PlayerPose::from_teleport(&teleport, self.pose);
                if let Some(previous) = previous {
                    self.last_motion_delta = [
                        pose.feet[0] - previous.feet[0],
                        pose.feet[2] - previous.feet[2],
                    ];
                }
                self.pose = Some(pose);
                self.camera_sync = Some(pose);
                self.physics_state.pos = pose.feet;
                self.physics_state.vel = [0.0; 3];
                self.physics_state.on_ground = false;
                self.physics_state.in_water = false;
                self.physics_state.in_lava = false;
                self.physics_state.eye_in_water = false;
                self.physics_state.swimming = false;
                self.physics_state.fall_distance = 0.0;
                self.reset_physics_interpolation();
                self.was_on_ground = false;
                self.was_submerged = false;
                self.fall_velocity_y = 0.0;
                self.update_audio_listener();
                self.refresh_prefetcher();
            }
            PlayEvent::ChunkData(chunk) => {
                let (cx, cz) = (chunk.cx, chunk.cz);
                self.reconcile_kinetic_telemetry_chunk(cx, cz);
                self.acoustic_world.sync_chunk(&self.world, cx, cz);
                self.audio.mixer.refresh_acoustics();
                self.chunk_fades.note_chunk(cx, cz);
                self.refresh_prefetcher();
                self.mark_chunk_column_dirty(cx, cz);
                self.terrain_dirty = true;
            }
            PlayEvent::BlockUpdate { pos, state_id } => {
                if self.world.blocks.is_air(state_id) {
                    self.evict_kinetic_part((pos[0], pos[1], pos[2]));
                } else {
                    self.reconcile_kinetic_telemetry_at((pos[0], pos[1], pos[2]));
                }
                self.acoustic_world.sync_block(&self.world, pos);
                self.audio.mixer.refresh_acoustics();
                self.mark_block_dirty(pos);
                self.terrain_dirty = true;
            }
            PlayEvent::SectionBlocksUpdate(changes) => {
                for (pos, state_id) in &changes {
                    if self.world.blocks.is_air(*state_id) {
                        self.evict_kinetic_part((pos[0], pos[1], pos[2]));
                    }
                    self.acoustic_world.sync_block(&self.world, *pos);
                    self.mark_block_dirty(*pos);
                }
                self.audio.mixer.refresh_acoustics();
                if !changes.is_empty() {
                    self.terrain_dirty = true;
                }
            }
            PlayEvent::UpdateLight { cx, cz, .. } => {
                self.mark_chunk_column_dirty(cx, cz);
                self.terrain_dirty = true;
            }
            PlayEvent::UnloadChunk { cx, cz } => {
                self.evict_kinetic_chunk(cx, cz);
                self.acoustic_world.unload_chunk(cx, cz);
                self.audio.mixer.refresh_acoustics();
                self.chunk_fades.forget(cx, cz);
                self.refresh_prefetcher();
                // The unloaded column's own sections leave tracking
                // entirely — in-flight SectionMesh results drain as stale.
                let min_sy = self.world.dimension.min_section_y();
                for sy in min_sy..min_sy + self.world.dimension.section_count() as i32 {
                    let coord = SectionCoord(cx, sy, cz);
                    self.section_mesh_lane.remove(coord);
                    self.offthread_candidates.remove(&coord);
                    self.remove_external_section(coord);
                }
                self.mark_chunk_column_dirty(cx, cz);
                self.terrain_dirty = true;
            }
            PlayEvent::GameEvent { event, value } => {
                self.apply_game_event(event, value);
            }
            PlayEvent::BlockChangedAck(sequence) => self.apply_block_changed_ack(sequence),
            PlayEvent::SetCenterChunk { cx, cz } => self.apply_set_center_chunk(cx, cz),
            _ => {}
        }
    }

    fn apply_play_entity_state_event(&mut self, event: PlayEvent) {
        match event {
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
                        let cem_name = spec.name.strip_prefix("minecraft:").unwrap_or(spec.name);
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
                    self.entity_metadata.remove(eid);
                    self.mob_effects.remove(eid);
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
            PlayEvent::SetEntityData { eid, raw_entries } => {
                let meta = self.entity_metadata.entry(eid).or_default();
                for (index, _ty, value) in parse_entity_metadata(&raw_entries) {
                    match (index, value) {
                        (0, EntityMetaValue::Byte(flags)) => meta.flags = Some(flags),
                        (1, EntityMetaValue::VarInt(air)) => meta.air = Some(air),
                        (2, EntityMetaValue::OptionalText(Some(name)))
                        | (2, EntityMetaValue::Text(name)) => {
                            meta.custom_name = Some(name);
                        }
                        (2, EntityMetaValue::OptionalText(None)) => {
                            meta.custom_name = None;
                        }
                        (3, EntityMetaValue::Bool(visible)) => {
                            meta.name_visible = Some(visible);
                        }
                        (6, EntityMetaValue::VarInt(pose)) => meta.pose = Some(pose),
                        (8, EntityMetaValue::Slot(bytes)) => {
                            meta.item_stack = decode_item_stack_components(&bytes)
                                .ok()
                                .map(|(stack, _)| stack);
                        }
                        (9, EntityMetaValue::Float(health)) => {
                            meta.living_health = Some(health);
                        }
                        (14, EntityMetaValue::Float(absorption)) => {
                            meta.absorption = Some(absorption);
                        }
                        _ => {}
                    }
                }
                // The local player's index-1 air supply drives the HUD
                // bubble row; index-14 extra hearts drive absorption.
                if Some(eid) == self.player_entity_id {
                    if let Some(air) = self.entity_metadata.get(&eid).and_then(|m| m.air) {
                        self.hud.air = air.clamp(0, 300) as u16;
                    }
                    if let Some(absorption) =
                        self.entity_metadata.get(&eid).and_then(|m| m.absorption)
                    {
                        self.hud.absorption = absorption.max(0.0);
                    }
                }
                self.bump_hud_revision();
            }
            PlayEvent::UpdateMobEffect {
                eid,
                effect_id,
                amplifier,
                duration,
                flags,
            } => {
                let effects = self.mob_effects.entry(eid).or_default();
                if effects.len() < 32 || effects.contains_key(&effect_id) {
                    effects.insert(
                        effect_id,
                        LiveMobEffect {
                            amplifier,
                            duration,
                            flags,
                        },
                    );
                }
                if Some(eid) == self.player_entity_id {
                    self.sync_hud_status_effects();
                }
            }
            PlayEvent::RemoveMobEffect { eid, effect_id } => {
                if let Some(effects) = self.mob_effects.get_mut(&eid) {
                    effects.remove(&effect_id);
                }
                if Some(eid) == self.player_entity_id {
                    self.sync_hud_status_effects();
                }
            }
            PlayEvent::HurtAnimation { eid, yaw } if Some(eid) == self.player_entity_id => {
                self.hurt_flash_alpha = 1.0;
                self.local_hurt = Some(LocalHurtAnimation {
                    yaw_deg: yaw,
                    elapsed_secs: 0.0,
                });
            }
            PlayEvent::SetPassengers {
                vehicle,
                passengers,
            } => {
                let local = self.player_entity_id;
                if passengers.iter().any(|id| Some(*id) == local) {
                    self.riding_vehicle = Some(vehicle);
                    let hp = self
                        .entity_metadata
                        .get(&vehicle)
                        .and_then(|meta| meta.living_health)
                        .filter(|v| v.is_finite() && *v > 0.0);
                    self.hud.mount_health = hp.map(|h| (h, h.max(20.0)));
                    self.hud.mount_jump_charge = Some(0.0);
                    self.hud_revision = self.hud_revision.wrapping_add(1);
                } else if self.riding_vehicle == Some(vehicle) {
                    self.riding_vehicle = None;
                    self.hud.mount_health = None;
                    self.hud.mount_jump_charge = None;
                    self.hud_revision = self.hud_revision.wrapping_add(1);
                }
            }
            PlayEvent::MoveVehicle {
                x,
                y,
                z,
                yaw,
                pitch,
            } => {
                if !x.is_finite()
                    || !y.is_finite()
                    || !z.is_finite()
                    || !yaw.is_finite()
                    || !pitch.is_finite()
                {
                    return;
                }
                let Some(vehicle) = self.riding_vehicle else {
                    return;
                };
                self.entity_positions.insert(vehicle, [x, y, z]);
                if let Some(entity) = self.world.entities.get_mut(&vehicle) {
                    entity.pos = [x, y, z];
                }
                if let Some(mut pose) = self.pose {
                    pose.feet = [x, y, z];
                    pose.yaw = yaw;
                    pose.pitch = pitch;
                    self.pose = Some(pose);
                    self.physics_state.pos = [x, y, z];
                    self.reset_physics_interpolation();
                }
            }
            PlayEvent::TakeItemEntity {
                item_entity,
                player,
                ..
            } => {
                // Vanilla plays `entity.item.pickup` on the PLAYERS
                // source at the collecting player — the item position
                // is the closest tracked anchor this client keeps.
                let pos = self
                    .entity_positions
                    .get(&item_entity)
                    .or_else(|| self.entity_positions.get(&player))
                    .map(|p| [p[0] as f32, p[1] as f32, p[2] as f32])
                    .unwrap_or([0.0; 3]);
                self.emit_effect_sound("minecraft:entity.item.pickup", 7, 0.2, 1.0, pos);
            }
            _ => {}
        }
    }

    /// Vanilla titles the mount screen with the entity's display name:
    /// its custom name when set, else the entity-type name ("Horse",
    /// "Donkey", "Llama", ...). `None` lets the schema fallback render.
    fn mount_screen_title(&self, entity_id: i32) -> Option<String> {
        let entity = self.world.entities.get(&entity_id)?;
        if let Some(name) = self
            .entity_metadata
            .get(&entity_id)
            .and_then(|meta| meta.custom_name.as_deref())
            .map(str::trim)
            .filter(|name| !name.is_empty())
        {
            return Some(name.to_owned());
        }
        let registry = megu_proto::ProtocolTable::v767_builtin();
        let name = registry.registry_name(
            "minecraft:entity_type",
            u32::try_from(entity.entity_type).ok()?,
        )?;
        let short = name.strip_prefix("minecraft:").unwrap_or(name);
        let mut title = short.replace('_', " ");
        if let Some(first) = title.get_mut(..1) {
            first.make_ascii_uppercase();
        }
        (!title.is_empty()).then_some(title)
    }

    fn apply_play_inventory_screen_event(&mut self, event: PlayEvent) {
        match event {
            PlayEvent::OpenScreen {
                window_id,
                menu_type,
                title_nbt,
            } => {
                let title_text = nbt_text(&title_nbt);
                self.open_screen = Some(LiveOpenScreen {
                    window_id,
                    menu_type,
                    content_slot_count: None,
                    title_nbt,
                    title_text,
                    state_id: 0,
                    slots: BTreeMap::new(),
                    carried: None,
                    // `container_set_data` may arrive before `open_screen`
                    // (late opens): the session-level map preserves them
                    // and this window picks up whatever was recorded.
                    properties: self
                        .container_properties
                        .iter()
                        .filter(|((id, _), _)| *id == window_id)
                        .map(|((_, property), value)| (*property, *value))
                        .collect(),
                });
            }
            PlayEvent::HorseScreenOpen {
                window_id,
                slots,
                entity_id,
            } => {
                // Mount inventories are not `minecraft:menu` registry ids:
                // `ContainerKind::Horse` maps the wire `slot_count` onto a
                // sentinel `menu_type` (`1000 + chest columns`) that
                // `menu_schema` resolves to vanilla `HorseInventoryMenu`
                // geometry, so `ContainerSetContent`/`ContainerSetSlot`,
                // clicks and `ContainerClose` flow through unchanged.
                let kind = megu_ui::container_screen::ContainerKind::horse(slots, entity_id);
                let window_id = i32::from(window_id);
                let title_text = self.mount_screen_title(entity_id);
                self.open_screen = Some(LiveOpenScreen {
                    window_id,
                    menu_type: kind.menu_id() as i32,
                    content_slot_count: None,
                    title_nbt: Vec::new(),
                    title_text,
                    state_id: 0,
                    slots: BTreeMap::new(),
                    carried: None,
                    properties: self
                        .container_properties
                        .iter()
                        .filter(|((id, _), _)| *id == window_id)
                        .map(|((_, property), value)| (*property, *value))
                        .collect(),
                });
            }
            PlayEvent::ContainerClose { window_id: _ } => {
                // Vanilla `ClientPacketListener.handleContainerClose` reads
                // nothing off the packet: it calls
                // `LocalPlayer.clientSideCloseContainer()`, which closes
                // whatever window is open. Matching on the packet's id
                // instead leaves a destroyed/far-away container screen up.
                self.open_screen = None;
            }
            PlayEvent::OpenSignEditor { pos, front_side } => {
                self.pending_sign_editor = Some(((pos[0], pos[1], pos[2]), front_side));
            }
            PlayEvent::OpenBook { hand } => {
                const PENDING_OPEN_BOOK_CAP: usize = 4;
                if self.pending_open_book_hands.len() >= PENDING_OPEN_BOOK_CAP {
                    self.pending_open_book_hands.pop_front();
                }
                self.pending_open_book_hands.push_back(hand);
            }
            PlayEvent::ContainerSetContent {
                window_id,
                state_id,
                slot_count,
                raw_payload,
            } => {
                if window_id == 0 {
                    // The player's own inventory: servers sync window 0
                    // without an `open_screen`, so the `E`-key screen has a
                    // live slot source of its own.
                    self.player_inventory_state_id = state_id;
                    self.bump_hud_revision();
                    let carried = Self::decode_container_slot_payload(
                        &raw_payload,
                        slot_count,
                        |index, stack| {
                            self.player_inventory.insert(index, stack);
                        },
                    );
                    if let Some(carried) = carried {
                        self.player_carried = Some(carried);
                    }
                    self.sync_hud_inventory();
                } else if let Some(screen) = &mut self.open_screen {
                    if screen.window_id == i32::from(window_id) {
                        screen.state_id = state_id;
                        screen.content_slot_count = Some(slot_count);
                        let carried = Self::decode_container_slot_payload(
                            &raw_payload,
                            slot_count,
                            |index, stack| {
                                screen.slots.insert(index, stack);
                            },
                        );
                        if let Some(carried) = carried {
                            screen.carried = Some(carried);
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
                if window_id == 0 {
                    self.player_inventory_state_id = state_id;
                    self.bump_hud_revision();
                    match decode_item_stack_components(&raw_item) {
                        Ok((stack, _)) => {
                            if slot < 0 {
                                self.player_carried = Some(stack);
                            } else {
                                self.player_inventory.insert(i32::from(slot), stack);
                            }
                        }
                        Err(_) => {
                            self.player_inventory.remove(&i32::from(slot));
                        }
                    }
                    self.sync_hud_inventory();
                } else if let Some(screen) = &mut self.open_screen {
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
            PlayEvent::ContainerSetData {
                window_id,
                property,
                value,
            } => {
                // Vanilla property indices differ per menu (furnace:
                // 0 burn-left, 1 burn-max, 2 cook progress, 3 cook max;
                // brewing: 0 brew time; enchant: 0-2 level hints, 3 seed).
                // Store the raw pairs — the container UI's schema knows
                // which index maps to which bar.
                self.container_properties
                    .insert((i32::from(window_id), property), value);
                if self.container_properties.len() > 256 {
                    self.container_properties.clear();
                }
                if let Some(screen) = &mut self.open_screen {
                    if screen.window_id == i32::from(window_id) {
                        screen.properties.insert(property, value);
                    }
                }
                self.bump_hud_revision();
            }
            PlayEvent::SetCarriedItem(slot) => {
                // Server-forced held-item correction (`set_carried_item`
                // S2C) — mirror it into the hotbar highlight.
                let slot = i32::from(slot);
                if (0..9).contains(&slot) {
                    self.hud.selected_slot = slot as usize;
                    self.sync_hud_map();
                    self.bump_hud_revision();
                }
            }
            _ => {}
        }
    }

    fn apply_play_vitals_chat_event(&mut self, event: PlayEvent) {
        match event {
            PlayEvent::SetHealth { health, food, .. } => {
                if health < self.hud.health {
                    self.hurt_flash_alpha = 1.0;
                }
                self.hud.health = health;
                self.hud.hunger = food.clamp(0, 20) as u8;
                self.bump_hud_revision();
            }
            PlayEvent::SetExperience { bar, level, .. } => {
                // `total` already fed the death screen's Score line via
                // `death.apply`; the HUD wants bar progress + level.
                self.hud.xp_progress = bar.clamp(0.0, 1.0);
                self.hud.xp_level = level.max(0) as u32;
                self.bump_hud_revision();
            }
            PlayEvent::SystemChat { text, overlay } => {
                if overlay {
                    self.hud.actionbar = text.clone();
                    self.bump_hud_revision();
                } else {
                    self.push_chat_line(text.clone());
                }
                self.chat_message = Some(text);
            }
            PlayEvent::SetActionBarText { nbt_bytes } => {
                self.hud.actionbar = nbt_text(&nbt_bytes).unwrap_or_default();
                self.actionbar_nbt = Some(nbt_bytes);
                self.bump_hud_revision();
            }
            PlayEvent::PlayerChat {
                sender,
                index,
                signature,
                text,
                ..
            } => {
                // Vanilla renders `<sender> message` for the default
                // chat type; the display name wins when the tab list
                // has one for this sender UUID.
                let name = self
                    .player_info
                    .get(&sender)
                    .and_then(|entry| {
                        entry
                            .display_name_nbt
                            .as_deref()
                            .and_then(nbt_text)
                            .or_else(|| entry.name.clone())
                    })
                    .unwrap_or_else(|| uuid_label(&sender));
                self.push_chat_line_keyed(
                    format!("<{name}> {text}"),
                    Some(ChatMessageKey { index, signature }),
                );
                self.chat_message = Some(text);
            }
            PlayEvent::DisguisedChat { text, .. } => {
                // The sender is already baked into the undecorated text
                // (team/system chat type) — show it verbatim.
                self.push_chat_line(text.clone());
                self.chat_message = Some(text);
            }
            PlayEvent::CommandSuggestions {
                transaction_id,
                start,
                length,
                matches,
            } => self.apply_command_suggestions(ServerCommandSuggestions {
                transaction_id,
                start,
                length,
                matches: matches.iter().map(|m| m.text.clone()).collect(),
            }),
            PlayEvent::ChatCompletions { action, entries } => {
                self.apply_chat_completions(action, &entries)
            }
            PlayEvent::Cooldown { item, ticks } => self.apply_cooldown(item, ticks),
            PlayEvent::DeleteChat {
                message_index,
                signature,
            } => self.apply_delete_chat(message_index, signature),
            PlayEvent::ResetChat => self.apply_reset_chat(),
            _ => {}
        }
    }

    fn apply_play_progression_event(&mut self, event: PlayEvent) {
        match event {
            PlayEvent::UpdateAdvancements {
                reset,
                advancements,
                removed,
                progress,
            } => {
                if reset {
                    self.advancements.clear();
                    self.advancement_progress.clear();
                }
                for key in &removed {
                    self.advancements.remove(key);
                    self.advancement_progress.remove(key);
                }
                for wire in &advancements {
                    let (title, description, frame_type, hidden, x, y) = match &wire.display {
                        Some(display) => (
                            nbt_text(&display.title_nbt).unwrap_or_default(),
                            nbt_text(&display.description_nbt).unwrap_or_default(),
                            display.frame_type,
                            display.flags & 0x04 != 0,
                            display.x,
                            display.y,
                        ),
                        None => (String::new(), String::new(), 0, false, 0.0, 0.0),
                    };
                    self.advancements.insert(
                        wire.key.clone(),
                        LiveAdvancement {
                            key: wire.key.clone(),
                            parent_id: wire.parent_id.clone(),
                            title,
                            description,
                            frame_type,
                            hidden,
                            x,
                            y,
                            criteria_total: wire
                                .requirements
                                .iter()
                                .map(|group| group.len().max(1))
                                .sum(),
                        },
                    );
                }
                for wire in &progress {
                    let entry = self
                        .advancement_progress
                        .entry(wire.key.clone())
                        .or_default();
                    let mut done = 0usize;
                    for (criterion, achieved) in &wire.criteria {
                        let got = achieved.is_some();
                        if got {
                            done += 1;
                        }
                        entry.criteria.push((criterion.clone(), got));
                    }
                    entry.done = done;
                }
            }
            PlayEvent::SelectAdvancementsTab { tab } => {
                self.advancements_tab = tab;
            }
            _ => {}
        }
    }

    fn apply_play_overlay_scoreboard_event(&mut self, event: PlayEvent) {
        match event {
            PlayEvent::SetTitle {
                fade_in,
                stay,
                fade_out,
            } => {
                self.title_times = Some([fade_in, stay, fade_out]);
                self.bump_title_revisions();
            }
            PlayEvent::SetTitleText { nbt_bytes } => {
                self.title_text = nbt_text(&nbt_bytes);
                self.title_text_nbt = Some(nbt_bytes);
                self.bump_title_revisions();
            }
            PlayEvent::SetSubtitleText { nbt_bytes } => {
                self.subtitle_text = nbt_text(&nbt_bytes);
                self.subtitle_text_nbt = Some(nbt_bytes);
                self.bump_title_revisions();
            }
            PlayEvent::ClearTitles => {
                // `clear_titles` hides the live overlay immediately; the
                // stored fade/stay defaults stay armed for the next
                // `set_title_text`.
                self.title_text = None;
                self.subtitle_text = None;
                self.title_text_nbt = None;
                self.subtitle_text_nbt = None;
                self.bump_title_revisions();
            }
            PlayEvent::BossEvent {
                uuid,
                action,
                raw_payload,
            } => {
                if action == 1 {
                    self.boss_bars.remove(&uuid);
                } else {
                    let bar = self.boss_bars.entry(uuid).or_insert_with(|| LiveBossBar {
                        action,
                        raw_payload: Vec::new(),
                        title: String::new(),
                        health: 1.0,
                        color: 0,
                        divisions: 0,
                        flags: 0,
                    });
                    bar.action = action;
                    decode_boss_event_payload(bar, &raw_payload);
                    bar.raw_payload = raw_payload;
                }
                self.sync_hud_bossbars();
                self.bump_hud_revision();
            }
            PlayEvent::PlayerInfo { actions, entries } => {
                for update in &entries {
                    let entry = self.player_info.entry(update.uuid).or_insert_with(|| {
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
                self.sync_hud_players();
                self.bump_hud_revision();
            }
            PlayEvent::PlayerInfoRemove { uuids } => {
                for uuid in &uuids {
                    self.player_info.remove(uuid);
                }
                self.sync_hud_players();
                self.bump_hud_revision();
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
                self.bump_hud_revision();
            }
            PlayEvent::SetScore {
                owner,
                objective,
                value,
                ..
            } => {
                self.scoreboard.scores.insert((objective, owner), value);
                self.bump_hud_revision();
            }
            PlayEvent::SetDisplayObjective { slot, objective } => {
                self.scoreboard.display_slots.insert(slot, objective);
                self.bump_hud_revision();
            }
            PlayEvent::ResetScore { owner, objective } => {
                match objective {
                    Some(objective) => self
                        .scoreboard
                        .scores
                        .retain(|(obj, who), _| obj != &objective || who != &owner),
                    None => self.scoreboard.scores.retain(|(_, who), _| who != &owner),
                }
                self.bump_hud_revision();
            }
            _ => {}
        }
    }

    fn apply_play_combat_effect_event(&mut self, event: PlayEvent) {
        match event {
            PlayEvent::BlockDestruction {
                breaker_eid,
                pos,
                stage,
            } => {
                if (0..=9).contains(&stage)
                    && (self.remote_block_destruction.len() < 128
                        || self.remote_block_destruction.contains_key(&breaker_eid))
                {
                    self.remote_block_destruction
                        .insert(breaker_eid, (pos, stage as u8));
                } else {
                    self.remote_block_destruction.remove(&breaker_eid);
                }
            }
            PlayEvent::Explosion {
                x,
                y,
                z,
                power,
                records,
                knockback,
                sound_name,
                sound_id,
                ..
            } => {
                // Knockback delta applies to the local player's motion
                // like vanilla's `setDeltaMovement` on the packet.
                for (v, k) in self.physics_state.vel.iter_mut().zip(knockback.iter()) {
                    *v += f64::from(*k);
                }
                // `apply_play_event` already wrote the destruction
                // records into the world as air; mark every touched
                // section (plus face neighbours) for mesh rebuild and
                // re-sync acoustics once per touched chunk column.
                let base = [x.floor() as i32, y.floor() as i32, z.floor() as i32];
                let mut touched = BTreeSet::new();
                for r in &records {
                    let pos = [
                        base[0] + i32::from(r[0]),
                        base[1] + i32::from(r[1]),
                        base[2] + i32::from(r[2]),
                    ];
                    self.evict_kinetic_part((pos[0], pos[1], pos[2]));
                    self.mark_block_dirty(pos);
                    touched.insert((pos[0].div_euclid(16), pos[2].div_euclid(16)));
                }
                for (cx, cz) in touched {
                    self.acoustic_world.sync_chunk(&self.world, cx, cz);
                }
                if !records.is_empty() {
                    self.audio.mixer.refresh_acoustics();
                    self.terrain_dirty = true;
                }
                // Camera kick: the landing-dip impulse doubles as a
                // bounded shake — the micro-physics clamps keep even a
                // charged-creeper blast inside its `0.14`-block cap.
                let kick = if power.is_finite() {
                    power.clamp(0.0, 8.0) * 2.0
                } else {
                    0.0
                };
                self.tactile_landing_impulse = self.tactile_landing_impulse.max(kick);
                // Vanilla plays the packet's `explosion_sound` holder on
                // the BLOCKS source at volume 4.0 with pitch wandering
                // `0.6..0.8` — the mid stays deterministic here.
                let center = [x as f32, y as f32, z as f32];
                match (sound_name, sound_id) {
                    (Some(name), _) => self.emit_effect_sound(&name, 4, 4.0, 0.7, center),
                    (None, Some(id)) => self.emit_registry_sound(id, 4, 4.0, 0.7, center),
                    (None, None) => self.emit_effect_sound(
                        "minecraft:entity.generic.explode",
                        4,
                        4.0,
                        0.7,
                        center,
                    ),
                }
                // Blast burst: a crisp spark core at the centre plus a
                // sprinkle at a bounded sample of destroyed blocks.
                let tick = self.next_tactile_event_tick();
                self.tactile_sim.emit_impact_sparks(
                    center,
                    [0.0, 1.0, 0.0],
                    ImpactMaterial::CriticalHit,
                    tick,
                );
                for r in records.iter().take(6) {
                    let pos = [
                        (base[0] + i32::from(r[0])) as f32 + 0.5,
                        (base[1] + i32::from(r[1])) as f32 + 0.5,
                        (base[2] + i32::from(r[2])) as f32 + 0.5,
                    ];
                    self.tactile_sim.emit_impact_sparks(
                        pos,
                        [0.0, 1.0, 0.0],
                        ImpactMaterial::OreOrStone,
                        tick,
                    );
                }
            }
            PlayEvent::LevelEvent {
                event, pos, data, ..
            } => self.apply_level_event(event, pos, data),
            PlayEvent::BlockEvent {
                pos,
                action,
                parameter,
                ..
            } => self.apply_block_event(pos, action, parameter),
            PlayEvent::LevelParticles {
                particle_id,
                x,
                y,
                z,
                count,
                ..
            } => self.apply_level_particles(particle_id, count, [x, y, z]),
            _ => {}
        }
    }

    fn apply_play_session_service_event(&mut self, event: PlayEvent) {
        match event {
            PlayEvent::PlayerCombatKill { .. } => {
                // `death.apply` above already folded the kill into the
                // death loop (dead + cause + victim id); the UI sync
                // turns it into `Screen::Death`.
                self.sync_hud_combat();
            }
            PlayEvent::Transfer { host, port } => match handle_transfer_packet(&host, port) {
                Ok(target) => self.pending_transfer = Some(target),
                Err(reason) => {
                    self.status = SessionStatus::Disconnected(reason);
                }
            },
            PlayEvent::Ping(id) => {
                // The worker answers the `pong` at read time (never gated
                // on render-thread drain); this records the probe so the
                // event is observably consumed session-side too.
                self.last_server_ping = Some(id);
            }
            PlayEvent::PlayerAbilities {
                flags,
                flying_speed,
                walking_speed,
            } => {
                // The server is authoritative: a flag sync can grant or
                // revoke `may_fly` (game-mode change) and force-drop
                // `flying`, so fold the whole byte in and mirror the
                // flying bit into the local physics prediction.
                self.player_abilities = LivePlayerAbilities {
                    flags,
                    flying_speed,
                    walking_speed,
                };
                self.physics_state.is_creative_flying =
                    self.player_abilities.may_fly() && self.player_abilities.flying();
            }
            PlayEvent::ResourcePackPush {
                uuid, forced, url, ..
            } => {
                // Track the pushed pack, then answer promptly. The client
                // never downloads arbitrary remote URLs; acknowledging
                // with ACCEPTED + SUCCESSFULLY_LOADED keeps required-pack
                // servers from timing the client out, and the `forced`
                // flag is surfaced through the recorded stack.
                if self.server_resource_packs.len() >= 32 {
                    self.server_resource_packs.pop_front();
                }
                self.server_resource_packs.push_back(uuid);
                self.send_resource_pack_response(
                    uuid,
                    megu_client::protocol::RESOURCE_PACK_ACCEPTED,
                );
                self.send_resource_pack_response(
                    uuid,
                    megu_client::protocol::RESOURCE_PACK_SUCCESSFULLY_LOADED,
                );
                let _ = (forced, url);
            }
            PlayEvent::ResourcePackPop { uuid } => {
                match uuid {
                    Some(uuid) => {
                        // A targeted pop removes just that pack; the
                        // DISCARDED echo tells the server the pack is
                        // gone client-side.
                        self.server_resource_packs.retain(|known| *known != uuid);
                        self.send_resource_pack_response(
                            uuid,
                            megu_client::protocol::RESOURCE_PACK_DISCARDED,
                        );
                    }
                    None => {
                        // UUID-less pops clear every server pack (vanilla
                        // applies the same all-or-nothing rule).
                        let drained: Vec<[u8; 16]> = self.server_resource_packs.drain(..).collect();
                        for uuid in drained {
                            self.send_resource_pack_response(
                                uuid,
                                megu_client::protocol::RESOURCE_PACK_DISCARDED,
                            );
                        }
                    }
                }
            }
            PlayEvent::Disconnect(reason) => self.status = SessionStatus::Disconnected(reason),
            _ => {}
        }
    }

    /// Session-side `game_event` handling layered over the world's
    /// weather mirror: game-mode flips drive the HUD vitals gate,
    /// `enable_respawn_screen(0)` is vanilla immediate respawn, demo
    /// events surface as chat hints, and the arrow/pufferfish/guardian
    /// ids play their player-source stingers.
    fn apply_game_event(&mut self, event: u8, value: f32) {
        match event {
            8 => self.thunder_flash = self.thunder_flash.max(value.clamp(0.0, 1.0)),
            GAME_EVENT_NO_RESPAWN_BLOCK => {
                // `block.minecraft.spawn.not_valid` — vanilla shows it
                // as a screen message when respawn finds no bed/anchor.
                self.push_chat_line(
                    "You have no home bed or charged respawn anchor, or it was obstructed"
                        .to_string(),
                );
            }
            GAME_EVENT_CHANGE_GAME_MODE => {
                let mode = if value.is_finite() { value as i32 } else { 0 };
                self.game_mode = mode;
                self.hud.game_mode = mode;
                self.hud_revision += 1;
            }
            GAME_EVENT_WIN_GAME => {
                // Protocol 767: value 0 immediately requests `PERFORM_RESPAWN`
                // so exiting the End never strands the player; value != 0 opens
                // the End Credits screen (respawn triggers on close/finish).
                if value == 0.0 {
                    self.pending_end_credits = false;
                    self.send_end_credits_complete();
                } else {
                    self.pending_end_credits = true;
                }
            }
            GAME_EVENT_DEMO => {
                if let Some(line) = demo_hint_line(value as i32) {
                    self.push_chat_line(line.to_string());
                }
            }
            GAME_EVENT_ARROW_HIT_PLAYER => {
                // `entity.arrow.hit_player` plays on the local player
                // (PLAYERS source), not at a world position.
                self.emit_player_sound("minecraft:entity.arrow.hit_player", 0.2, 1.0);
            }
            GAME_EVENT_PUFFERFISH_STING => {
                self.emit_player_sound("minecraft:entity.puffer_fish.sting", 1.0, 1.0);
            }
            GAME_EVENT_ELDER_GUARDIAN_APPEARANCE => {
                // Vanilla flashes the ghost elder-guardian overlay over
                // the HUD — there is no overlay channel for it yet, so
                // the curse stinger carries the whole effect.
                self.emit_player_sound("minecraft:entity.elder_guardian.curse", 1.0, 1.0);
            }
            GAME_EVENT_ENABLE_RESPAWN_SCREEN => {
                self.respawn_screen_enabled = value != 0.0;
                // Flipping to immediate respawn while already dead re-
                // requests `PERFORM_RESPAWN` on the spot.
                if !self.respawn_screen_enabled && self.death.is_dead() {
                    self.request_respawn();
                }
            }
            _ => {}
        }
    }

    fn sync_hud_status_effects(&mut self) {
        self.hud.status_effects = self
            .live_status_effect_snapshot()
            .into_iter()
            .filter(|effect| effect.visible_in_hud())
            .map(|effect| effect.as_hud_state())
            .collect();
        self.hud_revision = self.hud_revision.wrapping_add(1);
    }

    /// Advance per-frame gameplay animation, overlay, effect and ambience state.
    pub fn tick_gameplay_frame(&mut self, dt_secs: f32) {
        if !dt_secs.is_finite() || dt_secs <= 0.0 {
            return;
        }
        let dt = dt_secs.min(0.25);
        self.swing_progress = (self.swing_progress + dt / 0.3).min(1.0);
        self.attack_cooldown = (self.attack_cooldown + dt * self.attack_speed / 0.9).min(1.0);
        self.hud.attack_cooldown = self.attack_cooldown;
        self.hurt_flash_alpha = (self.hurt_flash_alpha - dt * 1.8).max(0.0);
        if let Some(hurt) = &mut self.local_hurt {
            hurt.elapsed_secs += dt;
            if hurt.elapsed_secs >= LocalHurtAnimation::DURATION_SECS {
                self.local_hurt = None;
            }
        }
        self.thunder_flash = (self.thunder_flash - dt * 1.5).max(0.0);
        self.ambient_bgm_clock += dt;
        self.ambient_bgm_cooldown = (self.ambient_bgm_cooldown - dt).max(0.0);
        self.status_effect_accumulator += dt;
        let effect_ticks = (self.status_effect_accumulator / 0.05).floor() as i32;
        if effect_ticks > 0 {
            self.status_effect_accumulator -= effect_ticks as f32 * 0.05;
            if let Some(effects) = self
                .player_entity_id
                .and_then(|id| self.mob_effects.get_mut(&id))
            {
                for effect in effects.values_mut() {
                    if effect.duration > 0 {
                        effect.duration = (effect.duration - effect_ticks).max(0);
                    }
                }
                effects.retain(|_, effect| effect.duration != 0);
                self.sync_hud_status_effects();
            }
        }
        let flags = self
            .player_entity_id
            .and_then(|id| self.entity_metadata.get(&id))
            .and_then(|meta| meta.flags)
            .unwrap_or(0);
        let overlay = megu_ui::state::HudScreenOverlay {
            hurt_alpha: self.hurt_flash_alpha,
            fire_alpha: if flags & 0x01 != 0 { 0.75 } else { 0.0 },
            underwater_alpha: if self.physics_state.eye_in_water {
                1.0
            } else {
                0.0
            },
            portal_alpha: self.hud.screen_overlay.portal_alpha,
            freezing_alpha: self.hud.screen_overlay.freezing_alpha,
            pumpkin_blur: self.hud.screen_overlay.pumpkin_blur,
            vignette_alpha: self
                .hud
                .screen_overlay
                .vignette_alpha
                .max(self.thunder_flash),
        };
        self.hud.screen_overlay = overlay;
        self.maybe_send_ping_probe();
        self.sync_survival_hud();
        if self.ambient_bgm_cooldown <= 0.0 && self.ambient_bgm_clock >= 600.0 {
            let name = if self.world.dimension.name == "minecraft:the_nether" {
                "minecraft:music.nether.nether_wastes"
            } else if self.world.dimension.name == "minecraft:the_end" {
                "minecraft:music.end"
            } else if self.hud.game_mode == 1 {
                "minecraft:music.creative"
            } else if self.physics_state.eye_in_water {
                "minecraft:music.under_water"
            } else {
                "minecraft:music.game"
            };
            self.emit_player_sound(name, 1.0, 1.0);
            self.ambient_bgm_clock = 0.0;
            self.ambient_bgm_cooldown = 600.0;
        }
    }

    pub fn gameplay_screen_overlay(&self) -> GameplayScreenOverlay {
        GameplayScreenOverlay {
            hurt_alpha: self.hud.screen_overlay.hurt_alpha,
            fire_alpha: self.hud.screen_overlay.fire_alpha,
            underwater_alpha: self.hud.screen_overlay.underwater_alpha,
            portal_alpha: self.hud.screen_overlay.portal_alpha,
            freezing_alpha: self.hud.screen_overlay.freezing_alpha,
            pumpkin_alpha: if self.hud.screen_overlay.pumpkin_blur {
                1.0
            } else {
                0.0
            },
            vignette_alpha: self.hud.screen_overlay.vignette_alpha,
            time_secs: self.ambient_bgm_clock,
        }
    }

    pub fn thunder_flash(&self) -> f32 {
        self.thunder_flash
    }

    pub fn take_pending_end_credits(&mut self) -> bool {
        std::mem::replace(&mut self.pending_end_credits, false)
    }

    pub fn play_ui_click(&mut self) {
        self.emit_player_sound("minecraft:ui.button.click", 0.25, 1.0);
    }

    pub fn apply_biome_colormaps(
        &mut self,
        assets: &crate::vanilla_world_assets::VanillaWorldAssets,
    ) {
        self.has_vanilla_cloud_texture = assets
            .env_textures
            .get(&crate::vanilla_world_assets::VanillaEnvTexture::Clouds)
            .is_some_and(|texture| {
                // Distinguish the built-in grey checkerboard from a cloud
                // texture supplied by the client JAR or a resource pack.
                texture.from_jar
                    || (texture.width, texture.height) != (64, 64)
                    || texture
                        .rgba
                        .as_chunks::<4>()
                        .0
                        .iter()
                        .any(|pixel| pixel[..3] != [105, 125, 150])
            });
        let grass = assets
            .colormaps
            .get(&crate::vanilla_world_assets::VanillaColormap::Grass)
            .map(|texture| texture.rgba.clone());
        let foliage = assets
            .colormaps
            .get(&crate::vanilla_world_assets::VanillaColormap::Foliage)
            .map(|texture| texture.rgba.clone());
        if let Ok(map) = megu_client::world::BiomeColormap::from_rgba(grass, foliage) {
            let mut resolver = megu_client::world::BiomeTintResolver::default();
            resolver.set_colormap(map);
            self.world.blocks.set_biome_tint_resolver(resolver);
        }
    }

    /// `level_event` id → spatial sound (+ the tactile spark burst for
    /// the block-break family). The bounded record queue in
    /// [`WorldState::level_events`](megu_client::world::WorldState) keeps
    /// the history; this is the live feedback half. Ids follow the
    /// vanilla `LevelEvent` ordinals.
    fn apply_level_event(&mut self, event: i32, pos: [i32; 3], data: i32) {
        let center = [
            pos[0] as f32 + 0.5,
            pos[1] as f32 + 0.5,
            pos[2] as f32 + 0.5,
        ];
        if event == 2001 {
            // Block break: `data` is the destroyed block-state id —
            // derive the family break sound and emit the crack burst.
            let sound =
                state_block_name(&self.world, self.mod_registry.as_ref(), data.max(0) as u32)
                    .map(break_sound_for_block)
                    .unwrap_or("minecraft:block.stone.break");
            self.emit_effect_sound(sound, 4, 1.0, 0.8, center);
            let tick = self.next_tactile_event_tick();
            self.tactile_sim.emit_impact_sparks(
                center,
                [0.0, 1.0, 0.0],
                ImpactMaterial::OreOrStone,
                tick,
            );
            return;
        }
        if matches!(event, 1505 | 2005) {
            self.on_tree_shake(center, None, (data.max(1) as f32 / 20.0).clamp(0.2, 1.0));
        } else if matches!(event, 2002 | 2007) {
            let tick = self.next_tactile_event_tick();
            self.tactile_sim.emit_fluid_entry(&FluidEntryEvent {
                pos: center,
                impact_speed: 0.0,
                body: megu_client_mesh::tactile_effects::FluidEntryBody::PlayerOrMediumMob,
                is_lava: false,
                seed_tick: tick,
            });
        }
        if let Some((name, category, volume, pitch)) = level_event_sound(event, data) {
            self.emit_effect_sound(name, category, volume, pitch, center);
        }
    }

    /// `block_event` → spatial sound resolved from the block actually
    /// at `pos` (pistons, chests/barrels, note blocks, bells). The wire
    /// carries a raw block *id* the client cannot map without the
    /// registry, so the live state wins.
    fn apply_block_event(&mut self, pos: [i32; 3], action: u8, parameter: u8) {
        let center = [
            pos[0] as f32 + 0.5,
            pos[1] as f32 + 0.5,
            pos[2] as f32 + 0.5,
        ];
        let name = self.block_name_at(pos);
        if let Some(sound) = block_event_sound(&name, action, parameter) {
            self.emit_effect_sound(sound, 4, 0.5, 1.0, center);
        }
    }

    /// `level_particles` → tactile spark stand-ins for the combat/blast
    /// particle families the renderer has no generic emitter for.
    /// Everything else stays cosmetic-only in the bounded world queue.
    fn apply_level_particles(&mut self, particle_id: i32, count: i32, pos: [f64; 3]) {
        // Vanilla particle-palette ids (alphabetical order): combat
        // hits get the bright spark burst, the explosion emitter maps
        // to debris sparks. Everything else stays cosmetic-only in the
        // bounded world queue until a generic particle stage lands.
        let material = match particle_id {
            14 | 16 | 33 => Some(ImpactMaterial::CriticalHit), // crit, damage_indicator, enchanted_hit
            36 => Some(ImpactMaterial::OreOrStone),            // explosion
            _ => None,
        };
        let Some(material) = material.filter(|_| count > 0) else {
            return;
        };
        let pos = [pos[0] as f32, pos[1] as f32, pos[2] as f32];
        let tick = self.next_tactile_event_tick();
        self.tactile_sim
            .emit_impact_sparks(pos, [0.0, 1.0, 0.0], material, tick);
    }

    /// Play one named spatial sound through the live audio runtime —
    /// the synthesized counterpart of a `sound_effect` packet for
    /// events that carry an effect id instead of a sound holder.
    fn emit_effect_sound(
        &mut self,
        name: &str,
        category: i32,
        volume: f32,
        pitch: f32,
        position: [f32; 3],
    ) {
        self.audio.ingest(&WireSoundEvent {
            name: Some(name.to_string()),
            registry_id: None,
            category,
            position: Some(position),
            entity_id: None,
            volume,
            pitch,
            seed: 0,
            fixed_range: None,
        });
    }

    /// [`Self::emit_effect_sound`] for a `sound_event` registry id —
    /// resolution flows through the synced registry table like the
    /// packet-decoded `registry_id` path.
    fn emit_registry_sound(
        &mut self,
        id: u32,
        category: i32,
        volume: f32,
        pitch: f32,
        position: [f32; 3],
    ) {
        self.audio.ingest(&WireSoundEvent {
            name: None,
            registry_id: Some(id),
            category,
            position: Some(position),
            entity_id: None,
            volume,
            pitch,
            seed: 0,
            fixed_range: None,
        });
    }

    /// Play one named sound on the PLAYERS source anchored to the local
    /// player (`None` position = non-positional when the pose is not
    /// known yet).
    fn emit_player_sound(&mut self, name: &str, volume: f32, pitch: f32) {
        let position = self.pose.map(|pose| pose.feet.map(|coord| coord as f32));
        self.audio.ingest(&WireSoundEvent {
            name: Some(name.to_string()),
            registry_id: None,
            category: 7,
            position,
            entity_id: None,
            volume,
            pitch,
            seed: 0,
            fixed_range: None,
        });
    }

    /// Vanilla `handleRespawn` for a respawn that stayed in the same
    /// dimension: the level — chunks, light and every GPU-resident section —
    /// survives, so only the local player the session re-creates is reset.
    /// `handleRespawn` closes the screen on every respawn
    /// (`this.minecraft.screen = null`), and the fresh `LocalPlayer` starts
    /// with no vehicle, no item cooldowns and no active effects. Health,
    /// food and equipment stay server-authoritative: the server re-sends
    /// them right after the respawn packet.
    fn reset_local_player_for_respawn(&mut self) {
        self.pose = None;
        self.render_frame_pose = None;
        self.reset_physics_interpolation();
        self.last_motion_delta = [0.0; 2];
        self.open_screen = None;
        self.riding_vehicle = None;
        self.remote_block_destruction.clear();
        self.item_cooldowns.clear();
        self.sync_hud_item_cooldowns();
        if let Some(player) = self.player_entity_id {
            self.mob_effects.remove(&player);
            self.entity_metadata.remove(&player);
        }
        self.hurt_flash_alpha = 0.0;
        self.local_hurt = None;
        self.pending_end_credits = false;
        self.bump_hud_revision();
    }

    fn reset_dimension(&mut self) {
        self.sync_hud_combat();
        self.acoustic_world.reset_dimension(&self.world);
        self.audio.mixer.refresh_acoustics();
        self.pending_end_credits = false;
        self.entity_positions.clear();
        self.entity_metadata.clear();
        self.mob_effects.clear();
        // Entity visual state is dimension-scoped: rigs, equipment, the
        // vehicle anchor, crack overlays and cooldowns all key off entity
        // ids that the new dimension reuses for different mobs, so a stale
        // entry would draw an old model, hang a dead vehicle anchor, keep a
        // destroyed block under a crack overlay, or leave a hotbar wipe
        // running in the wrong world.
        self.entity_rigs.clear();
        self.entity_equipment.clear();
        self.riding_vehicle = None;
        self.remote_block_destruction.clear();
        self.item_cooldowns.clear();
        self.sync_hud_item_cooldowns();
        self.open_screen = None;
        self.terrain_dirty = true;
        // Every resident coordinate may vanish — a full reconcile is the
        // only honest answer before dirty tracking resumes.
        self.terrain_full_sync = true;
        self.dirty_terrain_coords.clear();
        // The off-thread lane's epochs, candidates and published payloads
        // all reference the old dimension's sections — drop them wholesale
        // (in-flight jobs finish and drain as stale/unloaded).
        self.clear_external_sections();
        self.offthread_candidates.clear();
        self.section_mesh_lane.clear();
        // The dimension's whole block store swapped out under the same
        // section coordinates — the placed-light scan cache cannot
        // survive even if every key field happens to compare equal.
        self.placed_light_scan.get_mut().valid = false;
        self.pose = None;
        self.render_frame_pose = None;
        self.reset_physics_interpolation();
        self.last_motion_delta = [0.0; 2];
        self.prefetch_arrival_ring.clear();
        self.prefetch_last_jump_delta = None;
        self.chunk_fades = ChunkFadeTracker::new();
        self.prefetcher = SpeculativeChunkPrefetcher::new();
        // Interaction bookkeeping is dimension-scoped: acks for the old
        // dimension's predicted positions must not re-dirty the new one,
        // and the server re-sends `set_center_chunk` after respawn.
        self.pending_interaction_seqs.clear();
        self.view_center_chunk = None;
        self.command_suggestions = None;
        // Kinetic render instances are dimension-scoped too. Keep the
        // `(dimension_id, pos)` telemetry store intact for return trips, but
        // drop every visual id so an old-dimension rotor cannot draw into
        // the new dimension at the same bare coordinates.
        let kinetic_ids: Vec<u32> = self.kinetic_pos_to_id.values().copied().collect();
        for id in kinetic_ids {
            self.kinetic_live_angles.remove(&id);
            self.remove_kinetic_part(id);
        }
        self.kinetic_pos_to_id.clear();
        self.kinetic_telemetry.clear();
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
        // The atmosphere gate on layered voxel clouds flips with the
        // dimension (Nether/End/vacuum lose `has_skylight` and the fog
        // blend restarts at a new density) — drop the cached sheets so
        // the next `refresh_voxel_clouds` evaluates the gate fresh
        // instead of matrix-sliding a stale overworld mesh:
        // `remove_cloud_decks` drops all three deck ids and their
        // per-deck mesh caches.
        self.remove_cloud_decks();
    }

    /// Deterministic integration-test hook for custom-channel consumers.
    /// Production traffic still enters through the worker/event path; this
    /// wrapper simply avoids duplicating the channel dispatch table.
    #[doc(hidden)]
    pub fn ingest_custom_channel_payload_for_test(&mut self, channel: &str, payload: &[u8]) {
        let _ = self.apply_custom(ClientCustomChannelEvent::Unknown {
            channel: channel.to_owned(),
            payload: payload.to_vec(),
        });
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
            ClientCustomChannelEvent::Unknown { channel, payload }
                if matches!(
                    channel.as_str(),
                    CHANNEL_CREATE_KINETIC_SYNC | CHANNEL_CREATE_KINETIC_UPDATE
                ) =>
            {
                self.ingest_kinetic_payload(&payload);
            }
            ClientCustomChannelEvent::Unknown { channel, payload }
                if channel.as_str() == CHANNEL_MOD_SYNC_V1 =>
            {
                self.apply_mod_sync_manifest(&payload);
            }
            ClientCustomChannelEvent::Unknown { channel, payload }
                if channel.as_str() == CHANNEL_MOD_REGISTRY_SYNC =>
            {
                match decode_mrs1_frame(&payload) {
                    Ok(registry) => self.install_mrs1_dynamic_registry(registry),
                    Err(_) => self.mod_registry_rejected += 1,
                }
            }
            ClientCustomChannelEvent::Unknown { channel, payload }
                if channel.as_str() == SUBWORLD_SYNC_CHANNEL =>
            {
                if let Ok(frame) = decode_sync_frame(&payload) {
                    self.subworlds.apply_sync_frame(&frame);
                    // Legacy v1 snapshots have no dimension field; their rows
                    // are dimension zero by contract. `apply_sync_frame` cannot
                    // infer that scope from an empty row set, so the session
                    // ingest path reconciles the empty legacy baseline here.
                    if payload.len() == 7
                        && payload.starts_with(&SUBWORLD_SYNC_MAGIC)
                        && payload.get(4) == Some(&1)
                        && frame.poses.is_empty()
                    {
                        let stale: Vec<u64> = self
                            .subworlds
                            .entries()
                            .filter(|entry| {
                                matches!(entry.owner, SubworldOwner::Network { dim_id: 0, .. })
                            })
                            .map(|entry| entry.id)
                            .collect();
                        for id in stale {
                            // If a client-local contraption reuses the same id,
                            // preserve it rather than deleting the wrong owner;
                            // such a collision cannot be represented through the
                            // legacy unscoped `remove` API.
                            if self.subworlds.get(id).is_some_and(|entry| {
                                matches!(entry.owner, SubworldOwner::Network { dim_id: 0, .. })
                            }) {
                                self.subworlds.remove(id);
                            }
                        }
                    }
                    self.subworld_sync_received_at = Some(Instant::now());
                    self.player_subworld_id = frame
                        .poses
                        .iter()
                        .find(|pose| pose.flags & SUBWORLD_POSE_PLAYER_ABOARD != 0)
                        .map(|pose| pose.subworld_id);
                }
            }
            ClientCustomChannelEvent::Unknown { channel, payload }
                if channel.as_str() == SEAMLESS_PORTAL_CHANNEL =>
            {
                const SEAMLESS_PORTAL_FRAME_CAP: usize = 16;
                if self.seamless_portal_frames.len() >= SEAMLESS_PORTAL_FRAME_CAP {
                    self.seamless_portal_frames.pop_front();
                }
                self.seamless_portal_frames.push_back(payload);
            }
            ClientCustomChannelEvent::Unknown { channel, payload } => {
                // Generic mod channel (any valid `namespace:path` resource
                // location outside megu's typed family): validate, track
                // `minecraft:register`/`unregister` and queue the frame so a
                // mod consumer can drain it. Previously this arm discarded
                // everything outside the hardcoded channel list.
                self.custom_payload_inbox.accept(&channel, &payload);
            }
        }
        Ok(())
    }

    /// Drain queued `megu:portal_seamless_v1` (`MSP1`) channel payloads in
    /// arrival order; the window forwards each raw frame into the
    /// seamless dimension-transfer state machine.
    pub fn drain_seamless_portal_frames(&mut self) -> Vec<Vec<u8>> {
        self.seamless_portal_frames.drain(..).collect()
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

#[allow(clippy::too_many_arguments)]
fn run_worker(
    host: String,
    port: u16,
    username: String,
    version: String,
    login_uuid: [u8; 16],
    auth: OnlineAuthOptions,
    tx: SyncSender<WorkerMessage>,
    rx: Receiver<WorkerCommand>,
    view_distance: i8,
) {
    let result = run_connection(
        &host,
        port,
        &username,
        &version,
        login_uuid,
        auth,
        &tx,
        rx,
        view_distance,
    );
    if let Err(error) = result {
        let _ = tx.send(WorkerMessage::Closed(error));
    }
}

fn encode_custom_payload_body(channel: &str, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(channel.len() + payload.len() + 5);
    let mut value = channel.len() as u32;
    loop {
        if value & !0x7f == 0 {
            out.push(value as u8);
            break;
        }
        out.push(((value & 0x7f) | 0x80) as u8);
        value >>= 7;
    }
    out.extend_from_slice(channel.as_bytes());
    out.extend_from_slice(payload);
    out
}

fn play_custom_payload_packet_id(protocol_number: i32) -> i32 {
    if protocol_number == 777 { 0x16 } else { 0x12 }
}

#[allow(clippy::too_many_arguments)]
fn run_connection(
    host: &str,
    port: u16,
    username: &str,
    version: &str,
    login_uuid: [u8; 16],
    auth: OnlineAuthOptions,
    tx: &SyncSender<WorkerMessage>,
    rx: Receiver<WorkerCommand>,
    view_distance: i8,
) -> Result<(), String> {
    let protocol = megu_client::cli::parse_mc_version(version)?;
    let (mut client, _) = ProtocolClient::connect_for_version_with_auth(
        host,
        port,
        username,
        login_uuid,
        view_distance,
        Duration::from_secs(8),
        protocol,
        auth,
    )?;
    // GPU session owns the sink and spatial world. Prevent any global
    // headless audio configuration from creating a second active playback.
    client.audio = None;
    if tx.send(WorkerMessage::Connected).is_err() {
        return Ok(());
    }
    let mut last_input: Option<(PlayerMovement, bool)> = None;
    let mut last_send = Instant::now();
    // The protocol loop must never block on render-thread backpressure:
    // keep-alive and chunk-batch acknowledgements are written at read time,
    // so a stalled render side (atlas/pipeline compile during join) would
    // otherwise freeze reads long enough for the server to kick us with a
    // chunk-batch-ack timeout. Forwarding therefore goes through a bounded
    // overflow queue instead of a blocking channel send.
    let mut pending: VecDeque<WorkerMessage> = VecDeque::new();
    const PENDING_CAP: usize = 8192;
    let enqueue =
        |pending: &mut VecDeque<WorkerMessage>, msg: WorkerMessage| -> Result<(), String> {
            if pending.is_empty() {
                match tx.try_send(msg) {
                    Ok(()) => return Ok(()),
                    Err(TrySendError::Full(msg)) => pending.push_back(msg),
                    Err(TrySendError::Disconnected(_)) => return Ok(()),
                }
            } else {
                pending.push_back(msg);
            }
            if pending.len() > PENDING_CAP {
                return Err("render thread stalled: net→render backlog exceeded cap".to_string());
            }
            Ok(())
        };
    loop {
        while let Some(msg) = pending.pop_front() {
            match tx.try_send(msg) {
                Ok(()) => {}
                Err(TrySendError::Full(msg)) => {
                    pending.push_front(msg);
                    break;
                }
                Err(TrySendError::Disconnected(_)) => return Ok(()),
            }
        }
        loop {
            match rx.try_recv() {
                Ok(WorkerCommand::Movement {
                    movement,
                    on_ground,
                }) => {
                    last_input = Some((movement, on_ground));
                }
                Ok(WorkerCommand::Chat(message)) => {
                    client.send_chat_message(&message)?;
                }
                Ok(WorkerCommand::CommandSuggestion {
                    transaction_id,
                    text,
                }) => {
                    client.send_command_suggestion(transaction_id, &text)?;
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
                        window_id, state_id, slot, button, mode, &borrowed, &carried,
                    )?;
                }
                Ok(WorkerCommand::ContainerButtonClick {
                    container_id,
                    button_id,
                }) => {
                    client.send_container_button_click(container_id, button_id)?;
                }
                Ok(WorkerCommand::ContainerClose { window_id }) => {
                    client.send_container_close(window_id)?;
                }
                Ok(WorkerCommand::RenameItem(name)) => {
                    client.send_rename_item(&name)?;
                }
                Ok(WorkerCommand::SelectTrade { index }) => {
                    client.send_select_trade(index)?;
                }
                Ok(WorkerCommand::SetBeacon { primary, secondary }) => {
                    client.send_set_beacon(primary, secondary)?;
                }
                Ok(WorkerCommand::ContainerSlotStateChanged {
                    slot_id,
                    window_id,
                    state,
                }) => {
                    client.send_container_slot_state_changed(slot_id, window_id, state)?;
                }
                Ok(WorkerCommand::SeenAdvancements { tab }) => {
                    client.send_seen_advancements(tab.as_deref())?;
                }
                Ok(WorkerCommand::PlaceRecipe {
                    window_id,
                    recipe,
                    make_all,
                }) => {
                    client.send_place_recipe(window_id, &recipe, make_all)?;
                }
                Ok(WorkerCommand::ClientCommand { action }) => {
                    client.send_client_command(action)?;
                }
                Ok(WorkerCommand::Interact {
                    target,
                    action,
                    at,
                    hand,
                    sneaking,
                }) => {
                    client.send_interact(target, action, at, hand, sneaking)?;
                }
                Ok(WorkerCommand::SetCarriedItem { slot }) => {
                    client.send_set_carried_item(slot)?;
                }
                Ok(WorkerCommand::PlayerCommand {
                    entity_id,
                    action,
                    jump_boost,
                }) => {
                    client.send_player_command(entity_id, action, jump_boost)?;
                }
                Ok(WorkerCommand::PlayerAbilities { flags }) => {
                    client.send_player_abilities(flags)?;
                }
                Ok(WorkerCommand::ReleaseUseItem) => client.send_release_use_item()?,
                Ok(WorkerCommand::MoveVehicle {
                    x,
                    y,
                    z,
                    yaw,
                    pitch,
                }) => client.send_move_vehicle(x, y, z, yaw, pitch)?,
                Ok(WorkerCommand::PaddleBoat { left, right }) => {
                    client.send_paddle_boat(left, right)?
                }
                Ok(WorkerCommand::PickItem { slot }) => client.send_pick_item(slot)?,
                Ok(WorkerCommand::CreativeModeSlot {
                    slot,
                    item_id,
                    count,
                }) => client.send_set_creative_mode_slot(slot, item_id, count)?,
                Ok(WorkerCommand::EditBook { slot, pages, title }) => {
                    let pages: Vec<&str> = pages.iter().map(String::as_str).collect();
                    client.send_edit_book(slot, &pages, title.as_deref())?;
                }
                Ok(WorkerCommand::UpdateSign {
                    pos,
                    is_front,
                    lines,
                }) => {
                    let payload = encode_sign_update_payload(pos, is_front, &lines);
                    client.send_raw(sign_update_packet_id(client.protocol_version), &payload)?;
                }
                Ok(WorkerCommand::ResourcePackResponse { uuid, status }) => {
                    client.send_resource_pack(uuid, status)?;
                }
                Ok(WorkerCommand::ClientInformation { view_distance }) => {
                    client.send_client_information(view_distance)?;
                }
                Ok(WorkerCommand::PingRequest(id)) => {
                    client.send_ping_request(id)?;
                }
                Ok(WorkerCommand::SendCustomPayload { channel, payload }) => {
                    let wire = encode_custom_payload_body(&channel, &payload);
                    let packet_id = play_custom_payload_packet_id(client.protocol_version.number());
                    client.send_raw(packet_id, &wire)?;
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => return Ok(()),
            }
        }
        if last_send.elapsed() >= Duration::from_millis(50) {
            if let Some((input, on_ground)) = last_input {
                let pose = input.pose;
                client.send_player_position_rotation(
                    pose.feet[0],
                    pose.feet[1],
                    pose.feet[2],
                    pose.yaw,
                    pose.pitch,
                    on_ground,
                )?;
                // The 767 float is Vanilla's left-positive impulse. Newer
                // protocols encode left/right flags from right-positive input.
                let sideways = if client.protocol_version.number() == 767 {
                    -input.strafe
                } else {
                    input.strafe
                };
                client.send_player_input(sideways, input.forward, input.jump, input.sneak)?;
            }
            last_send = Instant::now();
        }
        match client.recv_play_with_custom_channels(Duration::from_millis(20))? {
            Some(ClientPlayEvent::Play(PlayEvent::KeepAlive(id))) => client.send_keep_alive(id)?,
            Some(ClientPlayEvent::Play(PlayEvent::Ping(id))) => {
                // Server sync probes get their `pong` at read time like
                // keep-alives — a stalled render thread must never delay
                // them into a timeout kick. The event is still forwarded
                // so `apply_play` can record it.
                client.send_pong(id)?;
                enqueue(
                    &mut pending,
                    WorkerMessage::Play(ClientPlayEvent::Play(PlayEvent::Ping(id))),
                )?;
            }
            Some(ClientPlayEvent::Play(PlayEvent::ChunkBatchFinished(count))) => {
                // Vanilla reports the chunks-per-tick rate it actually
                // consumed; echoing the server window would request a batch
                // larger than the render side can absorb during join.
                client.send_chunk_batch_received(count.clamp(1, 10) as f32)?;
            }
            Some(ClientPlayEvent::Play(PlayEvent::Teleport(ref teleport))) => {
                let previous_pose = last_input.map(|(movement, _)| movement.pose);
                let pose = PlayerPose::from_teleport(teleport, previous_pose);
                client.send_accept_teleportation(
                    teleport.teleport_id,
                    pose.feet,
                    pose.yaw,
                    pose.pitch,
                    false,
                )?;
                enqueue(
                    &mut pending,
                    WorkerMessage::Play(ClientPlayEvent::Play(PlayEvent::Teleport(
                        teleport.clone(),
                    ))),
                )?;
                last_input = Some((
                    PlayerMovement {
                        pose,
                        strafe: 0.0,
                        forward: 0.0,
                        jump: false,
                        sneak: false,
                    },
                    last_input.map(|(_, on_ground)| on_ground).unwrap_or(false),
                ));
            }
            Some(ClientPlayEvent::Play(PlayEvent::Disconnect(reason))) => return Err(reason),
            Some(event) => enqueue(&mut pending, WorkerMessage::Play(event))?,
            None => {}
        }
        for event in client.sound_events.drain(..) {
            enqueue(&mut pending, WorkerMessage::Sound(event))?;
        }
        for event in client.stop_sound_events.drain(..) {
            enqueue(&mut pending, WorkerMessage::StopSound(event))?;
        }
    }
}

/// Reserved `subworlds` id for the layered voxel-cloud sheet (kept clear of
/// contraption ids, which allocate from 1 upward).
pub const CLOUD_SUBWORLD_ID: u64 = u64::MAX;
/// Altocumulus deck of the hybrid cloud set — same reserved-id scheme as
/// [`CLOUD_SUBWORLD_ID`], one slot below.
pub const CLOUD_SUBWORLD_ID_ALTO: u64 = u64::MAX - 1;
/// Cirrus deck of the hybrid cloud set.
pub const CLOUD_SUBWORLD_ID_CIRRUS: u64 = u64::MAX - 2;
/// All three cloud deck ids in deck order: cumulus, altocumulus, cirrus.
/// `Vanilla`/`Fast` cloud modes keep only index 0 resident.
pub const CLOUD_DECK_IDS: [u64; 3] = [
    CLOUD_SUBWORLD_ID,
    CLOUD_SUBWORLD_ID_ALTO,
    CLOUD_SUBWORLD_ID_CIRRUS,
];

/// `true` for every reserved cloud deck id — shadow-caster filters and
/// dimension cleanup key on this so all three decks are managed as one
/// family.
pub fn is_cloud_subworld_id(id: u64) -> bool {
    CLOUD_DECK_IDS.contains(&id)
}

/// How `refresh_voxel_clouds` manages the cloud decks — derived by
/// `window.rs` from `CloudMode` x `VisualStylePreset`, never a user
/// setting of its own.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CloudDeckMode {
    /// `Clouds: Off` — no cloud subworlds at all.
    Off,
    /// One deck sampled from the vanilla cloud texture, or a greedy-meshed
    /// cubic deck when the texture is unavailable.
    #[default]
    Vanilla,
    /// Three greedy-meshed decks (cumulus/altocumulus/cirrus) with
    /// independent parallax wind, gentle sine bob/sway and
    /// `MAT_FLAG_CLOUD` cubic x realistic hybrid optics.
    Hybrid,
}

/// Per-deck hybrid-cloud tuning returned by [`cloud_deck_spec`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CloudDeckSpec {
    /// Reserved `subworlds` id for this deck.
    pub id: u64,
    /// Draw alpha: cumulus 0.82 > altocumulus 0.58 > cirrus 0.35.
    pub alpha: f32,
    /// Parallax wind-speed multiplier vs the base cloud scroll.
    pub wind_multiplier: f32,
    /// Extra height on top of the baked mesh base (blocks).
    pub y_offset: f32,
    /// Vertical bob amplitude (blocks).
    pub bob_amplitude: f32,
    /// Bob angular frequency (rad per world-second; 14-22s periods).
    pub bob_freq: f32,
    /// Cross-wind sway amplitude (blocks).
    pub sway_amplitude: f32,
    /// Sway angular frequency.
    pub sway_freq: f32,
    /// Phase offset so the decks never bob in lockstep.
    pub phase: f32,
}

/// Tuning for one hybrid cloud deck by index into [`CLOUD_DECK_IDS`].
/// Deck 0 is the backwards-compatible cumulus sheet every mode shares;
/// decks 1-2 only exist under [`CloudDeckMode::Hybrid`].
pub fn cloud_deck_spec(deck: usize) -> CloudDeckSpec {
    const TAU: f32 = std::f32::consts::TAU;
    match deck {
        1 => CloudDeckSpec {
            id: CLOUD_SUBWORLD_ID_ALTO,
            alpha: 0.58,
            wind_multiplier: 1.28,
            y_offset: 14.0,
            bob_amplitude: 0.42,
            bob_freq: TAU / 16.0,
            sway_amplitude: 0.35,
            sway_freq: TAU / 23.0,
            phase: 2.1,
        },
        2 => CloudDeckSpec {
            id: CLOUD_SUBWORLD_ID_CIRRUS,
            alpha: 0.35,
            wind_multiplier: 1.62,
            y_offset: 26.0,
            bob_amplitude: 0.55,
            bob_freq: TAU / 14.0,
            sway_amplitude: 0.35,
            sway_freq: TAU / 19.0,
            phase: 4.2,
        },
        _ => CloudDeckSpec {
            id: CLOUD_SUBWORLD_ID,
            alpha: 0.82,
            wind_multiplier: 1.0,
            y_offset: 0.0,
            bob_amplitude: 0.28,
            bob_freq: TAU / 18.0,
            sway_amplitude: 0.35,
            sway_freq: TAU / 26.0,
            phase: 0.0,
        },
    }
}

/// Per-deck cloud mesh cache slot: the integer-cell key the deck's mesh
/// was last generated for plus the quantized (1/32) cloud-lighting key
/// (weather or day-phase shifts that change the baked tint force a
/// regenerate even inside one cell).
#[derive(Clone, Copy, Default)]
struct CloudDeckCache {
    mesh_key: Option<(i64, i64, i64, u32, u32)>,
    lighting_key: Option<[u32; 10]>,
}

/// SW6D v1 pose-flag bit marking the sub-world the local player rides and
/// helms — the binding [`LiveSession::send_subworld_helm`] commands target.
const SUBWORLD_POSE_PLAYER_ABOARD: u8 = 0x01;

/// Quantize one [`CloudLighting`] state to 1/32 steps for the cloud-mesh
/// cache key: the tint is baked into vertex albedo, so a real change must
/// regenerate the mesh while sub-step drift keeps the cached frame.
fn cloud_lighting_key(lighting: &CloudLighting) -> [u32; 10] {
    let q = |v: f32| {
        if v.is_finite() {
            (v * 32.0).round() as i64 as u32
        } else {
            0
        }
    };
    [
        q(lighting.top_rgb[0]),
        q(lighting.top_rgb[1]),
        q(lighting.top_rgb[2]),
        q(lighting.side_rgb[0]),
        q(lighting.side_rgb[1]),
        q(lighting.side_rgb[2]),
        q(lighting.bottom_rgb[0]),
        q(lighting.bottom_rgb[1]),
        q(lighting.bottom_rgb[2]),
        q(lighting.rim_warmth),
    ]
}

/// Candidate version-directory names under `<gameDir>/versions/` for the active
/// session. Never falls back across different Minecraft versions: an explicit
/// `26.3` or `megu-26.3` session only probes `26.3` directories (preventing
/// cross-version asset/recipe mixing); only an unspecified `version_id` defaults
/// to `"1.21.1"`.
pub(crate) fn jar_version_ids(paths: &SessionPaths) -> Vec<String> {
    let mut ids = Vec::with_capacity(2);
    if let Some(id) = paths
        .version_id
        .as_deref()
        .map(str::trim)
        .filter(|id| !id.is_empty() && !id.contains(['/', '\\', ':']))
    {
        ids.push(id.to_owned());
        if let Some(base) = id.strip_prefix("megu-").filter(|b| !b.is_empty()) {
            if ids.iter().all(|x| x != base) {
                ids.push(base.to_owned());
            }
        }
    } else {
        ids.push("1.21.1".to_owned());
    }
    ids
}

fn configured_resource_pack_entries(game_dir: &Path) -> Vec<String> {
    let Ok(text) = std::fs::read_to_string(game_dir.join("options.txt")) else {
        return Vec::new();
    };
    let Some(value) = text
        .lines()
        .find_map(|line| line.trim().strip_prefix("resourcePacks:"))
    else {
        return Vec::new();
    };
    let Ok(parsed) = megu_core::json::parse_json(value) else {
        return Vec::new();
    };
    let megu_core::json::JsonValue::Array(entries) = parsed else {
        return Vec::new();
    };
    entries
        .iter()
        .filter_map(|entry| entry.as_str())
        .map(|entry| entry.strip_prefix("file/").unwrap_or(entry).to_owned())
        .collect()
}

/// Resolve the full texture layer stack for a session: the vanilla client
/// JAR (`<gameDir>/versions/<id>/<id>.jar`) as the base layer, then sorted
/// `mods/*.jar` assets, then every
/// pack `options.txt` enables inside `resourcepacks/` (dir or `.zip`), then
/// the explicit `MEGU_RESOURCE_PACK_DIR` override on top. Returns the
/// layers in lowest-to-highest priority order, an [`AssetDiscovery`] report
/// of every source resolved or skipped, and the on-disk pack roots CEM
/// model scanning can still walk (zip packs contribute no CEM dir).
pub fn discover_asset_layers(
    paths: &SessionPaths,
) -> (Vec<ResourcePackLayer>, AssetDiscovery, Vec<PathBuf>) {
    let mut layers = Vec::new();
    let mut report = AssetDiscovery::default();
    let mut cem_dirs = Vec::new();

    // Local-first vanilla JAR discovery runs with or without `--gameDir`:
    // `<gameDir>/versions/<id>/<id>.jar` first, then every launcher cache
    // (Official/Flatpak `.minecraft`, Prism libraries, Modrinth meta) and
    // Megu's own cache — a user who installed the same Minecraft version
    // anywhere gets vanilla textures with zero manual steps.
    for jar in crate::vanilla_bootstrap::local_vanilla_jar_candidates(paths) {
        if !crate::vanilla_bootstrap::is_valid_jar_candidate(&jar) {
            continue;
        }
        match crate::vanilla_jar::vanilla_jar_texture_layer(&jar) {
            Ok((layer, jar_report)) => {
                report.jar_textures = layer.sprites.len();
                report.jar_path = Some(jar.clone());
                report.zip_entries_seen += jar_report.entries_seen;
                for (entry, reason) in jar_report.skipped {
                    report
                        .notes
                        .push(format!("{}: {entry}: {reason}", jar.display()));
                }
                layers.push(layer);
                break;
            }
            Err(err) => {
                report
                    .notes
                    .push(format!("client JAR {} unreadable: {err}", jar.display()));
            }
        }
    }

    if let Some(game_dir) = paths.game_dir.as_deref() {
        let (mod_layers, mod_report) =
            crate::vanilla_jar::load_mod_jar_layers(&game_dir.join("mods"));
        report.zip_entries_seen += mod_report.entries_seen;
        for path in mod_report.loaded {
            report.pack_sources.push(path.display().to_string());
        }
        for (path, err) in mod_report.rejected {
            report
                .notes
                .push(format!("mod JAR {} skipped: {err}", path.display()));
        }
        layers.extend(mod_layers);
    }

    if let Some(server_mods_dir) = paths.server_mods_dir.as_deref() {
        let (mod_layers, mod_report) = crate::vanilla_jar::load_mod_jar_layers(server_mods_dir);
        report.zip_entries_seen += mod_report.entries_seen;
        for path in mod_report.loaded {
            report.pack_sources.push(path.display().to_string());
        }
        for (path, err) in mod_report.rejected {
            report
                .notes
                .push(format!("server mod JAR {} skipped: {err}", path.display()));
        }
        layers.extend(mod_layers);
    }

    // `options.txt`-configured packs, vanilla order: the file lists them
    // bottom-to-top, so each later layer overrides the earlier ones.
    if let Some(game_dir) = paths.game_dir.as_deref() {
        let pack_root = paths
            .resource_pack_dir
            .clone()
            .unwrap_or_else(|| game_dir.join("resourcepacks"));
        let mut official_builtin_paths = Vec::new();
        if let Some(jar) = crate::vanilla_bootstrap::local_vanilla_jar_candidates(paths)
            .into_iter()
            .find(|jar| crate::vanilla_bootstrap::is_valid_jar_candidate(jar))
        {
            let index_id = paths.asset_index.as_deref().unwrap_or("17");
            let assets_dir = paths
                .assets_dir
                .clone()
                .or_else(|| crate::vanilla_bootstrap::discover_local_assets_dir(index_id, paths));
            let extras = crate::vanilla_official_extras::VanillaOfficialExtras::load(
                Some(jar.as_path()),
                assets_dir.as_deref(),
                index_id,
            );
            official_builtin_paths =
                crate::item_visuals_3d::discover_official_builtin_packs(&extras)
                    .into_iter()
                    .map(|(id, _, path)| (id, path))
                    .collect();
        }
        for file in configured_resource_pack_entries(game_dir) {
            // options.txt names are pack-root-relative or official built-in
            // ids; refuse anything trying to walk out of the pack root.
            if file.contains("..") || file.starts_with('/') || file.contains('\\') {
                report
                    .notes
                    .push(format!("options.txt pack {file:?} rejected: unsafe path"));
                continue;
            }
            let bare_builtin = file
                .strip_prefix("minecraft:")
                .unwrap_or(&file)
                .strip_suffix(".zip")
                .unwrap_or(file.strip_prefix("minecraft:").unwrap_or(&file));
            let builtin_path = official_builtin_paths.iter().find_map(|(id, path)| {
                (id.strip_prefix("minecraft:").unwrap_or(id) == bare_builtin).then_some(path)
            });
            let candidate = builtin_path
                .map(|path| (*path).clone())
                .unwrap_or_else(|| pack_root.join(&file));
            let layer_result = if candidate.is_dir() {
                ResourcePackLayer::from_dir(&candidate).map_err(|e| e.to_string())
            } else if candidate.is_file() {
                crate::vanilla_jar::zip_pack_layer(&candidate)
                    .map(|(layer, zip_report)| {
                        report.zip_entries_seen += zip_report.entries_seen;
                        for (entry, reason) in zip_report.skipped {
                            report
                                .notes
                                .push(format!("{}: {entry}: {reason}", candidate.display()));
                        }
                        layer
                    })
                    .map_err(|e| e.to_string())
            } else {
                report.notes.push(format!(
                    "options.txt pack {file:?} missing from resourcepacks/"
                ));
                continue;
            };
            match layer_result {
                Ok(layer) => {
                    report.pack_sources.push(candidate.display().to_string());
                    if candidate.is_dir() {
                        cem_dirs.push(candidate);
                    }
                    layers.push(layer);
                }
                Err(err) => report.notes.push(format!(
                    "resource pack {} failed: {err}",
                    candidate.display()
                )),
            }
        }
    }

    // Explicit override stays last and wins every earlier layer, matching
    // the historical env-var behaviour.
    if let Some(dir) = std::env::var_os("MEGU_RESOURCE_PACK_DIR") {
        let dir = PathBuf::from(dir);
        match ResourcePackLayer::from_dir(&dir) {
            Ok(layer) => {
                report.pack_sources.push(dir.display().to_string());
                cem_dirs.push(dir);
                layers.push(layer);
            }
            Err(err) => report.notes.push(format!(
                "MEGU_RESOURCE_PACK_DIR {} failed: {err}",
                dir.display()
            )),
        }
    }

    (layers, report, cem_dirs)
}

/// Decode an anonymous-NBT text component (title/subtitle/action-bar payloads)
/// into plain text via `NbtValue::extract_text`.
fn nbt_text(nbt_bytes: &[u8]) -> Option<String> {
    let text = WireReader::new(nbt_bytes).read_nbt().ok()?.extract_text();
    (!text.is_empty()).then_some(text)
}

/// Only ChatFormatting IDs 0–15 tint names; styles and RESET do not.
fn vanilla_team_color(id: u32) -> Option<[u8; 3]> {
    const COLORS: [u32; 16] = [
        0x000000, 0x0000aa, 0x00aa00, 0x00aaaa, 0xaa0000, 0xaa00aa, 0xffaa00, 0xaaaaaa, 0x555555,
        0x5555ff, 0x55ff55, 0x55ffff, 0xff5555, 0xff55ff, 0xffff55, 0xffffff,
    ];
    let rgb = *COLORS.get(id as usize)?;
    Some([(rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8])
}

fn scoreboard_uuid(uuid: [u8; 16]) -> String {
    let [a, b, c, d, e, f, g, h, i, j, k, l, m, n, o, p] = uuid;
    format!(
        "{a:02x}{b:02x}{c:02x}{d:02x}-{e:02x}{f:02x}-{g:02x}{h:02x}-{i:02x}{j:02x}-{k:02x}{l:02x}{m:02x}{n:02x}{o:02x}{p:02x}"
    )
}

/// Short stable label for a player-info entry that arrived without a
/// profile name (vanilla shows nothing, but an unlabelled row is worse).
fn uuid_label(uuid: &[u8; 16]) -> String {
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}",
        uuid[0], uuid[1], uuid[2], uuid[3], uuid[4], uuid[5], uuid[6], uuid[7],
    )
}

/// Pull the boss-bar fields the HUD needs out of a `boss_event`
/// action payload (add = title component + f32 health + color +
/// division + flags; update-health = f32; update-name = component).
fn decode_boss_event_payload(bar: &mut LiveBossBar, payload: &[u8]) {
    let Ok(decoded) = decode_boss_bar_action(bar.action, payload) else {
        return;
    };
    match decoded {
        DecodedBossBarAction::Add {
            name_nbt,
            progress,
            color,
            divisions,
            flags,
        } => {
            if let Some(title) = nbt_text(&name_nbt) {
                bar.title = title;
            }
            bar.health = progress.clamp(0.0, 1.0);
            bar.color = color;
            bar.divisions = divisions;
            bar.flags = flags;
        }
        DecodedBossBarAction::Remove => {}
        DecodedBossBarAction::UpdateProgress(progress) => {
            bar.health = progress.clamp(0.0, 1.0);
        }
        DecodedBossBarAction::UpdateName(name_nbt) => {
            if let Some(title) = nbt_text(&name_nbt) {
                bar.title = title;
            }
        }
        DecodedBossBarAction::UpdateStyle { color, divisions } => {
            bar.color = color;
            bar.divisions = divisions;
        }
        DecodedBossBarAction::UpdateFlags(flags) => {
            bar.flags = flags;
        }
    }
}

/// Vanilla `ArmorMaterials` defense points for one equipped armor item
/// (helmet/chest/leg/boot rows 5..9 of the window-0 container; HUD
/// renders them as half-icons).
fn armor_points_for(item_name: Option<&str>) -> u32 {
    let Some(name) = item_name else { return 0 };
    let (material, slot) = if let Some(m) = name.strip_suffix("_helmet") {
        (m, 0usize)
    } else if let Some(m) = name.strip_suffix("_chestplate") {
        (m, 1)
    } else if let Some(m) = name.strip_suffix("_leggings") {
        (m, 2)
    } else if let Some(m) = name.strip_suffix("_boots") {
        (m, 3)
    } else {
        return u32::from(name.ends_with("turtle_shell")) * 2;
    };
    let material = material.rsplit("minecraft:").next().unwrap_or(material);
    // (helmet, chestplate, leggings, boots) — vanilla ArmorMaterials.
    let row: [u32; 4] = match material {
        "leather" => [1, 3, 2, 1],
        "golden" => [2, 5, 3, 1],
        "chainmail" => [2, 5, 4, 1],
        "iron" => [2, 6, 5, 2],
        "diamond" | "netherite" => [3, 8, 6, 3],
        _ => return 0,
    };
    row[slot]
}

/// Seed the session command dispatcher with the vanilla command-name set
/// (`zsh_command_bar`'s `COMMAND_SPECS`) so `complete_suggestions` answers the literals the bar
/// highlights before any `declare_commands`.
fn seeded_command_dispatcher() -> CommandDispatcher {
    let mut dispatcher = CommandDispatcher::new();
    for name in megu_client::zsh_command_bar::vanilla_command_names() {
        let _ = dispatcher.register_literal(name);
    }
    dispatcher
}

/// `sign_update` C2S packet id for the negotiated protocol: `0x35` on
/// vanilla 1.21.1 / protocol 767, `0x3e` on 26.3 / protocol 777
/// (`proto/src/data_v767.rs` / `data_v777.rs` `sign_update` rows).
fn sign_update_packet_id(version: megu_proto::ProtocolVersion) -> i32 {
    match version {
        megu_proto::ProtocolVersion::V26_3 => 0x3e,
        _ => 0x35,
    }
}

/// `ServerboundSignUpdatePacket` body: `Position` (packed `x:26|z:26|y:12`
/// big-endian `i64`), `is_front` bool, then the four lines as
/// VarInt-prefixed UTF-8 strings in row order.
pub(crate) fn encode_sign_update_payload(
    pos: (i32, i32, i32),
    is_front: bool,
    lines: &[String; 4],
) -> Vec<u8> {
    let mut payload = Vec::with_capacity(9 + 5 + 4 * 5);
    payload.extend_from_slice(&pack_position([pos.0, pos.1, pos.2]).to_be_bytes());
    payload.push(u8::from(is_front));
    for line in lines {
        write_string(&mut payload, line);
    }
    payload
}

/// Priority order for off-thread `SectionMesh` submissions — mirrors
/// `MeshCache::rebuild_dirty_ordered`'s queue key: in-frustum sections
/// first, then nearest section-box distance (bit pattern, `Ord`-safe).
fn offthread_queue_key(coord: SectionCoord, camera: [f32; 3], planes: &[[f32; 4]]) -> (bool, u64) {
    const EDGE: f32 = 16.0;
    let min = [
        coord.0 as f32 * EDGE,
        coord.1 as f32 * EDGE,
        coord.2 as f32 * EDGE,
    ];
    let mut dist2 = 0.0f32;
    for (axis, &p) in camera.iter().enumerate() {
        let lo = min[axis];
        let hi = lo + EDGE;
        let gap = if p < lo {
            lo - p
        } else if p > hi {
            p - hi
        } else {
            0.0
        };
        dist2 += gap * gap;
    }
    (
        !box_in_frustum(min, planes),
        dist2.max(0.0).to_bits() as u64,
    )
}

/// The off-thread `MeshJob::SectionMesh` submission cap the
/// [`DiscountedUcbAutotuner`] drain-profile `arm` applies for a poll —
/// the production consumption of `select_arm`: arm 0 is the
/// conservative profile (quarter-rate background drain, lightest
/// main-thread burst), arm 1 the balanced default
/// ([`OFFTHREAD_SUBMIT_MAX`]), arms 2 and 3 the aggressive and burst
/// profiles. Bounded at `4×` the steady cap so a runaway bandit can
/// never flood the worker queue ahead of the inline lane.
const fn ducb_submit_cap(arm: usize) -> usize {
    match arm {
        0 => OFFTHREAD_SUBMIT_MAX / 4,
        1 => OFFTHREAD_SUBMIT_MAX,
        2 => OFFTHREAD_SUBMIT_MAX * 2,
        _ => OFFTHREAD_SUBMIT_MAX * 4,
    }
}

/// Selection-only fallback shapes for non-colliding small blocks. Ordinary
/// solid/shaped blocks use the registry table's geometry; these cover the
/// plant/torch cases whose collision boxes are intentionally empty.
fn selection_fallback_boxes(block_path: &str) -> Vec<Aabb> {
    let path = block_state_base_name(block_path)
        .rsplit(':')
        .next()
        .unwrap_or(block_path);
    let one = |min: [f64; 3], max: [f64; 3]| Aabb { min, max };
    if path.contains("torch") {
        return vec![one(
            [7.0 / 16.0, 0.0, 7.0 / 16.0],
            [9.0 / 16.0, 10.0 / 16.0, 9.0 / 16.0],
        )];
    }
    if path.contains("flower")
        || path.contains("sapling")
        || path.contains("mushroom")
        || path.contains("fungus")
        || path.contains("roots")
        || path.contains("short_grass")
        || path == "fern"
    {
        return vec![one(
            [5.0 / 16.0, 0.0, 5.0 / 16.0],
            [11.0 / 16.0, 10.0 / 16.0, 11.0 / 16.0],
        )];
    }
    Vec::new()
}

/// Pinned Minecraft 1.21.1 block mining metadata generated from
/// `assets/blocks_1211.json`: `name|hardness|material|harvest-item-bitmask|diggable`.
/// Bits 0..29 are vanilla tool item ids 818..847; bit 40 is shears (983).
const VANILLA_BLOCK_MINING_ROWS: &str = r#"acacia_button|0.5|mineable/axe|0|1
acacia_door|3|mineable/axe|0|1
acacia_fence|2|mineable/axe|0|1
acacia_fence_gate|2|mineable/axe|0|1
acacia_hanging_sign|1|mineable/axe|0|1
acacia_leaves|0.2|leaves;mineable/hoe|0|1
acacia_log|2|mineable/axe|0|1
acacia_planks|2|mineable/axe|0|1
acacia_pressure_plate|0.5|mineable/axe|0|1
acacia_sapling|0|plant;mineable/axe|0|1
acacia_sign|1|mineable/axe|0|1
acacia_slab|2|mineable/axe|0|1
acacia_stairs|2|mineable/axe|0|1
acacia_trapdoor|3|mineable/axe|0|1
acacia_wall_hanging_sign|1|mineable/axe|0|1
acacia_wall_sign|1|mineable/axe|0|1
acacia_wood|2|mineable/axe|0|1
activator_rail|0.7|mineable/pickaxe|0|1
air|0|default|0|0
allium|0|plant|0|1
amethyst_block|1.5|mineable/pickaxe|8421084|1
amethyst_cluster|1.5|mineable/pickaxe|0|1
ancient_debris|30|incorrect_for_wooden_tool|8400000|1
andesite|1.5|mineable/pickaxe|8421084|1
andesite_slab|1.5|mineable/pickaxe|8421084|1
andesite_stairs|1.5|mineable/pickaxe|8421084|1
andesite_wall|1.5|mineable/pickaxe|8421084|1
anvil|5|mineable/pickaxe|8421084|1
attached_melon_stem|0|plant;mineable/axe|0|1
attached_pumpkin_stem|0|plant;mineable/axe|0|1
azalea|0|plant;mineable/axe|0|1
azalea_leaves|0.2|leaves;mineable/hoe|0|1
azure_bluet|0|plant|0|1
bamboo|1|mineable/axe|0|1
bamboo_block|2|mineable/axe|0|1
bamboo_button|0.5|mineable/axe|0|1
bamboo_door|3|mineable/axe|0|1
bamboo_fence|2|mineable/axe|0|1
bamboo_fence_gate|2|mineable/axe|0|1
bamboo_hanging_sign|1|mineable/axe|0|1
bamboo_mosaic|2|mineable/axe|0|1
bamboo_mosaic_slab|2|mineable/axe|0|1
bamboo_mosaic_stairs|2|mineable/axe|0|1
bamboo_planks|2|mineable/axe|0|1
bamboo_pressure_plate|0.5|mineable/axe|0|1
bamboo_sapling|1|default|0|1
bamboo_sign|1|mineable/axe|0|1
bamboo_slab|2|mineable/axe|0|1
bamboo_stairs|2|mineable/axe|0|1
bamboo_trapdoor|3|mineable/axe|0|1
bamboo_wall_hanging_sign|1|mineable/axe|0|1
bamboo_wall_sign|1|mineable/axe|0|1
barrel|2.5|mineable/axe|0|1
barrier|-1|default|0|0
basalt|1.25|mineable/pickaxe|8421084|1
beacon|3|default|0|1
bedrock|-1|default|0|0
bee_nest|0.3|mineable/axe|0|1
beehive|0.6|mineable/axe|0|1
beetroots|0|plant;mineable/axe|0|1
bell|5|mineable/pickaxe|8421084|1
big_dripleaf|0.1|plant;mineable/axe|0|1
big_dripleaf_stem|0.1|plant;mineable/axe|0|1
birch_button|0.5|mineable/axe|0|1
birch_door|3|mineable/axe|0|1
birch_fence|2|mineable/axe|0|1
birch_fence_gate|2|mineable/axe|0|1
birch_hanging_sign|1|mineable/axe|0|1
birch_leaves|0.2|leaves;mineable/hoe|0|1
birch_log|2|mineable/axe|0|1
birch_planks|2|mineable/axe|0|1
birch_pressure_plate|0.5|mineable/axe|0|1
birch_sapling|0|plant;mineable/axe|0|1
birch_sign|1|mineable/axe|0|1
birch_slab|2|mineable/axe|0|1
birch_stairs|2|mineable/axe|0|1
birch_trapdoor|3|mineable/axe|0|1
birch_wall_hanging_sign|1|mineable/axe|0|1
birch_wall_sign|1|mineable/axe|0|1
birch_wood|2|mineable/axe|0|1
black_banner|1|mineable/axe|0|1
black_bed|0.2|default|0|1
black_candle|0.1|default|0|1
black_candle_cake|0.5|default|0|1
black_carpet|0.1|default|0|1
black_concrete|1.8|mineable/pickaxe|8421084|1
black_concrete_powder|0.5|mineable/shovel|0|1
black_glazed_terracotta|1.4|mineable/pickaxe|8421084|1
black_shulker_box|2|mineable/pickaxe|0|1
black_stained_glass|0.3|default|0|1
black_stained_glass_pane|0.3|default|0|1
black_terracotta|1.25|mineable/pickaxe|8421084|1
black_wall_banner|1|mineable/axe|0|1
black_wool|0.8|wool|0|1
blackstone|1.5|mineable/pickaxe|8421084|1
blackstone_slab|2|mineable/pickaxe|8421084|1
blackstone_stairs|1.5|mineable/pickaxe|8421084|1
blackstone_wall|1.5|mineable/pickaxe|8421084|1
blast_furnace|3.5|mineable/pickaxe|8421084|1
blue_banner|1|mineable/axe|0|1
blue_bed|0.2|default|0|1
blue_candle|0.1|default|0|1
blue_candle_cake|0.5|default|0|1
blue_carpet|0.1|default|0|1
blue_concrete|1.8|mineable/pickaxe|8421084|1
blue_concrete_powder|0.5|mineable/shovel|0|1
blue_glazed_terracotta|1.4|mineable/pickaxe|8421084|1
blue_ice|2.8|mineable/pickaxe|0|1
blue_orchid|0|plant|0|1
blue_shulker_box|2|mineable/pickaxe|0|1
blue_stained_glass|0.3|default|0|1
blue_stained_glass_pane|0.3|default|0|1
blue_terracotta|1.25|mineable/pickaxe|8421084|1
blue_wall_banner|1|mineable/axe|0|1
blue_wool|0.8|wool|0|1
bone_block|2|mineable/pickaxe|8421084|1
bookshelf|1.5|mineable/axe|0|1
brain_coral|0|default|0|1
brain_coral_block|1.5|mineable/pickaxe|8421084|1
brain_coral_fan|0|default|0|1
brain_coral_wall_fan|0|default|0|1
brewing_stand|0.5|mineable/pickaxe|8421084|1
brick_slab|2|mineable/pickaxe|8421084|1
brick_stairs|2|mineable/pickaxe|8421084|1
brick_wall|2|mineable/pickaxe|8421084|1
bricks|2|mineable/pickaxe|8421084|1
brown_banner|1|mineable/axe|0|1
brown_bed|0.2|default|0|1
brown_candle|0.1|default|0|1
brown_candle_cake|0.5|default|0|1
brown_carpet|0.1|default|0|1
brown_concrete|1.8|mineable/pickaxe|8421084|1
brown_concrete_powder|0.5|mineable/shovel|0|1
brown_glazed_terracotta|1.4|mineable/pickaxe|8421084|1
brown_mushroom|0|plant;mineable/axe|0|1
brown_mushroom_block|0.2|mineable/axe|0|1
brown_shulker_box|2|mineable/pickaxe|0|1
brown_stained_glass|0.3|default|0|1
brown_stained_glass_pane|0.3|default|0|1
brown_terracotta|1.25|mineable/pickaxe|8421084|1
brown_wall_banner|1|mineable/axe|0|1
brown_wool|0.8|wool|0|1
bubble_column|0|default|0|1
bubble_coral|0|default|0|1
bubble_coral_block|1.5|mineable/pickaxe|8421084|1
bubble_coral_fan|0|default|0|1
bubble_coral_wall_fan|0|default|0|1
budding_amethyst|1.5|mineable/pickaxe|8421084|1
cactus|0.4|default|0|1
cake|0.5|default|0|1
calcite|0.75|mineable/pickaxe|8421084|1
calibrated_sculk_sensor|1.5|mineable/hoe|0|1
campfire|2|mineable/axe|0|1
candle|0.1|default|0|1
candle_cake|0.5|default|0|1
carrots|0|plant;mineable/axe|0|1
cartography_table|2.5|mineable/axe|0|1
carved_pumpkin|1|plant;mineable/axe|0|1
cauldron|2|mineable/pickaxe|8421084|1
cave_air|0|default|0|0
cave_vines|0|plant;mineable/axe|0|1
cave_vines_plant|0|plant;mineable/axe|0|1
chain|5|mineable/pickaxe|8421084|1
chain_command_block|-1|default|0|0
cherry_button|0.5|mineable/axe|0|1
cherry_door|3|mineable/axe|0|1
cherry_fence|2|mineable/axe|0|1
cherry_fence_gate|2|mineable/axe|0|1
cherry_hanging_sign|1|mineable/axe|0|1
cherry_leaves|0.2|leaves;mineable/hoe|0|1
cherry_log|2|mineable/axe|0|1
cherry_planks|2|mineable/axe|0|1
cherry_pressure_plate|0.5|mineable/axe|0|1
cherry_sapling|0|plant;mineable/axe|0|1
cherry_sign|1|mineable/axe|0|1
cherry_slab|2|mineable/axe|0|1
cherry_stairs|2|mineable/axe|0|1
cherry_trapdoor|3|mineable/axe|0|1
cherry_wall_hanging_sign|1|mineable/axe|0|1
cherry_wall_sign|1|mineable/axe|0|1
cherry_wood|2|mineable/axe|0|1
chest|2.5|mineable/axe|0|1
chipped_anvil|5|mineable/pickaxe|8421084|1
chiseled_bookshelf|1.5|mineable/axe|0|1
chiseled_copper|3|incorrect_for_wooden_tool|8420080|1
chiseled_deepslate|3.5|mineable/pickaxe|8421084|1
chiseled_nether_bricks|2|mineable/pickaxe|8421084|1
chiseled_polished_blackstone|1.5|mineable/pickaxe|8421084|1
chiseled_quartz_block|0.8|mineable/pickaxe|8421084|1
chiseled_red_sandstone|0.8|mineable/pickaxe|8421084|1
chiseled_sandstone|0.8|mineable/pickaxe|8421084|1
chiseled_stone_bricks|1.5|mineable/pickaxe|8421084|1
chiseled_tuff|1.5|mineable/pickaxe|8421084|1
chiseled_tuff_bricks|1.5|mineable/pickaxe|8421084|1
chorus_flower|0.4|plant;mineable/axe|0|1
chorus_plant|0.4|plant;mineable/axe|0|1
clay|0.6|mineable/shovel|0|1
coal_block|5|mineable/pickaxe|8421084|1
coal_ore|3|mineable/pickaxe|8421084|1
coarse_dirt|0.5|mineable/shovel|0|1
cobbled_deepslate|3.5|mineable/pickaxe|8421084|1
cobbled_deepslate_slab|3.5|mineable/pickaxe|8421084|1
cobbled_deepslate_stairs|3.5|mineable/pickaxe|8421084|1
cobbled_deepslate_wall|3.5|mineable/pickaxe|8421084|1
cobblestone|2|mineable/pickaxe|8421084|1
cobblestone_slab|2|mineable/pickaxe|8421084|1
cobblestone_stairs|2|mineable/pickaxe|8421084|1
cobblestone_wall|2|mineable/pickaxe|8421084|1
cobweb|4|coweb|10002108421|1
cocoa|0.2|plant;mineable/axe|0|1
command_block|-1|default|0|0
comparator|0|default|0|1
composter|0.6|mineable/axe|0|1
conduit|3|mineable/pickaxe|0|1
copper_block|3|incorrect_for_wooden_tool|8420080|1
copper_bulb|3|incorrect_for_wooden_tool|8420080|1
copper_door|3|incorrect_for_wooden_tool|8420080|1
copper_grate|3|incorrect_for_wooden_tool|8420080|1
copper_ore|3|incorrect_for_wooden_tool|8420080|1
copper_trapdoor|3|incorrect_for_wooden_tool|8420080|1
cornflower|0|plant|0|1
cracked_deepslate_bricks|3.5|mineable/pickaxe|8421084|1
cracked_deepslate_tiles|3.5|mineable/pickaxe|8421084|1
cracked_nether_bricks|2|mineable/pickaxe|8421084|1
cracked_polished_blackstone_bricks|1.5|mineable/pickaxe|8421084|1
cracked_stone_bricks|1.5|mineable/pickaxe|8421084|1
crafter|1.5|incorrect_for_wooden_tool|0|1
crafting_table|2.5|mineable/axe|0|1
creeper_head|1|default|0|1
creeper_wall_head|1|default|0|1
crimson_button|0.5|mineable/axe|0|1
crimson_door|3|mineable/axe|0|1
crimson_fence|2|mineable/axe|0|1
crimson_fence_gate|2|mineable/axe|0|1
crimson_fungus|0|plant;mineable/axe|0|1
crimson_hanging_sign|1|mineable/axe|0|1
crimson_hyphae|2|mineable/axe|0|1
crimson_nylium|0.4|mineable/pickaxe|8421084|1
crimson_planks|2|mineable/axe|0|1
crimson_pressure_plate|0.5|mineable/axe|0|1
crimson_roots|0|plant|0|1
crimson_sign|1|mineable/axe|0|1
crimson_slab|2|mineable/axe|0|1
crimson_stairs|2|mineable/axe|0|1
crimson_stem|2|mineable/axe|0|1
crimson_trapdoor|3|mineable/axe|0|1
crimson_wall_hanging_sign|1|mineable/axe|0|1
crimson_wall_sign|1|mineable/axe|0|1
crying_obsidian|50|incorrect_for_wooden_tool|8400000|1
cut_copper|3|incorrect_for_wooden_tool|8420080|1
cut_copper_slab|3|incorrect_for_wooden_tool|8420080|1
cut_copper_stairs|3|incorrect_for_wooden_tool|8420080|1
cut_red_sandstone|0.8|mineable/pickaxe|8421084|1
cut_red_sandstone_slab|2|mineable/pickaxe|8421084|1
cut_sandstone|0.8|mineable/pickaxe|8421084|1
cut_sandstone_slab|2|mineable/pickaxe|8421084|1
cyan_banner|1|mineable/axe|0|1
cyan_bed|0.2|default|0|1
cyan_candle|0.1|default|0|1
cyan_candle_cake|0.5|default|0|1
cyan_carpet|0.1|default|0|1
cyan_concrete|1.8|mineable/pickaxe|8421084|1
cyan_concrete_powder|0.5|mineable/shovel|0|1
cyan_glazed_terracotta|1.4|mineable/pickaxe|8421084|1
cyan_shulker_box|2|mineable/pickaxe|0|1
cyan_stained_glass|0.3|default|0|1
cyan_stained_glass_pane|0.3|default|0|1
cyan_terracotta|1.25|mineable/pickaxe|8421084|1
cyan_wall_banner|1|mineable/axe|0|1
cyan_wool|0.8|wool|0|1
damaged_anvil|5|mineable/pickaxe|8421084|1
dandelion|0|plant|0|1
dark_oak_button|0.5|mineable/axe|0|1
dark_oak_door|3|mineable/axe|0|1
dark_oak_fence|2|mineable/axe|0|1
dark_oak_fence_gate|2|mineable/axe|0|1
dark_oak_hanging_sign|1|mineable/axe|0|1
dark_oak_leaves|0.2|leaves;mineable/hoe|0|1
dark_oak_log|2|mineable/axe|0|1
dark_oak_planks|2|mineable/axe|0|1
dark_oak_pressure_plate|0.5|mineable/axe|0|1
dark_oak_sapling|0|plant;mineable/axe|0|1
dark_oak_sign|1|mineable/axe|0|1
dark_oak_slab|2|mineable/axe|0|1
dark_oak_stairs|2|mineable/axe|0|1
dark_oak_trapdoor|3|mineable/axe|0|1
dark_oak_wall_hanging_sign|1|mineable/axe|0|1
dark_oak_wall_sign|1|mineable/axe|0|1
dark_oak_wood|2|mineable/axe|0|1
dark_prismarine|1.5|mineable/pickaxe|8421084|1
dark_prismarine_slab|1.5|mineable/pickaxe|8421084|1
dark_prismarine_stairs|1.5|mineable/pickaxe|8421084|1
daylight_detector|0.2|mineable/axe|0|1
dead_brain_coral|0|mineable/pickaxe|8421084|1
dead_brain_coral_block|1.5|mineable/pickaxe|8421084|1
dead_brain_coral_fan|0|mineable/pickaxe|8421084|1
dead_brain_coral_wall_fan|0|mineable/pickaxe|8421084|1
dead_bubble_coral|0|mineable/pickaxe|8421084|1
dead_bubble_coral_block|1.5|mineable/pickaxe|8421084|1
dead_bubble_coral_fan|0|mineable/pickaxe|8421084|1
dead_bubble_coral_wall_fan|0|mineable/pickaxe|8421084|1
dead_bush|0|plant;mineable/axe|0|1
dead_fire_coral|0|mineable/pickaxe|8421084|1
dead_fire_coral_block|1.5|mineable/pickaxe|8421084|1
dead_fire_coral_fan|0|mineable/pickaxe|8421084|1
dead_fire_coral_wall_fan|0|mineable/pickaxe|8421084|1
dead_horn_coral|0|mineable/pickaxe|8421084|1
dead_horn_coral_block|1.5|mineable/pickaxe|8421084|1
dead_horn_coral_fan|0|mineable/pickaxe|8421084|1
dead_horn_coral_wall_fan|0|mineable/pickaxe|8421084|1
dead_tube_coral|0|mineable/pickaxe|8421084|1
dead_tube_coral_block|1.5|mineable/pickaxe|8421084|1
dead_tube_coral_fan|0|mineable/pickaxe|8421084|1
dead_tube_coral_wall_fan|0|mineable/pickaxe|8421084|1
decorated_pot|0|default|0|1
deepslate|3|mineable/pickaxe|8421084|1
deepslate_brick_slab|3.5|mineable/pickaxe|8421084|1
deepslate_brick_stairs|3.5|mineable/pickaxe|8421084|1
deepslate_brick_wall|3.5|mineable/pickaxe|8421084|1
deepslate_bricks|3.5|mineable/pickaxe|8421084|1
deepslate_coal_ore|4.5|mineable/pickaxe|8421084|1
deepslate_copper_ore|4.5|incorrect_for_wooden_tool|8420080|1
deepslate_diamond_ore|4.5|incorrect_for_wooden_tool|8420000|1
deepslate_emerald_ore|4.5|incorrect_for_wooden_tool|8420000|1
deepslate_gold_ore|4.5|incorrect_for_wooden_tool|8420000|1
deepslate_iron_ore|4.5|incorrect_for_wooden_tool|8420080|1
deepslate_lapis_ore|4.5|incorrect_for_wooden_tool|8420080|1
deepslate_redstone_ore|4.5|incorrect_for_wooden_tool|8420000|1
deepslate_tile_slab|3.5|mineable/pickaxe|8421084|1
deepslate_tile_stairs|3.5|mineable/pickaxe|8421084|1
deepslate_tile_wall|3.5|mineable/pickaxe|8421084|1
deepslate_tiles|3.5|mineable/pickaxe|8421084|1
detector_rail|0.7|mineable/pickaxe|0|1
diamond_block|5|incorrect_for_wooden_tool|8420000|1
diamond_ore|3|incorrect_for_wooden_tool|8420000|1
diorite|1.5|mineable/pickaxe|8421084|1
diorite_slab|1.5|mineable/pickaxe|8421084|1
diorite_stairs|1.5|mineable/pickaxe|8421084|1
diorite_wall|1.5|mineable/pickaxe|8421084|1
dirt|0.5|mineable/shovel|0|1
dirt_path|0.65|mineable/shovel|0|1
dispenser|3.5|mineable/pickaxe|8421084|1
dragon_egg|3|default|0|1
dragon_head|1|default|0|1
dragon_wall_head|1|default|0|1
dried_kelp_block|0.5|mineable/hoe|0|1
dripstone_block|1.5|mineable/pickaxe|8421084|1
dropper|3.5|mineable/pickaxe|8421084|1
emerald_block|5|incorrect_for_wooden_tool|8420000|1
emerald_ore|3|incorrect_for_wooden_tool|8420000|1
enchanting_table|5|mineable/pickaxe|8421084|1
end_gateway|-1|default|0|0
end_portal|-1|default|0|0
end_portal_frame|-1|default|0|0
end_rod|0|default|0|1
end_stone|3|mineable/pickaxe|8421084|1
end_stone_brick_slab|3|mineable/pickaxe|8421084|1
end_stone_brick_stairs|3|mineable/pickaxe|8421084|1
end_stone_brick_wall|3|mineable/pickaxe|8421084|1
end_stone_bricks|3|mineable/pickaxe|8421084|1
ender_chest|22.5|mineable/pickaxe|8421084|1
exposed_chiseled_copper|3|incorrect_for_wooden_tool|8420080|1
exposed_copper|3|incorrect_for_wooden_tool|8420080|1
exposed_copper_bulb|3|incorrect_for_wooden_tool|8420080|1
exposed_copper_door|3|incorrect_for_wooden_tool|8420080|1
exposed_copper_grate|3|incorrect_for_wooden_tool|8420080|1
exposed_copper_trapdoor|3|incorrect_for_wooden_tool|8420080|1
exposed_cut_copper|3|incorrect_for_wooden_tool|8420080|1
exposed_cut_copper_slab|3|incorrect_for_wooden_tool|8420080|1
exposed_cut_copper_stairs|3|incorrect_for_wooden_tool|8420080|1
farmland|0.6|mineable/shovel|0|1
fern|0|plant;mineable/axe|0|1
fire|0|default|0|1
fire_coral|0|default|0|1
fire_coral_block|1.5|mineable/pickaxe|8421084|1
fire_coral_fan|0|default|0|1
fire_coral_wall_fan|0|default|0|1
fletching_table|2.5|mineable/axe|0|1
flower_pot|0|default|0|1
flowering_azalea|0|plant;mineable/axe|0|1
flowering_azalea_leaves|0.2|leaves;mineable/hoe|0|1
frogspawn|0|default|0|1
frosted_ice|0.5|default|0|1
furnace|3.5|mineable/pickaxe|8421084|1
gilded_blackstone|1.5|mineable/pickaxe|8421084|1
glass|0.3|default|0|1
glass_pane|0.3|default|0|1
glow_lichen|0.2|vine_or_glow_lichen;plant;mineable/axe|0|1
glowstone|0.3|default|0|1
gold_block|3|incorrect_for_wooden_tool|8420000|1
gold_ore|3|incorrect_for_wooden_tool|8420000|1
granite|1.5|mineable/pickaxe|8421084|1
granite_slab|1.5|mineable/pickaxe|8421084|1
granite_stairs|1.5|mineable/pickaxe|8421084|1
granite_wall|1.5|mineable/pickaxe|8421084|1
grass_block|0.6|mineable/shovel|0|1
gravel|0.6|mineable/shovel|0|1
gray_banner|1|mineable/axe|0|1
gray_bed|0.2|default|0|1
gray_candle|0.1|default|0|1
gray_candle_cake|0.5|default|0|1
gray_carpet|0.1|default|0|1
gray_concrete|1.8|mineable/pickaxe|8421084|1
gray_concrete_powder|0.5|mineable/shovel|0|1
gray_glazed_terracotta|1.4|mineable/pickaxe|8421084|1
gray_shulker_box|2|mineable/pickaxe|0|1
gray_stained_glass|0.3|default|0|1
gray_stained_glass_pane|0.3|default|0|1
gray_terracotta|1.25|mineable/pickaxe|8421084|1
gray_wall_banner|1|mineable/axe|0|1
gray_wool|0.8|wool|0|1
green_banner|1|mineable/axe|0|1
green_bed|0.2|default|0|1
green_candle|0.1|default|0|1
green_candle_cake|0.5|default|0|1
green_carpet|0.1|default|0|1
green_concrete|1.8|mineable/pickaxe|8421084|1
green_concrete_powder|0.5|mineable/shovel|0|1
green_glazed_terracotta|1.4|mineable/pickaxe|8421084|1
green_shulker_box|2|mineable/pickaxe|0|1
green_stained_glass|0.3|default|0|1
green_stained_glass_pane|0.3|default|0|1
green_terracotta|1.25|mineable/pickaxe|8421084|1
green_wall_banner|1|mineable/axe|0|1
green_wool|0.8|wool|0|1
grindstone|2|mineable/pickaxe|8421084|1
hanging_roots|0|plant;mineable/axe|0|1
hay_block|0.5|mineable/hoe|0|1
heavy_core|10|mineable/pickaxe|0|1
heavy_weighted_pressure_plate|0.5|mineable/pickaxe|8421084|1
honey_block|0|default|0|1
honeycomb_block|0.6|default|0|1
hopper|3|mineable/pickaxe|8421084|1
horn_coral|0|default|0|1
horn_coral_block|1.5|mineable/pickaxe|8421084|1
horn_coral_fan|0|default|0|1
horn_coral_wall_fan|0|default|0|1
ice|0.5|mineable/pickaxe|0|1
infested_chiseled_stone_bricks|0.75|mineable/pickaxe|0|1
infested_cobblestone|1|mineable/pickaxe|0|1
infested_cracked_stone_bricks|0.75|mineable/pickaxe|0|1
infested_deepslate|1.5|mineable/pickaxe|0|1
infested_mossy_stone_bricks|0.75|mineable/pickaxe|0|1
infested_stone|0.75|mineable/pickaxe|0|1
infested_stone_bricks|0.75|mineable/pickaxe|0|1
iron_bars|5|mineable/pickaxe|8421084|1
iron_block|5|incorrect_for_wooden_tool|8420080|1
iron_door|5|mineable/pickaxe|8421084|1
iron_ore|3|incorrect_for_wooden_tool|8420080|1
iron_trapdoor|5|mineable/pickaxe|8421084|1
jack_o_lantern|1|gourd;mineable/axe|0|1
jigsaw|-1|default|0|0
jukebox|2|mineable/axe|0|1
jungle_button|0.5|mineable/axe|0|1
jungle_door|3|mineable/axe|0|1
jungle_fence|2|mineable/axe|0|1
jungle_fence_gate|2|mineable/axe|0|1
jungle_hanging_sign|1|mineable/axe|0|1
jungle_leaves|0.2|leaves;mineable/hoe|0|1
jungle_log|2|mineable/axe|0|1
jungle_planks|2|mineable/axe|0|1
jungle_pressure_plate|0.5|mineable/axe|0|1
jungle_sapling|0|plant;mineable/axe|0|1
jungle_sign|1|mineable/axe|0|1
jungle_slab|2|mineable/axe|0|1
jungle_stairs|2|mineable/axe|0|1
jungle_trapdoor|3|mineable/axe|0|1
jungle_wall_hanging_sign|1|mineable/axe|0|1
jungle_wall_sign|1|mineable/axe|0|1
jungle_wood|2|mineable/axe|0|1
kelp|0|default|0|1
kelp_plant|0|default|0|1
ladder|0.4|mineable/axe|0|1
lantern|3.5|mineable/pickaxe|8421084|1
lapis_block|3|incorrect_for_wooden_tool|8420080|1
lapis_ore|3|incorrect_for_wooden_tool|8420080|1
large_amethyst_bud|1.5|mineable/pickaxe|0|1
large_fern|0|plant;mineable/axe|0|1
lava|100|default|0|1
lava_cauldron|2|mineable/pickaxe|8421084|1
lectern|2.5|mineable/axe|0|1
lever|0.5|default|0|1
light|-1|default|0|0
light_blue_banner|1|mineable/axe|0|1
light_blue_bed|0.2|default|0|1
light_blue_candle|0.1|default|0|1
light_blue_candle_cake|0.5|default|0|1
light_blue_carpet|0.1|default|0|1
light_blue_concrete|1.8|mineable/pickaxe|8421084|1
light_blue_concrete_powder|0.5|mineable/shovel|0|1
light_blue_glazed_terracotta|1.4|mineable/pickaxe|8421084|1
light_blue_shulker_box|2|mineable/pickaxe|0|1
light_blue_stained_glass|0.3|default|0|1
light_blue_stained_glass_pane|0.3|default|0|1
light_blue_terracotta|1.25|mineable/pickaxe|8421084|1
light_blue_wall_banner|1|mineable/axe|0|1
light_blue_wool|0.8|wool|0|1
light_gray_banner|1|mineable/axe|0|1
light_gray_bed|0.2|default|0|1
light_gray_candle|0.1|default|0|1
light_gray_candle_cake|0.5|default|0|1
light_gray_carpet|0.1|default|0|1
light_gray_concrete|1.8|mineable/pickaxe|8421084|1
light_gray_concrete_powder|0.5|mineable/shovel|0|1
light_gray_glazed_terracotta|1.4|mineable/pickaxe|8421084|1
light_gray_shulker_box|2|mineable/pickaxe|0|1
light_gray_stained_glass|0.3|default|0|1
light_gray_stained_glass_pane|0.3|default|0|1
light_gray_terracotta|1.25|mineable/pickaxe|8421084|1
light_gray_wall_banner|1|mineable/axe|0|1
light_gray_wool|0.8|wool|0|1
light_weighted_pressure_plate|0.5|mineable/pickaxe|8421084|1
lightning_rod|3|incorrect_for_wooden_tool|8420080|1
lilac|0|plant|0|1
lily_of_the_valley|0|plant|0|1
lily_pad|0|plant;mineable/axe|0|1
lime_banner|1|mineable/axe|0|1
lime_bed|0.2|default|0|1
lime_candle|0.1|default|0|1
lime_candle_cake|0.5|default|0|1
lime_carpet|0.1|default|0|1
lime_concrete|1.8|mineable/pickaxe|8421084|1
lime_concrete_powder|0.5|mineable/shovel|0|1
lime_glazed_terracotta|1.4|mineable/pickaxe|8421084|1
lime_shulker_box|2|mineable/pickaxe|0|1
lime_stained_glass|0.3|default|0|1
lime_stained_glass_pane|0.3|default|0|1
lime_terracotta|1.25|mineable/pickaxe|8421084|1
lime_wall_banner|1|mineable/axe|0|1
lime_wool|0.8|wool|0|1
lodestone|3.5|mineable/pickaxe|8421084|1
loom|2.5|mineable/axe|0|1
magenta_banner|1|mineable/axe|0|1
magenta_bed|0.2|default|0|1
magenta_candle|0.1|default|0|1
magenta_candle_cake|0.5|default|0|1
magenta_carpet|0.1|default|0|1
magenta_concrete|1.8|mineable/pickaxe|8421084|1
magenta_concrete_powder|0.5|mineable/shovel|0|1
magenta_glazed_terracotta|1.4|mineable/pickaxe|8421084|1
magenta_shulker_box|2|mineable/pickaxe|0|1
magenta_stained_glass|0.3|default|0|1
magenta_stained_glass_pane|0.3|default|0|1
magenta_terracotta|1.25|mineable/pickaxe|8421084|1
magenta_wall_banner|1|mineable/axe|0|1
magenta_wool|0.8|wool|0|1
magma_block|0.5|mineable/pickaxe|8421084|1
mangrove_button|0.5|mineable/axe|0|1
mangrove_door|3|mineable/axe|0|1
mangrove_fence|2|mineable/axe|0|1
mangrove_fence_gate|2|mineable/axe|0|1
mangrove_hanging_sign|1|mineable/axe|0|1
mangrove_leaves|0.2|leaves;mineable/hoe|0|1
mangrove_log|2|mineable/axe|0|1
mangrove_planks|2|mineable/axe|0|1
mangrove_pressure_plate|0.5|mineable/axe|0|1
mangrove_propagule|0|plant;mineable/axe|0|1
mangrove_roots|0.7|mineable/axe|0|1
mangrove_sign|1|mineable/axe|0|1
mangrove_slab|2|mineable/axe|0|1
mangrove_stairs|2|mineable/axe|0|1
mangrove_trapdoor|3|mineable/axe|0|1
mangrove_wall_hanging_sign|1|mineable/axe|0|1
mangrove_wall_sign|1|mineable/axe|0|1
mangrove_wood|2|mineable/axe|0|1
medium_amethyst_bud|1.5|mineable/pickaxe|0|1
melon|1|gourd;mineable/axe|0|1
melon_stem|0|plant;mineable/axe|0|1
moss_block|0.1|mineable/hoe|0|1
moss_carpet|0.1|plant|0|1
mossy_cobblestone|2|mineable/pickaxe|8421084|1
mossy_cobblestone_slab|2|mineable/pickaxe|8421084|1
mossy_cobblestone_stairs|2|mineable/pickaxe|8421084|1
mossy_cobblestone_wall|2|mineable/pickaxe|8421084|1
mossy_stone_brick_slab|1.5|mineable/pickaxe|8421084|1
mossy_stone_brick_stairs|1.5|mineable/pickaxe|8421084|1
mossy_stone_brick_wall|1.5|mineable/pickaxe|8421084|1
mossy_stone_bricks|1.5|mineable/pickaxe|8421084|1
moving_piston|-1|default|0|0
mud|0.5|mineable/shovel|0|1
mud_brick_slab|1.5|mineable/pickaxe|8421084|1
mud_brick_stairs|1.5|mineable/pickaxe|8421084|1
mud_brick_wall|1.5|mineable/pickaxe|8421084|1
mud_bricks|1.5|mineable/pickaxe|8421084|1
muddy_mangrove_roots|0.7|mineable/shovel|0|1
mushroom_stem|0.2|mineable/axe|0|1
mycelium|0.6|mineable/shovel|0|1
nether_brick_fence|2|mineable/pickaxe|8421084|1
nether_brick_slab|2|mineable/pickaxe|8421084|1
nether_brick_stairs|2|mineable/pickaxe|8421084|1
nether_brick_wall|2|mineable/pickaxe|8421084|1
nether_bricks|2|mineable/pickaxe|8421084|1
nether_gold_ore|3|mineable/pickaxe|8421084|1
nether_portal|-1|default|0|0
nether_quartz_ore|3|mineable/pickaxe|8421084|1
nether_sprouts|0|plant|0|1
nether_wart|0|plant;mineable/axe|0|1
nether_wart_block|1|mineable/hoe|0|1
netherite_block|50|incorrect_for_wooden_tool|8400000|1
netherrack|0.4|mineable/pickaxe|8421084|1
note_block|0.8|mineable/axe|0|1
oak_button|0.5|mineable/axe|0|1
oak_door|3|mineable/axe|0|1
oak_fence|2|mineable/axe|0|1
oak_fence_gate|2|mineable/axe|0|1
oak_hanging_sign|1|mineable/axe|0|1
oak_leaves|0.2|leaves;mineable/hoe|0|1
oak_log|2|mineable/axe|0|1
oak_planks|2|mineable/axe|0|1
oak_pressure_plate|0.5|mineable/axe|0|1
oak_sapling|0|plant;mineable/axe|0|1
oak_sign|1|mineable/axe|0|1
oak_slab|2|mineable/axe|0|1
oak_stairs|2|mineable/axe|0|1
oak_trapdoor|3|mineable/axe|0|1
oak_wall_hanging_sign|1|mineable/axe|0|1
oak_wall_sign|1|mineable/axe|0|1
oak_wood|2|mineable/axe|0|1
observer|3|mineable/pickaxe|8421084|1
obsidian|50|incorrect_for_wooden_tool|8400000|1
ochre_froglight|0.3|default|0|1
orange_banner|1|mineable/axe|0|1
orange_bed|0.2|default|0|1
orange_candle|0.1|default|0|1
orange_candle_cake|0.5|default|0|1
orange_carpet|0.1|default|0|1
orange_concrete|1.8|mineable/pickaxe|8421084|1
orange_concrete_powder|0.5|mineable/shovel|0|1
orange_glazed_terracotta|1.4|mineable/pickaxe|8421084|1
orange_shulker_box|2|mineable/pickaxe|0|1
orange_stained_glass|0.3|default|0|1
orange_stained_glass_pane|0.3|default|0|1
orange_terracotta|1.25|mineable/pickaxe|8421084|1
orange_tulip|0|plant|0|1
orange_wall_banner|1|mineable/axe|0|1
orange_wool|0.8|wool|0|1
oxeye_daisy|0|plant|0|1
oxidized_chiseled_copper|3|incorrect_for_wooden_tool|8420080|1
oxidized_copper|3|incorrect_for_wooden_tool|8420080|1
oxidized_copper_bulb|3|incorrect_for_wooden_tool|8420080|1
oxidized_copper_door|3|incorrect_for_wooden_tool|8420080|1
oxidized_copper_grate|3|incorrect_for_wooden_tool|8420080|1
oxidized_copper_trapdoor|3|incorrect_for_wooden_tool|8420080|1
oxidized_cut_copper|3|incorrect_for_wooden_tool|8420080|1
oxidized_cut_copper_slab|3|incorrect_for_wooden_tool|8420080|1
oxidized_cut_copper_stairs|3|incorrect_for_wooden_tool|8420080|1
packed_ice|0.5|mineable/pickaxe|0|1
packed_mud|1|mineable/pickaxe|0|1
pearlescent_froglight|0.3|default|0|1
peony|0|plant|0|1
petrified_oak_slab|2|mineable/pickaxe|8421084|1
piglin_head|1|default|0|1
piglin_wall_head|1|default|0|1
pink_banner|1|mineable/axe|0|1
pink_bed|0.2|default|0|1
pink_candle|0.1|default|0|1
pink_candle_cake|0.5|default|0|1
pink_carpet|0.1|default|0|1
pink_concrete|1.8|mineable/pickaxe|8421084|1
pink_concrete_powder|0.5|mineable/shovel|0|1
pink_glazed_terracotta|1.4|mineable/pickaxe|8421084|1
pink_petals|0|plant|0|1
pink_shulker_box|2|mineable/pickaxe|0|1
pink_stained_glass|0.3|default|0|1
pink_stained_glass_pane|0.3|default|0|1
pink_terracotta|1.25|mineable/pickaxe|8421084|1
pink_tulip|0|plant|0|1
pink_wall_banner|1|mineable/axe|0|1
pink_wool|0.8|wool|0|1
piston|1.5|mineable/pickaxe|0|1
piston_head|1.5|mineable/pickaxe|0|1
pitcher_crop|0|plant|0|1
pitcher_plant|0|plant|0|1
player_head|1|default|0|1
player_wall_head|1|default|0|1
podzol|0.5|mineable/shovel|0|1
pointed_dripstone|1.5|mineable/pickaxe|0|1
polished_andesite|1.5|mineable/pickaxe|8421084|1
polished_andesite_slab|1.5|mineable/pickaxe|8421084|1
polished_andesite_stairs|1.5|mineable/pickaxe|8421084|1
polished_basalt|1.25|mineable/pickaxe|8421084|1
polished_blackstone|2|mineable/pickaxe|8421084|1
polished_blackstone_brick_slab|2|mineable/pickaxe|8421084|1
polished_blackstone_brick_stairs|1.5|mineable/pickaxe|8421084|1
polished_blackstone_brick_wall|1.5|mineable/pickaxe|8421084|1
polished_blackstone_bricks|1.5|mineable/pickaxe|8421084|1
polished_blackstone_button|0.5|mineable/pickaxe|0|1
polished_blackstone_pressure_plate|0.5|mineable/pickaxe|8421084|1
polished_blackstone_slab|2|mineable/pickaxe|8421084|1
polished_blackstone_stairs|2|mineable/pickaxe|8421084|1
polished_blackstone_wall|2|mineable/pickaxe|8421084|1
polished_deepslate|3.5|mineable/pickaxe|8421084|1
polished_deepslate_slab|3.5|mineable/pickaxe|8421084|1
polished_deepslate_stairs|3.5|mineable/pickaxe|8421084|1
polished_deepslate_wall|3.5|mineable/pickaxe|8421084|1
polished_diorite|1.5|mineable/pickaxe|8421084|1
polished_diorite_slab|1.5|mineable/pickaxe|8421084|1
polished_diorite_stairs|1.5|mineable/pickaxe|8421084|1
polished_granite|1.5|mineable/pickaxe|8421084|1
polished_granite_slab|1.5|mineable/pickaxe|8421084|1
polished_granite_stairs|1.5|mineable/pickaxe|8421084|1
polished_tuff|1.5|mineable/pickaxe|8421084|1
polished_tuff_slab|1.5|mineable/pickaxe|8421084|1
polished_tuff_stairs|1.5|mineable/pickaxe|8421084|1
polished_tuff_wall|1.5|mineable/pickaxe|8421084|1
poppy|0|plant|0|1
potatoes|0|plant;mineable/axe|0|1
potted_acacia_sapling|0|default|0|1
potted_allium|0|default|0|1
potted_azalea_bush|0|default|0|1
potted_azure_bluet|0|default|0|1
potted_bamboo|0|default|0|1
potted_birch_sapling|0|default|0|1
potted_blue_orchid|0|default|0|1
potted_brown_mushroom|0|default|0|1
potted_cactus|0|default|0|1
potted_cherry_sapling|0|default|0|1
potted_cornflower|0|default|0|1
potted_crimson_fungus|0|default|0|1
potted_crimson_roots|0|default|0|1
potted_dandelion|0|default|0|1
potted_dark_oak_sapling|0|default|0|1
potted_dead_bush|0|default|0|1
potted_fern|0|default|0|1
potted_flowering_azalea_bush|0|default|0|1
potted_jungle_sapling|0|default|0|1
potted_lily_of_the_valley|0|default|0|1
potted_mangrove_propagule|0|default|0|1
potted_oak_sapling|0|default|0|1
potted_orange_tulip|0|default|0|1
potted_oxeye_daisy|0|default|0|1
potted_pink_tulip|0|default|0|1
potted_poppy|0|default|0|1
potted_red_mushroom|0|default|0|1
potted_red_tulip|0|default|0|1
potted_spruce_sapling|0|default|0|1
potted_torchflower|0|default|0|1
potted_warped_fungus|0|default|0|1
potted_warped_roots|0|default|0|1
potted_white_tulip|0|default|0|1
potted_wither_rose|0|default|0|1
powder_snow|0.25|default|0|1
powder_snow_cauldron|2|mineable/pickaxe|8421084|1
powered_rail|0.7|mineable/pickaxe|0|1
prismarine|1.5|mineable/pickaxe|8421084|1
prismarine_brick_slab|1.5|mineable/pickaxe|8421084|1
prismarine_brick_stairs|1.5|mineable/pickaxe|8421084|1
prismarine_bricks|1.5|mineable/pickaxe|8421084|1
prismarine_slab|1.5|mineable/pickaxe|8421084|1
prismarine_stairs|1.5|mineable/pickaxe|8421084|1
prismarine_wall|1.5|mineable/pickaxe|8421084|1
pumpkin|1|gourd;mineable/axe|0|1
pumpkin_stem|0|plant;mineable/axe|0|1
purple_banner|1|mineable/axe|0|1
purple_bed|0.2|default|0|1
purple_candle|0.1|default|0|1
purple_candle_cake|0.5|default|0|1
purple_carpet|0.1|default|0|1
purple_concrete|1.8|mineable/pickaxe|8421084|1
purple_concrete_powder|0.5|mineable/shovel|0|1
purple_glazed_terracotta|1.4|mineable/pickaxe|8421084|1
purple_shulker_box|2|mineable/pickaxe|0|1
purple_stained_glass|0.3|default|0|1
purple_stained_glass_pane|0.3|default|0|1
purple_terracotta|1.25|mineable/pickaxe|8421084|1
purple_wall_banner|1|mineable/axe|0|1
purple_wool|0.8|wool|0|1
purpur_block|1.5|mineable/pickaxe|8421084|1
purpur_pillar|1.5|mineable/pickaxe|8421084|1
purpur_slab|2|mineable/pickaxe|8421084|1
purpur_stairs|1.5|mineable/pickaxe|8421084|1
quartz_block|0.8|mineable/pickaxe|8421084|1
quartz_bricks|0.8|mineable/pickaxe|8421084|1
quartz_pillar|0.8|mineable/pickaxe|8421084|1
quartz_slab|2|mineable/pickaxe|8421084|1
quartz_stairs|0.8|mineable/pickaxe|8421084|1
rail|0.7|mineable/pickaxe|0|1
raw_copper_block|5|incorrect_for_wooden_tool|8420080|1
raw_gold_block|5|incorrect_for_wooden_tool|8420000|1
raw_iron_block|5|incorrect_for_wooden_tool|8420080|1
red_banner|1|mineable/axe|0|1
red_bed|0.2|default|0|1
red_candle|0.1|default|0|1
red_candle_cake|0.5|default|0|1
red_carpet|0.1|default|0|1
red_concrete|1.8|mineable/pickaxe|8421084|1
red_concrete_powder|0.5|mineable/shovel|0|1
red_glazed_terracotta|1.4|mineable/pickaxe|8421084|1
red_mushroom|0|plant;mineable/axe|0|1
red_mushroom_block|0.2|mineable/axe|0|1
red_nether_brick_slab|2|mineable/pickaxe|8421084|1
red_nether_brick_stairs|2|mineable/pickaxe|8421084|1
red_nether_brick_wall|2|mineable/pickaxe|8421084|1
red_nether_bricks|2|mineable/pickaxe|8421084|1
red_sand|0.5|mineable/shovel|0|1
red_sandstone|0.8|mineable/pickaxe|8421084|1
red_sandstone_slab|2|mineable/pickaxe|8421084|1
red_sandstone_stairs|0.8|mineable/pickaxe|8421084|1
red_sandstone_wall|0.8|mineable/pickaxe|8421084|1
red_shulker_box|2|mineable/pickaxe|0|1
red_stained_glass|0.3|default|0|1
red_stained_glass_pane|0.3|default|0|1
red_terracotta|1.25|mineable/pickaxe|8421084|1
red_tulip|0|plant|0|1
red_wall_banner|1|mineable/axe|0|1
red_wool|0.8|wool|0|1
redstone_block|5|mineable/pickaxe|8421084|1
redstone_lamp|0.3|default|0|1
redstone_ore|3|incorrect_for_wooden_tool|8420000|1
redstone_torch|0|default|0|1
redstone_wall_torch|0|default|0|1
redstone_wire|0|default|0|1
reinforced_deepslate|55|default|0|1
repeater|0|default|0|1
repeating_command_block|-1|default|0|0
respawn_anchor|50|incorrect_for_wooden_tool|8400000|1
rooted_dirt|0.5|mineable/shovel|0|1
rose_bush|0|plant|0|1
sand|0.5|mineable/shovel|0|1
sandstone|0.8|mineable/pickaxe|8421084|1
sandstone_slab|2|mineable/pickaxe|8421084|1
sandstone_stairs|0.8|mineable/pickaxe|8421084|1
sandstone_wall|0.8|mineable/pickaxe|8421084|1
scaffolding|0|mineable/axe|0|1
sculk|0.2|mineable/hoe|0|1
sculk_catalyst|3|mineable/hoe|0|1
sculk_sensor|1.5|mineable/hoe|0|1
sculk_shrieker|3|mineable/hoe|0|1
sculk_vein|0.2|mineable/hoe|0|1
sea_lantern|0.3|default|0|1
sea_pickle|0|default|0|1
seagrass|0|default|0|1
short_grass|0|plant;mineable/axe|0|1
shroomlight|1|mineable/hoe|0|1
shulker_box|2|mineable/pickaxe|0|1
skeleton_skull|1|default|0|1
skeleton_wall_skull|1|default|0|1
slime_block|0|default|0|1
small_amethyst_bud|1.5|mineable/pickaxe|0|1
small_dripleaf|0|plant;mineable/axe|0|1
smithing_table|2.5|mineable/axe|0|1
smoker|3.5|mineable/pickaxe|8421084|1
smooth_basalt|1.25|mineable/pickaxe|8421084|1
smooth_quartz|2|mineable/pickaxe|8421084|1
smooth_quartz_slab|2|mineable/pickaxe|8421084|1
smooth_quartz_stairs|2|mineable/pickaxe|8421084|1
smooth_red_sandstone|2|mineable/pickaxe|8421084|1
smooth_red_sandstone_slab|2|mineable/pickaxe|8421084|1
smooth_red_sandstone_stairs|2|mineable/pickaxe|8421084|1
smooth_sandstone|2|mineable/pickaxe|8421084|1
smooth_sandstone_slab|2|mineable/pickaxe|8421084|1
smooth_sandstone_stairs|2|mineable/pickaxe|8421084|1
smooth_stone|2|mineable/pickaxe|8421084|1
smooth_stone_slab|2|mineable/pickaxe|8421084|1
sniffer_egg|0.5|default|0|1
snow|0.1|mineable/shovel|4210842|1
snow_block|0.2|mineable/shovel|4210842|1
soul_campfire|2|mineable/axe|0|1
soul_fire|0|default|0|1
soul_lantern|3.5|mineable/pickaxe|8421084|1
soul_sand|0.5|mineable/shovel|0|1
soul_soil|0.5|mineable/shovel|0|1
soul_torch|0|default|0|1
soul_wall_torch|0|default|0|1
spawner|5|mineable/pickaxe|8421084|1
sponge|0.6|mineable/hoe|0|1
spore_blossom|0|plant;mineable/axe|0|1
spruce_button|0.5|mineable/axe|0|1
spruce_door|3|mineable/axe|0|1
spruce_fence|2|mineable/axe|0|1
spruce_fence_gate|2|mineable/axe|0|1
spruce_hanging_sign|1|mineable/axe|0|1
spruce_leaves|0.2|leaves;mineable/hoe|0|1
spruce_log|2|mineable/axe|0|1
spruce_planks|2|mineable/axe|0|1
spruce_pressure_plate|0.5|mineable/axe|0|1
spruce_sapling|0|plant;mineable/axe|0|1
spruce_sign|1|mineable/axe|0|1
spruce_slab|2|mineable/axe|0|1
spruce_stairs|2|mineable/axe|0|1
spruce_trapdoor|3|mineable/axe|0|1
spruce_wall_hanging_sign|1|mineable/axe|0|1
spruce_wall_sign|1|mineable/axe|0|1
spruce_wood|2|mineable/axe|0|1
sticky_piston|1.5|mineable/pickaxe|0|1
stone|1.5|mineable/pickaxe|8421084|1
stone_brick_slab|2|mineable/pickaxe|8421084|1
stone_brick_stairs|1.5|mineable/pickaxe|8421084|1
stone_brick_wall|1.5|mineable/pickaxe|8421084|1
stone_bricks|1.5|mineable/pickaxe|8421084|1
stone_button|0.5|mineable/pickaxe|0|1
stone_pressure_plate|0.5|mineable/pickaxe|8421084|1
stone_slab|2|mineable/pickaxe|8421084|1
stone_stairs|1.5|mineable/pickaxe|8421084|1
stonecutter|3.5|mineable/pickaxe|8421084|1
stripped_acacia_log|2|mineable/axe|0|1
stripped_acacia_wood|2|mineable/axe|0|1
stripped_bamboo_block|2|mineable/axe|0|1
stripped_birch_log|2|mineable/axe|0|1
stripped_birch_wood|2|mineable/axe|0|1
stripped_cherry_log|2|mineable/axe|0|1
stripped_cherry_wood|2|mineable/axe|0|1
stripped_crimson_hyphae|2|mineable/axe|0|1
stripped_crimson_stem|2|mineable/axe|0|1
stripped_dark_oak_log|2|mineable/axe|0|1
stripped_dark_oak_wood|2|mineable/axe|0|1
stripped_jungle_log|2|mineable/axe|0|1
stripped_jungle_wood|2|mineable/axe|0|1
stripped_mangrove_log|2|mineable/axe|0|1
stripped_mangrove_wood|2|mineable/axe|0|1
stripped_oak_log|2|mineable/axe|0|1
stripped_oak_wood|2|mineable/axe|0|1
stripped_spruce_log|2|mineable/axe|0|1
stripped_spruce_wood|2|mineable/axe|0|1
stripped_warped_hyphae|2|mineable/axe|0|1
stripped_warped_stem|2|mineable/axe|0|1
structure_block|-1|default|0|0
structure_void|0|default|0|1
sugar_cane|0|plant;mineable/axe|0|1
sunflower|0|plant|0|1
suspicious_gravel|0.25|mineable/shovel|0|1
suspicious_sand|0.25|mineable/shovel|0|1
sweet_berry_bush|0|plant;mineable/axe|0|1
tall_grass|0|plant;mineable/axe|0|1
tall_seagrass|0|default|0|1
target|0.5|mineable/hoe|0|1
terracotta|1.25|mineable/pickaxe|8421084|1
tinted_glass|0.3|default|0|1
tnt|0|default|0|1
torch|0|default|0|1
torchflower|0|plant|0|1
torchflower_crop|0|plant|0|1
trapped_chest|2.5|mineable/axe|0|1
trial_spawner|50|default|0|1
tripwire|0|default|0|1
tripwire_hook|0|default|0|1
tube_coral|0|default|0|1
tube_coral_block|1.5|mineable/pickaxe|8421084|1
tube_coral_fan|0|default|0|1
tube_coral_wall_fan|0|default|0|1
tuff|1.5|mineable/pickaxe|8421084|1
tuff_brick_slab|1.5|mineable/pickaxe|8421084|1
tuff_brick_stairs|1.5|mineable/pickaxe|8421084|1
tuff_brick_wall|1.5|mineable/pickaxe|8421084|1
tuff_bricks|1.5|mineable/pickaxe|8421084|1
tuff_slab|1.5|mineable/pickaxe|8421084|1
tuff_stairs|1.5|mineable/pickaxe|8421084|1
tuff_wall|1.5|mineable/pickaxe|8421084|1
turtle_egg|0.5|default|0|1
twisting_vines|0|plant;mineable/axe|0|1
twisting_vines_plant|0|plant;mineable/axe|0|1
vault|50|default|0|1
verdant_froglight|0.3|default|0|1
vine|0.2|vine_or_glow_lichen;plant;mineable/axe|0|1
void_air|0|default|0|0
wall_torch|0|default|0|1
warped_button|0.5|mineable/axe|0|1
warped_door|3|mineable/axe|0|1
warped_fence|2|mineable/axe|0|1
warped_fence_gate|2|mineable/axe|0|1
warped_fungus|0|plant;mineable/axe|0|1
warped_hanging_sign|1|mineable/axe|0|1
warped_hyphae|2|mineable/axe|0|1
warped_nylium|0.4|mineable/pickaxe|8421084|1
warped_planks|2|mineable/axe|0|1
warped_pressure_plate|0.5|mineable/axe|0|1
warped_roots|0|plant|0|1
warped_sign|1|mineable/axe|0|1
warped_slab|2|mineable/axe|0|1
warped_stairs|2|mineable/axe|0|1
warped_stem|2|mineable/axe|0|1
warped_trapdoor|3|mineable/axe|0|1
warped_wall_hanging_sign|1|mineable/axe|0|1
warped_wall_sign|1|mineable/axe|0|1
warped_wart_block|1|mineable/hoe|0|1
water|100|default|0|1
water_cauldron|2|mineable/pickaxe|8421084|1
waxed_chiseled_copper|3|incorrect_for_wooden_tool|8420080|1
waxed_copper_block|3|incorrect_for_wooden_tool|8420080|1
waxed_copper_bulb|3|incorrect_for_wooden_tool|8420080|1
waxed_copper_door|3|incorrect_for_wooden_tool|8420080|1
waxed_copper_grate|3|incorrect_for_wooden_tool|8420080|1
waxed_copper_trapdoor|3|incorrect_for_wooden_tool|8420080|1
waxed_cut_copper|3|incorrect_for_wooden_tool|8420080|1
waxed_cut_copper_slab|3|incorrect_for_wooden_tool|8420080|1
waxed_cut_copper_stairs|3|incorrect_for_wooden_tool|8420080|1
waxed_exposed_chiseled_copper|3|incorrect_for_wooden_tool|8420080|1
waxed_exposed_copper|3|incorrect_for_wooden_tool|8420080|1
waxed_exposed_copper_bulb|3|incorrect_for_wooden_tool|8420080|1
waxed_exposed_copper_door|3|incorrect_for_wooden_tool|8420080|1
waxed_exposed_copper_grate|3|incorrect_for_wooden_tool|8420080|1
waxed_exposed_copper_trapdoor|3|incorrect_for_wooden_tool|8420080|1
waxed_exposed_cut_copper|3|incorrect_for_wooden_tool|8420080|1
waxed_exposed_cut_copper_slab|3|incorrect_for_wooden_tool|8420080|1
waxed_exposed_cut_copper_stairs|3|incorrect_for_wooden_tool|8420080|1
waxed_oxidized_chiseled_copper|3|incorrect_for_wooden_tool|8420080|1
waxed_oxidized_copper|3|incorrect_for_wooden_tool|8420080|1
waxed_oxidized_copper_bulb|3|incorrect_for_wooden_tool|8420080|1
waxed_oxidized_copper_door|3|incorrect_for_wooden_tool|8420080|1
waxed_oxidized_copper_grate|3|incorrect_for_wooden_tool|8420080|1
waxed_oxidized_copper_trapdoor|3|incorrect_for_wooden_tool|8420080|1
waxed_oxidized_cut_copper|3|incorrect_for_wooden_tool|8420080|1
waxed_oxidized_cut_copper_slab|3|incorrect_for_wooden_tool|8420080|1
waxed_oxidized_cut_copper_stairs|3|incorrect_for_wooden_tool|8420080|1
waxed_weathered_chiseled_copper|3|incorrect_for_wooden_tool|8420080|1
waxed_weathered_copper|3|incorrect_for_wooden_tool|8420080|1
waxed_weathered_copper_bulb|3|incorrect_for_wooden_tool|8420080|1
waxed_weathered_copper_door|3|incorrect_for_wooden_tool|8420080|1
waxed_weathered_copper_grate|3|incorrect_for_wooden_tool|8420080|1
waxed_weathered_copper_trapdoor|3|incorrect_for_wooden_tool|8420080|1
waxed_weathered_cut_copper|3|incorrect_for_wooden_tool|8420080|1
waxed_weathered_cut_copper_slab|3|incorrect_for_wooden_tool|8420080|1
waxed_weathered_cut_copper_stairs|3|incorrect_for_wooden_tool|8420080|1
weathered_chiseled_copper|3|incorrect_for_wooden_tool|8420080|1
weathered_copper|3|incorrect_for_wooden_tool|8420080|1
weathered_copper_bulb|3|incorrect_for_wooden_tool|8420080|1
weathered_copper_door|3|incorrect_for_wooden_tool|8420080|1
weathered_copper_grate|3|incorrect_for_wooden_tool|8420080|1
weathered_copper_trapdoor|3|incorrect_for_wooden_tool|8420080|1
weathered_cut_copper|3|incorrect_for_wooden_tool|8420080|1
weathered_cut_copper_slab|3|incorrect_for_wooden_tool|8420080|1
weathered_cut_copper_stairs|3|incorrect_for_wooden_tool|8420080|1
weeping_vines|0|plant;mineable/axe|0|1
weeping_vines_plant|0|plant;mineable/axe|0|1
wet_sponge|0.6|mineable/hoe|0|1
wheat|0|plant;mineable/axe|0|1
white_banner|1|mineable/axe|0|1
white_bed|0.2|default|0|1
white_candle|0.1|default|0|1
white_candle_cake|0.5|default|0|1
white_carpet|0.1|default|0|1
white_concrete|1.8|mineable/pickaxe|8421084|1
white_concrete_powder|0.5|mineable/shovel|0|1
white_glazed_terracotta|1.4|mineable/pickaxe|8421084|1
white_shulker_box|2|mineable/pickaxe|0|1
white_stained_glass|0.3|default|0|1
white_stained_glass_pane|0.3|default|0|1
white_terracotta|1.25|mineable/pickaxe|8421084|1
white_tulip|0|plant|0|1
white_wall_banner|1|mineable/axe|0|1
white_wool|0.8|wool|0|1
wither_rose|0|plant|0|1
wither_skeleton_skull|1|default|0|1
wither_skeleton_wall_skull|1|default|0|1
yellow_banner|1|mineable/axe|0|1
yellow_bed|0.2|default|0|1
yellow_candle|0.1|default|0|1
yellow_candle_cake|0.5|default|0|1
yellow_carpet|0.1|default|0|1
yellow_concrete|1.8|mineable/pickaxe|8421084|1
yellow_concrete_powder|0.5|mineable/shovel|0|1
yellow_glazed_terracotta|1.4|mineable/pickaxe|8421084|1
yellow_shulker_box|2|mineable/pickaxe|0|1
yellow_stained_glass|0.3|default|0|1
yellow_stained_glass_pane|0.3|default|0|1
yellow_terracotta|1.25|mineable/pickaxe|8421084|1
yellow_wall_banner|1|mineable/axe|0|1
yellow_wool|0.8|wool|0|1
zombie_head|1|default|0|1
zombie_wall_head|1|default|0|1"#;

#[derive(Clone, Copy, Debug)]
struct VanillaBlockMiningMeta {
    hardness: f32,
    preferred_tool: Option<HarvestTool>,
    harvest_mask: u64,
    diggable: bool,
}

fn vanilla_mining_material_tool(material: &str) -> Option<HarvestTool> {
    if material == "incorrect_for_wooden_tool" || material.contains("mineable/pickaxe") {
        Some(HarvestTool::Pickaxe)
    } else if material.contains("mineable/axe") {
        Some(HarvestTool::Axe)
    } else if material.contains("mineable/shovel") {
        Some(HarvestTool::Shovel)
    } else if material.contains("mineable/hoe") {
        Some(HarvestTool::Hoe)
    } else {
        None
    }
}

/// Exact 1.21.1 hardness, preferred tool family, correct-tool item set and
/// diggability for a vanilla block; unknown modded blocks use the conservative
/// generic 1.5-second hand-mining fallback.
fn vanilla_block_mining_meta(block_id: &str) -> VanillaBlockMiningMeta {
    let path = canonical_block_path(block_id);
    for row in VANILLA_BLOCK_MINING_ROWS.split('\n') {
        let mut parts = row.split('|');
        if parts.next() != Some(path) {
            continue;
        }
        let hardness = parts
            .next()
            .and_then(|value| value.parse::<f32>().ok())
            .unwrap_or(1.5);
        let material = parts.next().unwrap_or("default");
        let harvest_mask = parts
            .next()
            .and_then(|value| u64::from_str_radix(value, 16).ok())
            .unwrap_or(0);
        let diggable = parts.next() == Some("1");
        return VanillaBlockMiningMeta {
            hardness,
            preferred_tool: vanilla_mining_material_tool(material),
            harvest_mask,
            diggable,
        };
    }
    VanillaBlockMiningMeta {
        hardness: 1.5,
        preferred_tool: preferred_harvest_tool(block_id),
        harvest_mask: 0,
        diggable: true,
    }
}

fn vanilla_tool_harvest_bit(item_id: i32) -> u64 {
    if (818..=847).contains(&item_id) {
        1u64 << (item_id - 818)
    } else if item_id == 983 {
        1u64 << 40
    } else {
        0
    }
}

fn canonical_block_path(block_id: &str) -> &str {
    block_state_base_name(block_id)
        .rsplit(':')
        .next()
        .unwrap_or(block_id)
}

/// `(tool family, vanilla speed)` for a held tiered tool.
fn mining_tool_facts(item_name: &str) -> (Option<HarvestTool>, f32) {
    let path = item_name.rsplit(':').next().unwrap_or(item_name);
    let kind = if path.ends_with("_pickaxe") {
        Some(HarvestTool::Pickaxe)
    } else if path.ends_with("_axe") {
        Some(HarvestTool::Axe)
    } else if path.ends_with("_shovel") {
        Some(HarvestTool::Shovel)
    } else if path.ends_with("_hoe") {
        Some(HarvestTool::Hoe)
    } else {
        None
    };
    let speed = if path.starts_with("netherite_") {
        9.0
    } else if path.starts_with("diamond_") {
        8.0
    } else if path.starts_with("iron_") {
        6.0
    } else if path.starts_with("stone_") {
        4.0
    } else if path.starts_with("golden_") {
        12.0
    } else if path.starts_with("wooden_") || path == "shears" {
        2.0
    } else {
        1.0
    };
    (
        kind,
        if kind.is_some() || path == "shears" {
            speed
        } else {
            1.0
        },
    )
}

fn sword_efficient_block(block_id: &str) -> bool {
    let path = canonical_block_path(block_id);
    path.ends_with("_leaves")
        || matches!(
            path,
            "vine"
                | "glow_lichen"
                | "pumpkin"
                | "carved_pumpkin"
                | "jack_o_lantern"
                | "melon"
                | "cocoa"
                | "big_dripleaf"
                | "big_dripleaf_stem"
                | "chorus_plant"
                | "chorus_flower"
        )
}

/// The `minecraft:tool` component half of vanilla `Player.getDestroySpeed`:
/// shears and swords carry block-specific rules, while tiered tools use their
/// material speed only against the block's preferred tool family.
fn mining_item_destroy_speed(
    item_name: &str,
    block_id: &str,
    required_tool: Option<HarvestTool>,
    tool_kind: Option<HarvestTool>,
    tier_speed: f32,
    effective_tool: bool,
) -> f32 {
    let item = item_name.rsplit(':').next().unwrap_or(item_name);
    let block = canonical_block_path(block_id);
    if item == "shears" {
        return if block == "cobweb" || block.ends_with("_leaves") {
            15.0
        } else if block.ends_with("_wool") {
            5.0
        } else if matches!(block, "vine" | "glow_lichen") {
            2.0
        } else {
            1.0
        };
    }
    if item.ends_with("_sword") {
        return if block == "cobweb" {
            15.0
        } else if matches!(block, "bamboo" | "bamboo_sapling") {
            f32::MAX
        } else if sword_efficient_block(block_id) {
            1.5
        } else {
            1.0
        };
    }
    if tool_kind.is_some() && required_tool.is_some() && effective_tool {
        tier_speed
    } else {
        1.0
    }
}

fn component_varint(data: &[u8], pos: &mut usize) -> Option<i32> {
    let (value, len) = megu_core::varint::read_varint_i32(data.get(*pos..)?).ok()?;
    *pos += len;
    Some(value)
}

/// Decode an enchantment level from the component-preserving stack patches.
/// Protocol 767 enchantment registry ids are alphabetical; efficiency is 8
/// and aqua_affinity is 0.
fn item_enchantment_level(stack: &ItemStackComponents, enchantment_id: i32) -> i32 {
    for patch in &stack.added {
        if patch.type_id != COMP_ENCHANTMENTS && patch.type_id != COMP_STORED_ENCHANTMENTS {
            continue;
        }
        let mut pos = 0usize;
        let Some(count) = component_varint(&patch.data, &mut pos) else {
            continue;
        };
        for _ in 0..count.clamp(0, 256) {
            let (Some(id), Some(level)) = (
                component_varint(&patch.data, &mut pos),
                component_varint(&patch.data, &mut pos),
            ) else {
                break;
            };
            if id == enchantment_id {
                return level.max(0);
            }
        }
    }
    0
}

fn component_patch(stack: &ItemStackComponents, type_id: i32) -> Option<&[u8]> {
    if stack.removed.contains(&type_id) {
        return None;
    }
    stack
        .added
        .iter()
        .find(|patch| patch.type_id == type_id)
        .map(|patch| patch.data.as_slice())
}

fn component_varint_value(stack: &ItemStackComponents, type_id: i32) -> Option<i32> {
    let data = component_patch(stack, type_id)?;
    let mut pos = 0usize;
    component_varint(data, &mut pos)
}

fn component_text_value(stack: &ItemStackComponents, type_id: i32) -> Option<String> {
    nbt_text(component_patch(stack, type_id)?)
}

fn component_lore_values(stack: &ItemStackComponents) -> Vec<String> {
    let Some(data) = component_patch(stack, COMP_LORE) else {
        return Vec::new();
    };
    let mut reader = WireReader::new(data);
    let Ok(count) = reader.read_varint() else {
        return Vec::new();
    };
    let mut lore = Vec::new();
    for _ in 0..count.clamp(0, 256) {
        let before = reader.remaining();
        if before == 0 {
            break;
        }
        // Entries are optional anonymous NBT payloads: a zero byte denotes
        // absent; otherwise decode one component and use the reader's cursor
        // movement to reach the next entry.
        if reader.rest().first() == Some(&0) {
            let _ = reader.take(1);
            continue;
        }
        let Ok(value) = reader.read_nbt() else {
            break;
        };
        let text = value.extract_text();
        if !text.is_empty() {
            lore.push(text);
        }
    }
    lore
}

fn item_prototype_max_damage(item_name: &str) -> Option<i32> {
    let path = item_name.rsplit(':').next().unwrap_or(item_name);
    for (tier, durability) in [
        ("netherite", 2031),
        ("diamond", 1561),
        ("iron", 250),
        ("stone", 131),
        ("golden", 32),
        ("wooden", 59),
    ] {
        if path.starts_with(&format!("{tier}_"))
            && ["sword", "pickaxe", "axe", "shovel", "hoe"]
                .iter()
                .any(|kind| path.ends_with(kind))
        {
            return Some(durability);
        }
    }
    for piece in ["helmet", "chestplate", "leggings", "boots"] {
        let Some(material) = path.strip_suffix(&format!("_{piece}")) else {
            continue;
        };
        let multiplier = match material {
            "leather" => 5,
            "chainmail" | "iron" => 15,
            "golden" => 7,
            "diamond" => 33,
            "netherite" => 37,
            _ => return None,
        };
        let base = match piece {
            "helmet" => 11,
            "chestplate" => 16,
            "leggings" => 15,
            _ => 13,
        };
        return Some(multiplier * base);
    }
    let value = match path {
        "shears" => 238,
        "flint_and_steel" | "fishing_rod" | "brush" | "wolf_armor" => 64,
        "carrot_on_a_stick" => 25,
        "warped_fungus_on_a_stick" => 100,
        "bow" => 384,
        "crossbow" => 465,
        "trident" => 250,
        "mace" => 500,
        "elytra" => 432,
        "shield" => 336,
        "turtle_shell" => 275,
        _ => return None,
    };
    Some(value)
}

fn live_stack_summary(stack: &ItemStackComponents) -> Option<LiveHotbarStack> {
    if stack.item_id <= 0 || stack.count <= 0 {
        return None;
    }
    let item_name = crate::recipe_ingest::item_name_for_id(stack.item_id as u32);
    Some(LiveHotbarStack {
        max_damage: component_varint_value(stack, COMP_MAX_DAMAGE)
            .or_else(|| item_prototype_max_damage(&item_name)),
        damage: component_varint_value(stack, COMP_DAMAGE),
        custom_name: component_text_value(stack, COMP_CUSTOM_NAME),
        lore: component_lore_values(stack),
        enchanted: item_stack_has_glint(stack),
        count: stack.count,
        item_name,
    })
}

/// Best-effort harvest-tool family from a registry block id — the
/// looking-at HUD's tool hint while the session does not mirror block
/// material/destroy-time metadata. Ordered so the more specific
/// material families win over shared substrings (`sandstone` is
/// pickaxe-class, `iron_door` pickaxe over axe).
fn preferred_harvest_tool(block_id: &str) -> Option<HarvestTool> {
    let path = block_state_base_name(block_id)
        .rsplit(':')
        .next()
        .unwrap_or(block_id);
    const PICKAXE: &[&str] = &[
        "stone",
        "ore",
        "deepslate",
        "cobblestone",
        "netherrack",
        "blackstone",
        "basalt",
        "andesite",
        "diorite",
        "granite",
        "brick",
        "furnace",
        "rail",
        "obsidian",
        "ancient_debris",
        "concrete",
        "terracotta",
        "iron",
        "gold",
        "copper",
        "diamond",
        "emerald",
        "lapis",
        "quartz",
        "redstone",
        "amethyst",
        "calcite",
        "tuff",
        "dripstone",
        "netherite",
        "lodestone",
        "anvil",
        "bell",
        "cauldron",
        "hopper",
        "smoker",
        "blast_furnace",
        "grindstone",
        "stonecutter",
        "lantern",
        "chain",
        "ice",
        "packed_ice",
        "blue_ice",
        "packed_mud",
        "magma",
        "prismarine",
        "purpur",
        "end_stone",
        "nylium",
    ];
    const AXE: &[&str] = &[
        "log",
        "wood",
        "planks",
        "stem",
        "hyphae",
        "fence",
        "door",
        "chest",
        "barrel",
        "ladder",
        "sign",
        "banner",
        "bed",
        "bookshelf",
        "jukebox",
        "note_block",
        "crafting_table",
        "cartography_table",
        "fletching_table",
        "smithing_table",
        "loom",
        "composter",
        "beehive",
        "pumpkin",
        "melon",
        "bamboo",
    ];
    const SHOVEL: &[&str] = &[
        "dirt",
        "grass_block",
        "sand",
        "gravel",
        "clay",
        "snow",
        "soul_sand",
        "soul_soil",
        "mud",
        "podzol",
        "mycelium",
        "farmland",
        "dirt_path",
        "coarse_dirt",
        "rooted_dirt",
        "powder_snow",
        "concrete_powder",
    ];
    const HOE: &[&str] = &[
        "leaves",
        "hay_block",
        "moss",
        "sculk",
        "nether_wart_block",
        "warped_wart_block",
        "sponge",
        "shroomlight",
        "dried_kelp_block",
        "target",
    ];
    // Exact substrings that would otherwise hit the wrong family —
    // `concrete_powder` is shovel-class inside `concrete`'s pickaxe
    // match, and `ender_chest` is pickaxe-class inside `chest`'s axe.
    const OVERRIDES: &[(&str, HarvestTool)] = &[
        ("concrete_powder", HarvestTool::Shovel),
        ("ender_chest", HarvestTool::Pickaxe),
    ];
    for (keyword, tool) in OVERRIDES {
        if path.contains(keyword) {
            return Some(*tool);
        }
    }
    // Pickaxe first so shared substrings (`sandstone`, `redstone_ore`,
    // `iron_door`) resolve to their vanilla-correct tool family.
    for (family, tool) in [
        (PICKAXE, HarvestTool::Pickaxe),
        (AXE, HarvestTool::Axe),
        (SHOVEL, HarvestTool::Shovel),
        (HOE, HarvestTool::Hoe),
    ] {
        if family.iter().any(|keyword| path.contains(keyword)) {
            return Some(tool);
        }
    }
    None
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
    use crate::Camera;
    use crate::renderer::{
        GpuRenderer, OFFSCREEN_FORMAT, RendererError, probe_adapters, request_gpu_device,
        section_geometry,
    };
    use megu_client::protocol::{
        CHANNEL_ANIM_V1, CHANNEL_MAGIC_V1, CHANNEL_MOTION_V1, CHANNEL_SEASON_V1, CHANNEL_VFX_V1,
        DecodedMotionPhase, EquipmentEntry,
        SSN1_MAGIC, SSN1_VERSION, WireReader, decode_chunk_packet, decode_custom_payload_packet,
        decode_packet_body, encode_packet, write_varint,
    };
    use megu_client_mesh::RenderBackendPreference;
    use megu_client_mesh::tactile_effects::{FluidEntryBody, TactileParticleKind, TactilePreset};
    use megu_client_mesh::vanilla_plus_nature::{ActiveSeasonKind, NatureParticleKind};
    use megu_client_mesh::vfx_engine::{DynamicColoredLight, Vfx1Packet, VfxCommand};
    use megu_kernel::skeletal_anim::{AnimationTrigger, LoopMode, encode_animation_trigger};
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};

    fn session() -> (LiveSession, SyncSender<WorkerMessage>) {
        let (tx, incoming) = mpsc::sync_channel(8);
        let (outgoing, _rx) = mpsc::sync_channel(8);
        (LiveSession::with_channels(incoming, outgoing), tx)
    }

    #[test]
    fn packed_block_sprites_follow_model_directions_and_state_rotation() {
        use megu_client_mesh::model_baker::RawModelJson;

        let grass = RawModelJson::parse(r##"{
            "textures":{"particle":"dirt","top":"grass_block_top","bottom":"dirt","side":"grass_block_side"},
            "elements":[{"from":[0,0,0],"to":[16,16,16],"faces":{
              "down":{"texture":"#bottom","cullface":"down"},
              "up":{"texture":"#top","cullface":"up","tintindex":0},
              "north":{"texture":"#side","cullface":"north"},
              "south":{"texture":"#side","cullface":"south"},
              "west":{"texture":"#side","cullface":"west"},
              "east":{"texture":"#side","cullface":"east"}
            }}]
        }"##).unwrap();
        let log = RawModelJson::parse(
            r##"{
            "textures":{"end":"log_top","side":"log"},
            "elements":[{"from":[0,0,0],"to":[16,16,16],"faces":{
              "down":{"texture":"#end","cullface":"down"},
              "up":{"texture":"#end","cullface":"up"},
              "north":{"texture":"#side","cullface":"north"},
              "south":{"texture":"#side","cullface":"south"},
              "west":{"texture":"#side","cullface":"west"},
              "east":{"texture":"#side","cullface":"east"}
            }}]
        }"##,
        )
        .unwrap();
        let meta = CompiledAssetMeta {
            blockstates: BTreeMap::from([
                ("minecraft:grass_block".into(), BlockStateDef::parse(
                    r#"{"variants":{"snowy=false":{"model":"block/grass"}}}"#,
                ).unwrap()),
                ("minecraft:oak_log".into(), BlockStateDef::parse(
                    r#"{"variants":{"axis=x":{"model":"block/log","x":90,"y":90},"axis=y":{"model":"block/log"},"axis=z":{"model":"block/log","x":90}}}"#,
                ).unwrap()),
            ]),
            model_stack: ResourcePackModelStack::new().with_pack(BTreeMap::from([
                ("block/grass".into(), grass),
                ("block/log".into(), log),
            ])),
            ..CompiledAssetMeta::default()
        };
        let sprites = |name| block_face_sprites(&meta, name, |_| true).unwrap();
        let grass = sprites("minecraft:grass_block[snowy=false]");
        assert_eq!(
            grass.each_ref().map(Option::as_deref),
            [
                Some("dirt"),
                Some("grass_block_top"),
                Some("grass_block_side"),
                Some("grass_block_side"),
                Some("grass_block_side"),
                Some("grass_block_side"),
            ]
        );
        for (state, ends) in [
            ("minecraft:oak_log", [Direction::Down, Direction::Up]),
            (
                "minecraft:oak_log[axis=x]",
                [Direction::West, Direction::East],
            ),
            (
                "minecraft:oak_log[axis=z]",
                [Direction::North, Direction::South],
            ),
        ] {
            let faces = sprites(state);
            for direction in Direction::ALL {
                assert_eq!(
                    faces[direction as usize].as_deref(),
                    Some(if ends.contains(&direction) {
                        "log_top"
                    } else {
                        "log"
                    }),
                );
            }
        }
    }

    #[test]
    fn local_physics_interpolates_and_queues_tick_poses_after_server_teleport() {
        let (_tx, incoming) = mpsc::sync_channel(1);
        let (outgoing, rx) = mpsc::sync_channel(8);
        let mut live = LiveSession::with_channels(incoming, outgoing);
        live.status = SessionStatus::Playing;
        let teleport = |x| {
            ClientPlayEvent::Play(PlayEvent::Teleport(TeleportSync {
                x,
                y: 64.0,
                z: 0.0,
                yaw: 0.0,
                pitch: 0.0,
                flags: 0,
                teleport_id: 1,
            }))
        };
        live.apply_event(teleport(0.0)).unwrap();
        live.physics_state.vel[1] = -0.1;

        let mut frames = Vec::new();
        let mut packet_frames = Vec::new();
        for frame in 0..125 {
            let yaw = if frame < 6 { 180.0 } else { 180.0 + frame as f32 };
            let output = live
                .step_local_player_physics(0.016, 0.0, 1.0, false, true, false, yaw, -5.0)
                .unwrap();
            assert_eq!((output.pose.yaw, output.pose.pitch), (yaw, -5.0));
            frames.push(output);
            if let Ok(command) = rx.try_recv() {
                let WorkerCommand::Movement { movement, .. } = command else {
                    panic!("expected a movement command");
                };
                assert_eq!(movement.pose, live.pose.unwrap());
                assert_eq!(movement.strafe, 1.0);
                packet_frames.push(frame);
            }
            assert!(rx.try_recv().is_err());
        }
        assert_eq!(packet_frames.len(), 40);
        assert_eq!(&packet_frames[..3], &[3, 6, 9]);
        assert!(frames[4].pose.feet[0] > frames[3].pose.feet[0]);
        assert!(frames[3].pose.feet[1] > frames[4].pose.feet[1]);
        assert!(frames[4].pose.feet[1] > frames[5].pose.feet[1]);
        assert!(frames[3].eye_height > frames[4].eye_height);
        assert!(frames[4].eye_height > frames[5].eye_height);
        assert!(frames[5].eye_height > 1.27);

        live.apply_event(teleport(100.0)).unwrap();
        let snapped = live
            .step_local_player_physics(0.016, 0.0, 0.0, false, false, false, 160.0, -12.0)
            .unwrap();
        assert_eq!(snapped.pose.feet, [100.0, 64.0, 0.0]);
        assert_eq!(snapped.eye_height, 1.62);
        assert_eq!(snapped.walk_bob, (0.0, 0.0));
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn creative_flight_updates_every_frame_and_queues_at_twenty_hertz() {
        let (_tx, incoming) = mpsc::sync_channel(1);
        let (outgoing, rx) = mpsc::sync_channel(8);
        let mut live = LiveSession::with_channels(incoming, outgoing);
        live.status = SessionStatus::Playing;
        live.pose = Some(PlayerPose {
            feet: [0.0, 64.0, 0.0],
            yaw: 0.0,
            pitch: 0.0,
        });
        live.player_abilities.flags = 0x06;
        let mut packets = 0;
        let mut previous_x = 0.0;
        for _ in 0..125 {
            let output = live
                .step_local_player_physics(0.016, 0.0, 1.0, false, false, false, 180.0, 0.0)
                .unwrap();
            assert!(output.pose.feet[0] > previous_x);
            previous_x = output.pose.feet[0];
            packets += rx.try_iter().count();
        }
        assert_eq!(packets, 40);
        assert!((previous_x - 14.0).abs() < 1e-5);
    }

    #[test]
    fn workstation_actions_reach_the_protocol_worker_queue() {
        let (incoming_tx, incoming) = mpsc::sync_channel(8);
        let (outgoing, rx) = mpsc::sync_channel(8);
        let mut live = LiveSession::with_channels(incoming, outgoing);
        drop(incoming_tx);

        assert!(live.dispatch_workstation_action(
            &megu_ui::container_screen::WorkstationAction::RenameItem("Repairable tool".into())
        ));
        assert!(live.dispatch_workstation_action(
            &megu_ui::container_screen::WorkstationAction::SelectTrade(2)
        ));
        assert!(!live.select_trade(-1));
        assert!(live.dispatch_workstation_action(
            &megu_ui::container_screen::WorkstationAction::SetBeacon {
                primary: Some(5),
                secondary: Some(12),
            }
        ));
        assert_eq!(live.toggle_crafter_slot(4, 8), Some(true));
        assert_eq!(live.toggle_crafter_slot(4, 8), Some(false));
        assert_eq!(live.toggle_crafter_slot(4, 9), None);

        assert!(
            matches!(rx.recv().unwrap(), WorkerCommand::RenameItem(name) if name == "Repairable tool")
        );
        assert!(matches!(
            rx.recv().unwrap(),
            WorkerCommand::SelectTrade { index: 2 }
        ));
        assert!(matches!(
            rx.recv().unwrap(),
            WorkerCommand::SetBeacon {
                primary: Some(5),
                secondary: Some(12)
            }
        ));
        assert!(matches!(
            rx.recv().unwrap(),
            WorkerCommand::ContainerSlotStateChanged {
                slot_id: 8,
                window_id: 4,
                state: false
            }
        ));
        assert!(matches!(
            rx.recv().unwrap(),
            WorkerCommand::ContainerSlotStateChanged {
                slot_id: 8,
                window_id: 4,
                state: true
            }
        ));
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn world_light_sampling_falls_back_and_reads_loaded_chunk_lights() {
        let (mut live, _) = session();
        assert_eq!(live.sample_block_and_sky_light(-0.2, -64.0, -16.1), (0, 15));
        assert_eq!(live.sample_block_and_sky_light(0.0, f32::NAN, 0.0), (0, 15));
        assert_eq!(
            live.sample_block_and_sky_light(0.0, f32::INFINITY, 0.0),
            (0, 15)
        );
        let chunk = decode_chunk_packet(&decoded_frame(0x27, &stone_chunk()), 24, 64).unwrap();
        live.apply_event(ClientPlayEvent::Play(PlayEvent::ChunkData(Box::new(chunk))))
            .unwrap();
        assert_eq!(live.sample_block_and_sky_light(0.5, -0.5, 0.5), (0, 15));
    }

    #[test]
    fn subworld_custom_payload_channels_dispatch_inbound_and_outbound() {
        let (_incoming_tx, incoming) = mpsc::sync_channel(8);
        let (outgoing, outgoing_rx) = mpsc::sync_channel(8);
        let mut live = LiveSession::with_channels(incoming, outgoing);
        let mut sw6d = Vec::new();
        sw6d.extend_from_slice(b"SW6D");
        sw6d.push(1);
        sw6d.extend_from_slice(&1u16.to_be_bytes());
        sw6d.extend_from_slice(&77u64.to_be_bytes());
        for value in [4.0f64, 5.0, 6.0] {
            sw6d.extend_from_slice(&value.to_be_bytes());
        }
        for value in [0.0f32, 0.0, 0.0, 1.0] {
            sw6d.extend_from_slice(&value.to_be_bytes());
        }
        for _ in 0..6 {
            sw6d.extend_from_slice(&0.0f32.to_be_bytes());
        }
        sw6d.push(0);
        live.apply_event(framed(SUBWORLD_SYNC_CHANNEL, &sw6d))
            .unwrap();
        assert_eq!(
            live.subworlds.get(77).unwrap().transform.position,
            [4.0, 5.0, 6.0]
        );

        let cmd = SubWorldHelmCmd {
            subworld_id: 77,
            throttle: 0.25,
            ..Default::default()
        };
        assert!(live.send_subworld_helm(&cmd));
        let WorkerCommand::SendCustomPayload { channel, payload } = outgoing_rx.recv().unwrap()
        else {
            panic!("helm helper must enqueue a custom payload")
        };
        assert_eq!(channel, SUBWORLD_HELM_CHANNEL);
        assert_eq!(
            crate::subworld_sync::decode_helm_cmd(&payload).unwrap(),
            cmd
        );
        let wire = encode_custom_payload_body(&channel, &payload);
        assert!(wire.ends_with(&payload));
        assert_eq!(play_custom_payload_packet_id(767), 0x12);
        assert_eq!(play_custom_payload_packet_id(777), 0x16);
    }

    #[test]
    fn seamless_portal_channel_frames_queue_and_drain_bounded() {
        let (_incoming_tx, incoming) = mpsc::sync_channel(8);
        let (outgoing, _outgoing_rx) = mpsc::sync_channel(8);
        let mut live = LiveSession::with_channels(incoming, outgoing);
        assert!(live.drain_seamless_portal_frames().is_empty());

        live.apply_event(framed(SEAMLESS_PORTAL_CHANNEL, b"MSP1-frame-1"))
            .unwrap();
        live.apply_event(framed(SEAMLESS_PORTAL_CHANNEL, b"MSP1-frame-2"))
            .unwrap();
        // Unrelated custom channels must not enter the portal queue.
        live.apply_event(framed("megu:other_v1", b"ignored"))
            .unwrap();
        assert_eq!(
            live.drain_seamless_portal_frames(),
            vec![b"MSP1-frame-1".to_vec(), b"MSP1-frame-2".to_vec()]
        );
        assert!(live.drain_seamless_portal_frames().is_empty());

        // The queue is bounded: a burst keeps only the newest 16 frames.
        for index in 0..20u8 {
            live.apply_event(framed(SEAMLESS_PORTAL_CHANNEL, &[index]))
                .unwrap();
        }
        let frames = live.drain_seamless_portal_frames();
        assert_eq!(frames.len(), 16);
        assert_eq!(frames.first().unwrap(), &vec![4u8]);
        assert_eq!(frames.last().unwrap(), &vec![19u8]);
    }

    /// SW6D v1 builder for `(subworld_id, x, flags)` pose rows — the same
    /// layout `subworld_custom_payload_channels_dispatch_inbound_and_outbound`
    /// writes inline.
    fn sw6d_test_frame(poses: &[(u64, f64, u8)]) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"SW6D");
        bytes.push(1);
        bytes.extend_from_slice(&(poses.len() as u16).to_be_bytes());
        for &(id, x, flags) in poses {
            bytes.extend_from_slice(&id.to_be_bytes());
            for value in [x, 5.0, 6.0] {
                bytes.extend_from_slice(&value.to_be_bytes());
            }
            for value in [0.0f32, 0.0, 0.0, 1.0] {
                bytes.extend_from_slice(&value.to_be_bytes());
            }
            for _ in 0..6 {
                bytes.extend_from_slice(&0.0f32.to_be_bytes());
            }
            bytes.push(flags);
        }
        bytes
    }

    #[test]
    fn sw6d_frames_drain_into_render_bank_and_interpolate() {
        let (mut live, tx) = session();
        tx.send(WorkerMessage::Play(framed(
            SUBWORLD_SYNC_CHANNEL,
            &sw6d_test_frame(&[(77, 4.0, 0x01)]),
        )))
        .unwrap();
        tx.send(WorkerMessage::Play(framed(
            SUBWORLD_SYNC_CHANNEL,
            &sw6d_test_frame(&[(77, 10.0, 0x01)]),
        )))
        .unwrap();
        live.poll([0.0; 3], &[]);
        assert_eq!(live.player_subworld(), Some(77));
        // `poll` interpolates against the previous snapshot instead of
        // snapping the transform to the newest pose.
        let x = live.subworlds.get(77).unwrap().transform.position[0];
        assert!(x < 5.0, "interpolated transform must not snap to {x}");
    }

    #[test]
    fn player_subworld_binding_and_mainhand_item_helpers() {
        let (mut live, _tx) = session();
        // An unflagged pose registers in the bank but binds nobody.
        live.apply_event(framed(
            SUBWORLD_SYNC_CHANNEL,
            &sw6d_test_frame(&[(77, 4.0, 0)]),
        ))
        .unwrap();
        assert!(live.subworlds.get(77).is_some());
        assert_eq!(live.player_subworld(), None);
        // A player-aboard flagged pose binds; an empty snapshot releases.
        live.apply_event(framed(
            SUBWORLD_SYNC_CHANNEL,
            &sw6d_test_frame(&[(77, 4.0, 0x01)]),
        ))
        .unwrap();
        assert_eq!(live.player_subworld(), Some(77));
        live.apply_event(framed(SUBWORLD_SYNC_CHANNEL, &sw6d_test_frame(&[])))
            .unwrap();
        assert_eq!(live.player_subworld(), None);
        assert!(live.subworlds.get(77).is_none());

        live.apply_event(ClientPlayEvent::Play(PlayEvent::AddEntity {
            eid: 9,
            uuid: [1; 16],
            entity_type: 42,
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
        assert_eq!(live.entity_rig(9).unwrap().name, "minecraft:fox");
        assert_eq!(live.entity_mainhand_item_id(9), None);
        live.apply_event(ClientPlayEvent::Play(PlayEvent::SetEquipment {
            eid: 9,
            entries: vec![
                EquipmentEntry {
                    slot: 0,
                    item_wire: vec![1, 5, 0, 0],
                },
                EquipmentEntry {
                    slot: 4,
                    item_wire: vec![1, 7, 0, 0],
                },
            ],
        }))
        .unwrap();
        // Slot 0 only — the boots entry must not leak into the main hand.
        assert_eq!(live.entity_mainhand_item_id(9), Some(5));
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
    fn apply_event_records_stable_compact_block_update() {
        let (mut live, _) = session();
        live.apply_event(ClientPlayEvent::Play(PlayEvent::BlockUpdate {
            pos: [12, 64, -3],
            state_id: 0x1234,
        }))
        .unwrap();
        let frame = live
            .replay_ring
            .frames()
            .back()
            .expect("captured play event");
        assert_eq!(frame.packet_id, 0x4d52_0009);
        assert_eq!(frame.payload.len(), 16);
        assert_eq!(&frame.payload[0..4], &12i32.to_le_bytes());
        assert_eq!(&frame.payload[4..8], &64i32.to_le_bytes());
        assert_eq!(&frame.payload[8..12], &(-3i32).to_le_bytes());
        assert_eq!(&frame.payload[12..16], &0x1234u32.to_le_bytes());
    }

    #[test]
    fn configured_chat_history_limit_evicts_oldest_lines() {
        let (mut live, _) = session();
        live.chat_history_limit = 2;
        live.push_chat_line("one".to_owned());
        live.push_chat_line("two".to_owned());
        live.push_chat_line("three".to_owned());
        assert_eq!(live.hud.chat_lines, ["two", "three"]);
        assert_eq!(live.chat_log_keys.len(), live.hud.chat_lines.len());

        live.set_chat_history_limit(1);
        assert_eq!(live.chat_history_limit, 100, "configured minimum is 100");
        assert_eq!(live.chat_log_keys.len(), live.hud.chat_lines.len());

        let (mut live, _) = session();
        live.chat_history_limit = 150;
        for index in 0..125 {
            live.push_chat_line(index.to_string());
        }
        live.set_chat_history_limit(100);
        assert_eq!(live.hud.chat_lines.len(), 100);
        assert_eq!(live.hud.chat_lines.first().map(String::as_str), Some("25"));
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
        live.record_frame_and_refresh_compact_debug_hud(0.125, &mut ui);
        assert!((live.chunk_fades.fade_alpha(0, 0) - 0.5).abs() < 0.001);
        live.record_frame_and_refresh_compact_debug_hud(0.125, &mut ui);
        assert!(!live.chunk_fades.is_fading(0, 0));

        live.apply_event(ClientPlayEvent::Play(PlayEvent::UnloadChunk {
            cx: 0,
            cz: 0,
        }))
        .unwrap();
        assert_eq!(live.chunk_fades.len(), 0);
    }

    #[test]
    fn basic_and_advanced_compact_debug_hud_receive_frame_and_host_telemetry() {
        let (mut live, _) = session();
        let mut ui = UiState::new();
        ui.settings.qol.compact_debug_hud.enabled = false;
        live.record_frame_and_refresh_compact_debug_hud(0.02, &mut ui);
        assert_eq!(live.frame_timings.frame_count(), 1);
        assert_eq!(ui.hud.compact_debug_hud_data.fps, None);

        ui.settings.qol.cycle_compact_debug_hud();
        assert_eq!(
            ui.settings.qol.compact_debug_hud_preset(),
            CompactDebugHudPreset::Basic
        );
        live.record_frame_and_refresh_compact_debug_hud(0.02, &mut ui);
        assert!((ui.hud.compact_debug_hud_data.fps.unwrap() - 50.0).abs() < 0.01);
        assert!((ui.hud.compact_debug_hud_data.fps_one_percent_low.unwrap() - 50.0).abs() < 0.01);
        assert_eq!(ui.hud.compact_debug_hud_data.loaded_chunks, Some(0));

        ui.settings.qol.cycle_compact_debug_hud();
        assert_eq!(
            ui.settings.qol.compact_debug_hud_preset(),
            CompactDebugHudPreset::Advanced
        );
        live.record_frame_and_refresh_compact_debug_hud(0.04, &mut ui);
        assert_eq!(live.frame_timings.frame_count(), 3);
        assert!((live.frame_timings.frame_ms_avg() - 26.666_666).abs() < 0.001);
        assert!((ui.hud.compact_debug_hud_data.fps.unwrap() - 37.5).abs() < 0.01);
        assert!((ui.hud.compact_debug_hud_data.fps_one_percent_low.unwrap() - 25.0).abs() < 0.01);
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
        // Uploads are budget-deferred: the section becomes GPU-resident on
        // the next rendered frame's drain, not inside `update_live_terrain`.
        assert_eq!(renderer.pending_upload_count(), 1);
        assert_eq!(renderer.live_section_count(), 0);
        let rendered = renderer
            .render_offscreen_rgba(128, 128, &camera, None)
            .unwrap();
        assert_eq!(renderer.live_section_count(), 1);
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
            let mut renamed = false;
            let mut beacon_set = false;
            let mut trade_selected = false;
            let mut received_ids = Vec::new();
            for _ in 0..32 {
                let (packet_id, body) = read_loopback_frame(&mut socket);
                received_ids.push(packet_id);
                if packet_id == 0x18 {
                    assert_eq!(body, 29i64.to_be_bytes());
                    keep_alive = true;
                }
                if packet_id == 0x00 {
                    assert_eq!(WireReader::new(&body).read_varint().unwrap(), 3);
                    accepted_teleport = true;
                }
                if packet_id == 0x2a {
                    assert_eq!(body, vec![1, b'M']);
                    renamed = true;
                }
                if packet_id == 0x2e {
                    assert_eq!(body, vec![1, 5, 1, 12]);
                    beacon_set = true;
                }
                if packet_id == 0x2d {
                    assert_eq!(body, vec![2]);
                    trade_selected = true;
                }
                if keep_alive && accepted_teleport && renamed && beacon_set && trade_selected {
                    break;
                }
            }
            assert!(
                keep_alive && accepted_teleport && renamed && beacon_set && trade_selected,
                "server must receive play acknowledgments and all workstation packet bodies; got {received_ids:?}"
            );
            done_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        });

        let mut live =
            LiveSession::connect("127.0.0.1".into(), port, "Loopback".into(), "767".into());
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            live.poll([8.0, 20.0, 30.0], &[]);
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
        for action in [
            megu_ui::container_screen::WorkstationAction::RenameItem("M".to_owned()),
            megu_ui::container_screen::WorkstationAction::SetBeacon {
                primary: Some(5),
                secondary: Some(12),
            },
            megu_ui::container_screen::WorkstationAction::SelectTrade(2),
        ] {
            assert!(live.dispatch_workstation_action(&action));
        }
        done_tx.send(()).unwrap();
        server.join().unwrap();
        let mesh = live.world.packed_meshes().next().expect("TCP chunk mesh");
        let geometry = section_geometry(
            mesh.1,
            live.world.translucent_order(mesh.0).unwrap_or(&[]),
            [
                mesh.0 .0 as f32 * 16.0,
                mesh.0 .1 as f32 * 16.0,
                mesh.0 .2 as f32 * 16.0,
            ],
        );
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
        live.poll([0.0; 3], &[]);
        assert_eq!(live.status, SessionStatus::Playing);
        assert_eq!(live.hud.health, 14.0);
        assert_eq!(live.hud.hunger, 12);
        assert_eq!(live.hud.chat_lines.last().unwrap(), "server ready");
        tx.send(WorkerMessage::Closed("test disconnect".into()))
            .unwrap();
        live.poll([0.0; 3], &[]);
        assert_eq!(
            live.status,
            SessionStatus::Disconnected("test disconnect".into())
        );
    }

    #[test]
    fn kinetic_wire_parts_rotate_independently_feed_goggles_and_evict() {
        let (mut live, tx) = session();
        let mut payload = vec![2];
        for (x, rpm) in [(4, 60.0_f32), (5, -30.0_f32)] {
            for value in [0_i32, x, 64, 8] {
                payload.extend_from_slice(&value.to_be_bytes());
            }
            for value in [rpm, 100.0, 45.0] {
                payload.extend_from_slice(&value.to_be_bytes());
            }
            payload.push(u8::from(x == 5));
        }
        tx.send(WorkerMessage::Play(framed(
            CHANNEL_CREATE_KINETIC_SYNC,
            &payload,
        )))
        .unwrap();
        live.poll([0.0; 3], &[]);
        assert_eq!(live.kinetic_part_count(), 2);
        // No world block state under the parts: both register the generic
        // rotor template (axle + hub + marker blade = 3 boxes = 72 verts)
        // keyed `family 6 * 4 + axis Y slot 1`.
        let batch = live.subworlds.batches().next().unwrap();
        assert_eq!(batch.mesh_kind, 25);
        assert_eq!(batch.template_vertices.len(), 72);
        assert_eq!(live.kinetic_pos_to_id.get(&(0, (4, 64, 8))), Some(&0));
        assert_eq!(live.kinetic_pos_to_id.get(&(0, (5, 64, 8))), Some(&1));
        let first = live.block_inspector_telemetry_at((4, 64, 8)).unwrap();
        let second = live.block_inspector_telemetry_at((5, 64, 8)).unwrap();
        assert_eq!((first.rpm, second.rpm), (60.0, -30.0));
        assert_eq!(second.network_stress_used, 45.0);
        assert!(second.overstressed);

        live.sync_kinetic_live_parts(1.0);
        assert_eq!(live.subworlds.batches().next().unwrap().instances.len(), 2);
        assert!((live.kinetic_live_angles[&0] - std::f32::consts::TAU).abs() < 1e-5);
        assert!((live.kinetic_live_angles[&1] + std::f32::consts::PI).abs() < 1e-5);
        live.apply_event(ClientPlayEvent::Play(PlayEvent::BlockUpdate {
            pos: [4, 64, 8],
            state_id: 0,
        }))
        .unwrap();
        assert_eq!(live.kinetic_part_count(), 1);
        assert!(live.block_inspector_telemetry_at((4, 64, 8)).is_none());
        live.apply_event(ClientPlayEvent::Play(PlayEvent::UnloadChunk {
            cx: 0,
            cz: 0,
        }))
        .unwrap();
        assert_eq!(live.kinetic_part_count(), 0);
    }

    // The server starts Create's dynamic block states at the pinned vanilla count.
    const MRS1_WATER_WHEEL_STATE_ID: u32 = megu_kernel::STATE_COUNT;
    const MRS1_SHAFT_STATE_ID: u32 = MRS1_WATER_WHEEL_STATE_ID + 1;

    fn mrs1_frame() -> Vec<u8> {
        fn crc32(bytes: &[u8]) -> u32 {
            let mut crc = !0u32;
            for &byte in bytes {
                crc ^= u32::from(byte);
                for _ in 0..8 {
                    let mask = 0u32.wrapping_sub(crc & 1);
                    crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
                }
            }
            !crc
        }
        let mut frame = b"MRS1".to_vec();
        frame.extend_from_slice(&1u16.to_be_bytes());
        frame.extend_from_slice(&MRS1_WATER_WHEEL_STATE_ID.to_be_bytes()); // base_block_state_id
        frame.extend_from_slice(&9_000u32.to_be_bytes()); // base_item_id
        frame.extend_from_slice(&2u32.to_be_bytes()); // block state count
        for (id, name, axis) in [
            (MRS1_WATER_WHEEL_STATE_ID, "create:water_wheel", "x"),
            (MRS1_SHAFT_STATE_ID, "create:shaft", "y"),
        ] {
            frame.extend_from_slice(&id.to_be_bytes());
            frame.extend_from_slice(&(name.len() as u16).to_be_bytes());
            frame.extend_from_slice(name.as_bytes());
            frame.extend_from_slice(&1u16.to_be_bytes());
            for s in ["axis", axis] {
                frame.extend_from_slice(&(s.len() as u16).to_be_bytes());
                frame.extend_from_slice(s.as_bytes());
            }
        }
        frame.extend_from_slice(&1u32.to_be_bytes()); // item count
        frame.extend_from_slice(&9_000u32.to_be_bytes());
        let item = "create:wrench";
        frame.extend_from_slice(&(item.len() as u16).to_be_bytes());
        frame.extend_from_slice(item.as_bytes());
        let crc = crc32(&frame);
        frame.extend_from_slice(&crc.to_be_bytes());
        frame
    }

    fn load_mrs1_fixture(live: &mut LiveSession) {
        // Reserve the dynamic ids in the test world's palettes before chunk ingest.
        assert!(live.world.state_count <= MRS1_WATER_WHEEL_STATE_ID as usize);
        live.world.state_count = (MRS1_SHAFT_STATE_ID + 1) as usize;
        let body = decoded_frame(0x27, &stone_chunk());
        let chunk = decode_chunk_packet(&body, 24, 64).unwrap();
        live.apply_event(ClientPlayEvent::Play(PlayEvent::ChunkData(Box::new(chunk))))
            .unwrap();
        live.apply_event(framed(CHANNEL_MOD_REGISTRY_SYNC, &mrs1_frame()))
            .unwrap();
    }

    fn template_bounds(vertices: &[TerrainVertex]) -> ([f32; 3], [f32; 3]) {
        let mut min = [f32::INFINITY; 3];
        let mut max = [f32::NEG_INFINITY; 3];
        for vertex in vertices {
            for axis in 0..3 {
                min[axis] = min[axis].min(vertex.position[axis]);
                max[axis] = max[axis].max(vertex.position[axis]);
            }
        }
        (min, max)
    }

    #[test]
    fn kinetic_templates_are_family_and_axis_specific() {
        let (shaft_vertices, shaft_indices) =
            kinetic_template(KineticMeshFamily::Shaft, KineticAxis::Y);
        // 4×16×4 axle: one box, a full-block tall rod ¼-block wide.
        assert_eq!(shaft_vertices.len(), 24);
        assert_eq!(shaft_indices.len(), 36);
        let (min, max) = template_bounds(&shaft_vertices);
        assert!((max[1] - min[1] - 1.0).abs() < 1e-6);
        assert!((max[0] - min[0] - 0.25).abs() < 1e-6);
        assert!((max[2] - min[2] - 0.25).abs() < 1e-6);

        // The same shaft reoriented onto X keeps its long extent along X.
        let (shaft_x, _) = kinetic_template(KineticMeshFamily::Shaft, KineticAxis::X);
        let (min, max) = template_bounds(&shaft_x);
        assert!((max[0] - min[0] - 1.0).abs() < 1e-6);
        assert!((max[1] - min[1] - 0.25).abs() < 1e-6);

        // Cogwheel: axle + 8 rim segments + 8 teeth + 4 spokes.
        let (cog_vertices, _) = kinetic_template(KineticMeshFamily::Cogwheel, KineticAxis::Y);
        assert_eq!(cog_vertices.len(), (1 + 8 + 8 + 4) * 24);
        // Large cogwheel spins a wider ring: 12 rim + 12 tooth + 4 spoke.
        let (large_vertices, _) =
            kinetic_template(KineticMeshFamily::LargeCogwheel, KineticAxis::Y);
        assert_eq!(large_vertices.len(), (1 + 12 + 12 + 4) * 24);
        let (_, cog_max) = template_bounds(&cog_vertices);
        let (_, large_max) = template_bounds(&large_vertices);
        assert!(large_max[0] > cog_max[0]);

        // Water wheels: axle + hub + 8 paddles; the large reaches wider.
        let (wheel_vertices, _) = kinetic_template(KineticMeshFamily::WaterWheel, KineticAxis::Z);
        assert_eq!(wheel_vertices.len(), (1 + 1 + 8) * 24);
        let (_, wheel_max) = template_bounds(&wheel_vertices);
        // Z orientation: the axle runs along Z while the paddles radiate
        // across the wheel's XY plane (reach ~7.5px ≈ 0.47 blocks).
        assert!(wheel_max[0] > 0.4 && wheel_max[1] > 0.4);
        let (big_wheel, _) = kinetic_template(KineticMeshFamily::LargeWaterWheel, KineticAxis::Z);
        let (_, big_max) = template_bounds(&big_wheel);
        assert!(big_max[0] > wheel_max[0]);

        // Press: axle + piston shaft + head plate; the plate sits at the
        // block's bottom face and the family maps cleanly by name.
        let (press, _) = kinetic_template(KineticMeshFamily::MechanicalPress, KineticAxis::Y);
        assert_eq!(press.len(), 3 * 24);
        let (press_min, _) = template_bounds(&press);
        assert!((press_min[1] + 0.5).abs() < 1e-6);
        assert_eq!(
            KineticMeshFamily::from_block_name("create:large_cogwheel[axis=x]"),
            KineticMeshFamily::LargeCogwheel
        );
        assert_eq!(
            KineticMeshFamily::from_block_name("create:basin"),
            KineticMeshFamily::GenericRotor
        );
        // Unknown addon parts get the clean shaft+hub rotor: 3 boxes,
        // pivot-centered so the part spins instead of orbiting a corner.
        let (rotor, _) = kinetic_template(KineticMeshFamily::GenericRotor, KineticAxis::Y);
        assert_eq!(rotor.len(), 3 * 24);
        let (rotor_min, rotor_max) = template_bounds(&rotor);
        for axis in 0..3 {
            assert!(rotor_min[axis] < 0.0 && rotor_max[axis] > 0.0);
        }
    }

    #[test]
    fn mrs1_frame_updates_registry_and_counts_rejects() {
        let (mut live, _tx) = session();
        assert!(live.mod_registry.is_none());
        live.apply_event(framed(CHANNEL_MOD_REGISTRY_SYNC, &mrs1_frame()))
            .unwrap();
        let registry = live.mod_registry.as_ref().unwrap();
        assert_eq!(registry.base_block_state_id, MRS1_WATER_WHEEL_STATE_ID);
        assert_eq!(
            registry.block_state_name(MRS1_WATER_WHEEL_STATE_ID),
            Some("create:water_wheel")
        );
        assert_eq!(registry.item_name(9_000), Some("create:wrench"));

        let mut bad = mrs1_frame();
        let last = bad.len() - 1;
        bad[last] ^= 0xFF;
        live.apply_event(framed(CHANNEL_MOD_REGISTRY_SYNC, &bad))
            .unwrap();
        assert_eq!(live.mod_registry_rejected, 1);
        // The rejected frame never clobbers the stored registry.
        assert_eq!(
            live.mod_registry
                .as_ref()
                .unwrap()
                .block_state_name(MRS1_WATER_WHEEL_STATE_ID),
            Some("create:water_wheel")
        );
    }

    #[test]
    fn modded_open_screen_uses_generic_fallback_geometry() {
        let (mut live, _tx) = session();
        // `menu_type` 42 is outside Vanilla's 0..=24 registry table.
        live.apply_event(ClientPlayEvent::Play(PlayEvent::OpenScreen {
            window_id: 9,
            menu_type: 42,
            title_nbt: Vec::new(),
        }))
        .unwrap();
        let screen = live.open_screen.as_ref().expect("modded screen opens");
        assert_eq!(screen.menu_type, 42);
        assert_eq!(screen.content_slot_count, None);
        let schema = megu_ui::container_screen::menu_schema_or_generic(
            screen.menu_type,
            screen.content_slot_count,
        )
        .expect("modded menu has a generic fallback");
        assert_eq!(schema.menu_id, 2, "generic 9x3 until content arrives");

        // The server's `container_set_content` count sizes the fallback grid:
        // 54 container slots + the conventional 36 player slots.
        let mut payload = vec![0u8; 90];
        payload.push(0); // carried
        live.apply_event(ClientPlayEvent::Play(PlayEvent::ContainerSetContent {
            window_id: 9,
            state_id: 1,
            slot_count: 90,
            raw_payload: payload,
        }))
        .unwrap();
        let screen = live.open_screen.as_ref().unwrap();
        assert_eq!(screen.content_slot_count, Some(90));
        let schema = megu_ui::container_screen::menu_schema_or_generic(
            screen.menu_type,
            screen.content_slot_count,
        )
        .unwrap();
        assert_eq!(schema.menu_id, 5, "9x6 fallback for a 54-slot container");
        assert_eq!(schema.slot_count, 90);
    }

    #[test]
    fn generic_mod_channels_reach_the_custom_payload_inbox() {
        let (mut live, _tx) = session();
        live.ingest_custom_channel_payload_for_test("mymod:sync", &[1, 2, 3]);
        assert_eq!(live.custom_payload_inbox.queued_len(), 1);
        assert_eq!(
            live.custom_payload_inbox.entries()[0].channel,
            "mymod:sync"
        );
        assert_eq!(live.custom_payload_inbox.entries()[0].payload, vec![1, 2, 3]);
        // `minecraft:register` is tracked as channel negotiation, not queued.
        live.ingest_custom_channel_payload_for_test("minecraft:register", b"mymod:sync\0");
        assert!(live.custom_payload_inbox.is_registered("mymod:sync"));
        assert_eq!(live.custom_payload_inbox.queued_len(), 1);
        // A malformed channel is counted, never queued or panicked on.
        live.ingest_custom_channel_payload_for_test("mymod", &[0]);
        assert_eq!(
            live.custom_payload_inbox.stats().rejected_malformed_channel,
            1
        );
    }

    #[test]
    fn dynamic_blocks_drive_family_templates_and_goggles_names() {
        let (mut live, _tx) = session();
        load_mrs1_fixture(&mut live);
        // Dynamic mod block states: water wheel on X, shaft on Y.
        for (pos, state_id) in [
            ([7, 64, 3], MRS1_WATER_WHEEL_STATE_ID),
            ([8, 64, 3], MRS1_SHAFT_STATE_ID),
        ] {
            live.apply_event(ClientPlayEvent::Play(PlayEvent::BlockUpdate {
                pos,
                state_id,
            }))
            .unwrap();
        }
        let mut payload = vec![2];
        for (x, rpm) in [(7, 32.0_f32), (8, 16.0)] {
            for value in [0_i32, x, 64, 3] {
                payload.extend_from_slice(&value.to_be_bytes());
            }
            for value in [rpm, 100.0, 20.0] {
                payload.extend_from_slice(&value.to_be_bytes());
            }
            payload.push(0);
        }
        live.apply_event(framed(CHANNEL_CREATE_KINETIC_SYNC, &payload))
            .unwrap();

        // The block name picks the family and the MRS1 `axis` property
        // the template orientation: water wheel on X is kind 3*4+0=12,
        // shaft on Y is kind 0*4+1=1.
        let kinds: BTreeMap<u32, usize> = live
            .subworlds
            .batches()
            .map(|batch| (batch.mesh_kind, batch.template_vertices.len()))
            .collect();
        assert_eq!(kinds.get(&12), Some(&240));
        assert_eq!(kinds.get(&1), Some(&24));
        let mut specs: Vec<_> = live.kinetic_parts.values().collect();
        specs.sort_by_key(|spec| spec.mesh_kind);
        assert_eq!(specs[0].axis, [0.0, 1.0, 0.0]);
        assert_eq!(specs[0].position, [8.5, 64.5, 3.5]);
        assert_eq!(specs[1].axis, [1.0, 0.0, 0.0]);
        assert_eq!(specs[1].position, [7.5, 64.5, 3.5]);

        // Goggles resolve the real mod block name off the dynamic id.
        let telemetry = live.block_inspector_telemetry_at((7, 64, 3)).unwrap();
        assert_eq!(telemetry.block_id, "create:water_wheel");
        assert_eq!(telemetry.rpm, 32.0);
    }

    fn write_test_jar(path: &std::path::Path, entries: &[(&str, &[u8])]) {
        use std::io::{Seek, Write};
        let mut file = std::fs::File::create(path).expect("create jar");
        let mut central = Vec::new();
        for (name, data) in entries {
            let offset = file.stream_position().expect("position") as u32;
            let name_bytes = name.as_bytes();
            let crc = megu_core::hash::crc32_ieee(data);
            let mut local = Vec::new();
            local.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
            local.extend_from_slice(&20u16.to_le_bytes());
            local.extend_from_slice(&0u16.to_le_bytes());
            local.extend_from_slice(&0u16.to_le_bytes());
            local.extend_from_slice(&[0; 4]);
            local.extend_from_slice(&crc.to_le_bytes());
            local.extend_from_slice(&(data.len() as u32).to_le_bytes());
            local.extend_from_slice(&(data.len() as u32).to_le_bytes());
            local.extend_from_slice(&(name_bytes.len() as u16).to_le_bytes());
            local.extend_from_slice(&0u16.to_le_bytes());
            local.extend_from_slice(name_bytes);
            local.extend_from_slice(data);
            file.write_all(&local).expect("write entry");
            central.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
            central.extend_from_slice(&20u16.to_le_bytes());
            central.extend_from_slice(&20u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&[0; 4]);
            central.extend_from_slice(&crc.to_le_bytes());
            central.extend_from_slice(&(data.len() as u32).to_le_bytes());
            central.extend_from_slice(&(data.len() as u32).to_le_bytes());
            central.extend_from_slice(&(name_bytes.len() as u16).to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u32.to_le_bytes());
            central.extend_from_slice(&offset.to_le_bytes());
            central.extend_from_slice(name_bytes);
        }
        let cd_offset = file.stream_position().expect("position") as u32;
        file.write_all(&central).expect("write directory");
        let count = entries.len() as u16;
        let mut end = Vec::new();
        end.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
        end.extend_from_slice(&0u16.to_le_bytes());
        end.extend_from_slice(&0u16.to_le_bytes());
        end.extend_from_slice(&count.to_le_bytes());
        end.extend_from_slice(&count.to_le_bytes());
        end.extend_from_slice(&(central.len() as u32).to_le_bytes());
        end.extend_from_slice(&cd_offset.to_le_bytes());
        end.extend_from_slice(&0u16.to_le_bytes());
        file.write_all(&end).expect("write end record");
    }

    #[test]
    fn goggles_display_name_uses_loaded_mod_lang_table() {
        let dir =
            std::env::temp_dir().join(format!("megu-live-mrs1-goggles-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("mods")).expect("create mod dir");
        write_test_jar(
            &dir.join("mods/00-create.jar"),
            &[
                (
                    "assets/create/lang/en_us.json",
                    br#"{"block.create.water_wheel":"Water Wheel"}"#,
                ),
                (
                    "assets/create/lang/ja_jp.json",
                    "{\"block.create.water_wheel\":\"水車\"}".as_bytes(),
                ),
            ],
        );
        let mut live = LiveSession::offline_with_paths(SessionPaths {
            game_dir: Some(dir.clone()),
            ..SessionPaths::default()
        });
        live.set_ui_locale("ja-JP");
        load_mrs1_fixture(&mut live);
        live.apply_event(ClientPlayEvent::Play(PlayEvent::BlockUpdate {
            pos: [7, 64, 3],
            state_id: MRS1_WATER_WHEEL_STATE_ID,
        }))
        .unwrap();
        let mut payload = vec![1];
        for value in [0_i32, 7, 64, 3] {
            payload.extend_from_slice(&value.to_be_bytes());
        }
        for value in [64.0_f32, 100.0, 20.0] {
            payload.extend_from_slice(&value.to_be_bytes());
        }
        payload.push(0);
        live.apply_event(framed(CHANNEL_CREATE_KINETIC_SYNC, &payload))
            .unwrap();

        let telemetry = live.block_inspector_telemetry_at((7, 64, 3)).unwrap();
        assert_eq!(telemetry.block_id, "create:water_wheel");
        assert_eq!(telemetry.display_name.as_deref(), Some("水車"));
        // The Japanese table sticks to the resolved locale; switching the
        // UI locale back marks it stale so the next lookup reloads.
        live.set_ui_locale("en_us");
        let telemetry = live.block_inspector_telemetry_at((7, 64, 3)).unwrap();
        assert_eq!(telemetry.display_name.as_deref(), Some("Water Wheel"));
        std::fs::remove_dir_all(&dir).expect("remove temp dir");
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
        live.poll([8.0, 8.0, 24.0], &[]);
        assert_eq!(live.world.loaded_chunks(), 1);
        let mesh = live
            .world
            .packed_meshes()
            .next()
            .expect("solid section mesh");
        let geometry = section_geometry(
            mesh.1,
            live.world.translucent_order(mesh.0).unwrap_or(&[]),
            [
                mesh.0 .0 as f32 * 16.0,
                mesh.0 .1 as f32 * 16.0,
                mesh.0 .2 as f32 * 16.0,
            ],
        );
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
            dimension: DimensionMeta {
                name: "minecraft:the_nether".to_string(),
                min_y: 0,
                height: 256,
                has_skylight: false,
            },
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
        assert_eq!(
            live.scoreboard.display_slots.get(&1).map(String::as_str),
            Some("kills")
        );

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
            // Anonymous empty string name, zero progress/style, and no flags.
            raw_payload: vec![8, 0, 0, 0, 0, 0, 0, 0, 0, 0],
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

    /// Chat, roster, boss bars, inventory and held-slot packets must all
    /// land in the HUD channels the window copies each frame — the P0
    /// "half-wired" gap the vertical audit counts.
    #[test]
    fn play_events_feed_hud_chat_players_bossbars_and_inventory() {
        let (mut live, _tx) = session();
        let uuid = [9u8; 16];

        // Tab roster -> hud.players.
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
        assert_eq!(live.hud.players, vec![("Alex".to_string(), 17u16)]);

        // PlayerChat renders `<sender> text`; DisguisedChat verbatim.
        live.apply_event(ClientPlayEvent::Play(PlayEvent::PlayerChat {
            sender: uuid,
            index: 0,
            signature: None,
            text: "hello".into(),
            nbt_bytes: Vec::new(),
        }))
        .unwrap();
        live.apply_event(ClientPlayEvent::Play(PlayEvent::DisguisedChat {
            text: "<Alex> disguised".into(),
            nbt_bytes: Vec::new(),
        }))
        .unwrap();
        assert!(
            live.hud.chat_lines.iter().any(|l| l == "<Alex> hello"),
            "player chat must name the roster sender: {:?}",
            live.hud.chat_lines
        );
        assert!(live.hud.chat_lines.iter().any(|l| l == "<Alex> disguised"));

        // Boss bar add: title NBT + f32 health + color + division + flags.
        let mut boss_payload = death_message_nbt("Boss");
        boss_payload.extend_from_slice(&0.5f32.to_be_bytes());
        boss_payload.extend_from_slice(&[0, 0, 0]);
        live.apply_event(ClientPlayEvent::Play(PlayEvent::BossEvent {
            uuid,
            action: 0,
            raw_payload: boss_payload,
        }))
        .unwrap();
        assert_eq!(
            live.hud.bossbars,
            vec![("Boss".to_string(), 0.5)],
            "boss bar title/health must reach the HUD rows"
        );
        live.apply_event(ClientPlayEvent::Play(PlayEvent::BossEvent {
            uuid,
            action: 2,
            raw_payload: 0.25f32.to_be_bytes().to_vec(),
        }))
        .unwrap();
        assert_eq!(live.hud.bossbars[0].1, 0.25);

        // Window-0 inventory: hotbar slot 36 = torch (291), offhand slot
        // 45 = shield (1162), chest slot 6 = diamond chestplate (869).
        let stack = |count: u8, id: i32| {
            let mut v = vec![count];
            let mut id_bytes = Vec::new();
            write_varint(&mut id_bytes, id);
            v.extend_from_slice(&id_bytes);
            v.extend_from_slice(&[0, 0]);
            v
        };
        let mut slots = Vec::new();
        for index in 0..46 {
            match index {
                6 => slots.extend(stack(1, 869)),
                36 => slots.extend(stack(3, 291)),
                45 => slots.extend(stack(1, 1162)),
                _ => slots.push(0),
            }
        }
        slots.push(0); // carried
        live.apply_event(ClientPlayEvent::Play(PlayEvent::ContainerSetContent {
            window_id: 0,
            state_id: 7,
            slot_count: 46,
            raw_payload: slots,
        }))
        .unwrap();
        assert_eq!(
            live.hud.hotbar[0].as_deref(),
            Some("minecraft:torch"),
            "hotbar slot 36 must feed hud.hotbar[0]: {:?}",
            live.hud.hotbar
        );
        assert_eq!(live.hud.offhand.as_deref(), Some("minecraft:shield"));
        assert_eq!(live.hud.armor, 8, "diamond chestplate = 8 defense");

        // Server-forced held slot -> hud.selected_slot.
        live.apply_event(ClientPlayEvent::Play(PlayEvent::SetCarriedItem(3)))
            .unwrap();
        assert_eq!(live.hud.selected_slot, 3);

        // The local player's index-1 air + index-14 absorption metadata
        // drive the HUD bubble/heart rows.
        live.apply_event(ClientPlayEvent::Play(PlayEvent::JoinGame {
            entity_id: 11,
            dimension: DimensionMeta::default(),
            hardcore: false,
        }))
        .unwrap();
        let mut meta = vec![0x00, 0x00, 0x02]; // index 0 byte: sneaking flag
        meta.extend_from_slice(&[0x01, 0x01]); // index 1 varint air
        meta.extend_from_slice(&[0xAC, 0x02]); // varint 300
        meta.push(0x0E); // index 14 float absorption
        meta.push(0x03);
        meta.extend_from_slice(&6.0f32.to_be_bytes());
        meta.push(0xFF);
        live.apply_event(ClientPlayEvent::Play(PlayEvent::SetEntityData {
            eid: 11,
            raw_entries: meta,
        }))
        .unwrap();
        assert_eq!(live.hud.air, 300);
        assert_eq!(live.hud.absorption, 6.0);
        let stored = &live.entity_metadata[&11];
        assert_eq!(stored.flags, Some(0x02));
        assert_eq!(stored.air, Some(300));
        assert_eq!(stored.absorption, Some(6.0));

        // Mob-effect add/remove lands in the per-entity effect map.
        live.apply_event(ClientPlayEvent::Play(PlayEvent::UpdateMobEffect {
            eid: 11,
            effect_id: 10,
            amplifier: 1,
            duration: 200,
            flags: 0x02,
        }))
        .unwrap();
        assert_eq!(live.mob_effects[&11][&10].duration, 200);
        live.apply_event(ClientPlayEvent::Play(PlayEvent::RemoveMobEffect {
            eid: 11,
            effect_id: 10,
        }))
        .unwrap();
        assert!(live.mob_effects[&11].is_empty());
    }

    #[test]
    fn held_map_snapshot_resolves_inventory_component_and_live_patches() {
        let (mut live, _tx) = session();
        let patch = |map_id, color| {
            ClientPlayEvent::Play(PlayEvent::MapItemData {
                map_id,
                scale: 2,
                locked: false,
                icons: None,
                columns: 1,
                rows: 1,
                x: 0,
                y: 0,
                colors: vec![color],
            })
        };
        live.apply_event(patch(19, 18)).unwrap();
        assert!(live.hud.held_map.is_none());
        let id = megu_proto::ProtocolTable::v767_builtin()
            .registry_id("minecraft:item", "minecraft:filled_map")
            .unwrap();
        let mut stack = vec![1];
        write_varint(&mut stack, id as i32);
        stack.extend([1, 0, 26, 19]); // added=1, removed=0, map_id=19
        live.apply_event(ClientPlayEvent::Play(PlayEvent::ContainerSetSlot {
            window_id: 0,
            state_id: 1,
            slot: 36,
            raw_item: stack,
        }))
        .unwrap();
        let first = live.hud.held_map.clone().unwrap();
        assert_eq!(first.id, 19);
        assert_eq!(first.pixels[0], [255, 0, 0, 255]);
        live.apply_event(patch(21, 6)).unwrap();
        assert!(Arc::ptr_eq(live.hud.held_map.as_ref().unwrap(), &first));
        live.apply_event(patch(19, 6)).unwrap();
        assert_eq!(
            live.hud.held_map.as_ref().unwrap().pixels[0],
            [127, 178, 56, 255]
        );
        live.apply_event(ClientPlayEvent::Play(PlayEvent::SetCarriedItem(1)))
            .unwrap();
        assert!(live.hud.held_map.is_none());
    }

    #[test]
    fn live_tab_team_and_combat_packets_update_the_hud_and_death_fallback() {
        let (_tx, incoming) = mpsc::sync_channel(8);
        let (outgoing, _rx) = mpsc::sync_channel(8);
        let mut live = LiveSession::with_channels(incoming, outgoing);
        live.status = SessionStatus::Playing;
        let mut apply = |event| live.apply_event(ClientPlayEvent::Play(event)).unwrap();
        apply(PlayEvent::JoinGame {
            entity_id: 11,
            dimension: DimensionMeta::default(),
            hardcore: false,
        });
        apply(PlayEvent::TabList {
            header_nbt: death_message_nbt("Lobby"),
            footer_nbt: death_message_nbt("Server 1"),
        });
        apply(PlayEvent::PlayerInfo {
            actions: 0x19,
            entries: vec![PlayerInfoUpdateEntry {
                uuid: [88; 16],
                name: Some("Alex".into()),
                properties: vec![],
                chat_session: None,
                game_mode: None,
                listed: Some(true),
                latency: Some(40),
                display_name_nbt: None,
            }],
        });
        apply(PlayEvent::SetPlayerTeam {
            name: "red".into(),
            mode: 0,
            info: Some(Box::new(megu_client::protocol::PlayerTeamInfo {
                display_name_nbt: death_message_nbt("Red"),
                friendly_flags: 0,
                nametag_visibility: "always".into(),
                collision_rule: "never".into(),
                color: 12,
                prefix_nbt: death_message_nbt("["),
                suffix_nbt: death_message_nbt("]"),
            })),
            entities: vec!["Alex".into()],
        });
        apply(PlayEvent::PlayerCombatEnter);
        assert_eq!(live.hud.tab_header, "Lobby");
        assert_eq!(live.hud.tab_footer, "Server 1");
        assert_eq!(live.hud.players, vec![("[Alex]".into(), 40)]);
        assert_eq!(live.hud.player_colors, vec![Some([255, 85, 85])]);
        add_entity(&mut live, 88, 128, [0.0, 64.0, 8.0]);
        assert_eq!(
            live.entity_nametag(88, "Steve")
                .map(|(text, color)| (text.into_owned(), color)),
            Some(("[Alex]".into(), Some([255, 85, 85])))
        );
        assert!(live.hud.combat.in_combat);
        live.last_vfx_tick = Instant::now() - Duration::from_millis(150);
        live.poll([0.0; 3], &[]);
        assert!(live.hud.combat.ticks >= 3);
        let mut apply = |event| live.apply_event(ClientPlayEvent::Play(event)).unwrap();
        apply(PlayEvent::DamageEvent {
            eid: 11,
            source_type: 0,
            source_cause: 89,
            source_direct: 89,
            source_position: None,
        });
        apply(PlayEvent::PlayerCombatEnd { duration: 42 });
        apply(PlayEvent::SetHealth {
            health: 0.0,
            food: 20,
            saturation: 0.0,
        });
        assert!(!live.hud.combat.in_combat);
        assert_eq!(live.hud.combat.ticks, 42);
        assert_eq!(live.hud.combat.last_attacker_id, Some(88));
        assert_eq!(
            live.death_screen_state().unwrap().cause_message.as_deref(),
            Some("Last attacker: Alex")
        );
        live.apply_event(ClientPlayEvent::Play(PlayEvent::Respawn {
            dimension: DimensionMeta::default(),
        }))
        .unwrap();
        assert_eq!(live.hud.combat, HudCombat::default());
        assert_eq!(
            live.hud.tab_header, "Lobby",
            "tab list persists across dimension changes"
        );
    }

    /// Vanilla `MultiPlayerGameMode.performUseItemOn`: sneaking with a stack
    /// in either hand suppresses the block's own interaction, so only the
    /// face-adjacent cell is predicted; an empty-handed sneak still predicts
    /// the clicked cell (chest/door use is not skipped).
    #[test]
    fn sneak_with_an_item_skips_the_block_use_prediction() {
        let (_tx, incoming) = mpsc::sync_channel(8);
        let (outgoing, _rx) = mpsc::sync_channel(16);
        let mut live = LiveSession::with_channels(incoming, outgoing);
        live.status = SessionStatus::Playing;
        live.hud.hotbar = std::array::from_fn(|_| None);
        live.hud.offhand = None;
        live.hud.selected_slot = 0;

        // Empty hands: `isHandsEmpty()` keeps `skip_block_use` false even
        // while sneaking, so the clicked cell stays predicted.
        live.sneaking = true;
        assert!(live.hands_empty());
        assert!(!live.skips_block_use());
        live.use_item_on(LiveSession::MAIN_HAND, [10, 64, -3], 3, [0.5, 0.0, 0.5], false);
        let sequence = live.use_item_sequence;
        assert_eq!(
            live.pending_interaction_seqs.get(&sequence),
            Some(&vec![[10, 64, -3], [10, 64, -2]]),
            "empty-hand sneak keeps the block interaction prediction"
        );

        // Main hand holding a block: sneak-place skips the block use.
        live.hud.hotbar[0] = Some("minecraft:stone".to_owned());
        assert!(!live.hands_empty());
        assert!(live.skips_block_use());
        live.use_item_on(LiveSession::MAIN_HAND, [10, 64, -3], 3, [0.5, 0.0, 0.5], false);
        let sequence = live.use_item_sequence;
        assert_eq!(
            live.pending_interaction_seqs.get(&sequence),
            Some(&vec![[10, 64, -2]]),
            "sneak-place predicts only the item's face-adjacent cell"
        );

        // Not sneaking: the block interaction runs again.
        live.sneaking = false;
        assert!(!live.skips_block_use());
        live.use_item_on(LiveSession::MAIN_HAND, [10, 64, -3], 3, [0.5, 0.0, 0.5], false);
        let sequence = live.use_item_sequence;
        assert_eq!(
            live.pending_interaction_seqs.get(&sequence),
            Some(&vec![[10, 64, -3], [10, 64, -2]])
        );

        // Off hand only: the `[0, 1]` fallback resolves hand 1, and the
        // sneak rule already covers either hand.
        live.hud.hotbar[0] = None;
        live.hud.offhand = Some("minecraft:torch".to_owned());
        assert_eq!(live.use_hand(), LiveSession::OFF_HAND);
        live.sneaking = true;
        assert!(live.skips_block_use());
    }

    /// Every gameplay `LiveSession` producer must enqueue its
    /// `WorkerCommand` — the window's dig/place/attack/hotbar/sneak
    /// dispatch rides on these.
    #[test]
    fn gameplay_producers_enqueue_worker_commands() {
        let (_tx, incoming) = mpsc::sync_channel(8);
        let (outgoing, rx) = mpsc::sync_channel(16);
        let mut live = LiveSession::with_channels(incoming, outgoing);
        live.status = SessionStatus::Playing;
        live.apply_event(ClientPlayEvent::Play(PlayEvent::JoinGame {
            entity_id: 11,
            dimension: DimensionMeta::default(),
            hardcore: false,
        }))
        .unwrap();
        // JoinGame -> reset_dimension clears the pose; re-set after join.
        live.pose = Some(PlayerPose {
            feet: [1.0, 64.0, 1.0],
            yaw: 90.0,
            pitch: -10.0,
        });
        live.sneaking = true;

        live.attack_entity(42);
        live.interact_entity(42, Some([1.0, 2.0, 3.0]), LiveSession::MAIN_HAND);
        live.dig_start([10, 64, -3], 1);
        live.dig_abort([10, 64, -3], 1);
        live.dig_finish([10, 64, -3], 1);
        live.set_carried_item(4);
        live.player_command(0); // start sneak
        live.player_command(3); // start sprint
        live.use_item(0);
        live.use_item_on(0, [10, 64, -3], 1, [0.5, 0.0, 0.5], false);
        live.swing(0);

        let mut commands = Vec::new();
        while let Ok(cmd) = rx.try_recv() {
            commands.push(cmd);
        }
        assert!(
            commands.iter().any(|c| matches!(
                c,
                WorkerCommand::Interact {
                    target: 42,
                    action: 1,
                    sneaking: true,
                    ..
                }
            )),
            "attack must enqueue Interact action 1"
        );
        assert!(commands.iter().any(|c| matches!(
            c,
            WorkerCommand::Interact {
                target: 42,
                action: 2,
                at: Some(_),
                ..
            }
        )));
        for status in [0, 1, 2] {
            assert!(
                commands.iter().any(|c| matches!(
                    c,
                    WorkerCommand::PlayerAction { status: s, .. } if *s == status
                )),
                "dig status {status} missing"
            );
        }
        assert!(
            commands
            .iter()
                .any(|c| matches!(c, WorkerCommand::SetCarriedItem { slot: 4 }))
        );
        for action in [0, 3] {
            assert!(commands.iter().any(|c| matches!(
                c,
                WorkerCommand::PlayerCommand {
                    entity_id: 11,
                    action: a,
                    ..
                } if *a == action
            )));
        }
        assert!(
            commands
            .iter()
                .any(|c| matches!(c, WorkerCommand::UseItem { .. }))
        );
        assert!(
            commands
            .iter()
                .any(|c| matches!(c, WorkerCommand::UseItemOn { .. }))
        );
        assert!(
            commands
            .iter()
                .any(|c| matches!(c, WorkerCommand::Swing { hand: 0 }))
        );

        // Movement carries the sneak bit through to `player_input`.
        live.send_movement(PlayerMovement {
            pose: PlayerPose {
                feet: [1.0, 64.0, 2.0],
                yaw: 0.0,
                pitch: 0.0,
            },
            strafe: 0.0,
            forward: 1.0,
            jump: false,
            sneak: true,
        });
        assert!(live.sneaking);
        let Ok(WorkerCommand::Movement { movement: m, .. }) = rx.try_recv() else {
            panic!("expected Movement command");
        };
        assert!(m.sneak);
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
        assert_eq!(live.pending_transfer.as_ref().map(|t| t.port), Some(25565));
        live.apply_event(ClientPlayEvent::Play(PlayEvent::Transfer {
            host: String::new(),
            port: 0,
        }))
        .unwrap();
        assert!(matches!(live.status, SessionStatus::Disconnected(_)));
    }

    #[test]
    fn horse_screen_open_drives_mount_inventory() {
        let (mut live, _tx) = session();
        // Track the mount so the screen title resolves its entity name.
        let horse_type = i32::try_from(
            megu_proto::ProtocolTable::v767_builtin()
                .registry_id("minecraft:entity_type", "minecraft:horse")
                .expect("horse entity type"),
        )
        .unwrap();
        live.apply_event(ClientPlayEvent::Play(PlayEvent::AddEntity {
            eid: 9,
            uuid: [7; 16],
            entity_type: horse_type,
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

        // Plain horse: `slots = 2` → sentinel menu 1000, 38 window slots.
        live.apply_event(ClientPlayEvent::Play(PlayEvent::HorseScreenOpen {
            window_id: 4,
            slots: 2,
            entity_id: 9,
        }))
        .unwrap();
        let screen = live.open_screen.as_ref().unwrap();
        assert_eq!(screen.window_id, 4);
        assert_eq!(screen.menu_type, 1000);
        assert_eq!(screen.title_text.as_deref(), Some("Horse"));
        let schema = megu_ui::container_screen::menu_schema(screen.menu_type as u32)
            .expect("mount schema resolves");
        assert_eq!(schema.slot_count, 38);
        assert_eq!(schema.inventory_start, 2);

        // Saddle (0) + armor (1) populated, the 36 player slots empty,
        // then the carried slot at the tail.
        let mut payload = vec![1, 5, 0, 0, 1, 6, 0, 0];
        payload.extend(std::iter::repeat_n(0u8, 36));
        payload.push(0);
        live.apply_event(ClientPlayEvent::Play(PlayEvent::ContainerSetContent {
            window_id: 4,
            state_id: 9,
            slot_count: 38,
            raw_payload: payload,
        }))
        .unwrap();
        let screen = live.open_screen.as_ref().unwrap();
        assert_eq!(screen.state_id, 9);
        assert_eq!(screen.slots[&0].item_id, 5);
        assert_eq!(screen.slots[&1].item_id, 6);
        assert!(screen.slots[&2].item_id == 0 || !screen.slots.contains_key(&2));

        // A player-inventory slot patch lands on the same window.
        live.apply_event(ClientPlayEvent::Play(PlayEvent::ContainerSetSlot {
            window_id: 4,
            state_id: 10,
            slot: 20,
            raw_item: vec![1, 7, 0, 0],
        }))
        .unwrap();
        assert_eq!(live.open_screen.as_ref().unwrap().slots[&20].item_id, 7);
        live.apply_event(ClientPlayEvent::Play(PlayEvent::ContainerClose {
            window_id: 4,
        }))
        .unwrap();
        assert!(live.open_screen.is_none());

        // Chested donkey/mule: `slots = 17` → sentinel menu 1005, chest
        // slots 2..=16 fill through `container_set_slot`.
        live.apply_event(ClientPlayEvent::Play(PlayEvent::HorseScreenOpen {
            window_id: 5,
            slots: 17,
            entity_id: 9,
        }))
        .unwrap();
        let screen = live.open_screen.as_ref().unwrap();
        assert_eq!(screen.window_id, 5);
        assert_eq!(screen.menu_type, 1005);
        live.apply_event(ClientPlayEvent::Play(PlayEvent::ContainerSetSlot {
            window_id: 5,
            state_id: 1,
            slot: 16,
            raw_item: vec![1, 8, 0, 0],
        }))
        .unwrap();
        assert_eq!(live.open_screen.as_ref().unwrap().slots[&16].item_id, 8);
        live.apply_event(ClientPlayEvent::Play(PlayEvent::ContainerClose {
            window_id: 5,
        }))
        .unwrap();
        assert!(live.open_screen.is_none());
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
        assert!(matches!(
            rx.recv().unwrap(),
            WorkerCommand::Swing { hand: 1 }
        ));
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
            .register_jpm_model("skeleton", r#"{"id":"body","translate":[0,-12,0]}"#)
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
        assert!(
            live.world
            .animations
            .evaluate_entity_pose(10)
            .unwrap()
                .is_some()
        );
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
        assert!(
            live.world
            .animations
            .evaluate_entity_pose(11)
            .unwrap()
                .is_none()
        );
    }

    #[test]
    fn title_packets_decode_text_and_feed_hud_snapshot() {
        let (mut live, _tx) = session();
        // Anonymous NBT compound: {"text": "Hello"} — tag 0x0a root, one
        // string entry named "text", End terminator.
        let nbt: Vec<u8> = vec![
            0x0a, 0x08, 0x00, 0x04, b't', b'e', b'x', b't', 0x00, 0x05, b'H', b'e', b'l', b'l',
            b'o', 0x00,
        ];
        live.apply_event(ClientPlayEvent::Play(PlayEvent::SetTitle {
            fade_in: 5,
            stay: 40,
            fade_out: 15,
        }))
        .unwrap();
        live.apply_event(ClientPlayEvent::Play(PlayEvent::SetTitleText {
            nbt_bytes: nbt.clone(),
        }))
        .unwrap();
        assert_eq!(live.title_text.as_deref(), Some("Hello"));
        assert_eq!(live.title_revision, 2);

        live.apply_event(ClientPlayEvent::Play(PlayEvent::SetSubtitleText {
            nbt_bytes: nbt,
        }))
        .unwrap();
        assert_eq!(live.subtitle_text.as_deref(), Some("Hello"));
        assert_eq!(live.title_revision, 3);

        let feed = live.hud_feed();
        assert_eq!(feed.title_revision, 3);
        assert_eq!(feed.title_times, Some([5, 40, 15]));
        assert_eq!(feed.title_text.as_deref(), Some("Hello"));
        assert!(feed.looking_at_block.is_none());
        // Malformed NBT fails closed: text stays None, packet still counted.
        live.apply_event(ClientPlayEvent::Play(PlayEvent::SetTitleText {
            nbt_bytes: vec![0xff, 0x01],
        }))
        .unwrap();
        assert_eq!(live.title_text, None);
    }

    #[test]
    fn jar_version_ids_never_crosses_versions_on_explicit_26_3() {
        let paths = |vid: Option<&str>| SessionPaths {
            version_id: vid.map(str::to_owned),
            ..SessionPaths::default()
        };
        assert_eq!(jar_version_ids(&paths(None)), vec!["1.21.1"]);
        assert_eq!(
            jar_version_ids(&paths(Some("megu-1.21.1"))),
            vec!["megu-1.21.1", "1.21.1"]
        );
        assert_eq!(jar_version_ids(&paths(Some("26.3"))), vec!["26.3"]);
        assert_eq!(
            jar_version_ids(&paths(Some("megu-26.3"))),
            vec!["megu-26.3", "26.3"]
        );
    }

    /// Anonymous-NBT `{"text":"..."}` — the same component shape the
    /// `player_combat_kill` message field carries on the wire.
    fn death_message_nbt(text: &str) -> Vec<u8> {
        let mut v = vec![10u8]; // TAG_Compound
        v.push(8); // TAG_String
        v.extend_from_slice(&4u16.to_be_bytes());
        v.extend_from_slice(b"text");
        v.extend_from_slice(&(text.len() as u16).to_be_bytes());
        v.extend_from_slice(text.as_bytes());
        v.push(0);
        v
    }

    /// The session half of the death loop: `set_health(0)` marks dead
    /// even with no kill packet (the fallback), the kill packet fills
    /// the death screen's cause/score fields, movement+prediction lock
    /// while dead, `request_respawn` queues `client_command(0)`, and
    /// `respawn`/`set_health(>0)` recovery restores the alive path.
    #[test]
    fn death_loop_drives_screen_snapshot_and_respawn_command() {
        let (tx, incoming) = mpsc::sync_channel(8);
        let (outgoing, rx) = mpsc::sync_channel(8);
        let _ = tx;
        let mut live = LiveSession::with_channels(incoming, outgoing);
        live.status = SessionStatus::Playing;
        live.pose = Some(PlayerPose {
            feet: [1.0, 64.0, 1.0],
            yaw: 0.0,
            pitch: 0.0,
        });
        let move_cmd = |feet| PlayerMovement {
            pose: PlayerPose {
                feet,
                yaw: 0.0,
                pitch: 0.0,
            },
            strafe: 0.0,
            forward: 1.0,
            jump: false,
            sneak: false,
        };

        // Alive: movement reaches the worker queue.
        live.send_movement(move_cmd([1.0, 64.0, 2.0]));
        assert!(matches!(rx.recv().unwrap(), WorkerCommand::Movement { .. }));

        // Zero health without a kill packet: the fallback marks death.
        live.apply_event(ClientPlayEvent::Play(PlayEvent::SetHealth {
            health: 0.0,
            food: 19,
            saturation: 4.5,
        }))
        .unwrap();
        assert!(live.is_dead());
        assert!(live.death_screen_state().unwrap().cause_message.is_none());

        // Gameplay input is locked while dead: neither movement packets
        // nor physics prediction advance.
        live.send_movement(move_cmd([1.0, 64.0, 3.0]));
        assert!(rx.try_recv().is_err(), "dead player must not move");
        assert!(
            live.predict_movement(0.0, 1.0, false, false, false, 0.0, 0.0)
                .is_none()
        );

        // The kill packet enriches the snapshot with the death message.
        live.apply_event(ClientPlayEvent::Play(PlayEvent::PlayerCombatKill {
            player_id: 11,
            message_nbt: death_message_nbt("Tester was slain by Creeper"),
        }))
        .unwrap();
        let state = live.death_screen_state().unwrap();
        assert_eq!(
            state.cause_message.as_deref(),
            Some("Tester was slain by Creeper")
        );
        assert!(!state.hardcore);

        // Respawn click -> `client_command(0)`; the pending flag blocks a
        // duplicate request until the server acknowledges.
        assert!(live.request_respawn());
        assert!(matches!(
            rx.recv().unwrap(),
            WorkerCommand::ClientCommand { action: 0 }
        ));
        assert!(live.death.respawn_pending());
        assert!(!live.request_respawn(), "pending request must not resend");

        // Server recovery: respawn + positive health clear the loop.
        live.apply_event(ClientPlayEvent::Play(PlayEvent::Respawn {
            dimension: DimensionMeta::default(),
        }))
        .unwrap();
        live.apply_event(ClientPlayEvent::Play(PlayEvent::SetHealth {
            health: 20.0,
            food: 20,
            saturation: 5.0,
        }))
        .unwrap();
        assert!(!live.is_dead());
        assert!(live.death_screen_state().is_none());

        // `Respawn` drops the pose like vanilla (position re-arrives via
        // the follow-up teleport), then movement flows again once alive.
        live.pose = Some(PlayerPose {
            feet: [8.0, 70.0, 8.0],
            yaw: 45.0,
            pitch: -10.0,
        });
        live.send_movement(move_cmd([1.0, 64.0, 4.0]));
        assert!(matches!(rx.recv().unwrap(), WorkerCommand::Movement { .. }));
    }

    /// A `JoinGame` with `hardcore` set makes the death screen
    /// spectate-only: `death_screen_state` reports the flag and
    /// `request_respawn` is refused outright.
    #[test]
    fn hardcore_death_refuses_respawn_request() {
        let (tx, incoming) = mpsc::sync_channel(8);
        let (outgoing, rx) = mpsc::sync_channel(8);
        let _ = tx;
        let mut live = LiveSession::with_channels(incoming, outgoing);
        live.status = SessionStatus::Playing;

        live.apply_event(ClientPlayEvent::Play(PlayEvent::JoinGame {
            entity_id: 11,
            dimension: DimensionMeta::default(),
            hardcore: true,
        }))
        .unwrap();
        live.apply_event(ClientPlayEvent::Play(PlayEvent::SetHealth {
            health: 0.0,
            food: 20,
            saturation: 5.0,
        }))
        .unwrap();
        assert!(live.is_dead());
        assert!(live.death_screen_state().unwrap().hardcore);
        assert!(!live.request_respawn(), "hardcore never requests respawn");
        assert!(rx.try_recv().is_err());
    }

    /// Fixture keeping the command receiver so serverbound dispatches
    /// (`ResourcePackResponse`, `PlayerAbilities`, `ClientInformation`)
    /// are observable.
    fn session_with_commands() -> (
        LiveSession,
        SyncSender<WorkerMessage>,
        Receiver<WorkerCommand>,
    ) {
        let (tx, incoming) = mpsc::sync_channel(8);
        let (outgoing, rx) = mpsc::sync_channel(8);
        (LiveSession::with_channels(incoming, outgoing), tx, rx)
    }

    #[test]
    fn container_set_data_reaches_open_window_and_session_map() {
        let (mut live, _tx, _rx) = session_with_commands();
        live.apply_event(ClientPlayEvent::Play(PlayEvent::OpenScreen {
            window_id: 3,
            menu_type: 14, // minecraft:furnace
            title_nbt: Vec::new(),
        }))
        .unwrap();
        for (property, value) in [(0, 40), (1, 80), (2, 5), (3, 200)] {
            live.apply_event(ClientPlayEvent::Play(PlayEvent::ContainerSetData {
                window_id: 3,
                property,
                value,
            }))
            .unwrap();
        }
        let screen = live.open_screen.as_ref().expect("furnace open");
        assert_eq!(screen.properties.get(&0), Some(&40));
        assert_eq!(screen.properties.get(&1), Some(&80));
        assert_eq!(screen.properties.get(&2), Some(&5));
        assert_eq!(screen.properties.get(&3), Some(&200));
        // The session map mirrors the same pairs keyed by window id so a
        // screen opening after the updates still shows them.
        assert_eq!(live.container_properties.get(&(3, 2)), Some(&5));
    }

    #[test]
    fn container_set_data_before_open_survives_late_open() {
        let (mut live, _tx, _rx) = session_with_commands();
        live.apply_event(ClientPlayEvent::Play(PlayEvent::ContainerSetData {
            window_id: 4,
            property: 0,
            value: 300,
        }))
        .unwrap();
        live.apply_event(ClientPlayEvent::Play(PlayEvent::OpenScreen {
            window_id: 4,
            menu_type: 17, // minecraft:brewing_stand
            title_nbt: Vec::new(),
        }))
        .unwrap();
        assert_eq!(
            live.open_screen.as_ref().unwrap().properties.get(&0),
            Some(&300)
        );
    }

    #[test]
    fn play_ping_is_recorded_session_side() {
        let (mut live, _tx, _rx) = session_with_commands();
        live.apply_event(ClientPlayEvent::Play(PlayEvent::Ping(42)))
            .unwrap();
        assert_eq!(live.last_server_ping, Some(42));
    }

    #[test]
    fn player_abilities_syncs_flags_speeds_and_physics() {
        let (mut live, _tx, _rx) = session_with_commands();
        live.status = SessionStatus::Playing;
        live.apply_event(ClientPlayEvent::Play(PlayEvent::PlayerAbilities {
            flags: 0x0f, // invulnerable + flying + may_fly + instabuild
            flying_speed: 0.1,
            walking_speed: 0.2,
        }))
        .unwrap();
        assert!(live.player_abilities.may_fly());
        assert!(live.player_abilities.flying());
        assert!(live.player_abilities.invulnerable());
        assert!(live.player_abilities.instabuild());
        assert_eq!(live.player_abilities.flying_speed, 0.1);
        assert_eq!(live.player_abilities.walking_speed, 0.2);
        assert!(live.physics_state.is_creative_flying);

        // A revoke (survival gamemode switch) force-drops flight in the
        // physics mirror too.
        live.apply_event(ClientPlayEvent::Play(PlayEvent::PlayerAbilities {
            flags: 0x00,
            flying_speed: 0.05,
            walking_speed: 0.1,
        }))
        .unwrap();
        assert!(!live.player_abilities.may_fly());
        assert!(!live.physics_state.is_creative_flying);
    }

    #[test]
    fn toggle_flight_needs_may_fly_and_echoes_flags() {
        let (mut live, _tx, rx) = session_with_commands();
        live.status = SessionStatus::Playing;
        // No may_fly grant: a double-space never toggles and never emits.
        assert!(!live.toggle_flight());
        assert!(rx.try_recv().is_err());

        live.apply_event(ClientPlayEvent::Play(PlayEvent::PlayerAbilities {
            flags: 0x04, // may_fly only
            flying_speed: 0.05,
            walking_speed: 0.1,
        }))
        .unwrap();
        assert!(live.toggle_flight());
        assert_eq!(live.player_abilities.flags, 0x06);
        assert!(live.physics_state.is_creative_flying);
        match rx.try_recv() {
            Ok(WorkerCommand::PlayerAbilities { flags }) => assert_eq!(flags, 0x06),
            _ => panic!("expected PlayerAbilities echo"),
        }
        // Toggling back off clears the flag and echoes again.
        assert!(live.toggle_flight());
        assert_eq!(live.player_abilities.flags, 0x04);
        assert!(!live.physics_state.is_creative_flying);
        match rx.try_recv() {
            Ok(WorkerCommand::PlayerAbilities { flags }) => assert_eq!(flags, 0x04),
            _ => panic!("expected PlayerAbilities echo"),
        }
    }

    #[test]
    fn resource_pack_push_tracks_uuid_and_answers_accepted_loaded() {
        let (mut live, _tx, rx) = session_with_commands();
        let uuid = [7u8; 16];
        live.apply_event(ClientPlayEvent::Play(PlayEvent::ResourcePackPush {
            uuid,
            url: "https://packs.example.org/p.zip".to_string(),
            hash: "abcd".to_string(),
            forced: true,
            prompt_nbt: None,
        }))
        .unwrap();
        assert_eq!(live.server_resource_packs.back(), Some(&uuid));
        let first = rx.try_recv().expect("ACCEPTED response");
        let second = rx.try_recv().expect("SUCCESSFULLY_LOADED response");
        match first {
            WorkerCommand::ResourcePackResponse { uuid: u, status } => {
                assert_eq!(u, uuid);
                assert_eq!(status, megu_client::protocol::RESOURCE_PACK_ACCEPTED);
            }
            _ => panic!("expected ResourcePackResponse"),
        }
        match second {
            WorkerCommand::ResourcePackResponse { uuid: u, status } => {
                assert_eq!(u, uuid);
                assert_eq!(
                    status,
                    megu_client::protocol::RESOURCE_PACK_SUCCESSFULLY_LOADED
                );
            }
            _ => panic!("expected ResourcePackResponse"),
        }
    }

    #[test]
    fn resource_pack_pop_discards_targeted_and_all() {
        let (mut live, _tx, rx) = session_with_commands();
        let (a, b) = ([1u8; 16], [2u8; 16]);
        for uuid in [a, b] {
            live.apply_event(ClientPlayEvent::Play(PlayEvent::ResourcePackPush {
                uuid,
                url: String::new(),
                hash: String::new(),
                forced: false,
                prompt_nbt: None,
            }))
            .unwrap();
        }
        while rx.try_recv().is_ok() {}

        // Targeted pop: only `a` leaves, one DISCARDED echo.
        live.apply_event(ClientPlayEvent::Play(PlayEvent::ResourcePackPop {
            uuid: Some(a),
        }))
        .unwrap();
        assert_eq!(live.server_resource_packs.len(), 1);
        match rx.try_recv() {
            Ok(WorkerCommand::ResourcePackResponse { uuid, status }) => {
                assert_eq!(uuid, a);
                assert_eq!(status, megu_client::protocol::RESOURCE_PACK_DISCARDED);
            }
            _ => panic!("expected DISCARDED echo"),
        }

        // UUID-less pop clears everything and echoes per pack.
        live.apply_event(ClientPlayEvent::Play(PlayEvent::ResourcePackPop {
            uuid: None,
        }))
        .unwrap();
        assert!(live.server_resource_packs.is_empty());
        match rx.try_recv() {
            Ok(WorkerCommand::ResourcePackResponse { uuid, status }) => {
                assert_eq!(uuid, b);
                assert_eq!(status, megu_client::protocol::RESOURCE_PACK_DISCARDED);
            }
            _ => panic!("expected DISCARDED echo"),
        }
    }

    #[test]
    fn resend_client_information_emits_only_on_change_while_playing() {
        let (mut live, _tx, rx) = session_with_commands();
        // Still connecting: resends must not fire before play.
        live.resend_client_information(12);
        assert!(rx.try_recv().is_err());

        live.status = SessionStatus::Playing;
        live.client_view_distance = 10;
        live.resend_client_information(10); // unchanged: no packet
        assert!(rx.try_recv().is_err());
        live.resend_client_information(12);
        match rx.try_recv() {
            Ok(WorkerCommand::ClientInformation { view_distance }) => {
                assert_eq!(view_distance, 12)
            }
            _ => panic!("expected ClientInformation"),
        }
        assert_eq!(live.client_view_distance, 12);
        live.resend_client_information(12); // second identical call no-ops
        assert!(rx.try_recv().is_err());
    }

    // ------------------------------------------------------------------
    // L7 math/feedback wiring proofs: challenger probing, drain-profile
    // steering, prefetch boost, and live tick telemetry.
    // ------------------------------------------------------------------

    #[test]
    fn kernel_probe_dispatches_challenger_and_switches_on_real_timings() {
        let (mut live, _tx) = session();
        let selected = live.active_kernel_mode();
        let challenger = match selected {
            ComputeKernelMode::LutBatch => ComputeKernelMode::RecomputeSwar,
            ComputeKernelMode::RecomputeSwar => ComputeKernelMode::LutBatch,
        };

        // Ordinary polls dispatch the incumbent mode.
        for _ in 0..KERNEL_PROBE_INTERVAL_POLLS - 1 {
            assert_eq!(live.dispatch_kernel_mode(), selected);
        }
        assert_eq!(live.kernel_probe_count(), 0);
        // Every interval-th poll arms a single challenger dispatch so the
        // non-selected arm receives real measurements.
        assert_eq!(live.dispatch_kernel_mode(), challenger);
        assert_eq!(live.executed_kernel_mode(), challenger);
        assert_eq!(live.kernel_probe_count(), 1);
        assert_eq!(live.dispatch_kernel_mode(), selected);
        assert_eq!(live.executed_kernel_mode(), selected);

        // The switch can only ever happen because probes feed challenger
        // timings: consistently faster challenger samples flip the
        // selection after four qualifying observations.
        for _ in 0..8 {
            live.kernel_autotuner.observe_sample(selected, 20_000_000);
            live.kernel_autotuner.observe_sample(challenger, 2_000_000);
        }
        assert_eq!(
            live.active_kernel_mode(),
            challenger,
            "consistently faster challenger timings must flip selected_mode"
        );
    }

    #[test]
    fn executed_kernel_mode_drives_rebuild_budget_profile() {
        let (mut live, _tx) = session();
        live.executed_kernel_mode = ComputeKernelMode::LutBatch;
        let batched = live.mesh_rebuild_budget();
        live.executed_kernel_mode = ComputeKernelMode::RecomputeSwar;
        let streaming = live.mesh_rebuild_budget();
        assert!(batched >= 1 && streaming >= 1);
        assert!(
            streaming < batched,
            "RecomputeSwar = low-latency slice: {streaming} < {batched}"
        );
    }

    #[test]
    fn poll_consumes_ducb_select_arm_as_drain_profile() {
        let (mut live, _tx) = session();
        let mut ui = megu_ui::state::UiState::new();
        // Untried arms win lowest-first; each poll selects the next arm
        // and the submit cap follows the selected drain profile.
        let mut caps = Vec::new();
        for _ in 0..4 {
            live.poll([8.0, 20.0, 24.0], &[]);
            caps.push((live.ducb_selected_arm(), live.offthread_submit_cap()));
            live.record_frame_and_refresh_compact_debug_hud(1.0 / 60.0, &mut ui);
        }
        assert_eq!(
            caps,
            vec![(0, 2), (1, 8), (2, 16), (3, 32)],
            "select_arm must steer the off-thread submission cap each poll"
        );
    }

    #[test]
    fn prefetch_boost_map_prioritizes_predicted_heading_cone() {
        let (mut live, _tx) = session();
        live.pose = Some(PlayerPose {
            feet: [0.0, 70.0, 0.0],
            yaw: 0.0,
            pitch: 0.0,
        });
        live.last_motion_delta = [4.0, 0.0]; // moving +X
        live.refresh_prefetcher();
        let boosts = live.prefetch_boost_map();
        assert!(
            !boosts.is_empty(),
            "a clear heading must emit forward-cone prefetch entries"
        );
        assert!(
            boosts.keys().all(|&(cx, _)| cx > 0),
            "every boosted chunk lies in the +X heading cone: {boosts:?}"
        );
        let max_boost = boosts.values().copied().fold(0.0f32, f32::max);
        assert!(
            max_boost > 0.0
                && max_boost <= PREFETCH_MAX_CHUNKS as f32 * PREFETCH_BOOST_BLOCKS_PER_RANK,
            "boosts stay bounded, got {max_boost}"
        );

        // A heading reversal cancels the stale entries and emits the new
        // cone only — stale generations never produce a boost.
        live.last_motion_delta = [-4.0, 0.0];
        live.refresh_prefetcher();
        let boosted = live.prefetch_boost_map();
        assert!(
            boosted.keys().all(|&(cx, _)| cx < 0),
            "after reversal every boost must point -X: {boosted:?}"
        );
    }

    #[test]
    fn teleport_flushes_stale_prefetch_and_boosts_arrival_ring_once() {
        let (mut live, _tx) = session();
        live.pose = Some(PlayerPose {
            feet: [0.0, 70.0, 0.0],
            yaw: 0.0,
            pitch: 0.0,
        });
        live.last_motion_delta = [4.0, 0.0];
        live.refresh_prefetcher();
        assert!(live.prefetcher.pending_count() > 0);

        live.pose = Some(PlayerPose {
            feet: [100.0, 70.0, 0.0],
            yaw: 0.0,
            pitch: 0.0,
        });
        live.last_motion_delta = [40.0, 0.0];
        live.refresh_prefetcher();
        assert_eq!(
            live.prefetcher.pending_count(),
            0,
            "teleport must flush every stale directional bet immediately"
        );
        let ring = live.prefetch_boost_map();
        assert_eq!(ring.len(), 8);
        let base = ((100.0f64 / 16.0).floor() as i32, 0);
        assert!(
            ring.keys()
                .all(|&(cx, cz)| { (cx - base.0).abs().max((cz - base.1).abs()) == 1 })
        );
        assert!(ring.keys().any(|&(cx, _)| cx < base.0));
        assert!(ring.keys().any(|&(cx, _)| cx > base.0));
        assert!(ring.keys().any(|&(_, cz)| cz < base.1));
        assert!(ring.keys().any(|&(_, cz)| cz > base.1));

        // Refreshing the same sample again does not retrigger the teleport:
        // the ring is consumed and velocity-scaled +X lookahead resumes.
        live.refresh_prefetcher();
        let resumed = live.prefetch_boost_map();
        assert!(!resumed.is_empty());
        assert!(
            resumed.keys().all(|&(cx, _)| cx > base.0),
            "lookahead resumes in the movement direction: {resumed:?}"
        );
    }

    #[test]
    fn velocity_scaled_prefetch_budget_is_smooth_and_capped() {
        let walking = velocity_scaled_prefetch_budget([0.07, 0.0]);
        let sprinting = velocity_scaled_prefetch_budget([0.10, 0.0]);
        let ice_boat = velocity_scaled_prefetch_budget([0.67, 0.0]);
        let elytra = velocity_scaled_prefetch_budget([1.0, 0.0]);
        assert!(walking.0 >= PREFETCH_MIN_VIEW_DISTANCE);
        assert!(sprinting.1 >= walking.1);
        assert!(ice_boat.0 > sprinting.0 || ice_boat.1 > sprinting.1);
        assert_eq!(elytra, (PREFETCH_VIEW_DISTANCE, PREFETCH_MAX_CHUNKS));
        assert!(elytra.0 <= PREFETCH_VIEW_DISTANCE && elytra.1 <= PREFETCH_MAX_CHUNKS);
        assert_eq!(
            velocity_scaled_prefetch_budget([f64::NAN, 0.0]),
            (PREFETCH_MIN_VIEW_DISTANCE, PREFETCH_MIN_CHUNKS)
        );
    }

    #[test]
    fn live_contraption_transform_interpolates_and_teleport_snaps() {
        let (mut live, _tx) = session();
        live.upsert_live_contraption_subworld(
            42,
            SubWorldTransform::IDENTITY,
            Vec::new(),
            Vec::new(),
        );
        let mut target = SubWorldTransform::IDENTITY;
        target.position = [2.0, 0.0, 0.0];
        assert!(live.set_live_contraption_transform(42, target));
        let mid = live
            .live_contraption_interpolated_transform(42, 0.5)
            .unwrap();
        assert_eq!(mid.position, [1.0, 0.0, 0.0]);

        target.position = [40.0, 0.0, 0.0];
        assert!(live.set_live_contraption_transform(42, target));
        let snapped = live
            .live_contraption_interpolated_transform(42, 0.0)
            .unwrap();
        assert_eq!(
            snapped.position,
            [40.0, 0.0, 0.0],
            "large corrections snap both endpoints instead of smearing"
        );
    }

    #[test]
    fn live_ticks_feed_mspt_and_tps_telemetry() {
        let (mut live, _tx) = session();
        assert_eq!(live.frame_timings.tick_count(), 0);

        live.last_vfx_tick = Instant::now() - Duration::from_millis(150);
        live.poll([8.0, 20.0, 24.0], &[]);
        assert!(
            live.frame_timings.tick_count() > 0,
            "poll must push one tick sample per elapsed 50 ms step"
        );
        assert!(
            live.frame_timings.mspt_avg() > 0.0,
            "recorded tick durations must average above zero"
        );
        assert!(
            live.frame_timings.tps_avg() > 0.0,
            "a live mspt gives a positive TPS estimate"
        );

        // The samples must flow into the Compact Debug HUD copy path as well.
        let mut ui = megu_ui::state::UiState::new();
        ui.settings.qol.compact_debug_hud.enabled = true;
        live.record_frame_and_refresh_compact_debug_hud(1.0 / 60.0, &mut ui);
        assert!(
            ui.hud.compact_debug_hud_data.mspt.is_some()
                && ui.hud.compact_debug_hud_data.tps.is_some(),
            "live tick samples must reach Compact Debug HUD mspt/tps"
        );
    }

    /// `clear_titles` hides the live overlay immediately: text and raw
    /// NBT drop while the fade/stay timing state stays armed for the
    /// next `set_title_text`.
    #[test]
    fn clear_titles_clears_overlay_fields_and_bumps_revisions() {
        let (mut live, _tx) = session();
        let nbt: Vec<u8> = vec![
            0x0a, 0x08, 0x00, 0x04, b't', b'e', b'x', b't', 0x00, 0x05, b'H', b'e', b'l', b'l',
            b'o', 0x00,
        ];
        live.apply_event(ClientPlayEvent::Play(PlayEvent::SetTitleText {
            nbt_bytes: nbt.clone(),
        }))
        .unwrap();
        live.apply_event(ClientPlayEvent::Play(PlayEvent::SetSubtitleText {
            nbt_bytes: nbt,
        }))
        .unwrap();
        assert_eq!(live.title_text.as_deref(), Some("Hello"));
        assert_eq!(live.subtitle_text.as_deref(), Some("Hello"));
        let title_rev = live.title_revision;
        let hud_rev = live.hud_revision;

        live.apply_event(ClientPlayEvent::Play(PlayEvent::ClearTitles))
            .unwrap();
        assert_eq!(live.title_text, None);
        assert_eq!(live.subtitle_text, None);
        assert_eq!(live.title_text_nbt, None);
        assert_eq!(live.subtitle_text_nbt, None);
        assert!(live.title_revision > title_rev);
        assert!(live.hud_revision > hud_rev);
    }

    /// `game_event` session ids: mode flip mirrors into both the session
    /// and `hud.game_mode` (the layout vitals gate), `win_game` records
    /// the credits request, `enable_respawn_screen` flips the flag, and
    /// the demo event surfaces its hint as a chat line.
    #[test]
    fn game_event_feeds_game_mode_respawn_flag_and_demo_hints() {
        let (mut live, _tx) = session();
        live.apply_event(ClientPlayEvent::Play(PlayEvent::GameEvent {
            event: 3,
            value: 1.0,
        }))
        .unwrap();
        assert_eq!(live.game_mode, 1);
        assert_eq!(live.hud.game_mode, 1);

        live.apply_event(ClientPlayEvent::Play(PlayEvent::GameEvent {
            event: 4,
            value: 1.0,
        }))
        .unwrap();
        assert!(live.pending_end_credits);

        live.apply_event(ClientPlayEvent::Play(PlayEvent::GameEvent {
            event: 11,
            value: 0.0,
        }))
        .unwrap();
        assert!(!live.respawn_screen_enabled);

        let chat_before = live.hud.chat_lines.len();
        live.apply_event(ClientPlayEvent::Play(PlayEvent::GameEvent {
            event: 5,
            value: 103.0,
        }))
        .unwrap();
        assert_eq!(live.hud.chat_lines.len(), chat_before + 1);
        assert!(live.hud.chat_lines[chat_before].contains("jump"));

        // `reset_dimension` (respawn/login) clears the credits request.
        live.apply_event(ClientPlayEvent::Play(PlayEvent::Respawn {
            dimension: DimensionMeta::default(),
        }))
        .unwrap();
        assert!(!live.pending_end_credits);
    }

    /// `enable_respawn_screen(0)` is vanilla's immediate respawn: the
    /// moment death lands the client queues `client_command(0)` itself —
    /// no death screen, no click.
    #[test]
    fn respawn_screen_disabled_auto_requests_on_death() {
        let (mut live, _tx, rx) = session_with_commands();
        live.status = SessionStatus::Playing;
        live.apply_event(ClientPlayEvent::Play(PlayEvent::GameEvent {
            event: 11,
            value: 0.0,
        }))
        .unwrap();
        live.apply_event(ClientPlayEvent::Play(PlayEvent::SetHealth {
            health: 0.0,
            food: 20,
            saturation: 5.0,
        }))
        .unwrap();
        assert!(live.is_dead());
        assert!(matches!(
            rx.recv().unwrap(),
            WorkerCommand::ClientCommand { action: 0 }
        ));
    }

    /// `explode`: knockback lands on the local player's velocity, every
    /// destruction record marks its section dirty through the same path
    /// `block_update` uses, the camera gets a bounded kick, and the
    /// world counts the removals.
    #[test]
    fn explosion_knockback_dirty_marks_camera_kick_and_removals() {
        let (mut live, _tx) = session();
        live.apply_event(ClientPlayEvent::Play(PlayEvent::Explosion {
            x: 8.4,
            y: 65.2,
            z: -3.9,
            power: 4.0,
            records: vec![[0, 0, 0], [1, 2, 0], [-1, 0, 3]],
            knockback: [0.5, 1.0, -0.25],
            block_interaction: 1,
            small_particle_id: 0,
            large_particle_id: 0,
            sound_name: Some("minecraft:entity.generic.explode".to_string()),
            sound_id: None,
        }))
        .unwrap();
        assert_eq!(live.physics_state.vel, [0.5, 1.0, -0.25]);
        assert!(live.terrain_dirty);
        assert!(!live.dirty_terrain_coords.is_empty());
        assert!(live.tactile_landing_impulse > 0.0);
        assert_eq!(live.world.block_updates, 3);
    }

    /// `level_event`/`block_event`/`level_particles`/`take_item_entity`
    /// flow through `apply_play` into the bounded world history queues —
    /// and into synthesized sound ingests that must never error.
    #[test]
    fn world_feedback_events_route_to_queues_and_sound() {
        let (mut live, _tx) = session();
        let particle_data = [0.2f32, 0.8, 0.4, 1.0]
            .into_iter()
            .flat_map(f32::to_be_bytes)
            .collect();
        live.apply_event(ClientPlayEvent::Play(PlayEvent::LevelEvent {
            event: 1006,
            pos: [1, 64, 2],
            data: 0,
            disable_relative_volume: false,
        }))
        .unwrap();
        assert_eq!(live.world.level_events.len(), 1);

        live.apply_event(ClientPlayEvent::Play(PlayEvent::BlockEvent {
            pos: [1, 64, 2],
            action: 0,
            parameter: 0,
            block_type: 33,
        }))
        .unwrap();
        assert_eq!(live.world.block_events.len(), 1);

        live.apply_event(ClientPlayEvent::Play(PlayEvent::LevelParticles {
            particle_id: 13,
            long_distance: false,
            x: 1.0,
            y: 65.0,
            z: 2.0,
            offset_x: 0.0,
            offset_y: 0.0,
            offset_z: 0.0,
            max_speed: 0.0,
            count: 4,
            particle_data,
        }))
        .unwrap();
        assert_eq!(live.world.level_particles.len(), 1);

        live.apply_event(ClientPlayEvent::Play(PlayEvent::TakeItemEntity {
            item_entity: 5,
            player: 7,
            amount: 1,
        }))
        .unwrap();
        assert_eq!(live.world.item_pickups.len(), 1);
    }

    /// The synthesized sound tables stay pinned to the vanilla ordinals.
    #[test]
    fn synthesized_sound_tables_cover_the_vanilla_ids() {
        assert_eq!(
            level_event_sound(1006, 0).map(|(n, _, _, _)| n),
            Some("minecraft:block.wooden_door.open")
        );
        assert_eq!(
            level_event_sound(2002, 0).map(|(n, _, _, _)| n),
            Some("minecraft:entity.splash_potion.break")
        );
        assert!(level_event_sound(999, 0).is_none());

        assert_eq!(
            block_event_sound("minecraft:piston", 0, 0),
            Some("minecraft:block.piston.extend")
        );
        assert_eq!(
            block_event_sound("minecraft:chest", 1, 1),
            Some("minecraft:block.chest.open")
        );
        assert_eq!(
            block_event_sound("minecraft:chest", 1, 0),
            Some("minecraft:block.chest.close")
        );
        assert_eq!(block_event_sound("minecraft:stone", 0, 0), None);

        assert_eq!(
            break_sound_for_block("minecraft:oak_log"),
            "minecraft:block.wood.break"
        );
        assert_eq!(
            break_sound_for_block("minecraft:glass"),
            "minecraft:block.glass.break"
        );
        assert_eq!(
            break_sound_for_block("minecraft:stone"),
            "minecraft:block.stone.break"
        );

        assert!(demo_hint_line(103).is_some());
        assert!(demo_hint_line(104).is_some());
        assert!(demo_hint_line(55).is_none());
    }

    #[test]
    fn open_sign_editor_sets_pending_for_window() {
        let (mut live, _tx) = session();
        live.apply_event(ClientPlayEvent::Play(PlayEvent::OpenSignEditor {
            pos: [10, 64, -20],
            front_side: true,
        }))
        .unwrap();
        assert_eq!(live.take_pending_sign_editor(), Some(((10, 64, -20), true)));
        assert_eq!(live.take_pending_sign_editor(), None);
    }

    #[test]
    fn open_sign_editor_back_side_pending() {
        let (mut live, _tx) = session();
        live.apply_event(ClientPlayEvent::Play(PlayEvent::OpenSignEditor {
            pos: [-3, 70, 42],
            front_side: false,
        }))
        .unwrap();
        assert_eq!(live.take_pending_sign_editor(), Some(((-3, 70, 42), false)));
    }

    #[test]
    fn send_update_sign_queues_worker_command() {
        let (live, _tx, rx) = session_with_commands();
        let lines: [String; 4] = [
            "Hello".to_owned(),
            "看板".to_owned(),
            String::new(),
            "four".to_owned(),
        ];
        assert!(live.send_update_sign((10, 64, -20), true, lines.clone()));
        match rx.try_recv() {
            Ok(WorkerCommand::UpdateSign {
                pos,
                is_front,
                lines: got,
            }) => {
                assert_eq!(pos, (10, 64, -20));
                assert!(is_front);
                assert_eq!(got, lines);
            }
            _ => panic!("expected UpdateSign command"),
        }
    }

    #[test]
    fn sign_update_payload_decodes_to_position_face_and_lines() {
        // Vanilla `ServerboundSignUpdatePacket` body layout: Position
        // (packed i64, x:26|z:26|y:12), is_front bool, then four
        // VarInt-prefixed UTF-8 strings — including multi-byte CJK.
        let lines: [String; 4] = [
            "看板のテキスト".to_owned(),
            "line2".to_owned(),
            "san行目".to_owned(),
            String::new(),
        ];
        let payload = encode_sign_update_payload((-300, 64, 12345), true, &lines);
        let mut r = WireReader::new(&payload);
        assert_eq!(r.read_position().unwrap(), [-300, 64, 12345]);
        assert!(r.read_bool().unwrap());
        for want in &lines {
            assert_eq!(&r.read_string(384).unwrap(), want);
        }
        assert_eq!(r.remaining(), 0);

        // is_front=false round-trips the face flag.
        let payload = encode_sign_update_payload((0, -60, 0), false, &lines);
        let mut r = WireReader::new(&payload);
        assert_eq!(r.read_position().unwrap(), [0, -60, 0]);
        assert!(!r.read_bool().unwrap());
    }

    #[test]
    fn sign_update_packet_id_matches_registered_tables() {
        // `proto/src/data_v767.rs`: play/serverbound sign_update = 0x35;
        // `proto/src/data_v777.rs`: same name at 0x3e.
        assert_eq!(
            sign_update_packet_id(megu_proto::ProtocolVersion::V1_21_1),
            0x35
        );
        assert_eq!(
            sign_update_packet_id(megu_proto::ProtocolVersion::V26_3),
            0x3e
        );
    }

    #[test]
    fn permission_events_only_apply_to_local_player() {
        let mut live = LiveSession::offline_with_null_audio();
        live.player_entity_id = Some(7);
        live.apply_play(PlayEvent::EntityEvent { eid: 8, status: 28 }).unwrap();
        assert_eq!(live.permission_level, 0);
        live.apply_play(PlayEvent::EntityEvent { eid: 7, status: 26 }).unwrap();
        assert_eq!(live.permission_level, 2);
        live.apply_play(PlayEvent::EntityEvent { eid: 7, status: 2 }).unwrap();
        assert_eq!(live.permission_level, 2);
    }

    #[test]
    fn seeded_dispatcher_omits_non_vanilla_navigation_commands() {
        let dispatcher = seeded_command_dispatcher();
        for cmd in ["megu", "aero", "hub", "server", "world"] {
            // Brigadier completes on the typed prefix — the chat bar
            // asks at the cursor, so a one-letter head must offer it.
            let prefix = &cmd[..1];
            let suggestions = dispatcher
                .complete_suggestions(prefix, prefix.len())
                .unwrap_or_else(|_| panic!("suggestions for {cmd}"));
            assert!(
                !(0..suggestions.len()).any(|i| suggestions.get(i) == Some(cmd)),
                "seeded dispatcher must not invent /{cmd}"
            );
        }
    }

    // ------------------------------------------------------------------
    // REALISM-DYNAMIC-LIGHTS-LIVE
    // ------------------------------------------------------------------

    fn item_id(name: &str) -> i32 {
        megu_proto::table::ProtocolTable::v767_builtin()
            .registry_id("minecraft:item", name)
            .unwrap_or_else(|| panic!("builtin item table must know {name}")) as i32
    }

    fn stack(item_id: i32) -> ItemStackComponents {
        ItemStackComponents {
            count: 1,
            item_id,
            added: Vec::new(),
            removed: Vec::new(),
        }
    }

    fn slot_wire(item_id: i32) -> Vec<u8> {
        let mut wire = Vec::new();
        write_varint(&mut wire, 1);
        write_varint(&mut wire, item_id);
        write_varint(&mut wire, 0);
        write_varint(&mut wire, 0);
        wire
    }

    fn add_entity(live: &mut LiveSession, eid: i32, entity_type: i32, pos: [f64; 3]) {
        live.apply_event(ClientPlayEvent::Play(PlayEvent::AddEntity {
            eid,
            uuid: [eid as u8; 16],
            entity_type,
            x: pos[0],
            y: pos[1],
            z: pos[2],
            pitch: 0.0,
            yaw: 0.0,
            head_yaw: 0.0,
            data: 0,
            vx: 0.0,
            vy: 0.0,
            vz: 0.0,
        }))
        .unwrap();
    }

    #[test]
    fn item_dynamic_light_spec_maps_emitter_families() {
        let torch = item_dynamic_light_spec("minecraft:torch", false).unwrap();
        // Warm torch flame + open-flame flicker through the resolver.
        assert_eq!(torch.rgb, [1.0, 0.62, 0.22]);
        assert!(torch.flicker > 0.0, "open flame must flicker");
        assert!((13.0..=15.0).contains(&torch.radius));
        // Bare paths resolve the same as namespaced ids.
        assert_eq!(item_dynamic_light_spec("torch", false), Some(torch));

        // Open flames drown; enclosed lanterns keep burning underwater.
        assert_eq!(item_dynamic_light_spec("minecraft:torch", true), None);
        assert_eq!(item_dynamic_light_spec("minecraft:campfire", true), None);
        assert_eq!(item_dynamic_light_spec("minecraft:wall_torch", true), None);
        assert!(item_dynamic_light_spec("minecraft:lantern", true).is_some());
        assert!(item_dynamic_light_spec("minecraft:jack_o_lantern", true).is_some());

        let soul = item_dynamic_light_spec("minecraft:soul_lantern", false).unwrap();
        assert_eq!(soul.rgb, [0.3, 0.85, 1.0]);
        assert!((9.0..=11.0).contains(&soul.radius));

        let lava = item_dynamic_light_spec("minecraft:lava_bucket", false).unwrap();
        assert!(lava.rgb[0] == 1.0 && lava.rgb[2] < 0.2, "lava stays golden");
        assert!((13.0..=15.0).contains(&lava.radius));

        let sea = item_dynamic_light_spec("minecraft:sea_lantern", false).unwrap();
        assert!(sea.intensity > 0.5, "sea lantern must still shine");
        assert!((9.0..=15.0).contains(&sea.radius));

        let redstone = item_dynamic_light_spec("minecraft:redstone_torch", false).unwrap();
        assert!(
            redstone.rgb[0] > 0.9 && redstone.rgb[1] < 0.3 && redstone.rgb[2] < 0.2,
            "redstone torch must glow crimson: {:?}",
            redstone.rgb
        );
        assert!((6.0..=9.0).contains(&redstone.radius));

        // Modded light-bearing items resolve through the same universal
        // vocabulary — no per-mod tables.
        let mod_gem = item_dynamic_light_spec("mymod:plasma_gem", false).unwrap();
        assert!(mod_gem.radius >= 8.0 && mod_gem.intensity > 0.5);

        assert_eq!(item_dynamic_light_spec("minecraft:stone", false), None);
        assert_eq!(item_dynamic_light_spec("minecraft:dirt", true), None);
    }

    #[test]
    fn collect_dynamic_lights_held_torch_hand_anchor_and_settings_gate() {
        let (mut live, _tx) = session();
        live.pose = Some(PlayerPose {
            feet: [8.0, 64.0, 8.0],
            yaw: 0.0,
            pitch: 0.0,
        });
        live.hud.selected_slot = 0;
        live.player_inventory
            .insert(36, stack(item_id("minecraft:torch")));

        // World Effects (nature pack) no longer gates torch/entity
        // lights — the Video `Dynamic Lights` toggle owns the switch at
        // the call site — so a held torch lights even with it off.
        let lights = live.collect_dynamic_lights([8.0, 64.0, 8.0]);
        assert_eq!(lights.len(), 1);
        let torch = &lights[0];
        assert_eq!(torch.rgb, [1.0, 0.62, 0.22]);
        // Hand flame-tip anchor: yaw 0 -> forward +z, right -x, so the
        // torch floats slightly ahead (+z) and to the right (-x) at
        // hand height feet+1.48, not dead-center at the eye.
        assert_eq!(torch.pos, [7.75, 65.48, 8.35]);
        let spec = item_dynamic_light_spec("minecraft:torch", false).unwrap();
        let jitter = spec.intensity * spec.flicker;
        assert!(
            (torch.intensity - spec.intensity).abs() <= jitter + 1e-6,
            "modulated flicker must stay inside the ±{:.2} band: {} vs {}",
            spec.flicker,
            torch.intensity,
            spec.intensity
        );

        // The brighter of the two hands wins: offhand lava bucket beats
        // the mainhand torch.
        live.player_inventory
            .insert(45, stack(item_id("minecraft:lava_bucket")));
        let lights = live.collect_dynamic_lights([8.0, 64.0, 8.0]);
        assert_eq!(lights.len(), 1);
        assert_eq!(lights[0].rgb[0], 1.0);
        assert!(
            lights[0].rgb[2] < 0.2,
            "lava stays golden: {:?}",
            lights[0].rgb
        );
        assert_eq!(lights[0].radius, 15.0);
    }

    #[test]
    fn collect_dynamic_lights_mobs_fire_drops_and_remote_held_items() {
        let (mut live, _tx) = session();
        live.vanilla_plus_cfg.nature.enabled = true;

        // Blaze (type 8) and glow squid (type 48) emit on their own.
        add_entity(&mut live, 10, 8, [4.0, 64.0, 0.0]);
        add_entity(&mut live, 11, 48, [-4.0, 64.0, 0.0]);
        // A zombie (type 124) is dark until the on-fire flag lands.
        add_entity(&mut live, 12, 124, [0.0, 64.0, 4.0]);
        // Dropped torch: `minecraft:item` (type 58) + metadata slot 8.
        add_entity(&mut live, 13, 58, [0.0, 64.0, -4.0]);
        let mut item_meta = vec![8u8, 7];
        item_meta.extend_from_slice(&slot_wire(item_id("minecraft:torch")));
        item_meta.push(0xFF);
        live.apply_event(ClientPlayEvent::Play(PlayEvent::SetEntityData {
            eid: 13,
            raw_entries: item_meta,
        }))
        .unwrap();
        // Remote player (type 128) holding a torch in mainhand slot 0.
        add_entity(&mut live, 14, 128, [6.0, 64.0, 6.0]);
        live.apply_event(ClientPlayEvent::Play(PlayEvent::SetEquipment {
            eid: 14,
            entries: vec![EquipmentEntry {
                slot: 0,
                item_wire: slot_wire(item_id("minecraft:torch")),
            }],
        }))
        .unwrap();

        let lights = live.collect_dynamic_lights([0.0, 64.0, 0.0]);
        // Blaze, glow squid, dropped torch and the remote torch-holder:
        // four lights (the zombie stays dark until it burns).
        assert_eq!(lights.len(), 4);
        // Nearest-first ordering for the UBO pack: the farthest remote
        // player (dist² ~72.8) sorts last.
        assert_eq!(lights[3].pos, [6.0, 64.9, 6.0]);
        assert!(
            lights.iter().any(|l| l.rgb[0] == 1.0 && l.rgb[2] < 0.2),
            "blaze must glow magma-gold: {lights:?}"
        );
        assert!(
            lights.iter().any(|l| l.rgb[1] == 1.0 && l.rgb[0] < 1.0),
            "glow squid must emit its phosphor tint: {lights:?}"
        );
        assert!(
            lights.iter().filter(|l| l.rgb == [1.0, 0.62, 0.22]).count() == 2,
            "dropped + remote-held torches: {lights:?}"
        );

        // On-fire flag bit 0x01 lights even a non-luminous mob, and the
        // spectral-glow bit 0x40 gives a pale cyan-white emitter.
        live.apply_event(ClientPlayEvent::Play(PlayEvent::SetEntityData {
            eid: 12,
            raw_entries: vec![0, 0, 0x01, 0xFF],
        }))
        .unwrap();
        let lights = live.collect_dynamic_lights([0.0, 64.0, 0.0]);
        assert_eq!(lights.len(), 5);
        assert!(
            lights.iter().any(|l| l.rgb == [1.0, 0.55, 0.12]),
            "burning zombie must carry the fire overlay: {lights:?}"
        );
        live.apply_event(ClientPlayEvent::Play(PlayEvent::SetEntityData {
            eid: 12,
            raw_entries: vec![0, 0, 0x40, 0xFF],
        }))
        .unwrap();
        let lights = live.collect_dynamic_lights([0.0, 64.0, 0.0]);
        assert_eq!(lights.len(), 5);
        assert!(
            lights.iter().any(|l| l.rgb == [0.75, 0.95, 1.0]),
            "glowing zombie must carry the spectral overlay: {lights:?}"
        );
    }

    #[test]
    fn collect_dynamic_lights_caps_at_wgsl_64_light_budget() {
        let (mut live, _tx) = session();
        live.vanilla_plus_cfg.nature.enabled = true;
        // 80 blazes receding along +X from the camera.
        for index in 0..80 {
            add_entity(&mut live, 1000 + index, 8, [(index + 1) as f64, 64.0, 0.0]);
        }
        let lights = live.collect_dynamic_lights([0.0, 64.0, 0.0]);
        assert_eq!(lights.len(), MAX_DYNAMIC_LIGHTS);
        // The sort is stable nearest-first: the last admitted light is
        // closer than the first rejected one would have been.
        let farthest_kept = lights.iter().map(|l| l.pos[0]).fold(0.0f32, f32::max);
        assert!(
            farthest_kept <= MAX_DYNAMIC_LIGHTS as f32 + 0.5,
            "only the 64 nearest lights may be packed: {farthest_kept}"
        );
    }

    /// A seven-state synthetic pack: air, stone, torch, unlit/lit
    /// redstone lamp, redstone wire and a modded lantern — enough for
    /// the placed-block scan to prove name-driven universal emission.
    fn luminous_test_registry() -> megu_kernel::Registry {
        const IS_AIR: u8 = 1;
        const BLOCKS_MOTION: u8 = 1 << 2;
        const CAN_OCCLUDE: u8 = 1 << 3;
        const SOLID_RENDER: u8 = 1 << 4;
        const FLUID_EMPTY: u8 = 1 << 7;
        const SOLID: u8 = SOLID_RENDER | CAN_OCCLUDE | BLOCKS_MOTION | FLUID_EMPTY;
        let blocks: &[u8] = br#"[
            {"name":"air","defaultState":0,"minStateId":0,"maxStateId":0},
            {"name":"stone","defaultState":1,"minStateId":1,"maxStateId":1},
            {"name":"torch","defaultState":2,"minStateId":2,"maxStateId":2},
            {"name":"redstone_lamp","defaultState":3,"minStateId":3,"maxStateId":4},
            {"name":"redstone_wire","defaultState":5,"minStateId":5,"maxStateId":5},
            {"name":"mymod:void_lantern","defaultState":6,"minStateId":6,"maxStateId":6}
        ]"#;
        // Six-byte fact rows: [flags, luminance<<4|opacity, sturdy, 0,0,0].
        let mut facts = Vec::new();
        let mut row = |flags: u8, lum_opacity: u8, sturdy: u8| {
            facts.extend_from_slice(&[flags, lum_opacity, sturdy, 0, 0, 0]);
        };
        row(IS_AIR | FLUID_EMPTY, 0x00, 0x00); // 0 air
        row(SOLID, 0x0f, 0x3f); // 1 stone
        row(FLUID_EMPTY, 0xe0, 0x00); // 2 torch, luminance 14
        row(SOLID, 0x0f, 0x3f); // 3 lamp unlit
        row(SOLID, 0xff, 0x3f); // 4 lamp lit, luminance 15
        row(FLUID_EMPTY, 0x00, 0x00); // 5 redstone wire
        row(FLUID_EMPTY, 0xf0, 0x00); // 6 modded lantern, luminance 15
        megu_kernel::Registry::from_parts(blocks, None, &facts).expect("synthetic pack must parse")
    }

    /// Empty 24-section air column at `(0, 0)` bound to `state_count`
    /// so `get_block_state`/`apply_block_update` cover the scan box.
    fn seed_air_column(live: &mut LiveSession, state_count: usize) {
        use megu_client::world::{ChunkColumn, StoredSection};
        let sections = (0..live.world.dimension.section_count())
            .map(|_| StoredSection {
                palette: megu_kernel::PalettedSection::new_single_bounded(0, state_count)
                    .expect("air state must fit the bound"),
                block_light: [0; megu_client_mesh::LIGHT_BYTES],
                sky_light: [0xff; megu_client_mesh::LIGHT_BYTES],
                biomes: [0; 64],
                has_non_air: false,
            })
            .collect();
        live.world.columns.insert(
            (0, 0),
            ChunkColumn {
                cx: 0,
                cz: 0,
                sections,
            },
        );
    }

    #[test]
    fn collect_dynamic_lights_scans_placed_luminous_blocks() {
        let (mut live, _tx) = session();
        live.vanilla_plus_cfg.nature.enabled = true;
        let registry = luminous_test_registry();
        live.world.state_count = registry.state_count();
        live.world.registry = Some(registry);
        let state_count = live.world.state_count;
        seed_air_column(&mut live, state_count);

        // Torch on the floor, lit + unlit lamps, powered wire and a
        // modded lantern — plus a stone floor under them all.
        for x in -8..=8 {
            live.world
                .apply_block_update([x, 63, 0], 1)
                .expect("floor placement");
        }
        live.world.apply_block_update([0, 64, 0], 2).unwrap();
        live.world.apply_block_update([2, 64, 0], 4).unwrap();
        live.world.apply_block_update([-2, 64, 0], 3).unwrap();
        live.world.apply_block_update([4, 64, 0], 5).unwrap();
        live.world.apply_block_update([6, 64, 0], 6).unwrap();

        let lights = live.collect_dynamic_lights([0.0, 65.0, 0.0]);
        // Torch flame tip at the exact `torch_flame_world_offset`
        // standing origin [0.5, 0.625, 0.5], not the block centre.
        assert!(
            lights.iter().any(|l| l.pos == [0.5, 64.625, 0.5]),
            "placed torch must radiate from its flame tip: {lights:?}"
        );
        // Lit redstone lamp at the resolved block origin.
        assert!(
            lights.iter().any(|l| l.pos == [2.5, 64.5, 0.5]),
            "lit lamp must emit from its block origin: {lights:?}"
        );
        // Powered signal components glow subtly from the floor plane.
        let wire = lights
            .iter()
            .find(|l| (l.pos[0] - 4.5).abs() < 0.3 && l.pos[1] < 64.5);
        assert!(
            wire.is_some_and(|l| l.rgb[0] > 0.9 && l.rgb[1] < 0.3),
            "powered wire must glow crimson at floor height: {lights:?}"
        );
        // The modded lantern resolves through the same universal path.
        assert!(
            lights.iter().any(|l| l.pos == [6.5, 64.5, 0.5]),
            "modded luminous block must emit: {lights:?}"
        );
        // The unlit lamp state stays dark — the lit gate still applies.
        assert!(
            !lights.iter().any(|l| (l.pos[0] + 1.5).abs() < 0.6),
            "unlit lamp must not emit: {lights:?}"
        );
        // Torch flame is warm, lamp is warm-white, count is bounded.
        let torch = lights.iter().find(|l| l.pos == [0.5, 64.625, 0.5]).unwrap();
        assert_eq!(torch.rgb, [1.0, 0.62, 0.22]);
    }

    /// A torch-floored scan box: air column at `(0, 0)`, torch at
    /// `[0, 64, 0]`, lit lamp at `[2, 64, 0]`.
    fn seed_placed_light_fixture(live: &mut LiveSession) {
        live.vanilla_plus_cfg.nature.enabled = true;
        let registry = luminous_test_registry();
        live.world.state_count = registry.state_count();
        live.world.registry = Some(registry);
        let state_count = live.world.state_count;
        seed_air_column(live, state_count);
        live.world.apply_block_update([0, 64, 0], 2).unwrap();
        live.world.apply_block_update([2, 64, 0], 4).unwrap();
    }

    #[test]
    fn placed_light_cache_hits_on_steady_camera_and_preserves_flicker() {
        let (mut live, _tx) = session();
        seed_placed_light_fixture(&mut live);

        // First frame scans (miss); the next frames at the same
        // integer block — different sub-block camera coords — must
        // all hit with zero rescans.
        live.world.time_of_day = 0;
        let first = live.collect_dynamic_lights([0.4, 65.4, 0.4]);
        assert_eq!(live.placed_light_cache_stats(), (0, 1));
        let second = live.collect_dynamic_lights([0.9, 65.9, 0.9]);
        let third = live.collect_dynamic_lights([0.0, 65.0, 0.0]);
        assert_eq!(live.placed_light_cache_stats(), (2, 1));
        // Positions/colors freeze at scan time and re-emit identically.
        for lights in [&second, &third] {
            assert_eq!(
                lights
                    .iter()
                    .map(|l| (l.pos, l.rgb, l.radius))
                    .collect::<Vec<_>>(),
                first
                    .iter()
                    .map(|l| (l.pos, l.rgb, l.radius))
                    .collect::<Vec<_>>()
            );
        }
        assert!(first.iter().any(|l| l.pos == [0.5, 64.625, 0.5]));

        // A forced rescan must reproduce the cached output bit-for-bit
        // (same tick_time_sec, same camera).
        live.placed_light_scan.get_mut().valid = false;
        let rescanned = live.collect_dynamic_lights([0.4, 65.4, 0.4]);
        assert_eq!(
            first
                .iter()
                .map(|l| (l.pos, l.rgb, l.radius, l.intensity))
                .collect::<Vec<_>>(),
            rescanned
                .iter()
                .map(|l| (l.pos, l.rgb, l.radius, l.intensity))
                .collect::<Vec<_>>()
        );
        assert_eq!(live.placed_light_cache_stats().1, 2);

        // Advancing `tick_time_sec` stays on the hit path but flicker
        // still animates — the open-flame modulation re-evaluates per
        // frame instead of freezing at scan time.
        let torch = |lights: &[megu_client_mesh::vfx_engine::DynamicColoredLight]| {
            lights
                .iter()
                .find(|l| l.pos == [0.5, 64.625, 0.5])
                .map(|l| l.intensity)
                .expect("torch light present")
        };
        let mut distinct = std::collections::BTreeSet::new();
        for &tick in &[0_i64, 37, 91, 153, 611] {
            live.world.time_of_day = tick;
            let lights = live.collect_dynamic_lights([0.4, 65.4, 0.4]);
            distinct.insert(torch(&lights).to_bits());
        }
        assert!(
            distinct.len() > 1,
            "cached flicker must keep animating with tick time"
        );
        assert_eq!(live.placed_light_cache_stats().1, 2);
    }

    #[test]
    fn placed_light_cache_invalidates_on_block_place_break_and_chunk_unload() {
        let (mut live, _tx) = session();
        live.vanilla_plus_cfg.nature.enabled = true;
        let registry = luminous_test_registry();
        live.world.state_count = registry.state_count();
        live.world.registry = Some(registry);
        let state_count = live.world.state_count;
        seed_air_column(&mut live, state_count);

        // Baseline: all-air box scans once, then hits.
        let baseline = live.collect_dynamic_lights([0.0, 65.0, 0.0]);
        assert!(baseline.is_empty());
        let _ = live.collect_dynamic_lights([0.0, 65.0, 0.0]);
        assert_eq!(live.placed_light_cache_stats(), (1, 1));

        // Placing a torch under a still camera must invalidate on the
        // very next frame — the world mutation counters move
        // synchronously with `apply_block_update`.
        live.world.apply_block_update([0, 64, 0], 2).unwrap();
        let lit = live.collect_dynamic_lights([0.0, 65.0, 0.0]);
        assert!(
            lit.iter().any(|l| l.pos == [0.5, 64.625, 0.5]),
            "placed torch must emit the same frame: {lit:?}"
        );
        assert_eq!(live.placed_light_cache_stats(), (1, 2));
        let _ = live.collect_dynamic_lights([0.0, 65.0, 0.0]);
        assert_eq!(live.placed_light_cache_stats(), (2, 2));

        // Breaking it invalidates just as fast.
        live.world.apply_block_update([0, 64, 0], 0).unwrap();
        let dark = live.collect_dynamic_lights([0.0, 65.0, 0.0]);
        assert!(dark.is_empty(), "broken torch must go dark: {dark:?}");
        assert_eq!(live.placed_light_cache_stats(), (2, 3));

        // Relight, then unload the whole column: every overlapping
        // section disappears, so the cache drops and the box goes dark.
        live.world.apply_block_update([0, 64, 0], 2).unwrap();
        let _ = live.collect_dynamic_lights([0.0, 65.0, 0.0]);
        let _ = live.collect_dynamic_lights([0.0, 65.0, 0.0]);
        assert_eq!(live.placed_light_cache_stats(), (3, 4));
        live.world.unload_chunk(0, 0);
        let unloaded = live.collect_dynamic_lights([0.0, 65.0, 0.0]);
        assert!(
            unloaded.is_empty(),
            "unloaded chunk emits no lights: {unloaded:?}"
        );
        assert_eq!(live.placed_light_cache_stats(), (3, 5));
    }

    #[test]
    fn placed_light_cache_invalidates_on_camera_block_crossing() {
        let (mut live, _tx) = session();
        seed_placed_light_fixture(&mut live);

        let _ = live.collect_dynamic_lights([0.9, 65.0, 0.9]);
        assert_eq!(live.placed_light_cache_stats(), (0, 1));
        // Sub-block drift inside block 0 stays cached.
        let _ = live.collect_dynamic_lights([0.99, 65.9, 0.99]);
        assert_eq!(live.placed_light_cache_stats(), (1, 1));
        // Crossing into block 1 re-anchors the ±8 window: one miss,
        // then hits again on the new center.
        let _ = live.collect_dynamic_lights([1.1, 65.0, 0.9]);
        assert_eq!(live.placed_light_cache_stats(), (1, 2));
        let _ = live.collect_dynamic_lights([1.9, 65.0, 0.9]);
        assert_eq!(live.placed_light_cache_stats(), (2, 2));
        // Moving one block in y/z re-anchors as well.
        let _ = live.collect_dynamic_lights([1.9, 66.1, 0.9]);
        let _ = live.collect_dynamic_lights([1.9, 66.1, -0.1]);
        assert_eq!(live.placed_light_cache_stats(), (2, 4));
    }

    #[test]
    fn placed_light_overlapping_sections_always_bounded_by_eight() {
        use std::collections::BTreeSet;
        // For every `rem_euclid(16)` residue the ±8 axis window spans
        // exactly two sections whose local ranges union to the full
        // 17-cell window — so the 3-D box always overlaps exactly 8.
        for base in [-32, -16, 0, 16, 160] {
            for rem in 0..16 {
                let c = base + rem;
                let axis = placed_scan_axis(c);
                assert_eq!(axis.len(), 2);
                assert_eq!(axis[0].0 + 1, axis[1].0);
                let mut cells = BTreeSet::new();
                for (s, range) in &axis {
                    for l in range.clone() {
                        cells.insert(s * 16 + l);
                    }
                }
                let expect: BTreeSet<i32> =
                    (c - PLACED_LIGHT_SCAN_RADIUS..=c + PLACED_LIGHT_SCAN_RADIUS).collect();
                assert_eq!(cells, expect, "axis coverage for c={c}");
            }
        }
        // The 3-D sweep touches exactly eight distinct section coords
        // for a spread of centers (including negative coords).
        for center in [0, 7, 8, 15, -1, -9, 64, -64, 1000] {
            let mut coords = BTreeSet::new();
            for (sy, _) in placed_scan_axis(center) {
                for (sz, _) in placed_scan_axis(center) {
                    for (sx, _) in placed_scan_axis(center) {
                        coords.insert((sx, sy, sz));
                    }
                }
            }
            assert_eq!(coords.len(), 8, "center={center}");
        }
    }

    #[test]
    fn entity_contact_shadow_quads_ground_mobs_at_their_feet() {
        let (mut live, _tx) = session();
        live.vanilla_plus_cfg.nature.enabled = true;
        let state_count = live.world.state_count;
        seed_air_column(&mut live, state_count);
        // Solid floor: fill the whole section under y=64 with an opaque
        // builtin state so the collision probe reports a top of 64.0.
        {
            use megu_client::world::StoredSection;
            let floor: StoredSection = StoredSection {
                palette: megu_kernel::PalettedSection::new_single_bounded(5, state_count).unwrap(),
                block_light: [0; megu_client_mesh::LIGHT_BYTES],
                sky_light: [0x00; megu_client_mesh::LIGHT_BYTES],
                biomes: [0; 64],
                has_non_air: true,
            };
            let col = live.world.columns.get_mut(&(0, 0)).unwrap();
            // y in [48, 63] -> section index 7 for min_y = -64.
            col.sections[7] = floor;
        }

        // Zombie (type 124, biped) stands on the floor.
        add_entity(&mut live, 30, 124, [0.0, 64.0, 0.0]);
        // Item drop (type 58) never gets a contact shadow.
        add_entity(&mut live, 31, 58, [2.0, 64.0, 0.0]);

        let quads = live.entity_contact_shadow_quads();
        assert_eq!(quads.len(), 1, "only the mob grounds: {quads:?}");
        let quad = quads[0];
        assert_eq!(quad.entity_id, 30);
        assert_eq!(quad.normal, [0.0, 1.0, 0.0]);
        assert!(
            quad.positions.iter().all(|p| p[1] == 64.015),
            "shadow plane must float 0.015 above the floor: {:?}",
            quad.positions
        );
        // Width follows `rig.width * 0.85` (zombie: 0.6 -> 0.51 span).
        let span = quad.positions[1][0] - quad.positions[0][0];
        assert!(
            (span - 0.51).abs() < 1e-4,
            "shadow span must track rig width: {span}"
        );
        assert_eq!(quad.height_above_floor, 0.0);
        assert!(
            (quad.rgba[3] - 0.30).abs() < 1e-4,
            "grounded alpha must be full: {:?}",
            quad.rgba
        );

        // Hopping fades the shadow; airborne > 2.5 drops it entirely.
        live.set_entity_position(30, [0.0, 65.0, 0.0]);
        let quads = live.entity_contact_shadow_quads();
        assert_eq!(quads.len(), 1);
        assert!(
            (quads[0].rgba[3] - 0.18).abs() < 1e-3,
            "1-block hop must fade the shadow: {:?}",
            quads[0].rgba
        );
        live.set_entity_position(30, [0.0, 67.0, 0.0]);
        assert!(live.entity_contact_shadow_quads().is_empty());
    }

    #[test]
    fn enchanted_item_glint_modulates_3d_quad_tint() {
        let (live, _tx) = session();
        let mut enchanted = stack(item_id("minecraft:diamond_sword"));
        enchanted
            .added
            .push(megu_client::protocol::ItemComponentPatch {
                type_id: 9, // minecraft:enchantments
                data: vec![1, 0x1d, 0x05, 0x00],
            });
        let plain = stack(item_id("minecraft:diamond_sword"));

        assert!(item_stack_has_glint(&enchanted));
        assert!(!item_stack_has_glint(&plain));

        let base = [0.5, 0.5, 0.5];
        let glinted = live.enchanted_item_quad_tint(&enchanted, base, [0.0, 0.0]);
        assert!(
            glinted.iter().zip(base).any(|(t, b)| *t > b),
            "enchanted tint must pick up the glint: {glinted:?} vs {base:?}"
        );
        // The shimmer stays a subtle violet-lavender highlight.
        assert!(glinted[2] >= glinted[1], "glint stays violet: {glinted:?}");
        // Non-enchanted stacks pass the base tint through untouched.
        assert_eq!(
            live.enchanted_item_quad_tint(&plain, base, [0.0, 0.0]),
            base
        );
    }

    #[test]
    fn entity_enchanted_item_glint_reads_drops_and_equipment() {
        let (mut live, _tx) = session();
        // Dropped enchanted stack: `minecraft:item` (type 58) +
        // metadata index 8 carrying a component-patched slot.
        add_entity(&mut live, 40, 58, [0.0, 64.0, 0.0]);
        let mut item_meta = vec![8u8, 7];
        item_meta.extend_from_slice(&{
            let mut wire = Vec::new();
            write_varint(&mut wire, 1); // stack count
            write_varint(&mut wire, item_id("minecraft:diamond_sword"));
            write_varint(&mut wire, 1); // added component count
            write_varint(&mut wire, 0); // removed component count
            write_varint(&mut wire, 9); // enchantments component id
            wire.extend_from_slice(&[1, 0x1d, 0x05, 0x00]); // 1 entry, hidden=false
            wire
        });
        item_meta.push(0xFF);
        live.apply_event(ClientPlayEvent::Play(PlayEvent::SetEntityData {
            eid: 40,
            raw_entries: item_meta,
        }))
        .unwrap();
        assert!(live.entity_enchanted_item_glint(40));

        // A plain drop reports no glint.
        add_entity(&mut live, 41, 58, [1.0, 64.0, 0.0]);
        let mut plain_meta = vec![8u8, 7];
        plain_meta.extend_from_slice(&slot_wire(item_id("minecraft:diamond_sword")));
        plain_meta.push(0xFF);
        live.apply_event(ClientPlayEvent::Play(PlayEvent::SetEntityData {
            eid: 41,
            raw_entries: plain_meta,
        }))
        .unwrap();
        assert!(!live.entity_enchanted_item_glint(41));
    }

    #[test]
    fn voxel_clouds_follow_the_atmosphere_gate() {
        let (mut live, _tx) = session();
        live.vanilla_plus_cfg.nature.enabled = true;
        live.refresh_voxel_clouds();
        assert!(
            live.subworlds.get(CLOUD_SUBWORLD_ID).is_some(),
            "overworld default must emit a layered cloud sheet"
        );

        // Nether/End semantics: no skylight -> no cloud sheet.
        live.world.dimension.has_skylight = false;
        live.refresh_voxel_clouds();
        assert!(
            live.subworlds.get(CLOUD_SUBWORLD_ID).is_none(),
            "skylight-less dimension must drop the cloud sheet"
        );

        // Back to the overworld: clouds return ...
        live.world.dimension.has_skylight = true;
        live.refresh_voxel_clouds();
        assert!(live.subworlds.get(CLOUD_SUBWORLD_ID).is_some());

        // ... then the vacuum-orbit blend (zero atmosphere density)
        // removes them again.
        live.atmosphere_blender
            .set_target(AtmosphereParams::vacuum_orbit());
        live.atmosphere_blender.snap_to_target();
        live.refresh_voxel_clouds();
        assert!(
            live.subworlds.get(CLOUD_SUBWORLD_ID).is_none(),
            "vacuum atmosphere must suppress voxel clouds"
        );
    }
}

// Re-opened for the hybrid cloud deck tests — kept adjacent to the
// implementation so the required names stay easy to audit.
#[cfg(test)]
mod cloud_deck_tests {
    use super::*;

    /// Offline session with a hybrid cloud field over a camera where
    /// `generate_layered_voxel_clouds` emits cells — same "walk until a
    /// sheet exists" search the existing cloud-cache test uses.
    fn hybrid_session() -> LiveSession {
        let mut live = LiveSession::offline_with_null_audio();
        live.vanilla_plus_cfg.nature.enabled = true;
        live.vanilla_plus_cfg.nature.layered_voxel_clouds = true;
        live.cloud_deck_mode = CloudDeckMode::Hybrid;
        live.world.time_of_day = 6000;
        for step in 0..32 {
            live.pose = Some(PlayerPose {
                feet: [0.0, 80.0, step as f64 * 96.0],
                yaw: 0.0,
                pitch: 0.0,
            });
            live.refresh_voxel_clouds();
            if live.subworlds.get(CLOUD_SUBWORLD_ID).is_some() {
                break;
            }
        }
        live
    }

    #[test]
    fn hybrid_cloud_decks_parallax_and_gentle_bob_reuse_mesh() {
        let mut live = hybrid_session();
        for &id in &CLOUD_DECK_IDS {
            assert!(
                live.subworlds.get(id).is_some(),
                "hybrid mode must keep deck {id:#x} resident"
            );
        }
        // Deck alphas follow the cumulus > alto > cirrus contract.
        let alphas: Vec<f32> = CLOUD_DECK_IDS
            .iter()
            .map(|&id| live.subworlds.get(id).unwrap().alpha)
            .collect();
        assert!(
            alphas[0] > alphas[1] && alphas[1] > alphas[2],
            "deck opacities must descend cumulus>alto>cirrus: {alphas:?}"
        );
        assert!(
            (alphas[0] - 0.82).abs() < 1e-6
                && (alphas[1] - 0.58).abs() < 1e-6
                && (alphas[2] - 0.35).abs() < 1e-6,
            "deck opacities must be 0.82/0.58/0.35: {alphas:?}"
        );
        // Hybrid geometry carries MAT_FLAG_CLOUD on every vertex.
        assert!(
            live.subworlds
                .get(CLOUD_SUBWORLD_ID)
                .unwrap()
                .vertices
                .iter()
                .all(|v| v.material_flags() == MAT_FLAG_CLOUD),
            "hybrid cloud vertices must tag MAT_FLAG_CLOUD"
        );

        // Parallax + bob: five sub-cell ticks must be transform-only
        // hits (no geometry rebuild) with distinct per-deck X slides and
        // bounded vertical motion.
        let base_reuses = live.cloud_mesh_reuses;
        let mut x_slides: [Vec<f32>; 3] = [Vec::new(), Vec::new(), Vec::new()];
        let mut y_lifts: [Vec<f32>; 3] = [Vec::new(), Vec::new(), Vec::new()];
        for _ in 0..5 {
            live.world.time_of_day += 1;
            live.refresh_voxel_clouds();
            for (deck, &id) in CLOUD_DECK_IDS.iter().enumerate() {
                let t = live.subworlds.get(id).unwrap().transform.position;
                x_slides[deck].push(t[0]);
                y_lifts[deck].push(t[1]);
            }
        }
        assert!(
            live.cloud_mesh_reuses >= base_reuses + 12,
            "in-cell parallax must reuse cached meshes: {}",
            live.cloud_mesh_reuses
        );
        // Distinct wind multipliers give distinct cumulative X slides.
        for (a, b) in [(0usize, 1usize), (1, 2), (0, 2)] {
            let delta: f32 = x_slides[a]
                .iter()
                .zip(&x_slides[b])
                .map(|(x, y)| (x - y).abs())
                .sum();
            assert!(
                delta > 1e-5,
                "decks {a} and {b} must parallax differently: {:?} vs {:?}",
                x_slides[a],
                x_slides[b]
            );
        }
        // The gentle bob stays bounded inside each deck's amplitude band
        // and never becomes dizzying (smooth small oscillation).
        for (deck, spec) in (0..3).map(cloud_deck_spec).enumerate() {
            for &y in &y_lifts[deck] {
                let bob = (y - spec.y_offset).abs();
                assert!(
                    bob <= spec.bob_amplitude + 1e-5,
                    "deck {deck} bob {bob} exceeded amplitude {}",
                    spec.bob_amplitude
                );
            }
        }
        // Decks sit at distinct heights: 0, +14, +26 bands do not overlap.
        assert!(y_lifts[0].iter().all(|&y| y.abs() <= 0.5));
        assert!(y_lifts[1].iter().all(|&y| (13.0..=15.0).contains(&y)));
        assert!(y_lifts[2].iter().all(|&y| (25.0..=27.0).contains(&y)));
    }

    #[test]
    fn cloud_subworlds_clean_up_on_disable_or_dimension_swap() {
        let mut live = hybrid_session();
        assert!(
            CLOUD_DECK_IDS
            .iter()
                .all(|&id| live.subworlds.get(id).is_some())
        );

        // Clouds: Off removes all three decks at once.
        live.cloud_deck_mode = CloudDeckMode::Off;
        live.refresh_voxel_clouds();
        assert!(
            CLOUD_DECK_IDS
                .iter()
                .all(|&id| live.subworlds.get(id).is_none()),
            "Off must remove every cloud deck"
        );

        // Vanilla mode keeps only the classic cumulus deck.
        live.cloud_deck_mode = CloudDeckMode::Vanilla;
        live.refresh_voxel_clouds();
        assert!(live.subworlds.get(CLOUD_SUBWORLD_ID).is_some());
        assert!(live.subworlds.get(CLOUD_SUBWORLD_ID_ALTO).is_none());
        assert!(live.subworlds.get(CLOUD_SUBWORLD_ID_CIRRUS).is_none());
        assert!(
            live.subworlds
                .get(CLOUD_SUBWORLD_ID)
                .unwrap()
                .vertices
                .iter()
                .all(|v| v.material_flags() == 0),
            "vanilla clouds must not tag MAT_FLAG_CLOUD"
        );

        // Back to hybrid, then a skylight-less dimension (Nether/End)
        // drops all three decks on the next refresh.
        live.cloud_deck_mode = CloudDeckMode::Hybrid;
        live.refresh_voxel_clouds();
        assert!(live.subworlds.get(CLOUD_SUBWORLD_ID_CIRRUS).is_some());
        live.world.dimension.has_skylight = false;
        live.refresh_voxel_clouds();
        assert!(
            CLOUD_DECK_IDS
                .iter()
                .all(|&id| live.subworlds.get(id).is_none()),
            "a skylight-less dimension must remove every cloud deck"
        );
    }
}

#[cfg(test)]
mod nature_ambience_cache_tests {
    use super::*;

    #[test]
    fn nature_site_cache_hits_without_dense_reads_and_invalidates_on_edit_or_move() {
        let mut live = LiveSession::offline_with_null_audio();
        live.step_nature_ambience(0.05);
        let (_, misses, reads) = live.nature_ambience_scan_stats();
        assert_eq!(misses, 1);
        live.step_nature_ambience(0.05);
        let (hits, _, reads_after_hit) = live.nature_ambience_scan_stats();
        assert_eq!(hits, 1);
        assert_eq!(
            reads_after_hit, reads,
            "cache hit must not read block states"
        );

        live.world.block_updates = live.world.block_updates.wrapping_add(1);
        live.step_nature_ambience(0.05);
        assert_eq!(
            live.nature_ambience_scan_stats().1,
            2,
            "block edit epoch must invalidate"
        );

        live.nature_env.origin[0] += 1.0;
        live.step_nature_ambience(0.05);
        assert_eq!(
            live.nature_ambience_scan_stats().1,
            3,
            "moving to another center block must invalidate"
        );
    }

    #[test]
    fn uniform_air_and_stone_sections_skip_dense_site_scans() {
        let mut live = LiveSession::offline_with_null_audio();
        let min_sy = live.world.dimension.min_section_y();
        let count = live.world.dimension.section_count();
        for cz in [-1, 0] {
            for cx in [-1, 0] {
                let sections = (0..count)
                    .map(|index| {
                        let state = if index % 2 == 0 { 0 } else { 1 };
                        megu_client::world::StoredSection {
                            palette: megu_kernel::PalettedSection::new_single_bounded(
                                state,
                                live.world.state_count,
                            )
                            .unwrap(),
                            block_light: [0; megu_client_mesh::LIGHT_BYTES],
                            sky_light: [0; megu_client_mesh::LIGHT_BYTES],
                            biomes: [0; 64],
                            has_non_air: state != 0,
                        }
                    })
                    .collect();
                live.world.columns.insert(
                    (cx, cz),
                    megu_client::world::ChunkColumn { cx, cz, sections },
                );
            }
        }
        assert_eq!(min_sy, -4);
        live.step_nature_ambience(0.05);
        assert_eq!(live.nature_ambience_scan_stats().2, 0);
    }
}

#[cfg(test)]
mod survival_wiring_tests {
    use super::*;

    #[test]
    fn player_look_at_turns_camera_toward_world_target_smoothly() {
        let mut live = LiveSession::offline_with_null_audio();
        live.pose = Some(PlayerPose {
            feet: [0.0, 64.0, 0.0],
            yaw: 90.0,
            pitch: 0.0,
        });
        live.eye_height = 1.62;
        live.apply_play(PlayEvent::PlayerLookAt {
            anchor: 1,
            target: [0.0, 65.62, 10.0],
            entity: None,
        })
        .unwrap();
        let (yaw, pitch) = live
            .poll_player_look_at(90.0, 0.0, 0.05)
            .expect("look-at target armed");
        assert!(yaw < 90.0 && yaw > 0.0, "unexpected eased yaw {yaw}");
        assert!(pitch.abs() < 0.001);
    }

    #[test]
    fn survival_state_reaches_hud_and_statistics_summary() {
        let mut live = LiveSession::offline_with_null_audio();
        live.pose = Some(PlayerPose {
            feet: [0.0, 64.0, 0.0],
            yaw: 0.0,
            pitch: 0.0,
        });
        live.world.difficulty = Some((2, true));
        live.world.server_tick_rate = 15.0;
        live.world.server_ticks_frozen = true;
        live.world.server_tick_steps = 3;
        live.world.default_spawn_position = Some(([0, 64, 10], 0.0));
        live.world.statistics.insert((8, 25), 123);
        live.world.statistics.insert((0, 7), 9);
        live.magic.mana = 9.0;
        live.magic.max_mana = 20.0;
        live.tick_gameplay_frame(0.05);
        assert_eq!(live.hud.server_difficulty, Some((2, true)));
        assert_eq!(live.hud.server_tick_rate, 15.0);
        assert!(live.hud.server_ticks_frozen);
        assert_eq!(live.hud.server_tick_steps, 3);
        let bearing = live.hud.spawn_compass_bearing_deg.unwrap();
        assert!(bearing < 3.0 || bearing > 357.0, "bearing {bearing}");
        assert_eq!(live.hud.magic.mana, 9.0);
        let summary = live.statistics_summary();
        assert!(summary.contains("2 entries"));
        assert!(summary.contains("custom 1"));
        assert!(summary.contains("largest 8#25=123"));
    }

    #[test]
    fn matching_pong_response_records_real_rtt() {
        let mut live = LiveSession::offline_with_null_audio();
        live.pending_ping_probe = Some((7, Instant::now()));
        live.apply_play(PlayEvent::PongResponse { id: 7 }).unwrap();
        assert!(live.pong_rtt_ms.is_some_and(|rtt| rtt <= 5));
        assert!(live.pending_ping_probe.is_none());
    }

    // ----------------------------------------------------------------
    // Dimension switches and respawns (0x47 / 0x12 / 0x2A residuals)
    // ----------------------------------------------------------------

    /// One-section flat dimension so the fixtures stay single-section.
    fn flat_dimension() -> DimensionMeta {
        DimensionMeta {
            name: "megu:test_flat".to_string(),
            min_y: 0,
            height: 16,
            has_skylight: false,
        }
    }

    fn flat_chunk(cx: i32, cz: i32) -> megu_client::protocol::DecodedChunk {
        use megu_client::protocol::{DecodedChunk, DecodedLightData, DecodedSection};
        DecodedChunk {
            cx,
            cz,
            sections: vec![DecodedSection {
                non_air: SECTION_CELLS as i16,
                states: Box::new([1; SECTION_CELLS]),
                biomes: [0; 64],
            }],
            block_entities: Vec::new(),
            light: DecodedLightData {
                sky_mask: Vec::new(),
                block_mask: Vec::new(),
                empty_sky_mask: Vec::new(),
                empty_block_mask: Vec::new(),
                sky_arrays: Vec::new(),
                block_arrays: Vec::new(),
            },
        }
    }

    /// A loaded flat session with one resident column.
    fn live_flat_session() -> LiveSession {
        let mut live = LiveSession::offline_with_null_audio();
        live.apply_play(PlayEvent::JoinGame {
            entity_id: 11,
            dimension: flat_dimension(),
            hardcore: false,
        })
        .unwrap();
        live.apply_play(PlayEvent::ChunkData(Box::new(flat_chunk(0, 0))))
            .unwrap();
        assert!(live.world.columns.contains_key(&(0, 0)));
        live
    }

    fn open_chest_screen(live: &mut LiveSession, window_id: i32) {
        live.apply_play(PlayEvent::OpenScreen {
            window_id,
            menu_type: 0,
            title_nbt: vec![8, 0, 5, b'C', b'h', b'e', b's', b't'],
        })
        .unwrap();
        assert_eq!(
            live.open_screen.as_ref().map(|s| s.window_id),
            Some(window_id)
        );
    }

    /// Vanilla `handleRespawn` only rebuilds the level when the packet's
    /// dimension differs; otherwise the chunks stay and just the local player
    /// is re-created. Health/food stay server-authoritative.
    #[test]
    fn same_dimension_respawn_keeps_terrain_and_resets_the_local_player() {
        let mut live = live_flat_session();
        open_chest_screen(&mut live, 3);
        live.riding_vehicle = Some(21);
        live.item_cooldowns.insert(
            5,
            ItemCooldown {
                remaining_ticks: 40,
                total_ticks: 40,
            },
        );
        live.apply_event(ClientPlayEvent::Play(PlayEvent::Respawn {
            dimension: flat_dimension(),
        }))
        .unwrap();

        assert!(
            live.world.columns.contains_key(&(0, 0)),
            "a same-dimension respawn keeps the loaded level"
        );
        assert!(
            live.open_screen.is_none(),
            "`handleRespawn` sets `minecraft.screen = null` on every respawn"
        );
        assert!(
            live.riding_vehicle.is_none(),
            "the fresh player has no vehicle"
        );
        assert!(live.item_cooldowns.is_empty(), "cooldowns are player state");
    }

    /// …and a respawn that changes dimension is a level swap: the world drops
    /// its columns and every dimension-scoped visual residual with it.
    #[test]
    fn cross_dimension_respawn_drops_terrain_and_visual_residuals() {
        let mut live = live_flat_session();
        live.entity_equipment.insert(
            (21, 0),
            ItemStackComponents {
                count: 1,
                item_id: 5,
                added: Vec::new(),
                removed: Vec::new(),
            },
        );
        live.riding_vehicle = Some(21);
        live.remote_block_destruction.insert(21, ([4, 5, 6], 3));
        live.item_cooldowns.insert(
            5,
            ItemCooldown {
                remaining_ticks: 40,
                total_ticks: 40,
            },
        );
        let nether = DimensionMeta {
            name: "minecraft:the_nether".to_string(),
            min_y: 0,
            height: 256,
            has_skylight: false,
        };
        live.apply_event(ClientPlayEvent::Play(PlayEvent::Respawn {
            dimension: nether,
        }))
        .unwrap();

        assert!(
            live.world.columns.is_empty(),
            "a dimension change swaps the level"
        );
        assert!(
            live.entity_equipment.is_empty(),
            "old models' gear must not leak"
        );
        assert!(live.riding_vehicle.is_none(), "no dead vehicle anchor");
        assert!(
            live.remote_block_destruction.is_empty(),
            "crack overlays are dimension-scoped"
        );
        assert!(
            live.item_cooldowns.is_empty(),
            "cooldowns do not cross worlds"
        );
    }

    /// Vanilla `handleContainerClose` ignores the packet's window id and
    /// closes whatever screen is open (`clientSideCloseContainer`).
    #[test]
    fn container_close_closes_the_open_screen_whatever_the_packet_id() {
        let mut live = live_flat_session();
        open_chest_screen(&mut live, 3);
        live.apply_event(ClientPlayEvent::Play(PlayEvent::ContainerClose {
            window_id: 9,
        }))
        .unwrap();
        assert!(
            live.open_screen.is_none(),
            "a mismatched window id still closes the open container"
        );
    }

    /// A click the worker queue refuses must not leave a predicted stack on
    /// screen: the window restores its slots from authority instead.
    #[test]
    fn refused_container_click_is_counted_for_the_window_to_resync() {
        // A two-slot worker queue: the overflow clicks are refused, exactly
        // the `Err(TrySendError::Full)` the ghost-item guard keys off.
        let (_incoming_tx, incoming) = mpsc::sync_channel(2);
        let (outgoing, _outgoing_rx) = mpsc::sync_channel(2);
        let mut live = LiveSession::with_channels(incoming, outgoing);
        open_chest_screen(&mut live, 3);
        let attempts = 8u64;
        let mut queued = 0;
        for _ in 0..attempts {
            if live.container_click(3, 0, 5, 0, 0, vec![(5, vec![1, 0, 1])], vec![1, 0, 0]) {
                queued += 1;
            }
        }
        assert!(queued >= 1, "at least one click fits the bounded queue");
        assert!(
            live.dropped_container_clicks >= 1,
            "overflowing clicks are counted for the resync"
        );
        assert_eq!(
            queued + live.dropped_container_clicks,
            attempts,
            "every attempt is either queued or counted as dropped"
        );
    }
}
