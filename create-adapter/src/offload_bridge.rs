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

use mc_spine_core::perf_kit::FingerprintBuilder;
use mod_offload::region_scheduler::RegionKey;
use mod_offload::sleeping_transport::TransportError;

use crate::contraption_adapter::{ContraptionError, ContraptionKind, LiveContraption};
use crate::factory::CreateFactory;

/// Maximum simultaneously registered contraption assemblies.
pub const MAX_ASSEMBLIES: usize = 1024;
/// Maximum blocks one assembly may carry into the offload bridge.
pub const MAX_BLOCKS_PER_ASSEMBLY: usize = 4096;
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
            for pos in start..order.len() {
                position_of.insert(order[pos], pos);
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

/// The offload bridge: an optional [`CreateFactory`] for kinetic/factory
/// networks plus a capped, region-partitioned set of live contraptions.
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
            if !approved_ids.contains(&id) {
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
