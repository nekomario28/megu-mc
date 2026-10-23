// generation_context.rs — the local generation context: one mutable borrow of
// Gen plus the write-through overrides and caches the placement evaluators see.
// It owns no datapack types and no feature/tree logic.

use crate::block_registry::breg;
use crate::worldgen::Gen;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

/// FEAT_PROFILE diagnostic: time spent in feature-side terrain reads.
pub(crate) static READ_NS: AtomicU64 = AtomicU64::new(0);
pub(crate) static READ_N: AtomicU64 = AtomicU64::new(0);

// ---------- generation context ----------

pub(crate) struct FC<'a> {
    pub(crate) g: &'a mut Gen,
    pub(crate) wr: HashMap<(i32, i32, i32), u32>,
    rc: HashMap<(i32, i32, i32), u32>,   // terrain read-through cache

    bc: HashMap<(i32, i32, i32), Option<String>>, // biome cache per quart
    cc: Option<(i32, i32, std::sync::Arc<crate::worldgen::ChunkCols>)>,
    prof: bool,
}
impl<'a> FC<'a> {
    pub(crate) fn new(g: &'a mut Gen) -> Self {
        FC {
            g,
            wr: HashMap::new(),
            rc: HashMap::new(),
            bc: HashMap::new(),
            cc: None,
            prof: std::env::var("FEAT_PROFILE").is_ok(),
        }
    }
    pub(crate) fn get(&mut self, x: i32, y: i32, z: i32) -> u32 {
        if let Some(&s) = self.wr.get(&(x, y, z)) {
            return s;
        }
        if let Some(&s) = self.rc.get(&(x, y, z)) {
            return s;
        }
        let (cx, cz) = (x >> 4, z >> 4);
        if !matches!(&self.cc, Some((a, b, _)) if *a == cx && *b == cz) {
            match self.g.cols(cx, cz) {
                Some(c) => self.cc = Some((cx, cz, c)),
                None => return 0,
            }
        }
        let t0 = if self.prof {
            Some(std::time::Instant::now())
        } else {
            None
        };
        let st = match &self.cc {
            Some((_, _, c)) => self.g.terrain_cell(c, x, y, z),
            None => 0,
        };
        if let Some(t0) = t0 {
            READ_NS.fetch_add(t0.elapsed().as_nanos() as u64, Ordering::Relaxed);
            READ_N.fetch_add(1, Ordering::Relaxed);
        }
        self.rc.insert((x, y, z), st);
        st
    }
    pub(crate) fn set(&mut self, x: i32, y: i32, z: i32, st: u32) {
        self.wr.insert((x, y, z), st);
        // the heightmap lives on `Gen` so that a write for this chunk is visible to
        // the next chunk's placements too (vanilla updates the owning chunk's
        // heightmap on every setBlock)
        self.g.note_feature_write(x, y, z, st);
    }
    pub(crate) fn is_air(&mut self, x: i32, y: i32, z: i32) -> bool {
        let st = self.get(x, y, z);
        breg().map(|r| r.is_air(st)).unwrap_or(st == 0)
    }
    pub(crate) fn hm_ws(&mut self, x: i32, z: i32) -> i32 {
        let base = self.g.top_ws(x, z);
        self.g.feature_height(x, z, 0).max(base)
    }
    pub(crate) fn hm_oc(&mut self, x: i32, z: i32) -> i32 {
        let base = self.g.top_oc(x, z);
        self.g.feature_height(x, z, 1).max(base)
    }
    /// `LevelReader.getBiome(BlockPos)`, i.e. `BiomeManager.getBiome` — the
    /// *blended* biome, and the one vanilla's `BiomeFilter` reads. It is not the
    /// chunk's container (`biome_name_at`), and the two disagree at boundaries:
    /// a filter that checks the container rejects or accepts the wrong cells.
    pub(crate) fn biome(&mut self, x: i32, y: i32, z: i32) -> Option<String> {
        match self.g.biome_blended(x, y, z) {
            Some((_, leaf)) => self.g.biome_name_of_leaf(leaf),
            None => self.g.biome_name_at(x, y, z),
        }
    }
    pub(crate) fn out_of_h(&self, y: i32) -> bool {
        y < self.g.min_y || y >= self.g.min_y + self.g.height
    }
}
