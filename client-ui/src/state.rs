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

use mc_spine_core::crash_report::CrashContext;

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

/// Everything the settings screens can change.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct UiSettings {
    pub video: VideoSettings,
    pub audio: AudioSettings,
    pub shader_quick: ShaderQuickSettings,
    pub qol: QolSettings,
    pub fair_play: FairPlaySettings,
    pub language: LanguageSetting,
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

/// Everything the in-game HUD draws. `health`/`hunger`/`armor` are in
/// vanilla half-icon units (20 = full bar); `air` is in bubbles×30
/// (300 = full). Pure data — the game fills it, the HUD draws it.
#[derive(Clone, Debug, PartialEq)]
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
    /// Open the mods & packs browser.
    OpenModsPacks,
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
    // Shaders tab
    ShadersEnabled,
    ShadersOpenOptions,
    // Audio tab
    AudioMaster,
    AudioMusic,
    AudioBlocks,
    AudioHostile,
    AudioFriendly,
    AudioMaxVolumeCap,
    AudioMaxConcurrent,
    // QoL tab
    QolSmoothZoom,
    QolToolWarn,
    QolPreventBreak,
    QolChatHistory,
    QolRetainChat,
    QolCrashMode,
    QolShowSupportLinks,
    // Fair-Play tab
    FpMinimapMode,
    FpCaveMap,
    FpMobRadar,
    FpRadarScope,
    FpWaypointTeleport,
    // Language tab
    LanguageChoice,
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
    PauseInvite,
    PauseSaveQuit,
    PauseReportBug,
    // Crash recovery (+ confirmation modal)
    CrashSendReport,
    CrashReturnTitle,
    ModalOk,
    ModalCancel,
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
                | Self::AudioMaster
                | Self::AudioMusic
                | Self::AudioBlocks
                | Self::AudioHostile
                | Self::AudioFriendly
                | Self::AudioMaxVolumeCap
                | Self::AudioMaxConcurrent
                | Self::QolSmoothZoom
                | Self::QolToolWarn
                | Self::QolPreventBreak
                | Self::QolChatHistory
                | Self::QolRetainChat
                | Self::QolCrashMode
                | Self::QolShowSupportLinks
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

    invite_counter: u64,
    fps_field_focused: bool,
    fps_field_text: String,
    fps_field_error: bool,
    join_field_focused: bool,
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
            invite_counter: 0,
            fps_field_focused: false,
            fps_field_text: String::new(),
            fps_field_error: false,
            join_field_focused: false,
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

    /// Resolved display language.
    pub fn lang(&self) -> UiLang {
        self.settings.language.resolve()
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
        let mut regions = std::mem::take(&mut self.hit_regions);
        layout::layout_screen_into(self, width, height, out, &mut regions);
        self.hit_regions = regions;
        if let Some(i) = self.focus_index {
            if i >= self.hit_regions.len() {
                self.focus_index = None;
            }
        }
    }

    /// Push `screen` and navigate to it.
    pub fn navigate(&mut self, screen: Screen) {
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
                    self.settings.video.fps_cap = v;
                    self.settings.video.preset = VideoPreset::Custom;
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
            WidgetId::TitleMods => vec![UiAction::OpenModsPacks],
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
                None => Vec::new(),
            },
            WidgetId::ModalCancel => {
                self.modal = None;
                Vec::new()
            }

            // Adjustable widgets activate as a "+1" cycle/slider click.
            _ => self.adjust(id, 1),
        }
    }

    /// Left/Right (or click on cyclers/toggles): `dir` = -1 or +1.
    fn adjust(&mut self, id: WidgetId, dir: i32) -> Vec<UiAction> {
        let v = &mut self.settings.video;
        let a = &mut self.settings.audio;
        let q = &mut self.settings.qol;
        let f = &mut self.settings.fair_play;
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
            WidgetId::LanguageChoice => {
                self.settings.language = self.settings.language.next();
                true
            }
            _ => false,
        };
        if changed {
            vec![UiAction::SettingsChanged]
        } else {
            Vec::new()
        }
    }

    fn set_fps_step(&mut self, idx: usize) -> Vec<UiAction> {
        let idx = idx.min(FPS_CAP_STEPS.len() - 1);
        self.settings.video.fps_cap = FPS_CAP_STEPS[idx];
        self.settings.video.preset = VideoPreset::Custom;
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
pub use mc_spine_core::crash_report::redact_sensitive as ui_redact_sensitive;
