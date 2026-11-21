// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later

//! C boundary for the standalone kernels. The matching public header is
//! `include/mc_spine_mod_offload.h`.
#![allow(unsafe_code)]

use std::mem::{align_of, size_of};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::ptr;
use std::slice;
use std::time::Duration;

use crate::contraption::{Aabb, BlockCluster, BlockDelta, ClusterError, Contraption, Pose};
use crate::fluid_network::FluidError;
use crate::kinetic_graph::{GraphError, KineticNode, NetworkStatus};
use crate::recipe_processor::{Drive, Recipe, RecipeError, Resource, ResourceKind, Stack};
use crate::region_scheduler::{BatchError, BatchQueue, Region, RegionBudget, RegionKey};
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

impl From<ClusterError> for MsoStatus {
    fn from(error: ClusterError) -> Self {
        match error {
            ClusterError::DuplicatePosition(_) => Self::InvalidArgument,
            ClusterError::PaletteFull => Self::Full,
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

#[repr(C)]
#[derive(Clone, Copy)]
pub struct MsoBlockDiff {
    pub position: [i32; 3],
    pub state: u32,
    pub present: u8,
}

impl TryFrom<MsoBlockDiff> for BlockDelta {
    type Error = MsoStatus;

    fn try_from(diff: MsoBlockDiff) -> Result<Self> {
        Ok(Self {
            position: diff.position,
            state: flag(diff.present)?.then_some(diff.state),
        })
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct MsoContraptionPose {
    pub position: [f64; 3],
    pub yaw: f32,
    pub pitch: f32,
    pub linear_velocity: [f32; 3],
    pub angular_velocity: [f32; 3],
}

impl MsoContraptionPose {
    fn payload(self) -> [u8; 56] {
        let mut payload = [0; 56];
        for (out, value) in payload[..24]
            .as_chunks_mut::<8>()
            .0
            .iter_mut()
            .zip(self.position)
        {
            *out = value.to_le_bytes();
        }
        let motion = [self.yaw, self.pitch]
            .into_iter()
            .chain(self.linear_velocity)
            .chain(self.angular_velocity);
        for (out, value) in payload[24..].as_chunks_mut::<4>().0.iter_mut().zip(motion) {
            *out = value.to_le_bytes();
        }
        payload
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct MsoAabb {
    pub min: [f64; 3],
    pub max: [f64; 3],
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct MsoActor {
    pub bounds: MsoAabb,
    pub previous_anchor: [f64; 3],
    pub carried: u8,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct MsoActorCollision {
    pub delta: [f64; 3],
    pub intersects: u8,
}

pub const CONTRAPTION_POSE: u16 = 0x101;
pub const CONTRAPTION_BLOCK_DIFFS: u16 = 0x102;
const BLOCK_DIFF_BYTES: usize = 17;

/// Opaque to C. Each handle owns its simulation state and S5 stream.
pub struct MsoContraption {
    id: u32,
    tick: u64,
    body: Contraption,
    previous: Pose,
    pose: MsoContraptionPose,
    batch: BatchQueue,
}

impl MsoContraption {
    fn check_tick(&self, tick: u64) -> Result {
        if tick < self.tick {
            return Err(MsoStatus::TimeWentBackwards);
        }
        Ok(())
    }
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
    if ptr.is_null()
        || !ptr.addr().is_multiple_of(align_of::<T>())
        || ptr.addr().checked_add(size_of::<T>()).is_none()
    {
        return Err(MsoStatus::InvalidArgument);
    }
    Ok(())
}

fn validate_array<T>(data: *const T, count: usize) -> Result {
    if count == 0 {
        return Ok(());
    }
    validate_ptr(data)?;
    match count.checked_mul(size_of::<T>()) {
        Some(bytes) if bytes <= isize::MAX as usize && data.addr().checked_add(bytes).is_some() => {
            Ok(())
        }
        _ => Err(MsoStatus::InvalidArgument),
    }
}

unsafe fn read_array<'a, T>(data: *const T, count: usize) -> Result<&'a [T]> {
    validate_array(data, count)?;
    if count == 0 {
        return Ok(&[]);
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

unsafe fn export_batch(
    batch: &mut BatchQueue,
    buffer: *mut u8,
    capacity: usize,
    record_limit: usize,
    out: *mut MsoBatchReport,
) -> Result {
    validate_ptr(out)?;
    validate_array(buffer, capacity)?;
    let result = batch.export_with(capacity, record_limit, |offset, bytes| {
        // SAFETY: export_with bounds every write by capacity. The caller
        // supplies a disjoint writable buffer, which may be uninitialized.
        unsafe { ptr::copy_nonoverlapping(bytes.as_ptr(), buffer.add(offset), bytes.len()) };
    });
    let (report, status) = match result {
        Ok(report) => (
            MsoBatchReport {
                bytes: report.bytes,
                records: report.records,
                required_bytes: 0,
                sequence: report.sequence.unwrap_or(0),
            },
            Ok(()),
        ),
        Err(BatchError::BufferTooSmall { needed }) => (
            MsoBatchReport {
                required_bytes: needed,
                ..MsoBatchReport::default()
            },
            Err(MsoStatus::BufferTooSmall),
        ),
        Err(error) => return Err(error.into()),
    };
    // SAFETY: The caller provides one writable report.
    unsafe { out.write(report) };
    status
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
                y: 0,
                z,
            })))))
        })
    })
}

/// Create a region at the given tall-dimension banded coordinates. The `y`
/// argument is a vertical band index (block y div 32); pass 0 for
/// vanilla-height dimensions.
///
/// # Safety
///
/// `out` must point to writable, aligned storage for one handle.
#[no_mangle]
pub unsafe extern "C" fn mso_region_create_tall(
    dimension: i32,
    x: i32,
    y: i32,
    z: i32,
    out: *mut *mut MsoRegion,
) -> MsoStatus {
    boundary(|| unsafe {
        write_result(out, || {
            Ok(Box::into_raw(Box::new(MsoRegion(Region::new(RegionKey {
                dimension,
                x,
                y,
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

/// Create an independent contraption at the identity pose.
///
/// # Safety
///
/// The input array and writable handle output must satisfy the C header.
#[no_mangle]
pub unsafe extern "C" fn mso_contraption_create(
    dimension: i32,
    x: i32,
    z: i32,
    id: u32,
    blocks: *const MsoBlockDiff,
    count: usize,
    out: *mut *mut MsoContraption,
) -> MsoStatus {
    boundary(|| unsafe {
        write_result(out, || {
            let blocks = read_array(blocks, count)?
                .iter()
                .map(|&block| {
                    let block = BlockDelta::try_from(block)?;
                    Ok((
                        block.position,
                        block.state.ok_or(MsoStatus::InvalidArgument)?,
                    ))
                })
                .collect::<Result<Vec<_>>>()?;
            let previous = Pose::default();
            let body = Contraption::new(BlockCluster::new(blocks)?, previous);
            Ok(Box::into_raw(Box::new(MsoContraption {
                id,
                tick: 0,
                body,
                previous,
                pose: MsoContraptionPose::default(),
                batch: BatchQueue::new(RegionKey {
                    dimension,
                    x,
                    y: 0,
                    z,
                }),
            })))
        })
    })
}

/// Free a contraption. A null handle succeeds.
///
/// # Safety
///
/// Any non-null handle must be live, exclusively owned and returned by create.
#[no_mangle]
pub unsafe extern "C" fn mso_contraption_free(handle: *mut MsoContraption) -> MsoStatus {
    boundary(|| {
        if !handle.is_null() {
            validate_ptr(handle)?;
            // SAFETY: The caller transfers ownership of a live Box handle.
            drop(unsafe { Box::from_raw(handle) });
        }
        Ok(())
    })
}

unsafe fn with_contraption(
    handle: *mut MsoContraption,
    operation: impl FnOnce(&mut MsoContraption) -> Result,
) -> MsoStatus {
    boundary(|| {
        validate_ptr(handle)?;
        // SAFETY: The caller exclusively borrows the live handle.
        operation(unsafe { &mut *handle })
    })
}

/// Apply one array of incremental local block edits.
///
/// # Safety
///
/// The live handle and input array must satisfy the C header.
#[no_mangle]
pub unsafe extern "C" fn mso_contraption_apply_diffs(
    handle: *mut MsoContraption,
    tick: u64,
    diffs: *const MsoBlockDiff,
    count: usize,
) -> MsoStatus {
    unsafe {
        with_contraption(handle, |state| {
            state.check_tick(tick)?;
            let mut edits = read_array(diffs, count)?
                .iter()
                .copied()
                .map(BlockDelta::try_from)
                .collect::<Result<Vec<_>>>()?;
            edits.sort_unstable_by_key(|edit| edit.position);
            if edits
                .windows(2)
                .any(|pair| pair[0].position == pair[1].position)
            {
                return Err(MsoStatus::InvalidArgument);
            }
            edits.retain(|edit| state.body.cluster().state_at(edit.position) != edit.state);
            if !edits.is_empty() {
                let length = edits.len() * BLOCK_DIFF_BYTES;
                state.batch.check_room(length)?;
                let mut payload = Vec::with_capacity(length);
                for edit in &edits {
                    for coord in edit.position {
                        payload.extend_from_slice(&coord.to_le_bytes());
                    }
                    payload.push(u8::from(edit.state.is_some()));
                    payload.extend_from_slice(&edit.state.unwrap_or(0).to_le_bytes());
                }
                state.body.apply_deltas(&edits)?;
                state
                    .batch
                    .push(tick, state.id, CONTRAPTION_BLOCK_DIFFS, &payload);
            }
            state.tick = tick;
            Ok(())
        })
    }
}

/// Set the absolute pose and velocity metadata.
///
/// # Safety
///
/// The live handle must satisfy the C header.
#[no_mangle]
pub unsafe extern "C" fn mso_contraption_set_pose(
    handle: *mut MsoContraption,
    tick: u64,
    pose: MsoContraptionPose,
) -> MsoStatus {
    unsafe {
        with_contraption(handle, |state| {
            state.check_tick(tick)?;
            let transform = Pose::from_yaw_pitch(pose.position, pose.yaw, pose.pitch)
                .ok_or(MsoStatus::InvalidArgument)?;
            if !pose
                .linear_velocity
                .iter()
                .chain(&pose.angular_velocity)
                .all(|v| v.is_finite())
            {
                return Err(MsoStatus::InvalidArgument);
            }
            let previous = state.body.pose();
            if pose != state.pose {
                let payload = pose.payload();
                state.batch.check_room(payload.len())?;
                state.body.set_pose(transform);
                state.batch.push(tick, state.id, CONTRAPTION_POSE, &payload);
                state.pose = pose;
            }
            state.previous = previous;
            state.tick = tick;
            Ok(())
        })
    }
}

/// Query the current shape and the most recent accepted rigid motion.
///
/// # Safety
///
/// The live handle and writable report must satisfy the C header.
#[no_mangle]
pub unsafe extern "C" fn mso_contraption_collide_actor(
    handle: *mut MsoContraption,
    actor: MsoActor,
    out: *mut MsoActorCollision,
) -> MsoStatus {
    unsafe {
        with_contraption(handle, |state| {
            write_result(out, || {
                let bounds = Aabb::new(actor.bounds.min, actor.bounds.max)
                    .ok_or(MsoStatus::InvalidArgument)?;
                let delta = if flag(actor.carried)? {
                    state
                        .body
                        .pose()
                        .carried_delta(state.previous, actor.previous_anchor)
                        .ok_or(MsoStatus::InvalidArgument)?
                } else {
                    [0.0; 3]
                };
                Ok(MsoActorCollision {
                    delta,
                    intersects: u8::from(state.body.intersects(bounds)),
                })
            })
        })
    }
}

/// Export queued contraption records in an S5 frame.
///
/// # Safety
///
/// The live handle, buffer and writable report must satisfy the C header.
#[no_mangle]
pub unsafe extern "C" fn mso_contraption_export_delta(
    handle: *mut MsoContraption,
    buffer: *mut u8,
    capacity: usize,
    record_limit: usize,
    out: *mut MsoBatchReport,
) -> MsoStatus {
    unsafe {
        with_contraption(handle, |state| {
            export_batch(&mut state.batch, buffer, capacity, record_limit, out)
        })
    }
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
        unsafe { export_batch(&mut region.batch, buffer, capacity, record_limit, out) }
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
