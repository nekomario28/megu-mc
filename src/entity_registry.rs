// Protocol 767, Minecraft Java 1.21.1. Values are extracted from the vanilla
// server EntityType registration and DefaultAttributes, including EntityDimensions
// eye heights, tracking ranges, update intervals, and fire immunity.
// Rows are indexed by protocol ID.

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum EntityMobCategory {
    Monster,
    Creature,
    Ambient,
    Axolotls,
    UndergroundWaterCreature,
    WaterCreature,
    WaterAmbient,
    Misc,
}

#[cfg_attr(not(test), allow(dead_code))]
impl EntityMobCategory {
    pub(crate) const fn max_instances_per_chunk(self) -> i32 {
        match self {
            Self::Monster => 70,
            Self::Creature => 10,
            Self::Ambient => 15,
            Self::Axolotls | Self::UndergroundWaterCreature | Self::WaterCreature => 5,
            Self::WaterAmbient => 20,
            Self::Misc => -1,
        }
    }

    pub(crate) const fn is_friendly(self) -> bool {
        !matches!(self, Self::Monster)
    }

    pub(crate) const fn is_persistent(self) -> bool {
        matches!(self, Self::Creature | Self::Misc)
    }

    pub(crate) const fn despawn_distance(self) -> i32 {
        if matches!(self, Self::WaterAmbient) { 64 } else { 128 }
    }
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Copy, Debug)]
pub(crate) struct EntitySpec {
    pub id: i64,
    pub name: &'static str,
    pub width: f32,
    pub height: f32,
    pub eye_height: f32,
    pub category: EntityMobCategory,
    pub max_health: f32,
    pub fire_immune: bool,
    pub summonable: bool,
    /// Default live-entity/vehicle collision behavior before runtime state checks.
    pub pushable: bool,
    /// Vanilla EntityType client tracking range, measured in chunks.
    pub client_tracking_range: i32,
    /// Vanilla EntityType update interval, measured in ticks.
    pub update_interval: i32,
}

macro_rules! entity_spec_1211 {
    ($id:literal, $name:literal, $width:expr, $height:expr, $eye_height:expr,
     $category:ident, $max_health:expr, $fire_immune:expr, $summonable:expr,
     $pushable:expr, $tracking_range:expr, $update_interval:expr) => {
        EntitySpec {
            id: $id,
            name: concat!("minecraft:", $name),
            width: $width,
            height: $height,
            eye_height: $eye_height,
            category: EntityMobCategory::$category,
            max_health: $max_health,
            fire_immune: $fire_immune,
            summonable: $summonable,
            pushable: $pushable,
            client_tracking_range: $tracking_range,
            update_interval: $update_interval,
        }
    };
}

#[cfg_attr(not(test), allow(dead_code))]
pub(crate) static ENTITY_REGISTRY_1211: [EntitySpec; 130] = [
    entity_spec_1211!(0, "allay", 0.35, 0.6, 0.36, Creature, 2e+01, false, true, true, 8, 2),
    entity_spec_1211!(1, "area_effect_cloud", 6.0, 0.5, 0.425, Misc, 0.0, true, true, false, 10, 2147483647),
    entity_spec_1211!(2, "armadillo", 0.7, 0.65, 0.26, Creature, 12.0, false, true, true, 10, 3),
    entity_spec_1211!(3, "armor_stand", 0.5, 1.975, 1.7775, Misc, 2e+01, false, true, false, 10, 3),
    entity_spec_1211!(4, "arrow", 0.5, 0.5, 0.13, Misc, 0.0, false, true, false, 4, 20),
    entity_spec_1211!(5, "axolotl", 0.75, 0.42, 0.2751, Axolotls, 14.0, false, true, true, 10, 3),
    entity_spec_1211!(6, "bat", 0.5, 0.9, 0.45, Ambient, 6.0, false, true, false, 5, 3),
    entity_spec_1211!(7, "bee", 0.7, 0.6, 0.3, Creature, 1e+01, false, true, true, 8, 3),
    entity_spec_1211!(8, "blaze", 0.6, 1.8, 1.53, Monster, 2e+01, true, true, true, 8, 3),
    entity_spec_1211!(9, "block_display", 0.0, 0.0, 0.0, Misc, 0.0, false, true, false, 10, 1),
    entity_spec_1211!(10, "boat", 1.375, 0.5625, 0.5625, Misc, 0.0, false, true, true, 10, 3),
    entity_spec_1211!(11, "bogged", 0.6, 1.99, 1.74, Monster, 16.0, false, true, true, 8, 3),
    entity_spec_1211!(12, "breeze", 0.6, 1.77, 1.3452, Monster, 3e+01, false, true, true, 10, 3),
    entity_spec_1211!(13, "breeze_wind_charge", 0.3125, 0.3125, 0.0, Misc, 0.0, false, true, false, 4, 10),
    entity_spec_1211!(14, "camel", 1.7, 2.375, 2.275, Creature, 32.0, false, true, true, 10, 3),
    entity_spec_1211!(15, "cat", 0.6, 0.7, 0.35, Creature, 1e+01, false, true, true, 8, 3),
    entity_spec_1211!(16, "cave_spider", 0.7, 0.5, 0.45, Monster, 12.0, false, true, true, 8, 3),
    entity_spec_1211!(17, "chest_boat", 1.375, 0.5625, 0.5625, Misc, 0.0, false, true, true, 10, 3),
    entity_spec_1211!(18, "chest_minecart", 0.98, 0.7, 0.595, Misc, 0.0, false, true, true, 8, 3),
    entity_spec_1211!(19, "chicken", 0.4, 0.7, 0.644, Creature, 4.0, false, true, true, 10, 3),
    entity_spec_1211!(20, "cod", 0.5, 0.3, 0.195, WaterAmbient, 3.0, false, true, true, 4, 3),
    entity_spec_1211!(21, "command_block_minecart", 0.98, 0.7, 0.595, Misc, 0.0, false, true, true, 8, 3),
    entity_spec_1211!(22, "cow", 0.9, 1.4, 1.3, Creature, 1e+01, false, true, true, 10, 3),
    entity_spec_1211!(23, "creeper", 0.6, 1.7, 1.445, Monster, 2e+01, false, true, true, 8, 3),
    entity_spec_1211!(24, "dolphin", 0.9, 0.6, 0.3, WaterCreature, 1e+01, false, true, true, 5, 3),
    entity_spec_1211!(25, "donkey", 1.3964844, 1.5, 1.425, Creature, 53.0, false, true, true, 10, 3),
    entity_spec_1211!(26, "dragon_fireball", 1.0, 1.0, 0.85, Misc, 0.0, false, true, false, 4, 10),
    entity_spec_1211!(27, "drowned", 0.6, 1.95, 1.74, Monster, 2e+01, false, true, true, 8, 3),
    entity_spec_1211!(28, "egg", 0.25, 0.25, 0.2125, Misc, 0.0, false, true, false, 4, 10),
    entity_spec_1211!(29, "elder_guardian", 1.9975, 1.9975, 0.99875, Monster, 8e+01, false, true, true, 10, 3),
    entity_spec_1211!(30, "end_crystal", 2.0, 2.0, 1.7, Misc, 0.0, true, true, false, 16, 2147483647),
    entity_spec_1211!(31, "ender_dragon", 16.0, 8.0, 6.8, Monster, 2e+02, true, true, true, 10, 3),
    entity_spec_1211!(32, "ender_pearl", 0.25, 0.25, 0.2125, Misc, 0.0, false, true, false, 4, 10),
    entity_spec_1211!(33, "enderman", 0.6, 2.9, 2.55, Monster, 4e+01, false, true, true, 8, 3),
    entity_spec_1211!(34, "endermite", 0.4, 0.3, 0.13, Monster, 8.0, false, true, true, 8, 3),
    entity_spec_1211!(35, "evoker", 0.6, 1.95, 1.6575, Monster, 24.0, false, true, true, 8, 3),
    entity_spec_1211!(36, "evoker_fangs", 0.5, 0.8, 0.68, Misc, 0.0, false, true, false, 6, 2),
    entity_spec_1211!(37, "experience_bottle", 0.25, 0.25, 0.2125, Misc, 0.0, false, true, false, 4, 10),
    entity_spec_1211!(38, "experience_orb", 0.5, 0.5, 0.425, Misc, 0.0, false, true, false, 6, 20),
    entity_spec_1211!(39, "eye_of_ender", 0.25, 0.25, 0.2125, Misc, 0.0, false, true, false, 4, 4),
    entity_spec_1211!(40, "falling_block", 0.98, 0.98, 0.83300006, Misc, 0.0, false, true, false, 10, 20),
    entity_spec_1211!(41, "firework_rocket", 0.25, 0.25, 0.2125, Misc, 0.0, false, true, false, 4, 10),
    entity_spec_1211!(42, "fox", 0.6, 0.7, 0.4, Creature, 1e+01, false, true, true, 8, 3),
    entity_spec_1211!(43, "frog", 0.5, 0.5, 0.425, Creature, 1e+01, false, true, true, 10, 3),
    entity_spec_1211!(44, "furnace_minecart", 0.98, 0.7, 0.595, Misc, 0.0, false, true, true, 8, 3),
    entity_spec_1211!(45, "ghast", 4.0, 4.0, 2.6, Monster, 1e+01, true, true, true, 10, 3),
    entity_spec_1211!(46, "giant", 3.6, 12.0, 10.44, Monster, 1e+02, false, true, true, 10, 3),
    entity_spec_1211!(47, "glow_item_frame", 0.5, 0.5, 0.0, Misc, 0.0, false, true, false, 10, 2147483647),
    entity_spec_1211!(48, "glow_squid", 0.8, 0.8, 0.4, UndergroundWaterCreature, 1e+01, false, true, true, 10, 3),
    entity_spec_1211!(49, "goat", 0.9, 1.3, 1.105, Creature, 1e+01, false, true, true, 10, 3),
    entity_spec_1211!(50, "guardian", 0.85, 0.85, 0.425, Monster, 3e+01, false, true, true, 8, 3),
    entity_spec_1211!(51, "hoglin", 1.3964844, 1.4, 1.19, Monster, 4e+01, false, true, true, 8, 3),
    entity_spec_1211!(52, "hopper_minecart", 0.98, 0.7, 0.595, Misc, 0.0, false, true, true, 8, 3),
    entity_spec_1211!(53, "horse", 1.3964844, 1.6, 1.52, Creature, 53.0, false, true, true, 10, 3),
    entity_spec_1211!(54, "husk", 0.6, 1.95, 1.74, Monster, 2e+01, false, true, true, 8, 3),
    entity_spec_1211!(55, "illusioner", 0.6, 1.95, 1.6575, Monster, 32.0, false, true, true, 8, 3),
    entity_spec_1211!(56, "interaction", 0.0, 0.0, 0.0, Misc, 0.0, false, true, false, 10, 3),
    entity_spec_1211!(57, "iron_golem", 1.4, 2.7, 2.295, Misc, 1e+02, false, true, true, 10, 3),
    entity_spec_1211!(58, "item", 0.25, 0.25, 0.2125, Misc, 0.0, false, true, false, 6, 20),
    entity_spec_1211!(59, "item_display", 0.0, 0.0, 0.0, Misc, 0.0, false, true, false, 10, 1),
    entity_spec_1211!(60, "item_frame", 0.5, 0.5, 0.0, Misc, 0.0, false, true, false, 10, 2147483647),
    entity_spec_1211!(61, "ominous_item_spawner", 0.25, 0.25, 0.2125, Misc, 0.0, false, true, false, 8, 3),
    entity_spec_1211!(62, "fireball", 1.0, 1.0, 0.85, Misc, 0.0, false, true, false, 4, 10),
    entity_spec_1211!(63, "leash_knot", 0.375, 0.5, 0.0625, Misc, 0.0, false, true, false, 10, 2147483647),
    entity_spec_1211!(64, "lightning_bolt", 0.0, 0.0, 0.0, Misc, 0.0, false, true, false, 16, 2147483647),
    entity_spec_1211!(65, "llama", 0.9, 1.87, 1.7765, Creature, 53.0, false, true, true, 10, 3),
    entity_spec_1211!(66, "llama_spit", 0.25, 0.25, 0.2125, Misc, 0.0, false, true, false, 4, 10),
    entity_spec_1211!(67, "magma_cube", 0.52, 0.52, 0.325, Monster, 2e+01, true, true, true, 8, 3),
    entity_spec_1211!(68, "marker", 0.0, 0.0, 0.0, Misc, 0.0, false, true, false, 0, 3),
    entity_spec_1211!(69, "minecart", 0.98, 0.7, 0.595, Misc, 0.0, false, true, true, 8, 3),
    entity_spec_1211!(70, "mooshroom", 0.9, 1.4, 1.3, Creature, 1e+01, false, true, true, 10, 3),
    entity_spec_1211!(71, "mule", 1.3964844, 1.6, 1.52, Creature, 53.0, false, true, true, 8, 3),
    entity_spec_1211!(72, "ocelot", 0.6, 0.7, 0.595, Creature, 1e+01, false, true, true, 10, 3),
    entity_spec_1211!(73, "painting", 0.5, 0.5, 0.425, Misc, 0.0, false, true, false, 10, 2147483647),
    entity_spec_1211!(74, "panda", 1.3, 1.25, 1.0625, Creature, 2e+01, false, true, true, 10, 3),
    entity_spec_1211!(75, "parrot", 0.5, 0.9, 0.54, Creature, 6.0, false, true, true, 8, 3),
    entity_spec_1211!(76, "phantom", 0.9, 0.5, 0.175, Monster, 2e+01, false, true, true, 8, 3),
    entity_spec_1211!(77, "pig", 0.9, 0.9, 0.765, Creature, 1e+01, false, true, true, 10, 3),
    entity_spec_1211!(78, "piglin", 0.6, 1.95, 1.79, Monster, 16.0, false, true, true, 8, 3),
    entity_spec_1211!(79, "piglin_brute", 0.6, 1.95, 1.79, Monster, 5e+01, false, true, true, 8, 3),
    entity_spec_1211!(80, "pillager", 0.6, 1.95, 1.6575, Monster, 24.0, false, true, true, 8, 3),
    entity_spec_1211!(81, "polar_bear", 1.4, 1.4, 1.19, Creature, 3e+01, false, true, true, 10, 3),
    entity_spec_1211!(82, "potion", 0.25, 0.25, 0.2125, Misc, 0.0, false, true, false, 4, 10),
    entity_spec_1211!(83, "pufferfish", 0.7, 0.7, 0.455, WaterAmbient, 3.0, false, true, true, 4, 3),
    entity_spec_1211!(84, "rabbit", 0.4, 0.5, 0.425, Creature, 3.0, false, true, true, 8, 3),
    entity_spec_1211!(85, "ravager", 1.95, 2.2, 1.8700001, Monster, 1e+02, false, true, true, 10, 3),
    entity_spec_1211!(86, "salmon", 0.7, 0.4, 0.26, WaterAmbient, 3.0, false, true, true, 4, 3),
    entity_spec_1211!(87, "sheep", 0.9, 1.3, 1.235, Creature, 8.0, false, true, true, 10, 3),
    entity_spec_1211!(88, "shulker", 1.0, 1.0, 0.5, Monster, 3e+01, true, true, true, 10, 3),
    entity_spec_1211!(89, "shulker_bullet", 0.3125, 0.3125, 0.265625, Misc, 0.0, false, true, false, 8, 3),
    entity_spec_1211!(90, "silverfish", 0.4, 0.3, 0.13, Monster, 8.0, false, true, true, 8, 3),
    entity_spec_1211!(91, "skeleton", 0.6, 1.99, 1.74, Monster, 2e+01, false, true, true, 8, 3),
    entity_spec_1211!(92, "skeleton_horse", 1.3964844, 1.6, 1.52, Creature, 15.0, false, true, true, 10, 3),
    entity_spec_1211!(93, "slime", 0.52, 0.52, 0.325, Monster, 2e+01, false, true, true, 10, 3),
    entity_spec_1211!(94, "small_fireball", 0.3125, 0.3125, 0.265625, Misc, 0.0, false, true, false, 4, 10),
    entity_spec_1211!(95, "sniffer", 1.9, 1.75, 1.05, Creature, 14.0, false, true, true, 10, 3),
    entity_spec_1211!(96, "snow_golem", 0.7, 1.9, 1.7, Misc, 4.0, false, true, true, 8, 3),
    entity_spec_1211!(97, "snowball", 0.25, 0.25, 0.2125, Misc, 0.0, false, true, false, 4, 10),
    entity_spec_1211!(98, "spawner_minecart", 0.98, 0.7, 0.595, Misc, 0.0, false, true, true, 8, 3),
    entity_spec_1211!(99, "spectral_arrow", 0.5, 0.5, 0.13, Misc, 0.0, false, true, false, 4, 20),
    entity_spec_1211!(100, "spider", 1.4, 0.9, 0.65, Monster, 16.0, false, true, true, 8, 3),
    entity_spec_1211!(101, "squid", 0.8, 0.8, 0.4, WaterCreature, 1e+01, false, true, true, 8, 3),
    entity_spec_1211!(102, "stray", 0.6, 1.99, 1.74, Monster, 2e+01, false, true, true, 8, 3),
    entity_spec_1211!(103, "strider", 0.9, 1.7, 1.445, Creature, 2e+01, true, true, true, 10, 3),
    entity_spec_1211!(104, "tadpole", 0.4, 0.3, 0.19500001, Creature, 6.0, false, true, true, 10, 3),
    entity_spec_1211!(105, "text_display", 0.0, 0.0, 0.0, Misc, 0.0, false, true, false, 10, 1),
    entity_spec_1211!(106, "tnt", 0.98, 0.98, 0.15, Misc, 0.0, true, true, false, 10, 10),
    entity_spec_1211!(107, "tnt_minecart", 0.98, 0.7, 0.595, Misc, 0.0, false, true, true, 8, 3),
    entity_spec_1211!(108, "trader_llama", 0.9, 1.87, 1.7765, Creature, 53.0, false, true, true, 10, 3),
    entity_spec_1211!(109, "trident", 0.5, 0.5, 0.13, Misc, 0.0, false, true, false, 4, 20),
    entity_spec_1211!(110, "tropical_fish", 0.5, 0.4, 0.26, WaterAmbient, 3.0, false, true, true, 4, 3),
    entity_spec_1211!(111, "turtle", 1.2, 0.4, 0.34, Creature, 3e+01, false, true, true, 10, 3),
    entity_spec_1211!(112, "vex", 0.4, 0.8, 0.51875, Monster, 14.0, true, true, true, 8, 3),
    entity_spec_1211!(113, "villager", 0.6, 1.95, 1.62, Misc, 2e+01, false, true, true, 10, 3),
    entity_spec_1211!(114, "vindicator", 0.6, 1.95, 1.6575, Monster, 24.0, false, true, true, 8, 3),
    entity_spec_1211!(115, "wandering_trader", 0.6, 1.95, 1.62, Creature, 2e+01, false, true, true, 10, 3),
    entity_spec_1211!(116, "warden", 0.9, 2.9, 2.4650001, Monster, 5e+02, true, true, true, 16, 3),
    entity_spec_1211!(117, "wind_charge", 0.3125, 0.3125, 0.0, Misc, 0.0, false, true, false, 4, 10),
    entity_spec_1211!(118, "witch", 0.6, 1.95, 1.62, Monster, 26.0, false, true, true, 8, 3),
    entity_spec_1211!(119, "wither", 0.9, 3.5, 2.9750001, Monster, 3e+02, true, true, true, 10, 3),
    entity_spec_1211!(120, "wither_skeleton", 0.7, 2.4, 2.1, Monster, 2e+01, true, true, true, 8, 3),
    entity_spec_1211!(121, "wither_skull", 0.3125, 0.3125, 0.265625, Misc, 0.0, false, true, false, 4, 10),
    entity_spec_1211!(122, "wolf", 0.6, 0.85, 0.68, Creature, 8.0, false, true, true, 10, 3),
    entity_spec_1211!(123, "zoglin", 1.3964844, 1.4, 1.19, Monster, 4e+01, true, true, true, 8, 3),
    entity_spec_1211!(124, "zombie", 0.6, 1.95, 1.74, Monster, 2e+01, false, true, true, 8, 3),
    entity_spec_1211!(125, "zombie_horse", 1.3964844, 1.6, 1.52, Creature, 15.0, false, true, true, 10, 3),
    entity_spec_1211!(126, "zombie_villager", 0.6, 1.95, 1.74, Monster, 2e+01, false, true, true, 8, 3),
    entity_spec_1211!(127, "zombified_piglin", 0.6, 1.95, 1.79, Monster, 2e+01, true, true, true, 8, 3),
    entity_spec_1211!(128, "player", 0.6, 1.8, 1.62, Misc, 2e+01, false, false, true, 32, 2),
    entity_spec_1211!(129, "fishing_bobber", 0.25, 0.25, 0.2125, Misc, 0.0, false, false, false, 4, 5),
];

#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn entity_spec_by_id(id: i64) -> Option<&'static EntitySpec> {
    ENTITY_REGISTRY_1211.get(usize::try_from(id).ok()?)
}

/// Bare resource name -> row index into `ENTITY_REGISTRY_1211`. Built once from
/// the static table, so the keys are `&'static` slices of `spec.name` and a
/// lookup is a single map probe with no allocation.
fn entity_name_index() -> &'static std::collections::HashMap<&'static str, usize> {
    static INDEX: std::sync::OnceLock<std::collections::HashMap<&'static str, usize>> =
        std::sync::OnceLock::new();
    INDEX.get_or_init(|| {
        ENTITY_REGISTRY_1211
            .iter()
            .enumerate()
            .map(|(i, spec)| {
                (
                    spec.name.strip_prefix("minecraft:").unwrap_or(spec.name),
                    i,
                )
            })
            .collect()
    })
}

/// `minecraft:name` or bare `name` -> the 1.21.1 registry row. Only the
/// `minecraft:` namespace is stripped, so `other:zombie` does not match.
pub(crate) fn entity_spec_by_name(name: &str) -> Option<&'static EntitySpec> {
    let name = name.strip_prefix("minecraft:").unwrap_or(name);
    entity_name_index()
        .get(name)
        .map(|&i| &ENTITY_REGISTRY_1211[i])
}

/// Cross-version entity protocol-ID remap, built from a future release's
/// `minecraft:entity_type` registry listing: `(wire protocol id, resource name)`
/// pairs. A version that inserts a new type (1.21.4's `minecraft:creaking`)
/// shifts every later ID — the remap keeps that wire numbering separate from
/// the canonical 1.21.1 rows instead of leaking it into `EntitySpec.id`.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) struct EntityVersionRemap {
    /// Dense wire id -> canonical `ENTITY_REGISTRY_1211` index. Wire ids that
    /// name an entity the canonical table does not have stay `None`.
    wire_to_index: Vec<Option<usize>>,
    /// Canonical index -> wire id in the target version, `None` when the
    /// target registry does not carry that entity at all.
    index_to_wire: Vec<Option<i64>>,
    /// Wire entries whose name matched no canonical spec.
    unmapped: usize,
}

#[cfg_attr(not(test), allow(dead_code))]
impl EntityVersionRemap {
    /// Vanilla registry tables are dense from zero; a wire id outside this
    /// bound is malformed input, not a numbering worth allocating against.
    const WIRE_ID_LIMIT: i64 = 1 << 16;

    /// Build the remap from `(wire protocol id, resource name)` entries in any
    /// order. Names resolve through the same normalized lookup as
    /// `entity_spec_by_name`; unresolvable names keep their wire id but answer
    /// `None` and count toward `unmapped_count`.
    pub(crate) fn from_wire_entries(entries: &[(i64, &str)]) -> EntityVersionRemap {
        let width = entries
            .iter()
            .map(|&(id, _)| id)
            .filter(|&id| (0..Self::WIRE_ID_LIMIT).contains(&id))
            .max()
            .map_or(0, |id| id as usize + 1);
        let mut remap = EntityVersionRemap {
            wire_to_index: vec![None; width],
            index_to_wire: vec![None; ENTITY_REGISTRY_1211.len()],
            unmapped: 0,
        };
        for &(wire_id, name) in entries {
            let Ok(slot) = usize::try_from(wire_id) else {
                remap.unmapped += 1;
                continue;
            };
            if slot >= width {
                remap.unmapped += 1;
                continue;
            }
            match entity_spec_by_name(name) {
                Some(spec) => {
                    remap.wire_to_index[slot] = Some(spec.id as usize);
                    remap.index_to_wire[spec.id as usize] = Some(wire_id);
                }
                None => remap.unmapped += 1,
            }
        }
        remap
    }

    /// Wire id in the target version -> the canonical spec, or `None` for an
    /// entity type the canonical table does not have.
    pub(crate) fn wire_to_canonical(&self, wire_id: i64) -> Option<&'static EntitySpec> {
        let &index = self
            .wire_to_index
            .get(usize::try_from(wire_id).ok()?)?
            .as_ref()?;
        ENTITY_REGISTRY_1211.get(index)
    }

    /// Canonical 1.21.1 protocol id (`EntitySpec.id`) -> the wire id the target
    /// version uses, or `None` when the target registry dropped that entity.
    pub(crate) fn canonical_to_wire(&self, canonical_id: i64) -> Option<i64> {
        self.index_to_wire
            .get(usize::try_from(canonical_id).ok()?)
            .copied()
            .flatten()
    }

    /// Wire entries that matched no canonical entity: new types plus any
    /// malformed or out-of-range ids in the input.
    pub(crate) fn unmapped_count(&self) -> usize {
        self.unmapped
    }
}

// Invalid IDs use the current entity-collision fallback geometry.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn entity_dimensions_1211(id: i64) -> (f32, f32) {
    entity_spec_by_id(id)
        .map(|spec| (spec.width, spec.height))
        .unwrap_or((0.6, 1.8))
}

#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn entity_eye_height_1211(id: i64) -> f32 {
    entity_spec_by_id(id)
        .map(|spec| spec.eye_height)
        .unwrap_or(1.53)
}

#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn entity_max_health_1211(id: i64) -> f32 {
    entity_spec_by_id(id)
        .map(|spec| spec.max_health)
        .unwrap_or(0.0)
}

#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn entity_category_1211(id: i64) -> EntityMobCategory {
    entity_spec_by_id(id)
        .map(|spec| spec.category)
        .unwrap_or(EntityMobCategory::Misc)
}

#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn entity_is_fire_immune_1211(id: i64) -> bool {
    entity_spec_by_id(id)
        .is_some_and(|spec| spec.fire_immune)
}

#[cfg(test)]
mod entity_registry_tests {
    use super::*;
    use std::collections::HashSet;

    fn spec(name: &str) -> &'static EntitySpec {
        entity_spec_by_name(name).expect("registered vanilla entity")
    }

    #[test]
    fn all_130_protocol_ids_are_sorted_and_unique() {
        assert_eq!(ENTITY_REGISTRY_1211.len(), 130);
        let mut seen = HashSet::new();
        for (index, entry) in ENTITY_REGISTRY_1211.iter().enumerate() {
            assert_eq!(entry.id as usize, index, "{}", entry.name);
            assert!(entry.name.starts_with("minecraft:"), "{}", entry.name);
            assert!(seen.insert(entry.name), "duplicate name {}", entry.name);
            assert_eq!(super::name_to_etype(entry.name), Some(entry.id));
            assert_eq!(super::name_to_etype(&entry.name[10..]), Some(entry.id));
            assert_eq!(entity_spec_by_id(entry.id).map(|found| found.name), Some(entry.name));
            assert_eq!(entity_spec_by_name(entry.name).map(|found| found.id), Some(entry.id));
            assert_eq!(entity_spec_by_name(&entry.name[10..]).map(|found| found.id), Some(entry.id));
            assert!(entry.client_tracking_range >= 0);
            assert!(entry.update_interval > 0);
        }
    }

    #[test]
    fn existing_collision_dimensions_match_all_eleven_explicit_mobs() {
        for id in [3, 6, 16, 19, 22, 23, 77, 87, 91, 100, 124] {
            assert_eq!(entity_dimensions_1211(id), super::mob_dimensions(id), "id {id}");
        }
    }

    #[test]
    fn existing_explicit_health_cases_match_vanilla_except_magma_cube() {
        for name in [
            "elder_guardian", "witch", "cat", "cow", "pig", "shulker",
            "evoker", "vindicator", "pillager", "piglin", "spider",
            "piglin_brute", "hoglin", "iron_golem", "cave_spider",
            "silverfish", "sheep", "camel",
        ] {
            assert_eq!(spec(name).max_health, super::default_mob_hp(name), "{name}");
        }
        // The older default_mob_hp still returns 16 for magma cubes.
        // DefaultAttributes in vanilla 1.21.1 supplies 20 for a base-size cube.
        assert_eq!(super::default_mob_hp("magma_cube"), 16.0);
        assert_eq!(spec("magma_cube").max_health, 20.0);
    }

    #[test]
    fn vanilla_health_dimensions_and_fire_immunity() {
        let warden = spec("warden");
        assert_eq!((warden.max_health, warden.width, warden.height), (500.0, 0.9, 2.9));
        let dragon = spec("ender_dragon");
        assert_eq!(dragon.max_health, 200.0);
        assert!(dragon.fire_immune);
        let wither = spec("wither");
        assert_eq!(wither.max_health, 300.0);
        assert!(wither.fire_immune);
        assert_eq!(spec("ravager").max_health, 100.0);
        assert_eq!(spec("iron_golem").max_health, 100.0);
        assert_eq!(spec("pig").max_health, 10.0);
        assert_eq!(spec("zombie").max_health, 20.0);
        assert!(!spec("zombie").fire_immune);
        assert!(spec("zombified_piglin").fire_immune);
        assert!(!spec("piglin").fire_immune);
        assert!(!spec("hoglin").fire_immune);
        assert!(spec("zoglin").fire_immune);
        assert!(spec("blaze").fire_immune);
        assert!(spec("shulker").fire_immune);
        assert!(spec("warden").fire_immune);
        assert!(spec("vex").fire_immune);
        assert_eq!(entity_is_fire_immune_1211(dragon.id), dragon.fire_immune);
    }

    #[test]
    fn vanilla_spawner_ids_and_known_legacy_dimension_differences() {
        for (id, name) in [
            (124, "zombie"), (91, "skeleton"), (100, "spider"),
            (16, "cave_spider"), (8, "blaze"), (67, "magma_cube"),
            (90, "silverfish"), (27, "drowned"), (102, "stray"),
            (54, "husk"), (12, "breeze"), (77, "pig"),
        ] {
            assert_eq!(spec(name).id, id);
        }
        // These dimensions differed in the former spawner metadata table.
        for (name, size) in [
            ("blaze", (0.6, 1.8)),
            ("magma_cube", (0.52, 0.52)),
            ("silverfish", (0.4, 0.3)),
            ("drowned", (0.6, 1.95)),
            ("husk", (0.6, 1.95)),
            ("breeze", (0.6, 1.77)),
        ] {
            let entry = spec(name);
            assert_eq!((entry.width, entry.height), size, "{name}");
        }
    }

    #[test]
    fn category_caps_flags_and_important_tracking_values() {
        use EntityMobCategory::*;
        for (category, cap, friendly, persistent, despawn) in [
            (Monster, 70, false, false, 128),
            (Creature, 10, true, true, 128),
            (Ambient, 15, true, false, 128),
            (Axolotls, 5, true, false, 128),
            (UndergroundWaterCreature, 5, true, false, 128),
            (WaterCreature, 5, true, false, 128),
            (WaterAmbient, 20, true, false, 64),
            (Misc, -1, true, true, 128),
        ] {
            assert_eq!(category.max_instances_per_chunk(), cap);
            assert_eq!(category.is_friendly(), friendly);
            assert_eq!(category.is_persistent(), persistent);
            assert_eq!(category.despawn_distance(), despawn);
        }
        assert_eq!(spec("villager").category, Misc);
        assert_eq!(spec("iron_golem").category, Misc);
        assert_eq!(spec("cod").category, WaterAmbient);
        assert_eq!(spec("warden").category, Monster);
        assert_eq!(spec("player").client_tracking_range, 32);
        assert_eq!(spec("player").update_interval, 2);
        assert!(!spec("player").summonable);
        assert!(!spec("fishing_bobber").summonable);
        assert_eq!(spec("marker").client_tracking_range, 0);
        assert_eq!(spec("area_effect_cloud").update_interval, i32::MAX);
    }

    #[test]
    fn lookups_unknown_ids_and_static_pushability() {
        for invalid in [-10, -1, 130, i64::MAX, i64::MIN] {
            assert!(entity_spec_by_id(invalid).is_none());
            assert_eq!(entity_dimensions_1211(invalid), (0.6, 1.8));
            assert_eq!(entity_eye_height_1211(invalid), 1.53);
            assert_eq!(entity_max_health_1211(invalid), 0.0);
            assert_eq!(entity_category_1211(invalid), EntityMobCategory::Misc);
            assert!(!entity_is_fire_immune_1211(invalid));
        }
        for name in [
            "minecraft:not_an_entity",
            "",
            "unknown",
            "other:zombie",
            "minecraft:minecraft:zombie",
        ] {
            assert!(entity_spec_by_name(name).is_none());
            assert!(super::name_to_etype(name).is_none());
        }
        for name in [
            "armor_stand", "bat", "marker", "interaction", "block_display",
            "item_display", "text_display", "arrow", "item",
        ] {
            assert!(!spec(name).pushable, "{name}");
        }
        for name in ["pig", "zombie", "boat", "minecart", "iron_golem"] {
            assert!(spec(name).pushable, "{name}");
        }
        for name in ["arrow", "item", "boat", "minecart", "fishing_bobber"] {
            assert_eq!(spec(name).max_health, 0.0);
        }
    }

    #[test]
    fn name_lookup_normalizes_the_minecraft_namespace_only() {
        assert_eq!(entity_spec_by_name("zombie").map(|s| s.id), Some(124));
        assert_eq!(entity_spec_by_name("minecraft:zombie").map(|s| s.id), Some(124));
        assert_eq!(entity_spec_by_name("allay").map(|s| s.id), Some(0));
        assert_eq!(entity_spec_by_name("minecraft:allay").map(|s| s.id), Some(0));
        // Only the minecraft namespace strips; foreign namespaces must not match.
        for name in ["other:zombie", "minecraft:", "minecraft:minecraft:zombie", " zombie", "zombie "] {
            assert!(entity_spec_by_name(name).is_none(), "{name}");
        }
    }

    #[test]
    fn version_remap_tracks_an_inserted_entity_shifting_later_wire_ids() {
        // A 1.21.4-shaped wire table: `minecraft:creaking` sorts right after
        // `minecraft:cow`, so every canonical id from `creeper` (23) on shifts
        // up by one on the wire.
        let mut entries: Vec<(i64, &str)> = Vec::new();
        for spec in ENTITY_REGISTRY_1211.iter() {
            let shifted = spec.id + i64::from(spec.id >= 23);
            entries.push((shifted, spec.name));
            if spec.id == 22 {
                entries.push((23, "minecraft:creaking"));
            }
        }
        let remap = EntityVersionRemap::from_wire_entries(&entries);
        // The one new entity has no canonical row; every canonical row remapped.
        assert_eq!(remap.unmapped_count(), 1);
        assert!(remap.wire_to_canonical(23).is_none());
        // Unshifted prefix and shifted tail, both directions exact.
        assert_eq!(remap.wire_to_canonical(22).unwrap().name, "minecraft:cow");
        assert_eq!(remap.wire_to_canonical(24).unwrap().name, "minecraft:creeper");
        assert_eq!(remap.wire_to_canonical(125).unwrap().name, "minecraft:zombie");
        assert_eq!(remap.canonical_to_wire(22), Some(22));
        assert_eq!(remap.canonical_to_wire(23), Some(24));
        assert_eq!(remap.canonical_to_wire(124), Some(125));
        // Every canonical entity round-trips through the shifted table.
        for spec in ENTITY_REGISTRY_1211.iter() {
            let wire = remap.canonical_to_wire(spec.id).expect(spec.name);
            assert_eq!(remap.wire_to_canonical(wire).unwrap().id, spec.id);
        }
        // Past the end of the wire table there is nothing to map.
        assert!(remap.wire_to_canonical(entries.len() as i64 + 10).is_none());
        assert!(remap.wire_to_canonical(-1).is_none());
        assert!(remap.canonical_to_wire(-1).is_none());
        assert!(remap.canonical_to_wire(130).is_none());
    }

    #[test]
    fn version_remap_reports_removals_and_malformed_entries() {
        // A target registry that dropped `axolotl`: its canonical id maps to
        // nothing while the survivors still resolve in both directions.
        let entries: Vec<(i64, &str)> = ENTITY_REGISTRY_1211
            .iter()
            .filter(|spec| spec.name != "minecraft:axolotl")
            .map(|spec| (spec.id, spec.name))
            .collect();
        let remap = EntityVersionRemap::from_wire_entries(&entries);
        assert_eq!(remap.unmapped_count(), 0);
        assert_eq!(remap.canonical_to_wire(5), None);
        assert_eq!(remap.canonical_to_wire(6), Some(6));
        // Bare names resolve like namespaced ones; unknown names and negative
        // or out-of-range ids are counted, never matched or allocated against.
        let odd = EntityVersionRemap::from_wire_entries(&[
            (0, "allay"),
            (1, "minecraft:axolotl"),
            (2, "minecraft:creaking"),
            (-3, "minecraft:zombie"),
            (i64::MAX, "minecraft:cow"),
        ]);
        assert_eq!(odd.unmapped_count(), 3);
        assert_eq!(odd.wire_to_canonical(0).unwrap().name, "minecraft:allay");
        assert_eq!(odd.wire_to_canonical(1).unwrap().name, "minecraft:axolotl");
        assert!(odd.wire_to_canonical(2).is_none());
        assert!(odd.wire_to_canonical(i64::MAX).is_none());
        assert_eq!(odd.canonical_to_wire(0), Some(0));
        assert_eq!(odd.canonical_to_wire(5), Some(1));
        assert_eq!(odd.canonical_to_wire(124), None);
    }
}
