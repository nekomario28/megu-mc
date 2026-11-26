// SPDX-License-Identifier: LGPL-3.0-or-later
//! Small gameplay overlays whose ownership is attached to `UiState` by hosts.

use crate::draw::{first_px, palette, text_width, UiColor, UiDrawList, UiRect};
use crate::state::{tr, UiLang};

#[derive(Clone, Debug, Default, PartialEq)]
pub struct BedSleepOverlay {
    pub visible: bool,
}
impl BedSleepOverlay {
    /// Returns true when the pointer is over the leave-bed control; all other
    /// input can be passed through to chat/gameplay by the host.
    pub fn click(&self, x: i32, y: i32, w: u32, h: u32) -> bool {
        let bx = w as i32 / 2 - 48;
        let by = h as i32 / 2 + 12;
        self.visible && UiRect::new(bx, by, 96, 20).contains(x, y)
    }
    pub fn render(&self, w: u32, h: u32, out: &mut UiDrawList) {
        if !self.visible {
            return;
        }
        out.fill_rect(UiRect::new(0, 0, w, h), UiColor::rgba(0, 0, 0, 150));
        out.text(
            w as i32 / 2 - 40,
            h as i32 / 2 - 8,
            2,
            palette::TEXT,
            "Sleeping...",
        );
        let r = UiRect::new(w as i32 / 2 - 48, h as i32 / 2 + 12, 96, 20);
        out.fill_rect(r, palette::BUTTON);
        out.border_rect(r, palette::BUTTON_BORDER, 1);
        out.text(r.x + 17, r.y + 6, 1, palette::TEXT, "Leave Bed");
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BookEditorAction {
    Save {
        slot: i16,
        pages: Vec<String>,
        title: Option<String>,
    },
    Close,
}

/// Legacy semantic click targets retained for existing hosts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BookEditorClick {
    Previous,
    Next,
    Done,
    SignAndClose,
}

/// Mode separating page-body editing from title signing. `Written` is a
/// clean read-only viewer for signed books.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum BookEditorMode {
    #[default]
    Edit,
    Signing,
    Written,
}

/// Explicit clickable controls shared by the editor, signing page, and
/// written-book viewer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BookEditorButton {
    Sign,
    Done,
    Cancel,
    PrevPage,
    NextPage,
}

impl BookEditorButton {
    pub const ALL: [Self; 5] = [
        Self::PrevPage,
        Self::NextPage,
        Self::Done,
        Self::Sign,
        Self::Cancel,
    ];
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BookEditorState {
    pub slot: i16,
    pub pages: Vec<String>,
    pub page: usize,
    pub title: String,
    /// Backward-compatible mirror of `mode == Signing` used by the current host.
    pub title_entry: bool,
    pub mode: BookEditorMode,
    pub author: String,
    pub lang: UiLang,
}

impl Default for BookEditorState {
    fn default() -> Self {
        Self::new(0)
    }
}

impl BookEditorState {
    const PANEL_W: i32 = 220;
    const PANEL_H: i32 = 220;

    pub fn new(slot: i16) -> Self {
        Self {
            slot,
            pages: vec![String::new()],
            page: 0,
            title: String::new(),
            title_entry: false,
            mode: BookEditorMode::Edit,
            author: String::new(),
            lang: UiLang::En,
        }
    }

    /// Open a signed/written book in read-only viewing mode.
    pub fn new_written(
        slot: i16,
        pages: Vec<String>,
        title: impl Into<String>,
        author: impl Into<String>,
    ) -> Self {
        let mut state = Self::new(slot);
        state.pages = if pages.is_empty() { vec![String::new()] } else { pages };
        state.title = title.into();
        state.author = author.into();
        state.mode = BookEditorMode::Written;
        state
    }

    pub fn set_language(&mut self, lang: UiLang) {
        self.lang = lang;
    }

    pub fn is_read_only(&self) -> bool {
        self.mode == BookEditorMode::Written
    }

    fn set_mode(&mut self, mode: BookEditorMode) {
        self.mode = mode;
        self.title_entry = mode == BookEditorMode::Signing;
    }

    pub fn type_char(&mut self, c: char) {
        if self.is_read_only() || c.is_control() {
            return;
        }
        match self.mode {
            BookEditorMode::Signing => {
                if self.title.chars().count() < 16 {
                    self.title.push(c);
                }
            }
            BookEditorMode::Edit => {
                if self.page_text().chars().count() < 1024 {
                    self.page_text_mut().push(c);
                }
            }
            BookEditorMode::Written => {}
        }
    }

    pub fn backspace(&mut self) {
        if self.is_read_only() {
            return;
        }
        match self.mode {
            BookEditorMode::Signing => {
                self.title.pop();
            }
            BookEditorMode::Edit => {
                self.page_text_mut().pop();
            }
            BookEditorMode::Written => {}
        }
    }

    /// Insert a page-body newline. This deliberately does not save; saving is
    /// owned exclusively by the explicit Done/Sign buttons.
    pub fn newline(&mut self) {
        if self.mode == BookEditorMode::Edit && self.page_text().chars().count() < 1024 {
            self.page_text_mut().push('\n');
        }
    }

    /// Keyboard Enter semantics: newline while editing the page body, submit
    /// only while the separate signing prompt is active.
    pub fn press_enter(&mut self) -> Option<BookEditorAction> {
        match self.mode {
            BookEditorMode::Edit => {
                self.newline();
                None
            }
            BookEditorMode::Signing => Some(self.submit_title()),
            BookEditorMode::Written => None,
        }
    }

    fn page_text(&self) -> &str {
        self.pages.get(self.page).map(String::as_str).unwrap_or("")
    }

    fn page_text_mut(&mut self) -> &mut String {
        if self.pages.is_empty() {
            self.pages.push(String::new());
            self.page = 0;
        } else if self.page >= self.pages.len() {
            self.page = self.pages.len() - 1;
        }
        &mut self.pages[self.page]
    }

    fn previous_page(&mut self) {
        self.page = self.page.saturating_sub(1);
    }

    fn next_page(&mut self) {
        if self.is_read_only() {
            if self.page + 1 < self.pages.len() {
                self.page += 1;
            }
        } else if self.page + 1 < 100 {
            if self.page + 1 == self.pages.len() {
                self.pages.push(String::new());
            }
            self.page += 1;
        }
    }

    pub fn click(&mut self, click: BookEditorClick) -> Option<BookEditorAction> {
        match click {
            BookEditorClick::Previous => {
                self.previous_page();
                None
            }
            BookEditorClick::Next => {
                self.next_page();
                None
            }
            BookEditorClick::Done => self.click_button(BookEditorButton::Done),
            BookEditorClick::SignAndClose => self.click_button(BookEditorButton::Sign),
        }
    }

    /// Activate one explicit on-screen button. Signing-mode `Cancel` returns
    /// to page editing (matching Vanilla); edit-mode `Cancel` closes without
    /// saving. Written-book `Done`/`Cancel` close the read-only viewer.
    pub fn click_button(&mut self, button: BookEditorButton) -> Option<BookEditorAction> {
        match button {
            BookEditorButton::PrevPage => {
                self.previous_page();
                None
            }
            BookEditorButton::NextPage => {
                self.next_page();
                None
            }
            BookEditorButton::Sign => match self.mode {
                BookEditorMode::Edit => {
                    self.set_mode(BookEditorMode::Signing);
                    None
                }
                BookEditorMode::Signing if !self.title.is_empty() => Some(self.submit_title()),
                BookEditorMode::Signing | BookEditorMode::Written => None,
            },
            BookEditorButton::Done => match self.mode {
                BookEditorMode::Edit => Some(BookEditorAction::Save {
                    slot: self.slot,
                    pages: self.pages.clone(),
                    title: None,
                }),
                BookEditorMode::Signing => Some(self.submit_title()),
                BookEditorMode::Written => Some(BookEditorAction::Close),
            },
            BookEditorButton::Cancel => match self.mode {
                BookEditorMode::Edit | BookEditorMode::Written => Some(BookEditorAction::Close),
                BookEditorMode::Signing => {
                    self.set_mode(BookEditorMode::Edit);
                    None
                }
            },
        }
    }

    pub fn submit_title(&mut self) -> BookEditorAction {
        self.set_mode(BookEditorMode::Edit);
        BookEditorAction::Save {
            slot: self.slot,
            pages: self.pages.clone(),
            title: Some(self.title.clone()),
        }
    }

    pub fn close(&self) -> BookEditorAction {
        BookEditorAction::Close
    }

    pub fn panel_rect(&self, w: u32, h: u32) -> UiRect {
        UiRect::new(
            w as i32 / 2 - Self::PANEL_W / 2,
            h as i32 / 2 - Self::PANEL_H / 2,
            Self::PANEL_W as u32,
            Self::PANEL_H as u32,
        )
    }

    /// Exact rectangle shared by drawing and hit-testing for `button`.
    pub fn button_rect(&self, button: BookEditorButton, w: u32, h: u32) -> UiRect {
        let panel = self.panel_rect(w, h);
        let y = panel.bottom() - 26;
        let (x, width) = match button {
            BookEditorButton::PrevPage => (panel.x + 8, 34),
            BookEditorButton::NextPage => (panel.x + 46, 34),
            BookEditorButton::Done => (panel.x + 84, 38),
            BookEditorButton::Sign => (panel.x + 126, 38),
            BookEditorButton::Cancel => (panel.x + 168, 44),
        };
        UiRect::new(x, y, width, 14)
    }

    pub fn hit_test_button(&self, x: i32, y: i32, w: u32, h: u32) -> Option<BookEditorButton> {
        BookEditorButton::ALL
            .into_iter()
            .find(|button| self.button_rect(*button, w, h).contains(x, y))
    }

    fn draw_button(
        &self,
        out: &mut UiDrawList,
        w: u32,
        h: u32,
        button: BookEditorButton,
        label: &str,
        enabled: bool,
    ) {
        let r = self.button_rect(button, w, h);
        out.fill_rect(r, if enabled { palette::BUTTON } else { palette::BUTTON_DISABLED });
        out.border_rect(r, palette::BUTTON_BORDER, 1);
        let width = text_width(label, 1) as i32;
        out.text(
            r.x + ((r.w as i32 - width) / 2).max(2),
            r.y + 3,
            1,
            if enabled { palette::TEXT } else { palette::BUTTON_DISABLED_TEXT },
            label,
        );
    }

    pub fn render(&self, w: u32, h: u32, out: &mut UiDrawList) {
        let r = self.panel_rect(w, h);
        out.fill_rect(UiRect::new(0, 0, w, h), UiColor::rgba(0, 0, 0, 100));
        out.fill_rect(r, palette::PANEL);
        out.border_rect(r, palette::PANEL_BORDER, 1);
        let lang = self.lang;

        if self.mode == BookEditorMode::Signing {
            out.text(
                r.x + 16,
                r.y + 24,
                1,
                palette::TEXT,
                tr(lang, "Enter Book Title:", "本のタイトル:"),
            );
            let title_box = UiRect::new(r.x + 16, r.y + 44, 188, 16);
            out.fill_rect(title_box, palette::TOOLTIP_BG);
            out.border_rect(title_box, palette::TEXT_FOCUSED, 1);
            out.text(
                title_box.x + 4,
                title_box.y + 4,
                1,
                palette::TEXT,
                format!("{}|", self.title),
            );
        } else {
            if self.mode == BookEditorMode::Written {
                let title = if self.title.is_empty() {
                    tr(lang, "Written Book", "記入済みの本")
                } else {
                    self.title.as_str()
                };
                out.text(r.x + 12, r.y + 10, 1, palette::TEXT, first_px(title, 180).to_owned());
                if !self.author.is_empty() {
                    out.text(
                        r.x + 12,
                        r.y + 21,
                        1,
                        palette::TEXT_DIM,
                        format!("{} {}", tr(lang, "by", "作者:"), self.author),
                    );
                }
            }
            let body_top = r.y + if self.mode == BookEditorMode::Written { 34 } else { 12 };
            let body_bottom = r.bottom() - 34;
            let max_width = (r.w as i32 - 24).max(1) as usize;
            let mut y = body_top;
            for logical_line in self.page_text().lines().chain(std::iter::once("")) {
                let mut rest = logical_line;
                loop {
                    let line = first_px(rest, max_width);
                    if y + 8 > body_bottom {
                        break;
                    }
                    out.text(r.x + 12, y, 1, palette::TEXT, line.to_owned());
                    y += 10;
                    if line.len() == rest.len() || rest.is_empty() {
                        break;
                    }
                    rest = &rest[line.len()..];
                }
                if y + 8 > body_bottom {
                    break;
                }
            }
            let page_label = format!("{} {}/{}", tr(lang, "Page", "ページ"), self.page + 1, self.pages.len().max(1));
            out.text(r.x + 12, r.bottom() - 43, 1, palette::TEXT_DIM, page_label);
        }

        let signing = self.mode == BookEditorMode::Signing;
        let written = self.mode == BookEditorMode::Written;
        self.draw_button(out, w, h, BookEditorButton::PrevPage, tr(lang, "Prev", "前へ"), self.page > 0);
        self.draw_button(
            out,
            w,
            h,
            BookEditorButton::NextPage,
            tr(lang, "Next", "次へ"),
            if written { self.page + 1 < self.pages.len() } else { self.page + 1 < 100 },
        );
        self.draw_button(out, w, h, BookEditorButton::Done, tr(lang, "Done", "完了"), true);
        self.draw_button(
            out,
            w,
            h,
            BookEditorButton::Sign,
            tr(lang, "Sign", "署名"),
            !written && (!signing || !self.title.is_empty()),
        );
        self.draw_button(out, w, h, BookEditorButton::Cancel, tr(lang, "Cancel", "キャンセル"), true);
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EndCreditsAction {
    Finished,
}
#[derive(Clone, Debug, PartialEq)]
pub struct EndCreditsState {
    pub lines: Vec<String>,
    pub scroll: f32,
    pub speed: f32,
}
impl Default for EndCreditsState {
    fn default() -> Self {
        Self {
            lines: Self::fallback_lines(),
            scroll: 0.0,
            speed: 12.0,
        }
    }
}
impl EndCreditsState {
    pub fn fallback_lines() -> Vec<String> {
        [
            "Thank you for playing Minecraft!",
            "",
            "Waking up...",
            "",
            "The End",
            "",
            "Credits",
            "",
            "Written by Mojang Studios",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect()
    }
    pub fn load_text(&mut self, end: &str, postcredits: &str) {
        let lines = end
            .lines()
            .chain(postcredits.lines())
            .map(str::to_owned)
            .collect::<Vec<_>>();
        if !lines.is_empty() {
            self.lines = lines;
        }
    }
    pub fn tick(&mut self, dt: f32, space: bool) {
        self.scroll += dt.max(0.0) * self.speed * if space { 8.0 } else { 1.0 };
    }
    pub fn key_escape(&mut self) -> EndCreditsAction {
        EndCreditsAction::Finished
    }
    pub fn finished(&self, h: u32) -> bool {
        self.scroll > (self.lines.len() as f32 * 12.0 + h as f32)
    }
    pub fn render(&self, w: u32, h: u32, out: &mut UiDrawList) {
        out.fill_rect(UiRect::new(0, 0, w, h), UiColor::rgb(0, 0, 0));
        let scroll_px = self.scroll.max(0.0).floor() as i32;
        for (i, line) in self.lines.iter().enumerate() {
            let y = h as i32 + (i as i32) * 12 - scroll_px;
            if y < -12 {
                continue;
            }
            if y > h as i32 {
                break;
            }
            // Center on rendered glyph advance, not UTF-8 byte length. CJK
            // glyphs are full-width and `line.len()` would push them left.
            let text_w = text_width(line, 1) as i32;
            out.text(w as i32 / 2 - text_w / 2, y, 1, palette::TEXT, line);
        }
    }
}
