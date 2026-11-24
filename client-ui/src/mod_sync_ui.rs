// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! Mod Sync Diff & Explicit Consent approval screen.
//!
//! When a server's mod list differs from the client's, the host computes
//! a *diff* and hands it to this screen. The screen renders every
//! differing mod with its verdict ([`ModSyncDiffKind`]), where the file
//! comes from ([`ModSyncSource`]), its SPDX license tag, and the SHA-512
//! verification status, then waits for the player's explicit decision.
//!
//! Consent properties this screen guarantees:
//!
//! * **Explicit approval** — files are staged only after
//!   [`ModSyncUiAction::ApproveAndSync`]; keyboard navigation can never
//!   trigger it (`Escape` always means *Decline & Disconnect*).
//! * **Opt-in optional mods** — `Optional` rows carry a checkbox and are
//!   excluded from the plan unless the player ticks them.
//! * **Profile isolation** — the header spells out that everything lands
//!   in `profiles/<server-id>/mods`, so the player's global `mods/`
//!   folder is never touched.
//! * **Guided downloads** — CurseForge rows link out to their project
//!   page ([`ModSyncUiAction::OpenCurseForgePage`]); `Manual Required`
//!   rows explain that the player must supply the file.
//!
//! The list scrolls cleanly past 100+ entries and stays inside the
//! viewport at every supported size, in English and Japanese.

use std::collections::BTreeSet;

use crate::draw::{first_cells, palette, text_cells, text_width, UiColor, UiDrawList, UiRect};
use crate::state::{tr, GamepadButton, UiInputEvent, UiKey, UiLang};

// ---------------------------------------------------------------------------
// Diff model (host-supplied)
// ---------------------------------------------------------------------------

/// How one mod differs between the server's required set and the client.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModSyncDiffKind {
    /// The server requires it; the client does not have it — it will be
    /// downloaded into the isolated profile.
    Missing,
    /// Both sides have it but the versions differ — the server's version
    /// replaces the client's inside the profile.
    VersionMismatch,
    /// The server offers it but does not require it — installed only
    /// when the player ticks its checkbox.
    Optional,
    /// A client mod the server's set cannot run alongside — it is left
    /// out of the profile for this server.
    Incompatible,
    /// Identical on both sides — no action needed.
    Matched,
}

impl ModSyncDiffKind {
    /// Short row badge, bilingual and jargon-free.
    pub fn badge(self, lang: UiLang) -> &'static str {
        match self {
            Self::Missing => tr(lang, "NEW", "追加"),
            Self::VersionMismatch => tr(lang, "UPDATE", "更新"),
            Self::Optional => tr(lang, "OPTION", "オプション"),
            Self::Incompatible => tr(lang, "BLOCKED", "利用不可"),
            Self::Matched => tr(lang, "OK", "一致"),
        }
    }

    /// Badge tint.
    pub const fn color(self) -> UiColor {
        match self {
            Self::Missing => palette::WARNING,
            Self::VersionMismatch => palette::ACCENT,
            Self::Optional => palette::AIR,
            Self::Incompatible => palette::DANGER,
            Self::Matched => palette::BUTTON_DISABLED,
        }
    }
}

/// Where the file for a diff entry will come from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModSyncSource {
    /// Fetched automatically from Modrinth by the client.
    ModrinthDirect,
    /// CurseForge's terms require the player to grab the file from the
    /// project page — the client opens it for them.
    CurseForgeGuided,
    /// No automatic source exists; the player must place the file into
    /// the isolated profile directory themselves.
    ManualRequired,
}

impl ModSyncSource {
    /// Compact badge rendered on the row's second line.
    pub fn badge(self) -> &'static str {
        match self {
            Self::ModrinthDirect => "Modrinth",
            Self::CurseForgeGuided => "CurseForge",
            Self::ManualRequired => "Manual",
        }
    }

    /// One-line explanation shown in the expanded details.
    pub fn detail(self, lang: UiLang) -> &'static str {
        match self {
            Self::ModrinthDirect => tr(
                lang,
                "Modrinth Direct - downloaded for you",
                "Modrinthから自動でダウンロード",
            ),
            Self::CurseForgeGuided => tr(
                lang,
                "CurseForge Guided - open its page to download",
                "CurseForgeのページを開いて入手",
            ),
            Self::ManualRequired => tr(
                lang,
                "Manual Required - place the file in the profile folder",
                "手動 - プロファイルフォルダーに入れてください",
            ),
        }
    }
}

/// One row of the server-vs-client mod diff.
#[derive(Clone, Debug, PartialEq)]
pub struct ModSyncDiffEntry {
    /// Stable mod id (`"create"`), used as the opt-in key and echoed in
    /// [`ModSyncUiAction`].
    pub mod_id: String,
    /// Human-readable name shown on the row.
    pub name: String,
    /// Version the server requires.
    pub server_version: String,
    /// Version the client has installed (`None` = not installed).
    pub client_version: Option<String>,
    pub kind: ModSyncDiffKind,
    pub source: ModSyncSource,
    /// SPDX license tag shown next to the source badge.
    pub license_spdx: String,
    /// Server-declared SHA-512 digest (hex), displayed truncated.
    pub sha512_hex: Option<String>,
    /// `true` once the staged file's digest matched `sha512_hex`.
    pub sha512_verified: bool,
    /// Download size in bytes, for the expanded details (`0` = unknown).
    pub download_size: u64,
    /// One-line plain-language description (English).
    pub summary_en: String,
    /// One-line plain-language description (Japanese; empty = reuse EN).
    pub summary_ja: String,
    /// CurseForge project page for guided downloads.
    pub curseforge_url: Option<String>,
}

impl ModSyncDiffEntry {
    pub fn new(
        mod_id: impl Into<String>,
        name: impl Into<String>,
        server_version: impl Into<String>,
        kind: ModSyncDiffKind,
    ) -> Self {
        Self {
            mod_id: mod_id.into(),
            name: name.into(),
            server_version: server_version.into(),
            client_version: None,
            kind,
            source: ModSyncSource::ModrinthDirect,
            license_spdx: String::new(),
            sha512_hex: None,
            sha512_verified: false,
            download_size: 0,
            summary_en: String::new(),
            summary_ja: String::new(),
            curseforge_url: None,
        }
    }

    /// Localized summary line (English fallback for missing Japanese).
    pub fn summary(&self, lang: UiLang) -> &str {
        if lang == UiLang::Ja && !self.summary_ja.is_empty() {
            &self.summary_ja
        } else {
            &self.summary_en
        }
    }

    /// SHA-512 digest display: first 12 hex chars, or `None`.
    pub fn short_sha512(&self) -> Option<&str> {
        self.sha512_hex
            .as_deref()
            .map(|h| &h[..h.len().min(12)])
    }
}

// ---------------------------------------------------------------------------
// Actions, widgets, hit regions
// ---------------------------------------------------------------------------

/// What the embedding application must perform after an interaction.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ModSyncUiAction {
    /// The player explicitly approved the sync plan. `sync_ids` is every
    /// mod id the isolated profile will fetch or expect (`Missing` +
    /// `VersionMismatch` + `ManualRequired` + opted-in `Optional`);
    /// `optional_ids` is the opted-in subset. `Incompatible` entries are
    /// never part of the plan — the profile simply excludes them.
    ApproveAndSync {
        sync_ids: Vec<String>,
        optional_ids: Vec<String>,
    },
    /// The player asked to open a CurseForge project page for a guided
    /// download (`mod_id` identifies the row, `url` is the entry's
    /// declared `curseforge_url` — never a synthesized address).
    OpenCurseForgePage { mod_id: String, url: String },
    /// The player declined the diff — the host disconnects without
    /// writing anything.
    DeclineAndDisconnect,
}

/// Interactive element of the screen, reported by hit regions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModSyncWidgetId {
    /// A row's header band; payload is the entry index. Selects the row;
    /// selecting it again collapses/expands its details.
    Row(usize),
    /// An `Optional` row's opt-in checkbox; payload is the entry index.
    OptionalCheck(usize),
    /// The expanded details' "Open CurseForge Page" button; payload is
    /// the entry index.
    RowCurseForge(usize),
    /// List scroll buttons.
    ListScrollUp,
    ListScrollDown,
    /// Footer "Approve & Sync" (or "Join Server" when nothing changes).
    ApproveSync,
    /// Footer "CurseForge" shortcut — opens the first guided row's page.
    CurseForgePage,
    /// Footer "Decline & Disconnect".
    Decline,
}

/// A clickable rectangle emitted alongside the draw list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ModSyncHitRegion {
    pub rect: UiRect,
    pub id: ModSyncWidgetId,
}

impl ModSyncHitRegion {
    /// Point containment test (right/bottom exclusive).
    pub const fn contains(self, x: i32, y: i32) -> bool {
        self.rect.contains(x, y)
    }
}

// ---------------------------------------------------------------------------
// Screen state
// ---------------------------------------------------------------------------

/// Interactive state of the mod-sync consent screen.
#[derive(Clone, Debug)]
pub struct ModSyncUiState {
    /// Display name of the server asking to sync.
    pub server_name: String,
    /// Stable id used for the isolated profile directory.
    pub server_id: String,
    /// Full diff rows, in host-supplied order.
    pub entries: Vec<ModSyncDiffEntry>,
    /// Display language for the screen's own labels.
    pub lang: UiLang,
    /// Forced GUI scale (`0` = auto from height, `1..=4` fixed).
    pub ui_scale: u8,
    /// Opted-in `Optional` mod ids.
    optional_optins: BTreeSet<String>,
    /// Currently expanded row (entry index), if any.
    expanded: Option<usize>,
    /// Selected row (entry index).
    selected: usize,
    /// List scroll offset in logical pixels (pre-clamp).
    scroll_px: i32,
    /// Last hovered widget.
    hover: Option<ModSyncWidgetId>,
}

impl Default for ModSyncUiState {
    fn default() -> Self {
        Self {
            server_name: String::new(),
            server_id: "default".to_owned(),
            entries: Vec::new(),
            lang: UiLang::En,
            ui_scale: 0,
            optional_optins: BTreeSet::new(),
            expanded: None,
            selected: 0,
            scroll_px: 0,
            hover: None,
        }
    }
}

/// The consent plan the host installs on approval.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ModSyncPlan {
    /// Every mod id the profile will fetch or expect: `Missing`,
    /// `VersionMismatch`, `ManualRequired`, and opted-in `Optional`.
    pub sync_ids: Vec<String>,
    /// Opted-in `Optional` ids (a subset of `sync_ids`).
    pub optional_ids: Vec<String>,
    /// `ManualRequired` ids the player must still supply.
    pub manual_ids: Vec<String>,
    /// `Incompatible` ids the profile excludes.
    pub blocked_ids: Vec<String>,
}

impl ModSyncUiState {
    pub fn new(
        server_name: impl Into<String>,
        server_id: impl Into<String>,
        entries: Vec<ModSyncDiffEntry>,
    ) -> Self {
        Self {
            server_name: server_name.into(),
            server_id: server_id.into(),
            entries,
            ..Self::default()
        }
    }

    /// Effective GUI scale for `height` px tall output (`1..=4`), mirroring
    /// [`crate::state::UiState::effective_scale`].
    pub fn effective_scale(&self, height: u32) -> u32 {
        if self.ui_scale == 0 {
            (height / 270).clamp(1, 4)
        } else {
            u32::from(self.ui_scale.clamp(1, 4))
        }
    }

    /// `profiles/<server-id>/mods` — the isolated install root, with the
    /// id scrubbed of characters that could escape the profile tree.
    pub fn profile_dir(&self) -> String {
        format!("profiles/{}/mods", sanitize_profile_id(&self.server_id))
    }

    /// How many entries carry `kind`.
    pub fn count(&self, kind: ModSyncDiffKind) -> usize {
        self.entries.iter().filter(|e| e.kind == kind).count()
    }

    /// `true` while the optional mod `mod_id` is ticked.
    pub fn optional_opted(&self, mod_id: &str) -> bool {
        self.optional_optins.contains(mod_id)
    }

    /// Toggle an `Optional` row's checkbox. Non-optional entries and
    /// unknown ids are ignored (fail-closed: only genuine optional rows
    /// can enter the consent set).
    pub fn toggle_optional(&mut self, mod_id: &str) {
        let is_optional = self
            .entries
            .iter()
            .any(|e| e.mod_id == mod_id && e.kind == ModSyncDiffKind::Optional);
        if !is_optional {
            return;
        }
        if !self.optional_optins.insert(mod_id.to_owned()) {
            self.optional_optins.remove(mod_id);
        }
    }

    /// The plan an approval would install right now.
    pub fn plan(&self) -> ModSyncPlan {
        let mut plan = ModSyncPlan::default();
        for e in &self.entries {
            match e.kind {
                ModSyncDiffKind::Optional => {
                    if self.optional_opted(&e.mod_id) {
                        plan.optional_ids.push(e.mod_id.clone());
                        plan.sync_ids.push(e.mod_id.clone());
                    }
                }
                ModSyncDiffKind::Incompatible => plan.blocked_ids.push(e.mod_id.clone()),
                ModSyncDiffKind::Matched => {}
                kind => {
                    if e.source == ModSyncSource::ManualRequired {
                        plan.manual_ids.push(e.mod_id.clone());
                    }
                    let _ = kind;
                    plan.sync_ids.push(e.mod_id.clone());
                }
            }
        }
        plan
    }

    /// Whether the footer shows the CurseForge shortcut.
    pub fn has_guided_entries(&self) -> bool {
        self.entries
            .iter()
            .any(|e| e.source == ModSyncSource::CurseForgeGuided && e.curseforge_url.is_some())
    }

    /// First guided entry carrying a page URL.
    pub fn first_guided(&self) -> Option<&ModSyncDiffEntry> {
        self.entries
            .iter()
            .find(|e| e.source == ModSyncSource::CurseForgeGuided && e.curseforge_url.is_some())
    }

    /// Selected row as an index into `entries`.
    pub fn selected_row(&self) -> usize {
        self.selected.min(self.entries.len().saturating_sub(1))
    }

    /// Expanded row index, if any.
    pub fn expanded_row(&self) -> Option<usize> {
        self.expanded.filter(|&i| i < self.entries.len())
    }

    /// Current list scroll offset in logical pixels (pre-clamp).
    pub fn scroll_offset(&self) -> i32 {
        self.scroll_px
    }

    /// Height of row `i` in logical pixels including its gap.
    fn row_pitch(&self, i: usize) -> i32 {
        ROW_PITCH
            + if self.expanded_row() == Some(i) {
                DETAIL_PITCH
            } else {
                0
            }
    }

    /// Logical-pixel offset of row `i`'s top inside the list content.
    fn row_top(&self, i: usize) -> i32 {
        (0..i).map(|j| self.row_pitch(j)).sum()
    }

    /// Total scrollable content height in logical pixels.
    fn content_height(&self) -> i32 {
        (0..self.entries.len()).map(|i| self.row_pitch(i)).sum()
    }

    /// Visible list height in logical pixels at `height` physical px.
    fn view_height(&self, height: u32) -> i32 {
        let h = (height.min(i32::MAX as u32) as i32) / self.effective_scale(height) as i32;
        (h - 4 - LIST_BOTTOM_OFF - LIST_TOP - 4).max(0)
    }

    // -- interaction ---------------------------------------------------

    /// Activate a widget (from a click, or directly in tests). Returns
    /// the outward actions the host must perform.
    pub fn activate(&mut self, id: ModSyncWidgetId) -> Vec<ModSyncUiAction> {
        match id {
            ModSyncWidgetId::Row(i) => {
                if i < self.entries.len() {
                    if self.selected == i {
                        self.expanded = (self.expanded != Some(i)).then_some(i);
                    } else {
                        self.selected = i;
                        self.expanded = Some(i);
                    }
                }
            }
            ModSyncWidgetId::OptionalCheck(i) => {
                if let Some(e) = self.entries.get(i) {
                    self.toggle_optional(&e.mod_id.clone());
                }
            }
            ModSyncWidgetId::RowCurseForge(i) => {
                if let Some(e) = self.entries.get(i) {
                    if let Some(url) = &e.curseforge_url {
                        return vec![ModSyncUiAction::OpenCurseForgePage {
                            mod_id: e.mod_id.clone(),
                            url: url.clone(),
                        }];
                    }
                }
            }
            ModSyncWidgetId::ListScrollUp => {
                self.scroll_px = (self.scroll_px - ROW_PITCH * 3).max(0);
            }
            ModSyncWidgetId::ListScrollDown => {
                self.scroll_px += ROW_PITCH * 3;
            }
            ModSyncWidgetId::ApproveSync => {
                let plan = self.plan();
                return vec![ModSyncUiAction::ApproveAndSync {
                    sync_ids: plan.sync_ids,
                    optional_ids: plan.optional_ids,
                }];
            }
            ModSyncWidgetId::CurseForgePage => {
                if let Some(e) = self.first_guided() {
                    return vec![ModSyncUiAction::OpenCurseForgePage {
                        mod_id: e.mod_id.clone(),
                        url: e.curseforge_url.clone().unwrap_or_default(),
                    }];
                }
            }
            ModSyncWidgetId::Decline => return vec![ModSyncUiAction::DeclineAndDisconnect],
        }
        Vec::new()
    }

    /// The widget under `(x, y)` in a `width`×`height` frame, if any
    /// (physical pixels; regions are recomputed from the current state).
    pub fn widget_at(&self, width: u32, height: u32, x: i32, y: i32) -> Option<ModSyncWidgetId> {
        self.render(width, height)
            .1
            .iter()
            .rev()
            .find(|h| h.rect.contains(x, y))
            .map(|h| h.id)
    }

    /// Route one device-agnostic input event; returns the actions the
    /// host must perform. `width`/`height` are the frame's physical
    /// pixels, used for pointer hit-testing and page-step size.
    ///
    /// Keyboard/gamepad semantics (consent-safe):
    ///
    /// * `Up`/`Down` move through the rows (wrapping), `Left`/`Right`
    ///   page the list;
    /// * `Enter`/`Space` (or gamepad South) expands/collapses the
    ///   selected row's details;
    /// * `Escape` (or gamepad East/Start) is always *Decline & Disconnect*
    ///   — approval is never reachable by accident;
    /// * `Approve & Sync` itself is an explicit click on the footer
    ///   button (or `activate(ApproveSync)` for the host/tests).
    pub fn handle_input(
        &mut self,
        ev: UiInputEvent,
        width: u32,
        height: u32,
    ) -> Vec<ModSyncUiAction> {
        match ev {
            UiInputEvent::MouseMove { x, y } => {
                self.hover = self.widget_at(width, height, x, y);
                Vec::new()
            }
            UiInputEvent::Click { x, y } => match self.widget_at(width, height, x, y) {
                Some(id) => self.activate(id),
                None => Vec::new(),
            },
            UiInputEvent::KeyPressed(key) => self.handle_key(key, height),
            UiInputEvent::CharTyped(_) => Vec::new(),
            UiInputEvent::GamepadPressed(btn) => match btn {
                GamepadButton::DpadUp => self.handle_key(UiKey::Up, height),
                GamepadButton::DpadDown => self.handle_key(UiKey::Down, height),
                GamepadButton::DpadLeft => self.handle_key(UiKey::Left, height),
                GamepadButton::DpadRight => self.handle_key(UiKey::Right, height),
                GamepadButton::South => self.handle_key(UiKey::Enter, height),
                GamepadButton::East | GamepadButton::Start => {
                    vec![ModSyncUiAction::DeclineAndDisconnect]
                }
            },
        }
    }

    fn handle_key(&mut self, key: UiKey, height: u32) -> Vec<ModSyncUiAction> {
        match key {
            UiKey::Up => self.move_selection(-1, true),
            UiKey::Down => self.move_selection(1, true),
            UiKey::Left | UiKey::Right => {
                let page = self.view_height(height) / ROW_PITCH;
                let step = if key == UiKey::Left { -page } else { page };
                self.move_selection(step, false);
            }
            UiKey::Enter | UiKey::Space | UiKey::Tab => {
                if !self.entries.is_empty() {
                    let i = self.selected_row();
                    self.expanded = (self.expanded != Some(i)).then_some(i);
                }
            }
            UiKey::Backspace => {}
            UiKey::Escape => return vec![ModSyncUiAction::DeclineAndDisconnect],
        }
        Vec::new()
    }

    fn move_selection(&mut self, dir: i32, wrap: bool) {
        let len = self.entries.len() as i32;
        if len == 0 {
            return;
        }
        let next = self.selected as i32 + dir;
        self.selected = if wrap {
            next.rem_euclid(len) as usize
        } else {
            next.clamp(0, len - 1) as usize
        };
    }

    // -- rendering -----------------------------------------------------

    /// Build the frame: `(draw_list, hit_regions)`. Everything is
    /// computed in logical pixels and multiplied by
    /// [`effective_scale`](Self::effective_scale) on emit, so the same
    /// layout serves `640×360` through `3840×2160` and every emitted hit
    /// region is pixel-identical to its widget's visuals.
    pub fn render(&self, width: u32, height: u32) -> (UiDrawList, Vec<ModSyncHitRegion>) {
        let mut lay = Lay::new(self, width, height);
        let mut hits = Vec::new();
        if lay.w < 200 || lay.h < 140 {
            lay.fill(0, 0, lay.w, lay.h, palette::BACKGROUND_TOP);
            return (lay.finish(), hits);
        }
        lay.fill(0, 0, lay.w, lay.h, palette::BACKGROUND_TOP);
        lay.fill(
            0,
            lay.h / 2,
            lay.w,
            lay.h - lay.h / 2,
            palette::BACKGROUND_BOTTOM,
        );
        lay.fill(
            MARGIN,
            MARGIN,
            lay.w - 2 * MARGIN,
            lay.h - 2 * MARGIN,
            palette::PANEL,
        );
        lay.border(
            MARGIN,
            MARGIN,
            lay.w - 2 * MARGIN,
            lay.h - 2 * MARGIN,
            palette::PANEL_BORDER,
        );

        self.render_header(&mut lay);
        self.render_list(&mut lay, &mut hits, height);
        self.render_footer(&mut lay, &mut hits);
        (lay.finish(), hits)
    }

    fn render_header(&self, lay: &mut Lay) {
        let lang = self.lang;
        let inner_w = lay.w - 2 * MARGIN - 8;
        let title_cells = ((inner_w - 8) / 16).max(1) as usize;
        lay.text_fit(
            MARGIN + 4,
            MARGIN + 4,
            2,
            palette::TEXT,
            tr(lang, "Server Mod Sync", "サーバーMod同期"),
            title_cells,
        );

        // Line 1: which server is asking.
        let name = if self.server_name.is_empty() {
            tr(lang, "this server", "このサーバー").to_owned()
        } else {
            self.server_name.clone()
        };
        let ask = tr(
            lang,
            "wants to change your mods:",
            "がModの変更を求めています:",
        );
        lay.text_fit(
            MARGIN + 4,
            28,
            1,
            palette::TEXT_DIM,
            &format!("{} {}", fit_text(&name, 28), ask),
            ((inner_w - 8) / 8).max(1) as usize,
        );

        // Line 2: the diff counts.
        lay.text_fit(
            MARGIN + 4,
            38,
            1,
            palette::TEXT,
            &self.counts_label(lang),
            ((inner_w - 8) / 8).max(1) as usize,
        );

        // Lines 3-4: profile isolation — the consent headline.
        lay.text_fit(
            MARGIN + 4,
            50,
            1,
            palette::ACCENT,
            &format!(
                "{}: {}",
                tr(lang, "Installs to", "インストール先"),
                self.profile_dir()
            ),
            ((inner_w - 8) / 8).max(1) as usize,
        );
        lay.text_fit(
            MARGIN + 4,
            60,
            1,
            palette::TEXT_DIM,
            tr(
                lang,
                "Your main mods folder is never changed",
                "メインのmodsフォルダーは変更されません",
            ),
            ((inner_w - 8) / 8).max(1) as usize,
        );
    }

    fn counts_label(&self, lang: UiLang) -> String {
        let mut parts: Vec<String> = Vec::new();
        let mut push = |n: usize, en: &'static str, ja: &'static str| {
            if n > 0 {
                parts.push(format!("{} {}", n, tr(lang, en, ja)));
            }
        };
        push(self.count(ModSyncDiffKind::Missing), "new", "追加");
        push(
            self.count(ModSyncDiffKind::VersionMismatch),
            "to update",
            "更新",
        );
        push(self.count(ModSyncDiffKind::Optional), "optional", "オプション");
        push(
            self.count(ModSyncDiffKind::Incompatible),
            "blocked",
            "利用不可",
        );
        push(self.count(ModSyncDiffKind::Matched), "in sync", "一致");
        if parts.is_empty() {
            return tr(lang, "No mods to compare", "同期するModはありません").to_owned();
        }
        parts.join(" | ")
    }

    /// Effective scroll: clamped to content, then nudged so the selected
    /// row (and its expanded details) is fully inside the viewport.
    fn effective_scroll(&self, height: u32) -> i32 {
        let view = self.view_height(height);
        let max_scroll = (self.content_height() - view).max(0);
        let mut scroll = self.scroll_px.clamp(0, max_scroll);
        if !self.entries.is_empty() {
            let i = self.selected_row();
            let top = self.row_top(i);
            let bottom = top + self.row_pitch(i) - 2;
            if top < scroll {
                scroll = top;
            } else if bottom > scroll + view {
                scroll = bottom - view;
            }
            scroll = scroll.clamp(0, max_scroll);
        }
        scroll
    }

    fn render_list(&self, lay: &mut Lay, hits: &mut Vec<ModSyncHitRegion>, height: u32) {
        let lang = self.lang;
        let top = LIST_TOP;
        let bottom = lay.h - 4 - LIST_BOTTOM_OFF;
        let list_x = MARGIN + 4;
        let scrollable = self.content_height() > self.view_height(height);
        let scrollbar_w = if scrollable { 14 } else { 0 };
        let rows_w = lay.w - 2 * MARGIN - 8 - 4 - scrollbar_w;
        let list_rect = UiRect::new(list_x, top, (rows_w + scrollbar_w + 4).max(1) as u32, (bottom - top).max(1) as u32);
        lay.fill_rect(list_rect, DETAIL_BG);
        lay.border_rect(list_rect, palette::PANEL_BORDER);

        let view_h = (bottom - top - 4).max(0);
        let scroll = self.effective_scroll(height);
        let clip = UiRect::new(list_x + 1, top + 1, (list_rect.w as i32 - 2).max(0) as u32, (list_rect.h as i32 - 2).max(0) as u32);

        if self.entries.is_empty() {
            lay.text_fit(
                list_x + 6,
                top + 8,
                1,
                palette::TEXT_DIM,
                tr(lang, "Nothing to sync", "同期するModはありません"),
                ((rows_w - 12) / 8).max(1) as usize,
            );
        }

        let mut y = top + 2 - scroll;
        for (i, entry) in self.entries.iter().enumerate() {
            let pitch = self.row_pitch(i);
            let row_bottom = y + pitch - 2;
            if row_bottom >= top + 2 && y <= bottom - 2 {
                self.render_row(lay, hits, clip, i, entry, list_x + 2, y, rows_w);
            }
            y += pitch;
            if y > bottom {
                break;
            }
        }

        if scrollable {
            let sb_x = list_rect.right() - 13;
            let up = UiRect::new(sb_x, top + 2, 11, 12);
            let down = UiRect::new(sb_x, bottom - 14, 11, 12);
            lay.fill_rect(up, palette::BUTTON);
            lay.fill_rect(down, palette::BUTTON);
            lay.ctext_in(up, 1, palette::TEXT, "^");
            lay.ctext_in(down, 1, palette::TEXT, "v");
            let track = UiRect::new(sb_x, top + 16, 11, (bottom - top - 32).max(0) as u32);
            lay.fill_rect(track, palette::SLOT);
            let content = self.content_height().max(1);
            let thumb_h = ((view_h as i64 * track.h as i64) / content as i64)
                .clamp(8, track.h.max(1) as i64) as u32;
            let max_scroll = (content - view_h).max(1) as i64;
            let travel = (track.h - thumb_h.min(track.h)).max(1) as i64;
            let thumb_y = track.y + ((scroll as i64 * travel) / max_scroll) as i32;
            lay.fill_rect(UiRect::new(sb_x, thumb_y, 11, thumb_h.min(track.h)), palette::TEXT_DIM);
            lay.region(hits, ModSyncWidgetId::ListScrollUp, up);
            lay.region(hits, ModSyncWidgetId::ListScrollDown, down);
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn render_row(
        &self,
        lay: &mut Lay,
        hits: &mut Vec<ModSyncHitRegion>,
        clip: UiRect,
        i: usize,
        entry: &ModSyncDiffEntry,
        x: i32,
        y: i32,
        w: i32,
    ) {
        let lang = self.lang;
        let row = UiRect::new(x, y, w.max(1) as u32, ROW_H as u32);
        let is_sel = i == self.selected_row();
        let is_hover = self.hover == Some(ModSyncWidgetId::Row(i));
        let base = if is_sel {
            palette::BUTTON_HOVER
        } else if is_hover {
            palette::BUTTON
        } else {
            palette::SLOT
        };
        lay.fill_rect_clipped(row, clip, base);
        if entry.kind != ModSyncDiffKind::Matched {
            lay.fill_rect_clipped(UiRect::new(x, y, 2, ROW_H as u32), clip, entry.kind.color());
        }

        // Opt-in checkbox for Optional rows (kept outside the Row hit
        // region so hit regions never overlap).
        let mut text_x = x + 6;
        if entry.kind == ModSyncDiffKind::Optional {
            let opted = self.optional_opted(&entry.mod_id);
            let cb = UiRect::new(x + 4, y + 5, 10, 10);
            lay.fill_rect_clipped(cb, clip, palette::SLOT);
            lay.border_rect_clipped(
                cb,
                clip,
                if opted { palette::ACCENT } else { palette::PANEL_BORDER },
            );
            if opted {
                lay.text_clipped(x + 5, y + 6, 1, palette::ACCENT, "x", clip);
            }
            lay.region_clipped(hits, ModSyncWidgetId::OptionalCheck(i), cb, clip);
            text_x = x + 18;
        }

        // Kind pill, right-aligned on the name line.
        let badge = entry.kind.badge(lang);
        let pill_w = text_cells(badge) as i32 * 8 + 8;
        let pill = UiRect::new(x + w - pill_w - 3, y + 3, pill_w.max(8) as u32, 10);
        let pill_fg = if entry.kind == ModSyncDiffKind::Matched {
            palette::TEXT_DIM
        } else {
            palette::TEXT_DARK
        };
        lay.fill_rect_clipped(pill, clip, entry.kind.color());
        lay.ctext_in_clipped(pill, clip, 1, pill_fg, badge);

        // Line 1: name, bounded on the right by the kind pill.
        let name_cells = ((x + w - pill_w - 8 - text_x) / 8).max(1) as usize;
        lay.text_fit_clipped(
            text_x,
            y + 3,
            1,
            if entry.kind == ModSyncDiffKind::Incompatible {
                palette::TEXT_DIM
            } else {
                palette::TEXT
            },
            &entry.name,
            name_cells,
            clip,
        );

        // Line 2: versions + source + license + SHA-512 mark.
        let installed = entry
            .client_version
            .as_deref()
            .unwrap_or_else(|| tr(lang, "none", "なし"));
        let mut parts = format!("{} -> {}", installed, entry.server_version);
        parts.push_str(&format!("  {}", entry.source.badge()));
        if !entry.license_spdx.is_empty() {
            parts.push_str(&format!("  {}", entry.license_spdx));
        }
        let sha = match (&entry.sha512_hex, entry.sha512_verified) {
            (Some(_), true) => "  SHA-512 OK",
            (Some(_), false) => "  SHA-512 ?",
            (None, _) => "",
        };
        parts.push_str(sha);
        lay.text_fit_clipped(
            x + 14,
            y + 13,
            1,
            palette::TEXT_DIM,
            &parts,
            ((w - 20) / 8).max(1) as usize,
            clip,
        );

        // Row hit region covers the header band only (never the checkbox
        // or the expanded detail area), so regions stay overlap-free.
        let header = UiRect::new(text_x, y, (x + w - text_x).max(1) as u32, ROW_H as u32);
        lay.region_clipped(hits, ModSyncWidgetId::Row(i), header, clip);

        // Expanded accordion details.
        if self.expanded_row() == Some(i) {
            self.render_row_detail(lay, hits, clip, i, entry, x, y + ROW_PITCH, w);
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn render_row_detail(
        &self,
        lay: &mut Lay,
        hits: &mut Vec<ModSyncHitRegion>,
        clip: UiRect,
        i: usize,
        entry: &ModSyncDiffEntry,
        x: i32,
        y: i32,
        w: i32,
    ) {
        let lang = self.lang;
        let text_cells_w = ((w - 20) / 8).max(1) as usize;
        let detail_x = x + 14;

        // Version transition.
        let installed = entry
            .client_version
            .as_deref()
            .unwrap_or_else(|| tr(lang, "not installed", "なし"));
        lay.text_fit_clipped(
            detail_x,
            y + 2,
            1,
            palette::TEXT,
            &format!(
                "{}: {} -> {}",
                tr(lang, "Version", "バージョン"),
                installed,
                entry.server_version
            ),
            text_cells_w,
            clip,
        );

        // Source + license.
        let mut from = format!(
            "{}: {}",
            tr(lang, "From", "入手元"),
            entry.source.detail(lang)
        );
        if !entry.license_spdx.is_empty() {
            from.push_str(&format!("   {}: {}", tr(lang, "License", "ライセンス"), entry.license_spdx));
        }
        lay.text_fit_clipped(detail_x, y + 12, 1, palette::TEXT_DIM, &from, text_cells_w, clip);

        // SHA-512 verification.
        let sha_label = match (entry.short_sha512(), entry.sha512_verified) {
            (Some(short), true) => format!("SHA-512: {}... {}", short, tr(lang, "verified", "確認済み")),
            (Some(short), false) => {
                format!("SHA-512: {}... {}", short, tr(lang, "not verified yet", "確認前"))
            }
            (None, _) => format!("SHA-512: {}", tr(lang, "no fingerprint", "ハッシュなし")),
        };
        let sha_color = if entry.sha512_verified {
            palette::ACCENT
        } else {
            palette::TEXT_DIM
        };
        lay.text_fit_clipped(detail_x, y + 22, 1, sha_color, &sha_label, text_cells_w, clip);

        // Summary + optional per-row actions.
        let summary = entry.summary(lang);
        let mut sy = y + 32;
        if !summary.is_empty() {
            for line in wrap_text(summary, text_cells_w).iter().take(2) {
                lay.text_fit_clipped(detail_x, sy, 1, palette::TEXT_DIM, line, text_cells_w, clip);
                sy += 10;
            }
        }
        match entry.source {
            ModSyncSource::CurseForgeGuided => {
                if entry.curseforge_url.is_some() {
                    let label = tr(lang, "Open CurseForge Page", "CurseForgeページを開く");
                    let bw = text_cells(label) as i32 * 8 + 12;
                    let btn = UiRect::new(x + w - bw - 4, y + 30, bw.max(20) as u32, 14);
                    let hovered = self.hover == Some(ModSyncWidgetId::RowCurseForge(i));
                    lay.fill_rect_clipped(
                        btn,
                        clip,
                        if hovered { palette::BUTTON_HOVER } else { palette::BUTTON },
                    );
                    lay.border_rect_clipped(btn, clip, palette::BUTTON_BORDER);
                    lay.ctext_in_clipped(btn, clip, 1, palette::TEXT, label);
                    lay.region_clipped(hits, ModSyncWidgetId::RowCurseForge(i), btn, clip);
                }
            }
            ModSyncSource::ManualRequired => {
                lay.text_fit_clipped(
                    detail_x,
                    sy,
                    1,
                    palette::WARNING,
                    tr(
                        lang,
                        "You must supply this file yourself",
                        "このファイルは自分で追加してください",
                    ),
                    text_cells_w,
                    clip,
                );
            }
            ModSyncSource::ModrinthDirect => {}
        }
        if entry.kind == ModSyncDiffKind::Incompatible {
            lay.text_fit_clipped(
                detail_x,
                sy + 10,
                1,
                palette::DANGER,
                tr(
                    lang,
                    "This mod cannot be used on this server",
                    "このModはこのサーバーでは利用できません",
                ),
                text_cells_w,
                clip,
            );
        }
    }

    fn render_footer(&self, lay: &mut Lay, hits: &mut Vec<ModSyncHitRegion>) {
        let lang = self.lang;
        // Consent reminder just above the buttons.
        lay.ctext_in(
            UiRect::new(MARGIN + 4, lay.h - 4 - LIST_BOTTOM_OFF + 4, (lay.w - 2 * MARGIN - 8).max(1) as u32, 12),
            1,
            palette::TEXT_DIM,
            tr(
                lang,
                "Nothing downloads until you approve",
                "承認するまでダウンロードされません",
            ),
        );

        let plan = self.plan();
        let approve_label = if plan.sync_ids.is_empty() {
            tr(lang, "Join Server", "サーバーに参加")
        } else {
            tr(lang, "Approve & Sync", "承認して同期")
        };
        let decline_label = tr(lang, "Decline & Disconnect", "拒否して切断");
        let cf_label = tr(lang, "CurseForge", "CurseForge");

        let by = lay.h - 4 - 24;
        let bh = 18;
        let mut bx = lay.w - MARGIN - 4;
        // Right-to-left: Decline, CurseForge (conditional), Approve.
        let dw = text_cells(decline_label) as i32 * 8 + 16;
        bx -= dw;
        let decline = UiRect::new(bx, by, dw.max(20) as u32, bh);
        let hovered = self.hover == Some(ModSyncWidgetId::Decline);
        lay.fill_rect(decline, if hovered { palette::DANGER } else { palette::BUTTON });
        lay.border_rect(decline, palette::BUTTON_BORDER);
        lay.ctext_in(decline, 1, palette::TEXT, decline_label);
        lay.region(hits, ModSyncWidgetId::Decline, decline);

        if self.has_guided_entries() {
            let cw = text_cells(cf_label) as i32 * 8 + 16;
            bx -= cw + 6;
            let cf = UiRect::new(bx, by, cw.max(20) as u32, bh);
            let hovered = self.hover == Some(ModSyncWidgetId::CurseForgePage);
            lay.fill_rect(cf, if hovered { palette::BUTTON_HOVER } else { palette::BUTTON });
            lay.border_rect(cf, palette::BUTTON_BORDER);
            lay.ctext_in(cf, 1, palette::TEXT, cf_label);
            lay.region(hits, ModSyncWidgetId::CurseForgePage, cf);
        }

        let aw = text_cells(approve_label) as i32 * 8 + 16;
        bx -= aw + 6;
        let approve = UiRect::new(bx.max(MARGIN + 4), by, aw.max(20) as u32, bh);
        let hovered = self.hover == Some(ModSyncWidgetId::ApproveSync);
        lay.fill_rect(approve, if hovered { palette::ACCENT } else { palette::BUTTON });
        lay.border_rect(approve, palette::BUTTON_BORDER);
        lay.ctext_in(approve, 1, palette::TEXT, approve_label);
        lay.region(hits, ModSyncWidgetId::ApproveSync, approve);
    }
}

/// `profiles/<id>/` must never escape: keep `[A-Za-z0-9._-]`, map the
/// rest to `_`, and refuse dot-only ids like `.` / `..`.
fn sanitize_profile_id(id: &str) -> String {
    let scrubbed: String = id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') {
                c
            } else {
                '_'
            }
        })
        .collect();
    let trimmed = scrubbed.trim_matches('.');
    if trimmed.is_empty() {
        "default".to_owned()
    } else {
        trimmed.to_owned()
    }
}

// ---------------------------------------------------------------------------
// Layout constants + scaled-emit helper
// ---------------------------------------------------------------------------

const MARGIN: i32 = 4;
const LIST_TOP: i32 = 70;
const LIST_BOTTOM_OFF: i32 = 44;
/// Row header height in logical pixels (two text lines + padding).
const ROW_H: i32 = 22;
/// Per-row advance (row + gap).
const ROW_PITCH: i32 = 24;
/// Extra pitch an expanded row's detail block adds.
const DETAIL_PITCH: i32 = 76;

/// List fill: a hair lighter than the main panel.
const DETAIL_BG: UiColor = UiColor::rgba(26, 29, 38, 235);

/// Scales logical-pixel geometry to physical pixels on emit so visuals
/// and hit regions never diverge. `clip` variants additionally confine
/// output to the list's visible band so scrolled rows never paint or
/// hit-test outside it.
struct Lay {
    list: UiDrawList,
    w: i32,
    h: i32,
    s: i32,
}

impl Lay {
    fn new(state: &ModSyncUiState, width: u32, height: u32) -> Self {
        let s = state.effective_scale(height) as i32;
        Self {
            list: UiDrawList::new(),
            w: (width.min(i32::MAX as u32) as i32) / s,
            h: (height.min(i32::MAX as u32) as i32) / s,
            s,
        }
    }

    fn finish(self) -> UiDrawList {
        self.list
    }

    fn phys(&self, r: UiRect) -> UiRect {
        UiRect::new(
            r.x * self.s,
            r.y * self.s,
            (r.w as i32 * self.s).max(0) as u32,
            (r.h as i32 * self.s).max(0) as u32,
        )
    }

    fn rect(&self, x: i32, y: i32, w: i32, h: i32) -> UiRect {
        UiRect::new(x, y, w.max(0) as u32, h.max(0) as u32)
    }

    /// Intersection of two rects (logical pixels).
    fn clip_rect(a: UiRect, b: UiRect) -> Option<UiRect> {
        let x0 = a.x.max(b.x);
        let y0 = a.y.max(b.y);
        let x1 = a.right().min(b.right());
        let y1 = a.bottom().min(b.bottom());
        if x1 <= x0 || y1 <= y0 {
            return None;
        }
        Some(UiRect::new(x0, y0, (x1 - x0) as u32, (y1 - y0) as u32))
    }

    fn fill(&mut self, x: i32, y: i32, w: i32, h: i32, color: UiColor) {
        let r = self.rect(x, y, w, h);
        self.list.fill_rect(self.phys(r), color);
    }

    fn fill_rect(&mut self, r: UiRect, color: UiColor) {
        self.list.fill_rect(self.phys(r), color);
    }

    fn fill_rect_clipped(&mut self, r: UiRect, clip: UiRect, color: UiColor) {
        if let Some(r) = Self::clip_rect(r, clip) {
            self.list.fill_rect(self.phys(r), color);
        }
    }

    fn border(&mut self, x: i32, y: i32, w: i32, h: i32, color: UiColor) {
        let r = self.rect(x, y, w, h);
        self.list
            .border_rect(self.phys(r), color, self.s.max(1) as u32);
    }

    fn border_rect(&mut self, r: UiRect, color: UiColor) {
        self.list
            .border_rect(self.phys(r), color, self.s.max(1) as u32);
    }

    fn border_rect_clipped(&mut self, r: UiRect, clip: UiRect, color: UiColor) {
        if let Some(r) = Self::clip_rect(r, clip) {
            self.list
                .border_rect(self.phys(r), color, self.s.max(1) as u32);
        }
    }

    fn text_fit(&mut self, x: i32, y: i32, mult: u32, color: UiColor, text: &str, cells: usize) {
        let fitted = fit_text(text, cells);
        self.list.text_str(
            x * self.s,
            y * self.s,
            (mult as i32 * self.s).max(1) as u32,
            color,
            &fitted,
        );
    }

    /// Text emitted only while its top-left lands inside `clip` — used
    /// for list rows, which are clipped as whole bands.
    fn text_clipped(
        &mut self,
        x: i32,
        y: i32,
        mult: u32,
        color: UiColor,
        text: &str,
        clip: UiRect,
    ) {
        if y < clip.y || y + 8 * mult as i32 > clip.bottom() {
            return;
        }
        self.text_fit(x, y, mult, color, text, ((clip.right() - x - 1) / (8 * mult as i32)).max(0) as usize);
    }

    #[allow(clippy::too_many_arguments)]
    fn text_fit_clipped(
        &mut self,
        x: i32,
        y: i32,
        mult: u32,
        color: UiColor,
        text: &str,
        cells: usize,
        clip: UiRect,
    ) {
        if y < clip.y || y + 8 * mult as i32 > clip.bottom() {
            return;
        }
        self.text_fit(x, y, mult, color, text, cells);
    }

    /// Horizontally centered text inside `r` (8px glyph cell, `mult`×).
    fn ctext_in(&mut self, r: UiRect, mult: u32, color: UiColor, text: &str) {
        let text = fit_text(text, ((r.w as i32 - 4) / (8 * mult as i32)).max(0) as usize);
        let scale = (mult as i32 * self.s).max(1) as u32;
        let w = text_width(&text, scale) as i32;
        let cx = r.x + r.w as i32 / 2;
        let ty = r.y + ((r.h as i32 - 8 * mult as i32) / 2).max(0);
        self.list
            .text_str(cx * self.s - w / 2, ty * self.s, scale, color, &text);
    }

    /// Centered label, drawn only while `r` fits fully inside `clip` —
    /// a half-clipped badge draws nothing rather than bleeding text
    /// outside the list band.
    fn ctext_in_clipped(&mut self, r: UiRect, clip: UiRect, mult: u32, color: UiColor, text: &str) {
        if r.y >= clip.y && r.bottom() <= clip.bottom() && r.x >= clip.x && r.right() <= clip.right()
        {
            self.ctext_in(r, mult, color, text);
        }
    }

    fn region(&self, hits: &mut Vec<ModSyncHitRegion>, id: ModSyncWidgetId, r: UiRect) {
        let p = self.phys(r);
        let (x0, y0) = (p.x.max(0), p.y.max(0));
        let (x1, y1) = (p.right().min(self.w * self.s), p.bottom().min(self.h * self.s));
        if x1 <= x0 || y1 <= y0 {
            return;
        }
        hits.push(ModSyncHitRegion {
            rect: UiRect::new(x0, y0, (x1 - x0) as u32, (y1 - y0) as u32),
            id,
        });
    }

    fn region_clipped(
        &self,
        hits: &mut Vec<ModSyncHitRegion>,
        id: ModSyncWidgetId,
        r: UiRect,
        clip: UiRect,
    ) {
        if let Some(r) = Self::clip_rect(r, clip) {
            self.region(hits, id, r);
        }
    }
}

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
    let mut clipped = first_cells(&label, cells - 3).to_owned();
    clipped.push_str("...");
    clipped
}

/// Word-wrap `text` to `cells` glyph cells per line; overlong words are
/// hard-split so no line ever exceeds the width.
fn wrap_text(text: &str, cells: usize) -> Vec<String> {
    if cells == 0 {
        return Vec::new();
    }
    let mut lines: Vec<String> = Vec::new();
    let mut cur = String::new();
    for word in text.split_whitespace() {
        let mut rest = word;
        loop {
            if rest.is_empty() {
                break;
            }
            let cur_len = text_cells(&cur);
            let sep = usize::from(!cur.is_empty());
            let avail = cells.saturating_sub(cur_len + sep);
            if text_cells(rest) <= avail {
                if sep == 1 {
                    cur.push(' ');
                }
                cur.push_str(rest);
                break;
            }
            if cur_len > 0 {
                if avail == 0 {
                    lines.push(std::mem::take(&mut cur));
                    continue;
                }
                if sep == 1 {
                    cur.push(' ');
                }
                let at = first_cells(rest, avail).len();
                cur.push_str(&rest[..at]);
                rest = &rest[at..];
                lines.push(std::mem::take(&mut cur));
            } else {
                let at = first_cells(rest, cells).len();
                cur.push_str(&rest[..at]);
                rest = &rest[at..];
                lines.push(std::mem::take(&mut cur));
            }
        }
    }
    if !cur.is_empty() {
        lines.push(cur);
    }
    lines
}
