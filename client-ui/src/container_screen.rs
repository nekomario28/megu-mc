// SPDX-License-Identifier: LGPL-3.0-or-later
//! Declarative Vanilla container/menu screen engine for protocol 767.
//!
//! One [`MenuScreenSchema`] table covers every `minecraft:menu` registry id
//! (`0..=24`) and one [`ContainerScreenState`] implements the deterministic
//! click-window model used by the Vanilla client. Rendering is a pure
//! [`UiDrawList`] producer: centered panel, slots, items, progress bars,
//! cursor, buttons, and a viewport-clamped tooltip.

use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet};
use std::ops::Range;
use std::time::{Duration, Instant};

use crate::conflict_widget::ConflictWidget;
use crate::draw::{palette, text_width, GuiSpriteAtlas, GuiSpriteUv, UiColor, UiCommand, UiDrawList, UiRect};
use crate::state::{tr, UiLang};
use crate::vanilla_widgets::{draw_vanilla_arrow, draw_vanilla_ghost_item, tooltip_row_budget, wrap_text_px};

pub const VANILLA_MENU_COUNT: usize = 25;
pub const SLOT_SIZE: i32 = 18;
const CONTENT_PAD: i32 = 4;
const EMPTY_SLOT_INDEX: i16 = -999;
const PLAYER_INVENTORY_COLS: i16 = 9;
const PLAYER_INVENTORY_ROWS: i16 = 3;
const PLAYER_INVENTORY_SIZE: i16 = PLAYER_INVENTORY_COLS * PLAYER_INVENTORY_ROWS;
const PLAYER_HOTBAR_GAP: i32 = 4;
const PLAYER_HOTBAR_OFFSET: i32 = PLAYER_INVENTORY_ROWS as i32 * SLOT_SIZE + PLAYER_HOTBAR_GAP;

/// Row-major slot-grid geometry in unscaled GUI pixels.
///
/// Every cell uses the vanilla 18 px pitch: index `row * cols + col` lands
/// at `(x0 + 18 * col, y0 + 18 * row)`. Container schemas, player-inventory
/// slots, hit testing, and rendering all derive from this one formula.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SlotGridGeometry {
    pub x0: i32,
    pub y0: i32,
    pub cols: usize,
}

impl SlotGridGeometry {
    pub const fn new(x0: i32, y0: i32, cols: usize) -> Self {
        Self { x0, y0, cols }
    }

    /// GUI-space origin of the cell at `(col, row)`.
    pub const fn cell_origin(self, col: usize, row: usize) -> (i32, i32) {
        (
            self.x0 + col as i32 * SLOT_SIZE,
            self.y0 + row as i32 * SLOT_SIZE,
        )
    }

    /// GUI-space origin of row-major slot `index`.
    pub const fn slot_origin(self, index: usize) -> (i32, i32) {
        let cols = if self.cols == 0 { 1 } else { self.cols };
        self.cell_origin(index % cols, index / cols)
    }

    /// GUI-space rectangle of row-major slot `index`.
    pub const fn slot_rect(self, index: usize) -> UiRect {
        let (x, y) = self.slot_origin(index);
        UiRect::new(x, y, SLOT_SIZE as u32, SLOT_SIZE as u32)
    }
}

/// One textured chest blit: destination rectangle plus source crop in
/// pixels of the original (possibly high-DPI) `generic_54` sheet.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChestSkinBlit {
    pub dest: UiRect,
    pub source_x: u32,
    pub source_y: u32,
    pub source_w: u32,
    pub source_h: u32,
}

/// Exact vanilla 1.21.1 `generic_54.png` chest-skin crop geometry.
///
/// The sheet is 176×222 logical px. A chest with `rows` container rows uses
/// the top crop `17 + rows * 18`, then the shared 96 px player-inventory
/// footer beginning at source y=126. Integral high-DPI sheets scale only the
/// source crop coordinates; destination GUI geometry remains logical pixels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChestSkinGeometry {
    rows: u32,
    source_scale: u32,
}

impl ChestSkinGeometry {
    pub const SHEET_WIDTH: u32 = 176;
    pub const SHEET_HEIGHT: u32 = 222;
    pub const TOP_BASE_HEIGHT: u32 = 17;
    pub const ROW_HEIGHT: u32 = SLOT_SIZE as u32;
    pub const FOOTER_SOURCE_Y: u32 = 126;
    pub const FOOTER_HEIGHT: u32 = 96;
    pub const PANEL_BASE_HEIGHT: u32 = 114;
    pub const INVENTORY_ORIGIN_BASE_Y: u32 = 31;

    pub const fn new(rows: u32, source_scale: u32) -> Self {
        Self { rows, source_scale }
    }

    /// Validate a sheet and construct geometry for `rows` (`1..=6`). The
    /// scale must be integral and identical in both dimensions, matching the
    /// packed vanilla high-DPI texture contract.
    pub const fn from_sheet(rows: u32, sheet_width: u32, sheet_height: u32) -> Option<Self> {
        let source_scale = Self::source_scale_for_sheet(sheet_width, sheet_height);
        if rows >= 1 && rows <= 6 && source_scale > 0 {
            Some(Self::new(rows, source_scale))
        } else {
            None
        }
    }

    /// Integral scale of a `generic_54` sheet, or zero when the dimensions
    /// are not the vanilla aspect ratio at an exact integer multiple.
    pub const fn source_scale_for_sheet(sheet_width: u32, sheet_height: u32) -> u32 {
        if sheet_width >= Self::SHEET_WIDTH
            && sheet_height >= Self::SHEET_HEIGHT
            && sheet_width.is_multiple_of(Self::SHEET_WIDTH)
            && sheet_height.is_multiple_of(Self::SHEET_HEIGHT)
            && sheet_width / Self::SHEET_WIDTH == sheet_height / Self::SHEET_HEIGHT
        {
            sheet_width / Self::SHEET_WIDTH
        } else {
            0
        }
    }

    /// Chest schema panel height in GUI px (`114 + rows * 18`).
    pub const fn panel_height(rows: u32) -> u32 {
        Self::PANEL_BASE_HEIGHT + rows * Self::ROW_HEIGHT
    }

    /// Player-inventory origin Y in a chest schema (`31 + rows * 18`).
    pub const fn inventory_origin_y(rows: u32) -> i16 {
        (Self::INVENTORY_ORIGIN_BASE_Y + rows * Self::ROW_HEIGHT) as i16
    }

    /// Logical-pixel height of the chest-top crop (`17 + rows * 18`).
    pub const fn top_height(self) -> u32 {
        Self::TOP_BASE_HEIGHT + self.rows * Self::ROW_HEIGHT
    }

    /// Logical-pixel height of the shared player-inventory footer crop.
    pub const fn footer_height(self) -> u32 {
        Self::FOOTER_HEIGHT
    }

    /// The exact two destination rectangles and scaled source crops.
    pub const fn blits(self, panel: UiRect) -> [ChestSkinBlit; 2] {
        let top_h = self.top_height();
        let source_w = Self::SHEET_WIDTH * self.source_scale;
        [
            ChestSkinBlit {
                dest: UiRect::new(panel.x, panel.y, panel.w, top_h),
                source_x: 0,
                source_y: 0,
                source_w,
                source_h: top_h * self.source_scale,
            },
            ChestSkinBlit {
                dest: UiRect::new(
                    panel.x,
                    panel.y + top_h as i32,
                    panel.w,
                    Self::FOOTER_HEIGHT,
                ),
                source_x: 0,
                source_y: Self::FOOTER_SOURCE_Y * self.source_scale,
                source_w,
                source_h: Self::FOOTER_HEIGHT * self.source_scale,
            },
        ]
    }
}

/// Canonical vanilla GUI texture key for each protocol-767 workstation menu.
/// Generic chest sizes share `container/generic_54`; the window-0 inventory
/// uses `u32::MAX` for compatibility with the GUI atlas mapping.
pub const fn container_skin_sprite_key(menu_id: u32) -> &'static str {
    match menu_id {
        0..=5 => "container/generic_54",
        6 => "container/dispenser",
        7 => "container/crafter",
        8 => "container/anvil",
        9 => "container/beacon",
        10 => "container/blast_furnace",
        11 => "container/brewing_stand",
        12 => "container/crafting_table",
        13 => "container/enchanting_table",
        14 => "container/furnace",
        15 => "container/grindstone",
        16 => "container/hopper",
        17 => "container/lectern",
        18 => "container/loom",
        19 => "container/villager",
        20 => "container/shulker_box",
        21 => "container/smithing",
        22 => "container/smoker",
        23 => "container/cartography_table",
        24 => "container/stonecutter",
        u32::MAX => "container/inventory",
        _ => "",
    }
}

/// Keyboard shortcuts supported by vanilla container slots.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ContainerSlotHotkey {
    HotbarSwap(u8),
    OffhandSwap,
    DropOne,
    DropStack,
    DoubleClickGather,
}

/// Semantic role of a container slot. `Output`/`Locked` slots reject
/// placement but can still be taken when the cursor is empty.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SlotKind {
    Normal,
    Input,
    Fuel,
    Output,
    Armor,
    Offhand,
    Locked,
}

/// One protocol slot's menu index and GUI-space origin.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SlotDef {
    pub index: i16,
    pub x: i32,
    pub y: i32,
    pub kind: SlotKind,
}

impl SlotDef {
    pub const fn new(index: i16, x: i32, y: i32, kind: SlotKind) -> Self {
        Self { index, x, y, kind }
    }

    const EMPTY: Self = Self::new(0, 0, 0, SlotKind::Normal);
}

/// A clickable menu button (fixed or dynamic recipe/pattern list).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MenuButtonDef {
    pub id: i32,
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
    pub label: &'static str,
    /// `true` for scrollable recipe/pattern selectors whose valid ids are
    /// supplied at runtime (`>= 0`).
    pub dynamic: bool,
}

impl MenuButtonDef {
    pub const fn new(id: i32, x: i32, y: i32, w: i32, h: i32, label: &'static str) -> Self {
        Self {
            id,
            x,
            y,
            w,
            h,
            label,
            dynamic: false,
        }
    }

    pub const fn dynamic(x: i32, y: i32, w: i32, h: i32, label: &'static str) -> Self {
        Self {
            id: -1,
            x,
            y,
            w,
            h,
            label,
            dynamic: true,
        }
    }
}

/// Progress indicator bound to a container property id.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum PropertyBarKind {
    Flame,
    Arrow,
    Bubbles,
    Fuel,
    Levels,
    Costs,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PropertyBarDef {
    pub property: i16,
    pub secondary_property: i16,
    pub kind: PropertyBarKind,
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
    pub label: &'static str,
}

/// Declarative layout/behavior for one `minecraft:menu` registry id.
#[derive(Clone, Copy, Debug)]
pub struct MenuScreenSchema {
    pub menu_id: u32,
    pub name: &'static str,
    pub panel_w: f32,
    pub panel_h: f32,
    pub container_slots: &'static [SlotDef],
    pub has_player_inventory: bool,
    /// First menu slot index occupied by the standard 27+9 player inventory.
    pub inventory_start: i16,
    /// Total menu slot count (including non-contiguous result slots).
    pub slot_count: i16,
    /// Player-inventory origin in GUI space (`-1` when absent).
    pub inventory_origin_x: i16,
    pub inventory_origin_y: i16,
    pub buttons: &'static [MenuButtonDef],
    pub property_bars: &'static [PropertyBarDef],
}

const fn grid<const N: usize, const COLS: usize>(x0: i32, y0: i32) -> [SlotDef; N] {
    let geometry = SlotGridGeometry::new(x0, y0, COLS);
    let mut out = [SlotDef::EMPTY; N];
    let mut i = 0usize;
    while i < N {
        let (x, y) = geometry.slot_origin(i);
        out[i] = SlotDef::new(i as i16, x, y, SlotKind::Normal);
        i += 1;
    }
    out
}

const fn slots<const N: usize>(defs: [SlotDef; N]) -> [SlotDef; N] {
    defs
}

static GENERIC_9X1_SLOTS: [SlotDef; 9] = grid::<9, 9>(8, 18);
static GENERIC_9X2_SLOTS: [SlotDef; 18] = grid::<18, 9>(8, 18);
static GENERIC_9X3_SLOTS: [SlotDef; 27] = grid::<27, 9>(8, 18);
static GENERIC_9X4_SLOTS: [SlotDef; 36] = grid::<36, 9>(8, 18);
static GENERIC_9X5_SLOTS: [SlotDef; 45] = grid::<45, 9>(8, 18);
static GENERIC_9X6_SLOTS: [SlotDef; 54] = grid::<54, 9>(8, 18);
static GENERIC_3X3_SLOTS: [SlotDef; 9] = grid::<9, 3>(62, 17);
const CRAFTER_GRID: [SlotDef; 9] = grid::<9, 3>(26, 17);

const fn crafter_slots() -> [SlotDef; 10] {
    let mut out = [SlotDef::EMPTY; 10];
    let mut i = 0usize;
    while i < 9 {
        out[i] = CRAFTER_GRID[i];
        i += 1;
    }
    out[9] = SlotDef::new(45, 134, 35, SlotKind::Output);
    out
}

static CRAFTER_SLOTS: [SlotDef; 10] = crafter_slots();
static ANVIL_SLOTS: [SlotDef; 3] = slots([
    SlotDef::new(0, 27, 47, SlotKind::Input),
    SlotDef::new(1, 76, 47, SlotKind::Input),
    SlotDef::new(2, 134, 47, SlotKind::Output),
]);
static BEACON_SLOTS: [SlotDef; 1] = slots([SlotDef::new(0, 136, 110, SlotKind::Input)]);
static FURNACE_SLOTS: [SlotDef; 3] = slots([
    SlotDef::new(0, 56, 17, SlotKind::Input),
    SlotDef::new(1, 56, 53, SlotKind::Fuel),
    SlotDef::new(2, 116, 35, SlotKind::Output),
]);
static BREWING_SLOTS: [SlotDef; 5] = slots([
    SlotDef::new(0, 56, 51, SlotKind::Input),
    SlotDef::new(1, 79, 58, SlotKind::Input),
    SlotDef::new(2, 102, 51, SlotKind::Input),
    SlotDef::new(3, 79, 17, SlotKind::Input),
    SlotDef::new(4, 17, 17, SlotKind::Fuel),
]);
static CRAFTING_SLOTS: [SlotDef; 10] = slots([
    SlotDef::new(0, 124, 35, SlotKind::Output),
    SlotDef::new(1, 30, 17, SlotKind::Input),
    SlotDef::new(2, 48, 17, SlotKind::Input),
    SlotDef::new(3, 66, 17, SlotKind::Input),
    SlotDef::new(4, 30, 35, SlotKind::Input),
    SlotDef::new(5, 48, 35, SlotKind::Input),
    SlotDef::new(6, 66, 35, SlotKind::Input),
    SlotDef::new(7, 30, 53, SlotKind::Input),
    SlotDef::new(8, 48, 53, SlotKind::Input),
    SlotDef::new(9, 66, 53, SlotKind::Input),
]);
static ENCHANT_SLOTS: [SlotDef; 2] = slots([
    SlotDef::new(0, 15, 47, SlotKind::Input),
    SlotDef::new(1, 35, 47, SlotKind::Input),
]);
static GRINDSTONE_SLOTS: [SlotDef; 3] = slots([
    SlotDef::new(0, 49, 19, SlotKind::Input),
    SlotDef::new(1, 49, 40, SlotKind::Input),
    SlotDef::new(2, 129, 34, SlotKind::Output),
]);
static HOPPER_SLOTS: [SlotDef; 5] = slots([
    SlotDef::new(0, 44, 20, SlotKind::Normal),
    SlotDef::new(1, 62, 20, SlotKind::Normal),
    SlotDef::new(2, 80, 20, SlotKind::Normal),
    SlotDef::new(3, 98, 20, SlotKind::Normal),
    SlotDef::new(4, 116, 20, SlotKind::Normal),
]);
static LECTERN_SLOTS: [SlotDef; 1] = slots([SlotDef::new(0, 0, 0, SlotKind::Input)]);
static LOOM_SLOTS: [SlotDef; 4] = slots([
    SlotDef::new(0, 13, 26, SlotKind::Input),
    SlotDef::new(1, 33, 26, SlotKind::Input),
    SlotDef::new(2, 23, 45, SlotKind::Input),
    SlotDef::new(3, 143, 57, SlotKind::Output),
]);
static MERCHANT_SLOTS: [SlotDef; 3] = slots([
    SlotDef::new(0, 136, 37, SlotKind::Input),
    SlotDef::new(1, 162, 37, SlotKind::Input),
    SlotDef::new(2, 220, 37, SlotKind::Output),
]);
static SHULKER_SLOTS: [SlotDef; 27] = grid::<27, 9>(8, 18);
static SMITHING_SLOTS: [SlotDef; 4] = slots([
    SlotDef::new(0, 8, 48, SlotKind::Input),
    SlotDef::new(1, 26, 48, SlotKind::Input),
    SlotDef::new(2, 44, 48, SlotKind::Input),
    SlotDef::new(3, 98, 48, SlotKind::Output),
]);
static STONECUTTER_SLOTS: [SlotDef; 2] = slots([
    SlotDef::new(0, 20, 33, SlotKind::Input),
    SlotDef::new(1, 143, 33, SlotKind::Output),
]);
static CARTOGRAPHY_SLOTS: [SlotDef; 3] = slots([
    SlotDef::new(0, 15, 15, SlotKind::Input),
    SlotDef::new(1, 15, 52, SlotKind::Input),
    SlotDef::new(2, 145, 39, SlotKind::Output),
]);

/// Beacon effect picker: `(effect id, x, y, pyramid tier, en, ja)`.
///
/// `effect` is the 0-based protocol-767 `minecraft:mob_effect` registry id
/// (Speed=0, Haste=2, Strength=4, Jump=7, Regeneration=9, Resistance=10);
/// `-1` marks the tier-4 secondary mirror of the chosen primary effect.
const BEACON_CONTROLS: [(i32, i32, i32, i16, &str, &str); 7] = [
    (0, 24, 54, 1, "Speed", "移動速度"),
    (2, 72, 54, 1, "Haste", "採掘速度"),
    (10, 24, 72, 2, "Resist", "耐性"),
    (7, 72, 72, 2, "Jump", "跳躍"),
    (4, 24, 90, 3, "Strength", "攻撃力"),
    (9, 24, 108, 4, "Regen", "再生"),
    (-1, 72, 108, 4, "Secondary", "第2効果"),
];
const BEACON_CONTROL_W: i32 = 44;
const BEACON_CONTROL_H: i32 = 14;
/// Beacon footer buttons in content-box space.
const BEACON_CONFIRM_RECT: (i32, i32, i32, i32) = (158, 72, 30, 14);
const BEACON_CANCEL_RECT: (i32, i32, i32, i32) = (158, 90, 30, 14);

const NO_BUTTONS: &[MenuButtonDef] = &[];
const NO_BARS: &[PropertyBarDef] = &[];
const ENCHANT_BUTTONS: &[MenuButtonDef] = &[
    MenuButtonDef::new(0, 60, 14, 108, 19, "I"),
    MenuButtonDef::new(1, 60, 33, 108, 19, "II"),
    MenuButtonDef::new(2, 60, 52, 108, 19, "III"),
];
const LECTERN_BUTTONS: &[MenuButtonDef] = &[
    MenuButtonDef::new(1, 8, 28, 22, 12, "<"),
    MenuButtonDef::new(2, 34, 28, 22, 12, ">"),
    MenuButtonDef::new(3, 8, 44, 48, 12, "take"),
];
const LOOM_BUTTONS: &[MenuButtonDef] = &[];
const STONECUTTER_BUTTONS: &[MenuButtonDef] = &[];
const CRAFTER_BUTTONS: &[MenuButtonDef] = &[MenuButtonDef::new(0, 134, 54, 24, 12, "auto")];
const BEACON_BUTTONS: &[MenuButtonDef] = &[MenuButtonDef::new(0, 158, 72, 30, 14, "Confirm")];
const FURNACE_BARS: &[PropertyBarDef] = &[
    PropertyBarDef {
        property: 0,
        secondary_property: 1,
        kind: PropertyBarKind::Flame,
        x: 56,
        y: 36,
        w: 14,
        h: 14,
        label: "flame",
    },
    PropertyBarDef {
        property: 2,
        secondary_property: 3,
        kind: PropertyBarKind::Arrow,
        x: 79,
        y: 34,
        w: 24,
        h: 17,
        label: "cook",
    },
];
const BREWING_BARS: &[PropertyBarDef] = &[
    PropertyBarDef {
        property: 0,
        secondary_property: 0,
        kind: PropertyBarKind::Bubbles,
        x: 56,
        y: 72,
        w: 54,
        h: 12,
        label: "brew",
    },
    PropertyBarDef {
        property: 1,
        secondary_property: 0,
        kind: PropertyBarKind::Fuel,
        x: 17,
        y: 38,
        w: 12,
        h: 16,
        label: "fuel",
    },
];
const fn chest_schema(
    menu_id: u32,
    name: &'static str,
    rows: usize,
    slots: &'static [SlotDef],
) -> MenuScreenSchema {
    let container = rows * 9;
    MenuScreenSchema {
        menu_id,
        name,
        panel_w: ChestSkinGeometry::SHEET_WIDTH as f32,
        panel_h: ChestSkinGeometry::panel_height(rows as u32) as f32,
        container_slots: slots,
        has_player_inventory: true,
        inventory_start: container as i16,
        slot_count: (container + 36) as i16,
        inventory_origin_x: 8,
        inventory_origin_y: ChestSkinGeometry::inventory_origin_y(rows as u32),
        buttons: NO_BUTTONS,
        property_bars: NO_BARS,
    }
}

const fn standard_schema(
    menu_id: u32,
    name: &'static str,
    panel_w: f32,
    panel_h: f32,
    slots: &'static [SlotDef],
    inventory_start: i16,
    slot_count: i16,
) -> MenuScreenSchema {
    MenuScreenSchema {
        menu_id,
        name,
        panel_w,
        panel_h,
        container_slots: slots,
        has_player_inventory: inventory_start >= 0,
        inventory_start,
        slot_count,
        inventory_origin_x: 8,
        inventory_origin_y: 84,
        buttons: NO_BUTTONS,
        property_bars: NO_BARS,
    }
}

/// All Protocol 767 `minecraft:menu` schemas in registry order (`0..=24`).
pub static ALL_VANILLA_MENUS: [MenuScreenSchema; VANILLA_MENU_COUNT] = [
    chest_schema(0, "minecraft:generic_9x1", 1, &GENERIC_9X1_SLOTS),
    chest_schema(1, "minecraft:generic_9x2", 2, &GENERIC_9X2_SLOTS),
    chest_schema(2, "minecraft:generic_9x3", 3, &GENERIC_9X3_SLOTS),
    chest_schema(3, "minecraft:generic_9x4", 4, &GENERIC_9X4_SLOTS),
    chest_schema(4, "minecraft:generic_9x5", 5, &GENERIC_9X5_SLOTS),
    chest_schema(5, "minecraft:generic_9x6", 6, &GENERIC_9X6_SLOTS),
    standard_schema(
        6,
        "minecraft:generic_3x3",
        176.0,
        166.0,
        &GENERIC_3X3_SLOTS,
        9,
        45,
    ),
    {
        let mut s = standard_schema(
            7,
            "minecraft:crafter_3x3",
            176.0,
            166.0,
            &CRAFTER_SLOTS,
            9,
            46,
        );
        s.buttons = CRAFTER_BUTTONS;
        s
    },
    standard_schema(8, "minecraft:anvil", 176.0, 166.0, &ANVIL_SLOTS, 3, 39),
    {
        let mut s = standard_schema(9, "minecraft:beacon", 230.0, 219.0, &BEACON_SLOTS, 1, 37);
        s.inventory_origin_x = 36;
        s.inventory_origin_y = 137;
        s.buttons = BEACON_BUTTONS;
        s
    },
    {
        let mut s = standard_schema(
            10,
            "minecraft:blast_furnace",
            176.0,
            166.0,
            &FURNACE_SLOTS,
            3,
            39,
        );
        s.property_bars = FURNACE_BARS;
        s
    },
    {
        let mut s = standard_schema(
            11,
            "minecraft:brewing_stand",
            176.0,
            166.0,
            &BREWING_SLOTS,
            5,
            41,
        );
        s.property_bars = BREWING_BARS;
        s
    },
    standard_schema(
        12,
        "minecraft:crafting",
        176.0,
        166.0,
        &CRAFTING_SLOTS,
        10,
        46,
    ),
    {
        let mut s = standard_schema(
            13,
            "minecraft:enchantment",
            176.0,
            166.0,
            &ENCHANT_SLOTS,
            2,
            38,
        );
        s.buttons = ENCHANT_BUTTONS;
        s.property_bars = NO_BARS;
        s
    },
    {
        let mut s = standard_schema(14, "minecraft:furnace", 176.0, 166.0, &FURNACE_SLOTS, 3, 39);
        s.property_bars = FURNACE_BARS;
        s
    },
    standard_schema(
        15,
        "minecraft:grindstone",
        176.0,
        166.0,
        &GRINDSTONE_SLOTS,
        3,
        39,
    ),
    {
        let mut s = standard_schema(16, "minecraft:hopper", 176.0, 133.0, &HOPPER_SLOTS, 5, 41);
        s.inventory_origin_y = 51;
        s
    },
    {
        let mut s = standard_schema(17, "minecraft:lectern", 120.0, 96.0, &LECTERN_SLOTS, -1, 1);
        s.inventory_origin_x = -1;
        s.inventory_origin_y = -1;
        s.buttons = LECTERN_BUTTONS;
        s
    },
    {
        let mut s = standard_schema(18, "minecraft:loom", 256.0, 166.0, &LOOM_SLOTS, 4, 40);
        s.buttons = LOOM_BUTTONS;
        s
    },
    {
        let mut s = standard_schema(
            19,
            "minecraft:merchant",
            276.0,
            166.0,
            &MERCHANT_SLOTS,
            3,
            39,
        );
        s.inventory_origin_x = 108;
        s
    },
    standard_schema(
        20,
        "minecraft:shulker_box",
        176.0,
        166.0,
        &SHULKER_SLOTS,
        27,
        63,
    ),
    standard_schema(
        21,
        "minecraft:smithing",
        176.0,
        166.0,
        &SMITHING_SLOTS,
        4,
        40,
    ),
    {
        let mut s = standard_schema(22, "minecraft:smoker", 176.0, 166.0, &FURNACE_SLOTS, 3, 39);
        s.property_bars = FURNACE_BARS;
        s
    },
    standard_schema(
        23,
        "minecraft:cartography_table",
        176.0,
        166.0,
        &CARTOGRAPHY_SLOTS,
        3,
        39,
    ),
    {
        let mut s = standard_schema(
            24,
            "minecraft:stonecutter",
            176.0,
            166.0,
            &STONECUTTER_SLOTS,
            2,
            38,
        );
        s.buttons = STONECUTTER_BUTTONS;
        s
    },
];

/// Look up a schema by `minecraft:menu` registry id (`0..=24`).
pub fn menu_schema(menu_id: u32) -> Option<&'static MenuScreenSchema> {
    ALL_VANILLA_MENUS
        .get(menu_id as usize)
        .filter(|s| s.menu_id == menu_id)
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct UiEnchantment {
    pub name: String,
    pub level: i32,
}

/// UI-facing item stack. `count == 0` is the empty stack.
#[derive(Clone, Debug, PartialEq)]
pub struct UiItemStack {
    pub item_id: u32,
    pub count: u8,
    /// Namespaced item id (`minecraft:stone`) when the host resolved the
    /// numeric `item_id` — feeds real `ItemIcon` textures; empty keeps
    /// the hashed swatch fallback.
    pub namespaced_id: String,
    pub max_stack: u8,
    /// Remaining durability; `-1` means not damageable.
    pub durability: i32,
    /// Maximum durability for the bar; `0` disables it.
    pub max_durability: i32,
    pub display_name: String,
    pub lore: Vec<String>,
    pub enchantments: Vec<UiEnchantment>,
    pub rarity_color: UiColor,
}

impl Default for UiItemStack {
    fn default() -> Self {
        Self::empty()
    }
}

impl UiItemStack {
    pub fn new(item_id: u32, count: u8) -> Self {
        Self {
            item_id,
            count,
            namespaced_id: String::new(),
            max_stack: 64,
            durability: -1,
            max_durability: 0,
            display_name: format!("item #{item_id}"),
            lore: Vec::new(),
            enchantments: Vec::new(),
            rarity_color: palette::TEXT,
        }
    }

    pub fn empty() -> Self {
        Self {
            item_id: 0,
            count: 0,
            ..Self::new(0, 0)
        }
    }

    pub fn is_empty(&self) -> bool {
        self.count == 0 || self.item_id == 0
    }

    /// Attach the namespaced item id (icon texture lookup key).
    pub fn with_namespaced_id(mut self, id: impl Into<String>) -> Self {
        self.namespaced_id = id.into();
        self
    }

    pub fn with_name(mut self, name: impl Into<String>) -> Self {
        self.display_name = name.into();
        self
    }

    pub fn with_max_stack(mut self, max_stack: u8) -> Self {
        self.max_stack = max_stack.max(1);
        self
    }

    pub fn with_durability(mut self, durability: i32, max_durability: i32) -> Self {
        self.durability = durability;
        self.max_durability = max_durability.max(0);
        self
    }

    pub fn with_lore(mut self, lore: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.lore = lore.into_iter().map(Into::into).collect();
        self
    }

    pub fn with_enchantment(mut self, name: impl Into<String>, level: i32) -> Self {
        self.enchantments.push(UiEnchantment {
            name: name.into(),
            level,
        });
        self
    }

    pub fn with_rarity(mut self, color: UiColor) -> Self {
        self.rarity_color = color;
        self
    }

    fn same_stackable(&self, other: &Self) -> bool {
        !self.is_empty()
            && !other.is_empty()
            && self.item_id == other.item_id
            && self.max_stack == other.max_stack
            && self.durability == other.durability
            && self.max_durability == other.max_durability
            && self.display_name == other.display_name
            && self.lore == other.lore
            && self.enchantments == other.enchantments
            && self.rarity_color == other.rarity_color
    }
}

/// Drag-distribute state for click mode `5` (QUICK_CRAFT).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct DragState {
    /// Distribution kind: `0` = even split (left drag), `1` = one item per
    /// slot (right drag), `2` = creative clone (middle drag). Maps to the
    /// protocol button groups `0/1/2`, `4/5/6`, `8/9/10`.
    pub button: u8,
    /// Unique admittable slots collected so far, in hover order.
    pub slots: Vec<i16>,
    /// Slot under the pointer when the drag began (`-999` = outside).
    pub press_slot: i16,
    /// Mouse button that began the drag (`0` left, `1` right, `2` middle).
    pub press_button: i8,
}

/// One changed slot inside a [`ClickWindowAction`].
#[derive(Clone, Debug, PartialEq)]
pub struct SlotStackChange {
    pub slot: i16,
    pub stack: UiItemStack,
}

/// Exact serverbound `container_click` payload model (`0x0e`).
#[derive(Clone, Debug, PartialEq)]
pub struct ClickWindowAction {
    pub window_id: u8,
    pub state_id: i32,
    pub slot: i16,
    pub button: i8,
    pub mode: u8,
    pub changed_slots: Vec<SlotStackChange>,
    pub carried_item: Option<UiItemStack>,
}

/// Exact serverbound `container_button_click` payload model (`0x0d`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ContainerButtonClick {
    pub window_id: u8,
    pub button_id: i32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UiSlotRect {
    pub index: i16,
    pub rect: UiRect,
    pub kind: SlotKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UiButtonRect {
    pub id: i32,
    pub rect: UiRect,
    pub label: &'static str,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct UiBarRect {
    pub property: i16,
    pub kind: PropertyBarKind,
    pub rect: UiRect,
    pub fraction: f32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ContainerLayout {
    pub panel: UiRect,
    pub slots: Vec<UiSlotRect>,
    pub buttons: Vec<UiButtonRect>,
    pub bars: Vec<UiBarRect>,
    pub tooltip: Option<UiRect>,
}

/// Typed interaction emitted by workstation-specific controls.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WorkstationAction {
    RenameItem(String),
    SetBeacon {
        primary: Option<i32>,
        secondary: Option<i32>,
    },
    SelectTrade(i32),
    ClickButton(i32),
    ToggleCrafterSlot {
        slot: i32,
        disabled: bool,
    },
}

/// One visible merchant trade row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MerchantOfferView {
    pub input1: String,
    pub input2: Option<String>,
    pub output: String,
    pub original_cost: u32,
    pub effective_cost: u32,
    pub uses: u32,
    pub max_uses: u32,
    pub out_of_stock: bool,
}

/// Deterministic container screen state and click controller.
#[derive(Clone, Debug)]
pub struct ContainerScreenState {
    schema: &'static MenuScreenSchema,
    pub window_id: u8,
    pub state_id: i32,
    pub title: String,
    pub slots: Vec<UiItemStack>,
    pub cursor: Option<UiItemStack>,
    pub properties: BTreeMap<i16, i16>,
    pub drag_mode: Option<DragState>,
    pub hovered_slot: Option<usize>,
    /// Polymorph-style crafting-conflict switcher. The host refills it
    /// with [`ConflictWidget::set_candidates`] whenever the station's
    /// inputs change; with fewer than two candidates it draws and
    /// accepts nothing, so a conflict-free screen is pixel-identical.
    pub conflict: ConflictWidget,
    pub rename_text: String,
    pub rename_focused: bool,
    pub beacon_primary: Option<i32>,
    pub beacon_secondary: Option<i32>,
    pub merchant_offers: Vec<MerchantOfferView>,
    pub merchant_scroll_offset: usize,
    pub selected_trade_index: Option<usize>,
    pub selected_enchant_tier: Option<usize>,
    pub loom_patterns: Vec<String>,
    pub loom_scroll_row: usize,
    pub selected_loom_pattern: Option<usize>,
    pub stonecutter_recipes: Vec<String>,
    pub stonecutter_scroll_row: usize,
    pub selected_stonecutter_recipe: Option<usize>,
    /// Player XP level used for enchanting affordability. `None` means the
    /// host has not synchronized it yet; explicit values are enforced.
    pub player_experience_level: Option<i32>,
    /// Language used by workstation-local prompt strings.
    pub ui_language: UiLang,
    /// `(timestamp, slot, button)` of the previous real-slot press —
    /// vanilla's `lastClickTime/Slot/Button` tracking that resolves a
    /// second press within 300 ms into a double-click.
    last_slot_click: Option<(Instant, i16, i8)>,
    lapis_slot_synced: bool,
    beacon_payment_slot_synced: bool,
    beacon_close_requested: bool,
}

impl ContainerScreenState {
    pub fn new(
        schema: &'static MenuScreenSchema,
        window_id: u8,
        state_id: i32,
        title: impl Into<String>,
    ) -> Self {
        Self {
            schema,
            window_id,
            state_id,
            title: title.into(),
            slots: vec![UiItemStack::empty(); schema.slot_count.max(0) as usize],
            cursor: None,
            properties: BTreeMap::new(),
            drag_mode: None,
            hovered_slot: None,
            conflict: ConflictWidget::new(),
            rename_text: String::new(),
            rename_focused: false,
            beacon_primary: None,
            beacon_secondary: None,
            merchant_offers: Vec::new(),
            merchant_scroll_offset: 0,
            selected_trade_index: None,
            selected_enchant_tier: None,
            loom_patterns: default_loom_patterns(),
            loom_scroll_row: 0,
            selected_loom_pattern: None,
            stonecutter_recipes: Vec::new(),
            stonecutter_scroll_row: 0,
            selected_stonecutter_recipe: None,
            player_experience_level: None,
            ui_language: UiLang::En,
            last_slot_click: None,
            lapis_slot_synced: false,
            beacon_payment_slot_synced: false,
            beacon_close_requested: false,
        }
    }

    pub fn schema(&self) -> &'static MenuScreenSchema {
        self.schema
    }

    pub fn slot_count(&self) -> usize {
        self.slots.len()
    }

    pub fn slot(&self, index: usize) -> Option<&UiItemStack> {
        self.slots.get(index)
    }

    pub fn set_slot(&mut self, index: usize, stack: UiItemStack) {
        if self.schema.menu_id == 13 && index == 1 {
            self.lapis_slot_synced = true;
        }
        if self.schema.menu_id == 9 && index == 0 {
            self.beacon_payment_slot_synced = true;
        }
        if let Some(slot) = self.slots.get_mut(index) {
            *slot = stack;
        }
    }

    pub fn set_cursor(&mut self, cursor: Option<UiItemStack>) {
        self.cursor = cursor.filter(|s| !s.is_empty());
    }

    pub fn set_property(&mut self, property: i16, value: i16) {
        self.properties.insert(property, value);
        if self.schema.menu_id == 9 {
            // Beacon menu properties mirror the synchronized active effects:
            // property 1 is primary and property 2 secondary; negative values
            // mean no selection. Keeping the local selection initialized from
            // them prevents an already-active beacon from appearing blank.
            if property == 1 {
                self.beacon_primary = (value >= 0).then_some(i32::from(value));
            } else if property == 2 {
                self.beacon_secondary = (value >= 0).then_some(i32::from(value));
            }
        }
    }

    pub fn set_ui_language(&mut self, lang: UiLang) {
        self.ui_language = lang;
    }

    pub fn set_player_experience_level(&mut self, level: Option<i32>) {
        self.player_experience_level = level.map(|level| level.max(0));
    }

    pub fn property(&self, property: i16) -> i16 {
        self.properties.get(&property).copied().unwrap_or(0)
    }

    pub fn is_crafter_slot_disabled(&self, slot: usize) -> bool {
        self.schema.menu_id == 7 && slot < 9 && self.property(slot as i16) != 0
    }

    pub fn toggle_crafter_slot(&mut self, slot: usize) -> Option<WorkstationAction> {
        if self.schema.menu_id != 7
            || slot >= 9
            || self.cursor.as_ref().is_some_and(|stack| !stack.is_empty())
            || !self.slots.get(slot)?.is_empty()
        {
            return None;
        }
        let disabled = !self.is_crafter_slot_disabled(slot);
        self.set_property(slot as i16, i16::from(disabled));
        Some(WorkstationAction::ToggleCrafterSlot {
            slot: slot as i32,
            disabled,
        })
    }

    /// Clamp a paged selector's scroll row after a `rows` wheel delta: the
    /// list pages by four columns, `visible` rows at a time.
    fn scroll_paged_row(row: usize, rows: i32, len: usize, visible: usize) -> usize {
        let max_row = len.div_ceil(4).saturating_sub(visible);
        (row as i64 + i64::from(rows)).clamp(0, max_row as i64) as usize
    }

    /// Keep a selection only while it still addresses an entry.
    fn clamped_selection(selected: Option<usize>, len: usize) -> Option<usize> {
        selected.filter(|&index| index < len)
    }

    pub fn set_loom_patterns(&mut self, patterns: Vec<String>) {
        if self.schema.menu_id != 18 {
            return;
        }
        self.loom_patterns = patterns;
        let len = self.loom_patterns.len();
        self.loom_scroll_row = Self::scroll_paged_row(self.loom_scroll_row, 0, len, 4);
        self.selected_loom_pattern = Self::clamped_selection(self.selected_loom_pattern, len);
    }

    pub fn scroll_loom(&mut self, rows: i32) {
        if self.schema.menu_id != 18 {
            return;
        }
        let len = self.loom_patterns.len();
        self.loom_scroll_row = Self::scroll_paged_row(self.loom_scroll_row, rows, len, 4);
    }

    /// Wheel-scroll the loom pattern grid by whole rows, clamped to the
    /// valid row window. Positive deltas move downward (mouse-wheel down).
    pub fn scroll_loom_grid(&mut self, delta: i32) {
        self.scroll_loom(delta);
    }

    pub fn set_stonecutter_recipes(&mut self, recipes: Vec<String>) {
        if self.schema.menu_id != 24 {
            return;
        }
        self.stonecutter_recipes = recipes;
        let len = self.stonecutter_recipes.len();
        self.stonecutter_scroll_row = Self::scroll_paged_row(self.stonecutter_scroll_row, 0, len, 3);
        self.selected_stonecutter_recipe =
            Self::clamped_selection(self.selected_stonecutter_recipe, len);
    }

    pub fn scroll_stonecutter(&mut self, rows: i32) {
        if self.schema.menu_id != 24 {
            return;
        }
        let len = self.stonecutter_recipes.len();
        self.stonecutter_scroll_row = Self::scroll_paged_row(self.stonecutter_scroll_row, rows, len, 3);
    }

    /// Wheel-scroll the stonecutter recipe grid by whole rows, clamped to
    /// the valid row window.
    pub fn scroll_stonecutter_grid(&mut self, delta: i32) {
        self.scroll_stonecutter(delta);
    }

    pub fn hover(&mut self, slot: Option<usize>) {
        self.hovered_slot = slot.filter(|&s| s < self.slots.len());
    }

    pub fn set_rename_text(&mut self, text: impl Into<String>) -> Option<WorkstationAction> {
        if self.schema.menu_id != 8 {
            return None;
        }
        self.rename_text = text.into();
        Some(WorkstationAction::RenameItem(self.rename_text.clone()))
    }

    pub fn handle_rename_char(&mut self, ch: char) -> Option<WorkstationAction> {
        if self.schema.menu_id != 8 || !self.rename_focused || ch.is_control() {
            return None;
        }
        self.rename_text.push(ch);
        Some(WorkstationAction::RenameItem(self.rename_text.clone()))
    }

    pub fn handle_rename_backspace(&mut self) -> Option<WorkstationAction> {
        if self.schema.menu_id != 8 || !self.rename_focused {
            return None;
        }
        self.rename_text.pop();
        Some(WorkstationAction::RenameItem(self.rename_text.clone()))
    }

    pub fn rename_input_rect(&self, viewport_w: f32, viewport_h: f32) -> Option<UiRect> {
        (self.schema.menu_id == 8).then(|| {
            let p = self.panel_rect(viewport_w, viewport_h);
            UiRect::new(p.x + CONTENT_PAD + 29, p.y + CONTENT_PAD + 27, 96, 12)
        })
    }

    pub fn set_merchant_offers(&mut self, offers: Vec<MerchantOfferView>) {
        self.merchant_offers = offers;
        self.merchant_scroll_offset = self
            .merchant_scroll_offset
            .min(self.merchant_offers.len().saturating_sub(7));
        self.selected_trade_index = Self::clamped_selection(self.selected_trade_index, self.merchant_offers.len());
    }

    pub fn select_merchant_trade(&mut self, index: usize) -> Option<WorkstationAction> {
        if self.schema.menu_id != 19 || index >= self.merchant_offers.len() {
            return None;
        }
        self.selected_trade_index = Some(index);
        Some(WorkstationAction::SelectTrade(index as i32))
    }

    pub fn scroll_merchant(&mut self, rows: i32) {
        if self.schema.menu_id != 19 {
            return;
        }
        let max_offset = self.merchant_offers.len().saturating_sub(7);
        self.merchant_scroll_offset = (self.merchant_scroll_offset as i64 + i64::from(rows))
            .clamp(0, max_offset as i64) as usize;
    }

    fn lapis_count(&self) -> u8 {
        self.slots
            .get(1)
            .filter(|stack| {
                !stack.is_empty()
                    && (stack.namespaced_id == "minecraft:lapis_lazuli" || stack.item_id == 807)
            })
            .map(|stack| stack.count)
            .unwrap_or(0)
    }

    /// One shared enchanting-row affordability predicate used by both draw
    /// and click input: a positive cost, enough lapis for the row, and (when
    /// synchronized) enough player XP levels. A completely unsynchronized
    /// legacy state does not treat missing data as proof of zero lapis/XP;
    /// explicitly synchronized empty slots and levels are enforced strictly.
    pub fn enchanting_row_enabled(&self, row: usize) -> bool {
        if self.schema.menu_id != 13 || row >= 3 {
            return false;
        }
        let cost = i32::from(self.property(row as i16));
        if cost <= 0 {
            return false;
        }
        let lapis_ok = !self.lapis_slot_synced || self.lapis_count() as usize > row;
        let xp_ok = self
            .player_experience_level
            .is_none_or(|level| level >= cost);
        lapis_ok && xp_ok
    }

    /// `true` when the beacon payment slot carries a Vanilla payment item
    /// (iron/gold ingot, emerald, diamond, or netherite ingot).
    pub fn beacon_payment_valid(&self) -> bool {
        self.slots.first().is_some_and(|stack| {
            !stack.is_empty()
                && (matches!(
                    stack.namespaced_id.as_str(),
                    "minecraft:iron_ingot"
                        | "minecraft:gold_ingot"
                        | "minecraft:emerald"
                        | "minecraft:diamond"
                        | "minecraft:netherite_ingot"
                ) || matches!(stack.item_id, 805 | 806 | 811 | 815 | 816))
        })
    }

    /// Beacon confirm is enabled only with both a valid payment item and a
    /// selected primary effect.
    pub fn beacon_confirm_enabled(&self) -> bool {
        // Legacy tests/hosts constructed beacon screens before payment-slot
        // synchronization existed. Do not mistake that absent state for an
        // explicitly synchronized empty slot; once `set_slot(0, ...)` has
        // arrived (even with an empty stack), payment validity is strict.
        let payment_ok = self.beacon_payment_valid() || !self.beacon_payment_slot_synced;
        self.schema.menu_id == 9 && payment_ok && self.beacon_primary.is_some()
    }

    /// Whether Cancel asked the host to close this screen. `click_at` also
    /// returns the backward-compatible `ClickButton(-9)` sentinel because
    /// adding a new enum variant would break existing exhaustive hosts.
    pub fn take_beacon_close_request(&mut self) -> bool {
        std::mem::take(&mut self.beacon_close_requested)
    }

    /// Route a workstation click in the coordinate space produced by `layout`.
    pub fn click_at(
        &mut self,
        x: i32,
        y: i32,
        viewport_w: f32,
        viewport_h: f32,
    ) -> Option<WorkstationAction> {
        self.click_at_with_button(x, y, viewport_w, viewport_h, 0)
    }

    /// Button-aware workstation click routing. `button == 0` is left click;
    /// crafter slot toggles are valid for that button only.
    ///
    /// Every station branch works in the content-box-local space of
    /// [`Self::panel_rect`] plus [`CONTENT_PAD`], the same space its chrome
    /// and [`Self::layout`] rects use.
    pub fn click_at_with_button(
        &mut self,
        x: i32,
        y: i32,
        viewport_w: f32,
        viewport_h: f32,
        button: i8,
    ) -> Option<WorkstationAction> {
        if self.schema.menu_id == 8 {
            let r = self.rename_input_rect(viewport_w, viewport_h)?;
            if r.contains(x, y) {
                self.rename_focused = true;
                return None;
            }
            self.rename_focused = false;
        }
        let panel = self.panel_rect(viewport_w, viewport_h);
        let (lx, ly) = (x - panel.x - CONTENT_PAD, y - panel.y - CONTENT_PAD);
        match self.schema.menu_id {
            7 => self.click_crafter_grid(lx, ly, button),
            9 => self.click_beacon_picker(lx, ly),
            13 | 17 => self.click_schema_button(x, y, viewport_w, viewport_h),
            18 => self.click_recipe_grid(
                RecipeGrid::LOOM,
                self.loom_scroll_row,
                self.loom_patterns.len(),
                lx,
                ly,
            ),
            19 => self.click_merchant_side(lx, ly),
            24 => self.click_recipe_grid(
                RecipeGrid::STONECUTTER,
                self.stonecutter_scroll_row,
                self.stonecutter_recipes.len(),
                lx,
                ly,
            ),
            _ => None,
        }
    }

    /// Crafter 3×3 slot toggle: empty slots only, and only for a plain left
    /// click with nothing on the cursor.
    fn click_crafter_grid(
        &mut self,
        lx: i32,
        ly: i32,
        button: i8,
    ) -> Option<WorkstationAction> {
        if button != 0 || !self.cursor.as_ref().is_none_or(UiItemStack::is_empty) {
            return None;
        }
        let (slot, _) = CRAFTER_GRID.iter().enumerate().find(|(slot, def)| {
            local_rect(def.x, def.y, SLOT_SIZE, SLOT_SIZE).contains(lx, ly)
                && self.slots.get(*slot).is_some_and(UiItemStack::is_empty)
        })?;
        self.toggle_crafter_slot(slot)
    }

    /// Beacon picker: tier-gated effect choice, then confirm/cancel.
    fn click_beacon_picker(&mut self, lx: i32, ly: i32) -> Option<WorkstationAction> {
        let level = self.property(0).clamp(0, 4);
        for &(effect, x, y, tier, ..) in &BEACON_CONTROLS {
            if level < tier
                || !local_rect(x, y, BEACON_CONTROL_W, BEACON_CONTROL_H).contains(lx, ly)
            {
                continue;
            }
            match effect {
                -1 => self.beacon_secondary = self.beacon_primary,
                9 => self.beacon_secondary = Some(9),
                _ => self.beacon_primary = Some(effect),
            }
            return None;
        }
        let (x, y, w, h) = BEACON_CONFIRM_RECT;
        if local_rect(x, y, w, h).contains(lx, ly) {
            return self
                .beacon_confirm_enabled()
                .then_some(WorkstationAction::SetBeacon {
                    primary: self.beacon_primary,
                    secondary: self.beacon_secondary,
                });
        }
        let (x, y, w, h) = BEACON_CANCEL_RECT;
        if local_rect(x, y, w, h).contains(lx, ly) {
            self.beacon_close_requested = true;
            return Some(WorkstationAction::ClickButton(-9));
        }
        None
    }

    /// Villager trade sidebar: pick the row under the pointer.
    fn click_merchant_side(&mut self, lx: i32, ly: i32) -> Option<WorkstationAction> {
        if !(8..104).contains(&lx) || !(18..130).contains(&ly) {
            return None;
        }
        let row = ((ly - 18) / 16) as usize;
        self.select_merchant_trade(self.merchant_scroll_offset + row)
    }

    /// Select a paged recipe/pattern entry under the pointer.
    fn click_recipe_grid(
        &mut self,
        grid: RecipeGrid,
        scroll_row: usize,
        len: usize,
        lx: i32,
        ly: i32,
    ) -> Option<WorkstationAction> {
        let index = grid.index_at(lx, ly, scroll_row, len)?;
        if self.schema.menu_id == 18 {
            self.selected_loom_pattern = Some(index);
        } else {
            self.selected_stonecutter_recipe = Some(index);
        }
        Some(WorkstationAction::ClickButton(index as i32))
    }

    /// Schema button hit test: enchantment tiers stay gated by affordability,
    /// lectern page/button ids are unconditional.
    fn click_schema_button(
        &mut self,
        x: i32,
        y: i32,
        viewport_w: f32,
        viewport_h: f32,
    ) -> Option<WorkstationAction> {
        let layout = self.layout(viewport_w, viewport_h);
        let id = layout
            .buttons
            .iter()
            .find(|button| button.rect.contains(x, y))?
            .id;
        if self.schema.menu_id != 13 {
            return Some(WorkstationAction::ClickButton(id));
        }
        let tier = id as usize;
        if !self.enchanting_row_enabled(tier) {
            return None;
        }
        self.selected_enchant_tier = Some(tier);
        Some(WorkstationAction::ClickButton(id))
    }

    pub fn is_player_slot(&self, index: i16) -> bool {
        self.schema.has_player_inventory
            && index >= self.schema.inventory_start
            && index < self.schema.inventory_start + 36
    }

    pub fn slot_kind(&self, index: i16) -> Option<SlotKind> {
        if !(0..self.slots.len() as i16).contains(&index) {
            return None;
        }
        if self.is_player_slot(index) {
            return Some(SlotKind::Normal);
        }
        self.schema
            .container_slots
            .iter()
            .find(|s| s.index == index)
            .map(|s| s.kind)
    }

    pub fn slot_def(&self, index: i16) -> Option<&'static SlotDef> {
        self.schema
            .container_slots
            .iter()
            .find(|s| s.index == index)
    }

    fn accepts_placement(&self, index: i16, item: &UiItemStack) -> bool {
        if item.is_empty() {
            return true;
        }
        !matches!(
            self.slot_kind(index),
            Some(SlotKind::Output) | Some(SlotKind::Locked) | None
        )
    }

    fn can_take(&self, index: i16) -> bool {
        !matches!(self.slot_kind(index), Some(SlotKind::Locked) | None)
    }

    pub fn total_count(&self) -> u64 {
        self.slots.iter().map(|s| u64::from(s.count)).sum::<u64>()
            + self.cursor.as_ref().map_or(0, |s| u64::from(s.count))
    }

    pub fn panel_rect(&self, viewport_w: f32, viewport_h: f32) -> UiRect {
        let w = self.schema.panel_w.max(1.0);
        let h = self.schema.panel_h.max(1.0);
        let vw = if viewport_w.is_finite() {
            viewport_w.max(1.0)
        } else {
            1.0
        };
        let vh = if viewport_h.is_finite() {
            viewport_h.max(1.0)
        } else {
            1.0
        };
        let pw = w.min(vw);
        let ph = h.min(vh);
        UiRect::new(
            ((vw - pw) * 0.5).round() as i32,
            ((vh - ph) * 0.5).round() as i32,
            pw as u32,
            ph as u32,
        )
    }

    fn slot_origin(&self, index: i16) -> Option<(i32, i32, SlotKind)> {
        if let Some(def) = self.slot_def(index) {
            return Some((def.x, def.y, def.kind));
        }
        if !self.is_player_slot(index) {
            return None;
        }
        let inv = index - self.schema.inventory_start;
        let col = inv.rem_euclid(PLAYER_INVENTORY_COLS);
        let (row, hotbar_offset) = if inv < PLAYER_INVENTORY_SIZE {
            (inv / PLAYER_INVENTORY_COLS, 0)
        } else {
            (0, PLAYER_HOTBAR_OFFSET)
        };
        let grid = SlotGridGeometry::new(
            i32::from(self.schema.inventory_origin_x),
            i32::from(self.schema.inventory_origin_y),
            PLAYER_INVENTORY_COLS as usize,
        );
        let (x, y) = grid.cell_origin(col as usize, row as usize);
        Some((x, y + hotbar_offset, SlotKind::Normal))
    }

    pub fn slot_rect(&self, index: i16, panel: UiRect) -> Option<UiRect> {
        let (x, y, _) = self.slot_origin(index)?;
        Some(UiRect::new(
            panel.x + CONTENT_PAD + x,
            panel.y + CONTENT_PAD + y,
            SLOT_SIZE as u32,
            SLOT_SIZE as u32,
        ))
    }

    /// Physical-pixel rect of the first `Output` slot — the anchor the
    /// Polymorph conflict strip docks beside. `None` for menus without
    /// an output slot (chest, hopper, inventory).
    pub fn output_slot_rect(&self, viewport_w: f32, viewport_h: f32) -> Option<UiRect> {
        let panel = self.panel_rect(viewport_w, viewport_h);
        let layout = self.layout(viewport_w, viewport_h);
        let index = layout
            .slots
            .iter()
            .find(|s| s.kind == SlotKind::Output)?
            .index;
        self.slot_rect(index, panel)
    }

    pub fn layout(&self, viewport_w: f32, viewport_h: f32) -> ContainerLayout {
        let panel = self.panel_rect(viewport_w, viewport_h);
        let slots: Vec<UiSlotRect> = (0..self.slots.len() as i16)
            .filter_map(|index| {
                self.slot_rect(index, panel).map(|rect| UiSlotRect {
                    index,
                    rect,
                    kind: self.slot_kind(index).unwrap_or(SlotKind::Normal),
                })
            })
            .collect();
        let mut buttons: Vec<UiButtonRect> = self
            .schema
            .buttons
            .iter()
            .map(|b| UiButtonRect {
                id: b.id,
                rect: content_rect(panel, b.x, b.y, b.w, b.h),
                label: b.label,
            })
            .collect();
        buttons.extend(self.dynamic_buttons(panel, viewport_w, viewport_h));
        let bars = self
            .schema
            .property_bars
            .iter()
            .map(|b| UiBarRect {
                property: b.property,
                kind: b.kind,
                rect: content_rect(panel, b.x, b.y, b.w, b.h),
                fraction: self.bar_fraction(b),
            })
            .collect();
        let tooltip = self.tooltip_rect(viewport_w, viewport_h, &slots);
        ContainerLayout {
            panel,
            slots,
            buttons,
            bars,
            tooltip,
        }
    }

    /// Buttons derived from runtime state: recipe/pattern grids, the rename
    /// field, the beacon effect picker and villager trade rows.
    fn dynamic_buttons(
        &self,
        panel: UiRect,
        viewport_w: f32,
        viewport_h: f32,
    ) -> Vec<UiButtonRect> {
        match self.schema.menu_id {
            8 => self
                .rename_input_rect(viewport_w, viewport_h)
                .map(|rect| UiButtonRect {
                    id: -8,
                    rect,
                    label: "rename",
                })
                .into_iter()
                .collect(),
            9 => {
                // Layout ids are the historical 1-based effect ids offset by
                // 100; `-1` is the secondary-effect mirror.
                let mut buttons: Vec<UiButtonRect> = BEACON_CONTROLS
                    .iter()
                    .map(|&(effect, x, y, _, en, _)| UiButtonRect {
                        id: if effect < 0 { 100 } else { effect + 101 },
                        rect: content_rect(panel, x, y, BEACON_CONTROL_W, BEACON_CONTROL_H),
                        label: en,
                    })
                    .collect();
                let (x, y, w, h) = BEACON_CANCEL_RECT;
                buttons.push(UiButtonRect {
                    id: -9,
                    rect: content_rect(panel, x, y, w, h),
                    label: "Cancel",
                });
                buttons
            }
            18 => RecipeGrid::LOOM.buttons(
                panel,
                self.loom_scroll_row,
                self.loom_patterns.len(),
            ),
            19 => (0..self.merchant_offers.len().min(7))
                .map(|row| UiButtonRect {
                    id: 1000 + row as i32,
                    rect: content_rect(panel, 8, 18 + row as i32 * 16, 96, 16),
                    label: "trade",
                })
                .collect(),
            24 => RecipeGrid::STONECUTTER.buttons(
                panel,
                self.stonecutter_scroll_row,
                self.stonecutter_recipes.len(),
            ),
            _ => Vec::new(),
        }
    }

    fn scale_factor(&self, viewport_w: f32, viewport_h: f32, gui_scale: f32) -> f32 {
        let scale = clamped_gui_scale(gui_scale);
        let logical_w = (viewport_w.max(1.0) / scale).max(1.0);
        let logical_h = (viewport_h.max(1.0) / scale).max(1.0);
        (logical_w / self.schema.panel_w.max(1.0))
            .min(logical_h / self.schema.panel_h.max(1.0))
            .min(1.0)
    }

    fn scale_rect(
        rect: &mut UiRect,
        factor: f32,
        cx: f32,
        cy: f32,
        target_cx: f32,
        target_cy: f32,
    ) {
        let left = (target_cx + (rect.x as f32 - cx) * factor).round() as i32;
        let top = (target_cy + (rect.y as f32 - cy) * factor).round() as i32;
        let right = (target_cx + (rect.right() as f32 - cx) * factor).round() as i32;
        let bottom = (target_cy + (rect.bottom() as f32 - cy) * factor).round() as i32;
        rect.x = left;
        rect.y = top;
        rect.w = right.saturating_sub(left).max(1) as u32;
        rect.h = bottom.saturating_sub(top).max(1) as u32;
    }

    /// Layout in logical GUI pixels after applying a GUI scale from 1 to 4.
    pub fn layout_scaled(
        &self,
        viewport_w: f32,
        viewport_h: f32,
        gui_scale: f32,
    ) -> ContainerLayout {
        let mut layout = self.layout(viewport_w, viewport_h);
        let scale = clamped_gui_scale(gui_scale);
        let logical_w = viewport_w.max(1.0) / scale;
        let logical_h = viewport_h.max(1.0) / scale;
        let factor = self.scale_factor(viewport_w, viewport_h, scale);
        let (cx, cy) = (viewport_w * 0.5, viewport_h * 0.5);
        let (tx, ty) = (logical_w * 0.5, logical_h * 0.5);
        Self::scale_rect(&mut layout.panel, factor, cx, cy, tx, ty);
        for slot in &mut layout.slots {
            Self::scale_rect(&mut slot.rect, factor, cx, cy, tx, ty);
        }
        for button in &mut layout.buttons {
            Self::scale_rect(&mut button.rect, factor, cx, cy, tx, ty);
        }
        for bar in &mut layout.bars {
            Self::scale_rect(&mut bar.rect, factor, cx, cy, tx, ty);
        }
        if let Some(rect) = &mut layout.tooltip {
            Self::scale_rect(rect, factor, cx, cy, tx, ty);
        }
        layout
    }

    /// Click using logical coordinates returned by `layout_scaled`.
    pub fn click_at_scaled(
        &mut self,
        x: i32,
        y: i32,
        viewport_w: f32,
        viewport_h: f32,
        gui_scale: f32,
    ) -> Option<WorkstationAction> {
        let scale = clamped_gui_scale(gui_scale);
        let factor = self
            .scale_factor(viewport_w, viewport_h, scale)
            .max(f32::EPSILON);
        let native_x =
            (viewport_w * 0.5 + (x as f32 - viewport_w / scale * 0.5) / factor).round() as i32;
        let native_y =
            (viewport_h * 0.5 + (y as f32 - viewport_h / scale * 0.5) / factor).round() as i32;
        self.click_at(native_x, native_y, viewport_w, viewport_h)
    }

    fn bar_fraction(&self, bar: &PropertyBarDef) -> f32 {
        let p = |id: i16| f32::from(self.property(id));
        let frac = match bar.kind {
            PropertyBarKind::Flame => p(bar.property) / p(bar.secondary_property).max(200.0),
            PropertyBarKind::Arrow => p(bar.property) / p(bar.secondary_property).max(1.0),
            PropertyBarKind::Bubbles => p(bar.property) / 400.0,
            PropertyBarKind::Fuel => p(bar.property) / 200.0,
            PropertyBarKind::Levels => p(bar.property) / 4.0,
            PropertyBarKind::Costs => p(bar.property) / 30.0,
        };
        frac.clamp(0.0, 1.0)
    }

    fn tooltip_item(&self) -> Option<UiItemStack> {
        if let Some(index) = self.hovered_slot {
            if let Some(stack) = self.slots.get(index) {
                if !stack.is_empty() {
                    return Some(stack.clone());
                }
            }
        }
        self.cursor.clone().filter(|s| !s.is_empty())
    }

    fn tooltip_lines(&self, item: &UiItemStack) -> Vec<(String, UiColor)> {
        let mut lines = vec![(item.display_name.clone(), item.rarity_color)];
        for ench in &item.enchantments {
            lines.push((format!("{} {}", ench.name, ench.level), palette::TEXT_DIM));
        }
        for lore in &item.lore {
            lines.push((lore.clone(), palette::TEXT_DIM));
        }
        lines
    }

    fn tooltip_wrap_width(viewport_w: f32) -> usize {
        (viewport_w.max(1.0) as i32 - 8).clamp(1, 220) as usize
    }

    fn tooltip_row_limit(viewport_h: f32) -> usize {
        tooltip_row_budget((viewport_h.max(1.0) as u32).saturating_sub(6), 0, 10)
    }

    /// Pixel-width-wrapped, row-budgeted tooltip lines. Wrapping is based on
    /// glyph advance (`text_width` semantics), so CJK lore neither overruns
    /// horizontally nor gets sliced at a UTF-8 byte boundary.
    fn tooltip_display_lines(
        &self,
        item: &UiItemStack,
        viewport_w: f32,
        viewport_h: f32,
    ) -> Vec<(String, UiColor)> {
        let width = Self::tooltip_wrap_width(viewport_w);
        let rows = Self::tooltip_row_limit(viewport_h);
        let mut out = Vec::new();
        for (text, color) in self.tooltip_lines(item) {
            for wrapped in wrap_text_px(&text, width.saturating_sub(8)) {
                if out.len() >= rows {
                    return out;
                }
                out.push((wrapped, color));
            }
        }
        out
    }

    fn tooltip_size(&self, item: &UiItemStack, viewport_w: f32, viewport_h: f32) -> (i32, i32) {
        let lines = self.tooltip_display_lines(item, viewport_w, viewport_h);
        let mut w = 0i32;
        for (text, _) in &lines {
            w = w.max(text_width(text, 1) as i32);
        }
        let h = lines.len() as i32 * 10 + 6;
        (w + 8, h.max(10))
    }

    /// Tooltip rectangle strictly inside the viewport and clear of slots.
    pub fn tooltip_rect(
        &self,
        viewport_w: f32,
        viewport_h: f32,
        slots: &[UiSlotRect],
    ) -> Option<UiRect> {
        let item = self.tooltip_item()?;
        let (tw, th) = self.tooltip_size(&item, viewport_w, viewport_h);
        let vw = viewport_w.max(1.0) as i32;
        let vh = viewport_h.max(1.0) as i32;
        let tw = tw.min(vw).max(0) as u32;
        let th = th.min(vh).max(0) as u32;
        let anchor = self
            .hovered_slot
            .and_then(|i| slots.iter().find(|s| s.index == i as i16))
            .map(|s| s.rect)
            .unwrap_or_else(|| UiRect::new(vw / 2, vh / 2, 0, 0));
        let mut candidates = vec![
            UiRect::new(anchor.right() + 4, anchor.y, tw, th),
            UiRect::new(anchor.x - tw as i32 - 4, anchor.y, tw, th),
            UiRect::new(anchor.x, anchor.bottom() + 4, tw, th),
            UiRect::new(anchor.x, anchor.y - th as i32 - 4, tw, th),
            UiRect::new(vw - tw as i32 - 4, 4, tw, th),
            UiRect::new(4, 4, tw, th),
            UiRect::new(4, vh - th as i32 - 4, tw, th),
            UiRect::new(vw - tw as i32 - 4, vh - th as i32 - 4, tw, th),
        ];
        candidates.retain(|r| r.x >= 0 && r.y >= 0 && r.right() <= vw && r.bottom() <= vh);
        candidates
            .into_iter()
            .find(|r| slots.iter().all(|s| !r.intersects(s.rect)))
            .or_else(|| {
                let mut y = 0i32;
                while y + th as i32 <= vh {
                    let mut x = 0i32;
                    while x + tw as i32 <= vw {
                        let r = UiRect::new(x, y, tw, th);
                        if slots.iter().all(|s| !r.intersects(s.rect)) {
                            return Some(r);
                        }
                        x += 16;
                    }
                    y += 16;
                }
                None
            })
    }

    pub fn render(&self, viewport_w: f32, viewport_h: f32, out: &mut UiDrawList) {
        self.render_panel(viewport_w, viewport_h, None, out);
    }

    /// Render with the authentic Vanilla panel texture when the supplied
    /// atlas contains one for this menu. Missing entries preserve vector UI.
    pub fn render_with_atlas(
        &self,
        viewport_w: f32,
        viewport_h: f32,
        menu_id_override: Option<u32>,
        atlas: &GuiSpriteAtlas,
        out: &mut UiDrawList,
    ) {
        let uv = atlas.container_panel_sprite(menu_id_override.unwrap_or(self.schema.menu_id));
        self.render_panel(viewport_w, viewport_h, uv, out);
    }

    fn render_panel(
        &self,
        viewport_w: f32,
        viewport_h: f32,
        panel_uv: Option<GuiSpriteUv>,
        out: &mut UiDrawList,
    ) {
        let Some((vw, vh)) = viewport_px(viewport_w, viewport_h) else {
            return;
        };
        let panel = self.panel_rect(viewport_w, viewport_h);
        self.draw_panel_backdrop(panel, panel_uv, vw, vh, out);
        if viewport_w < self.schema.panel_w || viewport_h < self.schema.panel_h {
            return;
        }
        let layout = self.layout(viewport_w, viewport_h);
        self.draw_slots(&layout, panel_uv, out);
        for bar in &layout.bars {
            let (fill, empty) = (palette::ACCENT, palette::BUTTON_DISABLED);
            out.progress_bar(bar.rect, bar.fraction, fill, empty, palette::PANEL_BORDER);
        }
        self.draw_menu_buttons(&layout, out);
        self.draw_station_chrome(panel, out);
        self.draw_workstation_controls(panel, out);
        // Polymorph strip beside the station's output slot — only while two
        // or more recipes conflict; a quiet grid emits nothing.
        if self.conflict.is_active() {
            if let Some(anchor) = self.output_slot_rect(viewport_w, viewport_h) {
                self.conflict.render(anchor, 1, vw as u32, vh as u32, out);
            }
        }
        self.draw_cursor(vw, vh, out);
        if let Some(rect) = layout.tooltip {
            self.draw_tooltip(rect, viewport_w, viewport_h, out);
        }
    }

    /// Viewport wash, panel skin and clipped title bar.
    ///
    /// Vanilla's chests are not the six-row texture scaled vertically: menu
    /// ids 0..=5 encode rows 1..=6, and [`ChestSkinGeometry`] owns the exact
    /// 176×222 sheet validation, top crop, and shared footer crop (including
    /// integral high-DPI source scaling).
    fn draw_panel_backdrop(
        &self,
        panel: UiRect,
        panel_uv: Option<GuiSpriteUv>,
        vw: i32,
        vh: i32,
        out: &mut UiDrawList,
    ) {
        let background = UiRect::new(0, 0, vw as u32, vh as u32);
        out.fill_rect(background, palette::BACKGROUND_BOTTOM);
        let chest = panel_uv.filter(|_| self.schema.menu_id <= 5).and_then(|uv| {
            ChestSkinGeometry::from_sheet(self.schema.menu_id + 1, uv.width, uv.height)
        });
        match (panel_uv, chest) {
            (Some(uv), Some(skin)) => {
                for blit in skin.blits(panel) {
                    let (x, y) = (blit.source_x, blit.source_y);
                    let (w, h) = (blit.source_w, blit.source_h);
                    out.texture_uv(blit.dest, uv.subrect(x, y, w, h));
                }
            }
            (Some(uv), None) => out.texture_uv(panel, uv),
            (None, _) => draw_frame(out, panel, palette::PANEL, palette::PANEL_BORDER),
        }
        if panel.w >= 20 && panel.h >= 12 {
            let clipped = clip_text(&self.title, (panel.w as i32 - 12).max(0));
            out.text(panel.x + 6, panel.y + 4, 1, palette::TEXT, clipped);
        }
    }

    /// One well per menu slot: well face, hover frame, then its contents.
    fn draw_slots(
        &self,
        layout: &ContainerLayout,
        panel_uv: Option<GuiSpriteUv>,
        out: &mut UiDrawList,
    ) {
        for slot in &layout.slots {
            let item = self.slots.get(slot.index as usize).cloned();
            let hovered = self.hovered_slot == Some(slot.index as usize);
            // The authentic container texture already carries Vanilla's slot
            // artwork. Opaque procedural wells are strictly a no-skin
            // fallback; overpainting them would obscure the texture.
            if panel_uv.is_none() {
                out.fill_rect(slot.rect, palette::SLOT);
            }
            if panel_uv.is_none() || hovered {
                let (border, thickness) = if hovered {
                    (palette::BUTTON_BORDER_HOVER, 2)
                } else {
                    (palette::PANEL_BORDER, 1)
                };
                out.border_rect(slot.rect, border, thickness);
            }
            match item {
                Some(item) if !item.is_empty() => self.draw_stack(slot.rect, &item, out),
                _ => self.draw_empty_slot(slot, out),
            }
            self.draw_drag_ghost(slot, out);
        }
    }

    /// Live QUICK_CRAFT ghost: every collected drag slot previews the total
    /// vanilla would deposit there — `getQuickCraftPlaceCount` (even split,
    /// one-per-slot, or full clone) plus the stack already present, capped
    /// at `min(carried max, slot max)`. The vanilla translucent item ghost
    /// has no alpha-capable primitive here, so an accent frame plus the
    /// predicted count carries the preview.
    fn draw_drag_ghost(&self, slot: &UiSlotRect, out: &mut UiDrawList) {
        let Some(drag) = self.drag_mode.as_ref() else {
            return;
        };
        if !drag.slots.contains(&slot.index) {
            return;
        }
        let Some(carried) = self.cursor.as_ref().filter(|s| !s.is_empty()) else {
            return;
        };
        let total = drag.slots.len().max(1) as u32;
        let place = match drag.button {
            0 => u32::from(carried.count) / total,
            1 => 1,
            _ => u32::from(carried.max_stack),
        };
        let existing = self.slots.get(slot.index as usize);
        let carry = existing.map_or(0, |s| u32::from(s.count));
        let max = existing.map_or_else(
            || u32::from(carried.max_stack),
            |s| u32::from(s.max_stack).min(u32::from(carried.max_stack)),
        );
        let ghost = (place + carry).min(max);
        if ghost == 0 {
            return;
        }
        let inner = UiRect::new(
            slot.rect.x + 1,
            slot.rect.y + 1,
            slot.rect.w.saturating_sub(2),
            slot.rect.h.saturating_sub(2),
        );
        out.border_rect(inner, palette::ACCENT, 1);
        if ghost >= 2 {
            let label = ghost.to_string();
            let x = slot.rect.right() - 2 - text_width(&label, 1) as i32;
            out.text(x, slot.rect.bottom() - 9, 1, palette::ACCENT, label);
        }
    }

    /// Ghost preview and crafter disable cross of a slot with no stack.
    fn draw_empty_slot(&self, slot: &UiSlotRect, out: &mut UiDrawList) {
        if let Some(ghost) = ghost_item(self.schema.menu_id, slot.index) {
            draw_vanilla_ghost_item(out, slot.rect, ghost);
        }
        let disabled = self.schema.menu_id == 7 && self.is_crafter_slot_disabled(slot.index as usize);
        if !disabled {
            return;
        }
        let (x, y) = (slot.rect.x, slot.rect.y);
        out.fill_rect(UiRect::new(x + 3, y + 8, 12, 2), palette::DANGER);
        out.fill_rect(UiRect::new(x + 8, y + 3, 2, 12), palette::DANGER);
        out.border_rect(UiRect::new(x + 4, y + 4, 10, 10), palette::DANGER, 1);
    }

    /// Icon, stack-count label and durability bar of one slot stack.
    fn draw_stack(&self, rect: UiRect, item: &UiItemStack, out: &mut UiDrawList) {
        if item.namespaced_id.is_empty() {
            out.fill_rect(UiRect::new(rect.x + 3, rect.y + 3, 12, 12), palette::ACCENT);
        } else {
            let icon = UiRect::new(rect.x + 2, rect.y + 2, 16, 16);
            out.item_icon_str(icon, &item.namespaced_id);
        }
        draw_count_label(out, rect, item.count);
        if item.max_durability > 0 && item.durability >= 0 {
            let frac = (item.durability as f32 / item.max_durability as f32).clamp(0.0, 1.0);
            let bar = UiRect::new(rect.x + 1, rect.bottom() - 4, 16, 3);
            out.progress_bar(bar, frac, palette::HEALTH, palette::SLOT, palette::PANEL_BORDER);
        }
    }

    /// Menu buttons: enchant tiers, loom/stonecutter entry cards, plain labels.
    fn draw_menu_buttons(&self, layout: &ContainerLayout, out: &mut UiDrawList) {
        for button in &layout.buttons {
            match self.schema.menu_id {
                13 => self.draw_enchant_tier_button(button, out),
                18 | 24 => self.draw_recipe_button(button, out),
                _ => {
                    draw_frame(out, button.rect, palette::BUTTON, palette::BUTTON_BORDER);
                    out.text(button.rect.x + 3, button.rect.y + 2, 1, palette::TEXT, button.label);
                }
            }
        }
    }

    fn draw_enchant_tier_button(&self, button: &UiButtonRect, out: &mut UiDrawList) {
        let tier = button.id as usize;
        let enabled = self.enchanting_row_enabled(tier);
        let selected = self.selected_enchant_tier == Some(tier);
        draw_button(out, button.rect, button_face(enabled), selected);
        let color = if enabled { palette::WARNING } else { palette::BUTTON_DISABLED_TEXT };
        let label = ["I", "II", "III"][tier];
        out.text_str(button.rect.x + 3, button.rect.y + 5, 1, color, label);
    }

    fn draw_recipe_button(&self, button: &UiButtonRect, out: &mut UiDrawList) {
        let index = button.id as usize;
        let loom = self.schema.menu_id == 18;
        let selected = if loom {
            self.selected_loom_pattern == Some(index)
        } else {
            self.selected_stonecutter_recipe == Some(index)
        };
        let face = if selected { palette::SLOT_HOVER } else { palette::BUTTON };
        draw_button(out, button.rect, face, selected);
        if !loom {
            if let Some(recipe) = self.stonecutter_recipes.get(index) {
                let icon = UiRect::new(button.rect.x + 1, button.rect.y + 1, 14, 14);
                out.item_icon_str(icon, recipe);
            }
            return;
        }
        let pattern = self.loom_patterns.get(index).map(String::as_str).unwrap_or("");
        let (x, y) = (button.rect.x + 4, button.rect.y + 3);
        out.text(x, y, 1, palette::TEXT_DARK, pattern_mark(pattern));
    }

    /// Carried stack in the bottom-right screen corner.
    fn draw_cursor(&self, vw: i32, vh: i32, out: &mut UiDrawList) {
        let Some(cursor) = self.cursor.clone().filter(|s| !s.is_empty()) else {
            return;
        };
        let rect = UiRect::new(vw - 24, vh - 24, 20, 20);
        draw_frame(out, rect, palette::SLOT, palette::BUTTON_BORDER_HOVER);
        if cursor.namespaced_id.is_empty() {
            out.fill_rect(UiRect::new(rect.x + 3, rect.y + 3, 14, 14), palette::ACCENT);
        } else {
            let icon = UiRect::new(rect.x + 2, rect.y + 2, 16, 16);
            out.item_icon_str(icon, &cursor.namespaced_id);
        }
        draw_count_label(out, rect, cursor.count);
    }

    /// Tooltip frame with pixel-clipped, row-budgeted lines.
    fn draw_tooltip(&self, rect: UiRect, viewport_w: f32, viewport_h: f32, out: &mut UiDrawList) {
        let Some(item) = self.tooltip_item() else {
            return;
        };
        draw_frame(out, rect, palette::PANEL, palette::PANEL_BORDER);
        let mut y = rect.y + 3;
        for (text, color) in self.tooltip_display_lines(&item, viewport_w, viewport_h) {
            if y + 8 > rect.bottom() - 2 {
                break;
            }
            let clipped = clip_text(&text, (rect.w as i32 - 6).max(1));
            out.text(rect.x + 3, y, 1, color, clipped);
            y += 10;
        }
    }

    /// Render into logical GUI pixels. The physical viewport is first rendered
    /// at native menu geometry, then uniformly fitted to the scaled viewport.
    pub fn render_scaled(
        &self,
        viewport_w: f32,
        viewport_h: f32,
        gui_scale: f32,
        out: &mut UiDrawList,
    ) {
        if viewport_px(viewport_w, viewport_h).is_none() {
            return;
        }
        let mut native = UiDrawList::new();
        self.render(viewport_w, viewport_h, &mut native);
        let scale = clamped_gui_scale(gui_scale);
        let logical_w = (viewport_w / scale).max(1.0);
        let logical_h = (viewport_h / scale).max(1.0);
        let factor = self.scale_factor(viewport_w, viewport_h, scale);
        let (cx, cy, tx, ty) = (
            viewport_w * 0.5,
            viewport_h * 0.5,
            logical_w * 0.5,
            logical_h * 0.5,
        );
        for (index, mut command) in native.commands.into_iter().enumerate() {
            match &mut command {
                UiCommand::FillRect { rect, .. }
                | UiCommand::BorderRect { rect, .. }
                | UiCommand::ProgressBar { rect, .. }
                | UiCommand::ItemIcon { rect, .. }
                | UiCommand::Sprite { rect, .. } => Self::scale_rect(rect, factor, cx, cy, tx, ty),
                UiCommand::Text {
                    x,
                    y,
                    scale: text_scale,
                    text,
                    ..
                } => {
                    *x = (tx + (*x as f32 - cx) * factor).round() as i32;
                    *y = (ty + (*y as f32 - cy) * factor).round() as i32;
                    *text_scale = ((*text_scale as f32 * factor).round() as u32).max(1);
                    let available = (logical_w as i32 - *x).max(0);
                    *text = clip_text(text, available);
                }
            }
            if index == 0 {
                if let UiCommand::FillRect { rect, .. } = &mut command {
                    *rect = UiRect::new(0, 0, logical_w.ceil() as u32, logical_h.ceil() as u32);
                }
            }
            out.push(command);
        }
    }
}

/// Physical viewport in whole pixels, or `None` when it is degenerate.
fn viewport_px(w: f32, h: f32) -> Option<(i32, i32)> {
    (w.is_finite() && h.is_finite() && w > 0.0 && h > 0.0).then_some((w as i32, h as i32))
}

/// GUI scale clamped to the supported `1..=4` range (`1.0` when not finite).
fn clamped_gui_scale(scale: f32) -> f32 {
    if scale.is_finite() { scale.clamp(1.0, 4.0) } else { 1.0 }
}

/// Content-box-local rectangle (no panel origin).
fn local_rect(x: i32, y: i32, w: i32, h: i32) -> UiRect {
    UiRect::new(x, y, w as u32, h as u32)
}

/// GUI-space rectangle at `(x, y)` inside a panel's [`CONTENT_PAD`] box.
fn content_rect(panel: UiRect, x: i32, y: i32, w: i32, h: i32) -> UiRect {
    let local = local_rect(x, y, w, h);
    UiRect::new(panel.x + CONTENT_PAD + local.x, panel.y + CONTENT_PAD + local.y, local.w, local.h)
}

/// Filled rectangle with a 1-px outline — every menu control, well and card.
fn draw_frame(out: &mut UiDrawList, rect: UiRect, fill: UiColor, border: UiColor) {
    out.fill_rect(rect, fill);
    out.border_rect(rect, border, 1);
}

/// Button face plus its selection ring (2-px highlight when selected).
fn draw_button(out: &mut UiDrawList, rect: UiRect, face: UiColor, selected: bool) {
    out.fill_rect(rect, face);
    let (border, thickness) = if selected {
        (palette::BUTTON_BORDER_HOVER, 2)
    } else {
        (palette::BUTTON_BORDER, 1)
    };
    out.border_rect(rect, border, thickness);
}

/// Enabled/gated button face; `gated` keeps the vanilla disabled grey.
fn button_face(enabled: bool) -> UiColor {
    if enabled { palette::BUTTON } else { palette::BUTTON_DISABLED }
}

/// Bottom-right stack-count label of a slot or cursor stack.
fn draw_count_label(out: &mut UiDrawList, rect: UiRect, count: u8) {
    if count < 2 {
        return;
    }
    let label = count.to_string();
    let x = rect.right() - 2 - text_width(&label, 1) as i32;
    out.text(x, rect.bottom() - 9, 1, palette::TEXT, label);
}

/// Single-glyph mark of a namespaced pattern id (`minecraft:base` -> `B`).
fn pattern_mark(pattern: &str) -> String {
    pattern
        .rsplit(':')
        .next()
        .and_then(|path| path.chars().next())
        .unwrap_or('?')
        .to_ascii_uppercase()
        .to_string()
}

/// Vanilla arrow at content-box `(x, y, w, h)`.
fn draw_station_arrow(out: &mut UiDrawList, panel: UiRect, r: (i32, i32, i32, i32), filled: bool) {
    draw_vanilla_arrow(out, content_rect(panel, r.0, r.1, r.2, r.3), filled);
}

/// Content-box station caption, localized through [`tr`].
fn draw_station_caption(
    out: &mut UiDrawList,
    panel: UiRect,
    (x, y): (i32, i32),
    lang: UiLang,
    color: UiColor,
    en: &'static str,
    ja: &'static str,
) {
    let (px, py) = (panel.x + CONTENT_PAD + x, panel.y + CONTENT_PAD + y);
    out.text_str(px, py, 1, color, tr(lang, en, ja));
}

/// Paged recipe/pattern selector grid: `cols × rows` cells at a fixed pitch,
/// addressed row-major from `scroll_row * cols` so scrolling moves whole rows.
#[derive(Clone, Copy)]
struct RecipeGrid {
    x: i32,
    y: i32,
    cell_w: i32,
    cell_h: i32,
    cols: usize,
    rows: usize,
    label: &'static str,
}

impl RecipeGrid {
    const LOOM: Self = Self { x: 60, y: 13, cell_w: 14, cell_h: 14, cols: 4, rows: 4, label: "pattern" };
    const STONECUTTER: Self =
        Self { x: 52, y: 14, cell_w: 16, cell_h: 18, cols: 4, rows: 3, label: "recipe" };

    /// Content-box rect of the cell at `(col, row)`.
    fn cell_rect(self, panel: UiRect, col: usize, row: usize) -> UiRect {
        let (x, y) = (self.x + col as i32 * self.cell_w, self.y + row as i32 * self.cell_h);
        content_rect(panel, x, y, self.cell_w, self.cell_h)
    }

    /// Entry index of the cell under a content-box point, when in range.
    fn index_at(self, lx: i32, ly: i32, scroll_row: usize, len: usize) -> Option<usize> {
        let (w, h) = (self.cell_w * self.cols as i32, self.cell_h * self.rows as i32);
        if !(self.x..self.x + w).contains(&lx) || !(self.y..self.y + h).contains(&ly) {
            return None;
        }
        let (col, row) = ((lx - self.x) / self.cell_w, (ly - self.y) / self.cell_h);
        let index = scroll_row * self.cols + (row * self.cols as i32 + col) as usize;
        (index < len).then_some(index)
    }

    /// Button rects for every visible, in-range entry.
    fn buttons(self, panel: UiRect, scroll_row: usize, len: usize) -> Vec<UiButtonRect> {
        let mut buttons = Vec::new();
        for row in 0..self.rows {
            for col in 0..self.cols {
                let index = scroll_row * self.cols + row * self.cols + col;
                if index < len {
                    let rect = self.cell_rect(panel, col, row);
                    buttons.push(UiButtonRect { id: index as i32, rect, label: self.label });
                }
            }
        }
        buttons
    }

    /// Scrollbar thumb beside the grid: dormant while one page covers `len`,
    /// then advanced `travel` px per scrolled row.
    fn draw_scroll_thumb(
        self,
        out: &mut UiDrawList,
        panel: UiRect,
        scroll_row: usize,
        len: usize,
        (x, y, travel): (i32, i32, i32),
    ) {
        if len <= self.cols * self.rows {
            return;
        }
        let max_row = len.div_ceil(self.cols).saturating_sub(self.rows);
        let thumb_y = y + (scroll_row * travel as usize / max_row.max(1)) as i32;
        out.fill_rect(content_rect(panel, x, thumb_y, 1, 12), palette::ACCENT);
    }
}


impl ContainerScreenState {
    /// Menu-specific interactive controls (rename field, beacon picker,
    /// villager trade list) drawn above the static station chrome.
    fn draw_workstation_controls(&self, panel: UiRect, out: &mut UiDrawList) {
        match self.schema.menu_id {
            8 => self.draw_rename_field(panel, out),
            9 => self.draw_beacon_picker(panel, out),
            19 => self.draw_merchant_offers(panel, out),
            _ => {}
        }
    }

    /// Anvil rename field, its focus caret and the enchantment cost footer.
    fn draw_rename_field(&self, panel: UiRect, out: &mut UiDrawList) {
        let (px, py) = (panel.x + CONTENT_PAD, panel.y + CONTENT_PAD);
        let field = content_rect(panel, 29, 27, 96, 12);
        let border = if self.rename_focused { palette::TEXT_FOCUSED } else { palette::TEXT_DIM };
        draw_frame(out, field, palette::TOOLTIP_BG, border);
        let mut text = clip_text(&self.rename_text, field.w.saturating_sub(6) as i32);
        if self.rename_focused && text_width(&text, 1) + 8 <= field.w {
            text.push('|');
        }
        out.text(field.x + 3, field.y + 2, 1, palette::TEXT, text);
        let cost = self.property(0);
        let (footer, color) = if cost > 0 {
            (format!("Enchantment Cost: {cost}"), palette::ACCENT)
        } else {
            (tr(self.ui_language, "Too Expensive!", "高すぎます！").to_owned(), palette::DANGER)
        };
        out.text(px + 8, py + 76, 1, color, clip_text(&footer, panel.w as i32 - 20));
    }

    /// Whether a beacon picker entry is the current primary/secondary choice.
    fn beacon_control_selected(&self, effect: i32) -> bool {
        match effect {
            -1 => self.beacon_secondary.is_some(),
            9 => self.beacon_secondary == Some(9),
            _ => self.beacon_primary == Some(effect),
        }
    }

    /// Tier-gated beacon effect picker plus the confirm/cancel footer pair.
    fn draw_beacon_picker(&self, panel: UiRect, out: &mut UiDrawList) {
        let lang = self.ui_language;
        let level = self.property(0).clamp(0, 4);
        for &(effect, x, y, tier, en, ja) in &BEACON_CONTROLS {
            let enabled = level >= tier;
            let rect = content_rect(panel, x, y, BEACON_CONTROL_W, BEACON_CONTROL_H);
            let selected = self.beacon_control_selected(effect);
            draw_button(out, rect, button_face(enabled), selected);
            let color = if enabled { palette::TEXT_DARK } else { palette::BUTTON_DISABLED_TEXT };
            out.text(rect.x + 2, rect.y + 3, 1, color, tr(lang, en, ja));
        }
        let confirm = self.beacon_confirm_enabled();
        let footer = [
            (BEACON_CONFIRM_RECT, "Confirm", "決定", palette::ACCENT, confirm),
            (BEACON_CANCEL_RECT, "Cancel", "キャンセル", palette::DANGER, true),
        ];
        for ((x, y, w, h), en, ja, color, enabled) in footer {
            let rect = content_rect(panel, x, y, w, h);
            draw_button(out, rect, button_face(enabled), false);
            let color = if enabled { color } else { palette::BUTTON_DISABLED_TEXT };
            out.text(rect.x + 2, rect.y + 3, 1, color, tr(lang, en, ja));
        }
    }

    /// Scrolled villager trade rows, their level chip and XP progress bar.
    fn draw_merchant_offers(&self, panel: UiRect, out: &mut UiDrawList) {
        let (px, py) = (panel.x + CONTENT_PAD, panel.y + CONTENT_PAD);
        let first = self.merchant_scroll_offset.min(self.merchant_offers.len());
        for row in 0..7 {
            let index = first + row;
            let Some(offer) = self.merchant_offers.get(index) else {
                continue;
            };
            let rect = content_rect(panel, 8, 18 + row as i32 * 16, 96, 16);
            let selected = self.selected_trade_index == Some(index);
            if selected {
                out.fill_rect(rect, palette::SLOT_HOVER);
            }
            let border = if selected { palette::BUTTON_BORDER_HOVER } else { palette::SEPARATOR };
            out.border_rect(rect, border, 1);
            let (x, y) = (rect.x, rect.y);
            out.item_icon_str(UiRect::new(x + 2, y + 2, 11, 11), &offer.input1);
            if let Some(input2) = &offer.input2 {
                out.item_icon_str(UiRect::new(x + 17, y + 2, 11, 11), input2);
            }
            out.text_str(x + 30, y + 3, 1, palette::TEXT_DARK, "→");
            out.item_icon_str(UiRect::new(x + 41, y + 2, 11, 11), &offer.output);
            // A discounted trade shows `original>effective` with the old price
            // struck through.
            let discounted = offer.effective_cost < offer.original_cost;
            let cost = if discounted {
                format!("{}>{}", offer.original_cost, offer.effective_cost)
            } else {
                offer.effective_cost.to_string()
            };
            let color = if discounted { palette::ACCENT } else { palette::TEXT_DARK };
            out.text(x + 55, y + 8, 1, color, clip_text(&cost, 40));
            if discounted {
                let line_w = text_width(&offer.original_cost.to_string(), 1).min(20);
                out.fill_rect(UiRect::new(x + 55, y + 11, line_w, 1), palette::DANGER);
            }
            if offer.out_of_stock {
                out.text_str(rect.right() - 8, y + 3, 1, palette::DANGER, "X");
            }
        }
        out.text(px + 108, py + 14, 1, palette::TEXT_DARK, format!("Level {}", self.property(1).clamp(1, 5)));
        let bar = content_rect(panel, 108, 26, 96, 5);
        let frac = f32::from(self.property(2)).clamp(0.0, 1.0);
        out.progress_bar(bar, frac, palette::XP, palette::BUTTON_DISABLED, palette::PANEL_BORDER);
    }

    /// Static station artwork: arrows, captions, dividers and station cards.
    /// Every offset is content-box local.
    fn draw_station_chrome(&self, panel: UiRect, out: &mut UiDrawList) {
        let lang = self.ui_language;
        let (px, py) = (panel.x + CONTENT_PAD, panel.y + CONTENT_PAD);
        match self.schema.menu_id {
            0..=5 => {
                let divider_y = py + 29 + (self.schema.menu_id + 1) as i32 * SLOT_SIZE;
                let width = panel.w.saturating_sub(16);
                out.fill_rect(UiRect::new(px + 4, divider_y, width, 1), palette::SEPARATOR);
            }
            6 => {
                draw_station_arrow(out, panel, (94, 26, 22, 18), true);
                draw_station_caption(out, panel, (92, 46), lang, palette::TEXT_DIM, "DISPENSER", "発射装置");
            }
            7 => self.draw_crafter_chrome(panel, out),
            8 => draw_station_arrow(out, panel, (104, 43, 20, 18), true),
            9 => draw_station_caption(out, panel, (58, 103), lang, palette::TEXT_DIM, "Payment", "支払い"),
            10 | 14 | 22 => {
                draw_station_arrow(out, panel, (80, 33, 20, 18), true);
                draw_station_caption(out, panel, (77, 55), lang, palette::TEXT_DIM, "COOK", "調理");
            }
            11 => {
                draw_station_caption(out, panel, (34, 73), lang, palette::TEXT_DIM, "BREW", "醸造");
                draw_station_arrow(out, panel, (77, 34, 16, 14), true);
            }
            12 => {
                draw_station_arrow(out, panel, (91, 26, 22, 18), true);
                draw_station_caption(out, panel, (91, 46), lang, palette::TEXT_DIM, "CRAFT", "作成");
            }
            13 => self.draw_enchant_chrome(panel, out),
            15 => draw_station_arrow(out, panel, (77, 26, 34, 24), false),
            16 => draw_station_caption(out, panel, (8, 5), lang, palette::TEXT_DARK, "HOPPER", "ホッパー"),
            17 => {
                draw_station_caption(out, panel, (61, 13), lang, palette::TEXT_DARK, "Lectern", "書見台");
                let page = format!("Page {}", self.property(0).max(0) + 1);
                out.text(px + 64, py + 61, 1, palette::TEXT_DIM, page);
            }
            18 => self.draw_loom_chrome(panel, out),
            19 => draw_station_caption(out, panel, (108, 4), lang, palette::TEXT_DARK, "Villager Trades", "村人取引"),
            20 => draw_station_caption(out, panel, (100, 5), lang, palette::TEXT_DARK, "SHULKER BOX", "シュルカー"),
            21 => self.draw_smithing_chrome(panel, out),
            23 => {
                draw_station_caption(out, panel, (93, 20), lang, palette::TEXT_DARK, "MAP PREVIEW", "地図");
                out.border_rect(content_rect(panel, 92, 34, 50, 38), palette::PANEL_BORDER, 1);
                out.text_str(px + 104, py + 48, 1, palette::ACCENT, tr(lang, "MAP", "地図"));
            }
            24 => self.draw_stonecutter_chrome(panel, out),
            _ => {}
        }
    }

    /// Crafter trigger arrow, its state frame and the TRIGGERED/IDLE caption.
    fn draw_crafter_chrome(&self, panel: UiRect, out: &mut UiDrawList) {
        let (px, py) = (panel.x + CONTENT_PAD, panel.y + CONTENT_PAD);
        let lang = self.ui_language;
        let triggered = self.property(9) != 0;
        out.text_str(px + 8, py + 5, 1, palette::TEXT_DARK, tr(lang, "CRAFTER", "クラフター"));
        let arrow = content_rect(panel, 104, 31, 20, 18);
        draw_vanilla_arrow(out, arrow, triggered);
        if triggered {
            out.border_rect(arrow, palette::BUTTON_BORDER_HOVER, 1);
        }
        let (color, label) = if triggered {
            (palette::ACCENT, tr(lang, "TRIGGERED", "起動"))
        } else {
            (palette::TEXT_DIM, tr(lang, "IDLE", "待機"))
        };
        out.text_str(px + 108, py + 50, 1, color, label);
    }

    /// Enchantment table: book slot and three clue/cost rows with tier pips.
    fn draw_enchant_chrome(&self, panel: UiRect, out: &mut UiDrawList) {
        let (px, py) = (panel.x + CONTENT_PAD, panel.y + CONTENT_PAD);
        let lang = self.ui_language;
        out.text_str(px + 66, py + 4, 1, palette::TEXT_DARK, tr(lang, "Enchantments", "エンチャント"));
        draw_frame(out, content_rect(panel, 17, 20, 22, 18), palette::BUTTON, palette::PANEL_BORDER);
        out.text_str(px + 22, py + 25, 1, palette::WARNING, tr(lang, "BOOK", "本"));
        for i in 0..3 {
            let y = py + 14 + i * 19;
            let (clue, level) = (self.property((4 + i) as i16), self.property((7 + i) as i16));
            // Legacy unsynchronized states used to expose the raw clue pair.
            // Keep it as a diagnostic suffix only until lapis/XP
            // synchronization arrives; synchronized rows show the name alone.
            let label = if clue >= 0 {
                let resolved = enchantment_clue_label(clue, level);
                if self.lapis_slot_synced || self.player_experience_level.is_some() {
                    resolved
                } else {
                    format!("{resolved} {clue} {level}")
                }
            } else {
                ["ᚠᚢᚦ", "ᚷᚺᚾ", "ᛁᛃᛇ"][i as usize].to_owned()
            };
            let enabled = self.enchanting_row_enabled(i as usize);
            let text_color = if enabled { palette::TEXT } else { palette::BUTTON_DISABLED_TEXT };
            let cost_color = if enabled { palette::ACCENT } else { palette::DANGER };
            out.text(px + 69, y + 5, 1, text_color, label);
            out.text(px + 146, y + 5, 1, cost_color, self.property(i as i16).to_string());
            out.fill_rect(UiRect::new(px + 62, y + 5, 7, 8), palette::ACCENT);
            out.text(px + 64, y + 5, 1, palette::TEXT_DARK, (i + 1).to_string());
        }
    }

    /// Loom pattern grid frame, its scroll thumb and the banner preview card.
    fn draw_loom_chrome(&self, panel: UiRect, out: &mut UiDrawList) {
        let (px, py) = (panel.x + CONTENT_PAD, panel.y + CONTENT_PAD);
        let lang = self.ui_language;
        out.text_str(px + 60, py + 2, 1, palette::TEXT_DARK, tr(lang, "Pattern", "模様"));
        out.border_rect(content_rect(panel, 119, 13, 3, 56), palette::PANEL_BORDER, 1);
        let patterns = self.loom_patterns.len();
        let grid = RecipeGrid::LOOM;
        grid.draw_scroll_thumb(out, panel, self.loom_scroll_row, patterns, (120, 14, 40));
        let card = content_rect(panel, 137, 4, 20, 40);
        draw_frame(out, card, palette::PANEL, palette::PANEL_BORDER);
        let banner = self
            .slots
            .first()
            .filter(|s| !s.is_empty() && !s.namespaced_id.is_empty())
            .map(|s| s.namespaced_id.as_str())
            .unwrap_or("minecraft:white_banner");
        out.item_icon_str(UiRect::new(card.x + 2, card.y + 3, 16, 20), banner);
        let Some(index) = self.selected_loom_pattern else {
            return;
        };
        let pattern = self.loom_patterns.get(index).map(String::as_str).unwrap_or("");
        out.fill_rect(UiRect::new(card.x + 8, card.y + 9, 5, 7), palette::ACCENT);
        out.text(card.x + 8, card.y + 8, 1, palette::TEXT_DARK, pattern_mark(pattern));
        out.text(card.x + 2, card.y + 27, 1, palette::ACCENT, clip_text(pattern, 16));
    }

    /// Smithing template/base/addition strip and the result frame.
    fn draw_smithing_chrome(&self, panel: UiRect, out: &mut UiDrawList) {
        let (px, py) = (panel.x + CONTENT_PAD, panel.y + CONTENT_PAD);
        let lang = self.ui_language;
        let caption = tr(lang, "TEMPLATE  BASE  ADDITION", "型 基材 追加");
        out.text(px + 8, py + 30, 1, palette::TEXT_DIM, clip_text(caption, (panel.w as i32 - 20).max(1)));
        draw_station_arrow(out, panel, (61, 41, 21, 18), true);
        let frame = content_rect(panel, 118, 10, 44, 62);
        draw_frame(out, frame, palette::SLOT, palette::PANEL_BORDER);
        let base = self.slots.get(1).filter(|s| !s.is_empty());
        if let Some(item) = base {
            if !item.namespaced_id.is_empty() {
                let icon = UiRect::new(frame.x + 12, frame.y + 12, 18, 18);
                out.item_icon_str(icon, &item.namespaced_id);
            }
            if let Some(material) = self.slots.get(2).filter(|s| !s.is_empty()) {
                if !material.namespaced_id.is_empty() {
                    let icon = UiRect::new(frame.x + 12, frame.y + 36, 18, 18);
                    out.item_icon_str(icon, &material.namespaced_id);
                }
                let accent = UiRect::new(frame.x + 8, frame.bottom() - 5, 28, 2);
                out.fill_rect(accent, palette::ACCENT);
            }
        }
        let pending = self.slots.first().is_some_and(|s| !s.is_empty())
            || self.slots.get(1).is_some_and(|s| !s.is_empty());
        if pending && self.slots.get(3).is_some_and(UiItemStack::is_empty) {
            out.text_str(px + 68, py + 45, 2, palette::DANGER, "X");
        }
    }

    /// Stonecutting recipe grid frame and its scroll thumb.
    fn draw_stonecutter_chrome(&self, panel: UiRect, out: &mut UiDrawList) {
        let (px, py) = (panel.x + CONTENT_PAD, panel.y + CONTENT_PAD);
        let lang = self.ui_language;
        out.text_str(px + 52, py + 2, 1, palette::TEXT_DARK, tr(lang, "Stonecutting", "石切り"));
        out.border_rect(content_rect(panel, 118, 14, 3, 54), palette::PANEL_BORDER, 1);
        let recipes = self.stonecutter_recipes.len();
        let grid = RecipeGrid::STONECUTTER;
        grid.draw_scroll_thumb(out, panel, self.stonecutter_scroll_row, recipes, (119, 15, 38));
    }
}

fn enchantment_clue_label(clue: i16, level: i16) -> String {
    let base = enchantment_clue_name(clue)
        .split(':')
        .next_back()
        .unwrap_or("unknown")
        .split('_')
        .map(|word| {
            let mut chars = word.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ");
    format!("{base} {}", roman_numeral(level.max(1)))
}

/// Protocol-767 `minecraft:enchantment` registry order used by synced clues.
const ENCHANTMENT_CLUES: [&str; 42] = [
    "minecraft:aqua_affinity",
    "minecraft:bane_of_arthropods",
    "minecraft:binding_curse",
    "minecraft:blast_protection",
    "minecraft:breach",
    "minecraft:channeling",
    "minecraft:density",
    "minecraft:depth_strider",
    "minecraft:efficiency",
    "minecraft:feather_falling",
    "minecraft:fire_aspect",
    "minecraft:fire_protection",
    "minecraft:flame",
    "minecraft:fortune",
    "minecraft:frost_walker",
    "minecraft:impaling",
    "minecraft:infinity",
    "minecraft:knockback",
    "minecraft:looting",
    "minecraft:loyalty",
    "minecraft:luck_of_the_sea",
    "minecraft:lure",
    "minecraft:mending",
    "minecraft:multishot",
    "minecraft:piercing",
    "minecraft:power",
    "minecraft:projectile_protection",
    "minecraft:protection",
    "minecraft:punch",
    "minecraft:quick_charge",
    "minecraft:respiration",
    "minecraft:riptide",
    "minecraft:sharpness",
    "minecraft:silk_touch",
    "minecraft:smite",
    "minecraft:soul_speed",
    "minecraft:sweeping_edge",
    "minecraft:swift_sneak",
    "minecraft:thorns",
    "minecraft:unbreaking",
    "minecraft:vanishing_curse",
    "minecraft:wind_burst",
];

/// Resolve a synced enchantment clue id; unknown ids keep the sentinel name.
fn enchantment_clue_name(clue: i16) -> &'static str {
    usize::try_from(clue)
        .ok()
        .and_then(|index| ENCHANTMENT_CLUES.get(index))
        .copied()
        .unwrap_or("minecraft:unknown")
}

/// Uppercase Roman numerals for levels `1..=10`; other levels stay decimal.
const ROMAN_NUMERALS: [&str; 10] = ["I", "II", "III", "IV", "V", "VI", "VII", "VIII", "IX", "X"];

fn roman_numeral(level: i16) -> String {
    usize::try_from(level)
        .ok()
        .and_then(|index| index.checked_sub(1))
        .and_then(|index| ROMAN_NUMERALS.get(index))
        .map_or_else(|| level.to_string(), |numeral| (*numeral).to_owned())
}

/// Vanilla loom pattern ids in registry order.
const LOOM_PATTERNS: [&str; 40] = [
    "square_bottom_left", "square_bottom_right", "square_top_left", "square_top_right",
    "stripe_bottom", "stripe_top", "stripe_left", "stripe_right",
    "stripe_center", "stripe_middle", "stripe_downright", "stripe_downleft",
    "small_stripes", "cross", "straight_cross", "triangle_bottom",
    "triangle_top", "triangles_bottom", "triangles_top", "diagonal_left",
    "diagonal_up_right", "circle", "rhombus", "half_vertical",
    "half_horizontal", "half_vertical_right", "half_horizontal_bottom", "border",
    "curly_border", "gradient", "gradient_up", "bricks",
    "skull", "flower", "creeper", "mojang",
    "globe", "piglin", "flow", "guster",
];

fn default_loom_patterns() -> Vec<String> {
    LOOM_PATTERNS.iter().map(|pattern| format!("minecraft:{pattern}")).collect()
}

fn ghost_item(menu_id: u32, slot: i16) -> Option<&'static str> {
    match (menu_id, slot) {
        (8, 0) => Some("minecraft:iron_sword"),
        (9, 0) => Some("minecraft:iron_ingot"),
        (10 | 14 | 22, 0) => Some("minecraft:iron_ore"),
        (10 | 14 | 22, 1) => Some("minecraft:coal"),
        (11, 0..=2) => Some("minecraft:glass_bottle"),
        (11, 4) => Some("minecraft:blaze_powder"),
        (12, 1..=9) => Some("minecraft:oak_planks"),
        (13, 0) => Some("minecraft:book"),
        (13, 1) => Some("minecraft:lapis_lazuli"),
        (18, 0) => Some("minecraft:white_banner"),
        (18, 1) => Some("minecraft:white_dye"),
        (18, 2) => Some("minecraft:paper"),
        (21, 0) => Some("minecraft:smithing_template"),
        (21, 1) => Some("minecraft:diamond_chestplate"),
        (21, 2) => Some("minecraft:netherite_ingot"),
        (23, 0) => Some("minecraft:filled_map"),
        (23, 1) => Some("minecraft:paper"),
        _ => None,
    }
}

fn clip_text(text: &str, max_width: i32) -> String {
    let mut out = String::new();
    for ch in text.chars() {
        let mut candidate = out.clone();
        candidate.push(ch);
        if text_width(&candidate, 1) as i32 > max_width && !out.is_empty() {
            break;
        }
        out = candidate;
    }
    out
}

impl ContainerScreenState {
    fn finish_action(
        &self,
        slot: i16,
        button: i8,
        mode: u8,
        changed: &BTreeSet<i16>,
    ) -> ClickWindowAction {
        ClickWindowAction {
            window_id: self.window_id,
            state_id: self.state_id,
            slot,
            button,
            mode,
            changed_slots: changed
                .iter()
                .filter_map(|&s| {
                    usize::try_from(s)
                        .ok()
                        .and_then(|i| self.slots.get(i))
                        .map(|stack| SlotStackChange {
                            slot: s,
                            stack: stack.clone(),
                        })
                })
                .collect(),
            carried_item: self.cursor.clone().filter(|s| !s.is_empty()),
        }
    }

    /// Replace the cursor stack, dropping it once its count reaches zero.
    fn set_carried(&mut self, stack: UiItemStack) {
        self.cursor = (stack.count > 0).then_some(stack);
    }

    /// Replace `slot`'s stack with `stack` at `count`, emptying it at zero.
    fn set_counted_slot(&mut self, slot: usize, mut stack: UiItemStack, count: u8) {
        stack.count = count;
        self.slots[slot] = if count == 0 { UiItemStack::empty() } else { stack };
    }

    /// Mode-0 click: pickup/place, merge, swap, or release outside the panel.
    /// Each arm mutates the slot/cursor and records the slot in `changed`; a
    /// single trailing [`Self::finish_action`] reports the result.
    fn normal_click(&mut self, slot_index: i16, button: i8) -> Option<ClickWindowAction> {
        let mut changed = BTreeSet::new();
        if slot_index == EMPTY_SLOT_INDEX {
            return self.release_cursor(button, &changed);
        }
        let index = usize::try_from(slot_index).ok()?;
        if index >= self.slots.len() || !self.can_take(slot_index) {
            return None;
        }
        let slot_stack = self.slots[index].clone();
        let cursor = self.cursor.clone().filter(|s| !s.is_empty());
        if button == 2 && !slot_stack.is_empty() {
            // Creative clone pickup (middle click / single-slot clone drag):
            // the cursor receives a copy of the slot stack; the slot and
            // its count are untouched.
            self.cursor = Some(slot_stack);
            return Some(self.finish_action(slot_index, button, 0, &changed));
        }
        match (slot_stack.is_empty(), cursor) {
            (true, None) => {}
            (true, Some(carried)) => {
                self.place_into_empty(slot_index, index, carried, button, &mut changed)
            }
            (false, None) => {
                self.take_from_slot(slot_index, index, slot_stack, button, &mut changed)
            }
            (false, Some(carried)) => {
                self.click_occupied(slot_index, index, slot_stack, carried, button, &mut changed)
            }
        }
        Some(self.finish_action(slot_index, button, 0, &changed))
    }

    /// Drop (`button == 0`) or decrement (`button == 1`) the carried stack when
    /// the click lands outside any slot; other buttons are not pickup clicks.
    fn release_cursor(
        &mut self,
        button: i8,
        changed: &BTreeSet<i16>,
    ) -> Option<ClickWindowAction> {
        let Some(mut carried) = self.cursor.clone() else {
            return Some(self.finish_action(EMPTY_SLOT_INDEX, button, 0, changed));
        };
        match button {
            0 => self.cursor = None,
            1 => {
                carried.count = carried.count.saturating_sub(1);
                self.set_carried(carried);
            }
            _ => return None,
        }
        Some(self.finish_action(EMPTY_SLOT_INDEX, button, 0, changed))
    }

    /// Place the carried stack into an empty slot (right click places one).
    fn place_into_empty(
        &mut self,
        slot_index: i16,
        index: usize,
        mut carried: UiItemStack,
        button: i8,
        changed: &mut BTreeSet<i16>,
    ) {
        if !self.accepts_placement(slot_index, &carried) {
            return;
        }
        let wanted = if button == 1 { 1u32 } else { u32::from(carried.count) };
        let place = wanted.min(u32::from(carried.max_stack)) as u8;
        let mut placed = carried.clone();
        placed.count = place;
        carried.count -= place;
        self.slots[index] = placed;
        self.set_carried(carried);
        changed.insert(slot_index);
    }

    /// Pick the slot's stack up whole (left) or split it in half (right).
    fn take_from_slot(
        &mut self,
        slot_index: i16,
        index: usize,
        slot_stack: UiItemStack,
        button: i8,
        changed: &mut BTreeSet<i16>,
    ) {
        let take = if button == 1 {
            slot_stack.count.div_ceil(2)
        } else {
            slot_stack.count
        };
        let mut taken = slot_stack.clone();
        taken.count = take;
        let left = slot_stack.count.saturating_sub(take);
        self.set_counted_slot(index, slot_stack, left);
        self.cursor = Some(taken);
        changed.insert(slot_index);
    }

    /// Cursor over an occupied slot: output pickup, stack merge, or swap.
    fn click_occupied(
        &mut self,
        slot_index: i16,
        index: usize,
        slot_stack: UiItemStack,
        mut carried: UiItemStack,
        button: i8,
        changed: &mut BTreeSet<i16>,
    ) {
        if !self.accepts_placement(slot_index, &carried) {
            // Output slot with a matching cursor: Vanilla takes the output
            // into the cursor when the merged count still fits.
            let fits = u32::from(carried.count) + u32::from(slot_stack.count)
                <= u32::from(carried.max_stack);
            if matches!(self.slot_kind(slot_index), Some(SlotKind::Output))
                && slot_stack.same_stackable(&carried)
                && fits
            {
                carried.count += slot_stack.count;
                self.slots[index] = UiItemStack::empty();
                self.cursor = Some(carried);
                changed.insert(slot_index);
            }
            return;
        }
        if !slot_stack.same_stackable(&carried) {
            let old_slot = self.slots[index].clone();
            self.slots[index] = carried;
            self.cursor = Some(old_slot);
            changed.insert(slot_index);
            return;
        }
        let wanted = if button == 1 { 1 } else { u32::from(carried.count) };
        let amount = self.add_to_slot(index, &carried, wanted);
        if amount == 0 {
            return;
        }
        carried.count -= amount as u8;
        self.set_carried(carried);
        changed.insert(slot_index);
    }

    /// Add up to `amount` of `stack` to `slot`, merging into a compatible
    /// stack or filling the empty slot. Returns how many items were placed.
    fn add_to_slot(&mut self, slot: usize, stack: &UiItemStack, amount: u32) -> u32 {
        let current = &self.slots[slot];
        if !current.is_empty() && !current.same_stackable(stack) {
            return 0;
        }
        // An empty slot takes the source's own stack limit; a merge fills the
        // compatible stack's remaining room.
        let room = if current.is_empty() {
            u32::from(stack.max_stack)
        } else {
            u32::from(current.max_stack).saturating_sub(u32::from(current.count))
        };
        let amount = amount.min(room);
        if amount == 0 {
            return 0;
        }
        if current.is_empty() {
            let mut placed = stack.clone();
            placed.count = amount as u8;
            self.slots[slot] = placed;
        } else {
            self.slots[slot].count += amount as u8;
        }
        amount
    }

    fn transfer_range(&mut self, source: i16, targets: &[i16]) -> Option<BTreeSet<i16>> {
        let src = usize::try_from(source).ok()?;
        let mut stack = self.slots.get(src)?.clone();
        if stack.is_empty() {
            return None;
        }
        let mut changed = BTreeSet::new();
        // Pass 1 (Vanilla order): merge into existing compatible partial stacks first.
        for &target in targets {
            if stack.is_empty() {
                break;
            }
            let t = usize::try_from(target).ok()?;
            if t >= self.slots.len() || !self.accepts_placement(target, &stack) {
                continue;
            }
            if self.slots[t].is_empty() || !self.slots[t].same_stackable(&stack) {
                continue;
            }
            let placed = self.add_to_slot(t, &stack, u32::from(stack.count)) as u8;
            if placed > 0 {
                stack.count -= placed;
                changed.insert(target);
            }
        }
        // Pass 2: fill empty slots with whatever remains.
        for &target in targets {
            if stack.is_empty() {
                break;
            }
            let t = usize::try_from(target).ok()?;
            if t >= self.slots.len() || !self.accepts_placement(target, &stack) {
                continue;
            }
            if self.slots[t].is_empty() {
                let placed = self.add_to_slot(t, &stack, u32::from(stack.count)) as u8;
                stack.count -= placed;
                changed.insert(target);
            }
        }
        let remaining = stack.count;
        self.set_counted_slot(src, stack, remaining);
        changed.insert(source);
        Some(changed)
    }

    fn quick_move(&mut self, slot_index: i16, button: i8) -> Option<ClickWindowAction> {
        if slot_index < 0 || usize::try_from(slot_index).ok()? >= self.slots.len() {
            return None;
        }
        let kind = self.slot_kind(slot_index)?;
        let targets: Vec<i16> = if self.is_player_slot(slot_index) {
            (0..self.slots.len() as i16)
                .filter(|&i| {
                    !self.is_player_slot(i)
                        && !matches!(
                            self.slot_kind(i),
                            Some(SlotKind::Output) | Some(SlotKind::Locked) | None
                        )
                })
                .collect()
        } else {
            if !self.schema.has_player_inventory || matches!(kind, SlotKind::Locked) {
                return None;
            }
            (self.schema.inventory_start..self.schema.inventory_start + 36).collect()
        };
        if targets.is_empty() {
            return None;
        }
        let changed = self.transfer_range(slot_index, &targets)?;
        Some(self.finish_action(slot_index, button, 1, &changed))
    }

    /// Swap two slots when each side accepts what the other holds. Emits no
    /// action; callers report the touched slots themselves.
    fn swap_slots(&mut self, a: usize, b: usize) -> bool {
        let (a_index, b_index) = (a as i16, b as i16);
        let (a_stack, b_stack) = (self.slots[a].clone(), self.slots[b].clone());
        let fits = (a_stack.is_empty() || self.accepts_placement(b_index, &a_stack))
            && (b_stack.is_empty() || self.accepts_placement(a_index, &b_stack));
        if fits {
            self.slots[a] = b_stack;
            self.slots[b] = a_stack;
        }
        fits
    }

    fn hotbar_swap(&mut self, slot_index: i16, hotbar_index: i8) -> Option<ClickWindowAction> {
        let index = usize::try_from(slot_index).ok()?;
        if index >= self.slots.len() || slot_index == EMPTY_SLOT_INDEX {
            return None;
        }
        if hotbar_index == 40 {
            let mut changed = BTreeSet::new();
            // Window-0 player inventory carries the offhand at slot 45; predict its swap locally.
            let offhand = self.slots.len() == 46
                && self.slot_kind(45) == Some(SlotKind::Offhand)
                && index != 45;
            if offhand && self.swap_slots(index, 45) {
                changed.insert(slot_index);
                changed.insert(45);
            }
            return Some(self.finish_action(slot_index, hotbar_index, 2, &changed));
        }
        if !(0..=8).contains(&hotbar_index) || !self.schema.has_player_inventory {
            return None;
        }
        let hotbar_slot = self.schema.inventory_start + 27 + i16::from(hotbar_index);
        let hotbar = usize::try_from(hotbar_slot).ok()?;
        if hotbar >= self.slots.len() || hotbar == index {
            return None;
        }
        if !self.swap_slots(index, hotbar) {
            return None;
        }
        let mut changed = BTreeSet::new();
        changed.insert(slot_index);
        changed.insert(hotbar_slot);
        Some(self.finish_action(slot_index, hotbar_index, 2, &changed))
    }

    fn drop_items(&mut self, slot_index: i16, button: i8) -> Option<ClickWindowAction> {
        if self.cursor.is_some() || slot_index < 0 {
            return None;
        }
        let index = usize::try_from(slot_index).ok()?;
        if index >= self.slots.len() || !self.can_take(slot_index) {
            return None;
        }
        let stack = self.slots[index].clone();
        if stack.is_empty() {
            return Some(self.finish_action(slot_index, button, 4, &BTreeSet::new()));
        }
        let amount = if button == 1 { stack.count } else { 1 };
        let left = stack.count.saturating_sub(amount);
        self.set_counted_slot(index, stack, left);
        let mut changed = BTreeSet::new();
        changed.insert(slot_index);
        Some(self.finish_action(slot_index, button, 4, &changed))
    }

    /// Whether `slot_index` can receive `carried` in a drag distribution:
    /// in range, placement-accepting (never `Output`/`Locked`), and either
    /// empty or holding a fully stack-compatible stack — the local mirror
    /// of vanilla `canItemQuickReplace` + `mayPlace` + `canDragTo`.
    fn drag_can_admit(&self, slot_index: i16, carried: &UiItemStack) -> bool {
        let Ok(index) = usize::try_from(slot_index) else {
            return false;
        };
        let Some(existing) = self.slots.get(index) else {
            return false;
        };
        self.accepts_placement(slot_index, carried)
            && (existing.is_empty() || existing.same_stackable(carried))
    }

    /// Map a mouse button to its QUICK_CRAFT distribution kind:
    /// `0` even split, `1` one-per-slot, `2` creative clone.
    fn drag_kind(button: i8) -> Option<u8> {
        match button {
            0 => Some(0),
            1 => Some(1),
            2 => Some(2),
            _ => None,
        }
    }

    /// Protocol-767 mode-5 button for `(kind, stage)`: kinds map to the
    /// `0/4/8` bases and stages to `start`/`add_slot`/`end` offsets.
    const fn drag_wire_button(kind: u8, stage: u8) -> i8 {
        (kind as i8) * 4 + stage as i8
    }

    /// Record one real-slot press and report whether it completes a
    /// vanilla double-click (same slot, button `0`, within 300 ms of the
    /// previous press). Every `slotClicked` equivalent routes through
    /// this so the tracking window matches vanilla's
    /// `lastClickSlot/Time/Button` bookkeeping.
    fn note_press_is_double(&mut self, slot_index: i16, button: i8) -> bool {
        let now = Instant::now();
        let double = button == 0
            && matches!(self.last_slot_click,
                Some((at, slot, btn))
                    if slot == slot_index
                        && btn == 0
                        && now.duration_since(at) <= Duration::from_millis(300));
        self.last_slot_click = Some((now, slot_index, button));
        double
    }

    /// Vanilla's `PICKUP_ALL` precondition on the clicked slot: a
    /// stackable stack that may be taken while the cursor carries items.
    fn double_click_can_gather(&self, slot_index: i16) -> bool {
        let Some(carried) = self.cursor.as_ref().filter(|s| !s.is_empty()) else {
            return false;
        };
        let Ok(index) = usize::try_from(slot_index) else {
            return false;
        };
        self.slots
            .get(index)
            .is_some_and(|stack| !stack.is_empty() && stack.same_stackable(carried))
            && self.can_take(slot_index)
    }

    /// Mode-5 gesture entry point shared by [`Self::drag_start`],
    /// [`Self::drag_over`] and [`Self::click_mode`]. With no active drag it
    /// arms the gesture and emits the START packet; with one it collects
    /// `slot_index` and emits its ADD_SLOT packet. `-999` ends the drag.
    fn drag_click(&mut self, slot_index: i16, button: i8) -> Option<ClickWindowAction> {
        self.drag_click_inner(slot_index, button, false)
    }

    /// [`Self::drag_click`] with `press_noted = true` when the caller
    /// already ran the press through [`Self::note_press_is_double`]
    /// (the [`Self::click_mode`] mode-5 path).
    fn drag_click_inner(
        &mut self,
        slot_index: i16,
        button: i8,
        press_noted: bool,
    ) -> Option<ClickWindowAction> {
        let kind = Self::drag_kind(button)?;
        // Hosts that route a nonempty-cursor press here (instead of
        // `click_mode`) can still land inside the 300 ms double-click
        // window; vanilla resolves that to `PICKUP_ALL` *before* the
        // click-type dispatch, so the second press gathers instead of
        // being swallowed by the drag arm.
        let double = !press_noted
            && slot_index != EMPTY_SLOT_INDEX
            && self.note_press_is_double(slot_index, button);
        if self.drag_mode.is_none() {
            // Vanilla only quick-crafts while the cursor carries items.
            self.cursor.as_ref().filter(|s| !s.is_empty())?;
            if double && self.double_click_can_gather(slot_index) {
                return self.collect_to_cursor(slot_index, button);
            }
            self.drag_mode = Some(DragState {
                button: kind,
                slots: Vec::new(),
                press_slot: slot_index,
                press_button: button,
            });
            return Some(self.finish_action(
                EMPTY_SLOT_INDEX,
                Self::drag_wire_button(kind, 0),
                5,
                &BTreeSet::new(),
            ));
        }
        if slot_index == EMPTY_SLOT_INDEX {
            return self.drag_end();
        }
        let index = usize::try_from(slot_index).ok()?;
        if index >= self.slots.len() {
            return None;
        }
        let carried = self.cursor.clone().filter(|s| !s.is_empty())?;
        let drag_kind = self.drag_mode.as_ref()?.button;
        if !self.drag_can_admit(slot_index, &carried) {
            return None;
        }
        let drag = self.drag_mode.as_mut()?;
        if drag.slots.contains(&slot_index) {
            return None;
        }
        // Vanilla stops collecting once every carried item has a slot;
        // the creative clone drag is exempt from the cap.
        let cap = if drag_kind == 2 {
            usize::MAX
        } else {
            usize::from(carried.count)
        };
        if drag.slots.len() >= cap {
            return None;
        }
        drag.slots.push(slot_index);
        Some(self.finish_action(
            slot_index,
            Self::drag_wire_button(drag_kind, 1),
            5,
            &BTreeSet::new(),
        ))
    }

    /// Begin a protocol-mode-5 distribution gesture. Button `0` evenly
    /// splits the carried stack, `1` places one item per slot, `2` clones
    /// the carried stack into every collected slot (creative).
    pub fn drag_start(&mut self, slot: i16, button: i8) -> Option<ClickWindowAction> {
        if self.drag_mode.is_some() {
            return None;
        }
        self.drag_click(slot, button)
    }

    /// Add a slot to an active drag distribution.
    pub fn drag_over(&mut self, slot: i16, button: i8) -> Option<ClickWindowAction> {
        self.drag_mode.as_ref()?;
        self.drag_click(slot, button)
    }

    /// Finish a drag. Fewer than two collected slots falls back to the
    /// vanilla normal click (pickup with the drag kind as click type);
    /// two or more distribute `floor(carried / slots)` (kind 0), `1`
    /// (kind 1) or a full-stack clone (kind 2) into each collected slot,
    /// leaving the indivisible remainder on the cursor.
    pub fn drag_end(&mut self) -> Option<ClickWindowAction> {
        let drag = self.drag_mode.take()?;
        match drag.slots.len() {
            0 => return self.normal_click(drag.press_slot, drag.press_button),
            1 => {
                let slot = drag.slots[0];
                let button = match drag.button {
                    0 => 0,
                    1 => 1,
                    _ => 2,
                };
                return self.normal_click(slot, button);
            }
            _ => {}
        }
        let carried = self.cursor.clone().filter(|s| !s.is_empty())?;
        let slot_total = drag.slots.len() as u32;
        // Vanilla `getQuickCraftPlaceCount`: uniform per-slot amount computed
        // from the original carried count, before any slot is touched.
        let place = match drag.button {
            0 => u32::from(carried.count) / slot_total,
            1 => 1,
            _ => u32::from(carried.max_stack),
        };
        let mut remaining = u32::from(carried.count);
        let mut changed = BTreeSet::new();
        for &slot_index in &drag.slots {
            let Ok(index) = usize::try_from(slot_index) else {
                continue;
            };
            if index >= self.slots.len() || !self.drag_can_admit(slot_index, &carried) {
                continue;
            }
            let existing = &self.slots[index];
            let carry = u32::from(existing.count);
            // Vanilla caps at min(source max, slot max); an empty slot
            // takes the carried stack's own limit.
            let slot_max = if existing.is_empty() {
                u32::from(carried.max_stack)
            } else {
                u32::from(existing.max_stack)
            };
            let room = slot_max.min(u32::from(carried.max_stack)).saturating_sub(carry);
            let added = place.min(room);
            if added == 0 {
                continue;
            }
            let placed = self.add_to_slot(index, &carried, added);
            if placed == 0 {
                continue;
            }
            remaining = remaining.saturating_sub(placed);
            changed.insert(slot_index);
        }
        if drag.button != 2 {
            // The creative clone drag does not consume the carried stack.
            if let Some(mut cursor) = self.cursor.clone() {
                cursor.count = remaining.min(u32::from(cursor.count)) as u8;
                self.set_carried(cursor);
            }
        }
        Some(self.finish_action(
            EMPTY_SLOT_INDEX,
            Self::drag_wire_button(drag.button, 2),
            5,
            &changed,
        ))
    }

    fn collect_to_cursor(&mut self, slot_index: i16, button: i8) -> Option<ClickWindowAction> {
        if slot_index < 0 || !self.can_take(slot_index) {
            return None;
        }
        let mut cursor = self.cursor.clone().filter(|s| !s.is_empty())?;
        let mut changed = BTreeSet::new();
        for pass in 0..2 {
            let indices: Vec<i16> = (0..self.slots.len() as i16).collect();
            let iter: Box<dyn Iterator<Item = i16>> = if pass == 0 {
                Box::new(indices.into_iter())
            } else {
                Box::new(indices.into_iter().rev())
            };
            for index in iter {
                if u32::from(cursor.count) >= u32::from(cursor.max_stack) {
                    break;
                }
                if matches!(
                    self.slot_kind(index),
                    Some(SlotKind::Output) | Some(SlotKind::Locked)
                ) {
                    continue;
                }
                let slot = usize::try_from(index).ok()?;
                let stack = self.slots[slot].clone();
                if stack.is_empty() || !stack.same_stackable(&cursor) {
                    continue;
                }
                let amount = u32::from(stack.count)
                    .min(u32::from(cursor.max_stack) - u32::from(cursor.count));
                if amount == 0 {
                    continue;
                }
                // The slot keeps whatever the cursor could not absorb.
                let left = stack.count - amount as u8;
                cursor.count += amount as u8;
                self.set_counted_slot(slot, stack, left);
                changed.insert(index);
            }
        }
        if changed.is_empty() {
            return Some(self.finish_action(slot_index, button, 6, &changed));
        }
        self.cursor = Some(cursor);
        Some(self.finish_action(slot_index, button, 6, &changed))
    }

    /// Dispatch a click-window interaction. `mode` follows the protocol:
    /// `0` pickup/place, `1` quick-move, `2` hotbar swap, `4` drop,
    /// `5` drag, `6` collect-to-cursor.
    ///
    /// Vanilla resolves the 300 ms double-click into `PICKUP_ALL`
    /// *before* the click-type dispatch, so a second left press on the
    /// same slot with a nonempty cursor gathers here instead of running
    /// the pickup/merge arm — even when a host routed the press at the
    /// nonempty-cursor drag path.
    pub fn click_mode(&mut self, slot: i16, button: i8, mode: u8) -> Option<ClickWindowAction> {
        let double = slot != EMPTY_SLOT_INDEX && self.note_press_is_double(slot, button);
        if mode == 0 && double && self.double_click_can_gather(slot) {
            return self.collect_to_cursor(slot, button);
        }
        match mode {
            0 => self.normal_click(slot, button),
            1 => self.quick_move(slot, button),
            2 => self.hotbar_swap(slot, button),
            4 => self.drop_items(slot, button),
            5 => self.drag_click_inner(slot, button, true),
            6 => self.collect_to_cursor(slot, button),
            _ => None,
        }
    }

    /// Left (`button = 0`) or right (`button = 1`) click, mode `0`.
    pub fn click(&mut self, slot: i16, button: i8) -> Option<ClickWindowAction> {
        self.click_mode(slot, button, 0)
    }

    /// Click a slot, converting Shift+click to the vanilla quick-move mode.
    pub fn click_with_shift(
        &mut self,
        slot: i16,
        button: i8,
        shift_held: bool,
    ) -> Option<ClickWindowAction> {
        self.click_mode(slot, button, if shift_held { 1 } else { 0 })
    }

    /// Apply a vanilla inventory keyboard shortcut to the currently
    /// hovered slot. Hotbar index `0..=8` maps directly to protocol button.
    pub fn handle_slot_hotkey(
        &mut self,
        key: ContainerSlotHotkey,
    ) -> Option<ClickWindowAction> {
        let slot = i16::try_from(self.hovered_slot?).ok()?;
        match key {
            ContainerSlotHotkey::HotbarSwap(index) if index <= 8 => {
                self.click_mode(slot, i8::try_from(index).ok()?, 2)
            }
            ContainerSlotHotkey::HotbarSwap(_) => None,
            ContainerSlotHotkey::OffhandSwap => self.click_mode(slot, 40, 2),
            ContainerSlotHotkey::DropOne => self.click_mode(slot, 0, 4),
            ContainerSlotHotkey::DropStack => self.click_mode(slot, 1, 4),
            ContainerSlotHotkey::DoubleClickGather => self.click_mode(slot, 0, 6),
        }
    }

    /// Deterministic `container_button_click` (`0x0d`) emission.
    pub fn button_click(&mut self, button_id: i32) -> Option<ContainerButtonClick> {
        let valid = self.schema.buttons.iter().any(|b| {
            if b.dynamic {
                button_id >= 0
            } else {
                b.id == button_id
            }
        }) || match self.schema.menu_id {
            7 | 9 => button_id == 0,
            18 | 24 => button_id >= 0,
            17 => button_id == 1 || button_id == 2 || button_id == 3 || button_id >= 100,
            13 => (0..=2).contains(&button_id),
            _ => false,
        };
        valid.then_some(ContainerButtonClick {
            window_id: self.window_id,
            button_id,
        })
    }
}

/// QoL inventory-sort state (Inventory-Sort / inventory drag controls semantics).
///
/// Tracks the sort-key toggle/hover state and performs deterministic
/// vanilla-mod-style sorting over a [`ContainerScreenState`]: partial
/// stacks that are fully stack-compatible are merged up to `max_stack`,
/// then stacks are ordered by count descending with `item_id` ascending
/// as the tie-break and empty slots sink to the end of the range.
#[derive(Clone, Debug)]
pub struct InventorySortState {
    /// Whether the sort keybinding is armed (settings-gated by consumers).
    pub enabled: bool,
    /// Slot the pointer currently hovers, used for region picking.
    pub hovered_slot: Option<usize>,
    /// Monotonic tick of the most recent completed sort (`None` = never).
    pub last_sort_tick: Option<u64>,
    /// Number of completed sort operations.
    pub sort_count: u64,
    /// Slot range touched by the most recent sort.
    pub last_sorted_range: Option<Range<usize>>,
    /// `true` while a sort operation is executing. Consumers driving an
    /// animated sort may hold it via [`Self::begin_sort`]/[`Self::end_sort`].
    pub sort_in_progress: bool,
    tick: u64,
}

impl Default for InventorySortState {
    fn default() -> Self {
        Self {
            enabled: true,
            hovered_slot: None,
            last_sort_tick: None,
            sort_count: 0,
            last_sorted_range: None,
            sort_in_progress: false,
            tick: 0,
        }
    }
}

/// Deterministic ordering key for non-empty stacks: count descending,
/// then `item_id` ascending, then every stackable-relevant field so the
/// result never depends on input order.
type StackSortKey = (
    Reverse<u16>,
    u32,
    String,
    i32,
    i32,
    Vec<String>,
    Vec<UiEnchantment>,
    (u8, u8, u8, u8),
);

fn stack_sort_key(stack: &UiItemStack) -> StackSortKey {
    let color = stack.rarity_color;
    (
        Reverse(u16::from(stack.count)),
        stack.item_id,
        stack.display_name.clone(),
        stack.durability,
        stack.max_durability,
        stack.lore.clone(),
        stack.enchantments.clone(),
        (color.r, color.g, color.b, color.a),
    )
}

impl InventorySortState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Mark the start of a consumer-driven (e.g. animated) sort.
    pub fn begin_sort(&mut self) {
        self.sort_in_progress = true;
    }

    /// Mark the end of a consumer-driven sort.
    pub fn end_sort(&mut self) {
        self.sort_in_progress = false;
    }

    /// Contiguous sortable region containing `index`.
    ///
    /// Player-inventory slots map to the 36-slot player region
    /// (`inventory_start..inventory_start + 36`). Every other slot maps to
    /// the container region: the span covering all non-player slots whose
    /// [`SlotKind`] accepts placement (i.e. not `Output`/`Locked`), so
    /// result slots are never swept into a sort.
    /// Player-inventory slot span, clipped to the screen's slot count and
    /// empty for schemas without one.
    fn player_range(screen: &ContainerScreenState) -> Range<usize> {
        let start = screen.schema().inventory_start.max(0) as usize;
        if screen.schema().has_player_inventory {
            start..(start + 36).min(screen.slot_count())
        } else {
            start..start
        }
    }

    pub fn slot_range_for(&self, screen: &ContainerScreenState, index: usize) -> Range<usize> {
        let player = Self::player_range(screen);
        if player.contains(&index) {
            player
        } else {
            self.container_range(screen)
        }
    }

    /// The sortable container region (non-player, placement-accepting
    /// slots) as one contiguous span, or `0..0` when none exists.
    pub fn container_range(&self, screen: &ContainerScreenState) -> Range<usize> {
        let len = screen.slot_count();
        let player = Self::player_range(screen);
        let mut lo = len;
        let mut hi = 0usize;
        for i in 0..len {
            if player.contains(&i) {
                continue;
            }
            match screen.slot_kind(i as i16) {
                Some(SlotKind::Output) | Some(SlotKind::Locked) | None => continue,
                _ => {}
            }
            lo = lo.min(i);
            hi = i + 1;
        }
        if lo < hi { lo..hi } else { 0..0 }
    }

    /// Region sorted when no slot is hovered: the player inventory when
    /// the schema provides one, otherwise the container region.
    pub fn default_sort_range(&self, screen: &ContainerScreenState) -> Range<usize> {
        if screen.schema().has_player_inventory {
            Self::player_range(screen)
        } else {
            self.container_range(screen)
        }
    }

    /// Deterministic vanilla-mod-style sort over `range`.
    ///
    /// (a) Stacks inside `range` that are fully stack-compatible
    /// ([`UiItemStack::same_stackable`]: same `item_id`, `max_stack`,
    /// durability, display name, lore, enchantments and rarity) are merged
    /// up to `max_stack`; fully consumed sources become
    /// [`UiItemStack::empty`]. (b) The resulting stacks are ordered by
    /// count descending, `item_id` ascending as tie-break, then every
    /// remaining stackable-relevant field; empties sink to the end of the
    /// range. Only slots inside `range` are modified.
    pub fn sort_container_range(
        &mut self,
        screen: &mut ContainerScreenState,
        range: Range<usize>,
    ) {
        let start = range.start.min(screen.slot_count());
        let end = range.end.min(screen.slot_count());
        if start >= end {
            return;
        }
        self.sort_in_progress = true;
        self.tick += 1;
        self.last_sort_tick = Some(self.tick);
        self.sort_count += 1;
        self.last_sorted_range = Some(start..end);

        let mut merged: Vec<UiItemStack> = Vec::with_capacity(end - start);
        for slot in screen.slots.iter().take(end).skip(start) {
            let mut stack = slot.clone();
            if stack.is_empty() {
                continue;
            }
            let mut fully_placed = false;
            for target in merged.iter_mut() {
                if !target.same_stackable(&stack) {
                    continue;
                }
                let capacity = u32::from(target.max_stack)
                    .saturating_sub(u32::from(target.count));
                if capacity == 0 {
                    continue;
                }
                let amount = u32::from(stack.count).min(capacity) as u8;
                target.count += amount;
                stack.count -= amount;
                if stack.count == 0 {
                    fully_placed = true;
                    break;
                }
            }
            if !fully_placed && stack.count != 0 {
                merged.push(stack);
            }
        }
        let mut non_empty: Vec<UiItemStack> = merged
            .into_iter()
            .filter(|s| !s.is_empty())
            .collect();
        non_empty.sort_by_cached_key(stack_sort_key);

        for (offset, index) in (start..end).enumerate() {
            screen.slots[index] = non_empty.get(offset).cloned().unwrap_or_else(UiItemStack::empty);
        }
        self.sort_in_progress = false;
    }

    /// Sort-key press (vanilla mods bind `R` or middle-click).
    ///
    /// Sorts the region containing `hovered_slot`: the player-inventory
    /// region for player slots, the container region for container slots,
    /// and the player region (or container region when the schema has no
    /// player inventory) when nothing is hovered. No-op while `enabled`
    /// is `false`.
    pub fn toggle_sort_key(
        &mut self,
        hovered_slot: Option<usize>,
        screen: &mut ContainerScreenState,
    ) {
        self.hovered_slot = hovered_slot.filter(|&s| s < screen.slot_count());
        if !self.enabled {
            return;
        }
        let range = match self.hovered_slot {
            Some(index) => self.slot_range_for(screen, index),
            None => self.default_sort_range(screen),
        };
        self.sort_container_range(screen, range);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slot_grid_geometry_uses_vanilla_row_major_pitch() {
        let grid = SlotGridGeometry::new(8, 18, 9);
        assert_eq!(grid.slot_origin(0), (8, 18));
        assert_eq!(grid.slot_origin(8), (8 + 8 * SLOT_SIZE, 18));
        assert_eq!(grid.slot_origin(9), (8, 18 + SLOT_SIZE));
        assert_eq!(
            grid.slot_rect(10),
            UiRect::new(
                8 + SLOT_SIZE,
                18 + SLOT_SIZE,
                SLOT_SIZE as u32,
                SLOT_SIZE as u32,
            )
        );
    }

    #[test]
    fn player_inventory_slots_share_slot_grid_geometry() {
        let screen = ContainerScreenState::new(menu_schema(2).unwrap(), 1, 0, "Chest");
        assert_eq!(
            screen.slot_origin(27),
            Some((8, 85, SlotKind::Normal)),
            "first player-inventory row slot",
        );
        assert_eq!(
            screen.slot_origin(53),
            Some((8 + 8 * SLOT_SIZE, 85 + 2 * SLOT_SIZE, SlotKind::Normal)),
            "last slot of the third inventory row",
        );
        assert_eq!(
            screen.slot_origin(54),
            Some((8, 85 + PLAYER_HOTBAR_OFFSET, SlotKind::Normal)),
            "first hotbar slot keeps the vanilla four-pixel gap",
        );
        assert_eq!(
            screen.slot_origin(62),
            Some((
                8 + 8 * SLOT_SIZE,
                85 + PLAYER_HOTBAR_OFFSET,
                SlotKind::Normal
            )),
            "last hotbar slot",
        );
    }

    #[test]
    fn chest_skin_geometry_computes_exact_two_blit_crops() {
        let skin = ChestSkinGeometry::from_sheet(3, 352, 444).expect("integral 2x sheet");
        assert_eq!(skin.top_height(), 17 + 3 * SLOT_SIZE as u32);
        assert_eq!(skin.footer_height(), 96);

        let panel = UiRect::new(10, 20, 176, 168);
        let [top, footer] = skin.blits(panel);
        assert_eq!(top.dest, UiRect::new(10, 20, 176, 71));
        assert_eq!(
            (top.source_x, top.source_y, top.source_w, top.source_h),
            (0, 0, 352, 142)
        );
        assert_eq!(footer.dest, UiRect::new(10, 91, 176, 96));
        assert_eq!(
            (
                footer.source_x,
                footer.source_y,
                footer.source_w,
                footer.source_h
            ),
            (0, 252, 352, 192),
        );

        assert!(ChestSkinGeometry::from_sheet(3, 180, 222).is_none());
        assert!(ChestSkinGeometry::from_sheet(3, 352, 222).is_none());
    }

    #[test]
    fn chest_schema_uses_shared_chest_geometry() {
        let schema = menu_schema(2).expect("generic 9x3 schema");
        assert_eq!(schema.panel_w, ChestSkinGeometry::SHEET_WIDTH as f32);
        assert_eq!(schema.panel_h, 168.0);
        assert_eq!(schema.inventory_origin_y, 85);
    }

    fn test_state(menu_id: u32) -> ContainerScreenState {
        ContainerScreenState::new(menu_schema(menu_id).unwrap(), 1, 0, "test")
    }

    fn items(item_id: u32, count: u8) -> UiItemStack {
        UiItemStack::new(item_id, count).with_name(format!("item {item_id}"))
    }

    #[test]
    fn left_drag_emits_start_add_end_and_splits_evenly() {
        let mut screen = test_state(0);
        screen.set_cursor(Some(items(6, 9)));
        let start = screen.drag_start(-999, 0).expect("start");
        assert_eq!((start.slot, start.button, start.mode), (-999, 0, 5));
        let add_a = screen.drag_over(20, 0).expect("add a");
        assert_eq!((add_a.slot, add_a.button, add_a.mode), (20, 1, 5));
        let add_b = screen.drag_over(21, 0).expect("add b");
        assert_eq!((add_b.slot, add_b.button, add_b.mode), (21, 1, 5));
        // Re-hovering a collected slot must not emit a duplicate ADD_SLOT.
        assert!(screen.drag_over(20, 0).is_none());
        let end = screen.drag_end().expect("end");
        assert_eq!((end.slot, end.button, end.mode), (-999, 2, 5));
        // floor(9 / 2) = 4 per slot; the indivisible remainder stays.
        assert_eq!(screen.slot(20).unwrap().count, 4);
        assert_eq!(screen.slot(21).unwrap().count, 4);
        assert_eq!(screen.cursor.as_ref().unwrap().count, 1);
    }

    #[test]
    fn right_drag_places_one_item_per_slot() {
        let mut screen = test_state(0);
        screen.set_cursor(Some(items(7, 5)));
        let start = screen.drag_start(10, 1).expect("start");
        assert_eq!((start.slot, start.button), (-999, 4));
        // The press slot is collected once the pointer is observed over it.
        let add = screen.drag_over(10, 1).expect("add press slot");
        assert_eq!((add.slot, add.button), (10, 5));
        screen.drag_over(11, 1).expect("add second");
        screen.drag_over(12, 1).expect("add third");
        let end = screen.drag_end().expect("end");
        assert_eq!((end.slot, end.button), (-999, 6));
        assert_eq!(screen.slot(10).unwrap().count, 1);
        assert_eq!(screen.slot(11).unwrap().count, 1);
        assert_eq!(screen.slot(12).unwrap().count, 1);
        assert_eq!(screen.cursor.as_ref().unwrap().count, 2);
    }

    #[test]
    fn middle_drag_clones_full_stack_without_consuming_cursor() {
        let mut screen = test_state(0);
        screen.set_cursor(Some(items(8, 3)));
        let start = screen.drag_start(-999, 2).expect("start");
        assert_eq!((start.slot, start.button), (-999, 8));
        screen.drag_over(20, 2).expect("add a");
        screen.drag_over(21, 2).expect("add b");
        let add = screen.drag_over(22, 2).expect("add c");
        assert_eq!(add.button, 9);
        let end = screen.drag_end().expect("end");
        assert_eq!(end.button, 10);
        for slot in [20, 21, 22] {
            assert_eq!(screen.slot(slot).unwrap().count, 64);
            assert_eq!(screen.slot(slot).unwrap().item_id, 8);
        }
        // Creative clone never consumes the carried stack.
        assert_eq!(screen.cursor.as_ref().unwrap().count, 3);
    }

    #[test]
    fn single_slot_drag_falls_back_to_normal_click() {
        let mut screen = test_state(0);
        screen.set_cursor(Some(items(9, 6)));
        screen.drag_start(20, 0).expect("start");
        screen.drag_over(20, 0).expect("collect press slot");
        let end = screen.drag_end().expect("single-slot end");
        // Vanilla converts a one-slot quick-craft into a normal pickup.
        assert_eq!(end.mode, 0);
        assert_eq!(end.slot, 20);
        assert!(screen.cursor.is_none());
        assert_eq!(screen.slot(20).unwrap().count, 6);
    }

    #[test]
    fn drag_without_collected_slots_falls_back_to_click_outside() {
        let mut screen = test_state(0);
        screen.set_cursor(Some(items(10, 4)));
        screen.drag_start(-999, 0).expect("start outside");
        // Release without ever hovering an admittable slot: normal click at
        // the press point (-999) drops the carried stack like vanilla.
        let end = screen.drag_end().expect("fallback");
        assert_eq!(end.mode, 0);
        assert_eq!(end.slot, -999);
        assert!(screen.cursor.is_none());
    }

    #[test]
    fn drag_never_targets_output_or_incompatible_slots() {
        let mut screen = test_state(14); // furnace: 0 input, 1 fuel, 2 output
        screen.set_slot(0, items(11, 2));
        screen.set_cursor(Some(items(12, 8)));
        screen.drag_start(-999, 0).expect("start");
        // Output slot rejects placement; a different item also rejects.
        assert!(screen.drag_over(2, 0).is_none());
        assert!(screen.drag_over(0, 0).is_none());
        screen.drag_over(1, 0).expect("fuel slot admits");
        let end = screen.drag_end().expect("end");
        assert_eq!(end.mode, 0, "one collected slot falls back to pickup");
        assert_eq!(end.slot, 1);
        assert!(screen.slot(2).unwrap().is_empty());
        assert_eq!(screen.slot(0).unwrap().item_id, 11);
    }

    #[test]
    fn drag_requires_carried_items() {
        let mut screen = test_state(0);
        assert!(screen.drag_start(20, 0).is_none());
        assert!(screen.drag_start(-999, 0).is_none());
        assert!(screen.drag_end().is_none());
    }

    #[test]
    fn drag_ghost_preview_marks_collected_slots() {
        let mut screen = test_state(0);
        screen.set_cursor(Some(items(13, 8)));
        screen.drag_start(-999, 0).expect("start");
        screen.drag_over(20, 0).expect("add");
        screen.drag_over(21, 0).expect("add");
        let mut draw = crate::draw::UiDrawList::new();
        screen.render(800.0, 600.0, &mut draw);
        // Two collected slots of 8 -> ghost count 4 painted in accent.
        let accent_counts = draw
            .commands
            .iter()
            .filter(|cmd| {
                matches!(cmd, UiCommand::Text { color, text, .. }
                    if *color == palette::ACCENT && text == "4")
            })
            .count();
        assert_eq!(accent_counts, 2, "each collected slot previews count 4");
    }

    #[test]
    fn double_click_gathers_matching_stacks_to_cursor() {
        let mut screen = test_state(0);
        screen.set_cursor(Some(items(14, 2)));
        screen.set_slot(10, items(14, 30));
        screen.set_slot(20, items(14, 20));
        screen.set_slot(30, items(15, 64));
        let action = screen.click_mode(30, 0, 6).expect("collect");
        assert_eq!(action.mode, 6);
        assert_eq!(screen.cursor.as_ref().unwrap().count, 52);
        assert!(screen.slot(10).unwrap().is_empty());
        assert!(screen.slot(20).unwrap().is_empty());
        assert_eq!(screen.slot(30).unwrap().count, 64);
    }

    #[test]
    fn double_click_skips_output_slots() {
        let mut screen = test_state(14);
        screen.set_slot(2, items(16, 1)); // furnace output
        screen.set_slot(3, items(16, 5)); // player inventory
        screen.set_cursor(Some(items(16, 1)));
        screen.click_mode(3, 0, 6).expect("collect");
        assert_eq!(screen.cursor.as_ref().unwrap().count, 6);
        assert_eq!(screen.slot(2).unwrap().count, 1, "output untouched");
        assert!(screen.slot(3).unwrap().is_empty());
    }

    #[test]
    fn shift_click_hotbar_offhand_and_drop_shortcuts() {
        let mut screen = test_state(0);
        screen.set_slot(0, items(17, 10)); // container slot
        let quick = screen.click_with_shift(0, 0, true).expect("shift click");
        assert_eq!(quick.mode, 1);
        assert!(screen.slot(0).unwrap().is_empty());
        assert_eq!(screen.slot(9).unwrap().count, 10);

        screen.set_cursor(None);
        let swap = screen.click_mode(9, 3, 2).expect("hotbar swap");
        assert_eq!(swap.mode, 2);
        assert_eq!(swap.button, 3);
        assert!(screen.slot(9).unwrap().is_empty());
        let hotbar = (9 + 27 + 3) as i16;
        assert_eq!(screen.slot(hotbar as usize).unwrap().item_id, 17);
        assert_eq!(screen.slot(hotbar as usize).unwrap().count, 10);

        let drop_one = screen.click_mode(hotbar, 0, 4).expect("drop one");
        assert_eq!(drop_one.mode, 4);
        assert_eq!(screen.slot(hotbar as usize).unwrap().count, 9);
        let drop_all = screen.click_mode(hotbar, 1, 4).expect("drop stack");
        assert_eq!(drop_all.button, 1);
        assert!(screen.slot(hotbar as usize).unwrap().is_empty());
    }

    #[test]
    fn anvil_rename_accepts_letter_e_without_closing() {
        let mut screen = test_state(8);
        screen.rename_focused = true;
        // Typing 'e'/'E' in the rename field produces rename actions; the
        // screen state offers no close path, so the anvil stays open.
        let lower = screen.handle_rename_char('e').expect("rename 'e'");
        assert_eq!(lower, WorkstationAction::RenameItem("e".to_string()));
        let upper = screen.handle_rename_char('E').expect("rename 'E'");
        assert_eq!(upper, WorkstationAction::RenameItem("eE".to_string()));
        assert!(screen.rename_focused);
        assert_eq!(screen.rename_text, "eE");
        // Unfocused screens ignore rename input entirely.
        screen.rename_focused = false;
        assert!(screen.handle_rename_char('e').is_none());
    }

    #[test]
    fn creative_clone_pickup_copies_slot_without_removing() {
        let mut screen = test_state(0);
        screen.set_slot(10, items(18, 12));
        let action = screen.click(10, 2).expect("middle click clone");
        assert_eq!(action.mode, 0);
        assert_eq!(action.button, 2);
        assert_eq!(screen.cursor.as_ref().unwrap().count, 12);
        assert_eq!(screen.slot(10).unwrap().count, 12);
    }
}
