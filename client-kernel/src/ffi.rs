// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! The C pointer and lifetime contract is declared in include/mc_spine_kernel.h.

use crate::{
    Aabb, BlockFacts, CommandDispatcher, Error, FluidCell, FluidKind, FluidQuery, LightEdit,
    LightEngine, LightFact, LightKind, LightUpdate, PalettedSection, Registry, Suggestions,
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

#[repr(C)]
#[derive(Clone, Copy)]
pub struct LightBlock {
    opacity: u8,
    emission: u8,
    occlusion: [u8; 6],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct LightEditRecord {
    x: i32,
    y: i32,
    z: i32,
    block: LightBlock,
}

#[repr(C)]
pub struct LightUpdateInfo {
    chunk_count: u64,
    invalidated_cells: u64,
}

#[repr(C)]
pub struct LightChunkInfo {
    chunk_x: i32,
    chunk_z: i32,
    sky_mask: u64,
    empty_sky_mask: u64,
    block_mask: u64,
    empty_block_mask: u64,
    sky_count: u64,
    block_count: u64,
}

#[repr(C)]
pub struct LightSection {
    section_index: u32,
    data: *const u8,
}

impl From<LightBlock> for LightFact {
    fn from(block: LightBlock) -> Self {
        Self {
            opacity: block.opacity,
            emission: block.emission,
            occlusion: block.occlusion,
        }
    }
}

fn light_kind(kind: u8) -> Result<LightKind, Error> {
    match kind {
        0 => Ok(LightKind::Sky),
        1 => Ok(LightKind::Block),
        _ => Err(Error::InvalidArgument),
    }
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
pub unsafe extern "C" fn mc_spine_kernel_palette_decode_block_states_wire(
    input: *const u8,
    len: usize,
    out_consumed: *mut usize,
    out: *mut *mut PalettedSection,
) -> i32 {
    status(|| {
        check_ptr(out_consumed)?;
        check_ptr(out)?;
        let (section, consumed) =
            PalettedSection::decode_block_states_wire(unsafe { input_span(input, len)? })?;
        unsafe {
            out_consumed.write(consumed);
            out.write(Box::into_raw(Box::new(section)));
        }
        Ok(())
    })
}

#[no_mangle]
pub unsafe extern "C" fn mc_spine_kernel_light_new(
    min_y: i32,
    height: u32,
    has_skylight: u8,
    out: *mut *mut LightEngine,
) -> i32 {
    unsafe {
        write_result(out, || {
            if has_skylight > 1 {
                return Err(Error::InvalidArgument);
            }
            let engine = LightEngine::new(min_y, height, has_skylight == 1)?;
            Ok(Box::into_raw(Box::new(engine)))
        })
    }
}

#[no_mangle]
pub unsafe extern "C" fn mc_spine_kernel_light_free(engine: *mut LightEngine) -> i32 {
    unsafe { drop_handle(engine) }
}

#[no_mangle]
pub unsafe extern "C" fn mc_spine_kernel_light_load_chunk(
    engine: *mut LightEngine,
    chunk_x: i32,
    chunk_z: i32,
    cells: *const LightBlock,
    count: usize,
) -> i32 {
    status(|| {
        check_ptr(engine)?;
        let cells: Vec<LightFact> = unsafe { input_span(cells, count)? }
            .iter()
            .map(|&block| block.into())
            .collect();
        unsafe { &mut *engine }.load_chunk(chunk_x, chunk_z, &cells)
    })
}

#[no_mangle]
pub unsafe extern "C" fn mc_spine_kernel_light_apply_edit(
    engine: *mut LightEngine,
    edits: *const LightEditRecord,
    count: usize,
    out: *mut *mut LightUpdate,
) -> i32 {
    unsafe {
        write_result(out, || {
            check_ptr(engine)?;
            let edits: Vec<LightEdit> = input_span(edits, count)?
                .iter()
                .map(|edit| LightEdit {
                    x: edit.x,
                    y: edit.y,
                    z: edit.z,
                    fact: edit.block.into(),
                })
                .collect();
            Ok(Box::into_raw(Box::new((&mut *engine).apply_edits(&edits)?)))
        })
    }
}

#[no_mangle]
pub unsafe extern "C" fn mc_spine_kernel_light_get_section(
    engine: *const LightEngine,
    chunk_x: i32,
    chunk_z: i32,
    kind: u8,
    section_index: u32,
    out: *mut u8,
    capacity: usize,
) -> i32 {
    unsafe {
        write_result(out.cast::<[u8; crate::LIGHT_SECTION_BYTES]>(), || {
            let engine = read_handle(engine)?;
            let kind = light_kind(kind)?;
            if capacity < crate::LIGHT_SECTION_BYTES {
                return Err(Error::BufferTooSmall);
            }
            engine.section(chunk_x, chunk_z, kind, section_index as usize)
        })
    }
}

#[no_mangle]
pub unsafe extern "C" fn mc_spine_kernel_light_update_info(
    update: *const LightUpdate,
    out: *mut LightUpdateInfo,
) -> i32 {
    unsafe {
        write_result(out, || {
            let update = read_handle(update)?;
            Ok(LightUpdateInfo {
                chunk_count: update.chunks.len() as u64,
                invalidated_cells: update.invalidated_cells as u64,
            })
        })
    }
}

#[no_mangle]
pub unsafe extern "C" fn mc_spine_kernel_light_update_chunk(
    update: *const LightUpdate,
    index: usize,
    out: *mut LightChunkInfo,
) -> i32 {
    unsafe {
        write_result(out, || {
            let chunk = read_handle(update)?
                .chunks
                .get(index)
                .ok_or(Error::OutOfRange)?;
            Ok(LightChunkInfo {
                chunk_x: chunk.chunk.0,
                chunk_z: chunk.chunk.1,
                sky_mask: chunk.sky_mask,
                empty_sky_mask: chunk.empty_sky_mask,
                block_mask: chunk.block_mask,
                empty_block_mask: chunk.empty_block_mask,
                sky_count: chunk.sky_sections.len() as u64,
                block_count: chunk.block_sections.len() as u64,
            })
        })
    }
}

#[no_mangle]
pub unsafe extern "C" fn mc_spine_kernel_light_update_section(
    update: *const LightUpdate,
    chunk_index: usize,
    kind: u8,
    ordinal: usize,
    out: *mut LightSection,
) -> i32 {
    unsafe {
        write_result(out, || {
            let chunk = read_handle(update)?
                .chunks
                .get(chunk_index)
                .ok_or(Error::OutOfRange)?;
            let sections = match light_kind(kind)? {
                LightKind::Sky => &chunk.sky_sections,
                LightKind::Block => &chunk.block_sections,
            };
            let (index, data) = sections.get(ordinal).ok_or(Error::OutOfRange)?;
            Ok(LightSection {
                section_index: *index as u32,
                data: data.as_ptr(),
            })
        })
    }
}

#[no_mangle]
pub unsafe extern "C" fn mc_spine_kernel_light_update_free(update: *mut LightUpdate) -> i32 {
    unsafe { drop_handle(update) }
}

#[no_mangle]
pub unsafe extern "C" fn mc_spine_kernel_registry_open_json(
    states_json: *const u8,
    states_len: usize,
    blocks_json: *const u8,
    blocks_len: usize,
    out: *mut *mut Registry,
) -> i32 {
    unsafe {
        write_result(out, || {
            let registry = Registry::open_json(
                input_span(states_json, states_len)?,
                input_span(blocks_json, blocks_len)?,
            )?;
            Ok(Box::into_raw(Box::new(registry)))
        })
    }
}

/// Opens a registry from the raw buffers of any version pack. `states_json` may
/// be NULL to skip the optional `block_states.json` table.
#[no_mangle]
pub unsafe extern "C" fn mc_spine_kernel_registry_open_parts(
    blocks_json: *const u8,
    blocks_len: usize,
    states_json: *const u8,
    states_len: usize,
    facts_bin: *const u8,
    facts_len: usize,
    out: *mut *mut Registry,
) -> i32 {
    unsafe {
        write_result(out, || {
            let states = match states_json.is_null() {
                true => None,
                false => Some(input_span(states_json, states_len)?),
            };
            let registry = Registry::from_parts(
                input_span(blocks_json, blocks_len)?,
                states,
                input_span(facts_bin, facts_len)?,
            )?;
            Ok(Box::into_raw(Box::new(registry)))
        })
    }
}

/// Opens a registry from a version pack directory. `path` is a UTF-8 span.
#[no_mangle]
pub unsafe extern "C" fn mc_spine_kernel_registry_open_dir(
    path: *const u8,
    path_len: usize,
    out: *mut *mut Registry,
) -> i32 {
    unsafe {
        write_result(out, || {
            let dir = std::path::Path::new(text_input(path, path_len)?);
            Ok(Box::into_raw(Box::new(Registry::from_version_dir(dir)?)))
        })
    }
}

#[no_mangle]
pub unsafe extern "C" fn mc_spine_kernel_registry_free(registry: *mut Registry) -> i32 {
    unsafe { drop_handle(registry) }
}

#[no_mangle]
pub unsafe extern "C" fn mc_spine_kernel_registry_state_count(
    registry: *const Registry,
    out: *mut u32,
) -> i32 {
    unsafe {
        write_result(out, || {
            u32::try_from(read_handle(registry)?.state_count()).map_err(|_| Error::OutOfRange)
        })
    }
}

#[no_mangle]
pub unsafe extern "C" fn mc_spine_kernel_registry_state_name(
    registry: *const Registry,
    state: u32,
    out: *mut Text,
) -> i32 {
    unsafe {
        write_result(out, || {
            let name = read_handle(registry)?.state_name(state)?;
            Ok(Text {
                data: name.as_ptr(),
                len: name.len() as u64,
            })
        })
    }
}

#[no_mangle]
pub unsafe extern "C" fn mc_spine_kernel_registry_default_state(
    registry: *const Registry,
    name: *const u8,
    len: usize,
    out: *mut u32,
) -> i32 {
    unsafe {
        write_result(out, || {
            read_handle(registry)?.default_state(text_input(name, len)?)
        })
    }
}

#[no_mangle]
pub unsafe extern "C" fn mc_spine_kernel_registry_state_facts(
    registry: *const Registry,
    state: u32,
    out: *mut BlockFacts,
) -> i32 {
    unsafe { write_result(out, || read_handle(registry)?.facts(state)) }
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

    fn read_asset(name: &str) -> Vec<u8> {
        std::fs::read(format!("{}/../assets/{name}", env!("CARGO_MANIFEST_DIR"))).unwrap()
    }

    #[test]
    fn registry_open_parts_roundtrips_the_pinned_pack() {
        let blocks = read_asset("blocks_1211.json");
        let states = read_asset("block_states_1211.json");
        let facts = read_asset("block_state_facts_1211.bin");
        let mut registry: *mut Registry = ptr::null_mut();
        let mut count = 0u32;
        unsafe {
            assert_eq!(
                mc_spine_kernel_registry_open_parts(
                    ptr::null(),
                    0,
                    states.as_ptr(),
                    states.len(),
                    facts.as_ptr(),
                    facts.len(),
                    &mut registry
                ),
                Error::Null as i32
            );
            assert_eq!(
                mc_spine_kernel_registry_open_parts(
                    blocks.as_ptr(),
                    blocks.len(),
                    states.as_ptr(),
                    states.len(),
                    facts.as_ptr(),
                    facts.len(),
                    ptr::null_mut()
                ),
                Error::Null as i32
            );
            // A truncated facts table fails closed before any table is built.
            assert_eq!(
                mc_spine_kernel_registry_open_parts(
                    blocks.as_ptr(),
                    blocks.len(),
                    states.as_ptr(),
                    states.len(),
                    facts.as_ptr(),
                    facts.len() - 1,
                    &mut registry
                ),
                Error::InvalidJson as i32
            );
            assert!(registry.is_null());
            // With the optional states table.
            assert_eq!(
                mc_spine_kernel_registry_open_parts(
                    blocks.as_ptr(),
                    blocks.len(),
                    states.as_ptr(),
                    states.len(),
                    facts.as_ptr(),
                    facts.len(),
                    &mut registry
                ),
                0
            );
            assert_eq!(
                mc_spine_kernel_registry_state_count(registry, &mut count),
                0
            );
            assert_eq!(count as usize, crate::STATE_COUNT as usize);
            assert_eq!(mc_spine_kernel_registry_free(registry), 0);
            // Without it, `states_json` may be NULL and `states_len` is ignored.
            registry = ptr::null_mut();
            assert_eq!(
                mc_spine_kernel_registry_open_parts(
                    blocks.as_ptr(),
                    blocks.len(),
                    ptr::null(),
                    0,
                    facts.as_ptr(),
                    facts.len(),
                    &mut registry
                ),
                0
            );
            assert_eq!(
                mc_spine_kernel_registry_state_count(registry, &mut count),
                0
            );
            assert_eq!(count as usize, crate::STATE_COUNT as usize);
            assert_eq!(mc_spine_kernel_registry_free(registry), 0);
        }
    }

    #[test]
    fn registry_open_dir_opens_a_pack_and_fails_closed() {
        let assets = concat!(env!("CARGO_MANIFEST_DIR"), "/../assets");
        let missing = "definitely/not/a/pack/dir";
        let mut registry: *mut Registry = ptr::null_mut();
        let mut count = 0u32;
        unsafe {
            assert_eq!(
                mc_spine_kernel_registry_open_dir(assets.as_ptr(), assets.len(), &mut registry),
                0
            );
            assert_eq!(
                mc_spine_kernel_registry_state_count(registry, &mut count),
                0
            );
            assert_eq!(count as usize, crate::STATE_COUNT as usize);
            assert_eq!(mc_spine_kernel_registry_free(registry), 0);
            registry = ptr::null_mut();
            assert_eq!(
                mc_spine_kernel_registry_open_dir(missing.as_ptr(), missing.len(), &mut registry),
                Error::NotFound as i32
            );
            assert!(registry.is_null());
            assert_eq!(
                mc_spine_kernel_registry_open_dir([0xff].as_ptr(), 1, &mut registry),
                Error::InvalidUtf8 as i32
            );
            assert_eq!(
                mc_spine_kernel_registry_open_dir(assets.as_ptr(), assets.len(), ptr::null_mut()),
                Error::Null as i32
            );
        }
    }
}
