// SPDX-License-Identifier: LGPL-3.0-or-later
#![forbid(unsafe_code)]

//! The one shared Vanilla 1.21.1 widget kit.
//!
//! Every surface — vanilla screens, Megu's own UX screens (recipe browser overlay,
//! ModSync browser, settings, claim map) and [`ModUiManifest`] mod UIs —
//! draws through these same primitives, which take their colors
//! exclusively from [`megu_core::ui_contract::VANILLA_1_21_1`] (via
//! [`crate::draw::palette`]) and snap every coordinate through
//! [`megu_core::ui_contract::snap_px`]. A mod config panel is therefore
//! indistinguishable from a vanilla options row.
//!
//! Two chrome modes share one code path: when `client-gpu` decoded the
//! real vanilla `widget/*` sprites into a [`GuiSpriteAtlas`], widgets
//! emit [`crate::draw::UiCommand::Sprite`] and the GPU samples the
//! authentic texture (9-sliced where `.mcmeta` says so); on a clean-room
//! install the same calls emit the pixel-authentic vector stone bevels
//! below. Mod manifests never observe the difference.

use megu_core::ui_contract::{
    audit_manifest, compute_gui_scale, snap_px, ModUiManifest, SpriteId, UiBudgetReport,
    UiExtensionSlot, UiWidgetKind, VanillaThemeTokens,
};

use crate::draw::{palette, text_width, GuiSpriteAtlas, UiColor, UiDrawList, UiRect};

/// The widget chrome context: an integer GUI scale (device px per
/// logical px) plus the optional vanilla sprite atlas that switches a
/// widget from vector bevels to textured chrome.
#[derive(Clone, Copy, Debug)]
pub struct VanillaWidgets<'a> {
    /// Device px per logical px — always `>= 1`, always integer.
    pub scale: u32,
    /// Packed vanilla GUI atlas, when vanilla assets loaded.
    pub sprites: Option<&'a GuiSpriteAtlas>,
    /// Device-px origin every logical coordinate is offset by —
    /// `0, 0` for screens that author in viewport space, the slot
    /// anchor for manifest widgets.
    dx: i32,
    dy: i32,
}

impl<'a> VanillaWidgets<'a> {
    /// Clean-room kit at integer `scale` (clamped to `>= 1`).
    pub fn new(scale: u32) -> Self {
        Self {
            scale: scale.max(1),
            sprites: None,
            dx: 0,
            dy: 0,
        }
    }

    /// Sprite-backed kit: `sprites` entries that exist draw textured
    /// chrome, missing entries keep the vector fallback per widget.
    pub fn with_sprites(scale: u32, sprites: &'a GuiSpriteAtlas) -> Self {
        Self {
            scale: scale.max(1),
            sprites: if sprites.is_empty() {
                None
            } else {
                Some(sprites)
            },
            dx: 0,
            dy: 0,
        }
    }

    /// `true` when `sprite` is packed — the per-widget sprite/fallback
    /// decision point.
    fn has(&self, sprite: SpriteId) -> bool {
        self.sprites.is_some_and(|a| a.has(sprite))
    }

    /// `coord` logical px → snapped device px (origin applied).
    fn px(&self, logical: i32) -> i32 {
        snap_px(logical, self.scale)
    }

    /// Logical `(x, y, w, h)` → snapped device [`UiRect`] (origin applied).
    fn rect(&self, x: i32, y: i32, w: u32, h: u32) -> UiRect {
        UiRect::new(
            self.dx + self.px(x),
            self.dy + self.px(y),
            snap_px(w as i32, self.scale) as u32,
            snap_px(h as i32, self.scale) as u32,
        )
    }
}

/// Button interaction state — the three vanilla faces.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum VanillaButtonState {
    /// Stone face, normal text.
    #[default]
    Normal,
    /// Brightened face, `#FFFFFF` label border, `#FFFFA0` text.
    Hovered,
    /// `#3A3A3A` face, `#A0A0A0` shadowless text.
    Disabled,
}

/// A scrollbar track/thumb pair.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct VanillaScroll {
    /// Scroll position in `0.0..=1.0`.
    pub fraction: f32,
    /// Fraction of content visible in the viewport (`<= 1.0` means the
    /// thumb fills the track).
    pub visible_fraction: f32,
}

// ---------------------------------------------------------------------------
// Free-function primitives — the shared kit
// ---------------------------------------------------------------------------

/// Beveled background panel: `#C6C6C6` face, `#FFFFFF`/`#555555` 3D
/// bevel, 1px `#000000` outline — or `widget/panel`-class sprite chrome
/// when the atlas carries [`SpriteId::Panel`].
pub fn draw_vanilla_panel(list: &mut UiDrawList, kit: &VanillaWidgets, x: i32, y: i32, w: u32, h: u32) {
    let rect = kit.rect(x, y, w, h);
    if kit.has(SpriteId::Panel) {
        list.sprite(rect, SpriteId::Panel);
        return;
    }
    let t = kit.px(1);
    list.fill_rect(rect, palette::PANEL);
    // 3D bevel: lit top/left, shaded bottom/right.
    list.fill_rect(UiRect::new(rect.x, rect.y, rect.w, t as u32), palette::PANEL_BEVEL_LIGHT);
    list.fill_rect(UiRect::new(rect.x, rect.y, t as u32, rect.h), palette::PANEL_BEVEL_LIGHT);
    list.fill_rect(
        UiRect::new(rect.x, rect.bottom() - t, rect.w, t as u32),
        palette::PANEL_BEVEL_DARK,
    );
    list.fill_rect(
        UiRect::new(rect.right() - t, rect.y, t as u32, rect.h),
        palette::PANEL_BEVEL_DARK,
    );
    // Crisp 1px outline inside the bevel.
    list.border_rect(rect, palette::PANEL_BORDER, t as u32);
}

/// Vanilla 20px stone button: `#6F6F6F` face with `#A6A6A6`/`#484848`
/// bevel and 1px `#000000` outline. Hovered draws the `#FFFFFF` label
/// border and `#FFFFA0` text; disabled draws `#3A3A3A` + `#A0A0A0`
/// shadowless text. `h` defaults to [`VanillaThemeTokens::BUTTON_HEIGHT`]
/// logical px when `None`.
pub fn draw_vanilla_button(
    list: &mut UiDrawList,
    kit: &VanillaWidgets,
    x: i32,
    y: i32,
    w: u32,
    label: &str,
    state: VanillaButtonState,
) {
    let h = VanillaThemeTokens::BUTTON_HEIGHT;
    let rect = kit.rect(x, y, w, h);
    let sprite = match state {
        VanillaButtonState::Normal => SpriteId::Button,
        VanillaButtonState::Hovered => SpriteId::ButtonHighlighted,
        VanillaButtonState::Disabled => SpriteId::ButtonDisabled,
    };
    if kit.has(sprite) {
        list.sprite(rect, sprite);
    } else {
        let t = kit.px(1);
        let (face, bevel_l, bevel_d) = match state {
            VanillaButtonState::Disabled => (
                palette::BUTTON_DISABLED,
                palette::BUTTON_BEVEL_DARK,
                palette::BUTTON_BEVEL_DARK,
            ),
            VanillaButtonState::Hovered => (
                palette::BUTTON_HOVER,
                palette::BUTTON_BEVEL_LIGHT,
                palette::BUTTON_BEVEL_DARK,
            ),
            VanillaButtonState::Normal => (
                palette::BUTTON,
                palette::BUTTON_BEVEL_LIGHT,
                palette::BUTTON_BEVEL_DARK,
            ),
        };
        list.fill_rect(rect, face);
        // 3D stone bevel.
        list.fill_rect(UiRect::new(rect.x, rect.y, rect.w, t as u32), bevel_l);
        list.fill_rect(UiRect::new(rect.x, rect.y, t as u32, rect.h), bevel_l);
        list.fill_rect(
            UiRect::new(rect.x, rect.bottom() - t, rect.w, t as u32),
            bevel_d,
        );
        list.fill_rect(
            UiRect::new(rect.right() - t, rect.y, t as u32, rect.h),
            bevel_d,
        );
        list.border_rect(rect, palette::BUTTON_BORDER, t as u32);
        if state == VanillaButtonState::Hovered {
            // The `#FFFFFF` highlight border frames the label region.
            let inset = kit.px(2);
            let inner = inset_rect(rect, inset);
            if inner.w > 0 && inner.h > 0 {
                list.border_rect(inner, palette::BUTTON_BORDER_HOVER, t as u32);
            }
        }
    }
    if !label.is_empty() {
        let text_color = match state {
            VanillaButtonState::Disabled => palette::BUTTON_DISABLED_TEXT,
            VanillaButtonState::Hovered => palette::TEXT_FOCUSED,
            VanillaButtonState::Normal => palette::TEXT,
        };
        let tw = text_width(label, kit.scale) as i32;
        let cx = rect.x + ((rect.w as i32 - tw) / 2).max(kit.px(1));
        let cy = rect.y + ((rect.h as i32 - (8 * kit.scale) as i32) / 2).max(0);
        if state == VanillaButtonState::Disabled {
            // Disabled labels draw shadowless, exactly as vanilla.
            list.text_str(cx, cy, kit.scale, text_color, label);
        } else {
            draw_text_shadowed(list, kit, cx, cy, text_color, label);
        }
    }
}

/// Recessed 18×18 inventory slot: `#8B8B8B` face, `#373737` inner
/// top/left, `#FFFFFF` inner bottom/right. `hovered` washes
/// `rgba(255,255,255,128)` over the 16×16 icon area. `item_id` paints
/// an item icon inside when non-empty.
pub fn draw_vanilla_slot(
    list: &mut UiDrawList,
    kit: &VanillaWidgets,
    x: i32,
    y: i32,
    hovered: bool,
    item_id: &str,
) {
    let side = VanillaThemeTokens::SLOT_SIZE;
    let rect = kit.rect(x, y, side, side);
    if kit.has(SpriteId::Slot) {
        list.sprite(rect, SpriteId::Slot);
    } else {
        let t = kit.px(1);
        // Recessed well: dark inner top/left, light inner bottom/right.
        list.fill_rect(rect, palette::SLOT_INNER_DARK);
        list.fill_rect(
            UiRect::new(rect.right() - t, rect.y, t as u32, rect.h),
            palette::SLOT_INNER_LIGHT,
        );
        list.fill_rect(
            UiRect::new(rect.x, rect.bottom() - t, rect.w, t as u32),
            palette::SLOT_INNER_LIGHT,
        );
        let inner = inset_rect(rect, t);
        list.fill_rect(inner, palette::SLOT);
    }
    if hovered {
        let icon = kit.px(1);
        list.fill_rect(inset_rect(rect, icon), palette::SLOT_HOVER);
    }
    if !item_id.is_empty() {
        let icon_side = VanillaThemeTokens::SLOT_ICON_SIZE;
        let icon_rect = kit.rect(
            x + ((side - icon_side) / 2) as i32,
            y + ((side - icon_side) / 2) as i32,
            icon_side,
            icon_side,
        );
        list.item_icon_str(icon_rect, item_id);
    }
}

/// Empty-slot item silhouette used by Vanilla workstation screens.
pub fn draw_vanilla_ghost_item(list: &mut UiDrawList, rect: UiRect, item_id: &str) {
    if rect.w < 8 || rect.h < 8 || item_id.is_empty() {
        return;
    }
    list.item_icon_str(
        UiRect::new(rect.x + 2, rect.y + 2, rect.w - 4, rect.h - 4),
        item_id,
    );
}

/// Small recipe or workstation arrow using the shared Vanilla palette.
pub fn draw_vanilla_arrow(list: &mut UiDrawList, rect: UiRect, active: bool) {
    if rect.w == 0 || rect.h == 0 {
        return;
    }
    let (glyph, color) = if active {
        (">", palette::TEXT_DARK)
    } else {
        ("X", palette::DANGER)
    };
    let x = rect.x + ((rect.w as i32 - 8) / 2).max(0);
    let y = rect.y + ((rect.h as i32 - 8) / 2).max(0);
    list.text_str(x, y, 1, color, glyph);
}

/// Classic tooltip: `rgba(16,0,16,240)` near-black body, 1px purple
/// gradient frame (`#5000FF50` top → `#28007F50` bottom) — or the
/// tooltip sprites when the atlas carries them. `lines` paints each
/// entry as shadowed text inside [`VanillaThemeTokens::TOOLTIP_PADDING_PX`].
pub fn draw_vanilla_tooltip(
    list: &mut UiDrawList,
    kit: &VanillaWidgets,
    x: i32,
    y: i32,
    w: u32,
    h: u32,
    lines: &[&str],
) {
    let rect = kit.rect(x, y, w, h);
    if kit.has(SpriteId::TooltipBackground) {
        list.sprite(rect, SpriteId::TooltipBackground);
    } else {
        list.fill_rect(rect, palette::TOOLTIP_BG);
    }
    if kit.has(SpriteId::TooltipFrame) {
        list.sprite(rect, SpriteId::TooltipFrame);
    } else {
        let t = kit.px(1);
        list.fill_rect(UiRect::new(rect.x, rect.y, rect.w, t as u32), palette::TOOLTIP_FRAME_TOP);
        list.fill_rect(UiRect::new(rect.x, rect.y, t as u32, rect.h), palette::TOOLTIP_FRAME_TOP);
        list.fill_rect(
            UiRect::new(rect.x, rect.bottom() - t, rect.w, t as u32),
            palette::TOOLTIP_FRAME_BOTTOM,
        );
        list.fill_rect(
            UiRect::new(rect.right() - t, rect.y, t as u32, rect.h),
            palette::TOOLTIP_FRAME_BOTTOM,
        );
    }
    let pad = VanillaThemeTokens::TOOLTIP_PADDING_PX as i32;
    let line_h = 10 * kit.scale as i32;
    for (i, line) in lines.iter().enumerate() {
        if line.is_empty() {
            continue;
        }
        draw_text_shadowed(
            list,
            kit,
            rect.x + kit.px(pad),
            rect.y + kit.px(pad) + i as i32 * line_h,
            palette::TEXT,
            line,
        );
    }
}

/// Text-field presentation state for [`draw_vanilla_text_field`].
#[derive(Clone, Copy, Debug, Default)]
pub struct VanillaTextField<'a> {
    /// Current contents.
    pub text: &'a str,
    /// Focused → the `#FFFFFF` border / highlighted sprite.
    pub focused: bool,
    /// Blink phase — draw the `|` cursor block.
    pub cursor_visible: bool,
}

/// Vanilla single-line text field: near-black interior, 1px `#A0A0A0`
/// border (`#FFFFFF` while focused), text clipped to the field, a `|`
/// cursor block when `field.cursor_visible`.
pub fn draw_vanilla_text_field(
    list: &mut UiDrawList,
    kit: &VanillaWidgets,
    x: i32,
    y: i32,
    w: u32,
    field: VanillaTextField<'_>,
) {
    let h = VanillaThemeTokens::BUTTON_HEIGHT;
    let rect = kit.rect(x, y, w, h);
    let sprite = if field.focused {
        SpriteId::TextFieldHighlighted
    } else {
        SpriteId::TextField
    };
    if kit.has(sprite) {
        list.sprite(rect, sprite);
    } else {
        let t = kit.px(1);
        list.fill_rect(rect, UiColor::from_rgba8(megu_core::ui_contract::Rgba8::BLACK));
        list.border_rect(
            rect,
            if field.focused {
                palette::BUTTON_BORDER_HOVER
            } else {
                palette::TEXT_DIM
            },
            t as u32,
        );
    }
    let pad = kit.px(4);
    let mut tx = rect.x + pad;
    let ty = rect.y + ((rect.h as i32 - (8 * kit.scale) as i32) / 2).max(0);
    if !field.text.is_empty() {
        list.text_str(tx, ty, kit.scale, palette::TEXT, field.text);
        tx += text_width(field.text, kit.scale) as i32;
    }
    if field.focused && field.cursor_visible {
        let cur = UiRect::new(tx + 1, ty - 1, kit.px(1).max(1) as u32, (9 * kit.scale).min(rect.h));
        list.fill_rect(cur, palette::TEXT);
    }
}

/// Vanilla options slider: recessed track plus the stone knob at
/// `fraction` (`0.0..=1.0`, clamped). Label paints centered when
/// non-empty.
pub fn draw_vanilla_slider(
    list: &mut UiDrawList,
    kit: &VanillaWidgets,
    x: i32,
    y: i32,
    w: u32,
    fraction: f32,
    label: &str,
) {
    let h = VanillaThemeTokens::BUTTON_HEIGHT;
    let rect = kit.rect(x, y, w, h);
    if kit.has(SpriteId::ScrollerBackground) {
        list.sprite(rect, SpriteId::ScrollerBackground);
    } else {
        // Recessed track.
        list.fill_rect(rect, palette::SLOT_INNER_DARK);
        let inner = inset_rect(rect, kit.px(1));
        list.fill_rect(inner, UiColor::from_rgba8(megu_core::ui_contract::Rgba8::BLACK));
    }
    // The knob: 8px-wide stone button travelling the track interior.
    let knob_w = kit.px(8);
    let travel = (rect.w as i32 - knob_w).max(0);
    let kx = rect.x + (travel as f32 * fraction.clamp(0.0, 1.0)) as i32;
    let knob = UiRect::new(kx, rect.y, knob_w as u32, rect.h);
    if kit.has(SpriteId::Scroller) {
        list.sprite(knob, SpriteId::Scroller);
    } else {
        let t = kit.px(1);
        list.fill_rect(knob, palette::BUTTON);
        list.fill_rect(UiRect::new(knob.x, knob.y, knob.w, t as u32), palette::BUTTON_BEVEL_LIGHT);
        list.fill_rect(UiRect::new(knob.x, knob.y, t as u32, knob.h), palette::BUTTON_BEVEL_LIGHT);
        list.fill_rect(
            UiRect::new(knob.x, knob.bottom() - t, knob.w, t as u32),
            palette::BUTTON_BEVEL_DARK,
        );
        list.fill_rect(
            UiRect::new(knob.right() - t, knob.y, t as u32, knob.h),
            palette::BUTTON_BEVEL_DARK,
        );
        list.border_rect(knob, palette::BUTTON_BORDER, t as u32);
    }
    if !label.is_empty() {
        let tw = text_width(label, kit.scale) as i32;
        let cx = rect.x + ((rect.w as i32 - tw) / 2).max(kit.px(1));
        let cy = rect.y + ((rect.h as i32 - (8 * kit.scale) as i32) / 2).max(0);
        draw_text_shadowed(list, kit, cx, cy, palette::TEXT, label);
    }
}

/// Screen tab chrome: selected merges into the content below (no bottom
/// bevel), unselected sits one notch darker. `bounds` is the logical
/// `(x, y, w, h)` the kit snaps to the device grid.
pub fn draw_vanilla_tab(
    list: &mut UiDrawList,
    kit: &VanillaWidgets,
    bounds: UiRect,
    label: &str,
    selected: bool,
) {
    let rect = kit.rect(bounds.x, bounds.y, bounds.w, bounds.h);
    let sprite = if selected {
        SpriteId::TabSelected
    } else {
        SpriteId::TabUnselected
    };
    if kit.has(sprite) {
        list.sprite(rect, sprite);
    } else {
        let t = kit.px(1);
        list.fill_rect(
            rect,
            if selected {
                palette::PANEL
            } else {
                palette::BUTTON
            },
        );
        list.fill_rect(UiRect::new(rect.x, rect.y, rect.w, t as u32), palette::PANEL_BEVEL_LIGHT);
        list.fill_rect(UiRect::new(rect.x, rect.y, t as u32, rect.h), palette::PANEL_BEVEL_LIGHT);
        list.fill_rect(
            UiRect::new(rect.right() - t, rect.y, t as u32, rect.h),
            palette::PANEL_BEVEL_DARK,
        );
        if !selected {
            list.fill_rect(
                UiRect::new(rect.x, rect.bottom() - t, rect.w, t as u32),
                palette::PANEL_BEVEL_DARK,
            );
        }
        list.border_rect(rect, palette::PANEL_BORDER, t as u32);
    }
    if !label.is_empty() {
        let tw = text_width(label, kit.scale) as i32;
        let cx = rect.x + ((rect.w as i32 - tw) / 2).max(kit.px(1));
        let cy = rect.y + ((rect.h as i32 - (8 * kit.scale) as i32) / 2).max(0);
        draw_text_shadowed(list, kit, cx, cy, palette::TEXT, label);
    }
}

/// Vertical scrollbar: dark recessed track + stone thumb sized by
/// `scroll.visible_fraction` and positioned by `scroll.fraction`.
pub fn draw_vanilla_scrollbar(
    list: &mut UiDrawList,
    kit: &VanillaWidgets,
    x: i32,
    y: i32,
    w: u32,
    h: u32,
    scroll: VanillaScroll,
) {
    let rect = kit.rect(x, y, w, h);
    if kit.has(SpriteId::ScrollerBackground) {
        list.sprite(rect, SpriteId::ScrollerBackground);
    } else {
        list.fill_rect(rect, palette::SLOT_INNER_DARK);
        list.fill_rect(
            inset_rect(rect, kit.px(1)),
            UiColor::from_rgba8(megu_core::ui_contract::Rgba8::BLACK),
        );
    }
    let thumb_h = ((rect.h as f32) * scroll.visible_fraction.clamp(0.05, 1.0)) as i32;
    let travel = (rect.h as i32 - thumb_h).max(0);
    let ty = rect.y + (travel as f32 * scroll.fraction.clamp(0.0, 1.0)) as i32;
    let thumb = UiRect::new(rect.x, ty, rect.w, thumb_h.max(kit.px(4)) as u32);
    if kit.has(SpriteId::Scroller) {
        list.sprite(thumb, SpriteId::Scroller);
    } else {
        let t = kit.px(1);
        list.fill_rect(thumb, palette::BUTTON);
        list.fill_rect(UiRect::new(thumb.x, thumb.y, thumb.w, t as u32), palette::BUTTON_BEVEL_LIGHT);
        list.fill_rect(UiRect::new(thumb.x, thumb.y, t as u32, thumb.h), palette::BUTTON_BEVEL_LIGHT);
        list.fill_rect(
            UiRect::new(thumb.x, thumb.bottom() - t, thumb.w, t as u32),
            palette::BUTTON_BEVEL_DARK,
        );
        list.fill_rect(
            UiRect::new(thumb.right() - t, thumb.y, t as u32, thumb.h),
            palette::BUTTON_BEVEL_DARK,
        );
        list.border_rect(thumb, palette::BUTTON_BORDER, t as u32);
    }
}

/// Header/footer separator rule — `rgba(255,255,255,48)` when the
/// atlas lacks the separator sprites.
pub fn draw_vanilla_separator(list: &mut UiDrawList, kit: &VanillaWidgets, x: i32, y: i32, w: u32, footer: bool) {
    let rect = kit.rect(x, y, w, 1);
    let sprite = if footer {
        SpriteId::FooterSeparator
    } else {
        SpriteId::HeaderSeparator
    };
    if kit.has(sprite) {
        list.sprite(rect, sprite);
    } else {
        list.fill_rect(rect, palette::SEPARATOR);
    }
}

/// Text with the 1-px vanilla drop shadow (`#3F3F3F` at +1,+1 logical).
pub fn draw_text_shadowed(
    list: &mut UiDrawList,
    kit: &VanillaWidgets,
    x: i32,
    y: i32,
    color: UiColor,
    text: &str,
) {
    let off = kit.px(1);
    list.text_str(x + off, y + off, kit.scale, palette::TEXT_SHADOW, text);
    list.text_str(x, y, kit.scale, color, text);
}

/// `rect` shrunk by `m` px on every side (device px, clamped to empty).
fn inset_rect(rect: UiRect, m: i32) -> UiRect {
    UiRect::new(
        rect.x + m,
        rect.y + m,
        (rect.w as i32 - 2 * m).max(0) as u32,
        (rect.h as i32 - 2 * m).max(0) as u32,
    )
}

// ---------------------------------------------------------------------------
// Mod manifests — mod UIs inherit the identical foundation
// ---------------------------------------------------------------------------

/// The device-px anchor a manifest slot attaches to inside `viewport`.
/// Right/bottom anchors mirror the widget rect so `x`/`y` stay "inset
/// from the named edge" for manifest authors.
fn slot_anchor(
    slot: UiExtensionSlot,
    viewport: UiRect,
) -> (i32, i32, bool, bool) {
    match slot {
        UiExtensionSlot::HudTopLeft | UiExtensionSlot::ContainerLeftPanel => {
            (viewport.x, viewport.y, false, false)
        }
        UiExtensionSlot::HudTopRight | UiExtensionSlot::ContainerRightPanel => {
            (viewport.right(), viewport.y, true, false)
        }
        UiExtensionSlot::HudBottomLeft => (viewport.x, viewport.bottom(), false, true),
        UiExtensionSlot::HudBottomRight => (viewport.right(), viewport.bottom(), true, true),
        UiExtensionSlot::PauseMenuAction
        | UiExtensionSlot::OptionsCategory
        | UiExtensionSlot::CustomScreen => (viewport.x, viewport.y, false, false),
    }
}

/// Render a [`ModUiManifest`] into `list` — the mod-facing half of the
/// unified kit.
///
/// The manifest is first run through
/// [`audit_manifest`]: **any** contract violation (including the
/// title-screen-purity invariant — `slot` has no title-screen variant,
/// so no manifest can ever paint over the Vanilla 1.21.1 title screen)
/// aborts the draw and returns the report untouched. A clean manifest
/// then renders every widget with the exact same vanilla primitives
/// vanilla screens use, each logical coordinate snapped to the device
/// grid via [`snap_px`] at `scale` — normally
/// [`compute_gui_scale`]'s answer for the window.
///
/// `viewport` is the device-pixel window rect; HUD slots anchor to its
/// named edges, `CustomScreen` fills it.
///
/// Returns the [`UiBudgetReport`] either way so callers can log every
/// violation a hostile or buggy manifest produced.
pub fn draw_mod_ui_manifest(
    list: &mut UiDrawList,
    manifest: &ModUiManifest,
    scale: u32,
    viewport: UiRect,
) -> UiBudgetReport {
    draw_mod_ui_manifest_sprites(list, manifest, scale, viewport, None)
}

/// [`draw_mod_ui_manifest`] with an explicit sprite atlas — the
/// production entry point (window passes the decoded vanilla atlas;
/// `None` keeps the vector fallback).
pub fn draw_mod_ui_manifest_sprites(
    list: &mut UiDrawList,
    manifest: &ModUiManifest,
    scale: u32,
    viewport: UiRect,
    sprites: Option<&GuiSpriteAtlas>,
) -> UiBudgetReport {
    // `compute_gui_scale` normalizes the request first so the audit
    // sees the scale the host would actually apply.
    let scale = compute_gui_scale(viewport.w, viewport.h, scale.max(1));
    let report = audit_manifest(manifest, viewport.w, viewport.h, scale);
    if !report.is_within_budget() {
        return report;
    }
    // The audit resolved the effective GUI scale — draw on exactly the
    // grid it validated.
    let kit = VanillaWidgets {
        scale: report.gui_scale.max(1),
        sprites,
        dx: 0,
        dy: 0,
    };
    let (ax, ay, flip_x, flip_y) = slot_anchor(manifest.slot, viewport);
    for widget in &manifest.widgets {
        // Retarget the kit's device origin so this widget's logical
        // rect lands at its slot-anchored device position; the widget
        // is then drawn with the identical logical-coordinate calls
        // vanilla screens use.
        let lx = widget.rect.x;
        let ly = widget.rect.y;
        let w_px = snap_px(widget.rect.w as i32, kit.scale);
        let h_px = snap_px(widget.rect.h as i32, kit.scale);
        let mut wk = kit;
        // Non-flipped: dx = ax gives px(lx) + dx = ax + px(lx).
        // Flipped: px(lx) + dx = ax - px(lx) - w_px  →  dx = ax - w_px - 2*px(lx).
        wk.dx = if flip_x {
            ax - w_px - 2 * snap_px(lx, kit.scale)
        } else {
            ax
        };
        wk.dy = if flip_y {
            ay - h_px - 2 * snap_px(ly, kit.scale)
        } else {
            ay
        };
        draw_manifest_widget(list, &wk, widget);
    }
    report
}

/// One manifest widget drawn through the shared kit — identical calls
/// to what vanilla screens emit, so mod chrome is pixel-identical.
fn draw_manifest_widget(
    list: &mut UiDrawList,
    kit: &VanillaWidgets,
    widget: &megu_core::ui_contract::UiWidgetSpec,
) {
    let (lx, ly, lw, lh) = (
        widget.rect.x,
        widget.rect.y,
        widget.rect.w,
        widget.rect.h,
    );
    let state = if widget.enabled {
        VanillaButtonState::Normal
    } else {
        VanillaButtonState::Disabled
    };
    match widget.kind {
        UiWidgetKind::Panel => {
            draw_vanilla_panel(list, kit, lx, ly, lw, lh);
        }
        UiWidgetKind::Button | UiWidgetKind::Toggle => {
            draw_vanilla_button(list, kit, lx, ly, lw, &widget.label, state);
        }
        UiWidgetKind::Slider => {
            let span = (widget.max - widget.min).max(1) as f32;
            let fraction = (widget.value - widget.min) as f32 / span;
            draw_vanilla_slider(list, kit, lx, ly, lw, fraction, &widget.label);
        }
        UiWidgetKind::TextField => {
            draw_vanilla_text_field(
                list,
                kit,
                lx,
                ly,
                lw,
                VanillaTextField {
                    text: &widget.label,
                    focused: widget.enabled,
                    cursor_visible: false,
                },
            );
        }
        UiWidgetKind::Slot => {
            draw_vanilla_slot(list, kit, lx, ly, false, &widget.label);
        }
        UiWidgetKind::ProgressBar => {
            let span = (widget.max - widget.min).max(1) as f32;
            let fraction = (widget.value - widget.min) as f32 / span;
            let rect = kit.rect(lx, ly, lw, lh);
            list.progress_bar(
                rect,
                fraction,
                palette::ACCENT,
                UiColor::from_rgba8(megu_core::ui_contract::Rgba8::BLACK),
                palette::PANEL_BORDER,
            );
        }
        UiWidgetKind::Label => {
            let rect = kit.rect(lx, ly, lw, lh);
            list.text_str(rect.x, rect.y, kit.scale, palette::TEXT, &widget.label);
        }
        UiWidgetKind::ScrollList => {
            draw_vanilla_panel(list, kit, lx, ly, lw, lh);
            let track_w = 6u32;
            let track_x = lx + lw.saturating_sub(track_w) as i32;
            draw_vanilla_scrollbar(
                list,
                kit,
                track_x,
                ly,
                track_w,
                lh,
                VanillaScroll {
                    fraction: 0.0,
                    visible_fraction: 0.5,
                },
            );
        }
        UiWidgetKind::Tooltip => {
            let lines: Vec<&str> = widget
                .tooltip
                .as_deref()
                .unwrap_or(widget.label.as_str())
                .split('\n')
                .collect();
            draw_vanilla_tooltip(list, kit, lx, ly, lw, lh, &lines);
        }
        UiWidgetKind::Separator => {
            draw_vanilla_separator(list, kit, lx, ly, lw, false);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use megu_core::ui_contract::{UiRect as ContractRect, UiWidgetSpec};

    fn vector_kit() -> VanillaWidgets<'static> {
        VanillaWidgets::new(2)
    }

    #[test]
    fn vector_button_paints_stone_bevel_and_label() {
        let mut list = UiDrawList::new();
        let kit = vector_kit();
        draw_vanilla_button(&mut list, &kit, 4, 4, 60, "Done", VanillaButtonState::Normal);
        // Face + 4 bevel edges + outline + shadow + label.
        assert!(list.len() >= 8, "button should emit bevel+text cmds");
        assert!(list.commands.iter().any(|c| matches!(
            c,
            crate::draw::UiCommand::FillRect { color, .. } if *color == palette::BUTTON
        )));
    }

    #[test]
    fn clean_manifest_draws_with_snapped_coords() {
        let mut manifest = ModUiManifest::new("testmod", UiExtensionSlot::HudTopLeft);
        manifest.push_widget(
            UiWidgetSpec::new(
                UiWidgetKind::Button,
                ContractRect::new(10, 10, 60, 20),
            )
            .with_label("Go"),
        );
        let mut list = UiDrawList::new();
        let report = draw_mod_ui_manifest(
            &mut list,
            &manifest,
            2,
            UiRect::new(0, 0, 640, 480),
        );
        assert!(report.is_within_budget(), "{:?}", report.violations);
        assert!(!list.is_empty());
    }

    #[test]
    fn violating_manifest_draws_nothing() {
        let mut manifest = ModUiManifest::new("Bad Mod", UiExtensionSlot::HudTopLeft);
        manifest.push_widget(UiWidgetSpec::new(
            UiWidgetKind::Button,
            ContractRect::new(0, 0, 60, 20),
        ));
        let mut list = UiDrawList::new();
        let report = draw_mod_ui_manifest(
            &mut list,
            &manifest,
            2,
            UiRect::new(0, 0, 640, 480),
        );
        assert!(!report.is_within_budget());
        assert!(list.is_empty(), "violating manifest must draw nothing");
    }

    #[test]
    fn manifest_cannot_reach_title_screen() {
        // Purity is a type fact: no slot overlays the title screen.
        for slot in UiExtensionSlot::ALL {
            assert!(!slot.overlays_title_screen());
        }
    }
}
