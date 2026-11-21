// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! `mc-spine-state-store`: owned, instance-scoped runtime state for the server.
//!
//! This crate is a standalone, zero-dependency library implementing the two
//! architectural pillars of `specs/multi-version-architecture.md` §4 and the
//! version-bridge work: a chunk-sharded [`sharded::DimensionState`] owned by an
//! instance-scoped [`sharded::ServerContext`] (no process-global `OnceLock`
//! statics, so tests and multi-world servers stay isolated), and a declarative
//! [`datafix::DataFixer`] pipeline that walks saved data from the 1.21.1
//! DataVersion (3955) to the 26.3 DataVersion (5023).
//!
//! Layers:
//!
//! * [`sharded`] — `SHARD_COUNT = 64` chunk shards behind `RwLock`s, the
//!   deadlock-free multi-chunk write helper
//!   [`sharded::DimensionState::with_chunks_write`] (shard locks are taken in
//!   strictly ascending shard-index order, per the spec's lock-order
//!   invariant), entity and scheduled-tick state, and [`sharded::ServerContext`]
//!   holding every dimension (Overworld/Nether/End plus dynamic mod dimensions
//!   and instanced 6-DOF sub-worlds) in one instance-owned map.
//! * [`datafix`] — the format-agnostic [`datafix::DataValue`] tree,
//!   [`datafix::DataFixStep`] units and
//!   [`datafix::DataFixer::v1_21_1_to_26_3`]: block-state key normalization,
//!   `level.dat` version metadata, chunk-section palette migration,
//!   item-component upgrades, `pack_format` -> `min_format`/`max_format`
//!   migration, a data-driven resource-location rename table that leaves
//!   unknown/modded namespaces untouched, and `DataVersion` stamping that is
//!   idempotent and fails closed on documents newer than the target. The
//!   explicit opt-in [`datafix::DataFixer::v26_3_to_1_21_1_downgrade`]
//!   mirrors it for reversible roundtrips.
//! * [`region_tick`] — 8x8-chunk [`region_tick::RegionCoord`] super-regions
//!   and the four-color [`region_tick::partition_independent_regions`]
//!   checkerboard waves behind
//!   [`region_tick::tick_dimension_regions_parallel`]: same-wave regions can
//!   never share a chunk border, so parallel ticking is bit-identical to
//!   serial ticking.
//! * [`snapshot`] — the `STV1` binary codec ([`snapshot::encode_snapshot`] /
//!   [`snapshot::decode_snapshot`]): a deterministic, CRC32-sealed snapshot of
//!   a [`sharded::ServerContext`]'s chunk overlays keyed on
//!   `mc_spine_core::version::TargetVersion`, failing closed on any
//!   corruption.
//! * [`migrate`] — the operator-facing migration engine
//!   ([`migrate::inspect_snapshot`], [`migrate::migrate_snapshot`],
//!   [`migrate::migrate_json_document`]): `STV1` snapshot inspection and
//!   atomic `1.21.1` <-> `26.3` re-sealing, plus a minimal pure-`std` JSON
//!   front end that runs `level.dat`-style documents through the
//!   [`datafix::DataFixer`]. The `26.3` -> `1.21.1` direction exists only
//!   behind an explicit `allow_downgrade` opt-in.

pub mod datafix;
pub mod migrate;
pub mod region_tick;
pub mod sharded;
pub mod snapshot;

pub use datafix::{DataFixError, DataFixReport, DataFixStep, DataFixer, DataValue, DV_1_21_1, DV_26_3};
pub use migrate::{
    inspect_snapshot, migrate_json_document, migrate_snapshot, parse_data_json, to_data_json,
    write_file_atomic, JsonError, MigrationError, SnapshotMigrationReport, SnapshotSummary,
};
pub use region_tick::{
    partition_independent_regions, tick_dimension_regions_parallel, RegionCoord, RegionTickStats,
    RegionWork, REGION_SIDE_CHUNKS, WAVE_COUNT,
};
pub use sharded::{
    chunk_shard, BlockCoord, ChunkCoord, ChunkOverlay, DimensionState, EntityRecord, PendingTick,
    ServerContext, TickKind, SHARD_COUNT,
};
pub use snapshot::{decode_snapshot, encode_snapshot, SnapshotError};
