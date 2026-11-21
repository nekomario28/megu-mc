// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! Protocol-version identity and structured capability flags.
//!
//! Downstream code branches on [`VersionCapabilities`] instead of scattering
//! `protocol == 767`-style checks (the pattern `src/play_protocol.rs` already
//! uses for its `protocol_1_21_3`/`protocol_1_21_4` forks).
//!
//! The two primary targets (`1.21.1` and `26.3`) source every constant —
//! protocol number, release id, `DataVersion`, pack formats — from
//! [`mc_spine_core::version::TargetVersion`], the single source of truth the
//! whole crate DAG shares; the historical `1.21.3`/`1.21.4`/`1.21.5` rows stay
//! local since `mc-spine-core` only pins the two current targets.

use std::fmt;

use mc_spine_core::version::TargetVersion;

/// A negotiated Java-protocol version number.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ProtocolVersion {
    /// Minecraft 1.21 / 1.21.1 — the protocol mc-spine speaks today.
    V1_21_1,
    /// Minecraft 1.21.2 / 1.21.3.
    V1_21_3,
    /// Minecraft 1.21.4.
    V1_21_4,
    /// Minecraft 1.21.5.
    V1_21_5,
    /// Minecraft 26.3 — the latest vanilla release in the target set.
    V26_3,
    /// Any other (snapshot, newer, or older) numeric protocol.
    Custom(i32),
}

impl ProtocolVersion {
    pub const V1_21_1_NUMBER: i32 = TargetVersion::V1_21_1.protocol();
    pub const V1_21_3_NUMBER: i32 = 768;
    pub const V1_21_4_NUMBER: i32 = 769;
    pub const V1_21_5_NUMBER: i32 = 770;
    pub const V26_3_NUMBER: i32 = TargetVersion::V26_3.protocol();

    /// Map a wire/handshake number to the enum, keeping unknown numbers.
    pub fn from_number(number: i32) -> Self {
        match number {
            Self::V1_21_1_NUMBER => Self::V1_21_1,
            Self::V1_21_3_NUMBER => Self::V1_21_3,
            Self::V1_21_4_NUMBER => Self::V1_21_4,
            Self::V1_21_5_NUMBER => Self::V1_21_5,
            Self::V26_3_NUMBER => Self::V26_3,
            other => Self::Custom(other),
        }
    }

    /// The numeric protocol version used in handshakes and the `server_list_ping`.
    pub const fn number(self) -> i32 {
        match self {
            Self::V1_21_1 => Self::V1_21_1_NUMBER,
            Self::V1_21_3 => Self::V1_21_3_NUMBER,
            Self::V1_21_4 => Self::V1_21_4_NUMBER,
            Self::V1_21_5 => Self::V1_21_5_NUMBER,
            Self::V26_3 => Self::V26_3_NUMBER,
            Self::Custom(n) => n,
        }
    }

    /// The canonical Minecraft release name, when this is a known release.
    pub fn mc_version(self) -> Option<&'static str> {
        match self {
            Self::V1_21_1 => Some(TargetVersion::V1_21_1.id()),
            Self::V1_21_3 => Some("1.21.3"),
            Self::V1_21_4 => Some("1.21.4"),
            Self::V1_21_5 => Some("1.21.5"),
            Self::V26_3 => Some(TargetVersion::V26_3.id()),
            Self::Custom(_) => None,
        }
    }

    /// Structured capabilities for this version.
    pub fn capabilities(self) -> VersionCapabilities {
        match self {
            Self::V1_21_1 => CAP_1_21_1,
            Self::V1_21_3 => CAP_1_21_3,
            Self::V1_21_4 => CAP_1_21_4,
            Self::V1_21_5 => CAP_1_21_5,
            Self::V26_3 => CAP_26_3,
            // Unknown versions get the oldest known profile as a conservative
            // baseline; callers that need exact behaviour must check the
            // concrete `protocol_version` field anyway.
            Self::Custom(_) => CAP_1_21_1.with_protocol(self.number()),
        }
    }

    /// The lowest protocol version the codebase can negotiate.
    pub const MIN_SUPPORTED: i32 = Self::V1_21_1_NUMBER;
    /// The highest protocol version this crate ships a table for.
    pub const MAX_KNOWN: i32 = Self::V26_3_NUMBER;
}

impl fmt::Display for ProtocolVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.mc_version() {
            Some(name) => write!(f, "{name} (protocol {})", self.number()),
            None => write!(f, "protocol {}", self.number()),
        }
    }
}

/// Error from `TryFrom<ProtocolVersion>` for [`TargetVersion`]: the protocol
/// version is not one of the two canonical targets (`1.21.1`, `26.3`).
///
/// `V1_21_3`/`V1_21_4`/`V1_21_5` are real protocols mc-spine ships tables for
/// but they sit outside the `mc-spine-core` target set, and `Custom` is by
/// definition unidentified — all three cases fail here instead of silently
/// collapsing onto the nearest target.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct NoTargetVersion(pub ProtocolVersion);

impl fmt::Display for NoTargetVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} is not a canonical mc-spine target version", self.0)
    }
}

impl std::error::Error for NoTargetVersion {}

impl From<TargetVersion> for ProtocolVersion {
    fn from(target: TargetVersion) -> Self {
        match target {
            TargetVersion::V1_21_1 => Self::V1_21_1,
            TargetVersion::V26_3 => Self::V26_3,
        }
    }
}

impl TryFrom<ProtocolVersion> for TargetVersion {
    type Error = NoTargetVersion;

    fn try_from(version: ProtocolVersion) -> Result<Self, Self::Error> {
        match version {
            ProtocolVersion::V1_21_1 => Ok(Self::V1_21_1),
            ProtocolVersion::V26_3 => Ok(Self::V26_3),
            other => Err(NoTargetVersion(other)),
        }
    }
}

/// How a version distributes the recipe book.
///
/// - [`RecipeSyncMode::FullOnJoin`]: 1.21.x servers push the whole unlocked
///   recipe set in `unlock_recipes` during play login — the mode mc-spine's
///   `src/recipes_and_stats.rs` emits today.
/// - [`RecipeSyncMode::LazyOnDemand`]: newer protocols fetch recipes on demand;
///   declared now so the capability switch is in place before 1.21.6+ packs land.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum RecipeSyncMode {
    /// The server proactively sends its complete recipe set when play starts.
    FullOnJoin,
    /// Recipes resolve against a versioned catalog as the client asks for them.
    LazyOnDemand,
    /// The server syncs *recipe displays* — the 26.x `recipe_book_add` /
    /// `recipe_book_remove` / `recipe_book_settings` clientbound packets —
    /// instead of the 1.21.x `update_recipes` + `unlock_recipes` pair. The
    /// client renders the book from display entries; recipe evaluation moved
    /// server-side.
    RecipeBookDisplays,
}

/// Which JSON shape a version's DataGenerator emits for block-state reports,
/// so the same loader can normalize old and new reports.
///
/// - [`BlockStateJsonFormat::NameString`]: entries are `"minecraft:stone": 0`
///   (older reports).
/// - [`BlockStateJsonFormat::StructuredEntry`]: entries carry explicit
///   `protocol_id` fields (1.21.2+ reports).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum BlockStateJsonFormat {
    NameString,
    StructuredEntry,
    /// 26.x reports address each block both by its `minecraft:name` string
    /// and by numeric id — a loader must accept either keying form.
    DualNameOrId,
}

/// Structured facts about one protocol version. Every flag corresponds to a
/// real divergence already visible between 1.21.1 and 1.21.4/5, so callers
/// switch on capability, never on raw numbers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VersionCapabilities {
    /// Numeric protocol (e.g. 767).
    pub protocol_version: i32,
    /// Canonical Minecraft release string (e.g. `"1.21.1"`).
    pub mc_version: &'static str,
    /// The `DataVersion` world-save number reported by DataGenerator packs.
    pub data_version: u32,
    /// The data-pack `pack_format` for this release — the major component of
    /// [`VersionCapabilities::data_pack_format_tuple`], kept for callers that
    /// predate tuple pack formats (1.21.x versions only ever had a bare major
    /// number, so this remains the whole value for them).
    pub pack_format: u32,
    /// The data-pack `pack_format` as vanilla's `PackType.SERVER_DATA` version
    /// reports it: `(major, minor)`. Pre-tuple releases carry minor 0, and
    /// `major` always equals [`VersionCapabilities::pack_format`].
    pub data_pack_format_tuple: (u32, u32),
    /// The resource-pack `pack_format` as `PackType.CLIENT_RESOURCES` reports
    /// it: `(major, minor)`. Distinct from the data-pack format since vanilla
    /// numbers the two pack types independently.
    pub resource_pack_format_tuple: (u32, u32),
    /// How the recipe book synchronizes.
    pub recipe_sync_mode: RecipeSyncMode,
    /// Shape of DataGenerator block-state reports.
    pub block_state_json_format: BlockStateJsonFormat,
    /// Whether light masks can occupy multiple bit-set words on the wire.
    /// Always true on the protocol level; 1.21.1 geometry only ever needs one
    /// word, so codecs may fast-path the single-word case. Kept as a flag so
    /// taller-world support flips behavior without a signature change.
    pub multi_word_light_mask: bool,
    /// Whether `light_update`/`level_chunk_with_light` masks serialize as a
    /// `java.util.BitSet` byte array (`VarInt` byte count + little-endian
    /// bytes — the 1.21.5+ `ByteBufCodecs.BIT_SET` form) instead of the
    /// 1.21.1 `VarInt` word count + big-endian `u64` words.
    pub light_mask_byte_array: bool,
    /// Whether `minecraft:client_information` carries `particle_status`
    /// (1.21.2+ splits it out of the old "particle/all/decreased/minimal" view
    /// distance encoding into a separate display field).
    pub split_particle_status: bool,
    /// Whether `teleportation` packets became id-based (`TeleportConfirm`
    /// replaced by `AcceptTeleportation` numbering unchanged, but movement
    /// packets gained a relative-flag bitmask in 1.21.2+).
    pub relative_movement_flags: bool,
    /// Whether item components include `consumable`/`use_remainder`
    /// (1.21.2+ replaced the old `food` payload with these types).
    pub consumable_component: bool,
    /// Whether serverbound `player_input` is the 1.21.2+ `Input` record — a
    /// single byte of direction/jump/shift/sprint flags — instead of the
    /// 1.21.1 `f32 sideways, f32 forward, u8 jump/unmount flags` form.
    pub player_input_flags: bool,
}

impl VersionCapabilities {
    /// Copy these capabilities with a different numeric protocol — used when a
    /// `Custom` version inherits a baseline profile.
    pub fn with_protocol(&self, protocol_version: i32) -> Self {
        Self {
            protocol_version,
            mc_version: "custom",
            ..self.clone()
        }
    }
}

const CAP_1_21_1: VersionCapabilities = VersionCapabilities {
    protocol_version: ProtocolVersion::V1_21_1_NUMBER,
    mc_version: TargetVersion::V1_21_1.id(),
    data_version: TargetVersion::V1_21_1.data_version() as u32,
    pack_format: TargetVersion::V1_21_1.data_pack_format().0,
    data_pack_format_tuple: TargetVersion::V1_21_1.data_pack_format(),
    resource_pack_format_tuple: TargetVersion::V1_21_1.resource_pack_format(),
    recipe_sync_mode: RecipeSyncMode::FullOnJoin,
    block_state_json_format: BlockStateJsonFormat::NameString,
    multi_word_light_mask: true,
    light_mask_byte_array: false,
    split_particle_status: false,
    relative_movement_flags: false,
    consumable_component: false,
    player_input_flags: false,
};

const CAP_1_21_3: VersionCapabilities = VersionCapabilities {
    protocol_version: ProtocolVersion::V1_21_3_NUMBER,
    mc_version: "1.21.3",
    data_version: 4082,
    pack_format: 57,
    data_pack_format_tuple: (57, 0),
    resource_pack_format_tuple: (42, 0),
    recipe_sync_mode: RecipeSyncMode::FullOnJoin,
    block_state_json_format: BlockStateJsonFormat::StructuredEntry,
    multi_word_light_mask: true,
    light_mask_byte_array: false,
    split_particle_status: true,
    relative_movement_flags: true,
    consumable_component: true,
    player_input_flags: true,
};

const CAP_1_21_4: VersionCapabilities = VersionCapabilities {
    protocol_version: ProtocolVersion::V1_21_4_NUMBER,
    mc_version: "1.21.4",
    data_version: 4189,
    pack_format: 61,
    data_pack_format_tuple: (61, 0),
    resource_pack_format_tuple: (46, 0),
    recipe_sync_mode: RecipeSyncMode::FullOnJoin,
    block_state_json_format: BlockStateJsonFormat::StructuredEntry,
    multi_word_light_mask: true,
    light_mask_byte_array: false,
    split_particle_status: true,
    relative_movement_flags: true,
    consumable_component: true,
    player_input_flags: true,
};

const CAP_1_21_5: VersionCapabilities = VersionCapabilities {
    protocol_version: ProtocolVersion::V1_21_5_NUMBER,
    mc_version: "1.21.5",
    data_version: 4325,
    pack_format: 71,
    data_pack_format_tuple: (71, 0),
    resource_pack_format_tuple: (55, 0),
    recipe_sync_mode: RecipeSyncMode::FullOnJoin,
    block_state_json_format: BlockStateJsonFormat::StructuredEntry,
    multi_word_light_mask: true,
    light_mask_byte_array: true,
    split_particle_status: true,
    relative_movement_flags: true,
    consumable_component: true,
    player_input_flags: true,
};

/// 26.3 (protocol 777, `DataVersion` 5023): the latest vanilla release in the
/// target set. Pack formats went `(major, minor)` upstream — data (121, 0),
/// resources (97, 1) — and the recipe book moved to recipe-display sync
/// (`recipe_book_add`/`recipe_book_remove`/`recipe_book_settings`).
const CAP_26_3: VersionCapabilities = VersionCapabilities {
    protocol_version: ProtocolVersion::V26_3_NUMBER,
    mc_version: TargetVersion::V26_3.id(),
    data_version: TargetVersion::V26_3.data_version() as u32,
    pack_format: TargetVersion::V26_3.data_pack_format().0,
    data_pack_format_tuple: TargetVersion::V26_3.data_pack_format(),
    resource_pack_format_tuple: TargetVersion::V26_3.resource_pack_format(),
    recipe_sync_mode: RecipeSyncMode::RecipeBookDisplays,
    block_state_json_format: BlockStateJsonFormat::DualNameOrId,
    multi_word_light_mask: true,
    light_mask_byte_array: true,
    split_particle_status: true,
    relative_movement_flags: true,
    consumable_component: true,
    player_input_flags: true,
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protocol_numbers_round_trip() {
        assert_eq!(ProtocolVersion::from_number(767), ProtocolVersion::V1_21_1);
        assert_eq!(ProtocolVersion::from_number(769), ProtocolVersion::V1_21_4);
        assert_eq!(ProtocolVersion::from_number(777), ProtocolVersion::V26_3);
        assert_eq!(ProtocolVersion::from_number(999).number(), 999);
        assert_eq!(ProtocolVersion::V1_21_4.number(), 769);
        assert_eq!(ProtocolVersion::V26_3.number(), 777);
        assert_eq!(ProtocolVersion::V1_21_1.mc_version(), Some("1.21.1"));
        assert_eq!(ProtocolVersion::V26_3.mc_version(), Some("26.3"));
        assert_eq!(ProtocolVersion::Custom(5).mc_version(), None);
        assert_eq!(ProtocolVersion::MIN_SUPPORTED, 767);
        assert_eq!(ProtocolVersion::MAX_KNOWN, 777);
    }

    #[test]
    fn capabilities_distinguish_versions() {
        let a = ProtocolVersion::V1_21_1.capabilities();
        let b = ProtocolVersion::V1_21_4.capabilities();
        assert_eq!(a.block_state_json_format, BlockStateJsonFormat::NameString);
        assert_eq!(
            b.block_state_json_format,
            BlockStateJsonFormat::StructuredEntry
        );
        assert!(!a.consumable_component && b.consumable_component);
        assert_eq!(a.mc_version, "1.21.1");
        assert_eq!(b.pack_format, 61);
        // Custom versions inherit a conservative baseline but keep their number.
        let custom = ProtocolVersion::Custom(800).capabilities();
        assert_eq!(custom.protocol_version, 800);
        assert_eq!(custom.mc_version, "custom");
    }

    #[test]
    fn capabilities_26_3_exact_constants() {
        let c = ProtocolVersion::V26_3.capabilities();
        assert_eq!(c.protocol_version, 777);
        assert_eq!(c.mc_version, "26.3");
        assert_eq!(c.data_version, 5023);
        assert_eq!(c.pack_format, 121);
        assert_eq!(c.data_pack_format_tuple, (121, 0));
        assert_eq!(c.resource_pack_format_tuple, (97, 1));
        assert_eq!(c.recipe_sync_mode, RecipeSyncMode::RecipeBookDisplays);
        assert_eq!(
            c.block_state_json_format,
            BlockStateJsonFormat::DualNameOrId
        );
        assert!(c.multi_word_light_mask);
        assert!(c.player_input_flags);
    }

    #[test]
    fn pack_format_tuples_track_the_scalar_field() {
        for v in [
            ProtocolVersion::V1_21_1,
            ProtocolVersion::V1_21_3,
            ProtocolVersion::V1_21_4,
            ProtocolVersion::V1_21_5,
            ProtocolVersion::V26_3,
        ] {
            let c = v.capabilities();
            // The legacy scalar stays the data-pack major everywhere.
            assert_eq!(c.pack_format, c.data_pack_format_tuple.0);
        }
        let c = ProtocolVersion::V1_21_1.capabilities();
        assert_eq!(c.data_pack_format_tuple, (48, 0));
        assert_eq!(c.resource_pack_format_tuple, (34, 0));
    }

    #[test]
    fn primary_target_capabilities_come_from_mc_spine_core() {
        // Every field `mc-spine-core` pins must equal `TargetVersion::meta()`
        // exactly — this is what makes the core crate the single source of
        // truth for the two-target constants.
        for (proto, target) in [
            (ProtocolVersion::V1_21_1, TargetVersion::V1_21_1),
            (ProtocolVersion::V26_3, TargetVersion::V26_3),
        ] {
            let meta = target.meta();
            let c = proto.capabilities();
            assert_eq!(c.protocol_version, meta.protocol);
            assert_eq!(c.mc_version, meta.id);
            assert_eq!(proto.mc_version(), Some(meta.id));
            assert_eq!(proto.number(), meta.protocol);
            assert_eq!(c.data_version, meta.data_version as u32);
            assert_eq!(c.pack_format, meta.data_pack_format.0);
            assert_eq!(c.data_pack_format_tuple, meta.data_pack_format);
            assert_eq!(c.resource_pack_format_tuple, meta.resource_pack_format);
        }
    }

    #[test]
    fn target_version_protocol_version_roundtrip_is_total_over_all() {
        // `TargetVersion -> ProtocolVersion -> TargetVersion` must be the
        // identity for every entry in the canonical target set.
        for target in TargetVersion::ALL {
            let proto = ProtocolVersion::from(target);
            assert_eq!(proto.number(), target.protocol());
            assert_eq!(proto.mc_version(), Some(target.id()));
            assert_eq!(TargetVersion::try_from(proto), Ok(target));
        }
    }

    #[test]
    fn non_target_protocols_fail_try_from_cleanly() {
        for proto in [
            ProtocolVersion::V1_21_3,
            ProtocolVersion::V1_21_4,
            ProtocolVersion::V1_21_5,
            ProtocolVersion::Custom(766),
            ProtocolVersion::Custom(0),
        ] {
            assert_eq!(
                TargetVersion::try_from(proto),
                Err(NoTargetVersion(proto)),
                "{proto} must not map to a canonical target"
            );
        }
        // The error carries the offending version and formats usefully.
        let err = NoTargetVersion(ProtocolVersion::Custom(999));
        assert_eq!(err.0, ProtocolVersion::Custom(999));
        assert_eq!(
            err.to_string(),
            "protocol 999 is not a canonical mc-spine target version"
        );
    }
}
