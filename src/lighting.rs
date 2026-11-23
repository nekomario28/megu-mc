// Clean-room incremental block and sky lighting for loaded chunk windows.
//
// The game-state owner supplies emission, opacity, and occlusion shapes. Edits
// invalidate the 14-step influence region, darken it, and relight from live
// emitters, direct sky, and unchanged boundary light. Unloaded chunks are hard
// boundaries. The window owner supplies complete chunks before applying edits.

use std::collections::{HashMap as LightingStdHashMap, VecDeque as LightingVecDeque};
use std::hash::{BuildHasherDefault, Hasher};

/// Multiplicative hasher for the small integer chunk keys; SipHash dominates
/// neighbor lookups in the lighting BFS otherwise.
#[derive(Clone, Copy, Debug, Default)]
struct ChunkHasher(u64);

impl Hasher for ChunkHasher {
    fn finish(&self) -> u64 {
        self.0
    }

    fn write(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            self.write_u64(u64::from(byte));
        }
    }

    fn write_i32(&mut self, value: i32) {
        self.write_u64(value as u32 as u64);
    }

    fn write_u64(&mut self, value: u64) {
        self.0 = (self.0.rotate_left(5) ^ value).wrapping_mul(0x517c_c1b7_2722_0a95);
    }
}

type LightingHashMap<K, V> = LightingStdHashMap<K, V, BuildHasherDefault<ChunkHasher>>;
type LightingHashSet<T> = std::collections::HashSet<T, BuildHasherDefault<ChunkHasher>>;

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
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
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

/// The caller synchronizes block changes with the light window and publishes
/// the changed sections to clients holding those chunks.
#[derive(Clone, Debug)]
pub struct IncrementalLighting {
    min_y: i32,
    height: i32,
    has_skylight: bool,
    chunks: LightingHashMap<Chunk, LightingChunk>,
    // Buffers reused across edits.
    snap: SnapLog,
    scratch: Scratch,
}

#[derive(Clone, Debug, Default)]
struct Scratch {
    patch: Vec<Position>,
    // Patch membership lives in the shared scratch, not per chunk: a u32 stamp
    // per cell cost 384 KiB per Overworld chunk for data that only exists
    // while a relight patch runs. The set is cleared per patch and its
    // capacity rides the same retention bound as the other buffers.
    patch_set: LightingHashSet<Position>,
    frontier: LightingVecDeque<(Position, u8)>,
    queue: LightingVecDeque<(Position, u8)>,
    // `edit_batch` change lists, kept with the BFS buffers so a tick of edits
    // allocates nothing: `block_changes`/`sky_changes` feed `relight_patch`,
    // `columns` dedups the vertical sky refreshes between them.
    block_changes: Vec<Position>,
    sky_changes: Vec<Position>,
    columns: Vec<(i32, i32)>,
}

/// Retained-capacity bound for each reused lighting buffer. A pathological
/// edit batch can spike a queue far past the steady-state working set; past
/// this bound the buffer is shrunk on the way back into the scratch rather
/// than pinning its peak forever.
const SCRATCH_KEEP_BYTES: usize = 256 * 1024;

fn bound_vec_scratch<T>(v: &mut Vec<T>) {
    let keep = SCRATCH_KEEP_BYTES / std::mem::size_of::<T>().max(1);
    if v.capacity() > keep {
        v.shrink_to(keep);
    }
}

fn bound_deque_scratch<T>(v: &mut LightingVecDeque<T>) {
    let keep = SCRATCH_KEEP_BYTES / std::mem::size_of::<T>().max(1);
    if v.capacity() > keep {
        v.shrink_to(keep);
    }
}

/// Pre-edit copies of the light sections an edit batch writes to, taken on
/// first write so unchanged sections are never copied or repacked. Each
/// entry is one kind's 4096 levels packed to 2048 nibble bytes — the same
/// layout the wire carries — so a section compare is a 2048-byte memcmp.
#[derive(Clone, Debug, Default)]
struct SnapLog {
    active: bool,
    batch: u32,
    arena: Vec<u8>,
    // (chunk, kind, section, arena offset)
    entries: Vec<(Chunk, usize, usize, usize)>,
}

/// Scratch stamps never take part in chunk equality.
#[derive(Clone, Debug, Default)]
struct Stamps(Vec<u32>);

impl PartialEq for Stamps {
    fn eq(&self, _: &Self) -> bool {
        true
    }
}

impl Eq for Stamps {}

/// One chunk's light window, decomposed so a cell costs ~2.5 bytes instead of
/// ~11: opacity+emission share one byte, the 6-byte `FaceOcclusion` sits in a
/// per-chunk palette behind a one-byte index, both light layers share one
/// nibble byte, and the attenuated sky floor packs two cells per byte.
///
/// `direct_sky` is *not* a 0/15 flag: `refresh_column` attenuates the level
/// through translucent cover, so the field needs the whole nibble range.
#[derive(Clone, Debug, Eq, PartialEq)]
struct LightingChunk {
    // opacity in the low nibble, emission in the high, one byte per cell.
    states: Vec<u8>,
    // Palette index per cell; u8::MAX escapes to `occlusions_wide` for a chunk
    // carrying more than 255 distinct occlusion shapes.
    occlusion_idx: Vec<u8>,
    occlusions: Vec<FaceOcclusion>,
    occlusion_ids: LightingHashMap<FaceOcclusion, u8>,
    occlusions_wide: LightingHashMap<u32, FaceOcclusion>,
    // Attenuated direct skylight, first cell in the low nibble.
    direct_sky: Vec<u8>,
    // sky in the high nibble, block in the low, one byte per cell.
    light_nibbles: Vec<u8>,
    // Highest cell in each column below full direct skylight. Used to skip
    // the uniform sky over adjacent columns during initialization.
    sky_top: [i32; 256],
    // Last batch that snapshotted each (kind, section).
    touched: Stamps,
}

/// Cells pack two light levels per byte: even index in the low nibble.
fn get_packed_nibbles(packed: &[u8], index: usize) -> u8 {
    (packed[index >> 1] >> ((index & 1) * 4)) & 15
}

fn set_packed_nibbles(packed: &mut [u8], index: usize, level: u8) {
    let shift = (index & 1) * 4;
    packed[index >> 1] = (packed[index >> 1] & !(15 << shift)) | ((level & 15) << shift);
}

impl LightingChunk {
    fn new(min_y: i32, height: i32, blocks: Vec<LightBlock>) -> Self {
        let len = height as usize * 256;
        assert_eq!(blocks.len(), len);
        let mut chunk = Self {
            states: Vec::with_capacity(len),
            occlusion_idx: Vec::with_capacity(len),
            occlusions: Vec::new(),
            occlusion_ids: LightingHashMap::default(),
            occlusions_wide: LightingHashMap::default(),
            direct_sky: vec![0; len / 2],
            light_nibbles: vec![0; len],
            sky_top: [min_y - 1; 256],
            touched: Stamps(vec![0; height as usize / 8]),
        };
        for (index, block) in blocks.iter().enumerate() {
            chunk.states.push(block.opacity | (block.emission << 4));
            let slot = chunk.occlusion_slot(index, block.occlusion);
            chunk.occlusion_idx.push(slot);
        }
        chunk
    }

    /// The palette slot for `occlusion`, interning new shapes up to the u8
    /// bound; past it the cell index itself keys `occlusions_wide`.
    fn occlusion_slot(&mut self, index: usize, occlusion: FaceOcclusion) -> u8 {
        if let Some(&id) = self.occlusion_ids.get(&occlusion) {
            return id;
        }
        if self.occlusions.len() < u8::MAX as usize {
            let id = self.occlusions.len() as u8;
            self.occlusions.push(occlusion);
            self.occlusion_ids.insert(occlusion, id);
            return id;
        }
        self.occlusions_wide.insert(index as u32, occlusion);
        u8::MAX
    }

    fn light_block(&self, index: usize) -> LightBlock {
        let pe = self.states[index];
        let occlusion = match self.occlusion_idx[index] {
            u8::MAX => self
                .occlusions_wide
                .get(&(index as u32))
                .copied()
                .unwrap_or_default(),
            id => self.occlusions[id as usize],
        };
        LightBlock::with_occlusion(pe & 15, pe >> 4, occlusion)
    }

    fn set_block(&mut self, index: usize, block: LightBlock) -> LightBlock {
        let old = self.light_block(index);
        self.states[index] = block.opacity | (block.emission << 4);
        let slot = self.occlusion_slot(index, block.occlusion);
        self.occlusion_idx[index] = slot;
        if slot != u8::MAX {
            self.occlusions_wide.remove(&(index as u32));
        }
        old
    }

    fn light_at(&self, kind: LightKind, index: usize) -> u8 {
        let packed = self.light_nibbles[index];
        match kind {
            LightKind::Sky => packed >> 4,
            LightKind::Block => packed & 15,
        }
    }

    fn set_light(&mut self, kind: LightKind, index: usize, level: u8) {
        let packed = &mut self.light_nibbles[index];
        *packed = match kind {
            LightKind::Sky => (*packed & 15) | ((level & 15) << 4),
            LightKind::Block => (*packed & !15) | (level & 15),
        };
    }

    fn direct_sky_at(&self, index: usize) -> u8 {
        get_packed_nibbles(&self.direct_sky, index)
    }

    /// One section of one light kind, packed low-nibble-first exactly as the
    /// wire carries it — the canonical [`megu_core::bits::LightArray`]
    /// layout. Snapshot entries and update payloads share this form.
    fn pack_section(&self, kind: LightKind, section: usize) -> [u8; 2048] {
        let base = section << 12;
        let mut out = megu_core::bits::LightArray::new_zero();
        for (j, byte) in out.as_bytes_mut().iter_mut().enumerate() {
            *byte = self.light_at(kind, base + j * 2)
                | (self.light_at(kind, base + j * 2 + 1) << 4);
        }
        *out.as_bytes()
    }

    fn store(&mut self, kind: LightKind, index: usize, level: u8, key: Chunk, snap: &mut SnapLog) {
        if self.light_at(kind, index) == level {
            return;
        }
        if snap.active {
            let section = index >> 12;
            let slot = kind as usize * (self.touched.0.len() / 2) + section;
            if self.touched.0[slot] != snap.batch {
                self.touched.0[slot] = snap.batch;
                snap.entries
                    .push((key, kind as usize, section, snap.arena.len()));
                snap.arena.extend_from_slice(&self.pack_section(kind, section));
            }
        }
        self.set_light(kind, index, level);
    }

    fn clear_light(&mut self) {
        self.direct_sky.fill(0);
        self.light_nibbles.fill(0);
    }
}

impl IncrementalLighting {
    #[allow(dead_code)] // Also used by the standalone Vanilla probe.
    pub fn new(min_y: i32, height: i32) -> Self {
        Self::from_chunks(min_y, height, true, [])
    }

    /// Initialize together so a neighbor can contribute light without a
    /// complete rebuild for every added chunk. Cells are ordered y, z, x.
    pub fn from_chunks(
        min_y: i32,
        height: i32,
        has_skylight: bool,
        chunks: impl IntoIterator<Item = (Chunk, Vec<LightBlock>)>,
    ) -> Self {
        assert!(min_y % 16 == 0 && height > 0 && height % 16 == 0);
        assert!(height / 16 + 2 <= 64, "section masks must fit in u64");
        let mut engine = Self {
            min_y,
            height,
            has_skylight,
            chunks: chunks
                .into_iter()
                .map(|(key, blocks)| (key, LightingChunk::new(min_y, height, blocks)))
                .collect(),
            snap: SnapLog::default(),
            scratch: Scratch::default(),
        };
        engine.rebuild();
        engine
    }

    fn flat(min_y: i32, height: i32, p: Position) -> Option<usize> {
        let dy = p.y - min_y;
        (0..height)
            .contains(&dy)
            .then_some((dy as usize) << 8 | ((p.z & 15) as usize) << 4 | (p.x & 15) as usize)
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

    #[allow(dead_code)] // Also used by the standalone Vanilla probe.
    pub fn level(&self, kind: LightKind, p: Position) -> u8 {
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
    fn claim(&mut self, p: Position, patch: &mut LightingHashSet<Position>) -> bool {
        self.index(p).is_some()
            && self.chunks.contains_key(&p.chunk())
            && patch.insert(p)
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
                let key = next.chunk();
                let Some(chunk) = chunks.get_mut(&key) else {
                    continue;
                };
                if patch.is_some_and(|set| !set.contains(&next)) {
                    continue;
                }
                let candidate = from.transmitted(face, chunk.light_block(index), level);
                if candidate > chunk.light_at(kind, index) {
                    chunk.store(kind, index, candidate, key, snap);
                    queue.push_back((next, candidate));
                }
            }
        }
    }

    fn rebuild(&mut self) {
        // The block and sky BFS queues ride the engine scratch: `load_chunk`
        // and the coverage-miss path in the game layer rebuild often, and two
        // fresh multi-kilobyte `VecDeque`s per rebuild were pure heap churn.
        let mut scratch = std::mem::take(&mut self.scratch);
        scratch.queue.clear();
        for (&(cx, cz), chunk) in &mut self.chunks {
            chunk.clear_light();
            if self.has_skylight {
                for i in 0..256 {
                    Self::refresh_column(self.min_y, self.height, chunk, i, &mut |_| {});
                }
                for i in 0..chunk.light_nibbles.len() {
                    chunk.light_nibbles[i] = get_packed_nibbles(&chunk.direct_sky, i) << 4;
                }
            }
            for (i, &pe) in chunk.states.iter().enumerate() {
                let emission = pe >> 4;
                if emission > 0 {
                    chunk.light_nibbles[i] |= emission;
                    scratch.queue.push_back((
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

        self.propagate(LightKind::Block, &mut scratch.queue, None);
        if !self.has_skylight {
            self.stash_scratch(scratch);
            return;
        }
        // Direct light already handles vertical propagation. Seed the spread
        // where a column can illuminate a less-lit horizontal neighbor.
        scratch.frontier.clear();
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
                        let from = get_packed_nibbles(&chunk.direct_sky, offset | i);
                        if from > get_packed_nibbles(&other.direct_sky, offset | ni) + 1 {
                            scratch.frontier.push_back((Position::new(x, y, z), from));
                        }
                    }
                }
            }
        }
        self.propagate(LightKind::Sky, &mut scratch.frontier, None);
        self.stash_scratch(scratch);
    }

    /// Insert chunks that newly entered the window without rebuilding the
    /// settled ones.
    ///
    /// Each new chunk gets the same private state `from_chunks` computes —
    /// refreshed skylight columns, emitter seeds, and the lateral-sky seed
    /// pass over its own columns (covering pairs inside the inserted set and
    /// pairs that reach into already-settled terrain). For the opposite
    /// direction, every settled cell sharing a border face with a new chunk is
    /// queued with its current level: that stored value is the fixpoint level
    /// a `from_chunks` over the union would carry across the same face, and
    /// re-emitting it replays exactly the wavefront that crosses the
    /// boundary. Propagation is monotone and adding chunks only adds light
    /// paths, so the seed superset converges to the union's fixpoint — pinned
    /// cell-for-cell by `perf_light_incremental_insert_matches_full_from_chunks`.
    pub fn insert_chunks_incremental(&mut self, new_chunks: Vec<(Chunk, Vec<LightBlock>)>) {
        let mut scratch = std::mem::take(&mut self.scratch);
        scratch.queue.clear();
        scratch.frontier.clear();
        let mut inserted: Vec<Chunk> = Vec::with_capacity(new_chunks.len());
        for (key @ (cx, cz), blocks) in new_chunks {
            if self.chunks.contains_key(&key) {
                continue;
            }
            let mut chunk = LightingChunk::new(self.min_y, self.height, blocks);
            if self.has_skylight {
                for i in 0..256 {
                    Self::refresh_column(self.min_y, self.height, &mut chunk, i, &mut |_| {});
                }
                for i in 0..chunk.light_nibbles.len() {
                    chunk.light_nibbles[i] = get_packed_nibbles(&chunk.direct_sky, i) << 4;
                }
            }
            for (i, &pe) in chunk.states.iter().enumerate() {
                let emission = pe >> 4;
                if emission > 0 {
                    chunk.light_nibbles[i] |= emission;
                    scratch.queue.push_back((
                        Position::new(
                            cx * 16 + (i & 15) as i32,
                            self.min_y + (i >> 8) as i32,
                            cz * 16 + ((i >> 4) & 15) as i32,
                        ),
                        emission,
                    ));
                }
            }
            self.chunks.insert(key, chunk);
            inserted.push(key);
        }
        // Already-settled cells that touch a new chunk replay their light into
        // it: the border cell already holds the level the union's build would
        // produce there, so queuing it reproduces the crossing wavefront. New
        // chunks seed their own emitters above; settled chunks are never
        // rescanned — `rebuild`'s pair seeding is replayed below only for
        // columns that live in an inserted chunk.
        for &(cx, cz) in &inserted {
            for (dx, dz) in [(-1, 0), (1, 0), (0, -1), (0, 1)] {
                let neighbor @ (nx, nz) = (cx + dx, cz + dz);
                if inserted.contains(&neighbor) {
                    continue;
                }
                let Some(chunk) = self.chunks.get(&neighbor) else {
                    continue;
                };
                for edge in 0..16i32 {
                    let (lx, lz) = match (dx, dz) {
                        (1, 0) => (0, edge),
                        (-1, 0) => (15, edge),
                        (0, 1) => (edge, 0),
                        _ => (edge, 15),
                    };
                    let column = ((lz as usize) << 4) | lx as usize;
                    for dy in 0..self.height {
                        let index = ((dy as usize) << 8) | column;
                        let block = chunk.light_at(LightKind::Block, index);
                        if block > 1 {
                            scratch.queue.push_back((
                                Position::new(nx * 16 + lx, self.min_y + dy, nz * 16 + lz),
                                block,
                            ));
                        }
                        let sky = chunk.light_at(LightKind::Sky, index);
                        if sky > 1 {
                            scratch.frontier.push_back((
                                Position::new(nx * 16 + lx, self.min_y + dy, nz * 16 + lz),
                                sky,
                            ));
                        }
                    }
                }
            }
        }
        self.propagate(LightKind::Block, &mut scratch.queue, None);
        if self.has_skylight {
            for &(cx, cz) in &inserted {
                let Some(chunk) = self.chunks.get(&(cx, cz)) else {
                    continue;
                };
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
                            let from = get_packed_nibbles(&chunk.direct_sky, offset | i);
                            if from > get_packed_nibbles(&other.direct_sky, offset | ni) + 1 {
                                scratch
                                    .frontier
                                    .push_back((Position::new(x, y, z), from));
                            }
                        }
                    }
                }
            }
        }
        self.propagate(LightKind::Sky, &mut scratch.frontier, None);
        self.stash_scratch(scratch);
    }

    /// Drop chunks from the window and settle the border ring that lost their
    /// contribution, without rebuilding the rest.
    ///
    /// Light reaching a surviving cell through a removed chunk fades at least
    /// one step per hop, so every cell the removal can darken sits within 14
    /// Manhattan steps of the hole — the same radius `relight_patch` already
    /// invalidates. Cells beyond it hold values no hole path could produce.
    pub fn remove_chunks(&mut self, removed: &[Chunk]) {
        let mut changed: Vec<Position> = Vec::new();
        for &(cx, cz) in removed {
            if !self.chunks.contains_key(&(cx, cz)) {
                continue;
            }
            for (dx, dz) in [(-1, 0), (1, 0), (0, -1), (0, 1)] {
                let neighbor @ (nx, nz) = (cx + dx, cz + dz);
                if !self.chunks.contains_key(&neighbor) || removed.contains(&neighbor) {
                    continue;
                }
                for edge in 0..16i32 {
                    let (lx, lz) = match (dx, dz) {
                        (1, 0) => (0, edge),
                        (-1, 0) => (15, edge),
                        (0, 1) => (edge, 0),
                        _ => (edge, 15),
                    };
                    for dy in 0..self.height {
                        changed.push(Position::new(
                            nx * 16 + lx,
                            self.min_y + dy,
                            nz * 16 + lz,
                        ));
                    }
                }
            }
        }
        self.chunks.retain(|key, _| !removed.contains(key));
        if changed.is_empty() {
            return;
        }
        let mut scratch = std::mem::take(&mut self.scratch);
        self.relight_patch(LightKind::Block, &changed, &mut scratch);
        if self.has_skylight {
            self.relight_patch(LightKind::Sky, &changed, &mut scratch);
        }
        self.stash_scratch(scratch);
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
            if get_packed_nibbles(&chunk.direct_sky, i) != level {
                set_packed_nibbles(&mut chunk.direct_sky, i, level);
                changed(min_y + dy);
            }
        }
        chunk.sky_top[column] = top;
    }

    /// Replaces one complete chunk for the standalone engine tests.
    #[allow(dead_code)] // Also used by the standalone Vanilla probe.
    pub fn load_chunk(
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

    #[cfg(test)]
    pub fn unload_chunk(&mut self, cx: i32, cz: i32) {
        if self.chunks.remove(&(cx, cz)).is_some() {
            self.rebuild();
        }
    }

    /// Adds chunks the live engine does not cover yet, then re-derives the
    /// light field once. The game layer uses this when the edit halo outgrows
    /// the engine's chunk set: the already-loaded neighbours stay, so only the
    /// missing chunks' terrain has to be snapshotted. `rebuild` runs over the
    /// union either way — extra chunks only add accurate propagation context.
    pub fn add_chunks(&mut self, chunks: impl IntoIterator<Item = (Chunk, Vec<LightBlock>)>) {
        let mut added = false;
        for ((cx, cz), blocks) in chunks {
            if !self.chunks.contains_key(&(cx, cz)) {
                self.chunks
                    .insert((cx, cz), LightingChunk::new(self.min_y, self.height, blocks));
                added = true;
            }
        }
        if added {
            self.rebuild();
        }
    }

    /// Store a detached scratch back on the engine, shrinking any buffer
    /// whose retained capacity outgrew [`SCRATCH_KEEP_BYTES`].
    fn stash_scratch(&mut self, mut scratch: Scratch) {
        bound_vec_scratch(&mut scratch.patch);
        // `HashSet` capacity is in buckets, not bytes; the bound is the same
        // 256 KiB ceiling at a worst-case ~48 B/entry load factor.
        if scratch.patch_set.capacity() > SCRATCH_KEEP_BYTES / 48 {
            let mut set = LightingHashSet::with_capacity_and_hasher(
                SCRATCH_KEEP_BYTES / 48,
                BuildHasherDefault::default(),
            );
            set.extend(scratch.patch_set.drain());
            scratch.patch_set = set;
        }
        bound_deque_scratch(&mut scratch.frontier);
        bound_deque_scratch(&mut scratch.queue);
        bound_vec_scratch(&mut scratch.block_changes);
        bound_vec_scratch(&mut scratch.sky_changes);
        bound_vec_scratch(&mut scratch.columns);
        self.scratch = scratch;
    }

    /// The two BFS passes limit invalidation to 14 Manhattan steps per changed
    /// light source, because a level-15 source cannot illuminate a fifteenth step.
    /// Cells outside that region preserve their previously settled light.
    fn relight_patch(
        &mut self,
        kind: LightKind,
        changed: &[Position],
        scratch: &mut Scratch,
    ) -> usize {
        if changed.is_empty() {
            return 0;
        }
        let Scratch {
            patch,
            patch_set,
            frontier,
            queue,
            ..
        } = scratch;
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
        // Visit the patch in section-local Z-order: the darken and reseed
        // passes below would otherwise walk cells in arbitrary BFS order, so
        // a Morton walk keeps `set_level`/`cell` lookups spatially local.
        // The passes are order-free — darkening is idempotent and reseeding
        // reads only settled, non-patch boundary light — so this reorders
        // work, never results.
        patch.sort_unstable_by_key(|&p| {
            let (cx, cz) = p.chunk();
            (
                cx,
                cz,
                (p.y - self.min_y) >> 4,
                megu_core::morton::morton3_encode_u4(
                    (p.x & 15) as u8,
                    (p.y & 15) as u8,
                    (p.z & 15) as u8,
                ),
            )
        });
        // Darkening first is essential when an emitter is removed or opacity
        // rises. The surviving external boundary remains valid at distance 15.
        for &p in patch.iter() {
            self.set_level(kind, p, 0);
        }
        for &p in patch.iter() {
            let mut level = self.source(kind, p);
            let Some((chunk, index)) = self.cell(p) else {
                continue;
            };
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
        patch.len()
    }

    /// Replays an edit batch against a complete pre-edit window. Each affected
    /// light patch is settled once, even when several blocks change in a tick.
    pub fn edit_batch(
        &mut self,
        edits: impl IntoIterator<Item = (Position, LightBlock)>,
        targets: impl IntoIterator<Item = Chunk>,
    ) -> LightUpdate {
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
        // The change lists ride the engine scratch: a tick of edits used to
        // allocate three fresh `Vec`s before either relight pass ran.
        let mut scratch = std::mem::take(&mut self.scratch);
        let mut block_changes = std::mem::take(&mut scratch.block_changes);
        let mut sky_changes = std::mem::take(&mut scratch.sky_changes);
        let mut columns = std::mem::take(&mut scratch.columns);
        for (p, block) in edits {
            let Some(i) = self.index(p) else { continue };
            let Some(chunk) = self.chunks.get_mut(&p.chunk()) else {
                continue;
            };
            let old = chunk.set_block(i, block);
            if old == block {
                continue;
            }
            block_changes.push(p);
            if self.has_skylight
                && (old.opacity != block.opacity || old.occlusion != block.occlusion)
            {
                columns.push((p.x, p.z));
                // The lateral path can change even below a roof with no direct sky.
                sky_changes.push(p);
            }
        }
        columns.sort_unstable();
        columns.dedup();
        for (x, z) in columns.drain(..) {
            let Some(chunk) = self.chunks.get_mut(&(x >> 4, z >> 4)) else {
                continue;
            };
            let i = ((z & 15) << 4 | (x & 15)) as usize;
            Self::refresh_column(self.min_y, self.height, chunk, i, &mut |y| {
                sky_changes.push(Position::new(x, y, z));
            });
        }
        let work = self.relight_patch(LightKind::Sky, &sky_changes, &mut scratch)
            + self.relight_patch(LightKind::Block, &block_changes, &mut scratch);
        self.snap.active = false;
        self.snap.entries.sort_unstable();
        // Hand the drained lists back: their capacity is the next batch's
        // allocation it no longer has to make.
        block_changes.clear();
        sky_changes.clear();
        scratch.block_changes = block_changes;
        scratch.sky_changes = sky_changes;
        scratch.columns = columns;
        self.stash_scratch(scratch);
        let mut updates = Vec::new();
        for chunk in targets {
            let Some(light) = self.chunks.get(&chunk) else {
                continue;
            };
            let mut update = ChunkLightUpdate {
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
                let bit = 1u64 << (section + 1);
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
                // The canonical `LightArray` owns the same 2048 packed bytes;
                // its all-zero check is the wire's empty-mask predicate.
                if megu_core::bits::LightArray::from_bytes(packed).is_all_zero() {
                    *empty |= bit;
                } else {
                    *mask |= bit;
                    sections.push((section + 1, packed));
                }
            }
            if update.sky_mask | update.empty_sky_mask | update.block_mask | update.empty_block_mask
                != 0
            {
                updates.push(update);
            }
        }
        // Same retention bound as the BFS scratch: a huge batch's snapshot
        // arena must not stay resident once the updates are built.
        bound_vec_scratch(&mut self.snap.arena);
        bound_vec_scratch(&mut self.snap.entries);
        updates.sort_by_key(|update| update.chunk);
        LightUpdate {
            chunks: updates,
            invalidated_cells: work,
        }
    }

    #[allow(dead_code)] // Also used by the standalone Vanilla probe.
    pub fn edit(&mut self, p: Position, block: LightBlock) -> Option<LightUpdate> {
        self.cell(p)?;
        let (cx, cz) = p.chunk();
        let targets: Vec<_> = self
            .chunks
            .keys()
            .copied()
            .filter(|(x, z)| (x - cx).abs() <= 1 && (z - cz).abs() <= 1)
            .collect();
        Some(self.edit_batch([(p, block)], targets))
    }

    /// Packed 16x16x16 sections, including one guard section below and above
    /// the dimension. The section at index zero begins at `min_y - 16`.
    pub fn sections(&self, chunk: Chunk, kind: LightKind) -> Vec<[u8; 2048]> {
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

#[cfg(test)]
mod incremental_lighting_tests {
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
        for (key, chunk) in &engine.chunks {
            let rebuilt = &oracle.chunks[key];
            for (name, actual, expected) in [
                ("light", &chunk.light_nibbles, &rebuilt.light_nibbles),
                ("direct sky", &chunk.direct_sky, &rebuilt.direct_sky),
            ] {
                assert!(
                    actual == expected,
                    "incremental {name} diverged in {key:?}"
                );
            }
        }
    }

    fn nibble(sections: &[[u8; 2048]], y: i32, x: usize, z: usize) -> u8 {
        let section = (y.div_euclid(16) + 1) as usize;
        let idx = y.rem_euclid(16) as usize * 256 + z * 16 + x;
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
    fn batched_emitter_move_in_dark_dimension_matches_settled_chunks() {
        let chunks = [(0, 0), (1, 0)];
        let before = p(15, 12, 8);
        let after = p(16, 12, 8);
        let mut left = vec![AIR; 32 * 256];
        left[12 * 256 + 8 * 16 + 15] = TORCH;
        let mut engine = IncrementalLighting::from_chunks(
            0,
            32,
            false,
            [(chunks[0], left), (chunks[1], vec![AIR; 32 * 256])],
        );
        let update = engine.edit_batch([(before, AIR), (after, TORCH)], chunks);
        assert_eq!(
            update.chunks.iter().map(|u| u.chunk).collect::<Vec<_>>(),
            chunks
        );
        for chunk in update.chunks {
            assert_eq!(chunk.sky_mask | chunk.empty_sky_mask, 0);
        }
        expect_level(&engine, LightKind::Sky, after, 0);
        expect_level(&engine, LightKind::Block, before, 13);
        expect_level(&engine, LightKind::Block, after, 14);
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

    /// PERF-BEAT-VANILLA-1 lane: `apply_queued_light_updates` grows the live
    /// engine one missing chunk at a time instead of rebuilding the window, so
    /// the incremental insert path must land on the same fixpoint a
    /// `from_chunks` over the union does — every cell, both layers.
    #[test]
    fn perf_light_incremental_insert_matches_full_from_chunks() {
        const HEIGHT: i32 = 64;
        let mut state = 0x9e37_79b9u32;
        let mut rng = move || {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            state
        };
        // A 5x5 window of randomized terrain: varying floors, sparse roofs and
        // emitters make both kinds cross chunk borders.
        let mut terrain: Vec<(Chunk, Vec<LightBlock>)> = Vec::new();
        for cx in 0..5 {
            for cz in 0..5 {
                let mut blocks = vec![AIR; HEIGHT as usize * 256];
                for i in 0..256usize {
                    let floor = 8 + (rng() % 24) as usize;
                    for y in 0..floor {
                        blocks[y * 256 + i] = STONE;
                    }
                    match rng() % 17 {
                        0 => blocks[floor * 256 + i] = TORCH,
                        1 => blocks[floor * 256 + i] = LANTERN,
                        2 => blocks[(floor + 5).min(HEIGHT as usize - 1) * 256 + i] = STONE,
                        3 => blocks[(floor + 2).min(HEIGHT as usize - 1) * 256 + i] =
                            LightBlock::new(2, 0),
                        _ => {}
                    }
                }
                terrain.push(((cx, cz), blocks));
            }
        }
        let oracle =
            IncrementalLighting::from_chunks(0, HEIGHT, true, terrain.iter().cloned());
        let mut keys: Vec<Chunk> = terrain.iter().map(|(key, _)| *key).collect();
        for i in (1..keys.len()).rev() {
            keys.swap(i, (rng() % (i as u32 + 1)) as usize);
        }
        let mut engine = IncrementalLighting::from_chunks(0, HEIGHT, true, []);
        let mut cursor = 0;
        while cursor < keys.len() {
            let take = 1 + (rng() % 4) as usize;
            let batch: Vec<_> = keys[cursor..(cursor + take).min(keys.len())]
                .iter()
                .map(|key| (*key, terrain.iter().find(|(k, _)| k == key).unwrap().1.clone()))
                .collect();
            cursor += batch.len();
            engine.insert_chunks_incremental(batch);
        }
        let assert_window_matches = |engine: &IncrementalLighting,
                                     oracle: &IncrementalLighting| {
            assert_eq!(engine.chunks.len(), oracle.chunks.len());
            for (key, chunk) in &engine.chunks {
                let rebuilt = &oracle.chunks[key];
                for (name, actual, expected) in [
                    ("light", &chunk.light_nibbles, &rebuilt.light_nibbles),
                    ("direct sky", &chunk.direct_sky, &rebuilt.direct_sky),
                ] {
                    assert!(
                        actual == expected,
                        "incremental {name} diverged in {key:?}"
                    );
                }
            }
        };
        assert_window_matches(&engine, &oracle);

        // Evictions relight the ring a hole leaves behind; the survivors must
        // match a fresh build of the remainder.
        let mut removed: Vec<Chunk> = keys
            .iter()
            .copied()
            .filter(|_| rng() % 3 == 0)
            .collect();
        if removed.len() == keys.len() {
            removed.pop();
        }
        engine.remove_chunks(&removed);
        let survivors = IncrementalLighting::from_chunks(
            0,
            HEIGHT,
            true,
            terrain
                .iter()
                .filter(|(key, _)| !removed.contains(key))
                .cloned(),
        );
        assert_window_matches(&engine, &survivors);
        // Re-insert the evicted set as one batch and land back on the union.
        let reinserted: Vec<_> = terrain
            .iter()
            .filter(|(key, _)| removed.contains(key))
            .cloned()
            .collect();
        engine.insert_chunks_incremental(reinserted);
        assert_window_matches(&engine, &oracle);
    }

    /// PERF-MEM-ZERO-ALLOC lane benchmark: settled light is rebuilt and
    /// edit-batched in rounds over a fixed 5x5 world. The identical body compiles
    /// and runs at the lane base, so `cargo test --release perf_mem_ -- --nocapture`
    /// on both trees prints the before/after µs/chunk lines.
    #[test]
    fn perf_mem_lighting_scratch_bench() {
        let chunks: Vec<Chunk> = (0..5)
            .flat_map(|cx| (0..5).map(move |cz| (cx, cz)))
            .collect();
        let mut engine = world(64, &chunks);
        // Seed emitters and a partial roof so both light kinds propagate.
        for &(cx, cz) in &chunks {
            engine.edit_batch(
                [
                    (p(cx * 16 + 4, 30, cz * 16 + 4), LANTERN),
                    (p(cx * 16 + 11, 34, cz * 16 + 9), TORCH),
                    (p(cx * 16 + 8, 50, cz * 16 + 8), STONE),
                ],
                chunks.iter().copied(),
            );
        }

        let rounds = 20usize;
        let t0 = std::time::Instant::now();
        for _ in 0..rounds {
            engine.rebuild();
        }
        let rebuild_ns = t0.elapsed().as_secs_f64() * 1e9;

        // Alternating place/remove keeps every batch doing real relight work.
        let place: Vec<(Position, LightBlock)> = (0..24)
            .map(|i| (p(16 + i, 22, 16 + (i * 5) % 40), LANTERN))
            .collect();
        let remove: Vec<(Position, LightBlock)> =
            place.iter().map(|&(pos, _)| (pos, AIR)).collect();
        let t1 = std::time::Instant::now();
        let mut sink = 0usize;
        for i in 0..rounds {
            let edits = if i % 2 == 0 { &place } else { &remove };
            let update = engine.edit_batch(edits.iter().copied(), chunks.iter().copied());
            sink = sink.wrapping_add(update.chunks.len());
        }
        let batch_ns = t1.elapsed().as_secs_f64() * 1e9;
        std::hint::black_box(sink);

        assert_matches_full_rebuild(&engine);
        let n = (rounds * chunks.len()) as f64;
        eprintln!(
            "perf_mem_lighting chunks={} rebuild={:.2}us/chunk edit_batch={:.2}us/chunk rounds={rounds}",
            chunks.len(),
            rebuild_ns / n / 1000.0,
            batch_ns / n / 1000.0,
        );
    }

    /// ULTRA-RAM lane: the per-chunk light window used to hold 967+ KiB of
    /// heap (384 KiB of BFS patch stamps alone). The packed layout must keep a
    /// 384-cell-tall Overworld chunk comfortably under the 450 KiB budget so a
    /// 192-chunk live engine stays under ~60 MiB.
    #[test]
    fn ultra_lighting_chunk_ram_footprint_is_compact() {
        let len = 384 * 256;
        let mut blocks = vec![AIR; len];
        blocks[0] = TORCH;
        blocks[1] = STONE;
        blocks[2] = LightBlock::with_occlusion(1, 0, FaceOcclusion::slab(Half::Bottom));
        let chunk = LightingChunk::new(-64, 384, blocks);
        let heap = chunk.states.capacity()
            + chunk.occlusion_idx.capacity()
            + chunk.direct_sky.capacity()
            + chunk.light_nibbles.capacity()
            + chunk.occlusions.capacity() * std::mem::size_of::<FaceOcclusion>()
            + chunk.touched.0.capacity() * 4
            + chunk.occlusion_ids.capacity()
                * std::mem::size_of::<(FaceOcclusion, u8)>()
            + chunk.occlusions_wide.capacity()
                * std::mem::size_of::<(u32, FaceOcclusion)>();
        assert!(
            heap < 450 * 1024,
            "LightingChunk heap {heap} bytes exceeds the 450 KiB budget"
        );
        // The packed cell state must round-trip bit-exact.
        assert_eq!(chunk.light_block(0), TORCH);
        assert_eq!(chunk.light_block(1), STONE);
        assert_eq!(
            chunk.light_block(2),
            LightBlock::with_occlusion(1, 0, FaceOcclusion::slab(Half::Bottom))
        );
        assert_eq!(chunk.light_block(3), AIR);
    }
}
