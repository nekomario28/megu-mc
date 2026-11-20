// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later

//! C ABI v1. See `include/mc_spine_client_mesh.h` for the caller contract.

use std::ptr;

pub use crate::packed::PackedVertex;

use crate::{
    column::{mesh_column, ColumnSection, FacePlane},
    mesh_section,
    packed::{depth_key, is_packable, is_translucent, pack_quad, MAX_COLUMN_SECTIONS},
    Block, Direction, Layer, MissingNeighbor, Quad, Sample, SectionView, EDGE_CORNER_CELLS,
    FACE_CELLS, LIGHT_BYTES, SECTION_CELLS,
};

pub const ABI_VERSION: u32 = 1;
pub const ABI_OK: i32 = 0;
pub const ABI_INVALID_ARGUMENT: i32 = 1;
pub const ABI_BUFFER_TOO_SMALL: i32 = 2;
pub const ABI_AIR: u32 = 0;
pub const ABI_OPAQUE: u32 = 1;
pub const ABI_MAX_COLUMN_SECTIONS: usize = MAX_COLUMN_SECTIONS;
pub const ABI_COLUMN_UNIFORM_AIR: u32 = 1;

#[repr(C)]
pub struct MeshInput {
    pub abi_version: u32,
    pub missing: u32,
    pub blocks: *const Block,
    pub block_light: *const u8,
    pub sky_light: *const u8,
    pub borders: [*const Sample; 6],
    pub edges_and_corners: *const Sample,
}

#[no_mangle]
pub extern "C" fn mc_spine_mesh_abi_version() -> u32 {
    ABI_VERSION
}

/// Writes the required quad count, and copies all quads when capacity suffices.
/// A null output with zero capacity queries the required count.
///
/// # Safety
///
/// A non-null input pointer must reference an initialized, aligned `MeshInput`.
/// Its non-null buffers must be aligned, initialized and readable for the fixed
/// lengths in the header for the duration of the call. Non-null output and
/// count pointers must be aligned and writable for their declared lengths and
/// must not overlap each other or the input. Inputs may alias other inputs.
#[no_mangle]
pub unsafe extern "C" fn mc_spine_mesh_section(
    input: *const MeshInput,
    output: *mut Quad,
    capacity: usize,
    count: *mut usize,
) -> i32 {
    if count.is_null() {
        return ABI_INVALID_ARGUMENT;
    }
    // SAFETY: the caller supplies a writable count and, if present, a valid input.
    unsafe { count.write(0) };
    let Some(input) = (unsafe { input.as_ref() }) else {
        return ABI_INVALID_ARGUMENT;
    };
    if input.abi_version != ABI_VERSION || (output.is_null() && capacity != 0) {
        return ABI_INVALID_ARGUMENT;
    }
    let missing = match input.missing {
        ABI_AIR => MissingNeighbor::Air,
        ABI_OPAQUE => MissingNeighbor::Opaque,
        _ => return ABI_INVALID_ARGUMENT,
    };
    // SAFETY: non-null buffers have the fixed array lengths and alignment of
    // SectionView and remain immutable for the call. as_ref only borrows.
    let view = unsafe {
        let (Some(blocks), Some(block_light), Some(sky_light)) = (
            input.blocks.cast::<[Block; SECTION_CELLS]>().as_ref(),
            input.block_light.cast::<[u8; LIGHT_BYTES]>().as_ref(),
            input.sky_light.cast::<[u8; LIGHT_BYTES]>().as_ref(),
        ) else {
            return ABI_INVALID_ARGUMENT;
        };
        SectionView {
            blocks,
            block_light,
            sky_light,
            missing,
            borders: input
                .borders
                .map(|border| border.cast::<[Sample; FACE_CELLS]>().as_ref()),
            edges_and_corners: input
                .edges_and_corners
                .cast::<[Sample; EDGE_CORNER_CELLS]>()
                .as_ref(),
        }
    };
    let quads = mesh_section(&view);
    // SAFETY: caller supplies a separate writable count.
    unsafe { count.write(quads.len()) };
    if output.is_null() {
        return ABI_OK;
    }
    if capacity < quads.len() {
        return ABI_BUFFER_TOO_SMALL;
    }
    // SAFETY: the caller supplies a separate output buffer with enough capacity.
    unsafe { ptr::copy_nonoverlapping(quads.as_ptr(), output, quads.len()) };
    ABI_OK
}

#[repr(C)]
pub struct MeshColumnSection {
    pub flags: u32,
    pub blocks: *const Block,
    pub block_light: *const u8,
    pub sky_light: *const u8,
}

#[repr(C)]
pub struct MeshColumnInput {
    pub abi_version: u32,
    pub missing: u32,
    pub sections: *const MeshColumnSection,
    pub section_count: usize,
    pub borders: [*const Sample; 4],
}

static AIR_BLOCKS: [Block; SECTION_CELLS] = [Block::AIR; SECTION_CELLS];

/// Mesh sections from bottom to top, using a shared column-local origin.
///
/// # Safety
///
/// Non-null buffers must be aligned and valid for the lengths in the header.
/// The input and its buffers must be initialized and readable. Output and count
/// must be writable and disjoint from each other and all input buffers.
#[no_mangle]
pub unsafe extern "C" fn mc_spine_client_mesh_column(
    input: *const MeshColumnInput,
    output: *mut Quad,
    capacity: usize,
    count: *mut usize,
) -> i32 {
    if count.is_null() {
        return ABI_INVALID_ARGUMENT;
    }
    // SAFETY: non-null count is writable and non-null input is initialized.
    unsafe { count.write(0) };
    let Some(input) = (unsafe { input.as_ref() }) else {
        return ABI_INVALID_ARGUMENT;
    };
    if input.abi_version != ABI_VERSION
        || input.sections.is_null()
        || !(1..=MAX_COLUMN_SECTIONS).contains(&input.section_count)
        || (output.is_null() && capacity != 0)
    {
        return ABI_INVALID_ARGUMENT;
    }
    let missing = match input.missing {
        ABI_AIR => MissingNeighbor::Air,
        ABI_OPAQUE => MissingNeighbor::Opaque,
        _ => return ABI_INVALID_ARGUMENT,
    };
    // SAFETY: the header requires section_count readable descriptors and the
    // count is bounded before constructing the slice.
    let inputs = unsafe { std::slice::from_raw_parts(input.sections, input.section_count) };
    let mut sections = Vec::with_capacity(inputs.len());
    for section in inputs {
        let (blocks, uniform_air) = match section.flags {
            // SAFETY: a non-null dense block array has the fixed length and alignment.
            0 => match unsafe { section.blocks.cast::<[Block; SECTION_CELLS]>().as_ref() } {
                Some(blocks) => (blocks, false),
                None => return ABI_INVALID_ARGUMENT,
            },
            ABI_COLUMN_UNIFORM_AIR if section.blocks.is_null() => (&AIR_BLOCKS, true),
            _ => return ABI_INVALID_ARGUMENT,
        };
        // SAFETY: non-null light arrays have the fixed length in the header.
        let (Some(block_light), Some(sky_light)) = (unsafe {
            (
                section.block_light.cast::<[u8; LIGHT_BYTES]>().as_ref(),
                section.sky_light.cast::<[u8; LIGHT_BYTES]>().as_ref(),
            )
        }) else {
            return ABI_INVALID_ARGUMENT;
        };
        sections.push(ColumnSection {
            uniform_air,
            view: SectionView {
                blocks,
                block_light,
                sky_light,
                missing,
                borders: [None; 6],
                edges_and_corners: None,
            },
        });
    }
    let borders = input.borders.map(|border| {
        if border.is_null() {
            None
        } else {
            // SAFETY: each supplied cardinal buffer contains one plane per section.
            Some(unsafe { std::slice::from_raw_parts(border.cast::<FacePlane>(), sections.len()) })
        }
    });
    let quads = mesh_column(&sections, borders);
    // SAFETY: count is writable and separate from the other buffers.
    unsafe { count.write(quads.len()) };
    if output.is_null() {
        return ABI_OK;
    }
    if capacity < quads.len() {
        return ABI_BUFFER_TOO_SMALL;
    }
    // SAFETY: the caller supplies a disjoint, writable output with enough capacity.
    unsafe { ptr::copy_nonoverlapping(quads.as_ptr(), output, quads.len()) };
    ABI_OK
}

// Validate the byte-sized C discriminants before creating a Rust reference to
// any Quad. A C caller can initialize these bytes to values outside the enums.
unsafe fn checked_quads<'a>(quads: *const Quad, len: usize) -> Option<&'a [Quad]> {
    if quads.is_null() || len > isize::MAX as usize / std::mem::size_of::<Quad>() {
        return None;
    }
    for i in 0..len {
        // SAFETY: only read the initialized bytes at the two enum field offsets.
        let (direction, layer) = unsafe {
            let quad = quads.add(i);
            (
                ptr::addr_of!((*quad).direction).cast::<u8>().read(),
                ptr::addr_of!((*quad).layer).cast::<u8>().read(),
            )
        };
        if direction > Direction::East as u8 || layer > Layer::Fluid as u8 {
            return None;
        }
    }
    // SAFETY: the caller supplies initialized fields with the declared length
    // and alignment. Enum values and maximum slice length were checked above.
    Some(unsafe { std::slice::from_raw_parts(quads, len) })
}

/// Pack four vertices per input quad directly into a caller-owned buffer.
///
/// # Safety
///
/// Non-null buffers must be aligned and valid for their stated lengths.
/// Input fields must be initialized and readable. Output and count must be
/// writable and disjoint from the input and each other.
#[no_mangle]
pub unsafe extern "C" fn mc_spine_client_mesh_pack_vertices(
    quads: *const Quad,
    quad_count: usize,
    output: *mut PackedVertex,
    capacity: usize,
    count: *mut usize,
) -> i32 {
    if count.is_null() {
        return ABI_INVALID_ARGUMENT;
    }
    // SAFETY: non-null count is writable.
    unsafe { count.write(0) };
    if output.is_null() && capacity != 0 {
        return ABI_INVALID_ARGUMENT;
    }
    // SAFETY: non-null input is readable for the declared number of quads.
    let Some(quads) = (unsafe { checked_quads(quads, quad_count) }) else {
        return ABI_INVALID_ARGUMENT;
    };
    if !quads.iter().all(is_packable) {
        return ABI_INVALID_ARGUMENT;
    }
    let required = quads.len() * 4;
    // SAFETY: count is writable and separate from input and output.
    unsafe { count.write(required) };
    if output.is_null() {
        return ABI_OK;
    }
    if capacity < required {
        return ABI_BUFFER_TOO_SMALL;
    }
    let output = output.cast::<[PackedVertex; 4]>();
    for (i, quad) in quads.iter().enumerate() {
        // SAFETY: the caller supplies room for four writable vertices per quad.
        unsafe { output.add(i).write(pack_quad(quad)) };
    }
    ABI_OK
}

/// Return translucent quad indices in back-to-front order.
///
/// # Safety
///
/// Non-null buffers must be aligned and valid for their stated lengths.
/// Input fields must be initialized and readable. Output and count must be
/// writable and disjoint from the input and each other.
#[no_mangle]
pub unsafe extern "C" fn mc_spine_client_mesh_sort_translucent(
    quads: *const Quad,
    quad_count: usize,
    camera_x: f32,
    camera_y: f32,
    camera_z: f32,
    output: *mut u32,
    capacity: usize,
    count: *mut usize,
) -> i32 {
    if count.is_null() {
        return ABI_INVALID_ARGUMENT;
    }
    // SAFETY: non-null count is writable.
    unsafe { count.write(0) };
    let camera = [camera_x, camera_y, camera_z];
    if (output.is_null() && capacity != 0)
        || u32::try_from(quad_count).is_err()
        || !camera.into_iter().all(f32::is_finite)
    {
        return ABI_INVALID_ARGUMENT;
    }
    // SAFETY: non-null input is readable for the declared number of quads.
    let Some(quads) = (unsafe { checked_quads(quads, quad_count) }) else {
        return ABI_INVALID_ARGUMENT;
    };
    let mut required = 0;
    for quad in quads.iter().filter(|quad| is_translucent(quad)) {
        if !quad.positions.iter().flatten().all(|n| n.is_finite()) {
            return ABI_INVALID_ARGUMENT;
        }
        required += 1;
    }
    // SAFETY: count is writable and separate from input and output.
    unsafe { count.write(required) };
    if output.is_null() {
        return ABI_OK;
    }
    if capacity < required {
        return ABI_BUFFER_TOO_SMALL;
    }
    let mut order = Vec::with_capacity(required);
    for (i, quad) in quads.iter().enumerate() {
        if is_translucent(quad) {
            order.push((depth_key(quad, camera), i as u32));
        }
    }
    order.sort_unstable_by(|(left, i), (right, j)| {
        right
            .partial_cmp(left)
            .expect("finite depths")
            .then(i.cmp(j))
    });
    for (slot, (_, i)) in order.into_iter().enumerate() {
        // SAFETY: output has enough room for every translucent index.
        unsafe { output.add(slot).write(i) };
    }
    ABI_OK
}
