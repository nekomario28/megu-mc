// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later

pub const EDGE: usize = 16;
pub const FACE_CELLS: usize = EDGE * EDGE;
pub const SECTION_CELLS: usize = EDGE * FACE_CELLS;
pub const LIGHT_BYTES: usize = SECTION_CELLS / 2;
pub const EDGE_CORNER_CELLS: usize = 12 * EDGE + 8;

/// Direction values follow the server's voxel-face order.
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Direction {
    Down = 0,
    Up = 1,
    North = 2,
    South = 3,
    West = 4,
    East = 5,
}

impl Direction {
    pub const ALL: [Self; 6] = [
        Self::Down,
        Self::Up,
        Self::North,
        Self::South,
        Self::West,
        Self::East,
    ];

    pub const fn opposite(self) -> Self {
        Self::ALL[self as usize ^ 1]
    }

    pub const fn bit(self) -> u8 {
        1 << self as u8
    }

    pub(crate) const fn axis(self) -> usize {
        match self {
            Self::West | Self::East => 0,
            Self::Down | Self::Up => 1,
            Self::North | Self::South => 2,
        }
    }

    pub(crate) const fn step(self) -> i32 {
        if self as u8 & 1 == 0 {
            -1
        } else {
            1
        }
    }

    /// Tangent axes ordered so U × V points out of the face.
    pub(crate) const fn tangents(self) -> (usize, usize) {
        match self {
            Self::Down => (0, 2),
            Self::Up => (2, 0),
            Self::North => (1, 0),
            Self::South => (0, 1),
            Self::West => (2, 1),
            Self::East => (1, 2),
        }
    }

    /// Index a border slice using the two world axes left after removing the
    /// face normal. The lower-numbered axis changes fastest (X, Y, Z order).
    pub const fn border_index(self, p: [usize; 3]) -> usize {
        match self.axis() {
            0 => p[2] * EDGE + p[1],
            1 => p[2] * EDGE + p[0],
            _ => p[1] * EDGE + p[0],
        }
    }

    pub(crate) const fn offset(self, mut p: [i32; 3]) -> [i32; 3] {
        p[self.axis()] += self.step();
        p
    }
}

pub const OPAQUE: u8 = 1 << 0;
pub const CUTOUT: u8 = 1 << 1;
pub const TRANSLUCENT: u8 = 1 << 2;
pub const WATERLOGGED: u8 = 1 << 3;
pub const TINTED: u8 = 1 << 4;
pub const FLUID_TINTED: u8 = 1 << 5;
pub const SAME_STATE_CULL: u8 = 1 << 6;
pub const ALL_FACES: u8 = 0x3f;

pub const NO_FLUID: u8 = 0;
pub const WATER: u8 = 1;
pub const LAVA: u8 = 2;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Block {
    pub state_id: u32,
    pub tint: u32,
    pub fluid_tint: u32,
    pub flags: u8,
    pub occlusion: u8,
    pub fluid_kind: u8,
    pub fluid_level: u8,
}

impl Block {
    pub const AIR: Self = Self {
        state_id: 0,
        tint: 0,
        fluid_tint: 0,
        flags: 0,
        occlusion: 0,
        fluid_kind: NO_FLUID,
        fluid_level: 0,
    };

    pub const fn opaque_cube(state_id: u32) -> Self {
        Self {
            state_id,
            flags: OPAQUE,
            occlusion: ALL_FACES,
            ..Self::AIR
        }
    }

    pub(crate) fn occludes(self, side: Direction) -> bool {
        self.flags & OPAQUE != 0 && self.occlusion & side.bit() != 0
    }

    /// Mesher-level "can draw something": a renderable cube flag or any fluid.
    pub(crate) fn renderable(self) -> bool {
        self.flags & (OPAQUE | CUTOUT | TRANSLUCENT) != 0 || self.fluid() != NO_FLUID
    }

    pub(crate) fn fluid(self) -> u8 {
        if self.flags & WATERLOGGED != 0 {
            return WATER;
        }
        match self.fluid_kind {
            WATER | LAVA => self.fluid_kind,
            _ => NO_FLUID,
        }
    }
}

/// A single sampled cell's light, with block in the low nibble and sky in the high.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Light(pub u8);

impl Light {
    pub const fn new(block: u8, sky: u8) -> Self {
        Self((block & 15) | ((sky & 15) << 4))
    }

    pub const fn block(self) -> u8 {
        self.0 & 15
    }
    pub const fn sky(self) -> u8 {
        self.0 >> 4
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Sample {
    pub block: Block,
    pub light: Light,
    pub(crate) reserved: [u8; 3],
}

impl Sample {
    pub const fn new(block: Block, light: Light) -> Self {
        Self {
            block,
            light,
            reserved: [0; 3],
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum MissingNeighbor {
    #[default]
    Air,
    Opaque,
}

impl MissingNeighbor {
    pub(crate) const fn sample(self) -> Sample {
        match self {
            Self::Air => Sample::new(Block::AIR, Light::new(0, 15)),
            Self::Opaque => Sample::new(Block::opaque_cube(1), Light::new(0, 0)),
        }
    }
}

pub const fn section_index(x: usize, y: usize, z: usize) -> usize {
    y * FACE_CELLS + z * EDGE + x
}

fn nibble(bytes: &[u8; LIGHT_BYTES], index: usize) -> u8 {
    (bytes[index / 2] >> ((index & 1) * 4)) & 15
}

fn set_nibble(bytes: &mut [u8; LIGHT_BYTES], index: usize, level: u8) {
    let shift = (index & 1) * 4;
    let byte = &mut bytes[index / 2];
    *byte = (*byte & !(15 << shift)) | (level << shift);
}

/// Index the optional halo outside two or three axes. Coordinates are section
/// local, each in -1..=16. Interior and single-axis border positions have
/// their own arrays and return None.
pub fn edge_corner_index(p: [i32; 3]) -> Option<usize> {
    let mut sides = 0;
    let mut outside = 0;
    let mut free = 0;
    for (axis, coordinate) in p.into_iter().enumerate() {
        match coordinate {
            -1 => outside += 1,
            16 => {
                sides |= 1 << outside;
                outside += 1;
            }
            0..=15 => free = axis,
            _ => return None,
        }
    }
    match outside {
        2 => Some((free * 4 + sides) * EDGE + p[free] as usize),
        3 => Some(12 * EDGE + sides),
        _ => None,
    }
}

/// Expanded protocol-767 states and the two native packed light arrays.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Section {
    pub blocks: Box<[Block; SECTION_CELLS]>,
    pub block_light: [u8; LIGHT_BYTES],
    pub sky_light: [u8; LIGHT_BYTES],
}

/// Block storage for one section. A section whose 4,096 cells all hold the
/// same block (uniform air, deep stone, ocean water — the large majority of
/// sections in a world) allocates nothing: [`SectionBlocks::Uniform`] keeps
/// the single `Block` value inline. [`SectionBlocks::Dense`] is the full
/// expanded array and is only materialized by the first non-uniform write
/// ([`CompressedSection::set`]) or an explicit
/// [`CompressedSection::expand`].
#[derive(Clone, Debug)]
pub enum SectionBlocks {
    /// Every cell holds this block; zero heap allocation.
    Uniform(Block),
    /// Fully expanded per-cell storage.
    Dense(Box<[Block; SECTION_CELLS]>),
}

impl SectionBlocks {
    /// Whether every cell holds one shared block value.
    pub fn is_uniform(&self) -> bool {
        matches!(self, Self::Uniform(_))
    }

    /// The shared block value, or `None` for dense storage.
    pub fn uniform_block(&self) -> Option<Block> {
        match self {
            Self::Uniform(block) => Some(*block),
            Self::Dense(_) => None,
        }
    }

    /// Heap bytes held by the block storage: `0` for `Uniform`, the full
    /// expanded array for `Dense`. (Inline fields and the two light arrays
    /// are not heap-allocated and are not counted.)
    pub fn heap_bytes(&self) -> usize {
        match self {
            Self::Uniform(_) => 0,
            Self::Dense(dense) => std::mem::size_of_val(&**dense),
        }
    }

    /// Cell value at a flat `section_index` position; `index >= SECTION_CELLS`
    /// is out of range and panics like array indexing.
    pub fn cell(&self, index: usize) -> Block {
        match self {
            Self::Uniform(block) => {
                assert!(index < SECTION_CELLS);
                *block
            }
            Self::Dense(dense) => dense[index],
        }
    }

    /// The expanded array, when the section is dense.
    pub fn as_dense(&self) -> Option<&[Block; SECTION_CELLS]> {
        match self {
            Self::Dense(dense) => Some(dense),
            Self::Uniform(_) => None,
        }
    }

    /// Ensures dense storage and returns the expanded array for mutation.
    fn promote(&mut self) -> &mut [Block; SECTION_CELLS] {
        if let Self::Uniform(block) = self {
            *self = Self::Dense(Box::new([*block; SECTION_CELLS]));
        }
        match self {
            Self::Dense(dense) => dense,
            Self::Uniform(_) => unreachable!(),
        }
    }

    /// Content equality across representations: a `Uniform` equals a `Dense`
    /// whose cells all hold the same block.
    fn content_eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Uniform(a), Self::Uniform(b)) => a == b,
            (Self::Dense(a), Self::Dense(b)) => a == b,
            (Self::Uniform(u), Self::Dense(d)) | (Self::Dense(d), Self::Uniform(u)) => {
                d.iter().all(|cell| cell == u)
            }
        }
    }
}

impl PartialEq for SectionBlocks {
    fn eq(&self, other: &Self) -> bool {
        self.content_eq(other)
    }
}

impl Eq for SectionBlocks {}

/// Stored voxel + light data for one section, keeping [`Section`]'s exact
/// semantics while compressing uniform sections to zero heap blocks.
///
/// `set` promotes `Uniform` to `Dense` only on the first write that changes
/// the block value (light-only writes keep the zero-heap form) and collapses
/// back to `Uniform` automatically when a write makes every cell equal
/// again.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompressedSection {
    pub blocks: SectionBlocks,
    pub block_light: [u8; LIGHT_BYTES],
    pub sky_light: [u8; LIGHT_BYTES],
}

impl CompressedSection {
    /// A section of one repeated block and uniform light, in zero-heap form.
    pub fn uniform(block: Block, light: Light) -> Self {
        Self {
            blocks: SectionBlocks::Uniform(block),
            block_light: [light.block() * 17; LIGHT_BYTES],
            sky_light: [light.sky() * 17; LIGHT_BYTES],
        }
    }

    /// Whether the block storage is the zero-heap `Uniform` form.
    pub fn is_uniform(&self) -> bool {
        self.blocks.is_uniform()
    }

    /// Heap bytes held by the block storage (`0` when uniform).
    pub fn heap_bytes(&self) -> usize {
        self.blocks.heap_bytes()
    }

    /// Reads a local cell, identical to [`Section::get`]. Panics when a
    /// coordinate is outside 0..16.
    pub fn get(&self, x: usize, y: usize, z: usize) -> Sample {
        assert!(x < EDGE && y < EDGE && z < EDGE);
        let index = section_index(x, y, z);
        Sample::new(
            self.blocks.cell(index),
            Light::new(
                nibble(&self.block_light, index),
                nibble(&self.sky_light, index),
            ),
        )
    }

    /// Writes a cell with [`Section::set`] semantics. Returns whether the
    /// cell changed; out-of-section coordinates have no effect. The first
    /// write of a different block promotes `Uniform` storage to `Dense`; a
    /// write that makes every cell equal collapses it back.
    pub fn set(&mut self, x: usize, y: usize, z: usize, block: Block, light: Light) -> bool {
        if [x, y, z].into_iter().any(|coordinate| coordinate >= EDGE) {
            return false;
        }
        let index = section_index(x, y, z);
        if self.blocks.cell(index) == block
            && nibble(&self.block_light, index) == light.block()
            && nibble(&self.sky_light, index) == light.sky()
        {
            return false;
        }
        if self.blocks.uniform_block() != Some(block) {
            self.blocks.promote()[index] = block;
        }
        set_nibble(&mut self.block_light, index, light.block());
        set_nibble(&mut self.sky_light, index, light.sky());
        self.collapse_if_uniform();
        true
    }

    /// Collapses `Dense` storage to `Uniform` when every cell holds the same
    /// block. Returns whether the representation changed.
    pub fn collapse_if_uniform(&mut self) -> bool {
        let SectionBlocks::Dense(dense) = &self.blocks else {
            return false;
        };
        let first = dense[0];
        if !dense.iter().all(|&cell| cell == first) {
            return false;
        }
        self.blocks = SectionBlocks::Uniform(first);
        true
    }

    /// Materializes the fully expanded [`Section`] meshing snapshot. Dense
    /// sections copy their block array; `Uniform` sections expand the single
    /// block into it.
    pub fn expand(&self) -> Section {
        Section {
            blocks: match &self.blocks {
                SectionBlocks::Dense(dense) => dense.clone(),
                SectionBlocks::Uniform(block) => Box::new([*block; SECTION_CELLS]),
            },
            block_light: self.block_light,
            sky_light: self.sky_light,
        }
    }
}

impl From<Section> for CompressedSection {
    /// Stores the section, collapsing a fully uniform block array to the
    /// zero-heap form.
    fn from(section: Section) -> Self {
        let mut stored = Self {
            blocks: SectionBlocks::Dense(section.blocks),
            block_light: section.block_light,
            sky_light: section.sky_light,
        };
        stored.collapse_if_uniform();
        stored
    }
}

impl From<CompressedSection> for Section {
    fn from(stored: CompressedSection) -> Self {
        stored.expand()
    }
}

impl Section {
    pub fn filled(block: Block, light: Light) -> Self {
        Self {
            blocks: Box::new([block; SECTION_CELLS]),
            block_light: [light.block() * 17; LIGHT_BYTES],
            sky_light: [light.sky() * 17; LIGHT_BYTES],
        }
    }

    /// Reads a local cell. Panics when a coordinate is outside 0..16.
    pub fn get(&self, x: usize, y: usize, z: usize) -> Sample {
        assert!(x < EDGE && y < EDGE && z < EDGE);
        self.view().sample_local(x, y, z)
    }

    /// Returns whether the cell changed. Out-of-section coordinates have no effect.
    pub fn set(&mut self, x: usize, y: usize, z: usize, block: Block, light: Light) -> bool {
        if [x, y, z].into_iter().any(|coordinate| coordinate >= EDGE) {
            return false;
        }
        let index = section_index(x, y, z);
        if self.blocks[index] == block
            && nibble(&self.block_light, index) == light.block()
            && nibble(&self.sky_light, index) == light.sky()
        {
            return false;
        }
        self.blocks[index] = block;
        set_nibble(&mut self.block_light, index, light.block());
        set_nibble(&mut self.sky_light, index, light.sky());
        true
    }

    pub fn view(&self) -> SectionView<'_> {
        SectionView {
            blocks: &self.blocks,
            block_light: &self.block_light,
            sky_light: &self.sky_light,
            borders: [None; 6],
            edges_and_corners: None,
            missing: MissingNeighbor::Air,
        }
    }
}

#[derive(Clone, Copy)]
pub struct SectionView<'a> {
    pub blocks: &'a [Block; SECTION_CELLS],
    pub block_light: &'a [u8; LIGHT_BYTES],
    pub sky_light: &'a [u8; LIGHT_BYTES],
    pub borders: [Option<&'a [Sample; FACE_CELLS]>; 6],
    pub edges_and_corners: Option<&'a [Sample; EDGE_CORNER_CELLS]>,
    pub missing: MissingNeighbor,
}

impl SectionView<'_> {
    pub(crate) fn sample_local(&self, x: usize, y: usize, z: usize) -> Sample {
        let index = section_index(x, y, z);
        Sample::new(
            self.blocks[index],
            Light::new(
                nibble(self.block_light, index),
                nibble(self.sky_light, index),
            ),
        )
    }

    pub fn sample(&self, p: [i32; 3]) -> Sample {
        let mut outside = None;
        let mut local = [0; 3];
        for (axis, coordinate) in p.into_iter().enumerate() {
            let direction = match (axis, coordinate) {
                (_, 0..=15) => {
                    local[axis] = coordinate as usize;
                    continue;
                }
                (0, -1) => Direction::West,
                (0, 16) => Direction::East,
                (1, -1) => Direction::Down,
                (1, 16) => Direction::Up,
                (2, -1) => Direction::North,
                (2, 16) => Direction::South,
                _ => return self.missing.sample(),
            };
            if outside.is_some() {
                return self
                    .edges_and_corners
                    .zip(edge_corner_index(p))
                    .map_or_else(|| self.missing.sample(), |(halo, i)| halo[i]);
            }
            outside = Some(direction);
        }
        if let Some(direction) = outside {
            return self.borders[direction as usize].map_or_else(
                || self.missing.sample(),
                |border| border[direction.border_index(local)],
            );
        }
        self.sample_local(local[0], local[1], local[2])
    }
}
