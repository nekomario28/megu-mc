// generation_context.rs — the local generation context: one mutable borrow of
// Gen plus the write-through overrides and caches the placement evaluators see.
// It owns no datapack types; common feature and template tick reservations
// live here without importing higher-level tree or template implementations.

use crate::block_registry::{breg, BReg, HeightmapKind};
use crate::worldgen::{CoordMap, Gen};
use std::cell::OnceCell;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
#[allow(unused_imports)]
pub(crate) use crate::generation_payload::{ContainerKind, GenerationPayload};

/// Staged container contents, named at this layer so its callers need no new module edge.
pub(crate) type StagedPayloads =
    Vec<((i32, i32, i32), crate::generation_payload::GenerationPayload)>;

type PopulationOrder<'a> = (&'a HashMap<(i32, i32), usize>, usize);
#[cfg(test)]
type TestBlockWrites = Vec<((i32, i32, i32), u32)>;

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

// ---------- flat chunk-window write buffer ----------

/// The marker a dense section cell carries before its first write. No real
/// block state is `u32::MAX`; a write that literally arrives with it is routed
/// to the overflow so the sentinel keeps a single meaning.
const WR_EMPTY: u32 = u32::MAX;
/// Dense `y` band: 24 16-block sections covering `[-64, 320)`, the union of the
/// 1.21.1 generators' build windows. Writes above or below it land in the
/// overflow, which is what keeps tall custom dimensions equivalent.
const WR_MIN_Y: i32 = -64;
const WR_Y_SECTIONS: usize = 24;
/// 3x3 chunks times the `y` sections: the full dense index space.
const WR_SECTIONS: usize = 9 * WR_Y_SECTIONS;

/// The block-state write buffer behind [`FC::wr`].
///
/// Feature population writes almost every cell inside the 3x3 chunk window
/// `feature_write_allowed` enforces (the `pop_active` chunk plus its eight
/// neighbours), and `y` stays inside a few dozen 16-block sections. A
/// `HashMap` pays a SipHash plus a probe for each of the ~60k writes a
/// 9-chunk pass makes; this map resolves an in-window `(x, y, z)` to one flat
/// array index instead: 9 chunks x 24 sections of 4096 cells, each section a
/// boxed array allocated only once a write touches it, so untouched `y`
/// bands cost eight bytes and hot ore/cave/tree sections do O(1) indexed
/// reads and writes with zero hashing.
///
/// Coordinates outside the window, outside `[-64, 320)` on `y`, or any write
/// before the window anchored fall back to `overflow`, so every key a
/// `HashMap` would hold is held here with the same observable behaviour:
/// `get`, `contains_key`, `len`, `iter` and `PartialEq` agree on the key
/// *set*, which is all any consumer may rely on - the previous map's
/// iteration order was hasher-random per process, so no reader can depend on
/// order, only on membership.
///
/// `order` stores every live key once, in first-write order. Iteration must
/// hand out `(&key, &value)` pairs (callers pattern-match `(&pos, &st)`) and
/// the dense cells store no keys, so the vector is the key store; the value
/// half is still one indexed read. `touched` records which sections were
/// allocated so `clear` and sweeps visit only those.
///
/// `view` is a lazily materialised `HashMap` kept for the callers that pass
/// `&wr` to helpers typed `&HashMap` (the test digests) or coerce `&mut wr`
/// through `DerefMut`. Once materialised it becomes the authoritative store -
/// `view.get().is_some()` means `view` holds every entry - so all operations
/// delegate to it until `clear`/consumption. Materialisation is O(n) and only
/// ever happens on the compatibility paths; the hot write path never builds
/// it.
#[derive(Clone)]
pub(crate) struct FlatChunkWriteMap {
    /// Chunk coords of the 3x3 window centre: `pop_active` when the pass is
    /// running, otherwise the first write's chunk. `None` routes everything
    /// to the overflow.
    anchor: Option<(i32, i32)>,
    /// `sections[chunk * WR_Y_SECTIONS + section_y]`; a touched section is a
    /// 4096-cell array with `WR_EMPTY` meaning "no write".
    sections: [Option<Box<[u32; 4096]>>; WR_SECTIONS],
    /// Section indices allocated so far, so `clear` visits only real work.
    touched: Vec<u16>,
    /// Every live key exactly once, in first-write order: the `&key` store
    /// `iter`/`keys`/`into_iter` draw from.
    order: Vec<(i32, i32, i32)>,
    /// Keys the window cannot hold (out-of-window, out-of-`y`, sentinel
    /// writes). A key lives in exactly one of the two stores.
    overflow: HashMap<(i32, i32, i32), u32>,
    /// Live key count across both stores (only authoritative while `view` is
    /// absent).
    len: usize,
    /// Materialised compatibility view; when present it is the map.
    view: OnceCell<HashMap<(i32, i32, i32), u32>>,
}

/// The dense slot of a coordinate: `(section_index, cell_index)`, or `None`
/// when the coordinate does not fit the anchored 3x3 x 24-section window.
impl FlatChunkWriteMap {
    #[inline]
    fn slot(&self, x: i32, y: i32, z: i32) -> Option<(usize, usize)> {
        let (ax, az) = self.anchor?;
        if y < WR_MIN_Y || y >= WR_MIN_Y + 16 * WR_Y_SECTIONS as i32 {
            return None;
        }
        let (dx, dz) = ((x >> 4) - ax, (z >> 4) - az);
        if !(-1..=1).contains(&dx) || !(-1..=1).contains(&dz) {
            return None;
        }
        let chunk = (dx + 1) as usize + (dz + 1) as usize * 3;
        let section = chunk * WR_Y_SECTIONS + ((y - WR_MIN_Y) >> 4) as usize;
        let cell = (((y & 15) as usize) << 8) | (((z & 15) as usize) << 4) | (x & 15) as usize;
        Some((section, cell))
    }

    /// The dense key as `(x, y, z)` of a live cell: identical content to a
    /// `HashMap`, produced once for the compatibility view.
    fn full_map(&self) -> HashMap<(i32, i32, i32), u32> {
        self.iter().map(|(&k, &v)| (k, v)).collect()
    }

    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Centre the dense window on chunk `(cx, cz)` if no anchor was set yet.
    /// `FC::new` calls this with `pop_active` so the whole writable halo lands
    /// in the flat arrays instead of whichever chunk the first write happened
    /// to touch.
    pub(crate) fn anchor_window(&mut self, cx: i32, cz: i32) {
        if self.anchor.is_none() {
            self.anchor = Some((cx, cz));
        }
    }

    #[inline]
    pub(crate) fn get(&self, k: &(i32, i32, i32)) -> Option<&u32> {
        if let Some(view) = self.view.get() {
            return view.get(k);
        }
        if let Some((si, cell)) = self.slot(k.0, k.1, k.2) {
            if let Some(sec) = &self.sections[si] {
                let v = &sec[cell];
                if *v != WR_EMPTY {
                    return Some(v);
                }
            }
        }
        if self.overflow.is_empty() {
            return None;
        }
        self.overflow.get(k)
    }

    #[inline]
    pub(crate) fn contains_key(&self, k: &(i32, i32, i32)) -> bool {
        if let Some(view) = self.view.get() {
            return view.contains_key(k);
        }
        if let Some((si, cell)) = self.slot(k.0, k.1, k.2) {
            if self.sections[si]
                .as_ref()
                .is_some_and(|s| s[cell] != WR_EMPTY)
            {
                return true;
            }
        }
        !self.overflow.is_empty() && self.overflow.contains_key(k)
    }

    pub(crate) fn insert(&mut self, k: (i32, i32, i32), st: u32) -> Option<u32> {
        if let Some(view) = self.view.get_mut() {
            return view.insert(k, st);
        }
        let (x, y, z) = k;
        if self.anchor.is_none() {
            self.anchor = Some((x >> 4, z >> 4));
        }
        let slot = self.slot(x, y, z);
        match (slot, st == WR_EMPTY) {
            (Some((si, cell)), false) => {
                if self.sections[si].is_none() {
                    self.sections[si] = Some(Box::new([WR_EMPTY; 4096]));
                    self.touched.push(si as u16);
                }
                let sec = self.sections[si].as_mut().unwrap();
                let old = std::mem::replace(&mut sec[cell], st);
                if old != WR_EMPTY {
                    return Some(old);
                }
                // Fresh dense cell, but the key may already live in the
                // overflow (an earlier `WR_EMPTY`-valued write): reuse that
                // occupancy so each key lives in exactly one store. The
                // empty check keeps the usual no-overflow case off the
                // hashing path entirely.
                if !self.overflow.is_empty() {
                    if let Some(old) = self.overflow.remove(&k) {
                        return Some(old);
                    }
                }
                self.order.push(k);
                self.len += 1;
                None
            }
            _ => {
                // Out-of-window, out-of-`y`, or a literal `WR_EMPTY` state:
                // the key lives in the overflow. Any earlier dense slot is
                // cleared so a later `get` reaches the overflow entry.
                let mut prev = None;
                if let Some((si, cell)) = slot {
                    if let Some(sec) = &mut self.sections[si] {
                        if sec[cell] != WR_EMPTY {
                            prev = Some(std::mem::replace(&mut sec[cell], WR_EMPTY));
                        }
                    }
                }
                if let Some(old) = self.overflow.insert(k, st) {
                    prev = Some(old);
                }
                if prev.is_none() {
                    self.order.push(k);
                    self.len += 1;
                }
                prev
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn remove(&mut self, k: &(i32, i32, i32)) -> Option<u32> {
        if let Some(view) = self.view.get_mut() {
            return view.remove(k);
        }
        let mut out = None;
        if let Some((si, cell)) = self.slot(k.0, k.1, k.2) {
            if let Some(sec) = &mut self.sections[si] {
                if sec[cell] != WR_EMPTY {
                    out = Some(std::mem::replace(&mut sec[cell], WR_EMPTY));
                }
            }
        }
        if let Some(v) = self.overflow.remove(k) {
            out = Some(v);
        }
        if out.is_some() {
            self.len -= 1;
            if let Some(p) = self.order.iter().position(|e| e == k) {
                self.order.swap_remove(p);
            }
        }
        out
    }

    #[cfg(test)]
    pub(crate) fn clear(&mut self) {
        if let Some(view) = self.view.get_mut() {
            view.clear();
            return;
        }
        for si in self.touched.drain(..) {
            self.sections[si as usize] = None;
        }
        self.overflow.clear();
        self.order.clear();
        self.len = 0;
    }

    pub(crate) fn len(&self) -> usize {
        match self.view.get() {
            Some(view) => view.len(),
            None => self.len,
        }
    }

    #[cfg(test)]
    pub(crate) fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub(crate) fn iter(&self) -> WrIter<'_> {
        match self.view.get() {
            Some(view) => WrIter::Map(view.iter()),
            None => WrIter::Keys {
                keys: self.order.iter(),
                map: self,
            },
        }
    }

    #[cfg(test)]
    pub(crate) fn keys(&self) -> impl Iterator<Item = &(i32, i32, i32)> + '_ {
        self.iter().map(|(k, _)| k)
    }

    #[cfg(test)]
    #[cfg_attr(not(megu_vanilla_data), allow(dead_code))]
    pub(crate) fn values(&self) -> impl Iterator<Item = &u32> + '_ {
        self.iter().map(|(_, v)| v)
    }
}

/// `FlatChunkWriteMap::iter`: the materialised view when it exists, else the
/// `order` vector filtered through the live stores (a `remove`d or moved key
/// is skipped by the `get` miss).
pub(crate) enum WrIter<'a> {
    Map(std::collections::hash_map::Iter<'a, (i32, i32, i32), u32>),
    Keys {
        keys: std::slice::Iter<'a, (i32, i32, i32)>,
        map: &'a FlatChunkWriteMap,
    },
}

impl<'a> Iterator for WrIter<'a> {
    type Item = (&'a (i32, i32, i32), &'a u32);

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        match self {
            WrIter::Map(it) => it.next(),
            WrIter::Keys { keys, map } => loop {
                let k = keys.next()?;
                if let Some(v) = map.get(k) {
                    return Some((k, v));
                }
            },
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        match self {
            WrIter::Map(it) => it.size_hint(),
            WrIter::Keys { keys, .. } => (keys.len(), Some(keys.len())),
        }
    }
}

impl<'a> IntoIterator for &'a FlatChunkWriteMap {
    type Item = (&'a (i32, i32, i32), &'a u32);
    type IntoIter = WrIter<'a>;
    fn into_iter(self) -> WrIter<'a> {
        self.iter()
    }
}

impl IntoIterator for FlatChunkWriteMap {
    type Item = ((i32, i32, i32), u32);
    type IntoIter = std::vec::IntoIter<((i32, i32, i32), u32)>;
    fn into_iter(mut self) -> Self::IntoIter {
        let out: Vec<((i32, i32, i32), u32)> = match self.view.take() {
            Some(view) => view.into_iter().collect(),
            None => self
                .order
                .iter()
                .filter_map(|k| self.get(k).map(|v| (*k, *v)))
                .collect(),
        };
        out.into_iter()
    }
}

impl std::iter::FromIterator<((i32, i32, i32), u32)> for FlatChunkWriteMap {
    fn from_iter<I: IntoIterator<Item = ((i32, i32, i32), u32)>>(iter: I) -> Self {
        let mut m = Self::new();
        for (k, v) in iter {
            m.insert(k, v);
        }
        m
    }
}

impl Default for FlatChunkWriteMap {
    fn default() -> Self {
        Self {
            anchor: None,
            sections: std::array::from_fn(|_| None),
            touched: Vec::new(),
            order: Vec::new(),
            overflow: HashMap::new(),
            len: 0,
            view: OnceCell::new(),
        }
    }
}

impl PartialEq for FlatChunkWriteMap {
    fn eq(&self, other: &Self) -> bool {
        self.len() == other.len() && self.iter().all(|(k, v)| other.get(k) == Some(v))
    }
}

impl std::fmt::Debug for FlatChunkWriteMap {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_map().entries(self.iter()).finish()
    }
}

/// `&wr` where a `&HashMap` is required (the test digest helpers) and `&mut wr`
/// for `mem::take`-style derefs both coerce to the lazily built full map. The
/// view is only materialised on these paths and stays authoritative once built.
impl std::ops::Deref for FlatChunkWriteMap {
    type Target = HashMap<(i32, i32, i32), u32>;
    fn deref(&self) -> &Self::Target {
        self.view.get_or_init(|| self.full_map())
    }
}

impl std::ops::DerefMut for FlatChunkWriteMap {
    fn deref_mut(&mut self) -> &mut Self::Target {
        if self.view.get().is_none() {
            let full = self.full_map();
            let _ = self.view.set(full);
        }
        self.view.get_mut().unwrap()
    }
}

pub(crate) struct FC<'a> {
    pub(crate) g: &'a mut Gen,
    /// A3-a wave workers borrow the owner read-only for the state a feature placement reads
    /// across chunks (`feat_c`, `carve_c`, the feature heightmap and the built columns). The
    /// serial path has no owner. Every fallback here is read-only: a worker never mutates the
    /// owner, it only commits its own results when the wave ends.
    owner: Option<&'a Gen>,
    /// Population indices before wave scheduling reorders independent chunks.
    worker_order: Option<PopulationOrder<'a>>,
    /// The feature-write layer: a dense 3x3-chunk window (see
    /// [`FlatChunkWriteMap`]) instead of a plain `HashMap`. `get`/`insert` on
    /// in-window cells are O(1) array accesses; out-of-window cells fall back
    /// to the embedded overflow, so the observable key set is unchanged.
    pub(crate) wr: FlatChunkWriteMap,
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
    rc: CoordMap<(i32, i32, i32), u32>, // terrain read-through cache
    /// Frozen pre-CARVERS `*_WG` heightmaps, memoized per column. Feature writes
    /// do not invalidate this terrain-only view. Live (non-WG) variants are never
    /// cached: they follow carver and feature writes.
    hmwg: CoordMap<(i32, i32, HeightmapKind), i32>,

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
    pub(crate) test_writes: Option<TestBlockWrites>,
}
impl<'a> FC<'a> {
    pub(crate) fn new(g: &'a mut Gen) -> Self {
        // The 3x3 window `feature_write_allowed` enforces is centred on the
        // chunk being populated; anchoring the write buffer on it puts every
        // legal write on the flat path. Outside a FEATURES pass (tests, dump
        // helpers) the buffer anchors lazily on its first write.
        let anchor = g.pop_active;
        let mut ctx = FC {
            g,
            owner: None,
            worker_order: None,
            wr: FlatChunkWriteMap::new(),
            shape_marks: Vec::new(),
            rc: CoordMap::default(),
            hmwg: CoordMap::default(),
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
        };
        if let Some((ax, az)) = anchor {
            ctx.wr.anchor_window(ax, az);
        }
        ctx
    }

    /// A wave worker's context: its own scratch generator for writes, the owner for the
    /// read-only cross-chunk state the placement loop needs.
    pub(crate) fn new_worker(
        g: &'a mut Gen,
        owner: &'a Gen,
        order: &'a HashMap<(i32, i32), usize>,
        index: usize,
    ) -> Self {
        let mut ctx = FC::new(g);
        ctx.owner = Some(owner);
        ctx.worker_order = Some((order, index));
        ctx
    }

    /// The owner-side continuation of a wave worker: the worker's writes are installed on the
    /// owner and the same promotion boundary as the serial arm runs here, in population order.
    pub(crate) fn new_commit(
        g: &'a mut Gen,
        wr: FlatChunkWriteMap,
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

    pub(crate) fn has_mineshaft_spawner(&self, key: (i32, i32, usize)) -> bool {
        self.g.mineshaft_spawners.contains(&key)
            || self
                .owner
                .is_some_and(|o| o.mineshaft_spawners.contains(&key))
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

    /// Columns for a terrain read: the owner's resident columns first (A3-a leaves them
    /// there — the round share's columns and the wave commits' merged `col_c` entries),
    /// then a cold build on the worker through the round-scoped `TerrainShare`. A biome
    /// lookup in `FC` (`biome_leaf`/`biome`) never builds columns at all: the quart lookup
    /// answers from the biome tree.
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

    /// At FEATURES, a cold sky column reads 15 from SkyLightSectionStorage.
    /// LIGHT requires INITIALIZE_LIGHT in a 3x3, and section storage allocates
    /// a one-chunk halo around those initialized columns. Model that halo as
    /// zero-filled until the port has a FEATURES-time light field.
    pub(crate) fn feature_brightness(&self, x: i32, y: i32, z: i32) -> u8 {
        if !self.g.has_skylight() {
            return self.block_light(x, y, z);
        }
        let active = self.g.pop_active;
        let world = self.owner.unwrap_or(self.g);
        let completed = |chunk| {
            if let Some((order, index)) = self.worker_order {
                if let Some(&previous) = order.get(&chunk) {
                    return previous < index;
                }
            }
            Some(chunk) != active && world.pop_done.contains(&chunk)
        };
        let (cx, cz) = (x >> 4, z >> 4);
        for sx in cx - 2..=cx + 2 {
            for sz in cz - 2..=cz + 2 {
                if completed((sx, sz))
                    && (-1..=1).all(|dx| (-1..=1).all(|dz| completed((sx + dx, sz + dz))))
                {
                    return self.block_light(x, y, z);
                }
            }
        }
        15
    }

    /// The chunk-promotion boundary: sample the light the pass reads once.
    ///
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
    /// `None` outside a promotion pass. Feature placement uses
    /// [`FC::feature_brightness`] for the earlier light-storage state.
    pub(crate) fn raw_brightness(&mut self, x: i32, y: i32, z: i32) -> Option<u8> {
        let block = self.promo_light.as_ref()?.block.get(&(x, y, z)).copied().unwrap_or(0);
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

/// `MEGU_HM_DUMP=cx,cz`: print, for every column of one chunk, the three live
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

/// `MEGU_WG_DUMP_HM=<path>`: write the OCEAN_FLOOR height of every column of
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

// ---------- PERF-2a tests: FlatChunkWriteMap parity and cost ----------

/// Deterministic xorshift64 for the parity driver and the bench workload.
#[cfg(test)]
fn perf2a_rng(seed: u64) -> impl FnMut() -> u64 {
    let mut s = seed | 1;
    move || {
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        s
    }
}

/// 50k mixed operations must produce exactly the observable behaviour of
/// `HashMap<(i32, i32, i32), u32>`: same `insert`/`remove` returns, same `get`/
/// `contains_key`/`len`, same key set on `iter`/`into_iter`, working
/// `Clone`/`PartialEq`/`clear`/`FromIterator`, and a materialised `&HashMap`
/// deref that stays live under later writes.
#[cfg(test)]
#[test]
fn perf2a_flat_wr_differential_parity() {
    let mut rng = perf2a_rng(0x9E37_79B9_7F4A_7C15);
    let mut flat = FlatChunkWriteMap::new();
    // Anchor on chunk (0,0): in-window x,z in -16..=31, y in -64..=319.
    flat.anchor_window(0, 0);
    let mut map: HashMap<(i32, i32, i32), u32> = HashMap::new();

    for i in 0..50_000u64 {
        let r = rng();
        // ~64% in-window, ~8% window edge, ~8% negative in-window,
        // ~10% out-of-window x/z, ~5% out-of-range y, ~5% negative far.
        let (x, y, z) = match r % 100 {
            0..=63 => (
                ((r >> 8) % 48) as i32 - 16,
                -64 + ((r >> 16) % 384) as i32,
                ((r >> 32) % 48) as i32 - 16,
            ),
            64..=71 => (
                (((r >> 8) % 3) as i32 - 1) * 16 + ((r >> 16) % 16) as i32,
                -64 + ((r >> 32) % 384) as i32,
                (((r >> 40) % 3) as i32 - 1) * 16 + ((r >> 48) % 16) as i32,
            ),
            72..=79 => (
                -(((r >> 8) % 32) as i32),
                -64 + ((r >> 16) % 384) as i32,
                -(((r >> 32) % 32) as i32),
            ),
            80..=89 => (
                ((r >> 8) % 2000) as i32 - 1000,
                -64 + ((r >> 16) % 384) as i32,
                ((r >> 32) % 2000) as i32 - 1000,
            ),
            90..=94 => (
                ((r >> 8) % 48) as i32 - 16,
                -200 + ((r >> 16) % 700) as i32,
                ((r >> 32) % 48) as i32 - 16,
            ),
            _ => (
                -(((r >> 8) % 4000) as i32),
                -64 + ((r >> 16) % 384) as i32,
                -(((r >> 32) % 4000) as i32),
            ),
        };
        // A rare sentinel write proves the u32::MAX overflow routing.
        let st = if i % 997 == 0 {
            u32::MAX
        } else {
            ((r >> 40) % 4096) as u32
        };
        match r % 13 {
            0 => assert_eq!(
                map.remove(&(x, y, z)),
                flat.remove(&(x, y, z)),
                "remove {x},{y},{z}"
            ),
            _ => assert_eq!(
                map.insert((x, y, z), st),
                flat.insert((x, y, z), st),
                "insert {x},{y},{z}"
            ),
        }
        if i % 641 == 0 {
            assert_eq!(map.len(), flat.len());
            assert_eq!(map.get(&(x, y, z)), flat.get(&(x, y, z)));
            assert_eq!(map.contains_key(&(x, y, z)), flat.contains_key(&(x, y, z)));
        }
    }

    assert_eq!(map.len(), flat.len());
    assert_eq!(map.is_empty(), flat.is_empty());
    let mut a: Vec<_> = flat.iter().map(|(&k, &v)| (k, v)).collect();
    a.sort();
    let mut b: Vec<_> = map.iter().map(|(&k, &v)| (k, v)).collect();
    b.sort();
    assert_eq!(a, b, "iterated key set must equal the HashMap's");
    let mut ka: Vec<_> = flat.keys().copied().collect();
    ka.sort();
    let mut kb: Vec<_> = map.keys().copied().collect();
    kb.sort();
    assert_eq!(ka, kb);

    let clone = flat.clone();
    assert!(clone == flat);
    assert!(flat != FlatChunkWriteMap::new() || flat.is_empty());

    // Consuming iteration yields the same key set.
    let consumed: HashMap<_, _> = flat.clone().into_iter().collect();
    assert_eq!(consumed, map);
    let rebuilt: FlatChunkWriteMap = map.iter().map(|(&k, &v)| (k, v)).collect();
    assert!(rebuilt == flat);

    // The `&HashMap` compatibility view materialises complete contents, then
    // keeps answering as the map mutates (sha helpers rely on it).
    {
        let hv: &HashMap<(i32, i32, i32), u32> = &flat;
        assert_eq!(hv.len(), map.len());
    }
    let sentinel_key = (5_000, 64, -5_000);
    flat.insert(sentinel_key, 7);
    {
        let hv: &HashMap<(i32, i32, i32), u32> = &flat;
        assert_eq!(hv.get(&sentinel_key), Some(&7));
        assert_eq!(hv.len(), map.len() + 1);
    }

    // `&mut HashMap` deref mutations land in the map: `mem::take` semantics.
    {
        let hm: &mut HashMap<(i32, i32, i32), u32> = &mut flat;
        let took = std::mem::take(hm);
        assert_eq!(took.len(), map.len() + 1);
    }
    assert!(flat.is_empty());
    flat.insert((1, 2, 3), 9);
    assert_eq!(flat.get(&(1, 2, 3)), Some(&9));

    let mut cleared = flat.clone();
    cleared.clear();
    assert!(cleared.is_empty());
    assert_eq!(cleared.len(), 0);
}

/// After materialisation the view must stay authoritative across every op:
/// a `Deref` snapshot plus later writes/removes/iter must still agree with a
/// plain `HashMap` run through the same sequence.
#[cfg(test)]
#[test]
fn perf2a_flat_wr_view_stays_authoritative() {
    let mut rng = perf2a_rng(0xB529_7A4D_B529_7A4D);
    let mut flat = FlatChunkWriteMap::new();
    flat.anchor_window(0, 0);
    let mut map: HashMap<(i32, i32, i32), u32> = HashMap::new();
    for _ in 0..2_000 {
        let r = rng();
        let k = (
            ((r >> 8) % 48) as i32 - 16,
            -64 + ((r >> 16) % 384) as i32,
            ((r >> 32) % 48) as i32 - 16,
        );
        flat.insert(k, (r % 4096) as u32);
        map.insert(k, (r % 4096) as u32);
    }
    // Materialise, then mutate through both paths.
    {
        let hv: &HashMap<(i32, i32, i32), u32> = &flat;
        assert_eq!(hv.len(), map.len());
    }
    for _ in 0..1_000 {
        let r = rng();
        let k = (
            ((r >> 8) % 64) as i32 - 32,
            -64 + ((r >> 16) % 384) as i32,
            ((r >> 32) % 64) as i32 - 32,
        );
        if r.is_multiple_of(11) {
            flat.remove(&k);
            map.remove(&k);
        } else {
            flat.insert(k, (r % 8192) as u32);
            map.insert(k, (r % 8192) as u32);
        }
    }
    {
        let hv: &HashMap<(i32, i32, i32), u32> = &flat;
        assert_eq!(hv.len(), map.len());
    }
    let mut a: Vec<_> = flat.iter().map(|(&k, &v)| (k, v)).collect();
    a.sort();
    let mut b: Vec<_> = map.iter().map(|(&k, &v)| (k, v)).collect();
    b.sort();
    assert_eq!(a, b);
}

/// Micro-bench: an ore-vein-shaped workload of ~60k writes plus read-backs,
/// measuring the flat window against the `HashMap` it replaces. The numbers
/// are printed (not asserted) so the check stays stable across hardware; the
/// correctness of every written cell *is* asserted.
#[cfg(test)]
#[test]
fn perf2a_flat_wr_ore_vein_bench() {
    use std::hint::black_box;
    use std::time::Instant;

    // Clustered ore-vein cells: ~30 veins of ~2,000 writes inside a 3x3
    // chunk window, with ~20% read-backs of already-written cells.
    let mut rng = perf2a_rng(0xD1B5_4A32_D1B5_4A32);
    let mut cells: Vec<(i32, i32, i32)> = Vec::with_capacity(60_000);
    let mut reads: Vec<(i32, i32, i32)> = Vec::with_capacity(12_000);
    for v in 0..30 {
        let (bx, by, bz) = (
            ((rng() % 40) as i32) - 12,
            -40 + (rng() % 200) as i32,
            ((rng() % 40) as i32) - 12,
        );
        for _ in 0..2_000 {
            let r = rng();
            let k = (
                bx + ((r >> 8) % 11) as i32 - 5,
                by + ((r >> 16) % 11) as i32 - 5,
                bz + ((r >> 32) % 11) as i32 - 5,
            );
            cells.push(k);
            if r.is_multiple_of(5) && !cells.is_empty() {
                reads.push(cells[(r as usize) % cells.len()]);
            }
        }
        let _ = v;
    }

    let mut flat = FlatChunkWriteMap::new();
    flat.anchor_window(0, 0);
    let t0 = Instant::now();
    for &k in &cells {
        black_box(flat.insert(k, 42));
    }
    let flat_ins = t0.elapsed();
    let t0 = Instant::now();
    let mut acc = 0u64;
    for &k in &reads {
        acc += flat.get(&k).copied().unwrap_or(0) as u64;
    }
    let flat_get = t0.elapsed();

    let mut map: HashMap<(i32, i32, i32), u32> = HashMap::new();
    let t0 = Instant::now();
    for &k in &cells {
        black_box(map.insert(k, 42));
    }
    let map_ins = t0.elapsed();
    let t0 = Instant::now();
    let mut acc2 = 0u64;
    for &k in &reads {
        acc2 += map.get(&k).copied().unwrap_or(0) as u64;
    }
    let map_get = t0.elapsed();
    assert_eq!(acc, acc2);

    let mut a: Vec<_> = flat.iter().map(|(&k, &v)| (k, v)).collect();
    a.sort();
    let mut b: Vec<_> = map.iter().map(|(&k, &v)| (k, v)).collect();
    b.sort();
    assert_eq!(a, b);

    let flat_ip = flat_ins.as_nanos() as f64 / cells.len() as f64;
    let map_ip = map_ins.as_nanos() as f64 / cells.len() as f64;
    let flat_gp = flat_get.as_nanos() as f64 / reads.len() as f64;
    let map_gp = map_get.as_nanos() as f64 / reads.len() as f64;
    eprintln!(
        "perf2a ore-vein workload: {} cells ({} unique), {} reads\n\
         \x20 insert: flat {flat_ins:?} ({flat_ip:.1} ns/op)  hashmap {map_ins:?} ({map_ip:.1} ns/op)  speedup {:.2}x\n\
         \x20 get:    flat {flat_get:?} ({flat_gp:.1} ns/op)  hashmap {map_get:?} ({map_gp:.1} ns/op)  speedup {:.2}x",
        cells.len(),
        a.len(),
        reads.len(),
        map_ip / flat_ip,
        map_gp / flat_gp,
    );
}
