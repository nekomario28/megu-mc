// SPDX-License-Identifier: LGPL-3.0-or-later
#![deny(unsafe_code)]
//! Declarative single source of truth for the vanilla `1.21.1` save formats.
//!
//! Every place megu reads or writes a vanilla save — `level.dat`, the chunk
//! MCA payloads, `playerdata/<uuid>.dat` and the `data/*.dat` auxiliary
//! `SavedData` files — needs the same answers: which key names exist, which
//! NBT tag carries each one, and which keys are *not* vanilla so they can be
//! preserved instead of dropped. Spreading those answers across the readers
//! and writers is how a `1.20` shape sneaks back in (`ActiveEffects`, `Count`,
//! `tag`) and silently loses data. [`VANILLA_1211_SCHEMA`] holds them once.
//!
//! # What each row states
//!
//! * [`SchemaScope`] — the container the key belongs to: the root compound of
//!   one save format ([`SchemaScope::LevelDat`], [`SchemaScope::ChunkRoot`],
//!   [`SchemaScope::PlayerData`]), a nested vanilla container
//!   ([`SchemaScope::ChunkSection`], [`SchemaScope::BlockEntity`],
//!   [`SchemaScope::ItemStack121`], [`SchemaScope::ActiveEffect121`]), or the
//!   `SavedData` envelope of an auxiliary file ([`SchemaScope::AuxScoreboard`],
//!   [`SchemaScope::AuxRaids`], [`SchemaScope::AuxMap`]).
//! * [`SchemaField::key`] — the exact, case-sensitive NBT key. `active_effects`
//!   is not `ActiveEffects`, `count` is not `Count`, `components` is not `tag`.
//! * [`SchemaField::tag`] — the NBT tag the key is written as. Booleans are
//!   [`NbtTagKind::Byte`] on the wire (NBT has no boolean tag), and a
//!   fixed-length coordinate or rotation triple is a [`NbtTagKind::List`].
//! * [`SchemaField::required`] — `true` when a vanilla `1.21.1` save of that
//!   scope always carries the key, so its absence is a corruption signal;
//!   `false` when vanilla legitimately omits it (an empty-or-absent list, a
//!   spawn point the player never set, the singleplayer `Player` snapshot).
//!   A writer that emits this table must emit every `required` row.
//! * [`SchemaField::since_data_version`] — the lowest `DataVersion` this table
//!   asserts the row for. Every row of the `1.21.1` table is
//!   [`VANILLA_1211_DATA_VERSION`], because the table asserts the `3955` shape
//!   and does not date each key's first appearance; a future `26.3` table adds
//!   its own rows at its own DataVersion instead of editing these.
//!
//! # How the rows were verified
//!
//! Not from prose or memory: each key name and tag was read out of the pinned
//! `1.21.1` client jar (SHA-1 `59353fb40c36d304f2035d51e7d6e6baa98dc05c`,
//! DataVersion `3955`) with `javap -c -p`. A key's row comes from the
//! `CompoundTag` accessor or DFU codec the obfuscated serializer actually
//! calls next to that key's constant: `getInt`/`Codec.INT` is
//! [`NbtTagKind::Int`], `getByte` is [`NbtTagKind::Byte`], `getList` is
//! [`NbtTagKind::List`], `getCompound` is [`NbtTagKind::Compound`], and the
//! `Dynamic.get(...).asBoolean` reader of a boolean field is a byte tag. For
//! example `PrimaryLevelData` reads `Time` with `asLong`, `ItemStack` encodes
//! `count` with the `intRange(1, 99)` codec, and `MobEffectInstance`'s codec
//! holds `amplifier` as a `Codec.BYTE`.
//!
//! # Legacy keys fail closed
//!
//! [`is_legacy_forbidden_121_key`] names the shapes that only exist in `1.20`
//! and earlier: the entity-level `ActiveEffects` list, and `Count`/`tag` on an
//! item stack. None of those shapes is reachable in the `1.21.1` runtime — the
//! `ActiveEffects` string survives only in the `mob_effect_id` and
//! `effect_duration` datafixers, and an item stack's `Count`/`tag` only in the
//! item-componentization fixer, all of which upgrade old data and never write.
//! A reader that meets one must fail closed instead of accepting the value, so
//! the regression surfaces as an error rather than as an item stack that lost
//! its components.
//!
//! # Unknown keys are preserved
//!
//! [`unknown_keys_to_preserve`] turns "present in this compound" into "not
//! vanilla, therefore carry it through verbatim" for `level.dat`, chunk roots
//! and `playerdata`, which is what keeps mod payloads (`ForgeCaps`, a modded
//! `megu:*` key) alive across a load/save cycle. It is the complement of the
//! table, not a filter over the legacy list: a caller that wants fail-closed
//! behavior for a legacy key asks [`is_legacy_forbidden_121_key`] first.

/// The `DataVersion` this table describes: `1.21.1`.
pub const VANILLA_1211_DATA_VERSION: i32 = 3955;

/// The NBT tag a key is written as, for every tag the save formats use.
///
/// Bare `End` is absent on purpose: no schema key is an empty tag.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum NbtTagKind {
    /// `TAG_Byte` — also every boolean, which NBT has no tag for.
    Byte,
    /// `TAG_Short`.
    Short,
    /// `TAG_Int`.
    Int,
    /// `TAG_Long`.
    Long,
    /// `TAG_Float`.
    Float,
    /// `TAG_Double`.
    Double,
    /// `TAG_Byte_Array`.
    ByteArray,
    /// `TAG_String` — a UTF-8 length-prefixed string, resource locations included.
    String,
    /// `TAG_List` — a homogeneous list; its element kind is scope-specific.
    List,
    /// `TAG_Compound` — an unordered key/value map.
    Compound,
    /// `TAG_Int_Array`.
    IntArray,
    /// `TAG_Long_Array`.
    LongArray,
}

/// The compound a [`SchemaField`] describes.
///
/// Scopes are containers, not files: [`SchemaScope::AuxScoreboard`] is the
/// `SavedData` envelope of `data/scoreboard.dat`, while the scoreboard payload
/// nested under its `data` key is vanilla-owned and carried through verbatim
/// rather than enumerated here.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SchemaScope {
    /// The root compound of `level.dat`.
    LevelDat,
    /// One entry of a chunk root's `sections` list.
    ChunkSection,
    /// The root compound of a chunk payload inside `r.<x>.<z>.mca`.
    ChunkRoot,
    /// The root compound of `playerdata/<uuid>.dat` (an entity, saved without its `id`).
    PlayerData,
    /// One block-entity compound inside a chunk's `block_entities` list.
    BlockEntity,
    /// One entry of a living entity's `active_effects` list.
    ActiveEffect121,
    /// One item stack, whether in `Inventory`/`EnderItems`, a container's `Items`, or a slot list.
    ItemStack121,
    /// The `SavedData` envelope of `data/scoreboard.dat`.
    AuxScoreboard,
    /// The `SavedData` envelope of `data/raids.dat`.
    AuxRaids,
    /// The `SavedData` envelope of `data/map_<id>.dat`.
    AuxMap,
}

impl SchemaScope {
    /// Every scope, in declaration order.
    pub const ALL: [SchemaScope; 10] = [
        SchemaScope::LevelDat,
        SchemaScope::ChunkSection,
        SchemaScope::ChunkRoot,
        SchemaScope::PlayerData,
        SchemaScope::BlockEntity,
        SchemaScope::ActiveEffect121,
        SchemaScope::ItemStack121,
        SchemaScope::AuxScoreboard,
        SchemaScope::AuxRaids,
        SchemaScope::AuxMap,
    ];
}

/// One declarative row: a vanilla key, its tag, and whether vanilla always writes it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SchemaField {
    /// The compound the key belongs to.
    pub scope: SchemaScope,
    /// The exact, case-sensitive NBT key.
    pub key: &'static str,
    /// The tag the key is written as.
    pub tag: NbtTagKind,
    /// The lowest `DataVersion` this row is asserted for.
    pub since_data_version: i32,
    /// Whether a vanilla save of this scope always carries the key.
    pub required: bool,
}

/// Builds a [`SchemaField`] asserted at [`VANILLA_1211_DATA_VERSION`].
const fn field(
    scope: SchemaScope,
    key: &'static str,
    tag: NbtTagKind,
    required: bool,
) -> SchemaField {
    SchemaField {
        scope,
        key,
        tag,
        since_data_version: VANILLA_1211_DATA_VERSION,
        required,
    }
}

/// The `1.21.1` (`DataVersion` `3955`) save-format schema.
///
/// Rows are grouped by scope and, inside a scope, by what they describe. The
/// table is sorted per scope rather than globally so a reviewer can diff it
/// against the vanilla serializer of that scope one block at a time.
///
/// One row per `field(...)` call is the whole point of the table, so it opts out
/// of `rustfmt`, which would otherwise split every row across five lines.
#[rustfmt::skip]
pub const VANILLA_1211_SCHEMA: &[SchemaField] = &[
    // ---- level.dat: format metadata -------------------------------------
    field(SchemaScope::LevelDat, "DataVersion", NbtTagKind::Int, true),
    field(SchemaScope::LevelDat, "Version", NbtTagKind::Compound, true),
    field(SchemaScope::LevelDat, "version", NbtTagKind::Int, true),
    field(SchemaScope::LevelDat, "WasModded", NbtTagKind::Byte, true),
    field(SchemaScope::LevelDat, "ServerBrands", NbtTagKind::List, true),
    field(SchemaScope::LevelDat, "removed_features", NbtTagKind::List, false),
    // ---- level.dat: generator, rules and data packs ---------------------
    field(SchemaScope::LevelDat, "WorldGenSettings", NbtTagKind::Compound, true),
    field(SchemaScope::LevelDat, "GameRules", NbtTagKind::Compound, true),
    field(SchemaScope::LevelDat, "DataPacks", NbtTagKind::Compound, true),
    // ---- level.dat: clock, weather and play time ------------------------
    field(SchemaScope::LevelDat, "Time", NbtTagKind::Long, true),
    field(SchemaScope::LevelDat, "DayTime", NbtTagKind::Long, true),
    field(SchemaScope::LevelDat, "LastPlayed", NbtTagKind::Long, true),
    field(SchemaScope::LevelDat, "raining", NbtTagKind::Byte, true),
    field(SchemaScope::LevelDat, "rainTime", NbtTagKind::Int, true),
    field(SchemaScope::LevelDat, "thundering", NbtTagKind::Byte, true),
    field(SchemaScope::LevelDat, "thunderTime", NbtTagKind::Int, true),
    field(SchemaScope::LevelDat, "clearWeatherTime", NbtTagKind::Int, true),
    // ---- level.dat: spawn and game mode ---------------------------------
    field(SchemaScope::LevelDat, "SpawnX", NbtTagKind::Int, true),
    field(SchemaScope::LevelDat, "SpawnY", NbtTagKind::Int, true),
    field(SchemaScope::LevelDat, "SpawnZ", NbtTagKind::Int, true),
    field(SchemaScope::LevelDat, "SpawnAngle", NbtTagKind::Float, true),
    field(SchemaScope::LevelDat, "GameType", NbtTagKind::Int, true),
    field(SchemaScope::LevelDat, "allowCommands", NbtTagKind::Byte, true),
    field(SchemaScope::LevelDat, "hardcore", NbtTagKind::Byte, true),
    field(SchemaScope::LevelDat, "initialized", NbtTagKind::Byte, true),
    field(SchemaScope::LevelDat, "Difficulty", NbtTagKind::Byte, true),
    field(SchemaScope::LevelDat, "DifficultyLocked", NbtTagKind::Byte, true),
    field(SchemaScope::LevelDat, "LevelName", NbtTagKind::String, true),
    // ---- level.dat: world border (all nine settings) --------------------
    field(SchemaScope::LevelDat, "BorderSize", NbtTagKind::Double, true),
    field(SchemaScope::LevelDat, "BorderCenterX", NbtTagKind::Double, true),
    field(SchemaScope::LevelDat, "BorderCenterZ", NbtTagKind::Double, true),
    field(SchemaScope::LevelDat, "BorderDamagePerBlock", NbtTagKind::Double, true),
    field(SchemaScope::LevelDat, "BorderSafeZone", NbtTagKind::Double, true),
    field(SchemaScope::LevelDat, "BorderWarningBlocks", NbtTagKind::Int, true),
    field(SchemaScope::LevelDat, "BorderWarningTime", NbtTagKind::Int, true),
    field(SchemaScope::LevelDat, "BorderSizeLerpTarget", NbtTagKind::Double, true),
    field(SchemaScope::LevelDat, "BorderSizeLerpTime", NbtTagKind::Long, true),
    // ---- level.dat: boss, dragon and trader state -----------------------
    field(SchemaScope::LevelDat, "DragonFight", NbtTagKind::Compound, true),
    field(SchemaScope::LevelDat, "CustomBossEvents", NbtTagKind::Compound, false),
    field(SchemaScope::LevelDat, "ScheduledEvents", NbtTagKind::List, true),
    field(SchemaScope::LevelDat, "WanderingTraderId", NbtTagKind::IntArray, false),
    field(SchemaScope::LevelDat, "WanderingTraderSpawnDelay", NbtTagKind::Int, true),
    field(SchemaScope::LevelDat, "WanderingTraderSpawnChance", NbtTagKind::Int, true),
    // ---- level.dat: the singleplayer player snapshot --------------------
    field(SchemaScope::LevelDat, "Player", NbtTagKind::Compound, false),
    // ---- chunk root -----------------------------------------------------
    field(SchemaScope::ChunkRoot, "DataVersion", NbtTagKind::Int, true),
    field(SchemaScope::ChunkRoot, "xPos", NbtTagKind::Int, true),
    field(SchemaScope::ChunkRoot, "yPos", NbtTagKind::Int, true),
    field(SchemaScope::ChunkRoot, "zPos", NbtTagKind::Int, true),
    field(SchemaScope::ChunkRoot, "Status", NbtTagKind::String, true),
    field(SchemaScope::ChunkRoot, "LastUpdate", NbtTagKind::Long, true),
    field(SchemaScope::ChunkRoot, "InhabitedTime", NbtTagKind::Long, true),
    field(SchemaScope::ChunkRoot, "sections", NbtTagKind::List, true),
    field(SchemaScope::ChunkRoot, "block_entities", NbtTagKind::List, true),
    field(SchemaScope::ChunkRoot, "block_ticks", NbtTagKind::List, true),
    field(SchemaScope::ChunkRoot, "fluid_ticks", NbtTagKind::List, true),
    field(SchemaScope::ChunkRoot, "Heightmaps", NbtTagKind::Compound, true),
    field(SchemaScope::ChunkRoot, "structures", NbtTagKind::Compound, true),
    field(SchemaScope::ChunkRoot, "isLightOn", NbtTagKind::Byte, false),
    field(SchemaScope::ChunkRoot, "UpgradeData", NbtTagKind::Compound, false),
    field(SchemaScope::ChunkRoot, "CarvingMasks", NbtTagKind::Compound, false),
    field(SchemaScope::ChunkRoot, "blending_data", NbtTagKind::Compound, false),
    field(SchemaScope::ChunkRoot, "below_zero_retrogen", NbtTagKind::Compound, false),
    field(SchemaScope::ChunkRoot, "PostProcessing", NbtTagKind::List, false),
    // ---- chunk section --------------------------------------------------
    field(SchemaScope::ChunkSection, "Y", NbtTagKind::Byte, true),
    field(SchemaScope::ChunkSection, "block_states", NbtTagKind::Compound, true),
    field(SchemaScope::ChunkSection, "biomes", NbtTagKind::Compound, true),
    // ---- block entity ---------------------------------------------------
    field(SchemaScope::BlockEntity, "id", NbtTagKind::String, true),
    field(SchemaScope::BlockEntity, "x", NbtTagKind::Int, true),
    field(SchemaScope::BlockEntity, "y", NbtTagKind::Int, true),
    field(SchemaScope::BlockEntity, "z", NbtTagKind::Int, true),
    field(SchemaScope::BlockEntity, "Items", NbtTagKind::List, false),
    field(SchemaScope::BlockEntity, "LootTable", NbtTagKind::String, false),
    field(SchemaScope::BlockEntity, "LootTableSeed", NbtTagKind::Long, false),
    // ---- playerdata -----------------------------------------------------
    field(SchemaScope::PlayerData, "DataVersion", NbtTagKind::Int, false),
    field(SchemaScope::PlayerData, "Pos", NbtTagKind::List, true),
    field(SchemaScope::PlayerData, "Motion", NbtTagKind::List, true),
    field(SchemaScope::PlayerData, "Rotation", NbtTagKind::List, true),
    field(SchemaScope::PlayerData, "Dimension", NbtTagKind::String, true),
    field(SchemaScope::PlayerData, "Health", NbtTagKind::Float, true),
    field(SchemaScope::PlayerData, "Inventory", NbtTagKind::List, true),
    field(SchemaScope::PlayerData, "EnderItems", NbtTagKind::List, true),
    field(SchemaScope::PlayerData, "active_effects", NbtTagKind::List, true),
    field(SchemaScope::PlayerData, "attributes", NbtTagKind::List, true),
    field(SchemaScope::PlayerData, "abilities", NbtTagKind::Compound, true),
    field(SchemaScope::PlayerData, "foodLevel", NbtTagKind::Int, true),
    field(SchemaScope::PlayerData, "foodSaturationLevel", NbtTagKind::Float, true),
    field(SchemaScope::PlayerData, "foodExhaustionLevel", NbtTagKind::Float, true),
    field(SchemaScope::PlayerData, "XpLevel", NbtTagKind::Int, true),
    field(SchemaScope::PlayerData, "XpP", NbtTagKind::Float, true),
    field(SchemaScope::PlayerData, "XpTotal", NbtTagKind::Int, true),
    field(SchemaScope::PlayerData, "playerGameType", NbtTagKind::Int, true),
    field(SchemaScope::PlayerData, "previousPlayerGameType", NbtTagKind::Int, true),
    field(SchemaScope::PlayerData, "SpawnX", NbtTagKind::Int, false),
    field(SchemaScope::PlayerData, "SpawnY", NbtTagKind::Int, false),
    field(SchemaScope::PlayerData, "SpawnZ", NbtTagKind::Int, false),
    field(SchemaScope::PlayerData, "SpawnDimension", NbtTagKind::String, true),
    field(SchemaScope::PlayerData, "SpawnAngle", NbtTagKind::Float, false),
    field(SchemaScope::PlayerData, "SpawnForced", NbtTagKind::Byte, false),
    field(SchemaScope::PlayerData, "LastDeathLocation", NbtTagKind::Compound, false),
    field(SchemaScope::PlayerData, "recipeBook", NbtTagKind::Compound, true),
    field(SchemaScope::PlayerData, "warden_spawn_tracker", NbtTagKind::Compound, false),
    // ---- one active effect (1.21 lowercase shape) -----------------------
    field(SchemaScope::ActiveEffect121, "id", NbtTagKind::String, true),
    field(SchemaScope::ActiveEffect121, "amplifier", NbtTagKind::Byte, true),
    field(SchemaScope::ActiveEffect121, "duration", NbtTagKind::Int, true),
    field(SchemaScope::ActiveEffect121, "ambient", NbtTagKind::Byte, true),
    field(SchemaScope::ActiveEffect121, "show_particles", NbtTagKind::Byte, true),
    field(SchemaScope::ActiveEffect121, "show_icon", NbtTagKind::Byte, true),
    // ---- one item stack (1.21 component shape) --------------------------
    field(SchemaScope::ItemStack121, "id", NbtTagKind::String, true),
    field(SchemaScope::ItemStack121, "count", NbtTagKind::Int, true),
    field(SchemaScope::ItemStack121, "components", NbtTagKind::Compound, false),
    field(SchemaScope::ItemStack121, "Slot", NbtTagKind::Byte, false),
    // ---- SavedData envelopes of data/*.dat ------------------------------
    field(SchemaScope::AuxScoreboard, "data", NbtTagKind::Compound, true),
    field(SchemaScope::AuxScoreboard, "DataVersion", NbtTagKind::Int, true),
    field(SchemaScope::AuxRaids, "data", NbtTagKind::Compound, true),
    field(SchemaScope::AuxRaids, "DataVersion", NbtTagKind::Int, true),
    field(SchemaScope::AuxMap, "data", NbtTagKind::Compound, true),
    field(SchemaScope::AuxMap, "DataVersion", NbtTagKind::Int, true),
];

/// Every field declared for `scope`, in table order.
#[must_use = "an unused iterator means the caller never inspected the scope"]
pub fn fields_for(scope: SchemaScope) -> impl Iterator<Item = &'static SchemaField> {
    VANILLA_1211_SCHEMA
        .iter()
        .filter(move |field| field.scope == scope)
}

/// The row for `(scope, key)`, or `None` when the key is not vanilla there.
///
/// The lookup is scope-exact: `DataVersion` in [`SchemaScope::LevelDat`] and in
/// [`SchemaScope::ChunkRoot`] are two rows, and a key declared for one scope is
/// unknown for another.
#[must_use = "an ignored lookup silently treats a vanilla key as unknown"]
pub fn lookup_field(scope: SchemaScope, key: &str) -> Option<&'static SchemaField> {
    VANILLA_1211_SCHEMA
        .iter()
        .find(|field| field.scope == scope && field.key == key)
}

/// Whether `key` is a `1.20`-and-earlier shape that no `1.21.1` save carries.
///
/// `ActiveEffects` is forbidden in every scope: the `1.21.1` entity serializer
/// writes `active_effects`, and the string only survives in the effect
/// datafixers that upgrade old saves. `Count` and `tag` are forbidden only on
/// [`SchemaScope::ItemStack121`], where `count` and `components` replaced them.
/// That restriction is not cosmetic: `Count` is a live vanilla key elsewhere in
/// `1.21.1` (`ExperienceOrb` saves it as an `Int`), so a global flag would
/// reject a valid entity instead of a stale item stack.
///
/// A reader that meets a forbidden key must fail closed rather than accept the
/// value: silently reading `Count`/`tag` is how an item stack arrives with no
/// components and loses its enchantments on the next save.
#[must_use = "the result is the fail-closed verdict and must be acted on"]
pub fn is_legacy_forbidden_121_key(scope: SchemaScope, key: &str) -> bool {
    match key {
        "ActiveEffects" => true,
        "Count" | "tag" => scope == SchemaScope::ItemStack121,
        _ => false,
    }
}

/// The keys in `present_keys` that this table does not declare for `scope`.
///
/// This is the lossless-preservation driver for `level.dat`, chunk roots and
/// `playerdata`: everything it returns came from a mod, a newer vanilla
/// version, or another loader, so a save must write it back verbatim instead of
/// dropping it. Caller order is kept, and a repeated key collapses into its
/// first occurrence because an NBT compound cannot hold the key twice.
///
/// Legacy keys are *not* excluded here — [`is_legacy_forbidden_121_key`] is the
/// separate fail-closed gate, and a caller may want to reject rather than
/// preserve one.
#[must_use = "dropping the result is how a mod key is lost on save"]
pub fn unknown_keys_to_preserve<'a>(
    scope: SchemaScope,
    present_keys: impl IntoIterator<Item = &'a str>,
) -> Vec<&'a str> {
    let mut preserved: Vec<&'a str> = Vec::new();
    for key in present_keys {
        if lookup_field(scope, key).is_some() || preserved.contains(&key) {
            continue;
        }
        preserved.push(key);
    }
    preserved
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    /// The audit's persistence contract, scope by scope: every key a
    /// `1.21.1` reader or writer is allowed to name must have a row.
    const CONTRACT_KEYS: &[(SchemaScope, &[&str])] = &[
        (
            SchemaScope::LevelDat,
            &[
                "DataVersion",
                "Version",
                "WorldGenSettings",
                "GameRules",
                "Time",
                "DayTime",
                "raining",
                "rainTime",
                "thundering",
                "thunderTime",
                "clearWeatherTime",
                "SpawnX",
                "SpawnY",
                "SpawnZ",
                "SpawnAngle",
                "BorderSize",
                "BorderCenterX",
                "BorderCenterZ",
                "BorderDamagePerBlock",
                "BorderSafeZone",
                "BorderWarningBlocks",
                "BorderWarningTime",
                "BorderSizeLerpTarget",
                "BorderSizeLerpTime",
                "Difficulty",
                "DifficultyLocked",
                "DragonFight",
                "WanderingTraderId",
                "WanderingTraderSpawnChance",
                "WanderingTraderSpawnDelay",
                "CustomBossEvents",
                "ScheduledEvents",
                "DataPacks",
                "allowCommands",
                "hardcore",
                "initialized",
                "LevelName",
                "LastPlayed",
            ],
        ),
        (
            SchemaScope::ChunkRoot,
            &[
                "DataVersion",
                "xPos",
                "yPos",
                "zPos",
                "Status",
                "LastUpdate",
                "InhabitedTime",
                "sections",
                "block_entities",
                "block_ticks",
                "fluid_ticks",
                "Heightmaps",
                "structures",
                "isLightOn",
            ],
        ),
        (
            SchemaScope::PlayerData,
            &[
                "DataVersion",
                "Pos",
                "Motion",
                "Rotation",
                "Dimension",
                "Inventory",
                "EnderItems",
                "active_effects",
                "attributes",
                "abilities",
                "Health",
                "foodLevel",
                "foodSaturationLevel",
                "foodExhaustionLevel",
                "XpLevel",
                "XpP",
                "XpTotal",
                "playerGameType",
                "previousPlayerGameType",
                "SpawnX",
                "SpawnY",
                "SpawnZ",
                "SpawnDimension",
                "SpawnAngle",
                "SpawnForced",
                "LastDeathLocation",
                "recipeBook",
                "warden_spawn_tracker",
            ],
        ),
        (
            SchemaScope::ItemStack121,
            &["id", "count", "components", "Slot"],
        ),
        (
            SchemaScope::ActiveEffect121,
            &[
                "id",
                "amplifier",
                "duration",
                "ambient",
                "show_particles",
                "show_icon",
            ],
        ),
    ];

    #[test]
    fn data_version_is_pinned_to_3955() {
        assert_eq!(VANILLA_1211_DATA_VERSION, 3955);
    }

    #[test]
    fn the_contract_keys_all_have_a_row() {
        for (scope, keys) in CONTRACT_KEYS {
            for key in *keys {
                assert!(
                    lookup_field(*scope, key).is_some(),
                    "{scope:?} is missing the vanilla key {key:?}"
                );
            }
        }
    }

    #[test]
    fn scope_and_key_pairs_are_unique() {
        let mut seen: BTreeSet<(SchemaScope, &'static str)> = BTreeSet::new();
        for field in VANILLA_1211_SCHEMA {
            assert!(
                seen.insert((field.scope, field.key)),
                "{:?} declares {:?} twice",
                field.scope,
                field.key
            );
        }
        assert_eq!(seen.len(), VANILLA_1211_SCHEMA.len());
    }

    #[test]
    fn every_scope_has_rows_and_none_is_empty() {
        for scope in SchemaScope::ALL {
            assert!(
                fields_for(scope).next().is_some(),
                "{scope:?} has no rows; a scope with no rows cannot drive preservation"
            );
        }
    }

    #[test]
    fn every_row_is_asserted_at_the_pinned_data_version() {
        for field in VANILLA_1211_SCHEMA {
            assert_eq!(field.since_data_version, VANILLA_1211_DATA_VERSION);
            assert!(!field.key.is_empty());
        }
    }

    #[test]
    fn fields_for_returns_only_the_requested_scope() {
        for scope in SchemaScope::ALL {
            let expected = VANILLA_1211_SCHEMA
                .iter()
                .filter(|field| field.scope == scope)
                .count();
            let declared = fields_for(scope).count();
            assert_eq!(declared, expected);
            assert!(declared > 0);
            assert!(fields_for(scope).all(|field| field.scope == scope));
        }
    }

    #[test]
    fn lookup_is_scope_exact() {
        // `DataVersion` exists at three roots but not on a section.
        assert!(lookup_field(SchemaScope::LevelDat, "DataVersion").is_some());
        assert!(lookup_field(SchemaScope::ChunkRoot, "DataVersion").is_some());
        assert!(lookup_field(SchemaScope::PlayerData, "DataVersion").is_some());
        assert!(lookup_field(SchemaScope::ChunkSection, "DataVersion").is_none());
        // A chunk key is not a level key of the same name by accident.
        assert!(lookup_field(SchemaScope::ChunkRoot, "xPos").is_some());
        assert!(lookup_field(SchemaScope::LevelDat, "xPos").is_none());
        // `Time` is a level key only; a section has no clock.
        assert!(lookup_field(SchemaScope::LevelDat, "Time").is_some());
        assert!(lookup_field(SchemaScope::ChunkSection, "Time").is_none());
    }

    #[test]
    fn the_1_21_shapes_are_pinned_by_tag() {
        let pinned = [
            (SchemaScope::LevelDat, "DataVersion", NbtTagKind::Int),
            (SchemaScope::LevelDat, "Time", NbtTagKind::Long),
            (SchemaScope::LevelDat, "DayTime", NbtTagKind::Long),
            (SchemaScope::LevelDat, "raining", NbtTagKind::Byte),
            (SchemaScope::LevelDat, "Difficulty", NbtTagKind::Byte),
            (SchemaScope::LevelDat, "DifficultyLocked", NbtTagKind::Byte),
            (SchemaScope::LevelDat, "SpawnAngle", NbtTagKind::Float),
            (SchemaScope::LevelDat, "BorderSize", NbtTagKind::Double),
            (
                SchemaScope::LevelDat,
                "BorderSizeLerpTime",
                NbtTagKind::Long,
            ),
            (SchemaScope::LevelDat, "BorderWarningTime", NbtTagKind::Int),
            (
                SchemaScope::LevelDat,
                "WanderingTraderId",
                NbtTagKind::IntArray,
            ),
            (SchemaScope::LevelDat, "ScheduledEvents", NbtTagKind::List),
            (SchemaScope::LevelDat, "DataPacks", NbtTagKind::Compound),
            (SchemaScope::LevelDat, "LevelName", NbtTagKind::String),
            (SchemaScope::ChunkRoot, "xPos", NbtTagKind::Int),
            (SchemaScope::ChunkRoot, "Status", NbtTagKind::String),
            (SchemaScope::ChunkRoot, "InhabitedTime", NbtTagKind::Long),
            (SchemaScope::ChunkRoot, "sections", NbtTagKind::List),
            (SchemaScope::ChunkRoot, "Heightmaps", NbtTagKind::Compound),
            (SchemaScope::ChunkRoot, "isLightOn", NbtTagKind::Byte),
            (SchemaScope::ChunkSection, "Y", NbtTagKind::Byte),
            (SchemaScope::PlayerData, "active_effects", NbtTagKind::List),
            (SchemaScope::PlayerData, "abilities", NbtTagKind::Compound),
            (SchemaScope::PlayerData, "Health", NbtTagKind::Float),
            (SchemaScope::PlayerData, "foodLevel", NbtTagKind::Int),
            (SchemaScope::PlayerData, "XpP", NbtTagKind::Float),
            (
                SchemaScope::PlayerData,
                "SpawnDimension",
                NbtTagKind::String,
            ),
            (SchemaScope::PlayerData, "SpawnForced", NbtTagKind::Byte),
            (
                SchemaScope::PlayerData,
                "LastDeathLocation",
                NbtTagKind::Compound,
            ),
            (SchemaScope::ActiveEffect121, "id", NbtTagKind::String),
            (SchemaScope::ActiveEffect121, "amplifier", NbtTagKind::Byte),
            (SchemaScope::ActiveEffect121, "duration", NbtTagKind::Int),
            (
                SchemaScope::ActiveEffect121,
                "show_particles",
                NbtTagKind::Byte,
            ),
            (SchemaScope::ItemStack121, "id", NbtTagKind::String),
            (SchemaScope::ItemStack121, "count", NbtTagKind::Int),
            (
                SchemaScope::ItemStack121,
                "components",
                NbtTagKind::Compound,
            ),
            (SchemaScope::ItemStack121, "Slot", NbtTagKind::Byte),
            (SchemaScope::BlockEntity, "x", NbtTagKind::Int),
            (SchemaScope::BlockEntity, "Items", NbtTagKind::List),
            (SchemaScope::AuxMap, "data", NbtTagKind::Compound),
        ];
        for (scope, key, tag) in pinned {
            let field =
                lookup_field(scope, key).unwrap_or_else(|| panic!("{scope:?} is missing {key:?}"));
            assert_eq!(field.tag, tag, "{scope:?}/{key:?} has the wrong tag");
        }
    }

    #[test]
    fn legacy_1_20_keys_are_rejected() {
        // The 1.21 entity shape is `active_effects`; `ActiveEffects` is gone.
        assert!(is_legacy_forbidden_121_key(
            SchemaScope::PlayerData,
            "ActiveEffects"
        ));
        assert!(!is_legacy_forbidden_121_key(
            SchemaScope::PlayerData,
            "active_effects"
        ));
        // `ActiveEffects` is not scope-specific: no 1.21.1 serializer writes it.
        assert!(is_legacy_forbidden_121_key(
            SchemaScope::ItemStack121,
            "ActiveEffects"
        ));
        // The item stack renamed `Count` -> `count` and `tag` -> `components`.
        assert!(is_legacy_forbidden_121_key(
            SchemaScope::ItemStack121,
            "Count"
        ));
        assert!(is_legacy_forbidden_121_key(
            SchemaScope::ItemStack121,
            "tag"
        ));
        assert!(!is_legacy_forbidden_121_key(
            SchemaScope::ItemStack121,
            "count"
        ));
        assert!(!is_legacy_forbidden_121_key(
            SchemaScope::ItemStack121,
            "components"
        ));
        // `tag`/`Count` outside an item stack are different, legitimate keys:
        // a dropped experience orb still saves `Count` as an `Int`.
        assert!(!is_legacy_forbidden_121_key(SchemaScope::ChunkRoot, "tag"));
        assert!(!is_legacy_forbidden_121_key(
            SchemaScope::AuxScoreboard,
            "Count"
        ));
    }

    #[test]
    fn the_table_never_declares_a_legacy_key() {
        for field in VANILLA_1211_SCHEMA {
            assert!(
                !is_legacy_forbidden_121_key(field.scope, field.key),
                "{:?}/{:?} declares a legacy key",
                field.scope,
                field.key
            );
        }
        assert!(lookup_field(SchemaScope::PlayerData, "ActiveEffects").is_none());
        assert!(lookup_field(SchemaScope::ItemStack121, "Count").is_none());
        assert!(lookup_field(SchemaScope::ItemStack121, "tag").is_none());
    }

    #[test]
    fn unknown_keys_to_preserve_keeps_mod_payloads_in_order() {
        let present = ["DataVersion", "megu:custom", "Time", "ForgeCaps"];
        assert_eq!(
            unknown_keys_to_preserve(SchemaScope::LevelDat, present),
            vec!["megu:custom", "ForgeCaps"]
        );
    }

    #[test]
    fn unknown_keys_to_preserve_is_empty_for_a_pure_vanilla_compound() {
        let declared: Vec<&str> = fields_for(SchemaScope::ChunkRoot)
            .map(|field| field.key)
            .collect();
        assert_eq!(
            unknown_keys_to_preserve(SchemaScope::ChunkRoot, declared.clone()),
            Vec::<&str>::new()
        );
        // ... and every one of those keys is known, so nothing is preserved twice.
        assert_eq!(
            unknown_keys_to_preserve(SchemaScope::ChunkRoot, ["DataVersion", "xPos"]),
            Vec::<&str>::new()
        );
    }

    #[test]
    fn unknown_keys_to_preserve_collapses_repeats() {
        let present = ["mod:one", "mod:one", "Time", "mod:two"];
        assert_eq!(
            unknown_keys_to_preserve(SchemaScope::LevelDat, present),
            vec!["mod:one", "mod:two"]
        );
    }

    #[test]
    fn unknown_keys_to_preserve_is_scope_exact() {
        // `xPos` is vanilla in a chunk, so a chunk preserves nothing.
        assert_eq!(
            unknown_keys_to_preserve(SchemaScope::ChunkRoot, ["xPos"]),
            Vec::<&str>::new()
        );
        // In `level.dat` it is unknown, so a save must carry it through.
        assert_eq!(
            unknown_keys_to_preserve(SchemaScope::LevelDat, ["xPos"]),
            vec!["xPos"]
        );
    }

    #[test]
    fn unknown_keys_to_preserve_can_be_combined_with_the_legacy_gate() {
        // The gate is separate from preservation: a caller rejects legacy keys
        // and preserves the rest instead of writing a 1.20 shape back.
        let present = ["id", "count", "tag", "megu:attachment"];
        let legacy: Vec<&str> = present
            .iter()
            .copied()
            .filter(|key| is_legacy_forbidden_121_key(SchemaScope::ItemStack121, key))
            .collect();
        assert_eq!(legacy, vec!["tag"]);
        // `id`/`count` are vanilla, so they are known and stay put; only the
        // mod key is carried through verbatim.
        let preserved = unknown_keys_to_preserve(
            SchemaScope::ItemStack121,
            present
                .iter()
                .copied()
                .filter(|key| !is_legacy_forbidden_121_key(SchemaScope::ItemStack121, key)),
        );
        assert_eq!(preserved, vec!["megu:attachment"]);
    }

    #[test]
    fn required_rows_cover_the_writer_contract() {
        // A writer that emits this table must emit these.
        for (scope, key) in [
            (SchemaScope::LevelDat, "DataVersion"),
            (SchemaScope::LevelDat, "LevelName"),
            (SchemaScope::LevelDat, "GameRules"),
            (SchemaScope::ChunkRoot, "Status"),
            (SchemaScope::ChunkRoot, "sections"),
            (SchemaScope::PlayerData, "Inventory"),
            (SchemaScope::PlayerData, "active_effects"),
            (SchemaScope::ItemStack121, "count"),
            (SchemaScope::ActiveEffect121, "id"),
        ] {
            let field =
                lookup_field(scope, key).unwrap_or_else(|| panic!("{scope:?} is missing {key:?}"));
            assert!(field.required, "{scope:?}/{key:?} must be required");
        }
        // The rows vanilla only writes conditionally stay optional.
        for (scope, key) in [
            (SchemaScope::LevelDat, "Player"),
            (SchemaScope::LevelDat, "CustomBossEvents"),
            (SchemaScope::LevelDat, "WanderingTraderId"),
            (SchemaScope::ChunkRoot, "isLightOn"),
            (SchemaScope::PlayerData, "SpawnX"),
            (SchemaScope::PlayerData, "LastDeathLocation"),
            (SchemaScope::ItemStack121, "components"),
            (SchemaScope::ItemStack121, "Slot"),
        ] {
            let field =
                lookup_field(scope, key).unwrap_or_else(|| panic!("{scope:?} is missing {key:?}"));
            assert!(!field.required, "{scope:?}/{key:?} must stay optional");
        }
    }
}
