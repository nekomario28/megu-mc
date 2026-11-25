// SPDX-License-Identifier: LGPL-3.0-or-later
//! Vanilla 1.21.1 in-game HUD widgets: vitals, status-effect icons,
//! bossbars, scoreboard sidebar, title/subtitle/actionbar fades, the
//! hold-Tab player list, chat input with Brigadier tab-completion, and
//! the F3 debug overlay.
//!
//! This is a *standalone* widget pack: [`HudSnapshot`] is the pure-data
//! frame an embedder fills from the game state, and
//! [`HudWidgetsRenderer::render`] appends the frame's commands to any
//! [`UiDrawList`] — typically after [`crate::layout::layout_screen`]'s
//! own HUD pass. Nothing here touches `UiState`; the two HUDs can be
//! composited or used independently.
//!
//! Conventions mirror the rest of the crate:
//!
//! * Layout happens in *logical* pixels at an auto GUI scale
//!   ([`HudWidgetsRenderer::gui_scale`], `0` = auto), exactly like
//!   [`crate::magic_hud`] and [`crate::subtitle_hud`]; every emitted
//!   command is multiplied by the scale, so output is physical pixels.
//! * All host-supplied text is sanitized (control characters flattened,
//!   clipped to a cell budget), all counts are bounded by the `MAX_*`
//!   constants, and every emitted rectangle is clamped inside the
//!   viewport — a hostile or corrupt snapshot can never draw off-screen.
//! * Widgets are placed by a claim pass: each widget records the
//!   rectangle it occupies and later widgets are laid out around earlier
//!   claims, so the returned [`HudRegionReport`] is a machine-checkable
//!   non-overlap proof for any viewport.
//!
//! Vanilla anchors used (logical px, y down): bossbars stack from the
//! top edge, status effects fill the top-right in a two-row grid, the
//! debug overlay hugs both top corners, the tab list hangs below the
//! bossbars, title/subtitle center around the upper third, the actionbar
//! floats above the vitals, vitals flank a 224 px hotbar reservation at
//! the bottom edge, the scoreboard sidebar centers on the right edge,
//! and chat input/scrollback stack upward from above the actionbar on
//! the left.

use crate::draw::{palette, UiColor, UiDrawList, UiRect};

// ---------------------------------------------------------------------------
// Bounds and metrics (logical px unless noted)
// ---------------------------------------------------------------------------

/// Maximum bossbars rendered; extras are ignored.
pub const MAX_BOSSBARS: usize = 4;
/// Maximum scoreboard rows rendered (after sorting); extras drop off.
pub const MAX_SCOREBOARD_ROWS: usize = 15;
/// Maximum status-effect icons rendered; extras drop off.
pub const MAX_STATUS_EFFECTS: usize = 16;
/// Effect icons per grid row (16 icons = the vanilla two-row cap).
pub const STATUS_EFFECTS_PER_ROW: usize = 8;
/// Remaining ticks below which an effect icon blinks (vanilla flashes
/// expiring effects during their last ~10 s).
pub const EFFECT_BLINK_TICKS: u32 = 200;
/// Blink half-period in ticks while under [`EFFECT_BLINK_TICKS`].
pub const EFFECT_BLINK_PERIOD_TICKS: u32 = 10;
/// Maximum tab-list players considered (4 columns × 20 rows).
pub const MAX_TAB_ENTRIES: usize = 80;
/// Rows per tab-list column.
pub const TAB_ROWS_PER_COLUMN: usize = 20;
/// Maximum tab-list columns.
pub const MAX_TAB_COLUMNS: usize = 4;
/// Maximum stored chat history lines (oldest evicted first).
pub const MAX_CHAT_HISTORY: usize = 100;
/// Maximum stored chat scrollback lines.
pub const MAX_CHAT_SCROLLBACK: usize = 100;
/// Maximum stored completion candidates per refresh.
pub const MAX_CHAT_COMPLETIONS: usize = 64;
/// Maximum completion rows drawn above the chat input.
pub const MAX_CHAT_COMPLETION_ROWS: usize = 8;
/// Maximum scrollback lines drawn per frame.
pub const MAX_CHAT_SCROLLBACK_LINES: usize = 10;
/// Maximum characters kept in the chat input buffer.
pub const MAX_CHAT_INPUT_CELLS: usize = 256;
/// Maximum lines per debug column.
pub const MAX_DEBUG_LINES: usize = 16;
/// Sentinel for "no completion candidate applied yet".
pub const NO_COMPLETION: usize = usize::MAX;

const EDGE: i32 = 4;
const ROW_H: i32 = 10;
const BOSS_PITCH: i32 = 14;
const BOSS_W_MAX: i32 = 200;
const HOTBAR_W: i32 = 224;
const HOTBAR_BOTTOM_GAP: i32 = 4;
/// Logical px reserved at the bottom for the hotbar itself (22 px tall,
/// 4 px gap): the vitals stack above it.
const HOTBAR_RESERVE: i32 = 22 + HOTBAR_BOTTOM_GAP;
const XP_BAR_W: i32 = 182;
const ACTIONBAR_LIFT: i32 = 64;
const CHAT_INPUT_W_MAX: i32 = 280;
const CHAT_INPUT_H: i32 = 10;
const EFFECT_CELL_W: i32 = 34;
const EFFECT_CELL_H: i32 = ROW_H;
const TAB_COL_EXTRA: i32 = 30;
const DEBUG_COL_GAP: i32 = 12;

/// Longest cell budget for single-row labels (debug lines) — keeps the
/// two debug columns inside the free width.
const MAX_LABEL_CELLS: usize = 96;

// ---------------------------------------------------------------------------
// Shared sanitization helpers
// ---------------------------------------------------------------------------

/// Flatten control characters (a `\n` inside a hostile packet string must
/// never wrap the HUD) and truncate to `max_cells` glyph cells, adding an
/// ellipsis when truncated — the same rule `magic_hud::clipped_label`
/// applies.
fn clip_cells(text: &str, max_cells: usize) -> String {
    let flat: String = text
        .chars()
        .map(|ch| if ch.is_control() { ' ' } else { ch })
        .collect();
    let visible = crate::draw::first_cells(&flat, max_cells);
    if visible.len() < flat.len() && max_cells >= 3 {
        let mut clipped = crate::draw::first_cells(&flat, max_cells - 3).to_owned();
        clipped.push_str("...");
        clipped
    } else {
        visible.to_owned()
    }
}

/// `value` when finite, else `fallback` — NaN/inf can never reach layout.
fn finite_or(value: f32, fallback: f32) -> f32 {
    if value.is_finite() {
        value
    } else {
        fallback
    }
}

/// Fraction clamped to `0.0..=1.0`; NaN collapses to 0.
fn unit_frac(value: f32) -> f32 {
    if value.is_finite() {
        value.clamp(0.0, 1.0)
    } else {
        0.0
    }
}

/// Dark cell backing shared by every icon row (matches `layout.rs`).
const ICON_BACK: UiColor = palette::TOOLTIP_BG;

/// Translucent row backing shared by chat scrollback.
const ROW_BACK: UiColor = palette::OVERLAY_DIM.with_alpha(110);

/// First `limit` chars of `text` as a `&str` slice (no allocation).
fn first_chars(text: &str, limit: usize) -> &str {
    crate::draw::first_cells(text, limit)
}

// ---------------------------------------------------------------------------
// Vitals
// ---------------------------------------------------------------------------

/// Heart/armor icon style — vanilla swaps the heart texture for status
/// damage and game mode.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum HeartStyle {
    /// Survival/normal red hearts.
    #[default]
    Normal,
    /// Poisoned: green hearts.
    Poison,
    /// Withered: dark hearts.
    Wither,
    /// Frozen: pale-blue hearts.
    Frozen,
    /// Hardcore world: darker red hearts.
    Hardcore,
}

impl HeartStyle {
    fn color(self) -> UiColor {
        match self {
            HeartStyle::Normal => palette::HEALTH,
            HeartStyle::Poison => palette::ACCENT,
            HeartStyle::Wither => palette::TEXT_DARK,
            HeartStyle::Frozen => palette::AIR,
            HeartStyle::Hardcore => palette::DANGER,
        }
    }
}

/// Survival vitals snapshot. Units follow vanilla:
///
/// * `health`/`max_health`/`absorption` — hit points (20 = full bar);
/// * `armor`/`food` — points, 2 per icon, 20 max;
/// * `saturation` — food-scale points overlaid on the food icons;
/// * `air`/`max_air` — air supply, `max_air = 300` = ten bubbles of 30;
/// * `xp_progress` — `0.0..=1.0` toward the next level.
///
/// `show_vitals` is the creative/spectator gate: when `false` the hearts,
/// armor, food, and air rows are omitted (the XP bar still renders, as in
/// vanilla creative).
#[derive(Clone, Debug, PartialEq)]
pub struct VitalsState {
    pub health: f32,
    pub max_health: f32,
    pub absorption: f32,
    pub heart_style: HeartStyle,
    pub armor: u8,
    pub food: u8,
    pub saturation: f32,
    pub air: u16,
    pub max_air: u16,
    pub xp_progress: f32,
    pub xp_level: u32,
    pub show_vitals: bool,
}

impl Default for VitalsState {
    fn default() -> Self {
        Self {
            health: 20.0,
            max_health: 20.0,
            absorption: 0.0,
            heart_style: HeartStyle::Normal,
            armor: 0,
            food: 20,
            saturation: 0.0,
            air: 300,
            max_air: 300,
            xp_progress: 0.0,
            xp_level: 0,
            show_vitals: true,
        }
    }
}

// ---------------------------------------------------------------------------
// Status effects
// ---------------------------------------------------------------------------

/// One active status effect. `amplifier` 0 means level I; `remaining_ticks`
/// drives both the duration label and the low-duration blink.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StatusEffectEntry {
    /// Dedup key (effect type id); repeats refresh in place.
    pub id: String,
    /// Short label kept for tooling/debug; the grid shows icon + duration.
    pub label: String,
    pub amplifier: u8,
    pub remaining_ticks: u32,
    pub beneficial: bool,
}

/// Bounded top-right effect grid. [`push`](Self::push) deduplicates on
/// `id` and evicts oldest-first past [`MAX_STATUS_EFFECTS`]; rendering
/// additionally takes only the first [`MAX_STATUS_EFFECTS`] entries so an
/// oversized snapshot stays bounded.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StatusEffectHud {
    pub entries: Vec<StatusEffectEntry>,
}

impl StatusEffectHud {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn entries(&self) -> &[StatusEffectEntry] {
        &self.entries
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn clear(&mut self) {
        self.entries.clear();
    }

    /// Record an effect: an existing `id` refreshes duration/amplifier/
    /// polarity and moves to newest; a new id appends, evicting the
    /// oldest entry past [`MAX_STATUS_EFFECTS`].
    pub fn push(&mut self, entry: StatusEffectEntry) {
        let mut entry = entry;
        entry.label = clip_cells(&entry.label, MAX_LABEL_CELLS);
        if let Some(pos) = self.entries.iter().position(|e| e.id == entry.id) {
            self.entries.remove(pos);
        } else if self.entries.len() >= MAX_STATUS_EFFECTS {
            self.entries.remove(0);
        }
        self.entries.push(entry);
    }
}

/// Whether an effect icon is lit this frame. Entries at
/// `remaining_ticks >= EFFECT_BLINK_TICKS` always show; below that they
/// flash on a fixed [`EFFECT_BLINK_PERIOD_TICKS`] period — deterministic
/// from the snapshot, no frame clock needed.
pub fn effect_icon_visible(remaining_ticks: u32) -> bool {
    remaining_ticks > 0
        && (remaining_ticks >= EFFECT_BLINK_TICKS
            || (remaining_ticks / EFFECT_BLINK_PERIOD_TICKS).is_multiple_of(2))
}

/// Compact duration label: `"Ns"` under a minute, `"Nm"` under an hour,
/// `"Nh"` beyond (hours saturate at 99). Always ≤ 3 glyph cells.
fn effect_duration_label(ticks: u32) -> String {
    let secs = ticks.div_ceil(20);
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else {
        format!("{}h", (secs / 3600).min(99))
    }
}

// ---------------------------------------------------------------------------
// Bossbars
// ---------------------------------------------------------------------------

/// Vanilla bossbar colors (approximate palette mapping).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum BossBarColor {
    #[default]
    Pink,
    Blue,
    Red,
    Green,
    Yellow,
    Purple,
    White,
}

impl BossBarColor {
    fn color(self) -> UiColor {
        match self {
            BossBarColor::Pink => palette::HEALTH,
            BossBarColor::Blue => palette::AIR,
            BossBarColor::Red => palette::DANGER,
            BossBarColor::Green => palette::XP,
            BossBarColor::Yellow => palette::WARNING,
            BossBarColor::Purple => palette::ACCENT,
            BossBarColor::White => palette::TEXT,
        }
    }
}

/// One bossbar: centered name over a colored progress bar with optional
/// division notches (vanilla `NOTCHED_*` overlays). `notches` is the
/// number of divider ticks, clamped to `0..=20`; `0` is a solid bar.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct BossBarEntry {
    pub title: String,
    /// Fill fraction; sanitized to `0.0..=1.0` at render.
    pub fraction: f32,
    pub color: BossBarColor,
    pub notches: u8,
}

// ---------------------------------------------------------------------------
// Scoreboard sidebar
// ---------------------------------------------------------------------------

/// One scoreboard row. Rendered sorted by `score` descending (ties by
/// label ascending for determinism), capped at [`MAX_SCOREBOARD_ROWS`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ScoreboardEntry {
    pub label: String,
    pub score: i32,
}

/// Right-edge scoreboard sidebar: optional title line over sorted rows.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ScoreboardSidebar {
    pub title: Option<String>,
    pub entries: Vec<ScoreboardEntry>,
}

// ---------------------------------------------------------------------------
// Title / subtitle / actionbar
// ---------------------------------------------------------------------------

/// A text element with vanilla's fade-in / hold / fade-out alpha curve.
///
/// `alpha()` is a pure function of `age_ticks`:
///
/// * `age < fade_in` ramps `0 → 1` linearly (skipped when `fade_in = 0`),
/// * `fade_in ≤ age < fade_in + stay` holds at `1.0`,
/// * the final `fade_out` ticks ramp `1 → 0`, and
/// * `age ≥ fade_in + stay + fade_out` is fully transparent.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TimedText {
    pub text: String,
    pub age_ticks: u32,
    /// Vanilla defaults: 10 ticks in, 70 held, 20 out.
    pub fade_in_ticks: u32,
    pub stay_ticks: u32,
    pub fade_out_ticks: u32,
}

impl TimedText {
    /// Vanilla 1.21.1 title/subtitle timing (10 / 70 / 20 ticks).
    pub fn vanilla(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            age_ticks: 0,
            fade_in_ticks: 10,
            stay_ticks: 70,
            fade_out_ticks: 20,
        }
    }

    /// Current alpha in `0.0..=1.0`.
    pub fn alpha(&self) -> f32 {
        let fi = self.fade_in_ticks as f32;
        let st = self.stay_ticks as f32;
        let fo = self.fade_out_ticks as f32;
        let total = fi + st + fo;
        if total <= 0.0 {
            return 0.0;
        }
        let age = self.age_ticks as f32;
        if age >= total {
            return 0.0;
        }
        if fi > 0.0 && age < fi {
            return (age / fi).clamp(0.0, 1.0);
        }
        if age < fi + st {
            return 1.0;
        }
        if fo > 0.0 {
            return ((total - age) / fo).clamp(0.0, 1.0);
        }
        0.0
    }

    /// `true` while any part of the element is visible.
    pub fn is_visible(&self) -> bool {
        self.alpha() > 0.0
    }
}

/// Title block (big centered title + subtitle) plus the transient
/// actionbar message above the hotbar. Each element carries its own
/// [`TimedText`] clock so they fade independently, like vanilla.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TitleOverlay {
    pub title: Option<TimedText>,
    pub subtitle: Option<TimedText>,
    pub actionbar: Option<TimedText>,
}

// ---------------------------------------------------------------------------
// Tab player list
// ---------------------------------------------------------------------------

/// One row of the hold-Tab player list.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TabPlayerEntry {
    pub name: String,
    /// Round-trip milliseconds; drives the 5-bar latency indicator.
    pub ping_ms: u16,
    /// Vanilla hides unlisted players (`PlayerInfo::listed()`).
    pub listed: bool,
}

impl TabPlayerEntry {
    pub fn new(name: impl Into<String>, ping_ms: u16) -> Self {
        Self {
            name: name.into(),
            ping_ms,
            listed: true,
        }
    }
}

/// Hold-Tab overlay: up to [`MAX_TAB_COLUMNS`] columns of
/// [`TAB_ROWS_PER_COLUMN`] rows, centered below the bossbar stack.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TabPlayerList {
    pub entries: Vec<TabPlayerEntry>,
    /// Vanilla hold-Tab semantics: hidden unless the host sets this.
    pub visible: bool,
}

// ---------------------------------------------------------------------------
// Chat input + Brigadier completion
// ---------------------------------------------------------------------------

/// Chat state: the input line, history recall, scrollback, and the
/// Brigadier-style tab-completion popup.
///
/// Editing methods are deterministic and bounded: the buffer caps at
/// [`MAX_CHAT_INPUT_CELLS`] chars, history/scrollback at
/// [`MAX_CHAT_HISTORY`]/[`MAX_CHAT_SCROLLBACK`] lines, and completions at
/// [`MAX_CHAT_COMPLETIONS`] candidates. `cycle_completion` replaces the
/// current token (text after the last whitespace) with the next
/// candidate, wrapping in both directions — the same contract vanilla's
/// suggestion box implements.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ChatInputOverlay {
    /// Chat screen open: draws the input box and completion popup.
    pub open: bool,
    /// Raw input buffer (rendering sanitizes for display).
    pub text: String,
    /// Caret position as a *char* index into `text`.
    pub cursor: usize,
    /// Submitted lines, oldest first.
    pub history: Vec<String>,
    /// `Some(i)` while browsing `history[i]`; `None` edits `text` live.
    pub history_pos: Option<usize>,
    /// Stashed live input while browsing history.
    pub draft: String,
    /// Current completion candidates (host-supplied via
    /// [`set_completions`](Self::set_completions)).
    pub completions: Vec<String>,
    /// Index of the applied candidate, or [`NO_COMPLETION`].
    pub completion_index: usize,
    /// Byte offset where the active completion token starts; `None` when
    /// no completion has been applied yet. Internal to the cycle logic —
    /// hosts should not set it.
    pub completion_span_start: Option<usize>,
    /// Recent chat lines, oldest first; the HUD draws the newest
    /// [`MAX_CHAT_SCROLLBACK_LINES`].
    pub scrollback: Vec<String>,
}

impl ChatInputOverlay {
    pub fn new() -> Self {
        Self::default()
    }

    /// Character count of the input buffer.
    pub fn char_count(&self) -> usize {
        self.text.chars().count()
    }

    /// Caret clamped into the buffer.
    pub fn cursor(&self) -> usize {
        self.cursor.min(self.char_count())
    }

    fn reset_completion(&mut self) {
        self.completion_index = NO_COMPLETION;
        self.completion_span_start = None;
    }

    /// Insert `c` at the caret. Control characters are dropped; the
    /// buffer caps at [`MAX_CHAT_INPUT_CELLS`] chars.
    pub fn insert_char(&mut self, c: char) {
        if c.is_control() || self.char_count() >= MAX_CHAT_INPUT_CELLS {
            return;
        }
        let at = self.cursor();
        let mut text = String::with_capacity(self.text.len() + c.len_utf8());
        for (i, ch) in self.text.chars().enumerate() {
            if i == at {
                text.push(c);
            }
            text.push(ch);
        }
        if at == self.char_count() {
            text.push(c);
        }
        self.text = text;
        self.cursor = at + 1;
        self.reset_completion();
    }

    /// Delete the char before the caret.
    pub fn backspace(&mut self) {
        let at = self.cursor();
        if at == 0 {
            return;
        }
        let start = self
            .text
            .char_indices()
            .nth(at - 1)
            .map(|(i, _)| i)
            .unwrap_or(self.text.len());
        let end = self.text[start..]
            .chars()
            .next()
            .map(|c| start + c.len_utf8())
            .unwrap_or(self.text.len());
        self.text.replace_range(start..end, "");
        self.cursor = at - 1;
        self.reset_completion();
    }

    /// Replace the whole buffer (caret to end).
    pub fn set_text(&mut self, text: impl Into<String>) {
        self.text = text.into();
        self.cursor = self.char_count();
        self.reset_completion();
    }

    /// Older history entry, like vanilla's up-arrow recall. The live
    /// input is stashed in `draft` on the first step back.
    pub fn history_prev(&mut self) {
        if self.history.is_empty() {
            return;
        }
        match self.history_pos {
            None => {
                self.draft = self.text.clone();
                self.history_pos = Some(self.history.len() - 1);
            }
            Some(pos) => self.history_pos = Some(pos.saturating_sub(1)),
        }
        let pos = self.history_pos.unwrap_or(0);
        self.text = self.history[pos].clone();
        self.cursor = self.char_count();
        self.reset_completion();
    }

    /// Newer history entry; stepping past the newest restores `draft`.
    pub fn history_next(&mut self) {
        match self.history_pos {
            None => return,
            Some(pos) => {
                if pos + 1 >= self.history.len() {
                    self.history_pos = None;
                    self.text = std::mem::take(&mut self.draft);
                } else {
                    self.history_pos = Some(pos + 1);
                    self.text = self.history[pos + 1].clone();
                }
            }
        }
        self.cursor = self.char_count();
        self.reset_completion();
    }

    /// Replace the completion candidate list (host refresh after every
    /// keystroke that changes the token). Candidates are sanitized,
    /// deduplicated, and capped at [`MAX_CHAT_COMPLETIONS`].
    pub fn set_completions(&mut self, candidates: impl IntoIterator<Item = impl Into<String>>) {
        let mut out: Vec<String> = Vec::new();
        for cand in candidates {
            let clean = clip_cells(&cand.into(), MAX_LABEL_CELLS);
            if clean.is_empty() || out.contains(&clean) {
                continue;
            }
            out.push(clean);
            if out.len() >= MAX_CHAT_COMPLETIONS {
                break;
            }
        }
        self.completions = out;
        self.reset_completion();
    }

    /// Index of the currently applied candidate for popup highlighting.
    pub fn selected_completion(&self) -> Option<usize> {
        (self.completion_index != NO_COMPLETION && self.completion_index < self.completions.len())
            .then_some(self.completion_index)
    }

    /// Tab / Shift-Tab: advance the candidate and apply it to the current
    /// token. With nothing applied yet, forward selects the first
    /// candidate and backward selects the last; afterwards the index
    /// wraps around the list.
    pub fn cycle_completion(&mut self, forward: bool) {
        let len = self.completions.len();
        if len == 0 {
            self.reset_completion();
            return;
        }
        let next = if self.completion_index == NO_COMPLETION || self.completion_index >= len {
            if forward {
                0
            } else {
                len - 1
            }
        } else if forward {
            (self.completion_index + 1) % len
        } else {
            (self.completion_index + len - 1) % len
        };
        self.completion_index = next;
        // The token span is fixed for the whole cycle: record where the
        // current token starts on the first application so repeated Tabs
        // replace the same span even when candidates contain spaces.
        let start = self.completion_span_start.unwrap_or_else(|| {
            let mut start = 0usize;
            for (i, c) in self.text.char_indices() {
                if c.is_whitespace() {
                    start = i + c.len_utf8();
                }
            }
            start
        });
        self.completion_span_start = Some(start);
        let candidate = self.completions[next].clone();
        let mut text = String::with_capacity(start + candidate.len() + 8);
        text.push_str(&self.text[..start]);
        text.push_str(&candidate);
        self.text = text;
        self.cursor = self.char_count();
    }

    /// Submit the buffer: appends to `history` and `scrollback` (both
    /// bounded), clears the input and completion state, and closes the
    /// chat screen — vanilla's Enter semantics.
    pub fn submit(&mut self) {
        let line = self.text.trim().to_string();
        if !line.is_empty() {
            self.history.push(line.clone());
            if self.history.len() > MAX_CHAT_HISTORY {
                let excess = self.history.len() - MAX_CHAT_HISTORY;
                self.history.drain(..excess);
            }
            self.scrollback.push(line);
            if self.scrollback.len() > MAX_CHAT_SCROLLBACK {
                let excess = self.scrollback.len() - MAX_CHAT_SCROLLBACK;
                self.scrollback.drain(..excess);
            }
        }
        self.text.clear();
        self.cursor = 0;
        self.history_pos = None;
        self.draft.clear();
        self.completions.clear();
        self.reset_completion();
        self.open = false;
    }
}

// ---------------------------------------------------------------------------
// F3 debug overlay
// ---------------------------------------------------------------------------

/// Structured F3 debug state — the vanilla 1.21.1 two-column screen.
/// Every field is optional so embedders only fill what the backend
/// knows; non-finite numbers and over-long strings are sanitized at
/// render. `enabled` is the F3 toggle.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct F3DebugOverlay {
    pub enabled: bool,
    /// e.g. `"1.21.1"` → renders `Minecraft 1.21.1 (Megu)`.
    pub client_version: Option<String>,
    pub fps: Option<f32>,
    pub frame_ms: Option<f32>,
    /// `C:` line: rendered sections / loaded sections.
    pub visible_sections: Option<u32>,
    pub total_sections: Option<u32>,
    /// `L:` loaded chunk count, when known.
    pub loaded_chunks: Option<u32>,
    /// `D:` render distance in chunks.
    pub render_distance: Option<u32>,
    /// Dimension id (`minecraft:overworld`).
    pub dimension: Option<String>,
    pub position: Option<[f32; 3]>,
    /// Integer block coordinate under the crosshair's feet.
    pub block_pos: Option<[i32; 3]>,
    pub chunk: Option<(i32, i32)>,
    pub section_y: Option<i32>,
    /// Cardinal name (`south`).
    pub facing: Option<String>,
    /// Axis description (`Towards positive Z`).
    pub facing_axis: Option<String>,
    /// `(yaw, pitch)` degrees.
    pub look_deg: Option<[f32; 2]>,
    pub biome: Option<String>,
    pub sky_light: Option<u8>,
    pub block_light: Option<u8>,
    pub entity_count: Option<u32>,
    /// `(x, y, z, block_id)` under the crosshair, when a block is hit.
    pub target_block: Option<(i32, i32, i32, String)>,
    pub target_entity: Option<String>,
    /// Right column: platform/renderer/memory.
    pub os: Option<String>,
    /// `(width, height)` in physical px.
    pub display: Option<(u32, u32)>,
    /// GPU backend name (`Vulkan`, `wgpu`, …).
    pub backend: Option<String>,
    pub gpu: Option<String>,
    pub memory_used_mb: Option<u64>,
    pub memory_total_mb: Option<u64>,
    pub render_scale: Option<f32>,
    pub shadows: Option<bool>,
}

// ---------------------------------------------------------------------------
// Unified snapshot + renderer
// ---------------------------------------------------------------------------

/// One frame of HUD widget state — pure data, cheap to clone.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct HudSnapshot {
    pub vitals: VitalsState,
    pub status_effects: StatusEffectHud,
    pub bossbars: Vec<BossBarEntry>,
    pub scoreboard: ScoreboardSidebar,
    pub title: TitleOverlay,
    pub tab_list: TabPlayerList,
    pub chat: ChatInputOverlay,
    pub debug: F3DebugOverlay,
}

impl HudSnapshot {
    /// A fully populated snapshot exercising every widget — useful for
    /// demos, screenshots, and the non-overlap test matrix.
    pub fn demo() -> Self {
        let mut effects = StatusEffectHud::new();
        for (i, (id, label, beneficial, ticks)) in [
            ("speed", "Speed", true, 1400u32),
            ("strength", "Strength", true, 1100),
            ("regen", "Regeneration", true, 800),
            ("poison", "Poison", false, 500),
            ("wither", "Wither", false, 150),
        ]
        .iter()
        .enumerate()
        {
            effects.push(StatusEffectEntry {
                id: (*id).to_string(),
                label: (*label).to_string(),
                amplifier: (i % 3) as u8,
                remaining_ticks: *ticks,
                beneficial: *beneficial,
            });
        }
        let mut chat = ChatInputOverlay::new();
        chat.open = true;
        chat.text = "/give @s diamond".into();
        chat.cursor = chat.char_count();
        chat.history = vec!["<Steve> hi".into(), "<Alex> yo".into()];
        chat.scrollback = vec![
            "<Steve> hello world".into(),
            "<Alex> hi!".into(),
            "Server: welcome".into(),
        ];
        chat.set_completions(["give", "gamemode", "gamerule"]);
        let mut title = TitleOverlay::default();
        let mut t = TimedText::vanilla("Welcome");
        t.age_ticks = 30;
        title.title = Some(t);
        let mut st = TimedText::vanilla("A subtitle");
        st.age_ticks = 25;
        title.subtitle = Some(st);
        let mut ab = TimedText::vanilla("Demo actionbar");
        ab.age_ticks = 20;
        ab.fade_in_ticks = 5;
        ab.stay_ticks = 60;
        ab.fade_out_ticks = 10;
        title.actionbar = Some(ab);
        let mut tab = TabPlayerList {
            visible: true,
            ..Default::default()
        };
        for i in 0..30 {
            tab.entries.push(TabPlayerEntry::new(
                format!("Player{i:02}"),
                (i as u16 * 37) % 400,
            ));
        }
        let mut scoreboard = ScoreboardSidebar {
            title: Some("Scores".into()),
            entries: Vec::new(),
        };
        for i in 0..12i32 {
            scoreboard.entries.push(ScoreboardEntry {
                label: format!("Player{i}"),
                score: i * 37 % 100 - 20,
            });
        }
        Self {
            vitals: VitalsState {
                health: 14.0,
                max_health: 20.0,
                absorption: 4.0,
                heart_style: HeartStyle::Normal,
                armor: 12,
                food: 17,
                saturation: 9.0,
                air: 240,
                max_air: 300,
                xp_progress: 0.55,
                xp_level: 7,
                show_vitals: true,
            },
            status_effects: effects,
            bossbars: vec![
                BossBarEntry {
                    title: "Ender Dragon".into(),
                    fraction: 0.65,
                    color: BossBarColor::Pink,
                    notches: 0,
                },
                BossBarEntry {
                    title: "Wither".into(),
                    fraction: 0.3,
                    color: BossBarColor::Purple,
                    notches: 10,
                },
            ],
            scoreboard,
            title,
            tab_list: tab,
            chat,
            debug: F3DebugOverlay {
                enabled: true,
                fps: Some(144.0),
                position: Some([123.456, 64.0, -789.012]),
                chunk: Some((7, -12)),
                section_y: Some(4),
                facing: Some("north".into()),
                biome: Some("minecraft:plains".into()),
                sky_light: Some(15),
                block_light: Some(0),
                entity_count: Some(137),
                target_entity: Some("Zombie".into()),
                os: Some("Linux".into()),
                gpu: Some("llvmpipe".into()),
                memory_used_mb: Some(1024),
                memory_total_mb: Some(8192),
                render_scale: Some(2.0),
                shadows: Some(true),
                ..Default::default()
            },
        }
    }
}

/// Bounding box of one widget's emitted region, in *physical* pixels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HudRegion {
    /// Stable widget name (`"vitals.left"`, `"bossbars"`, …).
    pub name: &'static str,
    pub rect: UiRect,
}

/// Every region the last [`HudWidgetsRenderer::render`] claimed.
/// [`is_disjoint`](Self::is_disjoint) is the non-overlap proof: pairwise
/// `UiRect::intersects` over all claims must be false.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct HudRegionReport {
    regions: Vec<HudRegion>,
}

impl HudRegionReport {
    /// All claimed regions in claim order.
    pub fn regions(&self) -> &[HudRegion] {
        &self.regions
    }

    pub fn is_empty(&self) -> bool {
        self.regions.is_empty()
    }

    /// `true` when no two claimed regions share a pixel.
    pub fn is_disjoint(&self) -> bool {
        self.overlaps().is_empty()
    }

    /// Names of region pairs that intersect (empty when disjoint).
    pub fn overlaps(&self) -> Vec<(&'static str, &'static str)> {
        let mut out = Vec::new();
        for (i, a) in self.regions.iter().enumerate() {
            for b in &self.regions[i + 1..] {
                if a.rect.intersects(b.rect) {
                    out.push((a.name, b.name));
                }
            }
        }
        out
    }
}

/// Stateless-per-frame HUD widget renderer. Layout is a pure function of
/// `(snapshot, viewport)`; the only retained setting is `gui_scale`.
#[derive(Clone, Copy, Debug, Default)]
pub struct HudWidgetsRenderer {
    /// Fixed GUI scale `1..=4`; `0` (default) picks the auto scale
    /// (`min(h/360, w/320)` clamped, same formula as `magic_hud`).
    pub gui_scale: u32,
}

impl HudWidgetsRenderer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Renderer pinned to a fixed GUI scale.
    pub fn with_gui_scale(gui_scale: u32) -> Self {
        Self { gui_scale }
    }

    /// Render `snapshot` at `viewport_w`×`viewport_h` **physical** pixels,
    /// appending commands to `out` and returning the region report.
    pub fn render(
        &self,
        snapshot: &HudSnapshot,
        viewport_w: u32,
        viewport_h: u32,
        out: &mut UiDrawList,
    ) -> HudRegionReport {
        let mut report = HudRegionReport::default();
        self.render_into(snapshot, viewport_w, viewport_h, out, &mut report);
        report
    }

    /// [`render`](Self::render) with a caller-owned report buffer: `report`
    /// is cleared and refilled, keeping its heap capacity across frames.
    pub fn render_into(
        &self,
        snapshot: &HudSnapshot,
        viewport_w: u32,
        viewport_h: u32,
        out: &mut UiDrawList,
        report: &mut HudRegionReport,
    ) {
        report.regions.clear();
        let s = if self.gui_scale == 0 {
            (viewport_h / 360)
                .clamp(1, 4)
                .min((viewport_w / 320).max(1))
        } else {
            self.gui_scale.clamp(1, 4)
        };
        let lw = viewport_w / s;
        let lh = viewport_h / s;
        // Degenerate viewports host nothing — below ~96 logical px the
        // top-edge lanes (effects, debug, bossbars) cannot all fit
        // without overlapping, so the frame is skipped entirely.
        if lw < 96 || lh < 96 {
            return;
        }
        let mut ctx = Ctx {
            out,
            s,
            w: lw as i32,
            h: lh as i32,
            claims: Vec::new(),
            top_left_reserve: debug_left_reserve(&snapshot.debug, lw as i32),
        };
        // Claim order = layout priority: top corners first, then the
        // top-center stack, bottom chrome, and finally the widgets that
        // flow around everything already claimed.
        ctx.effects(&snapshot.status_effects);
        ctx.debug(&snapshot.debug);
        ctx.bossbars(&snapshot.bossbars);
        ctx.actionbar(snapshot.title.actionbar.as_ref());
        ctx.vitals(&snapshot.vitals);
        let chat_anchor = ctx.chat_chrome(&snapshot.chat);
        ctx.tab_list(&snapshot.tab_list);
        ctx.title_block(&snapshot.title);
        ctx.scoreboard(&snapshot.scoreboard);
        ctx.chat_scrollback(&snapshot.chat, chat_anchor);
        report.regions = ctx
            .claims
            .iter()
            .map(|(name, r)| HudRegion {
                name,
                rect: ctx.phys(*r),
            })
            .collect();
    }
}

// ---------------------------------------------------------------------------
// Layout context
// ---------------------------------------------------------------------------

/// Per-frame layout state: the draw list, GUI scale, logical viewport,
/// and the claim log (logical-px rects; converted to physical pixels for
/// the report at the end).
struct Ctx<'a> {
    out: &'a mut UiDrawList,
    s: u32,
    w: i32,
    h: i32,
    claims: Vec<(&'static str, UiRect)>,
    /// Logical px reserved at the top-left for the debug column: the
    /// status-effect grid must stay clear of it (computed before any
    /// claims exist, from the snapshot alone).
    top_left_reserve: i32,
}

impl Ctx<'_> {
    /// Logical rect → physical pixels.
    fn phys(&self, r: UiRect) -> UiRect {
        let s = self.s as i32;
        UiRect::new(r.x * s, r.y * s, (r.w as i32 * s) as u32, (r.h as i32 * s) as u32)
    }

    /// Clamp a logical rect into the viewport; `None` when empty.
    fn clamp_lr(&self, x: i32, y: i32, w: i32, h: i32) -> Option<UiRect> {
        let x0 = x.max(0);
        let y0 = y.max(0);
        let x1 = (x + w).min(self.w);
        let y1 = (y + h).min(self.h);
        if x1 <= x0 || y1 <= y0 {
            return None;
        }
        Some(UiRect::new(x0, y0, (x1 - x0) as u32, (y1 - y0) as u32))
    }

    /// Record `name`'s occupied logical rect (clamped; skipped when empty).
    fn claim(&mut self, name: &'static str, x: i32, y: i32, w: i32, h: i32) {
        if let Some(r) = self.clamp_lr(x, y, w, h) {
            self.claims.push((name, r));
        }
    }

    /// Bottom edge of the first claim with `name` (for stacking).
    fn claim_bottom(&self, name: &str) -> Option<i32> {
        self.claims
            .iter()
            .find(|(n, _)| *n == name)
            .map(|(_, r)| r.bottom())
    }

    /// Filled rect, clamped to the viewport.
    fn fill(&mut self, x: i32, y: i32, w: i32, h: i32, color: UiColor) {
        if let Some(r) = self.clamp_lr(x, y, w, h) {
            self.out.fill_rect(self.phys(r), color);
        }
    }

    /// Rect outline (`thickness` logical px), clamped.
    fn border(&mut self, x: i32, y: i32, w: i32, h: i32, color: UiColor, thickness: i32) {
        if let Some(r) = self.clamp_lr(x, y, w, h) {
            self.out.border_rect(
                self.phys(r),
                color,
                (thickness * self.s as i32).max(1) as u32,
            );
        }
    }

    /// Raw text emit at logical scale `m` (cell = `8*m` logical px).
    /// Caller guarantees the string fits; only the y bounds and left
    /// edge are checked — pre-clipped strings are the norm here.
    fn emit(&mut self, x: i32, y: i32, m: u32, color: UiColor, s: &str) {
        let cell = 8 * m as i32;
        if x < 0 || y < 0 || y + cell > self.h || x >= self.w || s.is_empty() {
            return;
        }
        self.out.text_str(x * self.s as i32, y * self.s as i32, m * self.s, color, s);
    }

    /// Left-aligned text clipped to the cells that fit before the
    /// viewport's right edge.
    fn text(&mut self, x: i32, y: i32, m: u32, color: UiColor, s: &str) {
        let cell = 8 * m as i32;
        if x < 0 || y < 0 || y + cell > self.h {
            return;
        }
        let cells = ((self.w - x) / cell).max(0) as usize;
        if cells == 0 {
            return;
        }
        self.emit(x, y, m, color, first_chars(s, cells));
    }

    /// Pre-clipped left-aligned text with a 1-logical-px drop shadow.
    fn shadow(&mut self, x: i32, y: i32, m: u32, color: UiColor, s: &str) {
        self.emit(x + 1, y + 1, m, palette::TEXT_SHADOW, s);
        self.emit(x, y, m, color, s);
    }

    /// Centered text on logical `cx`, clipped to `[EDGE, w-EDGE]`.
    fn ctext(&mut self, cx: i32, y: i32, m: u32, color: UiColor, s: &str) {
        let cell = 8 * m as i32;
        if y < 0 || y + cell > self.h {
            return;
        }
        let cells = (((self.w - 2 * EDGE) / cell).max(0)) as usize;
        let clipped = first_chars(s, cells);
        if clipped.is_empty() {
            return;
        }
        let tw = crate::draw::text_width(clipped, 1) as i32 * cell / 8;
        let x = (cx - tw / 2).clamp(EDGE, (self.w - EDGE - tw).max(EDGE));
        self.shadow(x, y, m, color, clipped);
    }

    /// Progress bar in logical coords.
    fn progress(&mut self, x: i32, y: i32, w: i32, h: i32, fraction: f32, fill: UiColor) {
        if let Some(r) = self.clamp_lr(x, y, w, h) {
            self.out.progress_bar(
                self.phys(r),
                unit_frac(fraction),
                fill,
                palette::SLOT_INNER_DARK,
                palette::BUTTON_BORDER,
            );
        }
    }

    /// `true` when logical rect `r` intersects any earlier claim.
    fn blocked(&self, r: UiRect) -> bool {
        self.claims.iter().any(|(_, c)| c.intersects(r))
    }

    /// Widest claim-free horizontal span overlapping the logical y-band
    /// `[y0, y1)`; prefers the gap containing screen center, else the
    /// widest gap. Used to keep centered widgets inside free lanes.
    fn free_span(&self, y0: i32, y1: i32) -> (i32, i32) {
        let mut iv: Vec<(i32, i32)> = self
            .claims
            .iter()
            .filter(|(_, r)| r.y < y1 && r.bottom() > y0)
            .map(|(_, r)| (r.x, r.right()))
            .collect();
        iv.sort_unstable();
        let mut gaps: Vec<(i32, i32)> = Vec::with_capacity(iv.len() + 1);
        let mut cur = 0i32;
        for (a, b) in iv {
            if a > cur {
                gaps.push((cur, a));
            }
            cur = cur.max(b);
        }
        gaps.push((cur, self.w));
        let cx = self.w / 2;
        gaps.iter()
            .copied()
            .find(|(a, b)| cx >= *a && cx < *b)
            .or_else(|| gaps.iter().copied().max_by_key(|(a, b)| b - a))
            .unwrap_or((cx, cx))
    }

    /// Vertical placement for a `height`-tall block whose x-range is
    /// `[x0, x1)`, centered as close to `center_y` as possible while
    /// avoiding every claim that horizontally overlaps it. Returns the
    /// top y, or `None` when no candidate fits `[lo, hi)`.
    fn fit_vertical(
        &self,
        center_y: i32,
        height: i32,
        x0: i32,
        x1: i32,
        lo: i32,
        hi: i32,
    ) -> Option<i32> {
        if height <= 0 || x1 <= x0 {
            return None;
        }
        let mut cands: Vec<i32> = vec![center_y - height / 2];
        for (_, r) in &self.claims {
            if r.x < x1 && r.right() > x0 {
                cands.push(r.bottom() + 3);
                cands.push(r.y - height - 3);
            }
        }
        cands.retain(|&t| t >= lo && t + height <= hi);
        cands.sort_by_key(|&t| (t + height / 2 - center_y).abs());
        cands.dedup();
        cands.into_iter().find(|&t| {
            let r = UiRect::new(x0, t, (x1 - x0) as u32, height as u32);
            !self.blocked(r)
        })
    }
}

// ---------------------------------------------------------------------------
// Widget: status effects (top-right grid)
// ---------------------------------------------------------------------------

impl Ctx<'_> {
    fn effects(&mut self, hud: &StatusEffectHud) {
        let visible: Vec<&StatusEffectEntry> = hud
            .entries
            .iter()
            .filter(|e| effect_icon_visible(e.remaining_ticks))
            .take(MAX_STATUS_EFFECTS)
            .collect();
        if visible.is_empty() {
            return;
        }
        // The grid hugs the top-right corner but must stay clear of the
        // debug-left lane reserved before any claims existed. Columns
        // shrink to fit; entries past two rows are dropped so the grid
        // never grows taller than the vanilla two-row layout.
        let avail = self.w - 2 * EDGE - self.top_left_reserve;
        let per_row = (avail / EFFECT_CELL_W).clamp(0, STATUS_EFFECTS_PER_ROW as i32) as usize;
        if per_row == 0 {
            return;
        }
        let shown = visible.len().min(per_row * 2);
        let rows = shown.div_ceil(per_row);
        let grid_w = per_row as i32 * EFFECT_CELL_W;
        let grid_h = rows as i32 * EFFECT_CELL_H;
        let x0 = self.w - EDGE - grid_w;
        let y0 = EDGE;
        // +1 right covers the duration text's drop shadow on the last
        // cell (the grid hugs the viewport edge).
        self.claim("status_effects", x0, y0, grid_w + 1, grid_h);
        for (i, entry) in visible.iter().take(shown).enumerate() {
            let col = i % per_row;
            let row = i / per_row;
            let cx = x0 + col as i32 * EFFECT_CELL_W;
            let cy = y0 + row as i32 * EFFECT_CELL_H;
            let tint = if entry.beneficial {
                palette::AIR
            } else {
                palette::DANGER
            };
            // Icon: tinted cell with a lighter core.
            self.fill(cx, cy, 8, 8, tint.with_alpha(220));
            self.fill(cx + 1, cy + 1, 6, 6, palette::TEXT_FOCUSED);
            // Amplifier pips (level II+) along the icon's bottom edge.
            let pips = usize::from(entry.amplifier).min(4);
            for p in 0..pips {
                self.fill(
                    cx + p as i32 * 2,
                    cy + 6,
                    2,
                    2,
                    palette::TEXT,
                );
            }
            let dur = effect_duration_label(entry.remaining_ticks);
            self.shadow(cx + 10, cy + 1, 1, palette::TEXT_DIM, &dur);
        }
    }

    fn debug(&mut self, dbg: &F3DebugOverlay) {
        if !dbg.enabled {
            return;
        }
        let left = debug_left_lines(dbg);
        let right = debug_right_lines(dbg);
        if left.is_empty() && right.is_empty() {
            return;
        }
        // Right column drops below the status-effect grid when both are
        // present (they share the top-right corner).
        let right_y = self
            .claim_bottom("status_effects")
            .map(|b| (b + 2).max(EDGE))
            .unwrap_or(EDGE);
        // Columns never grow into the actionbar band at the bottom edge;
        // on short viewports the tail lines are simply dropped.
        let left_budget =
            ((self.h - ACTIONBAR_LIFT - EDGE) / ROW_H).max(0) as usize;
        let right_budget =
            ((self.h - ACTIONBAR_LIFT - EDGE - right_y) / ROW_H).max(0) as usize;
        // Each column is width-capped so both fit with a gap; lines are
        // additionally cell-clipped at emit time.
        let cap = ((self.w - 2 * EDGE - DEBUG_COL_GAP) / 2 / 8).max(1) as usize;
        let left_lines: Vec<String> = left
            .iter()
            .take(MAX_DEBUG_LINES.min(left_budget))
            .map(|l| clip_cells(l, cap))
            .collect();
        let right_lines: Vec<String> = right
            .iter()
            .take(MAX_DEBUG_LINES.min(right_budget))
            .map(|l| clip_cells(l, cap))
            .collect();
        let left_w = left_lines
            .iter()
            .map(|l| crate::draw::text_width(l, 1) as i32)
            .max()
            .unwrap_or(0);
        let right_w = right_lines
            .iter()
            .map(|l| crate::draw::text_width(l, 1) as i32)
            .max()
            .unwrap_or(0);
        let left_h = left_lines.len() as i32 * ROW_H;
        let right_h = right_lines.len() as i32 * ROW_H;
        for (i, line) in left_lines.iter().enumerate() {
            self.shadow(EDGE, EDGE + i as i32 * ROW_H, 1, palette::TEXT, line);
        }
        if left_w > 0 {
            self.claim("debug.left", EDGE, EDGE, left_w + 2, left_h + 1);
        }
        let rx = self.w - EDGE - right_w;
        for (i, line) in right_lines.iter().enumerate() {
            self.shadow(rx, right_y + i as i32 * ROW_H, 1, palette::TEXT, line);
        }
        if right_w > 0 {
            self.claim("debug.right", rx, right_y, right_w + 2, right_h + 1);
        }
    }
}

/// Left debug column lines (vanilla 1.21.1 order).
fn debug_left_lines(d: &F3DebugOverlay) -> Vec<String> {
    let mut lines = Vec::new();
    if let Some(v) = &d.client_version {
        lines.push(format!("Minecraft {} (Megu)", clip_cells(v, 16)));
    }
    if let Some(fps) = d.fps.filter(|v| v.is_finite() && *v >= 0.0) {
        let mut line = format!("{fps:.0} fps");
        if let Some(ms) = d.frame_ms.filter(|v| v.is_finite() && *v >= 0.0) {
            line.push_str(&format!(" ({ms:.1} ms)"));
        }
        lines.push(line);
    }
    if d.visible_sections.is_some() || d.total_sections.is_some() {
        let mut line = format!(
            "C: {} / {}",
            d.visible_sections
                .map_or("?".to_string(), |v| v.to_string()),
            d.total_sections.map_or("?".to_string(), |v| v.to_string())
        );
        if let Some(dist) = d.render_distance {
            line.push_str(&format!("  D: {dist}"));
        }
        if let Some(loaded) = d.loaded_chunks {
            line.push_str(&format!(", L: {loaded}"));
        }
        lines.push(line);
    } else if let Some(dist) = d.render_distance {
        lines.push(format!("D: {dist}"));
    }
    if let Some(n) = d.entity_count {
        lines.push(format!("E: {n}"));
    }
    if let Some(dim) = &d.dimension {
        lines.push(clip_cells(dim, 40));
    }
    if let Some(p) = d.position {
        if p.iter().all(|v| v.is_finite()) {
            lines.push(format!("XYZ: {:.3} / {:.5} / {:.3}", p[0], p[1], p[2]));
        }
    }
    let block = d.block_pos.or_else(|| {
        d.position.filter(|p| p.iter().all(|v| v.is_finite())).map(|p| {
            [
                p[0].floor() as i32,
                p[1].floor() as i32,
                p[2].floor() as i32,
            ]
        })
    });
    if let Some(b) = block {
        let rel = |v: i32| v.rem_euclid(16);
        lines.push(format!(
            "Block: {} {} {} [{} {} {}]",
            b[0],
            b[1],
            b[2],
            rel(b[0]),
            rel(b[1]),
            rel(b[2])
        ));
    }
    if let Some((cx, cz)) = d.chunk {
        let mut line = format!("Chunk: {cx}");
        if let Some(y) = d.section_y {
            line.push_str(&format!(" {y}"));
        } else {
            line.push_str(" ?");
        }
        lines.push(format!(
            "{line} {cz} in {} {}",
            cx.div_euclid(32),
            cz.div_euclid(32)
        ));
    }
    if d.facing.is_some() || d.look_deg.is_some() {
        let mut line = "Facing:".to_string();
        if let Some(facing) = &d.facing {
            line.push_str(&format!(" {}", clip_cells(facing, 24)));
        }
        if let Some(axis) = &d.facing_axis {
            line.push_str(&format!(" ({})", clip_cells(axis, 28)));
        }
        if let Some([yaw, pitch]) = d.look_deg {
            if yaw.is_finite() && pitch.is_finite() {
                line.push_str(&format!(" ({yaw:.1} / {pitch:.1})"));
            }
        }
        lines.push(line);
    }
    if d.sky_light.is_some() || d.block_light.is_some() {
        let sky = d.sky_light.unwrap_or(0);
        let block = d.block_light.unwrap_or(0);
        lines.push(format!(
            "Client Light: {} ({} sky, {} block)",
            sky.max(block),
            d.sky_light.map_or("?".to_string(), |v| v.to_string()),
            d.block_light.map_or("?".to_string(), |v| v.to_string())
        ));
    }
    if let Some(biome) = &d.biome {
        lines.push(format!("Biome: {}", clip_cells(biome, 40)));
    }
    if let Some((x, y, z, id)) = &d.target_block {
        lines.push(format!(
            "Targeted Block: {x}, {y}, {z} ({})",
            clip_cells(id, 40)
        ));
    }
    if let Some(target) = &d.target_entity {
        lines.push(format!(
            "Targeted Entity: {}",
            clip_cells(target, 32)
        ));
    }
    lines
}

/// Right debug column lines: platform / GPU / memory / render settings.
fn debug_right_lines(d: &F3DebugOverlay) -> Vec<String> {
    let mut lines = Vec::new();
    match &d.os {
        Some(os) => lines.push(format!("Megu Native Rust ({})", clip_cells(os, 24))),
        None => lines.push("Megu Native Rust".to_string()),
    }
    if d.memory_used_mb.is_some() || d.memory_total_mb.is_some() {
        lines.push(format!(
            "Mem: {} / {} MB",
            d.memory_used_mb.map_or("?".to_string(), |v| v.to_string()),
            d.memory_total_mb.map_or("?".to_string(), |v| v.to_string())
        ));
    }
    if let Some((w, h)) = d.display {
        let mut line = format!("Display: {w}x{h}");
        if let Some(backend) = &d.backend {
            line.push_str(&format!(" ({})", clip_cells(backend, 16)));
        }
        lines.push(line);
    }
    if let Some(gpu) = &d.gpu {
        lines.push(format!("GPU: {}", clip_cells(gpu, 40)));
    }
    if let Some(scale) = d.render_scale.filter(|v| v.is_finite() && *v > 0.0) {
        lines.push(format!("Render Scale: {scale:.2}"));
    }
    if let Some(shadows) = d.shadows {
        lines.push(format!("Shadows: {}", if shadows { "on" } else { "off" }));
    }
    lines
}

/// Logical px reserved at the top-left edge for the debug column's
/// claim (shadow pad + inter-column gap), or 0 when the column will
/// not render. Computed before any claims exist so the status-effect
/// grid can keep clear; it mirrors `Ctx::debug`'s width math, so it
/// may overestimate (height-truncated tail lines) but never
/// underestimate the drawn claim.
fn debug_left_reserve(d: &F3DebugOverlay, w: i32) -> i32 {
    if !d.enabled {
        return 0;
    }
    let left = debug_left_lines(d);
    if left.is_empty() {
        return 0;
    }
    let cap = ((w - 2 * EDGE - DEBUG_COL_GAP) / 2 / 8).max(1) as usize;
    let cells = left
        .iter()
        .take(MAX_DEBUG_LINES)
        .map(|l| crate::draw::text_cells(&clip_cells(l, cap)))
        .max()
        .unwrap_or(0);
    if cells == 0 {
        0
    } else {
        cells as i32 * 8 + 2 + DEBUG_COL_GAP
    }
}

// ---------------------------------------------------------------------------
// Widget: bossbars (top-center stack)
// ---------------------------------------------------------------------------

impl Ctx<'_> {
    fn bossbars(&mut self, bars: &[BossBarEntry]) {
        let bars: Vec<&BossBarEntry> = bars.iter().take(MAX_BOSSBARS).collect();
        if bars.is_empty() {
            return;
        }
        let n = bars.len() as i32;
        let stack_h = n * BOSS_PITCH;
        let (sx0, sx1) = self.free_span(EDGE, EDGE + stack_h);
        let bw = (sx1 - sx0 - 8).min(BOSS_W_MAX);
        if bw < 24 {
            return;
        }
        let cx = self.w / 2;
        let bar_x = (cx - bw / 2).clamp(sx0 + 2, (sx1 - 2 - bw).max(sx0 + 2));
        let name_cells = ((bw - 4) / 8).max(1) as usize;
        for (i, bar) in bars.iter().enumerate() {
            let y = EDGE + i as i32 * BOSS_PITCH;
            let title = clip_cells(&bar.title, name_cells);
            // Center the name over the bar (not the viewport) so it
            // stays inside the free span.
            let tw = crate::draw::text_width(&title, 1) as i32;
            let tx = (bar_x + bw / 2 - tw / 2).clamp(bar_x + 1, (bar_x + bw - 1 - tw).max(bar_x + 1));
            self.shadow(tx, y, 1, palette::TEXT, &title);
            let by = y + 9;
            self.progress(bar_x, by, bw, 5, bar.fraction, bar.color.color());
            // Division notches: evenly spaced 1-px ticks inside the bar.
            let notches = usize::from(bar.notches).min(20);
            for k in 1..=notches {
                let nx = bar_x + (bw as i64 * k as i64 / (notches as i64 + 1)) as i32;
                self.fill(nx, by, 1, 5, palette::SLOT_INNER_DARK);
            }
        }
        self.claim("bossbars", bar_x, EDGE, bw, stack_h);
    }
}

// ---------------------------------------------------------------------------
// Widget: vitals (bottom-center, above the hotbar reservation)
// ---------------------------------------------------------------------------

/// One 8×8 icon cell: dark backing plus a left-to-right fill of `frac`.
fn icon_cell(ctx: &mut Ctx<'_>, x: i32, y: i32, frac: f32, color: UiColor) {
    ctx.fill(x, y, 8, 8, ICON_BACK);
    let fw = (8.0 * unit_frac(frac)) as i32;
    if fw > 0 {
        ctx.fill(x, y, fw, 8, color);
    }
}

/// `ceil(value / per_icon)` icons, clamped to `0..=max_icons`.
fn icon_count(value: f32, per_icon: f32, max_icons: i32) -> i32 {
    if !value.is_finite() || value <= 0.0 {
        return 0;
    }
    ((value / per_icon).ceil() as i32).clamp(0, max_icons)
}

impl Ctx<'_> {
    fn vitals(&mut self, v: &VitalsState) {
        let hotbar_top = self.h - HOTBAR_RESERVE;
        let cx = self.w / 2;
        let hb_left = cx - HOTBAR_W / 2;
        let hb_right = hb_left + HOTBAR_W;
        let mut left_top = i32::MAX;
        let mut left_bottom = i32::MIN;
        let mut left_x0 = i32::MAX;
        let mut right_top = i32::MAX;
        let mut right_bottom = i32::MIN;
        let mut right_x1 = i32::MIN;

        if v.show_vitals {
            // --- Left column: hearts (+absorption), armor -------------
            let max_health = {
                let m = finite_or(v.max_health, 20.0);
                if m > 0.0 {
                    m
                } else {
                    20.0
                }
            };
            let health = finite_or(v.health, 0.0).clamp(0.0, max_health);
            let absorption = finite_or(v.absorption, 0.0).max(0.0);
            let heart_icons = icon_count(max_health, 2.0, 10);
            let abs_icons = icon_count(absorption, 2.0, 10);
            let hearts_y = hotbar_top - 16;
            if heart_icons + abs_icons > 0 {
                let row_right = hb_left - 4;
                let total = heart_icons + abs_icons;
                let row_x = row_right - total * 8;
                for i in 0..heart_icons {
                    let frac = ((health - i as f32 * 2.0) / 2.0).clamp(0.0, 1.0);
                    icon_cell(self, row_x + i * 8, hearts_y, frac, v.heart_style.color());
                }
                for i in 0..abs_icons {
                    let frac = ((absorption - i as f32 * 2.0) / 2.0).clamp(0.0, 1.0);
                    icon_cell(
                        self,
                        row_x + (heart_icons + i) * 8,
                        hearts_y,
                        frac,
                        palette::ABSORPTION,
                    );
                }
                left_top = left_top.min(hearts_y);
                left_bottom = left_bottom.max(hearts_y + 8);
                left_x0 = left_x0.min(row_x);
            }
            // Armor row: 2 points per icon, proportional (empty at 0).
            let armor = f32::from(v.armor.min(20));
            let armor_icons = icon_count(armor, 2.0, 10);
            if armor_icons > 0 {
                let armor_y = hotbar_top - 26;
                let row_right = hb_left - 4;
                let row_x = row_right - armor_icons * 8;
                for i in 0..armor_icons {
                    let frac = ((armor - i as f32 * 2.0) / 2.0).clamp(0.0, 1.0);
                    icon_cell(self, row_x + i * 8, armor_y, frac, palette::ARMOR);
                }
                left_top = left_top.min(armor_y);
                left_bottom = left_bottom.max(armor_y + 8);
                left_x0 = left_x0.min(row_x);
            }

            // --- Right column: food (+saturation), air ---------------
            let food_y = hotbar_top - 16;
            let food = f32::from(v.food.min(20));
            let food_icons = icon_count(food, 2.0, 10);
            if food_icons > 0 {
                let row_x = hb_right + 4;
                let saturation = finite_or(v.saturation, 0.0).max(0.0);
                for i in 0..food_icons {
                    let x = row_x + i * 8;
                    let frac = ((food - i as f32 * 2.0) / 2.0).clamp(0.0, 1.0);
                    icon_cell(self, x, food_y, frac, palette::HUNGER);
                    // Saturation overlay: same half-unit scale as food.
                    let sat = ((saturation - i as f32 * 2.0) / 2.0).clamp(0.0, 1.0);
                    let sw = (8.0 * sat) as i32;
                    if sw > 0 {
                        self.fill(x, food_y + 6, sw, 2, palette::TEXT);
                    }
                }
                right_top = right_top.min(food_y);
                right_bottom = right_bottom.max(food_y + 8);
                right_x1 = right_x1.max(row_x + food_icons * 8);
            }
            // Air bubbles: only while the supply is depleted (vanilla
            // hides a full bar), 30 units per bubble.
            let max_air = u32::from(v.max_air);
            let air = u32::from(v.air).min(max_air);
            if max_air > 0 && air < max_air {
                let bubbles = max_air.div_ceil(30).min(10) as i32;
                let air_y = hotbar_top - 26;
                let row_x = hb_right + 4;
                for i in 0..bubbles {
                    let frac =
                        ((air as i32 - i * 30).max(0) as f32 / 30.0).clamp(0.0, 1.0);
                    icon_cell(self, row_x + i * 8, air_y, frac, palette::AIR);
                }
                right_top = right_top.min(air_y);
                right_bottom = right_bottom.max(air_y + 8);
                right_x1 = right_x1.max(row_x + bubbles * 8);
            }
        }

        if left_top != i32::MAX {
            self.claim(
                "vitals.left",
                left_x0,
                left_top,
                hb_left - 4 - left_x0,
                left_bottom - left_top,
            );
        }
        if right_top != i32::MAX {
            self.claim(
                "vitals.right",
                hb_right + 4,
                right_top,
                right_x1 - (hb_right + 4),
                right_bottom - right_top,
            );
        }

        // XP bar + level: drawn in every game mode that has XP, above
        // the hotbar reservation.
        let xp_frac = unit_frac(v.xp_progress);
        let show_xp = v.xp_level > 0 || xp_frac > 0.0;
        if show_xp {
            let bar_y = hotbar_top - 5;
            let bar_x = cx - XP_BAR_W / 2;
            self.progress(bar_x, bar_y, XP_BAR_W, 3, xp_frac, palette::XP);
            let mut x0 = bar_x;
            let mut x1 = bar_x + XP_BAR_W;
            let mut y0 = bar_y;
            let mut y1 = bar_y + 3;
            if v.xp_level > 0 {
                let level = v.xp_level.to_string();
                let tw = crate::draw::text_width(&level, 1) as i32;
                let tx = cx - tw / 2;
                let ty = bar_y - 9;
                self.text(tx, ty, 1, palette::XP, &level);
                x0 = x0.min(tx);
                x1 = x1.max(tx + tw);
                y0 = y0.min(ty);
                y1 = y1.max(ty + 8);
            }
            self.claim("vitals.xp", x0, y0, x1 - x0, y1 - y0);
        }
    }
}

// ---------------------------------------------------------------------------
// Widget: title / subtitle / actionbar
// ---------------------------------------------------------------------------

impl Ctx<'_> {
    /// Title + subtitle block: vertically centered around `h/3`, moved
    /// below any claim it would intersect (e.g. a tall tab list).
    fn title_block(&mut self, overlay: &TitleOverlay) {
        let title = overlay.title.as_ref().filter(|t| t.is_visible());
        let subtitle = overlay.subtitle.as_ref().filter(|t| t.is_visible());
        if title.is_none() && subtitle.is_none() {
            return;
        }
        let title_h = if title.is_some() { 16 } else { 0 };
        let gap = if title.is_some() && subtitle.is_some() { 4 } else { 0 };
        let sub_h = if subtitle.is_some() { 8 } else { 0 };
        let block_h = title_h + gap + sub_h;
        // Width in *pixels*: title is scale-2 (16 px cells), subtitle
        // scale-1 — clip each to its own cell budget first.
        let title_cells = ((self.w - 2 * EDGE) / 16).max(0) as usize;
        let sub_cells = ((self.w - 2 * EDGE) / 8).max(0) as usize;
        let block_w = title
            .map(|t| crate::draw::text_cells(&clip_cells(&t.text, title_cells)) * 16)
            .unwrap_or(0)
            .max(
                subtitle
                    .map(|t| crate::draw::text_cells(&clip_cells(&t.text, sub_cells)) * 8)
                    .unwrap_or(0),
            ) as i32;
        let cx = self.w / 2;
        let center_y = self.h / 3;
        let x0 = (cx - block_w / 2).max(0);
        let x1 = (cx + block_w / 2).min(self.w);
        let Some(top) = self.fit_vertical(center_y, block_h, x0, x1, EDGE, self.h - EDGE) else {
            return;
        };
        let mut y = top;
        if let Some(t) = title {
            let alpha = (t.alpha() * 255.0) as u8;
            self.ctext(cx, y, 2, palette::TEXT.with_alpha(alpha), &clip_cells(&t.text, title_cells));
            y += title_h + gap;
        }
        if let Some(t) = subtitle {
            let alpha = (t.alpha() * 255.0) as u8;
            self.ctext(cx, y, 1, palette::TEXT.with_alpha(alpha), &clip_cells(&t.text, sub_cells));
        }
        // +1 covers the drop shadow's right/bottom pixel.
        self.claim("title", x0, top, block_w + 1, block_h + 1);
    }

    /// Actionbar: one centered line in the band above the vitals.
    fn actionbar(&mut self, actionbar: Option<&TimedText>) {
        let Some(t) = actionbar.filter(|t| t.is_visible()) else {
            return;
        };
        let cells = ((self.w - 2 * EDGE) / 8).max(0) as usize;
        let text = clip_cells(&t.text, cells);
        if text.is_empty() {
            return;
        }
        let alpha = (t.alpha() * 255.0) as u8;
        let tw = crate::draw::text_width(&text, 1) as i32;
        let cx = self.w / 2;
        let y = self.h - ACTIONBAR_LIFT;
        let x = (cx - tw / 2).clamp(EDGE, (self.w - EDGE - tw).max(EDGE));
        self.shadow(x, y, 1, palette::TEXT.with_alpha(alpha), &text);
        // +1 covers the drop shadow's right/bottom pixel.
        self.claim("actionbar", x, y, tw + 1, 9);
    }
}

// ---------------------------------------------------------------------------
// Widget: tab player list
// ---------------------------------------------------------------------------

/// Vanilla 5-bar latency indicator (same thresholds as `layout.rs`).
fn ping_bars(ctx: &mut Ctx<'_>, x: i32, y: i32, ping_ms: u16) {
    let lit = match ping_ms {
        0..=60 => 5,
        61..=120 => 4,
        121..=160 => 3,
        161..=300 => 2,
        _ => 1,
    };
    for i in 0..5i32 {
        let bh = 2 + i;
        let color = if i < lit {
            palette::ACCENT
        } else {
            palette::TEXT_DARK
        };
        ctx.fill(x + i * 5, y + 8 - bh, 3, bh, color);
    }
}

impl Ctx<'_> {
    fn tab_list(&mut self, list: &TabPlayerList) {
        if !list.visible {
            return;
        }
        let entries: Vec<&TabPlayerEntry> = list
            .entries
            .iter()
            .filter(|e| e.listed)
            .take(MAX_TAB_ENTRIES)
            .collect();
        if entries.is_empty() {
            return;
        }
        let n = entries.len();
        let mut cols = n.div_ceil(TAB_ROWS_PER_COLUMN).clamp(1, MAX_TAB_COLUMNS);
        let mut rows = n.div_ceil(cols).min(TAB_ROWS_PER_COLUMN);
        let tab_top = self
            .claim_bottom("bossbars")
            .map(|b| b + 4)
            .unwrap_or(EDGE);
        // Width budget: the free horizontal span across the tallest
        // possible grid band, minus breathing room.
        let band_bot = (tab_top + TAB_ROWS_PER_COLUMN as i32 * ROW_H + 4).min(self.h);
        let (sx0, sx1) = self.free_span(tab_top, band_bot);
        let span_w = sx1 - sx0;
        if span_w < 48 {
            return;
        }
        // Widest sanitized name (in cells) per column under a
        // `rows`-tall column-major assignment over the first `limit`
        // entries (entries past `limit` are not displayed).
        let name_cells_for = |cols: usize, rows: usize, limit: usize| -> i32 {
            let mut widest = 0i32;
            for c in 0..cols {
                let mut m = 0i32;
                for (i, e) in entries.iter().enumerate().take(limit) {
                    if i / rows == c {
                        let cells = crate::draw::text_cells(&clip_cells(&e.name, MAX_LABEL_CELLS)) as i32;
                        m = m.max(cells);
                    }
                }
                widest = widest.max(m);
            }
            widest
        };
        let span_budget = span_w - 4;
        let mut name_cells = name_cells_for(cols, rows, n);
        // Shrink columns until the grid fits the free span.
        loop {
            let allowed = ((span_budget / cols as i32 - TAB_COL_EXTRA) / 8).max(1);
            name_cells = name_cells.min(allowed);
            let total_w = (name_cells * 8 + TAB_COL_EXTRA) * cols as i32;
            if total_w <= span_budget || cols <= 1 {
                break;
            }
            cols -= 1;
            rows = n.div_ceil(cols).min(TAB_ROWS_PER_COLUMN);
            name_cells = name_cells_for(cols, rows, n);
        }
        let mut col_w = name_cells * 8 + TAB_COL_EXTRA;
        let mut total_w = col_w * cols as i32;
        if total_w > span_budget {
            return;
        }
        // Rows also yield to claims below (chat input, popup, …): cap the
        // shown count so the grid's bottom stays above them.
        let tab_x = (self.w / 2 - total_w / 2).clamp(sx0 + 2, (sx1 - 2 - total_w).max(sx0 + 2));
        let mut bottom_limit = self.h - EDGE;
        for (_, r) in &self.claims {
            if r.bottom() > tab_top && r.x < tab_x + total_w && r.right() > tab_x {
                bottom_limit = bottom_limit.min(r.y - 3);
            }
        }
        let max_rows = ((bottom_limit - tab_top) / ROW_H).max(0) as usize;
        let shown = n.min(cols * rows).min(cols * max_rows);
        if shown == 0 {
            return;
        }
        rows = shown.div_ceil(cols);
        // Recompute the name budget for the final row count and shown
        // entries, then re-fit the width inside the span.
        name_cells = name_cells_for(cols, rows, shown);
        let allowed = ((span_budget / cols as i32 - TAB_COL_EXTRA) / 8).max(1);
        name_cells = name_cells.min(allowed);
        col_w = name_cells * 8 + TAB_COL_EXTRA;
        total_w = col_w * cols as i32;
        if total_w > span_budget {
            return;
        }
        let tab_x = (self.w / 2 - total_w / 2).clamp(sx0 + 2, (sx1 - 2 - total_w).max(sx0 + 2));
        let grid_h = rows as i32 * ROW_H;
        for (i, entry) in entries.iter().enumerate().take(shown) {
            let c = i / rows;
            let r = i % rows;
            let x = tab_x + c as i32 * col_w;
            let y = tab_top + r as i32 * ROW_H;
            let name = clip_cells(&entry.name, name_cells as usize);
            self.shadow(x + 2, y + 1, 1, palette::TEXT, &name);
            ping_bars(self, x + col_w - 25, y + 1, entry.ping_ms);
        }
        self.claim("tab_list", tab_x, tab_top, total_w, grid_h);
    }
}

// ---------------------------------------------------------------------------
// Widget: scoreboard sidebar (right-center)
// ---------------------------------------------------------------------------

impl Ctx<'_> {
    fn scoreboard(&mut self, sb: &ScoreboardSidebar) {
        let has_title = sb
            .title
            .as_ref()
            .is_some_and(|t| !clip_cells(t, MAX_LABEL_CELLS).is_empty());
        let mut rows: Vec<&ScoreboardEntry> = sb.entries.iter().collect();
        rows.sort_by(|a, b| b.score.cmp(&a.score).then_with(|| a.label.cmp(&b.label)));
        rows.truncate(MAX_SCOREBOARD_ROWS);
        if !has_title && rows.is_empty() {
            return;
        }
        let label_cells = rows
            .iter()
            .map(|e| crate::draw::text_cells(&clip_cells(&e.label, MAX_LABEL_CELLS)) as i32)
            .max()
            .unwrap_or(0);
        let score_cells = rows
            .iter()
            .map(|e| crate::draw::text_cells(&e.score.to_string()) as i32)
            .max()
            .unwrap_or(0);
        let title_cells = if has_title {
            clip_cells(sb.title.as_deref().unwrap_or(""), MAX_LABEL_CELLS)
                .chars()
                .count() as i32
        } else {
            0
        };
        // Row needs label + gap + right-aligned score; title needs its
        // own width. Take the max, then cap to the viewport.
        let content_cells = (label_cells + score_cells + 2).max(title_cells);
        let width = (content_cells * 8 + 8).min(self.w - 2 * EDGE);
        if width < 24 {
            return;
        }
        let x = self.w - EDGE - width;
        let max_rows = rows.len();
        for shown in (0..=max_rows).rev() {
            if shown == 0 && !has_title {
                break;
            }
            let title_h = if has_title { ROW_H } else { 0 };
            let height = 4 + title_h + shown as i32 * ROW_H + 2;
            let Some(top) =
                self.fit_vertical(self.h / 2, height, x, self.w - EDGE, EDGE, self.h - EDGE)
            else {
                continue;
            };
            self.fill(x, top, width, height, palette::TOOLTIP_BG);
            self.border(x, top, width, height, palette::PANEL_BORDER, 1);
            let mut y = top + 2;
            if has_title {
                let title = clip_cells(
                    sb.title.as_deref().unwrap_or(""),
                    ((width - 8) / 8).max(1) as usize,
                );
                self.emit(x + 4, y, 1, palette::TEXT, &title);
                y += ROW_H;
            }
            for entry in rows.iter().take(shown) {
                let label_cap = (((width - 8) / 8) - score_cells - 1).max(1) as usize;
                let label = clip_cells(&entry.label, label_cap);
                self.emit(x + 4, y, 1, palette::TEXT, &label);
                let score = entry.score.to_string();
                let sw = crate::draw::text_width(&score, 1) as i32;
                self.emit(x + width - 4 - sw, y, 1, palette::TEXT_DIM, &score);
                y += ROW_H;
            }
            self.claim("scoreboard", x, top, width, height);
            return;
        }
    }
}

// ---------------------------------------------------------------------------
// Widget: chat input, completion popup, scrollback
// ---------------------------------------------------------------------------

impl Ctx<'_> {
    /// Chat input box + completion popup. Returns the y the scrollback
    /// stack should hang from (`None` when nothing chat-related drew).
    fn chat_chrome(&mut self, chat: &ChatInputOverlay) -> Option<i32> {
        let mut anchor = None;
        if chat.open {
            let hb_left = self.w / 2 - HOTBAR_W / 2;
            let box_w = CHAT_INPUT_W_MAX
                .min(hb_left - EDGE - 4)
                .min(self.w - 2 * EDGE);
            if box_w >= 40 {
                let box_x = EDGE;
                let box_y = self.h - ACTIONBAR_LIFT - 2 - CHAT_INPUT_H;
                let box_h = CHAT_INPUT_H;
                self.fill(box_x, box_y, box_w, box_h, palette::TOOLTIP_BG);
                self.border(box_x, box_y, box_w, box_h, palette::PANEL_BORDER, 1);
                let text = clip_cells(&chat.text, ((box_w - 6) / 8).max(0) as usize);
                self.emit(box_x + 3, box_y + 1, 1, palette::TEXT, &text);
                // Caret: 1-px bar at the cursor's cell, clamped inside.
                let caret_x = (box_x + 3 + chat.cursor() as i32 * 8).min(box_x + box_w - 2);
                self.fill(caret_x, box_y + 2, 1, 6, palette::TEXT);
                self.claim("chat.input", box_x, box_y, box_w, box_h);
                anchor = Some(box_y - 2);
            }
            // Completion popup directly above the input box (or above
            // the actionbar band when the box itself did not fit).
            if !chat.completions.is_empty() {
                let bottom = anchor.unwrap_or(self.h - ACTIONBAR_LIFT - 2);
                // Popup height is rows*ROW_H + 2, so the row budget must
                // leave room for the 2-px chrome inside the viewport.
                let avail_rows = ((bottom - EDGE - 2) / ROW_H).max(0) as usize;
                let rows = chat
                    .completions
                    .len()
                    .min(MAX_CHAT_COMPLETION_ROWS)
                    .min(avail_rows);
                if rows > 0 {
                    let selected = chat.selected_completion();
                    let mut widest = 0i32;
                    for cand in chat.completions.iter().take(rows) {
                        widest =
                            widest.max(crate::draw::text_cells(&clip_cells(cand, MAX_LABEL_CELLS)) as i32);
                    }
                    let pop_w = (widest * 8 + 8).min(self.w - 2 * EDGE);
                    // Drop rows until the popup clears every earlier
                    // claim (debug columns, vitals on tiny screens).
                    let mut rows = rows;
                    loop {
                        let h = rows as i32 * ROW_H + 2;
                        let t = bottom - h;
                        let r = UiRect::new(EDGE, t, pop_w as u32, h as u32);
                        if rows == 0 || !self.blocked(r) {
                            break;
                        }
                        rows -= 1;
                    }
                    if rows > 0 {
                        let pop_h = rows as i32 * ROW_H + 2;
                        let top = bottom - pop_h;
                        self.fill(EDGE, top, pop_w, pop_h, palette::TOOLTIP_BG);
                        self.border(EDGE, top, pop_w, pop_h, palette::PANEL_BORDER, 1);
                        for (i, cand) in chat.completions.iter().take(rows).enumerate() {
                            let y = top + 1 + i as i32 * ROW_H;
                            if Some(i) == selected {
                                self.fill(
                                    EDGE + 1,
                                    y,
                                    pop_w - 2,
                                    ROW_H - 1,
                                    palette::BUTTON_HOVER,
                                );
                            }
                            let label =
                                clip_cells(cand, ((pop_w - 8) / 8).max(1) as usize);
                            self.emit(EDGE + 4, y + 1, 1, palette::TEXT, &label);
                        }
                        self.claim("chat.completion", EDGE, top, pop_w, pop_h);
                        anchor = Some(top - 2);
                    }
                }
            }
        }
        anchor
    }

    /// Scrollback lines stacked upward from `anchor` (the popup top, or
    /// the input top when no popup drew), each width-capped around
    /// earlier claims.
    fn chat_scrollback(&mut self, chat: &ChatInputOverlay, anchor: Option<i32>) {
        if chat.scrollback.is_empty() {
            return;
        }
        let bottom = anchor.unwrap_or(self.h - ACTIONBAR_LIFT - 14);
        for (i, line) in chat
            .scrollback
            .iter()
            .rev()
            .take(MAX_CHAT_SCROLLBACK_LINES)
            .enumerate()
        {
            let y = bottom - ROW_H * (i as i32 + 1);
            if y < EDGE {
                break;
            }
            let label = clip_cells(line, MAX_LABEL_CELLS);
            if label.is_empty() {
                continue;
            }
            // Width available before the nearest claim in this row's
            // band (backdrop spans y-1..y+9); claims covering the left
            // edge force a skip.
            let mut allowed = self.w - EDGE - 6;
            let mut blocked = false;
            for (_, r) in &self.claims {
                if r.y < y + ROW_H && r.bottom() > y - 1 && r.x < EDGE + allowed && r.right() > EDGE
                {
                    if r.x <= EDGE + 2 {
                        blocked = true;
                        break;
                    }
                    allowed = allowed.min(r.x - EDGE - 4);
                }
            }
            if blocked || allowed < 8 {
                continue;
            }
            let cells = ((allowed - 6) / 8).max(0) as usize;
            let text = first_chars(&label, cells);
            if text.is_empty() {
                continue;
            }
            let tw = crate::draw::text_width(text, 1) as i32;
            self.fill(EDGE - 2, y - 1, tw + 6, ROW_H, ROW_BACK);
            self.emit(EDGE, y, 1, palette::TEXT, text);
            self.claim("chat.line", EDGE - 2, y - 1, tw + 6, ROW_H);
        }
    }
}
