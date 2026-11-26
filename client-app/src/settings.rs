// SPDX-License-Identifier: LGPL-3.0-or-later
//! Player-facing graphics settings for the native megu client.
//!
//! Design rule (low cognitive load): optimizations with zero player-visible
//! downside are ALWAYS ON under the hood and deliberately have no toggle —
//! see [`BuiltInOptimizations`]. This module only exposes settings that
//! genuinely change what the player sees or that trade something off:
//! preset, view distance, distant horizon LOD, clouds, particles, smooth
//! lighting, entity shadows, FOV, VSync, an FPS cap, and the GPU backend
//! preference.
//!
//! Persistence is human-readable TOML (`megu-client.toml`) plus a
//! [`ClientSettings::import_vanilla_options_txt`] helper so players migrating
//! from the official launcher keep their FOV, render distance, clouds,
//! particles, and VSync automatically. Pure std Rust; zero dependencies.

use std::path::Path;

use megu_client_mesh::RenderBackendPreference;
use megu_core::hash::fnv1a64;

use crate::crash_reporter::{CrashSubmitPolicy, parse_report_url};

use crate::audio_qol::{
    DEFAULT_CHAT_HISTORY_LINES, DEFAULT_MAX_CONCURRENT_SAME_SOUND, DEFAULT_TOOL_WARN_PERCENT,
    DEFAULT_ZOOM_DIVISOR, MAX_CHAT_HISTORY_LINES, MAX_MAX_CONCURRENT_SAME_SOUND,
    MAX_TOOL_WARN_PERCENT, MAX_VOLUME_CAP, MAX_ZOOM_DIVISOR, MIN_CHAT_HISTORY_LINES,
    MIN_MAX_CONCURRENT_SAME_SOUND, MIN_ZOOM_DIVISOR,
};
use crate::lod::LodConfig;

/// Minimum render distance selectable in the UI (chunks).
pub const MIN_VIEW_DISTANCE: u8 = 6;
/// Maximum render distance selectable in the UI (chunks).
pub const MAX_VIEW_DISTANCE: u8 = 32;
/// Default render distance for a fresh profile (chunks).
pub const DEFAULT_VIEW_DISTANCE: u8 = 12;
/// Minimum field of view (degrees).
pub const MIN_FOV: u8 = 30;
/// Maximum field of view (degrees).
pub const MAX_FOV: u8 = 110;
/// Default field of view (degrees).
pub const DEFAULT_FOV: u8 = 70;
/// Minimum FPS cap when one is set (frames/second).
pub const MIN_MAX_FPS: u32 = 10;
/// Maximum FPS cap when one is set (frames/second).
pub const MAX_MAX_FPS: u32 = 2000;
/// Vanilla `options.txt` `maxFps` value the official UI labels Unlimited.
pub const VANILLA_OPTIONS_UNLIMITED_FPS: i64 = 260;
/// Canonical free-FPS slider stops, ending in `None` for Unlimited.
pub const FPS_SLIDER_PRESETS: &[Option<u32>] = &[
    Some(30),
    Some(60),
    Some(120),
    Some(144),
    Some(165),
    Some(240),
    Some(280),
    Some(320),
    Some(360),
    Some(480),
    Some(600),
    None,
];
/// Default frame-rate cap while the window is unfocused (frames/second).
pub const DEFAULT_BACKGROUND_FPS_LIMIT: u16 = 15;
/// Maximum selectable background FPS cap; `0` disables background throttling.
pub const MAX_BACKGROUND_FPS: u16 = 240;
/// Distant-horizon radius in chunks for [`DistantHorizon::Medium`].
pub const DISTANT_HORIZON_MEDIUM_CHUNKS: i32 = 64;
/// Distant-horizon radius in chunks for [`DistantHorizon::Far`].
pub const DISTANT_HORIZON_FAR_CHUNKS: i32 = 128;
/// LOD summary memory budget shared by every distant-horizon level.
const LOD_MEMORY_BUDGET_BYTES: usize = 64 * 1024 * 1024;
/// Default filename for [`ClientSettings::to_toml`] output.
pub const SETTINGS_FILE_NAME: &str = "megu-client.toml";
/// Default `qol.max_volume_cap` for a fresh profile: a soft `0.85` bus
/// ceiling so stacked loud sounds (thunder, Wither spawn, explosions)
/// never clip or blast headphones on first launch. `1.0` remains
/// available for players who opt out.
pub const DEFAULT_MAX_VOLUME_CAP: f32 = 0.85;

/// Stable physical key names used by the window's rebindable controls.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyBindings {
    pub forward: String,
    pub back: String,
    pub left: String,
    pub right: String,
    pub jump: String,
    pub sneak: String,
    pub sprint: String,
    pub inventory: String,
    pub drop: String,
    pub swap_offhand: String,
    pub chat: String,
    pub command: String,
    pub zoom: String,
    pub camera_toggle: String,
    pub fullscreen: String,
    pub advancements: String,
    pub sort_inventory: String,
}

impl Default for KeyBindings {
    fn default() -> Self {
        Self {
            forward: "KeyW".into(),
            back: "KeyS".into(),
            left: "KeyA".into(),
            right: "KeyD".into(),
            jump: "Space".into(),
            sneak: "ShiftLeft".into(),
            sprint: "ControlLeft".into(),
            inventory: "KeyE".into(),
            drop: "KeyQ".into(),
            swap_offhand: "KeyF".into(),
            chat: "KeyT".into(),
            command: "Slash".into(),
            zoom: "KeyC".into(),
            camera_toggle: "F5".into(),
            fullscreen: "F11".into(),
            advancements: "KeyL".into(),
            sort_inventory: "KeyR".into(),
        }
    }
}

/// Named bundle of player-visible settings.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QualityPreset {
    Fast,
    Balanced,
    High,
    /// Maximum view distance plus the widest far-terrain horizon.
    Ultra,
    /// The player tuned at least one field away from a preset.
    Custom,
}

/// Distance-tiered LOD horizon beyond the full-detail view distance.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DistantHorizon {
    /// No LOD drawing; the world ends at `view_distance`.
    Off,
    /// LOD columns out to 64 chunks.
    Medium,
    /// LOD columns out to 128 chunks.
    Far,
}

/// Cloud drawing mode, matching Vanilla semantics.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CloudMode {
    Off,
    Fast,
    Fancy,
}

/// Particle emission level, matching Vanilla semantics.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParticleLevel {
    Minimal,
    Decreased,
    All,
}

macro_rules! named_option {
    ($ty:ty, $default:expr, $($variant:ident => $name:literal),+ $(,)?) => {
        impl $ty {
            /// Lowercase TOML/options.txt token for this value.
            pub const fn as_str(self) -> &'static str {
                match self {
                    $(<$ty>::$variant => $name,)+
                }
            }

            /// Parse a lowercase-or-mixed-case token; `None` when unknown.
            pub fn parse(value: &str) -> Option<Self> {
                match value.to_ascii_lowercase().as_str() {
                    $($name => Some(<$ty>::$variant),)+
                    _ => None,
                }
            }
        }

        impl Default for $ty {
            fn default() -> Self {
                $default
            }
        }
    };
}

named_option!(QualityPreset, QualityPreset::Balanced,
    Fast => "fast", Balanced => "balanced", High => "high", Ultra => "ultra",
    Custom => "custom");
named_option!(DistantHorizon, DistantHorizon::Off,
    Off => "off", Medium => "medium", Far => "far");
named_option!(CloudMode, CloudMode::Fancy,
    Off => "off", Fast => "fast", Fancy => "fancy");
named_option!(ParticleLevel, ParticleLevel::All,
    Minimal => "minimal", Decreased => "decreased", All => "all");

/// Engine optimizations that are always compiled in and always enabled.
///
/// Every flag here is a zero-downside win, so none of them is exposed to the
/// player: the settings UI stays small and there is no confusing way to make
/// the game slower. This is also the coverage matrix behind the launcher's
/// built-in optimization catalog — every capability is an original
/// clean-room Rust implementation inside `client-port` / `client-app` /
/// `megu`; no third-party optimization-mod code, shaders, assets, or
/// logic is ever copied or linked (see
/// `specs/client-fork/ux-and-builtin-optimizations.md`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BuiltInOptimizations {
    /// Graph occlusion culling: BFS over the inter-section visibility
    /// graph intersected with the camera frustum.
    pub graph_occlusion_culling: bool,
    /// Per-section directional face bucketing for backface culling.
    pub backface_bucket_culling: bool,
    /// One multi-draw submission per render region x facing bucket.
    pub multidraw_region_batching: bool,
    /// Frustum-ordered dirty-section rebuilds + incremental translucent
    /// re-sort gated on camera displacement.
    pub frustum_prioritized_rebuilds: bool,
    /// Compact `PalettedSection` storage.
    pub compact_palettes: bool,
    /// Deduplicated blockstate/model resources.
    pub deduplicated_resources: bool,
    /// Batched HUD text/icon submission.
    pub fast_hud_batching: bool,
    /// Frustum + occlusion entity culling.
    pub entity_culling: bool,
}

impl BuiltInOptimizations {
    /// The only configuration this struct ever takes at runtime.
    pub const ALL_ON: Self = Self {
        graph_occlusion_culling: true,
        backface_bucket_culling: true,
        multidraw_region_batching: true,
        frustum_prioritized_rebuilds: true,
        compact_palettes: true,
        deduplicated_resources: true,
        fast_hud_batching: true,
        entity_culling: true,
    };
}

/// Xaero's Minimap/WorldMap fair-play control signal: a server broadcasts
/// this to clients to lock cave-map mode and entity radar.
pub const XAERO_FAIR_PLAY_SIGNAL: &str = "§f§a§i§r§x§a§e§r§o";
/// Control signal restoring a client's default map-mod feature set.
pub const XAERO_RESET_SIGNAL: &str = "§r§e§s§e§t§x§a§e§r§o";
/// Selective Xaero control signal locking cave-map mode only.
pub const XAERO_NO_CAVE_MAP_SIGNAL: &str = "§n§o§c§a§v§e§m§a§p§x§a§e§r§o";
/// Selective Xaero control signal locking entity radar only.
pub const XAERO_NO_ENTITY_RADAR_SIGNAL: &str = "§n§o§e§n§t§i§t§y§r§a§d§a§r§x§a§e§r§o";

/// `minimap_mode`: how much minimap/world-map mods may do in a world this
/// client hosts — 1:1 with the server's `minimap-mode` key (OPS-4).
///
/// Default-permissive by contract: an absent TOML key reads as
/// [`HostMinimapMode::Unrestricted`], so no map feature is ever locked
/// unless the host opts in. [`HostMinimapMode::parse`] fails closed —
/// an unrecognized token is `None`, never a guess.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HostMinimapMode {
    /// Map mods run with every feature enabled (the default).
    Unrestricted,
    /// Maps run, gated by the host's `allow_*` restriction switches.
    FairPlay,
    /// No minimap/world-map features at all.
    Disabled,
}

/// `player_radar_scope`: who may appear on a player's map radar — 1:1 with
/// the server's `player-radar-scope` key (OPS-4). Like
/// [`HostMinimapMode`], absent reads as the permissive default
/// [`HostPlayerRadarScope::All`] and [`HostPlayerRadarScope::parse`] fails
/// closed on an unrecognized token.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HostPlayerRadarScope {
    /// Every player is visible on every radar (the default).
    All,
    /// Self, operators, and party members / allies only.
    PartyOnly,
    /// Self and operators only — nobody else appears on the radar.
    None,
}

named_option!(HostMinimapMode, HostMinimapMode::Unrestricted,
    Unrestricted => "unrestricted", FairPlay => "fair_play", Disabled => "disabled");
named_option!(HostPlayerRadarScope, HostPlayerRadarScope::All,
    All => "all", PartyOnly => "party_only", None => "none");

/// `crash_report_mode`: consent gate for transmitting crash reports to the
/// optional report relay — 1:1 with
/// [`crate::crash_reporter::CrashSubmitPolicy`].
///
/// Default-safe: an absent TOML key reads as [`CrashReportMode::Ask`], so
/// crash files are written locally and nothing is transmitted until the
/// user confirms. [`CrashReportMode::parse`] fails closed — an
/// unrecognized token is `None`, never a guess.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CrashReportMode {
    /// Never send automatically; explicit user confirmation required.
    Ask,
    /// Automatic send for crash reports only; manual bug reports still
    /// require confirmation.
    AutoCrashOnly,
    /// Local files only — no report ever leaves the machine.
    Off,
}

named_option!(CrashReportMode, CrashReportMode::Ask,
    Ask => "ask", AutoCrashOnly => "auto_crash_only", Off => "off");

impl CrashReportMode {
    /// The [`CrashSubmitPolicy`] consent gate this mode maps onto.
    pub const fn submit_policy(self) -> CrashSubmitPolicy {
        match self {
            Self::Ask => CrashSubmitPolicy::Ask,
            Self::AutoCrashOnly => CrashSubmitPolicy::AutoCrashOnly,
            Self::Off => CrashSubmitPolicy::Off,
        }
    }
}

/// Map-mod fair-play policy for a world this client hosts directly
/// (integrated server, LAN share, or a `MEGU-XXXX-XXXX` friend relay).
///
/// The canonical state is the five-switch granular policy shared with the
/// server (OPS-4): `minimap_mode`, `allow_cave_map`, `allow_mob_radar`,
/// `player_radar_scope` and `allow_waypoint_teleport`. The pre-granular
/// spellings survive for backward compatibility: `minimap_fair_play`
/// mirrors `minimap_mode != Unrestricted` (TOML `minimap_fair_play` maps
/// onto the mode), and `allow_entity_radar` mirrors `allow_mob_radar` —
/// every parser and resolver keeps each mirrored pair in sync.
///
/// Xaero's Minimap/WorldMap honor server-sent `§`-coded control strings that
/// lock cave-map mode and entity radar; the waypoint-teleport and
/// player-radar flags carry the same idea and are forwarded to the hosted
/// server as env vars alongside the control signal (see `integrated.rs`).
/// All seven keys are optional in TOML and keep their permissive defaults
/// when a settings file written before this feature omits them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HostMapFairPlaySettings {
    /// Legacy master switch: `true` whenever the hosted world restricts
    /// map mods, i.e. `minimap_mode != HostMinimapMode::Unrestricted` —
    /// the parsers and resolvers keep the two in sync. When `false`,
    /// hosted clients get [`XAERO_RESET_SIGNAL`] regardless of the
    /// `allow_*` flags.
    pub minimap_fair_play: bool,
    /// Cave-map mode (below-ground ore/cave exposure) stays allowed.
    pub allow_cave_map: bool,
    /// Entity radar stays allowed — the pre-granular spelling of
    /// `allow_mob_radar`; the two fields always carry the same value.
    pub allow_entity_radar: bool,
    /// World-map waypoint teleportation stays allowed.
    pub allow_waypoint_teleport: bool,
    /// Granular minimap mode: `unrestricted` (default), `fair_play`
    /// (maps run gated by the `allow_*` switches) or `disabled` (no map
    /// features at all).
    pub minimap_mode: HostMinimapMode,
    /// Mob/entity radar stays allowed — the server-parity spelling of
    /// `allow_entity_radar`.
    pub allow_mob_radar: bool,
    /// Who may appear on hosted players' map radar: `all` (default),
    /// `party_only`, or `none`.
    pub player_radar_scope: HostPlayerRadarScope,
}

impl Default for HostMapFairPlaySettings {
    fn default() -> Self {
        Self {
            minimap_fair_play: false,
            allow_cave_map: true,
            allow_entity_radar: true,
            allow_waypoint_teleport: true,
            minimap_mode: HostMinimapMode::Unrestricted,
            allow_mob_radar: true,
            player_radar_scope: HostPlayerRadarScope::All,
        }
    }
}

impl HostMapFairPlaySettings {
    /// The `§`-coded control signal a hosted server broadcasts to joining
    /// clients. Deterministic: [`XAERO_RESET_SIGNAL`] when no restriction
    /// applies (fair play off and `minimap_mode` `Unrestricted`),
    /// [`XAERO_FAIR_PLAY_SIGNAL`] under `Disabled` mode — the strongest
    /// Xaero lock, since the protocol has no dedicated disable code — or
    /// when cave maps and entity radar are both locked, otherwise the
    /// reset signal followed by the selective lock code for each feature
    /// that stays blocked. `allow_waypoint_teleport` and
    /// `player_radar_scope` are not Xaero `§`-code features — they ride
    /// env vars to the hosted server instead.
    pub fn xaero_control_string(&self) -> String {
        let restricted =
            self.minimap_fair_play || self.minimap_mode != HostMinimapMode::Unrestricted;
        if !restricted {
            return XAERO_RESET_SIGNAL.to_string();
        }
        let cave_locked = !self.allow_cave_map;
        let radar_locked = !self.allow_entity_radar || !self.allow_mob_radar;
        if self.minimap_mode == HostMinimapMode::Disabled || (cave_locked && radar_locked) {
            return XAERO_FAIR_PLAY_SIGNAL.to_string();
        }
        let mut signal = String::from(XAERO_RESET_SIGNAL);
        if cave_locked {
            signal.push_str(XAERO_NO_CAVE_MAP_SIGNAL);
        }
        if radar_locked {
            signal.push_str(XAERO_NO_ENTITY_RADAR_SIGNAL);
        }
        signal
    }
}

/// Quality-of-life toggles that do not change rendering: audio limiting,
/// smooth zoom, tool-durability warnings, and chat history. Persisted under
/// the `[qol]` TOML table; every key is optional and falls back to the
/// permissive default when absent, so settings files written before this
/// section existed keep working unchanged.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ClientQolSettings {
    /// Hard ceiling for the mixed audio buffer, `0.0..=1.0`. The default
    /// `0.85` soft-caps the mix so loud stacked sounds never clip;
    /// `1.0` leaves the mix untouched for players who opt out.
    pub max_volume_cap: f32,
    /// How many instances of the same `(sound_id, cell)` may play per tick
    /// before redundant stacking is attenuated/suppressed, `1..=32`.
    pub max_concurrent_same_sound: u8,
    /// Fully-zoomed FOV divisor while the zoom key is held, `1.5..=10.0`.
    pub zoom_divisor: f32,
    /// Warn when a tool's remaining durability drops to this percent of
    /// max, `0..=100`.
    pub tool_warn_percent: u8,
    /// When `true`, the final swing that would break a tool is blocked.
    /// Defaults to `false` so vanilla breaking behavior is unchanged unless
    /// the player opts in.
    pub prevent_tool_break: bool,
    /// Chat history ring capacity in lines, `100..=5000`.
    pub chat_history_lines: usize,
    /// Whether chat history survives a server disconnect/reconnect.
    pub retain_chat_on_reconnect: bool,
}

impl Default for ClientQolSettings {
    fn default() -> Self {
        Self {
            max_volume_cap: DEFAULT_MAX_VOLUME_CAP,
            max_concurrent_same_sound: DEFAULT_MAX_CONCURRENT_SAME_SOUND,
            zoom_divisor: DEFAULT_ZOOM_DIVISOR,
            tool_warn_percent: DEFAULT_TOOL_WARN_PERCENT,
            prevent_tool_break: false,
            chat_history_lines: DEFAULT_CHAT_HISTORY_LINES,
            retain_chat_on_reconnect: true,
        }
    }
}

/// The "seamless" tiny-annoyance QoL toggles: four opinionated gameplay
/// helpers that are strictly opt-in (`false` by default) plus three
/// non-intrusive pure-UX improvements (`true` by default). Persisted
/// under the `[qol]` TOML table next to [`ClientQolSettings`]; a
/// separate struct so integrations constructing `ClientQolSettings`
/// literals keep compiling unchanged.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ClientSeamlessQolSettings {
    /// Opt-in auto-reconnect after an unexpected disconnect (with a
    /// capped exponential backoff; server refusals are never redialed).
    /// `false` by default — a strict opt-in.
    pub auto_reconnect: bool,
    /// Opt-in automatic hotbar-stack replenish from the inventory.
    /// `false` by default.
    pub auto_replenish: bool,
    /// Opt-in quick-stack-to-nearby-chests helper. `false` by default.
    pub quick_stack_nearby: bool,
    /// Opt-in hunger/saturation preview on food tooltips and the hunger
    /// bar. `false` by default (yields to a dedicated food-stats mod).
    pub food_saturation_preview: bool,
    /// Show a compact contents summary on Shulker Box / Bundle tooltips.
    /// `true` by default (pure UX, non-intrusive).
    pub shulker_tooltip_peek: bool,
    /// Keep the chat view anchored while reading scrollback instead of
    /// yanking to the newest line. `true` by default.
    pub chat_scroll_lock: bool,
    /// Show the non-intrusive top-right toast stack (connection status,
    /// warnings). `true` by default.
    pub toast_notifications: bool,
    /// JEI/EMI-style recipe side-overlay beside open containers
    /// (`Ctrl+O` toggles it in-game). `false` by default — opt-in.
    pub recipe_overlay_enabled: bool,
}

impl Default for ClientSeamlessQolSettings {
    fn default() -> Self {
        Self {
            auto_reconnect: false,
            auto_replenish: false,
            quick_stack_nearby: false,
            food_saturation_preview: false,
            shulker_tooltip_peek: true,
            chat_scroll_lock: true,
            toast_notifications: true,
            recipe_overlay_enabled: false,
        }
    }
}

/// Everything a player can tune. Fields are self-documenting; the TOML file
/// mirrors them 1:1.
#[derive(Clone, Debug, PartialEq)]
pub struct ClientSettings {
    pub preset: QualityPreset,
    /// `6..=32` chunks; also the LOD `full_detail_radius` boundary.
    pub view_distance: u8,
    pub distant_horizon: DistantHorizon,
    pub clouds: CloudMode,
    pub particles: ParticleLevel,
    pub smooth_lighting: bool,
    pub entity_shadows: bool,
    /// Eye-comfort exposure limiter: prevents harsh whiteout/glare on
    /// snow, sand, and stacked torches while keeping warm Minecraft
    /// colors. Default `true`.
    pub comfort_exposure: bool,
    /// Dynamic colored lights contributed by Vanilla + Redstone + modded
    /// blocks, items, and mobs. Default `true`.
    pub dynamic_lights: bool,
    /// 3D & 2D enchanted item/armor shimmer. Default `true`.
    pub enchantment_glint: bool,
    /// Soft indirect bounce & hemisphere reflection so shadowed or
    /// back-facing surfaces never drop to pitch black. Default `true`.
    pub indirect_light_bounce: bool,
    /// Opt-in realistic shadow-light gameplay for worlds this client
    /// hosts or plays locally (mob spawning, undead daylight burn, crop
    /// growth, dynamic-light spawn suppression). Remote servers keep
    /// their own host settings. Default `false`.
    pub directional_shadow_gameplay: bool,
    /// Opt-in O(1) unloaded-chunk catch-up for worlds this client hosts
    /// or plays locally (furnaces, crops, headless factories tick while
    /// their chunk stays unloaded). Remote servers keep their own host
    /// settings. Default `false`.
    pub lazy_catchup_gameplay: bool,
    /// `30..=110` degrees.
    pub fov: u8,
    /// Mouse-look sensitivity percent `10..=200` (`100` = 1.0×).
    pub mouse_sensitivity_pct: u16,
    /// Vanilla sound-category volume sliders, percent `0..=100`:
    /// `volume_master` scales every source; `volume_music` covers the
    /// music + records(jukebox) sources, `volume_blocks`/`volume_hostile`/
    /// `volume_friendly` cover the blocks, hostile-mob and
    /// neutral+players sources respectively.
    pub volume_master: u8,
    pub volume_music: u8,
    pub volume_blocks: u8,
    pub volume_hostile: u8,
    pub volume_friendly: u8,
    pub vsync: bool,
    /// `None` = unlimited; `Some(10..=2000)` otherwise.
    pub max_fps: Option<u32>,
    /// Frame-rate cap applied while the window is unfocused, in the spirit
    /// of Vanilla's "Dynamic FPS" graphics toggle: `0` disables background
    /// throttling entirely, `1..=240` caps background frames. Defaults to
    /// [`DEFAULT_BACKGROUND_FPS_LIMIT`].
    pub background_fps_limit: u16,
    /// Free host-RAM cache budget in MiB (`ram_budget_mib`): `0` = Auto
    /// lets the runtime pick a sane share of the detected host RAM;
    /// `> 0` caps how much RAM the chunk/asset caches may hold. The live
    /// OOM guard trims distant caches early whenever host available RAM
    /// drops low, so a high cap can never get the process OOM-killed —
    /// the budget never pre-allocates anything.
    pub ram_budget_mib: u32,
    /// GPU pipeline selection: `auto` negotiates from hardware
    /// capabilities, `classic-low-spec` pins the minimal-hardware path for
    /// legacy Intel integrated GPUs (pure wgpu Vulkan 1.1 / DX12 / Metal core).
    pub gpu_backend: RenderBackendPreference,
    /// Selected shaderpack: an external OptiFine/Iris/DistantHorizons pack
    /// (zip or directory) or a Megu native WGSL pack (`megu-shader.toml`).
    /// `None` — the default on every preset — keeps built-in rendering;
    /// packs that exceed the GPU's capabilities fail closed back to it.
    pub shader_pack: Option<String>,
    /// Whether the selected shader pack is active.
    pub shaders_enabled: bool,
    pub builtin_looking_at_hud: bool,
    pub inventory_drag_controls: bool,
    pub builtin_inventory_sort: bool,
    pub zsh_command_bar: bool,
    pub compact_debug_hud_enabled: bool,
    pub compact_debug_hud_segments: u16,
    /// Optional `profile.<NAME>` inside the pack's `shaders.properties`
    /// activated on load; `None` uses the pack's own defaults.
    pub shader_profile: Option<String>,
    /// Map-mod fair-play policy applied when this client hosts a world
    /// (integrated server, LAN share, friend relay); ignored on plain
    /// client joins to a remote server.
    pub map_fair_play: HostMapFairPlaySettings,
    /// Quality-of-life toggles (audio limiter, zoom, tool warnings, chat
    /// history) persisted under the `[qol]` TOML table.
    pub qol: ClientQolSettings,
    /// Seamless tiny-annoyance QoL helpers (auto-reconnect, replenish,
    /// quick stack, food preview, shulker peek, chat scroll-lock,
    /// toasts), persisted in the same `[qol]` table.
    pub seamless_qol: ClientSeamlessQolSettings,
    /// Crash-report consent policy (`crash_report_mode`): `ask` (the
    /// default) writes reports locally and asks before transmitting,
    /// `auto_crash_only` relays crash fingerprints without prompting, and
    /// `off` disables submission entirely.
    pub crash_report_mode: CrashReportMode,
    /// Optional report relay endpoint (`report_endpoint`):
    /// `https://<host>` or loopback `http://127.0.0.1:<port>/...` — the
    /// same allowlist [`crate::crash_reporter::parse_report_url`]
    /// enforces. `None` — the default — means zero network egress:
    /// reports stay on disk under `crash-reports/` no matter the mode.
    pub report_endpoint: Option<String>,
    pub fullscreen: bool,
    pub mouse_invert_y: bool,
    pub gamma_pct: u16,
    /// `0` = automatic, `1..=4` are fixed UI scale factors.
    pub gui_scale: u8,
    pub keys: KeyBindings,
}

impl Default for ClientSettings {
    fn default() -> Self {
        Self::from_preset(QualityPreset::Balanced)
    }
}

// `ClientQolSettings` carries f32 fields, so `Eq` cannot be derived. Every
// float is clamped to a finite range on every construction/parse path, so
// reflexivity holds in practice; `Eq` is still required by `CliConfig`'s
// derive in `cli.rs`.
impl Eq for ClientSettings {}

impl ClientSettings {
    /// One named bundle. `Custom` seeds the same defaults as `Balanced`;
    /// the preset merely records that the player then tuned fields.
    pub fn from_preset(preset: QualityPreset) -> Self {
        let mut settings = Self {
            preset,
            view_distance: DEFAULT_VIEW_DISTANCE,
            distant_horizon: DistantHorizon::Off,
            clouds: CloudMode::Fancy,
            particles: ParticleLevel::All,
            smooth_lighting: true,
            entity_shadows: true,
            comfort_exposure: true,
            dynamic_lights: true,
            enchantment_glint: true,
            indirect_light_bounce: true,
            directional_shadow_gameplay: false,
            lazy_catchup_gameplay: false,
            fov: DEFAULT_FOV,
            mouse_sensitivity_pct: 100,
            volume_master: 50,
            volume_music: 45,
            volume_blocks: 100,
            volume_hostile: 100,
            volume_friendly: 100,
            vsync: true,
            max_fps: None,
            background_fps_limit: DEFAULT_BACKGROUND_FPS_LIMIT,
            ram_budget_mib: 0,
            gpu_backend: RenderBackendPreference::Auto,
            shader_pack: None,
            shaders_enabled: false,
            builtin_looking_at_hud: true,
            inventory_drag_controls: true,
            builtin_inventory_sort: true,
            zsh_command_bar: true,
            compact_debug_hud_enabled: false,
            compact_debug_hud_segments: 0b1_1111,
            shader_profile: None,
            map_fair_play: HostMapFairPlaySettings::default(),
            qol: ClientQolSettings::default(),
            seamless_qol: ClientSeamlessQolSettings::default(),
            crash_report_mode: CrashReportMode::default(),
            report_endpoint: None,
            fullscreen: false,
            mouse_invert_y: false,
            gamma_pct: 100,
            gui_scale: 0,
            keys: KeyBindings::default(),
        };
        match preset {
            QualityPreset::Fast => {
                settings.view_distance = 8;
                settings.clouds = CloudMode::Fast;
                settings.particles = ParticleLevel::Decreased;
                settings.smooth_lighting = false;
                settings.entity_shadows = false;
            }
            QualityPreset::High => {
                settings.view_distance = 16;
                settings.distant_horizon = DistantHorizon::Medium;
            }
            QualityPreset::Ultra => {
                settings.view_distance = MAX_VIEW_DISTANCE;
                settings.distant_horizon = DistantHorizon::Far;
            }
            QualityPreset::Balanced | QualityPreset::Custom => {}
        }
        settings
    }

    /// Derive the [`LodConfig`] for the chunk-LOD subsystem, or `None` when
    /// `distant_horizon` is `Off`. `view_distance` becomes the full-detail
    /// radius and the horizon maps straight onto the coarse-LOD radius;
    /// fine LOD fills the midpoint so the tier ordering invariant
    /// `full <= fine <= coarse` always holds.
    pub fn to_lod_config(&self) -> Option<LodConfig> {
        let coarse = match self.distant_horizon {
            DistantHorizon::Off => return None,
            DistantHorizon::Medium => DISTANT_HORIZON_MEDIUM_CHUNKS,
            DistantHorizon::Far => DISTANT_HORIZON_FAR_CHUNKS,
        };
        let full = i32::from(self.view_distance);
        Some(LodConfig {
            full_detail_radius: full,
            fine_lod_radius: (full + coarse) / 2,
            coarse_lod_radius: coarse,
            max_memory_bytes: LOD_MEMORY_BUDGET_BYTES,
            ..LodConfig::default()
        })
    }

    /// Engine optimizations active for this session. Always
    /// [`BuiltInOptimizations::ALL_ON`]: these flags are intentionally not
    /// loadable from the user config so players never face a technical
    /// toggle that only makes the game slower when flipped.
    pub fn engine_optimizations(&self) -> BuiltInOptimizations {
        let _ = self;
        BuiltInOptimizations::ALL_ON
    }

    /// Serialize to the human-readable `megu-client.toml` format.
    pub fn to_toml(&self) -> String {
        let max_fps = self
            .max_fps
            .map_or_else(|| "\"unlimited\"".to_string(), |v| v.to_string());
        // Shader selectors are omitted entirely when unset so a default
        // file carries no shaderpack noise; [`is_valid_shader_selector`]
        // keeps programmatically-set unwritable values from producing
        // malformed TOML.
        let mut shader_lines = String::new();
        if let Some(pack) = &self.shader_pack {
            if is_valid_shader_selector(pack) {
                shader_lines.push_str(&format!("shader_pack = \"{pack}\"\n"));
            }
        }
        if let Some(profile) = &self.shader_profile {
            if is_valid_shader_selector(profile) {
                shader_lines.push_str(&format!("shader_profile = \"{profile}\"\n"));
            }
        }
        shader_lines.push_str(&format!("shaders_enabled = {}\n", self.shaders_enabled));
        shader_lines.push_str(&format!(
            "builtin_looking_at_hud = {}\n",
            self.builtin_looking_at_hud
        ));
        shader_lines.push_str(&format!(
            "inventory_drag_controls = {}\n",
            self.inventory_drag_controls
        ));
        shader_lines.push_str(&format!(
            "builtin_inventory_sort = {}\n",
            self.builtin_inventory_sort
        ));
        shader_lines.push_str(&format!("zsh_command_bar = {}\n", self.zsh_command_bar));
        shader_lines.push_str(&format!(
            "compact_debug_hud_enabled = {}\n",
            self.compact_debug_hud_enabled
        ));
        shader_lines.push_str(&format!(
            "compact_debug_hud_segments = {}\n",
            self.compact_debug_hud_segments
        ));
        // The report endpoint is omitted entirely when unset so a default
        // file configures no egress; only values that still pass the
        // transport URL allowlist are ever written back out.
        let mut report_line = String::new();
        if let Some(endpoint) = &self.report_endpoint {
            if parse_report_url(endpoint).is_ok() {
                report_line.push_str(&format!("report_endpoint = \"{endpoint}\"\n"));
            }
        }
        format!(
            "# megu client settings\n\
             preset = \"{}\"\n\
             view_distance = {}\n\
             distant_horizon = \"{}\"\n\
             clouds = \"{}\"\n\
             particles = \"{}\"\n\
             smooth_lighting = {}\n\
             entity_shadows = {}\n\
             comfort_exposure = {}\n\
             dynamic_lights = {}\n\
             enchantment_glint = {}\n\
             indirect_light_bounce = {}\n\
             directional_shadow_gameplay = {}\n\
             lazy_catchup_gameplay = {}\n\
             fov = {}\n\
             mouse_sensitivity_pct = {}\n\
             volume_master = {}\n\
             volume_music = {}\n\
             volume_blocks = {}\n\
             volume_hostile = {}\n\
             volume_friendly = {}\n\
             vsync = {}\n\
             max_fps = {}\n\
             background_fps_limit = {}\n\
             ram_budget_mib = {}\n\
             gpu_backend = \"{}\"\n\
             fullscreen = {}\n\
             mouse_invert_y = {}\n\
             gamma_pct = {}\n\
             gui_scale = {}\n\
             crash_report_mode = \"{}\"\n\
             {shader_lines}\
             {report_line}\
             \n\
             [host_map_fair_play]\n\
             minimap_fair_play = {}\n\
             allow_cave_map = {}\n\
             allow_entity_radar = {}\n\
             allow_waypoint_teleport = {}\n\
             minimap_mode = \"{}\"\n\
             allow_mob_radar = {}\n\
             player_radar_scope = \"{}\"\n\
             \n\
             [qol]\n\
             max_volume_cap = {}\n\
             max_concurrent_same_sound = {}\n\
             zoom_divisor = {}\n\
             tool_warn_percent = {}\n\
             prevent_tool_break = {}\n\
             chat_history_lines = {}\n\
             retain_chat_on_reconnect = {}\n\
             auto_reconnect = {}\n\
             auto_replenish = {}\n\
             quick_stack_nearby = {}\n\
             food_saturation_preview = {}\n\
             shulker_tooltip_peek = {}\n\
             chat_scroll_lock = {}\n\
             toast_notifications = {}\n\
             recipe_overlay_enabled = {}\n",
            self.preset.as_str(),
            self.view_distance,
            self.distant_horizon.as_str(),
            self.clouds.as_str(),
            self.particles.as_str(),
            self.smooth_lighting,
            self.entity_shadows,
            self.comfort_exposure,
            self.dynamic_lights,
            self.enchantment_glint,
            self.indirect_light_bounce,
            self.directional_shadow_gameplay,
            self.lazy_catchup_gameplay,
            self.fov,
            self.mouse_sensitivity_pct,
            self.volume_master,
            self.volume_music,
            self.volume_blocks,
            self.volume_hostile,
            self.volume_friendly,
            self.vsync,
            max_fps,
            self.background_fps_limit,
            self.ram_budget_mib,
            self.gpu_backend.as_str(),
            self.fullscreen,
            self.mouse_invert_y,
            self.gamma_pct,
            self.gui_scale,
            self.crash_report_mode.as_str(),
            self.map_fair_play.minimap_fair_play,
            self.map_fair_play.allow_cave_map,
            self.map_fair_play.allow_entity_radar,
            self.map_fair_play.allow_waypoint_teleport,
            self.map_fair_play.minimap_mode.as_str(),
            self.map_fair_play.allow_mob_radar,
            self.map_fair_play.player_radar_scope.as_str(),
            self.qol.max_volume_cap,
            self.qol.max_concurrent_same_sound,
            self.qol.zoom_divisor,
            self.qol.tool_warn_percent,
            self.qol.prevent_tool_break,
            self.qol.chat_history_lines,
            self.qol.retain_chat_on_reconnect,
            self.seamless_qol.auto_reconnect,
            self.seamless_qol.auto_replenish,
            self.seamless_qol.quick_stack_nearby,
            self.seamless_qol.food_saturation_preview,
            self.seamless_qol.shulker_tooltip_peek,
            self.seamless_qol.chat_scroll_lock,
            self.seamless_qol.toast_notifications,
            self.seamless_qol.recipe_overlay_enabled,
        ) + &format!(
            "\n[keys]\nforward = \"{}\"\nback = \"{}\"\nleft = \"{}\"\nright = \"{}\"\njump = \"{}\"\nsneak = \"{}\"\nsprint = \"{}\"\ninventory = \"{}\"\ndrop = \"{}\"\nswap_offhand = \"{}\"\nchat = \"{}\"\ncommand = \"{}\"\nzoom = \"{}\"\ncamera_toggle = \"{}\"\nfullscreen = \"{}\"\nadvancements = \"{}\"\nsort_inventory = \"{}\"\n",
            self.keys.forward,
            self.keys.back,
            self.keys.left,
            self.keys.right,
            self.keys.jump,
            self.keys.sneak,
            self.keys.sprint,
            self.keys.inventory,
            self.keys.drop,
            self.keys.swap_offhand,
            self.keys.chat,
            self.keys.command,
            self.keys.zoom,
            self.keys.camera_toggle,
            self.keys.fullscreen,
            self.keys.advancements,
            self.keys.sort_inventory,
        )
    }

    /// Parse a `megu-client.toml` document. Returns the resolved
    /// settings plus one warning string per unknown key, invalid value, or
    /// clamped out-of-range value — malformed files degrade gracefully to
    /// defaults instead of failing.
    pub fn from_toml(text: &str) -> (ClientSettings, Vec<String>) {
        let (settings, warnings, _) = Self::parse_toml(text);
        (settings, warnings)
    }

    /// Apply only the keys a `megu-client.toml` document actually
    /// defines onto `self`, keeping every field the file does not mention.
    /// Returns the same warnings [`ClientSettings::from_toml`] would.
    pub fn apply_toml(&mut self, text: &str) -> Vec<String> {
        let (parsed, warnings, seen) = Self::parse_toml(text);
        for key in seen {
            match key {
                "preset" => self.preset = parsed.preset,
                "view_distance" => self.view_distance = parsed.view_distance,
                "distant_horizon" => self.distant_horizon = parsed.distant_horizon,
                "clouds" => self.clouds = parsed.clouds,
                "particles" => self.particles = parsed.particles,
                "smooth_lighting" => self.smooth_lighting = parsed.smooth_lighting,
                "entity_shadows" => self.entity_shadows = parsed.entity_shadows,
                "comfort_exposure" => self.comfort_exposure = parsed.comfort_exposure,
                "dynamic_lights" => self.dynamic_lights = parsed.dynamic_lights,
                "enchantment_glint" => self.enchantment_glint = parsed.enchantment_glint,
                "indirect_light_bounce" => {
                    self.indirect_light_bounce = parsed.indirect_light_bounce;
                }
                "directional_shadow_gameplay" => {
                    self.directional_shadow_gameplay =
                        parsed.directional_shadow_gameplay;
                }
                "lazy_catchup_gameplay" => {
                    self.lazy_catchup_gameplay = parsed.lazy_catchup_gameplay;
                }
                "fov" => self.fov = parsed.fov,
                "mouse_sensitivity_pct" => {
                    self.mouse_sensitivity_pct = parsed.mouse_sensitivity_pct;
                }
                "volume_master" => self.volume_master = parsed.volume_master,
                "volume_music" => self.volume_music = parsed.volume_music,
                "volume_blocks" => self.volume_blocks = parsed.volume_blocks,
                "volume_hostile" => self.volume_hostile = parsed.volume_hostile,
                "volume_friendly" => self.volume_friendly = parsed.volume_friendly,
                "vsync" => self.vsync = parsed.vsync,
                "max_fps" => self.max_fps = parsed.max_fps,
                "background_fps_limit" => {
                    self.background_fps_limit = parsed.background_fps_limit;
                }
                "ram_budget_mib" => self.ram_budget_mib = parsed.ram_budget_mib,
                "gpu_backend" => self.gpu_backend = parsed.gpu_backend,
                "fullscreen" => self.fullscreen = parsed.fullscreen,
                "mouse_invert_y" => self.mouse_invert_y = parsed.mouse_invert_y,
                "gamma_pct" => self.gamma_pct = parsed.gamma_pct,
                "gui_scale" => self.gui_scale = parsed.gui_scale,
                "keys.forward" => self.keys.forward = parsed.keys.forward.clone(),
                "keys.back" => self.keys.back = parsed.keys.back.clone(),
                "keys.left" => self.keys.left = parsed.keys.left.clone(),
                "keys.right" => self.keys.right = parsed.keys.right.clone(),
                "keys.jump" => self.keys.jump = parsed.keys.jump.clone(),
                "keys.sneak" => self.keys.sneak = parsed.keys.sneak.clone(),
                "keys.sprint" => self.keys.sprint = parsed.keys.sprint.clone(),
                "keys.inventory" => self.keys.inventory = parsed.keys.inventory.clone(),
                "keys.drop" => self.keys.drop = parsed.keys.drop.clone(),
                "keys.swap_offhand" => self.keys.swap_offhand = parsed.keys.swap_offhand.clone(),
                "keys.chat" => self.keys.chat = parsed.keys.chat.clone(),
                "keys.command" => self.keys.command = parsed.keys.command.clone(),
                "keys.zoom" => self.keys.zoom = parsed.keys.zoom.clone(),
                "keys.camera_toggle" => self.keys.camera_toggle = parsed.keys.camera_toggle.clone(),
                "keys.fullscreen" => self.keys.fullscreen = parsed.keys.fullscreen.clone(),
                "keys.advancements" => self.keys.advancements = parsed.keys.advancements.clone(),
                "keys.sort_inventory" => {
                    self.keys.sort_inventory = parsed.keys.sort_inventory.clone()
                }
                "shader_pack" => self.shader_pack = parsed.shader_pack.clone(),
                "shaders_enabled" => self.shaders_enabled = parsed.shaders_enabled,
                "builtin_looking_at_hud" => {
                    self.builtin_looking_at_hud = parsed.builtin_looking_at_hud
                }
                "inventory_drag_controls" => {
                    self.inventory_drag_controls = parsed.inventory_drag_controls
                }
                "builtin_inventory_sort" => {
                    self.builtin_inventory_sort = parsed.builtin_inventory_sort
                }
                "zsh_command_bar" => self.zsh_command_bar = parsed.zsh_command_bar,
                "compact_debug_hud_enabled" => self.compact_debug_hud_enabled = parsed.compact_debug_hud_enabled,
                "compact_debug_hud_segments" => {
                    self.compact_debug_hud_segments = parsed.compact_debug_hud_segments
                }
                "shader_profile" => self.shader_profile = parsed.shader_profile.clone(),
                "minimap_fair_play" | "minimap_mode" => {
                    // The mirrored pair is already consistent in `parsed`.
                    self.map_fair_play.minimap_fair_play = parsed.map_fair_play.minimap_fair_play;
                    self.map_fair_play.minimap_mode = parsed.map_fair_play.minimap_mode;
                }
                "allow_cave_map" => {
                    self.map_fair_play.allow_cave_map = parsed.map_fair_play.allow_cave_map;
                }
                "allow_entity_radar" | "allow_mob_radar" => {
                    self.map_fair_play.allow_entity_radar = parsed.map_fair_play.allow_entity_radar;
                    self.map_fair_play.allow_mob_radar = parsed.map_fair_play.allow_mob_radar;
                }
                "player_radar_scope" => {
                    self.map_fair_play.player_radar_scope = parsed.map_fair_play.player_radar_scope;
                }
                "allow_waypoint_teleport" => {
                    self.map_fair_play.allow_waypoint_teleport =
                        parsed.map_fair_play.allow_waypoint_teleport;
                }
                "max_volume_cap" => self.qol.max_volume_cap = parsed.qol.max_volume_cap,
                "max_concurrent_same_sound" => {
                    self.qol.max_concurrent_same_sound = parsed.qol.max_concurrent_same_sound;
                }
                "zoom_divisor" => self.qol.zoom_divisor = parsed.qol.zoom_divisor,
                "tool_warn_percent" => {
                    self.qol.tool_warn_percent = parsed.qol.tool_warn_percent;
                }
                "prevent_tool_break" => {
                    self.qol.prevent_tool_break = parsed.qol.prevent_tool_break;
                }
                "chat_history_lines" => {
                    self.qol.chat_history_lines = parsed.qol.chat_history_lines;
                }
                "retain_chat_on_reconnect" => {
                    self.qol.retain_chat_on_reconnect = parsed.qol.retain_chat_on_reconnect;
                }
                "auto_reconnect" => self.seamless_qol.auto_reconnect = parsed.seamless_qol.auto_reconnect,
                "auto_replenish" => self.seamless_qol.auto_replenish = parsed.seamless_qol.auto_replenish,
                "quick_stack_nearby" => {
                    self.seamless_qol.quick_stack_nearby = parsed.seamless_qol.quick_stack_nearby;
                }
                "food_saturation_preview" => {
                    self.seamless_qol.food_saturation_preview = parsed.seamless_qol.food_saturation_preview;
                }
                "shulker_tooltip_peek" => {
                    self.seamless_qol.shulker_tooltip_peek = parsed.seamless_qol.shulker_tooltip_peek;
                }
                "chat_scroll_lock" => self.seamless_qol.chat_scroll_lock = parsed.seamless_qol.chat_scroll_lock,
                "toast_notifications" => {
                    self.seamless_qol.toast_notifications = parsed.seamless_qol.toast_notifications;
                }
                "recipe_overlay_enabled" => {
                    self.seamless_qol.recipe_overlay_enabled =
                        parsed.seamless_qol.recipe_overlay_enabled;
                }
                "crash_report_mode" => self.crash_report_mode = parsed.crash_report_mode,
                "report_endpoint" => self.report_endpoint = parsed.report_endpoint.clone(),
                _ => {}
            }
        }
        warnings
    }

    /// Write [`ClientSettings::to_toml`] to `path`, creating parent
    /// directories as needed.
    pub fn write_toml(&self, path: &Path) -> Result<(), String> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
            }
        }
        std::fs::write(path, self.to_toml())
            .map_err(|e| format!("cannot write settings file {}: {e}", path.display()))
    }

    /// Shared TOML parser: resolved settings, warnings, and the list of
    /// keys that parsed successfully (so overlays only touch what the
    /// document defined).
    fn parse_toml(text: &str) -> (ClientSettings, Vec<String>, Vec<&'static str>) {
        let mut settings = ClientSettings::default();
        let mut warnings = Vec::new();
        let mut seen: Vec<&'static str> = Vec::new();
        let mut section = "";
        for raw_line in text.lines() {
            let line = raw_line.split('#').next().unwrap_or("").trim();
            if line.is_empty() {
                continue;
            }
            if line.starts_with('[') && line.ends_with(']') {
                section = line[1..line.len() - 1].trim();
                continue;
            }
            let Some((key, raw_value)) = line.split_once('=') else {
                warnings.push(format!("ignored malformed line '{line}'"));
                continue;
            };
            let key = key.trim();
            let value = unquote(raw_value.trim());
            if section == "keys" {
                let slot = match key {
                    "forward" => (&mut settings.keys.forward, "keys.forward"),
                    "back" => (&mut settings.keys.back, "keys.back"),
                    "left" => (&mut settings.keys.left, "keys.left"),
                    "right" => (&mut settings.keys.right, "keys.right"),
                    "jump" => (&mut settings.keys.jump, "keys.jump"),
                    "sneak" => (&mut settings.keys.sneak, "keys.sneak"),
                    "sprint" => (&mut settings.keys.sprint, "keys.sprint"),
                    "inventory" => (&mut settings.keys.inventory, "keys.inventory"),
                    "drop" => (&mut settings.keys.drop, "keys.drop"),
                    "swap_offhand" => (&mut settings.keys.swap_offhand, "keys.swap_offhand"),
                    "chat" => (&mut settings.keys.chat, "keys.chat"),
                    "command" => (&mut settings.keys.command, "keys.command"),
                    "zoom" => (&mut settings.keys.zoom, "keys.zoom"),
                    "camera_toggle" => (&mut settings.keys.camera_toggle, "keys.camera_toggle"),
                    "fullscreen" => (&mut settings.keys.fullscreen, "keys.fullscreen"),
                    "advancements" => (&mut settings.keys.advancements, "keys.advancements"),
                    "sort_inventory" => (&mut settings.keys.sort_inventory, "keys.sort_inventory"),
                    _ => {
                        warnings.push(format!("unknown key '{key}' in [keys]"));
                        continue;
                    }
                };
                if !value.is_empty()
                    && value.len() <= 32
                    && value
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'_')
                {
                    *slot.0 = value.to_owned();
                    seen.push(slot.1);
                } else {
                    warnings.push(format!("invalid value for '{key}': '{value}'"));
                }
                continue;
            }
            let invalid = |warnings: &mut Vec<String>| {
                warnings.push(format!("invalid value for '{key}': '{value}'"));
            };
            match key {
                "preset" => match QualityPreset::parse(value) {
                    Some(p) => {
                        settings.preset = p;
                        seen.push("preset");
                    }
                    None => invalid(&mut warnings),
                },
                "view_distance" => match value.parse::<i64>() {
                    Ok(v) => {
                        settings.view_distance = clamp_u8(
                            v,
                            i64::from(MIN_VIEW_DISTANCE),
                            i64::from(MAX_VIEW_DISTANCE),
                            key,
                            &mut warnings,
                        );
                        seen.push("view_distance");
                    }
                    Err(_) => invalid(&mut warnings),
                },
                "distant_horizon" => match DistantHorizon::parse(value) {
                    Some(d) => {
                        settings.distant_horizon = d;
                        seen.push("distant_horizon");
                    }
                    None => invalid(&mut warnings),
                },
                "clouds" => match CloudMode::parse(value) {
                    Some(c) => {
                        settings.clouds = c;
                        seen.push("clouds");
                    }
                    None => invalid(&mut warnings),
                },
                "particles" => match ParticleLevel::parse(value) {
                    Some(p) => {
                        settings.particles = p;
                        seen.push("particles");
                    }
                    None => invalid(&mut warnings),
                },
                "smooth_lighting" => match parse_bool(value) {
                    Some(b) => {
                        settings.smooth_lighting = b;
                        seen.push("smooth_lighting");
                    }
                    None => invalid(&mut warnings),
                },
                "entity_shadows" => match parse_bool(value) {
                    Some(b) => {
                        settings.entity_shadows = b;
                        seen.push("entity_shadows");
                    }
                    None => invalid(&mut warnings),
                },
                "comfort_exposure" => match parse_bool(value) {
                    Some(b) => {
                        settings.comfort_exposure = b;
                        seen.push("comfort_exposure");
                    }
                    None => invalid(&mut warnings),
                },
                "dynamic_lights" => match parse_bool(value) {
                    Some(b) => {
                        settings.dynamic_lights = b;
                        seen.push("dynamic_lights");
                    }
                    None => invalid(&mut warnings),
                },
                "enchantment_glint" => match parse_bool(value) {
                    Some(b) => {
                        settings.enchantment_glint = b;
                        seen.push("enchantment_glint");
                    }
                    None => invalid(&mut warnings),
                },
                "indirect_light_bounce" => match parse_bool(value) {
                    Some(b) => {
                        settings.indirect_light_bounce = b;
                        seen.push("indirect_light_bounce");
                    }
                    None => invalid(&mut warnings),
                },
                "directional_shadow_gameplay" => match parse_bool(value) {
                    Some(b) => {
                        settings.directional_shadow_gameplay = b;
                        seen.push("directional_shadow_gameplay");
                    }
                    None => invalid(&mut warnings),
                },
                "lazy_catchup_gameplay" => match parse_bool(value) {
                    Some(b) => {
                        settings.lazy_catchup_gameplay = b;
                        seen.push("lazy_catchup_gameplay");
                    }
                    None => invalid(&mut warnings),
                },
                "fov" => match value.parse::<i64>() {
                    Ok(v) => {
                        settings.fov = clamp_u8(
                            v,
                            i64::from(MIN_FOV),
                            i64::from(MAX_FOV),
                            key,
                            &mut warnings,
                        );
                        seen.push("fov");
                    }
                    Err(_) => invalid(&mut warnings),
                },
                "mouse_sensitivity_pct" => match value.parse::<i64>() {
                    Ok(v) => {
                        settings.mouse_sensitivity_pct =
                            clamp_i64(v, 10, 200, key, &mut warnings) as u16;
                        seen.push("mouse_sensitivity_pct");
                    }
                    Err(_) => invalid(&mut warnings),
                },
                "volume_master" | "volume_music" | "volume_blocks" | "volume_hostile"
                | "volume_friendly" => match value.parse::<i64>() {
                    Ok(v) => {
                        let volume = clamp_u8(v, 0, 100, key, &mut warnings);
                        let canonical: &'static str = match key {
                            "volume_master" => {
                                settings.volume_master = volume;
                                "volume_master"
                            }
                            "volume_music" => {
                                settings.volume_music = volume;
                                "volume_music"
                            }
                            "volume_blocks" => {
                                settings.volume_blocks = volume;
                                "volume_blocks"
                            }
                            "volume_hostile" => {
                                settings.volume_hostile = volume;
                                "volume_hostile"
                            }
                            _ => {
                                settings.volume_friendly = volume;
                                "volume_friendly"
                            }
                        };
                        seen.push(canonical);
                    }
                    Err(_) => invalid(&mut warnings),
                },
                "vsync" => match parse_bool(value) {
                    Some(b) => {
                        settings.vsync = b;
                        seen.push("vsync");
                    }
                    None => invalid(&mut warnings),
                },
                "max_fps" => {
                    if value.eq_ignore_ascii_case("unlimited") || value.eq_ignore_ascii_case("none")
                    {
                        settings.max_fps = None;
                        seen.push("max_fps");
                    } else {
                        match value.parse::<i64>() {
                            Ok(0) => {
                                settings.max_fps = None;
                                seen.push("max_fps");
                            }
                            Ok(v) => {
                                settings.max_fps = Some(clamp_u32(
                                    v,
                                    i64::from(MIN_MAX_FPS),
                                    i64::from(MAX_MAX_FPS),
                                    key,
                                    &mut warnings,
                                ));
                                seen.push("max_fps");
                            }
                            Err(_) => invalid(&mut warnings),
                        }
                    }
                }
                "background_fps_limit" => match value.parse::<i64>() {
                    Ok(v) => {
                        settings.background_fps_limit =
                            clamp_i64(v, 0, i64::from(MAX_BACKGROUND_FPS), key, &mut warnings)
                                as u16;
                        seen.push("background_fps_limit");
                    }
                    Err(_) => invalid(&mut warnings),
                },
                "ram_budget_mib" => match value.parse::<i64>() {
                    Ok(v) => {
                        // `0` = Auto; anything above clamps to u32::MAX —
                        // the live OOM guard, not this cap, is the real
                        // protection against host memory exhaustion.
                        settings.ram_budget_mib =
                            clamp_u32(v, 0, i64::from(u32::MAX), key, &mut warnings);
                        seen.push("ram_budget_mib");
                    }
                    Err(_) => invalid(&mut warnings),
                },
                "gpu_backend" => match RenderBackendPreference::parse(value) {
                    Some(g) => {
                        settings.gpu_backend = g;
                        seen.push("gpu_backend");
                    }
                    None => invalid(&mut warnings),
                },
                "fullscreen" | "mouse_invert_y" => match parse_bool(value) {
                    Some(value) => {
                        if key == "fullscreen" {
                            settings.fullscreen = value;
                        } else {
                            settings.mouse_invert_y = value;
                        }
                        seen.push(if key == "fullscreen" {
                            "fullscreen"
                        } else {
                            "mouse_invert_y"
                        });
                    }
                    None => invalid(&mut warnings),
                },
                "gamma_pct" => match value.parse::<i64>() {
                    Ok(value) => {
                        settings.gamma_pct = clamp_i64(value, 50, 200, key, &mut warnings) as u16;
                        seen.push("gamma_pct");
                    }
                    Err(_) => invalid(&mut warnings),
                },
                "gui_scale" => match value.parse::<i64>() {
                    Ok(value) => {
                        settings.gui_scale = clamp_i64(value, 0, 4, key, &mut warnings) as u8;
                        seen.push("gui_scale");
                    }
                    Err(_) => invalid(&mut warnings),
                },
                "shader_pack" => match sanitize_shader_selector(value) {
                    Some(v) => {
                        settings.shader_pack = Some(v);
                        seen.push("shader_pack");
                    }
                    None => invalid(&mut warnings),
                },
                "shaders_enabled" => match parse_bool(value) {
                    Some(v) => {
                        settings.shaders_enabled = v;
                        seen.push("shaders_enabled");
                    }
                    None => invalid(&mut warnings),
                },
                "builtin_looking_at_hud" => match parse_bool(value) {
                    Some(v) => {
                        settings.builtin_looking_at_hud = v;
                        seen.push("builtin_looking_at_hud");
                    }
                    None => invalid(&mut warnings),
                },
                "inventory_drag_controls" | "builtin_mouse_tweaks" => match parse_bool(value) {
                    Some(v) => {
                        settings.inventory_drag_controls = v;
                        seen.push("inventory_drag_controls");
                    }
                    None => invalid(&mut warnings),
                },
                "builtin_inventory_sort" => match parse_bool(value) {
                    Some(v) => {
                        settings.builtin_inventory_sort = v;
                        seen.push("builtin_inventory_sort");
                    }
                    None => invalid(&mut warnings),
                },
                "zsh_command_bar" => match parse_bool(value) {
                    Some(v) => {
                        settings.zsh_command_bar = v;
                        seen.push("zsh_command_bar");
                    }
                    None => invalid(&mut warnings),
                },
                "compact_debug_hud_enabled" | "mini_hud_enabled" => match parse_bool(value) {
                    Some(v) => {
                        settings.compact_debug_hud_enabled = v;
                        seen.push("compact_debug_hud_enabled");
                    }
                    None => invalid(&mut warnings),
                },
                "compact_debug_hud_segments" | "mini_hud_segments" => match value.parse::<i64>() {
                    Ok(v) => {
                        settings.compact_debug_hud_segments =
                            clamp_i64(v, 0, 511, key, &mut warnings) as u16;
                        seen.push("compact_debug_hud_segments");
                    }
                    Err(_) => invalid(&mut warnings),
                },
                "shader_profile" => match sanitize_shader_selector(value) {
                    Some(v) => {
                        settings.shader_profile = Some(v);
                        seen.push("shader_profile");
                    }
                    None => invalid(&mut warnings),
                },
                "crash_report_mode" => match CrashReportMode::parse(value) {
                    Some(m) => {
                        settings.crash_report_mode = m;
                        seen.push("crash_report_mode");
                    }
                    None => invalid(&mut warnings),
                },
                "report_endpoint" => {
                    if value.is_empty() {
                        // An explicit empty string clears a configured
                        // endpoint, restoring the no-egress default.
                        settings.report_endpoint = None;
                        seen.push("report_endpoint");
                    } else {
                        match parse_report_url(value) {
                            // Fail closed: only allowlisted URLs are stored.
                            Ok(_) => {
                                settings.report_endpoint = Some(value.to_string());
                                seen.push("report_endpoint");
                            }
                            Err(err) => warnings
                                .push(format!("invalid value for '{key}': '{value}' ({err})")),
                        }
                    }
                }
                "minimap_fair_play" => match parse_bool(value) {
                    Some(b) => {
                        // Legacy master switch maps onto the granular
                        // mode: `true` is the pre-granular full lock gate,
                        // `false` leaves minimaps unrestricted.
                        settings.map_fair_play.minimap_fair_play = b;
                        settings.map_fair_play.minimap_mode = if b {
                            HostMinimapMode::FairPlay
                        } else {
                            HostMinimapMode::Unrestricted
                        };
                        seen.push("minimap_fair_play");
                    }
                    None => invalid(&mut warnings),
                },
                "minimap_mode" => match HostMinimapMode::parse(value) {
                    Some(m) => {
                        settings.map_fair_play.minimap_mode = m;
                        settings.map_fair_play.minimap_fair_play =
                            m != HostMinimapMode::Unrestricted;
                        seen.push("minimap_mode");
                    }
                    None => invalid(&mut warnings),
                },
                "allow_cave_map" => match parse_bool(value) {
                    Some(b) => {
                        settings.map_fair_play.allow_cave_map = b;
                        seen.push("allow_cave_map");
                    }
                    None => invalid(&mut warnings),
                },
                "allow_entity_radar" => match parse_bool(value) {
                    Some(b) => {
                        // One switch, two spellings — keep the mirror in
                        // sync no matter which the file uses.
                        settings.map_fair_play.allow_entity_radar = b;
                        settings.map_fair_play.allow_mob_radar = b;
                        seen.push("allow_entity_radar");
                    }
                    None => invalid(&mut warnings),
                },
                "allow_mob_radar" => match parse_bool(value) {
                    Some(b) => {
                        settings.map_fair_play.allow_entity_radar = b;
                        settings.map_fair_play.allow_mob_radar = b;
                        seen.push("allow_mob_radar");
                    }
                    None => invalid(&mut warnings),
                },
                "player_radar_scope" => match HostPlayerRadarScope::parse(value) {
                    Some(s) => {
                        settings.map_fair_play.player_radar_scope = s;
                        seen.push("player_radar_scope");
                    }
                    None => invalid(&mut warnings),
                },
                "allow_waypoint_teleport" => match parse_bool(value) {
                    Some(b) => {
                        settings.map_fair_play.allow_waypoint_teleport = b;
                        seen.push("allow_waypoint_teleport");
                    }
                    None => invalid(&mut warnings),
                },
                "max_volume_cap" => match value.parse::<f32>() {
                    Ok(v) if v.is_finite() => {
                        settings.qol.max_volume_cap =
                            clamp_f32(v, 0.0, MAX_VOLUME_CAP, key, &mut warnings);
                        seen.push("max_volume_cap");
                    }
                    _ => invalid(&mut warnings),
                },
                "max_concurrent_same_sound" => match value.parse::<i64>() {
                    Ok(v) => {
                        settings.qol.max_concurrent_same_sound = clamp_i64(
                            v,
                            i64::from(MIN_MAX_CONCURRENT_SAME_SOUND),
                            i64::from(MAX_MAX_CONCURRENT_SAME_SOUND),
                            key,
                            &mut warnings,
                        ) as u8;
                        seen.push("max_concurrent_same_sound");
                    }
                    Err(_) => invalid(&mut warnings),
                },
                "zoom_divisor" => match value.parse::<f32>() {
                    Ok(v) if v.is_finite() => {
                        settings.qol.zoom_divisor =
                            clamp_f32(v, MIN_ZOOM_DIVISOR, MAX_ZOOM_DIVISOR, key, &mut warnings);
                        seen.push("zoom_divisor");
                    }
                    _ => invalid(&mut warnings),
                },
                "tool_warn_percent" => match value.parse::<i64>() {
                    Ok(v) => {
                        settings.qol.tool_warn_percent =
                            clamp_i64(v, 0, i64::from(MAX_TOOL_WARN_PERCENT), key, &mut warnings)
                                as u8;
                        seen.push("tool_warn_percent");
                    }
                    Err(_) => invalid(&mut warnings),
                },
                "prevent_tool_break" => match parse_bool(value) {
                    Some(b) => {
                        settings.qol.prevent_tool_break = b;
                        seen.push("prevent_tool_break");
                    }
                    None => invalid(&mut warnings),
                },
                "chat_history_lines" => match value.parse::<i64>() {
                    Ok(v) => {
                        settings.qol.chat_history_lines = clamp_i64(
                            v,
                            MIN_CHAT_HISTORY_LINES as i64,
                            MAX_CHAT_HISTORY_LINES as i64,
                            key,
                            &mut warnings,
                        ) as usize;
                        seen.push("chat_history_lines");
                    }
                    Err(_) => invalid(&mut warnings),
                },
                "retain_chat_on_reconnect" => match parse_bool(value) {
                    Some(b) => {
                        settings.qol.retain_chat_on_reconnect = b;
                        seen.push("retain_chat_on_reconnect");
                    }
                    None => invalid(&mut warnings),
                },
                "auto_reconnect" => match parse_bool(value) {
                    Some(b) => {
                        settings.seamless_qol.auto_reconnect = b;
                        seen.push("auto_reconnect");
                    }
                    None => invalid(&mut warnings),
                },
                "auto_replenish" => match parse_bool(value) {
                    Some(b) => {
                        settings.seamless_qol.auto_replenish = b;
                        seen.push("auto_replenish");
                    }
                    None => invalid(&mut warnings),
                },
                "quick_stack_nearby" => match parse_bool(value) {
                    Some(b) => {
                        settings.seamless_qol.quick_stack_nearby = b;
                        seen.push("quick_stack_nearby");
                    }
                    None => invalid(&mut warnings),
                },
                "food_saturation_preview" => match parse_bool(value) {
                    Some(b) => {
                        settings.seamless_qol.food_saturation_preview = b;
                        seen.push("food_saturation_preview");
                    }
                    None => invalid(&mut warnings),
                },
                "shulker_tooltip_peek" => match parse_bool(value) {
                    Some(b) => {
                        settings.seamless_qol.shulker_tooltip_peek = b;
                        seen.push("shulker_tooltip_peek");
                    }
                    None => invalid(&mut warnings),
                },
                "chat_scroll_lock" => match parse_bool(value) {
                    Some(b) => {
                        settings.seamless_qol.chat_scroll_lock = b;
                        seen.push("chat_scroll_lock");
                    }
                    None => invalid(&mut warnings),
                },
                "toast_notifications" => match parse_bool(value) {
                    Some(b) => {
                        settings.seamless_qol.toast_notifications = b;
                        seen.push("toast_notifications");
                    }
                    None => invalid(&mut warnings),
                },
                "recipe_overlay_enabled" => match parse_bool(value) {
                    Some(b) => {
                        settings.seamless_qol.recipe_overlay_enabled = b;
                        seen.push("recipe_overlay_enabled");
                    }
                    None => invalid(&mut warnings),
                },
                other => warnings.push(format!("unknown key '{other}' ignored")),
            }
        }
        (settings, warnings, seen)
    }

    /// Import a Vanilla `options.txt` (`key:value` lines) so a migrating
    /// player keeps FOV, render distance, clouds, particles, VSync, smooth
    /// lighting, entity shadows, and the FPS cap. The result is preset
    /// `Custom`; every value is clamped into the supported range.
    ///
    /// Recognized keys: `renderDistance`, `fov` (Vanilla 0..1 slider or
    /// degrees), `particles` (0 all / 1 decreased / 2 minimal),
    /// `renderClouds` (true / fast / false), `enableVsync`, `maxFps`
    /// (260 = unlimited), `ao` (0 off / 1 min / 2 max), `entityShadows`,
    /// `graphicsMode` (0 fast / 1 fancy / 2 fabulous — Fast implies
    /// `smooth_lighting`/`entity_shadows` off unless `ao`/`entityShadows`
    /// override them).
    pub fn import_vanilla_options_txt(text: &str) -> ClientSettings {
        let mut settings = ClientSettings {
            preset: QualityPreset::Custom,
            ..ClientSettings::default()
        };
        settings.apply_vanilla_options_txt(text);
        settings
    }

    /// Merge the recognized keys of a Vanilla `options.txt` into `self`,
    /// leaving fields the file does not carry untouched. Returns whether at
    /// least one recognized key was applied; when any applied, `preset`
    /// becomes [`QualityPreset::Custom`].
    pub fn apply_vanilla_options_txt(&mut self, text: &str) -> bool {
        let mut pairs = Vec::new();
        for line in text.lines() {
            let line = line.trim();
            if let Some((key, value)) = line.split_once(':') {
                pairs.push((key.trim(), value.trim()));
            }
        }
        let get = |key: &str| {
            pairs
                .iter()
                .rev()
                .find(|(k, _)| k.eq_ignore_ascii_case(key))
                .map(|(_, v)| unquote(v))
        };
        let mut applied = false;
        // graphicsMode applies first: Fast graphics turns AO and entity
        // shadows off; explicit ao/entityShadows keys then override.
        if let Some(mode) = get("graphicsMode").and_then(parse_i64) {
            applied = true;
            if mode == 0 {
                self.smooth_lighting = false;
                self.entity_shadows = false;
            }
        }
        if let Some(v) = get("renderDistance").and_then(parse_i64) {
            applied = true;
            self.view_distance =
                v.clamp(i64::from(MIN_VIEW_DISTANCE), i64::from(MAX_VIEW_DISTANCE)) as u8;
        }
        if let Some(raw) = get("fov") {
            // Vanilla stores fov as a 0..1 slider fraction (0.5 = 70 deg);
            // accept plain degrees too so hand-edited files still work.
            if let Ok(v) = raw.parse::<f64>() {
                applied = true;
                let degrees = if v <= 1.5 { 30.0 + 80.0 * v } else { v };
                self.fov =
                    (degrees.round() as i64).clamp(i64::from(MIN_FOV), i64::from(MAX_FOV)) as u8;
            }
        }
        if let Some(v) = get("particles").and_then(parse_i64) {
            applied = true;
            self.particles = match v {
                0 => ParticleLevel::All,
                1 => ParticleLevel::Decreased,
                _ => ParticleLevel::Minimal,
            };
        }
        if let Some(v) = get("renderClouds") {
            applied = true;
            self.clouds = if v.eq_ignore_ascii_case("fast") {
                CloudMode::Fast
            } else if v.eq_ignore_ascii_case("true") {
                CloudMode::Fancy
            } else {
                CloudMode::Off
            };
        }
        if let Some(v) = get("enableVsync") {
            applied = true;
            self.vsync = v.eq_ignore_ascii_case("true");
        }
        if let Some(v) = get("maxFps").and_then(parse_i64) {
            applied = true;
            // Vanilla's slider tops out at 260, which its UI shows as
            // "Unlimited"; megu's own cap now extends to 2000.
            self.max_fps = if v >= VANILLA_OPTIONS_UNLIMITED_FPS {
                None
            } else {
                Some(v.clamp(i64::from(MIN_MAX_FPS), i64::from(MAX_MAX_FPS)) as u32)
            };
        }
        if let Some(v) = get("ao").and_then(parse_i64) {
            applied = true;
            self.smooth_lighting = v != 0;
        }
        if let Some(v) = get("invertYMouse") {
            applied = true;
            self.mouse_invert_y = v.eq_ignore_ascii_case("true");
        }
        if let Some(v) = get("fullscreen") {
            applied = true;
            self.fullscreen = v.eq_ignore_ascii_case("true");
        }
        if let Some(v) = get("guiScale").and_then(parse_i64) {
            applied = true;
            self.gui_scale = v.clamp(0, 4) as u8;
        }
        if let Some(v) = get("gamma").and_then(|v| v.parse::<f64>().ok()) {
            if v.is_finite() {
                applied = true;
                self.gamma_pct = (100.0 + v.clamp(0.0, 1.0) * 100.0).round() as u16;
            }
        }
        if let Some(v) = get("entityShadows") {
            applied = true;
            self.entity_shadows = v.eq_ignore_ascii_case("true");
        }
        if applied {
            self.preset = QualityPreset::Custom;
        }
        applied
    }
}

/// Outcome of one [`SettingsSaveGuard::flush_if_dirty`] call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SettingsFlush {
    /// Nothing changed since the last write — zero bytes hit the disk.
    Unchanged,
    /// The serialized content differed; the file was written atomically
    /// exactly once.
    Written,
}

/// Debounced, dirty-only persistence gate for the settings file
/// (`megu-client.toml`) and any equally serialized blob such as an
/// exported `options.txt`.
///
/// While the player drags sliders or flips toggles, UI code only updates
/// RAM and calls [`SettingsSaveGuard::mark_dirty`] — that path performs
/// zero disk I/O. On settings-screen close or process exit the caller runs
/// [`SettingsSaveGuard::flush_if_dirty`], which serializes once, compares
/// an FNV-1a digest plus byte length against the last content confirmed on
/// disk, and writes atomically (temp file + rename) only when the bytes
/// actually differ. A slider A→B→A roundtrip therefore costs `0 B` of
/// writes, and any number of flushes over identical content costs at most
/// one write total.
#[derive(Clone, Debug, Default)]
pub struct SettingsSaveGuard {
    /// `(fnv1a64, len)` of the last content confirmed on disk — written by
    /// this guard, or read back once on the first dirty flush of a session.
    last_disk: Option<(u64, usize)>,
    /// RAM-only "content may have changed" flag; cleared by every flush.
    dirty: bool,
    /// Lifetime count of atomic file writes performed (telemetry).
    disk_writes: u64,
    /// Lifetime count of dirty flushes skipped because the serialized
    /// bytes were identical to disk (telemetry).
    skipped_writes: u64,
}

impl SettingsSaveGuard {
    /// A clean guard that has never observed the target file.
    pub fn new() -> Self {
        Self::default()
    }

    /// Mark that the in-RAM settings may have changed. Pure bookkeeping —
    /// deliberately performs no serialization and no disk I/O, so slider
    /// drag bursts never touch the disk.
    pub fn mark_dirty(&mut self) {
        self.dirty = true;
    }

    /// Whether [`mark_dirty`](Self::mark_dirty) was called since the last
    /// flush.
    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    /// Lifetime count of atomic file writes performed (telemetry).
    pub fn disk_writes(&self) -> u64 {
        self.disk_writes
    }

    /// Lifetime count of dirty flushes that performed zero writes because
    /// the serialized bytes matched disk (telemetry).
    pub fn skipped_writes(&self) -> u64 {
        self.skipped_writes
    }

    /// Serialize `settings` once and write `path` only when the bytes
    /// differ from the last content confirmed on disk. Flushes while
    /// [`is_dirty`](Self::is_dirty) is `false` return immediately —
    /// `0 B` written, no serialization. The first dirty flush of a
    /// session reads the existing file once to seed the comparison, so
    /// closing an untouched settings screen never rewrites it.
    pub fn flush_if_dirty(
        &mut self,
        settings: &ClientSettings,
        path: &Path,
    ) -> Result<SettingsFlush, String> {
        if !self.dirty {
            return Ok(SettingsFlush::Unchanged);
        }
        self.flush_bytes_if_dirty(settings.to_toml().as_bytes(), path)
    }

    /// The serialized-bytes core of [`flush_if_dirty`](Self::flush_if_dirty)
    /// for non-TOML payloads (e.g. a generated `options.txt`).
    pub fn flush_bytes_if_dirty(
        &mut self,
        bytes: &[u8],
        path: &Path,
    ) -> Result<SettingsFlush, String> {
        if !self.dirty {
            return Ok(SettingsFlush::Unchanged);
        }
        self.dirty = false;
        let digest = (fnv1a64(bytes), bytes.len());
        let baseline = match self.last_disk {
            Some(known) => Some(known),
            None => std::fs::read(path)
                .ok()
                .map(|on_disk| (fnv1a64(&on_disk), on_disk.len())),
        };
        if baseline == Some(digest) {
            self.last_disk = baseline;
            self.skipped_writes += 1;
            return Ok(SettingsFlush::Unchanged);
        }
        write_atomic(path, bytes)?;
        self.last_disk = Some(digest);
        self.disk_writes += 1;
        Ok(SettingsFlush::Written)
    }
}

/// Write `bytes` to `path` atomically: a sibling `*.tmp` file is written
/// first, then renamed over `path`, so a crash mid-write can never leave a
/// truncated settings file.
fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
        }
    }
    let file_name = path
        .file_name()
        .ok_or_else(|| format!("settings path {} has no file name", path.display()))?;
    let tmp = path.with_file_name(format!("{}.tmp", file_name.to_string_lossy()));
    std::fs::write(&tmp, bytes).map_err(|e| format!("cannot write {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("cannot rename {} to {}: {e}", tmp.display(), path.display())
    })
}

/// Whether `value` is an acceptable `shader_pack`/`shader_profile`
/// selector: non-empty after trimming and free of `..` path-traversal
/// components and of characters that would break the settings file format.
pub fn is_valid_shader_selector(value: &str) -> bool {
    let trimmed = value.trim();
    !trimmed.is_empty()
        && !trimmed.chars().any(|c| c == '"' || c.is_control())
        && !trimmed.split(&['/', '\\'][..]).any(|part| part == "..")
}

/// Normalize a `shader_pack`/`shader_profile` selector for the settings
/// file and CLI: trims surrounding whitespace, returning `None` for empty
/// strings or `..` traversal so the caller degrades to built-in rendering
/// (TOML warns and keeps the default; the CLI fails closed).
pub fn sanitize_shader_selector(value: &str) -> Option<String> {
    let trimmed = value.trim();
    is_valid_shader_selector(trimmed).then(|| trimmed.to_owned())
}

fn unquote(value: &str) -> &str {
    let bytes = value.as_bytes();
    if bytes.len() >= 2
        && ((bytes[0] == b'"' && bytes[bytes.len() - 1] == b'"')
            || (bytes[0] == b'\'' && bytes[bytes.len() - 1] == b'\''))
    {
        &value[1..value.len() - 1]
    } else {
        value
    }
}

fn parse_bool(value: &str) -> Option<bool> {
    if value.eq_ignore_ascii_case("true") {
        Some(true)
    } else if value.eq_ignore_ascii_case("false") {
        Some(false)
    } else {
        None
    }
}

fn parse_i64(value: &str) -> Option<i64> {
    value.parse::<i64>().ok()
}

fn clamp_u8(value: i64, min: i64, max: i64, key: &str, warnings: &mut Vec<String>) -> u8 {
    clamp_i64(value, min, max, key, warnings) as u8
}

fn clamp_u32(value: i64, min: i64, max: i64, key: &str, warnings: &mut Vec<String>) -> u32 {
    clamp_i64(value, min, max, key, warnings) as u32
}

fn clamp_i64(value: i64, min: i64, max: i64, key: &str, warnings: &mut Vec<String>) -> i64 {
    let clamped = value.clamp(min, max);
    if clamped != value {
        warnings.push(format!("'{key}' value {value} clamped to {clamped}"));
    }
    clamped
}

fn clamp_f32(value: f32, min: f32, max: f32, key: &str, warnings: &mut Vec<String>) -> f32 {
    let clamped = value.clamp(min, max);
    if clamped != value {
        warnings.push(format!("'{key}' value {value} clamped to {clamped}"));
    }
    clamped
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_presets_fill_every_field_with_valid_lod() {
        for preset in [
            QualityPreset::Fast,
            QualityPreset::Balanced,
            QualityPreset::High,
            QualityPreset::Ultra,
            QualityPreset::Custom,
        ] {
            let s = ClientSettings::from_preset(preset);
            assert_eq!(s.preset, preset);
            assert!((MIN_VIEW_DISTANCE..=MAX_VIEW_DISTANCE).contains(&s.view_distance));
            assert!((MIN_FOV..=MAX_FOV).contains(&s.fov));
            if let Some(fps) = s.max_fps {
                assert!((MIN_MAX_FPS..=MAX_MAX_FPS).contains(&fps));
            }
            if let Some(lod) = s.to_lod_config() {
                assert!(
                    0 <= lod.full_detail_radius
                        && lod.full_detail_radius <= lod.fine_lod_radius
                        && lod.fine_lod_radius <= lod.coarse_lod_radius
                );
            }
            assert_eq!(s.engine_optimizations(), BuiltInOptimizations::ALL_ON);
        }
        // Spot-check the intended flavor differences.
        let fast = ClientSettings::from_preset(QualityPreset::Fast);
        assert_eq!(fast.view_distance, 8);
        assert_eq!(fast.clouds, CloudMode::Fast);
        assert!(!fast.smooth_lighting && !fast.entity_shadows);
        let high = ClientSettings::from_preset(QualityPreset::High);
        assert_eq!(high.view_distance, 16);
        assert_eq!(high.distant_horizon, DistantHorizon::Medium);
        let ultra = ClientSettings::from_preset(QualityPreset::Ultra);
        assert_eq!(ultra.view_distance, MAX_VIEW_DISTANCE);
        assert_eq!(ultra.distant_horizon, DistantHorizon::Far);
        assert_eq!(QualityPreset::parse("ultra"), Some(QualityPreset::Ultra));
    }

    #[test]
    fn lod_config_maps_distant_horizon_onto_radii() {
        let mut s = ClientSettings::default();
        assert_eq!(s.to_lod_config(), None);

        s.distant_horizon = DistantHorizon::Medium;
        let lod = s.to_lod_config().unwrap();
        assert_eq!(lod.full_detail_radius, 12);
        assert_eq!(lod.coarse_lod_radius, 64);
        assert_eq!(lod.fine_lod_radius, (12 + 64) / 2);

        s.view_distance = 32;
        s.distant_horizon = DistantHorizon::Far;
        let lod = s.to_lod_config().unwrap();
        assert_eq!(lod.full_detail_radius, 32);
        assert_eq!(lod.coarse_lod_radius, 128);
        // Ordering invariant survives the widest view distance.
        assert!(lod.full_detail_radius <= lod.fine_lod_radius);
        assert!(lod.fine_lod_radius <= lod.coarse_lod_radius);
    }

    #[test]
    fn toml_round_trip_is_lossless() {
        let mut s = ClientSettings {
            preset: QualityPreset::Custom,
            view_distance: 20,
            distant_horizon: DistantHorizon::Far,
            clouds: CloudMode::Off,
            particles: ParticleLevel::Minimal,
            smooth_lighting: false,
            entity_shadows: false,
            comfort_exposure: false,
            dynamic_lights: true,
            enchantment_glint: false,
            indirect_light_bounce: true,
            directional_shadow_gameplay: true,
            lazy_catchup_gameplay: true,
            fov: 90,
            mouse_sensitivity_pct: 150,
            volume_master: 80,
            volume_music: 55,
            volume_blocks: 95,
            volume_hostile: 60,
            volume_friendly: 40,
            vsync: false,
            max_fps: Some(144),
            background_fps_limit: 45,
            ram_budget_mib: 6144,
            gpu_backend: RenderBackendPreference::ClassicLowSpec,
            shader_pack: Some("shaderpacks/Complementary.zip".to_owned()),
            shaders_enabled: true,
            builtin_looking_at_hud: false,
            inventory_drag_controls: false,
            builtin_inventory_sort: false,
            zsh_command_bar: false,
            compact_debug_hud_enabled: true,
            compact_debug_hud_segments: 0b101_001_011,
            shader_profile: Some("HIGH".to_owned()),
            map_fair_play: HostMapFairPlaySettings {
                minimap_fair_play: true,
                allow_cave_map: false,
                allow_entity_radar: false,
                allow_waypoint_teleport: false,
                minimap_mode: HostMinimapMode::FairPlay,
                allow_mob_radar: false,
                player_radar_scope: HostPlayerRadarScope::None,
            },
            qol: ClientQolSettings {
                max_volume_cap: 0.5,
                max_concurrent_same_sound: 8,
                zoom_divisor: 6.0,
                tool_warn_percent: 25,
                prevent_tool_break: true,
                chat_history_lines: 250,
                retain_chat_on_reconnect: false,
            },
            seamless_qol: ClientSeamlessQolSettings {
                auto_reconnect: true,
                auto_replenish: true,
                quick_stack_nearby: true,
                food_saturation_preview: true,
                shulker_tooltip_peek: false,
                chat_scroll_lock: false,
                toast_notifications: false,
                recipe_overlay_enabled: true,
            },
            crash_report_mode: CrashReportMode::AutoCrashOnly,
            report_endpoint: Some("https://reports.example.com/api/report".to_owned()),
            fullscreen: true,
            mouse_invert_y: true,
            gamma_pct: 175,
            gui_scale: 3,
            keys: KeyBindings {
                forward: "KeyI".to_owned(),
                ..KeyBindings::default()
            },
        };
        let text = s.to_toml();
        let (back, warnings) = ClientSettings::from_toml(&text);
        assert_eq!(back, s);
        assert!(warnings.is_empty(), "unexpected warnings: {warnings:?}");

        // Unlimited max_fps round-trips through the "unlimited" string.
        s.max_fps = None;
        let (back, warnings) = ClientSettings::from_toml(&s.to_toml());
        assert_eq!(back.max_fps, None);
        assert!(warnings.is_empty());
    }

    #[test]
    fn neutral_qol_keys_write_and_legacy_keys_migrate() {
        let settings = ClientSettings {
            inventory_drag_controls: false,
            compact_debug_hud_enabled: true,
            compact_debug_hud_segments: 0b101_001_011,
            ..ClientSettings::default()
        };
        let text = settings.to_toml();
        assert!(text.contains("inventory_drag_controls = false"));
        assert!(text.contains("compact_debug_hud_enabled = true"));
        assert!(text.contains("compact_debug_hud_segments = 331"));
        assert!(!text.contains("builtin_mouse_tweaks"));
        assert!(!text.contains("mini_hud_enabled"));
        assert!(!text.contains("mini_hud_segments"));
        let (back, warnings) = ClientSettings::from_toml(&text);
        assert_eq!(back, settings);
        assert!(warnings.is_empty(), "{warnings:?}");

        let legacy = "builtin_mouse_tweaks = false\nmini_hud_enabled = true\nmini_hud_segments = 331\n";
        let (migrated, warnings) = ClientSettings::from_toml(legacy);
        assert!(warnings.is_empty(), "{warnings:?}");
        assert!(!migrated.inventory_drag_controls);
        assert!(migrated.compact_debug_hud_enabled);
        assert_eq!(migrated.compact_debug_hud_segments, 0b101_001_011);

        let mut overlaid = ClientSettings::default();
        let warnings = overlaid.apply_toml(legacy);
        assert!(warnings.is_empty(), "{warnings:?}");
        assert!(!overlaid.inventory_drag_controls);
        assert!(overlaid.compact_debug_hud_enabled);
        assert_eq!(overlaid.compact_debug_hud_segments, 0b101_001_011);
    }

    #[test]
    fn seamless_qol_defaults_opt_in_off_and_ux_on() {
        let q = ClientSeamlessQolSettings::default();
        // Opinionated gameplay helpers are strictly opt-in.
        assert!(!q.auto_reconnect);
        assert!(!q.auto_replenish);
        assert!(!q.quick_stack_nearby);
        assert!(!q.food_saturation_preview);
        // Non-intrusive pure-UX improvements default on.
        assert!(q.shulker_tooltip_peek);
        assert!(q.chat_scroll_lock);
        assert!(q.toast_notifications);
        // The recipe side-overlay is a strict opt-in.
        assert!(!q.recipe_overlay_enabled);
    }

    #[test]
    fn seamless_qol_toml_roundtrip_and_omitted_defaults() {
        let settings = ClientSettings {
            seamless_qol: ClientSeamlessQolSettings {
                auto_reconnect: true,
                auto_replenish: true,
                quick_stack_nearby: true,
                food_saturation_preview: true,
                shulker_tooltip_peek: false,
                chat_scroll_lock: false,
                toast_notifications: false,
                recipe_overlay_enabled: true,
            },
            ..ClientSettings::default()
        };
        let text = settings.to_toml();
        assert!(text.contains("[qol]"));
        assert!(text.contains("auto_reconnect = true"));
        assert!(text.contains("food_saturation_preview = true"));
        assert!(text.contains("chat_scroll_lock = false"));

        let (back, warnings) = ClientSettings::from_toml(&text);
        assert_eq!(back.seamless_qol, settings.seamless_qol);
        assert_eq!(back, settings);
        assert!(warnings.is_empty(), "unexpected warnings: {warnings:?}");

        // apply_toml overlays only the keys a file defines.
        let mut s = ClientSettings::default();
        let warnings = s.apply_toml("[qol]\nauto_reconnect = true\ntoast_notifications = false\n");
        assert!(warnings.is_empty());
        assert!(s.seamless_qol.auto_reconnect);
        assert!(!s.seamless_qol.toast_notifications);
        assert!(!s.seamless_qol.auto_replenish, "undefined keys keep defaults");
        assert!(s.seamless_qol.shulker_tooltip_peek);

        // Files written before the seamless keys existed parse clean.
        let (legacy, warnings) = ClientSettings::from_toml("[qol]\nmax_volume_cap = 0.5\n");
        assert_eq!(legacy.seamless_qol, ClientSeamlessQolSettings::default());
        assert!(warnings.is_empty());

        // Non-boolean junk warns and falls back to the default.
        let (parsed, warnings) = ClientSettings::from_toml("[qol]\nauto_reconnect = \"yes\"\n");
        assert!(!parsed.seamless_qol.auto_reconnect);
        assert!(!warnings.is_empty());
    }

    #[test]
    fn ram_budget_mib_defaults_auto_and_overlays_cleanly() {
        // Files written before the key existed stay valid and read Auto.
        let (s, warnings) = ClientSettings::from_toml("view_distance = 20\n");
        assert_eq!(s.ram_budget_mib, 0);
        assert!(warnings.is_empty());

        // Explicit budgets parse, clamp, and overlay via apply_toml.
        let (s, warnings) = ClientSettings::from_toml("ram_budget_mib = 4096\n");
        assert_eq!(s.ram_budget_mib, 4096);
        assert!(warnings.is_empty(), "{warnings:?}");
        let (s, warnings) = ClientSettings::from_toml("ram_budget_mib = -4\n");
        assert_eq!(s.ram_budget_mib, 0);
        assert!(!warnings.is_empty());
        let mut s = ClientSettings::default();
        s.apply_toml("ram_budget_mib = 2048\n");
        assert_eq!(s.ram_budget_mib, 2048);
    }

    #[test]
    fn gpu_backend_toml_roundtrip_defaults_and_fails_gracefully() {
        // Every token round-trips losslessly through to_toml/from_toml.
        for backend in [
            RenderBackendPreference::Auto,
            RenderBackendPreference::MeshShader,
            RenderBackendPreference::ComputeIndirect,
            RenderBackendPreference::ClassicLowSpec,
            RenderBackendPreference::Classic,
        ] {
            let s = ClientSettings {
                gpu_backend: backend,
                ..ClientSettings::default()
            };
            let (back, warnings) = ClientSettings::from_toml(&s.to_toml());
            assert_eq!(back, s, "{backend:?}");
            assert_eq!(back.gpu_backend, backend);
            assert!(warnings.is_empty(), "{backend:?}: {warnings:?}");
        }

        // Files written before the key existed stay valid and default to
        // Auto (backward compatible).
        let (s, warnings) = ClientSettings::from_toml("view_distance = 20\n");
        assert_eq!(s.gpu_backend, RenderBackendPreference::Auto);
        assert!(warnings.is_empty());

        // An unrecognized value warns and keeps the default, like every
        // other named-option key.
        let (s, warnings) = ClientSettings::from_toml("gpu_backend = \"vulkan\"\n");
        assert_eq!(s.gpu_backend, RenderBackendPreference::Auto);
        assert!(warnings.iter().any(|w| w.contains("gpu_backend")));

        // apply_toml overlays only the keys a file defines.
        let mut s = ClientSettings::from_preset(QualityPreset::Fast);
        s.apply_toml("gpu_backend = \"classic-low-spec\"\n");
        assert_eq!(s.gpu_backend, RenderBackendPreference::ClassicLowSpec);
        assert_eq!(s.preset, QualityPreset::Fast);
    }

    #[test]
    fn lighting_comfort_settings_default_parse_and_round_trip() {
        // Eye-friendly defaults: everything on except the opt-in
        // shadow-light gameplay toggle.
        let s = ClientSettings::default();
        assert!(s.comfort_exposure);
        assert!(s.dynamic_lights);
        assert!(s.enchantment_glint);
        assert!(s.indirect_light_bounce);
        assert!(!s.directional_shadow_gameplay);
        assert!(!s.lazy_catchup_gameplay);

        // Files written before these keys existed keep the defaults.
        let (s, warnings) = ClientSettings::from_toml("view_distance = 20\n");
        assert!(warnings.is_empty());
        assert!(s.comfort_exposure);
        assert!(s.dynamic_lights);
        assert!(s.enchantment_glint);
        assert!(s.indirect_light_bounce);
        assert!(!s.directional_shadow_gameplay);
        assert!(!s.lazy_catchup_gameplay);

        // Explicit values parse in both directions and round-trip.
        let (s, warnings) = ClientSettings::from_toml(
            "comfort_exposure = false\n\
             dynamic_lights = false\n\
             enchantment_glint = false\n\
             indirect_light_bounce = false\n\
             directional_shadow_gameplay = true\n\
             lazy_catchup_gameplay = true\n",
        );
        assert!(warnings.is_empty(), "unexpected warnings: {warnings:?}");
        assert!(!s.comfort_exposure);
        assert!(!s.dynamic_lights);
        assert!(!s.enchantment_glint);
        assert!(!s.indirect_light_bounce);
        assert!(s.directional_shadow_gameplay);
        assert!(s.lazy_catchup_gameplay);
        let (back, warnings) = ClientSettings::from_toml(&s.to_toml());
        assert_eq!(back, s);
        assert!(warnings.is_empty());

        // apply_toml overlays only the keys a file defines.
        let mut s = ClientSettings::default();
        s.apply_toml(
            "directional_shadow_gameplay = true\nlazy_catchup_gameplay = true\ncomfort_exposure = false\n",
        );
        assert!(s.directional_shadow_gameplay);
        assert!(s.lazy_catchup_gameplay);
        assert!(!s.comfort_exposure);
        assert!(s.dynamic_lights);
        assert!(s.enchantment_glint);
        assert!(s.indirect_light_bounce);
    }

    #[test]
    fn from_toml_clamps_and_warns_gracefully() {
        let text = "\
            preset = \"ludicrous\"\n\
            view_distance = 99\n\
            fov = 0\n\
            max_fps = 5\n\
            bogus_key = 1\n\
            vsync = maybe\n\
            smooth_lighting = false\n";
        let (s, warnings) = ClientSettings::from_toml(text);
        assert_eq!(s.view_distance, MAX_VIEW_DISTANCE);
        assert_eq!(s.fov, MIN_FOV);
        assert_eq!(s.max_fps, Some(MIN_MAX_FPS));
        assert!(warnings.iter().any(|w| w.contains("max_fps")));
        assert!(!s.smooth_lighting);
        // preset/vsync stay default on invalid input.
        assert_eq!(s.preset, QualityPreset::Balanced);
        assert!(s.vsync);
        assert!(warnings.iter().any(|w| w.contains("bogus_key")));
        assert!(warnings.iter().any(|w| w.contains("view_distance")));
        assert!(warnings.iter().any(|w| w.contains("'fov'")));
        assert!(warnings.iter().any(|w| w.contains("preset")));
        assert!(warnings.iter().any(|w| w.contains("vsync")));
    }

    #[test]
    fn vanilla_options_txt_import_maps_all_keys() {
        let options = "\
            version:3465\n\
            ao:2\n\
            fov:0.5\n\
            enableVsync:false\n\
            maxFps:144\n\
            renderClouds:\"fast\"\n\
            graphicsMode:1\n\
            particles:1\n\
            entityShadows:false\n\
            renderDistance:16\n";
        let s = ClientSettings::import_vanilla_options_txt(options);
        assert_eq!(s.preset, QualityPreset::Custom);
        assert_eq!(s.view_distance, 16);
        assert_eq!(s.fov, 70);
        assert!(!s.vsync);
        assert_eq!(s.max_fps, Some(144));
        assert_eq!(s.clouds, CloudMode::Fast);
        assert_eq!(s.particles, ParticleLevel::Decreased);
        assert!(s.smooth_lighting); // ao:2 => max smooth lighting
        assert!(!s.entity_shadows);
    }

    #[test]
    fn vanilla_import_clamps_and_handles_unlimited() {
        let s = ClientSettings::import_vanilla_options_txt(
            "renderDistance:64\nfov:1.0\nmaxFps:260\nparticles:2\n\
             renderClouds:false\ngraphicsMode:0\n",
        );
        assert_eq!(s.view_distance, MAX_VIEW_DISTANCE);
        assert_eq!(s.fov, MAX_FOV);
        assert_eq!(s.max_fps, None);
        assert_eq!(s.particles, ParticleLevel::Minimal);
        assert_eq!(s.clouds, CloudMode::Off);
        // Fast graphics disables AO and shadows.
        assert!(!s.smooth_lighting && !s.entity_shadows);
    }

    #[test]
    fn vanilla_import_missing_keys_keep_defaults() {
        let s = ClientSettings::import_vanilla_options_txt("fov:90\n");
        // Values above 1.5 are treated as degrees directly.
        assert_eq!(s.fov, 90);
        assert_eq!(s.view_distance, DEFAULT_VIEW_DISTANCE);
        assert!(s.vsync);
        assert_eq!(s.clouds, CloudMode::Fancy);
    }

    #[test]
    fn apply_toml_overlays_only_defined_keys() {
        let mut s = ClientSettings::from_preset(QualityPreset::Fast);
        let warnings = s.apply_toml("view_distance = 20\nfov = 95\nbogus = 1\n");
        assert_eq!(s.view_distance, 20);
        assert_eq!(s.fov, 95);
        // Untouched keys keep the preset's values.
        assert_eq!(s.clouds, CloudMode::Fast);
        assert_eq!(s.particles, ParticleLevel::Decreased);
        assert!(!s.smooth_lighting);
        // The file defined no preset key, so the base preset stays.
        assert_eq!(s.preset, QualityPreset::Fast);
        assert!(warnings.iter().any(|w| w.contains("bogus")));
    }

    #[test]
    fn apply_vanilla_options_txt_overlays_and_marks_custom() {
        let mut s = ClientSettings::from_preset(QualityPreset::High);
        assert!(s.apply_vanilla_options_txt("fov:0.5\nrenderDistance:7\n"));
        assert_eq!(s.fov, 70);
        assert_eq!(s.view_distance, 7);
        // High preset values survive for keys the file does not carry.
        assert_eq!(s.distant_horizon, DistantHorizon::Medium);
        assert_eq!(s.preset, QualityPreset::Custom);

        let mut s = ClientSettings::from_preset(QualityPreset::Fast);
        // A file with no recognized keys changes nothing and keeps the preset.
        assert!(!s.apply_vanilla_options_txt("version:3465\ntitleUpdateIndex:0\n"));
        assert_eq!(s.preset, QualityPreset::Fast);
    }

    #[test]
    fn write_toml_round_trips_through_from_toml() {
        let dir = std::env::temp_dir().join(format!("megu-settings-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("nested/out.toml");
        let s = ClientSettings::from_preset(QualityPreset::Ultra);
        s.write_toml(&path).expect("write_toml creates parents");
        let text = std::fs::read_to_string(&path).expect("read back");
        let (back, warnings) = ClientSettings::from_toml(&text);
        assert_eq!(back, s);
        assert!(warnings.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
