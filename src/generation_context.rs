// generation_context.rs — the local generation context: one mutable borrow of
// Gen plus the write-through overrides and caches the placement evaluators see.
// It owns no datapack types; common feature and template tick reservations
// live here without importing higher-level tree or template implementations.

use crate::block_registry::{breg, BReg, HeightmapKind};
use crate::worldgen::Gen;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

/// FEAT_PROFILE diagnostic: time spent in feature-side terrain reads.
pub(crate) static READ_NS: AtomicU64 = AtomicU64::new(0);
pub(crate) static READ_N: AtomicU64 = AtomicU64::new(0);
/// FEAT_PROFILE: WG-heightmap work. `HM_SCAN_N` counts computed columns (memo misses),
/// `HM_SCAN_CELLS` the `terrain_cell` evaluations those scans walked through. Measured
/// 2026-10-27: 43,048 scans / 141,530 cells over 25 populated chunks.
pub(crate) static HM_SCAN_N: AtomicU64 = AtomicU64::new(0);
pub(crate) static HM_SCAN_CELLS: AtomicU64 = AtomicU64::new(0);

/// The native LeavesBlock.updateShape scheduling contract used by both
/// tree finalization and the bounded solid-template edge visitor. It reserves
/// a tick rather than immediately changing leaf distance. Keep the existing
/// native tree-edge ordering/deduplication tests as the regression oracle.
/// Shared at the generation-context layer to avoid a template -> tree dependency.
pub(crate) fn schedule_leaf_edge_state(
    ctx: &mut FC<'_>,
    reg: &BReg,
    cell: (i32, i32, i32),
    st: u32,
    neighbor: u32,
) {
    let name = reg.name_of(st);
    if !name.ends_with("_leaves") {
        return;
    }
    let Some(distance) = reg
        .get_prop(st, "distance")
        .and_then(|v| v.parse::<u8>().ok())
    else {
        return;
    };
    let next = reg.distance_from_log(neighbor).unwrap_or(7) + 1;
    if next != 1 || distance != 1 {
        ctx.g
            .schedule_generated_leaf(cell, reg.state_of(name, None));
    }
}


// ---------- generation context ----------

/// The block-entity payloads a template can carry today, decoded at admission.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum StagedBlockEntity {
    /// `Items`: slot, item id, count, plus the store that owns them.
    Container { kind: ContainerKind, slots: Vec<(i16, i64, u8)> },
    /// A sign side: the four lines of its front text.
    Sign([String; 4]),
}

/// Which container store owns a placed container's contents.
///
/// The port keeps one store per menu, so a furnace's `Items` written into `chests()` would
/// be invisible: the furnace window reads `furnaces()`. The kind is decided from the placed
/// block state at admission, and an unrecognized container is refused rather than staged
/// where nothing will read it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum ContainerKind {
    Chest,
    Furnace,
}

pub(crate) struct FC<'a> {
    pub(crate) g: &'a mut Gen,
    pub(crate) wr: HashMap<(i32, i32, i32), u32>,
    rc: HashMap<(i32, i32, i32), u32>, // terrain read-through cache
    /// Frozen pre-CARVERS `*_WG` heightmaps, memoized per column. Feature writes
    /// do not invalidate this terrain-only view. Live (non-WG) variants are never
    /// cached: they follow carver and feature writes.
    hmwg: HashMap<(i32, i32, HeightmapKind), i32>,

    /// Block-entity data a template placed with NBT. Generation-owned staging, not a live
    /// store: the runtime handoff writes each variant into the one store that owns it
    /// (`chests()` / `signs()`), so a port that only staged this would still have empty
    /// containers and blank signs in game.
    pub(crate) block_entities: Vec<((i32, i32, i32), StagedBlockEntity)>,

    cc: Option<(i32, i32, std::sync::Arc<crate::worldgen::ChunkCols>)>,
    prof: bool,
    #[cfg(test)]
    pub(crate) test_no_terrain: bool,
    #[cfg(test)]
    pub(crate) test_writes: Option<Vec<((i32, i32, i32), u32)>>,
}
impl<'a> FC<'a> {
    pub(crate) fn new(g: &'a mut Gen) -> Self {
        FC {
            g,
            wr: HashMap::new(),
            rc: HashMap::new(),
            hmwg: HashMap::new(),
            block_entities: Vec::new(),
            cc: None,
            prof: crate::trace_env::env_flag!("FEAT_PROFILE"),
            #[cfg(test)]
            test_no_terrain: false,
            #[cfg(test)]
            test_writes: None,
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
        #[cfg(test)]
        assert!(
            !self.test_no_terrain,
            "unprovided scripted world input at {x},{y},{z}"
        );
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
        #[cfg(test)]
        if let Some(writes) = &mut self.test_writes {
            writes.push(((x, y, z), st));
        }
        if crate::trace_env::env_str!("FEATURE_CELL_TRACE")
            .is_some_and(|want| want.split(';').any(|cell| cell == format!("{x},{y},{z}")))
        {
            let f = crate::generation_math::XTRACE_FEAT.load(Ordering::Relaxed);
            eprintln!(
                "FCELL {x} {y} {z} state={st} feature={},{}",
                f >> 32,
                f & 0xFFFF
            );
        }
        // `FEATURE_WRITE_TRACE=<cx>,<cz>`: every feature write in that chunk, in the order it
        // happened. A per-cell filter cannot answer "which write diverged first", and a digest
        // cannot either - the order is the only place the first difference is visible.
        if crate::trace_env::env_str!("FEATURE_WRITE_TRACE")
            .is_some_and(|want| want == "all" || want == format!("{},{}", x >> 4, z >> 4))
        {
            let f = crate::generation_math::XTRACE_FEAT.load(Ordering::Relaxed);
            let c = crate::generation_math::CUR_POP_CHUNK.load(Ordering::Relaxed);
            eprintln!(
                "FWRITE {x} {y} {z} state={st} feature={},{} pop={},{}",
                f >> 32,
                f & 0xFFFF,
                c >> 32,
                c & 0xFFFF_FFFF
            );
        }
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
    /// Read the first available Y using the native type's state predicate.
    /// WG maps freeze before CARVERS; live maps also see carver/feature writes.
    /// Aquifers can push a solid above the density top (329/9216 columns on the
    /// kelp fixture), so the density top alone is not a heightmap. Conversely,
    /// a removed top must be scanned down, not retained as a max-only result.
    fn heightmap_above(&mut self, x: i32, z: i32, from: i32, kind: HeightmapKind) -> i32 {
        let reg = breg();
        let terrain = if kind.is_wg() {
            self.g.cols(x >> 4, z >> 4)
        } else {
            None
        };
        let prof = self.prof;
        if prof && kind.is_wg() {
            HM_SCAN_N.fetch_add(1, Ordering::Relaxed);
        }
        let mut y = from.min(self.g.min_y + self.g.height - 1);
        while y >= self.g.min_y {
            let st = if kind.is_wg() {
                terrain
                    .as_ref()
                    .map_or(0, |c| self.g.terrain_cell(c, x, y, z))
            } else {
                self.get(x, y, z)
            };
            if prof && kind.is_wg() {
                HM_SCAN_CELLS.fetch_add(1, Ordering::Relaxed);
            }
            if reg.is_some_and(|r| r.heightmap_counts(st, kind)) {
                return y + 1;
            }
            y -= 1;
        }
        self.g.min_y
    }
    pub(crate) fn heightmap(&mut self, kind: HeightmapKind, x: i32, z: i32) -> i32 {
        if kind.is_wg() {
            if let Some(&v) = self.hmwg.get(&(x, z, kind)) {
                return v;
            }
            #[cfg(test)]
            assert!(
                !self.test_no_terrain,
                "unprovided frozen height input at {x},{z}: {kind:?}"
            );
            let v = self.heightmap_uncached(kind, x, z);
            self.hmwg.insert((x, z, kind), v);
            return v;
        }
        self.heightmap_uncached(kind, x, z)
    }
    /// Initial scripted cells use the existing read-through cache, below writes.
    #[cfg(test)]
    pub(crate) fn test_base_cell(&mut self, pos: (i32, i32, i32), state: u32) {
        self.rc.insert(pos, state);
    }
    /// Frozen stage inputs for scripted native-oracle worlds. No live-map override.
    #[cfg(test)]
    pub(crate) fn test_frozen_height(&mut self, kind: HeightmapKind, x: i32, z: i32, y: i32) {
        assert!(kind.is_wg());
        self.hmwg.insert((x, z, kind), y);
    }
    fn heightmap_uncached(&mut self, kind: HeightmapKind, x: i32, z: i32) -> i32 {
        let ws = matches!(
            kind,
            HeightmapKind::WorldSurface | HeightmapKind::WorldSurfaceWg
        );
        let base = if ws {
            self.g.top_ws(x, z)
        } else {
            self.g.top_oc(x, z)
        };
        let mut start = base.max(self.g.sea + 1);
        if !kind.is_wg() {
            // The tracked maxima are scan bounds, not answers: a later writer
            // may have removed that cell or a different predicate may reject it.
            start = start.max(self.g.feature_height(x, z, 0));
        }
        self.heightmap_above(x, z, start, kind)
    }
    pub(crate) fn hm_ws(&mut self, x: i32, z: i32) -> i32 {
        self.heightmap(HeightmapKind::WorldSurface, x, z)
    }
    /// Live reads, but without extending the scan bound to feature maxima.
    /// Kept for the terrain-vs-feature heightmap diagnostic and kelp probes.
    pub(crate) fn hm_oc_terrain(&mut self, x: i32, z: i32) -> i32 {
        let base = self.g.top_oc(x, z);
        self.heightmap_above(x, z, base.max(self.g.sea + 1), HeightmapKind::OceanFloor)
    }
    pub(crate) fn hm_oc_wg(&mut self, x: i32, z: i32) -> i32 {
        self.heightmap(HeightmapKind::OceanFloorWg, x, z)
    }
    pub(crate) fn hm_oc(&mut self, x: i32, z: i32) -> i32 {
        self.heightmap(HeightmapKind::OceanFloor, x, z)
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
