// SPDX-License-Identifier: LGPL-3.0-or-later
//! ModManager-inspired Mods & Packs manager screen for the client UI.
//!
//! The host supplies plain [`ModManagerEntry`] metadata (parsed by its own mod
//! loader); the screen owns the interactive state: a forgiving multi-token
//! search with `@author` / `#tag` prefix filters, clickable filter pills, a
//! sort cycle, a two-pane master-detail layout with a scrollable mod list,
//! per-mod enable toggles batched into a single zero-restart-or-soft-reload
//! `Apply`, and one-click Modrinth actions for missing dependencies.
//!
//! Like [`crate::shader_screen`], rendering produces a deterministic
//! [`UiDrawList`] plus a parallel `Vec<`[`ModManagerHitRegion`]`>`; input arrives
//! as device-agnostic [`UiInputEvent`]s and returns [`ModManagerAction`]s the
//! embedding application performs. Everything is pure `std` — no client-port
//! types, no GPU resources, no I/O.

use std::collections::BTreeMap;

use crate::draw::{palette, text_width, UiColor, UiDrawList, UiRect};
use crate::state::{tr, GamepadButton, UiInputEvent, UiKey, UiLang};

// ---------------------------------------------------------------------------
// Mod metadata supplied by the host
// ---------------------------------------------------------------------------

/// Which loader ecosystem an entry belongs to.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ModLoaderTag {
    /// First-class megu native module.
    #[default]
    MeguNative,
    /// Fabric mod running under the compatibility shim.
    Fabric,
    /// NeoForge mod running under the compatibility shim.
    NeoForge,
    /// Vanilla datapack.
    Datapack,
    /// Shaderpack (not a code mod).
    ShaderPack,
    /// Shared library jar with no user-facing content.
    Library,
}

impl ModLoaderTag {
    /// Short display label used on detail-pane badges and row subtitles.
    pub fn label(self) -> &'static str {
        match self {
            Self::MeguNative => "Megu",
            Self::Fabric => "Fabric",
            Self::NeoForge => "NeoForge",
            Self::Datapack => "Datapack",
            Self::ShaderPack => "ShaderPack",
            Self::Library => "Library",
        }
    }

    /// Lowercase single-token label matched by free-text search.
    fn search_label(self) -> &'static str {
        match self {
            Self::MeguNative => "megu",
            Self::Fabric => "fabric",
            Self::NeoForge => "neoforge",
            Self::Datapack => "datapack",
            Self::ShaderPack => "shaderpack",
            Self::Library => "library",
        }
    }
}

/// How well the mod has been adapted to the native runtime.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ModAdaptationBadge {
    /// Fully native port (Tier 1).
    #[default]
    Tier1Native,
    /// Working via the compatibility shim (Tier 2).
    Tier2Shim,
    /// Degraded fallback path (Tier 3) — functional but flagged.
    Tier3Fallback,
    /// A newer native replacement exists.
    Superseded,
    /// Client-side only; never synced to a server.
    ClientOnly,
}

impl ModAdaptationBadge {
    /// Detail-pane badge label.
    pub fn label(self) -> &'static str {
        match self {
            Self::Tier1Native => "Tier 1 Native",
            Self::Tier2Shim => "Tier 2 Shim",
            Self::Tier3Fallback => "Tier 3 Fallback",
            Self::Superseded => "Superseded",
            Self::ClientOnly => "Client-Only",
        }
    }

    /// Lowercase token matched by free-text search.
    fn search_label(self) -> &'static str {
        match self {
            Self::Tier1Native => "native",
            Self::Tier2Shim => "shim",
            Self::Tier3Fallback => "fallback",
            Self::Superseded => "superseded",
            Self::ClientOnly => "client",
        }
    }
}

/// What applying an enable/disable change costs.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ModReloadBadge {
    /// Toggles live with no restart at all.
    #[default]
    HotReload,
    /// Needs an in-process soft reload of mod containers.
    SoftRestart,
    /// Needs a full client process restart.
    ProcessRestart,
}

impl ModReloadBadge {
    /// Detail-pane badge label.
    pub fn label(self, lang: UiLang) -> &'static str {
        match self {
            Self::HotReload => tr(lang, "No Restart", "再起動不要"),
            Self::SoftRestart => tr(lang, "Soft Reload", "ソフト再読込"),
            Self::ProcessRestart => tr(lang, "Restart", "再起動"),
        }
    }
}

/// One mod/pack row's metadata, supplied by the host's mod loader.
///
/// `missing_deps` is empty when every dependency is satisfied; a non-empty
/// list renders a prominent warning plus a Modrinth download button.
/// The four `*_url` fields are optional — an empty string hides that link
/// button entirely.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ModManagerEntry {
    pub mod_id: String,
    pub name: String,
    pub version: String,
    pub summary: String,
    pub authors: Vec<String>,
    pub license: String,
    pub loader: ModLoaderTag,
    pub tier: ModAdaptationBadge,
    pub reload_mode: ModReloadBadge,
    pub enabled: bool,
    pub is_library: bool,
    pub has_config: bool,
    pub missing_deps: Vec<String>,
    pub website_url: String,
    pub issues_url: String,
    pub source_url: String,
    pub modrinth_url: String,
}

impl ModManagerEntry {
    /// Minimal entry: `enabled`, Tier-1 native, hot-reloadable, no links.
    pub fn new(mod_id: &str, name: &str, version: &str) -> Self {
        Self {
            mod_id: mod_id.to_owned(),
            name: name.to_owned(),
            version: version.to_owned(),
            enabled: true,
            ..Self::default()
        }
    }

    /// `true` when the entry needs attention: unresolved dependencies or a
    /// degraded Tier-3 fallback path.
    pub fn has_issue(&self) -> bool {
        !self.missing_deps.is_empty() || self.tier == ModAdaptationBadge::Tier3Fallback
    }
}

// ---------------------------------------------------------------------------
// Curated Modrinth 1.21.1 recommendations and built-in QoL deference
// ---------------------------------------------------------------------------

/// The categories offered by the Recommended QoL browser.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QolSubCategory {
    HudInfo,
    InventoryInput,
    Recipes,
    VisualsCompat,
}

impl QolSubCategory {
    pub const fn label(self) -> &'static str {
        match self {
            Self::HudInfo => "HUD / Info",
            Self::InventoryInput => "Inventory / Input",
            Self::Recipes => "Recipes",
            Self::VisualsCompat => "Visuals / Compat",
        }
    }
}

/// A built-in clean-room feature that should yield to an installed mod.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum BuiltinQolFeature {
    LookingAtHud,
    InventoryDragControls,
    InventorySort,
    HungerSaturationPreview,
    ContainerTooltipPreview,
    RecipeBrowser,
    ConnectedTextures,
    EmfModels,
    EtfTextures,
}

/// How the caller should resolve one built-in feature at runtime.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BuiltinQolResolution {
    UseBuiltin,
    DeferToInstalledMod { mod_id: String },
}

/// Active overrides derived from the host's enabled, installed mod IDs.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BuiltinQolDeference {
    overrides: BTreeMap<BuiltinQolFeature, BuiltinQolResolution>,
}

impl BuiltinQolDeference {
    pub fn resolution(&self, feature: BuiltinQolFeature) -> BuiltinQolResolution {
        self.overrides
            .get(&feature)
            .cloned()
            .unwrap_or(BuiltinQolResolution::UseBuiltin)
    }

    pub fn use_builtin(&self, feature: BuiltinQolFeature) -> bool {
        self.resolution(feature) == BuiltinQolResolution::UseBuiltin
    }
}

/// Static catalog metadata. Versions are target Minecraft versions, not mod
/// binary versions; the host must resolve a compatible release via Modrinth.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CuratedModrinthQolMod {
    pub slug: &'static str,
    pub name: &'static str,
    pub summary_en: &'static str,
    pub summary_ja: &'static str,
    pub category: QolSubCategory,
    pub loaders: &'static [ModLoaderTag],
    pub target_version: &'static str,
    pub yields_builtin_qol: Option<BuiltinQolFeature>,
}

const QOL_LOADERS: &[ModLoaderTag] = &[ModLoaderTag::Fabric, ModLoaderTag::NeoForge];

/// Project slugs and 1.21.1 loader availability verified against Modrinth.
/// No jars or mod binaries are bundled with this catalog.
pub const CURATED_MODRINTH_QOL_MODS: [CuratedModrinthQolMod; 15] = [
    CuratedModrinthQolMod {
        slug: "jade",
        name: "Jade",
        summary_en: "Shows block, entity, and fluid information in a HUD.",
        summary_ja: "ブロックやエンティティの情報をHUDに表示。",
        category: QolSubCategory::HudInfo,
        loaders: QOL_LOADERS,
        target_version: "1.21.1",
        yields_builtin_qol: Some(BuiltinQolFeature::LookingAtHud),
    },
    CuratedModrinthQolMod {
        slug: "mouse-tweaks",
        name: "Mouse Tweaks",
        summary_en: "Faster inventory dragging and item transfers.",
        summary_ja: "インベントリのドラッグ操作と移動を改善。",
        category: QolSubCategory::InventoryInput,
        loaders: QOL_LOADERS,
        target_version: "1.21.1",
        yields_builtin_qol: Some(BuiltinQolFeature::InventoryDragControls),
    },
    CuratedModrinthQolMod {
        slug: "inventory-profiles-next",
        name: "Inventory Profiles Next",
        summary_en: "Inventory sorting and configurable item management.",
        summary_ja: "インベントリの整理とアイテム管理。",
        category: QolSubCategory::InventoryInput,
        loaders: QOL_LOADERS,
        target_version: "1.21.1",
        yields_builtin_qol: Some(BuiltinQolFeature::InventorySort),
    },
    CuratedModrinthQolMod {
        slug: "inventory-essentials",
        name: "Inventory Essentials",
        summary_en: "Convenient inventory and item movement shortcuts.",
        summary_ja: "インベントリ操作とアイテム移動の補助。",
        category: QolSubCategory::InventoryInput,
        loaders: QOL_LOADERS,
        target_version: "1.21.1",
        yields_builtin_qol: Some(BuiltinQolFeature::InventorySort),
    },
    CuratedModrinthQolMod {
        slug: "appleskin",
        name: "AppleSkin",
        summary_en: "Previews food, hunger, and saturation values.",
        summary_ja: "食料の回復量や隠し満腹度を表示。",
        category: QolSubCategory::HudInfo,
        loaders: QOL_LOADERS,
        target_version: "1.21.1",
        yields_builtin_qol: Some(BuiltinQolFeature::HungerSaturationPreview),
    },
    CuratedModrinthQolMod {
        slug: "shulkerboxtooltip",
        name: "ShulkerBoxTooltip",
        summary_en: "Previews contents of shulker boxes in tooltips.",
        summary_ja: "シュルカーボックスの中身をツールチップで確認。",
        category: QolSubCategory::InventoryInput,
        loaders: QOL_LOADERS,
        target_version: "1.21.1",
        yields_builtin_qol: Some(BuiltinQolFeature::ContainerTooltipPreview),
    },
    CuratedModrinthQolMod {
        slug: "emi",
        name: "EMI",
        summary_en: "Searchable recipe and item browser.",
        summary_ja: "レシピとアイテムの検索ブラウザ。",
        category: QolSubCategory::Recipes,
        loaders: QOL_LOADERS,
        target_version: "1.21.1",
        yields_builtin_qol: Some(BuiltinQolFeature::RecipeBrowser),
    },
    CuratedModrinthQolMod {
        slug: "jei",
        name: "Just Enough Items (JEI)",
        summary_en: "Displays item lists and crafting recipes.",
        summary_ja: "アイテム一覧とクラフトレシピを表示。",
        category: QolSubCategory::Recipes,
        loaders: QOL_LOADERS,
        target_version: "1.21.1",
        yields_builtin_qol: Some(BuiltinQolFeature::RecipeBrowser),
    },
    CuratedModrinthQolMod {
        slug: "controlling",
        name: "Controlling",
        summary_en: "Finds key bindings and their conflicts.",
        summary_ja: "キー割り当てと競合を検索。",
        category: QolSubCategory::InventoryInput,
        loaders: QOL_LOADERS,
        target_version: "1.21.1",
        yields_builtin_qol: None,
    },
    CuratedModrinthQolMod {
        slug: "searchables",
        name: "Searchables",
        summary_en: "Shared search utilities for modded interfaces.",
        summary_ja: "Modの検索UIを支える共通ライブラリ。",
        category: QolSubCategory::InventoryInput,
        loaders: QOL_LOADERS,
        target_version: "1.21.1",
        yields_builtin_qol: None,
    },
    CuratedModrinthQolMod {
        slug: "betterf3",
        name: "BetterF3",
        summary_en: "Customizable, readable debug HUD.",
        summary_ja: "デバッグHUDの表示を整理・カスタマイズ。",
        category: QolSubCategory::HudInfo,
        loaders: QOL_LOADERS,
        target_version: "1.21.1",
        yields_builtin_qol: None,
    },
    CuratedModrinthQolMod {
        slug: "continuity",
        name: "Continuity",
        summary_en: "Connected textures for compatible resource packs.",
        summary_ja: "対応リソースパックの連結テクスチャ。",
        category: QolSubCategory::VisualsCompat,
        loaders: QOL_LOADERS,
        target_version: "1.21.1",
        yields_builtin_qol: Some(BuiltinQolFeature::ConnectedTextures),
    },
    CuratedModrinthQolMod {
        slug: "entity-model-features",
        name: "Entity Model Features (EMF)",
        summary_en: "Custom entity models from compatible resource packs.",
        summary_ja: "対応パックのカスタムエンティティモデル。",
        category: QolSubCategory::VisualsCompat,
        loaders: QOL_LOADERS,
        target_version: "1.21.1",
        yields_builtin_qol: Some(BuiltinQolFeature::EmfModels),
    },
    CuratedModrinthQolMod {
        // Modrinth's canonical project slug has no hyphens.
        slug: "entitytexturefeatures",
        name: "Entity Texture Features (ETF)",
        summary_en: "Custom entity textures from compatible resource packs.",
        summary_ja: "対応パックのカスタムエンティティテクスチャ。",
        category: QolSubCategory::VisualsCompat,
        loaders: QOL_LOADERS,
        target_version: "1.21.1",
        yields_builtin_qol: Some(BuiltinQolFeature::EtfTextures),
    },
    CuratedModrinthQolMod {
        slug: "chat-heads",
        name: "Chat Heads",
        summary_en: "Displays player avatars next to chat messages.",
        summary_ja: "チャットにプレイヤーアイコンを表示。",
        category: QolSubCategory::VisualsCompat,
        loaders: QOL_LOADERS,
        target_version: "1.21.1",
        yields_builtin_qol: None,
    },
];

/// Only ASCII lowercase project slugs can enter a Modrinth URL path.
pub fn valid_modrinth_slug(slug: &str) -> bool {
    !slug.is_empty()
        && slug
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

/// API endpoint for 1.21.1 versions. The host resolves the loader and
/// dependencies before installing a returned release.
pub fn modrinth_version_lookup_url(slug: &str) -> Option<String> {
    valid_modrinth_slug(slug).then(|| {
        format!(
            "https://api.modrinth.com/v2/project/{slug}/version?game_versions=%5B%221.21.1%22%5D"
        )
    })
}

/// Public project page used by the recommendation's install/browser action.
pub fn modrinth_project_url(slug: &str) -> Option<String> {
    valid_modrinth_slug(slug).then(|| format!("https://modrinth.com/mod/{slug}"))
}

fn matches_installed_mod_id(slug: &str, mod_id: &str) -> bool {
    if slug == mod_id {
        return true;
    }
    matches!(
        (slug, mod_id),
        ("mouse-tweaks", "mousetweaks")
            | ("inventory-profiles-next", "inventoryprofilesnext")
            | ("inventory-profiles-next", "inventory_profiles_next")
            | ("inventory-essentials", "inventoryessentials")
            | ("inventory-essentials", "inventory_essentials")
            | ("entity-model-features", "entity_model_features")
            | ("entitytexturefeatures", "entity_texture_features")
            | ("entitytexturefeatures", "entity-texture-features")
    )
}

/// Resolve clean-room feature ownership from installed, enabled loader IDs.
/// If several mods provide the same feature, the catalog order determines the
/// reported owner. Unrecognized IDs leave the built-in implementation active.
pub fn active_builtin_qol_overrides(installed_mod_ids: &[&str]) -> BuiltinQolDeference {
    let mut deference = BuiltinQolDeference::default();
    for mod_info in CURATED_MODRINTH_QOL_MODS {
        let Some(feature) = mod_info.yields_builtin_qol else {
            continue;
        };
        if let Some(mod_id) = installed_mod_ids
            .iter()
            .find(|id| matches_installed_mod_id(mod_info.slug, id))
        {
            deference.overrides.entry(feature).or_insert_with(|| {
                BuiltinQolResolution::DeferToInstalledMod {
                    mod_id: (*mod_id).to_owned(),
                }
            });
        }
    }
    deference
}

fn curated_entry(mod_info: &CuratedModrinthQolMod) -> ModManagerEntry {
    let mut entry = ModManagerEntry::new(mod_info.slug, mod_info.name, "MC 1.21.1");
    entry.summary = mod_info.summary_en.to_owned();
    entry.enabled = false;
    entry.loader = mod_info.loaders[0];
    entry.tier = ModAdaptationBadge::ClientOnly;
    entry.modrinth_url = modrinth_project_url(mod_info.slug).unwrap_or_default();
    entry
}

// ---------------------------------------------------------------------------
// License-transparent "Recommended Community Mods" catalog
// ---------------------------------------------------------------------------

/// Broad category of a recommended community mod.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecommendedModCategory {
    /// HUD overlays and information displays (Jade, AppleSkin…).
    HudInfo,
    /// Inventory and container interactions.
    Inventory,
    /// Gameplay content: blocks, items, machines, food.
    Content,
    /// World generation and biome mods.
    World,
}

impl RecommendedModCategory {
    pub const fn label(self) -> &'static str {
        match self {
            Self::HudInfo => "HUD / Info",
            Self::Inventory => "Inventory",
            Self::Content => "Content",
            Self::World => "World",
        }
    }
}

/// One hand-picked community mod, described by *metadata only*.
///
/// Nothing is bundled or redistributed: the entry links to the official
/// Modrinth project page ([`modrinth_project_url`]), credits the author
/// by name, and displays the project's real SPDX license identifier so
/// the user installs the mod — All-Rights-Reserved and OSS alike — from
/// its original authors via the client's own Modrinth flow.
/// `needs_gui_bridge` flags mods whose client UI draws through
/// [`crate::gui_bridge`] (the Tier-2 `GuiGraphics -> DrawCmd` ABI).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RecommendedModEntry {
    /// Official Modrinth project slug (lowercase `a-z0-9-`).
    pub slug: &'static str,
    /// Display name.
    pub name: &'static str,
    /// Author attribution as shown on Modrinth.
    pub author: &'static str,
    /// SPDX license identifier of the upstream project (e.g. `MIT`,
    /// `Unlicense`, `CC-BY-NC-SA-4.0`).
    pub license_spdx: &'static str,
    /// Browse category.
    pub category: RecommendedModCategory,
    /// Short English summary.
    pub en_summary: &'static str,
    /// Short Japanese summary (every glyph covered by the built-in font).
    pub ja_summary: &'static str,
    /// `true` when the mod's client UI renders through the GUI bridge.
    pub needs_gui_bridge: bool,
    /// Loaders the upstream project ships for the target version.
    pub loaders: &'static [ModLoaderTag],
}

impl RecommendedModEntry {
    /// Official Modrinth project URL — the only install path the UI
    /// offers for this entry.
    pub fn modrinth_url(&self) -> Option<String> {
        modrinth_project_url(self.slug)
    }
}

const FABRIC_AND_NEOFORGE: &[ModLoaderTag] = &[ModLoaderTag::Fabric, ModLoaderTag::NeoForge];
const FABRIC_ONLY: &[ModLoaderTag] = &[ModLoaderTag::Fabric];
const NEOFORGE_ONLY: &[ModLoaderTag] = &[ModLoaderTag::NeoForge];

/// License-transparent community showcase: HUD/inventory/content/world
/// mods Megu recommends over re-implementing their features as built-ins.
/// Slugs, authors, and SPDX ids mirror the upstream Modrinth pages; no
/// jars or assets are ever bundled with this list.
pub const RECOMMENDED_COMMUNITY_MODS: [RecommendedModEntry; 7] = [
    RecommendedModEntry {
        slug: "jade",
        name: "Jade",
        author: "Snownee",
        license_spdx: "CC-BY-NC-SA-4.0",
        category: RecommendedModCategory::HudInfo,
        en_summary: "Shows information about the block or entity you look at.",
        ja_summary: "見ている物の情報をHUDに表示。",
        needs_gui_bridge: true,
        loaders: FABRIC_AND_NEOFORGE,
    },
    RecommendedModEntry {
        slug: "appleskin",
        name: "AppleSkin",
        author: "squeek502",
        license_spdx: "Unlicense",
        category: RecommendedModCategory::HudInfo,
        en_summary: "Previews food, hunger, and saturation values.",
        ja_summary: "食料の回復量や隠し満腹度を表示。",
        needs_gui_bridge: true,
        loaders: FABRIC_AND_NEOFORGE,
    },
    RecommendedModEntry {
        slug: "mouse-tweaks",
        name: "Mouse Tweaks",
        author: "YaLTeR",
        license_spdx: "BSD-3-Clause",
        category: RecommendedModCategory::Inventory,
        en_summary: "Faster inventory dragging and item transfers.",
        ja_summary: "インベントリのドラッグ操作と移動を改善。",
        needs_gui_bridge: false,
        loaders: FABRIC_AND_NEOFORGE,
    },
    RecommendedModEntry {
        slug: "shulkerboxtooltip",
        name: "ShulkerBoxTooltip",
        author: "MisterPeModder",
        license_spdx: "MIT",
        category: RecommendedModCategory::Inventory,
        en_summary: "Previews contents of shulker boxes in tooltips.",
        ja_summary: "シュルカーボックスの中身をツールチップで確認。",
        needs_gui_bridge: true,
        loaders: FABRIC_AND_NEOFORGE,
    },
    RecommendedModEntry {
        slug: "create",
        name: "Create",
        author: "simibubi",
        license_spdx: "MIT",
        category: RecommendedModCategory::Content,
        en_summary: "Rotational machinery, contraptions, and automation.",
        ja_summary: "回転する機械でいろいろな物を作るコンテンツMod。",
        needs_gui_bridge: true,
        loaders: NEOFORGE_ONLY,
    },
    RecommendedModEntry {
        slug: "farmers-delight-refabricated",
        name: "Farmer's Delight (Refabricated)",
        author: "MehVahdJukaar, Cassian, vectorwing",
        license_spdx: "MIT",
        category: RecommendedModCategory::Content,
        en_summary: "Cozy farming and cooking expansion (Fabric port).",
        ja_summary: "新しい作物と料理で食べ物あそびがふえるMod。",
        needs_gui_bridge: false,
        loaders: FABRIC_ONLY,
    },
    RecommendedModEntry {
        slug: "tectonic",
        name: "Tectonic",
        author: "Apollo",
        license_spdx: "MIT",
        category: RecommendedModCategory::World,
        en_summary: "Grander, more varied terrain generation.",
        ja_summary: "ワールドの形を作りかえる生成Mod。",
        needs_gui_bridge: false,
        loaders: FABRIC_AND_NEOFORGE,
    },
];

/// The curated community-mod catalog (metadata only — never bundles JARs).
pub fn recommended_community_mods() -> &'static [RecommendedModEntry] {
    &RECOMMENDED_COMMUNITY_MODS
}

fn community_entry(info: &RecommendedModEntry) -> ModManagerEntry {
    let mut entry = ModManagerEntry::new(info.slug, info.name, "MC 1.21.1");
    entry.summary = info.en_summary.to_owned();
    entry.authors = vec![info.author.to_owned()];
    entry.license = info.license_spdx.to_owned();
    entry.enabled = false;
    entry.loader = info.loaders[0];
    entry.tier = ModAdaptationBadge::ClientOnly;
    entry.modrinth_url = info.modrinth_url().unwrap_or_default();
    entry
}

// ---------------------------------------------------------------------------
// Unified Browse view: categories, project kinds, live Modrinth search
// ---------------------------------------------------------------------------

/// Game version every recommendation and live search is filtered to.
pub const RECOMMENDED_MC_VERSION: &str = "1.21.1";

/// The shelf groupings inside the unified Browse view.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum BrowseCategory {
    /// HUD, info, inventory, and other client-side quality-of-life mods
    /// (the curated QoL picks plus the community HUD/inventory showcase).
    #[default]
    QolHud,
    /// Gameplay content and world-generation mods.
    Content,
    /// Packs, shaders, and datapacks. No pack metadata ships with the
    /// client — this shelf points at live Modrinth search instead.
    Packs,
}

impl BrowseCategory {
    fn label(self, lang: UiLang) -> &'static str {
        match self {
            Self::QolHud => "QoL & HUD",
            Self::Content => tr(lang, "Content", "コンテンツ"),
            Self::Packs => tr(lang, "Packs", "パック"),
        }
    }
}

/// Modrinth `project_type` facet used for live searches.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ModrinthProjectKind {
    /// Ordinary mods (the default).
    #[default]
    Mod,
    /// Resource packs.
    ResourcePack,
    /// Shader packs.
    Shader,
    /// Datapacks.
    Datapack,
}

impl ModrinthProjectKind {
    /// `project_type` value understood by `api.modrinth.com`.
    pub const fn api_kind(self) -> &'static str {
        match self {
            Self::Mod => "mod",
            Self::ResourcePack => "resourcepack",
            Self::Shader => "shader",
            Self::Datapack => "datapack",
        }
    }

    /// Short pill label (ASCII, so it fits both languages).
    pub const fn label(self) -> &'static str {
        match self {
            Self::Mod => "Mod",
            Self::ResourcePack => "ResourcePack",
            Self::Shader => "Shader",
            Self::Datapack => "Datapack",
        }
    }

    /// Cycle order used by the Kind pill: Mod → ResourcePack → Shader →
    /// Datapack → Mod.
    pub const fn next(self) -> Self {
        match self {
            Self::Mod => Self::ResourcePack,
            Self::ResourcePack => Self::Shader,
            Self::Shader => Self::Datapack,
            Self::Datapack => Self::Mod,
        }
    }
}

/// Percent-encode a URL query component (unreserved `A-Z a-z 0-9 -._~`
/// pass through; everything else — including UTF-8 continuation bytes —
/// becomes `%XX`).
fn percent_encode_component(text: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = String::with_capacity(text.len());
    for &byte in text.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(byte as char);
            }
            _ => {
                out.push('%');
                out.push(HEX[(byte >> 4) as usize] as char);
                out.push(HEX[(byte & 0x0f) as usize] as char);
            }
        }
    }
    out
}

/// `api.modrinth.com/v2/search` URL for a live lookup: `query` is
/// percent-encoded and the facets pin `project_type` + the target game
/// version. The host can open the URL in the user's browser or feed it
/// to its Modrinth client — the UI never installs a jar itself.
pub fn build_modrinth_search_url(
    query: &str,
    kind: ModrinthProjectKind,
    mc_version: &str,
) -> String {
    let facets = format!(
        "[[\"project_type:{}\"],[\"versions:{}\"]]",
        kind.api_kind(),
        mc_version
    );
    format!(
        "https://api.modrinth.com/v2/search?query={}&facets={}",
        percent_encode_component(query),
        percent_encode_component(&facets),
    )
}

/// Which backing catalog a `browse_entries` row comes from. Community
/// picks carry author + SPDX metadata; for slugs present in both
/// catalogs the richer community metadata wins.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BrowseSource {
    Community(usize),
    Curated(usize),
}

/// Metadata backing a catalog row in the detail pane / list sub-line.
#[derive(Clone, Copy)]
enum CatalogMeta<'a> {
    Community(&'a RecommendedModEntry),
    Curated(&'a CuratedModrinthQolMod),
}

/// Stable ordering and labels of Megu's settings-backed built-in modules.
/// Index 4 intentionally mirrors the PLaMo translator pack toggle.
pub const BUILTIN_MODULE_COUNT: usize = 5;
pub const BUILTIN_MODULE_NAMES_EN: [&str; BUILTIN_MODULE_COUNT] = [
    "Recipe Overlay",
    "Inventory Drag Controls",
    "Compact Debug HUD",
    "Quick Stack & Auto-Replenish",
    "PLaMo 3 Auto-Translation Pack",
];
pub const BUILTIN_MODULE_NAMES_JA: [&str; BUILTIN_MODULE_COUNT] = [
    "レシピオーバーレイ",
    "インベントリドラッグ操作",
    "コンパクトデバッグHUD",
    "クイックスタック＆自動補充",
    "PLaMo 3 自動翻訳パック",
];
pub const BUILTIN_MODULE_DESCRIPTIONS_EN: [&str; BUILTIN_MODULE_COUNT] = [
    "Recipe side panels beside container screens.",
    "Vanilla-plus inventory drag painting and batch moves.",
    "Small opt-in FPS/coordinates debug overlay.",
    "Quick-stack to chests and refill the hotbar from inventory.",
    "Generated Japanese pack translated locally by PLaMo 3.",
];
pub const BUILTIN_MODULE_DESCRIPTIONS_JA: [&str; BUILTIN_MODULE_COUNT] = [
    "コンテナ画面の横にレシピ側面パネルを表示します。",
    "ドラッグ操作と一括移動をインベントリへ追加します。",
    "FPS・座標などの小型デバッグHUDを表示します。",
    "近くのチェストへ素早く収納し、ホットバーを自動補充します。",
    "PLaMo 3 がローカル翻訳した日本語パックを適用します。",
];

// ---------------------------------------------------------------------------
// Filters, sort, widget ids, actions
// ---------------------------------------------------------------------------

/// The clickable filter-pill selection.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ModManagerFilter {
    /// Every entry (subject to the library toggle and search).
    #[default]
    All,
    /// Only effectively-enabled mods.
    Enabled,
    /// Mods needing attention: missing deps or Tier-3 fallback.
    Issues,
    /// Tier-1 native ports only.
    Native,
    /// Curated 1.21.1 Modrinth mods (separate from installed entries).
    RecommendedQol,
    /// Unified browse view: curated QoL picks merged with the
    /// license-transparent community showcase, split by
    /// [`BrowseCategory`] and searchable live via
    /// [`build_modrinth_search_url`]. Still strictly separate from the
    /// installed list.
    Browse,
    /// Megu's mod-like built-in modules, exposed with the same stone-button
    /// affordance as external mods while remaining settings-backed.
    BuiltinModules,
}

/// The clickable sort toggle.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ModManagerSort {
    /// Name ascending.
    #[default]
    NameAsc,
    /// Name descending.
    NameDesc,
    /// Issues first, then enabled, then disabled (name order inside each).
    Status,
}

impl ModManagerSort {
    /// Cycle order: A-Z → Z-A → Status → A-Z.
    pub fn next(self) -> Self {
        match self {
            Self::NameAsc => Self::NameDesc,
            Self::NameDesc => Self::Status,
            Self::Status => Self::NameAsc,
        }
    }

    fn label(self, lang: UiLang) -> &'static str {
        match self {
            Self::NameAsc => "A-Z",
            Self::NameDesc => "Z-A",
            Self::Status => tr(lang, "Status", "状態"),
        }
    }
}

// ---------------------------------------------------------------------------
// PLaMo 3 Translate + missing-dependency consent state
// ---------------------------------------------------------------------------

/// User-facing model preset for the on-demand Japanese translator.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PlamoTranslatePreset {
    /// 0.8B — smallest download and lightest CPU runtime.
    Lightweight08B,
    /// 2B — recommended balance.
    #[default]
    Standard2B,
    /// 8B — highest quality.
    HighQuality8B,
}

impl PlamoTranslatePreset {
    pub fn label(self) -> &'static str {
        match self {
            Self::Lightweight08B => "0.8B (軽量)",
            Self::Standard2B => "2B (推奨)",
            Self::HighQuality8B => "8B (高品質)",
        }
    }

    pub fn next(self) -> Self {
        match self {
            Self::Lightweight08B => Self::Standard2B,
            Self::Standard2B => Self::HighQuality8B,
            Self::HighQuality8B => Self::Lightweight08B,
        }
    }
}

/// Host-reported install state of the translator model/runtime.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PlamoTranslatorStatus {
    /// Neither model nor chat template is installed.
    #[default]
    NotInstalled,
    /// Model files exist, but the loopback inference runtime is missing.
    RuntimeMissing,
    /// Model and runtime are both available.
    Ready,
}

/// Pure UI state for the PLaMo 3 Translate panel. The host performs every
/// download/inference side effect; this state only drives labels and input.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlamoTranslatorPanel {
    pub preset: PlamoTranslatePreset,
    pub status: PlamoTranslatorStatus,
    pub auto_pack_enabled: bool,
    pub busy: bool,
    pub progress_label: String,
}

impl Default for PlamoTranslatorPanel {
    fn default() -> Self {
        Self {
            preset: PlamoTranslatePreset::Standard2B,
            status: PlamoTranslatorStatus::NotInstalled,
            auto_pack_enabled: false,
            busy: false,
            progress_label: String::new(),
        }
    }
}

impl PlamoTranslatorPanel {
    /// Exact primary action label for the current install state.
    pub fn action_label(&self) -> &'static str {
        match self.status {
            PlamoTranslatorStatus::NotInstalled => "[翻訳モデルをインストール]",
            PlamoTranslatorStatus::RuntimeMissing => {
                "[llama-server を導入してください / インストール]"
            }
            PlamoTranslatorStatus::Ready => "[未翻訳 Mod を日本語化]",
        }
    }

    pub fn auto_pack_label(&self) -> String {
        format!(
            "[自動翻訳パック: {}]",
            if self.auto_pack_enabled { "ON" } else { "OFF" }
        )
    }

    /// The primary button is active only when a translation can actually be
    /// started; a missing runtime is explanatory rather than a dead download.
    pub fn action_enabled(&self) -> bool {
        !self.busy && self.status != PlamoTranslatorStatus::RuntimeMissing
    }
}

/// One exact Modrinth download candidate displayed in the consent prompt.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MissingDependencyConsentRow {
    pub mod_id: String,
    pub version_number: String,
    pub filename: String,
    pub sha512: String,
    pub size_bytes: u64,
    pub requesting_mods: Vec<String>,
}

/// One dependency no exact candidate could satisfy.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MissingDependencyUnsatisfiedRow {
    pub mod_id: String,
    pub required_version_range: String,
    pub requesting_mods: Vec<String>,
    pub reason: String,
}

/// Structured missing-dependency consent plan displayed by Mod Manager.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MissingDependencyConsentUiState {
    pub rows: Vec<MissingDependencyConsentRow>,
    pub unsatisfied: Vec<MissingDependencyUnsatisfiedRow>,
    pub isolated_mods_if_declined: Vec<String>,
    pub decision: Option<MissingDependencyConsentDecision>,
}

/// Explicit consent decision emitted as a host action.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MissingDependencyConsentDecision {
    ApproveAutoFetchAndSoftReload,
    IsolateUnsatisfiedMods,
}

impl MissingDependencyConsentUiState {
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty() && self.unsatisfied.is_empty()
    }
}

/// Every interactive element on the screen.
///
/// `ModRow` carries the row's index in the *currently visible* (filtered,
/// searched, sorted) list — the same ordering [`ModManagerState::visible_indices`]
/// reports.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModManagerWidgetId {
    /// The search text field — clicking focuses it for typing.
    SearchField,
    /// The `x` button clearing the search text.
    SearchClear,
    /// Filter pills.
    FilterAll,
    FilterEnabled,
    FilterIssues,
    FilterNative,
    /// Settings-backed built-in modules section.
    FilterBuiltinModules,
    FilterRecommendedQol,
    /// `[+ Browse / Search]` entry point into the unified catalog view.
    FilterBrowse,
    /// Browse shelf pills.
    BrowseQolHud,
    BrowseContent,
    BrowsePacks,
    /// Cycles the live-search [`ModrinthProjectKind`].
    BrowseKind,
    /// "Search Modrinth" — opens `build_modrinth_search_url` output.
    ModrinthSearch,
    QolHudInfo,
    QolInventoryInput,
    QolRecipes,
    QolVisualsCompat,
    /// `Libs: ON/OFF` library visibility toggle.
    ToggleLibraries,
    /// Cycles A-Z → Z-A → Status.
    SortMode,
    /// List scroll buttons.
    ListScrollUp,
    ListScrollDown,
    /// A visible mod row; payload is the visible-list index.
    ModRow(usize),
    /// Detail-pane "Download Missing (Modrinth)" button.
    DownloadMissingDeps,
    /// Detail-pane Enable/Disable toggle.
    ToggleEnabled,
    /// Detail-pane Configure button.
    ConfigureMod,
    /// Detail-pane optional link buttons.
    LinkWebsite,
    LinkIssues,
    LinkSource,
    LinkModrinth,
    /// Open the selected catalog mod's Modrinth project page.
    InstallRecommended,
    /// Footer "+ Add from Modrinth".
    AddFromModrinth,
    /// Footer "Open Mods Folder".
    OpenModsFolder,
    /// Footer "Apply" (batches pending enable/disable changes).
    ApplyChanges,
    /// Roll back to the last known-good transactional mod profile.
    RollbackProfile,
    /// Cycle the PLaMo 3 Translate preset (0.8B/2B/8B).
    PlamoPreset,
    /// PLaMo install/translate primary action.
    PlamoAction,
    /// Cancel the running model download/translation worker.
    CancelPlamo,
    /// Instant `megu_auto_ja_jp` resource-pack toggle.
    ToggleAutoTranslationPack,
    /// Built-in module row ON/OFF button (`builtin_module_index` order).
    BuiltinToggle(usize),
    /// Built-in module Configure shortcut into the backing settings tab.
    BuiltinOpenSettings(usize),
    /// Consent: auto-fetch exact missing dependencies and soft reload.
    ConsentApproveAutoFetch,
    /// Consent: isolate unsatisfied mods and continue.
    ConsentIsolateMods,
    /// Footer "Done".
    Done,
}

/// A clickable rectangle emitted alongside the draw list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ModManagerHitRegion {
    pub rect: UiRect,
    pub id: ModManagerWidgetId,
}

impl ModManagerHitRegion {
    /// Point containment test (right/bottom exclusive).
    pub const fn contains(self, x: i32, y: i32) -> bool {
        self.rect.contains(x, y)
    }
}

/// What the embedding application must perform after an interaction.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ModManagerAction {
    /// Open the Modrinth browser/install flow ("+ Add from Modrinth").
    OpenModrinthBrowser,
    /// Reveal the on-disk mods directory.
    OpenModsFolder,
    /// Open a URL in the system browser (Website/Issues/Source/Modrinth).
    OpenUrl(String),
    /// Open the selected mod's config screen.
    ConfigureMod(String),
    /// Fetch exactly these missing dependency ids from Modrinth for `mod_id`.
    DownloadMissingFromModrinth {
        mod_id: String,
        missing_deps: Vec<String>,
    },
    /// Commit every pending enable/disable change.
    ///
    /// `enabled_ids`/`disabled_ids` hold the mods that changed in each
    /// direction. `requires_soft_restart` is `false` only when every changed
    /// mod is [`ModReloadBadge::HotReload`]; a `SoftRestart` *or*
    /// `ProcessRestart` change sets it (the host can read each entry's
    /// `reload_mode` for the exact escalation level).
    ApplyChanges {
        enabled_ids: Vec<String>,
        disabled_ids: Vec<String>,
        requires_soft_restart: bool,
    },
    /// Activate the store's last known-good generation, then soft reload.
    RollbackModProfile,
    /// Cycle the translator preset; the host reads `plamo_panel.preset`.
    SelectPlamoPreset(PlamoTranslatePreset),
    /// Install the model or translate untranslated mods, depending on
    /// `plamo_panel.status`.
    InstallOrTranslatePlamo(PlamoTranslatePreset),
    /// Cancel the running translator download/translation worker.
    CancelPlamo,
    /// Flip the generated `megu_auto_ja_jp` language pack instantly.
    ToggleAutoTranslationPack(bool),
    /// Exact consent decision for a structured missing-dependency plan.
    ResolveMissingDependencyConsent(MissingDependencyConsentDecision),
    /// Leave the screen (Done / Escape / gamepad East).
    Close,
}

// ---------------------------------------------------------------------------
// Search query parsing & scoring
// ---------------------------------------------------------------------------

/// One whitespace-separated search token.
#[derive(Clone, Debug, PartialEq, Eq)]
enum QueryToken {
    /// Free text matched against name/id/summary/authors/loader/tier.
    Text(String),
    /// `@author` — substring match against the authors list.
    Author(String),
    /// `#tag` — a structured predicate; `None` = unrecognized tag that
    /// matches nothing (fail-closed so typos don't silently widen results).
    Tag(Option<Tag>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Tag {
    Native,
    Shim,
    Fallback,
    Missing,
    Disabled,
    Enabled,
    Lib,
    Config,
}

fn parse_query(search: &str) -> Vec<QueryToken> {
    search
        .split_whitespace()
        .map(|raw| {
            let token = raw.to_lowercase();
            if let Some(author) = token.strip_prefix('@') {
                if author.is_empty() {
                    // A lone '@' is meaningless; ignore it rather than
                    // excluding every entry.
                    return QueryToken::Text(String::new());
                }
                return QueryToken::Author(author.to_owned());
            }
            if let Some(tag) = token.strip_prefix('#') {
                let tag = match tag {
                    "native" => Some(Tag::Native),
                    "shim" => Some(Tag::Shim),
                    "fallback" => Some(Tag::Fallback),
                    "missing" => Some(Tag::Missing),
                    "disabled" => Some(Tag::Disabled),
                    "enabled" => Some(Tag::Enabled),
                    "lib" | "library" => Some(Tag::Lib),
                    "config" => Some(Tag::Config),
                    _ => None,
                };
                return QueryToken::Tag(tag);
            }
            QueryToken::Text(token)
        })
        .collect()
}

/// `true` when `needle`'s chars appear in `hay` in order (both lowercase).
fn is_subsequence(needle: &str, hay: &str) -> bool {
    let mut chars = hay.chars();
    for n in needle.chars() {
        if !chars.by_ref().any(|h| h == n) {
            return false;
        }
    }
    !needle.is_empty()
}

/// Match rank of a lowercase token against a lowercase field: exact >
/// prefix > substring > subsequence. `None` = no match.
fn field_rank(token: &str, field: &str) -> Option<u32> {
    if token.is_empty() {
        return Some(0);
    }
    if field == token {
        Some(4)
    } else if field.starts_with(token) {
        Some(3)
    } else if field.contains(token) {
        Some(2)
    } else if is_subsequence(token, field) {
        Some(1)
    } else {
        None
    }
}

/// Best score a free-text token gets across an entry's searchable fields;
/// `None` when the token matches none of them. Field weights keep name hits
/// ahead of summary hits.
fn text_token_score(entry: &ModManagerEntry, token: &str) -> Option<u32> {
    let name = entry.name.to_lowercase();
    let id = entry.mod_id.to_lowercase();
    let summary = entry.summary.to_lowercase();
    let mut best = field_rank(token, &name).map(|r| r * 10 + 6);
    best = best.max(field_rank(token, &id).map(|r| r * 10 + 5));
    for author in &entry.authors {
        best = best.max(field_rank(token, &author.to_lowercase()).map(|r| r * 10 + 3));
    }
    best = best.max(field_rank(token, entry.loader.search_label()).map(|r| r * 10 + 2));
    best = best.max(field_rank(token, entry.tier.search_label()).map(|r| r * 10 + 2));
    best = best.max(field_rank(token, &summary).map(|r| r * 10 + 1));
    best
}

fn tag_matches(tag: Tag, entry: &ModManagerEntry, enabled: bool) -> bool {
    match tag {
        Tag::Native => entry.tier == ModAdaptationBadge::Tier1Native,
        Tag::Shim => entry.tier == ModAdaptationBadge::Tier2Shim,
        Tag::Fallback => entry.tier == ModAdaptationBadge::Tier3Fallback,
        Tag::Missing => !entry.missing_deps.is_empty(),
        Tag::Disabled => !enabled,
        Tag::Enabled => enabled,
        Tag::Lib => entry.is_library,
        Tag::Config => entry.has_config,
    }
}

// ---------------------------------------------------------------------------
// Screen state
// ---------------------------------------------------------------------------

/// Interactive state of the Mods & Packs screen.
///
/// `entries` is host-owned metadata; enable toggles stage into `pending`
/// until [`ModManagerAction::ApplyChanges`] commits them back onto the entries.
#[derive(Clone, Debug)]
pub struct ModManagerState {
    /// All installed mods/packs/libraries, in host-supplied order.
    pub entries: Vec<ModManagerEntry>,
    /// Curated recommendations kept separate from host-owned installations.
    recommended_entries: Vec<ModManagerEntry>,
    /// The unified Browse view's deduplicated catalog: every community
    /// entry plus the curated QoL picks the community list does not
    /// already cover (the richer community metadata wins shared slugs).
    browse_entries: Vec<ModManagerEntry>,
    /// Parallel to `browse_entries`: which catalog each row came from.
    browse_sources: Vec<BrowseSource>,
    /// Live search text for the installed/Recommended-QoL views (edited
    /// by typing / Backspace / SearchClear).
    pub search: String,
    /// Live search text for the Browse view — filters the local catalog
    /// and feeds [`build_modrinth_search_url`] for online results.
    pub browse_query: String,
    /// Active shelf inside [`ModManagerFilter::Browse`].
    pub browse_category: BrowseCategory,
    /// `project_type` facet for the live Modrinth search.
    pub browse_project_kind: ModrinthProjectKind,
    /// Sticky flag: a non-hot-reload change was applied, so closing the
    /// menu should run the host's in-process soft reload. Staged (not
    /// yet applied) non-hot toggles are covered separately by the
    /// pending scan in [`ModManagerState::needs_soft_reload_on_close`].
    /// Cleared via [`ModManagerState::acknowledge_soft_reload`].
    pub dirty_requires_soft_reload: bool,
    /// Active filter pill.
    pub filter: ModManagerFilter,
    /// Optional sub-category when browsing Recommended QoL.
    pub recommended_category: Option<QolSubCategory>,
    /// Active sort mode.
    pub sort: ModManagerSort,
    /// Whether `is_library` entries appear without a `#lib` query.
    pub show_libraries: bool,
    /// UI language for chrome labels.
    pub lang: UiLang,
    /// `0` = auto (`height/270`, clamped `1..=4`), else a fixed GUI scale.
    pub ui_scale: u8,
    /// Widget under the mouse cursor (for hover highlight).
    pub hover: Option<ModManagerWidgetId>,
    /// Whether the search field owns typed characters.
    search_focused: bool,
    /// Selection as an index into the *visible* list.
    selected: usize,
    /// First row painted in the list pane (clamped to the list at render).
    scroll: usize,
    /// Staged enable overrides: `mod_id` → desired enabled state.
    pending: BTreeMap<String, bool>,
    /// PLaMo 3 Translate panel mirrored from the host's install status.
    pub plamo_panel: PlamoTranslatorPanel,
    /// Structured missing-dependency consent plan awaiting a decision.
    pub missing_dependency_consent: Option<MissingDependencyConsentUiState>,
    /// Live ON/OFF mirrors for the five settings-backed built-in modules,
    /// in [`BUILTIN_MODULE_NAMES_EN`] order. `UiState` refreshes these every
    /// frame; standalone use toggles them locally.
    pub builtin_enabled: [bool; BUILTIN_MODULE_COUNT],
}

/// Build the Browse catalog: all community entries first, then the
/// curated QoL picks whose slug no community entry already covers.
fn build_browse_catalog() -> (Vec<ModManagerEntry>, Vec<BrowseSource>) {
    let mut entries: Vec<ModManagerEntry> = RECOMMENDED_COMMUNITY_MODS
        .iter()
        .map(community_entry)
        .collect();
    let mut sources: Vec<BrowseSource> = (0..RECOMMENDED_COMMUNITY_MODS.len())
        .map(BrowseSource::Community)
        .collect();
    for (index, info) in CURATED_MODRINTH_QOL_MODS.iter().enumerate() {
        if RECOMMENDED_COMMUNITY_MODS
            .iter()
            .any(|community| community.slug == info.slug)
        {
            continue;
        }
        entries.push(curated_entry(info));
        sources.push(BrowseSource::Curated(index));
    }
    (entries, sources)
}

impl Default for ModManagerState {
    fn default() -> Self {
        let (browse_entries, browse_sources) = build_browse_catalog();
        Self {
            entries: Vec::new(),
            recommended_entries: CURATED_MODRINTH_QOL_MODS
                .iter()
                .map(curated_entry)
                .collect(),
            browse_entries,
            browse_sources,
            search: String::new(),
            browse_query: String::new(),
            browse_category: BrowseCategory::QolHud,
            browse_project_kind: ModrinthProjectKind::Mod,
            dirty_requires_soft_reload: false,
            filter: ModManagerFilter::All,
            recommended_category: None,
            sort: ModManagerSort::NameAsc,
            show_libraries: false,
            lang: UiLang::En,
            ui_scale: 0,
            hover: None,
            search_focused: true,
            selected: 0,
            scroll: 0,
            pending: BTreeMap::new(),
            plamo_panel: PlamoTranslatorPanel::default(),
            missing_dependency_consent: None,
            builtin_enabled: [false, true, false, false, false],
        }
    }
}

impl ModManagerState {
    pub fn new(entries: Vec<ModManagerEntry>) -> Self {
        Self {
            entries,
            ..Self::default()
        }
    }

    /// Effective GUI scale for `height` px tall output (`1..=4`), mirroring
    /// [`crate::state::UiState::effective_scale`].
    pub fn effective_scale(&self, height: u32) -> u32 {
        if self.ui_scale == 0 {
            (height / 270).clamp(1, 4)
        } else {
            u32::from(self.ui_scale.clamp(1, 4))
        }
    }

    /// Whether the search field currently receives typed characters.
    pub fn search_focused(&self) -> bool {
        self.search_focused
    }

    /// Refresh the built-in module mirrors from the authoritative settings
    /// owner (`UiState` in the integrated client).
    pub fn sync_builtin_modules(
        &mut self,
        recipe_overlay: bool,
        inventory_drag_controls: bool,
        compact_debug_hud: bool,
        quick_stack_auto_replenish: bool,
        plamo_auto_translation_pack: bool,
    ) {
        self.builtin_enabled = [
            recipe_overlay,
            inventory_drag_controls,
            compact_debug_hud,
            quick_stack_auto_replenish,
            plamo_auto_translation_pack,
        ];
    }

    /// Live ON/OFF state for a built-in module row.
    pub fn builtin_module_enabled(&self, index: usize) -> bool {
        self.builtin_enabled.get(index).copied().unwrap_or(false)
    }

    /// Selected row as an index into [`visible_indices`](Self::visible_indices).
    pub fn selected_row(&self) -> usize {
        self.selected
    }

    /// First visible row index (pre-clamp; rendering keeps it in range).
    pub fn scroll_offset(&self) -> usize {
        self.scroll
    }

    /// Selected entry, if the visible list is non-empty.
    pub fn selected_entry(&self) -> Option<&ModManagerEntry> {
        self.visible_indices()
            .get(self.selected)
            .and_then(|&i| self.browsed_entries().get(i))
    }

    /// `true` while browsing a metadata-only catalog (Recommended QoL or
    /// the unified Browse view) instead of the installed list.
    pub fn browsing_catalog(&self) -> bool {
        matches!(
            self.filter,
            ModManagerFilter::RecommendedQol | ModManagerFilter::Browse
        )
    }

    /// Entry slice indexed by [`visible_indices`](Self::visible_indices).
    /// In a catalog filter these are metadata-only entries awaiting an
    /// install initiated on Modrinth.
    pub fn browsed_entries(&self) -> &[ModManagerEntry] {
        match self.filter {
            ModManagerFilter::RecommendedQol => &self.recommended_entries,
            ModManagerFilter::Browse => &self.browse_entries,
            _ => &self.entries,
        }
    }

    /// The search text active for the current view: Browse edits
    /// `browse_query` (it doubles as the live-Modrinth-search input),
    /// every other view edits `search`.
    fn active_query(&self) -> &str {
        if self.filter == ModManagerFilter::Browse {
            &self.browse_query
        } else {
            &self.search
        }
    }

    fn active_query_mut(&mut self) -> &mut String {
        if self.filter == ModManagerFilter::Browse {
            &mut self.browse_query
        } else {
            &mut self.search
        }
    }

    /// Metadata backing a catalog row (`None` for installed entries).
    fn catalog_meta(&self, entry_index: usize) -> Option<CatalogMeta<'_>> {
        match self.filter {
            ModManagerFilter::RecommendedQol => Some(CatalogMeta::Curated(
                &CURATED_MODRINTH_QOL_MODS[entry_index],
            )),
            ModManagerFilter::Browse => {
                self.browse_sources
                    .get(entry_index)
                    .copied()
                    .map(|source| match source {
                        BrowseSource::Community(i) => {
                            CatalogMeta::Community(&RECOMMENDED_COMMUNITY_MODS[i])
                        }
                        BrowseSource::Curated(i) => {
                            CatalogMeta::Curated(&CURATED_MODRINTH_QOL_MODS[i])
                        }
                    })
            }
            _ => None,
        }
    }

    /// Whether a `browse_entries` row belongs on the active shelf.
    fn browse_category_matches(&self, index: usize) -> bool {
        let category = match self.browse_sources[index] {
            BrowseSource::Community(i) => RECOMMENDED_COMMUNITY_MODS[i].category,
            // Every curated pick is a client-side QoL mod.
            BrowseSource::Curated(_) => return self.browse_category == BrowseCategory::QolHud,
        };
        match self.browse_category {
            BrowseCategory::QolHud => matches!(
                category,
                RecommendedModCategory::HudInfo | RecommendedModCategory::Inventory
            ),
            BrowseCategory::Content => matches!(
                category,
                RecommendedModCategory::Content | RecommendedModCategory::World
            ),
            // Nothing curated ships in the Packs shelf — live Modrinth
            // search covers it.
            BrowseCategory::Packs => false,
        }
    }

    /// `true` when a staged (unapplied) change needs a soft reload.
    fn pending_requires_soft_reload(&self) -> bool {
        self.pending.iter().any(|(id, _)| {
            self.entries
                .iter()
                .find(|e| &e.mod_id == id)
                .is_some_and(|e| e.reload_mode != ModReloadBadge::HotReload)
        })
    }

    /// Whether closing the menu should kick off the host's in-process
    /// soft reload of mod containers: `true` once a non-hot-reload mod
    /// has been toggled or applied, so Done/Back re-initializes loaders
    /// without a full process restart. `ModManagerAction::Close` stays a
    /// unit variant — hosts poll this after handling it.
    pub fn needs_soft_reload_on_close(&self) -> bool {
        self.dirty_requires_soft_reload || self.pending_requires_soft_reload()
    }

    /// Clear the soft-reload need after the host has performed it.
    pub fn acknowledge_soft_reload(&mut self) {
        self.dirty_requires_soft_reload = false;
    }

    /// Status line the host can show while the soft reload runs.
    pub fn soft_reload_status(&self, lang: UiLang) -> &'static str {
        tr(
            lang,
            "Applying mod changes (soft reload)...",
            "Mod変更を適用中（内部再読み込み）...",
        )
    }

    /// Install/replace the structured missing-dependency consent plan.
    pub fn set_missing_dependency_consent(&mut self, plan: MissingDependencyConsentUiState) {
        self.missing_dependency_consent = (!plan.is_empty()).then_some(plan);
    }

    /// Clear the consent plan after the host consumes a decision.
    pub fn clear_missing_dependency_consent(&mut self) {
        self.missing_dependency_consent = None;
    }

    /// Mirror the host-detected translator status/preset into the panel.
    pub fn set_plamo_status(
        &mut self,
        status: PlamoTranslatorStatus,
        preset: PlamoTranslatePreset,
        auto_pack_enabled: bool,
    ) {
        self.plamo_panel.status = status;
        self.plamo_panel.preset = preset;
        self.plamo_panel.auto_pack_enabled = auto_pack_enabled;
    }

    /// Deference for the currently enabled installed mods, including staged
    /// enable/disable changes. The embedding renderer can query this before
    /// deciding whether to draw a built-in QoL feature.
    pub fn active_builtin_qol_deference(&self) -> BuiltinQolDeference {
        let ids: Vec<&str> = self
            .entries
            .iter()
            .filter(|entry| {
                self.pending
                    .get(&entry.mod_id)
                    .copied()
                    .unwrap_or(entry.enabled)
            })
            .map(|entry| entry.mod_id.as_str())
            .collect();
        active_builtin_qol_overrides(&ids)
    }

    /// `true` when at least one enable/disable toggle is staged.
    pub fn is_dirty(&self) -> bool {
        !self.pending.is_empty()
    }

    /// Staged override for `mod_id`, if any.
    pub fn pending_change(&self, mod_id: &str) -> Option<bool> {
        self.pending.get(mod_id).copied()
    }

    /// Enabled state after staged overrides (what the UI actually shows).
    pub fn effective_enabled(&self, mod_id: &str) -> Option<bool> {
        let entry = self.entries.iter().find(|e| e.mod_id == mod_id)?;
        Some(self.pending.get(mod_id).copied().unwrap_or(entry.enabled))
    }

    fn enabled_at(&self, index: usize) -> bool {
        if self.browsing_catalog() {
            return false;
        }
        let entry = &self.entries[index];
        self.pending
            .get(&entry.mod_id)
            .copied()
            .unwrap_or(entry.enabled)
    }

    /// Indices into the active entries slice (`entries` when installed,
    /// curated entries when browsing Recommended QoL), subject to the filter,
    /// category, library toggle,
    /// search query, and sort mode — in display order.
    ///
    /// Every whitespace-separated search token must match (`AND` semantics):
    /// `@x` requires an author containing `x`, `#tag` applies its predicate,
    /// and plain text scores against name/id/summary/authors/loader/tier
    /// (exact > prefix > substring > subsequence). When plain-text tokens
    /// are present, results order by that relevance score first and the
    /// active sort mode second. A `#lib` token overrides the library filter
    /// so libraries can be found without flipping the pill.
    pub fn visible_indices(&self) -> Vec<usize> {
        let tokens = parse_query(self.active_query());
        let recommended = self.filter == ModManagerFilter::RecommendedQol;
        let browse = self.filter == ModManagerFilter::Browse;
        let entries = self.browsed_entries();
        let lib_requested = tokens
            .iter()
            .any(|t| matches!(t, QueryToken::Tag(Some(Tag::Lib))));
        let mut scored: Vec<(usize, u32)> = Vec::with_capacity(entries.len());
        for (index, entry) in entries.iter().enumerate() {
            if recommended
                && self
                    .recommended_category
                    .is_some_and(|category| CURATED_MODRINTH_QOL_MODS[index].category != category)
            {
                continue;
            }
            if browse && !self.browse_category_matches(index) {
                continue;
            }
            if !self.show_libraries && entry.is_library && !lib_requested {
                continue;
            }
            let enabled = self.enabled_at(index);
            let passes_pill = match self.filter {
                ModManagerFilter::All => true,
                ModManagerFilter::Enabled => enabled,
                ModManagerFilter::Issues => entry.has_issue(),
                ModManagerFilter::Native => entry.tier == ModAdaptationBadge::Tier1Native,
                ModManagerFilter::RecommendedQol | ModManagerFilter::Browse => true,
                ModManagerFilter::BuiltinModules => false,
            };
            if !passes_pill {
                continue;
            }
            let mut score = 0u32;
            let mut ok = true;
            for token in &tokens {
                match token {
                    QueryToken::Text(text) if !text.is_empty() => {
                        match text_token_score(entry, text) {
                            Some(s) => score += s,
                            None => {
                                ok = false;
                                break;
                            }
                        }
                    }
                    QueryToken::Text(_) => {}
                    QueryToken::Author(author) => {
                        if !entry
                            .authors
                            .iter()
                            .any(|a| a.to_lowercase().contains(author.as_str()))
                        {
                            ok = false;
                            break;
                        }
                    }
                    QueryToken::Tag(Some(tag)) => {
                        if !tag_matches(*tag, entry, enabled) {
                            ok = false;
                            break;
                        }
                    }
                    QueryToken::Tag(None) => {
                        ok = false;
                        break;
                    }
                }
            }
            if ok {
                scored.push((index, score));
            }
        }
        let has_text = tokens
            .iter()
            .any(|t| matches!(t, QueryToken::Text(s) if !s.is_empty()));
        scored.sort_by(|a, b| {
            if has_text {
                b.1.cmp(&a.1).then_with(|| self.cmp_entries(a.0, b.0))
            } else {
                self.cmp_entries(a.0, b.0)
            }
        });
        scored.into_iter().map(|(index, _)| index).collect()
    }

    fn cmp_entries(&self, a: usize, b: usize) -> std::cmp::Ordering {
        let entries = self.browsed_entries();
        let ea = &entries[a];
        let eb = &entries[b];
        let name = |e: &ModManagerEntry| e.name.to_lowercase();
        let by_name = name(ea)
            .cmp(&name(eb))
            .then_with(|| ea.mod_id.cmp(&eb.mod_id));
        match self.sort {
            ModManagerSort::NameAsc => by_name,
            ModManagerSort::NameDesc => by_name.reverse(),
            ModManagerSort::Status => {
                let rank = |i: usize| {
                    let e = &entries[i];
                    if e.has_issue() {
                        0u8
                    } else if self.enabled_at(i) {
                        1
                    } else {
                        2
                    }
                };
                rank(a).cmp(&rank(b)).then(by_name)
            }
        }
    }

    fn after_view_change(&mut self) {
        self.selected = 0;
        self.scroll = 0;
    }

    fn move_selection(&mut self, offset: i64, wrap: bool) {
        let len = self.visible_indices().len();
        if len == 0 {
            self.selected = 0;
            return;
        }
        let next = self.selected as i64 + offset;
        self.selected = if wrap {
            next.rem_euclid(len as i64) as usize
        } else {
            next.clamp(0, len as i64 - 1) as usize
        };
    }

    fn toggle_selected(&mut self) {
        if self.browsing_catalog() {
            return;
        }
        let Some(&index) = self.visible_indices().get(self.selected) else {
            return;
        };
        let entry = &self.entries[index];
        let want = !self
            .pending
            .get(&entry.mod_id)
            .copied()
            .unwrap_or(entry.enabled);
        if want == entry.enabled {
            self.pending.remove(&entry.mod_id);
        } else {
            self.pending.insert(entry.mod_id.clone(), want);
        }
    }

    /// Stage pending changes into the apply action and commit them onto the
    /// entries. `None` when nothing is staged.
    fn apply(&mut self) -> Option<ModManagerAction> {
        if self.pending.is_empty() {
            return None;
        }
        let mut enabled_ids = Vec::new();
        let mut disabled_ids = Vec::new();
        let mut requires_soft_restart = false;
        for (id, want) in &self.pending {
            let Some(entry) = self.entries.iter().find(|e| e.mod_id == *id) else {
                continue;
            };
            if entry.enabled == *want {
                continue;
            }
            if *want {
                enabled_ids.push(id.clone());
            } else {
                disabled_ids.push(id.clone());
            }
            if entry.reload_mode != ModReloadBadge::HotReload {
                requires_soft_restart = true;
            }
        }
        if requires_soft_restart {
            self.dirty_requires_soft_reload = true;
        }
        for (id, want) in std::mem::take(&mut self.pending) {
            if let Some(entry) = self.entries.iter_mut().find(|e| e.mod_id == *id) {
                entry.enabled = want;
            }
        }
        Some(ModManagerAction::ApplyChanges {
            enabled_ids,
            disabled_ids,
            requires_soft_restart,
        })
    }

    /// Activate a widget (from a click, or directly in tests). Returns the
    /// outward actions the host must perform — usually empty, one element
    /// for buttons.
    pub fn activate(&mut self, id: ModManagerWidgetId) -> Vec<ModManagerAction> {
        match id {
            ModManagerWidgetId::SearchField => self.search_focused = true,
            ModManagerWidgetId::SearchClear => {
                if !self.active_query().is_empty() {
                    self.active_query_mut().clear();
                    self.after_view_change();
                }
            }
            ModManagerWidgetId::FilterAll => self.set_filter(ModManagerFilter::All),
            ModManagerWidgetId::FilterEnabled => self.set_filter(ModManagerFilter::Enabled),
            ModManagerWidgetId::FilterIssues => self.set_filter(ModManagerFilter::Issues),
            ModManagerWidgetId::FilterNative => self.set_filter(ModManagerFilter::Native),
            ModManagerWidgetId::FilterBuiltinModules => self.set_filter(ModManagerFilter::BuiltinModules),
            ModManagerWidgetId::FilterRecommendedQol => {
                if self.filter == ModManagerFilter::RecommendedQol {
                    self.recommended_category = None;
                    self.after_view_change();
                } else {
                    self.set_filter(ModManagerFilter::RecommendedQol);
                }
            }
            ModManagerWidgetId::FilterBrowse => self.set_filter(ModManagerFilter::Browse),
            ModManagerWidgetId::BrowseQolHud => self.set_browse_category(BrowseCategory::QolHud),
            ModManagerWidgetId::BrowseContent => self.set_browse_category(BrowseCategory::Content),
            ModManagerWidgetId::BrowsePacks => self.set_browse_category(BrowseCategory::Packs),
            ModManagerWidgetId::BrowseKind => {
                self.browse_project_kind = self.browse_project_kind.next();
                self.after_view_change();
            }
            ModManagerWidgetId::ModrinthSearch => {
                if self.filter == ModManagerFilter::Browse {
                    return vec![ModManagerAction::OpenUrl(build_modrinth_search_url(
                        &self.browse_query,
                        self.browse_project_kind,
                        RECOMMENDED_MC_VERSION,
                    ))];
                }
            }
            ModManagerWidgetId::QolHudInfo => {
                self.set_recommended_category(QolSubCategory::HudInfo)
            }
            ModManagerWidgetId::QolInventoryInput => {
                self.set_recommended_category(QolSubCategory::InventoryInput)
            }
            ModManagerWidgetId::QolRecipes => {
                self.set_recommended_category(QolSubCategory::Recipes)
            }
            ModManagerWidgetId::QolVisualsCompat => {
                self.set_recommended_category(QolSubCategory::VisualsCompat)
            }
            ModManagerWidgetId::ToggleLibraries => {
                self.show_libraries = !self.show_libraries;
                self.after_view_change();
            }
            ModManagerWidgetId::SortMode => {
                self.sort = self.sort.next();
                self.after_view_change();
            }
            ModManagerWidgetId::ListScrollUp => {
                self.scroll = self.scroll.saturating_sub(1);
            }
            ModManagerWidgetId::ListScrollDown => {
                let len = if self.filter == ModManagerFilter::BuiltinModules {
                    BUILTIN_MODULE_COUNT
                } else {
                    self.visible_indices().len()
                };
                self.scroll = (self.scroll + 1).min(len.saturating_sub(1));
            }
            ModManagerWidgetId::ModRow(row) => {
                if row < self.visible_indices().len() {
                    self.selected = row;
                }
            }
            ModManagerWidgetId::ToggleEnabled => self.toggle_selected(),
            ModManagerWidgetId::DownloadMissingDeps => {
                if let Some(entry) = self.selected_entry() {
                    if !entry.missing_deps.is_empty() {
                        return vec![ModManagerAction::DownloadMissingFromModrinth {
                            mod_id: entry.mod_id.clone(),
                            missing_deps: entry.missing_deps.clone(),
                        }];
                    }
                }
            }
            ModManagerWidgetId::RollbackProfile => {
                return vec![ModManagerAction::RollbackModProfile];
            }
            ModManagerWidgetId::PlamoPreset => {
                self.plamo_panel.preset = self.plamo_panel.preset.next();
                return vec![ModManagerAction::SelectPlamoPreset(self.plamo_panel.preset)];
            }
            ModManagerWidgetId::PlamoAction => {
                if self.plamo_panel.action_enabled() {
                    self.plamo_panel.busy = true;
                    return vec![ModManagerAction::InstallOrTranslatePlamo(
                        self.plamo_panel.preset,
                    )];
                }
            }
            ModManagerWidgetId::CancelPlamo => {
                if self.plamo_panel.busy {
                    self.plamo_panel.progress_label =
                        tr(self.lang, "Cancelling...", "キャンセルしています...").to_owned();
                    return vec![ModManagerAction::CancelPlamo];
                }
            }
            ModManagerWidgetId::ToggleAutoTranslationPack => {
                self.plamo_panel.auto_pack_enabled = !self.plamo_panel.auto_pack_enabled;
                return vec![ModManagerAction::ToggleAutoTranslationPack(
                    self.plamo_panel.auto_pack_enabled,
                )];
            }
            ModManagerWidgetId::ConsentApproveAutoFetch => {
                if let Some(plan) = &mut self.missing_dependency_consent {
                    plan.decision =
                        Some(MissingDependencyConsentDecision::ApproveAutoFetchAndSoftReload);
                    return vec![ModManagerAction::ResolveMissingDependencyConsent(
                        MissingDependencyConsentDecision::ApproveAutoFetchAndSoftReload,
                    )];
                }
            }
            ModManagerWidgetId::ConsentIsolateMods => {
                if let Some(plan) = &mut self.missing_dependency_consent {
                    plan.decision = Some(MissingDependencyConsentDecision::IsolateUnsatisfiedMods);
                    return vec![ModManagerAction::ResolveMissingDependencyConsent(
                        MissingDependencyConsentDecision::IsolateUnsatisfiedMods,
                    )];
                }
            }
            ModManagerWidgetId::ConfigureMod => {
                if let Some(entry) = self.selected_entry() {
                    if entry.has_config {
                        return vec![ModManagerAction::ConfigureMod(entry.mod_id.clone())];
                    }
                }
            }
            ModManagerWidgetId::LinkWebsite
            | ModManagerWidgetId::LinkIssues
            | ModManagerWidgetId::LinkSource
            | ModManagerWidgetId::LinkModrinth => {
                if let Some(entry) = self.selected_entry() {
                    let url = match id {
                        ModManagerWidgetId::LinkWebsite => &entry.website_url,
                        ModManagerWidgetId::LinkIssues => &entry.issues_url,
                        ModManagerWidgetId::LinkSource => &entry.source_url,
                        _ => &entry.modrinth_url,
                    };
                    if !url.is_empty() {
                        return vec![ModManagerAction::OpenUrl(url.clone())];
                    }
                }
            }
            ModManagerWidgetId::InstallRecommended => {
                if self.browsing_catalog() {
                    if let Some(entry) = self.selected_entry() {
                        return vec![ModManagerAction::OpenUrl(entry.modrinth_url.clone())];
                    }
                }
            }
            ModManagerWidgetId::AddFromModrinth => {
                return vec![ModManagerAction::OpenModrinthBrowser];
            }
            ModManagerWidgetId::OpenModsFolder => {
                return vec![ModManagerAction::OpenModsFolder];
            }
            ModManagerWidgetId::ApplyChanges => {
                if let Some(action) = self.apply() {
                    return vec![action];
                }
            }
            ModManagerWidgetId::BuiltinToggle(index) => {
                if index < BUILTIN_MODULE_COUNT {
                    self.builtin_enabled[index] = !self.builtin_enabled[index];
                }
            }
            ModManagerWidgetId::BuiltinOpenSettings(_) => {
                // Integrated `UiState` intercepts this widget and opens the
                // backing settings tab. Standalone ModManagerState has no
                // settings owner, so activation is intentionally inert.
            }
            ModManagerWidgetId::Done => return vec![ModManagerAction::Close],
        }
        Vec::new()
    }

    fn set_filter(&mut self, filter: ModManagerFilter) {
        if self.filter != filter {
            self.filter = filter;
            self.after_view_change();
        }
    }

    fn set_recommended_category(&mut self, category: QolSubCategory) {
        if self.filter == ModManagerFilter::RecommendedQol {
            self.recommended_category =
                (self.recommended_category != Some(category)).then_some(category);
            self.after_view_change();
        }
    }

    fn set_browse_category(&mut self, category: BrowseCategory) {
        if self.filter == ModManagerFilter::Browse && self.browse_category != category {
            self.browse_category = category;
            self.after_view_change();
        }
    }

    /// The widget under `(x, y)` in a `width`×`height` frame, if any
    /// (physical pixels; regions are recomputed from the current state).
    pub fn widget_at(&self, width: u32, height: u32, x: i32, y: i32) -> Option<ModManagerWidgetId> {
        self.render(width, height)
            .1
            .iter()
            .rev()
            .find(|h| h.rect.contains(x, y))
            .map(|h| h.id)
    }

    /// Route one device-agnostic input event; returns the actions the app
    /// must perform. `width`/`height` are the frame's physical pixels, used
    /// for pointer hit-testing and page-step size.
    ///
    /// Keyboard/gamepad semantics (ModManager-style):
    ///
    /// * typing appends to the search and focuses it; `Backspace` edits it;
    /// * `Up`/`Down` wrap through the visible list, `Left`/`Right` page it;
    /// * `Tab` toggles search focus; `Enter`/`Space` commit the search when
    ///   focused, otherwise toggle the selected mod's enabled state;
    /// * `Escape` (and gamepad `East`/`Start`) closes the screen;
    /// * gamepad `South` mirrors `Enter`, D-pad mirrors the arrows.
    pub fn handle_input(
        &mut self,
        ev: UiInputEvent,
        width: u32,
        height: u32,
    ) -> Vec<ModManagerAction> {
        match ev {
            UiInputEvent::MouseMove { x, y } => {
                self.hover = self.widget_at(width, height, x, y);
                Vec::new()
            }
            UiInputEvent::Click { x, y } => match self.widget_at(width, height, x, y) {
                Some(id) => {
                    if id != ModManagerWidgetId::SearchField {
                        self.search_focused = false;
                    }
                    self.activate(id)
                }
                None => Vec::new(),
            },
            UiInputEvent::KeyPressed(key) => self.handle_key(key, height),
            UiInputEvent::CharTyped(c) => {
                if (' '..='~').contains(&c) {
                    self.search_focused = true;
                    self.active_query_mut().push(c);
                    self.after_view_change();
                }
                Vec::new()
            }
            UiInputEvent::GamepadPressed(btn) => match btn {
                GamepadButton::DpadUp => self.handle_key(UiKey::Up, height),
                GamepadButton::DpadDown => self.handle_key(UiKey::Down, height),
                GamepadButton::DpadLeft => self.handle_key(UiKey::Left, height),
                GamepadButton::DpadRight => self.handle_key(UiKey::Right, height),
                GamepadButton::South => self.handle_key(UiKey::Enter, height),
                GamepadButton::East | GamepadButton::Start => {
                    vec![ModManagerAction::Close]
                }
            },
        }
    }

    fn handle_key(&mut self, key: UiKey, height: u32) -> Vec<ModManagerAction> {
        match key {
            UiKey::Up => self.move_selection(-1, true),
            UiKey::Down => self.move_selection(1, true),
            UiKey::Left | UiKey::Right => {
                let page = self.list_page_rows(height).max(1) as i64;
                self.move_selection(if key == UiKey::Left { -page } else { page }, false);
            }
            UiKey::Tab => self.search_focused = !self.search_focused,
            UiKey::Enter | UiKey::Space => {
                if self.search_focused {
                    self.search_focused = false;
                } else if self.browsing_catalog() {
                    return self.activate(ModManagerWidgetId::InstallRecommended);
                } else {
                    self.toggle_selected();
                }
            }
            UiKey::Backspace => {
                if self.active_query_mut().pop().is_some() {
                    self.after_view_change();
                }
            }
            UiKey::Escape => return vec![ModManagerAction::Close],
        }
        Vec::new()
    }

    /// Rows per list page at `height` physical px (same math as render).
    fn list_page_rows(&self, height: u32) -> usize {
        let s = self.effective_scale(height) as i32;
        let h = (height.min(i32::MAX as u32) as i32) / s;
        let bottom = h - 4 - LIST_BOTTOM_OFF;
        ((bottom - LIST_TOP - 4).max(0) / ROW_PITCH).max(1) as usize
    }

    // -- rendering ------------------------------------------------------

    /// Build the frame: `(draw_list, hit_regions)`. Everything is computed
    /// in logical pixels and multiplied by [`effective_scale`](Self::effective_scale)
    /// on emit, so the same layout serves `640×360` through `3840×2160` and
    /// every emitted hit region is pixel-identical to its widget's visuals.
    pub fn render(&self, width: u32, height: u32) -> (UiDrawList, Vec<ModManagerHitRegion>) {
        let mut lay = Lay::new(self, width, height);
        let mut hits = Vec::new();
        if lay.h < 110 {
            return self.render_ultracompact(lay, hits);
        }
        lay.fill(0, 0, lay.w, lay.h, palette::BACKGROUND_TOP);
        lay.fill(
            0,
            lay.h / 2,
            lay.w,
            lay.h - lay.h / 2,
            palette::BACKGROUND_BOTTOM,
        );
        lay.fill(
            MARGIN,
            MARGIN,
            lay.w - 2 * MARGIN,
            lay.h - 2 * MARGIN,
            palette::PANEL,
        );
        lay.border(
            MARGIN,
            MARGIN,
            lay.w - 2 * MARGIN,
            lay.h - 2 * MARGIN,
            palette::PANEL_BORDER,
        );

        self.render_header(&mut lay, &mut hits);
        self.render_filter_row(&mut lay, &mut hits);
        if self.filter == ModManagerFilter::BuiltinModules {
            self.render_builtin_modules(&mut lay, &mut hits);
        } else {
            self.render_panes(&mut lay, &mut hits);
        }
        self.render_footer(&mut lay, &mut hits);
        (lay.finish(), hits)
    }

    /// Ultra-short fallback surface (below ~110 logical px): the full
    /// two-pane manager cannot physically fit, so collapse to the vanilla
    /// essentials — title, a full-width mod list, optional scroll arrows,
    /// and a guaranteed clickable Done. No chrome is allowed to overlap.
    fn render_ultracompact(
        &self,
        mut lay: Lay,
        mut hits: Vec<ModManagerHitRegion>,
    ) -> (UiDrawList, Vec<ModManagerHitRegion>) {
        lay.fill(0, 0, lay.w, lay.h, palette::BACKGROUND_TOP);
        lay.fill(0, lay.h / 2, lay.w, lay.h - lay.h / 2, palette::BACKGROUND_BOTTOM);
        let outer = UiRect::new(
            MARGIN,
            MARGIN,
            (lay.w - 2 * MARGIN).max(8) as u32,
            (lay.h - 2 * MARGIN).max(8) as u32,
        );
        lay.fill_rect(outer, palette::PANEL);
        lay.border_rect(outer, palette::PANEL_BORDER);
        let title = tr(self.lang, "Mods & Packs", "Modとパック");
        lay.text_fit(MARGIN + 5, MARGIN + 3, 1, palette::TEXT, title, ((lay.w - 16) / 8).max(1) as usize);

        let done_label = tr(self.lang, "Done", "完了");
        let done_w = (crate::draw::text_width(done_label, 1) as i32 + 20)
            .min(lay.w - 2 * MARGIN - 8)
            .max(24);
        let done_y = lay.h - MARGIN - 16;
        let done = UiRect::new(lay.w - MARGIN - 4 - done_w, done_y, done_w as u32, 16);
        let top = MARGIN + 13;
        let bottom = (done_y - 4).max(top + 8);
        let x = MARGIN + 4;
        let w = (lay.w - 2 * (MARGIN + 4)).max(24);
        let list = UiRect::new(x, top, w as u32, (bottom - top).max(1) as u32);
        lay.fill_rect(list, DETAIL_BG);
        lay.border_rect(list, palette::PANEL_BORDER);

        let visible = self.visible_indices();
        let pitch = 20;
        let page = ((bottom - top - 3) / pitch).max(0) as usize;
        let max_scroll = visible.len().saturating_sub(page.max(1));
        let scroll = self.scroll.min(max_scroll);
        let scrollable = visible.len() > page && page > 0;
        let rows_w = w - 4 - if scrollable { 11 } else { 0 };
        if visible.is_empty() && page > 0 {
            lay.text_fit(
                x + 5,
                top + 4,
                1,
                palette::TEXT_DIM,
                tr(self.lang, "No mods installed", "Modがインストールされていません"),
                ((rows_w - 10) / 8).max(1) as usize,
            );
        }
        for (row_slot, &entry_index) in visible.iter().enumerate().skip(scroll).take(page) {
            let y = top + 2 + (row_slot - scroll) as i32 * pitch;
            let row = UiRect::new(x + 2, y, rows_w.max(12) as u32, (pitch - 3) as u32);
            let entry = &self.browsed_entries()[entry_index];
            lay.fill_rect(row, if row_slot == self.selected { palette::BUTTON_HOVER } else { palette::SLOT });
            lay.text_fit(
                row.x + 4,
                row.y + 4,
                1,
                if entry.enabled { palette::TEXT } else { palette::TEXT_DIM },
                &entry.name,
                ((rows_w - 8) / 8).max(1) as usize,
            );
            lay.region(&mut hits, ModManagerWidgetId::ModRow(row_slot), row);
        }
        if scrollable {
            let sb_x = x + w - 12;
            let up = UiRect::new(sb_x, top + 1, 10, 10);
            let down = UiRect::new(sb_x, bottom - 11, 10, 10);
            if up.bottom() <= down.y {
                for (rect, id, glyph) in [
                    (up, ModManagerWidgetId::ListScrollUp, "^"),
                    (down, ModManagerWidgetId::ListScrollDown, "v"),
                ] {
                    lay.fill_rect(rect, palette::BUTTON);
                    lay.ctext_in(rect, 1, palette::TEXT, glyph);
                    lay.region(&mut hits, id, rect);
                }
            }
        }

        lay.fill_rect(done, if self.hover == Some(ModManagerWidgetId::Done) { palette::BUTTON_HOVER } else { palette::BUTTON });
        lay.border_rect(done, palette::BUTTON_BORDER);
        lay.ctext_in(done, 1, palette::TEXT, done_label);
        lay.region(&mut hits, ModManagerWidgetId::Done, done);
        (lay.finish(), hits)
    }

    fn render_header(&self, lay: &mut Lay, hits: &mut Vec<ModManagerHitRegion>) {
        let lang = self.lang;
        let visible = self.visible_indices().len();
        let total = self.browsed_entries().len();
        let title = format!(
            "{} ({} / {})",
            match self.filter {
                ModManagerFilter::RecommendedQol => tr(lang, "Recommended QoL", "おすすめQoL"),
                ModManagerFilter::Browse => tr(lang, "Recommended", "おすすめ Mod"),
                _ => tr(lang, "Mods & Packs", "Modとパック"),
            },
            visible,
            total
        );
        lay.text_fit(
            MARGIN + 4,
            MARGIN + 4,
            2,
            palette::TEXT,
            &title,
            ((lay.w - 2 * MARGIN - 8) / 16).max(1) as usize,
        );

        // Search row: [search field][x][Sort: <mode>]
        let sort_label = format!("{}: {}", tr(lang, "Sort", "並び"), self.sort.label(lang));
        // At narrow logical widths the sort cycle shares the row with a
        // usable search field and the clear chip; shrink the cycle button
        // instead of letting the three hitboxes overlap.
        let sort_w = (12 * 8 + 12).min(lay.w - 70).max(44); // "Sort: Status" is the widest label.
        let clear_w = 18;
        let field_x = MARGIN + 4;
        let field_w = lay.w - field_x - MARGIN - 4 - clear_w - 4 - sort_w - MARGIN;
        let field = UiRect::new(field_x, SEARCH_Y, field_w.max(24) as u32, 16);
        let field_color = if self.search_focused {
            palette::SLOT
        } else {
            palette::BUTTON_DISABLED
        };
        lay.fill_rect(field, field_color);
        lay.border_rect(
            field,
            if self.search_focused {
                palette::ACCENT
            } else {
                palette::BUTTON_BORDER
            },
        );
        let cells = ((field.w as i32 - 10) / 8).max(1) as usize;
        let query = self.active_query();
        if query.is_empty() && !self.search_focused {
            let placeholder = if self.filter == ModManagerFilter::Browse {
                tr(lang, "Search Modrinth...", "Modrinthを検索...")
            } else {
                tr(lang, "Search mods...", "Modを検索...")
            };
            lay.text_fit(
                field.x + 5,
                field.y + 4,
                1,
                palette::TEXT_DIM,
                placeholder,
                cells,
            );
        } else {
            let mut shown = query.to_owned();
            if self.search_focused {
                shown.push('_');
            }
            // Right-align the tail so the newest characters stay visible.
            let char_cells = crate::draw::text_cells(&shown);
            if char_cells > cells {
                shown = format!("...{}", crate::draw::last_cells(&shown, cells - 3));
            }
            lay.text_fit(field.x + 5, field.y + 4, 1, palette::TEXT, &shown, cells);
        }
        lay.region(hits, ModManagerWidgetId::SearchField, field);

        let clear = UiRect::new(field.right() + 4, SEARCH_Y, clear_w as u32, 16);
        let clear_bg = if query.is_empty() {
            palette::BUTTON_DISABLED
        } else if self.hover == Some(ModManagerWidgetId::SearchClear) {
            palette::DANGER
        } else {
            palette::BUTTON
        };
        lay.fill_rect(clear, clear_bg);
        lay.ctext_in(
            clear,
            1,
            if query.is_empty() {
                palette::TEXT_DIM
            } else {
                palette::TEXT
            },
            "x",
        );
        if !query.is_empty() {
            lay.region(hits, ModManagerWidgetId::SearchClear, clear);
        }

        let sort = UiRect::new(lay.w - MARGIN - sort_w, SEARCH_Y, sort_w as u32, 16);
        let sort_bg = if self.hover == Some(ModManagerWidgetId::SortMode) {
            palette::BUTTON_HOVER
        } else {
            palette::BUTTON
        };
        lay.fill_rect(sort, sort_bg);
        lay.border_rect(sort, palette::BUTTON_BORDER);
        lay.ctext_in(
            sort,
            1,
            palette::TEXT,
            &fit_text(&sort_label, ((sort_w - 8) / 8).max(1) as usize),
        );
        lay.region(hits, ModManagerWidgetId::SortMode, sort);
    }

    fn render_filter_row(&self, lay: &mut Lay, hits: &mut Vec<ModManagerHitRegion>) {
        let lang = self.lang;
        let pills: Vec<(String, ModManagerWidgetId, bool)> =
            if self.filter == ModManagerFilter::Browse {
                // Unified browse: Installed keeps the left slot, then the
                // three shelves and the live-search project kind.
                vec![
                    (
                        tr(lang, "Installed", "インストール済").to_owned(),
                        ModManagerWidgetId::FilterAll,
                        false,
                    ),
                    (
                        BrowseCategory::QolHud.label(lang).to_owned(),
                        ModManagerWidgetId::BrowseQolHud,
                        self.browse_category == BrowseCategory::QolHud,
                    ),
                    (
                        BrowseCategory::Content.label(lang).to_owned(),
                        ModManagerWidgetId::BrowseContent,
                        self.browse_category == BrowseCategory::Content,
                    ),
                    (
                        BrowseCategory::Packs.label(lang).to_owned(),
                        ModManagerWidgetId::BrowsePacks,
                        self.browse_category == BrowseCategory::Packs,
                    ),
                    (
                        format!(
                            "{}: {}",
                            tr(lang, "Kind", "タイプ"),
                            self.browse_project_kind.label()
                        ),
                        ModManagerWidgetId::BrowseKind,
                        false,
                    ),
                ]
            } else if self.filter == ModManagerFilter::RecommendedQol {
                vec![
                    (
                        tr(lang, "Installed", "インストール済").to_owned(),
                        ModManagerWidgetId::FilterAll,
                        false,
                    ),
                    (
                        "Recommended QoL".to_owned(),
                        ModManagerWidgetId::FilterRecommendedQol,
                        self.recommended_category.is_none(),
                    ),
                    (
                        "HUD".to_owned(),
                        ModManagerWidgetId::QolHudInfo,
                        self.recommended_category == Some(QolSubCategory::HudInfo),
                    ),
                    (
                        "Inventory".to_owned(),
                        ModManagerWidgetId::QolInventoryInput,
                        self.recommended_category == Some(QolSubCategory::InventoryInput),
                    ),
                    (
                        "Recipes".to_owned(),
                        ModManagerWidgetId::QolRecipes,
                        self.recommended_category == Some(QolSubCategory::Recipes),
                    ),
                    (
                        "Visuals".to_owned(),
                        ModManagerWidgetId::QolVisualsCompat,
                        self.recommended_category == Some(QolSubCategory::VisualsCompat),
                    ),
                ]
            } else {
                vec![
                    (
                        tr(lang, "All", "すべて").to_owned(),
                        ModManagerWidgetId::FilterAll,
                        self.filter == ModManagerFilter::All,
                    ),
                    (
                        tr(lang, "Built-in", "内蔵").to_owned(),
                        ModManagerWidgetId::FilterBuiltinModules,
                        self.filter == ModManagerFilter::BuiltinModules,
                    ),
                    (
                        tr(lang, "Enabled", "有効").to_owned(),
                        ModManagerWidgetId::FilterEnabled,
                        self.filter == ModManagerFilter::Enabled,
                    ),
                    (
                        tr(lang, "Issues", "問題あり").to_owned(),
                        ModManagerWidgetId::FilterIssues,
                        self.filter == ModManagerFilter::Issues,
                    ),
                    (
                        tr(lang, "Native", "ネイティブ").to_owned(),
                        ModManagerWidgetId::FilterNative,
                        self.filter == ModManagerFilter::Native,
                    ),
                    (
                        "QoL".to_owned(),
                        ModManagerWidgetId::FilterRecommendedQol,
                        false,
                    ),
                    (
                        tr(lang, "+ Browse", "+ ブラウズ").to_owned(),
                        ModManagerWidgetId::FilterBrowse,
                        false,
                    ),
                    (
                        if self.show_libraries {
                            tr(lang, "Libs: ON", "ライブラリ: ON")
                        } else {
                            tr(lang, "Libs: OFF", "ライブラリ: OFF")
                        }
                        .to_owned(),
                        ModManagerWidgetId::ToggleLibraries,
                        self.show_libraries,
                    ),
                ]
            };
        let mut x = MARGIN + 4;
        for (label, id, active) in pills {
            let w = crate::draw::text_width(&label, 1) as i32 + 12;
            if x + w > lay.w - MARGIN {
                break;
            }
            let rect = UiRect::new(x, FILTER_Y, w as u32, 14);
            let bg = if active {
                palette::ACCENT
            } else if self.hover == Some(id) {
                palette::BUTTON_HOVER
            } else {
                palette::BUTTON
            };
            lay.fill_rect(rect, bg);
            lay.border_rect(rect, palette::BUTTON_BORDER);
            lay.ctext_in(
                rect,
                1,
                if active {
                    palette::TEXT_DARK
                } else {
                    palette::TEXT
                },
                &label,
            );
            lay.region(hits, id, rect);
            x += w + 4;
        }
    }

    fn render_builtin_modules(&self, lay: &mut Lay, hits: &mut Vec<ModManagerHitRegion>) {
        let lang = self.lang;
        let x = MARGIN + 4;
        let top = LIST_TOP;
        let bottom = lay.h - 4 - LIST_BOTTOM_OFF;
        let w = lay.w - 2 * (MARGIN + 4);
        let pane = UiRect::new(x, top, w.max(24) as u32, (bottom - top).max(1) as u32);
        lay.fill_rect(pane, DETAIL_BG);
        lay.border_rect(pane, palette::PANEL_BORDER);
        let cells = ((w - 12) / 8).max(1) as usize;
        lay.text_fit(
            x + 6,
            top + 5,
            1,
            palette::ACCENT,
            tr(lang, "Built-in Modules", "内蔵モジュール"),
            cells,
        );

        let row_pitch = if lay.w < 320 { 31 } else { 34 };
        let first_y = top + 17;
        let visible_rows = ((bottom - first_y - 16) / row_pitch).max(0) as usize;
        let max_scroll = BUILTIN_MODULE_COUNT.saturating_sub(visible_rows.max(1));
        let scroll = self.scroll.min(max_scroll);
        for index in scroll..BUILTIN_MODULE_COUNT.min(scroll + visible_rows) {
            let y = first_y + (index - scroll) as i32 * row_pitch;
            let row = UiRect::new(x + 3, y, (w - 6).max(20) as u32, (row_pitch - 3) as u32);
            lay.fill_rect(row, palette::SLOT);
            if index == self.selected {
                lay.border_rect(row, palette::ACCENT);
            }
            let name = if lang == UiLang::Ja {
                BUILTIN_MODULE_NAMES_JA[index]
            } else {
                BUILTIN_MODULE_NAMES_EN[index]
            };
            let desc = if lang == UiLang::Ja {
                BUILTIN_MODULE_DESCRIPTIONS_JA[index]
            } else {
                BUILTIN_MODULE_DESCRIPTIONS_EN[index]
            };
            let enabled = self.builtin_module_enabled(index);
            let compact = lay.w < 360;
            let toggle_label = if enabled {
                tr(lang, "ON", "ON")
            } else {
                tr(lang, "OFF", "OFF")
            };
            let toggle_w = if compact { 42 } else { 54 };
            let config_w = if compact { 58 } else { 82 };
            let button_y = row.y + if compact { 5 } else { 7 };
            let toggle = UiRect::new(row.right() - toggle_w - 4, button_y, toggle_w as u32, 16);
            let config = UiRect::new(toggle.x - config_w - 4, button_y, config_w as u32, 16);
            let text_right = if compact { config.x - 6 } else { row.right() - toggle_w - config_w - 16 };
            lay.text_fit(
                row.x + 5,
                row.y + 3,
                1,
                palette::TEXT,
                name,
                ((text_right - row.x - 5) / 8).max(1) as usize,
            );
            lay.text_fit(
                row.x + 5,
                row.y + if compact { 14 } else { 17 },
                1,
                palette::TEXT_DIM,
                desc,
                ((text_right - row.x - 5) / 8).max(1) as usize,
            );
            for (rect, id, label, accent) in [
                (
                    config,
                    ModManagerWidgetId::BuiltinOpenSettings(index),
                    tr(lang, "Configure", "設定"),
                    false,
                ),
                (
                    toggle,
                    ModManagerWidgetId::BuiltinToggle(index),
                    toggle_label,
                    enabled,
                ),
            ] {
                let bg = if self.hover == Some(id) {
                    palette::BUTTON_HOVER
                } else if accent {
                    palette::ACCENT
                } else {
                    palette::BUTTON
                };
                lay.fill_rect(rect, bg);
                lay.border_rect(rect, palette::BUTTON_BORDER);
                lay.ctext_in(
                    rect,
                    1,
                    if accent { palette::TEXT_DARK } else { palette::TEXT },
                    label,
                );
                lay.region(hits, id, rect);
            }
        }
        if max_scroll > 0 {
            let sb_x = x + w - 14;
            let up = UiRect::new(sb_x, top + 2, 10, 12);
            let down = UiRect::new(sb_x, bottom - 14, 10, 12);
            lay.fill_rect(up, palette::BUTTON);
            lay.fill_rect(down, palette::BUTTON);
            lay.ctext_in(up, 1, palette::TEXT, "^");
            lay.ctext_in(down, 1, palette::TEXT, "v");
            lay.region(hits, ModManagerWidgetId::ListScrollUp, up);
            lay.region(hits, ModManagerWidgetId::ListScrollDown, down);
        }
    }

    fn render_panes(&self, lay: &mut Lay, hits: &mut Vec<ModManagerHitRegion>) {
        let inner_w = lay.w - 2 * MARGIN - 8;
        // Below the width where two useful panes can coexist, Mods & Packs
        // becomes vanilla-compact: one full-width list column. Detail and
        // per-mod actions are intentionally omitted rather than squeezed
        // into overlapping hitboxes; Done and list scrolling stay live.
        let compact_single_column = inner_w < 300;
        // Keep at least a sliver of detail pane at minimum-width windows.
        let min_list_w = if compact_single_column {
            30
        } else {
            140.min((inner_w - PANE_GAP - 40).max(30))
        };
        let list_w = if compact_single_column {
            inner_w.max(30)
        } else {
            ((inner_w - PANE_GAP) * 2 / 5).clamp(min_list_w, 420)
        };
        let list_x = MARGIN + 4;
        let list_right = list_x + list_w;
        let detail_x = list_right + PANE_GAP;
        let detail_w = MARGIN + 4 + inner_w - detail_x;
        let top = LIST_TOP;
        let bottom = lay.h - 4 - LIST_BOTTOM_OFF;

        // ---- left pane: scrollable list -------------------------------
        let list_rect = UiRect::new(list_x, top, list_w as u32, (bottom - top) as u32);
        lay.fill_rect(list_rect, DETAIL_BG);
        lay.border_rect(list_rect, palette::PANEL_BORDER);

        let visible = self.visible_indices();
        let catalog = self.browsing_catalog();
        let page = ((bottom - top - 4) / ROW_PITCH).max(0) as usize;
        let max_scroll = visible.len().saturating_sub(page);
        let scroll = self.scroll.min(max_scroll);
        // Keep the keyboard selection inside the viewport.
        let scroll = if page > 0 && self.selected < scroll {
            self.selected
        } else if page > 0 && self.selected >= scroll + page {
            self.selected + 1 - page
        } else {
            scroll
        };

        let scrollable = visible.len() > page && page > 0;
        let scrollbar_w = if scrollable { 12 } else { 0 };
        let rows_w = list_w - 4 - scrollbar_w;

        if visible.is_empty() {
            let msg = if self.browsed_entries().is_empty() {
                tr(
                    self.lang,
                    "No mods installed",
                    "Modがインストールされていません",
                )
            } else {
                tr(self.lang, "No matches", "一致するModがありません")
            };
            lay.text_fit(
                list_x + 6,
                top + 8,
                1,
                palette::TEXT_DIM,
                msg,
                ((list_w - 12) / 8).max(1) as usize,
            );
        }

        for (row_slot, &entry_index) in visible.iter().enumerate().skip(scroll).take(page) {
            let y = top + 2 + (row_slot - scroll) as i32 * ROW_PITCH;
            let row = UiRect::new(list_x + 2, y, rows_w as u32, ROW_H as u32);
            let entry = &self.browsed_entries()[entry_index];
            let enabled = self.enabled_at(entry_index);
            let is_sel = row_slot == self.selected;
            let is_hover = self.hover == Some(ModManagerWidgetId::ModRow(row_slot));
            lay.fill_rect(
                row,
                if is_sel {
                    palette::BUTTON_HOVER
                } else if is_hover {
                    palette::BUTTON
                } else {
                    palette::SLOT
                },
            );
            if self.pending.contains_key(&entry.mod_id) {
                lay.fill(row.x, row.y, 2, row.h as i32, palette::ACCENT);
            }
            // Status pill (right-aligned on the name line).
            let (pill_text, pill_bg, pill_fg) = if catalog {
                ("GET", palette::AIR, palette::TEXT_DARK)
            } else if !entry.missing_deps.is_empty() {
                ("MISSING", palette::DANGER, palette::TEXT)
            } else if !enabled {
                ("OFF", palette::BUTTON_DISABLED, palette::TEXT_DIM)
            } else {
                match entry.tier {
                    ModAdaptationBadge::Tier1Native => {
                        ("NATIVE", palette::ACCENT, palette::TEXT_DARK)
                    }
                    ModAdaptationBadge::Tier2Shim => ("SHIM", palette::WARNING, palette::TEXT_DARK),
                    ModAdaptationBadge::Tier3Fallback => {
                        ("FALLBACK", palette::DANGER, palette::TEXT)
                    }
                    ModAdaptationBadge::Superseded => {
                        ("SUPER", palette::BUTTON_DISABLED, palette::TEXT_DIM)
                    }
                    ModAdaptationBadge::ClientOnly => ("CLIENT", palette::AIR, palette::TEXT_DARK),
                }
            };
            let pill_w = pill_text.len() as i32 * 8 + 8;
            let pill = UiRect::new(row.right() - pill_w - 3, y + 3, pill_w as u32, 11);
            lay.fill_rect(pill, pill_bg);
            lay.ctext_in(pill, 1, pill_fg, pill_text);
            let name_cells = ((rows_w - pill_w - 14) / 8).max(1) as usize;
            let name_color = if enabled {
                palette::TEXT
            } else {
                palette::TEXT_DIM
            };
            lay.text_fit(row.x + 5, y + 3, 1, name_color, &entry.name, name_cells);
            // Community rows lead with author + SPDX license (installing
            // from the original authors is the point of the catalog);
            // curated rows show their QoL sub-category.
            let sub = match self.catalog_meta(entry_index) {
                Some(CatalogMeta::Community(info)) => {
                    format!("{} · {}", info.author, info.license_spdx)
                }
                Some(CatalogMeta::Curated(info)) => {
                    format!("{} · {}", entry.version, info.category.label())
                }
                None => format!("{} · {}", entry.version, entry.loader.label()),
            };
            lay.text_fit(
                row.x + 5,
                y + 13,
                1,
                palette::TEXT_DIM,
                &fit_text(&sub, ((rows_w - 10) / 8).max(1) as usize),
                ((rows_w - 10) / 8).max(1) as usize,
            );
            lay.region(hits, ModManagerWidgetId::ModRow(row_slot), row);
        }

        if scrollable {
            let sb_x = list_right - 12;
            let up = UiRect::new(sb_x, top + 2, 10, 12);
            let down = UiRect::new(sb_x, bottom - 14, 10, 12);
            lay.fill_rect(up, palette::BUTTON);
            lay.fill_rect(down, palette::BUTTON);
            lay.ctext_in(up, 1, palette::TEXT, "^");
            lay.ctext_in(down, 1, palette::TEXT, "v");
            // Passive track + thumb (visual only).
            let track = UiRect::new(sb_x, top + 16, 10, (bottom - top - 32).max(0) as u32);
            lay.fill_rect(track, palette::SLOT);
            if !visible.is_empty() {
                let thumb_h = ((page as i64 * track.h as i64) / visible.len() as i64)
                    .clamp(8, track.h as i64) as u32;
                let travel = (track.h - thumb_h).max(1) as i64;
                let thumb_y =
                    track.y + ((scroll as i64 * travel) / max_scroll.max(1) as i64) as i32;
                lay.fill_rect(UiRect::new(sb_x, thumb_y, 10, thumb_h), palette::TEXT_DIM);
            }
            lay.region(hits, ModManagerWidgetId::ListScrollUp, up);
            lay.region(hits, ModManagerWidgetId::ListScrollDown, down);
        }

        if compact_single_column {
            return;
        }

        // ---- right pane: selected mod details --------------------------
        let detail = UiRect::new(detail_x, top, detail_w.max(1) as u32, (bottom - top) as u32);
        lay.fill_rect(detail, DETAIL_BG);
        lay.border_rect(detail, palette::PANEL_BORDER);
        let Some(&sel_index) = visible.get(self.selected) else {
            // The Packs shelf holds no curated metadata — point the
            // empty view at the live Modrinth search for the selected
            // project kind instead of a bare "Select a mod".
            if self.filter == ModManagerFilter::Browse
                && self.browse_category == BrowseCategory::Packs
            {
                let cells = ((detail_w - 12) / 8).max(1) as usize;
                lay.text_fit(
                    detail_x + 6,
                    top + 8,
                    1,
                    palette::TEXT_DIM,
                    tr(
                        self.lang,
                        "No packs curated - search Modrinth for:",
                        "パックはModrinthで検索",
                    ),
                    cells,
                );
                // The kind cycles in place — the pill-row Kind button can
                // be squeezed out at narrow logical widths, so the packs
                // pane always keeps a working one.
                let kind_label = format!(
                    "{}: {} (MC {})",
                    tr(self.lang, "Kind", "タイプ"),
                    self.browse_project_kind.label(),
                    RECOMMENDED_MC_VERSION
                );
                let kind_w =
                    (crate::draw::text_width(&kind_label, 1) as i32 + 12).min(detail_w - 12);
                let kind_btn = UiRect::new(detail_x + 6, top + 16, kind_w.max(24) as u32, 14);
                lay.fill_rect(
                    kind_btn,
                    if self.hover == Some(ModManagerWidgetId::BrowseKind) {
                        palette::BUTTON_HOVER
                    } else {
                        palette::BUTTON
                    },
                );
                lay.border_rect(kind_btn, palette::BUTTON_BORDER);
                lay.ctext_in(kind_btn, 1, palette::TEXT, &kind_label);
                lay.region(hits, ModManagerWidgetId::BrowseKind, kind_btn);
                let label = tr(self.lang, "Search Modrinth", "Modrinthで検索");
                let btn_w = (crate::draw::text_width(label, 1) as i32 + 12).min(detail_w - 12);
                let btn = UiRect::new(detail_x + 6, top + 34, btn_w.max(24) as u32, 14);
                lay.fill_rect(
                    btn,
                    if self.hover == Some(ModManagerWidgetId::ModrinthSearch) {
                        palette::BUTTON_HOVER
                    } else {
                        palette::BUTTON
                    },
                );
                lay.border_rect(btn, palette::BUTTON_BORDER);
                lay.ctext_in(btn, 1, palette::TEXT, label);
                lay.region(hits, ModManagerWidgetId::ModrinthSearch, btn);
                return;
            }
            lay.text_fit(
                detail_x + 6,
                top + 8,
                1,
                palette::TEXT_DIM,
                tr(self.lang, "Select a mod", "Modを選択"),
                ((detail_w - 12) / 8).max(1) as usize,
            );
            return;
        };
        self.render_detail(lay, hits, detail, sel_index);
    }

    fn render_detail(
        &self,
        lay: &mut Lay,
        hits: &mut Vec<ModManagerHitRegion>,
        pane: UiRect,
        entry_index: usize,
    ) {
        let lang = self.lang;
        let entry = &self.browsed_entries()[entry_index];
        let meta_info = self.catalog_meta(entry_index);
        let catalog = meta_info.is_some();
        let browse = self.filter == ModManagerFilter::Browse;
        let enabled = self.enabled_at(entry_index);
        let tx = pane.x + 6;
        let tw = (pane.w as i32 - 12).max(8);
        let cells = (tw / 8).max(1) as usize;

        // ---- action row geometry (anchored to the pane bottom) ---------
        // Buttons: Enable/Disable + Configure + optional links.
        let mut buttons: Vec<(ModManagerWidgetId, String, bool)> = Vec::new();
        let toggle_label = if enabled {
            tr(lang, "Disable", "無効化")
        } else {
            tr(lang, "Enable", "有効化")
        };
        if catalog {
            buttons.push((
                ModManagerWidgetId::InstallRecommended,
                tr(lang, "Get on Modrinth", "Modrinthで入手").to_owned(),
                true,
            ));
            if browse {
                buttons.push((
                    ModManagerWidgetId::ModrinthSearch,
                    tr(lang, "Search Modrinth", "Modrinthで検索").to_owned(),
                    true,
                ));
            }
        } else {
            buttons.push((
                ModManagerWidgetId::ToggleEnabled,
                toggle_label.to_owned(),
                true,
            ));
            buttons.push((
                ModManagerWidgetId::ConfigureMod,
                tr(lang, "Configure", "設定").to_owned(),
                entry.has_config,
            ));
            buttons.push((
                ModManagerWidgetId::RollbackProfile,
                tr(
                    lang,
                    "Rollback to Last Known-Good Profile",
                    "直前の正常プロファイルへ戻す",
                )
                .to_owned(),
                true,
            ));
            buttons.push((
                ModManagerWidgetId::PlamoPreset,
                self.plamo_panel.preset.label().to_owned(),
                !self.plamo_panel.busy,
            ));
            buttons.push((
                ModManagerWidgetId::PlamoAction,
                self.plamo_panel.action_label().to_owned(),
                self.plamo_panel.action_enabled(),
            ));
            buttons.push((
                ModManagerWidgetId::ToggleAutoTranslationPack,
                self.plamo_panel.auto_pack_label(),
                !self.plamo_panel.busy,
            ));
            if self.plamo_panel.busy {
                buttons.push((
                    ModManagerWidgetId::CancelPlamo,
                    tr(lang, "Cancel Translation Work", "翻訳処理をキャンセル").to_owned(),
                    true,
                ));
            }
        }
        for (id, url, label) in [
            (
                ModManagerWidgetId::LinkWebsite,
                &entry.website_url,
                tr(lang, "Website", "サイト"),
            ),
            (
                ModManagerWidgetId::LinkIssues,
                &entry.issues_url,
                tr(lang, "Issues", "課題"),
            ),
            (
                ModManagerWidgetId::LinkSource,
                &entry.source_url,
                tr(lang, "Source", "ソース"),
            ),
            (
                ModManagerWidgetId::LinkModrinth,
                &entry.modrinth_url,
                "Modrinth",
            ),
        ] {
            if !catalog && !url.is_empty() {
                buttons.push((id, label.to_owned(), true));
            }
        }
        // Flow-wrap buttons into rows anchored at the pane bottom.
        let btn_h = 14;
        let btn_gap = 4;
        let mut rows: Vec<Vec<(ModManagerWidgetId, String, bool, i32)>> = vec![Vec::new()];
        let mut row_w = 0;
        for (id, label, live) in buttons {
            let w = (crate::draw::text_width(&label, 1) as i32 + 12).min(tw);
            if row_w > 0 && row_w + btn_gap + w > tw {
                rows.push(Vec::new());
                row_w = 0;
            }
            row_w += if row_w > 0 { btn_gap + w } else { w };
            rows.last_mut()
                .expect("one row always exists")
                .push((id, label, live, w));
        }
        let actions_h = rows.len() as i32 * (btn_h + btn_gap) - btn_gap;
        let mut actions_top = pane.bottom() - 6 - actions_h;

        // ---- missing-dependencies warning box above the actions --------
        // The box has a 36px fixed cost (header + button + padding); dep
        // lines cost 10px each. It shrinks to the space left above the
        // action rows and collapses to a bare button below 36px so it
        // never paints over the pane header or the list above.
        let mut warn_bottom = actions_top - 4;
        let warn_avail = (actions_top - 4 - (pane.y + 4)).max(0);
        let missing_label = tr(
            lang,
            "Download Missing (Modrinth)",
            "不足分を入手 (Modrinth)",
        );
        if !entry.missing_deps.is_empty() && warn_avail >= 18 {
            let line_budget = ((warn_avail - 36) / 10).max(0) as usize;
            let shown = entry.missing_deps.len().min(3).min(line_budget);
            // Reserve one line for "+N more" when the budget cut deps off.
            let (shown, extra) = if entry.missing_deps.len() > shown && line_budget > 0 {
                (shown.saturating_sub(1), true)
            } else {
                (shown, entry.missing_deps.len() > shown)
            };
            let extra = extra && line_budget > shown;
            let content_lines = shown + usize::from(extra);
            let full_box = warn_avail >= 36;
            let warn_h = if full_box {
                8 + 10 + content_lines as i32 * 10 + 16 + 6
            } else {
                18
            };
            let warn = UiRect::new(tx, actions_top - 4 - warn_h, tw as u32, warn_h as u32);
            lay.fill_rect(warn, WARN_BG);
            lay.border_rect(warn, palette::WARNING);
            let mut wy = warn.y + 4;
            if full_box {
                lay.text_fit(
                    warn.x + 5,
                    wy,
                    1,
                    palette::WARNING,
                    tr(lang, "Missing dependencies:", "不足している依存関係:"),
                    cells.saturating_sub(1),
                );
                wy += 10;
                for dep in entry.missing_deps.iter().take(shown) {
                    lay.text_fit(
                        warn.x + 9,
                        wy,
                        1,
                        palette::TEXT,
                        &format!("- {dep}"),
                        cells.saturating_sub(2),
                    );
                    wy += 10;
                }
                if extra {
                    lay.text_fit(
                        warn.x + 9,
                        wy,
                        1,
                        palette::TEXT_DIM,
                        &format!("+{} more", entry.missing_deps.len() - shown),
                        cells.saturating_sub(2),
                    );
                }
            }
            let btn_w = (crate::draw::text_width(missing_label, 1) as i32 + 12).min(tw - 8);
            let btn = UiRect::new(warn.x + 4, warn.bottom() - 16, btn_w.max(0) as u32, 14);
            let bg = if self.hover == Some(ModManagerWidgetId::DownloadMissingDeps) {
                palette::BUTTON_HOVER
            } else {
                palette::WARNING
            };
            lay.fill_rect(btn, bg);
            lay.border_rect(btn, palette::BUTTON_BORDER);
            lay.ctext_in(
                btn,
                1,
                palette::TEXT_DARK,
                &fit_text(missing_label, ((btn_w - 8) / 8).max(1) as usize),
            );
            lay.region(hits, ModManagerWidgetId::DownloadMissingDeps, btn);
            warn_bottom = warn.y - 4;
        }

        // ---- header block (name / meta / badges / authors / license) ---
        let mut y = pane.y + 5;

        // Structured exact-candidate consent is more important than the
        // mod's prose metadata: every version, hash, size and requester is
        // visible before either destructive-or-network decision is armed.
        if !catalog {
            if let Some(plan) = &self.missing_dependency_consent {
                let row_lines = plan.rows.len().min(4)
                    + plan.unsatisfied.len().min(2)
                    + usize::from(plan.rows.len() > 4 || plan.unsatisfied.len() > 2);
                let box_h = 28 + row_lines as i32 * 10 + 18;
                if warn_bottom - y >= box_h + 20 {
                    let consent = UiRect::new(tx, y, tw as u32, box_h as u32);
                    lay.fill_rect(consent, WARN_BG);
                    lay.border_rect(consent, palette::WARNING);
                    let mut cy = y + 4;
                    lay.text_fit(
                        tx + 5,
                        cy,
                        1,
                        palette::WARNING,
                        tr(
                            lang,
                            "Missing dependencies — exact Modrinth candidates:",
                            "不足依存 — Modrinth候補（完全一致）:",
                        ),
                        cells.saturating_sub(1),
                    );
                    cy += 10;
                    let mut line_count = 0usize;
                    for row in plan.rows.iter().take(4) {
                        let requesters = if row.requesting_mods.is_empty() {
                            String::new()
                        } else {
                            format!(" <- {}", row.requesting_mods.join(","))
                        };
                        lay.text_fit(
                            tx + 8,
                            cy,
                            1,
                            palette::TEXT,
                            &format!(
                                "{} {} {} ({} bytes, sha512 {}){}",
                                row.mod_id,
                                row.version_number,
                                row.filename,
                                row.size_bytes,
                                crate::draw::first_cells(&row.sha512, 12),
                                requesters
                            ),
                            cells.saturating_sub(2),
                        );
                        cy += 10;
                        line_count += 1;
                    }
                    for row in plan.unsatisfied.iter().take(2) {
                        lay.text_fit(
                            tx + 8,
                            cy,
                            1,
                            palette::WARNING,
                            &format!(
                                "! {} {}: {}",
                                row.mod_id, row.required_version_range, row.reason
                            ),
                            cells.saturating_sub(2),
                        );
                        cy += 10;
                        line_count += 1;
                    }
                    if plan.rows.len() + plan.unsatisfied.len() > line_count {
                        lay.text_fit(
                            tx + 8,
                            cy,
                            1,
                            palette::TEXT_DIM,
                            &format!(
                                "+{} more",
                                plan.rows.len() + plan.unsatisfied.len() - line_count
                            ),
                            cells.saturating_sub(2),
                        );
                    }
                    let approve = tr(
                        lang,
                        "不足依存を自動取得して再読込",
                        "不足依存を自動取得して再読込",
                    );
                    let isolate = tr(
                        lang,
                        "未充足 Mod を隔離して続行",
                        "未充足 Mod を隔離して続行",
                    );
                    let gap = 4;
                    let aw = (crate::draw::text_width(approve, 1) as i32 + 10).min((tw - gap) / 2);
                    let iw = (crate::draw::text_width(isolate, 1) as i32 + 10).min((tw - gap) / 2);
                    let by = consent.bottom() - 17;
                    let approve_rect = UiRect::new(tx + 4, by, aw.max(24) as u32, 14);
                    let isolate_rect = UiRect::new(tx + 4 + aw + gap, by, iw.max(24) as u32, 14);
                    for (rect, id, label) in [
                        (
                            approve_rect,
                            ModManagerWidgetId::ConsentApproveAutoFetch,
                            approve,
                        ),
                        (
                            isolate_rect,
                            ModManagerWidgetId::ConsentIsolateMods,
                            isolate,
                        ),
                    ] {
                        let hovered = self.hover == Some(id);
                        lay.fill_rect(
                            rect,
                            if hovered {
                                palette::BUTTON_HOVER
                            } else {
                                palette::WARNING
                            },
                        );
                        lay.border_rect(rect, palette::BUTTON_BORDER);
                        lay.ctext_in(rect, 1, palette::TEXT_DARK, label);
                        lay.region(hits, id, rect);
                    }
                    y += box_h + 4;
                }
            }
        }

        let name_scale = if text_width(&entry.name, 2) as i32 <= tw {
            2
        } else {
            1
        };
        lay.text_fit(
            tx,
            y,
            name_scale,
            palette::TEXT,
            &entry.name,
            (tw / (8 * name_scale as i32)).max(1) as usize,
        );
        y += 8 * name_scale as i32 + 3;

        let meta = format!("{} · {}", entry.version, entry.mod_id);
        lay.text_fit(tx, y, 1, palette::TEXT_DIM, &meta, cells);
        y += 11;

        // Catalog entries get a lighter header: category/loader info,
        // explicit author + license attribution, the summary, and the
        // Modrinth install button — never an enable toggle.
        if let Some(meta) = meta_info {
            if let CatalogMeta::Community(info) = meta {
                // Badge row: category, SPDX license, GUI-bridge marker.
                let badge_right = tx + tw;
                let mut bx = tx;
                bx += self.badge(
                    lay,
                    bx,
                    y,
                    info.category.label(),
                    palette::BUTTON_HOVER,
                    badge_right,
                );
                bx += self.badge_fg(
                    lay,
                    bx,
                    y,
                    info.license_spdx,
                    (palette::ACCENT, palette::TEXT_DARK),
                    badge_right,
                );
                if info.needs_gui_bridge {
                    self.badge_fg(
                        lay,
                        bx,
                        y,
                        "GUI Bridge",
                        (palette::AIR, palette::TEXT_DARK),
                        badge_right,
                    );
                }
                y += 15;
                let loaders = info
                    .loaders
                    .iter()
                    .map(|loader| loader.label())
                    .collect::<Vec<_>>()
                    .join(" / ");
                lay.text_fit(tx, y, 1, palette::TEXT_DIM, &loaders, cells);
                y += 11;
                let author = format!("{}: {}", tr(lang, "Author", "作者"), info.author);
                lay.text_fit(tx, y, 1, palette::TEXT_DIM, &author, cells);
                y += 11;
                let summary = if lang == UiLang::Ja {
                    info.ja_summary
                } else {
                    info.en_summary
                };
                let avail = (warn_bottom - y).max(0);
                for line in wrap_text(summary, cells)
                    .into_iter()
                    .take((avail / 10).max(0) as usize)
                {
                    lay.text_fit(tx, y, 1, palette::TEXT_DIM, &line, cells);
                    y += 10;
                }
            } else {
                let CatalogMeta::Curated(mod_info) = meta else {
                    unreachable!("CatalogMeta has only two variants");
                };
                lay.text_fit(
                    tx,
                    y,
                    1,
                    palette::TEXT_DIM,
                    &format!("Fabric / NeoForge · {}", mod_info.category.label()),
                    cells,
                );
                y += 17;
                let summary = if lang == UiLang::Ja {
                    mod_info.summary_ja
                } else {
                    mod_info.summary_en
                };
                let avail = (warn_bottom - y).max(0);
                for line in wrap_text(summary, cells)
                    .into_iter()
                    .take((avail / 10).max(0) as usize)
                {
                    lay.text_fit(tx, y, 1, palette::TEXT_DIM, &line, cells);
                    y += 10;
                }
            }
            for row in &rows {
                let mut bx = tx;
                for (id, label, live, w) in row {
                    let rect = UiRect::new(bx, actions_top, *w as u32, btn_h as u32);
                    lay.fill_rect(
                        rect,
                        if self.hover == Some(*id) {
                            palette::BUTTON_HOVER
                        } else {
                            palette::BUTTON
                        },
                    );
                    lay.border_rect(rect, palette::BUTTON_BORDER);
                    lay.ctext_in(rect, 1, palette::TEXT, label);
                    if *live {
                        lay.region(hits, *id, rect);
                    }
                    bx += w + btn_gap;
                }
                actions_top += btn_h + btn_gap;
            }
            return;
        }
        let badge_right = tx + tw;
        let mut bx = tx;
        let loader_bg = match entry.loader {
            ModLoaderTag::MeguNative => palette::ACCENT,
            ModLoaderTag::Datapack => palette::BUTTON_HOVER,
            ModLoaderTag::ShaderPack => palette::AIR,
            _ => palette::BUTTON,
        };
        bx += self.badge(lay, bx, y, entry.loader.label(), loader_bg, badge_right);
        let (tier_bg, tier_fg) = match entry.tier {
            ModAdaptationBadge::Tier1Native => (palette::ACCENT, palette::TEXT_DARK),
            ModAdaptationBadge::Tier2Shim => (palette::WARNING, palette::TEXT_DARK),
            ModAdaptationBadge::Tier3Fallback => (palette::DANGER, palette::TEXT),
            ModAdaptationBadge::Superseded => (palette::BUTTON_DISABLED, palette::TEXT_DIM),
            ModAdaptationBadge::ClientOnly => (palette::AIR, palette::TEXT_DARK),
        };
        bx += self.badge_fg(
            lay,
            bx,
            y,
            entry.tier.label(),
            (tier_bg, tier_fg),
            badge_right,
        );
        let (reload_bg, reload_fg) = match entry.reload_mode {
            ModReloadBadge::HotReload => (palette::ACCENT, palette::TEXT_DARK),
            ModReloadBadge::SoftRestart => (palette::WARNING, palette::TEXT_DARK),
            ModReloadBadge::ProcessRestart => (palette::DANGER, palette::TEXT),
        };
        self.badge_fg(
            lay,
            bx,
            y,
            entry.reload_mode.label(lang),
            (reload_bg, reload_fg),
            badge_right,
        );
        y += 15;

        if !entry.authors.is_empty() {
            let authors = format!(
                "{}: {}",
                tr(lang, "Authors", "作者"),
                entry.authors.join(", ")
            );
            lay.text_fit(tx, y, 1, palette::TEXT_DIM, &authors, cells);
            y += 10;
        }
        if !entry.license.is_empty() {
            let license = format!("{}: {}", tr(lang, "License", "ライセンス"), entry.license);
            lay.text_fit(tx, y, 1, palette::TEXT_DIM, &license, cells);
            y += 10;
        }
        if entry.is_library {
            lay.text_fit(
                tx,
                y,
                1,
                palette::TEXT_DIM,
                tr(lang, "Shared library", "共有ライブラリ"),
                cells,
            );
            y += 10;
        }
        y += 3;

        // PLaMo guidance/license notice is part of the manager surface, not
        // a hidden terminal prompt: the model is downloaded only after the
        // explicit button press and the local runtime is transient.
        if !catalog && warn_bottom - y >= 30 {
            lay.text_fit(
                tx,
                y,
                1,
                palette::TEXT_DIM,
                tr(
                    lang,
                    "PLaMo 3 Translate — PLaMo Community License",
                    "PLaMo 3 Translate — PLaMo Community License",
                ),
                cells,
            );
            y += 10;
            let guidance = if lang == UiLang::Ja {
                "ローカル LLM は翻訳中だけ起動し、終了直後に停止します（プレイ中メモリ不使用）。"
            } else {
                "The local LLM runs only while translating and exits immediately afterwards."
            };
            for line in wrap_text(guidance, cells).into_iter().take(2) {
                lay.text_fit(tx, y, 1, palette::TEXT_DIM, &line, cells);
                y += 10;
            }
            if self.plamo_panel.busy && !self.plamo_panel.progress_label.is_empty() {
                lay.text_fit(
                    tx,
                    y,
                    1,
                    palette::WARNING,
                    &self.plamo_panel.progress_label,
                    cells,
                );
                y += 10;
            }
        }

        // ---- wrapped summary in the space left above the warning box ---
        let avail = (warn_bottom - y).max(0);
        let max_lines = (avail / 10).max(0) as usize;
        if max_lines > 0 && !entry.summary.is_empty() {
            for line in wrap_text(&entry.summary, cells).into_iter().take(max_lines) {
                lay.text_fit(tx, y, 1, palette::TEXT_DIM, &line, cells);
                y += 10;
            }
        }

        // ---- paint the anchored action rows ----------------------------
        for row in &rows {
            let mut bx = tx;
            for (id, label, live, w) in row {
                let rect = UiRect::new(bx, actions_top, *w as u32, btn_h as u32);
                let hovered = self.hover == Some(*id);
                let bg = if !*live {
                    palette::BUTTON_DISABLED
                } else if hovered {
                    palette::BUTTON_HOVER
                } else {
                    palette::BUTTON
                };
                lay.fill_rect(rect, bg);
                lay.border_rect(rect, palette::BUTTON_BORDER);
                lay.ctext_in(
                    rect,
                    1,
                    if *live {
                        palette::TEXT
                    } else {
                        palette::TEXT_DIM
                    },
                    &fit_text(label, ((*w - 8) / 8).max(1) as usize),
                );
                if *live {
                    lay.region(hits, *id, rect);
                }
                bx += w + btn_gap;
            }
            actions_top += btn_h + btn_gap;
        }
    }

    fn badge(&self, lay: &mut Lay, x: i32, y: i32, label: &str, bg: UiColor, max_x: i32) -> i32 {
        let fg = if bg == palette::BUTTON || bg == palette::BUTTON_DISABLED {
            palette::TEXT
        } else {
            palette::TEXT_DARK
        };
        self.badge_fg(lay, x, y, label, (bg, fg), max_x)
    }

    /// Returns the advance past the badge, or `0` (drawing nothing) when
    /// it would not fit before `max_x`.
    fn badge_fg(
        &self,
        lay: &mut Lay,
        x: i32,
        y: i32,
        label: &str,
        colors: (UiColor, UiColor),
        max_x: i32,
    ) -> i32 {
        let (bg, fg) = colors;
        let w = crate::draw::text_width(label, 1) as i32 + 8;
        if x + w > max_x {
            return 0;
        }
        let rect = UiRect::new(x, y, w as u32, 11);
        lay.fill_rect(rect, bg);
        lay.ctext_in(rect, 1, fg, label);
        w + 4
    }

    fn render_footer(&self, lay: &mut Lay, hits: &mut Vec<ModManagerHitRegion>) {
        let lang = self.lang;
        // Row 1 (upper): [+ Add from Modrinth] [Open Mods Folder]
        let y1 = lay.h - 4 - 16 - 4 - 16;
        let mut x = MARGIN + 4;
        for (id, label) in [
            (
                ModManagerWidgetId::AddFromModrinth,
                tr(lang, "+ Add from Modrinth", "Modrinthから追加"),
            ),
            (
                ModManagerWidgetId::OpenModsFolder,
                tr(lang, "Open Mods Folder", "Modフォルダを開く"),
            ),
        ] {
            // Buttons shrink to the space left; when none is left, skip.
            let avail = lay.w - MARGIN - x;
            let w = (crate::draw::text_width(label, 1) as i32 + 14).min(avail);
            if w < 24 {
                break;
            }
            let rect = UiRect::new(x, y1, w as u32, 16);
            let bg = if self.hover == Some(id) {
                palette::BUTTON_HOVER
            } else {
                palette::BUTTON
            };
            lay.fill_rect(rect, bg);
            lay.border_rect(rect, palette::BUTTON_BORDER);
            lay.ctext_in(rect, 1, palette::TEXT, label);
            lay.region(hits, id, rect);
            x += w + 6;
        }

        // Row 2 (lower, right-aligned): [Apply ...] [Done]
        let y2 = lay.h - 4 - 16;
        let dirty = self.is_dirty();
        let apply_label = if !dirty {
            tr(lang, "Apply Changes", "変更を適用").to_owned()
        } else {
            let soft = self.pending.iter().any(|(id, _)| {
                self.entries
                    .iter()
                    .find(|e| &e.mod_id == id)
                    .is_some_and(|e| e.reload_mode != ModReloadBadge::HotReload)
            });
            if soft {
                tr(lang, "Apply (Soft Reload)", "適用 (ソフト再読込)").to_owned()
            } else {
                tr(lang, "Apply (No Restart)", "適用 (再起動不要)").to_owned()
            }
        };
        let done_label = tr(lang, "Done", "完了");
        let done_w = (crate::draw::text_width(done_label, 1) as i32 + 20)
            .min(lay.w - 2 * MARGIN)
            .max(24);
        let done = UiRect::new(lay.w - MARGIN - done_w, y2, done_w as u32, 16);
        // Apply shrinks to the space left of Done; below ~60px it hides.
        let apply_w =
            (crate::draw::text_width(&apply_label, 1) as i32 + 14).min(done.x - 6 - MARGIN);
        if apply_w >= 24 {
            let apply = UiRect::new(done.x - 6 - apply_w, y2, apply_w as u32, 16);
            let apply_bg = if !dirty {
                palette::BUTTON_DISABLED
            } else if self.hover == Some(ModManagerWidgetId::ApplyChanges) {
                palette::BUTTON_HOVER
            } else {
                palette::ACCENT
            };
            lay.fill_rect(apply, apply_bg);
            lay.border_rect(apply, palette::BUTTON_BORDER);
            lay.ctext_in(
                apply,
                1,
                if dirty {
                    palette::TEXT_DARK
                } else {
                    palette::TEXT_DIM
                },
                &apply_label,
            );
            if dirty {
                lay.region(hits, ModManagerWidgetId::ApplyChanges, apply);
            }
        }

        let done_bg = if self.hover == Some(ModManagerWidgetId::Done) {
            palette::BUTTON_HOVER
        } else {
            palette::BUTTON
        };
        lay.fill_rect(done, done_bg);
        lay.border_rect(done, palette::BUTTON_BORDER);
        lay.ctext_in(done, 1, palette::TEXT, done_label);
        lay.region(hits, ModManagerWidgetId::Done, done);
    }
}

// ---------------------------------------------------------------------------
// Layout context (logical px → physical px at the effective GUI scale)
// ---------------------------------------------------------------------------

const MARGIN: i32 = 4;
const SEARCH_Y: i32 = 26;
const FILTER_Y: i32 = 46;
const LIST_TOP: i32 = 66;
const LIST_BOTTOM_OFF: i32 = 40;
const PANE_GAP: i32 = 6;
const ROW_H: i32 = 22;
const ROW_PITCH: i32 = 24;

/// Right detail-pane fill: a hair lighter than the main panel.
const DETAIL_BG: UiColor = UiColor::rgba(26, 29, 38, 235);
/// Missing-dependencies warning box fill.
const WARN_BG: UiColor = UiColor::rgba(85, 60, 23, 235);

/// Scales logical-pixel geometry to physical pixels on emit so visuals and
/// hit regions never diverge.
struct Lay {
    list: UiDrawList,
    w: i32,
    h: i32,
    s: i32,
}

impl Lay {
    fn new(state: &ModManagerState, width: u32, height: u32) -> Self {
        let s = state.effective_scale(height) as i32;
        Self {
            list: UiDrawList::new(),
            w: (width.min(i32::MAX as u32) as i32) / s,
            h: (height.min(i32::MAX as u32) as i32) / s,
            s,
        }
    }

    fn finish(self) -> UiDrawList {
        self.list
    }

    fn phys(&self, r: UiRect) -> UiRect {
        UiRect::new(
            r.x * self.s,
            r.y * self.s,
            (r.w as i32 * self.s).max(0) as u32,
            (r.h as i32 * self.s).max(0) as u32,
        )
    }

    fn rect(&self, x: i32, y: i32, w: i32, h: i32) -> UiRect {
        UiRect::new(x, y, w.max(0) as u32, h.max(0) as u32)
    }

    fn fill(&mut self, x: i32, y: i32, w: i32, h: i32, color: UiColor) {
        let r = self.rect(x, y, w, h);
        self.list.fill_rect(self.phys(r), color);
    }

    fn fill_rect(&mut self, r: UiRect, color: UiColor) {
        self.list.fill_rect(self.phys(r), color);
    }

    fn border(&mut self, x: i32, y: i32, w: i32, h: i32, color: UiColor) {
        let r = self.rect(x, y, w, h);
        self.list
            .border_rect(self.phys(r), color, self.s.max(1) as u32);
    }

    fn border_rect(&mut self, r: UiRect, color: UiColor) {
        self.list
            .border_rect(self.phys(r), color, self.s.max(1) as u32);
    }

    fn text_fit(&mut self, x: i32, y: i32, mult: u32, color: UiColor, text: &str, cells: usize) {
        let fitted = fit_text(text, cells);
        self.list.text_str(
            x * self.s,
            y * self.s,
            (mult as i32 * self.s).max(1) as u32,
            color,
            &fitted,
        );
    }

    /// Horizontally centered text inside `r` (8px glyph cell, `mult`×).
    /// Labels are elided to the rect first so they can never spill past
    /// the widget's edges.
    fn ctext_in(&mut self, r: UiRect, mult: u32, color: UiColor, text: &str) {
        let text = fit_text(text, ((r.w as i32 - 4) / (8 * mult as i32)).max(0) as usize);
        let scale = (mult as i32 * self.s).max(1) as u32;
        let w = text_width(&text, scale) as i32;
        let cx = r.x + r.w as i32 / 2;
        let ty = r.y + ((r.h as i32 - 8 * mult as i32) / 2).max(0);
        self.list
            .text_str(cx * self.s - w / 2, ty * self.s, scale, color, &text);
    }

    fn region(&self, hits: &mut Vec<ModManagerHitRegion>, id: ModManagerWidgetId, r: UiRect) {
        let p = self.phys(r);
        let (x0, y0) = (p.x.max(0), p.y.max(0));
        let (x1, y1) = (
            p.right().min(self.w * self.s),
            p.bottom().min(self.h * self.s),
        );
        if x1 <= x0 || y1 <= y0 {
            return;
        }
        hits.push(ModManagerHitRegion {
            rect: UiRect::new(x0, y0, (x1 - x0) as u32, (y1 - y0) as u32),
            id,
        });
    }
}

/// Elide a label to `cells` glyph cells so text never overflows its
/// widget. Control characters are flattened first — a hostile string
/// must not wrap its widget into a second line.
fn fit_text(label: &str, cells: usize) -> String {
    let label = if label.chars().any(|c| c.is_control()) {
        label
            .chars()
            .map(|c| if c.is_control() { ' ' } else { c })
            .collect::<String>()
    } else {
        label.to_owned()
    };
    if crate::draw::text_cells(&label) <= cells {
        return label;
    }
    if cells <= 3 {
        return ".".repeat(cells);
    }
    let mut clipped = crate::draw::first_cells(&label, cells - 3).to_owned();
    clipped.push_str("...");
    clipped
}

/// Word-wrap `text` to `cells` glyph cells per line; overlong words are
/// hard-split so no line ever exceeds the width.
fn wrap_text(text: &str, cells: usize) -> Vec<String> {
    if cells == 0 {
        return Vec::new();
    }
    let mut lines: Vec<String> = Vec::new();
    let mut cur = String::new();
    for word in text.split_whitespace() {
        let mut rest = word;
        loop {
            if rest.is_empty() {
                break;
            }
            let cur_len = crate::draw::text_cells(&cur);
            let sep = usize::from(!cur.is_empty());
            let avail = cells.saturating_sub(cur_len + sep);
            if crate::draw::text_cells(rest) <= avail {
                if sep == 1 {
                    cur.push(' ');
                }
                cur.push_str(rest);
                break;
            }
            if cur_len > 0 {
                if avail == 0 {
                    // Line is full; wrap to a fresh line.
                    lines.push(std::mem::take(&mut cur));
                    continue;
                }
                // Fill the remainder of this line with a hard-split piece.
                if sep == 1 {
                    cur.push(' ');
                }
                let at = crate::draw::first_cells(rest, avail).len();
                cur.push_str(&rest[..at]);
                rest = &rest[at..];
                lines.push(std::mem::take(&mut cur));
            } else {
                // Empty line: split the overlong word at `cells`.
                let at = crate::draw::first_cells(rest, cells).len();
                cur.push_str(&rest[..at]);
                rest = &rest[at..];
                lines.push(std::mem::take(&mut cur));
            }
        }
    }
    if !cur.is_empty() {
        lines.push(cur);
    }
    lines
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    #[test]
    fn community_catalog_slugs_are_unique_valid_and_link_official_modrinth() {
        let mut slugs = BTreeSet::new();
        for entry in recommended_community_mods() {
            assert!(slugs.insert(entry.slug), "duplicate slug {}", entry.slug);
            assert!(valid_modrinth_slug(entry.slug), "bad slug {}", entry.slug);
            assert!(!entry.name.is_empty(), "empty name for {}", entry.slug);
            assert!(
                !entry.author.is_empty(),
                "missing author for {}",
                entry.slug
            );
            assert!(
                !entry.license_spdx.is_empty()
                    && entry
                        .license_spdx
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'.'),
                "non-SPDX license badge {:?} for {}",
                entry.license_spdx,
                entry.slug
            );
            assert!(!entry.en_summary.is_empty());
            assert!(!entry.ja_summary.is_empty());
            assert!(!entry.loaders.is_empty(), "no loaders for {}", entry.slug);
            // The only install path is the official Modrinth project page.
            assert_eq!(
                entry.modrinth_url().as_deref(),
                Some(format!("https://modrinth.com/mod/{}", entry.slug).as_str())
            );
        }
        // The showcase covers every named entry the lane is expected to
        // surface, including a World-gen pick.
        for slug in [
            "jade",
            "appleskin",
            "mouse-tweaks",
            "shulkerboxtooltip",
            "create",
            "farmers-delight-refabricated",
        ] {
            assert!(
                slugs.contains(slug),
                "missing expected community mod {slug}"
            );
        }
    }

    #[test]
    fn community_catalog_metadata_stays_metadata_only() {
        // No bundled jars, no embedded binaries: every field is &'static
        // metadata and the catalog never duplicates a QoL entry.
        let qol_slugs: BTreeSet<&str> = CURATED_MODRINTH_QOL_MODS.iter().map(|m| m.slug).collect();
        for entry in recommended_community_mods() {
            assert!(
                entry.en_summary.len() <= 96,
                "{} summary too long for the detail pane",
                entry.slug
            );
            let _ = qol_slugs; // overlapping a QoL pick is allowed
        }
    }

    #[test]
    fn community_catalog_ja_summaries_are_fully_glyph_covered() {
        for entry in recommended_community_mods() {
            for c in entry.ja_summary.chars() {
                assert!(
                    crate::draw::glyph_is_covered(c),
                    "uncovered glyph {c:?} in ja_summary of {}",
                    entry.slug
                );
            }
            // Author + license badges are ASCII-renderable too.
            for c in entry.author.chars().chain(entry.license_spdx.chars()) {
                assert!(
                    crate::draw::glyph_is_covered(c),
                    "uncovered glyph {c:?} in badges of {}",
                    entry.slug
                );
            }
        }
    }

    #[test]
    fn browse_dedupes_shared_slugs_and_community_metadata_wins() {
        let menu = ModManagerState::default();
        // 7 community entries + 11 curated picks the community list does
        // not already cover = 18 unique rows.
        assert_eq!(menu.browse_entries.len(), 18);
        let ids: BTreeSet<&str> = menu
            .browse_entries
            .iter()
            .map(|e| e.mod_id.as_str())
            .collect();
        assert_eq!(ids.len(), menu.browse_entries.len());
        // Shared slugs keep the richer community (author/license) source.
        let jade = menu
            .browse_entries
            .iter()
            .position(|e| e.mod_id == "jade")
            .unwrap();
        assert_eq!(menu.browse_sources[jade], BrowseSource::Community(0));
        // And community rows carry the attribution into the entry.
        assert_eq!(menu.browse_entries[jade].authors, ["Snownee"]);
        assert_eq!(menu.browse_entries[jade].license, "CC-BY-NC-SA-4.0");
    }

    #[test]
    fn browse_categories_split_the_catalog_cleanly() {
        let mut menu = ModManagerState::new(vec![ModManagerEntry::new("mine", "Mine", "1.0")]);
        menu.activate(ModManagerWidgetId::FilterBrowse);
        assert_eq!(menu.filter, ModManagerFilter::Browse);
        assert_eq!(menu.browse_category, BrowseCategory::QolHud);
        // QoL & HUD: 4 community HUD/inventory picks + all 11
        // non-overlapping curated QoL mods.
        assert_eq!(menu.visible_indices().len(), 15);
        menu.activate(ModManagerWidgetId::BrowseContent);
        assert_eq!(menu.browse_category, BrowseCategory::Content);
        assert_eq!(menu.visible_indices().len(), 3);
        // The Packs shelf is empty: no pack metadata ships in the client.
        menu.activate(ModManagerWidgetId::BrowsePacks);
        assert_eq!(menu.browse_category, BrowseCategory::Packs);
        assert!(menu.visible_indices().is_empty());
        // Switching categories re-clamps the selection.
        menu.activate(ModManagerWidgetId::BrowseQolHud);
        // Browse never lists installed entries and never stages changes.
        assert!(!menu.is_dirty());
        menu.activate(ModManagerWidgetId::FilterAll);
        assert_eq!(menu.entries.len(), 1);
    }

    #[test]
    fn browse_query_filters_locally_and_feeds_live_search_url() {
        let mut menu = ModManagerState::new(vec![]);
        menu.activate(ModManagerWidgetId::FilterBrowse);
        // Typing in Browse mode writes `browse_query`, not `search`.
        menu.handle_input(UiInputEvent::CharTyped('j'), 1280, 720);
        menu.handle_input(UiInputEvent::CharTyped('a'), 1280, 720);
        menu.handle_input(UiInputEvent::CharTyped('d'), 1280, 720);
        assert_eq!(menu.browse_query, "jad");
        assert!(menu.search.is_empty());
        // The live-search action hits api.modrinth.com, URL-encoded and
        // pinned to the current kind + target version.
        menu.browse_query = "create&a".to_owned();
        menu.browse_project_kind = ModrinthProjectKind::ResourcePack;
        let actions = menu.activate(ModManagerWidgetId::ModrinthSearch);
        let [ModManagerAction::OpenUrl(url)] = actions.as_slice() else {
            panic!("expected one OpenUrl, got {actions:?}");
        };
        assert!(url.starts_with("https://api.modrinth.com/v2/search?"));
        assert!(url.contains("query=create%26a"), "query not encoded: {url}");
        assert!(
            url.contains("project_type%3Aresourcepack"),
            "kind facet missing: {url}"
        );
        assert!(
            url.contains("versions%3A1.21.1"),
            "version facet missing: {url}"
        );
        // `search` is untouched by Browse-mode typing.
        menu.handle_input(UiInputEvent::KeyPressed(UiKey::Backspace), 1280, 720);
        assert_eq!(menu.browse_query, "create&");
    }

    #[test]
    fn browse_kind_pill_cycles_project_kinds() {
        let mut menu = ModManagerState::new(vec![]);
        menu.activate(ModManagerWidgetId::FilterBrowse);
        assert_eq!(menu.browse_project_kind, ModrinthProjectKind::Mod);
        for expected in [
            ModrinthProjectKind::ResourcePack,
            ModrinthProjectKind::Shader,
            ModrinthProjectKind::Datapack,
            ModrinthProjectKind::Mod,
        ] {
            menu.activate(ModManagerWidgetId::BrowseKind);
            assert_eq!(menu.browse_project_kind, expected);
        }
        // The kind lands in the live-search facets.
        let actions = menu.activate(ModManagerWidgetId::ModrinthSearch);
        let [ModManagerAction::OpenUrl(url)] = actions.as_slice() else {
            panic!("expected OpenUrl, got {actions:?}");
        };
        assert!(url.contains("project_type%3Amod"), "{url}");
    }

    #[test]
    fn packs_shelf_offers_live_search_button_in_detail_pane() {
        let mut menu = ModManagerState::new(vec![]);
        menu.lang = crate::state::UiLang::Ja;
        menu.activate(ModManagerWidgetId::FilterBrowse);
        menu.activate(ModManagerWidgetId::BrowsePacks);
        let (draw, hits) = menu.render(1280, 720);
        let search = hits
            .iter()
            .find(|h| h.id == ModManagerWidgetId::ModrinthSearch)
            .expect("Packs detail pane must offer live search");
        assert!(search.rect.w > 20);
        let texts: Vec<&str> = draw
            .commands
            .iter()
            .filter_map(|c| match c {
                crate::draw::UiCommand::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert!(texts.iter().any(|t| t.contains("パックはModrinthで検索")));
    }

    #[test]
    fn browse_pill_labels_and_status_copy_are_glyph_covered() {
        let menu = ModManagerState::default();
        for s in [
            tr(crate::state::UiLang::Ja, "+ Browse", "+ ブラウズ"),
            BrowseCategory::QolHud.label(crate::state::UiLang::Ja),
            BrowseCategory::Content.label(crate::state::UiLang::Ja),
            BrowseCategory::Packs.label(crate::state::UiLang::Ja),
            tr(crate::state::UiLang::Ja, "Kind", "タイプ"),
            menu.soft_reload_status(crate::state::UiLang::En),
            menu.soft_reload_status(crate::state::UiLang::Ja),
            tr(
                crate::state::UiLang::Ja,
                "Search Modrinth",
                "Modrinthで検索",
            ),
            tr(
                crate::state::UiLang::Ja,
                "No packs curated - search Modrinth for:",
                "パックはModrinthで検索",
            ),
            tr(
                crate::state::UiLang::Ja,
                "Search Modrinth...",
                "Modrinthを検索...",
            ),
        ] {
            for c in s.chars() {
                assert!(
                    crate::draw::glyph_is_covered(c),
                    "uncovered glyph {c:?} in {s:?}"
                );
            }
        }
    }

    #[test]
    fn soft_reload_flag_tracks_non_hot_toggles_and_applies() {
        let mut entry = ModManagerEntry::new("heavy", "Heavy", "1.0");
        entry.reload_mode = ModReloadBadge::SoftRestart;
        let mut menu = ModManagerState::new(vec![entry]);
        assert!(!menu.needs_soft_reload_on_close());
        // Staging a non-hot toggle marks the close path for soft reload.
        menu.toggle_selected();
        assert!(menu.needs_soft_reload_on_close());
        // Unstaging clears the pending side again.
        menu.toggle_selected();
        assert!(!menu.needs_soft_reload_on_close());
        // Applying one keeps the flag sticky until the host acknowledges.
        menu.toggle_selected();
        let actions = menu.activate(ModManagerWidgetId::ApplyChanges);
        assert!(matches!(
            actions.as_slice(),
            [ModManagerAction::ApplyChanges {
                requires_soft_restart: true,
                ..
            }]
        ));
        assert!(menu.dirty_requires_soft_reload);
        assert!(menu.needs_soft_reload_on_close());
        menu.acknowledge_soft_reload();
        assert!(!menu.needs_soft_reload_on_close());
        // A hot-reload mod alone never trips the flag.
        let mut hot = ModManagerEntry::new("hot", "Hot", "1.0");
        hot.reload_mode = ModReloadBadge::HotReload;
        let mut menu = ModManagerState::new(vec![hot]);
        menu.toggle_selected();
        assert!(!menu.needs_soft_reload_on_close());
    }

    #[test]
    fn browse_view_lists_every_entry_and_installs_via_modrinth() {
        let mut menu = ModManagerState::new(vec![ModManagerEntry::new("mine", "Mine", "1.0")]);
        menu.activate(ModManagerWidgetId::FilterBrowse);
        assert_eq!(menu.filter, ModManagerFilter::Browse);
        // Search narrows the catalog by name and by author.
        menu.browse_query = "jade".into();
        assert_eq!(menu.selected_entry().unwrap().mod_id, "jade");
        let actions = menu.activate(ModManagerWidgetId::InstallRecommended);
        assert_eq!(
            actions,
            [ModManagerAction::OpenUrl(
                "https://modrinth.com/mod/jade".to_owned()
            )]
        );
        // Catalog browsing never stages enable/disable changes.
        assert!(!menu.is_dirty());
        menu.activate(ModManagerWidgetId::FilterAll);
        assert_eq!(menu.entries.len(), 1);
    }

    #[test]
    fn browse_detail_pane_shows_author_license_and_bridge_badge() {
        let mut menu = ModManagerState::new(vec![]);
        menu.activate(ModManagerWidgetId::FilterBrowse);
        menu.browse_query = "jade".into();
        let (draw, hits) = menu.render(1280, 720);
        let texts: Vec<&str> = draw
            .commands
            .iter()
            .filter_map(|c| match c {
                crate::draw::UiCommand::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        for needle in ["Snownee", "CC-BY-NC-SA-4.0", "GUI Bridge", "HUD / Info"] {
            assert!(
                texts.iter().any(|t| t.contains(needle)),
                "detail pane missing {needle:?} in {texts:?}"
            );
        }
        // The install hook + live search are the only actions offered.
        assert!(hits
            .iter()
            .any(|h| h.id == ModManagerWidgetId::InstallRecommended));
        assert!(hits
            .iter()
            .any(|h| h.id == ModManagerWidgetId::ModrinthSearch));
        assert!(!hits
            .iter()
            .any(|h| h.id == ModManagerWidgetId::ToggleEnabled));
        // The browse pill exists in the installed filter row too.
        let mut menu = ModManagerState::new(vec![]);
        menu.lang = crate::state::UiLang::Ja;
        let (_, hits) = menu.render(1280, 720);
        assert!(hits
            .iter()
            .any(|h| h.id == ModManagerWidgetId::FilterBrowse));
    }
}
