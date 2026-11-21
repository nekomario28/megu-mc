// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! 64-shard [`DimensionState`] and the instance-scoped [`ServerContext`].
//!
//! Implements `specs/multi-version-architecture.md` §4: the chunk map is the
//! hottest contended lock in the server, so chunk overlays live in
//! `SHARD_COUNT` independent `RwLock`s instead of one process-global
//! `Mutex`. The stated lock order is `ServerContext` internals ->
//! `DimensionState` fields -> chunk shards in *ascending shard index*; never
//! shard -> context. [`DimensionState::with_chunks_write`] enforces the third
//! leg mechanically — it deduplicates the requested chunks, sorts the shard
//! set and acquires the write locks in ascending order, so cross-chunk writes
//! (double chests, fluid spread, piston pushes across a border, `/clone`)
//! cannot deadlock no matter which order callers pass coordinates in.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock, RwLockReadGuard, RwLockWriteGuard};

/// Number of independent chunk shards per dimension. Power of two so the shard
/// index is a mask, not a division.
pub const SHARD_COUNT: usize = 64;

/// Deterministic shard index for a chunk column. The multiplicative constants
/// spread adjacent chunks across shards in both the negative and positive
/// halves of the coordinate space (wrapping ops on the raw bit patterns, so the
/// result is identical on every platform), and the xor-fold pulls high bits
/// into the low six that the mask keeps.
pub fn chunk_shard(cx: i32, cz: i32) -> usize {
    let h = (cx as u32)
        .wrapping_mul(0x9E37_79B1)
        .wrapping_add((cz as u32).wrapping_mul(0x85EB_CA77).rotate_left(13));
    let folded = h ^ (h >> 16);
    folded as usize & (SHARD_COUNT - 1)
}

/// A chunk column address.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ChunkCoord {
    pub cx: i32,
    pub cz: i32,
}

impl ChunkCoord {
    pub fn new(cx: i32, cz: i32) -> Self {
        Self { cx, cz }
    }

    pub fn shard(self) -> usize {
        chunk_shard(self.cx, self.cz)
    }
}

/// An absolute block position.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct BlockCoord {
    pub x: i32,
    pub y: i32,
    pub z: i32,
}

impl BlockCoord {
    pub fn new(x: i32, y: i32, z: i32) -> Self {
        Self { x, y, z }
    }

    /// The column holding this block. `div_euclid` floors toward negative
    /// infinity, so `x = -1` lands in chunk `-1` like vanilla.
    pub fn chunk_coord(self) -> ChunkCoord {
        ChunkCoord {
            cx: self.x.div_euclid(16),
            cz: self.z.div_euclid(16),
        }
    }

    /// In-chunk position `(local_x, y, local_z)` with `local_x/z` in `0..16`.
    pub fn local_coord(self) -> (u8, i32, u8) {
        (
            self.x.rem_euclid(16) as u8,
            self.y,
            self.z.rem_euclid(16) as u8,
        )
    }
}

/// Per-chunk diff against the generated/persisted base: block overrides,
/// serialized block-entity payloads and a monotonically increasing `revision`
/// bumped on every write (change readers poll the revision instead of
/// diffing). The maps are private so a write cannot bypass the revision bump.
#[derive(Debug, Default)]
pub struct ChunkOverlay {
    blocks: HashMap<(u8, i32, u8), u32>,
    block_entities: HashMap<(u8, i32, u8), Vec<u8>>,
    revision: u64,
}

impl ChunkOverlay {
    pub fn blocks(&self) -> &HashMap<(u8, i32, u8), u32> {
        &self.blocks
    }

    pub fn block(&self, local: (u8, i32, u8)) -> Option<u32> {
        self.blocks.get(&local).copied()
    }

    pub fn set_block(&mut self, local: (u8, i32, u8), state: u32) {
        self.blocks.insert(local, state);
        self.revision += 1;
    }

    pub fn clear_block(&mut self, local: (u8, i32, u8)) {
        self.blocks.remove(&local);
        self.revision += 1;
    }

    pub fn block_entities(&self) -> &HashMap<(u8, i32, u8), Vec<u8>> {
        &self.block_entities
    }

    pub fn block_entity(&self, local: (u8, i32, u8)) -> Option<&[u8]> {
        self.block_entities.get(&local).map(Vec::as_slice)
    }

    pub fn set_block_entity(&mut self, local: (u8, i32, u8), payload: Vec<u8>) {
        self.block_entities.insert(local, payload);
        self.revision += 1;
    }

    pub fn remove_block_entity(&mut self, local: (u8, i32, u8)) {
        self.block_entities.remove(&local);
        self.revision += 1;
    }

    /// Bumped on every write; never decreases for the life of the overlay.
    pub fn revision(&self) -> u64 {
        self.revision
    }

    pub fn is_empty(&self) -> bool {
        self.blocks.is_empty() && self.block_entities.is_empty()
    }
}

/// One tracked entity. Keyed by entity id in [`DimensionState::entities`].
#[derive(Clone, Debug, PartialEq)]
pub struct EntityRecord {
    pub kind: String,
    pub pos: [f64; 3],
    pub vel: [f64; 3],
    pub yaw: f32,
    pub pitch: f32,
    /// Column the entity is currently counted in (moves update it).
    pub chunk: ChunkCoord,
}

/// What a scheduled tick applies to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TickKind {
    Block,
    Fluid,
}

/// A scheduled block/fluid tick due at game tick `tick`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingTick {
    pub tick: u64,
    pub pos: BlockCoord,
    pub kind: TickKind,
    /// Lower runs first within one tick.
    pub priority: i32,
}

/// Read access to a fixed, sorted set of shard read guards.
pub struct ChunkReadSet<'a> {
    guards: Vec<(usize, RwLockReadGuard<'a, HashMap<ChunkCoord, ChunkOverlay>>)>,
}

impl ChunkReadSet<'_> {
    /// The overlay stored at `coord`, or `None` when absent — or when the
    /// coordinate's shard was not part of this read set.
    pub fn get(&self, coord: ChunkCoord) -> Option<&ChunkOverlay> {
        let shard = coord.shard();
        let idx = self
            .guards
            .binary_search_by_key(&shard, |(s, _)| *s)
            .ok()?;
        self.guards[idx].1.get(&coord)
    }
}

/// Write access to a fixed, sorted set of shard write guards. Constructed only
/// by [`DimensionState::with_chunks_write`], which guarantees every
/// coordinate's shard is present exactly once and locked in ascending order.
pub struct ChunkWriteSet<'a> {
    guards: Vec<(usize, RwLockWriteGuard<'a, HashMap<ChunkCoord, ChunkOverlay>>)>,
}

impl ChunkWriteSet<'_> {
    fn guard_index(&self, coord: ChunkCoord) -> Option<usize> {
        self.guards
            .binary_search_by_key(&coord.shard(), |(s, _)| *s)
            .ok()
    }

    pub fn get(&self, coord: ChunkCoord) -> Option<&ChunkOverlay> {
        self.guard_index(coord)
            .and_then(|i| self.guards[i].1.get(&coord))
    }

    /// The overlay at `coord`, inserting an empty one when missing. `None`
    /// only when the coordinate's shard is not part of this write set.
    pub fn get_mut(&mut self, coord: ChunkCoord) -> Option<&mut ChunkOverlay> {
        let i = self.guard_index(coord)?;
        Some(self.guards[i].1.entry(coord).or_default())
    }

    /// Convenience for the common cross-chunk edit: set `block` to `state`
    /// inside this write set. Returns `false` when the block's shard was not
    /// requested.
    pub fn set_block(&mut self, block: BlockCoord, state: u32) -> bool {
        match self.get_mut(block.chunk_coord()) {
            Some(overlay) => {
                overlay.set_block(block.local_coord(), state);
                true
            }
            None => false,
        }
    }
}

/// Runtime state of one dimension: 64 chunk shards, the entity map and the
/// scheduled-tick queue. One instance per dimension; nothing is global.
pub struct DimensionState {
    name: String,
    chunk_shards: [RwLock<HashMap<ChunkCoord, ChunkOverlay>>; SHARD_COUNT],
    entities: RwLock<HashMap<i32, EntityRecord>>,
    pending_ticks: Mutex<Vec<PendingTick>>,
}

impl DimensionState {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            chunk_shards: std::array::from_fn(|_| RwLock::new(HashMap::new())),
            entities: RwLock::new(HashMap::new()),
            pending_ticks: Mutex::new(Vec::new()),
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// Run `f` with the chunk shards covering `coords` read-locked in
    /// ascending shard-index order (the §4 lock order).
    pub fn with_chunks_read<R>(
        &self,
        coords: &[ChunkCoord],
        f: impl FnOnce(&ChunkReadSet<'_>) -> R,
    ) -> R {
        let shards = sorted_shards(coords);
        let guards = shards
            .iter()
            .map(|&s| {
                (
                    s,
                    self.chunk_shards[s]
                        .read()
                        .unwrap_or_else(|p| p.into_inner()),
                )
            })
            .collect();
        f(&ChunkReadSet { guards })
    }

    /// Run `f` with the chunk shards covering `coords` write-locked in
    /// strictly ascending shard-index order. This is the only supported way to
    /// take more than one chunk write lock at once, and because every caller
    /// acquires the same shards in the same order, lock cycles — and with them
    /// deadlocks — cannot form between concurrent cross-chunk writes.
    pub fn with_chunks_write<R>(
        &self,
        coords: &[ChunkCoord],
        f: impl FnOnce(&mut ChunkWriteSet<'_>) -> R,
    ) -> R {
        let shards = sorted_shards(coords);
        let guards = shards
            .iter()
            .map(|&s| {
                (
                    s,
                    self.chunk_shards[s]
                        .write()
                        .unwrap_or_else(|p| p.into_inner()),
                )
            })
            .collect();
        f(&mut ChunkWriteSet { guards })
    }

    /// Single-chunk read shortcut.
    pub fn block(&self, block: BlockCoord) -> Option<u32> {
        let coord = block.chunk_coord();
        let local = block.local_coord();
        self.with_chunks_read(&[coord], |set| set.get(coord).and_then(|o| o.block(local)))
    }

    /// Single-chunk write shortcut.
    pub fn set_block(&self, block: BlockCoord, state: u32) {
        let coord = block.chunk_coord();
        let local = block.local_coord();
        self.with_chunks_write(&[coord], |set| {
            if let Some(overlay) = set.get_mut(coord) {
                overlay.set_block(local, state);
            }
        });
    }

    /// Current overlay revision at `coord` (`0` when no overlay exists).
    pub fn overlay_revision(&self, coord: ChunkCoord) -> u64 {
        self.with_chunks_read(&[coord], |set| set.get(coord).map_or(0, ChunkOverlay::revision))
    }

    /// Number of chunk columns carrying an overlay.
    pub fn overlay_count(&self) -> usize {
        self.chunk_shards
            .iter()
            .map(|s| s.read().unwrap_or_else(|p| p.into_inner()).len())
            .sum()
    }

    pub fn add_entity(&self, id: i32, record: EntityRecord) {
        self.entities
            .write()
            .unwrap_or_else(|p| p.into_inner())
            .insert(id, record);
    }

    pub fn remove_entity(&self, id: i32) -> Option<EntityRecord> {
        self.entities
            .write()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&id)
    }

    pub fn entity(&self, id: i32) -> Option<EntityRecord> {
        self.entities
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .get(&id)
            .cloned()
    }

    pub fn entity_count(&self) -> usize {
        self.entities
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .len()
    }

    pub fn schedule_tick(&self, tick: PendingTick) {
        self.pending_ticks
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push(tick);
    }

    /// Remove and return every tick due at or before `now`, sorted by
    /// `(tick, priority)` so application order is deterministic.
    pub fn drain_due_ticks(&self, now: u64) -> Vec<PendingTick> {
        let mut queue = self.pending_ticks.lock().unwrap_or_else(|p| p.into_inner());
        let (mut due, keep): (Vec<PendingTick>, Vec<PendingTick>) =
            queue.drain(..).partition(|t| t.tick <= now);
        *queue = keep;
        due.sort_by_key(|t| (t.tick, t.priority));
        due
    }

    pub fn pending_tick_count(&self) -> usize {
        self.pending_ticks
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .len()
    }
}

/// The deduplicated, ascending shard set covering `coords`.
fn sorted_shards(coords: &[ChunkCoord]) -> Vec<usize> {
    let mut shards: Vec<usize> = coords.iter().map(|c| c.shard()).collect();
    shards.sort_unstable();
    shards.dedup();
    shards
}

/// Every dimension one server instance knows: the vanilla trio plus any
/// dynamic mod dimensions and instanced 6-DOF sub-worlds. Fully
/// instance-scoped — two `ServerContext`s share nothing, so tests run in
/// parallel without global bleed.
#[derive(Default)]
pub struct ServerContext {
    dimensions: RwLock<HashMap<String, Arc<DimensionState>>>,
}

impl ServerContext {
    pub fn new() -> Self {
        Self::default()
    }

    /// Look up a dimension without creating it.
    pub fn dimension(&self, name: &str) -> Option<Arc<DimensionState>> {
        self.dimensions
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .get(name)
            .cloned()
    }

    /// Look up a dimension, registering an empty [`DimensionState`] when
    /// missing. The map write lock is released before the `Arc` is returned,
    /// so callers then take shard locks on the context -> dimension -> shard
    /// path only.
    pub fn ensure_dimension(&self, name: &str) -> Arc<DimensionState> {
        if let Some(dim) = self.dimension(name) {
            return dim;
        }
        self.dimensions
            .write()
            .unwrap_or_else(|p| p.into_inner())
            .entry(name.to_string())
            .or_insert_with(|| Arc::new(DimensionState::new(name)))
            .clone()
    }

    /// Insert a caller-built dimension, returning the previous one.
    pub fn register_dimension(
        &self,
        name: impl Into<String>,
        dimension: Arc<DimensionState>,
    ) -> Option<Arc<DimensionState>> {
        self.dimensions
            .write()
            .unwrap_or_else(|p| p.into_inner())
            .insert(name.into(), dimension)
    }

    pub fn remove_dimension(&self, name: &str) -> Option<Arc<DimensionState>> {
        self.dimensions
            .write()
            .unwrap_or_else(|p| p.into_inner())
            .remove(name)
    }

    /// Dimension names in sorted (deterministic) order.
    pub fn dimension_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .dimensions
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .keys()
            .cloned()
            .collect();
        names.sort();
        names
    }

    pub fn dimension_count(&self) -> usize {
        self.dimensions
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .len()
    }

    pub fn is_empty(&self) -> bool {
        self.dimension_count() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::thread;

    #[test]
    fn block_coord_math_floors_like_vanilla() {
        let b = BlockCoord::new(-1, 70, -17);
        assert_eq!(b.chunk_coord(), ChunkCoord::new(-1, -2));
        assert_eq!(b.local_coord(), (15, 70, 15));
        let b = BlockCoord::new(31, -64, 32);
        assert_eq!(b.chunk_coord(), ChunkCoord::new(1, 2));
        assert_eq!(b.local_coord(), (15, -64, 0));
    }

    #[test]
    fn chunk_shard_is_deterministic_bounded_and_mixing() {
        // Same input, same shard.
        assert_eq!(chunk_shard(-17, 42), chunk_shard(-17, 42));
        // Every coordinate lands in range, on both halves of the axis.
        let mut seen = [false; SHARD_COUNT];
        for cx in -128..128 {
            for cz in -128..128 {
                let s = chunk_shard(cx, cz);
                assert!(s < SHARD_COUNT);
                seen[s] = true;
            }
        }
        // The grid hits every shard: adjacent columns do not clump.
        assert!(seen.iter().all(|&hit| hit));
        // Negative-only space mixes just as well as positive.
        let mut neg = [false; SHARD_COUNT];
        for cx in -64..0 {
            for cz in -64..0 {
                neg[chunk_shard(cx, cz)] = true;
            }
        }
        assert!(neg.iter().all(|&hit| hit));
    }

    fn different_shard(coord: ChunkCoord) -> ChunkCoord {
        (0..512)
            .map(|i| ChunkCoord::new(i, -2 * i - 1))
            .find(|c| c.shard() != coord.shard())
            .expect("shard function must spread a 512-long diagonal")
    }

    #[test]
    fn single_chunk_write_roundtrips_and_bumps_revision() {
        let dim = DimensionState::new("minecraft:overworld");
        let block = BlockCoord::new(-33, 64, 8);
        dim.set_block(block, 42);
        assert_eq!(dim.block(block), Some(42));
        let coord = block.chunk_coord();
        assert_eq!(dim.overlay_revision(coord), 1);
        dim.set_block(block, 43);
        assert_eq!(dim.overlay_revision(coord), 2);
        assert_eq!(dim.overlay_count(), 1);
    }

    #[test]
    fn with_chunks_write_spans_shards_atomically() {
        let dim = DimensionState::new("minecraft:overworld");
        let a = ChunkCoord::new(0, 0);
        let b = different_shard(a);
        let outside = (0..512)
            .map(|i| ChunkCoord::new(i, i + 3))
            .find(|c| c.shard() != a.shard() && c.shard() != b.shard())
            .expect("need a third shard");
        // Deliberately reversed request order: the helper must sort, not trust.
        dim.with_chunks_write(&[b, a, a], |set| {
            assert!(set.set_block(BlockCoord::new(a.cx * 16, 0, a.cz * 16), 7));
            assert!(set.set_block(BlockCoord::new(b.cx * 16, 0, b.cz * 16), 9));
            // A shard outside the write set is refused, not silently routed.
            assert!(!set.set_block(
                BlockCoord::new(outside.cx * 16, 0, outside.cz * 16),
                1
            ));
        });
        assert_eq!(dim.overlay_revision(a), 1);
        assert_eq!(dim.overlay_revision(b), 1);
    }

    #[test]
    fn cross_chunk_writes_from_eight_threads_never_deadlock_or_lose_one() {
        const THREADS: usize = 8;
        const ROUNDS: usize = 400;
        let dim = Arc::new(DimensionState::new("minecraft:overworld"));
        // Three coordinates on three different shards, so each thread grabs
        // the same shard set in a different order.
        let a = ChunkCoord::new(0, 0);
        let b = different_shard(a);
        let c = (0..512)
            .map(|i| ChunkCoord::new(-3 * i - 1, i))
            .find(|c| c.shard() != a.shard() && c.shard() != b.shard())
            .expect("need three distinct shards");
        let key = (0u8, 0i32, 0u8);
        let attempts = Arc::new(AtomicUsize::new(0));

        thread::scope(|scope| {
            for t in 0..THREADS {
                let dim = Arc::clone(&dim);
                let attempts = Arc::clone(&attempts);
                scope.spawn(move || {
                    // Half the threads request the chunks in the opposite
                    // order; one rotates the triple. Unsorted locking would
                    // cycle-wait here.
                    let order = match t % 4 {
                        0 => [a, b, c],
                        1 => [c, b, a],
                        2 => [b, c, a],
                        _ => [c, a, b],
                    };
                    for _ in 0..ROUNDS {
                        dim.with_chunks_write(&order, |set| {
                            for coord in order {
                                let overlay = set.get_mut(coord).unwrap();
                                let n = overlay.block(key).unwrap_or(0);
                                overlay.set_block(key, n + 1);
                            }
                        });
                        attempts.fetch_add(1, Ordering::Relaxed);
                    }
                });
            }
        });

        // Reaching this line proves no deadlock; the counts prove no write was
        // lost to a torn read-modify-write.
        let expected = (THREADS * ROUNDS) as u32;
        assert_eq!(attempts.load(Ordering::Relaxed), THREADS * ROUNDS);
        for coord in [a, b, c] {
            dim.with_chunks_read(&[coord], |set| {
                let overlay = set.get(coord).expect("overlay must exist");
                assert_eq!(overlay.block(key), Some(expected));
                assert_eq!(overlay.revision(), expected as u64);
            });
        }
    }

    #[test]
    fn entities_and_pending_ticks_are_dimension_scoped() {
        let dim = DimensionState::new("minecraft:the_nether");
        let record = EntityRecord {
            kind: "minecraft:ghast".to_string(),
            pos: [1.0, 70.0, -2.0],
            vel: [0.0; 3],
            yaw: 90.0,
            pitch: 0.0,
            chunk: ChunkCoord::new(0, -1),
        };
        dim.add_entity(7, record.clone());
        assert_eq!(dim.entity(7), Some(record));
        assert_eq!(dim.entity_count(), 1);
        assert!(dim.remove_entity(7).is_some());
        assert_eq!(dim.entity_count(), 0);

        dim.schedule_tick(PendingTick {
            tick: 5,
            pos: BlockCoord::new(0, 0, 0),
            kind: TickKind::Fluid,
            priority: 1,
        });
        dim.schedule_tick(PendingTick {
            tick: 5,
            pos: BlockCoord::new(1, 0, 0),
            kind: TickKind::Block,
            priority: 0,
        });
        dim.schedule_tick(PendingTick {
            tick: 9,
            pos: BlockCoord::new(2, 0, 0),
            kind: TickKind::Block,
            priority: 0,
        });
        let due = dim.drain_due_ticks(5);
        assert_eq!(due.len(), 2);
        assert_eq!(due[0].priority, 0); // priority orders within one tick
        assert_eq!(dim.pending_tick_count(), 1);
    }

    #[test]
    fn server_context_isolates_dimensions_and_instances() {
        let ctx = ServerContext::new();
        let overworld = ctx.ensure_dimension("minecraft:overworld");
        ctx.ensure_dimension("minecraft:the_nether");
        ctx.ensure_dimension("minecraft:the_end");
        let create = ctx.ensure_dimension("create:brass_dimension");
        let sub = ctx.ensure_dimension("instance:6dof_ship_42");
        // Idempotent lookup returns the same allocation.
        assert!(Arc::ptr_eq(&overworld, &ctx.ensure_dimension("minecraft:overworld")));

        let block = BlockCoord::new(8, 64, -8);
        overworld.set_block(block, 1);
        create.set_block(block, 2);
        sub.set_block(block, 3);
        assert_eq!(overworld.block(block), Some(1));
        assert_eq!(create.block(block), Some(2));
        assert_eq!(sub.block(block), Some(3));
        assert_eq!(ctx.dimension("minecraft:the_nether").unwrap().block(block), None);

        // A second context is a separate world; nothing leaks through statics.
        let other = ServerContext::new();
        assert!(other.is_empty());
        other.ensure_dimension("minecraft:overworld").set_block(block, 99);
        assert_eq!(overworld.block(block), Some(1));

        assert_eq!(
            ctx.dimension_names(),
            vec![
                "create:brass_dimension",
                "instance:6dof_ship_42",
                "minecraft:overworld",
                "minecraft:the_end",
                "minecraft:the_nether",
            ]
        );
    }

    #[test]
    fn dimension_state_is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<DimensionState>();
        assert_send_sync::<ServerContext>();
    }
}
