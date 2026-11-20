//! Clean-room incremental block and sky lighting for loaded chunk windows.
//!
//! The game-state owner supplies emission, opacity, and occlusion shapes. Edits
//! invalidate the 14-step influence region, darken it, and relight from live
//! emitters, direct sky, and unchanged boundary light. Unloaded chunks are hard
//! boundaries. This file is currently an independently tested integration seam.

use std::collections::{HashMap, HashSet, VecDeque};

type Chunk = (i32, i32);

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct Position {
    pub x: i32,
    pub y: i32,
    pub z: i32,
}

impl Position {
    pub const fn new(x: i32, y: i32, z: i32) -> Self {
        Self { x, y, z }
    }

    fn chunk(self) -> Chunk {
        (self.x.div_euclid(16), self.z.div_euclid(16))
    }

    fn adjacent(self) -> impl Iterator<Item = (Face, Self)> {
        Face::ALL.into_iter().map(move |face| {
            let (dx, dy, dz) = face.offset();
            (face, Self::new(self.x + dx, self.y + dy, self.z + dz))
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Face {
    // Opposite pairs follow the x, z, y octant-bit order.
    West,
    East,
    North,
    South,
    Down,
    Up,
}

impl Face {
    const ALL: [Self; 6] = [
        Self::West,
        Self::East,
        Self::North,
        Self::South,
        Self::Down,
        Self::Up,
    ];

    fn opposite(self) -> Self {
        Self::ALL[self as usize ^ 1]
    }

    fn offset(self) -> (i32, i32, i32) {
        match self {
            Self::West => (-1, 0, 0),
            Self::East => (1, 0, 0),
            Self::North => (0, 0, -1),
            Self::South => (0, 0, 1),
            Self::Down => (0, -1, 0),
            Self::Up => (0, 1, 0),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Half {
    Bottom,
    Top,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Facing {
    North,
    East,
    South,
    West,
}

impl Facing {
    fn quadrants(self) -> u8 {
        match self {
            Self::North => 0b0011,
            Self::East => 0b1010,
            Self::South => 0b1100,
            Self::West => 0b0101,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StairShape {
    Straight,
    InnerLeft,
    InnerRight,
    OuterLeft,
    OuterRight,
}

/// Occlusion at half-block resolution, sufficient for slab and stair shapes.
/// Each face records four quadrants in the same world-axis order as its opposite.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct FaceOcclusion([u8; 6]);

impl FaceOcclusion {
    const EMPTY: Self = Self([0; 6]);
    pub const FULL: Self = Self([0xf; 6]);

    /// Project occupied half-block octants onto the six outer faces. Octant
    /// indices use the coordinate bits x, z, y from least to most significant.
    pub fn from_octants(octants: u8) -> Self {
        let mut faces = [0; 6];
        for face in Face::ALL {
            let index = face as usize;
            let axis = index / 2;
            let below_axis = (1 << axis) - 1;
            for quadrant in 0..4 {
                // Insert the face's fixed coordinate into the quadrant index.
                let octant = (quadrant & below_axis)
                    | ((index & 1) << axis)
                    | ((quadrant & !below_axis) << 1);
                faces[index] |= ((octants >> octant) & 1) << quadrant;
            }
        }
        Self(faces)
    }

    pub fn slab(half: Half) -> Self {
        Self::with_base(half, 0)
    }

    pub fn stairs(half: Half, facing: Facing, shape: StairShape) -> Self {
        let sides = [Facing::North, Facing::East, Facing::South, Facing::West];
        let back = facing.quadrants();
        let left = sides[(facing as usize + 3) % 4].quadrants();
        let right = sides[(facing as usize + 1) % 4].quadrants();
        let step = match shape {
            StairShape::Straight => back,
            StairShape::InnerLeft => back | left,
            StairShape::InnerRight => back | right,
            StairShape::OuterLeft => back & left,
            StairShape::OuterRight => back & right,
        };
        Self::with_base(half, step)
    }

    fn with_base(half: Half, step: u8) -> Self {
        Self::from_octants(match half {
            Half::Bottom => 0xf | (step << 4),
            Half::Top => (0xf << 4) | step,
        })
    }

    /// Vanilla's face union can cover the whole face even if both sides are partial.
    pub fn seals(self, face: Face, neighbor: Self) -> bool {
        self.0[face as usize] | neighbor.0[face.opposite() as usize] == 0xf
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct LightBlock {
    opacity: u8,
    emission: u8,
    occlusion: FaceOcclusion,
}

impl LightBlock {
    pub const fn new(opacity: u8, emission: u8) -> Self {
        Self::with_occlusion(opacity, emission, FaceOcclusion::EMPTY)
    }

    pub const fn with_occlusion(opacity: u8, emission: u8, occlusion: FaceOcclusion) -> Self {
        Self {
            opacity: if opacity > 15 { 15 } else { opacity },
            emission: if emission > 15 { 15 } else { emission },
            occlusion,
        }
    }

    fn transmitted(self, face: Face, target: Self, level: u8) -> u8 {
        let remaining = level.saturating_sub(target.opacity.max(1));
        if remaining == 0 || self.occlusion.seals(face, target.occlusion) {
            0
        } else {
            remaining
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LightKind {
    Sky,
    Block,
}

/// Section payloads use the protocol's low-nibble-first layout. Only changed
/// sections are present. An entirely dark section uses its empty mask bit,
/// which clears the receiver's section without a zero-filled payload.
#[derive(Debug, Default, Eq, PartialEq)]
pub struct ChunkLightUpdate {
    pub chunk: Chunk,
    pub sky_mask: u64,
    pub empty_sky_mask: u64,
    pub block_mask: u64,
    pub empty_block_mask: u64,
    pub sky_sections: Vec<(usize, [u8; 2048])>,
    pub block_sections: Vec<(usize, [u8; 2048])>,
}

#[derive(Debug, Default, Eq, PartialEq)]
pub struct LightUpdate {
    pub chunks: Vec<ChunkLightUpdate>,
    /// Number of cells darkened/considered for relighting, across both layers.
    pub invalidated_cells: usize,
}

/// The caller owns synchronization and must publish each update to clients that
/// already hold the affected chunks. `load_chunk` rebuilds the window because a
/// newly loaded edge changes the set of valid light propagation paths.
#[derive(Clone, Debug)]
pub struct IncrementalLighting {
    min_y: i32,
    max_y: i32,
    loaded: HashSet<Chunk>,
    blocks: HashMap<Position, LightBlock>,
    direct_sky: HashMap<Position, u8>,
    sky: HashMap<Position, u8>,
    block: HashMap<Position, u8>,
}

impl IncrementalLighting {
    pub fn new(min_y: i32, height: i32) -> Self {
        assert!(min_y % 16 == 0 && height > 0 && height % 16 == 0);
        assert!(height / 16 + 2 <= 64, "section masks must fit in u64");
        Self {
            min_y,
            max_y: min_y + height - 1,
            loaded: HashSet::new(),
            blocks: HashMap::new(),
            direct_sky: HashMap::new(),
            sky: HashMap::new(),
            block: HashMap::new(),
        }
    }

    fn valid(&self, p: Position) -> bool {
        (self.min_y..=self.max_y).contains(&p.y) && self.loaded.contains(&p.chunk())
    }

    fn block_at(&self, p: Position) -> LightBlock {
        self.blocks.get(&p).copied().unwrap_or_default()
    }

    pub fn level(&self, kind: LightKind, p: Position) -> u8 {
        if !self.valid(p) {
            return 0;
        }
        self.light(kind).get(&p).copied().unwrap_or(0)
    }

    fn light(&self, kind: LightKind) -> &HashMap<Position, u8> {
        match kind {
            LightKind::Sky => &self.sky,
            LightKind::Block => &self.block,
        }
    }

    fn light_mut(&mut self, kind: LightKind) -> &mut HashMap<Position, u8> {
        match kind {
            LightKind::Sky => &mut self.sky,
            LightKind::Block => &mut self.block,
        }
    }

    fn source(&self, kind: LightKind, p: Position) -> u8 {
        match kind {
            LightKind::Sky => self.direct_sky.get(&p).copied().unwrap_or(0),
            LightKind::Block => self.block_at(p).emission,
        }
    }

    fn set_level(&mut self, kind: LightKind, p: Position, level: u8) {
        let light = self.light_mut(kind);
        if level == 0 {
            light.remove(&p);
        } else {
            light.insert(p, level);
        }
    }

    fn propagate(
        &mut self,
        kind: LightKind,
        mut queue: VecDeque<(Position, u8)>,
        patch: Option<&HashSet<Position>>,
    ) {
        while let Some((p, level)) = queue.pop_front() {
            if self.level(kind, p) != level || level <= 1 {
                continue;
            }
            let from = self.block_at(p);
            for (face, next) in p.adjacent() {
                if !self.valid(next) || patch.is_some_and(|area| !area.contains(&next)) {
                    continue;
                }
                let candidate = from.transmitted(face, self.block_at(next), level);
                if candidate > self.level(kind, next) {
                    self.set_level(kind, next, candidate);
                    queue.push_back((next, candidate));
                }
            }
        }
    }

    fn rebuild(&mut self) {
        self.direct_sky.clear();
        let columns: Vec<_> = self.loaded.iter().copied().collect();
        for (cx, cz) in columns {
            for lx in 0..16 {
                for lz in 0..16 {
                    self.refresh_column(cx * 16 + lx, cz * 16 + lz);
                }
            }
        }
        self.sky.clear();
        self.block.clear();
        for kind in [LightKind::Sky, LightKind::Block] {
            let sources: Vec<_> = match kind {
                LightKind::Sky => self
                    .direct_sky
                    .iter()
                    .map(|(&p, &level)| (p, level))
                    .collect(),
                LightKind::Block => self
                    .blocks
                    .iter()
                    .filter(|(p, b)| self.valid(**p) && b.emission > 0)
                    .map(|(&p, b)| (p, b.emission))
                    .collect(),
            };
            let mut queue = VecDeque::new();
            for (p, level) in sources {
                self.set_level(kind, p, level);
                queue.push_back((p, level));
            }
            self.propagate(kind, queue, None);
        }
    }

    /// Computes direct vertical skylight. Transparent air preserves level 15;
    /// scalar opacity attenuates it and a sealed vertical face stops it.
    fn refresh_column(&mut self, x: i32, z: i32) -> Vec<Position> {
        let mut changed = Vec::new();
        let mut level = 15u8;
        let mut above = FaceOcclusion::EMPTY;
        for y in (self.min_y..=self.max_y).rev() {
            let p = Position::new(x, y, z);
            if level > 0 {
                let block = self.block_at(p);
                level = if above.seals(Face::Down, block.occlusion) {
                    0
                } else {
                    level.saturating_sub(block.opacity)
                };
                above = block.occlusion;
            }
            let old = self.direct_sky.get(&p).copied().unwrap_or(0);
            if old != level {
                changed.push(p);
                if level == 0 {
                    self.direct_sky.remove(&p);
                } else {
                    self.direct_sky.insert(p, level);
                }
            }
        }
        changed
    }

    /// Loads one complete chunk's non-air block properties. Generated emitters
    /// must be supplied here as well as placed emitters on later edits.
    pub fn load_chunk(
        &mut self,
        cx: i32,
        cz: i32,
        cells: impl IntoIterator<Item = (Position, LightBlock)>,
    ) {
        self.loaded.insert((cx, cz));
        self.blocks.retain(|p, _| p.chunk() != (cx, cz));
        for (p, block) in cells {
            if self.valid(p) && block != LightBlock::default() {
                self.blocks.insert(p, block);
            }
        }
        self.rebuild();
    }

    pub fn unload_chunk(&mut self, cx: i32, cz: i32) {
        if self.loaded.remove(&(cx, cz)) {
            self.blocks.retain(|p, _| p.chunk() != (cx, cz));
            self.rebuild();
        }
    }

    /// The two BFS passes limit invalidation to 14 Manhattan steps per changed
    /// light source, because a level-15 source cannot illuminate a fifteenth step.
    /// Cells outside that region preserve their previously settled light.
    fn relight_patch(&mut self, kind: LightKind, changed: &[Position]) -> usize {
        if changed.is_empty() {
            return 0;
        }
        let mut patch = HashSet::new();
        let mut frontier = VecDeque::new();
        for &p in changed {
            if self.valid(p) && patch.insert(p) {
                frontier.push_back((p, 0u8));
            }
        }
        while let Some((p, distance)) = frontier.pop_front() {
            if distance == 14 {
                continue;
            }
            for (_, n) in p.adjacent() {
                if self.valid(n) && patch.insert(n) {
                    frontier.push_back((n, distance + 1));
                }
            }
        }
        // Darkening first is essential when an emitter is removed or opacity
        // rises. The surviving external boundary remains valid at distance 15.
        for &p in &patch {
            self.set_level(kind, p, 0);
        }
        let mut queue = VecDeque::new();
        for &p in &patch {
            let mut level = self.source(kind, p);
            let target = self.block_at(p);
            for (face, neighbor) in p.adjacent() {
                if !patch.contains(&neighbor) {
                    let incoming = self.level(kind, neighbor);
                    if incoming > 1 {
                        level = level.max(self.block_at(neighbor).transmitted(
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
        self.propagate(kind, queue, Some(&patch));
        patch.len()
    }

    /// Applies a live edit. Unloaded positions are ignored. The return value
    /// contains exact changed section masks and arrays for affected held chunks.
    pub fn edit(&mut self, p: Position, block: LightBlock) -> Option<LightUpdate> {
        if !self.valid(p) {
            return None;
        }
        let old_block = self.block_at(p);
        if old_block == block {
            return Some(LightUpdate::default());
        }
        let (cx, cz) = p.chunk();
        let mut before = Vec::new();
        for dx in -1..=1 {
            for dz in -1..=1 {
                let chunk = (cx + dx, cz + dz);
                if self.loaded.contains(&chunk) {
                    before.push((
                        chunk,
                        self.sections(chunk, LightKind::Sky),
                        self.sections(chunk, LightKind::Block),
                    ));
                }
            }
        }
        if block == LightBlock::default() {
            self.blocks.remove(&p);
        } else {
            self.blocks.insert(p, block);
        }
        let mut sky_changes = self.refresh_column(p.x, p.z);
        // A lateral skylight path may change even when the direct vertical
        // column is already dark beneath a roof.
        if (old_block.opacity != block.opacity || old_block.occlusion != block.occlusion)
            && !sky_changes.contains(&p)
        {
            sky_changes.push(p);
        }
        let work = self.relight_patch(LightKind::Sky, &sky_changes)
            + self.relight_patch(LightKind::Block, &[p]);
        let mut updates = Vec::new();
        for (chunk, sky_old, block_old) in before {
            let mut update = ChunkLightUpdate {
                chunk,
                ..Default::default()
            };
            let sky_new = self.sections(chunk, LightKind::Sky);
            let block_new = self.sections(chunk, LightKind::Block);
            for (i, (old, new)) in sky_old.iter().zip(sky_new).enumerate() {
                if old != &new {
                    if new.iter().all(|v| *v == 0) {
                        update.empty_sky_mask |= 1u64 << i;
                    } else {
                        update.sky_mask |= 1u64 << i;
                        update.sky_sections.push((i, new));
                    }
                }
            }
            for (i, (old, new)) in block_old.iter().zip(block_new).enumerate() {
                if old != &new {
                    if new.iter().all(|v| *v == 0) {
                        update.empty_block_mask |= 1u64 << i;
                    } else {
                        update.block_mask |= 1u64 << i;
                        update.block_sections.push((i, new));
                    }
                }
            }
            if update.sky_mask | update.empty_sky_mask | update.block_mask | update.empty_block_mask
                != 0
            {
                updates.push(update);
            }
        }
        updates.sort_by_key(|update| update.chunk);
        Some(LightUpdate {
            chunks: updates,
            invalidated_cells: work,
        })
    }

    /// Packed 16x16x16 sections, including one guard section below and above
    /// the dimension. The section at index zero begins at `min_y - 16`.
    pub fn sections(&self, chunk: Chunk, kind: LightKind) -> Vec<[u8; 2048]> {
        let count = ((self.max_y - self.min_y + 1) / 16 + 2) as usize;
        let mut out = vec![[0u8; 2048]; count];
        if !self.loaded.contains(&chunk) {
            return out;
        }
        for (section, array) in out.iter_mut().enumerate() {
            for ly in 0..16 {
                let y = self.min_y - 16 + section as i32 * 16 + ly;
                for lz in 0..16 {
                    for lx in 0..16 {
                        let p = Position::new(chunk.0 * 16 + lx, y, chunk.1 * 16 + lz);
                        let level = if y > self.max_y && kind == LightKind::Sky {
                            15
                        } else {
                            self.level(kind, p)
                        };
                        let index = (ly as usize * 256 + lz as usize * 16 + lx as usize) / 2;
                        array[index] |= level << ((lx as usize & 1) * 4);
                    }
                }
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const AIR: LightBlock = LightBlock::new(0, 0);
    const STONE: LightBlock = LightBlock::new(15, 0);
    const TORCH: LightBlock = LightBlock::new(0, 14);
    const LANTERN: LightBlock = LightBlock::new(0, 15);

    fn p(x: i32, y: i32, z: i32) -> Position {
        Position::new(x, y, z)
    }

    fn world(height: i32, chunks: &[Chunk]) -> IncrementalLighting {
        let mut engine = IncrementalLighting::new(0, height);
        for &(cx, cz) in chunks {
            engine.load_chunk(cx, cz, []);
        }
        engine
    }

    fn expect_level(engine: &IncrementalLighting, kind: LightKind, at: Position, expected: u8) {
        assert_eq!(engine.level(kind, at), expected, "light {kind:?} at {at:?}");
    }

    fn assert_matches_full_rebuild(engine: &IncrementalLighting) {
        let mut oracle = engine.clone();
        oracle.rebuild();
        assert_eq!(
            engine.block, oracle.block,
            "incremental block light diverged"
        );
        if engine.sky != oracle.sky {
            let mismatches: Vec<_> = engine
                .sky
                .keys()
                .chain(oracle.sky.keys())
                .filter(|&&p| engine.sky.get(&p) != oracle.sky.get(&p))
                .take(8)
                .map(|&p| (p, engine.sky.get(&p).copied(), oracle.sky.get(&p).copied()))
                .collect();
            panic!("incremental skylight diverged, examples {mismatches:?}");
        }
        assert_eq!(engine.direct_sky, oracle.direct_sky, "direct sky diverged");
    }

    fn nibble(sections: &[[u8; 2048]], y: i32, x: usize, z: usize) -> u8 {
        let section = (y.div_euclid(16) + 1) as usize;
        let idx = (y.rem_euclid(16) as usize * 256 + z * 16 + x) as usize;
        (sections[section][idx / 2] >> (4 * (idx % 2))) & 15
    }

    #[test]
    fn torch_and_lantern_emission_propagates_and_stops_at_fifteen_steps() {
        let mut engine = world(48, &[(0, 0), (1, 0)]);
        engine.edit(p(4, 20, 4), TORCH).unwrap();
        expect_level(&engine, LightKind::Block, p(4, 20, 4), 14);
        expect_level(&engine, LightKind::Block, p(5, 20, 4), 13);
        expect_level(&engine, LightKind::Block, p(17, 20, 4), 1);
        expect_level(&engine, LightKind::Block, p(18, 20, 4), 0);
        engine.edit(p(4, 20, 4), LANTERN).unwrap();
        expect_level(&engine, LightKind::Block, p(18, 20, 4), 1);
        expect_level(&engine, LightKind::Block, p(19, 20, 4), 0);
        assert_matches_full_rebuild(&engine);
    }

    #[test]
    fn generated_emitter_removal_darkens_and_surviving_source_relit() {
        let mut engine = IncrementalLighting::new(0, 32);
        engine.load_chunk(0, 0, [(p(4, 12, 4), LANTERN), (p(8, 12, 4), TORCH)]);
        expect_level(&engine, LightKind::Block, p(6, 12, 4), 13);
        engine.edit(p(8, 12, 4), AIR).unwrap();
        expect_level(&engine, LightKind::Block, p(6, 12, 4), 13);
        expect_level(&engine, LightKind::Block, p(8, 12, 4), 11);
        assert_matches_full_rebuild(&engine);
        engine.edit(p(4, 12, 4), AIR).unwrap();
        expect_level(&engine, LightKind::Block, p(6, 12, 4), 0);
        assert_matches_full_rebuild(&engine);
    }

    #[test]
    fn light_and_packet_masks_cross_vertical_section_boundary() {
        let mut engine = world(48, &[(0, 0)]);
        let update = engine.edit(p(5, 15, 5), LANTERN).unwrap();
        expect_level(&engine, LightKind::Block, p(5, 16, 5), 14);
        let sections = engine.sections((0, 0), LightKind::Block);
        assert_eq!(sections.len(), 5);
        assert_eq!(nibble(&sections, 15, 5, 5), 15);
        assert_eq!(nibble(&sections, 16, 5, 5), 14);
        assert_ne!(update.chunks[0].block_mask & (1 << 1), 0);
        assert_ne!(update.chunks[0].block_mask & (1 << 2), 0);
        let cleared = engine.edit(p(5, 15, 5), AIR).unwrap();
        assert_eq!(cleared.chunks[0].block_mask, 0);
        assert_eq!(cleared.chunks[0].empty_block_mask & 0b110, 0b110);
        assert!(cleared.chunks[0].block_sections.is_empty());
        assert_matches_full_rebuild(&engine);
    }

    #[test]
    fn last_emitter_removal_clears_exactly_the_previously_lit_sections() {
        let chunks = [(0, 0), (1, 0)];
        let mut engine = world(48, &chunks);
        let at = p(15, 20, 8);
        let placed = engine.edit(at, LANTERN).unwrap();
        let removed = engine.edit(at, AIR).unwrap();
        assert_eq!(placed.chunks.len(), chunks.len());
        assert_eq!(removed.chunks.len(), chunks.len());
        for ((chunk, lit), cleared) in chunks.into_iter().zip(placed.chunks).zip(removed.chunks) {
            assert_eq!(lit.chunk, chunk);
            assert_eq!(lit.block_mask, 0b1110);
            assert_eq!(lit.empty_block_mask, 0);
            assert_eq!(
                cleared,
                ChunkLightUpdate {
                    chunk,
                    empty_block_mask: 0b1110,
                    ..Default::default()
                }
            );
            assert_eq!(engine.sections(chunk, LightKind::Block), vec![[0; 2048]; 5]);
        }
    }

    #[test]
    fn slabs_seal_their_base_and_complementary_horizontal_faces() {
        let bottom = FaceOcclusion::slab(Half::Bottom);
        let top = FaceOcclusion::slab(Half::Top);
        for face in Face::ALL {
            assert_eq!(bottom.seals(face, FaceOcclusion::EMPTY), face == Face::Down);
            assert_eq!(top.seals(face, FaceOcclusion::EMPTY), face == Face::Up);
            assert!(FaceOcclusion::FULL.seals(face, FaceOcclusion::EMPTY));
        }
        for face in [Face::North, Face::East, Face::South, Face::West] {
            assert!(!bottom.seals(face, bottom));
            assert!(bottom.seals(face, top));
            assert!(top.seals(face.opposite(), bottom));
        }
    }

    #[test]
    fn stair_full_faces_follow_half_facing_and_corner() {
        let facings = [
            (Facing::North, Face::North),
            (Facing::East, Face::East),
            (Facing::South, Face::South),
            (Facing::West, Face::West),
        ];
        for (half, base) in [(Half::Bottom, Face::Down), (Half::Top, Face::Up)] {
            for (index, (facing, back)) in facings.into_iter().enumerate() {
                let left = facings[(index + 3) % 4].1;
                let right = facings[(index + 1) % 4].1;
                for (shape, full_faces) in [
                    (StairShape::Straight, &[base, back][..]),
                    (StairShape::InnerLeft, &[base, back, left][..]),
                    (StairShape::InnerRight, &[base, back, right][..]),
                    (StairShape::OuterLeft, &[base][..]),
                    (StairShape::OuterRight, &[base][..]),
                ] {
                    let stairs = FaceOcclusion::stairs(half, facing, shape);
                    for face in Face::ALL {
                        assert_eq!(
                            stairs.seals(face, FaceOcclusion::EMPTY),
                            full_faces.contains(&face),
                            "{half:?} {facing:?} {shape:?} {face:?}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn shape_only_edits_relight_vertical_sky_and_lateral_light() {
        let mut engine = world(32, &[(0, 0)]);
        let center = p(8, 16, 8);
        let below = p(8, 15, 8);
        let torch = p(8, 16, 7);
        engine.edit(torch, TORCH).unwrap();
        let slab = |half| LightBlock::with_occlusion(0, 0, FaceOcclusion::slab(half));
        let stairs = |facing| {
            LightBlock::with_occlusion(
                0,
                0,
                FaceOcclusion::stairs(Half::Bottom, facing, StairShape::Straight),
            )
        };
        for (block, sky, under, block_at_center) in [
            (slab(Half::Bottom), 15, 14, 13),
            (slab(Half::Top), 14, 14, 13),
            (
                LightBlock::with_occlusion(15, 0, FaceOcclusion::FULL),
                0,
                14,
                0,
            ),
            (stairs(Facing::North), 15, 14, 11),
            (stairs(Facing::South), 15, 14, 13),
            (AIR, 15, 15, 13),
        ] {
            let update = engine.edit(center, block).unwrap();
            assert!(!update.chunks.is_empty());
            expect_level(&engine, LightKind::Sky, center, sky);
            expect_level(&engine, LightKind::Sky, below, under);
            expect_level(&engine, LightKind::Block, center, block_at_center);
            assert_matches_full_rebuild(&engine);
        }
    }

    #[test]
    fn skylight_reaches_underhang_horizontally_and_crosses_section_edge() {
        let mut roof = Vec::new();
        for x in 3..=7 {
            for z in 3..=7 {
                roof.push((p(x, 16, z), STONE));
            }
        }
        let mut engine = IncrementalLighting::new(0, 48);
        engine.load_chunk(0, 0, roof);
        expect_level(&engine, LightKind::Sky, p(5, 17, 5), 15);
        expect_level(&engine, LightKind::Sky, p(5, 16, 5), 0);
        expect_level(&engine, LightKind::Sky, p(2, 15, 5), 15);
        expect_level(&engine, LightKind::Sky, p(5, 15, 5), 12);
        let sky = engine.sections((0, 0), LightKind::Sky);
        assert_eq!(nibble(&sky, 15, 5, 5), 12);
        assert_eq!(nibble(&sky, 16, 5, 5), 0);
        let update = engine.edit(p(5, 16, 5), AIR).unwrap();
        expect_level(&engine, LightKind::Sky, p(5, 15, 5), 15);
        assert_ne!(update.chunks[0].sky_mask & (1 << 1), 0);
        assert_matches_full_rebuild(&engine);
    }

    #[test]
    fn skylight_roof_placement_darkens_without_leakage() {
        let roof: Vec<_> = (0..16)
            .flat_map(|x| (0..16).map(move |z| (p(x, 16, z), STONE)))
            .collect();
        let mut engine = IncrementalLighting::new(0, 32);
        engine.load_chunk(0, 0, roof);
        expect_level(&engine, LightKind::Sky, p(8, 15, 8), 0);
        expect_level(&engine, LightKind::Sky, p(8, 17, 8), 15);
        assert_matches_full_rebuild(&engine);
        for _ in 0..4 {
            engine.edit(p(8, 16, 8), AIR).unwrap();
            expect_level(&engine, LightKind::Sky, p(8, 15, 8), 15);
            assert_matches_full_rebuild(&engine);
            engine.edit(p(8, 16, 8), STONE).unwrap();
            expect_level(&engine, LightKind::Sky, p(8, 15, 8), 0);
            assert_matches_full_rebuild(&engine);
        }
    }

    #[test]
    fn light_crosses_chunk_boundaries_in_both_axes_and_negative_coordinates() {
        let mut engine = world(32, &[(0, 0), (1, 0), (0, 1), (-1, 0), (0, -1)]);
        engine.edit(p(15, 12, 15), LANTERN).unwrap();
        expect_level(&engine, LightKind::Block, p(16, 12, 15), 14);
        expect_level(&engine, LightKind::Block, p(15, 12, 16), 14);
        engine.edit(p(0, 12, 0), TORCH).unwrap();
        expect_level(&engine, LightKind::Block, p(-1, 12, 0), 13);
        expect_level(&engine, LightKind::Block, p(0, 12, -1), 13);
        assert_matches_full_rebuild(&engine);
    }

    #[test]
    fn water_leaves_ice_and_solid_opacity_follow_registry_values() {
        let mut engine = world(32, &[(0, 0)]);
        engine.edit(p(4, 8, 5), LANTERN).unwrap();
        for (opacity, expected) in [(0, 14), (1, 14), (2, 13), (3, 12), (15, 0)] {
            let update = engine
                .edit(p(5, 8, 5), LightBlock::new(opacity, 0))
                .unwrap();
            if opacity > 0 {
                assert!(!update.chunks.is_empty());
            }
            expect_level(&engine, LightKind::Block, p(5, 8, 5), expected);
            assert_matches_full_rebuild(&engine);
        }
        engine.edit(p(5, 31, 5), LightBlock::new(3, 0)).unwrap();
        expect_level(&engine, LightKind::Sky, p(5, 31, 5), 12);
        assert_matches_full_rebuild(&engine);
    }

    #[test]
    fn repeated_emitter_toggle_is_idempotent_and_never_leaks() {
        let mut engine = world(32, &[(0, 0), (1, 0)]);
        let at = p(15, 12, 8);
        for _ in 0..12 {
            let lit = engine.edit(at, TORCH).unwrap();
            assert!(!lit.chunks.is_empty());
            assert!(lit.invalidated_cells <= 2 * 32 * 16 * 16 * 9);
            expect_level(&engine, LightKind::Block, p(16, 12, 8), 13);
            assert_matches_full_rebuild(&engine);
            let same = engine.edit(at, TORCH).unwrap();
            assert!(same.chunks.is_empty() && same.invalidated_cells == 0);
            let dark = engine.edit(at, AIR).unwrap();
            assert!(!dark.chunks.is_empty());
            expect_level(&engine, LightKind::Block, p(16, 12, 8), 0);
            assert_matches_full_rebuild(&engine);
        }
    }

    #[test]
    fn unloaded_chunk_is_hard_boundary_and_edits_do_no_work() {
        let mut engine = world(32, &[(0, 0)]);
        engine.edit(p(15, 12, 8), LANTERN).unwrap();
        expect_level(&engine, LightKind::Block, p(16, 12, 8), 0);
        assert!(engine.edit(p(16, 12, 8), TORCH).is_none());
        engine.load_chunk(1, 0, []);
        expect_level(&engine, LightKind::Block, p(16, 12, 8), 14);
        engine.unload_chunk(1, 0);
        expect_level(&engine, LightKind::Block, p(16, 12, 8), 0);
        assert_matches_full_rebuild(&engine);
    }

    #[test]
    fn opacity_toggle_darkens_and_relit_via_alternate_path() {
        let mut engine = world(32, &[(0, 0)]);
        engine.edit(p(5, 12, 5), LANTERN).unwrap();
        let obstacle = p(6, 12, 5);
        for _ in 0..5 {
            engine.edit(obstacle, STONE).unwrap();
            expect_level(&engine, LightKind::Block, obstacle, 0);
            expect_level(&engine, LightKind::Block, p(7, 12, 5), 11);
            assert_matches_full_rebuild(&engine);
            engine.edit(obstacle, AIR).unwrap();
            expect_level(&engine, LightKind::Block, p(7, 12, 5), 13);
            assert_matches_full_rebuild(&engine);
        }
    }

    #[test]
    fn skylight_underhang_crosses_chunk_and_section_edges() {
        let roof: Vec<_> = (12..=19)
            .flat_map(|x| (5..=11).map(move |z| (p(x, 16, z), STONE)))
            .collect();
        let mut engine = IncrementalLighting::new(0, 48);
        for cx in [0, 1] {
            engine.load_chunk(
                cx,
                0,
                roof.iter().copied().filter(|(at, _)| at.chunk() == (cx, 0)),
            );
        }
        expect_level(&engine, LightKind::Sky, p(15, 15, 8), 11);
        expect_level(&engine, LightKind::Sky, p(16, 15, 8), 11);
        let opened = engine.edit(p(16, 16, 8), AIR).unwrap();
        expect_level(&engine, LightKind::Sky, p(16, 15, 8), 15);
        expect_level(&engine, LightKind::Sky, p(15, 15, 8), 14);
        assert!(opened.chunks.iter().any(|c| c.chunk == (0, 0)));
        assert!(opened.chunks.iter().any(|c| c.chunk == (1, 0)));
        assert!(opened.chunks.iter().all(|c| c.sky_mask & (1 << 1) != 0));
        assert_matches_full_rebuild(&engine);
    }

    #[test]
    fn mixed_randomized_edits_match_rebuilt_light_and_exact_section_masks() {
        let mut engine = world(32, &[(0, 0), (1, 0)]);
        let mut state = 0x4154_2189u32;
        for step in 0..48 {
            // A fixed sequence samples edits around an x-chunk/y-section seam.
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let x = 11 + (state % 10) as i32;
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let y = 12 + (state % 9) as i32;
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let z = 5 + (state % 6) as i32;
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let block = [AIR, STONE, TORCH, LANTERN, LightBlock::new(2, 0)][(state % 5) as usize];
            let at = p(x, y, z);
            let before: Vec<_> = [(0, 0), (1, 0)]
                .into_iter()
                .map(|chunk| {
                    (
                        chunk,
                        engine.sections(chunk, LightKind::Sky),
                        engine.sections(chunk, LightKind::Block),
                    )
                })
                .collect();
            let update = engine.edit(at, block).unwrap();
            assert_matches_full_rebuild(&engine);
            for (chunk, sky_before, block_before) in before {
                for (kind, original) in [
                    (LightKind::Sky, sky_before),
                    (LightKind::Block, block_before),
                ] {
                    let after = engine.sections(chunk, kind);
                    let changed_mask = original
                        .iter()
                        .zip(&after)
                        .enumerate()
                        .fold(0u64, |mask, (i, (old, new))| {
                            mask | (u64::from(old != new) << i)
                        });
                    let actual = update.chunks.iter().find(|c| c.chunk == chunk);
                    let (mask, empty_mask, payload) = match (kind, actual) {
                        (LightKind::Sky, Some(c)) => {
                            (c.sky_mask, c.empty_sky_mask, &c.sky_sections)
                        }
                        (LightKind::Block, Some(c)) => {
                            (c.block_mask, c.empty_block_mask, &c.block_sections)
                        }
                        (_, None) => {
                            assert_eq!(changed_mask, 0, "step {step} chunk {chunk:?}");
                            continue;
                        }
                    };
                    assert_eq!(mask | empty_mask, changed_mask, "step {step}");
                    assert_eq!(mask & empty_mask, 0, "step {step}");
                    for (i, bytes) in payload {
                        assert!(mask & (1 << i) != 0);
                        assert_eq!(bytes, &after[*i], "step {step} section {i}");
                    }
                    assert_eq!(payload.len(), mask.count_ones() as usize);
                }
            }
        }
    }
}
