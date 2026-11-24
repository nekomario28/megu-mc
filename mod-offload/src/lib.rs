// SPDX-License-Identifier: LGPL-3.0-or-later

#![deny(unsafe_code, unsafe_op_in_unsafe_fn)]

pub mod auto_optimizer;
pub mod combat_motion;
pub mod contraption;
pub mod entity_spatial;
pub mod explosion_batch;
pub mod ffi;
pub mod fluid_network;
pub mod kinetic_graph;
pub mod lazy_catchup;
pub mod me_network;
pub mod party_storage;
pub mod recipe_processor;
pub mod region_scheduler;
pub mod sleeping_transport;
pub mod sync_bridge;

pub use auto_optimizer::{
    fingerprint_128, fingerprint_item_slots, pack_machine_key, AdaptivePollBackoff,
    AutoBulkSectionBatcher, AutoDeltaCoalescer, AutoRecipeMemoizer, BackoffStats,
    BulkBatchStats, BulkCellWrite, BulkSectionBatch, CoalesceStats, DistanceTier,
    EmitDecision, MemoStats, SectionKey, BACKOFF_LADDER, DEFAULT_MEMO_CAPACITY,
    MAX_COALESCE_SOURCES, MAX_TRACKED_MACHINES, SECTION_VOLUME, UNCHANGED_TICKS_PER_STEP,
};
pub use entity_spatial::{
    evaluate_mob_targets_uncached, merge_item_stacks, merge_xp_orbs, solve_collisions_spatial,
    tick_confined_ai, AiTickInput, AiTickResult, ChunkSensorBroadPhase, CollisionResult,
    DroppedItem, EntityAiState, EntityKind, MergeReport, MobSensor, SensorEntity, SoaEntityPool,
    XpOrb,
};
pub use explosion_batch::{
    blast_bounds, coalesce_item_counts, scrub_power, BlastBlockSource, BlastCell, BlastResult,
    BlastStats, BlastTickSlice, BlastVolumeCache, BlastWorld, BlastWorldStats, BlockRemoval,
    ExplosionBatch, ExplosionError, ItemStack, MegaBlastPlan, SectionBlockUpdate,
    SectionBlocksUpdate, AffectedCell, XorShift64,
    BLAST_BOUNDS_MARGIN, BLAST_REACH_FACTOR, BLOCK_COST_BASE, FLAG_INDESTRUCTIBLE,
    MAX_BLAST_POWER, MAX_CACHE_CELLS, MAX_RAY_STEPS, MAX_STACK_SIZE, MEGA_BLAST_MAX_PER_TICK,
    RAY_GRID, RAY_POWER_MIN, RAY_POWER_RANGE, RAY_STEP, RAY_SURFACE_CELLS, STEP_DRAIN, TNT_POWER,
};
pub use sync_bridge::{
    BridgeError, BridgeFailure, BridgeSyncReport, ExternalGraphDelta, GraphDeltaSink,
    MeGraphTarget, OffloadSyncBridge, MAX_INGEST_DELTAS,
};
pub use party_storage::{
    award_experience, decode_storage, eligible_evolutions, encode_storage,
    evaluate_capture_attempt, experience_for_level, level_for_experience, CaptureDeviceSpec,
    CaptureOutcome, CompanionRecord, DayPhase, EvolutionCandidate, EvolutionCondition,
    EvolutionContext, EvolutionRule, GrowthCurve, LevelUpReport, PlayerCompanionStorage,
    RecordError, StorageDecodeError, StorageError, StorageSlotRef, BOX_SLOTS, DEFAULT_BOX_COUNT,
    GUARANTEED_CAPTURE_MULTIPLIER, MAX_BOX_COUNT, MAX_LEVEL, MAX_MOVES, PARTY_SLOTS,
    PRTY_SCHEMA_VERSION, SHAKE_CHECKS,
};
pub use lazy_catchup::{
    catch_up_furnace, catch_up_growth_stage, catch_up_machine_batch, CatchUpLimits,
    FurnaceCatchUpReport, FurnaceCatchUpState, HeadlessError, HeadlessFactoryNode,
    HeadlessFactoryRegistry, HeadlessStepReport, MachineCatchUpReport, MachineCatchUpState,
    DEFAULT_MAX_DELTA_TICKS, HARD_MAX_DELTA_TICKS, MAX_HEADLESS_NODES, MILLI_PER_ITEM,
    TICKS_PER_HOUR,
};
