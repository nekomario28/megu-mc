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
    rc: HashMap<(i32, i32, i32), u32>, // terrain read-through cache

    cc: Option<(i32, i32, std::sync::Arc<crate::worldgen::ChunkCols>)>,
    prof: bool,
}
impl<'a> FC<'a> {
    pub(crate) fn new(g: &'a mut Gen) -> Self {
        FC {
            g,
            wr: HashMap::new(),
            rc: HashMap::new(),
            cc: None,
            prof: crate::trace_env::env_flag!("FEAT_PROFILE"),
        }
    }
    pub(crate) fn get(&mut self, x: i32, y: i32, z: i32) -> u32 {
        if let Some(&s) = self.wr.get(&(x, y, z)) {
            return s;
        }
        if let Some(&s) = self.rc.get(&(x, y, z)) {
            return s;
        }
        // Carvers run before features and write into the feature layer. A feature
        // reading terrain_cell alone would see the uncarved stone and, for example,
        // place an ore into the reference cave air.
        if let Some(st) = self.g.feature_override(x, y, z) {
            return st;
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
        // heightmap on every setBlock). This layer classifies the state, because
        // the block knowledge sits below worldgen and worldgen may not reach for it.
        let reg = breg();
        let solid = reg.map(|r| r.is_solid(st)).unwrap_or(false);
        let air = reg.map(|r| r.is_air(st)).unwrap_or(st == 0);
        self.g.note_feature_write(x, y, z, solid, air);
    }
    pub(crate) fn is_air(&mut self, x: i32, y: i32, z: i32) -> bool {
        let st = self.get(x, y, z);
        breg().map(|r| r.is_air(st)).unwrap_or(st == 0)
    }
    /// The topmost cell of a column that the heightmap counts, searching down from
    /// `from` and stopping at `base` (the density top, which is solid by
    /// construction, so `base` is returned when nothing above it counts).
    ///
    /// This is not the density top, and that is the point: vanilla maintains
    /// `OCEAN_FLOOR_WG`/`WORLD_SURFACE_WG` from every `setBlock` during terrain, so a
    /// solid the aquifer pushes *above* the noise surface belongs to it. The port's
    /// basis was the density top, which reads the floor too low wherever that
    /// happens - measured on the kelp fixture: cell (1264,33,-16) is stone in the
    /// port's own dump and sand in vanilla's, while the density top is 18, and 329 of
    /// the 9216 columns in that region read the floor below the block that is there.
    /// `fluid` selects the predicate: `OCEAN_FLOOR` counts motion-blocking blocks
    /// only, `WORLD_SURFACE` also counts a fluid cell.
    fn heightmap_above(&mut self, x: i32, z: i32, from: i32, base: i32, fluid: bool) -> i32 {
        let reg = breg();
        let mut y = from;
        while y >= base {
            let st = self.get(x, y, z);
            let counts = reg
                .map(|r| r.is_solid(st) || (fluid && r.is_liquid(st)))
                .unwrap_or(false);
            if counts {
                return y + 1;
            }
            y -= 1;
        }
        base
    }
    pub(crate) fn hm_ws(&mut self, x: i32, z: i32) -> i32 {
        let base = self.g.top_ws(x, z);
        let start = base.max(self.g.sea + 1);
        let terrain = self.heightmap_above(x, z, start, base, true);
        self.g.feature_height(x, z, 0).max(terrain)
    }
    /// The terrain part of `hm_oc`, without the feature layer.
    pub(crate) fn hm_oc_terrain(&mut self, x: i32, z: i32) -> i32 {
        let base = self.g.top_oc(x, z);
        let start = base.max(self.g.sea + 1);
        self.heightmap_above(x, z, start, base, false)
    }
    pub(crate) fn hm_oc(&mut self, x: i32, z: i32) -> i32 {
        let base = self.g.top_oc(x, z);
        let start = base.max(self.g.sea + 1);
        let terrain = self.heightmap_above(x, z, start, base, false);
        self.g.feature_height(x, z, 1).max(terrain)
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

/// `MC_SPINE_WG_DUMP_HM=<path>`: write the OCEAN_FLOOR height of every column of
/// the dumped region, 256 i32 per chunk in the block dump's chunk order.
///
/// Why: the kelp residual was attributed to "the two sides read a different
/// `getHeight(OCEAN_FLOOR)`", and a stored vanilla chunk carries its own heightmap,
/// so the attribution can be checked instead of assumed. It goes through the same
/// accessors a feature reads (`hm_oc`), which is the point: the raw column top and
/// the feature-facing value differ wherever the aquifer pushes a solid above the
/// noise surface.
///
/// `terrain_only` leaves the feature layer out, for telling the two apart.
pub(crate) fn heightmap_dump(path: &str, n: i32, ox: i32, oz: i32, terrain_only: bool) {
    let Some(out) = crate::worldgen::with_gen(|g| {
        let mut fc = FC::new(g);
        let mut out: Vec<u8> = Vec::new();
        for dz in 0..n {
            for dx in 0..n {
                let (cx, cz) = (ox + dx, oz + dz);
                for lz in 0..16 {
                    for lx in 0..16 {
                        let (x, z) = (cx * 16 + lx, cz * 16 + lz);
                        let h = if terrain_only {
                            fc.hm_oc_terrain(x, z)
                        } else {
                            fc.hm_oc(x, z)
                        };
                        out.extend_from_slice(&h.to_le_bytes());
                    }
                }
            }
        }
        out
    }) else {
        return;
    };
    match std::fs::write(path, &out) {
        Ok(()) => eprintln!(
            "[hmdump] {} chunks -> {} ({} bytes)",
            n * n,
            path,
            out.len()
        ),
        Err(e) => eprintln!("[hmdump] {path}: {e}"),
    }
}
