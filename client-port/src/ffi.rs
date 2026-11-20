// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later

//! C ABI v1. See `include/mc_spine_client_mesh.h` for the caller contract.

use std::ptr;

use crate::{
    mesh_section, Block, MissingNeighbor, Quad, Sample, SectionView, EDGE_CORNER_CELLS, FACE_CELLS,
    LIGHT_BYTES, SECTION_CELLS,
};

pub const ABI_VERSION: u32 = 1;
pub const ABI_OK: i32 = 0;
pub const ABI_INVALID_ARGUMENT: i32 = 1;
pub const ABI_BUFFER_TOO_SMALL: i32 = 2;
pub const ABI_AIR: u32 = 0;
pub const ABI_OPAQUE: u32 = 1;

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
