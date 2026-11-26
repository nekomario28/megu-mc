// SPDX-License-Identifier: LGPL-3.0-or-later
//! Standalone creative inventory model and deterministic renderer.
//!
//! The screen owns a built-in Vanilla 1.21.1 creative catalog. Host-supplied
//! inventory stacks may still be layered in through [`CreativeInventoryState::set_items`]
//! for legacy embedders, but a default state is fully browsable without any
//! synchronized inventory contents. All visible geometry is exposed through
//! shared rectangle accessors so rendering and hit-testing can never drift.

use std::collections::BTreeSet;

use crate::draw::{palette, text_width, UiColor, UiDrawList, UiRect};

pub const CREATIVE_PANEL_W: i32 = 194;
pub const CREATIVE_PANEL_H: i32 = 166;
const SLOT: i32 = 18;
const GRID_COLS: usize = 9;
const GRID_ROWS: usize = 5;
const GRID_VISIBLE: usize = GRID_COLS * GRID_ROWS;
const GRID_X: i32 = 8;
const GRID_Y: i32 = 24;
const HOTBAR_Y: i32 = 126;
const SEARCH_Y: i32 = 148;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CreativeTab {
    BuildingBlocks,
    ColoredBlocks,
    NaturalBlocks,
    FunctionalBlocks,
    Redstone,
    ToolsUtilities,
    Combat,
    FoodDrinks,
    Ingredients,
    Search,
    SurvivalInventory,
}

impl CreativeTab {
    pub const ALL: [Self; 11] = [
        Self::BuildingBlocks,
        Self::ColoredBlocks,
        Self::NaturalBlocks,
        Self::FunctionalBlocks,
        Self::Redstone,
        Self::ToolsUtilities,
        Self::Combat,
        Self::FoodDrinks,
        Self::Ingredients,
        Self::Search,
        Self::SurvivalInventory,
    ];
    pub const fn label(self) -> &'static str {
        match self {
            Self::BuildingBlocks => "Building Blocks",
            Self::ColoredBlocks => "Colored Blocks",
            Self::NaturalBlocks => "Natural Blocks",
            Self::FunctionalBlocks => "Functional Blocks",
            Self::Redstone => "Redstone Blocks",
            Self::ToolsUtilities => "Tools & Utilities",
            Self::Combat => "Combat",
            Self::FoodDrinks => "Food & Drinks",
            Self::Ingredients => "Ingredients",
            Self::Search => "Search",
            Self::SurvivalInventory => "Survival Inventory",
        }
    }

    const fn catalog(self) -> &'static [&'static str] {
        match self {
            Self::BuildingBlocks => BUILDING_ITEMS,
            Self::ColoredBlocks => COLORED_ITEMS,
            Self::NaturalBlocks => NATURAL_ITEMS,
            Self::FunctionalBlocks => FUNCTIONAL_ITEMS,
            Self::Redstone => REDSTONE_ITEMS,
            Self::ToolsUtilities => TOOLS_ITEMS,
            Self::Combat => COMBAT_ITEMS,
            Self::FoodDrinks => FOOD_ITEMS,
            Self::Ingredients => INGREDIENTS_ITEMS,
            Self::Search | Self::SurvivalInventory => &[],
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CreativeItem {
    pub item_id: i32,
    pub name: String,
    pub count: i32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CreativeInventoryAction {
    SetCreativeSlot { slot: i16, item_id: i32, count: i32 },
    ClearSlot { slot: i16 },
    Close,
}

#[derive(Clone, Debug)]
struct CatalogEntry {
    tab: CreativeTab,
    item: CreativeItem,
}

#[derive(Clone, Debug)]
pub struct CreativeInventoryState {
    pub tab: CreativeTab,
    /// Host-supplied legacy/inventory entries. Calling [`set_items`] makes
    /// this list replace the built-in catalog for compatibility with the
    /// original host adapter; the default state uses the Vanilla catalog.
    pub items: Vec<CreativeItem>,
    pub search: String,
    /// First catalog row currently visible.
    pub scroll: usize,
    pub hotbar: [Option<CreativeItem>; 9],
    /// Unlimited creative stack currently carried by the pointer.
    pub carried: Option<CreativeItem>,
    /// Whether typed characters edit the live search query.
    pub search_focused: bool,
    catalog: Vec<CatalogEntry>,
    external_catalog: bool,
}

impl Default for CreativeInventoryState {
    fn default() -> Self {
        Self {
            tab: CreativeTab::BuildingBlocks,
            items: Vec::new(),
            search: String::new(),
            scroll: 0,
            hotbar: Default::default(),
            carried: None,
            search_focused: false,
            catalog: build_vanilla_catalog(),
            external_catalog: false,
        }
    }
}

impl CreativeInventoryState {
    /// Replace the catalog with host-supplied stacks (the original adapter
    /// contract). New hosts can ignore this method entirely: [`Default`]
    /// already contains the Vanilla 1.21.1 creative inventory.
    pub fn set_items(&mut self, items: Vec<CreativeItem>) {
        self.items = items;
        self.external_catalog = true;
        self.scroll = 0;
    }

    /// Restore the built-in Vanilla catalog after a host-supplied list was installed.
    pub fn use_builtin_catalog(&mut self) {
        self.external_catalog = false;
        self.items.clear();
        self.scroll = 0;
    }

    pub fn filtered_items(&self) -> impl Iterator<Item = &CreativeItem> {
        let q = self.search.to_lowercase();
        let tab = self.tab;
        let external = self.external_catalog;
        let dedupe_search = !external && tab == CreativeTab::Search;
        let mut seen_search_items = BTreeSet::new();
        let builtin = self
            .catalog
            .iter()
            .filter(move |entry| {
                !external && tab != CreativeTab::SurvivalInventory
                    && (tab == CreativeTab::Search || entry.tab == tab)
            })
            // Items such as ender pearls legitimately appear in multiple
            // Vanilla tabs; Search is a union view and must not duplicate them.
            .filter(move |entry| !dedupe_search || seen_search_items.insert(entry.item.name.as_str()))
            .map(|entry| &entry.item);
        let custom = self
            .items
            .iter()
            .filter(move |_| external || tab == CreativeTab::SurvivalInventory);
        builtin
            .chain(custom)
            .filter(move |item| q.is_empty() || item.name.to_lowercase().contains(&q))
    }

    /// Number of rows needed by the current filtered catalog.
    pub fn filtered_row_count(&self) -> usize {
        self.filtered_items().count().div_ceil(GRID_COLS)
    }

    pub fn set_search(&mut self, text: impl Into<String>) {
        self.search = text.into();
        self.scroll = 0;
    }

    pub fn type_search_char(&mut self, ch: char) {
        if ch.is_control() {
            return;
        }
        self.search_focused = true;
        self.search.push(ch);
        self.scroll = 0;
    }

    pub fn search_backspace(&mut self) {
        self.search.pop();
        self.scroll = 0;
    }

    /// Scroll by whole catalog rows, clamped to the valid row window.
    pub fn scroll_by(&mut self, rows: i32) {
        let max_row = self.filtered_row_count().saturating_sub(GRID_ROWS);
        self.scroll = (self.scroll as i64 + i64::from(rows)).clamp(0, max_row as i64) as usize;
    }

    /// Centered panel rectangle for a viewport.
    pub fn panel_rect(&self, viewport_w: u32, viewport_h: u32) -> UiRect {
        UiRect::new(
            (viewport_w as i32 - CREATIVE_PANEL_W) / 2,
            (viewport_h as i32 - CREATIVE_PANEL_H) / 2,
            CREATIVE_PANEL_W as u32,
            CREATIVE_PANEL_H as u32,
        )
    }

    pub fn tab_rect(&self, tab: CreativeTab, viewport_w: u32, viewport_h: u32) -> Option<UiRect> {
        let index = CreativeTab::ALL.iter().position(|t| *t == tab)?;
        let panel = self.panel_rect(viewport_w, viewport_h);
        Some(UiRect::new(panel.x + 4 + index as i32 * 17, panel.y + 4, 16, 15))
    }

    /// Rectangle of one visible catalog grid cell (`0..44`). These are the
    /// same rectangles emitted by [`render`](Self::render) and consumed by
    /// [`hit_test`](Self::hit_test).
    pub fn catalog_slot_rect(&self, index: usize, viewport_w: u32, viewport_h: u32) -> Option<UiRect> {
        if index >= GRID_VISIBLE {
            return None;
        }
        let panel = self.panel_rect(viewport_w, viewport_h);
        Some(UiRect::new(
            panel.x + GRID_X + (index % GRID_COLS) as i32 * SLOT,
            panel.y + GRID_Y + (index / GRID_COLS) as i32 * SLOT,
            SLOT as u32,
            SLOT as u32,
        ))
    }

    /// Rectangle of hotbar slot `0..=8` (player-menu slots `36..=44`).
    pub fn hotbar_slot_rect(&self, slot: u8, viewport_w: u32, viewport_h: u32) -> Option<UiRect> {
        if slot > 8 {
            return None;
        }
        let panel = self.panel_rect(viewport_w, viewport_h);
        Some(UiRect::new(
            panel.x + 8 + i32::from(slot) * SLOT,
            panel.y + HOTBAR_Y,
            SLOT as u32,
            SLOT as u32,
        ))
    }

    /// Trash rectangle. It is deliberately to the right of hotbar slot 8:
    /// unlike the pre-fix layout, the two hit regions cannot overlap.
    pub fn trash_slot_rect(&self, viewport_w: u32, viewport_h: u32) -> UiRect {
        let panel = self.panel_rect(viewport_w, viewport_h);
        UiRect::new(panel.x + 174, panel.y + HOTBAR_Y, SLOT as u32, SLOT as u32)
    }

    pub fn search_box_rect(&self, viewport_w: u32, viewport_h: u32) -> UiRect {
        let panel = self.panel_rect(viewport_w, viewport_h);
        UiRect::new(panel.x + 8, panel.y + SEARCH_Y, 152, 14)
    }

    /// Hit-test through exactly the same rectangle accessors used for drawing.
    pub fn hit_test(&self, x: i32, y: i32, viewport_w: u32, viewport_h: u32) -> Option<CreativeClick> {
        for tab in CreativeTab::ALL {
            if self.tab_rect(tab, viewport_w, viewport_h).is_some_and(|r| r.contains(x, y)) {
                return Some(CreativeClick::Tab(tab));
            }
        }
        if self.search_box_rect(viewport_w, viewport_h).contains(x, y) {
            return Some(CreativeClick::SearchBox);
        }
        for i in 0..GRID_VISIBLE {
            if self.catalog_slot_rect(i, viewport_w, viewport_h).is_some_and(|r| r.contains(x, y)) {
                return Some(CreativeClick::Item(i));
            }
        }
        if self.trash_slot_rect(viewport_w, viewport_h).contains(x, y) {
            return Some(CreativeClick::Trash);
        }
        for slot in 0..=8u8 {
            if self.hotbar_slot_rect(slot, viewport_w, viewport_h).is_some_and(|r| r.contains(x, y)) {
                return Some(CreativeClick::Hotbar(slot));
            }
        }
        let panel = self.panel_rect(viewport_w, viewport_h);
        if panel.contains(x, y) && x >= panel.right() - 12 && y <= panel.y + 20 {
            return Some(CreativeClick::Close);
        }
        None
    }

    /// Click a visible item (0-based visible grid index), hotbar slot,
    /// trash, search box, or close control.
    pub fn click(&mut self, target: CreativeClick) -> Option<CreativeInventoryAction> {
        match target {
            CreativeClick::Item(i) => {
                let absolute = self.scroll * GRID_COLS + i;
                let item = self.filtered_items().nth(absolute)?.clone();
                self.carried = Some(item.clone());
                // Legacy externally supplied item lists historically emitted
                // the carried-stack packet immediately. Keep that adapter
                // behavior while the built-in catalog correctly performs a
                // local pick first and assigns a real hotbar slot on the
                // following click.
                self.external_catalog.then_some(CreativeInventoryAction::SetCreativeSlot {
                    slot: -1,
                    item_id: item.item_id,
                    count: item.count,
                })
            }
            CreativeClick::Hotbar(i) if i < 9 => {
                let slot = 36 + i as i16;
                if let Some(item) = self.carried.take() {
                    let count = item.count.max(1);
                    self.hotbar[i as usize] = Some(CreativeItem {
                        item_id: item.item_id,
                        name: item.name,
                        count,
                    });
                    return Some(CreativeInventoryAction::SetCreativeSlot {
                        slot,
                        item_id: item.item_id,
                        count,
                    });
                }
                match self.hotbar[i as usize].as_ref() {
                    Some(item) => Some(CreativeInventoryAction::SetCreativeSlot {
                        slot,
                        item_id: item.item_id,
                        count: item.count,
                    }),
                    None => Some(CreativeInventoryAction::ClearSlot { slot }),
                }
            }
            CreativeClick::Trash => {
                self.carried = None;
                Some(CreativeInventoryAction::ClearSlot { slot: -1 })
            }
            CreativeClick::Close => Some(CreativeInventoryAction::Close),
            CreativeClick::Tab(tab) => {
                self.tab = tab;
                self.scroll = 0;
                self.search_focused = tab == CreativeTab::Search;
                None
            }
            CreativeClick::SearchBox => {
                self.search_focused = true;
                None
            }
            _ => None,
        }
    }

    pub fn render(&self, w: u32, h: u32, out: &mut UiDrawList) {
        let panel = self.panel_rect(w, h);
        out.fill_rect(UiRect::new(0, 0, w, h), UiColor::rgba(0, 0, 0, 120));
        out.fill_rect(panel, palette::PANEL);
        out.border_rect(panel, palette::PANEL_BORDER, 1);

        for tab in CreativeTab::ALL {
            let Some(r) = self.tab_rect(tab, w, h) else { continue };
            let active = tab == self.tab;
            out.fill_rect(r, if active { palette::ACCENT } else { palette::BUTTON });
            out.border_rect(r, palette::PANEL_BORDER, 1);
            let short = match tab {
                CreativeTab::BuildingBlocks => "B",
                CreativeTab::ColoredBlocks => "C",
                CreativeTab::NaturalBlocks => "N",
                CreativeTab::FunctionalBlocks => "F",
                CreativeTab::Redstone => "R",
                CreativeTab::ToolsUtilities => "T",
                CreativeTab::Combat => "X",
                CreativeTab::FoodDrinks => "D",
                CreativeTab::Ingredients => "I",
                CreativeTab::Search => "?",
                CreativeTab::SurvivalInventory => "S",
            };
            out.text(r.x + 4, r.y + 4, 1, palette::TEXT, short);
        }
        out.text(panel.x + 8, panel.y + 21, 1, palette::TEXT_DIM, self.tab.label());

        let items: Vec<_> = self
            .filtered_items()
            .skip(self.scroll * GRID_COLS)
            .take(GRID_VISIBLE)
            .collect();
        for (i, item) in items.iter().enumerate() {
            let Some(r) = self.catalog_slot_rect(i, w, h) else { continue };
            out.fill_rect(r, palette::SLOT);
            out.border_rect(r, palette::PANEL_BORDER, 1);
            if item.name.contains(':') {
                out.item_icon_str(UiRect::new(r.x + 1, r.y + 1, 16, 16), &item.name);
            } else {
                out.text(
                    r.x + 4,
                    r.y + 5,
                    1,
                    palette::TEXT,
                    item.name.chars().next().unwrap_or('?').to_string(),
                );
            }
            if item.count > 1 {
                let count = item.count.to_string();
                out.text(
                    r.right() - 2 - text_width(&count, 1) as i32,
                    r.bottom() - 9,
                    1,
                    palette::TEXT,
                    count,
                );
            }
        }

        for slot in 0..=8u8 {
            let Some(r) = self.hotbar_slot_rect(slot, w, h) else { continue };
            out.fill_rect(r, palette::SLOT);
            out.border_rect(r, palette::PANEL_BORDER, 1);
            if let Some(item) = &self.hotbar[slot as usize] {
                if item.name.contains(':') {
                    out.item_icon_str(UiRect::new(r.x + 1, r.y + 1, 16, 16), &item.name);
                } else {
                    out.text(r.x + 4, r.y + 5, 1, palette::TEXT, item.name.chars().next().unwrap_or('?').to_string());
                }
            }
        }
        let trash = self.trash_slot_rect(w, h);
        out.fill_rect(trash, palette::DANGER);
        out.border_rect(trash, palette::PANEL_BORDER, 1);
        out.text(trash.x + 5, trash.y + 5, 1, palette::TEXT, "X");

        let search = self.search_box_rect(w, h);
        out.fill_rect(search, if self.search_focused { palette::SLOT } else { palette::BUTTON_DISABLED });
        out.border_rect(search, if self.search_focused { palette::ACCENT } else { palette::PANEL_BORDER }, 1);
        let mut query = self.search.clone();
        if self.search_focused {
            query.push('_');
        }
        if query.is_empty() {
            query.push_str("Search");
        }
        out.text(search.x + 4, search.y + 3, 1, palette::TEXT, query);

        let track = UiRect::new(panel.x + 174, panel.y + 25, 8, 89);
        out.fill_rect(track, palette::BUTTON_DISABLED);
        let max_row = self.filtered_row_count().saturating_sub(GRID_ROWS).max(1);
        let thumb_y = panel.y + 25 + (self.scroll * 70 / max_row) as i32;
        out.fill_rect(UiRect::new(panel.x + 174, thumb_y, 8, 14), palette::BUTTON);

        if let Some(item) = &self.carried {
            let r = UiRect::new(panel.right() - 24, panel.bottom() - 24, 18, 18);
            out.border_rect(r, palette::BUTTON_BORDER_HOVER, 1);
            out.item_icon_str(UiRect::new(r.x + 1, r.y + 1, 16, 16), &item.name);
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CreativeClick {
    Item(usize),
    Hotbar(u8),
    Trash,
    Close,
    Tab(CreativeTab),
    SearchBox,
}

fn vanilla_item_id(name: &str) -> i32 {
    // The table is ordered by numeric registry id (mirroring the protocol
    // ordering), not lexicographically. Use a linear lookup; this runs once
    // while constructing the immutable catalog and avoids silently returning
    // zero for most entries.
    VANILLA_ITEM_IDS
        .iter()
        .find_map(|(candidate, id)| (*candidate == name).then_some(*id))
        .unwrap_or(0)
}

fn creative_count(name: &str) -> i32 {
    let singular = [
        "sword", "pickaxe", "axe", "shovel", "hoe", "helmet", "chestplate", "leggings",
        "boots", "trident", "bow", "crossbow", "shield", "fishing_rod", "flint_and_steel",
        "shears", "elytra", "saddle", "mace", "brush", "potion", "bucket", "boat", "minecart",
    ];
    if singular.iter().any(|needle| name.ends_with(needle) || name.contains(&format!("_{needle}"))) {
        1
    } else {
        64
    }
}

fn build_vanilla_catalog() -> Vec<CatalogEntry> {
    let mut out = Vec::new();
    for tab in CreativeTab::ALL {
        for name in tab.catalog() {
            out.push(CatalogEntry {
                tab,
                item: CreativeItem {
                    item_id: vanilla_item_id(name),
                    name: (*name).to_owned(),
                    count: creative_count(name),
                },
            });
        }
    }
    out
}

const BUILDING_ITEMS: &[&str] = &[
    "minecraft:stone",
    "minecraft:granite",
    "minecraft:polished_granite",
    "minecraft:diorite",
    "minecraft:polished_diorite",
    "minecraft:andesite",
    "minecraft:polished_andesite",
    "minecraft:deepslate",
    "minecraft:cobbled_deepslate",
    "minecraft:polished_deepslate",
    "minecraft:calcite",
    "minecraft:tuff",
    "minecraft:tuff_slab",
    "minecraft:tuff_stairs",
    "minecraft:tuff_wall",
    "minecraft:chiseled_tuff",
    "minecraft:polished_tuff",
    "minecraft:tuff_bricks",
    "minecraft:chiseled_tuff_bricks",
    "minecraft:dripstone_block",
    "minecraft:cobblestone",
    "minecraft:mossy_cobblestone",
    "minecraft:oak_planks",
    "minecraft:spruce_planks",
    "minecraft:birch_planks",
    "minecraft:jungle_planks",
    "minecraft:acacia_planks",
    "minecraft:cherry_planks",
    "minecraft:dark_oak_planks",
    "minecraft:mangrove_planks",
    "minecraft:bamboo_planks",
    "minecraft:crimson_planks",
    "minecraft:warped_planks",
    "minecraft:bamboo_mosaic",
    "minecraft:oak_log",
    "minecraft:spruce_log",
    "minecraft:birch_log",
    "minecraft:jungle_log",
    "minecraft:acacia_log",
    "minecraft:cherry_log",
    "minecraft:dark_oak_log",
    "minecraft:mangrove_log",
    "minecraft:crimson_stem",
    "minecraft:warped_stem",
    "minecraft:oak_wood",
    "minecraft:spruce_wood",
    "minecraft:birch_wood",
    "minecraft:oak_stairs",
    "minecraft:spruce_stairs",
    "minecraft:birch_stairs",
    "minecraft:jungle_stairs",
    "minecraft:acacia_stairs",
    "minecraft:cherry_stairs",
    "minecraft:dark_oak_stairs",
    "minecraft:mangrove_stairs",
    "minecraft:bamboo_stairs",
    "minecraft:stone_stairs",
    "minecraft:cobblestone_stairs",
    "minecraft:brick_stairs",
    "minecraft:stone_brick_stairs",
    "minecraft:oak_slab",
    "minecraft:spruce_slab",
    "minecraft:birch_slab",
    "minecraft:stone_slab",
    "minecraft:cobblestone_slab",
    "minecraft:brick_slab",
    "minecraft:stone_brick_slab",
    "minecraft:bricks",
    "minecraft:stone_bricks",
    "minecraft:mossy_stone_bricks",
    "minecraft:cracked_stone_bricks",
    "minecraft:chiseled_stone_bricks",
    "minecraft:mud_bricks",
    "minecraft:nether_bricks",
    "minecraft:red_nether_bricks",
    "minecraft:quartz_block",
    "minecraft:smooth_quartz",
    "minecraft:quartz_bricks",
    "minecraft:purpur_block",
    "minecraft:purpur_pillar",
    "minecraft:prismarine",
    "minecraft:prismarine_bricks",
    "minecraft:dark_prismarine",
    "minecraft:sandstone",
    "minecraft:chiseled_sandstone",
    "minecraft:cut_sandstone",
    "minecraft:red_sandstone",
    "minecraft:chiseled_red_sandstone",
    "minecraft:cut_red_sandstone",
    "minecraft:obsidian",
    "minecraft:crying_obsidian",
    "minecraft:glass",
    "minecraft:tinted_glass",
    "minecraft:iron_block",
    "minecraft:gold_block",
    "minecraft:diamond_block",
    "minecraft:emerald_block",
    "minecraft:lapis_block",
    "minecraft:netherite_block",
    "minecraft:copper_block",
    "minecraft:exposed_copper",
    "minecraft:weathered_copper",
    "minecraft:oxidized_copper",
    "minecraft:amethyst_block",
    "minecraft:budding_amethyst",
    "minecraft:coal_block",
    "minecraft:raw_iron_block",
    "minecraft:raw_copper_block",
    "minecraft:raw_gold_block",
];

const COLORED_ITEMS: &[&str] = &[
    "minecraft:white_wool",
    "minecraft:orange_wool",
    "minecraft:magenta_wool",
    "minecraft:light_blue_wool",
    "minecraft:yellow_wool",
    "minecraft:lime_wool",
    "minecraft:pink_wool",
    "minecraft:gray_wool",
    "minecraft:light_gray_wool",
    "minecraft:cyan_wool",
    "minecraft:purple_wool",
    "minecraft:blue_wool",
    "minecraft:brown_wool",
    "minecraft:green_wool",
    "minecraft:red_wool",
    "minecraft:black_wool",
    "minecraft:white_concrete",
    "minecraft:orange_concrete",
    "minecraft:magenta_concrete",
    "minecraft:light_blue_concrete",
    "minecraft:yellow_concrete",
    "minecraft:lime_concrete",
    "minecraft:pink_concrete",
    "minecraft:gray_concrete",
    "minecraft:light_gray_concrete",
    "minecraft:cyan_concrete",
    "minecraft:purple_concrete",
    "minecraft:blue_concrete",
    "minecraft:brown_concrete",
    "minecraft:green_concrete",
    "minecraft:red_concrete",
    "minecraft:black_concrete",
    "minecraft:white_concrete_powder",
    "minecraft:orange_concrete_powder",
    "minecraft:magenta_concrete_powder",
    "minecraft:light_blue_concrete_powder",
    "minecraft:yellow_concrete_powder",
    "minecraft:lime_concrete_powder",
    "minecraft:pink_concrete_powder",
    "minecraft:gray_concrete_powder",
    "minecraft:light_gray_concrete_powder",
    "minecraft:cyan_concrete_powder",
    "minecraft:purple_concrete_powder",
    "minecraft:blue_concrete_powder",
    "minecraft:brown_concrete_powder",
    "minecraft:green_concrete_powder",
    "minecraft:red_concrete_powder",
    "minecraft:black_concrete_powder",
    "minecraft:white_terracotta",
    "minecraft:orange_terracotta",
    "minecraft:magenta_terracotta",
    "minecraft:light_blue_terracotta",
    "minecraft:yellow_terracotta",
    "minecraft:lime_terracotta",
    "minecraft:pink_terracotta",
    "minecraft:gray_terracotta",
    "minecraft:light_gray_terracotta",
    "minecraft:cyan_terracotta",
    "minecraft:purple_terracotta",
    "minecraft:blue_terracotta",
    "minecraft:brown_terracotta",
    "minecraft:green_terracotta",
    "minecraft:red_terracotta",
    "minecraft:black_terracotta",
    "minecraft:white_stained_glass",
    "minecraft:orange_stained_glass",
    "minecraft:magenta_stained_glass",
    "minecraft:light_blue_stained_glass",
    "minecraft:yellow_stained_glass",
    "minecraft:lime_stained_glass",
    "minecraft:pink_stained_glass",
    "minecraft:gray_stained_glass",
    "minecraft:light_gray_stained_glass",
    "minecraft:cyan_stained_glass",
    "minecraft:purple_stained_glass",
    "minecraft:blue_stained_glass",
    "minecraft:brown_stained_glass",
    "minecraft:green_stained_glass",
    "minecraft:red_stained_glass",
    "minecraft:black_stained_glass",
    "minecraft:white_stained_glass_pane",
    "minecraft:orange_stained_glass_pane",
    "minecraft:magenta_stained_glass_pane",
    "minecraft:light_blue_stained_glass_pane",
    "minecraft:yellow_stained_glass_pane",
    "minecraft:lime_stained_glass_pane",
    "minecraft:pink_stained_glass_pane",
    "minecraft:gray_stained_glass_pane",
    "minecraft:light_gray_stained_glass_pane",
    "minecraft:cyan_stained_glass_pane",
    "minecraft:purple_stained_glass_pane",
    "minecraft:blue_stained_glass_pane",
    "minecraft:brown_stained_glass_pane",
    "minecraft:green_stained_glass_pane",
    "minecraft:red_stained_glass_pane",
    "minecraft:black_stained_glass_pane",
    "minecraft:white_carpet",
    "minecraft:orange_carpet",
    "minecraft:magenta_carpet",
    "minecraft:light_blue_carpet",
    "minecraft:yellow_carpet",
    "minecraft:lime_carpet",
    "minecraft:pink_carpet",
    "minecraft:gray_carpet",
    "minecraft:light_gray_carpet",
    "minecraft:cyan_carpet",
    "minecraft:purple_carpet",
    "minecraft:blue_carpet",
    "minecraft:brown_carpet",
    "minecraft:green_carpet",
    "minecraft:red_carpet",
    "minecraft:black_carpet",
    "minecraft:white_bed",
    "minecraft:orange_bed",
    "minecraft:magenta_bed",
    "minecraft:light_blue_bed",
    "minecraft:yellow_bed",
    "minecraft:lime_bed",
    "minecraft:pink_bed",
    "minecraft:gray_bed",
    "minecraft:light_gray_bed",
    "minecraft:cyan_bed",
    "minecraft:purple_bed",
    "minecraft:blue_bed",
    "minecraft:brown_bed",
    "minecraft:green_bed",
    "minecraft:red_bed",
    "minecraft:black_bed",
    "minecraft:white_banner",
    "minecraft:orange_banner",
    "minecraft:magenta_banner",
    "minecraft:light_blue_banner",
    "minecraft:yellow_banner",
    "minecraft:lime_banner",
    "minecraft:pink_banner",
    "minecraft:gray_banner",
    "minecraft:light_gray_banner",
    "minecraft:cyan_banner",
    "minecraft:purple_banner",
    "minecraft:blue_banner",
    "minecraft:brown_banner",
    "minecraft:green_banner",
    "minecraft:red_banner",
    "minecraft:black_banner",
    "minecraft:white_candle",
    "minecraft:orange_candle",
    "minecraft:magenta_candle",
    "minecraft:light_blue_candle",
    "minecraft:yellow_candle",
    "minecraft:lime_candle",
    "minecraft:pink_candle",
    "minecraft:gray_candle",
    "minecraft:light_gray_candle",
    "minecraft:cyan_candle",
    "minecraft:purple_candle",
    "minecraft:blue_candle",
    "minecraft:brown_candle",
    "minecraft:green_candle",
    "minecraft:red_candle",
    "minecraft:black_candle",
    "minecraft:terracotta",
    "minecraft:white_glazed_terracotta",
    "minecraft:orange_glazed_terracotta",
    "minecraft:magenta_glazed_terracotta",
    "minecraft:light_blue_glazed_terracotta",
    "minecraft:yellow_glazed_terracotta",
    "minecraft:lime_glazed_terracotta",
    "minecraft:pink_glazed_terracotta",
    "minecraft:gray_glazed_terracotta",
    "minecraft:light_gray_glazed_terracotta",
    "minecraft:cyan_glazed_terracotta",
    "minecraft:purple_glazed_terracotta",
    "minecraft:blue_glazed_terracotta",
    "minecraft:brown_glazed_terracotta",
    "minecraft:green_glazed_terracotta",
    "minecraft:red_glazed_terracotta",
    "minecraft:black_glazed_terracotta",
];

const NATURAL_ITEMS: &[&str] = &[
    "minecraft:grass_block",
    "minecraft:dirt",
    "minecraft:coarse_dirt",
    "minecraft:podzol",
    "minecraft:rooted_dirt",
    "minecraft:mud",
    "minecraft:muddy_mangrove_roots",
    "minecraft:sand",
    "minecraft:suspicious_sand",
    "minecraft:red_sand",
    "minecraft:gravel",
    "minecraft:suspicious_gravel",
    "minecraft:clay",
    "minecraft:snow_block",
    "minecraft:ice",
    "minecraft:packed_ice",
    "minecraft:blue_ice",
    "minecraft:cobweb",
    "minecraft:short_grass",
    "minecraft:fern",
    "minecraft:tall_grass",
    "minecraft:large_fern",
    "minecraft:oak_sapling",
    "minecraft:spruce_sapling",
    "minecraft:birch_sapling",
    "minecraft:jungle_sapling",
    "minecraft:acacia_sapling",
    "minecraft:cherry_sapling",
    "minecraft:dark_oak_sapling",
    "minecraft:mangrove_propagule",
    "minecraft:oak_leaves",
    "minecraft:spruce_leaves",
    "minecraft:birch_leaves",
    "minecraft:jungle_leaves",
    "minecraft:acacia_leaves",
    "minecraft:cherry_leaves",
    "minecraft:dark_oak_leaves",
    "minecraft:mangrove_leaves",
    "minecraft:azalea_leaves",
    "minecraft:flowering_azalea_leaves",
    "minecraft:dandelion",
    "minecraft:poppy",
    "minecraft:blue_orchid",
    "minecraft:allium",
    "minecraft:azure_bluet",
    "minecraft:red_tulip",
    "minecraft:orange_tulip",
    "minecraft:white_tulip",
    "minecraft:pink_tulip",
    "minecraft:oxeye_daisy",
    "minecraft:cornflower",
    "minecraft:lily_of_the_valley",
    "minecraft:wither_rose",
    "minecraft:torchflower",
    "minecraft:pitcher_plant",
    "minecraft:spore_blossom",
    "minecraft:brown_mushroom",
    "minecraft:red_mushroom",
    "minecraft:crimson_fungus",
    "minecraft:warped_fungus",
    "minecraft:crimson_roots",
    "minecraft:warped_roots",
    "minecraft:nether_sprouts",
    "minecraft:weeping_vines",
    "minecraft:twisting_vines",
    "minecraft:sugar_cane",
    "minecraft:kelp",
    "minecraft:moss_carpet",
    "minecraft:pink_petals",
    "minecraft:moss_block",
    "minecraft:hanging_roots",
    "minecraft:big_dripleaf",
    "minecraft:small_dripleaf",
    "minecraft:bamboo",
    "minecraft:vine",
    "minecraft:glow_lichen",
    "minecraft:lily_pad",
    "minecraft:cactus",
    "minecraft:pumpkin",
    "minecraft:carved_pumpkin",
    "minecraft:jack_o_lantern",
    "minecraft:melon",
    "minecraft:sunflower",
    "minecraft:lilac",
    "minecraft:rose_bush",
    "minecraft:peony",
    "minecraft:seagrass",
    "minecraft:sea_pickle",
    "minecraft:netherrack",
    "minecraft:crimson_nylium",
    "minecraft:warped_nylium",
    "minecraft:soul_sand",
    "minecraft:soul_soil",
    "minecraft:basalt",
    "minecraft:smooth_basalt",
    "minecraft:blackstone",
    "minecraft:end_stone",
    "minecraft:coal_ore",
    "minecraft:iron_ore",
    "minecraft:copper_ore",
    "minecraft:gold_ore",
    "minecraft:redstone_ore",
    "minecraft:emerald_ore",
    "minecraft:lapis_ore",
    "minecraft:diamond_ore",
    "minecraft:deepslate_coal_ore",
    "minecraft:deepslate_iron_ore",
    "minecraft:deepslate_copper_ore",
    "minecraft:deepslate_gold_ore",
    "minecraft:deepslate_redstone_ore",
    "minecraft:deepslate_emerald_ore",
    "minecraft:deepslate_lapis_ore",
    "minecraft:deepslate_diamond_ore",
    "minecraft:nether_gold_ore",
    "minecraft:nether_quartz_ore",
    "minecraft:ancient_debris",
];

const FUNCTIONAL_ITEMS: &[&str] = &[
    "minecraft:crafting_table",
    "minecraft:furnace",
    "minecraft:blast_furnace",
    "minecraft:smoker",
    "minecraft:anvil",
    "minecraft:chipped_anvil",
    "minecraft:damaged_anvil",
    "minecraft:enchanting_table",
    "minecraft:brewing_stand",
    "minecraft:cauldron",
    "minecraft:lectern",
    "minecraft:loom",
    "minecraft:stonecutter",
    "minecraft:grindstone",
    "minecraft:smithing_table",
    "minecraft:cartography_table",
    "minecraft:fletching_table",
    "minecraft:composter",
    "minecraft:barrel",
    "minecraft:chest",
    "minecraft:trapped_chest",
    "minecraft:ender_chest",
    "minecraft:shulker_box",
    "minecraft:white_shulker_box",
    "minecraft:hopper",
    "minecraft:dropper",
    "minecraft:dispenser",
    "minecraft:observer",
    "minecraft:crafter",
    "minecraft:beacon",
    "minecraft:conduit",
    "minecraft:jukebox",
    "minecraft:note_block",
    "minecraft:bookshelf",
    "minecraft:chiseled_bookshelf",
    "minecraft:ladder",
    "minecraft:scaffolding",
    "minecraft:torch",
    "minecraft:soul_torch",
    "minecraft:lantern",
    "minecraft:soul_lantern",
    "minecraft:campfire",
    "minecraft:soul_campfire",
    "minecraft:bell",
    "minecraft:lightning_rod",
    "minecraft:lodestone",
    "minecraft:respawn_anchor",
    "minecraft:end_portal_frame",
    "minecraft:flower_pot",
    "minecraft:decorated_pot",
    "minecraft:armor_stand",
    "minecraft:item_frame",
    "minecraft:glow_item_frame",
    "minecraft:painting",
    "minecraft:oak_sign",
    "minecraft:spruce_sign",
    "minecraft:birch_sign",
    "minecraft:oak_hanging_sign",
    "minecraft:white_bed",
];

const REDSTONE_ITEMS: &[&str] = &[
    "minecraft:redstone",
    "minecraft:redstone_torch",
    "minecraft:redstone_block",
    "minecraft:repeater",
    "minecraft:comparator",
    "minecraft:piston",
    "minecraft:sticky_piston",
    "minecraft:slime_block",
    "minecraft:honey_block",
    "minecraft:observer",
    "minecraft:dropper",
    "minecraft:dispenser",
    "minecraft:hopper",
    "minecraft:crafter",
    "minecraft:lever",
    "minecraft:stone_button",
    "minecraft:oak_button",
    "minecraft:spruce_button",
    "minecraft:stone_pressure_plate",
    "minecraft:oak_pressure_plate",
    "minecraft:heavy_weighted_pressure_plate",
    "minecraft:light_weighted_pressure_plate",
    "minecraft:tripwire_hook",
    "minecraft:string",
    "minecraft:daylight_detector",
    "minecraft:target",
    "minecraft:lectern",
    "minecraft:sculk_sensor",
    "minecraft:calibrated_sculk_sensor",
    "minecraft:tnt",
    "minecraft:note_block",
    "minecraft:iron_door",
    "minecraft:oak_door",
    "minecraft:iron_trapdoor",
    "minecraft:oak_trapdoor",
    "minecraft:oak_fence_gate",
    "minecraft:redstone_lamp",
    "minecraft:bell",
    "minecraft:rail",
    "minecraft:powered_rail",
    "minecraft:detector_rail",
    "minecraft:activator_rail",
    "minecraft:minecart",
    "minecraft:chest_minecart",
    "minecraft:furnace_minecart",
    "minecraft:hopper_minecart",
    "minecraft:tnt_minecart",
    "minecraft:command_block",
    "minecraft:chain_command_block",
    "minecraft:repeating_command_block",
    "minecraft:structure_block",
    "minecraft:jigsaw",
];

const TOOLS_ITEMS: &[&str] = &[
    "minecraft:wooden_shovel",
    "minecraft:stone_shovel",
    "minecraft:iron_shovel",
    "minecraft:golden_shovel",
    "minecraft:diamond_shovel",
    "minecraft:netherite_shovel",
    "minecraft:wooden_pickaxe",
    "minecraft:stone_pickaxe",
    "minecraft:iron_pickaxe",
    "minecraft:golden_pickaxe",
    "minecraft:diamond_pickaxe",
    "minecraft:netherite_pickaxe",
    "minecraft:wooden_axe",
    "minecraft:stone_axe",
    "minecraft:iron_axe",
    "minecraft:golden_axe",
    "minecraft:diamond_axe",
    "minecraft:netherite_axe",
    "minecraft:wooden_hoe",
    "minecraft:stone_hoe",
    "minecraft:iron_hoe",
    "minecraft:golden_hoe",
    "minecraft:diamond_hoe",
    "minecraft:netherite_hoe",
    "minecraft:fishing_rod",
    "minecraft:flint_and_steel",
    "minecraft:shears",
    "minecraft:compass",
    "minecraft:recovery_compass",
    "minecraft:clock",
    "minecraft:map",
    "minecraft:bucket",
    "minecraft:water_bucket",
    "minecraft:lava_bucket",
    "minecraft:milk_bucket",
    "minecraft:powder_snow_bucket",
    "minecraft:axolotl_bucket",
    "minecraft:spyglass",
    "minecraft:lead",
    "minecraft:elytra",
    "minecraft:firework_rocket",
    "minecraft:fire_charge",
    "minecraft:name_tag",
    "minecraft:saddle",
    "minecraft:carrot_on_a_stick",
    "minecraft:warped_fungus_on_a_stick",
    "minecraft:oak_boat",
    "minecraft:oak_chest_boat",
    "minecraft:spruce_boat",
    "minecraft:birch_boat",
    "minecraft:jungle_boat",
    "minecraft:acacia_boat",
    "minecraft:cherry_boat",
    "minecraft:dark_oak_boat",
    "minecraft:mangrove_boat",
    "minecraft:bamboo_raft",
    "minecraft:minecart",
    "minecraft:chest_minecart",
    "minecraft:furnace_minecart",
    "minecraft:hopper_minecart",
    "minecraft:tnt_minecart",
    "minecraft:brush",
    "minecraft:bundle",
    "minecraft:ender_pearl",
    "minecraft:ender_eye",
    "minecraft:totem_of_undying",
    "minecraft:goat_horn",
];

const COMBAT_ITEMS: &[&str] = &[
    "minecraft:wooden_sword",
    "minecraft:stone_sword",
    "minecraft:iron_sword",
    "minecraft:golden_sword",
    "minecraft:diamond_sword",
    "minecraft:netherite_sword",
    "minecraft:trident",
    "minecraft:bow",
    "minecraft:crossbow",
    "minecraft:arrow",
    "minecraft:spectral_arrow",
    "minecraft:tipped_arrow",
    "minecraft:shield",
    "minecraft:turtle_helmet",
    "minecraft:leather_helmet",
    "minecraft:chainmail_helmet",
    "minecraft:iron_helmet",
    "minecraft:golden_helmet",
    "minecraft:diamond_helmet",
    "minecraft:netherite_helmet",
    "minecraft:leather_chestplate",
    "minecraft:chainmail_chestplate",
    "minecraft:iron_chestplate",
    "minecraft:golden_chestplate",
    "minecraft:diamond_chestplate",
    "minecraft:netherite_chestplate",
    "minecraft:leather_leggings",
    "minecraft:chainmail_leggings",
    "minecraft:iron_leggings",
    "minecraft:golden_leggings",
    "minecraft:diamond_leggings",
    "minecraft:netherite_leggings",
    "minecraft:leather_boots",
    "minecraft:chainmail_boots",
    "minecraft:iron_boots",
    "minecraft:golden_boots",
    "minecraft:diamond_boots",
    "minecraft:netherite_boots",
    "minecraft:totem_of_undying",
    "minecraft:firework_rocket",
    "minecraft:splash_potion",
    "minecraft:lingering_potion",
    "minecraft:enchanted_golden_apple",
    "minecraft:end_crystal",
    "minecraft:snowball",
    "minecraft:egg",
    "minecraft:experience_bottle",
];

const FOOD_ITEMS: &[&str] = &[
    "minecraft:apple",
    "minecraft:golden_apple",
    "minecraft:enchanted_golden_apple",
    "minecraft:bread",
    "minecraft:porkchop",
    "minecraft:cooked_porkchop",
    "minecraft:beef",
    "minecraft:cooked_beef",
    "minecraft:chicken",
    "minecraft:cooked_chicken",
    "minecraft:mutton",
    "minecraft:cooked_mutton",
    "minecraft:rabbit",
    "minecraft:cooked_rabbit",
    "minecraft:rabbit_stew",
    "minecraft:cod",
    "minecraft:cooked_cod",
    "minecraft:salmon",
    "minecraft:cooked_salmon",
    "minecraft:tropical_fish",
    "minecraft:pufferfish",
    "minecraft:potato",
    "minecraft:baked_potato",
    "minecraft:poisonous_potato",
    "minecraft:carrot",
    "minecraft:golden_carrot",
    "minecraft:beetroot",
    "minecraft:beetroot_soup",
    "minecraft:melon_slice",
    "minecraft:glistering_melon_slice",
    "minecraft:cookie",
    "minecraft:cake",
    "minecraft:pumpkin_pie",
    "minecraft:mushroom_stew",
    "minecraft:suspicious_stew",
    "minecraft:honey_bottle",
    "minecraft:milk_bucket",
    "minecraft:sweet_berries",
    "minecraft:glow_berries",
    "minecraft:chorus_fruit",
    "minecraft:dried_kelp",
    "minecraft:rotten_flesh",
    "minecraft:spider_eye",
];

const INGREDIENTS_ITEMS: &[&str] = &[
    "minecraft:stick",
    "minecraft:coal",
    "minecraft:charcoal",
    "minecraft:iron_ingot",
    "minecraft:gold_ingot",
    "minecraft:netherite_ingot",
    "minecraft:copper_ingot",
    "minecraft:emerald",
    "minecraft:diamond",
    "minecraft:lapis_lazuli",
    "minecraft:redstone",
    "minecraft:quartz",
    "minecraft:amethyst_shard",
    "minecraft:raw_iron",
    "minecraft:raw_gold",
    "minecraft:raw_copper",
    "minecraft:clay_ball",
    "minecraft:brick",
    "minecraft:nether_brick",
    "minecraft:flint",
    "minecraft:gunpowder",
    "minecraft:leather",
    "minecraft:rabbit_hide",
    "minecraft:rabbit_foot",
    "minecraft:string",
    "minecraft:feather",
    "minecraft:ink_sac",
    "minecraft:glow_ink_sac",
    "minecraft:slime_ball",
    "minecraft:magma_cream",
    "minecraft:blaze_powder",
    "minecraft:blaze_rod",
    "minecraft:ghast_tear",
    "minecraft:ender_pearl",
    "minecraft:ender_eye",
    "minecraft:nether_star",
    "minecraft:dragon_breath",
    "minecraft:phantom_membrane",
    "minecraft:shulker_shell",
    "minecraft:nautilus_shell",
    "minecraft:heart_of_the_sea",
    "minecraft:prismarine_shard",
    "minecraft:prismarine_crystals",
    "minecraft:honeycomb",
    "minecraft:bone",
    "minecraft:bone_meal",
    "minecraft:wheat",
    "minecraft:wheat_seeds",
    "minecraft:beetroot_seeds",
    "minecraft:melon_seeds",
    "minecraft:pumpkin_seeds",
    "minecraft:torchflower_seeds",
    "minecraft:pitcher_pod",
    "minecraft:cocoa_beans",
    "minecraft:sugar",
    "minecraft:paper",
    "minecraft:book",
    "minecraft:writable_book",
    "minecraft:written_book",
    "minecraft:fire_charge",
    "minecraft:echo_shard",
    "minecraft:disc_fragment_5",
    "minecraft:netherite_scrap",
    "minecraft:nether_wart",
    "minecraft:glowstone_dust",
    "minecraft:sugar_cane",
    "minecraft:egg",
    "minecraft:turtle_scute",
    "minecraft:armadillo_scute",
    "minecraft:breeze_rod",
    "minecraft:trial_key",
    "minecraft:ominous_trial_key",
    "minecraft:wind_charge",
    "minecraft:mace",
    "minecraft:white_dye",
    "minecraft:orange_dye",
    "minecraft:magenta_dye",
    "minecraft:light_blue_dye",
    "minecraft:yellow_dye",
    "minecraft:lime_dye",
    "minecraft:pink_dye",
    "minecraft:gray_dye",
    "minecraft:light_gray_dye",
    "minecraft:cyan_dye",
    "minecraft:purple_dye",
    "minecraft:blue_dye",
    "minecraft:brown_dye",
    "minecraft:green_dye",
    "minecraft:red_dye",
    "minecraft:black_dye",
];

/// `(namespaced item id, protocol-767 numeric item id)`, sorted by id string.
/// The table is generated from the in-repository Vanilla 1.21.1 registry data.
static VANILLA_ITEM_IDS: &[(&str, i32)] = &[
    ("minecraft:stone", 1),
    ("minecraft:granite", 2),
    ("minecraft:polished_granite", 3),
    ("minecraft:diorite", 4),
    ("minecraft:polished_diorite", 5),
    ("minecraft:andesite", 6),
    ("minecraft:polished_andesite", 7),
    ("minecraft:deepslate", 8),
    ("minecraft:cobbled_deepslate", 9),
    ("minecraft:polished_deepslate", 10),
    ("minecraft:calcite", 11),
    ("minecraft:tuff", 12),
    ("minecraft:tuff_slab", 13),
    ("minecraft:tuff_stairs", 14),
    ("minecraft:tuff_wall", 15),
    ("minecraft:chiseled_tuff", 16),
    ("minecraft:polished_tuff", 17),
    ("minecraft:tuff_bricks", 21),
    ("minecraft:chiseled_tuff_bricks", 25),
    ("minecraft:dripstone_block", 26),
    ("minecraft:grass_block", 27),
    ("minecraft:dirt", 28),
    ("minecraft:coarse_dirt", 29),
    ("minecraft:podzol", 30),
    ("minecraft:rooted_dirt", 31),
    ("minecraft:mud", 32),
    ("minecraft:crimson_nylium", 33),
    ("minecraft:warped_nylium", 34),
    ("minecraft:cobblestone", 35),
    ("minecraft:oak_planks", 36),
    ("minecraft:spruce_planks", 37),
    ("minecraft:birch_planks", 38),
    ("minecraft:jungle_planks", 39),
    ("minecraft:acacia_planks", 40),
    ("minecraft:cherry_planks", 41),
    ("minecraft:dark_oak_planks", 42),
    ("minecraft:mangrove_planks", 43),
    ("minecraft:bamboo_planks", 44),
    ("minecraft:crimson_planks", 45),
    ("minecraft:warped_planks", 46),
    ("minecraft:bamboo_mosaic", 47),
    ("minecraft:oak_sapling", 48),
    ("minecraft:spruce_sapling", 49),
    ("minecraft:birch_sapling", 50),
    ("minecraft:jungle_sapling", 51),
    ("minecraft:acacia_sapling", 52),
    ("minecraft:cherry_sapling", 53),
    ("minecraft:dark_oak_sapling", 54),
    ("minecraft:mangrove_propagule", 55),
    ("minecraft:sand", 57),
    ("minecraft:suspicious_sand", 58),
    ("minecraft:suspicious_gravel", 59),
    ("minecraft:red_sand", 60),
    ("minecraft:gravel", 61),
    ("minecraft:coal_ore", 62),
    ("minecraft:deepslate_coal_ore", 63),
    ("minecraft:iron_ore", 64),
    ("minecraft:deepslate_iron_ore", 65),
    ("minecraft:copper_ore", 66),
    ("minecraft:deepslate_copper_ore", 67),
    ("minecraft:gold_ore", 68),
    ("minecraft:deepslate_gold_ore", 69),
    ("minecraft:redstone_ore", 70),
    ("minecraft:deepslate_redstone_ore", 71),
    ("minecraft:emerald_ore", 72),
    ("minecraft:deepslate_emerald_ore", 73),
    ("minecraft:lapis_ore", 74),
    ("minecraft:deepslate_lapis_ore", 75),
    ("minecraft:diamond_ore", 76),
    ("minecraft:deepslate_diamond_ore", 77),
    ("minecraft:nether_gold_ore", 78),
    ("minecraft:nether_quartz_ore", 79),
    ("minecraft:ancient_debris", 80),
    ("minecraft:coal_block", 81),
    ("minecraft:raw_iron_block", 82),
    ("minecraft:raw_copper_block", 83),
    ("minecraft:raw_gold_block", 84),
    ("minecraft:amethyst_block", 86),
    ("minecraft:budding_amethyst", 87),
    ("minecraft:iron_block", 88),
    ("minecraft:copper_block", 89),
    ("minecraft:gold_block", 90),
    ("minecraft:diamond_block", 91),
    ("minecraft:netherite_block", 92),
    ("minecraft:exposed_copper", 93),
    ("minecraft:weathered_copper", 94),
    ("minecraft:oxidized_copper", 95),
    ("minecraft:oak_log", 132),
    ("minecraft:spruce_log", 133),
    ("minecraft:birch_log", 134),
    ("minecraft:jungle_log", 135),
    ("minecraft:acacia_log", 136),
    ("minecraft:cherry_log", 137),
    ("minecraft:dark_oak_log", 138),
    ("minecraft:mangrove_log", 139),
    ("minecraft:muddy_mangrove_roots", 141),
    ("minecraft:crimson_stem", 142),
    ("minecraft:warped_stem", 143),
    ("minecraft:oak_wood", 166),
    ("minecraft:spruce_wood", 167),
    ("minecraft:birch_wood", 168),
    ("minecraft:oak_leaves", 176),
    ("minecraft:spruce_leaves", 177),
    ("minecraft:birch_leaves", 178),
    ("minecraft:jungle_leaves", 179),
    ("minecraft:acacia_leaves", 180),
    ("minecraft:cherry_leaves", 181),
    ("minecraft:dark_oak_leaves", 182),
    ("minecraft:mangrove_leaves", 183),
    ("minecraft:azalea_leaves", 184),
    ("minecraft:flowering_azalea_leaves", 185),
    ("minecraft:glass", 188),
    ("minecraft:tinted_glass", 189),
    ("minecraft:lapis_block", 190),
    ("minecraft:sandstone", 191),
    ("minecraft:chiseled_sandstone", 192),
    ("minecraft:cut_sandstone", 193),
    ("minecraft:cobweb", 194),
    ("minecraft:short_grass", 195),
    ("minecraft:fern", 196),
    ("minecraft:seagrass", 200),
    ("minecraft:sea_pickle", 201),
    ("minecraft:white_wool", 202),
    ("minecraft:orange_wool", 203),
    ("minecraft:magenta_wool", 204),
    ("minecraft:light_blue_wool", 205),
    ("minecraft:yellow_wool", 206),
    ("minecraft:lime_wool", 207),
    ("minecraft:pink_wool", 208),
    ("minecraft:gray_wool", 209),
    ("minecraft:light_gray_wool", 210),
    ("minecraft:cyan_wool", 211),
    ("minecraft:purple_wool", 212),
    ("minecraft:blue_wool", 213),
    ("minecraft:brown_wool", 214),
    ("minecraft:green_wool", 215),
    ("minecraft:red_wool", 216),
    ("minecraft:black_wool", 217),
    ("minecraft:dandelion", 218),
    ("minecraft:poppy", 219),
    ("minecraft:blue_orchid", 220),
    ("minecraft:allium", 221),
    ("minecraft:azure_bluet", 222),
    ("minecraft:red_tulip", 223),
    ("minecraft:orange_tulip", 224),
    ("minecraft:white_tulip", 225),
    ("minecraft:pink_tulip", 226),
    ("minecraft:oxeye_daisy", 227),
    ("minecraft:cornflower", 228),
    ("minecraft:lily_of_the_valley", 229),
    ("minecraft:wither_rose", 230),
    ("minecraft:torchflower", 231),
    ("minecraft:pitcher_plant", 232),
    ("minecraft:spore_blossom", 233),
    ("minecraft:brown_mushroom", 234),
    ("minecraft:red_mushroom", 235),
    ("minecraft:crimson_fungus", 236),
    ("minecraft:warped_fungus", 237),
    ("minecraft:crimson_roots", 238),
    ("minecraft:warped_roots", 239),
    ("minecraft:nether_sprouts", 240),
    ("minecraft:weeping_vines", 241),
    ("minecraft:twisting_vines", 242),
    ("minecraft:sugar_cane", 243),
    ("minecraft:kelp", 244),
    ("minecraft:moss_carpet", 245),
    ("minecraft:pink_petals", 246),
    ("minecraft:moss_block", 247),
    ("minecraft:hanging_roots", 248),
    ("minecraft:big_dripleaf", 249),
    ("minecraft:small_dripleaf", 250),
    ("minecraft:bamboo", 251),
    ("minecraft:oak_slab", 252),
    ("minecraft:spruce_slab", 253),
    ("minecraft:birch_slab", 254),
    ("minecraft:stone_slab", 264),
    ("minecraft:cobblestone_slab", 269),
    ("minecraft:brick_slab", 270),
    ("minecraft:stone_brick_slab", 271),
    ("minecraft:smooth_quartz", 281),
    ("minecraft:bricks", 285),
    ("minecraft:bookshelf", 286),
    ("minecraft:chiseled_bookshelf", 287),
    ("minecraft:decorated_pot", 288),
    ("minecraft:mossy_cobblestone", 289),
    ("minecraft:obsidian", 290),
    ("minecraft:torch", 291),
    ("minecraft:purpur_block", 295),
    ("minecraft:purpur_pillar", 296),
    ("minecraft:chest", 299),
    ("minecraft:crafting_table", 300),
    ("minecraft:furnace", 302),
    ("minecraft:ladder", 303),
    ("minecraft:cobblestone_stairs", 304),
    ("minecraft:ice", 306),
    ("minecraft:snow_block", 307),
    ("minecraft:cactus", 308),
    ("minecraft:clay", 309),
    ("minecraft:jukebox", 310),
    ("minecraft:pumpkin", 322),
    ("minecraft:carved_pumpkin", 323),
    ("minecraft:jack_o_lantern", 324),
    ("minecraft:netherrack", 325),
    ("minecraft:soul_sand", 326),
    ("minecraft:soul_soil", 327),
    ("minecraft:basalt", 328),
    ("minecraft:smooth_basalt", 330),
    ("minecraft:soul_torch", 331),
    ("minecraft:stone_bricks", 340),
    ("minecraft:mossy_stone_bricks", 341),
    ("minecraft:cracked_stone_bricks", 342),
    ("minecraft:chiseled_stone_bricks", 343),
    ("minecraft:mud_bricks", 345),
    ("minecraft:melon", 358),
    ("minecraft:vine", 359),
    ("minecraft:glow_lichen", 360),
    ("minecraft:brick_stairs", 361),
    ("minecraft:stone_brick_stairs", 362),
    ("minecraft:lily_pad", 365),
    ("minecraft:nether_bricks", 366),
    ("minecraft:enchanting_table", 375),
    ("minecraft:end_portal_frame", 376),
    ("minecraft:end_stone", 377),
    ("minecraft:ender_chest", 381),
    ("minecraft:emerald_block", 382),
    ("minecraft:oak_stairs", 383),
    ("minecraft:spruce_stairs", 384),
    ("minecraft:birch_stairs", 385),
    ("minecraft:jungle_stairs", 386),
    ("minecraft:acacia_stairs", 387),
    ("minecraft:cherry_stairs", 388),
    ("minecraft:dark_oak_stairs", 389),
    ("minecraft:mangrove_stairs", 390),
    ("minecraft:bamboo_stairs", 391),
    ("minecraft:command_block", 395),
    ("minecraft:beacon", 396),
    ("minecraft:anvil", 419),
    ("minecraft:chipped_anvil", 420),
    ("minecraft:damaged_anvil", 421),
    ("minecraft:quartz_block", 423),
    ("minecraft:quartz_bricks", 424),
    ("minecraft:white_terracotta", 427),
    ("minecraft:orange_terracotta", 428),
    ("minecraft:magenta_terracotta", 429),
    ("minecraft:light_blue_terracotta", 430),
    ("minecraft:yellow_terracotta", 431),
    ("minecraft:lime_terracotta", 432),
    ("minecraft:pink_terracotta", 433),
    ("minecraft:gray_terracotta", 434),
    ("minecraft:light_gray_terracotta", 435),
    ("minecraft:cyan_terracotta", 436),
    ("minecraft:purple_terracotta", 437),
    ("minecraft:blue_terracotta", 438),
    ("minecraft:brown_terracotta", 439),
    ("minecraft:green_terracotta", 440),
    ("minecraft:red_terracotta", 441),
    ("minecraft:black_terracotta", 442),
    ("minecraft:white_carpet", 446),
    ("minecraft:orange_carpet", 447),
    ("minecraft:magenta_carpet", 448),
    ("minecraft:light_blue_carpet", 449),
    ("minecraft:yellow_carpet", 450),
    ("minecraft:lime_carpet", 451),
    ("minecraft:pink_carpet", 452),
    ("minecraft:gray_carpet", 453),
    ("minecraft:light_gray_carpet", 454),
    ("minecraft:cyan_carpet", 455),
    ("minecraft:purple_carpet", 456),
    ("minecraft:blue_carpet", 457),
    ("minecraft:brown_carpet", 458),
    ("minecraft:green_carpet", 459),
    ("minecraft:red_carpet", 460),
    ("minecraft:black_carpet", 461),
    ("minecraft:terracotta", 462),
    ("minecraft:packed_ice", 463),
    ("minecraft:sunflower", 465),
    ("minecraft:lilac", 466),
    ("minecraft:rose_bush", 467),
    ("minecraft:peony", 468),
    ("minecraft:tall_grass", 469),
    ("minecraft:large_fern", 470),
    ("minecraft:white_stained_glass", 471),
    ("minecraft:orange_stained_glass", 472),
    ("minecraft:magenta_stained_glass", 473),
    ("minecraft:light_blue_stained_glass", 474),
    ("minecraft:yellow_stained_glass", 475),
    ("minecraft:lime_stained_glass", 476),
    ("minecraft:pink_stained_glass", 477),
    ("minecraft:gray_stained_glass", 478),
    ("minecraft:light_gray_stained_glass", 479),
    ("minecraft:cyan_stained_glass", 480),
    ("minecraft:purple_stained_glass", 481),
    ("minecraft:blue_stained_glass", 482),
    ("minecraft:brown_stained_glass", 483),
    ("minecraft:green_stained_glass", 484),
    ("minecraft:red_stained_glass", 485),
    ("minecraft:black_stained_glass", 486),
    ("minecraft:white_stained_glass_pane", 487),
    ("minecraft:orange_stained_glass_pane", 488),
    ("minecraft:magenta_stained_glass_pane", 489),
    ("minecraft:light_blue_stained_glass_pane", 490),
    ("minecraft:yellow_stained_glass_pane", 491),
    ("minecraft:lime_stained_glass_pane", 492),
    ("minecraft:pink_stained_glass_pane", 493),
    ("minecraft:gray_stained_glass_pane", 494),
    ("minecraft:light_gray_stained_glass_pane", 495),
    ("minecraft:cyan_stained_glass_pane", 496),
    ("minecraft:purple_stained_glass_pane", 497),
    ("minecraft:blue_stained_glass_pane", 498),
    ("minecraft:brown_stained_glass_pane", 499),
    ("minecraft:green_stained_glass_pane", 500),
    ("minecraft:red_stained_glass_pane", 501),
    ("minecraft:black_stained_glass_pane", 502),
    ("minecraft:prismarine", 503),
    ("minecraft:prismarine_bricks", 504),
    ("minecraft:dark_prismarine", 505),
    ("minecraft:red_sandstone", 510),
    ("minecraft:chiseled_red_sandstone", 511),
    ("minecraft:cut_red_sandstone", 512),
    ("minecraft:repeating_command_block", 514),
    ("minecraft:chain_command_block", 515),
    ("minecraft:red_nether_bricks", 519),
    ("minecraft:shulker_box", 522),
    ("minecraft:white_shulker_box", 523),
    ("minecraft:white_glazed_terracotta", 539),
    ("minecraft:orange_glazed_terracotta", 540),
    ("minecraft:magenta_glazed_terracotta", 541),
    ("minecraft:light_blue_glazed_terracotta", 542),
    ("minecraft:yellow_glazed_terracotta", 543),
    ("minecraft:lime_glazed_terracotta", 544),
    ("minecraft:pink_glazed_terracotta", 545),
    ("minecraft:gray_glazed_terracotta", 546),
    ("minecraft:light_gray_glazed_terracotta", 547),
    ("minecraft:cyan_glazed_terracotta", 548),
    ("minecraft:purple_glazed_terracotta", 549),
    ("minecraft:blue_glazed_terracotta", 550),
    ("minecraft:brown_glazed_terracotta", 551),
    ("minecraft:green_glazed_terracotta", 552),
    ("minecraft:red_glazed_terracotta", 553),
    ("minecraft:black_glazed_terracotta", 554),
    ("minecraft:white_concrete", 555),
    ("minecraft:orange_concrete", 556),
    ("minecraft:magenta_concrete", 557),
    ("minecraft:light_blue_concrete", 558),
    ("minecraft:yellow_concrete", 559),
    ("minecraft:lime_concrete", 560),
    ("minecraft:pink_concrete", 561),
    ("minecraft:gray_concrete", 562),
    ("minecraft:light_gray_concrete", 563),
    ("minecraft:cyan_concrete", 564),
    ("minecraft:purple_concrete", 565),
    ("minecraft:blue_concrete", 566),
    ("minecraft:brown_concrete", 567),
    ("minecraft:green_concrete", 568),
    ("minecraft:red_concrete", 569),
    ("minecraft:black_concrete", 570),
    ("minecraft:white_concrete_powder", 571),
    ("minecraft:orange_concrete_powder", 572),
    ("minecraft:magenta_concrete_powder", 573),
    ("minecraft:light_blue_concrete_powder", 574),
    ("minecraft:yellow_concrete_powder", 575),
    ("minecraft:lime_concrete_powder", 576),
    ("minecraft:pink_concrete_powder", 577),
    ("minecraft:gray_concrete_powder", 578),
    ("minecraft:light_gray_concrete_powder", 579),
    ("minecraft:cyan_concrete_powder", 580),
    ("minecraft:purple_concrete_powder", 581),
    ("minecraft:blue_concrete_powder", 582),
    ("minecraft:brown_concrete_powder", 583),
    ("minecraft:green_concrete_powder", 584),
    ("minecraft:red_concrete_powder", 585),
    ("minecraft:black_concrete_powder", 586),
    ("minecraft:blue_ice", 619),
    ("minecraft:conduit", 620),
    ("minecraft:stone_stairs", 627),
    ("minecraft:scaffolding", 656),
    ("minecraft:redstone", 657),
    ("minecraft:redstone_torch", 658),
    ("minecraft:redstone_block", 659),
    ("minecraft:repeater", 660),
    ("minecraft:comparator", 661),
    ("minecraft:piston", 662),
    ("minecraft:sticky_piston", 663),
    ("minecraft:slime_block", 664),
    ("minecraft:honey_block", 665),
    ("minecraft:observer", 666),
    ("minecraft:hopper", 667),
    ("minecraft:dispenser", 668),
    ("minecraft:dropper", 669),
    ("minecraft:lectern", 670),
    ("minecraft:target", 671),
    ("minecraft:lever", 672),
    ("minecraft:lightning_rod", 673),
    ("minecraft:daylight_detector", 674),
    ("minecraft:sculk_sensor", 675),
    ("minecraft:calibrated_sculk_sensor", 676),
    ("minecraft:tripwire_hook", 677),
    ("minecraft:trapped_chest", 678),
    ("minecraft:tnt", 679),
    ("minecraft:redstone_lamp", 680),
    ("minecraft:note_block", 681),
    ("minecraft:stone_button", 682),
    ("minecraft:oak_button", 684),
    ("minecraft:spruce_button", 685),
    ("minecraft:stone_pressure_plate", 695),
    ("minecraft:light_weighted_pressure_plate", 697),
    ("minecraft:heavy_weighted_pressure_plate", 698),
    ("minecraft:oak_pressure_plate", 699),
    ("minecraft:iron_door", 710),
    ("minecraft:oak_door", 711),
    ("minecraft:iron_trapdoor", 730),
    ("minecraft:oak_trapdoor", 731),
    ("minecraft:oak_fence_gate", 750),
    ("minecraft:powered_rail", 761),
    ("minecraft:detector_rail", 762),
    ("minecraft:rail", 763),
    ("minecraft:activator_rail", 764),
    ("minecraft:saddle", 765),
    ("minecraft:minecart", 766),
    ("minecraft:chest_minecart", 767),
    ("minecraft:furnace_minecart", 768),
    ("minecraft:tnt_minecart", 769),
    ("minecraft:hopper_minecart", 770),
    ("minecraft:carrot_on_a_stick", 771),
    ("minecraft:warped_fungus_on_a_stick", 772),
    ("minecraft:elytra", 773),
    ("minecraft:oak_boat", 774),
    ("minecraft:oak_chest_boat", 775),
    ("minecraft:spruce_boat", 776),
    ("minecraft:birch_boat", 778),
    ("minecraft:jungle_boat", 780),
    ("minecraft:acacia_boat", 782),
    ("minecraft:cherry_boat", 784),
    ("minecraft:dark_oak_boat", 786),
    ("minecraft:mangrove_boat", 788),
    ("minecraft:bamboo_raft", 790),
    ("minecraft:structure_block", 792),
    ("minecraft:jigsaw", 793),
    ("minecraft:turtle_helmet", 794),
    ("minecraft:turtle_scute", 795),
    ("minecraft:armadillo_scute", 796),
    ("minecraft:flint_and_steel", 798),
    ("minecraft:apple", 800),
    ("minecraft:bow", 801),
    ("minecraft:arrow", 802),
    ("minecraft:coal", 803),
    ("minecraft:charcoal", 804),
    ("minecraft:diamond", 805),
    ("minecraft:emerald", 806),
    ("minecraft:lapis_lazuli", 807),
    ("minecraft:quartz", 808),
    ("minecraft:amethyst_shard", 809),
    ("minecraft:raw_iron", 810),
    ("minecraft:iron_ingot", 811),
    ("minecraft:raw_copper", 812),
    ("minecraft:copper_ingot", 813),
    ("minecraft:raw_gold", 814),
    ("minecraft:gold_ingot", 815),
    ("minecraft:netherite_ingot", 816),
    ("minecraft:netherite_scrap", 817),
    ("minecraft:wooden_sword", 818),
    ("minecraft:wooden_shovel", 819),
    ("minecraft:wooden_pickaxe", 820),
    ("minecraft:wooden_axe", 821),
    ("minecraft:wooden_hoe", 822),
    ("minecraft:stone_sword", 823),
    ("minecraft:stone_shovel", 824),
    ("minecraft:stone_pickaxe", 825),
    ("minecraft:stone_axe", 826),
    ("minecraft:stone_hoe", 827),
    ("minecraft:golden_sword", 828),
    ("minecraft:golden_shovel", 829),
    ("minecraft:golden_pickaxe", 830),
    ("minecraft:golden_axe", 831),
    ("minecraft:golden_hoe", 832),
    ("minecraft:iron_sword", 833),
    ("minecraft:iron_shovel", 834),
    ("minecraft:iron_pickaxe", 835),
    ("minecraft:iron_axe", 836),
    ("minecraft:iron_hoe", 837),
    ("minecraft:diamond_sword", 838),
    ("minecraft:diamond_shovel", 839),
    ("minecraft:diamond_pickaxe", 840),
    ("minecraft:diamond_axe", 841),
    ("minecraft:diamond_hoe", 842),
    ("minecraft:netherite_sword", 843),
    ("minecraft:netherite_shovel", 844),
    ("minecraft:netherite_pickaxe", 845),
    ("minecraft:netherite_axe", 846),
    ("minecraft:netherite_hoe", 847),
    ("minecraft:stick", 848),
    ("minecraft:mushroom_stew", 849),
    ("minecraft:string", 850),
    ("minecraft:feather", 851),
    ("minecraft:gunpowder", 852),
    ("minecraft:wheat_seeds", 853),
    ("minecraft:wheat", 854),
    ("minecraft:bread", 855),
    ("minecraft:leather_helmet", 856),
    ("minecraft:leather_chestplate", 857),
    ("minecraft:leather_leggings", 858),
    ("minecraft:leather_boots", 859),
    ("minecraft:chainmail_helmet", 860),
    ("minecraft:chainmail_chestplate", 861),
    ("minecraft:chainmail_leggings", 862),
    ("minecraft:chainmail_boots", 863),
    ("minecraft:iron_helmet", 864),
    ("minecraft:iron_chestplate", 865),
    ("minecraft:iron_leggings", 866),
    ("minecraft:iron_boots", 867),
    ("minecraft:diamond_helmet", 868),
    ("minecraft:diamond_chestplate", 869),
    ("minecraft:diamond_leggings", 870),
    ("minecraft:diamond_boots", 871),
    ("minecraft:golden_helmet", 872),
    ("minecraft:golden_chestplate", 873),
    ("minecraft:golden_leggings", 874),
    ("minecraft:golden_boots", 875),
    ("minecraft:netherite_helmet", 876),
    ("minecraft:netherite_chestplate", 877),
    ("minecraft:netherite_leggings", 878),
    ("minecraft:netherite_boots", 879),
    ("minecraft:flint", 880),
    ("minecraft:porkchop", 881),
    ("minecraft:cooked_porkchop", 882),
    ("minecraft:painting", 883),
    ("minecraft:golden_apple", 884),
    ("minecraft:enchanted_golden_apple", 885),
    ("minecraft:oak_sign", 886),
    ("minecraft:spruce_sign", 887),
    ("minecraft:birch_sign", 888),
    ("minecraft:oak_hanging_sign", 897),
    ("minecraft:bucket", 908),
    ("minecraft:water_bucket", 909),
    ("minecraft:lava_bucket", 910),
    ("minecraft:powder_snow_bucket", 911),
    ("minecraft:snowball", 912),
    ("minecraft:leather", 913),
    ("minecraft:milk_bucket", 914),
    ("minecraft:axolotl_bucket", 919),
    ("minecraft:brick", 921),
    ("minecraft:clay_ball", 922),
    ("minecraft:paper", 924),
    ("minecraft:book", 925),
    ("minecraft:slime_ball", 926),
    ("minecraft:egg", 927),
    ("minecraft:compass", 928),
    ("minecraft:recovery_compass", 929),
    ("minecraft:bundle", 930),
    ("minecraft:fishing_rod", 931),
    ("minecraft:clock", 932),
    ("minecraft:spyglass", 933),
    ("minecraft:glowstone_dust", 934),
    ("minecraft:cod", 935),
    ("minecraft:salmon", 936),
    ("minecraft:tropical_fish", 937),
    ("minecraft:pufferfish", 938),
    ("minecraft:cooked_cod", 939),
    ("minecraft:cooked_salmon", 940),
    ("minecraft:ink_sac", 941),
    ("minecraft:glow_ink_sac", 942),
    ("minecraft:cocoa_beans", 943),
    ("minecraft:white_dye", 944),
    ("minecraft:orange_dye", 945),
    ("minecraft:magenta_dye", 946),
    ("minecraft:light_blue_dye", 947),
    ("minecraft:yellow_dye", 948),
    ("minecraft:lime_dye", 949),
    ("minecraft:pink_dye", 950),
    ("minecraft:gray_dye", 951),
    ("minecraft:light_gray_dye", 952),
    ("minecraft:cyan_dye", 953),
    ("minecraft:purple_dye", 954),
    ("minecraft:blue_dye", 955),
    ("minecraft:brown_dye", 956),
    ("minecraft:green_dye", 957),
    ("minecraft:red_dye", 958),
    ("minecraft:black_dye", 959),
    ("minecraft:bone_meal", 960),
    ("minecraft:bone", 961),
    ("minecraft:sugar", 962),
    ("minecraft:cake", 963),
    ("minecraft:white_bed", 964),
    ("minecraft:orange_bed", 965),
    ("minecraft:magenta_bed", 966),
    ("minecraft:light_blue_bed", 967),
    ("minecraft:yellow_bed", 968),
    ("minecraft:lime_bed", 969),
    ("minecraft:pink_bed", 970),
    ("minecraft:gray_bed", 971),
    ("minecraft:light_gray_bed", 972),
    ("minecraft:cyan_bed", 973),
    ("minecraft:purple_bed", 974),
    ("minecraft:blue_bed", 975),
    ("minecraft:brown_bed", 976),
    ("minecraft:green_bed", 977),
    ("minecraft:red_bed", 978),
    ("minecraft:black_bed", 979),
    ("minecraft:cookie", 980),
    ("minecraft:crafter", 981),
    ("minecraft:shears", 983),
    ("minecraft:melon_slice", 984),
    ("minecraft:dried_kelp", 985),
    ("minecraft:pumpkin_seeds", 986),
    ("minecraft:melon_seeds", 987),
    ("minecraft:beef", 988),
    ("minecraft:cooked_beef", 989),
    ("minecraft:chicken", 990),
    ("minecraft:cooked_chicken", 991),
    ("minecraft:rotten_flesh", 992),
    ("minecraft:ender_pearl", 993),
    ("minecraft:blaze_rod", 994),
    ("minecraft:ghast_tear", 995),
    ("minecraft:nether_wart", 997),
    ("minecraft:spider_eye", 1000),
    ("minecraft:blaze_powder", 1002),
    ("minecraft:magma_cream", 1003),
    ("minecraft:brewing_stand", 1004),
    ("minecraft:cauldron", 1005),
    ("minecraft:ender_eye", 1006),
    ("minecraft:glistering_melon_slice", 1007),
    ("minecraft:experience_bottle", 1088),
    ("minecraft:fire_charge", 1089),
    ("minecraft:wind_charge", 1090),
    ("minecraft:writable_book", 1091),
    ("minecraft:written_book", 1092),
    ("minecraft:mace", 1093),
    ("minecraft:item_frame", 1094),
    ("minecraft:glow_item_frame", 1095),
    ("minecraft:flower_pot", 1096),
    ("minecraft:carrot", 1097),
    ("minecraft:potato", 1098),
    ("minecraft:baked_potato", 1099),
    ("minecraft:poisonous_potato", 1100),
    ("minecraft:map", 1101),
    ("minecraft:golden_carrot", 1102),
    ("minecraft:nether_star", 1110),
    ("minecraft:pumpkin_pie", 1111),
    ("minecraft:firework_rocket", 1112),
    ("minecraft:nether_brick", 1115),
    ("minecraft:prismarine_shard", 1116),
    ("minecraft:prismarine_crystals", 1117),
    ("minecraft:rabbit", 1118),
    ("minecraft:cooked_rabbit", 1119),
    ("minecraft:rabbit_stew", 1120),
    ("minecraft:rabbit_foot", 1121),
    ("minecraft:rabbit_hide", 1122),
    ("minecraft:armor_stand", 1123),
    ("minecraft:lead", 1128),
    ("minecraft:name_tag", 1129),
    ("minecraft:mutton", 1131),
    ("minecraft:cooked_mutton", 1132),
    ("minecraft:white_banner", 1133),
    ("minecraft:orange_banner", 1134),
    ("minecraft:magenta_banner", 1135),
    ("minecraft:light_blue_banner", 1136),
    ("minecraft:yellow_banner", 1137),
    ("minecraft:lime_banner", 1138),
    ("minecraft:pink_banner", 1139),
    ("minecraft:gray_banner", 1140),
    ("minecraft:light_gray_banner", 1141),
    ("minecraft:cyan_banner", 1142),
    ("minecraft:purple_banner", 1143),
    ("minecraft:blue_banner", 1144),
    ("minecraft:brown_banner", 1145),
    ("minecraft:green_banner", 1146),
    ("minecraft:red_banner", 1147),
    ("minecraft:black_banner", 1148),
    ("minecraft:end_crystal", 1149),
    ("minecraft:chorus_fruit", 1150),
    ("minecraft:torchflower_seeds", 1152),
    ("minecraft:pitcher_pod", 1153),
    ("minecraft:beetroot", 1154),
    ("minecraft:beetroot_seeds", 1155),
    ("minecraft:beetroot_soup", 1156),
    ("minecraft:dragon_breath", 1157),
    ("minecraft:splash_potion", 1158),
    ("minecraft:spectral_arrow", 1159),
    ("minecraft:tipped_arrow", 1160),
    ("minecraft:lingering_potion", 1161),
    ("minecraft:shield", 1162),
    ("minecraft:totem_of_undying", 1163),
    ("minecraft:shulker_shell", 1164),
    ("minecraft:disc_fragment_5", 1187),
    ("minecraft:trident", 1188),
    ("minecraft:phantom_membrane", 1189),
    ("minecraft:nautilus_shell", 1190),
    ("minecraft:heart_of_the_sea", 1191),
    ("minecraft:crossbow", 1192),
    ("minecraft:suspicious_stew", 1193),
    ("minecraft:loom", 1194),
    ("minecraft:goat_horn", 1203),
    ("minecraft:composter", 1204),
    ("minecraft:barrel", 1205),
    ("minecraft:smoker", 1206),
    ("minecraft:blast_furnace", 1207),
    ("minecraft:cartography_table", 1208),
    ("minecraft:fletching_table", 1209),
    ("minecraft:grindstone", 1210),
    ("minecraft:smithing_table", 1211),
    ("minecraft:stonecutter", 1212),
    ("minecraft:bell", 1213),
    ("minecraft:lantern", 1214),
    ("minecraft:soul_lantern", 1215),
    ("minecraft:sweet_berries", 1216),
    ("minecraft:glow_berries", 1217),
    ("minecraft:campfire", 1218),
    ("minecraft:soul_campfire", 1219),
    ("minecraft:honeycomb", 1221),
    ("minecraft:honey_bottle", 1224),
    ("minecraft:lodestone", 1226),
    ("minecraft:crying_obsidian", 1227),
    ("minecraft:blackstone", 1228),
    ("minecraft:respawn_anchor", 1240),
    ("minecraft:white_candle", 1242),
    ("minecraft:orange_candle", 1243),
    ("minecraft:magenta_candle", 1244),
    ("minecraft:light_blue_candle", 1245),
    ("minecraft:yellow_candle", 1246),
    ("minecraft:lime_candle", 1247),
    ("minecraft:pink_candle", 1248),
    ("minecraft:gray_candle", 1249),
    ("minecraft:light_gray_candle", 1250),
    ("minecraft:cyan_candle", 1251),
    ("minecraft:purple_candle", 1252),
    ("minecraft:blue_candle", 1253),
    ("minecraft:brown_candle", 1254),
    ("minecraft:green_candle", 1255),
    ("minecraft:red_candle", 1256),
    ("minecraft:black_candle", 1257),
    ("minecraft:echo_shard", 1267),
    ("minecraft:brush", 1268),
    ("minecraft:trial_key", 1328),
    ("minecraft:ominous_trial_key", 1329),
    ("minecraft:breeze_rod", 1332),
];
