// SPDX-License-Identifier: LGPL-3.0-or-later
//! Unified recipe & resource browser for the client UI — a clean-room
//! engine covering searchable recipes, craft-tree analysis, world resource
//! reference data, and machine-tutorial visualization:
//!
//! * **Universal recipe index** — a data-driven [`RecipeIndex`] of
//!   [`ItemEntry`]s and [`Recipe`]s whose [`RecipeCategory`] covers every
//!   vanilla crafting type, the whole Create machine family (including
//!   [`RecipeCategory::CreateSequencedAssembly`] with looped
//!   [`SequenceStep`]s), plus [`RecipeCategory::CustomModCategory`] so any
//!   mod or Create addon registers its own categories declaratively.
//!   [`RecipeIndex::load_recipe_json`] ingests standard datapack/mod recipe
//!   JSON fail-closed (malformed input is rejected, never panics).
//! * **Instant multi-filter search** — `plain text`, `@mod`, `#tag` and
//!   `$category` tokens with AND semantics, a searchable item grid, a left
//!   bookmarks panel, `R`/`U` recipes-vs-uses navigation and a `+` button
//!   that emits [`RecipeBrowserAction::TransferToCraftingGrid`].
//! * **Recursive craft trees** — [`RecipeIndex::recipe_tree`] recursively
//!   expands intermediate crafts (including looped sequenced assembly)
//!   into a [`RecipeTree`] of [`TreeNode`]s with hard cycle detection and
//!   a node budget, and totals raw base materials, fluids, byproducts and
//!   the remaining amounts a player inventory still lacks.
//! * **World resource reference data** — [`MobDropEntry`], [`OreDistribution`] (with a
//!   deterministic per-Y histogram rendered as a bar chart),
//!   [`VillagerTradeEntry`], [`DungeonLootEntry`] and
//!   [`EnchantmentCompat`] tabs.
//! * **Machine tutorials** — [`MachineTutorialScene`], a pure-data 3D voxel scene with
//!   step captions and animated parts, rendered through a deterministic
//!   isometric projection rasterized into plain `FillRect` scanlines, so
//!   it works identically on the software rasterizer and the GPU quad
//!   pipeline with zero extra shader work.
//!
//! Like [`crate::mod_manager`], rendering produces a deterministic
//! [`UiDrawList`] plus a parallel `Vec<`[`RecipeHitRegion`]`>`; input
//! arrives as device-agnostic [`UiInputEvent`]s and returns
//! [`RecipeBrowserAction`]s the embedding application performs. Pure
//! `std` — no GPU resources, no I/O, no external crates.

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;

use megu_core::json::{parse_json_with_limits, JsonLimits, JsonValue};
use megu_core::math::Mth;
use megu_core::search_index::{SearchDocId, SearchHit, TrigramIndex};

use crate::draw::{palette, text_width, UiColor, UiDrawList, UiRect};
use crate::recipe_overlay;
use crate::recipe_search;
use crate::state::{tr, GamepadButton, UiInputEvent, UiKey, UiLang};

// ---------------------------------------------------------------------------
// Items, stacks and ingredients
// ---------------------------------------------------------------------------

/// One item in the catalog the browser searches and displays.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ItemEntry {
    /// Namespaced id (`"minecraft:iron_ingot"`, `"create:cogwheel"`).
    pub id: String,
    /// English (`en_us`) display name — search always matches it.
    pub display_name: String,
    /// `locale code → translated name` (`"ja_jp"`, `"fr_fr"`, `"ko_kr"`,
    /// ...) loaded from `assets/<ns>/lang/<locale>.json`. Search matches
    /// the *active* display locale's entry (and English always), so any
    /// mod and any locale work without hardcoding.
    pub localized_names: BTreeMap<String, String>,
    /// Owning mod namespace (`@mod` filter token).
    pub mod_id: String,
    /// Item tags (`#tag` filter token), e.g. `c:ingots/iron`.
    pub tags: Vec<String>,
    /// Extra tooltip lines searched by plain-text and `%tooltip` tokens.
    pub tooltip_lines: Vec<String>,
}

impl ItemEntry {
    /// Minimal entry: the mod id is the namespace of `id`.
    pub fn new(id: &str, display_name: &str) -> Self {
        Self {
            id: id.to_owned(),
            display_name: display_name.to_owned(),
            localized_names: BTreeMap::new(),
            mod_id: id.split(':').next().unwrap_or("minecraft").to_owned(),
            tags: Vec::new(),
            tooltip_lines: Vec::new(),
        }
    }

    /// Name shown for `locale` (`"ja_jp"`): the translated entry when the
    /// lang file declared one, else the English [`display_name`].
    pub fn name_for_locale(&self, locale: &str) -> &str {
        if locale == "en_us" {
            return &self.display_name;
        }
        self.localized_names
            .get(locale)
            .map(String::as_str)
            .unwrap_or(&self.display_name)
    }
}

/// A counted item reference.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ItemStack {
    pub item: String,
    pub count: u32,
}

/// A counted fluid reference (millibuckets).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FluidStack {
    pub fluid: String,
    pub amount_mb: u32,
}

/// One recipe input slot: either a concrete item or a tag that resolves
/// against the [`RecipeIndex`] item catalog.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RecipeIngredient {
    /// A specific item id plus count.
    Item { id: String, count: u32 },
    /// Any item carrying `tag`; count applies to whichever resolves.
    Tag { tag: String, count: u32 },
}

impl RecipeIngredient {
    /// Convenience constructor for a concrete item input.
    pub fn item(id: &str, count: u32) -> Self {
        Self::Item {
            id: id.to_owned(),
            count: count.max(1),
        }
    }

    /// Convenience constructor for a tag input.
    pub fn tag(tag: &str, count: u32) -> Self {
        Self::Tag {
            tag: tag.to_owned(),
            count: count.max(1),
        }
    }

    /// How many units this slot consumes per craft.
    pub fn count(&self) -> u32 {
        match self {
            Self::Item { count, .. } | Self::Tag { count, .. } => *count,
        }
    }
}

/// `"some_mod:my_item/thing"` → `"Thing"` (title case) — the name a
/// lang-less catalog row shows. Mirror of the host's `humanize_id`.
fn humanize_item_id(id: &str) -> String {
    let path = id.rsplit([':', '/']).next().unwrap_or(id);
    let mut out = String::with_capacity(path.len());
    let mut cap = true;
    for ch in path.chars() {
        if ch == '_' || ch == '-' {
            out.push(' ');
            cap = true;
        } else if cap {
            out.extend(ch.to_uppercase());
            cap = false;
        } else {
            out.push(ch);
        }
    }
    if out.is_empty() {
        id.to_owned()
    } else {
        out
    }
}

/// Deterministic swatch color for an item id (no item textures exist in
/// this crate, so the grid paints a stable per-id color + first letter).
pub fn item_color(item_id: &str) -> UiColor {
    // FNV-1a over the id bytes → index into a fixed 16-color ramp.
    const RAMP: [UiColor; 16] = [
        UiColor::rgb(188, 84, 84),
        UiColor::rgb(188, 132, 84),
        UiColor::rgb(188, 172, 84),
        UiColor::rgb(140, 188, 84),
        UiColor::rgb(84, 188, 96),
        UiColor::rgb(84, 188, 150),
        UiColor::rgb(84, 178, 188),
        UiColor::rgb(84, 136, 188),
        UiColor::rgb(96, 96, 188),
        UiColor::rgb(140, 84, 188),
        UiColor::rgb(188, 84, 168),
        UiColor::rgb(188, 84, 116),
        UiColor::rgb(150, 150, 160),
        UiColor::rgb(110, 124, 84),
        UiColor::rgb(96, 110, 140),
        UiColor::rgb(176, 148, 96),
    ];
    let mut hash: u32 = 0x811c_9dc5;
    for &b in item_id.as_bytes() {
        hash = (hash ^ u32::from(b)).wrapping_mul(0x0100_0193);
    }
    RAMP[(hash >> 16) as usize & 15]
}

// ---------------------------------------------------------------------------
// Recipe categories & recipes
// ---------------------------------------------------------------------------

/// Kinetic heat a Create-style basin recipe requires.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum HeatRequirement {
    /// No external heat (or a non-Create recipe).
    #[default]
    None,
    /// Needs a blaze burner / heated block beneath.
    Heated,
    /// Needs a superheated (blazing) burner.
    Superheated,
}

impl HeatRequirement {
    /// Parse the JSON `heatRequirement` field; anything else is `None`.
    fn from_json_str(s: &str) -> Self {
        match s.to_ascii_lowercase().as_str() {
            "heated" => Self::Heated,
            "superheated" => Self::Superheated,
            _ => Self::None,
        }
    }

    pub fn label(self, lang: UiLang) -> &'static str {
        match self {
            Self::None => tr(lang, "No Heat", "加熱不要"),
            Self::Heated => tr(lang, "Heated", "加熱"),
            Self::Superheated => tr(lang, "Superheated", "過熱"),
        }
    }
}

/// Every recipe category the browser understands.
///
/// Vanilla covers the full crafting/cooking/smithing set; the `Create*`
/// family covers the kinetic machine ecosystem; and
/// [`CustomModCategory`](Self::CustomModCategory) lets **any** mod or
/// Create addon register its own categories declaratively — see
/// [`RecipeIndex::register_custom_category`] and
/// [`RecipeIndex::register_mod_namespace`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum RecipeCategory {
    #[default]
    CraftingShaped,
    CraftingShapeless,
    Smelting,
    Blasting,
    Smoking,
    Campfire,
    Stonecutting,
    Smithing,
    Brewing,
    Anvil,
    CreateMixing,
    CreateCompacting,
    CreateCrushing,
    CreateMilling,
    CreatePressing,
    CreateDeploying,
    CreateSequencedAssembly,
    CreateMechanicalCrafting,
    CreateSplashing,
    CreateHaunting,
    CreateSandpaperPolishing,
    CreateItemApplication,
    /// A mod-declared category (`id` is the recipe `"type"` it matches).
    CustomModCategory {
        id: String,
        title_en: String,
        title_ja: String,
    },
}

impl RecipeCategory {
    /// The recipe JSON `"type"` this category ingests / reports.
    pub fn type_id(&self) -> String {
        match self {
            Self::CraftingShaped => "minecraft:crafting_shaped",
            Self::CraftingShapeless => "minecraft:crafting_shapeless",
            Self::Smelting => "minecraft:smelting",
            Self::Blasting => "minecraft:blasting",
            Self::Smoking => "minecraft:smoking",
            Self::Campfire => "minecraft:campfire_cooking",
            Self::Stonecutting => "minecraft:stonecutting",
            Self::Smithing => "minecraft:smithing_transform",
            Self::Brewing => "minecraft:brewing",
            Self::Anvil => "minecraft:anvil",
            Self::CreateMixing => "create:mixing",
            Self::CreateCompacting => "create:compacting",
            Self::CreateCrushing => "create:crushing",
            Self::CreateMilling => "create:milling",
            Self::CreatePressing => "create:pressing",
            Self::CreateDeploying => "create:deploying",
            Self::CreateSequencedAssembly => "create:sequenced_assembly",
            Self::CreateMechanicalCrafting => "create:mechanical_crafting",
            Self::CreateSplashing => "create:splashing",
            Self::CreateHaunting => "create:haunting",
            Self::CreateSandpaperPolishing => "create:sandpaper_polishing",
            Self::CreateItemApplication => "create:item_application",
            Self::CustomModCategory { id, .. } => return id.clone(),
        }
        .to_owned()
    }

    /// Short lowercase token matched by the `$category` search filter:
    /// `$crushing` hits [`Self::CreateCrushing`], `$sequenced` hits
    /// [`Self::CreateSequencedAssembly`].
    pub fn search_id(&self) -> &str {
        match self {
            Self::CraftingShaped => "crafting",
            Self::CraftingShapeless => "shapeless",
            Self::Smelting => "smelting",
            Self::Blasting => "blasting",
            Self::Smoking => "smoking",
            Self::Campfire => "campfire",
            Self::Stonecutting => "stonecutting",
            Self::Smithing => "smithing",
            Self::Brewing => "brewing",
            Self::Anvil => "anvil",
            Self::CreateMixing => "mixing",
            Self::CreateCompacting => "compacting",
            Self::CreateCrushing => "crushing",
            Self::CreateMilling => "milling",
            Self::CreatePressing => "pressing",
            Self::CreateDeploying => "deploying",
            Self::CreateSequencedAssembly => "sequenced",
            Self::CreateMechanicalCrafting => "mechanical",
            Self::CreateSplashing => "splashing",
            Self::CreateHaunting => "haunting",
            Self::CreateSandpaperPolishing => "polishing",
            Self::CreateItemApplication => "application",
            Self::CustomModCategory { id, .. } => id.as_str(),
        }
    }

    /// `true` when the `$category` token names this category (substring on
    /// the short id or the full type id, so `$mech`, `$create:crushing`
    /// and `$crushing` all work).
    pub fn matches_query(&self, token: &str) -> bool {
        let token = token.to_ascii_lowercase();
        !token.is_empty()
            && (self.search_id().to_ascii_lowercase().contains(&token)
                || self.type_id().to_ascii_lowercase().contains(&token))
    }

    /// Whether this is a Create-family (or declared addon) machine recipe.
    pub fn is_machine(&self) -> bool {
        !matches!(
            self,
            Self::CraftingShaped
                | Self::CraftingShapeless
                | Self::Smelting
                | Self::Blasting
                | Self::Smoking
                | Self::Campfire
                | Self::Stonecutting
                | Self::Smithing
                | Self::Brewing
                | Self::Anvil
        )
    }

    /// Human title for recipe-row headers.
    pub fn title(&self, lang: UiLang) -> String {
        match self {
            Self::CraftingShaped => tr(lang, "Crafting (Shaped)", "クラフト (定型)"),
            Self::CraftingShapeless => tr(lang, "Crafting (Shapeless)", "クラフト (不定形)"),
            Self::Smelting => tr(lang, "Smelting", "精錬"),
            Self::Blasting => tr(lang, "Blasting", "高炉"),
            Self::Smoking => tr(lang, "Smoking", "燻製"),
            Self::Campfire => tr(lang, "Campfire", "焚き火"),
            Self::Stonecutting => tr(lang, "Stonecutting", "石切台"),
            Self::Smithing => tr(lang, "Smithing", "鍛造"),
            Self::Brewing => tr(lang, "Brewing", "醸造"),
            Self::Anvil => tr(lang, "Anvil", "金床"),
            Self::CreateMixing => tr(lang, "Create: Mixing", "Create: 混合"),
            Self::CreateCompacting => tr(lang, "Create: Compacting", "Create: 圧縮"),
            Self::CreateCrushing => tr(lang, "Create: Crushing", "Create: 粉砕"),
            Self::CreateMilling => tr(lang, "Create: Milling", "Create: 製粉"),
            Self::CreatePressing => tr(lang, "Create: Pressing", "Create: プレス"),
            Self::CreateDeploying => tr(lang, "Create: Deploying", "Create: デプロイ"),
            Self::CreateSequencedAssembly => {
                tr(lang, "Create: Sequenced Assembly", "Create: 順次組立")
            }
            Self::CreateMechanicalCrafting => {
                tr(lang, "Create: Mechanical Crafting", "Create: 機械クラフト")
            }
            Self::CreateSplashing => tr(lang, "Create: Splashing", "Create: 水洗"),
            Self::CreateHaunting => tr(lang, "Create: Haunting", "Create: 魂変換"),
            Self::CreateSandpaperPolishing => {
                tr(lang, "Create: Sandpaper Polishing", "Create: 研磨")
            }
            Self::CreateItemApplication => {
                tr(lang, "Create: Item Application", "Create: アイテム適用")
            }
            Self::CustomModCategory {
                title_en, title_ja, ..
            } => tr(lang, title_en, title_ja),
        }
        .to_owned()
    }
}

/// One operation inside a [`RecipeCategory::CreateSequencedAssembly`]
/// recipe (`deploying`, `pressing`, `filling`, `cutting`, … from any mod).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SequenceStep {
    /// The operation's category (parsed from the nested recipe `"type"`).
    pub category: RecipeCategory,
    /// Extra ingredients this step consumes. The transitional
    /// in-progress item is skipped during material accounting — only
    /// genuinely consumed inputs (e.g. the deployed item) are kept.
    pub inputs: Vec<RecipeIngredient>,
    /// Create deploying's `keepHeldItem` flag: the deployed item returns
    /// to the belt instead of being consumed.
    pub keep_held_item: bool,
}

/// A complete recipe record.
///
/// `outputs` holds guaranteed products; `chanced_outputs` holds
/// `(item_id, count, chance 0..=1)` rolls (Create crushing/splashing and
/// sequenced-assembly results). Create machine metadata lives in
/// `rpm_min`, `su_cost`, `heat`, `fluid_inputs`/`fluid_outputs`, and —
/// for sequenced assembly — `sequence_steps`, `transitional_item` and
/// `loops`.
#[derive(Clone, Debug, PartialEq)]
pub struct Recipe {
    /// Unique id (the datapack/mod recipe path).
    pub id: String,
    pub category: RecipeCategory,
    pub inputs: Vec<RecipeIngredient>,
    pub outputs: Vec<ItemStack>,
    /// `(item_id, count, chance)` for probabilistic products.
    pub chanced_outputs: Vec<(String, u32, f32)>,
    pub fluid_inputs: Vec<FluidStack>,
    pub fluid_outputs: Vec<FluidStack>,
    /// Minimum RPM the kinetic network must supply (Create).
    pub rpm_min: Option<u32>,
    /// Stress units consumed per operation, in SU·ticks (Create).
    pub su_cost: Option<u64>,
    /// Basin heat requirement (Create mixing/compacting).
    pub heat: HeatRequirement,
    /// Ordered operations for [`RecipeCategory::CreateSequencedAssembly`].
    pub sequence_steps: Vec<SequenceStep>,
    /// The in-progress item carried between sequence steps.
    pub transitional_item: String,
    /// Times the whole sequence repeats (≥ 1 for sequenced assembly).
    pub loops: u32,
    /// Processing duration in ticks when the recipe type declares one.
    pub processing_ticks: u32,
    /// Shaped recipes only: `grid[row*3+col]` = index into `inputs`.
    pub grid: Option<Vec<Option<usize>>>,
}

impl Recipe {
    pub fn new(id: &str, category: RecipeCategory) -> Self {
        Self {
            id: id.to_owned(),
            category,
            inputs: Vec::new(),
            outputs: Vec::new(),
            chanced_outputs: Vec::new(),
            fluid_inputs: Vec::new(),
            fluid_outputs: Vec::new(),
            rpm_min: None,
            su_cost: None,
            heat: HeatRequirement::None,
            sequence_steps: Vec::new(),
            transitional_item: String::new(),
            loops: 0,
            processing_ticks: 0,
            grid: None,
        }
    }

    /// Guaranteed output count for `item` (excluding chanced rolls).
    pub fn output_count_for(&self, item: &str) -> u64 {
        self.outputs
            .iter()
            .filter(|o| o.item == item)
            .map(|o| u64::from(o.count))
            .sum()
    }

    /// Whether `item` is produced by this recipe (guaranteed or chanced).
    pub fn produces(&self, item: &str) -> bool {
        self.output_count_for(item) > 0 || self.chanced_outputs.iter().any(|(i, ..)| i == item)
    }

    /// Effective per-craft material cost as `(resolved_item, count)`
    /// pairs: tag slots resolve to the lexicographically-first catalog
    /// item carrying the tag (unresolved tags stay as `#tag` so they
    /// surface in the raw-material total instead of vanishing), and
    /// sequenced-assembly steps contribute their non-transitional inputs
    /// multiplied by `loops`.
    pub fn effective_inputs(&self, index: &RecipeIndex) -> Vec<(String, u64)> {
        let mut out: Vec<(String, u64)> = Vec::new();
        let push = |out: &mut Vec<(String, u64)>, id: String, count: u64| {
            if let Some(e) = out.iter_mut().find(|(i, _)| *i == id) {
                e.1 += count;
            } else {
                out.push((id, count));
            }
        };
        for ing in &self.inputs {
            let id = index.resolve_ingredient(ing);
            push(&mut out, id, u64::from(ing.count()));
        }
        if self.category == RecipeCategory::CreateSequencedAssembly {
            let loops = self.loops.max(1) as u64;
            for step in &self.sequence_steps {
                for ing in &step.inputs {
                    let id = index.resolve_ingredient(ing);
                    if !self.transitional_item.is_empty() && id == self.transitional_item {
                        continue;
                    }
                    push(&mut out, id, u64::from(ing.count()) * loops);
                }
            }
        }
        out
    }
}

// ---------------------------------------------------------------------------
// World drops and resource reference data
// ---------------------------------------------------------------------------

/// One mob-drop table row.
#[derive(Clone, Debug, PartialEq)]
pub struct MobDropEntry {
    /// Mob id (`"minecraft:zombie"`).
    pub mob_id: String,
    pub item_id: String,
    pub min_count: u32,
    pub max_count: u32,
    /// Drop chance `0.0..=1.0` (`1.0` = guaranteed common drop).
    pub chance: f32,
    /// Extra items gained per Looting level.
    pub looting_bonus: u32,
    /// Free-form condition text (`"only in nether"`, `""` = none).
    pub condition: String,
}

/// Ore generation profile for one block in one dimension.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OreDistribution {
    pub block_id: String,
    /// `"minecraft:overworld"` / `"minecraft:the_nether"` / …
    pub dimension: String,
    pub min_y: i32,
    pub max_y: i32,
    /// Y level with the highest weight.
    pub peak_y: i32,
    /// Typical blocks per vein — scales the histogram height.
    pub vein_size: u32,
    /// Optional biome filter (`""` = every biome).
    pub biome: String,
}

impl OreDistribution {
    /// Weight at one Y level: a deterministic triangle ramping from
    /// `min_y` up to `peak_y` and back down to `max_y` — the classic triangular
    /// distribution rendered as a bar chart.
    pub fn weight_at(&self, y: i32) -> u32 {
        if y < self.min_y || y > self.max_y || self.vein_size == 0 {
            return 0;
        }
        let peak = self.peak_y.clamp(self.min_y, self.max_y);
        let (span, dist) = if y <= peak {
            ((peak - self.min_y).max(1), peak - y)
        } else {
            ((self.max_y - peak).max(1), y - peak)
        };
        // Integer-only triangle: vein_size at the peak, ~1 at the edges.
        let num = u64::from(self.vein_size) * (span as u64 + 1 - dist as u64);
        let w = num / (span as u64 + 1);
        w.max(1) as u32
    }

    /// One weight per Y level in `min_y..=max_y`, aligned with the axis.
    pub fn histogram(&self) -> Vec<u32> {
        (self.min_y..=self.max_y).map(|y| self.weight_at(y)).collect()
    }
}

/// One villager trade offer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VillagerTradeEntry {
    /// `"minecraft:librarian"` etc.
    pub profession: String,
    /// Required villager level `1..=5` (novice..master).
    pub level: u8,
    pub buy_a: (String, u32),
    /// Optional second payment slot.
    pub buy_b: Option<(String, u32)>,
    pub sell: (String, u32),
    pub max_uses: u32,
}

/// One chest-loot table row.
#[derive(Clone, Debug, PartialEq)]
pub struct DungeonLootEntry {
    /// Loot table id (`"minecraft:chests/simple_dungeon"`).
    pub chest: String,
    pub item_id: String,
    pub min_count: u32,
    pub max_count: u32,
    /// Table weight used for the displayed chance.
    pub weight: u32,
    /// Expected chance `0.0..=1.0` of seeing the item per chest roll.
    pub chance: f32,
}

/// Which enchantments apply to one item.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EnchantmentCompat {
    pub item_id: String,
    /// `(enchantment id, max level)` pairs.
    pub enchantments: Vec<(String, u32)>,
}

// ---------------------------------------------------------------------------
// Fail-closed JSON parsing — canonical `megu_core::json` reader adapted to
// this module's `Json` tree (recipe datapack ingestion)
// ---------------------------------------------------------------------------

/// Parsed JSON value. Numbers are `f64` — recipe files only carry small
/// ints and unit chances, so precision is a non-issue.
#[derive(Clone, Debug, PartialEq)]
enum Json {
    Null,
    Bool(bool),
    Num(f64),
    Str(String),
    Arr(Vec<Json>),
    Obj(BTreeMap<String, Json>),
}

impl Json {
    fn as_object(&self) -> Option<&BTreeMap<String, Json>> {
        match self {
            Self::Obj(m) => Some(m),
            _ => None,
        }
    }

    fn as_array(&self) -> Option<&[Json]> {
        match self {
            Self::Arr(a) => Some(a),
            _ => None,
        }
    }

    fn as_str(&self) -> Option<&str> {
        match self {
            Self::Str(s) => Some(s),
            _ => None,
        }
    }

    fn as_f64(&self) -> Option<f64> {
        match self {
            Self::Num(n) => Some(*n),
            _ => None,
        }
    }

    fn as_u64(&self) -> Option<u64> {
        self.as_f64()
            .and_then(|n| (n.is_finite() && n >= 0.0).then_some(n as u64))
    }

    fn as_bool(&self) -> Option<bool> {
        match self {
            Self::Bool(b) => Some(*b),
            _ => None,
        }
    }

    /// Object member lookup (no-op on non-objects).
    fn get(&self, key: &str) -> Option<&Json> {
        self.as_object()?.get(key)
    }
}

/// Deepest accepted nesting — recipe JSONs are flat, so anything beyond
/// this is hostile/cyclic input and rejected instead of overflowing.
const JSON_MAX_DEPTH: usize = 48;

/// Bounds for [`parse_json`], matching the retired parser: nesting deeper
/// than [`JSON_MAX_DEPTH`] levels rejected, no node or byte ceiling of its
/// own — recipe documents are already small and caller-bounded.
const JSON_LIMITS: JsonLimits = JsonLimits {
    max_depth: JSON_MAX_DEPTH + 1,
    max_nodes: usize::MAX,
    max_bytes: usize::MAX,
};

/// Parse `src` into a [`Json`] value; `None` on any malformed input.
fn parse_json(src: &str) -> Option<Json> {
    let value = parse_json_with_limits(src, JSON_LIMITS, false).ok()?;
    Some(json_value_to_json(&value))
}

/// Fold the canonical [`JsonValue`] tree into this module's [`Json`]:
/// document-ordered members collapse into a `BTreeMap` with
/// last-duplicate-key wins, exactly like the retired parser's `insert`.
fn json_value_to_json(value: &JsonValue) -> Json {
    match value {
        JsonValue::Null => Json::Null,
        JsonValue::Bool(b) => Json::Bool(*b),
        JsonValue::Number(n) => Json::Num(*n),
        JsonValue::String(s) => Json::Str(s.clone()),
        JsonValue::Array(items) => Json::Arr(items.iter().map(json_value_to_json).collect()),
        JsonValue::Object(members) => {
            let mut map = BTreeMap::new();
            for (key, member) in members {
                map.insert(key.clone(), json_value_to_json(member));
            }
            Json::Obj(map)
        }
    }
}

// ---------------------------------------------------------------------------
// Recipe index: catalog, recipes, world resource data, custom categories
// ---------------------------------------------------------------------------

/// The universal data-driven item + recipe + resource index the browser
/// renders. The host (datapack loader, mod adapter) fills it; the UI only
/// reads it.
/// One furnace fuel row (the world-resource "Fuels" tab): `burn_ticks` is the
/// NeoForge `furnace_fuels` data-map `burn_time`; vanilla smelts one
/// item per 200 ticks.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FuelEntry {
    /// Fuel item id (`"minecraft:coal"`) or tag (`"#minecraft:logs"`).
    pub item: String,
    pub burn_ticks: u32,
}

impl FuelEntry {
    /// Items a single unit smelts (`burn_ticks / 200`).
    pub fn items_smelted(&self) -> f32 {
        self.burn_ticks as f32 / 200.0
    }
}

#[derive(Debug, Default)]
pub struct RecipeIndex {
    pub items: Vec<ItemEntry>,
    pub recipes: Vec<Recipe>,
    pub mob_drops: Vec<MobDropEntry>,
    pub ore_distributions: Vec<OreDistribution>,
    pub villager_trades: Vec<VillagerTradeEntry>,
    pub dungeon_loot: Vec<DungeonLootEntry>,
    pub enchant_compat: Vec<EnchantmentCompat>,
    /// Furnace fuels — data-map + tag driven, zero hardcoded item lists.
    pub fuels: Vec<FuelEntry>,
    /// Explicitly registered custom categories (exact `"type"` match).
    custom_categories: Vec<RecipeCategory>,
    /// `(namespace, title_en, title_ja)` — any recipe `"type"` inside a
    /// registered namespace becomes a declared
    /// [`RecipeCategory::CustomModCategory`] on load.
    custom_namespaces: Vec<(String, String, String)>,
    /// Character-trigram shortlist over every field
    /// `item_text_score` can match: English + every localized name
    /// (raw, kana-folded and romaji-folded), id, mod, tags and
    /// tooltips. For a positive text token its `query` hits are a
    /// strict superset of the items the exact scorer accepts (a
    /// substring's trigrams are a subset of its field's trigrams;
    /// `score_field` also covers prefix/subsequence matches), so the
    /// per-item gate keeps results identical while the full-catalog
    /// scan is skipped. `RefCell` because `TrigramIndex::query`
    /// reuses internal scratch (`&mut`) while
    /// [`RecipeBrowserState::visible_items`] is a `&self` API.
    text_trigram: RefCell<TrigramIndex>,
    /// Fingerprint [`Self::search_fingerprint`] the trigram index was
    /// built against — `None` until first build. The fingerprint
    /// hashes every scored byte, so mutation through the `pub` fields
    /// (direct `items` pushes, in-place renames, lang merges) can
    /// never serve a stale shortlist.
    text_trigram_fp: Cell<Option<u64>>,
}

/// `TrigramIndex` has no `Clone` — the clone keeps the catalog and
/// lazily rebuilds the shortlist on first search.
impl Clone for RecipeIndex {
    fn clone(&self) -> Self {
        Self {
            items: self.items.clone(),
            recipes: self.recipes.clone(),
            mob_drops: self.mob_drops.clone(),
            ore_distributions: self.ore_distributions.clone(),
            villager_trades: self.villager_trades.clone(),
            dungeon_loot: self.dungeon_loot.clone(),
            enchant_compat: self.enchant_compat.clone(),
            fuels: self.fuels.clone(),
            custom_categories: self.custom_categories.clone(),
            custom_namespaces: self.custom_namespaces.clone(),
            text_trigram: RefCell::new(TrigramIndex::new()),
            text_trigram_fp: Cell::new(None),
        }
    }
}

/// Every field `item_text_score` can score, as index documents:
/// English + each localized name (raw lowercase, kana-folded and
/// romaji-folded — the same transforms the search fallback applies),
/// id, mod, tags and tooltips.
fn search_doc_fields(item: &ItemEntry) -> Vec<String> {
    let mut fields = vec![
        item.display_name.to_lowercase(),
        item.id.to_lowercase(),
        item.mod_id.to_lowercase(),
    ];
    for tag in &item.tags {
        fields.push(tag.to_lowercase());
    }
    for line in &item.tooltip_lines {
        fields.push(line.to_lowercase());
    }
    for name in item.localized_names.values() {
        let lc = name.to_lowercase();
        let folded = recipe_search::normalize_kana(&lc);
        let romaji = recipe_search::glide_fold(&recipe_search::kana_to_romaji(&folded));
        fields.push(lc);
        fields.push(folded);
        if !romaji.is_empty() {
            fields.push(romaji);
        }
    }
    fields
}

impl RecipeIndex {
    pub fn new() -> Self {
        Self::default()
    }

    /// FNV-1a over every byte `item_text_score` can score. Compared
    /// against [`Self::text_trigram_fp`] to decide whether the trigram
    /// shortlist is still fresh — hashing the field bytes themselves
    /// (not just lengths) means even in-place `pub`-field edits are
    /// detected.
    fn search_fingerprint(&self) -> u64 {
        let mut h = 0xcbf29ce484222325u64;
        let mut mix = |s: &str| {
            for &b in s.as_bytes() {
                h ^= b as u64;
                h = h.wrapping_mul(0x100000001b3);
            }
            h ^= 0xff;
            h = h.wrapping_mul(0x100000001b3);
        };
        for item in &self.items {
            mix(&item.id);
            mix(&item.display_name);
            mix(&item.mod_id);
            for tag in &item.tags {
                mix(tag);
            }
            for line in &item.tooltip_lines {
                mix(line);
            }
            for (locale, name) in &item.localized_names {
                mix(locale);
                mix(name);
            }
        }
        h
    }

    /// Build the trigram shortlist when the catalog moved since the
    /// last build (or it was never built). No-op when fresh.
    fn ensure_text_index(&self) {
        let fp = self.search_fingerprint();
        if self.text_trigram_fp.get() == Some(fp) {
            return;
        }
        let mut index = TrigramIndex::new();
        for (i, item) in self.items.iter().enumerate() {
            let fields = search_doc_fields(item);
            if let Some((primary, aliases)) = fields.split_first() {
                index.add_document(SearchDocId(i as u32), primary, aliases);
            }
        }
        *self.text_trigram.borrow_mut() = index;
        self.text_trigram_fp.set(Some(fp));
    }

    /// Item indices the trigram shortlist admits for `token` — the
    /// union of hits for the token itself plus every
    /// [`recipe_search::query_variants`] expansion, so the kana/romaji
    /// fallback in `item_text_score` stays inside the candidate set.
    /// Always a superset of the token's true match set.
    fn text_candidates(&self, token: &str, out_ids: &mut Vec<u32>) {
        self.ensure_text_index();
        let index = &mut *self.text_trigram.borrow_mut();
        let mut hits: Vec<SearchHit> = Vec::new();
        // `query` clears `out` on every call — drain into `out_ids`
        // after each probe or earlier results are lost.
        index.query(token, usize::MAX, &mut hits);
        out_ids.extend(hits.iter().map(|hit| hit.doc.0));
        for variant in recipe_search::query_variants(token) {
            if variant.is_empty() {
                continue;
            }
            index.query(&variant, usize::MAX, &mut hits);
            out_ids.extend(hits.iter().map(|hit| hit.doc.0));
        }
    }

    pub fn add_item(&mut self, entry: ItemEntry) {
        self.items.push(entry);
    }

    pub fn add_recipe(&mut self, recipe: Recipe) {
        self.recipes.push(recipe);
    }

    /// Declare a custom category so `load_recipe_json` accepts recipe
    /// `"type": "<id>"` and titles it bilingually.
    pub fn register_custom_category(&mut self, id: &str, title_en: &str, title_ja: &str) {
        self.custom_categories.push(RecipeCategory::CustomModCategory {
            id: id.to_owned(),
            title_en: title_en.to_owned(),
            title_ja: title_ja.to_owned(),
        });
    }

    /// Declare a whole mod namespace: every `ns:whatever` recipe type
    /// loads as a declared custom category named after the mod.
    pub fn register_mod_namespace(&mut self, namespace: &str, title_en: &str, title_ja: &str) {
        self.custom_namespaces.push((
            namespace.to_owned(),
            title_en.to_owned(),
            title_ja.to_owned(),
        ));
    }

    /// Catalog entry for `id`, if present.
    pub fn item(&self, id: &str) -> Option<&ItemEntry> {
        self.items.iter().find(|e| e.id == id)
    }

    /// Display name for `id` (falls back to the raw id).
    pub fn display_name(&self, id: &str) -> String {
        self.item(id)
            .map(|e| e.display_name.clone())
            .unwrap_or_else(|| id.to_owned())
    }

    /// Display name for `id` under `locale` (`"ja_jp"`, `"fr_fr"`, ...):
    /// the item's `assets/*/lang/<locale>.json` name when loaded, else
    /// the English name, else the raw id.
    pub fn display_name_for_locale(&self, id: &str, locale: &str) -> String {
        self.item(id)
            .map(|e| e.name_for_locale(locale).to_owned())
            .unwrap_or_else(|| id.to_owned())
    }

    /// Merge a `assets/<ns>/lang/<locale>.json` map into the catalog:
    /// `item.<ns>.<path>` / `block.<ns>.<path>` keys stamp
    /// `localized_names[locale]` on existing entries (unknown ids create
    /// a catalog row so lang-only items still surface in search).
    pub fn apply_lang_map(&mut self, locale: &str, map: &BTreeMap<String, String>) {
        for (key, name) in map {
            let Some((kind, rest)) = key.split_once('.') else {
                continue;
            };
            if !matches!(kind, "item" | "block") {
                continue;
            }
            let Some((ns, path)) = rest.split_once('.') else {
                continue;
            };
            if ns.is_empty() || path.is_empty() {
                continue;
            }
            let id = format!("{ns}:{path}");
            if let Some(entry) = self.items.iter_mut().find(|e| e.id == id) {
                entry
                    .localized_names
                    .insert(locale.to_owned(), name.clone());
            } else {
                let mut entry = ItemEntry::new(&id, name);
                if locale != "en_us" {
                    entry.display_name = humanize_item_id(&id);
                    entry
                        .localized_names
                        .insert(locale.to_owned(), name.clone());
                }
                self.items.push(entry);
            }
        }
    }

    /// Sorted catalog ids carrying `tag` (drives tag-slot icon cycling).
    pub fn tag_members(&self, tag: &str) -> Vec<String> {
        let mut out: Vec<String> = self
            .items
            .iter()
            .filter(|e| e.tags.iter().any(|t| t == tag))
            .map(|e| e.id.clone())
            .collect();
        out.sort();
        out
    }

    /// First catalog item carrying `tag` (lexicographic order — the
    /// deterministic pick used by the material calculator).
    pub fn first_in_tag(&self, tag: &str) -> Option<String> {
        self.items
            .iter()
            .filter(|e| e.tags.iter().any(|t| t == tag))
            .map(|e| e.id.clone())
            .min()
    }

    /// Resolve an ingredient slot to a concrete item id (or `#tag` when
    /// the tag is unknown to the catalog).
    pub fn resolve_ingredient(&self, ing: &RecipeIngredient) -> String {
        match ing {
            RecipeIngredient::Item { id, .. } => id.clone(),
            RecipeIngredient::Tag { tag, .. } => {
                self.first_in_tag(tag).unwrap_or_else(|| format!("#{tag}"))
            }
        }
    }

    /// Like [`resolve_ingredient`](Self::resolve_ingredient) but cycles
    /// tag slots through their members — `tick/20` picks the shown item
    /// (~1 Hz at 20 tps), matching recipe overlay tag cycling.
    pub fn resolve_ingredient_cycled(&self, ing: &RecipeIngredient, tick: u64) -> String {
        match ing {
            RecipeIngredient::Item { id, .. } => id.clone(),
            RecipeIngredient::Tag { tag, .. } => {
                let members = self.tag_members(tag);
                if members.is_empty() {
                    format!("#{tag}")
                } else {
                    members[(tick / 20) as usize % members.len()].clone()
                }
            }
        }
    }

    /// `true` when `item` satisfies this ingredient (id match, or the
    /// item carries the ingredient's tag).
    pub fn ingredient_accepts(&self, ing: &RecipeIngredient, item: &str) -> bool {
        match ing {
            RecipeIngredient::Item { id, .. } => id == item,
            RecipeIngredient::Tag { tag, .. } => self
                .item(item)
                .is_some_and(|e| e.tags.iter().any(|t| t == tag)),
        }
    }

    /// Every recipe producing `item` (guaranteed or chanced), in index
    /// order.
    pub fn recipes_producing(&self, item: &str) -> Vec<&Recipe> {
        self.recipes.iter().filter(|r| r.produces(item)).collect()
    }

    /// Every recipe consuming `item` — direct id match, tag-slot match,
    /// or a sequenced-assembly step input.
    pub fn recipes_consuming(&self, item: &str) -> Vec<&Recipe> {
        self.recipes
            .iter()
            .filter(|r| {
                r.inputs.iter().any(|i| self.ingredient_accepts(i, item))
                    || r.sequence_steps
                        .iter()
                        .flat_map(|s| s.inputs.iter())
                        .any(|i| self.ingredient_accepts(i, item))
                    || r.fluid_inputs.iter().any(|f| f.fluid == item)
            })
            .collect()
    }

    /// Recipe lookup by id.
    pub fn recipe(&self, id: &str) -> Option<&Recipe> {
        self.recipes.iter().find(|r| r.id == id)
    }

    /// The recipe the material calculator uses for `item`: the explicit
    /// `selections` override first, then the first producing recipe in
    /// index order.
    pub fn chosen_recipe_for<'a>(
        &'a self,
        item: &str,
        selections: &BTreeMap<String, String>,
    ) -> Option<&'a Recipe> {
        if let Some(id) = selections.get(item) {
            if let Some(r) = self.recipe(id) {
                if r.produces(item) {
                    return Some(r);
                }
            }
        }
        self.recipes_producing(item).into_iter().next()
    }

    // -- JSON datapack ingestion ----------------------------------------

    /// Map a recipe JSON `"type"` string to a category.
    /// `None` = unknown type → the load fails closed.
    fn category_for_type(&self, ty: &str) -> Option<RecipeCategory> {
        let known = [
            RecipeCategory::CraftingShaped,
            RecipeCategory::CraftingShapeless,
            RecipeCategory::Smelting,
            RecipeCategory::Blasting,
            RecipeCategory::Smoking,
            RecipeCategory::Campfire,
            RecipeCategory::Stonecutting,
            RecipeCategory::Smithing,
            RecipeCategory::Brewing,
            RecipeCategory::Anvil,
            RecipeCategory::CreateMixing,
            RecipeCategory::CreateCompacting,
            RecipeCategory::CreateCrushing,
            RecipeCategory::CreateMilling,
            RecipeCategory::CreatePressing,
            RecipeCategory::CreateDeploying,
            RecipeCategory::CreateSequencedAssembly,
            RecipeCategory::CreateMechanicalCrafting,
            RecipeCategory::CreateSplashing,
            RecipeCategory::CreateHaunting,
            RecipeCategory::CreateSandpaperPolishing,
            RecipeCategory::CreateItemApplication,
        ];
        for cat in known {
            let full = cat.type_id();
            if ty == full || ty == full.split(':').nth(1).unwrap_or("") {
                return Some(cat);
            }
        }
        for cat in &self.custom_categories {
            if let RecipeCategory::CustomModCategory { id, .. } = cat {
                if ty == id {
                    return Some(cat.clone());
                }
            }
        }
        let (ns, path) = ty.split_once(':')?;
        for (ans, en, ja) in &self.custom_namespaces {
            if ns == ans {
                return Some(RecipeCategory::CustomModCategory {
                    id: ty.to_owned(),
                    title_en: format!("{en}: {}", path.replace('_', " ")),
                    title_ja: ja.clone(),
                });
            }
        }
        None
    }

    /// Parse one datapack/mod recipe JSON and append it to the index.
    ///
    /// Fail-closed: malformed JSON, a non-object root, an unknown or
    /// unregistered `"type"`, and missing required fields all return
    /// `None` with the index untouched. On success the parsed [`Recipe`]
    /// is appended and a clone returned.
    pub fn load_recipe_json(&mut self, recipe_id: &str, json_str: &str) -> Option<Recipe> {
        let recipe = self.parse_recipe_json(recipe_id, json_str)?;
        self.recipes.push(recipe.clone());
        Some(recipe)
    }

    /// Like [`load_recipe_json`](Self::load_recipe_json) but only parses —
    /// used for the nested `sequence` steps of sequenced assembly so they
    /// never land in the index themselves.
    fn parse_recipe_json(&self, recipe_id: &str, json_str: &str) -> Option<Recipe> {
        let root = parse_json(json_str)?;
        let obj = root.as_object()?;
        let ty = obj.get("type").and_then(Json::as_str)?;
        let category = self.category_for_type(ty)?;
        let mut r = Recipe::new(recipe_id, category.clone());
        r.heat = obj
            .get("heatRequirement")
            .and_then(Json::as_str)
            .map(HeatRequirement::from_json_str)
            .unwrap_or_default();
        r.processing_ticks = obj
            .get("processingTime")
            .or_else(|| obj.get("processingDuration"))
            .or_else(|| obj.get("cookingtime"))
            .and_then(Json::as_u64)
            .unwrap_or(0)
            .min(u64::from(u32::MAX)) as u32;
        r.rpm_min = obj
            .get("rpm_min")
            .or_else(|| obj.get("rpmMin"))
            .or_else(|| obj.get("rpm"))
            .and_then(Json::as_u64)
            .map(|v| v.min(u64::from(u32::MAX)) as u32);
        r.su_cost = obj
            .get("su_cost")
            .or_else(|| obj.get("suCost"))
            .and_then(Json::as_u64);
        match category {
            RecipeCategory::CraftingShaped | RecipeCategory::CreateMechanicalCrafting => {
                self.parse_shaped(obj, &mut r)?;
            }
            RecipeCategory::CreateSequencedAssembly => {
                self.parse_sequenced(obj, &mut r)?;
            }
            _ => {
                if let Some(f) = obj.get("ingredient").or_else(|| obj.get("ingredients")) {
                    self.parse_ingredients_field(Some(f), &mut r)?;
                }
                // Smithing: template/base/addition are all inputs.
                for key in ["template", "base", "addition"] {
                    if let Some(v) = obj.get(key) {
                        self.parse_ingredients_field(Some(v), &mut r)?;
                    }
                }
                if r.inputs.is_empty() && r.fluid_inputs.is_empty() {
                    return None; // a recipe with no inputs is malformed
                }
                self.parse_results_field(obj.get("result").or_else(|| obj.get("results")), &mut r)?;
            }
        }
        if r.outputs.is_empty()
            && r.chanced_outputs.is_empty()
            && r.fluid_outputs.is_empty()
            && !matches!(category, RecipeCategory::CreateSequencedAssembly)
        {
            return None; // a recipe with no products is malformed
        }
        Some(r)
    }

    /// `pattern` + `key` shaped parse (vanilla crafting / Create
    /// mechanical crafting). Builds the 3×3-ish grid of `inputs` indices.
    fn parse_shaped(&self, obj: &BTreeMap<String, Json>, r: &mut Recipe) -> Option<()> {
        let pattern = obj.get("pattern").and_then(Json::as_array)?;
        let key = obj.get("key").and_then(Json::as_object)?;
        let mut rows: Vec<String> = Vec::with_capacity(pattern.len());
        for row in pattern {
            rows.push(row.as_str()?.to_owned());
        }
        if rows.is_empty() || rows.len() > 9 {
            return None;
        }
        let width = rows.iter().map(|r| r.chars().count()).max().unwrap_or(0);
        if width == 0 || width > 9 {
            return None;
        }
        let mut grid: Vec<Option<usize>> = Vec::with_capacity(rows.len() * width);
        let mut char_slot: BTreeMap<char, usize> = BTreeMap::new();
        for row in &rows {
            let mut count = 0usize;
            for ch in row.chars() {
                count += 1;
                if ch == ' ' {
                    grid.push(None);
                    continue;
                }
                let slot = if let Some(&s) = char_slot.get(&ch) {
                    s
                } else {
                    let spec = key.get(&ch.to_string())?;
                    self.parse_ingredients_field(Some(spec), r)?;
                    let s = r.inputs.len() - 1;
                    char_slot.insert(ch, s);
                    s
                };
                grid.push(Some(slot));
            }
            // Pad short rows so every row spans `width` cells.
            for _ in count..width {
                grid.push(None);
            }
        }
        r.grid = Some(grid);
        self.parse_results_field(obj.get("result"), r)
    }

    /// Sequenced assembly: base `ingredient`, `transitionalItem`, the
    /// nested `sequence` recipes, `loops` and `results`.
    fn parse_sequenced(&self, obj: &BTreeMap<String, Json>, r: &mut Recipe) -> Option<()> {
        self.parse_ingredients_field(obj.get("ingredient"), r)?;
        r.transitional_item = obj
            .get("transitionalItem")
            .and_then(|v| v.get("item").or_else(|| v.get("id")))
            .and_then(Json::as_str)
            .unwrap_or("")
            .to_owned();
        let seq = obj.get("sequence").and_then(Json::as_array)?;
        for step_json in seq {
            let step_obj = step_json.as_object()?;
            let ty = step_obj.get("type").and_then(Json::as_str)?;
            let cat = self.category_for_type(ty).unwrap_or_else(|| {
                // Unknown addon op types stay data-driven rather than
                // killing the whole recipe load.
                RecipeCategory::CustomModCategory {
                    id: ty.to_owned(),
                    title_en: ty.to_owned(),
                    title_ja: ty.to_owned(),
                }
            });
            let mut step = SequenceStep {
                category: cat,
                inputs: Vec::new(),
                keep_held_item: step_obj
                    .get("keepHeldItem")
                    .and_then(Json::as_bool)
                    .unwrap_or(false),
            };
            self.parse_step_ingredients(step_obj, &mut step)?;
            r.sequence_steps.push(step);
        }
        r.loops = obj
            .get("loops")
            .and_then(Json::as_u64)
            .unwrap_or(1)
            .clamp(1, 64) as u32;
        self.parse_results_field(obj.get("results").or_else(|| obj.get("result")), r)
    }

    /// Step ingredient list: same grammar as a recipe's `ingredients`,
    /// but fluids inside a sequence stay ignored for material accounting
    /// (they surface through `fluid_inputs` on real recipe types).
    fn parse_step_ingredients(
        &self,
        obj: &BTreeMap<String, Json>,
        step: &mut SequenceStep,
    ) -> Option<()> {
        let field = obj.get("ingredients").or_else(|| obj.get("ingredient"))?;
        let values: Vec<&Json> = match field {
            Json::Arr(a) => a.iter().collect(),
            other => vec![other],
        };
        for v in values {
            if let Some(ing) = parse_ingredient_value(v) {
                step.inputs.push(ing);
            }
        }
        Some(())
    }

    /// `ingredient`/`ingredients`: accepts the single-or-array grammar
    /// and routes fluid objects into `fluid_inputs`.
    fn parse_ingredients_field(
        &self,
        field: Option<&Json>,
        r: &mut Recipe,
    ) -> Option<()> {
        let field = field?;
        let values: Vec<&Json> = match field {
            Json::Arr(a) => a.iter().collect(),
            other => vec![other],
        };
        for v in values {
            match parse_ingredient_value(v) {
                Some(ing) => r.inputs.push(ing),
                None => {
                    let fl = parse_fluid_value(v)?;
                    r.fluid_inputs.push(fl);
                }
            }
        }
        Some(())
    }

    /// `result`/`results`: item objects land in `outputs` (or
    /// `chanced_outputs` when a `chance` key is present), fluid objects in
    /// `fluid_outputs`.
    fn parse_results_field(&self, field: Option<&Json>, r: &mut Recipe) -> Option<()> {
        let field = field?;
        let values: Vec<&Json> = match field {
            Json::Arr(a) => a.iter().collect(),
            other => vec![other],
        };
        for v in values {
            match v {
                Json::Str(s) => {
                    r.outputs.push(ItemStack {
                        item: s.clone(),
                        count: 1,
                    });
                }
                Json::Obj(m) => {
                    if m.contains_key("fluid") || m.contains_key("fluidTag") {
                        if let Some(fl) = parse_fluid_value(v) {
                            r.fluid_outputs.push(fl);
                            continue;
                        }
                        return None;
                    }
                    let item = m
                        .get("item")
                        .or_else(|| m.get("id"))
                        .and_then(Json::as_str)?;
                    let count = m
                        .get("count")
                        .and_then(Json::as_u64)
                        .unwrap_or(1)
                        .clamp(1, u64::from(u32::MAX)) as u32;
                    if let Some(chance) = m.get("chance").and_then(Json::as_f64) {
                        if !(0.0..=1.0).contains(&chance) {
                            return None;
                        }
                        r.chanced_outputs
                            .push((item.to_owned(), count, chance as f32));
                    } else {
                        r.outputs.push(ItemStack {
                            item: item.to_owned(),
                            count,
                        });
                    }
                }
                _ => return None,
            }
        }
        Some(())
    }
}

/// One parsed input value: item/tag slot or `None` when the value is a
/// fluid object (handled by `parse_fluid_value`).
fn parse_ingredient_value(v: &Json) -> Option<RecipeIngredient> {
    match v {
        Json::Str(s) => Some(RecipeIngredient::item(s, 1)),
        Json::Obj(m) => {
            if m.contains_key("fluid") || m.contains_key("fluidTag") {
                return None;
            }
            let count = m
                .get("count")
                .and_then(Json::as_u64)
                .unwrap_or(1)
                .clamp(1, u64::from(u32::MAX)) as u32;
            if let Some(id) = m.get("item").or_else(|| m.get("id")).and_then(Json::as_str) {
                return Some(RecipeIngredient::item(id, count));
            }
            if let Some(tag) = m.get("tag").and_then(Json::as_str) {
                return Some(RecipeIngredient::tag(tag, count));
            }
            None
        }
        // Vanilla allows an array of alternatives; the deterministic
        // pick is the first one (matching recipe browser's default slot cycling).
        Json::Arr(a) => a.first().and_then(parse_ingredient_value),
        _ => None,
    }
}

/// `{"fluid": "id", "amount": mb}` / `{"fluidTag": ...}` → [`FluidStack`].
fn parse_fluid_value(v: &Json) -> Option<FluidStack> {
    let m = v.as_object()?;
    let fluid = m
        .get("fluid")
        .or_else(|| m.get("fluidTag"))
        .and_then(Json::as_str)?;
    let amount_mb = m
        .get("amount")
        .and_then(Json::as_u64)
        .unwrap_or(1000)
        .clamp(1, u64::from(u32::MAX)) as u32;
    Some(FluidStack {
        fluid: fluid.to_owned(),
        amount_mb,
    })
}

// ---------------------------------------------------------------------------
// Recursive craft tree + raw material calculator
// ---------------------------------------------------------------------------

/// One node of a [`RecipeTree`]: `needed` units of `item`, produced by
/// `crafts` runs of `recipe_id` (or `None` when it is a raw material).
#[derive(Clone, Debug, PartialEq)]
pub struct TreeNode {
    pub item: String,
    /// Units required by the parent node.
    pub needed: u64,
    /// Recipe chosen for this node, if any.
    pub recipe_id: Option<String>,
    /// How many times `recipe_id` runs (ceil division of `needed` by the
    /// recipe's per-craft output of `item`).
    pub crafts: u64,
    /// Produced per craft (what `crafts` divides `needed` by).
    pub per_craft: u64,
    /// Ingredient expansion (intermediate crafts and raw leaves).
    pub children: Vec<TreeNode>,
    /// `true` when expansion stopped because this item is already being
    /// expanded up-stack — the loop closes on a raw requirement instead
    /// of recursing forever (ingot ↔ block compression, gear loops…).
    pub is_cycle: bool,
    /// `true` when expansion stopped because the node budget ran out.
    pub is_truncated: bool,
}

impl TreeNode {
    fn leaf(item: String, needed: u64) -> Self {
        Self {
            item,
            needed,
            recipe_id: None,
            crafts: 0,
            per_craft: 0,
            children: Vec::new(),
            is_cycle: false,
            is_truncated: false,
        }
    }

    /// Pre-order `(depth, node)` pairs for list rendering.
    pub fn flatten(&self) -> Vec<(usize, &TreeNode)> {
        let mut out = Vec::new();
        let mut stack: Vec<(usize, &TreeNode)> = vec![(0, self)];
        while let Some((d, n)) = stack.pop() {
            out.push((d, n));
            for c in n.children.iter().rev() {
                stack.push((d + 1, c));
            }
        }
        out
    }
}

/// The fully expanded craft plan for `(root_item, root_count)` plus the
/// aggregated totals the resource panel shows.
#[derive(Clone, Debug, Default)]
pub struct RecipeTree {
    pub root_item: String,
    pub root_count: u64,
    pub root: Option<TreeNode>,
    /// Leaf items with no recipe (or cycle-broken loops), `id → count`.
    pub raw_materials: BTreeMap<String, u64>,
    /// Total fluid inputs across all crafts, `fluid id → mB`.
    pub raw_fluids: BTreeMap<String, u64>,
    /// Extra guaranteed products of chosen recipes (item id → count)
    /// plus expected chanced rolls.
    pub byproducts: BTreeMap<String, u64>,
    /// Set when the node budget stopped some expansion.
    pub truncated: bool,
}

impl RecipeTree {
    /// `raw_materials` minus what `inventory` already holds (saturating,
    /// zero entries dropped): the shopping list the panel shows.
    pub fn remaining_needed(&self, inventory: &BTreeMap<String, u64>) -> BTreeMap<String, u64> {
        self.raw_materials
            .iter()
            .filter_map(|(id, need)| {
                let have = inventory.get(id).copied().unwrap_or(0);
                (need > &have).then(|| (id.clone(), need - have))
            })
            .collect()
    }
}

/// Hard bounds so a pathological recipe graph (a tree doubling per
/// level, or 10⁶-input "recipes") can never hang the UI thread.
const MAX_TREE_DEPTH: usize = 48;
const MAX_TREE_NODES: usize = 4096;

/// Mutable expansion context threaded through [`RecipeIndex::expand_node`]
/// (keeps the recursion signature small).
struct TreeBuild<'a> {
    selections: &'a BTreeMap<String, String>,
    /// Items currently being expanded — the cycle detector's stack.
    active: Vec<String>,
    /// Total nodes created so far (budget check).
    nodes: usize,
    /// Accumulated output: the tree root and running totals.
    tree: RecipeTree,
}

impl RecipeIndex {
    /// Expand `(item, count)` into a [`RecipeTree`].
    ///
    /// `selections` overrides which recipe produces an item
    /// (`item_id → recipe_id`, e.g. a user-picked alternative); items not
    /// present fall back to the first producing recipe in index order.
    /// Cycle safety: an item already on the expansion stack becomes a
    /// `is_cycle` leaf counted as raw material, so compression loops
    /// (`block ↔ 9×ingot`) terminate. Sequenced-assembly recipes expand
    /// their per-loop step inputs `loops` times.
    pub fn recipe_tree(
        &self,
        item: &str,
        count: u64,
        selections: &BTreeMap<String, String>,
    ) -> RecipeTree {
        let mut ctx = TreeBuild {
            selections,
            active: Vec::new(),
            nodes: 0,
            tree: RecipeTree {
                root_item: item.to_owned(),
                root_count: count,
                ..RecipeTree::default()
            },
        };
        let root = self.expand_node(item, count.max(1), 0, &mut ctx);
        ctx.tree.root = Some(root);
        ctx.tree
    }

    fn expand_node(&self, item: &str, needed: u64, depth: usize, ctx: &mut TreeBuild) -> TreeNode {
        ctx.nodes += 1;
        let mut node = TreeNode::leaf(item.to_owned(), needed);
        fn raw(tree: &mut RecipeTree, id: &str, n: u64) {
            *tree.raw_materials.entry(id.to_owned()).or_insert(0) += n;
        }
        if ctx.nodes >= MAX_TREE_NODES || depth > MAX_TREE_DEPTH {
            node.is_truncated = true;
            ctx.tree.truncated = true;
            raw(&mut ctx.tree, item, needed);
            return node;
        }
        if ctx.active.iter().any(|a| a == item) {
            node.is_cycle = true;
            raw(&mut ctx.tree, item, needed);
            return node;
        }
        let Some(recipe) = self.chosen_recipe_for(item, ctx.selections) else {
            raw(&mut ctx.tree, item, needed);
            return node;
        };
        let per_craft = recipe.output_count_for(item).max(1);
        let crafts = needed.div_ceil(per_craft);
        node.recipe_id = Some(recipe.id.clone());
        node.crafts = crafts;
        node.per_craft = per_craft;

        // Extra guaranteed outputs of the chosen recipe are byproducts.
        for out in &recipe.outputs {
            if out.item != item {
                *ctx.tree
                    .byproducts
                    .entry(out.item.clone())
                    .or_insert(0) += u64::from(out.count) * crafts;
            }
        }
        // Expected chanced rolls (floor of expected value, deterministic)
        // — but the target item itself is never a "byproduct".
        for (id, count, chance) in &recipe.chanced_outputs {
            if id == item {
                continue;
            }
            let expected = (f64::from(*count) * f64::from(*chance) * crafts as f64).floor() as u64;
            if expected > 0 {
                *ctx.tree.byproducts.entry(id.clone()).or_insert(0) += expected;
            }
        }
        // Fluid inputs aggregate into the raw-fluid total.
        for fl in &recipe.fluid_inputs {
            *ctx.tree.raw_fluids.entry(fl.fluid.clone()).or_insert(0) +=
                u64::from(fl.amount_mb) * crafts;
        }

        ctx.active.push(item.to_owned());
        for (ing_id, per) in recipe.effective_inputs(self) {
            let child = self.expand_node(&ing_id, per.saturating_mul(crafts), depth + 1, ctx);
            node.children.push(child);
        }
        ctx.active.pop();
        node
    }

    /// How many times `recipe` can run on the given `inventory`
    /// (item id → count). When the inventory is empty — the host simply
    /// did not supply one — this reports a full stack (`64`) so the `+`
    /// transfer button stays meaningful.
    pub fn max_craft(&self, recipe: &Recipe, inventory: &BTreeMap<String, u64>) -> u32 {
        let inputs = recipe.effective_inputs(self);
        if inputs.is_empty() {
            return u32::MAX;
        }
        if inventory.is_empty() {
            return 64;
        }
        let mut best = u64::MAX;
        for (id, per) in &inputs {
            let avail = inventory.get(id).copied().unwrap_or(0);
            if *per == 0 {
                continue;
            }
            best = best.min(avail / per);
        }
        best.min(u64::from(u32::MAX)) as u32
    }
}

// ---------------------------------------------------------------------------
// Machine-tutorial 3D scene + deterministic isometric renderer
// ---------------------------------------------------------------------------

/// One solid voxel in a [`MachineTutorialScene`] (`x`,`z` horizontal, `y` up).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MachineTutorialVoxel {
    pub x: i32,
    pub y: i32,
    pub z: i32,
    pub color: UiColor,
}

impl MachineTutorialVoxel {
    pub fn new(x: i32, y: i32, z: i32, color: UiColor) -> Self {
        Self { x, y, z, color }
    }
}

/// Axis for animated-part transforms.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MachineTutorialAxis {
    X,
    Y,
    Z,
}

/// Motion applied to the voxel at an [`AnimatedPart`] position.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum MachineTutorialAnimation {
    /// Continuous rotation about `axis` (cogwheel, millstone, press cam).
    Spin { axis: MachineTutorialAxis, rpm: f32 },
    /// Back-and-forth translation along `axis` (deployer arm, belt item).
    Oscillate {
        axis: MachineTutorialAxis,
        amplitude: f32,
        period_ticks: u32,
    },
}

/// The voxel position an animation applies to. When no [`MachineTutorialVoxel`]
/// sits at `pos`, a small marker cube is drawn instead (a moving arm or
/// item gliding along a belt).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AnimatedPart {
    pub pos: (i32, i32, i32),
    pub anim: MachineTutorialAnimation,
}

/// One narrated step of a [`MachineTutorialScene`].
#[derive(Clone, Debug, PartialEq)]
pub struct MachineTutorialStep {
    pub caption_en: String,
    pub caption_ja: String,
    /// Voxel position outlined in the highlight color this step.
    pub highlight: Option<(i32, i32, i32)>,
    /// Animations active during this step.
    pub animated_parts: Vec<AnimatedPart>,
    /// Step length in ticks (machine tutorials run at ~20 tps).
    pub duration_ticks: u32,
}

impl MachineTutorialStep {
    pub fn new(caption_en: &str, caption_ja: &str, duration_ticks: u32) -> Self {
        Self {
            caption_en: caption_en.to_owned(),
            caption_ja: caption_ja.to_owned(),
            highlight: None,
            animated_parts: Vec::new(),
            duration_ticks: duration_ticks.max(1),
        }
    }

    pub fn caption(&self, lang: UiLang) -> &str {
        tr(lang, &self.caption_en, &self.caption_ja)
    }
}

/// A pure-data 3D voxel machine scene: the host builds `blocks` and
/// `steps`, the browser renders it through a deterministic isometric
/// projection and steps it with [`advance_tick`](Self::advance_tick).
#[derive(Clone, Debug)]
pub struct MachineTutorialScene {
    pub title: String,
    pub blocks: Vec<MachineTutorialVoxel>,
    pub steps: Vec<MachineTutorialStep>,
    /// Camera yaw around the vertical axis, degrees.
    pub yaw_deg: f32,
    /// Camera pitch (down-tilt), degrees.
    pub pitch_deg: f32,
    /// Active step in `steps`.
    pub step_index: usize,
    /// Whether `advance_tick` progresses time.
    pub playing: bool,
    /// Global animation clock.
    pub tick: u32,
    /// Ticks elapsed in the current step.
    pub step_ticks: u32,
}

impl Default for MachineTutorialScene {
    fn default() -> Self {
        Self {
            title: String::new(),
            blocks: Vec::new(),
            steps: Vec::new(),
            yaw_deg: 45.0,
            pitch_deg: 30.0,
            step_index: 0,
            playing: true,
            tick: 0,
            step_ticks: 0,
        }
    }
}

impl MachineTutorialScene {
    pub fn new(title: &str) -> Self {
        Self {
            title: title.to_owned(),
            ..Self::default()
        }
    }

    pub fn add_block(&mut self, x: i32, y: i32, z: i32, color: UiColor) {
        self.blocks.push(MachineTutorialVoxel::new(x, y, z, color));
    }

    /// The currently narrated step, if any.
    pub fn current_step(&self) -> Option<&MachineTutorialStep> {
        self.steps.get(self.step_index)
    }

    /// Advance the animation/step clock by one tick (no-op while paused).
    /// Steps wrap around like a looping Create machine tutorial.
    pub fn advance_tick(&mut self) {
        if !self.playing {
            return;
        }
        self.tick = self.tick.wrapping_add(1);
        self.step_ticks += 1;
        let dur = self
            .steps
            .get(self.step_index)
            .map(|s| s.duration_ticks.max(1))
            .unwrap_or(1);
        if self.step_ticks >= dur && !self.steps.is_empty() {
            self.step_ticks = 0;
            self.step_index = (self.step_index + 1) % self.steps.len();
        }
    }

    pub fn next_step(&mut self) {
        if !self.steps.is_empty() {
            self.step_index = (self.step_index + 1) % self.steps.len();
            self.step_ticks = 0;
        }
    }

    pub fn prev_step(&mut self) {
        if !self.steps.is_empty() {
            self.step_index = if self.step_index == 0 {
                self.steps.len() - 1
            } else {
                self.step_index - 1
            };
            self.step_ticks = 0;
        }
    }

    /// Rotate the camera by `delta` degrees around the vertical axis.
    pub fn rotate(&mut self, delta: f32) {
        self.yaw_deg = Mth::wrap_degrees_f32(self.yaw_deg + delta);
    }

    /// The active animation for `pos` during the current step.
    fn anim_at(&self, pos: (i32, i32, i32)) -> Option<MachineTutorialAnimation> {
        self.current_step().and_then(|s| {
            s.animated_parts
                .iter()
                .find(|p| p.pos == pos)
                .map(|p| p.anim)
        })
    }
}

/// Render `scene` into `list`, clipped to physical-pixel viewport `vp`.
///
/// Pipeline: per-voxel corners are animated (spin/oscillate from the
/// current step), the scene is yaw+pitch rotated about its center and
/// orthographically projected; each cube's three camera-facing faces are
/// depth-sorted (painter's algorithm, far → near) and rasterized as 1px
/// scanline `FillRect`s, so the whole thing rides the existing
/// software/wgpu quad pipeline with no extra primitives.
pub fn draw_machine_tutorial_scene(list: &mut UiDrawList, vp: UiRect, scene: &MachineTutorialScene) {
    if vp.w < 8 || vp.h < 8 || scene.blocks.is_empty() {
        return;
    }
    let (min_x, max_x) = bounds_axis(&scene.blocks, |v| v.x);
    let (min_y, max_y) = bounds_axis(&scene.blocks, |v| v.y);
    let (min_z, max_z) = bounds_axis(&scene.blocks, |v| v.z);
    let cx = (min_x + max_x + 1) as f32 / 2.0;
    let cy = (min_y + max_y + 1) as f32 / 2.0;
    let cz = (min_z + max_z + 1) as f32 / 2.0;
    let extent = (max_x - min_x + 1)
        .max(max_y - min_y + 1)
        .max(max_z - min_z + 1) as f32;
    // Block edge length in px — fit the rotated scene with headroom.
    let size = (vp.w.min(vp.h) as f32 / (extent * 1.55 + 0.8)).clamp(3.0, 64.0);
    let ox = vp.x as f32 + vp.w as f32 / 2.0;
    let oy = vp.y as f32 + vp.h as f32 * 0.58;

    let yaw = scene.yaw_deg.to_radians();
    let pitch = scene.pitch_deg.to_radians();
    let (cyw, syw) = (Mth::cos(yaw), Mth::sin(yaw));
    let (cpt, spt) = (Mth::cos(pitch), Mth::sin(pitch));

    // Rotate a world-space point about the scene center → screen + depth.
    let project = |px: f32, py: f32, pz: f32| -> (f32, f32, f32) {
        let dx = px - cx;
        let dy = py - cy;
        let dz = pz - cz;
        let x1 = dx * cyw - dz * syw;
        let z1 = dx * syw + dz * cyw;
        let y2 = dy * cpt - z1 * spt;
        let z2 = dy * spt + z1 * cpt;
        (ox + x1 * size, oy - y2 * size, z2)
    };
    // Same rotation for face normals (no translation); only the z depth
    // is needed for back-face culling.
    let rotate_n = |nx: f32, ny: f32, nz: f32| -> f32 {
        let z1 = nx * syw + nz * cyw;
        ny * spt + z1 * cpt
    };

    // Corner index bits: x + 2y + 4z.
    type FaceDef = ([usize; 4], (f32, f32, f32), f32);
    const FACES: [FaceDef; 6] = [
        ([2, 3, 7, 6], (0.0, 1.0, 0.0), 1.00),   // +y top
        ([0, 4, 5, 1], (0.0, -1.0, 0.0), 0.45),  // -y bottom
        ([1, 5, 7, 3], (1.0, 0.0, 0.0), 0.85),   // +x
        ([0, 2, 6, 4], (-1.0, 0.0, 0.0), 0.70),  // -x
        ([4, 6, 7, 5], (0.0, 0.0, 1.0), 0.80),   // +z
        ([0, 1, 3, 2], (0.0, 0.0, -1.0), 0.62),  // -z
    ];

    struct Proj {
        quad: [(f32, f32); 4],
        color: UiColor,
        depth: f32,
    }
    let mut faces: Vec<Proj> = Vec::with_capacity(scene.blocks.len() * 3);

    // Collect one cube's faces: `corner(i)` yields local offsets in
    // {0,1}³; `half` shrinks marker cubes (0.5 = full voxel).
    let mut collect = |bx: f32, by: f32, bz: f32, half: f32, anim: Option<MachineTutorialAnimation>, base: UiColor| {
        let lo = 0.5 - half;
        let hi = 0.5 + half;
        let mut pts = [(0.0f32, 0.0f32, 0.0f32); 8];
        for (i, p) in pts.iter_mut().enumerate() {
            let mut lx = if i & 1 == 0 { lo } else { hi };
            let mut ly = if i & 2 == 0 { lo } else { hi };
            let mut lz = if i & 4 == 0 { lo } else { hi };
            match anim {
                Some(MachineTutorialAnimation::Spin { axis, rpm }) => {
                    let ang = scene.tick as f32 * rpm * std::f32::consts::TAU / 1200.0;
                    let (s, c) = (Mth::sin(ang), Mth::cos(ang));
                    let (mx, my, mz) = (lx - 0.5, ly - 0.5, lz - 0.5);
                    match axis {
                        MachineTutorialAxis::X => {
                            ly = 0.5 + my * c - mz * s;
                            lz = 0.5 + my * s + mz * c;
                        }
                        MachineTutorialAxis::Y => {
                            lx = 0.5 + mx * c - mz * s;
                            lz = 0.5 + mx * s + mz * c;
                        }
                        MachineTutorialAxis::Z => {
                            lx = 0.5 + mx * c - my * s;
                            ly = 0.5 + mx * s + my * c;
                        }
                    }
                }
                Some(MachineTutorialAnimation::Oscillate {
                    axis,
                    amplitude,
                    period_ticks,
                }) => {
                    let phase = scene.tick as f32 * std::f32::consts::TAU
                        / period_ticks.max(1) as f32;
                    let off = amplitude * Mth::sin(phase);
                    match axis {
                        MachineTutorialAxis::X => lx += off,
                        MachineTutorialAxis::Y => ly += off,
                        MachineTutorialAxis::Z => lz += off,
                    }
                }
                None => {}
            }
            *p = (bx + lx, by + ly, bz + lz);
        }
        for (idxs, n, shade) in FACES {
            // Camera sits on +z looking -z: a face shows when its rotated
            // normal still points at the camera.
            if rotate_n(n.0, n.1, n.2) <= 0.0 {
                continue;
            }
            let mut quad = [(0.0f32, 0.0f32); 4];
            let mut depth = 0.0f32;
            for (k, &ci) in idxs.iter().enumerate() {
                let (sx, sy, z2) = project(pts[ci].0, pts[ci].1, pts[ci].2);
                quad[k] = (sx, sy);
                depth += z2;
            }
            faces.push(Proj {
                quad,
                color: shade_color(base, shade),
                depth: depth / 4.0,
            });
        }
    };

    for v in &scene.blocks {
        let anim = scene.anim_at((v.x, v.y, v.z));
        collect(v.x as f32, v.y as f32, v.z as f32, 0.5, anim, v.color);
    }
    // Animated positions with no block → accent marker cube.
    if let Some(step) = scene.current_step() {
        for part in &step.animated_parts {
            let (px, py, pz) = part.pos;
            if scene
                .blocks
                .iter()
                .any(|b| b.x == px && b.y == py && b.z == pz)
            {
                continue;
            }
            collect(
                px as f32,
                py as f32,
                pz as f32,
                0.28,
                Some(part.anim),
                palette::WARNING,
            );
        }
    }

    // Painter's algorithm: smallest depth (farthest) first.
    faces.sort_by(|a, b| a.depth.total_cmp(&b.depth));
    for f in &faces {
        rasterize_quad(list, vp, f.quad, f.color);
    }

    // Highlight wireframe over the step's highlighted voxel.
    if let Some((hx, hy, hz)) = scene.current_step().and_then(|s| s.highlight) {
        let mut pc = [(0.0f32, 0.0f32); 8];
        for (i, p) in pc.iter_mut().enumerate() {
            let lx = if i & 1 == 0 { 0.0 } else { 1.0 };
            let ly = if i & 2 == 0 { 0.0 } else { 1.0 };
            let lz = if i & 4 == 0 { 0.0 } else { 1.0 };
            let (sx, sy, _) = project(hx as f32 + lx, hy as f32 + ly, hz as f32 + lz);
            *p = (sx, sy);
        }
        const EDGES: [(usize, usize); 12] = [
            (0, 1),
            (0, 2),
            (0, 4),
            (1, 3),
            (1, 5),
            (2, 3),
            (2, 6),
            (3, 7),
            (4, 5),
            (4, 6),
            (5, 7),
            (6, 7),
        ];
        for (a, b) in EDGES {
            rasterize_line(list, vp, pc[a], pc[b], palette::ACCENT);
        }
    }
}

fn bounds_axis(blocks: &[MachineTutorialVoxel], f: impl Fn(&MachineTutorialVoxel) -> i32) -> (i32, i32) {
    let mut lo = i32::MAX;
    let mut hi = i32::MIN;
    for b in blocks {
        let v = f(b);
        lo = lo.min(v);
        hi = hi.max(v);
    }
    if lo > hi {
        (0, 0)
    } else {
        (lo, hi)
    }
}

/// Multiply a color's RGB by `shade` (face lighting).
fn shade_color(c: UiColor, shade: f32) -> UiColor {
    let f = |v: u8| (f32::from(v) * shade).clamp(0.0, 255.0) as u8;
    UiColor::rgba(f(c.r), f(c.g), f(c.b), c.a)
}

/// Scanline-fill a convex quad (projected cube face) inside `vp`.
fn rasterize_quad(list: &mut UiDrawList, vp: UiRect, q: [(f32, f32); 4], color: UiColor) {
    let mut ymin = f32::MAX;
    let mut ymax = f32::MIN;
    for p in &q {
        ymin = ymin.min(p.1);
        ymax = ymax.max(p.1);
    }
    let y0 = (ymin.floor() as i32).max(vp.y);
    let y1 = (ymax.ceil() as i32).min(vp.bottom());
    for row in y0..y1 {
        let rc = row as f32 + 0.5;
        let mut xl = f32::MAX;
        let mut xr = f32::MIN;
        for i in 0..4 {
            let a = q[i];
            let b = q[(i + 1) & 3];
            if (a.1 <= rc) != (b.1 <= rc) {
                let t = (rc - a.1) / (b.1 - a.1);
                let x = a.0 + t * (b.0 - a.0);
                xl = xl.min(x);
                xr = xr.max(x);
            }
        }
        if xl > xr {
            continue;
        }
        let x0 = (xl.floor() as i32).max(vp.x);
        let x1 = (xr.ceil() as i32).min(vp.right());
        if x0 < x1 {
            list.fill_rect(
                UiRect::new(x0, row, (x1 - x0) as u32, 1),
                color,
            );
        }
    }
}

/// DDA a line between two projected points (highlight wireframe).
fn rasterize_line(list: &mut UiDrawList, vp: UiRect, a: (f32, f32), b: (f32, f32), color: UiColor) {
    let dx = b.0 - a.0;
    let dy = b.1 - a.1;
    let steps = dx.abs().max(dy.abs()).ceil().min(512.0) as i32;
    for i in 0..=steps {
        let t = if steps == 0 { 0.0 } else { i as f32 / steps as f32 };
        let x = (a.0 + dx * t).round() as i32;
        let y = (a.1 + dy * t).round() as i32;
        if x >= vp.x && x + 2 <= vp.right() && y >= vp.y && y + 2 <= vp.bottom() {
            list.fill_rect(UiRect::new(x - 1, y - 1, 2, 2), color);
        }
    }
}

// ---------------------------------------------------------------------------
// Browser state: tabs, views, widget ids, actions
// ---------------------------------------------------------------------------

/// The top-level browser tab (item grid, craft tree, world resources,
/// machine tutorials).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum BrowserTab {
    #[default]
    Items,
    Tree,
    WorldDropsInfo,
    MachineTutorials,
}

impl BrowserTab {
    pub const ALL: [Self; 4] = [Self::Items, Self::Tree, Self::WorldDropsInfo, Self::MachineTutorials];

    fn label(self, lang: UiLang) -> &'static str {
        match self {
            Self::Items => tr(lang, "Items", "アイテム"),
            Self::Tree => tr(lang, "Tree", "ツリー"),
            Self::WorldDropsInfo => tr(lang, "Drops & World Info", "ドロップとワールド情報"),
            Self::MachineTutorials => tr(lang, "Machine Tutorials", "機械チュートリアル"),
        }
    }
}

/// World resource sub-tab.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum WorldDropsTab {
    #[default]
    MobDrops,
    OreGen,
    Trades,
    Loot,
    Enchants,
    Fuels,
}

impl WorldDropsTab {
    pub const ALL: [Self; 6] = [
        Self::MobDrops,
        Self::OreGen,
        Self::Trades,
        Self::Loot,
        Self::Enchants,
        Self::Fuels,
    ];

    fn label(self, lang: UiLang) -> &'static str {
        match self {
            Self::MobDrops => tr(lang, "Mob Drops", "モブドロップ"),
            Self::OreGen => tr(lang, "Ore Gen", "鉱石生成"),
            Self::Trades => tr(lang, "Trades", "取引"),
            Self::Loot => tr(lang, "Loot", "戦利品"),
            Self::Enchants => tr(lang, "Enchants", "エンチャント"),
            Self::Fuels => tr(lang, "Fuels", "燃料"),
        }
    }
}

/// Recipes-vs-uses detail mode inside the Items tab.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DetailMode {
    /// "Recipes producing X" (`R`).
    Recipes,
    /// "Uses consuming X" (`U`).
    Uses,
}

impl DetailMode {
    fn label(self, lang: UiLang) -> &'static str {
        match self {
            Self::Recipes => tr(lang, "Recipes for", "レシピ:"),
            Self::Uses => tr(lang, "Uses of", "用途:"),
        }
    }
}

/// Every interactive element on the screen.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecipeWidgetId {
    SearchField,
    SearchClear,
    TabItems,
    TabTree,
    TabWorldDropsInfo,
    TabMachineTutorials,
    /// Breadcrumb back from a recipes/uses detail to the item grid.
    DetailBack,
    /// A grid slot; payload is the index into the *visible* item list.
    ItemSlot(usize),
    GridScrollUp,
    GridScrollDown,
    /// A bookmarks-panel row; payload is the index into `bookmarks`.
    BookmarkRow(usize),
    /// Reorder/remove actions on `bookmark_selected`.
    BookmarkUp,
    BookmarkDown,
    BookmarkRemove,
    /// A recipe row in the detail list; payload is the row index.
    RecipeRow(usize),
    /// The row's `+` transfer button (recipe browser "fill crafting grid").
    TransferRecipe(usize),
    /// An item slot inside a recipe diagram; `n` indexes the recipe's
    /// `inputs ++ outputs` order.
    DetailSlot(usize),
    /// Craftable-only filter toggle (grid + overlay).
    CraftableOnlyToggle,
    DetailScrollUp,
    DetailScrollDown,
    /// A node row in the recipe tree; payload is the pre-order index.
    TreeNodeRow(usize),
    TreeScrollUp,
    TreeScrollDown,
    /// Target-count ×2 / ÷2 on the recipe tree.
    TreeCountUp,
    TreeCountDown,
    /// Pin the current tree into the bookmarks panel.
    TreePin,
    /// A pinned-tree row in the bookmarks panel.
    PinnedTreeRow(usize),
    WorldDropsTabMobs,
    WorldDropsTabOres,
    WorldDropsTabTrades,
    WorldDropsTabLoot,
    WorldDropsTabEnchants,
    WorldDropsTabFuels,
    WorldDropsScrollUp,
    WorldDropsScrollDown,
    MachineTutorialPrev,
    MachineTutorialPlay,
    MachineTutorialNext,
    MachineTutorialRotateLeft,
    MachineTutorialRotateRight,
}

/// A clickable rectangle emitted alongside the draw list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RecipeHitRegion {
    pub rect: UiRect,
    pub id: RecipeWidgetId,
}

impl RecipeHitRegion {
    /// Point containment test (right/bottom exclusive).
    pub const fn contains(self, x: i32, y: i32) -> bool {
        self.rect.contains(x, y)
    }
}

/// What the embedding application must perform after an interaction.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RecipeBrowserAction {
    /// recipe-browser-style `+` button: fill the player's crafting grid with this
    /// recipe. `max_craft` is how many times the recipe can run on the
    /// host-supplied `inventory` (or `64` when none was supplied).
    TransferToCraftingGrid { recipe_id: String, max_craft: u32 },
    /// Leave the screen (Escape / gamepad East with nothing to back out).
    Close,
}

// ---------------------------------------------------------------------------
// Search query tokens (`text` `@mod` `#tag` `$category`)
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
enum QueryToken {
    Text(String),
    Mod(String),
    Tag(String),
    Category(String),
    Tooltip(String),
    /// `-token` — the wrapped token must NOT match.
    Neg(Box<QueryToken>),
}

/// Parse one whitespace token into a [`QueryToken`] (`@mod`, `#tag`,
/// `$category`, `%tooltip`, plain text), without any `-` handling.
fn query_token(body: &str) -> QueryToken {
    if let Some(m) = body.strip_prefix('@') {
        QueryToken::Mod(m.to_owned())
    } else if let Some(t) = body.strip_prefix('#') {
        QueryToken::Tag(t.to_owned())
    } else if let Some(c) = body.strip_prefix('$') {
        QueryToken::Category(c.to_owned())
    } else if let Some(t) = body.strip_prefix('%') {
        QueryToken::Tooltip(t.to_owned())
    } else {
        QueryToken::Text(body.to_owned())
    }
}

/// recipe-overlay query syntax: the search line splits into `|`-separated OR
/// clauses; each clause is a whitespace-separated AND of tokens where
/// `-token` negates. `a | b c` = `(a) OR (b AND c)`.
fn parse_query(search: &str) -> Vec<Vec<QueryToken>> {
    let mut clauses: Vec<Vec<QueryToken>> = vec![Vec::new()];
    for raw in search.split_whitespace() {
        if raw == "|" {
            if !clauses.last().is_some_and(|c| c.is_empty()) {
                clauses.push(Vec::new());
            }
            continue;
        }
        let lower = raw.to_lowercase();
        let (negated, body) = match lower.strip_prefix('-') {
            Some(rest) if !rest.is_empty() => (true, rest),
            _ => (false, lower.as_str()),
        };
        let token = query_token(body);
        clauses.last_mut().unwrap().push(if negated {
            QueryToken::Neg(Box::new(token))
        } else {
            token
        });
    }
    clauses
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

/// Match rank of a lowercase token against a lowercase field:
/// exact > prefix > substring > subsequence.
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

/// Interactive state of the recipe & resource browser.
#[derive(Clone, Debug)]
pub struct RecipeBrowserState {
    /// Host-supplied item, recipe, and world-resource data.
    pub index: RecipeIndex,
    /// UI language for chrome labels.
    pub lang: UiLang,
    /// `0` = auto (`height/270`, clamped `1..=4`), else fixed GUI scale.
    pub ui_scale: u8,
    /// Live search text.
    pub search: String,
    /// Active top-level tab.
    pub tab: BrowserTab,
    /// `(item_id, mode)` when viewing recipes/uses inside Items.
    pub detail: Option<(String, DetailMode)>,
    /// Bookmarked item ids, in panel order.
    pub bookmarks: Vec<String>,
    /// Which bookmark row is selected for reorder/remove.
    pub bookmark_selected: Option<usize>,
    /// `(item_id, count)` recipe trees pinned into the bookmarks panel.
    pub pinned_trees: Vec<(String, u64)>,
    /// Host-supplied player inventory (drives `max_craft` + "remaining").
    pub inventory: BTreeMap<String, u64>,
    /// Per-item recipe override for tree building (`item_id → recipe_id`).
    pub recipe_selections: BTreeMap<String, String>,
    /// Active world resource sub-tab.
    pub world_drops_tab: WorldDropsTab,
    /// The loaded machine-tutorial scene, if the host supplied one.
    pub machine_tutorial: Option<MachineTutorialScene>,
    /// Widget under the mouse cursor (hover highlight).
    pub hover: Option<RecipeWidgetId>,
    search_focused: bool,
    /// Selection as an index into the visible item list.
    selected: usize,
    /// First grid row painted.
    grid_scroll: usize,
    /// Selection inside the detail recipe list.
    recipe_selected: usize,
    detail_scroll: usize,
    /// The expanded recipe tree (`Tree` tab target), when built.
    pub tree: Option<RecipeTree>,
    tree_scroll: usize,
    world_drops_scroll: usize,
    /// Minecraft locale the UI currently displays (`"en_us"`, `"ja_jp"`,
    /// `"fr_fr"`, ...) — resolves localized item names and is matched by
    /// search; the host syncs it from `settings.locale`.
    pub active_locale: String,
    /// Craftable-only grid filter (player inventory driven).
    pub craftable_only: bool,
    /// Detail navigation stack — `Backspace`/`< Back` pops it.
    history: Vec<(String, DetailMode)>,
    /// Host-fed tick counter driving tag-slot icon cycling (~1 Hz).
    pub clock_ticks: u64,
    /// Last overlay-space mouse position (physical px) — feeds the
    /// hover tooltip anchor.
    pub overlay_mouse: Option<(i32, i32)>,
}

impl Default for RecipeBrowserState {
    fn default() -> Self {
        Self {
            index: RecipeIndex::new(),
            lang: UiLang::En,
            ui_scale: 0,
            search: String::new(),
            tab: BrowserTab::Items,
            detail: None,
            bookmarks: Vec::new(),
            bookmark_selected: None,
            pinned_trees: Vec::new(),
            inventory: BTreeMap::new(),
            recipe_selections: BTreeMap::new(),
            world_drops_tab: WorldDropsTab::MobDrops,
            machine_tutorial: None,
            hover: None,
            search_focused: true,
            selected: 0,
            grid_scroll: 0,
            recipe_selected: 0,
            detail_scroll: 0,
            tree: None,
            tree_scroll: 0,
            world_drops_scroll: 0,
            active_locale: "en_us".to_owned(),
            craftable_only: false,
            history: Vec::new(),
            clock_ticks: 0,
            overlay_mouse: None,
        }
    }
}

impl RecipeBrowserState {
    pub fn new(index: RecipeIndex) -> Self {
        Self {
            index,
            ..Self::default()
        }
    }

    /// Effective GUI scale for `height` px tall output (`1..=4`).
    pub fn effective_scale(&self, height: u32) -> u32 {
        if self.ui_scale == 0 {
            (height / 270).clamp(1, 4)
        } else {
            u32::from(self.ui_scale.clamp(1, 4))
        }
    }

    /// Whether the search field owns typed characters.
    pub fn search_focused(&self) -> bool {
        self.search_focused
    }

    /// Selected grid index into [`visible_items`](Self::visible_items).
    pub fn selected_index(&self) -> usize {
        self.selected
    }

    /// Selected item id, if the visible list is non-empty.
    pub fn selected_item(&self) -> Option<String> {
        self.visible_items()
            .get(self.selected)
            .map(|&i| self.index.items[i].id.clone())
    }

    /// Recipes shown in the current detail view.
    fn detail_recipes(&self) -> Vec<&Recipe> {
        let Some((item, mode)) = &self.detail else {
            return Vec::new();
        };
        match mode {
            DetailMode::Recipes => self.index.recipes_producing(item),
            DetailMode::Uses => self.index.recipes_consuming(item),
        }
    }

    /// `true` when `item` satisfies one query token (positive match).
    /// Text tokens score across English + active-locale names, id, mod,
    /// tags and tooltips; `@`/`#`/`$`/`%` pin a specific field.
    fn token_matches(&self, item: &ItemEntry, token: &QueryToken) -> bool {
        match token {
            QueryToken::Text(t) if t.is_empty() => true,
            QueryToken::Text(t) => self.item_text_score(item, t).is_some(),
            QueryToken::Mod(m) => {
                m.is_empty() || item.mod_id.to_lowercase().contains(m.as_str())
            }
            QueryToken::Tag(t) => {
                t.is_empty()
                    || item
                        .tags
                        .iter()
                        .any(|tag| tag.to_lowercase().contains(t.as_str()))
            }
            QueryToken::Category(c) => {
                c.is_empty()
                    || self
                        .index
                        .recipes
                        .iter()
                        .filter(|r| {
                            r.produces(&item.id) || {
                                r.inputs
                                    .iter()
                                    .chain(r.sequence_steps.iter().flat_map(|s| s.inputs.iter()))
                                    .any(|i| self.index.ingredient_accepts(i, &item.id))
                            }
                        })
                        .any(|r| r.category.matches_query(c))
            }
            QueryToken::Tooltip(t) => {
                t.is_empty()
                    || item
                        .tooltip_lines
                        .iter()
                        .any(|line| line.to_lowercase().contains(t.as_str()))
            }
            QueryToken::Neg(_) => true,
        }
    }

    /// A `-token` must not match; empty negations are free passes.
    fn token_blocked(&self, item: &ItemEntry, token: &QueryToken) -> bool {
        match token {
            QueryToken::Neg(inner) => self.token_matches(item, inner),
            _ => false,
        }
    }

    /// `true` while `craftable_only` allows `item` through: some recipe
    /// producing it is fully craftable from the live player inventory.
    fn craftable_check(&self, item: &ItemEntry) -> bool {
        !self.craftable_only
            || self
                .index
                .recipes_producing(&item.id)
                .iter()
                .any(|r| self.index.max_craft(r, &self.inventory) > 0)
    }

    /// Item indices passing the recipe tree query — `|`-separated OR clauses of
    /// AND'd tokens (`-token` negates) — then optionally the
    /// `craftable_only` inventory filter, scored by text relevance then
    /// alphabetical display order.
    pub fn visible_items(&self) -> Vec<usize> {
        let clauses = parse_query(&self.search);
        // Positive Text tokens prefilter each clause through the
        // trigram shortlist: an item outside a token's candidate set
        // cannot satisfy `item_text_score` (the set is a proven
        // superset), so the exact per-item gate only runs on plausible
        // survivors. Negated and non-text tokens don't restrict the
        // set — `token_matches`/`token_blocked` still decide them.
        let n = self.index.items.len();
        let clause_cands: Vec<Option<Vec<bool>>> = clauses
            .iter()
            .map(|clause| {
                let mut cand: Option<Vec<bool>> = None;
                let mut ids: Vec<u32> = Vec::new();
                for token in clause {
                    let QueryToken::Text(t) = token else {
                        continue;
                    };
                    if t.is_empty() {
                        continue;
                    }
                    ids.clear();
                    self.index.text_candidates(t, &mut ids);
                    match &mut cand {
                        None => {
                            let mut bits = vec![false; n];
                            for &i in &ids {
                                bits[i as usize] = true;
                            }
                            cand = Some(bits);
                        }
                        Some(bits) => {
                            let mut keep = vec![false; n];
                            for &i in &ids {
                                keep[i as usize] = true;
                            }
                            for (b, k) in bits.iter_mut().zip(keep) {
                                *b &= k;
                            }
                        }
                    }
                }
                cand
            })
            .collect();
        let mut scored: Vec<(usize, u32)> = Vec::with_capacity(self.index.items.len());
        'items: for (index, item) in self.index.items.iter().enumerate() {
            if !self.craftable_check(item) {
                continue;
            }
            let mut best_clause_score = None;
            for (ci, clause) in clauses.iter().enumerate() {
                if clause_cands[ci].as_ref().is_some_and(|bits| !bits[index]) {
                    continue;
                }
                if clause
                    .iter()
                    .any(|t| self.token_blocked(item, t) || !self.token_matches(item, t))
                {
                    continue;
                }
                let mut score = 0u32;
                for token in clause {
                    if let QueryToken::Text(t) = token {
                        if let Some(s) = self.item_text_score(item, t) {
                            score += s;
                        }
                    }
                }
                best_clause_score = Some(best_clause_score.unwrap_or(0).max(score));
            }
            let Some(score) = best_clause_score else {
                continue 'items;
            };
            scored.push((index, score));
        }
        let name = |i: usize| self.index.items[i].display_name.to_lowercase();
        scored.sort_by(|a, b| {
            b.1.cmp(&a.1)
                .then_with(|| name(a.0).cmp(&name(b.0)))
                .then_with(|| self.index.items[a.0].id.cmp(&self.index.items[b.0].id))
        });
        scored.into_iter().map(|(i, _)| i).collect()
    }

    // -- bookmarks -------------------------------------------------------

    pub fn is_bookmarked(&self, item: &str) -> bool {
        self.bookmarks.iter().any(|b| b == item)
    }

    /// Add `item` to bookmarks (or remove it if already there).
    pub fn toggle_bookmark(&mut self, item: &str) {
        if let Some(pos) = self.bookmarks.iter().position(|b| b == item) {
            self.bookmarks.remove(pos);
            self.bookmark_selected = self.bookmark_selected.and_then(|s| {
                match s.cmp(&pos) {
                    std::cmp::Ordering::Less => Some(s),
                    std::cmp::Ordering::Equal => None,
                    std::cmp::Ordering::Greater => Some(s - 1),
                }
            });
        } else {
            self.bookmarks.push(item.to_owned());
        }
    }

    /// Move the selected bookmark up/down one slot (wraps at the ends).
    pub fn move_bookmark(&mut self, delta: i64) {
        let Some(sel) = self.bookmark_selected else {
            return;
        };
        if sel >= self.bookmarks.len() {
            self.bookmark_selected = None;
            return;
        }
        let len = self.bookmarks.len() as i64;
        let next = (sel as i64 + delta).rem_euclid(len) as usize;
        self.bookmarks.swap(sel, next);
        self.bookmark_selected = Some(next);
    }

    /// Remove the selected bookmark.
    pub fn remove_selected_bookmark(&mut self) {
        if let Some(sel) = self.bookmark_selected {
            if sel < self.bookmarks.len() {
                self.bookmarks.remove(sel);
                self.bookmark_selected = (sel > 0).then(|| sel - 1);
            }
        }
    }

    // -- detail / tree ---------------------------------------------------

    /// Open the recipes (`R`) or uses (`U`) detail for `item`. The
    /// current detail — if any — is pushed onto the `Backspace`
    /// navigation history (capped at 64 entries).
    pub fn open_detail(&mut self, item: &str, mode: DetailMode) {
        if let Some(prev) = self.detail.take() {
            if prev != (item.to_owned(), mode) && self.history.len() < 64 {
                self.history.push(prev);
            }
        }
        self.detail = Some((item.to_owned(), mode));
        self.tab = BrowserTab::Items;
        self.recipe_selected = 0;
        self.detail_scroll = 0;
    }

    /// Item id behind the current hover widget, if it names one —
    /// grid slots, bookmark/pinned rows and recipe-diagram slots.
    pub fn hovered_item(&self) -> Option<String> {
        match self.hover? {
            RecipeWidgetId::ItemSlot(vi) => self
                .visible_items()
                .get(vi)
                .map(|&i| self.index.items[i].id.clone()),
            RecipeWidgetId::BookmarkRow(i) => self.bookmarks.get(i).cloned(),
            RecipeWidgetId::PinnedTreeRow(i) => self.pinned_trees.get(i).map(|p| p.0.clone()),
            RecipeWidgetId::DetailSlot(n) => self.detail_slot_item(n),
            _ => None,
        }
    }

    /// Resolve a `DetailSlot(n)` id against the selected detail recipe —
    /// slot space is `inputs ++ outputs ++ chanced_outputs` (tag inputs
    /// cycle by [`clock_ticks`](Self::clock_ticks)).
    pub fn detail_slot_item(&self, n: usize) -> Option<String> {
        let recipes = self.detail_recipes();
        let recipe = recipes.get(self.recipe_selected)?;
        if n < recipe.inputs.len() {
            return Some(
                self.index
                    .resolve_ingredient_cycled(&recipe.inputs[n], self.clock_ticks),
            );
        }
        let n = n - recipe.inputs.len();
        if n < recipe.outputs.len() {
            return Some(recipe.outputs[n].item.clone());
        }
        recipe
            .chanced_outputs
            .get(n - recipe.outputs.len())
            .map(|(id, ..)| id.clone())
    }

    /// Build (or rebuild) the recipe tree for `(item, count)`.
    pub fn open_tree(&mut self, item: &str, count: u64) {
        self.tree = Some(self.index.recipe_tree(item, count.max(1), &self.recipe_selections));
        self.tab = BrowserTab::Tree;
        self.tree_scroll = 0;
    }

    /// Pin/unpin the current tree target in the bookmarks panel.
    pub fn pin_tree(&mut self) {
        let Some(tree) = &self.tree else {
            return;
        };
        let key = (tree.root_item.clone(), tree.root_count);
        if let Some(pos) = self.pinned_trees.iter().position(|p| *p == key) {
            self.pinned_trees.remove(pos);
        } else {
            self.pinned_trees.push(key);
        }
    }

    // -- input -----------------------------------------------------------

    /// Activate a widget (from a click, or directly in tests).
    pub fn activate(&mut self, id: RecipeWidgetId) -> Vec<RecipeBrowserAction> {
        match id {
            RecipeWidgetId::SearchField => self.search_focused = true,
            RecipeWidgetId::SearchClear => {
                if !self.search.is_empty() {
                    self.search.clear();
                    self.after_view_change();
                }
            }
            RecipeWidgetId::TabItems => {
                self.tab = BrowserTab::Items;
            }
            RecipeWidgetId::TabTree => {
                self.tab = BrowserTab::Tree;
            }
            RecipeWidgetId::TabWorldDropsInfo => {
                self.tab = BrowserTab::WorldDropsInfo;
                self.world_drops_scroll = 0;
            }
            RecipeWidgetId::TabMachineTutorials => {
                self.tab = BrowserTab::MachineTutorials;
            }
            RecipeWidgetId::DetailBack => {
                return self.back_or_close();
            }
            RecipeWidgetId::ItemSlot(slot) => {
                let len = self.visible_items().len();
                if slot < len {
                    self.selected = slot;
                }
            }
            RecipeWidgetId::GridScrollUp => {
                self.grid_scroll = self.grid_scroll.saturating_sub(1);
            }
            RecipeWidgetId::GridScrollDown => {
                self.grid_scroll += 1;
            }
            RecipeWidgetId::BookmarkRow(i) => {
                self.bookmark_selected = Some(i);
                if let Some(item) = self.bookmarks.get(i).cloned() {
                    self.select_item_by_id(&item);
                    self.open_detail(&item, DetailMode::Recipes);
                }
            }
            RecipeWidgetId::BookmarkUp => self.move_bookmark(-1),
            RecipeWidgetId::BookmarkDown => self.move_bookmark(1),
            RecipeWidgetId::BookmarkRemove => self.remove_selected_bookmark(),
            RecipeWidgetId::RecipeRow(i) => {
                if i < self.detail_recipes().len() {
                    self.recipe_selected = i;
                }
            }
            RecipeWidgetId::TransferRecipe(i) => {
                if let Some(&recipe) = self.detail_recipes().get(i) {
                    let max_craft = self.index.max_craft(recipe, &self.inventory);
                    return vec![RecipeBrowserAction::TransferToCraftingGrid {
                        recipe_id: recipe.id.clone(),
                        max_craft,
                    }];
                }
            }
            RecipeWidgetId::DetailSlot(n) => {
                if let Some(item) = self.detail_slot_item(n) {
                    if !item.starts_with('#') {
                        self.open_detail(&item, DetailMode::Recipes);
                    }
                }
            }
            RecipeWidgetId::CraftableOnlyToggle => {
                self.craftable_only = !self.craftable_only;
                self.after_view_change();
            }
            RecipeWidgetId::DetailScrollUp => {
                self.detail_scroll = self.detail_scroll.saturating_sub(1);
            }
            RecipeWidgetId::DetailScrollDown => {
                self.detail_scroll += 1;
            }
            RecipeWidgetId::TreeNodeRow(i) => {
                if let Some(tree) = &self.tree {
                    if let Some(root) = &tree.root {
                        if let Some((_, node)) = root.flatten().get(i) {
                            let item = node.item.clone();
                            self.open_detail(&item, DetailMode::Recipes);
                        }
                    }
                }
            }
            RecipeWidgetId::TreeScrollUp => {
                self.tree_scroll = self.tree_scroll.saturating_sub(1);
            }
            RecipeWidgetId::TreeScrollDown => {
                self.tree_scroll += 1;
            }
            RecipeWidgetId::TreeCountUp => self.retarget_tree(2.0),
            RecipeWidgetId::TreeCountDown => self.retarget_tree(0.5),
            RecipeWidgetId::TreePin => self.pin_tree(),
            RecipeWidgetId::PinnedTreeRow(i) => {
                if let Some((item, count)) = self.pinned_trees.get(i).cloned() {
                    self.open_tree(&item, count);
                }
            }
            RecipeWidgetId::WorldDropsTabMobs => self.set_world_drops_tab(WorldDropsTab::MobDrops),
            RecipeWidgetId::WorldDropsTabOres => self.set_world_drops_tab(WorldDropsTab::OreGen),
            RecipeWidgetId::WorldDropsTabTrades => self.set_world_drops_tab(WorldDropsTab::Trades),
            RecipeWidgetId::WorldDropsTabLoot => self.set_world_drops_tab(WorldDropsTab::Loot),
            RecipeWidgetId::WorldDropsTabEnchants => self.set_world_drops_tab(WorldDropsTab::Enchants),
            RecipeWidgetId::WorldDropsTabFuels => self.set_world_drops_tab(WorldDropsTab::Fuels),
            RecipeWidgetId::WorldDropsScrollUp => {
                self.world_drops_scroll = self.world_drops_scroll.saturating_sub(1);
            }
            RecipeWidgetId::WorldDropsScrollDown => {
                self.world_drops_scroll += 1;
            }
            RecipeWidgetId::MachineTutorialPrev => {
                if let Some(p) = &mut self.machine_tutorial {
                    p.prev_step();
                }
            }
            RecipeWidgetId::MachineTutorialNext => {
                if let Some(p) = &mut self.machine_tutorial {
                    p.next_step();
                }
            }
            RecipeWidgetId::MachineTutorialPlay => {
                if let Some(p) = &mut self.machine_tutorial {
                    p.playing = !p.playing;
                }
            }
            RecipeWidgetId::MachineTutorialRotateLeft => {
                if let Some(p) = &mut self.machine_tutorial {
                    p.rotate(-15.0);
                }
            }
            RecipeWidgetId::MachineTutorialRotateRight => {
                if let Some(p) = &mut self.machine_tutorial {
                    p.rotate(15.0);
                }
            }
        }
        Vec::new()
    }

    fn set_world_drops_tab(&mut self, tab: WorldDropsTab) {
        if self.world_drops_tab != tab {
            self.world_drops_tab = tab;
            self.world_drops_scroll = 0;
        }
    }

    /// Rebuild the current tree at `count × factor` (clamped 1..=4096).
    fn retarget_tree(&mut self, factor: f64) {
        let Some(tree) = &self.tree else {
            return;
        };
        let count = ((tree.root_count as f64 * factor).round() as u64).clamp(1, 4096);
        let item = tree.root_item.clone();
        self.open_tree(&item, count);
    }

    /// Point `selected` at `item` in the visible list (no-op if absent).
    fn select_item_by_id(&mut self, item: &str) {
        if let Some(pos) = self
            .visible_items()
            .iter()
            .position(|&i| self.index.items[i].id == item)
        {
            self.selected = pos;
        }
    }

    fn after_view_change(&mut self) {
        self.selected = 0;
        self.grid_scroll = 0;
    }

    fn move_selection(&mut self, offset: i64, wrap: bool) {
        let len = self.visible_items().len();
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

    fn move_recipe_selection(&mut self, offset: i64) {
        let len = self.detail_recipes().len();
        if len == 0 {
            self.recipe_selected = 0;
            return;
        }
        self.recipe_selected = (self.recipe_selected as i64 + offset)
            .clamp(0, len as i64 - 1) as usize;
    }

    /// The widget under `(x, y)` in a `width`×`height` frame, if any.
    pub fn widget_at(&self, width: u32, height: u32, x: i32, y: i32) -> Option<RecipeWidgetId> {
        self.render(width, height)
            .1
            .iter()
            .rev()
            .find(|h| h.rect.contains(x, y))
            .map(|h| h.id)
    }

    /// Route one device-agnostic input event; returns the actions the
    /// host must perform. `width`/`height` are the frame's physical
    /// pixels for pointer hit-testing and grid-column math.
    ///
    /// Keyboard/gamepad semantics (recipe-browser-style):
    ///
    /// * typing into a focused search edits it; with the search unfocused,
    ///   `R` opens recipes, `U` uses, `A`/`B` toggles a bookmark, `T`
    ///   builds the recipe tree, `P` jumps to Machine Tutorials, `+` transfers;
    /// * `Tab` toggles search focus; arrows navigate grid/list/columns,
    ///   `Left`/`Right` rotate the tutorial camera, `Up`/`Down` step it;
    /// * `Enter`/`Space` activate; `Escape` backs out or closes;
    /// * gamepad `South` mirrors `Enter`, D-pad mirrors arrows,
    ///   `East`/`Start` backs out or closes.
    pub fn handle_input(
        &mut self,
        ev: UiInputEvent,
        width: u32,
        height: u32,
    ) -> Vec<RecipeBrowserAction> {
        match ev {
            UiInputEvent::MouseMove { x, y } => {
                self.hover = self.widget_at(width, height, x, y);
                Vec::new()
            }
            UiInputEvent::Click { x, y } => match self.widget_at(width, height, x, y) {
                Some(id) => {
                    if id != RecipeWidgetId::SearchField {
                        self.search_focused = false;
                    }
                    self.activate(id)
                }
                None => Vec::new(),
            },
            UiInputEvent::KeyPressed(key) => self.handle_key(key, width, height),
            UiInputEvent::CharTyped(c) => {
                if self.search_focused {
                    if (' '..='~').contains(&c) {
                        self.search.push(c);
                        self.after_view_change();
                    }
                } else {
                    match c {
                        'r' | 'R' => {
                            if let Some(item) = self.selected_item() {
                                self.open_detail(&item, DetailMode::Recipes);
                            }
                        }
                        'u' | 'U' => {
                            if let Some(item) = self.selected_item() {
                                self.open_detail(&item, DetailMode::Uses);
                            }
                        }
                        'a' | 'A' | 'b' | 'B' => {
                            if let Some(item) = self.selected_item() {
                                self.toggle_bookmark(&item);
                            }
                        }
                        't' | 'T' => {
                            if let Some(item) = self.selected_item() {
                                self.open_tree(&item, 1);
                            }
                        }
                        'p' | 'P' => self.tab = BrowserTab::MachineTutorials,
                        '+' => return self.transfer_selected_recipe(),
                        c if (' '..='~').contains(&c) => {
                            self.search_focused = true;
                            self.search.push(c);
                            self.after_view_change();
                        }
                        _ => {}
                    }
                }
                Vec::new()
            }
            UiInputEvent::GamepadPressed(btn) => match btn {
                GamepadButton::DpadUp => self.handle_key(UiKey::Up, width, height),
                GamepadButton::DpadDown => self.handle_key(UiKey::Down, width, height),
                GamepadButton::DpadLeft => self.handle_key(UiKey::Left, width, height),
                GamepadButton::DpadRight => self.handle_key(UiKey::Right, width, height),
                GamepadButton::South => self.handle_key(UiKey::Enter, width, height),
                GamepadButton::East | GamepadButton::Start => self.back_or_close(),
            },
        }
    }

    /// Overlay hit-region click: same focus bookkeeping as
    /// [`handle_input`](Self::handle_input)'s `Click` arm, but driven by
    /// the host's own `RecipeHitRegion` hit test instead of the
    /// full-screen widget layout.
    pub fn activate_region(&mut self, id: RecipeWidgetId) -> Vec<RecipeBrowserAction> {
        if id != RecipeWidgetId::SearchField {
            self.search_focused = false;
        }
        self.activate(id)
    }

    /// `+` transfer for the selected detail recipe (or the first recipe
    /// producing the grid selection).
    fn transfer_selected_recipe(&mut self) -> Vec<RecipeBrowserAction> {
        let recipe = if self.detail.is_some() {
            self.detail_recipes().get(self.recipe_selected).map(|r| r.id.clone())
        } else {
            self.selected_item()
                .and_then(|i| self.index.recipes_producing(&i).first().map(|r| r.id.clone()))
        };
        let Some(id) = recipe else {
            return Vec::new();
        };
        let Some(r) = self.index.recipe(&id) else {
            return Vec::new();
        };
        let max_craft = self.index.max_craft(r, &self.inventory);
        vec![RecipeBrowserAction::TransferToCraftingGrid {
            recipe_id: id,
            max_craft,
        }]
    }

    /// Navigate back: pop the detail history stack, else clear an open
    /// detail, else ask the host to close the surface.
    pub fn back_or_close(&mut self) -> Vec<RecipeBrowserAction> {
        if let Some(prev) = self.history.pop() {
            self.detail = Some(prev);
            self.tab = BrowserTab::Items;
            self.recipe_selected = 0;
            self.detail_scroll = 0;
            return Vec::new();
        }
        if self.detail.take().is_some() {
            return Vec::new();
        }
        vec![RecipeBrowserAction::Close]
    }

    fn handle_key(&mut self, key: UiKey, width: u32, height: u32) -> Vec<RecipeBrowserAction> {
        match key {
            UiKey::Tab => self.search_focused = !self.search_focused,
            UiKey::Up | UiKey::Down => {
                let dir: i64 = if key == UiKey::Up { -1 } else { 1 };
                match self.tab {
                    BrowserTab::Items => {
                        if self.detail.is_some() {
                            self.move_recipe_selection(dir);
                        } else {
                            let cols = self.grid_cols(width, height) as i64;
                            self.move_selection(dir * cols, false);
                        }
                    }
                    BrowserTab::Tree => {
                        self.tree_scroll = (self.tree_scroll as i64 + dir).max(0) as usize;
                    }
                    BrowserTab::WorldDropsInfo => {
                        self.world_drops_scroll = (self.world_drops_scroll as i64 + dir).max(0) as usize;
                    }
                    BrowserTab::MachineTutorials => {
                        if let Some(p) = &mut self.machine_tutorial {
                            if dir < 0 {
                                p.prev_step();
                            } else {
                                p.next_step();
                            }
                        }
                    }
                }
            }
            UiKey::Left | UiKey::Right => {
                let dir: i64 = if key == UiKey::Left { -1 } else { 1 };
                match self.tab {
                    BrowserTab::Items => {
                        if self.detail.is_some() {
                            let page = self.detail_page_rows(height) as i64;
                            self.move_recipe_selection(dir * page.max(1));
                        } else {
                            self.move_selection(dir, false);
                        }
                    }
                    BrowserTab::MachineTutorials => {
                        if let Some(p) = &mut self.machine_tutorial {
                            p.rotate(if dir < 0 { -15.0 } else { 15.0 });
                        }
                    }
                    BrowserTab::Tree => {
                        let page = 10i64;
                        self.tree_scroll = (self.tree_scroll as i64 + dir * page).max(0) as usize;
                    }
                    BrowserTab::WorldDropsInfo => {
                        self.world_drops_scroll = (self.world_drops_scroll as i64 + dir * 5).max(0) as usize;
                    }
                }
            }
            UiKey::Enter | UiKey::Space => match self.tab {
                BrowserTab::Items => {
                    if self.search_focused {
                        self.search_focused = false;
                    } else if let Some((item, _)) = self.detail.clone() {
                        self.open_tree(&item, 1);
                    } else if let Some(item) = self.selected_item() {
                        self.open_detail(&item, DetailMode::Recipes);
                    }
                }
                BrowserTab::MachineTutorials => {
                    if let Some(p) = &mut self.machine_tutorial {
                        p.playing = !p.playing;
                    }
                }
                _ => {}
            },
            UiKey::Backspace => {
                // Search text edits first (refocusing like recipe browser); an empty
                // search walks the recipe-viewing history back one entry.
                if !self.search.is_empty() {
                    self.search.pop();
                    self.search_focused = true;
                    self.after_view_change();
                } else {
                    self.search_focused = false;
                    return self.back_or_close();
                }
            }
            UiKey::Escape => return self.back_or_close(),
        }
        Vec::new()
    }

    /// Grid columns at `(w, h)` physical px (same math as render).
    fn grid_cols(&self, w: u32, h: u32) -> usize {
        let s = self.effective_scale(h) as i32;
        let lw = (w.min(i32::MAX as u32) as i32) / s;
        let lh = (h.min(i32::MAX as u32) as i32) / s;
        if lw < 220 || lh < 150 {
            return 1;
        }
        let main_x = MARGIN + 4 + BOOKMARK_W + PANE_GAP;
        let main_w = lw - MARGIN - 4 - main_x;
        ((main_w - 8) / SLOT_PITCH).max(1) as usize
    }

    /// Detail rows per page at `height` physical px.
    fn detail_page_rows(&self, height: u32) -> usize {
        let s = self.effective_scale(height) as i32;
        let h = (height.min(i32::MAX as u32) as i32) / s;
        ((h - 4 - FOOTER_H - CONTENT_TOP - 22) / RECIPE_ROW_PITCH).max(1) as usize
    }
}

impl RecipeBrowserState {
    /// Best score a text token gets across an item's searchable fields —
    /// English name AND the active display locale's name (plus id, mod,
    /// tags, tooltips) so both always match.
    fn item_text_score(&self, item: &ItemEntry, token: &str) -> Option<u32> {
        let name = item.display_name.to_lowercase();
        let id = item.id.to_lowercase();
        let mut best = field_rank(token, &name).map(|r| r * 10 + 6);
        if let Some(local) = item.localized_names.get(&self.active_locale) {
            best = best.max(field_rank(token, &local.to_lowercase()).map(|r| r * 10 + 6));
        }
        best = best.max(field_rank(token, &id).map(|r| r * 10 + 5));
        best = best.max(field_rank(token, &item.mod_id.to_lowercase()).map(|r| r * 10 + 3));
        for tag in &item.tags {
            best = best.max(field_rank(token, &tag.to_lowercase()).map(|r| r * 10 + 2));
        }
        for line in &item.tooltip_lines {
            best = best.max(field_rank(token, &line.to_lowercase()).map(|r| r * 10 + 1));
        }
        // Japanese-aware fallback shared with `recipe_search`: the token
        // expands through `query_variants` (romaji→kana, kana→romaji,
        // glide-folded spellings) and is matched against the localized
        // name's normalized forms, so `daia`, `ダイヤ` and `だいあ` all
        // reach the same entry even when its display name is kanji or
        // the typed spelling drifts (`daiya`/`diya`).
        if best.is_none() {
            let mut fields: Vec<&str> = Vec::with_capacity(2);
            if let Some(local) = item.localized_names.get(&self.active_locale) {
                fields.push(local);
            }
            if let Some(ja) = item.localized_names.get("ja_jp") {
                if !fields.contains(&ja.as_str()) {
                    fields.push(ja);
                }
            }
            'outer: for variant in recipe_search::query_variants(token) {
                if variant.is_empty() {
                    continue;
                }
                for field in &fields {
                    let folded = recipe_search::normalize_kana(&field.to_lowercase());
                    let romaji = recipe_search::glide_fold(&recipe_search::kana_to_romaji(&folded));
                    if folded.contains(&variant)
                        || (!romaji.is_empty() && romaji.contains(&variant))
                    {
                        best = Some(4);
                        break 'outer;
                    }
                }
            }
        }
        best
    }
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

impl RecipeBrowserState {
    /// Build the frame: `(draw_list, hit_regions)`. Everything is computed
    /// in logical pixels and multiplied by
    /// [`effective_scale`](Self::effective_scale) on emit, so the same
    /// layout serves `640×360` through `3840×2160` and every hit region is
    /// pixel-identical to its widget's visuals.
    pub fn render(&self, width: u32, height: u32) -> (UiDrawList, Vec<RecipeHitRegion>) {
        let mut lay = Lay::new(self, width, height);
        let mut hits = Vec::new();
        if lay.w < 220 || lay.h < 150 {
            self.render_compact(&mut lay, &mut hits);
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
        self.render_tabs(&mut lay, &mut hits);
        self.render_bookmarks(&mut lay, &mut hits);
        self.render_main(&mut lay, &mut hits);
        self.render_footer(&mut lay);
        (lay.finish(), hits)
    }

    /// recipe overlay side-panel overlay for an open container screen. `panel`
    /// is the container's physical-pixel rect; the browser draws a
    /// bookmarks/pinned-trees column to its left and a search + item
    /// grid (or recipe detail) column to its right. Returns `None` when
    /// either side lacks room — panels never overlap `panel`.
    pub fn render_overlay(
        &self,
        width: u32,
        height: u32,
        panel: UiRect,
    ) -> Option<(UiDrawList, Vec<RecipeHitRegion>)> {
        let mut lay = Lay::new(self, width, height);
        let mut hits = Vec::new();
        if lay.w < 220 || lay.h < 150 {
            return None;
        }
        let s = lay.s;
        let panel_l = UiRect::new(
            panel.x / s,
            panel.y / s,
            panel.w / (s.max(1)) as u32,
            panel.h / (s.max(1)) as u32,
        );
        // Preferred widths at scale 1-2; when the UI scale leaves less
        // room the panels shrink (recipe-browser-style) down to a floor — only then
        // does the overlay hide.
        const LEFT_W: i32 = 148;
        const RIGHT_W: i32 = 196;
        const LEFT_MIN: i32 = 96;
        const RIGHT_MIN: i32 = 120;
        const GAP: i32 = 4;
        let left_avail = panel_l.x - GAP;
        let right_avail = lay.w - panel_l.right() - GAP;
        if left_avail < LEFT_MIN || right_avail < RIGHT_MIN {
            return None;
        }
        let left_w = LEFT_W.min(left_avail);
        let right_w = RIGHT_W.min(right_avail);
        let left = UiRect::new(
            panel_l.x - GAP - left_w,
            panel_l.y,
            left_w as u32,
            panel_l.h,
        );
        let right = UiRect::new(panel_l.right() + GAP, panel_l.y, right_w as u32, panel_l.h);
        self.render_overlay_bookmarks(&mut lay, &mut hits, left);
        self.render_overlay_right(&mut lay, &mut hits, right);
        self.render_overlay_tooltip(&mut lay);
        Some((lay.finish(), hits))
    }

    /// Left overlay column: bookmarks + pinned trees, each row live-
    /// compared against `self.inventory` (remaining count, or a green
    /// check when the player can craft it right now).
    fn render_overlay_bookmarks(
        &self,
        lay: &mut Lay,
        hits: &mut Vec<RecipeHitRegion>,
        panel: UiRect,
    ) {
        let lang = self.lang;
        lay.fill_rect(panel, DETAIL_BG);
        lay.border_rect(panel, palette::PANEL_BORDER);
        lay.text_fit(
            panel.x + 4,
            panel.y + 3,
            1,
            palette::TEXT_DIM,
            tr(lang, "Bookmarks", "ブックマーク"),
            ((panel.w as i32 - 8) / 8).max(1) as usize,
        );
        let list_top = panel.y + 14;
        let list_bottom = panel.bottom() - 14;
        let rows = ((list_bottom - list_top) / BOOKMARK_PITCH).max(0) as usize;
        let mut y = list_top;
        let mut row_i = 0usize;
        for (i, (item, count)) in self.pinned_trees.iter().enumerate() {
            if row_i >= rows {
                break;
            }
            let rect = UiRect::new(panel.x + 2, y, panel.w - 4, BOOKMARK_H as u32);
            let hovered = self.hover == Some(RecipeWidgetId::PinnedTreeRow(i));
            lay.fill_rect(
                rect,
                if hovered {
                    palette::BUTTON
                } else {
                    palette::SLOT
                },
            );
            let tree = self
                .index
                .recipe_tree(item, (*count).max(1), &self.recipe_selections);
            let remaining = tree.remaining_needed(&self.inventory);
            let name = self
                .index
                .display_name_for_locale(item, &self.active_locale);
            let label = if remaining.is_empty() {
                format!("T: {count}x {name} [OK]")
            } else {
                let lack: String = remaining
                    .iter()
                    .take(2)
                    .map(|(id, n)| format!(" {n}x{}", self.index.display_name(id)))
                    .collect();
                format!("T: {count}x {name} -{lack}")
            };
            lay.text_fit(
                rect.x + 3,
                rect.y + 3,
                1,
                if remaining.is_empty() {
                    UiColor::rgb(120, 220, 120)
                } else {
                    palette::AIR
                },
                &label,
                ((rect.w as i32 - 6) / 8).max(1) as usize,
            );
            lay.region(hits, RecipeWidgetId::PinnedTreeRow(i), rect);
            y += BOOKMARK_PITCH;
            row_i += 1;
        }
        for (i, item) in self.bookmarks.iter().enumerate() {
            if row_i >= rows {
                break;
            }
            let rect = UiRect::new(panel.x + 2, y, panel.w - 4, BOOKMARK_H as u32);
            let hovered = self.hover == Some(RecipeWidgetId::BookmarkRow(i));
            lay.fill_rect(
                rect,
                if hovered {
                    palette::BUTTON
                } else {
                    palette::SLOT
                },
            );
            lay.icon(UiRect::new(rect.x + 2, rect.y + 1, 12, 12), item);
            let name = self
                .index
                .display_name_for_locale(item, &self.active_locale);
            let have = self.inventory.get(item).copied().unwrap_or(0);
            let label = if have > 0 {
                format!("{name} x{have}")
            } else {
                name
            };
            lay.text_fit(
                rect.x + 17,
                rect.y + 3,
                1,
                palette::TEXT,
                &label,
                ((rect.w as i32 - 21) / 8).max(1) as usize,
            );
            lay.region(hits, RecipeWidgetId::BookmarkRow(i), rect);
            y += BOOKMARK_PITCH;
            row_i += 1;
        }
        if self.bookmarks.is_empty() && self.pinned_trees.is_empty() {
            lay.text_fit(
                panel.x + 6,
                list_top + 4,
                1,
                palette::TEXT_DIM,
                tr(lang, "(press A on an item)", "(Aで追加)"),
                ((panel.w as i32 - 12) / 8).max(1) as usize,
            );
        }
    }

    /// Right overlay column: search row + `x` clear + craftable-only
    /// toggle, then either the recipe-detail card or the item grid.
    fn render_overlay_right(&self, lay: &mut Lay, hits: &mut Vec<RecipeHitRegion>, panel: UiRect) {
        let lang = self.lang;
        lay.fill_rect(panel, DETAIL_BG);
        lay.border_rect(panel, palette::PANEL_BORDER);

        // Search row: [field][x]
        let clear_w = 16;
        let field = UiRect::new(
            panel.x + 3,
            panel.y + 3,
            (panel.w as i32 - 6 - clear_w - 2).max(24) as u32,
            14,
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
                palette::ACCENT
            } else {
                palette::BUTTON_BORDER
            },
        );
        let cells = ((field.w as i32 - 8) / 8).max(1) as usize;
        if self.search.is_empty() && !self.search_focused {
            lay.text_fit(
                field.x + 4,
                field.y + 3,
                1,
                palette::TEXT_DIM,
                tr(lang, "Search...", "検索..."),
                cells,
            );
        } else {
            let mut shown = self.search.clone();
            if self.search_focused {
                shown.push('_');
            }
            if crate::draw::text_cells(&shown) > cells {
                shown = format!("...{}", crate::draw::last_cells(&shown, cells - 3));
            }
            lay.text_fit(field.x + 4, field.y + 3, 1, palette::TEXT, &shown, cells);
        }
        lay.region(hits, RecipeWidgetId::SearchField, field);
        let clear = UiRect::new(field.right() + 2, panel.y + 3, clear_w as u32, 14);
        lay.fill_rect(
            clear,
            if self.search.is_empty() {
                palette::BUTTON_DISABLED
            } else if self.hover == Some(RecipeWidgetId::SearchClear) {
                palette::DANGER
            } else {
                palette::BUTTON
            },
        );
        lay.ctext_in(clear, 1, palette::TEXT, "x");
        if !self.search.is_empty() {
            lay.region(hits, RecipeWidgetId::SearchClear, clear);
        }

        // Craftable-only toggle: [x] Craftable.
        let tog = UiRect::new(panel.x + 3, panel.y + 19, panel.w - 6, 12);
        let on = self.craftable_only;
        lay.fill_rect(
            UiRect::new(tog.x, tog.y + 1, 10, 10),
            if on { palette::ACCENT } else { palette::SLOT },
        );
        lay.border(
            tog.x,
            tog.y + 1,
            10,
            10,
            if on {
                palette::ACCENT
            } else {
                palette::BUTTON_BORDER
            },
        );
        lay.text_fit(
            tog.x + 14,
            tog.y + 2,
            1,
            if on { palette::TEXT } else { palette::TEXT_DIM },
            tr(lang, "Craftable only", "作成可能のみ"),
            ((tog.w as i32 - 16) / 8).max(1) as usize,
        );
        lay.region(hits, RecipeWidgetId::CraftableOnlyToggle, tog);

        let body = UiRect::new(
            panel.x + 2,
            panel.y + 33,
            (panel.w as i32 - 4).max(8) as u32,
            (panel.h as i32 - 35).max(8) as u32,
        );
        if self.detail.is_some() {
            self.render_detail(lay, hits, body);
        } else {
            self.render_overlay_grid(lay, hits, body);
        }
    }

    /// Icon-only item grid inside the right overlay column (recipe browser sidebar
    /// semantics: `ItemSlot` regions, scroll buttons at the bottom).
    fn render_overlay_grid(&self, lay: &mut Lay, hits: &mut Vec<RecipeHitRegion>, body: UiRect) {
        let cols = ((body.w as i32) / SLOT_PITCH).max(1) as usize;
        let visible = self.visible_items();
        let max_vi = visible.len().saturating_sub(1);
        let _ = max_vi;
        let rows = ((body.h as i32 - 12) / SLOT_PITCH).max(1) as usize;
        let per_page = cols * rows;
        let page = self
            .grid_scroll
            .min(visible.len().saturating_sub(1) / per_page.max(1));
        let start = page * per_page;
        for (k, &vi) in visible.iter().skip(start).take(per_page).enumerate() {
            let (gx, gy) = (
                body.x + (k % cols) as i32 * SLOT_PITCH,
                body.y + (k / cols) as i32 * SLOT_PITCH,
            );
            let item = &self.index.items[vi];
            let slot = UiRect::new(gx, gy, SLOT as u32, SLOT as u32);
            let sel = vi == self.selected;
            let hovered = self.hover == Some(RecipeWidgetId::ItemSlot(vi));
            lay.fill_rect(
                slot,
                if sel || hovered {
                    palette::BUTTON_HOVER
                } else {
                    UiColor::rgba(12, 12, 16, 200)
                },
            );
            lay.border_rect(
                slot,
                if sel {
                    palette::ACCENT
                } else {
                    UiColor::rgba(96, 96, 108, 140)
                },
            );
            lay.icon(UiRect::new(gx + 1, gy + 1, 16, 16), &item.id);
            lay.region(hits, RecipeWidgetId::ItemSlot(vi), slot);
        }
        if visible.len() > per_page {
            let up = UiRect::new(body.right() - 24, body.bottom() - 11, 11, 10);
            let down = UiRect::new(body.right() - 12, body.bottom() - 11, 11, 10);
            lay.fill_rect(up, palette::BUTTON);
            lay.fill_rect(down, palette::BUTTON);
            lay.ctext_in(up, 1, palette::TEXT, "^");
            lay.ctext_in(down, 1, palette::TEXT, "v");
            lay.region(hits, RecipeWidgetId::GridScrollUp, up);
            lay.region(hits, RecipeWidgetId::GridScrollDown, down);
        }
    }

    /// recipe-browser-style hover card for whichever item the pointer rests on:
    /// localized name, English name when it differs, `@mod`, `#tags`
    /// (abbreviated), inventory count, and the `R`/`U`/`A` hint row.
    fn render_overlay_tooltip(&self, lay: &mut Lay) {
        let lang = self.lang;
        let Some(item_id) = self.hovered_item() else {
            return;
        };
        let Some((mx, my)) = self.overlay_mouse else {
            return;
        };
        let s = lay.s.max(1);
        let entry = self.index.item(&item_id);
        let local_name = self
            .index
            .display_name_for_locale(&item_id, &self.active_locale);
        // The classic purple-bordered recipe browser tooltip — shared verbatim with
        // `recipe_overlay` so an item hover looks identical on the full
        // browser and the native container overlay.
        let mut item = recipe_overlay::OverlayItem::new(
            &item_id,
            &local_name,
            entry.map(|e| e.mod_id.as_str()).unwrap_or("minecraft"),
        );
        if let Some(e) = entry {
            item.tags = e.tags.clone();
            if e.display_name != local_name {
                item.info_lines.push(e.display_name.clone());
            }
        }
        let have = self.inventory.get(&item_id).copied().unwrap_or(0);
        if have > 0 {
            item.info_lines
                .push(format!("{} x{have}", tr(lang, "In inventory", "所持")));
        }
        item.info_lines.push(
            tr(
                lang,
                "R: recipes  U: uses  A: bookmark",
                "R: レシピ  U: 用途  A: 登録",
            )
            .to_owned(),
        );
        recipe_overlay::render_item_tooltip(
            &mut lay.list,
            lay.w * s,
            lay.h * s,
            (mx, my),
            &item,
            false,
            s,
        );
    }

    /// Compact single-column layout for narrow frames. The regular desktop
    /// layout needs the bookmarks sidebar plus a detail grid and therefore
    /// starts at 220 logical px; below that this fallback keeps the search,
    /// tabs, visible items/detail, scrolling and keyboard selection fully
    /// interactive down to 160 logical px (and still paints a bounded frame
    /// below that rather than the old blank screen).
    fn render_compact(&self, lay: &mut Lay, hits: &mut Vec<RecipeHitRegion>) {
        let lang = self.lang;
        lay.fill(0, 0, lay.w, lay.h, palette::BACKGROUND_TOP);
        // Below the supported compact floor there is no honest room for even
        // one row plus its controls. Keep a bounded nonblank backdrop rather
        // than emitting clipped controls that would overlap the frame edge.
        if lay.w < 160 || lay.h < 100 {
            return;
        }
        lay.fill(1, 1, lay.w - 2, lay.h - 2, palette::PANEL);
        lay.border(1, 1, lay.w - 2, lay.h - 2, palette::PANEL_BORDER);
        lay.text_fit(
            4,
            4,
            1,
            palette::TEXT,
            tr(lang, "Recipes", "レシピ"),
            ((lay.w - 8 - 56) / 8).max(1) as usize,
        );

        // Search is the primary narrow-screen affordance. Keep the clear
        // button adjacent and use the same widget ids as the wide layout.
        let clear_w = 14;
        // Keep the host shell's 52 logical px Done button corner clear.
        let field_w = (lay.w - 8 - clear_w - 2 - 56).max(20);
        let field = UiRect::new(4, 14, field_w as u32, 13);
        lay.fill_rect(
            field,
            if self.search_focused { palette::SLOT } else { palette::BUTTON_DISABLED },
        );
        lay.border_rect(
            field,
            if self.search_focused { palette::ACCENT } else { palette::BUTTON_BORDER },
        );
        let cells = ((field.w as i32 - 8) / 8).max(1) as usize;
        if self.search.is_empty() && !self.search_focused {
            lay.text_fit(field.x + 4, field.y + 3, 1, palette::TEXT_DIM, tr(lang, "Search...", "検索..."), cells);
        } else {
            let mut shown = self.search.clone();
            if self.search_focused {
                shown.push('_');
            }
            lay.text_fit(field.x + 4, field.y + 3, 1, palette::TEXT, &shown, cells);
        }
        lay.region(hits, RecipeWidgetId::SearchField, field);
        let clear = UiRect::new(field.right() + 2, 14, clear_w as u32, 13);
        lay.fill_rect(
            clear,
            if self.search.is_empty() { palette::BUTTON_DISABLED } else { palette::BUTTON },
        );
        lay.ctext_in(clear, 1, palette::TEXT, "x");
        if !self.search.is_empty() {
            lay.region(hits, RecipeWidgetId::SearchClear, clear);
        }

        // Abbreviated one-row tab strip: the full localized labels cannot fit
        // honestly at 160px, while these stable targets retain every mode.
        let tabs = [
            (BrowserTab::Items, RecipeWidgetId::TabItems, tr(lang, "I", "品")),
            (BrowserTab::Tree, RecipeWidgetId::TabTree, tr(lang, "T", "ツ")),
            (BrowserTab::WorldDropsInfo, RecipeWidgetId::TabWorldDropsInfo, tr(lang, "D", "ド")),
            (BrowserTab::MachineTutorials, RecipeWidgetId::TabMachineTutorials, tr(lang, "M", "機")),
        ];
        let tab_w = ((lay.w - 8 - 3 * 2) / 4).clamp(12, 32);
        for (i, (tab, id, label)) in tabs.iter().enumerate() {
            let rect = UiRect::new(4 + i as i32 * (tab_w + 2), 30, tab_w as u32, 12);
            let active = self.tab == *tab;
            lay.fill_rect(rect, if active { palette::ACCENT } else { palette::BUTTON });
            lay.border_rect(rect, palette::BUTTON_BORDER);
            lay.ctext_in(rect, 1, if active { palette::TEXT_DARK } else { palette::TEXT }, label);
            lay.region(hits, *id, rect);
        }

        let footer_h = 12;
        let main = UiRect::new(
            2,
            45,
            (lay.w - 4).max(1) as u32,
            (lay.h - 45 - footer_h - 2).max(1) as u32,
        );
        match self.tab {
            BrowserTab::Items if self.detail.is_some() => self.render_detail(lay, hits, main),
            BrowserTab::Items => self.render_compact_items(lay, hits, main),
            BrowserTab::Tree => self.render_tree(lay, hits, main),
            BrowserTab::WorldDropsInfo => self.render_world_drops_info(lay, hits, main),
            BrowserTab::MachineTutorials => self.render_machine_tutorial(lay, hits, main),
        }

        let y = lay.h - footer_h - 1;
        let selected = self.selected_item().unwrap_or_else(|| tr(lang, "none", "なし").to_owned());
        lay.text_fit(
            5,
            y + 2,
            1,
            palette::TEXT_DIM,
            &format!("{}: {}", tr(lang, "Sel", "選択"), selected),
            ((lay.w - 10) / 8).max(1) as usize,
        );
    }

    /// Single-column item browser used by [`render_compact`](Self::render_compact).
    /// Rows expose the same `ItemSlot(visible_index)` hit ids as the wide
    /// grid, so keyboard/mouse activation does not fork semantically.
    fn render_compact_items(&self, lay: &mut Lay, hits: &mut Vec<RecipeHitRegion>, main: UiRect) {
        lay.fill_rect(main, DETAIL_BG);
        lay.border_rect(main, palette::PANEL_BORDER);
        let visible = self.visible_items();
        if visible.is_empty() {
            lay.text_fit(
                main.x + 5,
                main.y + 6,
                1,
                palette::TEXT_DIM,
                if self.index.items.is_empty() {
                    tr(self.lang, "No items indexed", "アイテムなし")
                } else {
                    tr(self.lang, "No matches", "一致なし")
                },
                ((main.w as i32 - 10) / 8).max(1) as usize,
            );
            return;
        }
        let row_h = 15;
        let rows = ((main.h as i32 - 6) / row_h).max(1) as usize;
        let scroll = self.grid_scroll.min(visible.len().saturating_sub(rows));
        let scrollable = visible.len() > rows;
        let row_w = main.w.saturating_sub(if scrollable { 16 } else { 4 });
        for (vi, &item_index) in visible.iter().enumerate().skip(scroll).take(rows) {
            let slot_row = vi - scroll;
            let item = &self.index.items[item_index];
            let rect = UiRect::new(main.x + 2, main.y + 3 + slot_row as i32 * row_h, row_w, 14);
            let selected = vi == self.selected;
            let hovered = self.hover == Some(RecipeWidgetId::ItemSlot(vi));
            lay.fill_rect(
                rect,
                if selected {
                    palette::BUTTON
                } else if hovered {
                    palette::BUTTON_HOVER
                } else {
                    palette::SLOT
                },
            );
            if selected {
                lay.border_rect(rect, palette::ACCENT);
            }
            lay.icon(UiRect::new(rect.x + 1, rect.y + 1, 12, 12), &item.id);
            let name = self.index.display_name_for_locale(&item.id, &self.active_locale);
            lay.text_fit(
                rect.x + 16,
                rect.y + 3,
                1,
                palette::TEXT,
                &name,
                ((rect.w as i32 - 20) / 8).max(1) as usize,
            );
            lay.region(hits, RecipeWidgetId::ItemSlot(vi), rect);
        }
        if scrollable {
            let up = UiRect::new(main.right() - 11, main.y + 2, 9, 11);
            let down = UiRect::new(main.right() - 11, main.bottom() - 13, 9, 11);
            lay.fill_rect(up, palette::BUTTON);
            lay.fill_rect(down, palette::BUTTON);
            lay.ctext_in(up, 1, palette::TEXT, "^");
            lay.ctext_in(down, 1, palette::TEXT, "v");
            lay.region(hits, RecipeWidgetId::GridScrollUp, up);
            lay.region(hits, RecipeWidgetId::GridScrollDown, down);
        }
    }

    fn render_header(&self, lay: &mut Lay, hits: &mut Vec<RecipeHitRegion>) {
        let lang = self.lang;
        let title = format!(
            "{} ({} {})",
            tr(lang, "Recipes & Resources", "レシピと資源"),
            self.index.items.len(),
            tr(lang, "items", "個")
        );
        lay.text_fit(
            MARGIN + 4,
            MARGIN + 4,
            1,
            palette::TEXT,
            &title,
            // Reserve the top-right corner: the host shell draws a Done
            // chrome button there when this screen is embedded.
            ((lay.w - 2 * MARGIN - 8 - 64) / 8).max(1) as usize,
        );

        // Search row: [search field][x]
        let clear_w = 18;
        let field_x = MARGIN + 4;
        let field_w = lay.w - field_x - MARGIN - 4 - clear_w - 4 - MARGIN;
        let field = UiRect::new(field_x, SEARCH_Y, field_w.max(24) as u32, 16);
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
                tr(lang, "Search items... (@mod #tag $cat)", "アイテムを検索..."),
                cells,
            );
        } else {
            let mut shown = self.search.clone();
            if self.search_focused {
                shown.push('_');
            }
            if crate::draw::text_cells(&shown) > cells {
                shown = format!("...{}", crate::draw::last_cells(&shown, cells - 3));
            }
            lay.text_fit(field.x + 5, field.y + 4, 1, palette::TEXT, &shown, cells);
        }
        lay.region(hits, RecipeWidgetId::SearchField, field);

        let clear = UiRect::new(field.right() + 4, SEARCH_Y, clear_w as u32, 16);
        lay.fill_rect(
            clear,
            if self.search.is_empty() {
                palette::BUTTON_DISABLED
            } else if self.hover == Some(RecipeWidgetId::SearchClear) {
                palette::DANGER
            } else {
                palette::BUTTON
            },
        );
        lay.ctext_in(clear, 1, palette::TEXT, "x");
        if !self.search.is_empty() {
            lay.region(hits, RecipeWidgetId::SearchClear, clear);
        }
    }

    fn render_tabs(&self, lay: &mut Lay, hits: &mut Vec<RecipeHitRegion>) {
        let lang = self.lang;
        let tabs = [
            (BrowserTab::Items, RecipeWidgetId::TabItems),
            (BrowserTab::Tree, RecipeWidgetId::TabTree),
            (BrowserTab::WorldDropsInfo, RecipeWidgetId::TabWorldDropsInfo),
            (BrowserTab::MachineTutorials, RecipeWidgetId::TabMachineTutorials),
        ];
        let mut x = MARGIN + 4;
        for (tab, id) in tabs {
            let label = tab.label(lang);
            let w = crate::draw::text_width(label, 1) as i32 + 12;
            if x + w > lay.w - MARGIN {
                break;
            }
            let rect = UiRect::new(x, TABS_Y, w as u32, 14);
            let active = self.tab == tab;
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
                if active { palette::TEXT_DARK } else { palette::TEXT },
                label,
            );
            lay.region(hits, id, rect);
            x += w + 4;
        }
    }

    fn render_bookmarks(&self, lay: &mut Lay, hits: &mut Vec<RecipeHitRegion>) {
        let lang = self.lang;
        let panel = UiRect::new(
            MARGIN + 4,
            CONTENT_TOP,
            BOOKMARK_W as u32,
            (lay.h - 4 - FOOTER_H - CONTENT_TOP).max(8) as u32,
        );
        lay.fill_rect(panel, DETAIL_BG);
        lay.border_rect(panel, palette::PANEL_BORDER);
        lay.text_fit(
            panel.x + 4,
            panel.y + 3,
            1,
            palette::TEXT_DIM,
            tr(lang, "Bookmarks", "ブックマーク"),
            ((panel.w as i32 - 8) / 8).max(1) as usize,
        );

        // Pinned trees first, then bookmarked items; action buttons stay
        // pinned to the panel bottom so rows never overlap them.
        let controls_h = 18;
        let list_top = panel.y + 14;
        let list_bottom = panel.bottom() - controls_h - 2;
        let rows = ((list_bottom - list_top) / BOOKMARK_PITCH).max(0) as usize;
        let mut y = list_top;
        let mut row_i = 0usize;
        for (i, (item, count)) in self.pinned_trees.iter().enumerate() {
            if row_i >= rows {
                break;
            }
            let rect = UiRect::new(panel.x + 2, y, panel.w - 4, BOOKMARK_H as u32);
            let hovered = self.hover == Some(RecipeWidgetId::PinnedTreeRow(i));
            lay.fill_rect(rect, if hovered { palette::BUTTON } else { palette::SLOT });
            let label = format!("T: {}x {}", count, self.index.display_name(item));
            lay.text_fit(
                rect.x + 4,
                rect.y + 3,
                1,
                palette::AIR,
                &label,
                ((rect.w as i32 - 8) / 8).max(1) as usize,
            );
            lay.region(hits, RecipeWidgetId::PinnedTreeRow(i), rect);
            y += BOOKMARK_PITCH;
            row_i += 1;
        }
        for (i, item) in self.bookmarks.iter().enumerate() {
            if row_i >= rows {
                break;
            }
            let rect = UiRect::new(panel.x + 2, y, panel.w - 4, BOOKMARK_H as u32);
            let sel = self.bookmark_selected == Some(i);
            let hovered = self.hover == Some(RecipeWidgetId::BookmarkRow(i));
            lay.fill_rect(
                rect,
                if sel {
                    palette::BUTTON_HOVER
                } else if hovered {
                    palette::BUTTON
                } else {
                    palette::SLOT
                },
            );
            lay.fill(
                rect.x + 3,
                rect.y + 2,
                10,
                10,
                item_color(item),
            );
            let name = self.index.display_name(item);
            lay.text_fit(
                rect.x + 16,
                rect.y + 3,
                1,
                palette::TEXT,
                &name,
                ((rect.w as i32 - 20) / 8).max(1) as usize,
            );
            lay.region(hits, RecipeWidgetId::BookmarkRow(i), rect);
            y += BOOKMARK_PITCH;
            row_i += 1;
        }
        if self.bookmarks.is_empty() && self.pinned_trees.is_empty() {
            lay.text_fit(
                panel.x + 6,
                list_top + 4,
                1,
                palette::TEXT_DIM,
                tr(lang, "(press A on an item)", "(Aで追加)"),
                ((panel.w as i32 - 12) / 8).max(1) as usize,
            );
        }

        // Reorder/remove controls for the selected bookmark.
        let cy = panel.bottom() - controls_h + 1;
        let buttons = [
            (RecipeWidgetId::BookmarkUp, "^"),
            (RecipeWidgetId::BookmarkDown, "v"),
            (RecipeWidgetId::BookmarkRemove, "x"),
        ];
        let bw = 20;
        let mut bx = panel.x + 4;
        for (id, label) in buttons {
            let rect = UiRect::new(bx, cy, bw as u32, 14);
            let live = self.bookmark_selected.is_some();
            let bg = if !live {
                palette::BUTTON_DISABLED
            } else if self.hover == Some(id) {
                palette::BUTTON_HOVER
            } else {
                palette::BUTTON
            };
            lay.fill_rect(rect, bg);
            lay.border_rect(rect, palette::BUTTON_BORDER);
            lay.ctext_in(rect, 1, palette::TEXT, label);
            if live {
                lay.region(hits, id, rect);
            }
            bx += bw + 4;
        }
    }

    fn render_main(&self, lay: &mut Lay, hits: &mut Vec<RecipeHitRegion>) {
        let main = UiRect::new(
            MARGIN + 4 + BOOKMARK_W + PANE_GAP,
            CONTENT_TOP,
            (lay.w - MARGIN - 4 - (MARGIN + 4 + BOOKMARK_W + PANE_GAP)).max(8) as u32,
            (lay.h - 4 - FOOTER_H - CONTENT_TOP).max(8) as u32,
        );
        match self.tab {
            BrowserTab::Items => {
                if self.detail.is_some() {
                    self.render_detail(lay, hits, main);
                } else {
                    self.render_grid(lay, hits, main);
                }
            }
            BrowserTab::Tree => self.render_tree(lay, hits, main),
            BrowserTab::WorldDropsInfo => self.render_world_drops_info(lay, hits, main),
            BrowserTab::MachineTutorials => self.render_machine_tutorial(lay, hits, main),
        }
    }

    /// recipe browser item grid: 18px slots, deterministic swatch + initial letter.
    fn render_grid(&self, lay: &mut Lay, hits: &mut Vec<RecipeHitRegion>, main: UiRect) {
        lay.fill_rect(main, DETAIL_BG);
        lay.border_rect(main, palette::PANEL_BORDER);
        let visible = self.visible_items();
        let rows = ((main.h as i32 - 8) / SLOT_PITCH).max(1) as usize;
        // Reserve the scrollbar gutter up front when the grid overflows.
        let first_cols = ((main.w as i32 - 8) / SLOT_PITCH).max(1) as usize;
        let scrollable = visible.len() > first_cols * rows;
        let gutter = if scrollable { 16 } else { 0 };
        let cols = ((main.w as i32 - 8 - gutter) / SLOT_PITCH).max(1) as usize;
        let page = cols * rows;
        let scroll = self
            .grid_scroll
            .min(visible.len().saturating_sub(page).div_ceil(cols.max(1)))
            * cols;

        if visible.is_empty() {
            lay.text_fit(
                main.x + 6,
                main.y + 8,
                1,
                palette::TEXT_DIM,
                if self.index.items.is_empty() {
                    tr(self.lang, "No items indexed", "アイテムなし")
                } else {
                    tr(self.lang, "No matches", "一致なし")
                },
                ((main.w as i32 - 12) / 8).max(1) as usize,
            );
            return;
        }

        for (vi, &item_index) in visible.iter().enumerate().skip(scroll).take(page) {
            let slot_i = vi - scroll;
            let gx = main.x + 4 + (slot_i % cols) as i32 * SLOT_PITCH;
            let gy = main.y + 4 + (slot_i / cols) as i32 * SLOT_PITCH;
            let slot = UiRect::new(gx, gy, SLOT as u32, SLOT as u32);
            let item = &self.index.items[item_index];
            let sel = vi == self.selected;
            let hovered = self.hover == Some(RecipeWidgetId::ItemSlot(vi));
            lay.fill_rect(slot, palette::SLOT);
            lay.border_rect(
                slot,
                if sel {
                    palette::ACCENT
                } else if hovered {
                    palette::BUTTON_BORDER_HOVER
                } else {
                    palette::BUTTON_BORDER
                },
            );
            // Real item texture when the atlas has one; the software
            // rasterizer falls back to the deterministic swatch.
            lay.icon(
                UiRect::new(gx + 1, gy + 1, (SLOT - 2) as u32, (SLOT - 2) as u32),
                &item.id,
            );
            if self.is_bookmarked(&item.id) {
                lay.fill(gx + SLOT - 4, gy + 1, 3, 3, palette::WARNING);
            }
            lay.region(hits, RecipeWidgetId::ItemSlot(vi), slot);
        }

        // Scroll buttons only when the grid overflows its page.
        if scrollable {
            let sb_x = main.right() - 12;
            let up = UiRect::new(sb_x, main.y + 2, 10, 12);
            let down = UiRect::new(sb_x, main.bottom() - 14, 10, 12);
            lay.fill_rect(up, palette::BUTTON);
            lay.fill_rect(down, palette::BUTTON);
            lay.ctext_in(up, 1, palette::TEXT, "^");
            lay.ctext_in(down, 1, palette::TEXT, "v");
            lay.region(hits, RecipeWidgetId::GridScrollUp, up);
            lay.region(hits, RecipeWidgetId::GridScrollDown, down);
        }
    }

    /// Recipes/uses detail list inside the Items tab.
    fn render_detail(&self, lay: &mut Lay, hits: &mut Vec<RecipeHitRegion>, main: UiRect) {
        let lang = self.lang;
        lay.fill_rect(main, DETAIL_BG);
        lay.border_rect(main, palette::PANEL_BORDER);
        let Some((item, mode)) = self.detail.clone() else {
            return;
        };
        // Breadcrumb header: [< Back]  "Recipes for Iron Ingot"
        let back_label = tr(lang, "< Back", "< 戻る");
        let back_w = crate::draw::text_width(back_label, 1) as i32 + 12;
        let back = UiRect::new(main.x + 4, main.y + 4, back_w as u32, 14);
        lay.fill_rect(
            back,
            if self.hover == Some(RecipeWidgetId::DetailBack) {
                palette::BUTTON_HOVER
            } else {
                palette::BUTTON
            },
        );
        lay.border_rect(back, palette::BUTTON_BORDER);
        lay.ctext_in(back, 1, palette::TEXT, back_label);
        lay.region(hits, RecipeWidgetId::DetailBack, back);
        let header = format!("{} {}", mode.label(lang), self.index.display_name(&item));
        lay.text_fit(
            back.right() + 8,
            main.y + 7,
            1,
            palette::TEXT,
            &header,
            ((main.w as i32 - back_w - 20) / 8).max(1) as usize,
        );

        let recipes = self.detail_recipes();
        let list_top = main.y + 22;
        let list_bottom = main.bottom() - 4;
        let rows = ((list_bottom - list_top) / RECIPE_ROW_PITCH).max(0) as usize;
        let scrollable = recipes.len() > rows;
        let row_w = main.w.saturating_sub(if scrollable { 18 } else { 4 });
        let scroll = self.detail_scroll.min(recipes.len().saturating_sub(rows.max(1)));

        if recipes.is_empty() {
            lay.text_fit(
                main.x + 6,
                list_top + 4,
                1,
                palette::TEXT_DIM,
                tr(lang, "No recipes found", "レシピが見つかりません"),
                ((main.w as i32 - 12) / 8).max(1) as usize,
            );
        }
        for (ri, recipe) in recipes.iter().enumerate().skip(scroll).take(rows) {
            let y = list_top + (ri - scroll) as i32 * RECIPE_ROW_PITCH;
            let row = UiRect::new(main.x + 2, y, row_w, RECIPE_ROW_H as u32);
            let sel = ri == self.recipe_selected;
            let hovered = self.hover == Some(RecipeWidgetId::RecipeRow(ri));
            lay.fill_rect(
                row,
                if sel {
                    palette::BUTTON_HOVER
                } else if hovered {
                    palette::BUTTON
                } else {
                    palette::SLOT
                },
            );
            // Transfer "+" button occupies the right column; the row hit
            // region stops before it so regions never overlap. The
            // button is craftability-colored like recipe browser: green = fully
            // craftable now, amber = some inputs on hand, dim = missing.
            let plus = UiRect::new(row.right() - 20, y + 4, 16, 16);
            let max_craft = self.index.max_craft(recipe, &self.inventory);
            let partial = max_craft == 0
                && recipe.inputs.iter().any(|ing| {
                    self.inventory
                        .keys()
                        .any(|k| self.index.ingredient_accepts(ing, k))
                });
            lay.fill_rect(
                plus,
                if self.hover == Some(RecipeWidgetId::TransferRecipe(ri)) {
                    palette::ACCENT
                } else if max_craft > 0 {
                    UiColor::rgb(70, 150, 70)
                } else if partial {
                    UiColor::rgb(170, 130, 50)
                } else {
                    palette::BUTTON
                },
            );
            lay.border_rect(plus, palette::BUTTON_BORDER);
            lay.ctext_in(plus, 1, palette::TEXT, "+");
            lay.region(hits, RecipeWidgetId::TransferRecipe(ri), plus);
            // Row-select hit strip: the title band only — the diagram's
            // `DetailSlot` regions own the body (nested regions are
            // forbidden by the layout audit), `+` owns its column.
            let row_hit = UiRect::new(row.x, row.y, row.w.saturating_sub(22), 13);
            lay.region(hits, RecipeWidgetId::RecipeRow(ri), row_hit);

            // Line 1: category title + dim recipe id.
            let line1 = format!("{} · {}", recipe.category.title(lang), recipe.id);
            lay.text_fit(
                row.x + 5,
                y + 3,
                1,
                palette::TEXT_DIM,
                &line1,
                ((row.w as i32 - 32) / 8).max(1) as usize,
            );
            // Body: per-category visual slot diagram (real item icons),
            // then a machine-metadata line under it.
            let area = UiRect::new(
                row.x + 4,
                y + 13,
                (row.w as i32 - 34).max(0) as u32,
                (RECIPE_ROW_H - 27).max(0) as u32,
            );
            self.render_recipe_diagram(lay, hits, recipe, area);
            let mut meta: Vec<String> = Vec::new();
            if recipe.heat != HeatRequirement::None {
                meta.push(recipe.heat.label(lang).to_owned());
            }
            if let Some(rpm) = recipe.rpm_min {
                meta.push(format!(">= {rpm} RPM"));
            }
            if let Some(su) = recipe.su_cost {
                meta.push(format!("{su} SU"));
            }
            if recipe.loops > 1 {
                meta.push(format!("x{} {}", recipe.loops, tr(lang, "loops", "ループ")));
            }
            if !recipe.sequence_steps.is_empty() {
                meta.push(format!("{} steps", recipe.sequence_steps.len()));
            }
            if recipe.processing_ticks > 0 {
                meta.push(format!("{}t", recipe.processing_ticks));
            }
            for f in &recipe.fluid_inputs {
                meta.push(format!("{} mB {}", f.amount_mb, f.fluid));
            }
            if !meta.is_empty() {
                lay.text_fit(
                    row.x + 5,
                    y + RECIPE_ROW_H - 11,
                    1,
                    palette::AIR,
                    &meta.join(" · "),
                    ((row.w as i32 - 32) / 8).max(1) as usize,
                );
            }
        }

        if scrollable {
            let sb_x = main.right() - 12;
            let up = UiRect::new(sb_x, list_top, 10, 12);
            let down = UiRect::new(sb_x, list_bottom - 12, 10, 12);
            lay.fill_rect(up, palette::BUTTON);
            lay.fill_rect(down, palette::BUTTON);
            lay.ctext_in(up, 1, palette::TEXT, "^");
            lay.ctext_in(down, 1, palette::TEXT, "v");
            lay.region(hits, RecipeWidgetId::DetailScrollUp, up);
            lay.region(hits, RecipeWidgetId::DetailScrollDown, down);
        }
    }

    /// Per-category visual recipe diagram drawn with real item slots
    /// (18px wells + `ItemIcon` glyphs). `area` is the row body under the
    /// title line; every slot registers a `DetailSlot` hit region whose
    /// index follows `inputs ++ outputs ++ chanced_outputs` order —
    /// [`detail_slot_item`](Self::detail_slot_item) decodes it.
    fn render_recipe_diagram(
        &self,
        lay: &mut Lay,
        hits: &mut Vec<RecipeHitRegion>,
        recipe: &Recipe,
        area: UiRect,
    ) {
        match recipe.category {
            RecipeCategory::CraftingShaped
            | RecipeCategory::CraftingShapeless
            | RecipeCategory::CreateMechanicalCrafting => {
                self.diagram_crafting(lay, hits, recipe, area)
            }
            RecipeCategory::Smelting
            | RecipeCategory::Blasting
            | RecipeCategory::Smoking
            | RecipeCategory::Campfire => self.diagram_furnace(lay, hits, recipe, area),
            RecipeCategory::Smithing | RecipeCategory::Anvil => {
                self.diagram_inline(lay, hits, recipe, area)
            }
            RecipeCategory::Brewing => self.diagram_brewing(lay, hits, recipe, area),
            RecipeCategory::Stonecutting => self.diagram_inline(lay, hits, recipe, area),
            RecipeCategory::CreateSequencedAssembly => {
                self.diagram_sequenced(lay, hits, recipe, area)
            }
            _ => self.diagram_machine(lay, hits, recipe, area),
        }
    }

    /// 18px ingredient slot (tag slots cycle and get an amber frame);
    /// `idx` is the input ordinal for `DetailSlot`.
    fn diagram_ing(
        &self,
        lay: &mut Lay,
        hits: &mut Vec<RecipeHitRegion>,
        x: i32,
        y: i32,
        ing: &RecipeIngredient,
        idx: usize,
    ) {
        let id = self.index.resolve_ingredient_cycled(ing, self.clock_ticks);
        lay.slot(
            hits,
            x,
            y,
            &id,
            u64::from(ing.count()),
            Some(RecipeWidgetId::DetailSlot(idx)),
        );
        if matches!(ing, RecipeIngredient::Tag { .. }) {
            lay.border(x, y, 18, 18, palette::WARNING);
        }
    }

    /// 18px output slot; `idx` is the global `DetailSlot` ordinal.
    #[allow(clippy::too_many_arguments)]
    fn diagram_out(
        &self,
        lay: &mut Lay,
        hits: &mut Vec<RecipeHitRegion>,
        x: i32,
        y: i32,
        item: &str,
        count: u32,
        idx: usize,
    ) {
        lay.slot(
            hits,
            x,
            y,
            item,
            u64::from(count),
            Some(RecipeWidgetId::DetailSlot(idx)),
        );
    }

    /// `->` arrow + its landing column; returns the output x.
    fn diagram_arrow(&self, lay: &mut Lay, x: i32, y_mid: i32) -> i32 {
        lay.text_fit(x, y_mid - 4, 1, palette::TEXT_DIM, "->", 2);
        x + 16
    }

    /// 3×3 crafting matrix (shaped rows/cols preserved via `recipe.grid`)
    /// → arrow → output; a "Shapeless" badge marks unordered recipes.
    fn diagram_crafting(
        &self,
        lay: &mut Lay,
        hits: &mut Vec<RecipeHitRegion>,
        recipe: &Recipe,
        area: UiRect,
    ) {
        for cell in 0..9usize {
            let (row, col) = (cell / 3, cell % 3);
            let (x, y) = (area.x + col as i32 * 20, area.y + row as i32 * 19);
            let input_idx = recipe
                .grid
                .as_ref()
                .and_then(|g| g.get(cell))
                .copied()
                .flatten()
                .or({
                    // Shapeless: pack inputs left-to-right, top-to-bottom.
                    if recipe.grid.is_none() && cell < recipe.inputs.len() {
                        Some(cell)
                    } else {
                        None
                    }
                });
            match input_idx.and_then(|i| recipe.inputs.get(i).map(|ing| (i, ing))) {
                Some((i, ing)) => self.diagram_ing(lay, hits, x, y, ing, i),
                None => {
                    let r = lay.rect(x, y, 18, 18);
                    lay.fill_rect(r, UiColor::rgba(12, 12, 16, 140));
                    lay.border_rect(r, UiColor::rgba(96, 96, 108, 120));
                }
            }
        }
        let mid_y = area.y + 27;
        let ox = self.diagram_arrow(lay, area.x + 62, mid_y);
        let out_idx = recipe.inputs.len();
        if let Some(out) = recipe.outputs.first() {
            self.diagram_out(lay, hits, ox, area.y + 18, &out.item, out.count, out_idx);
        }
        if recipe.category == RecipeCategory::CraftingShapeless {
            lay.text_fit(
                ox + 22,
                area.y + 23,
                1,
                palette::WARNING,
                tr(self.lang, "Shapeless", "不定形"),
                8,
            );
        }
    }

    /// Furnace family: input above a flame glyph + cook seconds → output.
    fn diagram_furnace(
        &self,
        lay: &mut Lay,
        hits: &mut Vec<RecipeHitRegion>,
        recipe: &Recipe,
        area: UiRect,
    ) {
        if let Some(ing) = recipe.inputs.first() {
            self.diagram_ing(lay, hits, area.x, area.y, ing, 0);
        }
        // Flame under the input (recipe-browser-style indicator).
        let fx = area.x + 3;
        let fy = area.y + 24;
        lay.fill(fx, fy + 4, 12, 6, UiColor::rgb(196, 92, 32));
        lay.fill(fx + 2, fy + 1, 8, 4, UiColor::rgb(230, 150, 40));
        lay.fill(fx + 4, fy + 5, 4, 4, UiColor::rgb(250, 210, 90));
        if recipe.processing_ticks > 0 {
            lay.text_fit(
                area.x + 22,
                fy + 3,
                1,
                palette::TEXT_DIM,
                &format!("{:.1}s", recipe.processing_ticks as f32 / 20.0),
                6,
            );
        }
        let mid_y = area.y + 10;
        let ox = self.diagram_arrow(lay, area.x + 24, mid_y);
        let out_idx = recipe.inputs.len();
        if let Some(out) = recipe.outputs.first() {
            self.diagram_out(lay, hits, ox, area.y + 1, &out.item, out.count, out_idx);
        }
    }

    /// Smithing/anvil/stonecutting: inputs in a row → arrow → outputs.
    fn diagram_inline(
        &self,
        lay: &mut Lay,
        hits: &mut Vec<RecipeHitRegion>,
        recipe: &Recipe,
        area: UiRect,
    ) {
        let mut cx = area.x;
        let y = area.y + 4;
        for (i, ing) in recipe.inputs.iter().enumerate().take(6) {
            self.diagram_ing(lay, hits, cx, y, ing, i);
            cx += 22;
        }
        let ox = self.diagram_arrow(lay, cx + 2, y + 9);
        let mut ox = ox;
        let base = recipe.inputs.len();
        for (j, out) in recipe.outputs.iter().enumerate() {
            if ox + 18 > area.right() {
                break;
            }
            self.diagram_out(lay, hits, ox, y, &out.item, out.count, base + j);
            ox += 22;
        }
        for (k, (id, count, chance)) in recipe.chanced_outputs.iter().enumerate() {
            if ox + 18 > area.right() {
                break;
            }
            self.diagram_out(
                lay,
                hits,
                ox,
                y,
                id,
                *count,
                base + recipe.outputs.len() + k,
            );
            lay.text_fit(
                ox + 2,
                y + 19,
                1,
                palette::WARNING,
                &format!("{}%", (chance * 100.0).round() as u32),
                4,
            );
            ox += 22;
        }
    }

    /// Brewing stand: ingredient on top, three bottle wells below,
    /// arrow → result slot.
    fn diagram_brewing(
        &self,
        lay: &mut Lay,
        hits: &mut Vec<RecipeHitRegion>,
        recipe: &Recipe,
        area: UiRect,
    ) {
        if let Some(ing) = recipe.inputs.first() {
            self.diagram_ing(lay, hits, area.x + 20, area.y, ing, 0);
        }
        // Bottle wells: filled with a follow-up input when the recipe
        // declares one (reagent -> 3 potions is the common shape).
        for b in 0..3usize {
            let x = area.x + b as i32 * 20;
            let y = area.y + 24;
            match recipe.inputs.get(1 + b) {
                Some(ing) => self.diagram_ing(lay, hits, x, y, ing, 1 + b),
                None => {
                    let r = lay.rect(x, y, 18, 18);
                    lay.fill_rect(r, UiColor::rgba(12, 12, 16, 140));
                    lay.border_rect(r, UiColor::rgba(96, 96, 108, 120));
                }
            }
        }
        let ox = self.diagram_arrow(lay, area.x + 64, area.y + 22);
        let out_idx = recipe.inputs.len();
        if let Some(out) = recipe.outputs.first() {
            self.diagram_out(lay, hits, ox, area.y + 14, &out.item, out.count, out_idx);
        }
    }

    /// Sequenced assembly: start item → per-step operation slots →
    /// result, with the transitional item carried between steps.
    fn diagram_sequenced(
        &self,
        lay: &mut Lay,
        hits: &mut Vec<RecipeHitRegion>,
        recipe: &Recipe,
        area: UiRect,
    ) {
        let mut cx = area.x;
        let y = area.y + 2;
        if let Some(ing) = recipe.inputs.first() {
            self.diagram_ing(lay, hits, cx, y, ing, 0);
        }
        cx += 22;
        // Loop strip: each step shows its extra consumed inputs under a
        // small op label.
        for (si, step) in recipe.sequence_steps.iter().take(4).enumerate() {
            if cx + 30 > area.right() - 60 {
                break;
            }
            lay.text_fit(cx, y - 9, 1, palette::AIR, step.category.search_id(), 6);
            for (ii, ing) in step.inputs.iter().take(2).enumerate() {
                let id = self
                    .index
                    .resolve_ingredient_cycled(ing, self.clock_ticks + si as u64);
                lay.slot(
                    hits,
                    cx + ii as i32 * 20,
                    y,
                    &id,
                    u64::from(ing.count()),
                    None,
                );
            }
            cx += 46;
            lay.text_fit(cx - 12, y + 5, 1, palette::TEXT_DIM, ">", 1);
        }
        if recipe.loops > 1 {
            lay.text_fit(
                cx,
                y + 5,
                1,
                palette::TEXT_DIM,
                &format!("x{}", recipe.loops),
                4,
            );
            cx += 24;
        }
        let ox = self.diagram_arrow(lay, cx, y + 9);
        let base = recipe.inputs.len();
        for (j, out) in recipe.outputs.iter().enumerate() {
            if ox + 18 > area.right() {
                break;
            }
            self.diagram_out(
                lay,
                hits,
                ox + j as i32 * 22,
                y,
                &out.item,
                out.count,
                base + j,
            );
        }
    }

    /// Create-family machine and custom categories: machine title chip,
    /// heat badge, input row → arrow → outputs with `%` chance badges.
    fn diagram_machine(
        &self,
        lay: &mut Lay,
        hits: &mut Vec<RecipeHitRegion>,
        recipe: &Recipe,
        area: UiRect,
    ) {
        let y = area.y + 4;
        let mut cx = area.x;
        // Heat requirement badge right under the title area.
        if recipe.heat != HeatRequirement::None {
            let label = recipe.heat.label(self.lang);
            let w = crate::draw::text_width(label, 1) as i32 + 8;
            let badge = UiRect::new(cx, y - 3, w as u32, 12);
            lay.fill_rect(badge, UiColor::rgba(120, 60, 20, 200));
            lay.border_rect(badge, UiColor::rgb(200, 120, 60));
            lay.ctext_in(badge, 1, UiColor::rgb(255, 200, 140), label);
            cx += w + 6;
        }
        for f in &recipe.fluid_inputs {
            if cx + 30 > area.right() - 40 {
                break;
            }
            let tank = UiRect::new(cx, y, 14, 18);
            lay.fill_rect(tank, UiColor::rgba(30, 60, 120, 200));
            lay.border_rect(tank, palette::BUTTON_BORDER);
            cx += 18;
            lay.text_fit(cx, y + 5, 1, palette::AIR, &format!("{}", f.amount_mb), 4);
            cx += 26;
            let _ = f;
        }
        let mut shown = 0usize;
        for (i, ing) in recipe.inputs.iter().enumerate() {
            if cx + 20 > area.right() - 44 {
                shown += 1;
                continue;
            }
            self.diagram_ing(lay, hits, cx, y, ing, i);
            cx += 22;
        }
        if shown > 0 {
            lay.text_fit(cx, y + 5, 1, palette::TEXT_DIM, &format!("+{shown}"), 4);
            cx += 20;
        }
        let ox = self.diagram_arrow(lay, cx + 2, y + 9);
        let mut ox = ox;
        let base = recipe.inputs.len();
        for (j, out) in recipe.outputs.iter().enumerate() {
            if ox + 18 > area.right() {
                break;
            }
            self.diagram_out(lay, hits, ox, y, &out.item, out.count, base + j);
            ox += 22;
        }
        for (k, (id, count, chance)) in recipe.chanced_outputs.iter().enumerate() {
            if ox + 18 > area.right() {
                break;
            }
            self.diagram_out(
                lay,
                hits,
                ox,
                y,
                id,
                *count,
                base + recipe.outputs.len() + k,
            );
            lay.text_fit(
                ox + 2,
                y + 19,
                1,
                palette::WARNING,
                &format!("{}%", (chance * 100.0).round() as u32),
                4,
            );
            ox += 22;
        }
    }

    /// Craft-tree view: indented node list left, totals right.
    fn render_tree(&self, lay: &mut Lay, hits: &mut Vec<RecipeHitRegion>, main: UiRect) {
        let lang = self.lang;
        lay.fill_rect(main, DETAIL_BG);
        lay.border_rect(main, palette::PANEL_BORDER);
        let Some(tree) = &self.tree else {
            lay.text_fit(
                main.x + 6,
                main.y + 8,
                1,
                palette::TEXT_DIM,
                tr(
                    lang,
                    "Select an item and press T to build a recipe tree",
                    "アイテムを選んでTでツリー表示",
                ),
                ((main.w as i32 - 12) / 8).max(1) as usize,
            );
            return;
        };

        // Header: target + count controls + pin.
        let header = format!(
            "{}: {} x {}",
            tr(lang, "Craft", "作成"),
            tree.root_count,
            self.index.display_name(&tree.root_item)
        );
        let pin_label = tr(lang, "Pin", "ピン");
        let pin_w = crate::draw::text_width(pin_label, 1) as i32 + 12;
        // Right-side chrome (pin + count controls) hides entirely when
        // the pane is too narrow to host it inside the border.
        let show_pin = main.w as i32 >= pin_w + 14;
        let show_counts = main.w as i32 >= pin_w + 58;
        let chrome_w = match (show_pin, show_counts) {
            (true, true) => pin_w + 6 + 44,
            (true, false) => pin_w + 6,
            _ => 0,
        };
        lay.text_fit(
            main.x + 6,
            main.y + 5,
            1,
            palette::TEXT,
            &header,
            ((main.w as i32 - 12 - chrome_w) / 8).max(1) as usize,
        );
        let pin = UiRect::new(main.right() - 6 - pin_w, main.y + 3, pin_w as u32, 13);
        if show_pin {
            let pinned = self
                .pinned_trees
                .iter()
                .any(|(i, c)| *i == tree.root_item && *c == tree.root_count);
            lay.fill_rect(
                pin,
                if pinned {
                    palette::ACCENT
                } else if self.hover == Some(RecipeWidgetId::TreePin) {
                    palette::BUTTON_HOVER
                } else {
                    palette::BUTTON
                },
            );
            lay.border_rect(pin, palette::BUTTON_BORDER);
            lay.ctext_in(
                pin,
                1,
                if pinned { palette::TEXT_DARK } else { palette::TEXT },
                pin_label,
            );
            lay.region(hits, RecipeWidgetId::TreePin, pin);
        }
        if show_counts {
            for (id, label) in [
                (RecipeWidgetId::TreeCountDown, "-"),
                (RecipeWidgetId::TreeCountUp, "+"),
            ] {
                let w = 16;
                let x =
                    pin.x - 8 - w - if id == RecipeWidgetId::TreeCountDown { 20 } else { 0 };
                let rect = UiRect::new(x, main.y + 3, w as u32, 13);
                lay.fill_rect(
                    rect,
                    if self.hover == Some(id) {
                        palette::BUTTON_HOVER
                    } else {
                        palette::BUTTON
                    },
                );
                lay.border_rect(rect, palette::BUTTON_BORDER);
                lay.ctext_in(rect, 1, palette::TEXT, label);
                lay.region(hits, id, rect);
            }
        }

        // Split: node list | totals. The totals column yields first on
        // narrow panes so the node list keeps a usable width.
        let totals_w = (main.w as i32 / 3)
            .clamp(120, 200)
            .min((main.w as i32 - 52).max(24));
        let list_w = (main.w as i32 - totals_w - 8).max(8);
        let list_top = main.y + 20;
        let list_bottom = main.bottom() - 4;
        let rows = ((list_bottom - list_top) / TREE_ROW_PITCH).max(0) as usize;

        let Some(root) = &tree.root else {
            return;
        };
        let flat = root.flatten();
        let scrollable = flat.len() > rows;
        let row_w = list_w - if scrollable { 16 } else { 0 };
        let scroll = self.tree_scroll.min(flat.len().saturating_sub(rows.max(1)));
        for (fi, (depth, node)) in flat.iter().enumerate().skip(scroll).take(rows) {
            let y = list_top + (fi - scroll) as i32 * TREE_ROW_PITCH;
            let rect = UiRect::new(main.x + 2, y, row_w.max(8) as u32, (TREE_ROW_PITCH - 1) as u32);
            if node.recipe_id.is_some() {
                lay.region(hits, RecipeWidgetId::TreeNodeRow(fi), rect);
            }
            // Keep the indent inside the row: a deep tree on a narrow
            // pane clamps at the row edge instead of going negative.
            let indent = (*depth as i32 * 8).min((list_w - 40).max(0));
            let marker = if node.is_cycle {
                tr(lang, " [loop]", " [循環]")
            } else if node.is_truncated {
                " [...]"
            } else {
                ""
            };
            let line = if node.recipe_id.is_some() {
                format!(
                    "{}x {} ({}){}",
                    node.needed,
                    self.index.display_name(&node.item),
                    node.recipe_id.as_deref().unwrap_or(""),
                    marker
                )
            } else {
                format!(
                    "{}x {}{}",
                    node.needed,
                    self.index.display_name(&node.item),
                    marker
                )
            };
            lay.text_fit(
                rect.x + 4 + indent,
                rect.y + 2,
                1,
                if node.recipe_id.is_some() {
                    palette::TEXT
                } else {
                    palette::ACCENT
                },
                &line,
                ((list_w - indent - 8) / 8).max(1) as usize,
            );
        }
        if scrollable {
            let sb_x = main.x + 2 + list_w - 12;
            let up = UiRect::new(sb_x, list_top, 10, 12);
            let down = UiRect::new(sb_x, list_bottom - 12, 10, 12);
            lay.fill_rect(up, palette::BUTTON);
            lay.fill_rect(down, palette::BUTTON);
            lay.ctext_in(up, 1, palette::TEXT, "^");
            lay.ctext_in(down, 1, palette::TEXT, "v");
            lay.region(hits, RecipeWidgetId::TreeScrollUp, up);
            lay.region(hits, RecipeWidgetId::TreeScrollDown, down);
        }

        // Totals column.
        let tx = main.x + 4 + list_w + 4;
        let tw = (main.right() - tx - 4).max(8);
        let cells = ((tw - 8) / 8).max(1) as usize;
        let mut ty = list_top;
        lay.text_fit(tx, ty, 1, palette::WARNING, tr(lang, "Raw materials", "素材"), cells);
        ty += 10;
        for (id, n) in tree.raw_materials.iter().take(8) {
            let line = format!("{}x {}", n, self.index.display_name(id));
            lay.text_fit(tx + 4, ty, 1, palette::TEXT, &line, cells - 1);
            ty += 10;
        }
        if tree.raw_materials.len() > 8 {
            lay.text_fit(tx + 4, ty, 1, palette::TEXT_DIM, "+more", cells - 1);
            ty += 10;
        }
        ty += 4;
        if !tree.raw_fluids.is_empty() {
            lay.text_fit(tx, ty, 1, palette::WARNING, tr(lang, "Fluids (mB)", "流体 (mB)"), cells);
            ty += 10;
            for (id, n) in tree.raw_fluids.iter().take(4) {
                lay.text_fit(tx + 4, ty, 1, palette::AIR, &format!("{n} mB {id}"), cells - 1);
                ty += 10;
            }
            ty += 4;
        }
        if !tree.byproducts.is_empty() {
            lay.text_fit(tx, ty, 1, palette::WARNING, tr(lang, "Byproducts", "副産物"), cells);
            ty += 10;
            for (id, n) in tree.byproducts.iter().take(4) {
                let line = format!("{}x {}", n, self.index.display_name(id));
                lay.text_fit(tx + 4, ty, 1, palette::TEXT_DIM, &line, cells - 1);
                ty += 10;
            }
            ty += 4;
        }
        if !self.inventory.is_empty() {
            lay.text_fit(tx, ty, 1, palette::WARNING, tr(lang, "Still needed", "不足分"), cells);
            ty += 10;
            for (id, n) in tree.remaining_needed(&self.inventory).iter().take(6) {
                let line = format!("{}x {}", n, self.index.display_name(id));
                lay.text_fit(tx + 4, ty, 1, palette::DANGER, &line, cells - 1);
                ty += 10;
            }
        }
    }

    /// World resource tabs: mob drops, ore histograms, trades, loot, enchants.
    fn render_world_drops_info(&self, lay: &mut Lay, hits: &mut Vec<RecipeHitRegion>, main: UiRect) {
        let lang = self.lang;
        lay.fill_rect(main, DETAIL_BG);
        lay.border_rect(main, palette::PANEL_BORDER);
        // Sub-tab pills.
        let subs = [
            (WorldDropsTab::MobDrops, RecipeWidgetId::WorldDropsTabMobs),
            (WorldDropsTab::OreGen, RecipeWidgetId::WorldDropsTabOres),
            (WorldDropsTab::Trades, RecipeWidgetId::WorldDropsTabTrades),
            (WorldDropsTab::Loot, RecipeWidgetId::WorldDropsTabLoot),
            (WorldDropsTab::Enchants, RecipeWidgetId::WorldDropsTabEnchants),
            (WorldDropsTab::Fuels, RecipeWidgetId::WorldDropsTabFuels),
        ];
        let mut x = main.x + 4;
        for (tab, id) in subs {
            let label = tab.label(lang);
            let w = crate::draw::text_width(label, 1) as i32 + 10;
            if x + w > main.right() - 4 {
                break;
            }
            let rect = UiRect::new(x, main.y + 4, w as u32, 13);
            let active = self.world_drops_tab == tab;
            lay.fill_rect(
                rect,
                if active {
                    palette::ACCENT
                } else if self.hover == Some(id) {
                    palette::BUTTON_HOVER
                } else {
                    palette::BUTTON
                },
            );
            lay.border_rect(rect, palette::BUTTON_BORDER);
            lay.ctext_in(
                rect,
                1,
                if active { palette::TEXT_DARK } else { palette::TEXT },
                label,
            );
            lay.region(hits, id, rect);
            x += w + 3;
        }

        let list_top = main.y + 22;
        let list_bottom = main.bottom() - 4;
        let cells = ((main.w as i32 - 30) / 8).max(1) as usize;
        match self.world_drops_tab {
            WorldDropsTab::MobDrops => {
                let rows = ((list_bottom - list_top) / 12).max(0) as usize;
                let scroll = self.world_drops_scroll.min(self.index.mob_drops.len().saturating_sub(rows));
                for (ri, e) in self.index.mob_drops.iter().enumerate().skip(scroll).take(rows) {
                    let y = list_top + (ri - scroll) as i32 * 12;
                    let cond = if e.condition.is_empty() {
                        String::new()
                    } else {
                        format!(" [{}]", e.condition)
                    };
                    let line = format!(
                        "{} -> {} x{}-{} ({}%){}{}",
                        e.mob_id,
                        self.index.display_name(&e.item_id),
                        e.min_count,
                        e.max_count,
                        (e.chance * 100.0).round() as u32,
                        if e.looting_bonus > 0 {
                            format!(" +{}/L", e.looting_bonus)
                        } else {
                            String::new()
                        },
                        cond
                    );
                    lay.fill(main.x + 4, y + 1, 10, 10, item_color(&e.item_id));
                    lay.text_fit(main.x + 18, y + 2, 1, palette::TEXT, &line, cells);
                }
            }
            WorldDropsTab::OreGen => {
                let row_h = 44;
                let rows = ((list_bottom - list_top) / row_h).max(0) as usize;
                let scroll = self
                    .world_drops_scroll
                    .min(self.index.ore_distributions.len().saturating_sub(rows));
                for (ri, e) in self
                    .index
                    .ore_distributions
                    .iter()
                    .enumerate()
                    .skip(scroll)
                    .take(rows)
                {
                    let y = list_top + (ri - scroll) as i32 * row_h;
                    let head = format!(
                        "{} · {} · Y {}..{} peak {}",
                        self.index.display_name(&e.block_id),
                        e.dimension,
                        e.min_y,
                        e.max_y,
                        e.peak_y
                    );
                    lay.fill(main.x + 4, y, 10, 10, item_color(&e.block_id));
                    lay.text_fit(main.x + 18, y + 1, 1, palette::TEXT, &head, cells);
                    // Per-Y histogram bar chart.
                    let hist = e.histogram();
                    let chart = UiRect::new(
                        main.x + 18,
                        y + 14,
                        (main.w as i32 - 30).max(16) as u32,
                        26,
                    );
                    lay.fill_rect(chart, palette::SLOT);
                    lay.border_rect(chart, palette::BUTTON_BORDER);
                    let max_w = hist.iter().copied().max().unwrap_or(1).max(1) as f32;
                    let n = hist.len().max(1) as i32;
                    let inner_w = chart.w as i32 - 4;
                    for (i, w) in hist.iter().enumerate() {
                        let bx0 = chart.x + 2 + (i as i32 * inner_w) / n;
                        let bx1 = chart.x + 2 + ((i as i32 + 1) * inner_w) / n;
                        let bh = ((*w as f32 / max_w) * 22.0) as i32;
                        if bh <= 0 {
                            continue;
                        }
                        let color = if e.min_y + i as i32 == e.peak_y {
                            palette::ACCENT
                        } else {
                            palette::AIR
                        };
                        lay.fill(
                            bx0,
                            chart.bottom() - 2 - bh,
                            (bx1 - bx0 - 1).max(1),
                            bh,
                            color,
                        );
                    }
                }
            }
            WorldDropsTab::Trades => {
                let rows = ((list_bottom - list_top) / 12).max(0) as usize;
                let scroll = self
                    .world_drops_scroll
                    .min(self.index.villager_trades.len().saturating_sub(rows));
                for (ri, e) in self
                    .index
                    .villager_trades
                    .iter()
                    .enumerate()
                    .skip(scroll)
                    .take(rows)
                {
                    let y = list_top + (ri - scroll) as i32 * 12;
                    let buy_b = e
                        .buy_b
                        .as_ref()
                        .map(|(i, c)| format!(" + {}x {}", c, self.index.display_name(i)))
                        .unwrap_or_default();
                    let line = format!(
                        "{} L{}: {}x {}{} -> {}x {} ({} uses)",
                        e.profession,
                        e.level,
                        e.buy_a.1,
                        self.index.display_name(&e.buy_a.0),
                        buy_b,
                        e.sell.1,
                        self.index.display_name(&e.sell.0),
                        e.max_uses
                    );
                    lay.fill(main.x + 4, y + 1, 10, 10, item_color(&e.sell.0));
                    lay.text_fit(main.x + 18, y + 2, 1, palette::TEXT, &line, cells);
                }
            }
            WorldDropsTab::Loot => {
                let rows = ((list_bottom - list_top) / 12).max(0) as usize;
                let scroll = self
                    .world_drops_scroll
                    .min(self.index.dungeon_loot.len().saturating_sub(rows));
                for (ri, e) in self.index.dungeon_loot.iter().enumerate().skip(scroll).take(rows) {
                    let y = list_top + (ri - scroll) as i32 * 12;
                    let line = format!(
                        "{}: {} x{}-{} w{} ({}%)",
                        e.chest,
                        self.index.display_name(&e.item_id),
                        e.min_count,
                        e.max_count,
                        e.weight,
                        (e.chance * 100.0).round() as u32
                    );
                    lay.fill(main.x + 4, y + 1, 10, 10, item_color(&e.item_id));
                    lay.text_fit(main.x + 18, y + 2, 1, palette::TEXT, &line, cells);
                }
            }
            WorldDropsTab::Enchants => {
                let rows = ((list_bottom - list_top) / 12).max(0) as usize;
                let scroll = self
                    .world_drops_scroll
                    .min(self.index.enchant_compat.len().saturating_sub(rows));
                for (ri, e) in self.index.enchant_compat.iter().enumerate().skip(scroll).take(rows) {
                    let y = list_top + (ri - scroll) as i32 * 12;
                    let enchs = e
                        .enchantments
                        .iter()
                        .map(|(id, lvl)| {
                            format!("{} {}", id.split(':').nth(1).unwrap_or(id), roman(*lvl))
                        })
                        .collect::<Vec<_>>()
                        .join(", ");
                    let line = format!("{}: {}", self.index.display_name(&e.item_id), enchs);
                    lay.fill(main.x + 4, y + 1, 10, 10, item_color(&e.item_id));
                    lay.text_fit(main.x + 18, y + 2, 1, palette::TEXT, &line, cells);
                }
            }
            WorldDropsTab::Fuels => {
                // Furnace fuels: icon + name + burn ticks + a live
                // burn bar proportional to the hottest fuel in the list.
                let rows = ((list_bottom - list_top) / 14).max(0) as usize;
                let scroll = self
                    .world_drops_scroll
                    .min(self.index.fuels.len().saturating_sub(rows));
                let max_burn = self
                    .index
                    .fuels
                    .iter()
                    .map(|f| f.burn_ticks)
                    .max()
                    .unwrap_or(1)
                    .max(1) as f32;
                if self.index.fuels.is_empty() {
                    lay.text_fit(
                        main.x + 6,
                        list_top + 4,
                        1,
                        palette::TEXT_DIM,
                        tr(lang, "No fuels registered", "燃料データなし"),
                        cells,
                    );
                }
                for (ri, f) in self.index.fuels.iter().enumerate().skip(scroll).take(rows) {
                    let y = list_top + (ri - scroll) as i32 * 14;
                    let shown = if let Some(tag) = f.item.strip_prefix('#') {
                        format!("#{tag}")
                    } else {
                        self.index.resolve_ingredient_cycled(
                            &RecipeIngredient::item(&f.item, 1),
                            self.clock_ticks,
                        )
                    };
                    let name = self
                        .index
                        .display_name_for_locale(&shown, &self.active_locale);
                    let smelted = f.items_smelted();
                    let smelted_str = if (smelted.fract()).abs() < f32::EPSILON {
                        format!("{smelted:.0}")
                    } else {
                        format!("{smelted:.1}")
                    };
                    lay.fill(main.x + 4, y + 1, 10, 10, item_color(&shown));
                    let line = format!(
                        "{name} · {}t · {} {}",
                        f.burn_ticks,
                        smelted_str,
                        tr(lang, "items", "個分")
                    );
                    lay.text_fit(main.x + 18, y + 2, 1, palette::TEXT, &line, cells);
                    // Burn bar under the row, proportional to max burn.
                    let bar_w = main.w.saturating_sub(26);
                    let bw = ((f.burn_ticks as f32 / max_burn) * bar_w as f32) as i32;
                    lay.fill(
                        main.x + 18,
                        y + 12,
                        bw.max(1),
                        1,
                        UiColor::rgb(230, 150, 40),
                    );
                }
            }
        }
        // Shared scroll buttons.
        let count = match self.world_drops_tab {
            WorldDropsTab::MobDrops => self.index.mob_drops.len(),
            WorldDropsTab::OreGen => self.index.ore_distributions.len(),
            WorldDropsTab::Trades => self.index.villager_trades.len(),
            WorldDropsTab::Loot => self.index.dungeon_loot.len(),
            WorldDropsTab::Enchants => self.index.enchant_compat.len(),
            WorldDropsTab::Fuels => self.index.fuels.len(),
        };
        if count > 0 {
            let sb_x = main.right() - 12;
            let up = UiRect::new(sb_x, list_top, 10, 12);
            let down = UiRect::new(sb_x, list_bottom - 12, 10, 12);
            lay.fill_rect(up, palette::BUTTON);
            lay.fill_rect(down, palette::BUTTON);
            lay.ctext_in(up, 1, palette::TEXT, "^");
            lay.ctext_in(down, 1, palette::TEXT, "v");
            lay.region(hits, RecipeWidgetId::WorldDropsScrollUp, up);
            lay.region(hits, RecipeWidgetId::WorldDropsScrollDown, down);
        }
    }

    /// Machine Tutorials tab: 3D viewport + transport controls + caption.
    fn render_machine_tutorial(&self, lay: &mut Lay, hits: &mut Vec<RecipeHitRegion>, main: UiRect) {
        let lang = self.lang;
        lay.fill_rect(main, DETAIL_BG);
        lay.border_rect(main, palette::PANEL_BORDER);
        let Some(scene) = &self.machine_tutorial else {
            lay.text_fit(
                main.x + 6,
                main.y + 8,
                1,
                palette::TEXT_DIM,
                tr(lang, "No machine tutorial loaded", "機械チュートリアルなし"),
                ((main.w as i32 - 12) / 8).max(1) as usize,
            );
            return;
        };

        // Control row: [|<] [play/pause] [>|] [rot<] [rot>] step i/n.
        let controls = [
            (RecipeWidgetId::MachineTutorialPrev, "|<"),
            (
                RecipeWidgetId::MachineTutorialPlay,
                if scene.playing { "||" } else { "|>" },
            ),
            (RecipeWidgetId::MachineTutorialNext, ">|"),
            (RecipeWidgetId::MachineTutorialRotateLeft, "<-"),
            (RecipeWidgetId::MachineTutorialRotateRight, "->"),
        ];
        let mut x = main.x + 4;
        for (id, label) in controls {
            let w = 22;
            let rect = UiRect::new(x, main.y + 4, w as u32, 14);
            lay.fill_rect(
                rect,
                if self.hover == Some(id) {
                    palette::BUTTON_HOVER
                } else {
                    palette::BUTTON
                },
            );
            lay.border_rect(rect, palette::BUTTON_BORDER);
            lay.ctext_in(rect, 1, palette::TEXT, label);
            lay.region(hits, id, rect);
            x += w + 4;
        }
        let step_text = format!(
            "{} {}/{} · {}",
            tr(lang, "Step", "ステップ"),
            scene.step_index + 1,
            scene.steps.len(),
            scene.title
        );
        lay.text_fit(
            x + 4,
            main.y + 7,
            1,
            palette::TEXT_DIM,
            &step_text,
            ((main.right() - x - 8) / 8).max(1) as usize,
        );

        // Viewport + caption bar.
        let caption_h = 16;
        let vp = UiRect::new(
            main.x + 4,
            main.y + 22,
            (main.w as i32 - 8).max(8) as u32,
            (main.h as i32 - 22 - caption_h - 4).max(8) as u32,
        );
        lay.fill_rect(vp, palette::SLOT);
        lay.machine_tutorial(vp, scene);
        if let Some(step) = scene.current_step() {
            let bar = UiRect::new(vp.x, vp.bottom() + 2, vp.w, 14);
            lay.fill_rect(bar, palette::BUTTON_DISABLED);
            lay.text_fit(
                bar.x + 4,
                bar.y + 3,
                1,
                palette::TEXT,
                step.caption(lang),
                ((bar.w as i32 - 8) / 8).max(1) as usize,
            );
        }
    }

    fn render_footer(&self, lay: &mut Lay) {
        let lang = self.lang;
        let y = lay.h - 4 - 12;
        let item_hint = self
            .selected_item()
            .map(|id| self.index.display_name(&id))
            .unwrap_or_else(|| tr(lang, "none", "なし").to_owned());
        let hint = format!(
            "{}: {}   R {} · U {} · A {} · T {} · + {}",
            tr(lang, "Selected", "選択中"),
            fit_text(&item_hint, 20),
            tr(lang, "recipes", "レシピ"),
            tr(lang, "uses", "用途"),
            tr(lang, "bookmark", "登録"),
            tr(lang, "tree", "ツリー"),
            tr(lang, "fill", "転送"),
        );
        lay.text_fit(
            MARGIN + 4,
            y + 2,
            1,
            palette::TEXT_DIM,
            &hint,
            ((lay.w - 2 * MARGIN - 8) / 8).max(1) as usize,
        );
    }
}

/// Small Roman numeral for enchantment levels (`I..X`, else the number).
fn roman(level: u32) -> String {
    match level {
        1 => "I".to_owned(),
        2 => "II".to_owned(),
        3 => "III".to_owned(),
        4 => "IV".to_owned(),
        5 => "V".to_owned(),
        6 => "VI".to_owned(),
        7 => "VII".to_owned(),
        8 => "VIII".to_owned(),
        9 => "IX".to_owned(),
        10 => "X".to_owned(),
        other => other.to_string(),
    }
}

// ---------------------------------------------------------------------------
// Layout context (logical px → physical px at the effective GUI scale)
// ---------------------------------------------------------------------------

const MARGIN: i32 = 4;
const SEARCH_Y: i32 = 26;
const TABS_Y: i32 = 46;
const CONTENT_TOP: i32 = 64;
const FOOTER_H: i32 = 18;
const PANE_GAP: i32 = 6;
const BOOKMARK_W: i32 = 124;
const BOOKMARK_H: i32 = 14;
const BOOKMARK_PITCH: i32 = 15;
const SLOT: i32 = 18;
const SLOT_PITCH: i32 = 20;
const RECIPE_ROW_H: i32 = 68;
const RECIPE_ROW_PITCH: i32 = 70;
const TREE_ROW_PITCH: i32 = 12;

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
    fn new(state: &RecipeBrowserState, width: u32, height: u32) -> Self {
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
        let w = text_width(&text, scale) as i32;
        let cx = r.x + r.w as i32 / 2;
        let ty = r.y + ((r.h as i32 - 8 * mult as i32) / 2).max(0);
        self.list
            .text_str(cx * self.s - w / 2, ty * self.s, scale, color, &text);
    }

    /// MachineTutorial scene rasterized directly in physical pixels inside `r`.
    fn machine_tutorial(&mut self, r: UiRect, scene: &MachineTutorialScene) {
        let vp = self.phys(r);
        draw_machine_tutorial_scene(&mut self.list, vp, scene);
    }

    /// Emit an `ItemIcon` inside logical rect `r` — the GPU paints the
    /// atlas sprite; the software rasterizer falls back to the swatch.
    fn icon(&mut self, r: UiRect, item_id: &str) {
        self.list.item_icon_str(self.phys(r), item_id);
    }

    /// An 18px item slot: dark well + border + icon + optional count.
    /// `slot_id` registers the hit region when the slot is interactive.
    fn slot(
        &mut self,
        hits: &mut Vec<RecipeHitRegion>,
        x: i32,
        y: i32,
        item_id: &str,
        count: u64,
        slot_id: Option<RecipeWidgetId>,
    ) {
        let r = self.rect(x, y, 18, 18);
        self.fill_rect(r, UiColor::rgba(12, 12, 16, 220));
        self.border_rect(r, UiColor::rgb(96, 96, 108));
        self.icon(UiRect::new(x + 1, y + 1, 16, 16), item_id);
        if count > 1 {
            let label = format!("{count}");
            let w = text_width(&label, self.s as u32) as i32;
            self.list.text_str(
                (x + 18) * self.s - w,
                (y + 10) * self.s,
                self.s.max(1) as u32,
                UiColor::rgb(255, 255, 255),
                &label,
            );
        }
        if let Some(id) = slot_id {
            self.region(hits, id, r);
        }
    }

    fn region(&self, hits: &mut Vec<RecipeHitRegion>, id: RecipeWidgetId, r: UiRect) {
        let p = self.phys(r);
        let (x0, y0) = (p.x.max(0), p.y.max(0));
        let (x1, y1) = (
            p.right().min(self.w * self.s),
            p.bottom().min(self.h * self.s),
        );
        if x1 <= x0 || y1 <= y0 {
            return;
        }
        hits.push(RecipeHitRegion {
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
    if crate::draw::text_cells(&label) <= cells {
        return label;
    }
    if cells <= 3 {
        return ".".repeat(cells);
    }
    let mut clipped = crate::draw::first_cells(&label, cells - 3).to_owned();
    clipped.push_str("...");
    clipped
}
