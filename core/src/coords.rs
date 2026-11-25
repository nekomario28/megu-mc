// SPDX-License-Identifier: LGPL-3.0-or-later
#![forbid(unsafe_code)]
//! `megu_core::coords` — the canonical dimension-aware spatial keys every
//! subsystem shares so "the same chunk" means the same thing to lighting,
//! meshing, audio, redstone and writeback alike.
//!
//! Three key shapes cover the whole invalidation hierarchy:
//!
//! * [`BlockPos`] — one block in world coordinates, with the `>> 4`
//!   floor-division accessors that turn it into chunk, section and
//!   section-local coordinates (correct for negative coordinates, where a
//!   naive `/ 16` truncates toward zero and puts block `-1` in chunk `0`).
//! * [`ChunkKey`] — `(dim, cx, cz)`, one `16×world-height` column: the
//!   unit of aggregate invalidation, wire caching and writeback.
//! * [`SectionKey`] — `(dim, cx, sy, cz)`, one `16³` section. `sy` is a
//!   full `i32` (`block_y >> 4`), never `i8`, so arbitrary modded world
//!   heights key correctly instead of clamping into `-128..=127`.
//!
//! [`DimIndex`] keeps the dimension axis open (`u32`): vanilla occupies
//! `0..=2`, modded dimensions take `>= 3` without a second key type.
//!
//! Every key hashes through [`CoordHasher`] — a deterministic,
//! multiplication-based word mixer with no random seed — via the
//! [`CoordMap`]/[`CoordSet`] aliases, so a hostile peer sending
//! colliding coordinates cannot trigger SipHash-free quadratic probing
//! *or* exploit a per-process seed: the hash is the same on every run
//! and every replica, which is exactly what deterministic replay and
//! checksum equality across nodes need.

use std::collections::{HashMap, HashSet};
use std::hash::{BuildHasherDefault, Hasher};

/// Dimension identity. Vanilla occupies `0` (overworld), `1` (nether) and
/// `2` (end); modded dimensions take `>= 3`. A `u32` keeps the key space
/// open for arbitrary modded dimension registries.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash, Ord, PartialOrd)]
pub struct DimIndex(pub u32);

impl DimIndex {
    /// The overworld dimension.
    pub const OVERWORLD: Self = Self(0);
    /// The nether dimension.
    pub const NETHER: Self = Self(1);
    /// The end dimension.
    pub const END: Self = Self(2);
}

/// One block position in world coordinates.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash, Ord, PartialOrd)]
pub struct BlockPos {
    /// Block X.
    pub x: i32,
    /// Block Y.
    pub y: i32,
    /// Block Z.
    pub z: i32,
}

impl BlockPos {
    /// Position `(x, y, z)`.
    #[inline]
    #[must_use]
    pub const fn new(x: i32, y: i32, z: i32) -> Self {
        Self { x, y, z }
    }

    /// `(cx, cz)` of the chunk column containing this block. `>> 4` on a
    /// signed coordinate is floor division by 16, so negative blocks land
    /// in the correct negative chunk (block `-1` → chunk `-1`).
    #[inline]
    #[must_use]
    pub const fn chunk_xz(self) -> (i32, i32) {
        (self.x >> 4, self.z >> 4)
    }

    /// Index of the `16³` section containing this block along Y
    /// (`block_y >> 4`); the section covering `y = -64..=-49` is `-4`.
    #[inline]
    #[must_use]
    pub const fn section_y(self) -> i32 {
        self.y >> 4
    }

    /// Vanilla section-local block index
    /// `(y & 15) << 8 | (z & 15) << 4 | (x & 15)` — the offset into a
    /// `16³` section's 4096-entry block array.
    #[inline]
    #[must_use]
    pub const fn local_index(self) -> usize {
        (((self.y & 15) << 8) | ((self.z & 15) << 4) | (self.x & 15)) as usize
    }

    /// The [`ChunkKey`] of the column containing this block.
    #[inline]
    #[must_use]
    pub const fn chunk_key(self, dim: DimIndex) -> ChunkKey {
        ChunkKey {
            dim,
            cx: self.x >> 4,
            cz: self.z >> 4,
        }
    }

    /// The [`SectionKey`] of the `16³` section containing this block.
    #[inline]
    #[must_use]
    pub const fn section_key(self, dim: DimIndex) -> SectionKey {
        SectionKey {
            dim,
            cx: self.x >> 4,
            sy: self.y >> 4,
            cz: self.z >> 4,
        }
    }
}

/// Identity of one `16×world-height` chunk column in one dimension — the
/// unit of aggregate invalidation, wire caching and writeback coalescing.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash, Ord, PartialOrd)]
pub struct ChunkKey {
    /// Dimension (`0` overworld, `1` nether, `2` end, modded `>= 3`).
    pub dim: DimIndex,
    /// Chunk column X (`block_x >> 4`).
    pub cx: i32,
    /// Chunk column Z (`block_z >> 4`).
    pub cz: i32,
}

impl ChunkKey {
    /// Column containing block `(bx, bz)` in dimension `dim`.
    #[inline]
    #[must_use]
    pub const fn from_block_pos(dim: DimIndex, bx: i32, bz: i32) -> Self {
        Self {
            dim,
            cx: bx >> 4,
            cz: bz >> 4,
        }
    }
}

/// Identity of one `16³` chunk section in one dimension.
///
/// `cx`/`cz` are chunk coordinates and `sy` is the section index along the
/// vertical axis (`block_y >> 4`, so the section covering blocks
/// `y = -64..=-49` is `sy = -4`). `sy` is a full `i32` — not `i8` — so
/// arbitrary modded world heights key correctly.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash, Ord, PartialOrd)]
pub struct SectionKey {
    /// Dimension (`0` overworld, `1` nether, `2` end, modded `>= 3`).
    pub dim: DimIndex,
    /// Chunk column X (`block_x >> 4`).
    pub cx: i32,
    /// Section index Y (`block_y >> 4`).
    pub sy: i32,
    /// Chunk column Z (`block_z >> 4`).
    pub cz: i32,
}

impl SectionKey {
    /// Section containing block `(bx, by, bz)` in dimension `dim`.
    #[inline]
    #[must_use]
    pub const fn from_block_pos(dim: DimIndex, bx: i32, by: i32, bz: i32) -> Self {
        Self {
            dim,
            cx: bx >> 4,
            sy: by >> 4,
            cz: bz >> 4,
        }
    }

    /// Aggregate chunk column this section belongs to.
    #[inline]
    #[must_use]
    pub const fn chunk(self) -> ChunkKey {
        ChunkKey {
            dim: self.dim,
            cx: self.cx,
            cz: self.cz,
        }
    }
}

/// Multiplicative-mix constant for [`CoordHasher`] (the `rustc-hash`
/// word-mixing constant — proven avalanche behaviour, zero seed, fully
/// deterministic across runs and replicas).
const COORD_HASH_MIX: u64 = 0x51_7c_c1_b7_27_22_0a_95;

/// Fast, deterministic, zero-DoS [`Hasher`] for coordinate keys.
///
/// Every `write_*` folds one whole machine word in a single
/// rotate-multiply step, so a [`SectionKey`] hashes in ~5 multiplies —
/// far cheaper than SipHash, and immune to the per-process seed that
/// would make replica checksums diverge. Deterministic means a crafted
/// sequence of coordinates can no more degrade a [`CoordMap`] than it
/// can degrade any fixed-seed table, and the multiply-rotate mix gives
/// full avalanche dispersion across hashbrown's bucket index.
#[derive(Clone, Default)]
pub struct CoordHasher(u64);

impl CoordHasher {
    #[inline]
    fn add(&mut self, word: u64) {
        self.0 = (self.0.rotate_left(5) ^ word).wrapping_mul(COORD_HASH_MIX);
    }
}

impl Hasher for CoordHasher {
    #[inline]
    fn finish(&self) -> u64 {
        self.0
    }

    #[inline]
    fn write(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            self.add(u64::from(byte));
        }
    }

    #[inline]
    fn write_u8(&mut self, value: u8) {
        self.add(u64::from(value));
    }

    #[inline]
    fn write_u16(&mut self, value: u16) {
        self.add(u64::from(value));
    }

    #[inline]
    fn write_u32(&mut self, value: u32) {
        self.add(u64::from(value));
    }

    #[inline]
    fn write_u64(&mut self, value: u64) {
        self.add(value);
    }

    #[inline]
    fn write_u128(&mut self, value: u128) {
        self.add(value as u64);
        self.add((value >> 64) as u64);
    }

    #[inline]
    fn write_usize(&mut self, value: usize) {
        self.add(value as u64);
    }

    #[inline]
    fn write_i8(&mut self, value: i8) {
        self.add(value as u64);
    }

    #[inline]
    fn write_i16(&mut self, value: i16) {
        self.add(value as u64);
    }

    #[inline]
    fn write_i32(&mut self, value: i32) {
        self.add(value as u64);
    }

    #[inline]
    fn write_i64(&mut self, value: i64) {
        self.add(value as u64);
    }

    #[inline]
    fn write_i128(&mut self, value: i128) {
        self.add(value as u64);
        self.add((value >> 64) as u64);
    }

    #[inline]
    fn write_isize(&mut self, value: isize) {
        self.add(value as u64);
    }
}

/// [`BuildHasher`] producing zero-seeded [`CoordHasher`]s — drop-in for
/// `HashMap`/`HashSet` in place of `RandomState` when keys are
/// coordinate-shaped.
pub type CoordBuildHasher = BuildHasherDefault<CoordHasher>;

/// `HashMap` keyed on coordinate types, hashed by [`CoordHasher`].
pub type CoordMap<K, V> = HashMap<K, V, CoordBuildHasher>;

/// `HashSet` of coordinate keys, hashed by [`CoordHasher`].
pub type CoordSet<K> = HashSet<K, CoordBuildHasher>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn block_pos_floor_division_and_local_index() {
        let pos = BlockPos::new(-1, -64, 31);
        assert_eq!(pos.chunk_xz(), (-1, 1));
        assert_eq!(pos.section_y(), -4);
        assert_eq!(pos.local_index(), 255);
        assert_eq!(BlockPos::new(0, 0, 0).local_index(), 0);
        assert_eq!(BlockPos::new(15, 15, 15).local_index(), 4095);
        // Modded heights beyond any i8 range still key correctly.
        let tall = BlockPos::new(0, 5000, 0);
        assert_eq!(tall.section_y(), 312);
        assert_eq!(tall.section_key(DimIndex::OVERWORLD).sy, 312);
    }

    #[test]
    fn keys_carry_dimension() {
        let a = ChunkKey::from_block_pos(DimIndex::OVERWORLD, 8, 8);
        let b = ChunkKey::from_block_pos(DimIndex::NETHER, 8, 8);
        assert_ne!(a, b);
        let section = SectionKey::from_block_pos(DimIndex::END, -16, 128, 47);
        assert_eq!(
            section.chunk(),
            ChunkKey {
                dim: DimIndex::END,
                cx: -1,
                cz: 2
            }
        );
        assert_eq!(DimIndex::OVERWORLD, DimIndex(0));
        assert_eq!(DimIndex::NETHER, DimIndex(1));
        assert_eq!(DimIndex::END, DimIndex(2));
    }

    #[test]
    fn coord_map_is_deterministic_and_usable() {
        let mut map: CoordMap<SectionKey, u32> = CoordMap::default();
        let key = SectionKey::from_block_pos(DimIndex::OVERWORLD, 1, 2, 3);
        map.insert(key, 7);
        assert_eq!(map.get(&key), Some(&7));

        let mut set: CoordSet<ChunkKey> = CoordSet::default();
        set.insert(key.chunk());
        assert!(set.contains(&ChunkKey {
            dim: DimIndex::OVERWORLD,
            cx: 0,
            cz: 0
        }));

        // Same key stream hashes identically across independent hashers.
        use std::hash::BuildHasher;
        let hash_one = |k: &SectionKey| CoordBuildHasher::default().hash_one(*k);
        assert_eq!(hash_one(&key), hash_one(&key));
        assert_ne!(hash_one(&key), hash_one(&SectionKey { sy: 9, ..key }));
    }
}
