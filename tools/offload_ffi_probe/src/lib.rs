use std::slice;

const MAGIC: u32 = 0x3546_464f; // OFF5, little endian
const HEADER: usize = 24;
const MAX_FRAME: usize = 64 * 1024;
const MAX_RECORDS: usize = 4096;

#[repr(C)]
pub struct JNINativeInterface {
    _reserved: [*const (); 230],
    get_direct_buffer_address: unsafe extern "system" fn(*mut *const JNINativeInterface, *mut core::ffi::c_void) -> *mut core::ffi::c_void,
    get_direct_buffer_capacity: unsafe extern "system" fn(*mut *const JNINativeInterface, *mut core::ffi::c_void) -> isize,
}

fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &byte in bytes {
        crc ^= byte as u32;
        for _ in 0..8 { crc = (crc >> 1) ^ (0xedb8_8320 & (0u32.wrapping_sub(crc & 1))); }
    }
    !crc
}

fn u16at(b: &[u8], i: usize) -> usize { u16::from_le_bytes([b[i], b[i + 1]]) as usize }
fn u32at(b: &[u8], i: usize) -> u32 { u32::from_le_bytes(b[i..i + 4].try_into().unwrap()) }
fn u64at(b: &[u8], i: usize) -> u64 { u64::from_le_bytes(b[i..i + 8].try_into().unwrap()) }
fn f64at(b: &[u8], i: usize) -> f64 { f64::from_bits(u64at(b, i)) }

unsafe fn validate(frame: &[u8]) -> i32 {
    if frame.len() < HEADER || frame.len() > MAX_FRAME || u32at(frame, 0) != MAGIC || frame[4] != 1 { return -1; }
    let scenario = frame[5];
    let count = u16at(frame, 6);
    let generation = u64at(frame, 8);
    let payload_len = u32at(frame, 16) as usize;
    let expected_crc = u32at(frame, 20);
    if count > MAX_RECORDS || payload_len != frame.len() - HEADER || crc32(&frame[HEADER..]) != expected_crc { return -2; }
    let stride = match scenario { 1 => 8, 2 => 52, 3 => 24, _ => return -3 };
    let base_len = count.checked_mul(stride).ok_or(());
    if scenario == 2 {
        if count != 4 || base_len.is_err() || payload_len < base_len.unwrap()
            || payload_len - base_len.unwrap() > 192 || (payload_len - base_len.unwrap()) % 6 != 0 { return -4; }
    } else if base_len != Ok(payload_len) { return -4; }
    if scenario == 3 && (count != 1 || !f32::from_bits(u32at(frame, HEADER)).is_finite()
        || !f32::from_bits(u32at(frame, HEADER + 4)).is_finite()
        || u32at(frame, HEADER + 8) > 4096
        || !(0..3).all(|i| f32::from_bits(u32at(frame, HEADER + 12 + i * 4)).is_finite())) { return -7; }
    if scenario == 1 {
        for r in frame[HEADER..].chunks_exact(8) {
            if u16at(r, 0) >= 2000 || r[2] > 3 || r[3] & !1 != 0
                || !f32::from_bits(u32at(r, 4)).is_finite() { return -8; }
        }
    } else if scenario == 2 {
        for r in frame[HEADER..HEADER + 4 * 52].chunks_exact(52) {
            if !(0..3).all(|i| f64at(r, 8 + i * 8).is_finite())
                || !(0..5).all(|i| f32::from_bits(u32at(r, 32 + i * 4)).is_finite()) { return -8; }
        }
        for r in frame[HEADER + 4 * 52..].chunks_exact(6) {
            if u16at(r, 0) >= 1024 || u16at(r, 2) >= 64 || u16at(r, 4) != 1 { return -8; }
        }
    }
    let slots = LAST_GENERATION.get();
    let slot = (scenario - 1) as usize;
    let last = slots[slot];
    if generation <= last { return -6; }
    let mut updated = slots;
    updated[slot] = generation;
    LAST_GENERATION.set(updated);
    count as i32
}

thread_local! { static LAST_GENERATION: std::cell::Cell<[u64; 3]> = const { std::cell::Cell::new([0, 0, 0]) }; }

#[no_mangle]
pub unsafe extern "system" fn Java_probe_NativeBridge_transfer(env: *mut *const JNINativeInterface, _this: *mut core::ffi::c_void, buffer: *mut core::ffi::c_void) -> i32 {
    if env.is_null() || buffer.is_null() { return -10; }
    let table = *env;
    if table.is_null() { return -10; }
    let cap = ((*table).get_direct_buffer_capacity)(env, buffer);
    if cap < HEADER as isize || cap as usize > MAX_FRAME { return -11; }
    let ptr = ((*table).get_direct_buffer_address)(env, buffer);
    if ptr.is_null() { return -12; }
    let bytes = slice::from_raw_parts(ptr as *const u8, cap as usize);
    let length = u32at(bytes, 16) as usize;
    if length > cap as usize - HEADER { return -13; }
    validate(&bytes[..HEADER + length])
}
