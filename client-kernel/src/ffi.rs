// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! The C pointer and lifetime contract is declared in include/mc_spine_kernel.h.

use crate::{
    Aabb, CommandDispatcher, Error, FluidCell, FluidKind, FluidQuery, PalettedSection, Suggestions,
    SECTION_CELLS,
};
use std::{
    ffi::c_void,
    mem::size_of,
    panic::{catch_unwind, AssertUnwindSafe},
    ptr, slice, str,
};

#[repr(C)]
pub struct Bytes {
    data: *const u8,
    len: u64,
}

impl From<Vec<u8>> for Bytes {
    fn from(value: Vec<u8>) -> Self {
        let data = Box::into_raw(value.into_boxed_slice());
        Self {
            data: data.cast::<u8>(),
            len: data.len() as u64,
        }
    }
}

#[repr(C)]
pub struct CommandParse {
    cursor: u64,
    executable: u8,
}

#[repr(C)]
pub struct SuggestionsInfo {
    start: u64,
    end: u64,
    count: u64,
}

#[repr(C)]
pub struct Text {
    data: *const u8,
    len: u64,
}

#[repr(C)]
pub struct Collision {
    delta: [f64; 3],
    flags: u8,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct FluidInput {
    velocity: [f64; 3],
    kind: u8,
    flags: u8,
}

#[repr(C)]
#[derive(Default)]
pub struct FluidFacts {
    kind: u8,
    amount: u8,
    flags: u8,
    sturdy_faces: u8,
}

#[repr(C)]
pub struct FluidImpulse {
    push: [f64; 3],
    max_height: f64,
    in_fluid: u8,
}

type FluidLookup = unsafe extern "C" fn(*mut c_void, i32, i32, i32, *mut FluidFacts) -> i32;

fn status(op: impl FnOnce() -> Result<(), Error>) -> i32 {
    match catch_unwind(AssertUnwindSafe(op)) {
        Ok(Ok(())) => 0,
        Ok(Err(error)) => error as i32,
        Err(_) => Error::Panic as i32,
    }
}

fn check_ptr<T>(p: *const T) -> Result<(), Error> {
    if p.is_null() {
        return Err(Error::Null);
    }
    if !p.is_aligned() {
        return Err(Error::InvalidArgument);
    }
    Ok(())
}

unsafe fn input_span<'a, T>(p: *const T, len: usize) -> Result<&'a [T], Error> {
    check_ptr(p)?;
    if len > isize::MAX as usize / size_of::<T>() {
        return Err(Error::OutOfRange);
    }
    Ok(unsafe { slice::from_raw_parts(p, len) })
}

unsafe fn text_input<'a>(p: *const u8, len: usize) -> Result<&'a str, Error> {
    str::from_utf8(unsafe { input_span(p, len)? }).map_err(|_| Error::InvalidUtf8)
}

unsafe fn read_handle<'a, T>(p: *const T) -> Result<&'a T, Error> {
    check_ptr(p)?;
    Ok(unsafe { &*p })
}

unsafe fn drop_handle<T>(p: *mut T) -> i32 {
    status(|| {
        check_ptr(p)?;
        drop(unsafe { Box::from_raw(p) });
        Ok(())
    })
}

unsafe fn write_result<T>(out: *mut T, value: impl FnOnce() -> Result<T, Error>) -> i32 {
    status(|| {
        check_ptr(out)?;
        let value = value()?;
        unsafe { out.write(value) };
        Ok(())
    })
}

#[no_mangle]
pub extern "C" fn mc_spine_kernel_abi_version() -> u32 {
    crate::ABI_VERSION
}

#[no_mangle]
pub unsafe extern "C" fn mc_spine_kernel_bytes_free(bytes: Bytes) -> i32 {
    status(|| {
        check_ptr(bytes.data)?;
        let len = usize::try_from(bytes.len).map_err(|_| Error::OutOfRange)?;
        if len > isize::MAX as usize {
            return Err(Error::OutOfRange);
        }
        let data = ptr::slice_from_raw_parts_mut(bytes.data.cast_mut(), len);
        drop(unsafe { Box::from_raw(data) });
        Ok(())
    })
}

#[no_mangle]
pub unsafe extern "C" fn mc_spine_kernel_palette_new_single(
    state: u32,
    out: *mut *mut PalettedSection,
) -> i32 {
    unsafe {
        write_result(out, || {
            Ok(Box::into_raw(Box::new(PalettedSection::new_single(state)?)))
        })
    }
}

#[no_mangle]
pub unsafe extern "C" fn mc_spine_kernel_palette_free(section: *mut PalettedSection) -> i32 {
    unsafe { drop_handle(section) }
}

#[no_mangle]
pub unsafe extern "C" fn mc_spine_kernel_palette_get(
    section: *const PalettedSection,
    index: u32,
    out: *mut u32,
) -> i32 {
    unsafe { write_result(out, || read_handle(section)?.get(index as usize)) }
}

#[no_mangle]
pub unsafe extern "C" fn mc_spine_kernel_palette_set(
    section: *mut PalettedSection,
    index: u32,
    state: u32,
) -> i32 {
    status(|| {
        check_ptr(section)?;
        unsafe { &mut *section }.set(index as usize, state)
    })
}

#[no_mangle]
pub unsafe extern "C" fn mc_spine_kernel_palette_non_air(
    section: *const PalettedSection,
    out: *mut u16,
) -> i32 {
    unsafe { write_result(out, || Ok(read_handle(section)?.non_air())) }
}

#[no_mangle]
pub unsafe extern "C" fn mc_spine_kernel_palette_is_uniform(
    section: *const PalettedSection,
    out: *mut u8,
) -> i32 {
    unsafe { write_result(out, || Ok(read_handle(section)?.is_uniform().into())) }
}

#[no_mangle]
pub unsafe extern "C" fn mc_spine_kernel_palette_uniform_state(
    section: *const PalettedSection,
    out_present: *mut u8,
    out_state: *mut u32,
) -> i32 {
    unsafe {
        write_result(out_present, || {
            check_ptr(out_state)?;
            let state = read_handle(section)?.uniform_state();
            out_state.write(state.unwrap_or(0));
            Ok(state.is_some().into())
        })
    }
}

#[no_mangle]
pub unsafe extern "C" fn mc_spine_kernel_palette_bits(
    section: *const PalettedSection,
    out: *mut u8,
) -> i32 {
    unsafe { write_result(out, || Ok(read_handle(section)?.palette_bits())) }
}

#[no_mangle]
pub unsafe extern "C" fn mc_spine_kernel_palette_unpack_into(
    section: *const PalettedSection,
    out: *mut u32,
    capacity: usize,
) -> i32 {
    unsafe {
        write_result(out.cast::<[u32; SECTION_CELLS]>(), || {
            let section = read_handle(section)?;
            if capacity < SECTION_CELLS {
                return Err(Error::BufferTooSmall);
            }
            let mut states = [0; SECTION_CELLS];
            section.unpack_into(&mut states);
            Ok(states)
        })
    }
}

#[no_mangle]
pub unsafe extern "C" fn mc_spine_kernel_palette_write_block_states_wire(
    section: *const PalettedSection,
    out: *mut Bytes,
) -> i32 {
    unsafe {
        write_result(out, || {
            let mut bytes = Vec::new();
            read_handle(section)?.write_block_states_wire(&mut bytes);
            Ok(bytes.into())
        })
    }
}

#[no_mangle]
pub unsafe extern "C" fn mc_spine_kernel_nbt_read_named_validate(
    input: *const u8,
    len: usize,
) -> i32 {
    status(|| crate::nbt_read_named_validate(unsafe { input_span(input, len)? }))
}

#[no_mangle]
pub unsafe extern "C" fn mc_spine_kernel_nbt_read_gzip_validate(
    input: *const u8,
    len: usize,
    max_uncompressed: usize,
) -> i32 {
    status(|| crate::nbt_read_gzip_validate(unsafe { input_span(input, len)? }, max_uncompressed))
}

#[no_mangle]
pub unsafe extern "C" fn mc_spine_kernel_nbt_roundtrip_named(
    input: *const u8,
    len: usize,
    out: *mut Bytes,
) -> i32 {
    unsafe {
        write_result(out, || {
            Ok(crate::nbt_roundtrip_named(input_span(input, len)?)?.into())
        })
    }
}

#[no_mangle]
pub unsafe extern "C" fn mc_spine_kernel_dispatcher_new(out: *mut *mut CommandDispatcher) -> i32 {
    unsafe {
        write_result(out, || {
            Ok(Box::into_raw(Box::new(CommandDispatcher::new())))
        })
    }
}

#[no_mangle]
pub unsafe extern "C" fn mc_spine_kernel_dispatcher_free(
    dispatcher: *mut CommandDispatcher,
) -> i32 {
    unsafe { drop_handle(dispatcher) }
}

#[no_mangle]
pub unsafe extern "C" fn mc_spine_kernel_dispatcher_register_literal(
    dispatcher: *mut CommandDispatcher,
    literal: *const u8,
    len: usize,
) -> i32 {
    status(|| {
        check_ptr(dispatcher)?;
        let literal = unsafe { text_input(literal, len)? };
        unsafe { &mut *dispatcher }.register_literal(literal)
    })
}

#[no_mangle]
pub unsafe extern "C" fn mc_spine_kernel_dispatcher_parse(
    dispatcher: *const CommandDispatcher,
    input: *const u8,
    len: usize,
    out: *mut CommandParse,
) -> i32 {
    unsafe {
        write_result(out, || {
            let result = read_handle(dispatcher)?.parse(text_input(input, len)?);
            Ok(CommandParse {
                cursor: result.cursor as u64,
                executable: result.executable.into(),
            })
        })
    }
}

#[no_mangle]
pub unsafe extern "C" fn mc_spine_kernel_dispatcher_complete_suggestions(
    dispatcher: *const CommandDispatcher,
    input: *const u8,
    len: usize,
    cursor: usize,
    out: *mut *mut Suggestions,
) -> i32 {
    unsafe {
        write_result(out, || {
            let suggestions =
                read_handle(dispatcher)?.complete_suggestions(text_input(input, len)?, cursor)?;
            Ok(Box::into_raw(Box::new(suggestions)))
        })
    }
}

#[no_mangle]
pub unsafe extern "C" fn mc_spine_kernel_suggestions_info(
    suggestions: *const Suggestions,
    out: *mut SuggestionsInfo,
) -> i32 {
    unsafe {
        write_result(out, || {
            let suggestions = read_handle(suggestions)?;
            let range = suggestions.range();
            Ok(SuggestionsInfo {
                start: range.start as u64,
                end: range.end as u64,
                count: suggestions.len() as u64,
            })
        })
    }
}

#[no_mangle]
pub unsafe extern "C" fn mc_spine_kernel_suggestions_get(
    suggestions: *const Suggestions,
    index: usize,
    out: *mut Text,
) -> i32 {
    unsafe {
        write_result(out, || {
            let text = read_handle(suggestions)?
                .get(index)
                .ok_or(Error::OutOfRange)?;
            Ok(Text {
                data: text.as_ptr(),
                len: text.len() as u64,
            })
        })
    }
}

#[no_mangle]
pub unsafe extern "C" fn mc_spine_kernel_suggestions_free(suggestions: *mut Suggestions) -> i32 {
    unsafe { drop_handle(suggestions) }
}

#[no_mangle]
pub unsafe extern "C" fn mc_spine_kernel_collide(
    aabb: *const Aabb,
    delta: *const [f64; 3],
    on_ground: u8,
    max_up_step: f32,
    shapes: *const Aabb,
    count: usize,
    out: *mut Collision,
) -> i32 {
    unsafe {
        write_result(out, || {
            let aabb = *read_handle(aabb)?;
            let delta = *read_handle(delta)?;
            let on_ground = match on_ground {
                0 => false,
                1 => true,
                _ => return Err(Error::InvalidArgument),
            };
            let shapes = if count == 0 {
                &[]
            } else {
                input_span(shapes, count)?
            };
            let result = crate::collide(aabb, delta, on_ground, max_up_step, shapes)?;
            Ok(Collision {
                delta: result.delta,
                flags: u8::from(result.horizontal_collision)
                    | (u8::from(result.vertical_collision) << 1)
                    | (u8::from(result.vertical_collision_below) << 2)
                    | (u8::from(result.stepped_up) << 3),
            })
        })
    }
}

fn fluid_kind(kind: u8) -> Result<FluidKind, Error> {
    match kind {
        1 => Ok(FluidKind::Water),
        2 => Ok(FluidKind::Lava),
        _ => Err(Error::InvalidArgument),
    }
}

impl TryFrom<FluidFacts> for FluidCell {
    type Error = Error;

    fn try_from(facts: FluidFacts) -> Result<Self, Error> {
        if facts.flags & !0x07 != 0 {
            return Err(Error::InvalidArgument);
        }
        Ok(Self {
            kind: if facts.kind == 0 {
                None
            } else {
                Some(fluid_kind(facts.kind)?)
            },
            amount: facts.amount,
            falling: facts.flags & 1 != 0,
            blocks_motion: facts.flags & 2 != 0,
            is_ice: facts.flags & 4 != 0,
            sturdy_faces: facts.sturdy_faces,
        })
    }
}

#[no_mangle]
pub unsafe extern "C" fn mc_spine_kernel_fluid_impulse(
    aabb: *const Aabb,
    input: *const FluidInput,
    lookup: Option<FluidLookup>,
    context: *mut c_void,
    out: *mut FluidImpulse,
) -> i32 {
    unsafe {
        write_result(out, || {
            let aabb = *read_handle(aabb)?;
            let input = *read_handle(input)?;
            let lookup = lookup.ok_or(Error::Null)?;
            if input.flags & !0x07 != 0 {
                return Err(Error::InvalidArgument);
            }
            let query = FluidQuery {
                kind: fluid_kind(input.kind)?,
                ultra_warm: input.flags & 1 != 0,
                pushed_by_fluid: input.flags & 2 != 0,
                is_player: input.flags & 4 != 0,
                velocity: input.velocity,
            };
            let result = crate::fluid_impulse(aabb, query, |x, y, z| {
                let mut facts = FluidFacts::default();
                if lookup(context, x, y, z, &mut facts) != 0 {
                    return Err(Error::QueryFailed);
                }
                facts.try_into()
            })?;
            Ok(FluidImpulse {
                push: result.push,
                max_height: result.max_height,
                in_fluid: result.in_fluid.into(),
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_stays_untouched_on_panic_and_validation_errors() {
        let mut output = 0x1234u32;
        unsafe {
            assert_eq!(
                write_result(&mut output, || panic!("injected unwind")),
                Error::Panic as i32
            );
            assert_eq!(
                mc_spine_kernel_palette_get(ptr::null(), 0, &mut output),
                Error::Null as i32
            );
        }
        assert_eq!(output, 0x1234);
    }

    #[test]
    fn spans_reject_null_misalignment_and_oversized_lengths() {
        let storage = [0u32; 2];
        // These addresses must be rejected before any memory access.
        let unaligned = storage.as_ptr().cast::<u8>().wrapping_add(1).cast::<u32>();
        unsafe {
            assert_eq!(input_span::<u8>(ptr::null(), 0), Err(Error::Null));
            assert_eq!(input_span(unaligned, 1), Err(Error::InvalidArgument));
            assert_eq!(
                input_span(storage.as_ptr(), isize::MAX as usize / 4 + 1),
                Err(Error::OutOfRange)
            );
        }
    }
}
