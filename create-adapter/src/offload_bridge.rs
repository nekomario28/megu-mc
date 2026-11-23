// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later

//! Deterministic parallel contraption & factory offload bridge.
//!
//! Wires [`mod_offload`]'s region-isolated offload kernels into the Create
//! adapter surface: [`CreateFactory`] keeps owning kinetic graphs, belts,
//! recipes and fluids through its [`RegionScheduler`], while assembled
//! [`LiveContraption`]s are registered here and ticked in region-disjoint
//! batches planned by [`ContraptionOffloadPlan`].
//!
//! Determinism contract:
//! - Every assembly belongs to exactly one [`RegionBatch`], keyed by
//!   [`RegionKey::for_block`] of its `(dim, anchor)` home cell. Batches are
//!   sorted by region key and members by assembly id — no `HashMap`
//!   iteration order ever reaches the result.
//! - The work-unit budget scan is single-threaded over that canonical order,
//!   so the set of assemblies approved for a tick is identical for any
//!   worker count. Workers only decide *which thread* steps an approved
//!   assembly — never *whether* it steps — making `workers = 1` and
//!   `workers = N` runs bit-identical, mirroring
//!   [`RegionScheduler::advance_all`](mod_offload::region_scheduler::RegionScheduler::advance_all).
//! - [`OffloadBridge::state_digest`] folds every assembly's pose, bounds,
//!   stress status and the factory's exported frames into one FNV-1a
//!   fingerprint ([`FingerprintBuilder`]) in canonical order, so partition
//!   shape cannot leak into the digest.
//!
//! Fail-closed caps:
//! - [`MAX_ASSEMBLIES`] live assemblies and [`MAX_BLOCKS_PER_ASSEMBLY`]
//!   blocks per assembly are hard registration limits.
//! - [`AssemblyStress`] compares capacity vs consumption in the same common
//!   stress units
//!   [`KineticNode`](mod_offload::kinetic_graph::KineticNode) uses:
//!   `consumption > capacity` freezes the contraption (overstressed
//!   fail-closed stop) until the spec changes; a driver error latches
//!   [`AssemblyStatus::Halted`] until [`OffloadBridge::resume_assembly`].
//! - [`OffloadBudget::max_work_units`] bounds block-visits per tick. Deferred
//!   assemblies keep their state and are retried on later ticks — nothing is
//!   dropped — while an assembly whose cost alone exceeds the budget is
//!   reported [`AssemblyStatus::OverBudget`] instead of half-stepping.

use std::collections::{BTreeMap, BTreeSet};
use std::thread;

use megu_core::perf_kit::FingerprintBuilder;
use mod_offload::region_scheduler::{Region, RegionKey};
use mod_offload::sleeping_transport::TransportError;
use mod_offload::sync_bridge::{GraphDeltaSink, MeGraphTarget, OffloadSyncBridge};

// The sync-bridge public surface re-exported so callers that only depend
// on `create-adapter` (e.g. the root live bridge under `create-offload`,
// which has no `mod-offload` dep of its own) can name the delta/report
// types without a second edge in the workspace graph.
pub use mod_offload::sync_bridge::{BridgeError, BridgeSyncReport, ExternalGraphDelta};

// The combat-motion (MOT1) and companion party-storage (PRTY) surfaces,
// re-exported for the same single-edge reason as the sync bridge above:
// the server wire layer names the codecs and timeline types through the
// bridge that owns their live state.
pub use mod_offload::combat_motion::{
    decode_motion_trigger, encode_motion_trigger, validate_hit, ActionPhase, ActionSpec,
    ActiveAction, ComboChain, ComboState, Hand, HitCandidate, HitShape, HitValidationVerdict,
    MotionError, MotionTrigger, TickWindow, Vec3 as MotionVec3, MOT1_MAGIC, MOT1_PACKET_LEN,
    MOTION_CHANNEL,
};
pub use mod_offload::party_storage::{
    decode_storage, encode_storage, CompanionRecord, PlayerCompanionStorage, StorageDecodeError,
    StorageError, StorageSlotRef, BOX_SLOTS, DEFAULT_BOX_COUNT, MAX_BOX_COUNT, PARTY_SLOTS,
    PRTY_SCHEMA_VERSION,
};

use crate::aeronautics::RigidBodySubWorld;
use crate::contraption_adapter::{ContraptionError, ContraptionKind, LiveContraption};
use crate::factory::CreateFactory;
use crate::orbital::{
    OrbitalDimension, OrbitalTransitionRouter, OrbitalTransitionRule, ShipDimensionTransition,
};

/// Maximum simultaneously registered contraption assemblies.
pub const MAX_ASSEMBLIES: usize = 1024;
/// Maximum blocks one assembly may carry into the offload bridge.
pub const MAX_BLOCKS_PER_ASSEMBLY: usize = 4096;
/// Maximum simultaneously registered orbital/aeronautics 6-DOF ships.
pub const MAX_ORBITAL_SHIPS: usize = 256;
/// Maximum entities with a tracked combat-motion timeline.
pub const MAX_COMBAT_MOTIONS: usize = 4096;
/// Maximum companion/party storages attached to the bridge.
pub const MAX_COMPANION_STORAGES: usize = 4096;
/// Fixed orbital-ship integration step: one server tick in seconds.
const ORBITAL_SHIP_STEP_DT: f64 = 0.05;
/// Default per-region factory work budget, matching the main-crate bridge.
const DEFAULT_FACTORY_REGION_STEPS: usize = 1_024;
/// Default per-region factory time budget in microseconds.
const DEFAULT_FACTORY_REGION_MICROS: u64 = 500;

/// Stress capacity vs consumption for one assembly's motion drive, in the
/// same common stress units
/// [`KineticNode::capacity`](mod_offload::kinetic_graph::KineticNode::capacity)/
/// [`KineticNode::load`](mod_offload::kinetic_graph::KineticNode::load) use —
/// the adapter normalizes speed-dependent rules before supplying them.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AssemblyStress {
    /// Stress units the drive can supply per tick.
    pub capacity: f64,
    /// Stress units the assembly consumes per tick.
    pub consumption: f64,
}

impl AssemblyStress {
    /// Fail-closed constructor: both fields must be finite and non-negative.
    pub fn new(capacity: f64, consumption: f64) -> Result<Self, OffloadError> {
        Self::validated(capacity, consumption)
    }

    fn validated(capacity: f64, consumption: f64) -> Result<Self, OffloadError> {
        if !capacity.is_finite()
            || capacity < 0.0
            || !consumption.is_finite()
            || consumption < 0.0
        {
            return Err(OffloadError::InvalidStress);
        }
        Ok(Self {
            capacity,
            consumption,
        })
    }

    /// `true` when consumption exceeds capacity — the fail-closed stop
    /// condition, identical to
    /// [`NetworkStatus::Overstressed`](mod_offload::kinetic_graph::NetworkStatus::Overstressed).
    pub fn overstressed(&self) -> bool {
        self.consumption > self.capacity
    }
}

/// Last-tick outcome of one registered assembly.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AssemblyStatus {
    /// Registered but never stepped yet.
    Idle,
    /// The tick stepped this assembly (pose may or may not have moved).
    Running,
    /// Eligible but the work-unit budget ran out before its turn; retried on
    /// later ticks, never dropped.
    Deferred,
    /// `consumption > capacity`: fail-closed freeze until the stress spec
    /// changes.
    Overstressed,
    /// `work_cost > max_work_units`: can never fit this tick's budget, so it
    /// is reported instead of half-stepping. Raise the budget to run it.
    OverBudget,
    /// Driver error latched; frozen until [`OffloadBridge::resume_assembly`].
    Halted,
}

/// One region-disjoint batch of the offload plan: all assemblies whose home
/// cell maps to `region`, members sorted by assembly id.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RegionBatch {
    pub region: RegionKey,
    pub members: Vec<u32>,
}

/// Deterministic partition of registered assemblies into region-disjoint
/// batches. Canonical order is batch order (sorted [`RegionKey`]) then
/// ascending assembly id — rebuilt wholesale on every register/unregister so
/// the plan can never drift from the assembly map.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ContraptionOffloadPlan {
    batches: Vec<RegionBatch>,
    /// Flat canonical work order: every batch's members concatenated.
    order: Vec<u32>,
    /// Assembly id -> index into `order`.
    position_of: BTreeMap<u32, usize>,
    /// Canonical position -> batch index.
    batch_of_pos: Vec<usize>,
}

impl ContraptionOffloadPlan {
    /// Build a plan from `(assembly_id, home_region, block_count)` entries in
    /// any order. Fails closed on duplicate ids, oversized assemblies, or an
    /// assembly count past [`MAX_ASSEMBLIES`].
    pub fn build(entries: &[(u32, RegionKey, usize)]) -> Result<Self, OffloadError> {
        if entries.len() > MAX_ASSEMBLIES {
            return Err(OffloadError::TooManyAssemblies {
                count: entries.len(),
                limit: MAX_ASSEMBLIES,
            });
        }
        let mut seen = BTreeSet::new();
        let mut by_region: BTreeMap<RegionKey, BTreeMap<u32, ()>> = BTreeMap::new();
        for &(id, region, blocks) in entries {
            if blocks > MAX_BLOCKS_PER_ASSEMBLY {
                return Err(OffloadError::AssemblyTooLarge {
                    id,
                    count: blocks,
                    limit: MAX_BLOCKS_PER_ASSEMBLY,
                });
            }
            if !seen.insert(id) {
                return Err(OffloadError::DuplicateAssembly(id));
            }
            by_region.entry(region).or_default().insert(id, ());
        }

        let mut batches = Vec::with_capacity(by_region.len());
        let mut order = Vec::with_capacity(entries.len());
        let mut position_of = BTreeMap::new();
        let mut batch_of_pos = Vec::with_capacity(entries.len());
        for (region, members) in by_region {
            let start = order.len();
            order.extend(members.into_keys());
            for (pos, id) in order.iter().enumerate().skip(start) {
                position_of.insert(*id, pos);
                batch_of_pos.push(batches.len());
            }
            batches.push(RegionBatch {
                region,
                members: order[start..].to_vec(),
            });
        }
        Ok(Self {
            batches,
            order,
            position_of,
            batch_of_pos,
        })
    }

    /// Region batches in sorted [`RegionKey`] order.
    pub fn batches(&self) -> &[RegionBatch] {
        &self.batches
    }

    /// Flat canonical work order across all batches.
    pub fn order(&self) -> &[u32] {
        &self.order
    }

    /// Canonical position of `id` in [`order`](Self::order).
    pub fn position_of(&self, id: u32) -> Option<usize> {
        self.position_of.get(&id).copied()
    }

    /// Batch index owning canonical position `pos`.
    pub fn batch_of_pos(&self, pos: usize) -> usize {
        self.batch_of_pos[pos]
    }

    pub fn len(&self) -> usize {
        self.order.len()
    }

    pub fn is_empty(&self) -> bool {
        self.order.is_empty()
    }
}

/// Per-tick budget for [`OffloadBridge::step_tick`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OffloadBudget {
    /// Total block-visit work units the contraption side may consume this
    /// tick. An assembly costs its block count (minimum 1), so a 50 ms tick
    /// budget translates directly into a hard cap on contraption motion work.
    pub max_work_units: usize,
    /// Per-region `max_steps` handed to the factory's [`RegionScheduler`].
    pub factory_region_steps: usize,
    /// Per-region `max_time` handed to the factory's [`RegionScheduler`].
    pub factory_region_micros: Option<u64>,
    /// Partition workers for contraption execution; `1` runs sequentially.
    /// Worker count never changes results — only wall-clock parallelism.
    pub workers: usize,
}

impl OffloadBudget {
    /// Contraption-only budget with default factory region limits and one
    /// sequential worker.
    pub const fn new(max_work_units: usize) -> Self {
        Self {
            max_work_units,
            factory_region_steps: DEFAULT_FACTORY_REGION_STEPS,
            factory_region_micros: Some(DEFAULT_FACTORY_REGION_MICROS),
            workers: 1,
        }
    }

    /// Same budget executed by up to `workers` partition workers.
    pub const fn with_workers(mut self, workers: usize) -> Self {
        self.workers = if workers == 0 { 1 } else { workers };
        self
    }
}

/// Errors from registration, planning and ticking. Every variant is
/// fail-closed: the offending assembly is rejected or frozen, never
/// half-applied.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OffloadError {
    /// Registration would exceed [`MAX_ASSEMBLIES`].
    TooManyAssemblies { count: usize, limit: usize },
    /// One assembly carries more than [`MAX_BLOCKS_PER_ASSEMBLY`] blocks.
    AssemblyTooLarge { id: u32, count: usize, limit: usize },
    /// Assembly id already registered (or duplicated in plan entries).
    DuplicateAssembly(u32),
    /// No assembly with this id is registered.
    MissingAssembly(u32),
    /// Stress spec was non-finite or negative.
    InvalidStress,
    /// `tick` did not strictly increase past the last stepped tick.
    TickWentBackwards { current: u64, requested: u64 },
    /// The factory's region scheduler rejected the tick.
    Factory(TransportError),
    /// Registration would exceed [`MAX_ORBITAL_SHIPS`].
    TooManyOrbitalShips { count: usize, limit: usize },
    /// Orbital ship id already registered.
    DuplicateOrbitalShip(u64),
    /// Registration would exceed [`MAX_COMBAT_MOTIONS`].
    TooManyCombatMotions { count: usize, limit: usize },
    /// A combat-motion timeline rejected the step or trigger (tick
    /// rewind, invalid spec/combo, action lock).
    CombatMotion(MotionError),
    /// Registration would exceed [`MAX_COMPANION_STORAGES`].
    TooManyCompanionStorages { count: usize, limit: usize },
    /// A `PRTY` blob failed fail-closed decoding; nothing was installed.
    CompanionStorageDecode(StorageDecodeError),
}

/// Per-batch outcome of one [`OffloadBridge::step_tick`], in plan order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BatchTickReport {
    pub region: RegionKey,
    /// Assemblies belonging to this batch.
    pub assemblies: usize,
    /// Approved by the work-unit budget scan this tick.
    pub scheduled: usize,
    /// Actually stepped (driver returned `Ok`).
    pub stepped: usize,
    /// Eligible but deferred by the work-unit budget.
    pub deferred: usize,
    /// Frozen by overstress, halt latch, or an over-budget cost.
    pub stopped: usize,
}

/// Deterministic summary of one [`OffloadBridge::step_tick`].
#[derive(Clone, Debug)]
pub struct OffloadTickReport {
    pub tick: u64,
    pub work_units_budget: usize,
    /// Work units actually consumed (`<= work_units_budget` always).
    pub work_units_used: usize,
    /// Assemblies stepped without driver error.
    pub stepped: usize,
    /// Eligible assemblies deferred by the budget (retried next tick).
    pub deferred: usize,
    /// Assemblies frozen by the overstress fail-closed stop.
    pub overstressed: usize,
    /// Assemblies skipped because a driver error latched earlier.
    pub halted: usize,
    /// Assemblies whose block cost alone exceeds `max_work_units`.
    pub over_budget: usize,
    /// Per-region-batch breakdown in canonical plan order.
    pub batches: Vec<BatchTickReport>,
    /// Factory regions that produced step output this tick.
    pub factory_regions: usize,
    /// Factory batch records exported this tick.
    pub factory_records: usize,
    /// Orbital dimension transitions that fired this tick.
    pub orbital_transitions: usize,
    /// Combat-motion timelines still inside a non-idle phase this tick.
    pub combat_motions_active: usize,
    /// FNV-1a state digest after this tick — identical across worker counts.
    pub digest: u64,
}

/// A registered assembly: the live contraption plus its stress spec and
/// last-tick latch state.
struct OffloadedAssembly {
    contraption: LiveContraption,
    stress: AssemblyStress,
    status: AssemblyStatus,
    halted: bool,
}

impl OffloadedAssembly {
    /// Block-visits one full tick costs — bounded by
    /// [`MAX_BLOCKS_PER_ASSEMBLY`], minimum 1 so empty clusters still tick.
    fn work_cost(&self) -> usize {
        self.contraption.block_count().max(1)
    }
}

/// One entity's combat-motion state: the combo sequencing timeline plus
/// the hit latch for the swing currently in flight.
///
/// The latch lives here, not inside [`ComboState`], because the kernel's
/// [`ActiveAction::already_hit`] set is private mutation surface — the
/// bridge validates each hit against a throwaway clone of the live action
/// and records accepted target ids itself, resetting the set whenever a
/// new action starts.
#[derive(Debug)]
struct CombatMotionEntry {
    state: ComboState,
    /// Target ids already hit by the in-flight action.
    latched_hits: BTreeSet<u64>,
}

/// The offload bridge: an optional [`CreateFactory`] for kinetic/factory
/// networks plus a capped, region-partitioned set of live contraptions,
/// an [`OffloadSyncBridge`] feeding external/JVM graph deltas into the
/// offload kernels, an [`OrbitalTransitionRouter`] stepping every
/// registered 6-DOF ship through orbital dimension transitions,
/// server-authoritative [`combat_motion`](mod_offload::combat_motion)
/// timelines, and companion [`party_storage`](mod_offload::party_storage)
/// registries.
pub struct OffloadBridge {
    factory: Option<CreateFactory>,
    assemblies: BTreeMap<u32, OffloadedAssembly>,
    plan: ContraptionOffloadPlan,
    /// Round-robin scan start into `plan.order` — the fairness cursor that
    /// keeps deferred assemblies from starving behind cheap predecessors.
    cursor: usize,
    /// Last strictly-increasing tick stepped.
    tick: u64,
    started: bool,
    /// Running FNV-1a fingerprint of factory step outputs, folded in
    /// canonical region order each tick.
    factory_fp: FingerprintBuilder,
    /// Incremental ingestion of external/JVM dirty-node graph deltas into
    /// the offload kernels — kinetic, fluid, transport and ME.
    sync_bridge: OffloadSyncBridge<OffloadGraphTarget>,
    /// Atomic dimension-transition router for the registered ships.
    orbital_router: OrbitalTransitionRouter,
    /// Registered 6-DOF ships keyed by ship id — `BTreeMap` so stepping and
    /// the digest both run in canonical order.
    orbital_ships: BTreeMap<u64, RigidBodySubWorld>,
    /// Transitions fired by the most recent [`step_tick`](Self::step_tick)
    /// / [`step_orbital_ships`](Self::step_orbital_ships), queued until
    /// [`take_orbital_transitions`](Self::take_orbital_transitions) drains
    /// them (teleport intents ride out to the wire layer).
    pending_orbital_transitions: Vec<ShipDimensionTransition>,
    /// Server-authoritative combat-motion timelines keyed by entity id —
    /// `BTreeMap` so stepping and the digest run in canonical order.
    combat_motions: BTreeMap<u64, CombatMotionEntry>,
    /// Companion party + box storages keyed by owner UUID — `BTreeMap` so
    /// the digest is canonical. Empty by default: a server that never
    /// registers one pays nothing.
    companion_storages: BTreeMap<[u8; 16], PlayerCompanionStorage>,
}

/// Composite [`GraphDeltaSink`] the bridge's sync path owns: kinetic,
/// fluid and transport deltas land in a dedicated external [`Region`]
/// (namespace `dimension: i32::MAX`, disjoint from every live dim band)
/// while `Me*` deltas route to an [`MeGraphTarget`] — `Region` itself
/// rejects those as unsupported. The external region is authoritative
/// kernel state: ingest applies deltas through the same
/// [`GraphDeltaSink for Region`](mod_offload::sync_bridge) semantics the
/// scheduler uses, and `external_graph_target` exposes it for solutions
/// and exports.
pub struct OffloadGraphTarget {
    region: Region,
    me: MeGraphTarget,
}

impl OffloadGraphTarget {
    fn new() -> Self {
        Self {
            region: Region::new(RegionKey::for_block(i32::MAX, 0, 0)),
            me: MeGraphTarget::default(),
        }
    }

    /// The external region holding ingested kinetic/fluid/transport state.
    pub fn region(&self) -> &Region {
        &self.region
    }

    /// The ingested ME channel graph + priority storage index.
    pub fn me(&self) -> &MeGraphTarget {
        &self.me
    }
}

impl GraphDeltaSink for OffloadGraphTarget {
    fn apply(&mut self, delta: &ExternalGraphDelta) -> Result<(), BridgeError> {
        match delta {
            ExternalGraphDelta::MeNodeUpsert { .. }
            | ExternalGraphDelta::MeNodeRemove { .. }
            | ExternalGraphDelta::MeLink { .. }
            | ExternalGraphDelta::MeCellUpsert { .. }
            | ExternalGraphDelta::MeCellRemove { .. } => self.me.apply(delta),
            _ => self.region.apply(delta),
        }
    }
}

impl OffloadBridge {
    /// Bridge with contraption offload only; add a factory later via
    /// [`with_factory`](Self::with_factory) or field-free construction here.
    pub fn new() -> Self {
        Self {
            factory: None,
            assemblies: BTreeMap::new(),
            plan: ContraptionOffloadPlan::default(),
            cursor: 0,
            tick: 0,
            started: false,
            factory_fp: FingerprintBuilder::new(),
            sync_bridge: OffloadSyncBridge::new(OffloadGraphTarget::new()),
            orbital_router: OrbitalTransitionRouter::new(),
            orbital_ships: BTreeMap::new(),
            pending_orbital_transitions: Vec::new(),
            combat_motions: BTreeMap::new(),
            companion_storages: BTreeMap::new(),
        }
    }

    /// Bridge owning a [`CreateFactory`]; every [`step_tick`](Self::step_tick)
    /// also advances the factory's region scheduler under the budget's
    /// per-region limits and folds its exported frames into the digest.
    pub fn with_factory(factory: CreateFactory) -> Self {
        Self {
            factory: Some(factory),
            ..Self::new()
        }
    }

    /// The wrapped factory, when present.
    pub fn factory(&self) -> Option<&CreateFactory> {
        self.factory.as_ref()
    }

    /// Mutable factory access for setup (placing blocks, belts, tanks).
    /// Per-tick stepping always goes through [`step_tick`](Self::step_tick)
    /// so export bookkeeping and the digest stay consistent.
    pub fn factory_mut(&mut self) -> Option<&mut CreateFactory> {
        self.factory.as_mut()
    }

    /// Ingest one batch of external/JVM dirty-node deltas through the
    /// bridge's [`OffloadSyncBridge`]: state deltas coalesce last-wins per
    /// shadow key and apply in canonical order, `TransportInsert` events
    /// always apply in relative order, unchanged deltas are skipped, and
    /// per-delta failures collect in the returned [`BridgeSyncReport`].
    pub fn ingest_external_deltas(
        &mut self,
        deltas: &[ExternalGraphDelta],
    ) -> Result<BridgeSyncReport, BridgeError> {
        self.sync_bridge.ingest(deltas)
    }

    /// The sync bridge's sink — the external [`Region`] (kinetic, fluid,
    /// transport) plus [`MeGraphTarget`] (ME channel graph + storage).
    /// Read-only on purpose: mutating the sink behind the shadow map would
    /// desynchronize ingest's skip bookkeeping.
    pub fn external_graph_target(&self) -> &OffloadGraphTarget {
        self.sync_bridge.sink()
    }

    /// Register a dimension the orbital router can route ships through.
    pub fn register_orbital_dimension(&mut self, dim: u32, descriptor: OrbitalDimension) {
        self.orbital_router.register_dimension(dim, descriptor);
    }

    /// Register an orbital transition rule (e.g. `Y >= 560` Overworld ->
    /// orbit). Takes effect on the next ship step.
    pub fn register_orbital_rule(&mut self, rule: OrbitalTransitionRule) {
        self.orbital_router.register_rule(rule);
    }

    /// The orbital transition router (dimension table, rules, passengers,
    /// cooldowns).
    pub fn orbital_router(&self) -> &OrbitalTransitionRouter {
        &self.orbital_router
    }

    /// Register a 6-DOF ship the router steps every [`step_tick`]. Fails
    /// closed on a duplicate id or the [`MAX_ORBITAL_SHIPS`] cap.
    pub fn register_orbital_ship(
        &mut self,
        ship: RigidBodySubWorld,
    ) -> Result<(), OffloadError> {
        let id = ship.id;
        if self.orbital_ships.contains_key(&id) {
            return Err(OffloadError::DuplicateOrbitalShip(id));
        }
        if self.orbital_ships.len() >= MAX_ORBITAL_SHIPS {
            return Err(OffloadError::TooManyOrbitalShips {
                count: self.orbital_ships.len() + 1,
                limit: MAX_ORBITAL_SHIPS,
            });
        }
        self.orbital_ships.insert(id, ship);
        Ok(())
    }

    /// Remove a registered ship and return it to the caller. `None` when
    /// the id is not registered.
    pub fn unregister_orbital_ship(&mut self, ship_id: u64) -> Option<RigidBodySubWorld> {
        self.orbital_ships.remove(&ship_id)
    }

    /// Read-only access to a registered ship.
    pub fn orbital_ship(&self, ship_id: u64) -> Option<&RigidBodySubWorld> {
        self.orbital_ships.get(&ship_id)
    }

    /// Mutable access to a registered ship — the live caller's only way to
    /// apply thrust, player input, or block changes between ticks.
    pub fn orbital_ship_mut(&mut self, ship_id: u64) -> Option<&mut RigidBodySubWorld> {
        self.orbital_ships.get_mut(&ship_id)
    }

    /// Registered ship count; [`step_tick`](Self::step_tick) skips orbital
    /// work entirely when this is `0`.
    pub fn orbital_ship_count(&self) -> usize {
        self.orbital_ships.len()
    }

    /// Attach rider `entity_id` to `ship_id` at a local offset from the
    /// ship's center of mass; the rider rides through every dimension
    /// transition as a
    /// [`ShipPassengerTransition`](crate::orbital::ShipPassengerTransition).
    pub fn attach_orbital_passenger(
        &mut self,
        entity_id: i32,
        ship_id: u64,
        local_offset: [f64; 3],
    ) -> bool {
        self.orbital_router
            .attach_passenger(entity_id, ship_id, local_offset)
    }

    /// Detach rider `entity_id` (dismount, disassembly, logout).
    pub fn detach_orbital_passenger(&mut self, entity_id: i32) -> bool {
        self.orbital_router.detach_passenger(entity_id)
    }

    /// Step every registered ship once under the router at `dt` seconds,
    /// in ship-id order. Each ship flies under its current dimension's
    /// atmosphere, then the declarative rules evaluate against its
    /// post-step center of mass; fired rules atomically transfer the ship
    /// plus every attached passenger and emit one
    /// [`ShipDimensionTransition`]. Fired transitions are queued for
    /// [`take_orbital_transitions`](Self::take_orbital_transitions) AND
    /// returned to the caller in ship-id order.
    pub fn step_orbital_ships(&mut self, dt: f64) -> Vec<ShipDimensionTransition> {
        let mut fired = Vec::new();
        for ship in self.orbital_ships.values_mut() {
            if let Some(transition) =
                self.orbital_router.step_with_dimension_transitions(ship, dt)
            {
                fired.push(transition);
            }
        }
        self.pending_orbital_transitions.extend(fired.iter().cloned());
        fired
    }

    /// Drain every queued orbital transition — the wire layer's hand-off
    /// for ship re-parenting and passenger teleport intents.
    pub fn take_orbital_transitions(&mut self) -> Vec<ShipDimensionTransition> {
        std::mem::take(&mut self.pending_orbital_transitions)
    }

    /// Trigger the next combo step for `entity_id` at the authoritative
    /// server tick. The entity's [`ComboState`] is created on first use;
    /// `chain` is the caller's action table and `speed` the server-side
    /// attack-speed multiplier (clamped inside the kernel). A successful
    /// trigger starts a fresh swing, so the entity's latched hit set is
    /// cleared — a new action may hit the same targets again.
    ///
    /// Fails closed past [`MAX_COMBAT_MOTIONS`] or when the kernel rejects
    /// the trigger (`TickRewind`, `ActionLocked`, invalid spec/combo).
    pub fn trigger_combat_action(
        &mut self,
        entity_id: u64,
        chain: &ComboChain,
        now_tick: u64,
        speed: f32,
    ) -> Result<ActionPhase, OffloadError> {
        if !self.combat_motions.contains_key(&entity_id)
            && self.combat_motions.len() >= MAX_COMBAT_MOTIONS
        {
            return Err(OffloadError::TooManyCombatMotions {
                count: self.combat_motions.len() + 1,
                limit: MAX_COMBAT_MOTIONS,
            });
        }
        let entry = self.combat_motions.entry(entity_id).or_insert_with(|| {
            CombatMotionEntry {
                state: ComboState::default(),
                latched_hits: BTreeSet::new(),
            }
        });
        let phase = entry
            .state
            .trigger(chain, now_tick, speed)
            .map_err(OffloadError::CombatMotion)?
            .phase();
        entry.latched_hits.clear();
        Ok(phase)
    }

    /// Advance every tracked combat-motion timeline to `now_tick` — the
    /// same authoritative server tick [`step_tick`](Self::step_tick)
    /// forwards. Returns how many entities are inside a non-idle phase.
    /// Zero cost when no entity is tracked; a per-entity `TickRewind`
    /// fails closed as [`OffloadError::CombatMotion`].
    pub fn step_combat_motions(&mut self, now_tick: u64) -> Result<usize, OffloadError> {
        let mut active = 0usize;
        for entry in self.combat_motions.values_mut() {
            let phase = entry
                .state
                .advance_to(now_tick)
                .map_err(OffloadError::CombatMotion)?;
            if phase != ActionPhase::Idle {
                active += 1;
            }
        }
        Ok(active)
    }

    /// Server-side hit validation for `attacker_id`'s in-flight action.
    /// Untracked attackers and idle timelines fail as
    /// [`HitValidationVerdict::OutsideActiveWindow`]; a target already
    /// accepted this swing returns [`HitValidationVerdict::AlreadyHit`]
    /// and everything else defers to
    /// [`validate_hit`](mod_offload::combat_motion::validate_hit) on a
    /// clone of the live action — accepted hits are latched bridge-side
    /// so one sweep can never double-hit a target.
    ///
    /// `source`, `facing`, `candidate` and `target_action` (the target's
    /// dodge action, if any) are authoritative server state.
    pub fn validate_combat_hit(
        &mut self,
        attacker_id: u64,
        source: MotionVec3,
        facing: MotionVec3,
        candidate: HitCandidate,
        target_action: Option<&ActiveAction>,
    ) -> HitValidationVerdict {
        let Some(entry) = self.combat_motions.get_mut(&attacker_id) else {
            return HitValidationVerdict::OutsideActiveWindow;
        };
        let Some(action) = entry.state.current() else {
            return HitValidationVerdict::OutsideActiveWindow;
        };
        if entry.latched_hits.contains(&candidate.target_id) {
            return HitValidationVerdict::AlreadyHit;
        }
        let mut probe = action.clone();
        let verdict = validate_hit(&mut probe, source, facing, candidate, target_action);
        if verdict == HitValidationVerdict::Accepted {
            entry.latched_hits.insert(candidate.target_id);
        }
        verdict
    }

    /// The entity's live action snapshot — `None` when the entity is
    /// untracked or its last action has finished (the [`ComboState`] is
    /// retained for the combo window).
    pub fn combat_motion(&self, entity_id: u64) -> Option<&ActiveAction> {
        self.combat_motions
            .get(&entity_id)
            .and_then(|entry| entry.state.current())
    }

    /// Number of entities with a tracked combat-motion timeline.
    pub fn combat_motion_count(&self) -> usize {
        self.combat_motions.len()
    }

    /// Latest MOT1 wire snapshot for `entity_id` — `None` when the entity
    /// holds no live action.
    pub fn combat_motion_trigger(&self, entity_id: u64) -> Option<MotionTrigger> {
        self.combat_motion(entity_id)
            .map(|action| MotionTrigger::from_action(entity_id, action))
    }

    /// MOT1-encoded snapshot bytes — `None` when the entity holds no live
    /// action. Encoding cannot fail here: a stored action always carries
    /// the kernel-validated, range-clamped speed multiplier.
    pub fn encode_combat_motion(&self, entity_id: u64) -> Option<[u8; MOT1_PACKET_LEN]> {
        self.combat_motion_trigger(entity_id)
            .and_then(|trigger| encode_motion_trigger(&trigger).ok())
    }

    /// Drop the entity's combat-motion timeline entirely (despawn,
    /// logout). Returns whether a timeline existed.
    pub fn remove_combat_motion(&mut self, entity_id: u64) -> bool {
        self.combat_motions.remove(&entity_id).is_some()
    }

    /// Register (or replace) a player's companion party/box storage under
    /// `owner_uuid`. Fails closed past [`MAX_COMPANION_STORAGES`] for a
    /// new owner; replacing an existing owner never counts against the
    /// cap.
    pub fn register_companion_storage(
        &mut self,
        owner_uuid: [u8; 16],
        storage: PlayerCompanionStorage,
    ) -> Result<(), OffloadError> {
        if !self.companion_storages.contains_key(&owner_uuid)
            && self.companion_storages.len() >= MAX_COMPANION_STORAGES
        {
            return Err(OffloadError::TooManyCompanionStorages {
                count: self.companion_storages.len() + 1,
                limit: MAX_COMPANION_STORAGES,
            });
        }
        self.companion_storages.insert(owner_uuid, storage);
        Ok(())
    }

    /// Decode `bytes` as a `PRTY` blob and register it under
    /// `owner_uuid`. Decoding is fail-closed — bad magic, a CRC mismatch,
    /// truncation, a record invariant violation, or trailing garbage
    /// installs nothing and keeps any existing storage untouched.
    pub fn install_companion_storage(
        &mut self,
        owner_uuid: [u8; 16],
        bytes: &[u8],
    ) -> Result<(), OffloadError> {
        let storage = decode_storage(bytes).map_err(OffloadError::CompanionStorageDecode)?;
        self.register_companion_storage(owner_uuid, storage)
    }

    /// Remove and return the player's storage (logout, export, world
    /// hand-off). `None` when no storage is registered for the owner.
    pub fn take_companion_storage(
        &mut self,
        owner_uuid: &[u8; 16],
    ) -> Option<PlayerCompanionStorage> {
        self.companion_storages.remove(owner_uuid)
    }

    /// Read-only access to a player's companion storage.
    pub fn companion_storage(&self, owner_uuid: &[u8; 16]) -> Option<&PlayerCompanionStorage> {
        self.companion_storages.get(owner_uuid)
    }

    /// Mutable access to a player's companion storage — the live caller's
    /// route for captures, slot swaps, heals and releases between ticks.
    pub fn companion_storage_mut(
        &mut self,
        owner_uuid: &[u8; 16],
    ) -> Option<&mut PlayerCompanionStorage> {
        self.companion_storages.get_mut(owner_uuid)
    }

    /// Number of registered companion storages.
    pub fn companion_storage_count(&self) -> usize {
        self.companion_storages.len()
    }

    /// Canonical `PRTY` bytes for one owner's storage — the codec is
    /// byte-deterministic, so identical state always encodes identically.
    /// `None` when the owner has no registered storage.
    pub fn encode_companion_storage(&self, owner_uuid: &[u8; 16]) -> Option<Vec<u8>> {
        self.companion_storages.get(owner_uuid).map(encode_storage)
    }

    /// Register a live contraption under its own `id` with a stress spec.
    /// Fails closed on duplicate id, the [`MAX_ASSEMBLIES`] cap, the
    /// [`MAX_BLOCKS_PER_ASSEMBLY`] cap, or a non-finite/negative stress spec.
    pub fn register_contraption(
        &mut self,
        contraption: LiveContraption,
        stress: AssemblyStress,
    ) -> Result<(), OffloadError> {
        let id = contraption.id;
        if self.assemblies.contains_key(&id) {
            return Err(OffloadError::DuplicateAssembly(id));
        }
        if self.assemblies.len() >= MAX_ASSEMBLIES {
            return Err(OffloadError::TooManyAssemblies {
                count: self.assemblies.len() + 1,
                limit: MAX_ASSEMBLIES,
            });
        }
        let blocks = contraption.block_count();
        if blocks > MAX_BLOCKS_PER_ASSEMBLY {
            return Err(OffloadError::AssemblyTooLarge {
                id,
                count: blocks,
                limit: MAX_BLOCKS_PER_ASSEMBLY,
            });
        }
        let stress = AssemblyStress::validated(stress.capacity, stress.consumption)?;
        self.assemblies.insert(
            id,
            OffloadedAssembly {
                contraption,
                stress,
                status: AssemblyStatus::Idle,
                halted: false,
            },
        );
        self.rebuild_plan()?;
        Ok(())
    }

    /// Remove an assembly and return it to the caller (e.g. for checked
    /// disassembly). Returns `None` when the id is not registered.
    pub fn unregister_contraption(&mut self, id: u32) -> Option<LiveContraption> {
        let removed = self.assemblies.remove(&id)?;
        self.rebuild_plan().ok()?;
        Some(removed.contraption)
    }

    /// Replace an assembly's stress spec; takes effect on the next tick.
    pub fn set_assembly_stress(
        &mut self,
        id: u32,
        stress: AssemblyStress,
    ) -> Result<(), OffloadError> {
        let asm = self
            .assemblies
            .get_mut(&id)
            .ok_or(OffloadError::MissingAssembly(id))?;
        asm.stress = AssemblyStress::validated(stress.capacity, stress.consumption)?;
        Ok(())
    }

    /// Clear a latched driver halt so the assembly is eligible again. An
    /// assembly whose driver still fails simply re-halts on its next step.
    pub fn resume_assembly(&mut self, id: u32) -> Result<(), OffloadError> {
        let asm = self
            .assemblies
            .get_mut(&id)
            .ok_or(OffloadError::MissingAssembly(id))?;
        asm.halted = false;
        asm.status = AssemblyStatus::Idle;
        Ok(())
    }

    /// Read-only access to a registered contraption.
    pub fn assembly(&self, id: u32) -> Option<&LiveContraption> {
        self.assemblies.get(&id).map(|asm| &asm.contraption)
    }

    /// Last-tick status of a registered assembly.
    pub fn assembly_status(&self, id: u32) -> Option<AssemblyStatus> {
        self.assemblies.get(&id).map(|asm| asm.status)
    }

    /// Registered assembly ids in ascending order.
    pub fn assembly_ids(&self) -> impl Iterator<Item = u32> + '_ {
        self.assemblies.keys().copied()
    }

    pub fn assembly_count(&self) -> usize {
        self.assemblies.len()
    }

    /// The current region-disjoint offload plan.
    pub fn plan(&self) -> &ContraptionOffloadPlan {
        &self.plan
    }

    /// Last stepped tick.
    pub fn tick(&self) -> u64 {
        self.tick
    }

    /// FNV-1a state digest over every assembly's canonical state plus the
    /// accumulated factory output fingerprint. Bit-identical for any worker
    /// count or partition shape — the parity proof the tests rely on.
    pub fn state_digest(&self) -> u64 {
        let mut f = FingerprintBuilder::new();
        f.write_u64(self.tick);
        f.write_u64(self.cursor as u64);
        f.write_u64(self.factory_fp.finish());
        f.write_usize(self.assemblies.len());
        for id in &self.plan.order {
            let Some(asm) = self.assemblies.get(id) else {
                continue;
            };
            let c = &asm.contraption;
            f.write_u32(c.id);
            f.write_i32(c.dim);
            for v in c.anchor {
                f.write_i32(v);
            }
            f.write_usize(c.block_count());
            write_kind(&mut f, c.kind);
            f.write_u32(c.yaw_deg.to_bits());
            f.write_u32(c.pitch_deg.to_bits());
            for v in c.translation {
                f.write_f64(v);
            }
            match c.world_aabb() {
                Some(aabb) => {
                    f.write_bool(true);
                    for v in aabb.min() {
                        f.write_f64(v);
                    }
                    for v in aabb.max() {
                        f.write_f64(v);
                    }
                }
                None => {
                    f.write_bool(false);
                }
            }
            f.write_u8(status_code(asm.status));
            f.write_bool(asm.halted);
            f.write_f64(asm.stress.capacity);
            f.write_f64(asm.stress.consumption);
        }
        // Orbital fleet: ship id order is canonical, and the router tick +
        // queued transitions keep bridge state fully fingerprinted.
        f.write_u64(self.orbital_router.tick());
        f.write_usize(self.pending_orbital_transitions.len());
        f.write_usize(self.orbital_ships.len());
        for (id, ship) in &self.orbital_ships {
            f.write_u64(*id);
            f.write_u32(ship.parent_dim);
            for v in ship.pos {
                f.write_f64(v);
            }
            for v in ship.quat {
                f.write_f64(v);
            }
            for v in ship.vel {
                f.write_f64(v);
            }
            for v in ship.omega {
                f.write_f64(v);
            }
        }
        // Combat-motion timelines: entity-id order is canonical.
        f.write_usize(self.combat_motions.len());
        for (entity_id, entry) in &self.combat_motions {
            f.write_u64(*entity_id);
            match entry.state.current() {
                Some(action) => {
                    f.write_bool(true);
                    write_combat_action(&mut f, action);
                }
                None => {
                    f.write_bool(false);
                }
            }
            f.write_usize(entry.latched_hits.len());
            for target in &entry.latched_hits {
                f.write_u64(*target);
            }
        }
        // Companion storages: owner-UUID order is canonical, and the
        // PRTY codec is already byte-deterministic — folding the encoded
        // form fingerprints every record field with zero extra codec
        // surface.
        f.write_usize(self.companion_storages.len());
        for (owner_uuid, storage) in &self.companion_storages {
            f.write_bytes(owner_uuid);
            f.write_bytes(&encode_storage(storage));
        }
        f.finish()
    }

    /// Advance the whole bridge by one tick.
    ///
    /// Order of operations is fixed and deterministic: the factory's region
    /// scheduler runs first (its outputs fold into the digest), then the
    /// contraption budget scan approves work in canonical order, and finally
    /// approved assemblies step — sequentially for `workers = 1`, or spread
    /// over up to `workers` scoped threads at region-batch granularity.
    /// `tick` must strictly increase.
    pub fn step_tick(
        &mut self,
        tick: u64,
        budget: OffloadBudget,
    ) -> Result<OffloadTickReport, OffloadError> {
        if self.started && tick <= self.tick {
            return Err(OffloadError::TickWentBackwards {
                current: self.tick,
                requested: tick,
            });
        }
        self.started = true;
        self.tick = tick;

        let mut report = OffloadTickReport {
            tick,
            work_units_budget: budget.max_work_units,
            ..OffloadTickReport::empty()
        };

        // 1. Factory side: region-scheduled kinetic/factory work. Outputs
        // arrive in sorted region order, so folding them here is canonical.
        if let Some(factory) = self.factory.as_mut() {
            let outputs = factory
                .step_tick(
                    tick,
                    budget.factory_region_steps,
                    budget.factory_region_micros,
                )
                .map_err(OffloadError::Factory)?;
            report.factory_regions = outputs.len();
            for out in &outputs {
                report.factory_records += out.batch.records;
                let r = out.region;
                self.factory_fp.write_i32(r.dimension);
                self.factory_fp.write_i32(r.x);
                self.factory_fp.write_i32(r.y);
                self.factory_fp.write_i32(r.z);
                self.factory_fp.write_usize(out.report.processed);
                self.factory_fp.write_bool(out.report.output_blocked);
                self.factory_fp.write_bool(out.report.time_exhausted);
                match out.report.backlog {
                    Some(due) => {
                        self.factory_fp.write_bool(true);
                        self.factory_fp.write_u64(due);
                    }
                    None => {
                        self.factory_fp.write_bool(false);
                    }
                }
                self.factory_fp.write_usize(out.batch.records);
                self.factory_fp.write_usize(out.batch.bytes);
                match out.batch.sequence {
                    Some(seq) => {
                        self.factory_fp.write_bool(true);
                        self.factory_fp.write_u64(seq);
                    }
                    None => {
                        self.factory_fp.write_bool(false);
                    }
                }
                self.factory_fp.write_bytes(&out.s5_frame);
            }
        }

        // 2. Contraption budget scan — single-threaded, canonical order.
        let n = self.plan.order.len();
        let mut batch_acc = vec![BatchAcc::default(); self.plan.batches.len()];
        let mut approved: Vec<usize> = Vec::new();
        if n > 0 {
            let mut remaining = budget.max_work_units;
            let mut first_deferrable = None;
            let start = self.cursor % n;
            for k in 0..n {
                let pos = (start + k) % n;
                let id = self.plan.order[pos];
                let batch = self.plan.batch_of_pos[pos];
                let asm = self.assemblies.get_mut(&id).expect("plan mirrors map");
                if asm.halted {
                    asm.status = AssemblyStatus::Halted;
                    report.halted += 1;
                    batch_acc[batch].stopped += 1;
                    continue;
                }
                if asm.stress.overstressed() {
                    asm.status = AssemblyStatus::Overstressed;
                    report.overstressed += 1;
                    batch_acc[batch].stopped += 1;
                    continue;
                }
                let cost = asm.work_cost();
                if cost > budget.max_work_units {
                    asm.status = AssemblyStatus::OverBudget;
                    report.over_budget += 1;
                    batch_acc[batch].stopped += 1;
                    continue;
                }
                if cost <= remaining {
                    remaining -= cost;
                    approved.push(pos);
                    batch_acc[batch].scheduled += 1;
                } else {
                    asm.status = AssemblyStatus::Deferred;
                    report.deferred += 1;
                    batch_acc[batch].deferred += 1;
                    if first_deferrable.is_none() {
                        first_deferrable = Some(pos);
                    }
                }
            }
            report.work_units_used = budget.max_work_units - remaining;
            // Fairness: next tick starts at the first assembly the budget
            // could not reach, so it claims the fresh budget first. Over-
            // budget assemblies never pin the cursor — they are reported,
            // not served, and cannot starve servable neighbours.
            self.cursor = first_deferrable.unwrap_or(start);
        }

        // 3. Execute approved work. Assignment to workers is at region-batch
        // granularity and computed from the already-approved set, so the
        // executed set is identical for every worker count.
        let workers = budget.workers.max(1);
        let worker_of_pos = self.assign_workers(&approved, workers);
        let approved_ids: BTreeSet<u32> = approved
            .iter()
            .map(|&pos| self.plan.order[pos])
            .collect();
        let mut per_worker: Vec<Vec<&mut OffloadedAssembly>> =
            (0..workers).map(|_| Vec::new()).collect();
        for (id, asm) in self.assemblies.iter_mut() {
            if !approved_ids.contains(id) {
                continue;
            }
            let pos = self.plan.position_of(*id).expect("plan mirrors map");
            per_worker[worker_of_pos[pos]].push(asm);
        }

        let mut outcomes: Vec<(u32, Result<bool, ContraptionError>)> = Vec::new();
        let active_workers = per_worker.iter().filter(|w| !w.is_empty()).count();
        if workers == 1 || active_workers <= 1 {
            for chunk in per_worker.iter_mut() {
                for asm in chunk.iter_mut() {
                    outcomes.push((asm.contraption.id, asm.contraption.step_tick()));
                }
            }
        } else {
            thread::scope(|scope| {
                let mut handles = Vec::with_capacity(active_workers);
                for chunk in per_worker.iter_mut() {
                    if chunk.is_empty() {
                        continue;
                    }
                    handles.push(scope.spawn(move || {
                        let mut local = Vec::with_capacity(chunk.len());
                        for asm in chunk.iter_mut() {
                            local.push((asm.contraption.id, asm.contraption.step_tick()));
                        }
                        local
                    }));
                }
                for handle in handles {
                    outcomes.extend(
                        handle
                            .join()
                            .unwrap_or_else(|payload| std::panic::resume_unwind(payload)),
                    );
                }
            });
        }

        // 4. Merge outcomes in canonical order — deterministic regardless of
        // which worker produced them.
        outcomes.sort_by_key(|(id, _)| {
            self.plan.position_of(*id).unwrap_or(usize::MAX)
        });
        for (id, result) in outcomes {
            let pos = self.plan.position_of(id).expect("plan mirrors map");
            let batch = self.plan.batch_of_pos[pos];
            let asm = self.assemblies.get_mut(&id).expect("plan mirrors map");
            match result {
                Ok(_) => {
                    asm.status = AssemblyStatus::Running;
                    report.stepped += 1;
                    batch_acc[batch].stepped += 1;
                }
                Err(_) => {
                    asm.halted = true;
                    asm.status = AssemblyStatus::Halted;
                    report.halted += 1;
                    batch_acc[batch].stopped += 1;
                }
            }
        }

        // 5. Orbital ships — one router step per registered 6-DOF ship at
        // the fixed server-tick dt; transition rules fire against the
        // post-step pose. Zero cost when no ship is registered.
        if !self.orbital_ships.is_empty() {
            report.orbital_transitions =
                self.step_orbital_ships(ORBITAL_SHIP_STEP_DT).len();
        }

        // 6. Combat-motion timelines — advance every tracked entity to
        // the same authoritative server tick. Zero cost when empty.
        if !self.combat_motions.is_empty() {
            report.combat_motions_active = self.step_combat_motions(tick)?;
        }

        report.batches = self
            .plan
            .batches
            .iter()
            .zip(batch_acc.iter())
            .map(|(batch, acc)| BatchTickReport {
                region: batch.region,
                assemblies: batch.members.len(),
                scheduled: acc.scheduled,
                stepped: acc.stepped,
                deferred: acc.deferred,
                stopped: acc.stopped,
            })
            .collect();
        report.digest = self.state_digest();
        Ok(report)
    }

    /// Map each approved canonical position to a worker index. Approved
    /// positions are contiguous per region batch, so workers receive
    /// consecutive runs of whole batches — region-disjoint by construction.
    fn assign_workers(&self, approved: &[usize], workers: usize) -> Vec<usize> {
        let n = self.plan.order.len();
        let mut worker_of_pos = vec![usize::MAX; n];
        if approved.is_empty() {
            return worker_of_pos;
        }
        // Count region batches that actually have approved work.
        let mut nonempty = 0usize;
        let mut last_batch = usize::MAX;
        for &pos in approved {
            let batch = self.plan.batch_of_pos[pos];
            if batch != last_batch {
                nonempty += 1;
                last_batch = batch;
            }
        }
        let quota = nonempty.div_ceil(workers).max(1);
        let mut worker = 0usize;
        let mut filled = 0usize;
        let mut current_batch = usize::MAX;
        for &pos in approved {
            let batch = self.plan.batch_of_pos[pos];
            if batch != current_batch {
                current_batch = batch;
                if filled == quota && worker + 1 < workers {
                    worker += 1;
                    filled = 0;
                }
                filled += 1;
            }
            worker_of_pos[pos] = worker;
        }
        worker_of_pos
    }

    fn rebuild_plan(&mut self) -> Result<(), OffloadError> {
        let entries: Vec<(u32, RegionKey, usize)> = self
            .assemblies
            .iter()
            .map(|(&id, asm)| {
                let c = &asm.contraption;
                (
                    id,
                    RegionKey::for_block(c.dim, c.anchor[0], c.anchor[2]),
                    c.block_count(),
                )
            })
            .collect();
        self.plan = ContraptionOffloadPlan::build(&entries)?;
        if !self.plan.order.is_empty() {
            self.cursor %= self.plan.order.len();
        }
        Ok(())
    }
}

impl Default for OffloadBridge {
    fn default() -> Self {
        Self::new()
    }
}

impl OffloadTickReport {
    fn empty() -> Self {
        Self {
            tick: 0,
            work_units_budget: 0,
            work_units_used: 0,
            stepped: 0,
            deferred: 0,
            overstressed: 0,
            halted: 0,
            over_budget: 0,
            batches: Vec::new(),
            factory_regions: 0,
            factory_records: 0,
            orbital_transitions: 0,
            combat_motions_active: 0,
            digest: 0,
        }
    }
}

#[derive(Clone, Default)]
struct BatchAcc {
    scheduled: usize,
    stepped: usize,
    deferred: usize,
    stopped: usize,
}

fn status_code(status: AssemblyStatus) -> u8 {
    match status {
        AssemblyStatus::Idle => 0,
        AssemblyStatus::Running => 1,
        AssemblyStatus::Deferred => 2,
        AssemblyStatus::Overstressed => 3,
        AssemblyStatus::OverBudget => 4,
        AssemblyStatus::Halted => 5,
    }
}

/// Deterministic fingerprint of one live combat action: spec, timeline
/// position and the per-swing hit set, in field order — the digest never
/// depends on `BTreeMap`/`BTreeSet` internals.
fn write_combat_action(f: &mut FingerprintBuilder, action: &ActiveAction) {
    let spec = action.spec();
    f.write_u32(spec.action_id);
    f.write_u32(spec.anticipation_ticks);
    f.write_u32(spec.active_ticks);
    f.write_u32(spec.recovery_ticks);
    match spec.cancel_after_tick {
        Some(t) => {
            f.write_bool(true);
            f.write_u32(t);
        }
        None => {
            f.write_bool(false);
        }
    }
    match spec.invulnerable_window {
        Some(w) => {
            f.write_bool(true);
            f.write_u32(w.start_tick);
            f.write_u32(w.end_tick);
        }
        None => {
            f.write_bool(false);
        }
    }
    f.write_u32(spec.movement_speed_factor.to_bits());
    match spec.hit_shape {
        HitShape::SweepArc {
            reach,
            half_angle_deg,
            vertical_tolerance,
        } => {
            f.write_u8(0);
            f.write_f64(reach);
            f.write_f64(half_angle_deg);
            f.write_f64(vertical_tolerance);
        }
        HitShape::ThrustCapsule { reach, radius } => {
            f.write_u8(1);
            f.write_f64(reach);
            f.write_f64(radius);
        }
        HitShape::RadialAoe { radius } => {
            f.write_u8(2);
            f.write_f64(radius);
        }
        HitShape::ForwardCone {
            reach,
            half_angle_deg,
        } => {
            f.write_u8(3);
            f.write_f64(reach);
            f.write_f64(half_angle_deg);
        }
    }
    f.write_u64(action.start_tick());
    f.write_u32(action.elapsed_ticks());
    f.write_u32(action.speed_multiplier().to_bits());
    f.write_u8(action.hand() as u8);
    f.write_u16(action.combo_index());
    f.write_u8(action.phase() as u8);
    f.write_usize(action.already_hit().len());
    for target in action.already_hit() {
        f.write_u64(*target);
    }
}

fn write_kind(f: &mut FingerprintBuilder, kind: ContraptionKind) {
    match kind {
        ContraptionKind::Bearing {
            angular_deg_per_tick,
        } => {
            f.write_u8(0);
            f.write_u32(angular_deg_per_tick.to_bits());
        }
        ContraptionKind::Linear { velocity_per_tick } => {
            f.write_u8(1);
            for v in velocity_per_tick {
                f.write_f64(v);
            }
        }
        ContraptionKind::BoundedLinear {
            velocity_per_tick,
            min_offset,
            max_offset,
            reverse_at_bounds,
        } => {
            f.write_u8(2);
            for v in velocity_per_tick {
                f.write_f64(v);
            }
            for v in min_offset {
                f.write_f64(v);
            }
            for v in max_offset {
                f.write_f64(v);
            }
            f.write_bool(reverse_at_bounds);
        }
        ContraptionKind::Minecart {
            speed_per_tick,
            heading_yaw,
        } => {
            f.write_u8(3);
            f.write_f64(speed_per_tick);
            f.write_f64(heading_yaw);
        }
    }
}
