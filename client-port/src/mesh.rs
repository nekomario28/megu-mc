// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later

use std::cell::RefCell;

use crate::section::{
    Block, Direction, Light, MissingNeighbor, Sample, SectionView, ALL_FACES, CUTOUT, EDGE,
    FACE_CELLS, FLUID_TINTED, LIGHT_BYTES, NO_FLUID, OPAQUE, SAME_STATE_CULL, SECTION_CELLS,
    TINTED, TRANSLUCENT, WATERLOGGED,
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

pub(crate) const PAD_EDGE: usize = EDGE + 2;
pub(crate) const PAD_SLICE: usize = PAD_EDGE * PAD_EDGE;
pub(crate) const PAD_CELLS: usize = PAD_EDGE * PAD_SLICE;

const CUBE_BIT: u8 = 1 << 7;
const CUBE_FLAGS: u8 = OPAQUE | CUTOUT | TRANSLUCENT;

const CORNERS: [(i32, i32); 4] = [(0, 0), (1, 0), (1, 1), (0, 1)];

#[derive(Clone, Copy, Default, PartialEq, Eq)]
struct PackedFace {
    material: u64,
    shading: u64,
}

impl PackedFace {
    #[inline(always)]
    fn new(state_id: u32, tint: u32, layer: Layer, flags: u8, shade_bits: u64) -> Self {
        Self {
            material: u64::from(state_id) | (u64::from(tint) << 32),
            shading: shade_bits | (u64::from(layer as u8) << 56) | (u64::from(flags) << 58),
        }
    }

    #[inline(always)]
    fn can_merge_u(self) -> bool {
        ((self.shading ^ (self.shading >> 12)) & 0x0000_000f_ff00_0fff) == 0
    }

    #[inline(always)]
    fn can_merge_v(self) -> bool {
        let s = self.shading;
        (s & 0xfff) == ((s >> 36) & 0xfff) && ((s >> 12) & 0xfff) == ((s >> 24) & 0xfff)
    }

    #[inline(always)]
    fn quad(self, direction: Direction, positions: [[f32; 3]; 4]) -> Quad {
        let s = self.shading;
        let ao0 = ((s >> 8) & 3) as u8;
        let ao1 = ((s >> 20) & 3) as u8;
        let ao2 = ((s >> 32) & 3) as u8;
        let ao3 = ((s >> 44) & 3) as u8;
        let l0 = (s & 0xff) as u8;
        let l1 = ((s >> 12) & 0xff) as u8;
        let l2 = ((s >> 24) & 0xff) as u8;
        let l3 = ((s >> 36) & 0xff) as u8;
        let layer = match (s >> 56) & 3 {
            0 => Layer::Opaque,
            1 => Layer::Cutout,
            _ => Layer::Translucent,
        };
        Quad {
            positions,
            direction,
            state_id: self.material as u32,
            tint: (self.material >> 32) as u32,
            flow: [0.0; 2],
            ao: [ao0, ao1, ao2, ao3],
            block_light: [l0 & 15, l1 & 15, l2 & 15, l3 & 15],
            sky_light: [l0 >> 4, l1 >> 4, l2 >> 4, l3 >> 4],
            face_light: Light(((s >> 48) & 0xff) as u8),
            layer,
            diagonal: u8::from(ao0 + ao2 > ao1 + ao3),
            fluid_kind: NO_FLUID,
            flags: ((s >> 58) & 3) as u8,
            reserved: 0,
        }
    }
}

pub(crate) trait QuadSink {
    fn push(&mut self, quad: Quad);
}

impl QuadSink for Vec<Quad> {
    #[inline(always)]
    fn push(&mut self, quad: Quad) {
        Vec::push(self, quad);
    }
}

pub(crate) struct CountSink(pub usize);

impl QuadSink for CountSink {
    #[inline(always)]
    fn push(&mut self, _quad: Quad) {
        self.0 += 1;
    }
}

pub(crate) trait BorderStates {
    fn border_state_id(&self, direction: Direction, x: usize, y: usize, z: usize) -> u32;
}

struct SectionViewBorders<'a>(&'a SectionView<'a>);

impl BorderStates for SectionViewBorders<'_> {
    #[inline(always)]
    fn border_state_id(&self, direction: Direction, x: usize, y: usize, z: usize) -> u32 {
        match self.0.borders[direction as usize] {
            Some(border) => border[direction.border_index([x, y, z])].block.state_id,
            None => 0,
        }
    }
}

pub(crate) struct MesherScratch {
    pub(crate) occ: Box<[u8; PAD_CELLS]>,
    pub(crate) light: Box<[u8; PAD_CELLS]>,
    pub(crate) fluid: Box<[u8; PAD_CELLS]>,
    cube_rows: [u16; FACE_CELLS],
    fluid_rows: [u16; FACE_CELLS],
    occ_rows: [[u16; FACE_CELLS]; 6],
    face_mask: [PackedFace; FACE_CELLS],
    pub(crate) quads: Vec<Quad>,
    pub(crate) sort_buf: Vec<(f64, u32)>,
}

impl MesherScratch {
    fn new() -> Self {
        Self {
            occ: Box::new([0; PAD_CELLS]),
            light: Box::new([0; PAD_CELLS]),
            fluid: Box::new([0; PAD_CELLS]),
            cube_rows: [0; FACE_CELLS],
            fluid_rows: [0; FACE_CELLS],
            occ_rows: [[0; FACE_CELLS]; 6],
            face_mask: [PackedFace::default(); FACE_CELLS],
            quads: Vec::new(),
            sort_buf: Vec::new(),
        }
    }
}

thread_local! {
    static SCRATCH: RefCell<MesherScratch> = RefCell::new(MesherScratch::new());
}

pub(crate) fn with_scratch<R>(f: impl FnOnce(&mut MesherScratch) -> R) -> R {
    SCRATCH.with(|cell| {
        if let Ok(mut guard) = cell.try_borrow_mut() {
            f(&mut guard)
        } else {
            let mut fallback = MesherScratch::new();
            f(&mut fallback)
        }
    })
}

#[inline(always)]
pub(crate) const fn pad_index(px: usize, py: usize, pz: usize) -> usize {
    py * PAD_SLICE + pz * PAD_EDGE + px
}

#[inline(always)]
pub(crate) fn encode_occ(block: Block) -> u8 {
    let occ = if block.flags & OPAQUE != 0 {
        block.occlusion & ALL_FACES
    } else {
        0
    };
    let cube = if block.flags & CUBE_FLAGS != 0 {
        CUBE_BIT
    } else {
        0
    };
    occ | (block.flags & SAME_STATE_CULL) | cube
}

#[inline(always)]
pub(crate) fn encode_fluid(block: Block) -> u8 {
    let kind = block.fluid();
    if kind == NO_FLUID {
        return 0;
    }
    let level = fluid_level(block);
    let amount = if level < 8 { 8 - level } else { 8 };
    kind | (amount << 2)
}

#[inline(always)]
pub(crate) fn missing_encoded(missing: MissingNeighbor) -> (u8, u8) {
    match missing {
        MissingNeighbor::Air => (0, 0xf0),
        MissingNeighbor::Opaque => (ALL_FACES | CUBE_BIT, 0x00),
    }
}

#[inline(always)]
pub(crate) fn unpack_light_row(
    block_light: &[u8; LIGHT_BYTES],
    sky_light: &[u8; LIGHT_BYTES],
    src_byte_offset: usize,
    dst: &mut [u8],
) {
    let bl = &block_light[src_byte_offset..src_byte_offset + 8];
    let sl = &sky_light[src_byte_offset..src_byte_offset + 8];
    let out = &mut dst[..16];
    for k in 0..8 {
        let b = bl[k];
        let s = sl[k];
        out[k * 2] = (b & 0x0f) | ((s & 0x0f) << 4);
        out[k * 2 + 1] = (b >> 4) | (s & 0xf0);
    }
}

#[inline(always)]
pub(crate) fn transpose_16x16(src: [u16; 16]) -> [u16; 16] {
    #[inline(always)]
    fn pack4(a: u16, b: u16, c: u16, d: u16) -> u64 {
        u64::from(a) | (u64::from(b) << 16) | (u64::from(c) << 32) | (u64::from(d) << 48)
    }

    #[inline(always)]
    fn transpose_4x4_in_u64(mut w: u64) -> u64 {
        let t = ((w >> 15) ^ w) & 0x0000_aaaa_0000_aaaa;
        w = w ^ t ^ (t << 15);
        let t = ((w >> 30) ^ w) & 0x0000_0000_cccc_cccc;
        w ^ t ^ (t << 30)
    }

    let mut w0 = transpose_4x4_in_u64(pack4(src[0], src[1], src[2], src[3]));
    let mut w1 = transpose_4x4_in_u64(pack4(src[4], src[5], src[6], src[7]));
    let mut w2 = transpose_4x4_in_u64(pack4(src[8], src[9], src[10], src[11]));
    let mut w3 = transpose_4x4_in_u64(pack4(src[12], src[13], src[14], src[15]));

    let t = ((w1 << 4) ^ w0) & 0xf0f0_f0f0_f0f0_f0f0;
    w0 ^= t;
    w1 ^= t >> 4;
    let t = ((w3 << 4) ^ w2) & 0xf0f0_f0f0_f0f0_f0f0;
    w2 ^= t;
    w3 ^= t >> 4;

    let t = ((w2 << 8) ^ w0) & 0xff00_ff00_ff00_ff00;
    w0 ^= t;
    w2 ^= t >> 8;
    let t = ((w3 << 8) ^ w1) & 0xff00_ff00_ff00_ff00;
    w1 ^= t;
    w3 ^= t >> 8;

    [
        w0 as u16,
        (w0 >> 16) as u16,
        (w0 >> 32) as u16,
        (w0 >> 48) as u16,
        w1 as u16,
        (w1 >> 16) as u16,
        (w1 >> 32) as u16,
        (w1 >> 48) as u16,
        w2 as u16,
        (w2 >> 16) as u16,
        (w2 >> 32) as u16,
        (w2 >> 48) as u16,
        w3 as u16,
        (w3 >> 16) as u16,
        (w3 >> 32) as u16,
        (w3 >> 48) as u16,
    ]
}

pub(crate) struct InteriorScan {
    pub any_cube: bool,
    pub any_fluid: bool,
    pub any_same_state_cull: bool,
    pub any_partial_occ: bool,
}

pub(crate) fn load_interior(
    blocks: &[Block; SECTION_CELLS],
    block_light: &[u8; LIGHT_BYTES],
    sky_light: &[u8; LIGHT_BYTES],
    scratch: &mut MesherScratch,
) -> InteriorScan {
    let mut any_cube_bits = 0u16;
    let mut any_fluid_bits = 0u16;
    let mut any_same_state_cull = false;
    let mut any_partial_occ = false;

    for y in 0..EDGE {
        let pad_y = (y + 1) * PAD_SLICE;
        let sec_y = y * FACE_CELLS;
        for z in 0..EDGE {
            let row_idx = y * EDGE + z;
            let sec_base = sec_y + z * EDGE;
            let pad_base = pad_y + (z + 1) * PAD_EDGE + 1;
            let row_blocks = &blocks[sec_base..sec_base + EDGE];
            let occ_dst = &mut scratch.occ[pad_base..pad_base + EDGE];

            let mut cube_m = 0u16;
            let mut fluid_m = 0u16;
            let mut occ_all_m = 0u16;
            let mut has_partial = false;

            for x in 0..EDGE {
                let b = row_blocks[x];
                if b.flags == OPAQUE && b.occlusion == ALL_FACES && b.fluid_kind == NO_FLUID {
                    let bit = 1u16 << x;
                    cube_m |= bit;
                    occ_all_m |= bit;
                    occ_dst[x] = ALL_FACES | CUBE_BIT;
                } else if b.flags == 0 && b.fluid_kind == NO_FLUID {
                    occ_dst[x] = 0;
                } else {
                    let bit = 1u16 << x;
                    let occ = if b.flags & OPAQUE != 0 {
                        b.occlusion & ALL_FACES
                    } else {
                        0
                    };
                    let is_cube = (b.flags & CUBE_FLAGS) != 0;
                    if is_cube {
                        cube_m |= bit;
                    }
                    if occ == ALL_FACES {
                        occ_all_m |= bit;
                    } else if occ != 0 {
                        has_partial = true;
                    }
                    if b.flags & SAME_STATE_CULL != 0 {
                        any_same_state_cull = true;
                    }
                    if b.fluid() != NO_FLUID {
                        fluid_m |= bit;
                    }
                    occ_dst[x] =
                        occ | (b.flags & SAME_STATE_CULL) | if is_cube { CUBE_BIT } else { 0 };
                }
            }

            scratch.cube_rows[row_idx] = cube_m;
            scratch.fluid_rows[row_idx] = fluid_m;
            any_cube_bits |= cube_m;
            any_fluid_bits |= fluid_m;

            if !has_partial {
                scratch.occ_rows[0][row_idx] = occ_all_m;
                if any_partial_occ {
                    scratch.occ_rows[1][row_idx] = occ_all_m;
                    scratch.occ_rows[2][row_idx] = occ_all_m;
                    scratch.occ_rows[3][row_idx] = occ_all_m;
                    scratch.occ_rows[4][row_idx] = occ_all_m;
                    scratch.occ_rows[5][row_idx] = occ_all_m;
                }
            } else {
                if !any_partial_occ {
                    any_partial_occ = true;
                    let (first, rest) = scratch.occ_rows.split_at_mut(1);
                    for dst in rest {
                        dst[..row_idx].copy_from_slice(&first[0][..row_idx]);
                    }
                }
                let mut m = [0u16; 6];
                for (x, &o) in occ_dst.iter().enumerate() {
                    let bit = 1u16 << x;
                    if o & (1 << 0) != 0 {
                        m[0] |= bit;
                    }
                    if o & (1 << 1) != 0 {
                        m[1] |= bit;
                    }
                    if o & (1 << 2) != 0 {
                        m[2] |= bit;
                    }
                    if o & (1 << 3) != 0 {
                        m[3] |= bit;
                    }
                    if o & (1 << 4) != 0 {
                        m[4] |= bit;
                    }
                    if o & (1 << 5) != 0 {
                        m[5] |= bit;
                    }
                }
                for (d, &mask) in m.iter().enumerate() {
                    scratch.occ_rows[d][row_idx] = mask;
                }
            }
        }
    }

    let any_cube = any_cube_bits != 0;
    let any_fluid = any_fluid_bits != 0;
    if !any_cube && !any_fluid {
        return InteriorScan {
            any_cube,
            any_fluid,
            any_same_state_cull,
            any_partial_occ,
        };
    }

    for y in 0..EDGE {
        let pad_y = (y + 1) * PAD_SLICE;
        let light_y = y * (FACE_CELLS / 2);
        for z in 0..EDGE {
            let pad_base = pad_y + (z + 1) * PAD_EDGE + 1;
            unpack_light_row(
                block_light,
                sky_light,
                light_y + z * (EDGE / 2),
                &mut scratch.light[pad_base..pad_base + EDGE],
            );
        }
    }

    if any_fluid {
        for y in 0..EDGE {
            let pad_y = (y + 1) * PAD_SLICE;
            let sec_y = y * FACE_CELLS;
            for z in 0..EDGE {
                let row_idx = y * EDGE + z;
                let pad_base = pad_y + (z + 1) * PAD_EDGE + 1;
                let dst = &mut scratch.fluid[pad_base..pad_base + EDGE];
                let mut fm = scratch.fluid_rows[row_idx];
                dst.fill(0);
                if fm != 0 {
                    let row_blocks = &blocks[sec_y + z * EDGE..sec_y + z * EDGE + EDGE];
                    while fm != 0 {
                        let x = fm.trailing_zeros() as usize;
                        fm &= fm - 1;
                        dst[x] = encode_fluid(row_blocks[x]);
                    }
                }
            }
        }
    }

    InteriorScan {
        any_cube,
        any_fluid,
        any_same_state_cull,
        any_partial_occ,
    }
}

fn load_section_shell(view: &SectionView<'_>, load_fluid: bool, scratch: &mut MesherScratch) {
    let (miss_occ, miss_light) = missing_encoded(view.missing);

    for (dir, py) in [(Direction::Down, 0usize), (Direction::Up, PAD_EDGE - 1)] {
        let plane_base = py * PAD_SLICE;
        match view.borders[dir as usize] {
            Some(border) => {
                for z in 0..EDGE {
                    let src = &border[z * EDGE..z * EDGE + EDGE];
                    let dst = plane_base + (z + 1) * PAD_EDGE + 1;
                    for (x, &s) in src.iter().enumerate() {
                        scratch.occ[dst + x] = encode_occ(s.block);
                        scratch.light[dst + x] = s.light.0;
                    }
                    if load_fluid {
                        for (x, &s) in src.iter().enumerate() {
                            scratch.fluid[dst + x] = encode_fluid(s.block);
                        }
                    }
                }
            }
            None => {
                for z in 0..EDGE {
                    let dst = plane_base + (z + 1) * PAD_EDGE + 1;
                    scratch.occ[dst..dst + EDGE].fill(miss_occ);
                    scratch.light[dst..dst + EDGE].fill(miss_light);
                    if load_fluid {
                        scratch.fluid[dst..dst + EDGE].fill(0);
                    }
                }
            }
        }
    }

    for (dir, pz) in [(Direction::North, 0usize), (Direction::South, PAD_EDGE - 1)] {
        match view.borders[dir as usize] {
            Some(border) => {
                for y in 0..EDGE {
                    let src = &border[y * EDGE..y * EDGE + EDGE];
                    let dst = (y + 1) * PAD_SLICE + pz * PAD_EDGE + 1;
                    for (x, &s) in src.iter().enumerate() {
                        scratch.occ[dst + x] = encode_occ(s.block);
                        scratch.light[dst + x] = s.light.0;
                    }
                    if load_fluid {
                        for (x, &s) in src.iter().enumerate() {
                            scratch.fluid[dst + x] = encode_fluid(s.block);
                        }
                    }
                }
            }
            None => {
                for y in 0..EDGE {
                    let dst = (y + 1) * PAD_SLICE + pz * PAD_EDGE + 1;
                    scratch.occ[dst..dst + EDGE].fill(miss_occ);
                    scratch.light[dst..dst + EDGE].fill(miss_light);
                    if load_fluid {
                        scratch.fluid[dst..dst + EDGE].fill(0);
                    }
                }
            }
        }
    }

    for (dir, px) in [(Direction::West, 0usize), (Direction::East, PAD_EDGE - 1)] {
        match view.borders[dir as usize] {
            Some(border) => {
                for z in 0..EDGE {
                    let src = &border[z * EDGE..z * EDGE + EDGE];
                    let col_base = (z + 1) * PAD_EDGE + px;
                    for (y, &s) in src.iter().enumerate() {
                        let idx = (y + 1) * PAD_SLICE + col_base;
                        scratch.occ[idx] = encode_occ(s.block);
                        scratch.light[idx] = s.light.0;
                    }
                    if load_fluid {
                        for (y, &s) in src.iter().enumerate() {
                            let idx = (y + 1) * PAD_SLICE + col_base;
                            scratch.fluid[idx] = encode_fluid(s.block);
                        }
                    }
                }
            }
            None => {
                for y in 0..EDGE {
                    let row_base = (y + 1) * PAD_SLICE + px;
                    for z in 0..EDGE {
                        let idx = row_base + (z + 1) * PAD_EDGE;
                        scratch.occ[idx] = miss_occ;
                        scratch.light[idx] = miss_light;
                    }
                    if load_fluid {
                        for z in 0..EDGE {
                            let idx = row_base + (z + 1) * PAD_EDGE;
                            scratch.fluid[idx] = 0;
                        }
                    }
                }
            }
        }
    }

    match view.edges_and_corners {
        Some(halo) => {
            for sides in 0..4usize {
                let py = if sides & 1 != 0 { PAD_EDGE - 1 } else { 0 };
                let pz = if sides & 2 != 0 { PAD_EDGE - 1 } else { 0 };
                let src = &halo[sides * EDGE..sides * EDGE + EDGE];
                let dst = pad_index(1, py, pz);
                for (x, &s) in src.iter().enumerate() {
                    scratch.occ[dst + x] = encode_occ(s.block);
                    scratch.light[dst + x] = s.light.0;
                }
                if load_fluid {
                    for (x, &s) in src.iter().enumerate() {
                        scratch.fluid[dst + x] = encode_fluid(s.block);
                    }
                }
            }
            for sides in 0..4usize {
                let px = if sides & 1 != 0 { PAD_EDGE - 1 } else { 0 };
                let pz = if sides & 2 != 0 { PAD_EDGE - 1 } else { 0 };
                let src = &halo[(4 + sides) * EDGE..(4 + sides) * EDGE + EDGE];
                for (y, &s) in src.iter().enumerate() {
                    let idx = pad_index(px, y + 1, pz);
                    scratch.occ[idx] = encode_occ(s.block);
                    scratch.light[idx] = s.light.0;
                    if load_fluid {
                        scratch.fluid[idx] = encode_fluid(s.block);
                    }
                }
            }
            for sides in 0..4usize {
                let px = if sides & 1 != 0 { PAD_EDGE - 1 } else { 0 };
                let py = if sides & 2 != 0 { PAD_EDGE - 1 } else { 0 };
                let src = &halo[(8 + sides) * EDGE..(8 + sides) * EDGE + EDGE];
                for (z, &s) in src.iter().enumerate() {
                    let idx = pad_index(px, py, z + 1);
                    scratch.occ[idx] = encode_occ(s.block);
                    scratch.light[idx] = s.light.0;
                    if load_fluid {
                        scratch.fluid[idx] = encode_fluid(s.block);
                    }
                }
            }
            for sides in 0..8usize {
                let px = if sides & 1 != 0 { PAD_EDGE - 1 } else { 0 };
                let py = if sides & 2 != 0 { PAD_EDGE - 1 } else { 0 };
                let pz = if sides & 4 != 0 { PAD_EDGE - 1 } else { 0 };
                let idx = pad_index(px, py, pz);
                let s = halo[12 * EDGE + sides];
                scratch.occ[idx] = encode_occ(s.block);
                scratch.light[idx] = s.light.0;
                if load_fluid {
                    scratch.fluid[idx] = encode_fluid(s.block);
                }
            }
        }
        None => {
            fill_halo_missing(miss_occ, miss_light, load_fluid, scratch);
        }
    }
}

pub(crate) fn fill_halo_missing(
    miss_occ: u8,
    miss_light: u8,
    load_fluid: bool,
    scratch: &mut MesherScratch,
) {
    for py in [0, PAD_EDGE - 1] {
        for pz in [0, PAD_EDGE - 1] {
            let dst = pad_index(0, py, pz);
            scratch.occ[dst..dst + PAD_EDGE].fill(miss_occ);
            scratch.light[dst..dst + PAD_EDGE].fill(miss_light);
            if load_fluid {
                scratch.fluid[dst..dst + PAD_EDGE].fill(0);
            }
        }
        for z in 1..PAD_EDGE - 1 {
            for px in [0, PAD_EDGE - 1] {
                let idx = pad_index(px, py, z);
                scratch.occ[idx] = miss_occ;
                scratch.light[idx] = miss_light;
                if load_fluid {
                    scratch.fluid[idx] = 0;
                }
            }
        }
    }
    for y in 1..PAD_EDGE - 1 {
        let base = y * PAD_SLICE;
        for pz in [0, PAD_EDGE - 1] {
            for px in [0, PAD_EDGE - 1] {
                let idx = base + pz * PAD_EDGE + px;
                scratch.occ[idx] = miss_occ;
                scratch.light[idx] = miss_light;
                if load_fluid {
                    scratch.fluid[idx] = 0;
                }
            }
        }
    }
}

pub(crate) fn fill_vertical_edges_and_corners_missing(
    miss_occ: u8,
    miss_light: u8,
    load_fluid: bool,
    scratch: &mut MesherScratch,
) {
    for y in 0..PAD_EDGE {
        let base = y * PAD_SLICE;
        for pz in [0, PAD_EDGE - 1] {
            for px in [0, PAD_EDGE - 1] {
                let idx = base + pz * PAD_EDGE + px;
                scratch.occ[idx] = miss_occ;
                scratch.light[idx] = miss_light;
                if load_fluid {
                    scratch.fluid[idx] = 0;
                }
            }
        }
    }
}

pub(crate) fn load_sample_border_plane_into(
    samples: &[Sample; FACE_CELLS],
    dir: Direction,
    load_fluid: bool,
    scratch: &mut MesherScratch,
) {
    match dir {
        Direction::North | Direction::South => {
            let pz = if dir == Direction::North {
                0
            } else {
                PAD_EDGE - 1
            };
            for y in 0..EDGE {
                let src = &samples[y * EDGE..y * EDGE + EDGE];
                let dst = (y + 1) * PAD_SLICE + pz * PAD_EDGE + 1;
                for (x, &s) in src.iter().enumerate() {
                    scratch.occ[dst + x] = encode_occ(s.block);
                    scratch.light[dst + x] = s.light.0;
                }
                if load_fluid {
                    for (x, &s) in src.iter().enumerate() {
                        scratch.fluid[dst + x] = encode_fluid(s.block);
                    }
                }
            }
        }
        Direction::West | Direction::East => {
            let px = if dir == Direction::West {
                0
            } else {
                PAD_EDGE - 1
            };
            for z in 0..EDGE {
                let src = &samples[z * EDGE..z * EDGE + EDGE];
                let col_base = (z + 1) * PAD_EDGE + px;
                for (y, &s) in src.iter().enumerate() {
                    let idx = (y + 1) * PAD_SLICE + col_base;
                    scratch.occ[idx] = encode_occ(s.block);
                    scratch.light[idx] = s.light.0;
                }
                if load_fluid {
                    for (y, &s) in src.iter().enumerate() {
                        let idx = (y + 1) * PAD_SLICE + col_base;
                        scratch.fluid[idx] = encode_fluid(s.block);
                    }
                }
            }
        }
        Direction::Down | Direction::Up => unreachable!(),
    }
}

const DIR_PAD_STRIDES: [(isize, isize, isize, isize); 6] = [
    (
        PAD_SLICE as isize,
        1,
        PAD_EDGE as isize,
        -(PAD_SLICE as isize),
    ),
    (PAD_SLICE as isize, PAD_EDGE as isize, 1, PAD_SLICE as isize),
    (
        PAD_EDGE as isize,
        PAD_SLICE as isize,
        1,
        -(PAD_EDGE as isize),
    ),
    (PAD_EDGE as isize, 1, PAD_SLICE as isize, PAD_EDGE as isize),
    (1, PAD_EDGE as isize, PAD_SLICE as isize, -1),
    (1, PAD_SLICE as isize, PAD_EDGE as isize, 1),
];

const DIR_SEC_STRIDES: [(usize, usize, usize, isize); 6] = [
    (FACE_CELLS, 1, EDGE, -(FACE_CELLS as isize)),
    (FACE_CELLS, EDGE, 1, FACE_CELLS as isize),
    (EDGE, FACE_CELLS, 1, -(EDGE as isize)),
    (EDGE, 1, FACE_CELLS, EDGE as isize),
    (1, EDGE, FACE_CELLS, -1),
    (1, FACE_CELLS, EDGE, 1),
];

#[derive(Clone, Copy)]
struct CornerBase {
    face_bl: u8,
    face_sk: u8,
    src_bl: u8,
}

#[derive(Clone, Copy)]
struct SideSample {
    occ: u8,
    bl: u8,
    sk: u8,
}

#[inline(always)]
fn shade_corner_unpacked(base: CornerBase, a: SideSample, b: SideSample, od: u8, ld: u8) -> u64 {
    let both = a.occ & b.occ;
    let ao = if both != 0 { 0 } else { 3 - a.occ - b.occ - od };
    let (d_bl, d_sk) = if both != 0 {
        (base.face_bl, base.face_sk)
    } else {
        (ld & 15, ld >> 4)
    };
    let c_bl = ((base.face_bl + a.bl + b.bl + d_bl + 2) >> 2).max(base.src_bl);
    let c_sk = (base.face_sk + a.sk + b.sk + d_sk + 2) >> 2;
    u64::from(c_bl | (c_sk << 4)) | (u64::from(ao) << 8)
}

/// SAFETY invariant for callers: `pad_idx` must address an interior cell with
/// padded coordinates `(px, py, pz)` each in `1..=16`. Then `out = pad_idx + step_dir`
/// and all 8 tangent neighbors `out ± su ± sv` have coordinates in `0..=17`, so all
/// indices lie strictly within `0..PAD_CELLS`.
#[inline(always)]
unsafe fn shade_fast(
    occ: &[u8; PAD_CELLS],
    light: &[u8; PAD_CELLS],
    pad_idx: usize,
    step_dir: isize,
    su: isize,
    sv: isize,
    opp_bit: u8,
) -> u64 {
    let out = pad_idx.wrapping_add_signed(step_dir);
    let i_mu = out.wrapping_add_signed(-su);
    let i_pu = out.wrapping_add_signed(su);
    let i_mv = out.wrapping_add_signed(-sv);
    let i_pv = out.wrapping_add_signed(sv);
    let i_mm = i_mu.wrapping_add_signed(-sv);
    let i_pm = i_pu.wrapping_add_signed(-sv);
    let i_pp = i_pu.wrapping_add_signed(sv);
    let i_mp = i_mu.wrapping_add_signed(sv);

    // SAFETY: caller guarantees `pad_idx` is an interior padded cell (1..=16)^3.
    unsafe {
        let src_bl = *light.get_unchecked(pad_idx) & 15;
        let center = *light.get_unchecked(out);
        let face_bl = (center & 15).max(src_bl);
        let face_sk = center >> 4;
        let face = face_bl | (face_sk << 4);

        let o0 = *occ.get_unchecked(i_mu);
        let o1 = *occ.get_unchecked(i_pu);
        let o2 = *occ.get_unchecked(i_mv);
        let o3 = *occ.get_unchecked(i_pv);
        let o4 = *occ.get_unchecked(i_mm);
        let o5 = *occ.get_unchecked(i_pm);
        let o6 = *occ.get_unchecked(i_pp);
        let o7 = *occ.get_unchecked(i_mp);

        let l_mu = *light.get_unchecked(i_mu);
        let l_pu = *light.get_unchecked(i_pu);
        let l_mv = *light.get_unchecked(i_mv);
        let l_pv = *light.get_unchecked(i_pv);
        let l_mm = *light.get_unchecked(i_mm);
        let l_pm = *light.get_unchecked(i_pm);
        let l_pp = *light.get_unchecked(i_pp);
        let l_mp = *light.get_unchecked(i_mp);

        let occ_any = (o0 | o1 | o2 | o3 | o4 | o5 | o6 | o7) & opp_bit;
        let light_diff = (l_mu ^ face)
            | (l_pu ^ face)
            | (l_mv ^ face)
            | (l_pv ^ face)
            | (l_mm ^ face)
            | (l_pm ^ face)
            | (l_pp ^ face)
            | (l_mp ^ face);

        if (occ_any | light_diff) == 0 {
            let c = u64::from(face) | (3u64 << 8);
            return c | (c << 12) | (c << 24) | (c << 36) | (u64::from(face) << 48);
        }

        let base = CornerBase {
            face_bl,
            face_sk,
            src_bl,
        };
        let s_mu = SideSample {
            occ: u8::from((o0 & opp_bit) != 0),
            bl: l_mu & 15,
            sk: l_mu >> 4,
        };
        let s_pu = SideSample {
            occ: u8::from((o1 & opp_bit) != 0),
            bl: l_pu & 15,
            sk: l_pu >> 4,
        };
        let s_mv = SideSample {
            occ: u8::from((o2 & opp_bit) != 0),
            bl: l_mv & 15,
            sk: l_mv >> 4,
        };
        let s_pv = SideSample {
            occ: u8::from((o3 & opp_bit) != 0),
            bl: l_pv & 15,
            sk: l_pv >> 4,
        };
        let o_mm = u8::from((o4 & opp_bit) != 0);
        let o_pm = u8::from((o5 & opp_bit) != 0);
        let o_pp = u8::from((o6 & opp_bit) != 0);
        let o_mp = u8::from((o7 & opp_bit) != 0);

        let c0 = shade_corner_unpacked(base, s_mu, s_mv, o_mm, l_mm);
        let c1 = shade_corner_unpacked(base, s_pu, s_mv, o_pm, l_pm);
        let c2 = shade_corner_unpacked(base, s_pu, s_pv, o_pp, l_pp);
        let c3 = shade_corner_unpacked(base, s_mu, s_pv, o_mp, l_mp);
        c0 | (c1 << 12) | (c2 << 24) | (c3 << 36) | (u64::from(face) << 48)
    }
}

#[inline(always)]
fn position(slice: usize, u: usize, v: usize, direction: Direction) -> [i32; 3] {
    let (u_axis, v_axis) = direction.tangents();
    let mut p = [0; 3];
    p[direction.axis()] = slice as i32;
    p[u_axis] = u as i32;
    p[v_axis] = v as i32;
    p
}

#[inline(always)]
fn rectangle_y(
    p: [i32; 3],
    direction: Direction,
    width: usize,
    height: usize,
    y_offset: f32,
) -> [[f32; 3]; 4] {
    let (u, v) = direction.tangents();
    let axis = direction.axis();
    let step = i32::from(direction.step() > 0);
    CORNERS.map(|(cu, cv)| {
        let mut q = p;
        q[axis] += step;
        q[u] += cu * width as i32;
        q[v] += cv * height as i32;
        [q[0] as f32, q[1] as f32 + y_offset, q[2] as f32]
    })
}

#[inline(always)]
fn rectangle(p: [i32; 3], direction: Direction, width: usize, height: usize) -> [[f32; 3]; 4] {
    rectangle_y(p, direction, width, height, 0.0)
}

#[inline(always)]
fn try_uniform_full_slice(
    blocks: &[Block; SECTION_CELLS],
    occ: &[u8; PAD_CELLS],
    light: &[u8; PAD_CELLS],
    direction: Direction,
    slice: usize,
    any_same_state_cull: bool,
    y_offset: f32,
) -> Option<Quad> {
    let dir_idx = direction as usize;
    let opp_bit = direction.opposite().bit();
    let (pad_s, pad_u, pad_v, pad_step) = DIR_PAD_STRIDES[dir_idx];
    let (sec_s, sec_u, sec_v, _) = DIR_SEC_STRIDES[dir_idx];
    let pad_origin = (PAD_SLICE + PAD_EDGE + 1) as isize + slice as isize * pad_s;
    let sec_origin = slice * sec_s;

    let b0 = blocks[sec_origin];
    if any_same_state_cull && (b0.flags & SAME_STATE_CULL) != 0 {
        return None;
    }
    let layer = if b0.flags & OPAQUE != 0 {
        Layer::Opaque
    } else if b0.flags & CUTOUT != 0 {
        Layer::Cutout
    } else {
        Layer::Translucent
    };
    let tinted = b0.flags & TINTED != 0;
    let tint0 = if tinted { b0.tint } else { 0 };
    let flags_mask = CUBE_FLAGS | TINTED | SAME_STATE_CULL;
    let f0 = b0.flags & flags_mask;
    let src_bl0 = light[pad_origin as usize] & 15;

    let out_base = pad_origin + pad_step - pad_u - pad_v;
    let out_light0 = light[out_base as usize];
    for v in 0..PAD_EDGE {
        let row = out_base + v as isize * pad_v;
        for u in 0..PAD_EDGE {
            let idx = (row + u as isize * pad_u) as usize;
            // SAFETY: out_base covers the exact 18x18 outside plane inside 0..PAD_CELLS.
            let (o, l) = unsafe { (*occ.get_unchecked(idx), *light.get_unchecked(idx)) };
            if (o & opp_bit) != 0 || l != out_light0 {
                return None;
            }
        }
    }

    for v in 0..EDGE {
        let sec_row = sec_origin + v * sec_v;
        let pad_row = pad_origin + v as isize * pad_v;
        for u in 0..EDGE {
            let b = unsafe { *blocks.get_unchecked(sec_row + u * sec_u) };
            let l = unsafe { *light.get_unchecked((pad_row + u as isize * pad_u) as usize) };
            let btint = if b.flags & TINTED != 0 { b.tint } else { 0 };
            if b.state_id != b0.state_id
                || (b.flags & flags_mask) != f0
                || btint != tint0
                || (l & 15) != src_bl0
            {
                return None;
            }
        }
    }

    let face_bl = (out_light0 & 15).max(src_bl0);
    let face_sk = out_light0 >> 4;
    let face_light = Light(face_bl | (face_sk << 4));
    let origin = position(slice, 0, 0, direction);
    Some(Quad {
        positions: rectangle_y(origin, direction, EDGE, EDGE, y_offset),
        direction,
        state_id: b0.state_id,
        tint: tint0,
        flow: [0.0; 2],
        ao: [3; 4],
        block_light: [face_bl; 4],
        sky_light: [face_sk; 4],
        face_light,
        layer,
        diagonal: 0,
        fluid_kind: NO_FLUID,
        flags: if tinted { QUAD_TINTED } else { 0 },
        reserved: 0,
    })
}

struct SliceCtx<'a, B> {
    blocks: &'a [Block; SECTION_CELLS],
    borders: &'a B,
    any_same_state_cull: bool,
    y_offset: f32,
}

#[inline(always)]
fn greedy_slice<B: BorderStates, S: QuadSink>(
    ctx: &SliceCtx<'_, B>,
    scratch: &mut MesherScratch,
    direction: Direction,
    slice: usize,
    mut vis_rows: [u16; EDGE],
    quads: &mut S,
) {
    let blocks = ctx.blocks;
    let borders = ctx.borders;
    let any_same_state_cull = ctx.any_same_state_cull;
    let y_offset = ctx.y_offset;
    let dir_idx = direction as usize;
    let opp_bit = direction.opposite().bit();
    let (pad_s, pad_u, pad_v, pad_step) = DIR_PAD_STRIDES[dir_idx];
    let (sec_s, sec_u, sec_v, sec_step) = DIR_SEC_STRIDES[dir_idx];
    let pad_origin = (PAD_SLICE + PAD_EDGE + 1) as isize + slice as isize * pad_s;
    let sec_origin = slice * sec_s;
    let on_border = if direction.step() < 0 {
        slice == 0
    } else {
        slice == EDGE - 1
    };

    let occ = &*scratch.occ;
    let light = &*scratch.light;

    if vis_rows == [u16::MAX; EDGE] {
        if let Some(quad) = try_uniform_full_slice(
            blocks,
            occ,
            light,
            direction,
            slice,
            any_same_state_cull,
            y_offset,
        ) {
            quads.push(quad);
            return;
        }
    }

    let mut adj = 0u16;
    for v in 0..EDGE {
        let r = vis_rows[v];
        adj |= r & (r >> 1);
        if v + 1 < EDGE {
            adj |= r & vis_rows[v + 1];
        }
    }
    if adj == 0 {
        for (v, &vis_row) in vis_rows.iter().enumerate() {
            let mut row = vis_row;
            if row == 0 {
                continue;
            }
            let pad_row = pad_origin + v as isize * pad_v;
            let sec_row = sec_origin + v * sec_v;
            while row != 0 {
                let u = row.trailing_zeros() as usize;
                row &= row - 1;
                let sec_idx = sec_row + u * sec_u;
                // SAFETY: slice, u, v in 0..16 guarantees sec_idx in 0..4096.
                let block = unsafe { *blocks.get_unchecked(sec_idx) };
                let pad_idx = (pad_row + u as isize * pad_u) as usize;
                if any_same_state_cull && (block.flags & SAME_STATE_CULL) != 0 {
                    let next_pad = pad_idx.wrapping_add_signed(pad_step);
                    // SAFETY: pad_idx is interior (1..=16)^3 so next_pad is in 0..PAD_CELLS.
                    if (unsafe { *occ.get_unchecked(next_pad) } & SAME_STATE_CULL) != 0 {
                        let next_state = if on_border {
                            let p = position(slice, u, v, direction);
                            borders.border_state_id(
                                direction,
                                p[0] as usize,
                                p[1] as usize,
                                p[2] as usize,
                            )
                        } else {
                            blocks[(sec_idx as isize + sec_step) as usize].state_id
                        };
                        if next_state == block.state_id {
                            continue;
                        }
                    }
                }
                let layer = if block.flags & OPAQUE != 0 {
                    Layer::Opaque
                } else if block.flags & CUTOUT != 0 {
                    Layer::Cutout
                } else {
                    Layer::Translucent
                };
                let tinted = block.flags & TINTED != 0;
                // SAFETY: slice, u, v in 0..16 guarantees pad_idx is an interior padded cell (1..=16)^3.
                let shade_bits =
                    unsafe { shade_fast(occ, light, pad_idx, pad_step, pad_u, pad_v, opp_bit) };
                let face = PackedFace::new(
                    block.state_id,
                    if tinted { block.tint } else { 0 },
                    layer,
                    if tinted { QUAD_TINTED } else { 0 },
                    shade_bits,
                );
                let origin = position(slice, u, v, direction);
                quads.push(face.quad(direction, rectangle_y(origin, direction, 1, 1, y_offset)));
            }
        }
        return;
    }

    let face_mask = &mut scratch.face_mask;

    for (v, vis_row) in vis_rows.iter_mut().enumerate() {
        let mut row = *vis_row;
        if row == 0 {
            continue;
        }
        let pad_row = pad_origin + v as isize * pad_v;
        let sec_row = sec_origin + v * sec_v;
        let mask_row = v * EDGE;
        let mut active = row;
        while row != 0 {
            let u = row.trailing_zeros() as usize;
            row &= row - 1;
            let sec_idx = sec_row + u * sec_u;
            // SAFETY: slice, u, v in 0..16 guarantees sec_idx in 0..4096.
            let block = unsafe { *blocks.get_unchecked(sec_idx) };
            let pad_idx = (pad_row + u as isize * pad_u) as usize;
            if any_same_state_cull && (block.flags & SAME_STATE_CULL) != 0 {
                let next_pad = pad_idx.wrapping_add_signed(pad_step);
                // SAFETY: pad_idx is interior (1..=16)^3 so next_pad is in 0..PAD_CELLS.
                if (unsafe { *occ.get_unchecked(next_pad) } & SAME_STATE_CULL) != 0 {
                    let next_state = if on_border {
                        let p = position(slice, u, v, direction);
                        borders.border_state_id(
                            direction,
                            p[0] as usize,
                            p[1] as usize,
                            p[2] as usize,
                        )
                    } else {
                        blocks[(sec_idx as isize + sec_step) as usize].state_id
                    };
                    if next_state == block.state_id {
                        active &= !(1u16 << u);
                        continue;
                    }
                }
            }
            let layer = if block.flags & OPAQUE != 0 {
                Layer::Opaque
            } else if block.flags & CUTOUT != 0 {
                Layer::Cutout
            } else {
                Layer::Translucent
            };
            let tinted = block.flags & TINTED != 0;
            // SAFETY: slice, u, v in 0..16 guarantees pad_idx is an interior padded cell (1..=16)^3.
            let shade_bits =
                unsafe { shade_fast(occ, light, pad_idx, pad_step, pad_u, pad_v, opp_bit) };
            // SAFETY: v in 0..16 and u in 0..16 so mask_row + u < 256.
            unsafe {
                *face_mask.get_unchecked_mut(mask_row + u) = PackedFace::new(
                    block.state_id,
                    if tinted { block.tint } else { 0 },
                    layer,
                    if tinted { QUAD_TINTED } else { 0 },
                    shade_bits,
                );
            }
        }
        *vis_row = active;
    }

    for v in 0..EDGE {
        let mut row = vis_rows[v];
        while row != 0 {
            let u = row.trailing_zeros() as usize;
            let index = v * EDGE + u;
            // SAFETY: v, u in 0..16 so index < 256.
            let face = unsafe { *face_mask.get_unchecked(index) };
            let width = if face.can_merge_u() {
                let max_w = (!(row >> u)).trailing_zeros() as usize;
                let mut w = 1;
                while w < max_w && unsafe { *face_mask.get_unchecked(index + w) } == face {
                    w += 1;
                }
                w
            } else {
                1
            };
            let span_mask = (((1u32 << width) - 1) << u) as u16;
            let mut height = 1;
            if face.can_merge_v() {
                while v + height < EDGE {
                    if (vis_rows[v + height] & span_mask) != span_mask {
                        break;
                    }
                    let start = index + height * EDGE;
                    let mut all_eq = true;
                    for k in 0..width {
                        // SAFETY: v + height < 16 and u + width <= 16.
                        if unsafe { *face_mask.get_unchecked(start + k) } != face {
                            all_eq = false;
                            break;
                        }
                    }
                    if !all_eq {
                        break;
                    }
                    height += 1;
                }
            }
            let clear = !span_mask;
            for vis_row in &mut vis_rows[v..v + height] {
                *vis_row &= clear;
            }
            row &= clear;
            let origin = position(slice, u, v, direction);
            quads.push(face.quad(
                direction,
                rectangle_y(origin, direction, width, height, y_offset),
            ));
        }
    }
}

fn mesh_cubes_fast<B: BorderStates, S: QuadSink>(
    blocks: &[Block; SECTION_CELLS],
    borders: &B,
    scratch: &mut MesherScratch,
    any_same_state_cull: bool,
    any_partial_occ: bool,
    y_offset: f32,
    quads: &mut S,
) {
    let ctx = SliceCtx {
        blocks,
        borders,
        any_same_state_cull,
        y_offset,
    };
    let occ_up_idx = if any_partial_occ {
        Direction::Up as usize
    } else {
        0
    };
    let occ_down_idx = 0usize;
    let occ_south_idx = if any_partial_occ {
        Direction::South as usize
    } else {
        0
    };
    let occ_north_idx = if any_partial_occ {
        Direction::North as usize
    } else {
        0
    };
    let occ_east_idx = if any_partial_occ {
        Direction::East as usize
    } else {
        0
    };
    let occ_west_idx = if any_partial_occ {
        Direction::West as usize
    } else {
        0
    };

    // 0: Direction::Down (-Y)
    {
        let mut border_occ = [0u16; EDGE];
        let opp_bit = Direction::Up.bit();
        for (z, slot) in border_occ.iter_mut().enumerate() {
            let base = (z + 1) * PAD_EDGE + 1;
            let mut m = 0u16;
            for x in 0..EDGE {
                if (scratch.occ[base + x] & opp_bit) != 0 {
                    m |= 1u16 << x;
                }
            }
            *slot = m;
        }
        for y in 0..EDGE {
            let mut vis_rows = [0u16; EDGE];
            let mut any = 0u16;
            let base = y * EDGE;
            if y == 0 {
                for (z, (&b_occ, slot)) in border_occ.iter().zip(&mut vis_rows).enumerate() {
                    let v = scratch.cube_rows[base + z] & !b_occ;
                    *slot = v;
                    any |= v;
                }
            } else {
                let prev = (y - 1) * EDGE;
                for (z, slot) in vis_rows.iter_mut().enumerate() {
                    let v = scratch.cube_rows[base + z] & !scratch.occ_rows[occ_up_idx][prev + z];
                    *slot = v;
                    any |= v;
                }
            }
            if any != 0 {
                greedy_slice(&ctx, scratch, Direction::Down, y, vis_rows, quads);
            }
        }
    }

    // 1: Direction::Up (+Y)
    {
        let mut border_occ = [0u16; EDGE];
        let opp_bit = Direction::Down.bit();
        let top_plane = (PAD_EDGE - 1) * PAD_SLICE;
        for (z, slot) in border_occ.iter_mut().enumerate() {
            let base = top_plane + (z + 1) * PAD_EDGE + 1;
            let mut m = 0u16;
            for x in 0..EDGE {
                if (scratch.occ[base + x] & opp_bit) != 0 {
                    m |= 1u16 << x;
                }
            }
            *slot = m;
        }
        for y in 0..EDGE {
            let mut raw_vis = [0u16; EDGE];
            let mut any = 0u16;
            let base = y * EDGE;
            if y == EDGE - 1 {
                for (z, (&b_occ, slot)) in border_occ.iter().zip(&mut raw_vis).enumerate() {
                    let v = scratch.cube_rows[base + z] & !b_occ;
                    *slot = v;
                    any |= v;
                }
            } else {
                let next = (y + 1) * EDGE;
                for (z, slot) in raw_vis.iter_mut().enumerate() {
                    let v = scratch.cube_rows[base + z] & !scratch.occ_rows[occ_down_idx][next + z];
                    *slot = v;
                    any |= v;
                }
            }
            if any != 0 {
                let vis_rows = transpose_16x16(raw_vis);
                greedy_slice(&ctx, scratch, Direction::Up, y, vis_rows, quads);
            }
        }
    }

    // 2: Direction::North (-Z)
    {
        let mut border_occ = [0u16; EDGE];
        let opp_bit = Direction::South.bit();
        for (y, slot) in border_occ.iter_mut().enumerate() {
            let base = (y + 1) * PAD_SLICE + 1;
            let mut m = 0u16;
            for x in 0..EDGE {
                if (scratch.occ[base + x] & opp_bit) != 0 {
                    m |= 1u16 << x;
                }
            }
            *slot = m;
        }
        for z in 0..EDGE {
            let mut raw_vis = [0u16; EDGE];
            let mut any = 0u16;
            if z == 0 {
                for (y, (&b_occ, slot)) in border_occ.iter().zip(&mut raw_vis).enumerate() {
                    let v = scratch.cube_rows[y * EDGE] & !b_occ;
                    *slot = v;
                    any |= v;
                }
            } else {
                for (y, slot) in raw_vis.iter_mut().enumerate() {
                    let idx = y * EDGE + z;
                    let v = scratch.cube_rows[idx] & !scratch.occ_rows[occ_south_idx][idx - 1];
                    *slot = v;
                    any |= v;
                }
            }
            if any != 0 {
                let vis_rows = transpose_16x16(raw_vis);
                greedy_slice(&ctx, scratch, Direction::North, z, vis_rows, quads);
            }
        }
    }

    // 3: Direction::South (+Z)
    {
        let mut border_occ = [0u16; EDGE];
        let opp_bit = Direction::North.bit();
        let south_offset = (PAD_EDGE - 1) * PAD_EDGE + 1;
        for (y, slot) in border_occ.iter_mut().enumerate() {
            let base = (y + 1) * PAD_SLICE + south_offset;
            let mut m = 0u16;
            for x in 0..EDGE {
                if (scratch.occ[base + x] & opp_bit) != 0 {
                    m |= 1u16 << x;
                }
            }
            *slot = m;
        }
        for z in 0..EDGE {
            let mut vis_rows = [0u16; EDGE];
            let mut any = 0u16;
            if z == EDGE - 1 {
                for (y, (&b_occ, slot)) in border_occ.iter().zip(&mut vis_rows).enumerate() {
                    let v = scratch.cube_rows[y * EDGE + (EDGE - 1)] & !b_occ;
                    *slot = v;
                    any |= v;
                }
            } else {
                for (y, slot) in vis_rows.iter_mut().enumerate() {
                    let idx = y * EDGE + z;
                    let v = scratch.cube_rows[idx] & !scratch.occ_rows[occ_north_idx][idx + 1];
                    *slot = v;
                    any |= v;
                }
            }
            if any != 0 {
                greedy_slice(&ctx, scratch, Direction::South, z, vis_rows, quads);
            }
        }
    }

    // 4: Direction::West (-X)
    {
        let opp_bit = Direction::East.bit();
        let mut west_by_x = [[0u16; EDGE]; EDGE];
        let mut active_x = 0u16;
        #[allow(clippy::needless_range_loop)]
        for y in 0..EDGE {
            let pad_y = (y + 1) * PAD_SLICE;
            let base = y * EDGE;
            let mut raw = [0u16; EDGE];
            let mut row_any = 0u16;
            for (z, slot) in raw.iter_mut().enumerate() {
                let border_bit =
                    u16::from((scratch.occ[pad_y + (z + 1) * PAD_EDGE] & opp_bit) != 0);
                let occ_e = scratch.occ_rows[occ_east_idx][base + z];
                let v = scratch.cube_rows[base + z] & !((occ_e << 1) | border_bit);
                *slot = v;
                row_any |= v;
            }
            if row_any != 0 {
                active_x |= row_any;
                let t = transpose_16x16(raw);
                let mut m = row_any;
                while m != 0 {
                    let x = m.trailing_zeros() as usize;
                    m &= m - 1;
                    west_by_x[x][y] = t[x];
                }
            }
        }
        while active_x != 0 {
            let x = active_x.trailing_zeros() as usize;
            active_x &= active_x - 1;
            greedy_slice(&ctx, scratch, Direction::West, x, west_by_x[x], quads);
        }
    }

    // 5: Direction::East (+X)
    {
        let opp_bit = Direction::West.bit();
        let mut east_by_x = [[0u16; EDGE]; EDGE];
        let mut active_x = 0u16;
        #[allow(clippy::needless_range_loop)]
        for y in 0..EDGE {
            let pad_y = (y + 1) * PAD_SLICE + (PAD_EDGE - 1);
            let base = y * EDGE;
            let mut raw = [0u16; EDGE];
            let mut row_any = 0u16;
            for (z, slot) in raw.iter_mut().enumerate() {
                let border_bit =
                    u16::from((scratch.occ[pad_y + (z + 1) * PAD_EDGE] & opp_bit) != 0);
                let occ_w = scratch.occ_rows[occ_west_idx][base + z];
                let v = scratch.cube_rows[base + z] & !((occ_w >> 1) | (border_bit << 15));
                *slot = v;
                row_any |= v;
            }
            if row_any != 0 {
                active_x |= row_any;
                let t = transpose_16x16(raw);
                let mut m = row_any;
                while m != 0 {
                    let x = m.trailing_zeros() as usize;
                    m &= m - 1;
                    east_by_x[x][y] = t[x];
                }
            }
        }
        while active_x != 0 {
            let x = active_x.trailing_zeros() as usize;
            active_x &= active_x - 1;
            let vis_rows = transpose_16x16(east_by_x[x]);
            greedy_slice(&ctx, scratch, Direction::East, x, vis_rows, quads);
        }
    }
}

#[inline(always)]
fn fluid_level(block: Block) -> u8 {
    if block.flags & WATERLOGGED != 0 {
        0
    } else {
        block.fluid_level
    }
}

/// Height shared by the four cells meeting at this horizontal corner. Integer
/// accumulation keeps the interpolation order independent across section seams.
///
/// SAFETY invariant: `base_idx` corresponds to `(px, py, pz)` with `px, pz in 1..=17`
/// and `py in 1..=16`, so `idx` in `{base_idx - 19, base_idx - 1, base_idx - 18, base_idx}`
/// and `idx + PAD_SLICE` are all strictly less than `PAD_CELLS`.
#[inline(always)]
unsafe fn fluid_corner_fast(
    occ: &[u8; PAD_CELLS],
    fluid: &[u8; PAD_CELLS],
    base_idx: usize,
    kind: u8,
) -> f32 {
    let mut amount_sum = 0u32;
    let mut weight_sum = 0u32;
    let offsets = [
        base_idx - 1 - PAD_EDGE,
        base_idx - 1,
        base_idx - PAD_EDGE,
        base_idx,
    ];
    for idx in offsets {
        debug_assert!(idx + PAD_SLICE < PAD_CELLS);
        // SAFETY: caller guarantees base_idx bounds.
        let f = unsafe { *fluid.get_unchecked(idx) };
        if (f & 3) == kind {
            // SAFETY: caller guarantees idx + PAD_SLICE < PAD_CELLS.
            if (unsafe { *fluid.get_unchecked(idx + PAD_SLICE) } & 3) == kind {
                return 1.0;
            }
            let amount = u32::from(f >> 2);
            let weight = if amount == 8 { 10 } else { 1 };
            amount_sum += amount * weight;
            weight_sum += weight;
        } else if (unsafe { *occ.get_unchecked(idx) } & ALL_FACES) != ALL_FACES {
            weight_sum += 1;
        }
    }
    amount_sum as f32 / (weight_sum * 9) as f32
}

fn mesh_fluids_fast<S: QuadSink>(
    blocks: &[Block; SECTION_CELLS],
    scratch: &MesherScratch,
    y_offset: f32,
    quads: &mut S,
) {
    let occ = &*scratch.occ;
    let light = &*scratch.light;
    let fluid = &*scratch.fluid;

    for y in 0..EDGE {
        let pad_y = (y + 1) * PAD_SLICE;
        let sec_y = y * FACE_CELLS;
        let row_y = y * EDGE;
        for z in 0..EDGE {
            let mut fm = scratch.fluid_rows[row_y + z];
            if fm == 0 {
                continue;
            }
            let pad_row = pad_y + (z + 1) * PAD_EDGE + 1;
            let sec_row = sec_y + z * EDGE;
            while fm != 0 {
                let x = fm.trailing_zeros() as usize;
                fm &= fm - 1;
                // SAFETY: x, y, z in 0..16 so sec_row + x < 4096 and pad_idx is in (1..=16)^3.
                let (block, pad_idx, kind, b_occ, n_down, n_up, n_north, n_south, n_west, n_east) = unsafe {
                    let pad_idx = pad_row + x;
                    (
                        *blocks.get_unchecked(sec_row + x),
                        pad_idx,
                        *fluid.get_unchecked(pad_idx) & 3,
                        *occ.get_unchecked(pad_idx),
                        pad_idx - PAD_SLICE,
                        pad_idx + PAD_SLICE,
                        pad_idx - PAD_EDGE,
                        pad_idx + PAD_EDGE,
                        pad_idx - 1,
                        pad_idx + 1,
                    )
                };

                // SAFETY: pad_idx is in (1..=16)^3 so all 6 face neighbors are in 0..PAD_CELLS.
                let (mut vis_mask, same_above) = unsafe {
                    let up_same = (*fluid.get_unchecked(n_up) & 3) == kind;
                    let mut m = 0u8;
                    if (*fluid.get_unchecked(n_down) & 3) != kind
                        && (b_occ & (1 << 0)) == 0
                        && (*occ.get_unchecked(n_down) & (1 << 1)) == 0
                    {
                        m |= 1 << 0;
                    }
                    if !up_same && (b_occ & (1 << 1)) == 0 {
                        m |= 1 << 1;
                    }
                    if (*fluid.get_unchecked(n_north) & 3) != kind
                        && (b_occ & (1 << 2)) == 0
                        && (*occ.get_unchecked(n_north) & (1 << 3)) == 0
                    {
                        m |= 1 << 2;
                    }
                    if (*fluid.get_unchecked(n_south) & 3) != kind
                        && (b_occ & (1 << 3)) == 0
                        && (*occ.get_unchecked(n_south) & (1 << 2)) == 0
                    {
                        m |= 1 << 3;
                    }
                    if (*fluid.get_unchecked(n_west) & 3) != kind
                        && (b_occ & (1 << 4)) == 0
                        && (*occ.get_unchecked(n_west) & (1 << 5)) == 0
                    {
                        m |= 1 << 4;
                    }
                    if (*fluid.get_unchecked(n_east) & 3) != kind
                        && (b_occ & (1 << 5)) == 0
                        && (*occ.get_unchecked(n_east) & (1 << 4)) == 0
                    {
                        m |= 1 << 5;
                    }
                    (m, up_same)
                };
                if vis_mask == 0 {
                    continue;
                }

                let (h, flow) = if same_above {
                    ([[1.0f32; 2]; 2], [0.0f32; 2])
                } else {
                    // SAFETY: x, y, z in 0..16 so pad_idx has (px, py, pz) in (1..=16)^3.
                    let h = unsafe {
                        [
                            [
                                fluid_corner_fast(occ, fluid, pad_idx, kind),
                                fluid_corner_fast(occ, fluid, pad_idx + 1, kind),
                            ],
                            [
                                fluid_corner_fast(occ, fluid, pad_idx + PAD_EDGE, kind),
                                fluid_corner_fast(occ, fluid, pad_idx + PAD_EDGE + 1, kind),
                            ],
                        ]
                    };
                    if h[0][0] == 1.0
                        && h[0][1] == 1.0
                        && h[1][0] == 1.0
                        && h[1][1] == 1.0
                        && (unsafe { *occ.get_unchecked(n_up) } & (1 << 0)) != 0
                    {
                        vis_mask &= !(1 << 1);
                        if vis_mask == 0 {
                            continue;
                        }
                    }
                    let flow = [
                        ((h[0][0] - h[0][1]) + (h[1][0] - h[1][1])) * 0.5,
                        ((h[0][0] - h[1][0]) + (h[0][1] - h[1][1])) * 0.5,
                    ];
                    (h, flow)
                };

                let p = [x as i32, y as i32, z as i32];
                let yf = y as f32;
                let tinted = block.flags & FLUID_TINTED != 0;
                let tint = if tinted { block.fluid_tint } else { 0 };
                let flags = (if tinted { QUAD_TINTED } else { 0 })
                    | (if fluid_level(block) >= 8 {
                        QUAD_FALLING
                    } else {
                        0
                    });

                let mut vm = vis_mask;
                while vm != 0 {
                    let d = vm.trailing_zeros() as usize;
                    vm &= vm - 1;
                    let direction = Direction::ALL[d];
                    let mut positions = rectangle(p, direction, 1, 1);
                    for vertex in &mut positions {
                        if vertex[1] > yf {
                            let local_y = yf
                                + h[(vertex[2] as i32 - z as i32) as usize]
                                    [(vertex[0] as i32 - x as i32) as usize];
                            vertex[1] = local_y + y_offset;
                        } else {
                            vertex[1] += y_offset;
                        }
                    }
                    let (_, pad_u, pad_v, pad_step) = DIR_PAD_STRIDES[d];
                    // SAFETY: pad_idx is an interior padded cell (1..=16)^3.
                    let s = unsafe {
                        shade_fast(
                            occ,
                            light,
                            pad_idx,
                            pad_step,
                            pad_u,
                            pad_v,
                            direction.opposite().bit(),
                        )
                    };
                    let ao0 = ((s >> 8) & 3) as u8;
                    let ao1 = ((s >> 20) & 3) as u8;
                    let ao2 = ((s >> 32) & 3) as u8;
                    let ao3 = ((s >> 44) & 3) as u8;
                    let l0 = (s & 0xff) as u8;
                    let l1 = ((s >> 12) & 0xff) as u8;
                    let l2 = ((s >> 24) & 0xff) as u8;
                    let l3 = ((s >> 36) & 0xff) as u8;
                    quads.push(Quad {
                        positions,
                        direction,
                        state_id: block.state_id,
                        tint,
                        flow,
                        ao: [ao0, ao1, ao2, ao3],
                        block_light: [l0 & 15, l1 & 15, l2 & 15, l3 & 15],
                        sky_light: [l0 >> 4, l1 >> 4, l2 >> 4, l3 >> 4],
                        face_light: Light(((s >> 48) & 0xff) as u8),
                        layer: Layer::Fluid,
                        diagonal: u8::from(ao0 + ao2 > ao1 + ao3),
                        fluid_kind: kind,
                        flags,
                        reserved: 0,
                    });
                }
            }
        }
    }
}

pub(crate) fn mesh_loaded_section<B: BorderStates, S: QuadSink>(
    blocks: &[Block; SECTION_CELLS],
    borders: &B,
    scan: InteriorScan,
    scratch: &mut MesherScratch,
    y_offset: f32,
    quads: &mut S,
) {
    if scan.any_cube {
        mesh_cubes_fast(
            blocks,
            borders,
            scratch,
            scan.any_same_state_cull,
            scan.any_partial_occ,
            y_offset,
            quads,
        );
    }
    if scan.any_fluid {
        mesh_fluids_fast(blocks, scratch, y_offset, quads);
    }
}

pub(crate) fn mesh_section_with_scratch<S: QuadSink>(
    view: &SectionView<'_>,
    scratch: &mut MesherScratch,
    y_offset: f32,
    quads: &mut S,
) {
    let scan = load_interior(view.blocks, view.block_light, view.sky_light, scratch);
    if !scan.any_cube && !scan.any_fluid {
        return;
    }
    load_section_shell(view, scan.any_fluid, scratch);
    let borders = SectionViewBorders(view);
    mesh_loaded_section(view.blocks, &borders, scan, scratch, y_offset, quads);
}

/// Mesh the current section in stable direction/slice/row order, then append fluids
/// in section index order. All opaque, cutout and translucent models are unit cubes.
pub fn mesh_section(view: &SectionView<'_>) -> Vec<Quad> {
    with_scratch(|scratch| {
        let mut quads = Vec::new();
        mesh_section_with_scratch(view, scratch, 0.0, &mut quads);
        quads
    })
}
