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
        let mut counts: HashMap<(i32, i32), u32> = HashMap::with_capacity(entities.len().max(1));
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
                    *counts.entry((cx, cz)).or_insert(0) += 1;
                    total_entries += 1;
                }
            }
        }

        let mut cells: HashMap<(i32, i32), (u32, u32)> = HashMap::with_capacity(counts.len());
        let mut cursors: HashMap<(i32, i32), u32> = HashMap::with_capacity(counts.len());
        let mut offset = 0u32;
        for (key, cnt) in counts {
            let end = offset + cnt;
            cells.insert(key, (offset, end));
            cursors.insert(key, offset);
            offset = end;
        }

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
                    if let Some(cur) = cursors.get_mut(&(cx, cz)) {
                        cell_entries[*cur as usize] = slot as u32;
                        *cur += 1;
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

#[cfg(test)]
mod entity_spatial_tests {
    use super::*;

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
}
