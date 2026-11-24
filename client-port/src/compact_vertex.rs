// SPDX-License-Identifier: LGPL-3.0-or-later

use std::mem::size_of;

use crate::PackedVertex;

/// Largest section-local edge coordinate representable by the compact format.
pub const COMPACT_VERTEX_MAX_EDGE: u32 = 16;
/// Largest packed Y fixed-point value representable for a section-local vertex.
pub const COMPACT_VERTEX_MAX_Y_FIXED: u32 = COMPACT_VERTEX_MAX_EDGE * 1024;
/// Largest block-state id representable by the compact format.
pub const COMPACT_VERTEX_MAX_STATE_ID: u32 = (1 << 19) - 1;
/// Largest tint value representable by the compact format (`0x00RRGGBB`).
pub const COMPACT_VERTEX_MAX_TINT: u32 = 0x00ff_ffff;

const POSITION_INDEX_MASK: u32 = 0x007f_ffff;
const POSITION_BASE: u32 = COMPACT_VERTEX_MAX_EDGE + 1;
const POSITION_Y_STRIDE: u32 = POSITION_BASE * POSITION_BASE;
const PACKED_ATTRIBUTE_LOW_MASK: u32 = 0x000f_ffff;
const PACKED_WIDTH_SHIFT: u32 = 20;
const PACKED_HEIGHT_SHIFT: u32 = 25;
const PACKED_CORNER_SHIFT: u32 = 30;
const POSITION_Y_SCALE: f32 = 1024.0;

/// WGSL unpacking for the three-`u32` compact vertex stream.
///
/// The returned words are the exact `PackedVertex` layout consumed by the
/// existing terrain shaders, so a later `client-gpu` wave can vertex-pull the
/// compact buffer and reuse the current decode path unchanged.
pub const COMPACT_VERTEX_WGSL_UNPACK: &str = r#"
struct CompactVoxelVertexInput {
    @location(0) word0: u32,
    @location(1) word1: u32,
    @location(2) word2: u32,
}

struct PackedVoxelVertexWords {
    position: u32,
    state_id: u32,
    attributes: u32,
    tint: u32,
}

fn unpack_compact_voxel_vertex(input: CompactVoxelVertexInput) -> PackedVoxelVertexWords {
    let pos_index = input.word0 & 0x7fffffu;
    let attr_lo = input.word0 >> 23u;
    let attr_hi = input.word1 & 0x1fffffu;
    let attr = attr_lo | (attr_hi << 9u);

    let y_fixed = pos_index / 289u;
    let rem = pos_index - y_fixed * 289u;
    let z = rem / 17u;
    let x = rem - z * 17u;

    var out: PackedVoxelVertexWords;
    out.position = x | (z << 5u) | (y_fixed << 10u);
    out.state_id = ((input.word1 >> 21u) & 0x7ffu) | ((input.word2 & 0xffu) << 11u);
    out.attributes = (attr & 0xfffffu)
        | ((((attr >> 20u) & 15u) + 1u) << 20u)
        | ((((attr >> 24u) & 15u) + 1u) << 25u)
        | ((attr & 0x30000000u) << 2u);
    out.tint = input.word2 >> 8u;
    return out;
}
"#;

/// 12-byte section-local voxel vertex: three packed `u32` words carrying the
/// same visual information as [`PackedVertex`] for the representable domain.
///
/// Bit layout, LSB-first across `word0`, `word1`, `word2`:
///
/// - bits 0..22: mixed-radix position index (`x + 17 * (z + 17 * y_fixed)`),
///   where `x`/`z` are integral section coordinates `0..=16` and `y_fixed` is
///   `PackedVertex` Y precision (`y * 1024`, `0..=16384` for one section);
/// - bits 23..52: 30-bit attributes (`direction`, AO, block/sky light, layer,
///   fluid kind, diagonal, flags, `width - 1`, `height - 1`, corner index);
/// - bits 53..71: 19-bit block-state id;
/// - bits 72..95: 24-bit `0x00RRGGBB` tint.
///
/// This preserves every field emitted by section meshing for section-local
/// vertices while reducing the upload from 16 to 12 bytes per vertex. Inputs
/// outside the documented range are rejected by [`Self::try_from_packed`];
/// [`Self::from_packed`] panics rather than silently truncating state ids,
/// tints, positions, or quad extents.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct CompactVoxelVertex {
    pub word0: u32,
    pub word1: u32,
    pub word2: u32,
}

impl CompactVoxelVertex {
    /// `PackedVertex` bytes divided by compact bytes (`16 / 12`).
    pub fn compression_ratio() -> f32 {
        size_of::<PackedVertex>() as f32 / size_of::<Self>() as f32
    }

    /// Returns whether `vertex` fits the compact section-local domain.
    pub fn is_representable(vertex: &PackedVertex) -> bool {
        let position = vertex.position;
        let x = position & 0x1f;
        let z = (position >> 5) & 0x1f;
        let y_fixed = (position >> 10) & 0x000f_ffff;
        let width = (vertex.attributes >> PACKED_WIDTH_SHIFT) & 0x1f;
        let height = (vertex.attributes >> PACKED_HEIGHT_SHIFT) & 0x1f;

        position & 0xc000_0000 == 0
            && x <= COMPACT_VERTEX_MAX_EDGE
            && z <= COMPACT_VERTEX_MAX_EDGE
            && y_fixed <= COMPACT_VERTEX_MAX_Y_FIXED
            && vertex.state_id <= COMPACT_VERTEX_MAX_STATE_ID
            && vertex.tint <= COMPACT_VERTEX_MAX_TINT
            && (1..=COMPACT_VERTEX_MAX_EDGE).contains(&width)
            && (1..=COMPACT_VERTEX_MAX_EDGE).contains(&height)
    }

    /// Converts a [`PackedVertex`] without loss when it is representable.
    pub fn try_from_packed(vertex: &PackedVertex) -> Option<Self> {
        if !Self::is_representable(vertex) {
            return None;
        }
        let x = vertex.position & 0x1f;
        let z = (vertex.position >> 5) & 0x1f;
        let y_fixed = (vertex.position >> 10) & 0x000f_ffff;
        let position_index = x + POSITION_BASE * (z + POSITION_BASE * y_fixed);
        let attributes = vertex.attributes;
        let compact_attributes = (attributes & PACKED_ATTRIBUTE_LOW_MASK)
            | ((((attributes >> PACKED_WIDTH_SHIFT) & 0x1f) - 1) << 20)
            | ((((attributes >> PACKED_HEIGHT_SHIFT) & 0x1f) - 1) << 24)
            | (((attributes >> PACKED_CORNER_SHIFT) & 0x3) << 28);
        let state_id = vertex.state_id;
        let tint = vertex.tint;

        Some(Self {
            word0: position_index | ((compact_attributes & 0x1ff) << 23),
            word1: ((compact_attributes >> 9) & 0x1f_ffff) | ((state_id & 0x7ff) << 21),
            word2: ((state_id >> 11) & 0xff) | (tint << 8),
        })
    }

    /// Converts a [`PackedVertex`], panicking if it is outside the compact
    /// section-local domain. Use [`Self::try_from_packed`] when the input may
    /// carry column-local Y above one section, oversized state ids, or tints
    /// outside `0x00RRGGBB`.
    #[track_caller]
    pub fn from_packed(vertex: &PackedVertex) -> Self {
        Self::try_from_packed(vertex)
            .expect("PackedVertex is outside CompactVoxelVertex's representable domain")
    }

    /// Rebuilds the equivalent [`PackedVertex`] words.
    pub fn to_packed(self) -> PackedVertex {
        let (position_index, attributes, state_id, tint) = self.unpack_fields();
        let y_fixed = position_index / POSITION_Y_STRIDE;
        let rem = position_index % POSITION_Y_STRIDE;
        let z = rem / POSITION_BASE;
        let x = rem % POSITION_BASE;
        let width = ((attributes >> 20) & 0xf) + 1;
        let height = ((attributes >> 24) & 0xf) + 1;
        let corner = (attributes >> 28) & 0x3;

        PackedVertex {
            position: x | (z << 5) | (y_fixed << 10),
            state_id,
            attributes: (attributes & PACKED_ATTRIBUTE_LOW_MASK)
                | (width << PACKED_WIDTH_SHIFT)
                | (height << PACKED_HEIGHT_SHIFT)
                | (corner << PACKED_CORNER_SHIFT),
            tint,
        }
    }

    /// Packed position index before mixed-radix decoding.
    pub fn position_index(self) -> u32 {
        self.word0 & POSITION_INDEX_MASK
    }

    /// Section-local fixed-point `[x, y_fixed, z]`; Y uses `PackedVertex`'s
    /// 1/1024-block units.
    pub fn position_fixed(self) -> [u32; 3] {
        let position_index = self.position_index();
        let y_fixed = position_index / POSITION_Y_STRIDE;
        let rem = position_index % POSITION_Y_STRIDE;
        [rem % POSITION_BASE, y_fixed, rem / POSITION_BASE]
    }

    /// Section-local position in blocks.
    pub fn local_position(self) -> [f32; 3] {
        let [x, y_fixed, z] = self.position_fixed();
        [x as f32, y_fixed as f32 / POSITION_Y_SCALE, z as f32]
    }

    /// Raw 30-bit compact attributes (width/height are stored minus one).
    pub fn compact_attributes(self) -> u32 {
        self.unpack_fields().1
    }

    /// Attributes re-expanded to the [`PackedVertex`] bit layout.
    pub fn packed_attributes(self) -> u32 {
        self.to_packed().attributes
    }

    /// Compact block-state id.
    pub fn state_id(self) -> u32 {
        self.unpack_fields().2
    }

    /// Compact `0x00RRGGBB` tint.
    pub fn tint(self) -> u32 {
        self.unpack_fields().3
    }

    #[inline(always)]
    fn unpack_fields(self) -> (u32, u32, u32, u32) {
        let position_index = self.word0 & POSITION_INDEX_MASK;
        let attr_lo = self.word0 >> 23;
        let attr_hi = self.word1 & 0x1f_ffff;
        let attributes = attr_lo | (attr_hi << 9);
        let state_id = ((self.word1 >> 21) & 0x7ff) | ((self.word2 & 0xff) << 11);
        let tint = self.word2 >> 8;
        (position_index, attributes, state_id, tint)
    }
}

/// Converts a slice of [`PackedVertex`] values to the compact GPU layout.
///
/// # Panics
/// Panics if any vertex is outside [`CompactVoxelVertex::is_representable`]'s
/// domain; use [`try_pack_vertices_compact`] for fallible conversion.
pub fn pack_vertices_compact(vertices: &[PackedVertex]) -> Vec<CompactVoxelVertex> {
    vertices
        .iter()
        .map(CompactVoxelVertex::from_packed)
        .collect()
}

/// Fallible [`pack_vertices_compact`]; returns `None` if any vertex is outside
/// the compact representable domain.
pub fn try_pack_vertices_compact(vertices: &[PackedVertex]) -> Option<Vec<CompactVoxelVertex>> {
    vertices
        .iter()
        .map(CompactVoxelVertex::try_from_packed)
        .collect()
}
