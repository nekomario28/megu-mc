// SPDX-License-Identifier: LGPL-3.0-or-later
#![forbid(unsafe_code)]
//! `megu_core::section_rev` — [`SharedSectionRevTable`]: the single
//! canonical revision table every block-derived cache reads from.
//!
//! A revision is a per-key monotonically nondecreasing `u64` that starts
//! at `0` ("never touched") and is bumped by every block edit affecting
//! that key. Consumers remember the revision they last computed against;
//! [`SharedSectionRevTable::section_changed_since`] /
//! [`SharedSectionRevTable::chunk_changed_since`] answer staleness in
//! `O(1)` with zero per-subsystem bookkeeping.
//!
//! One table, no renderer or WAL dependency: lighting, meshing, spatial
//! audio, nature ambience, redstone netlist caches, copy-on-write chunk
//! wire caches and the writeback log all subscribe to the *same*
//! monotonically increasing numbers, so no subsystem can ever track a
//! duplicate revision that drifts out of sync.
//!
//! [`SharedSectionRevTable::bump_block`] also bumps the adjacent sections
//! (and their parent chunks) when the edited block sits on a `0`/`15`
//! section boundary, so a slab light leak or a mesh seam can never stale
//! a neighbour's cache. Keys come from [`crate::coords`], so table
//! lookups ride the deterministic [`crate::coords::CoordHasher`] — no
//! per-process seed and no SipHash cost on the mutation hot path.

use crate::coords::{ChunkKey, CoordMap, DimIndex, SectionKey};

/// The single shared revision table every block-derived cache reads from.
#[derive(Default, Debug)]
pub struct SharedSectionRevTable {
    sections: CoordMap<SectionKey, u64>,
    chunks: CoordMap<ChunkKey, u64>,
    block_edits: u64,
}

impl SharedSectionRevTable {
    /// Empty table; every key reads revision `0` until first bumped.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Current revision of `key`, `0` if the section was never modified.
    #[inline]
    #[must_use]
    pub fn section_rev(&self, key: SectionKey) -> u64 {
        self.sections.get(&key).copied().unwrap_or(0)
    }

    /// Current aggregate revision of `key`, `0` if the chunk column was
    /// never modified. Bumped once per block edit in the column (plus
    /// border-neighbour bumps that cross into the column), so it is a
    /// strict upper-bound invalidator for anything chunk-scoped.
    #[inline]
    #[must_use]
    pub fn chunk_rev(&self, key: ChunkKey) -> u64 {
        self.chunks.get(&key).copied().unwrap_or(0)
    }

    /// Record one block edit at `(bx, by, bz)` in dimension `dim`.
    ///
    /// Bumps the block's own section revision and its parent chunk
    /// revision, and — when the block lies on a section boundary
    /// (`coord & 15 == 0` or `== 15` on any axis) — every adjacent
    /// section across each touched boundary *and* those sections' parent
    /// chunks. A corner block therefore touches all 8 neighbouring
    /// sections, exactly matching how lighting, meshing and acoustic
    /// propagation actually leak across section lines.
    ///
    /// Returns the primary `(section_rev, chunk_rev)` pair after the
    /// bump.
    pub fn bump_block(&mut self, dim: DimIndex, bx: i32, by: i32, bz: i32) -> (u64, u64) {
        let primary = self.bump_block_local(dim, bx, by, bz);
        let skey = SectionKey::from_block_pos(dim, bx, by, bz);
        for &dx in border_offsets(bx) {
            for &dy in border_offsets(by) {
                for &dz in border_offsets(bz) {
                    if dx == 0 && dy == 0 && dz == 0 {
                        continue;
                    }
                    let (Some(cx), Some(sy), Some(cz)) = (
                        skey.cx.checked_add(dx),
                        skey.sy.checked_add(dy),
                        skey.cz.checked_add(dz),
                    ) else {
                        continue;
                    };
                    let nkey = SectionKey {
                        dim: skey.dim,
                        cx,
                        sy,
                        cz,
                    };
                    Self::bump(&mut self.sections, nkey);
                    Self::bump(&mut self.chunks, nkey.chunk());
                }
            }
        }
        primary
    }

    /// Like [`Self::bump_block`] but bumps only the block's own section
    /// and its parent chunk — no border-neighbour propagation. For
    /// consumers whose derived state provably cannot leak across a
    /// section line (chunk-column dirty tracking, wire-cache keys).
    pub fn bump_block_local(&mut self, dim: DimIndex, bx: i32, by: i32, bz: i32) -> (u64, u64) {
        let skey = SectionKey::from_block_pos(dim, bx, by, bz);
        self.block_edits = self.block_edits.saturating_add(1);
        (
            Self::bump(&mut self.sections, skey),
            Self::bump(&mut self.chunks, skey.chunk()),
        )
    }

    /// `true` if `key`'s section revision differs from `seen` — i.e. the
    /// section changed since the caller last looked. `seen = 0` answers
    /// "was this section ever modified".
    #[inline]
    #[must_use]
    pub fn section_changed_since(&self, key: SectionKey, seen: u64) -> bool {
        self.section_rev(key) != seen
    }

    /// `true` if `key`'s chunk revision differs from `seen`.
    #[inline]
    #[must_use]
    pub fn chunk_changed_since(&self, key: ChunkKey, seen: u64) -> bool {
        self.chunk_rev(key) != seen
    }

    /// Forget one section (e.g. its consumer evicted its cache entry).
    /// Returns the revision that was dropped, if the section was tracked.
    pub fn remove_section(&mut self, key: SectionKey) -> Option<u64> {
        self.sections.remove(&key)
    }

    /// Forget a whole chunk column on unload: drops the column's
    /// aggregate revision and every tracked section inside it. Returns
    /// the chunk revision that was dropped, if tracked.
    pub fn evict_chunk(&mut self, key: ChunkKey) -> Option<u64> {
        self.sections
            .retain(|s, _| s.dim != key.dim || s.cx != key.cx || s.cz != key.cz);
        self.chunks.remove(&key)
    }

    /// Number of sections with at least one recorded edit.
    #[inline]
    #[must_use]
    pub fn tracked_sections(&self) -> usize {
        self.sections.len()
    }

    /// Number of chunk columns with at least one recorded edit.
    #[inline]
    #[must_use]
    pub fn tracked_chunks(&self) -> usize {
        self.chunks.len()
    }

    /// Total primary block edits recorded (border-neighbour bumps are
    /// derived, not counted).
    #[inline]
    #[must_use]
    pub fn total_block_edits(&self) -> u64 {
        self.block_edits
    }

    /// Drop every tracked revision (full world unload).
    pub fn clear(&mut self) {
        self.sections.clear();
        self.chunks.clear();
    }

    fn bump<K: Eq + std::hash::Hash>(table: &mut CoordMap<K, u64>, key: K) -> u64 {
        let entry = table.entry(key).or_insert(0);
        *entry = entry.saturating_add(1);
        *entry
    }
}

/// Section-internal neighbour offsets along one axis: `-1` when the block
/// sits on the low boundary, `+1` on the high boundary, nothing interior.
#[inline]
fn border_offsets(coord: i32) -> &'static [i32] {
    match coord & 15 {
        0 => &[-1, 0],
        15 => &[0, 1],
        _ => &[0],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const OW: DimIndex = DimIndex::OVERWORLD;

    #[test]
    fn interior_edit_bumps_only_own_section_and_chunk() {
        let mut t = SharedSectionRevTable::new();
        let (srev, crev) = t.bump_block(OW, 5, 70, -9);
        assert_eq!((srev, crev), (1, 1));
        let skey = SectionKey::from_block_pos(OW, 5, 70, -9);
        assert_eq!(t.section_rev(skey), 1);
        assert_eq!(t.chunk_rev(skey.chunk()), 1);
        assert_eq!(t.tracked_sections(), 1);
        assert_eq!(t.tracked_chunks(), 1);
        assert_eq!(t.total_block_edits(), 1);
    }

    #[test]
    fn boundary_edit_bumps_neighbour_sections_and_chunks() {
        let mut t = SharedSectionRevTable::new();
        // Block on the high-X section edge leaks into the +X neighbour,
        // which lives in the next chunk column.
        t.bump_block(OW, 15, 8, 8);
        let own = SectionKey::from_block_pos(OW, 15, 8, 8);
        let east = SectionKey {
            cx: own.cx + 1,
            ..own
        };
        assert_eq!(t.section_rev(own), 1);
        assert_eq!(t.section_rev(east), 1);
        assert_eq!(t.chunk_rev(own.chunk()), 1);
        assert_eq!(t.chunk_rev(east.chunk()), 1);

        // Interior block on the same sections adds nothing new.
        t.bump_block(OW, 16, 8, 8);
        assert_eq!(t.section_rev(east), 2);
        assert_eq!(t.tracked_sections(), 2);
        assert_eq!(t.tracked_chunks(), 2);
    }

    #[test]
    fn corner_edit_touches_all_eight_neighbours() {
        let mut t = SharedSectionRevTable::new();
        // (0,0,0) is the low corner of its section in all three axes.
        t.bump_block(OW, 0, 0, 0);
        let own = SectionKey::from_block_pos(OW, 0, 0, 0);
        for dx in -1..=0 {
            for dy in -1..=0 {
                for dz in -1..=0 {
                    let key = SectionKey {
                        dim: OW,
                        cx: own.cx + dx,
                        sy: own.sy + dy,
                        cz: own.cz + dz,
                    };
                    assert_eq!(t.section_rev(key), 1, "section {key:?}");
                }
            }
        }
        assert_eq!(t.tracked_sections(), 8);
    }

    #[test]
    fn negative_coords_hit_negative_sections() {
        let mut t = SharedSectionRevTable::new();
        t.bump_block(DimIndex::NETHER, -1, -1, -1);
        let key = SectionKey::from_block_pos(DimIndex::NETHER, -1, -1, -1);
        assert_eq!(
            key,
            SectionKey {
                dim: DimIndex::NETHER,
                cx: -1,
                sy: -1,
                cz: -1
            }
        );
        assert_eq!(t.section_rev(key), 1);
    }

    #[test]
    fn evict_chunk_drops_column_and_its_sections() {
        let mut t = SharedSectionRevTable::new();
        t.bump_block(OW, 3, 0, 3);
        t.bump_block(OW, 3, 32, 3); // same column, different section
        t.bump_block(OW, 100, 5, 100); // other column
        let dropped = ChunkKey::from_block_pos(OW, 3, 3);
        let kept = ChunkKey::from_block_pos(OW, 100, 100);
        // Each edit bumps the chunk once directly and once more via the
        // in-column border-neighbour section, so the column reads 4.
        assert_eq!(t.evict_chunk(dropped), Some(4));
        assert_eq!(t.tracked_chunks(), 1);
        assert_eq!(t.chunk_rev(kept), 1);
        // Only the evicted column's sections are gone.
        assert_eq!(t.tracked_sections(), 1);
        assert_eq!(t.evict_chunk(dropped), None);
    }

    #[test]
    fn changed_since_tracks_seen_revision() {
        let mut t = SharedSectionRevTable::new();
        let key = SectionKey::from_block_pos(OW, 8, 8, 8);
        assert!(!t.section_changed_since(key, 0));
        let seen = t.bump_block(OW, 8, 8, 8).0;
        assert!(!t.section_changed_since(key, seen));
        t.bump_block_local(OW, 9, 9, 9);
        assert!(t.section_changed_since(key, seen));
        assert!(t.chunk_changed_since(key.chunk(), 0));
    }
}
