// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! Tiny-annoyance QoL helpers — the pure-logic core behind the
//! `Settings -> QoL` toggles.
//!
//! Everything here is `std`-only and deterministic (no clock reads — the
//! host feeds `dt_ms`): OS system-language auto-detect, the opt-in
//! auto-reconnect state machine with anti-spam/fair-play guardrails, the
//! bounded toast-notification stack, Shulker/Bundle tooltip peek
//! summaries, food-value previews, keybind conflict
//! detection, auto-replenish / quick-stack-to-chest
//! quick-stack / container-sort planners, and the chat scroll-lock
//! state.

use crate::state::UiLang;

// ---------------------------------------------------------------------------
// OS system language auto-detect
// ---------------------------------------------------------------------------

/// Parse one locale string (BCP-47 `ja-JP`, POSIX `ja_JP.UTF-8`, or
/// Windows `Japanese_Japan.932`) into the coarse UI language: Japanese
/// maps to [`UiLang::Ja`], everything else to [`UiLang::En`].
pub fn parse_system_locale(raw: &str) -> UiLang {
    let trimmed = raw.trim();
    // LANGUAGE is a colon-separated gettext priority list; honor the
    // first entry only so `ja:en_US` still detects Japanese.
    let first = trimmed.split(':').next().unwrap_or("");
    // Strip the POSIX codeset (`ja_JP.UTF-8`) and modifier (`@cjknarrow`).
    let base = first.split(['.', '@']).next().unwrap_or("");
    let lang = base.split(['_', '-']).next().unwrap_or("");
    if lang.eq_ignore_ascii_case("ja") || lang.eq_ignore_ascii_case("japanese") {
        UiLang::Ja
    } else {
        UiLang::En
    }
}

/// Resolve the UI language from explicit env-var values, in POSIX
/// precedence order `LC_ALL` > `LC_MESSAGES` > `LANG`, with GNU gettext's
/// `LANGUAGE` honored last. `C`/`POSIX`/empty values carry no language
/// and fall through; the first meaningful value wins.
pub fn detect_system_language_from_env_vars(
    lc_all: Option<&str>,
    lc_messages: Option<&str>,
    lang: Option<&str>,
    language: Option<&str>,
) -> UiLang {
    for raw in [lc_all, lc_messages, lang, language].into_iter().flatten() {
        let value = raw.trim();
        if value.is_empty()
            || value.eq_ignore_ascii_case("c")
            || value.eq_ignore_ascii_case("posix")
        {
            continue;
        }
        return parse_system_locale(value);
    }
    UiLang::En
}

/// Detect the OS system language from the process environment
/// (`LC_ALL`, `LC_MESSAGES`, `LANG`, `LANGUAGE`).
pub fn detect_system_language_from_env() -> UiLang {
    let lc_all = std::env::var("LC_ALL").ok();
    let lc_messages = std::env::var("LC_MESSAGES").ok();
    let lang = std::env::var("LANG").ok();
    let language = std::env::var("LANGUAGE").ok();
    detect_system_language_from_env_vars(
        lc_all.as_deref(),
        lc_messages.as_deref(),
        lang.as_deref(),
        language.as_deref(),
    )
}

// ---------------------------------------------------------------------------
// Auto-reconnect (opt-in, anti-spam guarded)
// ---------------------------------------------------------------------------

/// Hard cap on automatic re-dial attempts — a dropped client never
/// connection-spams a public server.
pub const MAX_RECONNECT_ATTEMPTS: u32 = 5;

/// Exponential-ish backoff ladder in milliseconds: `2s -> 4s -> 8s ->
/// 15s -> 30s`, one slot per attempt.
pub const RECONNECT_BACKOFF_MS: [u32; MAX_RECONNECT_ATTEMPTS as usize] =
    [2_000, 4_000, 8_000, 15_000, 30_000];

/// Disconnect-reason substrings (lowercased match) that mean the server
/// *refused* the client: retrying is pointless and looks like a
/// connection-spam bot to Watchdog/Grim-style anti-spam. Any hit blocks
/// auto-reconnect entirely.
const SERVER_REFUSED_REASONS: &[&str] = &[
    "banned",
    "blacklist",
    "whitelist",
    "white-listed",
    "kicked",
    "outdated",
    "invalid session",
    "failed to verify",
    "server is full",
    "not whitelisted",
    "verify username",
    "multiplayer disabled",
    "authentication servers are down",
];

/// `true` when an unexpected disconnect is safe to auto-reconnect from:
/// the reason carries no server-refusal signal (bans, kicks, whitelist
/// rejections, version mismatches, full servers, auth failures are all
/// refused) so retrying can never look like connection spam.
pub fn should_auto_reconnect(reason: &str) -> bool {
    let reason = reason.to_ascii_lowercase();
    !SERVER_REFUSED_REASONS
        .iter()
        .any(|needle| reason.contains(needle))
}

/// Auto-reconnect state machine phases.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum AutoReconnectState {
    /// No reconnect pending.
    #[default]
    Idle,
    /// Counting down before the next dial.
    Reconnecting {
        /// Target host to redial.
        host: String,
        /// Target port.
        port: u16,
        /// 1-based attempt number (of [`MAX_RECONNECT_ATTEMPTS`]).
        attempt: u32,
        /// Backoff used for this attempt, milliseconds.
        delay_ms: u32,
        /// Milliseconds left before dialing.
        remaining_ms: u32,
        /// `true` when the target is an integrated server still waking
        /// up — the host shows "waiting for server" status instead of
        /// a bare countdown.
        waking_server: bool,
    },
    /// Suppressed by an intentional disconnect or an explicit cancel.
    Cancelled,
}

/// Opt-in auto-reconnect controller: pure countdown state machine — the
/// host calls [`tick`](Self::tick) each frame and dials once it returns
/// `true`. Never reconnects when the feature is disabled, when the
/// disconnect was intentional, or when the server refused the client
/// ([`should_auto_reconnect`]), and stops hard at
/// [`MAX_RECONNECT_ATTEMPTS`].
#[derive(Clone, Debug, Default)]
pub struct AutoReconnectController {
    /// Master switch mirroring `QolSettings::auto_reconnect`; the host
    /// keeps it in sync so a mid-countdown toggle-off stops dialing.
    pub enabled: bool,
    /// Current phase.
    pub state: AutoReconnectState,
}

impl AutoReconnectController {
    pub fn new(enabled: bool) -> Self {
        Self {
            enabled,
            state: AutoReconnectState::Idle,
        }
    }

    /// React to a connection drop the player did not ask for. Starts the
    /// countdown only when the feature is enabled and the reason is not
    /// a server refusal; otherwise the controller stays inert.
    pub fn on_unexpected_disconnect(&mut self, host: &str, port: u16, reason: &str) {
        if !self.enabled || !should_auto_reconnect(reason) {
            return;
        }
        self.state = AutoReconnectState::Reconnecting {
            host: host.to_string(),
            port,
            attempt: 1,
            delay_ms: RECONNECT_BACKOFF_MS[0],
            remaining_ms: RECONNECT_BACKOFF_MS[0],
            waking_server: false,
        };
    }

    /// The player left (or got kicked via UI) on purpose: suppress any
    /// pending or future reconnect until the next unexpected drop.
    pub fn on_intentional_disconnect(&mut self) {
        self.state = AutoReconnectState::Cancelled;
    }

    /// Stop the countdown (Cancel button, screen change, toggle-off).
    pub fn cancel(&mut self) {
        self.state = AutoReconnectState::Cancelled;
    }

    /// `true` while a dial is being counted down.
    pub fn is_reconnecting(&self) -> bool {
        matches!(self.state, AutoReconnectState::Reconnecting { .. })
    }

    /// Countdown in milliseconds for the status line (`None` unless
    /// reconnecting).
    pub fn remaining_ms(&self) -> Option<u32> {
        match self.state {
            AutoReconnectState::Reconnecting { remaining_ms, .. } => Some(remaining_ms),
            _ => None,
        }
    }

    /// `(attempt, max)` progress for the status line.
    pub fn attempt_progress(&self) -> Option<(u32, u32)> {
        match self.state {
            AutoReconnectState::Reconnecting { attempt, .. } => {
                Some((attempt, MAX_RECONNECT_ATTEMPTS))
            }
            _ => None,
        }
    }

    /// Mark the current target as a still-booting integrated server —
    /// the countdown keeps running but the status line can say "waking
    /// server" instead of implying a public-server redial.
    pub fn mark_waking_server(&mut self) {
        if let AutoReconnectState::Reconnecting {
            waking_server, ..
        } = &mut self.state
        {
            *waking_server = true;
        }
    }

    /// Advance the countdown by `dt_ms`. Returns `true` exactly once —
    /// the frame the timer reaches zero, when the host should dial —
    /// then either schedules the next attempt with the next backoff rung
    /// or exhausts the ladder and lands in `Cancelled`.
    pub fn tick(&mut self, dt_ms: u32) -> bool {
        let AutoReconnectState::Reconnecting {
            attempt,
            remaining_ms,
            ..
        } = &mut self.state
        else {
            return false;
        };
        if !self.enabled {
            self.state = AutoReconnectState::Cancelled;
            return false;
        }
        *remaining_ms = remaining_ms.saturating_sub(dt_ms);
        if *remaining_ms > 0 {
            return false;
        }
        // Ready to dial now: pre-schedule the follow-up attempt so a
        // failed dial keeps backing off without a second "fire" event.
        if *attempt >= MAX_RECONNECT_ATTEMPTS {
            self.state = AutoReconnectState::Cancelled;
        } else {
            let next = *attempt;
            let next_delay = RECONNECT_BACKOFF_MS[next as usize];
            if let AutoReconnectState::Reconnecting {
                attempt,
                delay_ms,
                remaining_ms,
                ..
            } = &mut self.state
            {
                *attempt = next + 1;
                *delay_ms = next_delay;
                *remaining_ms = next_delay;
            }
        }
        true
    }

    /// The `(host, port)` a fired [`tick`](Self::tick) should dial.
    pub fn dial_target(&self) -> Option<(&str, u16)> {
        match &self.state {
            AutoReconnectState::Reconnecting { host, port, .. } => {
                Some((host.as_str(), *port))
            }
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Toast notifications (bounded, non-intrusive, top-right)
// ---------------------------------------------------------------------------

/// Maximum toasts rendered at once — the stack can never bury the HUD.
pub const TOAST_MAX_VISIBLE: usize = 3;
/// Pending toasts retained beyond the visible window; older waiting
/// toasts drop first.
pub const TOAST_MAX_QUEUED: usize = 8;
/// Default toast lifetime, milliseconds.
pub const TOAST_DEFAULT_TTL_MS: u32 = 3_000;
/// Fade ramp length at each end of a toast's life, milliseconds.
pub const TOAST_FADE_MS: u32 = 250;

/// Severity/flavor of a toast — selects the accent stripe color.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ToastKind {
    /// Neutral information (default).
    #[default]
    Info,
    /// Good-news accent (connected, screenshot saved, ...).
    Success,
    /// Caution accent (low durability, connection retrying, ...).
    Warning,
    /// Failure accent (disconnected, save failed, ...).
    Error,
}

/// One queued toast: title + optional body, fixed TTL, fade alpha.
#[derive(Clone, Debug, PartialEq)]
pub struct ToastNotification {
    /// Visual flavor.
    pub kind: ToastKind,
    /// Short headline (EN/JA-safe — clipped by the renderer).
    pub title: String,
    /// Optional second line; empty when unused.
    pub body: String,
    /// Total lifetime in milliseconds.
    pub ttl_ms: u32,
    /// Milliseconds already lived.
    pub elapsed_ms: u32,
}

impl ToastNotification {
    pub fn new(kind: ToastKind, title: impl Into<String>, body: impl Into<String>) -> Self {
        Self {
            kind,
            title: title.into(),
            body: body.into(),
            ttl_ms: TOAST_DEFAULT_TTL_MS,
            elapsed_ms: 0,
        }
    }

    /// `true` while the toast has lifetime left.
    pub fn is_alive(&self) -> bool {
        self.elapsed_ms < self.ttl_ms
    }

    /// Milliseconds left before expiry (for tests/status).
    pub fn remaining_ms(&self) -> u32 {
        self.ttl_ms.saturating_sub(self.elapsed_ms)
    }

    /// Opacity `0.0..=1.0`: ramps up over the first [`TOAST_FADE_MS`]
    /// and back down over the last — a smooth fade instead of a pop.
    pub fn alpha(&self) -> f32 {
        if self.ttl_ms == 0 {
            return 1.0;
        }
        let fade = TOAST_FADE_MS as f32;
        let fade_in = (self.elapsed_ms as f32 / fade).min(1.0);
        let fade_out = (self.remaining_ms() as f32 / fade).min(1.0);
        fade_in.min(fade_out).clamp(0.0, 1.0)
    }
}

/// Bounded toast stack feeding the top-right overlay. Identical
/// `(title, body)` pushes refresh the live toast instead of stacking —
/// a reconnect status line flapping every frame never fills the queue.
#[derive(Clone, Debug, Default)]
pub struct ToastQueue {
    toasts: Vec<ToastNotification>,
}

impl ToastQueue {
    pub fn new() -> Self {
        Self::default()
    }

    /// Queue a toast with the default [`TOAST_DEFAULT_TTL_MS`]. A
    /// duplicate of an already-active toast just resets that toast's
    /// timer (dedup); genuinely new toasts queue behind the visible
    /// window and the oldest pending ones drop past
    /// [`TOAST_MAX_QUEUED`].
    pub fn push(&mut self, kind: ToastKind, title: &str, body: &str) {
        if let Some(existing) = self
            .toasts
            .iter_mut()
            .find(|t| t.title == title && t.body == body)
        {
            existing.elapsed_ms = 0;
            existing.kind = kind;
            return;
        }
        self.toasts
            .push(ToastNotification::new(kind, title, body));
        // Bound the queue: once the visible window is full the oldest
        // waiting toasts are the first to go.
        while self.toasts.len() > TOAST_MAX_QUEUED {
            self.toasts.remove(0);
        }
    }

    /// Advance every toast by `dt_ms`, dropping expired ones.
    pub fn tick(&mut self, dt_ms: u32) {
        for toast in &mut self.toasts {
            toast.elapsed_ms = toast.elapsed_ms.saturating_add(dt_ms);
        }
        self.toasts.retain(ToastNotification::is_alive);
    }

    /// The visible window: at most [`TOAST_MAX_VISIBLE`] toasts, oldest
    /// on top so the stack reads in arrival order.
    pub fn visible(&self) -> &[ToastNotification] {
        &self.toasts[..self.toasts.len().min(TOAST_MAX_VISIBLE)]
    }

    /// Toasts waiting behind the visible window (for a "+N" hint).
    pub fn pending_count(&self) -> usize {
        self.toasts.len().saturating_sub(TOAST_MAX_VISIBLE)
    }

    pub fn is_empty(&self) -> bool {
        self.toasts.is_empty()
    }

    pub fn len(&self) -> usize {
        self.toasts.len()
    }

    /// Drop every toast (screen change, world unload).
    pub fn clear(&mut self) {
        self.toasts.clear();
    }
}

// ---------------------------------------------------------------------------
// Shulker box / bundle tooltip peek
// ---------------------------------------------------------------------------

/// Compact tooltip summary of a container's contents — the Shulker Box
/// peek and Bundle peek share it. `lines` holds the top stacks,
/// `more_kinds` the hidden remainder (`"+K more"`), and `is_empty`
/// short-circuits empty containers so the caller can skip the tooltip.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ShulkerPeekSummary {
    /// `true` when the container holds nothing.
    pub is_empty: bool,
    /// Top stacks, display order preserved: `("name", count)` pairs.
    pub top_stacks: Vec<(String, u32)>,
    /// Distinct item kinds beyond `top_stacks` (the "+K more" count).
    pub more_kinds: usize,
    /// Total item count across every stack.
    pub total_items: u32,
    /// Total non-empty stacks in the container.
    pub total_stacks: usize,
}

impl ShulkerPeekSummary {
    /// Summarize `(item_name, count)` slot contents, keeping the first
    /// `top_n` distinct stacks verbatim and counting the rest.
    pub fn from_contents(contents: &[(impl AsRef<str>, u32)], top_n: usize) -> Self {
        let mut top_stacks = Vec::new();
        let mut more_kinds = 0usize;
        let mut total_items = 0u32;
        let mut total_stacks = 0usize;
        for (name, count) in contents {
            let name = name.as_ref();
            if name.is_empty() || *count == 0 {
                continue;
            }
            total_stacks += 1;
            total_items = total_items.saturating_add(*count);
            if top_stacks.len() < top_n {
                top_stacks.push((name.to_string(), *count));
            } else {
                more_kinds += 1;
            }
        }
        Self {
            is_empty: total_stacks == 0,
            top_stacks,
            more_kinds,
            total_items,
            total_stacks,
        }
    }

    /// Rendered tooltip rows (`"name ×count"`), then a `"+K more"`
    /// trailer — empty containers yield no rows at all.
    pub fn lines(&self, lang: UiLang) -> Vec<String> {
        let mut lines: Vec<String> = self
            .top_stacks
            .iter()
            .map(|(name, count)| format!("{name} x{count}"))
            .collect();
        if self.more_kinds > 0 {
            lines.push(match lang {
                UiLang::En => format!("+{} more", self.more_kinds),
                UiLang::Ja => format!("ほか{}種", self.more_kinds),
            });
        }
        lines
    }
}

// ---------------------------------------------------------------------------
// Food-value preview (opt-in)
// ---------------------------------------------------------------------------

/// Vanilla 1.21.1 `(hunger_points, saturation_points)` for a food item
/// name — hunger in half-drumstick points, saturation in vanilla's
/// internal half-point units. Accepts the
/// namespaced form (`minecraft:cooked_beef`) and common aliases
/// (`steak`); returns `None` for non-foods.
pub fn food_preview_stats(item_name: &str) -> Option<(u8, f32)> {
    let name = item_name
        .trim()
        .rsplit(':')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    let (hunger, saturation): (u8, f32) = match name.as_str() {
        "apple" => (4, 2.4),
        "baked_potato" => (5, 6.0),
        "beetroot" => (1, 1.2),
        "beetroot_soup" => (6, 7.2),
        "bread" => (5, 6.0),
        "carrot" => (3, 3.6),
        "chorus_fruit" => (4, 2.4),
        "cooked_beef" | "steak" => (8, 12.8),
        "cooked_chicken" => (6, 7.2),
        "cooked_cod" => (5, 6.0),
        "cooked_mutton" => (6, 9.6),
        "cooked_porkchop" => (8, 12.8),
        "cooked_rabbit" => (5, 6.0),
        "cooked_salmon" => (6, 9.6),
        "cookie" => (2, 0.4),
        "dried_kelp" => (1, 0.6),
        "enchanted_golden_apple" => (4, 9.6),
        "glow_berries" => (2, 0.4),
        "golden_apple" => (4, 9.6),
        "golden_carrot" => (6, 14.4),
        "honey_bottle" => (6, 1.2),
        "melon_slice" => (2, 1.2),
        "mushroom_stew" => (6, 7.2),
        "poisonous_potato" => (2, 1.2),
        "potato" => (1, 0.6),
        "pufferfish" => (1, 0.2),
        "pumpkin_pie" => (8, 4.8),
        "rabbit_stew" => (10, 12.0),
        "raw_beef" | "beef" => (3, 1.8),
        "raw_chicken" | "chicken" => (2, 1.2),
        "raw_cod" | "cod" => (2, 0.4),
        "raw_mutton" | "mutton" => (2, 1.2),
        "raw_porkchop" | "porkchop" => (3, 1.8),
        "raw_rabbit" | "rabbit" => (3, 1.8),
        "raw_salmon" | "salmon" => (2, 0.4),
        "rotten_flesh" => (4, 0.8),
        "spider_eye" => (2, 3.2),
        "suspicious_stew" => (6, 7.2),
        "sweet_berries" => (2, 0.4),
        "tropical_fish" => (1, 0.2),
        _ => return None,
    };
    Some((hunger, saturation))
}

// ---------------------------------------------------------------------------
// Keybind conflict detection
// ---------------------------------------------------------------------------

/// One assignable control row shown on `Settings -> Controls`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeybindEntry {
    /// Human-readable action name (`"Forward"`, `"Open Chat"`).
    pub action: String,
    /// The key it currently fires on (`"W"`, `"Space"`, `"LControl"`).
    /// Empty = unbound; unbound rows never conflict.
    pub key: String,
    /// `true` for bindings that only fire inside a GUI context (chat,
    /// inventory, menus). GUI-context keys collide only with other
    /// GUI-context bindings — a chat key sharing a letter with a
    /// movement key is not a real conflict.
    pub gui_context: bool,
}

impl KeybindEntry {
    pub fn new(action: &str, key: &str, gui_context: bool) -> Self {
        Self {
            action: action.to_string(),
            key: key.to_string(),
            gui_context,
        }
    }
}

/// The Controls page binding table. Pure display data — the host owns
/// rebinding; this crate only detects and highlights collisions.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct KeyBindings {
    /// Rows in display order; conflict reports index into this.
    pub entries: Vec<KeybindEntry>,
}

impl KeyBindings {
    /// The stock client control set, pre-populated so `Settings ->
    /// Controls` has something to audit before the host syncs real
    /// bindings in.
    pub fn vanilla_defaults() -> Self {
        let entries = [
            ("Forward", "W", false),
            ("Back", "S", false),
            ("Left", "A", false),
            ("Right", "D", false),
            ("Jump", "Space", false),
            ("Sneak", "LShift", false),
            ("Sprint", "LControl", false),
            ("Attack", "MouseLeft", false),
            ("Use Item", "MouseRight", false),
            ("Pick Block", "MouseMiddle", false),
            ("Drop", "Q", false),
            ("Hotbar 1", "1", false),
            ("Hotbar 9", "9", false),
            ("Swap Hands", "F", false),
            ("Perspective", "F5", false),
            ("Screenshot", "F2", false),
            ("Zoom", "C", false),
            ("Inventory", "E", true),
            ("Chat", "T", true),
            ("Command", "/", true),
            ("Player List", "Tab", true),
            ("Sort Inventory", "R", true),
        ];
        Self {
            entries: entries
                .iter()
                .map(|(a, k, g)| KeybindEntry::new(a, k, *g))
                .collect(),
        }
    }
}

/// One detected collision: two binding rows sharing a normalized key in
/// the same context. `(first_index, second_index, normalized_key)`.
pub type KeybindConflict = (usize, usize, String);

/// Normalize a key name for comparison: case-insensitive, whitespace
/// tolerant (`"left ctrl"` ≡ `"LControl"` ≡ `"lcontrol"`).
fn normalize_key(key: &str) -> String {
    key.chars()
        .filter(|c| !c.is_whitespace() && *c != '_' && *c != '-')
        .flat_map(char::to_lowercase)
        .collect()
}

/// Find every colliding key assignment — red flags on the Controls
/// page. Unbound rows never conflict, and gameplay keys
/// only collide with gameplay keys (GUI-context bindings live in a
/// separate input domain). Pairs are reported in stable index order.
pub fn detect_keybind_conflicts(bindings: &KeyBindings) -> Vec<KeybindConflict> {
    let mut conflicts = Vec::new();
    for (i, a) in bindings.entries.iter().enumerate() {
        let ka = normalize_key(&a.key);
        if ka.is_empty() {
            continue;
        }
        for (j, b) in bindings.entries.iter().enumerate().skip(i + 1) {
            if a.gui_context != b.gui_context {
                continue;
            }
            if normalize_key(&b.key) == ka {
                conflicts.push((i, j, ka.clone()));
            }
        }
    }
    conflicts
}

/// Indices of every row participating in at least one conflict — the
/// set the Controls page badges with `[!]`.
pub fn conflicted_bind_indices(conflicts: &[KeybindConflict]) -> Vec<usize> {
    let mut indices: Vec<usize> = Vec::new();
    for &(i, j, _) in conflicts {
        if !indices.contains(&i) {
            indices.push(i);
        }
        if !indices.contains(&j) {
            indices.push(j);
        }
    }
    indices.sort_unstable();
    indices
}

// ---------------------------------------------------------------------------
// Auto-replenish planner (opt-in; yields to inventory mods & fair-play)
// ---------------------------------------------------------------------------

/// Tool suffixes recognized for tier/type matching when the exact item
/// id is gone from the inventory.
const TOOL_KINDS: &[&str] = &[
    "pickaxe",
    "axe",
    "shovel",
    "hoe",
    "sword",
    "helmet",
    "chestplate",
    "leggings",
    "boots",
    "bow",
    "crossbow",
    "fishing_rod",
    "shears",
    "flint_and_steel",
    "shield",
    "trident",
];

/// Material tiers in replacement-preference order (best last). Only
/// consulted when the exact tool is unavailable.
const TOOL_TIERS: &[&str] = &[
    "wooden",
    "golden",
    "stone",
    "leather",
    "iron",
    "chainmail",
    "copper",
    "diamond",
    "netherite",
    "turtle",
];

fn tool_parts(item: &str) -> Option<(usize, usize)> {
    let name = item.rsplit(':').next().unwrap_or(item);
    for (kind_i, kind) in TOOL_KINDS.iter().enumerate() {
        if name == *kind || name.ends_with(&format!("_{kind}")) {
            let prefix = &name[..name.len() - kind.len()];
            let tier = prefix
                .trim_end_matches('_')
                .split('_')
                .next()
                .and_then(|head| TOOL_TIERS.iter().position(|t| *t == head))
                // Unprefixed tools (`shears`, `bow`, `shield`, ...) share
                // a single tier slot so same-kind tools still match.
                .unwrap_or(TOOL_TIERS.len());
            return Some((kind_i, tier));
        }
    }
    None
}

/// Choose the inventory slot that should replace `depleted_item` when
/// `hotbar_slot` empties: exact-name matches win, preferring the
/// *smallest* stack for stackables (partial stacks get consumed first
/// like vanilla's refill intuition); when the depleted item was a tool
/// and no exact match remains, the same tool kind is picked at the
/// same-or-better tier (falling back to the best available tier). `None`
/// when nothing qualifies. `inv_slots` excludes the hotbar — indices
/// return straight into that slice.
pub fn plan_auto_replenish(
    depleted_item: &str,
    hotbar_slot: usize,
    inv_slots: &[Option<(&str, u8)>],
) -> Option<usize> {
    let _ = hotbar_slot; // Reserved for host-side range checks.
    let item = depleted_item.trim();
    if item.is_empty() {
        return None;
    }
    let base = item.rsplit(':').next().unwrap_or(item);

    // Pass 1: exact item match — smallest stack first so partial stacks
    // drain before fresh ones.
    let mut exact: Option<(usize, u8)> = None;
    for (i, slot) in inv_slots.iter().enumerate() {
        let Some((name, count)) = slot else { continue };
        if *count == 0 {
            continue;
        }
        let candidate = name.rsplit(':').next().unwrap_or(name);
        if candidate == base {
            match exact {
                Some((_, best)) if *count >= best => {}
                _ => exact = Some((i, *count)),
            }
        }
    }
    if let Some((i, _)) = exact {
        return Some(i);
    }

    // Pass 2 (tools only): same kind, prefer the lowest tier that is
    // still >= the depleted tier, otherwise the best tier available.
    let (kind, tier) = tool_parts(base)?;
    let mut best: Option<(usize, usize, bool)> = None; // (index, tier, meets_tier)
    for (i, slot) in inv_slots.iter().enumerate() {
        let Some((name, count)) = slot else { continue };
        if *count == 0 {
            continue;
        }
        let cand_base = name.rsplit(':').next().unwrap_or(name);
        let Some((cand_kind, cand_tier)) = tool_parts(cand_base) else {
            continue;
        };
        if cand_kind != kind {
            continue;
        }
        let meets = cand_tier >= tier;
        let dominated = match best {
            // Prefer any meeting-tier candidate; among equals pick the
            // tier closest to the depleted one (meetings) or the highest
            // tier (fallbacks).
            Some((_, bt, bm)) => {
                (bm && !meets)
                    || (bm == meets && ((meets && cand_tier >= bt) || (!meets && cand_tier <= bt)))
            }
            None => false,
        };
        if !dominated {
            best = Some((i, cand_tier, meets));
        }
    }
    best.map(|(i, _, _)| i)
}

// ---------------------------------------------------------------------------
// Quick Stack to Nearby Chests + deterministic container sort (opt-in)
// ---------------------------------------------------------------------------

/// Quick-stack-to-chest planner: `(player_slot, chest_slot, amount)`
/// moves that top up stacks whose item type *already* exists in the
/// chest — never seeding a new item kind, never touching hotbar slots
/// `0..9` of `player_inv` (indices `0..=8` are the hotbar). Moves are
/// deterministic: player slots in index order, chest stacks filled
/// lowest-index-first until `max_stack`.
pub fn plan_quick_stack(
    player_inv: &[Option<(&str, u8)>],
    chest_slots: &[Option<(&str, u8)>],
    max_stack: u8,
) -> Vec<(usize, usize, u8)> {
    if max_stack == 0 {
        return Vec::new();
    }
    // Remaining room per chest slot for each item kind already inside.
    let mut room: Vec<(String, u8)> = Vec::with_capacity(chest_slots.len());
    let mut known: Vec<String> = Vec::new();
    for slot in chest_slots {
        if let Some((name, count)) = slot {
            if *count == 0 {
                continue;
            }
            let name = (*name).to_string();
            room.push((name.clone(), max_stack.saturating_sub(*count)));
            if !known.contains(&name) {
                known.push(name);
            }
        } else {
            room.push((String::new(), 0));
        }
    }

    let mut moves = Vec::new();
    for (pi, slot) in player_inv.iter().enumerate().skip(9) {
        let Some((name, count)) = slot else { continue };
        if *count == 0 || !known.iter().any(|k| k == name) {
            continue;
        }
        let mut left = *count;
        for (ci, (cname, croom)) in room.iter_mut().enumerate() {
            if left == 0 {
                break;
            }
            if cname != name || *croom == 0 {
                continue;
            }
            let moved = left.min(*croom);
            *croom -= moved;
            left -= moved;
            moves.push((pi, ci, moved));
        }
    }
    moves
}

/// Deterministic container sort order: a permutation of slot indices —
/// named stacks first sorted by `(name asc, count desc)`, empty slots
/// last in index order — so a chest sorted twice is a fixed point and
/// two clients agree byte-for-byte.
pub fn plan_sort_container(slots: &[Option<(&str, u8)>]) -> Vec<usize> {
    let mut order: Vec<usize> = (0..slots.len()).collect();
    order.sort_by(|&a, &b| match (&slots[a], &slots[b]) {
        (Some((na, ca)), Some((nb, cb))) => na.cmp(nb).then(cb.cmp(ca)),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => a.cmp(&b),
    });
    order
}

// ---------------------------------------------------------------------------
// Chat scroll-lock (never yank a reader back to the bottom)
// ---------------------------------------------------------------------------

/// Chat scroll-lock state for the HUD scrollback. While `scrolled_up`
/// the renderer shows lines `scroll_offset` above the tail and incoming
/// messages accumulate in `unread_while_scrolled` instead of dragging
/// the view to the bottom.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ChatScrollLockState {
    /// `true` while the player is reading older chat.
    pub scrolled_up: bool,
    /// Lines above the newest the view is anchored at.
    pub scroll_offset: usize,
    /// Messages arrived since the lock engaged (for a "+N new" badge).
    pub unread_while_scrolled: usize,
}

impl ChatScrollLockState {
    /// Scroll `delta` lines away from the bottom (clamped by the host to
    /// the scrollback length); crossing zero drops the lock.
    pub fn scroll_by(&mut self, delta: usize) {
        if delta == 0 {
            return;
        }
        self.scroll_offset = self.scroll_offset.saturating_add(delta);
        self.scrolled_up = self.scroll_offset > 0;
        if !self.scrolled_up {
            self.unread_while_scrolled = 0;
        }
    }

    /// Scroll `delta` lines back toward the bottom; reaching zero clears
    /// the lock and the unread counter.
    pub fn scroll_back(&mut self, delta: usize) {
        self.scroll_offset = self.scroll_offset.saturating_sub(delta);
        if self.scroll_offset == 0 {
            self.scrolled_up = false;
            self.unread_while_scrolled = 0;
        }
    }

    /// Jump straight to the newest message (End key / chat open).
    pub fn scroll_to_bottom(&mut self) {
        self.scrolled_up = false;
        self.scroll_offset = 0;
        self.unread_while_scrolled = 0;
    }

    /// A new chat line arrived. Locked readers keep their place: the
    /// offset slides with the buffer and the unread counter ticks up.
    /// Unlocked readers stay pinned to the bottom untouched.
    pub fn on_new_message(&mut self) {
        if self.scrolled_up {
            self.scroll_offset = self.scroll_offset.saturating_add(1);
            self.unread_while_scrolled = self.unread_while_scrolled.saturating_add(1);
        }
    }

    /// Clamp the offset into `0..=max_offset` (scrollback length minus
    /// the visible window) — a shrinking buffer can never strand the
    /// lock above the oldest line.
    pub fn clamp_offset(&mut self, max_offset: usize) {
        if self.scroll_offset > max_offset {
            self.scroll_offset = max_offset;
        }
        self.scrolled_up = self.scroll_offset > 0;
        if !self.scrolled_up {
            self.unread_while_scrolled = 0;
        }
    }

    /// `"+N new"` badge text while unread messages accumulate (`None`
    /// when locked-out or caught up).
    pub fn unread_label(&self, lang: UiLang) -> Option<String> {
        if !self.scrolled_up || self.unread_while_scrolled == 0 {
            return None;
        }
        Some(match lang {
            UiLang::En => format!("+{} new", self.unread_while_scrolled),
            UiLang::Ja => format!("+{}", self.unread_while_scrolled),
        })
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -- locale ---------------------------------------------------------

    #[test]
    fn parse_system_locale_japanese_variants() {
        for raw in [
            "ja",
            "JA",
            "ja_JP",
            "ja-JP",
            "ja_JP.UTF-8",
            "ja-jp.utf8",
            "Japanese_Japan.932",
            "japanese",
            "ja_JP@cjknarrow",
            "ja:en_US",
        ] {
            assert_eq!(parse_system_locale(raw), UiLang::Ja, "{raw}");
        }
    }

    #[test]
    fn parse_system_locale_everything_else_is_english() {
        for raw in [
            "",
            "en",
            "en_US.UTF-8",
            "en-US",
            "C",
            "POSIX",
            "ko_KR",
            "zh_CN",
            "jav",
            "javanese",
            "de_DE",
        ] {
            assert_eq!(parse_system_locale(raw), UiLang::En, "{raw}");
        }
    }

    #[test]
    fn detect_system_language_env_precedence() {
        // LC_ALL beats everything.
        assert_eq!(
            detect_system_language_from_env_vars(
                Some("ja_JP.UTF-8"),
                Some("en_US"),
                Some("en_US"),
                None
            ),
            UiLang::Ja
        );
        // LC_MESSAGES next, then LANG, then LANGUAGE.
        assert_eq!(
            detect_system_language_from_env_vars(
                Some("C"),
                Some("ja_JP"),
                Some("en_US"),
                Some("en_US")
            ),
            UiLang::Ja
        );
        assert_eq!(
            detect_system_language_from_env_vars(
                None,
                None,
                Some("Japanese_Japan.932"),
                Some("en_US")
            ),
            UiLang::Ja
        );
        assert_eq!(
            detect_system_language_from_env_vars(None, None, None, Some("ja:en_US")),
            UiLang::Ja
        );
        // Empty / C / POSIX fall through; nothing set => En.
        assert_eq!(
            detect_system_language_from_env_vars(Some(""), Some("C"), Some("POSIX"), None),
            UiLang::En
        );
        assert_eq!(
            detect_system_language_from_env_vars(None, None, None, None),
            UiLang::En
        );
        assert_eq!(
            detect_system_language_from_env_vars(None, None, Some("ko_KR"), None),
            UiLang::En
        );
    }

    // -- auto-reconnect -------------------------------------------------

    #[test]
    fn server_refusals_never_auto_reconnect() {
        for reason in [
            "You are banned from this server",
            "You are not white-listed on this server!",
            "whitelist",
            "Kicked by an operator",
            "Outdated client! Please use 1.21.1",
            "Invalid session (Try restarting your game)",
            "Failed to verify username!",
            "Server is full",
            "You were kicked for flying",
        ] {
            assert!(!should_auto_reconnect(reason), "{reason}");

            let mut c = AutoReconnectController::new(true);
            c.on_unexpected_disconnect("example.org", 25565, reason);
            assert_eq!(c.state, AutoReconnectState::Idle, "{reason}");
        }
    }

    #[test]
    fn ordinary_drops_reconnect_when_enabled() {
        for reason in [
            "Connection timed out",
            "java.io.IOException: Connection reset by peer",
            "Disconnected",
            "Server closed",
        ] {
            assert!(should_auto_reconnect(reason), "{reason}");
        }
        let mut c = AutoReconnectController::new(true);
        c.on_unexpected_disconnect("example.org", 25565, "Connection timed out");
        match &c.state {
            AutoReconnectState::Reconnecting {
                host,
                port,
                attempt,
                delay_ms,
                remaining_ms,
                ..
            } => {
                assert_eq!(host, "example.org");
                assert_eq!(*port, 25565);
                assert_eq!(*attempt, 1);
                assert_eq!(*delay_ms, 2_000);
                assert_eq!(*remaining_ms, 2_000);
            }
            other => panic!("expected Reconnecting, got {other:?}"),
        }
    }

    #[test]
    fn disabled_feature_never_reconnects() {
        let mut c = AutoReconnectController::new(false);
        c.on_unexpected_disconnect("example.org", 25565, "timed out");
        assert_eq!(c.state, AutoReconnectState::Idle);
    }

    #[test]
    fn intentional_disconnect_suppresses_reconnect() {
        let mut c = AutoReconnectController::new(true);
        c.on_unexpected_disconnect("example.org", 25565, "timed out");
        assert!(c.is_reconnecting());
        // Choosing "Disconnect" mid-countdown cancels the dial.
        c.on_intentional_disconnect();
        assert_eq!(c.state, AutoReconnectState::Cancelled);
        assert!(!c.tick(10_000));
        // A genuinely new unexpected drop later re-arms the controller.
        c.on_unexpected_disconnect("example.org", 25565, "timed out");
        assert!(c.is_reconnecting());
    }

    #[test]
    fn backoff_ladder_and_attempt_cap() {
        let mut c = AutoReconnectController::new(true);
        c.on_unexpected_disconnect("h", 25565, "timed out");
        assert_eq!(c.attempt_progress(), Some((1, MAX_RECONNECT_ATTEMPTS)));

        assert!(!c.tick(1_999));
        assert!(c.tick(1)); // attempt 1 fires at 2s
        assert_eq!(c.dial_target(), Some(("h", 25565)));
        assert_eq!(c.attempt_progress(), Some((2, MAX_RECONNECT_ATTEMPTS)));

        assert!(!c.tick(3_999));
        assert!(c.tick(1)); // attempt 2 fires at 4s
        assert!(!c.tick(7_999));
        assert!(c.tick(1)); // attempt 3 fires at 8s
        assert!(!c.tick(14_999));
        assert!(c.tick(1)); // attempt 4 fires at 15s
        assert!(!c.tick(29_999));
        assert!(c.tick(1)); // attempt 5 fires at 30s — ladder exhausted.
        assert_eq!(c.state, AutoReconnectState::Cancelled);
        assert!(!c.tick(1_000));
    }

    #[test]
    fn cancel_stops_the_countdown() {
        let mut c = AutoReconnectController::new(true);
        c.on_unexpected_disconnect("h", 25565, "timed out");
        c.tick(1_000);
        c.cancel();
        assert_eq!(c.state, AutoReconnectState::Cancelled);
        assert!(!c.tick(10_000));
    }

    #[test]
    fn toggle_off_mid_countdown_cancels() {
        let mut c = AutoReconnectController::new(true);
        c.on_unexpected_disconnect("h", 25565, "timed out");
        c.enabled = false;
        assert!(!c.tick(5_000));
        assert_eq!(c.state, AutoReconnectState::Cancelled);
    }

    #[test]
    fn waking_server_flag_is_status_only() {
        let mut c = AutoReconnectController::new(true);
        c.on_unexpected_disconnect("localhost", 25565, "timed out");
        c.mark_waking_server();
        match &c.state {
            AutoReconnectState::Reconnecting { waking_server, .. } => assert!(waking_server),
            other => panic!("expected Reconnecting, got {other:?}"),
        }
    }

    // -- toasts -----------------------------------------------------------

    #[test]
    fn toast_queue_bounds_visible_and_dedups() {
        let mut q = ToastQueue::new();
        for i in 0..5 {
            q.push(ToastKind::Info, &format!("t{i}"), "");
        }
        assert_eq!(q.visible().len(), TOAST_MAX_VISIBLE);
        assert_eq!(q.pending_count(), 2);
        // Duplicate of an active toast refreshes instead of stacking.
        q.push(ToastKind::Warning, "t0", "");
        assert_eq!(q.len(), 5);
        assert_eq!(q.visible()[0].kind, ToastKind::Warning);
        assert_eq!(q.visible()[0].elapsed_ms, 0);
    }

    #[test]
    fn toast_queue_caps_pending_overflow() {
        let mut q = ToastQueue::new();
        for i in 0..(TOAST_MAX_QUEUED + 4) {
            q.push(ToastKind::Info, &format!("n{i}"), "");
        }
        assert_eq!(q.len(), TOAST_MAX_QUEUED);
        // Oldest pending dropped first.
        assert_eq!(q.visible()[0].title, "n4");
    }

    #[test]
    fn toast_ttl_and_fade_alpha() {
        let mut t = ToastNotification::new(ToastKind::Info, "a", "b");
        assert_eq!(t.alpha(), 0.0); // born at 0 — fades in.
        t.elapsed_ms = TOAST_FADE_MS;
        assert_eq!(t.alpha(), 1.0);
        t.elapsed_ms = TOAST_DEFAULT_TTL_MS - TOAST_FADE_MS / 2;
        assert!((t.alpha() - 0.5).abs() < 0.01);
        t.elapsed_ms = TOAST_DEFAULT_TTL_MS;
        assert!(!t.is_alive());
    }

    #[test]
    fn toast_tick_expires() {
        let mut q = ToastQueue::new();
        q.push(ToastKind::Info, "a", "");
        q.push(ToastKind::Info, "b", "");
        q.tick(TOAST_DEFAULT_TTL_MS);
        assert!(q.is_empty());
    }

    // -- shulker peek ---------------------------------------------------

    #[test]
    fn shulker_peek_summarizes_top_stacks_and_more() {
        let contents = [
            ("minecraft:cobblestone", 64u32),
            ("minecraft:dirt", 32),
            ("minecraft:torch", 17),
            ("minecraft:oak_log", 8),
            ("minecraft:stone", 64),
        ];
        let s = ShulkerPeekSummary::from_contents(&contents, 3);
        assert!(!s.is_empty);
        assert_eq!(s.top_stacks.len(), 3);
        assert_eq!(s.more_kinds, 2);
        assert_eq!(s.total_items, 185);
        assert_eq!(s.total_stacks, 5);
        let lines = s.lines(UiLang::En);
        assert_eq!(lines.len(), 4);
        assert_eq!(lines[0], "minecraft:cobblestone x64");
        assert_eq!(lines[3], "+2 more");
    }

    #[test]
    fn shulker_peek_empty_and_zero_counts() {
        let empty: [(&str, u32); 0] = [];
        assert!(ShulkerPeekSummary::from_contents(&empty, 5).is_empty);
        let zeros = [("minecraft:air", 0u32), ("", 5)];
        let s = ShulkerPeekSummary::from_contents(&zeros, 5);
        assert!(s.is_empty);
        assert!(s.lines(UiLang::En).is_empty());
    }

    // -- food preview -----------------------------------------------------

    #[test]
    fn food_preview_vanilla_values() {
        assert_eq!(food_preview_stats("minecraft:apple"), Some((4, 2.4)));
        assert_eq!(food_preview_stats("bread"), Some((5, 6.0)));
        assert_eq!(food_preview_stats("cooked_beef"), Some((8, 12.8)));
        assert_eq!(food_preview_stats("steak"), Some((8, 12.8)));
        assert_eq!(food_preview_stats("golden_carrot"), Some((6, 14.4)));
        assert_eq!(food_preview_stats("cooked_porkchop"), Some((8, 12.8)));
        assert_eq!(food_preview_stats("golden_apple"), Some((4, 9.6)));
        assert_eq!(food_preview_stats("baked_potato"), Some((5, 6.0)));
        assert_eq!(food_preview_stats("melon_slice"), Some((2, 1.2)));
        assert_eq!(food_preview_stats("sweet_berries"), Some((2, 0.4)));
        assert_eq!(food_preview_stats("suspicious_stew"), Some((6, 7.2)));
        assert_eq!(food_preview_stats("pumpkin_pie"), Some((8, 4.8)));
        assert_eq!(food_preview_stats("rabbit_stew"), Some((10, 12.0)));
        assert_eq!(food_preview_stats("minecraft:stone"), None);
        assert_eq!(food_preview_stats(""), None);
        assert_eq!(food_preview_stats("diamond_pickaxe"), None);
    }

    // -- keybind conflicts ------------------------------------------------

    #[test]
    fn conflicts_detect_same_context_collisions() {
        let mut binds = KeyBindings::vanilla_defaults();
        assert!(detect_keybind_conflicts(&binds).is_empty());
        // Collide Sprint with Zoom on C (both gameplay context).
        binds.entries[6].key = "C".to_string();
        let conflicts = detect_keybind_conflicts(&binds);
        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0].0, 6);
        assert_eq!(conflicts[0].2, "c");
        let flagged = conflicted_bind_indices(&conflicts);
        assert!(flagged.contains(&6));
    }

    #[test]
    fn gui_context_keys_do_not_conflict_with_gameplay() {
        let binds = KeyBindings {
            entries: vec![
                KeybindEntry::new("Sneak", "LShift", false),
                KeybindEntry::new("Chat", "T", true),
                KeybindEntry::new("Team Chat", "t", true), // collides — both GUI.
                KeybindEntry::new("Inventory", "E", true),
                KeybindEntry::new("Drop Modifier", "E", false), // cross-context: fine.
            ],
        };
        let conflicts = detect_keybind_conflicts(&binds);
        assert_eq!(conflicts.len(), 1);
        assert_eq!((conflicts[0].0, conflicts[0].1), (1, 2));
        // Unbound rows never conflict.
        let unbound = KeyBindings {
            entries: vec![
                KeybindEntry::new("A", "", false),
                KeybindEntry::new("B", " ", false),
            ],
        };
        assert!(detect_keybind_conflicts(&unbound).is_empty());
    }

    // -- auto-replenish ----------------------------------------------------

    #[test]
    fn replenish_prefers_smallest_partial_stack() {
        let inv = [
            Some(("minecraft:cobblestone", 64u8)),
            Some(("minecraft:cobblestone", 12)),
            Some(("minecraft:dirt", 64)),
            Some(("minecraft:cobblestone", 33)),
        ];
        assert_eq!(plan_auto_replenish("cobblestone", 3, &inv), Some(1));
    }

    #[test]
    fn replenish_tool_tier_fallback() {
        let inv = [
            Some(("minecraft:wooden_pickaxe", 1u8)),
            Some(("minecraft:diamond_pickaxe", 1)),
            Some(("minecraft:cobblestone", 64)),
        ];
        // Exact match first.
        let with_exact = [
            Some(("minecraft:iron_pickaxe", 1u8)),
            Some(("minecraft:diamond_pickaxe", 1)),
        ];
        assert_eq!(plan_auto_replenish("iron_pickaxe", 0, &with_exact), Some(0));
        // No exact match: same kind, next tier up.
        assert_eq!(plan_auto_replenish("stone_pickaxe", 0, &inv), Some(1));
        // Nothing matching kind -> None (sword is a different kind).
        assert_eq!(plan_auto_replenish("iron_sword", 0, &inv), None);
        // Plain items without matches -> None.
        assert_eq!(plan_auto_replenish("dirt", 0, &inv), None);
    }

    // -- quick stack + sort ----------------------------------------------

    #[test]
    fn quick_stack_only_tops_existing_kinds_and_skips_hotbar() {
        let mut player: Vec<Option<(&str, u8)>> = vec![None; 36];
        player[0] = Some(("cobblestone", 64)); // hotbar — never touched.
        player[9] = Some(("cobblestone", 40));
        player[10] = Some(("diamond", 5)); // kind absent in chest — stays.
        player[20] = Some(("cobblestone", 10));
        let chest = [
            Some(("cobblestone", 50u8)),
            Some(("cobblestone", 64)),
            None,
        ];
        let moves = plan_quick_stack(&player, &chest, 64);
        // slot 9 pours 14 into chest[0] (50/64 has 14 room); chest[1]
        // is already full so the remaining 26 stay put, and slot 20's
        // kind already had its room spent — only one move total.
        assert_eq!(moves, vec![(9usize, 0usize, 14u8)]);
        // Hotbar slot 0 is never a move source.
        assert!(moves.iter().all(|(p, _, _)| *p != 0));
    }

    #[test]
    fn quick_stack_empty_inputs() {
        let empty: Vec<Option<(&str, u8)>> = Vec::new();
        assert!(plan_quick_stack(&empty, &empty, 64).is_empty());
        assert!(plan_quick_stack(&vec![None; 36], &[Some(("dirt", 5))], 64).is_empty());
        assert_eq!(
            plan_quick_stack(
                &vec![Some(("dirt", 5)); 12],
                &[Some(("dirt", 60))],
                0
            ),
            Vec::<(usize, usize, u8)>::new()
        );
    }

    #[test]
    fn sort_container_is_deterministic() {
        let slots = [
            Some(("dirt", 10u8)),
            None,
            Some(("cobblestone", 3)),
            Some(("dirt", 40)),
        ];
        let order = plan_sort_container(&slots);
        assert_eq!(order, vec![2, 3, 0, 1]); // cobblestone, dirt x40, dirt x10, empty
        // Sorting a sorted permutation is a fixed point.
        let sorted: Vec<Option<(&str, u8)>> = order.iter().map(|&i| slots[i]).collect();
        assert_eq!(plan_sort_container(&sorted), vec![0, 1, 2, 3]);
    }

    // -- chat scroll lock ---------------------------------------------------

    #[test]
    fn scroll_lock_keeps_reader_place_and_counts_unread() {
        let mut s = ChatScrollLockState::default();
        assert!(!s.scrolled_up);
        s.scroll_by(5);
        assert!(s.scrolled_up);
        assert_eq!(s.scroll_offset, 5);
        s.on_new_message();
        s.on_new_message();
        assert_eq!(s.scroll_offset, 7); // view anchors to the same lines.
        assert_eq!(s.unread_while_scrolled, 2);
        assert_eq!(s.unread_label(UiLang::En), Some("+2 new".to_string()));
        s.scroll_back(7);
        assert!(!s.scrolled_up);
        assert_eq!(s.unread_label(UiLang::En), None);
    }

    #[test]
    fn scroll_lock_clamps_and_bottom() {
        let mut s = ChatScrollLockState {
            scrolled_up: true,
            scroll_offset: 40,
            unread_while_scrolled: 3,
        };
        s.clamp_offset(10);
        assert_eq!(s.scroll_offset, 10);
        assert!(s.scrolled_up);
        s.scroll_to_bottom();
        assert_eq!(s.scroll_offset, 0);
        assert_eq!(s.unread_while_scrolled, 0);
        // Overscroll is harmless.
        s.scroll_by(3);
        s.scroll_back(99);
        assert!(!s.scrolled_up);
    }
}
