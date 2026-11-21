// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! Player-facing graphics settings for the native mc-spine client.
//!
//! Design rule (low cognitive load): optimizations with zero player-visible
//! downside are ALWAYS ON under the hood and deliberately have no toggle —
//! see [`BuiltInOptimizations`]. This module only exposes settings that
//! genuinely change what the player sees or that trade something off:
//! preset, view distance, distant horizon LOD, clouds, particles, smooth
//! lighting, entity shadows, FOV, VSync, an FPS cap, and the GPU backend
//! preference.
//!
//! Persistence is human-readable TOML (`mc-spine-client.toml`) plus a
//! [`ClientSettings::import_vanilla_options_txt`] helper so players migrating
//! from the official launcher keep their FOV, render distance, clouds,
//! particles, and VSync automatically. Pure std Rust; zero dependencies.

use std::path::Path;

use mc_spine_client_mesh::RenderBackendPreference;

use crate::audio_qol::{
    DEFAULT_CHAT_HISTORY_LINES, DEFAULT_MAX_CONCURRENT_SAME_SOUND, DEFAULT_MAX_VOLUME_CAP,
    DEFAULT_TOOL_WARN_PERCENT, DEFAULT_ZOOM_DIVISOR, MAX_CHAT_HISTORY_LINES,
    MAX_MAX_CONCURRENT_SAME_SOUND, MAX_TOOL_WARN_PERCENT, MAX_VOLUME_CAP, MAX_ZOOM_DIVISOR,
    MIN_CHAT_HISTORY_LINES, MIN_MAX_CONCURRENT_SAME_SOUND, MIN_ZOOM_DIVISOR,
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
pub const SETTINGS_FILE_NAME: &str = "mc-spine-client.toml";

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
/// `mc-spine`; no third-party optimization-mod code, shaders, assets, or
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
    /// Hard ceiling for the mixed audio buffer, `0.0..=1.0`. `1.0` (the
    /// default) leaves the mix untouched; lower values cap spikes such as
    /// thunder or Wither hurt sounds.
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
    /// `30..=110` degrees.
    pub fov: u8,
    pub vsync: bool,
    /// `None` = unlimited; `Some(10..=2000)` otherwise.
    pub max_fps: Option<u32>,
    /// Frame-rate cap applied while the window is unfocused, in the spirit
    /// of Vanilla's "Dynamic FPS" graphics toggle: `0` disables background
    /// throttling entirely, `1..=240` caps background frames. Defaults to
    /// [`DEFAULT_BACKGROUND_FPS_LIMIT`].
    pub background_fps_limit: u16,
    /// GPU pipeline selection: `auto` negotiates from hardware
    /// capabilities, `classic-low-spec` pins the minimal-hardware path for
    /// legacy Intel integrated GPUs (GL 3.3 / Vulkan 1.1 only).
    pub gpu_backend: RenderBackendPreference,
    /// Map-mod fair-play policy applied when this client hosts a world
    /// (integrated server, LAN share, friend relay); ignored on plain
    /// client joins to a remote server.
    pub map_fair_play: HostMapFairPlaySettings,
    /// Quality-of-life toggles (audio limiter, zoom, tool warnings, chat
    /// history) persisted under the `[qol]` TOML table.
    pub qol: ClientQolSettings,
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
            fov: DEFAULT_FOV,
            vsync: true,
            max_fps: None,
            background_fps_limit: DEFAULT_BACKGROUND_FPS_LIMIT,
            gpu_backend: RenderBackendPreference::Auto,
            map_fair_play: HostMapFairPlaySettings::default(),
            qol: ClientQolSettings::default(),
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

    /// Serialize to the human-readable `mc-spine-client.toml` format.
    pub fn to_toml(&self) -> String {
        let max_fps = self
            .max_fps
            .map_or_else(|| "\"unlimited\"".to_string(), |v| v.to_string());
        format!(
            "# mc-spine client settings\n\
             preset = \"{}\"\n\
             view_distance = {}\n\
             distant_horizon = \"{}\"\n\
             clouds = \"{}\"\n\
             particles = \"{}\"\n\
             smooth_lighting = {}\n\
             entity_shadows = {}\n\
             fov = {}\n\
             vsync = {}\n\
             max_fps = {}\n\
             background_fps_limit = {}\n\
             gpu_backend = \"{}\"\n\
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
             retain_chat_on_reconnect = {}\n",
            self.preset.as_str(),
            self.view_distance,
            self.distant_horizon.as_str(),
            self.clouds.as_str(),
            self.particles.as_str(),
            self.smooth_lighting,
            self.entity_shadows,
            self.fov,
            self.vsync,
            max_fps,
            self.background_fps_limit,
            self.gpu_backend.as_str(),
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
        )
    }

    /// Parse a `mc-spine-client.toml` document. Returns the resolved
    /// settings plus one warning string per unknown key, invalid value, or
    /// clamped out-of-range value — malformed files degrade gracefully to
    /// defaults instead of failing.
    pub fn from_toml(text: &str) -> (ClientSettings, Vec<String>) {
        let (settings, warnings, _) = Self::parse_toml(text);
        (settings, warnings)
    }

    /// Apply only the keys a `mc-spine-client.toml` document actually
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
                "fov" => self.fov = parsed.fov,
                "vsync" => self.vsync = parsed.vsync,
                "max_fps" => self.max_fps = parsed.max_fps,
                "background_fps_limit" => {
                    self.background_fps_limit = parsed.background_fps_limit;
                }
                "gpu_backend" => self.gpu_backend = parsed.gpu_backend,
                "minimap_fair_play" | "minimap_mode" => {
                    // The mirrored pair is already consistent in `parsed`.
                    self.map_fair_play.minimap_fair_play =
                        parsed.map_fair_play.minimap_fair_play;
                    self.map_fair_play.minimap_mode = parsed.map_fair_play.minimap_mode;
                }
                "allow_cave_map" => {
                    self.map_fair_play.allow_cave_map = parsed.map_fair_play.allow_cave_map;
                }
                "allow_entity_radar" | "allow_mob_radar" => {
                    self.map_fair_play.allow_entity_radar =
                        parsed.map_fair_play.allow_entity_radar;
                    self.map_fair_play.allow_mob_radar = parsed.map_fair_play.allow_mob_radar;
                }
                "player_radar_scope" => {
                    self.map_fair_play.player_radar_scope =
                        parsed.map_fair_play.player_radar_scope;
                }
                "allow_waypoint_teleport" => {
                    self.map_fair_play.allow_waypoint_teleport =
                        parsed.map_fair_play.allow_waypoint_teleport;
                }
                "max_volume_cap" => self.qol.max_volume_cap = parsed.qol.max_volume_cap,
                "max_concurrent_same_sound" => {
                    self.qol.max_concurrent_same_sound =
                        parsed.qol.max_concurrent_same_sound;
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
        for raw_line in text.lines() {
            let line = raw_line.split('#').next().unwrap_or("").trim();
            if line.is_empty() || (line.starts_with('[') && line.ends_with(']')) {
                continue;
            }
            let Some((key, raw_value)) = line.split_once('=') else {
                warnings.push(format!("ignored malformed line '{line}'"));
                continue;
            };
            let key = key.trim();
            let value = unquote(raw_value.trim());
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
                "vsync" => match parse_bool(value) {
                    Some(b) => {
                        settings.vsync = b;
                        seen.push("vsync");
                    }
                    None => invalid(&mut warnings),
                },
                "max_fps" => {
                    if value.eq_ignore_ascii_case("unlimited")
                        || value.eq_ignore_ascii_case("none")
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
                "gpu_backend" => match RenderBackendPreference::parse(value) {
                    Some(g) => {
                        settings.gpu_backend = g;
                        seen.push("gpu_backend");
                    }
                    None => invalid(&mut warnings),
                },
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
                        settings.qol.zoom_divisor = clamp_f32(
                            v,
                            MIN_ZOOM_DIVISOR,
                            MAX_ZOOM_DIVISOR,
                            key,
                            &mut warnings,
                        );
                        seen.push("zoom_divisor");
                    }
                    _ => invalid(&mut warnings),
                },
                "tool_warn_percent" => match value.parse::<i64>() {
                    Ok(v) => {
                        settings.qol.tool_warn_percent = clamp_i64(
                            v,
                            0,
                            i64::from(MAX_TOOL_WARN_PERCENT),
                            key,
                            &mut warnings,
                        ) as u8;
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
            self.view_distance = v.clamp(
                i64::from(MIN_VIEW_DISTANCE),
                i64::from(MAX_VIEW_DISTANCE),
            ) as u8;
        }
        if let Some(raw) = get("fov") {
            // Vanilla stores fov as a 0..1 slider fraction (0.5 = 70 deg);
            // accept plain degrees too so hand-edited files still work.
            if let Ok(v) = raw.parse::<f64>() {
                applied = true;
                let degrees = if v <= 1.5 { 30.0 + 80.0 * v } else { v };
                self.fov = (degrees.round() as i64)
                    .clamp(i64::from(MIN_FOV), i64::from(MAX_FOV))
                    as u8;
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
            // "Unlimited"; mc-spine's own cap now extends to 2000.
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
                assert!(0 <= lod.full_detail_radius
                    && lod.full_detail_radius <= lod.fine_lod_radius
                    && lod.fine_lod_radius <= lod.coarse_lod_radius);
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
            fov: 90,
            vsync: false,
            max_fps: Some(144),
            background_fps_limit: 45,
            gpu_backend: RenderBackendPreference::ClassicLowSpec,
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
        let dir = std::env::temp_dir().join(format!(
            "mc-spine-settings-test-{}",
            std::process::id()
        ));
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
