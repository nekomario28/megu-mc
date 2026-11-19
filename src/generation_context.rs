// generation_context.rs — the local generation context: one mutable borrow of
// Gen plus the write-through overrides and caches the placement evaluators see.
// It owns no datapack types; common feature and template tick reservations
// live here without importing higher-level tree or template implementations.

use crate::block_registry::{breg, BReg, HeightmapKind};
use crate::worldgen::Gen;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
#[allow(unused_imports)]
pub(crate) use crate::generation_payload::{ContainerKind, GenerationPayload};

/// Staged container contents, named at this layer so its callers need no new module edge.
pub(crate) type StagedPayloads =
    Vec<((i32, i32, i32), crate::generation_payload::GenerationPayload)>;

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

/// The block-light field of one chunk-promotion pass: cell -> 1..=15, with every other cell 0.
/// Built once by [`FC::begin_promotion_pass`] from the state as of the boundary and reused for
/// every mark of that chunk (see there for why the pass must not relight).
pub(crate) struct PromoLight {
    block: HashMap<(i32, i32, i32), u8>,
}

pub(crate) struct FC<'a> {
    pub(crate) g: &'a mut Gen,
    /// A3-a wave workers borrow the owner read-only for the state a feature placement reads
    /// across chunks (`feat_c`, `carve_c`, the feature heightmap and the built columns). The
    /// serial path has no owner. Every fallback here is read-only: a worker never mutates the
    /// owner, it only commits its own results when the wave ends.
    owner: Option<&'a Gen>,
    pub(crate) wr: HashMap<(i32, i32, i32), u32>,
    /// `ChunkAccess.postProcessing` as it stands after this chunk's own generation:
    /// the write positions `StructurePiece.placeBlock` marked, in mark order. They
    /// are consumed at the end of this chunk's `populate` (see
    /// `feature::populate`), and the ones that belong to *other* chunks are handed
    /// to `Gen` for those chunks' own promotion.
    ///
    /// Two mark sources file here, and they are the two 1.21.1 has for a
    /// `WorldGenRegion` write: `StructurePiece.placeBlock` for
    /// `SHAPE_CHECK_BLOCKS` (`BReg::needs_postprocess_shape`, called by the piece
    /// writer) and `WorldGenRegion.setBlock`'s own `hasPostProcess` gate
    /// (`BReg::has_post_process`, filed by [`FC::set`]). The sets are disjoint.
    pub(crate) shape_marks: Vec<(i32, i32, i32)>,
    rc: HashMap<(i32, i32, i32), u32>, // terrain read-through cache
    /// Frozen pre-CARVERS `*_WG` heightmaps, memoized per column. Feature writes
    /// do not invalidate this terrain-only view. Live (non-WG) variants are never
    /// cached: they follow carver and feature writes.
    hmwg: HashMap<(i32, i32, HeightmapKind), i32>,

    /// Generated entity and block-entity data, not a live store. The runtime
    /// handoff routes each payload to its owner after releasing the generator.
    /// Durable source claims suppress replay when block caches are recomputed.
    pub(crate) payloads: Vec<((i32, i32, i32), crate::generation_payload::GenerationPayload)>,

    cc: Option<(i32, i32, std::sync::Arc<crate::worldgen::ChunkCols>)>,
    /// The block-light field of the chunk-promotion pass that is running, or `None`
    /// outside one (the sky term is computed per query from the column). Sampled once per promoted chunk, before the pass rewrites
    /// anything, because `postProcessGeneration` writes with
    /// `UPDATE_INVISIBLE | UPDATE_KNOWN_SHAPE` (`20`) and does not relight the chunk.
    promo_light: Option<PromoLight>,
    prof: bool,
    #[cfg(test)]
    pub(crate) test_no_terrain: bool,
    #[cfg(test)]
    pub(crate) test_default_air: bool,
    #[cfg(test)]
    pub(crate) test_biome_leaf: Option<u16>,
    #[cfg(test)]
    pub(crate) test_block_light: u8,
    #[cfg(test)]
    pub(crate) test_writes: Option<Vec<((i32, i32, i32), u32)>>,
}
impl<'a> FC<'a> {
    pub(crate) fn new(g: &'a mut Gen) -> Self {
        FC {
            g,
            owner: None,
            wr: HashMap::new(),
            shape_marks: Vec::new(),
            rc: HashMap::new(),
            hmwg: HashMap::new(),
            payloads: Vec::new(),
            cc: None,
            promo_light: None,
            prof: crate::trace_env::env_flag!("FEAT_PROFILE"),
            #[cfg(test)]
            test_no_terrain: false,
            #[cfg(test)]
            test_default_air: false,
            #[cfg(test)]
            test_biome_leaf: None,
            #[cfg(test)]
            test_block_light: 0,
            #[cfg(test)]
            test_writes: None,
        }
    }

    /// A wave worker's context: its own scratch generator for writes, the owner for the
    /// read-only cross-chunk state the placement loop needs.
    pub(crate) fn new_worker(g: &'a mut Gen, owner: &'a Gen) -> Self {
        let mut ctx = FC::new(g);
        ctx.owner = Some(owner);
        ctx
    }

    /// The owner-side continuation of a wave worker: the worker's writes are installed on the
    /// owner and the same promotion boundary as the serial arm runs here, in population order.
    pub(crate) fn new_commit(
        g: &'a mut Gen,
        wr: HashMap<(i32, i32, i32), u32>,
        shape_marks: Vec<(i32, i32, i32)>,
        payloads: Vec<((i32, i32, i32), crate::generation_payload::GenerationPayload)>,
    ) -> Self {
        let mut ctx = FC::new(g);
        ctx.wr = wr;
        ctx.shape_marks = shape_marks;
        ctx.payloads = payloads;
        ctx
    }

    /// `feature_override` through the worker's own layer first, then the owner's. A worker's
    /// own `feat_c` is empty (it never calls `feat_write`), so in practice this is the owner's
    /// carve + earlier-chunk feature writes.
    fn feature_override(&self, x: i32, y: i32, z: i32) -> Option<u32> {
        self.g
            .feature_override(x, y, z)
            .or_else(|| self.owner.and_then(|o| o.feature_override(x, y, z)))
    }

    fn wg_after_carve_height(&self, ocean_floor: bool, x: i32, z: i32) -> Option<i32> {
        self.g.wg_after_carve_height(ocean_floor, x, z).or_else(|| {
            self.owner
                .and_then(|o| o.wg_after_carve_height(ocean_floor, x, z))
        })
    }

    /// The live heightmap scan bound: the maximum of the worker's own writes and the owner's
    /// earlier writes. Wave windows are disjoint, so a column is at most in one of them.
    fn feature_height(&self, x: i32, z: i32, solid: usize) -> i32 {
        let own = self.g.feature_height(x, z, solid);
        match self.owner {
            Some(o) => own.max(o.feature_height(x, z, solid)),
            None => own,
        }
    }

    fn prefetched_cell(&self, x: i32, y: i32, z: i32) -> Option<u32> {
        self.g.prefetched_cell(x, y, z).or_else(|| {
            self.owner
                .and_then(|o| o.prefetched_cell(x, y, z))
        })
    }

    /// Columns for a terrain read: the worker's cache, then the owner's already-built columns
    /// (A3-a's batch pre-carve leaves them resident), finally a cold build on the worker.
    fn cols_for(&mut self, cx: i32, cz: i32) -> Option<std::sync::Arc<crate::worldgen::ChunkCols>> {
        if let Some(o) = self.owner {
            if let Some(c) = o.col_c_get(cx, cz) {
                return Some(c);
            }
        }
        self.g.cols(cx, cz)
    }

    pub(crate) fn push_loot_container(
        &mut self,
        pos: (i32, i32, i32),
        kind: crate::generation_payload::ContainerKind,
        loot_table: String,
        loot_seed: Option<i64>,
    ) {
        self.payloads.push((
            pos,
            crate::generation_payload::GenerationPayload::LootContainer {
                kind,
                loot_table,
                loot_seed,
            },
        ));
    }
    pub(crate) fn push_spawner(&mut self, pos: (i32, i32, i32), entity_id: &str) {
        self.payloads.push((
            pos,
            crate::generation_payload::GenerationPayload::SpawnerEntity(entity_id.to_owned()),
        ));
    }
    pub(crate) fn push_entity(&mut self, id: &str, pos: [f64; 3], yaw: f32) {
        let block_pos = (
            pos[0].floor() as i32,
            pos[1].floor() as i32,
            pos[2].floor() as i32,
        );
        self.payloads.push((
            block_pos,
            crate::generation_payload::GenerationPayload::Entity {
                id: id.to_owned(),
                pos,
                yaw,
            },
        ));
    }
    /// `StructurePiece.placeBlock`'s `chunk.markPosForPostprocessing(pos)` for a state
    /// in `StructurePiece.SHAPE_CHECK_BLOCKS`. The owning chunk is `pos >> 4`; the mark
    /// is only consumed while that chunk is still generating.
    pub(crate) fn mark_shape(&mut self, x: i32, y: i32, z: i32) {
        self.shape_marks.push((x, y, z));
    }
    /// A hanging entity: `pos` is the block it hangs in. `HangingEntity` derives its
    /// own position from that block and its facing, so the payload keeps the block
    /// anchor the marker or template named instead of a computed entity position.
    pub(crate) fn push_item_frame(
        &mut self,
        pos: (i32, i32, i32),
        glow: bool,
        facing: u8,
        item: Option<(String, u8)>,
    ) {
        self.payloads.push((
            pos,
            crate::generation_payload::GenerationPayload::ItemFrame { glow, facing, item },
        ));
    }
    pub(crate) fn get(&mut self, x: i32, y: i32, z: i32) -> u32 {
        if let Some(&s) = self.wr.get(&(x, y, z)) {
            return s;
        }
        if let Some(&s) = self.rc.get(&(x, y, z)) {
            // M3 P1a M3: answered from this pass' own terrain read-through cache.
            crate::worldgen::m3_inc(crate::worldgen::M3_RC_HIT);
            return s;
        }
        // Carvers run before features and write into the feature layer. A feature
        // reading terrain_cell alone would see the uncarved stone and, for example,
        // place an ore into the reference cave air.
        if let Some(st) = self.feature_override(x, y, z) {
            return st;
        }
        #[cfg(test)]
        if self.test_default_air {
            return 0;
        }
        #[cfg(test)]
        assert!(
            !self.test_no_terrain,
            "unprovided scripted world input at {x},{y},{z}"
        );
        let t0 = if self.prof {
            Some(std::time::Instant::now())
        } else {
            None
        };
        // A worker may already have filled this chunk's section: the landed base row is the
        // same answer `terrain_cell` would compute, without the cold memo behind it (see
        // `Gen::prefetched_cell`).
        //
        // M3 P1a M3: the scope says the terrain memos read below are being read *for a
        // feature*, not by the fill that warms them, so `deep_c`/`aq_lv`/`prelim_c` hits and
        // misses are attributed to the feature layer's cold reads.
        let st = {
            let _read = crate::worldgen::M3FeatureRead::enter();
            match self.prefetched_cell(x, y, z) {
                Some(st) => st,
                None => {
                    let (cx, cz) = (x >> 4, z >> 4);
                    if !matches!(&self.cc, Some((a, b, _)) if *a == cx && *b == cz) {
                        match self.cols_for(cx, cz) {
                            Some(c) => self.cc = Some((cx, cz, c)),
                            None => return 0,
                        }
                    }
                    match &self.cc {
                        Some((_, _, c)) => self.g.terrain_cell(c, x, y, z),
                        None => 0,
                    }
                }
            }
        };
        if let Some(t0) = t0 {
            READ_NS.fetch_add(t0.elapsed().as_nanos() as u64, Ordering::Relaxed);
            READ_N.fetch_add(1, Ordering::Relaxed);
        }
        self.rc.insert((x, y, z), st);
        st
    }
    /// `FC::get` with the provenance of the answer: `wr` (a feature write), `rc` (a cached
    /// terrain read), `ovr` (the carver layer) or `terrain` (a fresh `terrain_cell`). Used by
    /// `CEL_COLUMN_TRACE`: a column read that disagrees with the same chunk's dump has to say
    /// *which* layer answered before it can be attributed.
    ///
    /// The tag is decided first and the value is then read through `get`, so this cannot
    /// drift from the read it annotates.
    pub(crate) fn get_src(&mut self, x: i32, y: i32, z: i32) -> (u32, &'static str) {
        let tag = if self.wr.contains_key(&(x, y, z)) {
            "wr"
        } else if self.rc.contains_key(&(x, y, z)) {
            "rc"
        } else if self.feature_override(x, y, z).is_some() {
            "ovr"
        } else {
            "terrain"
        };
        (self.get(x, y, z), tag)
    }
    pub(crate) fn set(&mut self, x: i32, y: i32, z: i32, st: u32) {
        // Refused before anything about this write is recorded (the override map, the
        // feature heightmap tally and the writer's own later reads), because vanilla's
        // `WorldGenRegion.ensureCanWrite` drops the write and reports only a log line.
        // See `Gen::feature_write_allowed` for the window and for the chunk-level
        // `ImposterProtoChunk.allowWrites` refusal that is deliberately not modelled;
        // either way `WorldGenRegion.setBlock` still returns true, so a
        // `FCELL ... wrote=true` trace line proves nothing about persistence
        // (tools/carve-agent/README.md).
        if !self.g.feature_write_allowed(x, z) {
            if crate::trace_env::env_str!("FEATURE_REFUSE_TRACE")
                .is_some_and(|want| want == "all" || want == format!("{},{}", x >> 4, z >> 4))
            {
                let pop = self.g.pop_active;
                eprintln!(
                    "FREFUSE {x} {y} {z} state={st} pop={},{}",
                    pop.map(|p| p.0).unwrap_or(i32::MIN),
                    pop.map(|p| p.1).unwrap_or(i32::MIN)
                );
            }
            return;
        }
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
        // `WorldGenRegion.setBlock`'s last two statements: the state that was actually
        // written is asked `hasPostProcess`, and if it answers true the *owning* chunk
        // (`getChunk(pos)`, not the chunk being generated) files the cell with
        // `markPosForPostprocessing`. The gate is on the state written, and the mark is
        // filed after the write, so a refused write marks nothing.
        //
        // Marked only inside a FEATURES window: a `WorldGenRegion` exists only there, and
        // the promotion pass itself writes to a `LevelChunk` (outside this function) - a
        // mark filed by the pass would be a mark nothing consumes or a second pass over an
        // already-promoted chunk.
        if self.g.pop_active.is_some() && reg.is_some_and(|r| r.has_post_process(st)) {
            self.shape_marks.push((x, y, z));
        }
    }
    pub(crate) fn is_air(&mut self, x: i32, y: i32, z: i32) -> bool {
        let st = self.get(x, y, z);
        breg().map(|r| r.is_air(st)).unwrap_or(st == 0)
    }
    /// Read the first available Y using the native type's state predicate.
    /// WG maps freeze when the carver pass ends (see `Gen::wg_after_carve`) and this
    /// scan is the surface-stage fallback for chunks that have no carver snapshot;
    /// live maps also see carver/feature writes.
    /// Aquifers can push a solid above the density top (329/9216 columns on the
    /// kelp fixture), so the density top alone is not a heightmap. Conversely,
    /// a removed top must be scanned down, not retained as a max-only result.
    fn heightmap_above(&mut self, x: i32, z: i32, from: i32, kind: HeightmapKind) -> i32 {
        let reg = breg();
        let terrain = if kind.is_wg() {
            self.cols_for(x >> 4, z >> 4)
        } else {
            None
        };
        let prof = self.prof;
        if prof && kind.is_wg() {
            HM_SCAN_N.fetch_add(1, Ordering::Relaxed);
        }
        // A heightmap is a property of the chunk, so its scan bound and its "nothing
        // found" answer are the **world** window, not the generator's: the End's
        // `WORLD_SURFACE` for the tower chunk is 140, above the top of its terrain.
        let mut y = from.min(self.g.world_min_y + self.g.world_height - 1);
        while y >= self.g.world_min_y {
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
        self.g.world_min_y
    }
    pub(crate) fn heightmap(&mut self, kind: HeightmapKind, x: i32, z: i32) -> i32 {
        if kind.is_wg() {
            // The native WG maps are as of the end of the carver pass, not the surface:
            // the carver writes land while the chunk still reports the SURFACE status,
            // whose `heightmapsAfter` is exactly these two maps. Feature and structure
            // writes (the FEATURES status) never move them, so the answer is neither the
            // pre-carve terrain nor a live scan.
            if let Some(v) = self.wg_after_carve_height(
                matches!(kind, HeightmapKind::OceanFloorWg),
                x,
                z,
            ) {
                return v;
            }
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
    #[cfg(test)]
    pub(crate) fn test_biome(&mut self, name: &str) {
        let leaf = self
            .g
            .biome_leaf_of_name(name)
            .unwrap_or_else(|| panic!("unknown biome {name}"));
        self.test_biome_leaf = Some(leaf);
    }
    fn heightmap_uncached(&mut self, kind: HeightmapKind, x: i32, z: i32) -> i32 {
        #[cfg(test)]
        if self.test_default_air {
            let start = self
                .rc
                .keys()
                .chain(self.wr.keys())
                .filter(|&&(kx, _, kz)| kx == x && kz == z)
                .map(|&(_, ky, _)| ky)
                .max()
                .unwrap_or(self.g.min_y - 1);
            return self.heightmap_above(x, z, start, kind);
        }
        let oc = matches!(
            kind,
            HeightmapKind::OceanFloor | HeightmapKind::OceanFloorWg
        );
        let base = if oc {
            self.g.top_oc(x, z)
        } else {
            self.g.top_ws(x, z)
        };
        let mut start = base.max(self.g.sea + 1);
        if !kind.is_wg() {
            // The tracked maxima are scan bounds, not answers: a later writer
            // may have removed that cell or a different predicate may reject it.
            start = start.max(self.feature_height(x, z, 0));
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
        #[cfg(test)]
        if let Some(leaf) = self.test_biome_leaf {
            return self.g.biome_name_of_leaf(leaf);
        }
        match self.g.biome_blended(x, y, z) {
            Some((_, leaf)) => self.g.biome_name_of_leaf(leaf),
            None => self.g.biome_name_at(x, y, z),
        }
    }
    pub(crate) fn biome_leaf(&mut self, x: i32, y: i32, z: i32) -> Option<u16> {
        #[cfg(test)]
        if let Some(leaf) = self.test_biome_leaf {
            return Some(leaf);
        }
        self.g
            .biome_blended(x, y, z)
            .map(|(_, leaf)| leaf)
            .or_else(|| {
                self.g
                    .biome_quart(x >> 2, y >> 2, z >> 2, None)
                    .map(|(_, leaf)| leaf)
            })
    }
    pub(crate) fn cold_enough_to_snow(&self, leaf: u16, x: i32, y: i32, z: i32) -> bool {
        self.g.biome_cold_at_leaf(leaf, x, y, z)
    }
    pub(crate) fn block_light(&self, _x: i32, _y: i32, _z: i32) -> u8 {
        #[cfg(test)]
        return self.test_block_light;
        #[cfg(not(test))]
        0
    }

    /// The chunk-promotion boundary: sample the light the pass reads, once.    ///
    /// 1.21.1 consumes `postProcessGeneration`'s marks when the proto chunk is promoted
    /// (`ChunkMap`'s `prepareTickingChunk` lambda, its first statement), which is after
    /// `INITIALIZE_LIGHT`/`LIGHT` have run - so `BushBlock.updateShape` there re-asks
    /// `MushroomBlock.canSurvive` against a **lit** engine while the same check at `FEATURES`
    /// time read the uninitialized one. The port has no light stage, so this samples the two
    /// layers the check can see, at the boundary and not before it:
    ///
    /// * block light: a multi-source flood from every state in the promoted chunk's 3x3 with
    ///   `getLightEmission() > 0`, attenuating `max(1, getLightBlock())` per entered cell
    ///   (`LayerLightEngine`'s rule). Sources are enumerated from the write layers (the
    ///   in-flight chunk's `wr` and the neighbouring `feat_c` buckets), because those are the
    ///   only cells a generation-time light source can be in without a full terrain scan; the
    ///   flood's *reads* still go through [`FC::get`], so terrain occludes and terrain lava is
    ///   not skipped once the flood reaches it.
    /// * sky light: [`FC::sky_light`], and only in a dimension with a sky layer.
    ///
    /// Sampled **before** the pass rewrites anything and reused for every mark of the promoted
    /// chunk. `postProcessGeneration` sets with flags `20` (`UPDATE_INVISIBLE |
    /// UPDATE_KNOWN_SHAPE`), which carries no light update, so a mark must not change the light
    /// a later mark of the same chunk reads; that is also what keeps the pass independent of
    /// mark order.
    ///
    /// Only sampled when a mark is itself a `hasPostProcess` block: nothing else in the pass
    /// reads light, and the flood must not be paid by every chunk that only carries a fence.
    pub(crate) fn begin_promotion_pass(&mut self, cx: i32, cz: i32, marks: &[(i32, i32, i32)]) {
        self.promo_light = None;
        let Some(reg) = breg() else { return };
        let mut needed = false;
        for &(x, y, z) in marks {
            if reg.has_post_process(self.get(x, y, z)) {
                needed = true;
                break;
            }
        }
        if needed {
            let field = self.sample_block_light(cx, cz, reg);
            self.promo_light = Some(field);
        }
    }

    /// End the promotion pass: drop the sample, so nothing after the boundary (a later
    /// chunk's pass, a feature-time read) can answer from a field sampled for a chunk that
    /// is already promoted.
    pub(crate) fn end_promotion_pass(&mut self) {
        self.promo_light = None;
    }

    fn sample_block_light(&mut self, cx: i32, cz: i32, reg: &BReg) -> PromoLight {
        let mut sources: Vec<((i32, i32, i32), u8)> = Vec::new();
        for (&pos, &st) in self.wr.iter() {
            let l = reg.luminance_of(st);
            if l > 0 {
                sources.push((pos, l));
            }
        }
        for dx in -1..=1 {
            for dz in -1..=1 {
                for (pos, st) in self.g.feature_bucket_writes(cx + dx, cz + dz) {
                    let l = reg.luminance_of(st);
                    if l > 0 {
                        sources.push((pos, l));
                    }
                }
            }
        }
        // Max-propagation is a fixed point, so the seed order cannot change the answer; the
        // sort is for the *trace*: two runs of the same chunk must flood in the same order.
        sources.sort_unstable();
        let mut block: HashMap<(i32, i32, i32), u8> = HashMap::new();
        let mut queue: std::collections::VecDeque<((i32, i32, i32), u8)> =
            std::collections::VecDeque::new();
        for (pos, l) in sources {
            if block.get(&pos).copied().unwrap_or(0) < l {
                block.insert(pos, l);
                queue.push_back((pos, l));
            }
        }
        while let Some((pos, v)) = queue.pop_front() {
            if v < 2 {
                continue;
            }
            for (dx, dy, dz) in [
                (0i32, -1i32, 0i32),
                (0, 1, 0),
                (0, 0, -1),
                (0, 0, 1),
                (-1, 0, 0),
                (1, 0, 0),
            ] {
                let n = (pos.0 + dx, pos.1 + dy, pos.2 + dz);
                if self.out_of_h(n.1) {
                    continue;
                }
                let cost = reg.opacity_of(self.get(n.0, n.1, n.2)).max(1);
                let Some(nv) = v.checked_sub(cost) else {
                    continue;
                };
                if nv >= 1 && block.get(&n).copied().unwrap_or(0) < nv {
                    block.insert(n, nv);
                    queue.push_back((n, nv));
                }
            }
        }
        PromoLight { block }
    }

    /// `LevelReader.getRawBrightness(pos, 0)` (`dcz.b(jd,int)`) as of the promotion boundary,
    /// i.e. `max(blockLight, skyLight - 0)`.
    ///
    /// `None` outside a promotion pass. A feature-time caller has no `INITIALIZE_LIGHT`/`LIGHT`
    /// result to read - the uninitialized answer is `generation_providers::uninitialized_raw_brightness`
    /// - and a shape rule must say "no lit engine here" rather than invent one.
    pub(crate) fn raw_brightness(&mut self, x: i32, y: i32, z: i32) -> Option<u8> {
        let block = match &self.promo_light {
            Some(field) => field.block.get(&(x, y, z)).copied().unwrap_or(0),
            None => return None,
        };
        if !self.g.has_skylight() {
            return Some(block);
        }
        let sky = self.sky_light(x, y, z);
        Some(block.max(sky))
    }

    /// The sky term of [`FC::raw_brightness`]: 15 minus the light the column above the cell
    /// stops, capped at 15 (`15 - sum(getLightBlock)`), which is the value `SkyLightSectionStorage`
    /// answers for a cell that sees the sky straight up. A model, not a light engine: it does not
    /// spread sideways from a neighbouring lit column, which is the one thing the shape rules in
    /// this pass do not need (a mushroom under a canopy reads its own column).
    fn sky_light(&mut self, x: i32, y: i32, z: i32) -> u8 {
        let Some(reg) = breg() else { return 15 };
        let top = self.g.world_min_y + self.g.world_height - 1;
        let mut stopped: u16 = 0;
        let mut yy = y + 1;
        while yy <= top {
            stopped += reg.opacity_of(self.get(x, yy, z)) as u16;
            if stopped >= 15 {
                return 0;
            }
            yy += 1;
        }
        15 - stopped as u8
    }
    pub(crate) fn out_of_h(&self, y: i32) -> bool {
        // `Level.isOutsideBuildHeight`: the *world* window. A feature may legitimately
        // write above the generator top (the End's end_city does), and
        // `BlockPlacement`/`WorldGenRegion` refuse by the level's build height.
        y < self.g.world_min_y || y >= self.g.world_min_y + self.g.world_height
    }
}

/// `MC_SPINE_HM_DUMP=cx,cz`: print, for every column of one chunk, the three live
/// heightmaps a feature can read - `OCEAN_FLOOR`, `WORLD_SURFACE` and `MOTION_BLOCKING` -
/// followed by the same values for `MOTION_BLOCKING_NO_LEAVES`.
///
/// Why it lives here and not in `worldgen`: the answer is what `FC::heightmap` returns,
/// i.e. the live scan `heightmap_above` makes with the native `Heightmap.Types` predicate
/// (`BReg::heightmap_counts`) over the *feature-facing* block view (`FC::get`: writes,
/// then the carver layer, then terrain). `worldgen` may not reach for the generation
/// context, and computing the answer from the column tops plus `Gen::feature_height`
/// published the scan bound instead of the answer - a max-only tally that cannot see a
/// removal, a carver write, or a state its predicate rejects (a leaf above the floor).
///
/// The provenance of each number matters for a comparison against a saved chunk:
/// `--port-hm` in `tools/port_triage.py` dumps `OCEAN_FLOOR` the same way, and a saved
/// vanilla chunk's `Heightmaps` were maintained by `Heightmap.setBlock` on the very same
/// predicates (`dyy.a`), so the two are the same quantity.
pub(crate) fn hm_dump(spec: &str, populate: fn(&mut Gen, i32, i32)) {
    let p: Vec<i32> = spec.split(',').filter_map(|n| n.parse().ok()).collect();
    if p.len() != 2 {
        return;
    }
    let (cx, cz) = (p[0], p[1]);
    crate::worldgen::with_gen(|g| {
        // The feature layer's height a *running* world holds only exists once the chunks
        // around the column have been populated; reading before that returns terrain
        // heights and silently "proves" a difference.
        g.ensure_populated(cx, cz, populate);
        if g.cols(cx, cz).is_none() {
            return;
        }
        let mut fc = FC::new(g);
        for lz in 0..16i32 {
            for lx in 0..16i32 {
                let (x, z) = (cx * 16 + lx, cz * 16 + lz);
                let oc = fc.hm_oc(x, z);
                let ws = fc.hm_ws(x, z);
                let mb = fc.heightmap(HeightmapKind::MotionBlocking, x, z);
                let mbnl = fc.heightmap(HeightmapKind::MotionBlockingNoLeaves, x, z);
                eprintln!("HM {lx} {lz} {oc} {ws} {mb} {mbnl}");
            }
        }
    });
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
