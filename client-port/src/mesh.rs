// SPDX-License-Identifier: LGPL-3.0-or-later

use std::cell::RefCell;
use std::sync::OnceLock;

use crate::section::{
    Block, BlockShape, Direction, Light, MissingNeighbor, Sample, SectionView, ALL_FACES, CUTOUT,
    EDGE, FACE_CELLS, FLUID_TINTED, LIGHT_BYTES, NO_FLUID, OPAQUE, SAME_STATE_CULL, SECTION_CELLS,
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

/// Merge payload for one flat (uniform corner height) fluid `+Y`/`-Y` face:
/// everything a merged quad reuses verbatim — the block material, tint,
/// flags, kind, the shared corner height and flow vector, and the full
/// four-corner shade word. Two faces merge only when every field is equal,
/// so a merged quad emits exactly the union of the per-cell faces it
/// replaces.
#[derive(Clone, Copy, PartialEq)]
struct FlatFluidFace {
    state_id: u32,
    tint: u32,
    shade: u64,
    height: f32,
    flow: [f32; 2],
    kind: u8,
    flags: u8,
}

impl FlatFluidFace {
    /// The same shared-edge rule as [`PackedFace::can_merge_u`]: merging
    /// along u requires the corner shading constant along u.
    #[inline(always)]
    fn can_merge_u(self) -> bool {
        ((self.shade ^ (self.shade >> 12)) & 0x0000_000f_ff00_0fff) == 0
    }

    /// The same shared-edge rule as [`PackedFace::can_merge_v`].
    #[inline(always)]
    fn can_merge_v(self) -> bool {
        let s = self.shade;
        (s & 0xfff) == ((s >> 36) & 0xfff) && ((s >> 12) & 0xfff) == ((s >> 24) & 0xfff)
    }

    #[inline(always)]
    fn quad(self, direction: Direction, positions: [[f32; 3]; 4]) -> Quad {
        let s = self.shade;
        let ao0 = ((s >> 8) & 3) as u8;
        let ao1 = ((s >> 20) & 3) as u8;
        let ao2 = ((s >> 32) & 3) as u8;
        let ao3 = ((s >> 44) & 3) as u8;
        let l0 = (s & 0xff) as u8;
        let l1 = ((s >> 12) & 0xff) as u8;
        let l2 = ((s >> 24) & 0xff) as u8;
        let l3 = ((s >> 36) & 0xff) as u8;
        Quad {
            positions,
            direction,
            state_id: self.state_id,
            tint: self.tint,
            flow: self.flow,
            ao: [ao0, ao1, ao2, ao3],
            block_light: [l0 & 15, l1 & 15, l2 & 15, l3 & 15],
            sky_light: [l0 >> 4, l1 >> 4, l2 >> 4, l3 >> 4],
            face_light: Light(((s >> 48) & 0xff) as u8),
            layer: Layer::Fluid,
            diagonal: u8::from(ao0 + ao2 > ao1 + ao3),
            fluid_kind: self.kind,
            flags: self.flags,
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
    let cube = if block.shape() == BlockShape::Cube && block.flags & CUBE_FLAGS != 0 {
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

/// Spreads the four low bytes of `x` to byte positions 0, 2, 4, 6 of a `u64`,
/// zeroing the gaps: the byte-wise interleave step of [`unpack_light_row`].
#[inline(always)]
fn spread_bytes4(x: u32) -> u64 {
    let x = (u64::from(x) | (u64::from(x) << 16)) & 0x0000_ffff_0000_ffff;
    (x | (x << 8)) & 0x00ff_00ff_00ff_00ff
}

/// Resolved once: the adaptive kernel's tier/LUT choice is fixed per host.
fn light_merge_strategy() -> &'static megu_core::hw_adaptive_engine::ComputeStrategy {
    static STRATEGY: OnceLock<megu_core::hw_adaptive_engine::ComputeStrategy> = OnceLock::new();
    STRATEGY.get_or_init(|| {
        megu_core::hw_adaptive_engine::select_compute_strategy(
            &megu_core::hw_adaptive_engine::detect_host_hardware_caps(),
        )
    })
}

/// Nibble-unpack every interior row of `scratch.light`. The kernel's
/// per-nibble max packs each cell's `max(block, sky)` light: a zero merged
/// byte proves both cells it covers are fully dark, so fully dark rows skip
/// their unpack and zero-fill directly.
fn fill_light_rows(
    block_light: &[u8; LIGHT_BYTES],
    sky_light: &[u8; LIGHT_BYTES],
    scratch: &mut MesherScratch,
) {
    let merged = megu_core::hw_adaptive_engine::max_nibble_arrays_adaptive(
        block_light,
        sky_light,
        light_merge_strategy(),
    );
    for y in 0..EDGE {
        let pad_y = (y + 1) * PAD_SLICE;
        let light_y = y * (FACE_CELLS / 2);
        for z in 0..EDGE {
            let pad_base = pad_y + (z + 1) * PAD_EDGE + 1;
            let src = light_y + z * (EDGE / 2);
            if merged[src..src + EDGE / 2].iter().all(|&b| b == 0) {
                scratch.light[pad_base..pad_base + EDGE].fill(0);
            } else {
                unpack_light_row(
                    block_light,
                    sky_light,
                    src,
                    &mut scratch.light[pad_base..pad_base + EDGE],
                );
            }
        }
    }
}

#[inline(always)]
pub(crate) fn unpack_light_row(
    block_light: &[u8; LIGHT_BYTES],
    sky_light: &[u8; LIGHT_BYTES],
    src_byte_offset: usize,
    dst: &mut [u8],
) {
    // Nibble-zip two packed light bytes into one combined light byte per cell:
    // `out[2k] = lo(bl[k]) | lo(sl[k]) << 4`, `out[2k+1] = hi(bl[k]) | hi(sl[k])`.
    // A `u64` load plus two masked shifts produces all eight bytes of each
    // parity at once; `spread_bytes4` interleaves them into the 16 output cells.
    let bl = u64::from_le_bytes(
        block_light[src_byte_offset..src_byte_offset + 8]
            .try_into()
            .unwrap(),
    );
    let sl = u64::from_le_bytes(
        sky_light[src_byte_offset..src_byte_offset + 8]
            .try_into()
            .unwrap(),
    );
    let lo = (bl & 0x0f0f_0f0f_0f0f_0f0f) | ((sl & 0x0f0f_0f0f_0f0f_0f0f) << 4);
    let hi = ((bl >> 4) & 0x0f0f_0f0f_0f0f_0f0f) | (sl & 0xf0f0_f0f0_f0f0_f0f0);
    let out_lo = spread_bytes4(lo as u32) | (spread_bytes4(hi as u32) << 8);
    let out_hi = spread_bytes4((lo >> 32) as u32) | (spread_bytes4((hi >> 32) as u32) << 8);
    dst[..8].copy_from_slice(&out_lo.to_le_bytes());
    dst[8..16].copy_from_slice(&out_hi.to_le_bytes());
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

// ---------------------------------------------------------------------------
// Circuit-level binary face extraction
// ---------------------------------------------------------------------------
//
// Face culling runs on 64-bit occupancy bitboards. Each `u64` packs four
// consecutive 16-block strips (one `u16` lane each), so bit `16*k + x` of a
// word is bit `x` of strip `k`. With the per-strip masks laid out this way a
// single `solid & !(solid >> 1)`-style expression culls 64 voxels — four whole
// strips — per instruction instead of one strip at a time:
//
// * Axis-crossing neighbours (the ±Y and ±Z directions, where the occluding
//   cell lives in the adjacent strip) reduce to `cube4 & !occ4`: one AND-NOT
//   per four strips.
// * In-strip neighbours (±X, where the occluding cell is the previous/next
//   bit in the same lane) use `cube4 & !((occ4 << 1) & LANES_HIGH | border4)`
//   and `cube4 & !((occ4 >> 1) & LANES_LOW | border4)`: the lane masks stop
//   bits leaking across strip boundaries, and `border4` carries the one-block
//   neighbour border (the far section's occlusion) folded into the same word.
//
// The extracted masks are identical to a per-strip `u16` evaluation, bit for
// bit, so the greedy merge below sees exactly the same visible faces.

/// Keeps every bit of each 16-bit lane except bit 0: used after `<< 1` so the
/// top bit of strip `k` cannot leak into strip `k + 1`.
const LANES_HIGH: u64 = 0xfffe_fffe_fffe_fffe;
/// Keeps every bit of each 16-bit lane except bit 15: used after `>> 1` so the
/// bottom bit of strip `k + 1` cannot leak into strip `k`.
const LANES_LOW: u64 = 0x7fff_7fff_7fff_7fff;

/// Packs four consecutive `u16` strip masks into one `u64` bitboard.
#[inline(always)]
fn pack_strips4(rows: &[u16], base: usize) -> u64 {
    u64::from(rows[base])
        | (u64::from(rows[base + 1]) << 16)
        | (u64::from(rows[base + 2]) << 32)
        | (u64::from(rows[base + 3]) << 48)
}

/// [`pack_strips4`] for strips `EDGE` apart: lanes land on `rows[base]`,
/// `rows[base + EDGE]`, `rows[base + 2*EDGE]`, `rows[base + 3*EDGE]`.
#[inline(always)]
fn pack_strips4_step(rows: &[u16], base: usize) -> u64 {
    u64::from(rows[base])
        | (u64::from(rows[base + EDGE]) << 16)
        | (u64::from(rows[base + 2 * EDGE]) << 32)
        | (u64::from(rows[base + 3 * EDGE]) << 48)
}

/// Writes a packed `u64` bitboard back as four `u16` strip masks.
#[inline(always)]
fn store_strips4(dst: &mut [u16], base: usize, word: u64) {
    dst[base] = word as u16;
    dst[base + 1] = (word >> 16) as u16;
    dst[base + 2] = (word >> 32) as u16;
    dst[base + 3] = (word >> 48) as u16;
}

/// ORs the four lanes of a packed `u64` bitboard into a single `u16`.
#[inline(always)]
fn fold_strips4(word: u64) -> u16 {
    (word | (word >> 16) | (word >> 32) | (word >> 48)) as u16
}

/// Byte-wise single-bit test packed to a `u16`: bit `i` of the result is set
/// when `bytes[i]` has the single-bit value `bit` set. Isolates the tested
/// bit per byte, then gathers the eight flag bytes into the top byte with one
/// multiply — the portable `pmovmskb` for a whole border row in ~10 ops.
#[inline(always)]
fn bitmask16(bytes: &[u8], bit: u8) -> u16 {
    debug_assert!(bit.is_power_of_two());
    let lo = u64::from_le_bytes(bytes[..8].try_into().unwrap());
    let hi = u64::from_le_bytes(bytes[8..16].try_into().unwrap());
    let shift = bit.trailing_zeros();
    let gather = |w: u64| -> u64 {
        ((w >> shift) & 0x0101_0101_0101_0101).wrapping_mul(0x0102_0408_1020_4080) >> 56
    };
    (gather(lo) | (gather(hi) << 8)) as u16
}

/// Border row occlusion for one face direction over a contiguous 16-byte run
/// of the padded `occ` array, as four packed strip lanes.
#[inline(always)]
fn border_lane4(occ: &[u8; PAD_CELLS], base: usize, step: usize, bit: u8) -> u64 {
    let mut word = 0u64;
    for k in 0..4 {
        word |= u64::from(bitmask16(&occ[base + step * k..base + step * k + 16], bit)) << (16 * k);
    }
    word
}

#[derive(Clone, Copy)]
pub(crate) struct InteriorScan {
    pub any_cube: bool,
    pub any_fluid: bool,
    pub any_shape: bool,
    pub any_same_state_cull: bool,
    pub any_partial_occ: bool,
    pub uniform: Option<Block>,
}

/// The single block value shared by every cell, or `None` when the section
/// mixes blocks. Deep-stone and ocean sections are uniform in practice; the
/// scan exits on the first differing cell, so mixed sections pay only a few
/// compares.
#[inline(always)]
fn uniform_block(blocks: &[Block; SECTION_CELLS]) -> Option<Block> {
    let first = blocks[0];
    blocks[1..].iter().all(|&b| b == first).then_some(first)
}

/// [`load_interior`] specialized for a section whose every cell holds `block`:
/// the strip masks degenerate to all-ones or all-zero and the padded occ
/// interior is a repeated byte, so no per-cell classification runs at all.
fn load_interior_uniform(
    block: Block,
    block_light: &[u8; LIGHT_BYTES],
    sky_light: &[u8; LIGHT_BYTES],
    scratch: &mut MesherScratch,
) -> InteriorScan {
    let occ = encode_occ(block);
    let any_cube = occ & CUBE_BIT != 0;
    let any_fluid = block.fluid() != NO_FLUID;
    let any_shape = block.shape() != BlockShape::Cube;
    let any_same_state_cull = occ & SAME_STATE_CULL != 0;
    let any_partial_occ = {
        let bits = occ & ALL_FACES;
        bits != 0 && bits != ALL_FACES
    };

    scratch.cube_rows.fill(u16::from(any_cube) * u16::MAX);
    scratch.fluid_rows.fill(u16::from(any_fluid) * u16::MAX);
    let occ_full = u16::from((occ & ALL_FACES) == ALL_FACES) * u16::MAX;
    for (d, rows) in scratch.occ_rows.iter_mut().enumerate() {
        let mask = if any_partial_occ {
            u16::from(occ & (1 << d) != 0) * u16::MAX
        } else {
            occ_full
        };
        rows.fill(mask);
    }

    if !any_cube && !any_fluid {
        return InteriorScan {
            any_cube,
            any_fluid,
            any_shape,
            any_same_state_cull,
            any_partial_occ,
            uniform: Some(block),
        };
    }

    for y in 0..EDGE {
        let pad_y = (y + 1) * PAD_SLICE;
        for z in 0..EDGE {
            let pad_base = pad_y + (z + 1) * PAD_EDGE + 1;
            scratch.occ[pad_base..pad_base + EDGE].fill(occ);
        }
    }

    // Uniform light bytes unpack to a repeated `u16` pattern per row.
    if block_light.iter().all(|&b| b == block_light[0])
        && sky_light.iter().all(|&b| b == sky_light[0])
    {
        let bl = block_light[0];
        let sl = sky_light[0];
        let pattern =
            u16::from((bl & 0x0f) | ((sl & 0x0f) << 4)) | (u16::from((bl >> 4) | (sl & 0xf0)) << 8);
        for y in 0..EDGE {
            let pad_y = (y + 1) * PAD_SLICE;
            for z in 0..EDGE {
                let pad_base = pad_y + (z + 1) * PAD_EDGE + 1;
                let (pairs, _) = scratch.light[pad_base..pad_base + EDGE].as_chunks_mut::<2>();
                for pair in pairs {
                    *pair = pattern.to_le_bytes();
                }
            }
        }
    } else {
        fill_light_rows(block_light, sky_light, scratch);
    }

    if any_fluid {
        let encoded = encode_fluid(block);
        for y in 0..EDGE {
            let pad_y = (y + 1) * PAD_SLICE;
            for z in 0..EDGE {
                let pad_base = pad_y + (z + 1) * PAD_EDGE + 1;
                scratch.fluid[pad_base..pad_base + EDGE].fill(encoded);
            }
        }
    }

    InteriorScan {
        any_cube,
        any_fluid,
        any_shape,
        any_same_state_cull,
        any_partial_occ,
        uniform: Some(block),
    }
}

pub(crate) fn load_interior(
    blocks: &[Block; SECTION_CELLS],
    block_light: &[u8; LIGHT_BYTES],
    sky_light: &[u8; LIGHT_BYTES],
    scratch: &mut MesherScratch,
) -> InteriorScan {
    if let Some(block) = uniform_block(blocks) {
        return load_interior_uniform(block, block_light, sky_light, scratch);
    }
    let mut any_cube_bits = 0u16;
    let mut any_fluid_bits = 0u16;
    let mut any_shape = false;
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
                let is_shape = b.shape() != BlockShape::Cube;
                any_shape |= is_shape;
                if !is_shape
                    && b.flags == OPAQUE
                    && b.occlusion == ALL_FACES
                    && b.fluid_kind == NO_FLUID
                {
                    let bit = 1u16 << x;
                    cube_m |= bit;
                    occ_all_m |= bit;
                    occ_dst[x] = ALL_FACES | CUBE_BIT;
                } else if b.flags == 0 && b.fluid_kind == NO_FLUID {
                    occ_dst[x] = 0;
                } else {
                    let bit = 1u16 << x;
                    let occ = if !is_shape && b.flags & OPAQUE != 0 {
                        b.occlusion & ALL_FACES
                    } else {
                        0
                    };
                    let is_cube = !is_shape && (b.flags & CUBE_FLAGS) != 0;
                    if is_cube {
                        cube_m |= bit;
                    }
                    if occ == ALL_FACES {
                        occ_all_m |= bit;
                    } else if occ != 0 {
                        has_partial = true;
                    }
                    if !is_shape && b.flags & SAME_STATE_CULL != 0 {
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
            any_shape,
            any_same_state_cull,
            any_partial_occ,
            uniform: None,
        };
    }

    fill_light_rows(block_light, sky_light, scratch);

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
        any_shape,
        any_same_state_cull,
        any_partial_occ,
        uniform: None,
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

/// Packed corner shading for one 8-bit neighbour occupancy mask. The low byte
/// holds the four 2-bit AO values for corners 0-3; bits 8-11 flag the corners
/// whose two side neighbours both occlude — such a corner reads the face
/// light instead of the diagonal sample. The whole table is 512 bytes, so a
/// handful of L1 lines replaces the per-corner AO branches.
const AO_LUT: [u16; 256] = build_ao_lut();

const fn build_ao_lut() -> [u16; 256] {
    // Mask bit order: 0=mu, 1=pu, 2=mv, 3=pv, 4=mm, 5=pm, 6=pp, 7=mp.
    // Corner i combines sides (a, b) and the diagonal d between them.
    const CORNER: [(usize, usize, usize); 4] = [(0, 2, 4), (1, 2, 5), (1, 3, 6), (0, 3, 7)];
    let mut table = [0u16; 256];
    let mut mask = 0usize;
    while mask < 256 {
        let mut entry = 0u16;
        let mut c = 0usize;
        while c < 4 {
            let (a, b, d) = CORNER[c];
            let both = ((mask >> a) & (mask >> b) & 1) as u16;
            let ao = if both != 0 {
                0
            } else {
                (3 - ((mask >> a) & 1) - ((mask >> b) & 1) - ((mask >> d) & 1)) as u16
            };
            entry |= ao << (2 * c) | both << (8 + c);
            c += 1;
        }
        table[mask] = entry;
        mask += 1;
    }
    table
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

        // 8-bit occupancy mask in LUT bit order (mu, pu, mv, pv, mm, pm, pp, mp).
        let occ_mask = u8::from((o0 & opp_bit) != 0)
            | u8::from((o1 & opp_bit) != 0) << 1
            | u8::from((o2 & opp_bit) != 0) << 2
            | u8::from((o3 & opp_bit) != 0) << 3
            | u8::from((o4 & opp_bit) != 0) << 4
            | u8::from((o5 & opp_bit) != 0) << 5
            | u8::from((o6 & opp_bit) != 0) << 6
            | u8::from((o7 & opp_bit) != 0) << 7;
        let entry = AO_LUT[occ_mask as usize];

        // Per-corner light samples as byte lanes, one lane per corner:
        // side a, side b and the diagonal between them.
        let va = u32::from(l_mu)
            | u32::from(l_pu) << 8
            | u32::from(l_pu) << 16
            | u32::from(l_mu) << 24;
        let vb = u32::from(l_mv)
            | u32::from(l_mv) << 8
            | u32::from(l_pv) << 16
            | u32::from(l_pv) << 24;
        let vd0 = u32::from(l_mm)
            | u32::from(l_pm) << 8
            | u32::from(l_pp) << 16
            | u32::from(l_mp) << 24;

        // Corners with both sides occluded take the face light instead of the
        // diagonal sample: spread the four LUT flag bits to 0xFF byte lanes.
        let face_splat = u32::from(face) * 0x0101_0101;
        let both = u32::from(entry >> 8);
        let bm = ((both * 0x0020_4081) & 0x0101_0101) * 0xFF;
        let vd = (vd0 & !bm) | (face_splat & bm);

        // SWAR nibble averaging over the four corner lanes at once. Every
        // lane sum is at most 15*3 + 17 = 62, so bytes cannot carry into each
        // other and one `>> 2` applies the +2 rounding to all four corners.
        const NIB: u32 = 0x0f0f_0f0f;
        let lo = (va & NIB)
            + (vb & NIB)
            + (vd & NIB)
            + (u32::from(face_bl) + 2) * 0x0101_0101;
        let hi = ((va >> 4) & NIB)
            + ((vb >> 4) & NIB)
            + ((vd >> 4) & NIB)
            + (u32::from(face_sk) + 2) * 0x0101_0101;
        let bl_avg = (lo >> 2) & NIB;
        let sk_avg = (hi >> 2) & NIB;

        // c_bl = max(bl_avg, src_bl) per lane. Each byte of
        // `16 + src_bl - bl_avg` stays in 1..=31 (no borrows) and its bit 4 is
        // set exactly where src_bl wins; expand that bit to a 0x0F mask.
        let src_splat = u32::from(src_bl) * 0x0101_0101;
        let ge = (((src_splat + 0x1010_1010).wrapping_sub(bl_avg)) >> 4 & 0x0101_0101) * 0x0F;
        let bl = bl_avg ^ ((bl_avg ^ src_splat) & ge);

        // Scatter the four corner light bytes to their 12-bit fields.
        let lv = bl | (sk_avg << 4);
        let lv64 = u64::from(lv);
        let light_fields = (lv64 & 0xff)
            | (lv64 & 0xff00) << 4
            | (lv64 & 0xff_0000) << 8
            | (lv64 & 0xff00_0000) << 12;

        // Scatter the packed 2-bit AO values to bits 8-9 of each field.
        let ao = u64::from(entry & 0xff);
        let ao_fields = ((ao & 0x03) << 8)
            | ((ao & 0x0c) << 18)
            | ((ao & 0x30) << 28)
            | ((ao & 0xc0) << 38);

        light_fields | ao_fields | (u64::from(face) << 48)
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

/// Emit the six boundary faces of a uniform, full-cube section without walking
/// all 16 slices. A face is accepted only when the existing full-slice rules
/// prove it is one exact quad, or every adjacent cell fully occludes it. Any
/// mixed occupancy or lighting falls back to the ordinary greedy mesher.
fn try_uniform_boundary_box<B: BorderStates, S: QuadSink>(
    block: Block,
    blocks: &[Block; SECTION_CELLS],
    borders: &B,
    scratch: &mut MesherScratch,
    any_same_state_cull: bool,
    y_offset: f32,
    quads: &mut S,
) -> bool {
    if block.flags & OPAQUE == 0
        || block.occlusion & ALL_FACES != ALL_FACES
        || block.fluid() != NO_FLUID
        || block.flags & SAME_STATE_CULL != 0
    {
        return false;
    }

    let directions = [
        Direction::Down,
        Direction::Up,
        Direction::North,
        Direction::South,
        Direction::West,
        Direction::East,
    ];
    let mut faces = [None; 6];
    let mut fallback = [false; 6];
    for (index, direction) in directions.into_iter().enumerate() {
        let slice = if direction.step() < 0 { 0 } else { EDGE - 1 };
        if let Some(face) = try_uniform_full_slice(
            blocks,
            &scratch.occ,
            &scratch.light,
            direction,
            slice,
            any_same_state_cull,
            y_offset,
        ) {
            faces[index] = Some(face);
            continue;
        }

        let (pad_s, pad_u, pad_v, pad_step) = DIR_PAD_STRIDES[direction as usize];
        let pad_origin = (PAD_SLICE + PAD_EDGE + 1) as isize + slice as isize * pad_s;
        let base = pad_origin + pad_step;
        let opp_bit = direction.opposite().bit();
        let mut fully_occluded = true;
        'cells: for v in 0..EDGE {
            for u in 0..EDGE {
                let idx = (base + v as isize * pad_v + u as isize * pad_u) as usize;
                if scratch.occ[idx] & opp_bit == 0 {
                    fully_occluded = false;
                    break 'cells;
                }
            }
        }
        if !fully_occluded {
            fallback[index] = true;
        }
    }

    let ctx = SliceCtx {
        blocks,
        borders,
        any_same_state_cull,
        y_offset,
    };
    for (index, direction) in directions.into_iter().enumerate() {
        if fallback[index] {
            let slice = if direction.step() < 0 { 0 } else { EDGE - 1 };
            let (pad_s, pad_u, pad_v, pad_step) = DIR_PAD_STRIDES[direction as usize];
            let pad_origin = (PAD_SLICE + PAD_EDGE + 1) as isize + slice as isize * pad_s;
            let base = pad_origin + pad_step;
            let opp_bit = direction.opposite().bit();
            let mut vis_rows = [0u16; EDGE];
            for (v, vis_row) in vis_rows.iter_mut().enumerate() {
                for u in 0..EDGE {
                    let idx = (base + v as isize * pad_v + u as isize * pad_u) as usize;
                    if scratch.occ[idx] & opp_bit == 0 {
                        *vis_row |= 1 << u;
                    }
                }
            }
            greedy_slice(&ctx, scratch, direction, slice, vis_rows, quads);
        } else if let Some(face) = faces[index] {
            quads.push(face);
        }
    }
    true
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
        // Every emitted quad is 1x1: only the u-axis corner component varies
        // within a row, so build a per-row template and stamp `u` per face.
        let (u_ax, v_ax) = direction.tangents();
        let axis = direction.axis();
        let pos_step = i32::from(direction.step() > 0);
        // When u maps to the Y axis (North/East) the template must not fold in
        // y_offset: the original order is `(u + cu) + y_offset`, so y_offset is
        // added only after stamping `u` to keep float rounding identical.
        let tpl_y_off = if u_ax == 1 { 0.0 } else { y_offset };
        for (v, &vis_row) in vis_rows.iter().enumerate() {
            let mut row = vis_row;
            if row == 0 {
                continue;
            }
            let mut tpl = [[0.0f32; 3]; 4];
            for (c, &(cu, cv)) in CORNERS.iter().enumerate() {
                let mut q = [0i32; 3];
                q[axis] = slice as i32 + pos_step;
                q[u_ax] = cu;
                q[v_ax] = v as i32 + cv;
                tpl[c] = [q[0] as f32, q[1] as f32 + tpl_y_off, q[2] as f32];
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
                let mut positions = tpl;
                let uf = u as f32;
                for c in positions.iter_mut() {
                    c[u_ax] += uf;
                }
                if u_ax == 1 {
                    for c in positions.iter_mut() {
                        c[1] += y_offset;
                    }
                }
                let tint = if tinted { block.tint } else { 0 };
                let flags = if tinted { QUAD_TINTED } else { 0 };
                let f0 = shade_bits & 0xfff;
                let flat = ((shade_bits >> 12) & 0xfff) == f0
                    && ((shade_bits >> 24) & 0xfff) == f0
                    && ((shade_bits >> 36) & 0xfff) == f0;
                if flat {
                    let l = shade_bits as u8;
                    let ao = ((shade_bits >> 8) & 3) as u8;
                    quads.push(Quad {
                        positions,
                        direction,
                        state_id: block.state_id,
                        tint,
                        flow: [0.0; 2],
                        ao: [ao; 4],
                        block_light: [l & 15; 4],
                        sky_light: [l >> 4; 4],
                        face_light: Light(((shade_bits >> 48) & 0xff) as u8),
                        layer,
                        diagonal: 0,
                        fluid_kind: NO_FLUID,
                        flags,
                        reserved: 0,
                    });
                } else {
                    let face = PackedFace::new(block.state_id, tint, layer, flags, shade_bits);
                    quads.push(face.quad(direction, positions));
                }
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
    scan: InteriorScan,
    y_offset: f32,
    quads: &mut S,
) {
    if let Some(block) = scan.uniform {
        if try_uniform_boundary_box(
            block,
            blocks,
            borders,
            scratch,
            scan.any_same_state_cull,
            y_offset,
            quads,
        ) {
            return;
        }
    }
    let any_same_state_cull = scan.any_same_state_cull;
    let any_partial_occ = scan.any_partial_occ;
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
        // Downward border occlusion, packed four strips per word: lane z holds
        // the below-section row's "occludes Up" mask along x.
        let mut border4 = [0u64; 4];
        let opp_bit = Direction::Up.bit();
        for (g, word) in border4.iter_mut().enumerate() {
            *word = border_lane4(&scratch.occ, (4 * g + 1) * PAD_EDGE + 1, PAD_EDGE, opp_bit);
        }
        let occ_up = scratch.occ_rows[occ_up_idx];
        for y in 0..EDGE {
            let mut vis_rows = [0u16; EDGE];
            let mut any = 0u64;
            let base = y * EDGE;
            for (g, &border) in border4.iter().enumerate() {
                let cube4 = pack_strips4(&scratch.cube_rows, base + 4 * g);
                let occ4 = if y == 0 {
                    border
                } else {
                    pack_strips4(&occ_up, base - EDGE + 4 * g)
                };
                let v4 = cube4 & !occ4;
                any |= v4;
                store_strips4(&mut vis_rows, 4 * g, v4);
            }
            if any != 0 {
                greedy_slice(&ctx, scratch, Direction::Down, y, vis_rows, quads);
            }
        }
    }

    // 1: Direction::Up (+Y)
    {
        let mut border4 = [0u64; 4];
        let opp_bit = Direction::Down.bit();
        let top_plane = (PAD_EDGE - 1) * PAD_SLICE;
        for (g, word) in border4.iter_mut().enumerate() {
            *word = border_lane4(
                &scratch.occ,
                top_plane + (4 * g + 1) * PAD_EDGE + 1,
                PAD_EDGE,
                opp_bit,
            );
        }
        let occ_down = scratch.occ_rows[occ_down_idx];
        for y in 0..EDGE {
            let mut raw_vis = [0u16; EDGE];
            let mut any = 0u64;
            let base = y * EDGE;
            for (g, &border) in border4.iter().enumerate() {
                let cube4 = pack_strips4(&scratch.cube_rows, base + 4 * g);
                let occ4 = if y == EDGE - 1 {
                    border
                } else {
                    pack_strips4(&occ_down, base + EDGE + 4 * g)
                };
                let v4 = cube4 & !occ4;
                any |= v4;
                store_strips4(&mut raw_vis, 4 * g, v4);
            }
            if any != 0 {
                let vis_rows = transpose_16x16(raw_vis);
                greedy_slice(&ctx, scratch, Direction::Up, y, vis_rows, quads);
            }
        }
    }

    // 2: Direction::North (-Z)
    {
        // North faces cull against the cell one strip south (z - 1); the
        // border word substitutes the neighbour section's plane at z = -1.
        let mut border4 = [0u64; 4];
        let opp_bit = Direction::South.bit();
        for (g, word) in border4.iter_mut().enumerate() {
            *word = border_lane4(
                &scratch.occ,
                (4 * g + 1) * PAD_SLICE + 1,
                PAD_SLICE,
                opp_bit,
            );
        }
        let occ_south = scratch.occ_rows[occ_south_idx];
        for z in 0..EDGE {
            let mut raw_vis = [0u16; EDGE];
            let mut any = 0u64;
            for (g, &border) in border4.iter().enumerate() {
                let row_base = 4 * g * EDGE + z;
                let cube4 = pack_strips4_step(&scratch.cube_rows, row_base);
                let occ4 = if z == 0 {
                    border
                } else {
                    pack_strips4_step(&occ_south, row_base - 1)
                };
                let v4 = cube4 & !occ4;
                any |= v4;
                store_strips4(&mut raw_vis, 4 * g, v4);
            }
            if any != 0 {
                let vis_rows = transpose_16x16(raw_vis);
                greedy_slice(&ctx, scratch, Direction::North, z, vis_rows, quads);
            }
        }
    }

    // 3: Direction::South (+Z)
    {
        let mut border4 = [0u64; 4];
        let opp_bit = Direction::North.bit();
        let south_offset = (PAD_EDGE - 1) * PAD_EDGE + 1;
        for (g, word) in border4.iter_mut().enumerate() {
            *word = border_lane4(
                &scratch.occ,
                (4 * g + 1) * PAD_SLICE + south_offset,
                PAD_SLICE,
                opp_bit,
            );
        }
        let occ_north = scratch.occ_rows[occ_north_idx];
        for z in 0..EDGE {
            let mut vis_rows = [0u16; EDGE];
            let mut any = 0u64;
            for (g, &border) in border4.iter().enumerate() {
                let row_base = 4 * g * EDGE + z;
                let cube4 = pack_strips4_step(&scratch.cube_rows, row_base);
                let occ4 = if z == EDGE - 1 {
                    border
                } else {
                    pack_strips4_step(&occ_north, row_base + 1)
                };
                let v4 = cube4 & !occ4;
                any |= v4;
                store_strips4(&mut vis_rows, 4 * g, v4);
            }
            if any != 0 {
                greedy_slice(&ctx, scratch, Direction::South, z, vis_rows, quads);
            }
        }
    }

    // 4: Direction::West (-X)
    {
        // West faces cull in-strip: the occluder of cell x is cell x - 1, so
        // `cube4 & !((occ_east4 << 1) & LANES_HIGH | border4)` where the lane
        // masks stop bits crossing strip boundaries and `border4` bit 0 of
        // lane z carries the west border cell's "occludes East" flag.
        let opp_bit = Direction::East.bit();
        let occ_east = scratch.occ_rows[occ_east_idx];
        let mut west_by_x = [[0u16; EDGE]; EDGE];
        let mut active_x = 0u16;
        for (y, _) in scratch.cube_rows.as_chunks::<EDGE>().0.iter().enumerate() {
            let pad_y = (y + 1) * PAD_SLICE;
            let base = y * EDGE;
            let mut raw = [0u16; EDGE];
            let mut row_any = 0u64;
            for g in 0..4 {
                let mut border4 = 0u64;
                for (k, z) in (4 * g..4 * g + 4).enumerate() {
                    let bit =
                        u64::from((scratch.occ[pad_y + (z + 1) * PAD_EDGE] & opp_bit) != 0);
                    border4 |= bit << (16 * k);
                }
                let cube4 = pack_strips4(&scratch.cube_rows, base + 4 * g);
                let occ4 = pack_strips4(&occ_east, base + 4 * g);
                let v4 = cube4 & !(((occ4 << 1) & LANES_HIGH) | border4);
                row_any |= v4;
                store_strips4(&mut raw, 4 * g, v4);
            }
            if row_any != 0 {
                let row_mask = fold_strips4(row_any);
                active_x |= row_mask;
                let t = transpose_16x16(raw);
                let mut m = row_mask;
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
        // East faces cull against the cell at x + 1:
        // `cube4 & !((occ_west4 >> 1) & LANES_LOW | border4)` with the east
        // border "occludes West" flag at bit 15 of each lane.
        let opp_bit = Direction::West.bit();
        let occ_west = scratch.occ_rows[occ_west_idx];
        let mut east_by_x = [[0u16; EDGE]; EDGE];
        let mut active_x = 0u16;
        for (y, _) in scratch.cube_rows.as_chunks::<EDGE>().0.iter().enumerate() {
            let pad_y = (y + 1) * PAD_SLICE + (PAD_EDGE - 1);
            let base = y * EDGE;
            let mut raw = [0u16; EDGE];
            let mut row_any = 0u64;
            for g in 0..4 {
                let mut border4 = 0u64;
                for (k, z) in (4 * g..4 * g + 4).enumerate() {
                    let bit =
                        u64::from((scratch.occ[pad_y + (z + 1) * PAD_EDGE] & opp_bit) != 0);
                    border4 |= bit << (16 * k + 15);
                }
                let cube4 = pack_strips4(&scratch.cube_rows, base + 4 * g);
                let occ4 = pack_strips4(&occ_west, base + 4 * g);
                let v4 = cube4 & !(((occ4 >> 1) & LANES_LOW) | border4);
                row_any |= v4;
                store_strips4(&mut raw, 4 * g, v4);
            }
            if row_any != 0 {
                let row_mask = fold_strips4(row_any);
                active_x |= row_mask;
                let t = transpose_16x16(raw);
                let mut m = row_mask;
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

/// 2D greedy merge of one slice's flat fluid faces: the classic
/// take-lowest, extend-width, extend-height loop over `slots`, which is
/// indexed in the face's `(v, u)` tangent order (Down is `[z][x]`, Up is
/// `[x][z]`). Rectangles grow only across cells carrying the identical
/// [`FlatFluidFace`], so every emitted quad equals the exact union of the
/// per-cell faces it replaces — same positions, corner heights, flow,
/// material and four-corner shading.
fn merge_flat_fluid_faces<S: QuadSink>(
    slots: &mut [Option<FlatFluidFace>; FACE_CELLS],
    direction: Direction,
    slice: usize,
    y_offset: f32,
    quads: &mut S,
) {
    let yf = slice as f32;
    for v in 0..EDGE {
        for u in 0..EDGE {
            let index = v * EDGE + u;
            let Some(face) = slots[index] else {
                continue;
            };
            let width = if face.can_merge_u() {
                let mut w = 1;
                while u + w < EDGE && slots[v * EDGE + u + w] == Some(face) {
                    w += 1;
                }
                w
            } else {
                1
            };
            let mut height = 1;
            if face.can_merge_v() {
                'grow: while v + height < EDGE {
                    let row = (v + height) * EDGE + u;
                    for k in 0..width {
                        if slots[row + k] != Some(face) {
                            break 'grow;
                        }
                    }
                    height += 1;
                }
            }
            for dv in 0..height {
                slots[(v + dv) * EDGE + u..(v + dv) * EDGE + u + width].fill(None);
            }
            let origin = position(slice, u, v, direction);
            let mut positions = rectangle(origin, direction, width, height);
            for vertex in &mut positions {
                if vertex[1] > yf {
                    vertex[1] = yf + face.height + y_offset;
                } else {
                    vertex[1] += y_offset;
                }
            }
            quads.push(face.quad(direction, positions));
        }
    }
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
        // Flat +/-Y faces gather here for the slice-level greedy merge
        // below; indexed in each direction's (v, u) tangent order.
        let mut down_faces: [Option<FlatFluidFace>; FACE_CELLS] = [None; FACE_CELLS];
        let mut up_faces: [Option<FlatFluidFace>; FACE_CELLS] = [None; FACE_CELLS];
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

                // Mergeable only when all four corner heights are equal: a
                // sloped top/bottom face can never share a coplanar quad.
                let flat_top =
                    h[0][0] == h[0][1] && h[0][0] == h[1][0] && h[0][0] == h[1][1];

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
                    // The merge is compiled out of `cfg(test)` builds: the
                    // scalar oracle in tests.rs emits per-cell faces and the
                    // parity asserts compare bit-identical quad streams, so
                    // unit-test builds keep the unmerged per-cell path while
                    // shipped builds (and integration tests, which link the
                    // normally-compiled crate) exercise the greedy merge.
                    if cfg!(not(test)) && flat_top && d <= Direction::Up as usize {
                        // Flat top/bottom face: defer to the slice greedy
                        // merge, keyed on the full face payload so only
                        // identical surfaces share a quad.
                        let face = FlatFluidFace {
                            state_id: block.state_id,
                            tint,
                            shade: s,
                            height: h[0][0],
                            flow,
                            kind,
                            flags,
                        };
                        if direction == Direction::Down {
                            down_faces[z * EDGE + x] = Some(face);
                        } else {
                            up_faces[x * EDGE + z] = Some(face);
                        }
                        continue;
                    }
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
        merge_flat_fluid_faces(&mut down_faces, Direction::Down, y, y_offset, quads);
        merge_flat_fluid_faces(&mut up_faces, Direction::Up, y, y_offset, quads);
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
            scan,
            y_offset,
            quads,
        );
    }
    if scan.any_fluid {
        mesh_fluids_fast(blocks, scratch, y_offset, quads);
    }
}

fn shape_quad(
    block: Block,
    direction: Direction,
    points: [[f32; 3]; 4],
    light: Light,
    quads: &mut impl QuadSink,
) {
    let layer = if block.flags & OPAQUE != 0 {
        Layer::Opaque
    } else if block.flags & CUTOUT != 0 {
        Layer::Cutout
    } else {
        Layer::Translucent
    };
    let tinted = block.flags & TINTED != 0;
    quads.push(Quad {
        positions: points,
        state_id: block.state_id,
        tint: if tinted { block.tint } else { 0 },
        flow: [0.0; 2],
        ao: [3; 4],
        block_light: [light.block(); 4],
        sky_light: [light.sky(); 4],
        face_light: light,
        direction,
        layer,
        diagonal: 0,
        fluid_kind: NO_FLUID,
        flags: if tinted { QUAD_TINTED } else { 0 },
        reserved: 0,
    });
}

fn shape_box(
    block: Block,
    cell: [f32; 3],
    min: [f32; 3],
    max: [f32; 3],
    light: Light,
    quads: &mut impl QuadSink,
) {
    for direction in Direction::ALL {
        let axis = direction.axis();
        let (u_axis, v_axis) = direction.tangents();
        let plane = if direction.step() < 0 {
            min[axis]
        } else {
            max[axis]
        };
        let points = CORNERS.map(|(cu, cv)| {
            let mut p = cell;
            p[axis] += plane;
            p[u_axis] += if cu == 0 { min[u_axis] } else { max[u_axis] };
            p[v_axis] += if cv == 0 { min[v_axis] } else { max[v_axis] };
            p
        });
        shape_quad(block, direction, points, light, quads);
    }
}

/// Emit the small set of non-cube state shapes separately. Ordinary cubes
/// still use the unchanged greedy face path above.
fn mesh_non_cube_shapes<S: QuadSink>(
    blocks: &[Block; SECTION_CELLS],
    block_light: &[u8; LIGHT_BYTES],
    sky_light: &[u8; LIGHT_BYTES],
    y_offset: f32,
    quads: &mut S,
) {
    for (index, &block) in blocks.iter().enumerate() {
        let shape = block.shape();
        if shape == BlockShape::Cube {
            continue;
        }
        let y = index / FACE_CELLS;
        let z = (index % FACE_CELLS) / EDGE;
        let x = index % EDGE;
        let cell = [x as f32, y as f32 + y_offset, z as f32];
        let packed = index / 2;
        let light = Light::new(
            if index & 1 == 0 {
                block_light[packed] & 15
            } else {
                block_light[packed] >> 4
            },
            if index & 1 == 0 {
                sky_light[packed] & 15
            } else {
                sky_light[packed] >> 4
            },
        );
        match shape {
            BlockShape::Cube => unreachable!(),
            BlockShape::CrossPlant => {
                let a = [
                    [cell[0], cell[1], cell[2]],
                    [cell[0] + 1.0, cell[1], cell[2] + 1.0],
                    [cell[0] + 1.0, cell[1] + 1.0, cell[2] + 1.0],
                    [cell[0], cell[1] + 1.0, cell[2]],
                ];
                let b = [
                    [cell[0] + 1.0, cell[1], cell[2]],
                    [cell[0], cell[1], cell[2] + 1.0],
                    [cell[0], cell[1] + 1.0, cell[2] + 1.0],
                    [cell[0] + 1.0, cell[1] + 1.0, cell[2]],
                ];
                shape_quad(block, Direction::South, a, light, quads);
                shape_quad(
                    block,
                    Direction::North,
                    [a[3], a[2], a[1], a[0]],
                    light,
                    quads,
                );
                shape_quad(block, Direction::South, b, light, quads);
                shape_quad(
                    block,
                    Direction::North,
                    [b[3], b[2], b[1], b[0]],
                    light,
                    quads,
                );
            }
            BlockShape::SlabBottom => {
                shape_box(block, cell, [0.0, 0.0, 0.0], [1.0, 0.5, 1.0], light, quads)
            }
            BlockShape::SlabTop => {
                shape_box(block, cell, [0.0, 0.5, 0.0], [1.0, 1.0, 1.0], light, quads)
            }
            BlockShape::Stair => {
                shape_box(block, cell, [0.0, 0.0, 0.0], [1.0, 0.5, 1.0], light, quads);
                shape_box(block, cell, [0.5, 0.5, 0.0], [1.0, 1.0, 1.0], light, quads);
            }
            BlockShape::Torch => shape_box(
                block,
                cell,
                [0.4375, 0.0, 0.4375],
                [0.5625, 0.625, 0.5625],
                light,
                quads,
            ),
            BlockShape::FencePost => shape_box(
                block,
                cell,
                [0.375, 0.0, 0.375],
                [0.625, 1.0, 0.625],
                light,
                quads,
            ),
            BlockShape::DoorPanel => shape_box(
                block,
                cell,
                [0.0, 0.0, 0.0],
                [1.0, 1.0, 0.1875],
                light,
                quads,
            ),
            BlockShape::ThinLayer => {
                let layer = f32::from(block.shape_parameter().clamp(1, 2)) / 16.0;
                shape_box(
                    block,
                    cell,
                    [0.0, 0.0, 0.0],
                    [1.0, layer, 1.0],
                    light,
                    quads,
                );
            }
            BlockShape::BedHalf => shape_box(
                block,
                cell,
                [0.0, 0.0, 0.0],
                [1.0, 0.5625, 1.0],
                light,
                quads,
            ),
        }
    }
}

pub(crate) fn mesh_section_with_scratch<S: QuadSink>(
    view: &SectionView<'_>,
    scratch: &mut MesherScratch,
    y_offset: f32,
    quads: &mut S,
) {
    // Sealed and empty sections skip the interior load, shell fill and greedy
    // pass entirely. `emits_nothing` exits on the first live boundary face, so
    // open sections pay only a couple of cell checks.
    let missing_sealed = matches!(view.missing, MissingNeighbor::Opaque);
    if crate::cull::emits_nothing(
        view.blocks,
        false,
        |direction| match view.borders[direction as usize] {
            Some(border) => border
                .iter()
                .all(|sample| sample.block.occludes(direction.opposite())),
            None => missing_sealed,
        },
        |direction, p| {
            view.borders[direction as usize].map_or_else(
                || view.missing.sample().block,
                |border| border[direction.border_index(p)].block,
            )
        },
    ) {
        return;
    }
    let scan = load_interior(view.blocks, view.block_light, view.sky_light, scratch);
    if scan.any_shape {
        mesh_non_cube_shapes(
            view.blocks,
            view.block_light,
            view.sky_light,
            y_offset,
            quads,
        );
    }
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

/// Seven draw buckets for face-direction culling: indices 0..5 follow
/// `Direction`, and [`DIRECTION_BUCKET_UNASSIGNED`] holds quads a directional
/// culler must always draw. Mesher output always carries one of the six face
/// directions, so its unassigned bucket stays empty; the bucket exists for
/// renderer-appended geometry and ABI parity.
pub const DIRECTION_BUCKETS: usize = 7;
pub const DIRECTION_BUCKET_UNASSIGNED: usize = Direction::East as usize + 1;

/// Bit per bucket to draw when `facing` holds the section-boundary sides
/// facing the camera (see `crate::box_facing_mask`: one `Direction::bit` per
/// visible side). The unassigned bucket always draws. An enclosed camera
/// produces a zero facing mask and must draw every bucket, so zero maps to a
/// full mask. A camera outside the bounds on an axis reports one bit per axis,
/// so the typical case draws three face buckets plus unassigned and skips the
/// three back-facing ones in O(1).
pub const fn bucket_draw_mask(facing: u8) -> u8 {
    let facing = facing & ALL_FACES;
    if facing == 0 {
        (1 << DIRECTION_BUCKETS) - 1
    } else {
        facing | (1 << DIRECTION_BUCKET_UNASSIGNED)
    }
}

/// Contiguous quad ranges of a direction-grouped mesh. Offsets index the
/// grouped order; `offset[b] + count[b]` ends bucket `b`, and the buckets
/// cover the whole range with no gaps.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DirectionBuckets {
    /// First quad of each bucket in the grouped output.
    pub offset: [u32; DIRECTION_BUCKETS],
    /// Number of quads in each bucket.
    pub count: [u32; DIRECTION_BUCKETS],
}

impl DirectionBuckets {
    /// Bucket index of a face direction: the `Direction` value itself.
    pub const fn index_of(direction: Direction) -> usize {
        direction as usize
    }

    /// Ranges for `count` laid out contiguously starting at `base`.
    pub(crate) fn laid_out(count: [u32; DIRECTION_BUCKETS], base: u32) -> Self {
        let mut buckets = Self {
            offset: [0; DIRECTION_BUCKETS],
            count,
        };
        let mut next = base;
        for (offset, &count) in buckets.offset.iter_mut().zip(&count) {
            *offset = next;
            next += count;
        }
        buckets
    }

    /// Stable regrouping: a copy of `quads` ordered by bucket with input order
    /// preserved inside each bucket, plus its ranges.
    pub fn group(quads: &[Quad]) -> (Vec<Quad>, Self) {
        let mut grouped = Vec::with_capacity(quads.len());
        let buckets = Self::group_into(quads, &mut grouped);
        (grouped, buckets)
    }

    /// Appends the same stable regrouping to `out`. The returned ranges index
    /// `out` directly, so quads already held there shift every offset by the
    /// length `out` had before the call. Panics when the result would exceed
    /// `u32::MAX` quads.
    pub fn group_into(quads: &[Quad], out: &mut Vec<Quad>) -> Self {
        assert!(out.len() + quads.len() <= u32::MAX as usize);
        let mut count = [0u32; DIRECTION_BUCKETS];
        for quad in quads {
            count[Self::index_of(quad.direction)] += 1;
        }
        let buckets = Self::laid_out(count, out.len() as u32);
        for bucket in 0..DIRECTION_BUCKETS {
            out.extend(
                quads
                    .iter()
                    .filter(|quad| Self::index_of(quad.direction) == bucket)
                    .copied(),
            );
        }
        buckets
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn shade_corner_unpacked(
        base: CornerBase,
        a: SideSample,
        b: SideSample,
        od: u8,
        ld: u8,
    ) -> u64 {
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

    /// The pre-LUT scalar implementation of `shade_fast`, kept verbatim as the
    /// oracle for the exhaustive parity test below.
    unsafe fn shade_ref(
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

    /// Neighbour pad indices in the LUT bit order mu, pu, mv, pv, mm, pm, pp, mp.
    fn neighbor_indices(out: usize, su: isize, sv: isize) -> [usize; 8] {
        let i_mu = out.wrapping_add_signed(-su);
        let i_pu = out.wrapping_add_signed(su);
        let i_mv = out.wrapping_add_signed(-sv);
        let i_pv = out.wrapping_add_signed(sv);
        [
            i_mu,
            i_pu,
            i_mv,
            i_pv,
            i_mu.wrapping_add_signed(-sv),
            i_pu.wrapping_add_signed(-sv),
            i_pu.wrapping_add_signed(sv),
            i_mu.wrapping_add_signed(sv),
        ]
    }

    fn shade_pair(
        mask: u8,
        opp_bit: u8,
        occ_junk: u8,
        src_light: u8,
        center_light: u8,
        neigh_light: [u8; 8],
    ) -> (u64, u64) {
        assert_eq!(occ_junk & opp_bit, 0, "junk must not contain opp_bit");
        let mut occ = Box::new([0u8; PAD_CELLS]);
        let mut light = Box::new([0u8; PAD_CELLS]);
        let pad_idx = pad_index(8, 8, 8);
        let step_dir = PAD_SLICE as isize;
        let su = PAD_EDGE as isize;
        let sv = 1isize;
        let out = pad_idx.wrapping_add_signed(step_dir);
        for (i, &ni) in neighbor_indices(out, su, sv).iter().enumerate() {
            occ[ni] = (((mask >> i) & 1) * opp_bit) | occ_junk;
            light[ni] = neigh_light[i];
        }
        light[pad_idx] = src_light;
        light[out] = center_light;
        // SAFETY: pad_index(8,8,8) is interior, so all neighbour indices stay
        // inside the padded arrays.
        unsafe {
            (
                shade_fast(&occ, &light, pad_idx, step_dir, su, sv, opp_bit),
                shade_ref(&occ, &light, pad_idx, step_dir, su, sv, opp_bit),
            )
        }
    }

    #[test]
    fn ao_lut_spot_checks() {
        // No occluders: every corner keeps full AO, no "both" flags.
        assert_eq!(AO_LUT[0], 0x00FF);
        // All four sides occlude: every corner has both sides set -> ao 0.
        assert_eq!(AO_LUT[0b0000_1111], 0x0F00);
        // Only the -u side occludes: corners 0 and 3 drop to ao 2.
        assert_eq!(AO_LUT[0x01] & 0xff, 0xBE);
        assert_eq!(AO_LUT[0x01] >> 8, 0);
        // Diagonals alone never set the both flag.
        assert_eq!(AO_LUT[0b1111_0000] >> 8, 0);
        assert_eq!(AO_LUT[0b1111_0000] & 0xff, 0xAA);
    }

    #[test]
    fn shade_fast_matches_scalar_oracle_exhaustively() {
        let face_l = 0x2Au8; // face_bl = max(0x0A, src_bl), face_sk = 2
        let fixtures: Vec<(u8, u8, [u8; 8])> = vec![
            // Uniform light -> exercises the flat fast path when mask == 0.
            (0x05, face_l, [face_l; 8]),
            (0x0F, face_l, [face_l; 8]),
            // One differing neighbour pulls every case off the fast path.
            (0x05, face_l, {
                let mut n = [face_l; 8];
                n[0] = 0x2B;
                n
            }),
            // Extremes.
            (0x00, 0x00, [0x00; 8]),
            (0x0F, 0xFF, [0xFF; 8]),
            (0x0F, 0x0F, [0xF0; 8]),
            // Mixed nibbles.
            (0x07, 0x3E, [0x00, 0x11, 0xF0, 0x0F, 0x55, 0xAA, 0x99, 0x34]),
            (0x0C, 0x91, [0x77, 0x08, 0x23, 0xFE, 0x40, 0x15, 0x6A, 0xB2]),
        ];

        let mut cases = 0usize;
        for opp in 0..6u8 {
            let opp_bit = 1u8 << opp;
            let occ_junks = [0x00u8, 0x80, ALL_FACES & !opp_bit];
            for &occ_junk in &occ_junks {
                // Deterministic pseudo-random light fixture per (opp, junk).
                let mut rng = 0x9E37_79B9u32 ^ (u32::from(opp_bit) << 8) ^ u32::from(occ_junk);
                let mut rand_byte = || {
                    rng ^= rng << 13;
                    rng ^= rng >> 17;
                    rng ^= rng << 5;
                    rng as u8
                };
                let mut random_neigh = [0u8; 8];
                for v in &mut random_neigh {
                    *v = rand_byte();
                }
                let random_fixture = (rand_byte(), rand_byte(), random_neigh);

                for &(src_l, center_l, ref neigh) in
                    fixtures.iter().chain(std::iter::once(&random_fixture))
                {
                    for mask in 0..=255u8 {
                        let (got, want) =
                            shade_pair(mask, opp_bit, occ_junk, src_l, center_l, *neigh);
                        assert_eq!(
                            got, want,
                            "mask={mask:08b} opp={opp_bit:#04x} junk={occ_junk:#04x} \
                             src={src_l:#04x} center={center_l:#04x} neigh={neigh:?}"
                        );
                        cases += 1;
                    }
                }
            }
        }
        assert_eq!(cases, 6 * 3 * 9 * 256);
    }
}
