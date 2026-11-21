// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later

pub const EDGE: usize = 16;
pub const FACE_CELLS: usize = EDGE * EDGE;
pub const SECTION_CELLS: usize = EDGE * FACE_CELLS;
pub const LIGHT_BYTES: usize = SECTION_CELLS / 2;
pub const EDGE_CORNER_CELLS: usize = 12 * EDGE + 8;

/// Direction values follow the server's voxel-face order.
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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

    #[cfg(test)]
    pub(crate) fn occludes(self, side: Direction) -> bool {
        self.flags & OPAQUE != 0 && self.occlusion & side.bit() != 0
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
