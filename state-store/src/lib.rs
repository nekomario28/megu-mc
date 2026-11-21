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
//!   `pack_format` -> `min_format`/`max_format` migration, a data-driven
//!   resource-location rename table that leaves unknown/modded namespaces
//!   untouched, and `DataVersion` stamping that is idempotent and fails closed
//!   on documents newer than the target.

pub mod datafix;
pub mod sharded;

pub use datafix::{DataFixError, DataFixReport, DataFixStep, DataFixer, DataValue, DV_1_21_1, DV_26_3};
pub use sharded::{
    chunk_shard, BlockCoord, ChunkCoord, ChunkOverlay, DimensionState, EntityRecord, PendingTick,
    ServerContext, TickKind, SHARD_COUNT,
};
