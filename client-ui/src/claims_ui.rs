// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! Visual chunk-claim map and per-player permission screen — the client
//! half of the `ClaimsWorld` sync feed (OpenPAC/FTB-Chunks-style).
//!
//! The screen is a self-contained [`ClaimsUiState`] the host drives with
//! the same device-agnostic [`UiInputEvent`]s every other screen uses and
//! renders into a pure [`UiDrawList`] plus its [`ClaimsHitRegion`]s. All
//! domain data (claimed chunks, cost quotes, the `known_players` roster)
//! is host-supplied; the screen only ever *emits* [`ClaimsUiAction`]s —
//! it never mutates claim truth itself, so a rejected claim can be rolled
//! back by the host's next sync snapshot.
//!
//! * **Chunk Map** (`マップ`) — a 9×9 grid centered on the player's chunk
//!   with N/E/S/W compass labels, a player marker, and a distinct
//!   accessible color per [`ChunkClaimState`]. Left-click an unclaimed
//!   chunk to emit [`ClaimsUiAction::ClaimChunk`]; right-click (or the
//!   on-screen Unclaim button / `Backspace`) a self-owned chunk to emit
//!   [`ClaimsUiAction::UnclaimChunk`]. Hovering inspects the chunk:
//!   coordinates, owner, free-claim status or the item/XP cost preview,
//!   and active flags.
//! * **Trusted Players** (`プレイヤー許可`) — the `known_players` roster
//!   with a live search filter and six granular per-player toggles
//!   ([`ClaimPermission`]), emitting
//!   [`ClaimsUiAction::SetPlayerPermission`] and
//!   [`ClaimsUiAction::RevokePlayer`].
//!
//! Keyboard/gamepad semantics: `Tab` switches tabs, arrows move the map
//! cursor (or the player/permission selection), `Enter`/`Space`/gamepad
//! `South` activates, `Backspace` unclaims the focused chunk (or revokes
//! the selected player when the search box is empty), and `Escape`/
//! gamepad `East`/`Start` emits [`ClaimsUiAction::Close`].

use std::collections::BTreeMap;

use crate::draw::{palette, text_cells, UiColor, UiDrawList, UiRect};
use crate::state::{tr, GamepadButton, UiInputEvent, UiKey, UiLang};

// ---------------------------------------------------------------------------
// Model
// ---------------------------------------------------------------------------

/// Chunks the map shows on each side of the player (→ 9×9).
pub const CLAIMS_GRID_RADIUS: i32 = 4;

/// Map grid edge length, in chunks.
pub const CLAIMS_GRID_DIM: usize = (2 * CLAIMS_GRID_RADIUS + 1) as usize;

/// Who a map cell's claim belongs to — each variant owns a distinct,
/// colorblind-discernible fill (see [`ChunkClaimState::fill_color`]).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum ChunkClaimState {
    /// Nobody claims this chunk; it can be claimed.
    #[default]
    Unclaimed,
    /// The local player's own claim.
    OwnedBySelf,
    /// A claim owned by a trusted/party player.
    Trusted,
    /// Another (untrusted) player's claim; payload is the display name
    /// the server reported for the owner.
    OwnedByOther(String),
    /// Admin/server claim (spawn protection, infrastructure) — cannot be
    /// claimed or unclaimed by players.
    SpawnProtected,
}

impl ChunkClaimState {
    /// Map fill color for the state. The five hues are far apart in
    /// color space and also differ in lightness, so the map stays
    /// readable for the common color-vision deficiencies.
    pub fn fill_color(&self) -> UiColor {
        match self {
            Self::Unclaimed => UiColor::rgb(46, 48, 56),
            Self::OwnedBySelf => UiColor::rgb(62, 158, 74),
            Self::Trusted => UiColor::rgb(72, 132, 200),
            Self::OwnedByOther(_) => UiColor::rgb(188, 74, 64),
            Self::SpawnProtected => UiColor::rgb(148, 96, 200),
        }
    }

    /// Short legend label for the state.
    pub fn legend_label(&self, lang: UiLang) -> String {
        match self {
            Self::Unclaimed => tr(lang, "Free", "フリー").to_owned(),
            Self::OwnedBySelf => tr(lang, "You", "自分").to_owned(),
            Self::Trusted => tr(lang, "Trusted", "なかま").to_owned(),
            Self::OwnedByOther(_) => tr(lang, "Other", "ほか").to_owned(),
            Self::SpawnProtected => tr(lang, "Spawn", "スポーン").to_owned(),
        }
    }
}

/// One map cell's claim data, as synced from the server.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ClaimChunkCell {
    /// Ownership state (drives the map color).
    pub state: ChunkClaimState,
    /// The claim is force-loaded (keeps ticking while its owner is away).
    pub forceloaded: bool,
    /// Sub-claim name when the chunk belongs to a named sub-claim
    /// (`None` = the `"main"` sub-claim).
    pub sub_claim: Option<String>,
}

/// Client-side claim-cost preview the inspector shows for an unclaimed
/// chunk. The host fills [`ClaimsUiState::cost_quotes`] from the server's
/// pricing model; [`ClaimsUiState::cost_at`] falls back to the
/// starter-free allowance and [`ClaimsUiState::default_cost`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClaimCostPreview {
    /// Inside the starter-free allowance ("first chunk free").
    Free,
    /// Organic points (the `OrganicFluid` economy).
    Points(u64),
    /// Experience levels, for hosts pricing claims in XP.
    XpLevels(u32),
}

/// The six granular per-player permissions the Trusted Players tab
/// edits — the rows of the backend's per-player permission matrix.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClaimPermission {
    /// Break and place blocks.
    Build,
    /// Open chests, barrels, shulkers, hoppers, machines.
    Containers,
    /// Doors, trapdoors, gates, buttons, levers, pressure plates.
    DoorsAndSwitches,
    /// Redstone components inside the claim.
    Redstone,
    /// Right-click/leash/shear/ride entities and vehicles.
    EntityInteract,
    /// Explosion / TNT interactions.
    Explosions,
}

impl ClaimPermission {
    /// All toggles, in display order.
    pub const ALL: [Self; 6] = [
        Self::Build,
        Self::Containers,
        Self::DoorsAndSwitches,
        Self::Redstone,
        Self::EntityInteract,
        Self::Explosions,
    ];

    /// Bilingual toggle label.
    ///
    /// Japanese labels avoid `建築`/`爆発` — those kanji have no glyph in
    /// the built-in `font_cjk` table yet, so the katakana/hiragana forms
    /// are used to keep the screen tofu-free (pinned by tests).
    pub fn label(self, lang: UiLang) -> &'static str {
        match self {
            Self::Build => tr(lang, "Build", "ビルド"),
            Self::Containers => tr(lang, "Containers", "チェスト"),
            Self::DoorsAndSwitches => tr(lang, "Doors & Switches", "ドア・ボタン"),
            Self::Redstone => tr(lang, "Redstone", "レッドストーン"),
            Self::EntityInteract => tr(lang, "Entity Interact", "モブ操作"),
            Self::Explosions => tr(lang, "Explosions / TNT", "ばくはつ・TNT"),
        }
    }
}

/// One player's current six-toggle mask (what the host last synced).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TrustPermissions {
    pub build: bool,
    pub containers: bool,
    pub doors_and_switches: bool,
    pub redstone: bool,
    pub entity_interact: bool,
    pub explosions: bool,
}

impl TrustPermissions {
    /// All six toggles allowed.
    pub fn all_allowed() -> Self {
        Self {
            build: true,
            containers: true,
            doors_and_switches: true,
            redstone: true,
            entity_interact: true,
            explosions: true,
        }
    }

    /// `true` when `perm` is allowed.
    pub fn allows(&self, perm: ClaimPermission) -> bool {
        match perm {
            ClaimPermission::Build => self.build,
            ClaimPermission::Containers => self.containers,
            ClaimPermission::DoorsAndSwitches => self.doors_and_switches,
            ClaimPermission::Redstone => self.redstone,
            ClaimPermission::EntityInteract => self.entity_interact,
            ClaimPermission::Explosions => self.explosions,
        }
    }

    /// `true` when at least one toggle is allowed.
    pub fn any(&self) -> bool {
        ClaimPermission::ALL.iter().any(|p| self.allows(*p))
    }
}

/// One row of the backend's `known_players` roster: a player the world
/// has seen, with the permissions this claim's owner granted them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlayerTrustEntry {
    /// Canonical player UUID string.
    pub uuid: String,
    /// Last seen display name.
    pub name: String,
    /// Currently online (a dim "online" badge is drawn).
    pub online: bool,
    /// The six per-player toggles.
    pub permissions: TrustPermissions,
}

impl PlayerTrustEntry {
    pub fn new(uuid: &str, name: &str) -> Self {
        Self {
            uuid: uuid.to_owned(),
            name: name.to_owned(),
            online: false,
            permissions: TrustPermissions::default(),
        }
    }
}

/// Top-level tab.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ClaimsTab {
    /// The 9×9 interactive claim map.
    #[default]
    Map,
    /// The per-player permission editor.
    Trusted,
}

impl ClaimsTab {
    /// `Map -> Trusted -> Map`.
    pub fn next(self) -> Self {
        match self {
            Self::Map => Self::Trusted,
            Self::Trusted => Self::Map,
        }
    }

    fn label(self, lang: UiLang) -> &'static str {
        match self {
            Self::Map => tr(lang, "Chunk Map", "マップ"),
            // `権限管理` would draw tofu (権 has no built-in glyph), so the
            // covered `プレイヤー許可` is used — pinned by the zero-tofu test.
            Self::Trusted => tr(lang, "Trusted Players", "プレイヤー許可"),
        }
    }
}

/// Every interactive element on the screen.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClaimsWidgetId {
    TabMap,
    TabTrusted,
    /// A map cell; payload is the row-major grid index `0..81`
    /// (row 0 = north / lowest `cz`).
    ChunkCell(u8),
    /// Inspector button: claim the focused chunk.
    ClaimFocused,
    /// Inspector button: unclaim the focused chunk.
    UnclaimFocused,
    SearchField,
    SearchClear,
    /// A player-list row; payload indexes `visible_players()`.
    PlayerRow(usize),
    /// One of the six permission toggles; payload indexes
    /// [`ClaimPermission::ALL`].
    PermToggle(usize),
    /// Revoke every permission of the selected player.
    RevokeSelected,
    PlayerScrollUp,
    PlayerScrollDown,
}

/// A clickable rectangle emitted alongside the draw list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ClaimsHitRegion {
    pub rect: UiRect,
    pub id: ClaimsWidgetId,
}

impl ClaimsHitRegion {
    /// Point containment test (right/bottom exclusive).
    pub const fn contains(self, x: i32, y: i32) -> bool {
        self.rect.contains(x, y)
    }
}

/// What the embedding application must perform after an interaction.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ClaimsUiAction {
    /// Claim chunk `(cx, cz)` for the local player.
    ClaimChunk { cx: i32, cz: i32 },
    /// Release the local player's claim on `(cx, cz)`.
    UnclaimChunk { cx: i32, cz: i32 },
    /// Set one granular permission for `player_uuid`.
    SetPlayerPermission {
        player_uuid: String,
        permission: ClaimPermission,
        allowed: bool,
    },
    /// Drop every permission granted to `player_uuid`.
    RevokePlayer { player_uuid: String },
    /// Leave the screen (Escape / gamepad East with nothing to undo).
    Close,
}

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

/// Interactive state of the claims screen.
#[derive(Clone, Debug)]
pub struct ClaimsUiState {
    /// UI language for chrome labels.
    pub lang: UiLang,
    /// `0` = auto (`height/270`, clamped `1..=4`), else fixed GUI scale.
    pub ui_scale: u8,
    /// Dimension id the map shows (e.g. `"minecraft:overworld"`).
    pub dimension: String,
    /// The player's current chunk — the grid's center cell.
    pub center: (i32, i32),
    /// Chunks the player already owns (the `Claims Used: N` header).
    pub claims_used: usize,
    /// Claim quota shown as `N / Max`.
    pub claims_max: usize,
    /// Starter-free allowance: while `claims_used` is below it the next
    /// claim costs nothing (mirrors `OrganicClaimPolicy`).
    pub starter_free_chunks: usize,
    /// Cost shown for unquoted unclaimed chunks once the freebie is used.
    pub default_cost: ClaimCostPreview,
    /// Host-supplied per-chunk price previews (unquoted cells fall back
    /// to [`cost_at`](Self::cost_at)'s freebie/default logic).
    pub cost_quotes: BTreeMap<(i32, i32), ClaimCostPreview>,
    /// Host-synced claim data keyed by absolute `(cx, cz)`; absent cells
    /// are unclaimed.
    pub chunks: BTreeMap<(i32, i32), ClaimChunkCell>,
    /// The backend `known_players` roster.
    pub known_players: Vec<PlayerTrustEntry>,
    /// Active tab.
    pub tab: ClaimsTab,
    /// Live player filter text.
    pub search: String,
    /// Whether the search box owns the caret highlight.
    pub search_focused: bool,
    /// Keyboard/gamepad cursor on the map (absolute chunk coords,
    /// clamped inside the grid).
    pub cursor: (i32, i32),
    /// Widget under the mouse.
    pub hover: Option<ClaimsWidgetId>,
    /// Chunk the mouse is over — the inspector prefers it over `cursor`.
    pub hover_chunk: Option<(i32, i32)>,
    /// Selection as an index into [`visible_players`](Self::visible_players).
    pub selected_player: usize,
    /// Permission row the keyboard/gamepad cursor is on (`0..6`).
    pub perm_selected: usize,
    /// First painted player-list row.
    pub player_scroll: usize,
}

impl Default for ClaimsUiState {
    fn default() -> Self {
        Self {
            lang: UiLang::En,
            ui_scale: 0,
            dimension: "minecraft:overworld".to_owned(),
            center: (0, 0),
            claims_used: 0,
            claims_max: 500,
            starter_free_chunks: 1,
            default_cost: ClaimCostPreview::Points(100),
            cost_quotes: BTreeMap::new(),
            chunks: BTreeMap::new(),
            known_players: Vec::new(),
            tab: ClaimsTab::Map,
            search: String::new(),
            search_focused: false,
            cursor: (0, 0),
            hover: None,
            hover_chunk: None,
            selected_player: 0,
            perm_selected: 0,
            player_scroll: 0,
        }
    }
}

impl ClaimsUiState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Effective GUI scale for `height` px tall output (`1..=4`).
    pub fn effective_scale(&self, height: u32) -> u32 {
        if self.ui_scale == 0 {
            (height / 270).clamp(1, 4)
        } else {
            u32::from(self.ui_scale.clamp(1, 4))
        }
    }

    /// Recenter the map on the player's current chunk and reseat the
    /// keyboard cursor there.
    pub fn set_center(&mut self, cx: i32, cz: i32) {
        self.center = (cx, cz);
        self.cursor = (cx, cz);
    }

    /// `true` when `(cx, cz)` is inside the visible grid.
    pub fn in_grid(&self, cx: i32, cz: i32) -> bool {
        (i64::from(cx) - i64::from(self.center.0)).abs() <= i64::from(CLAIMS_GRID_RADIUS)
            && (i64::from(cz) - i64::from(self.center.1)).abs() <= i64::from(CLAIMS_GRID_RADIUS)
    }

    /// Row-major grid index of `(cx, cz)` — row 0 is the northern
    /// (`cz - R`) edge. `None` outside the grid.
    pub fn grid_index(&self, cx: i32, cz: i32) -> Option<u8> {
        if !self.in_grid(cx, cz) {
            return None;
        }
        let gx = (i64::from(cx) - (i64::from(self.center.0) - i64::from(CLAIMS_GRID_RADIUS))) as u8;
        let gz = (i64::from(cz) - (i64::from(self.center.1) - i64::from(CLAIMS_GRID_RADIUS))) as u8;
        Some(gz * CLAIMS_GRID_DIM as u8 + gx)
    }

    /// Absolute chunk coords of grid index `i` (`0..81`), `None` when out
    /// of range.
    pub fn grid_coord(&self, i: u8) -> Option<(i32, i32)> {
        let i = i as usize;
        if i >= CLAIMS_GRID_DIM * CLAIMS_GRID_DIM {
            return None;
        }
        Some((
            self.center
                .0
                .saturating_sub(CLAIMS_GRID_RADIUS)
                .saturating_add((i % CLAIMS_GRID_DIM) as i32),
            self.center
                .1
                .saturating_sub(CLAIMS_GRID_RADIUS)
                .saturating_add((i / CLAIMS_GRID_DIM) as i32),
        ))
    }

    /// Synced claim data for `(cx, cz)`, if any.
    pub fn cell(&self, cx: i32, cz: i32) -> Option<&ClaimChunkCell> {
        self.chunks.get(&(cx, cz))
    }

    /// Ownership state of `(cx, cz)` — [`ChunkClaimState::Unclaimed`]
    /// when nothing is synced.
    pub fn state_at(&self, cx: i32, cz: i32) -> ChunkClaimState {
        self.cell(cx, cz)
            .map(|c| c.state.clone())
            .unwrap_or_default()
    }

    /// `true` while the next claim is inside the starter-free allowance.
    pub fn next_claim_free(&self) -> bool {
        self.claims_used < self.starter_free_chunks
    }

    /// The price preview shown for an unclaimed chunk: an explicit
    /// `cost_quotes` entry wins, then the starter-freebie, then
    /// `default_cost`.
    pub fn cost_at(&self, cx: i32, cz: i32) -> ClaimCostPreview {
        if let Some(q) = self.cost_quotes.get(&(cx, cz)) {
            return *q;
        }
        if self.next_claim_free() {
            ClaimCostPreview::Free
        } else {
            self.default_cost
        }
    }

    /// Cost line for the inspector (e.g. `"FREE!"`, `"225 pt"`,
    /// `"XP 3"`).
    pub fn cost_label(&self, cost: ClaimCostPreview, lang: UiLang) -> String {
        match cost {
            ClaimCostPreview::Free => tr(lang, "FREE!", "無料！").to_owned(),
            ClaimCostPreview::Points(p) => {
                format!("{} {}", p, tr(lang, "pt", "ポイント"))
            }
            ClaimCostPreview::XpLevels(l) => format!("XP {}", l),
        }
    }

    /// The chunk the inspector describes: the hovered cell when the mouse
    /// is over the map, else the keyboard cursor.
    pub fn focus_chunk(&self) -> (i32, i32) {
        self.hover_chunk.unwrap_or(self.cursor)
    }

    /// Indices into `known_players` passing the search filter, sorted by
    /// lowercase name then uuid for a deterministic roster order.
    pub fn visible_players(&self) -> Vec<usize> {
        let needle = self.search.to_lowercase();
        let mut out: Vec<usize> = self
            .known_players
            .iter()
            .enumerate()
            .filter(|(_, p)| {
                needle.is_empty()
                    || p.name.to_lowercase().contains(&needle)
                    || p.uuid.to_lowercase().contains(&needle)
            })
            .map(|(i, _)| i)
            .collect();
        out.sort_by(|&a, &b| {
            self.known_players[a]
                .name
                .to_lowercase()
                .cmp(&self.known_players[b].name.to_lowercase())
                .then_with(|| self.known_players[a].uuid.cmp(&self.known_players[b].uuid))
        });
        out
    }

    /// The currently selected roster entry, if any survive the filter.
    pub fn selected_player_entry(&self) -> Option<&PlayerTrustEntry> {
        let vis = self.visible_players();
        vis.get(self.selected_player)
            .map(|&i| &self.known_players[i])
    }

    fn clamp_selection(&mut self) {
        let len = self.visible_players().len();
        if len == 0 {
            self.selected_player = 0;
        } else {
            self.selected_player = self.selected_player.min(len - 1);
        }
        self.perm_selected = self.perm_selected.min(ClaimPermission::ALL.len() - 1);
    }

    // -- input ---------------------------------------------------------

    /// The widget under `(x, y)` in a `width`×`height` frame, if any.
    pub fn widget_at(&self, width: u32, height: u32, x: i32, y: i32) -> Option<ClaimsWidgetId> {
        self.render(width, height)
            .1
            .iter()
            .rev()
            .find(|h| h.rect.contains(x, y))
            .map(|h| h.id)
    }

    fn chunk_for_widget(&self, id: ClaimsWidgetId) -> Option<(i32, i32)> {
        match id {
            ClaimsWidgetId::ChunkCell(i) => self.grid_coord(i),
            _ => None,
        }
    }

    /// Primary (left) click at physical `(x, y)`.
    ///
    /// * Map cells: unclaimed emits [`ClaimsUiAction::ClaimChunk`];
    ///   anything else just focuses the chunk for the inspector.
    /// * Everything else activates the widget under the cursor.
    pub fn primary_click_at(&mut self, x: i32, y: i32, w: u32, h: u32) -> Vec<ClaimsUiAction> {
        let id = self.widget_at(w, h, x, y);
        if id != Some(ClaimsWidgetId::SearchField) {
            self.search_focused = false;
        }
        match id {
            Some(id) => self.activate(id),
            None => Vec::new(),
        }
    }

    /// Secondary (right) click at physical `(x, y)` — the mouse path to
    /// unclaiming, since [`UiInputEvent`] has no right-button variant:
    /// over a self-owned cell it emits [`ClaimsUiAction::UnclaimChunk`];
    /// over any other cell it only focuses the inspector.
    pub fn secondary_click_at(&mut self, x: i32, y: i32, w: u32, h: u32) -> Vec<ClaimsUiAction> {
        let Some(ClaimsWidgetId::ChunkCell(i)) = self.widget_at(w, h, x, y) else {
            return Vec::new();
        };
        let Some((cx, cz)) = self.grid_coord(i) else {
            return Vec::new();
        };
        self.cursor = (cx, cz);
        self.hover_chunk = Some((cx, cz));
        if self.state_at(cx, cz) == ChunkClaimState::OwnedBySelf {
            return vec![ClaimsUiAction::UnclaimChunk { cx, cz }];
        }
        Vec::new()
    }

    /// Activate a widget by id (click, `Enter`, or gamepad `South`).
    pub fn activate(&mut self, id: ClaimsWidgetId) -> Vec<ClaimsUiAction> {
        match id {
            ClaimsWidgetId::TabMap => {
                self.tab = ClaimsTab::Map;
            }
            ClaimsWidgetId::TabTrusted => {
                self.tab = ClaimsTab::Trusted;
                self.clamp_selection();
            }
            ClaimsWidgetId::ChunkCell(i) => {
                if let Some((cx, cz)) = self.grid_coord(i) {
                    self.cursor = (cx, cz);
                    self.hover_chunk = Some((cx, cz));
                    if self.state_at(cx, cz) == ChunkClaimState::Unclaimed {
                        return vec![ClaimsUiAction::ClaimChunk { cx, cz }];
                    }
                }
            }
            ClaimsWidgetId::ClaimFocused => {
                let (cx, cz) = self.focus_chunk();
                if self.state_at(cx, cz) == ChunkClaimState::Unclaimed {
                    return vec![ClaimsUiAction::ClaimChunk { cx, cz }];
                }
            }
            ClaimsWidgetId::UnclaimFocused => {
                let (cx, cz) = self.focus_chunk();
                if self.state_at(cx, cz) == ChunkClaimState::OwnedBySelf {
                    return vec![ClaimsUiAction::UnclaimChunk { cx, cz }];
                }
            }
            ClaimsWidgetId::SearchField => self.search_focused = true,
            ClaimsWidgetId::SearchClear => {
                self.search.clear();
                self.search_focused = true;
                self.player_scroll = 0;
                self.clamp_selection();
            }
            ClaimsWidgetId::PlayerRow(vi) => self.selected_player = vi,
            ClaimsWidgetId::PermToggle(i) => {
                let Some(&perm) = ClaimPermission::ALL.get(i) else {
                    return Vec::new();
                };
                let Some(entry) = self.selected_player_entry() else {
                    return Vec::new();
                };
                return vec![ClaimsUiAction::SetPlayerPermission {
                    player_uuid: entry.uuid.clone(),
                    permission: perm,
                    allowed: !entry.permissions.allows(perm),
                }];
            }
            ClaimsWidgetId::RevokeSelected => {
                if let Some(entry) = self.selected_player_entry() {
                    return vec![ClaimsUiAction::RevokePlayer {
                        player_uuid: entry.uuid.clone(),
                    }];
                }
            }
            ClaimsWidgetId::PlayerScrollUp => {
                self.player_scroll = self.player_scroll.saturating_sub(1);
            }
            ClaimsWidgetId::PlayerScrollDown => {
                self.player_scroll =
                    (self.player_scroll + 1).min(self.visible_players().len().saturating_sub(1));
            }
        }
        Vec::new()
    }

    /// Route one device-agnostic input event; returns the actions the
    /// host must perform. `width`/`height` are the frame's physical
    /// pixels for pointer hit-testing.
    pub fn handle_input(
        &mut self,
        ev: UiInputEvent,
        width: u32,
        height: u32,
    ) -> Vec<ClaimsUiAction> {
        match ev {
            UiInputEvent::MouseMove { x, y } => {
                self.hover = self.widget_at(width, height, x, y);
                self.hover_chunk = self.hover.and_then(|id| self.chunk_for_widget(id));
                Vec::new()
            }
            UiInputEvent::Click { x, y } => self.primary_click_at(x, y, width, height),
            UiInputEvent::KeyPressed(key) => self.handle_key(key, width, height),
            UiInputEvent::CharTyped(c) => {
                if self.tab == ClaimsTab::Trusted && (' '..='~').contains(&c) {
                    self.search_focused = true;
                    self.search.push(c);
                    self.player_scroll = 0;
                    self.clamp_selection();
                }
                Vec::new()
            }
            UiInputEvent::GamepadPressed(btn) => match btn {
                GamepadButton::DpadUp => self.handle_key(UiKey::Up, width, height),
                GamepadButton::DpadDown => self.handle_key(UiKey::Down, width, height),
                GamepadButton::DpadLeft => self.handle_key(UiKey::Left, width, height),
                GamepadButton::DpadRight => self.handle_key(UiKey::Right, width, height),
                GamepadButton::South => self.handle_key(UiKey::Enter, width, height),
                GamepadButton::East | GamepadButton::Start => self.escape(),
            },
        }
    }

    fn escape(&mut self) -> Vec<ClaimsUiAction> {
        if self.tab == ClaimsTab::Trusted && !self.search.is_empty() {
            self.search.clear();
            self.player_scroll = 0;
            self.clamp_selection();
            return Vec::new();
        }
        vec![ClaimsUiAction::Close]
    }

    /// Unclaim the given chunk when it is self-owned (keyboard/right-
    /// click path); `None` becomes a no-op.
    fn try_unclaim(&self, chunk: (i32, i32)) -> Vec<ClaimsUiAction> {
        let (cx, cz) = chunk;
        if self.state_at(cx, cz) == ChunkClaimState::OwnedBySelf {
            return vec![ClaimsUiAction::UnclaimChunk { cx, cz }];
        }
        Vec::new()
    }

    /// Claim the given chunk when unclaimed (keyboard path).
    fn try_claim(&self, chunk: (i32, i32)) -> Vec<ClaimsUiAction> {
        let (cx, cz) = chunk;
        if self.state_at(cx, cz) == ChunkClaimState::Unclaimed {
            return vec![ClaimsUiAction::ClaimChunk { cx, cz }];
        }
        Vec::new()
    }

    fn handle_key(&mut self, key: UiKey, _width: u32, _height: u32) -> Vec<ClaimsUiAction> {
        match key {
            UiKey::Tab => {
                self.tab = self.tab.next();
                self.clamp_selection();
            }
            UiKey::Up | UiKey::Down | UiKey::Left | UiKey::Right => match self.tab {
                ClaimsTab::Map => {
                    let (dx, dz) = match key {
                        UiKey::Up => (0, -1),
                        UiKey::Down => (0, 1),
                        UiKey::Left => (-1, 0),
                        _ => (1, 0),
                    };
                    let nx = self.cursor.0.saturating_add(dx).clamp(
                        self.center.0.saturating_sub(CLAIMS_GRID_RADIUS),
                        self.center.0.saturating_add(CLAIMS_GRID_RADIUS),
                    );
                    let nz = self.cursor.1.saturating_add(dz).clamp(
                        self.center.1.saturating_sub(CLAIMS_GRID_RADIUS),
                        self.center.1.saturating_add(CLAIMS_GRID_RADIUS),
                    );
                    self.cursor = (nx, nz);
                    self.hover_chunk = Some((nx, nz));
                }
                ClaimsTab::Trusted => {
                    let len = self.visible_players().len() as i64;
                    match key {
                        UiKey::Up | UiKey::Down => {
                            let dir: i64 = if key == UiKey::Up { -1 } else { 1 };
                            if len > 0 {
                                self.selected_player =
                                    (self.selected_player as i64 + dir).clamp(0, len - 1) as usize;
                            }
                        }
                        UiKey::Left | UiKey::Right => {
                            let dir: i64 = if key == UiKey::Left { -1 } else { 1 };
                            let max = ClaimPermission::ALL.len() as i64 - 1;
                            self.perm_selected =
                                (self.perm_selected as i64 + dir).clamp(0, max) as usize;
                        }
                        _ => {}
                    }
                }
            },
            UiKey::Enter | UiKey::Space => match self.tab {
                ClaimsTab::Map => return self.try_claim(self.cursor),
                ClaimsTab::Trusted => {
                    return self.activate(ClaimsWidgetId::PermToggle(self.perm_selected));
                }
            },
            UiKey::Backspace => match self.tab {
                ClaimsTab::Map => return self.try_unclaim(self.cursor),
                ClaimsTab::Trusted => {
                    if self.search.pop().is_some() {
                        self.player_scroll = 0;
                        self.clamp_selection();
                    } else if let Some(entry) = self.selected_player_entry() {
                        return vec![ClaimsUiAction::RevokePlayer {
                            player_uuid: entry.uuid.clone(),
                        }];
                    }
                }
            },
            UiKey::Escape => return self.escape(),
        }
        Vec::new()
    }

    // -- rendering ------------------------------------------------------

    /// Build the frame: `(draw_list, hit_regions)`. Everything is
    /// computed in logical pixels and multiplied by
    /// [`effective_scale`](Self::effective_scale) on emit, so the same
    /// layout serves `640×360` through `3840×2160` and every hit region
    /// is pixel-identical to its widget's visuals.
    pub fn render(&self, width: u32, height: u32) -> (UiDrawList, Vec<ClaimsHitRegion>) {
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
        self.render_tabs(&mut lay, &mut hits);
        match self.tab {
            ClaimsTab::Map => self.render_map(&mut lay, &mut hits),
            ClaimsTab::Trusted => self.render_trusted(&mut lay, &mut hits),
        }
        self.render_footer(&mut lay);
        (lay.finish(), hits)
    }

    fn render_header(&self, lay: &mut Lay) {
        let lang = self.lang;
        lay.text_fit(
            MARGIN + 6,
            MARGIN + 4,
            1,
            palette::TEXT,
            tr(lang, "Chunk Claims", "クレーム管理"),
            ((lay.w - 2 * MARGIN - 12) / 8).max(1) as usize,
        );

        // Claims-used counter, right-aligned on the title row.
        let used = format!(
            "{} {} / {}",
            tr(lang, "Claims Used:", "クレーム:"),
            self.claims_used,
            self.claims_max
        );
        let ux = (lay.w - MARGIN - 6 - text_cells(&used) as i32 * 8).max(MARGIN + 6);
        lay.text_fit(ux, MARGIN + 4, 1, palette::TEXT_DIM, &used, 30);

        // Second header row: starter-free badge + dimension chip.
        if self.next_claim_free() {
            let badge = tr(lang, "(first chunk free!)", "(初回無料!)");
            lay.text_fit(MARGIN + 6, MARGIN + 14, 1, palette::XP, badge, 26);
        }
        let dim = self
            .dimension
            .rsplit(':')
            .next()
            .unwrap_or(self.dimension.as_str());
        let chip_w = (text_cells(dim).max(3) as i32 * 8 + 8).min(lay.w / 3);
        let chip = lay.rect(lay.w - MARGIN - 6 - chip_w, MARGIN + 12, chip_w, 12);
        lay.fill_rect(chip, palette::SLOT);
        lay.border_rect(chip, palette::BUTTON_BORDER);
        lay.ctext_in(chip, 1, palette::AIR, dim);
    }

    fn render_tabs(&self, lay: &mut Lay, hits: &mut Vec<ClaimsHitRegion>) {
        let lang = self.lang;
        let mut x = MARGIN + 6;
        for (tab, id) in [
            (ClaimsTab::Map, ClaimsWidgetId::TabMap),
            (ClaimsTab::Trusted, ClaimsWidgetId::TabTrusted),
        ] {
            let label = tab.label(lang);
            let w = (text_cells(label) as i32 * 8 + 16).max(56);
            let r = lay.rect(x, TABS_Y, w, 13);
            let active = self.tab == tab;
            lay.fill_rect(
                r,
                if active {
                    palette::BUTTON_HOVER
                } else {
                    palette::BUTTON
                },
            );
            lay.border_rect(
                r,
                if active {
                    palette::BUTTON_BORDER_HOVER
                } else {
                    palette::BUTTON_BORDER
                },
            );
            lay.ctext_in(r, 1, palette::TEXT, label);
            lay.region(hits, id, r);
            x += w + 4;
        }
    }

    /// The map grid's top-left corner and cell pitch in logical pixels.
    /// The grid is vertically centered in the content band; the pitch
    /// shrinks so 9 cells always fit inside it.
    fn grid_origin(&self, lay: &Lay) -> (i32, i32, i32) {
        let band_top = CONTENT_TOP + COMPASS_H;
        let band_bottom = lay.h - FOOTER_H - COMPASS_H;
        let pitch = ((band_bottom - band_top) / CLAIMS_GRID_DIM as i32).clamp(10, GRID_PITCH);
        let grid_px = pitch * CLAIMS_GRID_DIM as i32;
        let y0 = band_top + ((band_bottom - band_top - grid_px) / 2).max(0);
        (GRID_X0, y0, pitch)
    }

    fn cell_rect(&self, lay: &Lay, i: u8) -> Option<UiRect> {
        let (cx, cz) = self.grid_coord(i)?;
        let (x0, y0, pitch) = self.grid_origin(lay);
        let gx = cx - (self.center.0 - CLAIMS_GRID_RADIUS);
        let gz = cz - (self.center.1 - CLAIMS_GRID_RADIUS);
        Some(lay.rect(
            x0 + gx * pitch + 1,
            y0 + gz * pitch + 1,
            pitch - 2,
            pitch - 2,
        ))
    }

    fn render_map(&self, lay: &mut Lay, hits: &mut Vec<ClaimsHitRegion>) {
        let (x0, y0, pitch) = self.grid_origin(lay);
        let grid_px = pitch * CLAIMS_GRID_DIM as i32;

        // Compass: N/E/S/W hugging the grid edges.
        let compass = palette::TEXT_DIM;
        let mid = x0 + grid_px / 2;
        lay.text_fit(mid - 4, y0 - COMPASS_H + 2, 1, compass, "N", 1);
        lay.text_fit(mid - 4, y0 + grid_px + 3, 1, compass, "S", 1);
        lay.text_fit(x0 - COMPASS_H + 2, y0 + grid_px / 2 - 4, 1, compass, "W", 1);
        lay.text_fit(
            x0 + grid_px + COMPASS_H - 8,
            y0 + grid_px / 2 - 4,
            1,
            compass,
            "E",
            1,
        );

        // Cells (row 0 = north edge).
        for i in 0..(CLAIMS_GRID_DIM * CLAIMS_GRID_DIM) as u8 {
            let Some((cx, cz)) = self.grid_coord(i) else {
                continue;
            };
            let Some(r) = self.cell_rect(lay, i) else {
                continue;
            };
            let state = self.state_at(cx, cz);
            lay.fill_rect(r, state.fill_color());
            let focused = self.cursor == (cx, cz) || self.hover_chunk == Some((cx, cz));
            lay.border_rect(
                r,
                if focused {
                    palette::BUTTON_BORDER_HOVER
                } else {
                    GRID_LINE
                },
            );
            if let Some(cell) = self.cell(cx, cz) {
                if cell.forceloaded {
                    lay.fill(r.right() - 5, r.y + 1, 4, 3, palette::WARNING);
                }
            }
            lay.region(hits, ClaimsWidgetId::ChunkCell(i), r);
        }

        // Player marker on the center cell.
        lay.text_fit(mid - 4, y0 + grid_px / 2 - 4, 1, palette::TEXT, "@", 1);

        self.render_inspector(lay, hits, x0 + grid_px + COMPASS_H + PANE_GAP);
    }

    fn render_inspector(&self, lay: &mut Lay, hits: &mut Vec<ClaimsHitRegion>, pane_x: i32) {
        let lang = self.lang;
        let pane = lay.rect(
            pane_x,
            CONTENT_TOP,
            lay.w - MARGIN - 4 - pane_x,
            lay.h - FOOTER_H - 6 - CONTENT_TOP,
        );
        lay.fill_rect(pane, DETAIL_BG);
        lay.border_rect(pane, palette::PANEL_BORDER);

        let (cx, cz) = self.focus_chunk();
        let state = self.state_at(cx, cz);
        let tx = pane.x + 6;
        let text_w = ((pane.w as i32 - 12) / 8).max(1) as usize;
        let mut y = pane.y + 5;

        lay.text_fit(
            tx,
            y,
            1,
            palette::TEXT,
            &format!("Chunk ({cx}, {cz})"),
            text_w,
        );
        y += 11;

        let owner = match &state {
            ChunkClaimState::Unclaimed => tr(lang, "Owner: -", "オーナー: なし").to_owned(),
            ChunkClaimState::OwnedBySelf => tr(lang, "Owner: You", "オーナー: 自分").to_owned(),
            ChunkClaimState::Trusted => tr(lang, "Owner: trusted", "オーナー: なかま").to_owned(),
            ChunkClaimState::OwnedByOther(name) => {
                format!("{} {}", tr(lang, "Owner:", "オーナー:"), name)
            }
            ChunkClaimState::SpawnProtected => {
                tr(lang, "Owner: Server", "オーナー: サーバー").to_owned()
            }
        };
        lay.text_fit(tx, y, 1, palette::TEXT_DIM, &owner, text_w);
        y += 11;

        // Cost line: only unclaimed chunks have a price.
        if state == ChunkClaimState::Unclaimed {
            let cost = self.cost_at(cx, cz);
            let line = format!(
                "{} {}",
                tr(lang, "Cost:", "コスト:"),
                self.cost_label(cost, lang)
            );
            let color = if cost == ClaimCostPreview::Free {
                palette::XP
            } else {
                palette::WARNING
            };
            lay.text_fit(tx, y, 1, color, &line, text_w);
        } else {
            lay.text_fit(tx, y, 1, palette::TEXT_DIM, "-", text_w);
        }
        y += 11;

        // Active flags.
        if let Some(cell) = self.cell(cx, cz) {
            let mut flags: Vec<String> = Vec::new();
            if cell.forceloaded {
                flags.push(tr(lang, "forceload", "フォースロード").to_owned());
            }
            if let Some(sub) = &cell.sub_claim {
                flags.push(format!("sub:{sub}"));
            }
            if !flags.is_empty() {
                lay.text_fit(tx, y, 1, palette::TEXT_DIM, &flags.join("  "), text_w);
            }
        }
        y += 13;

        // Action buttons.
        let claimable = state == ChunkClaimState::Unclaimed;
        let unclaimable = state == ChunkClaimState::OwnedBySelf;
        let btn_w = ((pane.w as i32 - 18) / 2).clamp(40, 96);
        let claim_r = lay.rect(tx, y, btn_w, 13);
        self.draw_button(lay, claim_r, claimable, tr(lang, "Claim", "クレーム"));
        lay.region(hits, ClaimsWidgetId::ClaimFocused, claim_r);
        let unclaim_r = lay.rect(tx + btn_w + 6, y, btn_w, 13);
        self.draw_button(lay, unclaim_r, unclaimable, tr(lang, "Unclaim", "削除"));
        lay.region(hits, ClaimsWidgetId::UnclaimFocused, unclaim_r);
        y += 17;

        // Per-state hint.
        let hint = match &state {
            ChunkClaimState::Unclaimed => tr(lang, "Click the map to claim.", "マップでクレーム"),
            ChunkClaimState::OwnedBySelf => tr(lang, "Right-click to unclaim.", "右クリックで削除"),
            ChunkClaimState::Trusted => tr(lang, "Party claim.", "なかまのクレーム"),
            ChunkClaimState::OwnedByOther(_) => tr(lang, "Not your claim.", "ほかのクレーム"),
            ChunkClaimState::SpawnProtected => tr(lang, "Spawn protected.", "スポーン"),
        };
        lay.text_fit(tx, y, 1, palette::TEXT_DIM, hint, text_w);

        // Legend pinned to the pane's bottom.
        let mut ly = pane.bottom() - 53;
        if ly < y + 12 {
            ly = y + 12;
        }
        lay.text_fit(
            tx,
            ly,
            1,
            palette::TEXT_DIM,
            tr(lang, "Legend:", "レジェンド:"),
            text_w,
        );
        let swatches = [
            ChunkClaimState::OwnedBySelf,
            ChunkClaimState::Trusted,
            ChunkClaimState::OwnedByOther(String::new()),
            ChunkClaimState::SpawnProtected,
            ChunkClaimState::Unclaimed,
        ];
        for (i, s) in swatches.iter().enumerate() {
            let sy = ly + 9 + i as i32 * 9;
            lay.fill(tx, sy + 1, 7, 7, s.fill_color());
            lay.border(tx, sy + 1, 7, 7, GRID_LINE);
            lay.text_fit(
                tx + 10,
                sy + 1,
                1,
                palette::TEXT_DIM,
                &s.legend_label(lang),
                text_w.saturating_sub(2),
            );
        }
    }

    fn draw_button(&self, lay: &mut Lay, r: UiRect, enabled: bool, label: &str) {
        lay.fill_rect(
            r,
            if enabled {
                palette::BUTTON
            } else {
                palette::BUTTON_DISABLED
            },
        );
        lay.border_rect(r, palette::BUTTON_BORDER);
        lay.ctext_in(
            r,
            1,
            if enabled {
                palette::TEXT
            } else {
                palette::TEXT_DIM
            },
            label,
        );
    }

    /// Rows the player list can show at this frame height.
    fn player_list_rows(&self, lay: &Lay) -> usize {
        ((LIST_BOTTOM_MAX.min(lay.h - FOOTER_H - 8) - LIST_TOP) / PLAYER_ROW_PITCH).max(1) as usize
    }

    fn render_trusted(&self, lay: &mut Lay, hits: &mut Vec<ClaimsHitRegion>) {
        let lang = self.lang;
        let list_right = (lay.w * 3 / 5).min(LIST_MAX_RIGHT);

        // Search field + clear button.
        let clear_w = 16;
        let field = lay.rect(
            MARGIN + 6,
            CONTENT_TOP + 2,
            list_right - MARGIN - 10 - clear_w - 3,
            13,
        );
        lay.fill_rect(
            field,
            if self.search_focused {
                palette::SLOT
            } else {
                palette::BUTTON_DISABLED
            },
        );
        lay.border_rect(
            field,
            if self.search_focused {
                palette::BUTTON_BORDER_HOVER
            } else {
                palette::BUTTON_BORDER
            },
        );
        let prompt = if self.search.is_empty() {
            tr(lang, "search players", "検索…").to_owned()
        } else {
            self.search.clone()
        };
        lay.text_fit(
            field.x + 4,
            field.y + 3,
            1,
            if self.search.is_empty() {
                palette::TEXT_DIM
            } else {
                palette::TEXT
            },
            &prompt,
            ((field.w as i32 - 8) / 8).max(1) as usize,
        );
        lay.region(hits, ClaimsWidgetId::SearchField, field);
        let clear = lay.rect(field.right() + 3, CONTENT_TOP + 2, clear_w, 13);
        lay.fill_rect(clear, palette::BUTTON);
        lay.border_rect(clear, palette::BUTTON_BORDER);
        lay.ctext_in(clear, 1, palette::TEXT_DIM, "x");
        lay.region(hits, ClaimsWidgetId::SearchClear, clear);

        // Player list.
        let rows = self.player_list_rows(lay);
        let list_bottom = LIST_TOP + rows as i32 * PLAYER_ROW_PITCH;
        let vis = self.visible_players();
        let scroll = self.player_scroll.min(vis.len().saturating_sub(rows));
        if vis.is_empty() {
            lay.text_fit(
                MARGIN + 10,
                LIST_TOP + 6,
                1,
                palette::TEXT_DIM,
                tr(lang, "no players", "プレイヤーなし"),
                ((list_right - MARGIN - 20) / 8).max(1) as usize,
            );
        }
        for row in 0..rows {
            let vi = scroll + row;
            let Some(&pi) = vis.get(vi) else { break };
            let entry = &self.known_players[pi];
            let r = lay.rect(
                MARGIN + 6,
                LIST_TOP + row as i32 * PLAYER_ROW_PITCH,
                list_right - MARGIN - 12,
                PLAYER_ROW_H,
            );
            if vi == self.selected_player {
                lay.fill_rect(r, palette::BUTTON_HOVER);
            } else if self.hover == Some(ClaimsWidgetId::PlayerRow(vi)) {
                lay.fill_rect(r, palette::BUTTON);
            }
            lay.border_rect(r, palette::BUTTON_BORDER);
            let granted = ClaimPermission::ALL
                .iter()
                .filter(|p| entry.permissions.allows(**p))
                .count();
            let label = format!(
                "{}{}  {}/6",
                if entry.online { "+" } else { "" },
                entry.name,
                granted
            );
            lay.text_fit(
                r.x + 4,
                r.y + 2,
                1,
                if vi == self.selected_player {
                    palette::TEXT
                } else {
                    palette::TEXT_DIM
                },
                &label,
                ((r.w as i32 - 8) / 8).max(1) as usize,
            );
            lay.region(hits, ClaimsWidgetId::PlayerRow(vi), r);
        }

        // List scroll arrows on the pane's right edge — parked exactly at
        // the rows' right edge so the hit regions can never overlap.
        let up = lay.rect(list_right - 6, LIST_TOP, 12, 12);
        lay.fill_rect(up, palette::BUTTON);
        lay.border_rect(up, palette::BUTTON_BORDER);
        lay.ctext_in(up, 1, palette::TEXT_DIM, "^");
        lay.region(hits, ClaimsWidgetId::PlayerScrollUp, up);
        let down = lay.rect(list_right - 6, list_bottom - 12, 12, 12);
        lay.fill_rect(down, palette::BUTTON);
        lay.border_rect(down, palette::BUTTON_BORDER);
        lay.ctext_in(down, 1, palette::TEXT_DIM, "v");
        lay.region(hits, ClaimsWidgetId::PlayerScrollDown, down);

        self.render_perm_panel(lay, hits, list_right + PANE_GAP);
    }

    fn render_perm_panel(&self, lay: &mut Lay, hits: &mut Vec<ClaimsHitRegion>, pane_x: i32) {
        let lang = self.lang;
        let pane = lay.rect(
            pane_x,
            CONTENT_TOP,
            lay.w - MARGIN - 4 - pane_x,
            lay.h - FOOTER_H - 6 - CONTENT_TOP,
        );
        lay.fill_rect(pane, DETAIL_BG);
        lay.border_rect(pane, palette::PANEL_BORDER);
        let tx = pane.x + 6;
        let text_w = ((pane.w as i32 - 12) / 8).max(1) as usize;
        let mut y = pane.y + 5;

        match self.selected_player_entry() {
            Some(entry) => {
                let name = format!(
                    "{}{}",
                    if entry.online {
                        tr(lang, "[online] ", "[オン] ")
                    } else {
                        ""
                    },
                    entry.name
                );
                lay.text_fit(tx, y, 1, palette::TEXT, &name, text_w);
                y += 10;
                lay.text_fit(tx, y, 1, palette::TEXT_DIM, &entry.uuid, text_w);
                y += 14;

                for i in 0..ClaimPermission::ALL.len() {
                    let perm = ClaimPermission::ALL[i];
                    let allowed = entry.permissions.allows(perm);
                    let r = lay.rect(tx - 2, y, pane.w as i32 - 10, PERM_ROW_H);
                    if i == self.perm_selected {
                        lay.border_rect(r, palette::BUTTON_BORDER_HOVER);
                    }
                    let checkbox = lay.rect(tx + 2, y + 2, 9, 9);
                    lay.fill_rect(
                        checkbox,
                        if allowed {
                            palette::ACCENT
                        } else {
                            palette::BUTTON_DISABLED
                        },
                    );
                    lay.border_rect(checkbox, palette::BUTTON_BORDER);
                    if allowed {
                        lay.text_fit(
                            checkbox.x + 1,
                            checkbox.y + 1,
                            1,
                            palette::TEXT_DARK,
                            "x",
                            1,
                        );
                    }
                    lay.text_fit(
                        tx + 15,
                        y + 2,
                        1,
                        palette::TEXT,
                        perm.label(lang),
                        (text_w.saturating_sub(2)).max(1),
                    );
                    let state_txt = tr(lang, "ON", "オン");
                    let state_txt = if allowed {
                        state_txt
                    } else {
                        tr(lang, "OFF", "オフ")
                    };
                    let sx = (pane.right() - 6 - text_cells(state_txt) as i32 * 8).max(tx + 15);
                    lay.text_fit(
                        sx,
                        y + 2,
                        1,
                        if allowed {
                            palette::ACCENT
                        } else {
                            palette::TEXT_DIM
                        },
                        state_txt,
                        6,
                    );
                    lay.region(hits, ClaimsWidgetId::PermToggle(i), r);
                    y += PERM_ROW_PITCH;
                }

                y += 6;
                let revoke_w = (pane.w as i32 - 12).clamp(60, 220);
                let revoke = lay.rect(tx, y, revoke_w, 13);
                lay.fill_rect(revoke, palette::DANGER.with_alpha(160));
                lay.border_rect(revoke, palette::BUTTON_BORDER);
                lay.ctext_in(
                    revoke,
                    1,
                    palette::TEXT,
                    tr(lang, "Revoke all permissions", "すべての許可を削除"),
                );
                lay.region(hits, ClaimsWidgetId::RevokeSelected, revoke);
            }
            None => {
                lay.text_fit(
                    tx,
                    y,
                    1,
                    palette::TEXT_DIM,
                    tr(lang, "Select a player.", "プレイヤーを選択"),
                    text_w,
                );
            }
        }
    }

    fn render_footer(&self, lay: &mut Lay) {
        let lang = self.lang;
        let hint = match self.tab {
            ClaimsTab::Map => tr(
                lang,
                "LMB claim / RMB unclaim - Tab: players - Esc: close",
                "左:クレーム 右:削除 Tab:切替 Esc:戻る",
            ),
            ClaimsTab::Trusted => tr(
                lang,
                "Type to filter - Enter toggles - Backspace(empty): revoke - Tab: map",
                "文字で検索 Enter:切替 Backspace:削除 Tab:マップ",
            ),
        };
        lay.text_fit(
            MARGIN + 6,
            lay.h - MARGIN - 12,
            1,
            palette::TEXT_DIM,
            hint,
            ((lay.w - 2 * MARGIN - 12) / 8).max(1) as usize,
        );
    }
}

// ---------------------------------------------------------------------------
// Layout context (logical px -> physical px at the effective GUI scale)
// ---------------------------------------------------------------------------

const MARGIN: i32 = 4;
const TABS_Y: i32 = 30;
const CONTENT_TOP: i32 = 46;
const FOOTER_H: i32 = 14;
const COMPASS_H: i32 = 12;
const PANE_GAP: i32 = 8;
const GRID_X0: i32 = 18;
const GRID_PITCH: i32 = 19;
const LIST_TOP: i32 = 66;
const LIST_MAX_RIGHT: i32 = 296;
const LIST_BOTTOM_MAX: i32 = 336;
const PLAYER_ROW_H: i32 = 11;
const PLAYER_ROW_PITCH: i32 = 13;
const PERM_ROW_H: i32 = 12;
const PERM_ROW_PITCH: i32 = 15;

/// Grid cell outline: a hair darker than the panel so unclaimed cells
/// still read as slots.
const GRID_LINE: UiColor = UiColor::rgb(16, 17, 22);

/// Right/detail pane fill: a hair lighter than the main panel.
const DETAIL_BG: UiColor = UiColor::rgba(26, 29, 38, 235);

/// Scales logical-pixel geometry to physical pixels on emit so visuals
/// and hit regions never diverge.
struct Lay {
    list: UiDrawList,
    w: i32,
    h: i32,
    s: i32,
}

impl Lay {
    fn new(state: &ClaimsUiState, width: u32, height: u32) -> Self {
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
    /// Labels are elided to the rect first so they never spill past the
    /// widget's edges.
    fn ctext_in(&mut self, r: UiRect, mult: u32, color: UiColor, text: &str) {
        let text = fit_text(text, ((r.w as i32 - 4) / (8 * mult as i32)).max(0) as usize);
        let scale = (mult as i32 * self.s).max(1) as u32;
        let w = crate::draw::text_width(&text, scale) as i32;
        let cx = r.x + r.w as i32 / 2;
        let ty = r.y + ((r.h as i32 - 8 * mult as i32) / 2).max(0);
        self.list
            .text_str(cx * self.s - w / 2, ty * self.s, scale, color, &text);
    }

    fn region(&self, hits: &mut Vec<ClaimsHitRegion>, id: ClaimsWidgetId, r: UiRect) {
        let p = self.phys(r);
        let (x0, y0) = (p.x.max(0), p.y.max(0));
        let (x1, y1) = (
            p.right().min(self.w * self.s),
            p.bottom().min(self.h * self.s),
        );
        if x1 <= x0 || y1 <= y0 {
            return;
        }
        hits.push(ClaimsHitRegion {
            rect: UiRect::new(x0, y0, (x1 - x0) as u32, (y1 - y0) as u32),
            id,
        });
    }
}

/// Elide a label to `cells` glyph cells so text never overflows.
/// Control characters are flattened first — a hostile name must not
/// wrap its widget into a second line.
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
