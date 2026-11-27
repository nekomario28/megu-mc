// SPDX-License-Identifier: LGPL-3.0-or-later
#![forbid(unsafe_code)]
//! `megu_core::gamerules` — the 51 Vanilla 1.21.1 gamerules as one typed,
//! declarative table.
//!
//! [`VanillaGamerules`] is the single source of truth the rest of the tree
//! keys on: the `/gamerule` command, the per-subsystem behaviour switches
//! (weather cycle, insomnia, daylight cycle, source conversion, …) and
//! `level.dat` persistence all read the same struct instead of keeping a
//! `HashMap<&str, i32>` and a pile of atomics that can drift apart.
//!
//! Everything below is generated from [`vanilla_gamerules`]'s one table, so a
//! rule's camelCase wire name, its type, its default, its category and its
//! typed accessor cannot disagree with each other.
//!
//! Verified against the pinned 1.21.1 jar's `GameRules.<clinit>`
//! (`tools/vanilla_data/`): 51 registrations, in registration order, each with
//! the same `Category` and default. `GameRules` stores its keys in a
//! `TreeMap` ordered by name, which is why [`GameruleId::ALL`] is the
//! alphabetical order rather than the `<clinit>` order — see the `iter_all`
//! test that re-derives that order from the table.
//!
//! ```
//! use megu_core::gamerules::{GameruleId, GameruleValue, VanillaGamerules};
//!
//! let mut rules = VanillaGamerules::default();
//! assert_eq!(rules.random_tick_speed(), 3);
//! assert!(rules.set(GameruleId::RandomTickSpeed, GameruleValue::Int(0)));
//! assert_eq!(rules.random_tick_speed(), 0);
//! // A value of the wrong kind is rejected instead of silently coerced.
//! assert!(!rules.set(GameruleId::RandomTickSpeed, GameruleValue::Bool(true)));
//! ```

/// The seven `GameRules.Category` buckets, in vanilla's ordinal order.
///
/// `GameRules.Category` carries only the translation key
/// (`gamerule.category.<name>`); the bucket itself changes nothing about how a
/// rule behaves, so this exists for UI grouping and `/gamerule` listing.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum GameruleCategory {
    /// `PLAYER` — per-player capability and damage switches.
    Player,
    /// `MOBS` — mob behaviour, griefing and cramming.
    Mobs,
    /// `SPAWNING` — natural spawn toggles.
    Spawning,
    /// `DROPS` — loot, decay and explosion drop rules.
    Drops,
    /// `UPDATES` — tick-driven world simulation.
    Updates,
    /// `CHAT` — operator and chat feedback.
    Chat,
    /// `MISC` — command budgets and the leftovers.
    Misc,
}

impl GameruleCategory {
    /// The translation key vanilla registers for this category.
    #[must_use]
    pub const fn description_id(self) -> &'static str {
        match self {
            Self::Player => "gamerule.category.player",
            Self::Mobs => "gamerule.category.mobs",
            Self::Spawning => "gamerule.category.spawning",
            Self::Drops => "gamerule.category.drops",
            Self::Updates => "gamerule.category.updates",
            Self::Chat => "gamerule.category.chat",
            Self::Misc => "gamerule.category.misc",
        }
    }

    /// Every category, in vanilla's ordinal order.
    pub const ALL: &'static [Self] = &[
        Self::Player,
        Self::Mobs,
        Self::Spawning,
        Self::Drops,
        Self::Updates,
        Self::Chat,
        Self::Misc,
    ];
}

/// Which `GameRules.Type` a rule is: `BooleanValue` or `IntegerValue`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum GameruleKind {
    /// `GameRules.BooleanValue`.
    Bool,
    /// `GameRules.IntegerValue`.
    Int,
}

/// A gamerule value: the two `GameRules.Value` shapes vanilla has.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum GameruleValue {
    /// `GameRules.BooleanValue`.
    Bool(bool),
    /// `GameRules.IntegerValue`.
    Int(i32),
}

impl GameruleValue {
    /// Which rule kind this value can be stored in.
    #[must_use]
    pub const fn kind(self) -> GameruleKind {
        match self {
            Self::Bool(_) => GameruleKind::Bool,
            Self::Int(_) => GameruleKind::Int,
        }
    }

    /// The boolean payload, or `None` for an integer rule.
    #[must_use]
    pub const fn as_bool(self) -> Option<bool> {
        match self {
            Self::Bool(value) => Some(value),
            Self::Int(_) => None,
        }
    }

    /// The integer payload, or `None` for a boolean rule.
    #[must_use]
    pub const fn as_int(self) -> Option<i32> {
        match self {
            Self::Int(value) => Some(value),
            Self::Bool(_) => None,
        }
    }
}

/// Maps a table row's `bool`/`int` marker onto the storage type, so one table
/// row carries the name, kind, default and accessor without repeating itself.
macro_rules! gamerule_type {
    (bool) => {
        bool
    };
    (int) => {
        i32
    };
}

/// Builds the [`GameruleValue`] of a row's kind around an expression of that
/// kind's payload type.
macro_rules! gamerule_value {
    (bool, $value:expr) => {
        GameruleValue::Bool($value)
    };
    (int, $value:expr) => {
        GameruleValue::Int($value)
    };
}

/// A row's [`GameruleKind`], as a constant expression.
macro_rules! gamerule_kind {
    (bool) => {
        GameruleKind::Bool
    };
    (int) => {
        GameruleKind::Int
    };
}

/// Extracts a row's payload out of a [`GameruleValue`].
///
/// `set` checks [`GameruleValue::kind`] first, so `None` is unreachable in
/// practice; keeping it as data avoids a production panic path if that
/// invariant is ever broken by a future edit.
macro_rules! gamerule_payload {
    (bool, $value:expr) => {
        match $value {
            GameruleValue::Bool(raw) => Some(raw),
            GameruleValue::Int(_) => None,
        }
    };
    (int, $value:expr) => {
        match $value {
            GameruleValue::Int(raw) => Some(raw),
            GameruleValue::Bool(_) => None,
        }
    };
}

/// The one declarative gamerule table, in `GameRules`' iteration order.
///
/// Each row is `(GameruleId variant, camelCase name, Category, kind, default,
/// accessor)`. Everything else in this module — the enum, the struct, the
/// defaults, the name table, `get`/`set`, `iter_all` — is generated from it.
///
/// The rows are sorted by `name`, not by `GameRules.<clinit>` order: vanilla
/// keeps the keys in a `TreeMap` ordered by name, so that is the order
/// `visitAll`, the `/gamerule` suggestions and the gamerule packet see. The
/// `<clinit>` order only decides the enum discriminants, which are never on the
/// wire.
macro_rules! vanilla_gamerules {
    (
        $( ($variant:ident, $name:literal, $category:ident, $kind:ident, $default:literal, $getter:ident) ),*
        $(,)?
    ) => {
        /// One variant per Vanilla 1.21.1 gamerule, in `GameRules`' `TreeMap`
        /// order (ascending by name).
        #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub enum GameruleId {
            $(
                #[doc = concat!("`", $name, "`.")]
                $variant,
            )*
        }

        impl GameruleId {
            /// Every gamerule key in `GameRules`' `TreeMap` order (ascending by
            /// `name`), which is the order vanilla iterates and lists them in.
            pub const ALL: &'static [Self] = &[ $( Self::$variant, )* ];

            /// The camelCase name `/gamerule` and `level.dat` use.
            #[must_use]
            pub const fn name(self) -> &'static str {
                match self {
                    $( Self::$variant => $name, )*
                }
            }

            /// The `GameRules.Category` vanilla registers this rule under.
            #[must_use]
            pub const fn category(self) -> GameruleCategory {
                match self {
                    $( Self::$variant => GameruleCategory::$category, )*
                }
            }

            /// Whether this rule stores a boolean or an integer.
            #[must_use]
            pub const fn kind(self) -> GameruleKind {
                match self {
                    $( Self::$variant => gamerule_kind!($kind), )*
                }
            }

            /// The value a freshly created world starts with.
            #[must_use]
            pub const fn default_value(self) -> GameruleValue {
                match self {
                    $( Self::$variant => gamerule_value!($kind, $default), )*
                }
            }

            /// Resolve a camelCase name exactly as `GameRules.getRule` does:
            /// case-sensitive, no namespace, no trimming.
            #[must_use]
            pub fn parse_name(name: &str) -> Option<Self> {
                match name {
                    $( $name => Some(Self::$variant), )*
                    _ => None,
                }
            }
        }

        /// All 51 Vanilla 1.21.1 gamerules, as typed fields.
        ///
        /// Boolean rules are `bool` and integer rules are `i32`, so a caller
        /// cannot read `randomTickSpeed` as a boolean the way a
        /// `HashMap<&str, i32>` lets a stale key be read as anything at all.
        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        pub struct VanillaGamerules {
            $( $getter: gamerule_type!($kind), )*
        }

        impl Default for VanillaGamerules {
            fn default() -> Self {
                Self { $( $getter: $default, )* }
            }
        }

        impl VanillaGamerules {
            /// A rule set at the audited 1.21.1 defaults.
            #[must_use]
            pub fn new() -> Self {
                Self::default()
            }

            $(
                #[doc = concat!("`", $name, "` — the typed accessor for [`GameruleId::", stringify!($variant), "`].")]
                #[must_use]
                pub fn $getter(&self) -> gamerule_type!($kind) {
                    self.$getter
                }
            )*

            /// The current value of `id`.
            #[must_use]
            pub fn get(&self, id: GameruleId) -> GameruleValue {
                match id {
                    $( GameruleId::$variant => gamerule_value!($kind, self.$getter), )*
                }
            }

            /// Set `id`, returning `false` and leaving the rule untouched when
            /// the value's kind does not match the rule's kind.
            ///
            /// Vanilla reaches this state through a typed `GameRules.Value`; a
            /// wrong-kind pair can only come from a caller bug or a corrupt
            /// `level.dat`, and both should fail closed rather than coerce
            /// `true` into `1`.
            pub fn set(&mut self, id: GameruleId, value: GameruleValue) -> bool {
                if value.kind() != id.kind() {
                    return false;
                }
                match id {
                    $( GameruleId::$variant => {
                        self.$getter = match gamerule_payload!($kind, value) {
                            Some(payload) => payload,
                            None => return false,
                        };
                    } )*
                }
                true
            }

            /// Every rule with its current value, in [`GameruleId::ALL`] order.
            pub fn iter_all(&self) -> impl Iterator<Item = (GameruleId, GameruleValue)> + '_ {
                GameruleId::ALL.iter().map(move |&id| (id, self.get(id)))
            }

            /// Restore every rule to its vanilla default.
            pub fn reset(&mut self) {
                *self = Self::default();
            }
        }
    };
}

vanilla_gamerules! {
    (AnnounceAdvancements, "announceAdvancements", Chat, bool, true, announce_advancements),
    (BlockExplosionDropDecay, "blockExplosionDropDecay", Drops, bool, true, block_explosion_drop_decay),
    (CommandBlockOutput, "commandBlockOutput", Chat, bool, true, command_block_output),
    (CommandModificationBlockLimit, "commandModificationBlockLimit", Misc, int, 32768, command_modification_block_limit),
    (DisableElytraMovementCheck, "disableElytraMovementCheck", Player, bool, false, disable_elytra_movement_check),
    (DisableRaids, "disableRaids", Mobs, bool, false, disable_raids),
    (DoDaylightCycle, "doDaylightCycle", Updates, bool, true, do_daylight_cycle),
    (DoEntityDrops, "doEntityDrops", Drops, bool, true, do_entity_drops),
    (DoFireTick, "doFireTick", Updates, bool, true, do_fire_tick),
    (DoImmediateRespawn, "doImmediateRespawn", Player, bool, false, do_immediate_respawn),
    (DoInsomnia, "doInsomnia", Spawning, bool, true, do_insomnia),
    (DoLimitedCrafting, "doLimitedCrafting", Player, bool, false, do_limited_crafting),
    (DoMobLoot, "doMobLoot", Drops, bool, true, do_mob_loot),
    (DoMobSpawning, "doMobSpawning", Spawning, bool, true, do_mob_spawning),
    (DoPatrolSpawning, "doPatrolSpawning", Spawning, bool, true, do_patrol_spawning),
    (DoTileDrops, "doTileDrops", Drops, bool, true, do_tile_drops),
    (DoTraderSpawning, "doTraderSpawning", Spawning, bool, true, do_trader_spawning),
    (DoVinesSpread, "doVinesSpread", Updates, bool, true, do_vines_spread),
    (DoWardenSpawning, "doWardenSpawning", Spawning, bool, true, do_warden_spawning),
    (DoWeatherCycle, "doWeatherCycle", Updates, bool, true, do_weather_cycle),
    (DrowningDamage, "drowningDamage", Player, bool, true, drowning_damage),
    (EnderPearlsVanishOnDeath, "enderPearlsVanishOnDeath", Player, bool, true, ender_pearls_vanish_on_death),
    (FallDamage, "fallDamage", Player, bool, true, fall_damage),
    (FireDamage, "fireDamage", Player, bool, true, fire_damage),
    (ForgiveDeadPlayers, "forgiveDeadPlayers", Mobs, bool, true, forgive_dead_players),
    (FreezeDamage, "freezeDamage", Player, bool, true, freeze_damage),
    (GlobalSoundEvents, "globalSoundEvents", Misc, bool, true, global_sound_events),
    (KeepInventory, "keepInventory", Player, bool, false, keep_inventory),
    (LavaSourceConversion, "lavaSourceConversion", Updates, bool, false, lava_source_conversion),
    (LogAdminCommands, "logAdminCommands", Chat, bool, true, log_admin_commands),
    (MaxCommandChainLength, "maxCommandChainLength", Misc, int, 65536, max_command_chain_length),
    (MaxCommandForkCount, "maxCommandForkCount", Misc, int, 65536, max_command_fork_count),
    (MaxEntityCramming, "maxEntityCramming", Mobs, int, 24, max_entity_cramming),
    (MobExplosionDropDecay, "mobExplosionDropDecay", Drops, bool, true, mob_explosion_drop_decay),
    (MobGriefing, "mobGriefing", Mobs, bool, true, mob_griefing),
    (NaturalRegeneration, "naturalRegeneration", Player, bool, true, natural_regeneration),
    (PlayersNetherPortalCreativeDelay, "playersNetherPortalCreativeDelay", Player, int, 1, players_nether_portal_creative_delay),
    (PlayersNetherPortalDefaultDelay, "playersNetherPortalDefaultDelay", Player, int, 80, players_nether_portal_default_delay),
    (PlayersSleepingPercentage, "playersSleepingPercentage", Player, int, 100, players_sleeping_percentage),
    (ProjectilesCanBreakBlocks, "projectilesCanBreakBlocks", Drops, bool, true, projectiles_can_break_blocks),
    (RandomTickSpeed, "randomTickSpeed", Updates, int, 3, random_tick_speed),
    (ReducedDebugInfo, "reducedDebugInfo", Misc, bool, false, reduced_debug_info),
    (SendCommandFeedback, "sendCommandFeedback", Chat, bool, true, send_command_feedback),
    (ShowDeathMessages, "showDeathMessages", Chat, bool, true, show_death_messages),
    (SnowAccumulationHeight, "snowAccumulationHeight", Updates, int, 1, snow_accumulation_height),
    (SpawnChunkRadius, "spawnChunkRadius", Misc, int, 2, spawn_chunk_radius),
    (SpawnRadius, "spawnRadius", Player, int, 10, spawn_radius),
    (SpectatorsGenerateChunks, "spectatorsGenerateChunks", Player, bool, true, spectators_generate_chunks),
    (TntExplosionDropDecay, "tntExplosionDropDecay", Drops, bool, false, tnt_explosion_drop_decay),
    (UniversalAnger, "universalAnger", Mobs, bool, false, universal_anger),
    (WaterSourceConversion, "waterSourceConversion", Updates, bool, true, water_source_conversion),
}

/// The only integer rule vanilla bounds: `spawnChunkRadius` is registered as
/// `GameRules.IntegerValue(2, 0, 32)` and clamped on read.
pub const SPAWN_CHUNK_RADIUS_RANGE: (i32, i32) = (0, 32);

/// `IntegerValue(2, 0, 32)`'s clamp, applied to a value that may come from a
/// hand-edited `level.dat`.
#[must_use]
pub fn clamp_spawn_chunk_radius(value: i32) -> i32 {
    crate::math::Mth::clamp(
        value,
        SPAWN_CHUNK_RADIUS_RANGE.0,
        SPAWN_CHUNK_RADIUS_RANGE.1,
    )
}

/// The number of gamerules Vanilla 1.21.1 registers.
pub const VANILLA_GAMERULE_COUNT: usize = 51;

#[cfg(test)]
mod tests {
    use super::*;

    /// The independently transcribed audit table: name, kind, default. If the
    /// macro table above ever drifts, this is what catches it.
    const AUDITED: [(&str, GameruleKind, GameruleValue); 51] = [
        ("doFireTick", GameruleKind::Bool, GameruleValue::Bool(true)),
        ("mobGriefing", GameruleKind::Bool, GameruleValue::Bool(true)),
        (
            "keepInventory",
            GameruleKind::Bool,
            GameruleValue::Bool(false),
        ),
        (
            "doMobSpawning",
            GameruleKind::Bool,
            GameruleValue::Bool(true),
        ),
        ("doMobLoot", GameruleKind::Bool, GameruleValue::Bool(true)),
        (
            "projectilesCanBreakBlocks",
            GameruleKind::Bool,
            GameruleValue::Bool(true),
        ),
        ("doTileDrops", GameruleKind::Bool, GameruleValue::Bool(true)),
        (
            "doEntityDrops",
            GameruleKind::Bool,
            GameruleValue::Bool(true),
        ),
        (
            "commandBlockOutput",
            GameruleKind::Bool,
            GameruleValue::Bool(true),
        ),
        (
            "naturalRegeneration",
            GameruleKind::Bool,
            GameruleValue::Bool(true),
        ),
        (
            "doDaylightCycle",
            GameruleKind::Bool,
            GameruleValue::Bool(true),
        ),
        (
            "logAdminCommands",
            GameruleKind::Bool,
            GameruleValue::Bool(true),
        ),
        (
            "showDeathMessages",
            GameruleKind::Bool,
            GameruleValue::Bool(true),
        ),
        ("randomTickSpeed", GameruleKind::Int, GameruleValue::Int(3)),
        (
            "sendCommandFeedback",
            GameruleKind::Bool,
            GameruleValue::Bool(true),
        ),
        (
            "reducedDebugInfo",
            GameruleKind::Bool,
            GameruleValue::Bool(false),
        ),
        (
            "spectatorsGenerateChunks",
            GameruleKind::Bool,
            GameruleValue::Bool(true),
        ),
        ("spawnRadius", GameruleKind::Int, GameruleValue::Int(10)),
        (
            "disableElytraMovementCheck",
            GameruleKind::Bool,
            GameruleValue::Bool(false),
        ),
        (
            "maxEntityCramming",
            GameruleKind::Int,
            GameruleValue::Int(24),
        ),
        (
            "doWeatherCycle",
            GameruleKind::Bool,
            GameruleValue::Bool(true),
        ),
        (
            "doLimitedCrafting",
            GameruleKind::Bool,
            GameruleValue::Bool(false),
        ),
        (
            "maxCommandChainLength",
            GameruleKind::Int,
            GameruleValue::Int(65536),
        ),
        (
            "maxCommandForkCount",
            GameruleKind::Int,
            GameruleValue::Int(65536),
        ),
        (
            "commandModificationBlockLimit",
            GameruleKind::Int,
            GameruleValue::Int(32768),
        ),
        (
            "announceAdvancements",
            GameruleKind::Bool,
            GameruleValue::Bool(true),
        ),
        (
            "disableRaids",
            GameruleKind::Bool,
            GameruleValue::Bool(false),
        ),
        ("doInsomnia", GameruleKind::Bool, GameruleValue::Bool(true)),
        (
            "doImmediateRespawn",
            GameruleKind::Bool,
            GameruleValue::Bool(false),
        ),
        (
            "playersNetherPortalDefaultDelay",
            GameruleKind::Int,
            GameruleValue::Int(80),
        ),
        (
            "playersNetherPortalCreativeDelay",
            GameruleKind::Int,
            GameruleValue::Int(1),
        ),
        (
            "drowningDamage",
            GameruleKind::Bool,
            GameruleValue::Bool(true),
        ),
        ("fallDamage", GameruleKind::Bool, GameruleValue::Bool(true)),
        ("fireDamage", GameruleKind::Bool, GameruleValue::Bool(true)),
        (
            "freezeDamage",
            GameruleKind::Bool,
            GameruleValue::Bool(true),
        ),
        (
            "doPatrolSpawning",
            GameruleKind::Bool,
            GameruleValue::Bool(true),
        ),
        (
            "doTraderSpawning",
            GameruleKind::Bool,
            GameruleValue::Bool(true),
        ),
        (
            "doWardenSpawning",
            GameruleKind::Bool,
            GameruleValue::Bool(true),
        ),
        (
            "forgiveDeadPlayers",
            GameruleKind::Bool,
            GameruleValue::Bool(true),
        ),
        (
            "universalAnger",
            GameruleKind::Bool,
            GameruleValue::Bool(false),
        ),
        (
            "playersSleepingPercentage",
            GameruleKind::Int,
            GameruleValue::Int(100),
        ),
        (
            "blockExplosionDropDecay",
            GameruleKind::Bool,
            GameruleValue::Bool(true),
        ),
        (
            "mobExplosionDropDecay",
            GameruleKind::Bool,
            GameruleValue::Bool(true),
        ),
        (
            "tntExplosionDropDecay",
            GameruleKind::Bool,
            GameruleValue::Bool(false),
        ),
        (
            "snowAccumulationHeight",
            GameruleKind::Int,
            GameruleValue::Int(1),
        ),
        (
            "waterSourceConversion",
            GameruleKind::Bool,
            GameruleValue::Bool(true),
        ),
        (
            "lavaSourceConversion",
            GameruleKind::Bool,
            GameruleValue::Bool(false),
        ),
        (
            "globalSoundEvents",
            GameruleKind::Bool,
            GameruleValue::Bool(true),
        ),
        (
            "doVinesSpread",
            GameruleKind::Bool,
            GameruleValue::Bool(true),
        ),
        (
            "enderPearlsVanishOnDeath",
            GameruleKind::Bool,
            GameruleValue::Bool(true),
        ),
        ("spawnChunkRadius", GameruleKind::Int, GameruleValue::Int(2)),
    ];

    #[test]
    fn every_audited_rule_exists_with_its_audited_kind_and_default() {
        let rules = VanillaGamerules::default();
        assert_eq!(AUDITED.len(), VANILLA_GAMERULE_COUNT);
        assert_eq!(GameruleId::ALL.len(), VANILLA_GAMERULE_COUNT);
        for &(name, kind, default) in &AUDITED {
            let id = GameruleId::parse_name(name)
                .unwrap_or_else(|| panic!("{name} is missing from the gamerule table"));
            assert_eq!(id.name(), name, "{name} round-trips through parse_name");
            assert_eq!(id.kind(), kind, "{name} kind");
            assert_eq!(id.default_value(), default, "{name} default");
            assert_eq!(rules.get(id), default, "{name} default in the struct");
            assert_eq!(
                default.kind(),
                kind,
                "{name} value kind agrees with the rule kind"
            );
        }
    }

    #[test]
    fn the_table_has_exactly_the_fifty_one_vanilla_names() {
        let mut names: Vec<&str> = GameruleId::ALL.iter().map(|id| id.name()).collect();
        names.sort_unstable();
        let mut audited: Vec<&str> = AUDITED.iter().map(|(name, _, _)| *name).collect();
        audited.sort_unstable();
        assert_eq!(names, audited);
        // No duplicates: `parse_name` resolving both directions is not enough.
        names.dedup();
        assert_eq!(names.len(), VANILLA_GAMERULE_COUNT);
    }

    #[test]
    fn all_lists_rules_in_vanilla_tree_map_order() {
        // `GameRules` keeps its keys in a TreeMap ordered by name, so both the
        // packet order and the `/gamerule` listing are alphabetical.
        let names: Vec<&str> = GameruleId::ALL.iter().map(|id| id.name()).collect();
        let mut sorted = names.clone();
        sorted.sort_unstable();
        assert_eq!(names, sorted, "GameruleId::ALL must be ascending by name");
    }

    #[test]
    fn parse_name_is_case_sensitive_and_namespaced_free() {
        assert_eq!(
            GameruleId::parse_name("doWeatherCycle"),
            Some(GameruleId::DoWeatherCycle)
        );
        assert_eq!(GameruleId::parse_name("doweathercycle"), None);
        assert_eq!(GameruleId::parse_name("minecraft:doWeatherCycle"), None);
        assert_eq!(GameruleId::parse_name(" doWeatherCycle"), None);
        assert_eq!(GameruleId::parse_name(""), None);
    }

    #[test]
    fn typed_getters_read_the_same_slot_get_does() {
        let mut rules = VanillaGamerules::default();
        assert!(rules.do_weather_cycle());
        assert_eq!(rules.random_tick_speed(), 3);
        assert_eq!(rules.players_sleeping_percentage(), 100);
        assert_eq!(rules.players_nether_portal_default_delay(), 80);
        assert_eq!(rules.players_nether_portal_creative_delay(), 1);
        assert_eq!(rules.max_command_chain_length(), 65536);
        assert_eq!(rules.max_command_fork_count(), 65536);
        assert_eq!(rules.command_modification_block_limit(), 32768);
        assert_eq!(rules.snow_accumulation_height(), 1);
        assert_eq!(rules.spawn_chunk_radius(), 2);
        assert_eq!(rules.spawn_radius(), 10);
        assert_eq!(rules.max_entity_cramming(), 24);

        rules
            .set(GameruleId::DoWeatherCycle, GameruleValue::Bool(false))
            .then_some(())
            .expect("boolean rule accepts a boolean");
        assert!(!rules.do_weather_cycle());
        assert_eq!(
            rules.get(GameruleId::DoWeatherCycle),
            GameruleValue::Bool(false)
        );
    }

    #[test]
    fn set_rejects_a_mismatched_kind_without_touching_the_rule() {
        let mut rules = VanillaGamerules::default();
        let before = rules;
        assert!(!rules.set(GameruleId::RandomTickSpeed, GameruleValue::Bool(true)));
        assert!(!rules.set(GameruleId::DoWeatherCycle, GameruleValue::Int(0)));
        assert_eq!(rules, before);
        assert_eq!(rules.random_tick_speed(), 3);
        assert!(rules.do_weather_cycle());
    }

    #[test]
    fn iter_all_yields_every_rule_once_in_order() {
        let mut rules = VanillaGamerules::default();
        rules.set(GameruleId::RandomTickSpeed, GameruleValue::Int(11));
        let listed: Vec<(GameruleId, GameruleValue)> = rules.iter_all().collect();
        assert_eq!(listed.len(), VANILLA_GAMERULE_COUNT);
        assert_eq!(
            listed.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
            GameruleId::ALL.to_vec()
        );
        for (id, value) in &listed {
            assert_eq!(*value, rules.get(*id));
        }
        assert!(listed.contains(&(GameruleId::RandomTickSpeed, GameruleValue::Int(11))));
    }

    #[test]
    fn reset_restores_every_default() {
        let mut rules = VanillaGamerules::default();
        for id in GameruleId::ALL {
            let flipped = match id.default_value() {
                GameruleValue::Bool(value) => GameruleValue::Bool(!value),
                GameruleValue::Int(value) => GameruleValue::Int(value.wrapping_add(7)),
            };
            assert!(
                rules.set(*id, flipped),
                "{} accepts its own kind",
                id.name()
            );
        }
        assert_ne!(rules, VanillaGamerules::new());
        rules.reset();
        assert_eq!(rules, VanillaGamerules::new());
    }

    #[test]
    fn categories_match_the_vanilla_registration() {
        let category = |name: &str| GameruleId::parse_name(name).expect("known rule").category();
        assert_eq!(category("doFireTick"), GameruleCategory::Updates);
        assert_eq!(category("randomTickSpeed"), GameruleCategory::Updates);
        assert_eq!(category("doDaylightCycle"), GameruleCategory::Updates);
        assert_eq!(category("doWeatherCycle"), GameruleCategory::Updates);
        assert_eq!(category("mobGriefing"), GameruleCategory::Mobs);
        assert_eq!(category("maxEntityCramming"), GameruleCategory::Mobs);
        assert_eq!(category("disableRaids"), GameruleCategory::Mobs);
        assert_eq!(category("doMobSpawning"), GameruleCategory::Spawning);
        assert_eq!(category("doInsomnia"), GameruleCategory::Spawning);
        assert_eq!(category("doWardenSpawning"), GameruleCategory::Spawning);
        assert_eq!(category("doMobLoot"), GameruleCategory::Drops);
        assert_eq!(category("tntExplosionDropDecay"), GameruleCategory::Drops);
        assert_eq!(category("commandBlockOutput"), GameruleCategory::Chat);
        assert_eq!(category("sendCommandFeedback"), GameruleCategory::Chat);
        assert_eq!(category("reducedDebugInfo"), GameruleCategory::Misc);
        assert_eq!(category("spawnChunkRadius"), GameruleCategory::Misc);
        assert_eq!(category("keepInventory"), GameruleCategory::Player);
        assert_eq!(
            category("playersSleepingPercentage"),
            GameruleCategory::Player
        );

        for bucket in GameruleCategory::ALL {
            assert!(
                GameruleId::ALL.iter().any(|id| id.category() == *bucket),
                "{} must own at least one rule",
                bucket.description_id()
            );
        }
        assert_eq!(
            GameruleCategory::Player.description_id(),
            "gamerule.category.player"
        );
        assert_eq!(
            GameruleCategory::Misc.description_id(),
            "gamerule.category.misc"
        );
    }

    #[test]
    fn spawn_chunk_radius_is_the_only_bounded_integer_rule() {
        assert_eq!(SPAWN_CHUNK_RADIUS_RANGE, (0, 32));
        assert_eq!(clamp_spawn_chunk_radius(-1), 0);
        assert_eq!(clamp_spawn_chunk_radius(2), 2);
        assert_eq!(clamp_spawn_chunk_radius(33), 32);
        assert_eq!(clamp_spawn_chunk_radius(i32::MIN), 0);
    }
}
