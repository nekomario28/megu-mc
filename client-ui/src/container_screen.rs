// SPDX-License-Identifier: LGPL-3.0-or-later
//! Declarative Vanilla container/menu screen engine for protocol 767.
//!
//! One [`MenuScreenSchema`] table covers every `minecraft:menu` registry id
//! (`0..=24`) and one [`ContainerScreenState`] implements the deterministic
//! click-window model used by the Vanilla client. Rendering is a pure
//! [`UiDrawList`] producer: centered panel, slots, items, progress bars,
//! cursor, buttons, and a viewport-clamped tooltip.

use std::collections::{BTreeMap, BTreeSet};

use crate::draw::{palette, text_height, text_width, UiColor, UiDrawList, UiRect};

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
const BEACON_BUTTONS: &[MenuButtonDef] = &[MenuButtonDef::new(0, 136, 80, 24, 12, "ok")];
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

#[derive(Clone, Debug, PartialEq)]
pub struct UiEnchantment {
    pub name: String,
    pub level: i32,
}

/// UI-facing item stack. `count == 0` is the empty stack.
#[derive(Clone, Debug, PartialEq)]
pub struct UiItemStack {
    pub item_id: u32,
    pub count: u8,
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
        let buttons = self
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
                continue;
            }
            let icon = item_icon_color(item.item_id);
            let inset = UiRect::new(slot.rect.x + 3, slot.rect.y + 3, 12, 12);
            out.fill_rect(inset, icon);
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
        if let Some(cursor) = self.cursor.clone().filter(|s| !s.is_empty()) {
            let rect = UiRect::new(vw - 24, vh - 24, 20, 20);
            out.fill_rect(rect, palette::SLOT);
            out.border_rect(rect, palette::BUTTON_BORDER_HOVER, 1);
            out.fill_rect(
                UiRect::new(rect.x + 3, rect.y + 3, 14, 14),
                item_icon_color(cursor.item_id),
            );
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
}

fn item_icon_color(item_id: u32) -> UiColor {
    let h = item_id.wrapping_mul(2654435761);
    UiColor::rgb(
        ((h >> 16) & 0xff) as u8,
        ((h >> 8) & 0xff) as u8,
        (h & 0xff) as u8,
    )
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
