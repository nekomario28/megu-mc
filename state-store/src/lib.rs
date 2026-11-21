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
//! * [`anvil_inspect`] — the pure-`std` vanilla Anvil inspector: an RFC
//!   1952/1951 gzip reader, a strict binary NBT <-> [`datafix::DataValue`]
//!   bridge (depth cap 64, size cap 32 MiB), `r.*.*.mca` region-header
//!   scanning, and [`anvil_inspect::inspect_anvil_world_dir`] which reads
//!   `level.dat` (gzip or raw), classifies the save's `DataVersion` against
//!   the two pinned targets, and counts populated chunks across
//!   `region/`, `DIM-1/region/` and `DIM1/region/` — plus
//!   [`anvil_inspect::migrate_anvil_level_dat`], which runs a decoded
//!   `level.dat` through the fixer and re-seals it as a verified gzip
//!   member.
//! * [`audit_log`] — the CoreProtect-style `AUD1` append-only audit WAL:
//!   CRC32-framed [`audit_log::AuditRecord`]s whose torn trailing write is
//!   dropped cleanly on [`audit_log::AuditLog::recover_from_file`], a
//!   `(dimension, cx, cz)` chunk-bucket index for spatial
//!   [`audit_log::AuditQuery`]s, and atomic
//!   [`audit_log::AuditLog::rollback_blocks`] /
//!   [`audit_log::AuditLog::restore_blocks`] that replay block mutations
//!   against the live [`sharded::ServerContext`] in strict `seq` order.
//! * [`pregen`] — the Chunky-style [`pregen::PregenJob`]: a deterministic
//!   outward spiral over a square or circular radius, handed out in
//!   per-tick-capped batches with pause/resume/cancel — and the
//!   [`pregen::ChunkTicketLruUnloader`], which pins resident chunks through
//!   reference-counted [`pregen::TicketKind`]s and evicts unticketed ones in
//!   strict LRU order, flushing their overlays back to the caller.
//! * [`incremental_save`] — the autosave anti-stall machinery: a deterministic
//!   FIFO/oldest-dirty [`incremental_save::IncrementalSaveQueue`] drained
//!   under a per-tick [`incremental_save::IncrementalSaveBudget`]
//!   (`max_items_per_tick`, `max_bytes_per_tick`) with a guaranteed
//!   `flush_all` for `/save-all` and shutdown, plus the
//!   [`incremental_save::PersistedFingerprints`] content-hash cache that lets
//!   an unchanged payload skip its disk write entirely.

pub mod anvil_inspect;
pub mod audit_log;
pub mod datafix;
pub mod incremental_save;
pub mod migrate;
pub mod pregen;
pub mod region_tick;
pub mod sharded;
pub mod snapshot;

pub use anvil_inspect::{
    decode_nbt_to_data_value, encode_data_value_to_nbt, gzip_decompress, gzip_encode_stored,
    inspect_anvil_world_dir, inspect_level_dat, inspect_mca_header, migrate_anvil_level_dat,
    AnvilCompatibility, AnvilDimensionReport, AnvilInspectError, AnvilRegionFileReport,
    AnvilWorldReport, LevelDatCompression, LevelDatSummary, McaChunkInfo, McaCompression,
    McaRegionSummary, MCA_HEADER_BYTES, MCA_SECTOR_BYTES, MCA_SLOT_COUNT, NBT_DEPTH_CAP,
    NBT_SIZE_CAP,
};
pub use audit_log::{
    AuditAction, AuditLog, AuditQuery, AuditRecord, BlockRollbackDelta,
};
pub use datafix::{DataFixError, DataFixReport, DataFixStep, DataFixer, DataValue, DV_1_21_1, DV_26_3};
pub use incremental_save::{
    content_fingerprint, DrainStats, FlushOutcome, IncrementalSaveBudget, IncrementalSaveQueue,
    PersistedFingerprints, QueuedSave,
};
pub use migrate::{
    cleanup_stale_migrate_temps, inspect_snapshot, migrate_json_document, migrate_snapshot,
    parse_data_json, to_data_json, write_file_atomic, JsonError, MigrationError,
    SnapshotMigrationReport, SnapshotSummary,
};
pub use pregen::{
    spiral_order, ChunkTicketLruUnloader, FlushedChunk, PregenJob, PregenProgress, PregenShape,
    PregenState, TicketKind,
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
