// SPDX-License-Identifier: LGPL-3.0-or-later
//! Clean-room native vanilla 1.21.1 GUI sprite atlas, 9-slice
//! tessellator and title panorama loader.
//!
//! When the player's *own* local vanilla client JAR is present, this
//! module reads the 1.21.1 GUI sprites **in memory** through
//! [`ZipSliceIndex`]/[`LazyZipIndex`] — zero temp files, zero Mojang
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
//! order, so a later pack's entry silently overrides earlier ones —
//! including a pack that replaces a sprite's `.png` but leaves an
//! earlier layer's `.png.mcmeta` 9-slice metadata in force.
//!
//! Everything fails closed: a missing JAR, malformed archive or
//! undecodable PNG is skipped with a note, never a panic, and
//! [`VanillaGuiAssets::has_widget_skin`] / [`VanillaGuiAssets::has_panorama`]
//! report cleanly so callers fall back to Megu's beveled vector UI.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use megu_client_mesh::png_decode;
use megu_core::zip_archive::{LazyZipIndex, ZipError, ZipSliceIndex};

use crate::title_branding::RgbaImage;

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
}

/// Prefix shared by every catalogued GUI texture entry.
const GUI_TEX_PREFIX: &str = "assets/minecraft/textures/gui/";

/// Every sprite id in deterministic catalog order — the same order
/// [`VanillaGuiAtlas`] packs shelves, so atlas output is stable.
pub const ALL_GUI_SPRITES: [VanillaGuiSpriteId; 28] = [
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
];

impl VanillaGuiSpriteId {
    /// Exact JAR member name of this sprite's `.png`.
    pub const fn jar_entry(self) -> &'static str {
        match self {
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
            Self::SliderHandle => {
                "assets/minecraft/textures/gui/sprites/widget/slider_handle.png"
            }
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
            Self::HeartContainer => {
                "assets/minecraft/textures/gui/sprites/hud/heart/container.png"
            }
            Self::HeartFull => "assets/minecraft/textures/gui/sprites/hud/heart/full.png",
            Self::HeartHalf => "assets/minecraft/textures/gui/sprites/hud/heart/half.png",
            Self::FoodEmpty => "assets/minecraft/textures/gui/sprites/hud/food_empty.png",
            Self::FoodFull => "assets/minecraft/textures/gui/sprites/hud/food_full.png",
            Self::ArmorEmpty => "assets/minecraft/textures/gui/sprites/hud/armor_empty.png",
            Self::ArmorFull => "assets/minecraft/textures/gui/sprites/hud/armor_full.png",
            Self::Panorama0 => {
                "assets/minecraft/textures/gui/title/background/panorama_0.png"
            }
            Self::Panorama1 => {
                "assets/minecraft/textures/gui/title/background/panorama_1.png"
            }
            Self::Panorama2 => {
                "assets/minecraft/textures/gui/title/background/panorama_2.png"
            }
            Self::Panorama3 => {
                "assets/minecraft/textures/gui/title/background/panorama_3.png"
            }
            Self::Panorama4 => {
                "assets/minecraft/textures/gui/title/background/panorama_4.png"
            }
            Self::Panorama5 => {
                "assets/minecraft/textures/gui/title/background/panorama_5.png"
            }
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
            // The slider trough skins the scrollbar track; the handle
            // skins the thumb — the contract's closest widgets.
            Self::Slider => Some(S::ScrollerBackground),
            Self::SliderHandle => Some(S::Scroller),
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
fn json_key_value_pos(text: &str, key: &str, from: usize) -> Option<usize> {
    let needle = format!("\"{key}\"");
    let mut at = from;
    while let Some(rel) = text[at..].find(&needle) {
        let after = at + rel + needle.len();
        let rest = text[after..].trim_start();
        if rest.starts_with(':') {
            return Some(after + (text[after..].len() - rest.len()) + 1);
        }
        at = after;
    }
    None
}

/// Parse a non-negative integer starting at `pos` (whitespace skipped);
/// returns `(value, end_pos)`.
fn parse_uint(text: &str, pos: usize) -> Option<(u32, usize)> {
    let s = text[pos..].trim_start();
    let start = pos + (text[pos..].len() - s.len());
    let digits: usize = s.bytes().take_while(|b| b.is_ascii_digit()).count();
    if digits == 0 {
        return None;
    }
    let value = s[..digits].parse::<u32>().ok()?;
    Some((value, start + digits))
}

/// Outcome of parsing one `.png.mcmeta` sidecar.
enum McmetaBorder {
    /// Valid mcmeta whose `gui.scaling` is *not* `nine_slice`
    /// (`stretch`/`tile`/absent): the sprite stretches whole, and this
    /// layer's mcmeta clears any earlier layer's border — file-level
    /// override semantics.
    NoBorder,
    /// `nine_slice` scaling with a parseable `border`.
    Border(NineSliceBorder),
    /// Malformed: not UTF-8, or `nine_slice` declared but `border`
    /// missing/unparseable. Reported in `notes`; earlier state kept.
    Invalid,
}

/// Parse a `.mcmeta` `gui.scaling` block.
///
/// Accepts the two vanilla `border` spellings — `"border": 3` and
/// `"border": {"left": 3, "top": 3, "right": 3, "bottom": 3}` (object
/// members in any order) — only when the block declares a
/// `"nine_slice"` scaling type. Missing object sides default to `0`.
fn parse_nine_slice_border(mcmeta: &[u8]) -> McmetaBorder {
    let Some(text) = std::str::from_utf8(mcmeta).ok() else {
        return McmetaBorder::Invalid;
    };
    if !text.contains("nine_slice") {
        return McmetaBorder::NoBorder;
    }
    let Some(pos) = json_key_value_pos(text, "border", 0) else {
        return McmetaBorder::Invalid;
    };
    let rest = text[pos..].trim_start();
    if rest.starts_with('{') {
        let Some(end) = rest.find('}') else {
            return McmetaBorder::Invalid;
        };
        let inner = &rest[1..end];
        let mut b = NineSliceBorder::default();
        for (key, field) in [
            ("left", &mut b.left),
            ("top", &mut b.top),
            ("right", &mut b.right),
            ("bottom", &mut b.bottom),
        ] {
            if let Some(vpos) = json_key_value_pos(inner, key, 0) {
                if let Some((v, _)) = parse_uint(inner, vpos) {
                    *field = v;
                }
            }
        }
        McmetaBorder::Border(b)
    } else {
        match parse_uint(text, pos) {
            Some((v, _)) => McmetaBorder::Border(NineSliceBorder::uniform(v)),
            None => McmetaBorder::Invalid,
        }
    }
}

// ---------------------------------------------------------------------------
// Decoded assets + fail-closed loaders
// ---------------------------------------------------------------------------

/// Per-entry materialization cap for GUI sprites (a hostile pack cannot
/// stall boot; the PNG decoder additionally caps at 4096 px).
const MAX_GUI_SPRITE_BYTES: usize = 32 * 1024 * 1024;
/// `.mcmeta` sidecar cap — vanilla blocks are a few hundred bytes.
const MAX_MCMETA_BYTES: usize = 64 * 1024;
/// Resource-pack archives honored per load, in priority order.
const MAX_RESOURCE_PACKS: usize = 64;

/// Everything [`load_vanilla_gui_from_jar_bytes`] /
/// [`load_vanilla_gui_from_path`] recovered: decoded sprites keyed by
/// catalog id (panorama faces included), plus parsed `.mcmeta` borders.
#[derive(Clone, Debug, Default)]
pub struct VanillaGuiAssets {
    /// Successfully decoded RGBA sprites; a missing key means the entry
    /// was absent or rejected — the renderer falls back per sprite.
    pub sprites: BTreeMap<VanillaGuiSpriteId, RgbaImage>,
    /// `.mcmeta`-declared 9-slice borders that survived all layers.
    /// Call [`VanillaGuiAssets::nine_slice_border`] for the effective
    /// border (metadata, else vanilla defaults).
    pub borders: BTreeMap<VanillaGuiSpriteId, NineSliceBorder>,
    /// The client JAR the sprites came from, when one was read.
    pub source_jar: Option<PathBuf>,
    /// Every skipped/malformed source — a dropped entry can never look
    /// like a clean negative result.
    pub notes: Vec<String>,
}

impl VanillaGuiAssets {
    /// The no-JAR state: no sprites, so every accessor reports "absent"
    /// and the renderer keeps Megu's beveled vector UI.
    pub fn fallback() -> Self {
        Self::default()
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
        self.borders
            .get(&id)
            .copied()
            .or_else(|| id.default_nine_slice_border())
    }

    /// Shelf-pack every decoded non-panorama sprite into one RGBA8
    /// atlas with 1 px extruded padding — see [`VanillaGuiAtlas`].
    pub fn build_atlas(&self) -> VanillaGuiAtlas {
        pack_gui_atlas(self)
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
fn decode_sprite_png(bytes: &[u8]) -> Result<RgbaImage, String> {
    let (width, height, rgba) =
        png_decode::decode_png_rgba(bytes).map_err(|e| e.to_string())?;
    let expected = width as usize * height as usize * 4;
    if rgba.len() != expected {
        return Err(format!(
            "decoded {width}x{height} PNG gave {} bytes, expected {expected}",
            rgba.len()
        ));
    }
    Ok(RgbaImage {
        width,
        height,
        rgba,
    })
}

/// Fold one archive layer into `out`: for each catalogued sprite, a
/// present-and-decodable `.png` overwrites any earlier layer's copy,
/// and a parseable `.png.mcmeta` `nine_slice.border` overwrites any
/// earlier border — exactly Minecraft's later-pack-wins precedence.
/// `read` fetches one member by name (fail-closed) and `layer` labels
/// notes.
fn fold_archive_layer<F>(
    read: &mut F,
    layer: &str,
    out: &mut VanillaGuiAssets,
) where
    F: FnMut(&str, usize) -> Result<Vec<u8>, ZipError>,
{
    for id in ALL_GUI_SPRITES {
        match read(id.jar_entry(), MAX_GUI_SPRITE_BYTES) {
            Ok(bytes) => match decode_sprite_png(&bytes) {
                Ok(img) => {
                    out.sprites.insert(id, img);
                }
                Err(reason) => out.notes.push(format!(
                    "{layer}:{}: sprite PNG rejected: {reason}",
                    id.jar_entry()
                )),
            },
            Err(ZipError::EntryNotFound(_)) => {}
            Err(err) => out.notes.push(format!(
                "{layer}:{}: sprite entry unreadable: {err}",
                id.jar_entry()
            )),
        }
        let mcmeta_name = id.jar_mcmeta_entry();
        match read(&mcmeta_name, MAX_MCMETA_BYTES) {
            Ok(bytes) => match parse_nine_slice_border(&bytes) {
                McmetaBorder::Border(border) => {
                    out.borders.insert(id, border);
                }
                McmetaBorder::NoBorder => {
                    out.borders.remove(&id);
                }
                McmetaBorder::Invalid => out.notes.push(format!(
                    "{layer}:{mcmeta_name}: malformed nine_slice metadata"
                )),
            },
            Err(ZipError::EntryNotFound(_)) => {}
            Err(err) => out
                .notes
                .push(format!("{layer}:{mcmeta_name}: mcmeta unreadable: {err}")),
        }
    }
}

/// Load the vanilla GUI sprite set from in-memory archive bytes.
///
/// `jar_bytes` is the player's own local client JAR image;
/// `resource_pack_layers` are resource-pack ZIP images in ascending
/// priority order — every later archive's entries silently override
/// earlier ones, and all packs override the base JAR (Minecraft
/// semantics). Nothing touches disk; nothing is extracted.
///
/// Fail-closed: `None`-equivalent pieces are simply absent from
/// [`VanillaGuiAssets::sprites`]; `notes` records every rejection.
pub fn load_vanilla_gui_from_jar_bytes(
    jar_bytes: &[u8],
    resource_pack_layers: &[&[u8]],
) -> VanillaGuiAssets {
    let mut out = VanillaGuiAssets::fallback();
    match ZipSliceIndex::open_filtered(jar_bytes, VanillaGuiSpriteId::is_catalog_entry) {
        Ok(index) => {
            let mut read = |name: &str, cap: usize| index.read_entry(name, cap);
            fold_archive_layer(&mut read, "client.jar", &mut out);
        }
        Err(err) => out
            .notes
            .push(format!("client JAR bytes unreadable: {err}; using built-in UI")),
    }
    for (pack_no, pack) in resource_pack_layers
        .iter()
        .take(MAX_RESOURCE_PACKS)
        .enumerate()
    {
        match ZipSliceIndex::open_filtered(pack, VanillaGuiSpriteId::is_catalog_entry) {
            Ok(index) => {
                let mut read = |name: &str, cap: usize| index.read_entry(name, cap);
                fold_archive_layer(&mut read, &format!("pack[{pack_no}]"), &mut out);
            }
            Err(err) => out
                .notes
                .push(format!("pack[{pack_no}] unreadable: {err}; layer skipped")),
        }
    }
    out
}

/// Load the vanilla GUI sprite set from a client JAR path plus
/// resource-pack paths — the on-disk twin of
/// [`load_vanilla_gui_from_jar_bytes`], indexed lazily through
/// [`LazyZipIndex`] so only the EOCD tail, Central Directory and
/// selected GUI members are read. Later paths override earlier ones.
pub fn load_vanilla_gui_from_path(
    jar_path: &Path,
    resource_pack_paths: &[PathBuf],
) -> VanillaGuiAssets {
    let mut out = VanillaGuiAssets::fallback();
    if !jar_path.is_file() {
        out.notes.push(format!(
            "client JAR {} missing; using built-in UI",
            jar_path.display()
        ));
    } else {
        out.source_jar = Some(jar_path.to_path_buf());
        match LazyZipIndex::open_filtered(jar_path, VanillaGuiSpriteId::is_catalog_entry) {
            Ok(index) => {
                let mut read = |name: &str, cap: usize| index.read_entry(name, cap);
                fold_archive_layer(&mut read, "client.jar", &mut out);
            }
            Err(err) => out.notes.push(format!(
                "client JAR {} unreadable: {err}; using built-in UI",
                jar_path.display()
            )),
        }
    }
    for pack in resource_pack_paths.iter().take(MAX_RESOURCE_PACKS) {
        if !pack.is_file() {
            continue;
        }
        match LazyZipIndex::open_filtered(pack, VanillaGuiSpriteId::is_catalog_entry) {
            Ok(index) => {
                let mut read = |name: &str, cap: usize| index.read_entry(name, cap);
                fold_archive_layer(&mut read, &pack.display().to_string(), &mut out);
            }
            Err(err) => out.notes.push(format!(
                "pack {} unreadable: {err}; layer skipped",
                pack.display()
            )),
        }
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

/// One RGBA8 atlas texture holding every decoded non-panorama sprite:
/// power-of-two width, tight height, and a 1 px extruded (edge-pixel
/// duplicated) gutter around each sprite so bilinear filtering can
/// never bleed across cells.
#[derive(Clone, Debug, Default)]
pub struct VanillaGuiAtlas {
    /// Atlas width in px (power of two, ≥ 1).
    pub width: u32,
    /// Atlas height in px (tight — exactly the used shelf space, ≥ 1).
    pub height: u32,
    /// `width * height * 4` RGBA8 bytes.
    pub rgba: Vec<u8>,
    /// Sprite placements in catalog order.
    pub entries: Vec<(VanillaGuiSpriteId, SpriteAtlasEntry)>,
}

/// Smallest atlas edge emitted for an empty sprite set — a valid 1x1
/// transparent texture.
const EMPTY_ATLAS_DIM: u32 = 1;
/// Maximum atlas edge: the PNG decoder's own 4096 px cap bounds every
/// source sprite, and shelf packing never needs more than 8192.
const MAX_ATLAS_DIM: u32 = 8192;

impl VanillaGuiAtlas {
    /// Look up one sprite's placement.
    pub fn entry(&self, id: VanillaGuiSpriteId) -> Option<&SpriteAtlasEntry> {
        self.entries
            .iter()
            .find(|(k, _)| *k == id)
            .map(|(_, e)| e)
    }

    /// Re-key the packed atlas to [`megu_ui::draw::GuiSpriteAtlas`] — the
    /// `SpriteId` table `client-ui`'s tessellator and `vanilla_widgets`
    /// resolve [`UiCommand::Sprite`](megu_ui::draw::UiCommand::Sprite)
    /// against. Widget sprites without a contract counterpart (HUD
    /// pieces, highlighted slider twins, panorama faces) are dropped —
    /// their `SpriteId`s keep the vector-chrome fallback.
    ///
    /// The RGBA moves across unchanged, so the texture
    /// [`GpuRenderer::upload_gui_sprite_atlas`] uploads is byte-for-byte
    /// the atlas these UVs were normalized against.
    #[must_use]
    pub fn to_gui_sprite_atlas(&self) -> megu_ui::draw::GuiSpriteAtlas {
        let entries = self
            .entries
            .iter()
            .filter_map(|(id, e)| {
                id.to_ui_contract().map(|sprite| {
                    let border = e.nine_slice_border.unwrap_or_default();
                    (
                        sprite,
                        megu_ui::draw::GuiSpriteEntry {
                            uv_min: e.uv_min,
                            uv_max: e.uv_max,
                            width: e.width,
                            height: e.height,
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
            .collect();
        megu_ui::draw::GuiSpriteAtlas::from_packed(
            self.width,
            self.height,
            self.rgba.clone(),
            entries,
        )
    }
}

/// Smallest power of two ≥ `v`, clamped to `[1, MAX_ATLAS_DIM]`.
fn next_pow2_clamped(v: u32) -> u32 {
    v.max(1).next_power_of_two().clamp(1, MAX_ATLAS_DIM)
}

/// Shelf-pack `assets`' non-panorama sprites in catalog order.
fn pack_gui_atlas(assets: &VanillaGuiAssets) -> VanillaGuiAtlas {
    let sprites: Vec<(VanillaGuiSpriteId, &RgbaImage)> = ALL_GUI_SPRITES
        .iter()
        .filter(|id| !id.is_panorama_face())
        .filter_map(|id| assets.sprites.get(id).map(|img| (*id, img)))
        .collect();
    if sprites.is_empty() {
        return VanillaGuiAtlas {
            width: EMPTY_ATLAS_DIM,
            height: EMPTY_ATLAS_DIM,
            rgba: vec![0; (EMPTY_ATLAS_DIM * EMPTY_ATLAS_DIM * 4) as usize],
            entries: Vec::new(),
        };
    }

    // Cell size includes the 1 px extruded gutter on every side.
    let cell_area: u64 = sprites
        .iter()
        .map(|(_, img)| u64::from(img.width + 2) * u64::from(img.height + 2))
        .sum();
    let widest_cell = sprites
        .iter()
        .map(|(_, img)| img.width + 2)
        .max()
        .unwrap_or(2);
    let side = (cell_area as f64).sqrt().ceil() as u32;
    let atlas_w = next_pow2_clamped(side.max(widest_cell).max(2));

    // Shelf-pack: deterministic catalog order, new shelf when a cell
    // would cross the right edge.
    struct Placed<'a> {
        id: VanillaGuiSpriteId,
        img: &'a RgbaImage,
        cell_x: u32,
        cell_y: u32,
    }
    let mut placed = Vec::with_capacity(sprites.len());
    let mut x = 0u32;
    let mut y = 0u32;
    let mut shelf_h = 0u32;
    for (id, img) in &sprites {
        let cw = img.width + 2;
        let ch = img.height + 2;
        if x > 0 && x + cw > atlas_w {
            y += shelf_h;
            x = 0;
            shelf_h = 0;
        }
        placed.push(Placed {
            id: *id,
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
            let src_row = if row == p.cell_y { iy } else { iy + img.height - 1 };
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
        entries.push((
            p.id,
            SpriteAtlasEntry {
                uv_min: [ix as f32 / aw, iy as f32 / ah],
                uv_max: [(ix + img.width) as f32 / aw, (iy + img.height) as f32 / ah],
                width: img.width,
                height: img.height,
                nine_slice_border: assets.nine_slice_border(p.id),
            },
        ));
    }

    VanillaGuiAtlas {
        width: atlas_w,
        height: atlas_h,
        rgba,
        entries,
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
pub fn tessellate_nine_slice(
    dest: [f32; 4],
    entry: &SpriteAtlasEntry,
) -> Vec<TexturedUiQuad> {
    let [x, y, w, h] = dest;
    let Some(b) = entry.nine_slice_border.filter(|b| !b.is_zero()) else {
        return vec![TexturedUiQuad {
            pos: [x, y, w, h],
            uv: [entry.uv_min[0], entry.uv_min[1], entry.uv_max[0], entry.uv_max[1]],
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
