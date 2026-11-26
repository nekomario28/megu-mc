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

const POSITION_INDEX_MASK: u32 = COMPACT_VERTEX_SCHEMA[0].mask();
const POSITION_BASE: u32 = COMPACT_VERTEX_MAX_EDGE + 1;
const POSITION_Y_STRIDE: u32 = POSITION_BASE * POSITION_BASE;
const PACKED_ATTRIBUTE_LOW_MASK: u32 = 0x000f_ffff;
const PACKED_WIDTH_SHIFT: u32 = 20;
const PACKED_HEIGHT_SHIFT: u32 = 25;
const PACKED_CORNER_SHIFT: u32 = 30;
const POSITION_Y_SCALE: f32 = 1024.0;

/// One occupied bit range in the three-word compact vertex stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BitFieldSpec {
    pub name: &'static str,
    pub word: u8,
    pub shift: u8,
    pub bits: u8,
}

impl BitFieldSpec {
    pub const fn mask(self) -> u32 {
        if self.bits == 32 {
            u32::MAX
        } else {
            ((1u32 << self.bits) - 1) << self.shift
        }
    }
}

/// Single source of truth for all occupied compact vertex word ranges.
pub const COMPACT_VERTEX_SCHEMA: &[BitFieldSpec] = &[
    BitFieldSpec {
        name: "position_index",
        word: 0,
        shift: 0,
        bits: 23,
    },
    BitFieldSpec {
        name: "attributes_low",
        word: 0,
        shift: 23,
        bits: 9,
    },
    BitFieldSpec {
        name: "attributes_high",
        word: 1,
        shift: 0,
        bits: 21,
    },
    BitFieldSpec {
        name: "state_id_low",
        word: 1,
        shift: 21,
        bits: 11,
    },
    BitFieldSpec {
        name: "state_id_high",
        word: 2,
        shift: 0,
        bits: 8,
    },
    BitFieldSpec {
        name: "tint",
        word: 2,
        shift: 8,
        bits: 24,
    },
];

const fn compact_schema_is_valid() -> bool {
    let mut i = 0;
    while i < COMPACT_VERTEX_SCHEMA.len() {
        let a = COMPACT_VERTEX_SCHEMA[i];
        if a.bits == 0 || a.bits > 32 || a.shift as u16 + a.bits as u16 > 32 || a.word > 2 {
            return false;
        }
        let mut j = i + 1;
        while j < COMPACT_VERTEX_SCHEMA.len() {
            let b = COMPACT_VERTEX_SCHEMA[j];
            if a.word == b.word && (a.mask() & b.mask()) != 0 {
                return false;
            }
            j += 1;
        }
        i += 1;
    }
    true
}

const _: () = assert!(compact_schema_is_valid());

/// Emits WGSL masks and offsets from [`COMPACT_VERTEX_SCHEMA`], followed by
/// the compact unpack helper used as the format's reference implementation.
pub fn emit_wgsl_unpack_helpers() -> String {
    let mut out = String::new();
    for field in COMPACT_VERTEX_SCHEMA {
        out.push_str(&format!(
            "const CV_{}_SHIFT: u32 = {}u;\nconst CV_{}_MASK: u32 = 0x{:08x}u;\n",
            field.name.to_ascii_uppercase(),
            field.shift,
            field.name.to_ascii_uppercase(),
            field.mask(),
        ));
    }
    out.push_str(
        r#"
struct CompactVoxelVertexInput { @location(0) word0: u32, @location(1) word1: u32, @location(2) word2: u32, }
struct PackedVoxelVertexWords { position: u32, state_id: u32, attributes: u32, tint: u32, }
fn unpack_compact_voxel_vertex(input: CompactVoxelVertexInput) -> PackedVoxelVertexWords {
    let pos_index = (input.word0 & CV_POSITION_INDEX_MASK) >> CV_POSITION_INDEX_SHIFT;
    let attr_lo = (input.word0 & CV_ATTRIBUTES_LOW_MASK) >> CV_ATTRIBUTES_LOW_SHIFT;
    let attr_hi = (input.word1 & CV_ATTRIBUTES_HIGH_MASK) >> CV_ATTRIBUTES_HIGH_SHIFT;
    let attr = attr_lo | (attr_hi << 9u);
    let y_fixed = pos_index / 289u;
    let rem = pos_index - y_fixed * 289u;
    let z = rem / 17u;
    let x = rem - z * 17u;
    var out: PackedVoxelVertexWords;
    out.position = x | (z << 5u) | (y_fixed << 10u);
    out.state_id = ((input.word1 & CV_STATE_ID_LOW_MASK) >> CV_STATE_ID_LOW_SHIFT)
        | (((input.word2 & CV_STATE_ID_HIGH_MASK) >> CV_STATE_ID_HIGH_SHIFT) << 11u);
    out.attributes = (attr & 0xfffffu)
        | ((((attr >> 20u) & 15u) + 1u) << 20u)
        | ((((attr >> 24u) & 15u) + 1u) << 25u)
        | ((attr & 0x30000000u) << 2u);
    out.tint = (input.word2 & CV_TINT_MASK) >> CV_TINT_SHIFT;
    return out;
}
"#,
    );
    out
}

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
            word0: put_compact_field(position_index, 0) | put_compact_field(compact_attributes, 1),
            word1: put_compact_field(compact_attributes >> 9, 2) | put_compact_field(state_id, 3),
            word2: put_compact_field(state_id >> 11, 4) | put_compact_field(tint, 5),
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
        let position_index = get_compact_field(self.word0, 0);
        let attr_lo = get_compact_field(self.word0, 1);
        let attr_hi = get_compact_field(self.word1, 2);
        let attributes = attr_lo | (attr_hi << 9);
        let state_id = get_compact_field(self.word1, 3) | (get_compact_field(self.word2, 4) << 11);
        let tint = get_compact_field(self.word2, 5);
        (position_index, attributes, state_id, tint)
    }
}

#[inline(always)]
fn put_compact_field(value: u32, spec_index: usize) -> u32 {
    let field = COMPACT_VERTEX_SCHEMA[spec_index];
    (value << field.shift) & field.mask()
}

#[inline(always)]
fn get_compact_field(word: u32, spec_index: usize) -> u32 {
    let field = COMPACT_VERTEX_SCHEMA[spec_index];
    (word & field.mask()) >> field.shift
}

/// Logical fields packed after the full-width state id in a planar record.
/// Offsets count from the least-significant bit of `payload[0]`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct QuadFieldSpec {
    name: &'static str,
    offset: u8,
    bits: u8,
}

const PLANAR_QUAD_SCHEMA: &[QuadFieldSpec] = &[
    QuadFieldSpec {
        name: "origin_x_fixed",
        offset: 0,
        bits: 15,
    },
    QuadFieldSpec {
        name: "origin_y_fixed",
        offset: 15,
        bits: 15,
    },
    QuadFieldSpec {
        name: "origin_z_fixed",
        offset: 30,
        bits: 15,
    },
    QuadFieldSpec {
        name: "span_u_fixed",
        offset: 45,
        bits: 15,
    },
    QuadFieldSpec {
        name: "span_v_fixed",
        offset: 60,
        bits: 15,
    },
    QuadFieldSpec {
        name: "face",
        offset: 75,
        bits: 3,
    },
    QuadFieldSpec {
        name: "diagonal",
        offset: 78,
        bits: 1,
    },
    QuadFieldSpec {
        name: "ao",
        offset: 79,
        bits: 8,
    },
    QuadFieldSpec {
        name: "corner_light",
        offset: 87,
        bits: 32,
    },
    QuadFieldSpec {
        name: "tint",
        offset: 119,
        bits: 24,
    },
    QuadFieldSpec {
        name: "material_flags",
        offset: 143,
        bits: 8,
    },
];

/// Opaque planar quad record: full-width state id plus a 151-bit payload.
/// Coordinates and spans use 1/1024 block units, matching packed vertex Y.
#[repr(C)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct PlanarQuadRecord {
    pub state_id: u32,
    payload: [u32; 5],
}

const _: () = assert!(std::mem::size_of::<PlanarQuadRecord>() == 24);

/// One expanded vertex pulled from a [`PlanarQuadRecord`].
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct ExpandedQuadCorner {
    pub position: [f32; 3],
    pub normal: [f32; 3],
    pub uv: [f32; 2],
    pub ao: u8,
    pub block_light: u8,
    pub sky_light: u8,
    pub state_id: u32,
    pub tint: u32,
    pub material_flags: u8,
    pub corner: u8,
}

impl PlanarQuadRecord {
    /// Packs a section-local planar quad. `origin_fixed` is corner 0 and all
    /// coordinates/spans are in 1/1024-block units. Returns `None` for values
    /// the 24-byte schema cannot represent.
    #[allow(clippy::too_many_arguments)]
    pub fn pack(
        state_id: u32,
        origin_fixed: [u32; 3],
        span_fixed: [u32; 2],
        face: u8,
        diagonal: bool,
        ao: [u8; 4],
        block_light: [u8; 4],
        sky_light: [u8; 4],
        tint: u32,
        material_flags: u8,
    ) -> Option<Self> {
        if origin_fixed.iter().any(|&v| v > 16_384)
            || span_fixed.iter().any(|&v| v == 0 || v > 16_384)
            || face >= 6
            || ao.iter().any(|&v| v > 3)
            || block_light.iter().chain(&sky_light).any(|&v| v > 15)
            || tint > COMPACT_VERTEX_MAX_TINT
        {
            return None;
        }
        if !origin_fixed[0].is_multiple_of(1024)
            || !origin_fixed[2].is_multiple_of(1024)
            || span_fixed.iter().any(|&value| !value.is_multiple_of(1024))
        {
            return None;
        }
        let tangents = QUAD_TANGENTS[usize::from(face)];
        for (span, tangent) in span_fixed.iter().zip(tangents) {
            let axis = usize::from(tangent.unsigned_abs() - 1);
            if origin_fixed[axis] + span > 16_384 {
                return None;
            }
        }

        let mut payload = [0; 5];
        let values = [
            origin_fixed[0],
            origin_fixed[1],
            origin_fixed[2],
            span_fixed[0],
            span_fixed[1],
            u32::from(face),
            u32::from(diagonal),
            u32::from(ao[0])
                | (u32::from(ao[1]) << 2)
                | (u32::from(ao[2]) << 4)
                | (u32::from(ao[3]) << 6),
            u32::from(block_light[0] | (sky_light[0] << 4))
                | (u32::from(block_light[1] | (sky_light[1] << 4)) << 8)
                | (u32::from(block_light[2] | (sky_light[2] << 4)) << 16)
                | (u32::from(block_light[3] | (sky_light[3] << 4)) << 24),
            tint,
            u32::from(material_flags),
        ];
        for (field, value) in PLANAR_QUAD_SCHEMA.iter().zip(values) {
            write_bits(
                &mut payload,
                usize::from(field.offset),
                usize::from(field.bits),
                value,
            );
        }
        Some(Self { state_id, payload })
    }

    /// Triangle indices matching the greedy mesher's two diagonal choices.
    pub const fn indices(&self) -> [u8; 6] {
        if read_bits_const(&self.payload, 78, 1) == 0 {
            [0, 1, 2, 0, 2, 3]
        } else {
            [0, 1, 3, 1, 2, 3]
        }
    }

    /// Expands corners in their original order with cardinal face normal,
    /// planar UV projection, AO, and packed block/sky light.
    pub fn expand_corners(&self) -> [ExpandedQuadCorner; 4] {
        let origin = [
            read_bits(&self.payload, 0, 15) as f32 / 1024.0,
            read_bits(&self.payload, 15, 15) as f32 / 1024.0,
            read_bits(&self.payload, 30, 15) as f32 / 1024.0,
        ];
        let spans = [
            read_bits(&self.payload, 45, 15) as f32 / 1024.0,
            read_bits(&self.payload, 60, 15) as f32 / 1024.0,
        ];
        let face = read_bits(&self.payload, 75, 3) as usize;
        let ao_word = read_bits(&self.payload, 79, 8);
        let light_word = read_bits(&self.payload, 87, 32);
        let tangent = QUAD_TANGENTS[face];
        let normal = QUAD_NORMALS[face];
        let uv_axes = QUAD_UV_AXES[face];
        let tint = read_bits(&self.payload, 119, 24);
        let material_flags = read_bits(&self.payload, 143, 8) as u8;
        std::array::from_fn(|corner| {
            let mut position = origin;
            if corner == 1 || corner == 2 {
                position[tangent[0].unsigned_abs() as usize - 1] +=
                    spans[0] * f32::from(tangent[0].signum());
            }
            if corner >= 2 {
                position[tangent[1].unsigned_abs() as usize - 1] +=
                    spans[1] * f32::from(tangent[1].signum());
            }
            let packed_light = (light_word >> (corner * 8)) as u8;
            ExpandedQuadCorner {
                position,
                normal,
                uv: [position[uv_axes[0]], position[uv_axes[1]]],
                ao: ((ao_word >> (corner * 2)) & 3) as u8,
                block_light: packed_light & 15,
                sky_light: packed_light >> 4,
                state_id: self.state_id,
                tint,
                material_flags,
                corner: corner as u8,
            }
        })
    }
}

const QUAD_TANGENTS: [[i8; 2]; 6] = [[1, 3], [3, 1], [2, 1], [1, 2], [3, 2], [2, 3]];
const QUAD_NORMALS: [[f32; 3]; 6] = [
    [0.0, -1.0, 0.0],
    [0.0, 1.0, 0.0],
    [0.0, 0.0, -1.0],
    [0.0, 0.0, 1.0],
    [-1.0, 0.0, 0.0],
    [1.0, 0.0, 0.0],
];
const QUAD_UV_AXES: [[usize; 2]; 6] = [[0, 2], [0, 2], [0, 1], [0, 1], [2, 1], [2, 1]];

fn write_bits(words: &mut [u32; 5], offset: usize, bits: usize, mut value: u32) {
    let mut remaining = bits;
    let mut cursor = offset;
    while remaining != 0 {
        let word = cursor / 32;
        let shift = cursor % 32;
        let count = remaining.min(32 - shift);
        let mask = if count == 32 {
            u32::MAX
        } else {
            (1u32 << count) - 1
        };
        words[word] |= (value & mask) << shift;
        value >>= count;
        cursor += count;
        remaining -= count;
    }
}

fn read_bits(words: &[u32; 5], offset: usize, bits: usize) -> u32 {
    let mut value = 0;
    let mut written = 0;
    let mut remaining = bits;
    let mut cursor = offset;
    while remaining != 0 {
        let word = cursor / 32;
        let shift = cursor % 32;
        let count = remaining.min(32 - shift);
        let mask = if count == 32 {
            u32::MAX
        } else {
            (1u32 << count) - 1
        };
        value |= ((words[word] >> shift) & mask) << written;
        cursor += count;
        written += count;
        remaining -= count;
    }
    value
}

const fn read_bits_const(words: &[u32; 5], offset: usize, bits: usize) -> u32 {
    let word = offset / 32;
    let shift = offset % 32;
    let mask = (1u32 << bits) - 1;
    (words[word] >> shift) & mask
}

/// WGSL decode constants and planar vertex-pulling helper from the same field
/// offsets used by [`PlanarQuadRecord::pack`].
pub fn emit_wgsl_quad_pull_unpack() -> String {
    let mut out =
        String::from("struct PlanarQuadRecord { state_id: u32, payload: array<u32, 5>, }\n");
    for field in PLANAR_QUAD_SCHEMA {
        let mask = (1u64 << field.bits) - 1;
        out.push_str(&format!(
            "const PQ_{}_OFFSET: u32 = {}u;\nconst PQ_{}_MASK: u32 = 0x{:x}u;\n",
            field.name.to_ascii_uppercase(),
            field.offset,
            field.name.to_ascii_uppercase(),
            mask,
        ));
    }
    out.push_str("fn planar_quad_field(payload: array<u32, 5>, bit_offset: u32, width: u32) -> u32 {\n    let word_index = bit_offset / 32u;\n    let shift = bit_offset % 32u;\n    var value = payload[word_index] >> shift;\n    if shift + width > 32u { value = value | (payload[word_index + 1u] << (32u - shift)); }\n    var mask = 0xffffffffu;\n    if width < 32u { mask = (1u << width) - 1u; }\n    return value & mask;\n}\n");
    out
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
