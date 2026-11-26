// SPDX-License-Identifier: LGPL-3.0-or-later
//! Small gameplay overlays whose ownership is attached to `UiState` by hosts.

use crate::draw::{palette, UiColor, UiDrawList, UiRect};

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
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BookEditorClick {
    Previous,
    Next,
    Done,
    SignAndClose,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BookEditorState {
    pub slot: i16,
    pub pages: Vec<String>,
    pub page: usize,
    pub title: String,
    pub title_entry: bool,
}
impl Default for BookEditorState {
    fn default() -> Self {
        Self::new(0)
    }
}
impl BookEditorState {
    pub fn new(slot: i16) -> Self {
        Self {
            slot,
            pages: vec![String::new()],
            page: 0,
            title: String::new(),
            title_entry: false,
        }
    }
    pub fn type_char(&mut self, c: char) {
        if self.title_entry {
            if !c.is_control() && self.title.chars().count() < 16 {
                self.title.push(c);
            }
        } else if !c.is_control() && self.page_text().len() < 1024 {
            self.page_text_mut().push(c);
        }
    }
    pub fn backspace(&mut self) {
        if self.title_entry {
            self.title.pop();
        } else {
            self.page_text_mut().pop();
        }
    }
    pub fn newline(&mut self) {
        if !self.title_entry && self.page_text().len() < 1024 {
            self.page_text_mut().push('\n');
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
    pub fn click(&mut self, click: BookEditorClick) -> Option<BookEditorAction> {
        match click {
            BookEditorClick::Previous => {
                self.page = self.page.saturating_sub(1);
                None
            }
            BookEditorClick::Next => {
                if self.page + 1 < 100 {
                    if self.page + 1 == self.pages.len() {
                        self.pages.push(String::new());
                    }
                    self.page += 1;
                }
                None
            }
            BookEditorClick::Done => Some(BookEditorAction::Save {
                slot: self.slot,
                pages: self.pages.clone(),
                title: None,
            }),
            BookEditorClick::SignAndClose => {
                self.title_entry = true;
                None
            }
        }
    }
    pub fn submit_title(&mut self) -> BookEditorAction {
        self.title_entry = false;
        BookEditorAction::Save {
            slot: self.slot,
            pages: self.pages.clone(),
            title: Some(self.title.clone()),
        }
    }
    pub fn close(&self) -> BookEditorAction {
        BookEditorAction::Close
    }
    pub fn render(&self, w: u32, h: u32, out: &mut UiDrawList) {
        let r = UiRect::new(w as i32 / 2 - 92, h as i32 / 2 - 110, 184, 220);
        out.fill_rect(r, palette::PANEL);
        out.border_rect(r, palette::PANEL_BORDER, 1);
        if self.title_entry {
            out.text(r.x + 16, r.y + 24, 1, palette::TEXT, "Enter Book Title:");
            let title_box = UiRect::new(r.x + 16, r.y + 44, 152, 16);
            out.fill_rect(title_box, palette::TOOLTIP_BG);
            out.border_rect(title_box, palette::TEXT_FOCUSED, 1);
            out.text(
                title_box.x + 4,
                title_box.y + 4,
                1,
                palette::TEXT,
                format!("{}|", self.title),
            );
            out.text(
                r.x + 20,
                r.bottom() - 26,
                1,
                palette::TEXT,
                "Sign and Close     Cancel",
            );
            return;
        }
        out.text(
            r.x + 12,
            r.y + 12,
            1,
            palette::TEXT,
            self.page_text().chars().take(700).collect::<String>(),
        );
        out.text(
            r.x + 20,
            r.bottom() - 26,
            1,
            palette::TEXT,
            "< Prev     Next >     Done     Sign",
        );
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
            out.text(
                w as i32 / 2 - ((line.len() * 4) as i32),
                y,
                1,
                palette::TEXT,
                line,
            );
        }
    }
}
