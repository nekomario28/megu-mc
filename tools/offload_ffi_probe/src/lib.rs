#![deny(unsafe_op_in_unsafe_fn)]

mod fixture;
mod frame;

use std::cell::Cell;
use std::collections::BTreeMap;
use std::ffi::c_void;
use std::ptr::{self, NonNull};
use std::slice;
use std::sync::Mutex;
use std::time::Instant;

use mod_offload::ffi::*;
use mod_offload::region_scheduler::{BATCH_HEADER_BYTES, MAX_BATCH_BYTES, MAX_BATCH_RECORDS};

const BAD_BUFFER: i32 = -10;
const BAD_CAPACITY: i32 = -11;
const BAD_ADDRESS: i32 = -12;
const BAD_LENGTH: i32 = -13;
const BAD_HANDLE: i32 = -14;
const BACKLOG: i64 = 1 << 32;

type Result<T = ()> = std::result::Result<T, i32>;
type Env = *mut *const JniFunctions;
type Object = *mut c_void;

#[repr(C)]
struct JniFunctions {
    // JNIEnv slots 230 and 231 in OpenJDK's jni.h.
    _preceding_entries: [*const c_void; 230],
    direct_address: unsafe extern "system" fn(Env, Object) -> *mut u8,
    direct_capacity: unsafe extern "system" fn(Env, Object) -> i64,
}

struct DirectBuffer {
    address: NonNull<u8>,
    capacity: usize,
}

impl DirectBuffer {
    // JNI keeps the argument live for the call. Java owns and exclusively
    // accesses its storage until the call returns.
    unsafe fn get(env: Env, buffer: Object, min: usize, max: usize) -> Result<Self> {
        if env.is_null() || buffer.is_null() {
            return Err(BAD_BUFFER);
        }
        let table = unsafe { *env };
        if table.is_null() {
            return Err(BAD_BUFFER);
        }
        let capacity = unsafe { ((*table).direct_capacity)(env, buffer) };
        let capacity = usize::try_from(capacity).map_err(|_| BAD_CAPACITY)?;
        if !(min..=max).contains(&capacity) {
            return Err(BAD_CAPACITY);
        }
        let address = unsafe { ((*table).direct_address)(env, buffer) };
        let address = NonNull::new(address).ok_or(BAD_ADDRESS)?;
        Ok(Self { address, capacity })
    }

    // The caller keeps the initialized frame prefix unmodified while borrowed.
    unsafe fn frame(&self, length: i32, max: usize) -> Result<&[u8]> {
        let length = usize::try_from(length).map_err(|_| BAD_LENGTH)?;
        if !(frame::HEADER..=self.capacity.min(max)).contains(&length) {
            return Err(BAD_LENGTH);
        }
        let bytes = unsafe { slice::from_raw_parts(self.address.as_ptr(), length) };
        if frame::u32at(bytes, 16) as usize > self.capacity - frame::HEADER {
            return Err(BAD_LENGTH);
        }
        Ok(bytes)
    }
}

fn status(value: MsoStatus) -> Result {
    match value {
        MsoStatus::Ok => Ok(()),
        other => Err(-100 - other as i32),
    }
}

const INVALID_ARGUMENT: i32 = -100 - MsoStatus::InvalidArgument as i32;
const ABI_PANIC: i32 = -100 - MsoStatus::Panic as i32;

struct RegionHandle {
    region: NonNull<MsoRegion>,
    generations: [u64; 3],
    last_tick: u64,
    factory: Option<fixture::Factory>,
}

// The region owns its state. The registry lock serializes all access and
// destruction, so no C call can overlap any other use of the same handle.
unsafe impl Send for RegionHandle {}

const _: fn() = || {
    fn require_send<T: Send>() {}
    require_send::<MsoRegion>();
};

impl RegionHandle {
    fn new(dimension: i32, x: i32, z: i32) -> Result<Self> {
        let mut region = ptr::null_mut();
        status(unsafe { mso_region_create(dimension, x, z, &mut region) })?;
        Ok(Self {
            region: NonNull::new(region).ok_or(BAD_HANDLE)?,
            generations: [0; 3],
            last_tick: 0,
            factory: None,
        })
    }

    fn step(&mut self, tick: u64) -> Result<MsoStepReport> {
        let mut report = MsoStepReport::default();
        status(unsafe {
            mso_region_step(
                self.region.as_ptr(),
                tick,
                MAX_BATCH_RECORDS,
                u64::MAX,
                &mut report,
            )
        })?;
        if report.has_backlog == 0 {
            self.last_tick = tick;
        }
        Ok(report)
    }

    fn export(&mut self, address: *mut u8, capacity: usize) -> Result<usize> {
        let mut report = MsoBatchReport::default();
        status(unsafe {
            mso_region_export(
                self.region.as_ptr(),
                address,
                capacity,
                MAX_BATCH_RECORDS,
                &mut report,
            )
        })?;
        Ok(report.bytes)
    }
}

impl Drop for RegionHandle {
    fn drop(&mut self) {
        // The registry removes the sole owner before dropping it. A region
        // that reported MSO_PANIC is also released here.
        unsafe { mso_region_free(self.region.as_ptr()) };
    }
}

struct Registry {
    last_id: i64,
    regions: BTreeMap<i64, RegionHandle>,
}

static REGISTRY: Mutex<Registry> = Mutex::new(Registry {
    last_id: 0,
    regions: BTreeMap::new(),
});

fn register(region: RegionHandle) -> Result<i64> {
    let mut registry = REGISTRY.lock().map_err(|_| ABI_PANIC)?;
    let id = registry.last_id.checked_add(1).ok_or(BAD_HANDLE)?;
    registry.regions.insert(id, region);
    registry.last_id = id;
    Ok(id)
}

fn with_region<T>(handle: i64, run: impl FnOnce(&mut RegionHandle) -> Result<T>) -> Result<T> {
    let mut registry = REGISTRY.lock().map_err(|_| ABI_PANIC)?;
    let region = registry.regions.get_mut(&handle).ok_or(BAD_HANDLE)?;
    let result = run(region);
    if matches!(result, Err(ABI_PANIC)) {
        registry.regions.remove(&handle);
    }
    result
}

thread_local! {
    static LAST_GENERATION: Cell<[u64; 3]> = const { Cell::new([0, 0, 0]) };
}

#[no_mangle]
unsafe extern "system" fn Java_probe_NativeBridge_transfer(
    env: Env,
    _: Object,
    buffer: Object,
    length: i32,
) -> i32 {
    let transfer = || {
        let buffer = unsafe { DirectBuffer::get(env, buffer, frame::HEADER, frame::MAX_FRAME) }?;
        let bytes = unsafe { buffer.frame(length, frame::MAX_FRAME) }?;
        let mut generations = LAST_GENERATION.get();
        let count = frame::validate(bytes, &generations)?.accept(&mut generations);
        LAST_GENERATION.set(generations);
        Ok(count)
    };
    transfer().unwrap_or_else(|err| err)
}

#[no_mangle]
extern "system" fn Java_probe_NativeBridge_create(
    _: Env,
    _: Object,
    dimension: i32,
    x: i32,
    z: i32,
) -> i64 {
    RegionHandle::new(dimension, x, z)
        .and_then(register)
        .unwrap_or_else(i64::from)
}

#[no_mangle]
unsafe extern "system" fn Java_probe_NativeBridge_enqueueDelta(
    env: Env,
    _: Object,
    handle: i64,
    kind: i32,
    source: i32,
    buffer: Object,
    length: i32,
) -> i32 {
    with_region(handle, |region| {
        let kind = u16::try_from(kind).map_err(|_| INVALID_ARGUMENT)?;
        let buffer = unsafe { DirectBuffer::get(env, buffer, frame::HEADER, frame::MAX_FRAME) }?;
        let bytes = unsafe { buffer.frame(length, u16::MAX as usize) }?;
        let validated = frame::validate(bytes, &region.generations)?;
        status(unsafe {
            mso_region_queue_delta(
                region.region.as_ptr(),
                kind,
                source as u32,
                bytes.as_ptr(),
                bytes.len(),
            )
        })?;
        validated.accept(&mut region.generations);
        Ok(0)
    })
    .unwrap_or_else(|err| err)
}

#[no_mangle]
unsafe extern "system" fn Java_probe_NativeBridge_stepInto(
    env: Env,
    _: Object,
    handle: i64,
    tick: i64,
    output: Object,
) -> i64 {
    with_region(handle, |region| {
        let tick = u64::try_from(tick).map_err(|_| INVALID_ARGUMENT)?;
        let output =
            unsafe { DirectBuffer::get(env, output, BATCH_HEADER_BYTES, MAX_BATCH_BYTES) }?;
        let step = region.step(tick)?;
        let bytes = region.export(output.address.as_ptr(), output.capacity)?;
        Ok(bytes as i64 | if step.has_backlog != 0 { BACKLOG } else { 0 })
    })
    .unwrap_or_else(i64::from)
}

#[no_mangle]
extern "system" fn Java_probe_NativeBridge_destroy(_: Env, _: Object, handle: i64) -> i32 {
    let Ok(mut registry) = REGISTRY.lock() else {
        return ABI_PANIC;
    };
    match registry.regions.remove(&handle) {
        Some(_) => 0,
        None => BAD_HANDLE,
    }
}

#[no_mangle]
extern "system" fn Java_probe_NativeBridge_createSettled(_: Env, _: Object) -> i64 {
    (|| {
        let mut region = RegionHandle::new(0, 0, 0)?;
        fixture::settle(&mut region)?;
        register(region)
    })()
    .unwrap_or_else(i64::from)
}

#[no_mangle]
extern "system" fn Java_probe_NativeBridge_createFactory(
    _: Env,
    _: Object,
    dormant: i32,
    active: i32,
) -> i64 {
    (|| {
        let dormant = u32::try_from(dormant).map_err(|_| INVALID_ARGUMENT)?;
        let active = u32::try_from(active).map_err(|_| INVALID_ARGUMENT)?;
        let mut region = RegionHandle::new(0, 0, 0)?;
        region.factory = Some(fixture::Factory::new(&mut region, dormant, active)?);
        register(region)
    })()
    .unwrap_or_else(i64::from)
}

#[no_mangle]
extern "system" fn Java_probe_NativeBridge_factoryPhase(
    _: Env,
    _: Object,
    handle: i64,
    phase: i32,
) -> i32 {
    with_region(handle, |region| {
        let factory = region.factory.as_mut().ok_or(INVALID_ARGUMENT)?;
        factory.prepare(region.region.as_ptr(), region.last_tick, phase)?;
        Ok(0)
    })
    .unwrap_or_else(|err| err)
}

#[no_mangle]
extern "system" fn Java_probe_NativeBridge_settledKernelNanos(
    _: Env,
    _: Object,
    handle: i64,
    first_tick: i64,
    ticks: i32,
) -> i64 {
    with_region(handle, |region| {
        if first_tick < 0 || ticks <= 0 {
            return Err(INVALID_ARGUMENT);
        }
        let last_tick = first_tick
            .checked_add(i64::from(ticks) - 1)
            .ok_or(INVALID_ARGUMENT)?;
        let start = Instant::now();
        for tick in first_tick as u64..=last_tick as u64 {
            let report = region.step(tick)?;
            if report.processed != 0 || report.has_backlog != 0 {
                return Err(INVALID_ARGUMENT);
            }
        }
        Ok(start.elapsed().as_nanos() as i64)
    })
    .unwrap_or_else(i64::from)
}
