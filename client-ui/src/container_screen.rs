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

use crate::conflict_widget::ConflictWidget;
use crate::draw::{palette, text_height, text_width, UiColor, UiCommand, UiDrawList, UiRect};
use crate::vanilla_widgets::{draw_vanilla_arrow, draw_vanilla_ghost_item};

pub const VANILLA_MENU_COUNT: usize = 25;
pub const SLOT_SIZE: i32 = 18;
const CONTENT_PAD: i32 = 4;
const EMPTY_SLOT_INDEX: i16 = -999;

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
    let mut out = [SlotDef::EMPTY; N];
    let mut i = 0usize;
    while i < N {
        let row = i / COLS;
        let col = i % COLS;
        out[i] = SlotDef::new(
            i as i16,
            x0 + col as i32 * SLOT_SIZE,
            y0 + row as i32 * SLOT_SIZE,
            SlotKind::Normal,
        );
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

const NO_BUTTONS: &[MenuButtonDef] = &[];
const NO_BARS: &[PropertyBarDef] = &[];
const ENCHANT_BUTTONS: &[MenuButtonDef] = &[
    MenuButtonDef::new(0, 52, 74, 30, 10, "I"),
    MenuButtonDef::new(1, 86, 74, 30, 10, "II"),
    MenuButtonDef::new(2, 120, 74, 30, 10, "III"),
];
const LECTERN_BUTTONS: &[MenuButtonDef] = &[
    MenuButtonDef::new(1, 8, 28, 22, 12, "<"),
    MenuButtonDef::new(2, 34, 28, 22, 12, ">"),
    MenuButtonDef::new(3, 8, 44, 48, 12, "take"),
];
const LOOM_BUTTONS: &[MenuButtonDef] = &[MenuButtonDef::dynamic(8, 68, 120, 12, "pattern")];
const STONECUTTER_BUTTONS: &[MenuButtonDef] = &[MenuButtonDef::dynamic(8, 58, 160, 12, "recipe")];
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
const ENCHANT_BARS: &[PropertyBarDef] = &[
    PropertyBarDef {
        property: 0,
        secondary_property: 0,
        kind: PropertyBarKind::Costs,
        x: 60,
        y: 58,
        w: 8,
        h: 8,
        label: "cost",
    },
    PropertyBarDef {
        property: 1,
        secondary_property: 0,
        kind: PropertyBarKind::Costs,
        x: 102,
        y: 58,
        w: 8,
        h: 8,
        label: "cost",
    },
    PropertyBarDef {
        property: 2,
        secondary_property: 0,
        kind: PropertyBarKind::Costs,
        x: 60,
        y: 66,
        w: 8,
        h: 8,
        label: "cost",
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
        panel_w: 176.0,
        panel_h: (114 + rows * 18) as f32,
        container_slots: slots,
        has_player_inventory: true,
        inventory_start: container as i16,
        slot_count: (container + 36) as i16,
        inventory_origin_x: 8,
        inventory_origin_y: (31 + rows * 18) as i16,
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
        s.property_bars = ENCHANT_BARS;
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

/// Drag-distribute state for click mode `5`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct DragState {
    /// `0` = even split, `1` = one item per slot.
    pub button: u8,
    pub slots: Vec<i16>,
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
        if let Some(slot) = self.slots.get_mut(index) {
            *slot = stack;
        }
    }

    pub fn set_cursor(&mut self, cursor: Option<UiItemStack>) {
        self.cursor = cursor.filter(|s| !s.is_empty());
    }

    pub fn set_property(&mut self, property: i16, value: i16) {
        self.properties.insert(property, value);
    }

    pub fn property(&self, property: i16) -> i16 {
        self.properties.get(&property).copied().unwrap_or(0)
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
        if self
            .selected_trade_index
            .is_some_and(|i| i >= self.merchant_offers.len())
        {
            self.selected_trade_index = None;
        }
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

    /// Route a workstation click in the coordinate space produced by `layout`.
    pub fn click_at(
        &mut self,
        x: i32,
        y: i32,
        viewport_w: f32,
        viewport_h: f32,
    ) -> Option<WorkstationAction> {
        if self.schema.menu_id == 8 {
            let r = self.rename_input_rect(viewport_w, viewport_h)?;
            if r.contains(x, y) {
                self.rename_focused = true;
                return None;
            }
            self.rename_focused = false;
        }
        if self.schema.menu_id == 9 {
            let panel = self.panel_rect(viewport_w, viewport_h);
            let lx = x - panel.x - CONTENT_PAD;
            let ly = y - panel.y - CONTENT_PAD;
            let level = self.property(0).clamp(0, 4);
            let choices: &[(i32, i32, i32, i32, i32)] = &[
                (1, 24, 54, 44, 14),
                (3, 72, 54, 44, 14),
                (11, 24, 72, 44, 14),
                (8, 72, 72, 44, 14),
                (5, 24, 90, 44, 14),
                (10, 24, 108, 44, 14),
                (0, 72, 108, 44, 14),
            ];
            for &(id, bx, by, bw, bh) in choices {
                let tier = if id == 1 || id == 3 {
                    1
                } else if id == 11 || id == 8 {
                    2
                } else if id == 5 {
                    3
                } else {
                    4
                };
                if level >= tier && UiRect::new(bx, by, bw as u32, bh as u32).contains(lx, ly) {
                    if id == 0 {
                        self.beacon_secondary = self.beacon_primary;
                    } else if id == 10 {
                        self.beacon_secondary = Some(10);
                    } else {
                        self.beacon_primary = Some(id);
                    }
                    return None;
                }
            }
            if UiRect::new(158, 72, 30, 14).contains(lx, ly) {
                return Some(WorkstationAction::SetBeacon {
                    primary: self.beacon_primary,
                    secondary: self.beacon_secondary,
                });
            }
            if UiRect::new(158, 90, 30, 14).contains(lx, ly) {
                self.beacon_primary = None;
                self.beacon_secondary = None;
                return None;
            }
        }
        if self.schema.menu_id == 19 {
            let panel = self.panel_rect(viewport_w, viewport_h);
            let local_x = x - panel.x - CONTENT_PAD;
            let local_y = y - panel.y - CONTENT_PAD;
            if (8..104).contains(&local_x) && (18..130).contains(&local_y) {
                let row = ((local_y - 18) / 16) as usize;
                return self.select_merchant_trade(self.merchant_scroll_offset + row);
            }
        }
        None
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
        let col = inv.rem_euclid(9);
        let (row, hotbar_offset) = if inv < 27 { (inv / 9, 0) } else { (0, 58) };
        let x = i32::from(self.schema.inventory_origin_x) + i32::from(col) * SLOT_SIZE;
        let y =
            i32::from(self.schema.inventory_origin_y) + i32::from(row) * SLOT_SIZE + hotbar_offset;
        Some((x, y, SlotKind::Normal))
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
        let mut slots = Vec::with_capacity(self.slots.len());
        for index in 0..self.slots.len() as i16 {
            if let Some(rect) = self.slot_rect(index, panel) {
                slots.push(UiSlotRect {
                    index,
                    rect,
                    kind: self.slot_kind(index).unwrap_or(SlotKind::Normal),
                });
            }
        }
        let mut buttons: Vec<UiButtonRect> = self
            .schema
            .buttons
            .iter()
            .map(|b| UiButtonRect {
                id: b.id,
                rect: UiRect::new(
                    panel.x + CONTENT_PAD + b.x,
                    panel.y + CONTENT_PAD + b.y,
                    b.w as u32,
                    b.h as u32,
                ),
                label: b.label,
            })
            .collect();
        if self.schema.menu_id == 8 {
            buttons.push(UiButtonRect {
                id: -8,
                rect: self.rename_input_rect(viewport_w, viewport_h).unwrap(),
                label: "rename",
            });
        } else if self.schema.menu_id == 9 {
            for (id, x, y, label) in [
                (101, 24, 54, "Speed"),
                (103, 72, 54, "Haste"),
                (111, 24, 72, "Resist"),
                (108, 72, 72, "Jump"),
                (105, 24, 90, "Strength"),
                (110, 24, 108, "Regen"),
                (100, 72, 108, "Secondary"),
                (-9, 158, 90, "Cancel"),
            ] {
                buttons.push(UiButtonRect {
                    id,
                    rect: UiRect::new(
                        panel.x + CONTENT_PAD + x,
                        panel.y + CONTENT_PAD + y,
                        if id == -9 { 30 } else { 44 },
                        14,
                    ),
                    label,
                });
            }
        } else if self.schema.menu_id == 19 {
            for row in 0..self.merchant_offers.len().min(7) {
                buttons.push(UiButtonRect {
                    id: 1000 + row as i32,
                    rect: UiRect::new(
                        panel.x + CONTENT_PAD + 8,
                        panel.y + CONTENT_PAD + 18 + row as i32 * 16,
                        96,
                        16,
                    ),
                    label: "trade",
                });
            }
        }
        let bars = self
            .schema
            .property_bars
            .iter()
            .map(|b| UiBarRect {
                property: b.property,
                kind: b.kind,
                rect: UiRect::new(
                    panel.x + CONTENT_PAD + b.x,
                    panel.y + CONTENT_PAD + b.y,
                    b.w as u32,
                    b.h as u32,
                ),
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

    fn scale_factor(&self, viewport_w: f32, viewport_h: f32, gui_scale: f32) -> f32 {
        let scale = if gui_scale.is_finite() {
            gui_scale.clamp(1.0, 4.0)
        } else {
            1.0
        };
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
        let scale = if gui_scale.is_finite() {
            gui_scale.clamp(1.0, 4.0)
        } else {
            1.0
        };
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

    fn tooltip_size(&self, item: &UiItemStack) -> (i32, i32) {
        let lines = self.tooltip_lines(item);
        let mut w = 0i32;
        for (text, _) in &lines {
            w = w.max(text_width(text, 1) as i32);
        }
        let h = lines.len() as i32 * text_height("", 1) as i32 + 6;
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
        let (tw, th) = self.tooltip_size(&item);
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
        if !viewport_w.is_finite()
            || !viewport_h.is_finite()
            || viewport_w <= 0.0
            || viewport_h <= 0.0
        {
            return;
        }
        let vw = viewport_w as i32;
        let vh = viewport_h as i32;
        let panel = self.panel_rect(viewport_w, viewport_h);
        out.fill_rect(
            UiRect::new(0, 0, vw as u32, vh as u32),
            palette::BACKGROUND_BOTTOM,
        );
        out.fill_rect(panel, palette::PANEL);
        out.border_rect(panel, palette::PANEL_BORDER, 1);
        if panel.w >= 20 && panel.h >= 12 {
            let title = format!("{}  #{}", self.title, self.schema.menu_id);
            let clipped = clip_text(&title, (panel.w as i32 - 12).max(0));
            out.text(panel.x + 6, panel.y + 4, 1, palette::TEXT, clipped);
        }
        if viewport_w < self.schema.panel_w || viewport_h < self.schema.panel_h {
            return;
        }
        let layout = self.layout(viewport_w, viewport_h);
        for slot in &layout.slots {
            let item = self
                .slots
                .get(slot.index as usize)
                .cloned()
                .unwrap_or_else(UiItemStack::empty);
            let hovered = self.hovered_slot == Some(slot.index as usize);
            out.fill_rect(slot.rect, palette::SLOT);
            out.border_rect(
                slot.rect,
                if hovered {
                    palette::BUTTON_BORDER_HOVER
                } else {
                    palette::PANEL_BORDER
                },
                if hovered { 2 } else { 1 },
            );
            if item.is_empty() {
                if let Some(ghost) = ghost_item(self.schema.menu_id, slot.index) {
                    draw_vanilla_ghost_item(out, slot.rect, ghost);
                }
                if self.schema.menu_id == 7 && self.property(slot.index) != 0 && slot.index < 9 {
                    out.text_str(slot.rect.x + 5, slot.rect.y + 4, 1, palette::DANGER, "X");
                }
                continue;
            }
            let inset = UiRect::new(slot.rect.x + 2, slot.rect.y + 2, 16, 16);
            if item.namespaced_id.is_empty() {
                let icon = palette::ACCENT;
                out.fill_rect(UiRect::new(slot.rect.x + 3, slot.rect.y + 3, 12, 12), icon);
            } else {
                out.item_icon_str(inset, &item.namespaced_id);
            }
            if item.count > 1 {
                let label = item.count.to_string();
                out.text(
                    slot.rect.right() - 2 - text_width(&label, 1) as i32,
                    slot.rect.bottom() - 9,
                    1,
                    palette::TEXT,
                    label,
                );
            }
            if item.max_durability > 0 && item.durability >= 0 {
                let frac = (item.durability as f32 / item.max_durability as f32).clamp(0.0, 1.0);
                out.progress_bar(
                    UiRect::new(slot.rect.x + 1, slot.rect.bottom() - 4, 16, 3),
                    frac,
                    palette::HEALTH,
                    palette::SLOT,
                    palette::PANEL_BORDER,
                );
            }
        }

        for bar in &layout.bars {
            out.progress_bar(
                bar.rect,
                bar.fraction,
                palette::ACCENT,
                palette::BUTTON_DISABLED,
                palette::PANEL_BORDER,
            );
        }
        for button in &layout.buttons {
            out.fill_rect(button.rect, palette::BUTTON);
            out.border_rect(button.rect, palette::BUTTON_BORDER, 1);
            out.text(
                button.rect.x + 3,
                button.rect.y + 2,
                1,
                palette::TEXT,
                button.label,
            );
        }
        self.draw_station_chrome(panel, out);
        self.draw_workstation_controls(panel, out);
        // Polymorph strip beside the station's output slot — only while
        // two or more recipes conflict; a quiet grid emits nothing.
        if self.conflict.is_active() {
            if let Some(anchor) = self.output_slot_rect(viewport_w, viewport_h) {
                self.conflict
                    .render(anchor, 1, vw as u32, vh as u32, out);
            }
        }
        if let Some(cursor) = self.cursor.clone().filter(|s| !s.is_empty()) {
            let rect = UiRect::new(vw - 24, vh - 24, 20, 20);
            out.fill_rect(rect, palette::SLOT);
            out.border_rect(rect, palette::BUTTON_BORDER_HOVER, 1);
            if cursor.namespaced_id.is_empty() {
                out.fill_rect(
                    UiRect::new(rect.x + 3, rect.y + 3, 14, 14),
                    palette::ACCENT,
                );
            } else {
                out.item_icon_str(
                    UiRect::new(rect.x + 2, rect.y + 2, 16, 16),
                    &cursor.namespaced_id,
                );
            }
            if cursor.count > 1 {
                out.text(
                    rect.right() - 2 - text_width(&cursor.count.to_string(), 1) as i32,
                    rect.bottom() - 9,
                    1,
                    palette::TEXT,
                    cursor.count.to_string(),
                );
            }
        }
        if let (Some(item), Some(rect)) = (self.tooltip_item(), layout.tooltip) {
            out.fill_rect(rect, palette::PANEL);
            out.border_rect(rect, palette::PANEL_BORDER, 1);
            let lines = self.tooltip_lines(&item);
            let mut y = rect.y + 3;
            for (text, color) in lines {
                let clipped = clip_text(&text, (rect.w as i32 - 6).max(1));
                out.text(rect.x + 3, y, 1, color, clipped);
                y += text_height("", 1) as i32;
            }
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
        if !viewport_w.is_finite()
            || !viewport_h.is_finite()
            || viewport_w <= 0.0
            || viewport_h <= 0.0
        {
            return;
        }
        let mut native = UiDrawList::new();
        self.render(viewport_w, viewport_h, &mut native);
        let scale = if gui_scale.is_finite() {
            gui_scale.clamp(1.0, 4.0)
        } else {
            1.0
        };
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

impl ContainerScreenState {
    fn draw_workstation_controls(&self, panel: UiRect, out: &mut UiDrawList) {
        let px = panel.x + CONTENT_PAD;
        let py = panel.y + CONTENT_PAD;
        match self.schema.menu_id {
            8 => {
                let field = UiRect::new(px + 29, py + 27, 96, 12);
                out.fill_rect(field, palette::TOOLTIP_BG);
                out.border_rect(
                    field,
                    if self.rename_focused {
                        palette::TEXT_FOCUSED
                    } else {
                        palette::TEXT_DIM
                    },
                    1,
                );
                let mut text = clip_text(&self.rename_text, field.w.saturating_sub(6) as i32);
                if self.rename_focused && text_width(&text, 1) + 8 <= field.w {
                    text.push('|');
                }
                out.text(field.x + 3, field.y + 2, 1, palette::TEXT, text);
                let cost = self.property(0);
                let footer = if cost > 0 {
                    format!("Enchantment Cost: {cost}")
                } else {
                    "Too Expensive!".to_owned()
                };
                out.text(
                    px + 8,
                    py + 76,
                    1,
                    if cost > 0 {
                        palette::ACCENT
                    } else {
                        palette::DANGER
                    },
                    clip_text(&footer, panel.w as i32 - 20),
                );
            }
            9 => {
                let controls = [
                    (1, 24, 54, "Speed", 1),
                    (3, 72, 54, "Haste", 1),
                    (11, 24, 72, "Resist", 2),
                    (8, 72, 72, "Jump", 2),
                    (5, 24, 90, "Strength", 3),
                    (10, 24, 108, "Regen", 4),
                    (0, 72, 108, "Secondary", 4),
                ];
                let level = self.property(0).clamp(0, 4);
                for (id, x, y, label, tier) in controls {
                    let rect = UiRect::new(px + x, py + y, 44, 14);
                    let enabled = level >= tier;
                    let selected = if id == 0 {
                        self.beacon_secondary.is_some()
                    } else {
                        self.beacon_primary == Some(id)
                    };
                    out.fill_rect(
                        rect,
                        if enabled {
                            palette::BUTTON
                        } else {
                            palette::BUTTON_DISABLED
                        },
                    );
                    out.border_rect(
                        rect,
                        if selected {
                            palette::BUTTON_BORDER_HOVER
                        } else {
                            palette::BUTTON_BORDER
                        },
                        if selected { 2 } else { 1 },
                    );
                    out.text(
                        rect.x + 2,
                        rect.y + 3,
                        1,
                        if enabled {
                            palette::TEXT_DARK
                        } else {
                            palette::BUTTON_DISABLED_TEXT
                        },
                        label,
                    );
                }
                for (y, label, color) in [
                    (72, "Confirm", palette::ACCENT),
                    (90, "Cancel", palette::DANGER),
                ] {
                    let rect = UiRect::new(px + 158, py + y, 30, 14);
                    out.fill_rect(rect, palette::BUTTON);
                    out.border_rect(rect, palette::BUTTON_BORDER, 1);
                    out.text(rect.x + 2, rect.y + 3, 1, color, label);
                }
            }
            19 => {
                let first = self.merchant_scroll_offset.min(self.merchant_offers.len());
                for row in 0..7 {
                    let index = first + row;
                    let Some(offer) = self.merchant_offers.get(index) else {
                        continue;
                    };
                    let rect = UiRect::new(px + 8, py + 18 + row as i32 * 16, 96, 16);
                    if self.selected_trade_index == Some(index) {
                        out.fill_rect(rect, palette::SLOT_HOVER);
                    }
                    out.border_rect(
                        rect,
                        if self.selected_trade_index == Some(index) {
                            palette::BUTTON_BORDER_HOVER
                        } else {
                            palette::SEPARATOR
                        },
                        1,
                    );
                    out.item_icon_str(UiRect::new(rect.x + 2, rect.y + 2, 11, 11), &offer.input1);
                    if let Some(input2) = &offer.input2 {
                        out.item_icon_str(UiRect::new(rect.x + 17, rect.y + 2, 11, 11), input2);
                    }
                    out.text_str(rect.x + 30, rect.y + 3, 1, palette::TEXT_DARK, "→");
                    out.item_icon_str(UiRect::new(rect.x + 41, rect.y + 2, 11, 11), &offer.output);
                    let cost = if offer.effective_cost < offer.original_cost {
                        format!("{}>{}", offer.original_cost, offer.effective_cost)
                    } else {
                        offer.effective_cost.to_string()
                    };
                    let cost_color = if offer.effective_cost < offer.original_cost {
                        palette::ACCENT
                    } else {
                        palette::TEXT_DARK
                    };
                    out.text(rect.x + 55, rect.y + 8, 1, cost_color, clip_text(&cost, 40));
                    if offer.effective_cost < offer.original_cost {
                        let old = offer.original_cost.to_string();
                        let line_w = text_width(&old, 1).min(20);
                        out.fill_rect(
                            UiRect::new(rect.x + 55, rect.y + 11, line_w, 1),
                            palette::DANGER,
                        );
                    }
                    if offer.out_of_stock {
                        out.text_str(rect.right() - 8, rect.y + 3, 1, palette::DANGER, "X");
                    }
                }
                out.text(
                    px + 108,
                    py + 14,
                    1,
                    palette::TEXT_DARK,
                    format!("Level {}", self.property(1).clamp(1, 5)),
                );
                out.progress_bar(
                    UiRect::new(px + 108, py + 26, 96, 5),
                    f32::from(self.property(2)).clamp(0.0, 1.0),
                    palette::XP,
                    palette::BUTTON_DISABLED,
                    palette::PANEL_BORDER,
                );
            }
            _ => {}
        }
    }

    fn draw_station_chrome(&self, panel: UiRect, out: &mut UiDrawList) {
        let px = panel.x + CONTENT_PAD;
        let py = panel.y + CONTENT_PAD;
        match self.schema.menu_id {
            0..=5 => {
                let rows = (self.schema.menu_id + 1) as i32;
                let divider_y = py + 29 + rows * SLOT_SIZE;
                out.fill_rect(
                    UiRect::new(px + 4, divider_y, panel.w.saturating_sub(16), 1),
                    palette::SEPARATOR,
                );
            }
            6 => {
                draw_vanilla_arrow(out, UiRect::new(px + 94, py + 26, 22, 18), true);
                out.text_str(px + 92, py + 46, 1, palette::TEXT_DIM, "DISPENSER");
            }
            7 => {
                out.text_str(px + 8, py + 5, 1, palette::TEXT_DARK, "CRAFTER");
                draw_vanilla_arrow(out, UiRect::new(px + 104, py + 31, 20, 18), self.property(0) == 0);
                out.text_str(
                    px + 120,
                    py + 50,
                    1,
                    if self.property(0) == 0 { palette::ACCENT } else { palette::DANGER },
                    if self.property(0) == 0 { "ON" } else { "OFF" },
                );
            }
            8 => {
                draw_vanilla_arrow(out, UiRect::new(px + 104, py + 43, 20, 18), true);
            }
            9 => {
                out.text_str(px + 58, py + 103, 1, palette::TEXT_DIM, "Payment");
            }
            10 | 14 | 22 => {
                draw_vanilla_arrow(out, UiRect::new(px + 80, py + 33, 20, 18), true);
                out.text_str(px + 77, py + 55, 1, palette::TEXT_DIM, "COOK");
            }
            11 => {
                out.text_str(px + 34, py + 73, 1, palette::TEXT_DIM, "BREW");
                draw_vanilla_arrow(out, UiRect::new(px + 77, py + 34, 16, 14), true);
            }
            12 => {
                draw_vanilla_arrow(out, UiRect::new(px + 91, py + 26, 22, 18), true);
                out.text_str(px + 91, py + 46, 1, palette::TEXT_DIM, "CRAFT");
            }
            13 => {
                out.text_str(px + 53, py + 15, 1, palette::TEXT_DARK, "Select Enchantment");
                for i in 0..3 {
                    let y = py + 71 + i * 12;
                    out.text_str(px + 57, y, 1, palette::WARNING, "ᚠᚢᚦ");
                    let cost = self.property(i as i16);
                    out.text(
                        px + 119,
                        y,
                        1,
                        if cost > 0 { palette::ACCENT } else { palette::DANGER },
                        cost.to_string(),
                    );
                }
            }
            15 => draw_vanilla_arrow(out, UiRect::new(px + 77, py + 26, 34, 24), false),
            16 => out.text_str(px + 8, py + 5, 1, palette::TEXT_DARK, "HOPPER"),
            17 => {
                out.text_str(px + 61, py + 13, 1, palette::TEXT_DARK, "Lectern");
                out.text_str(px + 63, py + 51, 1, palette::TEXT_DIM, "Book Page");
            }
            18 => {
                out.text_str(px + 74, py + 15, 1, palette::TEXT_DARK, "Banner Pattern");
                out.text_str(px + 75, py + 43, 1, palette::ACCENT, "PREVIEW");
            }
            19 => {
                out.text_str(px + 108, py + 4, 1, palette::TEXT_DARK, "Villager Trades");
            }
            20 => out.text_str(px + 100, py + 5, 1, palette::TEXT_DARK, "SHULKER BOX"),
            21 => {
                out.text(
                    px + 8,
                    py + 30,
                    1,
                    palette::TEXT_DIM,
                    clip_text("TEMPLATE  BASE  ADDITION", (panel.w as i32 - 20).max(1)),
                );
                draw_vanilla_arrow(out, UiRect::new(px + 61, py + 41, 21, 18), true);
            }
            23 => {
                out.text_str(px + 93, py + 20, 1, palette::TEXT_DARK, "MAP PREVIEW");
                out.border_rect(
                    UiRect::new(px + 92, py + 34, 50, 38),
                    palette::PANEL_BORDER,
                    1,
                );
                out.text_str(px + 104, py + 48, 1, palette::ACCENT, "MAP");
            }
            24 => out.text_str(px + 8, py + 44, 1, palette::TEXT_DARK, "STONECUTTER"),
            _ => {}
        }
    }
}

fn ghost_item(menu_id: u32, slot: i16) -> Option<&'static str> {
    match (menu_id, slot) {
        (8, 0) => Some("minecraft:iron_sword"),
        (9, 0) => Some("minecraft:iron_ingot"),
        (10 | 14 | 22, 0) => Some("minecraft:iron_ore"),
        (10 | 14 | 22, 1) => Some("minecraft:coal"),
        (11, 1..=3) => Some("minecraft:glass_bottle"),
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

    fn normal_click(&mut self, slot_index: i16, button: i8) -> Option<ClickWindowAction> {
        if slot_index == EMPTY_SLOT_INDEX {
            let changed = BTreeSet::new();
            if let Some(mut carried) = self.cursor.clone() {
                if button == 0 {
                    self.cursor = None;
                } else if button == 1 {
                    carried.count = carried.count.saturating_sub(1);
                    self.cursor = if carried.count == 0 {
                        None
                    } else {
                        Some(carried)
                    };
                } else {
                    return None;
                }
                return Some(self.finish_action(slot_index, button, 0, &changed));
            }
            return Some(self.finish_action(slot_index, button, 0, &changed));
        }
        let index = usize::try_from(slot_index).ok()?;
        if index >= self.slots.len() || !self.can_take(slot_index) {
            return None;
        }
        let mut changed = BTreeSet::new();
        let slot_stack = self.slots[index].clone();
        let cursor = self.cursor.clone().filter(|s| !s.is_empty());

        match (slot_stack.is_empty(), cursor) {
            (true, None) => {}
            (true, Some(mut carried)) => {
                if !self.accepts_placement(slot_index, &carried) {
                    return Some(self.finish_action(slot_index, button, 0, &changed));
                }
                let place = u32::from(carried.count).min(u32::from(carried.max_stack));
                let place = place as u8;
                let mut placed = carried.clone();
                placed.count = place;
                carried.count -= place;
                self.slots[index] = placed;
                self.cursor = if carried.count == 0 {
                    None
                } else {
                    Some(carried)
                };
                changed.insert(slot_index);
            }
            (false, None) => {
                let take = if button == 1 {
                    slot_stack.count.div_ceil(2)
                } else {
                    slot_stack.count
                };
                let mut taken = slot_stack.clone();
                taken.count = take;
                let mut left = slot_stack.clone();
                left.count = left.count.saturating_sub(take);
                self.slots[index] = if left.count == 0 {
                    UiItemStack::empty()
                } else {
                    left
                };
                self.cursor = Some(taken);
                changed.insert(slot_index);
            }
            (false, Some(mut carried)) => {
                if slot_stack.same_stackable(&carried) {
                    let capacity =
                        u32::from(slot_stack.max_stack).saturating_sub(u32::from(slot_stack.count));
                    let amount = if button == 1 {
                        1
                    } else {
                        u32::from(carried.count)
                    }
                    .min(capacity);
                    if amount == 0 {
                        return Some(self.finish_action(slot_index, button, 0, &changed));
                    }
                    let amount = amount as u8;
                    let mut slot = slot_stack.clone();
                    slot.count += amount;
                    carried.count -= amount;
                    self.slots[index] = slot;
                    self.cursor = if carried.count == 0 {
                        None
                    } else {
                        Some(carried)
                    };
                    changed.insert(slot_index);
                } else if self.accepts_placement(slot_index, &carried) {
                    let old_slot = self.slots[index].clone();
                    self.slots[index] = carried;
                    self.cursor = Some(old_slot);
                    changed.insert(slot_index);
                }
            }
        }
        Some(self.finish_action(slot_index, button, 0, &changed))
    }

    fn transfer_range(&mut self, source: i16, targets: &[i16]) -> Option<BTreeSet<i16>> {
        let src = usize::try_from(source).ok()?;
        let mut stack = self.slots.get(src)?.clone();
        if stack.is_empty() {
            return None;
        }
        let mut changed = BTreeSet::new();
        for &target in targets {
            if stack.is_empty() {
                break;
            }
            let t = usize::try_from(target).ok()?;
            if t >= self.slots.len() || !self.accepts_placement(target, &stack) {
                continue;
            }
            let current = self.slots[t].clone();
            if current.is_empty() {
                let amount = u32::from(stack.count).min(u32::from(stack.max_stack));
                let mut placed = stack.clone();
                placed.count = amount as u8;
                stack.count -= amount as u8;
                self.slots[t] = placed;
                changed.insert(target);
            } else if current.same_stackable(&stack) {
                let capacity =
                    u32::from(current.max_stack).saturating_sub(u32::from(current.count));
                let amount = u32::from(stack.count).min(capacity);
                if amount == 0 {
                    continue;
                }
                let mut merged = current;
                merged.count += amount as u8;
                stack.count -= amount as u8;
                self.slots[t] = merged;
                changed.insert(target);
            }
        }
        self.slots[src] = if stack.count == 0 {
            UiItemStack::empty()
        } else {
            stack
        };
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

    fn hotbar_swap(&mut self, slot_index: i16, hotbar_index: i8) -> Option<ClickWindowAction> {
        if !(0..=8).contains(&hotbar_index) || !self.schema.has_player_inventory {
            return None;
        }
        let index = usize::try_from(slot_index).ok()?;
        if index >= self.slots.len() || slot_index == EMPTY_SLOT_INDEX {
            return None;
        }
        let hotbar_slot = self.schema.inventory_start + 27 + i16::from(hotbar_index);
        let hotbar = usize::try_from(hotbar_slot).ok()?;
        if hotbar >= self.slots.len() || hotbar == index {
            return None;
        }
        let a = self.slots[index].clone();
        let b = self.slots[hotbar].clone();
        if !a.is_empty() && !self.accepts_placement(hotbar_slot, &a) {
            return None;
        }
        if !b.is_empty() && !self.accepts_placement(slot_index, &b) {
            return None;
        }
        self.slots[index] = b;
        self.slots[hotbar] = a;
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
        let mut left = stack.clone();
        left.count = left.count.saturating_sub(amount);
        self.slots[index] = if left.count == 0 {
            UiItemStack::empty()
        } else {
            left
        };
        let mut changed = BTreeSet::new();
        changed.insert(slot_index);
        Some(self.finish_action(slot_index, button, 4, &changed))
    }

    fn drag_click(&mut self, slot_index: i16, button: i8) -> Option<ClickWindowAction> {
        if slot_index == EMPTY_SLOT_INDEX {
            return self.drag_end();
        }
        let index = usize::try_from(slot_index).ok()?;
        if index >= self.slots.len() {
            return None;
        }
        if self.drag_mode.is_none() {
            let carried = self.cursor.clone()?;
            if carried.is_empty() || !self.accepts_placement(slot_index, &carried) {
                return None;
            }
            self.drag_mode = Some(DragState {
                button: button.max(0) as u8,
                slots: vec![slot_index],
            });
            let mut changed = BTreeSet::new();
            changed.insert(slot_index);
            return Some(self.finish_action(slot_index, button, 5, &changed));
        }
        let probe = self.cursor.clone().unwrap_or_else(UiItemStack::empty);
        let accepts = self.accepts_placement(slot_index, &probe);
        let drag = self.drag_mode.as_mut()?;
        if !drag.slots.contains(&slot_index) && accepts {
            drag.slots.push(slot_index);
            drag.slots.sort_unstable();
        }
        let mut changed = BTreeSet::new();
        changed.insert(slot_index);
        Some(self.finish_action(slot_index, button, 5, &changed))
    }

    pub fn drag_end(&mut self) -> Option<ClickWindowAction> {
        let drag = self.drag_mode.take()?;
        let carried = self.cursor.clone().filter(|s| !s.is_empty())?;
        let slots: Vec<i16> = drag
            .slots
            .iter()
            .copied()
            .filter(|&s| self.accepts_placement(s, &carried))
            .collect();
        if slots.is_empty() {
            return Some(self.finish_action(
                EMPTY_SLOT_INDEX,
                drag.button as i8,
                5,
                &BTreeSet::new(),
            ));
        }
        let mut remaining = u32::from(carried.count);
        let per = if drag.button == 0 {
            remaining / slots.len() as u32
        } else {
            1
        };
        if per == 0 {
            return Some(self.finish_action(
                EMPTY_SLOT_INDEX,
                drag.button as i8,
                5,
                &BTreeSet::new(),
            ));
        }
        let mut changed = BTreeSet::new();
        for &slot_index in &slots {
            if remaining == 0 {
                break;
            }
            let index = usize::try_from(slot_index).ok()?;
            let current = self.slots[index].clone();
            if current.is_empty() {
                let amount = per.min(remaining).min(u32::from(carried.max_stack));
                if amount == 0 {
                    continue;
                }
                let mut placed = carried.clone();
                placed.count = amount as u8;
                self.slots[index] = placed;
                remaining -= amount;
                changed.insert(slot_index);
            } else if current.same_stackable(&carried) {
                let capacity =
                    u32::from(current.max_stack).saturating_sub(u32::from(current.count));
                let amount = per.min(remaining).min(capacity);
                if amount == 0 {
                    continue;
                }
                let mut merged = current;
                merged.count += amount as u8;
                self.slots[index] = merged;
                remaining -= amount;
                changed.insert(slot_index);
            }
        }
        if let Some(mut cursor) = self.cursor.clone() {
            cursor.count = remaining as u8;
            self.cursor = if remaining == 0 { None } else { Some(cursor) };
        }
        Some(self.finish_action(EMPTY_SLOT_INDEX, drag.button as i8, 5, &changed))
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
                let mut left = stack.clone();
                left.count -= amount as u8;
                self.slots[slot] = if left.count == 0 {
                    UiItemStack::empty()
                } else {
                    left
                };
                cursor.count += amount as u8;
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
    pub fn click_mode(&mut self, slot: i16, button: i8, mode: u8) -> Option<ClickWindowAction> {
        match mode {
            0 => self.normal_click(slot, button),
            1 => self.quick_move(slot, button),
            2 => self.hotbar_swap(slot, button),
            4 => self.drop_items(slot, button),
            5 => self.drag_click(slot, button),
            6 => self.collect_to_cursor(slot, button),
            _ => None,
        }
    }

    /// Left (`button = 0`) or right (`button = 1`) click, mode `0`.
    pub fn click(&mut self, slot: i16, button: i8) -> Option<ClickWindowAction> {
        self.click_mode(slot, button, 0)
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

/// QoL inventory-sort state (Inventory-Sort / MouseTweaks semantics).
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
    pub fn slot_range_for(
        &self,
        screen: &ContainerScreenState,
        index: usize,
    ) -> Range<usize> {
        let len = screen.slot_count();
        let schema = screen.schema();
        let inv_start = schema.inventory_start.max(0) as usize;
        let inv_end = if schema.has_player_inventory {
            (inv_start + 36).min(len)
        } else {
            inv_start
        };
        if schema.has_player_inventory && index >= inv_start && index < inv_end {
            return inv_start..inv_end;
        }
        self.container_range(screen)
    }

    /// The sortable container region (non-player, placement-accepting
    /// slots) as one contiguous span, or `0..0` when none exists.
    pub fn container_range(&self, screen: &ContainerScreenState) -> Range<usize> {
        let len = screen.slot_count();
        let schema = screen.schema();
        let inv_start = schema.inventory_start.max(0) as usize;
        let inv_end = if schema.has_player_inventory {
            (inv_start + 36).min(len)
        } else {
            inv_start
        };
        let mut lo = len;
        let mut hi = 0usize;
        for i in 0..len {
            if schema.has_player_inventory && i >= inv_start && i < inv_end {
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
        let schema = screen.schema();
        if schema.has_player_inventory {
            let inv_start = schema.inventory_start.max(0) as usize;
            inv_start..(inv_start + 36).min(screen.slot_count())
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
