// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later

//! C boundary for the standalone kernels. The matching public header is
//! `include/mc_spine_mod_offload.h`.
#![allow(unsafe_code)]

use std::mem::{align_of, size_of};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::ptr;
use std::slice;
use std::time::Duration;

use crate::fluid_network::FluidError;
use crate::kinetic_graph::{GraphError, KineticNode, NetworkStatus};
use crate::recipe_processor::{Drive, Recipe, RecipeError, Resource, ResourceKind, Stack};
use crate::region_scheduler::{BatchError, Region, RegionBudget, RegionKey};
use crate::sleeping_transport::TransportError;

#[repr(i32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MsoStatus {
    Ok = 0,
    InvalidArgument = 1,
    NotFound = 2,
    AlreadyExists = 3,
    Conflict = 4,
    Full = 5,
    TimeWentBackwards = 6,
    BufferTooSmall = 7,
    SequenceExhausted = 8,
    Panic = 9,
}

impl From<GraphError> for MsoStatus {
    fn from(error: GraphError) -> Self {
        match error {
            GraphError::MissingNode(_) | GraphError::MissingEdge => Self::NotFound,
            GraphError::DuplicateNode(_) | GraphError::DuplicateEdge => Self::AlreadyExists,
            _ => Self::InvalidArgument,
        }
    }
}

impl From<TransportError> for MsoStatus {
    fn from(error: TransportError) -> Self {
        match error {
            TransportError::MissingMachine(_) => Self::NotFound,
            TransportError::DuplicateMachine(_) => Self::AlreadyExists,
            TransportError::InputFull(_) => Self::Full,
            TransportError::OccupiedUpstream(_) => Self::Conflict,
            TransportError::TimeWentBackwards { .. } => Self::TimeWentBackwards,
            _ => Self::InvalidArgument,
        }
    }
}

impl From<FluidError> for MsoStatus {
    fn from(error: FluidError) -> Self {
        match error {
            FluidError::MissingTank(_) => Self::NotFound,
            FluidError::DuplicateTank(_) => Self::AlreadyExists,
            FluidError::IncompatibleFluids | FluidError::ConflictingHead => Self::Conflict,
            _ => Self::InvalidArgument,
        }
    }
}

impl From<RecipeError> for MsoStatus {
    fn from(error: RecipeError) -> Self {
        match error {
            RecipeError::UnknownMachine => Self::NotFound,
            RecipeError::DuplicateMachine => Self::AlreadyExists,
            _ => Self::InvalidArgument,
        }
    }
}

impl From<BatchError> for MsoStatus {
    fn from(error: BatchError) -> Self {
        match error {
            BatchError::QueueFull => Self::Full,
            BatchError::BufferTooSmall { .. } => Self::BufferTooSmall,
            BatchError::SequenceExhausted => Self::SequenceExhausted,
            _ => Self::InvalidArgument,
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct MsoNode {
    pub capacity: f64,
    pub load: f64,
    pub speed: f64,
    pub driven: u8,
}

impl TryFrom<MsoNode> for KineticNode {
    type Error = MsoStatus;

    fn try_from(node: MsoNode) -> Result<Self> {
        Ok(Self {
            capacity: node.capacity,
            load: node.load,
            driven_speed: flag(node.driven)?.then_some(node.speed),
        })
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct MsoSolution {
    pub capacity: f64,
    pub load: f64,
    pub speed: f64,
    pub component: u32,
    pub status: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct MsoItem {
    pub item: u32,
    pub present: u8,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct MsoTank {
    pub fluid: u32,
    pub amount: u32,
    pub present: u8,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct MsoDrive {
    pub kind: u32,
    pub rpm: u32,
    pub fuel_ticks: u64,
    pub overstressed: u8,
}

impl TryFrom<MsoDrive> for Drive {
    type Error = MsoStatus;

    fn try_from(drive: MsoDrive) -> Result<Self> {
        let overstressed = flag(drive.overstressed)?;
        match drive.kind {
            0 => Ok(Self::Mechanical {
                rpm: drive.rpm,
                overstressed,
            }),
            1 => Ok(Self::Furnace {
                fuel_ticks: drive.fuel_ticks,
            }),
            _ => Err(MsoStatus::InvalidArgument),
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct MsoStack {
    pub kind: u32,
    pub id: u32,
    pub amount: u32,
}

impl TryFrom<MsoStack> for Stack {
    type Error = MsoStatus;

    fn try_from(stack: MsoStack) -> Result<Self> {
        let kind = match stack.kind {
            0 => ResourceKind::Item,
            1 => ResourceKind::Fluid,
            _ => return Err(MsoStatus::InvalidArgument),
        };
        Ok(Self {
            resource: Resource { kind, id: stack.id },
            amount: stack.amount,
        })
    }
}

#[repr(C)]
pub struct MsoRecipe {
    pub id: u32,
    pub base_ticks: u32,
    pub inputs: *const MsoStack,
    pub input_count: usize,
    pub outputs: *const MsoStack,
    pub output_count: usize,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct MsoStepReport {
    pub processed: usize,
    pub backlog_tick: u64,
    pub has_backlog: u8,
    pub output_blocked: u8,
    pub time_exhausted: u8,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct MsoBatchReport {
    pub bytes: usize,
    pub records: usize,
    pub required_bytes: usize,
    pub sequence: u64,
}

/// Opaque to C. Only this module can access the owned region.
pub struct MsoRegion(Region);

type Result<T = ()> = std::result::Result<T, MsoStatus>;

fn boundary(operation: impl FnOnce() -> Result) -> MsoStatus {
    match catch_unwind(AssertUnwindSafe(operation)) {
        Ok(Ok(())) => MsoStatus::Ok,
        Ok(Err(status)) => status,
        Err(_) => MsoStatus::Panic,
    }
}

fn flag(value: u8) -> Result<bool> {
    match value {
        0 => Ok(false),
        1 => Ok(true),
        _ => Err(MsoStatus::InvalidArgument),
    }
}

fn validate_ptr<T>(ptr: *const T) -> Result {
    if ptr.is_null() || !ptr.addr().is_multiple_of(align_of::<T>()) {
        return Err(MsoStatus::InvalidArgument);
    }
    Ok(())
}

unsafe fn read_array<'a, T>(data: *const T, count: usize) -> Result<&'a [T]> {
    if count == 0 {
        return Ok(&[]);
    }
    validate_ptr(data)?;
    if count > isize::MAX as usize / size_of::<T>() {
        return Err(MsoStatus::InvalidArgument);
    }
    // SAFETY: The header requires initialized arrays in a single allocation
    // that remain readable and unmodified throughout the call.
    Ok(unsafe { slice::from_raw_parts(data, count) })
}

unsafe fn write_result<T>(out: *mut T, produce: impl FnOnce() -> Result<T>) -> Result {
    validate_ptr(out)?;
    // SAFETY: The caller supplies writable, aligned storage for one T.
    unsafe { out.write(produce()?) };
    Ok(())
}

/// Create a region at the given region coordinates.
///
/// # Safety
///
/// `out` must point to writable, aligned storage for one handle.
#[no_mangle]
pub unsafe extern "C" fn mso_region_create(
    dimension: i32,
    x: i32,
    z: i32,
    out: *mut *mut MsoRegion,
) -> MsoStatus {
    boundary(|| unsafe {
        write_result(out, || {
            Ok(Box::into_raw(Box::new(MsoRegion(Region::new(RegionKey {
                dimension,
                x,
                z,
            })))))
        })
    })
}

/// Free a region. A null handle succeeds.
///
/// # Safety
///
/// Any non-null handle must be live, exclusively owned and returned by create.
#[no_mangle]
pub unsafe extern "C" fn mso_region_free(handle: *mut MsoRegion) -> MsoStatus {
    boundary(|| {
        if !handle.is_null() {
            validate_ptr(handle)?;
            // SAFETY: The caller transfers ownership of a live Box handle.
            drop(unsafe { Box::from_raw(handle) });
        }
        Ok(())
    })
}

macro_rules! region_api {
    ($(fn $name:ident($region:ident $(, $arg:ident: $ty:ty)*) $body:block)*) => {$(
        /// See the matching declaration in `include/mc_spine_mod_offload.h`.
        ///
        /// # Safety
        ///
        /// A non-null handle must be live and exclusively borrowed for the
        /// call. Pointer arguments must satisfy the header's buffer contract.
        #[no_mangle]
        pub unsafe extern "C" fn $name(handle: *mut MsoRegion $(, $arg: $ty)*) -> MsoStatus {
            boundary(|| {
                validate_ptr(handle)?;
                // SAFETY: The caller exclusively borrows the live handle.
                let $region = unsafe { &mut (*handle).0 };
                $body
            })
        }
    )*};
}

region_api! {
    fn mso_region_step(region, target: u64, step_limit: usize, budget_us: u64, out: *mut MsoStepReport) {
        // SAFETY: The caller provides one writable report.
        unsafe { write_result(out, || {
            let report = region.advance_to(target, RegionBudget {
                max_steps: step_limit,
                max_time: (budget_us != u64::MAX).then(|| Duration::from_micros(budget_us)),
            })?;
            Ok(MsoStepReport {
                processed: report.processed,
                backlog_tick: report.backlog.unwrap_or(0),
                has_backlog: u8::from(report.backlog.is_some()),
                output_blocked: u8::from(report.output_blocked),
                time_exhausted: u8::from(report.time_exhausted),
            })
        }) }
    }

    fn mso_region_export(region, buffer: *mut u8, capacity: usize, record_limit: usize, out: *mut MsoBatchReport) {
        validate_ptr(out)?;
        if capacity != 0 {
            validate_ptr(buffer)?;
        }
        let result = region.export_with(capacity, record_limit, |offset, bytes| {
            // SAFETY: export_with bounds every write by capacity. The caller
            // supplies a disjoint writable buffer, which may be uninitialized.
            unsafe { ptr::copy_nonoverlapping(bytes.as_ptr(), buffer.add(offset), bytes.len()) };
        });
        match result {
            Ok(report) => {
                // SAFETY: The caller provides one writable report.
                unsafe { out.write(MsoBatchReport {
                    bytes: report.bytes,
                    records: report.records,
                    required_bytes: 0,
                    sequence: report.sequence.unwrap_or(0),
                }) };
                Ok(())
            }
            Err(BatchError::BufferTooSmall { needed }) => {
                // SAFETY: The caller provides one writable report.
                unsafe { out.write(MsoBatchReport { required_bytes: needed, ..MsoBatchReport::default() }) };
                Err(MsoStatus::BufferTooSmall)
            }
            Err(error) => Err(error.into()),
        }
    }

    fn mso_region_queue_delta(region, kind: u16, source: u32, payload: *const u8, length: usize) {
        if length > u16::MAX as usize {
            return Err(MsoStatus::InvalidArgument);
        }
        // SAFETY: The caller supplies a readable payload of length bytes.
        region.queue_delta(kind, source, unsafe { read_array(payload, length) }?)?;
        Ok(())
    }

    fn mso_kinetic_add(region, id: u32, node: MsoNode) {
        region.kinetic.add_node(id, node.try_into()?)?;
        Ok(())
    }

    fn mso_kinetic_update(region, id: u32, node: MsoNode) {
        region.kinetic.update_node(id, node.try_into()?)?;
        Ok(())
    }

    fn mso_kinetic_remove(region, id: u32) {
        region.kinetic.remove_node(id)?;
        Ok(())
    }

    fn mso_kinetic_link(region, a: u32, b: u32, connected: u8, ratio: f64) {
        if flag(connected)? {
            region.kinetic.connect(a, b, ratio)?;
        } else {
            region.kinetic.disconnect(a, b)?;
        }
        Ok(())
    }

    fn mso_kinetic_get(region, id: u32, out: *mut MsoSolution) {
        // SAFETY: The caller provides one writable solution.
        unsafe { write_result(out, || {
            let solution = region.kinetic.solution(id).ok_or(MsoStatus::NotFound)?;
            Ok(MsoSolution {
                capacity: solution.capacity,
                load: solution.load,
                speed: solution.speed,
                component: solution.component,
                status: match solution.status {
                    NetworkStatus::Unpowered => 0,
                    NetworkStatus::Running => 1,
                    NetworkStatus::Overstressed => 2,
                    NetworkStatus::Conflict => 3,
                },
            })
        }) }
    }

    fn mso_transport_add(region, id: u32, capacity: u32, delay: u64) {
        region.add_machine(id, capacity, delay)?;
        Ok(())
    }

    fn mso_transport_link(region, id: u32, connected: u8, destination: u32) {
        region.set_downstream(id, flag(connected)?.then_some(destination))?;
        Ok(())
    }

    fn mso_transport_insert(region, id: u32, item: u32) {
        region.insert_item(id, item)?;
        Ok(())
    }

    fn mso_transport_extract(region, id: u32, out: *mut MsoItem) {
        // SAFETY: The caller provides one writable item.
        unsafe { write_result(out, || {
            let item = region.extract_output(id)?;
            Ok(MsoItem { item: item.unwrap_or(0), present: u8::from(item.is_some()) })
        }) }
    }

    fn mso_fluid_add(region, id: u32, capacity: u32, elevation: i32) {
        region.add_tank(id, capacity, elevation)?;
        Ok(())
    }

    fn mso_fluid_set_tank(region, id: u32, capacity: u32, elevation: i32) {
        region.set_tank(id, capacity, elevation)?;
        Ok(())
    }

    fn mso_fluid_link(region, a: u32, b: u32, connected: u8, head: i32) {
        region.set_fluid_link(a, b, flag(connected)?.then_some(head))?;
        Ok(())
    }

    fn mso_fluid_insert(region, id: u32, fluid: u32, amount: u32, out: *mut u32) {
        // SAFETY: The caller provides one writable count.
        unsafe { write_result(out, || Ok(region.insert_fluid(id, fluid, amount)?)) }
    }

    fn mso_fluid_extract(region, id: u32, amount: u32, out: *mut u32) {
        // SAFETY: The caller provides one writable count.
        unsafe { write_result(out, || Ok(region.extract_fluid(id, amount)?)) }
    }

    fn mso_fluid_get(region, id: u32, out: *mut MsoTank) {
        // SAFETY: The caller provides one writable tank state.
        unsafe { write_result(out, || {
            let tank = region.fluid().tank(id).ok_or(MsoStatus::NotFound)?;
            Ok(MsoTank { fluid: tank.fluid.unwrap_or(0), amount: tank.amount, present: u8::from(tank.fluid.is_some()) })
        }) }
    }

    fn mso_recipe_add(region, id: u32, input_capacity: u32, output_capacity: u32, drive: MsoDrive, recipes: *const MsoRecipe, count: usize) {
        let drive = drive.try_into()?;
        // SAFETY: All recipe descriptors and their stack arrays are readable
        // throughout this call and copied into owned Rust configuration.
        let recipes = unsafe { read_array(recipes, count) }?.iter().map(|recipe| {
            let stacks = |data, length| {
                unsafe { read_array(data, length) }?.iter().copied().map(Stack::try_from).collect::<Result<Vec<_>>>()
            };
            Ok(Recipe {
                id: recipe.id,
                base_ticks: recipe.base_ticks,
                inputs: stacks(recipe.inputs, recipe.input_count)?,
                outputs: stacks(recipe.outputs, recipe.output_count)?,
            })
        }).collect::<Result<Vec<_>>>()?;
        region.add_processor(id, input_capacity, output_capacity, recipes, drive)?;
        Ok(())
    }

    fn mso_recipe_set_drive(region, id: u32, drive: MsoDrive) {
        region.set_processor_drive(id, drive.try_into()?)?;
        Ok(())
    }

    fn mso_recipe_insert(region, id: u32, stack: MsoStack, out: *mut u32) {
        // SAFETY: The caller provides one writable count.
        unsafe { write_result(out, || Ok(region.insert_ingredient(id, stack.try_into()?)?)) }
    }

    fn mso_recipe_extract(region, id: u32, stack: MsoStack, out: *mut u32) {
        // SAFETY: The caller provides one writable count.
        unsafe { write_result(out, || Ok(region.extract_product(id, stack.try_into()?)?)) }
    }

    fn mso_recipe_distribute(region, id: u32, stack: MsoStack, destinations: *const u32, count: usize, out: *mut u32) {
        // SAFETY: The caller provides a readable array of IDs and one
        // disjoint writable count.
        unsafe { write_result(out, || {
            let destinations = read_array(destinations, count)?;
            Ok(region.distribute_product(id, stack.try_into()?, destinations)?)
        }) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_boundary_contains_unwinding_panics() {
        assert_eq!(boundary(|| panic!("test panic")), MsoStatus::Panic);
    }
}
