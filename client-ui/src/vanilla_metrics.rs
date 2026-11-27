// SPDX-License-Identifier: LGPL-3.0-or-later
//! Single geometry authority for the Vanilla 1.21.1 pixel constants.
//!
//! Every screen builder that has to match the shipped game's layout —
//! the in-game HUD ([`crate::layout`]), the death and creative screens
//! ([`crate::death_screen`], [`crate::creative_screen`]) and the shared
//! font metrics in [`crate::draw`] — reads its numbers from here instead
//! of open-coding them. That is the whole point of the module: two
//! consumers that used to disagree about the hotbar width (198 vs 182)
//! can no longer drift, because there is exactly one `HOTBAR_WIDTH`.
//!
//! All values are **logical** pixels measured at GUI scale 1, in the
//! coordinate system vanilla's `GuiGraphics` uses: the origin is the
//! top-left of the scaled GUI viewport, `x` grows right, `y` grows down,
//! and the hotbar/vitals block is horizontally centred on `w / 2` while
//! being pinned to the viewport's bottom edge.
//!
//! Clean-room note: these are the layout numbers the shipped 1.21.1
//! client is documented to use (its own GUI sprites are `182×22` and its
//! death screen offsets are `h / 4 + 72` / `h / 4 + 96`); no Mojang
//! source or asset is copied, and every derived rectangle below is
//! re-expressed as a named constant or a `const fn`.

// ---------------------------------------------------------------------------
// Hotbar (`HotbarRenderer`)
// ---------------------------------------------------------------------------

/// Width of the nine-slot hotbar strip sprite.
pub const HOTBAR_WIDTH: i32 = 182;
/// Height of the hotbar strip sprite.
pub const HOTBAR_HEIGHT: i32 = 22;
/// Selectable hotbar cells.
pub const HOTBAR_SLOT_COUNT: usize = 9;
/// Distance between adjacent hotbar cell origins.
pub const HOTBAR_SLOT_PITCH: i32 = 20;
/// Distance from the viewport bottom to the hotbar's top edge.
pub const HOTBAR_Y_OFFSET: i32 = 22;
/// Width of the moving selection sprite.
pub const HOTBAR_SELECT_W: i32 = 24;
/// Height of the moving selection sprite.
pub const HOTBAR_SELECT_H: i32 = 23;
/// Edge of the 16×16 item icon inside a cell.
pub const HOTBAR_ITEM_SIZE: i32 = 16;
/// Inset of the item icon from its cell origin.
pub const HOTBAR_ITEM_INSET: i32 = 3;
/// Width of the offhand slot sprite.
pub const OFFHAND_W: i32 = 29;
/// Height of the offhand slot sprite.
pub const OFFHAND_H: i32 = 24;
/// Distance from the viewport bottom to the offhand slot's top edge.
pub const OFFHAND_Y_OFFSET: i32 = 23;

/// Left edge of the hotbar strip: `w / 2 - 91`.
pub const fn hotbar_x(w: i32) -> i32 {
    w / 2 - HOTBAR_WIDTH / 2
}

/// Top edge of the hotbar strip: `h - 22`.
pub const fn hotbar_y(h: i32) -> i32 {
    h - HOTBAR_Y_OFFSET
}

/// Left edge of hotbar cell `index` (`w / 2 - 91 + i * 20`).
pub const fn hotbar_slot_x(w: i32, index: usize) -> i32 {
    hotbar_x(w) + HOTBAR_SLOT_PITCH * index as i32
}

/// Left edge of the 16×16 item icon in hotbar cell `index`
/// (`w / 2 - 90 + i * 20 + 2`).
pub const fn hotbar_item_x(w: i32, index: usize) -> i32 {
    hotbar_slot_x(w, index) + HOTBAR_ITEM_INSET
}

/// Top edge of every hotbar item icon: `h - 19`.
pub const fn hotbar_item_y(h: i32) -> i32 {
    h - HOTBAR_Y_OFFSET + HOTBAR_ITEM_INSET
}

/// Left edge of the selection sprite over hotbar cell `index`
/// (`w / 2 - 92 + i * 20`): the 24×23 frame overhangs its 20×20 cell by
/// one pixel on every side that has a border pixel.
pub const fn hotbar_select_x(w: i32, index: usize) -> i32 {
    hotbar_slot_x(w, index) - 1
}

/// Top edge of the selection sprite: `h - 23`.
pub const fn hotbar_select_y(h: i32) -> i32 {
    hotbar_y(h) - 1
}

/// Left edge of the offhand slot on the main-hand-agnostic default side:
/// `w / 2 - 120`, i.e. one full slot left of the hotbar strip.
pub const fn offhand_left_x(w: i32) -> i32 {
    hotbar_x(w) - OFFHAND_W
}

/// Left edge of the offhand slot when the player's main hand is the
/// left hand and vanilla mirrors it to `w / 2 + 91`.
pub const fn offhand_right_x(w: i32) -> i32 {
    hotbar_x(w) + HOTBAR_WIDTH
}

/// Top edge of the offhand slot: `h - 23`.
pub const fn offhand_y(h: i32) -> i32 {
    h - OFFHAND_Y_OFFSET
}

/// Width of the whole bottom block the HUD reserves: the offhand slot
/// plus the hotbar strip (`211`), measured from the offhand's left edge
/// to the hotbar's right edge.
pub const HOTBAR_BLOCK_WIDTH: i32 = OFFHAND_W + HOTBAR_WIDTH;

/// Logical px reserved along the bottom edge for the hotbar row.
pub const HOTBAR_RESERVE: i32 = HOTBAR_HEIGHT;

// ---------------------------------------------------------------------------
// Vitals (`Gui.renderPlayerHealth`, `Gui.renderArmor`, `Gui.renderFood`,
// `Gui.renderAir`)
// ---------------------------------------------------------------------------

/// Half the horizontal span between the left and right vitals columns:
/// hearts start at `w / 2 - 91`, food ends at `w / 2 + 91`.
pub const VITALS_HALF_SPAN: i32 = 91;
/// Distance from the viewport bottom to the hearts/food row:
/// `y = h - 39`.
pub const VITALS_BOTTOM_ROW_Y_OFFSET: i32 = 39;
/// Distance from the viewport bottom to the armor/air row:
/// `y = h - 49`.
pub const VITALS_UPPER_ROW_Y_OFFSET: i32 = 49;

/// Left edge of the hearts column: `w / 2 - 91`.
pub const fn vitals_left_x(w: i32) -> i32 {
    w / 2 - VITALS_HALF_SPAN
}

/// Exclusive right edge of the food column: `w / 2 + 91`.
pub const fn vitals_right_x(w: i32) -> i32 {
    w / 2 + VITALS_HALF_SPAN
}

/// Top edge of the hearts and food row: `h - 39`.
pub const fn vitals_bottom_row_y(h: i32) -> i32 {
    h - VITALS_BOTTOM_ROW_Y_OFFSET
}

/// Top edge of the armor and air row: `h - 49`.
pub const fn vitals_upper_row_y(h: i32) -> i32 {
    h - VITALS_UPPER_ROW_Y_OFFSET
}

// ---------------------------------------------------------------------------
// Experience bar (`Gui.renderExperienceBar`)
// ---------------------------------------------------------------------------

/// Width of the experience bar sprite.
pub const XP_BAR_WIDTH: i32 = 182;
/// Height of the experience bar sprite.
pub const XP_BAR_HEIGHT: i32 = 5;
/// Distance from the viewport bottom to the experience bar's top edge
/// (`h - 32 + 3`).
pub const XP_BAR_Y_OFFSET: i32 = 29;
/// The progress sprite is cropped against a 183 px source span, so a full
/// bar is `(progress * 183.0)` pixels wide inside the 182 px frame.
pub const XP_BAR_FILL_SPAN: f32 = 183.0;
/// Distance from the viewport bottom to the experience level number. It
/// shares the hearts/food band, in the empty centre of the screen between
/// the two vitals columns.
pub const XP_LEVEL_Y_OFFSET: i32 = VITALS_BOTTOM_ROW_Y_OFFSET;

/// Left edge of the experience bar: `w / 2 - 91`.
pub const fn xp_bar_x(w: i32) -> i32 {
    w / 2 - XP_BAR_WIDTH / 2
}

/// Top edge of the experience bar: `h - 29`.
pub const fn xp_bar_y(h: i32) -> i32 {
    h - XP_BAR_Y_OFFSET
}

/// Cropped progress width in pixels for `progress` in `0.0..=1.0`.
pub fn xp_bar_fill(progress: f32) -> i32 {
    let progress = if progress.is_finite() {
        progress.clamp(0.0, 1.0)
    } else {
        0.0
    };
    ((progress * XP_BAR_FILL_SPAN) as i32).min(XP_BAR_WIDTH)
}

// ---------------------------------------------------------------------------
// Bossbars (`BossHealthOverlay`)
// ---------------------------------------------------------------------------

/// Width of a boss bar sprite.
pub const BOSS_BAR_WIDTH: i32 = 182;
/// Height of a boss bar sprite.
pub const BOSS_BAR_HEIGHT: i32 = 5;
/// Top edge of the first (topmost) boss bar.
pub const BOSS_BAR_START_Y: i32 = 12;
/// Vertical distance between adjacent boss bars.
pub const BOSS_BAR_PITCH: i32 = 19;

/// Top edge of boss bar `index`.
pub const fn boss_bar_y(index: usize) -> i32 {
    BOSS_BAR_START_Y + index as i32 * BOSS_BAR_PITCH
}

/// How many boss bars fit above the upper-third cutoff. Vanilla keeps
/// stacking until the next bar's top edge would reach `h / 3`; there is
/// no fixed bar-count cap.
pub fn visible_boss_bars(count: usize, h: i32) -> usize {
    let limit = h / 3;
    let mut visible = 0usize;
    while visible < count && boss_bar_y(visible) < limit {
        visible += 1;
    }
    visible
}

/// Bottom edge of the painted boss bar stack: the top margin while no bar
/// is visible, otherwise the last visible bar's bottom plus its gap.
pub fn boss_bar_stack_bottom(count: usize, h: i32) -> i32 {
    match visible_boss_bars(count, h) {
        0 => 4,
        visible => boss_bar_y(visible - 1) + BOSS_BAR_HEIGHT + 2,
    }
}

// ---------------------------------------------------------------------------
// Title screen (`TitleScreen.init`)
// ---------------------------------------------------------------------------

/// Vanilla anchors the title button column below the wordmark at
/// `h / 4 + 48`.
pub const fn title_top(h: i32) -> i32 {
    h / 4 + 48
}

/// Vertical distance between adjacent title/pause button rows.
pub const TITLE_ROW_STEP: i32 = 24;
/// Height of a standard vanilla button.
pub const BUTTON_H: i32 = 20;
/// Width of a standard vanilla button.
pub const BUTTON_W: i32 = 200;
/// Vertical anchor vanilla gives the pause-menu button grid, expressed as
/// the fraction of the leftover space above the grid (`0.25`).
pub const PAUSE_ALIGN_Y: f32 = 0.25;
/// Vanilla draws the version line at the very bottom-left corner.
pub const FOOTER_X: i32 = 2;
/// Distance from the viewport bottom to the version line.
pub const FOOTER_Y_OFFSET: i32 = 10;

// ---------------------------------------------------------------------------
// Death screen (`DeathScreen`)
// ---------------------------------------------------------------------------

/// Top edge of the "You died!" title.
pub const DEATH_TITLE_Y: i32 = 30;
/// Top edge of the cause-of-death line.
pub const DEATH_CAUSE_Y: i32 = 85;
/// Top edge of the score line.
pub const DEATH_SCORE_Y: i32 = 100;
/// Width of both stacked death buttons.
pub const DEATH_BTN_W: i32 = 200;
/// Top edge of the primary (Respawn / Spectate World) button.
pub const fn death_btn1_y(h: i32) -> i32 {
    h / 4 + 72
}
/// Top edge of the secondary (Title Screen) button.
pub const fn death_btn2_y(h: i32) -> i32 {
    h / 4 + 96
}
/// Client ticks both death buttons stay inert after the screen appears,
/// so a click already in flight when the player died cannot respawn them.
pub const DEATH_ENABLE_DELAY_TICKS: u32 = 20;

// ---------------------------------------------------------------------------
// Creative inventory (`CreativeModeInventoryScreen`)
// ---------------------------------------------------------------------------

/// Background width of the creative panel.
pub const CREATIVE_IMAGE_W: i32 = 195;
/// Background height of the creative panel.
pub const CREATIVE_IMAGE_H: i32 = 136;
/// Left inset of the 9×5 item grid inside the panel.
pub const CREATIVE_GRID_X: i32 = 9;
/// Top inset of the 9×5 item grid inside the panel.
pub const CREATIVE_GRID_Y: i32 = 18;
/// Left inset of the player hotbar row inside the panel.
pub const CREATIVE_HOTBAR_X: i32 = 9;
/// Top inset of the player hotbar row inside the panel.
pub const CREATIVE_HOTBAR_Y: i32 = 112;
/// Edge of one creative inventory slot.
pub const CREATIVE_SLOT: i32 = 18;
/// Columns of the creative item grid.
pub const CREATIVE_GRID_COLS: usize = 9;
/// Visible rows of the creative item grid.
pub const CREATIVE_GRID_ROWS: usize = 5;

// ---------------------------------------------------------------------------
// Font (`Font`)
// ---------------------------------------------------------------------------

/// Advance between consecutive text lines. Vanilla's font is 9 px tall per
/// line, so stacked labels never touch; single-line heights keep using
/// [`crate::draw::GLYPH_SIZE`] because this renderer's glyphs are 8 px.
pub const FONT_LINE_HEIGHT: u32 = 9;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hotbar_centres_and_anchors_at_the_vanilla_offsets() {
        for w in [320, 640, 854, 1280, 1920] {
            let x = hotbar_x(w);
            assert_eq!(x, w / 2 - 91);
            // The strip is horizontally centred to within the odd-width
            // half pixel vanilla itself ignores.
            assert!((x + HOTBAR_WIDTH / 2 - w / 2).abs() <= 1);
            assert_eq!(hotbar_slot_x(w, 0), x);
            assert_eq!(hotbar_slot_x(w, 8), x + 160);
            assert_eq!(hotbar_item_x(w, 0), w / 2 - 90 + 2);
            assert_eq!(hotbar_item_x(w, 8), w / 2 - 90 + 8 * 20 + 2);
            assert_eq!(hotbar_select_x(w, 0), w / 2 - 92);
            assert_eq!(offhand_left_x(w), w / 2 - 120);
            assert_eq!(offhand_right_x(w), w / 2 + 91);
            assert_eq!(vitals_left_x(w), w / 2 - 91);
            assert_eq!(vitals_right_x(w), w / 2 + 91);
            assert_eq!(xp_bar_x(w), w / 2 - 91);
        }
        assert_eq!(hotbar_y(360), 338);
        assert_eq!(hotbar_select_y(360), 337);
        assert_eq!(hotbar_item_y(360), 341);
        assert_eq!(offhand_y(360), 337);
        assert_eq!(vitals_bottom_row_y(360), 321);
        assert_eq!(vitals_upper_row_y(360), 311);
        assert_eq!(xp_bar_y(360), 331);
        // The reserved bottom block is offhand + slot pitch + hotbar.
        assert_eq!(HOTBAR_BLOCK_WIDTH, 211);
        assert_eq!(HOTBAR_RESERVE, 22);
    }

    #[test]
    fn vitals_rows_sit_above_the_hotbar_and_below_the_xp_bar() {
        let h = 720;
        // hearts/food (9 px tall) bottom out one pixel above the XP bar.
        assert!(vitals_bottom_row_y(h) + 9 < xp_bar_y(h));
        // The XP bar clears the hotbar strip.
        assert!(xp_bar_y(h) + XP_BAR_HEIGHT <= hotbar_y(h));
        assert_eq!(vitals_bottom_row_y(h), hotbar_y(h) - 17);
    }

    #[test]
    fn xp_fill_crops_against_the_183px_source_span() {
        assert_eq!(xp_bar_fill(0.0), 0);
        assert_eq!(xp_bar_fill(1.0), XP_BAR_WIDTH);
        assert_eq!(xp_bar_fill(0.5), 91);
        assert_eq!(xp_bar_fill(f32::NAN), 0);
        assert_eq!(xp_bar_fill(-1.0), 0);
        assert_eq!(xp_bar_fill(4.0), XP_BAR_WIDTH);
    }

    #[test]
    fn bossbars_stack_until_the_upper_third_without_a_bar_cap() {
        assert_eq!(boss_bar_y(0), 12);
        assert_eq!(boss_bar_y(1), 31);
        assert_eq!(boss_bar_y(2), 50);
        // 720 / 3 = 240 -> bars at 12, 31, ... up to 221 all fit.
        assert_eq!(visible_boss_bars(20, 720), 12);
        assert_eq!(visible_boss_bars(3, 720), 3);
        // A 60-logical-px viewport only has room above y = 20.
        assert_eq!(visible_boss_bars(5, 60), 1);
        assert_eq!(boss_bar_stack_bottom(0, 720), 4);
        assert_eq!(boss_bar_stack_bottom(1, 720), 19);
        assert_eq!(boss_bar_stack_bottom(3, 720), 57);
    }

    #[test]
    fn screen_anchors_are_the_vanilla_offsets() {
        assert_eq!(title_top(720), 228);
        assert_eq!(title_top(360), 138);
        assert_eq!(death_btn1_y(720), 252);
        assert_eq!(death_btn2_y(720), 276);
        assert_eq!(DEATH_BTN_W, 200);
        assert_eq!(DEATH_ENABLE_DELAY_TICKS, 20);
        assert_eq!(title_top(720) - 48, 720 / 4);
        assert_eq!(death_btn1_y(720) - 72, 720 / 4);
        assert_eq!(FONT_LINE_HEIGHT, 9);
    }
}
