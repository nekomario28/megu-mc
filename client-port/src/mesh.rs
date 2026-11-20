// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later

use crate::section::{
    Block, Direction, Light, SectionView, ALL_FACES, CUTOUT, EDGE, FACE_CELLS, FLUID_TINTED,
    NO_FLUID, OPAQUE, SAME_STATE_CULL, TINTED, TRANSLUCENT, WATERLOGGED,
};

#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Layer {
    Opaque = 0,
    Cutout = 1,
    Translucent = 2,
    Fluid = 3,
}

pub const QUAD_TINTED: u8 = 1 << 0;
pub const QUAD_FALLING: u8 = 1 << 1;

/// Section-local vertices in outward counterclockwise order.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Quad {
    pub positions: [[f32; 3]; 4],
    pub state_id: u32,
    pub tint: u32,
    pub flow: [f32; 2],
    pub ao: [u8; 4],
    pub block_light: [u8; 4],
    pub sky_light: [u8; 4],
    pub face_light: Light,
    pub direction: Direction,
    pub layer: Layer,
    pub diagonal: u8,
    pub fluid_kind: u8,
    pub flags: u8,
    pub(crate) reserved: u8,
}

impl Quad {
    pub fn indices(&self) -> [u8; 6] {
        if self.diagonal == 0 {
            [0, 1, 2, 0, 2, 3]
        } else {
            [0, 1, 3, 1, 2, 3]
        }
    }
}

const CORNERS: [(i32, i32); 4] = [(0, 0), (1, 0), (1, 1), (0, 1)];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Shade {
    face: Light,
    ao: [u8; 4],
    corners: [Light; 4],
}

impl Shade {
    fn equal_corners(self, a: usize, b: usize) -> bool {
        self.ao[a] == self.ao[b] && self.corners[a] == self.corners[b]
    }

    fn can_merge_u(self) -> bool {
        self.equal_corners(0, 1) && self.equal_corners(3, 2)
    }
    fn can_merge_v(self) -> bool {
        self.equal_corners(0, 3) && self.equal_corners(1, 2)
    }

    fn diagonal(self) -> u8 {
        u8::from(self.ao[0] + self.ao[2] > self.ao[1] + self.ao[3])
    }
}

fn shade(view: &SectionView<'_>, p: [i32; 3], direction: Direction, light: Light) -> Shade {
    let (u, v) = direction.tangents();
    let outside = direction.offset(p);
    let center = view.sample(outside).light;
    let face = Light::new(center.block().max(light.block()), center.sky());
    let mut result = Shade {
        face,
        ao: [0; 4],
        corners: [face; 4],
    };
    for (index, (cu, cv)) in CORNERS.into_iter().enumerate() {
        let mut side_u = outside;
        let mut side_v = outside;
        let mut diagonal = outside;
        side_u[u] += cu * 2 - 1;
        side_v[v] += cv * 2 - 1;
        diagonal[u] = side_u[u];
        diagonal[v] = side_v[v];
        let a = view.sample(side_u);
        let b = view.sample(side_v);
        let d = view.sample(diagonal);
        let side = direction.opposite();
        let (ao, diagonal_light) = if a.block.occludes(side) && b.block.occludes(side) {
            (0, face)
        } else {
            let occupied = [a, b, d]
                .into_iter()
                .filter(|s| s.block.occludes(side))
                .count();
            (3 - occupied as u8, d.light)
        };
        let [block_sum, sky_sum] = [face, a.light, b.light, diagonal_light]
            .into_iter()
            .fold([0, 0], |[bl, sk], s| [bl + s.block(), sk + s.sky()]);
        result.ao[index] = ao;
        result.corners[index] =
            Light::new(((block_sum + 2) / 4).max(light.block()), (sky_sum + 2) / 4);
    }
    result
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Face {
    state_id: u32,
    tint: u32,
    layer: Layer,
    flags: u8,
    shade: Shade,
}

impl Face {
    fn cube(view: &SectionView<'_>, p: [i32; 3], direction: Direction) -> Option<Self> {
        let sample = view.sample(p);
        let block = sample.block;
        let layer = match block.flags {
            flags if flags & OPAQUE != 0 => Layer::Opaque,
            flags if flags & CUTOUT != 0 => Layer::Cutout,
            flags if flags & TRANSLUCENT != 0 => Layer::Translucent,
            _ => return None,
        };
        let next = view.sample(direction.offset(p)).block;
        if next.occludes(direction.opposite())
            || (block.flags & next.flags & SAME_STATE_CULL != 0 && block.state_id == next.state_id)
        {
            return None;
        }
        let tinted = block.flags & TINTED != 0;
        Some(Self {
            state_id: block.state_id,
            tint: if tinted { block.tint } else { 0 },
            layer,
            flags: if tinted { QUAD_TINTED } else { 0 },
            shade: shade(view, p, direction, sample.light),
        })
    }

    fn quad(self, direction: Direction, positions: [[f32; 3]; 4]) -> Quad {
        Quad {
            positions,
            direction,
            state_id: self.state_id,
            tint: self.tint,
            flow: [0.0; 2],
            ao: self.shade.ao,
            block_light: self.shade.corners.map(Light::block),
            sky_light: self.shade.corners.map(Light::sky),
            face_light: self.shade.face,
            layer: self.layer,
            diagonal: self.shade.diagonal(),
            fluid_kind: NO_FLUID,
            flags: self.flags,
            reserved: 0,
        }
    }
}

fn position(slice: usize, u: usize, v: usize, direction: Direction) -> [i32; 3] {
    let (u_axis, v_axis) = direction.tangents();
    let mut p = [0; 3];
    p[direction.axis()] = slice as i32;
    p[u_axis] = u as i32;
    p[v_axis] = v as i32;
    p
}

fn rectangle(p: [i32; 3], direction: Direction, width: usize, height: usize) -> [[f32; 3]; 4] {
    let (u, v) = direction.tangents();
    CORNERS.map(|(cu, cv)| {
        let mut p = p;
        p[direction.axis()] += i32::from(direction.step() > 0);
        p[u] += cu * width as i32;
        p[v] += cv * height as i32;
        p.map(|n| n as f32)
    })
}

fn mesh_cubes(view: &SectionView<'_>, quads: &mut Vec<Quad>) {
    let mut mask = [None; FACE_CELLS];
    for direction in Direction::ALL {
        for slice in 0..EDGE {
            for (index, cell) in mask.iter_mut().enumerate() {
                *cell = Face::cube(
                    view,
                    position(slice, index % EDGE, index / EDGE, direction),
                    direction,
                );
            }
            for v in 0..EDGE {
                for u in 0..EDGE {
                    let index = v * EDGE + u;
                    let Some(face) = mask[index] else {
                        continue;
                    };
                    let merge_u = face.shade.can_merge_u();
                    let merge_v = face.shade.can_merge_v();
                    let equal = |cell: &Option<Face>| *cell == Some(face);
                    let width = if merge_u {
                        mask[index..(v + 1) * EDGE]
                            .iter()
                            .take_while(|cell| equal(cell))
                            .count()
                    } else {
                        1
                    };
                    let mut height = 1;
                    while merge_v && v + height < EDGE {
                        let start = index + height * EDGE;
                        if !mask[start..start + width].iter().all(equal) {
                            break;
                        }
                        height += 1;
                    }
                    for row in v..v + height {
                        let start = row * EDGE + u;
                        mask[start..start + width].fill(None);
                    }
                    let origin = position(slice, u, v, direction);
                    quads.push(face.quad(direction, rectangle(origin, direction, width, height)));
                }
            }
        }
    }
}

/// Height shared by the four cells meeting at this horizontal corner. Integer
/// accumulation keeps the interpolation order independent across section seams.
fn fluid_corner(view: &SectionView<'_>, x: i32, y: i32, z: i32, kind: u8) -> f32 {
    let mut amount_sum = 0;
    let mut weight_sum = 0;
    for x in x - 1..=x {
        for z in z - 1..=z {
            let block = view.sample([x, y, z]).block;
            if block.fluid() == kind {
                if view.sample([x, y + 1, z]).block.fluid() == kind {
                    return 1.0;
                }
                let level = fluid_level(block);
                let amount = if level < 8 { 8 - level } else { 8 };
                let weight = if amount == 8 { 10 } else { 1 };
                amount_sum += u32::from(amount) * weight;
                weight_sum += weight;
            } else if block.flags & OPAQUE == 0 || block.occlusion & ALL_FACES != ALL_FACES {
                weight_sum += 1;
            }
        }
    }
    // The caller's fluid cell contributes to every corner.
    amount_sum as f32 / (weight_sum * 9) as f32
}

fn fluid_level(block: Block) -> u8 {
    if block.flags & WATERLOGGED != 0 {
        0
    } else {
        block.fluid_level
    }
}

fn mesh_fluid(view: &SectionView<'_>, p: [i32; 3], quads: &mut Vec<Quad>) {
    let sample = view.sample(p);
    let block = sample.block;
    let kind = block.fluid();
    if kind == NO_FLUID {
        return;
    }

    let mut visible = Direction::ALL.map(|d| {
        let next = view.sample(d.offset(p)).block;
        next.fluid() != kind
            && !block.occludes(d)
            && (d == Direction::Up || !next.occludes(d.opposite()))
    });
    if !visible.contains(&true) {
        return;
    }

    // h[z][x], for vertices at offsets 0 and 1 from the block.
    let [x, y, z] = p;
    let h = [
        [
            fluid_corner(view, x, y, z, kind),
            fluid_corner(view, x + 1, y, z, kind),
        ],
        [
            fluid_corner(view, x, y, z + 1, kind),
            fluid_corner(view, x + 1, y, z + 1, kind),
        ],
    ];
    // A ceiling can hide the top only when it touches the whole fluid surface.
    if h.into_iter().flatten().all(|height| height == 1.0)
        && view
            .sample(Direction::Up.offset(p))
            .block
            .occludes(Direction::Down)
    {
        visible[Direction::Up as usize] = false;
    }
    let flow = [
        ((h[0][0] - h[0][1]) + (h[1][0] - h[1][1])) * 0.5,
        ((h[0][0] - h[1][0]) + (h[0][1] - h[1][1])) * 0.5,
    ];
    for direction in Direction::ALL {
        if !visible[direction as usize] {
            continue;
        }
        let mut positions = rectangle(p, direction, 1, 1);
        for vertex in &mut positions {
            if vertex[1] > y as f32 {
                vertex[1] =
                    y as f32 + h[(vertex[2] as i32 - z) as usize][(vertex[0] as i32 - x) as usize];
            }
        }
        let tinted = block.flags & FLUID_TINTED != 0;
        let face = Face {
            state_id: block.state_id,
            tint: if tinted { block.fluid_tint } else { 0 },
            layer: Layer::Fluid,
            flags: (if tinted { QUAD_TINTED } else { 0 })
                | (if fluid_level(block) >= 8 {
                    QUAD_FALLING
                } else {
                    0
                }),
            shade: shade(view, p, direction, sample.light),
        };
        let mut quad = face.quad(direction, positions);
        quad.fluid_kind = kind;
        quad.flow = flow;
        quads.push(quad);
    }
}

/// Mesh the current section in stable direction/slice/row order, then append fluids
/// in section index order. All opaque, cutout and translucent models are unit cubes.
pub fn mesh_section(view: &SectionView<'_>) -> Vec<Quad> {
    let mut quads = Vec::new();
    append_section(view, &mut quads);
    quads
}

pub(crate) fn append_section(view: &SectionView<'_>, quads: &mut Vec<Quad>) {
    mesh_cubes(view, quads);
    for (index, block) in view.blocks.iter().enumerate() {
        if block.fluid() != NO_FLUID {
            let p = [
                (index % EDGE) as i32,
                (index / FACE_CELLS) as i32,
                (index / EDGE % EDGE) as i32,
            ];
            mesh_fluid(view, p, quads);
        }
    }
}
