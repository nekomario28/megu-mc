// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later

//! Create Mod factory adapter bridging world block edits onto `mod-offload`'s
//! region-isolated `KineticGraph`, `SleepingTransport`, `RecipeProcessor`, and
//! `FluidNetwork` kernels across arbitrary dynamic dimensions (`dim: i32`).

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::time::Duration;

use megu_core::perf_kit::{
    BoundedScratchVec, CoalescedDeltaBatch, DirtyFingerprintGate, FingerprintBuilder,
    FingerprintStats,
};
use mod_offload::fluid_network::FluidError;
use mod_offload::kinetic_graph::{GraphError, KineticNode, NetworkStatus, Solution};
use mod_offload::recipe_processor::{Drive, Recipe, RecipeError, Stack};
use mod_offload::region_scheduler::{
    BatchReport, RegionBudget, RegionKey, RegionReport, RegionScheduler, MAX_BATCH_BYTES,
    MAX_BATCH_RECORDS,
};
use mod_offload::sleeping_transport::TransportError;

use crate::blockstates::{CreateBlockRegistry, CreateBlockRole};

/// Capacity (elements) the reconcile scratch vecs retain across calls —
/// past this, [`BoundedScratchVec::clear_bounded`] shrinks them back so a
/// one-off huge region doesn't inflate idle RSS.
const RECONCILE_SCRATCH_RETAIN: usize = 4096;

/// World-space block coordinate with arbitrary dynamic dimension ID (`0=Overworld`,
/// `1=Nether`, `2=End`, `3+=custom`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DimBlockPos {
    pub dim: i32,
    pub x: i32,
    pub y: i32,
    pub z: i32,
}

impl DimBlockPos {
    pub const fn new(dim: i32, x: i32, y: i32, z: i32) -> Self {
        Self { dim, x, y, z }
    }

    pub fn region_key(self) -> RegionKey {
        RegionKey::for_block(self.dim, self.x, self.z)
    }

    pub const fn offset(self, dx: i32, dy: i32, dz: i32) -> Self {
        Self {
            dim: self.dim,
            x: self.x + dx,
            y: self.y + dy,
            z: self.z + dz,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Axis {
    X,
    Y,
    Z,
}

impl Axis {
    /// `x`/`y`/`z` axis name (`"x"`, `"y"`, `"z"`).
    pub fn name(self) -> &'static str {
        match self {
            Self::X => "x",
            Self::Y => "y",
            Self::Z => "z",
        }
    }

    /// Parse `x`/`y`/`z` into an [`Axis`].
    pub fn from_name(s: &str) -> Option<Self> {
        match s {
            "x" => Some(Self::X),
            "y" => Some(Self::Y),
            "z" => Some(Self::Z),
            _ => None,
        }
    }

    /// Component index (`x`->0, `y`->1, `z`->2) into `[f64; 3]` vectors.
    pub fn index(self) -> usize {
        match self {
            Self::X => 0,
            Self::Y => 1,
            Self::Z => 2,
        }
    }

    fn from_prop(s: Option<&str>) -> Option<Self> {
        match s {
            Some("x") => Some(Self::X),
            Some("y") => Some(Self::Y),
            Some("z") => Some(Self::Z),
            _ => None,
        }
    }

    fn from_facing(s: Option<&str>) -> Option<Self> {
        match s {
            Some("east" | "west") => Some(Self::X),
            Some("up" | "down") => Some(Self::Y),
            Some("north" | "south") => Some(Self::Z),
            _ => None,
        }
    }

    fn unit_deltas(self) -> [(i32, i32, i32); 2] {
        match self {
            Self::X => [(1, 0, 0), (-1, 0, 0)],
            Self::Y => [(0, 1, 0), (0, -1, 0)],
            Self::Z => [(0, 0, 1), (0, 0, -1)],
        }
    }

    fn perpendicular_deltas(self) -> [(i32, i32, i32); 4] {
        match self {
            Self::X => [(0, 1, 0), (0, -1, 0), (0, 0, 1), (0, 0, -1)],
            Self::Y => [(1, 0, 0), (-1, 0, 0), (0, 0, 1), (0, 0, -1)],
            Self::Z => [(1, 0, 0), (-1, 0, 0), (0, 1, 0), (0, -1, 0)],
        }
    }

    fn diagonal_deltas(self) -> [(i32, i32, i32); 4] {
        match self {
            Self::X => [(0, 1, 1), (0, 1, -1), (0, -1, 1), (0, -1, -1)],
            Self::Y => [(1, 0, 1), (1, 0, -1), (-1, 0, 1), (-1, 0, -1)],
            Self::Z => [(1, 1, 0), (1, -1, 0), (-1, 1, 0), (-1, -1, 0)],
        }
    }
}

/// Minimal BlockEntity UpdateTag delta emitted only when a node's solved
/// kinetic state changes.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct KineticUpdateTag {
    pub pos: DimBlockPos,
    pub speed_rpm: f64,
    pub stress_capacity: f64,
    pub stress_load: f64,
    pub overstressed: bool,
}

#[derive(Clone, Copy, Debug)]
struct PlacedKinetic {
    id: u32,
    state_id: u32,
    role: CreateBlockRole,
    axis: Axis,
    powered: bool,
    stress_impact: f64,
    stress_capacity: f64,
}

/// Per-tick region execution summary returned by [`CreateFactory::step_tick`].
#[derive(Clone, Debug)]
pub struct RegionStepOutput {
    pub region: RegionKey,
    pub report: RegionReport,
    pub batch: BatchReport,
    pub s5_frame: Vec<u8>,
}

/// Native Rust Create factory engine owning a [`CreateBlockRegistry`] and a
/// [`RegionScheduler`] partitioned by `(dim, region_x, region_z)`.
pub struct CreateFactory {
    registry: CreateBlockRegistry,
    scheduler: RegionScheduler,
    active_regions: BTreeSet<RegionKey>,
    active_regions_last_tick: usize,
    next_node_id: u32,
    kinetics: HashMap<DimBlockPos, PlacedKinetic>,
    kinetic_edges: BTreeMap<(DimBlockPos, DimBlockPos), f64>,
    /// Per-node publish gate: the committed fingerprint of each block
    /// entity's last-exported [`Solution`], replacing a full `Solution`
    /// map with 64-bit entries. A re-solved state whose fingerprint
    /// matches is skipped in O(1) — no UpdateTag is emitted.
    update_gate: DirtyFingerprintGate<DimBlockPos>,
    /// Last-write-wins UpdateTag accumulator for the per-tick sync flush;
    /// a node re-solved several times inside one window emits one delta.
    update_batch: CoalescedDeltaBatch<DimBlockPos, KineticUpdateTag>,
    processors_by_pos: HashMap<DimBlockPos, u32>,
    /// Regions whose batch queue may hold unexported records. Only work
    /// processed by `advance_to` (or an external `queue_delta`) pushes
    /// records, so a region absent from this set provably exports nothing
    /// and its `export_into` call is skipped in O(1).
    pending_exports: BTreeSet<RegionKey>,
    /// `export_into` calls skipped because the batch was provably empty.
    idle_export_skips: u64,
    /// Shared export window — one buffer for every region, every tick.
    export_scratch: BoundedScratchVec<u8>,
    /// Reusable region-node snapshot for [`Self::reconcile_stress_and_diffs`].
    reconcile_nodes: BoundedScratchVec<(DimBlockPos, PlacedKinetic)>,
    /// Reusable nominal-RPM snapshot for the same reconcile pass.
    reconcile_rpms: BoundedScratchVec<(PlacedKinetic, f64)>,
}

impl CreateFactory {
    pub fn new(registry: CreateBlockRegistry) -> Self {
        let mut export_scratch = BoundedScratchVec::new();
        export_scratch.resize(MAX_BATCH_BYTES, 0);
        Self {
            registry,
            scheduler: RegionScheduler::default(),
            active_regions: BTreeSet::new(),
            active_regions_last_tick: 0,
            next_node_id: 1,
            kinetics: HashMap::new(),
            kinetic_edges: BTreeMap::new(),
            update_gate: DirtyFingerprintGate::new(),
            update_batch: CoalescedDeltaBatch::new(),
            processors_by_pos: HashMap::new(),
            pending_exports: BTreeSet::new(),
            idle_export_skips: 0,
            export_scratch,
            reconcile_nodes: BoundedScratchVec::new(),
            reconcile_rpms: BoundedScratchVec::new(),
        }
    }

    /// Build a factory whose registry merges the core `create:*` catalog with
    /// every [`crate::addon_profile::CreateAddonProfile`]'s flagship block
    /// definitions. Addon ids colliding with `create:*` or each other fail
    /// closed via [`crate::blockstates::RegistryError`].
    pub fn new_with_addons(
        base_state_id: u32,
        addons: &[crate::addon_profile::CreateAddonProfile],
    ) -> Result<Self, crate::blockstates::RegistryError> {
        Ok(Self::new(crate::addon_profile::create_registry_with_addons(
            base_state_id,
            addons,
        )?))
    }

    pub fn registry(&self) -> &CreateBlockRegistry {
        &self.registry
    }

    pub fn scheduler(&self) -> &RegionScheduler {
        &self.scheduler
    }

    pub fn scheduler_mut(&mut self) -> &mut RegionScheduler {
        // Direct region access can push batch records (`queue_delta`) or
        // reschedule work through paths the factory cannot see. Mark every
        // active region as possibly-pending so the next `step_tick`
        // re-checks them instead of skipping a queue that silently filled.
        self.pending_exports
            .extend(self.active_regions.iter().copied());
        &mut self.scheduler
    }

    pub fn kinetic_node_count(&self) -> usize {
        self.kinetics.len()
    }

    /// Skip/update counters for the UpdateTag publish gate: `skips` counts
    /// re-solved states identical to the last publish (no tag emitted),
    /// `updates` counts committed state changes.
    pub fn update_gate_stats(&self) -> FingerprintStats {
        self.update_gate.stats()
    }

    /// UpdateTags coalesced since the last
    /// [`drain_update_batch_into`](Self::drain_update_batch_into) flush.
    pub fn pending_update_deltas(&self) -> usize {
        self.update_batch.len()
    }

    /// Append every coalesced UpdateTag delta to `out` in sorted
    /// `DimBlockPos` order, emptying the batch — one tag per touched block
    /// entity no matter how many times it re-solved inside the window.
    pub fn drain_update_batch_into(&mut self, out: &mut Vec<(DimBlockPos, KineticUpdateTag)>) {
        self.update_batch.drain_sorted_into(out);
    }

    /// `export_into` calls skipped because the region's batch queue was
    /// provably empty — the measurable "idle regions cost O(1)" counter.
    pub fn idle_export_skips(&self) -> u64 {
        self.idle_export_skips
    }

    /// Active factory regions parked until a mutation or scheduled deadline.
    pub fn sleeping_regions(&self) -> usize {
        self.active_regions
            .iter()
            .filter(|&&key| self.scheduler.is_sleeping(key))
            .count()
    }

    /// Regions advanced during the most recent factory tick.
    pub fn active_regions_last_tick(&self) -> usize {
        self.active_regions_last_tick
    }

    pub fn kinetic_solution(&self, pos: DimBlockPos) -> Option<Solution> {
        let placed = self.kinetics.get(&pos)?;
        let region = self.scheduler.region(pos.region_key())?;
        region.kinetic.solution(placed.id)
    }

    /// Place or replace a Create block at `pos`, updating the kinetic graph
    /// topology and returning only the nodes whose solved state changed.
    pub fn place_block(&mut self, pos: DimBlockPos, state_id: u32) -> Vec<KineticUpdateTag> {
        if self.kinetics.contains_key(&pos) {
            let _ = self.remove_block(pos);
        }
        let Some(entry) = self.registry.state(state_id) else {
            return Vec::new();
        };
        let Some(def) = self.registry.block_def_for_state(state_id) else {
            return Vec::new();
        };

        let axis = Axis::from_prop(entry.property("axis"))
            .or_else(|| Axis::from_facing(entry.property("facing")))
            .unwrap_or(Axis::Y);
        let powered = entry.property("powered") == Some("true");

        let region_key = pos.region_key();
        self.active_regions.insert(region_key);

        let mut placed = PlacedKinetic {
            id: 0,
            state_id,
            role: def.role,
            axis,
            powered,
            stress_impact: def.load_factor,
            stress_capacity: def.capacity_factor,
        };
        let input = kinetic_node_input(placed, 1.0);
        let region = self.scheduler.region_mut(region_key);
        // Draw ids until the graph accepts one: next_node_id wraps rather
        // than panicking past u32::MAX, and an id still live in the graph
        // is skipped (DuplicateNode) instead of panic-colliding. Any other
        // failure means the registry-derived input itself was rejected —
        // placement aborts rather than panicking.
        placed.id = loop {
            let id = self.next_node_id;
            self.next_node_id = self.next_node_id.wrapping_add(1).max(1);
            match region.kinetic.add_node(id, input) {
                Ok(()) => break id,
                Err(GraphError::DuplicateNode(_)) => continue,
                Err(_) => return Vec::new(),
            }
        };
        self.kinetics.insert(pos, placed);

        // Connect compatible adjacent nodes within the same region.
        let candidates = self.candidate_links(pos, placed);
        for (neighbor_pos, ratio) in candidates {
            if neighbor_pos.region_key() != region_key {
                continue;
            }
            if let Some(&neighbor) = self.kinetics.get(&neighbor_pos) {
                let edge_key = canonical_pair(pos, neighbor_pos);
                if self.kinetic_edges.contains_key(&edge_key) {
                    continue;
                }
                let reg = self.scheduler.region_mut(region_key);
                if reg.kinetic.connect(placed.id, neighbor.id, ratio).is_ok() {
                    self.kinetic_edges.insert(edge_key, ratio);
                }
            }
        }

        self.reconcile_stress_and_diffs(region_key)
    }

    /// Remove a placed Create block and re-solve affected components.
    pub fn remove_block(&mut self, pos: DimBlockPos) -> Vec<KineticUpdateTag> {
        let Some(removed) = self.kinetics.remove(&pos) else {
            return Vec::new();
        };
        self.update_gate.remove(&pos);
        self.update_batch.remove(&pos);
        self.kinetic_edges.retain(|&(a, b), _| a != pos && b != pos);
        let region_key = pos.region_key();
        if let Some(region) = self.scheduler.region_mut_if_present(region_key) {
            let _ = region.kinetic.remove_node(removed.id);
        }
        self.reconcile_stress_and_diffs(region_key)
    }

    /// Cycle a block's `axis` (Create wrench action) and update topology.
    pub fn wrench_rotate(&mut self, pos: DimBlockPos) -> Option<(u32, Vec<KineticUpdateTag>)> {
        let current_state = self.kinetics.get(&pos)?.state_id;
        let next_state = self.registry.cycle_property(current_state, "axis")?;
        let diffs = self.place_block(pos, next_state);
        Some((next_state, diffs))
    }

    /// Set a block's redstone `powered` property (Clutch / Gearshift) and re-solve.
    pub fn set_powered(
        &mut self,
        pos: DimBlockPos,
        powered: bool,
    ) -> Option<(u32, Vec<KineticUpdateTag>)> {
        let current_state = self.kinetics.get(&pos)?.state_id;
        let next_state = self.registry.with_property(
            current_state,
            "powered",
            if powered { "true" } else { "false" },
        )?;
        let diffs = self.place_block(pos, next_state);
        Some((next_state, diffs))
    }

    /// Register a sleeping belt/chute transport node in the region of `pos`.
    pub fn add_belt_segment(
        &mut self,
        pos: DimBlockPos,
        id: u32,
        capacity: u32,
        delay_ticks: u64,
    ) -> Result<(), TransportError> {
        let key = pos.region_key();
        self.active_regions.insert(key);
        self.scheduler
            .region_mut(key)
            .add_machine(id, capacity, delay_ticks)
    }

    pub fn link_belt(
        &mut self,
        pos: DimBlockPos,
        from: u32,
        to: Option<u32>,
    ) -> Result<(), TransportError> {
        self.scheduler
            .region_mut(pos.region_key())
            .set_downstream(from, to)
    }

    pub fn insert_belt_item(
        &mut self,
        pos: DimBlockPos,
        id: u32,
        item_id: u32,
    ) -> Result<(), TransportError> {
        self.scheduler
            .region_mut(pos.region_key())
            .insert_item(id, item_id)
    }

    /// Register a mechanical processor (Press / Mixer / Millstone / Spout) tied
    /// to the solved kinetic speed at `pos`.
    pub fn add_processor(
        &mut self,
        pos: DimBlockPos,
        machine_id: u32,
        input_capacity: u32,
        output_capacity: u32,
        recipes: Vec<Recipe>,
    ) -> Result<(), RecipeError> {
        let key = pos.region_key();
        self.active_regions.insert(key);
        let drive = self.drive_for_pos(pos);
        self.scheduler.region_mut(key).add_processor(
            machine_id,
            input_capacity,
            output_capacity,
            recipes,
            drive,
        )?;
        self.processors_by_pos.insert(pos, machine_id);
        Ok(())
    }

    pub fn insert_processor_ingredient(
        &mut self,
        pos: DimBlockPos,
        machine_id: u32,
        stack: Stack,
    ) -> Result<u32, RecipeError> {
        self.scheduler
            .region_mut(pos.region_key())
            .insert_ingredient(machine_id, stack)
    }

    pub fn extract_processor_product(
        &mut self,
        pos: DimBlockPos,
        machine_id: u32,
        stack: Stack,
    ) -> Result<u32, RecipeError> {
        self.scheduler
            .region_mut(pos.region_key())
            .extract_product(machine_id, stack)
    }

    /// Register a fluid tank in the region of `pos`.
    pub fn add_fluid_tank(
        &mut self,
        pos: DimBlockPos,
        tank_id: u32,
        capacity_mb: u32,
    ) -> Result<(), FluidError> {
        let key = pos.region_key();
        self.active_regions.insert(key);
        self.scheduler
            .region_mut(key)
            .add_tank(tank_id, capacity_mb, pos.y)
    }

    pub fn link_fluid_tanks(
        &mut self,
        pos: DimBlockPos,
        a: u32,
        b: u32,
        pump_head: Option<i32>,
    ) -> Result<(), FluidError> {
        self.scheduler
            .region_mut(pos.region_key())
            .set_fluid_link(a, b, pump_head)
    }

    pub fn insert_fluid(
        &mut self,
        pos: DimBlockPos,
        tank_id: u32,
        fluid_id: u32,
        amount_mb: u32,
    ) -> Result<u32, FluidError> {
        self.scheduler
            .region_mut(pos.region_key())
            .insert_fluid(tank_id, fluid_id, amount_mb)
    }

    /// Advance all active regions to `tick` under per-region work and time
    /// budgets, exporting S5 delta frames for regions that produced output.
    pub fn step_tick(
        &mut self,
        tick: u64,
        max_steps_per_region: usize,
        max_us_per_region: Option<u64>,
    ) -> Result<Vec<RegionStepOutput>, TransportError> {
        let due_count = self
            .scheduler
            .runnable_region_count_for(&self.active_regions, tick);
        let workers = if due_count >= 2 { 4 } else { 1 };
        self.step_tick_parallel(tick, max_steps_per_region, max_us_per_region, workers)
    }

    /// Advance factory regions using deterministic per-region parallelism.
    /// Worker counts are capped at four; region stepping and output merging
    /// remain in sorted `(dimension, rx, rz)` order.
    pub fn step_tick_parallel(
        &mut self,
        tick: u64,
        max_steps_per_region: usize,
        max_us_per_region: Option<u64>,
        max_workers: usize,
    ) -> Result<Vec<RegionStepOutput>, TransportError> {
        let budget = RegionBudget {
            max_steps: max_steps_per_region,
            max_time: max_us_per_region.map(Duration::from_micros),
        };
        let mut outputs = Vec::new();
        let stepped = self.scheduler.advance_active_parallel_for(
            &self.active_regions,
            tick,
            budget,
            max_workers.clamp(1, 4),
        );
        self.idle_export_skips += self.active_regions.len().saturating_sub(stepped.len()) as u64;
        self.active_regions_last_tick = stepped.len();
        for (key, result) in stepped {
            let report = result?;
            let region = self.scheduler.region_mut(key);
            // `advance_to` pushes one batch record per processed work unit
            // and `output_blocked` only fires when the queue was full, so
            // `processed == 0` with no pending residue means the export
            // queue is provably empty: `export_into` would return an empty
            // `BatchReport` anyway, and skipping it is byte-identical.
            let may_hold_records = report.processed > 0
                || report.output_blocked
                || self.pending_exports.contains(&key);
            let batch = if may_hold_records {
                let batch = region
                    .export_into(&mut self.export_scratch, MAX_BATCH_RECORDS)
                    .expect("scratch buffer matches MAX_BATCH_BYTES");
                if batch.records > 0 {
                    self.pending_exports.insert(key);
                } else {
                    self.pending_exports.remove(&key);
                }
                batch
            } else {
                self.idle_export_skips += 1;
                BatchReport {
                    bytes: 0,
                    records: 0,
                    sequence: None,
                }
            };
            if report.processed > 0 || batch.records > 0 {
                outputs.push(RegionStepOutput {
                    region: key,
                    report,
                    batch,
                    s5_frame: self.export_scratch[..batch.bytes].to_vec(),
                });
            }
            self.scheduler.refresh_region_sleep(key);
        }
        Ok(outputs)
    }

    fn drive_for_pos(&self, pos: DimBlockPos) -> Drive {
        match self.kinetic_solution(pos) {
            Some(sol) if sol.status == NetworkStatus::Running => Drive::Mechanical {
                rpm: sol.speed.abs().round() as u32,
                overstressed: false,
            },
            Some(sol) if sol.status == NetworkStatus::Overstressed => Drive::Mechanical {
                rpm: 0,
                overstressed: true,
            },
            _ => Drive::Mechanical {
                rpm: 0,
                overstressed: false,
            },
        }
    }

    fn candidate_links(&self, pos: DimBlockPos, placed: PlacedKinetic) -> Vec<(DimBlockPos, f64)> {
        let mut out = Vec::new();

        // 1. Axial shaft connections along `placed.axis`.
        let allows_axial = !matches!(placed.role, CreateBlockRole::Clutch if placed.powered);
        if allows_axial {
            let ratio = if matches!(placed.role, CreateBlockRole::Gearshift if placed.powered) {
                -1.0
            } else {
                1.0
            };
            for (dx, dy, dz) in placed.axis.unit_deltas() {
                let npos = pos.offset(dx, dy, dz);
                if let Some(&neighbor) = self.kinetics.get(&npos) {
                    if neighbor.axis == placed.axis
                        && !matches!(neighbor.role, CreateBlockRole::Clutch if neighbor.powered)
                    {
                        let effective = if matches!(neighbor.role, CreateBlockRole::Gearshift if neighbor.powered)
                        {
                            -ratio
                        } else {
                            ratio
                        };
                        out.push((npos, effective));
                    }
                }
            }
        }

        // 2. Cogwheel <-> Cogwheel (same axis, perpendicular neighbor -> -1:1)
        //    and Large Cogwheel <-> Large Cogwheel (-1:1).
        if matches!(
            placed.role,
            CreateBlockRole::Cogwheel | CreateBlockRole::LargeCogwheel
        ) {
            for (dx, dy, dz) in placed.axis.perpendicular_deltas() {
                let npos = pos.offset(dx, dy, dz);
                if let Some(&neighbor) = self.kinetics.get(&npos) {
                    if neighbor.axis == placed.axis && neighbor.role == placed.role {
                        out.push((npos, -1.0));
                    }
                }
            }
            // 3. Large Cogwheel <-> Small Cogwheel diagonal meshing (2:1 / 0.5:1).
            for (dx, dy, dz) in placed.axis.diagonal_deltas() {
                let npos = pos.offset(dx, dy, dz);
                if let Some(&neighbor) = self.kinetics.get(&npos) {
                    if neighbor.axis == placed.axis {
                        match (placed.role, neighbor.role) {
                            (CreateBlockRole::LargeCogwheel, CreateBlockRole::Cogwheel) => {
                                out.push((npos, -2.0));
                            }
                            (CreateBlockRole::Cogwheel, CreateBlockRole::LargeCogwheel) => {
                                out.push((npos, -0.5));
                            }
                            _ => {}
                        }
                    }
                }
            }
        }

        // 4. Gearbox: links perpendicular neighbors whose axis points into the gearbox.
        if placed.role == CreateBlockRole::Gearbox {
            for (dx, dy, dz) in placed.axis.perpendicular_deltas() {
                let npos = pos.offset(dx, dy, dz);
                let dir_axis = if dx != 0 {
                    Axis::X
                } else if dy != 0 {
                    Axis::Y
                } else {
                    Axis::Z
                };
                if let Some(&neighbor) = self.kinetics.get(&npos) {
                    if neighbor.axis == dir_axis
                        && !matches!(neighbor.role, CreateBlockRole::Clutch if neighbor.powered)
                    {
                        out.push((npos, -1.0));
                    }
                }
            }
        } else {
            // Non-gearbox block placed next to an existing gearbox along its axis.
            for (dx, dy, dz) in placed.axis.unit_deltas() {
                let npos = pos.offset(dx, dy, dz);
                if let Some(&neighbor) = self.kinetics.get(&npos) {
                    if neighbor.role == CreateBlockRole::Gearbox && neighbor.axis != placed.axis {
                        out.push((npos, -1.0));
                    }
                }
            }
        }

        out
    }

    /// Reconcile RPM-scaled Create stress loads (`|RPM| * stress_impact`) and
    /// return UpdateTag diffs for every node in `region_key` whose solved state
    /// changed.
    fn reconcile_stress_and_diffs(&mut self, region_key: RegionKey) -> Vec<KineticUpdateTag> {
        // `mem::take` the scratch vecs so the collection borrows don't
        // overlap the `self` field borrows below; both go back at the end
        // with their capacity intact for the next reconcile.
        let mut region_nodes = std::mem::take(&mut self.reconcile_nodes);
        region_nodes.clear_bounded(RECONCILE_SCRATCH_RETAIN);
        region_nodes.extend(
            self.kinetics
                .iter()
                .filter(|(p, _)| p.region_key() == region_key)
                .map(|(&p, &k)| (p, k)),
        );
        region_nodes.sort_by_key(|(p, _)| *p);

        let mut nominal_rpms = std::mem::take(&mut self.reconcile_rpms);
        nominal_rpms.clear_bounded(RECONCILE_SCRATCH_RETAIN);

        if let Some(region) = self.scheduler.region_mut_if_present(region_key) {
            // Pass 1: clear consumer loads so KineticGraph computes exact
            // kinematic RPMs even when a component was previously overstressed.
            for &(_, placed) in region_nodes.iter() {
                if !matches!(placed.role, CreateBlockRole::Generator { .. }) {
                    let _ = region
                        .kinetic
                        .update_node(placed.id, kinetic_node_input(placed, 0.0));
                }
            }
            // Snapshot nominal kinematic RPMs before applying RPM-scaled loads.
            nominal_rpms.extend(region_nodes.iter().map(|&(_, placed)| {
                let rpm = region
                    .kinetic
                    .solution(placed.id)
                    .map(|s| s.speed.abs())
                    .unwrap_or(0.0);
                (placed, rpm)
            }));
            // Pass 2: apply exact `stress_impact * |nominal_rpm|` to consumers
            // so Running vs Overstressed is deterministic and oscillation-free.
            for &(placed, rpm) in nominal_rpms.iter() {
                if !matches!(placed.role, CreateBlockRole::Generator { .. }) {
                    let _ = region
                        .kinetic
                        .update_node(placed.id, kinetic_node_input(placed, rpm));
                }
            }
        }

        let mut diffs = Vec::new();
        for &(pos, placed) in region_nodes.iter() {
            let Some(sol) = self
                .scheduler
                .region(region_key)
                .and_then(|r| r.kinetic.solution(placed.id))
            else {
                continue;
            };
            // Sync any co-located mechanical processor drive to the solved RPM.
            if let Some(&machine_id) = self.processors_by_pos.get(&pos) {
                let drive = match sol.status {
                    NetworkStatus::Running => Drive::Mechanical {
                        rpm: sol.speed.abs().round() as u32,
                        overstressed: false,
                    },
                    NetworkStatus::Overstressed => Drive::Mechanical {
                        rpm: 0,
                        overstressed: true,
                    },
                    _ => Drive::Mechanical {
                        rpm: 0,
                        overstressed: false,
                    },
                };
                let _ = self
                    .scheduler
                    .region_mut(region_key)
                    .set_processor_drive(machine_id, drive);
            }

            // Fingerprint gate: a re-solved state identical to the last
            // publish is skipped in O(1); a changed one commits and emits.
            // `NaN != NaN` under `PartialEq`, so a NaN solution always
            // diffs in the reference semantics — invalidate instead of
            // committing to keep that exactly.
            let changed = if sol.capacity.is_nan() || sol.load.is_nan() || sol.speed.is_nan() {
                self.update_gate.invalidate(&pos);
                true
            } else {
                self.update_gate.observe(pos, solution_fingerprint(&sol))
            };
            if changed {
                let tag = KineticUpdateTag {
                    pos,
                    speed_rpm: sol.speed,
                    stress_capacity: sol.capacity,
                    stress_load: sol.load,
                    overstressed: sol.status == NetworkStatus::Overstressed,
                };
                self.update_batch.record(pos, tag);
                diffs.push(tag);
            }
        }
        self.reconcile_nodes = region_nodes;
        self.reconcile_rpms = nominal_rpms;
        diffs
    }
}

trait RegionSchedulerExt {
    fn region_mut_if_present(
        &mut self,
        key: RegionKey,
    ) -> Option<&mut mod_offload::region_scheduler::Region>;
}

impl RegionSchedulerExt for RegionScheduler {
    fn region_mut_if_present(
        &mut self,
        key: RegionKey,
    ) -> Option<&mut mod_offload::region_scheduler::Region> {
        if self.region(key).is_some() {
            Some(self.region_mut(key))
        } else {
            None
        }
    }
}

fn kinetic_node_input(placed: PlacedKinetic, rpm_scale: f64) -> KineticNode {
    match placed.role {
        CreateBlockRole::Generator { driven_rpm } => {
            let speed = f64::from(driven_rpm);
            KineticNode {
                capacity: placed.stress_capacity * speed.abs(),
                load: 0.0,
                driven_speed: Some(speed),
            }
        }
        _ => KineticNode {
            capacity: 0.0,
            load: placed.stress_impact * rpm_scale,
            driven_speed: None,
        },
    }
}

/// 64-bit fingerprint of a solved kinetic state — component id, stress
/// numbers, speed, and status discriminant.
///
/// The encoding mirrors `Solution`'s `PartialEq` so the gate's skip set is
/// exactly the reference `!=` set (modulo the negligible 64-bit collision
/// bound): `0.0` and `-0.0` fold to the same bytes because `==` says
/// they're equal, and every NaN maps to one canonical value because NaN
/// never reaches the gate — callers invalidate instead of committing.
fn solution_fingerprint(sol: &Solution) -> u64 {
    fn push_f64(f: &mut FingerprintBuilder, v: f64) {
        if v == 0.0 {
            // `-0.0 == 0.0` under `PartialEq`, so both fold to one slot.
            f.write_u64(0);
        } else if v.is_nan() {
            f.write_u64(u64::MAX);
        } else {
            f.write_u64(v.to_bits());
        }
    }
    let mut f = FingerprintBuilder::new();
    f.write_u64(u64::from(sol.component));
    push_f64(&mut f, sol.capacity);
    push_f64(&mut f, sol.load);
    push_f64(&mut f, sol.speed);
    f.write_u8(match sol.status {
        NetworkStatus::Unpowered => 0,
        NetworkStatus::Running => 1,
        NetworkStatus::Overstressed => 2,
        NetworkStatus::Conflict => 3,
    });
    f.finish()
}

fn canonical_pair(a: DimBlockPos, b: DimBlockPos) -> (DimBlockPos, DimBlockPos) {
    if a <= b {
        (a, b)
    } else {
        (b, a)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::blockstates::{core_create_registry, DEFAULT_BASE_STATE_ID};
    use crate::data_loader::{parse_create_recipe_json, ItemFluidIdTable};
    use mod_offload::recipe_processor::{Resource, ResourceKind};

    #[test]
    fn idle_factory_ticks_do_not_export_and_active_ticks_are_bounded() {
        let mut factory = CreateFactory::new(core_create_registry(DEFAULT_BASE_STATE_ID));
        for tick in 1..=32 {
            assert!(factory.step_tick(tick, 4, None).unwrap().is_empty());
        }

        let pos = DimBlockPos::new(0, 0, 64, 0);
        factory.add_belt_segment(pos, 1, 4, 1).unwrap();
        factory.add_belt_segment(pos, 2, 4, 1).unwrap();
        factory.link_belt(pos, 1, Some(2)).unwrap();
        factory.insert_belt_item(pos, 1, 42).unwrap();

        let mut produced = 0;
        for tick in 33..=40 {
            for output in factory.step_tick(tick, 4, None).unwrap() {
                assert_eq!(output.region, pos.region_key());
                assert!(output.report.processed <= 4);
                assert_eq!(output.s5_frame.len(), output.batch.bytes);
                produced += output.batch.records;
            }
        }
        assert!(produced > 0);
        for tick in 41..=48 {
            assert!(factory.step_tick(tick, 4, None).unwrap().is_empty());
        }
    }

    #[test]
    fn one_item_wakes_only_its_region_after_one_hundred_regions_sleep() {
        let mut factory = CreateFactory::new(core_create_registry(DEFAULT_BASE_STATE_ID));
        let mut target = None;
        for index in 0..100 {
            let rx = index % 10;
            let rz = index / 10;
            let pos = DimBlockPos::new(0, rx * 32, 64, rz * 32);
            factory.add_belt_segment(pos, 1, 4, 1).unwrap();
            factory.add_belt_segment(pos, 2, 4, 1).unwrap();
            factory.link_belt(pos, 1, Some(2)).unwrap();
            if rx == 5 && rz == 7 {
                target = Some(pos);
            }
        }
        let target = target.expect("target region is among the 100 belt regions");

        assert!(factory.step_tick(1, 16, None).unwrap().is_empty());
        assert_eq!(factory.active_regions_last_tick(), 100);
        assert_eq!(factory.sleeping_regions(), 100);
        assert!(factory.step_tick(2, 16, None).unwrap().is_empty());
        assert_eq!(factory.active_regions_last_tick(), 0);

        factory.insert_belt_item(target, 1, 0x51ee_0001).unwrap();
        let outputs = factory.step_tick(3, 16, None).unwrap();
        assert_eq!(factory.active_regions_last_tick(), 1);
        assert!(outputs
            .iter()
            .all(|output| output.region == target.region_key()));
        assert_eq!(factory.sleeping_regions(), 100);
        let outputs = factory.step_tick(4, 16, None).unwrap();
        assert_eq!(factory.active_regions_last_tick(), 1);
        assert!(outputs
            .iter()
            .all(|output| output.region == target.region_key()));
        assert_eq!(
            factory
                .scheduler_mut()
                .region_mut(target.region_key())
                .extract_output(2),
            Ok(Some(0x51ee_0001))
        );
    }

    #[test]
    fn sixteen_region_serial_and_parallel_runs_are_bit_identical() {
        fn build_world() -> (CreateFactory, Vec<(RegionKey, DimBlockPos, DimBlockPos)>) {
            let registry = core_create_registry(DEFAULT_BASE_STATE_ID);
            let wheel = registry
                .with_property(
                    registry.default_state("create:water_wheel").unwrap(),
                    "axis",
                    "z",
                )
                .unwrap();
            let press = registry
                .with_property(
                    registry.default_state("create:mechanical_press").unwrap(),
                    "facing",
                    "north",
                )
                .unwrap();
            let gearbox = registry
                .with_property(
                    registry.default_state("create:gearbox").unwrap(),
                    "axis",
                    "x",
                )
                .unwrap();
            let mixer = registry.default_state("create:mechanical_mixer").unwrap();
            let mut factory = CreateFactory::new(registry);
            let input_a = Resource {
                kind: ResourceKind::Item,
                id: 11,
            };
            let output_a = Resource {
                kind: ResourceKind::Item,
                id: 12,
            };
            let input_b = Resource {
                kind: ResourceKind::Item,
                id: 13,
            };
            let output_b = Resource {
                kind: ResourceKind::Item,
                id: 14,
            };
            let recipe_a = Recipe {
                id: 100,
                base_ticks: 20,
                inputs: vec![Stack {
                    resource: input_a,
                    amount: 1,
                }],
                outputs: vec![Stack {
                    resource: output_a,
                    amount: 1,
                }],
            };
            let recipe_b = Recipe {
                id: 101,
                base_ticks: 24,
                inputs: vec![Stack {
                    resource: input_b,
                    amount: 1,
                }],
                outputs: vec![Stack {
                    resource: output_b,
                    amount: 1,
                }],
            };
            let mut regions = Vec::new();
            for index in 0..16 {
                let rx = index % 4;
                let rz = index / 4;
                let base = DimBlockPos::new(0, rx * 32, 64, rz * 32);
                let press_pos = base.offset(0, 0, 2);
                let mixer_pos = base.offset(0, 1, 0);
                let wheel_pos = base.offset(0, 0, 1);
                factory.place_block(base, gearbox);
                factory.place_block(wheel_pos, wheel);
                factory.place_block(press_pos, press);
                factory.place_block(mixer_pos, mixer);
                factory
                    .add_processor(press_pos, 20, 8, 8, vec![recipe_a.clone()])
                    .unwrap();
                factory
                    .add_processor(mixer_pos, 21, 8, 8, vec![recipe_b.clone()])
                    .unwrap();
                factory
                    .insert_processor_ingredient(
                        press_pos,
                        20,
                        Stack {
                            resource: input_a,
                            amount: 1,
                        },
                    )
                    .unwrap();
                factory
                    .insert_processor_ingredient(
                        mixer_pos,
                        21,
                        Stack {
                            resource: input_b,
                            amount: 1,
                        },
                    )
                    .unwrap();
                factory.add_belt_segment(base, 1, 4, 2).unwrap();
                factory.add_belt_segment(base, 2, 4, 2).unwrap();
                factory.link_belt(base, 1, Some(2)).unwrap();
                factory
                    .insert_belt_item(base, 1, 0x7000 + index as u32)
                    .unwrap();
                let tank_a = base.offset(3, 0, 0);
                let tank_b = base.offset(4, 0, 0);
                factory.add_fluid_tank(tank_a, 30, 8000).unwrap();
                factory.add_fluid_tank(tank_b, 31, 8000).unwrap();
                factory.link_fluid_tanks(tank_a, 30, 31, Some(0)).unwrap();
                factory.insert_fluid(tank_a, 30, 50, 4000).unwrap();
                regions.push((base.region_key(), press_pos, mixer_pos));
            }
            (factory, regions)
        }

        let (mut serial, regions) = build_world();
        let (mut parallel, parallel_regions) = build_world();
        assert_eq!(regions, parallel_regions);
        let mut serial_trace = Vec::new();
        let mut parallel_trace = Vec::new();
        for tick in 1..=200 {
            for (factory, trace, workers) in [
                (&mut serial, &mut serial_trace, 1),
                (&mut parallel, &mut parallel_trace, 4),
            ] {
                for output in factory
                    .step_tick_parallel(tick, 1024, None, workers)
                    .unwrap()
                {
                    trace.push((output.region, output.report, output.batch, output.s5_frame));
                }
            }
        }
        assert_eq!(serial_trace, parallel_trace);

        for ((region, press_pos, mixer_pos), (_, parallel_press_pos, parallel_mixer_pos)) in
            regions.iter().zip(&parallel_regions)
        {
            let left = serial.scheduler().region(*region).unwrap();
            let right = parallel.scheduler().region(*region).unwrap();
            let left_solution = serial.kinetic_solution(*press_pos).unwrap();
            let right_solution = parallel.kinetic_solution(*parallel_press_pos).unwrap();
            assert_eq!(left_solution.component, right_solution.component);
            assert_eq!(
                left_solution.capacity.to_bits(),
                right_solution.capacity.to_bits()
            );
            assert_eq!(left_solution.load.to_bits(), right_solution.load.to_bits());
            assert_eq!(
                left_solution.speed.to_bits(),
                right_solution.speed.to_bits()
            );
            assert_eq!(left_solution.status, right_solution.status);
            let left_mixer = serial.kinetic_solution(*mixer_pos).unwrap();
            let right_mixer = parallel.kinetic_solution(*parallel_mixer_pos).unwrap();
            assert_eq!(left_mixer.speed.to_bits(), right_mixer.speed.to_bits());
            assert_eq!(left_mixer.status, right_mixer.status);
            assert_eq!(left.fluid().tank(30), right.fluid().tank(30));
            assert_eq!(left.fluid().tank(31), right.fluid().tank(31));
        }

        for (_, press_pos, mixer_pos) in &regions {
            for factory in [&mut serial, &mut parallel] {
                for (pos, machine_id, resource_id) in [(*press_pos, 20, 12), (*mixer_pos, 21, 14)] {
                    assert_eq!(
                        factory
                            .extract_processor_product(
                                pos,
                                machine_id,
                                Stack {
                                    resource: Resource {
                                        kind: ResourceKind::Item,
                                        id: resource_id,
                                    },
                                    amount: 1,
                                },
                            )
                            .unwrap(),
                        1
                    );
                }
            }
        }
        for ((region, _, _), (parallel_region, _, _)) in regions.iter().zip(&parallel_regions) {
            assert_eq!(region, parallel_region);
            for factory in [&mut serial, &mut parallel] {
                assert_eq!(
                    factory
                        .scheduler_mut()
                        .region_mut(*region)
                        .extract_output(2),
                    Ok(Some(0x7000 + region.x as u32 + 4 * region.z as u32))
                );
            }
        }
    }

    #[test]
    fn cogwheel_gearing_clutch_gearshift_overstress_and_recovery() {
        let reg = core_create_registry(DEFAULT_BASE_STATE_ID);
        let water_wheel_z = reg
            .with_property(
                reg.default_state("create:water_wheel").unwrap(),
                "axis",
                "z",
            )
            .unwrap();
        let large_cog_z = reg
            .with_property(
                reg.default_state("create:large_cogwheel").unwrap(),
                "axis",
                "z",
            )
            .unwrap();
        let small_cog_z = reg
            .with_property(reg.default_state("create:cogwheel").unwrap(), "axis", "z")
            .unwrap();
        let clutch_z = reg
            .with_property(reg.default_state("create:clutch").unwrap(), "axis", "z")
            .unwrap();
        let gearshift_z = reg
            .with_property(reg.default_state("create:gearshift").unwrap(), "axis", "z")
            .unwrap();
        let press = reg.default_state("create:mechanical_press").unwrap();

        let mut factory = CreateFactory::new(reg);
        // Water wheel (8 RPM, 256 SU) -> Large Cog -> diagonal Small Cog (-16 RPM) -> Clutch -> Gearshift
        let p_wheel = DimBlockPos::new(0, 0, 64, 0);
        let p_large = DimBlockPos::new(0, 0, 64, 1);
        let p_small = DimBlockPos::new(0, 1, 65, 1);
        let p_clutch = DimBlockPos::new(0, 1, 65, 2);
        let p_shift = DimBlockPos::new(0, 1, 65, 3);

        let diffs = factory.place_block(p_wheel, water_wheel_z);
        assert_eq!(diffs.len(), 1);
        assert_eq!(diffs[0].speed_rpm, 8.0);
        assert_eq!(diffs[0].stress_capacity, 256.0);

        factory.place_block(p_large, large_cog_z);
        factory.place_block(p_small, small_cog_z);
        factory.place_block(p_clutch, clutch_z);
        factory.place_block(p_shift, gearshift_z);

        let sol_small = factory.kinetic_solution(p_small).unwrap();
        assert_eq!(sol_small.status, NetworkStatus::Running);
        assert!((sol_small.speed - (-16.0)).abs() < 1e-9);

        // Engage gearshift -> downstream speed flips to +16 RPM.
        let (_, _) = factory.set_powered(p_shift, true).unwrap();
        let sol_shift = factory.kinetic_solution(p_shift).unwrap();
        assert!((sol_shift.speed - 16.0).abs() < 1e-9);

        // Engage clutch -> gearshift disconnects and becomes unpowered (0 RPM).
        let (_, clutch_diffs) = factory.set_powered(p_clutch, true).unwrap();
        assert!(!clutch_diffs.is_empty());
        assert_eq!(
            factory.kinetic_solution(p_shift).unwrap().status,
            NetworkStatus::Unpowered
        );

        // Disengage clutch -> power returns.
        factory.set_powered(p_clutch, false).unwrap();
        assert_eq!(
            factory.kinetic_solution(p_shift).unwrap().status,
            NetworkStatus::Running
        );

        // Add 3 mechanical presses (each 8 SU/RPM * 16 RPM = 128 SU -> 384 SU > 256 SU) -> OVERSTRESSED!
        let p_press1 = DimBlockPos::new(0, 1, 65, 4);
        let p_press2 = DimBlockPos::new(0, 1, 65, 5);
        let p_press3 = DimBlockPos::new(0, 1, 65, 6);
        let press_z = factory
            .registry()
            .with_property(press, "facing", "north")
            .unwrap();
        factory.place_block(p_press1, press_z);
        factory.place_block(p_press2, press_z);
        assert_eq!(
            factory.kinetic_solution(p_press2).unwrap().status,
            NetworkStatus::Running
        );
        factory.place_block(p_press3, press_z);
        assert_eq!(
            factory.kinetic_solution(p_press3).unwrap().status,
            NetworkStatus::Overstressed
        );

        // Remove 1 press -> network recovers cleanly!
        factory.remove_block(p_press3);
        assert_eq!(
            factory.kinetic_solution(p_press2).unwrap().status,
            NetworkStatus::Running
        );
    }

    #[test]
    fn wrench_rotation_disconnects_and_reconnects_shafts() {
        let reg = core_create_registry(DEFAULT_BASE_STATE_ID);
        let wheel_x = reg
            .with_property(
                reg.default_state("create:water_wheel").unwrap(),
                "axis",
                "x",
            )
            .unwrap();
        let shaft_x = reg
            .with_property(reg.default_state("create:shaft").unwrap(), "axis", "x")
            .unwrap();

        let mut factory = CreateFactory::new(reg);
        let p0 = DimBlockPos::new(0, 0, 64, 0);
        let p1 = DimBlockPos::new(0, 1, 64, 0);
        factory.place_block(p0, wheel_x);
        factory.place_block(p1, shaft_x);
        assert_eq!(
            factory.kinetic_solution(p1).unwrap().status,
            NetworkStatus::Running
        );

        // Wrench shaft from X -> Y -> disconnects from X-axis water wheel.
        factory.wrench_rotate(p1).unwrap();
        assert_eq!(
            factory.kinetic_solution(p1).unwrap().status,
            NetworkStatus::Unpowered
        );
        // Wrench Y -> Z -> still disconnected.
        factory.wrench_rotate(p1).unwrap();
        assert_eq!(
            factory.kinetic_solution(p1).unwrap().status,
            NetworkStatus::Unpowered
        );
        // Wrench Z -> X -> reconnects!
        factory.wrench_rotate(p1).unwrap();
        assert_eq!(
            factory.kinetic_solution(p1).unwrap().status,
            NetworkStatus::Running
        );
    }

    #[test]
    fn end_to_end_recipes_belts_fluids_and_custom_dimension_isolation() {
        let reg = core_create_registry(DEFAULT_BASE_STATE_ID);
        let wheel = reg
            .with_property(
                reg.default_state("create:windmill_bearing").unwrap(),
                "facing",
                "south",
            )
            .unwrap();
        let press = reg
            .with_property(
                reg.default_state("create:mechanical_press").unwrap(),
                "facing",
                "north",
            )
            .unwrap();
        let mut factory = CreateFactory::new(reg);

        // Parse a real Create pressing recipe JSON.
        let mut ids = ItemFluidIdTable::new();
        let parsed = parse_create_recipe_json(
            "create:pressing/iron_sheet",
            1,
            br#"{"type":"create:pressing","ingredients":[{"item":"minecraft:iron_ingot"}],"results":[{"id":"create:iron_sheet","count":1}],"processing_time":10}"#,
            &mut ids,
        )
        .unwrap()
        .unwrap();
        let ingot = ids.item_id("minecraft:iron_ingot").unwrap();
        let sheet = ids.item_id("create:iron_sheet").unwrap();
        let water = ids.intern_fluid("minecraft:water");

        // Build factory in Overworld (dim = 0) AND custom dimension (dim = 3)!
        for dim in [0, 3] {
            let p_gen = DimBlockPos::new(dim, 0, 64, 0);
            let p_press = DimBlockPos::new(dim, 0, 64, 1);
            factory.place_block(p_gen, wheel);
            factory.place_block(p_press, press);
            assert_eq!(
                factory.kinetic_solution(p_press).unwrap().status,
                NetworkStatus::Running
            );
            factory
                .add_processor(p_press, 100, 64, 64, vec![parsed.recipe.clone()])
                .unwrap();
            factory
                .insert_processor_ingredient(
                    p_press,
                    100,
                    Stack {
                        resource: Resource {
                            kind: ResourceKind::Item,
                            id: ingot,
                        },
                        amount: 1,
                    },
                )
                .unwrap();
            factory.add_belt_segment(p_gen, 1, 4, 2).unwrap();
            factory.add_belt_segment(p_gen, 2, 4, 2).unwrap();
            factory.link_belt(p_gen, 1, Some(2)).unwrap();
            factory.insert_belt_item(p_gen, 1, ingot).unwrap();

            let t1 = DimBlockPos::new(dim, 2, 64, 0);
            let t2 = DimBlockPos::new(dim, 3, 64, 0);
            factory.add_fluid_tank(t1, 10, 8000).unwrap();
            factory.add_fluid_tank(t2, 11, 8000).unwrap();
            factory.link_fluid_tanks(t1, 10, 11, Some(0)).unwrap();
            factory.insert_fluid(t1, 10, water, 4000).unwrap();
        }

        // Advance 12 ticks: both dimensions independently complete fluid balance,
        // belt transfer, and pressing recipe!
        let mut total_records = 0usize;
        for tick in 1..=12 {
            let outs = factory.step_tick(tick, 1024, None).unwrap();
            for o in outs {
                total_records += o.batch.records;
            }
        }
        assert!(total_records >= 8);

        for dim in [0, 3] {
            let p_press = DimBlockPos::new(dim, 0, 64, 1);
            let got = factory
                .extract_processor_product(
                    p_press,
                    100,
                    Stack {
                        resource: Resource {
                            kind: ResourceKind::Item,
                            id: sheet,
                        },
                        amount: 1,
                    },
                )
                .unwrap();
            assert_eq!(got, 1);

            let region = factory.scheduler().region(p_press.region_key()).unwrap();
            assert_eq!(region.fluid().tank(10).unwrap().amount, 2000);
            assert_eq!(region.fluid().tank(11).unwrap().amount, 2000);
        }
    }
}
