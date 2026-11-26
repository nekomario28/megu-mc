// SPDX-License-Identifier: LGPL-3.0-or-later
//! Clean-room native vanilla 1.21.1 GUI sprite atlas, 9-slice
//! tessellator and title panorama loader.
//!
//! When the player's *own* local vanilla client JAR is present, this
//! module reads the 1.21.1 GUI sprites **in memory** through
//! bounded ZIP reads — zero temp files, zero Mojang
//! assets in git — so the renderer can skin buttons, sliders, menu
//! chrome, the title panorama and HUD widgets with authentic textures:
//!
//! * Widgets under `assets/minecraft/textures/gui/sprites/widget/`
//! * Menu chrome under `assets/minecraft/textures/gui/`
//! * HUD sprites under `assets/minecraft/textures/gui/sprites/hud/`
//! * Title panorama faces
//!   `assets/minecraft/textures/gui/title/background/panorama_{0..5}.png`
//!
//! Resource-pack layers are supported exactly like Minecraft: every
//! supplied archive after the client JAR is a later pack in priority
//! order. A valid replacement image uses its own layer's scaling metadata.
//!
//! Everything fails closed: a missing JAR, malformed archive or
//! undecodable PNG is skipped with a note, never a panic, and
//! [`VanillaGuiAssets::has_widget_skin`] / [`VanillaGuiAssets::has_panorama`]
//! report cleanly so callers fall back to Megu's beveled vector UI.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use megu_core::json::{parse_json, JsonValue};

use crate::title_branding::RgbaImage;
use crate::vanilla_jar::{
    check_png_budget, decode_asset_png, prioritized_packs, AssetLayer, JarError,
    MAX_DECODED_RGBA_BYTES, MAX_VISUAL_ATLAS_DIM,
};
use megu_ui::draw::VanillaContainerTextureId;

// ---------------------------------------------------------------------------
// Sprite catalog
// ---------------------------------------------------------------------------

/// One vanilla 1.21.1 GUI texture this module knows how to load.
///
/// `jar_entry` is the exact archive member path inside the client JAR
/// (or an overriding resource pack); `jar_mcmeta_entry` is the same
/// member with the `.mcmeta` suffix that carries `gui.scaling`
/// 9-slice border metadata.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum VanillaGuiSpriteId {
    /// `widget/button.png` — normal button skin.
    Button,
    /// `widget/button_highlighted.png` — hovered/focused button skin.
    ButtonHighlighted,
    /// `widget/button_disabled.png` — greyed-out button skin.
    ButtonDisabled,
    /// `widget/slider.png` — slider trough skin.
    Slider,
    /// `widget/slider_highlighted.png` — hovered slider trough skin.
    SliderHighlighted,
    /// `widget/slider_handle.png` — slider grab handle skin.
    SliderHandle,
    /// `widget/slider_handle_highlighted.png` — hovered grab handle skin.
    SliderHandleHighlighted,
    /// `menu_background.png` — tiled menu backdrop texture.
    MenuBackground,
    /// `header_separator.png` — list header divider strip.
    HeaderSeparator,
    /// `footer_separator.png` — list footer divider strip.
    FooterSeparator,
    /// `hud/hotbar.png` — hotbar frame.
    Hotbar,
    /// `hud/hotbar_selection.png` — selected-slot highlight frame.
    HotbarSelection,
    /// `hud/crosshair.png` — center-screen crosshair.
    Crosshair,
    /// `hud/experience_bar_background.png` — XP bar trough.
    XpBarBg,
    /// `hud/experience_bar_progress.png` — XP bar fill.
    XpBarFill,
    /// `hud/heart/container.png` — empty heart slot.
    HeartContainer,
    /// `hud/heart/full.png` — full heart.
    HeartFull,
    /// `hud/heart/half.png` — half heart.
    HeartHalf,
    /// `hud/food_empty.png` — empty hunger shank.
    FoodEmpty,
    /// `hud/food_full.png` — full hunger shank.
    FoodFull,
    /// `hud/armor_empty.png` — empty armor plate.
    ArmorEmpty,
    /// `hud/armor_full.png` — full armor plate.
    ArmorFull,
    /// `title/background/panorama_0.png` — panorama cube face 0.
    Panorama0,
    /// `title/background/panorama_1.png` — panorama cube face 1.
    Panorama1,
    /// `title/background/panorama_2.png` — panorama cube face 2.
    Panorama2,
    /// `title/background/panorama_3.png` — panorama cube face 3.
    Panorama3,
    /// `title/background/panorama_4.png` — panorama cube face 4.
    Panorama4,
    /// `title/background/panorama_5.png` — panorama cube face 5.
    Panorama5,
    Scroller,
    ScrollerBackground,
}

/// Prefix shared by every catalogued GUI texture entry.
const GUI_TEX_PREFIX: &str = "assets/minecraft/textures/gui/";

/// Every supported Vanilla 1.21.1 container background, kept separate
/// from `ALL_GUI_SPRITES` so its legacy public shape stays unchanged.
pub const ALL_CONTAINER_TEXTURES: [VanillaContainerTextureId; 22] = [
    VanillaContainerTextureId::Inventory,
    VanillaContainerTextureId::CraftingTable,
    VanillaContainerTextureId::Generic54,
    VanillaContainerTextureId::ShulkerBox,
    VanillaContainerTextureId::Dispenser,
    VanillaContainerTextureId::Hopper,
    VanillaContainerTextureId::Furnace,
    VanillaContainerTextureId::BlastFurnace,
    VanillaContainerTextureId::Smoker,
    VanillaContainerTextureId::EnchantingTable,
    VanillaContainerTextureId::Anvil,
    VanillaContainerTextureId::BrewingStand,
    VanillaContainerTextureId::Beacon,
    VanillaContainerTextureId::Grindstone,
    VanillaContainerTextureId::Loom,
    VanillaContainerTextureId::Stonecutter,
    VanillaContainerTextureId::CartographyTable,
    VanillaContainerTextureId::Smithing,
    VanillaContainerTextureId::Villager,
    VanillaContainerTextureId::Crafter,
    VanillaContainerTextureId::Horse,
    VanillaContainerTextureId::Book,
];

/// Workstation and HUD overlay textures loaded from the same local GUI
/// resource tree. Paths are relative to `textures/gui/`.
pub const VANILLA_WORKSTATION_SPRITES: &[&str] = &[
    "sprites/container/furnace/lit_progress.png",
    "sprites/container/furnace/burn_progress.png",
    "sprites/container/blast_furnace/lit_progress.png",
    "sprites/container/blast_furnace/burn_progress.png",
    "sprites/container/smoker/lit_progress.png",
    "sprites/container/smoker/burn_progress.png",
    "sprites/container/brewing_stand/fuel_length.png",
    "sprites/container/brewing_stand/bubbles.png",
    "sprites/container/brewing_stand/brew_progress.png",
    "sprites/container/enchanting_table/enchantment_slot.png",
    "sprites/container/enchanting_table/enchantment_slot_highlighted.png",
    "sprites/container/enchanting_table/enchantment_slot_disabled.png",
    "sprites/container/anvil/text_field.png",
    "sprites/container/anvil/text_field_disabled.png",
    "sprites/container/anvil/error.png",
    "sprites/container/beacon/button.png",
    "sprites/container/beacon/button_highlighted.png",
    "sprites/container/beacon/button_selected.png",
    "sprites/container/beacon/button_disabled.png",
    "sprites/container/beacon/confirm.png",
    "sprites/container/beacon/cancel.png",
    "sprites/container/crafter/disabled_slot.png",
    "sprites/container/crafter/powered_redstone.png",
    "sprites/container/crafter/unpowered_redstone.png",
    "sprites/container/villager/trade_arrow.png",
    "sprites/container/villager/trade_arrow_out_of_stock.png",
    "sprites/container/villager/experience_bar_background.png",
    "sprites/container/villager/experience_bar_current.png",
    "sprites/container/villager/experience_bar_result.png",
    "sprites/hud/heart/absorbing_full.png",
    "sprites/hud/heart/poisoned_full.png",
    "sprites/hud/heart/withered_full.png",
    "sprites/hud/heart/frozen_full.png",
    "sprites/hud/heart/hardcore_full.png",
    "sprites/hud/air.png",
    "sprites/hud/air_bursting.png",
    "sprites/boss_bar/pink_background.png",
    "sprites/boss_bar/pink_progress.png",
    "sprites/boss_bar/blue_background.png",
    "sprites/boss_bar/blue_progress.png",
    "sprites/boss_bar/red_background.png",
    "sprites/boss_bar/red_progress.png",
    "sprites/boss_bar/green_background.png",
    "sprites/boss_bar/green_progress.png",
    "sprites/boss_bar/yellow_background.png",
    "sprites/boss_bar/yellow_progress.png",
    "sprites/boss_bar/purple_background.png",
    "sprites/boss_bar/purple_progress.png",
    "sprites/boss_bar/white_background.png",
    "sprites/boss_bar/white_progress.png",
];

/// Every sprite id in deterministic catalog order — the same order
/// [`VanillaGuiAtlas`] packs shelves, so atlas output is stable.
pub const ALL_GUI_SPRITES: [VanillaGuiSpriteId; 30] = [
    VanillaGuiSpriteId::Button,
    VanillaGuiSpriteId::ButtonHighlighted,
    VanillaGuiSpriteId::ButtonDisabled,
    VanillaGuiSpriteId::Slider,
    VanillaGuiSpriteId::SliderHighlighted,
    VanillaGuiSpriteId::SliderHandle,
    VanillaGuiSpriteId::SliderHandleHighlighted,
    VanillaGuiSpriteId::MenuBackground,
    VanillaGuiSpriteId::HeaderSeparator,
    VanillaGuiSpriteId::FooterSeparator,
    VanillaGuiSpriteId::Hotbar,
    VanillaGuiSpriteId::HotbarSelection,
    VanillaGuiSpriteId::Crosshair,
    VanillaGuiSpriteId::XpBarBg,
    VanillaGuiSpriteId::XpBarFill,
    VanillaGuiSpriteId::HeartContainer,
    VanillaGuiSpriteId::HeartFull,
    VanillaGuiSpriteId::HeartHalf,
    VanillaGuiSpriteId::FoodEmpty,
    VanillaGuiSpriteId::FoodFull,
    VanillaGuiSpriteId::ArmorEmpty,
    VanillaGuiSpriteId::ArmorFull,
    VanillaGuiSpriteId::Panorama0,
    VanillaGuiSpriteId::Panorama1,
    VanillaGuiSpriteId::Panorama2,
    VanillaGuiSpriteId::Panorama3,
    VanillaGuiSpriteId::Panorama4,
    VanillaGuiSpriteId::Panorama5,
    VanillaGuiSpriteId::Scroller,
    VanillaGuiSpriteId::ScrollerBackground,
];

impl VanillaGuiSpriteId {
    /// Exact JAR member name of this sprite's `.png`.
    pub const fn jar_entry(self) -> &'static str {
        match self {
            Self::Scroller => "assets/minecraft/textures/gui/sprites/widget/scroller.png",
            Self::ScrollerBackground => {
                "assets/minecraft/textures/gui/sprites/widget/scroller_background.png"
            }
            Self::Button => "assets/minecraft/textures/gui/sprites/widget/button.png",
            Self::ButtonHighlighted => {
                "assets/minecraft/textures/gui/sprites/widget/button_highlighted.png"
            }
            Self::ButtonDisabled => {
                "assets/minecraft/textures/gui/sprites/widget/button_disabled.png"
            }
            Self::Slider => "assets/minecraft/textures/gui/sprites/widget/slider.png",
            Self::SliderHighlighted => {
                "assets/minecraft/textures/gui/sprites/widget/slider_highlighted.png"
            }
            Self::SliderHandle => "assets/minecraft/textures/gui/sprites/widget/slider_handle.png",
            Self::SliderHandleHighlighted => {
                "assets/minecraft/textures/gui/sprites/widget/slider_handle_highlighted.png"
            }
            Self::MenuBackground => "assets/minecraft/textures/gui/menu_background.png",
            Self::HeaderSeparator => "assets/minecraft/textures/gui/header_separator.png",
            Self::FooterSeparator => "assets/minecraft/textures/gui/footer_separator.png",
            Self::Hotbar => "assets/minecraft/textures/gui/sprites/hud/hotbar.png",
            Self::HotbarSelection => {
                "assets/minecraft/textures/gui/sprites/hud/hotbar_selection.png"
            }
            Self::Crosshair => "assets/minecraft/textures/gui/sprites/hud/crosshair.png",
            Self::XpBarBg => {
                "assets/minecraft/textures/gui/sprites/hud/experience_bar_background.png"
            }
            Self::XpBarFill => {
                "assets/minecraft/textures/gui/sprites/hud/experience_bar_progress.png"
            }
            Self::HeartContainer => "assets/minecraft/textures/gui/sprites/hud/heart/container.png",
            Self::HeartFull => "assets/minecraft/textures/gui/sprites/hud/heart/full.png",
            Self::HeartHalf => "assets/minecraft/textures/gui/sprites/hud/heart/half.png",
            Self::FoodEmpty => "assets/minecraft/textures/gui/sprites/hud/food_empty.png",
            Self::FoodFull => "assets/minecraft/textures/gui/sprites/hud/food_full.png",
            Self::ArmorEmpty => "assets/minecraft/textures/gui/sprites/hud/armor_empty.png",
            Self::ArmorFull => "assets/minecraft/textures/gui/sprites/hud/armor_full.png",
            Self::Panorama0 => "assets/minecraft/textures/gui/title/background/panorama_0.png",
            Self::Panorama1 => "assets/minecraft/textures/gui/title/background/panorama_1.png",
            Self::Panorama2 => "assets/minecraft/textures/gui/title/background/panorama_2.png",
            Self::Panorama3 => "assets/minecraft/textures/gui/title/background/panorama_3.png",
            Self::Panorama4 => "assets/minecraft/textures/gui/title/background/panorama_4.png",
            Self::Panorama5 => "assets/minecraft/textures/gui/title/background/panorama_5.png",
        }
    }

    /// Exact JAR member name of this sprite's `.png.mcmeta` sidecar
    /// (9-slice border metadata). Panorama faces carry no mcmeta in
    /// vanilla, but the path is still well-defined so a resource pack
    /// supplying one is honored uniformly.
    pub fn jar_mcmeta_entry(self) -> String {
        let mut out = String::with_capacity(self.jar_entry().len() + 7);
        out.push_str(self.jar_entry());
        out.push_str(".mcmeta");
        out
    }

    /// `true` for the six title panorama cube faces — they are kept as
    /// separate equal-size faces ([`PanoramaCube`]) rather than packed
    /// into the 2D widget atlas.
    pub const fn is_panorama_face(self) -> bool {
        matches!(
            self,
            Self::Panorama0
                | Self::Panorama1
                | Self::Panorama2
                | Self::Panorama3
                | Self::Panorama4
                | Self::Panorama5
        )
    }

    /// 0..=5 cube face index for panorama sprites, `None` otherwise.
    pub const fn panorama_face(self) -> Option<usize> {
        match self {
            Self::Panorama0 => Some(0),
            Self::Panorama1 => Some(1),
            Self::Panorama2 => Some(2),
            Self::Panorama3 => Some(3),
            Self::Panorama4 => Some(4),
            Self::Panorama5 => Some(5),
            _ => None,
        }
    }

    /// Vanilla-default 9-slice border used when the JAR/pack supplies no
    /// `.mcmeta` `gui.scaling` block: widgets are stretched nine-slice
    /// skins in vanilla, while HUD icons and separators draw at native
    /// size (`None` = plain stretch).
    pub const fn default_nine_slice_border(self) -> Option<NineSliceBorder> {
        match self {
            Self::Button
            | Self::ButtonHighlighted
            | Self::ButtonDisabled
            | Self::Slider
            | Self::SliderHighlighted => {
                // Vanilla 200x20 skins keep the outer 3 px unstretched.
                Some(NineSliceBorder::uniform(3))
            }
            Self::SliderHandle | Self::SliderHandleHighlighted => {
                // Vanilla 8x20 handle skin keeps the outer 2 px unstretched.
                Some(NineSliceBorder::uniform(2))
            }
            Self::Hotbar => Some(NineSliceBorder::uniform(3)),
            _ => None,
        }
    }

    /// The canonical `megu_core::ui_contract::SpriteId` this sprite
    /// skins, when one exists. Only the widget chrome the contract
    /// enumerates maps — HUD sprites, slider hover twins and panorama
    /// faces stay `None` (the tessellator falls back to vector chrome
    /// for unmapped `SpriteId`s).
    #[must_use]
    pub const fn to_ui_contract(self) -> Option<megu_core::ui_contract::SpriteId> {
        use megu_core::ui_contract::SpriteId as S;
        match self {
            Self::Button => Some(S::Button),
            Self::ButtonHighlighted => Some(S::ButtonHighlighted),
            Self::ButtonDisabled => Some(S::ButtonDisabled),
            Self::ScrollerBackground => Some(S::ScrollerBackground),
            Self::Scroller => Some(S::Scroller),
            Self::MenuBackground => Some(S::Panel),
            Self::HeaderSeparator => Some(S::HeaderSeparator),
            Self::FooterSeparator => Some(S::FooterSeparator),
            _ => None,
        }
    }

    /// `true` when `name` is a catalogued GUI texture or its `.mcmeta`
    /// sidecar anywhere under `assets/minecraft/textures/gui/`.
    fn is_catalog_entry(name: &str) -> bool {
        name.starts_with(GUI_TEX_PREFIX)
            && !name.contains("..")
            && (name.ends_with(".png") || name.ends_with(".png.mcmeta"))
    }
}

// ---------------------------------------------------------------------------
// 9-slice border + .mcmeta parsing
// ---------------------------------------------------------------------------

/// Fixed-size unstretched border of a 9-slice sprite, in source pixels.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct NineSliceBorder {
    /// Unstretched left column width in px.
    pub left: u32,
    /// Unstretched top row height in px.
    pub top: u32,
    /// Unstretched right column width in px.
    pub right: u32,
    /// Unstretched bottom row height in px.
    pub bottom: u32,
}

impl NineSliceBorder {
    /// Same border on every side.
    pub const fn uniform(px: u32) -> Self {
        Self {
            left: px,
            top: px,
            right: px,
            bottom: px,
        }
    }

    /// `true` when no side stretches — the sprite draws as one quad.
    pub const fn is_zero(&self) -> bool {
        self.left == 0 && self.top == 0 && self.right == 0 && self.bottom == 0
    }
}

/// Find `"key"` followed (after whitespace) by `:` in `text`; returns
/// the offset just past the colon. This is a forgiving mini-scanner for
/// the tiny `.mcmeta` shape, not a general JSON parser.
enum McmetaBorder {
    Missing,
    Stretch,
    Border(NineSliceBorder),
}

fn parse_nine_slice_border(mcmeta: &[u8]) -> Option<McmetaBorder> {
    let value = parse_json(std::str::from_utf8(mcmeta).ok()?).ok()?;
    let Some(scaling) = value.get("gui").and_then(|gui| gui.get("scaling")) else {
        return Some(McmetaBorder::Missing);
    };
    match scaling.get_str("type")? {
        "stretch" | "tile" => Some(McmetaBorder::Stretch),
        "nine_slice" => {
            let border = scaling.get("border")?;
            let uint = |number: &JsonValue| {
                let n = number.as_f64()?;
                ((0.0..=f64::from(u32::MAX)).contains(&n) && n.fract() == 0.0).then_some(n as u32)
            };
            if let Some(px) = uint(border) {
                return Some(McmetaBorder::Border(NineSliceBorder::uniform(px)));
            }
            border.as_object()?;
            let side = |name| border.get(name).map_or(Some(0), uint);
            Some(McmetaBorder::Border(NineSliceBorder {
                left: side("left")?,
                top: side("top")?,
                right: side("right")?,
                bottom: side("bottom")?,
            }))
        }
        _ => None,
    }
}

const MAX_GUI_SPRITE_BYTES: usize = 32 * 1024 * 1024;
const MAX_MCMETA_BYTES: usize = 64 * 1024;

fn procedural_core_sprites() -> BTreeMap<VanillaGuiSpriteId, RgbaImage> {
    ALL_GUI_SPRITES
        .into_iter()
        .filter(|id| !id.is_panorama_face())
        .map(|id| {
            let (width, height) = match id {
                VanillaGuiSpriteId::Button
                | VanillaGuiSpriteId::ButtonHighlighted
                | VanillaGuiSpriteId::ButtonDisabled
                | VanillaGuiSpriteId::Slider
                | VanillaGuiSpriteId::SliderHighlighted => (32, 16),
                _ => (16, 16),
            };
            let base = match id {
                VanillaGuiSpriteId::HeartFull | VanillaGuiSpriteId::HeartHalf => [190, 42, 48],
                VanillaGuiSpriteId::FoodEmpty | VanillaGuiSpriteId::FoodFull => [181, 96, 35],
                VanillaGuiSpriteId::ArmorEmpty | VanillaGuiSpriteId::ArmorFull => [160, 174, 190],
                VanillaGuiSpriteId::ButtonHighlighted | VanillaGuiSpriteId::SliderHighlighted => {
                    [115, 151, 68]
                }
                VanillaGuiSpriteId::ButtonDisabled => [85, 85, 85],
                VanillaGuiSpriteId::Crosshair => [235, 235, 235],
                _ => [104, 82, 57],
            };
            let mut rgba = vec![0; width as usize * height as usize * 4];
            for y in 0..height {
                for x in 0..width {
                    let edge = x == 0 || y == 0 || x + 1 == width || y + 1 == height;
                    let mark = match id {
                        VanillaGuiSpriteId::HeartFull | VanillaGuiSpriteId::HeartHalf => {
                            ((1..=2).contains(&(x % 8)) || (5..=6).contains(&(x % 8)))
                                && (3..=6).contains(&y)
                                || (2..=5).contains(&(x % 8)) && (6..=10).contains(&y)
                        }
                        VanillaGuiSpriteId::Crosshair => x == width / 2 || y == height / 2,
                        VanillaGuiSpriteId::HotbarSelection => edge || x == 1 || y == 1,
                        _ => true,
                    };
                    if !mark {
                        continue;
                    }
                    let shade = if edge {
                        0.55
                    } else if y < height / 3 {
                        1.2
                    } else {
                        1.0
                    };
                    let at = (y as usize * width as usize + x as usize) * 4;
                    rgba[at] = (base[0] as f32 * shade).min(255.0) as u8;
                    rgba[at + 1] = (base[1] as f32 * shade).min(255.0) as u8;
                    rgba[at + 2] = (base[2] as f32 * shade).min(255.0) as u8;
                    rgba[at + 3] = 255;
                }
            }
            (
                id,
                RgbaImage {
                    width,
                    height,
                    rgba,
                },
            )
        })
        .collect()
}

/// Everything [`load_vanilla_gui_from_jar_bytes`] /
/// [`load_vanilla_gui_from_path`] recovered: decoded sprites keyed by
/// catalog id (panorama faces included), plus parsed `.mcmeta` borders.
#[derive(Clone, Debug, Default)]
pub struct VanillaGuiAssets {
    /// Successfully decoded RGBA sprites; a missing key means the entry
    /// was absent or rejected — the renderer falls back per sprite.
    pub sprites: BTreeMap<VanillaGuiSpriteId, RgbaImage>,
    /// Every decoded PNG below `textures/gui/`, keyed by its resource
    /// path relative to that directory. This includes workstation screens
    /// and the full HUD/container sprite set, including assets added by
    /// later Vanilla versions without requiring a source edit.
    pub named_textures: BTreeMap<String, RgbaImage>,
    /// Clean-room pixel-art images for the typed core sprites. These remain
    /// separate from `sprites`, preserving the meaning of "loaded from JAR".
    pub procedural_fallbacks: BTreeMap<VanillaGuiSpriteId, RgbaImage>,
    /// `.mcmeta`-declared 9-slice borders that survived all layers.
    /// Call [`VanillaGuiAssets::nine_slice_border`] for the effective
    /// border (metadata, else vanilla defaults).
    pub borders: BTreeMap<VanillaGuiSpriteId, NineSliceBorder>,
    /// Sprites whose winning image declares non-nine-slice scaling.
    pub stretch: BTreeSet<VanillaGuiSpriteId>,
    /// The client JAR the sprites came from, when one was read.
    pub source_jar: Option<PathBuf>,
    /// Every skipped/malformed source — a dropped entry can never look
    /// like a clean negative result.
    pub notes: Vec<String>,
}

impl VanillaGuiAssets {
    /// The no-JAR state: downloaded sprites stay absent, while typed core
    /// widgets receive clean-room pixel-art fallback images.
    pub fn fallback() -> Self {
        Self {
            procedural_fallbacks: procedural_core_sprites(),
            ..Self::default()
        }
    }

    /// Resolve a decoded sprite, falling back to clean-room pixel art.
    pub fn sprite_or_fallback(&self, id: VanillaGuiSpriteId) -> Option<&RgbaImage> {
        self.sprites
            .get(&id)
            .or_else(|| self.procedural_fallbacks.get(&id))
    }

    /// Look up any decoded Vanilla GUI texture by a path relative to
    /// `assets/minecraft/textures/gui/`, for example
    /// `container/furnace.png` or `sprites/hud/heart/poisoned_full.png`.
    pub fn named_texture(&self, relative_path: &str) -> Option<&RgbaImage> {
        self.named_textures.get(relative_path)
    }

    /// `true` when the normal-state button skin decoded — the flagship
    /// widget skin that decides "vanilla widgets available". Individual
    /// states/widgets can still be missing; check `sprites` per id.
    pub fn has_widget_skin(&self) -> bool {
        self.sprites.contains_key(&VanillaGuiSpriteId::Button)
    }

    /// `true` when all six panorama faces decoded as equal-size squares
    /// — the cubemap can be uploaded. Anything less falls back to the
    /// procedural title backdrop.
    pub fn has_panorama(&self) -> bool {
        self.panorama_face_size().is_some()
    }

    /// Shared face dimension when all six panorama faces are present,
    /// square and equal-size; `None` otherwise.
    fn panorama_face_size(&self) -> Option<u32> {
        let mut size = None;
        for id in ALL_GUI_SPRITES {
            if id.panorama_face().is_none() {
                continue;
            }
            let img = self.sprites.get(&id)?;
            if img.width == 0 || img.width != img.height {
                return None;
            }
            match size {
                None => size = Some(img.width),
                Some(s) if s != img.width => return None,
                _ => {}
            }
        }
        size
    }

    /// Assemble the validated six-face panorama cube for cubemap
    /// upload, cloning the decoded faces. `None` unless
    /// [`Self::has_panorama`] is `true`.
    pub fn panorama_cube(&self) -> Option<PanoramaCube> {
        let size = self.panorama_face_size()?;
        let faces: [Option<RgbaImage>; 6] = std::array::from_fn(|i| {
            let id = ALL_GUI_SPRITES[22 + i];
            self.sprites.get(&id).cloned()
        });
        let faces = faces.into_iter().collect::<Option<Vec<_>>>()?;
        let faces: [RgbaImage; 6] = faces.try_into().ok()?;
        Some(PanoramaCube { size, faces })
    }

    /// Effective 9-slice border for `id`: `.mcmeta` metadata wins, then
    /// vanilla widget defaults, then `None` (plain stretch sprite).
    pub fn nine_slice_border(&self, id: VanillaGuiSpriteId) -> Option<NineSliceBorder> {
        if self.stretch.contains(&id) {
            return None;
        }
        self.borders
            .get(&id)
            .copied()
            .or_else(|| id.default_nine_slice_border())
    }

    /// Build the GUI atlas used by the window, including container panels.
    pub fn build_atlas(&self) -> VanillaGuiAtlas {
        self.build_full_atlas()
    }

    /// Shelf-pack every decoded GUI texture, including named HUD and
    /// workstation resources, into one atlas.
    pub fn build_full_atlas(&self) -> VanillaGuiTextureAtlas {
        pack_gui_atlas(self, false)
    }

    /// Build a renderable atlas that fills missing typed sprites with the
    /// clean-room pixel-art set while keeping decoded JAR textures preferred.
    pub fn build_atlas_with_fallbacks(&self) -> VanillaGuiTextureAtlas {
        pack_gui_atlas(self, true)
    }
}

/// Six equal-size RGBA8 faces, ready for cubemap upload in face order
/// `0..=5` (the `panorama_{0..5}.png` naming order).
#[derive(Clone, Debug)]
pub struct PanoramaCube {
    /// Edge length shared by every face, in px.
    pub size: u32,
    /// Face images `0..=5`, each `size * size * 4` RGBA bytes.
    pub faces: [RgbaImage; 6],
}

/// Decode one PNG entry into [`RgbaImage`], validating the buffer size
/// against the header exactly like `title_branding` does.
fn decode_sprite_png(bytes: &[u8], remaining: usize) -> Result<RgbaImage, String> {
    check_png_budget(bytes, remaining)?;
    let (width, height, rgba) = decode_asset_png(bytes)?;
    Ok(RgbaImage {
        width,
        height,
        rgba,
    })
}

/// Crop the upper-left Vanilla panel area, scaling crop coordinates for
/// high-DPI resource packs. Only integral, uniform scale factors are
/// accepted; a malformed/truncated sheet fails closed.
fn container_panel_dims(id: VanillaContainerTextureId, image: &RgbaImage) -> Option<(u32, u32)> {
    let (panel_w, panel_h) = id.panel_size();
    let (sheet_w, sheet_h) = if id == VanillaContainerTextureId::Villager {
        (512, 256)
    } else {
        (256, 256)
    };
    let scale = if image.width.is_multiple_of(sheet_w)
        && image.height.is_multiple_of(sheet_h)
        && image.width / sheet_w == image.height / sheet_h
    {
        image.width / sheet_w
    } else if image.width >= panel_w && image.height >= panel_h {
        1
    } else {
        return None;
    };
    if scale == 0 {
        return None;
    }
    let width = panel_w.checked_mul(scale)?;
    let height = panel_h.checked_mul(scale)?;
    if width > image.width || height > image.height {
        return None;
    }
    Some((width, height))
}

fn crop_container_panel(id: VanillaContainerTextureId, image: &RgbaImage) -> Option<RgbaImage> {
    let (width, height) = container_panel_dims(id, image)?;
    let expected = (image.width as usize)
        .checked_mul(image.height as usize)?
        .checked_mul(4)?;
    if image.rgba.len() != expected {
        return None;
    }
    let mut rgba = vec![
        0;
        (width as usize)
            .checked_mul(height as usize)?
            .checked_mul(4)?
    ];
    for row in 0..height as usize {
        let src = row.checked_mul(image.width as usize)?.checked_mul(4)?;
        let dst = row.checked_mul(width as usize)?.checked_mul(4)?;
        let bytes = width as usize * 4;
        rgba[dst..dst + bytes].copy_from_slice(&image.rgba[src..src + bytes]);
    }
    Some(RgbaImage {
        width,
        height,
        rgba,
    })
}

fn fold_layer(
    layer: Result<AssetLayer<'_>, JarError>,
    label: &str,
    out: &mut VanillaGuiAssets,
    remaining: &mut usize,
) {
    let mut layer = match layer {
        Ok(layer) => layer,
        Err(err) => {
            out.notes.push(format!("{label} unreadable: {err}"));
            return;
        }
    };
    let names = layer.names().to_vec();
    for name in names.iter().filter(|name| name.ends_with(".png")) {
        let Some(relative) = name.strip_prefix(GUI_TEX_PREFIX) else {
            continue;
        };
        if out.named_textures.contains_key(relative) {
            continue;
        }
        let typed = ALL_GUI_SPRITES
            .iter()
            .copied()
            .find(|id| id.jar_entry() == name);
        // Keep both public catalogs, but decode an overlapping PNG once.
        let copies = if typed.is_some() { 2 } else { 1 };
        let image = layer
            .read(name, MAX_GUI_SPRITE_BYTES)
            .map_err(|err| err.to_string())
            .and_then(|bytes| decode_sprite_png(&bytes, *remaining / copies));
        let image = match image {
            Ok(image) => image,
            Err(error) => {
                out.notes
                    .push(format!("{label}:{name}: GUI texture rejected: {error}"));
                continue;
            }
        };
        if ALL_CONTAINER_TEXTURES
            .iter()
            .copied()
            .find(|id| id.path() == relative)
            .is_some_and(|id| container_panel_dims(id, &image).is_none())
        {
            out.notes.push(format!(
                "{label}:{name}: container panel dimensions invalid"
            ));
            continue;
        }
        *remaining -= image.rgba.len() * copies;
        if let Some(id) = typed {
            out.sprites.insert(id, image.clone());
            let meta = id.jar_mcmeta_entry();
            if names.binary_search(&meta).is_ok() {
                match layer
                    .read(&meta, MAX_MCMETA_BYTES)
                    .ok()
                    .and_then(|b| parse_nine_slice_border(&b))
                {
                    Some(McmetaBorder::Border(border)) => {
                        out.borders.insert(id, border);
                    }
                    Some(McmetaBorder::Stretch) => {
                        out.stretch.insert(id);
                    }
                    Some(McmetaBorder::Missing) => {}
                    None => out
                        .notes
                        .push(format!("{label}:{meta}: malformed GUI scaling metadata")),
                }
            }
        }
        out.named_textures.insert(relative.to_owned(), image);
    }
}

/// Load the base JAR and then each pack in increasing priority order.
pub fn load_vanilla_gui_from_jar_bytes(
    jar_bytes: &[u8],
    resource_pack_layers: &[&[u8]],
) -> VanillaGuiAssets {
    let wanted = VanillaGuiSpriteId::is_catalog_entry;
    let mut out = VanillaGuiAssets::fallback();
    let mut remaining = MAX_DECODED_RGBA_BYTES;
    for (i, bytes) in prioritized_packs(resource_pack_layers)
        .iter()
        .enumerate()
        .rev()
    {
        fold_layer(
            AssetLayer::from_bytes(bytes, true, wanted),
            &format!("pack[{i}]"),
            &mut out,
            &mut remaining,
        );
    }
    fold_layer(
        AssetLayer::from_bytes(jar_bytes, false, wanted),
        "client.jar",
        &mut out,
        &mut remaining,
    );
    out
}

/// Load GUI textures from a local client JAR and a priority-ordered pack stack.
pub fn load_vanilla_gui_from_path(
    jar_path: &Path,
    resource_pack_paths: &[PathBuf],
) -> VanillaGuiAssets {
    load_gui_stack(Some(jar_path), resource_pack_paths)
}

pub(crate) fn load_gui_stack(jar: Option<&Path>, packs: &[PathBuf]) -> VanillaGuiAssets {
    let wanted = VanillaGuiSpriteId::is_catalog_entry;
    let mut out = VanillaGuiAssets::fallback();
    let mut remaining = MAX_DECODED_RGBA_BYTES;
    for path in prioritized_packs(packs).iter().rev() {
        fold_layer(
            AssetLayer::open(path, true, wanted),
            &path.display().to_string(),
            &mut out,
            &mut remaining,
        );
    }
    if let Some(path) = jar {
        let layer = AssetLayer::open(path, false, wanted);
        if layer.is_ok() {
            out.source_jar = Some(path.to_owned());
        }
        fold_layer(layer, &path.display().to_string(), &mut out, &mut remaining);
    }
    out
}

// ---------------------------------------------------------------------------
// Single-atlas shelf packer
// ---------------------------------------------------------------------------

/// One sprite's placement inside [`VanillaGuiAtlas`]: normalized UVs of
/// the *inner* (un-padded) rect plus its pixel size and effective
/// 9-slice border.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct SpriteAtlasEntry {
    /// `[u_min, v_min]` normalized to atlas dimensions.
    pub uv_min: [f32; 2],
    /// `[u_max, v_max]` normalized to atlas dimensions.
    pub uv_max: [f32; 2],
    /// Sprite pixel width (unpadded).
    pub width: u32,
    /// Sprite pixel height (unpadded).
    pub height: u32,
    /// Effective 9-slice border in source px, when the sprite is
    /// nine-sliced (mcmeta or vanilla widget default).
    pub nine_slice_border: Option<NineSliceBorder>,
}

/// Compatibility name for the complete GUI atlas used by the window.
pub type VanillaGuiAtlas = VanillaGuiTextureAtlas;

/// Full named GUI atlas used for workstation, HUD and other textures that
/// do not have a legacy [`VanillaGuiSpriteId`] entry.
#[derive(Clone, Debug, Default)]
pub struct VanillaGuiTextureAtlas {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
    pub entries: Vec<(VanillaGuiSpriteId, SpriteAtlasEntry)>,
    pub named_entries: Vec<(String, SpriteAtlasEntry)>,
}

impl VanillaGuiTextureAtlas {
    /// Look up one typed sprite's placement.
    pub fn entry(&self, id: VanillaGuiSpriteId) -> Option<&SpriteAtlasEntry> {
        self.entries
            .iter()
            .find(|(key, _)| *key == id)
            .map(|(_, entry)| entry)
    }

    /// Build the UI bridge with both widget sprites and container panels.
    /// Container menu IDs resolve to the matching cropped background UV;
    /// unavailable textures remain absent and keep the caller's fallback.
    #[must_use]
    pub fn to_gui_sprite_atlas(&self) -> megu_ui::draw::GuiSpriteAtlas {
        let entries = self
            .entries
            .iter()
            .filter_map(|(id, entry)| {
                id.to_ui_contract().map(|sprite| {
                    let border = entry.nine_slice_border.unwrap_or_default();
                    (
                        sprite,
                        megu_ui::draw::GuiSpriteEntry {
                            uv_min: entry.uv_min,
                            uv_max: entry.uv_max,
                            width: entry.width,
                            height: entry.height,
                            nine_slice_border: [
                                border.left,
                                border.top,
                                border.right,
                                border.bottom,
                            ],
                        },
                    )
                })
            })
            .collect::<Vec<_>>();
        let container_panels = (0..=24)
            .chain(std::iter::once(u32::MAX))
            .filter_map(|menu_id| {
                let id = VanillaContainerTextureId::for_menu_id(menu_id)?;
                let entry = self
                    .named_entries
                    .iter()
                    .find(|(name, _)| name == id.path())?
                    .1;
                Some((
                    menu_id,
                    megu_ui::draw::GuiSpriteEntry {
                        uv_min: entry.uv_min,
                        uv_max: entry.uv_max,
                        width: entry.width,
                        height: entry.height,
                        nine_slice_border: [0; 4],
                    },
                ))
            })
            .collect();
        megu_ui::draw::GuiSpriteAtlas::from_packed_with_container_panels(
            self.width,
            self.height,
            self.rgba.clone(),
            entries,
            container_panels,
        )
    }
}

/// Smallest atlas edge emitted for an empty sprite set — a valid 1x1
/// transparent texture.
const EMPTY_ATLAS_DIM: u32 = 1;
/// Maximum atlas edge: the PNG decoder's own 4096 px cap bounds every
/// source sprite, and shelf packing never needs more than 8192.
const MAX_ATLAS_DIM: u32 = MAX_VISUAL_ATLAS_DIM;

/// Smallest power of two ≥ `v`, clamped to `[1, MAX_ATLAS_DIM]`.
fn next_pow2_clamped(v: u32) -> u32 {
    v.max(1).next_power_of_two().clamp(1, MAX_ATLAS_DIM)
}

/// Shelf-pack `assets`' non-panorama sprites in catalog order.
fn pack_gui_atlas(assets: &VanillaGuiAssets, include_fallbacks: bool) -> VanillaGuiTextureAtlas {
    let container_crops: BTreeMap<String, RgbaImage> = ALL_CONTAINER_TEXTURES
        .iter()
        .filter_map(|id| {
            let source = assets.named_textures.get(id.path())?;
            Some((id.path().to_owned(), crop_container_panel(*id, source)?))
        })
        .collect();
    let mut sprites: Vec<(Option<VanillaGuiSpriteId>, Option<String>, &RgbaImage)> =
        ALL_GUI_SPRITES
            .iter()
            .filter(|id| !id.is_panorama_face())
            .filter_map(|id| {
                assets
                    .sprites
                    .get(id)
                    .or_else(|| {
                        include_fallbacks
                            .then(|| assets.procedural_fallbacks.get(id))
                            .flatten()
                    })
                    .map(|img| (Some(*id), None, img))
            })
            .collect();
    sprites.extend(assets.named_textures.iter().filter_map(|(name, image)| {
        let full_name = format!("{GUI_TEX_PREFIX}{name}");
        let already_typed = ALL_GUI_SPRITES.iter().any(|id| id.jar_entry() == full_name);
        if already_typed {
            return None;
        }
        let packed_image = container_crops.get(name).unwrap_or(image);
        Some((None, Some(name.clone()), packed_image))
    }));
    if sprites.is_empty() {
        return VanillaGuiTextureAtlas {
            width: EMPTY_ATLAS_DIM,
            height: EMPTY_ATLAS_DIM,
            rgba: vec![0; (EMPTY_ATLAS_DIM * EMPTY_ATLAS_DIM * 4) as usize],
            entries: Vec::new(),
            named_entries: Vec::new(),
        };
    }

    // Cell size includes the 1 px extruded gutter on every side.
    let cell_area: u64 = sprites
        .iter()
        .map(|(_, _, img)| u64::from(img.width + 2) * u64::from(img.height + 2))
        .sum();
    let widest_cell = sprites
        .iter()
        .map(|(_, _, img)| img.width + 2)
        .max()
        .unwrap_or(2);
    let side = (cell_area as f64).sqrt().ceil() as u32;
    let atlas_w = next_pow2_clamped(side.max(widest_cell).max(2));
    let height_limit = MAX_ATLAS_DIM.min((MAX_DECODED_RGBA_BYTES / (atlas_w as usize * 4)) as u32);

    // Shelf-pack: deterministic catalog order, new shelf when a cell
    // would cross the right edge.
    struct Placed<'a> {
        id: Option<VanillaGuiSpriteId>,
        name: Option<String>,
        img: &'a RgbaImage,
        cell_x: u32,
        cell_y: u32,
    }
    let mut placed = Vec::with_capacity(sprites.len());
    let mut x = 0u32;
    let mut y = 0u32;
    let mut shelf_h = 0u32;
    for (id, name, img) in &sprites {
        let cw = img.width + 2;
        let ch = img.height + 2;
        if cw > atlas_w || ch > height_limit {
            continue;
        }
        let (next_x, next_y, next_shelf) = if x + cw > atlas_w {
            (0, y + shelf_h, 0)
        } else {
            (x, y, shelf_h)
        };
        if next_y + next_shelf.max(ch) > height_limit {
            continue;
        }
        (x, y, shelf_h) = (next_x, next_y, next_shelf);
        placed.push(Placed {
            id: *id,
            name: name.clone(),
            img,
            cell_x: x,
            cell_y: y,
        });
        x += cw;
        shelf_h = shelf_h.max(ch);
    }
    let atlas_h = (y + shelf_h).max(EMPTY_ATLAS_DIM);
    let mut rgba = vec![0u8; atlas_w as usize * atlas_h as usize * 4];

    let mut entries = Vec::with_capacity(placed.len());
    let mut named_entries = Vec::with_capacity(placed.len().saturating_sub(ALL_GUI_SPRITES.len()));
    for p in &placed {
        let img = p.img;
        let ix = p.cell_x + 1;
        let iy = p.cell_y + 1;
        // Inner blit.
        for row in 0..img.height {
            let src = (row * img.width * 4) as usize;
            let dst = ((iy + row) * atlas_w + ix) as usize * 4;
            rgba[dst..dst + img.width as usize * 4]
                .copy_from_slice(&img.rgba[src..src + img.width as usize * 4]);
        }
        // Extrude rows: copy the first/last inner row into the gutter.
        for row in [p.cell_y, iy + img.height] {
            let src_row = if row == p.cell_y {
                iy
            } else {
                iy + img.height - 1
            };
            let src = (src_row * atlas_w + ix) as usize * 4;
            let dst = (row * atlas_w + ix) as usize * 4;
            rgba.copy_within(src..src + img.width as usize * 4, dst);
        }
        // Extrude columns across the full cell height (fills corners).
        for row in p.cell_y..p.cell_y + img.height + 2 {
            let base = (row * atlas_w) as usize * 4;
            let left_dst = base + p.cell_x as usize * 4;
            let left_src = base + ix as usize * 4;
            rgba.copy_within(left_src..left_src + 4, left_dst);
            let right_src = base + (ix + img.width - 1) as usize * 4;
            let right_dst = base + (ix + img.width) as usize * 4;
            rgba.copy_within(right_src..right_src + 4, right_dst);
        }
        let aw = atlas_w as f32;
        let ah = atlas_h as f32;
        let entry = SpriteAtlasEntry {
            uv_min: [ix as f32 / aw, iy as f32 / ah],
            uv_max: [(ix + img.width) as f32 / aw, (iy + img.height) as f32 / ah],
            width: img.width,
            height: img.height,
            nine_slice_border: p.id.and_then(|id| assets.nine_slice_border(id)),
        };
        if let Some(id) = p.id {
            entries.push((id, entry));
        } else if let Some(name) = &p.name {
            named_entries.push((name.clone(), entry));
        }
    }

    VanillaGuiTextureAtlas {
        width: atlas_w,
        height: atlas_h,
        rgba,
        entries,
        named_entries,
    }
}

// ---------------------------------------------------------------------------
// Pixel-exact 9-slice tessellator
// ---------------------------------------------------------------------------

/// One textured UI quad: `pos` is `(x, y, w, h)` in logical UI px and
/// `uv` is `(u_min, v_min, u_max, v_max)` — the same quad shape
/// `tessellate_draw_list`/`UI_QUAD_WGSL` consume.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct TexturedUiQuad {
    /// Destination `(x, y, w, h)`.
    pub pos: [f32; 4],
    /// Atlas UV `(u_min, v_min, u_max, v_max)`.
    pub uv: [f32; 4],
}

/// Tessellate `entry`'s sprite into `dest = (x, y, w, h)` as up to nine
/// quads — four fixed-pixel corners, four edges stretched on one axis,
/// and a center stretched on both — so vanilla button corners never
/// distort at any destination size.
///
/// When `w < left + right` or `h < top + bottom` the two borders of
/// that axis are scaled down uniformly (corners keep their full source
/// texel span), so the emitted quads still tile `dest` exactly with no
/// overlap. Degenerate (zero-area) slices are skipped; a sprite with no
/// border emits a single stretched quad. Return order is row-major:
/// top row, middle row, bottom row, left-to-right within each row.
pub fn tessellate_nine_slice(dest: [f32; 4], entry: &SpriteAtlasEntry) -> Vec<TexturedUiQuad> {
    let [x, y, w, h] = dest;
    let Some(b) = entry.nine_slice_border.filter(|b| !b.is_zero()) else {
        return vec![TexturedUiQuad {
            pos: [x, y, w, h],
            uv: [
                entry.uv_min[0],
                entry.uv_min[1],
                entry.uv_max[0],
                entry.uv_max[1],
            ],
        }];
    };

    let src_w = entry.width.max(1) as f32;
    let src_h = entry.height.max(1) as f32;
    let du = entry.uv_max[0] - entry.uv_min[0];
    let dv = entry.uv_max[1] - entry.uv_min[1];

    // Uniformly shrink each axis' borders when the destination cannot
    // fit them; each side keeps its whole source texel span.
    let scale_x = if b.left + b.right > 0 {
        (w / (b.left + b.right) as f32).min(1.0)
    } else {
        1.0
    };
    let scale_y = if b.top + b.bottom > 0 {
        (h / (b.top + b.bottom) as f32).min(1.0)
    } else {
        1.0
    };
    let dl = b.left as f32 * scale_x;
    let dr = b.right as f32 * scale_x;
    let dt = b.top as f32 * scale_y;
    let db = b.bottom as f32 * scale_y;

    // Destination column/row boundaries (always tile `dest` exactly).
    let px = [x, x + dl, x + w - dr, x + w];
    let py = [y, y + dt, y + h - db, y + h];
    // Source UV boundaries split at the *real* border texels.
    let pu = [
        entry.uv_min[0],
        entry.uv_min[0] + du * (b.left as f32 / src_w),
        entry.uv_max[0] - du * (b.right as f32 / src_w),
        entry.uv_max[0],
    ];
    let pv = [
        entry.uv_min[1],
        entry.uv_min[1] + dv * (b.top as f32 / src_h),
        entry.uv_max[1] - dv * (b.bottom as f32 / src_h),
        entry.uv_max[1],
    ];

    let mut out = Vec::with_capacity(9);
    for row in 0..3 {
        let (qy, qh) = (py[row], py[row + 1] - py[row]);
        if qh <= 0.0 {
            continue;
        }
        for col in 0..3 {
            let (qx, qw) = (px[col], px[col + 1] - px[col]);
            if qw <= 0.0 {
                continue;
            }
            out.push(TexturedUiQuad {
                pos: [qx, qy, qw, qh],
                uv: [pu[col], pv[row], pu[col + 1], pv[row + 1]],
            });
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Title panorama camera helper
// ---------------------------------------------------------------------------

/// Gentle panorama yaw speed: one full revolution every 120 s — the
/// slow drift of the vanilla title backdrop.
const PANORAMA_YAW_DEG_PER_SEC: f64 = 3.0;
/// Subtle pitch nod: ±10° peak, ~12.6 s period.
const PANORAMA_PITCH_AMPLITUDE_DEG: f64 = 10.0;
/// Pitch wave angular rate (rad/s).
const PANORAMA_PITCH_RATE: f64 = 0.5;

/// Smooth title-panorama camera angles `(yaw_deg, pitch_deg)`: a slow
/// yaw spin wrapped to `[0, 360)` plus a bounded sinusoidal pitch nod.
/// Finite and bounded for arbitrary `elapsed_secs` — `NaN`/infinite or
/// negative input maps to the `t = 0` pose.
pub fn panorama_yaw_pitch(elapsed_secs: f32) -> (f32, f32) {
    if !elapsed_secs.is_finite() || elapsed_secs < 0.0 {
        return (0.0, 0.0);
    }
    let t = f64::from(elapsed_secs);
    let yaw = (t * PANORAMA_YAW_DEG_PER_SEC).rem_euclid(360.0) as f32;
    let pitch = ((t * PANORAMA_PITCH_RATE).sin() * PANORAMA_PITCH_AMPLITUDE_DEG) as f32;
    (yaw, pitch)
}
