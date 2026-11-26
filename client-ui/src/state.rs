// SPDX-License-Identifier: LGPL-3.0-or-later
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
use megu_core::i18n::{ALL_BUILTIN_LOCALES, BuiltinLocale, I18nCatalog, UiTextKey};
use megu_core::sys_metrics::{CpuSampler, sample_host_metrics};

/// Neutral public name for the core compact telemetry overlay configuration.
pub type CompactDebugHudConfig = megu_core::sys_metrics::MiniHudConfig;
/// Neutral public name for the core compact telemetry overlay frame data.
pub type CompactDebugHudData = megu_core::sys_metrics::MiniHudData;
/// Neutral public name for the core compact telemetry overlay anchor.
pub type CompactDebugHudAnchor = megu_core::sys_metrics::MiniHudAnchor;
use std::collections::HashMap;

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
    /// Rebindable gameplay controls.
    Controls,
}

impl SettingsTab {
    /// All tabs, left to right.
    pub const ALL: [Self; 7] = [
        Self::Video,
        Self::Audio,
        Self::Shaders,
        Self::Qol,
        Self::FairPlay,
        Self::Language,
        Self::Controls,
    ];

    pub fn label(self, lang: UiLang) -> &'static str {
        match self {
            Self::Video => tr(lang, "Video", "ビデオ"),
            Self::Audio => tr(lang, "Audio", "オーディオ"),
            Self::Shaders => tr(lang, "Shaders", "シェーダー"),
            Self::Qol => tr(lang, "QoL", "QoL"),
            Self::FairPlay => tr(lang, "Fair-Play", "フェアプレイ"),
            Self::Language => tr(lang, "Language", "言語"),
            Self::Controls => tr(lang, "Controls", "操作設定"),
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
    /// Singleplayer world select + "Create New World" — reached from the
    /// Title `Singleplayer` button; the host fills
    /// [`UiState::singleplayer`] from `<gameDir>/saves/`.
    Singleplayer,
    /// Multiplayer server list + direct-connect — reached from the Title
    /// `Multiplayer` button; the host fills [`UiState::multiplayer`] from
    /// its server-list store (`<gameDir>/megu_servers.toml`).
    Multiplayer,
    /// Tabbed settings; [`SettingsTab`] picks the active page.
    Settings(SettingsTab),
    /// Megu details, license, and configured project links.
    AboutMegu,
    /// Host invite codes + guest join field.
    Friends,
    /// Explicit consent gate before a server's mod list is applied.
    ModSyncConsent,
    /// Per-server missing/version-mismatched mod diagnosis and download.
    ModSyncPrompt,
    /// Integrated ModManager-style Mods & Packs manager — draw and input are
    /// delegated to [`UiState::mod_manager`].
    ModManager,
    /// recipe and resource browser — draw and input are delegated
    /// to [`UiState::recipe_browser`].
    RecipeBrowser,
    /// In-game HUD (hotbar, vitals, bossbars, chat, tab list).
    InGameHud,
    /// Death & respawn screen (vanilla `DeathScreen`): driven by
    /// [`UiState::death`], entered via [`UiState::show_death`], left via
    /// [`UiAction::PerformRespawn`] / [`UiAction::LeaveWorld`].
    Death,
    /// Kicked/dropped connection screen (vanilla `DisconnectedScreen`):
    /// driven by [`UiState::disconnected`], entered via
    /// [`UiState::show_disconnected`], left via the back button
    /// ([`WidgetId::DisconnectedBack`]) or Escape.
    Disconnected,
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

    pub const fn label(self, lang: UiLang) -> &'static str {
        match (self, lang) {
            (Self::Fast, UiLang::En) => "Fast",
            (Self::Balanced, UiLang::En) => "Balanced",
            (Self::High, UiLang::En) => "High",
            (Self::Ultra, UiLang::En) => "Ultra",
            (Self::Custom, UiLang::En) => "Custom",
            (Self::Fast, UiLang::Ja) => "高速",
            (Self::Balanced, UiLang::Ja) => "バランス",
            (Self::High, UiLang::Ja) => "高",
            (Self::Ultra, UiLang::Ja) => "最高",
            (Self::Custom, UiLang::Ja) => "カスタム",
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

    pub const fn label(self, lang: UiLang) -> &'static str {
        match (self, lang) {
            (Self::Off, UiLang::En) => "Off",
            (Self::Medium, UiLang::En) => "Medium (64c)",
            (Self::Far, UiLang::En) => "Far (128c)",
            (Self::Off, UiLang::Ja) => "オフ",
            (Self::Medium, UiLang::Ja) => "中 (64c)",
            (Self::Far, UiLang::Ja) => "遠 (128c)",
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

    pub const fn label(self, lang: UiLang) -> &'static str {
        match (self, lang) {
            (Self::Auto, UiLang::En) => "Auto",
            (Self::MeshShader, UiLang::En) => "Mesh Shader",
            (Self::ComputeIndirect, UiLang::En) => "Compute",
            (Self::Classic, UiLang::En) => "Classic",
            (Self::ClassicLowSpec, UiLang::En) => "Low-Spec",
            (Self::Auto, UiLang::Ja) => "自動",
            (Self::MeshShader, UiLang::Ja) => "メッシュ",
            (Self::ComputeIndirect, UiLang::Ja) => "コンピュート",
            (Self::Classic, UiLang::Ja) => "クラシック",
            (Self::ClassicLowSpec, UiLang::Ja) => "低スペック",
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
/// Host-RAM cache-budget slider step (MiB). `0` on the setting is `Auto`;
/// every other stop adds one step up to the detected host total — there
/// is deliberately no artificial ceiling below it.
pub const RAM_BUDGET_STEP_MIB: u32 = 512;
/// Fallback total host RAM (MiB) when `megu_core::sys_metrics` cannot
/// detect it (tests, unknown platforms).
pub const DEFAULT_HOST_TOTAL_RAM_MIB: u32 = 8192;

/// Total host RAM in MiB via `megu_core::sys_metrics`, falling back to
/// [`DEFAULT_HOST_TOTAL_RAM_MIB`] when the platform probe reports
/// nothing. The probe is fail-safe: it never panics and simply yields
/// `None` fields on unsupported platforms.
pub fn detect_host_total_ram_mib() -> u32 {
    let mut probe = CpuSampler::new();
    sample_host_metrics(&mut probe)
        .mem_total_mib
        .and_then(|mib| u32::try_from(mib).ok())
        .filter(|&mib| mib > 0)
        .unwrap_or(DEFAULT_HOST_TOTAL_RAM_MIB)
}

/// Cloud draw mode (mirrors `ClientSettings.clouds` / vanilla
/// `options.txt` `clouds`): `Off` draws nothing, `Fast` keeps the single
/// vanilla deck, `Fancy` allows the richer layered decks under the Megu
/// visual styles.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CloudSetting {
    /// No cloud decks.
    Off,
    /// Single vanilla cubic deck.
    Fast,
    /// Full deck detail (default).
    #[default]
    Fancy,
}

impl CloudSetting {
    /// All values in cycle order.
    pub const ALL: [Self; 3] = [Self::Off, Self::Fast, Self::Fancy];

    pub fn next(self) -> Self {
        let i = Self::ALL.iter().position(|c| *c == self).unwrap_or(0);
        Self::ALL[(i + 1) % Self::ALL.len()]
    }

    pub fn label(self, lang: UiLang) -> &'static str {
        match (self, lang) {
            (Self::Off, UiLang::En) => "Off",
            (Self::Off, UiLang::Ja) => "オフ",
            (Self::Fast, UiLang::En) => "Fast",
            (Self::Fast, UiLang::Ja) => "高速",
            (Self::Fancy, UiLang::En) => "Fancy",
            (Self::Fancy, UiLang::Ja) => "描画優先",
        }
    }
}

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
    /// Cloud deck detail (`Off`/`Fast`/`Fancy`, default `Fancy`).
    pub clouds: CloudSetting,
    /// Entity contact shadows + realistic shadow rendering switch
    /// (`Shadows`, default `true`; the `Fast` preset drops it).
    pub entity_shadows: bool,
    /// Eye-comfort exposure limiter (soft glow instead of glare on
    /// snow/sand/stacked torches). Default `true`.
    pub comfort_exposure: bool,
    /// Dynamic colored lights from Vanilla + Redstone + modded
    /// blocks/items/mobs. Default `true`.
    pub dynamic_lights: bool,
    /// 3D & 2D enchanted item/armor shimmer. Default `true`.
    pub enchantment_glint: bool,
    /// Soft indirect bounce & hemisphere reflection so shadowed surfaces
    /// never drop to pitch black. Default `true`.
    pub indirect_light_bounce: bool,
    /// Opt-in realistic shadow-light gameplay for Singleplayer / Friends
    /// Host worlds (mob spawning, undead daylight burn, crop growth,
    /// dynamic-light spawn suppression). Remote servers keep their host
    /// settings. Default `false`.
    pub directional_shadow_gameplay: bool,
    /// Opt-in O(1) unloaded-chunk catch-up for Singleplayer / Friends
    /// Host worlds (furnaces, crops, headless factories tick while their
    /// chunk stays unloaded). Remote servers keep their host settings.
    /// Default `false`.
    pub lazy_catchup_gameplay: bool,
    /// Free host-RAM cache budget in MiB (`ram_budget_mib`): `0` = Auto
    /// lets the runtime pick a sane share of the detected host RAM;
    /// `> 0` caps how much RAM the chunk/asset caches may hold. The live
    /// OOM guard trims distant caches early whenever host available RAM
    /// drops low, so a high cap can never get the process OOM-killed —
    /// the budget never pre-allocates anything.
    pub ram_budget_mib: u32,
    /// Detected total host RAM in MiB — populated from
    /// [`detect_host_total_ram_mib`] ([`DEFAULT_HOST_TOTAL_RAM_MIB`]
    /// fallback). Bounds the [`Self::ram_budget_mib`] slider; not a
    /// persisted setting.
    pub host_total_ram_mib: u32,
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
            clouds: CloudSetting::Fancy,
            entity_shadows: true,
            comfort_exposure: true,
            dynamic_lights: true,
            enchantment_glint: true,
            indirect_light_bounce: true,
            directional_shadow_gameplay: false,
            lazy_catchup_gameplay: false,
            ram_budget_mib: 0,
            host_total_ram_mib: detect_host_total_ram_mib(),
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

    /// Display string for the current cap (`"Unlimited"`/`"無制限"` for `0`).
    pub fn fps_cap_label(&self, lang: UiLang) -> String {
        if self.fps_cap == 0 {
            tr(lang, "Unlimited", "無制限").to_string()
        } else {
            self.fps_cap.to_string()
        }
    }

    /// Slider fraction `0.0..=1.0` for [`Self::ram_budget_mib`]: `0`
    /// (Auto) sits at the left edge, the detected host total at `1.0`.
    pub fn ram_budget_frac(&self) -> f32 {
        let host = self.host_total_ram_mib.max(1) as f32;
        (self.ram_budget_mib as f32 / host).clamp(0.0, 1.0)
    }

    /// Snap a slider fraction `0.0..=1.0` onto the
    /// [`RAM_BUDGET_STEP_MIB`] stop grid (`0` stays Auto); the result is
    /// clamped to `0..=host_total_ram_mib`.
    pub fn set_ram_budget_frac(&mut self, frac: f32) {
        let host = self.host_total_ram_mib.max(1);
        let raw = (frac.clamp(0.0, 1.0) * host as f32).round() as u32;
        let steps = (f64::from(raw) / f64::from(RAM_BUDGET_STEP_MIB)).round() as u32;
        self.ram_budget_mib = steps
            .saturating_mul(RAM_BUDGET_STEP_MIB)
            .min(host);
    }

    /// Arrow-key/click step for [`Self::ram_budget_mib`]: `dir` moves one
    /// [`RAM_BUDGET_STEP_MIB`] stop; `0` (Auto) is the bottom stop and
    /// the detected host total the top one.
    pub fn ram_budget_step(&self, dir: i32) -> u32 {
        let host = self.host_total_ram_mib.max(1);
        let max_index = i64::from(host.div_ceil(RAM_BUDGET_STEP_MIB));
        let index = if self.ram_budget_mib == 0 {
            0
        } else {
            i64::from(self.ram_budget_mib.div_ceil(RAM_BUDGET_STEP_MIB))
        };
        let next = (index + i64::from(dir)).clamp(0, max_index);
        if next <= 0 {
            0
        } else {
            (next as u32)
                .saturating_mul(RAM_BUDGET_STEP_MIB)
                .min(host)
        }
    }

    /// Human-readable budget readout: `Auto` at `0`, otherwise the size
    /// in GB (MiB below 1 GiB), suffixed with an OS-RAM caution only
    /// when the budget exceeds 80% of the detected host total.
    pub fn ram_budget_label(&self, lang: UiLang) -> String {
        let mib = self.ram_budget_mib;
        if mib == 0 {
            return tr(lang, "Auto", "自動（おすすめ）").to_string();
        }
        let size = if mib < 1024 {
            format!("{mib} MiB")
        } else {
            format!("{:.1} GB", f64::from(mib) / 1024.0)
        };
        if u64::from(mib) * 100 > u64::from(self.host_total_ram_mib.max(1)) * 80 {
            format!("{size}{}", tr(lang, " (! OS RAM)", " (! OS RAM不足)"))
        } else {
            size
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
            master: 50,
            music: 45,
            blocks: 100,
            hostile: 100,
            friendly: 100,
            max_volume_cap: 85,
            max_concurrent_same_sound: 4,
        }
    }
}

impl AudioSettings {
    /// Intuitive readout for the loud-sound limiter: `OFF` at `100`,
    /// `Muted` at `0`, otherwise a Gentle/Medium/Strong band label with
    /// the equivalent attenuation in dB (`20·log10(cap)`), so the slider
    /// shows *how much* the guard turns loud sounds down instead of a
    /// bare percent.
    pub fn max_volume_cap_label(&self, lang: UiLang) -> String {
        let cap = self.max_volume_cap.min(100);
        if cap == 100 {
            return tr(lang, "OFF (No Limit)", "OFF（制限なし）").to_string();
        }
        if cap == 0 {
            return tr(lang, "Muted", "ミュート").to_string();
        }
        let (en, ja) = match cap {
            80..=99 => ("Gentle", "軽"),
            60..=79 => ("Medium", "中"),
            _ => ("Strong", "重"),
        };
        let db = 20.0 * (f32::from(cap) / 100.0).log10();
        format!("{} ({db:.1} dB)", tr(lang, en, ja))
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

    pub const fn label(self, lang: UiLang) -> &'static str {
        match (self, lang) {
            (Self::Ask, UiLang::En) => "Ask",
            (Self::AutoCrashOnly, UiLang::En) => "Auto (crashes only)",
            (Self::Off, UiLang::En) => "Off",
            (Self::Ask, UiLang::Ja) => "確認",
            (Self::AutoCrashOnly, UiLang::Ja) => "クラッシュ時のみ",
            (Self::Off, UiLang::Ja) => "オフ",
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
    /// Opt-in Compact Debug HUD overlay; `enabled` stays `false` by default so the
    /// vanilla-style HUD is untouched until the player turns it on.
    pub compact_debug_hud: CompactDebugHudConfig,
    /// Directional accessibility subtitles — opt-in (`false` by default).
    pub show_subtitles: bool,
    /// Opt-in auto-reconnect on unexpected drops
    /// ([`crate::qol_helpers::AutoReconnectController`]) — `false` by
    /// default so public servers never see an unattended redial.
    pub auto_reconnect: bool,
    /// Opt-in hotbar refill from the inventory on stack depletion —
    /// `false` by default (opinionated gameplay helper).
    pub auto_replenish: bool,
    /// Opt-in quick-stack-to-nearby-chests helper — `false` by
    /// default (opinionated gameplay helper).
    pub quick_stack_nearby: bool,
    /// Opt-in hunger/saturation preview on food tooltips — `false` by
    /// default (yields to a dedicated food-stats mod install).
    pub food_saturation_preview: bool,
    /// Shulker Box / Bundle contents inside the item tooltip — `false`
    /// by default (yields to dedicated shulker-tooltip mods; kept only
    /// for settings-file backward compatibility and programmatic
    /// toggles — the QoL settings tab no longer surfaces it).
    pub shulker_tooltip_peek: bool,
    /// Chat scroll-lock: reading older chat is never yanked to the
    /// bottom by new messages — pure UX, `true` by default.
    pub chat_scroll_lock: bool,
    /// Non-intrusive top-right toast stack — `true` by default.
    pub toast_notifications: bool,
    /// Recipe browser side panels beside open container
    /// screens — `false` by default (opt-in; `Ctrl+O` toggles it
    /// in-game too).
    pub recipe_overlay_enabled: bool,
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
            compact_debug_hud: CompactDebugHudConfig::default(),
            show_subtitles: false,
            // Opinionated gameplay helpers are strictly opt-in; pure-UX
            // improvements ship on. The four mod-overlapping helpers
            // (replenish / quick-stack / food preview / shulker peek)
            // stay defaulted off — Mods > Recommended points players at
            // the dedicated community mods instead.
            auto_reconnect: false,
            auto_replenish: false,
            quick_stack_nearby: false,
            food_saturation_preview: false,
            shulker_tooltip_peek: false,
            chat_scroll_lock: true,
            toast_notifications: true,
            recipe_overlay_enabled: false,
        }
    }
}

/// Coarse Compact Debug HUD preset the QoL toggle cycles through.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CompactDebugHudPreset {
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
    /// The preset the QoL Compact Debug HUD toggle reports: `Off` while
    /// `compact_debug_hud.enabled` is `false`, `Advanced` when every advanced
    /// segment is on, `Basic` otherwise.
    pub fn compact_debug_hud_preset(&self) -> CompactDebugHudPreset {
        if !self.compact_debug_hud.enabled {
            CompactDebugHudPreset::Off
        } else if self.compact_debug_hud_is_advanced() {
            CompactDebugHudPreset::Advanced
        } else {
            CompactDebugHudPreset::Basic
        }
    }

    fn compact_debug_hud_is_advanced(&self) -> bool {
        let m = &self.compact_debug_hud;
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

    /// Cycle the Compact Debug HUD Off → Basic → Advanced → Off, preserving the
    /// configured anchor and the player's individual segment choices
    /// while the overlay stays enabled.
    pub fn cycle_compact_debug_hud(&mut self) {
        match self.compact_debug_hud_preset() {
            CompactDebugHudPreset::Off => {
                self.compact_debug_hud = CompactDebugHudConfig {
                    enabled: true,
                    anchor: self.compact_debug_hud.anchor,
                    ..CompactDebugHudConfig::default()
                };
            }
            CompactDebugHudPreset::Basic => {
                let m = &mut self.compact_debug_hud;
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
            CompactDebugHudPreset::Advanced => {
                self.compact_debug_hud.enabled = false;
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

    pub const fn label(self, lang: UiLang) -> &'static str {
        match (self, lang) {
            (Self::Unrestricted, UiLang::En) => "Unrestricted",
            (Self::FairPlay, UiLang::En) => "Fair-Play",
            (Self::Disabled, UiLang::En) => "Disabled",
            (Self::Unrestricted, UiLang::Ja) => "無制限",
            (Self::FairPlay, UiLang::Ja) => "フェアプレイ",
            (Self::Disabled, UiLang::Ja) => "無効",
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

    pub const fn label(self, lang: UiLang) -> &'static str {
        match (self, lang) {
            (Self::All, UiLang::En) => "All",
            (Self::PartyOnly, UiLang::En) => "Party Only",
            (Self::None, UiLang::En) => "None",
            (Self::All, UiLang::Ja) => "すべて",
            (Self::PartyOnly, UiLang::Ja) => "パーティーのみ",
            (Self::None, UiLang::Ja) => "なし",
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
    /// Full Megu lighting, shadows, reflections, and atmosphere.
    MeguNatural,
}

impl VisualStylePreset {
    /// All presets in their fixed cycle order.
    pub const ALL: [Self; 2] = [Self::Vanilla, Self::MeguNatural];

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
            (Self::Vanilla, UiLang::En) => "Vanilla",
            (Self::Vanilla, UiLang::Ja) => "バニラ",
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

/// UI mirror of the persisted physical gameplay bindings.
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

impl KeyBindings {
    pub fn get(&self, index: usize) -> Option<&str> {
        Some(String::as_str(match index {
            0 => &self.forward,
            1 => &self.back,
            2 => &self.left,
            3 => &self.right,
            4 => &self.jump,
            5 => &self.sneak,
            6 => &self.sprint,
            7 => &self.inventory,
            8 => &self.drop,
            9 => &self.swap_offhand,
            10 => &self.chat,
            11 => &self.command,
            12 => &self.zoom,
            13 => &self.camera_toggle,
            14 => &self.fullscreen,
            15 => &self.advancements,
            16 => &self.sort_inventory,
            _ => return None,
        }))
    }

    pub fn set(&mut self, index: usize, value: String) -> bool {
        let slot = match index {
            0 => &mut self.forward,
            1 => &mut self.back,
            2 => &mut self.left,
            3 => &mut self.right,
            4 => &mut self.jump,
            5 => &mut self.sneak,
            6 => &mut self.sprint,
            7 => &mut self.inventory,
            8 => &mut self.drop,
            9 => &mut self.swap_offhand,
            10 => &mut self.chat,
            11 => &mut self.command,
            12 => &mut self.zoom,
            13 => &mut self.camera_toggle,
            14 => &mut self.fullscreen,
            15 => &mut self.advancements,
            16 => &mut self.sort_inventory,
            _ => return false,
        };
        *slot = value;
        true
    }
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
    pub fullscreen: bool,
    pub mouse_invert_y: bool,
    pub gamma_pct: u16,
    /// `0` = automatic, `1..=4` are fixed UI scale factors.
    pub gui_scale: u8,
    pub keys: KeyBindings,
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
    /// Built-in inventory drag controls, yielded when an external drag-controls mod is installed.
    pub inventory_drag_controls: bool,
    /// Built-in inventory sorting, yielded when Inventory Sort is installed.
    pub qol_inventory_sort: bool,
    /// Zsh-style command bar with ghost completions and 8x8 inline badges.
    pub zsh_command_bar: bool,
    /// Generated PLaMo 3 auto-translation resource pack enabled.
    pub plamo_auto_translation_pack: bool,
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
            fullscreen: false,
            mouse_invert_y: false,
            gamma_pct: 100,
            gui_scale: 0,
            keys: KeyBindings::default(),
            audio: AudioSettings::default(),
            shader_quick: ShaderQuickSettings::default(),
            qol: QolSettings::default(),
            fov_deg: 70,
            mouse_sensitivity_pct: 100,
            zoom_divisor_tenths: 40,
            qol_looking_at_hud: true,
            inventory_drag_controls: true,
            qol_inventory_sort: true,
            zsh_command_bar: true,
            plamo_auto_translation_pack: false,
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

    /// Enable the built-in drag behavior only if an external drag-controls mod does not own it.
    pub fn builtin_inventory_drag_controls_active(&self, external_drag_controls_installed: bool) -> bool {
        self.inventory_drag_controls && !external_drag_controls_installed
    }

    /// Enable the built-in sorting only if Inventory Sort does not own it.
    pub fn builtin_inventory_sort_active(&self, inventory_sort_installed: bool) -> bool {
        self.qol_inventory_sort && !inventory_sort_installed
    }

    /// Built-in hotbar refill runs only when the player opted in AND no
    /// dedicated inventory-management mod owns
    /// the feature AND the joined server is not fair-play strict —
    /// inventory automation on a strict server trips Watchdog/Grim-style
    /// anti-cheat, so Megu yields automatically.
    pub fn builtin_auto_replenish_active(
        &self,
        inventory_mod_installed: bool,
        fair_play_strict: bool,
    ) -> bool {
        self.qol.auto_replenish && !inventory_mod_installed && !fair_play_strict
    }

    /// Built-in Quick Stack runs only when the player opted in AND no
    /// dedicated inventory mod owns it AND the joined server is not
    /// fair-play strict (same anti-cheat yield as
    /// [`builtin_auto_replenish_active`](Self::builtin_auto_replenish_active)).
    pub fn builtin_quick_stack_active(
        &self,
        inventory_mod_installed: bool,
        fair_play_strict: bool,
    ) -> bool {
        self.qol.quick_stack_nearby && !inventory_mod_installed && !fair_play_strict
    }

    /// Built-in food-value preview runs only when the player opted in
    /// AND a dedicated food-stats mod does not already own the tooltip —
    /// a modded client sees exactly one hunger preview, never two.
    pub fn builtin_food_preview_active(&self, mod_installed: bool) -> bool {
        self.qol.food_saturation_preview && !mod_installed
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
    VideoClouds,
    VideoEntityShadows,
    VideoComfortExposure,
    VideoDynamicLights,
    VideoEnchantmentGlint,
    VideoIndirectLightBounce,
    VideoDirectionalShadowGameplay,
    VideoLazyCatchupGameplay,
    VideoRamBudget,
    DisplayFullscreen,
    DisplayInvertY,
    DisplayGamma,
    DisplayGuiScale,
    KeyBindings,
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
    QolInventoryDragControls,
    QolInventorySort,
    ZshCommandBar,
    QolSmoothZoom,
    QolToolWarnPercent,
    QolPreventToolBreak,
    QolChatHistoryLines,
    QolRetainChatOnReconnect,
    QolCrashReportMode,
    QolShowSupportLinks,
    QolCompactDebugHud,
    QolShowSubtitles,
    QolAutoReconnect,
    QolAutoReplenish,
    QolQuickStack,
    QolFoodPreview,
    QolShulkerPeek,
    QolChatScrollLock,
    QolToastNotifications,
    QolRecipeOverlay,
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
            K::VideoSmoothLighting => current.video.smooth_lighting == opened.video.smooth_lighting,
            K::VideoClouds => current.video.clouds == opened.video.clouds,
            K::VideoEntityShadows => {
                current.video.entity_shadows == opened.video.entity_shadows
            }
            K::VideoComfortExposure => {
                current.video.comfort_exposure == opened.video.comfort_exposure
            }
            K::VideoDynamicLights => {
                current.video.dynamic_lights == opened.video.dynamic_lights
            }
            K::VideoEnchantmentGlint => {
                current.video.enchantment_glint == opened.video.enchantment_glint
            }
            K::VideoIndirectLightBounce => {
                current.video.indirect_light_bounce
                    == opened.video.indirect_light_bounce
            }
            K::VideoDirectionalShadowGameplay => {
                current.video.directional_shadow_gameplay
                    == opened.video.directional_shadow_gameplay
            }
            K::VideoLazyCatchupGameplay => {
                current.video.lazy_catchup_gameplay == opened.video.lazy_catchup_gameplay
            }
            K::VideoRamBudget => current.video.ram_budget_mib == opened.video.ram_budget_mib,
            K::DisplayFullscreen => current.fullscreen == opened.fullscreen,
            K::DisplayInvertY => current.mouse_invert_y == opened.mouse_invert_y,
            K::DisplayGamma => current.gamma_pct == opened.gamma_pct,
            K::DisplayGuiScale => current.gui_scale == opened.gui_scale,
            K::KeyBindings => current.keys == opened.keys,
            K::AudioMaster => current.audio.master == opened.audio.master,
            K::AudioMusic => current.audio.music == opened.audio.music,
            K::AudioBlocks => current.audio.blocks == opened.audio.blocks,
            K::AudioHostile => current.audio.hostile == opened.audio.hostile,
            K::AudioFriendly => current.audio.friendly == opened.audio.friendly,
            K::AudioMaxVolumeCap => current.audio.max_volume_cap == opened.audio.max_volume_cap,
            K::AudioMaxConcurrentSameSound => {
                current.audio.max_concurrent_same_sound == opened.audio.max_concurrent_same_sound
            }
            K::AudioSpatial3d => current.visual.spatial_audio_3d == opened.visual.spatial_audio_3d,
            K::ShadersEnabled => {
                current.shader_quick.shaders_enabled == opened.shader_quick.shaders_enabled
            }
            K::VisualStylePreset => current.visual.preset == opened.visual.preset,
            K::ExternalShaderPack => {
                current.visual.external_shader_pack_name == opened.visual.external_shader_pack_name
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
            K::QolInventoryDragControls => current.inventory_drag_controls == opened.inventory_drag_controls,
            K::QolInventorySort => current.qol_inventory_sort == opened.qol_inventory_sort,
            K::ZshCommandBar => current.zsh_command_bar == opened.zsh_command_bar,
            K::QolSmoothZoom => current.qol.smooth_zoom == opened.qol.smooth_zoom,
            K::QolToolWarnPercent => current.qol.tool_warn_percent == opened.qol.tool_warn_percent,
            K::QolPreventToolBreak => {
                current.qol.prevent_tool_break == opened.qol.prevent_tool_break
            }
            K::QolChatHistoryLines => {
                current.qol.chat_history_lines == opened.qol.chat_history_lines
            }
            K::QolRetainChatOnReconnect => {
                current.qol.retain_chat_on_reconnect == opened.qol.retain_chat_on_reconnect
            }
            K::QolCrashReportMode => current.qol.crash_report_mode == opened.qol.crash_report_mode,
            K::QolShowSupportLinks => {
                current.qol.show_support_links == opened.qol.show_support_links
            }
            K::QolCompactDebugHud => current.qol.compact_debug_hud == opened.qol.compact_debug_hud,
            K::QolShowSubtitles => current.qol.show_subtitles == opened.qol.show_subtitles,
            K::QolAutoReconnect => current.qol.auto_reconnect == opened.qol.auto_reconnect,
            K::QolAutoReplenish => current.qol.auto_replenish == opened.qol.auto_replenish,
            K::QolQuickStack => current.qol.quick_stack_nearby == opened.qol.quick_stack_nearby,
            K::QolFoodPreview => {
                current.qol.food_saturation_preview == opened.qol.food_saturation_preview
            }
            K::QolShulkerPeek => {
                current.qol.shulker_tooltip_peek == opened.qol.shulker_tooltip_peek
            }
            K::QolChatScrollLock => {
                current.qol.chat_scroll_lock == opened.qol.chat_scroll_lock
            }
            K::QolToastNotifications => {
                current.qol.toast_notifications == opened.qol.toast_notifications
            }
            K::QolRecipeOverlay => {
                current.qol.recipe_overlay_enabled == opened.qol.recipe_overlay_enabled
            }
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
            K::Language => current.language == opened.language && current.locale == opened.locale,
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
            K::VideoSmoothLighting => current.video.smooth_lighting = opened.video.smooth_lighting,
            K::VideoClouds => current.video.clouds = opened.video.clouds,
            K::VideoEntityShadows => {
                current.video.entity_shadows = opened.video.entity_shadows
            }
            K::VideoComfortExposure => {
                current.video.comfort_exposure = opened.video.comfort_exposure
            }
            K::VideoDynamicLights => {
                current.video.dynamic_lights = opened.video.dynamic_lights
            }
            K::VideoEnchantmentGlint => {
                current.video.enchantment_glint = opened.video.enchantment_glint
            }
            K::VideoIndirectLightBounce => {
                current.video.indirect_light_bounce =
                    opened.video.indirect_light_bounce
            }
            K::VideoDirectionalShadowGameplay => {
                current.video.directional_shadow_gameplay =
                    opened.video.directional_shadow_gameplay
            }
            K::VideoLazyCatchupGameplay => {
                current.video.lazy_catchup_gameplay = opened.video.lazy_catchup_gameplay
            }
            K::VideoRamBudget => current.video.ram_budget_mib = opened.video.ram_budget_mib,
            K::DisplayFullscreen => current.fullscreen = opened.fullscreen,
            K::DisplayInvertY => current.mouse_invert_y = opened.mouse_invert_y,
            K::DisplayGamma => current.gamma_pct = opened.gamma_pct,
            K::DisplayGuiScale => current.gui_scale = opened.gui_scale,
            K::KeyBindings => current.keys = opened.keys.clone(),
            K::AudioMaster => current.audio.master = opened.audio.master,
            K::AudioMusic => current.audio.music = opened.audio.music,
            K::AudioBlocks => current.audio.blocks = opened.audio.blocks,
            K::AudioHostile => current.audio.hostile = opened.audio.hostile,
            K::AudioFriendly => current.audio.friendly = opened.audio.friendly,
            K::AudioMaxVolumeCap => current.audio.max_volume_cap = opened.audio.max_volume_cap,
            K::AudioMaxConcurrentSameSound => {
                current.audio.max_concurrent_same_sound = opened.audio.max_concurrent_same_sound
            }
            K::AudioSpatial3d => current.visual.spatial_audio_3d = opened.visual.spatial_audio_3d,
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
                current.visual.world_effects_detail = opened.visual.world_effects_detail.clone()
            }
            K::CameraMicroMotion => {
                current.visual.camera_micro_motion = opened.visual.camera_micro_motion
            }
            K::FovDeg => current.fov_deg = opened.fov_deg,
            K::MouseSensitivityPct => current.mouse_sensitivity_pct = opened.mouse_sensitivity_pct,
            K::ZoomDivisorTenths => current.zoom_divisor_tenths = opened.zoom_divisor_tenths,
            K::QolLookingAtHud => current.qol_looking_at_hud = opened.qol_looking_at_hud,
            K::QolInventoryDragControls => current.inventory_drag_controls = opened.inventory_drag_controls,
            K::QolInventorySort => current.qol_inventory_sort = opened.qol_inventory_sort,
            K::ZshCommandBar => current.zsh_command_bar = opened.zsh_command_bar,
            K::QolSmoothZoom => current.qol.smooth_zoom = opened.qol.smooth_zoom,
            K::QolToolWarnPercent => current.qol.tool_warn_percent = opened.qol.tool_warn_percent,
            K::QolPreventToolBreak => {
                current.qol.prevent_tool_break = opened.qol.prevent_tool_break
            }
            K::QolChatHistoryLines => {
                current.qol.chat_history_lines = opened.qol.chat_history_lines
            }
            K::QolRetainChatOnReconnect => {
                current.qol.retain_chat_on_reconnect = opened.qol.retain_chat_on_reconnect
            }
            K::QolCrashReportMode => current.qol.crash_report_mode = opened.qol.crash_report_mode,
            K::QolShowSupportLinks => {
                current.qol.show_support_links = opened.qol.show_support_links
            }
            K::QolCompactDebugHud => current.qol.compact_debug_hud = opened.qol.compact_debug_hud.clone(),
            K::QolShowSubtitles => current.qol.show_subtitles = opened.qol.show_subtitles,
            K::QolAutoReconnect => current.qol.auto_reconnect = opened.qol.auto_reconnect,
            K::QolAutoReplenish => current.qol.auto_replenish = opened.qol.auto_replenish,
            K::QolQuickStack => {
                current.qol.quick_stack_nearby = opened.qol.quick_stack_nearby
            }
            K::QolFoodPreview => {
                current.qol.food_saturation_preview = opened.qol.food_saturation_preview
            }
            K::QolShulkerPeek => {
                current.qol.shulker_tooltip_peek = opened.qol.shulker_tooltip_peek
            }
            K::QolChatScrollLock => {
                current.qol.chat_scroll_lock = opened.qol.chat_scroll_lock
            }
            K::QolToastNotifications => {
                current.qol.toast_notifications = opened.qol.toast_notifications
            }
            K::QolRecipeOverlay => {
                current.qol.recipe_overlay_enabled = opened.qol.recipe_overlay_enabled
            }
            K::FpMinimapMode => current.fair_play.minimap_mode = opened.fair_play.minimap_mode,
            K::FpAllowCaveMap => current.fair_play.allow_cave_map = opened.fair_play.allow_cave_map,
            K::FpAllowMobRadar => {
                current.fair_play.allow_mob_radar = opened.fair_play.allow_mob_radar
            }
            K::FpPlayerRadarScope => {
                current.fair_play.player_radar_scope = opened.fair_play.player_radar_scope
            }
            K::FpAllowWaypointTeleport => {
                current.fair_play.allow_waypoint_teleport = opened.fair_play.allow_waypoint_teleport
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

/// One row in the one-screen reconnect diagnostic.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModSyncPromptRow {
    pub mod_id: String,
    pub required_version: String,
    pub installed_version: Option<String>,
    pub sha512: Option<String>,
    pub sha256: Option<String>,
    pub modrinth_slug: Option<String>,
}

/// Host supplied display state for the explicit Modrinth download prompt.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ModSyncPromptState {
    pub server_address: String,
    pub rows: Vec<ModSyncPromptRow>,
    pub progress_label: String,
    pub progress_percent: Option<u8>,
}

impl ModSyncEntry {
    pub fn added(name: impl Into<String>, sha256_hex: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            sha256_hex: sha256_hex.into(),
            change: ModSyncChange::Added,
        }
    }

    /// Short digest shown in the list (first 12 hex chars). Invalid or
    /// non-ASCII input yields an empty string instead of a byte-slice panic.
    pub fn short_sha(&self) -> &str {
        if !self.sha256_hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            return "";
        }
        let end = self
            .sha256_hex
            .char_indices()
            .nth(12)
            .map(|(i, _)| i)
            .unwrap_or(self.sha256_hex.len());
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

/// Vanilla heart sprite family used for the health rows.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum HeartStyle {
    #[default]
    Normal,
    Poisoned,
    Withered,
    Frozen,
    Hardcore,
}

/// Vanilla bossbar color names in protocol order.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum BossBarColor {
    #[default]
    Pink,
    Blue,
    Red,
    Green,
    Yellow,
    Purple,
    White,
}

impl BossBarColor {
    pub const ALL: [Self; 7] = [
        Self::Pink,
        Self::Blue,
        Self::Red,
        Self::Green,
        Self::Yellow,
        Self::Purple,
        Self::White,
    ];

    pub const fn name(self) -> &'static str {
        match self {
            Self::Pink => "pink",
            Self::Blue => "blue",
            Self::Red => "red",
            Self::Green => "green",
            Self::Yellow => "yellow",
            Self::Purple => "purple",
            Self::White => "white",
        }
    }
}

/// Everything the in-game HUD draws. `health`/`hunger`/`armor` are in
/// vanilla half-icon units (20 = full bar); `air` is in bubbles×30
/// (300 = full). Pure data — the game fills it, the HUD draws it.
#[derive(Clone, Debug)]
pub struct HudState {
    /// Hides every native HUD element (vanilla F1 state).
    pub hud_hidden: bool,
    pub status_effects: Vec<LiveStatusEffect>,
    pub attack_cooldown: f32,
    pub mount_health: Option<(f32, f32)>,
    pub mount_jump_charge: Option<f32>,
    pub screen_overlay: HudScreenOverlay,
    pub health: f32,
    /// Maximum health in half-heart units; vanilla players normally use
    /// 20, while attribute-modified/modded players can use multiple rows.
    pub max_health: f32,
    /// Heart sprite family for the health rows.
    pub heart_style: HeartStyle,
    /// Convenience hardcore flag kept separate from `heart_style` so a
    /// host can mark hardcore without understanding sprite selection.
    pub hardcore: bool,
    pub absorption: f32,
    pub armor: u8,
    pub hunger: u8,
    pub xp_level: u32,
    /// `0.0..=1.0` toward the next level.
    pub xp_progress: f32,
    /// `0..=300` air supply (bubbles hidden at 300).
    pub air: u16,
    /// Local player game mode: `0` survival, `1` creative, `2` adventure,
    /// `3` spectator. `layout::hud` uses it for the vanilla draw gates —
    /// creative and spectator hide the hearts/armor/hunger/air/XP rows,
    /// spectator also drops the hotbar and crosshair.
    pub game_mode: i32,
    /// `(title, fraction)` boss bars, top of screen.
    pub bossbars: Vec<(String, f32)>,
    /// Optional parallel color table for `bossbars`; missing entries use pink.
    pub bossbar_colors: Vec<BossBarColor>,
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
    /// Telemetry snapshot the opt-in Compact Debug HUD overlay reads.
    pub compact_debug_hud_data: CompactDebugHudData,
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
    /// Chat/command input bar mirror: the host fills `open`, `text`,
    /// `cursor` and the completion popup rows each frame while the chat
    /// bar is up; `layout::hud` draws the input box + suggestion popup
    /// bottom-left, vanilla style.
    pub chat_input: crate::hud_widgets::ChatInputOverlay,
    /// Vanilla-style two-column F3 debug overlay (`hud.debug_overlay`):
    /// the host fills it at its own refresh cadence and flips `enabled`
    /// with the F3 chord; `layout::hud` composites it over the HUD —
    /// the `actionbar` keeps carrying real game messages while F3 is up.
    pub debug_overlay: crate::hud_widgets::F3DebugOverlay,
    /// Item cooldown wipe per item name (the same registry id
    /// `hotbar` entries carry): `0.0..=1.0` fraction of the cooldown
    /// remaining. `layout::hud` darkens the top fraction of matching
    /// hotbar slots exactly like vanilla's descending curtain.
    pub item_cooldowns: std::collections::BTreeMap<String, f32>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LiveStatusEffect { pub effect_id: i32, pub name: String, pub amplifier: u8, pub duration_ticks: u32, pub beneficial: bool }
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct HudScreenOverlay { pub hurt_alpha:f32, pub fire_alpha:f32, pub underwater_alpha:f32, pub portal_alpha:f32, pub freezing_alpha:f32, pub pumpkin_blur:bool, pub vignette_alpha:f32 }

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
            hud_hidden: false,
            status_effects: Vec::new(),
            attack_cooldown: 1.0,
            mount_health: None,
            mount_jump_charge: None,
            screen_overlay: HudScreenOverlay::default(),
            health: 20.0,
            max_health: 20.0,
            heart_style: HeartStyle::Normal,
            hardcore: false,
            absorption: 0.0,
            armor: 0,
            hunger: 20,
            xp_level: 0,
            xp_progress: 0.0,
            air: 300,
            game_mode: 0,
            bossbars: Vec::new(),
            bossbar_colors: Vec::new(),
            hotbar: Default::default(),
            offhand: None,
            selected_slot: 0,
            actionbar: String::new(),
            chat_lines: Vec::new(),
            show_player_list: false,
            players: Vec::new(),
            magic: crate::magic_hud::MagicHudState::default(),
            compact_debug_hud_data: CompactDebugHudData::default(),
            subtitles: Vec::new(),
            subtitle_hud: crate::subtitle_hud::SubtitleHudState::default(),
            crafting_conflict: CraftingConflictOverlay::default(),
            scoreboard: ScoreboardState::default(),
            title_overlay: TitleOverlayState::default(),
            looking_at: crate::magic_hud::LookingAtBlockState::default(),
            chat_input: crate::hud_widgets::ChatInputOverlay::default(),
            debug_overlay: crate::hud_widgets::F3DebugOverlay::default(),
            item_cooldowns: std::collections::BTreeMap::new(),
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
            hud_hidden: false,
            status_effects: Vec::new(),
            attack_cooldown: 0.75,
            mount_health: None,
            mount_jump_charge: None,
            screen_overlay: HudScreenOverlay::default(),
            health: 18.0,
            max_health: 20.0,
            heart_style: HeartStyle::Normal,
            hardcore: false,
            absorption: 4.0,
            armor: 15,
            hunger: 19,
            xp_level: 12,
            xp_progress: 0.6,
            air: 210,
            game_mode: 0,
            bossbars: vec![("Ender Dragon".to_string(), 0.65)],
            bossbar_colors: vec![BossBarColor::Pink],
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
            compact_debug_hud_data: CompactDebugHudData::default(),
            subtitles: Vec::new(),
            subtitle_hud: crate::subtitle_hud::SubtitleHudState::default(),
            crafting_conflict: CraftingConflictOverlay::default(),
            scoreboard: ScoreboardState::default(),
            title_overlay: TitleOverlayState::default(),
            looking_at: crate::magic_hud::LookingAtBlockState::default(),
            chat_input: crate::hud_widgets::ChatInputOverlay::default(),
            debug_overlay: crate::hud_widgets::F3DebugOverlay::default(),
            item_cooldowns: std::collections::BTreeMap::new(),
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

/// Translate a [`crate::gamepad`] engine button into the nav-level
/// [`GamepadButton`] of [`UiInputEvent::GamepadPressed`]. `None` for
/// buttons with no UI navigation role (stick clicks, triggers,
/// shoulders, `Guide`, `West`/`North` face — those still flip
/// [`UiState::input_mode`] to gamepad via the engine's own event feed).
pub fn nav_button(btn: crate::gamepad::GamepadButton) -> Option<GamepadButton> {
    use crate::gamepad::GamepadButton as G;
    Some(match btn {
        G::DpadUp => GamepadButton::DpadUp,
        G::DpadDown => GamepadButton::DpadDown,
        G::DpadLeft => GamepadButton::DpadLeft,
        G::DpadRight => GamepadButton::DpadRight,
        G::South => GamepadButton::South,
        G::East => GamepadButton::East,
        G::Start => GamepadButton::Start,
        _ => return None,
    })
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
    /// Notification that the UI state already performed one Back
    /// transition. Hosts must not synthesize a second Escape/pop for it.
    Back,
    /// Quit the game.
    QuitGame,
    /// Start a singleplayer world.
    StartSingleplayer,
    /// Launch the named singleplayer world — a world-list row click
    /// (`seed: None`) or "Create & Play" (`seed` parsed from the seed
    /// field). The host validates `name` with `validate_world_name`.
    PlayWorld {
        /// World folder name under `<gameDir>/saves/`.
        name: String,
        /// Optional seed for a newly created world (`None` = default).
        seed: Option<i64>,
    },
    /// Open the server browser / connect flow.
    StartMultiplayer,
    /// Connect to `address:port` — the Multiplayer screen's Join button
    /// (selected row or the direct-connect field).
    JoinServer {
        /// Resolved host (DNS name or IP literal; never carries a port).
        address: String,
        /// Server port (`1..=65535`).
        port: u16,
    },
    /// Persist the Multiplayer screen's direct-connect field as a new
    /// saved-server row in the host's server-list store.
    SaveServer {
        /// Display name the new row gets.
        name: String,
        /// Resolved host.
        address: String,
        /// Server port (`1..=65535`).
        port: u16,
    },
    /// Remove saved-server row `index` from the host's server-list store.
    DeleteServer {
        /// Index into `multiplayer.servers` (store order).
        index: usize,
    },
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
    /// Copy `0` to the system clipboard — the player's own `megu-id:` or
    /// a generated guest link (host reports when no clipboard backend).
    CopyText(String),
    /// Friends screen `[Add]`: pair `friend_id` (a `megu-id:` string)
    /// under `name`. The host validates fail-closed — bad checksum,
    /// duplicate key, or the player's own key are rejected into
    /// `friends.add_error`.
    AddFriend {
        /// Display name for the new friend row.
        name: String,
        /// The pasted `megu-id:` text.
        friend_id: String,
    },
    /// Friends row `[Remove]`: instantly revoke `friend_id` (a
    /// `megu-id:`) from `megu-friends.toml` and kick any live guest
    /// stream via the share session's allowlist refresh.
    RemoveFriend {
        /// Canonical `megu-id:` of the friend to drop.
        friend_id: String,
    },
    /// Host World Sharing cycle landed on a new mode; the host persists
    /// it to `megu-friends.toml` and starts/stops the E2EE share session.
    SetFriendShareMode(FriendShareMode),
    /// A friend row's Selected-Friends checkbox toggled; the host
    /// persists `selected_for_share` and refreshes the live allowlist.
    SetFriendSelected {
        /// Canonical `megu-id:` of the friend toggled.
        friend_id: String,
        /// New `selected_for_share` value.
        selected: bool,
    },
    /// The relay-address field committed; the host persists it to
    /// `megu-friends.toml` (`""` clears) — joins and sharing never need
    /// `--relay` once this is saved.
    SaveRelayAddr {
        /// The typed `host:port` text, verbatim.
        addr: String,
    },
    /// `[Generate Guest Link]`: mint a 128-bit one-off guest token bound
    /// to `friend_id` and compose the shareable `megu-friend:` link —
    /// admits a guest who is not on the always-on allowlist exactly once
    /// while the token entry lives.
    GenerateGuestLink {
        /// Canonical `megu-id:` of the friend the token binds to.
        friend_id: String,
    },
    /// A "Friends & LAN" friend-world row click: the host resolves row
    /// `index` of `multiplayer.friend_worlds` and opens the E2EE join
    /// (`FriendGuestBridge::start_e2ee_friend`).
    JoinFriendWorld {
        /// Index into `multiplayer.friend_worlds`.
        index: usize,
    },
    /// Guest pasted a `megu-friend:` guest link in the join field; the
    /// host parses host id + invite code + 128-bit token and opens the
    /// E2EE join (`start_e2ee_friend_with_token`).
    JoinGuestLink {
        /// The pasted `megu-friend:` link, verbatim.
        link: String,
    },
    /// Player explicitly accepted the pending mod sync; install `0`.
    ApplyModSync(Vec<ModSyncEntry>),
    /// Player explicitly requested the missing dependencies from Modrinth.
    DownloadMissingFromModrinth(Vec<String>),
    /// Player confirmed downloading the listed requirements and reconnecting.
    ConfirmModSyncDownload,
    /// Player cancelled the mod mismatch recovery flow.
    CancelModSync,
    /// Player declined the pending mod sync.
    DeclineModSync,
    /// Player explicitly confirmed `"Send OK?"` on the crash screen.
    SubmitCrashReport,
    /// Death-screen `Respawn` click: the host sends the play-phase
    /// `client_command` packet with action `0` (`PERFORM_RESPAWN`) and
    /// keeps [`Screen::Death`] up until the server's `respawn` /
    /// `set_health` recovery clears [`UiState::death`].
    PerformRespawn,
    /// Death-screen `Title Screen` click (also the only live escape on
    /// hardcore deaths): the host drops the live session and lands on
    /// [`Screen::Title`].
    LeaveWorld,
    /// Save the current bounded packet history to the game directory.
    SaveInstantReplay,
    /// The integrated Mods & Packs screen produced a host action (apply
    /// staged toggles, open the mods folder, fetch deps, ...). `Close`
    /// and `OpenUrl` are flattened into [`UiAction::Back`] /
    /// [`UiAction::OpenUrl`] instead of appearing here.
    ModManager(crate::mod_manager::ModManagerAction),
    /// The recipe & resource browser produced a host action (crafting
    /// grid transfer). `Close` is flattened into [`UiAction::Back`].
    RecipeBrowser(crate::recipe_browser::RecipeBrowserAction),
    /// The claim-map overlay asks the host to run a claims operation
    /// (`claim`/`unclaim`/`SetPlayerPermission`/`RevokePlayer`) on the
    /// `ClaimsWorld` channel. `Close` is flattened into an internal
    /// overlay close and never reaches the host.
    Claims(crate::claims_ui::ClaimsUiAction),
    /// Player cycled the crafting-conflict switcher; the new candidate
    /// is `hud.crafting_conflict.selected_index`.
    CycleCraftingConflict,
    /// Server-driven sign editor `[Done]`: the host sends the edited sign
    /// text update and closes the edit surface.
    CloseSignEditor,
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
    /// 1-click EN/JA flip — vanilla's globe-button chip on the title
    /// bottom row.
    TitleLanguage,
    /// `?` chip on the title bottom row — opens [`Screen::AboutMegu`].
    TitleAbout,
    WebsiteLink,
    SupportLink,
    ContributeLink,
    ReportLink,
    // Singleplayer world select
    /// A row in the discovered-worlds list; `0` indexes
    /// `singleplayer.worlds`.
    SingleplayerWorld(usize),
    SingleplayerNameField,
    SingleplayerSeedField,
    SingleplayerCreate,
    SingleplayerGameMode,
    SingleplayerDifficulty,
    SingleplayerBack,
    SingleplayerListUp,
    SingleplayerListDown,
    // Multiplayer server select
    /// A row in `multiplayer.servers`; `0` indexes the store order the
    /// host loads.
    MultiplayerServer(usize),
    /// Direct-connect / quick-add `host[:port]` text field.
    MultiplayerAddressField,
    MultiplayerJoin,
    MultiplayerAdd,
    MultiplayerDelete,
    MultiplayerListUp,
    MultiplayerListDown,
    MultiplayerBack,
    /// A friend-hosted world row in the "Friends & LAN" section; `0`
    /// indexes `multiplayer.friend_worlds`. Click = 1-click E2EE join.
    MultiplayerFriendWorld(usize),
    /// A LAN-discovered world row; `0` indexes `multiplayer.lan_worlds`.
    MultiplayerLanWorld(usize),
    /// "Friends & LAN" section pager.
    MultiplayerFriendLanUp,
    MultiplayerFriendLanDown,
    /// Shortcut into `Screen::Friends` from the section header.
    MultiplayerManageFriends,
    // Disconnected screen
    DisconnectedBack,
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
    TabControls,
    ControlKey(usize),
    QolBuiltin(usize),
    CompactDebugHudSegment(usize),
    VideoFullscreen,
    VideoInvertY,
    VideoGamma,
    VideoGuiScale,
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
    VideoClouds,
    VideoEntityShadows,
    VideoComfortExposure,
    VideoDynamicLights,
    VideoEnchantmentGlint,
    VideoIndirectLightBounce,
    VideoDirectionalShadowGameplay,
    VideoLazyCatchupGameplay,
    VideoRamBudget,
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
    VisualNightSky,
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
    QolCompactDebugHud,
    QolSubtitles,
    QolAutoReconnect,
    QolAutoReplenish,
    QolQuickStack,
    QolFoodPreview,
    QolShulkerPeek,
    QolChatScrollLock,
    QolToasts,
    QolRecipeOverlay,
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
    /// `[Copy ID]` beside the player's own `megu-id:`.
    FriendsCopyId,
    /// Add-friend display-name text field.
    FriendsAddNameField,
    /// Add-friend `megu-id:` text field.
    FriendsAddIdField,
    /// `[Add]` — pair the typed `megu-id:` under the typed name.
    FriendsAdd,
    /// A friend row's name/id zone; `0` indexes `friends.friends`.
    /// Click selects the row for `[Generate Guest Link]`.
    FriendRow(usize),
    /// A friend row's Selected-Friends checkbox; `0` indexes
    /// `friends.friends`.
    FriendSelected(usize),
    /// A friend row's instant `[Remove]`; `0` indexes `friends.friends`.
    FriendRemove(usize),
    /// Host World Sharing cycle toggle (Off / Friends Only / Selected).
    FriendsShareMode,
    /// Saved `Relay Address` text field.
    FriendsRelayField,
    /// `[Generate 128-bit Guest Link]` — one-off token for the selected
    /// friend row.
    FriendsGenerateGuestLink,
    /// `[Copy Guest Link]` — copies `friends.guest_link`.
    FriendsCopyGuestLink,
    /// Friends allowlist pager.
    FriendsListUp,
    FriendsListDown,
    // Mod sync
    ModSyncAccept,
    ModSyncDecline,
    ModSyncDownloadMissing,
    ModSyncPromptConfirm,
    ModSyncPromptCancel,
    // Pause
    PauseBackToGame,
    PauseAdvancements,
    PauseSettings,
    PauseStatistics,
    PauseRecipeBrowser,
    PauseSaveReplay,
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
    ModManager(crate::mod_manager::ModManagerWidgetId),
    /// A widget inside the recipe & resource browser.
    Recipe(crate::recipe_browser::RecipeWidgetId),
    /// Native chrome on the recipe browser: the close/back button the
    /// sub-screen itself does not provide.
    RecipeBrowserDone,
    // Server-driven sign editor overlay chrome.
    SignEditorDone,
    // In-game HUD (present only while a crafting conflict is active)
    CraftConflictNext,
    // Death screen
    DeathRespawn,
    DeathTitleScreen,
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
                | Self::VideoClouds
                | Self::VideoEntityShadows
                | Self::VideoRamBudget
                | Self::QolBuiltin(_)
                | Self::CompactDebugHudSegment(_)
                | Self::ShadersEnabled
                | Self::VisualPreset
                | Self::VisualBlocks3d
                | Self::VisualWorldEffects
                | Self::VisualCameraMotion
                | Self::VisualNightSky
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
                | Self::QolCompactDebugHud
                | Self::QolSubtitles
                | Self::QolAutoReconnect
                | Self::QolAutoReplenish
                | Self::QolQuickStack
                | Self::QolFoodPreview
                | Self::QolShulkerPeek
                | Self::QolChatScrollLock
                | Self::QolToasts
                | Self::QolRecipeOverlay
                | Self::FpMinimapMode
                | Self::FpCaveMap
                | Self::FpMobRadar
                | Self::FpRadarScope
                | Self::FpWaypointTeleport
                | Self::LanguageChoice
                | Self::FriendsShareMode
                | Self::FriendSelected(_)
        )
    }

    /// `true` for editable text fields (typing routes to them).
    pub const fn is_text_field(self) -> bool {
        matches!(
            self,
            Self::VideoFpsField
                | Self::FriendsJoinField
                | Self::FriendsAddNameField
                | Self::FriendsAddIdField
                | Self::FriendsRelayField
                | Self::SingleplayerNameField
                | Self::SingleplayerSeedField
                | Self::MultiplayerAddressField
        )
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
/// Largest accepted join field text — a `MEGU-XXXX-XXXX` invite code or
/// a `megu-friend:megu-id:<58>:<8>:<26>` guest link (≈115 chars + slack).
const JOIN_FIELD_MAX_LEN: usize = 128;
/// Largest accepted add-friend display-name text.
const FRIEND_NAME_FIELD_MAX_LEN: usize = 32;
/// Largest accepted add-friend `megu-id:` text (8 + 58 + slack).
const FRIEND_ID_FIELD_MAX_LEN: usize = 80;
/// Largest accepted saved-relay `host:port` text.
const RELAY_FIELD_MAX_LEN: usize = 260;
/// Largest accepted world-name field text on [`Screen::Singleplayer`].
const WORLD_NAME_FIELD_MAX_LEN: usize = 64;
/// Largest accepted seed field text on [`Screen::Singleplayer`].
const WORLD_SEED_FIELD_MAX_LEN: usize = 64;
/// Crash report preview cap: 4 KiB of already-redacted markdown.
pub const CRASH_PREVIEW_MAX_BYTES: usize = 4096;

/// The [`Screen::Singleplayer`] world select & "Create New World" flow.
///
/// The host fills `worlds` from `<gameDir>/saves/` (sorted
/// deterministically) each time the screen opens; the fields are pure
/// UI-side text the host reads back through [`UiAction::PlayWorld`].
#[derive(Clone, Debug)]
pub struct SingleplayerState {
    /// Discovered world folder names, sorted deterministically.
    pub worlds: Vec<String>,
    /// Editable "World Name" text for a new world (default `"New World"`).
    pub new_name: String,
    /// Editable "Seed" text: blank = server default, a signed integer is
    /// used verbatim, anything else maps through Java's
    /// `String.hashCode()` — the same string-seed mapping vanilla world
    /// creation applies (see [`seed_from_text`]).
    pub seed_text: String,
    /// First visible row in `worlds` when the list overflows the panel.
    pub scroll: usize,
    pub game_mode: SingleplayerGameMode,
    pub difficulty: SingleplayerDifficulty,
    name_focused: bool,
    seed_focused: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SingleplayerGameMode { #[default] Survival, Creative, Hardcore }
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SingleplayerDifficulty { Peaceful, Easy, #[default] Normal, Hard }

/// Create-world options selected on the Singleplayer screen. Kept as a
/// small copyable payload because `UiAction::PlayWorld` is a stable
/// cross-crate contract; hosts forward this alongside that action.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct WorldLaunchOptions {
    pub game_mode: SingleplayerGameMode,
    pub difficulty: SingleplayerDifficulty,
}

/// Full create-and-play payload after field validation/defaulting.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CreateWorldPayload {
    pub name: String,
    pub seed: Option<i64>,
    pub game_mode: SingleplayerGameMode,
    pub difficulty: SingleplayerDifficulty,
}

impl SingleplayerState {
    /// The Create New World form is part of the current singleplayer
    /// surface. This predicate keeps scroll plumbing explicit and leaves
    /// room for a future collapsed create panel without changing callers.
    pub fn create_expanded(&self) -> bool {
        true
    }

    /// The game-mode/difficulty pair currently selected in Create World.
    pub fn launch_options(&self) -> WorldLaunchOptions {
        WorldLaunchOptions {
            game_mode: self.game_mode,
            difficulty: self.difficulty,
        }
    }
}

/// Vertical scroll offsets (logical px) for menu screens whose content
/// can exceed a high-GUI-scale viewport.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MenuScrollState {
    pub settings_video: usize,
    pub settings_audio: usize,
    pub settings_shaders: usize,
    pub settings_qol: usize,
    pub settings_fairplay: usize,
    pub settings_language: usize,
    pub settings_controls: usize,
    pub singleplayer_create: usize,
    pub friends: usize,
}

impl Default for SingleplayerState {
    fn default() -> Self {
        Self {
            worlds: Vec::new(),
            new_name: "New World".to_string(),
            seed_text: String::new(),
            scroll: 0,
            game_mode: SingleplayerGameMode::Survival,
            difficulty: SingleplayerDifficulty::Normal,
            name_focused: false,
            seed_focused: false,
        }
    }
}

/// Resolve the optional seed field to a launch seed: blank (or
/// whitespace-only) is `None` — the server default — a signed integer
/// parses verbatim, and any other text maps through Java's
/// `String.hashCode()` semantics (UTF-16 code units, `h = 31*h + u`),
/// the same string-to-seed rule vanilla world creation uses.
pub fn seed_from_text(text: &str) -> Option<i64> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Ok(v) = trimmed.parse::<i64>() {
        return Some(v);
    }
    let mut h: i32 = 0;
    for u in trimmed.encode_utf16() {
        h = h.wrapping_mul(31).wrapping_add(i32::from(u));
    }
    Some(i64::from(h))
}

/// One saved-server row on [`Screen::Multiplayer`]. The host fills these
/// from its persistent store (`megu_servers.toml`) each time the screen
/// opens; `Multiplayer` never reads the disk itself.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MultiplayerServerEntry {
    /// Display name the row shows first.
    pub name: String,
    /// Resolved host (DNS name or IP literal; never carries a port).
    pub address: String,
    /// Server port.
    pub port: u16,
}

/// The [`Screen::Multiplayer`] server select + direct-connect flow.
///
/// The host fills `servers` from its server-list store when the screen
/// opens and persists changes triggered by
/// [`UiAction::SaveServer`]/[`UiAction::DeleteServer`]; the fields are
/// pure UI-side text the host reads back through [`UiAction::JoinServer`]
/// / [`UiAction::SaveServer`].
#[derive(Clone, Debug, Default)]
pub struct MultiplayerScreenState {
    /// Saved servers in store order.
    pub servers: Vec<MultiplayerServerEntry>,
    /// Friend-hosted worlds currently online (host-filled presence feed).
    /// Row clicks are 1-click E2EE joins ([`UiAction::JoinFriendWorld`]).
    pub friend_worlds: Vec<FriendWorldEntry>,
    /// LAN-discovered worlds (host-filled discovery feed). Row clicks
    /// join `address`/`port` directly ([`UiAction::JoinServer`]).
    pub lan_worlds: Vec<LanWorldEntry>,
    /// Highlighted row — the Join button's target when the
    /// direct-connect field is blank.
    pub selected: Option<usize>,
    /// Editable direct-connect / quick-add `host[:port]` text.
    pub address_text: String,
    /// First visible row in `servers` when the list overflows the panel.
    pub scroll: usize,
    /// First visible row across `friend_worlds` + `lan_worlds` in the
    /// "Friends & LAN" section when it overflows its panel.
    pub friend_lan_scroll: usize,
    /// Last inline status/error line (invalid input, store failure) —
    /// rendered as a banner so joins never fail silently.
    pub status: Option<String>,
    address_focused: bool,
}

impl MultiplayerScreenState {
    /// `true` while the direct-connect field owns typing.
    pub fn address_field_active(&self) -> bool {
        self.address_focused
    }
}

/// The [`Screen::Disconnected`] kick/disconnect reason view. The host
/// fills it when the live session reports `SessionStatus::Disconnected`
/// and drops the session before the screen renders.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DisconnectedScreenState {
    /// Banner title (e.g. `Disconnected`).
    pub title: String,
    /// Exact reason the server/session reported, verbatim.
    pub reason: String,
}

/// One friend-hosted live world row in the "Friends & LAN" section of
/// [`Screen::Multiplayer`]. The host fills these from its friend-presence
/// feed; a row click is a 1-click E2EE join ([`UiAction::JoinFriendWorld`]).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FriendWorldEntry {
    /// Display name of the hosting friend.
    pub friend_name: String,
    /// Canonical `megu-id:` of the host (pinned by the E2EE handshake).
    pub friend_id: String,
    /// World name the friend registered on the relay.
    pub world_name: String,
    /// Players currently in that world.
    pub online_players: u32,
    /// Host-declared guest cap.
    pub max_players: u32,
    /// Relay `host:port` the world is reachable through.
    pub relay_addr: String,
    /// Invite code the friend world published (`MEGU-XXXX-XXXX`).
    pub invite_code: String,
}

/// One LAN-discovered world row in the "Friends & LAN" section; a row
/// click joins `address`/`port` directly ([`UiAction::JoinServer`]).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LanWorldEntry {
    /// Display name of the LAN world.
    pub name: String,
    /// Resolved host (IP literal or DNS name).
    pub address: String,
    /// Server port.
    pub port: u16,
    /// Optional motd/player line the discovery feed reported.
    pub motd: String,
}

/// Who a hosted world is shared with over `FRL3-FRIEND`. Mirrors the
/// host's `megu-friends.toml` `share_mode`; strictly one hop — there is
/// no friends-of-friends expansion.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum FriendShareMode {
    /// Friend hosting is disabled; the allowlist is always empty.
    #[default]
    Off,
    /// Every paired friend may join.
    FriendsOnly,
    /// Only friends flagged `selected_for_share` may join.
    SelectedFriends,
}

impl FriendShareMode {
    /// The next mode in the `Off → Friends Only → Selected Friends`
    /// cycle; `dir` steps backward when negative.
    pub fn step(self, dir: i32) -> Self {
        const ALL: [FriendShareMode; 3] = [
            FriendShareMode::Off,
            FriendShareMode::FriendsOnly,
            FriendShareMode::SelectedFriends,
        ];
        let i = ALL.iter().position(|m| *m == self).unwrap_or(0) as i32;
        ALL[(i + dir).rem_euclid(ALL.len() as i32) as usize]
    }

    /// Localized label for the cycle toggle.
    pub fn label(self, lang: UiLang) -> &'static str {
        match self {
            FriendShareMode::Off => tr(lang, "Off", "オフ"),
            FriendShareMode::FriendsOnly => {
                tr(lang, "Friends Only", "フレンドのみ")
            }
            FriendShareMode::SelectedFriends => {
                tr(lang, "Selected Friends", "選択したフレンド")
            }
        }
    }
}

/// One paired friend row on [`Screen::Friends`]. The host mirrors
/// `megu-friends.toml` here; runtime columns (`connected`, `sas_code`)
/// come from the live share/join sessions.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FriendListEntry {
    /// Display name the player gave this friend.
    pub name: String,
    /// Canonical `megu-id:` (rendered truncated).
    pub friend_id: String,
    /// Admitted under [`FriendShareMode::SelectedFriends`].
    pub selected_for_share: bool,
    /// `true` while this friend holds a live guest stream on our world.
    pub connected: bool,
    /// Six-digit `FRL3-FRIEND` transcript SAS from a live pairing
    /// session, when one is known.
    pub sas_code: Option<u32>,
}

/// The [`Screen::Friends`] FRL3 identity + allowlist + host-sharing
/// state. Everything here is a host-maintained mirror of
/// `<gameDir>/megu-friends.toml` plus editable text fields; mutations
/// are emitted as [`UiAction`]s the host persists.
#[derive(Clone, Debug, Default)]
pub struct FriendsScreenState {
    /// The player's own full `megu-id:` (rendered truncated; copied whole).
    pub my_friend_id: String,
    /// Paired friends in store order.
    pub friends: Vec<FriendListEntry>,
    /// Row selected for `[Generate Guest Link]`, if any.
    pub selected: Option<usize>,
    /// Host World Sharing toggle mirror (host writes from the store).
    pub share_mode: FriendShareMode,
    /// `true` while the host's E2EE friend-share session is live.
    pub sharing_active: bool,
    /// Editable saved-relay `host:port` text (persisted via
    /// [`UiAction::SaveRelayAddr`]).
    pub relay_addr_text: String,
    /// Latest generated 128-bit guest link text, if any.
    pub guest_link: Option<String>,
    /// Add-friend display-name field.
    pub add_name_text: String,
    /// Add-friend `megu-id:` field.
    pub add_id_text: String,
    /// Fail-closed add-friend rejection text (bad checksum, duplicate,
    /// self-key) — rendered inline until the next input.
    pub add_error: Option<String>,
    /// Inline status line (share/relay/guest-link feedback).
    pub status: Option<String>,
    /// First visible row in `friends` when the list overflows the panel.
    pub scroll: usize,
    name_focused: bool,
    id_focused: bool,
    relay_focused: bool,
}

impl FriendsScreenState {
    /// `true` while the add-friend name field owns typing.
    pub fn add_name_field_active(&self) -> bool {
        self.name_focused
    }

    /// `true` while the add-friend `megu-id:` field owns typing.
    pub fn add_id_field_active(&self) -> bool {
        self.id_focused
    }

    /// `true` while the relay-address field owns typing.
    pub fn relay_field_active(&self) -> bool {
        self.relay_focused
    }
}

/// Largest accepted direct-connect text (`"[v6]:65535"` + slack).
const SERVER_ADDRESS_FIELD_MAX_LEN: usize = 260;

/// Parse the Multiplayer direct-connect field: `host`, `host:port`, or
/// `[v6]`/`[v6]:port`. A missing port defaults to vanilla's `25565`; a
/// port outside `1..=65535` or an empty host rejects.
pub fn parse_server_address(text: &str) -> Option<(String, u16)> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Some(rest) = trimmed.strip_prefix('[') {
        let (host, port_part) = rest.split_once(']')?;
        let port = match port_part.strip_prefix(':') {
            Some(p) => p.parse::<u16>().ok()?,
            None if port_part.is_empty() => 25565,
            None => return None,
        };
        return (!host.is_empty() && port != 0).then(|| (host.to_string(), port));
    }
    match trimmed.split_once(':') {
        Some((host, port_text)) if !port_text.contains(':') => {
            let port = port_text.parse::<u16>().ok()?;
            (!host.is_empty() && port != 0).then(|| (host.to_string(), port))
        }
        // Multiple colons with no brackets: treat as a bare IPv6 literal.
        Some(_) if trimmed.matches(':').count() > 1 => Some((trimmed.to_string(), 25565)),
        Some(_) => None,
        None => Some((trimmed.to_string(), 25565)),
    }
}

/// The whole client UI state machine.
#[derive(Clone, Debug)]
pub struct UiState {
    /// Runtime-provided vanilla or mod translation overrides.
    pub translations: HashMap<String, String>,
    pub creative_inventory: crate::creative_screen::CreativeInventoryState,
    pub bed_sleep_overlay: Option<crate::gameplay_screens::BedSleepOverlay>,
    pub book_editor: Option<crate::gameplay_screens::BookEditorState>,
    pub end_credits: Option<crate::gameplay_screens::EndCreditsState>,
    /// Current screen.
    pub screen: Screen,
    /// Navigation stack for Back/Escape (`navigate` pushes).
    pub nav_stack: Vec<Screen>,
    /// All player-facing settings.
    pub settings: UiSettings,
    /// Selected binding row while waiting for a physical key press.
    pub capturing_key_binding: Option<usize>,
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
    /// Mismatch diagnosis and download progress shown by `ModSyncPrompt`.
    pub mod_sync_prompt: ModSyncPromptState,
    /// `Some(true)` = accepted, `Some(false)` = declined.
    pub mod_sync_decision: Option<bool>,

    /// HUD data the game fills per frame.
    pub hud: HudState,
    /// Server-driven sign-edit session (S2C `open_sign_editor`): while
    /// `Some`, the HUD composites the 4-line editor overlay and the host
    /// routes all keyboard/IME/clipboard input into it.
    pub sign_editor: Option<crate::sign_and_chat::SignEditorState>,
    /// Claim-map overlay (OpenPAC/FTB-Chunks style): while `Some` it
    /// composites over the current screen and owns all input — its
    /// [`crate::claims_ui::ClaimsUiAction`]s surface to the host as
    /// [`UiAction::Claims`]. The host fills chunk/roster data from the
    /// `ClaimsWorld` sync feed and opens/closes it on the map keybind.
    pub claims_ui: Option<crate::claims_ui::ClaimsUiState>,
    /// Which input family owns the UI right now — keyboard/mouse vs
    /// controller — fed by [`handle_input`](Self::handle_input) through
    /// the `gamepad` engine's tracker. Prompts stay keyboard/mouse until
    /// the first real pad activity arrives.
    pub input_mode: crate::gamepad::InputModeTracker,
    /// Last mouse position, for motion-delta source switching.
    last_mouse: Option<(i32, i32)>,
    /// Death-screen data the live session fills on `player_combat_kill` /
    /// `set_health(<=0)`; rendered while [`Screen::Death`] is up.
    pub death: crate::death_screen::DeathScreenState,

    /// Last status line the host reported (join failures, invite
    /// results, ...). Rendered on [`Screen::Title`] and
    /// [`Screen::Singleplayer`] so errors never fail silently; the HUD
    /// shows the same text through `hud.chat_lines` in game.
    pub title_status: Option<String>,
    /// `true` while the host composites the rotating 1.21.1 panorama
    /// behind the Title screen — `title()` then skips its opaque menu
    /// gradient so the 3D backdrop shows through (`false` = painted
    /// fallback, e.g. headless).
    pub title_panorama_active: bool,
    /// The [`Screen::Singleplayer`] world select & create state.
    pub singleplayer: SingleplayerState,
    /// The [`Screen::Multiplayer`] server select & direct-connect state.
    pub multiplayer: MultiplayerScreenState,
    /// The [`Screen::Friends`] FRL3 identity/allowlist/sharing state.
    pub friends: FriendsScreenState,
    /// The [`Screen::Disconnected`] kick/drop reason data.
    pub disconnected: DisconnectedScreenState,

    /// The crash being shown on [`Screen::CrashRecovery`].
    pub crash: Option<CrashContext>,
    /// `true` after the report was submitted once (button disables).
    pub crash_sent: bool,

    /// Optional link on the About screen — hidden when empty.
    pub support_url: String,
    /// Optional project website, shown on About when configured.
    pub website_url: String,
    /// Optional link on the About screen — hidden when empty.
    pub contribute_url: String,
    /// Optional bug report URL on About and Pause, hidden while empty.
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

    /// The integrated ModManager-style Mods & Packs screen
    /// ([`Screen::ModManager`]). The host fills `entries`; `lang`/`ui_scale`
    /// are re-synced from this state on every [`render`](Self::render).
    pub mod_manager: crate::mod_manager::ModManagerState,
    /// The integrated recipe & resource browser
    /// ([`Screen::RecipeBrowser`]). The host fills `index`/`machine_tutorial`;
    /// `lang`/`ui_scale` are re-synced every frame.
    pub recipe_browser: crate::recipe_browser::RecipeBrowserState,

    /// Bounded top-right toast stack
    /// ([`crate::qol_helpers::ToastQueue`]); rendered by
    /// [`crate::layout::layout_toast_stack`] while
    /// `settings.qol.toast_notifications` is on.
    pub toasts: crate::qol_helpers::ToastQueue,
    /// Controls-page binding table the host syncs; the QoL Controls
    /// section audits it with
    /// [`crate::qol_helpers::detect_keybind_conflicts`].
    pub keybinds: crate::qol_helpers::KeyBindings,
    /// Opt-in auto-reconnect countdown state; the host ticks it and
    /// dials [`crate::qol_helpers::AutoReconnectController::dial_target`].
    pub auto_reconnect: crate::qol_helpers::AutoReconnectController,
    /// Chat scroll-lock: while the player reads older chat, incoming
    /// lines count unread instead of yanking the view to the bottom.
    pub chat_scroll: crate::qol_helpers::ChatScrollLockState,
    /// Vertical scroll offsets for scrollable menu screens.
    pub menu_scroll: MenuScrollState,

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
            translations: HashMap::new(),
            creative_inventory: crate::creative_screen::CreativeInventoryState::default(),
            bed_sleep_overlay: None,
            book_editor: None,
            end_credits: None,
            screen: Screen::Title,
            nav_stack: Vec::new(),
            settings: UiSettings::default(),
            capturing_key_binding: None,
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
            mod_sync_prompt: ModSyncPromptState::default(),
            mod_sync_decision: None,
            hud: HudState::default(),
            sign_editor: None,
            claims_ui: None,
            input_mode: crate::gamepad::InputModeTracker::default(),
            last_mouse: None,
            death: crate::death_screen::DeathScreenState::default(),
            title_status: None,
            title_panorama_active: false,
            singleplayer: SingleplayerState::default(),
            multiplayer: MultiplayerScreenState::default(),
            friends: FriendsScreenState::default(),
            disconnected: DisconnectedScreenState::default(),
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
            mod_manager: crate::mod_manager::ModManagerState::default(),
            recipe_browser: crate::recipe_browser::RecipeBrowserState::default(),
            toasts: crate::qol_helpers::ToastQueue::default(),
            keybinds: crate::qol_helpers::KeyBindings::vanilla_defaults(),
            auto_reconnect: crate::qol_helpers::AutoReconnectController::default(),
            chat_scroll: crate::qol_helpers::ChatScrollLockState::default(),
            menu_scroll: MenuScrollState::default(),
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
    pub fn set_translations(&mut self, map: HashMap<String, String>) { self.translations = map; }

    pub fn translate<'a>(&'a self, key: &str, fallback: &'a str) -> &'a str {
        self.translations.get(key).map(String::as_str).unwrap_or(fallback)
    }

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

    /// Detect the OS locale and apply it as the player's language
    /// preference. The coarse En/Ja projection resolves through
    /// [`crate::qol_helpers::detect_system_language_from_env`]
    /// (`LC_ALL`/`LC_MESSAGES`/`LANG`/`LANGUAGE`, including
    /// Windows-style names like `Japanese_Japan.932` that the core
    /// catalog parser cannot read), while the full 16-locale `locale`
    /// field resolves through `megu_core::i18n`. Reports `Auto` in the
    /// legacy field because the choice was automatic.
    pub fn detect_and_apply_os_locale(&mut self) {
        let mut detected = megu_core::i18n::detect_os_locale();
        // A Japanese system language always wins `ja_jp` — the QoL
        // parser accepts Windows-style names (`Japanese_Japan.932`,
        // bare `japanese`) the stricter catalog parser falls back on.
        if detected != BuiltinLocale::JaJp
            && crate::qol_helpers::detect_system_language_from_env() == UiLang::Ja
        {
            detected = BuiltinLocale::JaJp;
        }
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
        let prior = std::mem::replace(&mut self.settings, self.settings_tx.opened_snapshot.clone());
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

    /// Queue a toast for the top-right stack — a no-op while
    /// `settings.qol.toast_notifications` is off, so hosts can report
    /// events unconditionally.
    pub fn push_toast(&mut self, kind: crate::qol_helpers::ToastKind, title: &str, body: &str) {
        if self.settings.qol.toast_notifications {
            self.toasts.push(kind, title, body);
        }
    }

    /// Record a newly arrived chat line under the scroll-lock contract:
    /// locked readers keep their place (the unread badge ticks up),
    /// bottom-pinned readers are untouched.
    pub fn on_chat_message(&mut self) {
        if self.settings.qol.chat_scroll_lock {
            self.chat_scroll.on_new_message();
        }
    }

    /// Buffer-reusing [`render`](Self::render): `out` is cleared and
    /// refilled (its `Text` payload buffers are recycled, so steady-state
    /// frames emit no text allocations), and `self.hit_regions` is rebuilt
    /// in place keeping its capacity.
    pub fn render_into(&mut self, width: u32, height: u32, out: &mut crate::draw::UiDrawList) {
        self.render_into_with_gui_atlas(width, height, out, None);
    }

    /// Buffer-reusing render with the host GUI atlas threaded into HUD
    /// sprite binding. Passing `None` is identical to [`Self::render_into`].
    pub fn render_into_with_gui_atlas(
        &mut self,
        width: u32,
        height: u32,
        out: &mut crate::draw::UiDrawList,
        gui_atlas: Option<&crate::draw::GuiSpriteAtlas>,
    ) {
        self.render_w = width;
        self.render_h = height;
        // The embedded screens own their own language/scale mirrors so
        // they can also run standalone; when routed through `UiState`
        // this state is the source of truth.
        let lang = self.lang();
        self.mod_manager.lang = lang;
        self.mod_manager.ui_scale = self.ui_scale;
        self.settings.plamo_auto_translation_pack = self.mod_manager.plamo_panel.auto_pack_enabled;
        self.mod_manager.sync_builtin_modules(
            self.settings.qol.recipe_overlay_enabled,
            self.settings.inventory_drag_controls,
            self.settings.qol.compact_debug_hud.enabled,
            self.settings.qol.quick_stack_nearby && self.settings.qol.auto_replenish,
            self.mod_manager.plamo_panel.auto_pack_enabled,
        );
        self.recipe_browser.lang = lang;
        self.recipe_browser.ui_scale = self.ui_scale;
        self.recipe_browser.active_locale = self.settings.locale.code().to_ascii_lowercase();
        let mut regions = std::mem::take(&mut self.hit_regions);
        layout::layout_screen_into_with_gui_atlas(self, width, height, out, &mut regions, gui_atlas);
        if self.screen == Screen::InGameHud {
            layout::layout_subtitle_hud(self, &self.hud.subtitle_hud, width, height, out);
            layout::layout_scoreboard_sidebar(self, width, height, out);
            layout::layout_title_overlay(self, width, height, out);
            layout::layout_looking_at(self, width, height, out);
            layout::layout_debug_overlay(self, width, height, out);
        }
        // The sign-edit session composites over whatever screen is up —
        // the server owns its open/close lifecycle, not the nav stack.
        if let Some(done_rect) = layout::layout_sign_editor_region(self, width, height, out) {
            regions.push(HitRegion::new(WidgetId::SignEditorDone, done_rect));
        }
        // The claim-map overlay composites over everything else too —
        // the host owns its open/close lifecycle (map keybind, sync loss).
        if let Some(claims) = &self.claims_ui {
            let (list, _regions) = claims.render(width, height);
            out.commands.extend(list.commands);
        }
        if let Some(overlay) = &self.bed_sleep_overlay { overlay.render(width, height, out); }
        if let Some(editor) = &self.book_editor { editor.render(width, height, out); }
        if let Some(credits) = &self.end_credits { credits.render(width, height, out); }
        layout::layout_toast_stack(self, width, height, out);
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

    /// Put the death screen up with `state`: the nav stack is dropped
    /// because death is a leaf the host alone resolves (vanilla's
    /// `DeathScreen` has no back path), and `Escape` stays inert.
    pub fn show_death(&mut self, state: crate::death_screen::DeathScreenState) {
        self.death = state;
        self.nav_stack.clear();
        self.screen = Screen::Death;
        self.modal = None;
        self.focus_index = None;
    }

    /// Refresh the displayed death data while [`Screen::Death`] is up
    /// (e.g. the server-side cooldown ticked or the cause packet landed
    /// a frame after `set_health(0)` did).
    pub fn update_death(&mut self, state: crate::death_screen::DeathScreenState) {
        self.death = state;
    }

    /// Put the disconnected screen up with `state`: like death, the nav
    /// stack is dropped because the connection is already gone — only
    /// the back button (or Escape, landing on Title) leaves the screen.
    pub fn show_disconnected(&mut self, state: DisconnectedScreenState) {
        self.disconnected = state;
        self.nav_stack.clear();
        self.screen = Screen::Disconnected;
        self.modal = None;
        self.focus_index = None;
    }

    /// Open the explicit mod mismatch recovery screen and discard stale navigation.
    pub fn open_mod_sync_prompt(&mut self, state: ModSyncPromptState) {
        self.mod_sync_prompt = state;
        self.nav_stack.clear();
        self.screen = Screen::ModSyncPrompt;
        self.modal = None;
        self.focus_index = None;
    }

    /// Clear the death state after the server's respawn/health recovery;
    /// lands back on [`Screen::InGameHud`] when the death screen was up.
    pub fn clear_death(&mut self) {
        self.death = crate::death_screen::DeathScreenState::default();
        if self.screen == Screen::Death {
            self.screen = Screen::InGameHud;
            self.focus_index = None;
        }
    }

    /// Open the integrated Mods & Packs screen — the Title
    /// `"Mods & Packs"` button, the Settings `"Mods"` button, or a host
    /// hotkey. Pushes the nav stack so Back returns to the caller's
    /// screen; `lang`/`ui_scale` are copied across immediately (and kept
    /// in sync on every later render).
    pub fn open_mod_manager(&mut self) -> Vec<UiAction> {
        self.mod_manager.lang = self.lang();
        self.mod_manager.ui_scale = self.ui_scale;
        self.navigate(Screen::ModManager);
        vec![UiAction::OpenScreen(Screen::ModManager)]
    }

    /// Open the integrated recipe & resource browser — the Pause
    /// `"Recipe Browser"` button or a host hotkey (`R`/`U` over an item
    /// is a host-side binding). Pushes the nav stack so Back returns to
    /// the caller's screen.
    pub fn open_recipe_browser(&mut self) -> Vec<UiAction> {
        self.recipe_browser.lang = self.lang();
        self.recipe_browser.ui_scale = self.ui_scale;
        self.recipe_browser.active_locale = self.settings.locale.code().to_ascii_lowercase();
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

    /// `true` while the Singleplayer world-name field owns typing.
    pub fn world_name_field_active(&self) -> bool {
        self.singleplayer.name_focused
    }

    /// `true` while the Singleplayer seed field owns typing.
    pub fn world_seed_field_active(&self) -> bool {
        self.singleplayer.seed_focused
    }

    /// `true` while the Multiplayer direct-connect field owns typing.
    pub fn server_address_field_active(&self) -> bool {
        self.multiplayer.address_focused
    }

    /// The displayed invite code text, or `"-"` when none is active.
    pub fn invite_display(&self) -> String {
        self.active_invite
            .map(invite_code_display)
            .unwrap_or_else(|| "-".to_string())
    }

    // -- input ---------------------------------------------------------

    /// Scroll the active menu screen by signed logical pixels. Positive
    /// values move downward. Hosts without a device-agnostic wheel event
    /// can call this directly from their native wheel input.
    pub fn scroll_menu(&mut self, delta: i32) {
        let scale = self.effective_scale(self.render_h.max(1));
        let logical_w = if self.render_w == 0 { 320 } else { (self.render_w / scale) as i32 };
        let logical_h = if self.render_h == 0 { 240 } else { (self.render_h / scale) as i32 };
        let limit = self.active_menu_scroll_limit(logical_w, logical_h) as i32;
        if let Some(slot) = self.active_menu_scroll_mut() {
            *slot = (*slot as i32 + delta).clamp(0, limit) as usize;
        }
    }

    fn active_menu_scroll_mut(&mut self) -> Option<&mut usize> {
        match self.screen {
            Screen::Settings(tab) => Some(match tab {
                SettingsTab::Video => &mut self.menu_scroll.settings_video,
                SettingsTab::Audio => &mut self.menu_scroll.settings_audio,
                SettingsTab::Shaders => &mut self.menu_scroll.settings_shaders,
                SettingsTab::Qol => &mut self.menu_scroll.settings_qol,
                SettingsTab::FairPlay => &mut self.menu_scroll.settings_fairplay,
                SettingsTab::Language => &mut self.menu_scroll.settings_language,
                SettingsTab::Controls => &mut self.menu_scroll.settings_controls,
            }),
            Screen::Singleplayer => {
                if self.singleplayer.create_expanded() {
                    Some(&mut self.menu_scroll.singleplayer_create)
                } else {
                    None
                }
            }
            Screen::Friends => Some(&mut self.menu_scroll.friends),
            _ => None,
        }
    }

    /// Conservative content-overflow limit for the active menu screen.
    /// Layout uses the same helper so input and hit regions agree.
    pub fn active_menu_scroll_limit(&self, logical_w: i32, logical_h: i32) -> usize {
        // Limits are measured from the same un-scrolled bottom edge the
        // layout painter uses, minus the fixed footer's top (`h - 32`).
        // This avoids both dead scrolling (too small) and over-scrolling
        // the bottom-most control above the content viewport (too large).
        let footer_top = logical_h - 32;
        let row_step = if logical_h < 300 { 22 } else { 24 };
        let bottom = match self.screen {
            Screen::Settings(SettingsTab::Video) => 56 + 9 * row_step + 10,
            Screen::Settings(SettingsTab::Audio) => 56 + 8 * 24 + 10,
            Screen::Settings(SettingsTab::Shaders) => {
                let base = if logical_h < 300 { 76 } else { 82 };
                base + 92 + 20
            }
            Screen::Settings(SettingsTab::Qol) => {
                let compact = logical_h < 400;
                let (step, bh) = if compact { (18, 16) } else { (24, 20) };
                let base = if logical_h < 300 { 76 } else { 82 } - if compact { 2 } else { 0 };
                let last_row = if self.settings.qol.compact_debug_hud.enabled { 13 } else { 10 };
                base + last_row * step + bh
            }
            Screen::Settings(SettingsTab::FairPlay) => {
                let base = if logical_h < 300 { 76 } else { 82 };
                base + 5 * row_step + 12
            }
            Screen::Settings(SettingsTab::Language) => 56 + 2 * row_step + 12,
            Screen::Settings(SettingsTab::Controls) => {
                let base = if logical_h < 300 { 76 } else { 82 };
                base + 9 * 22 + 18 + usize::from(self.capturing_key_binding.is_some()) as i32 * 26
            }
            Screen::Singleplayer if self.singleplayer.create_expanded() => {
                // Mirror layout.rs: the create block is anchored above the
                // footer (`band_top - 104`, minimum y=36) and is 96px tall.
                let band_top = logical_h - 44;
                let anchor = (band_top - 104).max(36);
                return (anchor + 96 - band_top).max(0) as usize;
            }
            Screen::Friends => {
                if logical_w < 360 { 438 } else { 292 }
            }
            _ => 0,
        };
        (bottom - footer_top).max(0) as usize
    }

    /// One non-redundant menu back transition. `UiAction::Back` remains
    /// in the returned action stream for embedders built against the
    /// original contract.
    fn back_action(&mut self) -> Vec<UiAction> {
        self.back();
        vec![UiAction::Back]
    }

    /// Route one input event; returns the actions the app must perform.
    pub fn handle_input(&mut self, ev: UiInputEvent) -> Vec<UiAction> {
        // Input-source tracking runs first so prompt glyphs switch the
        // instant the device family changes (whichever produced the last
        // event wins; small mouse jitter never flips the mode).
        match &ev {
            UiInputEvent::MouseMove { x, y } => {
                let (dx, dy) = self
                    .last_mouse
                    .map_or((0.0, 0.0), |(lx, ly)| {
                        ((*x - lx) as f32, (*y - ly) as f32)
                    });
                self.last_mouse = Some((*x, *y));
                self.input_mode.on_mouse_motion(dx, dy);
            }
            UiInputEvent::Click { .. } => {
                self.input_mode.on_mouse_button();
            }
            UiInputEvent::KeyPressed(_) | UiInputEvent::CharTyped(_) => {
                self.input_mode.on_keyboard_input();
            }
            UiInputEvent::GamepadPressed(_) => {
                self.input_mode.on_gamepad_input();
            }
        }
        // The claim-map overlay owns all input while open — its actions
        // translate to [`UiAction::Claims`] (or close internally).
        if self.claims_ui.is_some() {
            let actions = self
                .claims_ui
                .as_mut()
                .map(|claims| claims.handle_input(ev, self.render_w, self.render_h))
                .unwrap_or_default();
            return self.apply_claims_actions(actions);
        }
        // Embedded screens own their hit-testing, search focus, and
        // scroll state — while one is up (and no modal blocks it) the
        // raw event is forwarded verbatim and its actions are translated
        // back into [`UiAction`]s.
        if self.modal.is_none() {
            match self.screen {
                Screen::ModManager => return self.forward_to_mod_manager(ev),
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
            UiInputEvent::KeyPressed(UiKey::Escape)
                if self.capturing_key_binding.take().is_some() =>
            {
                Vec::new()
            }
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

    /// Bridge from the [`crate::gamepad`] engine: feed one physical
    /// button press edge. The press always flips
    /// [`UiState::input_mode`] to gamepad; buttons with a UI navigation
    /// role ([`nav_button`]) are then routed as
    /// [`UiInputEvent::GamepadPressed`] and produce their usual actions.
    /// `Start` still opens the pause/back path — D-pad hold-repeat stays
    /// with the engine's own `FocusJumped` feed.
    pub fn on_gamepad_button(&mut self, btn: crate::gamepad::GamepadButton) -> Vec<UiAction> {
        self.input_mode.on_gamepad_input();
        nav_button(btn)
            .map(|nav| self.handle_input(UiInputEvent::GamepadPressed(nav)))
            .unwrap_or_default()
    }

    /// Map the claim-map overlay's actions onto [`UiAction`]s —
    /// `Close` is handled internally, everything else reaches the host.
    fn apply_claims_actions(
        &mut self,
        actions: Vec<crate::claims_ui::ClaimsUiAction>,
    ) -> Vec<UiAction> {
        let mut out = Vec::with_capacity(actions.len());
        for action in actions {
            if action == crate::claims_ui::ClaimsUiAction::Close {
                self.claims_ui = None;
            } else {
                out.push(UiAction::Claims(action));
            }
        }
        out
    }

    /// Forward `ev` into [`crate::mod_manager::ModManagerState`] and map its
    /// actions back onto [`UiAction`]. Clicks on native chrome (any
    /// non-`ModManager` region) are routed by [`handle_click`](Self::handle_click)
    /// instead — everything else reaches the sub-screen untouched.
    fn forward_to_mod_manager(&mut self, ev: UiInputEvent) -> Vec<UiAction> {
        match ev {
            UiInputEvent::Click { x, y } => {
                if let Some(region) = self.hit_regions.iter().find(|r| r.contains(x, y)).copied() {
                    if !matches!(region.id, WidgetId::ModManager(_)) {
                        return self.handle_click(x, y);
                    }
                    if matches!(
                        region.id,
                        WidgetId::ModManager(crate::mod_manager::ModManagerWidgetId::BuiltinToggle(_))
                            | WidgetId::ModManager(crate::mod_manager::ModManagerWidgetId::BuiltinOpenSettings(_))
                    ) {
                        return self.activate(region.id, x, y, region.rect);
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
        let acts = self.mod_manager.handle_input(ev, self.render_w, self.render_h);
        let mut out = Vec::with_capacity(acts.len());
        for action in acts {
            match action {
                crate::mod_manager::ModManagerAction::Close => {
                    self.back();
                    out.push(UiAction::Back);
                }
                crate::mod_manager::ModManagerAction::OpenUrl(url) => {
                    out.push(UiAction::OpenUrl(url));
                }
                other => out.push(UiAction::ModManager(other)),
            }
        }
        out
    }

    /// Recipe-browser counterpart of [`forward_to_mod_manager`](Self::forward_to_mod_manager).
    fn forward_to_recipe_browser(&mut self, ev: UiInputEvent) -> Vec<UiAction> {
        match ev {
            UiInputEvent::Click { x, y } => {
                if let Some(region) = self.hit_regions.iter().find(|r| r.contains(x, y)).copied() {
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

    /// `true` while any editable text field owns typing.
    fn any_field_focused(&self) -> bool {
        self.fps_field_focused
            || self.join_field_focused
            || self.singleplayer.name_focused
            || self.singleplayer.seed_focused
            || self.multiplayer.address_focused
            || self.friends.name_focused
            || self.friends.id_focused
            || self.friends.relay_focused
    }

    /// Scroll a menu when keyboard focus reaches the edge of the visible
    /// content viewport. Returns `true` when the key was consumed purely
    /// as a scroll step; after the next render the newly revealed control
    /// is focusable, which gives vanilla-style focus-scroll-into-view.
    fn keyboard_edge_scroll(&mut self, dir: i32) -> bool {
        let scale_u = self.effective_scale(self.render_h.max(1));
        let scale = scale_u as i32;
        let logical_h = if self.render_h == 0 { 240 } else { (self.render_h / scale_u) as i32 };
        let logical_w = if self.render_w == 0 { 320 } else { (self.render_w / scale_u) as i32 };
        let limit = self.active_menu_scroll_limit(logical_w, logical_h) as i32;
        let Some(focus) = self.focus_index.and_then(|i| self.hit_regions.get(i).copied()) else {
            return false;
        };
        let Some(slot) = self.active_menu_scroll_mut() else {
            return false;
        };
        let y = focus.rect.y / scale;
        let bottom = focus.rect.bottom() / scale;
        let step = 24;
        if dir > 0 && *slot < limit as usize && bottom >= logical_h - 42 {
            *slot = (*slot as i32 + step).min(limit) as usize;
            return true;
        }
        if dir < 0 && *slot > 0 && y <= 66 {
            *slot = slot.saturating_sub(step as usize);
            return true;
        }
        false
    }

    fn handle_key(&mut self, key: UiKey) -> Vec<UiAction> {
        // While a text field owns typing, most keys feed the field.
        if self.any_field_focused() {
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
                if !self.keyboard_edge_scroll(-1) {
                    self.focus_step(-1);
                }
                Vec::new()
            }
            UiKey::Down => {
                if !self.keyboard_edge_scroll(1) {
                    self.focus_step(1);
                }
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
                } else if self.singleplayer.name_focused {
                    self.singleplayer.new_name.pop();
                } else if self.singleplayer.seed_focused {
                    self.singleplayer.seed_text.pop();
                } else if self.multiplayer.address_focused {
                    self.multiplayer.address_text.pop();
                    self.multiplayer.status = None;
                } else if self.friends.name_focused {
                    self.friends.add_name_text.pop();
                    self.friends.add_error = None;
                } else if self.friends.id_focused {
                    self.friends.add_id_text.pop();
                    self.friends.add_error = None;
                } else if self.friends.relay_focused {
                    self.friends.relay_addr_text.pop();
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
            && !self.any_field_focused()
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
        } else if self.join_field_focused {
            if (c.is_ascii_alphanumeric() || c == '-' || c == ':')
                && self.join_code_text.len() < JOIN_FIELD_MAX_LEN
            {
                self.join_code_text.push(c.to_ascii_uppercase());
                self.join_error = false;
            }
        } else if self.friends.name_focused {
            if !c.is_control()
                && self.friends.add_name_text.chars().count() < FRIEND_NAME_FIELD_MAX_LEN
            {
                self.friends.add_name_text.push(c);
                self.friends.add_error = None;
            }
        } else if self.friends.id_focused {
            if (c.is_ascii_alphanumeric() || matches!(c, ':' | '-' | ' '))
                && self.friends.add_id_text.len() < FRIEND_ID_FIELD_MAX_LEN
            {
                self.friends.add_id_text.push(c.to_ascii_uppercase());
                self.friends.add_error = None;
            }
        } else if self.friends.relay_focused {
            if (c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | ':' | '[' | ']'))
                && self.friends.relay_addr_text.chars().count() < RELAY_FIELD_MAX_LEN
            {
                self.friends.relay_addr_text.push(c);
            }
        } else if self.singleplayer.name_focused {
            if !c.is_control()
                && self.singleplayer.new_name.chars().count() < WORLD_NAME_FIELD_MAX_LEN
            {
                self.singleplayer.new_name.push(c);
            }
        } else if self.singleplayer.seed_focused
            && !c.is_control()
            && self.singleplayer.seed_text.chars().count() < WORLD_SEED_FIELD_MAX_LEN
        {
            self.singleplayer.seed_text.push(c);
        } else if self.multiplayer.address_focused
            && (c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | ':' | '[' | ']'))
            && self.multiplayer.address_text.chars().count() < SERVER_ADDRESS_FIELD_MAX_LEN
        {
            self.multiplayer.address_text.push(c);
            self.multiplayer.status = None;
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
        } else if self.singleplayer.name_focused {
            // Form flow: Enter in the name field steps down to the seed
            // field instead of launching half-typed input.
            self.singleplayer.name_focused = false;
            self.singleplayer.seed_focused = true;
            Vec::new()
        } else if self.singleplayer.seed_focused {
            self.unfocus_fields();
            self.try_create_world()
        } else if self.multiplayer.address_focused {
            self.unfocus_fields();
            self.try_join_server()
        } else if self.friends.name_focused {
            // Form flow: Enter in the name field steps down to the
            // `megu-id:` field instead of adding half-typed input.
            self.friends.name_focused = false;
            self.friends.id_focused = true;
            Vec::new()
        } else if self.friends.id_focused {
            self.unfocus_fields();
            self.try_add_friend()
        } else if self.friends.relay_focused {
            self.unfocus_fields();
            vec![UiAction::SaveRelayAddr {
                addr: self.friends.relay_addr_text.trim().to_string(),
            }]
        } else {
            Vec::new()
        }
    }

    fn try_join(&mut self) -> Vec<UiAction> {
        let text = self.join_code_text.trim();
        // A pasted `megu-friend:` guest link takes the 128-bit-token join
        // path; the host re-parses it (fail-closed) before connecting.
        if text.to_ascii_lowercase().starts_with("megu-friend:") {
            self.join_error = false;
            return vec![UiAction::JoinGuestLink {
                link: text.to_string(),
            }];
        }
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

    /// Friends `[Add]` (or Enter on the `megu-id:` field): emits the
    /// pairing request; the host validates fail-closed (bad checksum,
    /// duplicate, self-key) and reports through `friends.add_error`.
    fn try_add_friend(&mut self) -> Vec<UiAction> {
        let name = self.friends.add_name_text.trim();
        let friend_id = self.friends.add_id_text.trim();
        if friend_id.is_empty() {
            self.friends.add_error = Some(
                tr(
                    self.lang(),
                    "Paste a megu-id: to add",
                    "追加する megu-id: を貼り付けてください",
                )
                .to_string(),
            );
            return Vec::new();
        }
        let name = if name.is_empty() {
            // Unnamed friends display as their truncated id until the
            // player renames them (no rename UI yet).
            friend_id.chars().take(16).collect::<String>()
        } else {
            name.to_string()
        };
        self.friends.add_error = None;
        vec![UiAction::AddFriend {
            name,
            friend_id: friend_id.to_string(),
        }]
    }

    /// "Create & Play" (or Enter on the seed field): the name the field
    /// shows trimmed — an empty field falls back to the `"New World"`
    /// default — plus the parsed seed. The host re-checks the name with
    /// `validate_world_name` and reports failures through the visible
    /// `title_status` line, so nothing here fails silently.
    fn try_create_world(&mut self) -> Vec<UiAction> {
        let trimmed = self.singleplayer.new_name.trim();
        let name = if trimmed.is_empty() {
            "New World".to_string()
        } else {
            trimmed.to_string()
        };
        vec![UiAction::PlayWorld {
            name,
            seed: seed_from_text(&self.singleplayer.seed_text),
        }]
    }

    /// Read the create-and-play payload that corresponds to the current
    /// Singleplayer form. Hosts use this together with
    /// `UiAction::PlayWorld` so game-mode/difficulty survive the stable
    /// cross-crate action contract.
    pub fn create_world_payload(&self) -> CreateWorldPayload {
        let trimmed = self.singleplayer.new_name.trim();
        CreateWorldPayload {
            name: if trimmed.is_empty() { "New World".to_string() } else { trimmed.to_string() },
            seed: seed_from_text(&self.singleplayer.seed_text),
            game_mode: self.singleplayer.game_mode,
            difficulty: self.singleplayer.difficulty,
        }
    }

    /// Multiplayer Join (or Enter on the direct-connect field): the
    /// typed `host[:port]` wins over the highlighted row, matching
    /// vanilla's "type an address, press Join" flow. Bad input lands on
    /// the inline status banner instead of failing silently.
    fn try_join_server(&mut self) -> Vec<UiAction> {
        let direct = self.multiplayer.address_text.trim();
        if !direct.is_empty() {
            return match parse_server_address(direct) {
                Some((address, port)) => {
                    self.multiplayer.status = None;
                    vec![UiAction::JoinServer { address, port }]
                }
                None => {
                    self.multiplayer.status = Some(
                        tr(
                            self.lang(),
                            "Invalid server address",
                            "サーバーアドレスが無効です",
                        )
                        .to_string(),
                    );
                    Vec::new()
                }
            };
        }
        match self
            .multiplayer
            .selected
            .and_then(|i| self.multiplayer.servers.get(i))
        {
            Some(entry) => {
                self.multiplayer.status = None;
                vec![UiAction::JoinServer {
                    address: entry.address.clone(),
                    port: entry.port,
                }]
            }
            None => {
                self.multiplayer.status = Some(
                    tr(
                        self.lang(),
                        "Select a server or enter an address",
                        "サーバーかアドレスを入力してください",
                    )
                    .to_string(),
                );
                Vec::new()
            }
        }
    }

    /// Multiplayer Add: persist the typed `host[:port]` through the
    /// host's store (`name` mirrors the typed text until a rename UI
    /// exists — the row stays honest about what it connects to).
    fn try_add_server(&mut self) -> Vec<UiAction> {
        let direct = self.multiplayer.address_text.trim();
        match parse_server_address(direct) {
            Some((address, port)) => {
                self.multiplayer.status = None;
                vec![UiAction::SaveServer {
                    name: direct.to_string(),
                    address,
                    port,
                }]
            }
            None => {
                self.multiplayer.status = Some(
                    tr(
                        self.lang(),
                        "Invalid server address",
                        "サーバーアドレスが無効です",
                    )
                    .to_string(),
                );
                Vec::new()
            }
        }
    }

    fn unfocus_fields(&mut self) {
        self.fps_field_focused = false;
        self.join_field_focused = false;
        self.singleplayer.name_focused = false;
        self.singleplayer.seed_focused = false;
        self.multiplayer.address_focused = false;
        self.friends.name_focused = false;
        self.friends.id_focused = false;
        self.friends.relay_focused = false;
    }

    fn escape(&mut self) -> Vec<UiAction> {
        if self.modal.take().is_some() {
            return Vec::new(); // modal dismissed only
        }
        match self.screen {
            // Death is absorbing: vanilla's DeathScreen ignores Escape —
            // only the Respawn/Title buttons (or server recovery) leave it.
            Screen::Title | Screen::CrashRecovery | Screen::Death => Vec::new(),
            Screen::InGameHud => {
                self.navigate(Screen::Pause);
                vec![UiAction::OpenScreen(Screen::Pause)]
            }
            Screen::Pause => {
                self.back_action()
            }
            _ => {
                self.back_action()
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
            WidgetId::TitleSingleplayer => {
                self.singleplayer.scroll = 0;
                self.navigate(Screen::Singleplayer);
                vec![UiAction::OpenScreen(Screen::Singleplayer)]
            }
            WidgetId::TitleMultiplayer => {
                self.multiplayer.scroll = 0;
                self.multiplayer.status = None;
                self.navigate(Screen::Multiplayer);
                vec![UiAction::OpenScreen(Screen::Multiplayer)]
            }
            WidgetId::TitleFriends => {
                self.navigate(Screen::Friends);
                vec![UiAction::OpenScreen(Screen::Friends)]
            }
            WidgetId::TitleMods => self.open_mod_manager(),
            WidgetId::TitleSettings => {
                let s = Screen::Settings(SettingsTab::Video);
                self.navigate(s);
                vec![UiAction::OpenScreen(s)]
            }
            WidgetId::TitleQuit => vec![UiAction::QuitGame],
            WidgetId::TitleLanguage => {
                // Vanilla-globe 1-click flip between the two resolved UI
                // languages.
                let next = if self.lang() == UiLang::Ja {
                    BuiltinLocale::EnUs
                } else {
                    BuiltinLocale::JaJp
                };
                self.set_locale(next);
                vec![UiAction::SettingsChanged]
            }
            WidgetId::TitleAbout => {
                self.navigate(Screen::AboutMegu);
                vec![UiAction::OpenScreen(Screen::AboutMegu)]
            }

            WidgetId::SingleplayerWorld(i) => match self.singleplayer.worlds.get(i) {
                Some(name) => vec![UiAction::PlayWorld {
                    name: name.clone(),
                    seed: None,
                }],
                None => Vec::new(),
            },
            WidgetId::SingleplayerNameField => {
                self.unfocus_fields();
                self.singleplayer.name_focused = true;
                Vec::new()
            }
            WidgetId::SingleplayerSeedField => {
                self.unfocus_fields();
                self.singleplayer.seed_focused = true;
                Vec::new()
            }
            WidgetId::SingleplayerCreate => self.try_create_world(),
            WidgetId::SingleplayerGameMode => {
                self.singleplayer.game_mode = match self.singleplayer.game_mode { SingleplayerGameMode::Survival => SingleplayerGameMode::Creative, SingleplayerGameMode::Creative => SingleplayerGameMode::Hardcore, SingleplayerGameMode::Hardcore => SingleplayerGameMode::Survival };
                Vec::new()
            }
            WidgetId::SingleplayerDifficulty => {
                self.singleplayer.difficulty = match self.singleplayer.difficulty { SingleplayerDifficulty::Peaceful => SingleplayerDifficulty::Easy, SingleplayerDifficulty::Easy => SingleplayerDifficulty::Normal, SingleplayerDifficulty::Normal => SingleplayerDifficulty::Hard, SingleplayerDifficulty::Hard => SingleplayerDifficulty::Peaceful };
                Vec::new()
            }
            WidgetId::SingleplayerBack => {
                self.back_action()
            }
            WidgetId::SingleplayerListUp => {
                self.singleplayer.scroll = self.singleplayer.scroll.saturating_sub(1);
                Vec::new()
            }
            WidgetId::SingleplayerListDown => {
                let max_scroll = self.singleplayer.worlds.len().saturating_sub(1);
                self.singleplayer.scroll = (self.singleplayer.scroll + 1).min(max_scroll);
                Vec::new()
            }

            WidgetId::MultiplayerServer(i) => {
                self.multiplayer.selected = (i < self.multiplayer.servers.len()).then_some(i);
                self.multiplayer.status = None;
                Vec::new()
            }
            WidgetId::MultiplayerAddressField => {
                self.unfocus_fields();
                self.multiplayer.address_focused = true;
                Vec::new()
            }
            WidgetId::MultiplayerJoin => self.try_join_server(),
            WidgetId::MultiplayerAdd => self.try_add_server(),
            WidgetId::MultiplayerDelete => match self.multiplayer.selected {
                Some(index) if index < self.multiplayer.servers.len() => {
                    vec![UiAction::DeleteServer { index }]
                }
                _ => {
                    self.multiplayer.status = Some(
                        tr(
                            self.lang(),
                            "Select a server to delete",
                            "削除するサーバーを選択してください",
                        )
                        .to_string(),
                    );
                    Vec::new()
                }
            },
            WidgetId::MultiplayerListUp => {
                self.multiplayer.scroll = self.multiplayer.scroll.saturating_sub(1);
                Vec::new()
            }
            WidgetId::MultiplayerListDown => {
                let max_scroll = self.multiplayer.servers.len().saturating_sub(1);
                self.multiplayer.scroll = (self.multiplayer.scroll + 1).min(max_scroll);
                Vec::new()
            }
            WidgetId::MultiplayerFriendWorld(index) => {
                if index < self.multiplayer.friend_worlds.len() {
                    vec![UiAction::JoinFriendWorld { index }]
                } else {
                    Vec::new()
                }
            }
            WidgetId::MultiplayerLanWorld(index) => {
                match self.multiplayer.lan_worlds.get(index) {
                    Some(entry) => vec![UiAction::JoinServer {
                        address: entry.address.clone(),
                        port: entry.port,
                    }],
                    None => Vec::new(),
                }
            }
            WidgetId::MultiplayerFriendLanUp => {
                self.multiplayer.friend_lan_scroll =
                    self.multiplayer.friend_lan_scroll.saturating_sub(1);
                Vec::new()
            }
            WidgetId::MultiplayerFriendLanDown => {
                let total =
                    self.multiplayer.friend_worlds.len() + self.multiplayer.lan_worlds.len();
                let max_scroll = total.saturating_sub(1);
                self.multiplayer.friend_lan_scroll =
                    (self.multiplayer.friend_lan_scroll + 1).min(max_scroll);
                Vec::new()
            }
            WidgetId::MultiplayerManageFriends => {
                self.navigate(Screen::Friends);
                vec![UiAction::OpenScreen(Screen::Friends)]
            }
            WidgetId::MultiplayerBack => {
                self.back_action()
            }
            WidgetId::DisconnectedBack => {
                self.go_title();
                vec![UiAction::OpenScreen(Screen::Title)]
            }
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
                self.back_action()
            }
            WidgetId::SettingsAboutMegu => {
                self.navigate(Screen::AboutMegu);
                vec![UiAction::OpenScreen(Screen::AboutMegu)]
            }
            WidgetId::SettingsMods => self.open_mod_manager(),
            WidgetId::AboutBack => {
                self.back_action()
            }
            WidgetId::TabVideo => self.select_tab(SettingsTab::Video),
            WidgetId::TabAudio => self.select_tab(SettingsTab::Audio),
            WidgetId::TabShaders => self.select_tab(SettingsTab::Shaders),
            WidgetId::TabQol => self.select_tab(SettingsTab::Qol),
            WidgetId::TabFairPlay => self.select_tab(SettingsTab::FairPlay),
            WidgetId::TabLanguage => self.select_tab(SettingsTab::Language),
            WidgetId::TabControls => self.select_tab(SettingsTab::Controls),
            WidgetId::ControlKey(index) => {
                self.capturing_key_binding = (index < 17).then_some(index);
                Vec::new()
            }
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
            WidgetId::VideoRamBudget => {
                // Map click x onto the 512-MiB stop grid (0 = Auto).
                let frac = ((x - rect.x) as f32 / rect.w.max(1) as f32).clamp(0.0, 1.0);
                let prior = self.settings.clone();
                self.settings.video.set_ram_budget_frac(frac);
                self.settings.video.preset = VideoPreset::Custom;
                if self.settings == prior {
                    Vec::new()
                } else {
                    self.push_settings_undo(prior);
                    vec![UiAction::SettingsChanged]
                }
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
                self.back_action()
            }
            WidgetId::FriendsCopyId => {
                if self.friends.my_friend_id.is_empty() {
                    Vec::new()
                } else {
                    vec![UiAction::CopyText(self.friends.my_friend_id.clone())]
                }
            }
            WidgetId::FriendsAddNameField => {
                self.unfocus_fields();
                self.friends.name_focused = true;
                Vec::new()
            }
            WidgetId::FriendsAddIdField => {
                self.unfocus_fields();
                self.friends.id_focused = true;
                Vec::new()
            }
            WidgetId::FriendsAdd => self.try_add_friend(),
            WidgetId::FriendRow(index) => {
                self.friends.selected = (index < self.friends.friends.len()).then_some(index);
                Vec::new()
            }
            WidgetId::FriendRemove(index) => match self.friends.friends.get(index) {
                Some(entry) => {
                    let friend_id = entry.friend_id.clone();
                    if self.friends.selected == Some(index) {
                        self.friends.selected = None;
                    }
                    vec![UiAction::RemoveFriend { friend_id }]
                }
                None => Vec::new(),
            },
            WidgetId::FriendsRelayField => {
                self.unfocus_fields();
                self.friends.relay_focused = true;
                Vec::new()
            }
            WidgetId::FriendsGenerateGuestLink => match self.friends.selected {
                Some(index) => match self.friends.friends.get(index) {
                    Some(entry) => vec![UiAction::GenerateGuestLink {
                        friend_id: entry.friend_id.clone(),
                    }],
                    None => {
                        self.friends.selected = None;
                        self.friends.status = Some(
                            tr(
                                self.lang(),
                                "Select a friend row first",
                                "先にフレンド行を選択してください",
                            )
                            .to_string(),
                        );
                        Vec::new()
                    }
                },
                None => {
                    self.friends.status = Some(
                        tr(
                            self.lang(),
                            "Select a friend row first",
                            "先にフレンド行を選択してください",
                        )
                        .to_string(),
                    );
                    Vec::new()
                }
            },
            WidgetId::FriendsCopyGuestLink => match self.friends.guest_link.clone() {
                Some(link) => vec![UiAction::CopyText(link)],
                None => Vec::new(),
            },
            WidgetId::FriendsListUp => {
                self.friends.scroll = self.friends.scroll.saturating_sub(1);
                Vec::new()
            }
            WidgetId::FriendsListDown => {
                let max_scroll = self.friends.friends.len().saturating_sub(1);
                self.friends.scroll = (self.friends.scroll + 1).min(max_scroll);
                Vec::new()
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
            WidgetId::ModSyncPromptConfirm => vec![UiAction::ConfirmModSyncDownload],
            WidgetId::ModSyncPromptCancel => {
                self.go_title();
                vec![UiAction::CancelModSync]
            }

            WidgetId::PauseBackToGame => {
                self.back_action()
            }
            WidgetId::PauseSettings => {
                let s = Screen::Settings(SettingsTab::Video);
                self.navigate(s);
                vec![UiAction::OpenScreen(s)]
            }
            WidgetId::PauseStatistics => vec![UiAction::ShowStatistics],
            WidgetId::PauseRecipeBrowser => self.open_recipe_browser(),
            WidgetId::PauseSaveReplay => vec![UiAction::SaveInstantReplay],
            WidgetId::PauseInvite => {
                self.navigate(Screen::Friends);
                vec![UiAction::OpenScreen(Screen::Friends)]
            }
            WidgetId::PauseSaveQuit => {
                self.go_title();
                // `LeaveWorld` tears the live session down before the
                // title screen lands — like `DeathTitleScreen`, leaving
                // world must drop the connection, not leak it.
                vec![UiAction::LeaveWorld]
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

            WidgetId::DeathRespawn => {
                // The layout only emits a live region when
                // `can_respawn()` holds; re-check here so a stale focus
                // activation can never send a hardcore/cooldown request.
                if self.death.can_respawn() {
                    vec![UiAction::PerformRespawn]
                } else {
                    Vec::new()
                }
            }
            WidgetId::DeathTitleScreen => {
                self.go_title();
                vec![UiAction::LeaveWorld]
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
                self.back_action()
            }
            WidgetId::SignEditorDone => vec![UiAction::CloseSignEditor],
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
        // Friends-screen controls own `self.friends`, not `self.settings` —
        // handle them before the settings-snapshot match below.
        match id {
            WidgetId::FriendsShareMode => {
                let next = self.friends.share_mode.step(dir);
                if next == self.friends.share_mode {
                    return Vec::new();
                }
                self.friends.share_mode = next;
                self.friends.status = None;
                return vec![UiAction::SetFriendShareMode(next)];
            }
            WidgetId::FriendSelected(index) => {
                return match self.friends.friends.get_mut(index) {
                    Some(entry) => {
                        entry.selected_for_share = !entry.selected_for_share;
                        vec![UiAction::SetFriendSelected {
                            friend_id: entry.friend_id.clone(),
                            selected: entry.selected_for_share,
                        }]
                    }
                    None => Vec::new(),
                };
            }
            _ => {}
        }
        match id {
            WidgetId::ModManager(crate::mod_manager::ModManagerWidgetId::BuiltinToggle(index)) => {
                let changed = match index {
                    0 => { self.settings.qol.recipe_overlay_enabled = !self.settings.qol.recipe_overlay_enabled; true }
                    1 => { self.settings.inventory_drag_controls = !self.settings.inventory_drag_controls; true }
                    2 => { self.settings.qol.compact_debug_hud.enabled = !self.settings.qol.compact_debug_hud.enabled; true }
                    3 => {
                        let enabled = !(self.settings.qol.quick_stack_nearby && self.settings.qol.auto_replenish);
                        self.settings.qol.quick_stack_nearby = enabled;
                        self.settings.qol.auto_replenish = enabled;
                        true
                    }
                    4 => {
                        let enabled = !self.mod_manager.plamo_panel.auto_pack_enabled;
                        self.mod_manager.plamo_panel.auto_pack_enabled = enabled;
                        self.settings.plamo_auto_translation_pack = enabled;
                        return vec![
                            UiAction::ModManager(crate::mod_manager::ModManagerAction::ToggleAutoTranslationPack(enabled)),
                            UiAction::SettingsChanged,
                        ];
                    }
                    _ => false,
                };
                return if changed { vec![UiAction::SettingsChanged] } else { Vec::new() };
            }
            WidgetId::ModManager(crate::mod_manager::ModManagerWidgetId::BuiltinOpenSettings(index)) => {
                let tab = match index {
                    0 | 1 | 3 => SettingsTab::Qol,
                    2 => SettingsTab::Video,
                    4 => SettingsTab::Language,
                    _ => SettingsTab::Qol,
                };
                let screen = Screen::Settings(tab);
                self.navigate(screen);
                return vec![UiAction::OpenScreen(screen)];
            }
            _ => {}
        }
        let prior = self.settings.clone();
        let v = &mut self.settings.video;
        let fullscreen = &mut self.settings.fullscreen;
        let mouse_invert_y = &mut self.settings.mouse_invert_y;
        let gamma_pct = &mut self.settings.gamma_pct;
        let gui_scale = &mut self.settings.gui_scale;
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
            WidgetId::VideoClouds => {
                v.clouds = v.clouds.next();
                v.preset = VideoPreset::Custom;
                true
            }
            WidgetId::VideoEntityShadows => {
                v.entity_shadows = !v.entity_shadows;
                v.preset = VideoPreset::Custom;
                true
            }
            WidgetId::VideoComfortExposure => {
                v.comfort_exposure = !v.comfort_exposure;
                v.preset = VideoPreset::Custom;
                true
            }
            WidgetId::VideoDynamicLights => {
                v.dynamic_lights = !v.dynamic_lights;
                v.preset = VideoPreset::Custom;
                true
            }
            WidgetId::VideoEnchantmentGlint => {
                v.enchantment_glint = !v.enchantment_glint;
                v.preset = VideoPreset::Custom;
                true
            }
            WidgetId::VideoIndirectLightBounce => {
                v.indirect_light_bounce = !v.indirect_light_bounce;
                v.preset = VideoPreset::Custom;
                true
            }
            WidgetId::VideoDirectionalShadowGameplay => {
                v.directional_shadow_gameplay = !v.directional_shadow_gameplay;
                v.preset = VideoPreset::Custom;
                true
            }
            WidgetId::VideoLazyCatchupGameplay => {
                v.lazy_catchup_gameplay = !v.lazy_catchup_gameplay;
                v.preset = VideoPreset::Custom;
                true
            }
            WidgetId::VideoRamBudget => {
                let next = v.ram_budget_step(dir);
                if next == v.ram_budget_mib {
                    false
                } else {
                    v.ram_budget_mib = next;
                    v.preset = VideoPreset::Custom;
                    true
                }
            }
            WidgetId::VideoFullscreen => {
                *fullscreen = !*fullscreen;
                true
            }
            WidgetId::VideoInvertY => {
                *mouse_invert_y = !*mouse_invert_y;
                true
            }
            WidgetId::VideoGamma => {
                *gamma_pct = (i32::from(*gamma_pct) + 5 * dir).clamp(50, 200) as u16;
                true
            }
            WidgetId::VideoGuiScale => {
                *gui_scale = (*gui_scale % 5 + 1) % 5;
                true
            }
            WidgetId::QolBuiltin(index) => match index {
                0 => {
                    self.settings.qol_looking_at_hud = !self.settings.qol_looking_at_hud;
                    true
                }
                1 => {
                    self.settings.inventory_drag_controls = !self.settings.inventory_drag_controls;
                    true
                }
                2 => {
                    self.settings.qol_inventory_sort = !self.settings.qol_inventory_sort;
                    true
                }
                3 => {
                    self.settings.zsh_command_bar = !self.settings.zsh_command_bar;
                    true
                }
                _ => false,
            },
            WidgetId::CompactDebugHudSegment(index) => {
                if index >= 9 {
                    false
                } else {
                    let compact_debug_hud = &mut q.compact_debug_hud;
                    compact_debug_hud.enabled = true;
                    let enabled = match index {
                        0 => &mut compact_debug_hud.show_fps,
                        1 => &mut compact_debug_hud.show_coords,
                        2 => &mut compact_debug_hud.show_biome,
                        3 => &mut compact_debug_hud.show_facing,
                        4 => &mut compact_debug_hud.show_ping,
                        5 => &mut compact_debug_hud.show_mspt_tps,
                        6 => &mut compact_debug_hud.show_light_level,
                        7 => &mut compact_debug_hud.show_chunks_entities,
                        _ => &mut compact_debug_hud.show_ram,
                    };
                    *enabled = !*enabled;
                    true
                }
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
            WidgetId::VisualNightSky => {
                s.world_effects_detail.night_sky = !s.world_effects_detail.night_sky;
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
            WidgetId::QolCompactDebugHud => {
                q.cycle_compact_debug_hud();
                true
            }
            WidgetId::QolSubtitles => {
                q.show_subtitles = !q.show_subtitles;
                true
            }
            WidgetId::QolAutoReconnect => {
                q.auto_reconnect = !q.auto_reconnect;
                // Toggling off mid-countdown cancels any pending redial.
                self.auto_reconnect.enabled = q.auto_reconnect;
                true
            }
            WidgetId::QolAutoReplenish => {
                q.auto_replenish = !q.auto_replenish;
                true
            }
            WidgetId::QolQuickStack => {
                q.quick_stack_nearby = !q.quick_stack_nearby;
                true
            }
            WidgetId::QolFoodPreview => {
                q.food_saturation_preview = !q.food_saturation_preview;
                true
            }
            WidgetId::QolShulkerPeek => {
                q.shulker_tooltip_peek = !q.shulker_tooltip_peek;
                true
            }
            WidgetId::QolChatScrollLock => {
                q.chat_scroll_lock = !q.chat_scroll_lock;
                // Disabling the lock drops any stale scroll offset so
                // chat returns to bottom-pinned immediately.
                self.chat_scroll.scroll_to_bottom();
                true
            }
            WidgetId::QolToasts => {
                q.toast_notifications = !q.toast_notifications;
                if !q.toast_notifications {
                    self.toasts.clear();
                }
                true
            }
            WidgetId::QolRecipeOverlay => {
                q.recipe_overlay_enabled = !q.recipe_overlay_enabled;
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

#[cfg(test)]
mod mod_sync_prompt_tests {
    use super::*;

    #[test]
    fn prompt_actions_are_explicit_and_cancellation_returns_to_title() {
        let mut ui = UiState::default();
        ui.open_mod_sync_prompt(ModSyncPromptState {
            server_address: "example.test:25565".into(),
            rows: vec![ModSyncPromptRow {
                mod_id: "example".into(),
                required_version: "2".into(),
                installed_version: None,
                sha512: None,
                sha256: None,
                modrinth_slug: None,
            }],
            progress_label: "Waiting for confirmation".into(),
            progress_percent: None,
        });
        assert_eq!(ui.screen, Screen::ModSyncPrompt);
        let list = ui.render(800, 600);
        assert!(list.commands.iter().any(|command| matches!(command, crate::draw::UiCommand::Text { text, .. } if text.contains("example.test"))));
        assert!(ui.hit_regions.iter().any(|region| region.id == WidgetId::ModSyncPromptConfirm));
        assert!(ui.hit_regions.iter().any(|region| region.id == WidgetId::ModSyncPromptCancel));
        assert_eq!(ui.activate(WidgetId::ModSyncPromptConfirm, 0, 0, UiRect::new(0, 0, 0, 0)), vec![UiAction::ConfirmModSyncDownload]);
        assert_eq!(ui.activate(WidgetId::ModSyncPromptCancel, 0, 0, UiRect::new(0, 0, 0, 0)), vec![UiAction::CancelModSync]);
        assert_eq!(ui.screen, Screen::Title);
    }

    #[test]
    fn pause_replay_widget_emits_save_action() {
        let mut ui = UiState { screen: Screen::Pause, ..UiState::default() };
        let list = ui.render(800, 600);
        assert!(list.commands.iter().any(|command| matches!(command, crate::draw::UiCommand::Text { text, .. } if text.contains("Save Replay"))));
        assert!(ui.hit_regions.iter().any(|region| region.id == WidgetId::PauseSaveReplay));
        assert_eq!(ui.activate(WidgetId::PauseSaveReplay, 0, 0, UiRect::new(0, 0, 0, 0)), vec![UiAction::SaveInstantReplay]);
    }
}

#[cfg(test)]
mod friends_screen_tests {
    use super::*;

    const ZERO: UiRect = UiRect::new(0, 0, 0, 0);

    fn demo_friends() -> UiState {
        let mut ui = UiState {
            screen: Screen::Friends,
            ..UiState::default()
        };
        ui.friends.my_friend_id = "megu-id:TESTHOST".into();
        ui.friends.friends = vec![
            FriendListEntry {
                name: "Alice".into(),
                friend_id: "megu-id:ALICE".into(),
                selected_for_share: false,
                connected: true,
                sas_code: Some(123456),
            },
            FriendListEntry {
                name: "Bob".into(),
                friend_id: "megu-id:BOB".into(),
                selected_for_share: true,
                connected: false,
                sas_code: None,
            },
        ];
        ui
    }

    #[test]
    fn friends_identity_add_and_remove_actions() {
        let mut ui = demo_friends();

        // [Copy ID] emits the full megu-id, never the truncated display.
        assert_eq!(
            ui.activate(WidgetId::FriendsCopyId, 0, 0, ZERO),
            vec![UiAction::CopyText("megu-id:TESTHOST".into())]
        );
        ui.friends.my_friend_id.clear();
        assert!(ui.activate(WidgetId::FriendsCopyId, 0, 0, ZERO).is_empty());

        // [Add] fails closed on an empty id field.
        assert!(ui.activate(WidgetId::FriendsAdd, 0, 0, ZERO).is_empty());
        assert!(ui.friends.add_error.is_some());
        ui.friends.add_name_text = "Carol".into();
        ui.friends.add_id_text = "megu-id:CAROL".into();
        ui.friends.add_error = None;
        assert_eq!(
            ui.activate(WidgetId::FriendsAdd, 0, 0, ZERO),
            vec![UiAction::AddFriend {
                name: "Carol".into(),
                friend_id: "megu-id:CAROL".into(),
            }]
        );

        // Row [Remove] emits the instant-revoke action keyed by megu-id.
        ui.friends.selected = Some(0);
        assert_eq!(
            ui.activate(WidgetId::FriendRemove(0), 0, 0, ZERO),
            vec![UiAction::RemoveFriend {
                friend_id: "megu-id:ALICE".into(),
            }]
        );
        assert_eq!(ui.friends.selected, None);
        // Out-of-range rows never emit.
        assert!(ui.activate(WidgetId::FriendRemove(9), 0, 0, ZERO).is_empty());
    }

    #[test]
    fn friends_share_mode_cycles_and_checkbox_persists() {
        let mut ui = demo_friends();
        assert_eq!(ui.friends.share_mode, FriendShareMode::Off);
        assert_eq!(
            ui.activate(WidgetId::FriendsShareMode, 0, 0, ZERO),
            vec![UiAction::SetFriendShareMode(FriendShareMode::FriendsOnly)]
        );
        assert_eq!(
            ui.activate(WidgetId::FriendsShareMode, 0, 0, ZERO),
            vec![UiAction::SetFriendShareMode(FriendShareMode::SelectedFriends)]
        );
        assert_eq!(
            ui.activate(WidgetId::FriendsShareMode, 0, 0, ZERO),
            vec![UiAction::SetFriendShareMode(FriendShareMode::Off)]
        );

        // Selected-Friends checkbox toggles the row mirror and reports.
        assert_eq!(
            ui.activate(WidgetId::FriendSelected(0), 0, 0, ZERO),
            vec![UiAction::SetFriendSelected {
                friend_id: "megu-id:ALICE".into(),
                selected: true,
            }]
        );
        assert!(ui.friends.friends[0].selected_for_share);
        assert!(ui.activate(WidgetId::FriendSelected(9), 0, 0, ZERO).is_empty());
    }

    #[test]
    fn friends_guest_link_needs_a_selected_row() {
        let mut ui = demo_friends();

        // No row selected: fail-closed status, no action.
        assert!(
            ui.activate(WidgetId::FriendsGenerateGuestLink, 0, 0, ZERO)
                .is_empty()
        );
        assert!(ui.friends.status.is_some());

        ui.friends.status = None;
        ui.activate(WidgetId::FriendRow(1), 0, 0, ZERO);
        assert_eq!(ui.friends.selected, Some(1));
        assert_eq!(
            ui.activate(WidgetId::FriendsGenerateGuestLink, 0, 0, ZERO),
            vec![UiAction::GenerateGuestLink {
                friend_id: "megu-id:BOB".into(),
            }]
        );

        // Copy only fires once a link exists.
        assert!(
            ui.activate(WidgetId::FriendsCopyGuestLink, 0, 0, ZERO)
                .is_empty()
        );
        ui.friends.guest_link = Some("megu-friend:x:y:z".into());
        assert_eq!(
            ui.activate(WidgetId::FriendsCopyGuestLink, 0, 0, ZERO),
            vec![UiAction::CopyText("megu-friend:x:y:z".into())]
        );
    }

    #[test]
    fn friends_relay_field_commits_saved_address() {
        let mut ui = demo_friends();
        ui.activate(WidgetId::FriendsRelayField, 0, 0, ZERO);
        for c in "relay.example.net:25575".chars() {
            ui.handle_input(UiInputEvent::CharTyped(c));
        }
        let acts = ui.handle_input(UiInputEvent::KeyPressed(UiKey::Enter));
        assert_eq!(
            acts,
            vec![UiAction::SaveRelayAddr {
                addr: "relay.example.net:25575".into(),
            }]
        );
    }

    #[test]
    fn friends_join_field_routes_guest_links_to_token_join() {
        let mut ui = demo_friends();
        ui.activate(WidgetId::FriendsJoinField, 0, 0, ZERO);
        ui.join_code_text =
            "megu-friend:megu-id:HOSTKEY58:ABCDEFGH:TOKEN26CHARS".into();
        assert_eq!(
            ui.activate(WidgetId::FriendsJoin, 0, 0, ZERO),
            vec![UiAction::JoinGuestLink {
                link: "megu-friend:megu-id:HOSTKEY58:ABCDEFGH:TOKEN26CHARS".into(),
            }]
        );
    }

    #[test]
    fn multiplayer_friends_lan_rows_emit_join_actions() {
        let mut ui = UiState {
            screen: Screen::Multiplayer,
            ..UiState::default()
        };
        ui.multiplayer.friend_worlds = vec![FriendWorldEntry {
            friend_name: "Alice".into(),
            friend_id: "megu-id:ALICE".into(),
            world_name: "Skyblock".into(),
            online_players: 2,
            max_players: 8,
            relay_addr: "relay.example.net:25575".into(),
            invite_code: "MEGU-AAAA-BBBB".into(),
        }];
        ui.multiplayer.lan_worlds = vec![LanWorldEntry {
            name: "Kitchen LAN".into(),
            address: "192.168.1.20".into(),
            port: 25565,
            motd: "hi".into(),
        }];

        assert_eq!(
            ui.activate(WidgetId::MultiplayerFriendWorld(0), 0, 0, ZERO),
            vec![UiAction::JoinFriendWorld { index: 0 }]
        );
        assert_eq!(
            ui.activate(WidgetId::MultiplayerLanWorld(0), 0, 0, ZERO),
            vec![UiAction::JoinServer {
                address: "192.168.1.20".into(),
                port: 25565,
            }]
        );
        assert_eq!(
            ui.activate(WidgetId::MultiplayerManageFriends, 0, 0, ZERO),
            vec![UiAction::OpenScreen(Screen::Friends)]
        );
        assert_eq!(ui.screen, Screen::Friends);
    }

    #[test]
    fn friends_and_multiplayer_have_full_controls_at_ship_sizes() {
        let mut ui = demo_friends();
        ui.multiplayer.friend_worlds = vec![FriendWorldEntry {
            friend_name: "Alice".into(),
            friend_id: "megu-id:ALICE".into(),
            world_name: "Skyblock".into(),
            online_players: 1,
            max_players: 8,
            relay_addr: "r".into(),
            invite_code: "MEGU-AAAA-BBBB".into(),
        }];
        ui.multiplayer.lan_worlds = vec![LanWorldEntry {
            name: "L".into(),
            address: "10.0.0.1".into(),
            port: 25565,
            motd: String::new(),
        }];
        ui.multiplayer.servers = vec![MultiplayerServerEntry {
            name: "Hub".into(),
            address: "hub.example.net".into(),
            port: 25565,
        }];

        for &(w, h) in &[(854u32, 480u32), (1280, 720)] {
            ui.screen = Screen::Friends;
            ui.render(w, h);
            for id in [
                WidgetId::FriendsCopyId,
                WidgetId::FriendsAddNameField,
                WidgetId::FriendsAddIdField,
                WidgetId::FriendsAdd,
                WidgetId::FriendRow(0),
                WidgetId::FriendRemove(0),
                WidgetId::FriendsShareMode,
                WidgetId::FriendsRelayField,
                WidgetId::FriendsGenerateGuestLink,
                WidgetId::FriendsCopyGuestLink,
                WidgetId::FriendsGenerate,
                WidgetId::FriendsCopy,
                WidgetId::FriendsRevoke,
                WidgetId::FriendsJoinField,
                WidgetId::FriendsJoin,
                WidgetId::FriendsBack,
            ] {
                assert!(
                    ui.hit_regions.iter().any(|r| r.id == id),
                    "{id:?} missing on Friends @{w}x{h}"
                );
            }
            assert_no_overlap(&ui.hit_regions, w, h);

            ui.screen = Screen::Multiplayer;
            ui.render(w, h);
            for id in [
                WidgetId::MultiplayerFriendWorld(0),
                WidgetId::MultiplayerLanWorld(0),
                WidgetId::MultiplayerManageFriends,
                WidgetId::MultiplayerServer(0),
                WidgetId::MultiplayerAddressField,
                WidgetId::MultiplayerJoin,
                WidgetId::MultiplayerAdd,
                WidgetId::MultiplayerDelete,
                WidgetId::MultiplayerBack,
            ] {
                assert!(
                    ui.hit_regions.iter().any(|r| r.id == id),
                    "{id:?} missing on Multiplayer @{w}x{h}"
                );
            }
            assert_no_overlap(&ui.hit_regions, w, h);
        }
    }

    fn assert_no_overlap(regions: &[HitRegion], w: u32, h: u32) {
        for (i, a) in regions.iter().enumerate() {
            assert!(
                a.rect.x >= 0
                    && a.rect.y >= 0
                    && a.rect.right() <= w as i32
                    && a.rect.bottom() <= h as i32,
                "region outside {w}x{h}: {a:?}"
            );
            for b in &regions[i + 1..] {
                assert!(
                    !a.rect.intersects(b.rect),
                    "regions overlap: {a:?} vs {b:?}"
                );
            }
        }
    }
}

#[cfg(test)]
mod sign_editor_overlay_tests {
    use super::*;
    use crate::sign_and_chat::SignEditorState;

    /// Every draw command the sign-edit overlay emits must land inside
    /// the viewport — checked at the 854×480 minimum and a 4K extreme.
    #[test]
    fn sign_editor_overlay_stays_inside_viewport() {
        for &(w, h) in &[(854u32, 480u32), (3840, 2160), (640, 360)] {
            let mut ui = UiState::new();
            ui.screen = Screen::InGameHud;
            ui.sign_editor = Some(SignEditorState::new(
                (0, 64, 0),
                true,
                [
                    "Hello".to_owned(),
                    "看板のテキスト編集の長い行".to_owned(),
                    String::new(),
                    "x".repeat(400),
                ],
            ));
            let mut out = crate::draw::UiDrawList::new();
            assert!(
                crate::layout::layout_sign_editor(&ui, w, h, &mut out),
                "overlay must draw at {w}x{h}"
            );
            assert!(!out.commands.is_empty());
            for cmd in &out.commands {
                match cmd {
                    crate::draw::UiCommand::FillRect { rect, .. }
                    | crate::draw::UiCommand::BorderRect { rect, .. }
                    | crate::draw::UiCommand::ItemIcon { rect, .. }
                    | crate::draw::UiCommand::Sprite { rect, .. }
                    | crate::draw::UiCommand::ProgressBar { rect, .. } => {
                        assert!(rect.x >= 0 && rect.y >= 0, "{cmd:?} left/top overflow @{w}x{h}");
                        assert!(
                            rect.right() <= w as i32 && rect.bottom() <= h as i32,
                            "{cmd:?} right/bottom overflow @{w}x{h}"
                        );
                    }
                    crate::draw::UiCommand::Text {
                        x,
                        y,
                        scale,
                        text,
                        ..
                    } => {
                        assert!(*x >= 0 && *y >= 0, "{cmd:?} left/top overflow @{w}x{h}");
                        assert!(
                            *y + 8 * *scale as i32 <= h as i32,
                            "{cmd:?} bottom overflow @{w}x{h}"
                        );
                        assert!(
                            *x + crate::draw::text_width(text, *scale) as i32 <= w as i32,
                            "{cmd:?} right overflow @{w}x{h}"
                        );
                    }
                }
            }
        }
    }

    /// The overlay must also emit through `UiState::render` (the path the
    /// GPU window actually draws) and disappear with the editor.
    #[test]
    fn sign_editor_overlay_renders_through_ui_state() {
        let mut ui = UiState::new();
        ui.screen = Screen::InGameHud;
        ui.sign_editor = Some(SignEditorState::new((1, 2, 3), false, Default::default()));
        let with_editor = ui.render(854, 480);
        assert!(!with_editor.commands.is_empty());

        ui.sign_editor = None;
        let without_editor = ui.render(854, 480);
        assert!(
            with_editor.commands.len() > without_editor.commands.len(),
            "the open editor must add draw commands to the frame"
        );
    }

    /// The Japanese locale draws the localized panel title.
    #[test]
    fn sign_editor_overlay_uses_japanese_title() {
        let mut ui = UiState::new();
        ui.screen = Screen::InGameHud;
        ui.settings.language = LanguageSetting::Japanese;
        ui.sign_editor = Some(SignEditorState::new((0, 0, 0), true, Default::default()));
        let mut out = crate::draw::UiDrawList::new();
        assert!(crate::layout::layout_sign_editor(&ui, 1280, 720, &mut out));
        assert!(out.commands.iter().any(|cmd| matches!(
            cmd,
            crate::draw::UiCommand::Text { text, .. } if text == "看板のテキスト編集"
        )));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn qol_settings_seamless_defaults() {
        let q = QolSettings::default();
        // Opinionated gameplay helpers are strictly opt-in.
        assert!(!q.auto_reconnect);
        assert!(!q.auto_replenish);
        assert!(!q.quick_stack_nearby);
        assert!(!q.food_saturation_preview);
        // Shulker peek yields to dedicated community mods (kept only for
        // settings-file compat, defaulted off like the other helpers).
        assert!(!q.shulker_tooltip_peek);
        // Pure-UX improvements ship on.
        assert!(q.chat_scroll_lock);
        assert!(q.toast_notifications);
    }

    #[test]
    fn seamless_toggles_round_trip_through_adjust_and_undo() {
        let mut ui = UiState::new();
        ui.navigate(Screen::Settings(SettingsTab::Qol));
        let cases = [
            (WidgetId::QolAutoReconnect, SettingKey::QolAutoReconnect),
            (WidgetId::QolAutoReplenish, SettingKey::QolAutoReplenish),
            (WidgetId::QolQuickStack, SettingKey::QolQuickStack),
            (WidgetId::QolFoodPreview, SettingKey::QolFoodPreview),
            (WidgetId::QolShulkerPeek, SettingKey::QolShulkerPeek),
            (WidgetId::QolChatScrollLock, SettingKey::QolChatScrollLock),
            (WidgetId::QolToasts, SettingKey::QolToastNotifications),
        ];
        for (widget, key) in cases {
            let acts = ui.adjust(widget, 1);
            assert_eq!(acts, vec![UiAction::SettingsChanged], "{widget:?}");
            assert!(ui.is_setting_dirty(key), "{key:?} should be dirty");
            assert_eq!(ui.revert_single_setting(key), vec![UiAction::SettingsChanged]);
            assert!(!ui.is_setting_dirty(key), "{key:?} revert restores");
        }
        assert_eq!(ui.settings.qol, QolSettings::default());
    }

    #[test]
    fn auto_reconnect_toggle_syncs_controller_gate() {
        let mut ui = UiState::new();
        ui.navigate(Screen::Settings(SettingsTab::Qol));
        ui.adjust(WidgetId::QolAutoReconnect, 1);
        assert!(ui.settings.qol.auto_reconnect);
        assert!(ui.auto_reconnect.enabled);
        ui.auto_reconnect
            .on_unexpected_disconnect("example.org", 25565, "timed out");
        assert!(ui.auto_reconnect.is_reconnecting());
        ui.adjust(WidgetId::QolAutoReconnect, -1);
        assert!(!ui.auto_reconnect.enabled);
        assert!(!ui.auto_reconnect.tick(60_000));
    }

    #[test]
    fn inventory_helpers_yield_to_mods_and_strict_fair_play() {
        let mut s = UiSettings::default();
        s.qol.auto_replenish = true;
        s.qol.quick_stack_nearby = true;
        s.qol.food_saturation_preview = true;
        // Opted-in, no mod, permissive server -> built-ins run.
        assert!(s.builtin_auto_replenish_active(false, false));
        assert!(s.builtin_quick_stack_active(false, false));
        assert!(s.builtin_food_preview_active(false));
        // A dedicated mod installed -> built-ins yield (zero conflicts).
        assert!(!s.builtin_auto_replenish_active(true, false));
        assert!(!s.builtin_quick_stack_active(true, false));
        assert!(!s.builtin_food_preview_active(true));
        // Strict fair-play server -> inventory automation yields.
        assert!(!s.builtin_auto_replenish_active(false, true));
        assert!(!s.builtin_quick_stack_active(false, true));
        // Food preview is display-only — safe even on strict servers.
        assert!(s.builtin_food_preview_active(false));
        // Default settings: nothing runs even on permissive servers.
        let d = UiSettings::default();
        assert!(!d.builtin_auto_replenish_active(false, false));
        assert!(!d.builtin_quick_stack_active(false, false));
        assert!(!d.builtin_food_preview_active(false));
    }

    #[test]
    fn push_toast_respects_the_toggle() {
        let mut ui = UiState::new();
        ui.push_toast(crate::qol_helpers::ToastKind::Info, "hi", "");
        assert_eq!(ui.toasts.len(), 1);
        ui.settings.qol.toast_notifications = false;
        ui.push_toast(crate::qol_helpers::ToastKind::Info, "nope", "");
        assert_eq!(ui.toasts.len(), 1);
    }

    #[test]
    fn chat_scroll_lock_toggle_resets_stale_offset() {
        let mut ui = UiState::new();
        ui.navigate(Screen::Settings(SettingsTab::Qol));
        ui.chat_scroll.scroll_by(4);
        ui.on_chat_message();
        assert_eq!(ui.chat_scroll.unread_while_scrolled, 1);
        ui.adjust(WidgetId::QolChatScrollLock, 1);
        assert!(!ui.settings.qol.chat_scroll_lock);
        assert!(!ui.chat_scroll.scrolled_up);
        assert_eq!(ui.chat_scroll.scroll_offset, 0);
        // With the lock off, new messages no longer count unread.
        ui.on_chat_message();
        assert_eq!(ui.chat_scroll.unread_while_scrolled, 0);
    }

    #[test]
    fn toast_toggle_off_clears_queue() {
        let mut ui = UiState::new();
        ui.navigate(Screen::Settings(SettingsTab::Qol));
        ui.push_toast(crate::qol_helpers::ToastKind::Info, "a", "");
        ui.adjust(WidgetId::QolToasts, 1);
        assert!(ui.toasts.is_empty());
    }

    #[test]
    fn ui_state_defaults_carry_seamless_qol() {
        let ui = UiState::new();
        assert!(ui.toasts.is_empty());
        assert!(!ui.keybinds.entries.is_empty());
        assert!(crate::qol_helpers::detect_keybind_conflicts(&ui.keybinds).is_empty());
        assert_eq!(ui.chat_scroll, crate::qol_helpers::ChatScrollLockState::default());
        assert!(!ui.auto_reconnect.is_reconnecting());
    }
}
