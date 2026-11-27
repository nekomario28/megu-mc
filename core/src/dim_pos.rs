// SPDX-License-Identifier: LGPL-3.0-or-later
#![forbid(unsafe_code)]
//! A block coordinate that carries its dimension, so world position and world
//! identity cannot be separated by accident.
//!
//! [`crate::coords::BlockPos`] is deliberately dimension-free: it is the right
//! type for shape math, ray casts and section-local indexing. The moment a
//! position is stored, queued, sent, compared for equality, or used as a map
//! key, the dimension is part of its identity — the same `(x, y, z)` in the
//! overworld and the nether is two different blocks. [`DimBlockCoord`] adds
//! exactly that axis while reusing [`DimIndex`] and [`BlockPos`] instead of
//! redefining either.
//!
//! Chunk and section math delegates to [`BlockPos`], whose `>> 4` accessors are
//! floor division and therefore already correct for negative coordinates.

use crate::coords::{BlockPos, ChunkKey, DimIndex, SectionKey};

/// One block position in one dimension: the canonical dimension-aware
/// coordinate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct DimBlockCoord {
    /// Dimension identity (`0` overworld, `1` nether, `2` end, modded `>= 3`).
    pub dim: DimIndex,
    /// Block X.
    pub x: i32,
    /// Block Y.
    pub y: i32,
    /// Block Z.
    pub z: i32,
}

impl DimBlockCoord {
    /// Position `(x, y, z)` in dimension `dim`.
    #[must_use]
    pub const fn new(dim: DimIndex, x: i32, y: i32, z: i32) -> Self {
        Self { dim, x, y, z }
    }

    /// Attach `dim` to a dimension-free [`BlockPos`].
    #[must_use]
    pub const fn from_block_pos(dim: DimIndex, pos: BlockPos) -> Self {
        Self {
            dim,
            x: pos.x,
            y: pos.y,
            z: pos.z,
        }
    }

    /// The dimension-free position, for shape/ray math that does not key state.
    #[must_use]
    pub const fn block_pos(self) -> BlockPos {
        BlockPos::new(self.x, self.y, self.z)
    }

    /// Offset by `(dx, dy, dz)` blocks in the same dimension.
    ///
    /// Addition saturates at the `i32` bounds instead of panicking or wrapping:
    /// a coordinate that leaves the representable world stays at the edge
    /// deterministically, on every build profile.
    #[must_use]
    pub const fn offset(self, dx: i32, dy: i32, dz: i32) -> Self {
        Self {
            dim: self.dim,
            x: self.x.saturating_add(dx),
            y: self.y.saturating_add(dy),
            z: self.z.saturating_add(dz),
        }
    }

    /// `(cx, cz)` of the chunk column containing this block.
    #[must_use]
    pub const fn chunk_xz(self) -> (i32, i32) {
        self.block_pos().chunk_xz()
    }

    /// The dimension-tagged chunk column containing this block.
    #[must_use]
    pub const fn chunk_key(self) -> ChunkKey {
        self.block_pos().chunk_key(self.dim)
    }

    /// Index of the `16³` section containing this block along Y
    /// (`block_y >> 4`, so `y = -64` is section `-4`).
    #[must_use]
    pub const fn section_y(self) -> i32 {
        self.block_pos().section_y()
    }

    /// The dimension-tagged section containing this block.
    #[must_use]
    pub const fn section_key(self) -> SectionKey {
        self.block_pos().section_key(self.dim)
    }

    /// Whether this block lies in the vertical world bounds
    /// `min_y..min_y_excl`, e.g. `(-64, 320)` for the 1.21.1 overworld.
    #[must_use]
    pub const fn in_bounds(self, min_y: i32, max_y_excl: i32) -> bool {
        self.y >= min_y && self.y < max_y_excl
    }
}

#[cfg(test)]
mod tests {
    use super::DimBlockCoord;
    use crate::coords::{BlockPos, DimIndex};
    use std::collections::{HashMap, HashSet};

    #[test]
    fn negative_coordinates_floor_to_the_correct_chunk_and_section() {
        let coord = DimBlockCoord::new(DimIndex::OVERWORLD, -1, -64, -17);
        assert_eq!(coord.chunk_xz(), (-1, -2));
        assert_eq!(coord.section_y(), -4);
        // A naive `/ 16` would truncate -1 to 0 and put it in the wrong chunk.
        assert_ne!(coord.chunk_xz(), (0, 0));

        assert_eq!(
            DimBlockCoord::new(DimIndex::OVERWORLD, -16, -1, -16).chunk_xz(),
            (-1, -1)
        );
        assert_eq!(
            DimBlockCoord::new(DimIndex::OVERWORLD, -17, -1, -17).chunk_xz(),
            (-2, -2)
        );
        assert_eq!(
            DimBlockCoord::new(DimIndex::OVERWORLD, 15, 0, 15).chunk_xz(),
            (0, 0)
        );
        assert_eq!(
            DimBlockCoord::new(DimIndex::OVERWORLD, 16, 0, 16).chunk_xz(),
            (1, 1)
        );
    }

    #[test]
    fn chunk_and_section_keys_keep_the_dimension() {
        let coord = DimBlockCoord::new(DimIndex::NETHER, 100, 70, -100);
        let chunk = coord.chunk_key();
        assert_eq!(chunk.dim, DimIndex::NETHER);
        assert_eq!((chunk.cx, chunk.cz), (6, -7));

        let section = coord.section_key();
        assert_eq!(section.dim, DimIndex::NETHER);
        assert_eq!((section.cx, section.sy, section.cz), (6, 4, -7));
        assert_eq!(section.chunk(), chunk);
    }

    #[test]
    fn dimension_is_part_of_identity() {
        let overworld = DimBlockCoord::new(DimIndex::OVERWORLD, 3, 64, 3);
        let nether = DimBlockCoord::new(DimIndex::NETHER, 3, 64, 3);
        assert_ne!(overworld, nether);
        assert_eq!(overworld.block_pos(), nether.block_pos());

        let mut set = HashSet::new();
        set.insert(overworld);
        set.insert(nether);
        assert_eq!(set.len(), 2);

        let mut map = HashMap::new();
        map.insert(overworld, "overworld");
        map.insert(nether, "nether");
        assert_eq!(
            map.get(&DimBlockCoord::new(DimIndex::NETHER, 3, 64, 3)),
            Some(&"nether")
        );
        assert_eq!(map.get(&DimBlockCoord::new(DimIndex::END, 3, 64, 3)), None);
    }

    #[test]
    fn offset_moves_all_axes_and_saturates() {
        let coord = DimBlockCoord::new(DimIndex::END, 0, 64, 0);
        assert_eq!(
            coord.offset(1, -2, 3),
            DimBlockCoord::new(DimIndex::END, 1, 62, 3)
        );
        assert_eq!(coord.offset(0, 0, 0), coord);

        let max = DimBlockCoord::new(DimIndex::OVERWORLD, i32::MAX, i32::MAX, i32::MAX);
        assert_eq!(
            max.offset(1, 1, 1),
            DimBlockCoord::new(DimIndex::OVERWORLD, i32::MAX, i32::MAX, i32::MAX)
        );
        let min = DimBlockCoord::new(DimIndex::OVERWORLD, i32::MIN, i32::MIN, i32::MIN);
        assert_eq!(
            min.offset(-1, -1, -1),
            DimBlockCoord::new(DimIndex::OVERWORLD, i32::MIN, i32::MIN, i32::MIN)
        );
        // Saturation never drops the dimension.
        assert_eq!(max.offset(1, 0, 0).dim, DimIndex::OVERWORLD);
    }

    #[test]
    fn in_bounds_uses_an_exclusive_upper_bound() {
        let coord = DimBlockCoord::new(DimIndex::OVERWORLD, 0, -64, 0);
        assert!(coord.in_bounds(-64, 320));
        assert!(coord.offset(0, 1, 0).in_bounds(-64, 320));
        assert!(!coord.offset(0, -1, 0).in_bounds(-64, 320));

        let top = DimBlockCoord::new(DimIndex::OVERWORLD, 0, 319, 0);
        assert!(top.in_bounds(-64, 320));
        assert!(!top.offset(0, 1, 0).in_bounds(-64, 320));

        // An empty range contains nothing, including its own lower bound.
        assert!(!top.in_bounds(320, 320));
    }

    #[test]
    fn block_pos_roundtrip_preserves_coordinates() {
        let pos = BlockPos::new(-30, 5, 900);
        let coord = DimBlockCoord::from_block_pos(DimIndex::OVERWORLD, pos);
        assert_eq!(coord.block_pos(), pos);
        assert_eq!(coord.chunk_xz(), pos.chunk_xz());
        assert_eq!(coord.section_y(), pos.section_y());
    }
}
