// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later

#![deny(unsafe_code, unsafe_op_in_unsafe_fn)]

pub mod contraption;
pub mod entity_spatial;
pub mod ffi;
pub mod fluid_network;
pub mod kinetic_graph;
pub mod me_network;
pub mod recipe_processor;
pub mod region_scheduler;
pub mod sleeping_transport;

pub use entity_spatial::{
    evaluate_mob_targets_uncached, merge_item_stacks, merge_xp_orbs, solve_collisions_spatial,
    tick_confined_ai, AiTickInput, AiTickResult, ChunkSensorBroadPhase, CollisionResult,
    DroppedItem, EntityAiState, EntityKind, MergeReport, MobSensor, SensorEntity, SoaEntityPool,
    XpOrb,
};
