// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! `mc-spine-proto`: the canonical, version-aware Minecraft Java protocol core.
//!
//! This crate is a standalone, zero-dependency library so the same protocol
//! knowledge can be shared by the mc-spine server (`src/play_protocol.rs`), the
//! mc-spine client (`client-app/src/protocol.rs`) and any future tooling, and so
//! new Minecraft versions slot in as *data* (a version pack) rather than as a
//! second copy of the codec.
//!
//! Layers, bottom to top:
//!
//! * [`wire`] — byte-level primitives: `VarInt`/`VarLong`, big-endian scalars,
//!   bounded strings, UUIDs, packed block positions, multi-word bit sets and
//!   packet frames (`VarInt(len) + VarInt(id) + payload`). Everything fails
//!   closed through [`wire::ProtoError`]; nothing in the crate panics on
//!   malformed, truncated or adversarial input.
//! * [`version`] — [`version::ProtocolVersion`] and the structured
//!   [`version::VersionCapabilities`] downstream code switches on instead of
//!   scattering version-string `if`s.
//! * [`table`] — [`table::ProtocolTable`], the data-driven bidirectional
//!   `resource_name <-> protocol_id` map for packet ids (per
//!   [`table::PacketPhase`]/[`table::PacketDir`]) and the core registries
//!   (`minecraft:block`, `minecraft:item`, `minecraft:entity_type`,
//!   `minecraft:menu`, `minecraft:data_component_type`), plus
//!   [`table::IdRemapper`] for O(1) id translation between two versions of one
//!   registry. [`table::ProtocolTable::v767_builtin`] ships the verified 1.21.1
//!   tables; [`table::ProtocolTable::from_version_pack_json`] loads the same
//!   shape from normalized DataGenerator reports at runtime.
//! * [`codec`] — the canonical packet IR ([`codec::S2CPacket`],
//!   [`codec::C2SPacket`]), the [`codec::ProtocolCodec`] trait and
//!   [`codec::CodecV767`], the protocol-767 implementation.

pub mod codec;
mod data_v767;
pub mod table;
pub mod version;
pub mod wire;

pub use codec::{
    C2SPacket, CodecV767, EquipmentSlot, ItemStack, LoginProperty, MenuKind,
    ProtocolCodec, RawComponent, RegistryDataEntry, S2CPacket, TeamInfo, TeamOperation,
};
pub use table::{IdRemapper, PacketDir, PacketPhase, ProtocolTable, Registry, RemapReport};
pub use version::{
    BlockStateJsonFormat, ProtocolVersion, RecipeSyncMode, VersionCapabilities,
};
pub use wire::{BitSet, BlockPos, ProtoError, WireReader, WireWriter};
