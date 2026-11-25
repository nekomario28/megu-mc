// SPDX-License-Identifier: LGPL-3.0-or-later

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
//! still blocks it. The flood fill itself is
//! [`megu_core::render_cs::compute_section_visibility`] — allocation-free
//! with a `VoxelBitset4096` visited set and a fixed 4096-entry stack.

use megu_core::bloom::VoxelBitset4096;
use megu_core::render_cs::{compute_section_visibility, FaceDir, SectionVisBits};

use crate::section::{Block, Direction, ALL_FACES, OPAQUE, SECTION_CELLS};

const FACES: usize = 6;
/// All 36 entries of the 6x6 matrix, bit `enter * 6 + exit` per pair.
const MATRIX_BITS: u64 = (1 << (FACES * FACES)) - 1;

/// `Direction` order (Down/Up/North/South/West/East) expressed as
/// `FaceDir`s — `DIR_TO_FACEDIR[d as usize]` is the same world face.
const DIR_TO_FACEDIR: [FaceDir; FACES] = [
    FaceDir::NegY, // Direction::Down
    FaceDir::PosY, // Direction::Up
    FaceDir::NegZ, // Direction::North
    FaceDir::PosZ, // Direction::South
    FaceDir::NegX, // Direction::West
    FaceDir::PosX, // Direction::East
];

/// `FaceDir` index order (PosX/NegX/PosY/NegY/PosZ/NegZ) expressed as
/// `Direction`s — `FACEDIR_TO_DIR[fd as usize]` is the same world face.
const FACEDIR_TO_DIR: [Direction; FACES] = [
    Direction::East,  // FaceDir::PosX
    Direction::West,  // FaceDir::NegX
    Direction::Up,    // FaceDir::PosY
    Direction::Down,  // FaceDir::NegY
    Direction::South, // FaceDir::PosZ
    Direction::North, // FaceDir::NegZ
];

/// The `FaceDir` naming the same world axis/sign as `direction`.
#[inline(always)]
pub const fn direction_face_dir(direction: Direction) -> FaceDir {
    DIR_TO_FACEDIR[direction as usize]
}

/// The `Direction` naming the same world axis/sign as `face`.
#[inline(always)]
pub const fn face_dir_direction(face: FaceDir) -> Direction {
    FACEDIR_TO_DIR[face as usize]
}

/// Whether `block` stops sight traversal through its cell.
#[inline(always)]
pub(crate) fn blocks_sight(block: Block) -> bool {
    block.flags & OPAQUE != 0 && block.occlusion & ALL_FACES == ALL_FACES
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
    /// `MEGU_MESH_COLUMN_UNIFORM_AIR` guarantee.
    pub const fn uniform_air() -> Self {
        Self::ALL
    }

    /// O(1) graph for a caller-known section of only full-occlusion opaque
    /// non-fluid cubes, matching the `MEGU_MESH_COLUMN_SEALED_SOLID`
    /// guarantee: sight cannot cross any cell, so no face reaches another.
    pub const fn sealed_solid() -> Self {
        Self::NONE
    }

    /// The packed matrix: bit `from * 6 + to`.
    pub const fn bits(self) -> u64 {
        self.0
    }

    /// Wrap an already-packed `Direction`-ordered matrix — the inverse of
    /// [`Self::bits`].
    pub const fn from_bits(bits: u64) -> Self {
        Self(bits)
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

    /// The same connectivity in `FaceDir` bit order — bit `a * 6 + b` of
    /// the [`SectionVisBits`] is set iff entering through `FaceDir` `a`
    /// can exit through `b`. `SectionVisBits::set` marks both directions,
    /// matching this matrix's symmetry.
    #[must_use]
    pub fn to_vis_bits(self) -> SectionVisBits {
        if self.0 == 0 {
            return SectionVisBits::EMPTY;
        }
        if self.0 == MATRIX_BITS {
            return SectionVisBits::ALL;
        }
        let mut out = SectionVisBits::EMPTY;
        for (enter, &fd_enter) in DIR_TO_FACEDIR.iter().enumerate() {
            for (exit, &fd_exit) in DIR_TO_FACEDIR.iter().enumerate() {
                if self.0 >> (enter * FACES + exit) & 1 != 0 {
                    out.set(fd_enter, fd_exit);
                }
            }
        }
        out
    }

    /// The same connectivity in `Direction` bit order — the inverse of
    /// [`Self::to_vis_bits`].
    #[must_use]
    pub fn from_vis_bits(bits: SectionVisBits) -> Self {
        if bits == SectionVisBits::EMPTY {
            return Self::NONE;
        }
        if bits == SectionVisBits::ALL {
            return Self::ALL;
        }
        let mut out = 0u64;
        for (enter, &fd_enter) in DIR_TO_FACEDIR.iter().enumerate() {
            for (exit, &fd_exit) in DIR_TO_FACEDIR.iter().enumerate() {
                if bits.can_see(fd_enter, fd_exit) {
                    out |= 1 << (enter * FACES + exit);
                }
            }
        }
        Self(out)
    }

    /// Compute the graph from cell data alone. The closed-cell predicate
    /// packs into a [`VoxelBitset4096`] whose flood fill is
    /// [`megu_core::render_cs::compute_section_visibility`]'s — the same
    /// 6-connected component walk, the same "every pair of faces one
    /// component touches is mutually reachable" rule, and O(1) exits for
    /// uniform sections.
    pub fn compute(blocks: &[Block; SECTION_CELLS]) -> Self {
        let mut opaque = VoxelBitset4096::new();
        let mut any_open = false;
        let mut any_closed = false;
        let mut states = [0u16; 64];
        for (w, chunk) in blocks.as_chunks::<64>().0.iter().enumerate() {
            for (i, &block) in chunk.iter().enumerate() {
                states[i] = u16::from(blocks_sight(block));
            }
            let mut m = megu_core::simd_voxel::build_non_air_bitmask_64(&states);
            any_closed |= m != 0;
            any_open |= m != u64::MAX;
            while m != 0 {
                let bit = m.trailing_zeros() as usize;
                opaque.set((w * 64 + bit) as u16);
                m &= m - 1;
            }
        }
        if !any_open {
            return Self::NONE;
        }
        if !any_closed {
            return Self::ALL;
        }
        Self::from_vis_bits(compute_section_visibility(&opaque))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::section::{EDGE, FACE_CELLS};

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
