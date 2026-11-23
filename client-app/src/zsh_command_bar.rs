// SPDX-License-Identifier: LGPL-3.0-or-later
//! Zsh/fzf-style client command bar with inline completions, history,
//! ghost-text suggestions, syntax highlighting, and deterministic 8x8 badge
//! glyphs for command candidates.
//!
//! The module is intentionally pure `std`: the windowing layer owns key
//! events and atlas sprites, while this type owns editing state, ranking,
//! command syntax, and clean-room badge generation.

use std::collections::{BTreeSet, VecDeque};

use megu_kernel::command::CommandDispatcher;

/// Maximum number of submitted command lines retained by the bar.
pub const COMMAND_HISTORY_CAPACITY: usize = 200;
/// Maximum number of ranked completion candidates returned at once.
pub const MAX_COMPLETION_CANDIDATES: usize = 96;
/// Badge glyph edge length in pixels.
pub const GLYPH_SIZE: usize = 8;
/// Number of palette-indexed pixels in an 8x8 badge glyph.
pub const GLYPH_PIXELS: usize = GLYPH_SIZE * GLYPH_SIZE;

/// Side of the completion row on which a badge is rendered.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum BadgeSide {
    Left,
    Right,
}

/// Visual family for an inline completion badge.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum BadgeKind {
    MobFace,
    ItemSprite,
    BlockSwatch,
    EnchantBook,
    StatusEffect,
    Semantic,
}

/// Coarse mob category used for the right-side completion tag.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MobCategory {
    Hostile,
    Passive,
    Boss,
    Water,
    Ambient,
    Other,
}

impl MobCategory {
    /// Right-side tag shown next to a mob candidate.
    pub fn tag(self) -> &'static str {
        match self {
            Self::Hostile => "Hostile",
            Self::Passive => "Passive",
            Self::Boss => "Boss",
            Self::Water => "Water",
            Self::Ambient => "Ambient",
            Self::Other => "Mob",
        }
    }

    fn accent(self) -> [u8; 4] {
        match self {
            Self::Hostile => [226, 72, 64, 255],
            Self::Passive => [88, 190, 104, 255],
            Self::Boss => [176, 92, 220, 255],
            Self::Water => [72, 144, 226, 255],
            Self::Ambient => [168, 168, 176, 255],
            Self::Other => [150, 150, 160, 255],
        }
    }
}

/// Deterministic inline badge attached to a completion candidate.
///
/// `glyph_8x8` is palette-indexed (0 is transparent); use
/// [`CompletionIconBadge::glyph_rgba`] to expand it through the badge palette.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompletionIconBadge {
    pub kind: BadgeKind,
    pub sprite_key: String,
    pub accent_rgba: [u8; 4],
    pub glyph_8x8: [u8; GLYPH_PIXELS],
}

impl CompletionIconBadge {
    /// Build a badge and generate its deterministic glyph from `sprite_key`.
    pub fn new(kind: BadgeKind, sprite_key: impl Into<String>, accent_rgba: [u8; 4]) -> Self {
        let sprite_key = sprite_key.into();
        Self::with_glyph_seed(kind, sprite_key.clone(), accent_rgba, &sprite_key)
    }

    fn with_glyph_seed(
        kind: BadgeKind,
        sprite_key: String,
        accent_rgba: [u8; 4],
        glyph_seed: &str,
    ) -> Self {
        Self {
            kind,
            sprite_key,
            accent_rgba,
            glyph_8x8: generate_glyph(kind, glyph_seed),
        }
    }

    /// Expand the palette-indexed glyph to tightly packed RGBA bytes.
    pub fn glyph_rgba(&self) -> Vec<u8> {
        let palette = self.palette_rgba();
        let mut out = Vec::with_capacity(GLYPH_PIXELS * 4);
        for &index in self.glyph_8x8.iter() {
            let rgba = palette[usize::from(index).min(palette.len() - 1)];
            out.extend_from_slice(&rgba);
        }
        out
    }

    /// RGBA palette used by [`CompletionIconBadge::glyph_rgba`].
    pub fn palette_rgba(&self) -> [[u8; 4]; 8] {
        badge_palette(self.kind, &self.sprite_key, self.accent_rgba)
    }
}

/// One ranked command-line completion candidate.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompletionCandidate {
    /// Stable deterministic id used for ranking tie-breaks.
    pub id: String,
    /// Text inserted when the candidate is accepted.
    pub text: String,
    /// Human-facing label rendered beside the badge.
    pub display_label: String,
    /// Side of the row occupied by `badge`.
    pub badge_side: BadgeSide,
    /// Optional inline font-size badge.
    pub badge: Option<CompletionIconBadge>,
    /// Optional right-side tag such as `Hostile`, `Buff`, `Max V`, or `Curse`.
    pub right_tag: Option<String>,
}

impl CompletionCandidate {
    pub fn new(
        id: impl Into<String>,
        text: impl Into<String>,
        display_label: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            text: text.into(),
            display_label: display_label.into(),
            badge_side: BadgeSide::Left,
            badge: None,
            right_tag: None,
        }
    }

    pub fn simple(text: impl Into<String>) -> Self {
        let text = text.into();
        Self::new(text.clone(), text.clone(), text)
    }

    pub fn with_badge_side(mut self, badge_side: BadgeSide) -> Self {
        self.badge_side = badge_side;
        self
    }

    pub fn with_badge(mut self, badge: CompletionIconBadge) -> Self {
        self.badge = Some(badge);
        self
    }

    pub fn with_right_tag(mut self, right_tag: impl Into<String>) -> Self {
        self.right_tag = Some(right_tag.into());
        self
    }
}

/// Syntax role assigned to a highlighted command-line span.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum HighlightKind {
    Command,
    Subcommand,
    TargetSelector,
    Coordinate,
    StringOrNbt,
    Invalid,
}

/// Color paired with [`HighlightKind`] for immediate renderer use.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum HighlightColor {
    Green,
    Cyan,
    Yellow,
    Blue,
    White,
    Red,
}

impl HighlightKind {
    pub fn color(self) -> HighlightColor {
        match self {
            Self::Command => HighlightColor::Green,
            Self::Subcommand => HighlightColor::Cyan,
            Self::TargetSelector => HighlightColor::Yellow,
            Self::Coordinate => HighlightColor::Blue,
            Self::StringOrNbt => HighlightColor::White,
            Self::Invalid => HighlightColor::Red,
        }
    }
}

/// One highlighted byte range of the command line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HighlightedSpan {
    pub start: usize,
    pub end: usize,
    pub kind: HighlightKind,
    pub color: HighlightColor,
    pub text: String,
}

/// Keyboard-level input understood by [`ZshCommandBar`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CommandKey {
    Char(char),
    Backspace,
    Delete,
    Left,
    Right,
    Up,
    Down,
    Home,
    End,
    Enter,
    Escape,
    Tab,
    ShiftTab,
    CtrlA,
    CtrlE,
    CtrlW,
    CtrlU,
    CtrlR,
}

impl CommandKey {
    /// Convert a terminal-style input character to a command-bar key.
    pub fn from_char(ch: char) -> Option<Self> {
        match ch {
            '\t' => Some(Self::Tab),
            '\n' | '\r' => Some(Self::Enter),
            '\u{7f}' => Some(Self::Backspace),
            '\u{1}' => Some(Self::CtrlA),
            '\u{5}' => Some(Self::CtrlE),
            '\u{17}' => Some(Self::CtrlW),
            '\u{15}' => Some(Self::CtrlU),
            '\u{12}' => Some(Self::CtrlR),
            '\u{1b}' => Some(Self::Escape),
            ch if !ch.is_control() => Some(Self::Char(ch)),
            _ => None,
        }
    }
}

/// Result of handling one [`CommandKey`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CommandBarAction {
    Changed,
    Submitted(String),
    Completed { index: usize, text: String },
    GhostAccepted,
    ReverseSearchStarted,
    ReverseSearchChanged,
    ReverseSearchFinished,
    ReverseSearchCancelled,
    Ignored,
}

/// Deterministic badge/icon factory used by the command bar.
pub struct CompletionBadgeCatalog;

impl CompletionBadgeCatalog {
    pub fn badge(
        kind: BadgeKind,
        sprite_key: impl Into<String>,
        accent_rgba: [u8; 4],
    ) -> CompletionIconBadge {
        CompletionIconBadge::new(kind, sprite_key, accent_rgba)
    }

    pub fn mob_badge(id: impl AsRef<str>) -> CompletionIconBadge {
        let id = id.as_ref();
        let category = mob_category(id);
        CompletionIconBadge::new(
            BadgeKind::MobFace,
            sprite_key(BadgeKind::MobFace, id),
            category.accent(),
        )
    }

    pub fn item_badge(id: impl AsRef<str>) -> CompletionIconBadge {
        let id = id.as_ref();
        CompletionIconBadge::new(
            BadgeKind::ItemSprite,
            sprite_key(BadgeKind::ItemSprite, id),
            item_accent(id),
        )
    }

    pub fn effect_badge(id: impl AsRef<str>) -> CompletionIconBadge {
        let id = id.as_ref();
        let accent = if effect_is_debuff(id) {
            [204, 72, 72, 255]
        } else {
            [96, 190, 112, 255]
        };
        CompletionIconBadge::new(
            BadgeKind::StatusEffect,
            sprite_key(BadgeKind::StatusEffect, id),
            accent,
        )
    }

    pub fn enchant_badge(id: impl AsRef<str>, curse: bool) -> CompletionIconBadge {
        let id = id.as_ref();
        let accent = if curse {
            [216, 72, 72, 255]
        } else {
            [188, 132, 236, 255]
        };
        CompletionIconBadge::with_glyph_seed(
            BadgeKind::EnchantBook,
            "item/enchanted_book".to_string(),
            accent,
            &format!("enchantment/{id}"),
        )
    }

    pub fn block_badge(id: impl AsRef<str>) -> CompletionIconBadge {
        let id = id.as_ref();
        CompletionIconBadge::new(
            BadgeKind::BlockSwatch,
            sprite_key(BadgeKind::BlockSwatch, id),
            block_accent(id),
        )
    }

    pub fn semantic_badge(
        sprite_key: impl Into<String>,
        accent_rgba: [u8; 4],
    ) -> CompletionIconBadge {
        CompletionIconBadge::new(BadgeKind::Semantic, sprite_key, accent_rgba)
    }

    /// Deterministic 8x8 palette-indexed glyph for any badge identity.
    pub fn glyph(kind: BadgeKind, sprite_key: &str, _accent_rgba: [u8; 4]) -> [u8; GLYPH_PIXELS] {
        generate_glyph(kind, sprite_key)
    }
}

#[derive(Clone, Debug)]
struct ReverseSearchState {
    query: String,
    saved_buffer: String,
    saved_cursor: usize,
    match_index: Option<usize>,
}

#[derive(Clone, Debug)]
struct CompletionMenu {
    candidates: Vec<CompletionCandidate>,
    start: usize,
    end: usize,
    index: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CompletionContext {
    Commands,
    Selectors,
    Entities,
    SelectorEntities,
    Items,
    Effects,
    Enchants,
    Blocks,
    EffectActions,
    ItemActions,
    TimeActions,
    GameModes,
    Times,
    TimeQueries,
    Weathers,
    Coordinates,
    None,
}

#[derive(Clone, Debug)]
struct CompletionTarget {
    start: usize,
    end: usize,
    query: String,
    context: CompletionContext,
    suffix: String,
}

#[derive(Clone, Debug)]
enum GhostSuggestion {
    History(String),
    Completion {
        start: usize,
        end: usize,
        text: String,
        ghost: String,
    },
}

#[derive(Clone, Copy, Debug)]
struct TokenSpan {
    start: usize,
    end: usize,
}

/// Readline-inspired Minecraft command input engine.
#[derive(Clone, Debug)]
pub struct ZshCommandBar {
    buffer: String,
    cursor: usize,
    history: VecDeque<String>,
    history_capacity: usize,
    history_nav: Option<usize>,
    history_saved: String,
    completion_menu: Option<CompletionMenu>,
    reverse_search: Option<ReverseSearchState>,
    items: BTreeSet<String>,
    entities: BTreeSet<String>,
    effects: BTreeSet<String>,
    enchants: BTreeSet<String>,
    blocks: BTreeSet<String>,
}

impl Default for ZshCommandBar {
    fn default() -> Self {
        Self::new()
    }
}

impl ZshCommandBar {
    pub fn new() -> Self {
        Self::with_history_capacity(COMMAND_HISTORY_CAPACITY)
    }

    pub fn with_history_capacity(history_capacity: usize) -> Self {
        Self {
            buffer: String::new(),
            cursor: 0,
            history: VecDeque::new(),
            history_capacity: history_capacity.clamp(1, COMMAND_HISTORY_CAPACITY),
            history_nav: None,
            history_saved: String::new(),
            completion_menu: None,
            reverse_search: None,
            items: BTreeSet::new(),
            entities: BTreeSet::new(),
            effects: BTreeSet::new(),
            enchants: BTreeSet::new(),
            blocks: BTreeSet::new(),
        }
    }

    pub fn text(&self) -> &str {
        &self.buffer
    }

    pub fn len(&self) -> usize {
        self.buffer.len()
    }

    pub fn is_empty(&self) -> bool {
        self.buffer.is_empty()
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    pub fn set_text(&mut self, text: impl AsRef<str>) {
        self.buffer = text.as_ref().to_string();
        self.cursor = self.buffer.len();
        self.reset_edit_state();
    }

    pub fn clear(&mut self) {
        self.buffer.clear();
        self.cursor = 0;
        self.reset_edit_state();
    }

    pub fn set_cursor(&mut self, cursor: usize) {
        self.cursor = floor_char_boundary(&self.buffer, cursor.min(self.buffer.len()));
        self.completion_menu = None;
        self.reverse_search = None;
    }

    pub fn insert_str(&mut self, text: impl AsRef<str>) {
        let text = text.as_ref();
        if text.is_empty() {
            return;
        }
        self.buffer.insert_str(self.cursor, text);
        self.cursor += text.len();
        self.reset_edit_state();
    }

    pub fn push_char(&mut self, ch: char) {
        let mut buf = [0u8; 4];
        self.insert_str(ch.encode_utf8(&mut buf));
    }

    pub fn delete_backward(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let start = prev_char_boundary(&self.buffer, self.cursor);
        self.buffer.replace_range(start..self.cursor, "");
        self.cursor = start;
        self.reset_edit_state();
    }

    pub fn delete_forward(&mut self) {
        if self.cursor >= self.buffer.len() {
            return;
        }
        let end = next_char_boundary(&self.buffer, self.cursor);
        self.buffer.replace_range(self.cursor..end, "");
        self.reset_edit_state();
    }

    pub fn delete_word_backward(&mut self) {
        let mut start = self.cursor;
        while start > 0 {
            let prev = prev_char_boundary(&self.buffer, start);
            let ch = self.buffer[prev..start].chars().next().unwrap();
            if !ch.is_whitespace() {
                break;
            }
            start = prev;
        }
        while start > 0 {
            let prev = prev_char_boundary(&self.buffer, start);
            let ch = self.buffer[prev..start].chars().next().unwrap();
            if ch.is_whitespace() {
                break;
            }
            start = prev;
        }
        if start < self.cursor {
            self.buffer.replace_range(start..self.cursor, "");
            self.cursor = start;
            self.reset_edit_state();
        }
    }

    pub fn kill_to_start(&mut self) {
        if self.cursor == 0 {
            return;
        }
        self.buffer.replace_range(..self.cursor, "");
        self.cursor = 0;
        self.reset_edit_state();
    }

    pub fn move_cursor_left(&mut self) {
        if self.cursor > 0 {
            self.cursor = prev_char_boundary(&self.buffer, self.cursor);
            self.completion_menu = None;
        }
    }

    pub fn move_cursor_right(&mut self) {
        if self.cursor < self.buffer.len() {
            self.cursor = next_char_boundary(&self.buffer, self.cursor);
            self.completion_menu = None;
        }
    }

    pub fn move_cursor_home(&mut self) {
        self.cursor = 0;
        self.completion_menu = None;
    }

    pub fn move_cursor_end(&mut self) {
        self.cursor = self.buffer.len();
        self.completion_menu = None;
    }

    pub fn history(&self) -> impl Iterator<Item = &str> {
        self.history.iter().map(String::as_str)
    }

    pub fn history_len(&self) -> usize {
        self.history.len()
    }

    pub fn clear_history(&mut self) {
        self.history.clear();
        self.history_nav = None;
        self.history_saved.clear();
    }

    pub fn push_history_entry(&mut self, entry: impl AsRef<str>) {
        let entry = entry.as_ref().trim();
        if entry.is_empty() {
            return;
        }
        self.history.push_back(entry.to_string());
        while self.history.len() > self.history_capacity {
            self.history.pop_front();
        }
    }

    pub fn register_item(&mut self, id: impl AsRef<str>) {
        self.items.insert(normalize_registry_id(id.as_ref()));
    }

    pub fn register_entity(&mut self, id: impl AsRef<str>) {
        self.entities.insert(normalize_registry_id(id.as_ref()));
    }

    pub fn register_effect(&mut self, id: impl AsRef<str>) {
        self.effects.insert(normalize_registry_id(id.as_ref()));
    }

    pub fn register_enchant(&mut self, id: impl AsRef<str>) {
        self.enchants.insert(normalize_registry_id(id.as_ref()));
    }

    pub fn register_block(&mut self, id: impl AsRef<str>) {
        self.blocks.insert(normalize_registry_id(id.as_ref()));
    }

    /// Ranked completion candidates for the token at the cursor.
    pub fn completions(&self) -> Vec<CompletionCandidate> {
        let target = self.completion_target();
        self.candidates_for_target(&target)
    }

    /// Alias for [`ZshCommandBar::completions`].
    pub fn completion_candidates(&self) -> Vec<CompletionCandidate> {
        self.completions()
    }

    /// Candidate currently selected by an open completion menu.
    pub fn selected_completion(&self) -> Option<CompletionCandidate> {
        self.completion_menu
            .as_ref()
            .and_then(|menu| menu.candidates.get(menu.index).cloned())
    }

    pub fn completion_menu_active(&self) -> bool {
        self.completion_menu.is_some()
    }

    /// Open the completion menu or cycle it when already open.
    pub fn cycle_completion(&mut self, forward: bool) -> Option<CompletionCandidate> {
        if let Some(mut menu) = self.completion_menu.clone() {
            let len = menu.candidates.len();
            if len == 0 {
                self.completion_menu = None;
                return None;
            }
            menu.index = if forward {
                (menu.index + 1) % len
            } else {
                (menu.index + len - 1) % len
            };
            let candidate = menu.candidates[menu.index].clone();
            self.apply_completion_candidate(&mut menu, &candidate);
            self.cursor = floor_char_boundary(&self.buffer, menu.end);
            self.completion_menu = Some(menu);
            return Some(candidate);
        }

        let target = self.completion_target();
        let candidates = self.candidates_for_target(&target);
        if candidates.is_empty() {
            return None;
        }
        let index = if forward { 0 } else { candidates.len() - 1 };
        let mut menu = CompletionMenu {
            candidates,
            start: target.start,
            end: target.end,
            index,
        };
        let candidate = menu.candidates[index].clone();
        self.apply_completion_candidate(&mut menu, &candidate);
        self.cursor = floor_char_boundary(&self.buffer, menu.end);
        self.completion_menu = Some(menu);
        Some(candidate)
    }

    /// Inline ghost text suggested by history or the top completion.
    pub fn ghost_text(&self) -> String {
        match self.ghost_suggestion() {
            Some(GhostSuggestion::History(suffix)) => suffix,
            Some(GhostSuggestion::Completion { ghost, .. }) => ghost,
            None => String::new(),
        }
    }

    /// Accept the current ghost suggestion, returning whether one existed.
    pub fn accept_ghost_text(&mut self) -> bool {
        let Some(ghost) = self.ghost_suggestion() else {
            return false;
        };
        match ghost {
            GhostSuggestion::History(suffix) => {
                self.insert_str(suffix);
            }
            GhostSuggestion::Completion {
                start, end, text, ..
            } => {
                self.buffer.replace_range(start..end, &text);
                self.cursor = start + text.len();
                self.reset_edit_state();
            }
        }
        true
    }

    /// Live syntax highlighting for the current command line.
    pub fn syntax_highlight(&self) -> Vec<HighlightedSpan> {
        highlight_command_line(&self.buffer)
    }

    /// Alias for [`ZshCommandBar::syntax_highlight`].
    pub fn highlighted_spans(&self) -> Vec<HighlightedSpan> {
        self.syntax_highlight()
    }

    /// Usage hint for the current command, or an unknown-command hint.
    pub fn usage_hint(&self) -> Option<String> {
        let tokens = lexical_tokens(&self.buffer);
        let first = tokens.first()?;
        let text = token_text(&self.buffer, *first);
        let name = text.trim_start_matches('/');
        if name.is_empty() {
            return None;
        }
        if let Some(spec) = command_spec(name) {
            return Some(spec.2.to_string());
        }
        if is_selector_token(text) {
            return Some("@<selector>[arguments]".to_string());
        }
        Some(format!("Unknown command '{text}'; try /help"))
    }

    pub fn is_reverse_search_active(&self) -> bool {
        self.reverse_search.is_some()
    }

    pub fn reverse_search_query(&self) -> Option<&str> {
        self.reverse_search
            .as_ref()
            .map(|state| state.query.as_str())
    }

    pub fn reverse_search_match(&self) -> Option<String> {
        let state = self.reverse_search.as_ref()?;
        state
            .match_index
            .and_then(|index| self.history.get(index))
            .cloned()
    }

    /// Submit the current line: record non-empty input in bounded history and clear the bar.
    pub fn submit(&mut self) -> Option<String> {
        let command = self.buffer.trim().to_string();
        self.clear();
        if command.is_empty() {
            return None;
        }
        self.push_history_entry(command.clone());
        Some(command)
    }

    pub fn handle_key(&mut self, key: CommandKey) -> CommandBarAction {
        if let Some(action) = self.handle_reverse_search_key(key) {
            return action;
        }

        match key {
            CommandKey::Char(ch) if !ch.is_control() => {
                self.insert_str(ch.to_string());
                CommandBarAction::Changed
            }
            CommandKey::Char(_) => CommandBarAction::Ignored,
            CommandKey::Backspace => {
                self.delete_backward();
                CommandBarAction::Changed
            }
            CommandKey::Delete => {
                self.delete_forward();
                CommandBarAction::Changed
            }
            CommandKey::Left => {
                self.move_cursor_left();
                CommandBarAction::Changed
            }
            CommandKey::Right => {
                if self.cursor == self.buffer.len() && self.accept_ghost_text() {
                    return CommandBarAction::GhostAccepted;
                }
                self.move_cursor_right();
                CommandBarAction::Changed
            }
            CommandKey::Up => {
                if self.completion_menu.is_some() {
                    return self.complete_key(false);
                }
                self.history_previous();
                CommandBarAction::Changed
            }
            CommandKey::Down => {
                if self.completion_menu.is_some() {
                    return self.complete_key(true);
                }
                self.history_next();
                CommandBarAction::Changed
            }
            CommandKey::Home | CommandKey::CtrlA => {
                self.move_cursor_home();
                CommandBarAction::Changed
            }
            CommandKey::End | CommandKey::CtrlE => {
                if key == CommandKey::End && self.accept_ghost_text() {
                    return CommandBarAction::GhostAccepted;
                }
                self.move_cursor_end();
                CommandBarAction::Changed
            }
            CommandKey::Enter => match self.submit() {
                Some(command) => CommandBarAction::Submitted(command),
                None => CommandBarAction::Ignored,
            },
            CommandKey::Escape => {
                if self.completion_menu.take().is_some() {
                    CommandBarAction::Changed
                } else {
                    self.clear();
                    CommandBarAction::Changed
                }
            }
            CommandKey::Tab => self.complete_key(true),
            CommandKey::ShiftTab => self.complete_key(false),
            CommandKey::CtrlW => {
                self.delete_word_backward();
                CommandBarAction::Changed
            }
            CommandKey::CtrlU => {
                self.kill_to_start();
                CommandBarAction::Changed
            }
            CommandKey::CtrlR => {
                self.start_reverse_search();
                CommandBarAction::ReverseSearchStarted
            }
        }
    }

    /// Handle a raw character through [`CommandKey::from_char`].
    pub fn handle_char(&mut self, ch: char) -> CommandBarAction {
        match CommandKey::from_char(ch) {
            Some(key) => self.handle_key(key),
            None => CommandBarAction::Ignored,
        }
    }

    fn complete_key(&mut self, forward: bool) -> CommandBarAction {
        match self.cycle_completion(forward) {
            Some(candidate) => CommandBarAction::Completed {
                index: self
                    .completion_menu
                    .as_ref()
                    .map(|menu| menu.index)
                    .unwrap_or(0),
                text: candidate.text,
            },
            None => CommandBarAction::Ignored,
        }
    }

    fn apply_completion_candidate(
        &mut self,
        menu: &mut CompletionMenu,
        candidate: &CompletionCandidate,
    ) {
        self.buffer
            .replace_range(menu.start..menu.end, &candidate.text);
        menu.end = menu.start + candidate.text.len();
    }

    fn reset_edit_state(&mut self) {
        self.completion_menu = None;
        self.reverse_search = None;
        self.history_nav = None;
    }

    fn ghost_suggestion(&self) -> Option<GhostSuggestion> {
        if self.completion_menu.is_some() || self.cursor != self.buffer.len() {
            return None;
        }
        if let Some(suffix) = self.history_ghost_suffix() {
            return Some(GhostSuggestion::History(suffix));
        }
        let target = self.completion_target();
        let candidates = self.candidates_for_target(&target);
        let candidate = candidates.into_iter().next()?;
        let query = target.query.as_str();
        if query.is_empty() || candidate.text.len() < query.len() {
            return None;
        }
        if candidate
            .text
            .get(..query.len())
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case(query))
        {
            let ghost = candidate.text[query.len()..].to_string();
            if ghost.is_empty() {
                return None;
            }
            return Some(GhostSuggestion::Completion {
                start: target.start,
                end: target.end,
                text: candidate.text,
                ghost,
            });
        }
        None
    }

    fn history_ghost_suffix(&self) -> Option<String> {
        if self.buffer.is_empty() {
            return None;
        }
        self.history.iter().rev().find_map(|entry| {
            if entry.len() > self.buffer.len()
                && entry
                    .get(..self.buffer.len())
                    .is_some_and(|prefix| prefix.eq_ignore_ascii_case(&self.buffer))
            {
                Some(entry[self.buffer.len()..].to_string())
            } else {
                None
            }
        })
    }

    fn history_previous(&mut self) {
        if self.history.is_empty() {
            return;
        }
        let index = match self.history_nav {
            Some(index) if index > 0 => index - 1,
            Some(index) => index,
            None => {
                self.history_saved = self.buffer.clone();
                self.history.len() - 1
            }
        };
        self.buffer = self.history[index].clone();
        self.cursor = self.buffer.len();
        self.history_nav = Some(index);
        self.completion_menu = None;
    }

    fn history_next(&mut self) {
        let Some(index) = self.history_nav else {
            return;
        };
        if index + 1 < self.history.len() {
            let next = index + 1;
            self.buffer = self.history[next].clone();
            self.cursor = self.buffer.len();
            self.history_nav = Some(next);
        } else {
            self.buffer = self.history_saved.clone();
            self.cursor = self.buffer.len();
            self.history_nav = None;
            self.history_saved.clear();
        }
        self.completion_menu = None;
    }

    fn start_reverse_search(&mut self) {
        self.reverse_search = Some(ReverseSearchState {
            query: String::new(),
            saved_buffer: self.buffer.clone(),
            saved_cursor: self.cursor,
            match_index: None,
        });
        self.completion_menu = None;
        self.history_nav = None;
    }

    fn handle_reverse_search_key(&mut self, key: CommandKey) -> Option<CommandBarAction> {
        self.reverse_search.as_ref()?;
        match key {
            CommandKey::Char(ch) if !ch.is_control() => {
                self.reverse_search_push(ch);
                Some(CommandBarAction::ReverseSearchChanged)
            }
            CommandKey::Backspace => {
                self.reverse_search_pop();
                Some(CommandBarAction::ReverseSearchChanged)
            }
            CommandKey::Enter => {
                self.finish_reverse_search(true);
                Some(CommandBarAction::ReverseSearchFinished)
            }
            CommandKey::Escape => {
                self.finish_reverse_search(false);
                Some(CommandBarAction::ReverseSearchCancelled)
            }
            CommandKey::CtrlR => {
                self.reverse_search_older();
                Some(CommandBarAction::ReverseSearchChanged)
            }
            _ => {
                self.finish_reverse_search(true);
                None
            }
        }
    }

    fn reverse_search_push(&mut self, ch: char) {
        if let Some(state) = self.reverse_search.as_mut() {
            state.query.push(ch);
        }
        self.refresh_reverse_search(false);
    }

    fn reverse_search_pop(&mut self) {
        if let Some(state) = self.reverse_search.as_mut() {
            state.query.pop();
        }
        self.refresh_reverse_search(false);
    }

    fn reverse_search_older(&mut self) {
        self.refresh_reverse_search(true);
    }

    fn refresh_reverse_search(&mut self, older: bool) {
        let Some(state) = self.reverse_search.as_mut() else {
            return;
        };
        let query = state.query.to_lowercase();
        if query.is_empty() {
            state.match_index = None;
            let saved_buffer = state.saved_buffer.clone();
            let saved_cursor = state.saved_cursor;
            self.buffer = saved_buffer;
            self.cursor = saved_cursor;
            return;
        }

        let current = state.match_index;
        let history_len = self.history.len();
        let mut found = None;
        if older {
            if let Some(current_index) = current {
                for index in (0..current_index).rev() {
                    if self
                        .history
                        .get(index)
                        .is_some_and(|entry| entry.to_lowercase().contains(&query))
                    {
                        found = Some(index);
                        break;
                    }
                }
            }
        }
        if found.is_none() && !older {
            for index in (0..history_len).rev() {
                if self
                    .history
                    .get(index)
                    .is_some_and(|entry| entry.to_lowercase().contains(&query))
                {
                    found = Some(index);
                    break;
                }
            }
        }
        if found.is_none() && older {
            for index in (0..history_len).rev() {
                if self
                    .history
                    .get(index)
                    .is_some_and(|entry| entry.to_lowercase().contains(&query))
                {
                    found = Some(index);
                    break;
                }
            }
        }

        state.match_index = found;
        match found.and_then(|index| self.history.get(index).cloned()) {
            Some(entry) => {
                self.buffer = entry;
                self.cursor = self.buffer.len();
            }
            None => {
                self.buffer = state.saved_buffer.clone();
                self.cursor = state.saved_cursor;
            }
        }
    }

    fn finish_reverse_search(&mut self, accept: bool) {
        let Some(state) = self.reverse_search.take() else {
            return;
        };
        if accept {
            if state.match_index.is_none() {
                self.buffer = state.saved_buffer;
                self.cursor = state.saved_cursor;
            } else {
                self.cursor = self.buffer.len();
            }
        } else {
            self.buffer = state.saved_buffer;
            self.cursor = state.saved_cursor;
        }
        self.completion_menu = None;
    }

    fn completion_target(&self) -> CompletionTarget {
        let tokens = lexical_tokens(&self.buffer);
        let cursor = self.cursor.min(self.buffer.len());
        let current = find_current_token(&tokens, cursor);
        if let Some((index, span)) = current {
            if let Some(selector) = self.selector_type_target(&tokens, index, span, cursor) {
                return selector;
            }
        }

        let position = if let Some((index, _)) = current {
            index
        } else {
            tokens.iter().filter(|span| span.end <= cursor).count()
        };
        let context = completion_context(&self.buffer, &tokens, position);
        let (start, end, query) = if let Some((_, span)) = current {
            (
                span.start,
                span.end,
                token_text(&self.buffer, span).to_string(),
            )
        } else {
            (cursor, cursor, String::new())
        };
        CompletionTarget {
            start,
            end,
            query,
            context,
            suffix: String::new(),
        }
    }

    fn selector_type_target(
        &self,
        tokens: &[TokenSpan],
        token_index: usize,
        span: TokenSpan,
        cursor: usize,
    ) -> Option<CompletionTarget> {
        let _ = tokens;
        let _ = token_index;
        let token = token_text(&self.buffer, span);
        if !token.starts_with('@') {
            return None;
        }
        let type_pos = token.find("type=")?;
        let value_start_rel = type_pos + "type=".len();
        let value_end_rel = token[value_start_rel..]
            .find([',', ']'])
            .map(|offset| value_start_rel + offset)
            .unwrap_or(token.len());
        let cursor_rel = cursor.saturating_sub(span.start).min(token.len());
        if cursor_rel < value_start_rel || cursor_rel > value_end_rel {
            return None;
        }
        let mut suffix = token[value_end_rel..].to_string();
        if !suffix.contains(']') {
            suffix.push(']');
        }
        Some(CompletionTarget {
            start: span.start + value_start_rel,
            end: span.end,
            query: token[value_start_rel..cursor_rel].to_string(),
            context: CompletionContext::SelectorEntities,
            suffix,
        })
    }

    fn candidates_for_target(&self, target: &CompletionTarget) -> Vec<CompletionCandidate> {
        let mut candidates = Vec::new();
        match target.context {
            CompletionContext::Commands => {
                // The kernel `command` dispatcher is the authoritative
                // literal filter — `complete_suggestions` against the same
                // vanilla seed `LiveSession` feeds its wire path answers
                // exactly the command names the session would complete,
                // and the bar layers its badge/humanized presentation over
                // that live set. An empty accepted set (no literal prefix
                // match — e.g. a fuzzy subsequence query like `tm`) keeps
                // the unfiltered spec list so the fzf-style ranker still
                // owns those keystrokes; a dispatcher error does the same.
                let accepted = command_completion_filter(&target.query)
                    .filter(|accepted| !accepted.is_empty());
                for &(name, _, _) in COMMAND_SPECS {
                    if let Some(accepted) = &accepted {
                        let literal = name.trim_start_matches('/');
                        if !accepted.iter().any(|item| item == literal) {
                            continue;
                        }
                    }
                    let id = format!("command/{}", name.trim_start_matches('/'));
                    candidates.push(
                        CompletionCandidate::new(
                            id,
                            name,
                            humanize_id(name.trim_start_matches('/')),
                        )
                        .with_badge(
                            CompletionBadgeCatalog::semantic_badge(
                                format!("command/{}", name.trim_start_matches('/')),
                                command_accent(name),
                            ),
                        ),
                    );
                }
            }
            CompletionContext::Selectors => {
                for &(text, id, label) in SELECTOR_CANDIDATES {
                    candidates.push(CompletionCandidate::new(id, text, label).with_badge(
                        CompletionBadgeCatalog::semantic_badge(
                            format!("selector/{id}"),
                            [226, 190, 72, 255],
                        ),
                    ));
                }
            }
            CompletionContext::Entities | CompletionContext::SelectorEntities => {
                candidates.extend(self.entity_candidates(target));
            }
            CompletionContext::Items => {
                candidates.extend(self.item_candidates(target.query.as_str()))
            }
            CompletionContext::Effects => candidates.extend(self.effect_candidates()),
            CompletionContext::Enchants => candidates.extend(self.enchant_candidates()),
            CompletionContext::Blocks => candidates.extend(self.block_candidates()),
            CompletionContext::EffectActions => {
                for text in ["give", "clear"] {
                    candidates.push(
                        CompletionCandidate::new(
                            format!("effect_action/{text}"),
                            text,
                            humanize_id(text),
                        )
                        .with_badge(
                            CompletionBadgeCatalog::semantic_badge(
                                format!("subcommand/{text}"),
                                [96, 180, 220, 255],
                            ),
                        ),
                    );
                }
            }
            CompletionContext::ItemActions => {
                for text in ["replace", "modify"] {
                    candidates.push(
                        CompletionCandidate::new(
                            format!("item_action/{text}"),
                            text,
                            humanize_id(text),
                        )
                        .with_badge(
                            CompletionBadgeCatalog::semantic_badge(
                                format!("subcommand/{text}"),
                                [96, 180, 220, 255],
                            ),
                        ),
                    );
                }
            }
            CompletionContext::TimeActions => {
                for text in ["set", "add", "query"] {
                    candidates.push(
                        CompletionCandidate::new(
                            format!("time_action/{text}"),
                            text,
                            humanize_id(text),
                        )
                        .with_badge(
                            CompletionBadgeCatalog::semantic_badge(
                                format!("subcommand/{text}"),
                                [230, 190, 90, 255],
                            ),
                        ),
                    );
                }
            }
            CompletionContext::GameModes => {
                for &mode in GAME_MODES {
                    candidates.push(
                        CompletionCandidate::new(
                            format!("gamemode/{mode}"),
                            mode,
                            humanize_id(mode),
                        )
                        .with_badge(
                            CompletionBadgeCatalog::semantic_badge(
                                format!("gamemode/{mode}"),
                                [120, 170, 90, 255],
                            ),
                        ),
                    );
                }
            }
            CompletionContext::Times => {
                for &value in TIME_VALUES {
                    candidates.push(
                        CompletionCandidate::new(
                            format!("time/{value}"),
                            value,
                            humanize_id(value),
                        )
                        .with_badge(
                            CompletionBadgeCatalog::semantic_badge(
                                format!("time/{value}"),
                                [230, 190, 90, 255],
                            ),
                        ),
                    );
                }
            }
            CompletionContext::TimeQueries => {
                for &value in TIME_QUERY_VALUES {
                    candidates.push(
                        CompletionCandidate::new(
                            format!("time_query/{value}"),
                            value,
                            humanize_id(value),
                        )
                        .with_badge(
                            CompletionBadgeCatalog::semantic_badge(
                                format!("time_query/{value}"),
                                [120, 160, 220, 255],
                            ),
                        ),
                    );
                }
            }
            CompletionContext::Weathers => {
                for &value in WEATHER_TYPES {
                    candidates.push(
                        CompletionCandidate::new(
                            format!("weather/{value}"),
                            value,
                            humanize_id(value),
                        )
                        .with_badge(
                            CompletionBadgeCatalog::semantic_badge(
                                format!("weather/{value}"),
                                [90, 150, 210, 255],
                            ),
                        ),
                    );
                }
            }
            CompletionContext::Coordinates => {
                for value in ["~", "^", "0", "1", "-1"] {
                    candidates.push(
                        CompletionCandidate::new(format!("coordinate/{value}"), value, value)
                            .with_badge(CompletionBadgeCatalog::semantic_badge(
                                format!("coordinate/{value}"),
                                [90, 150, 220, 255],
                            )),
                    );
                }
            }
            CompletionContext::None => {}
        }

        if matches!(
            target.context,
            CompletionContext::Entities
                | CompletionContext::SelectorEntities
                | CompletionContext::Items
                | CompletionContext::Effects
                | CompletionContext::Enchants
                | CompletionContext::Blocks
        ) {
            if let Some(candidate) = synthesized_id_candidate(target) {
                candidates.push(candidate);
            }
        }

        rank_candidates(candidates, &normalized_rank_query(target))
            .into_iter()
            .take(MAX_COMPLETION_CANDIDATES)
            .collect()
    }

    fn entity_candidates(&self, target: &CompletionTarget) -> Vec<CompletionCandidate> {
        let mut ids = ENTITY_IDS
            .iter()
            .map(|id| (*id).to_string())
            .collect::<Vec<_>>();
        ids.extend(self.entities.iter().cloned());
        let mut out = Vec::new();
        for id in ids {
            let normalized = normalize_registry_id(&id);
            let category = mob_category(&normalized);
            let text = if target.context == CompletionContext::SelectorEntities {
                format!("{}{}", normalized.as_str(), target.suffix)
            } else {
                normalized.clone()
            };
            out.push(
                CompletionCandidate::new(
                    format!("entity/{}", canonical_registry_id(&normalized)),
                    text,
                    humanize_id(&normalized),
                )
                .with_badge(CompletionBadgeCatalog::mob_badge(&normalized))
                .with_right_tag(category.tag()),
            );
        }
        out
    }

    fn item_candidates(&self, query: &str) -> Vec<CompletionCandidate> {
        let _ = query;
        let mut ids = ITEM_IDS
            .iter()
            .map(|id| (*id).to_string())
            .collect::<Vec<_>>();
        ids.extend(BLOCK_IDS.iter().map(|id| (*id).to_string()));
        ids.extend(self.items.iter().cloned());
        let mut out = Vec::new();
        for id in ids {
            let normalized = normalize_registry_id(&id);
            out.push(
                CompletionCandidate::new(
                    format!("item/{}", canonical_registry_id(&normalized)),
                    normalized.as_str(),
                    humanize_id(&normalized),
                )
                .with_badge(CompletionBadgeCatalog::item_badge(&normalized)),
            );
        }
        out
    }

    fn effect_candidates(&self) -> Vec<CompletionCandidate> {
        let mut ids = EFFECT_IDS
            .iter()
            .map(|id| (*id).to_string())
            .collect::<Vec<_>>();
        ids.extend(self.effects.iter().cloned());
        let mut out = Vec::new();
        for id in ids {
            let normalized = normalize_registry_id(&id);
            let tag = if effect_is_debuff(&normalized) {
                "Debuff"
            } else {
                "Buff"
            };
            out.push(
                CompletionCandidate::new(
                    format!("effect/{}", canonical_registry_id(&normalized)),
                    normalized.as_str(),
                    humanize_id(&normalized),
                )
                .with_badge(CompletionBadgeCatalog::effect_badge(&normalized))
                .with_right_tag(tag),
            );
        }
        out
    }

    fn enchant_candidates(&self) -> Vec<CompletionCandidate> {
        let mut out = Vec::new();
        for id in ENCHANTS {
            push_enchant_candidate(&mut out, id.id, id.max_level, id.curse);
        }
        for id in self.enchants.iter() {
            push_enchant_candidate(&mut out, id, 1, false);
        }
        out
    }

    fn block_candidates(&self) -> Vec<CompletionCandidate> {
        let mut ids = BLOCK_IDS
            .iter()
            .map(|id| (*id).to_string())
            .collect::<Vec<_>>();
        ids.extend(self.blocks.iter().cloned());
        let mut out = Vec::new();
        for id in ids {
            let normalized = normalize_registry_id(&id);
            out.push(
                CompletionCandidate::new(
                    format!("block/{}", canonical_registry_id(&normalized)),
                    normalized.as_str(),
                    humanize_id(&normalized),
                )
                .with_badge(CompletionBadgeCatalog::block_badge(&normalized)),
            );
        }
        out
    }
}

fn push_enchant_candidate(
    out: &mut Vec<CompletionCandidate>,
    id: &str,
    max_level: u32,
    curse: bool,
) {
    let normalized = normalize_registry_id(id);
    let tag = if curse {
        "Curse".to_string()
    } else {
        format!("Max {}", roman_numeral(max_level))
    };
    out.push(
        CompletionCandidate::new(
            format!("enchant/{}", canonical_registry_id(&normalized)),
            normalized.as_str(),
            humanize_id(&normalized),
        )
        .with_badge(CompletionBadgeCatalog::enchant_badge(&normalized, curse))
        .with_right_tag(tag),
    );
}

fn completion_context(input: &str, tokens: &[TokenSpan], position: usize) -> CompletionContext {
    if tokens.is_empty() || position == 0 {
        return CompletionContext::Commands;
    }
    let command_token = tokens[0];
    let command_text = token_text(input, command_token);
    let command = command_text.trim_start_matches('/');
    if command.is_empty() {
        return CompletionContext::Commands;
    }
    if command_spec(command).is_none() && !is_selector_token(command_text) {
        return CompletionContext::None;
    }

    let action = tokens.get(1).map(|span| token_text(input, *span));
    match command {
        "give" | "clear" => match position {
            1 => CompletionContext::Selectors,
            2 => CompletionContext::Items,
            _ => CompletionContext::None,
        },
        "item" => match position {
            1 => CompletionContext::ItemActions,
            _ => CompletionContext::Items,
        },
        "summon" => match position {
            1 => CompletionContext::Entities,
            2..=4 => CompletionContext::Coordinates,
            _ => CompletionContext::None,
        },
        "effect" => match position {
            1 => CompletionContext::EffectActions,
            2 => CompletionContext::Selectors,
            3 => CompletionContext::Effects,
            _ => CompletionContext::None,
        },
        "enchant" => match position {
            1 => CompletionContext::Selectors,
            2 => CompletionContext::Enchants,
            _ => CompletionContext::None,
        },
        "setblock" => match position {
            1..=3 => CompletionContext::Coordinates,
            4 => CompletionContext::Blocks,
            _ => CompletionContext::None,
        },
        "fill" => match position {
            1..=6 => CompletionContext::Coordinates,
            7 => CompletionContext::Blocks,
            _ => CompletionContext::None,
        },
        "gamemode" => match position {
            1 => CompletionContext::GameModes,
            2 => CompletionContext::Selectors,
            _ => CompletionContext::None,
        },
        "time" => match position {
            1 => CompletionContext::TimeActions,
            2 => match action {
                Some("query") => CompletionContext::TimeQueries,
                _ => CompletionContext::Times,
            },
            _ => CompletionContext::None,
        },
        "weather" => match position {
            1 => CompletionContext::Weathers,
            2 => CompletionContext::None,
            _ => CompletionContext::None,
        },
        "kill" | "tp" | "teleport" => match position {
            1 => CompletionContext::Selectors,
            _ => CompletionContext::Coordinates,
        },
        _ => CompletionContext::None,
    }
}

fn find_current_token(tokens: &[TokenSpan], cursor: usize) -> Option<(usize, TokenSpan)> {
    tokens
        .iter()
        .enumerate()
        .find(|(_, span)| span.start <= cursor && cursor <= span.end)
        .map(|(index, span)| (index, *span))
}

fn lexical_tokens(input: &str) -> Vec<TokenSpan> {
    let mut spans = Vec::new();
    let mut start: Option<usize> = None;
    let mut quote: Option<char> = None;
    let mut depth = 0usize;
    for (index, ch) in input.char_indices() {
        if let Some(active_quote) = quote {
            if ch == active_quote {
                quote = None;
            }
            continue;
        }
        match ch {
            '"' | '\'' => {
                if start.is_none() {
                    start = Some(index);
                }
                quote = Some(ch);
            }
            '{' | '[' | '(' => {
                if start.is_none() {
                    start = Some(index);
                }
                depth += 1;
            }
            '}' | ']' | ')' => {
                depth = depth.saturating_sub(1);
            }
            ch if ch.is_whitespace() && depth == 0 => {
                if let Some(span_start) = start.take() {
                    spans.push(TokenSpan {
                        start: span_start,
                        end: index,
                    });
                }
            }
            _ => {
                if start.is_none() {
                    start = Some(index);
                }
            }
        }
    }
    if let Some(span_start) = start {
        spans.push(TokenSpan {
            start: span_start,
            end: input.len(),
        });
    }
    spans
}

fn token_text(input: &str, span: TokenSpan) -> &str {
    &input[span.start..span.end]
}

fn highlight_command_line(input: &str) -> Vec<HighlightedSpan> {
    let tokens = lexical_tokens(input);
    if tokens.is_empty() {
        return Vec::new();
    }
    let mut spans = Vec::new();
    let first_text = token_text(input, tokens[0]);
    let command_name = first_text.trim_start_matches('/');
    let valid_command = command_spec(command_name).is_some();
    let selector_only = is_selector_token(first_text);

    for (index, span) in tokens.iter().enumerate() {
        let text = token_text(input, *span).to_string();
        let kind = if index == 0 {
            if selector_only {
                HighlightKind::TargetSelector
            } else if valid_command {
                HighlightKind::Command
            } else {
                HighlightKind::Invalid
            }
        } else if !valid_command && !selector_only {
            HighlightKind::Invalid
        } else {
            classify_argument(input, &tokens, index, command_name)
        };
        spans.push(HighlightedSpan {
            start: span.start,
            end: span.end,
            kind,
            color: kind.color(),
            text,
        });
    }
    spans
}

fn classify_argument(
    input: &str,
    tokens: &[TokenSpan],
    index: usize,
    command: &str,
) -> HighlightKind {
    let span = tokens[index];
    let text = token_text(input, span);
    if is_selector_token(text) {
        return HighlightKind::TargetSelector;
    }
    if text.starts_with('"')
        || text.starts_with('\'')
        || text.starts_with('{')
        || text.starts_with('[')
    {
        return HighlightKind::StringOrNbt;
    }

    match command {
        "effect" => match index {
            1 => literal_or_invalid(text, &["give", "clear"]),
            2 => HighlightKind::TargetSelector,
            3 => registry_or_literal(text, EFFECT_IDS, true),
            _ => HighlightKind::StringOrNbt,
        },
        "enchant" => match index {
            1 => HighlightKind::TargetSelector,
            2 => registry_or_literal(text, ENCHANT_IDS_ONLY, true),
            _ => HighlightKind::StringOrNbt,
        },
        "give" | "clear" => match index {
            1 => HighlightKind::TargetSelector,
            2 => registry_or_literal(text, ITEM_IDS, true),
            _ => HighlightKind::StringOrNbt,
        },
        "item" => match index {
            1 => literal_or_invalid(text, &["replace", "modify"]),
            _ => registry_or_literal(text, ITEM_IDS, true),
        },
        "summon" => match index {
            1 => registry_or_literal(text, ENTITY_IDS, true),
            2..=4 if is_coordinate_token(text) => HighlightKind::Coordinate,
            _ => HighlightKind::StringOrNbt,
        },
        "setblock" => match index {
            1..=3 if is_coordinate_token(text) => HighlightKind::Coordinate,
            4 => registry_or_literal(text, BLOCK_IDS, true),
            _ => HighlightKind::StringOrNbt,
        },
        "fill" => match index {
            1..=6 if is_coordinate_token(text) => HighlightKind::Coordinate,
            7 => registry_or_literal(text, BLOCK_IDS, true),
            _ => HighlightKind::StringOrNbt,
        },
        "gamemode" => match index {
            1 => literal_or_invalid(text, GAME_MODES),
            2 => HighlightKind::TargetSelector,
            _ => HighlightKind::StringOrNbt,
        },
        "time" => match index {
            1 => literal_or_invalid(text, &["set", "add", "query"]),
            _ => HighlightKind::StringOrNbt,
        },
        "weather" => match index {
            1 => literal_or_invalid(text, WEATHER_TYPES),
            _ => HighlightKind::StringOrNbt,
        },
        "kill" | "tp" | "teleport" => match index {
            1 => HighlightKind::TargetSelector,
            _ if is_coordinate_token(text) => HighlightKind::Coordinate,
            _ => HighlightKind::StringOrNbt,
        },
        _ => HighlightKind::StringOrNbt,
    }
}

fn literal_or_invalid(text: &str, allowed: &[&str]) -> HighlightKind {
    if allowed.iter().any(|value| value.eq_ignore_ascii_case(text)) {
        HighlightKind::Subcommand
    } else {
        HighlightKind::Invalid
    }
}

fn registry_or_literal(text: &str, known: &[&str], allow_namespaced: bool) -> HighlightKind {
    let normalized = normalize_registry_id(text);
    if known.iter().any(|id| *id == normalized) {
        return HighlightKind::StringOrNbt;
    }
    if allow_namespaced && normalized.contains(':') {
        return HighlightKind::StringOrNbt;
    }
    if !normalized.contains(':') && is_numeric_or_relative(text) {
        return HighlightKind::StringOrNbt;
    }
    HighlightKind::Invalid
}

fn is_selector_token(text: &str) -> bool {
    text.starts_with('@')
}

fn is_coordinate_token(text: &str) -> bool {
    if let Some(rest) = text.strip_prefix('~').or_else(|| text.strip_prefix('^')) {
        return rest.is_empty() || rest.parse::<f64>().is_ok();
    }
    is_numeric_or_relative(text)
}

fn is_numeric_or_relative(text: &str) -> bool {
    if text.is_empty() {
        return false;
    }
    text.chars()
        .all(|ch| ch.is_ascii_digit() || matches!(ch, '+' | '-' | '.' | 'e' | 'E'))
        && text.chars().any(|ch| ch.is_ascii_digit())
        && text.parse::<f64>().is_ok()
}

fn normalized_rank_query(target: &CompletionTarget) -> String {
    target
        .query
        .strip_prefix("minecraft:")
        .unwrap_or(target.query.as_str())
        .to_lowercase()
}

fn rank_candidates(candidates: Vec<CompletionCandidate>, query: &str) -> Vec<CompletionCandidate> {
    let mut ranked = candidates
        .into_iter()
        .filter_map(|candidate| match_rank(query, &candidate).map(|rank| (rank, candidate)))
        .collect::<Vec<_>>();
    ranked.sort_by(|(rank_a, a), (rank_b, b)| {
        rank_a
            .cmp(rank_b)
            .then_with(|| a.id.cmp(&b.id))
            .then_with(|| a.text.cmp(&b.text))
    });
    let mut out = Vec::new();
    for (_, candidate) in ranked {
        if out.iter().any(|existing: &CompletionCandidate| {
            existing.text.eq_ignore_ascii_case(&candidate.text)
        }) {
            continue;
        }
        out.push(candidate);
        if out.len() >= MAX_COMPLETION_CANDIDATES {
            break;
        }
    }
    out
}

fn match_rank(query: &str, candidate: &CompletionCandidate) -> Option<u8> {
    if query.is_empty() {
        return Some(1);
    }
    let keys = [candidate.text.as_str(), candidate.id.as_str()];
    let mut best = None;
    for key in keys {
        let rank = if key.eq_ignore_ascii_case(query) {
            0
        } else if key.len() >= query.len()
            && key
                .get(..query.len())
                .is_some_and(|prefix| prefix.eq_ignore_ascii_case(query))
        {
            1
        } else if key.to_lowercase().contains(query) {
            2
        } else if is_subsequence(query, &key.to_lowercase()) {
            3
        } else {
            continue;
        };
        best = Some(best.map_or(rank, |current: u8| current.min(rank)));
    }
    best
}

fn is_subsequence(query: &str, text: &str) -> bool {
    let mut chars = text.chars();
    for expected in query.chars() {
        loop {
            match chars.next() {
                Some(ch) if ch == expected => break,
                Some(_) => continue,
                None => return false,
            }
        }
    }
    true
}

fn synthesized_id_candidate(target: &CompletionTarget) -> Option<CompletionCandidate> {
    let normalized = normalize_registry_id(&target.query);
    if !is_plausible_namespaced_id(&normalized) {
        return None;
    }
    let (kind, badge, tag) = match target.context {
        CompletionContext::Entities | CompletionContext::SelectorEntities => (
            "entity",
            CompletionBadgeCatalog::mob_badge(&normalized),
            Some(mob_category(&normalized).tag().to_string()),
        ),
        CompletionContext::Items => (
            "item",
            CompletionBadgeCatalog::item_badge(&normalized),
            None,
        ),
        CompletionContext::Effects => (
            "effect",
            CompletionBadgeCatalog::effect_badge(&normalized),
            Some(if effect_is_debuff(&normalized) {
                "Debuff".to_string()
            } else {
                "Buff".to_string()
            }),
        ),
        CompletionContext::Enchants => (
            "enchant",
            CompletionBadgeCatalog::enchant_badge(&normalized, enchant_is_curse(&normalized)),
            Some("Max I".to_string()),
        ),
        CompletionContext::Blocks => (
            "block",
            CompletionBadgeCatalog::block_badge(&normalized),
            None,
        ),
        _ => return None,
    };
    let text = if target.context == CompletionContext::SelectorEntities {
        format!("{}{}", normalized.as_str(), target.suffix)
    } else {
        normalized.clone()
    };
    Some(
        CompletionCandidate::new(
            format!("{kind}/{}", canonical_registry_id(&normalized)),
            text,
            humanize_id(&normalized),
        )
        .with_badge(badge)
        .with_right_tag_option(tag),
    )
}

trait WithRightTagOption {
    fn with_right_tag_option(self, tag: Option<String>) -> Self;
}

impl WithRightTagOption for CompletionCandidate {
    fn with_right_tag_option(mut self, tag: Option<String>) -> Self {
        self.right_tag = tag;
        self
    }
}

fn is_plausible_namespaced_id(id: &str) -> bool {
    let Some((namespace, path)) = id.split_once(':') else {
        return false;
    };
    !namespace.is_empty()
        && !path.is_empty()
        && namespace.chars().chain(path.chars()).all(|ch| {
            ch.is_ascii_lowercase() || ch.is_ascii_digit() || matches!(ch, '_' | '-' | '.' | '/')
        })
}

fn normalize_registry_id(id: &str) -> String {
    let id = id.trim().to_lowercase();
    id.strip_prefix("minecraft:")
        .map(str::to_string)
        .unwrap_or(id)
}

fn canonical_registry_id(id: &str) -> &str {
    id.strip_prefix("minecraft:").unwrap_or(id)
}

fn sprite_key(kind: BadgeKind, id: &str) -> String {
    let folder = match kind {
        BadgeKind::MobFace => "entity",
        BadgeKind::ItemSprite => "item",
        BadgeKind::BlockSwatch => "block",
        BadgeKind::EnchantBook => "item",
        BadgeKind::StatusEffect => "mob_effect",
        BadgeKind::Semantic => "semantic",
    };
    format!("{folder}/{}", canonical_registry_id(id))
}

fn humanize_id(id: &str) -> String {
    let id = canonical_registry_id(id);
    let mut parts = id.split(':');
    let namespace = parts.next().unwrap_or(id);
    let path = parts.next().unwrap_or(namespace);
    let mut label = String::new();
    if path != namespace {
        label.push_str(&title_words(namespace));
        label.push(' ');
    }
    label.push_str(&title_words(path));
    label
}

fn title_words(text: &str) -> String {
    text.split(['_', '-', '.', '/'])
        .filter(|part| !part.is_empty())
        .map(|part| {
            let mut chars = part.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn command_spec(name: &str) -> Option<&'static (&'static str, &'static str, &'static str)> {
    COMMAND_SPECS
        .iter()
        .find(|spec| spec.0.trim_start_matches('/') == name)
}

fn roman_numeral(mut value: u32) -> String {
    if value == 0 {
        return "0".to_string();
    }
    const TABLE: [(u32, &str); 13] = [
        (1000, "M"),
        (900, "CM"),
        (500, "D"),
        (400, "CD"),
        (100, "C"),
        (90, "XC"),
        (50, "L"),
        (40, "XL"),
        (10, "X"),
        (9, "IX"),
        (5, "V"),
        (4, "IV"),
        (1, "I"),
    ];
    let mut out = String::new();
    for (amount, numeral) in TABLE {
        while value >= amount {
            out.push_str(numeral);
            value -= amount;
        }
    }
    out
}

fn generate_glyph(kind: BadgeKind, seed: &str) -> [u8; GLYPH_PIXELS] {
    let mut glyph = match kind {
        BadgeKind::MobFace => glyph_mob_face(seed),
        BadgeKind::ItemSprite => glyph_item(seed),
        BadgeKind::BlockSwatch => glyph_block(seed),
        BadgeKind::EnchantBook => glyph_enchant_book(seed),
        BadgeKind::StatusEffect => glyph_status_effect(seed),
        BadgeKind::Semantic => glyph_semantic(seed),
    };
    if glyph.iter().all(|&px| px == 0) {
        glyph = glyph_fallback(seed, kind as u8);
    }
    glyph
}

fn set_px(glyph: &mut [u8; GLYPH_PIXELS], x: i32, y: i32, value: u8) {
    if (0..GLYPH_SIZE as i32).contains(&x) && (0..GLYPH_SIZE as i32).contains(&y) {
        glyph[(y as usize) * GLYPH_SIZE + x as usize] = value;
    }
}

fn fill_rect(glyph: &mut [u8; GLYPH_PIXELS], x: i32, y: i32, w: i32, h: i32, value: u8) {
    for dy in 0..h {
        for dx in 0..w {
            set_px(glyph, x + dx, y + dy, value);
        }
    }
}

fn fill_disc(glyph: &mut [u8; GLYPH_PIXELS], cx: i32, cy: i32, radius: i32, value: u8) {
    for y in 0..GLYPH_SIZE as i32 {
        for x in 0..GLYPH_SIZE as i32 {
            let dx = x - cx;
            let dy = y - cy;
            if dx * dx + dy * dy <= radius * radius {
                set_px(glyph, x, y, value);
            }
        }
    }
}

fn draw_line(glyph: &mut [u8; GLYPH_PIXELS], x0: i32, y0: i32, x1: i32, y1: i32, value: u8) {
    let mut x = x0;
    let mut y = y0;
    let dx = (x1 - x0).abs();
    let sx = if x0 < x1 { 1 } else { -1 };
    let dy = -(y1 - y0).abs();
    let sy = if y0 < y1 { 1 } else { -1 };
    let mut err = dx + dy;
    loop {
        set_px(glyph, x, y, value);
        if x == x1 && y == y1 {
            break;
        }
        let e2 = 2 * err;
        if e2 >= dy {
            err += dy;
            x += sx;
        }
        if e2 <= dx {
            err += dx;
            y += sy;
        }
    }
}

fn glyph_mob_face(seed: &str) -> [u8; GLYPH_PIXELS] {
    let mut glyph = [0u8; GLYPH_PIXELS];
    let key = seed.to_lowercase();
    let (base, eyes, detail) = if key.contains("creeper") {
        (1, 4, 4)
    } else if key.contains("enderman") || key.contains("warden") {
        (4, 3, 3)
    } else if key.contains("skeleton") || key.contains("stray") {
        (5, 4, 4)
    } else if key.contains("spider") || key.contains("cave_spider") {
        (4, 3, 5)
    } else if key.contains("ghast") {
        (5, 4, 3)
    } else if key.contains("blaze") {
        (3, 4, 4)
    } else if key.contains("slime") || key.contains("magma_cube") {
        (5, 4, 4)
    } else if key.contains("piglin") || key.contains("villager") {
        (2, 4, 5)
    } else {
        (2, 4, 4)
    };

    fill_rect(&mut glyph, 1, 1, 6, 6, base);
    if key.contains("creeper") {
        fill_rect(&mut glyph, 1, 2, 2, 2, eyes);
        fill_rect(&mut glyph, 5, 2, 2, 2, eyes);
        fill_rect(&mut glyph, 2, 4, 4, 2, detail);
        return glyph;
    }
    if key.contains("ghast") {
        fill_rect(&mut glyph, 1, 2, 6, 4, base);
        set_px(&mut glyph, 2, 2, eyes);
        set_px(&mut glyph, 5, 2, eyes);
        set_px(&mut glyph, 2, 5, detail);
        set_px(&mut glyph, 5, 5, detail);
        return glyph;
    }

    set_px(&mut glyph, 2, 2, eyes);
    set_px(&mut glyph, 5, 2, eyes);
    if key.contains("enderman") || key.contains("warden") {
        set_px(&mut glyph, 3, 4, detail);
        set_px(&mut glyph, 4, 4, detail);
        set_px(&mut glyph, 3, 5, detail);
        set_px(&mut glyph, 4, 5, detail);
    } else if key.contains("skeleton") || key.contains("stray") {
        set_px(&mut glyph, 3, 4, detail);
        set_px(&mut glyph, 4, 4, detail);
        set_px(&mut glyph, 3, 3, detail);
        set_px(&mut glyph, 4, 3, detail);
    } else {
        set_px(&mut glyph, 3, 4, detail);
        set_px(&mut glyph, 4, 4, detail);
        set_px(&mut glyph, 3, 5, detail);
        set_px(&mut glyph, 4, 5, detail);
    }
    glyph
}

fn glyph_item(seed: &str) -> [u8; GLYPH_PIXELS] {
    let key = seed.to_lowercase();
    let mut glyph = [0u8; GLYPH_PIXELS];
    if key.contains("pickaxe") {
        draw_line(&mut glyph, 1, 6, 5, 2, 4);
        draw_line(&mut glyph, 2, 1, 6, 2, 1);
        set_px(&mut glyph, 1, 1, 1);
        set_px(&mut glyph, 6, 1, 1);
        set_px(&mut glyph, 6, 3, 1);
        return glyph;
    }
    if key.contains("sword") || key.contains("blade") || key.contains("trident") {
        draw_line(&mut glyph, 1, 6, 6, 1, 1);
        draw_line(&mut glyph, 0, 7, 1, 6, 4);
        fill_rect(&mut glyph, 1, 5, 3, 1, 2);
        set_px(&mut glyph, 0, 7, 3);
        return glyph;
    }
    if key.contains("axe") {
        draw_line(&mut glyph, 1, 6, 4, 2, 4);
        fill_rect(&mut glyph, 3, 1, 4, 3, 2);
        fill_rect(&mut glyph, 3, 1, 4, 1, 1);
        return glyph;
    }
    if key.contains("shovel") || key.contains("hoe") {
        draw_line(&mut glyph, 1, 6, 5, 2, 4);
        fill_disc(&mut glyph, 5, 1, 1, 1);
        return glyph;
    }
    if key.contains("apple") {
        fill_disc(&mut glyph, 4, 4, 3, 1);
        set_px(&mut glyph, 3, 3, 5);
        draw_line(&mut glyph, 4, 1, 4, 2, 4);
        set_px(&mut glyph, 5, 1, 2);
        set_px(&mut glyph, 6, 2, 2);
        return glyph;
    }
    if key.contains("potion") || key.contains("bottle") || key.contains("dragon_breath") {
        fill_rect(&mut glyph, 3, 0, 2, 1, 2);
        fill_rect(&mut glyph, 3, 1, 2, 2, 5);
        fill_rect(&mut glyph, 2, 3, 4, 4, 4);
        fill_rect(&mut glyph, 3, 3, 2, 4, 3);
        set_px(&mut glyph, 2, 3, 5);
        return glyph;
    }
    if key.contains("book") {
        fill_rect(&mut glyph, 1, 1, 6, 6, 1);
        fill_rect(&mut glyph, 2, 2, 4, 4, 5);
        fill_rect(&mut glyph, 1, 2, 1, 4, 4);
        set_px(&mut glyph, 3, 3, 4);
        set_px(&mut glyph, 4, 4, 4);
        return glyph;
    }
    if key.contains("block") || BLOCK_ITEM_HINTS.iter().any(|hint| key.contains(hint)) {
        return glyph_block(&key);
    }
    if key.contains("bow") {
        draw_line(&mut glyph, 2, 1, 2, 6, 4);
        draw_line(&mut glyph, 2, 1, 5, 3, 4);
        draw_line(&mut glyph, 2, 6, 5, 4, 4);
        draw_line(&mut glyph, 2, 3, 6, 3, 5);
        return glyph;
    }
    if key.contains("arrow") {
        draw_line(&mut glyph, 1, 4, 6, 4, 4);
        fill_rect(&mut glyph, 6, 3, 1, 3, 1);
        set_px(&mut glyph, 1, 3, 2);
        set_px(&mut glyph, 1, 5, 2);
        return glyph;
    }
    if key.contains("ingot")
        || key.contains("nugget")
        || key.contains("gem")
        || key.contains("shard")
    {
        fill_rect(&mut glyph, 3, 1, 2, 1, 3);
        fill_rect(&mut glyph, 2, 2, 4, 1, 3);
        fill_rect(&mut glyph, 1, 3, 6, 2, 3);
        fill_rect(&mut glyph, 2, 5, 4, 1, 3);
        fill_rect(&mut glyph, 3, 6, 2, 1, 3);
        set_px(&mut glyph, 3, 3, 5);
        return glyph;
    }
    glyph_fallback(seed, 1)
}

fn glyph_block(seed: &str) -> [u8; GLYPH_PIXELS] {
    let key = seed.to_lowercase();
    let mut glyph = [0u8; GLYPH_PIXELS];
    fill_rect(&mut glyph, 1, 1, 6, 6, 1);
    fill_rect(&mut glyph, 1, 1, 6, 1, 5);
    fill_rect(&mut glyph, 1, 1, 1, 6, 4);
    fill_rect(&mut glyph, 6, 1, 1, 6, 4);
    fill_rect(&mut glyph, 1, 6, 6, 1, 4);

    if key.contains("ore") {
        let spots = [(2, 2), (5, 2), (3, 4), (5, 5), (2, 5)];
        for (x, y) in spots {
            set_px(&mut glyph, x, y, 3);
        }
    } else if key.contains("glass") {
        fill_rect(&mut glyph, 2, 2, 4, 4, 5);
        set_px(&mut glyph, 3, 3, 1);
        set_px(&mut glyph, 4, 4, 1);
    } else if key.contains("log") || key.contains("stem") || key.contains("hyphae") {
        for y in 2..6 {
            set_px(&mut glyph, 2, y, 2);
            set_px(&mut glyph, 5, y, 2);
        }
    } else if key.contains("planks") || key.contains("wood") {
        for x in 2..6 {
            set_px(&mut glyph, x, 2, 2);
            set_px(&mut glyph, x, 4, 2);
        }
    } else if key.contains("leaves") || key.contains("grass") {
        let hash = stable_hash(seed);
        for y in 2..6 {
            for x in 2..6 {
                if (hash >> ((x + y * 8) % 32)) & 1 == 1 {
                    set_px(&mut glyph, x, y, 2);
                }
            }
        }
    } else {
        let hash = stable_hash(seed);
        for y in 2..6 {
            for x in 2..6 {
                if (hash >> ((x * 3 + y * 5) % 32)) & 1 == 1 {
                    set_px(&mut glyph, x, y, 2);
                }
                if (hash >> ((x * 5 + y * 3 + 7) % 32)) & 1 == 1 {
                    set_px(&mut glyph, x, y, 3);
                }
            }
        }
    }
    glyph
}

fn glyph_enchant_book(seed: &str) -> [u8; GLYPH_PIXELS] {
    let mut glyph = [0u8; GLYPH_PIXELS];
    fill_rect(&mut glyph, 1, 1, 6, 6, 1);
    fill_rect(&mut glyph, 2, 2, 4, 4, 5);
    fill_rect(&mut glyph, 1, 2, 1, 4, 4);
    for i in 0..5 {
        set_px(&mut glyph, 2 + i, 2 + i, 3);
        set_px(&mut glyph, 6 - i, 2 + i, 3);
    }
    let hash = stable_hash(seed);
    for i in 0..8 {
        if (hash >> i) & 1 == 1 {
            set_px(&mut glyph, 1 + i, 6 - i, 3);
        }
    }
    glyph
}

fn glyph_status_effect(seed: &str) -> [u8; GLYPH_PIXELS] {
    let mut glyph = [0u8; GLYPH_PIXELS];
    let key = seed.to_lowercase();
    fill_rect(&mut glyph, 3, 0, 2, 1, 2);
    fill_rect(&mut glyph, 3, 1, 2, 1, 5);
    fill_rect(&mut glyph, 2, 2, 4, 5, 4);
    fill_rect(&mut glyph, 3, 3, 2, 4, 3);
    if key.contains("speed") || key.contains("haste") {
        draw_line(&mut glyph, 1, 2, 3, 2, 6);
        draw_line(&mut glyph, 1, 4, 3, 4, 6);
        draw_line(&mut glyph, 1, 6, 3, 6, 6);
    } else if key.contains("strength") || key.contains("health") || key.contains("regeneration") {
        set_px(&mut glyph, 3, 4, 5);
        set_px(&mut glyph, 4, 4, 5);
        set_px(&mut glyph, 3, 5, 5);
        set_px(&mut glyph, 4, 5, 5);
    } else if key.contains("poison") || key.contains("wither") || key.contains("weakness") {
        set_px(&mut glyph, 3, 4, 1);
        set_px(&mut glyph, 4, 4, 1);
        set_px(&mut glyph, 3, 5, 1);
        set_px(&mut glyph, 4, 6, 1);
    } else {
        set_px(&mut glyph, 3, 4, 5);
        set_px(&mut glyph, 4, 5, 5);
    }
    glyph
}

fn glyph_semantic(seed: &str) -> [u8; GLYPH_PIXELS] {
    let mut glyph = [0u8; GLYPH_PIXELS];
    fill_disc(&mut glyph, 3, 3, 3, 1);
    fill_disc(&mut glyph, 3, 3, 2, 5);
    let hash = stable_hash(seed);
    for i in 0..8 {
        if (hash >> i) & 1 == 1 {
            set_px(&mut glyph, 1 + i, 3, 3);
        }
    }
    glyph
}

fn glyph_fallback(seed: &str, salt: u8) -> [u8; GLYPH_PIXELS] {
    let mut glyph = [0u8; GLYPH_PIXELS];
    fill_rect(&mut glyph, 1, 1, 6, 6, 1);
    let hash = stable_hash(&format!("{seed}:{salt}"));
    for y in 1..7 {
        for x in 1..7 {
            let bit = ((x * 7 + y * 11 + salt as usize) % 32) as u32;
            if (hash >> bit) & 1 == 1 {
                let mirrored_x = 7 - x;
                set_px(&mut glyph, x as i32, y as i32, 2);
                set_px(&mut glyph, mirrored_x as i32, y as i32, 2);
            }
            if (hash >> ((bit + 5) % 32)) & 1 == 1 {
                set_px(&mut glyph, x as i32, y as i32, 3);
            }
        }
    }
    set_px(&mut glyph, 3, 3, 4);
    set_px(&mut glyph, 4, 3, 4);
    set_px(&mut glyph, 3, 4, 4);
    set_px(&mut glyph, 4, 4, 4);
    glyph
}

fn stable_hash(text: &str) -> u64 {
    megu_core::hash::fnv1a64_str(text)
}

fn badge_palette(kind: BadgeKind, sprite_key: &str, accent: [u8; 4]) -> [[u8; 4]; 8] {
    let key = sprite_key.to_lowercase();
    let (primary, secondary) = match kind {
        BadgeKind::MobFace => mob_palette_colors(&key),
        BadgeKind::ItemSprite => item_palette_colors(&key),
        BadgeKind::BlockSwatch => block_palette_colors(&key),
        BadgeKind::EnchantBook => ([172, 96, 220, 255], [96, 62, 140, 255]),
        BadgeKind::StatusEffect => ([226, 226, 226, 255], [140, 140, 150, 255]),
        BadgeKind::Semantic => ([92, 98, 118, 255], [58, 62, 76, 255]),
    };
    let mid = [
        ((primary[0] as u16 + secondary[0] as u16) / 2) as u8,
        ((primary[1] as u16 + secondary[1] as u16) / 2) as u8,
        ((primary[2] as u16 + secondary[2] as u16) / 2) as u8,
        255,
    ];
    [
        [0, 0, 0, 0],
        primary,
        secondary,
        accent,
        [32, 32, 36, 255],
        [232, 232, 236, 255],
        mid,
        [255, 255, 255, 255],
    ]
}

fn mob_palette_colors(key: &str) -> ([u8; 4], [u8; 4]) {
    if key.contains("creeper") {
        ([72, 148, 62, 255], [44, 96, 38, 255])
    } else if key.contains("enderman") {
        ([34, 30, 42, 255], [72, 44, 96, 255])
    } else if key.contains("skeleton") || key.contains("stray") {
        ([226, 222, 206, 255], [150, 146, 132, 255])
    } else if key.contains("zombie") || key.contains("husk") || key.contains("drowned") {
        ([96, 138, 72, 255], [58, 88, 46, 255])
    } else if key.contains("spider") {
        ([44, 38, 42, 255], [24, 20, 24, 255])
    } else if key.contains("ghast") || key.contains("blaze") {
        ([238, 220, 170, 255], [190, 150, 80, 255])
    } else if key.contains("slime") || key.contains("magma") {
        ([126, 196, 96, 255], [72, 132, 58, 255])
    } else if key.contains("warden") {
        ([38, 46, 58, 255], [20, 24, 34, 255])
    } else if key.contains("piglin") || key.contains("hoglin") {
        ([178, 106, 72, 255], [116, 64, 44, 255])
    } else if key.contains("villager") {
        ([168, 128, 82, 255], [112, 82, 52, 255])
    } else {
        let hash = stable_hash(key);
        let palette = [
            ([168, 132, 92, 255], [112, 84, 56, 255]),
            ([146, 104, 70, 255], [96, 66, 44, 255]),
            ([122, 96, 72, 255], [80, 60, 46, 255]),
            ([150, 118, 88, 255], [98, 76, 56, 255]),
        ];
        palette[(hash % palette.len() as u64) as usize]
    }
}

fn item_palette_colors(key: &str) -> ([u8; 4], [u8; 4]) {
    if key.contains("sword") || key.contains("axe") || key.contains("pickaxe") {
        ([190, 196, 204, 255], [96, 102, 112, 255])
    } else if key.contains("gold") || key.contains("golden") {
        ([238, 196, 72, 255], [164, 124, 34, 255])
    } else if key.contains("diamond") || key.contains("netherite") {
        ([96, 190, 220, 255], [52, 110, 140, 255])
    } else if key.contains("iron") || key.contains("chainmail") {
        ([188, 192, 198, 255], [120, 124, 132, 255])
    } else if key.contains("wood") || key.contains("stick") || key.contains("log") {
        ([142, 96, 56, 255], [88, 58, 34, 255])
    } else if key.contains("apple") || key.contains("food") || key.contains("bread") {
        ([196, 92, 52, 255], [132, 58, 32, 255])
    } else if key.contains("potion") || key.contains("bottle") {
        ([150, 92, 210, 255], [92, 52, 140, 255])
    } else if key.contains("book") {
        ([142, 88, 48, 255], [88, 52, 28, 255])
    } else {
        ([150, 150, 156, 255], [96, 96, 104, 255])
    }
}

fn block_palette_colors(key: &str) -> ([u8; 4], [u8; 4]) {
    if key.contains("water") || key.contains("ice") || key.contains("packed_ice") {
        ([72, 140, 220, 255], [40, 88, 150, 255])
    } else if key.contains("lava") || key.contains("magma") || key.contains("fire") {
        ([220, 110, 44, 255], [150, 62, 24, 255])
    } else if key.contains("grass") || key.contains("leaves") || key.contains("vine") {
        ([86, 158, 72, 255], [48, 104, 44, 255])
    } else if key.contains("dirt") || key.contains("mud") || key.contains("soil") {
        ([128, 92, 58, 255], [82, 56, 34, 255])
    } else if key.contains("stone") || key.contains("deepslate") || key.contains("tuff") {
        ([128, 128, 132, 255], [82, 82, 88, 255])
    } else if key.contains("sand") || key.contains("quartz") {
        ([214, 196, 140, 255], ([150, 132, 88, 255]))
    } else if key.contains("ore") {
        ([150, 120, 90, 255], [96, 74, 52, 255])
    } else if key.contains("wool") || key.contains("concrete") {
        ([170, 170, 178, 255], [110, 110, 120, 255])
    } else if key.contains("planks") || key.contains("log") || key.contains("wood") {
        ([142, 96, 56, 255], [88, 58, 34, 255])
    } else {
        let hash = stable_hash(key);
        let palette = [
            ([150, 150, 156, 255], [96, 96, 104, 255]),
            ([120, 124, 132, 255], [76, 80, 90, 255]),
            ([140, 132, 120, 255], [92, 84, 74, 255]),
            ([112, 118, 128, 255], [70, 76, 88, 255]),
        ];
        palette[(hash % palette.len() as u64) as usize]
    }
}

fn item_accent(id: &str) -> [u8; 4] {
    let key = id.to_lowercase();
    if key.contains("netherite")
        || key.contains("elytra")
        || key.contains("totem")
        || key.contains("trident")
    {
        [190, 96, 220, 255]
    } else if key.contains("diamond") || key.contains("emerald") || key.contains("amethyst") {
        [86, 180, 230, 255]
    } else if key.contains("gold") || key.contains("golden") {
        [232, 190, 72, 255]
    } else if key.contains("iron") || key.contains("chainmail") {
        [190, 194, 200, 255]
    } else if key.contains("wood") || key.contains("stick") || key.contains("log") {
        [150, 104, 62, 255]
    } else {
        let hash = stable_hash(&key);
        [
            120 + (hash % 80) as u8,
            120 + ((hash >> 8) % 80) as u8,
            120 + ((hash >> 16) % 80) as u8,
            255,
        ]
    }
}

fn block_accent(id: &str) -> [u8; 4] {
    let key = id.to_lowercase();
    if key.contains("ore") {
        [220, 190, 90, 255]
    } else if key.contains("water") || key.contains("ice") {
        [80, 150, 230, 255]
    } else if key.contains("lava") || key.contains("fire") {
        [230, 110, 50, 255]
    } else if key.contains("grass") || key.contains("leaves") {
        [100, 180, 80, 255]
    } else {
        let hash = stable_hash(&key);
        [
            120 + (hash % 80) as u8,
            120 + ((hash >> 8) % 80) as u8,
            120 + ((hash >> 16) % 80) as u8,
            255,
        ]
    }
}

fn mob_category(id: &str) -> MobCategory {
    let key = id.to_lowercase();
    if key.contains("ender_dragon") || key == "wither" || key.contains("wither_boss") {
        return MobCategory::Boss;
    }
    if key.contains("happy_ghast") {
        return MobCategory::Passive;
    }
    if [
        "cod",
        "salmon",
        "pufferfish",
        "tropical_fish",
        "squid",
        "glow_squid",
        "dolphin",
        "axolotl",
        "turtle",
        "frog",
        "tadpole",
        "guardian",
        "elder_guardian",
    ]
    .iter()
    .any(|hint| key.contains(hint))
    {
        if key.contains("guardian") {
            return MobCategory::Hostile;
        }
        return MobCategory::Water;
    }
    if key == "bat" {
        return MobCategory::Ambient;
    }
    if [
        "zombie",
        "husk",
        "drowned",
        "zombified_piglin",
        "creeper",
        "skeleton",
        "stray",
        "wither_skeleton",
        "spider",
        "cave_spider",
        "silverfish",
        "endermite",
        "enderman",
        "witch",
        "slime",
        "magma_cube",
        "ghast",
        "blaze",
        "warden",
        "pillager",
        "vindicator",
        "evoker",
        "ravager",
        "hoglin",
        "piglin",
        "piglin_brute",
        "phantom",
        "shulker",
        "breeze",
        "bogged",
        "creaking",
        "vex",
        "illusioner",
    ]
    .iter()
    .any(|hint| key.contains(hint))
    {
        return MobCategory::Hostile;
    }
    if [
        "villager",
        "wandering_trader",
        "cow",
        "pig",
        "sheep",
        "chicken",
        "horse",
        "donkey",
        "mule",
        "llama",
        "cat",
        "wolf",
        "fox",
        "rabbit",
        "goat",
        "mooshroom",
        "ocelot",
        "parrot",
        "bee",
        "polar_bear",
        "panda",
        "iron_golem",
        "snow_golem",
        "camel",
        "sniffer",
        "armadillo",
        "allay",
    ]
    .iter()
    .any(|hint| key.contains(hint))
    {
        return MobCategory::Passive;
    }
    MobCategory::Other
}

fn effect_is_debuff(id: &str) -> bool {
    let key = id.to_lowercase();
    [
        "slowness",
        "mining_fatigue",
        "instant_damage",
        "nausea",
        "blindness",
        "hunger",
        "weakness",
        "poison",
        "wither",
        "levitation",
        "darkness",
        "bad_omen",
        "trial_omen",
        "raid_omen",
        "wind_charged",
        "weaving",
        "oozing",
        "infested",
        "unluck",
    ]
    .iter()
    .any(|hint| key.contains(hint))
}

fn enchant_is_curse(id: &str) -> bool {
    let key = id.to_lowercase();
    key.contains("binding_curse") || key.contains("vanishing_curse")
}

fn command_accent(name: &str) -> [u8; 4] {
    let hash = stable_hash(name);
    [
        90 + (hash % 90) as u8,
        120 + ((hash >> 8) % 80) as u8,
        140 + ((hash >> 16) % 80) as u8,
        255,
    ]
}

fn prev_char_boundary(input: &str, cursor: usize) -> usize {
    input[..cursor]
        .char_indices()
        .next_back()
        .map(|(index, _)| index)
        .unwrap_or(0)
}

fn next_char_boundary(input: &str, cursor: usize) -> usize {
    input[cursor..]
        .chars()
        .next()
        .map(|ch| cursor + ch.len_utf8())
        .unwrap_or(input.len())
}

fn floor_char_boundary(input: &str, mut cursor: usize) -> usize {
    cursor = cursor.min(input.len());
    while cursor > 0 && !input.is_char_boundary(cursor) {
        cursor -= 1;
    }
    cursor
}

/// Every vanilla command name the bar completes, in `COMMAND_SPECS` order —
/// used to seed the session's `CommandDispatcher` so `complete_suggestions`
/// answers the same names the bar highlights.
pub fn vanilla_command_names() -> impl Iterator<Item = &'static str> {
    COMMAND_SPECS.iter().map(|(_, name, _)| *name)
}

/// Seed a kernel [`CommandDispatcher`] with the same vanilla literal set
/// `LiveSession` installs and return the literals its
/// `complete_suggestions` accepts for `query` — the text after the
/// leading `/`, matching the wire path's strip convention. `None` on a
/// dispatcher error (caller keeps the unfiltered spec list);
/// `Some(vec![])` means the wire path genuinely offers nothing.
fn command_completion_filter(query: &str) -> Option<Vec<String>> {
    let mut dispatcher = CommandDispatcher::new();
    for name in vanilla_command_names() {
        dispatcher.register_literal(name).ok()?;
    }
    let input = query.trim_start_matches('/');
    let suggestions = dispatcher.complete_suggestions(input, input.len()).ok()?;
    let mut accepted = Vec::with_capacity(suggestions.len());
    for index in 0..suggestions.len() {
        accepted.push(suggestions.get(index)?.to_string());
    }
    Some(accepted)
}

static COMMAND_SPECS: &[(&str, &str, &str)] = &[
    ("/help", "help", "/help [command]"),
    ("/list", "list", "/list [players]"),
    ("/me", "me", "/me <action>"),
    ("/say", "say", "/say <message>"),
    ("/tell", "tell", "/tell <target> <message>"),
    ("/msg", "msg", "/msg <target> <message>"),
    ("/w", "w", "/w <target> <message>"),
    ("/teammsg", "teammsg", "/teammsg <message>"),
    ("/give", "give", "/give <target> <item> [count]"),
    ("/clear", "clear", "/clear [target] [item] [count]"),
    ("/item", "item", "/item <modifier> <target> <slot> <item>"),
    ("/summon", "summon", "/summon <entity> [pos] [nbt]"),
    (
        "/effect",
        "effect",
        "/effect give|clear <target> [effect] [seconds] [amplifier]",
    ),
    (
        "/enchant",
        "enchant",
        "/enchant <target> <enchantment> [level]",
    ),
    ("/setblock", "setblock", "/setblock <pos> <block> [mode]"),
    ("/fill", "fill", "/fill <from> <to> <block> [mode]"),
    ("/gamemode", "gamemode", "/gamemode <mode> [target]"),
    ("/time", "time", "/time set|add|query <value>"),
    (
        "/weather",
        "weather",
        "/weather <clear|rain|thunder> [duration]",
    ),
    ("/tp", "tp", "/tp <target> <destination>"),
    ("/teleport", "teleport", "/teleport <target> <destination>"),
    ("/kill", "kill", "/kill <target>"),
    ("/execute", "execute", "/execute <subcommand>"),
    (
        "/scoreboard",
        "scoreboard",
        "/scoreboard <objective|players|teams>",
    ),
    ("/data", "data", "/data <get|merge|remove> <target> <path>"),
    ("/loot", "loot", "/loot <spawn|give|replace> ..."),
    ("/recipe", "recipe", "/recipe <give|take> <player> <recipe>"),
    (
        "/advancement",
        "advancement",
        "/advancement <grant|revoke> <player> <advancement>",
    ),
    ("/function", "function", "/function <name> [args]"),
    ("/schedule", "schedule", "/schedule <function> <time>"),
    ("/locate", "locate", "/locate <structure|biome> <query>"),
    (
        "/spreadplayers",
        "spreadplayers",
        "/spreadplayers <center> <range> <spacing> <maxRange> <team|players>",
    ),
    (
        "/playsound",
        "playsound",
        "/playsound <sound> <target> [pos] [volume] [pitch]",
    ),
    ("/stopsound", "stopsound", "/stopsound <target> [sound]"),
    (
        "/title",
        "title",
        "/title <target> <title|subtitle|actionbar|times|clear|reset>",
    ),
    ("/tellraw", "tellraw", "/tellraw <target> <json>"),
    ("/seed", "seed", "/seed"),
    (
        "/difficulty",
        "difficulty",
        "/difficulty <peaceful|easy|normal|hard>",
    ),
    ("/gamerule", "gamerule", "/gamerule <rule> [value]"),
    (
        "/defaultgamemode",
        "defaultgamemode",
        "/defaultgamemode <mode>",
    ),
    ("/spawnpoint", "spawnpoint", "/spawnpoint [target] [pos]"),
    ("/setworldspawn", "setworldspawn", "/setworldspawn [pos]"),
    (
        "/forceload",
        "forceload",
        "/forceload add|remove <from> <to>",
    ),
    (
        "/whitelist",
        "whitelist",
        "/whitelist <on|off|add|remove|list|reload>",
    ),
    ("/op", "op", "/op <players>"),
    ("/deop", "deop", "/deop <players>"),
    ("/kick", "kick", "/kick <players> [reason]"),
    ("/ban", "ban", "/ban <players> [reason]"),
    ("/pardon", "pardon", "/pardon <players>"),
    ("/save-all", "save-all", "/save-all [flush]"),
    ("/save-off", "save-off", "/save-off"),
    ("/save-on", "save-on", "/save-on"),
    ("/stop", "stop", "/stop"),
];

const SELECTOR_CANDIDATES: &[(&str, &str, &str)] = &[
    ("@a", "selector/all_players", "All Players"),
    ("@e", "selector/all_entities", "All Entities"),
    ("@p", "selector/nearest_player", "Nearest Player"),
    ("@r", "selector/random_player", "Random Player"),
    ("@s", "selector/self", "Self"),
];

const GAME_MODES: &[&str] = &["survival", "creative", "adventure", "spectator"];
const TIME_VALUES: &[&str] = &["day", "night", "noon", "midnight", "sunrise", "sunset"];
const TIME_QUERY_VALUES: &[&str] = &["day", "daytime", "game_time", "time_of_day"];
const WEATHER_TYPES: &[&str] = &["clear", "rain", "thunder"];

const ENTITY_IDS: &[&str] = &[
    "allay",
    "armadillo",
    "axolotl",
    "bat",
    "bee",
    "blaze",
    "bogged",
    "breeze",
    "camel",
    "cat",
    "cave_spider",
    "chicken",
    "cod",
    "copper_golem",
    "cow",
    "creaking",
    "creeper",
    "dolphin",
    "donkey",
    "drowned",
    "elder_guardian",
    "end_crystal",
    "ender_dragon",
    "enderman",
    "endermite",
    "evoker",
    "fox",
    "frog",
    "ghast",
    "glow_squid",
    "goat",
    "guardian",
    "happy_ghast",
    "hoglin",
    "horse",
    "husk",
    "iron_golem",
    "llama",
    "magma_cube",
    "mooshroom",
    "mule",
    "ocelot",
    "panda",
    "parrot",
    "phantom",
    "pig",
    "piglin",
    "piglin_brute",
    "pillager",
    "polar_bear",
    "pufferfish",
    "rabbit",
    "ravager",
    "salmon",
    "sheep",
    "shulker",
    "silverfish",
    "skeleton",
    "slime",
    "sniffer",
    "snow_golem",
    "spider",
    "squid",
    "stray",
    "strider",
    "tadpole",
    "trader_llama",
    "tropical_fish",
    "turtle",
    "vex",
    "villager",
    "vindicator",
    "wandering_trader",
    "warden",
    "witch",
    "wither",
    "wither_skeleton",
    "wolf",
    "zoglin",
    "zombie",
    "zombie_horse",
    "zombie_villager",
    "zombified_piglin",
];

const ITEM_IDS: &[&str] = &[
    "arrow",
    "baked_potato",
    "beetroot",
    "bone",
    "book",
    "bowl",
    "bread",
    "bucket",
    "bundle",
    "carrot",
    "chainmail_boots",
    "chainmail_chestplate",
    "chainmail_helmet",
    "chainmail_leggings",
    "charcoal",
    "chest",
    "clock",
    "coal",
    "compass",
    "cookie",
    "cooked_beef",
    "cooked_chicken",
    "cooked_cod",
    "cooked_mutton",
    "cooked_porkchop",
    "cooked_rabbit",
    "cooked_salmon",
    "crossbow",
    "diamond",
    "diamond_axe",
    "diamond_boots",
    "diamond_chestplate",
    "diamond_helmet",
    "diamond_hoe",
    "diamond_leggings",
    "diamond_pickaxe",
    "diamond_shovel",
    "diamond_sword",
    "dirt",
    "echo_shard",
    "egg",
    "elytra",
    "emerald",
    "enchanted_book",
    "enchanted_golden_apple",
    "end_crystal",
    "ender_eye",
    "ender_pearl",
    "experience_bottle",
    "feather",
    "fire_charge",
    "firework_rocket",
    "fishing_rod",
    "flint",
    "flint_and_steel",
    "glass_bottle",
    "glow_ink_sac",
    "glowstone_dust",
    "gold_ingot",
    "golden_apple",
    "golden_boots",
    "golden_carrot",
    "golden_helmet",
    "golden_hoe",
    "golden_leggings",
    "golden_pickaxe",
    "golden_shovel",
    "golden_sword",
    "gunpowder",
    "heart_of_the_sea",
    "heavy_core",
    "ink_sac",
    "iron_axe",
    "iron_boots",
    "iron_chestplate",
    "iron_helmet",
    "iron_hoe",
    "iron_ingot",
    "iron_leggings",
    "iron_pickaxe",
    "iron_shovel",
    "iron_sword",
    "ladder",
    "lantern",
    "lead",
    "leather",
    "leather_boots",
    "leather_chestplate",
    "leather_helmet",
    "leather_leggings",
    "mace",
    "melon_slice",
    "milk_bucket",
    "minecart",
    "music_disc_13",
    "music_disc_cat",
    "music_disc_blocks",
    "music_disc_chirp",
    "music_disc_far",
    "music_disc_mall",
    "music_disc_mellohi",
    "music_disc_stal",
    "music_disc_strad",
    "music_disc_wait",
    "music_disc_ward",
    "name_tag",
    "nether_brick",
    "netherite_ingot",
    "netherite_scrap",
    "netherite_sword",
    "netherite_upgrade_smithing_template",
    "netherite_chestplate",
    "netherite_helmet",
    "netherite_leggings",
    "netherite_boots",
    "netherite_pickaxe",
    "netherite_axe",
    "netherite_shovel",
    "netherite_hoe",
    "nether_star",
    "oak_boat",
    "oak_door",
    "oak_planks",
    "oak_sign",
    "ominous_trial_key",
    "packed_ice",
    "paper",
    "phantom_membrane",
    "pickaxe",
    "potion",
    "prismarine_shard",
    "pumpkin_pie",
    "quartz",
    "rabbit_foot",
    "rabbit_hide",
    "raw_beef",
    "raw_chicken",
    "raw_cod",
    "raw_mutton",
    "raw_porkchop",
    "raw_rabbit",
    "raw_salmon",
    "recovery_compass",
    "redstone",
    "rotten_flesh",
    "saddle",
    "shears",
    "shield",
    "shulker_shell",
    "slime_ball",
    "spectral_arrow",
    "spider_eye",
    "splash_potion",
    "spyglass",
    "stick",
    "stone",
    "stone_axe",
    "stone_pickaxe",
    "stone_sword",
    "string",
    "sugar",
    "sugar_cane",
    "suspicious_stew",
    "sweet_berries",
    "tipped_arrow",
    "torch",
    "totem_of_undying",
    "trident",
    "turtle_helmet",
    "turtle_scute",
    "wind_charge",
    "writable_book",
    "written_book",
    "wooden_axe",
    "wooden_pickaxe",
    "wooden_shovel",
    "wooden_sword",
];

const EFFECT_IDS: &[&str] = &[
    "absorption",
    "bad_omen",
    "blindness",
    "conduit_power",
    "darkness",
    "dolphins_grace",
    "fire_resistance",
    "glowing",
    "haste",
    "health_boost",
    "hero_of_the_village",
    "hunger",
    "infested",
    "instant_damage",
    "instant_health",
    "invisibility",
    "jump_boost",
    "levitation",
    "luck",
    "mining_fatigue",
    "nausea",
    "night_vision",
    "oozing",
    "poison",
    "raid_omen",
    "resistance",
    "saturation",
    "slow_falling",
    "slowness",
    "speed",
    "strength",
    "trial_omen",
    "unluck",
    "water_breathing",
    "weakness",
    "weaving",
    "wind_charged",
    "wither",
];

struct EnchantSpec {
    id: &'static str,
    max_level: u32,
    curse: bool,
}

const ENCHANTS: &[EnchantSpec] = &[
    EnchantSpec {
        id: "aqua_affinity",
        max_level: 1,
        curse: false,
    },
    EnchantSpec {
        id: "bane_of_arthropods",
        max_level: 5,
        curse: false,
    },
    EnchantSpec {
        id: "binding_curse",
        max_level: 1,
        curse: true,
    },
    EnchantSpec {
        id: "blast_protection",
        max_level: 4,
        curse: false,
    },
    EnchantSpec {
        id: "breach",
        max_level: 4,
        curse: false,
    },
    EnchantSpec {
        id: "channeling",
        max_level: 1,
        curse: false,
    },
    EnchantSpec {
        id: "density",
        max_level: 5,
        curse: false,
    },
    EnchantSpec {
        id: "depth_strider",
        max_level: 3,
        curse: false,
    },
    EnchantSpec {
        id: "efficiency",
        max_level: 5,
        curse: false,
    },
    EnchantSpec {
        id: "feather_falling",
        max_level: 4,
        curse: false,
    },
    EnchantSpec {
        id: "fire_aspect",
        max_level: 2,
        curse: false,
    },
    EnchantSpec {
        id: "fire_protection",
        max_level: 4,
        curse: false,
    },
    EnchantSpec {
        id: "flame",
        max_level: 1,
        curse: false,
    },
    EnchantSpec {
        id: "fortune",
        max_level: 3,
        curse: false,
    },
    EnchantSpec {
        id: "frost_walker",
        max_level: 2,
        curse: false,
    },
    EnchantSpec {
        id: "impaling",
        max_level: 5,
        curse: false,
    },
    EnchantSpec {
        id: "infinity",
        max_level: 1,
        curse: false,
    },
    EnchantSpec {
        id: "knockback",
        max_level: 2,
        curse: false,
    },
    EnchantSpec {
        id: "looting",
        max_level: 3,
        curse: false,
    },
    EnchantSpec {
        id: "loyalty",
        max_level: 3,
        curse: false,
    },
    EnchantSpec {
        id: "luck_of_the_sea",
        max_level: 3,
        curse: false,
    },
    EnchantSpec {
        id: "lunge",
        max_level: 3,
        curse: false,
    },
    EnchantSpec {
        id: "lure",
        max_level: 3,
        curse: false,
    },
    EnchantSpec {
        id: "mending",
        max_level: 1,
        curse: false,
    },
    EnchantSpec {
        id: "multishot",
        max_level: 1,
        curse: false,
    },
    EnchantSpec {
        id: "piercing",
        max_level: 4,
        curse: false,
    },
    EnchantSpec {
        id: "power",
        max_level: 5,
        curse: false,
    },
    EnchantSpec {
        id: "projectile_protection",
        max_level: 4,
        curse: false,
    },
    EnchantSpec {
        id: "protection",
        max_level: 5,
        curse: false,
    },
    EnchantSpec {
        id: "punch",
        max_level: 2,
        curse: false,
    },
    EnchantSpec {
        id: "quick_charge",
        max_level: 3,
        curse: false,
    },
    EnchantSpec {
        id: "respiration",
        max_level: 3,
        curse: false,
    },
    EnchantSpec {
        id: "riptide",
        max_level: 3,
        curse: false,
    },
    EnchantSpec {
        id: "silk_touch",
        max_level: 1,
        curse: false,
    },
    EnchantSpec {
        id: "smite",
        max_level: 5,
        curse: false,
    },
    EnchantSpec {
        id: "soul_speed",
        max_level: 3,
        curse: false,
    },
    EnchantSpec {
        id: "swift_sneak",
        max_level: 3,
        curse: false,
    },
    EnchantSpec {
        id: "sweeping_edge",
        max_level: 3,
        curse: false,
    },
    EnchantSpec {
        id: "thorns",
        max_level: 3,
        curse: false,
    },
    EnchantSpec {
        id: "unbreaking",
        max_level: 3,
        curse: false,
    },
    EnchantSpec {
        id: "vanishing_curse",
        max_level: 1,
        curse: true,
    },
    EnchantSpec {
        id: "wind_burst",
        max_level: 3,
        curse: false,
    },
];

const ENCHANT_IDS_ONLY: &[&str] = &[
    "aqua_affinity",
    "bane_of_arthropods",
    "binding_curse",
    "blast_protection",
    "breach",
    "channeling",
    "density",
    "depth_strider",
    "efficiency",
    "feather_falling",
    "fire_aspect",
    "fire_protection",
    "flame",
    "fortune",
    "frost_walker",
    "impaling",
    "infinity",
    "knockback",
    "looting",
    "loyalty",
    "luck_of_the_sea",
    "lunge",
    "lure",
    "mending",
    "multishot",
    "piercing",
    "power",
    "projectile_protection",
    "protection",
    "punch",
    "quick_charge",
    "respiration",
    "riptide",
    "silk_touch",
    "smite",
    "soul_speed",
    "swift_sneak",
    "sweeping_edge",
    "thorns",
    "unbreaking",
    "vanishing_curse",
    "wind_burst",
];

const BLOCK_IDS: &[&str] = &[
    "air",
    "amethyst_block",
    "andesite",
    "anvil",
    "bamboo_planks",
    "barrel",
    "basalt",
    "beacon",
    "bedrock",
    "blackstone",
    "blue_ice",
    "bookshelf",
    "bricks",
    "bundle",
    "calcite",
    "cave_air",
    "cherry_planks",
    "chest",
    "clay",
    "coal_ore",
    "cobbled_deepslate",
    "cobblestone",
    "command_block",
    "composter",
    "conduit",
    "crafting_table",
    "crimson_planks",
    "deepslate",
    "deepslate_diamond_ore",
    "deepslate_emerald_ore",
    "deepslate_gold_ore",
    "deepslate_iron_ore",
    "deepslate_lapis_ore",
    "deepslate_redstone_ore",
    "diamond_block",
    "diamond_ore",
    "diorite",
    "dirt",
    "dried_kelp_block",
    "emerald_block",
    "emerald_ore",
    "enchanting_table",
    "end_portal",
    "end_portal_frame",
    "end_stone",
    "end_stone_bricks",
    "furnace",
    "glass",
    "glass_pane",
    "glowstone",
    "gold_block",
    "gold_ore",
    "granite",
    "grass_block",
    "gravel",
    "hay_block",
    "honey_block",
    "ice",
    "iron_block",
    "iron_ore",
    "jigsaw",
    "jukebox",
    "lantern",
    "lapis_block",
    "lapis_ore",
    "lava",
    "lodestone",
    "mangrove_planks",
    "magma_block",
    "melon",
    "mud",
    "mud_bricks",
    "mushroom_stem",
    "nether_bricks",
    "nether_gold_ore",
    "nether_quartz_ore",
    "netherite_block",
    "netherrack",
    "note_block",
    "oak_log",
    "oak_planks",
    "obsidian",
    "packed_ice",
    "packed_mud",
    "prismarine",
    "pumpkin",
    "purpur_block",
    "quartz_block",
    "quartz_ore",
    "red_nether_bricks",
    "redstone_block",
    "redstone_ore",
    "reinforced_deepslate",
    "sand",
    "sculk",
    "sculk_catalyst",
    "sculk_shrieker",
    "sea_lantern",
    "shroomlight",
    "slime_block",
    "smooth_quartz",
    "soul_sand",
    "soul_soil",
    "spawner",
    "sponge",
    "stone",
    "stone_bricks",
    "structure_block",
    "terracotta",
    "tinted_glass",
    "torch",
    "trapped_chest",
    "tuff",
    "warped_planks",
    "water",
    "wool",
];

const BLOCK_ITEM_HINTS: &[&str] = &[
    "stone",
    "dirt",
    "sand",
    "gravel",
    "clay",
    "glass",
    "wool",
    "concrete",
    "terracotta",
    "planks",
    "log",
    "leaves",
    "ore",
    "block",
    "bricks",
    "stone_bricks",
    "chest",
    "furnace",
    "torch",
    "lantern",
    "obsidian",
    "bedrock",
    "netherrack",
    "quartz",
    "ice",
    "packed_ice",
];
