// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later

//! CPU section meshing for Minecraft Java 1.21.1 (protocol 767).
#![deny(unsafe_op_in_unsafe_fn)]

mod cache;
mod column;
mod cull;
pub mod ffi;
mod lod;
mod mesh;
mod packed;
mod section;

#[cfg(test)]
mod tests;

pub use cache::{MeshCache, SectionCoord};
pub use cull::{box_facing_mask, box_in_frustum, SectionVis};
pub use lod::{summarize_column, LodCell, LodSummary, LOD_COARSE, LOD_FINE};
pub use mesh::{mesh_section, Layer, Quad, QUAD_FALLING, QUAD_TINTED};
pub use section::*;

pub const PROTOCOL_VERSION: u32 = 767;
