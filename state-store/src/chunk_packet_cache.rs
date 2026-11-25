// SPDX-License-Identifier: LGPL-3.0-or-later
//! Bounded cache for complete, already-encoded and compressed `0x27` chunk
//! wire frames. Callers must bump the affected chunk when its blocks, block
//! entities, or lighting change, before another viewer requests the frame.
//!
//! Cache access requires `&mut self`; a server sharing the cache between
//! threads can synchronize it at the owner boundary. Cached bytes are shared
//! with viewers through `Arc`, without copying the packet payload.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::Arc;

use megu_core::bounded::{BoundedMap, MustConsume};
use megu_core::coords::{ChunkKey as WireChunkKey, DimIndex};
use megu_core::net_budget::EncodeOnceFrameCache;

pub const DEFAULT_MAX_ENTRIES: usize = 2048;
pub const DEFAULT_MAX_BYTES: usize = 32 * 1024 * 1024;

type ChunkId = (u32, i32, i32);

/// One dimension, chunk position, and negotiated packet compression threshold.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ChunkPacketKey {
    pub dim: u32,
    pub cx: i32,
    pub cz: i32,
    pub compression_threshold: i32,
}

impl ChunkPacketKey {
    fn chunk(self) -> ChunkId {
        (self.dim, self.cx, self.cz)
    }
}

/// The exact wire frame to send to every viewer with this cache key.
#[derive(Clone, Debug)]
pub struct CachedChunkPacket {
    pub generation: u64,
    pub encoded_frame: Arc<[u8]>,
}

/// Lifetime counters. `invalidations` counts removed cached frames, while
/// `evictions` counts frames removed to make room for new entries.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ChunkPacketCacheStats {
    pub hits: u64,
    pub misses: u64,
    pub invalidations: u64,
    pub evictions: u64,
    pub bytes_used: usize,
}

struct Entry {
    packet: CachedChunkPacket,
    last_touch: u64,
}

/// An entry- and byte-capped LRU of completed `0x27` frames.
///
/// Hits and removals use a hash index plus an ordered touch index. Updating
/// the LRU is `O(log N)`; invalidating a chunk takes time proportional to the
/// number of its cached compression-threshold variants.
pub struct ChunkPacketCache {
    max_entries: usize,
    max_bytes: usize,
    entries: HashMap<ChunkPacketKey, Entry>,
    keys_by_chunk: HashMap<ChunkId, HashSet<ChunkPacketKey>>,
    generations: BoundedMap<ChunkId, u64, 16_384>,
    encode_once: EncodeOnceFrameCache<4_096>,
    lru: BTreeSet<(u64, ChunkPacketKey)>,
    touch_counter: u64,
    stats: ChunkPacketCacheStats,
}

impl Default for ChunkPacketCache {
    fn default() -> Self {
        Self::new(DEFAULT_MAX_ENTRIES, DEFAULT_MAX_BYTES)
    }
}

impl ChunkPacketCache {
    pub fn new(max_entries: usize, max_bytes: usize) -> Self {
        Self {
            max_entries,
            max_bytes,
            entries: HashMap::new(),
            keys_by_chunk: HashMap::new(),
            generations: BoundedMap::new(),
            encode_once: EncodeOnceFrameCache::new(),
            lru: BTreeSet::new(),
            touch_counter: 0,
            stats: ChunkPacketCacheStats::default(),
        }
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn bytes_used(&self) -> usize {
        self.stats.bytes_used
    }

    pub fn stats(&self) -> ChunkPacketCacheStats {
        self.stats
    }

    /// Increment this chunk's generation and discard every cached threshold
    /// variant. Call for block-entity edits and light edits as well as blocks.
    pub fn bump_chunk(&mut self, dim: u32, cx: i32, cz: i32) -> u64 {
        let chunk = (dim, cx, cz);
        let result = self
            .generations
            .get(&chunk)
            .copied()
            .unwrap_or(0)
            .checked_add(1)
            .expect("chunk generation counter exhausted");
        if let Some((evicted_chunk, _)) = self.generations.insert(chunk, result) {
            self.remove_chunk_entries(evicted_chunk).consume();
        }

        if let Some(keys) = self.keys_by_chunk.get(&chunk).cloned() {
            for key in keys {
                if self.remove(key) {
                    self.stats.invalidations += 1;
                }
            }
        }
        result
    }

    /// Invalidate the edited block's chunk. At a seam, invalidate the
    /// neighboring chunk as well; a corner also touches its diagonal neighbor.
    /// Euclidean division handles negative world coordinates correctly.
    pub fn bump_block(&mut self, dim: u32, block_x: i32, block_z: i32) -> u64 {
        let cx = block_x.div_euclid(16);
        let cz = block_z.div_euclid(16);
        let x_offset = match block_x.rem_euclid(16) {
            0 => -1,
            15 => 1,
            _ => 0,
        };
        let z_offset = match block_z.rem_euclid(16) {
            0 => -1,
            15 => 1,
            _ => 0,
        };

        let result = self.bump_chunk(dim, cx, cz);
        if x_offset != 0 {
            self.bump_chunk(dim, cx + x_offset, cz);
        }
        if z_offset != 0 {
            self.bump_chunk(dim, cx, cz + z_offset);
        }
        if x_offset != 0 && z_offset != 0 {
            self.bump_chunk(dim, cx + x_offset, cz + z_offset);
        }
        result
    }

    /// Look up `key` at the chunk's current generation without running an
    /// encoder while holding `&mut self`. Returns `Ok(frame)` on a cache hit
    /// (including an encode-once promotion hit) or `Err(generation)` on a miss
    /// so the caller can build the frame outside any lock and commit it with
    /// [`Self::insert_if_generation`].
    pub fn try_get(&mut self, key: ChunkPacketKey) -> Result<Arc<[u8]>, u64> {
        self.lookup_for_build(key).map(|(frame, _)| frame)
    }

    /// Store a frame encoded outside the cache lock when the chunk is still at
    /// `expected_generation`. If a concurrent invalidation bumped the chunk
    /// while encoding was in flight, the frame is returned without being cached.
    pub fn insert_if_generation<B>(
        &mut self,
        key: ChunkPacketKey,
        expected_generation: u64,
        frame: B,
    ) -> Arc<[u8]>
    where
        B: Into<Arc<[u8]>>,
    {
        let frame: Arc<[u8]> = frame.into();
        let current_generation = self.generations.get(&key.chunk()).copied().unwrap_or(0);
        if current_generation != expected_generation {
            return frame;
        }
        if key.compression_threshold == -1 {
            let wire_key = WireChunkKey {
                dim: DimIndex(key.dim),
                cx: key.cx,
                cz: key.cz,
            };
            self.encode_once
                .store(wire_key, expected_generation, Arc::clone(&frame));
        }
        self.store_entry(key, expected_generation, Arc::clone(&frame));
        frame
    }

    /// Return the shared wire frame and whether it came from the cache.
    /// The encoder is invoked exactly once on a miss and never on a hit.
    /// Oversized packets and zero-capacity caches return the encoded frame
    /// without displacing any existing entries.
    pub fn get_or_encode<F, B>(&mut self, key: ChunkPacketKey, encode_fn: F) -> (Arc<[u8]>, bool)
    where
        F: FnOnce() -> B,
        B: Into<Arc<[u8]>>,
    {
        match self.lookup_for_build(key) {
            Ok(hit) => hit,
            Err(generation) => {
                let frame = self.insert_if_generation(key, generation, encode_fn());
                (frame, false)
            }
        }
    }

    fn lookup_for_build(&mut self, key: ChunkPacketKey) -> Result<(Arc<[u8]>, bool), u64> {
        let generation = self.generations.get(&key.chunk()).copied().unwrap_or(0);
        if let Some(entry) = self.entries.get(&key) {
            if entry.packet.generation == generation {
                let frame = Arc::clone(&entry.packet.encoded_frame);
                let old_touch = entry.last_touch;
                self.lru.remove(&(old_touch, key));
                let touch = self.next_touch();
                self.entries.get_mut(&key).unwrap().last_touch = touch;
                self.lru.insert((touch, key));
                self.stats.hits += 1;
                return Ok((frame, true));
            }

            if self.remove(key) {
                self.stats.invalidations += 1;
            }
        }

        self.stats.misses += 1;
        if key.compression_threshold == -1 {
            let wire_key = WireChunkKey {
                dim: DimIndex(key.dim),
                cx: key.cx,
                cz: key.cz,
            };
            if let Some(frame) = self.encode_once.get_rev(wire_key, generation) {
                let frame = Arc::clone(&frame.bytes);
                self.store_entry(key, generation, Arc::clone(&frame));
                return Ok((frame, false));
            }
        }
        Err(generation)
    }

    fn store_entry(&mut self, key: ChunkPacketKey, generation: u64, frame: Arc<[u8]>) {
        let frame_len = frame.len();
        if self.max_entries == 0 || frame_len > self.max_bytes {
            return;
        }

        if self.remove(key) {
            self.stats.invalidations += 1;
        }
        while self.entries.len() >= self.max_entries
            || self.stats.bytes_used > self.max_bytes - frame_len
        {
            let oldest = self.lru.iter().next().expect("nonempty LRU").1;
            self.remove(oldest);
            self.stats.evictions += 1;
        }

        let touch = self.next_touch();
        self.entries.insert(
            key,
            Entry {
                packet: CachedChunkPacket {
                    generation,
                    encoded_frame: frame,
                },
                last_touch: touch,
            },
        );
        self.keys_by_chunk
            .entry(key.chunk())
            .or_default()
            .insert(key);
        self.lru.insert((touch, key));
        self.stats.bytes_used += frame_len;
    }

    /// Remove every packet entry belonging to `chunk`. The returned token makes
    /// the eviction cascade impossible to drop accidentally at a call site.
    fn remove_chunk_entries(&mut self, chunk: ChunkId) -> MustConsume<usize> {
        let mut removed = 0usize;
        if let Some(keys) = self.keys_by_chunk.get(&chunk).cloned() {
            for key in keys {
                removed += usize::from(self.remove(key));
            }
        }
        MustConsume::new(removed)
    }

    fn remove(&mut self, key: ChunkPacketKey) -> bool {
        let Some(entry) = self.entries.remove(&key) else {
            return false;
        };
        self.stats.bytes_used -= entry.packet.encoded_frame.len();
        self.lru.remove(&(entry.last_touch, key));
        if let Some(keys) = self.keys_by_chunk.get_mut(&key.chunk()) {
            keys.remove(&key);
            if keys.is_empty() {
                self.keys_by_chunk.remove(&key.chunk());
            }
        }
        true
    }

    fn next_touch(&mut self) -> u64 {
        if self.touch_counter == u64::MAX {
            // Rebase the rare exhausted logical clock in LRU order, keeping
            // every entry's relative age and deterministic tie-breaking.
            let keys: Vec<_> = self.lru.iter().map(|(_, key)| *key).collect();
            self.lru.clear();
            for (index, key) in keys.into_iter().enumerate() {
                let touch = index as u64 + 1;
                self.entries.get_mut(&key).unwrap().last_touch = touch;
                self.lru.insert((touch, key));
            }
            self.touch_counter = self.entries.len() as u64;
        }
        self.touch_counter += 1;
        self.touch_counter
    }
}
