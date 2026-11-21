// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! Protocol-version identity and structured capability flags.
//!
//! Downstream code branches on [`VersionCapabilities`] instead of scattering
//! `protocol == 767`-style checks (the pattern `src/play_protocol.rs` already
//! uses for its `protocol_1_21_3`/`protocol_1_21_4` forks).

use std::fmt;

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
    /// Any other (snapshot, newer, or older) numeric protocol.
    Custom(i32),
}

impl ProtocolVersion {
    pub const V1_21_1_NUMBER: i32 = 767;
    pub const V1_21_3_NUMBER: i32 = 768;
    pub const V1_21_4_NUMBER: i32 = 769;
    pub const V1_21_5_NUMBER: i32 = 770;

    /// Map a wire/handshake number to the enum, keeping unknown numbers.
    pub fn from_number(number: i32) -> Self {
        match number {
            Self::V1_21_1_NUMBER => Self::V1_21_1,
            Self::V1_21_3_NUMBER => Self::V1_21_3,
            Self::V1_21_4_NUMBER => Self::V1_21_4,
            Self::V1_21_5_NUMBER => Self::V1_21_5,
            other => Self::Custom(other),
        }
    }

    /// The numeric protocol version used in handshakes and the `server_list_ping`.
    pub fn number(self) -> i32 {
        match self {
            Self::V1_21_1 => Self::V1_21_1_NUMBER,
            Self::V1_21_3 => Self::V1_21_3_NUMBER,
            Self::V1_21_4 => Self::V1_21_4_NUMBER,
            Self::V1_21_5 => Self::V1_21_5_NUMBER,
            Self::Custom(n) => n,
        }
    }

    /// The canonical Minecraft release name, when this is a known release.
    pub fn mc_version(self) -> Option<&'static str> {
        match self {
            Self::V1_21_1 => Some("1.21.1"),
            Self::V1_21_3 => Some("1.21.3"),
            Self::V1_21_4 => Some("1.21.4"),
            Self::V1_21_5 => Some("1.21.5"),
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
            // Unknown versions get the oldest known profile as a conservative
            // baseline; callers that need exact behaviour must check the
            // concrete `protocol_version` field anyway.
            Self::Custom(_) => CAP_1_21_1.with_protocol(self.number()),
        }
    }

    /// The lowest protocol version the codebase can negotiate.
    pub const MIN_SUPPORTED: i32 = Self::V1_21_1_NUMBER;
    /// The highest protocol version this crate ships a table for.
    pub const MAX_KNOWN: i32 = Self::V1_21_5_NUMBER;
}

impl fmt::Display for ProtocolVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.mc_version() {
            Some(name) => write!(f, "{name} (protocol {})", self.number()),
            None => write!(f, "protocol {}", self.number()),
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
    /// The data-pack `pack_format` for this release.
    pub pack_format: u32,
    /// How the recipe book synchronizes.
    pub recipe_sync_mode: RecipeSyncMode,
    /// Shape of DataGenerator block-state reports.
    pub block_state_json_format: BlockStateJsonFormat,
    /// Whether light masks can occupy multiple bit-set words on the wire.
    /// Always true on the protocol level; 1.21.1 geometry only ever needs one
    /// word, so codecs may fast-path the single-word case. Kept as a flag so
    /// taller-world support flips behavior without a signature change.
    pub multi_word_light_mask: bool,
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
    mc_version: "1.21.1",
    data_version: 3955,
    pack_format: 48,
    recipe_sync_mode: RecipeSyncMode::FullOnJoin,
    block_state_json_format: BlockStateJsonFormat::NameString,
    multi_word_light_mask: true,
    split_particle_status: false,
    relative_movement_flags: false,
    consumable_component: false,
};

const CAP_1_21_3: VersionCapabilities = VersionCapabilities {
    protocol_version: ProtocolVersion::V1_21_3_NUMBER,
    mc_version: "1.21.3",
    data_version: 4082,
    pack_format: 57,
    recipe_sync_mode: RecipeSyncMode::FullOnJoin,
    block_state_json_format: BlockStateJsonFormat::StructuredEntry,
    multi_word_light_mask: true,
    split_particle_status: true,
    relative_movement_flags: true,
    consumable_component: true,
};

const CAP_1_21_4: VersionCapabilities = VersionCapabilities {
    protocol_version: ProtocolVersion::V1_21_4_NUMBER,
    mc_version: "1.21.4",
    data_version: 4189,
    pack_format: 61,
    recipe_sync_mode: RecipeSyncMode::FullOnJoin,
    block_state_json_format: BlockStateJsonFormat::StructuredEntry,
    multi_word_light_mask: true,
    split_particle_status: true,
    relative_movement_flags: true,
    consumable_component: true,
};

const CAP_1_21_5: VersionCapabilities = VersionCapabilities {
    protocol_version: ProtocolVersion::V1_21_5_NUMBER,
    mc_version: "1.21.5",
    data_version: 4325,
    pack_format: 71,
    recipe_sync_mode: RecipeSyncMode::FullOnJoin,
    block_state_json_format: BlockStateJsonFormat::StructuredEntry,
    multi_word_light_mask: true,
    split_particle_status: true,
    relative_movement_flags: true,
    consumable_component: true,
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protocol_numbers_round_trip() {
        assert_eq!(ProtocolVersion::from_number(767), ProtocolVersion::V1_21_1);
        assert_eq!(ProtocolVersion::from_number(769), ProtocolVersion::V1_21_4);
        assert_eq!(ProtocolVersion::from_number(999).number(), 999);
        assert_eq!(ProtocolVersion::V1_21_4.number(), 769);
        assert_eq!(ProtocolVersion::V1_21_1.mc_version(), Some("1.21.1"));
        assert_eq!(ProtocolVersion::Custom(5).mc_version(), None);
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
}
