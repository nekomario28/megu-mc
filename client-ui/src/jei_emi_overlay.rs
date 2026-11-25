// SPDX-License-Identifier: LGPL-3.0-or-later
//! JEI/EMI-style *native* container overlay — the side-by-side item list
//! that renders *alongside* any centered container GUI (inventory,
//! crafting table, furnace, chest, anvil, ...) instead of replacing the
//! screen like the full [`crate::recipe_browser`] does.
//!
//! 100% clean-room Rust drawing on top of [`crate::draw`]
//! ([`UiDrawList`]/[`UiRect`]/[`UiColor`]) — zero JEI/EMI code, zero
//! external textures, zero external dependencies. The look is a
//! byte-faithful *palette* match of the classic mods:
//!
//! * **Non-overlapping 3-zone geometry** — [`JeiEmiGeometry::compute`]
//!   keeps the center container zone untouched and parks a
//!   right item-list sidebar and a left bookmarks/craft-tree sidebar in
//!   the slack space, guaranteed never to intersect the container from
//!   `640×360` through `3840×2160`.
//! * **Right sidebar (JEI/EMI item list)** — auto-sized grid of classic
//!   18×18 recessed-bevel slots (dark `#373737` top/left, `#FFFFFF`
//!   bottom/right, `#8B8B8B` inner, `rgba(255,255,255,128)` hover), a
//!   `< page / total >` top pager, and a bottom row with the classic
//!   search box (double-click = yellow-border inventory-highlight
//!   search mode) plus a compact funnel filter toggle.
//! * **Left sidebar** — toggleable [`LeftPanelMode::Bookmarks`] slot
//!   grid (items pinned with `A`) and [`LeftPanelMode::CraftTree`]
//!   EMI-style live raw-material/missing-ingredient checklist.
//! * **Recipe popup card** — [`render_recipe_popup`]: vanilla light-grey
//!   `#C6C6C6` beveled card, category tab strip (Crafting 3×3, Smelting,
//!   Blasting, Smoking, Stonecutting, Smithing, Brewing, Create, JER,
//!   Ponder), `< n / N >` recipe pager, EMI "View Recipe Tree" button,
//!   exact 3×18px crafting-grid → arrow → 26×26 output + green `+`
//!   transfer (hovering `+` with missing materials red-tints the
//!   missing slots), and a furnace card with flame burn icon, fuel
//!   slot, XP and cook-time readout.
//! * **Classic purple tooltip** — [`render_item_tooltip`]: `rgba(16,0,
//!   16,240)` backdrop, top→bottom violet gradient inner border
//!   (`#5000FF` → `#28007F`), name line, optional info/tag lines,
//!   optional F3+H grey namespaced id, and the blue `#5555FF` mod-name
//!   footer — always clamped inside the viewport.
//! * **Runtime sprite binding** — every visible slot also emits an
//!   [`ItemSpriteSlot`] so `client-gpu` can blit real 1.21.1
//!   `textures/item|block/*.png` into the 16×16 inner area; the draw
//!   list carries `ItemIcon` commands as the stylized offline fallback.
//! * **Input** — `R` recipes / `U` uses / `A` bookmark for the hovered
//!   item, `Ctrl+O` hide/show, `Escape` close popup, wheel = page
//!   prev/next, double-click search = yellow highlight mode.
#![forbid(unsafe_code)]

use std::collections::{BTreeMap, BTreeSet};

use crate::draw::{first_px, text_width, UiColor, UiDrawList, UiRect};

// ---------------------------------------------------------------------------
// Classic JEI/EMI / vanilla palette
// ---------------------------------------------------------------------------

/// Classic 18×18 item slot (logical GUI px).
pub const JEI_SLOT: i32 = 18;
/// Recessed slot inner fill `#8B8B8B`.
pub const SLOT_INNER: UiColor = UiColor::rgb(0x8B, 0x8B, 0x8B);
/// Recessed slot dark bevel (top/left) `#373737`.
pub const BEVEL_DARK: UiColor = UiColor::rgb(0x37, 0x37, 0x37);
/// Recessed slot light bevel (bottom/right) `#FFFFFF`.
pub const BEVEL_LIGHT: UiColor = UiColor::rgb(0xFF, 0xFF, 0xFF);
/// Bright white-translucent hover highlight `rgba(255,255,255,128)`.
pub const HOVER_HIGHLIGHT: UiColor = UiColor::rgba(255, 255, 255, 128);
/// Vanilla GUI panel fill `#C6C6C6`.
pub const VANILLA_PANEL: UiColor = UiColor::rgb(0xC6, 0xC6, 0xC6);
/// Raised-panel highlight edge `#FFFFFF`.
pub const PANEL_LIGHT: UiColor = UiColor::rgb(0xFF, 0xFF, 0xFF);
/// Raised-panel shadow edge `#555555`.
pub const PANEL_DARK: UiColor = UiColor::rgb(0x55, 0x55, 0x55);
/// Text drawn on the vanilla panel `#404040`.
pub const PANEL_TEXT: UiColor = UiColor::rgb(0x40, 0x40, 0x40);
/// Tooltip backdrop `rgba(16,0,16,240)`.
pub const TOOLTIP_BG: UiColor = UiColor::rgba(16, 0, 16, 240);
/// Tooltip gradient border, top edge `#5000FF`.
pub const TOOLTIP_EDGE_TOP: UiColor = UiColor::rgb(0x50, 0x00, 0xFF);
/// Tooltip gradient border, bottom edge `#28007F`.
pub const TOOLTIP_EDGE_BOTTOM: UiColor = UiColor::rgb(0x28, 0x00, 0x7F);
/// Blue mod-name footer line `#5555FF`.
pub const MOD_NAME_BLUE: UiColor = UiColor::rgb(0x55, 0x55, 0xFF);
/// F3+H advanced namespaced-id grey.
pub const ID_GREY: UiColor = UiColor::rgb(0xAA, 0xAA, 0xAA);
/// Generic white tooltip/search text.
pub const TEXT_WHITE: UiColor = UiColor::rgb(0xFF, 0xFF, 0xFF);
/// Dim secondary tooltip line.
pub const TEXT_DIM: UiColor = UiColor::rgb(0xA0, 0xA0, 0xA0);
/// Yellow search-box border in inventory-highlight mode.
pub const SEARCH_YELLOW: UiColor = UiColor::rgb(0xFF, 0xFF, 0x00);
/// `+` transfer button tint when the recipe is craftable right now.
pub const CRAFTABLE_GREEN: UiColor = UiColor::rgb(0x6A, 0xC8, 0x6A);
/// Red overlay painted on missing-ingredient slots (hovering `+`).
pub const MISSING_TINT: UiColor = UiColor::rgba(220, 40, 40, 120);
/// Search field fill (classic black edit box).
const SEARCH_BG: UiColor = UiColor::rgb(0x00, 0x00, 0x00);
/// Search field idle border.
const SEARCH_EDGE: UiColor = UiColor::rgb(0xA0, 0xA0, 0xA0);
/// Sidebar slot-grid backdrop (JEI leaves items floating; a whisper of
/// dark keeps the grid legible over bright scenes).
const GRID_TINT: UiColor = UiColor::rgba(0, 0, 0, 72);

const EDGE: i32 = 2;
const GAP: i32 = 4;
const RIGHT_PREF_W: i32 = 170; // 9 columns * 18 + padding
const RIGHT_MIN_W: i32 = 80; // 4 columns + padding
const LEFT_PREF_W: i32 = 98; // 5 columns + padding
const LEFT_MIN_W: i32 = 62; // 3 columns + padding
const POPUP_W: i32 = 200;
const POPUP_H: i32 = 104;
const POPUP_TAB_W: i32 = 20;
const POPUP_TAB_H: i32 = 12;
const DOUBLE_CLICK_MS: u64 = 400;

// ---------------------------------------------------------------------------
// Data model (host-supplied; the overlay itself stays zero-dependency)
// ---------------------------------------------------------------------------

/// One catalog item in the right sidebar grid.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct OverlayItem {
    /// Namespaced item id (`"minecraft:diamond_pickaxe"`).
    pub id: String,
    /// Display name — tooltip line 1 and search key.
    pub display_name: String,
    /// Owning mod display name — blue tooltip footer (`"Minecraft"`).
    pub mod_name: String,
    /// Rarity color for the name line (`None` = white).
    pub rarity_color: Option<UiColor>,
    /// Item tags — `#tag` search tokens and a dim tooltip line.
    pub tags: Vec<String>,
    /// Optional extra tooltip lines (recipe/count info).
    pub info_lines: Vec<String>,
}

impl OverlayItem {
    pub fn new(id: &str, display_name: &str, mod_name: &str) -> Self {
        Self {
            id: id.to_owned(),
            display_name: display_name.to_owned(),
            mod_name: mod_name.to_owned(),
            rarity_color: None,
            tags: Vec::new(),
            info_lines: Vec::new(),
        }
    }
}

/// A counted item reference inside a recipe card slot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OverlaySlot {
    pub item_id: String,
    pub count: u32,
}

impl OverlaySlot {
    pub fn new(item_id: &str, count: u32) -> Self {
        Self {
            item_id: item_id.to_owned(),
            count: count.max(1),
        }
    }
}

/// Recipe-popup category tabs (JEI/EMI top strip).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PopupTab {
    /// 3×3 crafting grid → arrow → 26×26 output.
    #[default]
    Crafting,
    /// Furnace card (input + flame + fuel → output, XP/cook-time).
    Smelting,
    Blasting,
    Smoking,
    Stonecutting,
    Smithing,
    Brewing,
    Create,
    Jer,
    Ponder,
}

impl PopupTab {
    pub const ALL: [PopupTab; 10] = [
        Self::Crafting,
        Self::Smelting,
        Self::Blasting,
        Self::Smoking,
        Self::Stonecutting,
        Self::Smithing,
        Self::Brewing,
        Self::Create,
        Self::Jer,
        Self::Ponder,
    ];

    /// 2-glyph tab label (tab strip is only 20px wide).
    pub fn label(self) -> &'static str {
        match self {
            Self::Crafting => "Cr",
            Self::Smelting => "Sm",
            Self::Blasting => "Bl",
            Self::Smoking => "Sk",
            Self::Stonecutting => "Ct",
            Self::Smithing => "Si",
            Self::Brewing => "Br",
            Self::Create => "Ce",
            Self::Jer => "JE",
            Self::Ponder => "Po",
        }
    }

    /// Long title (card footer).
    pub fn title(self) -> &'static str {
        match self {
            Self::Crafting => "Crafting",
            Self::Smelting => "Smelting",
            Self::Blasting => "Blasting",
            Self::Smoking => "Smoking",
            Self::Stonecutting => "Stonecutting",
            Self::Smithing => "Smithing",
            Self::Brewing => "Brewing",
            Self::Create => "Create",
            Self::Jer => "JER",
            Self::Ponder => "Ponder",
        }
    }

    /// Furnace-family categories share the same card layout.
    pub fn is_furnace(self) -> bool {
        matches!(self, Self::Smelting | Self::Blasting | Self::Smoking)
    }
}

/// One recipe record a popup card can render.
#[derive(Clone, Debug, PartialEq)]
pub struct OverlayRecipe {
    pub id: String,
    pub tab: PopupTab,
    /// Crafting categories: row-major 3×3 cells (`None` = empty cell).
    /// Other categories: linear input list (`Some` entries only).
    pub inputs: Vec<Option<OverlaySlot>>,
    pub output: OverlaySlot,
    /// Smelting-family experience yield.
    pub xp: f32,
    /// Smelting-family processing time in ticks.
    pub cook_ticks: u32,
}

impl OverlayRecipe {
    pub fn new(id: &str, tab: PopupTab, output: OverlaySlot) -> Self {
        Self {
            id: id.to_owned(),
            tab,
            inputs: Vec::new(),
            output,
            xp: 0.0,
            cook_ticks: 0,
        }
    }

    /// Aggregated per-item requirement (`item_id -> total count`).
    pub fn required(&self) -> BTreeMap<String, u64> {
        let mut out = BTreeMap::new();
        for slot in self.inputs.iter().flatten() {
            *out.entry(slot.item_id.clone()).or_insert(0u64) += u64::from(slot.count);
        }
        out
    }

    /// `true` when `item` is consumed by any input slot.
    pub fn consumes(&self, item: &str) -> bool {
        self.inputs
            .iter()
            .flatten()
            .any(|s| s.item_id == item)
    }
}

/// Recipes-vs-uses popup mode (`R` / `U`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PopupMode {
    /// "Recipes producing the hovered item" (`R`).
    #[default]
    Recipes,
    /// "Recipes consuming the hovered item" (`U`).
    Uses,
}

/// Open recipe-popup state.
#[derive(Clone, Debug, PartialEq)]
pub struct RecipePopup {
    /// Item the popup was opened for.
    pub item_id: String,
    pub mode: PopupMode,
    pub tab: PopupTab,
    /// Page inside the current tab's recipe list (0-based).
    pub page: usize,
}

/// Left sidebar content selector.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum LeftPanelMode {
    /// `A`-pinned items in an 18×18 slot grid.
    #[default]
    Bookmarks,
    /// EMI-style pinned craft checklist (raw materials vs inventory).
    CraftTree,
}

/// One raw-material row of a pinned craft tree.
#[derive(Clone, Debug, PartialEq)]
pub struct PinnedMaterial {
    pub item_id: String,
    pub display_name: String,
    /// Total units the pinned recipe consumes per craft.
    pub need: u64,
}

/// EMI-style pinned recipe: target + flat material checklist. `have`
/// is re-read from [`JeiEmiOverlayState::inventory`] every frame, so the
/// checklist is live.
#[derive(Clone, Debug, PartialEq)]
pub struct PinnedTree {
    pub target_id: String,
    pub target_name: String,
    pub target_count: u64,
    pub materials: Vec<PinnedMaterial>,
}

// ---------------------------------------------------------------------------
// Frame products: hit regions, sprite slots, actions
// ---------------------------------------------------------------------------

/// Parallel runtime-sprite binding emitted for every visible item slot —
/// `client-gpu` blits the real 1.21.1 `textures/item|block/*.png` into
/// `rect` (the 16×16 inner area in physical px); [`UiDrawList`]
/// `ItemIcon` commands provide the stylized offline fallback.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ItemSpriteSlot {
    /// Physical-pixel destination rect for the runtime texture.
    pub rect: UiRect,
    pub item_id: String,
    /// Stack count to badge (`None` = no badge).
    pub count: Option<u32>,
    /// `true` while the slot is red-tinted as a missing ingredient
    /// (host may draw the sprite dimmed/desaturated).
    pub missing_overlay: bool,
}

/// Every interactive element of the overlay.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OverlayWidgetId {
    /// A right-grid item slot; payload = index into the *filtered* list.
    ItemSlot(usize),
    /// A bookmarks-grid slot; payload = index into `bookmarks`.
    BookmarkSlot(usize),
    /// Right sidebar `<` / `>` pager.
    PagePrev,
    PageNext,
    /// Bottom search input box.
    SearchBox,
    /// Bottom funnel filter/config toggle (craftable-only).
    FilterToggle,
    /// Left sidebar mode tabs.
    LeftTabBookmarks,
    LeftTabTree,
    /// A craft-tree material row; payload = `materials` index.
    TreeMaterial(usize),
    /// A popup category tab; payload = index into [`PopupTab::ALL`].
    PopupTab(usize),
    /// Popup `<` / `>` recipe pager.
    PopupPrev,
    PopupNext,
    /// Popup `+` transfer button (JEI "fill the crafting grid").
    PopupTransfer,
    /// Popup "View Recipe Tree" button.
    PopupTree,
    /// An ingredient slot inside the popup card; payload = input index.
    PopupSlot(usize),
    /// The popup 26×26 output slot.
    PopupOutput,
}

/// A clickable rectangle emitted alongside the draw list (physical px).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OverlayHitRegion {
    pub rect: UiRect,
    pub id: OverlayWidgetId,
}

impl OverlayHitRegion {
    /// Point containment test (right/bottom exclusive).
    pub const fn contains(self, x: i32, y: i32) -> bool {
        self.rect.contains(x, y)
    }
}

/// What the embedding application must perform after an interaction.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OverlayAction {
    /// `+` transfer — host fills the player's crafting grid.
    TransferRecipe {
        recipe_id: String,
        /// `true` when every ingredient is satisfied by `inventory`.
        craftable: bool,
    },
}

/// Device-agnostic overlay input for [`JeiEmiOverlayState::handle_key`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OverlayKey {
    /// A printable key press (`R`, `U`, `A`, digits, ...).
    Char(char),
    /// A Ctrl+letter chord (`CtrlChar('o')` = Ctrl+O).
    CtrlChar(char),
    Escape,
    Backspace,
    Enter,
    /// Mouse wheel up / down — page prev/next.
    WheelUp,
    WheelDown,
}

/// One rendered overlay frame.
#[derive(Clone, Debug, Default)]
pub struct OverlayFrame {
    pub list: UiDrawList,
    pub hits: Vec<OverlayHitRegion>,
    /// Parallel sprite bindings — one per visible item slot.
    pub sprites: Vec<ItemSpriteSlot>,
    /// Item ids matching the current search while the yellow
    /// inventory-highlight mode is armed — the host tints every
    /// container slot holding one of these.
    pub search_matches: Vec<String>,
}

// ---------------------------------------------------------------------------
// Non-overlapping 3-zone geometry
// ---------------------------------------------------------------------------

/// Static layout of the overlay around `container_rect`, in logical px
/// (physical px = logical × [`scale`](Self::scale)).
#[derive(Clone, Debug)]
pub struct JeiEmiGeometry {
    /// Effective GUI scale (`>= 1`); `gui_scale = 0` = auto.
    pub scale: i32,
    /// Logical viewport size.
    pub logical_w: i32,
    pub logical_h: i32,
    /// Container rect *covered* in logical px — `phys(container)` is a
    /// superset of the physical `container_rect` passed to
    /// [`compute`](Self::compute), so sidebar placement against it can
    /// never overlap the real container.
    pub container: UiRect,
    /// Right item-list sidebar (`None` when the viewport is too narrow).
    pub right: Option<UiRect>,
    /// Left bookmarks/craft-tree sidebar (`None` = hidden).
    pub left: Option<UiRect>,
    /// Right grid slot area (`right` zones only).
    pub grid: UiRect,
    pub cols: usize,
    pub rows: usize,
    /// Right pager buttons + centered label strip.
    pub page_prev: UiRect,
    pub page_next: UiRect,
    pub page_label: UiRect,
    /// Bottom row: centered search box + funnel filter toggle.
    pub search_box: UiRect,
    pub filter_button: UiRect,
    /// Left sidebar mode tabs + body.
    pub tab_bookmarks: UiRect,
    pub tab_tree: UiRect,
    pub left_body: UiRect,
    pub left_cols: usize,
    pub left_rows: usize,
    /// Recipe popup card (centered over the container).
    pub popup_card: UiRect,
}

impl JeiEmiGeometry {
    /// Lay the sidebars into the slack left/right of `container_rect`
    /// (physical px). `gui_scale = 0` picks `height/270` clamped `1..=4`
    /// like the rest of the UI; an explicit scale is clamped `1..=4`.
    /// Sidebars are `None` — never overlapped — when there is no room.
    pub fn compute(
        viewport_w: u32,
        viewport_h: u32,
        container_rect: UiRect,
        gui_scale: u32,
    ) -> Self {
        let raw = if gui_scale == 0 {
            (viewport_h.min(i32::MAX as u32) / 270).clamp(1, 4)
        } else {
            gui_scale.clamp(1, 4)
        };
        let s = raw.max(1) as i32;
        let lw = ((viewport_w.min(i32::MAX as u32)) as i32 / s).max(1);
        let lh = ((viewport_h.min(i32::MAX as u32)) as i32 / s).max(1);
        // Covering logical rect for the physical container: floor the
        // origin, ceil the far edges — phys(container) ⊇ container_rect.
        let floor = |v: i32| v.div_euclid(s);
        let ceil = |v: i32| -((-v).div_euclid(s));
        let cx = floor(container_rect.x);
        let cy = floor(container_rect.y);
        let cw = (ceil(container_rect.right()) - cx).max(0);
        let ch = (ceil(container_rect.bottom()) - cy).max(0);
        let container = UiRect::new(cx, cy, cw as u32, ch as u32);

        let mut g = Self {
            scale: s,
            logical_w: lw,
            logical_h: lh,
            container,
            right: None,
            left: None,
            grid: UiRect::default(),
            cols: 0,
            rows: 0,
            page_prev: UiRect::default(),
            page_next: UiRect::default(),
            page_label: UiRect::default(),
            search_box: UiRect::default(),
            filter_button: UiRect::default(),
            tab_bookmarks: UiRect::default(),
            tab_tree: UiRect::default(),
            left_body: UiRect::default(),
            left_cols: 0,
            left_rows: 0,
            popup_card: UiRect::default(),
        };

        // Right sidebar: full height, hard against the slack strip.
        let ravail = lw - container.right() - GAP - EDGE;
        if ravail >= RIGHT_MIN_W {
            let rw = RIGHT_PREF_W.min(ravail);
            let r = UiRect::new(
                container.right() + GAP,
                EDGE,
                rw as u32,
                (lh - 2 * EDGE).max(0) as u32,
            );
            g.right = Some(r);
            let top = UiRect::new(r.x + 3, r.y + 3, r.w - 6, 11);
            g.page_prev = UiRect::new(top.x, top.y, 10, 10);
            g.page_next = UiRect::new(top.right() - 10, top.y, 10, 10);
            g.page_label = UiRect::new(top.x + 11, top.y, top.w - 22, 10);
            let bot_y = r.bottom() - 15;
            g.filter_button = UiRect::new(r.right() - 15, bot_y, 12, 12);
            // Search box centered in the space left of the filter button.
            let zone = r.w as i32 - 6 - 16;
            let sb_w = zone.clamp(18, 92);
            let sb_x = r.x + 3 + (zone - sb_w) / 2;
            g.search_box = UiRect::new(sb_x, bot_y, sb_w as u32, 12);
            g.grid = UiRect::new(
                r.x + 4,
                r.y + 17,
                r.w - 8,
                (r.h as i32 - 17 - 19).max(0) as u32,
            );
            g.cols = ((g.grid.w as i32) / JEI_SLOT).max(1) as usize;
            g.rows = ((g.grid.h as i32) / JEI_SLOT).max(1) as usize;
        }

        // Left sidebar: mirror image.
        let lavail = container.x - GAP - EDGE;
        if lavail >= LEFT_MIN_W {
            let lwid = LEFT_PREF_W.min(lavail);
            let l = UiRect::new(
                container.x - GAP - lwid,
                EDGE,
                lwid as u32,
                (lh - 2 * EDGE).max(0) as u32,
            );
            g.left = Some(l);
            g.tab_bookmarks = UiRect::new(l.x + 3, l.y + 3, 14, 11);
            g.tab_tree = UiRect::new(l.x + 19, l.y + 3, 14, 11);
            g.left_body = UiRect::new(
                l.x + 2,
                l.y + 17,
                l.w - 4,
                (l.h as i32 - 19).max(0) as u32,
            );
            g.left_cols = ((g.left_body.w as i32) / JEI_SLOT).max(1) as usize;
            g.left_rows = ((g.left_body.h as i32) / JEI_SLOT).max(1) as usize;
        }

        // Popup card centered on the container, clamped to the viewport.
        // Min-y leaves room for the floating tab strip above the card.
        let pw = POPUP_W.min(lw - 4).max(40);
        let ph = POPUP_H.min(lh - 4).max(40);
        let px = (container.x + (container.w as i32 - pw) / 2)
            .clamp(2, (lw - pw - 2).max(2));
        let py = (container.y + (container.h as i32 - ph) / 2)
            .clamp(POPUP_TAB_H + EDGE, (lh - ph - 2).max(POPUP_TAB_H + EDGE));
        g.popup_card = UiRect::new(px, py, pw as u32, ph as u32);
        g
    }

    /// Logical → physical pixel rect at this geometry's scale.
    pub fn phys(&self, r: UiRect) -> UiRect {
        UiRect::new(
            r.x * self.scale,
            r.y * self.scale,
            (r.w as i32 * self.scale).max(0) as u32,
            (r.h as i32 * self.scale).max(0) as u32,
        )
    }

    /// Right-grid slot rect for the `k`-th slot on the page (row-major).
    pub fn slot_rect(&self, k: usize) -> UiRect {
        UiRect::new(
            self.grid.x + (k % self.cols.max(1)) as i32 * JEI_SLOT,
            self.grid.y + (k / self.cols.max(1)) as i32 * JEI_SLOT,
            JEI_SLOT as u32,
            JEI_SLOT as u32,
        )
    }

    /// Left-grid slot rect for the `k`-th bookmark (row-major).
    pub fn left_slot_rect(&self, k: usize) -> UiRect {
        UiRect::new(
            self.left_body.x + (k % self.left_cols.max(1)) as i32 * JEI_SLOT,
            self.left_body.y + (k / self.left_cols.max(1)) as i32 * JEI_SLOT,
            JEI_SLOT as u32,
            JEI_SLOT as u32,
        )
    }
}

// ---------------------------------------------------------------------------
// Drawing helpers
// ---------------------------------------------------------------------------

/// Logical-px emitter — every primitive multiplies by `s` on emit so
/// visuals and hit regions never diverge (same contract as the
/// recipe_browser `Lay`).
struct Canvas<'a> {
    list: &'a mut UiDrawList,
    s: i32,
}

impl<'a> Canvas<'a> {
    fn phys(&self, r: UiRect) -> UiRect {
        UiRect::new(
            r.x * self.s,
            r.y * self.s,
            (r.w as i32 * self.s).max(0) as u32,
            (r.h as i32 * self.s).max(0) as u32,
        )
    }

    fn fill(&mut self, x: i32, y: i32, w: i32, h: i32, color: UiColor) {
        self.list
            .fill_rect(self.phys(UiRect::new(x, y, w.max(0) as u32, h.max(0) as u32)), color);
    }

    fn fill_rect(&mut self, r: UiRect, color: UiColor) {
        self.list.fill_rect(self.phys(r), color);
    }

    fn border(&mut self, r: UiRect, color: UiColor) {
        self.list.border_rect(self.phys(r), color, self.s.max(1) as u32);
    }

    fn text(&mut self, x: i32, y: i32, color: UiColor, text: &str) {
        self.list
            .text_str(x * self.s, y * self.s, self.s.max(1) as u32, color, text);
    }

    /// Pixel-fitted text inside `max_px` logical px (elides with `..`).
    fn text_fit(&mut self, x: i32, y: i32, color: UiColor, text: &str, max_px: usize) {
        let fitted = fit_px(text, max_px);
        self.text(x, y, color, &fitted);
    }

    /// Centered text inside `r` (elided to the rect first).
    fn ctext(&mut self, r: UiRect, color: UiColor, text: &str) {
        let budget = ((r.w as i32 - 4) / 8).max(0) as usize * 8;
        let fitted = fit_px(text, budget);
        let w = text_width(&fitted, 1) as i32;
        let tx = r.x + ((r.w as i32 - w) / 2).max(0);
        let ty = r.y + ((r.h as i32 - 8) / 2).max(0);
        self.text(tx, ty, color, &fitted);
    }

    /// Item icon inside logical `r` (runtime atlas or swatch fallback).
    fn icon(&mut self, r: UiRect, item_id: &str) {
        self.list.item_icon_str(self.phys(r), item_id);
    }
}

/// Elide `text` to `max_px` scale-1 px of rendered advance (`..` tail).
fn fit_px(text: &str, max_px: usize) -> String {
    if text_width(text, 1) as usize <= max_px {
        return text.to_owned();
    }
    if max_px <= 16 {
        return ".".repeat(max_px / 8);
    }
    let mut out = first_px(text, max_px - 16).to_owned();
    out.push_str("..");
    out
}

/// Classic 18×18 recessed-bevel item slot: `#373737` top/left,
/// `#FFFFFF` bottom/right, `#8B8B8B` inner.
fn bevel_slot(cv: &mut Canvas, r: UiRect) {
    cv.fill_rect(r, SLOT_INNER);
    cv.fill(r.x, r.y, r.w as i32, 1, BEVEL_DARK);
    cv.fill(r.x, r.y, 1, r.h as i32, BEVEL_DARK);
    cv.fill(r.x, r.bottom() - 1, r.w as i32, 1, BEVEL_LIGHT);
    cv.fill(r.right() - 1, r.y, 1, r.h as i32, BEVEL_LIGHT);
}

/// Vanilla raised `#C6C6C6` panel/button (`#FFFFFF` top/left, `#555555`
/// bottom/right).
fn raised_panel(cv: &mut Canvas, r: UiRect) {
    cv.fill_rect(r, VANILLA_PANEL);
    cv.fill(r.x, r.y, r.w as i32, 1, PANEL_LIGHT);
    cv.fill(r.x, r.y, 1, r.h as i32, PANEL_LIGHT);
    cv.fill(r.x, r.bottom() - 1, r.w as i32, 1, PANEL_DARK);
    cv.fill(r.right() - 1, r.y, 1, r.h as i32, PANEL_DARK);
}

/// Small vanilla button: raised bevel + centered dark label.
fn mini_button(cv: &mut Canvas, r: UiRect, label: &str, hovered: bool) {
    raised_panel(cv, r);
    if hovered {
        cv.border(r, PANEL_LIGHT);
    }
    cv.ctext(r, PANEL_TEXT, label);
}

/// JEI right-arrow indicator (shaft + tapering head).
fn draw_arrow(cv: &mut Canvas, x: i32, y: i32, w: i32, h: i32, color: UiColor) {
    let mid = h / 2;
    cv.fill(x, y + mid - 1, (w - 5).max(1), 2, color);
    for i in 0..5 {
        let hh = (h - 2 * i).max(2);
        cv.fill(x + w - 5 + i, y + (h - hh) / 2, 1, hh, color);
    }
}

/// Tiny bitmap painter — each row char maps through `colors` (a `.` is
/// transparent). Used for the furnace flame icon.
fn blit(cv: &mut Canvas, x: i32, y: i32, rows: &[&str], colors: &[(char, UiColor)]) {
    for (dy, row) in rows.iter().enumerate() {
        for (dx, ch) in row.chars().enumerate() {
            if let Some(c) = colors.iter().find(|(k, _)| *k == ch).map(|(_, c)| *c) {
                cv.fill(x + dx as i32, y + dy as i32, 1, 1, c);
            }
        }
    }
}

/// Clean-room 13×13 flame burn icon (red base → orange → yellow tip).
fn draw_flame(cv: &mut Canvas, x: i32, y: i32) {
    const FLAME: [&str; 13] = [
        ".....yy......",
        "....yyy......",
        "...yyyyo.....",
        "...yyoyo.....",
        "..yyooyo.....",
        "..yooyoo.r...",
        ".yoooyoorr...",
        ".yoooooorr...",
        "ryoooorrrr...",
        "rooorrrrr....",
        ".rrrrrrr.....",
        "..rrrrr......",
        "...rrr.......",
    ];
    blit(
        cv,
        x,
        y,
        &FLAME,
        &[
            ('y', UiColor::rgb(0xFF, 0xD0, 0x40)),
            ('o', UiColor::rgb(0xEE, 0x88, 0x22)),
            ('r', UiColor::rgb(0xB8, 0x30, 0x18)),
        ],
    );
}

/// Top→bottom violet gradient border (`#5000FF` → `#28007F`) in
/// physical px: top/bottom rows solid, side columns lerped per row.
fn tooltip_gradient_border(list: &mut UiDrawList, r: UiRect, s: i32) {
    let s = s.max(1);
    list.fill_rect(UiRect::new(r.x, r.y, r.w, s as u32), TOOLTIP_EDGE_TOP);
    list.fill_rect(
        UiRect::new(r.x, r.bottom() - s, r.w, s as u32),
        TOOLTIP_EDGE_BOTTOM,
    );
    let rows = ((r.h as i32 - 2 * s) / s).max(0);
    for i in 0..rows {
        let t_num = i + 1;
        let t_den = rows + 1;
        let lerp = |a: u8, b: u8| -> u8 {
            (i32::from(a) + (i32::from(b) - i32::from(a)) * t_num / t_den) as u8
        };
        let c = UiColor::rgb(
            lerp(TOOLTIP_EDGE_TOP.r, TOOLTIP_EDGE_BOTTOM.r),
            lerp(TOOLTIP_EDGE_TOP.g, TOOLTIP_EDGE_BOTTOM.g),
            lerp(TOOLTIP_EDGE_TOP.b, TOOLTIP_EDGE_BOTTOM.b),
        );
        let y = r.y + s + i * s;
        list.fill_rect(UiRect::new(r.x, y, s as u32, s as u32), c);
        list.fill_rect(UiRect::new(r.right() - s, y, s as u32, s as u32), c);
    }
}

/// Classic Minecraft purple-bordered hover tooltip, drawn in *physical*
/// px and clamped strictly inside `(viewport_w, viewport_h)`. Line
/// order: display name (rarity/white), `info_lines`, joined `#tags`,
/// namespaced id in grey (advanced/F3+H only), blue mod-name footer.
/// Returns the drawn rect so callers/tests can assert clamping.
pub fn render_item_tooltip(
    list: &mut UiDrawList,
    viewport_w: i32,
    viewport_h: i32,
    mouse: (i32, i32),
    item: &OverlayItem,
    advanced_tooltips: bool,
    scale: i32,
) -> UiRect {
    let s = scale.max(1);
    let mut lines: Vec<(String, UiColor)> = vec![(
        item.display_name.clone(),
        item.rarity_color.unwrap_or(TEXT_WHITE),
    )];
    for l in &item.info_lines {
        lines.push((l.clone(), TEXT_DIM));
    }
    if !item.tags.is_empty() {
        let tags = item
            .tags
            .iter()
            .take(3)
            .map(|t| format!("#{t}"))
            .collect::<Vec<_>>()
            .join(" ");
        let tags = if item.tags.len() > 3 {
            format!("{tags} +{}", item.tags.len() - 3)
        } else {
            tags
        };
        lines.push((tags, TEXT_DIM));
    }
    if advanced_tooltips && !item.id.is_empty() {
        lines.push((item.id.clone(), ID_GREY));
    }
    lines.push((item.mod_name.clone(), MOD_NAME_BLUE));

    let line_h = 10 * s;
    let w = lines
        .iter()
        .map(|(t, _)| text_width(t, s as u32) as i32)
        .max()
        .unwrap_or(0)
        + 8 * s;
    let h = lines.len() as i32 * line_h + 4 * s;
    let vw = viewport_w.max(0);
    let vh = viewport_h.max(0);
    let w = w.min(vw).max(0);
    let h = h.min(vh).max(0);
    let x = (mouse.0 + 8 * s).min(vw - w).max(0);
    let y = (mouse.1 + 12 * s).min(vh - h).max(0);
    let rect = UiRect::new(x, y, w as u32, h as u32);
    if w == 0 || h == 0 {
        return rect;
    }
    list.fill_rect(rect, TOOLTIP_BG);
    tooltip_gradient_border(list, rect, s);
    let mut ty = rect.y + 2 * s;
    for (text, color) in lines {
        list.text_str(rect.x + 4 * s, ty, s as u32, color, &text);
        ty += line_h;
    }
    rect
}

// ---------------------------------------------------------------------------
// Overlay state
// ---------------------------------------------------------------------------

/// Interactive state of the JEI/EMI container overlay.
#[derive(Clone, Debug)]
pub struct JeiEmiOverlayState {
    /// Host-supplied item catalog (right grid).
    pub items: Vec<OverlayItem>,
    /// Host-supplied recipes (popup cards + craftability checks).
    pub recipes: Vec<OverlayRecipe>,
    /// `A`-pinned item ids, in grid order.
    pub bookmarks: Vec<String>,
    /// Live search text.
    pub search: String,
    /// Search field owns typed characters.
    pub search_focused: bool,
    /// Yellow-border inventory-highlight search mode (double-click).
    pub highlight_search: bool,
    /// Funnel toggle: grid shows only currently-craftable items.
    pub craftable_only: bool,
    /// `Ctrl+O` master visibility.
    pub visible: bool,
    /// F3+H advanced tooltips (namespaced id line).
    pub advanced_tooltips: bool,
    pub left_mode: LeftPanelMode,
    /// Current right-grid page (0-based).
    pub page: usize,
    /// Open recipe popup card.
    pub popup: Option<RecipePopup>,
    /// EMI pinned craft checklist.
    pub pinned_tree: Option<PinnedTree>,
    /// Widget under the cursor (hover highlight).
    pub hovered: Option<OverlayWidgetId>,
    /// Last mouse position in physical px (tooltip anchor).
    pub mouse: (i32, i32),
    /// Host-supplied player inventory (`item_id -> count`) — drives
    /// craftability, missing-slot tint and the craft-tree checklist.
    pub inventory: BTreeMap<String, u64>,
    /// Last rendered geometry + hit regions — lets `handle_click` and
    /// `handle_mouse_move` hit-test without the caller re-passing them.
    last_geo: Option<JeiEmiGeometry>,
    last_hits: Vec<OverlayHitRegion>,
    last_search_click_ms: Option<u64>,
}

impl Default for JeiEmiOverlayState {
    fn default() -> Self {
        Self::new()
    }
}

impl JeiEmiOverlayState {
    pub fn new() -> Self {
        Self {
            items: Vec::new(),
            recipes: Vec::new(),
            bookmarks: Vec::new(),
            search: String::new(),
            search_focused: false,
            highlight_search: false,
            craftable_only: false,
            visible: true,
            advanced_tooltips: false,
            left_mode: LeftPanelMode::Bookmarks,
            page: 0,
            popup: None,
            pinned_tree: None,
            hovered: None,
            mouse: (0, 0),
            inventory: BTreeMap::new(),
            last_geo: None,
            last_hits: Vec::new(),
            last_search_click_ms: None,
        }
    }

    // --------------------------------------------------------------- queries

    /// Indexes into `items` surviving search + craftable filter.
    pub fn filtered_items(&self) -> Vec<usize> {
        (0..self.items.len())
            .filter(|&i| self.item_visible(&self.items[i]))
            .collect()
    }

    fn item_visible(&self, item: &OverlayItem) -> bool {
        self.item_matches(item) && (!self.craftable_only || self.is_craftable(&item.id))
    }

    /// Whitespace-AND token match; `@mod`/`#tag` prefixes pin a field.
    fn item_matches(&self, item: &OverlayItem) -> bool {
        let q = self.search.trim().to_lowercase();
        if q.is_empty() {
            return true;
        }
        q.split_whitespace().all(|tok| {
            if let Some(m) = tok.strip_prefix('@') {
                m.is_empty() || item.mod_name.to_lowercase().contains(m)
            } else if let Some(t) = tok.strip_prefix('#') {
                t.is_empty()
                    || item
                        .tags
                        .iter()
                        .any(|tag| tag.to_lowercase().contains(t))
            } else {
                item.display_name.to_lowercase().contains(tok)
                    || item.id.to_lowercase().contains(tok)
                    || item.mod_name.to_lowercase().contains(tok)
                    || item
                        .tags
                        .iter()
                        .any(|t| t.to_lowercase().contains(tok))
            }
        })
    }

    /// `true` when a recipe producing `item` is fully satisfied by
    /// [`inventory`](Self::inventory) right now.
    pub fn is_craftable(&self, item: &str) -> bool {
        self.recipes
            .iter()
            .any(|r| r.output.item_id == item && self.missing_items(r).is_empty())
    }

    /// Item ids whose aggregated recipe requirement exceeds inventory.
    pub fn missing_items(&self, r: &OverlayRecipe) -> BTreeSet<String> {
        r.required()
            .iter()
            .filter(|(id, need)| self.inventory.get(*id).copied().unwrap_or(0) < **need)
            .map(|(id, _)| id.clone())
            .collect()
    }

    /// Recipes shown by the popup for `item` on `tab` in `mode`.
    pub fn recipes_for(&self, item: &str, tab: PopupTab, mode: PopupMode) -> Vec<&OverlayRecipe> {
        self.recipes
            .iter()
            .filter(|r| r.tab == tab)
            .filter(|r| match mode {
                PopupMode::Recipes => r.output.item_id == item,
                PopupMode::Uses => r.consumes(item),
            })
            .collect()
    }

    fn popup_recipes(&self) -> Vec<&OverlayRecipe> {
        let Some(pop) = &self.popup else {
            return Vec::new();
        };
        self.recipes_for(&pop.item_id, pop.tab, pop.mode)
    }

    /// Total grid pages for the filtered list at this geometry.
    pub fn total_pages(&self, geo: &JeiEmiGeometry) -> usize {
        let per = (geo.cols * geo.rows).max(1);
        self.filtered_items().len().div_ceil(per).max(1)
    }

    fn display_name(&self, item_id: &str) -> String {
        self.items
            .iter()
            .find(|i| i.id == item_id)
            .map(|i| i.display_name.clone())
            .unwrap_or_else(|| humanize_id(item_id))
    }

    fn item(&self, item_id: &str) -> Option<&OverlayItem> {
        self.items.iter().find(|i| i.id == item_id)
    }

    /// Item id under the cursor (grid, bookmark or popup slot).
    fn hovered_item_id(&self) -> Option<String> {
        match self.hovered? {
            OverlayWidgetId::ItemSlot(vi) => self
                .filtered_items()
                .get(vi)
                .and_then(|&i| self.items.get(i))
                .map(|i| i.id.clone()),
            OverlayWidgetId::BookmarkSlot(i) => self.bookmarks.get(i).cloned(),
            OverlayWidgetId::TreeMaterial(i) => self
                .pinned_tree
                .as_ref()
                .and_then(|t| t.materials.get(i))
                .map(|m| m.item_id.clone()),
            OverlayWidgetId::PopupSlot(n) => self
                .popup_recipes()
                .get(self.popup.as_ref().map_or(0, |p| p.page))
                .and_then(|r| r.inputs.get(n))
                .and_then(|s| s.as_ref())
                .map(|s| s.item_id.clone()),
            OverlayWidgetId::PopupOutput => self
                .popup_recipes()
                .get(self.popup.as_ref().map_or(0, |p| p.page))
                .map(|r| r.output.item_id.clone()),
            _ => None,
        }
    }

    fn widget_at(&self, x: i32, y: i32) -> Option<OverlayWidgetId> {
        // Reverse: popup regions are appended after the sidebars and win.
        self.last_hits
            .iter()
            .rev()
            .find(|h| h.contains(x, y))
            .map(|h| h.id)
    }

    // ------------------------------------------------------------- mutations

    fn open_popup(&mut self, mode: PopupMode) {
        let Some(item) = self.hovered_item_id() else {
            return;
        };
        let Some(&tab) = PopupTab::ALL
            .iter()
            .find(|&&t| !self.recipes_for(&item, t, mode).is_empty())
        else {
            return;
        };
        self.popup = Some(RecipePopup {
            item_id: item,
            mode,
            tab,
            page: 0,
        });
    }

    /// Recipes popup for an explicit item id (bookmark/tree/popup slot).
    fn open_popup_for(&mut self, item: &str, mode: PopupMode) {
        let Some(&tab) = PopupTab::ALL
            .iter()
            .find(|&&t| !self.recipes_for(item, t, mode).is_empty())
        else {
            return;
        };
        self.popup = Some(RecipePopup {
            item_id: item.to_owned(),
            mode,
            tab,
            page: 0,
        });
    }

    /// `A`: pin/unpin the hovered item in the bookmarks grid.
    pub fn toggle_bookmark(&mut self, item: &str) {
        if let Some(pos) = self.bookmarks.iter().position(|b| b == item) {
            self.bookmarks.remove(pos);
        } else {
            self.bookmarks.push(item.to_owned());
        }
    }

    /// Turn the right-grid page by `dir` (clamped).
    pub fn turn_page(&mut self, dir: i64) {
        let Some(geo) = &self.last_geo else {
            return;
        };
        let total = self.total_pages(geo) as i64;
        self.page = (self.page as i64 + dir).clamp(0, total - 1) as usize;
    }

    /// Pin the currently-displayed recipe into the craft-tree panel.
    fn pin_current_recipe(&mut self) {
        let Some((target_id, target_count, required)) = self
            .popup_recipes()
            .get(self.popup.as_ref().map_or(0, |p| p.page))
            .map(|r| (r.output.item_id.clone(), u64::from(r.output.count), r.required()))
        else {
            return;
        };
        let materials = required
            .into_iter()
            .map(|(id, need)| PinnedMaterial {
                display_name: self.display_name(&id),
                item_id: id,
                need,
            })
            .collect();
        self.pinned_tree = Some(PinnedTree {
            target_name: self.display_name(&target_id),
            target_id,
            target_count,
            materials,
        });
        self.left_mode = LeftPanelMode::CraftTree;
    }

    // ---------------------------------------------------------------- input

    /// Key/chord input. Returns host actions (e.g. `+` transfer).
    pub fn handle_key(&mut self, key: OverlayKey) -> Vec<OverlayAction> {
        match key {
            OverlayKey::Escape => {
                if self.popup.is_some() {
                    self.popup = None;
                } else if self.search_focused {
                    self.search_focused = false;
                }
            }
            OverlayKey::Backspace => {
                if self.search_focused {
                    self.search.pop();
                    self.page = 0;
                    if self.search.is_empty() {
                        self.highlight_search = false;
                    }
                }
            }
            OverlayKey::Enter => {
                if self.search_focused {
                    self.search_focused = false;
                } else {
                    self.open_popup(PopupMode::Recipes);
                }
            }
            OverlayKey::WheelUp => self.turn_page(-1),
            OverlayKey::WheelDown => self.turn_page(1),
            OverlayKey::CtrlChar(c) => {
                if c.eq_ignore_ascii_case(&'o') {
                    self.visible = !self.visible;
                }
            }
            OverlayKey::Char(c) => {
                if self.search_focused && (' '..='~').contains(&c) {
                    self.search.push(c);
                    self.page = 0;
                } else {
                    match c {
                        'r' | 'R' => self.open_popup(PopupMode::Recipes),
                        'u' | 'U' => self.open_popup(PopupMode::Uses),
                        'a' | 'A' => {
                            if let Some(item) = self.hovered_item_id() {
                                self.toggle_bookmark(&item);
                            }
                        }
                        c if (' '..='~').contains(&c) => {
                            // JEI-style: any other keystroke jumps into
                            // the search box.
                            self.search_focused = true;
                            self.search.push(c);
                            self.page = 0;
                        }
                        _ => {}
                    }
                }
            }
        }
        Vec::new()
    }

    /// Cursor move — updates the hover widget and tooltip anchor.
    /// `x`/`y` are physical px against the last rendered frame.
    pub fn handle_mouse_move(&mut self, x: i32, y: i32) {
        self.mouse = (x, y);
        self.hovered = self.widget_at(x, y);
    }

    /// Primary click at `(x, y)` physical px; `now_ms` drives the
    /// search-box double-click detection (yellow highlight mode).
    pub fn handle_click(&mut self, x: i32, y: i32, now_ms: u64) -> Vec<OverlayAction> {
        match self.widget_at(x, y) {
            Some(id) => self.activate(id, now_ms),
            None => {
                if self.popup.is_some() {
                    self.popup = None;
                } else {
                    self.search_focused = false;
                }
                Vec::new()
            }
        }
    }

    /// Activate a hit region (host may also feed these from its own
    /// hit test — same path as `handle_click`).
    pub fn activate(&mut self, id: OverlayWidgetId, now_ms: u64) -> Vec<OverlayAction> {
        match id {
            OverlayWidgetId::ItemSlot(vi) => {
                let item = self
                    .filtered_items()
                    .get(vi)
                    .and_then(|&i| self.items.get(i))
                    .map(|i| i.id.clone());
                if let Some(item) = item {
                    self.open_popup_for(&item, PopupMode::Recipes);
                }
            }
            OverlayWidgetId::BookmarkSlot(i) => {
                if let Some(item) = self.bookmarks.get(i).cloned() {
                    self.open_popup_for(&item, PopupMode::Recipes);
                }
            }
            OverlayWidgetId::PagePrev => self.turn_page(-1),
            OverlayWidgetId::PageNext => self.turn_page(1),
            OverlayWidgetId::SearchBox => {
                self.search_focused = true;
                let dbl = self
                    .last_search_click_ms
                    .is_some_and(|t| now_ms.saturating_sub(t) <= DOUBLE_CLICK_MS);
                self.last_search_click_ms = Some(now_ms);
                if dbl {
                    self.highlight_search = !self.highlight_search;
                    self.last_search_click_ms = None;
                }
            }
            OverlayWidgetId::FilterToggle => {
                self.craftable_only = !self.craftable_only;
                self.page = 0;
            }
            OverlayWidgetId::LeftTabBookmarks => self.left_mode = LeftPanelMode::Bookmarks,
            OverlayWidgetId::LeftTabTree => self.left_mode = LeftPanelMode::CraftTree,
            OverlayWidgetId::TreeMaterial(i) => {
                let item = self
                    .pinned_tree
                    .as_ref()
                    .and_then(|t| t.materials.get(i))
                    .map(|m| m.item_id.clone());
                if let Some(item) = item {
                    self.open_popup_for(&item, PopupMode::Recipes);
                }
            }
            OverlayWidgetId::PopupTab(i) => {
                if let (Some(pop), Some(&tab)) =
                    (&mut self.popup, PopupTab::ALL.get(i))
                {
                    pop.tab = tab;
                    pop.page = 0;
                }
            }
            OverlayWidgetId::PopupPrev | OverlayWidgetId::PopupNext => {
                let dir = if id == OverlayWidgetId::PopupPrev {
                    -1i64
                } else {
                    1i64
                };
                let total = match &self.popup {
                    Some(pop) => self
                        .recipes_for(&pop.item_id, pop.tab, pop.mode)
                        .len()
                        .max(1) as i64,
                    None => 1,
                };
                if let Some(pop) = &mut self.popup {
                    pop.page = (pop.page as i64 + dir).clamp(0, total - 1) as usize;
                }
            }
            OverlayWidgetId::PopupTransfer => {
                if let Some(r) = self
                    .popup_recipes()
                    .get(self.popup.as_ref().map_or(0, |p| p.page))
                {
                    return vec![OverlayAction::TransferRecipe {
                        recipe_id: r.id.clone(),
                        craftable: self.missing_items(r).is_empty(),
                    }];
                }
            }
            OverlayWidgetId::PopupTree => self.pin_current_recipe(),
            OverlayWidgetId::PopupSlot(n) => {
                let item = self
                    .popup_recipes()
                    .get(self.popup.as_ref().map_or(0, |p| p.page))
                    .and_then(|r| r.inputs.get(n))
                    .and_then(|s| s.as_ref())
                    .map(|s| s.item_id.clone());
                if let Some(item) = item {
                    self.open_popup_for(&item, PopupMode::Recipes);
                }
            }
            OverlayWidgetId::PopupOutput => {
                let item = self
                    .popup_recipes()
                    .get(self.popup.as_ref().map_or(0, |p| p.page))
                    .map(|r| r.output.item_id.clone());
                if let Some(item) = item {
                    self.open_popup_for(&item, PopupMode::Recipes);
                }
            }
        }
        Vec::new()
    }

    // -------------------------------------------------------------- render

    /// Build the frame: `(draw_list, hit_regions, sprite_slots,
    /// search_matches)`. All geometry is computed in logical px and
    /// multiplied by [`JeiEmiGeometry::scale`] on emit. The container
    /// zone is never touched — the sidebars park strictly outside it.
    pub fn render(
        &mut self,
        viewport_w: u32,
        viewport_h: u32,
        container_rect: UiRect,
        gui_scale: u32,
    ) -> OverlayFrame {
        let geo = JeiEmiGeometry::compute(viewport_w, viewport_h, container_rect, gui_scale);
        let mut list = UiDrawList::new();
        let mut hits = Vec::new();
        let mut sprites = Vec::new();
        if !self.visible {
            self.last_geo = Some(geo);
            self.last_hits = Vec::new();
            return OverlayFrame {
                list,
                hits,
                sprites,
                search_matches: Vec::new(),
            };
        }
        {
            let mut cv = Canvas {
                list: &mut list,
                s: geo.scale,
            };
            if geo.right.is_some() {
                self.render_right(&mut cv, &geo, &mut hits, &mut sprites);
            }
            if geo.left.is_some() {
                self.render_left(&mut cv, &geo, &mut hits, &mut sprites);
            }
        }
        // Popup card is its own layer (drawn over the center zone).
        let (plist, phits, psprites) = self.render_recipe_popup(&geo);
        list.commands.extend(plist.commands);
        hits.extend(phits);
        sprites.extend(psprites);
        // Tooltip floats above everything, in physical px.
        if let Some(item_id) = self.hovered_item_id() {
            if let Some(item) = self.item(&item_id).cloned() {
                render_item_tooltip(
                    &mut list,
                    viewport_w.min(i32::MAX as u32) as i32,
                    viewport_h.min(i32::MAX as u32) as i32,
                    self.mouse,
                    &item,
                    self.advanced_tooltips,
                    geo.scale,
                );
            }
        }
        let search_matches = if self.highlight_search && !self.search.trim().is_empty() {
            self.filtered_items()
                .into_iter()
                .map(|i| self.items[i].id.clone())
                .collect()
        } else {
            Vec::new()
        };
        let frame = OverlayFrame {
            list,
            hits,
            sprites,
            search_matches,
        };
        self.last_geo = Some(geo);
        self.last_hits = frame.hits.clone();
        frame
    }

    /// Right sidebar: top pager, 18×18 bevel slot grid, bottom
    /// search+filter row. Never draws inside `geo.container`.
    fn render_right(
        &self,
        cv: &mut Canvas,
        geo: &JeiEmiGeometry,
        hits: &mut Vec<OverlayHitRegion>,
        sprites: &mut Vec<ItemSpriteSlot>,
    ) {
        let Some(r) = geo.right else {
            return;
        };
        cv.fill_rect(r, GRID_TINT);

        // Top pager: `<` [`page/total`] `>`
        let filtered = self.filtered_items();
        let total = self.total_pages(geo);
        let page = self.page.min(total - 1);
        mini_button(cv, geo.page_prev, "<", self.hovered == Some(OverlayWidgetId::PagePrev));
        mini_button(cv, geo.page_next, ">", self.hovered == Some(OverlayWidgetId::PageNext));
        push_hit(hits, geo, OverlayWidgetId::PagePrev, geo.page_prev);
        push_hit(hits, geo, OverlayWidgetId::PageNext, geo.page_next);
        cv.ctext(geo.page_label, TEXT_WHITE, &format!("{} / {}", page + 1, total));

        // Item grid.
        let per = geo.cols * geo.rows;
        for (k, &vi) in filtered.iter().skip(page * per).take(per).enumerate() {
            let slot = geo.slot_rect(k);
            let item = &self.items[vi];
            bevel_slot(cv, slot);
            let inner = UiRect::new(slot.x + 1, slot.y + 1, 16, 16);
            cv.icon(inner, &item.id);
            sprites.push(ItemSpriteSlot {
                rect: cv.phys(inner),
                item_id: item.id.clone(),
                count: None,
                missing_overlay: false,
            });
            if self.hovered == Some(OverlayWidgetId::ItemSlot(vi)) {
                cv.fill_rect(inner, HOVER_HIGHLIGHT);
            }
            push_hit(hits, geo, OverlayWidgetId::ItemSlot(vi), slot);
        }

        // Bottom row: centered search box + funnel filter toggle.
        let sb = geo.search_box;
        cv.fill_rect(sb, SEARCH_BG);
        cv.border(
            sb,
            if self.highlight_search {
                SEARCH_YELLOW
            } else if self.search_focused {
                BEVEL_LIGHT
            } else {
                SEARCH_EDGE
            },
        );
        let budget = (sb.w as i32 - 8).max(0) as usize;
        if self.search.is_empty() && !self.search_focused {
            cv.text_fit(sb.x + 4, sb.y + 2, TEXT_DIM, "Search", budget);
        } else {
            let mut shown = self.search.clone();
            if self.search_focused {
                shown.push('_');
            }
            cv.text_fit(sb.x + 4, sb.y + 2, TEXT_WHITE, &shown, budget);
        }
        push_hit(hits, geo, OverlayWidgetId::SearchBox, sb);
        // Funnel filter/config toggle (EMI-style compact button).
        let fb = geo.filter_button;
        raised_panel(cv, fb);
        const FUNNEL: [&str; 7] = [
            "xxxxxxxxx",
            ".xxxxxxx.",
            "..xxxxx..",
            "...xxx...",
            "...xxx...",
            "...xxx...",
            "....x....",
        ];
        blit(
            cv,
            fb.x + 1,
            fb.y + 2,
            &FUNNEL,
            &[(
                'x',
                if self.craftable_only {
                    UiColor::rgb(0x30, 0xA0, 0x30)
                } else {
                    PANEL_TEXT
                },
            )],
        );
        if self.hovered == Some(OverlayWidgetId::FilterToggle) {
            cv.border(fb, PANEL_LIGHT);
        }
        push_hit(hits, geo, OverlayWidgetId::FilterToggle, fb);
    }

    /// Left sidebar: `[B][T]` mode tabs, then bookmarks grid or the
    /// pinned craft-tree checklist.
    fn render_left(
        &self,
        cv: &mut Canvas,
        geo: &JeiEmiGeometry,
        hits: &mut Vec<OverlayHitRegion>,
        sprites: &mut Vec<ItemSpriteSlot>,
    ) {
        let Some(l) = geo.left else {
            return;
        };
        cv.fill_rect(l, GRID_TINT);

        // Mode tabs — active one raised into the panel look.
        for (rect, mode, label, wid) in [
            (
                geo.tab_bookmarks,
                LeftPanelMode::Bookmarks,
                "B",
                OverlayWidgetId::LeftTabBookmarks,
            ),
            (
                geo.tab_tree,
                LeftPanelMode::CraftTree,
                "T",
                OverlayWidgetId::LeftTabTree,
            ),
        ] {
            if self.left_mode == mode {
                raised_panel(cv, rect);
            } else {
                cv.fill_rect(rect, SLOT_INNER);
                cv.border(rect, BEVEL_DARK);
            }
            cv.ctext(
                rect,
                if self.left_mode == mode {
                    PANEL_TEXT
                } else {
                    TEXT_DIM
                },
                label,
            );
            push_hit(hits, geo, wid, rect);
        }

        match self.left_mode {
            LeftPanelMode::Bookmarks => {
                if self.bookmarks.is_empty() {
                    cv.text_fit(
                        geo.left_body.x + 4,
                        geo.left_body.y + 4,
                        TEXT_DIM,
                        "(A to pin)",
                        (geo.left_body.w as i32 - 8).max(0) as usize,
                    );
                }
                let cap = geo.left_cols * geo.left_rows;
                for (i, item_id) in self.bookmarks.iter().take(cap).enumerate() {
                    let slot = geo.left_slot_rect(i);
                    bevel_slot(cv, slot);
                    let inner = UiRect::new(slot.x + 1, slot.y + 1, 16, 16);
                    cv.icon(inner, item_id);
                    sprites.push(ItemSpriteSlot {
                        rect: cv.phys(inner),
                        item_id: item_id.clone(),
                        count: None,
                        missing_overlay: false,
                    });
                    if self.hovered == Some(OverlayWidgetId::BookmarkSlot(i)) {
                        cv.fill_rect(inner, HOVER_HIGHLIGHT);
                    }
                    push_hit(hits, geo, OverlayWidgetId::BookmarkSlot(i), slot);
                }
            }
            LeftPanelMode::CraftTree => {
                let body = geo.left_body;
                match &self.pinned_tree {
                    None => cv.text_fit(
                        body.x + 4,
                        body.y + 4,
                        TEXT_DIM,
                        "(pin a recipe)",
                        (body.w as i32 - 8).max(0) as usize,
                    ),
                    Some(tree) => {
                        let header = format!(
                            "{}x {}",
                            tree.target_count,
                            tree.target_name
                        );
                        cv.text_fit(
                            body.x + 3,
                            body.y + 2,
                            TEXT_WHITE,
                            &header,
                            (body.w as i32 - 6).max(0) as usize,
                        );
                        let mut y = body.y + 13;
                        for (i, m) in tree.materials.iter().enumerate() {
                            if y + 12 > body.bottom() {
                                break;
                            }
                            let have = self.inventory.get(&m.item_id).copied().unwrap_or(0);
                            let ok = have >= m.need;
                            let row = UiRect::new(body.x + 1, y, body.w - 2, 12);
                            if self.hovered == Some(OverlayWidgetId::TreeMaterial(i)) {
                                cv.fill_rect(row, UiColor::rgba(255, 255, 255, 32));
                            }
                            cv.icon(UiRect::new(row.x + 1, row.y + 1, 10, 10), &m.item_id);
                            let line = format!("{}/{} {}", have, m.need, m.display_name);
                            cv.text_fit(
                                row.x + 13,
                                row.y + 2,
                                if ok {
                                    UiColor::rgb(0x80, 0xE0, 0x80)
                                } else {
                                    UiColor::rgb(0xF0, 0x90, 0x50)
                                },
                                &line,
                                (row.w as i32 - 15).max(0) as usize,
                            );
                            push_hit(hits, geo, OverlayWidgetId::TreeMaterial(i), row);
                            y += 12;
                        }
                    }
                }
            }
        }
    }

    /// The JEI/EMI recipe popup card layer: category tabs, `< n / N >`
    /// pager, EMI "Recipe Tree" button and the per-category diagram —
    /// 3×3 crafting grid → arrow → 26×26 output + `+` transfer, or the
    /// furnace card (input + flame + fuel → output, XP/cook readout).
    /// Rendered centered over the container zone (modal card).
    pub fn render_recipe_popup(
        &self,
        geo: &JeiEmiGeometry,
    ) -> (UiDrawList, Vec<OverlayHitRegion>, Vec<ItemSpriteSlot>) {
        let mut list = UiDrawList::new();
        let mut hits = Vec::new();
        let mut sprites = Vec::new();
        let Some(pop) = &self.popup else {
            return (list, hits, sprites);
        };
        let card = geo.popup_card;
        {
            let mut cv = Canvas {
                list: &mut list,
                s: geo.scale,
            };

            // Card panel + floating tab strip (active tab raised into it).
            raised_panel(&mut cv, card);
            for (i, tab) in PopupTab::ALL.iter().enumerate() {
                let active = pop.tab == *tab;
                let tr = UiRect::new(
                    card.x + i as i32 * POPUP_TAB_W,
                    card.y - POPUP_TAB_H,
                    POPUP_TAB_W as u32,
                    (POPUP_TAB_H + if active { 2 } else { 0 }) as u32,
                );
                if active {
                    // Raised tab merged into the card: paint panel color
                    // over the seam and bevel the other three edges.
                    cv.fill_rect(tr, VANILLA_PANEL);
                    cv.fill(tr.x, tr.y, tr.w as i32, 1, PANEL_LIGHT);
                    cv.fill(tr.x, tr.y, 1, tr.h as i32, PANEL_LIGHT);
                    cv.fill(tr.right() - 1, tr.y, 1, tr.h as i32, PANEL_DARK);
                } else {
                    cv.fill_rect(tr, SLOT_INNER);
                    cv.border(tr, BEVEL_DARK);
                }
                cv.ctext(
                    UiRect::new(tr.x, tr.y, tr.w, POPUP_TAB_H as u32),
                    if active { PANEL_TEXT } else { TEXT_DIM },
                    tab.label(),
                );
                push_hit(&mut hits, geo, OverlayWidgetId::PopupTab(i), tr);
            }

            // Header: `< n / N >` pager + "Recipe Tree" EMI button.
            let prev = UiRect::new(card.x + 4, card.y + 4, 10, 10);
            let next = UiRect::new(card.x + 88, card.y + 4, 10, 10);
            mini_button(&mut cv, prev, "<", self.hovered == Some(OverlayWidgetId::PopupPrev));
            mini_button(&mut cv, next, ">", self.hovered == Some(OverlayWidgetId::PopupNext));
            push_hit(&mut hits, geo, OverlayWidgetId::PopupPrev, prev);
            push_hit(&mut hits, geo, OverlayWidgetId::PopupNext, next);
            let recipes = self.popup_recipes();
            let n_total = recipes.len().max(1);
            let page = pop.page.min(n_total - 1);
            cv.ctext(
                UiRect::new(card.x + 15, card.y + 4, 72, 10),
                PANEL_TEXT,
                &format!("{} / {}", page + 1, n_total),
            );
            let tree_btn = UiRect::new(card.right() - 98, card.y + 3, 92, 12);
            raised_panel(&mut cv, tree_btn);
            if self.hovered == Some(OverlayWidgetId::PopupTree) {
                cv.border(tree_btn, PANEL_LIGHT);
            }
            cv.ctext(tree_btn, PANEL_TEXT, "Recipe Tree");
            push_hit(&mut hits, geo, OverlayWidgetId::PopupTree, tree_btn);

            // Card body.
            let body = UiRect::new(
                card.x + 8,
                card.y + 18,
                card.w - 16,
                (card.h as i32 - 20).max(8) as u32,
            );
            let missing: BTreeSet<String> = recipes
                .get(page)
                .map(|r| self.missing_items(r))
                .unwrap_or_default();
            let show_missing =
                self.hovered == Some(OverlayWidgetId::PopupTransfer) && !missing.is_empty();
            match recipes.get(page) {
                Some(recipe) if pop.tab == PopupTab::Crafting => {
                    self.card_crafting(
                        &mut cv, geo, &mut hits, &mut sprites, recipe, body, &missing,
                        show_missing,
                    );
                }
                Some(recipe) if pop.tab.is_furnace() => {
                    self.card_furnace(
                        &mut cv, geo, &mut hits, &mut sprites, recipe, body, &missing,
                        show_missing,
                    );
                }
                Some(recipe) => {
                    self.card_inline(
                        &mut cv, geo, &mut hits, &mut sprites, recipe, body, &missing,
                        show_missing,
                    );
                }
                None => cv.ctext(body, PANEL_TEXT, "No recipes"),
            }
            // Category caption at the card's bottom-left.
            cv.text(card.x + 6, card.bottom() - 11, PANEL_TEXT, pop.tab.title());
        }
        (list, hits, sprites)
    }

    /// One card slot: bevel + icon + sprite binding + count badge +
    /// hover/missing tint + optional hit region. `size` is 18 or 26.
    #[allow(clippy::too_many_arguments)]
    fn card_slot(
        &self,
        cv: &mut Canvas,
        geo: &JeiEmiGeometry,
        hits: &mut Vec<OverlayHitRegion>,
        sprites: &mut Vec<ItemSpriteSlot>,
        r: UiRect,
        slot: Option<&OverlaySlot>,
        wid: Option<OverlayWidgetId>,
        missing_tint: bool,
        hovered_wid: Option<OverlayWidgetId>,
    ) {
        bevel_slot(cv, r);
        let inset = (r.w as i32 - 16) / 2;
        let inner = UiRect::new(r.x + inset, r.y + inset, 16, 16);
        if let Some(s) = slot {
            if !s.item_id.is_empty() {
                cv.icon(inner, &s.item_id);
                sprites.push(ItemSpriteSlot {
                    rect: cv.phys(inner),
                    item_id: s.item_id.clone(),
                    count: (s.count > 1).then_some(s.count),
                    missing_overlay: missing_tint,
                });
                if s.count > 1 {
                    let label = s.count.to_string();
                    let tw = text_width(&label, 1) as i32;
                    cv.text(r.right() - 2 - tw, r.bottom() - 9, TEXT_WHITE, &label);
                }
            }
        }
        if missing_tint {
            cv.fill_rect(inner, MISSING_TINT);
        }
        if hovered_wid.is_some() && hovered_wid == self.hovered {
            cv.fill_rect(inner, HOVER_HIGHLIGHT);
        }
        if let Some(id) = wid {
            push_hit(hits, geo, id, r);
        }
    }

    /// 3×3 crafting card: grid → arrow → 26×26 output + `+` transfer.
    #[allow(clippy::too_many_arguments)]
    fn card_crafting(
        &self,
        cv: &mut Canvas,
        geo: &JeiEmiGeometry,
        hits: &mut Vec<OverlayHitRegion>,
        sprites: &mut Vec<ItemSpriteSlot>,
        recipe: &OverlayRecipe,
        body: UiRect,
        missing: &BTreeSet<String>,
        show_missing: bool,
    ) {
        let mid_y = body.y + 27;
        for cell in 0..9usize {
            let (row, col) = (cell / 3, cell % 3);
            let r = UiRect::new(
                body.x + col as i32 * 18,
                body.y + 2 + row as i32 * 18,
                18,
                18,
            );
            let s = recipe.inputs.get(cell).and_then(|s| s.as_ref());
            let tint = show_missing
                && s.is_some_and(|s| missing.contains(&s.item_id));
            self.card_slot(
                cv,
                geo,
                hits,
                sprites,
                r,
                s,
                Some(OverlayWidgetId::PopupSlot(cell)),
                tint,
                Some(OverlayWidgetId::PopupSlot(cell)),
            );
        }
        draw_arrow(cv, body.x + 58, mid_y - 5, 18, 10, PANEL_TEXT);
        // 26×26 output slot + `+` transfer button.
        let out_r = UiRect::new(body.x + 80, mid_y - 13, 26, 26);
        self.card_slot(
            cv,
            geo,
            hits,
            sprites,
            out_r,
            Some(&recipe.output),
            Some(OverlayWidgetId::PopupOutput),
            false,
            Some(OverlayWidgetId::PopupOutput),
        );
        let plus = UiRect::new(body.x + 112, mid_y - 6, 14, 12);
        let craftable = missing.is_empty();
        raised_panel(cv, plus);
        if craftable {
            cv.fill_rect(
                UiRect::new(plus.x + 1, plus.y + 1, plus.w - 2, plus.h - 2),
                CRAFTABLE_GREEN,
            );
            cv.fill(plus.x, plus.y, plus.w as i32, 1, PANEL_LIGHT);
            cv.fill(plus.x, plus.y, 1, plus.h as i32, PANEL_LIGHT);
            cv.fill(plus.x, plus.bottom() - 1, plus.w as i32, 1, PANEL_DARK);
            cv.fill(plus.right() - 1, plus.y, 1, plus.h as i32, PANEL_DARK);
        }
        if self.hovered == Some(OverlayWidgetId::PopupTransfer) {
            cv.border(plus, PANEL_LIGHT);
        }
        cv.ctext(plus, PANEL_TEXT, "+");
        push_hit(hits, geo, OverlayWidgetId::PopupTransfer, plus);
    }

    /// Furnace card: input + flame + fuel → arrow → output, with an
    /// XP + cook-time readout at the card's bottom-left.
    #[allow(clippy::too_many_arguments)]
    fn card_furnace(
        &self,
        cv: &mut Canvas,
        geo: &JeiEmiGeometry,
        hits: &mut Vec<OverlayHitRegion>,
        sprites: &mut Vec<ItemSpriteSlot>,
        recipe: &OverlayRecipe,
        body: UiRect,
        missing: &BTreeSet<String>,
        show_missing: bool,
    ) {
        let mid_y = body.y + 24;
        let ins: Vec<&OverlaySlot> = recipe.inputs.iter().flatten().collect();
        let input = ins.first().copied();
        let fuel = ins.get(1).copied();
        let in_tint = show_missing
            && input.is_some_and(|s| missing.contains(&s.item_id));
        let fuel_tint = show_missing
            && fuel.is_some_and(|s| missing.contains(&s.item_id));
        let in_r = UiRect::new(body.x + 16, body.y + 2, 18, 18);
        self.card_slot(
            cv,
            geo,
            hits,
            sprites,
            in_r,
            input,
            Some(OverlayWidgetId::PopupSlot(0)),
            in_tint,
            Some(OverlayWidgetId::PopupSlot(0)),
        );
        draw_flame(cv, body.x + 18, body.y + 24);
        let fuel_r = UiRect::new(body.x + 16, body.y + 42, 18, 18);
        self.card_slot(
            cv,
            geo,
            hits,
            sprites,
            fuel_r,
            fuel,
            Some(OverlayWidgetId::PopupSlot(1)),
            fuel_tint,
            Some(OverlayWidgetId::PopupSlot(1)),
        );
        draw_arrow(cv, body.x + 58, mid_y - 5, 18, 10, PANEL_TEXT);
        let out_r = UiRect::new(body.x + 80, mid_y - 13, 26, 26);
        self.card_slot(
            cv,
            geo,
            hits,
            sprites,
            out_r,
            Some(&recipe.output),
            Some(OverlayWidgetId::PopupOutput),
            false,
            Some(OverlayWidgetId::PopupOutput),
        );
        let plus = UiRect::new(body.x + 112, mid_y - 6, 14, 12);
        raised_panel(cv, plus);
        if missing.is_empty() {
            cv.fill_rect(
                UiRect::new(plus.x + 1, plus.y + 1, plus.w - 2, plus.h - 2),
                CRAFTABLE_GREEN,
            );
            cv.fill(plus.x, plus.y, plus.w as i32, 1, PANEL_LIGHT);
            cv.fill(plus.x, plus.y, 1, plus.h as i32, PANEL_LIGHT);
            cv.fill(plus.x, plus.bottom() - 1, plus.w as i32, 1, PANEL_DARK);
            cv.fill(plus.right() - 1, plus.y, 1, plus.h as i32, PANEL_DARK);
        }
        if self.hovered == Some(OverlayWidgetId::PopupTransfer) {
            cv.border(plus, PANEL_LIGHT);
        }
        cv.ctext(plus, PANEL_TEXT, "+");
        push_hit(hits, geo, OverlayWidgetId::PopupTransfer, plus);
        // XP + cook-time readout.
        let mut info = Vec::new();
        if recipe.xp > 0.0 {
            info.push(format!("{:.1} XP", recipe.xp));
        }
        if recipe.cook_ticks > 0 {
            info.push(format!("{} ticks", recipe.cook_ticks));
        }
        if !info.is_empty() {
            cv.text_fit(
                body.x + 4,
                body.bottom() - 10,
                PANEL_TEXT,
                &info.join("  "),
                (body.w as i32 - 8).max(0) as usize,
            );
        }
    }

    /// Generic inline card (stonecutting/smithing/brewing/Create/JER/
    /// Ponder): input slots in a row → arrow → 26×26 output.
    #[allow(clippy::too_many_arguments)]
    fn card_inline(
        &self,
        cv: &mut Canvas,
        geo: &JeiEmiGeometry,
        hits: &mut Vec<OverlayHitRegion>,
        sprites: &mut Vec<ItemSpriteSlot>,
        recipe: &OverlayRecipe,
        body: UiRect,
        missing: &BTreeSet<String>,
        show_missing: bool,
    ) {
        let mid_y = body.y + 27;
        let ins: Vec<(usize, &OverlaySlot)> = recipe
            .inputs
            .iter()
            .enumerate()
            .filter_map(|(i, s)| s.as_ref().map(|s| (i, s)))
            .take(3)
            .collect();
        for (k, (idx, s)) in ins.iter().enumerate() {
            let r = UiRect::new(body.x + 2 + k as i32 * 20, mid_y - 9, 18, 18);
            let tint = show_missing && missing.contains(&s.item_id);
            self.card_slot(
                cv,
                geo,
                hits,
                sprites,
                r,
                Some(s),
                Some(OverlayWidgetId::PopupSlot(*idx)),
                tint,
                Some(OverlayWidgetId::PopupSlot(*idx)),
            );
        }
        draw_arrow(cv, body.x + 62, mid_y - 5, 18, 10, PANEL_TEXT);
        let out_r = UiRect::new(body.x + 84, mid_y - 13, 26, 26);
        self.card_slot(
            cv,
            geo,
            hits,
            sprites,
            out_r,
            Some(&recipe.output),
            Some(OverlayWidgetId::PopupOutput),
            false,
            Some(OverlayWidgetId::PopupOutput),
        );
    }
}

/// Emit a physical-px hit region clamped inside the viewport.
fn push_hit(
    hits: &mut Vec<OverlayHitRegion>,
    geo: &JeiEmiGeometry,
    id: OverlayWidgetId,
    logical: UiRect,
) {
    let p = geo.phys(logical);
    let (x0, y0) = (p.x.max(0), p.y.max(0));
    let (x1, y1) = (
        p.right().min(geo.logical_w * geo.scale),
        p.bottom().min(geo.logical_h * geo.scale),
    );
    if x1 <= x0 || y1 <= y0 {
        return;
    }
    hits.push(OverlayHitRegion {
        rect: UiRect::new(x0, y0, (x1 - x0) as u32, (y1 - y0) as u32),
        id,
    });
}

/// `"some_mod:my_item/thing"` → `"Thing"` (title case) for catalog-less
/// ids in the craft tree.
fn humanize_id(id: &str) -> String {
    let path = id.rsplit([':', '/']).next().unwrap_or(id);
    let mut out = String::with_capacity(path.len());
    let mut cap = true;
    for ch in path.chars() {
        if ch == '_' || ch == '-' {
            out.push(' ');
            cap = true;
        } else if cap {
            out.extend(ch.to_uppercase());
            cap = false;
        } else {
            out.push(ch);
        }
    }
    if out.is_empty() {
        id.to_owned()
    } else {
        out
    }
}
