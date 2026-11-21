// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later

//! C ABI v1. See `include/mc_spine_client_mesh.h` for the caller contract.

use std::mem::MaybeUninit;
use std::ptr;

pub use crate::packed::PackedVertex;

use crate::{
    column::{mesh_column_into, ColumnSection, FacePlane},
    mesh::{mesh_section_with_scratch, with_scratch, CountSink},
    packed::{depth_key, is_packable, is_translucent, pack_quad, MAX_COLUMN_SECTIONS},
    Block, Direction, DirectionBuckets, Layer, MissingNeighbor, Quad, Sample, SectionView,
    EDGE_CORNER_CELLS, FACE_CELLS, LIGHT_BYTES, SECTION_CELLS,
};

pub const ABI_VERSION: u32 = 1;
pub const ABI_OK: i32 = 0;
pub const ABI_INVALID_ARGUMENT: i32 = 1;
pub const ABI_BUFFER_TOO_SMALL: i32 = 2;
pub const ABI_AIR: u32 = 0;
pub const ABI_OPAQUE: u32 = 1;
pub const ABI_MAX_COLUMN_SECTIONS: usize = MAX_COLUMN_SECTIONS;
pub const ABI_COLUMN_UNIFORM_AIR: u32 = 1;
pub const ABI_COLUMN_SEALED_SOLID: u32 = 2;

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
    with_scratch(|scratch| {
        if output.is_null() {
            let mut sink = CountSink(0);
            mesh_section_with_scratch(&view, scratch, 0.0, &mut sink);
            // SAFETY: caller supplies a separate writable count.
            unsafe { count.write(sink.0) };
            return ABI_OK;
        }
        let mut quads = std::mem::take(&mut scratch.quads);
        quads.clear();
        mesh_section_with_scratch(&view, scratch, 0.0, &mut quads);
        let len = quads.len();
        // SAFETY: caller supplies a separate writable count.
        unsafe { count.write(len) };
        let status = if capacity < len {
            ABI_BUFFER_TOO_SMALL
        } else {
            // SAFETY: the caller supplies a separate output buffer with enough capacity.
            unsafe { ptr::copy_nonoverlapping(quads.as_ptr(), output, len) };
            ABI_OK
        };
        scratch.quads = quads;
        status
    })
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
    let mut section_slots: [MaybeUninit<ColumnSection<'_>>; MAX_COLUMN_SECTIONS] =
        [const { MaybeUninit::uninit() }; MAX_COLUMN_SECTIONS];
    for (slot, section) in section_slots.iter_mut().zip(inputs) {
        let (blocks, uniform_air, sealed) = match section.flags {
            // SAFETY: a non-null dense block array has the fixed length and alignment.
            0 | ABI_COLUMN_SEALED_SOLID => {
                match unsafe { section.blocks.cast::<[Block; SECTION_CELLS]>().as_ref() } {
                    Some(blocks) => (blocks, false, section.flags == ABI_COLUMN_SEALED_SOLID),
                    None => return ABI_INVALID_ARGUMENT,
                }
            }
            ABI_COLUMN_UNIFORM_AIR if section.blocks.is_null() => (&AIR_BLOCKS, true, false),
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
        slot.write(ColumnSection {
            uniform_air,
            sealed,
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
    // SAFETY: the first inputs.len() slots were initialized in the loop above.
    let sections = unsafe {
        std::slice::from_raw_parts(
            section_slots.as_ptr().cast::<ColumnSection<'_>>(),
            inputs.len(),
        )
    };
    let borders = input.borders.map(|border| {
        if border.is_null() {
            None
        } else {
            // SAFETY: each supplied cardinal buffer contains one plane per section.
            Some(unsafe { std::slice::from_raw_parts(border.cast::<FacePlane>(), sections.len()) })
        }
    });
    with_scratch(|scratch| {
        if output.is_null() {
            let mut sink = CountSink(0);
            mesh_column_into(sections, borders, scratch, &mut sink);
            // SAFETY: count is writable and separate from the other buffers.
            unsafe { count.write(sink.0) };
            return ABI_OK;
        }
        let mut quads = std::mem::take(&mut scratch.quads);
        quads.clear();
        mesh_column_into(sections, borders, scratch, &mut quads);
        let len = quads.len();
        // SAFETY: count is writable and separate from the other buffers.
        unsafe { count.write(len) };
        let status = if capacity < len {
            ABI_BUFFER_TOO_SMALL
        } else {
            // SAFETY: the caller supplies a disjoint, writable output with enough capacity.
            unsafe { ptr::copy_nonoverlapping(quads.as_ptr(), output, len) };
            ABI_OK
        };
        scratch.quads = quads;
        status
    })
}

// Validate the byte-sized C discriminants before creating a Rust reference to
// any Quad. A C caller can initialize these bytes to values outside the enums.
#[inline(always)]
unsafe fn validate_quad_enums(quad: *const Quad) -> bool {
    // SAFETY: only read the initialized bytes at the two enum field offsets.
    let (direction, layer) = unsafe {
        (
            ptr::addr_of!((*quad).direction).cast::<u8>().read(),
            ptr::addr_of!((*quad).layer).cast::<u8>().read(),
        )
    };
    direction <= Direction::East as u8 && layer <= Layer::Fluid as u8
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
    if (output.is_null() && capacity != 0)
        || quads.is_null()
        || quad_count > isize::MAX as usize / std::mem::size_of::<Quad>()
    {
        return ABI_INVALID_ARGUMENT;
    }
    // Validate enum discriminants and packability in a single pass before touching output.
    for i in 0..quad_count {
        // SAFETY: quads is valid for quad_count initialized elements.
        let qptr = unsafe { quads.add(i) };
        if !unsafe { validate_quad_enums(qptr) } {
            return ABI_INVALID_ARGUMENT;
        }
        // SAFETY: enum discriminants for qptr were validated above.
        if !is_packable(unsafe { &*qptr }) {
            return ABI_INVALID_ARGUMENT;
        }
    }
    let required = quad_count * 4;
    // SAFETY: count is writable and separate from input and output.
    unsafe { count.write(required) };
    if output.is_null() {
        return ABI_OK;
    }
    if capacity < required {
        return ABI_BUFFER_TOO_SMALL;
    }
    // SAFETY: all quad_count elements were validated above.
    let quads = unsafe { std::slice::from_raw_parts(quads, quad_count) };
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
        || quads.is_null()
        || quad_count > isize::MAX as usize / std::mem::size_of::<Quad>()
        || u32::try_from(quad_count).is_err()
        || !camera.into_iter().all(f32::is_finite)
    {
        return ABI_INVALID_ARGUMENT;
    }
    with_scratch(|scratch| {
        let mut order = std::mem::take(&mut scratch.sort_buf);
        order.clear();
        let collect = !output.is_null();
        let mut required = 0usize;
        for i in 0..quad_count {
            // SAFETY: quads is valid for quad_count initialized elements.
            let qptr = unsafe { quads.add(i) };
            if !unsafe { validate_quad_enums(qptr) } {
                scratch.sort_buf = order;
                return ABI_INVALID_ARGUMENT;
            }
            // SAFETY: enum discriminants for qptr were validated above.
            let quad = unsafe { &*qptr };
            if is_translucent(quad) {
                for pos in &quad.positions {
                    if !pos[0].is_finite() || !pos[1].is_finite() || !pos[2].is_finite() {
                        scratch.sort_buf = order;
                        return ABI_INVALID_ARGUMENT;
                    }
                }
                if collect && required < capacity {
                    order.push((depth_key(quad, camera), i as u32));
                }
                required += 1;
            }
        }
        // SAFETY: count is writable and separate from input and output.
        unsafe { count.write(required) };
        if output.is_null() {
            scratch.sort_buf = order;
            return ABI_OK;
        }
        if capacity < required {
            scratch.sort_buf = order;
            return ABI_BUFFER_TOO_SMALL;
        }
        order.sort_unstable_by(|(left, i), (right, j)| {
            right
                .partial_cmp(left)
                .expect("finite depths")
                .then(i.cmp(j))
        });
        for (slot, (_, i)) in order.iter().enumerate() {
            // SAFETY: output has enough room for every translucent index.
            unsafe { output.add(slot).write(*i) };
        }
        scratch.sort_buf = order;
        ABI_OK
    })
}

/// Regroups quads into the seven direction buckets, stable within each bucket,
/// and reports the contiguous ranges. A null output with zero capacity queries
/// `count` and still fills `buckets` when non-null.
///
/// # Safety
///
/// Non-null buffers must be aligned and valid for their stated lengths. Input
/// fields must be initialized and readable. Output, buckets and count must be
/// writable and disjoint from the input and each other; buckets may be null.
#[no_mangle]
pub unsafe extern "C" fn mc_spine_client_mesh_bucket_directions(
    quads: *const Quad,
    quad_count: usize,
    output: *mut Quad,
    capacity: usize,
    buckets: *mut DirectionBuckets,
    count: *mut usize,
) -> i32 {
    if count.is_null() {
        return ABI_INVALID_ARGUMENT;
    }
    // SAFETY: non-null count is writable.
    unsafe { count.write(0) };
    if (output.is_null() && capacity != 0)
        || quads.is_null()
        || quad_count > isize::MAX as usize / std::mem::size_of::<Quad>()
        || u32::try_from(quad_count).is_err()
    {
        return ABI_INVALID_ARGUMENT;
    }
    for i in 0..quad_count {
        // SAFETY: quads is valid for quad_count initialized elements.
        let qptr = unsafe { quads.add(i) };
        if !unsafe { validate_quad_enums(qptr) } {
            return ABI_INVALID_ARGUMENT;
        }
    }
    // SAFETY: enum discriminants were validated above.
    let quads = unsafe { std::slice::from_raw_parts(quads, quad_count) };
    let mut counts = [0u32; crate::DIRECTION_BUCKETS];
    for quad in quads {
        counts[DirectionBuckets::index_of(quad.direction)] += 1;
    }
    let buckets_layout = DirectionBuckets::laid_out(counts, 0);
    // SAFETY: count is writable and separate from input, output and buckets.
    unsafe { count.write(quad_count) };
    if !buckets.is_null() {
        // SAFETY: non-null buckets is writable and disjoint from other buffers.
        unsafe { buckets.write(buckets_layout) };
    }
    if output.is_null() {
        return ABI_OK;
    }
    if capacity < quad_count {
        return ABI_BUFFER_TOO_SMALL;
    }
    let mut cursor = buckets_layout.offset;
    for &quad in quads {
        let bucket = DirectionBuckets::index_of(quad.direction);
        // SAFETY: bucket ranges partition 0..quad_count and output is disjoint
        // from input, so each write lands exactly once inside capacity.
        unsafe { output.add(cursor[bucket] as usize).write(quad) };
        cursor[bucket] += 1;
    }
    ABI_OK
}
