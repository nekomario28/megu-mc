// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later

use crate::mesh::{DirectionBuckets, DIRECTION_BUCKETS};
use crate::{Layer, Quad, EDGE, LAVA, QUAD_FALLING, QUAD_TINTED, WATER};

/// Input bound for column meshing and LOD calls, mirrored by
/// `MC_SPINE_MESH_MAX_COLUMN_SECTIONS` in the C header. Sized for tall Mod
/// dimensions well past the vanilla 24-section window (e.g. a 96-section
/// custom dimension at Y -512..1024): 128 sections is 2,048 blocks.
pub(crate) const MAX_COLUMN_SECTIONS: usize = 128;
const Y_SCALE: f32 = 1024.0;

/// Column-local Y the packed vertex format can still encode: bits 10..29 of
/// `PackedVertex::position` hold `Y * Y_SCALE` (20 bits), so `y` must stay
/// below `PACK_Y_LIMIT` — the first 64 sections of a column. Columns up to
/// `MAX_COLUMN_SECTIONS` mesh, cull and summarize fine; quads at or above
/// this height are simply not representable in this vertex format and
/// [`is_packable`] says so.
const PACK_Y_LIMIT: f32 = 1024.0;

/// Four native-endian u32 words, with the GPU bit layout defined in the C header.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PackedVertex {
    pub position: u32,
    pub state_id: u32,
    pub attributes: u32,
    pub tint: u32,
}

#[inline(always)]
pub(crate) fn is_translucent(quad: &Quad) -> bool {
    quad.layer == Layer::Translucent || (quad.layer == Layer::Fluid && quad.fluid_kind == WATER)
}

#[inline(always)]
pub(crate) fn depth_key(quad: &Quad, camera: [f32; 3]) -> f64 {
    let p = &quad.positions;
    let cx = (((f64::from(p[0][0]) + f64::from(p[1][0])) + f64::from(p[2][0]))
        + f64::from(p[3][0]))
        * 0.25;
    let cy = (((f64::from(p[0][1]) + f64::from(p[1][1])) + f64::from(p[2][1]))
        + f64::from(p[3][1]))
        * 0.25;
    let cz = (((f64::from(p[0][2]) + f64::from(p[1][2])) + f64::from(p[2][2]))
        + f64::from(p[3][2]))
        * 0.25;
    ((cx * (cx - 2.0 * f64::from(camera[0]))) + (cy * (cy - 2.0 * f64::from(camera[1]))))
        + (cz * (cz - 2.0 * f64::from(camera[2])))
}

#[inline(always)]
fn whole_range(value: f32, min: f32, max: f32) -> bool {
    value >= min && value <= max && (value as u32) as f32 == value
}

#[inline(always)]
fn extents(quad: &Quad) -> [f32; 2] {
    if quad.layer == Layer::Fluid {
        return [1.0; 2];
    }
    let (u, v) = quad.direction.tangents();
    [
        quad.positions[1][u] - quad.positions[0][u],
        quad.positions[3][v] - quad.positions[0][v],
    ]
}

#[inline(always)]
pub(crate) fn is_packable(quad: &Quad) -> bool {
    let edge_f = EDGE as f32;
    for &[x, y, z] in &quad.positions {
        if !whole_range(x, 0.0, edge_f)
            || !(0.0..PACK_Y_LIMIT).contains(&y)
            || !whole_range(z, 0.0, edge_f)
        {
            return false;
        }
    }
    let [w, h] = extents(quad);
    if !whole_range(w, 1.0, edge_f) || !whole_range(h, 1.0, edge_f) {
        return false;
    }
    let ao_bad = u32::from_ne_bytes(quad.ao) & 0xfcfc_fcfc;
    let light_bad =
        (u32::from_ne_bytes(quad.block_light) | u32::from_ne_bytes(quad.sky_light)) & 0xf0f0_f0f0;
    (ao_bad | light_bad) == 0
        && quad.diagonal <= 1
        && quad.fluid_kind <= LAVA
        && quad.flags & !(QUAD_TINTED | QUAD_FALLING) == 0
}

#[inline(always)]
pub(crate) fn pack_quad(quad: &Quad) -> [PackedVertex; 4] {
    let [w, h] = extents(quad);
    let width = w as u32;
    let height = h as u32;
    let base_attr = quad.direction as u32
        | ((quad.layer as u32) << 13)
        | (u32::from(quad.fluid_kind) << 15)
        | (u32::from(quad.diagonal) << 17)
        | (u32::from(quad.flags) << 18)
        | (width << 20)
        | (height << 25);
    let state_id = quad.state_id;
    let tint = quad.tint;

    let make = |corner: usize| {
        let [x, y, z] = quad.positions[corner];
        let y_fixed = (f64::from(y * Y_SCALE) + 0.5) as u32;
        PackedVertex {
            position: (x as u32) | ((z as u32) << 5) | (y_fixed << 10),
            state_id,
            attributes: base_attr
                | (u32::from(quad.ao[corner]) << 3)
                | (u32::from(quad.block_light[corner]) << 5)
                | (u32::from(quad.sky_light[corner]) << 9)
                | ((corner as u32) << 30),
            tint,
        }
    };
    [make(0), make(1), make(2), make(3)]
}

impl DirectionBuckets {
    /// `(offset, count)` per bucket in packed vertices. `pack_quad` emits four
    /// vertices per quad in order, so both quad values scale by four; the
    /// ranges index the buffer produced by packing the grouped quads
    /// sequentially into the same base.
    pub fn vertex_ranges(&self) -> [(u32, u32); DIRECTION_BUCKETS] {
        let mut ranges = [(0, 0); DIRECTION_BUCKETS];
        for (i, (&offset, &count)) in self.offset.iter().zip(&self.count).enumerate() {
            ranges[i] = (offset * 4, count * 4);
        }
        ranges
    }
}
