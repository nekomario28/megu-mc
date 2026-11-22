// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! Canonical two-target version registry.
//!
//! megu simulates exactly two vanilla releases: `1.21.1` (protocol 767)
//! and the latest vanilla `26.3` (protocol 777). Downstream crates key on
//! [`TargetVersion`] instead of scattering protocol literals, and on
//! [`VersionMeta`] for the per-version constants (data version, pack formats,
//! required Java major) that a wire handshake alone does not carry.

use std::fmt;

/// One of the two vanilla releases megu targets.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum TargetVersion {
    /// Minecraft Java 1.21.1 — protocol 767.
    V1_21_1,
    /// Minecraft Java 26.3 — protocol 777, the latest vanilla release.
    V26_3,
}

/// The full constant set for one target version.
///
/// `data_pack_format`/`resource_pack_format` are `(major, minor)` pairs, the
/// shape vanilla's `pack.mcmeta` carries since the minor-component split.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct VersionMeta {
    /// Canonical release id (`"1.21.1"`, `"26.3"`).
    pub id: &'static str,
    /// Numeric protocol version sent in handshakes and `server_list_ping`.
    pub protocol: i32,
    /// `DataVersion` written into saved data (`level.dat`, chunk NBT).
    pub data_version: i32,
    /// `(major, minor)` datapack format this version reads and writes.
    pub data_pack_format: (u32, u32),
    /// `(major, minor)` resource-pack format this version reads and writes.
    pub resource_pack_format: (u32, u32),
    /// Minimum Java major version the matching vanilla build requires.
    pub java_major: u32,
}

/// The pinned constants for 1.21.1.
pub const META_V1_21_1: VersionMeta = VersionMeta {
    id: "1.21.1",
    protocol: 767,
    data_version: 3955,
    data_pack_format: (48, 0),
    resource_pack_format: (34, 0),
    java_major: 21,
};

/// The pinned constants for 26.3.
pub const META_V26_3: VersionMeta = VersionMeta {
    id: "26.3",
    protocol: 777,
    data_version: 5023,
    data_pack_format: (121, 0),
    resource_pack_format: (97, 1),
    java_major: 25,
};

impl TargetVersion {
    /// Every target, oldest first. Exhaustive by construction.
    pub const ALL: [TargetVersion; 2] = [TargetVersion::V1_21_1, TargetVersion::V26_3];

    /// The newest vanilla release in the target set.
    pub const LATEST: TargetVersion = TargetVersion::V26_3;

    /// `id -> TargetVersion`. Returns `None` for anything outside the target
    /// set — a version string must never silently fall back to a default.
    pub fn parse(id: &str) -> Option<Self> {
        Self::ALL
            .iter()
            .copied()
            .find(|v| v.meta().id == id)
    }

    /// `protocol -> TargetVersion` for the wire handshake number.
    pub fn from_protocol(protocol: i32) -> Option<Self> {
        Self::ALL
            .iter()
            .copied()
            .find(|v| v.meta().protocol == protocol)
    }

    /// `DataVersion -> TargetVersion` for saved-world provenance checks.
    pub fn from_data_version(data_version: i32) -> Option<Self> {
        Self::ALL
            .iter()
            .copied()
            .find(|v| v.meta().data_version == data_version)
    }

    /// The pinned constant set for this target.
    pub const fn meta(self) -> &'static VersionMeta {
        match self {
            Self::V1_21_1 => &META_V1_21_1,
            Self::V26_3 => &META_V26_3,
        }
    }

    /// Canonical release id, e.g. `"1.21.1"`.
    pub const fn id(self) -> &'static str {
        self.meta().id
    }

    /// Numeric protocol version.
    pub const fn protocol(self) -> i32 {
        self.meta().protocol
    }

    /// `DataVersion` for saved data.
    pub const fn data_version(self) -> i32 {
        self.meta().data_version
    }

    /// `(major, minor)` datapack format.
    pub const fn data_pack_format(self) -> (u32, u32) {
        self.meta().data_pack_format
    }

    /// `(major, minor)` resource-pack format.
    pub const fn resource_pack_format(self) -> (u32, u32) {
        self.meta().resource_pack_format
    }

    /// Minimum Java major version of the matching vanilla build.
    pub const fn java_major(self) -> u32 {
        self.meta().java_major
    }
}

impl fmt::Display for TargetVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.id())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_two_targets_carry_their_pinned_constants() {
        let m = TargetVersion::V1_21_1.meta();
        assert_eq!(m.id, "1.21.1");
        assert_eq!(m.protocol, 767);
        assert_eq!(m.data_version, 3955);
        assert_eq!(m.data_pack_format, (48, 0));
        assert_eq!(m.resource_pack_format, (34, 0));
        assert_eq!(m.java_major, 21);

        let m = TargetVersion::V26_3.meta();
        assert_eq!(m.id, "26.3");
        assert_eq!(m.protocol, 777);
        assert_eq!(m.data_version, 5023);
        assert_eq!(m.data_pack_format, (121, 0));
        assert_eq!(m.resource_pack_format, (97, 1));
        assert_eq!(m.java_major, 25);
    }

    #[test]
    fn parse_accepts_exactly_the_two_release_ids() {
        assert_eq!(TargetVersion::parse("1.21.1"), Some(TargetVersion::V1_21_1));
        assert_eq!(TargetVersion::parse("26.3"), Some(TargetVersion::V26_3));
        for bad in [
            "", "1.21", "1.21.2", "1.21.1 ", " 1.21.1", "26.3.1", "26", "26.4", "v1.21.1",
            "1_21_1",
        ] {
            assert_eq!(TargetVersion::parse(bad), None, "must not parse {bad:?}");
        }
    }

    #[test]
    fn the_numeric_lookups_are_reverse_maps_of_meta() {
        for v in TargetVersion::ALL {
            assert_eq!(TargetVersion::from_protocol(v.protocol()), Some(v));
            assert_eq!(TargetVersion::from_data_version(v.data_version()), Some(v));
            assert_eq!(TargetVersion::parse(v.id()), Some(v));
        }
        for unknown in [0, 766, 768, 776, 778, i32::MAX, i32::MIN] {
            assert_eq!(TargetVersion::from_protocol(unknown), None);
        }
        for unknown in [0, 3954, 3956, 5022, 5024] {
            assert_eq!(TargetVersion::from_data_version(unknown), None);
        }
    }

    #[test]
    fn every_lookup_key_is_unique_across_the_target_set() {
        // A duplicate id/protocol/data_version would make the reverse lookups
        // order-dependent; fail loudly here instead of misidentifying a save.
        for (i, a) in TargetVersion::ALL.iter().enumerate() {
            for b in &TargetVersion::ALL[i + 1..] {
                assert_ne!(a.id(), b.id());
                assert_ne!(a.protocol(), b.protocol());
                assert_ne!(a.data_version(), b.data_version());
            }
        }
    }

    #[test]
    fn latest_is_the_newest_target() {
        assert_eq!(TargetVersion::LATEST, TargetVersion::V26_3);
        assert!(TargetVersion::ALL.contains(&TargetVersion::LATEST));
        assert_eq!(TargetVersion::ALL.len(), 2);
    }

    #[test]
    fn display_is_the_release_id() {
        assert_eq!(TargetVersion::V1_21_1.to_string(), "1.21.1");
        assert_eq!(TargetVersion::V26_3.to_string(), "26.3");
        assert_eq!(format!("{}", TargetVersion::LATEST), "26.3");
    }
}
