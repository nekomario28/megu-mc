// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later

use crate::{Layer, Quad, EDGE, LAVA, QUAD_FALLING, QUAD_TINTED, WATER};

pub(crate) const MAX_COLUMN_SECTIONS: usize = 32;
const Y_SCALE: f32 = 1024.0;

/// Four native-endian u32 words, with the GPU bit layout defined in the C header.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PackedVertex {
    pub position: u32,
    pub state_id: u32,
    pub attributes: u32,
    pub tint: u32,
}

pub(crate) fn is_translucent(quad: &Quad) -> bool {
    quad.layer == Layer::Translucent || (quad.layer == Layer::Fluid && quad.fluid_kind == WATER)
}

pub(crate) fn depth_key(quad: &Quad, camera: [f32; 3]) -> f64 {
    let mut key = 0.0;
    for (axis, camera) in camera.into_iter().enumerate() {
        let center = quad
            .positions
            .iter()
            .map(|position| f64::from(position[axis]))
            .sum::<f64>()
            * 0.25;
        // Squared distance with the constant camera² removed to retain precision
        // when finite camera coordinates are much larger than the column.
        key += center * (center - 2.0 * f64::from(camera));
    }
    key
}

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

pub(crate) fn is_packable(quad: &Quad) -> bool {
    let whole = |value: f32, min, max| (min..=max).contains(&value) && value.fract() == 0.0;
    quad.positions.iter().all(|&[x, y, z]| {
        whole(x, 0.0, EDGE as f32)
            && (0.0..=(MAX_COLUMN_SECTIONS * EDGE) as f32).contains(&y)
            && whole(z, 0.0, EDGE as f32)
    }) && extents(quad)
        .into_iter()
        .all(|n| whole(n, 1.0, EDGE as f32))
        && quad.ao.iter().all(|&n| n <= 3)
        && quad.block_light.iter().all(|&n| n <= 15)
        && quad.sky_light.iter().all(|&n| n <= 15)
        && quad.diagonal <= 1
        && quad.fluid_kind <= LAVA
        && quad.flags & !(QUAD_TINTED | QUAD_FALLING) == 0
}

pub(crate) fn pack_quad(quad: &Quad) -> [PackedVertex; 4] {
    let [width, height] = extents(quad).map(|n| n as u32);
    let attributes = quad.direction as u32
        | ((quad.layer as u32) << 13)
        | (u32::from(quad.fluid_kind) << 15)
        | (u32::from(quad.diagonal) << 17)
        | (u32::from(quad.flags) << 18)
        | (width << 20)
        | (height << 25);
    std::array::from_fn(|corner| {
        let [x, y, z] = quad.positions[corner];
        PackedVertex {
            position: x as u32 | ((z as u32) << 5) | (((y * Y_SCALE).round() as u32) << 10),
            state_id: quad.state_id,
            attributes: attributes
                | (u32::from(quad.ao[corner]) << 3)
                | (u32::from(quad.block_light[corner]) << 5)
                | (u32::from(quad.sky_light[corner]) << 9)
                | ((corner as u32) << 30),
            tint: quad.tint,
        }
    })
}
