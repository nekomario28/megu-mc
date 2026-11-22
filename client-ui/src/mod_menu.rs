// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! ModMenu-inspired Mods & Packs manager screen for the client UI.
//!
//! The host supplies plain [`ModMenuEntry`] metadata (parsed by its own mod
//! loader); the screen owns the interactive state: a forgiving multi-token
//! search with `@author` / `#tag` prefix filters, clickable filter pills, a
//! sort cycle, a two-pane master-detail layout with a scrollable mod list,
//! per-mod enable toggles batched into a single zero-restart-or-soft-reload
//! `Apply`, and one-click Modrinth actions for missing dependencies.
//!
//! Like [`crate::shader_screen`], rendering produces a deterministic
//! [`UiDrawList`] plus a parallel `Vec<`[`ModMenuHitRegion`]`>`; input arrives
//! as device-agnostic [`UiInputEvent`]s and returns [`ModMenuAction`]s the
//! embedding application performs. Everything is pure `std` — no client-port
//! types, no GPU resources, no I/O.

use std::collections::BTreeMap;

use crate::draw::{palette, text_width, UiColor, UiDrawList, UiRect};
use crate::state::{tr, GamepadButton, UiInputEvent, UiKey, UiLang};

// ---------------------------------------------------------------------------
// Mod metadata supplied by the host
// ---------------------------------------------------------------------------

/// Which loader ecosystem an entry belongs to.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ModLoaderTag {
    /// First-class mc-spine native module.
    #[default]
    MeguNative,
    /// Fabric mod running under the compatibility shim.
    Fabric,
    /// NeoForge mod running under the compatibility shim.
    NeoForge,
    /// Vanilla datapack.
    Datapack,
    /// Shaderpack (not a code mod).
    ShaderPack,
    /// Shared library jar with no user-facing content.
    Library,
}

impl ModLoaderTag {
    /// Short display label used on detail-pane badges and row subtitles.
    pub fn label(self) -> &'static str {
        match self {
            Self::MeguNative => "Megu",
            Self::Fabric => "Fabric",
            Self::NeoForge => "NeoForge",
            Self::Datapack => "Datapack",
            Self::ShaderPack => "ShaderPack",
            Self::Library => "Library",
        }
    }

    /// Lowercase single-token label matched by free-text search.
    fn search_label(self) -> &'static str {
        match self {
            Self::MeguNative => "megu",
            Self::Fabric => "fabric",
            Self::NeoForge => "neoforge",
            Self::Datapack => "datapack",
            Self::ShaderPack => "shaderpack",
            Self::Library => "library",
        }
    }
}

/// How well the mod has been adapted to the native runtime.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ModAdaptationBadge {
    /// Fully native port (Tier 1).
    #[default]
    Tier1Native,
    /// Working via the compatibility shim (Tier 2).
    Tier2Shim,
    /// Degraded fallback path (Tier 3) — functional but flagged.
    Tier3Fallback,
    /// A newer native replacement exists.
    Superseded,
    /// Client-side only; never synced to a server.
    ClientOnly,
}

impl ModAdaptationBadge {
    /// Detail-pane badge label.
    pub fn label(self) -> &'static str {
        match self {
            Self::Tier1Native => "Tier 1 Native",
            Self::Tier2Shim => "Tier 2 Shim",
            Self::Tier3Fallback => "Tier 3 Fallback",
            Self::Superseded => "Superseded",
            Self::ClientOnly => "Client-Only",
        }
    }

    /// Lowercase token matched by free-text search.
    fn search_label(self) -> &'static str {
        match self {
            Self::Tier1Native => "native",
            Self::Tier2Shim => "shim",
            Self::Tier3Fallback => "fallback",
            Self::Superseded => "superseded",
            Self::ClientOnly => "client",
        }
    }
}

/// What applying an enable/disable change costs.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ModReloadBadge {
    /// Toggles live with no restart at all.
    #[default]
    HotReload,
    /// Needs an in-process soft reload of mod containers.
    SoftRestart,
    /// Needs a full client process restart.
    ProcessRestart,
}

impl ModReloadBadge {
    /// Detail-pane badge label.
    pub fn label(self, lang: UiLang) -> &'static str {
        match self {
            Self::HotReload => tr(lang, "No Restart", "再起動不要"),
            Self::SoftRestart => tr(lang, "Soft Reload", "ソフト再読込"),
            Self::ProcessRestart => tr(lang, "Restart", "再起動"),
        }
    }
}

/// One mod/pack row's metadata, supplied by the host's mod loader.
///
/// `missing_deps` is empty when every dependency is satisfied; a non-empty
/// list renders a prominent warning plus a Modrinth download button.
/// The four `*_url` fields are optional — an empty string hides that link
/// button entirely.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ModMenuEntry {
    pub mod_id: String,
    pub name: String,
    pub version: String,
    pub summary: String,
    pub authors: Vec<String>,
    pub license: String,
    pub loader: ModLoaderTag,
    pub tier: ModAdaptationBadge,
    pub reload_mode: ModReloadBadge,
    pub enabled: bool,
    pub is_library: bool,
    pub has_config: bool,
    pub missing_deps: Vec<String>,
    pub website_url: String,
    pub issues_url: String,
    pub source_url: String,
    pub modrinth_url: String,
}

impl ModMenuEntry {
    /// Minimal entry: `enabled`, Tier-1 native, hot-reloadable, no links.
    pub fn new(mod_id: &str, name: &str, version: &str) -> Self {
        Self {
            mod_id: mod_id.to_owned(),
            name: name.to_owned(),
            version: version.to_owned(),
            enabled: true,
            ..Self::default()
        }
    }

    /// `true` when the entry needs attention: unresolved dependencies or a
    /// degraded Tier-3 fallback path.
    pub fn has_issue(&self) -> bool {
        !self.missing_deps.is_empty() || self.tier == ModAdaptationBadge::Tier3Fallback
    }
}

// ---------------------------------------------------------------------------
// Filters, sort, widget ids, actions
// ---------------------------------------------------------------------------

/// The clickable filter-pill selection.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ModMenuFilter {
    /// Every entry (subject to the library toggle and search).
    #[default]
    All,
    /// Only effectively-enabled mods.
    Enabled,
    /// Mods needing attention: missing deps or Tier-3 fallback.
    Issues,
    /// Tier-1 native ports only.
    Native,
}

/// The clickable sort toggle.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ModMenuSort {
    /// Name ascending.
    #[default]
    NameAsc,
    /// Name descending.
    NameDesc,
    /// Issues first, then enabled, then disabled (name order inside each).
    Status,
}

impl ModMenuSort {
    /// Cycle order: A-Z → Z-A → Status → A-Z.
    pub fn next(self) -> Self {
        match self {
            Self::NameAsc => Self::NameDesc,
            Self::NameDesc => Self::Status,
            Self::Status => Self::NameAsc,
        }
    }

    fn label(self, lang: UiLang) -> &'static str {
        match self {
            Self::NameAsc => "A-Z",
            Self::NameDesc => "Z-A",
            Self::Status => tr(lang, "Status", "状態"),
        }
    }
}

/// Every interactive element on the screen.
///
/// `ModRow` carries the row's index in the *currently visible* (filtered,
/// searched, sorted) list — the same ordering [`ModMenuState::visible_indices`]
/// reports.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModMenuWidgetId {
    /// The search text field — clicking focuses it for typing.
    SearchField,
    /// The `x` button clearing the search text.
    SearchClear,
    /// Filter pills.
    FilterAll,
    FilterEnabled,
    FilterIssues,
    FilterNative,
    /// `Libs: ON/OFF` library visibility toggle.
    ToggleLibraries,
    /// Cycles A-Z → Z-A → Status.
    SortMode,
    /// List scroll buttons.
    ListScrollUp,
    ListScrollDown,
    /// A visible mod row; payload is the visible-list index.
    ModRow(usize),
    /// Detail-pane "Download Missing (Modrinth)" button.
    DownloadMissingDeps,
    /// Detail-pane Enable/Disable toggle.
    ToggleEnabled,
    /// Detail-pane Configure button.
    ConfigureMod,
    /// Detail-pane optional link buttons.
    LinkWebsite,
    LinkIssues,
    LinkSource,
    LinkModrinth,
    /// Footer "+ Add from Modrinth".
    AddFromModrinth,
    /// Footer "Open Mods Folder".
    OpenModsFolder,
    /// Footer "Apply" (batches pending enable/disable changes).
    ApplyChanges,
    /// Footer "Done".
    Done,
}

/// A clickable rectangle emitted alongside the draw list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ModMenuHitRegion {
    pub rect: UiRect,
    pub id: ModMenuWidgetId,
}

impl ModMenuHitRegion {
    /// Point containment test (right/bottom exclusive).
    pub const fn contains(self, x: i32, y: i32) -> bool {
        self.rect.contains(x, y)
    }
}

/// What the embedding application must perform after an interaction.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ModMenuAction {
    /// Open the Modrinth browser/install flow ("+ Add from Modrinth").
    OpenModrinthBrowser,
    /// Reveal the on-disk mods directory.
    OpenModsFolder,
    /// Open a URL in the system browser (Website/Issues/Source/Modrinth).
    OpenUrl(String),
    /// Open the selected mod's config screen.
    ConfigureMod(String),
    /// Fetch exactly these missing dependency ids from Modrinth for `mod_id`.
    DownloadMissingFromModrinth {
        mod_id: String,
        missing_deps: Vec<String>,
    },
    /// Commit every pending enable/disable change.
    ///
    /// `enabled_ids`/`disabled_ids` hold the mods that changed in each
    /// direction. `requires_soft_restart` is `false` only when every changed
    /// mod is [`ModReloadBadge::HotReload`]; a `SoftRestart` *or*
    /// `ProcessRestart` change sets it (the host can read each entry's
    /// `reload_mode` for the exact escalation level).
    ApplyChanges {
        enabled_ids: Vec<String>,
        disabled_ids: Vec<String>,
        requires_soft_restart: bool,
    },
    /// Leave the screen (Done / Escape / gamepad East).
    Close,
}

// ---------------------------------------------------------------------------
// Search query parsing & scoring
// ---------------------------------------------------------------------------

/// One whitespace-separated search token.
#[derive(Clone, Debug, PartialEq, Eq)]
enum QueryToken {
    /// Free text matched against name/id/summary/authors/loader/tier.
    Text(String),
    /// `@author` — substring match against the authors list.
    Author(String),
    /// `#tag` — a structured predicate; `None` = unrecognized tag that
    /// matches nothing (fail-closed so typos don't silently widen results).
    Tag(Option<Tag>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Tag {
    Native,
    Shim,
    Fallback,
    Missing,
    Disabled,
    Enabled,
    Lib,
    Config,
}

fn parse_query(search: &str) -> Vec<QueryToken> {
    search
        .split_whitespace()
        .map(|raw| {
            let token = raw.to_lowercase();
            if let Some(author) = token.strip_prefix('@') {
                if author.is_empty() {
                    // A lone '@' is meaningless; ignore it rather than
                    // excluding every entry.
                    return QueryToken::Text(String::new());
                }
                return QueryToken::Author(author.to_owned());
            }
            if let Some(tag) = token.strip_prefix('#') {
                let tag = match tag {
                    "native" => Some(Tag::Native),
                    "shim" => Some(Tag::Shim),
                    "fallback" => Some(Tag::Fallback),
                    "missing" => Some(Tag::Missing),
                    "disabled" => Some(Tag::Disabled),
                    "enabled" => Some(Tag::Enabled),
                    "lib" | "library" => Some(Tag::Lib),
                    "config" => Some(Tag::Config),
                    _ => None,
                };
                return QueryToken::Tag(tag);
            }
            QueryToken::Text(token)
        })
        .collect()
}

/// `true` when `needle`'s chars appear in `hay` in order (both lowercase).
fn is_subsequence(needle: &str, hay: &str) -> bool {
    let mut chars = hay.chars();
    for n in needle.chars() {
        if !chars.by_ref().any(|h| h == n) {
            return false;
        }
    }
    !needle.is_empty()
}

/// Match rank of a lowercase token against a lowercase field: exact >
/// prefix > substring > subsequence. `None` = no match.
fn field_rank(token: &str, field: &str) -> Option<u32> {
    if token.is_empty() {
        return Some(0);
    }
    if field == token {
        Some(4)
    } else if field.starts_with(token) {
        Some(3)
    } else if field.contains(token) {
        Some(2)
    } else if is_subsequence(token, field) {
        Some(1)
    } else {
        None
    }
}

/// Best score a free-text token gets across an entry's searchable fields;
/// `None` when the token matches none of them. Field weights keep name hits
/// ahead of summary hits.
fn text_token_score(entry: &ModMenuEntry, token: &str) -> Option<u32> {
    let name = entry.name.to_lowercase();
    let id = entry.mod_id.to_lowercase();
    let summary = entry.summary.to_lowercase();
    let mut best = field_rank(token, &name).map(|r| r * 10 + 6);
    best = best.max(field_rank(token, &id).map(|r| r * 10 + 5));
    for author in &entry.authors {
        best = best.max(field_rank(token, &author.to_lowercase()).map(|r| r * 10 + 3));
    }
    best = best.max(field_rank(token, entry.loader.search_label()).map(|r| r * 10 + 2));
    best = best.max(field_rank(token, entry.tier.search_label()).map(|r| r * 10 + 2));
    best = best.max(field_rank(token, &summary).map(|r| r * 10 + 1));
    best
}

fn tag_matches(tag: Tag, entry: &ModMenuEntry, enabled: bool) -> bool {
    match tag {
        Tag::Native => entry.tier == ModAdaptationBadge::Tier1Native,
        Tag::Shim => entry.tier == ModAdaptationBadge::Tier2Shim,
        Tag::Fallback => entry.tier == ModAdaptationBadge::Tier3Fallback,
        Tag::Missing => !entry.missing_deps.is_empty(),
        Tag::Disabled => !enabled,
        Tag::Enabled => enabled,
        Tag::Lib => entry.is_library,
        Tag::Config => entry.has_config,
    }
}

// ---------------------------------------------------------------------------
// Screen state
// ---------------------------------------------------------------------------

/// Interactive state of the Mods & Packs screen.
///
/// `entries` is host-owned metadata; enable toggles stage into `pending`
/// until [`ModMenuAction::ApplyChanges`] commits them back onto the entries.
#[derive(Clone, Debug)]
pub struct ModMenuState {
    /// All installed mods/packs/libraries, in host-supplied order.
    pub entries: Vec<ModMenuEntry>,
    /// Live search text (edited by typing / Backspace / SearchClear).
    pub search: String,
    /// Active filter pill.
    pub filter: ModMenuFilter,
    /// Active sort mode.
    pub sort: ModMenuSort,
    /// Whether `is_library` entries appear without a `#lib` query.
    pub show_libraries: bool,
    /// UI language for chrome labels.
    pub lang: UiLang,
    /// `0` = auto (`height/270`, clamped `1..=4`), else a fixed GUI scale.
    pub ui_scale: u8,
    /// Widget under the mouse cursor (for hover highlight).
    pub hover: Option<ModMenuWidgetId>,
    /// Whether the search field owns typed characters.
    search_focused: bool,
    /// Selection as an index into the *visible* list.
    selected: usize,
    /// First row painted in the list pane (clamped to the list at render).
    scroll: usize,
    /// Staged enable overrides: `mod_id` → desired enabled state.
    pending: BTreeMap<String, bool>,
}

impl Default for ModMenuState {
    fn default() -> Self {
        Self {
            entries: Vec::new(),
            search: String::new(),
            filter: ModMenuFilter::All,
            sort: ModMenuSort::NameAsc,
            show_libraries: false,
            lang: UiLang::En,
            ui_scale: 0,
            hover: None,
            search_focused: true,
            selected: 0,
            scroll: 0,
            pending: BTreeMap::new(),
        }
    }
}

impl ModMenuState {
    pub fn new(entries: Vec<ModMenuEntry>) -> Self {
        Self {
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

    /// Whether the search field currently receives typed characters.
    pub fn search_focused(&self) -> bool {
        self.search_focused
    }

    /// Selected row as an index into [`visible_indices`](Self::visible_indices).
    pub fn selected_row(&self) -> usize {
        self.selected
    }

    /// First visible row index (pre-clamp; rendering keeps it in range).
    pub fn scroll_offset(&self) -> usize {
        self.scroll
    }

    /// Selected entry, if the visible list is non-empty.
    pub fn selected_entry(&self) -> Option<&ModMenuEntry> {
        self.visible_indices()
            .get(self.selected)
            .and_then(|&i| self.entries.get(i))
    }

    /// `true` when at least one enable/disable toggle is staged.
    pub fn is_dirty(&self) -> bool {
        !self.pending.is_empty()
    }

    /// Staged override for `mod_id`, if any.
    pub fn pending_change(&self, mod_id: &str) -> Option<bool> {
        self.pending.get(mod_id).copied()
    }

    /// Enabled state after staged overrides (what the UI actually shows).
    pub fn effective_enabled(&self, mod_id: &str) -> Option<bool> {
        let entry = self.entries.iter().find(|e| e.mod_id == mod_id)?;
        Some(self.pending.get(mod_id).copied().unwrap_or(entry.enabled))
    }

    fn enabled_at(&self, index: usize) -> bool {
        let entry = &self.entries[index];
        self.pending
            .get(&entry.mod_id)
            .copied()
            .unwrap_or(entry.enabled)
    }

    /// Entry indices visible under the current filter pill, library toggle,
    /// search query, and sort mode — in display order.
    ///
    /// Every whitespace-separated search token must match (`AND` semantics):
    /// `@x` requires an author containing `x`, `#tag` applies its predicate,
    /// and plain text scores against name/id/summary/authors/loader/tier
    /// (exact > prefix > substring > subsequence). When plain-text tokens
    /// are present, results order by that relevance score first and the
    /// active sort mode second. A `#lib` token overrides the library filter
    /// so libraries can be found without flipping the pill.
    pub fn visible_indices(&self) -> Vec<usize> {
        let tokens = parse_query(&self.search);
        let lib_requested = tokens
            .iter()
            .any(|t| matches!(t, QueryToken::Tag(Some(Tag::Lib))));
        let mut scored: Vec<(usize, u32)> = Vec::with_capacity(self.entries.len());
        for (index, entry) in self.entries.iter().enumerate() {
            if !self.show_libraries && entry.is_library && !lib_requested {
                continue;
            }
            let enabled = self.enabled_at(index);
            let passes_pill = match self.filter {
                ModMenuFilter::All => true,
                ModMenuFilter::Enabled => enabled,
                ModMenuFilter::Issues => entry.has_issue(),
                ModMenuFilter::Native => entry.tier == ModAdaptationBadge::Tier1Native,
            };
            if !passes_pill {
                continue;
            }
            let mut score = 0u32;
            let mut ok = true;
            for token in &tokens {
                match token {
                    QueryToken::Text(text) if !text.is_empty() => {
                        match text_token_score(entry, text) {
                            Some(s) => score += s,
                            None => {
                                ok = false;
                                break;
                            }
                        }
                    }
                    QueryToken::Text(_) => {}
                    QueryToken::Author(author) => {
                        if !entry
                            .authors
                            .iter()
                            .any(|a| a.to_lowercase().contains(author.as_str()))
                        {
                            ok = false;
                            break;
                        }
                    }
                    QueryToken::Tag(Some(tag)) => {
                        if !tag_matches(*tag, entry, enabled) {
                            ok = false;
                            break;
                        }
                    }
                    QueryToken::Tag(None) => {
                        ok = false;
                        break;
                    }
                }
            }
            if ok {
                scored.push((index, score));
            }
        }
        let has_text = tokens
            .iter()
            .any(|t| matches!(t, QueryToken::Text(s) if !s.is_empty()));
        scored.sort_by(|a, b| {
            if has_text {
                b.1.cmp(&a.1).then_with(|| self.cmp_entries(a.0, b.0))
            } else {
                self.cmp_entries(a.0, b.0)
            }
        });
        scored.into_iter().map(|(index, _)| index).collect()
    }

    fn cmp_entries(&self, a: usize, b: usize) -> std::cmp::Ordering {
        let ea = &self.entries[a];
        let eb = &self.entries[b];
        let name = |e: &ModMenuEntry| e.name.to_lowercase();
        let by_name = name(ea)
            .cmp(&name(eb))
            .then_with(|| ea.mod_id.cmp(&eb.mod_id));
        match self.sort {
            ModMenuSort::NameAsc => by_name,
            ModMenuSort::NameDesc => by_name.reverse(),
            ModMenuSort::Status => {
                let rank = |i: usize| {
                    let e = &self.entries[i];
                    if e.has_issue() {
                        0u8
                    } else if self.enabled_at(i) {
                        1
                    } else {
                        2
                    }
                };
                rank(a).cmp(&rank(b)).then(by_name)
            }
        }
    }

    fn after_view_change(&mut self) {
        self.selected = 0;
        self.scroll = 0;
    }

    fn move_selection(&mut self, offset: i64, wrap: bool) {
        let len = self.visible_indices().len();
        if len == 0 {
            self.selected = 0;
            return;
        }
        let next = self.selected as i64 + offset;
        self.selected = if wrap {
            next.rem_euclid(len as i64) as usize
        } else {
            next.clamp(0, len as i64 - 1) as usize
        };
    }

    fn toggle_selected(&mut self) {
        let Some(&index) = self.visible_indices().get(self.selected) else {
            return;
        };
        let entry = &self.entries[index];
        let want = !self
            .pending
            .get(&entry.mod_id)
            .copied()
            .unwrap_or(entry.enabled);
        if want == entry.enabled {
            self.pending.remove(&entry.mod_id);
        } else {
            self.pending.insert(entry.mod_id.clone(), want);
        }
    }

    /// Stage pending changes into the apply action and commit them onto the
    /// entries. `None` when nothing is staged.
    fn apply(&mut self) -> Option<ModMenuAction> {
        if self.pending.is_empty() {
            return None;
        }
        let mut enabled_ids = Vec::new();
        let mut disabled_ids = Vec::new();
        let mut requires_soft_restart = false;
        for (id, want) in &self.pending {
            let Some(entry) = self.entries.iter().find(|e| e.mod_id == *id) else {
                continue;
            };
            if entry.enabled == *want {
                continue;
            }
            if *want {
                enabled_ids.push(id.clone());
            } else {
                disabled_ids.push(id.clone());
            }
            if entry.reload_mode != ModReloadBadge::HotReload {
                requires_soft_restart = true;
            }
        }
        for (id, want) in std::mem::take(&mut self.pending) {
            if let Some(entry) = self.entries.iter_mut().find(|e| e.mod_id == *id) {
                entry.enabled = want;
            }
        }
        Some(ModMenuAction::ApplyChanges {
            enabled_ids,
            disabled_ids,
            requires_soft_restart,
        })
    }

    /// Activate a widget (from a click, or directly in tests). Returns the
    /// outward actions the host must perform — usually empty, one element
    /// for buttons.
    pub fn activate(&mut self, id: ModMenuWidgetId) -> Vec<ModMenuAction> {
        match id {
            ModMenuWidgetId::SearchField => self.search_focused = true,
            ModMenuWidgetId::SearchClear => {
                if !self.search.is_empty() {
                    self.search.clear();
                    self.after_view_change();
                }
            }
            ModMenuWidgetId::FilterAll => self.set_filter(ModMenuFilter::All),
            ModMenuWidgetId::FilterEnabled => self.set_filter(ModMenuFilter::Enabled),
            ModMenuWidgetId::FilterIssues => self.set_filter(ModMenuFilter::Issues),
            ModMenuWidgetId::FilterNative => self.set_filter(ModMenuFilter::Native),
            ModMenuWidgetId::ToggleLibraries => {
                self.show_libraries = !self.show_libraries;
                self.after_view_change();
            }
            ModMenuWidgetId::SortMode => {
                self.sort = self.sort.next();
                self.after_view_change();
            }
            ModMenuWidgetId::ListScrollUp => {
                self.scroll = self.scroll.saturating_sub(1);
            }
            ModMenuWidgetId::ListScrollDown => {
                let len = self.visible_indices().len();
                self.scroll = (self.scroll + 1).min(len.saturating_sub(1));
            }
            ModMenuWidgetId::ModRow(row) => {
                if row < self.visible_indices().len() {
                    self.selected = row;
                }
            }
            ModMenuWidgetId::ToggleEnabled => self.toggle_selected(),
            ModMenuWidgetId::DownloadMissingDeps => {
                if let Some(entry) = self.selected_entry() {
                    if !entry.missing_deps.is_empty() {
                        return vec![ModMenuAction::DownloadMissingFromModrinth {
                            mod_id: entry.mod_id.clone(),
                            missing_deps: entry.missing_deps.clone(),
                        }];
                    }
                }
            }
            ModMenuWidgetId::ConfigureMod => {
                if let Some(entry) = self.selected_entry() {
                    if entry.has_config {
                        return vec![ModMenuAction::ConfigureMod(entry.mod_id.clone())];
                    }
                }
            }
            ModMenuWidgetId::LinkWebsite
            | ModMenuWidgetId::LinkIssues
            | ModMenuWidgetId::LinkSource
            | ModMenuWidgetId::LinkModrinth => {
                if let Some(entry) = self.selected_entry() {
                    let url = match id {
                        ModMenuWidgetId::LinkWebsite => &entry.website_url,
                        ModMenuWidgetId::LinkIssues => &entry.issues_url,
                        ModMenuWidgetId::LinkSource => &entry.source_url,
                        _ => &entry.modrinth_url,
                    };
                    if !url.is_empty() {
                        return vec![ModMenuAction::OpenUrl(url.clone())];
                    }
                }
            }
            ModMenuWidgetId::AddFromModrinth => {
                return vec![ModMenuAction::OpenModrinthBrowser];
            }
            ModMenuWidgetId::OpenModsFolder => {
                return vec![ModMenuAction::OpenModsFolder];
            }
            ModMenuWidgetId::ApplyChanges => {
                if let Some(action) = self.apply() {
                    return vec![action];
                }
            }
            ModMenuWidgetId::Done => return vec![ModMenuAction::Close],
        }
        Vec::new()
    }

    fn set_filter(&mut self, filter: ModMenuFilter) {
        if self.filter != filter {
            self.filter = filter;
            self.after_view_change();
        }
    }

    /// The widget under `(x, y)` in a `width`×`height` frame, if any
    /// (physical pixels; regions are recomputed from the current state).
    pub fn widget_at(&self, width: u32, height: u32, x: i32, y: i32) -> Option<ModMenuWidgetId> {
        self.render(width, height)
            .1
            .iter()
            .rev()
            .find(|h| h.rect.contains(x, y))
            .map(|h| h.id)
    }

    /// Route one device-agnostic input event; returns the actions the app
    /// must perform. `width`/`height` are the frame's physical pixels, used
    /// for pointer hit-testing and page-step size.
    ///
    /// Keyboard/gamepad semantics (ModMenu-style):
    ///
    /// * typing appends to the search and focuses it; `Backspace` edits it;
    /// * `Up`/`Down` wrap through the visible list, `Left`/`Right` page it;
    /// * `Tab` toggles search focus; `Enter`/`Space` commit the search when
    ///   focused, otherwise toggle the selected mod's enabled state;
    /// * `Escape` (and gamepad `East`/`Start`) closes the screen;
    /// * gamepad `South` mirrors `Enter`, D-pad mirrors the arrows.
    pub fn handle_input(
        &mut self,
        ev: UiInputEvent,
        width: u32,
        height: u32,
    ) -> Vec<ModMenuAction> {
        match ev {
            UiInputEvent::MouseMove { x, y } => {
                self.hover = self.widget_at(width, height, x, y);
                Vec::new()
            }
            UiInputEvent::Click { x, y } => match self.widget_at(width, height, x, y) {
                Some(id) => {
                    if id != ModMenuWidgetId::SearchField {
                        self.search_focused = false;
                    }
                    self.activate(id)
                }
                None => Vec::new(),
            },
            UiInputEvent::KeyPressed(key) => self.handle_key(key, height),
            UiInputEvent::CharTyped(c) => {
                if (' '..='~').contains(&c) {
                    self.search_focused = true;
                    self.search.push(c);
                    self.after_view_change();
                }
                Vec::new()
            }
            UiInputEvent::GamepadPressed(btn) => match btn {
                GamepadButton::DpadUp => self.handle_key(UiKey::Up, height),
                GamepadButton::DpadDown => self.handle_key(UiKey::Down, height),
                GamepadButton::DpadLeft => self.handle_key(UiKey::Left, height),
                GamepadButton::DpadRight => self.handle_key(UiKey::Right, height),
                GamepadButton::South => self.handle_key(UiKey::Enter, height),
                GamepadButton::East | GamepadButton::Start => {
                    vec![ModMenuAction::Close]
                }
            },
        }
    }

    fn handle_key(&mut self, key: UiKey, height: u32) -> Vec<ModMenuAction> {
        match key {
            UiKey::Up => self.move_selection(-1, true),
            UiKey::Down => self.move_selection(1, true),
            UiKey::Left | UiKey::Right => {
                let page = self.list_page_rows(height).max(1) as i64;
                self.move_selection(if key == UiKey::Left { -page } else { page }, false);
            }
            UiKey::Tab => self.search_focused = !self.search_focused,
            UiKey::Enter | UiKey::Space => {
                if self.search_focused {
                    self.search_focused = false;
                } else {
                    self.toggle_selected();
                }
            }
            UiKey::Backspace => {
                if self.search.pop().is_some() {
                    self.after_view_change();
                }
            }
            UiKey::Escape => return vec![ModMenuAction::Close],
        }
        Vec::new()
    }

    /// Rows per list page at `height` physical px (same math as render).
    fn list_page_rows(&self, height: u32) -> usize {
        let s = self.effective_scale(height) as i32;
        let h = (height.min(i32::MAX as u32) as i32) / s;
        let bottom = h - 4 - LIST_BOTTOM_OFF;
        ((bottom - LIST_TOP - 4).max(0) / ROW_PITCH).max(1) as usize
    }

    // -- rendering ------------------------------------------------------

    /// Build the frame: `(draw_list, hit_regions)`. Everything is computed
    /// in logical pixels and multiplied by [`effective_scale`](Self::effective_scale)
    /// on emit, so the same layout serves `640×360` through `3840×2160` and
    /// every emitted hit region is pixel-identical to its widget's visuals.
    pub fn render(&self, width: u32, height: u32) -> (UiDrawList, Vec<ModMenuHitRegion>) {
        let mut lay = Lay::new(self, width, height);
        let mut hits = Vec::new();
        if lay.w < 220 || lay.h < 150 {
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

        self.render_header(&mut lay, &mut hits);
        self.render_filter_row(&mut lay, &mut hits);
        self.render_panes(&mut lay, &mut hits);
        self.render_footer(&mut lay, &mut hits);
        (lay.finish(), hits)
    }

    fn render_header(&self, lay: &mut Lay, hits: &mut Vec<ModMenuHitRegion>) {
        let lang = self.lang;
        let visible = self.visible_indices().len();
        let title = format!(
            "{} ({} / {})",
            tr(lang, "Mods & Packs", "Modとパック"),
            visible,
            self.entries.len()
        );
        lay.text_fit(
            MARGIN + 4,
            MARGIN + 4,
            2,
            palette::TEXT,
            &title,
            ((lay.w - 2 * MARGIN - 8) / 16).max(1) as usize,
        );

        // Search row: [search field][x][Sort: <mode>]
        let sort_label = format!("{}: {}", tr(lang, "Sort", "並び"), self.sort.label(lang));
        let sort_w = 12 * 8 + 12; // "Sort: Status" is the widest label.
        let clear_w = 18;
        let field_x = MARGIN + 4;
        let field_w = lay.w - field_x - MARGIN - 4 - clear_w - 4 - sort_w - MARGIN;
        let field = UiRect::new(field_x, SEARCH_Y, field_w.max(24) as u32, 16);
        let field_color = if self.search_focused {
            palette::SLOT
        } else {
            palette::BUTTON_DISABLED
        };
        lay.fill_rect(field, field_color);
        lay.border_rect(
            field,
            if self.search_focused {
                palette::ACCENT
            } else {
                palette::BUTTON_BORDER
            },
        );
        let cells = ((field.w as i32 - 10) / 8).max(1) as usize;
        if self.search.is_empty() && !self.search_focused {
            lay.text_fit(
                field.x + 5,
                field.y + 4,
                1,
                palette::TEXT_DIM,
                tr(lang, "Search mods...", "Modを検索..."),
                cells,
            );
        } else {
            let mut shown = self.search.clone();
            if self.search_focused {
                shown.push('_');
            }
            // Right-align the tail so the newest characters stay visible.
            let char_count = shown.chars().count();
            if char_count > cells {
                shown = format!(
                    "...{}",
                    shown
                        .chars()
                        .skip(char_count - cells + 3)
                        .collect::<String>()
                );
            }
            lay.text_fit(field.x + 5, field.y + 4, 1, palette::TEXT, &shown, cells);
        }
        lay.region(hits, ModMenuWidgetId::SearchField, field);

        let clear = UiRect::new(field.right() + 4, SEARCH_Y, clear_w as u32, 16);
        let clear_bg = if self.search.is_empty() {
            palette::BUTTON_DISABLED
        } else if self.hover == Some(ModMenuWidgetId::SearchClear) {
            palette::DANGER
        } else {
            palette::BUTTON
        };
        lay.fill_rect(clear, clear_bg);
        lay.ctext_in(
            clear,
            1,
            if self.search.is_empty() {
                palette::TEXT_DIM
            } else {
                palette::TEXT
            },
            "x",
        );
        if !self.search.is_empty() {
            lay.region(hits, ModMenuWidgetId::SearchClear, clear);
        }

        let sort = UiRect::new(lay.w - MARGIN - sort_w, SEARCH_Y, sort_w as u32, 16);
        let sort_bg = if self.hover == Some(ModMenuWidgetId::SortMode) {
            palette::BUTTON_HOVER
        } else {
            palette::BUTTON
        };
        lay.fill_rect(sort, sort_bg);
        lay.border_rect(sort, palette::BUTTON_BORDER);
        lay.ctext_in(sort, 1, palette::TEXT, &fit_text(&sort_label, 12));
        lay.region(hits, ModMenuWidgetId::SortMode, sort);
    }

    fn render_filter_row(&self, lay: &mut Lay, hits: &mut Vec<ModMenuHitRegion>) {
        let lang = self.lang;
        let pills: [(&str, ModMenuWidgetId, bool); 5] = [
            (
                tr(lang, "All", "すべて"),
                ModMenuWidgetId::FilterAll,
                self.filter == ModMenuFilter::All,
            ),
            (
                tr(lang, "Enabled", "有効"),
                ModMenuWidgetId::FilterEnabled,
                self.filter == ModMenuFilter::Enabled,
            ),
            (
                tr(lang, "Issues", "問題あり"),
                ModMenuWidgetId::FilterIssues,
                self.filter == ModMenuFilter::Issues,
            ),
            (
                tr(lang, "Native", "ネイティブ"),
                ModMenuWidgetId::FilterNative,
                self.filter == ModMenuFilter::Native,
            ),
            (
                if self.show_libraries {
                    tr(lang, "Libs: ON", "ライブラリ: ON")
                } else {
                    tr(lang, "Libs: OFF", "ライブラリ: OFF")
                },
                ModMenuWidgetId::ToggleLibraries,
                self.show_libraries,
            ),
        ];
        let mut x = MARGIN + 4;
        for (label, id, active) in pills {
            let w = label.chars().count() as i32 * 8 + 12;
            if x + w > lay.w - MARGIN {
                break;
            }
            let rect = UiRect::new(x, FILTER_Y, w as u32, 14);
            let bg = if active {
                palette::ACCENT
            } else if self.hover == Some(id) {
                palette::BUTTON_HOVER
            } else {
                palette::BUTTON
            };
            lay.fill_rect(rect, bg);
            lay.border_rect(rect, palette::BUTTON_BORDER);
            lay.ctext_in(
                rect,
                1,
                if active {
                    palette::TEXT_DARK
                } else {
                    palette::TEXT
                },
                label,
            );
            lay.region(hits, id, rect);
            x += w + 4;
        }
    }

    fn render_panes(&self, lay: &mut Lay, hits: &mut Vec<ModMenuHitRegion>) {
        let inner_w = lay.w - 2 * MARGIN - 8;
        // Keep at least a sliver of detail pane at minimum-width windows.
        let min_list_w = 140.min((inner_w - PANE_GAP - 40).max(30));
        let list_w = ((inner_w - PANE_GAP) * 2 / 5).clamp(min_list_w, 420);
        let list_x = MARGIN + 4;
        let list_right = list_x + list_w;
        let detail_x = list_right + PANE_GAP;
        let detail_w = MARGIN + 4 + inner_w - detail_x;
        let top = LIST_TOP;
        let bottom = lay.h - 4 - LIST_BOTTOM_OFF;

        // ---- left pane: scrollable list -------------------------------
        let list_rect = UiRect::new(list_x, top, list_w as u32, (bottom - top) as u32);
        lay.fill_rect(list_rect, DETAIL_BG);
        lay.border_rect(list_rect, palette::PANEL_BORDER);

        let visible = self.visible_indices();
        let page = ((bottom - top - 4) / ROW_PITCH).max(0) as usize;
        let max_scroll = visible.len().saturating_sub(page);
        let scroll = self.scroll.min(max_scroll);
        // Keep the keyboard selection inside the viewport.
        let scroll = if page > 0 && self.selected < scroll {
            self.selected
        } else if page > 0 && self.selected >= scroll + page {
            self.selected + 1 - page
        } else {
            scroll
        };

        let scrollable = visible.len() > page && page > 0;
        let scrollbar_w = if scrollable { 12 } else { 0 };
        let rows_w = list_w - 4 - scrollbar_w;

        if visible.is_empty() {
            let msg = if self.entries.is_empty() {
                tr(
                    self.lang,
                    "No mods installed",
                    "Modがインストールされていません",
                )
            } else {
                tr(self.lang, "No matches", "一致するModがありません")
            };
            lay.text_fit(
                list_x + 6,
                top + 8,
                1,
                palette::TEXT_DIM,
                msg,
                ((list_w - 12) / 8).max(1) as usize,
            );
        }

        for (row_slot, &entry_index) in visible.iter().enumerate().skip(scroll).take(page) {
            let y = top + 2 + (row_slot - scroll) as i32 * ROW_PITCH;
            let row = UiRect::new(list_x + 2, y, rows_w as u32, ROW_H as u32);
            let entry = &self.entries[entry_index];
            let enabled = self.enabled_at(entry_index);
            let is_sel = row_slot == self.selected;
            let is_hover = self.hover == Some(ModMenuWidgetId::ModRow(row_slot));
            lay.fill_rect(
                row,
                if is_sel {
                    palette::BUTTON_HOVER
                } else if is_hover {
                    palette::BUTTON
                } else {
                    palette::SLOT
                },
            );
            if self.pending.contains_key(&entry.mod_id) {
                lay.fill(row.x, row.y, 2, row.h as i32, palette::ACCENT);
            }
            // Status pill (right-aligned on the name line).
            let (pill_text, pill_bg, pill_fg) = if !entry.missing_deps.is_empty() {
                ("MISSING", palette::DANGER, palette::TEXT)
            } else if !enabled {
                ("OFF", palette::BUTTON_DISABLED, palette::TEXT_DIM)
            } else {
                match entry.tier {
                    ModAdaptationBadge::Tier1Native => {
                        ("NATIVE", palette::ACCENT, palette::TEXT_DARK)
                    }
                    ModAdaptationBadge::Tier2Shim => ("SHIM", palette::WARNING, palette::TEXT_DARK),
                    ModAdaptationBadge::Tier3Fallback => {
                        ("FALLBACK", palette::DANGER, palette::TEXT)
                    }
                    ModAdaptationBadge::Superseded => {
                        ("SUPER", palette::BUTTON_DISABLED, palette::TEXT_DIM)
                    }
                    ModAdaptationBadge::ClientOnly => ("CLIENT", palette::AIR, palette::TEXT_DARK),
                }
            };
            let pill_w = pill_text.len() as i32 * 8 + 8;
            let pill = UiRect::new(row.right() - pill_w - 3, y + 3, pill_w as u32, 11);
            lay.fill_rect(pill, pill_bg);
            lay.ctext_in(pill, 1, pill_fg, pill_text);
            let name_cells = ((rows_w - pill_w - 14) / 8).max(1) as usize;
            let name_color = if enabled {
                palette::TEXT
            } else {
                palette::TEXT_DIM
            };
            lay.text_fit(row.x + 5, y + 3, 1, name_color, &entry.name, name_cells);
            let sub = format!("{} · {}", entry.version, entry.loader.label());
            lay.text_fit(
                row.x + 5,
                y + 13,
                1,
                palette::TEXT_DIM,
                &fit_text(&sub, ((rows_w - 10) / 8).max(1) as usize),
                ((rows_w - 10) / 8).max(1) as usize,
            );
            lay.region(hits, ModMenuWidgetId::ModRow(row_slot), row);
        }

        if scrollable {
            let sb_x = list_right - 12;
            let up = UiRect::new(sb_x, top + 2, 10, 12);
            let down = UiRect::new(sb_x, bottom - 14, 10, 12);
            lay.fill_rect(up, palette::BUTTON);
            lay.fill_rect(down, palette::BUTTON);
            lay.ctext_in(up, 1, palette::TEXT, "^");
            lay.ctext_in(down, 1, palette::TEXT, "v");
            // Passive track + thumb (visual only).
            let track = UiRect::new(sb_x, top + 16, 10, (bottom - top - 32).max(0) as u32);
            lay.fill_rect(track, palette::SLOT);
            if !visible.is_empty() {
                let thumb_h = ((page as i64 * track.h as i64) / visible.len() as i64)
                    .clamp(8, track.h as i64) as u32;
                let travel = (track.h - thumb_h).max(1) as i64;
                let thumb_y =
                    track.y + ((scroll as i64 * travel) / max_scroll.max(1) as i64) as i32;
                lay.fill_rect(UiRect::new(sb_x, thumb_y, 10, thumb_h), palette::TEXT_DIM);
            }
            lay.region(hits, ModMenuWidgetId::ListScrollUp, up);
            lay.region(hits, ModMenuWidgetId::ListScrollDown, down);
        }

        // ---- right pane: selected mod details --------------------------
        let detail = UiRect::new(detail_x, top, detail_w.max(1) as u32, (bottom - top) as u32);
        lay.fill_rect(detail, DETAIL_BG);
        lay.border_rect(detail, palette::PANEL_BORDER);
        let Some(&sel_index) = visible.get(self.selected) else {
            lay.text_fit(
                detail_x + 6,
                top + 8,
                1,
                palette::TEXT_DIM,
                tr(self.lang, "Select a mod", "Modを選択"),
                ((detail_w - 12) / 8).max(1) as usize,
            );
            return;
        };
        self.render_detail(lay, hits, detail, sel_index);
    }

    fn render_detail(
        &self,
        lay: &mut Lay,
        hits: &mut Vec<ModMenuHitRegion>,
        pane: UiRect,
        entry_index: usize,
    ) {
        let lang = self.lang;
        let entry = &self.entries[entry_index];
        let enabled = self.enabled_at(entry_index);
        let tx = pane.x + 6;
        let tw = (pane.w as i32 - 12).max(8);
        let cells = (tw / 8).max(1) as usize;

        // ---- action row geometry (anchored to the pane bottom) ---------
        // Buttons: Enable/Disable + Configure + optional links.
        let mut buttons: Vec<(ModMenuWidgetId, String, bool)> = Vec::new();
        let toggle_label = if enabled {
            tr(lang, "Disable", "無効化")
        } else {
            tr(lang, "Enable", "有効化")
        };
        buttons.push((
            ModMenuWidgetId::ToggleEnabled,
            toggle_label.to_owned(),
            true,
        ));
        buttons.push((
            ModMenuWidgetId::ConfigureMod,
            tr(lang, "Configure", "設定").to_owned(),
            entry.has_config,
        ));
        for (id, url, label) in [
            (
                ModMenuWidgetId::LinkWebsite,
                &entry.website_url,
                tr(lang, "Website", "サイト"),
            ),
            (
                ModMenuWidgetId::LinkIssues,
                &entry.issues_url,
                tr(lang, "Issues", "課題"),
            ),
            (
                ModMenuWidgetId::LinkSource,
                &entry.source_url,
                tr(lang, "Source", "ソース"),
            ),
            (
                ModMenuWidgetId::LinkModrinth,
                &entry.modrinth_url,
                "Modrinth",
            ),
        ] {
            if !url.is_empty() {
                buttons.push((id, label.to_owned(), true));
            }
        }
        // Flow-wrap buttons into rows anchored at the pane bottom.
        let btn_h = 14;
        let btn_gap = 4;
        let mut rows: Vec<Vec<(ModMenuWidgetId, String, bool, i32)>> = vec![Vec::new()];
        let mut row_w = 0;
        for (id, label, live) in buttons {
            let w = (label.chars().count() as i32 * 8 + 12).min(tw);
            if row_w > 0 && row_w + btn_gap + w > tw {
                rows.push(Vec::new());
                row_w = 0;
            }
            row_w += if row_w > 0 { btn_gap + w } else { w };
            rows.last_mut()
                .expect("one row always exists")
                .push((id, label, live, w));
        }
        let actions_h = rows.len() as i32 * (btn_h + btn_gap) - btn_gap;
        let mut actions_top = pane.bottom() - 6 - actions_h;

        // ---- missing-dependencies warning box above the actions --------
        // The box has a 36px fixed cost (header + button + padding); dep
        // lines cost 10px each. It shrinks to the space left above the
        // action rows and collapses to a bare button below 36px so it
        // never paints over the pane header or the list above.
        let mut warn_bottom = actions_top - 4;
        let warn_avail = (actions_top - 4 - (pane.y + 4)).max(0);
        let missing_label = tr(
            lang,
            "Download Missing (Modrinth)",
            "不足分を入手 (Modrinth)",
        );
        if !entry.missing_deps.is_empty() && warn_avail >= 18 {
            let line_budget = ((warn_avail - 36) / 10).max(0) as usize;
            let shown = entry.missing_deps.len().min(3).min(line_budget);
            // Reserve one line for "+N more" when the budget cut deps off.
            let (shown, extra) = if entry.missing_deps.len() > shown && line_budget > 0 {
                (shown.saturating_sub(1), true)
            } else {
                (shown, entry.missing_deps.len() > shown)
            };
            let extra = extra && line_budget > shown;
            let content_lines = shown + usize::from(extra);
            let full_box = warn_avail >= 36;
            let warn_h = if full_box {
                8 + 10 + content_lines as i32 * 10 + 16 + 6
            } else {
                18
            };
            let warn = UiRect::new(tx, actions_top - 4 - warn_h, tw as u32, warn_h as u32);
            lay.fill_rect(warn, WARN_BG);
            lay.border_rect(warn, palette::WARNING);
            let mut wy = warn.y + 4;
            if full_box {
                lay.text_fit(
                    warn.x + 5,
                    wy,
                    1,
                    palette::WARNING,
                    tr(lang, "Missing dependencies:", "不足している依存関係:"),
                    cells.saturating_sub(1),
                );
                wy += 10;
                for dep in entry.missing_deps.iter().take(shown) {
                    lay.text_fit(
                        warn.x + 9,
                        wy,
                        1,
                        palette::TEXT,
                        &format!("- {dep}"),
                        cells.saturating_sub(2),
                    );
                    wy += 10;
                }
                if extra {
                    lay.text_fit(
                        warn.x + 9,
                        wy,
                        1,
                        palette::TEXT_DIM,
                        &format!("+{} more", entry.missing_deps.len() - shown),
                        cells.saturating_sub(2),
                    );
                }
            }
            let btn_w = (missing_label.chars().count() as i32 * 8 + 12).min(tw - 8);
            let btn = UiRect::new(warn.x + 4, warn.bottom() - 16, btn_w.max(0) as u32, 14);
            let bg = if self.hover == Some(ModMenuWidgetId::DownloadMissingDeps) {
                palette::BUTTON_HOVER
            } else {
                palette::WARNING
            };
            lay.fill_rect(btn, bg);
            lay.border_rect(btn, palette::BUTTON_BORDER);
            lay.ctext_in(
                btn,
                1,
                palette::TEXT_DARK,
                &fit_text(missing_label, ((btn_w - 8) / 8).max(1) as usize),
            );
            lay.region(hits, ModMenuWidgetId::DownloadMissingDeps, btn);
            warn_bottom = warn.y - 4;
        }

        // ---- header block (name / meta / badges / authors / license) ---
        let mut y = pane.y + 5;
        let name_scale = if text_width(&entry.name, 2) as i32 <= tw {
            2
        } else {
            1
        };
        lay.text_fit(
            tx,
            y,
            name_scale,
            palette::TEXT,
            &entry.name,
            (tw / (8 * name_scale as i32)).max(1) as usize,
        );
        y += 8 * name_scale as i32 + 3;

        let meta = format!("{} · {}", entry.version, entry.mod_id);
        lay.text_fit(tx, y, 1, palette::TEXT_DIM, &meta, cells);
        y += 11;

        // Badge row: loader / tier / reloadability — badges that would
        // cross the pane edge are skipped, not clipped.
        let badge_right = tx + tw;
        let mut bx = tx;
        let loader_bg = match entry.loader {
            ModLoaderTag::MeguNative => palette::ACCENT,
            ModLoaderTag::Datapack => palette::BUTTON_HOVER,
            ModLoaderTag::ShaderPack => palette::AIR,
            _ => palette::BUTTON,
        };
        bx += self.badge(lay, bx, y, entry.loader.label(), loader_bg, badge_right);
        let (tier_bg, tier_fg) = match entry.tier {
            ModAdaptationBadge::Tier1Native => (palette::ACCENT, palette::TEXT_DARK),
            ModAdaptationBadge::Tier2Shim => (palette::WARNING, palette::TEXT_DARK),
            ModAdaptationBadge::Tier3Fallback => (palette::DANGER, palette::TEXT),
            ModAdaptationBadge::Superseded => (palette::BUTTON_DISABLED, palette::TEXT_DIM),
            ModAdaptationBadge::ClientOnly => (palette::AIR, palette::TEXT_DARK),
        };
        bx += self.badge_fg(lay, bx, y, entry.tier.label(), (tier_bg, tier_fg), badge_right);
        let (reload_bg, reload_fg) = match entry.reload_mode {
            ModReloadBadge::HotReload => (palette::ACCENT, palette::TEXT_DARK),
            ModReloadBadge::SoftRestart => (palette::WARNING, palette::TEXT_DARK),
            ModReloadBadge::ProcessRestart => (palette::DANGER, palette::TEXT),
        };
        self.badge_fg(
            lay,
            bx,
            y,
            entry.reload_mode.label(lang),
            (reload_bg, reload_fg),
            badge_right,
        );
        y += 15;

        if !entry.authors.is_empty() {
            let authors = format!(
                "{}: {}",
                tr(lang, "Authors", "作者"),
                entry.authors.join(", ")
            );
            lay.text_fit(tx, y, 1, palette::TEXT_DIM, &authors, cells);
            y += 10;
        }
        if !entry.license.is_empty() {
            let license = format!("{}: {}", tr(lang, "License", "ライセンス"), entry.license);
            lay.text_fit(tx, y, 1, palette::TEXT_DIM, &license, cells);
            y += 10;
        }
        if entry.is_library {
            lay.text_fit(
                tx,
                y,
                1,
                palette::TEXT_DIM,
                tr(lang, "Shared library", "共有ライブラリ"),
                cells,
            );
            y += 10;
        }
        y += 3;

        // ---- wrapped summary in the space left above the warning box ---
        let avail = (warn_bottom - y).max(0);
        let max_lines = (avail / 10).max(0) as usize;
        if max_lines > 0 && !entry.summary.is_empty() {
            for line in wrap_text(&entry.summary, cells).into_iter().take(max_lines) {
                lay.text_fit(tx, y, 1, palette::TEXT_DIM, &line, cells);
                y += 10;
            }
        }

        // ---- paint the anchored action rows ----------------------------
        for row in &rows {
            let mut bx = tx;
            for (id, label, live, w) in row {
                let rect = UiRect::new(bx, actions_top, *w as u32, btn_h as u32);
                let hovered = self.hover == Some(*id);
                let bg = if !*live {
                    palette::BUTTON_DISABLED
                } else if hovered {
                    palette::BUTTON_HOVER
                } else {
                    palette::BUTTON
                };
                lay.fill_rect(rect, bg);
                lay.border_rect(rect, palette::BUTTON_BORDER);
                lay.ctext_in(
                    rect,
                    1,
                    if *live {
                        palette::TEXT
                    } else {
                        palette::TEXT_DIM
                    },
                    &fit_text(label, ((*w - 8) / 8).max(1) as usize),
                );
                if *live {
                    lay.region(hits, *id, rect);
                }
                bx += w + btn_gap;
            }
            actions_top += btn_h + btn_gap;
        }
    }

    fn badge(&self, lay: &mut Lay, x: i32, y: i32, label: &str, bg: UiColor, max_x: i32) -> i32 {
        let fg = if bg == palette::BUTTON || bg == palette::BUTTON_DISABLED {
            palette::TEXT
        } else {
            palette::TEXT_DARK
        };
        self.badge_fg(lay, x, y, label, (bg, fg), max_x)
    }

    /// Returns the advance past the badge, or `0` (drawing nothing) when
    /// it would not fit before `max_x`.
    fn badge_fg(
        &self,
        lay: &mut Lay,
        x: i32,
        y: i32,
        label: &str,
        colors: (UiColor, UiColor),
        max_x: i32,
    ) -> i32 {
        let (bg, fg) = colors;
        let w = label.chars().count() as i32 * 8 + 8;
        if x + w > max_x {
            return 0;
        }
        let rect = UiRect::new(x, y, w as u32, 11);
        lay.fill_rect(rect, bg);
        lay.ctext_in(rect, 1, fg, label);
        w + 4
    }

    fn render_footer(&self, lay: &mut Lay, hits: &mut Vec<ModMenuHitRegion>) {
        let lang = self.lang;
        // Row 1 (upper): [+ Add from Modrinth] [Open Mods Folder]
        let y1 = lay.h - 4 - 16 - 4 - 16;
        let mut x = MARGIN + 4;
        for (id, label) in [
            (
                ModMenuWidgetId::AddFromModrinth,
                tr(lang, "+ Add from Modrinth", "Modrinthから追加"),
            ),
            (
                ModMenuWidgetId::OpenModsFolder,
                tr(lang, "Open Mods Folder", "Modフォルダを開く"),
            ),
        ] {
            // Buttons shrink to the space left; when none is left, skip.
            let avail = lay.w - MARGIN - x;
            let w = (label.chars().count() as i32 * 8 + 14).min(avail);
            if w < 24 {
                break;
            }
            let rect = UiRect::new(x, y1, w as u32, 16);
            let bg = if self.hover == Some(id) {
                palette::BUTTON_HOVER
            } else {
                palette::BUTTON
            };
            lay.fill_rect(rect, bg);
            lay.border_rect(rect, palette::BUTTON_BORDER);
            lay.ctext_in(rect, 1, palette::TEXT, label);
            lay.region(hits, id, rect);
            x += w + 6;
        }

        // Row 2 (lower, right-aligned): [Apply ...] [Done]
        let y2 = lay.h - 4 - 16;
        let dirty = self.is_dirty();
        let apply_label = if !dirty {
            tr(lang, "Apply Changes", "変更を適用").to_owned()
        } else {
            let soft = self.pending.iter().any(|(id, _)| {
                self.entries
                    .iter()
                    .find(|e| &e.mod_id == id)
                    .is_some_and(|e| e.reload_mode != ModReloadBadge::HotReload)
            });
            if soft {
                tr(lang, "Apply (Soft Reload)", "適用 (ソフト再読込)").to_owned()
            } else {
                tr(lang, "Apply (No Restart)", "適用 (再起動不要)").to_owned()
            }
        };
        let done_label = tr(lang, "Done", "完了");
        let done_w = (done_label.chars().count() as i32 * 8 + 20)
            .min(lay.w - 2 * MARGIN)
            .max(24);
        let done = UiRect::new(lay.w - MARGIN - done_w, y2, done_w as u32, 16);
        // Apply shrinks to the space left of Done; below ~60px it hides.
        let apply_w = (apply_label.chars().count() as i32 * 8 + 14)
            .min(done.x - 6 - MARGIN);
        if apply_w >= 24 {
            let apply = UiRect::new(done.x - 6 - apply_w, y2, apply_w as u32, 16);
            let apply_bg = if !dirty {
                palette::BUTTON_DISABLED
            } else if self.hover == Some(ModMenuWidgetId::ApplyChanges) {
                palette::BUTTON_HOVER
            } else {
                palette::ACCENT
            };
            lay.fill_rect(apply, apply_bg);
            lay.border_rect(apply, palette::BUTTON_BORDER);
            lay.ctext_in(
                apply,
                1,
                if dirty {
                    palette::TEXT_DARK
                } else {
                    palette::TEXT_DIM
                },
                &apply_label,
            );
            if dirty {
                lay.region(hits, ModMenuWidgetId::ApplyChanges, apply);
            }
        }

        let done_bg = if self.hover == Some(ModMenuWidgetId::Done) {
            palette::BUTTON_HOVER
        } else {
            palette::BUTTON
        };
        lay.fill_rect(done, done_bg);
        lay.border_rect(done, palette::BUTTON_BORDER);
        lay.ctext_in(done, 1, palette::TEXT, done_label);
        lay.region(hits, ModMenuWidgetId::Done, done);
    }
}

// ---------------------------------------------------------------------------
// Layout context (logical px → physical px at the effective GUI scale)
// ---------------------------------------------------------------------------

const MARGIN: i32 = 4;
const SEARCH_Y: i32 = 26;
const FILTER_Y: i32 = 46;
const LIST_TOP: i32 = 66;
const LIST_BOTTOM_OFF: i32 = 40;
const PANE_GAP: i32 = 6;
const ROW_H: i32 = 22;
const ROW_PITCH: i32 = 24;

/// Right detail-pane fill: a hair lighter than the main panel.
const DETAIL_BG: UiColor = UiColor::rgba(26, 29, 38, 235);
/// Missing-dependencies warning box fill.
const WARN_BG: UiColor = UiColor::rgba(85, 60, 23, 235);

/// Scales logical-pixel geometry to physical pixels on emit so visuals and
/// hit regions never diverge.
struct Lay {
    list: UiDrawList,
    w: i32,
    h: i32,
    s: i32,
}

impl Lay {
    fn new(state: &ModMenuState, width: u32, height: u32) -> Self {
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

    fn fill(&mut self, x: i32, y: i32, w: i32, h: i32, color: UiColor) {
        let r = self.rect(x, y, w, h);
        self.list.fill_rect(self.phys(r), color);
    }

    fn fill_rect(&mut self, r: UiRect, color: UiColor) {
        self.list.fill_rect(self.phys(r), color);
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

    /// Horizontally centered text inside `r` (8px glyph cell, `mult`×).
    /// Labels are elided to the rect first so they can never spill past
    /// the widget's edges.
    fn ctext_in(&mut self, r: UiRect, mult: u32, color: UiColor, text: &str) {
        let text = fit_text(text, ((r.w as i32 - 4) / (8 * mult as i32)).max(0) as usize);
        let scale = (mult as i32 * self.s).max(1) as u32;
        let w = text_width(&text, scale) as i32;
        let cx = r.x + r.w as i32 / 2;
        let ty = r.y + ((r.h as i32 - 8 * mult as i32) / 2).max(0);
        self.list
            .text_str(cx * self.s - w / 2, ty * self.s, scale, color, &text);
    }

    fn region(&self, hits: &mut Vec<ModMenuHitRegion>, id: ModMenuWidgetId, r: UiRect) {
        let p = self.phys(r);
        let (x0, y0) = (p.x.max(0), p.y.max(0));
        let (x1, y1) = (
            p.right().min(self.w * self.s),
            p.bottom().min(self.h * self.s),
        );
        if x1 <= x0 || y1 <= y0 {
            return;
        }
        hits.push(ModMenuHitRegion {
            rect: UiRect::new(x0, y0, (x1 - x0) as u32, (y1 - y0) as u32),
            id,
        });
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
    let count = label.chars().count();
    if count <= cells {
        return label;
    }
    if cells <= 3 {
        return ".".repeat(cells);
    }
    let mut clipped = label.chars().take(cells - 3).collect::<String>();
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
            let cur_len = cur.chars().count();
            let sep = usize::from(!cur.is_empty());
            let avail = cells.saturating_sub(cur_len + sep);
            if rest.chars().count() <= avail {
                if sep == 1 {
                    cur.push(' ');
                }
                cur.push_str(rest);
                break;
            }
            if cur_len > 0 {
                if avail == 0 {
                    // Line is full; wrap to a fresh line.
                    lines.push(std::mem::take(&mut cur));
                    continue;
                }
                // Fill the remainder of this line with a hard-split piece.
                if sep == 1 {
                    cur.push(' ');
                }
                let at = rest
                    .char_indices()
                    .nth(avail)
                    .map(|(i, _)| i)
                    .unwrap_or(rest.len());
                cur.push_str(&rest[..at]);
                rest = &rest[at..];
                lines.push(std::mem::take(&mut cur));
            } else {
                // Empty line: split the overlong word at `cells`.
                let at = rest
                    .char_indices()
                    .nth(cells)
                    .map(|(i, _)| i)
                    .unwrap_or(rest.len());
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
