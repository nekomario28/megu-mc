// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later

//! Opt-in MineAll/CutAll 3D BFS planner.
//!
//! The planner is deliberately conservative: both trigger modes default to
//! [`TriggerMode::Off`], every candidate passes the caller's `can_break`
//! protection check, and the durability budget is computed before a position
//! is added to the plan.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

pub const DEFAULT_MAX_BLOCKS: usize = 64;
pub const MIN_MAX_BLOCKS: usize = 1;
pub const MAX_MAX_BLOCKS: usize = 256;
pub const BLOCK_BREAK_EXHAUSTION: f64 = 0.005;

const ORE_TAGS: &[&str] = &[
    "c:ores",
    "forge:ores",
    "neoforge:ores",
    "minecraft:coal_ores",
    "minecraft:copper_ores",
    "minecraft:diamond_ores",
    "minecraft:emerald_ores",
    "minecraft:gold_ores",
    "minecraft:iron_ores",
    "minecraft:lapis_ores",
    "minecraft:redstone_ores",
];

const LOG_TAGS: &[&str] = &[
    "minecraft:logs",
    "minecraft:logs_that_burn",
    "minecraft:crimson_stems",
    "minecraft:warped_stems",
    "minecraft:bamboo_blocks",
    "megu:cutall_logs",
    "c:logs",
    "forge:logs",
    "neoforge:logs",
];

const LEAF_TAGS: &[&str] = &[
    "minecraft:leaves",
    "minecraft:wart_blocks",
    "c:leaves",
    "forge:leaves",
    "neoforge:leaves",
];

/// A block position in one world/dimension. MineAll and CutAll are local 3D
/// searches, so the planner intentionally does not model chunk or dimension
/// transitions.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct MineCutAllPos {
    pub x: i32,
    pub y: i32,
    pub z: i32,
}

impl MineCutAllPos {
    pub const fn new(x: i32, y: i32, z: i32) -> Self {
        Self { x, y, z }
    }

    pub const fn offset(self, dx: i32, dy: i32, dz: i32) -> Self {
        Self {
            x: self.x + dx,
            y: self.y + dy,
            z: self.z + dz,
        }
    }
}

/// When a MineAll/CutAll trigger is allowed to expand beyond the block the
/// player broke.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum TriggerMode {
    /// Vanilla behavior only. This is the default and must remain off unless
    /// the operator explicitly opts in.
    #[default]
    Off,
    /// Expand only while the player is sneaking.
    SneakOnly,
    /// Expand only while the player is not sneaking.
    WhileNotSneaking,
    /// Expand regardless of sneak state.
    Always,
}

impl TriggerMode {
    pub fn allows(self, sneaking: bool) -> bool {
        match self {
            Self::Off => false,
            Self::SneakOnly => sneaking,
            Self::WhileNotSneaking => !sneaking,
            Self::Always => true,
        }
    }

    pub fn is_off(self) -> bool {
        matches!(self, Self::Off)
    }
}

/// Declarative configuration for the opt-in planner.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MineCutAllConfig {
    pub mine_all: TriggerMode,
    pub cut_all: TriggerMode,
    /// Maximum connected blocks returned by one plan. The effective value is
    /// always clamped to `1..=256`, including when callers mutate the public
    /// field directly.
    pub max_blocks: usize,
    /// Preserve the final durability point on damageable tools.
    pub stop_before_tool_break: bool,
    /// Allow untagged mod trunks when a connected same-id component is capped
    /// by leaf blocks. This is intentionally leaf-gated so ordinary plank or
    /// stone construction is not mistaken for a tree.
    pub allow_leaf_capped_custom_trees: bool,
    /// Additional ore block ids or `#tag` selectors.
    pub extra_ore_blocks: BTreeSet<String>,
    /// Additional log/trunk block ids or `#tag` selectors.
    pub extra_log_blocks: BTreeSet<String>,
    /// Additional leaf block ids or `#tag` selectors used by the custom-tree
    /// cap check.
    pub extra_leaf_blocks: BTreeSet<String>,
}

impl Default for MineCutAllConfig {
    fn default() -> Self {
        Self {
            mine_all: TriggerMode::Off,
            cut_all: TriggerMode::Off,
            max_blocks: DEFAULT_MAX_BLOCKS,
            stop_before_tool_break: true,
            allow_leaf_capped_custom_trees: true,
            extra_ore_blocks: BTreeSet::new(),
            extra_log_blocks: BTreeSet::new(),
            extra_leaf_blocks: BTreeSet::new(),
        }
    }
}

impl MineCutAllConfig {
    pub fn effective_max_blocks(&self) -> usize {
        self.max_blocks.clamp(MIN_MAX_BLOCKS, MAX_MAX_BLOCKS)
    }

    pub fn with_max_blocks(mut self, max_blocks: usize) -> Self {
        self.max_blocks = max_blocks.clamp(MIN_MAX_BLOCKS, MAX_MAX_BLOCKS);
        self
    }

    pub fn with_trigger(mut self, mine_all: TriggerMode, cut_all: TriggerMode) -> Self {
        self.mine_all = mine_all;
        self.cut_all = cut_all;
        self
    }
}

/// The kind of expansion selected for a non-empty plan. [`MineCutAllKind::None`]
/// is used for empty/default-off plans so callers never have to invent a kind.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum MineCutAllKind {
    #[default]
    None,
    MineAll,
    CutAll,
}

impl MineCutAllKind {
    pub fn is_none(self) -> bool {
        matches!(self, Self::None)
    }
}

pub type MineCutAllPlanKind = MineCutAllKind;

/// Deterministic result of [`plan_vein_or_tree_break`]. `positions` is in BFS
/// discovery order and includes the origin as `positions[0]` when non-empty.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MineCutAllPlan {
    pub kind: MineCutAllKind,
    pub positions: Vec<MineCutAllPos>,
    pub durability_cost: u32,
    pub exhaustion_cost: f64,
    pub stopped_by_durability: bool,
    pub skipped_by_claim: bool,
}

impl MineCutAllPlan {
    pub fn empty(kind: MineCutAllKind) -> Self {
        Self {
            kind,
            positions: Vec::new(),
            durability_cost: 0,
            exhaustion_cost: 0.0,
            stopped_by_durability: false,
            skipped_by_claim: false,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.positions.is_empty()
    }

    pub fn blocks_broken(&self) -> usize {
        self.positions.len()
    }
}

/// Read-only world view required by the planner. `block_id_at` returns `None`
/// for air, unloaded cells, or missing blocks; all of those are hard BFS
/// boundaries. `block_has_tag` should answer effective tag membership,
/// including nested tag expansion when the backing tag index supports it.
pub trait MineCutAllWorld {
    fn block_id_at(&self, pos: MineCutAllPos) -> Option<&str>;

    fn block_tags(&self, _block_id: &str) -> Vec<String> {
        Vec::new()
    }

    fn block_has_tag(&self, block_id: &str, tag: &str) -> bool {
        let wanted = normalize_tag(tag);
        self.block_tags(block_id)
            .iter()
            .any(|have| normalize_tag(have) == wanted)
    }

    /// Protection/claim callback. Returning `false` makes the position a hard
    /// BFS boundary: it is not planned and is not searched through.
    fn can_break(&self, _pos: MineCutAllPos) -> bool {
        true
    }
}

/// Flattened block-tag index built from datapack-style `tag -> values` maps.
/// Values beginning with `#` are resolved recursively; cycles fail closed by
/// ignoring the recursive edge.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MineCutAllTagIndex {
    block_tags: BTreeMap<String, BTreeSet<String>>,
}

impl MineCutAllTagIndex {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn from_tag_map(tags: &BTreeMap<String, Vec<String>>) -> Self {
        let mut definitions = BTreeMap::new();
        for (tag, values) in tags {
            definitions.insert(normalize_tag(tag).to_string(), values.clone());
        }

        let mut index = Self::default();
        let tag_names: Vec<String> = definitions.keys().cloned().collect();
        for tag in tag_names {
            let mut members = BTreeSet::new();
            let mut visiting = BTreeSet::new();
            collect_tag_members(&tag, &definitions, &mut visiting, &mut members);
            for block_id in members {
                index
                    .block_tags
                    .entry(block_id)
                    .or_default()
                    .insert(tag.clone());
            }
        }
        index
    }

    pub fn add_block_tag(&mut self, block_id: &str, tag: &str) {
        self.block_tags
            .entry(block_id.to_string())
            .or_default()
            .insert(normalize_tag(tag).to_string());
    }

    pub fn tags_for_block(&self, block_id: &str) -> Option<&BTreeSet<String>> {
        self.block_tags.get(block_id)
    }

    pub fn has_tag(&self, block_id: &str, tag: &str) -> bool {
        self.tags_for_block(block_id)
            .is_some_and(|tags| tags.contains(normalize_tag(tag)))
    }
}

fn collect_tag_members(
    tag: &str,
    definitions: &BTreeMap<String, Vec<String>>,
    visiting: &mut BTreeSet<String>,
    members: &mut BTreeSet<String>,
) {
    let Some(values) = definitions.get(tag) else {
        return;
    };
    for value in values {
        let value = value.trim();
        if value.is_empty() {
            continue;
        }
        if let Some(nested) = value.strip_prefix('#') {
            let nested = normalize_tag(nested).to_string();
            if visiting.insert(nested.clone()) {
                collect_tag_members(&nested, definitions, visiting, members);
                visiting.remove(&nested);
            }
        } else {
            members.insert(value.to_string());
        }
    }
}

/// Concrete in-memory world for tests and for adapters that already have a
/// materialized block/tag snapshot.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MineCutAllWorldData {
    pub blocks: BTreeMap<MineCutAllPos, String>,
    pub tag_index: MineCutAllTagIndex,
    pub protected: BTreeSet<MineCutAllPos>,
}

impl MineCutAllWorldData {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_block(&mut self, pos: MineCutAllPos, block_id: &str) {
        self.blocks.insert(pos, block_id.to_string());
    }

    pub fn remove_block(&mut self, pos: MineCutAllPos) {
        self.blocks.remove(&pos);
    }

    pub fn protect(&mut self, pos: MineCutAllPos) {
        self.protected.insert(pos);
    }

    pub fn unprotect(&mut self, pos: MineCutAllPos) {
        self.protected.remove(&pos);
    }

    pub fn is_protected(&self, pos: MineCutAllPos) -> bool {
        self.protected.contains(&pos)
    }

    pub fn add_block_tag(&mut self, block_id: &str, tag: &str) {
        self.tag_index.add_block_tag(block_id, tag);
    }

    pub fn set_tag_index(&mut self, tag_index: MineCutAllTagIndex) {
        self.tag_index = tag_index;
    }
}

impl MineCutAllWorld for MineCutAllWorldData {
    fn block_id_at(&self, pos: MineCutAllPos) -> Option<&str> {
        self.blocks.get(&pos).map(String::as_str)
    }

    fn block_tags(&self, block_id: &str) -> Vec<String> {
        self.tag_index
            .tags_for_block(block_id)
            .map(|tags| tags.iter().cloned().collect())
            .unwrap_or_default()
    }

    fn block_has_tag(&self, block_id: &str, tag: &str) -> bool {
        self.tag_index.has_tag(block_id, tag)
    }

    fn can_break(&self, pos: MineCutAllPos) -> bool {
        !self.protected.contains(&pos)
    }
}

/// Tool family inferred from item tags. The `minecraft:tool` component path is
/// handled separately because its `mineable_tags` are matched against the
/// broken block's tags.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ToolFamily {
    Pickaxe,
    Axe,
    Shovel,
    Hoe,
    Shears,
}

/// Mod-agnostic held-tool descriptor. Recognition is data-driven: item tags
/// such as `#minecraft:pickaxes`, `#minecraft:axes`, or
/// `#c:tools/mining_tool`, or a 1.21.1-style `minecraft:tool` component's
/// `mineable_tags`. No modded item ids are hardcoded.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ToolDescriptor {
    pub item_id: String,
    pub item_tags: BTreeSet<String>,
    /// Effective block tags from the item's `minecraft:tool` component.
    pub mineable_tags: BTreeSet<String>,
    /// Remaining durability uses. `None` means the item does not consume
    /// durability (including explicit unbreakable items).
    pub remaining_durability: Option<u32>,
    pub unbreakable: bool,
}

impl ToolDescriptor {
    pub fn new(item_id: impl Into<String>) -> Self {
        Self {
            item_id: item_id.into(),
            ..Self::default()
        }
    }

    pub fn with_item_tags<I, S>(mut self, tags: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        self.item_tags = tags
            .into_iter()
            .map(|tag| tag.as_ref().to_string())
            .collect();
        self
    }

    pub fn with_mineable_tags<I, S>(mut self, tags: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        self.mineable_tags = tags
            .into_iter()
            .map(|tag| tag.as_ref().to_string())
            .collect();
        self
    }

    pub fn with_remaining_durability(mut self, remaining_durability: u32) -> Self {
        self.remaining_durability = Some(remaining_durability);
        self
    }

    pub fn with_unbreakable(mut self) -> Self {
        self.unbreakable = true;
        self.remaining_durability = None;
        self
    }

    pub fn is_recognized(&self) -> bool {
        self.tool_families().is_some()
            || self.has_generic_mining_tag()
            || !self.mineable_tags.is_empty()
    }

    pub fn has_mineable_tags(&self) -> bool {
        !self.mineable_tags.is_empty()
    }

    fn has_generic_mining_tag(&self) -> bool {
        self.item_tags.iter().any(|tag| {
            matches!(
                normalize_tag(tag),
                "c:tools/mining_tool" | "forge:tools/mining_tool" | "neoforge:tools/mining_tool"
            )
        })
    }

    fn tool_families(&self) -> Option<BTreeSet<ToolFamily>> {
        let mut families = BTreeSet::new();
        for tag in &self.item_tags {
            match normalize_tag(tag) {
                "minecraft:pickaxes"
                | "c:tools/pickaxe"
                | "c:tools/pickaxes"
                | "forge:tools/pickaxe"
                | "forge:tools/pickaxes"
                | "neoforge:tools/pickaxe"
                | "neoforge:tools/pickaxes" => {
                    families.insert(ToolFamily::Pickaxe);
                }
                "minecraft:axes"
                | "c:tools/axe"
                | "c:tools/axes"
                | "forge:tools/axe"
                | "forge:tools/axes"
                | "neoforge:tools/axe"
                | "neoforge:tools/axes" => {
                    families.insert(ToolFamily::Axe);
                }
                "minecraft:shovels"
                | "c:tools/shovel"
                | "c:tools/shovels"
                | "forge:tools/shovel"
                | "forge:tools/shovels"
                | "neoforge:tools/shovel"
                | "neoforge:tools/shovels" => {
                    families.insert(ToolFamily::Shovel);
                }
                "minecraft:hoes"
                | "c:tools/hoe"
                | "c:tools/hoes"
                | "forge:tools/hoe"
                | "forge:tools/hoes"
                | "neoforge:tools/hoe"
                | "neoforge:tools/hoes" => {
                    families.insert(ToolFamily::Hoe);
                }
                "minecraft:shears"
                | "c:tools/shears"
                | "c:tools/shear"
                | "forge:tools/shears"
                | "forge:tools/shear"
                | "neoforge:tools/shears"
                | "neoforge:tools/shear" => {
                    families.insert(ToolFamily::Shears);
                }
                _ => {}
            }
        }
        (!families.is_empty()).then_some(families)
    }

    /// Whether this tool may break `block_id`. `fallback_family` is used only
    /// when the block has no explicit `minecraft:mineable/*` tag; MineAll
    /// passes [`ToolFamily::Pickaxe`] and CutAll passes [`ToolFamily::Axe`].
    pub fn can_mine_block<W: MineCutAllWorld + ?Sized>(
        &self,
        world: &W,
        block_id: &str,
        fallback_family: ToolFamily,
    ) -> bool {
        if !self.is_recognized() {
            return false;
        }

        if self
            .mineable_tags
            .iter()
            .any(|tag| world.block_has_tag(block_id, tag))
        {
            return true;
        }

        let required = preferred_tool_family(world, block_id).unwrap_or(fallback_family);
        if let Some(families) = self.tool_families() {
            return families.contains(&required);
        }
        self.has_generic_mining_tag()
    }

    /// Budget in blocks this tool may break under `stop_before_tool_break`.
    /// `None` means durability does not limit the plan.
    pub fn durability_budget(&self, stop_before_tool_break: bool) -> Option<usize> {
        if self.unbreakable {
            return None;
        }
        let remaining = usize::try_from(self.remaining_durability?).ok()?;
        Some(if stop_before_tool_break {
            remaining.saturating_sub(1)
        } else {
            remaining
        })
    }
}

fn preferred_tool_family<W: MineCutAllWorld + ?Sized>(
    world: &W,
    block_id: &str,
) -> Option<ToolFamily> {
    for (tag, family) in [
        ("minecraft:mineable/pickaxe", ToolFamily::Pickaxe),
        ("minecraft:mineable/axe", ToolFamily::Axe),
        ("minecraft:mineable/shovel", ToolFamily::Shovel),
        ("minecraft:mineable/hoe", ToolFamily::Hoe),
    ] {
        if world.block_has_tag(block_id, tag) {
            return Some(family);
        }
    }
    None
}

/// Whether `block_id` is an ore candidate. Exact ids and `#tag` selectors in
/// [`MineCutAllConfig::extra_ore_blocks`] are supported in addition to the
/// built-in ore tag families above.
pub fn is_ore_block<W: MineCutAllWorld + ?Sized>(
    config: &MineCutAllConfig,
    world: &W,
    block_id: &str,
) -> bool {
    ORE_TAGS
        .iter()
        .any(|tag| world.block_has_tag(block_id, tag))
        || selectors_match(world, block_id, &config.extra_ore_blocks)
}

/// Whether `block_id` is a standard log/trunk candidate.
pub fn is_log_block<W: MineCutAllWorld + ?Sized>(
    config: &MineCutAllConfig,
    world: &W,
    block_id: &str,
) -> bool {
    LOG_TAGS
        .iter()
        .any(|tag| world.block_has_tag(block_id, tag))
        || selectors_match(world, block_id, &config.extra_log_blocks)
}

/// Whether `block_id` is a leaf candidate for the custom-tree cap check.
pub fn is_leaf_block<W: MineCutAllWorld + ?Sized>(
    config: &MineCutAllConfig,
    world: &W,
    block_id: &str,
) -> bool {
    LEAF_TAGS
        .iter()
        .any(|tag| world.block_has_tag(block_id, tag))
        || selectors_match(world, block_id, &config.extra_leaf_blocks)
        || world.block_tags(block_id).iter().any(|tag| {
            let leaf = last_tag_segment(tag);
            leaf == "leaves" || leaf.ends_with("_leaves") || leaf.ends_with("-leaves")
        })
        || block_id_looks_like_leaves(block_id)
}

fn selectors_match<W: MineCutAllWorld + ?Sized>(
    world: &W,
    block_id: &str,
    selectors: &BTreeSet<String>,
) -> bool {
    selectors.iter().any(|selector| {
        let selector = normalize_tag(selector);
        !selector.is_empty()
            && (same_block_id(block_id, selector) || world.block_has_tag(block_id, selector))
    })
}

fn leaf_capped_trunk_candidate<W: MineCutAllWorld + ?Sized>(
    config: &MineCutAllConfig,
    world: &W,
    origin: MineCutAllPos,
    block_id: &str,
) -> bool {
    if !config.allow_leaf_capped_custom_trees {
        return false;
    }

    // Classification scans the physical component without claim/durability
    // limits so a protected lower trunk cannot hide the canopy above it. The
    // hard scan bound is the global 256-block safety cap, not the player's
    // configured per-break limit.
    let component = connected_component(world, origin, block_id, MAX_MAX_BLOCKS, None, true, false);
    if component.is_empty() {
        return false;
    }
    let max_trunk_y = component.iter().map(|pos| pos.y).max().unwrap_or(origin.y);
    let offsets = neighbor_offsets(true);
    let mut leaf_cache: BTreeMap<String, bool> = BTreeMap::new();

    for trunk in &component {
        for offset in &offsets {
            let leaf_pos = trunk.offset(offset.0, offset.1, offset.2);
            if leaf_pos.y < trunk.y {
                continue;
            }
            let capped_by_side_ring = trunk.y == max_trunk_y && leaf_pos.y == trunk.y;
            if leaf_pos.y == trunk.y && !capped_by_side_ring {
                continue;
            }
            let Some(leaf_id) = world.block_id_at(leaf_pos) else {
                continue;
            };
            let is_leaf = *leaf_cache
                .entry(leaf_id.to_string())
                .or_insert_with(|| is_leaf_block(config, world, leaf_id));
            if is_leaf {
                return true;
            }
        }
    }
    false
}

fn connected_component<W: MineCutAllWorld + ?Sized>(
    world: &W,
    origin: MineCutAllPos,
    target_id: &str,
    max_blocks: usize,
    durability_budget: Option<usize>,
    prefer_up: bool,
    respect_claims: bool,
) -> Vec<MineCutAllPos> {
    search_connected(
        world,
        origin,
        target_id,
        max_blocks,
        durability_budget,
        prefer_up,
        respect_claims,
    )
    .positions
}

struct SearchResult {
    positions: Vec<MineCutAllPos>,
    skipped_by_claim: bool,
    stopped_by_durability: bool,
}

fn search_connected<W: MineCutAllWorld + ?Sized>(
    world: &W,
    origin: MineCutAllPos,
    target_id: &str,
    max_blocks: usize,
    durability_budget: Option<usize>,
    prefer_up: bool,
    respect_claims: bool,
) -> SearchResult {
    let mut result = SearchResult {
        positions: Vec::new(),
        skipped_by_claim: false,
        stopped_by_durability: false,
    };
    let max_blocks = max_blocks.clamp(MIN_MAX_BLOCKS, MAX_MAX_BLOCKS);
    let offsets = neighbor_offsets(prefer_up);
    let mut visited = BTreeSet::new();
    let mut queue = VecDeque::new();

    visited.insert(origin);
    queue.push_back(origin);

    while let Some(pos) = queue.pop_front() {
        let Some(block_id) = world.block_id_at(pos) else {
            continue;
        };
        if !same_block_id(block_id, target_id) {
            continue;
        }
        if respect_claims && !world.can_break(pos) {
            result.skipped_by_claim = true;
            continue;
        }
        if result.positions.len() >= max_blocks {
            break;
        }
        if let Some(budget) = durability_budget {
            if result.positions.len() >= budget {
                result.stopped_by_durability = true;
                break;
            }
        }

        result.positions.push(pos);
        if result.positions.len() >= max_blocks {
            break;
        }

        for offset in &offsets {
            let next = pos.offset(offset.0, offset.1, offset.2);
            if !visited.insert(next) {
                continue;
            }
            let Some(next_id) = world.block_id_at(next) else {
                continue;
            };
            if !same_block_id(next_id, target_id) {
                continue;
            }
            if respect_claims && !world.can_break(next) {
                result.skipped_by_claim = true;
                continue;
            }
            queue.push_back(next);
        }
    }

    result
}

fn neighbor_offsets(prefer_up: bool) -> Vec<(i32, i32, i32)> {
    let mut offsets = Vec::with_capacity(26);
    for dy in -1..=1 {
        for dx in -1..=1 {
            for dz in -1..=1 {
                if dx == 0 && dy == 0 && dz == 0 {
                    continue;
                }
                offsets.push((dx, dy, dz));
            }
        }
    }
    offsets.sort_by(|a, b| {
        let horizontal_a = a.0 * a.0 + a.2 * a.2;
        let horizontal_b = b.0 * b.0 + b.2 * b.2;
        if prefer_up {
            b.1.cmp(&a.1)
                .then(horizontal_a.cmp(&horizontal_b))
                .then(a.0.cmp(&b.0))
                .then(a.2.cmp(&b.2))
        } else {
            a.1.cmp(&b.1)
                .then(horizontal_a.cmp(&horizontal_b))
                .then(a.0.cmp(&b.0))
                .then(a.2.cmp(&b.2))
        }
    });
    offsets
}

fn normalize_tag(tag: &str) -> &str {
    tag.trim().strip_prefix('#').unwrap_or(tag.trim())
}

fn same_block_id(a: &str, b: &str) -> bool {
    a == b || a.strip_prefix("minecraft:") == Some(b) || b.strip_prefix("minecraft:") == Some(a)
}

fn last_tag_segment(tag: &str) -> &str {
    normalize_tag(tag)
        .rsplit(['/', ':'])
        .next()
        .unwrap_or("")
}

fn block_id_looks_like_leaves(block_id: &str) -> bool {
    let path = block_id.rsplit(':').next().unwrap_or(block_id);
    let leaf = path.rsplit('/').next().unwrap_or(path);
    leaf == "leaves" || leaf.ends_with("_leaves") || leaf.ends_with("-leaves")
}

fn empty_plan(kind: MineCutAllKind) -> MineCutAllPlan {
    MineCutAllPlan::empty(kind)
}

fn finish_plan(kind: MineCutAllKind, search: SearchResult) -> MineCutAllPlan {
    let blocks_broken = search.positions.len();
    MineCutAllPlan {
        kind,
        positions: search.positions,
        durability_cost: blocks_broken as u32,
        exhaustion_cost: blocks_broken as f64 * BLOCK_BREAK_EXHAUSTION,
        stopped_by_durability: search.stopped_by_durability,
        skipped_by_claim: search.skipped_by_claim,
    }
}

/// Plan a MineAll expansion for an ore block. Returns an empty
/// [`MineCutAllKind::None`] plan when the trigger is off or `origin` is not an
/// ore candidate; returns a typed empty [`MineCutAllKind::MineAll`] plan when
/// the origin is an ore but claims, tool recognition, or durability prevent
/// any break.
pub fn plan_mine_all<W: MineCutAllWorld + ?Sized>(
    config: &MineCutAllConfig,
    tool: &ToolDescriptor,
    world: &W,
    origin: MineCutAllPos,
    sneaking: bool,
) -> MineCutAllPlan {
    if !config.mine_all.allows(sneaking) {
        return empty_plan(MineCutAllKind::None);
    }
    let Some(block_id) = world.block_id_at(origin) else {
        return empty_plan(MineCutAllKind::None);
    };
    if !is_ore_block(config, world, block_id) {
        return empty_plan(MineCutAllKind::None);
    }
    if !tool.can_mine_block(world, block_id, ToolFamily::Pickaxe) {
        return empty_plan(MineCutAllKind::MineAll);
    }

    let search = search_connected(
        world,
        origin,
        block_id,
        config.effective_max_blocks(),
        tool.durability_budget(config.stop_before_tool_break),
        false,
        true,
    );
    finish_plan(MineCutAllKind::MineAll, search)
}

/// Plan a CutAll expansion for a log/trunk block. Untagged mod trunks are
/// accepted only when [`MineCutAllConfig::allow_leaf_capped_custom_trees`] is
/// enabled and the same-id component is capped by leaf blocks.
pub fn plan_cut_all<W: MineCutAllWorld + ?Sized>(
    config: &MineCutAllConfig,
    tool: &ToolDescriptor,
    world: &W,
    origin: MineCutAllPos,
    sneaking: bool,
) -> MineCutAllPlan {
    if !config.cut_all.allows(sneaking) {
        return empty_plan(MineCutAllKind::None);
    }
    let Some(block_id) = world.block_id_at(origin) else {
        return empty_plan(MineCutAllKind::None);
    };
    let standard_log = is_log_block(config, world, block_id);
    let custom_tree = !standard_log && leaf_capped_trunk_candidate(config, world, origin, block_id);
    if !standard_log && !custom_tree {
        return empty_plan(MineCutAllKind::None);
    }
    if !tool.can_mine_block(world, block_id, ToolFamily::Axe) {
        return empty_plan(MineCutAllKind::CutAll);
    }

    let search = search_connected(
        world,
        origin,
        block_id,
        config.effective_max_blocks(),
        tool.durability_budget(config.stop_before_tool_break),
        true,
        true,
    );
    finish_plan(MineCutAllKind::CutAll, search)
}

/// Convenience entry point: try MineAll first for ore candidates, then CutAll
/// for log/tree candidates. With default configuration both modes are off and
/// this always returns an empty [`MineCutAllKind::None`] plan.
pub fn plan_vein_or_tree_break<W: MineCutAllWorld + ?Sized>(
    config: &MineCutAllConfig,
    tool: &ToolDescriptor,
    world: &W,
    origin: MineCutAllPos,
    sneaking: bool,
) -> MineCutAllPlan {
    let mine = plan_mine_all(config, tool, world, origin, sneaking);
    if !mine.kind.is_none() {
        return mine;
    }
    plan_cut_all(config, tool, world, origin, sneaking)
}
