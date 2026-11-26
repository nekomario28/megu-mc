// SPDX-License-Identifier: LGPL-3.0-or-later
//! Vanilla item defaults and worldgen compatibility per target release.
//!
//! Sources are the Mojang 1.21.1 and 26.3 server JARs (SHA-1
//! `59353fb40c36d304f2035d51e7d6e6baa98dc05c` and
//! `33680f5f2ac32864d6d7cf5e56a705fdb3e05f4c`). Dimension bounds are
//! from the built-in dimension types. Item data comes from the default
//! components and the furnace fuel registry or cooking fuel providers.

use std::collections::BTreeMap;
use std::sync::LazyLock;

use crate::dye_and_items::DyeColor;
use crate::hash::Fnv1a64;
use crate::version::TargetVersion;

/// The worldgen compatibility identifier pinned when a world is created.
pub const fn worldgen_epoch_id(version: TargetVersion) -> &'static str {
    match version {
        TargetVersion::V1_21_1 => "mc-1.21.1-v1",
        TargetVersion::V26_3 => "mc-26.3-v1",
    }
}

/// Vertical build and storage bounds, measured in blocks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DimensionBounds {
    pub min_y: i32,
    pub height: u32,
}

/// Worldgen identity and defaults for the three built-in dimensions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WorldgenEpochSpec {
    pub id: &'static str,
    pub overworld: DimensionBounds,
    pub nether: DimensionBounds,
    pub end: DimensionBounds,
    pub data_version: i32,
    pub protocol_version: i32,
    /// Datapack format as a (major, minor) pair.
    pub pack_format: (u32, u32),
}

impl WorldgenEpochSpec {
    pub const fn for_version(version: TargetVersion) -> Self {
        let overworld = DimensionBounds {
            min_y: -64,
            height: 384,
        };
        let nether_and_end = DimensionBounds {
            min_y: 0,
            height: 256,
        };
        Self {
            id: worldgen_epoch_id(version),
            overworld,
            nether: nether_and_end,
            end: nether_and_end,
            data_version: version.data_version(),
            protocol_version: version.protocol(),
            pack_format: version.data_pack_format(),
        }
    }
}

/// Capabilities of the target's vanilla gameplay and data format.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum VersionFeature {
    DataComponents = 1 << 0,
    TrialChambers = 1 << 1,
    BundleDyeing = 1 << 2,
    /// Dimension types can exceed 256 blocks in height.
    TallDimensionSupport = 1 << 3,
    /// Consumption time and container remainder are separate from food stats.
    SeparateConsumableComponent = 1 << 4,
    CookingFuelComponent = 1 << 5,
}

/// Default food-component stats and consumption metadata for a vanilla item.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FoodFact {
    /// Bare vanilla item ID.
    pub item_id: &'static str,
    /// Nutrition recorded in the default food component.
    pub nutrition: u8,
    /// The vanilla builder's modifier. Saturation gain is twice nutrition
    /// times the modifier when the item applies its food component.
    pub saturation_modifier: f32,
    /// Whether the item can be consumed with a full hunger bar.
    pub can_always_eat: bool,
    pub eat_ticks: u16,
    /// Bare ID of the container left by survival consumption, if any.
    pub converts_to: Option<&'static str>,
}

impl FoodFact {
    const fn new(
        item_id: &'static str,
        nutrition: u8,
        saturation_modifier: f32,
        can_always_eat: bool,
        eat_ticks: u16,
        converts_to: Option<&'static str>,
    ) -> Self {
        Self {
            item_id,
            nutrition,
            saturation_modifier,
            can_always_eat,
            eat_ticks,
            converts_to,
        }
    }

    const fn standard(item_id: &'static str, nutrition: u8, saturation_modifier: f32) -> Self {
        Self::new(item_id, nutrition, saturation_modifier, false, 32, None)
    }
}

/// Default burn duration in a regular furnace.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FuelFact {
    /// Bare vanilla item ID.
    pub item_id: Box<str>,
    pub burn_ticks: u16,
}

/// The immutable vanilla defaults for one target.
#[derive(Debug)]
pub struct GameplayFactPack {
    epoch: WorldgenEpochSpec,
    features: u8,
    foods: BTreeMap<&'static str, &'static FoodFact>,
    fuels: Vec<FuelFact>,
    fingerprint: u64,
}

impl GameplayFactPack {
    /// Select the shared pack, expanding the family tables once per version.
    pub fn for_version(version: TargetVersion) -> &'static Self {
        static V1_21_1: LazyLock<GameplayFactPack> =
            LazyLock::new(|| GameplayFactPack::build(TargetVersion::V1_21_1));
        static V26_3: LazyLock<GameplayFactPack> =
            LazyLock::new(|| GameplayFactPack::build(TargetVersion::V26_3));

        match version {
            TargetVersion::V1_21_1 => &V1_21_1,
            TargetVersion::V26_3 => &V26_3,
        }
    }

    pub const fn worldgen_epoch(&self) -> &WorldgenEpochSpec {
        &self.epoch
    }

    pub const fn supports_feature(&self, feature: VersionFeature) -> bool {
        self.features & feature as u8 != 0
    }

    /// The default food fact for a bare or `minecraft:`-qualified item ID.
    pub fn food_by_name(&self, name: &str) -> Option<&'static FoodFact> {
        self.foods.get(vanilla_path(name)).copied()
    }

    /// Default food facts in item ID order.
    pub fn food_facts(&self) -> impl ExactSizeIterator<Item = &'static FoodFact> + '_ {
        self.foods.values().copied()
    }

    /// Regular furnace burn ticks for a bare or `minecraft:`-qualified ID.
    pub fn fuel_ticks_by_name(&self, name: &str) -> Option<u16> {
        let path = vanilla_path(name);
        let index = self
            .fuels
            .binary_search_by(|fact| fact.item_id.as_ref().cmp(path))
            .ok()?;
        Some(self.fuels[index].burn_ticks)
    }

    /// Default furnace fuel facts in item ID order.
    pub fn fuel_facts(&self) -> &[FuelFact] {
        &self.fuels
    }

    /// FNV-1a over the epoch, features, and resolved tables in item ID order.
    pub const fn content_fingerprint(&self) -> u64 {
        self.fingerprint
    }

    fn build(version: TargetVersion) -> Self {
        let epoch = WorldgenEpochSpec::for_version(version);
        let mut features = VersionFeature::DataComponents as u8
            | VersionFeature::TrialChambers as u8
            | VersionFeature::TallDimensionSupport as u8;
        let mut foods: BTreeMap<_, _> = FOOD_V1_21_1
            .iter()
            .map(|food| (food.item_id, food))
            .collect();
        let mut fuels = fuels_v1_21_1();
        if version == TargetVersion::V26_3 {
            features |= VersionFeature::BundleDyeing as u8
                | VersionFeature::SeparateConsumableComponent as u8
                | VersionFeature::CookingFuelComponent as u8;
            for (id, replacement) in FOOD_OVERRIDES_V26_3 {
                match replacement {
                    Some(food) => foods.insert(id, food),
                    None => foods.remove(id),
                };
            }
            add_fuels_v26_3(&mut fuels);
        }
        fuels.sort_unstable_by(|left, right| left.item_id.cmp(&right.item_id));
        let fingerprint = fingerprint(&epoch, features, foods.values().copied(), &fuels);
        Self {
            epoch,
            features,
            foods,
            fuels,
            fingerprint,
        }
    }
}

fn vanilla_path(name: &str) -> &str {
    name.strip_prefix("minecraft:").unwrap_or(name)
}

// Most items use the food component directly. Honey and ominous bottles
// bypass the hunger check. Honey supplies a glass bottle in item code.
const FOOD_V1_21_1: &[FoodFact] = &[
    FoodFact::standard("apple", 4, 0.3),
    FoodFact::standard("baked_potato", 5, 0.6),
    FoodFact::standard("beef", 3, 0.3),
    FoodFact::standard("beetroot", 1, 0.6),
    FoodFact::new("beetroot_soup", 6, 0.6, false, 32, Some("bowl")),
    FoodFact::standard("bread", 5, 0.6),
    FoodFact::standard("carrot", 3, 0.6),
    FoodFact::standard("chicken", 2, 0.3),
    FoodFact::new("chorus_fruit", 4, 0.3, true, 32, None),
    FoodFact::standard("cod", 2, 0.1),
    FoodFact::standard("cooked_beef", 8, 0.8),
    FoodFact::standard("cooked_chicken", 6, 0.6),
    FoodFact::standard("cooked_cod", 5, 0.6),
    FoodFact::standard("cooked_mutton", 6, 0.8),
    FoodFact::standard("cooked_porkchop", 8, 0.8),
    FoodFact::standard("cooked_rabbit", 5, 0.6),
    FoodFact::standard("cooked_salmon", 6, 0.8),
    FoodFact::standard("cookie", 2, 0.1),
    FoodFact::new("dried_kelp", 1, 0.3, false, 16, None),
    FoodFact::new("enchanted_golden_apple", 4, 1.2, true, 32, None),
    FoodFact::standard("glow_berries", 2, 0.1),
    FoodFact::new("golden_apple", 4, 1.2, true, 32, None),
    FoodFact::standard("golden_carrot", 6, 1.2),
    FoodFact::new("honey_bottle", 6, 0.1, true, 40, Some("glass_bottle")),
    FoodFact::standard("melon_slice", 2, 0.3),
    FoodFact::new("mushroom_stew", 6, 0.6, false, 32, Some("bowl")),
    FoodFact::standard("mutton", 2, 0.3),
    // The default component exists, but the drink's completion path only
    // applies Bad Omen and consumes the stack item.
    FoodFact::new("ominous_bottle", 1, 0.1, true, 32, None),
    FoodFact::standard("poisonous_potato", 2, 0.3),
    FoodFact::standard("porkchop", 3, 0.3),
    FoodFact::standard("potato", 1, 0.3),
    FoodFact::standard("pufferfish", 1, 0.1),
    FoodFact::standard("pumpkin_pie", 8, 0.3),
    FoodFact::standard("rabbit", 3, 0.3),
    FoodFact::new("rabbit_stew", 10, 0.6, false, 32, Some("bowl")),
    FoodFact::standard("rotten_flesh", 4, 0.1),
    FoodFact::standard("salmon", 2, 0.1),
    FoodFact::standard("spider_eye", 2, 0.8),
    FoodFact::new("suspicious_stew", 6, 0.6, true, 32, Some("bowl")),
    FoodFact::standard("sweet_berries", 2, 0.1),
    FoodFact::standard("tropical_fish", 1, 0.1),
];

// Ominous bottles now have only a consumable component. Fish buckets have
// food stats for other mechanics and cannot be consumed by a player.
const FOOD_OVERRIDES_V26_3: &[(&str, Option<FoodFact>)] = &[("ominous_bottle", None)];

const WOODS_V1_21_1: &[&str] = &[
    "acacia", "birch", "cherry", "dark_oak", "jungle", "mangrove", "oak", "spruce",
];
const WOODS_ADDED_V26_3: &[&str] = &["pale_oak", "poplar"];

const WOOD_PRODUCTS: &[(&str, u16)] = &[
    ("_button", 100),
    ("_door", 200),
    ("_fence", 300),
    ("_fence_gate", 300),
    ("_hanging_sign", 800),
    ("_planks", 300),
    ("_pressure_plate", 300),
    ("_sign", 200),
    ("_slab", 150),
    ("_stairs", 300),
    ("_trapdoor", 300),
];

const EXACT_FUELS_V1_21_1: &[(u16, &[&str])] = &[
    (50, &["bamboo", "scaffolding"]),
    (
        100,
        &[
            "azalea",
            "bowl",
            "dead_bush",
            "flowering_azalea",
            "mangrove_propagule",
            "stick",
        ],
    ),
    (
        200,
        &[
            "wooden_axe",
            "wooden_hoe",
            "wooden_pickaxe",
            "wooden_shovel",
            "wooden_sword",
        ],
    ),
    (
        300,
        &[
            "barrel",
            "bookshelf",
            "bow",
            "cartography_table",
            "chest",
            "chiseled_bookshelf",
            "composter",
            "crafting_table",
            "crossbow",
            "daylight_detector",
            "fishing_rod",
            "fletching_table",
            "jukebox",
            "ladder",
            "lectern",
            "loom",
            "mangrove_roots",
            "note_block",
            "smithing_table",
            "trapped_chest",
        ],
    ),
    (1_600, &["charcoal", "coal"]),
    (2_400, &["blaze_rod"]),
    (4_001, &["dried_kelp_block"]),
    (16_000, &["coal_block"]),
    (20_000, &["lava_bucket"]),
];

fn push_fuel(fuels: &mut Vec<FuelFact>, item_id: impl Into<Box<str>>, burn_ticks: u16) {
    fuels.push(FuelFact {
        item_id: item_id.into(),
        burn_ticks,
    });
}

fn add_fuel_family(fuels: &mut Vec<FuelFact>, stem: &str, products: &[(&str, u16)]) {
    for &(suffix, ticks) in products {
        push_fuel(fuels, format!("{stem}{suffix}"), ticks);
    }
}

fn add_wood_fuels(fuels: &mut Vec<FuelFact>, woods: &[&str]) {
    for &wood in woods {
        add_fuel_family(fuels, wood, WOOD_PRODUCTS);
        add_fuel_family(
            fuels,
            wood,
            &[
                ("_log", 300),
                ("_wood", 300),
                ("_boat", 1_200),
                ("_chest_boat", 1_200),
            ],
        );
        add_fuel_family(
            fuels,
            &format!("stripped_{wood}"),
            &[("_log", 300), ("_wood", 300)],
        );
        if wood != "mangrove" {
            add_fuel_family(fuels, wood, &[("_sapling", 100)]);
        }
    }
}

fn fuels_v1_21_1() -> Vec<FuelFact> {
    let mut fuels = Vec::new();
    for &(ticks, items) in EXACT_FUELS_V1_21_1 {
        for &item in items {
            push_fuel(&mut fuels, item, ticks);
        }
    }
    add_wood_fuels(&mut fuels, WOODS_V1_21_1);
    add_fuel_family(&mut fuels, "bamboo", WOOD_PRODUCTS);
    add_fuel_family(
        &mut fuels,
        "bamboo",
        &[("_block", 300), ("_raft", 1_200), ("_chest_raft", 1_200)],
    );
    push_fuel(&mut fuels, "stripped_bamboo_block", 300);
    add_fuel_family(
        &mut fuels,
        "bamboo_mosaic",
        &[("", 300), ("_stairs", 300), ("_slab", 150)],
    );
    for color in DyeColor::ALL {
        add_fuel_family(
            &mut fuels,
            color.name(),
            &[("_banner", 300), ("_carpet", 67), ("_wool", 100)],
        );
    }
    fuels
}

fn add_fuels_v26_3(fuels: &mut Vec<FuelFact>) {
    add_wood_fuels(fuels, WOODS_ADDED_V26_3);
    for &wood in WOODS_V1_21_1
        .iter()
        .chain(WOODS_ADDED_V26_3)
        .chain(&["bamboo"])
    {
        add_fuel_family(fuels, wood, &[("_shelf", 300)]);
    }
    for color in DyeColor::ALL {
        add_fuel_family(
            fuels,
            color.name(),
            &[("_cushion", 50), ("_wool_slab", 50), ("_wool_stairs", 100)],
        );
    }
    for item in ["leaf_litter", "short_dry_grass", "tall_dry_grass"] {
        push_fuel(fuels, item, 100);
    }
    push_fuel(fuels, "wooden_spear", 200);
}

fn fingerprint<'a>(
    epoch: &WorldgenEpochSpec,
    features: u8,
    foods: impl ExactSizeIterator<Item = &'a FoodFact>,
    fuels: &[FuelFact],
) -> u64 {
    let mut hash = Fnv1a64::new();
    hash.update(b"megu/gameplay-facts/v1\0");
    hash.update_str(epoch.id);
    hash.update_u8(0);
    for bounds in [epoch.overworld, epoch.nether, epoch.end] {
        hash.update_i32(bounds.min_y);
        hash.update_u32(bounds.height);
    }
    hash.update_i32(epoch.data_version);
    hash.update_i32(epoch.protocol_version);
    hash.update_u32(epoch.pack_format.0);
    hash.update_u32(epoch.pack_format.1);
    hash.update_u8(features);
    hash.update_u64(foods.len() as u64);
    for food in foods {
        hash.update_str(food.item_id);
        hash.update_u8(0);
        hash.update_u8(food.nutrition);
        hash.update_f32_bits(food.saturation_modifier);
        hash.update_u8(u8::from(food.can_always_eat));
        hash.update_u16(food.eat_ticks);
        hash.update_str(food.converts_to.unwrap_or_default());
        hash.update_u8(0);
    }
    hash.update_u64(fuels.len() as u64);
    for fuel in fuels {
        hash.update_str(&fuel.item_id);
        hash.update_u8(0);
        hash.update_u16(fuel.burn_ticks);
    }
    hash.finish()
}
