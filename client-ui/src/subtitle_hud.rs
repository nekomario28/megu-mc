// SPDX-License-Identifier: LGPL-3.0-or-later
//! Spatial subtitle HUD: a bounded, self-expiring stack of directional
//! caption rows for audio-cue accessibility.
//!
//! This is the display half of the spatial-audio subtitle path: the
//! engine side (`client-app`'s `SpatialSubtitleCue`, derived from
//! `SpatialAudioEngine::subtitle_cue`) reports *which way* a sound came
//! from and how loud it feels; an embedder translates each cue into
//! [`SubtitleHudState::push`] calls — `id` deduplicates repeat plays of
//! the same sound, `label` is the caption text, `arrow` is the direction
//! bucket, and `duration_secs` is the cue's audible lifetime. The state
//! ticks down on the frame clock and [`SubtitleHudState::render`] (or
//! [`crate::layout::layout_subtitle_hud`] for full HUD collision
//! awareness) emits the bottom-right panel every frame.
//!
//! Rendering mirrors vanilla 1.21.1's subtitle overlay and this crate's
//! existing `HudSubtitle` rows: a dark translucent backdrop per row,
//! `"< label"` when the source is left, `"label >"` when right, `"v
//! label"` when behind, and a bare `"label"` when centered. Rows fade
//! out over the final [`FADE_TAIL_SECS`] of their lifetime.
//!
//! Placement is deterministic and overlap-free by construction: the
//! panel is right-anchored and a candidate-bottom sweep lifts it above
//! every reserved rectangle (hotbar, vitals, XP bar, actionbar band —
//! always — plus whatever [`render_avoiding`](Self::render_avoiding)
//! callers supply, e.g. chat, bossbars, MagicHUD, the legacy subtitle
//! rows, and the crafting-conflict button). Rows that still cannot fit
//! are dropped oldest-first rather than drawn off-viewport.

use crate::draw::{palette, UiColor, UiDrawList, UiRect};
use megu_core::ui_contract::compute_gui_scale;

/// Maximum live subtitle entries; pushing past the cap evicts the
/// oldest entry (index 0 — entries are kept oldest→newest).
pub const MAX_SUBTITLE_ENTRIES: usize = 6;

/// Fallback lifetime applied when `push` receives a non-finite or
/// non-positive `duration_secs`.
pub const DEFAULT_TTL_SECS: f32 = 3.0;

/// Seconds at end-of-life over which alpha ramps to zero.
pub const FADE_TAIL_SECS: f32 = 0.5;

/// Longest caption kept per entry (control characters flattened to
/// spaces, overflow ellipsized) — matches `magic_hud`'s label budget.
const MAX_LABEL_CELLS: usize = 40;

/// Longest dedup key kept per entry.
const MAX_ID_CELLS: usize = 64;

/// Logical px per subtitle row (8 px glyphs + 2 px leading), matching the
/// legacy `hud.subtitles` row pitch.
const ROW_H: i32 = 10;

/// Horizontal padding inside a row's backdrop, logical px.
const ROW_PAD_X: i32 = 3;

/// Viewport edge margin, logical px.
const EDGE: i32 = 4;

/// Gap kept between the panel and whatever it stacks above, logical px.
const LIFT_GAP: i32 = 2;

/// Preferred top margin when the panel is lifted by collisions.
const TOP_EDGE: i32 = 4;

/// Default panel bottom offset from the viewport bottom, logical px —
/// inside the hotbar band on purpose: the reserved-rect sweep lifts the
/// panel only when it would actually intersect something.
const DEFAULT_BOTTOM_LIFT: i32 = 32;

/// Backdrop alpha at full brightness (same translucent HUD backing the
/// chat/minimap/subtitle rows use).
const BACKDROP_ALPHA: u8 = 110;

/// Where a spatial cue originated relative to the listener's facing.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SubtitleArrow {
    /// Source left of facing — drawn `"< label"`.
    Left,
    /// Source roughly ahead — drawn `"label"`.
    #[default]
    Center,
    /// Source right of facing — drawn `"label >"`.
    Right,
    /// Source behind the listener — drawn `"v label"`.
    Behind,
}

impl SubtitleArrow {
    /// Text before the label (`""` when the marker trails or is absent).
    fn prefix(self) -> &'static str {
        match self {
            SubtitleArrow::Left => "< ",
            SubtitleArrow::Behind => "v ",
            SubtitleArrow::Center | SubtitleArrow::Right => "",
        }
    }

    /// Text after the label (`""` unless the marker trails).
    fn suffix(self) -> &'static str {
        match self {
            SubtitleArrow::Right => " >",
            SubtitleArrow::Left | SubtitleArrow::Center | SubtitleArrow::Behind => "",
        }
    }

    /// Glyph cells the markers occupy (`< `, `v `, ` >` are 2 cells,
    /// centered cues have none).
    fn marker_cells(self) -> usize {
        crate::draw::text_cells(self.prefix()) + crate::draw::text_cells(self.suffix())
    }
}

/// One live subtitle cue. `ttl_secs` counts down via
/// [`SubtitleHudState::tick`]; `max_ttl_secs` records the lifetime the
/// entry was last refreshed to; `count` tracks repeat plays deduplicated
/// onto the same `id`.
#[derive(Clone, Debug, PartialEq)]
pub struct SubtitleEntry {
    pub id: String,
    pub label: String,
    pub arrow: SubtitleArrow,
    pub ttl_secs: f32,
    pub max_ttl_secs: f32,
    pub count: u32,
}

/// Bounded set of live subtitle cues, oldest first.
///
/// All mutation is deterministic: `push` sanitizes its inputs, dedup is
/// an exact `id` match, eviction is strictly oldest-first, and `tick`
/// ignores non-finite or non-positive deltas so a bad frame clock can
/// never corrupt lifetimes.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SubtitleHudState {
    entries: Vec<SubtitleEntry>,
}

impl SubtitleHudState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Live entries, oldest first (index 0 evicts next).
    pub fn entries(&self) -> &[SubtitleEntry] {
        &self.entries
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Drop every entry.
    pub fn clear(&mut self) {
        self.entries.clear();
    }

    /// Record a cue. `duration_secs` is sanitized: anything non-finite
    /// or `<= 0.0` becomes [`DEFAULT_TTL_SECS`]. `label` has control
    /// characters flattened and is truncated to [`MAX_LABEL_CELLS`]
    /// cells with an ellipsis; `id` is truncated to [`MAX_ID_CELLS`]
    /// cells and deduplicates — a repeat `id` refreshes `ttl_secs` and
    /// `max_ttl_secs` to the new duration, updates `arrow`/`label`,
    /// increments `count`, and moves the entry to newest.
    pub fn push(
        &mut self,
        id: impl Into<String>,
        label: impl Into<String>,
        arrow: SubtitleArrow,
        duration_secs: f32,
    ) {
        let ttl = if duration_secs.is_finite() && duration_secs > 0.0 {
            duration_secs
        } else {
            DEFAULT_TTL_SECS
        };
        let id: String = crate::draw::first_cells(id.into().as_str(), MAX_ID_CELLS).to_owned();
        let label = clean_label(&label.into());
        if let Some(pos) = self.entries.iter().position(|e| e.id == id) {
            let mut entry = self.entries.remove(pos);
            entry.label = label;
            entry.arrow = arrow;
            entry.ttl_secs = ttl;
            entry.max_ttl_secs = ttl;
            entry.count = entry.count.saturating_add(1);
            self.entries.push(entry);
            return;
        }
        if self.entries.len() >= MAX_SUBTITLE_ENTRIES {
            self.entries.remove(0);
        }
        self.entries.push(SubtitleEntry {
            id,
            label,
            arrow,
            ttl_secs: ttl,
            max_ttl_secs: ttl,
            count: 1,
        });
    }

    /// Advance every lifetime by `dt_sec` seconds and expire entries at
    /// `ttl <= 0.0`. Non-finite or non-positive deltas are ignored
    /// entirely — a paused or corrupt clock freezes the stack instead of
    /// expiring or resurrecting rows.
    pub fn tick(&mut self, dt_sec: f32) {
        if !dt_sec.is_finite() || dt_sec <= 0.0 {
            return;
        }
        for entry in &mut self.entries {
            entry.ttl_secs -= dt_sec;
        }
        self.entries.retain(|entry| entry.ttl_secs > 0.0);
    }

    /// Append this frame's subtitle rows to `out`, bottom-right anchored
    /// above the hotbar band. `viewport_w`/`viewport_h` are physical
    /// pixels; the panel manages its own GUI scale exactly like
    /// [`crate::magic_hud::build_magic_hud_draw_list`]. Emits nothing
    /// when empty or the viewport is degenerate.
    ///
    /// This standalone path only knows viewport-derivable chrome (it
    /// assumes the default auto GUI scale); embedders compositing over a
    /// live HUD should prefer
    /// [`crate::layout::layout_subtitle_hud`], which supplies the real
    /// occupied rectangles.
    pub fn render(&self, viewport_w: f32, viewport_h: f32, out: &mut UiDrawList) {
        self.render_avoiding(viewport_w, viewport_h, &[], out);
    }

    /// [`render`](Self::render) plus caller-supplied occupied rectangles
    /// in *physical* pixels (chat rows, bossbars, MagicHUD frame, the
    /// crafting-conflict button, …) that the panel must not intersect.
    /// Returns `true` when at least one row was emitted.
    pub(crate) fn render_avoiding(
        &self,
        viewport_w: f32,
        viewport_h: f32,
        reserved_phys: &[UiRect],
        out: &mut UiDrawList,
    ) -> bool {
        if self.entries.is_empty()
            || !viewport_w.is_finite()
            || !viewport_h.is_finite()
            || viewport_w <= 0.0
            || viewport_h <= 0.0
        {
            return false;
        }
        let ms = gui_scale(viewport_w, viewport_h);
        let (lw, lh) = logical_size(viewport_w, viewport_h, ms);
        if lw < MIN_LOGICAL_W || lh < MIN_LOGICAL_H {
            return false;
        }

        // Reserved rectangles in this overlay's logical-pixel space:
        // viewport-derivable chrome first (auto-scale assumption), then
        // whatever the HUD layout pass measured.
        let mut reserved = hud_chrome_reserved(viewport_w, viewport_h, ms);
        reserved.extend(reserved_phys.iter().map(|r| phys_to_logical(*r, ms)));

        // Rows render newest-on-top, oldest at the bottom — the same
        // order the legacy `hud.subtitles` stack uses.
        let max_rows = ((lh - 2 * TOP_EDGE) / ROW_H).max(0) as usize;
        let mut rows: Vec<(&SubtitleEntry, usize)> = self
            .entries
            .iter()
            .rev()
            .filter_map(|entry| {
                let marker = entry.arrow.marker_cells();
                (marker <= max_inner_cells(lw)).then_some((entry, marker))
            })
            .take(max_rows.min(MAX_SUBTITLE_ENTRIES))
            .collect();
        if rows.is_empty() {
            return false;
        }

        // Uniform panel width: widest formatted row, capped to the
        // viewport. Rows wider than the cap get their label hard-clipped
        // at emit time; the backdrop stays rectangular.
        let inner_cells = rows
            .iter()
            .map(|(entry, marker)| marker + crate::draw::text_cells(&entry.label))
            .max()
            .unwrap_or(0)
            .min(max_inner_cells(lw));
        let panel_w = (inner_cells as i32) * 8 + 2 * ROW_PAD_X;
        let panel_x = lw - EDGE - panel_w;

        // Candidate bottoms: the default anchor plus "just above each
        // reserved rect". The largest feasible one wins, so the panel
        // stays as low as possible while intersecting nothing. If even
        // one row cannot fit, oldest rows drop until it can.
        let find_bottom = |row_count: usize, min_top: i32| -> Option<i32> {
            let panel_h = row_count as i32 * ROW_H;
            let mut bottoms: Vec<i32> = Vec::with_capacity(reserved.len() + 1);
            bottoms.push(lh - DEFAULT_BOTTOM_LIFT);
            bottoms.extend(reserved.iter().map(|r| r.y - LIFT_GAP));
            bottoms.retain(|&b| b <= lh - DEFAULT_BOTTOM_LIFT && b - panel_h >= min_top);
            bottoms.sort_unstable_by(|a, b| b.cmp(a));
            bottoms.dedup();
            bottoms.into_iter().find(|&b| {
                let rect = UiRect::new(
                    panel_x,
                    b - panel_h,
                    panel_w as u32,
                    panel_h as u32,
                );
                reserved.iter().all(|r| !rect.intersects(*r))
            })
        };

        let panel_h;
        let bottom = loop {
            let h = rows.len() as i32 * ROW_H;
            if let Some(b) = find_bottom(rows.len(), TOP_EDGE) {
                panel_h = h;
                break b;
            }
            if rows.len() > 1 {
                rows.pop();
                continue;
            }
            // Single row, no clean spot: relax the top margin, then fall
            // back to a viewport-clamped placement rather than dropping
            // the last caption on degenerate screens.
            panel_h = h;
            break find_bottom(1, 0).unwrap_or(TOP_EDGE + ROW_H);
        };
        let top = bottom - panel_h;

        for (i, (entry, marker)) in rows.iter().enumerate() {
            let row_y = top + i as i32 * ROW_H;
            let fade = fade_factor(entry.ttl_secs);
            out.fill_rect(
                UiRect::new(
                    panel_x * ms as i32,
                    row_y * ms as i32,
                    (panel_w * ms as i32) as u32,
                    (ROW_H * ms as i32) as u32,
                ),
                UiColor::rgba(0, 0, 0, (f32::from(BACKDROP_ALPHA) * fade) as u8),
            );
            let label_budget = inner_cells.saturating_sub(*marker);
            let clipped = crate::draw::first_cells(&entry.label, label_budget);
            let mut text = String::with_capacity(entry.label.len() + 4);
            text.push_str(entry.arrow.prefix());
            text.push_str(clipped);
            text.push_str(entry.arrow.suffix());
            out.text_str(
                (panel_x + ROW_PAD_X) * ms as i32,
                (row_y + 1) * ms as i32,
                ms,
                palette::TEXT.with_alpha((255.0 * fade) as u8),
                &text,
            );
        }
        true
    }
}

/// Smallest logical width/height that can host even one row.
const MIN_LOGICAL_W: i32 = 48;
const MIN_LOGICAL_H: i32 = 44;

/// GUI scale for the subtitle overlay — the canonical vanilla auto
/// scale ([`compute_gui_scale`] with `requested == 0`) so every
/// bottom-corner overlay agrees with `UiState::effective_scale`.
fn gui_scale(viewport_w: f32, viewport_h: f32) -> u32 {
    compute_gui_scale(viewport_w as u32, viewport_h as u32, 0)
}

/// Logical-pixel viewport at `ms` GUI scale.
fn logical_size(viewport_w: f32, viewport_h: f32, ms: u32) -> (i32, i32) {
    (
        (viewport_w / ms as f32) as i32,
        (viewport_h / ms as f32) as i32,
    )
}

/// Convert a physical-pixel rect into this overlay's logical space,
/// rounding outward so collisions can never be missed by a pixel.
fn phys_to_logical(rect: UiRect, ms: u32) -> UiRect {
    let m = ms as f32;
    let x0 = (rect.x as f32 / m).floor() as i32;
    let y0 = (rect.y as f32 / m).floor() as i32;
    let x1 = (rect.right() as f32 / m).ceil() as i32;
    let y1 = (rect.bottom() as f32 / m).ceil() as i32;
    UiRect::new(x0, y0, (x1 - x0).max(0) as u32, (y1 - y0).max(0) as u32)
}

/// Same conversion for a float physical rect (the internal chrome math).
fn phys_f32_to_logical(x: f32, y: f32, w: f32, h: f32, ms: u32) -> UiRect {
    let m = ms as f32;
    let x0 = (x / m).floor() as i32;
    let y0 = (y / m).floor() as i32;
    let x1 = ((x + w) / m).ceil() as i32;
    let y1 = ((y + h) / m).ceil() as i32;
    UiRect::new(x0, y0, (x1 - x0).max(0) as u32, (y1 - y0).max(0) as u32)
}

/// Chrome every HUD draws regardless of game state — hotbar block, XP
/// bar + level, both vitals icon columns (worst-case row counts), and
/// the actionbar band — expressed in physical pixels under the default
/// auto GUI scale, then converted to this overlay's logical space.
fn hud_chrome_reserved(viewport_w: f32, viewport_h: f32, ms: u32) -> Vec<UiRect> {
    // `ms` is the canonical auto GUI scale, identical to the scale the
    // HUD chrome itself resolves at `ui_scale == 0`.
    let sa = ms as f32;
    let cx = viewport_w * 0.5;
    // Hotbar incl. offhand + gap: logical cx±112 × lh-26..lh-4.
    // XP bar + level text: logical cx±96 × lh-40..lh-27.
    // Vitals columns, worst case (absorption row left, air row right):
    // left logical cx-86..cx-6 × lh-58..lh-30, right cx+32..cx+112 × lh-48..lh-30.
    // Actionbar band (worst-case full width): logical lh-70..lh-62.
    vec![
        phys_f32_to_logical(cx - 224.0 * sa, viewport_h - 26.0 * sa, 448.0 * sa, 22.0 * sa, ms),
        phys_f32_to_logical(cx - 96.0 * sa, viewport_h - 40.0 * sa, 192.0 * sa, 14.0 * sa, ms),
        phys_f32_to_logical(cx - 86.0 * sa, viewport_h - 58.0 * sa, 80.0 * sa, 28.0 * sa, ms),
        phys_f32_to_logical(cx + 32.0 * sa, viewport_h - 48.0 * sa, 80.0 * sa, 18.0 * sa, ms),
        phys_f32_to_logical(0.0, viewport_h - 70.0 * sa, viewport_w, 8.0 * sa, ms),
    ]
}

/// Widest row (markers + label) in glyph cells that still fits the
/// viewport with edge margins and backdrop padding.
fn max_inner_cells(lw: i32) -> usize {
    ((lw - 2 * EDGE - 2 * ROW_PAD_X).max(0) / 8) as usize
}

/// Flatten control characters and truncate to [`MAX_LABEL_CELLS`] with
/// an ellipsis — the same rule `magic_hud::clipped_label` applies.
fn clean_label(label: &str) -> String {
    let flat: String = label
        .chars()
        .map(|ch| if ch.is_control() { ' ' } else { ch })
        .collect();
    let visible = crate::draw::first_cells(&flat, MAX_LABEL_CELLS);
    if visible.len() < flat.len() && MAX_LABEL_CELLS >= 3 {
        let mut clipped = crate::draw::first_cells(&flat, MAX_LABEL_CELLS - 3).to_owned();
        clipped.push_str("...");
        clipped
    } else {
        visible.to_owned()
    }
}

/// Alpha multiplier for the final [`FADE_TAIL_SECS`] of an entry's life.
fn fade_factor(ttl_secs: f32) -> f32 {
    if !ttl_secs.is_finite() {
        return 0.0;
    }
    (ttl_secs / FADE_TAIL_SECS).clamp(0.0, 1.0)
}
