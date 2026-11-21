// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later

//! Section visibility graph for graph occlusion culling. For every ordered
//! pair of section
//! faces `(enter, exit)` the graph records whether a path of open cells joins
//! the two boundary planes: a traversal that enters a section through `enter`
//! only needs to keep walking toward the faces `visible_from(enter)` reports.
//! Reachability is symmetric, so `is_connected(a, b)` equals
//! `is_connected(b, a)`; the packed matrix stores both directions so callers
//! can index it by entry face in O(1).
//!
//! A cell is open when it is not a full-occlusion opaque cube (`OPAQUE` with
//! all six `occlusion` bits set). Air, partial-occlusion cubes, cutout and
//! translucent blocks, and fluids all admit sight; a waterlogged opaque cube
//! still blocks it. The flood fill is allocation-free: a 512-bit closed
//! bitmap, a 512-bit visited bitmap reused from it, and a fixed 4096-entry
//! stack live entirely in stack memory.

use crate::section::{Block, Direction, ALL_FACES, EDGE, FACE_CELLS, OPAQUE, SECTION_CELLS};

const WORD_BITS: usize = u64::BITS as usize;
const WORDS: usize = SECTION_CELLS / WORD_BITS;
const FACES: usize = 6;
/// All 36 entries of the 6x6 matrix, bit `enter * 6 + exit` per pair.
const MATRIX_BITS: u64 = (1 << (FACES * FACES)) - 1;
const STEPS: [isize; FACES] = [
    -(FACE_CELLS as isize),
    FACE_CELLS as isize,
    -(EDGE as isize),
    EDGE as isize,
    -1,
    1,
];

/// Whether `block` stops sight traversal through its cell.
#[inline(always)]
fn blocks_sight(block: Block) -> bool {
    block.flags & OPAQUE != 0 && block.occlusion & ALL_FACES == ALL_FACES
}

/// `Direction::bit` for every boundary plane the cell at `index` lies on; an
/// edge or corner cell reports two or three faces.
#[inline(always)]
fn cell_faces(x: usize, y: usize, z: usize) -> u8 {
    let mut faces = 0;
    if y == 0 {
        faces |= Direction::Down.bit();
    }
    if y == EDGE - 1 {
        faces |= Direction::Up.bit();
    }
    if z == 0 {
        faces |= Direction::North.bit();
    }
    if z == EDGE - 1 {
        faces |= Direction::South.bit();
    }
    if x == 0 {
        faces |= Direction::West.bit();
    }
    if x == EDGE - 1 {
        faces |= Direction::East.bit();
    }
    faces
}

/// Symmetric 6x6 face-to-face reachability for one 16x16x16 section. Bit
/// `from * 6 + to` of [`FaceVisibility::bits`] is set when entering through
/// `from` can see out through `to`. `from` and `to` are `Direction` values.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct FaceVisibility(u64);

impl FaceVisibility {
    /// No pair of faces is connected.
    pub const NONE: Self = Self(0);
    /// Every face reaches every face, including itself.
    pub const ALL: Self = Self(MATRIX_BITS);

    /// O(1) graph for a caller-known uniform-air section, matching the
    /// `MC_SPINE_MESH_COLUMN_UNIFORM_AIR` guarantee.
    pub const fn uniform_air() -> Self {
        Self::ALL
    }

    /// O(1) graph for a caller-known section of only full-occlusion opaque
    /// non-fluid cubes, matching the `MC_SPINE_MESH_COLUMN_SEALED_SOLID`
    /// guarantee: sight cannot cross any cell, so no face reaches another.
    pub const fn sealed_solid() -> Self {
        Self::NONE
    }

    /// The packed matrix: bit `from * 6 + to`.
    pub const fn bits(self) -> u64 {
        self.0
    }

    /// Whether exiting through `to` is possible after entering through `from`.
    /// A face connects to itself when at least one of its boundary cells is
    /// open. `from` and `to` are `Direction` values.
    #[inline(always)]
    pub const fn is_connected(self, from: Direction, to: Direction) -> bool {
        (self.0 >> (from as usize * FACES + to as usize)) & 1 != 0
    }

    /// `Direction::bit` set of exits reachable after entering through `from`.
    #[inline(always)]
    pub const fn visible_from(self, from: Direction) -> u8 {
        (self.0 >> (from as usize * FACES)) as u8 & ALL_FACES
    }

    /// Whether no pair is connected (the `sealed_solid` shape).
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// Whether every pair is connected (the `uniform_air` shape).
    pub const fn is_full(self) -> bool {
        self.0 == MATRIX_BITS
    }

    /// Compute the graph for a section with caller fill guarantees.
    /// `uniform_air` and `sealed_solid` mirror the column-mesh flags and each
    /// return in O(1) without reading `blocks`; `uniform_air` wins when both
    /// are set.
    pub fn compute_hinted(
        blocks: &[Block; SECTION_CELLS],
        uniform_air: bool,
        sealed_solid: bool,
    ) -> Self {
        if uniform_air {
            Self::ALL
        } else if sealed_solid {
            Self::NONE
        } else {
            Self::compute(blocks)
        }
    }

    /// Compute the graph from cell data alone. One pass packs a closed-cell
    /// bitmap (uniform sections exit there), then each unvisited open cell
    /// floods its connected component and connects every boundary-face pair
    /// the component touches.
    pub fn compute(blocks: &[Block; SECTION_CELLS]) -> Self {
        let mut visited = [0u64; WORDS];
        let mut any_open = false;
        let mut any_closed = false;
        for (w, word) in visited.iter_mut().enumerate() {
            let mut m = 0u64;
            for (i, &block) in blocks[w * WORD_BITS..(w + 1) * WORD_BITS]
                .iter()
                .enumerate()
            {
                if blocks_sight(block) {
                    m |= 1 << i;
                }
            }
            *word = m;
            any_closed |= m != 0;
            any_open |= m != u64::MAX;
        }
        if !any_open {
            return Self::NONE;
        }
        if !any_closed {
            return Self::ALL;
        }

        // Closed cells start marked visited, so the walk only enters open ones.
        let mut stack = [0u16; SECTION_CELLS];
        let mut bits = 0u64;
        for start in 0..SECTION_CELLS {
            let word = &mut visited[start / WORD_BITS];
            let start_bit = 1 << (start % WORD_BITS);
            if *word & start_bit != 0 {
                continue;
            }
            let mut faces = 0u8;
            let mut top = 0usize;
            *word |= start_bit;
            stack[top] = start as u16;
            top += 1;
            while top != 0 {
                top -= 1;
                let index = stack[top] as usize;
                let x = index % EDGE;
                let z = (index / EDGE) % EDGE;
                let y = index / FACE_CELLS;
                faces |= cell_faces(x, y, z);
                let mut count = 0usize;
                let mut next = [0usize; FACES];
                if x != 0 {
                    next[count] = (index as isize + STEPS[Direction::West as usize]) as usize;
                    count += 1;
                }
                if x != EDGE - 1 {
                    next[count] = (index as isize + STEPS[Direction::East as usize]) as usize;
                    count += 1;
                }
                if z != 0 {
                    next[count] = (index as isize + STEPS[Direction::North as usize]) as usize;
                    count += 1;
                }
                if z != EDGE - 1 {
                    next[count] = (index as isize + STEPS[Direction::South as usize]) as usize;
                    count += 1;
                }
                if y != 0 {
                    next[count] = (index as isize + STEPS[Direction::Down as usize]) as usize;
                    count += 1;
                }
                if y != EDGE - 1 {
                    next[count] = (index as isize + STEPS[Direction::Up as usize]) as usize;
                    count += 1;
                }
                for &n in &next[..count] {
                    let word = &mut visited[n / WORD_BITS];
                    let bit = 1 << (n % WORD_BITS);
                    if *word & bit == 0 {
                        *word |= bit;
                        stack[top] = n as u16;
                        top += 1;
                    }
                }
            }
            let row = u64::from(faces);
            let mut touched = faces;
            while touched != 0 {
                let enter = touched.trailing_zeros() as usize;
                touched &= touched - 1;
                bits |= row << (enter * FACES);
            }
            if bits == MATRIX_BITS {
                break;
            }
        }
        Self(bits)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn blocks_of(block: Block) -> Box<[Block; SECTION_CELLS]> {
        Box::new([block; SECTION_CELLS])
    }

    fn idx(x: usize, y: usize, z: usize) -> usize {
        y * FACE_CELLS + z * EDGE + x
    }

    #[test]
    fn air_section_connects_every_face_pair() {
        let vis = FaceVisibility::compute(&blocks_of(Block::AIR));
        assert_eq!(vis, FaceVisibility::ALL);
        assert!(vis.is_full());
        for &enter in &Direction::ALL {
            assert_eq!(vis.visible_from(enter), ALL_FACES);
            for &exit in &Direction::ALL {
                assert!(vis.is_connected(enter, exit));
            }
        }
        assert_eq!(
            FaceVisibility::compute_hinted(&blocks_of(Block::AIR), true, false),
            FaceVisibility::ALL
        );
        assert_eq!(FaceVisibility::uniform_air(), FaceVisibility::ALL);
    }

    #[test]
    fn sealed_section_connects_nothing() {
        let solid = blocks_of(Block::opaque_cube(1));
        assert_eq!(FaceVisibility::compute(&solid), FaceVisibility::NONE);
        assert!(FaceVisibility::compute(&solid).is_empty());
        assert_eq!(
            FaceVisibility::compute_hinted(&solid, false, true),
            FaceVisibility::NONE
        );
        assert_eq!(FaceVisibility::sealed_solid(), FaceVisibility::NONE);
    }

    #[test]
    fn enclosed_cavity_never_reaches_a_face() {
        let mut blocks = blocks_of(Block::opaque_cube(1));
        for y in 2..14 {
            for z in 2..14 {
                for x in 2..14 {
                    blocks[idx(x, y, z)] = Block::AIR;
                }
            }
        }
        assert_eq!(FaceVisibility::compute(&blocks), FaceVisibility::NONE);
    }

    #[test]
    fn partial_occlusion_and_fluid_cells_stay_open() {
        let mut stairs = Block::opaque_cube(7);
        stairs.occlusion = Direction::Up.bit();
        let mut blocks = blocks_of(stairs);
        for b in blocks.iter_mut().step_by(3) {
            b.fluid_kind = crate::WATER;
        }
        assert_eq!(FaceVisibility::compute(&blocks), FaceVisibility::ALL);
    }

    #[test]
    fn full_wall_disconnects_opposite_faces_only() {
        let mut blocks = blocks_of(Block::AIR);
        for z in 0..EDGE {
            for y in 0..EDGE {
                blocks[idx(8, y, z)] = Block::opaque_cube(1);
            }
        }
        let vis = FaceVisibility::compute(&blocks);
        assert!(!vis.is_connected(Direction::West, Direction::East));
        assert!(!vis.is_connected(Direction::East, Direction::West));
        for (a, b) in [
            (Direction::West, Direction::West),
            (Direction::West, Direction::Down),
            (Direction::West, Direction::North),
            (Direction::East, Direction::Up),
            (Direction::East, Direction::South),
            (Direction::North, Direction::East),
            (Direction::Down, Direction::Up),
            (Direction::North, Direction::South),
        ] {
            assert!(vis.is_connected(a, b), "{a:?} -> {b:?}");
        }
        // Each half touches its own side plus the four shared planes.
        assert_eq!(vis.visible_from(Direction::West), 0b011111);
        assert_eq!(vis.visible_from(Direction::East), 0b101111);
        assert_eq!(vis.visible_from(Direction::North), ALL_FACES);
    }

    #[test]
    fn straight_tunnel_connects_exactly_two_faces() {
        let mut blocks = blocks_of(Block::opaque_cube(1));
        for x in 0..EDGE {
            blocks[idx(x, 8, 8)] = Block::AIR;
        }
        let vis = FaceVisibility::compute(&blocks);
        let we = Direction::West.bit() | Direction::East.bit();
        assert_eq!(vis.visible_from(Direction::West), we);
        assert_eq!(vis.visible_from(Direction::East), we);
        for &dir in &[
            Direction::Down,
            Direction::Up,
            Direction::North,
            Direction::South,
        ] {
            assert_eq!(vis.visible_from(dir), 0);
        }
        let expected = (u64::from(we) << (Direction::West as usize * 6))
            | (u64::from(we) << (Direction::East as usize * 6));
        assert_eq!(vis.bits(), expected);
        for &enter in &Direction::ALL {
            for &exit in &Direction::ALL {
                assert_eq!(
                    vis.is_connected(enter, exit),
                    vis.is_connected(exit, enter),
                    "asymmetry {enter:?} {exit:?}"
                );
            }
        }
    }

    #[test]
    fn waterlogged_opaque_cube_still_blocks_sight() {
        let mut waterlogged = Block::opaque_cube(1);
        waterlogged.flags |= crate::WATERLOGGED;
        assert_eq!(
            FaceVisibility::compute(&blocks_of(waterlogged)),
            FaceVisibility::NONE
        );
    }
}
