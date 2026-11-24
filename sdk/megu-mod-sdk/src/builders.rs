// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later

//! Typed ergonomic builders producing a [`ModProject`].
//!
//! Every builder validates at `.build()` time with the same fail-closed
//! rules the runtime loaders apply: `ResourceLocation` id checking, vanilla
//! numeric bounds, and — for [`ModProject::export_megu_bundle_json`] — the
//! exact `megu-mod-bundle/1` key set `DeclarativeModBundle::from_json`
//! accepts verbatim.
//!
//! Id conventions: *declaration* ids (`BlockBuilder::new("brass_block")`)
//! may be bare — they are qualified with the project's mod id at
//! [`ModProjectBuilder::build`] time. *Reference* ids (ingredients, loot,
//! tag members, tame items) follow vanilla rules: bare means `minecraft:`,
//! and `#ns:tag` marks a tag reference.

use std::collections::BTreeMap;

use crate::diag::mk;
use crate::test_harness::SdkDiagnostic;
use crate::{check_mod_id, check_version_range, json, normalize_ref, qualify_decl_id, SdkError};

/// The only `schema` value emitted into `megu-mod-bundle.json`.
pub const BUNDLE_SCHEMA: &str = "megu-mod-bundle/1";

/// Default `mc_version_range` when the author does not declare one — covers
/// both first-class megu targets (`1.21.1`, `26.3`), matching
/// `unified_bundle.rs`'s built-in profile range.
const DEFAULT_MC_RANGE: &str = "[1.21,)";

// ---------------------------------------------------------------------------
// Declared model (`ModProject`)
// ---------------------------------------------------------------------------

/// A mod dependency (`version_range` in Maven or SemVer syntax).
#[derive(Clone, Debug, PartialEq)]
pub struct DependencySpec {
    /// Dependency mod id (`[a-z0-9_.-]+`).
    pub mod_id: String,
    /// Declared version range.
    pub version_range: String,
    /// `true` for a hard requirement.
    pub required: bool,
}

/// A declared block. `hardness` of `-1.0` marks an unbreakable (bedrock)
/// block; `light_level` is `0..=15`.
#[derive(Clone, Debug, PartialEq)]
pub struct BlockSpec {
    /// Qualified `ns:path` block id.
    pub id: String,
    /// Digging time base (`>= 0`, or `-1.0` unbreakable).
    pub hardness: f64,
    /// Explosion resistance (`>= 0`).
    pub blast_resistance: f64,
    /// Emitted light, `0..=15`.
    pub light_level: u8,
    /// Whether drops require the correct tool.
    pub requires_tool: bool,
    /// Item/block dropped on break (`ns:id`), when different from the block.
    pub loot_drop: Option<String>,
    /// Block tags this block joins (`ns:tag` ids).
    pub tags: Vec<String>,
    /// Optional block-entity capability attachment.
    pub capability: Option<CapabilitySpec>,
}

/// Item rarity tiers (vanilla enum).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rarity {
    Common,
    Uncommon,
    Rare,
    Epic,
}

impl Rarity {
    /// Lowercase wire spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Common => "common",
            Self::Uncommon => "uncommon",
            Self::Rare => "rare",
            Self::Epic => "epic",
        }
    }
}

/// Food properties for edible items.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FoodProps {
    /// Hunger restored (`0..=20`).
    pub nutrition: u32,
    /// Saturation restored (`>= 0`).
    pub saturation: f64,
}

/// A declared item.
#[derive(Clone, Debug, PartialEq)]
pub struct ItemSpec {
    /// Qualified `ns:path` item id.
    pub id: String,
    /// Stack size, `1..=64`.
    pub max_stack: u32,
    /// Durability (max damage) when the item is damageable.
    pub durability: Option<u32>,
    /// Attack damage bonus (`>= 0`) for weapons/tools.
    pub attack_damage: Option<f64>,
    /// Food stats for edible items.
    pub food: Option<FoodProps>,
    /// Tooltip rarity tier.
    pub rarity: Rarity,
    /// Item tags this item joins (`ns:tag` ids).
    pub tags: Vec<String>,
}

/// A NeoForge-style capability attachment on a block.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum CapabilitySpec {
    /// Item inventory with `slots` (`1..=256`).
    ItemHandler { slots: u32 },
    /// Fluid tank with `capacity_mb` millibuckets (`1..=i32::MAX`).
    FluidTank { capacity_mb: u64 },
    /// FE energy storage (`capacity_fe`, `max_io_fe <= capacity_fe`).
    EnergyStorage { capacity_fe: u64, max_io_fe: u64 },
}

impl CapabilitySpec {
    /// The capability's wire kind name.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::ItemHandler { .. } => "item_handler",
            Self::FluidTank { .. } => "fluid_tank",
            Self::EnergyStorage { .. } => "energy_storage",
        }
    }

    /// Bounds check shared by builder validation and the test harness.
    pub(crate) fn check_bounds(&self) -> Result<(), String> {
        match *self {
            Self::ItemHandler { slots } if !(1..=256).contains(&slots) => {
                Err("item_handler.slots must be in 1..=256".to_string())
            }
            Self::FluidTank { capacity_mb }
                if capacity_mb == 0 || capacity_mb > i32::MAX as u64 =>
            {
                Err("fluid_tank.capacity_mb must be in 1..=2147483647".to_string())
            }
            Self::EnergyStorage {
                capacity_fe,
                max_io_fe,
            } => {
                if capacity_fe == 0 || capacity_fe > i64::MAX as u64 {
                    return Err(format!("energy_storage.capacity_fe must be in 1..={}", i64::MAX));
                }
                if max_io_fe == 0 || max_io_fe > capacity_fe {
                    return Err(
                        "energy_storage.max_io_fe must be in 1..=capacity_fe".to_string()
                    );
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }
}

/// One recipe declaration; the variant pins the emitted `type`.
#[derive(Clone, Debug, PartialEq)]
pub enum RecipeKind {
    /// `minecraft:crafting_shaped`.
    Shaped {
        /// Grid rows (1..=3 rows of 1..=3 chars; `' '` = empty slot).
        pattern: Vec<String>,
        /// Symbol → ingredient (`ns:item` or `#ns:tag`).
        keys: Vec<(char, String)>,
        /// Crafted stack.
        result: ItemResult,
    },
    /// `minecraft:crafting_shapeless` (1..=9 ingredients).
    Shapeless {
        /// Input items/tags.
        ingredients: Vec<String>,
        /// Crafted stack.
        result: ItemResult,
    },
    /// `minecraft:smelting`.
    Smelting {
        /// Input item/tag.
        input: String,
        /// Smelted stack.
        result: ItemResult,
        /// Experience granted (`>= 0`).
        experience: f64,
        /// Cook time in ticks (`>= 1`).
        cook_ticks: u32,
    },
    /// `<mod>:pressing` — Create-style single-input pressing.
    Pressing {
        /// Input item/tag.
        input: String,
        /// Pressed stack.
        result: ItemResult,
    },
    /// `<mod>:alloy_forge` — base + catalyst + element alloy recipe.
    AlloyForge {
        /// Base ingredient.
        base: String,
        /// Catalyst ingredient.
        catalyst: String,
        /// Element id (qualified into the mod namespace).
        element: String,
        /// Heat units consumed (`>= 1`).
        heat_cost: u32,
        /// Forged stack.
        result: ItemResult,
    },
}

/// A recipe output stack.
#[derive(Clone, Debug, PartialEq)]
pub struct ItemResult {
    /// Result item id (`ns:id`).
    pub item: String,
    /// Stack count (`1..=64`).
    pub count: u32,
}

/// A declared recipe.
#[derive(Clone, Debug, PartialEq)]
pub struct RecipeSpec {
    /// Qualified `ns:path` recipe id.
    pub id: String,
    /// Typed recipe payload.
    pub kind: RecipeKind,
}

impl RecipeSpec {
    /// The emitted JSON `type` (`minecraft:*` for vanilla kinds,
    /// `<mod_ns>:*` for mod kinds).
    pub fn recipe_type(&self, mod_ns: &str) -> String {
        match &self.kind {
            RecipeKind::Shaped { .. } => "minecraft:crafting_shaped".to_string(),
            RecipeKind::Shapeless { .. } => "minecraft:crafting_shapeless".to_string(),
            RecipeKind::Smelting { .. } => "minecraft:smelting".to_string(),
            RecipeKind::Pressing { .. } => format!("{mod_ns}:pressing"),
            RecipeKind::AlloyForge { .. } => format!("{mod_ns}:alloy_forge"),
        }
    }

    /// Every ingredient reference the recipe consumes.
    pub(crate) fn inputs(&self) -> Vec<&str> {
        match &self.kind {
            RecipeKind::Shaped { keys, .. } => keys.iter().map(|(_, v)| v.as_str()).collect(),
            RecipeKind::Shapeless { ingredients, .. } => {
                ingredients.iter().map(String::as_str).collect()
            }
            RecipeKind::Smelting { input, .. } | RecipeKind::Pressing { input, .. } => {
                vec![input.as_str()]
            }
            RecipeKind::AlloyForge {
                base, catalyst, ..
            } => vec![base.as_str(), catalyst.as_str()],
        }
    }

    /// The produced item id.
    pub(crate) fn result_item(&self) -> &str {
        match &self.kind {
            RecipeKind::Shaped { result, .. }
            | RecipeKind::Shapeless { result, .. }
            | RecipeKind::Smelting { result, .. }
            | RecipeKind::Pressing { result, .. }
            | RecipeKind::AlloyForge { result, .. } => result.item.as_str(),
        }
    }

    /// The produced stack count.
    pub(crate) fn result_count(&self) -> u32 {
        match &self.kind {
            RecipeKind::Shaped { result, .. }
            | RecipeKind::Shapeless { result, .. }
            | RecipeKind::Smelting { result, .. }
            | RecipeKind::Pressing { result, .. }
            | RecipeKind::AlloyForge { result, .. } => result.count,
        }
    }
}

/// Whether a tag file lives under `tags/item/` or `tags/block/`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum TagKind {
    /// `data/<ns>/tags/item/<path>.json`.
    Item,
    /// `data/<ns>/tags/block/<path>.json`.
    Block,
}

impl TagKind {
    /// The datapack directory name.
    pub fn dir(self) -> &'static str {
        match self {
            Self::Item => "item",
            Self::Block => "block",
        }
    }
}

/// A declared tag file's contents.
#[derive(Clone, Debug, PartialEq)]
pub struct TagSpec {
    /// `Item` or `Block` tag directory.
    pub kind: TagKind,
    /// Qualified `ns:path` tag id (e.g. `c:ores/brass`).
    pub id: String,
    /// Members: `ns:id` entries and `#ns:tag` nested-tag references.
    pub values: Vec<String>,
    /// Whether the file replaces (not merges into) same-id tags.
    pub replace: bool,
}

/// Creature movement domain, mapped onto the bundle's `movement` kinds.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Movement {
    /// `{"kind":"land","ground_speed":s}`.
    Land {
        /// Blocks/tick ground speed (`>= 0`).
        ground_speed: f64,
    },
    /// `{"kind":"gliding_flight",...}` — flying creatures.
    Flying {
        /// Ground speed (`>= 0`).
        ground_speed: f64,
        /// Air speed (`>= 0`).
        fly_speed: f64,
        /// Fall-speed multiplier while gliding (`>= 0`).
        glide_fall_scale: f64,
    },
    /// `{"kind":"aquatic",...}` — water-bound creatures.
    Aquatic {
        /// Swim speed (`>= 0`).
        swim_speed: f64,
        /// Air suffocation budget in ticks (`>= 1`).
        suffocates_in_air_ticks: u32,
    },
}

impl Movement {
    /// Vanilla-ish land default (`0.23`).
    pub fn land() -> Self {
        Self::Land { ground_speed: 0.23 }
    }

    /// Vanilla-ish flying default (`0.23` ground / `0.6` fly / `0.1` glide).
    pub fn flying() -> Self {
        Self::Flying {
            ground_speed: 0.23,
            fly_speed: 0.6,
            glide_fall_scale: 0.1,
        }
    }

    /// Vanilla-ish aquatic default (`0.5` swim, 15 s air budget).
    pub fn aquatic() -> Self {
        Self::Aquatic {
            swim_speed: 0.5,
            suffocates_in_air_ticks: 300,
        }
    }
}

/// Elemental breath weapon carried by the extended creature record
/// (registry JSON; not part of `megu-mod-bundle/1`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ElementalBreath {
    Fire,
    Ice,
    Lightning,
}

impl ElementalBreath {
    /// Lowercase wire spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Fire => "fire",
            Self::Ice => "ice",
            Self::Lightning => "lightning",
        }
    }
}

/// A declared creature. Maps onto the bundle's `creatures[]` entries;
/// `growth_stages` and `elemental_breath` live in the extended registry.
#[derive(Clone, Debug, PartialEq)]
pub struct CreatureSpec {
    /// Qualified entity type id (`ns:path`).
    pub entity_type: String,
    /// Maximum health (`> 0`).
    pub max_health: f64,
    /// Movement domain.
    pub movement: Movement,
    /// Items/`#tags` that trigger a tame roll.
    pub tame_items: Vec<String>,
    /// Tame success probability (`0.0..=1.0`).
    pub tame_chance: f64,
    /// Items/`#tags` that trigger love mode.
    pub breed_items: Vec<String>,
    /// `(item_or_tag, heal)` pairs usable on hurt tamed creatures.
    pub heal_items: Vec<(String, f64)>,
    /// Ticks a baby takes to mature (`None` = runtime default 24 000).
    pub baby_growth_ticks: Option<u32>,
    /// Post-breeding cooldown (`None` = runtime default 6 000).
    pub breed_cooldown_ticks: Option<u32>,
    /// Include `Wander` in the command cycle.
    pub supports_wander_mode: bool,
    /// Bird/perch pets may ride the owner's shoulder.
    pub can_shoulder_perch: bool,
    /// Mountable once tamed.
    pub rideable_when_tamed: bool,
    /// Visual growth stages (`1..=5`, extended record).
    pub growth_stages: Option<u8>,
    /// Elemental breath weapon (extended record).
    pub elemental_breath: Option<ElementalBreath>,
}

/// A declared custom dimension (emitted as a `megu-dimension/1`
/// `dimension_type` document inside the archive).
#[derive(Clone, Debug, PartialEq)]
pub struct DimensionSpec {
    /// Qualified `ns:path` dimension id.
    pub id: String,
    /// Bottom build height — multiple of 16 in `[-2032, 2031]`.
    pub min_y: i32,
    /// Build height — multiple of 16 in `[16, 4064]`, `min_y + height <= 4064`.
    pub height: u32,
    /// Whether the sky is visible.
    pub has_skylight: bool,
    /// Overworld coordinate scale (`1e-5..=3e7`; `8.0` = nether-like).
    pub coordinate_scale: f64,
    /// Constant ambient light (`0.0..=1.0`; `0.0` = vanilla-style lighting).
    pub ambient_light: f64,
}

/// One locale file (`en_us`, `ja_jp`, …) with flat key → text entries.
#[derive(Clone, Debug, PartialEq)]
pub struct LocaleSpec {
    /// BCP-47-ish code (`[a-z]+_[a-z]+`).
    pub locale: String,
    /// Translation keys (`block.<ns>.<id>`, `item.<ns>.<id>`, …) → text.
    pub entries: BTreeMap<String, String>,
}

/// The fully validated mod project every exporter consumes.
#[derive(Clone, Debug, PartialEq)]
pub struct ModProject {
    /// Mod id (`[a-z0-9_.-]+`, also the default id namespace).
    pub mod_id: String,
    /// Mod version string.
    pub version: String,
    /// Human-readable name (defaults to `mod_id`).
    pub display_name: String,
    /// Declared Minecraft compatibility range.
    pub mc_version_range: String,
    /// Mod dependencies.
    pub dependencies: Vec<DependencySpec>,
    /// Declared blocks.
    pub blocks: Vec<BlockSpec>,
    /// Declared items.
    pub items: Vec<ItemSpec>,
    /// Declared recipes.
    pub recipes: Vec<RecipeSpec>,
    /// Declared tag files.
    pub tags: Vec<TagSpec>,
    /// Declared creatures.
    pub creatures: Vec<CreatureSpec>,
    /// Declared dimensions.
    pub dimensions: Vec<DimensionSpec>,
    /// Locale files (`en_us`, `ja_jp`, …).
    pub locales: Vec<LocaleSpec>,
}

impl ModProject {
    /// Emit the strict `megu-mod-bundle/1` document — exactly the top-level
    /// keys `DeclarativeModBundle::from_json` accepts (`schema`, `mod_id`,
    /// `mc_version_range`, the four required `*_barriers`-style sections,
    /// `creatures`, `custom_channels`). Creatures declared via
    /// [`CreatureBuilder`] are folded into `creatures[]`; every other
    /// section lives in archive data files.
    ///
    /// # Errors
    ///
    /// `SdkError::Diagnostics` when the (possibly hand-constructed) project
    /// fails [`crate::test_harness`] validation — exporting a bundle for an
    /// invalid project would produce a document the runtime rejects anyway.
    pub fn export_megu_bundle_json(&self) -> Result<String, SdkError> {
        let diags = crate::test_harness::validate_project(self);
        if !diags.is_empty() {
            return Err(SdkError::Diagnostics(diags));
        }
        Ok(json::render(&self.bundle_ast()))
    }

    /// The bundle document as a canonical [`json`] AST.
    pub(crate) fn bundle_ast(&self) -> json::J {
        let creatures = self
            .creatures
            .iter()
            .map(creature_ast)
            .collect();
        json::obj(vec![
            ("schema", json::s(BUNDLE_SCHEMA)),
            ("mod_id", json::s(&self.mod_id)),
            ("mc_version_range", json::s(&self.mc_version_range)),
            ("portals", json::arr(Vec::new())),
            ("progression_barriers", json::arr(Vec::new())),
            ("biome_regions", json::arr(Vec::new())),
            ("creatures", json::arr(creatures)),
            ("custom_channels", json::arr(Vec::new())),
        ])
    }

    /// The extended `megu-mod-registry/1` document covering the sections
    /// `megu-mod-bundle/1` cannot express: block/item property records,
    /// capability attachments, and extended creature fields.
    pub(crate) fn registry_ast(&self) -> json::J {
        let blocks = self
            .blocks
            .iter()
            .map(|b| (b.id.clone(), block_ast(b)))
            .collect();
        let items = self
            .items
            .iter()
            .map(|i| (i.id.clone(), item_ast(i)))
            .collect();
        let creatures = self
            .creatures
            .iter()
            .map(|c| (c.entity_type.clone(), creature_ext_ast(c)))
            .collect();
        json::obj(vec![
            ("schema", json::s("megu-mod-registry/1")),
            ("blocks", json::J::Obj(blocks)),
            ("items", json::J::Obj(items)),
            ("creatures", json::J::Obj(creatures)),
        ])
    }
}

fn creature_ast(c: &CreatureSpec) -> json::J {
    let movement = match c.movement {
        Movement::Land { ground_speed } => json::obj(vec![
            ("kind", json::s("land")),
            ("ground_speed", json::n(ground_speed)),
        ]),
        Movement::Flying {
            ground_speed,
            fly_speed,
            glide_fall_scale,
        } => json::obj(vec![
            ("kind", json::s("gliding_flight")),
            ("ground_speed", json::n(ground_speed)),
            ("fly_speed", json::n(fly_speed)),
            ("glide_fall_scale", json::n(glide_fall_scale)),
        ]),
        Movement::Aquatic {
            swim_speed,
            suffocates_in_air_ticks,
        } => json::obj(vec![
            ("kind", json::s("aquatic")),
            ("swim_speed", json::n(swim_speed)),
            (
                "suffocates_in_air_ticks",
                json::u(u64::from(suffocates_in_air_ticks)),
            ),
        ]),
    };
    let mut pairs: Vec<(&str, json::J)> = vec![
        ("entity_type", json::s(&c.entity_type)),
        ("max_health", json::n(c.max_health)),
        ("movement", movement),
    ];
    if !c.tame_items.is_empty() {
        pairs.push((
            "tame_items",
            json::arr(c.tame_items.iter().map(|i| json::s(i)).collect()),
        ));
    }
    if c.tame_chance != 0.0 {
        pairs.push(("tame_chance", json::n(c.tame_chance)));
    }
    if !c.breed_items.is_empty() {
        pairs.push((
            "breed_items",
            json::arr(c.breed_items.iter().map(|i| json::s(i)).collect()),
        ));
    }
    if !c.heal_items.is_empty() {
        pairs.push((
            "heal_items",
            json::arr(
                c.heal_items
                    .iter()
                    .map(|(item, heal)| {
                        json::obj(vec![
                            ("item", json::s(item)),
                            ("heal", json::n(*heal)),
                        ])
                    })
                    .collect(),
            ),
        ));
    }
    if let Some(ticks) = c.baby_growth_ticks {
        pairs.push(("baby_growth_ticks", json::u(u64::from(ticks))));
    }
    if let Some(ticks) = c.breed_cooldown_ticks {
        pairs.push(("breed_cooldown_ticks", json::u(u64::from(ticks))));
    }
    if c.supports_wander_mode {
        pairs.push(("supports_wander_mode", json::b(true)));
    }
    if c.can_shoulder_perch {
        pairs.push(("can_shoulder_perch", json::b(true)));
    }
    if c.rideable_when_tamed {
        pairs.push(("rideable_when_tamed", json::b(true)));
    }
    json::obj(pairs)
}

fn block_ast(b: &BlockSpec) -> json::J {
    let mut pairs: Vec<(&str, json::J)> = vec![
        ("hardness", json::n(b.hardness)),
        ("blast_resistance", json::n(b.blast_resistance)),
        ("light_level", json::u(u64::from(b.light_level))),
        ("requires_tool", json::b(b.requires_tool)),
    ];
    if let Some(drop) = &b.loot_drop {
        pairs.push(("loot_drop", json::s(drop)));
    }
    if !b.tags.is_empty() {
        pairs.push((
            "tags",
            json::arr(b.tags.iter().map(|t| json::s(t)).collect()),
        ));
    }
    if let Some(cap) = &b.capability {
        pairs.push(("capability", capability_ast(cap)));
    }
    json::obj(pairs)
}

fn capability_ast(cap: &CapabilitySpec) -> json::J {
    match *cap {
        CapabilitySpec::ItemHandler { slots } => json::obj(vec![
            ("kind", json::s("item_handler")),
            ("slots", json::u(u64::from(slots))),
        ]),
        CapabilitySpec::FluidTank { capacity_mb } => json::obj(vec![
            ("kind", json::s("fluid_tank")),
            ("capacity_mb", json::u(capacity_mb)),
        ]),
        CapabilitySpec::EnergyStorage {
            capacity_fe,
            max_io_fe,
        } => json::obj(vec![
            ("kind", json::s("energy_storage")),
            ("capacity_fe", json::u(capacity_fe)),
            ("max_io_fe", json::u(max_io_fe)),
        ]),
    }
}

fn item_ast(i: &ItemSpec) -> json::J {
    let mut pairs: Vec<(&str, json::J)> = vec![
        ("max_stack", json::u(u64::from(i.max_stack))),
        ("rarity", json::s(i.rarity.as_str())),
    ];
    if let Some(d) = i.durability {
        pairs.push(("durability", json::u(u64::from(d))));
    }
    if let Some(d) = i.attack_damage {
        pairs.push(("attack_damage", json::n(d)));
    }
    if let Some(food) = i.food {
        pairs.push((
            "food",
            json::obj(vec![
                ("nutrition", json::u(u64::from(food.nutrition))),
                ("saturation", json::n(food.saturation)),
            ]),
        ));
    }
    if !i.tags.is_empty() {
        pairs.push((
            "tags",
            json::arr(i.tags.iter().map(|t| json::s(t)).collect()),
        ));
    }
    json::obj(pairs)
}

fn creature_ext_ast(c: &CreatureSpec) -> json::J {
    let mut pairs: Vec<(&str, json::J)> = Vec::new();
    if let Some(stages) = c.growth_stages {
        pairs.push(("growth_stages", json::u(u64::from(stages))));
    }
    if let Some(breath) = c.elemental_breath {
        pairs.push(("elemental_breath", json::s(breath.as_str())));
    }
    json::obj(pairs)
}

// ---------------------------------------------------------------------------
// Spec builders
// ---------------------------------------------------------------------------

/// Fluent block declaration. Terminal [`build`](Self::build) is called by
/// [`ModProjectBuilder::build`]; call it directly only for standalone specs.
#[derive(Clone, Debug)]
pub struct BlockBuilder {
    id: String,
    hardness: f64,
    blast_resistance: f64,
    light_level: u8,
    requires_tool: bool,
    loot_drop: Option<String>,
    tags: Vec<String>,
    capability: Option<CapabilitySpec>,
}

impl BlockBuilder {
    /// New block; `id` may be bare (`"brass_block"` → `<mod>:brass_block`)
    /// or namespaced.
    pub fn new(id: &str) -> Self {
        Self {
            id: id.to_string(),
            hardness: 1.0,
            blast_resistance: 1.0,
            light_level: 0,
            requires_tool: false,
            loot_drop: None,
            tags: Vec::new(),
            capability: None,
        }
    }

    /// Digging hardness (`>= 0`, or `-1.0` for unbreakable).
    pub fn hardness(mut self, v: f64) -> Self {
        self.hardness = v;
        self
    }

    /// Explosion resistance (`>= 0`).
    pub fn blast_resistance(mut self, v: f64) -> Self {
        self.blast_resistance = v;
        self
    }

    /// Emitted light level (`0..=15`).
    pub fn light_level(mut self, v: u8) -> Self {
        self.light_level = v;
        self
    }

    /// Whether breaking requires the correct tool for drops.
    pub fn requires_tool(mut self, v: bool) -> Self {
        self.requires_tool = v;
        self
    }

    /// Item dropped on break (`ns:item`), if different from the block.
    pub fn loot_drop(mut self, id: &str) -> Self {
        self.loot_drop = Some(id.to_string());
        self
    }

    /// Join a block tag (`"mineable/pickaxe"` → `minecraft:mineable/pickaxe`,
    /// `"c:ores/brass"` stays `c:`).
    pub fn tag(mut self, tag_id: &str) -> Self {
        self.tags.push(tag_id.to_string());
        self
    }

    /// Attach a block-entity capability.
    pub fn capability(mut self, cap: CapabilitySpec) -> Self {
        self.capability = Some(cap);
        self
    }

    /// Validate into a [`BlockSpec`], qualifying the id with `default_ns`.
    pub fn build(self, default_ns: &str) -> Result<BlockSpec, SdkDiagnostic> {
        const CTX: &str = "block";
        let id = qualify_decl_id(&self.id, default_ns, CTX)?;
        if !self.hardness.is_finite() || (self.hardness < 0.0 && self.hardness != -1.0) {
            return Err(range_diag(CTX, "hardness", ">= 0 or -1.0 (unbreakable)"));
        }
        if !self.blast_resistance.is_finite() || self.blast_resistance < 0.0 {
            return Err(range_diag(CTX, "blast_resistance", ">= 0"));
        }
        if self.light_level > 15 {
            return Err(range_diag(CTX, "light_level", "0..=15"));
        }
        let loot_drop = self
            .loot_drop
            .as_deref()
            .map(|d| normalize_ref(d, &format!("{CTX}:{id}.loot_drop")))
            .transpose()?;
        if let Some(d) = &loot_drop {
            if d.starts_with('#') {
                return Err(mk(
                    "E_REF_ID",
                    format!("{CTX}:{id}.loot_drop"),
                    format!("loot drop {d:?} must be an item id, not a tag"),
                    "pass a concrete 'ns:item' id",
                ));
            }
        }
        let mut tags = Vec::with_capacity(self.tags.len());
        for t in &self.tags {
            let n = normalize_ref(t, &format!("{CTX}:{id}.tags"))?;
            if n.starts_with('#') {
                return Err(mk(
                    "E_REF_ID",
                    format!("{CTX}:{id}.tags"),
                    format!("tag member {n:?} must be a tag id, not a tag reference"),
                    "pass plain 'ns:tag' ids to .tag()",
                ));
            }
            tags.push(n);
        }
        if let Some(cap) = &self.capability {
            cap.check_bounds().map_err(|why| {
                mk(
                    "E_CAPABILITY",
                    format!("{CTX}:{id}.capability"),
                    why,
                    cap_fix_hint(cap),
                )
            })?;
        }
        Ok(BlockSpec {
            id,
            hardness: self.hardness,
            blast_resistance: self.blast_resistance,
            light_level: self.light_level,
            requires_tool: self.requires_tool,
            loot_drop,
            tags,
            capability: self.capability,
        })
    }
}

fn cap_fix_hint(cap: &CapabilitySpec) -> String {
    match *cap {
        CapabilitySpec::ItemHandler { .. } => {
            "use ItemHandler { slots: 1..=256 }".to_string()
        }
        CapabilitySpec::FluidTank { .. } => {
            "use FluidTank { capacity_mb: 1..=2147483647 }".to_string()
        }
        CapabilitySpec::EnergyStorage { .. } => {
            "use EnergyStorage { capacity_fe, max_io_fe } with 1 <= max_io_fe <= capacity_fe"
                .to_string()
        }
    }
}

/// Fluent item declaration.
#[derive(Clone, Debug)]
pub struct ItemBuilder {
    id: String,
    max_stack: u32,
    durability: Option<u32>,
    attack_damage: Option<f64>,
    food: Option<FoodProps>,
    rarity: Rarity,
    tags: Vec<String>,
}

impl ItemBuilder {
    /// New item; `id` may be bare or namespaced.
    pub fn new(id: &str) -> Self {
        Self {
            id: id.to_string(),
            max_stack: 64,
            durability: None,
            attack_damage: None,
            food: None,
            rarity: Rarity::Common,
            tags: Vec::new(),
        }
    }

    /// Stack size (`1..=64`).
    pub fn max_stack(mut self, v: u32) -> Self {
        self.max_stack = v;
        self
    }

    /// Durability (max damage) — makes the item damageable.
    pub fn durability(mut self, v: u32) -> Self {
        self.durability = Some(v);
        self
    }

    /// Attack damage bonus (`>= 0`).
    pub fn attack_damage(mut self, v: f64) -> Self {
        self.attack_damage = Some(v);
        self
    }

    /// Food stats: `(nutrition 0..=20, saturation >= 0)`.
    pub fn food(mut self, nutrition: u32, saturation: f64) -> Self {
        self.food = Some(FoodProps {
            nutrition,
            saturation,
        });
        self
    }

    /// Tooltip rarity.
    pub fn rarity(mut self, v: Rarity) -> Self {
        self.rarity = v;
        self
    }

    /// Join an item tag (`"c:ingots/brass"` style).
    pub fn tag(mut self, tag_id: &str) -> Self {
        self.tags.push(tag_id.to_string());
        self
    }

    /// Validate into an [`ItemSpec`].
    pub fn build(self, default_ns: &str) -> Result<ItemSpec, SdkDiagnostic> {
        const CTX: &str = "item";
        let id = qualify_decl_id(&self.id, default_ns, CTX)?;
        if !(1..=64).contains(&self.max_stack) {
            return Err(range_diag(CTX, "max_stack", "1..=64"));
        }
        if let Some(d) = self.durability {
            if d == 0 {
                return Err(range_diag(CTX, "durability", ">= 1"));
            }
        }
        if let Some(d) = self.attack_damage {
            if !d.is_finite() || d < 0.0 {
                return Err(range_diag(CTX, "attack_damage", ">= 0"));
            }
        }
        if let Some(food) = self.food {
            if food.nutrition > 20 {
                return Err(range_diag(CTX, "food.nutrition", "0..=20"));
            }
            if !food.saturation.is_finite() || food.saturation < 0.0 {
                return Err(range_diag(CTX, "food.saturation", ">= 0"));
            }
        }
        let mut tags = Vec::with_capacity(self.tags.len());
        for t in &self.tags {
            let n = normalize_ref(t, &format!("{CTX}:{id}.tags"))?;
            if n.starts_with('#') {
                return Err(mk(
                    "E_REF_ID",
                    format!("{CTX}:{id}.tags"),
                    format!("tag member {n:?} must be a tag id, not a tag reference"),
                    "pass plain 'ns:tag' ids to .tag()",
                ));
            }
            tags.push(n);
        }
        Ok(ItemSpec {
            id,
            max_stack: self.max_stack,
            durability: self.durability,
            attack_damage: self.attack_damage,
            food: self.food,
            rarity: self.rarity,
            tags,
        })
    }
}

/// Fluent recipe declaration — pick a constructor per `type`.
#[derive(Clone, Debug)]
pub struct RecipeBuilder {
    id: String,
    kind: RecipeKind,
}

impl RecipeBuilder {
    /// `minecraft:crafting_shaped`: `pattern` rows (1..=3 × 1..=3 chars,
    /// `' '` empty), `keys` mapping symbols to `ns:item`/`#ns:tag`.
    pub fn shaped(id: &str, pattern: &[&str], keys: &[(char, &str)], result: &str) -> Self {
        Self {
            id: id.to_string(),
            kind: RecipeKind::Shaped {
                pattern: pattern.iter().map(|s| (*s).to_string()).collect(),
                keys: keys
                    .iter()
                    .map(|(c, v)| (*c, (*v).to_string()))
                    .collect(),
                result: ItemResult {
                    item: result.to_string(),
                    count: 1,
                },
            },
        }
    }

    /// `minecraft:crafting_shapeless` (1..=9 ingredients).
    pub fn shapeless(id: &str, ingredients: &[&str], result: &str) -> Self {
        Self {
            id: id.to_string(),
            kind: RecipeKind::Shapeless {
                ingredients: ingredients.iter().map(|s| (*s).to_string()).collect(),
                result: ItemResult {
                    item: result.to_string(),
                    count: 1,
                },
            },
        }
    }

    /// `minecraft:smelting` with XP and cook ticks.
    pub fn smelting(id: &str, input: &str, result: &str, xp: f64, cook_ticks: u32) -> Self {
        Self {
            id: id.to_string(),
            kind: RecipeKind::Smelting {
                input: input.to_string(),
                result: ItemResult {
                    item: result.to_string(),
                    count: 1,
                },
                experience: xp,
                cook_ticks,
            },
        }
    }

    /// `<mod>:pressing` — Create-style press recipe.
    pub fn pressing(id: &str, input: &str, result: &str) -> Self {
        Self {
            id: id.to_string(),
            kind: RecipeKind::Pressing {
                input: input.to_string(),
                result: ItemResult {
                    item: result.to_string(),
                    count: 1,
                },
            },
        }
    }

    /// `<mod>:alloy_forge` — base + catalyst fused by `element`
    /// (an id qualified into the mod namespace, e.g. `"fire"` → `<mod>:fire`).
    pub fn alloy_forge(
        id: &str,
        base: &str,
        catalyst: &str,
        element: &str,
        heat_cost: u32,
        result: &str,
    ) -> Self {
        Self {
            id: id.to_string(),
            kind: RecipeKind::AlloyForge {
                base: base.to_string(),
                catalyst: catalyst.to_string(),
                element: element.to_string(),
                heat_cost,
                result: ItemResult {
                    item: result.to_string(),
                    count: 1,
                },
            },
        }
    }

    /// Result stack size (`1..=64`).
    pub fn result_count(mut self, count: u32) -> Self {
        match &mut self.kind {
            RecipeKind::Shaped { result, .. }
            | RecipeKind::Shapeless { result, .. }
            | RecipeKind::Smelting { result, .. }
            | RecipeKind::Pressing { result, .. }
            | RecipeKind::AlloyForge { result, .. } => result.count = count,
        }
        self
    }

    /// Validate into a [`RecipeSpec`].
    pub fn build(self, default_ns: &str) -> Result<RecipeSpec, SdkDiagnostic> {
        const CTX: &str = "recipe";
        let id = qualify_decl_id(&self.id, default_ns, CTX)?;
        let kind = match self.kind {
            RecipeKind::Shaped {
                pattern,
                keys,
                result,
            } => {
                if pattern.is_empty() || pattern.len() > 3 {
                    return Err(range_diag(CTX, "pattern", "1..=3 rows"));
                }
                let width = pattern[0].len();
                if width == 0 || width > 3 {
                    return Err(range_diag(CTX, "pattern", "rows of 1..=3 chars"));
                }
                if pattern.iter().any(|row| row.len() != width) {
                    return Err(mk(
                        "E_RANGE",
                        format!("{CTX}:{id}.pattern"),
                        "shaped pattern rows must have equal width".to_string(),
                        "pad shorter rows with ' ' for empty slots",
                    ));
                }
                let mut key_map = BTreeMap::new();
                for (c, v) in &keys {
                    if *c == ' ' {
                        return Err(mk(
                            "E_RANGE",
                            format!("{CTX}:{id}.key"),
                            "' ' is reserved for empty slots".to_string(),
                            "use a non-space symbol in the key map",
                        ));
                    }
                    key_map.insert(
                        *c,
                        normalize_ref(v, &format!("{CTX}:{id}.key[{c:?}]"))?,
                    );
                }
                let mut filled = false;
                for ch in pattern.iter().flat_map(|row| row.chars()) {
                    if ch == ' ' {
                        continue;
                    }
                    filled = true;
                    if !key_map.contains_key(&ch) {
                        return Err(mk(
                            "E_RECIPE",
                            format!("{CTX}:{id}.pattern"),
                            format!("pattern symbol {ch:?} has no key entry"),
                            "add the symbol to the keys map",
                        ));
                    }
                }
                if !filled {
                    return Err(mk(
                        "E_RECIPE",
                        format!("{CTX}:{id}.pattern"),
                        "shaped pattern is entirely empty".to_string(),
                        "fill at least one slot",
                    ));
                }
                RecipeKind::Shaped {
                    pattern,
                    keys: key_map.into_iter().collect(),
                    result: check_result(result, &id, CTX)?,
                }
            }
            RecipeKind::Shapeless {
                ingredients,
                result,
            } => {
                if ingredients.is_empty() || ingredients.len() > 9 {
                    return Err(range_diag(CTX, "ingredients", "1..=9 items"));
                }
                let ingredients = ingredients
                    .iter()
                    .map(|i| normalize_ref(i, &format!("{CTX}:{id}.ingredients")))
                    .collect::<Result<_, _>>()?;
                RecipeKind::Shapeless {
                    ingredients,
                    result: check_result(result, &id, CTX)?,
                }
            }
            RecipeKind::Smelting {
                input,
                result,
                experience,
                cook_ticks,
            } => {
                if !experience.is_finite() || experience < 0.0 {
                    return Err(range_diag(CTX, "experience", ">= 0"));
                }
                if cook_ticks == 0 {
                    return Err(range_diag(CTX, "cook_ticks", ">= 1"));
                }
                RecipeKind::Smelting {
                    input: normalize_ref(&input, &format!("{CTX}:{id}.ingredient"))?,
                    result: check_result(result, &id, CTX)?,
                    experience,
                    cook_ticks,
                }
            }
            RecipeKind::Pressing { input, result } => RecipeKind::Pressing {
                input: normalize_ref(&input, &format!("{CTX}:{id}.ingredients"))?,
                result: check_result(result, &id, CTX)?,
            },
            RecipeKind::AlloyForge {
                base,
                catalyst,
                element,
                heat_cost,
                result,
            } => {
                if heat_cost == 0 {
                    return Err(range_diag(CTX, "heat_cost", ">= 1"));
                }
                RecipeKind::AlloyForge {
                    base: normalize_ref(&base, &format!("{CTX}:{id}.base"))?,
                    catalyst: normalize_ref(&catalyst, &format!("{CTX}:{id}.catalyst"))?,
                    element: qualify_decl_id(&element, default_ns, &format!("{CTX}:{id}.element"))?,
                    heat_cost,
                    result: check_result(result, &id, CTX)?,
                }
            }
        };
        Ok(RecipeSpec { id, kind })
    }
}

fn check_result(result: ItemResult, id: &str, ctx: &str) -> Result<ItemResult, SdkDiagnostic> {
    let item = normalize_ref(&result.item, &format!("{ctx}:{id}.result"))?;
    if item.starts_with('#') {
        return Err(mk(
            "E_RECIPE",
            format!("{ctx}:{id}.result"),
            format!("result {item:?} must be an item id, not a tag"),
            "name a concrete 'ns:item' as the recipe result",
        ));
    }
    if !(1..=64).contains(&result.count) {
        return Err(range_diag(ctx, "result.count", "1..=64"));
    }
    Ok(ItemResult {
        item,
        count: result.count,
    })
}

/// Fluent tag declaration (`data/<ns>/tags/{item,block}/<id>.json`).
#[derive(Clone, Debug)]
pub struct TagBuilder {
    kind: TagKind,
    id: String,
    values: Vec<String>,
    replace: bool,
}

impl TagBuilder {
    /// An item tag file.
    pub fn item(tag_id: &str) -> Self {
        Self::of(TagKind::Item, tag_id)
    }

    /// A block tag file.
    pub fn block(tag_id: &str) -> Self {
        Self::of(TagKind::Block, tag_id)
    }

    fn of(kind: TagKind, tag_id: &str) -> Self {
        Self {
            kind,
            id: tag_id.to_string(),
            values: Vec::new(),
            replace: false,
        }
    }

    /// Add a member id (`ns:id`; bare → `minecraft:`).
    #[allow(clippy::should_implement_trait)]
    pub fn add(mut self, id: &str) -> Self {
        self.values.push(id.to_string());
        self
    }

    /// Add a nested tag reference; accepts `"ns:tag"` or `"#ns:tag"`.
    /// Unified common tags (`#c:`) and legacy (`#forge:`, `#neoforge:`)
    /// namespaces are all valid members.
    pub fn add_tag(mut self, nested_tag: &str) -> Self {
        let tag = nested_tag.strip_prefix('#').unwrap_or(nested_tag);
        self.values.push(format!("#{tag}"));
        self
    }

    /// Replace instead of merge when another datapack declares this tag.
    pub fn replace(mut self, v: bool) -> Self {
        self.replace = v;
        self
    }

    /// Validate into a [`TagSpec`].
    pub fn build(self, default_ns: &str) -> Result<TagSpec, SdkDiagnostic> {
        const CTX: &str = "tag";
        let id = qualify_decl_id(&self.id, default_ns, CTX)?;
        let mut seen = std::collections::BTreeSet::new();
        let mut values = Vec::with_capacity(self.values.len());
        for v in &self.values {
            let n = normalize_ref(v, &format!("{CTX}:{id}.values"))?;
            if seen.insert(n.clone()) {
                values.push(n);
            }
        }
        Ok(TagSpec {
            kind: self.kind,
            id,
            values,
            replace: self.replace,
        })
    }
}

/// Fluent creature declaration → the bundle's `creatures[]` section.
#[derive(Clone, Debug)]
pub struct CreatureBuilder {
    id: String,
    max_health: f64,
    movement: Movement,
    tame_items: Vec<String>,
    tame_chance: f64,
    breed_items: Vec<String>,
    heal_items: Vec<(String, f64)>,
    baby_growth_ticks: Option<u32>,
    breed_cooldown_ticks: Option<u32>,
    supports_wander_mode: bool,
    can_shoulder_perch: bool,
    rideable_when_tamed: bool,
    growth_stages: Option<u8>,
    elemental_breath: Option<ElementalBreath>,
}

impl CreatureBuilder {
    /// New creature; `id` may be bare or namespaced.
    pub fn new(id: &str) -> Self {
        Self {
            id: id.to_string(),
            max_health: 20.0,
            movement: Movement::land(),
            tame_items: Vec::new(),
            tame_chance: 0.0,
            breed_items: Vec::new(),
            heal_items: Vec::new(),
            baby_growth_ticks: None,
            breed_cooldown_ticks: None,
            supports_wander_mode: false,
            can_shoulder_perch: false,
            rideable_when_tamed: false,
            growth_stages: None,
            elemental_breath: None,
        }
    }

    /// Maximum health (`> 0`).
    pub fn max_health(mut self, v: f64) -> Self {
        self.max_health = v;
        self
    }

    /// Movement domain ([`Movement::land`], [`Movement::flying`],
    /// [`Movement::aquatic`], or an explicit variant).
    pub fn movement(mut self, v: Movement) -> Self {
        self.movement = v;
        self
    }

    /// Item/`#tag` that triggers a tame roll.
    pub fn tame_item(mut self, id: &str) -> Self {
        self.tame_items.push(id.to_string());
        self
    }

    /// Tame success probability (`0.0..=1.0`).
    pub fn tame_chance(mut self, v: f64) -> Self {
        self.tame_chance = v;
        self
    }

    /// Item/`#tag` that triggers love mode.
    pub fn breed_item(mut self, id: &str) -> Self {
        self.breed_items.push(id.to_string());
        self
    }

    /// `(item_or_tag, heal_amount)` usable on hurt tamed creatures.
    pub fn heal_item(mut self, id: &str, heal: f64) -> Self {
        self.heal_items.push((id.to_string(), heal));
        self
    }

    /// Baby growth time in ticks.
    pub fn baby_growth_ticks(mut self, v: u32) -> Self {
        self.baby_growth_ticks = Some(v);
        self
    }

    /// Post-breeding cooldown in ticks.
    pub fn breed_cooldown_ticks(mut self, v: u32) -> Self {
        self.breed_cooldown_ticks = Some(v);
        self
    }

    /// Include `Wander` in the command cycle.
    pub fn supports_wander_mode(mut self, v: bool) -> Self {
        self.supports_wander_mode = v;
        self
    }

    /// Shoulder-perch flag.
    pub fn can_shoulder_perch(mut self, v: bool) -> Self {
        self.can_shoulder_perch = v;
        self
    }

    /// Mountable once tamed.
    pub fn rideable_when_tamed(mut self, v: bool) -> Self {
        self.rideable_when_tamed = v;
        self
    }

    /// Visual growth stages (`1..=5`; extended record).
    pub fn growth_stages(mut self, v: u8) -> Self {
        self.growth_stages = Some(v);
        self
    }

    /// Elemental breath weapon (extended record).
    pub fn elemental_breath(mut self, v: ElementalBreath) -> Self {
        self.elemental_breath = Some(v);
        self
    }

    /// Validate into a [`CreatureSpec`].
    pub fn build(self, default_ns: &str) -> Result<CreatureSpec, SdkDiagnostic> {
        const CTX: &str = "creature";
        let entity_type = qualify_decl_id(&self.id, default_ns, CTX)?;
        if !self.max_health.is_finite() || self.max_health <= 0.0 {
            return Err(range_diag(CTX, "max_health", "> 0"));
        }
        let speed_bad = |v: f64| !v.is_finite() || v < 0.0;
        match self.movement {
            Movement::Land { ground_speed } if speed_bad(ground_speed) => {
                return Err(range_diag(CTX, "movement.ground_speed", ">= 0"));
            }
            Movement::Flying {
                ground_speed,
                fly_speed,
                glide_fall_scale,
            } => {
                if speed_bad(ground_speed) || speed_bad(fly_speed) || speed_bad(glide_fall_scale)
                {
                    return Err(range_diag(CTX, "movement speeds", ">= 0"));
                }
            }
            Movement::Aquatic {
                swim_speed,
                suffocates_in_air_ticks,
            } => {
                if speed_bad(swim_speed) {
                    return Err(range_diag(CTX, "movement.swim_speed", ">= 0"));
                }
                if suffocates_in_air_ticks == 0 {
                    return Err(range_diag(
                        CTX,
                        "movement.suffocates_in_air_ticks",
                        ">= 1",
                    ));
                }
            }
            _ => {}
        }
        if !(0.0..=1.0).contains(&self.tame_chance) {
            return Err(range_diag(CTX, "tame_chance", "0.0..=1.0"));
        }
        if let Some(stages) = self.growth_stages {
            if !(1..=5).contains(&stages) {
                return Err(range_diag(CTX, "growth_stages", "1..=5"));
            }
        }
        for (name, ticks) in [
            ("baby_growth_ticks", self.baby_growth_ticks),
            ("breed_cooldown_ticks", self.breed_cooldown_ticks),
        ] {
            if let Some(t) = ticks {
                if t == 0 {
                    return Err(range_diag(CTX, name, ">= 1"));
                }
            }
        }
        let tame_items = self
            .tame_items
            .iter()
            .map(|i| normalize_ref(i, &format!("{CTX}:{entity_type}.tame_items")))
            .collect::<Result<_, _>>()?;
        let breed_items = self
            .breed_items
            .iter()
            .map(|i| normalize_ref(i, &format!("{CTX}:{entity_type}.breed_items")))
            .collect::<Result<_, _>>()?;
        let mut heal_items = Vec::with_capacity(self.heal_items.len());
        for (item, heal) in &self.heal_items {
            if !heal.is_finite() || *heal < 0.0 {
                return Err(range_diag(CTX, "heal_items.heal", ">= 0"));
            }
            heal_items.push((
                normalize_ref(item, &format!("{CTX}:{entity_type}.heal_items"))?,
                *heal,
            ));
        }
        Ok(CreatureSpec {
            entity_type,
            max_health: self.max_health,
            movement: self.movement,
            tame_items,
            tame_chance: self.tame_chance,
            breed_items,
            heal_items,
            baby_growth_ticks: self.baby_growth_ticks,
            breed_cooldown_ticks: self.breed_cooldown_ticks,
            supports_wander_mode: self.supports_wander_mode,
            can_shoulder_perch: self.can_shoulder_perch,
            rideable_when_tamed: self.rideable_when_tamed,
            growth_stages: self.growth_stages,
            elemental_breath: self.elemental_breath,
        })
    }
}

/// Fluent dimension declaration (vanilla `dimension_type` bounds).
#[derive(Clone, Debug)]
pub struct DimensionBuilder {
    id: String,
    min_y: i32,
    height: u32,
    has_skylight: bool,
    coordinate_scale: f64,
    ambient_light: f64,
}

impl DimensionBuilder {
    /// New dimension with overworld-like defaults (`min_y = -64`,
    /// `height = 384`, skylight, scale 1.0).
    pub fn new(id: &str) -> Self {
        Self {
            id: id.to_string(),
            min_y: -64,
            height: 384,
            has_skylight: true,
            coordinate_scale: 1.0,
            ambient_light: 0.0,
        }
    }

    /// Bottom build height (multiple of 16, `[-2032, 2031]`).
    pub fn min_y(mut self, v: i32) -> Self {
        self.min_y = v;
        self
    }

    /// Build height (multiple of 16, `[16, 4064]`).
    pub fn height(mut self, v: u32) -> Self {
        self.height = v;
        self
    }

    /// Sky visibility.
    pub fn has_skylight(mut self, v: bool) -> Self {
        self.has_skylight = v;
        self
    }

    /// Coordinate scale (`8.0` = nether-like).
    pub fn coordinate_scale(mut self, v: f64) -> Self {
        self.coordinate_scale = v;
        self
    }

    /// Constant ambient light (`0.0..=1.0`).
    pub fn ambient_light(mut self, v: f64) -> Self {
        self.ambient_light = v;
        self
    }

    /// Validate into a [`DimensionSpec`] under vanilla `dimension_type` bounds.
    pub fn build(self, default_ns: &str) -> Result<DimensionSpec, SdkDiagnostic> {
        const CTX: &str = "dimension";
        let id = qualify_decl_id(&self.id, default_ns, CTX)?;
        if self.min_y % 16 != 0 || !(-2032..=2031).contains(&self.min_y) {
            return Err(mk(
                "E_DIMENSION",
                format!("{CTX}:{id}.min_y"),
                format!("min_y {} must be a multiple of 16 in [-2032, 2031]", self.min_y),
                "pick a chunk-aligned bottom (e.g. -64, 0)",
            ));
        }
        if !self.height.is_multiple_of(16) || !(16..=4064).contains(&self.height) {
            return Err(mk(
                "E_DIMENSION",
                format!("{CTX}:{id}.height"),
                format!(
                    "height {} must be a multiple of 16 in [16, 4064]",
                    self.height
                ),
                "pick a chunk-aligned height (e.g. 256, 384)",
            ));
        }
        if i64::from(self.min_y) + i64::from(self.height) > 4064 {
            return Err(mk(
                "E_DIMENSION",
                format!("{CTX}:{id}"),
                "min_y + height exceeds the 4064 build limit".to_string(),
                "lower min_y or shrink height",
            ));
        }
        if !(1.0e-5..=3.0e7).contains(&self.coordinate_scale) {
            return Err(mk(
                "E_DIMENSION",
                format!("{CTX}:{id}.coordinate_scale"),
                format!(
                    "coordinate_scale {} must lie in [1e-5, 3e7]",
                    self.coordinate_scale
                ),
                "use 1.0 for overworld-like, 8.0 for nether-like travel",
            ));
        }
        if !(0.0..=1.0).contains(&self.ambient_light) {
            return Err(mk(
                "E_DIMENSION",
                format!("{CTX}:{id}.ambient_light"),
                format!("ambient_light {} must lie in [0.0, 1.0]", self.ambient_light),
                "0.0 = vanilla lighting; 1.0 = full brightness",
            ));
        }
        Ok(DimensionSpec {
            id,
            min_y: self.min_y,
            height: self.height,
            has_skylight: self.has_skylight,
            coordinate_scale: self.coordinate_scale,
            ambient_light: self.ambient_light,
        })
    }
}

/// Fluent locale file builder (`assets/<mod>/lang/<locale>.json`).
#[derive(Clone, Debug)]
pub struct LocaleBuilder {
    locale: String,
    entries: BTreeMap<String, String>,
    /// Deferred `block.`/`item.` translations keyed by raw ids, qualified
    /// with the project namespace at build time.
    pending: Vec<(&'static str, String, String)>,
}

impl LocaleBuilder {
    /// `en_us` locale.
    pub fn en_us() -> Self {
        Self::new("en_us")
    }

    /// `ja_jp` locale.
    pub fn ja_jp() -> Self {
        Self::new("ja_jp")
    }

    /// Any `[a-z]+_[a-z]+` locale code.
    pub fn new(locale: &str) -> Self {
        Self {
            locale: locale.to_string(),
            entries: BTreeMap::new(),
            pending: Vec::new(),
        }
    }

    /// Add a raw translation key (`block.<ns>.<id>`, `item.<ns>.<id>`,
    /// `gui.<ns>.title`, …).
    pub fn add(mut self, key: &str, value: &str) -> Self {
        self.entries.insert(key.to_string(), value.to_string());
        self
    }

    /// Translate a block: `block.<ns>.<path> = name`. `id` may be bare.
    pub fn translate_block(mut self, id: &str, name: &str) -> Self {
        self.pending
            .push(("block", id.to_string(), name.to_string()));
        self
    }

    /// Translate an item: `item.<ns>.<path> = name`. `id` may be bare.
    pub fn translate_item(mut self, id: &str, name: &str) -> Self {
        self.pending
            .push(("item", id.to_string(), name.to_string()));
        self
    }

    /// Validate into a [`LocaleSpec`].
    pub fn build(self, default_ns: &str) -> Result<LocaleSpec, SdkDiagnostic> {
        const CTX: &str = "locale";
        let parts: Vec<&str> = self.locale.split('_').collect();
        let valid = parts.len() == 2
            && parts
                .iter()
                .all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_lowercase()));
        if !valid {
            return Err(mk(
                "E_LOCALE",
                CTX,
                format!("locale code {:?} must look like 'en_us'", self.locale),
                "use a lowercase '<lang>_<region>' code",
            ));
        }
        let mut entries = self.entries;
        for key in entries.keys() {
            if key.is_empty() || key.chars().any(|c| c.is_control()) {
                return Err(mk(
                    "E_LOCALE",
                    format!("{CTX}:{}", self.locale),
                    format!("translation key {key:?} is empty or contains control characters"),
                    "use dotted keys like 'block.<mod>.<id>'",
                ));
            }
        }
        for (prefix, raw_id, name) in &self.pending {
            let qid = qualify_decl_id(raw_id, default_ns, &format!("{CTX}:{}", self.locale))?;
            let (ns, path) = qid.split_once(':').unwrap_or((default_ns, raw_id));
            entries.insert(format!("{prefix}.{ns}.{path}"), name.clone());
        }
        Ok(LocaleSpec {
            locale: self.locale,
            entries,
        })
    }
}

fn range_diag(ctx: &str, field: &str, bounds: &str) -> SdkDiagnostic {
    mk(
        "E_RANGE",
        format!("{ctx}.{field}"),
        format!("{field} must be {bounds}"),
        format!("set {field} within {bounds}"),
    )
}

// ---------------------------------------------------------------------------
// Project builder
// ---------------------------------------------------------------------------

/// The entry-point builder: validates the mod id eagerly at
/// [`build`](Self::build), collects every spec, and returns either a fully
/// validated [`ModProject`] or **all** diagnostics found in one pass.
#[derive(Clone, Debug)]
pub struct ModProjectBuilder {
    mod_id: String,
    version: String,
    display_name: Option<String>,
    mc_version_range: Option<String>,
    dependencies: Vec<DependencySpec>,
    blocks: Vec<BlockBuilder>,
    items: Vec<ItemBuilder>,
    recipes: Vec<RecipeBuilder>,
    tags: Vec<TagBuilder>,
    creatures: Vec<CreatureBuilder>,
    dimensions: Vec<DimensionBuilder>,
    locales: Vec<LocaleBuilder>,
}

impl ModProjectBuilder {
    /// Start a mod. `mod_id` must match `[a-z0-9_.-]+` (also used as the
    /// default declaration namespace); `version` is any non-empty string.
    /// Both are validated strictly at [`build`](Self::build).
    pub fn new(mod_id: &str, version: &str) -> Self {
        Self {
            mod_id: mod_id.to_string(),
            version: version.to_string(),
            display_name: None,
            mc_version_range: None,
            dependencies: Vec::new(),
            blocks: Vec::new(),
            items: Vec::new(),
            recipes: Vec::new(),
            tags: Vec::new(),
            creatures: Vec::new(),
            dimensions: Vec::new(),
            locales: Vec::new(),
        }
    }

    /// Human-readable mod name (defaults to `mod_id`).
    pub fn display_name(mut self, name: &str) -> Self {
        self.display_name = Some(name.to_string());
        self
    }

    /// Minecraft compatibility range — Maven (`"[1.21.1,1.22)"`) or
    /// SemVer (`">=1.21.1 <1.22"`) syntax. Defaults to `"[1.21,)"`.
    pub fn mc_version_range(mut self, range: &str) -> Self {
        self.mc_version_range = Some(range.to_string());
        self
    }

    /// Declare a dependency on `dep_id` (`required` = hard requirement).
    pub fn dependency(mut self, dep_id: &str, version_range: &str, required: bool) -> Self {
        self.dependencies.push(DependencySpec {
            mod_id: dep_id.to_string(),
            version_range: version_range.to_string(),
            required,
        });
        self
    }

    /// Add a block declaration.
    pub fn block(mut self, b: BlockBuilder) -> Self {
        self.blocks.push(b);
        self
    }

    /// Add an item declaration.
    pub fn item(mut self, i: ItemBuilder) -> Self {
        self.items.push(i);
        self
    }

    /// Add a recipe declaration.
    pub fn recipe(mut self, r: RecipeBuilder) -> Self {
        self.recipes.push(r);
        self
    }

    /// Add a tag file.
    pub fn tag(mut self, t: TagBuilder) -> Self {
        self.tags.push(t);
        self
    }

    /// Add a creature declaration.
    pub fn creature(mut self, c: CreatureBuilder) -> Self {
        self.creatures.push(c);
        self
    }

    /// Add a dimension declaration.
    pub fn dimension(mut self, d: DimensionBuilder) -> Self {
        self.dimensions.push(d);
        self
    }

    /// Add a locale file.
    pub fn locale(mut self, l: LocaleBuilder) -> Self {
        self.locales.push(l);
        self
    }

    /// Validate everything into a [`ModProject`], collecting **all**
    /// diagnostics (spec errors, project-level id/duplicates, and the full
    /// [`crate::test_harness::validate_project`] battery) in one pass.
    ///
    /// # Errors
    ///
    /// Every [`SdkDiagnostic`] found, in declaration order — fixable in one
    /// read by humans or agents.
    pub fn build(self) -> Result<ModProject, Vec<SdkDiagnostic>> {
        let mut diags = Vec::new();
        if let Err(why) = check_mod_id(&self.mod_id) {
            diags.push(mk(
                "E_MOD_ID",
                "mod_id",
                why,
                "use a lowercase [a-z0-9_.-] mod id (e.g. 'my_mod')",
            ));
        }
        if self.version.is_empty() || self.version.chars().any(|c| c.is_control()) {
            diags.push(mk(
                "E_VERSION",
                "version",
                "version must be non-empty free text".to_string(),
                "pass a semver-ish version like '1.0.0'",
            ));
        }
        let mc_range = self
            .mc_version_range
            .unwrap_or_else(|| DEFAULT_MC_RANGE.to_string());
        if let Err(why) = check_version_range(&mc_range) {
            diags.push(mk(
                "E_VERSION_RANGE",
                "mc_version_range",
                why,
                "use Maven '[1.21.1,1.22)' or SemVer '>=1.21.1 <1.22' syntax",
            ));
        }
        for (idx, dep) in self.dependencies.iter().enumerate() {
            if let Err(why) = check_mod_id(&dep.mod_id) {
                diags.push(mk(
                    "E_MOD_ID",
                    format!("dependencies[{idx}]"),
                    why,
                    "dependency mod ids follow the same [a-z0-9_.-] rules",
                ));
            }
            if let Err(why) = check_version_range(&dep.version_range) {
                diags.push(mk(
                    "E_VERSION_RANGE",
                    format!("dependencies[{idx}].version_range"),
                    why,
                    "use a Maven or SemVer range",
                ));
            }
        }

        let display_name = self.display_name.unwrap_or_else(|| self.mod_id.clone());
        let mut project = ModProject {
            mod_id: self.mod_id,
            version: self.version,
            display_name,
            mc_version_range: mc_range,
            dependencies: self.dependencies,
            blocks: Vec::new(),
            items: Vec::new(),
            recipes: Vec::new(),
            tags: Vec::new(),
            creatures: Vec::new(),
            dimensions: Vec::new(),
            locales: Vec::new(),
        };
        let ns = project.mod_id.as_str();
        for b in self.blocks {
            match b.build(ns) {
                Ok(spec) => project.blocks.push(spec),
                Err(d) => diags.push(d),
            }
        }
        for i in self.items {
            match i.build(ns) {
                Ok(spec) => project.items.push(spec),
                Err(d) => diags.push(d),
            }
        }
        for r in self.recipes {
            match r.build(ns) {
                Ok(spec) => project.recipes.push(spec),
                Err(d) => diags.push(d),
            }
        }
        for t in self.tags {
            match t.build(ns) {
                Ok(spec) => project.tags.push(spec),
                Err(d) => diags.push(d),
            }
        }
        for c in self.creatures {
            match c.build(ns) {
                Ok(spec) => project.creatures.push(spec),
                Err(d) => diags.push(d),
            }
        }
        for d in self.dimensions {
            match d.build(ns) {
                Ok(spec) => project.dimensions.push(spec),
                Err(d) => diags.push(d),
            }
        }
        for l in self.locales {
            match l.build(ns) {
                Ok(spec) => project.locales.push(spec),
                Err(d) => diags.push(d),
            }
        }

        check_duplicates(
            project.blocks.iter().map(|b| b.id.as_str()),
            "blocks",
            &mut diags,
        );
        check_duplicates(
            project.items.iter().map(|i| i.id.as_str()),
            "items",
            &mut diags,
        );
        check_duplicates(
            project.recipes.iter().map(|r| r.id.as_str()),
            "recipes",
            &mut diags,
        );
        let tag_keys: Vec<String> = project
            .tags
            .iter()
            .map(|t| format!("{}:{}", t.kind.dir(), t.id))
            .collect();
        check_duplicates(tag_keys.iter().map(String::as_str), "tags", &mut diags);
        check_duplicates(
            project.creatures.iter().map(|c| c.entity_type.as_str()),
            "creatures",
            &mut diags,
        );
        check_duplicates(
            project.dimensions.iter().map(|d| d.id.as_str()),
            "dimensions",
            &mut diags,
        );
        check_duplicates(
            project.locales.iter().map(|l| l.locale.as_str()),
            "locales",
            &mut diags,
        );

        diags.extend(crate::test_harness::validate_project(&project));
        // The battery re-checks project-level fields already diagnosed
        // above — collapse identical (code, path, message) triples.
        let mut seen = std::collections::BTreeSet::new();
        diags.retain(|d| seen.insert((d.code, d.path.clone(), d.message.clone())));
        if diags.is_empty() {
            Ok(project)
        } else {
            Err(diags)
        }
    }

    /// Build the project then emit its `megu-mod-bundle/1` document.
    ///
    /// # Errors
    ///
    /// `SdkError::Diagnostics` wrapping every diagnostic [`build`](Self::build)
    /// or the bundle export produces.
    pub fn export_megu_bundle_json(&self) -> Result<String, SdkError> {
        self.clone().build().map_err(SdkError::Diagnostics)?.export_megu_bundle_json()
    }
}

fn check_duplicates<'a>(
    ids: impl Iterator<Item = &'a str>,
    section: &str,
    diags: &mut Vec<SdkDiagnostic>,
) {
    let mut seen = std::collections::BTreeSet::new();
    for id in ids {
        if !seen.insert(id.to_string()) {
            diags.push(mk(
                "E_DUPLICATE_ID",
                section,
                format!("duplicate id '{id}' in {section}"),
                "rename or remove the duplicate declaration",
            ));
        }
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;

    /// A complete sample mod exercising every SDK feature: two blocks (one
    /// with a capability), two items, five recipes of every kind, item and
    /// block tags with a nested `#` reference, one creature, one dimension,
    /// EN + JA locales, and a dependency.
    pub(crate) fn sample_project() -> ModProject {
        ModProjectBuilder::new("horizon_demo", "1.4.2")
            .display_name("Horizon Demo")
            .mc_version_range("[1.21.1,1.22)")
            .dependency("create", "[0.5.1,)", false)
            .block(
                BlockBuilder::new("brass_block")
                    .hardness(3.5)
                    .blast_resistance(6.0)
                    .light_level(7)
                    .requires_tool(true)
                    .loot_drop("horizon_demo:brass_ingot")
                    .tag("mineable/pickaxe")
                    .tag("c:storage_blocks/brass")
                    .capability(CapabilitySpec::ItemHandler { slots: 9 }),
            )
            .block(
                BlockBuilder::new("sky_forge")
                    .hardness(4.0)
                    .blast_resistance(12.0)
                    .requires_tool(true)
                    .tag("mineable/pickaxe")
                    .capability(CapabilitySpec::EnergyStorage {
                        capacity_fe: 100_000,
                        max_io_fe: 1_000,
                    }),
            )
            .block(
                BlockBuilder::new("brass_ore")
                    .hardness(3.0)
                    .requires_tool(true)
                    .loot_drop("horizon_demo:brass_ingot")
                    .tag("mineable/pickaxe")
                    .tag("c:ores"),
            )
            .item(
                ItemBuilder::new("brass_ingot")
                    .max_stack(64)
                    .rarity(Rarity::Uncommon)
                    .tag("c:ingots/brass"),
            )
            .item(ItemBuilder::new("brass_sheet").max_stack(64))
            .item(
                ItemBuilder::new("storm_blade")
                    .max_stack(1)
                    .durability(1561)
                    .attack_damage(7.5)
                    .rarity(Rarity::Epic),
            )
            .item(
                ItemBuilder::new("skyberry")
                    .max_stack(16)
                    .food(4, 0.3),
            )
            .recipe(RecipeBuilder::shaped(
                "brass_block",
                &["###", "###", "###"],
                &[('#', "horizon_demo:brass_ingot")],
                "horizon_demo:brass_block",
            ))
            .recipe(RecipeBuilder::shapeless(
                "brass_ingot_from_block",
                &["horizon_demo:brass_block"],
                "horizon_demo:brass_ingot",
            ))
            .recipe(RecipeBuilder::smelting(
                "brass_ingot_smelting",
                "#c:ores/brass",
                "horizon_demo:brass_ingot",
                0.7,
                200,
            ))
            .recipe(RecipeBuilder::pressing(
                "brass_sheet",
                "horizon_demo:brass_ingot",
                "horizon_demo:brass_sheet",
            ))
            .recipe(
                RecipeBuilder::alloy_forge(
                    "storm_alloy",
                    "horizon_demo:brass_ingot",
                    "#c:gems/sky",
                    "lightning",
                    400,
                    "horizon_demo:storm_blade",
                )
                .result_count(1),
            )
            .tag(
                TagBuilder::item("c:ingots/brass")
                    .add("horizon_demo:brass_ingot"),
            )
            .tag(
                TagBuilder::item("c:ores/brass")
                    .add("horizon_demo:brass_ore")
                    .add_tag("#c:ores"),
            )
            .tag(
                TagBuilder::block("mineable/pickaxe")
                    .add("horizon_demo:brass_block")
                    .add("horizon_demo:sky_forge"),
            )
            .creature(
                CreatureBuilder::new("sky_drake")
                    .max_health(80.0)
                    .movement(Movement::flying())
                    .tame_item("#c:raw_fishes")
                    .tame_chance(0.1)
                    .breed_item("horizon_demo:skyberry")
                    .heal_item("horizon_demo:skyberry", 4.0)
                    .supports_wander_mode(true)
                    .rideable_when_tamed(true)
                    .growth_stages(3)
                    .elemental_breath(ElementalBreath::Lightning),
            )
            .dimension(
                DimensionBuilder::new("sky_realm")
                    .min_y(-64)
                    .height(384)
                    .has_skylight(true)
                    .coordinate_scale(1.0)
                    .ambient_light(0.0),
            )
            .locale(
                LocaleBuilder::en_us()
                    .translate_block("brass_block", "Block of Brass")
                    .translate_block("sky_forge", "Sky Forge")
                    .translate_item("brass_ingot", "Brass Ingot")
                    .translate_item("storm_blade", "Storm Blade")
                    .translate_item("skyberry", "Skyberry")
                    .add("itemGroup.horizon_demo.main", "Horizon Demo"),
            )
            .locale(
                LocaleBuilder::ja_jp()
                    .translate_block("brass_block", "真鍮ブロック")
                    .translate_item("brass_ingot", "真鍮インゴット"),
            )
            .build()
            .expect("sample project must validate")
    }
}
