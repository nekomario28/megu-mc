// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later

//! Clean-room, zero-dependency, fully generic culinary-mod engine for the
//! Farmer's Delight (`farmersdelight`) mechanic family — and, crucially, for
//! every data-driven addon built on it (`Nether's Delight`, `Ocean's
//! Delight`, `End's Delight`, `Cultural Delights`, `Delightful`,
//! `Brewin' and Chewin'`, `Chef's Delight`, …) without a single hardcoded
//! addon id anywhere in this file.
//!
//! The whole ecosystem is ~95 % pure JSON on five small mechanics, all of
//! which are implemented here from scratch:
//!
//! 1. **Heated multi-input cooking pot** — any `*:cooking` or
//!    `*:fermenting` recipe JSON found under `data/<ns>/recipe{s}/` is
//!    parsed into a [`CookingRecipe`] with up to six unordered ingredient
//!    slots, a container requirement (bowl, bottle, …), a result stack, a
//!    cook time, and an experience yield.
//! 2. **Tool-action cutting board** — any `*:cutting` recipe JSON becomes a
//!    [`CuttingRecipe`] whose [`ToolMatcher`] accepts item ids, item tags
//!    (`#c:tools/knife`), or free-form tool actions (`axe_strip`,
//!    `shears_dig`, `knife`) and whose outputs carry fractional `chance`
//!    rolls resolved by a deterministic [`Xoroshiro128pp`] PRNG.
//! 3. **Sleeping-timer execution** — [`CookingPotState::is_sleeping`] is
//!    true whenever the pot is unheated, has no matching recipe, or its
//!    output is full, and [`CookingPotState::advance_to`] jumps straight to
//!    `next_completion_tick`, so idle pots cost exactly zero per-tick CPU.
//! 4. **Global loot modifiers** — `data/<ns>/loot_modifiers/*.json`
//!    `*:add_item` and `*:add_table` rules with `loot_table_id` /
//!    `match_tool` conditions become [`LootModifierRule`]s evaluated by
//!    [`CulinaryRegistry::evaluate_loot_modifiers`].
//! 5. **Data-driven food specs** — `data/<ns>/food{s}/*.json` files parse
//!    into [`FoodSpec`] (nutrition, saturation, `using_converts_to`
//!    container return, timed effects).
//!
//! Clean-room note: nothing here copies, links, or derives from any
//! third-party mod's code or data. Only user-supplied JAR bytes are read,
//! and only the declarative JSON shape described above is interpreted.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use megu_core::perf_kit::{FingerprintBuilder, FingerprintStats};
use megu_core::random::Xoroshiro128pp;

use crate::data_loader::{parse_json, DataLoadError, JsonValue};
use crate::mod_tick_runtime::ModTickRuntime;
use crate::modpack;

/// Cumulative decompressed-byte budget for one JAR scan, shared with the
/// mixin scanner's policy so a hostile archive cannot OOM the host.
const MAX_CULINARY_SCAN_BYTES: u64 = 256 * 1024 * 1024;

/// Maximum stack size a cooking-pot output slot accepts.
const MAX_STACK: u16 = 64;

/// Input capacity of the cooking pot block entity.
pub const POT_INPUT_SLOTS: usize = 6;

/// Vanilla default `cookingtime` when a recipe omits it.
const DEFAULT_COOKING_TIME: u32 = 200;

/// Random-tick growth speed-up rich soil grants an affected crop.
pub const RICH_SOIL_GROWTH_MULTIPLIER: u32 = 2;

/// Item/block tag contents keyed by `"namespace:path"`. Values may contain
/// nested `"#namespace:path"` references, resolved by [`tag_contains`].
pub type TagIndex = BTreeMap<String, BTreeSet<String>>;

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Failure of [`CulinaryRegistry::load_from_jar_bytes`]. Fail-closed: a
/// corrupt archive or a malformed JSON member aborts the whole load rather
/// than leaving a half-populated registry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CulinaryError {
    /// The JAR itself is not a readable archive (bad EOCD, truncated
    /// member, CRC mismatch, ZIP64, encryption, unknown method).
    Archive(String),
    /// A scanned `*.json` member did not parse.
    Json(String),
    /// A scanned member decoded to valid-but-unsound data (for example an
    /// ingredient array that is not an array).
    Malformed(String),
}

impl fmt::Display for CulinaryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Archive(m) | Self::Json(m) | Self::Malformed(m) => f.write_str(m),
        }
    }
}

impl std::error::Error for CulinaryError {}

impl From<crate::modpack::ModpackError> for CulinaryError {
    fn from(e: crate::modpack::ModpackError) -> Self {
        Self::Archive(e.reason().to_string())
    }
}

impl From<DataLoadError> for CulinaryError {
    fn from(e: DataLoadError) -> Self {
        let msg = match e {
            DataLoadError::Io(m) | DataLoadError::Json(m) | DataLoadError::Zip(m) => m,
        };
        Self::Json(msg)
    }
}

fn malformed(msg: impl Into<String>) -> CulinaryError {
    CulinaryError::Malformed(msg.into())
}

// ---------------------------------------------------------------------------
// ID helpers
// ---------------------------------------------------------------------------

/// Prepend `minecraft:` to a bare path id; namespaced ids pass through.
pub(crate) fn normalize_id(raw: &str) -> String {
    let raw = raw.trim();
    if raw.contains(':') {
        raw.to_string()
    } else {
        format!("minecraft:{raw}")
    }
}

/// Strip a `[k=v]` blockstate suffix, returning `(id, state_suffix)`.
fn split_state(block: &str) -> (&str, &str) {
    let block = block.trim();
    match block.find('[') {
        Some(i) => (&block[..i], &block[i..]),
        None => (block, ""),
    }
}

/// Depth-bounded nested-tag membership test. Members prefixed `#` recurse
/// into another tag id; cycles are cut by the `visited` set.
pub fn tag_contains(tags: &TagIndex, tag_id: &str, item: &str) -> bool {
    let tag_id = tag_id.strip_prefix('#').unwrap_or(tag_id);
    let mut visited: BTreeSet<String> = BTreeSet::new();
    let mut stack = vec![tag_id.to_string()];
    while let Some(current) = stack.pop() {
        if !visited.insert(current.clone()) {
            continue;
        }
        let Some(members) = tags.get(&current) else {
            continue;
        };
        for member in members {
            if let Some(nested) = member.strip_prefix('#') {
                stack.push(normalize_id(nested));
            } else if *member == item || normalize_id(member) == item {
                return true;
            }
        }
    }
    false
}

// ---------------------------------------------------------------------------
// Matchers
// ---------------------------------------------------------------------------

/// One recipe ingredient slot. `AnyOf` covers the NeoForge compound
/// ingredient and JSON-array spellings.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IngredientMatcher {
    /// Exact `"namespace:item"` id.
    Item(String),
    /// Everything in `"namespace:tag"` (nested `#` refs included).
    Tag(String),
    /// Any sub-matcher satisfies the slot.
    AnyOf(Vec<IngredientMatcher>),
}

impl IngredientMatcher {
    /// Does `item` satisfy this ingredient under `tags`?
    pub fn matches(&self, item: &str, tags: &TagIndex) -> bool {
        let item = normalize_id(item);
        match self {
            Self::Item(id) => *id == item,
            Self::Tag(tag) => tag_contains(tags, tag, &item),
            Self::AnyOf(any) => any.iter().any(|m| m.matches(&item, tags)),
        }
    }
}

/// What a [`CuttingRecipe`] or [`LootModifierRule`] requires in hand.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ToolMatcher {
    /// Exact `"namespace:item"` id.
    Item(String),
    /// Membership in `"namespace:tag"` (e.g. `c:tools/knife`).
    Tag(String),
    /// A free-form tool action such as `"axe_strip"`, `"shears_dig"`, or
    /// `"knife"`, matched by tag convention or tool-name family.
    ToolAction(String),
}

impl ToolMatcher {
    /// Does `tool` satisfy this matcher under `tags`?
    pub fn matches(&self, tool: &str, tags: &TagIndex) -> bool {
        let tool = normalize_id(tool);
        match self {
            Self::Item(id) => *id == tool,
            Self::Tag(tag) => tag_contains(tags, tag, &tool),
            Self::ToolAction(action) => tool_action_matches(action, &tool, tags),
        }
    }
}

/// Clean-room tool-action resolution. A tool satisfies an action when it
/// sits in `c:tools/<action>` or `c:tools/<family>` (family = leading
/// `snake` token, e.g. `axe` for `axe_strip`) or when its id's path tokens
/// include the family (`iron_axe` → `axe`, `flint_knife` → `knife`).
fn tool_action_matches(action: &str, tool: &str, tags: &TagIndex) -> bool {
    let family = action.split('_').next().unwrap_or(action);
    for cand in [
        format!("c:tools/{action}"),
        format!("c:tools/{family}"),
        format!("farmersdelight:tools/{action}"),
    ] {
        if tag_contains(tags, &cand, tool) {
            return true;
        }
    }
    let path = tool.rsplit(':').next().unwrap_or(tool);
    path.split('_').any(|token| token == family)
}

// ---------------------------------------------------------------------------
// Data model
// ---------------------------------------------------------------------------

/// A heated multi-input pot recipe (`*:cooking`, `*:fermenting`).
#[derive(Clone, Debug, PartialEq)]
pub struct CookingRecipe {
    /// `"namespace:path"` derived from the recipe file location.
    pub id: String,
    /// Full recipe type string, e.g. `"farmersdelight:cooking"`.
    pub recipe_type: String,
    /// 1..=6 unordered ingredient slots (multiset — duplicates allowed).
    pub ingredients: Vec<IngredientMatcher>,
    /// Required container item (`minecraft:bowl`, glass bottle, …).
    pub container: Option<String>,
    /// Result item id.
    pub result_item: String,
    /// Result stack size per completion.
    pub result_count: u16,
    /// Ticks until completion once armed (default 200).
    pub cooking_time: u32,
    /// Experience emitted per completion.
    pub experience: f32,
}

/// One weighted output of a [`CuttingRecipe`].
#[derive(Clone, Debug, PartialEq)]
pub struct CuttingOutput {
    /// Output item id.
    pub item: String,
    /// Stack size.
    pub count: u16,
    /// Drop probability in `[0, 1]` (default `1.0`).
    pub chance: f32,
}

/// A tool-action board recipe (`*:cutting`).
#[derive(Clone, Debug, PartialEq)]
pub struct CuttingRecipe {
    /// `"namespace:path"` derived from the recipe file location.
    pub id: String,
    /// Single input slot (alternatives collapse into `AnyOf`).
    pub ingredient: IngredientMatcher,
    /// Required hand tool.
    pub tool: ToolMatcher,
    /// Ordered weighted outputs.
    pub results: Vec<CuttingOutput>,
    /// Optional sound id.
    pub sound: Option<String>,
}

/// A global loot modifier (`*:add_item` / `*:add_table`).
#[derive(Clone, Debug, PartialEq)]
pub struct LootModifierRule {
    /// `"namespace:path"` derived from the modifier file location.
    pub id: String,
    /// Target loot table id (or `*` suffix pattern); `None` = every table.
    pub target_loot_table: Option<String>,
    /// Required held tool (e.g. `#c:tools/knife`); `None` = no tool gate.
    pub required_tool: Option<ToolMatcher>,
    /// Item directly added to the drop list.
    pub add_item: Option<String>,
    /// Sub-table injected into the drop list.
    pub add_table: Option<String>,
    /// Application probability in `[0, 1]` (default `1.0`).
    pub chance: f32,
}

/// Data-driven `FoodComponent` equivalent: nutrition, saturation,
/// `using_converts_to` container return, and timed effects as
/// `(effect_id, duration_ticks, amplifier, probability)`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct FoodSpec {
    /// Hunger shanks restored (vanilla `nutrition`).
    pub nutrition: u16,
    /// Saturation restored (already vanilla-multiplied where supplied).
    pub saturation: f32,
    /// Item left in hand after eating (`using_converts_to`).
    pub using_converts_to: Option<String>,
    /// `(effect_id, duration_ticks, amplifier, probability)` tuples.
    pub effects: Vec<(String, u32, u8, f32)>,
}

/// What one cooking-pot completion produced.
#[derive(Clone, Debug, PartialEq)]
pub struct CookResult {
    /// Completed recipe id.
    pub recipe_id: String,
    /// Emitted item id.
    pub result_item: String,
    /// Emitted stack size.
    pub result_count: u16,
    /// Experience emitted.
    pub experience: f32,
    /// Tick the recipe completed on.
    pub completed_tick: u64,
}

/// Result of [`CulinaryRegistry::execute_cutting`].
#[derive(Clone, Debug, PartialEq)]
pub struct CuttingOutcome {
    /// Recipe that matched.
    pub recipe_id: String,
    /// `(item, count)` pairs that survived their `chance` rolls.
    pub outputs: Vec<(String, u16)>,
    /// Optional sound id carried through from the recipe.
    pub sound: Option<String>,
}

/// One drop injected by [`CulinaryRegistry::evaluate_loot_modifiers`].
#[derive(Clone, Debug, PartialEq)]
pub struct LootAddition {
    /// Rule that fired.
    pub rule_id: String,
    /// Direct item drop, when the rule adds one.
    pub item: Option<String>,
    /// Injected sub-table, when the rule adds one.
    pub table: Option<String>,
}

// ---------------------------------------------------------------------------
// Cooking pot simulator — sleeping-timer contract
// ---------------------------------------------------------------------------

/// `(item_id, count)` stack in a pot slot.
pub type SlotStack = (String, u16);

/// Six-input heated pot. The pot does **no** per-tick work: state changes
/// call [`refresh`](Self::refresh), and [`advance_to`](Self::advance_to)
/// either sleeps in O(1) or jumps directly to `next_completion_tick`.
#[derive(Clone, Debug)]
pub struct CookingPotState {
    /// Six input slots; `None` = empty. Order is irrelevant — recipe
    /// matching is exact-unordered (bipartite).
    pub inputs: [Option<SlotStack>; POT_INPUT_SLOTS],
    /// Container slot (`minecraft:bowl`, …).
    pub container: Option<SlotStack>,
    /// Output slot accumulating completed results.
    pub output: Option<SlotStack>,
    /// Whether a heat source is underneath (`is_heated`).
    pub heated: bool,
    /// Ticks of progress already banked toward `active_recipe`; survives
    /// unheated pauses, reset whenever the matched recipe changes.
    pub progress_ticks: u32,
    /// Index into [`CulinaryRegistry::cooking_recipes`] currently armed.
    pub active_recipe: Option<usize>,
    /// Absolute tick the armed recipe finishes on; `None` while sleeping.
    pub next_completion_tick: Option<u64>,
    /// Slot indices assigned to `active_recipe`'s ingredients.
    matched_slots: Vec<usize>,
}

impl Default for CookingPotState {
    fn default() -> Self {
        Self {
            inputs: std::array::from_fn(|_| None),
            container: None,
            output: None,
            heated: false,
            progress_ticks: 0,
            active_recipe: None,
            next_completion_tick: None,
            matched_slots: Vec::new(),
        }
    }
}

impl CookingPotState {
    /// Empty, unheated pot.
    pub fn new() -> Self {
        Self::default()
    }

    /// Place `(item, count)` into the first free input slot. Returns
    /// `false` when all six slots are occupied.
    pub fn insert_input(&mut self, item: &str, count: u16) -> bool {
        for slot in &mut self.inputs {
            if slot.is_none() {
                *slot = Some((normalize_id(item), count));
                return true;
            }
        }
        false
    }

    /// `true` when the pot consumes zero CPU: unheated, no armed recipe
    /// (no match, missing container, or blocked output).
    pub fn is_sleeping(&self) -> bool {
        !self.heated || self.next_completion_tick.is_none()
    }

    /// Would `recipe`'s result fit the output slot right now?
    fn output_accepts(&self, recipe: &CookingRecipe) -> bool {
        match &self.output {
            None => true,
            Some((id, count)) => {
                *id == recipe.result_item && *count + recipe.result_count <= MAX_STACK
            }
        }
    }

    /// Re-evaluate the world after an input, container, heat, or output
    /// change. Arms `next_completion_tick` when heated and a recipe
    /// matches; otherwise leaves the pot sleeping. Resuming the same
    /// recipe after a pause keeps banked `progress_ticks`.
    pub fn refresh(&mut self, registry: &CulinaryRegistry, now: u64) {
        let container_id = self.container.as_ref().map(|(id, _)| id.as_str());
        let found = registry
            .find_cooking_match(&self.inputs, container_id)
            .filter(|(idx, _)| self.output_accepts(&registry.cooking_recipes[*idx]));
        match found {
            Some((idx, slots)) => {
                if self.active_recipe != Some(idx) {
                    self.progress_ticks = 0;
                }
                self.active_recipe = Some(idx);
                self.matched_slots = slots;
                let recipe = &registry.cooking_recipes[idx];
                self.next_completion_tick = if self.heated {
                    let remaining = recipe.cooking_time.saturating_sub(self.progress_ticks);
                    Some(now + u64::from(remaining))
                } else {
                    None
                };
            }
            None => {
                self.active_recipe = None;
                self.matched_slots.clear();
                self.progress_ticks = 0;
                self.next_completion_tick = None;
            }
        }
    }

    /// Jump the pot to `now`. O(1) always: while sleeping — or before
    /// `next_completion_tick` — nothing happens; on or after it the
    /// completion fires, slots are consumed, the result lands in
    /// `output`, and the pot immediately re-arms (or sleeps).
    pub fn advance_to(&mut self, now: u64, registry: &CulinaryRegistry) -> Option<CookResult> {
        if self.is_sleeping() {
            return None;
        }
        let idx = self.active_recipe?;
        let next = self.next_completion_tick?;
        let recipe = &registry.cooking_recipes[idx];
        if now < next {
            self.progress_ticks = recipe
                .cooking_time
                .saturating_sub((next - now).min(u64::from(u32::MAX)) as u32);
            return None;
        }

        let result = CookResult {
            recipe_id: recipe.id.clone(),
            result_item: recipe.result_item.clone(),
            result_count: recipe.result_count,
            experience: recipe.experience,
            completed_tick: next,
        };
        self.progress_ticks = recipe.cooking_time;

        // Consume one unit from every matched input slot.
        let slots = self.matched_slots.clone();
        for slot in slots {
            if let Some((_, count)) = &mut self.inputs[slot] {
                *count = count.saturating_sub(1);
                if *count == 0 {
                    self.inputs[slot] = None;
                }
            }
        }
        // Consume one container when the recipe requires it.
        if recipe.container.is_some() {
            if let Some((_, count)) = &mut self.container {
                *count = count.saturating_sub(1);
                if *count == 0 {
                    self.container = None;
                }
            }
        }
        // Emit into the output slot.
        match &mut self.output {
            Some((id, count)) if *id == result.result_item => {
                *count = (*count).saturating_add(result.result_count).min(MAX_STACK);
            }
            _ => {
                self.output = Some((result.result_item.clone(), result.result_count));
            }
        }

        // Re-arm for another craft or go back to sleep. A fresh craft
        // starts from zero progress regardless of what just finished.
        self.progress_ticks = 0;
        self.refresh(registry, next);
        Some(result)
    }
}

// ---------------------------------------------------------------------------
// Heat & soil environment
// ---------------------------------------------------------------------------

/// Is the block directly under a pot/stove a heat source?
///
/// Direct sources: `campfire`/`soul_campfire`/`fire`/`soul_fire`/
/// `magma_block`/`lava`, everything in `#farmersdelight:heat_sources`, and
/// `farmersdelight:stove[lit=true]`. A block in
/// `#farmersdelight:heat_conductors` conducts heat from a direct source
/// one block further down (`block_two_below`).
pub fn is_heated(block_below: &str, block_two_below: Option<&str>, tags: &TagIndex) -> bool {
    fn direct_source(id: &str, state: &str, tags: &TagIndex) -> bool {
        // An explicitly unlit campfire or stove emits no heat.
        if state.contains("lit=false") {
            return false;
        }
        if id == "farmersdelight:stove" {
            return state.contains("lit=true");
        }
        const BUILTIN_HEAT: &[&str] = &[
            "minecraft:campfire",
            "minecraft:soul_campfire",
            "minecraft:fire",
            "minecraft:soul_fire",
            "minecraft:magma_block",
            "minecraft:lava",
            "minecraft:lava_cauldron",
        ];
        BUILTIN_HEAT.contains(&id) || tag_contains(tags, "farmersdelight:heat_sources", id)
    }

    let (id, state) = split_state(block_below);
    let id = normalize_id(id);
    if direct_source(&id, state, tags) {
        return true;
    }
    if tag_contains(tags, "farmersdelight:heat_conductors", &id) {
        if let Some(below2) = block_two_below {
            let (id2, state2) = split_state(below2);
            return direct_source(&normalize_id(id2), state2, tags);
        }
    }
    false
}

/// Random-tick growth speed for `crop_block` planted on `soil_block`:
/// [`RICH_SOIL_GROWTH_MULTIPLIER`] when the soil is in
/// `#farmersdelight:rich_soil` and the crop is not in
/// `#farmersdelight:unaffected_by_rich_soil`; `1` otherwise.
pub fn rich_soil_growth_multiplier(soil_block: &str, crop_block: &str, tags: &TagIndex) -> u32 {
    let (soil, _) = split_state(soil_block);
    let (crop, _) = split_state(crop_block);
    let soil = normalize_id(soil);
    let crop = normalize_id(crop);
    if !tag_contains(tags, "farmersdelight:rich_soil", &soil) {
        return 1;
    }
    if tag_contains(tags, "farmersdelight:unaffected_by_rich_soil", &crop) {
        return 1;
    }
    RICH_SOIL_GROWTH_MULTIPLIER
}

// ---------------------------------------------------------------------------
// Exact-unordered ingredient matching
// ---------------------------------------------------------------------------

/// Bipartite backtracking assignment: each `ingredient` must take one
/// distinct slot. Returns the slot indices in ingredient order. Greedy
/// left-to-right matching fails on overlapping matchers (a tag that also
/// contains the item a later ingredient needs), so we backtrack.
fn match_ingredients(
    slots: &[&str],
    ingredients: &[IngredientMatcher],
    tags: &TagIndex,
) -> Option<Vec<usize>> {
    if slots.len() != ingredients.len() {
        return None;
    }
    let mut assignment = vec![usize::MAX; ingredients.len()];
    let mut used = vec![false; slots.len()];

    fn backtrack(
        ing_idx: usize,
        slots: &[&str],
        ingredients: &[IngredientMatcher],
        tags: &TagIndex,
        used: &mut [bool],
        assignment: &mut [usize],
    ) -> bool {
        if ing_idx == ingredients.len() {
            return true;
        }
        for (slot_idx, slot_item) in slots.iter().enumerate() {
            if used[slot_idx] || !ingredients[ing_idx].matches(slot_item, tags) {
                continue;
            }
            used[slot_idx] = true;
            assignment[ing_idx] = slot_idx;
            if backtrack(ing_idx + 1, slots, ingredients, tags, used, assignment) {
                return true;
            }
            used[slot_idx] = false;
            assignment[ing_idx] = usize::MAX;
        }
        false
    }

    backtrack(0, slots, ingredients, tags, &mut used, &mut assignment).then_some(assignment)
}

// ---------------------------------------------------------------------------
// JAR member classification & JSON parsing
// ---------------------------------------------------------------------------

/// Which `data/<ns>/` subtree a member belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DataKind {
    Recipe,
    ItemTag,
    BlockTag,
    LootModifier,
    FoodSpec,
}

/// `data/<ns>/<sub>/<rel>.json` → `(namespace, kind, rel-path-without-.json)`.
fn classify_member(name: &str) -> Option<(&str, DataKind, &str)> {
    let rest = name.strip_prefix("data/")?;
    let (namespace, rest) = rest.split_once('/')?;
    if namespace.is_empty() || !name.ends_with(".json") {
        return None;
    }
    let (kind, sub) = if let Some(r) = rest
        .strip_prefix("recipe/")
        .or_else(|| rest.strip_prefix("recipes/"))
    {
        (DataKind::Recipe, r)
    } else if let Some(r) = rest
        .strip_prefix("tags/item/")
        .or_else(|| rest.strip_prefix("tags/items/"))
    {
        (DataKind::ItemTag, r)
    } else if let Some(r) = rest
        .strip_prefix("tags/block/")
        .or_else(|| rest.strip_prefix("tags/blocks/"))
    {
        (DataKind::BlockTag, r)
    } else if let Some(r) = rest
        .strip_prefix("loot_modifiers/")
        .or_else(|| rest.strip_prefix("loot_modifier/"))
    {
        (DataKind::LootModifier, r)
    } else {
        let r = rest
            .strip_prefix("food/")
            .or_else(|| rest.strip_prefix("foods/"))?;
        (DataKind::FoodSpec, r)
    };
    let rel = sub.strip_suffix(".json")?;
    if rel.is_empty() {
        return None;
    }
    Some((namespace, kind, rel))
}

/// Parse one ingredient JSON value (string / `item` / `tag` / array /
/// compound-object spellings). `None` = unparseable, caller skips.
fn parse_ingredient(v: &JsonValue) -> Option<IngredientMatcher> {
    match v {
        JsonValue::String(s) => {
            let s = s.trim();
            if let Some(tag) = s.strip_prefix('#') {
                Some(IngredientMatcher::Tag(normalize_id(tag)))
            } else {
                Some(IngredientMatcher::Item(normalize_id(s)))
            }
        }
        JsonValue::Array(items) => {
            if items.is_empty() {
                return None;
            }
            let subs: Vec<_> = items.iter().map(parse_ingredient).collect::<Option<_>>()?;
            Some(IngredientMatcher::AnyOf(subs))
        }
        JsonValue::Object(o) => {
            if let Some(tag) = o.get("tag").and_then(JsonValue::as_str) {
                return Some(IngredientMatcher::Tag(normalize_id(
                    tag.trim_start_matches('#'),
                )));
            }
            if let Some(item) = o.get("item").and_then(JsonValue::as_str) {
                return Some(IngredientMatcher::Item(normalize_id(
                    item.trim_start_matches('#'),
                )));
            }
            // NeoForge compound ingredient: {"type":"…:compound","children":[…]}
            // (some addons spell the list key "ingredients" instead).
            for key in ["children", "ingredients", "items"] {
                if let Some(list) = o.get(key).and_then(JsonValue::as_array) {
                    if !list.is_empty() {
                        let subs: Vec<_> =
                            list.iter().map(parse_ingredient).collect::<Option<_>>()?;
                        return Some(IngredientMatcher::AnyOf(subs));
                    }
                }
            }
            None
        }
        _ => None,
    }
}

/// `{"id":"…","count":n}` / `{"item":"…"}` / `"…"` → `(item_id, count)`.
fn parse_item_stack(v: &JsonValue) -> Option<(String, u16)> {
    match v {
        JsonValue::String(s) => Some((normalize_id(s), 1)),
        JsonValue::Object(o) => {
            let id = o
                .get("id")
                .or_else(|| o.get("item"))
                .and_then(JsonValue::as_str)?;
            let count = o.get("count").and_then(JsonValue::as_u64).unwrap_or(1);
            Some((normalize_id(id), u16::try_from(count).ok()?))
        }
        _ => None,
    }
}

/// `"item"`/`"id"` container field → normalized item id.
fn parse_container(v: &JsonValue) -> Option<String> {
    match v {
        JsonValue::String(s) => Some(normalize_id(s)),
        JsonValue::Object(o) => o
            .get("id")
            .or_else(|| o.get("item"))
            .and_then(JsonValue::as_str)
            .map(normalize_id),
        _ => None,
    }
}

/// Tool field of a cutting recipe: string, `{"item"}`, `{"tag"}`, or
/// `{"type":"…:item_ability"/"…:tool_action","action":"…"}`.
fn parse_tool_matcher(v: &JsonValue) -> Option<ToolMatcher> {
    match v {
        JsonValue::String(s) => {
            let s = s.trim();
            if let Some(tag) = s.strip_prefix('#') {
                Some(ToolMatcher::Tag(normalize_id(tag)))
            } else {
                Some(ToolMatcher::Item(normalize_id(s)))
            }
        }
        JsonValue::Object(o) => {
            if let Some(action) = o.get("action").and_then(JsonValue::as_str) {
                return Some(ToolMatcher::ToolAction(action.to_string()));
            }
            if let Some(tag) = o.get("tag").and_then(JsonValue::as_str) {
                return Some(ToolMatcher::Tag(normalize_id(tag.trim_start_matches('#'))));
            }
            if let Some(item) = o.get("item").and_then(JsonValue::as_str) {
                return Some(ToolMatcher::Item(normalize_id(
                    item.trim_start_matches('#'),
                )));
            }
            // {"type":"…:item_ability"/"…:tool_action","tool":{…}} nests.
            if let Some(nested) = o.get("tool") {
                return parse_tool_matcher(nested);
            }
            None
        }
        _ => None,
    }
}

/// The type path (`namespace:path` → `path`) a JSON `"type"` string names.
fn type_path(type_str: &str) -> &str {
    type_str.rsplit(':').next().unwrap_or(type_str)
}

/// Parse a `*:cooking` / `*:fermenting` recipe. `None` = not a pot recipe
/// (wrong shape — foreign schema, skipped cleanly).
fn parse_cooking_recipe(
    id: &str,
    recipe_type: &str,
    obj: &BTreeMap<String, JsonValue>,
) -> Option<CookingRecipe> {
    let ingredients_json = obj.get("ingredients").and_then(JsonValue::as_array)?;
    if ingredients_json.is_empty() {
        return None;
    }
    let ingredients: Vec<_> = ingredients_json
        .iter()
        .map(parse_ingredient)
        .collect::<Option<_>>()?;
    if ingredients.is_empty() {
        return None;
    }
    let (result_item, result_count) = parse_item_stack(obj.get("result")?)?;
    let container = obj.get("container").and_then(parse_container);
    let cooking_time = obj
        .get("cookingtime")
        .or_else(|| obj.get("cooking_time"))
        .and_then(JsonValue::as_u64)
        .map(|v| u32::try_from(v).unwrap_or(DEFAULT_COOKING_TIME))
        .unwrap_or(DEFAULT_COOKING_TIME);
    let experience = obj
        .get("experience")
        .and_then(JsonValue::as_f64)
        .unwrap_or(0.0) as f32;
    Some(CookingRecipe {
        id: id.to_string(),
        recipe_type: recipe_type.to_string(),
        ingredients,
        container,
        result_item,
        result_count,
        cooking_time,
        experience,
    })
}

/// Parse one `result[]` entry of a cutting recipe.
fn parse_cutting_output(v: &JsonValue) -> Option<CuttingOutput> {
    let o = v.as_object()?;
    let (item, count) = match o.get("item") {
        // 1.21+ shape: {"item":{"id":"…","count":n},"chance":f}
        Some(inner @ JsonValue::Object(_)) => parse_item_stack(inner)?,
        // Legacy shape: {"item":"…","count":n}
        Some(inner @ JsonValue::String(_)) => {
            let (item, _) = parse_item_stack(inner)?;
            let count = o.get("count").and_then(JsonValue::as_u64).unwrap_or(1);
            (item, u16::try_from(count).ok()?)
        }
        // Bare-stack shape: {"id":"…","count":n,"chance":f}
        None => parse_item_stack(v)?,
        _ => return None,
    };
    let chance = o
        .get("chance")
        .or_else(|| o.get("probability"))
        .and_then(JsonValue::as_f64)
        .unwrap_or(1.0) as f32;
    Some(CuttingOutput {
        item,
        count,
        chance,
    })
}

/// Parse a `*:cutting` recipe. `None` = not a board recipe.
fn parse_cutting_recipe(id: &str, obj: &BTreeMap<String, JsonValue>) -> Option<CuttingRecipe> {
    let ingredient = if let Some(list) = obj.get("ingredients").and_then(JsonValue::as_array) {
        match list.len() {
            0 => return None,
            1 => parse_ingredient(&list[0])?,
            _ => {
                IngredientMatcher::AnyOf(list.iter().map(parse_ingredient).collect::<Option<_>>()?)
            }
        }
    } else {
        parse_ingredient(obj.get("input").or_else(|| obj.get("ingredient"))?)?
    };
    let tool = parse_tool_matcher(obj.get("tool")?)?;
    let results: Vec<_> = obj
        .get("result")
        .or_else(|| obj.get("results"))
        .and_then(JsonValue::as_array)?
        .iter()
        .map(parse_cutting_output)
        .collect::<Option<_>>()?;
    if results.is_empty() {
        return None;
    }
    let sound = obj
        .get("sound")
        .and_then(JsonValue::as_str)
        .map(str::to_string);
    Some(CuttingRecipe {
        id: id.to_string(),
        ingredient,
        tool,
        results,
        sound,
    })
}

/// Extract a `ToolMatcher` out of a `match_tool`-style condition object.
fn parse_condition_tool(cond: &BTreeMap<String, JsonValue>) -> Option<ToolMatcher> {
    let predicate = cond.get("predicate").unwrap_or(&JsonValue::Null);
    let pred_obj = predicate.as_object().unwrap_or(cond);
    let items = pred_obj.get("items").or_else(|| pred_obj.get("item"))?;
    match items {
        JsonValue::String(s) => parse_tool_matcher(&JsonValue::String(s.clone())),
        JsonValue::Array(arr) => parse_tool_matcher(arr.first()?),
        JsonValue::Object(_) => parse_tool_matcher(items),
        _ => None,
    }
}

/// Parse a `data/<ns>/loot_modifiers/*.json` global loot modifier.
/// `None` = a modifier type we do not model (skipped, not an error).
fn parse_loot_modifier(id: &str, obj: &BTreeMap<String, JsonValue>) -> Option<LootModifierRule> {
    let type_str = obj.get("type").and_then(JsonValue::as_str)?;
    let (mut add_item, mut add_table) = (None, None);
    match type_path(type_str) {
        "add_item" => {
            add_item = obj
                .get("item")
                .and_then(JsonValue::as_str)
                .map(normalize_id);
        }
        "add_table" => {
            add_table = obj
                .get("table")
                .and_then(JsonValue::as_str)
                .map(normalize_id);
        }
        _ => return None,
    }
    if add_item.is_none() && add_table.is_none() {
        return None;
    }

    let mut target_loot_table = None;
    let mut required_tool = None;
    if let Some(conditions) = obj.get("conditions").and_then(JsonValue::as_array) {
        for cond in conditions {
            let Some(co) = cond.as_object() else {
                continue;
            };
            let cond_type = co
                .get("condition")
                .or_else(|| co.get("type"))
                .and_then(JsonValue::as_str)
                .unwrap_or("");
            match type_path(cond_type) {
                "loot_table_id" | "loot_table" => {
                    if let Some(t) = co
                        .get("loot_table_id")
                        .or_else(|| co.get("loot_table"))
                        .or_else(|| co.get("id"))
                        .and_then(JsonValue::as_str)
                    {
                        target_loot_table = Some(t.to_string());
                    }
                }
                t if t.contains("match_tool") || t.contains("tool") => {
                    if required_tool.is_none() {
                        required_tool = parse_condition_tool(co);
                    }
                }
                _ => {
                    // Unnamed conditions carrying a tool predicate still gate.
                    if required_tool.is_none()
                        && co.get("predicate").and_then(JsonValue::as_object).is_some()
                    {
                        required_tool = parse_condition_tool(co);
                    }
                }
            }
        }
    }

    let chance = obj
        .get("chance")
        .or_else(|| obj.get("probability"))
        .and_then(JsonValue::as_f64)
        .unwrap_or(1.0) as f32;
    Some(LootModifierRule {
        id: id.to_string(),
        target_loot_table,
        required_tool,
        add_item,
        add_table,
        chance,
    })
}

/// Parse a `data/<ns>/food{s}/*.json` food component spec.
fn parse_food_spec(obj: &BTreeMap<String, JsonValue>) -> Option<FoodSpec> {
    let nutrition = obj
        .get("nutrition")
        .and_then(JsonValue::as_u64)
        .map(|v| u16::try_from(v).unwrap_or(0))
        .unwrap_or(0);
    let saturation = obj
        .get("saturation")
        .or_else(|| obj.get("saturation_modifier"))
        .and_then(JsonValue::as_f64)
        .unwrap_or(0.0) as f32;
    let using_converts_to = obj.get("using_converts_to").and_then(|v| match v {
        JsonValue::String(s) => Some(normalize_id(s)),
        JsonValue::Object(o) => o
            .get("id")
            .or_else(|| o.get("item"))
            .and_then(JsonValue::as_str)
            .map(normalize_id),
        _ => None,
    });
    let mut effects = Vec::new();
    if let Some(list) = obj.get("effects").and_then(JsonValue::as_array) {
        for e in list {
            let Some(eo) = e.as_object() else {
                continue;
            };
            let Some(id) = eo
                .get("id")
                .or_else(|| eo.get("effect"))
                .and_then(JsonValue::as_str)
            else {
                continue;
            };
            let duration = eo
                .get("duration")
                .or_else(|| eo.get("duration_ticks"))
                .and_then(JsonValue::as_u64)
                .map(|v| u32::try_from(v).unwrap_or(u32::MAX))
                .unwrap_or(0);
            let amplifier = eo
                .get("amplifier")
                .and_then(JsonValue::as_u64)
                .map(|v| u8::try_from(v).unwrap_or(0))
                .unwrap_or(0);
            let probability = eo
                .get("probability")
                .or_else(|| eo.get("chance"))
                .and_then(JsonValue::as_f64)
                .unwrap_or(1.0) as f32;
            effects.push((normalize_id(id), duration, amplifier, probability));
        }
    }
    Some(FoodSpec {
        nutrition,
        saturation,
        using_converts_to,
        effects,
    })
}

// ---------------------------------------------------------------------------
// Registry & JAR auto-discovery
// ---------------------------------------------------------------------------

/// Per-JAR accounting returned by
/// [`CulinaryRegistry::load_from_jar_bytes`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CulinaryDiscoveryStats {
    /// Total archive members scanned.
    pub jar_members: usize,
    /// Recognized `data/<ns>/…/*.json` members parsed.
    pub data_files: usize,
    /// `*:cooking` / `*:fermenting` recipes loaded.
    pub cooking_recipes: usize,
    /// `*:cutting` recipes loaded.
    pub cutting_recipes: usize,
    /// Recipe JSONs skipped as unrelated (vanilla or other-mod types).
    pub skipped_recipes: usize,
    /// Tag files merged into the shared index.
    pub tag_files: usize,
    /// Tag member entries merged.
    pub tag_values: usize,
    /// `*:add_item` / `*:add_table` rules loaded.
    pub loot_modifiers: usize,
    /// Food specs loaded.
    pub food_specs: usize,
}

/// Everything discovered from the loaded culinary JARs: merged tag index
/// plus every recipe, loot rule, and food spec across all addons.
#[derive(Clone, Debug, Default)]
pub struct CulinaryRegistry {
    /// Merged item+block tag index (`tag_id → members`).
    pub tags: TagIndex,
    /// All `*:cooking` / `*:fermenting` recipes, load order.
    pub cooking_recipes: Vec<CookingRecipe>,
    /// All `*:cutting` recipes.
    pub cutting_recipes: Vec<CuttingRecipe>,
    /// All `*:add_item` / `*:add_table` loot rules.
    pub loot_rules: Vec<LootModifierRule>,
    /// Food specs keyed by item id derived from the file path.
    pub food_specs: BTreeMap<String, FoodSpec>,
}

impl CulinaryRegistry {
    /// Empty registry; feed JARs through [`load_from_jar_bytes`].
    pub fn new() -> Self {
        Self::default()
    }

    /// Auto-discover every culinary data file in `jar_bytes` — works for
    /// the base mod **and any addon**, since nothing is keyed to a
    /// hardcoded namespace. Fail-closed on corrupt archives, unreadable
    /// members, or malformed JSON; unrelated recipe types are skipped.
    ///
    /// # Errors
    ///
    /// [`CulinaryError::Archive`] on any archive/member integrity problem,
    /// [`CulinaryError::Json`] on malformed JSON inside a scanned member.
    pub fn load_from_jar_bytes(
        &mut self,
        jar_bytes: &[u8],
    ) -> Result<CulinaryDiscoveryStats, CulinaryError> {
        let entries = modpack::zip_member_list(jar_bytes, "culinary JAR")?;
        let mut stats = CulinaryDiscoveryStats {
            jar_members: entries.len(),
            ..Default::default()
        };
        let mut extracted = 0u64;

        for entry in &entries {
            let Some((namespace, kind, rel)) = classify_member(&entry.name) else {
                continue;
            };
            let bytes = modpack::read_zip_member(jar_bytes, entry, &entry.name)?;
            extracted = extracted
                .checked_add(bytes.len() as u64)
                .ok_or_else(|| malformed("culinary scan byte budget overflow"))?;
            if extracted > MAX_CULINARY_SCAN_BYTES {
                return Err(malformed("culinary JAR scan byte budget exceeded"));
            }
            let root = parse_json(&bytes)?;
            let obj = root
                .as_object()
                .ok_or_else(|| malformed(format!("{}: root is not an object", entry.name)))?;
            stats.data_files += 1;
            let file_id = format!("{namespace}:{rel}");

            match kind {
                DataKind::Recipe => {
                    let type_str = obj.get("type").and_then(JsonValue::as_str).unwrap_or("");
                    match type_path(type_str) {
                        "cooking" | "fermenting" => {
                            match parse_cooking_recipe(&file_id, type_str, obj) {
                                Some(recipe) => {
                                    self.cooking_recipes.push(recipe);
                                    stats.cooking_recipes += 1;
                                }
                                None => stats.skipped_recipes += 1,
                            }
                        }
                        "cutting" => match parse_cutting_recipe(&file_id, obj) {
                            Some(recipe) => {
                                self.cutting_recipes.push(recipe);
                                stats.cutting_recipes += 1;
                            }
                            None => stats.skipped_recipes += 1,
                        },
                        _ => stats.skipped_recipes += 1,
                    }
                }
                DataKind::ItemTag | DataKind::BlockTag => {
                    let tag_id = file_id.clone();
                    let set = self.tags.entry(tag_id).or_default();
                    if let Some(values) = obj.get("values").and_then(JsonValue::as_array) {
                        for v in values {
                            // Entries may be strings or optional
                            // {"id":"…","required":bool} objects.
                            let id = match v {
                                JsonValue::String(s) => Some(s.clone()),
                                JsonValue::Object(o) => {
                                    o.get("id").and_then(JsonValue::as_str).map(str::to_string)
                                }
                                _ => None,
                            };
                            if let Some(id) = id {
                                set.insert(id);
                                stats.tag_values += 1;
                            }
                        }
                    }
                    stats.tag_files += 1;
                }
                DataKind::LootModifier => {
                    if let Some(rule) = parse_loot_modifier(&file_id, obj) {
                        self.loot_rules.push(rule);
                        stats.loot_modifiers += 1;
                    }
                }
                DataKind::FoodSpec => {
                    if let Some(spec) = parse_food_spec(obj) {
                        self.food_specs.insert(file_id, spec);
                        stats.food_specs += 1;
                    }
                }
            }
        }
        Ok(stats)
    }

    /// Food spec for `item`, if any loaded JAR declared one.
    pub fn food_spec(&self, item: &str) -> Option<&FoodSpec> {
        self.food_specs.get(&normalize_id(item))
    }

    /// Exact-unordered match of occupied `inputs` against every loaded
    /// cooking recipe. Returns `(recipe_index, slot_indices)` of the first
    /// recipe whose ingredient multiset bijects the occupied slots; the
    /// `container` must satisfy `recipe.container`.
    pub fn find_cooking_match(
        &self,
        inputs: &[Option<SlotStack>; POT_INPUT_SLOTS],
        container: Option<&str>,
    ) -> Option<(usize, Vec<usize>)> {
        let occupied: Vec<(usize, &str)> = inputs
            .iter()
            .enumerate()
            .filter_map(|(i, s)| s.as_ref().map(|(id, _)| (i, id.as_str())))
            .collect();
        for (idx, recipe) in self.cooking_recipes.iter().enumerate() {
            if let Some(required) = &recipe.container {
                match container {
                    Some(held) if normalize_id(held) == *required => {}
                    _ => continue,
                }
            }
            let slot_items: Vec<&str> = occupied.iter().map(|(_, id)| *id).collect();
            if let Some(local) = match_ingredients(&slot_items, &recipe.ingredients, &self.tags) {
                // local[] indexes `occupied`; map back to real slot indices.
                let slots = local.into_iter().map(|li| occupied[li].0).collect();
                return Some((idx, slots));
            }
        }
        None
    }

    /// Run the cutting board: first loaded cutting recipe whose
    /// ingredient matches `input_item` and whose tool matches `tool_item`
    /// rolls its outputs through a deterministic [`Xoroshiro128pp`] seeded
    /// with `seed`. Fractional `chance < 1` outputs drop iff
    /// `next_float() < chance`, rolled in recipe order.
    pub fn execute_cutting(
        &self,
        input_item: &str,
        tool_item: &str,
        seed: u64,
    ) -> Option<CuttingOutcome> {
        let input_item = normalize_id(input_item);
        let tool_item = normalize_id(tool_item);
        for recipe in &self.cutting_recipes {
            if !recipe.ingredient.matches(&input_item, &self.tags)
                || !recipe.tool.matches(&tool_item, &self.tags)
            {
                continue;
            }
            let mut rng = Xoroshiro128pp::new(seed);
            let outputs = recipe
                .results
                .iter()
                .filter(|o| o.chance >= 1.0 || rng.next_float() < o.chance)
                .map(|o| (o.item.clone(), o.count))
                .collect();
            return Some(CuttingOutcome {
                recipe_id: recipe.id.clone(),
                outputs,
                sound: recipe.sound.clone(),
            });
        }
        None
    }

    /// Fire every loot rule whose target table and tool gate pass for a
    /// `loot_table` drop rolled under deterministic `seed`. Rules with
    /// `chance < 1` consume one `next_float` each, in load order.
    pub fn evaluate_loot_modifiers(
        &self,
        loot_table: &str,
        held_tool: Option<&str>,
        seed: u64,
    ) -> Vec<LootAddition> {
        let mut rng = Xoroshiro128pp::new(seed);
        let mut out = Vec::new();
        for rule in &self.loot_rules {
            if let Some(target) = &rule.target_loot_table {
                let matches = target
                    .strip_suffix('*')
                    .map_or(target == loot_table, |prefix| {
                        loot_table.starts_with(prefix)
                    });
                if !matches {
                    continue;
                }
            }
            if let Some(required) = &rule.required_tool {
                let ok = held_tool
                    .map(normalize_id)
                    .is_some_and(|tool| required.matches(&tool, &self.tags));
                if !ok {
                    continue;
                }
            }
            if rule.chance < 1.0 && rng.next_float() >= rule.chance {
                continue;
            }
            out.push(LootAddition {
                rule_id: rule.id.clone(),
                item: rule.add_item.clone(),
                table: rule.add_table.clone(),
            });
        }
        out
    }
}

// ---------------------------------------------------------------------------
// Pot cluster — perf_kit-backed O(due) tick driver
// ---------------------------------------------------------------------------

/// Outbound state-sync snapshot for one pot — the last-write-wins delta
/// [`CookingPotCluster`] accumulates between flushes. Callers drain it via
/// [`CookingPotCluster::drain_sync_deltas`] once per sync window instead of
/// shipping every intermediate mutation.
#[derive(Clone, Debug, PartialEq)]
pub struct PotSyncSnapshot {
    /// Current output slot contents.
    pub output: Option<SlotStack>,
    /// Banked progress toward the armed recipe.
    pub progress_ticks: u32,
    /// Armed completion tick; `None` while the pot sleeps.
    pub next_completion_tick: Option<u64>,
    /// Whether a heat source is under the pot.
    pub heated: bool,
}

impl PotSyncSnapshot {
    fn of(pot: &CookingPotState) -> Self {
        Self {
            output: pot.output.clone(),
            progress_ticks: pot.progress_ticks,
            next_completion_tick: pot.next_completion_tick,
            heated: pot.heated,
        }
    }
}

/// Per-tick summary returned by [`CookingPotCluster::tick`].
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ClusterTickReport {
    /// Machines pulled off the wake backlog this tick.
    pub processed: usize,
    /// `(pot_id, result)` completions emitted this tick, in backlog order.
    pub results: Vec<(u64, CookResult)>,
}

/// Cluster of [`CookingPotState`] machines driven by
/// [`crate::mod_tick_runtime::ModTickRuntime`]: pots sleep in O(1), wake
/// exactly on `next_completion_tick`, burst completions slice through a
/// per-tick budget, and outbound state deltas coalesce last-write-wins.
///
/// Every mutator re-fingerprints the pot: a mutation that changes nothing
/// (a full-pot insert, an unchanged heat flag) skips both the recipe
/// re-match and the schedule update, and — crucially — never disturbs an
/// already-armed `next_completion_tick`, since [`CookingPotState::refresh`]
/// would otherwise re-arm relative to the new `now`.
pub struct CookingPotCluster {
    pots: BTreeMap<u64, CookingPotState>,
    runtime: ModTickRuntime<u64, PotSyncSnapshot>,
}

impl Default for CookingPotCluster {
    fn default() -> Self {
        Self::new()
    }
}

impl CookingPotCluster {
    /// Empty cluster.
    pub fn new() -> Self {
        Self {
            pots: BTreeMap::new(),
            runtime: ModTickRuntime::new(),
        }
    }

    /// Pots in the cluster.
    pub fn len(&self) -> usize {
        self.pots.len()
    }

    /// `true` when empty.
    pub fn is_empty(&self) -> bool {
        self.pots.is_empty()
    }

    /// Read a pot's state.
    pub fn pot(&self, id: u64) -> Option<&CookingPotState> {
        self.pots.get(&id)
    }

    /// Add a fresh sleeping pot. `false` when `id` is already registered —
    /// the existing pot is left untouched.
    pub fn add_pot(&mut self, id: u64) -> bool {
        if self.pots.contains_key(&id) {
            return false;
        }
        self.pots.insert(id, CookingPotState::new());
        let pot = &self.pots[&id];
        self.runtime.commit(id, pot_fingerprint(pot), None);
        true
    }

    /// Remove a pot and every trace of it from the runtime (gate, wake
    /// schedule, backlog, pending delta). `false` when absent.
    pub fn remove_pot(&mut self, id: u64) -> bool {
        if self.pots.remove(&id).is_none() {
            return false;
        }
        self.runtime.remove(&id);
        true
    }

    /// Shared post-mutation path: fingerprint the pot, skip the refresh
    /// when nothing effectively changed, otherwise re-match the recipe,
    /// commit the new fingerprint + wake, and accumulate a sync delta.
    /// Returns `true` when the pot was actually re-evaluated.
    fn observe_pot(&mut self, id: u64, registry: &CulinaryRegistry, now: u64) -> bool {
        let Some(pot) = self.pots.get_mut(&id) else {
            return false;
        };
        if !self.runtime.is_dirty(&id, pot_fingerprint(pot)) {
            return false;
        }
        pot.refresh(registry, now);
        let fp = pot_fingerprint(pot);
        let wake = pot.next_completion_tick;
        let snapshot = PotSyncSnapshot::of(pot);
        self.runtime.commit(id, fp, wake);
        self.runtime.record_delta(id, snapshot);
        true
    }

    /// Insert `(item, count)` into `id`'s first free slot and re-observe.
    /// Returns [`CookingPotState::insert_input`]'s result (`false` when
    /// full — and the no-op is fingerprint-skipped, not re-matched).
    pub fn insert_input(
        &mut self,
        id: u64,
        item: &str,
        count: u16,
        registry: &CulinaryRegistry,
        now: u64,
    ) -> bool {
        let Some(pot) = self.pots.get_mut(&id) else {
            return false;
        };
        let inserted = pot.insert_input(item, count);
        self.observe_pot(id, registry, now);
        inserted
    }

    /// Replace the container slot (`minecraft:bowl`, …) and re-observe.
    pub fn set_container(
        &mut self,
        id: u64,
        container: Option<SlotStack>,
        registry: &CulinaryRegistry,
        now: u64,
    ) {
        if let Some(pot) = self.pots.get_mut(&id) {
            pot.container = container;
            self.observe_pot(id, registry, now);
        }
    }

    /// Set the heated flag directly and re-observe.
    pub fn set_heated(&mut self, id: u64, heated: bool, registry: &CulinaryRegistry, now: u64) {
        if let Some(pot) = self.pots.get_mut(&id) {
            pot.heated = heated;
            self.observe_pot(id, registry, now);
        }
    }

    /// Recompute `heated` from the blocks below via [`is_heated`], then
    /// re-observe. Convenience for callers tracking the world instead of
    /// the flag.
    pub fn set_heat_source(
        &mut self,
        id: u64,
        block_below: &str,
        block_two_below: Option<&str>,
        tags: &TagIndex,
        registry: &CulinaryRegistry,
        now: u64,
    ) {
        self.set_heated(
            id,
            is_heated(block_below, block_two_below, tags),
            registry,
            now,
        );
    }

    /// Take the output slot's contents and re-observe. Returns what was
    /// in the slot.
    pub fn take_output(
        &mut self,
        id: u64,
        registry: &CulinaryRegistry,
        now: u64,
    ) -> Option<SlotStack> {
        let pot = self.pots.get_mut(&id)?;
        let taken = pot.output.take();
        self.observe_pot(id, registry, now);
        taken
    }

    /// Advance the cluster to `now`. Pots whose wake tick arrived move
    /// from the wheel onto the budget queue, and at most `max_process` of
    /// them are advanced this tick — burst completions beyond the budget
    /// stay queued for the next tick, but each still reports its own
    /// scheduled `completed_tick`, so results are identical regardless of
    /// how the burst sliced.
    pub fn tick(
        &mut self,
        now: u64,
        registry: &CulinaryRegistry,
        max_process: usize,
    ) -> ClusterTickReport {
        let mut due = Vec::new();
        let drain = self
            .runtime
            .tick(now, max_process, u64::MAX, |_| 1, &mut due);
        let mut results = Vec::new();
        for id in due {
            let Some(pot) = self.pots.get_mut(&id) else {
                continue;
            };
            if let Some(result) = pot.advance_to(now, registry) {
                results.push((id, result));
            }
            let fp = pot_fingerprint(pot);
            let wake = pot.next_completion_tick;
            let snapshot = PotSyncSnapshot::of(pot);
            self.runtime.commit(id, fp, wake);
            self.runtime.record_delta(id, snapshot);
        }
        ClusterTickReport {
            processed: drain.items,
            results,
        }
    }

    /// Append every coalesced outbound snapshot to `out` in sorted pot-id
    /// order, emptying the batch — one delta per touched pot no matter how
    /// many mutations or completions happened since the last flush.
    pub fn drain_sync_deltas(&mut self, out: &mut Vec<(u64, PotSyncSnapshot)>) {
        self.runtime.drain_deltas_into(out);
    }

    /// Fingerprint-gate counters: `skips` proves idle mutations skipped
    /// the recipe re-match, `updates` counts committed state changes.
    pub fn gate_stats(&self) -> FingerprintStats {
        self.runtime.gate_stats()
    }

    /// Pots armed on the wake wheel.
    pub fn scheduled_len(&self) -> usize {
        self.runtime.scheduled_len()
    }

    /// Due-but-unprocessed pots waiting out a burst budget.
    pub fn backlog_len(&self) -> usize {
        self.runtime.backlog_len()
    }

    /// Pots with an unflushed sync delta.
    pub fn pending_delta_len(&self) -> usize {
        self.runtime.pending_delta_len()
    }

    /// `id`'s armed wake tick, if any.
    pub fn wake_of(&self, id: u64) -> Option<u64> {
        self.runtime.wake_of(&id)
    }
}

/// Fingerprint of the pot's schedule-relevant state: input slots, output,
/// container, heat, banked progress, and armed recipe. `next_completion_tick`
/// is deliberately excluded — it is derived from these fields plus `now`,
/// so including it would make every commit distinct and defeat the gate.
fn pot_fingerprint(pot: &CookingPotState) -> u64 {
    let mut f = FingerprintBuilder::new();
    f.write_bool(pot.heated)
        .write_u64(u64::from(pot.progress_ticks))
        .write_u64(pot.active_recipe.map(|i| i as u64 + 1).unwrap_or(0));
    for slot in &pot.inputs {
        write_slot_fp(&mut f, slot);
    }
    write_slot_fp(&mut f, &pot.container);
    write_slot_fp(&mut f, &pot.output);
    f.finish()
}

fn write_slot_fp(f: &mut FingerprintBuilder, slot: &Option<SlotStack>) {
    match slot {
        None => {
            f.write_u8(0);
        }
        Some((id, count)) => {
            f.write_u8(1).write_str(id).write_u16(*count);
        }
    }
}
