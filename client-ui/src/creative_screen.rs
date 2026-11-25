// SPDX-License-Identifier: LGPL-3.0-or-later
//! Standalone creative inventory model and deterministic renderer.

use crate::draw::{palette, UiColor, UiDrawList, UiRect};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CreativeTab {
    BuildingBlocks,
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
    pub const ALL: [Self; 10] = [
        Self::BuildingBlocks,
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
            Self::NaturalBlocks => "Natural Blocks",
            Self::FunctionalBlocks => "Functional Blocks",
            Self::Redstone => "Redstone",
            Self::ToolsUtilities => "Tools & Utilities",
            Self::Combat => "Combat",
            Self::FoodDrinks => "Food & Drinks",
            Self::Ingredients => "Ingredients",
            Self::Search => "Search",
            Self::SurvivalInventory => "Survival Inventory",
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
pub struct CreativeInventoryState {
    pub tab: CreativeTab,
    pub items: Vec<CreativeItem>,
    pub search: String,
    pub scroll: usize,
    pub hotbar: [Option<CreativeItem>; 9],
}

impl Default for CreativeInventoryState {
    fn default() -> Self {
        Self {
            tab: CreativeTab::BuildingBlocks,
            items: Vec::new(),
            search: String::new(),
            scroll: 0,
            hotbar: Default::default(),
        }
    }
}

impl CreativeInventoryState {
    pub fn set_items(&mut self, items: Vec<CreativeItem>) {
        self.items = items;
        self.scroll = 0;
    }
    pub fn filtered_items(&self) -> impl Iterator<Item = &CreativeItem> {
        let q = self.search.to_lowercase();
        self.items
            .iter()
            .filter(move |item| q.is_empty() || item.name.to_lowercase().contains(&q))
    }
    pub fn set_search(&mut self, text: impl Into<String>) {
        self.search = text.into();
        self.scroll = 0;
    }
    pub fn scroll_by(&mut self, rows: i32) {
        let n = self.filtered_items().count().saturating_sub(45);
        self.scroll = (self.scroll as i64 + i64::from(rows)).clamp(0, n as i64) as usize;
    }
    /// Click a visible item (0-based filtered index), hotbar slot, trash, or close.
    pub fn click(&mut self, target: CreativeClick) -> Option<CreativeInventoryAction> {
        match target {
            CreativeClick::Item(i) => {
                let item = self.filtered_items().nth(self.scroll + i)?.clone();
                Some(CreativeInventoryAction::SetCreativeSlot {
                    slot: -1,
                    item_id: item.item_id,
                    count: item.count,
                })
            }
            CreativeClick::Hotbar(i) if i < 9 => {
                let slot = 36 + i as i16;
                match self.hotbar[i as usize].as_ref() {
                    Some(item) => Some(CreativeInventoryAction::SetCreativeSlot {
                        slot,
                        item_id: item.item_id,
                        count: item.count,
                    }),
                    None => Some(CreativeInventoryAction::ClearSlot { slot }),
                }
            }
            CreativeClick::Trash => Some(CreativeInventoryAction::ClearSlot { slot: -1 }),
            CreativeClick::Close => Some(CreativeInventoryAction::Close),
            CreativeClick::Tab(tab) => {
                self.tab = tab;
                self.scroll = 0;
                None
            }
            _ => None,
        }
    }
    pub fn render(&self, w: u32, h: u32, out: &mut UiDrawList) {
        if w < 200 || h < 140 {
            return;
        }
        let x = (w as i32 - 194) / 2;
        let y = (h as i32 - 166) / 2;
        let panel = UiRect::new(x, y, 194, 166);
        out.fill_rect(UiRect::new(0, 0, w, h), UiColor::rgba(0, 0, 0, 120));
        out.fill_rect(panel, palette::PANEL);
        out.border_rect(panel, palette::PANEL_BORDER, 1);
        out.text(x + 8, y + 6, 1, palette::TEXT, self.tab.label());
        let items: Vec<_> = self.filtered_items().skip(self.scroll).take(45).collect();
        for (i, item) in items.iter().enumerate() {
            let r = UiRect::new(
                x + 8 + (i % 9) as i32 * 19,
                y + 24 + (i / 9) as i32 * 19,
                18,
                18,
            );
            out.fill_rect(r, palette::SLOT);
            out.border_rect(r, palette::PANEL_BORDER, 1);
            out.text(
                r.x + 4,
                r.y + 5,
                1,
                palette::TEXT,
                item.name.chars().next().unwrap_or('?').to_string(),
            );
        }
        let hot_y = y + 132;
        for i in 0..9 {
            let r = UiRect::new(x + 8 + i * 19, hot_y, 18, 18);
            out.fill_rect(r, palette::SLOT);
            out.border_rect(r, palette::PANEL_BORDER, 1);
        }
        let trash = UiRect::new(x + 174, hot_y, 18, 18);
        out.fill_rect(trash, palette::DANGER);
        out.border_rect(trash, palette::PANEL_BORDER, 1);
        if self.tab == CreativeTab::Search {
            out.text(x + 8, y + 151, 1, palette::TEXT, &self.search);
        }
        let track = UiRect::new(x + 184, y + 25, 6, 100);
        out.fill_rect(track, palette::BUTTON_DISABLED);
        let max = self.filtered_items().count().saturating_sub(45).max(1);
        let thumb_y = y + 25 + (self.scroll * 88 / max) as i32;
        out.fill_rect(UiRect::new(x + 183, thumb_y, 8, 12), palette::BUTTON);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CreativeClick {
    Item(usize),
    Hotbar(u8),
    Trash,
    Close,
    Tab(CreativeTab),
}
