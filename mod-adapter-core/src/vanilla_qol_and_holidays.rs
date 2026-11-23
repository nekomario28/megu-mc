// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later

//! Vanilla & mod-pack quality-of-life primitives — pure `std`, zero new
//! dependencies, and zero wall-clock reads: every holiday decision is taken
//! from a caller-supplied [`CalendarDate`] so the tick loop stays fully
//! deterministic.
//!
//! Three subsystems:
//!
//! 1. Vanilla calendar holiday events ([`CalendarDate`],
//!    [`HolidayConfig`], [`HolidayEvent`]): the authentic vanilla
//!    Halloween headgear roll for zombie/skeleton-family mobs (exact
//!    `Zombie.finalizeSpawn` / `AbstractSkeleton.finalizeSpawn`
//!    probabilities — 2.5% jack o'lantern, 22.5% carved pumpkin, head
//!    drop chance pinned to `0.0` so the event can never be farmed), the
//!    December 24–26 Christmas chest/trapped-chest reskin that leaves
//!    ender chests untouched, and the October 20 – November 3 bat
//!    Halloween brightness window shared with `natural_spawner`.
//! 2. [`RecipeConflictResolver`]: a zero-clutter crafting-recipe
//!    conflict switcher — invisible for vanilla grids (0 or 1 matching
//!    recipe draws literally no UI), a `< 1/2 >`-style compact switcher
//!    only when a modded recipe collides, and it auto-yields when a
//!    dedicated conflict-resolution mod is already installed.
//! 3. [`QuarkCompatQolConfig`]: strictly opt-in (every flag defaults
//!    `false`) Quark-style helpers — right-click harvest with
//!    auto-replant, downward ladder extension, and quick-stack to nearby
//!    chests with an exact item-conservation invariant.
//!
//! Clean-room note: every rule, threshold, and UI shape here is an
//! original declarative design reproducing only publicly observable
//! vanilla behaviour — no code is ported from Mojang or any third-party
//! mod.

use std::collections::BTreeMap;
use std::collections::BTreeSet;

use crate::world_extensions::SortableStack;

// ===========================================================================
// Shared bounds
// ===========================================================================

/// Maximum length of any identifier string (item id, block id, mob id,
/// recipe id) accepted anywhere in this module.
pub const MAX_ID_LEN: usize = 128;

fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= MAX_ID_LEN
}

/// Strip a `minecraft:` namespace prefix; `None` when the id carries a
/// *different* namespace (modded ids are never treated as vanilla ids).
fn vanilla_path(id: &str) -> Option<&str> {
    match id.split_once(':') {
        Some(("minecraft", path)) => Some(path),
        Some(_) => None,
        None => Some(id),
    }
}

// ===========================================================================
// 1. Vanilla calendar holiday events
// ===========================================================================

/// A month/day calendar date supplied by the caller (from the world
/// clock or an event admin command). Never read from the wall clock
/// inside this module.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct CalendarDate {
    /// Month `1..=12`.
    pub month: u8,
    /// Day `1..=31`.
    pub day: u8,
}

impl CalendarDate {
    /// Validated constructor; `None` when `month`/`day` is out of range.
    pub fn new(month: u8, day: u8) -> Option<CalendarDate> {
        let d = CalendarDate { month, day };
        d.is_valid().then_some(d)
    }

    /// `month` in `1..=12` and `day` in `1..=31` (vanilla does not
    /// distinguish month lengths, so `Feb 31` is a syntactically valid
    /// non-holiday date).
    pub fn is_valid(&self) -> bool {
        (1..=12).contains(&self.month) && (1..=31).contains(&self.day)
    }
}

/// Vanilla holiday events this module can evaluate.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum HolidayEvent {
    /// October 31 — mob headgear (`zombie`, `skeleton` families).
    Halloween,
    /// December 24–26 — chest & trapped-chest present reskin.
    Christmas,
}

/// Holiday configuration. `enabled` defaults to `true` because vanilla
/// runs these events unconditionally — opting out is the non-vanilla
/// choice and must be explicit.
#[derive(Clone, Debug, PartialEq)]
pub struct HolidayConfig {
    /// Master switch (default `true`).
    pub enabled: bool,
    /// When `Some`, that event is active regardless of the calendar date
    /// and no other event is (forced server events and deterministic
    /// tests). When `None`, the real calendar decides.
    pub force_event: Option<HolidayEvent>,
}

impl Default for HolidayConfig {
    fn default() -> Self {
        HolidayConfig {
            enabled: true,
            force_event: None,
        }
    }
}

impl HolidayConfig {
    /// Whether `event` is active on `date` under this config.
    pub fn is_event_active(&self, event: HolidayEvent, date: CalendarDate) -> bool {
        if !self.enabled || !date.is_valid() {
            return false;
        }
        match self.force_event {
            Some(forced) => forced == event,
            None => match event {
                HolidayEvent::Halloween => is_halloween(date),
                HolidayEvent::Christmas => is_christmas_window(date),
            },
        }
    }
}

/// Vanilla Halloween day: October 31.
pub fn is_halloween(date: CalendarDate) -> bool {
    date.is_valid() && date.month == 10 && date.day == 31
}

/// Vanilla Christmas chest window: December 24 through 26.
pub fn is_christmas_window(date: CalendarDate) -> bool {
    date.is_valid() && date.month == 12 && (24..=26).contains(&date.day)
}

/// Vanilla `SharedConstants.isAroundHalloween` bat window: October 20
/// through November 3. Kept bit-identical to
/// `natural_spawner::halloween_from_epoch_days`.
pub fn is_bat_halloween_window(date: CalendarDate) -> bool {
    date.is_valid() && ((date.month == 10 && date.day >= 20) || (date.month == 11 && date.day <= 3))
}

/// Outer gate of the vanilla Halloween headgear roll
/// (`random.nextFloat() < 0.25F` in `finalizeSpawn`).
pub const HALLOWEEN_HEADGEAR_ROLL_GATE: f32 = 0.25;

/// Inner jack o'lantern gate expressed against the *same* roll value:
/// vanilla draws a second `< 0.1F` roll inside the outer 25%, which is
/// equivalent to a single `roll < 0.025` (10% of 25% = 2.5% total).
pub const HALLOWEEN_JACK_O_LANTERN_ROLL: f32 = 0.025;

/// The headgear kind vanilla equips on Halloween.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum HalloweenHeadgearKind {
    /// `minecraft:carved_pumpkin` — the common outcome (22.5% total).
    CarvedPumpkin,
    /// `minecraft:jack_o_lantern` — the rare outcome (2.5% total).
    JackOLantern,
}

/// A decided Halloween headgear equip.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct HalloweenHeadgear {
    /// Which helmet item is equipped.
    pub kind: HalloweenHeadgearKind,
    /// Equipment-slot drop chance. Vanilla pins this to `0.0`
    /// (`setDropChance(EquipmentSlot.HEAD, 0.0F)`) so Halloween mobs can
    /// never be farmed for free pumpkins.
    pub head_drop_chance: f32,
}

/// Vanilla mob paths eligible for the Halloween headgear roll: the
/// zombie family (`Zombie.finalizeSpawn`) and the skeleton family
/// (`AbstractSkeleton.finalizeSpawn`).
pub const HALLOWEEN_HEADGEAR_MOBS: &[&str] = &[
    "zombie",
    "zombie_villager",
    "husk",
    "drowned",
    "skeleton",
    "stray",
    "bogged",
    "wither_skeleton",
    "zombified_piglin",
];

/// Roll the vanilla Halloween headgear equip for `mob_id` on `date`.
///
/// `roll_0_1` is the caller-supplied uniform roll in `[0.0, 1.0)`; a
/// non-finite or negative roll is treated as a miss. Returns:
///
/// - `Some(JackOLantern)` when `roll < 0.025` (2.5% total),
/// - `Some(CarvedPumpkin)` when `roll < 0.25` (22.5% total),
/// - `None` otherwise.
///
/// The result always carries `head_drop_chance == 0.0`. Only the nine
/// vanilla [`HALLOWEEN_HEADGEAR_MOBS`] qualify; modded mobs and
/// non-eligible vanilla mobs never roll. Callers must invoke this only
/// when the mob's head slot is still empty — exactly like vanilla's
/// `getItemBySlot(HEAD).isEmpty()` check inside `finalizeSpawn`.
pub fn roll_halloween_headgear(
    mob_id: &str,
    date: CalendarDate,
    cfg: &HolidayConfig,
    roll_0_1: f32,
) -> Option<HalloweenHeadgear> {
    if !cfg.is_event_active(HolidayEvent::Halloween, date) || !valid_id(mob_id) {
        return None;
    }
    let path = vanilla_path(mob_id)?;
    if !HALLOWEEN_HEADGEAR_MOBS.contains(&path) {
        return None;
    }
    let roll = if roll_0_1.is_finite() && roll_0_1 >= 0.0 {
        roll_0_1
    } else {
        return None;
    };
    let kind = if roll < HALLOWEEN_JACK_O_LANTERN_ROLL {
        HalloweenHeadgearKind::JackOLantern
    } else if roll < HALLOWEEN_HEADGEAR_ROLL_GATE {
        HalloweenHeadgearKind::CarvedPumpkin
    } else {
        return None;
    };
    Some(HalloweenHeadgear {
        kind,
        head_drop_chance: 0.0,
    })
}

/// Visual variant the client renders for a chest block.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ChestVisualVariant {
    /// Ordinary chest texture.
    Standard,
    /// Vanilla Christmas present reskin (Dec 24–26).
    ChristmasPresent,
}

/// The chest visual variant for `block_id` on `date`.
///
/// Vanilla reskins `minecraft:chest` and `minecraft:trapped_chest` into
/// presents during the Christmas window; `minecraft:ender_chest` is
/// *never* reskinned, and neither is any modded block. Anything outside
/// the window, any non-chest id, or a disabled config yields
/// [`ChestVisualVariant::Standard`].
pub fn chest_holiday_variant(
    block_id: &str,
    date: CalendarDate,
    cfg: &HolidayConfig,
) -> ChestVisualVariant {
    if !cfg.is_event_active(HolidayEvent::Christmas, date) || !valid_id(block_id) {
        return ChestVisualVariant::Standard;
    }
    match vanilla_path(block_id) {
        Some("chest") | Some("trapped_chest") => ChestVisualVariant::ChristmasPresent,
        _ => ChestVisualVariant::Standard,
    }
}

// ===========================================================================
// 2. Zero-clutter recipe conflict resolver
// ===========================================================================

/// Configuration for [`RecipeConflictResolver`].
#[derive(Clone, Debug, PartialEq)]
pub struct RecipeConflictConfig {
    /// Master switch (default `true`). When `false` the resolver is
    /// inert and no UI is ever reported.
    pub enabled: bool,
    /// `true` when a dedicated conflict-resolution mod (Polymorph-style)
    /// is installed; the resolver then auto-yields and stays
    /// [`ConflictUiVisibility::Hidden`] so the external mod owns the UI.
    pub external_conflict_mod_present: bool,
}

impl Default for RecipeConflictConfig {
    fn default() -> Self {
        RecipeConflictConfig {
            enabled: true,
            external_conflict_mod_present: false,
        }
    }
}

/// One recipe whose crafting grid matches the current input.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecipeCandidate {
    /// Fully namespaced recipe id (`"minecraft:iron_pickaxe"`).
    pub recipe_id: String,
    /// Output item id (`"minecraft:iron_pickaxe"`).
    pub output_item: String,
    /// Items produced per craft.
    pub output_count: u32,
    /// Mod (or `"minecraft"`) contributing the recipe.
    pub mod_id: String,
}

/// How much conflict-resolution UI the caller should draw.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConflictUiVisibility {
    /// Draw nothing — the indistinguishable-from-vanilla state.
    Hidden,
    /// Draw the compact `< selected/total >` switcher next to the
    /// output slot.
    CompactSwitcher { selected_index: usize, total: usize },
}

/// Deterministic content signature for a crafting grid — FNV-1a over
/// each slot's item id with a separator byte so `["ab","c"]` and
/// `["a","bc"]` differ. Used to key the per-grid preference memory.
pub fn recipe_grid_signature(slots: &[&str]) -> u64 {
    let mut hash = megu_core::hash::Fnv1a64::new();
    for slot in slots { hash.update(slot.as_bytes()); hash.update_u8(0xff); }
    hash.value()
}

fn recipe_sort_key(c: &RecipeCandidate) -> (bool, &str) {
    // Vanilla (`minecraft:`-namespaced) recipes sort first, then plain
    // lexicographic recipe_id order — deterministic across restarts.
    (
        !c.recipe_id.starts_with("minecraft:"),
        c.recipe_id.as_str(),
    )
}

/// Stateful resolver for colliding crafting recipes.
///
/// The resolver keeps the current grid's sorted candidate list and a
/// per-`grid_signature` memory of the recipe the player last picked, so
/// re-crafting the same mod item never needs a second click. It owns no
/// rendering: [`Self::ui_visibility`] is the whole contract.
#[derive(Debug)]
pub struct RecipeConflictResolver {
    config: RecipeConflictConfig,
    signature: u64,
    candidates: Vec<RecipeCandidate>,
    selected: usize,
    /// grid signature -> last-picked `recipe_id`.
    remembered: BTreeMap<u64, String>,
}

impl RecipeConflictResolver {
    /// An empty resolver under `config`.
    pub fn new(config: RecipeConflictConfig) -> Self {
        RecipeConflictResolver {
            config,
            signature: 0,
            candidates: Vec::new(),
            selected: 0,
            remembered: BTreeMap::new(),
        }
    }

    /// The active configuration.
    pub fn config(&self) -> &RecipeConflictConfig {
        &self.config
    }

    /// Replace the matching-recipe set for `signature` (computed via
    /// [`recipe_grid_signature`]). Candidates are re-sorted into the
    /// deterministic order (vanilla first, then `recipe_id`), and the
    /// selection is restored — in priority order — from the still-valid
    /// in-progress pick, then the remembered pick for this signature,
    /// then index `0`.
    pub fn set_candidates(&mut self, signature: u64, mut candidates: Vec<RecipeCandidate>) {
        candidates.sort_by(|a, b| recipe_sort_key(a).cmp(&recipe_sort_key(b)));
        let preferred = if signature == self.signature {
            self.candidates
                .get(self.selected)
                .map(|c| c.recipe_id.clone())
                .or_else(|| self.remembered.get(&signature).cloned())
        } else {
            self.remembered.get(&signature).cloned()
        };
        self.selected = preferred
            .and_then(|id| candidates.iter().position(|c| c.recipe_id == id))
            .unwrap_or(0);
        self.signature = signature;
        self.candidates = candidates;
    }

    /// Clear the grid (all slots emptied).
    pub fn clear(&mut self) {
        self.signature = 0;
        self.candidates.clear();
        self.selected = 0;
    }

    /// Sorted candidates for the current grid.
    pub fn candidates(&self) -> &[RecipeCandidate] {
        &self.candidates
    }

    /// Currently selected index into [`Self::candidates`].
    pub fn selected_index(&self) -> usize {
        self.selected
    }

    /// The recipe the craft should produce.
    pub fn selected_recipe(&self) -> Option<&RecipeCandidate> {
        self.candidates.get(self.selected)
    }

    /// The only UI decision a caller needs.
    ///
    /// [`ConflictUiVisibility::Hidden`] when the resolver is disabled,
    /// an external conflict mod is present, or fewer than two recipes
    /// match — a vanilla grid draws literally nothing. Only a genuine
    /// collision exposes the compact switcher.
    pub fn ui_visibility(&self) -> ConflictUiVisibility {
        if !self.config.enabled
            || self.config.external_conflict_mod_present
            || self.candidates.len() <= 1
        {
            return ConflictUiVisibility::Hidden;
        }
        ConflictUiVisibility::CompactSwitcher {
            selected_index: self.selected,
            total: self.candidates.len(),
        }
    }

    /// Cycle forward, wrapping past the last candidate.
    pub fn next(&mut self) {
        if self.candidates.is_empty() {
            return;
        }
        self.selected = (self.selected + 1) % self.candidates.len();
        self.record_pick();
    }

    /// Cycle backward, wrapping before the first candidate.
    pub fn prev(&mut self) {
        if self.candidates.is_empty() {
            return;
        }
        self.selected = (self.selected + self.candidates.len() - 1) % self.candidates.len();
        self.record_pick();
    }

    /// Jump to `idx` (wrapped modulo the candidate count, so callers may
    /// pass raw scroll input).
    pub fn select(&mut self, idx: usize) {
        if self.candidates.is_empty() {
            return;
        }
        self.selected = idx % self.candidates.len();
        self.record_pick();
    }

    fn record_pick(&mut self) {
        if let Some(c) = self.candidates.get(self.selected) {
            self.remembered.insert(self.signature, c.recipe_id.clone());
        }
    }
}

// ===========================================================================
// 3. Opt-in Quark-compatible QoL primitives
// ===========================================================================

/// Opt-in QoL toggles — every flag defaults to `false`, so a stock
/// server is bit-for-bit vanilla until an operator opts in.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct QuarkCompatQolConfig {
    /// Right-click a mature crop to harvest + auto-replant.
    pub right_click_harvest: bool,
    /// Right-click a ladder with a ladder to extend the column downward.
    pub ladder_extend_down: bool,
    /// One-click stack of matching inventory items into nearby chests.
    pub quick_stack_to_nearby_chests: bool,
}

/// What a successful right-click harvest does to the crop.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RightClickHarvestPlan {
    /// Age the crop block is reset to (always `0` — a fresh crop).
    pub reset_age: u8,
    /// Seed items the caller deducts from the crop's normal drop table
    /// to pay for the auto-replant (always `1` per harvested crop).
    pub seeds_deducted: u32,
}

/// Evaluate a right-click on a crop block.
///
/// Returns `Some` only when `cfg.right_click_harvest` is on, `block_id`
/// is a well-formed id, and the crop is mature (`age >= max_age` with
/// `max_age >= 1`). The plan says "reset to `reset_age` and drop the
/// normal loot table minus `seeds_deducted` seeds" — the caller runs
/// the actual block-state write and loot roll. Immature crops and the
/// disabled flag return `None`, so the right-click then falls through
/// to ordinary interaction.
pub fn evaluate_right_click_harvest(
    block_id: &str,
    age: u8,
    max_age: u8,
    cfg: &QuarkCompatQolConfig,
) -> Option<RightClickHarvestPlan> {
    if !cfg.right_click_harvest || !valid_id(block_id) || max_age == 0 || age < max_age {
        return None;
    }
    Some(RightClickHarvestPlan {
        reset_age: 0,
        seeds_deducted: 1,
    })
}

/// Maximum cells [`evaluate_ladder_extend_down`] scans below the click.
pub const LADDER_EXTEND_MAX_DEPTH: u32 = 64;

/// How one cell below the clicked ladder reads for downward extension.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LadderColumnCell {
    /// Another ladder — the column continues, keep scanning down.
    Ladder,
    /// Open air with a support face that can hold a ladder — the target.
    SupportedAir,
    /// Solid block, unsupported air, fluid, or anything else — the
    /// column ends here.
    Blocked,
}

/// Find the first open air cell directly below the ladder column under
/// `clicked_y`, at most [`LADDER_EXTEND_MAX_DEPTH`] cells down.
///
/// `probe_below` classifies each cell by Y: scanning skips existing
/// [`LadderColumnCell::Ladder`] rungs, lands on the first
/// [`LadderColumnCell::SupportedAir`], and stops (returning `None`) on
/// the first [`LadderColumnCell::Blocked`] cell — a solid floor, an
/// unsupported gap, or an exhausted scan. Returns `None` when the
/// feature is disabled.
pub fn evaluate_ladder_extend_down(
    clicked_y: i64,
    mut probe_below: impl FnMut(i64) -> LadderColumnCell,
    cfg: &QuarkCompatQolConfig,
) -> Option<i64> {
    if !cfg.ladder_extend_down {
        return None;
    }
    for depth in 1..=i64::from(LADDER_EXTEND_MAX_DEPTH) {
        match probe_below(clicked_y - depth) {
            LadderColumnCell::Ladder => continue,
            LadderColumnCell::SupportedAir => return Some(clicked_y - depth),
            LadderColumnCell::Blocked => return None,
        }
    }
    None
}

/// One planned item movement inside [`QuickStackPlan`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QuickStackMove {
    /// Index into the player inventory the items leave.
    pub from_slot: usize,
    /// Index into `nearby_chests` receiving the items.
    pub to_chest: usize,
    /// Slot inside that chest the items merge onto.
    pub to_chest_slot: usize,
    /// Item being moved (with the same `component_hash` — moves only
    /// ever merge fully identical stacks).
    pub item_id: String,
    /// Items moved by this step (`>= 1`).
    pub count: u32,
}

/// A validated quick-stack plan: the moves to replay, in order.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct QuickStackPlan {
    /// Ordered moves; empty means "nothing stackable nearby".
    pub moves: Vec<QuickStackMove>,
}

impl QuickStackPlan {
    /// `true` when no items can be stacked.
    pub fn is_empty(&self) -> bool {
        self.moves.is_empty()
    }

    /// Total items moved across every step.
    pub fn total_moved(&self) -> u64 {
        self.moves.iter().map(|m| u64::from(m.count)).sum()
    }

    /// Replay the plan against live slots. Every step is re-validated
    /// (source stack still present and large enough, destination still
    /// the same item below its cap); any mismatch aborts with `false`
    /// *before* that step — earlier steps remain applied. Conservation
    /// is then verified in debug builds.
    pub fn apply(
        &self,
        player_inv: &mut [Option<SortableStack>],
        nearby_chests: &mut [Vec<Option<SortableStack>>],
    ) -> bool {
        for m in &self.moves {
            let src_ok = player_inv
                .get(m.from_slot)
                .and_then(|s| s.as_ref())
                .is_some_and(|s| s.item_id == m.item_id && s.count >= m.count);
            let dst_ok = nearby_chests
                .get(m.to_chest)
                .and_then(|c| c.get(m.to_chest_slot))
                .and_then(|s| s.as_ref())
                .is_some_and(|s| {
                    s.item_id == m.item_id
                        && u64::from(s.count) + u64::from(m.count)
                            <= u64::from(s.max_stack_size.max(1))
                });
            if !src_ok || !dst_ok {
                return false;
            }
            if let Some(src) = player_inv[m.from_slot].as_mut() {
                src.count -= m.count;
                if src.count == 0 {
                    player_inv[m.from_slot] = None;
                }
            }
            if let Some(dst) = nearby_chests[m.to_chest][m.to_chest_slot].as_mut() {
                dst.count += m.count;
            }
        }
        true
    }
}

/// Plan a quick-stack to nearby chests, Quark-style.
///
/// Only player slots *not* in `locked_slots` are touched — the caller
/// marks the hotbar (and any pinned slots) locked. An item moves only
/// onto chest stacks that already hold the *identical* item (same
/// `item_id` and `component_hash`), filling each existing stack up to
/// `max_stack_size` in deterministic chest-then-slot order. Nothing is
/// ever placed into an empty chest cell, and the sum of `count` per
/// item across inventory + chests is bit-for-bit conserved.
pub fn plan_quick_stack(
    player_inv: &[Option<SortableStack>],
    nearby_chests: &[Vec<Option<SortableStack>>],
    locked_slots: &BTreeSet<usize>,
    cfg: &QuarkCompatQolConfig,
) -> QuickStackPlan {
    if !cfg.quick_stack_to_nearby_chests {
        return QuickStackPlan::default();
    }

    // Mutable copy of every chest stack's remaining capacity, keyed by
    // the original chest slot index, so planned moves compose
    // deterministically without touching live state.
    let mut capacity: Vec<Vec<(usize, u32)>> = nearby_chests
        .iter()
        .map(|chest| {
            chest
                .iter()
                .enumerate()
                .filter_map(|(i, slot)| {
                    slot.as_ref().map(|s| {
                        (i, s.max_stack_size.max(1).saturating_sub(s.count))
                    })
                })
                .collect()
        })
        .collect();

    let mut moves = Vec::new();
    for (slot_i, slot) in player_inv.iter().enumerate() {
        let Some(stack) = slot else { continue };
        if locked_slots.contains(&slot_i) || !valid_id(&stack.item_id) || stack.count == 0 {
            continue;
        }
        let mut remaining = stack.count;
        for (chest_i, cells) in capacity.iter_mut().enumerate() {
            for (chest_slot_i, free) in cells.iter_mut() {
                if remaining == 0 || *free == 0 {
                    continue;
                }
                let dst = nearby_chests[chest_i][*chest_slot_i]
                    .as_ref()
                    .expect("capacity cell always maps to a live slot");
                if dst.item_id != stack.item_id || dst.component_hash != stack.component_hash {
                    continue;
                }
                let take = remaining.min(*free);
                moves.push(QuickStackMove {
                    from_slot: slot_i,
                    to_chest: chest_i,
                    to_chest_slot: *chest_slot_i,
                    item_id: stack.item_id.clone(),
                    count: take,
                });
                *free -= take;
                remaining -= take;
            }
            if remaining == 0 {
                break;
            }
        }
    }
    QuickStackPlan { moves }
}
