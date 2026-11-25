// SPDX-License-Identifier: LGPL-3.0-or-later
#![forbid(unsafe_code)]
//! `megu_core::ui_contract` — the canonical Vanilla 1.21.1 theme tokens
//! and the declarative UI contract every renderer and mod surface
//! shares.
//!
//! This module lives in `megu-core` — pure `std`, zero GPU dependency —
//! so `client-ui`, `client-gpu`, `mod-adapter-core` and the WASM mod
//! runtime/SDK all compile against *one* visual language and *one*
//! extension contract instead of each growing a private look:
//!
//! * [`Rgba8`] + [`VANILLA_1_21_1`] — the authoritative Minecraft 1.21.1
//!   color/metric token table: the stone-bevel palette, recessed slot
//!   colors, the classic tooltip gradient, the exact text/shadow
//!   colors and the authentic widget metrics. When vanilla assets are
//!   present `client-gpu` draws the real 9-slice sprites addressed by
//!   [`SpriteId`]; on a clean-room install it pixel-matches these
//!   tokens.
//! * [`compute_gui_scale`] + [`snap_px`] — the vanilla
//!   `Window.calculateScale` integer-scale rule and the logical →
//!   device pixel snap, so every surface lands on the same pixel grid.
//! * [`UiExtensionSlot`], [`UiWidgetKind`], [`UiWidgetSpec`],
//!   [`ModUiManifest`] — the declarative tree both vanilla screens and
//!   mod surfaces (Fabric/NeoForge/Create/WASM) emit. Bounds
//!   ([`MAX_WIDGETS_PER_MANIFEST`], [`MAX_LABEL_LEN`], …) are part of
//!   the contract, enforced by [`ModUiManifest::validate`] and reported
//!   whole by [`audit_manifest`]: a rogue mod can never freeze the UI
//!   or exhaust its memory.
//!
//! **Architectural invariant — title screen purity.** There is
//! deliberately *no* title-screen extension slot: the Vanilla 1.21.1
//! title screen ships exactly as authored, and no manifest can request
//! space on it. [`UiExtensionSlot`] has no `TitleScreenOverlay`
//! variant and [`UiExtensionSlot::overlays_title_screen`] answers
//! `false` for every variant — the purity guarantee is a type-level
//! fact, not a policy a validator could forget to check.

use std::error::Error;
use std::fmt;

/// 8-bit-per-channel RGBA color — the single pixel currency of the UI
/// contract, matching vanilla's packed ARGB quads and every `client-*`
/// vertex format.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Rgba8 {
    /// Red channel.
    pub r: u8,
    /// Green channel.
    pub g: u8,
    /// Blue channel.
    pub b: u8,
    /// Alpha channel (`255` opaque).
    pub a: u8,
}

impl Rgba8 {
    /// Fully transparent black.
    pub const TRANSPARENT: Self = Self::rgba(0, 0, 0, 0);
    /// Opaque black.
    pub const BLACK: Self = Self::rgb(0, 0, 0);
    /// Opaque white.
    pub const WHITE: Self = Self::rgb(255, 255, 255);

    /// Opaque color.
    #[inline]
    #[must_use]
    pub const fn rgb(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b, a: 255 }
    }

    /// Color with explicit alpha.
    #[inline]
    #[must_use]
    pub const fn rgba(r: u8, g: u8, b: u8, a: u8) -> Self {
        Self { r, g, b, a }
    }

    /// Same RGB with a different alpha.
    #[inline]
    #[must_use]
    pub const fn with_alpha(self, a: u8) -> Self {
        Self { a, ..self }
    }

    /// Normalized `[r, g, b, a]` floats in `0.0..=1.0` — the vertex /
    /// shader form.
    #[inline]
    #[must_use]
    pub fn to_f32(self) -> [f32; 4] {
        [
            f32::from(self.r) / 255.0,
            f32::from(self.g) / 255.0,
            f32::from(self.b) / 255.0,
            f32::from(self.a) / 255.0,
        ]
    }
}

/// Canonical 9-slice sprite keys for the 1.21.1 widget chrome.
///
/// These are *keys*, not file paths: `client-gpu` resolves each into
/// the real vanilla sprite (`textures/gui/sprites/…`) when vanilla
/// assets are installed, and into pixel-identical stone-bevel geometry
/// from [`VANILLA_1_21_1`] on a clean-room install. Mod manifests may
/// only name members of this enum — never arbitrary texture paths — so
/// the renderer's sprite atlas stays fully enumerable.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum SpriteId {
    /// `widget/button` — normal stone button.
    Button,
    /// `widget/button_highlighted` — hovered/focused button.
    ButtonHighlighted,
    /// `widget/button_disabled` — greyed button.
    ButtonDisabled,
    /// `container/slot` — recessed 18×18 inventory slot.
    Slot,
    /// Generic beveled panel — clean-room key; vanilla has no
    /// single-sprite panel and tiles this chrome per screen.
    Panel,
    /// `widget/scroller` — scrollbar thumb.
    Scroller,
    /// `widget/scroller_background` — scrollbar track.
    ScrollerBackground,
    /// `widget/text_field` — edit box.
    TextField,
    /// `widget/text_field_highlighted` — focused edit box.
    TextFieldHighlighted,
    /// `tooltip/background` — tooltip body fill.
    TooltipBackground,
    /// `tooltip/frame` — tooltip gradient border.
    TooltipFrame,
    /// Selected tab chrome (e.g. Create-a-world tabs).
    TabSelected,
    /// Unselected tab chrome.
    TabUnselected,
    /// `header_separator` — 2px rule under a screen's header.
    HeaderSeparator,
    /// `footer_separator` — 2px rule above a screen's footer.
    FooterSeparator,
}

impl SpriteId {
    /// All sprite keys, in declaration order.
    pub const ALL: [Self; Self::COUNT] = [
        Self::Button,
        Self::ButtonHighlighted,
        Self::ButtonDisabled,
        Self::Slot,
        Self::Panel,
        Self::Scroller,
        Self::ScrollerBackground,
        Self::TextField,
        Self::TextFieldHighlighted,
        Self::TooltipBackground,
        Self::TooltipFrame,
        Self::TabSelected,
        Self::TabUnselected,
        Self::HeaderSeparator,
        Self::FooterSeparator,
    ];

    /// Number of canonical sprite keys.
    pub const COUNT: usize = 15;

    /// Stable snake_case key — the spelling manifests and the sprite
    /// atlas use.
    #[inline]
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Button => "button",
            Self::ButtonHighlighted => "button_highlighted",
            Self::ButtonDisabled => "button_disabled",
            Self::Slot => "slot",
            Self::Panel => "panel",
            Self::Scroller => "scroller",
            Self::ScrollerBackground => "scroller_background",
            Self::TextField => "text_field",
            Self::TextFieldHighlighted => "text_field_highlighted",
            Self::TooltipBackground => "tooltip_background",
            Self::TooltipFrame => "tooltip_frame",
            Self::TabSelected => "tab_selected",
            Self::TabUnselected => "tab_unselected",
            Self::HeaderSeparator => "header_separator",
            Self::FooterSeparator => "footer_separator",
        }
    }
}

impl fmt::Display for SpriteId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One authoritative Minecraft 1.21.1 visual token table.
///
/// Field names describe *roles*, not widgets, so `client-ui` (vanilla
/// screens), `mod-adapter-core` (mod screens) and the WASM SDK all
/// paint the same chrome: a Fabric config panel drawn with
/// `button_face`/`button_bevel_*` is indistinguishable from the vanilla
/// options list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VanillaThemeTokens {
    /// Panel face — the flat stone-gray field (`#C6C6C6`).
    pub panel_face: Rgba8,
    /// Panel top/left bevel — lit edge (`#FFFFFF`).
    pub panel_bevel_light: Rgba8,
    /// Panel bottom/right bevel — shaded edge (`#555555`).
    pub panel_bevel_dark: Rgba8,
    /// 1px crisp panel outline (`#000000`).
    pub panel_outline: Rgba8,

    /// Stone button face, normal state (`#6F6F6F`).
    pub button_face: Rgba8,
    /// Button top/left bevel (`#A6A6A6`).
    pub button_bevel_light: Rgba8,
    /// Button bottom/right bevel (`#484848`).
    pub button_bevel_dark: Rgba8,
    /// Clean-room hover face (`#7E88A8`) — the muted blue-grey tint
    /// used when no `widget/button_highlighted` sprite is installed.
    /// With vanilla assets the highlight is a sprite, not a recolor.
    pub button_hover_face: Rgba8,
    /// Crisp `#FFFFFF` 1px highlight border vanilla draws around a
    /// focused button's label region.
    pub button_highlight_border: Rgba8,
    /// Disabled button face (`#3A3A3A`).
    pub button_disabled_face: Rgba8,
    /// Disabled button label (`#A0A0A0`).
    pub button_disabled_text: Rgba8,

    /// Recessed inventory slot face (`#8B8B8B`).
    pub slot_face: Rgba8,
    /// Slot inner top/left bevel — the recessed shadow (`#373737`).
    pub slot_inner_bevel_dark: Rgba8,
    /// Slot inner bottom/right bevel — the recessed light edge
    /// (`#FFFFFF`).
    pub slot_inner_bevel_light: Rgba8,
    /// Slot hover highlight — `rgba(255,255,255,128)` washed over the
    /// 16×16 icon area.
    pub slot_hover_highlight: Rgba8,

    /// Classic tooltip background — `rgba(16,0,16,240)`, near-black
    /// violet.
    pub tooltip_background: Rgba8,
    /// Tooltip border gradient top — `rgba(80,0,255,80)`.
    pub tooltip_border_top: Rgba8,
    /// Tooltip border gradient bottom — `rgba(40,0,127,80)`.
    pub tooltip_border_bottom: Rgba8,

    /// Primary light text (`#FFFFFF`), always drawn with
    /// [`text_shadow`](Self::text_shadow) 1px below-right.
    pub text_light: Rgba8,
    /// Light-text drop shadow (`#3F3F3F`).
    pub text_shadow: Rgba8,
    /// Focused/hovered control label (`#FFFFA0`).
    pub text_focused: Rgba8,
    /// Container-panel dark text (`#404040`) — inventory captions.
    /// Drawn *without* the drop shadow, exactly as vanilla.
    pub text_dark: Rgba8,
    /// Muted/secondary text (`#A0A0A0`).
    pub text_muted: Rgba8,
    /// Danger/error text (`#FF5555`).
    pub text_danger: Rgba8,
    /// Positive/accept text (`#55FF55`).
    pub text_positive: Rgba8,
    /// Warning text (`#FFFF55`).
    pub text_warning: Rgba8,

    /// Menu-blur scrim behind screens — `rgba(16,16,16,192)`.
    pub scrim: Rgba8,
    /// Header/footer separator tint for the clean-room path — a subtle
    /// `rgba(255,255,255,48)` rule. With vanilla assets the
    /// `header_separator`/`footer_separator` sprites are drawn instead.
    pub separator: Rgba8,
}

impl VanillaThemeTokens {
    /// Vanilla button height, logical px.
    pub const BUTTON_HEIGHT: u32 = 20;
    /// Vanilla default button width, logical px.
    pub const BUTTON_WIDTH_DEFAULT: u32 = 200;
    /// Inventory slot outer size, logical px (18×18 incl. bezel).
    pub const SLOT_SIZE: u32 = 18;
    /// Item-icon area inside a slot, logical px (16×16).
    pub const SLOT_ICON_SIZE: u32 = 16;
    /// Hotbar slot count.
    pub const HOTBAR_SLOTS: usize = 9;
    /// Text drop-shadow offset, device px.
    pub const TEXT_SHADOW_OFFSET_PX: i32 = 1;
    /// Tooltip inner padding, logical px.
    pub const TOOLTIP_PADDING_PX: u32 = 4;
}

/// The authoritative Vanilla 1.21.1 token table — every value above is
/// the measured vanilla color, so any renderer consuming this constant
/// is visually indistinguishable from stock 1.21.1.
pub const VANILLA_1_21_1: VanillaThemeTokens = VanillaThemeTokens {
    panel_face: Rgba8::rgb(0xC6, 0xC6, 0xC6),
    panel_bevel_light: Rgba8::rgb(0xFF, 0xFF, 0xFF),
    panel_bevel_dark: Rgba8::rgb(0x55, 0x55, 0x55),
    panel_outline: Rgba8::rgb(0x00, 0x00, 0x00),

    button_face: Rgba8::rgb(0x6F, 0x6F, 0x6F),
    button_bevel_light: Rgba8::rgb(0xA6, 0xA6, 0xA6),
    button_bevel_dark: Rgba8::rgb(0x48, 0x48, 0x48),
    button_hover_face: Rgba8::rgb(0x7E, 0x88, 0xA8),
    button_highlight_border: Rgba8::rgb(0xFF, 0xFF, 0xFF),
    button_disabled_face: Rgba8::rgb(0x3A, 0x3A, 0x3A),
    button_disabled_text: Rgba8::rgb(0xA0, 0xA0, 0xA0),

    slot_face: Rgba8::rgb(0x8B, 0x8B, 0x8B),
    slot_inner_bevel_dark: Rgba8::rgb(0x37, 0x37, 0x37),
    slot_inner_bevel_light: Rgba8::rgb(0xFF, 0xFF, 0xFF),
    slot_hover_highlight: Rgba8::rgba(0xFF, 0xFF, 0xFF, 128),

    tooltip_background: Rgba8::rgba(16, 0, 16, 240),
    tooltip_border_top: Rgba8::rgba(80, 0, 255, 80),
    tooltip_border_bottom: Rgba8::rgba(40, 0, 127, 80),

    text_light: Rgba8::rgb(0xFF, 0xFF, 0xFF),
    text_shadow: Rgba8::rgb(0x3F, 0x3F, 0x3F),
    text_focused: Rgba8::rgb(0xFF, 0xFF, 0xA0),
    text_dark: Rgba8::rgb(0x40, 0x40, 0x40),
    text_muted: Rgba8::rgb(0xA0, 0xA0, 0xA0),
    text_danger: Rgba8::rgb(0xFF, 0x55, 0x55),
    text_positive: Rgba8::rgb(0x55, 0xFF, 0x55),
    text_warning: Rgba8::rgb(0xFF, 0xFF, 0x55),

    scrim: Rgba8::rgba(16, 16, 16, 192),
    separator: Rgba8::rgba(0xFF, 0xFF, 0xFF, 48),
};

/// Smallest logical viewport dimension vanilla's auto GUI scale
/// preserves (`Window.calculateScale`: `w/s >= 320`, `h/s >= 240`).
pub const GUI_SCALE_MIN_LOGICAL_W: u32 = 320;
/// See [`GUI_SCALE_MIN_LOGICAL_W`].
pub const GUI_SCALE_MIN_LOGICAL_H: u32 = 240;

/// Vanilla `Window.calculateScale` — the integer GUI scale.
///
/// `requested` mirrors the options value: `0` means *auto* — the
/// largest integer `s >= 1` with `phys_w / s >= 320` and
/// `phys_h / s >= 240`. A nonzero `requested` caps the result at that
/// scale (the loop stops once `s == requested`), and the size limit
/// can still pull it lower on small windows. The result is always an
/// integer `>= 1`, so every renderer snaps to the same grid.
///
/// Faithful port of vanilla's loop; the `forceUnicode` even-snap is a
/// font-path concern handled by `client-ui`, not this contract.
#[must_use]
pub fn compute_gui_scale(phys_w: u32, phys_h: u32, requested: u32) -> u32 {
    let mut scale: u32 = 1;
    while scale != requested
        && scale < phys_w
        && scale < phys_h
        && phys_w / (scale + 1) >= GUI_SCALE_MIN_LOGICAL_W
        && phys_h / (scale + 1) >= GUI_SCALE_MIN_LOGICAL_H
    {
        scale += 1;
    }
    scale
}

/// Snap a logical coordinate onto the device pixel grid: `logical *
/// scale`, saturating instead of wrapping on absurd inputs. Used for
/// bevel offsets, text-shadow placement and `snap_px`-aligned widget
/// rects so 1px chrome stays crisp at every integer scale.
#[inline]
#[must_use]
pub const fn snap_px(logical: i32, scale: u32) -> i32 {
    let s = if scale > i32::MAX as u32 {
        i32::MAX
    } else {
        scale as i32
    };
    logical.saturating_mul(s)
}

/// Where a [`ModUiManifest`] may attach in the canonical UI tree.
///
/// The slot list is closed on purpose. **`TitleScreenOverlay` does not
/// exist**: the Vanilla 1.21.1 title screen ships exactly as authored
/// and no manifest can request space on it — see the module-level
/// purity invariant. Every variant reports
/// [`overlays_title_screen`](Self::overlays_title_screen) == `false`,
/// making the guarantee auditable rather than implied.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum UiExtensionSlot {
    /// HUD cluster anchored top-left (F3-style status lines).
    HudTopLeft,
    /// HUD cluster anchored top-right (minimap-style widgets).
    HudTopRight,
    /// HUD cluster anchored bottom-left (chat-adjacent status).
    HudBottomLeft,
    /// HUD cluster anchored bottom-right (Create goggles-style readouts).
    HudBottomRight,
    /// Extra panel docked to the left edge of a container screen
    /// (JEI/EMI ingredient lists).
    ContainerLeftPanel,
    /// Extra panel docked to the right edge of a container screen
    /// (JEI/EMI bookmark lists).
    ContainerRightPanel,
    /// Additional action button inside the pause menu.
    PauseMenuAction,
    /// Additional category inside the options screens.
    OptionsCategory,
    /// A fully mod-owned screen opened on demand — the only slot that
    /// may cover the whole viewport.
    CustomScreen,
}

impl UiExtensionSlot {
    /// All extension slots, in declaration order.
    pub const ALL: [Self; Self::COUNT] = [
        Self::HudTopLeft,
        Self::HudTopRight,
        Self::HudBottomLeft,
        Self::HudBottomRight,
        Self::ContainerLeftPanel,
        Self::ContainerRightPanel,
        Self::PauseMenuAction,
        Self::OptionsCategory,
        Self::CustomScreen,
    ];

    /// Number of extension slots.
    pub const COUNT: usize = 9;

    /// Stable snake_case key for manifests and logs.
    #[inline]
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::HudTopLeft => "hud_top_left",
            Self::HudTopRight => "hud_top_right",
            Self::HudBottomLeft => "hud_bottom_left",
            Self::HudBottomRight => "hud_bottom_right",
            Self::ContainerLeftPanel => "container_left_panel",
            Self::ContainerRightPanel => "container_right_panel",
            Self::PauseMenuAction => "pause_menu_action",
            Self::OptionsCategory => "options_category",
            Self::CustomScreen => "custom_screen",
        }
    }

    /// `false` for every variant, by architectural invariant: no
    /// extension slot may paint over the Vanilla 1.21.1 title screen.
    /// The method exists so renderers and audits *check* the guarantee
    /// instead of re-deriving it from the variant list.
    #[inline]
    #[must_use]
    pub const fn overlays_title_screen(self) -> bool {
        false
    }
}

impl fmt::Display for UiExtensionSlot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The closed set of declarative widgets a screen — vanilla *or*
/// modded — may emit.
///
/// Renderers match on this enum; there is no `Custom` escape hatch, so
/// every drawable a mod can produce is already accounted for in the
/// theme tokens and the 9-slice sprite table.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum UiWidgetKind {
    /// Beveled background panel ([`SpriteId::Panel`]).
    Panel,
    /// Stone button ([`SpriteId::Button`] family).
    Button,
    /// Boolean toggle (drawn as a button with a latch state).
    Toggle,
    /// Bounded numeric slider — uses `min`/`max`/`value`.
    Slider,
    /// Single-line text field ([`SpriteId::TextField`] family).
    TextField,
    /// Recessed 18×18 inventory slot ([`SpriteId::Slot`]).
    Slot,
    /// Determinate progress bar — `value` inside `min..=max`.
    ProgressBar,
    /// Static text.
    Label,
    /// Scrollable list ([`SpriteId::Scroller`] family).
    ScrollList,
    /// Hover tooltip ([`SpriteId::TooltipBackground`] +
    /// [`SpriteId::TooltipFrame`]).
    Tooltip,
    /// Horizontal rule ([`SpriteId::HeaderSeparator`] /
    /// [`SpriteId::FooterSeparator`]).
    Separator,
}

impl UiWidgetKind {
    /// All widget kinds, in declaration order.
    pub const ALL: [Self; Self::COUNT] = [
        Self::Panel,
        Self::Button,
        Self::Toggle,
        Self::Slider,
        Self::TextField,
        Self::Slot,
        Self::ProgressBar,
        Self::Label,
        Self::ScrollList,
        Self::Tooltip,
        Self::Separator,
    ];

    /// Number of widget kinds.
    pub const COUNT: usize = 11;

    /// Stable snake_case key for manifests and logs.
    #[inline]
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Panel => "panel",
            Self::Button => "button",
            Self::Toggle => "toggle",
            Self::Slider => "slider",
            Self::TextField => "text_field",
            Self::Slot => "slot",
            Self::ProgressBar => "progress_bar",
            Self::Label => "label",
            Self::ScrollList => "scroll_list",
            Self::Tooltip => "tooltip",
            Self::Separator => "separator",
        }
    }
}

impl fmt::Display for UiWidgetKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

// ---------------------------------------------------------------------------
// Contract bounds — the numbers that keep a rogue mod bounded
// ---------------------------------------------------------------------------

/// Maximum widgets a single [`ModUiManifest`] may declare.
pub const MAX_WIDGETS_PER_MANIFEST: usize = 64;
/// Maximum widget label length in bytes — the vanilla chat bound.
pub const MAX_LABEL_LEN: usize = 256;
/// Maximum tooltip text length in bytes.
pub const MAX_TOOLTIP_LEN: usize = 512;
/// Maximum `mod_id` length in bytes.
pub const MAX_MOD_ID_LEN: usize = 64;
/// Logical-space coordinate bound (`±`), px.
pub const MAX_UI_COORD: i32 = 16_384;
/// Maximum widget dimension, px.
pub const MAX_UI_DIM: u32 = 8_192;

/// Axis-aligned rectangle in logical UI pixels — the layout currency
/// for [`UiWidgetSpec`]. Coordinates may be negative only within
/// [`MAX_UI_COORD`] so off-viewport staging stays legal but bounded.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct UiRect {
    /// Left edge, logical px.
    pub x: i32,
    /// Top edge, logical px.
    pub y: i32,
    /// Width, logical px.
    pub w: u32,
    /// Height, logical px.
    pub h: u32,
}

impl UiRect {
    /// Construct a rect — validation against [`MAX_UI_COORD`] /
    /// [`MAX_UI_DIM`] happens in [`UiWidgetSpec::validate`].
    #[inline]
    #[must_use]
    pub const fn new(x: i32, y: i32, w: u32, h: u32) -> Self {
        Self { x, y, w, h }
    }

    /// `true` when the rect covers no pixels.
    #[inline]
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.w == 0 || self.h == 0
    }

    /// Right edge (`x + w`), saturating.
    #[inline]
    #[must_use]
    pub const fn right(self) -> i32 {
        let w = if self.w > i32::MAX as u32 {
            i32::MAX
        } else {
            self.w as i32
        };
        self.x.saturating_add(w)
    }

    /// Bottom edge (`y + h`), saturating.
    #[inline]
    #[must_use]
    pub const fn bottom(self) -> i32 {
        let h = if self.h > i32::MAX as u32 {
            i32::MAX
        } else {
            self.h as i32
        };
        self.y.saturating_add(h)
    }

    /// `true` when logical point `(px, py)` is inside the rect.
    #[inline]
    #[must_use]
    pub const fn contains(self, px: i32, py: i32) -> bool {
        px >= self.x && px < self.right() && py >= self.y && py < self.bottom()
    }
}

/// One declarative widget in a screen's UI tree.
///
/// A `UiWidgetSpec` is *data*, not behavior: kind, rect, bounded text,
/// a value range and an enabled flag. Vanilla screens emit the same
/// specs mods do, so the renderer (and its dirty-layer bookkeeping in
/// [`crate::rev`]) sees one shape regardless of who authored the
/// screen.
#[derive(Clone, Debug, PartialEq)]
pub struct UiWidgetSpec {
    /// Which chrome/behavior the renderer instantiates.
    pub kind: UiWidgetKind,
    /// Logical-pixel bounds — validated against [`MAX_UI_COORD`] /
    /// [`MAX_UI_DIM`].
    pub rect: UiRect,
    /// Display text — bounded by [`MAX_LABEL_LEN`].
    pub label: String,
    /// Optional hover text — bounded by [`MAX_TOOLTIP_LEN`].
    pub tooltip: Option<String>,
    /// Current value for [`UiWidgetKind::Slider`] /
    /// [`UiWidgetKind::ProgressBar`]; ignored by other kinds.
    pub value: i32,
    /// Range minimum for value-bearing kinds.
    pub min: i32,
    /// Range maximum for value-bearing kinds.
    pub max: i32,
    /// `false` draws the widget disabled (greyed, non-interactive).
    pub enabled: bool,
    /// Optional sprite override — restricted to the canonical
    /// [`SpriteId`] table, so a mod can re-skin a widget but can never
    /// point the renderer at an arbitrary texture path.
    pub sprite: Option<SpriteId>,
}

impl UiWidgetSpec {
    /// Bare widget: enabled, empty label, `0..=100` value range.
    #[must_use]
    pub fn new(kind: UiWidgetKind, rect: UiRect) -> Self {
        Self {
            kind,
            rect,
            label: String::new(),
            tooltip: None,
            value: 0,
            min: 0,
            max: 100,
            enabled: true,
            sprite: None,
        }
    }

    /// Set the display label (validated on
    /// [`validate`](Self::validate), not here — builders stay
    /// allocation-free).
    #[must_use]
    pub fn with_label(mut self, label: impl Into<String>) -> Self {
        self.label = label.into();
        self
    }

    /// Attach hover text.
    #[must_use]
    pub fn with_tooltip(mut self, tooltip: impl Into<String>) -> Self {
        self.tooltip = Some(tooltip.into());
        self
    }

    /// Set the `min..=max`/`value` triple for
    /// [`UiWidgetKind::Slider`] and [`UiWidgetKind::ProgressBar`].
    #[must_use]
    pub fn with_range(mut self, min: i32, max: i32, value: i32) -> Self {
        self.min = min;
        self.max = max;
        self.value = value;
        self
    }

    /// Draw the widget disabled.
    #[must_use]
    pub const fn disabled(mut self) -> Self {
        self.enabled = false;
        self
    }

    /// Override the widget's sprite with a canonical [`SpriteId`].
    #[must_use]
    pub const fn with_sprite(mut self, sprite: SpriteId) -> Self {
        self.sprite = Some(sprite);
        self
    }

    /// Check the spec against every contract bound, returning the
    /// *first* violation.
    ///
    /// # Errors
    ///
    /// [`UiContractError::TextTooLong`], [`UiContractError::RectOutOfBounds`],
    /// [`UiContractError::RangeInverted`] or
    /// [`UiContractError::ValueOutOfRange`] — never panics, so hostile
    /// manifests fail closed.
    pub fn validate(&self) -> Result<(), UiContractError> {
        if self.label.len() > MAX_LABEL_LEN {
            return Err(UiContractError::TextTooLong {
                field: "label",
                len: self.label.len(),
                max: MAX_LABEL_LEN,
            });
        }
        if let Some(tooltip) = &self.tooltip {
            if tooltip.len() > MAX_TOOLTIP_LEN {
                return Err(UiContractError::TextTooLong {
                    field: "tooltip",
                    len: tooltip.len(),
                    max: MAX_TOOLTIP_LEN,
                });
            }
        }
        let r = self.rect;
        let coord_ok = |c: i32| c.abs() <= MAX_UI_COORD;
        if !coord_ok(r.x)
            || !coord_ok(r.y)
            || r.w == 0
            || r.h == 0
            || r.w > MAX_UI_DIM
            || r.h > MAX_UI_DIM
        {
            return Err(UiContractError::RectOutOfBounds { rect: r });
        }
        if matches!(self.kind, UiWidgetKind::Slider | UiWidgetKind::ProgressBar) {
            if self.min > self.max {
                return Err(UiContractError::RangeInverted {
                    min: self.min,
                    max: self.max,
                });
            }
            if self.value < self.min || self.value > self.max {
                return Err(UiContractError::ValueOutOfRange {
                    value: self.value,
                    min: self.min,
                    max: self.max,
                });
            }
        }
        Ok(())
    }
}

/// Why a widget or manifest violated the UI contract.
#[derive(Clone, Debug, PartialEq)]
pub enum UiContractError {
    /// `mod_id` is empty.
    EmptyModId,
    /// `mod_id` exceeds [`MAX_MOD_ID_LEN`] bytes.
    ModIdTooLong {
        /// The offending length.
        len: usize,
    },
    /// `mod_id` contains a character outside the namespace set
    /// `[a-z0-9_.-]` — the same set resource-location namespaces use.
    BadModIdChar {
        /// The offending character.
        ch: char,
    },
    /// A manifest declared more than [`MAX_WIDGETS_PER_MANIFEST`]
    /// widgets.
    TooManyWidgets {
        /// Declared widget count.
        count: usize,
    },
    /// A text field exceeded its byte cap.
    TextTooLong {
        /// Which field: `"label"` or `"tooltip"`.
        field: &'static str,
        /// The offending length.
        len: usize,
        /// The applicable cap.
        max: usize,
    },
    /// A widget rect is empty, oversized or anchored outside
    /// [`MAX_UI_COORD`].
    RectOutOfBounds {
        /// The offending rect.
        rect: UiRect,
    },
    /// `min > max` on a value-bearing widget.
    RangeInverted {
        /// Declared minimum.
        min: i32,
        /// Declared maximum.
        max: i32,
    },
    /// `value` outside `min..=max` on a value-bearing widget.
    ValueOutOfRange {
        /// Declared value.
        value: i32,
        /// Declared minimum.
        min: i32,
        /// Declared maximum.
        max: i32,
    },
    /// A manifest attempted to attach to the title screen. Unreachable
    /// while [`UiExtensionSlot`] keeps its no-title-screen invariant —
    /// kept as the fail-closed escape valve so a future enum change
    /// surfaces as a violation instead of silently painting the title
    /// screen.
    TitleScreenOverlayRequested,
}

impl fmt::Display for UiContractError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyModId => f.write_str("mod_id is empty"),
            Self::ModIdTooLong { len } => {
                write!(f, "mod_id is {len} bytes, cap is {MAX_MOD_ID_LEN}")
            }
            Self::BadModIdChar { ch } => {
                write!(f, "mod_id contains {ch:?}, outside [a-z0-9_.-]")
            }
            Self::TooManyWidgets { count } => {
                write!(
                    f,
                    "manifest declares {count} widgets, cap is {MAX_WIDGETS_PER_MANIFEST}"
                )
            }
            Self::TextTooLong { field, len, max } => {
                write!(f, "{field} is {len} bytes, cap is {max}")
            }
            Self::RectOutOfBounds { rect } => {
                write!(
                    f,
                    "rect ({}, {} {}x{}) is empty or outside ±{MAX_UI_COORD}/{MAX_UI_DIM}",
                    rect.x, rect.y, rect.w, rect.h
                )
            }
            Self::RangeInverted { min, max } => {
                write!(f, "range inverted: min {min} > max {max}")
            }
            Self::ValueOutOfRange { value, min, max } => {
                write!(f, "value {value} outside {min}..={max}")
            }
            Self::TitleScreenOverlayRequested => {
                f.write_str("title screen extension requested — forbidden by invariant")
            }
        }
    }
}

impl Error for UiContractError {}

/// One mod's complete declarative UI contribution: an id, the
/// [`UiExtensionSlot`] it attaches to, and a bounded widget list.
///
/// The same shape describes vanilla's own screens when the built-in
/// UI is expressed through the contract — the validator makes no
/// distinction between `minecraft:` and `create:` manifests.
#[derive(Clone, Debug, PartialEq)]
pub struct ModUiManifest {
    /// Owning mod id — namespace grammar (`[a-z0-9_.-]`), bounded by
    /// [`MAX_MOD_ID_LEN`].
    pub mod_id: String,
    /// Attachment point in the canonical UI tree.
    pub slot: UiExtensionSlot,
    /// Declared widgets — bounded by [`MAX_WIDGETS_PER_MANIFEST`].
    pub widgets: Vec<UiWidgetSpec>,
}

impl ModUiManifest {
    /// Empty manifest for `mod_id` attaching at `slot`.
    #[must_use]
    pub fn new(mod_id: impl Into<String>, slot: UiExtensionSlot) -> Self {
        Self {
            mod_id: mod_id.into(),
            slot,
            widgets: Vec::new(),
        }
    }

    /// Append a widget. Contract bounds are enforced by
    /// [`validate`](Self::validate), not here — construction stays
    /// infallible so partial manifests can be staged before checks.
    pub fn push_widget(&mut self, widget: UiWidgetSpec) {
        self.widgets.push(widget);
    }

    /// Check the whole manifest against the contract, returning the
    /// *first* violation — the fail-fast gate for the hot path.
    ///
    /// # Errors
    ///
    /// Any [`UiContractError`]; [`audit_manifest`] reports all of them
    /// at once instead.
    pub fn validate(&self) -> Result<(), UiContractError> {
        if self.mod_id.is_empty() {
            return Err(UiContractError::EmptyModId);
        }
        if self.mod_id.len() > MAX_MOD_ID_LEN {
            return Err(UiContractError::ModIdTooLong {
                len: self.mod_id.len(),
            });
        }
        for ch in self.mod_id.chars() {
            if !(ch.is_ascii_lowercase()
                || ch.is_ascii_digit()
                || matches!(ch, '_' | '-' | '.'))
            {
                return Err(UiContractError::BadModIdChar { ch });
            }
        }
        if self.widgets.len() > MAX_WIDGETS_PER_MANIFEST {
            return Err(UiContractError::TooManyWidgets {
                count: self.widgets.len(),
            });
        }
        for widget in &self.widgets {
            widget.validate()?;
        }
        // Title-screen purity: `slot` is a closed enum with no
        // title-screen variant, but the check is kept explicit so the
        // invariant stays load-bearing if the enum ever grows.
        if self.slot.overlays_title_screen() {
            return Err(UiContractError::TitleScreenOverlayRequested);
        }
        Ok(())
    }
}

/// Whole-contract audit of one [`ModUiManifest`] — every violation,
/// not just the first, plus the integer GUI scale the same window
/// would get under vanilla. Used by loaders that want to report all
/// problems at once, and by the budget harness that proves a rogue mod
/// cannot freeze the UI.
#[derive(Clone, Debug)]
pub struct UiBudgetReport {
    /// Vanilla `compute_gui_scale` for the audited window.
    pub gui_scale: u32,
    /// Widgets the manifest declared.
    pub widget_count: usize,
    /// The cap that was applied — [`MAX_WIDGETS_PER_MANIFEST`].
    pub max_widgets: usize,
    /// Longest label or tooltip seen, bytes.
    pub longest_text: usize,
    /// The label cap — [`MAX_LABEL_LEN`].
    pub max_text_len: usize,
    /// Every contract violation found, in widget order (mod-id and
    /// count violations first).
    pub violations: Vec<UiContractError>,
}

impl UiBudgetReport {
    /// `true` when the manifest is fully inside the contract — no
    /// violations at all.
    #[inline]
    #[must_use]
    pub fn is_within_budget(&self) -> bool {
        self.violations.is_empty()
    }
}

/// Audit `manifest` against every contract bound, collecting *all*
/// violations, and record the vanilla integer GUI scale a
/// `phys_w`×`phys_h` window with `requested_scale` would get — the
/// scale every emitted widget will be rasterized at.
#[must_use]
pub fn audit_manifest(
    manifest: &ModUiManifest,
    phys_w: u32,
    phys_h: u32,
    requested_scale: u32,
) -> UiBudgetReport {
    let mut violations = Vec::new();
    if manifest.mod_id.is_empty() {
        violations.push(UiContractError::EmptyModId);
    } else if manifest.mod_id.len() > MAX_MOD_ID_LEN {
        violations.push(UiContractError::ModIdTooLong {
            len: manifest.mod_id.len(),
        });
    } else {
        for ch in manifest.mod_id.chars() {
            if !(ch.is_ascii_lowercase()
                || ch.is_ascii_digit()
                || matches!(ch, '_' | '-' | '.'))
            {
                violations.push(UiContractError::BadModIdChar { ch });
            }
        }
    }
    if manifest.widgets.len() > MAX_WIDGETS_PER_MANIFEST {
        violations.push(UiContractError::TooManyWidgets {
            count: manifest.widgets.len(),
        });
    }
    let mut longest_text = manifest.mod_id.len();
    for widget in &manifest.widgets {
        longest_text = longest_text
            .max(widget.label.len())
            .max(widget.tooltip.as_ref().map_or(0, String::len));
        if let Err(err) = widget.validate() {
            violations.push(err);
        }
    }
    if manifest.slot.overlays_title_screen() {
        // Unreachable while the invariant holds; kept so an enum change
        // that ever breaks purity surfaces as a violation, not silence.
        violations.push(UiContractError::TitleScreenOverlayRequested);
    }
    UiBudgetReport {
        gui_scale: compute_gui_scale(phys_w, phys_h, requested_scale),
        widget_count: manifest.widgets.len(),
        max_widgets: MAX_WIDGETS_PER_MANIFEST,
        longest_text,
        max_text_len: MAX_LABEL_LEN,
        violations,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gui_scale_matches_vanilla_window_math() {
        // (phys, requested) → vanilla Window.calculateScale result.
        let cases: &[((u32, u32), u32, u32)] = &[
            ((640, 480), 0, 2),
            ((1280, 720), 0, 3),
            ((1920, 1080), 0, 4),
            ((2560, 1440), 0, 6),
            ((3840, 2160), 0, 9),
            ((320, 240), 0, 1),
            ((300, 200), 0, 1),
            ((3840, 2160), 2, 2),
            ((1920, 1080), 8, 4), // size limit pulls requested down
            ((1920, 1080), 1, 1),
        ];
        for &((w, h), req, want) in cases {
            assert_eq!(compute_gui_scale(w, h, req), want, "{w}x{h} req={req}");
        }
    }

    #[test]
    fn title_screen_purity_is_a_type_fact() {
        assert!(UiExtensionSlot::ALL
            .iter()
            .all(|slot| !slot.overlays_title_screen()));
        assert!(UiExtensionSlot::ALL
            .iter()
            .all(|slot| !slot.as_str().contains("title")));
    }

    #[test]
    fn vanilla_tokens_hold_exact_hex() {
        let t = VANILLA_1_21_1;
        assert_eq!(t.panel_face, Rgba8::rgb(0xC6, 0xC6, 0xC6));
        assert_eq!(t.button_face, Rgba8::rgb(0x6F, 0x6F, 0x6F));
        assert_eq!(t.slot_face, Rgba8::rgb(0x8B, 0x8B, 0x8B));
        assert_eq!(t.tooltip_background, Rgba8::rgba(16, 0, 16, 240));
        assert_eq!(t.text_dark, Rgba8::rgb(0x40, 0x40, 0x40));
        assert_eq!(t.scrim, Rgba8::rgba(16, 16, 16, 192));
    }
}
