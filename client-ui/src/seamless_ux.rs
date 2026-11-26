// SPDX-License-Identifier: LGPL-3.0-or-later
//! Seamless onboarding UX — the pure-logic core behind three
//! player-facing flows, all `std`-only and deterministic:
//!
//! * [`QualityPreset`] — one-click quality presets
//!   (`UltraLowLaptop` / `Balanced` / `Cinematic` / `SteamDeck60`) with
//!   concrete render-distance / FPS-cap / shadow / shader parameters and
//!   bilingual EN/JA names and descriptions, plus
//!   [`SettingsSearchIndex`], a bilingual ranked search over every
//!   [`SettingKey`], and [`LivePreviewBackdrop`], the translucent
//!   split-panel layout that keeps the world live-rendered behind the
//!   settings panel so shader/FOV/render-distance/font changes are
//!   visible in real time.
//! * [`DropPayloadClassifier`] — the universal window drag-and-drop
//!   classifier: turns a dropped filesystem path into a
//!   [`DroppedAssetKind`] (`.mrpack`, Wasm mod, local Java `.jar`,
//!   resource/shader-pack `.zip`, 64×64/64×32 player-skin `.png`, world
//!   save dir-or-zip) and a mandatory bilingual [`DropConfirmPrompt`]
//!   (`ApplyNow` / `StageForNextWorld` / `Cancel`). It inspects ZIP
//!   central directories and PNG headers in place — it never extracts,
//!   never writes, and fail-closed rejects path traversal / zip-slip /
//!   symlink payloads / oversized bombs. Nothing installs without the
//!   player's explicit consent.
//! * [`ExistingLauncherImporter`] — zero-pain import from an existing
//!   `.minecraft`, Flatpak, Prism Launcher or Modrinth App install:
//!   discovers only safe user data (`servers.dat`, `options.txt`,
//!   `saves/`, `resourcepacks/`) behind a hard-coded secret-file
//!   denylist ([`ExistingLauncherImporter::is_forbidden_secret_path`])
//!   so account tokens, credentials and session files are never opened,
//!   listed or copied. All entry points take explicit base paths so
//!   tests stay 100% hermetic.

#![forbid(unsafe_code)]

use std::collections::BTreeMap;
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use megu_core::zip_archive::LazyZipIndex;

use crate::draw::UiRect;
use crate::state::{
    tr, CloudSetting, GpuBackend, LodMode, SettingKey, SettingsTab, UiLang, VideoPreset,
    VideoSettings,
};

// ---------------------------------------------------------------------------
// 1-click quality presets
// ---------------------------------------------------------------------------

/// A one-click quality preset the player can apply from the top of the
/// Video tab. Every preset expands into a complete concrete
/// [`QualityPresetSpec`] — no field is left to guesswork.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QualityPreset {
    /// Bare-metal survival mode for very old or very thin hardware:
    /// tiny render distance, no shaders, no shadows, hard 30 fps cap.
    UltraLowLaptop,
    /// The default everyday profile — comfortable 60+ fps on typical
    /// desktop GPUs with all readability features on.
    Balanced,
    /// Maximum beauty for showcases and screenshots: far LOD, shaders
    /// on, high-resolution shadows, uncapped frame rate.
    Cinematic,
    /// Tuned for the Steam Deck (and similar handhelds): locked 60 fps,
    /// medium draw distance, modest shadows — smooth without cooking
    /// the battery.
    SteamDeck60,
}

impl QualityPreset {
    /// All presets, in menu order.
    pub const ALL: [Self; 4] = [
        Self::UltraLowLaptop,
        Self::Balanced,
        Self::Cinematic,
        Self::SteamDeck60,
    ];

    /// Short preset name in the UI language.
    pub const fn name(self, lang: UiLang) -> &'static str {
        match (self, lang) {
            (Self::UltraLowLaptop, UiLang::En) => "Ultra-Low Laptop",
            (Self::Balanced, UiLang::En) => "Balanced",
            (Self::Cinematic, UiLang::En) => "Cinematic",
            (Self::SteamDeck60, UiLang::En) => "Steam Deck 60",
            (Self::UltraLowLaptop, UiLang::Ja) => "超低スペックノート",
            (Self::Balanced, UiLang::Ja) => "バランス",
            (Self::Cinematic, UiLang::Ja) => "シネマティック",
            (Self::SteamDeck60, UiLang::Ja) => "Steam Deck 60",
        }
    }

    /// One-line bilingual description shown next to the preset button.
    pub const fn description(self, lang: UiLang) -> &'static str {
        match (self, lang) {
            (Self::UltraLowLaptop, UiLang::En) => {
                "Minimum load for weak laptops: 6 chunks, no shaders or shadows, 30 fps cap."
            }
            (Self::Balanced, UiLang::En) => {
                "Everyday default: 12 chunks, smooth lighting, VSync, all comfort effects on."
            }
            (Self::Cinematic, UiLang::En) => {
                "Maximum beauty: 32 chunks + far LOD, shaders, 2048px shadows, uncapped fps."
            }
            (Self::SteamDeck60, UiLang::En) => {
                "Handheld sweet spot: 10 chunks, modest shadows, locked 60 fps."
            }
            (Self::UltraLowLaptop, UiLang::Ja) => {
                "低スペックPC向け最小負荷: 6チャンク・シェーダー/影なし・30fps上限。"
            }
            (Self::Balanced, UiLang::Ja) => {
                "日常プレイの標準: 12チャンク・スムースライティング・垂直同期あり。"
            }
            (Self::Cinematic, UiLang::Ja) => {
                "最高画質: 32チャンク+遠景LOD・シェーダー・2048px影・fps無制限。"
            }
            (Self::SteamDeck60, UiLang::Ja) => {
                "携帯機向け最適化: 10チャンク・控えめな影・60fps固定。"
            }
        }
    }

    /// The full concrete parameter set this preset applies.
    pub const fn spec(self) -> QualityPresetSpec {
        match self {
            Self::UltraLowLaptop => QualityPresetSpec {
                render_distance_chunks: 6,
                lod: LodMode::Off,
                gpu_backend: GpuBackend::ClassicLowSpec,
                fps_cap: 30,
                vsync: false,
                smooth_lighting: false,
                clouds: CloudSetting::Off,
                entity_shadows: false,
                shadow_map_resolution: 0,
                shaders_enabled: false,
                dynamic_lights: false,
                ram_budget_mib: 1024,
                matched_video_preset: VideoPreset::Fast,
            },
            Self::Balanced => QualityPresetSpec {
                render_distance_chunks: 12,
                lod: LodMode::Medium,
                gpu_backend: GpuBackend::Auto,
                fps_cap: 0,
                vsync: true,
                smooth_lighting: true,
                clouds: CloudSetting::Fancy,
                entity_shadows: true,
                shadow_map_resolution: 1024,
                shaders_enabled: false,
                dynamic_lights: true,
                ram_budget_mib: 2048,
                matched_video_preset: VideoPreset::Balanced,
            },
            Self::Cinematic => QualityPresetSpec {
                render_distance_chunks: 32,
                lod: LodMode::Far,
                gpu_backend: GpuBackend::ComputeIndirect,
                fps_cap: 0,
                vsync: false,
                smooth_lighting: true,
                clouds: CloudSetting::Fancy,
                entity_shadows: true,
                shadow_map_resolution: 2048,
                shaders_enabled: true,
                dynamic_lights: true,
                ram_budget_mib: 6144,
                matched_video_preset: VideoPreset::Ultra,
            },
            Self::SteamDeck60 => QualityPresetSpec {
                render_distance_chunks: 10,
                lod: LodMode::Medium,
                gpu_backend: GpuBackend::Auto,
                fps_cap: 60,
                vsync: false,
                smooth_lighting: true,
                clouds: CloudSetting::Fast,
                entity_shadows: true,
                shadow_map_resolution: 1024,
                shaders_enabled: false,
                dynamic_lights: true,
                ram_budget_mib: 2048,
                matched_video_preset: VideoPreset::High,
            },
        }
    }

    /// Apply every concrete video parameter to `settings`. Fields the
    /// preset does not own (e.g. gameplay-only toggles) are left alone.
    pub fn apply(self, settings: &mut VideoSettings) {
        let spec = self.spec();
        settings.preset = spec.matched_video_preset;
        settings.view_distance = spec.render_distance_chunks;
        settings.lod = spec.lod;
        settings.gpu_backend = spec.gpu_backend;
        settings.fps_cap = spec.fps_cap;
        settings.vsync = spec.vsync;
        settings.smooth_lighting = spec.smooth_lighting;
        settings.clouds = spec.clouds;
        settings.entity_shadows = spec.entity_shadows;
        settings.dynamic_lights = spec.dynamic_lights;
        settings.ram_budget_mib = spec.ram_budget_mib;
    }
}

/// The concrete parameter set a [`QualityPreset`] stands for.
///
/// `fps_cap` follows [`VideoSettings::fps_cap`] convention: `0` means
/// unlimited. `shadow_map_resolution` is the shadow-map texel size; `0`
/// disables shadow rendering entirely.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct QualityPresetSpec {
    /// View/render distance in chunks (`6..=32`).
    pub render_distance_chunks: u8,
    /// Distant-LOD column drawing.
    pub lod: LodMode,
    /// GPU pipeline the renderer should negotiate.
    pub gpu_backend: GpuBackend,
    /// Frame-rate cap (`0` = unlimited).
    pub fps_cap: u32,
    /// Vertical sync.
    pub vsync: bool,
    /// Smooth (interpolated) lighting.
    pub smooth_lighting: bool,
    /// Cloud deck detail.
    pub clouds: CloudSetting,
    /// Entity contact shadows toggle.
    pub entity_shadows: bool,
    /// Shadow-map texel size; `0` = shadow pass off.
    pub shadow_map_resolution: u16,
    /// Whether the shader pipeline is enabled.
    pub shaders_enabled: bool,
    /// Dynamic colored lights.
    pub dynamic_lights: bool,
    /// Host chunk/asset cache budget in MiB (`0` = auto).
    pub ram_budget_mib: u32,
    /// The closest existing [`VideoPreset`] so the legacy preset field
    /// stays truthful after applying.
    pub matched_video_preset: VideoPreset,
}

// ---------------------------------------------------------------------------
// Bilingual settings search index
// ---------------------------------------------------------------------------

/// One searchable settings entry: the canonical [`SettingKey`], the tab
/// (category) it lives under, its bilingual display name and a set of
/// bilingual + jargon keywords (`影`, `fps`, `fov`, `コントローラ`,
/// `音量`, `shader`, `distance`, ...).
#[derive(Clone, Copy, Debug)]
pub struct SettingsSearchEntry {
    /// The setting this entry resolves to.
    pub key: SettingKey,
    /// The settings tab (category) that owns it.
    pub tab: SettingsTab,
    /// English display name.
    pub name_en: &'static str,
    /// Japanese display name.
    pub name_ja: &'static str,
    /// Extra lowercase search terms (both languages plus gamer jargon).
    pub keywords: &'static [&'static str],
}

/// One ranked match returned by [`SettingsSearchIndex::search`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SearchHit {
    /// The matching setting.
    pub key: SettingKey,
    /// The tab (category) the player should open.
    pub tab: SettingsTab,
    /// English display name.
    pub name_en: &'static str,
    /// Japanese display name.
    pub name_ja: &'static str,
    /// Match strength; higher ranks first.
    pub score: u32,
}

/// The complete searchable catalog — one entry per [`SettingKey`],
/// covering all seven [`SettingsTab`] categories.
const SEARCH_CATALOG: &[SettingsSearchEntry] = &[
    entry(SettingKey::VideoPreset, SettingsTab::Video, "Quality Preset", "画質プリセット", &["preset", "quality", "画質", "プリセット", "performance", "設定"]),
    entry(SettingKey::VideoViewDistance, SettingsTab::Video, "View Distance", "描画距離", &["distance", "render", "chunks", "描画距離", "チャンク", "距離", "render distance"]),
    entry(SettingKey::VideoLod, SettingsTab::Video, "LOD", "遠景LOD", &["lod", "distant", "horizon", "遠景"]),
    entry(SettingKey::VideoGpuBackend, SettingsTab::Video, "GPU Backend", "GPUバックエンド", &["gpu", "vulkan", "opengl", "backend", "グラフィック"]),
    entry(SettingKey::VideoFpsCap, SettingsTab::Video, "FPS Cap", "FPS上限", &["fps", "frame", "rate", "limit", "cap", "フレーム", "上限"]),
    entry(SettingKey::VideoBackgroundFps, SettingsTab::Video, "Background FPS", "バックグラウンドFPS", &["fps", "background", "idle", "throttle", "省電力"]),
    entry(SettingKey::VideoVsync, SettingsTab::Video, "VSync", "垂直同期", &["vsync", "sync", "tearing", "垂直同期", "ティアリング"]),
    entry(SettingKey::VideoSmoothLighting, SettingsTab::Video, "Smooth Lighting", "スムースライティング", &["lighting", "smooth", "光", "照明", "shade"]),
    entry(SettingKey::VideoClouds, SettingsTab::Video, "Clouds", "雲", &["clouds", "sky", "雲", "空"]),
    entry(SettingKey::VideoEntityShadows, SettingsTab::Video, "Entity Shadows", "エンティティの影", &["shadow", "shadows", "影", "entity", "mob"]),
    entry(SettingKey::VideoComfortExposure, SettingsTab::Video, "Comfort Exposure", "目に優しい露出", &["exposure", "brightness", "comfort", "eye", "眩しさ", "露出"]),
    entry(SettingKey::VideoDynamicLights, SettingsTab::Video, "Dynamic Lights", "ダイナミックライト", &["light", "lights", "dynamic", "torch", "光", "松明"]),
    entry(SettingKey::VideoEnchantmentGlint, SettingsTab::Video, "Enchantment Glint", "エンチャントの輝き", &["glint", "enchant", "エンチャント", "輝き", "shimmer"]),
    entry(SettingKey::VideoIndirectLightBounce, SettingsTab::Video, "Indirect Light Bounce", "間接光バウンス", &["bounce", "indirect", "gi", "lighting", "間接光", "光"]),
    entry(SettingKey::VideoDirectionalShadowGameplay, SettingsTab::Video, "Directional Shadow Gameplay", "方向性シャドウ", &["shadow", "影", "directional", "gameplay"]),
    entry(SettingKey::VideoLazyCatchupGameplay, SettingsTab::Video, "Lazy Chunk Catch-Up", "遅延チャンク更新", &["chunk", "catchup", "lazy", "チャンク", "かまど"]),
    entry(SettingKey::VideoRamBudget, SettingsTab::Video, "RAM Budget", "RAM使用量上限", &["ram", "memory", "メモリ", "cache", "上限"]),
    entry(SettingKey::DisplayFullscreen, SettingsTab::Video, "Fullscreen", "フルスクリーン", &["fullscreen", "window", "screen", "全画面", "フルスクリーン"]),
    entry(SettingKey::DisplayInvertY, SettingsTab::Controls, "Invert Y Axis", "Y軸反転", &["invert", "mouse", "y axis", "反転", "マウス"]),
    entry(SettingKey::DisplayGamma, SettingsTab::Video, "Gamma", "ガンマ", &["gamma", "brightness", "明るさ", "輝度"]),
    entry(SettingKey::DisplayGuiScale, SettingsTab::Video, "GUI Scale", "GUIスケール", &["gui", "scale", "ui", "サイズ", "拡大", "表示"]),
    entry(SettingKey::KeyBindings, SettingsTab::Controls, "Key Bindings", "キー割り当て", &["key", "keys", "bind", "bindings", "controls", "キー", "コントローラ", "操作", "ボタン"]),
    entry(SettingKey::AudioMaster, SettingsTab::Audio, "Master Volume", "マスター音量", &["volume", "音量", "audio", "sound", "master", "音"]),
    entry(SettingKey::AudioMusic, SettingsTab::Audio, "Music Volume", "音楽の音量", &["music", "音楽", "音量", "volume", "bgm"]),
    entry(SettingKey::AudioBlocks, SettingsTab::Audio, "Block Sounds", "ブロックの音", &["blocks", "block", "音", "音量", "sound"]),
    entry(SettingKey::AudioHostile, SettingsTab::Audio, "Hostile Mob Sounds", "敵モブの音", &["hostile", "mob", "敵", "音", "音量", "monster"]),
    entry(SettingKey::AudioFriendly, SettingsTab::Audio, "Friendly Mob Sounds", "友好モブの音", &["friendly", "mob", "動物", "音", "音量", "animal"]),
    entry(SettingKey::AudioMaxVolumeCap, SettingsTab::Audio, "Max Volume Cap", "音量の上限", &["volume", "cap", "音量", "上限", "limit"]),
    entry(SettingKey::AudioMaxConcurrentSameSound, SettingsTab::Audio, "Same-Sound Limit", "同一音の同時再生数", &["sound", "concurrent", "同時", "音", "limit"]),
    entry(SettingKey::AudioSpatial3d, SettingsTab::Audio, "3D Spatial Audio", "3D立体音響", &["spatial", "3d", "audio", "立体音響", "音", "surround"]),
    entry(SettingKey::ShadersEnabled, SettingsTab::Shaders, "Shaders", "シェーダー", &["shader", "shaders", "シェーダー", "影", "光影", "影mod"]),
    entry(SettingKey::VisualStylePreset, SettingsTab::Shaders, "Visual Style", "ビジュアルスタイル", &["style", "visual", "preset", "スタイル", "見た目"]),
    entry(SettingKey::ExternalShaderPack, SettingsTab::Shaders, "Shader Pack", "シェーダーパック", &["shader", "pack", "シェーダー", "パック", "shaderpack"]),
    entry(SettingKey::BlocksAndSkins3d, SettingsTab::Shaders, "3D Blocks & Skins", "3Dブロックとスキン", &["3d", "skin", "skins", "スキン", "block", "blocks"]),
    entry(SettingKey::WorldEffects, SettingsTab::Shaders, "World Effects", "ワールドエフェクト", &["effects", "world", "エフェクト", "effect"]),
    entry(SettingKey::WorldEffectsDetail, SettingsTab::Shaders, "Effects Detail", "エフェクト詳細度", &["effects", "detail", "エフェクト", "詳細"]),
    entry(SettingKey::CameraMicroMotion, SettingsTab::Qol, "Camera Micro-Motion", "カメラの微細モーション", &["camera", "motion", "カメラ", "shake", "揺れ"]),
    entry(SettingKey::FovDeg, SettingsTab::Qol, "Field of View", "視野角", &["fov", "field", "view", "視野角", "視野"]),
    entry(SettingKey::MouseSensitivityPct, SettingsTab::Controls, "Mouse Sensitivity", "マウス感度", &["sensitivity", "sens", "mouse", "感度", "マウス"]),
    entry(SettingKey::ZoomDivisorTenths, SettingsTab::Qol, "Zoom Divisor", "ズーム倍率", &["zoom", "ズーム", "divisor", "望遠"]),
    entry(SettingKey::QolLookingAtHud, SettingsTab::Qol, "Looking-At HUD", "注視HUD", &["hud", "jade", "waila", "注視", "looking"]),
    entry(SettingKey::QolInventoryDragControls, SettingsTab::Qol, "Inventory Drag Controls", "インベントリドラッグ操作", &["mouse", "tweaks", "mousetweaks", "drag", "マウス", "ドラッグ"]), // trademark-hygiene:allow external-mod search aliases
    entry(SettingKey::QolInventorySort, SettingsTab::Qol, "Inventory Sort", "インベントリ整理", &["inventory", "sort", "整理", "インベントリ", "ソート"]),
    entry(SettingKey::ZshCommandBar, SettingsTab::Qol, "Command Bar", "コマンドバー", &["command", "bar", "zsh", "コマンド"]),
    entry(SettingKey::QolSmoothZoom, SettingsTab::Qol, "Smooth Zoom", "スムーズズーム", &["zoom", "smooth", "ズーム", "スムーズ"]),
    entry(SettingKey::QolToolWarnPercent, SettingsTab::Qol, "Tool Durability Warning", "ツール耐久警告", &["tool", "durability", "warn", "耐久", "警告"]),
    entry(SettingKey::QolPreventToolBreak, SettingsTab::Qol, "Prevent Tool Break", "ツール破壊の防止", &["tool", "break", "耐久", "防止", "壊れ"]),
    entry(SettingKey::QolChatHistoryLines, SettingsTab::Qol, "Chat History Lines", "チャット履歴行数", &["chat", "history", "チャット", "履歴", "lines"]),
    entry(SettingKey::QolRetainChatOnReconnect, SettingsTab::Qol, "Keep Chat on Reconnect", "再接続時にチャットを保持", &["chat", "reconnect", "チャット", "再接続", "保持"]),
    entry(SettingKey::QolCrashReportMode, SettingsTab::Qol, "Crash Report Mode", "クラッシュレポート方式", &["crash", "report", "クラッシュ", "レポート"]),
    entry(SettingKey::QolShowSupportLinks, SettingsTab::Qol, "Support Links", "サポートリンク", &["support", "links", "サポート", "リンク", "help"]),
    entry(SettingKey::QolCompactDebugHud, SettingsTab::Qol, "Compact Debug HUD", "コンパクトデバッグHUD", &["hud", "mini", "minihud", "ミニ", "overlay"]), // trademark-hygiene:allow external-mod search aliases
    entry(SettingKey::QolShowSubtitles, SettingsTab::Qol, "Subtitles", "字幕", &["subtitles", "字幕", "captions", "caption"]),
    entry(SettingKey::QolAutoReconnect, SettingsTab::Qol, "Auto Reconnect", "自動再接続", &["reconnect", "auto", "再接続", "自動"]),
    entry(SettingKey::QolAutoReplenish, SettingsTab::Qol, "Auto Replenish", "自動補充", &["replenish", "補充", "auto", "refill"]),
    entry(SettingKey::QolQuickStack, SettingsTab::Qol, "Quick Stack", "クイックスタック", &["stack", "quick", "スタック", "収納"]),
    entry(SettingKey::QolFoodPreview, SettingsTab::Qol, "Food Preview", "食料プレビュー", &["food", "hunger", "食料", "満腹度", "食べ物"]),
    entry(SettingKey::QolShulkerPeek, SettingsTab::Qol, "Shulker Peek", "シュルカーボックス中身表示", &["shulker", "peek", "シュルカー", "中身"]),
    entry(SettingKey::QolChatScrollLock, SettingsTab::Qol, "Chat Scroll Lock", "チャットスクロール固定", &["chat", "scroll", "lock", "スクロール", "固定"]),
    entry(SettingKey::QolToastNotifications, SettingsTab::Qol, "Toast Notifications", "トースト通知", &["toast", "notification", "通知", "トースト"]),
    entry(SettingKey::QolRecipeOverlay, SettingsTab::Qol, "Recipe Overlay", "レシピオーバーレイ", &["recipe", "overlay", "レシピ", "jei", "rei"]), // trademark-hygiene:allow external-mod search aliases
    entry(SettingKey::FpMinimapMode, SettingsTab::FairPlay, "Minimap", "ミニマップ", &["minimap", "map", "ミニマップ", "マップ"]),
    entry(SettingKey::FpAllowCaveMap, SettingsTab::FairPlay, "Cave Mapping", "洞窟マッピング", &["cave", "map", "洞窟", "マッピング", "mapping"]),
    entry(SettingKey::FpAllowMobRadar, SettingsTab::FairPlay, "Mob Radar", "モブレーダー", &["radar", "mob", "レーダー", "mob radar"]),
    entry(SettingKey::FpPlayerRadarScope, SettingsTab::FairPlay, "Player Radar Scope", "プレイヤーレーダー範囲", &["radar", "player", "プレイヤー", "scope", "範囲"]),
    entry(SettingKey::FpAllowWaypointTeleport, SettingsTab::FairPlay, "Waypoint Teleport", "ウェイポイントテレポート", &["waypoint", "teleport", "テレポート", "ウェイポイント"]),
    entry(SettingKey::Language, SettingsTab::Language, "Language", "言語", &["language", "lang", "locale", "言語", "日本語", "japanese", "english"]),
];

/// Catalog row shorthand so the table above stays one line per setting.
const fn entry(
    key: SettingKey,
    tab: SettingsTab,
    name_en: &'static str,
    name_ja: &'static str,
    keywords: &'static [&'static str],
) -> SettingsSearchEntry {
    SettingsSearchEntry {
        key,
        tab,
        name_en,
        name_ja,
        keywords,
    }
}

/// Bilingual ranked search over the whole settings catalog.
///
/// The index is built once ([`SettingsSearchIndex::all_settings`]) and
/// queried per keystroke; queries are whitespace-tokenized and *every*
/// token must match somewhere (EN name, JA name, keyword, or tab name)
/// for an entry to rank — so `mouse 感度`, `fps cap` and `影` all work.
#[derive(Clone, Debug, Default)]
pub struct SettingsSearchIndex {
    entries: Vec<SettingsSearchEntry>,
}

impl SettingsSearchIndex {
    /// An index over the built-in catalog of every [`SettingKey`].
    pub fn all_settings() -> Self {
        Self {
            entries: SEARCH_CATALOG.to_vec(),
        }
    }

    /// The indexed entries, in catalog order.
    pub fn entries(&self) -> &[SettingsSearchEntry] {
        &self.entries
    }

    /// Ranked hits for `query`, best first. An empty or
    /// whitespace-only query returns no hits.
    pub fn search(&self, query: &str) -> Vec<SearchHit> {
        let tokens: Vec<String> = query
            .split_whitespace()
            .map(|t| t.to_lowercase())
            .filter(|t| !t.is_empty())
            .collect();
        if tokens.is_empty() {
            return Vec::new();
        }
        let mut hits: Vec<SearchHit> = self
            .entries
            .iter()
            .filter_map(|e| {
                score_entry(e, &tokens).map(|score| SearchHit {
                    key: e.key,
                    tab: e.tab,
                    name_en: e.name_en,
                    name_ja: e.name_ja,
                    score,
                })
            })
            .collect();
        hits.sort_by_key(|h| std::cmp::Reverse(h.score));
        hits
    }
}

/// Per-token contribution toward an entry's score. Returns `None` when
/// any token fails to match the entry at all (AND semantics).
fn score_entry(e: &SettingsSearchEntry, tokens: &[String]) -> Option<u32> {
    let name_en = e.name_en.to_lowercase();
    let mut total = 0u32;
    for token in tokens {
        let mut best = 0u32;
        if name_en == *token {
            best = best.max(120);
        } else if name_en.starts_with(token.as_str()) {
            best = best.max(95);
        } else if name_en.contains(token.as_str()) {
            best = best.max(70);
        }
        if e.name_ja.contains(token.as_str()) {
            best = best.max(100);
        }
        for kw in e.keywords {
            let kw_l = kw.to_lowercase();
            if kw_l == *token {
                best = best.max(110);
            } else if kw_l.starts_with(token.as_str()) {
                best = best.max(80);
            } else if kw_l.contains(token.as_str()) {
                best = best.max(55);
            } else if token.len() >= 3 && token.contains(kw_l.as_str()) {
                best = best.max(40);
            }
        }
        let tab_en = e.tab.label(UiLang::En).to_lowercase();
        let tab_ja = e.tab.label(UiLang::Ja);
        if tab_en.contains(token.as_str()) || tab_ja.contains(token.as_str()) {
            best = best.max(30);
        }
        if best == 0 {
            return None;
        }
        total += best;
    }
    Some(total)
}

// ---------------------------------------------------------------------------
// Live transparent settings preview backdrop
// ---------------------------------------------------------------------------

/// Minimum window width for the split-panel layout; narrower windows
/// fall back to a near-full-screen translucent panel.
const SPLIT_MIN_WIDTH: u32 = 1024;
/// Panel scrim alpha in split mode — translucent enough that shader /
/// FOV / render-distance changes read through the panel (~85%).
const PANEL_ALPHA_SPLIT: u8 = 216;
/// Panel scrim alpha in fallback mode — the panel covers most of the
/// window, so the world only bleeds through the scrim (~92%).
const PANEL_ALPHA_FALLBACK: u8 = 235;
/// Outer padding around both rects, in physical pixels.
const PREVIEW_PAD: i32 = 8;

/// How the window is divided while settings are open over a live world.
///
/// The host keeps rendering the world every frame: fully visible inside
/// [`Self::live_world_viewport_rect`], and dimmed behind the translucent
/// settings panel everywhere else (alpha [`Self::preview_alpha`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PreviewLayout {
    /// Alpha of the translucent panel scrim (`0..=255`, higher = more
    /// opaque). The world is never fully hidden — even in fallback mode
    /// a sliver of it bleeds through.
    pub preview_alpha: u8,
    /// The settings panel rect (left side, translucent scrim).
    pub settings_panel_rect: UiRect,
    /// The clear world viewport rect (right side, rendered untinted).
    /// Zero-sized when the window is too narrow for a split.
    pub live_world_viewport_rect: UiRect,
    /// `true` when the window was wide enough for a real split.
    pub split: bool,
}

/// Computes the translucent split-panel layout for the
/// live-settings-preview feature.
#[derive(Clone, Copy, Debug, Default)]
pub struct LivePreviewBackdrop;

impl LivePreviewBackdrop {
    /// Layout for a `window_w`×`window_h` framebuffer.
    ///
    /// Wide windows get a left settings panel (~9/16 of the width, at
    /// least 480 px) and a clear live world viewport on the right;
    /// narrow windows get a centered near-full-screen panel and an
    /// empty viewport — deterministic, `ui_scale`-independent.
    pub fn compute(window_w: u32, window_h: u32) -> PreviewLayout {
        let (w, h) = (window_w as i32, window_h as i32);
        let inner_h = (h - 2 * PREVIEW_PAD).max(0) as u32;
        if window_w >= SPLIT_MIN_WIDTH {
            let mut panel_w = ((window_w * 9) / 16).max(480);
            // Always keep at least 160 px of clear viewport.
            let max_panel = window_w.saturating_sub(160 + 2 * PREVIEW_PAD as u32);
            panel_w = panel_w.min(max_panel.max(160));
            let panel = UiRect::new(PREVIEW_PAD, PREVIEW_PAD, panel_w, inner_h);
            let vx = PREVIEW_PAD + panel_w as i32 + PREVIEW_PAD;
            let vw = (w - vx - PREVIEW_PAD).max(0) as u32;
            PreviewLayout {
                preview_alpha: PANEL_ALPHA_SPLIT,
                settings_panel_rect: panel,
                live_world_viewport_rect: UiRect::new(vx, PREVIEW_PAD, vw, inner_h),
                split: true,
            }
        } else {
            let margin = (w / 20).clamp(4, 24);
            let panel_w = (w - 2 * margin).max(0) as u32;
            PreviewLayout {
                preview_alpha: PANEL_ALPHA_FALLBACK,
                settings_panel_rect: UiRect::new(margin, PREVIEW_PAD, panel_w, inner_h),
                live_world_viewport_rect: UiRect::new(w - PREVIEW_PAD, PREVIEW_PAD, 0, inner_h),
                split: false,
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Universal drag-and-drop payload classifier + consent prompt
// ---------------------------------------------------------------------------

/// The payload kind a dropped filesystem path was identified as.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DroppedAssetKind {
    /// A Modrinth `.mrpack` export (`modrinth.index.json` present).
    Mrpack,
    /// A Wasm component mod (`.wasm`, valid `\0asm` module header).
    WasmMod,
    /// A local Java `.jar` mod (mod metadata marker present).
    LocalJavaModJar,
    /// A resource-pack `.zip` (`pack.mcmeta` + `assets/`).
    ResourcePackZip,
    /// A shader-pack `.zip` (`shaders/` tree present).
    ShaderPackZip,
    /// A 64×64 or 64×32 PNG player skin.
    PlayerSkinPng,
    /// A world save directory or zipped save (`level.dat` / `region/`).
    WorldSaveDirOrZip,
    /// Unrecognized or rejected — nothing is staged.
    Unsupported,
}

impl DroppedAssetKind {
    /// Whether this kind can ever lead to an install prompt.
    pub const fn installable(self) -> bool {
        !matches!(self, Self::Unsupported)
    }
}

/// Why a dropped payload was rejected (`kind == Unsupported`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DropRejection {
    /// The path does not exist or could not be stat'ed.
    Io(String),
    /// The path's name (or the name it resolves to) is on the
    /// secret-file denylist — never even opened.
    ForbiddenSecretName,
    /// The dropped path itself is a symlink — never followed.
    SymlinkPath,
    /// Extension and magic bytes did not match any supported payload.
    UnrecognizedKind,
    /// The PNG header was invalid or not a 64×64/64×32 player skin.
    BadSkinPng,
    /// The file exists but is not a readable/valid ZIP archive, or the
    /// archive failed a safety check (traversal name, encryption,
    /// multi-disk, special-file member, ...).
    UnsafeArchive(String),
    /// The archive's declared sizes exceed the decompression caps
    /// (zip-bomb guard).
    ArchiveBomb {
        /// Total declared uncompressed bytes.
        total_uncompressed: u64,
    },
    /// A ZIP that parses fine but lacks the marker files of any known
    /// payload kind.
    MissingMarkers,
}

/// The player's answer to a [`DropConfirmPrompt`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DropConsent {
    /// Install/enable the payload right now.
    ApplyNow,
    /// Keep it staged; activate on the next world load.
    StageForNextWorld,
    /// Do nothing — discard the staged payload.
    Cancel,
}

impl DropConsent {
    /// The three options every drop prompt offers, in display order.
    pub const ALL: [Self; 3] = [Self::ApplyNow, Self::StageForNextWorld, Self::Cancel];

    /// Bilingual button label.
    pub const fn label(self, lang: UiLang) -> &'static str {
        match (self, lang) {
            (Self::ApplyNow, UiLang::En) => "Apply Now",
            (Self::StageForNextWorld, UiLang::En) => "Stage for Next World",
            (Self::Cancel, UiLang::En) => "Cancel",
            (Self::ApplyNow, UiLang::Ja) => "今すぐ適用",
            (Self::StageForNextWorld, UiLang::Ja) => "次のワールドで有効化",
            (Self::Cancel, UiLang::Ja) => "キャンセル",
        }
    }
}

/// The mandatory bilingual consent dialog produced for every
/// installable drop. The classifier only *describes* payloads — the
/// host must show this prompt and wait for an explicit
/// [`DropConsent`]; nothing is ever installed silently.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DropConfirmPrompt {
    /// What the payload was classified as.
    pub kind: DroppedAssetKind,
    /// Display name of the dropped file/dir (basename only).
    pub asset_label: String,
    /// The ordered consent options (always all three).
    pub options: [DropConsent; 3],
}

impl DropConfirmPrompt {
    /// Bilingual dialog title for the payload kind.
    pub fn title(&self, lang: UiLang) -> String {
        match self.kind {
            DroppedAssetKind::Mrpack => {
                tr(lang, "Install modpack?", "このModパックを導入しますか？").to_string()
            }
            DroppedAssetKind::WasmMod => {
                tr(lang, "Install Wasm mod?", "このWasm Modを導入しますか？").to_string()
            }
            DroppedAssetKind::LocalJavaModJar => {
                tr(lang, "Install Java mod?", "このJava Modを導入しますか？").to_string()
            }
            DroppedAssetKind::ResourcePackZip => tr(
                lang,
                "Enable resource pack?",
                "このリソースパックを有効にしますか？",
            )
            .to_string(),
            DroppedAssetKind::ShaderPackZip => {
                tr(lang, "Enable shader pack?", "このシェーダーパックを有効にしますか？")
                    .to_string()
            }
            DroppedAssetKind::PlayerSkinPng => {
                tr(lang, "Change player skin?", "プレイヤーのスキンを変更しますか？").to_string()
            }
            DroppedAssetKind::WorldSaveDirOrZip => {
                tr(lang, "Import world save?", "このワールドをインポートしますか？").to_string()
            }
            DroppedAssetKind::Unsupported => {
                tr(lang, "Unsupported file", "未対応のファイルです").to_string()
            }
        }
    }

    /// Bilingual one-line body naming the dropped asset.
    pub fn message(&self, lang: UiLang) -> String {
        match lang {
            UiLang::En => format!("\"{}\" was dropped. Apply it?", self.asset_label),
            UiLang::Ja => format!("「{}」がドロップされました。適用しますか？", self.asset_label),
        }
    }
}

/// The full classifier verdict for one dropped path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DropVerdict {
    /// Identified payload kind (`Unsupported` when `rejection` is set).
    pub kind: DroppedAssetKind,
    /// Why the payload was rejected, if it was.
    pub rejection: Option<DropRejection>,
    /// The mandatory consent dialog — always present when `kind` is
    /// installable; the host must gate staging/install on it.
    pub prompt: Option<DropConfirmPrompt>,
}

/// Largest single dropped file the classifier will open (4 GiB).
const MAX_DROP_FILE_BYTES: u64 = 4 * 1024 * 1024 * 1024;
/// Largest declared uncompressed size of any single ZIP member (1 GiB).
const MAX_DROP_ENTRY_UNCOMPRESSED: u64 = 1024 * 1024 * 1024;
/// Largest total declared uncompressed size across all members (8 GiB).
const MAX_DROP_TOTAL_UNCOMPRESSED: u64 = 8 * 1024 * 1024 * 1024;

/// Stateless universal drop-payload classifier. All methods are
/// read-only: paths are stat'ed, archive central directories and PNG
/// headers are parsed in place, and nothing is extracted or written.
pub struct DropPayloadClassifier;

impl DropPayloadClassifier {
    /// Classify a dropped filesystem path.
    ///
    /// The verdict is fail-closed: anything unrecognized, unsafe or
    /// unreadable becomes `Unsupported` with a [`DropRejection`], and
    /// every installable kind comes with the mandatory
    /// [`DropConfirmPrompt`].
    pub fn classify_path(path: &Path) -> DropVerdict {
        let label = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();

        if ExistingLauncherImporter::is_forbidden_secret_path(path) {
            return reject(DropRejection::ForbiddenSecretName);
        }
        let meta = match fs::symlink_metadata(path) {
            Ok(m) => m,
            Err(e) => return reject(DropRejection::Io(e.to_string())),
        };
        if meta.file_type().is_symlink() {
            return reject(DropRejection::SymlinkPath);
        }
        if meta.len() > MAX_DROP_FILE_BYTES {
            return reject(DropRejection::ArchiveBomb {
                total_uncompressed: meta.len(),
            });
        }
        if meta.is_dir() {
            return Self::classify_dir(path, &label);
        }
        Self::finish(path, &label)
    }

    /// Extension-aware classification, with magic-byte sniffing as the
    /// fallback for extension-less or mislabeled payloads.
    fn finish(path: &Path, label: &str) -> DropVerdict {
        let ext = ext_lower(path);
        match ext.as_deref().unwrap_or("") {
            "wasm" => {
                if matches!(magic_sniff(path), Sniff::Wasm) {
                    DropVerdict::ok(DroppedAssetKind::WasmMod, label)
                } else {
                    reject(DropRejection::UnrecognizedKind)
                }
            }
            "png" => Self::classify_skin_png(path),
            "mrpack" | "jar" | "zip" => Self::classify_zip(path, ext.as_deref()),
            _ => match magic_sniff(path) {
                Sniff::Png => Self::classify_skin_png(path),
                Sniff::Zip => Self::classify_zip(path, ext.as_deref()),
                Sniff::Wasm => DropVerdict::ok(DroppedAssetKind::WasmMod, label),
                Sniff::Other => reject(DropRejection::UnrecognizedKind),
            },
        }
    }

    /// A dropped directory: a world save when it carries `level.dat` or
    /// a `region/` subdir, otherwise unsupported. Any symlinked
    /// immediate child fails closed.
    fn classify_dir(path: &Path, label: &str) -> DropVerdict {
        let rd = match fs::read_dir(path) {
            Ok(r) => r,
            Err(e) => return reject(DropRejection::Io(e.to_string())),
        };
        let mut is_save = false;
        for child in rd.flatten() {
            let ft = match child.file_type() {
                Ok(t) => t,
                Err(_) => continue,
            };
            if ft.is_symlink() {
                return reject(DropRejection::SymlinkPath);
            }
            let name = child.file_name().to_string_lossy().into_owned();
            if name == "level.dat" && ft.is_file() {
                is_save = true;
            }
            if name == "region" && ft.is_dir() {
                is_save = true;
            }
        }
        if is_save {
            DropVerdict::ok(DroppedAssetKind::WorldSaveDirOrZip, label)
        } else {
            reject(DropRejection::MissingMarkers)
        }
    }

    /// Validate the PNG signature + IHDR in place: a real player skin
    /// is exactly 64×64 (modern) or 64×32 (legacy). No other size is
    /// accepted, and the IHDR CRC must verify.
    fn classify_skin_png(path: &Path) -> DropVerdict {
        match read_png_header(path) {
            Ok((64, 64)) | Ok((64, 32)) => {
                DropVerdict::ok(DroppedAssetKind::PlayerSkinPng, &label_of(path))
            }
            _ => reject(DropRejection::BadSkinPng),
        }
    }

    /// Inspect a ZIP archive's central directory — never extracting —
    /// and classify by marker entries.
    fn classify_zip(path: &Path, ext: Option<&str>) -> DropVerdict {
        let label = label_of(path);
        let index = match LazyZipIndex::open_all(path) {
            Ok(i) => i,
            Err(e) => return reject(DropRejection::UnsafeArchive(e.to_string())),
        };
        let mut total = 0u64;
        let mut saw_modrinth = false;
        let mut saw_mod_marker = false;
        let mut saw_pack_mcmeta = false;
        let mut saw_assets = false;
        let mut saw_shader_tree = false;
        let mut saw_level_dat = false;
        let mut saw_region = false;
        for e in index.entries() {
            if e.uncompressed_size > MAX_DROP_ENTRY_UNCOMPRESSED {
                return reject(DropRejection::ArchiveBomb {
                    total_uncompressed: e.uncompressed_size,
                });
            }
            total = total.saturating_add(e.uncompressed_size);
            if total > MAX_DROP_TOTAL_UNCOMPRESSED {
                return reject(DropRejection::ArchiveBomb {
                    total_uncompressed: total,
                });
            }
            if e.archive_id == 0 && cde_is_special_file(path, e.central_header_offset) {
                return reject(DropRejection::UnsafeArchive(
                    "special-file member (symlink/device)".to_string(),
                ));
            }
            let name = e.name.as_str();
            let top = name.split('/').next().unwrap_or("");
            match name {
                "modrinth.index.json" => saw_modrinth = true,
                "fabric.mod.json" | "quilt.mod.json" | "mcmod.info"
                | "META-INF/mods.toml" | "META-INF/neoforge.mods.toml" => {
                    saw_mod_marker = true;
                }
                "pack.mcmeta" => saw_pack_mcmeta = true,
                "level.dat" => saw_level_dat = true,
                _ => {}
            }
            if top == "shaders" && (name.ends_with(".fsh") || name.ends_with(".vsh")) {
                saw_shader_tree = true;
            }
            if top == "assets" {
                saw_assets = true;
            }
            if top == "region" && name.ends_with(".mca") {
                saw_region = true;
            }
        }

        let kind = match ext {
            Some("mrpack") => {
                if saw_modrinth {
                    DroppedAssetKind::Mrpack
                } else {
                    return reject(DropRejection::MissingMarkers);
                }
            }
            Some("jar") => {
                if saw_mod_marker {
                    DroppedAssetKind::LocalJavaModJar
                } else {
                    return reject(DropRejection::MissingMarkers);
                }
            }
            _ => {
                if saw_shader_tree {
                    DroppedAssetKind::ShaderPackZip
                } else if saw_modrinth {
                    DroppedAssetKind::Mrpack
                } else if saw_level_dat || saw_region {
                    DroppedAssetKind::WorldSaveDirOrZip
                } else if saw_pack_mcmeta && saw_assets {
                    DroppedAssetKind::ResourcePackZip
                } else {
                    return reject(DropRejection::MissingMarkers);
                }
            }
        };
        DropVerdict::ok(kind, &label)
    }
}

fn reject(rejection: DropRejection) -> DropVerdict {
    DropVerdict {
        kind: DroppedAssetKind::Unsupported,
        rejection: Some(rejection),
        prompt: None,
    }
}

impl DropVerdict {
    fn ok(kind: DroppedAssetKind, label: &str) -> Self {
        Self {
            kind,
            rejection: None,
            prompt: Some(DropConfirmPrompt {
                kind,
                asset_label: label.to_string(),
                options: DropConsent::ALL,
            }),
        }
    }
}

fn label_of(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

fn ext_lower(path: &Path) -> Option<String> {
    path.extension()
        .map(|e| e.to_string_lossy().to_lowercase())
}

/// Content sniffing so extension-less renames still classify.
enum Sniff {
    Png,
    Zip,
    Wasm,
    Other,
}

fn magic_sniff(path: &Path) -> Sniff {
    let mut head = [0u8; 8];
    let n = match fs::File::open(path).and_then(|mut f| f.read(&mut head)) {
        Ok(n) => n,
        Err(_) => return Sniff::Other,
    };
    if n >= 8 && head[..8] == [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A] {
        return Sniff::Png;
    }
    if n >= 4 && head[..4] == [b'P', b'K', 0x03, 0x04] {
        return Sniff::Zip;
    }
    if n >= 4 && head[..4] == [0x00, 0x61, 0x73, 0x6D] {
        return Sniff::Wasm;
    }
    Sniff::Other
}

/// Parse `sig + IHDR` out of a PNG without decoding pixels. Returns
/// `(width, height)` or `Err` when the signature, chunk layout or IHDR
/// CRC does not check out.
fn read_png_header(path: &Path) -> Result<(u32, u32), ()> {
    let mut buf = [0u8; 33];
    let mut f = fs::File::open(path).map_err(|_| ())?;
    f.read_exact(&mut buf).map_err(|_| ())?;
    if buf[..8] != [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A] {
        return Err(());
    }
    let ihdr_len = u32::from_be_bytes([buf[8], buf[9], buf[10], buf[11]]);
    if ihdr_len != 13 || &buf[12..16] != b"IHDR" {
        return Err(());
    }
    let crc = u32::from_be_bytes([buf[29], buf[30], buf[31], buf[32]]);
    if megu_core::hash::crc32_ieee(&buf[12..29]) != crc {
        return Err(());
    }
    let w = u32::from_be_bytes([buf[16], buf[17], buf[18], buf[19]]);
    let h = u32::from_be_bytes([buf[20], buf[21], buf[22], buf[23]]);
    Ok((w, h))
}

/// Read the Central-Directory record at `offset` and return `true` when
/// its Unix mode bits mark a non-regular file (symlink, fifo, device,
/// socket). Re-parses only the 46 fixed bytes the
/// [`megu_core::zip_archive`] index deliberately does not expose;
/// non-Unix zips (mode `0`) pass.
fn cde_is_special_file(path: &Path, central_offset: u64) -> bool {
    const SIG_CDE: u32 = 0x0201_4b50;
    let mut fixed = [0u8; 46];
    let mut f = match fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return true, // fail closed
    };
    if f.seek(SeekFrom::Start(central_offset)).is_err() || f.read_exact(&mut fixed).is_err() {
        return true;
    }
    if u32::from_le_bytes([fixed[0], fixed[1], fixed[2], fixed[3]]) != SIG_CDE {
        return true;
    }
    let attrs = u32::from_le_bytes([fixed[38], fixed[39], fixed[40], fixed[41]]);
    let mode = (attrs >> 16) & 0o170000;
    mode != 0 && mode != 0o100000 && mode != 0o040000
}

// ---------------------------------------------------------------------------
// Existing-launcher importer (strict secret-file denylist)
// ---------------------------------------------------------------------------

/// Which existing launcher install a discovered profile belongs to.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum LauncherSource {
    /// `~/.minecraft`
    #[default]
    VanillaDotMinecraft,
    /// Flatpak `~/.var/app/com.mojang.Minecraft/.minecraft`
    VanillaFlatpak,
    /// Prism Launcher (`instances/<name>/.minecraft`).
    PrismLauncher,
    /// Modrinth App (`profiles/<name>`).
    ModrinthApp,
}

/// The safe user data found inside one launcher profile directory.
/// Only whitelisted artifacts are ever listed: `servers.dat`,
/// `options.txt`, save dirs containing `level.dat`, and
/// `resourcepacks/` entries — every candidate is filtered through
/// [`ExistingLauncherImporter::is_forbidden_secret_path`] first.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LauncherProfile {
    /// Which launcher this profile belongs to.
    pub source: LauncherSource,
    /// The profile/instance root that was scanned.
    pub root: PathBuf,
    /// `servers.dat` (multiplayer server list), when present.
    pub servers_dat: Option<PathBuf>,
    /// `options.txt` (keybinds/FOV/gamma/lang), when present.
    pub options_txt: Option<PathBuf>,
    /// World save directories (each contains `level.dat`).
    pub saves: Vec<PathBuf>,
    /// Entries inside `resourcepacks/` (zips or extracted dirs).
    pub resource_packs: Vec<PathBuf>,
}

/// Errors the importer can produce — deliberately tiny, and
/// [`ImportError::ForbiddenSecret`] is checked *before* any file is
/// opened, so a denied path is never read.
#[derive(Debug)]
pub enum ImportError {
    /// Filesystem I/O failed.
    Io(std::io::Error),
    /// The path (or its canonical target) is on the secret denylist.
    ForbiddenSecret(PathBuf),
    /// `options.txt` exceeded the parse cap (4 MiB — real files are
    /// a few KiB).
    TooLarge {
        /// Declared file size.
        size: u64,
    },
}

impl std::fmt::Display for ImportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "import I/O: {e}"),
            Self::ForbiddenSecret(p) => {
                write!(f, "refusing to touch denylisted secret path {}", p.display())
            }
            Self::TooLarge { size } => write!(f, "options.txt too large ({size} bytes)"),
        }
    }
}

impl std::error::Error for ImportError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for ImportError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

/// Parsed view of one `options.txt`: every raw `key:value` pair plus
/// the fields the migrator cares about.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ImportedOptions {
    /// All `key:value` pairs, verbatim.
    pub pairs: BTreeMap<String, String>,
    /// `fov` denormalized to degrees (`30..=110`); vanilla stores a
    /// `0.0..=1.0` slider over that range.
    pub fov_deg: Option<u16>,
    /// `gamma` as a percentage (`0..=100`).
    pub gamma_pct: Option<u16>,
    /// `lang` locale string (e.g. `en_us`, `ja_jp`).
    pub lang: Option<String>,
    /// `key_*` rebinds, `(action, key)` pairs with the `key_` prefix
    /// stripped (e.g. `("key.forward", "key.keyboard.w")` →
    /// `("forward", "key.keyboard.w")`).
    pub keybinds: Vec<(String, String)>,
}

/// Cap on `options.txt` size — the real file is a few KiB.
const MAX_OPTIONS_TXT_BYTES: u64 = 4 * 1024 * 1024;

/// Zero-pain importer for existing launcher installs.
///
/// Discovery and parsing are 100% hermetic: every entry point takes
/// explicit base paths, nothing probes the real home directory, and the
/// only files ever opened are `options.txt` (capped) plus directory
/// listings — after the denylist check.
pub struct ExistingLauncherImporter {
    roots: Vec<(LauncherSource, PathBuf)>,
}

impl ExistingLauncherImporter {
    /// An importer over explicit `(source, root)` candidates. Roots
    /// that do not exist are simply skipped by [`Self::discover`].
    pub fn new(roots: Vec<(LauncherSource, PathBuf)>) -> Self {
        Self { roots }
    }

    /// The conventional Linux candidate list under a *caller-supplied*
    /// `home` — passing a temp dir keeps tests fully hermetic.
    pub fn default_roots(home: &Path) -> Vec<(LauncherSource, PathBuf)> {
        vec![
            (LauncherSource::VanillaDotMinecraft, home.join(".minecraft")),
            (
                LauncherSource::VanillaFlatpak,
                home.join(".var/app/com.mojang.Minecraft/.minecraft"),
            ),
            (
                LauncherSource::PrismLauncher,
                home.join(".local/share/PrismLauncher"),
            ),
            (
                LauncherSource::PrismLauncher,
                home.join(".var/app/org.prismlauncher.PrismLauncher/data/PrismLauncher"),
            ),
            (LauncherSource::ModrinthApp, home.join(".local/share/com.modrinth.theseus")),
            (
                LauncherSource::ModrinthApp,
                home.join(".var/app/com.modrinth.theseus"),
            ),
        ]
    }

    /// An importer over the conventional candidates under `home`.
    pub fn with_default_candidates(home: &Path) -> Self {
        Self::new(Self::default_roots(home))
    }

    /// The hard denylist: `true` when `path` — at any depth — names a
    /// file or directory that may hold account credentials.
    ///
    /// Matching is case-insensitive, applies to *every* path component
    /// (so a denylisted parent taints everything under it), and also
    /// covers the canonicalized path so a symlink named innocently
    /// cannot smuggle a secret through.
    pub fn is_forbidden_secret_path(path: &Path) -> bool {
        let raw_hit = path.components().any(|c| {
            c.as_os_str()
                .to_str()
                .map(is_forbidden_name)
                .unwrap_or(false)
        });
        if raw_hit {
            return true;
        }
        if let Ok(canon) = path.canonicalize() {
            return canon.components().any(|c| {
                c.as_os_str()
                    .to_str()
                    .map(is_forbidden_name)
                    .unwrap_or(false)
            });
        }
        false
    }

    /// Walk every candidate root and report the profiles that exist,
    /// expanding launcher-specific instance layouts:
    /// Prism `instances/<name>/{.minecraft,minecraft}` and Modrinth
    /// `profiles/<name>` each yield their own profile.
    pub fn discover(&self) -> Vec<LauncherProfile> {
        let mut out = Vec::new();
        for (source, root) in &self.roots {
            for minecraft_dir in instance_dirs(*source, root) {
                if let Some(profile) = scan_profile(*source, &minecraft_dir) {
                    out.push(profile);
                }
            }
        }
        out
    }

    /// Parse one `options.txt` (`key:value` lines). The denylist is
    /// enforced before the file is opened — a forbidden path returns
    /// [`ImportError::ForbiddenSecret`] without a single read.
    pub fn read_options_txt(path: &Path) -> Result<ImportedOptions, ImportError> {
        if Self::is_forbidden_secret_path(path) {
            return Err(ImportError::ForbiddenSecret(path.to_path_buf()));
        }
        let size = fs::symlink_metadata(path)?.len();
        if size > MAX_OPTIONS_TXT_BYTES {
            return Err(ImportError::TooLarge { size });
        }
        let text = fs::read_to_string(path)?;
        Ok(parse_options_txt(&text))
    }
}

/// Does `name` match the secret-file denylist? Hard-coded, case
/// insensitive; covers launcher account stores, MSA credential caches,
/// Microsoft-account artifacts, token/key/credential/session/auth
/// files, certificate bundles, and — by substring so nothing with a
/// common auth-secret word slips through — any name containing
/// `token`, `secret`, `passwd` or `password`.
fn is_forbidden_name(name: &str) -> bool {
    let n = name.to_lowercase();
    n == "launcher_accounts.json"
        || n == "launcher_profiles.json"
        || n == "accounts.json"
        || n.starts_with("launcher_msa_credentials")
        || n.starts_with("microsoft")
        || n.starts_with("credentials")
        || n.starts_with("session")
        || n.starts_with("auth")
        || n.ends_with(".token")
        || n.ends_with(".key")
        || n.ends_with(".pem")
        || n.ends_with(".pfx")
        || n.contains("token")
        || n.contains("secret")
        || n.contains("passwd")
        || n.contains("password")
}

/// Expand a candidate root into the concrete `.minecraft`-style
/// directories to scan, honoring each launcher's instance layout.
/// A denylisted or symlinked root/component yields nothing.
fn instance_dirs(source: LauncherSource, root: &Path) -> Vec<PathBuf> {
    if !root.is_dir() || ExistingLauncherImporter::is_forbidden_secret_path(root) {
        return Vec::new();
    }
    let mut out = Vec::new();
    let children_of = |sub: &Path| -> Vec<PathBuf> {
        match fs::read_dir(sub) {
            Ok(rd) => rd
                .flatten()
                .filter(|e| {
                    e.file_type()
                        .map(|t| t.is_dir() && !t.is_symlink())
                        .unwrap_or(false)
                })
                .map(|e| e.path())
                .collect(),
            Err(_) => Vec::new(),
        }
    };
    match source {
        LauncherSource::PrismLauncher => {
            let mut any = false;
            for inst in children_of(&root.join("instances")) {
                for cand in [inst.join(".minecraft"), inst.join("minecraft")] {
                    if cand.is_dir() {
                        out.push(cand);
                        any = true;
                    }
                }
            }
            if !any {
                out.push(root.to_path_buf());
            }
        }
        LauncherSource::ModrinthApp => {
            let profiles = children_of(&root.join("profiles"));
            if profiles.is_empty() {
                out.push(root.to_path_buf());
            } else {
                out.extend(profiles);
            }
        }
        LauncherSource::VanillaDotMinecraft | LauncherSource::VanillaFlatpak => {
            out.push(root.to_path_buf());
        }
    }
    out
}

/// Scan one `.minecraft`-style directory for the whitelisted artifacts.
/// Returns `None` when nothing importable is present.
fn scan_profile(source: LauncherSource, root: &Path) -> Option<LauncherProfile> {
    if ExistingLauncherImporter::is_forbidden_secret_path(root) {
        return None;
    }
    let mut profile = LauncherProfile {
        source,
        root: root.to_path_buf(),
        ..Default::default()
    };

    for name in ["servers.dat", "options.txt"] {
        let p = root.join(name);
        if p.is_file() && !ExistingLauncherImporter::is_forbidden_secret_path(&p) {
            if name == "servers.dat" {
                profile.servers_dat = Some(p);
            } else {
                profile.options_txt = Some(p);
            }
        }
    }

    let saves_dir = root.join("saves");
    if let Ok(rd) = fs::read_dir(&saves_dir) {
        for e in rd.flatten() {
            let ft = match e.file_type() {
                Ok(t) => t,
                Err(_) => continue,
            };
            if !ft.is_dir() || ft.is_symlink() {
                continue;
            }
            let dir = e.path();
            if ExistingLauncherImporter::is_forbidden_secret_path(&dir) {
                continue;
            }
            if dir.join("level.dat").is_file() {
                profile.saves.push(dir);
            }
        }
        profile.saves.sort();
    }

    let packs_dir = root.join("resourcepacks");
    if let Ok(rd) = fs::read_dir(&packs_dir) {
        for e in rd.flatten() {
            let ft = match e.file_type() {
                Ok(t) => t,
                Err(_) => continue,
            };
            if ft.is_symlink() {
                continue;
            }
            let p = e.path();
            if ExistingLauncherImporter::is_forbidden_secret_path(&p) {
                continue;
            }
            profile.resource_packs.push(p);
        }
        profile.resource_packs.sort();
    }

    if profile.servers_dat.is_some()
        || profile.options_txt.is_some()
        || !profile.saves.is_empty()
        || !profile.resource_packs.is_empty()
    {
        Some(profile)
    } else {
        None
    }
}

/// Parse `options.txt` body (`key:value` lines, `#` comments ignored).
fn parse_options_txt(text: &str) -> ImportedOptions {
    let mut out = ImportedOptions::default();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((k, v)) = line.split_once(':') else {
            continue;
        };
        let (k, v) = (k.trim(), v.trim());
        out.pairs.insert(k.to_string(), v.to_string());
        match k {
            "fov" => {
                if let Ok(f) = v.parse::<f64>() {
                    let deg = (30.0 + f.clamp(0.0, 1.0) * 80.0).round() as u16;
                    out.fov_deg = Some(deg);
                }
            }
            "gamma" => {
                if let Ok(g) = v.parse::<f64>() {
                    out.gamma_pct = Some((g.clamp(0.0, 1.0) * 100.0).round() as u16);
                }
            }
            "lang" => out.lang = Some(v.to_string()),
            _ => {
                // Vanilla writes `key_key.forward` — the options-file
                // `key_` prefix plus the binding's own `key.` prefix.
                if let Some(action) = k.strip_prefix("key_") {
                    let action = action.strip_prefix("key.").unwrap_or(action);
                    out.keybinds.push((action.to_string(), v.to_string()));
                }
            }
        }
    }
    out
}
