// M3 P5 Spatial Grid: Chunk-Cell Entity Spatial Grid Index (`EntitySpatialGrid`)
// Eliminates O(N^2) neighborhood scans in `mob_tick` (`pushEntities` AABB overlap + breeding radius).

use megu_core::coords::CoordMap;

#[derive(Clone, Copy, Debug)]
struct SpatialEntity {
    idx: usize,
    eid: i32,
    is_mob: bool,
    is_spectator: bool,
    is_pushable: bool,
    is_passenger: bool,
    is_vehicle: bool,
    no_physics: bool,
    etype: i64,
    aabb: EntityAabb,
    x: f64,
    z: f64,
}

#[derive(Default)]
struct SpatialScratch {
    seen: Vec<u32>,
    cells_order: Vec<(i32, i32)>,
    stamp: u32,
}

impl SpatialScratch {
    #[inline]
    fn ensure_len(&mut self, len: usize) {
        if self.seen.len() < len {
            self.seen.resize(len, 0);
        }
    }

    #[inline]
    fn next_stamp(&mut self) -> u32 {
        self.stamp = self.stamp.wrapping_add(1);
        if self.stamp == 0 {
            self.seen.fill(0);
            self.stamp = 1;
        }
        self.stamp
    }
}

type SpatialCellRanges = CoordMap<(i32, i32), (u32, u32)>;
type ChunkViewDelta = (Vec<(i32, i32)>, Vec<(i32, i32)>);

struct EntitySpatialGrid {
    entities: Vec<SpatialEntity>,
    cells: SpatialCellRanges,
    cell_entries: Vec<u32>,
}

/// Retained-capacity bound for each reused spatial buffer. A burst of entities
/// far above the steady-state working set must not pin its peak capacity in
/// the thread-local pool forever; past this bound a stashed buffer is shrunk.
const SPATIAL_POOL_KEEP_BYTES: usize = 256 * 1024;

fn bound_spatial_vec<T>(v: &mut Vec<T>) {
    let keep = SPATIAL_POOL_KEEP_BYTES / std::mem::size_of::<T>().max(1);
    if v.capacity() > keep {
        v.shrink_to(keep);
    }
}

fn bound_spatial_cells(cells: &mut SpatialCellRanges) {
    let keep =
        SPATIAL_POOL_KEEP_BYTES / std::mem::size_of::<((i32, i32), (u32, u32))>().max(1);
    if cells.capacity() > keep {
        cells.shrink_to(keep);
    }
}

/// Buffers a dropped [`EntitySpatialGrid`] returns for the next rebuild, plus
/// the dedup stamp/output pair for ad-hoc neighbor queries. Rebuilds run on
/// the tick thread every tick, so one pool slot per buffer is enough.
#[derive(Default)]
struct SpatialPool {
    entities: Vec<SpatialEntity>,
    cells: SpatialCellRanges,
    cell_entries: Vec<u32>,
    query_scratch: SpatialScratch,
    query_out: Vec<u32>,
}

thread_local! {
    static SPATIAL_POOL: std::cell::RefCell<SpatialPool> =
        std::cell::RefCell::new(SpatialPool {
            entities: Vec::new(),
            cells: CoordMap::default(),
            cell_entries: Vec::new(),
            query_scratch: SpatialScratch::default(),
            query_out: Vec::new(),
        });
}

/// Take the pooled cell map and entry array (empty, capacity retained) for a
/// grid rebuild.
fn spatial_pool_take_cells() -> (SpatialCellRanges, Vec<u32>) {
    SPATIAL_POOL.with(|pool| {
        pool.try_borrow_mut()
            .map(|mut pool| {
                (
                    std::mem::take(&mut pool.cells),
                    std::mem::take(&mut pool.cell_entries),
                )
            })
            .unwrap_or_default()
    })
}

/// Take the pooled entity snapshot vector (empty, capacity retained).
fn spatial_pool_take_entities() -> Vec<SpatialEntity> {
    SPATIAL_POOL.with(|pool| {
        pool.try_borrow_mut()
            .map(|mut pool| std::mem::take(&mut pool.entities))
            .unwrap_or_default()
    })
}

/// Return a dropped grid's buffers to the pool, bounded at
/// [`SPATIAL_POOL_KEEP_BYTES`]. The pool keeps whichever side holds more
/// capacity so a small grid never evicts a larger warmed buffer.
fn spatial_pool_stash(
    mut entities: Vec<SpatialEntity>,
    mut cells: SpatialCellRanges,
    mut cell_entries: Vec<u32>,
) {
    entities.clear();
    cells.clear();
    cell_entries.clear();
    bound_spatial_vec(&mut entities);
    bound_spatial_cells(&mut cells);
    bound_spatial_vec(&mut cell_entries);
    let _ = SPATIAL_POOL.try_with(|pool| {
        if let Ok(mut pool) = pool.try_borrow_mut() {
            if entities.capacity() > pool.entities.capacity() {
                pool.entities = entities;
            }
            if cells.capacity() > pool.cells.capacity() {
                pool.cells = cells;
            }
            if cell_entries.capacity() > pool.cell_entries.capacity() {
                pool.cell_entries = cell_entries;
            }
        }
    });
}

/// Borrow the per-thread neighbor-query buffers (dedup stamp + output slots)
/// for one query batch. `SpatialScratch` creation is free, but its `seen`
/// vector has to grow to the entity count once; pooling it amortizes that
/// growth across every ad-hoc query on the thread.
fn with_spatial_query_scratch<R>(
    f: impl FnOnce(&mut SpatialScratch, &mut Vec<u32>) -> R,
) -> R {
    SPATIAL_POOL.with(|pool| match pool.try_borrow_mut() {
        Ok(mut pool) => {
            let pool = &mut *pool;
            let result = f(&mut pool.query_scratch, &mut pool.query_out);
            bound_spatial_vec(&mut pool.query_scratch.seen);
            bound_spatial_vec(&mut pool.query_scratch.cells_order);
            bound_spatial_vec(&mut pool.query_out);
            result
        }
        Err(_) => f(&mut SpatialScratch::default(), &mut Vec::new()),
    })
}

#[inline]
fn spatial_chunk_coord(v: f64) -> i32 {
    (v.floor() as i32) >> 4
}

#[inline]
fn mob_type_is_pushable(etype: i64) -> bool {
    // 1.21.1 ArmorStand (`ciw.bB() -> false`, etype=3) and Bat (`cez.bB() -> false`, etype=6).
    !matches!(etype, 3 | 6)
}

impl std::ops::Drop for EntitySpatialGrid {
    fn drop(&mut self) {
        spatial_pool_stash(
            std::mem::take(&mut self.entities),
            std::mem::take(&mut self.cells),
            std::mem::take(&mut self.cell_entries),
        );
    }
}

impl EntitySpatialGrid {
    fn from_entities(entities: Vec<SpatialEntity>) -> Self {
        // Pass 1: per-chunk entity counts. The single `cells` map holds
        // `(0, count)` — range start in the first field, write cursor in the
        // second — so the separate `counts` map disappears. The canonical
        // non-air-mask kernel marks the slots whose AABB is finite, so pass
        // 2 walks live slots by draining set bits instead of re-running
        // four float checks per entity.
        let (mut cells, mut cell_entries) = spatial_pool_take_cells();
        cells.clear();
        let mut total_entries = 0usize;
        let mut occupancy = Vec::with_capacity(entities.len().div_ceil(64));

        for block in entities.chunks(64) {
            let mut states = [0u16; 64];
            for (i, ent) in block.iter().enumerate() {
                let finite = ent.aabb.min_x.is_finite()
                    && ent.aabb.max_x.is_finite()
                    && ent.aabb.min_z.is_finite()
                    && ent.aabb.max_z.is_finite();
                states[i] = u16::from(finite);
                if !finite {
                    continue;
                }
                let cx0 = spatial_chunk_coord(ent.aabb.min_x);
                let cx1 = spatial_chunk_coord(ent.aabb.max_x);
                let cz0 = spatial_chunk_coord(ent.aabb.min_z);
                let cz1 = spatial_chunk_coord(ent.aabb.max_z);
                if cx1.saturating_sub(cx0) > 16 || cz1.saturating_sub(cz0) > 16 {
                    continue;
                }
                for cx in cx0..=cx1 {
                    for cz in cz0..=cz1 {
                        cells.entry((cx, cz)).or_insert((0, 0)).1 += 1;
                        total_entries += 1;
                    }
                }
            }
            occupancy.push(megu_core::simd_voxel::build_non_air_bitmask_64(&states));
        }

        // Conversion pass: `(0, count)` becomes `(start, start)` — the range
        // start in the first field and the cursor in the second. Ranges are
        // assigned in chunk Z-order instead of hash order so a multi-cell
        // query touches `cell_entries` spans that sit next to each other.
        let mut order: Vec<(i32, i32)> = cells.keys().copied().collect();
        order.sort_unstable_by_key(|&(cx, cz)| {
            (
                megu_core::morton::morton2_encode_i16(cx as i16, cz as i16),
                cx,
                cz,
            )
        });
        let mut offset = 0u32;
        for key in order {
            let value = cells.get_mut(&key).unwrap();
            let count = value.1;
            value.0 = offset;
            value.1 = offset;
            offset += count;
        }

        // Pass 2: fill `cell_entries` through the per-cell cursors, walking
        // the occupancy mask's set bits in ascending slot order. After the
        // pass each value reads `(start, start + count)` — the query range.
        cell_entries.clear();
        cell_entries.resize(total_entries, 0);
        for (block_idx, &mask) in occupancy.iter().enumerate() {
            let mut mask = mask;
            while mask != 0 {
                let slot = block_idx * 64 + mask.trailing_zeros() as usize;
                mask &= mask - 1;
                let ent = &entities[slot];
                let cx0 = spatial_chunk_coord(ent.aabb.min_x);
                let cx1 = spatial_chunk_coord(ent.aabb.max_x);
                let cz0 = spatial_chunk_coord(ent.aabb.min_z);
                let cz1 = spatial_chunk_coord(ent.aabb.max_z);
                if cx1.saturating_sub(cx0) > 16 || cz1.saturating_sub(cz0) > 16 {
                    continue;
                }
                for cx in cx0..=cx1 {
                    for cz in cz0..=cz1 {
                        if let Some(cursor) = cells.get_mut(&(cx, cz)) {
                            cell_entries[cursor.1 as usize] = slot as u32;
                            cursor.1 += 1;
                        }
                    }
                }
            }
        }

        Self {
            entities,
            cells,
            cell_entries,
        }
    }

    fn build_mobs_and_clients(mobs_snap: &[Arc<Mob>], cls_snap: &[Arc<Client>]) -> Self {
        let mut entities = spatial_pool_take_entities();
        entities.clear();
        entities.reserve(mobs_snap.len() + cls_snap.len());
        for (idx, m) in mobs_snap.iter().enumerate() {
            let (x, y, z, _, _) = *m.pos.lock().unwrap_or_else(|e| e.into_inner());
            let alive = *m.hp.lock().unwrap_or_else(|e| e.into_inner()) > 0.0;
            let (w, h) = mob_dimensions(m.etype);
            let aabb = EntityAabb::from_dimensions(x, y, z, w, h);
            entities.push(SpatialEntity {
                idx,
                eid: m.eid,
                is_mob: true,
                is_spectator: false,
                is_pushable: alive && mob_type_is_pushable(m.etype),
                is_passenger: false,
                is_vehicle: false,
                no_physics: false,
                etype: m.etype,
                aabb,
                x,
                z,
            });
        }
        for (idx, c) in cls_snap.iter().enumerate() {
            let (x, y, z, _, _, _) = *c.pos.lock().unwrap_or_else(|e| e.into_inner());
            let alive = *c.hp.lock().unwrap_or_else(|e| e.into_inner()) > 0.0;
            let spectator = c.is_spectator();
            let sneaking = *c.sneaking.lock().unwrap_or_else(|e| e.into_inner());
            let fall_fly = *c.fall_fly.lock().unwrap_or_else(|e| e.into_inner());
            let aabb = player_bounding_box(x, y, z, sneaking, fall_fly);
            entities.push(SpatialEntity {
                idx,
                eid: c.eid,
                is_mob: false,
                is_spectator: spectator,
                is_pushable: alive && !spectator,
                is_passenger: false,
                is_vehicle: false,
                no_physics: spectator,
                etype: 128,
                aabb,
                x,
                z,
            });
        }
        Self::from_entities(entities)
    }

    fn query_aabb_into(
        &self,
        aabb: EntityAabb,
        exclude_eid: i32,
        scratch: &mut SpatialScratch,
        out: &mut Vec<u32>,
    ) {
        out.clear();
        if !aabb.min_x.is_finite()
            || !aabb.max_x.is_finite()
            || !aabb.min_z.is_finite()
            || !aabb.max_z.is_finite()
        {
            return;
        }
        let cx0 = spatial_chunk_coord(aabb.min_x);
        let cx1 = spatial_chunk_coord(aabb.max_x);
        let cz0 = spatial_chunk_coord(aabb.min_z);
        let cz1 = spatial_chunk_coord(aabb.max_z);
        if cx0 == cx1 && cz0 == cz1 {
            if let Some(&(start, end)) = self.cells.get(&(cx0, cz0)) {
                for &slot in &self.cell_entries[start as usize..end as usize] {
                    let ent = &self.entities[slot as usize];
                    if ent.eid != exclude_eid && ent.aabb.intersects(aabb) {
                        out.push(slot);
                    }
                }
            }
            return;
        }
        scratch.ensure_len(self.entities.len());
        let stamp = scratch.next_stamp();
        // `cell_entries` is laid out in chunk Z-order, so visit the query
        // window in that same order: each hit reads a contiguous span and
        // consecutive cells sit consecutively in the array.
        scratch.cells_order.clear();
        scratch.cells_order.extend(
            (cx0..=cx1).flat_map(|cx| (cz0..=cz1).map(move |cz| (cx, cz))),
        );
        scratch.cells_order.sort_unstable_by_key(|&(cx, cz)| {
            megu_core::morton::morton2_encode_i16(cx as i16, cz as i16)
        });
        for i in 0..scratch.cells_order.len() {
            let (cx, cz) = scratch.cells_order[i];
            if let Some(&(start, end)) = self.cells.get(&(cx, cz)) {
                for &slot in &self.cell_entries[start as usize..end as usize] {
                    let slot_idx = slot as usize;
                    if scratch.seen[slot_idx] == stamp {
                        continue;
                    }
                    scratch.seen[slot_idx] = stamp;
                    let ent = &self.entities[slot_idx];
                    if ent.eid != exclude_eid && ent.aabb.intersects(aabb) {
                        out.push(slot);
                    }
                }
            }
        }
        out.sort_unstable();
    }

    #[allow(dead_code)]
    fn query_aabb(&self, aabb: EntityAabb, exclude_eid: i32) -> Vec<u32> {
        let mut out = Vec::new();
        with_spatial_query_scratch(|scratch, _| {
            self.query_aabb_into(aabb, exclude_eid, scratch, &mut out)
        });
        out
    }

    fn query_radius_xz_into(
        &self,
        x: f64,
        z: f64,
        r: f64,
        exclude_eid: i32,
        scratch: &mut SpatialScratch,
        out: &mut Vec<u32>,
    ) {
        out.clear();
        if !x.is_finite() || !z.is_finite() || !r.is_finite() || r <= 0.0 {
            return;
        }
        let r_sq = r * r;
        let cx0 = spatial_chunk_coord(x - r);
        let cx1 = spatial_chunk_coord(x + r);
        let cz0 = spatial_chunk_coord(z - r);
        let cz1 = spatial_chunk_coord(z + r);
        if cx0 == cx1 && cz0 == cz1 {
            if let Some(&(start, end)) = self.cells.get(&(cx0, cz0)) {
                for &slot in &self.cell_entries[start as usize..end as usize] {
                    let ent = &self.entities[slot as usize];
                    if ent.eid != exclude_eid {
                        let dx = ent.x - x;
                        let dz = ent.z - z;
                        if dx * dx + dz * dz < r_sq {
                            out.push(slot);
                        }
                    }
                }
            }
            return;
        }
        scratch.ensure_len(self.entities.len());
        let stamp = scratch.next_stamp();
        scratch.cells_order.clear();
        scratch.cells_order.extend(
            (cx0..=cx1).flat_map(|cx| (cz0..=cz1).map(move |cz| (cx, cz))),
        );
        scratch.cells_order.sort_unstable_by_key(|&(cx, cz)| {
            megu_core::morton::morton2_encode_i16(cx as i16, cz as i16)
        });
        for i in 0..scratch.cells_order.len() {
            let (cx, cz) = scratch.cells_order[i];
            if let Some(&(start, end)) = self.cells.get(&(cx, cz)) {
                for &slot in &self.cell_entries[start as usize..end as usize] {
                    let slot_idx = slot as usize;
                    if scratch.seen[slot_idx] == stamp {
                        continue;
                    }
                    scratch.seen[slot_idx] = stamp;
                    let ent = &self.entities[slot_idx];
                    if ent.eid != exclude_eid {
                        let dx = ent.x - x;
                        let dz = ent.z - z;
                        if dx * dx + dz * dz < r_sq {
                            out.push(slot);
                        }
                    }
                }
            }
        }
        out.sort_unstable();
    }

    #[allow(dead_code)]
    fn query_radius_xz(&self, x: f64, z: f64, r: f64, exclude_eid: i32) -> Vec<u32> {
        let mut out = Vec::new();
        with_spatial_query_scratch(|scratch, _| {
            self.query_radius_xz_into(x, z, r, exclude_eid, scratch, &mut out)
        });
        out
    }
}

// Vanilla 1.21.1 bsx.<clinit> / bsx$a.a(int), verified against the
// bundled server jar. Unknown IDs use the builder's default of five chunks;
// callers must validate modded or otherwise unknown IDs before using it for culling.
#[allow(dead_code)]
fn client_tracking_range_chunks(entity_id: &str) -> u32 {
    let name = entity_id.strip_prefix("minecraft:").unwrap_or(entity_id);
    ENTITY_REGISTRY_1211.iter()
        .find(|spec| spec.name.strip_prefix("minecraft:") == Some(name))
        .map_or(5, |spec| spec.client_tracking_range as u32)
}

// The registry is extracted from the 1.21.1 EntityType registrations. The
// builder default is three; unknown IDs must be validated before use.
#[allow(dead_code)]
fn entity_update_interval_ticks(entity_id: &str) -> u32 {
    let name = entity_id.strip_prefix("minecraft:").unwrap_or(entity_id);
    ENTITY_REGISTRY_1211.iter()
        .find(|spec| spec.name.strip_prefix("minecraft:") == Some(name))
        .map_or(3, |spec| spec.update_interval as u32)
}

/// A bounded queue of not-yet-serialized chunks. The supplied window is a
/// scheduling constraint, not a substitute for vanilla's tracked chunk set.
#[allow(dead_code)]
struct ChunkSendBudget {
    queued: Vec<(i32, i32)>,
    max_chunks_per_tick: usize,
}

#[allow(dead_code)]
impl ChunkSendBudget {
    fn new(max_chunks_per_tick: usize) -> Self {
        Self { queued: Vec::new(), max_chunks_per_tick }
    }

    fn enqueue(&mut self, chunk: (i32, i32)) {
        if !self.queued.contains(&chunk) { self.queued.push(chunk); }
    }

    fn drain_tick(&mut self, center: (i32, i32), radius: u32, look: (f64, f64)) -> Vec<(i32, i32)> {
        // The vanilla rounded-square window reaches radius+1 on its bounding
        // axes; the queue only ever holds window cells, so this is a superset
        // cull, not the tracking predicate itself.
        let radius = i64::from(radius) + 1;
        self.queued.retain(|&(x, z)| {
            (i64::from(x) - i64::from(center.0)).abs() <= radius &&
            (i64::from(z) - i64::from(center.1)).abs() <= radius
        });
        prioritize_chunks(&mut self.queued, center, look);
        let count = self.max_chunks_per_tick.min(self.queued.len());
        self.queued.drain(..count).collect()
    }
}

/// Uniform sections can use a single palette entry; air and light remain
/// independent and neither permits omitting a section from the chunk format.
#[allow(dead_code)]
fn uniform_section<T: Eq + Copy>(states: &[T]) -> Option<T> {
    let (&first, rest) = states.split_first()?;
    rest.iter().all(|state| *state == first).then_some(first)
}

/// Select light arrays already known to be needed within an inclusive band.
/// Outside entries are not asserted empty and must not be encoded as zero.
#[allow(dead_code)]
fn light_sections_in_band(nonempty: &[bool], first: usize, last: usize) -> Vec<usize> {
    nonempty.iter().enumerate().filter_map(|(i, &present)|
        (present && first <= i && i <= last).then_some(i)).collect()
}

/// Inputs are the actual per-player tracked and eligible sets, after all Vanilla
/// distance, passenger, visibility, and chunk-tracking checks have been applied.
#[allow(dead_code)]
fn compute_tracking_diff(previous: &[i32], eligible: &[i32]) -> (Vec<i32>, Vec<i32>, Vec<i32>) {
    let old: std::collections::BTreeSet<_> = previous.iter().copied().collect();
    let new: std::collections::BTreeSet<_> = eligible.iter().copied().collect();
    (new.difference(&old).copied().collect(),
     new.intersection(&old).copied().collect(),
     old.difference(&new).copied().collect())
}

/// A square Chebyshev window. This is a scheduling primitive, not Vanilla's
/// chunk-tracking shape; callers must supply the appropriate actual tracked set.
#[allow(dead_code)]
fn chunk_view_diff(old: (i32, i32), new: (i32, i32), radius: u32)
    -> ChunkViewDelta
{
    fn window(center: (i32, i32), radius: u32) -> std::collections::BTreeSet<(i32, i32)> {
        let r = i64::from(radius);
        let (x, z) = (i64::from(center.0), i64::from(center.1));
        let lo = i64::from(i32::MIN);
        let hi = i64::from(i32::MAX);
        (x.saturating_sub(r).max(lo)..=x.saturating_add(r).min(hi))
            .flat_map(|cx| (z.saturating_sub(r).max(lo)..=z.saturating_add(r).min(hi))
                .map(move |cz| (cx as i32, cz as i32)))
            .collect()
    }
    if old == new { return (Vec::new(), Vec::new()); }
    let before = window(old, radius);
    let after = window(new, radius);
    (after.difference(&before).copied().collect(), before.difference(&after).copied().collect())
}

/// Reorder the complete queue by horizontal look direction, keeping every entry.
/// Center first, then forward/side/behind; ties use distance and coordinates.
#[allow(dead_code)]
fn prioritize_chunks(chunks: &mut [(i32, i32)], center: (i32, i32), look: (f64, f64)) {
    let valid = look.0.is_finite() && look.1.is_finite();
    // Scale first to avoid overflow with large finite look vectors.
    let scale = if valid { look.0.abs().max(look.1.abs()) } else { 0.0 };
    let direction = if scale > 0.0 { (look.0 / scale, look.1 / scale) } else { (0.0, 0.0) };
    chunks.sort_by(|a, b| {
        let key = |&(x, z): &(i32, i32)| {
            let dx = i64::from(x) - i64::from(center.0);
            let dz = i64::from(z) - i64::from(center.1);
            let dot = dx as f64 * direction.0 + dz as f64 * direction.1;
            let rank = if dx == 0 && dz == 0 { 0 } else if dot > 0.0 { 1 }
                else if dot == 0.0 { 2 } else { 3 };
            let distance = i128::from(dx) * i128::from(dx) + i128::from(dz) * i128::from(dz);
            (rank, distance, x, z)
        };
        key(a).cmp(&key(b))
    });
}

/// Block emptiness and light emptiness are independent. `air` must describe all
/// 4096 block states (including cave/void air); each light array is 2048 bytes.
/// The returned booleans mark sections requiring block data and light arrays,
/// respectively. A false light bit means a known all-zero array, not absent data.
#[allow(dead_code)]
fn section_nonempty_masks(air: &[[bool; 4096]], light: &[[u8; 2048]]) -> (Vec<bool>, Vec<bool>) {
    (air.iter().map(|section| section.iter().any(|&is_air| !is_air)).collect(),
     light.iter().map(|section| section.iter().any(|&byte| byte != 0)).collect())
}

#[cfg(test)]
mod entity_spatial_tests {
    use super::*;

    #[test]
    fn entity_spatial_budget_drops_stale_before_draining() {
        let mut budget = ChunkSendBudget::new(2);
        for chunk in [(0, 0), (1, 0), (-1, 0), (0, 1), (5, 5), (1, 0)] { budget.enqueue(chunk); }
        assert_eq!(budget.drain_tick((0, 0), 1, (1.0, 0.0)), vec![(0, 0), (1, 0)]);
        // The scheduling cull retains the documented radius+1 bounding
        // window; (-1, 0) is still inside it after moving the center to (1, 0).
        assert_eq!(
            budget.drain_tick((1, 0), 1, (1.0, 0.0)),
            vec![(0, 1), (-1, 0)]
        );
        assert!(budget.drain_tick((1, 0), 1, (1.0, 0.0)).is_empty());
    }

    #[test]
    fn entity_spatial_intervals_and_section_classification() {
        for (name, ticks) in [("player", 2), ("arrow", 20), ("block_display", 1),
            ("area_effect_cloud", i32::MAX as u32), ("other:cow", 3)] {
            assert_eq!(entity_update_interval_ticks(name), ticks);
        }
        let mut blocks = [0u16; 4096];
        assert_eq!(uniform_section(&blocks), Some(0));
        blocks[4095] = 1;
        assert_eq!(uniform_section(&blocks), None);
        assert_eq!(uniform_section::<u8>(&[]), None);
        assert_eq!(light_sections_in_band(&[true, false, true, true], 1, 2), vec![2]);
    }

    #[test]
    fn entity_spatial_tracking_ranges_and_diffs() {
        for (name, range) in [("minecraft:marker", 0), ("arrow", 4), ("bat", 5),
            ("item", 6), ("zombie", 8), ("cow", 10), ("end_crystal", 16),
            ("minecraft:player", 32), ("other:cow", 5)] {
            assert_eq!(client_tracking_range_chunks(name), range, "{name}");
        }
        assert_eq!(compute_tracking_diff(&[1, 2, 2, 4], &[2, 3, 3, 4]),
            (vec![3], vec![2, 4], vec![1]));
        assert_eq!(compute_tracking_diff(&[2, 1], &[1, 2]), (vec![], vec![1, 2], vec![]));
        assert_eq!(compute_tracking_diff(&[], &[7]), (vec![7], vec![], vec![]));
        assert_eq!(compute_tracking_diff(&[7], &[]), (vec![], vec![], vec![7]));
    }

    #[test]
    fn entity_spatial_chunk_views_diff_at_borders() {
        let (load, unload) = chunk_view_diff((-1, 0), (0, 0), 1);
        assert_eq!(load, vec![(1, -1), (1, 0), (1, 1)]);
        assert_eq!(unload, vec![(-2, -1), (-2, 0), (-2, 1)]);
        assert_eq!(chunk_view_diff((0, 0), (0, 0), 3), (vec![], vec![]));
        let (load, unload) = chunk_view_diff((0, 0), (1, 1), 1);
        assert_eq!(load.len(), 5);
        assert_eq!(unload.len(), 5);
        assert!(load.contains(&(2, 2)) && unload.contains(&(-1, -1)));
        assert_eq!(chunk_view_diff((i32::MAX, 0), (i32::MAX - 1, 0), 1).0,
            vec![(i32::MAX - 2, -1), (i32::MAX - 2, 0), (i32::MAX - 2, 1)]);
    }

    #[test]
    fn entity_spatial_chunk_priority_keeps_all_chunks() {
        let mut chunks: Vec<_> = (-2..=2).flat_map(|x| (-2..=2).map(move |z| (x, z))).collect();
        let original = chunks.clone();
        prioritize_chunks(&mut chunks, (0, 0), (1.0, 0.0));
        assert_eq!(chunks[0], (0, 0));
        assert!(chunks[1..11].iter().all(|&(x, _)| x > 0));
        assert!(chunks[15..].iter().all(|&(x, _)| x < 0));
        let mut sorted = chunks.clone(); sorted.sort_unstable();
        assert_eq!(sorted, original);
        chunks.reverse();
        prioritize_chunks(&mut chunks, (0, 0), (1.0, 0.0));
        assert_ne!(chunks, original);
        let mut repeated = original.clone();
        prioritize_chunks(&mut repeated, (0, 0), (1.0, 0.0));
        assert_eq!(chunks, repeated);
        prioritize_chunks(&mut chunks, (0, 0), (f64::NAN, 0.0));
        assert_eq!(chunks[0], (0, 0));
    }

    #[test]
    fn entity_spatial_section_masks_keep_light_in_air() {
        let mut air = vec![[true; 4096]; 3];
        air[1][4095] = false;
        let mut light = vec![[0; 2048]; 4];
        light[0][0] = 0xff;
        light[2][2047] = 1;
        assert_eq!(section_nonempty_masks(&air, &light),
            (vec![false, true, false], vec![true, false, true, false]));
        assert_eq!(section_nonempty_masks(&[], &[]), (vec![], vec![]));
    }

    fn make_test_entity(idx: usize, eid: i32, x: f64, y: f64, z: f64, w: f32, h: f32) -> SpatialEntity {
        SpatialEntity {
            idx,
            eid,
            is_mob: true,
            is_spectator: false,
            is_pushable: true,
            is_passenger: false,
            is_vehicle: false,
            no_physics: false,
            etype: 22,
            aabb: EntityAabb::from_dimensions(x, y, z, w, h),
            x,
            z,
        }
    }

    #[test]
    fn entity_spatial_grid_matches_brute_force_across_1200_entities_and_chunk_boundaries() {
        let mut entities = Vec::with_capacity(1200);
        // Deterministic LCG across negative/positive chunk boundaries (-48..48) plus dense clusters.
        let mut seed = 0x9e37_79b9_7f4a_7c15u64;
        let mut next_f = || -> f64 {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            ((seed >> 32) as f64) / (u32::MAX as f64)
        };
        for i in 0..1200usize {
            let (x, y, z) = if i < 200 {
                // Straddle chunk boundaries x = -16, 0, 16 and z = -16, 0, 16.
                let bx = [-16.0, 0.0, 16.0][i % 3];
                let bz = [-16.0, 0.0, 16.0][(i / 3) % 3];
                (bx + (next_f() - 0.5) * 1.4, 64.0, bz + (next_f() - 0.5) * 1.4)
            } else {
                ((next_f() - 0.5) * 96.0, 64.0 + (i % 3) as f64 * 0.5, (next_f() - 0.5) * 96.0)
            };
            entities.push(make_test_entity(i, 1000 + i as i32, x, y, z, 0.9, 1.4));
        }

        let grid = EntitySpatialGrid::from_entities(entities.clone());
        let mut scratch = SpatialScratch::default();
        let mut grid_aabb_hits = Vec::new();
        let mut grid_rad_hits = Vec::new();
        let mut total_aabb_matches = 0usize;
        let mut total_rad_matches = 0usize;

        for ent in &entities {
            grid.query_aabb_into(ent.aabb, ent.eid, &mut scratch, &mut grid_aabb_hits);
            let brute_aabb: Vec<u32> = entities
                .iter()
                .enumerate()
                .filter(|(_, o)| o.eid != ent.eid && o.aabb.intersects(ent.aabb))
                .map(|(slot, _)| slot as u32)
                .collect();
            assert_eq!(
                grid_aabb_hits, brute_aabb,
                "AABB query mismatch for eid={}",
                ent.eid
            );
            total_aabb_matches += brute_aabb.len();

            grid.query_radius_xz_into(ent.x, ent.z, 4.0, ent.eid, &mut scratch, &mut grid_rad_hits);
            let brute_rad: Vec<u32> = entities
                .iter()
                .enumerate()
                .filter(|(_, o)| {
                    o.eid != ent.eid
                        && ((o.x - ent.x) * (o.x - ent.x) + (o.z - ent.z) * (o.z - ent.z) < 16.0)
                })
                .map(|(slot, _)| slot as u32)
                .collect();
            assert_eq!(
                grid_rad_hits, brute_rad,
                "radius_xz query mismatch for eid={}",
                ent.eid
            );
            total_rad_matches += brute_rad.len();
        }
        assert!(total_aabb_matches > 100);
        assert!(total_rad_matches > 500);
    }

    #[test]
    fn entity_spatial_grid_chunk_boundary_deduplicates_spanning_entities() {
        // Two entities both straddling x=0 (chunk -1 and chunk 0) and z=0 (chunk -1 and chunk 0).
        let e0 = make_test_entity(0, 1, 0.0, 64.0, 0.0, 0.9, 1.4);
        let e1 = make_test_entity(1, 2, 0.1, 64.0, -0.1, 0.9, 1.4);
        let e2 = make_test_entity(2, 3, 15.9, 64.0, 15.9, 0.9, 1.4);
        let e3 = make_test_entity(3, 4, 16.1, 64.0, 16.1, 0.9, 1.4);
        let grid = EntitySpatialGrid::from_entities(vec![e0, e1, e2, e3]);

        assert_eq!(grid.query_aabb(e0.aabb, e0.eid), vec![1]);
        assert_eq!(grid.query_aabb(e1.aabb, e1.eid), vec![0]);
        assert_eq!(grid.query_aabb(e2.aabb, e2.eid), vec![3]);
        assert_eq!(grid.query_aabb(e3.aabb, e3.eid), vec![2]);
    }

    #[test]
    fn entity_spatial_grid_from_entities_is_identical_across_multi_chunk_distributions() {
        // Multi-chunk distribution: entities in several chunks, entities that
        // straddle chunk boundaries (so one entity writes several cells), and
        // dense duplicates inside cells.
        let mut entities = Vec::new();
        for (x, z) in [
            (0.5, 0.5),
            (15.6, 15.6),
            (0.1, -0.1),
            (16.05, -0.2),
            (-0.05, 0.05),
            (-15.5, -15.5),
            (31.5, -31.5),
            (47.9, 47.9),
            (32.1, -32.1),
            (0.5, 0.5),
        ] {
            entities.push(make_test_entity(
                entities.len(),
                1000 + entities.len() as i32,
                x,
                64.0,
                z,
                0.9,
                1.4,
            ));
        }

        let grid = EntitySpatialGrid::from_entities(entities.clone());
        // A second construction over the same distribution produces the same
        // cell ranges: every cell maps to the identical ascending entry set
        // (absolute offsets may differ — the map is a HashMap).
        let twin = EntitySpatialGrid::from_entities(entities.clone());
        let range_of = |g: &EntitySpatialGrid| {
            g.cells
                .iter()
                .map(|(&cell, &(start, end))| {
                    (cell, g.cell_entries[start as usize..end as usize].to_vec())
                })
                .collect::<HashMap<(i32, i32), Vec<u32>>>()
        };
        assert_eq!(range_of(&grid), range_of(&twin));

        // Every cell range is non-empty, in bounds, ascending by entity slot,
        // and exactly matches the brute-force span of its cell.
        let mut expected: HashMap<(i32, i32), Vec<u32>> = HashMap::new();
        for (slot, ent) in entities.iter().enumerate() {
            let cx0 = spatial_chunk_coord(ent.aabb.min_x);
            let cx1 = spatial_chunk_coord(ent.aabb.max_x);
            let cz0 = spatial_chunk_coord(ent.aabb.min_z);
            let cz1 = spatial_chunk_coord(ent.aabb.max_z);
            for cx in cx0..=cx1 {
                for cz in cz0..=cz1 {
                    expected.entry((cx, cz)).or_default().push(slot as u32);
                }
            }
        }
        assert_eq!(grid.cells.len(), expected.len());
        for (cell, expected_slots) in &expected {
            let (start, end) = grid
                .cells
                .get(cell)
                .copied()
                .unwrap_or_else(|| panic!("missing cell {cell:?}"));
            assert!(end > start, "non-empty range for {cell:?}");
            assert!(end as usize <= grid.cell_entries.len());
            assert_eq!(
                end - start,
                expected_slots.len() as u32,
                "range length for {cell:?}"
            );
            assert_eq!(
                grid.cell_entries[start as usize..end as usize].to_vec(),
                expected_slots.to_vec(),
                "range mismatch for cell {cell:?}"
            );
        }
        assert_eq!(range_of(&grid), expected);

        // Query results match brute force across the multi-chunk distribution.
        let mut scratch = SpatialScratch::default();
        let mut grid_aabb_hits = Vec::new();
        let mut grid_rad_hits = Vec::new();
        for ent in &entities {
            grid.query_aabb_into(ent.aabb, ent.eid, &mut scratch, &mut grid_aabb_hits);
            let brute_aabb: Vec<u32> = entities
                .iter()
                .enumerate()
                .filter(|(_, o)| o.eid != ent.eid && o.aabb.intersects(ent.aabb))
                .map(|(slot, _)| slot as u32)
                .collect();
            assert_eq!(
                grid_aabb_hits, brute_aabb,
                "AABB query mismatch for eid={}",
                ent.eid
            );
            grid.query_radius_xz_into(ent.x, ent.z, 4.0, ent.eid, &mut scratch, &mut grid_rad_hits);
            let brute_rad: Vec<u32> = entities
                .iter()
                .enumerate()
                .filter(|(_, o)| {
                    o.eid != ent.eid
                        && ((o.x - ent.x) * (o.x - ent.x) + (o.z - ent.z) * (o.z - ent.z) < 16.0)
                })
                .map(|(slot, _)| slot as u32)
                .collect();
            assert_eq!(
                grid_rad_hits, brute_rad,
                "radius_xz query mismatch for eid={}",
                ent.eid
            );
        }
    }

    fn spatial_pool_capacities() -> (usize, usize, usize, usize, usize) {
        SPATIAL_POOL.with(|pool| {
            let pool = pool.borrow();
            (
                pool.entities.capacity(),
                pool.cells.capacity(),
                pool.cell_entries.capacity(),
                pool.query_scratch.seen.capacity(),
                pool.query_out.capacity(),
            )
        })
    }

    #[test]
    fn xpoll_entity_scratch_spatial_queries_match_bruteforce_with_zero_growth() {
        // 500 entities straddling chunk borders so multi-cell dedup runs.
        let entities: Vec<SpatialEntity> = (0..500usize)
            .map(|i| {
                let x = (i % 10) as f64 * 3.7 - 18.0;
                let z = (i / 10) as f64 * 5.3 - 120.0;
                SpatialEntity {
                    idx: i,
                    eid: i as i32 + 1,
                    is_mob: i % 2 == 0,
                    is_spectator: false,
                    is_pushable: true,
                    is_passenger: false,
                    is_vehicle: false,
                    no_physics: false,
                    etype: 87,
                    aabb: EntityAabb::from_dimensions(x, 64.0, z, 0.7, 1.3),
                    x,
                    z,
                }
            })
            .collect();
        let grid = EntitySpatialGrid::from_entities(entities);

        // Pooled query path vs the same queries through per-call scratch; both
        // are already brute-force verified by the grid tests above.
        let queries: Vec<(f64, f64, f64)> = (0..500usize)
            .map(|i| {
                let e = &grid.entities[i];
                (e.x + (i % 4) as f64 * 0.3, e.z - (i % 6) as f64 * 0.2, e.aabb.min_x)
            })
            .collect();

        // Warm the pooled path so the timed loops measure steady state.
        with_spatial_query_scratch(|scratch, out| {
            grid.query_radius_xz_into(0.0, 0.0, 24.0, -1, scratch, out);
        });

        let fresh_start = Instant::now();
        let mut expected = Vec::with_capacity(500);
        for &(x, z, ax) in &queries {
            let mut scratch = SpatialScratch::default();
            let mut out = Vec::new();
            grid.query_radius_xz_into(x, z, 24.0, -1, &mut scratch, &mut out);
            let probe = EntityAabb::from_min_max([ax, 63.5, z], [ax + 0.9, 65.0, z + 0.9]);
            grid.query_aabb_into(probe, -1, &mut scratch, &mut out);
            expected.push(out);
        }
        let fresh_us = fresh_start.elapsed().as_micros();

        let pooled_start = Instant::now();
        for (i, &(x, z, ax)) in queries.iter().enumerate() {
            let out = with_spatial_query_scratch(|scratch, out| {
                grid.query_radius_xz_into(x, z, 24.0, -1, scratch, out);
                let probe = EntityAabb::from_min_max([ax, 63.5, z], [ax + 0.9, 65.0, z + 0.9]);
                grid.query_aabb_into(probe, -1, scratch, out);
                out.clone()
            });
            assert_eq!(expected[i], out, "pooled query #{i} must be identical");
        }
        let pooled_us = pooled_start.elapsed().as_micros();
        eprintln!(
            "xpoll_entity_scratch_spatial: fresh {fresh_us}us vs pooled {pooled_us}us over 500 queries"
        );

        // After warmup, 500 more pooled queries must not grow the buffers.
        let (_, _, _, seen_before, out_before) = spatial_pool_capacities();
        for &(x, z, ax) in &queries {
            with_spatial_query_scratch(|scratch, out| {
                grid.query_radius_xz_into(x, z, 24.0, -1, scratch, out);
                let probe = EntityAabb::from_min_max([ax, 63.5, z], [ax + 0.9, 65.0, z + 0.9]);
                grid.query_aabb_into(probe, -1, scratch, out);
            });
        }
        let (_, _, _, seen_after, out_after) = spatial_pool_capacities();
        assert_eq!(seen_before, seen_after, "dedup stamp buffer must not grow");
        assert_eq!(out_before, out_after, "pooled output buffer must not grow");
        assert!(seen_before >= 500 && out_before > 0, "warmup must fill scratch");

        // Dropping a grid returns its storage; the next rebuild reuses it
        // instead of reallocating the entity list, cell map and entry array.
        drop(grid);
        let (entities_cap, cells_cap, entries_cap, _, _) = spatial_pool_capacities();
        assert!(
            entities_cap >= 500 && cells_cap > 0 && entries_cap > 0,
            "dropped grid storage must be stashed for reuse: {entities_cap}/{cells_cap}/{entries_cap}"
        );

        // The rebuilt grid uses the pooled storage and stays bit-identical.
        let rebuilt = EntitySpatialGrid::from_entities((0..500usize)
            .map(|i| {
                let x = (i % 10) as f64 * 3.7 - 18.0;
                let z = (i / 10) as f64 * 5.3 - 120.0;
                SpatialEntity {
                    idx: i,
                    eid: i as i32 + 1,
                    is_mob: i % 2 == 0,
                    is_spectator: false,
                    is_pushable: true,
                    is_passenger: false,
                    is_vehicle: false,
                    no_physics: false,
                    etype: 87,
                    aabb: EntityAabb::from_dimensions(x, 64.0, z, 0.7, 1.3),
                    x,
                    z,
                }
            })
            .collect());
        let mut scratch = SpatialScratch::default();
        let mut hits = Vec::new();
        let mut all = Vec::new();
        for &(x, z, ax) in &queries {
            rebuilt.query_radius_xz_into(x, z, 24.0, -1, &mut scratch, &mut hits);
            let probe = EntityAabb::from_min_max([ax, 63.5, z], [ax + 0.9, 65.0, z + 0.9]);
            rebuilt.query_aabb_into(probe, -1, &mut scratch, &mut hits);
            all.extend_from_slice(&hits);
        }
        assert_eq!(
            all,
            expected.iter().flatten().copied().collect::<Vec<u32>>(),
            "rebuilt-grid queries must match the first grid's results"
        );
    }

    #[test]
    fn entity_spatial_grid_skips_non_finite_aabbs_via_occupancy_mask() {
        // A non-finite AABB never lands in a cell; the occupancy mask must
        // carry its slot as zero so pass 2 skips it outright.
        let mut entities: Vec<SpatialEntity> = (0..70usize)
            .map(|i| make_test_entity(i, i as i32 + 1, i as f64 - 32.0, 64.0, 0.0, 0.9, 1.4))
            .collect();
        entities[7].aabb.min_x = f64::NAN;
        entities[33].aabb.max_z = f64::INFINITY;
        entities[69].aabb.min_z = f64::NEG_INFINITY;
        let grid = EntitySpatialGrid::from_entities(entities.clone());

        let mut scratch = SpatialScratch::default();
        let mut hits = Vec::new();
        // A window covering every entity must surface every live slot in
        // ascending order and none of the masked ones.
        grid.query_aabb_into(
            EntityAabb::from_min_max([-512.0, -512.0, -512.0], [512.0, 512.0, 512.0]),
            -1,
            &mut scratch,
            &mut hits,
        );
        let live: Vec<u32> = (0..70u32).filter(|s| ![7, 33, 69].contains(s)).collect();
        assert_eq!(hits, live, "masked slots must not reach queries");
        for &slot in &hits {
            assert!(grid.entities[slot as usize].aabb.min_x.is_finite());
        }
    }
}
