// SPDX-License-Identifier: LGPL-3.0-or-later
use crate::Error;

// The server's lighting source is imported verbatim and extended only with
// accessors for the facts it keeps private.
#[allow(dead_code, clippy::all)]
pub(crate) mod owner {
    include!("../../src/lighting.rs");

    use super::LightFact;
    use crate::Error;

    impl FaceOcclusion {
        pub(crate) fn from_quadrants(quadrants: [u8; 6]) -> Self {
            Self(quadrants)
        }

        pub(crate) fn quadrants(self) -> [u8; 6] {
            self.0
        }
    }

    impl IncrementalLighting {
        pub(crate) fn is_loaded(&self, chunk: Chunk) -> bool {
            self.chunks.contains_key(&chunk)
        }

        pub(crate) fn window(&self) -> (i32, i32) {
            (self.min_y, self.height)
        }

        pub(crate) fn has_skylight(&self) -> bool {
            self.has_skylight
        }
    }

    impl IncrementalLighting {
        /// `load_chunk` fed directly from the packed cell facts, skipping the
        /// intermediate `(Position, LightBlock)` vector the iterator form
        /// requires. Cell order and per-fact validation match `load_chunk`.
        pub(crate) fn load_chunk_facts(
            &mut self,
            cx: i32,
            cz: i32,
            cells: &[LightFact],
        ) -> Result<(), Error> {
            let mut blocks = vec![LightBlock::default(); self.height as usize * 256];
            for (block, fact) in blocks.iter_mut().zip(cells) {
                *block = fact.block()?;
            }
            if !self.chunks.contains_key(&(cx, cz)) {
                self.insert_chunks_incremental(vec![((cx, cz), blocks)]);
            } else {
                self.chunks.insert(
                    (cx, cz),
                    LightingChunk::new(self.min_y, self.height, blocks),
                );
                self.rebuild();
            }
            Ok(())
        }

        /// `sections` for one index without materializing every section:
        /// index zero is the below guard, `count - 1` the above guard, and
        /// payload sections pack nibbles straight out of `light(kind)`.
        pub(crate) fn section_packed(
            &self,
            chunk: Chunk,
            kind: LightKind,
            index: usize,
        ) -> [u8; 2048] {
            let mut out = [0u8; 2048];
            let count = (self.height / 16 + 2) as usize;
            if index >= count {
                return out;
            }
            if let Some(chunk) = self.chunks.get(&chunk) {
                if (1..count - 1).contains(&index) {
                    out = chunk.pack_section(kind, index - 1);
                } else if index + 1 == count && self.has_skylight && kind == LightKind::Sky {
                    out.fill(0xff);
                }
            }
            out
        }
    }

    /// `IncrementalLighting` generalized past its baked-in `u64` section-mask
    /// cap. The algorithm, per-chunk storage, snapshot log, and 14-step patch
    /// semantics are identical to the owner's; only the `edit_batch` change
    /// masks become `BitSet`-shaped `Vec<u64>` word lists, so a window of up
    /// to 128 payload sections (130 with the two guard sections) reports every
    /// changed section instead of being unrepresentable in one word.
    #[derive(Clone, Debug)]
    pub(crate) struct TallLighting {
        min_y: i32,
        height: i32,
        has_skylight: bool,
        chunks: LightingHashMap<Chunk, LightingChunk>,
        stamp: u32,
        snap: SnapLog,
        scratch: Scratch,
        batch_scratch: BatchScratch,
    }

    /// Per-call edit-batch temporaries retained across `edit_batch` calls so
    /// streaming edits do not reallocate the change lists every tick. Bounded
    /// by [`BATCH_SCRATCH_CAP`]: an oversized batch releases the buffers.
    #[derive(Clone, Debug, Default)]
    struct BatchScratch {
        block_changes: Vec<Position>,
        sky_changes: Vec<Position>,
        columns: Vec<(i32, i32)>,
    }

    /// `ChunkLightUpdate` whose masks are `BitSet.toLongArray`-shaped word
    /// lists: bit `i` is bit `i % 64` of word `i / 64`, and trailing all-zero
    /// words never exist because the list only grows to the highest set bit.
    #[derive(Debug, Default, Eq, PartialEq)]
    pub(crate) struct TallChunkLightUpdate {
        pub(crate) chunk: Chunk,
        pub(crate) sky_mask: Vec<u64>,
        pub(crate) empty_sky_mask: Vec<u64>,
        pub(crate) block_mask: Vec<u64>,
        pub(crate) empty_block_mask: Vec<u64>,
        pub(crate) sky_sections: Vec<(usize, [u8; 2048])>,
        pub(crate) block_sections: Vec<(usize, [u8; 2048])>,
    }

    #[derive(Debug, Default, Eq, PartialEq)]
    pub(crate) struct TallLightUpdate {
        pub(crate) chunks: Vec<TallChunkLightUpdate>,
        /// Number of cells darkened/considered for relighting, across both layers.
        pub(crate) invalidated_cells: usize,
    }

    /// 128 payload sections plus the below/above guard sections.
    pub(crate) const TALL_SECTION_LIMIT: i32 = 128 + 2;

    /// Retained-capacity ceiling (cells) for [`BatchScratch`]: a batch larger
    /// than this releases its buffers after the update is built.
    const BATCH_SCRATCH_CAP: usize = 1 << 15;

    /// `BitSet.set(bit)`: bit `bit % 64` of word `bit / 64`, growing the list.
    pub(crate) fn set_mask_bit(words: &mut Vec<u64>, bit: usize) {
        let word = bit / 64;
        if words.len() <= word {
            words.resize(word + 1, 0);
        }
        words[word] |= 1 << (bit % 64);
    }

    /// `BitSet.toLongArray`: drop trailing all-zero words from the tail.
    pub(crate) fn trim_mask_words(mut words: Vec<u64>) -> Vec<u64> {
        let kept = words.iter().rposition(|w| *w != 0).map_or(0, |i| i + 1);
        words.truncate(kept);
        words
    }

    /// The multi-word `mask != 0`.
    pub(crate) fn mask_any(words: &[u64]) -> bool {
        words.iter().any(|word| *word != 0)
    }

    impl TallLighting {
        /// Mirrors `IncrementalLighting::from_chunks` under the tall cap.
        pub(crate) fn from_chunks(
            min_y: i32,
            height: i32,
            has_skylight: bool,
            chunks: impl IntoIterator<Item = (Chunk, Vec<LightBlock>)>,
        ) -> Self {
            assert!(min_y % 16 == 0 && height > 0 && height % 16 == 0);
            assert!(
                height / 16 + 2 <= TALL_SECTION_LIMIT,
                "section masks must fit the tall-dimension cap"
            );
            let mut engine = Self {
                min_y,
                height,
                has_skylight,
                chunks: chunks
                    .into_iter()
                    .map(|(key, blocks)| (key, LightingChunk::new(min_y, height, blocks)))
                    .collect(),
                stamp: 0,
                snap: SnapLog::default(),
                scratch: Scratch::default(),
                batch_scratch: BatchScratch::default(),
            };
            engine.rebuild();
            engine
        }

        pub(crate) fn is_loaded(&self, chunk: Chunk) -> bool {
            self.chunks.contains_key(&chunk)
        }

        pub(crate) fn window(&self) -> (i32, i32) {
            (self.min_y, self.height)
        }

        pub(crate) fn has_skylight(&self) -> bool {
            self.has_skylight
        }

        fn flat(min_y: i32, height: i32, p: Position) -> Option<usize> {
            let dy = p.y - min_y;
            (0..height)
                .contains(&dy)
                .then(|| (dy as usize) << 8 | ((p.z & 15) as usize) << 4 | (p.x & 15) as usize)
        }

        fn index(&self, p: Position) -> Option<usize> {
            Self::flat(self.min_y, self.height, p)
        }

        fn cell(&self, p: Position) -> Option<(&LightingChunk, usize)> {
            Some((self.chunks.get(&p.chunk())?, self.index(p)?))
        }

        fn source(&self, kind: LightKind, p: Position) -> u8 {
            self.cell(p).map_or(0, |(chunk, index)| match kind {
                LightKind::Sky => chunk.direct_sky_at(index),
                LightKind::Block => chunk.states[index] >> 4,
            })
        }

        pub(crate) fn level(&self, kind: LightKind, p: Position) -> u8 {
            self.cell(p)
                .map_or(0, |(chunk, index)| chunk.light_at(kind, index))
        }

        fn set_level(&mut self, kind: LightKind, p: Position, level: u8) {
            if let Some(index) = self.index(p) {
                let key = p.chunk();
                if let Some(chunk) = self.chunks.get_mut(&key) {
                    chunk.store(kind, index, level, key, &mut self.snap);
                }
            }
        }

        /// Marks a loaded cell as part of the patch; false if absent or already marked.
        fn claim(&self, p: Position, patch_set: &mut LightingHashSet<Position>) -> bool {
            if self.index(p).is_none() || !self.chunks.contains_key(&p.chunk()) {
                return false;
            }
            patch_set.insert(p)
        }

        fn propagate(
            &mut self,
            kind: LightKind,
            queue: &mut LightingVecDeque<(Position, u8)>,
            patch: Option<&LightingHashSet<Position>>,
        ) {
            let Self {
                min_y,
                height,
                chunks,
                snap,
                ..
            } = self;
            while let Some((p, level)) = queue.pop_front() {
                if level <= 1 {
                    continue;
                }
                let Some(index) = Self::flat(*min_y, *height, p) else {
                    continue;
                };
                let Some(chunk) = chunks.get(&p.chunk()) else {
                    continue;
                };
                if chunk.light_at(kind, index) != level {
                    continue;
                }
                let from = chunk.light_block(index);
                for (face, next) in p.adjacent() {
                    let Some(index) = Self::flat(*min_y, *height, next) else {
                        continue;
                    };
                    if patch.is_some_and(|set| !set.contains(&next)) {
                        continue;
                    }
                    let key = next.chunk();
                    let Some(chunk) = chunks.get_mut(&key) else {
                        continue;
                    };
                    let candidate = from.transmitted(face, chunk.light_block(index), level);
                    if candidate > chunk.light_at(kind, index) {
                        chunk.store(kind, index, candidate, key, snap);
                        queue.push_back((next, candidate));
                    }
                }
            }
        }

        fn rebuild(&mut self) {
            let mut block = LightingVecDeque::new();
            for (&(cx, cz), chunk) in &mut self.chunks {
                chunk.clear_light();
                if self.has_skylight {
                    for i in 0..256 {
                        Self::refresh_column(self.min_y, self.height, chunk, i, &mut |_| {});
                    }
                    for i in 0..chunk.light_nibbles.len() {
                        chunk.set_light(LightKind::Sky, i, chunk.direct_sky_at(i));
                    }
                }
                for i in 0..chunk.states.len() {
                    let emission = chunk.states[i] >> 4;
                    if emission > 0 {
                        chunk.set_light(LightKind::Block, i, emission);
                        block.push_back((
                            Position::new(
                                cx * 16 + (i & 15) as i32,
                                self.min_y + (i >> 8) as i32,
                                cz * 16 + ((i >> 4) & 15) as i32,
                            ),
                            emission,
                        ));
                    }
                }
            }

            self.propagate(LightKind::Block, &mut block, None);
            if !self.has_skylight {
                return;
            }
            // Direct light already handles vertical propagation. Seed the spread
            // where a column can illuminate a less-lit horizontal neighbor.
            let mut sky = LightingVecDeque::new();
            for (&(cx, cz), chunk) in &self.chunks {
                for i in 0..256 {
                    let x = cx * 16 + (i & 15) as i32;
                    let z = cz * 16 + (i >> 4) as i32;
                    for (dx, dz) in [(-1, 0), (1, 0), (0, -1), (0, 1)] {
                        let nx = x + dx;
                        let nz = z + dz;
                        let Some(other) = self.chunks.get(&(nx >> 4, nz >> 4)) else {
                            continue;
                        };
                        let ni = ((nz & 15) << 4 | (nx & 15)) as usize;
                        for y in self.min_y..=other.sky_top[ni] {
                            let offset = ((y - self.min_y) as usize) << 8;
                            let from = chunk.direct_sky_at(offset | i);
                            if from > other.direct_sky_at(offset | ni) + 1 {
                                sky.push_back((Position::new(x, y, z), from));
                            }
                        }
                    }
                }
            }
            self.propagate(LightKind::Sky, &mut sky, None);
        }

        /// Computes direct skylight independently of the lateral light field.
        fn refresh_column(
            min_y: i32,
            height: i32,
            chunk: &mut LightingChunk,
            column: usize,
            changed: &mut impl FnMut(i32),
        ) {
            let mut level = 15u8;
            let mut above = FaceOcclusion::EMPTY;
            let mut top = min_y - 1;
            for dy in (0..height).rev() {
                let i = ((dy as usize) << 8) | column;
                if level > 0 {
                    let block = chunk.light_block(i);
                    level = if above.seals(Face::Down, block.occlusion) {
                        0
                    } else {
                        level.saturating_sub(block.opacity)
                    };
                    above = block.occlusion;
                }
                if level < 15 {
                    top = top.max(min_y + dy);
                }
                if chunk.direct_sky_at(i) != level {
                    set_packed_nibbles(&mut chunk.direct_sky, i, level);
                    changed(min_y + dy);
                }
            }
            chunk.sky_top[column] = top;
        }

        /// Replaces one complete chunk.
        pub(crate) fn load_chunk(
            &mut self,
            cx: i32,
            cz: i32,
            cells: impl IntoIterator<Item = (Position, LightBlock)>,
        ) {
            let mut blocks = vec![LightBlock::default(); self.height as usize * 256];
            for (p, block) in cells {
                if p.chunk() == (cx, cz) {
                    if let Some(i) = self.index(p) {
                        blocks[i] = block;
                    }
                }
            }
            self.chunks.insert(
                (cx, cz),
                LightingChunk::new(self.min_y, self.height, blocks),
            );
            self.rebuild();
        }

        /// `load_chunk` fed directly from the packed cell facts, skipping the
        /// intermediate `(Position, LightBlock)` vector the iterator form
        /// requires. Cell order and per-fact validation match `load_chunk`.
        pub(crate) fn load_chunk_facts(
            &mut self,
            cx: i32,
            cz: i32,
            cells: &[LightFact],
        ) -> Result<(), Error> {
            let mut blocks = vec![LightBlock::default(); self.height as usize * 256];
            for (block, fact) in blocks.iter_mut().zip(cells) {
                *block = fact.block()?;
            }
            self.chunks.insert(
                (cx, cz),
                LightingChunk::new(self.min_y, self.height, blocks),
            );
            self.rebuild();
            Ok(())
        }

        /// `sections` for one index without materializing every section:
        /// index zero is the below guard, `count - 1` the above guard, and
        /// payload sections pack nibbles straight out of `light(kind)`.
        pub(crate) fn section_packed(
            &self,
            chunk: Chunk,
            kind: LightKind,
            index: usize,
        ) -> [u8; 2048] {
            let mut out = [0u8; 2048];
            let count = (self.height / 16 + 2) as usize;
            if index >= count {
                return out;
            }
            if let Some(chunk) = self.chunks.get(&chunk) {
                if (1..count - 1).contains(&index) {
                    out = chunk.pack_section(kind, index - 1);
                } else if index + 1 == count && self.has_skylight && kind == LightKind::Sky {
                    out.fill(0xff);
                }
            }
            out
        }

        /// The two BFS passes limit invalidation to 14 Manhattan steps per changed
        /// light source, because a level-15 source cannot illuminate a fifteenth step.
        /// Cells outside that region preserve their previously settled light.
        fn relight_patch(&mut self, kind: LightKind, changed: &[Position]) -> usize {
            if changed.is_empty() {
                return 0;
            }
            let mut scratch = std::mem::take(&mut self.scratch);
            let Scratch {
                patch,
                patch_set,
                frontier,
                queue,
                ..
            } = &mut scratch;
            patch.clear();
            patch_set.clear();
            frontier.clear();
            queue.clear();
            for &p in changed {
                if self.claim(p, patch_set) {
                    patch.push(p);
                    frontier.push_back((p, 0u8));
                }
            }
            while let Some((p, distance)) = frontier.pop_front() {
                if distance == 14 {
                    continue;
                }
                for (_, n) in p.adjacent() {
                    if self.claim(n, patch_set) {
                        patch.push(n);
                        frontier.push_back((n, distance + 1));
                    }
                }
            }
            // Darkening first is essential when an emitter is removed or opacity
            // rises. The surviving external boundary remains valid at distance 15.
            for &p in patch.iter() {
                self.set_level(kind, p, 0);
            }
            for &p in patch.iter() {
                let mut level = self.source(kind, p);
                let (chunk, index) = self.cell(p).expect("patch lies in the light window");
                let target = chunk.light_block(index);
                for (face, neighbor) in p.adjacent() {
                    if !patch_set.contains(&neighbor) {
                        if let Some((chunk, index)) = self.cell(neighbor) {
                            let incoming = chunk.light_at(kind, index);
                            level = level.max(chunk.light_block(index).transmitted(
                                face.opposite(),
                                target,
                                incoming,
                            ));
                        }
                    }
                }
                if level > 0 {
                    self.set_level(kind, p, level);
                    queue.push_back((p, level));
                }
            }
            self.propagate(kind, queue, Some(patch_set));
            let len = patch.len();
            self.scratch = scratch;
            len
        }

        /// Replays an edit batch against a complete pre-edit window. Each affected
        /// light patch is settled once, even when several blocks change in a tick.
        /// Identical to `IncrementalLighting::edit_batch`, except the section
        /// change masks are word lists so bits past 63 stay representable.
        pub(crate) fn edit_batch(
            &mut self,
            edits: impl IntoIterator<Item = (Position, LightBlock)>,
            targets: impl IntoIterator<Item = Chunk>,
        ) -> TallLightUpdate {
            if self.snap.batch == u32::MAX {
                for chunk in self.chunks.values_mut() {
                    chunk.touched.0.fill(0);
                }
                self.snap.batch = 0;
            }
            self.snap.batch += 1;
            self.snap.entries.clear();
            self.snap.arena.clear();
            self.snap.active = true;
            let mut scratch = std::mem::take(&mut self.batch_scratch);
            scratch.block_changes.clear();
            scratch.sky_changes.clear();
            scratch.columns.clear();
            for (p, block) in edits {
                let Some(i) = self.index(p) else { continue };
                let Some(chunk) = self.chunks.get_mut(&p.chunk()) else {
                    continue;
                };
                let old = chunk.set_block(i, block);
                if old == block {
                    continue;
                }
                scratch.block_changes.push(p);
                if self.has_skylight
                    && (old.opacity != block.opacity || old.occlusion != block.occlusion)
                {
                    scratch.columns.push((p.x, p.z));
                    // The lateral path can change even below a roof with no direct sky.
                    scratch.sky_changes.push(p);
                }
            }
            scratch.columns.sort_unstable();
            scratch.columns.dedup();
            for &(x, z) in &scratch.columns {
                let Some(chunk) = self.chunks.get_mut(&(x >> 4, z >> 4)) else {
                    continue;
                };
                let i = ((z & 15) << 4 | (x & 15)) as usize;
                Self::refresh_column(self.min_y, self.height, chunk, i, &mut |y| {
                    scratch.sky_changes.push(Position::new(x, y, z));
                });
            }
            let work = self.relight_patch(LightKind::Sky, &scratch.sky_changes)
                + self.relight_patch(LightKind::Block, &scratch.block_changes);
            self.batch_scratch = scratch;
            self.snap.active = false;
            self.snap.entries.sort_unstable();
            let mut updates = Vec::new();
            for chunk in targets {
                let Some(light) = self.chunks.get(&chunk) else {
                    continue;
                };
                let mut update = TallChunkLightUpdate {
                    chunk,
                    ..Default::default()
                };
                let start = self.snap.entries.partition_point(|e| e.0 < chunk);
                for &(key, kind, section, offset) in &self.snap.entries[start..] {
                    if key != chunk {
                        break;
                    }
                    let kind = [LightKind::Sky, LightKind::Block][kind];
                    let packed = light.pack_section(kind, section);
                    if self.snap.arena[offset..offset + 2048] == packed {
                        continue;
                    }
                    let bit = section + 1;
                    let (mask, empty, sections) = match kind {
                        LightKind::Sky => (
                            &mut update.sky_mask,
                            &mut update.empty_sky_mask,
                            &mut update.sky_sections,
                        ),
                        LightKind::Block => (
                            &mut update.block_mask,
                            &mut update.empty_block_mask,
                            &mut update.block_sections,
                        ),
                    };
                    if packed.iter().all(|v| *v == 0) {
                        set_mask_bit(empty, bit);
                    } else {
                        set_mask_bit(mask, bit);
                        sections.push((bit, packed));
                    }
                }
                if mask_any(&update.sky_mask)
                    || mask_any(&update.empty_sky_mask)
                    || mask_any(&update.block_mask)
                    || mask_any(&update.empty_block_mask)
                {
                    updates.push(update);
                }
            }
            // Chunk keys are unique across `targets`, so the unstable sort
            // keeps the deterministic order without allocating scratch.
            updates.sort_unstable_by_key(|update| update.chunk);
            if self.batch_scratch.block_changes.capacity() > BATCH_SCRATCH_CAP
                || self.batch_scratch.sky_changes.capacity() > BATCH_SCRATCH_CAP
                || self.batch_scratch.columns.capacity() > BATCH_SCRATCH_CAP
            {
                self.batch_scratch = BatchScratch::default();
            }
            TallLightUpdate {
                chunks: updates,
                invalidated_cells: work,
            }
        }

        /// Packed 16x16x16 sections, including one guard section below and above
        /// the dimension. The section at index zero begins at `min_y - 16`.
        pub(crate) fn sections(&self, chunk: Chunk, kind: LightKind) -> Vec<[u8; 2048]> {
            let count = (self.height / 16 + 2) as usize;
            let mut out = vec![[0u8; 2048]; count];
            if let Some(chunk) = self.chunks.get(&chunk) {
                for (section, array) in out[1..count - 1].iter_mut().enumerate() {
                    *array = chunk.pack_section(kind, section);
                }
                if self.has_skylight && kind == LightKind::Sky {
                    out[count - 1].fill(0xff);
                }
            }
            out
        }
    }
}

pub use owner::{ChunkLightUpdate, LightKind, LightUpdate};

/// Bytes in one packed 16x16x16 nibble array.
pub const LIGHT_SECTION_BYTES: usize = 2048;

/// Face order is West, East, North, South, Down, Up. Each mask holds the
/// four covered half-block quadrants of that face.
pub fn face_occlusion_from_octants(octants: u8) -> [u8; 6] {
    owner::FaceOcclusion::from_octants(octants).quadrants()
}

/// Light facts for one block cell.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LightFact {
    pub opacity: u8,
    pub emission: u8,
    pub occlusion: [u8; 6],
}

impl LightFact {
    fn block(self) -> Result<owner::LightBlock, Error> {
        if self.opacity > 15 || self.emission > 15 || self.occlusion.iter().any(|&q| q > 0xf) {
            return Err(Error::InvalidArgument);
        }
        Ok(owner::LightBlock::with_occlusion(
            self.opacity,
            self.emission,
            owner::FaceOcclusion::from_quadrants(self.occlusion),
        ))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LightEdit {
    pub x: i32,
    pub y: i32,
    pub z: i32,
    pub fact: LightFact,
}

/// `ChunkLightUpdate` with protocol `BitSet`-shaped `Vec<u64>` masks: bit `i`
/// is bit `i % 64` of word `i / 64` and trailing all-zero words are trimmed
/// like `BitSet.toLongArray`, so changed sections past index 63 of a tall
/// window stay representable. On a <=62-payload-section window word zero of
/// each mask equals the `u64` field `ChunkLightUpdate` would have reported.
#[derive(Debug, Default, Eq, PartialEq)]
pub struct ChunkLightUpdateMultiWord {
    pub chunk: (i32, i32),
    pub sky_mask: Vec<u64>,
    pub empty_sky_mask: Vec<u64>,
    pub block_mask: Vec<u64>,
    pub empty_block_mask: Vec<u64>,
    pub sky_sections: Vec<(usize, [u8; LIGHT_SECTION_BYTES])>,
    pub block_sections: Vec<(usize, [u8; LIGHT_SECTION_BYTES])>,
}

/// `LightUpdate` carrying multi-word chunk masks, produced by
/// [`LightEngine::apply_edits_multi_word`].
#[derive(Debug, Default, Eq, PartialEq)]
pub struct LightUpdateMultiWord {
    pub chunks: Vec<ChunkLightUpdateMultiWord>,
    /// Number of cells darkened/considered for relighting, across both layers.
    pub invalidated_cells: usize,
}

impl From<LightUpdate> for LightUpdateMultiWord {
    fn from(update: LightUpdate) -> Self {
        // A zero u64 mask is the empty `BitSet` (`[]` on the wire); a nonzero
        // one is the single word, both already trailing-zero-trimmed.
        let word = |mask: u64| {
            if mask == 0 {
                Vec::new()
            } else {
                vec![mask]
            }
        };
        Self {
            chunks: update
                .chunks
                .into_iter()
                .map(|chunk| ChunkLightUpdateMultiWord {
                    chunk: chunk.chunk,
                    sky_mask: word(chunk.sky_mask),
                    empty_sky_mask: word(chunk.empty_sky_mask),
                    block_mask: word(chunk.block_mask),
                    empty_block_mask: word(chunk.empty_block_mask),
                    sky_sections: chunk.sky_sections,
                    block_sections: chunk.block_sections,
                })
                .collect(),
            invalidated_cells: update.invalidated_cells,
        }
    }
}

impl From<owner::TallLightUpdate> for LightUpdateMultiWord {
    fn from(update: owner::TallLightUpdate) -> Self {
        Self {
            chunks: update
                .chunks
                .into_iter()
                .map(|chunk| ChunkLightUpdateMultiWord {
                    chunk: chunk.chunk,
                    sky_mask: owner::trim_mask_words(chunk.sky_mask),
                    empty_sky_mask: owner::trim_mask_words(chunk.empty_sky_mask),
                    block_mask: owner::trim_mask_words(chunk.block_mask),
                    empty_block_mask: owner::trim_mask_words(chunk.empty_block_mask),
                    sky_sections: chunk.sky_sections,
                    block_sections: chunk.block_sections,
                })
                .collect(),
            invalidated_cells: update.invalidated_cells,
        }
    }
}

/// One loaded column's packed light sections split the way a light-update
/// packet encodes them: `sky_mask`/`block_mask` cover sections with a nonzero
/// array, `empty_sky_mask`/`empty_block_mask` cover all-zero sections, and
/// `sky_arrays`/`block_arrays` carry the nonempty arrays in ascending section
/// order (index zero begins one section below the window, so the top guard
/// section sits in `sky_mask` whenever the dimension has skylight). All masks
/// are `BitSet.toLongArray`-shaped: bit `i % 64` of word `i / 64`, trailing
/// all-zero words trimmed. A window without skylight leaves every sky field
/// empty, matching the server's packet layout.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LightMasksMultiWord {
    pub sky_mask: Vec<u64>,
    pub block_mask: Vec<u64>,
    pub empty_sky_mask: Vec<u64>,
    pub empty_block_mask: Vec<u64>,
    pub sky_arrays: Vec<[u8; LIGHT_SECTION_BYTES]>,
    pub block_arrays: Vec<[u8; LIGHT_SECTION_BYTES]>,
}

/// The backing incremental engine: the server's `u64`-mask engine for windows
/// that fit it, or the tall multi-word engine for >62 payload sections.
enum EngineInner {
    Standard(owner::IncrementalLighting),
    Tall(owner::TallLighting),
}

/// Reusable per-call buffers for [`LightEngine::apply_edits`]: the validated
/// `(Position, LightBlock)` change list and the sorted neighbor-target set.
/// Retained across calls so steady-state edit batches do not reallocate;
/// [`EDIT_SCRATCH_CAP`] bounds the retained footprint.
#[derive(Default)]
struct EditScratch {
    changes: Vec<(owner::Position, owner::LightBlock)>,
    targets: Vec<(i32, i32)>,
}

/// Retained-capacity ceiling (cells) for [`EditScratch`]: a batch larger than
/// this releases its buffers after the update is built.
const EDIT_SCRATCH_CAP: usize = 1 << 16;

/// A loaded window of chunk columns lit by the server's incremental engine.
pub struct LightEngine {
    inner: EngineInner,
    edit_scratch: EditScratch,
}

/// Chunk coordinates keep every neighbor offset within `i32` block space.
const CHUNK_LIMIT: i32 = 1 << 26;

/// Split packed sections into (nonzero mask, empty mask, nonzero arrays), the
/// same shape `write_light_bitset`/`encode_light_update_multi` emit per kind.
fn split_light_sections(
    sections: Vec<[u8; LIGHT_SECTION_BYTES]>,
) -> (Vec<u64>, Vec<u64>, Vec<[u8; LIGHT_SECTION_BYTES]>) {
    let mut mask = Vec::new();
    let mut empty = Vec::new();
    let mut arrays = Vec::new();
    for (i, section) in sections.iter().enumerate() {
        if section.iter().all(|byte| *byte == 0) {
            owner::set_mask_bit(&mut empty, i);
        } else {
            owner::set_mask_bit(&mut mask, i);
            arrays.push(*section);
        }
    }
    (
        owner::trim_mask_words(mask),
        owner::trim_mask_words(empty),
        arrays,
    )
}

fn inner_window(inner: &EngineInner) -> (i32, i32) {
    match inner {
        EngineInner::Standard(engine) => engine.window(),
        EngineInner::Tall(engine) => engine.window(),
    }
}

fn inner_is_loaded(inner: &EngineInner, cx: i32, cz: i32) -> bool {
    match inner {
        EngineInner::Standard(engine) => engine.is_loaded((cx, cz)),
        EngineInner::Tall(engine) => engine.is_loaded((cx, cz)),
    }
}

impl LightEngine {
    pub fn new(min_y: i32, height: u32, has_skylight: bool) -> Result<Self, Error> {
        let height = i32::try_from(height).map_err(|_| Error::OutOfRange)?;
        if min_y % 16 != 0
            || height <= 0
            || height % 16 != 0
            || height / 16 + 2 > 64
            || min_y.checked_add(height).is_none()
        {
            return Err(Error::InvalidArgument);
        }
        Ok(Self {
            inner: EngineInner::Standard(owner::IncrementalLighting::from_chunks(
                min_y,
                height,
                has_skylight,
                [],
            )),
            edit_scratch: EditScratch::default(),
        })
    }

    /// `new` for tall custom dimensions: accepts up to 128 payload sections
    /// (130 including the two guard sections, e.g. `min_y = -512,
    /// height = 1536` for a 96-section world). Windows that still fit the
    /// `u64` cap run the standard engine, so the whole `u64` API surface keeps
    /// working on them; taller windows report edits and packet masks through
    /// [`Self::apply_edits_multi_word`] and [`Self::section_masks_multi_word`].
    pub fn new_tall(min_y: i32, height: u32, has_skylight: bool) -> Result<Self, Error> {
        let height = i32::try_from(height).map_err(|_| Error::OutOfRange)?;
        if min_y % 16 != 0
            || height <= 0
            || height % 16 != 0
            || height / 16 + 2 > owner::TALL_SECTION_LIMIT
            || min_y.checked_add(height).is_none()
        {
            return Err(Error::InvalidArgument);
        }
        let inner = if height / 16 + 2 <= 64 {
            EngineInner::Standard(owner::IncrementalLighting::from_chunks(
                min_y,
                height,
                has_skylight,
                [],
            ))
        } else {
            EngineInner::Tall(owner::TallLighting::from_chunks(
                min_y,
                height,
                has_skylight,
                [],
            ))
        };
        Ok(Self {
            inner,
            edit_scratch: EditScratch::default(),
        })
    }

    /// Number of sections per layer, including one guard section below and above.
    pub fn section_count(&self) -> usize {
        (self.window().1 / 16 + 2) as usize
    }

    pub fn cells_per_chunk(&self) -> usize {
        self.window().1 as usize * 256
    }

    fn window(&self) -> (i32, i32) {
        inner_window(&self.inner)
    }

    fn is_loaded(&self, cx: i32, cz: i32) -> bool {
        inner_is_loaded(&self.inner, cx, cz)
    }

    fn has_skylight(&self) -> bool {
        match &self.inner {
            EngineInner::Standard(engine) => engine.has_skylight(),
            EngineInner::Tall(engine) => engine.has_skylight(),
        }
    }

    fn sections_vec(&self, cx: i32, cz: i32, kind: LightKind) -> Vec<[u8; LIGHT_SECTION_BYTES]> {
        match &self.inner {
            EngineInner::Standard(engine) => engine.sections((cx, cz), kind),
            EngineInner::Tall(engine) => engine.sections((cx, cz), kind),
        }
    }

    fn check_chunk(cx: i32, cz: i32) -> Result<(), Error> {
        if cx.abs() < CHUNK_LIMIT && cz.abs() < CHUNK_LIMIT {
            Ok(())
        } else {
            Err(Error::OutOfRange)
        }
    }

    /// Replace one complete column. Cells are ordered y, z, x from the window's bottom.
    pub fn load_chunk(&mut self, cx: i32, cz: i32, cells: &[LightFact]) -> Result<(), Error> {
        Self::check_chunk(cx, cz)?;
        if cells.len() != self.cells_per_chunk() {
            return Err(Error::OutOfRange);
        }
        match &mut self.inner {
            EngineInner::Standard(engine) => engine.load_chunk_facts(cx, cz, cells),
            EngineInner::Tall(engine) => engine.load_chunk_facts(cx, cz, cells),
        }
    }

    /// Validate a batch the way `edit_batch` requires: nonempty, each edit in
    /// the window and in a loaded column, each fact representable. Fills the
    /// reusable edit scratch instead of allocating fresh lists per call; the
    /// neighbor targets are sorted and deduplicated exactly like the `BTreeSet`
    /// they replace, so `edit_batch` observes identical input.
    fn prepare_edits(&mut self, edits: &[LightEdit]) -> Result<(), Error> {
        if edits.is_empty() {
            return Err(Error::InvalidArgument);
        }
        let (min_y, height) = inner_window(&self.inner);
        let inner = &self.inner;
        let scratch = &mut self.edit_scratch;
        scratch.changes.clear();
        scratch.targets.clear();
        scratch.changes.reserve(edits.len());
        for edit in edits {
            let position = owner::Position::new(edit.x, edit.y, edit.z);
            let (cx, cz) = (edit.x.div_euclid(16), edit.z.div_euclid(16));
            if !(min_y..min_y + height).contains(&edit.y) || !inner_is_loaded(inner, cx, cz) {
                return Err(Error::OutOfRange);
            }
            scratch.changes.push((position, edit.fact.block()?));
            for (dx, dz) in (-1..=1).flat_map(|dx| (-1..=1).map(move |dz| (dx, dz))) {
                if inner_is_loaded(inner, cx + dx, cz + dz) {
                    scratch.targets.push((cx + dx, cz + dz));
                }
            }
        }
        scratch.targets.sort_unstable();
        scratch.targets.dedup();
        Ok(())
    }

    /// Drop oversized edit scratch so one huge batch cannot pin memory for
    /// the engine's lifetime.
    fn release_edit_scratch(&mut self) {
        if self.edit_scratch.changes.capacity() > EDIT_SCRATCH_CAP
            || self.edit_scratch.targets.capacity() > EDIT_SCRATCH_CAP
        {
            self.edit_scratch = EditScratch::default();
        }
    }

    /// Apply one or more edits atomically and report each changed neighbor section.
    ///
    /// On a tall window (>62 payload sections, built with [`Self::new_tall`])
    /// the `u64` change masks of `LightUpdate` cannot represent every changed
    /// section, so this fails closed with `Error::OutOfRange`; use
    /// [`Self::apply_edits_multi_word`] there.
    pub fn apply_edits(&mut self, edits: &[LightEdit]) -> Result<LightUpdate, Error> {
        self.prepare_edits(edits)?;
        let update = match &mut self.inner {
            EngineInner::Standard(engine) => engine.edit_batch(
                self.edit_scratch.changes.iter().copied(),
                self.edit_scratch.targets.iter().copied(),
            ),
            EngineInner::Tall(_) => return Err(Error::OutOfRange),
        };
        self.release_edit_scratch();
        Ok(update)
    }

    /// `apply_edits` reporting `BitSet`-shaped multi-word masks. Works on both
    /// engine flavors: on <=62-payload-section windows each mask's word zero
    /// equals the `u64` mask `apply_edits` returns.
    pub fn apply_edits_multi_word(
        &mut self,
        edits: &[LightEdit],
    ) -> Result<LightUpdateMultiWord, Error> {
        self.prepare_edits(edits)?;
        let update = match &mut self.inner {
            EngineInner::Standard(engine) => engine
                .edit_batch(
                    self.edit_scratch.changes.iter().copied(),
                    self.edit_scratch.targets.iter().copied(),
                )
                .into(),
            EngineInner::Tall(engine) => engine
                .edit_batch(
                    self.edit_scratch.changes.iter().copied(),
                    self.edit_scratch.targets.iter().copied(),
                )
                .into(),
        };
        self.release_edit_scratch();
        Ok(update)
    }

    /// Copy a section nibble array. Index zero starts one section below the window.
    pub fn section(
        &self,
        cx: i32,
        cz: i32,
        kind: LightKind,
        index: usize,
    ) -> Result<[u8; LIGHT_SECTION_BYTES], Error> {
        if !self.is_loaded(cx, cz) || index >= self.section_count() {
            return Err(Error::OutOfRange);
        }
        Ok(match &self.inner {
            EngineInner::Standard(engine) => engine.section_packed((cx, cz), kind, index),
            EngineInner::Tall(engine) => engine.section_packed((cx, cz), kind, index),
        })
    }

    /// One column's light sections split into the four packet masks and the
    /// two array lists, with multi-word `BitSet.toLongArray`-shaped masks.
    /// Works on both engine flavors; `None` when the column was never loaded.
    /// On <=62-payload-section windows word zero of each mask equals the `u64`
    /// mask the same split would produce.
    pub fn section_masks_multi_word(&self, cx: i32, cz: i32) -> Option<LightMasksMultiWord> {
        if !self.is_loaded(cx, cz) {
            return None;
        }
        let (sky_mask, empty_sky_mask, sky_arrays) = if self.has_skylight() {
            split_light_sections(self.sections_vec(cx, cz, LightKind::Sky))
        } else {
            (Vec::new(), Vec::new(), Vec::new())
        };
        let (block_mask, empty_block_mask, block_arrays) =
            split_light_sections(self.sections_vec(cx, cz, LightKind::Block));
        Some(LightMasksMultiWord {
            sky_mask,
            block_mask,
            empty_sky_mask,
            empty_block_mask,
            sky_arrays,
            block_arrays,
        })
    }
}
