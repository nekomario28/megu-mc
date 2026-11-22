// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! Client UI screen state machine and input router.
//!
//! [`UiState`] owns *what* is on screen — the current [`Screen`], every
//! player-facing setting, navigation history, modal state, and the small
//! per-screen working state (invite codes, join field, pending mod-sync
//! consent, crash report). [`crate::layout::layout_screen`] decides *where*
//! everything sits; this module decides *what happens* when input arrives.
//!
//! Input is deliberately device-agnostic: [`UiInputEvent`] covers mouse
//! clicks, keys, typed characters, and gamepad buttons, and
//! [`UiState::handle_input`] maps all of them onto the same
//! [`UiAction`]s. Widgets that only change settings mutate
//! [`UiState::settings`] in place and report [`UiAction::SettingsChanged`];
//! anything the embedding app must act on (navigation, clipboard, network
//! join, mod install, crash submit) is a dedicated action — so the UI can
//! never e.g. send a crash report without the app routing
//! [`UiAction::SubmitCrashReport`] itself, and that action is only ever
//! produced after the explicit `"Send OK?"` confirmation modal.
//!
//! All copy is bilingual-ready: [`UiLang`] selects English or Japanese and
//! every label goes through [`tr`]. The built-in bitmap font covers ASCII,
//! so Japanese strings render as fallback glyphs in the software
//! rasterizer — the string data stays correct for richer GPU atlases.

use megu_core::crash_report::CrashContext;
use megu_core::i18n::{BuiltinLocale, I18nCatalog, UiTextKey, ALL_BUILTIN_LOCALES};
use megu_core::sys_metrics::{MiniHudConfig, MiniHudData};

use crate::draw::UiRect;
use crate::layout;

// ---------------------------------------------------------------------------
// Language
// ---------------------------------------------------------------------------

/// Resolved display language.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum UiLang {
    /// English labels.
    #[default]
    En,
    /// Japanese labels (rendered with fallback glyphs until a CJK atlas is
    /// wired in; the strings themselves are complete).
    Ja,
}

/// The player's language *preference* (`Auto` resolves to [`UiLang::En`]
/// deterministically — there is no OS locale dependency in this crate).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum LanguageSetting {
    /// Follow the (deterministic) built-in default: English.
    #[default]
    Auto,
    /// Always English.
    English,
    /// Always Japanese.
    Japanese,
}

impl LanguageSetting {
    /// All selectable values, in cycle order.
    pub const ALL: [Self; 3] = [Self::Auto, Self::English, Self::Japanese];

    pub fn next(self) -> Self {
        Self::ALL[(self.index() + 1) % Self::ALL.len()]
    }

    fn index(self) -> usize {
        match self {
            Self::Auto => 0,
            Self::English => 1,
            Self::Japanese => 2,
        }
    }

    /// The language actually used for rendering.
    pub const fn resolve(self) -> UiLang {
        match self {
            Self::Auto | Self::English => UiLang::En,
            Self::Japanese => UiLang::Ja,
        }
    }

    pub const fn label_en(self) -> &'static str {
        match self {
            Self::Auto => "Auto",
            Self::English => "English",
            Self::Japanese => "Japanese",
        }
    }
}

/// Bilingual label selection: `tr(lang, "English text", "日本語テキスト")`.
pub fn tr<'a>(lang: UiLang, en: &'a str, ja: &'a str) -> &'a str {
    match lang {
        UiLang::En => en,
        UiLang::Ja => ja,
    }
}

// ---------------------------------------------------------------------------
// Screens and tabs
// ---------------------------------------------------------------------------

/// Which settings tab is visible.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SettingsTab {
    /// Preset, view distance, LOD, GPU backend, FPS cap, background FPS,
    /// VSync, smooth lighting.
    #[default]
    Video,
    /// Per-source volumes plus the max-volume limiter and sound
    /// coalescing caps.
    Audio,
    /// Shaderpack controls and the host shader options editor.
    Shaders,
    /// Quality-of-life: zoom, tool-durability warnings, chat history,
    /// crash-report mode.
    Qol,
    /// Opt-in host fair-play switches for worlds this client hosts.
    FairPlay,
    /// UI language.
    Language,
}

impl SettingsTab {
    /// All tabs, left to right.
    pub const ALL: [Self; 6] = [
        Self::Video,
        Self::Audio,
        Self::Shaders,
        Self::Qol,
        Self::FairPlay,
        Self::Language,
    ];

    pub fn label(self, lang: UiLang) -> &'static str {
        match self {
            Self::Video => tr(lang, "Video", "ビデオ"),
            Self::Audio => tr(lang, "Audio", "オーディオ"),
            Self::Shaders => tr(lang, "Shaders", "シェーダー"),
            Self::Qol => tr(lang, "QoL", "QoL"),
            Self::FairPlay => tr(lang, "Fair-Play", "フェアプレイ"),
            Self::Language => tr(lang, "Language", "言語"),
        }
    }
}

/// The top-level screen the UI is showing.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Screen {
    /// Main menu: Singleplayer / Multiplayer / Play with Friends /
    /// Mods & Packs / Settings / Quit (+ quiet optional corner links).
    #[default]
    Title,
    /// Tabbed settings; [`SettingsTab`] picks the active page.
    Settings(SettingsTab),
    /// Megu details, license, and configured project links.
    AboutMegu,
    /// Host invite codes + guest join field.
    Friends,
    /// Explicit consent gate before a server's mod list is applied.
    ModSyncConsent,
    /// Integrated ModMenu-style Mods & Packs manager — draw and input are
    /// delegated to [`UiState::mod_menu`].
    ModMenu,
    /// JEI-style recipe & resource browser — draw and input are delegated
    /// to [`UiState::recipe_browser`].
    RecipeBrowser,
    /// In-game HUD (hotbar, vitals, bossbars, chat, tab list).
    InGameHud,
    /// Pause menu.
    Pause,
    /// NotEnoughCrashes-style recovery screen after a caught crash.
    CrashRecovery,
}

// ---------------------------------------------------------------------------
// Settings model
// ---------------------------------------------------------------------------

/// Named graphics bundle (mirrors `client-app`'s preset semantics).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum VideoPreset {
    Fast,
    #[default]
    Balanced,
    High,
    Ultra,
    /// At least one field was tuned away from a preset.
    Custom,
}

impl VideoPreset {
    pub const ALL: [Self; 5] = [
        Self::Fast,
        Self::Balanced,
        Self::High,
        Self::Ultra,
        Self::Custom,
    ];

    pub fn next(self) -> Self {
        let i = Self::ALL.iter().position(|p| *p == self).unwrap_or(0);
        Self::ALL[(i + 1) % Self::ALL.len()]
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::Fast => "Fast",
            Self::Balanced => "Balanced",
            Self::High => "High",
            Self::Ultra => "Ultra",
            Self::Custom => "Custom",
        }
    }
}

/// Distant-horizon LOD tier beyond full-detail view distance.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum LodMode {
    /// No LOD drawing.
    #[default]
    Off,
    /// LOD columns out to 64 chunks.
    Medium,
    /// LOD columns out to 128 chunks.
    Far,
}

impl LodMode {
    pub fn next(self) -> Self {
        match self {
            Self::Off => Self::Medium,
            Self::Medium => Self::Far,
            Self::Far => Self::Off,
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::Off => "Off",
            Self::Medium => "Medium (64c)",
            Self::Far => "Far (128c)",
        }
    }
}

/// GPU pipeline preference (mirrors `client-port`'s negotiation order).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum GpuBackend {
    /// Negotiate from hardware capabilities.
    #[default]
    Auto,
    /// Require task+mesh shader path.
    MeshShader,
    /// Require compute culling + multi-draw-indirect-count.
    ComputeIndirect,
    /// Plain vertex pipeline; always available.
    Classic,
    /// Minimal-hardware path for legacy iGPUs (GL 3.3 / Vulkan 1.1).
    ClassicLowSpec,
}

impl GpuBackend {
    pub fn next(self) -> Self {
        match self {
            Self::Auto => Self::MeshShader,
            Self::MeshShader => Self::ComputeIndirect,
            Self::ComputeIndirect => Self::Classic,
            Self::Classic => Self::ClassicLowSpec,
            Self::ClassicLowSpec => Self::Auto,
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::Auto => "Auto",
            Self::MeshShader => "Mesh Shader",
            Self::ComputeIndirect => "Compute Indirect",
            Self::Classic => "Classic",
            Self::ClassicLowSpec => "Classic Low-Spec",
        }
    }
}

/// FPS-cap slider stops, in order. `0` renders as "Unlimited".
pub const FPS_CAP_STEPS: [u32; 12] = [30, 60, 120, 144, 165, 240, 280, 320, 360, 480, 600, 0];
/// Smallest accepted custom FPS cap (`0` also means unlimited).
pub const CUSTOM_FPS_MIN: u32 = 10;
/// Largest accepted custom FPS cap.
pub const CUSTOM_FPS_MAX: u32 = 2000;
/// Selectable background-FPS values: throttle to 15 or disable (`0`).
pub const BACKGROUND_FPS_STEPS: [u16; 2] = [15, 0];

/// Minimum view distance (chunks).
pub const MIN_VIEW_DISTANCE: u8 = 6;
/// Maximum view distance (chunks).
pub const MAX_VIEW_DISTANCE: u8 = 32;
/// Chat history ring bounds (lines).
pub const MIN_CHAT_HISTORY: usize = 100;
pub const MAX_CHAT_HISTORY: usize = 5000;
/// Sound-coalescing cap bounds.
pub const MIN_CONCURRENT_SAME_SOUND: u8 = 1;
pub const MAX_CONCURRENT_SAME_SOUND: u8 = 32;

/// Video settings page state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VideoSettings {
    pub preset: VideoPreset,
    /// `6..=32` chunks.
    pub view_distance: u8,
    pub lod: LodMode,
    pub gpu_backend: GpuBackend,
    /// `0` = unlimited; otherwise `10..=2000` fps.
    pub fps_cap: u32,
    /// `15` = throttle unfocused window, `0` = no throttling.
    pub background_fps: u16,
    pub vsync: bool,
    pub smooth_lighting: bool,
}

impl Default for VideoSettings {
    fn default() -> Self {
        Self {
            preset: VideoPreset::Balanced,
            view_distance: 12,
            lod: LodMode::Off,
            gpu_backend: GpuBackend::Auto,
            fps_cap: 0,
            background_fps: 15,
            vsync: true,
            smooth_lighting: true,
        }
    }
}

impl VideoSettings {
    /// Slider step index closest to the current `fps_cap`.
    pub fn fps_step_index(&self) -> usize {
        if self.fps_cap == 0 {
            return FPS_CAP_STEPS.len() - 1;
        }
        FPS_CAP_STEPS
            .iter()
            .position(|&s| s == self.fps_cap)
            .unwrap_or_else(|| {
                // Custom value: nearest named step for the slider thumb.
                FPS_CAP_STEPS
                    .iter()
                    .enumerate()
                    .filter(|(_, &s)| s != 0)
                    .min_by_key(|(_, &s)| s.abs_diff(self.fps_cap))
                    .map(|(i, _)| i)
                    .unwrap_or(FPS_CAP_STEPS.len() - 1)
            })
    }

    /// Display string for the current cap (`"Unlimited"` for `0`).
    pub fn fps_cap_label(&self) -> String {
        if self.fps_cap == 0 {
            "Unlimited".to_string()
        } else {
            self.fps_cap.to_string()
        }
    }
}

/// Audio settings page state (volumes in percent `0..=100`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AudioSettings {
    pub master: u8,
    pub music: u8,
    pub blocks: u8,
    pub hostile: u8,
    pub friendly: u8,
    /// Hard ceiling for the mixed buffer, percent `0..=100`.
    pub max_volume_cap: u8,
    /// Max instances of the same `(sound, cell)` per tick, `1..=32`.
    pub max_concurrent_same_sound: u8,
}

impl Default for AudioSettings {
    fn default() -> Self {
        Self {
            master: 100,
            music: 70,
            blocks: 100,
            hostile: 100,
            friendly: 100,
            max_volume_cap: 100,
            max_concurrent_same_sound: 4,
        }
    }
}

/// When (and whether) a crash report may leave the machine.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CrashReportMode {
    /// Always ask first (the default; the recovery screen gates on it).
    #[default]
    Ask,
    /// Send automatically, but only for hard crashes.
    AutoCrashOnly,
    /// Never send.
    Off,
}

impl CrashReportMode {
    pub fn next(self) -> Self {
        match self {
            Self::Ask => Self::AutoCrashOnly,
            Self::AutoCrashOnly => Self::Off,
            Self::Off => Self::Ask,
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::Ask => "Ask",
            Self::AutoCrashOnly => "Auto (crashes only)",
            Self::Off => "Off",
        }
    }
}

/// Quality-of-life settings page state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QolSettings {
    /// Smooth scroll-zoom on a held key.
    pub smooth_zoom: bool,
    /// Warn when tool durability drops below this percent (`0..=100`).
    pub tool_warn_percent: u8,
    /// Opt-in guard blocking the swing that would break a tool.
    pub prevent_tool_break: bool,
    /// Chat ring capacity `100..=5000` lines.
    pub chat_history_lines: usize,
    /// Keep chat history across disconnects/reconnects.
    pub retain_chat_on_reconnect: bool,
    pub crash_report_mode: CrashReportMode,
    /// Whether optional Support/Contribute links appear on the title screen.
    pub show_support_links: bool,
    /// Opt-in MiniHUD overlay; `enabled` stays `false` by default so the
    /// vanilla-style HUD is untouched until the player turns it on.
    pub mini_hud: MiniHudConfig,
    /// Directional accessibility subtitles — opt-in (`false` by default).
    pub show_subtitles: bool,
}

impl Default for QolSettings {
    fn default() -> Self {
        Self {
            smooth_zoom: true,
            tool_warn_percent: 10,
            prevent_tool_break: false,
            chat_history_lines: 1000,
            retain_chat_on_reconnect: true,
            crash_report_mode: CrashReportMode::Ask,
            show_support_links: true,
            mini_hud: MiniHudConfig::default(),
            show_subtitles: false,
        }
    }
}

/// Coarse MiniHUD preset the QoL toggle cycles through.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum MiniHudPreset {
    /// Overlay hidden (default).
    #[default]
    Off,
    /// Basic segments only: fps, coords, facing, biome, ping.
    Basic,
    /// Basic plus every Advanced segment (1% low, MSPT/TPS, RAM, CPU,
    /// GPU, VRAM, chunks/entities, light, shader/audio).
    Advanced,
}

impl QolSettings {
    /// The preset the QoL MiniHUD toggle reports: `Off` while
    /// `mini_hud.enabled` is `false`, `Advanced` when every advanced
    /// segment is on, `Basic` otherwise.
    pub fn mini_hud_preset(&self) -> MiniHudPreset {
        if !self.mini_hud.enabled {
            MiniHudPreset::Off
        } else if self.mini_hud_is_advanced() {
            MiniHudPreset::Advanced
        } else {
            MiniHudPreset::Basic
        }
    }

    fn mini_hud_is_advanced(&self) -> bool {
        let m = &self.mini_hud;
        m.show_one_percent_low
            && m.show_mspt_tps
            && m.show_ram
            && m.show_cpu
            && m.show_gpu
            && m.show_vram
            && m.show_chunks_entities
            && m.show_light_level
            && m.show_shader_audio_status
    }

    /// Cycle the MiniHUD Off → Basic → Advanced → Off, preserving the
    /// configured anchor and the player's individual segment choices
    /// while the overlay stays enabled.
    pub fn cycle_mini_hud(&mut self) {
        match self.mini_hud_preset() {
            MiniHudPreset::Off => {
                self.mini_hud = MiniHudConfig {
                    enabled: true,
                    anchor: self.mini_hud.anchor,
                    ..MiniHudConfig::default()
                };
            }
            MiniHudPreset::Basic => {
                let m = &mut self.mini_hud;
                m.show_one_percent_low = true;
                m.show_mspt_tps = true;
                m.show_ram = true;
                m.show_cpu = true;
                m.show_gpu = true;
                m.show_vram = true;
                m.show_chunks_entities = true;
                m.show_light_level = true;
                m.show_shader_audio_status = true;
            }
            MiniHudPreset::Advanced => {
                self.mini_hud.enabled = false;
            }
        }
    }
}

/// How much map-mod feature surface a hosted world permits.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum MinimapMode {
    /// Map mods run with every feature enabled (default).
    #[default]
    Unrestricted,
    /// Maps run gated by the `allow_*` switches.
    FairPlay,
    /// No minimap/world-map features at all.
    Disabled,
}

impl MinimapMode {
    pub fn next(self) -> Self {
        match self {
            Self::Unrestricted => Self::FairPlay,
            Self::FairPlay => Self::Disabled,
            Self::Disabled => Self::Unrestricted,
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::Unrestricted => "Unrestricted",
            Self::FairPlay => "Fair-Play",
            Self::Disabled => "Disabled",
        }
    }
}

/// Who may appear on a player's radar in a hosted world.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PlayerRadarScope {
    /// Every player (default).
    #[default]
    All,
    /// Self, operators, and party members only.
    PartyOnly,
    /// Self and operators only.
    None,
}

impl PlayerRadarScope {
    pub fn next(self) -> Self {
        match self {
            Self::All => Self::PartyOnly,
            Self::PartyOnly => Self::None,
            Self::None => Self::All,
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::All => "All",
            Self::PartyOnly => "Party Only",
            Self::None => "None",
        }
    }
}

/// Host fair-play switches applied to worlds this client hosts
/// (integrated server, LAN share, friend relay). All-opt-in: defaults
/// permit everything, matching the server's `OPS-4` keys.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FairPlaySettings {
    pub minimap_mode: MinimapMode,
    pub allow_cave_map: bool,
    pub allow_mob_radar: bool,
    pub player_radar_scope: PlayerRadarScope,
    pub allow_waypoint_teleport: bool,
}

impl Default for FairPlaySettings {
    fn default() -> Self {
        Self {
            minimap_mode: MinimapMode::Unrestricted,
            allow_cave_map: true,
            allow_mob_radar: true,
            player_radar_scope: PlayerRadarScope::All,
            allow_waypoint_teleport: true,
        }
    }
}

/// Quick shader controls. The host owns the full shaderpack option editor.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ShaderQuickSettings {
    pub shaders_enabled: bool,
}

// ---------------------------------------------------------------------------
// Visual style & experience settings (zero-jargon, mistake-proof controls)
// ---------------------------------------------------------------------------

/// Built-in visual style preset. Three plain-language stops — no
/// confusing names (`Feather`) or jargon (`陰翳礼讃`) in the UI.
///
/// The ordering is deliberate: lightest first, richest last, with
/// `Vanilla` as the 100%-pure-vanilla default.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum VisualStylePreset {
    /// Pure vanilla rendering — nothing extra drawn (default).
    #[default]
    Vanilla,
    /// Megu Natural with no shadowmaps — fast even on Intel Celeron/iGPU.
    MeguNaturalLight,
    /// Full Megu lighting, shadows, reflections, and atmosphere.
    MeguNatural,
}

impl VisualStylePreset {
    /// All presets in their fixed cycle order.
    pub const ALL: [Self; 3] = [Self::Vanilla, Self::MeguNaturalLight, Self::MeguNatural];

    pub fn next(self) -> Self {
        self.step(1)
    }

    /// Cycle forward (`dir > 0`) or backward (`dir < 0`) through [`Self::ALL`].
    pub fn step(self, dir: i32) -> Self {
        let i = Self::ALL.iter().position(|p| *p == self).unwrap_or(0) as i32;
        Self::ALL[(i + dir).rem_euclid(Self::ALL.len() as i32) as usize]
    }

    pub const fn label(self, lang: UiLang) -> &'static str {
        match (self, lang) {
            (Self::Vanilla, UiLang::En) => "Vanilla (Lightest)",
            (Self::Vanilla, UiLang::Ja) => "Vanilla (最軽量)",
            (Self::MeguNaturalLight, UiLang::En) => "Megu Natural (Light)",
            (Self::MeguNaturalLight, UiLang::Ja) => "Megu Natural (軽量)",
            (Self::MeguNatural, _) => "Megu Natural",
        }
    }
}

/// Individual world-effect sub-toggles. The single `World Effects`
/// ON/OFF switch gates them all; these let a player keep e.g. water
/// ripples while dropping falling leaves without leaving the config.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorldEffectsDetail {
    pub water_splash: bool,
    pub ripples: bool,
    pub cherry_leaves: bool,
    pub autumn_leaves: bool,
    pub cold_breath: bool,
    pub night_sky: bool,
}

impl Default for WorldEffectsDetail {
    fn default() -> Self {
        Self {
            water_splash: true,
            ripples: true,
            cherry_leaves: true,
            autumn_leaves: true,
            cold_breath: true,
            night_sky: true,
        }
    }
}

/// The spatial-audio tracer always runs this many rays internally —
/// frame-budget pacing smooths the cost, so the UI offers a simple
/// ON/OFF with no ray-count dropdown.
pub const SPATIAL_AUDIO_3D_RAYS: u32 = 64;

/// The four clean two-way experience toggles plus the visual-style
/// preset. Every toggle is a plain `bool` — no four-level ladders.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VisualStyleSettings {
    /// Built-in style preset (`Vanilla` by default).
    pub preset: VisualStylePreset,
    /// When the host installed an external Minecraft shader pack it
    /// always wins: `Some(name)` shows a status line and yields every
    /// built-in visual shader to it.
    pub external_shader_pack_name: Option<String>,
    /// `3D Blocks & Skins` — OFF/ON.
    pub blocks_and_skins_3d: bool,
    /// `World Effects` — OFF/ON master switch for [`Self::world_effects_detail`].
    pub world_effects: bool,
    /// Per-effect sub-toggles under the `World Effects` switch.
    pub world_effects_detail: WorldEffectsDetail,
    /// `3D Audio` — OFF/ON. Internally [`SPATIAL_AUDIO_3D_RAYS`] rays.
    pub spatial_audio_3d: bool,
    /// `Camera Motion` — OFF/ON, independent of every other effect and
    /// default `false` for motion-sickness safety.
    pub camera_micro_motion: bool,
}

impl Default for VisualStyleSettings {
    fn default() -> Self {
        Self {
            preset: VisualStylePreset::Vanilla,
            external_shader_pack_name: None,
            blocks_and_skins_3d: false,
            world_effects: false,
            world_effects_detail: WorldEffectsDetail::default(),
            spatial_audio_3d: false,
            camera_micro_motion: false,
        }
    }
}

impl VisualStyleSettings {
    /// `true` while an external shader pack owns visual rendering and
    /// every built-in visual shader yields to it.
    pub fn builtin_shaders_yielded(&self) -> bool {
        self.external_shader_pack_name.is_some()
    }

    /// The clean status line shown while an external pack is active
    /// (`"External Shader Active: <name>"` / `"外部シェーダー優先中: <name>"`).
    pub fn external_shader_status(&self, lang: UiLang) -> Option<String> {
        self.external_shader_pack_name.as_ref().map(|name| {
            format!(
                "{}: {}",
                tr(lang, "External Shader Active", "外部シェーダー優先中"),
                name
            )
        })
    }
}

/// Everything the settings screens can change.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UiSettings {
    pub video: VideoSettings,
    pub audio: AudioSettings,
    pub shader_quick: ShaderQuickSettings,
    pub qol: QolSettings,
    /// Camera field of view in degrees (`30..=110`).
    pub fov_deg: u16,
    /// Mouse sensitivity as a percentage (`10..=200`, where 100 = 1.0).
    pub mouse_sensitivity_pct: u16,
    /// Zoom divisor in tenths (`15..=100`, where 40 = 4.0x).
    pub zoom_divisor_tenths: u16,
    /// Built-in Jade-style looking-at HUD, yielded when Jade is installed.
    pub qol_looking_at_hud: bool,
    /// Built-in mouse drag tweaks, yielded when Mouse Tweaks is installed.
    pub qol_mouse_tweaks: bool,
    /// Built-in inventory sorting, yielded when Inventory Sort is installed.
    pub qol_inventory_sort: bool,
    /// Zsh-style command bar with ghost completions and 8x8 inline badges.
    pub zsh_command_bar: bool,
    pub fair_play: FairPlaySettings,
    pub language: LanguageSetting,
    /// Full-fidelity locale selection (16 Tier-1 built-ins); the legacy
    /// [`LanguageSetting`] above stays synchronized for backward
    /// compatibility — `ja_jp` maps to `Japanese`, `en_us` to
    /// `English`, every other locale reports `Auto`.
    pub locale: BuiltinLocale,
    /// Visual-style preset and the four OFF/ON experience toggles.
    pub visual: VisualStyleSettings,
}

impl Default for UiSettings {
    fn default() -> Self {
        Self {
            video: VideoSettings::default(),
            audio: AudioSettings::default(),
            shader_quick: ShaderQuickSettings::default(),
            qol: QolSettings::default(),
            fov_deg: 70,
            mouse_sensitivity_pct: 100,
            zoom_divisor_tenths: 40,
            qol_looking_at_hud: true,
            qol_mouse_tweaks: true,
            qol_inventory_sort: true,
            zsh_command_bar: true,
            fair_play: FairPlaySettings::default(),
            language: LanguageSetting::Auto,
            locale: BuiltinLocale::EnUs,
            visual: VisualStyleSettings::default(),
        }
    }
}

impl UiSettings {
    /// Effective camera FOV in degrees, bounded even if a public field was set directly.
    pub fn fov_f32(&self) -> f32 {
        self.fov_deg.clamp(30, 110) as f32
    }

    /// Effective mouse sensitivity multiplier (`0.10..=2.00`).
    pub fn mouse_sensitivity_f32(&self) -> f32 {
        self.mouse_sensitivity_pct.clamp(10, 200) as f32 / 100.0
    }

    /// Effective camera zoom divisor (`1.5..=10.0`).
    pub fn zoom_divisor_f32(&self) -> f32 {
        self.zoom_divisor_tenths.clamp(15, 100) as f32 / 10.0
    }

    pub fn set_fov_deg(&mut self, value: u16) {
        self.fov_deg = value.clamp(30, 110);
    }

    pub fn set_mouse_sensitivity_pct(&mut self, value: u16) {
        self.mouse_sensitivity_pct = value.clamp(10, 200);
    }

    pub fn set_zoom_divisor_tenths(&mut self, value: u16) {
        self.zoom_divisor_tenths = value.clamp(15, 100);
    }

    /// Enable the built-in looking-at HUD only if Jade does not own it.
    pub fn builtin_looking_at_hud_active(&self, jade_installed: bool) -> bool {
        self.qol_looking_at_hud && !jade_installed
    }

    /// Enable the built-in drag behavior only if Mouse Tweaks does not own it.
    pub fn builtin_mouse_tweaks_active(&self, mouse_tweaks_installed: bool) -> bool {
        self.qol_mouse_tweaks && !mouse_tweaks_installed
    }

    /// Enable the built-in sorting only if Inventory Sort does not own it.
    pub fn builtin_inventory_sort_active(&self, inventory_sort_installed: bool) -> bool {
        self.qol_inventory_sort && !inventory_sort_installed
    }
}

// ---------------------------------------------------------------------------
// Settings session transaction (undo/redo + per-item revert + guarded reset)
// ---------------------------------------------------------------------------

/// A point-in-time copy of every player-facing setting.
pub type SettingsSnapshot = UiSettings;

/// Hard cap on the undo/redo history kept per settings session.
pub const SETTINGS_SESSION_UNDO_CAP: usize = 64;

/// Identifies one revertable setting for dirty tracking and
/// [`UiState::revert_single_setting`]. `Language` deliberately covers
/// both [`UiSettings::language`] and [`UiSettings::locale`] so the two
/// synchronized fields always move together.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SettingKey {
    VideoPreset,
    VideoViewDistance,
    VideoLod,
    VideoGpuBackend,
    VideoFpsCap,
    VideoBackgroundFps,
    VideoVsync,
    VideoSmoothLighting,
    AudioMaster,
    AudioMusic,
    AudioBlocks,
    AudioHostile,
    AudioFriendly,
    AudioMaxVolumeCap,
    AudioMaxConcurrentSameSound,
    AudioSpatial3d,
    ShadersEnabled,
    VisualStylePreset,
    ExternalShaderPack,
    BlocksAndSkins3d,
    WorldEffects,
    WorldEffectsDetail,
    CameraMicroMotion,
    FovDeg,
    MouseSensitivityPct,
    ZoomDivisorTenths,
    QolLookingAtHud,
    QolMouseTweaks,
    QolInventorySort,
    ZshCommandBar,
    QolSmoothZoom,
    QolToolWarnPercent,
    QolPreventToolBreak,
    QolChatHistoryLines,
    QolRetainChatOnReconnect,
    QolCrashReportMode,
    QolShowSupportLinks,
    QolMiniHud,
    QolShowSubtitles,
    FpMinimapMode,
    FpAllowCaveMap,
    FpAllowMobRadar,
    FpPlayerRadarScope,
    FpAllowWaypointTeleport,
    Language,
}

/// The open-settings-session bookkeeping: the snapshot taken on entry
/// plus the bounded undo/redo history built by edits inside the
/// session. Even a confirmed reset-to-defaults stays one Undo away.
#[derive(Clone, Debug, Default)]
pub struct SettingsTransaction {
    /// What the settings looked like when `Screen::Settings(_)` opened.
    pub opened_snapshot: SettingsSnapshot,
    /// Prior states, oldest first; drops the front past
    /// [`SETTINGS_SESSION_UNDO_CAP`].
    pub undo_stack: Vec<SettingsSnapshot>,
    /// States recoverable via redo; cleared by every fresh edit.
    pub redo_stack: Vec<SettingsSnapshot>,
}

impl SettingsTransaction {
    /// `true` when `key`'s field(s) in `current` equal the session-open
    /// snapshot.
    fn field_equal(&self, current: &UiSettings, key: SettingKey) -> bool {
        let opened = &self.opened_snapshot;
        use SettingKey as K;
        match key {
            K::VideoPreset => current.video.preset == opened.video.preset,
            K::VideoViewDistance => current.video.view_distance == opened.video.view_distance,
            K::VideoLod => current.video.lod == opened.video.lod,
            K::VideoGpuBackend => current.video.gpu_backend == opened.video.gpu_backend,
            K::VideoFpsCap => current.video.fps_cap == opened.video.fps_cap,
            K::VideoBackgroundFps => current.video.background_fps == opened.video.background_fps,
            K::VideoVsync => current.video.vsync == opened.video.vsync,
            K::VideoSmoothLighting => {
                current.video.smooth_lighting == opened.video.smooth_lighting
            }
            K::AudioMaster => current.audio.master == opened.audio.master,
            K::AudioMusic => current.audio.music == opened.audio.music,
            K::AudioBlocks => current.audio.blocks == opened.audio.blocks,
            K::AudioHostile => current.audio.hostile == opened.audio.hostile,
            K::AudioFriendly => current.audio.friendly == opened.audio.friendly,
            K::AudioMaxVolumeCap => current.audio.max_volume_cap == opened.audio.max_volume_cap,
            K::AudioMaxConcurrentSameSound => {
                current.audio.max_concurrent_same_sound
                    == opened.audio.max_concurrent_same_sound
            }
            K::AudioSpatial3d => current.visual.spatial_audio_3d == opened.visual.spatial_audio_3d,
            K::ShadersEnabled => {
                current.shader_quick.shaders_enabled == opened.shader_quick.shaders_enabled
            }
            K::VisualStylePreset => current.visual.preset == opened.visual.preset,
            K::ExternalShaderPack => {
                current.visual.external_shader_pack_name
                    == opened.visual.external_shader_pack_name
            }
            K::BlocksAndSkins3d => {
                current.visual.blocks_and_skins_3d == opened.visual.blocks_and_skins_3d
            }
            K::WorldEffects => current.visual.world_effects == opened.visual.world_effects,
            K::WorldEffectsDetail => {
                current.visual.world_effects_detail == opened.visual.world_effects_detail
            }
            K::CameraMicroMotion => {
                current.visual.camera_micro_motion == opened.visual.camera_micro_motion
            }
            K::FovDeg => current.fov_deg == opened.fov_deg,
            K::MouseSensitivityPct => current.mouse_sensitivity_pct == opened.mouse_sensitivity_pct,
            K::ZoomDivisorTenths => current.zoom_divisor_tenths == opened.zoom_divisor_tenths,
            K::QolLookingAtHud => current.qol_looking_at_hud == opened.qol_looking_at_hud,
            K::QolMouseTweaks => current.qol_mouse_tweaks == opened.qol_mouse_tweaks,
            K::QolInventorySort => current.qol_inventory_sort == opened.qol_inventory_sort,
            K::ZshCommandBar => current.zsh_command_bar == opened.zsh_command_bar,
            K::QolSmoothZoom => current.qol.smooth_zoom == opened.qol.smooth_zoom,
            K::QolToolWarnPercent => {
                current.qol.tool_warn_percent == opened.qol.tool_warn_percent
            }
            K::QolPreventToolBreak => {
                current.qol.prevent_tool_break == opened.qol.prevent_tool_break
            }
            K::QolChatHistoryLines => {
                current.qol.chat_history_lines == opened.qol.chat_history_lines
            }
            K::QolRetainChatOnReconnect => {
                current.qol.retain_chat_on_reconnect == opened.qol.retain_chat_on_reconnect
            }
            K::QolCrashReportMode => {
                current.qol.crash_report_mode == opened.qol.crash_report_mode
            }
            K::QolShowSupportLinks => {
                current.qol.show_support_links == opened.qol.show_support_links
            }
            K::QolMiniHud => current.qol.mini_hud == opened.qol.mini_hud,
            K::QolShowSubtitles => current.qol.show_subtitles == opened.qol.show_subtitles,
            K::FpMinimapMode => current.fair_play.minimap_mode == opened.fair_play.minimap_mode,
            K::FpAllowCaveMap => {
                current.fair_play.allow_cave_map == opened.fair_play.allow_cave_map
            }
            K::FpAllowMobRadar => {
                current.fair_play.allow_mob_radar == opened.fair_play.allow_mob_radar
            }
            K::FpPlayerRadarScope => {
                current.fair_play.player_radar_scope == opened.fair_play.player_radar_scope
            }
            K::FpAllowWaypointTeleport => {
                current.fair_play.allow_waypoint_teleport
                    == opened.fair_play.allow_waypoint_teleport
            }
            K::Language => {
                current.language == opened.language && current.locale == opened.locale
            }
        }
    }

    /// Copy `key`'s field(s) from the session-open snapshot into
    /// `current`. Callers push the prior state themselves.
    fn restore_field(&self, current: &mut UiSettings, key: SettingKey) {
        let opened = &self.opened_snapshot;
        use SettingKey as K;
        match key {
            K::VideoPreset => current.video.preset = opened.video.preset,
            K::VideoViewDistance => current.video.view_distance = opened.video.view_distance,
            K::VideoLod => current.video.lod = opened.video.lod,
            K::VideoGpuBackend => current.video.gpu_backend = opened.video.gpu_backend,
            K::VideoFpsCap => current.video.fps_cap = opened.video.fps_cap,
            K::VideoBackgroundFps => current.video.background_fps = opened.video.background_fps,
            K::VideoVsync => current.video.vsync = opened.video.vsync,
            K::VideoSmoothLighting => {
                current.video.smooth_lighting = opened.video.smooth_lighting
            }
            K::AudioMaster => current.audio.master = opened.audio.master,
            K::AudioMusic => current.audio.music = opened.audio.music,
            K::AudioBlocks => current.audio.blocks = opened.audio.blocks,
            K::AudioHostile => current.audio.hostile = opened.audio.hostile,
            K::AudioFriendly => current.audio.friendly = opened.audio.friendly,
            K::AudioMaxVolumeCap => current.audio.max_volume_cap = opened.audio.max_volume_cap,
            K::AudioMaxConcurrentSameSound => {
                current.audio.max_concurrent_same_sound =
                    opened.audio.max_concurrent_same_sound
            }
            K::AudioSpatial3d => {
                current.visual.spatial_audio_3d = opened.visual.spatial_audio_3d
            }
            K::ShadersEnabled => {
                current.shader_quick.shaders_enabled = opened.shader_quick.shaders_enabled
            }
            K::VisualStylePreset => current.visual.preset = opened.visual.preset,
            K::ExternalShaderPack => {
                current.visual.external_shader_pack_name =
                    opened.visual.external_shader_pack_name.clone()
            }
            K::BlocksAndSkins3d => {
                current.visual.blocks_and_skins_3d = opened.visual.blocks_and_skins_3d
            }
            K::WorldEffects => current.visual.world_effects = opened.visual.world_effects,
            K::WorldEffectsDetail => {
                current.visual.world_effects_detail =
                    opened.visual.world_effects_detail.clone()
            }
            K::CameraMicroMotion => {
                current.visual.camera_micro_motion = opened.visual.camera_micro_motion
            }
            K::FovDeg => current.fov_deg = opened.fov_deg,
            K::MouseSensitivityPct => current.mouse_sensitivity_pct = opened.mouse_sensitivity_pct,
            K::ZoomDivisorTenths => current.zoom_divisor_tenths = opened.zoom_divisor_tenths,
            K::QolLookingAtHud => current.qol_looking_at_hud = opened.qol_looking_at_hud,
            K::QolMouseTweaks => current.qol_mouse_tweaks = opened.qol_mouse_tweaks,
            K::QolInventorySort => current.qol_inventory_sort = opened.qol_inventory_sort,
            K::ZshCommandBar => current.zsh_command_bar = opened.zsh_command_bar,
            K::QolSmoothZoom => current.qol.smooth_zoom = opened.qol.smooth_zoom,
            K::QolToolWarnPercent => {
                current.qol.tool_warn_percent = opened.qol.tool_warn_percent
            }
            K::QolPreventToolBreak => {
                current.qol.prevent_tool_break = opened.qol.prevent_tool_break
            }
            K::QolChatHistoryLines => {
                current.qol.chat_history_lines = opened.qol.chat_history_lines
            }
            K::QolRetainChatOnReconnect => {
                current.qol.retain_chat_on_reconnect = opened.qol.retain_chat_on_reconnect
            }
            K::QolCrashReportMode => {
                current.qol.crash_report_mode = opened.qol.crash_report_mode
            }
            K::QolShowSupportLinks => {
                current.qol.show_support_links = opened.qol.show_support_links
            }
            K::QolMiniHud => current.qol.mini_hud = opened.qol.mini_hud.clone(),
            K::QolShowSubtitles => current.qol.show_subtitles = opened.qol.show_subtitles,
            K::FpMinimapMode => {
                current.fair_play.minimap_mode = opened.fair_play.minimap_mode
            }
            K::FpAllowCaveMap => {
                current.fair_play.allow_cave_map = opened.fair_play.allow_cave_map
            }
            K::FpAllowMobRadar => {
                current.fair_play.allow_mob_radar = opened.fair_play.allow_mob_radar
            }
            K::FpPlayerRadarScope => {
                current.fair_play.player_radar_scope = opened.fair_play.player_radar_scope
            }
            K::FpAllowWaypointTeleport => {
                current.fair_play.allow_waypoint_teleport =
                    opened.fair_play.allow_waypoint_teleport
            }
            K::Language => {
                current.language = opened.language;
                current.locale = opened.locale;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Adaptive startup telemetry (instant boot + live loading timeline)
// ---------------------------------------------------------------------------

/// One measured stage of the client boot sequence.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StartupStageTiming {
    /// Short stage name (`"kernel-init"`, `"mod-scan"`, ...).
    pub stage: String,
    /// Wall time the stage consumed, milliseconds.
    pub duration_ms: u32,
    /// Optional extra context (`"42 mods"`, `"wgpu"`).
    pub detail: String,
}

/// Whole-boot timing the launcher fills in. Fast boots
/// (`total_ms < 250`) never show a splash; slower ones feed the live
/// progress overlay, and afterwards a compact `Boot: N ms` badge stays
/// available on Title/About.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StartupTimeline {
    /// End-to-end boot time, milliseconds.
    pub total_ms: u32,
    /// Per-stage breakdown, in execution order.
    pub stages: Vec<StartupStageTiming>,
    /// Mods that ran on the native hot path.
    pub native_mods: u32,
    /// Mods automatically routed onto the wgpu path.
    pub wgpu_routed_mods: u32,
    /// Mods quarantined by fault isolation during boot.
    pub quarantined_mods: u32,
}

/// Boot at or above this many milliseconds shows the live overlay.
pub const LIVE_LOADING_OVERLAY_MIN_MS: u32 = 250;

impl StartupTimeline {
    /// `true` only when the boot was slow enough to be worth a live
    /// splash (`>= 250 ms`); fast boots go straight to Title.
    pub fn should_show_live_overlay(&self) -> bool {
        self.total_ms >= LIVE_LOADING_OVERLAY_MIN_MS
    }
}

// ---------------------------------------------------------------------------
// Friends invite codes (MEGU-XXXX-XXXX, Crockford Base32)
// ---------------------------------------------------------------------------

/// Crockford Base32 alphabet: digits + letters minus ambiguous I/L/O/U.
const CROCKFORD: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

/// Render a 40-bit code as `MEGU-XXXX-XXXX` (Crockford Base32).
pub fn invite_code_display(value: u64) -> String {
    let v = value & 0xff_ffff_ffff;
    let mut d = [b'0'; 8];
    for (i, ch) in d.iter_mut().enumerate() {
        *ch = CROCKFORD[((v >> (35 - 5 * i)) & 31) as usize];
    }
    let body = String::from_utf8_lossy(&d);
    format!("MEGU-{}-{}", &body[..4], &body[4..])
}

/// Tolerant-but-fail-closed `MEGU-XXXX-XXXX` parse: optional `MEGU`
/// prefix, optional hyphens, case-insensitive, `o→0`, `i/l→1`; anything
/// else is `None`.
pub fn invite_code_parse(text: &str) -> Option<u64> {
    let mut norm = String::with_capacity(text.len());
    for ch in text.chars() {
        if ch == '-' {
            continue;
        }
        if !ch.is_ascii() {
            return None;
        }
        norm.push(ch.to_ascii_uppercase());
    }
    let body = norm.strip_prefix("MEGU").unwrap_or(&norm);
    if body.len() != 8 {
        return None;
    }
    let mut value = 0u64;
    for &b in body.as_bytes() {
        let digit = match b {
            b'O' => b'0',
            b'I' | b'L' => b'1',
            other => other,
        };
        let idx = CROCKFORD.iter().position(|&c| c == digit)? as u64;
        value = (value << 5) | idx;
    }
    Some(value)
}

/// Deterministic splitmix64 step for invite-code generation. The caller
/// supplies the seed (`UiState::invite_seed`) — this crate never reads a
/// clock, so generation order is fully reproducible for a given seed.
pub fn next_invite_code(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^= z >> 31;
    z & 0xff_ffff_ffff
}

// ---------------------------------------------------------------------------
// Mod sync consent
// ---------------------------------------------------------------------------

/// What the host wants to do with one mod.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModSyncChange {
    Added,
    Updated,
    Removed,
}

impl ModSyncChange {
    pub const fn marker(self) -> &'static str {
        match self {
            Self::Added => "+",
            Self::Updated => "~",
            Self::Removed => "-",
        }
    }
}

/// One pending mod change, shown verbatim in the consent list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModSyncEntry {
    pub name: String,
    /// Hex SHA-256 digest displayed (truncated) and carried to the
    /// installer so it can verify the staged file.
    pub sha256_hex: String,
    pub change: ModSyncChange,
}

impl ModSyncEntry {
    pub fn added(name: impl Into<String>, sha256_hex: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            sha256_hex: sha256_hex.into(),
            change: ModSyncChange::Added,
        }
    }

    /// Short digest shown in the list (first 12 hex chars).
    pub fn short_sha(&self) -> &str {
        let end = self.sha256_hex.len().min(12);
        &self.sha256_hex[..end]
    }
}

// ---------------------------------------------------------------------------
// HUD state
// ---------------------------------------------------------------------------

/// Where a HUD-subtitle sound originated, relative to the player.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SubtitleDirection {
    /// Sound to the left — drawn `"< label"`.
    Left,
    /// Sound roughly centered — drawn `"label"`.
    Center,
    /// Sound to the right — drawn `"label >"`.
    Right,
    /// Sound behind the player — drawn `"v label"`.
    Behind,
}

/// One accessibility subtitle row; the game fills it, the HUD draws it
/// only when [`QolSettings::show_subtitles`] is on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HudSubtitle {
    /// Sound label, e.g. `"Zombie groans"`.
    pub label: String,
    /// Direction marker to render.
    pub direction: SubtitleDirection,
}

/// Everything the in-game HUD draws. `health`/`hunger`/`armor` are in
/// vanilla half-icon units (20 = full bar); `air` is in bubbles×30
/// (300 = full). Pure data — the game fills it, the HUD draws it.
#[derive(Clone, Debug)]
pub struct HudState {
    pub health: f32,
    pub absorption: f32,
    pub armor: u8,
    pub hunger: u8,
    pub xp_level: u32,
    /// `0.0..=1.0` toward the next level.
    pub xp_progress: f32,
    /// `0..=300` air supply (bubbles hidden at 300).
    pub air: u16,
    /// `(title, fraction)` boss bars, top of screen.
    pub bossbars: Vec<(String, f32)>,
    /// Item names per hotbar slot (`None` = empty slot).
    pub hotbar: [Option<String>; 9],
    pub offhand: Option<String>,
    pub selected_slot: usize,
    /// Transient message above the hotbar.
    pub actionbar: String,
    /// Chat scrollback, oldest first.
    pub chat_lines: Vec<String>,
    /// Player-list overlay visibility (hold-tab semantics).
    pub show_player_list: bool,
    /// `(name, ping_ms)` for the tab list.
    pub players: Vec<(String, u16)>,
    /// Optional mana, casting, cooldown, and custom status overlays.
    pub magic: crate::magic_hud::MagicHudState,
    /// Telemetry snapshot the opt-in MiniHUD overlay reads.
    pub mini_hud_data: MiniHudData,
    /// Directional subtitle rows, newest last (drawn only when
    /// [`QolSettings::show_subtitles`] is on).
    pub subtitles: Vec<HudSubtitle>,
    /// Live spatial-subtitle panel state: the game feeds
    /// [`crate::subtitle_hud::SubtitleHudState::push`] from drained audio
    /// cues and ticks it on the frame clock; the HUD composites it via
    /// [`crate::layout::layout_subtitle_hud`] when `show_subtitles` is on.
    pub subtitle_hud: crate::subtitle_hud::SubtitleHudState,
    /// Crafting-conflict switcher: `total_candidates >= 2` draws one
    /// compact `< k/N >` cycle button; `<= 1` emits nothing at all.
    pub crafting_conflict: CraftingConflictOverlay,
    /// Scoreboard sidebar state (`hud.scoreboard`): objective title +
    /// sorted score entries; hidden until `visible` is set.
    pub scoreboard: ScoreboardState,
    /// Title/subtitle overlay (`hud.title`): vanilla fade timing driven
    /// by [`TitleOverlayState::tick`].
    pub title_overlay: TitleOverlayState,
    /// Jade/WAILA-style looking-at-block overlay (`hud.jade_looking_at`).
    pub looking_at: crate::magic_hud::LookingAtBlockState,
}

/// Compact in-HUD recipe-conflict resolver state.
///
/// When a crafting station matches more than one recipe for the same
/// inputs (mod packs regularly create such collisions), the game fills
/// `total_candidates` and the HUD draws a single tiny cycle button —
/// the zero-clutter resolution path. `total_candidates <= 1` (or `0`)
/// emits no draw commands and no hit region, so a conflict-free HUD is
/// pixel-identical to vanilla.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CraftingConflictOverlay {
    /// Selected candidate, `0`-based; clicks wrap modulo
    /// `total_candidates`.
    pub selected_index: usize,
    /// Number of conflicting recipes the game reports; the switcher only
    /// exists while this is `>= 2`.
    pub total_candidates: usize,
}

impl CraftingConflictOverlay {
    /// `true` while the switcher must be drawn (two or more candidates).
    pub fn is_active(&self) -> bool {
        self.total_candidates >= 2
    }

    /// Selected candidate clamped into `0..total_candidates` for display.
    pub fn selected(&self) -> usize {
        self.selected_index
            .min(self.total_candidates.saturating_sub(1))
    }
}

// ---------------------------------------------------------------------------
// HUD feature states (scoreboard sidebar, title overlay)
// ---------------------------------------------------------------------------

/// Vanilla sidebar cap: an objective display renders at most 15 rows.
pub const SCOREBOARD_SIDEBAR_MAX_ROWS: usize = 15;

/// Sidebar scoreboard state (`hud.scoreboard`): an objective title plus
/// score entries the game fills from scoreboard S2C packets. Pure data —
/// [`ScoreboardState::sidebar`] converts to the
/// [`crate::hud_widgets::ScoreboardSidebar`] render model and
/// [`crate::layout::layout_scoreboard_sidebar`] draws it on the right
/// edge while [`visible`](Self::visible) is set.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ScoreboardState {
    /// Sidebar title; `None` (or empty) draws rows without a header,
    /// matching vanilla's untitled sidebar.
    pub objective_title: Option<String>,
    /// Unsorted score entries; [`sorted_entries`](Self::sorted_entries)
    /// orders them score-descending (label ascending on ties) and caps
    /// at [`SCOREBOARD_SIDEBAR_MAX_ROWS`].
    pub entries: Vec<crate::hud_widgets::ScoreboardEntry>,
    /// Show/hide flag: `false` draws nothing even when populated.
    pub visible: bool,
}

impl ScoreboardState {
    pub fn new() -> Self {
        Self::default()
    }

    /// `true` when a non-empty title or at least one labelled entry
    /// exists to show.
    pub fn has_content(&self) -> bool {
        self.objective_title.as_ref().is_some_and(|t| !t.is_empty())
            || self.entries.iter().any(|e| !e.label.is_empty())
    }

    /// `true` while the sidebar should draw this frame.
    pub fn should_render(&self) -> bool {
        self.visible && self.has_content()
    }

    /// Entries sorted score-descending with label-ascending tie-break,
    /// truncated to the vanilla 15-row sidebar limit.
    pub fn sorted_entries(&self) -> Vec<crate::hud_widgets::ScoreboardEntry> {
        let mut rows = self.entries.clone();
        rows.sort_by(|a, b| b.score.cmp(&a.score).then_with(|| a.label.cmp(&b.label)));
        rows.truncate(SCOREBOARD_SIDEBAR_MAX_ROWS);
        rows
    }

    /// Convert into the widget-pack render model consumed by
    /// [`crate::layout::layout_scoreboard_sidebar`].
    pub fn sidebar(&self) -> crate::hud_widgets::ScoreboardSidebar {
        crate::hud_widgets::ScoreboardSidebar {
            title: self.objective_title.clone().filter(|t| !t.is_empty()),
            entries: self.sorted_entries(),
        }
    }
}

/// Title/subtitle overlay state (`hud.title`) with vanilla's
/// fade-in / hold / fade-out tick timing. The game calls
/// [`show`](Self::show) when the Set Titles packet arrives and
/// [`tick`](Self::tick) once per client tick; the HUD composites
/// [`overlay`](Self::overlay) — a [`crate::hud_widgets::TitleOverlay`]
/// of [`TimedText`](crate::hud_widgets::TimedText) — while any part is
/// visible.
#[derive(Clone, Debug, PartialEq)]
pub struct TitleOverlayState {
    /// Big centered title; empty means no title line this overlay.
    pub title: String,
    /// Smaller line under the title.
    pub subtitle: String,
    /// Ticks spent fading in (vanilla default 10).
    pub fade_in_ticks: u32,
    /// Ticks held at full alpha (vanilla default 70).
    pub stay_ticks: u32,
    /// Ticks spent fading out (vanilla default 20).
    pub fade_out_ticks: u32,
    /// Ticks since [`show`](Self::show); saturates at `u32::MAX`.
    pub age_ticks: u32,
}

impl Default for TitleOverlayState {
    fn default() -> Self {
        Self::vanilla()
    }
}

impl TitleOverlayState {
    /// An idle overlay with vanilla 1.21.1 timing (10 in / 70 stay /
    /// 20 out ticks) and no text.
    pub fn vanilla() -> Self {
        Self {
            title: String::new(),
            subtitle: String::new(),
            fade_in_ticks: 10,
            stay_ticks: 70,
            fade_out_ticks: 20,
            age_ticks: 0,
        }
    }

    /// Show `title`/`subtitle` with explicit phase lengths and reset the
    /// clock. Empty strings omit that line.
    pub fn show(
        &mut self,
        title: impl Into<String>,
        subtitle: impl Into<String>,
        fade_in_ticks: u32,
        stay_ticks: u32,
        fade_out_ticks: u32,
    ) {
        self.title = title.into();
        self.subtitle = subtitle.into();
        self.fade_in_ticks = fade_in_ticks;
        self.stay_ticks = stay_ticks;
        self.fade_out_ticks = fade_out_ticks;
        self.age_ticks = 0;
    }

    /// [`show`](Self::show) with the vanilla 10/70/20 timing.
    pub fn show_vanilla(&mut self, title: impl Into<String>, subtitle: impl Into<String>) {
        self.show(title, subtitle, 10, 70, 20);
    }

    /// Hide the overlay immediately (next render emits nothing).
    pub fn clear(&mut self) {
        self.title.clear();
        self.subtitle.clear();
        self.age_ticks = 0;
    }

    /// Advance the overlay one client tick (20 ticks per second).
    pub fn tick(&mut self) {
        self.age_ticks = self.age_ticks.saturating_add(1);
    }

    /// Total lifetime in ticks: `fade_in + stay + fade_out`.
    pub fn total_ticks(&self) -> u32 {
        self.fade_in_ticks
            .saturating_add(self.stay_ticks)
            .saturating_add(self.fade_out_ticks)
    }

    /// `true` while title or subtitle text exists and the timing window
    /// has not fully elapsed.
    pub fn is_active(&self) -> bool {
        (!self.title.is_empty() || !self.subtitle.is_empty()) && self.age_ticks < self.total_ticks()
    }

    /// Convert into the widget-pack render model. Returns `None` once
    /// the overlay has fully expired; individual lines may still be
    /// mid-fade (alpha &lt; 1) or fully transparent on the first fade-in
    /// tick — the renderer filters those itself.
    pub fn overlay(&self) -> Option<crate::hud_widgets::TitleOverlay> {
        if !self.is_active() {
            return None;
        }
        let timed = |text: &str| {
            (!text.is_empty()).then(|| crate::hud_widgets::TimedText {
                text: text.to_string(),
                age_ticks: self.age_ticks,
                fade_in_ticks: self.fade_in_ticks,
                stay_ticks: self.stay_ticks,
                fade_out_ticks: self.fade_out_ticks,
            })
        };
        Some(crate::hud_widgets::TitleOverlay {
            title: timed(&self.title),
            subtitle: timed(&self.subtitle),
            actionbar: None,
        })
    }
}

impl Default for HudState {
    fn default() -> Self {
        Self {
            health: 20.0,
            absorption: 0.0,
            armor: 0,
            hunger: 20,
            xp_level: 0,
            xp_progress: 0.0,
            air: 300,
            bossbars: Vec::new(),
            hotbar: Default::default(),
            offhand: None,
            selected_slot: 0,
            actionbar: String::new(),
            chat_lines: Vec::new(),
            show_player_list: false,
            players: Vec::new(),
            magic: crate::magic_hud::MagicHudState::default(),
            mini_hud_data: MiniHudData::default(),
            subtitles: Vec::new(),
            subtitle_hud: crate::subtitle_hud::SubtitleHudState::default(),
            crafting_conflict: CraftingConflictOverlay::default(),
            scoreboard: ScoreboardState::default(),
            title_overlay: TitleOverlayState::default(),
            looking_at: crate::magic_hud::LookingAtBlockState::default(),
        }
    }
}

impl HudState {
    /// A populated state for demos and tests — every bar filled, one
    /// bossbar, a chat line, and three tab-list players.
    pub fn demo() -> Self {
        let mut hotbar: [Option<String>; 9] = Default::default();
        hotbar[0] = Some("Sword".to_string());
        hotbar[1] = Some("Pick".to_string());
        hotbar[2] = Some("Torch".to_string());
        hotbar[8] = Some("Food".to_string());
        Self {
            health: 18.0,
            absorption: 4.0,
            armor: 15,
            hunger: 19,
            xp_level: 12,
            xp_progress: 0.6,
            air: 210,
            bossbars: vec![("Ender Dragon".to_string(), 0.65)],
            hotbar,
            offhand: Some("Shield".to_string()),
            selected_slot: 0,
            actionbar: "Demo HUD".to_string(),
            chat_lines: vec!["<Steve> hello".to_string(), "<Alex> hi!".to_string()],
            show_player_list: false,
            players: vec![
                ("Steve".to_string(), 12),
                ("Alex".to_string(), 34),
                ("Megu".to_string(), 88),
            ],
            magic: crate::magic_hud::MagicHudState::default(),
            mini_hud_data: MiniHudData::default(),
            subtitles: Vec::new(),
            subtitle_hud: crate::subtitle_hud::SubtitleHudState::default(),
            crafting_conflict: CraftingConflictOverlay::default(),
            scoreboard: ScoreboardState::default(),
            title_overlay: TitleOverlayState::default(),
            looking_at: crate::magic_hud::LookingAtBlockState::default(),
        }
    }
}

// ---------------------------------------------------------------------------
// Input & actions
// ---------------------------------------------------------------------------

/// Keys the router understands (typing arrives as [`UiInputEvent::CharTyped`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UiKey {
    Tab,
    Up,
    Down,
    Left,
    Right,
    Enter,
    Space,
    Escape,
    Backspace,
}

/// Gamepad buttons mapped onto the same navigation semantics.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GamepadButton {
    DpadUp,
    DpadDown,
    DpadLeft,
    DpadRight,
    /// South face button (A / Cross): activate.
    South,
    /// East face button (B / Circle): back.
    East,
    /// Start / Menu: pause or back.
    Start,
}

/// One device-agnostic input event.
#[derive(Clone, Debug, PartialEq)]
pub enum UiInputEvent {
    /// Mouse moved (updates hover highlight only).
    MouseMove { x: i32, y: i32 },
    /// Primary button pressed at `(x, y)`.
    Click { x: i32, y: i32 },
    /// A control key.
    KeyPressed(UiKey),
    /// A typed character (text fields only).
    CharTyped(char),
    /// A gamepad button press.
    GamepadPressed(GamepadButton),
}

/// What the embedding application must do after input. Pure settings
/// changes report the single marker [`UiAction::SettingsChanged`] — the
/// new value is read from [`UiState::settings`].
#[derive(Clone, Debug, PartialEq)]
pub enum UiAction {
    /// Replace the current screen, pushing it onto the nav stack.
    OpenScreen(Screen),
    /// Pop the nav stack (or land on Title).
    Back,
    /// Quit the game.
    QuitGame,
    /// Start a singleplayer world.
    StartSingleplayer,
    /// Open the server browser / connect flow.
    StartMultiplayer,
    /// Show the statistics screen (owned by the app layer).
    ShowStatistics,
    /// Open a configured URL in the system browser.
    OpenUrl(String),
    /// Open the host application's full shader option editor.
    OpenShaderOptions,
    /// Mint a new host invite code (result appears as `active_invite`).
    GenerateInvite,
    /// Copy `0`'s rendered code to the system clipboard.
    CopyInvite(String),
    /// Revoke the active invite code.
    RevokeInvite,
    /// Guest wants to join using this parsed code value.
    JoinWithCode(u64),
    /// Player explicitly accepted the pending mod sync; install `0`.
    ApplyModSync(Vec<ModSyncEntry>),
    /// Player explicitly requested the missing dependencies from Modrinth.
    DownloadMissingFromModrinth(Vec<String>),
    /// Player declined the pending mod sync.
    DeclineModSync,
    /// Player explicitly confirmed `"Send OK?"` on the crash screen.
    SubmitCrashReport,
    /// The integrated Mods & Packs screen produced a host action (apply
    /// staged toggles, open the mods folder, fetch deps, ...). `Close`
    /// and `OpenUrl` are flattened into [`UiAction::Back`] /
    /// [`UiAction::OpenUrl`] instead of appearing here.
    ModMenu(crate::mod_menu::ModMenuAction),
    /// The recipe & resource browser produced a host action (crafting
    /// grid transfer). `Close` is flattened into [`UiAction::Back`].
    RecipeBrowser(crate::recipe_browser::RecipeBrowserAction),
    /// Player cycled the crafting-conflict switcher; the new candidate
    /// is `hud.crafting_conflict.selected_index`.
    CycleCraftingConflict,
    /// One or more settings changed; app should persist.
    SettingsChanged,
}

/// Every interactive element that can own a [`HitRegion`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WidgetId {
    // Title
    TitleSingleplayer,
    TitleMultiplayer,
    TitleFriends,
    TitleMods,
    TitleSettings,
    TitleQuit,
    WebsiteLink,
    SupportLink,
    ContributeLink,
    ReportLink,
    // Settings chrome
    SettingsDone,
    SettingsAboutMegu,
    SettingsMods,
    // About Megu
    AboutBack,
    TabVideo,
    TabAudio,
    TabShaders,
    TabQol,
    TabFairPlay,
    TabLanguage,
    // Video tab
    VideoPreset,
    VideoViewDistance,
    VideoLod,
    VideoGpuBackend,
    VideoFpsSlider,
    VideoFpsField,
    VideoBackgroundFps,
    VideoVsync,
    VideoSmoothLighting,
    // Settings session toolbar (mistake-proof undo/redo/discard/reset)
    SettingsUndo,
    SettingsRedo,
    SettingsDiscard,
    SettingsResetDefaults,
    // Shaders tab
    ShadersEnabled,
    ShadersOpenOptions,
    VisualPreset,
    VisualBlocks3d,
    VisualWorldEffects,
    VisualCameraMotion,
    // Audio tab
    AudioMaster,
    AudioMusic,
    AudioBlocks,
    AudioHostile,
    AudioFriendly,
    AudioMaxVolumeCap,
    AudioMaxConcurrent,
    AudioSpatial3d,
    // QoL tab
    QolSmoothZoom,
    QolToolWarn,
    QolPreventBreak,
    QolChatHistory,
    QolRetainChat,
    QolCrashMode,
    QolShowSupportLinks,
    QolMiniHud,
    QolSubtitles,
    // Fair-Play tab
    FpMinimapMode,
    FpCaveMap,
    FpMobRadar,
    FpRadarScope,
    FpWaypointTeleport,
    // Language tab
    LanguageChoice,
    LanguageDetectOs,
    // Telemetry badge (Title/About bottom-right)
    BootBadge,
    // Friends
    FriendsGenerate,
    FriendsCopy,
    FriendsRevoke,
    FriendsJoinField,
    FriendsJoin,
    FriendsBack,
    // Mod sync
    ModSyncAccept,
    ModSyncDecline,
    ModSyncDownloadMissing,
    // Pause
    PauseBackToGame,
    PauseSettings,
    PauseStatistics,
    PauseRecipeBrowser,
    PauseInvite,
    PauseSaveQuit,
    PauseReportBug,
    // Crash recovery (+ confirmation modal)
    CrashSendReport,
    CrashReturnTitle,
    ModalOk,
    ModalCancel,
    // Embedded screens — every region the delegated sub-screens emit is
    // re-tagged into this id space so the unified router stays in charge.
    /// A widget inside the integrated Mods & Packs screen.
    ModMenu(crate::mod_menu::ModMenuWidgetId),
    /// A widget inside the recipe & resource browser.
    Recipe(crate::recipe_browser::RecipeWidgetId),
    /// Native chrome on the recipe browser: the close/back button the
    /// sub-screen itself does not provide.
    RecipeBrowserDone,
    // In-game HUD (present only while a crafting conflict is active)
    CraftConflictNext,
}

impl WidgetId {
    /// `true` for widgets Left/Right arrows *adjust* instead of moving
    /// focus: sliders and cycle buttons.
    pub const fn adjustable(self) -> bool {
        matches!(
            self,
            Self::VideoPreset
                | Self::VideoViewDistance
                | Self::VideoLod
                | Self::VideoGpuBackend
                | Self::VideoFpsSlider
                | Self::VideoBackgroundFps
                | Self::VideoVsync
                | Self::VideoSmoothLighting
                | Self::ShadersEnabled
                | Self::VisualPreset
                | Self::VisualBlocks3d
                | Self::VisualWorldEffects
                | Self::VisualCameraMotion
                | Self::AudioMaster
                | Self::AudioMusic
                | Self::AudioBlocks
                | Self::AudioHostile
                | Self::AudioFriendly
                | Self::AudioMaxVolumeCap
                | Self::AudioMaxConcurrent
                | Self::AudioSpatial3d
                | Self::QolSmoothZoom
                | Self::QolToolWarn
                | Self::QolPreventBreak
                | Self::QolChatHistory
                | Self::QolRetainChat
                | Self::QolCrashMode
                | Self::QolShowSupportLinks
                | Self::QolMiniHud
                | Self::QolSubtitles
                | Self::FpMinimapMode
                | Self::FpCaveMap
                | Self::FpMobRadar
                | Self::FpRadarScope
                | Self::FpWaypointTeleport
                | Self::LanguageChoice
        )
    }

    /// `true` for editable text fields (typing routes to them).
    pub const fn is_text_field(self) -> bool {
        matches!(self, Self::VideoFpsField | Self::FriendsJoinField)
    }
}

/// A clickable/focusable rectangle produced by the layout pass.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HitRegion {
    pub id: WidgetId,
    pub rect: UiRect,
    /// Modal regions: the only live regions while a modal is open.
    pub modal: bool,
}

impl HitRegion {
    pub const fn new(id: WidgetId, rect: UiRect) -> Self {
        Self {
            id,
            rect,
            modal: false,
        }
    }

    pub const fn modal(id: WidgetId, rect: UiRect) -> Self {
        Self {
            id,
            rect,
            modal: true,
        }
    }

    pub const fn contains(self, x: i32, y: i32) -> bool {
        self.rect.contains(x, y)
    }
}

/// Modal overlays (block input to the screen underneath).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UiModal {
    /// `"Send OK?"` gate before a crash report leaves the machine.
    ConfirmSendCrashReport,
    /// `"Reset all settings to defaults?"` gate — a single accidental
    /// click can never wipe the session's settings.
    ConfirmResetDefaults,
    /// Per-stage boot timeline opened from the `Boot: N ms` badge.
    StartupTimeline,
}

// ---------------------------------------------------------------------------
// UI state
// ---------------------------------------------------------------------------

/// Largest accepted FPS field text (`"2000"`, `0` handled separately).
const FPS_FIELD_MAX_LEN: usize = 4;
/// Largest accepted join field text (`"MEGU-XXXX-XXXX"` + slack).
const JOIN_FIELD_MAX_LEN: usize = 14;
/// Crash report preview cap: 4 KiB of already-redacted markdown.
pub const CRASH_PREVIEW_MAX_BYTES: usize = 4096;

/// The whole client UI state machine.
#[derive(Clone, Debug)]
pub struct UiState {
    /// Current screen.
    pub screen: Screen,
    /// Navigation stack for Back/Escape (`navigate` pushes).
    pub nav_stack: Vec<Screen>,
    /// All player-facing settings.
    pub settings: UiSettings,
    /// GUI scale: `0` = auto (`height/270`, clamped `1..=4`), else fixed.
    pub ui_scale: u8,
    /// Open modal, if any.
    pub modal: Option<UiModal>,
    /// Hit regions cached from the last [`UiState::render`].
    pub hit_regions: Vec<HitRegion>,
    /// Keyboard/gamepad focus: index into `hit_regions`.
    pub focus_index: Option<usize>,
    /// Widget under the mouse cursor.
    pub hover: Option<WidgetId>,

    /// Seed the deterministic invite-code generator starts from. The app
    /// fills this from its own entropy source — this crate reads no clock.
    pub invite_seed: u64,
    /// Generated-but-not-yet-revoked host invite code.
    pub active_invite: Option<u64>,
    /// Guest join field contents (raw text, parsed on Join).
    pub join_code_text: String,
    /// `true` after a failed join-code parse.
    pub join_error: bool,

    /// Pending mod-sync changes shown on [`Screen::ModSyncConsent`].
    pub pending_mod_sync: Vec<ModSyncEntry>,
    /// Missing dependency IDs available for an explicit Modrinth request.
    pub missing_mod_deps: Vec<String>,
    /// `Some(true)` = accepted, `Some(false)` = declined.
    pub mod_sync_decision: Option<bool>,

    /// HUD data the game fills per frame.
    pub hud: HudState,

    /// The crash being shown on [`Screen::CrashRecovery`].
    pub crash: Option<CrashContext>,
    /// `true` after the report was submitted once (button disables).
    pub crash_sent: bool,

    /// Optional quiet corner link on the title screen — hidden when empty.
    pub support_url: String,
    /// Optional project website, shown on Title and About when configured.
    pub website_url: String,
    /// Optional quiet corner link on the title screen — hidden when empty.
    pub contribute_url: String,
    /// Optional bug report URL on Title and Pause, hidden while empty.
    pub report_url: String,
    /// Current shaderpack display name, or empty for vanilla rendering.
    pub active_shaderpack_name: String,
    /// Current shaderpack profile label, if provided by the host.
    pub active_shaderpack_profile: String,
    /// Shaderpack selection and options supplied by the host loader.
    pub shader_packs: crate::shader_screen::ShaderPackScreenState,

    /// Mistake-proof settings session: opened snapshot + bounded
    /// undo/redo history + dirty tracking backing [`is_setting_dirty`].
    pub settings_tx: SettingsTransaction,
    /// Optional external Minecraft `.json` language packs layered over
    /// the 16 built-in locales; [`UiState::ui_text`] resolves through it.
    pub i18n_catalog: Option<I18nCatalog>,
    /// Launcher-supplied boot timing; powers the live-loading decision
    /// and the `Boot: N ms` telemetry badge.
    pub startup_timeline: Option<StartupTimeline>,

    /// The integrated ModMenu-style Mods & Packs screen
    /// ([`Screen::ModMenu`]). The host fills `entries`; `lang`/`ui_scale`
    /// are re-synced from this state on every [`render`](Self::render).
    pub mod_menu: crate::mod_menu::ModMenuState,
    /// The integrated recipe & resource browser
    /// ([`Screen::RecipeBrowser`]). The host fills `index`/`ponder`;
    /// `lang`/`ui_scale` are re-synced every frame.
    pub recipe_browser: crate::recipe_browser::RecipeBrowserState,

    invite_counter: u64,
    fps_field_focused: bool,
    fps_field_text: String,
    fps_field_error: bool,
    join_field_focused: bool,
    /// Physical-pixel viewport of the last [`render`](Self::render), used
    /// to forward pointer input into the embedded screens.
    render_w: u32,
    render_h: u32,
}

impl Default for UiState {
    fn default() -> Self {
        Self {
            screen: Screen::Title,
            nav_stack: Vec::new(),
            settings: UiSettings::default(),
            ui_scale: 0,
            modal: None,
            hit_regions: Vec::new(),
            focus_index: None,
            hover: None,
            invite_seed: 0,
            active_invite: None,
            join_code_text: String::new(),
            join_error: false,
            pending_mod_sync: Vec::new(),
            missing_mod_deps: Vec::new(),
            mod_sync_decision: None,
            hud: HudState::default(),
            crash: None,
            crash_sent: false,
            support_url: String::new(),
            website_url: String::new(),
            contribute_url: String::new(),
            report_url: String::new(),
            active_shaderpack_name: String::new(),
            active_shaderpack_profile: String::new(),
            shader_packs: crate::shader_screen::ShaderPackScreenState::default(),
            settings_tx: SettingsTransaction::default(),
            i18n_catalog: None,
            startup_timeline: None,
            mod_menu: crate::mod_menu::ModMenuState::default(),
            recipe_browser: crate::recipe_browser::RecipeBrowserState::default(),
            invite_counter: 0,
            fps_field_focused: false,
            fps_field_text: String::new(),
            fps_field_error: false,
            join_field_focused: false,
            render_w: 0,
            render_h: 0,
        }
    }
}

impl UiState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Effective GUI scale for `height` px tall output (`1..=4`).
    pub fn effective_scale(&self, height: u32) -> u32 {
        if self.ui_scale == 0 {
            (height / 270).clamp(1, 4)
        } else {
            u32::from(self.ui_scale.clamp(1, 4))
        }
    }

    /// Resolved display language. Backward compatible with the legacy
    /// [`LanguageSetting`]: `Japanese` renders `Ja` exactly as before,
    /// and a directly selected `ja_jp` locale does too.
    pub fn lang(&self) -> UiLang {
        if self.settings.locale == BuiltinLocale::JaJp
            || self.settings.language == LanguageSetting::Japanese
        {
            UiLang::Ja
        } else {
            UiLang::En
        }
    }

    /// Select one of the 16 built-in locales directly, keeping the
    /// legacy [`LanguageSetting`] and any external pack in sync.
    pub fn set_locale(&mut self, locale: BuiltinLocale) {
        self.settings.locale = locale;
        self.settings.language = match locale {
            BuiltinLocale::EnUs => LanguageSetting::English,
            BuiltinLocale::JaJp => LanguageSetting::Japanese,
            _ => LanguageSetting::Auto,
        };
        if let Some(catalog) = &mut self.i18n_catalog {
            // Built-in codes always parse; keep the catalog consistent.
            let _ = catalog.set_locale(locale.code());
        }
    }

    /// Detect the OS locale (POSIX `LC_ALL`/`LC_MESSAGES`/`LANG`) and
    /// apply it as the player's language preference. Reports `Auto` in
    /// the legacy field because the choice was automatic.
    pub fn detect_and_apply_os_locale(&mut self) {
        let detected = megu_core::i18n::detect_os_locale();
        self.set_locale(detected);
        self.settings.language = LanguageSetting::Auto;
    }

    /// The Language-tab label for the current selection: `"Auto"` while
    /// the auto default holds, otherwise the locale's native autonym.
    pub fn language_display_name(&self) -> String {
        if self.settings.language == LanguageSetting::Auto
            && self.settings.locale == BuiltinLocale::EnUs
        {
            tr(self.lang(), "Auto", "自動").to_string()
        } else {
            self.settings.locale.native_name().to_string()
        }
    }

    /// Cycle the language selection: `Auto → English → Japanese →`
    /// every remaining built-in locale in catalog order `→ Auto`.
    /// `dir` steps backward when negative. The legacy
    /// [`LanguageSetting`] stays a faithful projection of the cycle,
    /// so `Auto → English → Japanese → Auto` still takes three clicks.
    fn cycle_language(&mut self, dir: i32) {
        // Step 0 is the Auto stop; steps 1..=16 index ALL_BUILTIN_LOCALES.
        let step: i32 = match (self.settings.language, self.settings.locale) {
            (LanguageSetting::Auto, BuiltinLocale::EnUs) => 0,
            (_, locale) => {
                1 + ALL_BUILTIN_LOCALES
                    .iter()
                    .position(|&l| l == locale)
                    .unwrap_or(0) as i32
            }
        };
        let next = (step + dir).rem_euclid(1 + ALL_BUILTIN_LOCALES.len() as i32);
        if next == 0 {
            self.settings.language = LanguageSetting::Auto;
            self.settings.locale = BuiltinLocale::EnUs;
            if let Some(catalog) = &mut self.i18n_catalog {
                let _ = catalog.set_locale("en_us");
            }
        } else {
            self.set_locale(ALL_BUILTIN_LOCALES[(next - 1) as usize]);
        }
    }

    /// Resolve built-in UI text for the active locale, layered under
    /// any externally loaded Minecraft `.json` language pack.
    pub fn ui_text(&self, key: UiTextKey) -> &str {
        match &self.i18n_catalog {
            Some(catalog) => catalog.resolve_key(key),
            None => key.builtin_text(self.settings.locale),
        }
    }

    // -- settings session transaction ----------------------------------

    /// `true` when `key`'s setting differs from the session-open
    /// snapshot — the `●` dirty indicator shown beside the widget.
    pub fn is_setting_dirty(&self, key: SettingKey) -> bool {
        !self.settings_tx.field_equal(&self.settings, key)
    }

    /// `true` when anything in the session differs from the snapshot.
    pub fn is_session_dirty(&self) -> bool {
        self.settings != self.settings_tx.opened_snapshot
    }

    /// Begin (or restart) a settings session: snapshot the current
    /// settings and clear the bounded undo/redo history.
    fn begin_settings_session(&mut self) {
        self.settings_tx.opened_snapshot = self.settings.clone();
        self.settings_tx.undo_stack.clear();
        self.settings_tx.redo_stack.clear();
    }

    /// Record `prior` on the bounded undo stack and clear redo — every
    /// committed settings edit routes here.
    fn push_settings_undo(&mut self, prior: SettingsSnapshot) {
        let tx = &mut self.settings_tx;
        if tx.undo_stack.len() >= SETTINGS_SESSION_UNDO_CAP {
            tx.undo_stack.remove(0);
        }
        tx.undo_stack.push(prior);
        tx.redo_stack.clear();
    }

    /// Commit an externally driven settings edit through the same undo/redo
    /// history and change notification as the built-in settings widgets.
    pub fn edit_settings(&mut self, edit: impl FnOnce(&mut UiSettings)) -> Vec<UiAction> {
        let prior = self.settings.clone();
        edit(&mut self.settings);
        if self.settings == prior {
            return Vec::new();
        }
        self.push_settings_undo(prior);
        vec![UiAction::SettingsChanged]
    }

    /// Restore ONLY `key`'s setting to the session-open snapshot,
    /// pushing the prior full state onto the undo stack first.
    pub fn revert_single_setting(&mut self, key: SettingKey) -> Vec<UiAction> {
        if !self.is_setting_dirty(key) {
            return Vec::new();
        }
        let prior = self.settings.clone();
        self.settings_tx.restore_field(&mut self.settings, key);
        self.push_settings_undo(prior);
        vec![UiAction::SettingsChanged]
    }

    /// `Ctrl+Z` / Undo: pop one prior state (also reachable via the
    /// settings toolbar button).
    pub fn undo_settings(&mut self) -> Vec<UiAction> {
        match self.settings_tx.undo_stack.pop() {
            Some(prev) => {
                let current = std::mem::replace(&mut self.settings, prev);
                self.settings_tx.redo_stack.push(current);
                vec![UiAction::SettingsChanged]
            }
            None => Vec::new(),
        }
    }

    /// `Ctrl+Y` / Redo: re-apply the last undone state.
    pub fn redo_settings(&mut self) -> Vec<UiAction> {
        match self.settings_tx.redo_stack.pop() {
            Some(next) => {
                let current = std::mem::replace(&mut self.settings, next);
                self.settings_tx.undo_stack.push(current);
                vec![UiAction::SettingsChanged]
            }
            None => Vec::new(),
        }
    }

    /// Restore the whole session-open snapshot; the discarded state is
    /// itself one Undo away.
    pub fn discard_settings_session(&mut self) -> Vec<UiAction> {
        if self.settings == self.settings_tx.opened_snapshot {
            return Vec::new();
        }
        let prior = std::mem::replace(
            &mut self.settings,
            self.settings_tx.opened_snapshot.clone(),
        );
        self.push_settings_undo(prior);
        vec![UiAction::SettingsChanged]
    }

    /// Open the [`UiModal::ConfirmResetDefaults`] gate — never resets on
    /// one accidental click.
    pub fn request_reset_defaults(&mut self) -> Vec<UiAction> {
        self.modal = Some(UiModal::ConfirmResetDefaults);
        Vec::new()
    }

    /// `true` only when the recorded boot took at least 250 ms — fast
    /// boots go straight to Title with zero splash wait.
    pub fn should_show_live_loading_overlay(&self) -> bool {
        self.startup_timeline
            .as_ref()
            .is_some_and(StartupTimeline::should_show_live_overlay)
    }

    /// Lay out the current screen, cache the hit regions, and return the
    /// draw list for this frame.
    pub fn render(&mut self, width: u32, height: u32) -> crate::draw::UiDrawList {
        let mut list = crate::draw::UiDrawList::new();
        self.render_into(width, height, &mut list);
        list
    }

    /// Buffer-reusing [`render`](Self::render): `out` is cleared and
    /// refilled (its `Text` payload buffers are recycled, so steady-state
    /// frames emit no text allocations), and `self.hit_regions` is rebuilt
    /// in place keeping its capacity.
    pub fn render_into(&mut self, width: u32, height: u32, out: &mut crate::draw::UiDrawList) {
        self.render_w = width;
        self.render_h = height;
        // The embedded screens own their own language/scale mirrors so
        // they can also run standalone; when routed through `UiState`
        // this state is the source of truth.
        let lang = self.lang();
        self.mod_menu.lang = lang;
        self.mod_menu.ui_scale = self.ui_scale;
        self.recipe_browser.lang = lang;
        self.recipe_browser.ui_scale = self.ui_scale;
        let mut regions = std::mem::take(&mut self.hit_regions);
        layout::layout_screen_into(self, width, height, out, &mut regions);
        if self.screen == Screen::InGameHud {
            layout::layout_subtitle_hud(self, &self.hud.subtitle_hud, width, height, out);
            layout::layout_scoreboard_sidebar(self, width, height, out);
            layout::layout_title_overlay(self, width, height, out);
            layout::layout_looking_at(self, width, height, out);
        }
        self.hit_regions = regions;
        if let Some(i) = self.focus_index {
            if i >= self.hit_regions.len() {
                self.focus_index = None;
            }
        }
    }

    /// Push `screen` and navigate to it.
    pub fn navigate(&mut self, screen: Screen) {
        // Entering Settings from outside snapshots the session so dirty
        // tracking, per-item revert, and Discard measure from this entry.
        if matches!(screen, Screen::Settings(_)) && !matches!(self.screen, Screen::Settings(_)) {
            self.begin_settings_session();
        }
        if self.screen != screen {
            self.nav_stack.push(self.screen);
        }
        self.screen = screen;
        self.modal = None;
        self.focus_index = None;
    }

    /// Pop the nav stack; lands on Title when empty.
    pub fn back(&mut self) {
        self.screen = self.nav_stack.pop().unwrap_or(Screen::Title);
        self.modal = None;
        self.focus_index = None;
    }

    /// Abandon the stack and land on Title.
    pub fn go_title(&mut self) {
        self.nav_stack.clear();
        self.screen = Screen::Title;
        self.modal = None;
        self.focus_index = None;
    }

    /// Load a crash onto the recovery screen and navigate to it.
    pub fn show_crash(&mut self, ctx: CrashContext) {
        self.crash = Some(ctx);
        self.crash_sent = false;
        self.navigate(Screen::CrashRecovery);
    }

    /// Open the integrated Mods & Packs screen — the Title
    /// `"Mods & Packs"` button, the Settings `"Mods"` button, or a host
    /// hotkey. Pushes the nav stack so Back returns to the caller's
    /// screen; `lang`/`ui_scale` are copied across immediately (and kept
    /// in sync on every later render).
    pub fn open_mod_menu(&mut self) -> Vec<UiAction> {
        self.mod_menu.lang = self.lang();
        self.mod_menu.ui_scale = self.ui_scale;
        self.navigate(Screen::ModMenu);
        vec![UiAction::OpenScreen(Screen::ModMenu)]
    }

    /// Open the integrated recipe & resource browser — the Pause
    /// `"Recipe Browser"` button or a host hotkey (`R`/`U` over an item
    /// is a host-side binding). Pushes the nav stack so Back returns to
    /// the caller's screen.
    pub fn open_recipe_browser(&mut self) -> Vec<UiAction> {
        self.recipe_browser.lang = self.lang();
        self.recipe_browser.ui_scale = self.ui_scale;
        self.navigate(Screen::RecipeBrowser);
        vec![UiAction::OpenScreen(Screen::RecipeBrowser)]
    }

    /// The 4 KiB redacted crash report preview shown on
    /// [`Screen::CrashRecovery`] (empty without an active crash).
    pub fn crash_preview(&self) -> String {
        let Some(ctx) = &self.crash else {
            return String::new();
        };
        let md = ctx.markdown_summary(); // already redacted
        let mut end = md.len().min(CRASH_PREVIEW_MAX_BYTES);
        while end > 0 && !md.is_char_boundary(end) {
            end -= 1;
        }
        md[..end].to_string()
    }

    /// The fingerprint of the active crash (`""` without one).
    pub fn crash_fingerprint(&self) -> String {
        self.crash
            .as_ref()
            .map(CrashContext::fingerprint)
            .unwrap_or_default()
    }

    /// Focused widget id, if any.
    pub fn focused(&self) -> Option<WidgetId> {
        self.focus_index
            .and_then(|i| self.hit_regions.get(i))
            .map(|r| r.id)
    }

    /// `true` while the FPS custom field owns typing.
    pub fn fps_field_active(&self) -> bool {
        self.fps_field_focused
    }

    /// Current contents of the FPS custom field.
    pub fn fps_field_text(&self) -> &str {
        &self.fps_field_text
    }

    /// `true` after a rejected custom FPS entry.
    pub fn fps_field_error(&self) -> bool {
        self.fps_field_error
    }

    /// `true` while the join-code field owns typing.
    pub fn join_field_active(&self) -> bool {
        self.join_field_focused
    }

    /// The displayed invite code text, or `"-"` when none is active.
    pub fn invite_display(&self) -> String {
        self.active_invite
            .map(invite_code_display)
            .unwrap_or_else(|| "-".to_string())
    }

    // -- input ---------------------------------------------------------

    /// Route one input event; returns the actions the app must perform.
    pub fn handle_input(&mut self, ev: UiInputEvent) -> Vec<UiAction> {
        // Embedded screens own their hit-testing, search focus, and
        // scroll state — while one is up (and no modal blocks it) the
        // raw event is forwarded verbatim and its actions are translated
        // back into [`UiAction`]s.
        if self.modal.is_none() {
            match self.screen {
                Screen::ModMenu => return self.forward_to_mod_menu(ev),
                Screen::RecipeBrowser => return self.forward_to_recipe_browser(ev),
                _ => {}
            }
        }
        match ev {
            UiInputEvent::MouseMove { x, y } => {
                self.hover = self
                    .hit_regions
                    .iter()
                    .find(|r| r.contains(x, y))
                    .map(|r| r.id);
                Vec::new()
            }
            UiInputEvent::Click { x, y } => self.handle_click(x, y),
            UiInputEvent::KeyPressed(key) => self.handle_key(key),
            UiInputEvent::CharTyped(c) => self.handle_char(c),
            UiInputEvent::GamepadPressed(btn) => match btn {
                GamepadButton::DpadUp => self.handle_key(UiKey::Up),
                GamepadButton::DpadDown => self.handle_key(UiKey::Down),
                GamepadButton::DpadLeft => self.handle_key(UiKey::Left),
                GamepadButton::DpadRight => self.handle_key(UiKey::Right),
                GamepadButton::South => self.handle_key(UiKey::Enter),
                GamepadButton::East | GamepadButton::Start => self.handle_key(UiKey::Escape),
            },
        }
    }

    /// Forward `ev` into [`crate::mod_menu::ModMenuState`] and map its
    /// actions back onto [`UiAction`]. Clicks on native chrome (any
    /// non-`ModMenu` region) are routed by [`handle_click`](Self::handle_click)
    /// instead — everything else reaches the sub-screen untouched.
    fn forward_to_mod_menu(&mut self, ev: UiInputEvent) -> Vec<UiAction> {
        match ev {
            UiInputEvent::Click { x, y } => {
                if let Some(region) = self
                    .hit_regions
                    .iter()
                    .find(|r| r.contains(x, y))
                    .copied()
                {
                    if !matches!(region.id, WidgetId::ModMenu(_)) {
                        return self.handle_click(x, y);
                    }
                }
            }
            UiInputEvent::MouseMove { x, y } => {
                self.hover = self
                    .hit_regions
                    .iter()
                    .find(|r| r.contains(x, y))
                    .map(|r| r.id);
            }
            _ => {}
        }
        let acts = self.mod_menu.handle_input(ev, self.render_w, self.render_h);
        let mut out = Vec::with_capacity(acts.len());
        for action in acts {
            match action {
                crate::mod_menu::ModMenuAction::Close => {
                    self.back();
                    out.push(UiAction::Back);
                }
                crate::mod_menu::ModMenuAction::OpenUrl(url) => {
                    out.push(UiAction::OpenUrl(url));
                }
                other => out.push(UiAction::ModMenu(other)),
            }
        }
        out
    }

    /// Recipe-browser counterpart of [`forward_to_mod_menu`](Self::forward_to_mod_menu).
    fn forward_to_recipe_browser(&mut self, ev: UiInputEvent) -> Vec<UiAction> {
        match ev {
            UiInputEvent::Click { x, y } => {
                if let Some(region) = self
                    .hit_regions
                    .iter()
                    .find(|r| r.contains(x, y))
                    .copied()
                {
                    if !matches!(region.id, WidgetId::Recipe(_)) {
                        return self.handle_click(x, y);
                    }
                }
            }
            UiInputEvent::MouseMove { x, y } => {
                self.hover = self
                    .hit_regions
                    .iter()
                    .find(|r| r.contains(x, y))
                    .map(|r| r.id);
            }
            _ => {}
        }
        let acts = self
            .recipe_browser
            .handle_input(ev, self.render_w, self.render_h);
        let mut out = Vec::with_capacity(acts.len());
        for action in acts {
            match action {
                crate::recipe_browser::RecipeBrowserAction::Close => {
                    self.back();
                    out.push(UiAction::Back);
                }
                other => out.push(UiAction::RecipeBrowser(other)),
            }
        }
        out
    }

    fn handle_click(&mut self, x: i32, y: i32) -> Vec<UiAction> {
        let Some(region) = self.hit_regions.iter().find(|r| r.contains(x, y)).copied() else {
            // Click on empty space releases any text field.
            self.unfocus_fields();
            return Vec::new();
        };
        self.focus_index = self.hit_regions.iter().position(|r| r.id == region.id);
        if !region.id.is_text_field() {
            self.unfocus_fields();
        }
        self.activate(region.id, x, y, region.rect)
    }

    fn handle_key(&mut self, key: UiKey) -> Vec<UiAction> {
        // While a text field owns typing, most keys feed the field.
        if self.fps_field_focused || self.join_field_focused {
            return self.handle_key_in_field(key);
        }
        match key {
            UiKey::Escape => self.escape(),
            UiKey::Tab => {
                if self.screen == Screen::InGameHud {
                    self.hud.show_player_list = !self.hud.show_player_list;
                    Vec::new()
                } else {
                    self.focus_step(1);
                    Vec::new()
                }
            }
            UiKey::Up => {
                self.focus_step(-1);
                Vec::new()
            }
            UiKey::Down => {
                self.focus_step(1);
                Vec::new()
            }
            UiKey::Left => self.arrow_adjust(-1),
            UiKey::Right => self.arrow_adjust(1),
            UiKey::Enter | UiKey::Space => self.activate_focused(),
            UiKey::Backspace => Vec::new(),
        }
    }

    fn handle_key_in_field(&mut self, key: UiKey) -> Vec<UiAction> {
        match key {
            UiKey::Enter => self.commit_fields(),
            UiKey::Escape => {
                self.unfocus_fields();
                Vec::new()
            }
            UiKey::Backspace => {
                if self.fps_field_focused {
                    self.fps_field_text.pop();
                } else if self.join_field_focused {
                    self.join_code_text.pop();
                    self.join_error = false;
                }
                Vec::new()
            }
            UiKey::Tab => {
                self.unfocus_fields();
                self.focus_step(1);
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    fn handle_char(&mut self, c: char) -> Vec<UiAction> {
        // Ctrl+Z / Ctrl+Y arrive as control characters when no text
        // field owns typing: session undo/redo on the settings screen.
        if self.modal.is_none()
            && !self.fps_field_focused
            && !self.join_field_focused
            && matches!(self.screen, Screen::Settings(_))
        {
            match c {
                '\u{1a}' => return self.undo_settings(),
                '\u{19}' => return self.redo_settings(),
                _ => {}
            }
        }
        if self.fps_field_focused {
            if c.is_ascii_digit() && self.fps_field_text.len() < FPS_FIELD_MAX_LEN {
                self.fps_field_text.push(c);
                self.fps_field_error = false;
            }
        } else if self.join_field_focused
            && (c.is_ascii_alphanumeric() || c == '-')
            && self.join_code_text.len() < JOIN_FIELD_MAX_LEN
        {
            self.join_code_text.push(c.to_ascii_uppercase());
            self.join_error = false;
        }
        Vec::new()
    }

    /// Commit whichever text field is focused; FPS validates `0` or
    /// `10..=2000`, join validates the `MEGU-XXXX-XXXX` shape.
    fn commit_fields(&mut self) -> Vec<UiAction> {
        if self.fps_field_focused {
            match self.fps_field_text.parse::<u32>() {
                Ok(v) if v == 0 || (CUSTOM_FPS_MIN..=CUSTOM_FPS_MAX).contains(&v) => {
                    let prior = self.settings.clone();
                    self.settings.video.fps_cap = v;
                    self.settings.video.preset = VideoPreset::Custom;
                    self.push_settings_undo(prior);
                    self.unfocus_fields();
                    vec![UiAction::SettingsChanged]
                }
                _ => {
                    self.fps_field_error = true;
                    Vec::new()
                }
            }
        } else if self.join_field_focused {
            self.unfocus_fields();
            self.try_join()
        } else {
            Vec::new()
        }
    }

    fn try_join(&mut self) -> Vec<UiAction> {
        match invite_code_parse(&self.join_code_text) {
            Some(code) => {
                self.join_error = false;
                vec![UiAction::JoinWithCode(code)]
            }
            None => {
                self.join_error = true;
                Vec::new()
            }
        }
    }

    fn unfocus_fields(&mut self) {
        self.fps_field_focused = false;
        self.join_field_focused = false;
    }

    fn escape(&mut self) -> Vec<UiAction> {
        if self.modal.take().is_some() {
            return Vec::new(); // modal dismissed only
        }
        match self.screen {
            Screen::Title | Screen::CrashRecovery => Vec::new(),
            Screen::InGameHud => {
                self.navigate(Screen::Pause);
                vec![UiAction::OpenScreen(Screen::Pause)]
            }
            Screen::Pause => {
                self.back();
                vec![UiAction::Back]
            }
            _ => {
                self.back();
                vec![UiAction::Back]
            }
        }
    }

    fn focus_step(&mut self, dir: i32) {
        if self.hit_regions.is_empty() {
            self.focus_index = None;
            return;
        }
        let n = self.hit_regions.len() as i32;
        let i = self
            .focus_index
            .map(|i| (i as i32 + dir).rem_euclid(n))
            .unwrap_or(if dir >= 0 { 0 } else { n - 1 });
        self.focus_index = Some(i as usize);
    }

    fn arrow_adjust(&mut self, dir: i32) -> Vec<UiAction> {
        match self.focused() {
            Some(id) if id.adjustable() => self.adjust(id, dir),
            Some(_) | None => {
                self.focus_step(dir);
                Vec::new()
            }
        }
    }

    fn activate_focused(&mut self) -> Vec<UiAction> {
        let Some(i) = self.focus_index else {
            return Vec::new();
        };
        let Some(region) = self.hit_regions.get(i).copied() else {
            return Vec::new();
        };
        self.activate(region.id, region.rect.x, region.rect.y, region.rect)
    }

    /// Click-activation dispatch: `x`/`y` matter for sliders.
    fn activate(&mut self, id: WidgetId, x: i32, _y: i32, rect: UiRect) -> Vec<UiAction> {
        match id {
            WidgetId::TitleSingleplayer => vec![UiAction::StartSingleplayer],
            WidgetId::TitleMultiplayer => vec![UiAction::StartMultiplayer],
            WidgetId::TitleFriends => {
                self.navigate(Screen::Friends);
                vec![UiAction::OpenScreen(Screen::Friends)]
            }
            WidgetId::TitleMods => self.open_mod_menu(),
            WidgetId::TitleSettings => {
                let s = Screen::Settings(SettingsTab::Video);
                self.navigate(s);
                vec![UiAction::OpenScreen(s)]
            }
            WidgetId::TitleQuit => vec![UiAction::QuitGame],
            WidgetId::WebsiteLink => {
                if self.website_url.is_empty() {
                    Vec::new()
                } else {
                    vec![UiAction::OpenUrl(self.website_url.clone())]
                }
            }
            WidgetId::SupportLink => {
                if (self.screen == Screen::Title && !self.settings.qol.show_support_links)
                    || self.support_url.is_empty()
                {
                    Vec::new()
                } else {
                    vec![UiAction::OpenUrl(self.support_url.clone())]
                }
            }
            WidgetId::ContributeLink => {
                if (self.screen == Screen::Title && !self.settings.qol.show_support_links)
                    || self.contribute_url.is_empty()
                {
                    Vec::new()
                } else {
                    vec![UiAction::OpenUrl(self.contribute_url.clone())]
                }
            }
            WidgetId::ReportLink | WidgetId::PauseReportBug => {
                if self.report_url.is_empty() {
                    Vec::new()
                } else {
                    vec![UiAction::OpenUrl(self.report_url.clone())]
                }
            }

            WidgetId::SettingsDone => {
                self.back();
                vec![UiAction::Back]
            }
            WidgetId::SettingsAboutMegu => {
                self.navigate(Screen::AboutMegu);
                vec![UiAction::OpenScreen(Screen::AboutMegu)]
            }
            WidgetId::SettingsMods => self.open_mod_menu(),
            WidgetId::AboutBack => {
                self.back();
                vec![UiAction::Back]
            }
            WidgetId::TabVideo => self.select_tab(SettingsTab::Video),
            WidgetId::TabAudio => self.select_tab(SettingsTab::Audio),
            WidgetId::TabShaders => self.select_tab(SettingsTab::Shaders),
            WidgetId::TabQol => self.select_tab(SettingsTab::Qol),
            WidgetId::TabFairPlay => self.select_tab(SettingsTab::FairPlay),
            WidgetId::TabLanguage => self.select_tab(SettingsTab::Language),
            WidgetId::ShadersOpenOptions => vec![UiAction::OpenShaderOptions],

            WidgetId::SettingsUndo => self.undo_settings(),
            WidgetId::SettingsRedo => self.redo_settings(),
            WidgetId::SettingsDiscard => self.discard_settings_session(),
            WidgetId::SettingsResetDefaults => self.request_reset_defaults(),
            WidgetId::LanguageDetectOs => {
                let prior = self.settings.clone();
                self.detect_and_apply_os_locale();
                if self.settings != prior {
                    self.push_settings_undo(prior);
                    vec![UiAction::SettingsChanged]
                } else {
                    Vec::new()
                }
            }
            WidgetId::BootBadge => {
                if self.startup_timeline.is_some() {
                    self.modal = Some(UiModal::StartupTimeline);
                }
                Vec::new()
            }

            WidgetId::VideoFpsSlider => {
                // Map click x to the nearest slider stop.
                let frac = ((x - rect.x) as f32 / rect.w.max(1) as f32).clamp(0.0, 1.0);
                let idx = ((frac * (FPS_CAP_STEPS.len() - 1) as f32).round() as usize)
                    .min(FPS_CAP_STEPS.len() - 1);
                self.set_fps_step(idx)
            }
            WidgetId::VideoFpsField => {
                self.fps_field_focused = true;
                self.join_field_focused = false;
                self.fps_field_text.clear();
                self.fps_field_error = false;
                Vec::new()
            }
            WidgetId::FriendsJoinField => {
                self.join_field_focused = true;
                self.fps_field_focused = false;
                Vec::new()
            }
            WidgetId::FriendsJoin => self.try_join(),

            WidgetId::FriendsGenerate => {
                let v = next_invite_code(&mut self.invite_counter);
                let v = v ^ self.invite_seed.wrapping_add(0x5f5f_5f5f_5f5f);
                self.active_invite = Some(v & 0xff_ffff_ffff);
                vec![UiAction::GenerateInvite]
            }
            WidgetId::FriendsCopy => match self.active_invite {
                Some(code) => vec![UiAction::CopyInvite(invite_code_display(code))],
                None => Vec::new(),
            },
            WidgetId::FriendsRevoke => {
                if self.active_invite.take().is_some() {
                    vec![UiAction::RevokeInvite]
                } else {
                    Vec::new()
                }
            }
            WidgetId::FriendsBack => {
                self.back();
                vec![UiAction::Back]
            }

            WidgetId::ModSyncAccept => {
                self.mod_sync_decision = Some(true);
                let entries = std::mem::take(&mut self.pending_mod_sync);
                vec![UiAction::ApplyModSync(entries)]
            }
            WidgetId::ModSyncDecline => {
                self.mod_sync_decision = Some(false);
                vec![UiAction::DeclineModSync]
            }
            WidgetId::ModSyncDownloadMissing => {
                if self.missing_mod_deps.is_empty() {
                    Vec::new()
                } else {
                    vec![UiAction::DownloadMissingFromModrinth(
                        self.missing_mod_deps.clone(),
                    )]
                }
            }

            WidgetId::PauseBackToGame => {
                self.back();
                vec![UiAction::Back]
            }
            WidgetId::PauseSettings => {
                let s = Screen::Settings(SettingsTab::Video);
                self.navigate(s);
                vec![UiAction::OpenScreen(s)]
            }
            WidgetId::PauseStatistics => vec![UiAction::ShowStatistics],
            WidgetId::PauseRecipeBrowser => self.open_recipe_browser(),
            WidgetId::PauseInvite => {
                self.navigate(Screen::Friends);
                vec![UiAction::OpenScreen(Screen::Friends)]
            }
            WidgetId::PauseSaveQuit => {
                self.go_title();
                vec![UiAction::OpenScreen(Screen::Title)]
            }

            WidgetId::CrashSendReport => {
                if self.crash_sent || self.crash.is_none() {
                    Vec::new()
                } else {
                    self.modal = Some(UiModal::ConfirmSendCrashReport);
                    Vec::new()
                }
            }
            WidgetId::CrashReturnTitle => {
                self.go_title();
                vec![UiAction::OpenScreen(Screen::Title)]
            }
            WidgetId::ModalOk => match self.modal {
                Some(UiModal::ConfirmSendCrashReport) => {
                    self.modal = None;
                    self.crash_sent = true;
                    vec![UiAction::SubmitCrashReport]
                }
                Some(UiModal::ConfirmResetDefaults) => {
                    self.modal = None;
                    // Even a confirmed reset stays 1-click Undo-recoverable.
                    let prior = std::mem::take(&mut self.settings);
                    self.push_settings_undo(prior);
                    vec![UiAction::SettingsChanged]
                }
                Some(UiModal::StartupTimeline) | None => {
                    self.modal = None;
                    Vec::new()
                }
            },
            WidgetId::ModalCancel => {
                self.modal = None;
                Vec::new()
            }

            WidgetId::RecipeBrowserDone => {
                self.back();
                vec![UiAction::Back]
            }
            WidgetId::CraftConflictNext => {
                let overlay = &mut self.hud.crafting_conflict;
                if overlay.is_active() {
                    overlay.selected_index =
                        (overlay.selected_index + 1) % overlay.total_candidates;
                    vec![UiAction::CycleCraftingConflict]
                } else {
                    Vec::new()
                }
            }

            // Adjustable widgets activate as a "+1" cycle/slider click.
            _ => self.adjust(id, 1),
        }
    }

    /// Left/Right (or click on cyclers/toggles): `dir` = -1 or +1.
    fn adjust(&mut self, id: WidgetId, dir: i32) -> Vec<UiAction> {
        // Language cycling touches `language` + `locale` together, so it
        // can't live inside the field-split match below.
        if id == WidgetId::LanguageChoice {
            let prior = self.settings.clone();
            self.cycle_language(dir);
            if self.settings != prior {
                self.push_settings_undo(prior);
                return vec![UiAction::SettingsChanged];
            }
            return Vec::new();
        }
        let prior = self.settings.clone();
        let v = &mut self.settings.video;
        let a = &mut self.settings.audio;
        let q = &mut self.settings.qol;
        let f = &mut self.settings.fair_play;
        let s = &mut self.settings.visual;
        let changed = match id {
            WidgetId::VideoPreset => {
                v.preset = v.preset.next();
                true
            }
            WidgetId::VideoViewDistance => {
                let d = v.view_distance as i32 + dir;
                v.view_distance = d.clamp(MIN_VIEW_DISTANCE as i32, MAX_VIEW_DISTANCE as i32) as u8;
                v.preset = VideoPreset::Custom;
                true
            }
            WidgetId::VideoLod => {
                v.lod = v.lod.next();
                v.preset = VideoPreset::Custom;
                true
            }
            WidgetId::VideoGpuBackend => {
                v.gpu_backend = v.gpu_backend.next();
                v.preset = VideoPreset::Custom;
                true
            }
            WidgetId::VideoFpsSlider => {
                let i = v.fps_step_index() as i32;
                let n = FPS_CAP_STEPS.len() as i32;
                let next = (i + dir).rem_euclid(n) as usize;
                return self.set_fps_step(next);
            }
            WidgetId::VideoBackgroundFps => {
                v.background_fps = if v.background_fps == 15 { 0 } else { 15 };
                v.preset = VideoPreset::Custom;
                true
            }
            WidgetId::VideoVsync => {
                v.vsync = !v.vsync;
                v.preset = VideoPreset::Custom;
                true
            }
            WidgetId::VideoSmoothLighting => {
                v.smooth_lighting = !v.smooth_lighting;
                v.preset = VideoPreset::Custom;
                true
            }
            WidgetId::ShadersEnabled => {
                self.settings.shader_quick.shaders_enabled =
                    !self.settings.shader_quick.shaders_enabled;
                true
            }
            WidgetId::VisualPreset => {
                s.preset = s.preset.step(dir);
                true
            }
            WidgetId::VisualBlocks3d => {
                s.blocks_and_skins_3d = !s.blocks_and_skins_3d;
                true
            }
            WidgetId::VisualWorldEffects => {
                s.world_effects = !s.world_effects;
                true
            }
            WidgetId::VisualCameraMotion => {
                s.camera_micro_motion = !s.camera_micro_motion;
                true
            }
            WidgetId::AudioSpatial3d => {
                s.spatial_audio_3d = !s.spatial_audio_3d;
                true
            }
            WidgetId::AudioMaster => {
                a.master = clamp_i(a.master as i32 + 5 * dir, 0, 100);
                true
            }
            WidgetId::AudioMusic => {
                a.music = clamp_i(a.music as i32 + 5 * dir, 0, 100);
                true
            }
            WidgetId::AudioBlocks => {
                a.blocks = clamp_i(a.blocks as i32 + 5 * dir, 0, 100);
                true
            }
            WidgetId::AudioHostile => {
                a.hostile = clamp_i(a.hostile as i32 + 5 * dir, 0, 100);
                true
            }
            WidgetId::AudioFriendly => {
                a.friendly = clamp_i(a.friendly as i32 + 5 * dir, 0, 100);
                true
            }
            WidgetId::AudioMaxVolumeCap => {
                a.max_volume_cap = clamp_i(a.max_volume_cap as i32 + 5 * dir, 0, 100);
                true
            }
            WidgetId::AudioMaxConcurrent => {
                a.max_concurrent_same_sound = clamp_i(
                    a.max_concurrent_same_sound as i32 + dir,
                    MIN_CONCURRENT_SAME_SOUND as i32,
                    MAX_CONCURRENT_SAME_SOUND as i32,
                );
                true
            }
            WidgetId::QolSmoothZoom => {
                q.smooth_zoom = !q.smooth_zoom;
                true
            }
            WidgetId::QolToolWarn => {
                q.tool_warn_percent = clamp_i(q.tool_warn_percent as i32 + 5 * dir, 0, 100);
                true
            }
            WidgetId::QolPreventBreak => {
                q.prevent_tool_break = !q.prevent_tool_break;
                true
            }
            WidgetId::QolChatHistory => {
                let step = if dir >= 0 { 100 } else { -100 };
                let v0 = q.chat_history_lines as i64 + step;
                q.chat_history_lines =
                    (v0.clamp(MIN_CHAT_HISTORY as i64, MAX_CHAT_HISTORY as i64)) as usize;
                true
            }
            WidgetId::QolRetainChat => {
                q.retain_chat_on_reconnect = !q.retain_chat_on_reconnect;
                true
            }
            WidgetId::QolCrashMode => {
                q.crash_report_mode = q.crash_report_mode.next();
                true
            }
            WidgetId::QolShowSupportLinks => {
                q.show_support_links = !q.show_support_links;
                true
            }
            WidgetId::QolMiniHud => {
                q.cycle_mini_hud();
                true
            }
            WidgetId::QolSubtitles => {
                q.show_subtitles = !q.show_subtitles;
                true
            }
            WidgetId::FpMinimapMode => {
                f.minimap_mode = f.minimap_mode.next();
                true
            }
            WidgetId::FpCaveMap => {
                f.allow_cave_map = !f.allow_cave_map;
                true
            }
            WidgetId::FpMobRadar => {
                f.allow_mob_radar = !f.allow_mob_radar;
                true
            }
            WidgetId::FpRadarScope => {
                f.player_radar_scope = f.player_radar_scope.next();
                true
            }
            WidgetId::FpWaypointTeleport => {
                f.allow_waypoint_teleport = !f.allow_waypoint_teleport;
                true
            }
            _ => false,
        };
        if changed {
            self.push_settings_undo(prior);
            vec![UiAction::SettingsChanged]
        } else {
            Vec::new()
        }
    }

    fn set_fps_step(&mut self, idx: usize) -> Vec<UiAction> {
        let idx = idx.min(FPS_CAP_STEPS.len() - 1);
        if self.settings.video.fps_cap == FPS_CAP_STEPS[idx]
            && self.settings.video.preset == VideoPreset::Custom
        {
            return Vec::new();
        }
        let prior = self.settings.clone();
        self.settings.video.fps_cap = FPS_CAP_STEPS[idx];
        self.settings.video.preset = VideoPreset::Custom;
        self.push_settings_undo(prior);
        vec![UiAction::SettingsChanged]
    }

    fn select_tab(&mut self, tab: SettingsTab) -> Vec<UiAction> {
        self.screen = Screen::Settings(tab);
        self.focus_index = None;
        Vec::new()
    }
}

fn clamp_i(v: i32, lo: i32, hi: i32) -> u8 {
    v.clamp(lo, hi) as u8
}

/// Re-export for tests/embedders: privacy redaction of free-form text.
pub use megu_core::crash_report::redact_sensitive as ui_redact_sensitive;
