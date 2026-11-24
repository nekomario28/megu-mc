// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
#![deny(unsafe_code)]
//! Clean-room models for in-game text entry: the four-line sign editor and
//! the chat/command input line.
//!
//! [`SignEditorState`] mirrors the Vanilla 1.21.1 sign editing surface —
//! four lines of at most [`SIGN_MAX_LINE_CHARS`] characters each — as a
//! pure state machine. [`ChatInputState`] models the Java Edition chat box:
//! a bounded sent-message history with Up/Down recall (including draft
//! restoration), a 256-character buffer, and Tab completion over built-in
//! Vanilla/Megu commands plus the online player list.
//!
//! Both types are UTF-8 safe: every cursor position is maintained on a
//! `char` boundary, so multi-byte input (Japanese, emoji, …) edits and
//! deletes cleanly.

use std::collections::VecDeque;

/// Number of editable lines on a sign.
pub const SIGN_LINE_COUNT: usize = 4;

/// Vanilla sign line cap: 384 px of text at the default glyph advance
/// works out to 45 characters on the clean-room model.
pub const SIGN_MAX_LINE_CHARS: usize = 45;

/// Vanilla chat input cap, in characters (not bytes).
pub const CHAT_MAX_CHARS: usize = 256;

/// Maximum number of sent messages retained for Up/Down recall.
pub const CHAT_HISTORY_CAP: usize = 64;

// ---------------------------------------------------------------------------
// Sign editor
// ---------------------------------------------------------------------------

/// Byte offset of character column `col` inside `s` (`s.len()` when `col`
/// is past the end). Always a `char` boundary.
fn byte_of_char_col(s: &str, col: usize) -> usize {
    s.char_indices()
        .nth(col)
        .map(|(i, _)| i)
        .unwrap_or(s.len())
}

/// Character-column editing state for one sign block face.
///
/// `cursor_col` counts *characters*, not bytes, matching how the vanilla
/// client tracks the caret. `active_line` is always `< SIGN_LINE_COUNT`.
#[derive(Clone, Debug)]
pub struct SignEditorState {
    /// Block position being edited.
    pub pos: (i32, i32, i32),
    /// `true` for the front face, `false` for the back face.
    pub is_front: bool,
    lines: [String; SIGN_LINE_COUNT],
    active_line: usize,
    cursor_col: usize,
}

impl SignEditorState {
    /// Open the editor for the sign face at `pos`. `initial_lines` is
    /// padded/truncated to [`SIGN_LINE_COUNT`] entries of at most
    /// [`SIGN_MAX_LINE_CHARS`] characters.
    pub fn new(pos: (i32, i32, i32), is_front: bool, initial_lines: [String; SIGN_LINE_COUNT]) -> Self {
        let mut lines = [String::new(), String::new(), String::new(), String::new()];
        for (dst, src) in lines.iter_mut().zip(initial_lines) {
            *dst = src.chars().take(SIGN_MAX_LINE_CHARS).collect();
        }
        Self {
            pos,
            is_front,
            lines,
            active_line: 0,
            cursor_col: 0,
        }
    }

    /// The four line buffers, in display order.
    pub fn lines(&self) -> &[String; SIGN_LINE_COUNT] {
        &self.lines
    }

    /// Index of the line the caret is on (`0..SIGN_LINE_COUNT`).
    pub const fn active_line(&self) -> usize {
        self.active_line
    }

    /// Caret column in characters within the active line.
    pub const fn cursor_col(&self) -> usize {
        self.cursor_col
    }

    /// Insert `text` at the caret on the active line.
    ///
    /// Control characters (including `\n`/`\r`) are stripped and insertion
    /// stops once the line reaches [`SIGN_MAX_LINE_CHARS`] characters.
    pub fn insert_str(&mut self, text: &str) {
        let line_idx = self.active_line;
        for ch in text.chars() {
            if ch.is_control() {
                continue;
            }
            let line = &mut self.lines[line_idx];
            if line.chars().count() >= SIGN_MAX_LINE_CHARS {
                break;
            }
            let col = self.cursor_col.min(line.chars().count());
            let at = byte_of_char_col(line, col);
            line.insert(at, ch);
            self.cursor_col = col + 1;
        }
    }

    /// Delete the character before the caret. At column 0 this is a no-op.
    pub fn backspace(&mut self) {
        self.clamp_cursor();
        if self.cursor_col == 0 {
            return;
        }
        let line = &mut self.lines[self.active_line];
        let prev_col = self.cursor_col - 1;
        let at = byte_of_char_col(line, prev_col);
        let next = byte_of_char_col(line, self.cursor_col);
        line.replace_range(at..next, "");
        self.cursor_col = prev_col;
    }

    /// Move the caret one character left, staying on the same line.
    pub fn move_left(&mut self) {
        self.cursor_col = self.cursor_col.saturating_sub(1);
    }

    /// Move the caret one character right, clamped to the line length.
    pub fn move_right(&mut self) {
        let len = self.lines[self.active_line].chars().count();
        self.cursor_col = (self.cursor_col + 1).min(len);
    }

    /// Move the caret to the start of the line above, clamped at line 0.
    /// The column is preserved when possible and clamped to the shorter
    /// target line.
    pub fn move_up(&mut self) {
        if self.active_line > 0 {
            self.active_line -= 1;
        }
        self.clamp_cursor();
    }

    /// Move the caret to the start of the line below, clamped at the last
    /// line.
    pub fn move_down(&mut self) {
        if self.active_line + 1 < SIGN_LINE_COUNT {
            self.active_line += 1;
        }
        self.clamp_cursor();
    }

    /// Advance to the next line, resetting the caret to column 0.
    ///
    /// Returns `true` when Enter was pressed on the last line — the
    /// embedding app should treat that as "done editing" (vanilla
    /// finishes the sign there).
    pub fn enter(&mut self) -> bool {
        if self.active_line + 1 < SIGN_LINE_COUNT {
            self.active_line += 1;
            self.cursor_col = 0;
            false
        } else {
            true
        }
    }

    fn clamp_cursor(&mut self) {
        let len = self.lines[self.active_line].chars().count();
        self.cursor_col = self.cursor_col.min(len);
    }
}

// ---------------------------------------------------------------------------
// Chat input
// ---------------------------------------------------------------------------

/// Built-in command set: the Vanilla single-player/OP commands plus the
/// Megu network commands. Always offered to Tab completion; servers may
/// add more via [`ChatInputState::tab_complete`]'s `extra_commands`.
pub const DEFAULT_COMMANDS: &[&str] = &[
    "/help",
    "/list",
    "/say",
    "/tell",
    "/msg",
    "/tp",
    "/gamemode",
    "/give",
    "/time",
    "/weather",
    "/gamerule",
    "/seed",
    "/spawnpoint",
    "/setworldspawn",
    "/kick",
    "/ban",
    "/pardon",
    "/whitelist",
    "/op",
    "/deop",
    "/stop",
    "/megu",
    "/aero",
    "/hub",
    "/server",
    "/world",
];

/// Chat/command line state: editable buffer, bounded history, Tab
/// completion.
///
/// `cursor_byte` is a byte offset into `buffer` and is always kept on a
/// `char` boundary. `history` is ordered oldest → newest (`back()` is the
/// most recently sent message); `history_cursor` indexes into it while
/// recalling, and `draft_backup` preserves the in-progress message the
/// first time Up is pressed.
#[derive(Clone, Debug)]
pub struct ChatInputState {
    buffer: String,
    cursor_byte: usize,
    history: VecDeque<String>,
    history_cursor: Option<usize>,
    draft_backup: String,
    tab_matches: Vec<String>,
    tab_index: usize,
    tab_prefix_start: usize,
}

impl Default for ChatInputState {
    fn default() -> Self {
        Self::new()
    }
}

impl ChatInputState {
    /// Empty input line with no history.
    pub fn new() -> Self {
        Self {
            buffer: String::new(),
            cursor_byte: 0,
            history: VecDeque::new(),
            history_cursor: None,
            draft_backup: String::new(),
            tab_matches: Vec::new(),
            tab_index: 0,
            tab_prefix_start: 0,
        }
    }

    /// Current buffer contents.
    pub fn buffer(&self) -> &str {
        &self.buffer
    }

    /// Caret position as a byte offset into [`Self::buffer`].
    pub const fn cursor_byte(&self) -> usize {
        self.cursor_byte
    }

    /// Sent-message history, oldest first.
    pub fn history(&self) -> &VecDeque<String> {
        &self.history
    }

    /// Insert `text` at the caret. `\r`/`\n` are stripped and insertion
    /// stops at [`CHAT_MAX_CHARS`] characters.
    pub fn insert_str(&mut self, text: &str) {
        self.reset_completion();
        let room = CHAT_MAX_CHARS.saturating_sub(self.buffer.chars().count());
        if room == 0 {
            return;
        }
        let cleaned: String = text
            .chars()
            .filter(|c| *c != '\r' && *c != '\n')
            .take(room)
            .collect();
        if cleaned.is_empty() {
            return;
        }
        self.buffer
            .insert_str(self.cursor_byte, cleaned.as_str());
        self.cursor_byte += cleaned.len();
    }

    /// Delete the character before the caret.
    pub fn backspace(&mut self) {
        self.reset_completion();
        if self.cursor_byte == 0 {
            return;
        }
        let (start, ch) = self.buffer[..self.cursor_byte]
            .char_indices()
            .next_back()
            .expect("cursor_byte > 0 implies a preceding char");
        self.cursor_byte = start;
        self.buffer
            .replace_range(start..start + ch.len_utf8(), "");
    }

    /// Delete the character at the caret (vanilla Delete key).
    pub fn delete(&mut self) {
        self.reset_completion();
        if let Some(ch) = self.buffer[self.cursor_byte..].chars().next() {
            self.buffer
                .replace_range(self.cursor_byte..self.cursor_byte + ch.len_utf8(), "");
        }
    }

    /// Move the caret one character left.
    pub fn move_left(&mut self) {
        self.reset_completion();
        self.cursor_byte = self.buffer[..self.cursor_byte]
            .char_indices()
            .next_back()
            .map(|(i, _)| i)
            .unwrap_or(0);
    }

    /// Move the caret one character right.
    pub fn move_right(&mut self) {
        self.reset_completion();
        if let Some(ch) = self.buffer[self.cursor_byte..].chars().next() {
            self.cursor_byte += ch.len_utf8();
        }
    }

    /// Move the caret to the start of the buffer.
    pub fn home(&mut self) {
        self.reset_completion();
        self.cursor_byte = 0;
    }

    /// Move the caret to the end of the buffer.
    pub fn end(&mut self) {
        self.reset_completion();
        self.cursor_byte = self.buffer.len();
    }

    /// Send the current buffer.
    ///
    /// Whitespace-only input yields `None`. A non-empty message is pushed
    /// onto history unless it duplicates the most recent entry, history is
    /// capped at [`CHAT_HISTORY_CAP`], and the buffer plus all recall/Tab
    /// state are reset.
    pub fn submit(&mut self) -> Option<String> {
        let message = self.buffer.trim().to_string();
        if message.is_empty() {
            return None;
        }
        if self.history.back() != Some(&message) {
            if self.history.len() >= CHAT_HISTORY_CAP {
                self.history.pop_front();
            }
            self.history.push_back(message.clone());
        }
        self.buffer.clear();
        self.cursor_byte = 0;
        self.history_cursor = None;
        self.draft_backup.clear();
        self.reset_completion();
        Some(message)
    }

    /// Recall the previous sent message (Up arrow).
    ///
    /// The first press saves the current buffer as `draft_backup`; further
    /// presses walk toward older entries. At the oldest entry the caret
    /// stays put.
    pub fn history_up(&mut self) {
        if self.history.is_empty() {
            return;
        }
        self.history_cursor = match self.history_cursor {
            None => {
                self.draft_backup = self.buffer.clone();
                Some(self.history.len() - 1)
            }
            Some(0) => Some(0),
            Some(i) => Some(i - 1),
        };
        self.load_history_entry();
    }

    /// Recall the next sent message, or restore the saved draft once the
    /// walk reaches the newest entry (Down arrow past the bottom).
    pub fn history_down(&mut self) {
        let Some(i) = self.history_cursor else {
            return;
        };
        if i + 1 < self.history.len() {
            self.history_cursor = Some(i + 1);
            self.load_history_entry();
        } else {
            self.history_cursor = None;
            self.buffer = std::mem::take(&mut self.draft_backup);
            self.cursor_byte = self.buffer.len();
            self.reset_completion();
        }
    }

    /// Tab completion over commands and online players.
    ///
    /// When the caret sits on the first token and that token starts with
    /// `/`, candidates are [`DEFAULT_COMMANDS`] plus `extra_commands`
    /// (matched case-insensitively on the part after `/`). Otherwise
    /// candidates are `online_players` matched case-insensitively by
    /// prefix. Repeated calls cycle through the candidate list; any other
    /// edit clears the cycle.
    ///
    /// Returns the newly inserted candidate, or `None` when nothing
    /// matches.
    pub fn tab_complete(
        &mut self,
        online_players: &[String],
        extra_commands: &[String],
    ) -> Option<&str> {
        let (ts, te) = self.token_range();
        let token = &self.buffer[ts..te];

        // Continue an active cycle when the caret is still on the token we
        // last completed (the buffer segment equals the current match).
        if !self.tab_matches.is_empty()
            && ts == self.tab_prefix_start
            && token == self.tab_matches[self.tab_index]
        {
            self.tab_index = (self.tab_index + 1) % self.tab_matches.len();
            let candidate = self.tab_matches[self.tab_index].clone();
            self.buffer.replace_range(ts..te, candidate.as_str());
            self.cursor_byte = ts + candidate.len();
            return self.tab_matches.get(self.tab_index).map(String::as_str);
        }

        let matches = if ts == 0 && token.starts_with('/') {
            let prefix = token[1..].to_lowercase();
            let mut out: Vec<String> = Vec::new();
            for cmd in DEFAULT_COMMANDS
                .iter()
                .map(|s| s.to_string())
                .chain(extra_commands.iter().cloned())
            {
                let name = cmd.strip_prefix('/').unwrap_or(cmd.as_str());
                if name.to_lowercase().starts_with(&prefix) {
                    let display = if cmd.starts_with('/') {
                        cmd
                    } else {
                        format!("/{}", cmd)
                    };
                    if !out.contains(&display) {
                        out.push(display);
                    }
                }
            }
            out
        } else {
            let prefix = token.to_lowercase();
            online_players
                .iter()
                .filter(|p| p.to_lowercase().starts_with(&prefix))
                .cloned()
                .collect()
        };

        if matches.is_empty() {
            self.reset_completion();
            return None;
        }

        self.tab_matches = matches;
        self.tab_index = 0;
        self.tab_prefix_start = ts;
        let candidate = self.tab_matches[0].clone();
        self.buffer.replace_range(ts..te, candidate.as_str());
        self.cursor_byte = ts + candidate.len();
        Some(self.tab_matches[0].as_str())
    }

    /// Byte range `(start, end)` of the whitespace-delimited token around
    /// the caret.
    fn token_range(&self) -> (usize, usize) {
        let cursor = self.cursor_byte;
        let start = self.buffer[..cursor]
            .char_indices()
            .rev()
            .find(|(_, c)| c.is_whitespace())
            .map(|(i, c)| i + c.len_utf8())
            .unwrap_or(0);
        let end = self.buffer[cursor..]
            .char_indices()
            .find(|(_, c)| c.is_whitespace())
            .map(|(i, _)| cursor + i)
            .unwrap_or(self.buffer.len());
        (start, end)
    }

    fn load_history_entry(&mut self) {
        if let Some(i) = self.history_cursor {
            self.buffer = self.history[i].clone();
            self.cursor_byte = self.buffer.len();
        }
        self.reset_completion();
    }

    fn reset_completion(&mut self) {
        self.tab_matches.clear();
        self.tab_index = 0;
        self.tab_prefix_start = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sign() -> SignEditorState {
        SignEditorState::new((1, 64, -3), true, [
            "Hello".to_string(),
            String::new(),
            String::new(),
            String::new(),
        ])
    }

    fn blank_sign() -> SignEditorState {
        SignEditorState::new((0, 0, 0), true, [
            String::new(),
            String::new(),
            String::new(),
            String::new(),
        ])
    }

    fn chat() -> ChatInputState {
        ChatInputState::new()
    }

    fn players() -> Vec<String> {
        vec![
            "Steve".to_string(),
            "Alex".to_string(),
            "Notch".to_string(),
            "PlayerOne".to_string(),
            "Pluto".to_string(),
        ]
    }

    // -- SignEditorState ----------------------------------------------------

    #[test]
    fn sign_new_truncates_lines_and_sets_metadata() {
        let long = "x".repeat(SIGN_MAX_LINE_CHARS + 10);
        let s = SignEditorState::new((0, 0, 0), false, [
            long.clone(),
            "ok".to_string(),
            String::new(),
            String::new(),
        ]);
        assert_eq!(s.pos, (0, 0, 0));
        assert!(!s.is_front);
        assert_eq!(s.lines()[0].chars().count(), SIGN_MAX_LINE_CHARS);
        assert_eq!(s.lines()[1], "ok");
        assert_eq!(s.lines()[2], "");
        assert_eq!(s.active_line(), 0);
        assert_eq!(s.cursor_col(), 0);
    }

    #[test]
    fn sign_insert_and_backspace_ascii() {
        let mut s = sign();
        // Caret starts at column 0; walk to just before the final 'o'.
        for _ in 0..4 {
            s.move_right();
        }
        s.insert_str("LL");
        assert_eq!(s.lines()[0], "HellLLo");
        assert_eq!(s.cursor_col(), 6);
        s.backspace();
        assert_eq!(s.lines()[0], "HellLo");
        assert_eq!(s.cursor_col(), 5);
    }

    #[test]
    fn sign_backspace_at_col_zero_is_noop() {
        let mut s = sign();
        s.backspace();
        assert_eq!(s.lines()[0], "Hello");
        assert_eq!(s.cursor_col(), 0);
    }

    #[test]
    fn sign_utf8_japanese_insert_and_backspace() {
        let mut s = blank_sign();
        s.insert_str("こんにちは");
        assert_eq!(s.lines()[0], "こんにちは");
        assert_eq!(s.cursor_col(), 5);
        s.backspace();
        assert_eq!(s.lines()[0], "こんにち");
        assert_eq!(s.cursor_col(), 4);
        // Byte length is 15 for 5 chars × 3 bytes; caret stays on boundary.
        assert_eq!(s.lines()[0].len(), 12);
    }

    #[test]
    fn sign_line_cap_respected() {
        let mut s = blank_sign();
        s.insert_str(&"a".repeat(SIGN_MAX_LINE_CHARS + 20));
        assert_eq!(s.lines()[0].chars().count(), SIGN_MAX_LINE_CHARS);
        // Inserting more is fully rejected.
        s.insert_str("b");
        assert_eq!(s.lines()[0].chars().count(), SIGN_MAX_LINE_CHARS);
        // Multi-byte chars count once each against the cap.
        let mut s2 = blank_sign();
        s2.insert_str(&"あ".repeat(SIGN_MAX_LINE_CHARS));
        s2.insert_str("い");
        assert_eq!(s2.lines()[0].chars().count(), SIGN_MAX_LINE_CHARS);
    }

    #[test]
    fn sign_strips_control_chars_and_newlines() {
        let mut s = blank_sign();
        s.insert_str("a\nb\rc\u{7}d");
        assert_eq!(s.lines()[0], "abcd");
        assert_eq!(s.cursor_col(), 4);
    }

    #[test]
    fn sign_navigation_between_lines() {
        let mut s = sign(); // "Hello" on line 0
        s.move_down();
        assert_eq!(s.active_line(), 1);
        assert_eq!(s.cursor_col(), 0); // clamped: line 1 empty
        s.move_down();
        s.move_down();
        s.move_down(); // clamps at line 3
        assert_eq!(s.active_line(), 3);
        s.move_up();
        assert_eq!(s.active_line(), 2);
        s.move_up();
        s.move_up();
        s.move_up(); // clamps at line 0
        assert_eq!(s.active_line(), 0);
    }

    #[test]
    fn sign_move_preserves_column_when_possible() {
        let mut s = sign();
        s.move_right();
        s.move_right();
        s.move_right(); // col 3
        s.move_down(); // line 1 empty → col clamps to 0
        assert_eq!(s.cursor_col(), 0);
        s.insert_str("abcd");
        s.move_up(); // back to line 0 "Hello" (len 5)
        assert_eq!(s.cursor_col(), 4);
        s.move_up();
        assert_eq!(s.cursor_col(), 4);
    }

    #[test]
    fn sign_enter_advances_and_finishes_on_last_line() {
        let mut s = sign();
        assert!(!s.enter());
        assert_eq!(s.active_line(), 1);
        assert_eq!(s.cursor_col(), 0);
        assert!(!s.enter());
        assert!(!s.enter());
        assert_eq!(s.active_line(), 3);
        // Enter on line 3 signals done.
        assert!(s.enter());
        assert_eq!(s.active_line(), 3);
    }

    #[test]
    fn sign_insert_at_cursor_mid_line() {
        let mut s = sign();
        s.move_right();
        s.move_right();
        s.move_right(); // col 3, between 'l' and 'o'
        s.insert_str("X");
        assert_eq!(s.lines()[0], "HelXlo");
        assert_eq!(s.cursor_col(), 4);
    }

    // -- ChatInputState: editing --------------------------------------------

    #[test]
    fn chat_insert_and_cursor_moves_utf8() {
        let mut c = chat();
        c.insert_str("こんにちは");
        assert_eq!(c.buffer(), "こんにちは");
        assert_eq!(c.cursor_byte(), 15);
        c.move_left();
        assert_eq!(c.cursor_byte(), 12);
        c.insert_str("!");
        assert_eq!(c.buffer(), "こんにち!は");
        c.backspace();
        assert_eq!(c.buffer(), "こんにちは");
        assert_eq!(c.cursor_byte(), 12);
        c.home();
        assert_eq!(c.cursor_byte(), 0);
        c.delete();
        assert_eq!(c.buffer(), "んにちは");
        c.end();
        c.move_right(); // already at end: no-op
        assert_eq!(c.cursor_byte(), c.buffer().len());
    }

    #[test]
    fn chat_insert_strips_crlf_and_caps_at_256_chars() {
        let mut c = chat();
        c.insert_str("a\nb\rc");
        assert_eq!(c.buffer(), "abc");
        let mut c2 = chat();
        c2.insert_str(&"x".repeat(CHAT_MAX_CHARS + 50));
        assert_eq!(c2.buffer().chars().count(), CHAT_MAX_CHARS);
        // Room accounting is in chars, not bytes.
        let mut c3 = chat();
        c3.insert_str(&"あ".repeat(CHAT_MAX_CHARS));
        c3.insert_str("b");
        assert_eq!(c3.buffer().chars().count(), CHAT_MAX_CHARS);
    }

    #[test]
    fn chat_submit_history_dedup_and_cap() {
        let mut c = chat();
        assert_eq!(c.submit(), None); // empty
        c.insert_str("   ");
        assert_eq!(c.submit(), None); // whitespace-only

        c.insert_str("first");
        assert_eq!(c.submit(), Some("first".to_string()));
        assert_eq!(c.buffer(), "");
        assert_eq!(c.cursor_byte(), 0);

        // Duplicate of the last entry is not pushed again.
        c.insert_str("first");
        assert_eq!(c.submit(), Some("first".to_string()));
        assert_eq!(c.history().len(), 1);

        c.insert_str("second");
        c.submit();
        assert_eq!(c.history().len(), 2);
        assert_eq!(c.history()[0], "first");
        assert_eq!(c.history()[1], "second");

        // Cap at CHAT_HISTORY_CAP, evicting oldest.
        let mut c3 = chat();
        for i in 0..CHAT_HISTORY_CAP + 5 {
            c3.insert_str(&format!("msg{i}"));
            c3.submit();
        }
        assert_eq!(c3.history().len(), CHAT_HISTORY_CAP);
        // 69 pushes (msg0..=msg68); the oldest 5 are evicted, leaving
        // msg5 as the front.
        assert_eq!(c3.history()[0], "msg5");
    }

    #[test]
    fn chat_history_recall_and_draft_restore() {
        let mut c = chat();
        for msg in ["one", "two", "three"] {
            c.insert_str(msg);
            c.submit();
        }
        // Down with no recall active is a no-op.
        c.history_down();
        assert_eq!(c.buffer(), "");

        // Type a draft, then walk history.
        c.insert_str("draft");
        c.history_up();
        assert_eq!(c.buffer(), "three");
        c.history_up();
        assert_eq!(c.buffer(), "two");
        c.history_up();
        assert_eq!(c.buffer(), "one");
        c.history_up(); // oldest: stays
        assert_eq!(c.buffer(), "one");

        // Walk back down; past the newest entry restores the draft.
        c.history_down();
        assert_eq!(c.buffer(), "two");
        c.history_down();
        assert_eq!(c.buffer(), "three");
        c.history_down();
        assert_eq!(c.buffer(), "draft");
        c.history_down(); // recall ended
        assert_eq!(c.buffer(), "draft");

        // Caret lands at end after recall.
        c.history_up();
        assert_eq!(c.cursor_byte(), c.buffer().len());
    }

    // -- ChatInputState: tab completion -------------------------------------

    #[test]
    fn tab_completes_commands_and_cycles() {
        let mut c = chat();
        let p = players();
        c.insert_str("/h");
        // "/h" matches /help and /hub; first Tab inserts the first match.
        assert_eq!(c.tab_complete(&p, &[]), Some("/help"));
        assert_eq!(c.buffer(), "/help");
        // Second Tab cycles to /hub, third wraps to /help.
        assert_eq!(c.tab_complete(&p, &[]), Some("/hub"));
        assert_eq!(c.buffer(), "/hub");
        assert_eq!(c.tab_complete(&p, &[]), Some("/help"));
        assert_eq!(c.buffer(), "/help");
    }

    #[test]
    fn tab_command_matching_is_case_insensitive_and_uses_extras() {
        let mut c = chat();
        let p = players();
        c.insert_str("/ME");
        let extra = vec!["/megablast".to_string()];
        // "/me" prefix matches /megu, /msg? no — msg doesn't start with me.
        // Matches: /megu, plus extra /megablast (sorted? insertion order:
        // defaults first then extras).
        assert_eq!(c.tab_complete(&p, &extra), Some("/megu"));
        assert_eq!(c.tab_complete(&p, &extra), Some("/megablast"));
        assert_eq!(c.buffer(), "/megablast");
    }

    #[test]
    fn tab_no_match_returns_none_and_clears_state() {
        let mut c = chat();
        let p = players();
        c.insert_str("/zzz");
        assert_eq!(c.tab_complete(&p, &[]), None);
        assert_eq!(c.buffer(), "/zzz");

        // A later Tab with a valid prefix starts a fresh cycle.
        let mut c2 = chat();
        c2.insert_str("/he");
        assert_eq!(c2.tab_complete(&p, &[]), Some("/help"));
    }

    #[test]
    fn tab_completes_players_case_insensitively() {
        let mut c = chat();
        let p = players();
        c.insert_str("tell pl");
        // Candidates in online_players order: PlayerOne, Pluto.
        assert_eq!(c.tab_complete(&p, &[]), Some("PlayerOne"));
        assert_eq!(c.buffer(), "tell PlayerOne");
        assert_eq!(c.tab_complete(&p, &[]), Some("Pluto"));
        assert_eq!(c.buffer(), "tell Pluto");
        assert_eq!(c.tab_complete(&p, &[]), Some("PlayerOne"));
    }

    #[test]
    fn tab_player_completion_mid_sentence_and_slash_non_first_token() {
        let mut c = chat();
        let p = players();
        // Player completion works mid-sentence.
        c.insert_str("say hi Not");
        assert_eq!(c.tab_complete(&p, &[]), Some("Notch"));
        assert_eq!(c.buffer(), "say hi Notch");

        // A '/' token that is NOT the first token completes players, not
        // commands — "/he" matches no player name.
        let mut c2 = chat();
        c2.insert_str("say /he");
        assert_eq!(c2.tab_complete(&p, &[]), None);
        assert_eq!(c2.buffer(), "say /he");
    }

    #[test]
    fn tab_cycle_resets_on_any_edit() {
        let mut c = chat();
        let p = players();
        c.insert_str("/h");
        assert_eq!(c.tab_complete(&p, &[]), Some("/help"));
        // Edit breaks the cycle; next Tab starts fresh at index 0.
        c.insert_str("x"); // buffer "/helpx" — no matches now
        assert_eq!(c.tab_complete(&p, &[]), None);

        let mut c2 = chat();
        c2.insert_str("/h");
        assert_eq!(c2.tab_complete(&p, &[]), Some("/help"));
        // Three backspaces leave "/h", matching /help and /hub again.
        c2.backspace();
        c2.backspace();
        c2.backspace();
        assert_eq!(c2.buffer(), "/h");
        assert_eq!(c2.tab_complete(&p, &[]), Some("/help"));
        assert_eq!(c2.tab_complete(&p, &[]), Some("/hub"));
    }

    #[test]
    fn tab_completion_with_empty_prefix() {
        let mut c = chat();
        let p = players();
        // Bare "/" offers every command, cycling through all of them.
        c.insert_str("/");
        let first = c.tab_complete(&p, &[]).unwrap().to_string();
        assert_eq!(first, "/help");
        // Cycling eventually wraps through the whole DEFAULT_COMMANDS set.
        let mut seen = vec![first.clone()];
        for _ in 0..DEFAULT_COMMANDS.len() {
            seen.push(c.tab_complete(&p, &[]).unwrap().to_string());
        }
        assert_eq!(seen.last().unwrap(), &first);
        assert_eq!(seen.len(), DEFAULT_COMMANDS.len() + 1);

        // Trailing space + Tab on empty token completes players.
        let mut c2 = chat();
        c2.insert_str("msg ");
        assert_eq!(c2.tab_complete(&p, &[]), Some("Steve"));
    }
}
