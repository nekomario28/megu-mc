// SPDX-License-Identifier: LGPL-3.0-or-later
//! Direct-sampled procedural far terrain, independent of chunk generation.
//!
//! The five rings use strides of 2, 4, 8, 16 and 32 blocks per sample. The
//! full-detail square ends at 32 chunks; half-open distance bands then end at
//! 64, 128, 256, 512 and 1,024 chunks. Tile generation is queued and bounded
//! per frame, and packed quad storage is capped by a strict byte budget.

use std::collections::{HashMap, VecDeque};

pub const LOD_RING_COUNT: usize = 5;
pub const TILE_CELLS: usize = 16;
pub const FULL_DETAIL_RADIUS_CHUNKS: u32 = 32;
pub const RING_STRIDES_BLOCKS: [u8; LOD_RING_COUNT] = [2, 4, 8, 16, 32];
pub const RING_OUTER_RADII_CHUNKS: [u32; LOD_RING_COUNT] = [64, 128, 256, 512, 1024];

/// One direct sample of terrain surface and its rendered material.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FarHeightSample {
    pub surface_y: i32,
    pub sea_level: i32,
    pub top_color_rgba: u32,
    pub water_depth: u8,
}

/// Deterministic source of far terrain samples. Implementations should return
/// the same value for the same coordinates and stride.
pub trait FarHeightBiomeSampler {
    fn sample(&self, world_x: i64, world_z: i64, stride_blocks: u8) -> FarHeightSample;
}

/// Small standalone multi-octave terrain sampler for previews and tests.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProceduralFarSampler {
    pub seed: u64,
    pub sea_level: i32,
}

impl Default for ProceduralFarSampler {
    fn default() -> Self {
        Self {
            seed: 0x4d45_4755_4641_5201,
            sea_level: 63,
        }
    }
}

impl FarHeightBiomeSampler for ProceduralFarSampler {
    fn sample(&self, world_x: i64, world_z: i64, stride_blocks: u8) -> FarHeightSample {
        let stride = i64::from(stride_blocks.max(1));
        let x = world_x.div_euclid(stride);
        let z = world_z.div_euclid(stride);
        let broad = signed_noise(self.seed, x.div_euclid(32), z.div_euclid(32));
        let medium = signed_noise(
            self.seed ^ 0x9e37_79b9_7f4a_7c15,
            x.div_euclid(8),
            z.div_euclid(8),
        );
        let detail = signed_noise(self.seed ^ 0xd1b5_4a32_d192_ed03, x, z);
        let height = 68 + broad * 34 / 32768 + medium * 12 / 32768 + detail * 4 / 32768;
        let surface_y = height.clamp(-64, 319);
        let water_depth = self.sea_level.saturating_sub(surface_y).clamp(0, 255) as u8;
        let top_color_rgba = if water_depth > 0 {
            0x4a_82_c8_ff
        } else if surface_y > 92 {
            0x76_75_63_ff
        } else if broad < -7000 {
            0x91_87_55_ff
        } else {
            0x58_9b_4b_ff
        };
        FarHeightSample {
            surface_y,
            sea_level: self.sea_level,
            top_color_rgba,
            water_depth,
        }
    }
}

fn signed_noise(seed: u64, x: i64, z: i64) -> i32 {
    let mut value = seed
        ^ (x as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15)
        ^ (z as u64).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    value = (value ^ (value >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    value = (value ^ (value >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    (((value ^ (value >> 31)) >> 16) as u16 as i32) - 32768
}

/// Ring level and stride for a tile in the far horizon.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[repr(u8)]
pub enum LodRingLevel {
    Level0 = 0,
    Level1 = 1,
    Level2 = 2,
    Level3 = 3,
    Level4 = 4,
}

impl LodRingLevel {
    pub const ALL: [Self; LOD_RING_COUNT] = [
        Self::Level0,
        Self::Level1,
        Self::Level2,
        Self::Level3,
        Self::Level4,
    ];

    pub const fn index(self) -> usize {
        self as usize
    }
    pub const fn stride_blocks(self) -> u8 {
        RING_STRIDES_BLOCKS[self.index()]
    }
    pub const fn outer_radius_chunks(self) -> u32 {
        RING_OUTER_RADII_CHUNKS[self.index()]
    }
    pub const fn inner_radius_chunks(self) -> u32 {
        if self.index() == 0 {
            FULL_DETAIL_RADIUS_CHUNKS
        } else {
            RING_OUTER_RADII_CHUNKS[self.index() - 1]
        }
    }
    pub const fn tile_width_chunks(self) -> i64 {
        self.stride_blocks() as i64
    }

    /// Ring membership uses Chebyshev chunk distance. The inner boundary is
    /// exclusive, the outer boundary inclusive, so levels never overlap.
    pub fn contains_distance(self, distance_chunks: u32) -> bool {
        distance_chunks > self.inner_radius_chunks()
            && distance_chunks <= self.outer_radius_chunks()
    }

    pub fn for_distance(distance_chunks: u32) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|level| level.contains_distance(distance_chunks))
    }
}

/// Stable key for a tile on its level's globally aligned chunk grid.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct FarTileKey {
    pub level: LodRingLevel,
    pub tile_x: i64,
    pub tile_z: i64,
}

impl FarTileKey {
    pub fn origin_blocks(self) -> (i64, i64) {
        let width_blocks = self.level.tile_width_chunks() * 16;
        (self.tile_x * width_blocks, self.tile_z * width_blocks)
    }
}

/// Packed quad record. `face` is 0 for a top surface and 1..=4 for the
/// negative/positive X and Z skirts. A skirt spans `lower_y..y`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FarLodQuad {
    pub x: i64,
    pub z: i64,
    pub y: i32,
    pub lower_y: i32,
    pub extent: u8,
    pub color_rgba: u32,
    pub face: u8,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FarLodTile {
    pub key: FarTileKey,
    pub quads: Vec<FarLodQuad>,
}

impl FarLodTile {
    pub fn memory_bytes(&self) -> usize {
        std::mem::size_of::<Self>() + self.quads.capacity() * std::mem::size_of::<FarLodQuad>()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FarDirectLodConfig {
    pub max_memory_bytes: usize,
    pub max_tiles_per_frame: usize,
    pub skirt_depth_blocks: u16,
}

impl Default for FarDirectLodConfig {
    fn default() -> Self {
        Self {
            max_memory_bytes: 64 * 1024 * 1024,
            max_tiles_per_frame: 8,
            skirt_depth_blocks: 64,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FarDirectLodStats {
    pub cached_tiles: usize,
    pub memory_bytes: usize,
    pub queued_tiles: usize,
    pub built_last_frame: usize,
    pub evictions: u64,
}

struct CacheEntry {
    tile: FarLodTile,
    bytes: usize,
    last_used: u64,
}

/// Single-threaded ring tile queue and strict-memory LRU cache.
pub struct FarDirectLodEngine {
    config: FarDirectLodConfig,
    entries: HashMap<FarTileKey, CacheEntry>,
    queue: VecDeque<FarTileKey>,
    queued: HashMap<FarTileKey, ()>,
    memory_bytes: usize,
    tick: u64,
    evictions: u64,
    camera_chunk: (i64, i64),
    built_last_frame: usize,
}

impl FarDirectLodEngine {
    pub fn new(config: FarDirectLodConfig) -> Self {
        Self {
            config,
            entries: HashMap::new(),
            queue: VecDeque::new(),
            queued: HashMap::new(),
            memory_bytes: 0,
            tick: 0,
            evictions: 0,
            camera_chunk: (0, 0),
            built_last_frame: 0,
        }
    }

    pub fn config(&self) -> FarDirectLodConfig {
        self.config
    }

    pub fn stats(&self) -> FarDirectLodStats {
        FarDirectLodStats {
            cached_tiles: self.entries.len(),
            memory_bytes: self.memory_bytes,
            queued_tiles: self.queue.len(),
            built_last_frame: self.built_last_frame,
            evictions: self.evictions,
        }
    }

    pub fn tile(&mut self, key: FarTileKey) -> Option<&FarLodTile> {
        self.tick = self.tick.wrapping_add(1);
        let entry = self.entries.get_mut(&key)?;
        entry.last_used = self.tick;
        Some(&entry.tile)
    }

    /// Queue all visible ring tiles centered on the camera. Tile ownership is
    /// assigned by the tile center, preserving one ring per point; generation
    /// remains incremental through [`Self::build_frame`].
    pub fn set_camera(&mut self, camera_x_blocks: i64, camera_z_blocks: i64) {
        self.camera_chunk = (
            camera_x_blocks.div_euclid(16),
            camera_z_blocks.div_euclid(16),
        );
        self.queue.clear();
        self.queued.clear();
        for level in LodRingLevel::ALL {
            let width = level.tile_width_chunks();
            let outer = i64::from(level.outer_radius_chunks());
            let center_tx = self.camera_chunk.0.div_euclid(width);
            let center_tz = self.camera_chunk.1.div_euclid(width);
            let radius = outer.div_euclid(width) + 2;
            for tz in center_tz - radius..=center_tz + radius {
                for tx in center_tx - radius..=center_tx + radius {
                    let center_x = tx * width + width / 2;
                    let center_z = tz * width + width / 2;
                    let distance = (center_x - self.camera_chunk.0)
                        .abs()
                        .max((center_z - self.camera_chunk.1).abs());
                    let Ok(distance) = u32::try_from(distance) else {
                        continue;
                    };
                    if level.contains_distance(distance) {
                        let key = FarTileKey {
                            level,
                            tile_x: tx,
                            tile_z: tz,
                        };
                        if !self.entries.contains_key(&key) && self.queued.insert(key, ()).is_none()
                        {
                            self.queue.push_back(key);
                        }
                    }
                }
            }
        }
        self.evict_over_budget();
    }

    /// Build at most `max_tiles_per_frame` queued tiles, sampling every cell
    /// directly from `sampler` without constructing or saving chunks.
    pub fn build_frame<S: FarHeightBiomeSampler>(&mut self, sampler: &S) -> usize {
        let mut built = 0;
        for _ in 0..self.config.max_tiles_per_frame {
            let Some(key) = self.queue.pop_front() else {
                break;
            };
            self.queued.remove(&key);
            if self.entries.contains_key(&key) {
                continue;
            }
            let tile = build_tile(key, sampler, self.config.skirt_depth_blocks);
            let bytes = tile.memory_bytes();
            if bytes > self.config.max_memory_bytes {
                continue;
            }
            self.tick = self.tick.wrapping_add(1);
            self.memory_bytes = self.memory_bytes.saturating_add(bytes);
            self.entries.insert(
                key,
                CacheEntry {
                    tile,
                    bytes,
                    last_used: self.tick,
                },
            );
            built += 1;
            self.evict_over_budget();
        }
        self.built_last_frame = built;
        built
    }

    fn evict_over_budget(&mut self) {
        while self.memory_bytes > self.config.max_memory_bytes {
            let victim = self
                .entries
                .iter()
                .min_by(|(key_a, a), (key_b, b)| {
                    a.last_used.cmp(&b.last_used).then_with(|| {
                        distance_to_camera(self.camera_chunk, **key_b)
                            .cmp(&distance_to_camera(self.camera_chunk, **key_a))
                    })
                })
                .map(|(key, _)| *key);
            let Some(victim) = victim else { break };
            if let Some(entry) = self.entries.remove(&victim) {
                self.memory_bytes = self.memory_bytes.saturating_sub(entry.bytes);
                self.evictions = self.evictions.saturating_add(1);
            }
        }
    }
}

fn distance_to_camera(camera: (i64, i64), key: FarTileKey) -> i64 {
    let width = key.level.tile_width_chunks();
    let x = key.tile_x * width + width / 2;
    let z = key.tile_z * width + width / 2;
    (x - camera.0).abs().max((z - camera.1).abs())
}

fn build_tile<S: FarHeightBiomeSampler>(
    key: FarTileKey,
    sampler: &S,
    skirt_depth: u16,
) -> FarLodTile {
    let stride = i64::from(key.level.stride_blocks());
    let (origin_x, origin_z) = key.origin_blocks();
    let extent = key.level.stride_blocks();
    let mut quads = Vec::with_capacity(TILE_CELLS * TILE_CELLS + TILE_CELLS * 4);
    let mut heights = [[0_i32; TILE_CELLS]; TILE_CELLS];
    let mut colors = [[0_u32; TILE_CELLS]; TILE_CELLS];
    for z in 0..TILE_CELLS {
        for x in 0..TILE_CELLS {
            let world_x = origin_x + x as i64 * stride;
            let world_z = origin_z + z as i64 * stride;
            let sample = sampler.sample(world_x, world_z, extent);
            heights[z][x] = sample.surface_y;
            colors[z][x] = sample.top_color_rgba;
            quads.push(FarLodQuad {
                x: world_x,
                z: world_z,
                y: sample.surface_y,
                lower_y: sample.surface_y,
                extent,
                color_rgba: sample.top_color_rgba,
                face: 0,
            });
        }
    }
    let skirt = i32::from(skirt_depth).max(i32::from(extent));
    for i in 0..TILE_CELLS {
        let i = i as i64;
        let edges = [
            (
                origin_x,
                origin_z + i * stride,
                heights[i as usize][0],
                colors[i as usize][0],
                1,
            ),
            (
                origin_x + TILE_CELLS as i64 * stride,
                origin_z + i * stride,
                heights[i as usize][TILE_CELLS - 1],
                colors[i as usize][TILE_CELLS - 1],
                2,
            ),
            (
                origin_x + i * stride,
                origin_z,
                heights[0][i as usize],
                colors[0][i as usize],
                3,
            ),
            (
                origin_x + i * stride,
                origin_z + TILE_CELLS as i64 * stride,
                heights[TILE_CELLS - 1][i as usize],
                colors[TILE_CELLS - 1][i as usize],
                4,
            ),
        ];
        for (x, z, y, color_rgba, face) in edges {
            quads.push(FarLodQuad {
                x,
                z,
                y,
                lower_y: y.saturating_sub(skirt),
                extent,
                color_rgba,
                face,
            });
        }
    }
    FarLodTile { key, quads }
}
