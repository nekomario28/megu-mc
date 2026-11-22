// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later

#![deny(unsafe_code, unsafe_op_in_unsafe_fn)]

pub mod combat_motion;
pub mod contraption;
pub mod entity_spatial;
pub mod explosion_batch;
pub mod ffi;
pub mod fluid_network;
pub mod kinetic_graph;
pub mod me_network;
pub mod party_storage;
pub mod recipe_processor;
pub mod region_scheduler;
pub mod sleeping_transport;

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
    MAX_BLAST_POWER, MAX_CACHE_CELLS, MAX_RAY_STEPS, MAX_STACK_SIZE, RAY_GRID, RAY_POWER_MIN,
    RAY_POWER_RANGE, RAY_STEP, RAY_SURFACE_CELLS, STEP_DRAIN, TNT_POWER,
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
