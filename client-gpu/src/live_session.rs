// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! Asynchronous live Protocol 767/777 session and renderer-facing world state.
//! Socket operations happen on a worker thread; `poll` never performs I/O.

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, SyncSender, TryRecvError, TrySendError};
use std::time::{Duration, Instant};

use megu_client::audio_out::{
    AudioCmd, AudioCmdRing, AudioRuntime, AudioSessionConfig, AudioSinkKind,
};
use megu_client::kinetic_sync::{
    CHANNEL_CREATE_KINETIC_SYNC, CHANNEL_CREATE_KINETIC_UPDATE, KineticSyncRecord,
    decode_kinetic_sync_batch,
};
use megu_client::mod_assets_meta::{
    CHANNEL_MOD_REGISTRY_SYNC, ClientModRegistry, ModMetaReport, ModTranslationTable,
    decode_mrs1_frame,
};
use megu_client::online_auth::OnlineAuthOptions;
use megu_client::protocol::{
    ClientCustomChannelEvent, ClientPlayEvent, DecodedMagicCastState, DecodedMagicSync,
    DecodedMotionTrigger, DimensionMeta, EntityMetaValue, ItemStackComponents, PlayEvent,
    PlayerInfoUpdateEntry, ProtocolClient, SeasonWireSync, TeleportSync, TransferTarget,
    WireReader, WireSoundEvent, WireStopSound, decode_item_stack_components,
    handle_transfer_packet, offline_uuid, pack_position, parse_entity_metadata, parse_uuid,
    write_string,
};
use megu_client::render::ClientVfxHost;
use megu_client::render_graph::GpuPipelineKind;
use megu_client::session::{AudioStats, DeathLoopState, WorldAcousticSnapshot};
use megu_client::spatial_audio::{SpatialAudioPreset, SpatialSubtitleCue, SubtitleDirection};
use megu_client::world::{BlockTable, WorldState, pack_vertices, sort_translucent};
use megu_client_mesh::asset_pipeline::{CompiledAssetBundle, ResourcePackLayer};
use megu_client_mesh::atlas::{AtlasMip, PackedAtlas, SpriteAnimation, SpritePlacement};
use megu_client_mesh::builtin_shadow_wgsl::BuiltinShadowQuality;
use megu_client_mesh::entity_rig::{EntityRigSpec, classify_entity_rig};
use megu_client_mesh::mesher::{
    BlockVisualEnhancementResult, BlockVisualInput, VanillaPlusPipelineConfig,
};
use megu_client_mesh::model_baker::ResourcePackModelStack;
use megu_client_mesh::model_baker::blockstate::{self, BlockStateDef, BlockStateResolution};
use megu_client_mesh::tactile_effects::{
    CameraMicroOffset, EnvironmentAudioHint, FluidEntryEvent, GroundSurfaceKind, ImpactMaterial,
    LeafCanopyKind, NightSkyDrama, TactileEffectsConfig, TactileEffectsSim,
    compute_environment_audio_hint, evaluate_night_sky_drama,
};
use megu_client_mesh::vanilla_plus_nature::{
    ActiveSeasonKind, CLOUD_CELL_BLOCKS, CLOUD_SCROLL_BLOCKS_PER_TICK, CloudLighting,
    NatureAmbienceEnv, NatureAmbienceState, NatureModRegistry, SeasonAmbienceInput,
    compute_cloud_lighting, generate_layered_voxel_clouds, tick_nature_ambience,
};
use megu_client_mesh::{
    BuiltSectionMesh, DIRECTION_BUCKETS, DirectionBuckets, SectionCoord, box_in_frustum,
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
use megu_ui::create_goggles_hud::GogglesTargetTelemetry;
use megu_ui::death_screen::DeathScreenState;
use megu_ui::magic_hud::{CastBarState, CooldownSlotState, MagicHudState, StatusBadgeState};
use megu_ui::state::{HudState, MiniHudPreset, UiState};
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
    voxel_clouds_to_geometry,
};
use crate::subworld_render::{
    InstancedPartBatch, InstancedPartRecord, SubWorldRenderBank, SubWorldRenderEntry,
    SubWorldTransform, TerrainVertex, box_mesh, quat_from_axis_angle,
};
use crate::subworld_sync::{
    decode_sync_frame, encode_helm_cmd, SubWorldHelmCmd, SUBWORLD_HELM_CHANNEL,
    SUBWORLD_SYNC_CHANNEL,
};
use hw_adaptive_engine::{
    ComputeKernelMode, ComputeStrategy, DomainKind, DomainSpecializationPlan, HostHardwareCaps,
    KernelAutotuner, PagedSectionPool, SpeculativeChunkPrefetcher, SpeculativeState,
    UnifiedOptimizationPlan, build_unified_optimization_plan, detect_host_hardware_caps,
    detect_universal_hardware_profile, select_compute_strategy,
};
use sys_metrics::{CpuSampler, FrameTimingStats, sample_host_metrics};

use crate::mesh_pool::{MeshJob, MeshJobPool, MeshResult, SectionMeshLane};
use crate::renderer::{
    ExternalSectionMesh, GpuRenderer, external_packed_digest, translucent_order_digest,
};

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
/// the mini-HUD is enabled. The `/proc` + DRM sysfs reads are throttled to
/// 2 Hz; frame-time statistics still update every frame and the latest
/// [`sys_metrics::HostMetricsSnapshot`] is reused between samples.
pub const MINIHUD_SYS_SAMPLE_INTERVAL_MS: u64 = 500;

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
    /// Sneak key state — rides `player_input`'s shift bit (which doubles
    /// as the dismount flag while riding) and `interact`'s
    /// secondary-action flag.
    pub sneak: bool,
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
    Movement(PlayerMovement),
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
        window_id: u8,
        button_id: i32,
    },
    /// `container_close` — client-initiated window close.
    ContainerClose {
        window_id: u8,
    },
    /// `seen_advancements` — the advancements screen opened (`Some(tab)`)
    /// or closed (`None`), per vanilla's screen-notification packet.
    SeenAdvancements {
        tab: Option<String>,
    },
    /// `place_recipe` — the recipe-book/JEI `+` fill request: window id,
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
    /// Versioned Play C2S plugin/custom payload.
    SendCustomPayload { channel: String, payload: Vec<u8> },
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
/// UI sync each frame (`record_frame_and_refresh_minihud` refreshes the
/// `looking_at_*` picks first).
#[derive(Clone, Debug)]
pub struct LiveHudFeed {
    pub scoreboard: LiveScoreboard,
    pub title_revision: u64,
    pub title_times: Option<[i32; 3]>,
    pub title_text: Option<String>,
    pub subtitle_text: Option<String>,
    pub looking_at_block: Option<([i32; 3], String)>,
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
    let short = name.rsplit(':').next().unwrap_or(name);
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
    let short = name.rsplit(':').next().unwrap_or(name);
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

/// The atlas sprite id standing in for a whole block: the `particle`
/// texture of the model `block`'s blockstate resolves to under synthetic
/// representative props — vanilla's own stand-in for a block's look, and
/// the finest granularity the packed-vertex path (one sprite per state
/// id) can express. Candidates fall through `all`/`side`/`up`/`front`/
/// `top` then any resolved texture; `has_sprite` filters to ids actually
/// packed in the atlas so one missing texture cannot fail the whole
/// state table upload.
fn representative_texture(
    meta: &CompiledAssetMeta,
    block: &str,
    has_sprite: &dyn Fn(&str) -> bool,
) -> Option<String> {
    let def = meta
        .blockstates
        .get(block)
        .or_else(|| meta.blockstates.get(&format!("minecraft:{block}")))?;
    let props = blockstate::representative_props(def);
    let specs = match blockstate::resolve_blockstate(def, &props, [0, 0, 0]).ok()? {
        BlockStateResolution::Variant(variant) => vec![variant],
        BlockStateResolution::Multipart(variants) => variants,
    };
    for spec in &specs {
        let Ok(textures) = meta.model_stack.resolved_textures(&spec.model) else {
            continue;
        };
        for key in ["particle", "all", "side", "up", "front", "top"] {
            match textures.get(key) {
                Some(texture) if has_sprite(texture) => return Some(texture.clone()),
                _ => {}
            }
        }
        for texture in textures.values() {
            if has_sprite(texture) {
                return Some(texture.clone());
            }
        }
    }
    None
}

/// Every registered block-state id mapped to its block's representative
/// atlas sprite — the data [`GpuRenderer::set_block_state_sprites`]
/// uploads. Blocks are resolved once and the result shared across their
/// state ids (the kernel registry names blocks per id but retains no
/// per-state properties).
fn state_sprite_assignments(
    world: &WorldState,
    mod_registry: Option<&ClientModRegistry>,
    meta: &CompiledAssetMeta,
    renderer: &GpuRenderer,
) -> Vec<(u32, String)> {
    let mut out = Vec::new();
    let mut per_block: HashMap<&str, Option<String>> = HashMap::new();
    for state in 0..world.state_count as u32 {
        let Some(block) = state_block_name(world, mod_registry, state) else {
            continue;
        };
        let texture = per_block.entry(block).or_insert_with(|| {
            representative_texture(meta, block, &|name| {
                renderer.atlas_sprite_index(name).is_some()
            })
        });
        if let Some(texture) = texture {
            out.push((state, texture.clone()));
        }
    }
    out
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

/// Owns UI-facing world data on the event-loop thread. The worker owns its socket.
pub struct LiveSession {
    incoming: Receiver<WorkerMessage>,
    outgoing: SyncSender<WorkerCommand>,
    _offline_sender: Option<SyncSender<WorkerMessage>>,
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
    /// Entity the crosshair ray currently rests on (see `refresh_target`).
    pub looking_at_entity: Option<i32>,
    /// Server's open-container instruction, if any.
    pub open_screen: Option<LiveOpenScreen>,
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
    /// [`MINIHUD_SYS_SAMPLE_INTERVAL_MS`] of accumulated frame time while
    /// the mini-HUD is enabled.
    mini_hud_host: Option<sys_metrics::HostMetricsSnapshot>,
    /// Frame milliseconds accumulated since the last host-metric sample.
    mini_hud_sys_accum_ms: f32,
    /// Lifetime count of `sample_host_metrics` calls — the throttled
    /// `/proc`/DRM reads the mini-HUD would otherwise do every frame.
    pub sys_sample_count: u64,
    /// Adaptive packed-nibble kernel autotuner seeded from
    /// `compute_strategy` and fed real frame times by
    /// `record_frame_and_refresh_minihud` and real mesh-batch durations
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
    /// Integer-cell key the layered voxel-cloud mesh was last generated
    /// for: `(cloud_cx, cloud_cz, scroll_cell, cloud_layers,
    /// cloud_height_bits)`. Between key hits only the sub-cell X offset
    /// changes, which slides through the existing `SubWorld` model
    /// matrix instead of rebuilding the mesh.
    cloud_mesh_key: Option<(i64, i64, i64, u32, u32)>,
    /// Quantized (1/32) cloud-lighting key for the cached mesh: weather
    /// or day-phase shifts that change the baked tint force a
    /// regenerate even inside one cell.
    cloud_mesh_lighting_key: Option<[u32; 10]>,
    /// Composed frames that reused the cached cloud mesh with a
    /// matrix-only sub-cell scroll slide.
    pub cloud_mesh_reuses: u64,
    pub atmosphere_blender: BiomeAtmosphereBlender,
    pub chunk_fades: ChunkFadeTracker,
    pub subworlds: SubWorldRenderBank,
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
    kinetic_pos_to_id: HashMap<(i32, i32, i32), u32>,
    /// Latest wire telemetry, keyed by world position.
    kinetic_telemetry: HashMap<(i32, i32, i32), KineticSyncRecord>,
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
    /// Mod-JAR `assets/*/lang` translation table loaded for `ui_locale`;
    /// `None` until first use so connect stays lazy.
    mod_translations: Option<ModTranslationTable>,
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
    /// Client-side wall-clock seconds driving `.mcmeta` sprite animation.
    /// Vanilla reticks `TextureManager` once per client tick (50 ms)
    /// regardless of server `time_of_day`, so this runs off `poll`'s own
    /// heartbeat rather than `world_time_ticks` (which wraps at 24 000
    /// and follows server `set_time` jumps).
    atlas_anim_clock: f64,
    last_atlas_anim_tick: Instant,
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
    fn new() -> Self { Self { bytes: [0; 512], len: 0 } }
    fn put(&mut self, value: &[u8]) {
        let take = value.len().min(self.bytes.len().saturating_sub(self.len));
        self.bytes[self.len..self.len + take].copy_from_slice(&value[..take]);
        self.len += take;
    }
    fn i32(&mut self, value: i32) { self.put(&value.to_le_bytes()); }
    fn u32(&mut self, value: u32) { self.put(&value.to_le_bytes()); }
    fn i64(&mut self, value: i64) { self.put(&value.to_le_bytes()); }
    fn f32(&mut self, value: f32) { self.put(&value.to_le_bytes()); }
    fn f64(&mut self, value: f64) { self.put(&value.to_le_bytes()); }
    fn flag(&mut self, value: bool) { self.put(&[u8::from(value)]); }
    fn bytes(&mut self, value: &[u8]) {
        let len = value.len().min(u16::MAX as usize).min(self.bytes.len().saturating_sub(self.len + 2));
        self.put(&(len as u16).to_le_bytes());
        self.put(&value[..len]);
    }
    fn text(&mut self, value: &str) { self.bytes(value.as_bytes()); }
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
        E::JoinGame { entity_id, dimension, hardcore } => { out.i32(*entity_id); out.text(&dimension.name); out.i32(dimension.min_y); out.i32(dimension.height); out.flag(dimension.has_skylight); out.flag(*hardcore); 1 }
        E::Respawn { dimension } => { out.text(&dimension.name); out.i32(dimension.min_y); out.i32(dimension.height); out.flag(dimension.has_skylight); 2 }
        E::KeepAlive(id) => { out.i64(*id); 3 }
        E::Teleport(t) => { out.f64(t.x); out.f64(t.y); out.f64(t.z); out.f32(t.yaw); out.f32(t.pitch); out.i32(t.flags); out.i32(t.teleport_id); 4 }
        E::ChunkBatchStart => 5,
        E::ChunkData(chunk) => {
            out.i32(chunk.cx); out.i32(chunk.cz); out.u32(chunk.sections.len().min(u32::MAX as usize) as u32);
            let mut digest = 0xcbf2_9ce4_8422_2325u64;
            for section in &chunk.sections { out.i32(i32::from(section.non_air)); for state in section.states.iter() { digest ^= u64::from(*state); digest = digest.wrapping_mul(0x100_0000_01b3); } }
            out.put(&digest.to_le_bytes()); 6
        }
        E::ChunkBatchFinished(count) => { out.i32(*count); 7 }
        E::UpdateLight { cx, cz, light } => { out.i32(*cx); out.i32(*cz); out.u32(light.sky_arrays.len() as u32); out.u32(light.block_arrays.len() as u32); 8 }
        E::BlockUpdate { pos, state_id } => { for v in pos { out.i32(*v); } out.u32(*state_id); 9 }
        E::SectionBlocksUpdate(entries) => { let n = entries.len().min(24); out.put(&[n as u8]); for (pos, state) in entries.iter().take(n) { for v in pos { out.i32(*v); } out.u32(*state); } 10 }
        E::UnloadChunk { cx, cz } => { out.i32(*cx); out.i32(*cz); 11 }
        E::SetCenterChunk { cx, cz } => { out.i32(*cx); out.i32(*cz); 12 }
        E::SetHealth { health, food, saturation } => { out.f32(*health); out.i32(*food); out.f32(*saturation); 13 }
        E::SystemChat { text, overlay } => { out.text(text); out.flag(*overlay); 14 }
        E::AddEntity { eid, uuid, entity_type, x, y, z, pitch, yaw, head_yaw, data, vx, vy, vz } => { out.i32(*eid); out.put(uuid); out.i32(*entity_type); out.f64(*x); out.f64(*y); out.f64(*z); out.f32(*pitch); out.f32(*yaw); out.f32(*head_yaw); out.i32(*data); out.f64(*vx); out.f64(*vy); out.f64(*vz); 15 }
        E::RemoveEntities { eids } => { let n = eids.len().min(80); out.put(&[n as u8]); for eid in eids.iter().take(n) { out.i32(*eid); } 16 }
        E::MoveEntityPos { eid, dx, dy, dz, on_ground } => { out.i32(*eid); out.f64(*dx); out.f64(*dy); out.f64(*dz); out.flag(*on_ground); 17 }
        E::MoveEntityPosRot { eid, dx, dy, dz, yaw, pitch, on_ground } => { out.i32(*eid); out.f64(*dx); out.f64(*dy); out.f64(*dz); out.f32(*yaw); out.f32(*pitch); out.flag(*on_ground); 18 }
        E::MoveEntityRot { eid, yaw, pitch, on_ground } => { out.i32(*eid); out.f32(*yaw); out.f32(*pitch); out.flag(*on_ground); 19 }
        E::TeleportEntity { eid, x, y, z, yaw, pitch, on_ground } => { out.i32(*eid); out.f64(*x); out.f64(*y); out.f64(*z); out.f32(*yaw); out.f32(*pitch); out.flag(*on_ground); 20 }
        E::RotateHead { eid, head_yaw } => { out.i32(*eid); out.f32(*head_yaw); 21 }
        E::SetEntityMotion { eid, vx, vy, vz } => { out.i32(*eid); out.f64(*vx); out.f64(*vy); out.f64(*vz); 22 }
        E::EntityEvent { eid, status } => { out.i32(*eid); out.put(&[*status as u8]); 23 }
        E::HurtAnimation { eid, yaw } => { out.i32(*eid); out.f32(*yaw); 24 }
        E::SetEntityData { eid, raw_entries } => { out.i32(*eid); out.bytes(raw_entries); 25 }
        E::SetTime { world_age, time_of_day } => { out.i64(*world_age); out.i64(*time_of_day); 26 }
        E::GameEvent { event, value } => { out.put(&[*event]); out.f32(*value); 27 }
        E::LevelEvent { event, pos, data, disable_relative_volume } => { out.i32(*event); for v in pos { out.i32(*v); } out.i32(*data); out.flag(*disable_relative_volume); 28 }
        E::BlockDestruction { breaker_eid, pos, stage } => { out.i32(*breaker_eid); for v in pos { out.i32(*v); } out.put(&[*stage as u8]); 29 }
        E::Explosion { x, y, z, power, records, knockback, block_interaction, small_particle_id, large_particle_id, .. } => { out.f64(*x); out.f64(*y); out.f64(*z); out.f32(*power); out.u32(records.len().min(u32::MAX as usize) as u32); for v in knockback { out.f32(*v); } out.i32(*block_interaction); out.i32(*small_particle_id); out.i32(*large_particle_id); 30 }
        E::BlockEntityData { pos, kind, nbt_bytes } => { for v in pos { out.i32(*v); } out.i32(*kind); out.bytes(nbt_bytes); 31 }
        E::ContainerSetContent { window_id, state_id, slot_count, raw_payload } => { out.put(&[*window_id]); out.i32(*state_id); out.i32(*slot_count); out.bytes(raw_payload); 32 }
        E::ContainerSetSlot { window_id, state_id, slot, raw_item } => { out.put(&[*window_id]); out.i32(*state_id); out.put(&slot.to_le_bytes()); out.bytes(raw_item); 33 }
        E::SetExperience { bar, level, total } => { out.f32(*bar); out.i32(*level); out.i32(*total); 34 }
        E::UpdateMobEffect { eid, effect_id, amplifier, duration, flags } => { out.i32(*eid); out.i32(*effect_id); out.i32(*amplifier); out.i32(*duration); out.put(&[*flags]); 35 }
        E::RemoveMobEffect { eid, effect_id } => { out.i32(*eid); out.i32(*effect_id); 36 }
        E::SetActionBarText { nbt_bytes } => { out.bytes(nbt_bytes); 37 }
        E::SetTitleText { nbt_bytes } => { out.bytes(nbt_bytes); 38 }
        E::SetSubtitleText { nbt_bytes } => { out.bytes(nbt_bytes); 39 }
        E::BossEvent { uuid, action, raw_payload } => { out.put(uuid); out.i32(*action); out.bytes(raw_payload); 40 }
        E::SetPassengers { vehicle, passengers } => { out.i32(*vehicle); let n = passengers.len().min(80); out.put(&[n as u8]); for id in passengers.iter().take(n) { out.i32(*id); } 41 }
        E::TakeItemEntity { item_entity, player, amount } => { out.i32(*item_entity); out.i32(*player); out.i32(*amount); 42 }
        E::SetCamera { eid } => { out.i32(*eid); 43 }
        E::PlayerChat { sender, index, text, nbt_bytes, .. } => { out.put(sender); out.i32(*index); out.text(text); out.bytes(nbt_bytes); 44 }
        E::DisguisedChat { text, nbt_bytes } => { out.text(text); out.bytes(nbt_bytes); 45 }
        E::Ping(id) => { out.i32(*id); 46 }
        E::BlockChangedAck(id) => { out.i32(*id); 47 }
        E::PlayerAbilities { flags, flying_speed, walking_speed } => { out.put(&[*flags]); out.f32(*flying_speed); out.f32(*walking_speed); 48 }
        E::PlayerCombatKill { player_id, message_nbt } => { out.i32(*player_id); out.bytes(message_nbt); 49 }
        E::Disconnect(reason) => { out.text(reason); 50 }
        E::Other(packet_id) => { out.i32(*packet_id); 51 }
        _ => return None,
    };
    Some(out.finish(tag))
}

impl LiveSession {
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
        let (outgoing, _rx) = mpsc::sync_channel(8);
        let mut session =
            Self::with_channels_and_sink(incoming, outgoing, AudioSinkKind::Null, paths);
        session._offline_sender = Some(tx);
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
        // `record_frame_and_refresh_minihud` feeds it real frame times.
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
        let world = WorldState::new(DimensionMeta::default(), BlockTable::load(None));
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
            looking_at_entity: None,
            open_screen: None,
            container_properties: BTreeMap::new(),
            player_abilities: LivePlayerAbilities::default(),
            server_resource_packs: VecDeque::new(),
            last_server_ping: None,
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
            use_item_sequence: 0,
            pending_interaction_seqs: BTreeMap::new(),
            highest_acknowledged_sequence: 0,
            view_center_chunk: None,
            item_cooldowns: BTreeMap::new(),
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
            asset_bundle_pending_upload: false,
            asset_discovery: AssetDiscovery::default(),
            commands: seeded_command_dispatcher(),
            player_entity_id: None,
            death: DeathLoopState::default(),
            game_mode: 0,
            respawn_screen_enabled: true,
            pending_end_credits: false,
            hud: HudState::default(),
            magic: MagicHudState::default(),
            vfx,
            motion: None,
            pose: None,
            camera_sync: None,
            terrain_dirty: false,
            dirty_terrain_coords: BTreeSet::new(),
            // Until the first sync finishes, residency is unknown — the
            // renderer needs one legacy full reconcile to establish it.
            terrain_full_sync: true,
            terrain_order_sweep: false,
            external_sections: BTreeMap::new(),
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
            mini_hud_host: None,
            mini_hud_sys_accum_ms: 0.0,
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
            cloud_mesh_key: None,
            cloud_mesh_lighting_key: None,
            cloud_mesh_reuses: 0,
            atmosphere_blender: BiomeAtmosphereBlender::new(AtmosphereParams::default()),
            chunk_fades: ChunkFadeTracker::new(),
            subworlds: SubWorldRenderBank::new(),
            subworld_sync_received_at: None,
            player_subworld_id: None,
            hud_revision: 0,
            last_synced_hud_revision: u64::MAX,
            hud_sync_skips: 0,
            kinetic_parts: BTreeMap::new(),
            kinetic_pos_to_id: HashMap::new(),
            kinetic_telemetry: HashMap::new(),
            kinetic_live_angles: HashMap::new(),
            kinetic_scratch: BTreeMap::new(),
            kinetic_network_rpm: 0.0,
            kinetic_time_secs: 0.0,
            next_kinetic_part_id: 0,
            vanilla_plus_cfg: VanillaPlusPipelineConfig::default(),
            nature_mod_registry: NatureModRegistry::default(),
            mod_registry: None,
            mod_registry_rejected: 0,
            mod_translations: None,
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
            tactile_event_tick: 0,
            tactile_last_pose: None,
            tactile_landing_impulse: 0.0,
            last_motion_delta: [0.0; 2],
            last_vfx_tick: Instant::now(),
            last_animation_tick: Instant::now(),
            atlas_anim_clock: 0.0,
            last_atlas_anim_tick: Instant::now(),
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
        if let Some(received_at) = self.subworld_sync_received_at {
            let alpha = (received_at.elapsed().as_secs_f32() / 0.05).clamp(0.0, 1.0);
            self.subworlds.interpolate_poses(alpha);
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
                self.status = SessionStatus::Disconnected(format!("mesh: {error}"));
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
                self.status = SessionStatus::Disconnected(format!("mesh: {error}"));
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
            let elapsed_ticks = (now.duration_since(self.last_vfx_tick).as_millis() / 50).min(4);
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
            for _ in 0..elapsed_ticks {
                // Time each client sim-tick step so the MiniHUD's MSPT/TPS
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
            self.last_vfx_tick = now;
            let animation_dt = now.duration_since(self.last_animation_tick).as_secs_f32();
            self.last_animation_tick = now;
            if let Err(error) = self.world.advance_animations(animation_dt.min(0.1)) {
                self.status = SessionStatus::Disconnected(format!("animation: {error:?}"));
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
        if let Some(workers) = &self.section_mesh_workers {
            workers.drain(|result| results.push(result));
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
            self.install_built_external(built, camera_position);
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
            let workers = self
                .section_mesh_workers
                .get_or_insert_with(|| MeshJobPool::new(threads));
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
    fn install_built_external(&mut self, built: BuiltSectionMesh, camera_position: [f32; 3]) {
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

    pub fn send_movement(&mut self, movement: PlayerMovement) {
        if !matches!(self.status, SessionStatus::Playing)
            || self.pose.is_none()
            || self.death.is_dead()
        {
            return;
        }
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
        let _ = self.outgoing.try_send(WorkerCommand::Movement(movement));
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
        // Placement predicts the clicked cell and the face-adjacent cell;
        // the `block_changed_ack` for this sequence reconciles both.
        self.track_pending_interaction(
            self.use_item_sequence,
            &[pos, adjacent_block_pos(pos, face)],
        );
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
    /// back to plain `interact` without a hit point).
    pub fn interact_entity(&mut self, entity_id: i32, at: Option<[f32; 3]>) {
        let _ = self.outgoing.try_send(WorkerCommand::Interact {
            target: entity_id,
            action: if at.is_some() { 2 } else { 0 },
            at,
            hand: 0,
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

    /// `set_carried_item` — tell the server which hotbar slot (0..=8) is
    /// held after a local selection change.
    pub fn set_carried_item(&mut self, slot: i32) {
        let _ = self
            .outgoing
            .try_send(WorkerCommand::SetCarriedItem { slot });
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

    /// Queue one Aeronautics helm command for the connection worker.
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
        let mut players: Vec<(String, u16)> = self
            .player_info
            .values()
            .filter(|entry| entry.listed != Some(false))
            .map(|entry| {
                let name = entry
                    .display_name_nbt
                    .as_deref()
                    .and_then(nbt_text)
                    .or_else(|| entry.name.clone())
                    .unwrap_or_else(|| uuid_label(&entry.uuid));
                (
                    name,
                    entry.latency.unwrap_or(0).clamp(0, i32::from(u16::MAX)) as u16,
                )
            })
            .collect();
        players.sort_by_key(|a| a.0.to_lowercase());
        self.hud.players = players;
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

    /// Snapshot for `Screen::Death` while dead (`None` while alive) —
    /// cause text, hardcore flag, and the XP-total "Score" line straight
    /// from the [`DeathLoopState`].
    pub fn death_screen_state(&self) -> Option<DeathScreenState> {
        self.death.is_dead().then(|| DeathScreenState {
            cause_message: self.death.cause_message().map(str::to_owned),
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

    /// `container_click` — one window transaction; `changed`/`carried` are
    /// raw `Slot` wire blobs (build simple stacks via
    /// `megu_client::protocol::encode_slot_stack`).
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
        let _ = self.outgoing.try_send(WorkerCommand::ContainerButtonClick {
            window_id,
            button_id,
        });
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
        let _ = self
            .outgoing
            .try_send(WorkerCommand::ContainerClose { window_id });
    }

    /// `place_recipe` — the JEI `+` / recipe-book grid-fill request for
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
    }

    /// Block under the crosshair ray (`raycast_blocks`, vanilla 4.5-block
    /// reach, no fluid hits) — feeds the looking-at overlay alongside
    /// `crosshair_entity`. Non-air voxels present a full-cube selection
    /// shape; the face/distance stay honest while exact per-block outline
    /// shapes are out of scope.
    pub fn crosshair_block(&self) -> Option<([i32; 3], f64)> {
        self.crosshair_block_hit()
            .map(|hit| (hit.block_pos, hit.distance))
    }

    /// Full [`RayHit`] behind [`Self::crosshair_block`] — the
    /// `qol_gameplay` looking-at projection consumes the face and
    /// cursor-in-block detail too.
    fn crosshair_block_hit(&self) -> Option<RayHit> {
        let pose = self.pose?;
        let eye = [pose.feet[0], pose.feet[1] + 1.62, pose.feet[2]];
        let yaw = f64::from(pose.yaw).to_radians();
        let pitch = f64::from(pose.pitch).to_radians();
        let direction = [
            -yaw.sin() * pitch.cos(),
            -pitch.sin(),
            yaw.cos() * pitch.cos(),
        ];
        raycast_blocks(
            eye,
            direction,
            4.5,
            FluidClipMode::None,
            |x, y, z| match self.world.get_block_state(x, y, z) {
                Some(0) | None => RayBlockShapes::default(),
                Some(_) => RayBlockShapes {
                    boxes: vec![Aabb {
                        min: [0.0; 3],
                        max: [1.0; 3],
                    }],
                    is_fluid: false,
                    is_fluid_source: false,
                },
            },
        )
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
        let columns = &self.world.columns;
        self.prefetcher.update(
            anchor_x,
            anchor_z,
            self.last_motion_delta[0],
            self.last_motion_delta[1],
            PREFETCH_VIEW_DISTANCE,
            PREFETCH_MAX_CHUNKS,
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
    /// `record_frame_and_refresh_minihud` credits the same arm the poll
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
                submerged: false,
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
        for dir in &cem_dirs {
            self.register_cem_models(dir);
        }
        match CompiledAssetBundle::build(&layers) {
            Ok(bundle) => {
                if !bundle.skipped_errors.is_empty() {
                    eprintln!(
                        "megu-client assets: {} pack entries skipped",
                        bundle.skipped_errors.len()
                    );
                }
                let (meta, pixels) = split_compiled_bundle(bundle);
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
                    // The packed-terrain shader resolves `state_id` ->
                    // sprite through `atlas_states`; without this table
                    // every quad falls back to its palette colour and no
                    // `.mcmeta` frame ever reaches the screen.
                    let assignments = state_sprite_assignments(
                        &self.world,
                        self.mod_registry.as_ref(),
                        meta,
                        renderer,
                    );
                    let refs: Vec<(u32, &str)> = assignments
                        .iter()
                        .map(|(id, sprite)| (*id, sprite.as_str()))
                        .collect();
                    if let Err(err) = renderer.set_block_state_sprites(&refs) {
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

    /// Snapshot the server-driven HUD channels for `GpuApp::sync_live_hud`.
    pub fn hud_feed(&self) -> LiveHudFeed {
        LiveHudFeed {
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
    /// Cells come from `generate_layered_voxel_clouds`; the pack's
    /// `enabled`/`layered_voxel_clouds` toggles gate emission (an empty set
    /// removes the sheet instead of leaving a stale frame).
    ///
    /// The mesh is cached on the integer cell grid `(cloud_cx, cloud_cz,
    /// scroll_cell, cloud_layers, cloud_height)`: `generate` bakes
    /// `cx*cell - scroll` into `min.x`, so feeding the cell-aligned
    /// canonical tick produces a mesh whose only drift inside one cell is
    /// the sub-cell X remainder — applied to the existing `SubWorld`
    /// model matrix via `set_transform` instead of a geometry rebuild.
    /// `cloud_mesh_reuses` counts the matrix-only slides.
    pub fn refresh_voxel_clouds(&mut self) {
        let (cam_x, cam_z) = self
            .pose
            .map(|pose| (pose.feet[0] as f32, pose.feet[2] as f32))
            .unwrap_or((0.0, 0.0));
        let time = self.world.time_of_day.max(0) as u64;
        let cell = f64::from(CLOUD_CELL_BLOCKS);
        let scroll_rate = f64::from(CLOUD_SCROLL_BLOCKS_PER_TICK);
        let scroll = time as f64 * scroll_rate;
        let scroll_cell = (scroll / cell).floor() as i64;
        // Canonical tick whose baked scroll lands on the cell grid:
        // `generate` computes `min.x = cx*cell - ticks*rate`, so this tick
        // makes the cached mesh carry only whole-cell offsets.
        let canon_ticks = ((scroll_cell as f64 * cell) / scroll_rate)
            .round()
            .clamp(0.0, u64::MAX as f64) as u64;
        let canon_scroll = canon_ticks as f64 * scroll_rate;
        let cam_x = if cam_x.is_finite() { cam_x } else { 0.0 };
        let cam_z = if cam_z.is_finite() { cam_z } else { 0.0 };
        let cloud_cx = ((f64::from(cam_x) + canon_scroll) / cell).floor() as i64;
        let cloud_cz = (f64::from(cam_z) / cell).floor() as i64;
        let nature = &self.vanilla_plus_cfg.nature;
        let key = (
            cloud_cx,
            cloud_cz,
            scroll_cell,
            u32::from(nature.effective_cloud_layers()),
            nature.effective_cloud_height().to_bits(),
        );
        // Sub-cell drift this frame: the model matrix slides the cached
        // mesh by the fraction of a cell scrolled since the last boundary.
        let slide_x = -((scroll - canon_scroll) as f32);
        let transform = SubWorldTransform {
            position: [slide_x, 0.0, 0.0],
            ..SubWorldTransform::IDENTITY
        };
        // Vanilla celestial angle: 0 = sunrise, 6000 = noon, 18000 = midnight.
        let sun_elevation = ((time % 24_000) as f32 * (std::f32::consts::TAU / 24_000.0)).sin();
        let lighting = compute_cloud_lighting(
            sun_elevation,
            self.world.rain_level,
            self.world.thunder_level,
        );
        let lighting_key = cloud_lighting_key(&lighting);
        if self.cloud_mesh_key == Some(key)
            && self.cloud_mesh_lighting_key == Some(lighting_key)
            && self.subworlds.get(CLOUD_SUBWORLD_ID).is_some()
        {
            self.subworlds.set_transform(CLOUD_SUBWORLD_ID, transform);
            self.cloud_mesh_reuses += 1;
            return;
        }
        let cells = generate_layered_voxel_clouds(nature, cam_x, cam_z, canon_ticks);
        if cells.is_empty() {
            self.subworlds.remove(CLOUD_SUBWORLD_ID);
            self.cloud_mesh_key = None;
            self.cloud_mesh_lighting_key = None;
            return;
        }
        let (vertices, indices) = voxel_clouds_to_geometry(&cells, &lighting);
        let mut entry = SubWorldRenderEntry::new(CLOUD_SUBWORLD_ID, transform, vertices, indices);
        entry.alpha = CLOUD_RENDER_ALPHA;
        self.subworlds.upsert(entry);
        self.cloud_mesh_key = Some(key);
        self.cloud_mesh_lighting_key = Some(lighting_key);
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
        *hud = self.hud.clone();
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
        for (pos, id) in &self.kinetic_pos_to_id {
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
        self.mod_translations = Some(table);
    }

    /// Live Create goggles telemetry for the kinetic part at `pos`.
    /// Dynamic mod block-state ids resolve their real `namespace:block`
    /// through the `MRS1` registry, and their `display_name` comes from
    /// the loaded mod lang table (`ui_locale`, English fallback).
    pub fn goggles_telemetry_at(&mut self, pos: (i32, i32, i32)) -> Option<GogglesTargetTelemetry> {
        let record = self.kinetic_telemetry.get(&pos)?.clone();
        let state = self.world.get_block_state(pos.0, pos.1, pos.2);
        let resolved = state.and_then(|state| {
            state_block_name(&self.world, self.mod_registry.as_ref(), state)
                .filter(|name| !name.is_empty())
                .map(|name| (state, name))
        });
        let (block_id, dynamic) = resolved
            .map(|(state, name)| {
                let vanilla = if let Some(registry) = &self.world.registry {
                    registry.state_name(state).ok()
                } else {
                    self.world.blocks.state_name(state)
                };
                (name.to_owned(), vanilla.is_none())
            })
            .unwrap_or_else(|| ("create:kinetic_part".to_owned(), false));
        let display_name = if dynamic {
            self.ensure_mod_translations();
            self.mod_translations
                .as_ref()
                .map(|table| table.translate_block(&block_id))
                .filter(|name| !name.is_empty())
        } else {
            None
        };
        Some(GogglesTargetTelemetry {
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
        })
    }

    fn upsert_kinetic_record(&mut self, record: KineticSyncRecord) {
        let pos = record.position();
        if self.kinetic_pos_to_id.contains_key(&pos) {
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
        self.kinetic_pos_to_id.insert(pos, id);
        self.kinetic_live_angles.insert(id, 0.0);
        self.kinetic_telemetry.insert(pos, record);
    }

    fn evict_kinetic_part(&mut self, pos: (i32, i32, i32)) {
        self.kinetic_telemetry.remove(&pos);
        if let Some(id) = self.kinetic_pos_to_id.remove(&pos) {
            self.kinetic_live_angles.remove(&id);
            self.remove_kinetic_part(id);
        }
    }

    fn evict_kinetic_chunk(&mut self, cx: i32, cz: i32) {
        let positions: Vec<_> = self
            .kinetic_pos_to_id
            .keys()
            .copied()
            .filter(|(x, _, z)| x.div_euclid(16) == cx && z.div_euclid(16) == cz)
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
                self.subworlds.set_transform(contraption_id, transform);
            }
            Some(_) => {
                self.subworlds
                    .set_geometry(contraption_id, vertices, indices);
                self.subworlds.set_transform(contraption_id, transform);
            }
            None => self.subworlds.upsert(SubWorldRenderEntry::new(
                contraption_id,
                transform,
                vertices,
                indices,
            )),
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
        self.subworlds.set_transform(contraption_id, transform)
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
    pub fn record_frame_and_refresh_minihud(&mut self, frame_dt_sec: f32, ui: &mut UiState) {
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
            self.mini_hud_sys_accum_ms += frame_dt_sec * 1_000.0;
        }
        self.chunk_fades.tick(frame_dt_sec);
        self.atmosphere_blender.advance(frame_dt_sec);
        self.refresh_subtitle_hud(frame_dt_sec, &mut ui.hud);
        self.refresh_looking_at();
        self.ensure_asset_bundle();
        self.refresh_voxel_clouds();

        if ui.settings.qol.mini_hud_preset() == MiniHudPreset::Off {
            return;
        }

        // `/proc` + DRM sysfs reads are throttled to
        // `MINIHUD_SYS_SAMPLE_INTERVAL_MS` of accumulated frame time; the
        // cached snapshot feeds every frame's telemetry below.
        if self.mini_hud_host.is_none()
            || self.mini_hud_sys_accum_ms >= MINIHUD_SYS_SAMPLE_INTERVAL_MS as f32
        {
            self.mini_hud_host = Some(sample_host_metrics(&mut self.metrics_probe));
            self.sys_sample_count += 1;
            self.mini_hud_sys_accum_ms = 0.0;
        }
        let host = self
            .mini_hud_host
            .as_ref()
            .expect("host metrics sampled above");
        let telemetry = sys_metrics::MiniHudData::from_telemetry(host, &self.frame_timings);
        let hud = &mut ui.hud.mini_hud_data;
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
        hud.subtitle_hud = self.hud.subtitle_hud.clone();
    }

    /// Apply a decoded packet, also used by framed protocol regression tests.
    pub fn apply_event(&mut self, event: ClientPlayEvent) -> Result<(), String> {
        if let ClientPlayEvent::Play(play) = &event {
            if let Some((tag, payload, len)) = compact_replay_event(play) {
                self.replay_ring.push(
                    self.replay_started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64,
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
            PlayEvent::JoinGame { entity_id, .. } => {
                self.player_entity_id = Some(entity_id);
                self.reset_dimension();
            }
            PlayEvent::PlayerCombatKill { .. } => {
                // `death.apply` above already folded the kill into the
                // death loop (dead + cause + victim id); the UI sync
                // turns it into `Screen::Death`.
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
                self.mark_chunk_column_dirty(cx, cz);
                self.terrain_dirty = true;
            }
            PlayEvent::BlockUpdate { pos, state_id } => {
                if self.world.blocks.is_air(state_id) {
                    self.evict_kinetic_part((pos[0], pos[1], pos[2]));
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
            PlayEvent::ContainerClose { window_id } => {
                if self
                    .open_screen
                    .as_ref()
                    .is_some_and(|screen| screen.window_id == i32::from(window_id))
                {
                    self.open_screen = None;
                }
            }
            PlayEvent::OpenSignEditor { pos, front_side } => {
                self.pending_sign_editor =
                    Some(((pos[0], pos[1], pos[2]), front_side));
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
                    self.hud_revision += 1;
                    let mut offset = 0usize;
                    for index in 0..slot_count {
                        let tail = &raw_payload[offset.min(raw_payload.len())..];
                        match decode_item_stack_components(tail) {
                            Ok((stack, consumed)) => {
                                offset += consumed;
                                self.player_inventory.insert(index, stack);
                            }
                            Err(_) => break,
                        }
                    }
                    if offset < raw_payload.len() {
                        if let Ok((carried, _)) =
                            decode_item_stack_components(&raw_payload[offset..])
                        {
                            self.player_carried = Some(carried);
                        }
                    }
                    self.sync_hud_inventory();
                } else if let Some(screen) = &mut self.open_screen {
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
                if window_id == 0 {
                    self.player_inventory_state_id = state_id;
                    self.hud_revision += 1;
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
            PlayEvent::SetHealth { health, food, .. } => {
                self.hud.health = health;
                self.hud.hunger = food.clamp(0, 20) as u8;
                self.hud_revision += 1;
            }
            PlayEvent::SetExperience { bar, level, .. } => {
                // `total` already fed the death screen's Score line via
                // `death.apply`; the HUD wants bar progress + level.
                self.hud.xp_progress = bar.clamp(0.0, 1.0);
                self.hud.xp_level = level.max(0) as u32;
                self.hud_revision += 1;
            }
            PlayEvent::SystemChat { text, overlay } => {
                if overlay {
                    self.hud.actionbar = text.clone();
                    self.hud_revision += 1;
                } else {
                    self.push_chat_line(text.clone());
                }
                self.chat_message = Some(text);
            }
            PlayEvent::SetActionBarText { nbt_bytes } => {
                self.hud.actionbar = nbt_text(&nbt_bytes).unwrap_or_default();
                self.actionbar_nbt = Some(nbt_bytes);
                self.hud_revision += 1;
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
            PlayEvent::SetCarriedItem(slot) => {
                // Server-forced held-item correction (`set_carried_item`
                // S2C) — mirror it into the hotbar highlight.
                let slot = i32::from(slot);
                if (0..9).contains(&slot) {
                    self.hud.selected_slot = slot as usize;
                    self.hud_revision += 1;
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
                self.hud_revision += 1;
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
            }
            PlayEvent::RemoveMobEffect { eid, effect_id } => {
                if let Some(effects) = self.mob_effects.get_mut(&eid) {
                    effects.remove(&effect_id);
                }
            }
            PlayEvent::SetTitle {
                fade_in,
                stay,
                fade_out,
            } => {
                self.title_times = Some([fade_in, stay, fade_out]);
                self.title_revision += 1;
                self.hud_revision += 1;
            }
            PlayEvent::SetTitleText { nbt_bytes } => {
                self.title_text = nbt_text(&nbt_bytes);
                self.title_text_nbt = Some(nbt_bytes);
                self.title_revision += 1;
                self.hud_revision += 1;
            }
            PlayEvent::SetSubtitleText { nbt_bytes } => {
                self.subtitle_text = nbt_text(&nbt_bytes);
                self.subtitle_text_nbt = Some(nbt_bytes);
                self.title_revision += 1;
                self.hud_revision += 1;
            }
            PlayEvent::ClearTitles => {
                // `clear_titles` hides the live overlay immediately; the
                // stored fade/stay defaults stay armed for the next
                // `set_title_text`.
                self.title_text = None;
                self.subtitle_text = None;
                self.title_text_nbt = None;
                self.subtitle_text_nbt = None;
                self.title_revision += 1;
                self.hud_revision += 1;
            }
            PlayEvent::GameEvent { event, value } => {
                self.apply_game_event(event, value);
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
                    });
                    bar.action = action;
                    decode_boss_event_payload(bar, &raw_payload);
                    bar.raw_payload = raw_payload;
                }
                self.sync_hud_bossbars();
                self.hud_revision += 1;
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
                self.hud_revision += 1;
            }
            PlayEvent::PlayerInfoRemove { uuids } => {
                for uuid in &uuids {
                    self.player_info.remove(uuid);
                }
                self.sync_hud_players();
                self.hud_revision += 1;
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
                self.hud_revision += 1;
            }
            PlayEvent::SetScore {
                owner,
                objective,
                value,
                ..
            } => {
                self.scoreboard.scores.insert((objective, owner), value);
                self.hud_revision += 1;
            }
            PlayEvent::SetDisplayObjective { slot, objective } => {
                self.scoreboard.display_slots.insert(slot, objective);
                self.hud_revision += 1;
            }
            PlayEvent::ResetScore { owner, objective } => {
                match objective {
                    Some(objective) => self
                        .scoreboard
                        .scores
                        .retain(|(obj, who), _| obj != &objective || who != &owner),
                    None => self.scoreboard.scores.retain(|(_, who), _| who != &owner),
                }
                self.hud_revision += 1;
            }
            PlayEvent::Transfer { host, port } => match handle_transfer_packet(&host, port) {
                Ok(target) => self.pending_transfer = Some(target),
                Err(reason) => {
                    self.status = SessionStatus::Disconnected(reason);
                }
            },
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
                self.hud_revision += 1;
            }
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
                let _ = self.outgoing.try_send(WorkerCommand::ResourcePackResponse {
                    uuid,
                    status: megu_client::protocol::RESOURCE_PACK_ACCEPTED,
                });
                let _ = self.outgoing.try_send(WorkerCommand::ResourcePackResponse {
                    uuid,
                    status: megu_client::protocol::RESOURCE_PACK_SUCCESSFULLY_LOADED,
                });
                let _ = (forced, url);
            }
            PlayEvent::ResourcePackPop { uuid } => {
                match uuid {
                    Some(uuid) => {
                        // A targeted pop removes just that pack; the
                        // DISCARDED echo tells the server the pack is
                        // gone client-side.
                        self.server_resource_packs.retain(|known| *known != uuid);
                        let _ = self.outgoing.try_send(WorkerCommand::ResourcePackResponse {
                            uuid,
                            status: megu_client::protocol::RESOURCE_PACK_DISCARDED,
                        });
                    }
                    None => {
                        // UUID-less pops clear every server pack (vanilla
                        // applies the same all-or-nothing rule).
                        let drained: Vec<[u8; 16]> = self.server_resource_packs.drain(..).collect();
                        for uuid in drained {
                            let _ = self.outgoing.try_send(WorkerCommand::ResourcePackResponse {
                                uuid,
                                status: megu_client::protocol::RESOURCE_PACK_DISCARDED,
                            });
                        }
                    }
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
            PlayEvent::BlockChangedAck(sequence) => self.apply_block_changed_ack(sequence),
            PlayEvent::SetCenterChunk { cx, cz } => self.apply_set_center_chunk(cx, cz),
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
            PlayEvent::Disconnect(reason) => self.status = SessionStatus::Disconnected(reason),
            _ => {}
        }
        Ok(())
    }

    /// Session-side `game_event` handling layered over the world's
    /// weather mirror: game-mode flips drive the HUD vitals gate,
    /// `enable_respawn_screen(0)` is vanilla immediate respawn, demo
    /// events surface as chat hints, and the arrow/pufferfish/guardian
    /// ids play their player-source stingers.
    fn apply_game_event(&mut self, event: u8, value: f32) {
        match event {
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
                // `value != 0` rolls credits before the respawn; the
                // credits screen is not implemented, so the request is
                // recorded until the dimension reset clears it.
                self.pending_end_credits = value != 0.0;
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

    fn reset_dimension(&mut self) {
        self.acoustic_world.reset_dimension(&self.world);
        self.audio.mixer.refresh_acoustics();
        self.pending_end_credits = false;
        self.entity_positions.clear();
        self.entity_metadata.clear();
        self.mob_effects.clear();
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
        self.pose = None;
        self.last_motion_delta = [0.0; 2];
        self.chunk_fades = ChunkFadeTracker::new();
        self.prefetcher = SpeculativeChunkPrefetcher::new();
        // Interaction bookkeeping is dimension-scoped: acks for the old
        // dimension's predicted positions must not re-dirty the new one,
        // and the server re-sends `set_center_chunk` after respawn.
        self.pending_interaction_seqs.clear();
        self.view_center_chunk = None;
        self.command_suggestions = None;
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
            ClientCustomChannelEvent::Unknown { channel, payload }
                if matches!(
                    channel.as_str(),
                    CHANNEL_CREATE_KINETIC_SYNC | CHANNEL_CREATE_KINETIC_UPDATE
                ) =>
            {
                if let Ok(records) = decode_kinetic_sync_batch(&payload) {
                    for record in records {
                        self.upsert_kinetic_record(record);
                    }
                }
            }
            ClientCustomChannelEvent::Unknown { channel, payload }
                if channel.as_str() == CHANNEL_MOD_REGISTRY_SYNC =>
            {
                match decode_mrs1_frame(&payload) {
                    Ok(registry) => self.mod_registry = Some(registry),
                    Err(_) => self.mod_registry_rejected += 1,
                }
            }
            ClientCustomChannelEvent::Unknown { channel, payload }
                if channel.as_str() == SUBWORLD_SYNC_CHANNEL =>
            {
                if let Ok(frame) = decode_sync_frame(&payload) {
                    self.subworlds.apply_sync_frame(&frame);
                    self.subworld_sync_received_at = Some(Instant::now());
                    self.player_subworld_id = frame
                        .poses
                        .iter()
                        .find(|pose| pose.flags & SUBWORLD_POSE_PLAYER_ABOARD != 0)
                        .map(|pose| pose.subworld_id);
                }
            }
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
    let mut last_input = None;
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
                Ok(WorkerCommand::Movement(movement)) => last_input = Some(movement),
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
                    window_id,
                    button_id,
                }) => {
                    client.send_container_button_click(window_id, button_id)?;
                }
                Ok(WorkerCommand::ContainerClose { window_id }) => {
                    client.send_container_close(window_id)?;
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
                client.send_player_input(input.strafe, input.forward, input.jump, input.sneak)?;
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
                let previous_pose = last_input.map(|movement: PlayerMovement| movement.pose);
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
                last_input = Some(PlayerMovement {
                    pose,
                    strafe: 0.0,
                    forward: 0.0,
                    jump: false,
                    sneak: false,
                });
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
            report.notes.push(format!("server mod JAR {} skipped: {err}", path.display()));
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
        for file in crate::vanilla_jar::configured_resource_packs(game_dir) {
            // options.txt names are pack-root-relative; refuse anything
            // trying to walk out of it.
            if file.contains("..") || file.starts_with('/') || file.contains('\\') {
                report
                    .notes
                    .push(format!("options.txt pack {file:?} rejected: unsafe path"));
                continue;
            }
            let candidate = pack_root.join(&file);
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
    let mut r = WireReader::new(payload);
    match bar.action {
        0 => {
            if let Some(title) = r.read_nbt().ok().and_then(|nbt| {
                let text = nbt.extract_text();
                (!text.is_empty()).then_some(text)
            }) {
                bar.title = title;
            }
            if let Ok(health) = r.read_f32() {
                bar.health = health.clamp(0.0, 1.0);
            }
        }
        2 => {
            if let Ok(health) = r.read_f32() {
                bar.health = health.clamp(0.0, 1.0);
            }
        }
        3 => {
            if let Some(title) = r.read_nbt().ok().and_then(|nbt| {
                let text = nbt.extract_text();
                (!text.is_empty()).then_some(text)
            }) {
                bar.title = title;
            }
        }
        _ => {}
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
/// (`zsh_command_bar`'s `COMMAND_SPECS`) plus the megu/server navigation
/// shortcuts so `complete_suggestions` answers the same literals the bar
/// highlights before any `declare_commands`.
fn seeded_command_dispatcher() -> CommandDispatcher {
    let mut dispatcher = CommandDispatcher::new();
    for name in megu_client::zsh_command_bar::vanilla_command_names() {
        let _ = dispatcher.register_literal(name);
    }
    // Client/server navigation commands servers commonly expose; not in
    // vanilla's command table but always safe completion candidates.
    for name in ["megu", "aero", "hub", "server", "world"] {
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
    payload.extend_from_slice(
        &pack_position([pos.0, pos.1, pos.2]).to_be_bytes(),
    );
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

/// Best-effort harvest-tool family from a registry block id — the
/// looking-at HUD's tool hint while the session does not mirror block
/// material/destroy-time metadata. Ordered so the more specific
/// material families win over shared substrings (`sandstone` is
/// pickaxe-class, `iron_door` pickaxe over axe).
fn preferred_harvest_tool(block_id: &str) -> Option<HarvestTool> {
    let path = block_id.rsplit(':').next().unwrap_or(block_id);
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
        DecodedMotionPhase, EquipmentEntry, SSN1_MAGIC, SSN1_VERSION, WireReader,
        decode_chunk_packet, decode_custom_payload_packet, decode_packet_body, encode_packet,
        write_varint,
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
    fn subworld_custom_payload_channels_dispatch_inbound_and_outbound() {
        let (_incoming_tx, incoming) = mpsc::sync_channel(8);
        let (outgoing, outgoing_rx) = mpsc::sync_channel(8);
        let mut live = LiveSession::with_channels(incoming, outgoing);
        let mut sw6d = Vec::new();
        sw6d.extend_from_slice(b"SW6D");
        sw6d.push(1);
        sw6d.extend_from_slice(&1u16.to_be_bytes());
        sw6d.extend_from_slice(&77u64.to_be_bytes());
        for value in [4.0f64, 5.0, 6.0] { sw6d.extend_from_slice(&value.to_be_bytes()); }
        for value in [0.0f32, 0.0, 0.0, 1.0] { sw6d.extend_from_slice(&value.to_be_bytes()); }
        for _ in 0..6 { sw6d.extend_from_slice(&0.0f32.to_be_bytes()); }
        sw6d.push(0);
        live.apply_event(framed(SUBWORLD_SYNC_CHANNEL, &sw6d)).unwrap();
        assert_eq!(live.subworlds.get(77).unwrap().transform.position, [4.0, 5.0, 6.0]);

        let cmd = SubWorldHelmCmd { subworld_id: 77, throttle: 0.25, ..Default::default() };
        assert!(live.send_subworld_helm(&cmd));
        let WorkerCommand::SendCustomPayload { channel, payload } = outgoing_rx.recv().unwrap() else {
            panic!("helm helper must enqueue a custom payload")
        };
        assert_eq!(channel, SUBWORLD_HELM_CHANNEL);
        assert_eq!(crate::subworld_sync::decode_helm_cmd(&payload).unwrap(), cmd);
        let wire = encode_custom_payload_body(&channel, &payload);
        assert!(wire.ends_with(&payload));
        assert_eq!(play_custom_payload_packet_id(767), 0x12);
        assert_eq!(play_custom_payload_packet_id(777), 0x16);
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
        let frame = live.replay_ring.frames().back().expect("captured play event");
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
        done_tx.send(()).unwrap();
        server.join().unwrap();
        let mesh = live.world.packed_meshes().next().expect("TCP chunk mesh");
        let geometry = section_geometry(
            mesh.1,
            live.world.translucent_order(mesh.0).unwrap_or(&[]),
            [
                mesh.0.0 as f32 * 16.0,
                mesh.0.1 as f32 * 16.0,
                mesh.0.2 as f32 * 16.0,
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
        assert_eq!(live.kinetic_pos_to_id.get(&(4, 64, 8)), Some(&0));
        assert_eq!(live.kinetic_pos_to_id.get(&(5, 64, 8)), Some(&1));
        let first = live.goggles_telemetry_at((4, 64, 8)).unwrap();
        let second = live.goggles_telemetry_at((5, 64, 8)).unwrap();
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
        assert!(live.goggles_telemetry_at((4, 64, 8)).is_none());
        live.apply_event(ClientPlayEvent::Play(PlayEvent::UnloadChunk {
            cx: 0,
            cz: 0,
        }))
        .unwrap();
        assert_eq!(live.kinetic_part_count(), 0);
    }

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
        frame.extend_from_slice(&40u32.to_be_bytes()); // base_block_state_id
        frame.extend_from_slice(&9_000u32.to_be_bytes()); // base_item_id
        frame.extend_from_slice(&2u32.to_be_bytes()); // block state count
        for (id, name, axis) in [
            (40_u32, "create:water_wheel", "x"),
            (41_u32, "create:shaft", "y"),
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
        assert_eq!(registry.base_block_state_id, 40);
        assert_eq!(registry.block_state_name(40), Some("create:water_wheel"));
        assert_eq!(registry.item_name(9_000), Some("create:wrench"));

        let mut bad = mrs1_frame();
        let last = bad.len() - 1;
        bad[last] ^= 0xFF;
        live.apply_event(framed(CHANNEL_MOD_REGISTRY_SYNC, &bad))
            .unwrap();
        assert_eq!(live.mod_registry_rejected, 1);
        // The rejected frame never clobbers the stored registry.
        assert_eq!(
            live.mod_registry.as_ref().unwrap().block_state_name(40),
            Some("create:water_wheel")
        );
    }

    #[test]
    fn dynamic_blocks_drive_family_templates_and_goggles_names() {
        let (mut live, _tx) = session();
        let body = decoded_frame(0x27, &stone_chunk());
        let chunk = decode_chunk_packet(&body, 24, 64).unwrap();
        live.apply_event(ClientPlayEvent::Play(PlayEvent::ChunkData(Box::new(chunk))))
            .unwrap();
        live.apply_event(framed(CHANNEL_MOD_REGISTRY_SYNC, &mrs1_frame()))
            .unwrap();
        // Dynamic mod block states: water wheel on X, shaft on Y.
        for (pos, state_id) in [([7, 64, 3], 40_u32), ([8, 64, 3], 41_u32)] {
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
        let telemetry = live.goggles_telemetry_at((7, 64, 3)).unwrap();
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
            std::env::temp_dir().join(format!("megu-live-mrs1-{}-{}", std::process::id(), 40_u32));
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
        let body = decoded_frame(0x27, &stone_chunk());
        let chunk = decode_chunk_packet(&body, 24, 64).unwrap();
        live.apply_event(ClientPlayEvent::Play(PlayEvent::ChunkData(Box::new(chunk))))
            .unwrap();
        live.apply_event(framed(CHANNEL_MOD_REGISTRY_SYNC, &mrs1_frame()))
            .unwrap();
        live.apply_event(ClientPlayEvent::Play(PlayEvent::BlockUpdate {
            pos: [7, 64, 3],
            state_id: 40,
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

        let telemetry = live.goggles_telemetry_at((7, 64, 3)).unwrap();
        assert_eq!(telemetry.block_id, "create:water_wheel");
        assert_eq!(telemetry.display_name.as_deref(), Some("水車"));
        // The Japanese table sticks to the resolved locale; switching the
        // UI locale back marks it stale so the next lookup reloads.
        live.set_ui_locale("en_us");
        let telemetry = live.goggles_telemetry_at((7, 64, 3)).unwrap();
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
                mesh.0.0 as f32 * 16.0,
                mesh.0.1 as f32 * 16.0,
                mesh.0.2 as f32 * 16.0,
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
        live.interact_entity(42, Some([1.0, 2.0, 3.0]));
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
        let Ok(WorkerCommand::Movement(m)) = rx.try_recv() else {
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
        assert!(matches!(rx.recv().unwrap(), WorkerCommand::Movement(_)));

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
        assert!(matches!(rx.recv().unwrap(), WorkerCommand::Movement(_)));
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
            live.record_frame_and_refresh_minihud(1.0 / 60.0, &mut ui);
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

        // The samples must flow into the MiniHUD copy path as well.
        let mut ui = megu_ui::state::UiState::new();
        ui.settings.qol.mini_hud.enabled = true;
        live.record_frame_and_refresh_minihud(1.0 / 60.0, &mut ui);
        assert!(
            ui.hud.mini_hud_data.mspt.is_some() && ui.hud.mini_hud_data.tps.is_some(),
            "live tick samples must reach MiniHUD mspt/tps"
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
            particle_id: 14,
            long_distance: false,
            x: 1.0,
            y: 65.0,
            z: 2.0,
            offset_x: 0.0,
            offset_y: 0.0,
            offset_z: 0.0,
            max_speed: 0.0,
            count: 4,
            particle_data: Vec::new(),
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
        assert_eq!(
            live.take_pending_sign_editor(),
            Some(((10, 64, -20), true))
        );
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
        assert_eq!(
            live.take_pending_sign_editor(),
            Some(((-3, 70, 42), false))
        );
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
    fn seeded_dispatcher_offers_megu_navigation_commands() {
        let dispatcher = seeded_command_dispatcher();
        for cmd in ["megu", "aero", "hub", "server", "world"] {
            // Brigadier completes on the typed prefix — the chat bar
            // asks at the cursor, so a one-letter head must offer it.
            let prefix = &cmd[..1];
            let suggestions = dispatcher
                .complete_suggestions(prefix, prefix.len())
                .unwrap_or_else(|_| panic!("suggestions for {cmd}"));
            assert!(
                (0..suggestions.len()).any(|i| suggestions.get(i) == Some(cmd)),
                "seeded dispatcher must complete /{cmd} from /{prefix}"
            );
        }
    }
}
