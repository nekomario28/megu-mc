// M3 P5 Spatial Grid: Chunk-Cell Entity Spatial Grid Index (`EntitySpatialGrid`)
// Eliminates O(N^2) neighborhood scans in `mob_tick` (`pushEntities` AABB overlap + breeding radius).

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

struct EntitySpatialGrid {
    entities: Vec<SpatialEntity>,
    cells: HashMap<(i32, i32), (u32, u32)>,
    cell_entries: Vec<u32>,
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

impl EntitySpatialGrid {
    fn from_entities(entities: Vec<SpatialEntity>) -> Self {
        // Pass 1: per-chunk entity counts. The single `cells` map holds
        // `(0, count)` — range start in the first field, write cursor in the
        // second — so the separate `counts` map disappears.
        let mut cells: HashMap<(i32, i32), (u32, u32)> =
            HashMap::with_capacity(entities.len().max(1));
        let mut total_entries = 0usize;

        for ent in &entities {
            if !ent.aabb.min_x.is_finite()
                || !ent.aabb.max_x.is_finite()
                || !ent.aabb.min_z.is_finite()
                || !ent.aabb.max_z.is_finite()
            {
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

        // Conversion pass: `(0, count)` becomes `(start, start)` — the range
        // start in the first field and the cursor in the second.
        let mut offset = 0u32;
        for value in cells.values_mut() {
            let count = value.1;
            value.0 = offset;
            value.1 = offset;
            offset += count;
        }

        // Pass 2: fill `cell_entries` through the per-cell cursors. After the
        // pass each value reads `(start, start + count)` — the query range.
        let mut cell_entries = vec![0u32; total_entries];
        for (slot, ent) in entities.iter().enumerate() {
            if !ent.aabb.min_x.is_finite()
                || !ent.aabb.max_x.is_finite()
                || !ent.aabb.min_z.is_finite()
                || !ent.aabb.max_z.is_finite()
            {
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
                    if let Some(cursor) = cells.get_mut(&(cx, cz)) {
                        cell_entries[cursor.1 as usize] = slot as u32;
                        cursor.1 += 1;
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
        let mut entities = Vec::with_capacity(mobs_snap.len() + cls_snap.len());
        for (idx, m) in mobs_snap.iter().enumerate() {
            let (x, y, z, _, _) = *m.pos.lock().unwrap();
            let alive = *m.hp.lock().unwrap() > 0.0;
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
            let (x, y, z, _, _, _) = *c.pos.lock().unwrap();
            let alive = *c.hp.lock().unwrap() > 0.0;
            let spectator = c.is_spectator();
            let sneaking = *c.sneaking.lock().unwrap();
            let fall_fly = *c.fall_fly.lock().unwrap();
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
        for cx in cx0..=cx1 {
            for cz in cz0..=cz1 {
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
        }
        out.sort_unstable();
    }

    #[allow(dead_code)]
    fn query_aabb(&self, aabb: EntityAabb, exclude_eid: i32) -> Vec<u32> {
        let mut scratch = SpatialScratch::default();
        let mut out = Vec::new();
        self.query_aabb_into(aabb, exclude_eid, &mut scratch, &mut out);
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
        for cx in cx0..=cx1 {
            for cz in cz0..=cz1 {
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
        }
        out.sort_unstable();
    }

    #[allow(dead_code)]
    fn query_radius_xz(&self, x: f64, z: f64, r: f64, exclude_eid: i32) -> Vec<u32> {
        let mut scratch = SpatialScratch::default();
        let mut out = Vec::new();
        self.query_radius_xz_into(x, z, r, exclude_eid, &mut scratch, &mut out);
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
    -> (Vec<(i32, i32)>, Vec<(i32, i32)>)
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
}
