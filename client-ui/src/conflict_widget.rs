// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! Polymorph-style crafting-conflict switcher.
//!
//! Mod packs routinely register two or more recipes that match the same
//! crafting grid (or furnace input). Vanilla silently picks the first
//! match; this widget gives the player the Polymorph answer: a compact
//! `[ < ] k/N [ > ]` strip anchored right beside the output slot of the
//! station that produced the conflict, the selected candidate's output
//! name plus its `@mod` badge, and an optional drop-down listing every
//! conflicting output so a click jumps straight to the desired recipe.
//!
//! The widget exists only while a conflict exists: with zero or one
//! candidate [`render`](ConflictWidget::render) emits no draw commands
//! and no hit regions at all, so a conflict-free screen is
//! pixel-identical to vanilla.
//!
//! Selections are sticky per *input signature*: the host hashes the
//! crafting grid's contents into a `u64` key and the widget remembers the
//! preferred recipe id per key in a `BTreeMap<u64, String>`. Re-fill the
//! same grid and the previous choice is restored automatically. Every
//! user-driven selection emits
//! [`ConflictWidgetAction::SelectRecipe`], which the host turns into the
//! container's recipe-choice packet.

use std::collections::BTreeMap;

use crate::draw::{palette, text_cells, text_width, UiColor, UiDrawList, UiRect};
use crate::state::{tr, GamepadButton, UiInputEvent, UiKey, UiLang};

/// One recipe that matches the station's current input.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConflictCandidate {
    /// Registry id of the recipe (`"namespace:path"`), echoed back in
    /// [`ConflictWidgetAction::SelectRecipe`].
    pub recipe_id: String,
    /// Display name of the recipe's output item.
    pub output_name: String,
    /// Mod namespace the recipe came from — rendered as the `@mod` badge.
    pub mod_id: String,
}

impl ConflictCandidate {
    pub fn new(
        recipe_id: impl Into<String>,
        output_name: impl Into<String>,
        mod_id: impl Into<String>,
    ) -> Self {
        Self {
            recipe_id: recipe_id.into(),
            output_name: output_name.into(),
            mod_id: mod_id.into(),
        }
    }
}

/// Interactive element of the widget, reported by hit regions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConflictWidgetId {
    /// `[ < ]` — cycle to the previous candidate.
    Prev,
    /// The `k/N` counter — toggles the expanded candidate list.
    Counter,
    /// `[ > ]` — cycle to the next candidate.
    Next,
    /// A visible row in the expanded candidate list; payload is the
    /// candidate index.
    PopupRow(usize),
    /// Popup list scroll arrows (emitted only when the list overflows).
    PopupScrollUp,
    PopupScrollDown,
}

/// A clickable rectangle emitted alongside the widget's draw commands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ConflictHitRegion {
    pub rect: UiRect,
    pub id: ConflictWidgetId,
}

impl ConflictHitRegion {
    /// Point containment test (right/bottom exclusive).
    pub const fn contains(self, x: i32, y: i32) -> bool {
        self.rect.contains(x, y)
    }
}

/// What the embedding application must perform after an interaction.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConflictWidgetAction {
    /// The player picked `recipe_id` (candidate `index`). The host should
    /// update the output slot and, on servers, send the recipe-choice
    /// packet.
    SelectRecipe { recipe_id: String, index: usize },
}

/// Interactive state of the crafting-conflict switcher.
///
/// The host refills [`set_candidates`](Self::set_candidates) whenever the
/// station's input changes; everything else is widget-owned.
#[derive(Clone, Debug, Default)]
pub struct ConflictWidget {
    /// Candidate recipes matching the current input, in host order.
    pub candidates: Vec<ConflictCandidate>,
    /// Display language for the widget's own labels.
    pub lang: UiLang,
    /// Hash key of the current input grid — the host supplies it with
    /// `set_candidates`; preferences are remembered per key.
    input_signature: u64,
    /// Currently displayed candidate (`0`-based, clamped on use).
    selected: usize,
    /// Whether the expanded candidate list is open.
    expanded: bool,
    /// First visible popup row index (pre-clamp; rendering keeps range).
    popup_scroll: usize,
    /// Preferred recipe id per input signature.
    remembered: BTreeMap<u64, String>,
    /// Last hovered widget (hover highlight).
    hover: Option<ConflictWidgetId>,
}

/// Logical-height of the button strip in `scale` units.
const STRIP_H: i32 = 14;
/// Popup row pitch in `scale` units.
const POPUP_ROW_H: i32 = 12;
/// Localized caption line at the top of the expanded popup.
const CAPTION_H: i32 = 12;
/// Maximum popup rows on screen at once; longer lists scroll.
const POPUP_MAX_ROWS: usize = 8;
/// Logical gap between the anchor slot and the widget.
const ANCHOR_GAP: i32 = 4;
/// Width reserved for the popup scrollbar when the list overflows.
const POPUP_SCROLLBAR_W: i32 = 12;

/// Resolved logical-pixel geometry for one render/input pass. All rects
/// are in `scale` units; multiplying by `scale` yields physical pixels.
struct Geom {
    /// Whole strip `[<] k/N [>]` (bordered panel incl. the name line).
    strip: UiRect,
    prev: UiRect,
    counter: UiRect,
    next: UiRect,
    /// Where the name + `@mod` line is drawn, and its cell budget.
    name_pos: (i32, i32),
    name_cells: usize,
    /// Expanded popup panel and the visible rows inside it.
    popup: Option<UiRect>,
    popup_rows: Vec<(usize, UiRect)>,
    scroll_up: Option<UiRect>,
    scroll_down: Option<UiRect>,
}

impl ConflictWidget {
    pub fn new() -> Self {
        Self::default()
    }

    /// Fill the widget with the recipes matching the host's new input
    /// state. `signature` is the host-computed hash of the crafting grid
    /// (or furnace slot) contents; when a preference was previously
    /// stored under that key the remembered recipe is pre-selected. The
    /// popup closes on every refill — a changed input invalidates the old
    /// list.
    pub fn set_candidates(&mut self, signature: u64, candidates: Vec<ConflictCandidate>) {
        self.input_signature = signature;
        self.candidates = candidates;
        self.expanded = false;
        self.popup_scroll = 0;
        self.selected = self
            .remembered
            .get(&signature)
            .and_then(|id| self.candidates.iter().position(|c| &c.recipe_id == id))
            .unwrap_or(0);
    }

    /// `true` while two or more candidates exist — the only state in
    /// which the widget draws or accepts input.
    pub fn is_active(&self) -> bool {
        self.candidates.len() >= 2
    }

    /// Selected candidate index, clamped into range.
    pub fn selected(&self) -> usize {
        self.selected.min(self.candidates.len().saturating_sub(1))
    }

    /// The currently selected candidate, if any.
    pub fn selected_candidate(&self) -> Option<&ConflictCandidate> {
        self.candidates.get(self.selected())
    }

    /// Whether the expanded candidate list is open.
    pub fn expanded(&self) -> bool {
        self.expanded && self.is_active()
    }

    /// The recipe remembered for `signature`, if the player picked one.
    pub fn preferred_recipe(&self, signature: u64) -> Option<&str> {
        self.remembered.get(&signature).map(String::as_str)
    }

    /// Read-only view of the per-signature preference table.
    pub fn remembered_preferences(&self) -> &BTreeMap<u64, String> {
        &self.remembered
    }

    /// Forget every remembered preference.
    pub fn clear_preferences(&mut self) {
        self.remembered.clear();
    }

    /// Cycle the selection by `dir` (`+1` / `-1`, wrapping). Emits the
    /// `SelectRecipe` action and stores the preference.
    pub fn cycle(&mut self, dir: i32) -> Vec<ConflictWidgetAction> {
        if !self.is_active() {
            return Vec::new();
        }
        let n = self.candidates.len() as i32;
        let next = (self.selected() as i32 + dir).rem_euclid(n) as usize;
        self.select_index(next)
    }

    /// Select candidate `index` directly (popup row click, remembered
    /// preference). Out-of-range indices are clamped. Emits
    /// `SelectRecipe` and stores the preference.
    pub fn select_index(&mut self, index: usize) -> Vec<ConflictWidgetAction> {
        if !self.is_active() {
            return Vec::new();
        }
        let index = index.min(self.candidates.len() - 1);
        self.selected = index;
        // Keep the newly selected row inside the popup window.
        if self.expanded {
            if index < self.popup_scroll {
                self.popup_scroll = index;
            } else if index >= self.popup_scroll + POPUP_MAX_ROWS {
                self.popup_scroll = index + 1 - POPUP_MAX_ROWS;
            }
        }
        let recipe_id = self.candidates[index].recipe_id.clone();
        self.remembered.insert(self.input_signature, recipe_id.clone());
        vec![ConflictWidgetAction::SelectRecipe {
            recipe_id,
            index,
        }]
    }

    /// Open or close the expanded candidate list.
    pub fn toggle_expanded(&mut self) {
        if self.is_active() {
            self.expanded = !self.expanded;
        }
    }

    // -- geometry ------------------------------------------------------

    /// Candidate's `Name @mod` label for the strip's second element.
    fn name_badge_text(&self) -> String {
        match self.selected_candidate() {
            Some(c) if !c.mod_id.is_empty() => format!("{} @{}", c.output_name, c.mod_id),
            Some(c) => c.output_name.clone(),
            None => String::new(),
        }
    }

    /// Resolve the full logical-pixel layout for a `scale`-unit frame of
    /// `viewport` physical pixels around `anchor` (also physical pixels).
    fn geom(&self, anchor: UiRect, scale: u32, vw: u32, vh: u32) -> Geom {
        let s = scale.max(1) as i32;
        let w = (vw.min(i32::MAX as u32) as i32) / s;
        let h = (vh.min(i32::MAX as u32) as i32) / s;
        let a = UiRect::new(
            anchor.x / s,
            anchor.y / s,
            (anchor.w as i32 / s).max(0) as u32,
            (anchor.h as i32 / s).max(0) as u32,
        );

        let counter_label = format!("{}/{}", self.selected() + 1, self.candidates.len());
        let counter_w = (text_cells(&counter_label) as i32) * 8 + 8;
        let name_label = self.name_badge_text();
        let name_full_cells = text_cells(&name_label);
        // `[<]` + `k/N` + `[>]` with 2-px gaps and panel padding.
        let buttons_w = 4 + 3 * 2 + ARROW_W * 2 + counter_w;

        // Preferred spot: right of the anchor slot, vertically centered
        // on it. If the buttons alone would not fit on the right the
        // strip flips to the slot's left edge (`edge` then bounds the
        // strip by the slot, not the viewport).
        let mut x = a.right() + ANCHOR_GAP;
        let mut edge = w - 1;
        if x + buttons_w > w - 1 && a.x - ANCHOR_GAP - buttons_w >= 1 {
            x = a.x - ANCHOR_GAP - buttons_w;
            edge = a.x - 1;
        }
        x = x.clamp(1, (w - buttons_w - 1).max(1));
        // The name run (second line) takes whatever room remains before
        // `edge`; `strip` widens to cover it and never passes `edge`.
        let room = (edge - x).max(0);
        let name_cells = name_full_cells.min(((room - 8) / 8).max(0) as usize);
        let strip_w = buttons_w.max(8 + name_cells as i32 * 8);
        let y = (a.y + (a.h as i32 - STRIP_H - 10) / 2).clamp(1, (h - STRIP_H - 10 - 1).max(1));
        let strip = UiRect::new(x, y, strip_w.max(1) as u32, (STRIP_H + 10) as u32);

        let mut bx = x + 2;
        let by = y + 2;
        let prev = UiRect::new(bx, by, ARROW_W as u32, STRIP_H as u32 - 4);
        bx += ARROW_W + 2;
        let counter = UiRect::new(bx, by, counter_w as u32, STRIP_H as u32 - 4);
        bx += counter_w + 2;
        let next = UiRect::new(bx, by, ARROW_W as u32, STRIP_H as u32 - 4);

        let name_pos = (x + 4, y + STRIP_H + 1);

        // Popup: opens below the strip, flips above when it would fall
        // off the bottom. A localized caption leads the list; rows are
        // clipped to the viewport so nothing ever draws off-screen.
        let mut popup = None;
        let mut popup_rows = Vec::new();
        let mut scroll_up = None;
        let mut scroll_down = None;
        if self.expanded() {
            let caption = tr(self.lang, "Pick an output", "レシピを選択");
            let longest = self
                .candidates
                .iter()
                .map(|c| text_cells(&format!("{} @{}", c.output_name, c.mod_id)) as i32)
                .max()
                .unwrap_or(0)
                .max(text_cells(caption) as i32);
            let inner_w = (longest * 8 + 8)
                .max(strip_w - 4)
                .min(w - 4 - POPUP_SCROLLBAR_W);
            let overflow = self.candidates.len() > POPUP_MAX_ROWS;
            let sb_w = if overflow { POPUP_SCROLLBAR_W } else { 0 };
            let px = (x + strip_w - inner_w - sb_w - 4).clamp(1, (w - inner_w - sb_w - 1).max(1));
            let rows_shown = self.candidates.len().min(POPUP_MAX_ROWS);
            let ph = rows_shown as i32 * POPUP_ROW_H + CAPTION_H + 4 + if overflow { 2 } else { 0 };
            let below = strip.bottom() + 2;
            let py = if below + ph > h - 1 {
                (strip.y - 2 - ph).max(1)
            } else {
                below
            };
            let popup_rect = UiRect::new(px, py, (inner_w + sb_w + 4) as u32, ph.max(4) as u32);

            // Manual scroll position (clamped); selection changes pull
            // the window along inside `select_index` instead, so the
            // scroll buttons stay authoritative.
            let max_scroll = self.candidates.len() - rows_shown;
            let scroll = self.popup_scroll.min(max_scroll);

            if overflow {
                scroll_up = Some(UiRect::new(
                    popup_rect.right() - POPUP_SCROLLBAR_W - 1,
                    py + 2,
                    POPUP_SCROLLBAR_W as u32 - 2,
                    10,
                ));
                scroll_down = Some(UiRect::new(
                    popup_rect.right() - POPUP_SCROLLBAR_W - 1,
                    popup_rect.bottom() - 12,
                    POPUP_SCROLLBAR_W as u32 - 2,
                    10,
                ));
            }
            for (slot, (ci, _)) in self
                .candidates
                .iter()
                .enumerate()
                .skip(scroll)
                .take(rows_shown)
                .enumerate()
            {
                popup_rows.push((
                    ci,
                    UiRect::new(
                        px + 2,
                        py + 2 + CAPTION_H + slot as i32 * POPUP_ROW_H,
                        inner_w as u32,
                        POPUP_ROW_H as u32 - 1,
                    ),
                ));
            }
            popup = Some(popup_rect);
        }

        Geom {
            strip,
            prev,
            counter,
            next,
            name_pos,
            name_cells,
            popup,
            popup_rows,
            scroll_up,
            scroll_down,
        }
    }

    // -- interaction ---------------------------------------------------

    /// The widget under physical point `(x, y)`, if any — resolved
    /// against exactly the hit regions [`render`](Self::render) emits.
    pub fn widget_at(
        &self,
        anchor: UiRect,
        scale: u32,
        vw: u32,
        vh: u32,
        x: i32,
        y: i32,
    ) -> Option<ConflictWidgetId> {
        if !self.is_active() {
            return None;
        }
        let mut scratch = UiDrawList::new();
        self.render(anchor, scale, vw, vh, &mut scratch)
            .iter()
            .rev()
            .find(|h| h.rect.contains(x, y))
            .map(|h| h.id)
    }

    /// Activate a widget (from a click, or directly in tests). Returns
    /// the outward actions the host must perform.
    pub fn activate(&mut self, id: ConflictWidgetId) -> Vec<ConflictWidgetAction> {
        match id {
            ConflictWidgetId::Prev => self.cycle(-1),
            ConflictWidgetId::Next => self.cycle(1),
            ConflictWidgetId::Counter => {
                self.toggle_expanded();
                Vec::new()
            }
            ConflictWidgetId::PopupRow(i) => {
                self.expanded = false;
                self.select_index(i)
            }
            ConflictWidgetId::PopupScrollUp => {
                self.popup_scroll = self.popup_scroll.saturating_sub(1);
                Vec::new()
            }
            ConflictWidgetId::PopupScrollDown => {
                let max = self.candidates.len().saturating_sub(POPUP_MAX_ROWS);
                self.popup_scroll = (self.popup_scroll + 1).min(max);
                Vec::new()
            }
        }
    }

    /// Route one device-agnostic input event; returns the actions the
    /// host must perform. `anchor`, `scale`, `vw` and `vh` describe the
    /// frame exactly like [`render`](Self::render).
    ///
    /// Keyboard/gamepad semantics:
    ///
    /// * `Left`/`Right` (or D-pad left/right) cycle candidates;
    /// * `Enter`/`Space` (or gamepad South) toggles the expanded list, or
    ///   confirms the highlighted row and closes it;
    /// * `Up`/`Down` move through the expanded list's selection;
    /// * `Escape` (or gamepad East/Start) closes an open list;
    /// * clicking outside an open list closes it without a selection.
    pub fn handle_input(
        &mut self,
        ev: UiInputEvent,
        anchor: UiRect,
        scale: u32,
        vw: u32,
        vh: u32,
    ) -> Vec<ConflictWidgetAction> {
        if !self.is_active() {
            return Vec::new();
        }
        match ev {
            UiInputEvent::MouseMove { x, y } => {
                self.hover = self.widget_at(anchor, scale, vw, vh, x, y);
                Vec::new()
            }
            UiInputEvent::Click { x, y } => match self.widget_at(anchor, scale, vw, vh, x, y) {
                Some(id) => self.activate(id),
                None => {
                    if self.expanded {
                        self.expanded = false;
                    }
                    Vec::new()
                }
            },
            UiInputEvent::KeyPressed(key) => self.handle_key(key),
            UiInputEvent::CharTyped(_) => Vec::new(),
            UiInputEvent::GamepadPressed(btn) => match btn {
                GamepadButton::DpadLeft => self.cycle(-1),
                GamepadButton::DpadRight => self.cycle(1),
                GamepadButton::DpadUp => self.handle_key(UiKey::Up),
                GamepadButton::DpadDown => self.handle_key(UiKey::Down),
                GamepadButton::South => self.handle_key(UiKey::Enter),
                GamepadButton::East | GamepadButton::Start => self.handle_key(UiKey::Escape),
            },
        }
    }

    fn handle_key(&mut self, key: UiKey) -> Vec<ConflictWidgetAction> {
        match key {
            UiKey::Left => self.cycle(-1),
            UiKey::Right | UiKey::Tab => self.cycle(1),
            UiKey::Up => {
                if self.expanded {
                    self.cycle(-1)
                } else {
                    Vec::new()
                }
            }
            UiKey::Down => {
                if self.expanded {
                    self.cycle(1)
                } else {
                    self.expanded = true;
                    Vec::new()
                }
            }
            UiKey::Enter | UiKey::Space => {
                self.toggle_expanded();
                Vec::new()
            }
            UiKey::Escape => {
                self.expanded = false;
                Vec::new()
            }
            UiKey::Backspace => Vec::new(),
        }
    }

    // -- rendering -----------------------------------------------------

    /// Paint the widget into `out` and return its hit regions.
    ///
    /// `anchor` is the output slot's rectangle in physical pixels; the
    /// strip hugs its right edge (or its left edge near a viewport
    /// boundary). `scale` is the host's GUI scale (`1` = 8-px glyphs);
    /// `vw`/`vh` are the physical frame size used for edge clamping.
    ///
    /// With fewer than two candidates this emits **nothing** — no
    /// commands, no regions — so non-conflicting screens are untouched.
    pub fn render(
        &self,
        anchor: UiRect,
        scale: u32,
        vw: u32,
        vh: u32,
        out: &mut UiDrawList,
    ) -> Vec<ConflictHitRegion> {
        let mut hits = Vec::new();
        if !self.is_active() {
            return hits;
        }
        let g = self.geom(anchor, scale, vw, vh);
        let mut p = Paint {
            out,
            hits: &mut hits,
            s: scale.max(1) as i32,
            w: vw.min(i32::MAX as u32) as i32,
            h: vh.min(i32::MAX as u32) as i32,
        };

        // Strip panel.
        p.fill(g.strip, palette::PANEL);
        p.border(g.strip, palette::PANEL_BORDER);

        // [ < ]  k/N  [ > ]
        for (rect, id, label) in [
            (g.prev, ConflictWidgetId::Prev, "<"),
            (g.next, ConflictWidgetId::Next, ">"),
        ] {
            let hovered = self.hover == Some(id);
            p.fill(
                rect,
                if hovered {
                    palette::BUTTON_HOVER
                } else {
                    palette::BUTTON
                },
            );
            p.border(rect, palette::BUTTON_BORDER);
            p.ctext(rect, palette::TEXT, label);
            p.region(id, rect);
        }
        {
            let hovered = self.hover == Some(ConflictWidgetId::Counter);
            let bg = if self.expanded() || hovered {
                palette::BUTTON_HOVER
            } else {
                palette::BUTTON
            };
            p.fill(g.counter, bg);
            p.border(g.counter, palette::BUTTON_BORDER);
            let label = format!("{}/{}", self.selected() + 1, self.candidates.len());
            p.ctext(g.counter, palette::TEXT, &label);
            p.region(ConflictWidgetId::Counter, g.counter);
        }

        // Selected candidate name + @mod badge on the strip's lower line.
        if g.name_cells > 0 {
            if let Some(c) = self.selected_candidate() {
                let name = fit_text(&c.output_name, g.name_cells);
                let badge = if c.mod_id.is_empty() {
                    String::new()
                } else {
                    format!(" @{}", c.mod_id)
                };
                let badge = fit_text(&badge, g.name_cells.saturating_sub(text_cells(&name)));
                p.text(g.name_pos.0, g.name_pos.1, palette::TEXT, &name);
                let bx = g.name_pos.0 + text_cells(&name) as i32 * 8;
                p.text(bx, g.name_pos.1, palette::TEXT_DIM, &badge);
            }
        }

        // Expanded candidate list.
        if let Some(popup) = g.popup {
            p.fill(popup, palette::PANEL);
            p.border(popup, palette::PANEL_BORDER);
            let caption = tr(self.lang, "Pick an output", "レシピを選択");
            let cap_cells = ((popup.w as i32 - 8) / 8).max(0) as usize;
            let caption = fit_text(caption, cap_cells);
            p.text(popup.x + 4, popup.y + 3, palette::TEXT_DIM, &caption);
            for &(ci, row) in &g.popup_rows {
                let c = &self.candidates[ci];
                let is_sel = ci == self.selected();
                let is_hover = self.hover == Some(ConflictWidgetId::PopupRow(ci));
                p.fill(
                    row,
                    if is_sel {
                        palette::BUTTON_HOVER
                    } else if is_hover {
                        palette::BUTTON
                    } else {
                        palette::SLOT
                    },
                );
                if is_sel {
                    p.fill(UiRect::new(row.x, row.y, 2, row.h), palette::ACCENT);
                }
                let cells = ((row.w as i32 - 8) / 8).max(0) as usize;
                let name = fit_text(&c.output_name, cells);
                p.text(row.x + 4, row.y + 2, palette::TEXT, &name);
                let badge_cells =
                    cells.saturating_sub(text_cells(&name) + usize::from(!c.mod_id.is_empty()));
                if !c.mod_id.is_empty() && badge_cells > 0 {
                    let badge = fit_text(&format!("@{}", c.mod_id), badge_cells);
                    let bx = row.right() - 4 - text_cells(&badge) as i32 * 8;
                    p.text(bx, row.y + 2, palette::TEXT_DIM, &badge);
                }
                p.region(ConflictWidgetId::PopupRow(ci), row);
            }
            for (rect, id, label) in [
                (g.scroll_up, ConflictWidgetId::PopupScrollUp, "^"),
                (g.scroll_down, ConflictWidgetId::PopupScrollDown, "v"),
            ] {
                if let Some(r) = rect {
                    p.fill(r, palette::BUTTON);
                    p.border(r, palette::BUTTON_BORDER);
                    p.ctext(r, palette::TEXT, label);
                    p.region(id, r);
                }
            }
        }
        hits
    }
}

/// Scales logical-pixel geometry to physical pixels on emit so visuals
/// and hit regions never diverge.
struct Paint<'a> {
    out: &'a mut UiDrawList,
    hits: &'a mut Vec<ConflictHitRegion>,
    s: i32,
    w: i32,
    h: i32,
}

impl Paint<'_> {
    fn phys(&self, r: UiRect) -> UiRect {
        UiRect::new(
            r.x * self.s,
            r.y * self.s,
            (r.w as i32 * self.s).max(0) as u32,
            (r.h as i32 * self.s).max(0) as u32,
        )
    }

    fn fill(&mut self, r: UiRect, c: UiColor) {
        self.out.fill_rect(self.phys(r), c);
    }

    fn border(&mut self, r: UiRect, c: UiColor) {
        self.out
            .border_rect(self.phys(r), c, self.s.max(1) as u32);
    }

    fn text(&mut self, x: i32, y: i32, color: UiColor, t: &str) {
        self.out
            .text_str(x * self.s, y * self.s, self.s.max(1) as u32, color, t);
    }

    /// Horizontally centered label inside a logical rect.
    fn ctext(&mut self, r: UiRect, color: UiColor, t: &str) {
        let scale = self.s.max(1) as u32;
        let tw = text_width(t, scale) as i32;
        let cx = r.x + r.w as i32 / 2;
        let ty = r.y + ((r.h as i32 - 8) / 2).max(0);
        self.out.text_str(cx * self.s - tw / 2, ty * self.s, scale, color, t);
    }

    /// Emit a hit region for a logical rect, clipped to the viewport.
    fn region(&mut self, id: ConflictWidgetId, r: UiRect) {
        let p = self.phys(r);
        let (x0, y0) = (p.x.max(0), p.y.max(0));
        let (x1, y1) = (p.right().min(self.w), p.bottom().min(self.h));
        if x1 > x0 && y1 > y0 {
            self.hits.push(ConflictHitRegion {
                rect: UiRect::new(x0, y0, (x1 - x0) as u32, (y1 - y0) as u32),
                id,
            });
        }
    }
}

const ARROW_W: i32 = 12;

/// Elide a label to `cells` glyph cells so text never overflows its
/// widget. Control characters are flattened first — a hostile string
/// must not wrap its widget into a second line.
fn fit_text(label: &str, cells: usize) -> String {
    let label = if label.chars().any(|c| c.is_control()) {
        label
            .chars()
            .map(|c| if c.is_control() { ' ' } else { c })
            .collect::<String>()
    } else {
        label.to_owned()
    };
    if text_cells(&label) <= cells {
        return label;
    }
    if cells <= 3 {
        return ".".repeat(cells);
    }
    let mut clipped = crate::draw::first_cells(&label, cells - 3).to_owned();
    clipped.push_str("...");
    clipped
}
