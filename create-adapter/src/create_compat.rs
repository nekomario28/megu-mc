// SPDX-License-Identifier: LGPL-3.0-or-later

//! Create 6.0.x machine-behaviour compatibility kernels.
//!
//! These small deterministic models cover the factory semantics that are
//! independent of chunk/region scheduling: direction-sensitive gearbox
//! transfer, RPM-scaled pumps and processors, filtered funnel handoff, and
//! mechanical-arm target reservation. They intentionally use integer
//! arithmetic only; tests pin conservation and ordering rather than
//! implementation-internal timestamps.

use std::collections::BTreeMap;
use std::collections::BTreeSet;

/// Create 6.0.x's default symmetric rotation-speed ceiling, in whole RPM.
pub const CREATE_MAX_RPM: i64 = 256;

// ---------------------------------------------------------------------------
// Direction-sensitive gearbox transfer
// ---------------------------------------------------------------------------

/// A world direction used for shaft/gearbox faces. Sign matters: Create's
/// bevel-gear transfer is not a universal `-1` on every gearbox port.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CreateFace {
    East,
    West,
    Up,
    Down,
    South,
    North,
}

impl CreateFace {
    fn axis(self) -> u8 {
        match self {
            Self::East | Self::West => 0,
            Self::Up | Self::Down => 1,
            Self::South | Self::North => 2,
        }
    }

    fn positive(self) -> bool {
        matches!(self, Self::East | Self::Up | Self::South)
    }
}

/// Exact Create 6.0.x gearbox speed modifier from the powered input face to
/// an output face, both expressed as directions outward from the gearbox.
/// This is `GearboxBlockEntity`'s direction-dependent `getAxisModifier`:
/// straight-through opposite faces reverse; same-direction perpendicular
/// axis signs reverse; opposite perpendicular axis signs preserve direction.
pub fn create_gearbox_transfer_sign(input_face: CreateFace, output_face: CreateFace) -> i32 {
    if input_face.axis() == output_face.axis() {
        if input_face == output_face {
            1
        } else {
            -1
        }
    } else if input_face.positive() == output_face.positive() {
        -1
    } else {
        1
    }
}

// ---------------------------------------------------------------------------
// Item/attribute filters used by filtered funnel handoff
// ---------------------------------------------------------------------------

/// Whether listed entries are accepted or rejected.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CreateFilterMode {
    AllowListed,
    DenyListed,
}

/// Deterministic item/attribute filter. Item IDs and attribute IDs are the
/// adapter's interned numeric identifiers, so evaluation has no string-order
/// or hash-order dependence.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CreateItemFilter {
    mode: CreateFilterMode,
    items: BTreeSet<u32>,
    attributes: BTreeSet<u32>,
}

impl CreateItemFilter {
    pub fn unfiltered() -> Self {
        Self {
            mode: CreateFilterMode::DenyListed,
            items: BTreeSet::new(),
            attributes: BTreeSet::new(),
        }
    }

    pub fn allow_items(items: impl IntoIterator<Item = u32>) -> Self {
        Self {
            mode: CreateFilterMode::AllowListed,
            items: items.into_iter().collect(),
            attributes: BTreeSet::new(),
        }
    }

    pub fn deny_items(items: impl IntoIterator<Item = u32>) -> Self {
        Self {
            mode: CreateFilterMode::DenyListed,
            items: items.into_iter().collect(),
            attributes: BTreeSet::new(),
        }
    }

    pub fn with_attributes(mut self, attributes: impl IntoIterator<Item = u32>) -> Self {
        self.attributes.extend(attributes);
        self
    }

    /// An unlisted empty filter accepts everything; otherwise direct item
    /// membership or any listed attribute marks the stack as listed.
    pub fn matches(&self, item_id: u32, item_attributes: &[u32]) -> bool {
        let listed = self.items.contains(&item_id)
            || item_attributes
                .iter()
                .any(|attribute| self.attributes.contains(attribute));
        match self.mode {
            CreateFilterMode::AllowListed => listed,
            CreateFilterMode::DenyListed => !listed,
        }
    }
}

// ---------------------------------------------------------------------------
// RPM-scaled pump network
// ---------------------------------------------------------------------------

/// Fixed-point pump throughput denominator: Create 6.0.x pumps move
/// `abs(RPM) / 2` milli-buckets per tick. Doubling RPM exactly doubles flow.
pub const CREATE_PUMP_MB_PER_RPM_DENOMINATOR: u32 = 2;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CreateFluidTankSnapshot {
    pub fluid: Option<u32>,
    pub amount_mb: u32,
    pub capacity_mb: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CreateFluidError {
    DuplicateTank(u32),
    MissingTank(u32),
    InvalidCapacity(u32),
    DuplicatePump(u32),
    MissingPump(u32),
    PumpSelfLink,
}

#[derive(Clone, Copy, Debug)]
struct CompatFluidTank {
    fluid: Option<u32>,
    amount_mb: u32,
    capacity_mb: u32,
}

#[derive(Clone, Copy, Debug)]
struct CompatFluidPump {
    left: u32,
    right: u32,
    rpm: i32,
    reversed: bool,
    remainder_num: u32,
}

/// Deterministic conservation-first pipe/pump network. Each pump connects
/// two tank endpoints. Like Create's arrow-facing mechanical pump, rotational
/// direction does not select flow direction: magnitude `abs(RPM)` drives flow
/// along the pump arrow (`left -> right` unless wrenched/reversed), and zero
/// RPM moves nothing. Multiple pumps may connect the same pair and are
/// evaluated in ascending pump-ID order every tick.
pub struct CreateFluidNetwork {
    tanks: BTreeMap<u32, CompatFluidTank>,
    pumps: BTreeMap<u32, CompatFluidPump>,
    total_moved_mb: u64,
}

impl Default for CreateFluidNetwork {
    fn default() -> Self {
        Self::new()
    }
}

impl CreateFluidNetwork {
    pub fn new() -> Self {
        Self {
            tanks: BTreeMap::new(),
            pumps: BTreeMap::new(),
            total_moved_mb: 0,
        }
    }

    pub fn add_tank(&mut self, id: u32, capacity_mb: u32) -> Result<(), CreateFluidError> {
        if capacity_mb == 0 {
            return Err(CreateFluidError::InvalidCapacity(id));
        }
        if self.tanks.contains_key(&id) {
            return Err(CreateFluidError::DuplicateTank(id));
        }
        self.tanks.insert(
            id,
            CompatFluidTank {
                fluid: None,
                amount_mb: 0,
                capacity_mb,
            },
        );
        Ok(())
    }

    pub fn add_pump(&mut self, id: u32, left: u32, right: u32) -> Result<(), CreateFluidError> {
        if left == right {
            return Err(CreateFluidError::PumpSelfLink);
        }
        if !self.tanks.contains_key(&left) {
            return Err(CreateFluidError::MissingTank(left));
        }
        if !self.tanks.contains_key(&right) {
            return Err(CreateFluidError::MissingTank(right));
        }
        if self.pumps.contains_key(&id) {
            return Err(CreateFluidError::DuplicatePump(id));
        }
        self.pumps.insert(
            id,
            CompatFluidPump {
                left,
                right,
                rpm: 0,
                reversed: false,
                remainder_num: 0,
            },
        );
        Ok(())
    }

    pub fn set_pump_rpm(&mut self, id: u32, rpm: i32) -> Result<(), CreateFluidError> {
        let pump = self.pumps.get_mut(&id).ok_or(CreateFluidError::MissingPump(id))?;
        pump.rpm = rpm;
        Ok(())
    }

    /// Apply the wrench-visible pump arrow direction. Rotation sign never
    /// reverses a Create mechanical pump; this orientation flag does.
    pub fn set_pump_reversed(&mut self, id: u32, reversed: bool) -> Result<(), CreateFluidError> {
        let pump = self.pumps.get_mut(&id).ok_or(CreateFluidError::MissingPump(id))?;
        pump.reversed = reversed;
        Ok(())
    }

    /// Insert into one tank. The receiver never accepts a second fluid type;
    /// the unaccepted remainder is returned to the caller, never voided.
    pub fn insert_fluid(
        &mut self,
        id: u32,
        fluid: u32,
        amount_mb: u32,
    ) -> Result<u32, CreateFluidError> {
        let tank = self.tanks.get_mut(&id).ok_or(CreateFluidError::MissingTank(id))?;
        if tank.amount_mb != 0 && tank.fluid != Some(fluid) {
            return Ok(0);
        }
        let accepted = amount_mb.min(tank.capacity_mb - tank.amount_mb);
        if accepted != 0 {
            tank.fluid = Some(fluid);
            tank.amount_mb += accepted;
        }
        Ok(accepted)
    }

    pub fn extract_fluid(&mut self, id: u32, amount_mb: u32) -> Result<u32, CreateFluidError> {
        let tank = self.tanks.get_mut(&id).ok_or(CreateFluidError::MissingTank(id))?;
        let extracted = amount_mb.min(tank.amount_mb);
        tank.amount_mb -= extracted;
        if tank.amount_mb == 0 {
            tank.fluid = None;
        }
        Ok(extracted)
    }

    pub fn tank(&self, id: u32) -> Option<CreateFluidTankSnapshot> {
        self.tanks.get(&id).map(|tank| CreateFluidTankSnapshot {
            fluid: tank.fluid,
            amount_mb: tank.amount_mb,
            capacity_mb: tank.capacity_mb,
        })
    }

    pub fn total_moved_mb(&self) -> u64 {
        self.total_moved_mb
    }

    /// Evaluate all pumps once in pump-ID order. The returned value is the
    /// total milli-buckets transferred this tick; full receivers, empty
    /// sources, and incompatible fluids apply backpressure without loss.
    pub fn tick(&mut self) -> u64 {
        let mut moved_total = 0u64;
        let tanks = &mut self.tanks;
        for pump in self.pumps.values_mut() {
            let rpm_abs = pump.rpm.unsigned_abs();
            if rpm_abs == 0 {
                continue;
            }
            let numerator = rpm_abs + pump.remainder_num;
            let rate_mb = numerator / CREATE_PUMP_MB_PER_RPM_DENOMINATOR;
            pump.remainder_num = numerator % CREATE_PUMP_MB_PER_RPM_DENOMINATOR;
            if rate_mb == 0 {
                continue;
            }
            let (source_id, destination_id) = if pump.reversed {
                (pump.right, pump.left)
            } else {
                (pump.left, pump.right)
            };
            let (Some(&source), Some(&destination)) =
                (tanks.get(&source_id), tanks.get(&destination_id))
            else {
                continue;
            };
            if source.amount_mb == 0
                || destination.amount_mb == destination.capacity_mb
                || (destination.amount_mb != 0 && destination.fluid != source.fluid)
            {
                continue;
            }
            let moved = rate_mb
                .min(source.amount_mb)
                .min(destination.capacity_mb - destination.amount_mb);
            if moved == 0 {
                continue;
            }
            let fluid = source.fluid.unwrap();
            {
                let source = tanks.get_mut(&source_id).unwrap();
                source.amount_mb -= moved;
                if source.amount_mb == 0 {
                    source.fluid = None;
                }
            }
            {
                let destination = tanks.get_mut(&destination_id).unwrap();
                destination.fluid = Some(fluid);
                destination.amount_mb += moved;
            }
            moved_total += u64::from(moved);
        }
        self.total_moved_mb += moved_total;
        moved_total
    }
}

// ---------------------------------------------------------------------------
// RPM-scaled mechanical processors and basin output backpressure
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProcessingMachineKind {
    MechanicalPress,
    MechanicalMixer,
}

impl ProcessingMachineKind {
    /// Create 6.0.x minimum rotational speed. The mixer refuses to operate
    /// below 30 RPM; the press has no additional minimum beyond nonzero power.
    pub fn minimum_rpm(self) -> u32 {
        match self {
            Self::MechanicalPress => 1,
            Self::MechanicalMixer => 30,
        }
    }
}

/// Create 6.0.x mechanical press animation length in internal press ticks.
pub const CREATE_PRESS_CYCLE_TICKS: u64 = 240;

/// Internal press-tick advance per game tick at `rpm`, mirroring Create's
/// integer-truncated `lerp(clamp(abs(rpm) / 512, 0, 1), 1, 60)`.
pub fn create_press_running_tick_speed(rpm: u32) -> u64 {
    if rpm == 0 {
        return 0;
    }
    let scaled = u64::from(rpm.min(512));
    1 + (scaled * 59) / 512
}

/// Exact mixer processing ticks at a constant nonzero RPM. `recipe_duration`
/// is Create's data-driven `processing_time`; zero uses Create's default
/// recipe speed of one. The total craft cadence is 40 animation ticks plus
/// this value.
pub fn create_mixer_processing_ticks(rpm: u32, recipe_duration: u32) -> u64 {
    if rpm == 0 {
        return 0;
    }
    let ratio = (512 / rpm).max(1);
    let log2 = u64::from(u32::BITS - 1 - ratio.leading_zeros());
    let recipe_speed_ceil = if recipe_duration == 0 {
        15
    } else {
        (u64::from(recipe_duration) * 15).div_ceil(100)
    };
    (log2 * recipe_speed_ceil + 1).max(1)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CreateProcessingRecipe {
    pub input_item: u32,
    pub input_count: u32,
    /// `(item_id, count)` outputs, including probabilistic byproducts after
    /// their deterministic roll has already selected them.
    pub outputs: Vec<(u32, u32)>,
    /// Create's data-driven `processing_time`. Zero selects the default
    /// mixer recipe speed; press cadence is animation-controlled regardless.
    pub processing_time: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProcessingError {
    InvalidRecipe,
    WrongItem,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProcessorTickState {
    Idle,
    Stalled,
    Working,
    Backpressured,
    Completed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProcessorSnapshot {
    pub input_items: u32,
    pub output_items: u32,
    pub active: bool,
    pub progress: u64,
    pub required_progress: u64,
}

/// One press/mixer coupled to a basin-like output buffer. Inputs are consumed
/// exactly once when a craft starts; completion is all-or-nothing, so a
/// near-full basin cannot drop a secondary byproduct or duplicate the craft.
pub struct CreateProcessingMachine {
    kind: ProcessingMachineKind,
    recipe: CreateProcessingRecipe,
    rpm: i32,
    input_items: u32,
    input_capacity: u32,
    outputs: BTreeMap<u32, u32>,
    output_capacity: u32,
    active: bool,
    progress: u64,
}

impl CreateProcessingMachine {
    pub fn new(
        kind: ProcessingMachineKind,
        recipe: CreateProcessingRecipe,
        input_capacity: u32,
        output_capacity: u32,
    ) -> Result<Self, ProcessingError> {
        if recipe.input_count == 0
            || recipe.outputs.is_empty()
            || recipe.outputs.iter().any(|&(_, count)| count == 0)
        {
            return Err(ProcessingError::InvalidRecipe);
        }
        Ok(Self {
            kind,
            recipe,
            rpm: 0,
            input_items: 0,
            input_capacity,
            outputs: BTreeMap::new(),
            output_capacity,
            active: false,
            progress: 0,
        })
    }

    pub fn set_rpm(&mut self, rpm: i32) {
        self.rpm = rpm;
    }

    pub fn insert_input(&mut self, item: u32, count: u32) -> Result<u32, ProcessingError> {
        if item != self.recipe.input_item {
            return Err(ProcessingError::WrongItem);
        }
        let accepted = count.min(self.input_capacity.saturating_sub(self.input_items));
        self.input_items += accepted;
        Ok(accepted)
    }

    /// Insert output produced by another machine sharing the basin. This is
    /// what can make a previously started craft backpressure before its
    /// all-or-nothing completion check.
    pub fn insert_output(&mut self, item: u32, count: u32) -> u32 {
        let accepted = count.min(self.output_capacity.saturating_sub(self.output_items()));
        if accepted != 0 {
            *self.outputs.entry(item).or_insert(0) += accepted;
        }
        accepted
    }

    pub fn extract_output(&mut self, item: u32, count: u32) -> u32 {
        let Some(held) = self.outputs.get_mut(&item) else {
            return 0;
        };
        let extracted = count.min(*held);
        *held -= extracted;
        if *held == 0 {
            self.outputs.remove(&item);
        }
        extracted
    }

    pub fn required_progress(&self) -> u64 {
        match self.kind {
            // Completion fires only once runningTicks exceeds 240, so use
            // 241 as the integer threshold.
            ProcessingMachineKind::MechanicalPress => CREATE_PRESS_CYCLE_TICKS + 1,
            ProcessingMachineKind::MechanicalMixer => {
                40 + create_mixer_processing_ticks(
                    self.rpm.unsigned_abs(),
                    self.recipe.processing_time,
                )
            }
        }
    }

    pub fn output_items(&self) -> u32 {
        self.outputs.values().sum()
    }

    pub fn snapshot(&self) -> ProcessorSnapshot {
        ProcessorSnapshot {
            input_items: self.input_items,
            output_items: self.output_items(),
            active: self.active,
            progress: self.progress,
            required_progress: self.required_progress(),
        }
    }

    fn powered(&self) -> bool {
        self.rpm.unsigned_abs() >= self.kind.minimum_rpm()
    }

    fn output_space_available(&self) -> bool {
        let needed: u32 = self.recipe.outputs.iter().map(|&(_, count)| count).sum();
        self.output_items().saturating_add(needed) <= self.output_capacity
    }

    fn try_start(&mut self) {
        if !self.active
            && self.powered()
            && self.input_items >= self.recipe.input_count
            && self.output_space_available()
        {
            self.input_items -= self.recipe.input_count;
            self.active = true;
            self.progress = 0;
        }
    }

    fn try_finish(&mut self) -> bool {
        if !self.active || self.progress < self.required_progress() || !self.output_space_available()
        {
            return false;
        }
        for &(item, count) in &self.recipe.outputs {
            *self.outputs.entry(item).or_insert(0) += count;
        }
        self.active = false;
        self.progress = 0;
        true
    }

    pub fn tick(&mut self) -> ProcessorTickState {
        self.try_start();
        if !self.active {
            return ProcessorTickState::Idle;
        }
        if self.powered() {
            let advance = match self.kind {
                ProcessingMachineKind::MechanicalPress => {
                    create_press_running_tick_speed(self.rpm.unsigned_abs())
                }
                // Mixer animation and mixing ticks elapse one game tick at a
                // time; RPM has already selected processing_ticks.
                ProcessingMachineKind::MechanicalMixer => 1,
            };
            self.progress = self.progress.saturating_add(advance);
        }
        if self.try_finish() {
            return ProcessorTickState::Completed;
        }
        if self.progress >= self.required_progress() && !self.output_space_available() {
            return ProcessorTickState::Backpressured;
        }
        if self.powered() {
            ProcessorTickState::Working
        } else {
            ProcessorTickState::Stalled
        }
    }
}

// ---------------------------------------------------------------------------
// Mechanical Arm target selection and reservation
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArmDistributionMode {
    /// Skip currently unavailable targets while scanning in circular order.
    RoundRobin,
    /// Do not skip the next target: if it cannot accept the stack, the arm
    /// waits instead of re-ordering the distribution.
    ForcedRoundRobin,
    /// Always use the earliest selected target that can accept the stack.
    PreferFirstTarget,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ArmTargetSnapshot {
    pub capacity: u32,
    pub held: u32,
    pub reserved: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArmError {
    EmptyTargetList,
    InvalidCount,
    MissingTarget(usize),
    NoTransferInFlight,
}

#[derive(Clone, Copy, Debug)]
struct ArmTarget {
    capacity: u32,
    held: u32,
    reserved: u32,
}

/// Deterministic mechanical-arm target selector. A reservation is part of a
/// target's occupied capacity while the arm is in flight, preventing two
/// dispatches from overfilling a nearly full inventory.
pub struct CreateMechanicalArm {
    mode: ArmDistributionMode,
    targets: Vec<ArmTarget>,
    next_target: usize,
    in_flight: Option<(usize, u32)>,
}

impl CreateMechanicalArm {
    pub fn new(mode: ArmDistributionMode, target_capacities: &[u32]) -> Self {
        Self {
            mode,
            targets: target_capacities
                .iter()
                .map(|&capacity| ArmTarget {
                    capacity,
                    held: 0,
                    reserved: 0,
                })
                .collect(),
            next_target: 0,
            in_flight: None,
        }
    }

    pub fn target(&self, index: usize) -> Option<ArmTargetSnapshot> {
        self.targets.get(index).copied().map(|target| ArmTargetSnapshot {
            capacity: target.capacity,
            held: target.held,
            reserved: target.reserved,
        })
    }

    pub fn next_target(&self) -> usize {
        self.next_target
    }

    pub fn in_flight(&self) -> Option<(usize, u32)> {
        self.in_flight
    }

    fn available(target: ArmTarget, count: u32) -> bool {
        target
            .held
            .saturating_add(target.reserved)
            .saturating_add(count)
            <= target.capacity
    }

    pub fn begin_transfer(&mut self, count: u32) -> Result<Option<usize>, ArmError> {
        if self.targets.is_empty() {
            return Err(ArmError::EmptyTargetList);
        }
        if count == 0 {
            return Err(ArmError::InvalidCount);
        }
        if self.in_flight.is_some() {
            return Ok(None);
        }
        let chosen = match self.mode {
            ArmDistributionMode::RoundRobin => (0..self.targets.len())
                .map(|offset| (self.next_target + offset) % self.targets.len())
                .find(|&index| Self::available(self.targets[index], count)),
            ArmDistributionMode::ForcedRoundRobin => {
                let index = self.next_target;
                Self::available(self.targets[index], count).then_some(index)
            }
            ArmDistributionMode::PreferFirstTarget => (0..self.targets.len())
                .find(|&index| Self::available(self.targets[index], count)),
        };
        if let Some(index) = chosen {
            self.targets[index].reserved += count;
            self.in_flight = Some((index, count));
        }
        Ok(chosen)
    }

    pub fn complete_transfer(&mut self) -> Result<usize, ArmError> {
        let Some((index, count)) = self.in_flight.take() else {
            return Err(ArmError::NoTransferInFlight);
        };
        let target = self
            .targets
            .get_mut(index)
            .ok_or(ArmError::MissingTarget(index))?;
        target.reserved -= count;
        target.held += count;
        self.next_target = (index + 1) % self.targets.len();
        Ok(index)
    }

    pub fn cancel_transfer(&mut self) -> Result<(), ArmError> {
        let Some((index, count)) = self.in_flight.take() else {
            return Err(ArmError::NoTransferInFlight);
        };
        let target = self
            .targets
            .get_mut(index)
            .ok_or(ArmError::MissingTarget(index))?;
        target.reserved -= count;
        Ok(())
    }
}
