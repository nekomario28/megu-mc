// SPDX-License-Identifier: LGPL-3.0-or-later
#![forbid(unsafe_code)]
//! Reusable spatial, crowd scheduling, synchronization, and render budgeting
//! primitives. These are policy helpers; callers still own world collision,
//! packet emission, damage acceptance, and GPU submission.

use std::collections::HashMap;
use std::hash::{BuildHasherDefault, Hasher};

pub const ENTITY_PUSH_MIN_ABS_MAX: f64 = 0.009999999776482582_f64;
pub const ENTITY_PUSH_STRENGTH: f64 = 0.05000000074505806_f64;
pub const ENTITY_CRAMMING_DAMAGE: f32 = 6.0_f32;
pub const DEFAULT_MAX_ENTITY_CRAMMING: i32 = 24_i32;

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Aabb {
    pub min_x: f64,
    pub min_y: f64,
    pub min_z: f64,
    pub max_x: f64,
    pub max_y: f64,
    pub max_z: f64,
}
impl Aabb {
    pub fn intersects(self, other: Self) -> bool {
        self.min_x < other.max_x
            && self.max_x > other.min_x
            && self.min_y < other.max_y
            && self.max_y > other.min_y
            && self.min_z < other.max_z
            && self.max_z > other.min_z
    }
    fn valid(self) -> bool {
        [
            self.min_x, self.min_y, self.min_z, self.max_x, self.max_y, self.max_z,
        ]
        .iter()
        .all(|v| v.is_finite())
            && self.min_x <= self.max_x
            && self.min_y <= self.max_y
            && self.min_z <= self.max_z
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct MicroCell {
    pub x: i32,
    pub y: i32,
    pub z: i32,
}
impl MicroCell {
    pub fn at(x: f64, y: f64, z: f64) -> Option<Self> {
        if !x.is_finite() || !y.is_finite() || !z.is_finite() {
            return None;
        }
        let (x, y, z) = (x.floor(), y.floor(), z.floor());
        if x < i32::MIN as f64
            || x >= i32::MAX as f64 + 1.0
            || y < i32::MIN as f64
            || y >= i32::MAX as f64 + 1.0
            || z < i32::MIN as f64
            || z >= i32::MAX as f64 + 1.0
        {
            return None;
        }
        Some(Self {
            x: (x as i32) >> 1,
            y: (y as i32) >> 1,
            z: (z as i32) >> 1,
        })
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct CrowdEntity {
    pub id: i32,
    pub x: f64,
    pub y: f64,
    pub z: f64,
    pub aabb: Aabb,
    pub pushable: bool,
    pub spectator: bool,
    pub passenger: bool,
    pub vehicle: bool,
    pub no_physics: bool,
    pub same_vehicle_group: i64,
}
impl CrowdEntity {
    pub fn cell(self) -> Option<MicroCell> {
        MicroCell::at(self.x, self.y, self.z)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Entry {
    cell: MicroCell,
    entity: usize,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CellRange {
    pub cell: MicroCell,
    pub start: usize,
    pub end: usize,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct EntityPair {
    pub first: usize,
    pub second: usize,
}

/// A reusable, deterministic 3D micro-cell index. `rebuild` retains its flat
/// entry and cell capacities. Entities spanning cell edges are inserted into
/// every touched cell; returned pairs are sorted and deduplicated.
#[derive(Default)]
pub struct MicroCellGrid3D {
    entries: Vec<Entry>,
    cells: Vec<CellRange>,
    pairs: Vec<EntityPair>,
    seen: Vec<u32>,
    stamp: u32,
    skipped_by_sap: u64,
}
impl MicroCellGrid3D {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn clear(&mut self) {
        self.entries.clear();
        self.cells.clear();
        self.pairs.clear();
        self.skipped_by_sap = 0;
    }
    pub fn cells(&self) -> &[CellRange] {
        &self.cells
    }
    pub fn rebuild(&mut self, entities: &[CrowdEntity]) {
        self.clear();
        for (idx, e) in entities.iter().enumerate() {
            if !e.aabb.valid() {
                continue;
            }
            let (Some(lo), Some(hi)) = (
                MicroCell::at(e.aabb.min_x, e.aabb.min_y, e.aabb.min_z),
                MicroCell::at(e.aabb.max_x, e.aabb.max_y, e.aabb.max_z),
            ) else {
                continue;
            };
            let (dx, dy, dz) = (
                hi.x.saturating_sub(lo.x).unsigned_abs().min(64),
                hi.y.saturating_sub(lo.y).unsigned_abs().min(64),
                hi.z.saturating_sub(lo.z).unsigned_abs().min(64),
            );
            for x in lo.x..=lo.x.saturating_add(dx as i32) {
                for y in lo.y..=lo.y.saturating_add(dy as i32) {
                    for z in lo.z..=lo.z.saturating_add(dz as i32) {
                        self.entries.push(Entry {
                            cell: MicroCell { x, y, z },
                            entity: idx,
                        });
                    }
                }
            }
        }
        self.entries.sort_unstable_by(|a, b| {
            a.cell
                .cmp(&b.cell)
                .then_with(|| {
                    entities[a.entity]
                        .aabb
                        .min_x
                        .total_cmp(&entities[b.entity].aabb.min_x)
                })
                .then_with(|| a.entity.cmp(&b.entity))
        });
        let mut start = 0;
        while start < self.entries.len() {
            let cell = self.entries[start].cell;
            let mut end = start + 1;
            while end < self.entries.len() && self.entries[end].cell == cell {
                end += 1
            }
            self.cells.push(CellRange { cell, start, end });
            start = end;
        }
    }
    /// Return broadphase AABB overlaps, once per entity pair. X-sorted runs
    /// stop as soon as the next minimum X reaches the current maximum X.
    pub fn overlapping_pairs(&mut self, entities: &[CrowdEntity]) -> &[EntityPair] {
        self.collect_pairs(entities, &[]);
        &self.pairs
    }
    fn collect_pairs(&mut self, entities: &[CrowdEntity], excluded_cells: &[MicroCell]) {
        self.pairs.clear();
        self.skipped_by_sap = 0;
        if self.seen.len() < entities.len() {
            self.seen.resize(entities.len(), 0);
        }
        for (ia, ea) in entities.iter().enumerate() {
            if !ea.aabb.valid() {
                continue;
            }
            let (Some(lo), Some(hi)) = (
                MicroCell::at(ea.aabb.min_x, ea.aabb.min_y, ea.aabb.min_z),
                MicroCell::at(ea.aabb.max_x, ea.aabb.max_y, ea.aabb.max_z),
            ) else {
                continue;
            };
            let end = cell_end(lo, hi);
            self.stamp = self.stamp.wrapping_add(1);
            if self.stamp == 0 {
                self.seen.fill(0);
                self.stamp = 1;
            }
            let stamp = self.stamp;
            for x in lo.x..=end.x {
                for y in lo.y..=end.y {
                    for z in lo.z..=end.z {
                        let cell = MicroCell { x, y, z };
                        if excluded_cells.binary_search(&cell).is_ok() {
                            continue;
                        }
                        let Ok(cell_idx) = self.cells.binary_search_by_key(&cell, |c| c.cell)
                        else {
                            continue;
                        };
                        let range = self.cells[cell_idx];
                        for entry_idx in range.start..range.end {
                            let ib = self.entries[entry_idx].entity;
                            if ib <= ia || self.seen[ib] == stamp {
                                continue;
                            }
                            self.seen[ib] = stamp;
                            let eb = entities[ib];
                            if eb.aabb.min_x >= ea.aabb.max_x {
                                self.skipped_by_sap += (range.end - entry_idx) as u64;
                                break;
                            }
                            if ea.aabb.intersects(eb.aabb) {
                                self.pairs.push(EntityPair {
                                    first: ia,
                                    second: ib,
                                });
                            }
                        }
                    }
                }
            }
        }
        self.pairs.sort_unstable();
    }
}

fn cell_end(lo: MicroCell, hi: MicroCell) -> MicroCell {
    MicroCell {
        x: lo
            .x
            .saturating_add(hi.x.saturating_sub(lo.x).unsigned_abs().min(64) as i32),
        y: lo
            .y
            .saturating_add(hi.y.saturating_sub(lo.y).unsigned_abs().min(64) as i32),
        z: lo
            .z
            .saturating_add(hi.z.saturating_sub(lo.z).unsigned_abs().min(64) as i32),
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct PushOutcome {
    pub impulse_x: f64,
    pub impulse_z: f64,
    pub pushable_neighbors: u32,
    pub non_passenger_neighbors: u32,
    pub cramming_damage: f32,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DensePushStats {
    pub exact_pairs: u64,
    pub bounded_pairs: u64,
    pub hyperdense_cells: u64,
    pub crammed_entities: u64,
    pub skipped_by_sap: u64,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PushMode {
    Exact,
    BoundedApproximation,
}
pub struct DensePushSolver {
    pub exact_threshold: usize,
    pub bounded_neighbors: usize,
    pub outcomes: Vec<PushOutcome>,
    pub stats: DensePushStats,
    pub mode: PushMode,
    grid: MicroCellGrid3D,
    dense_cells: Vec<MicroCell>,
    members: Vec<usize>,
    seen: Vec<u32>,
    dense_entities: Vec<bool>,
    stamp: u32,
}
impl Default for DensePushSolver {
    fn default() -> Self {
        Self::new()
    }
}
impl DensePushSolver {
    pub fn new() -> Self {
        Self {
            exact_threshold: 32,
            bounded_neighbors: 16,
            outcomes: Vec::new(),
            stats: DensePushStats::default(),
            mode: PushMode::Exact,
            grid: MicroCellGrid3D::new(),
            dense_cells: Vec::new(),
            members: Vec::new(),
            seen: Vec::new(),
            dense_entities: Vec::new(),
            stamp: 0,
        }
    }
    /// Solves each exact pair once. Over-threshold cells use a bounded,
    /// deterministic sample plus centroid repulsion. Approximation is explicit
    /// in `mode`; it must not be described as vanilla parity.
    pub fn solve(&mut self, entities: &[CrowdEntity], max_cramming: i32, cram_rolls: &[u64]) {
        self.outcomes.clear();
        self.outcomes.resize(entities.len(), PushOutcome::default());
        self.stats = DensePushStats::default();
        self.mode = PushMode::Exact;
        self.grid.rebuild(entities);
        if self.seen.len() < entities.len() {
            self.seen.resize(entities.len(), 0);
        }
        self.dense_entities.clear();
        self.dense_entities.resize(entities.len(), false);
        self.dense_cells.clear();
        for c in &self.grid.cells {
            let mut pushable = 0;
            for entry in &self.grid.entries[c.start..c.end] {
                let e = entities[entry.entity];
                if e.pushable && !e.spectator {
                    pushable += 1;
                }
            }
            if pushable > self.exact_threshold {
                self.dense_cells.push(c.cell);
            }
        }
        for c in &self.grid.cells {
            if self.dense_cells.binary_search(&c.cell).is_ok() {
                for entry in &self.grid.entries[c.start..c.end] {
                    self.dense_entities[entry.entity] = true;
                }
            }
        }
        // Accurate cramming census is independently bounded by max_cramming.
        // This preserves the threshold decision without requiring all pairs.
        for i in 0..entities.len() {
            if !entities[i].pushable || entities[i].spectator {
                continue;
            }
            let Some(lo) = MicroCell::at(
                entities[i].aabb.min_x,
                entities[i].aabb.min_y,
                entities[i].aabb.min_z,
            ) else {
                continue;
            };
            let Some(hi) = MicroCell::at(
                entities[i].aabb.max_x,
                entities[i].aabb.max_y,
                entities[i].aabb.max_z,
            ) else {
                continue;
            };
            self.stamp = self.stamp.wrapping_add(1);
            if self.stamp == 0 {
                self.seen.fill(0);
                self.stamp = 1;
            }
            let stamp = self.stamp;
            let end = cell_end(lo, hi);
            let mut count = 0u32;
            let mut non = 0u32;
            'cells: for x in lo.x..=end.x {
                for y in lo.y..=end.y {
                    for z in lo.z..=end.z {
                        let cell = MicroCell { x, y, z };
                        let Ok(cell_idx) = self.grid.cells.binary_search_by_key(&cell, |c| c.cell)
                        else {
                            continue;
                        };
                        let range = self.grid.cells[cell_idx];
                        for entry in &self.grid.entries[range.start..range.end] {
                            let j = entry.entity;
                            if j == i || self.seen[j] == stamp {
                                continue;
                            }
                            self.seen[j] = stamp;
                            let e = entities[j];
                            if e.id == entities[i].id
                                || !e.pushable
                                || e.spectator
                                || !entities[i].aabb.intersects(e.aabb)
                            {
                                continue;
                            }
                            count = count.saturating_add(1);
                            if !e.passenger {
                                non = non.saturating_add(1)
                            }
                            if max_cramming > 0 && non >= max_cramming as u32 {
                                break 'cells;
                            }
                        }
                    }
                }
            }
            self.outcomes[i].pushable_neighbors = count;
            self.outcomes[i].non_passenger_neighbors = non;
            if max_cramming > 0
                && non >= max_cramming as u32
                && cram_rolls.get(i).copied().unwrap_or(1) % 4 == 0
            {
                self.outcomes[i].cramming_damage = ENTITY_CRAMMING_DAMAGE;
                self.stats.crammed_entities += 1;
            }
        }
        self.grid.collect_pairs(entities, &self.dense_cells);
        self.stats.skipped_by_sap = self.grid.skipped_by_sap;
        for pair_idx in 0..self.grid.pairs.len() {
            let p = self.grid.pairs[pair_idx];
            if self.dense_entities[p.first] || self.dense_entities[p.second] {
                continue;
            }
            let a = entities[p.first];
            let b = entities[p.second];
            self.stats.exact_pairs += 1;
            let (left, right) = self.outcomes.split_at_mut(p.second);
            apply_exact_pair(a, b, &mut left[p.first], &mut right[0]);
        }
        for c_idx in 0..self.grid.cells.len() {
            let c = self.grid.cells[c_idx];
            if self.dense_cells.binary_search(&c.cell).is_err() {
                continue;
            }
            self.members.clear();
            for entry in &self.grid.entries[c.start..c.end] {
                let e = entities[entry.entity];
                if e.pushable && !e.spectator {
                    self.members.push(entry.entity);
                }
            }
            self.members.sort_unstable();
            self.members.dedup();
            self.mode = PushMode::BoundedApproximation;
            self.stats.hyperdense_cells += 1;
            let n = self.members.len();
            let mut cx = 0.0;
            let mut cz = 0.0;
            for (slot, &i) in self.members.iter().enumerate() {
                let weight = slot as f64 / (slot + 1) as f64;
                cx = cx * weight + entities[i].x / (slot + 1) as f64;
                cz = cz * weight + entities[i].z / (slot + 1) as f64;
            }
            let take = self.bounded_neighbors.min(n.saturating_sub(1));
            for slot in 0..n {
                let i = self.members[slot];
                let mut sx = 0.0;
                let mut sz = 0.0;
                let mut used = 0;
                for k in 1..=take {
                    let j = self.members[(slot + k * prime_stride(n)) % n];
                    self.stats.bounded_pairs += 1;
                    if i == j || !entities[i].aabb.intersects(entities[j].aabb) {
                        continue;
                    }
                    if let Some((x, z)) = vanilla_pair_delta(entities[i], entities[j]) {
                        sx += x;
                        sz += z
                    }
                    used += 1;
                }
                let rx = entities[i].x - cx;
                let rz = entities[i].z - cz;
                let d = rx.abs().max(rz.abs());
                let (rx, rz) = if d > 1e-9 {
                    let len = d.sqrt();
                    if len.is_finite() {
                        (
                            rx / len * ENTITY_PUSH_STRENGTH,
                            rz / len * ENTITY_PUSH_STRENGTH,
                        )
                    } else {
                        (0.0, 0.0)
                    }
                } else {
                    (0.0, 0.0)
                };
                let scale = 1.0 / (used.max(1) as f64);
                self.outcomes[i].impulse_x =
                    (self.outcomes[i].impulse_x + (sx * scale) + rx).clamp(-0.05, 0.05);
                self.outcomes[i].impulse_z =
                    (self.outcomes[i].impulse_z + (sz * scale) + rz).clamp(-0.05, 0.05);
            }
        }
    }
    pub fn outcomes(&self) -> &[PushOutcome] {
        &self.outcomes
    }
}
fn prime_stride(n: usize) -> usize {
    if n <= 3 {
        return 1;
    }
    let mut candidate = (n / 2).max(2);
    if candidate > 2 && candidate % 2 == 0 {
        candidate -= 1;
    }
    while candidate > 2 {
        if is_prime(candidate) && gcd(candidate, n) == 1 {
            return candidate;
        }
        candidate -= 2;
    }
    let mut candidate = (n / 2 + 1).max(3);
    if candidate % 2 == 0 {
        candidate += 1;
    }
    while candidate < n {
        if is_prime(candidate) && gcd(candidate, n) == 1 {
            return candidate;
        }
        candidate += 2;
    }
    1
}
fn is_prime(n: usize) -> bool {
    if n < 2 {
        return false;
    }
    let mut d = 2;
    while d <= n / d {
        if n % d == 0 {
            return false;
        }
        d += if d == 2 { 1 } else { 2 };
    }
    true
}
fn gcd(mut a: usize, mut b: usize) -> usize {
    while b != 0 {
        let r = a % b;
        a = b;
        b = r;
    }
    a
}
fn vanilla_pair_delta(a: CrowdEntity, b: CrowdEntity) -> Option<(f64, f64)> {
    if a.no_physics
        || b.no_physics
        || (a.same_vehicle_group != 0 && a.same_vehicle_group == b.same_vehicle_group)
    {
        return None;
    }
    let mut dx = b.x - a.x;
    let mut dz = b.z - a.z;
    let d = dx.abs().max(dz.abs());
    if !d.is_finite() || d < ENTITY_PUSH_MIN_ABS_MAX {
        return None;
    }
    let d = d.sqrt();
    dx /= d;
    dz /= d;
    let scale = (1.0 / d).min(1.0);
    dx *= scale * ENTITY_PUSH_STRENGTH;
    dz *= scale * ENTITY_PUSH_STRENGTH;
    Some((dx, dz))
}
fn apply_exact_pair(a: CrowdEntity, b: CrowdEntity, oa: &mut PushOutcome, ob: &mut PushOutcome) {
    if !a.pushable || a.spectator || !b.pushable || b.spectator {
        return;
    }
    let Some((dx, dz)) = vanilla_pair_delta(a, b) else {
        return;
    };
    if !a.vehicle {
        oa.impulse_x -= dx;
        oa.impulse_z -= dz
    }
    if !b.vehicle {
        ob.impulse_x += dx;
        ob.impulse_z += dz
    }
}

#[derive(Default)]
struct FastHasher(u64);
impl Hasher for FastHasher {
    fn finish(&self) -> u64 {
        self.0
    }
    fn write(&mut self, b: &[u8]) {
        let mut h = 0xcbf29ce484222325u64;
        for x in b {
            h = (h ^ u64::from(*x)).wrapping_mul(0x100000001b3)
        }
        self.0 = h
    }
    fn write_u64(&mut self, i: u64) {
        let mut z = i.wrapping_add(0x9e3779b97f4a7c15);
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d049bb133111eb);
        self.0 = z ^ (z >> 31)
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct CrowdSyncFlags {
    pub player: bool,
    pub boss: bool,
    pub projectile: bool,
    pub hurt: bool,
    pub dying_or_crammed: bool,
    pub vy: f64,
    pub breeding: bool,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CrowdSyncStats {
    pub sent: u64,
    pub suppressed: u64,
    pub forced_periodic: u64,
    pub critical_bypass: u64,
}
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SyncDecision {
    pub send: bool,
    pub deadband: f64,
    pub forced: bool,
    pub critical: bool,
}
#[derive(Default)]
pub struct CrowdSyncGovernor {
    pub stats: CrowdSyncStats,
}
impl CrowdSyncGovernor {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn decide(
        &mut self,
        eid: i32,
        tick: u64,
        density: u32,
        delta: f64,
        flags: CrowdSyncFlags,
    ) -> SyncDecision {
        let critical = flags.player
            || flags.boss
            || flags.projectile
            || flags.hurt
            || flags.dying_or_crammed
            || flags.vy.abs() > 0.05
            || flags.breeding;
        let (band, stride) = if density >= 65 {
            (0.06, 20)
        } else if density >= 25 {
            (0.035, 12)
        } else if density >= 9 {
            (0.015, 8)
        } else {
            (0.0, 0)
        };
        let forced = stride > 0 && ((eid.unsigned_abs() as u64).wrapping_add(tick) % stride == 0);
        let send = critical || density <= 8 || !delta.is_finite() || delta.abs() >= band || forced;
        if send {
            self.stats.sent += 1;
            if critical {
                self.stats.critical_bypass += 1
            }
            if forced && !critical && delta.abs() < band {
                self.stats.forced_periodic += 1
            }
        } else {
            self.stats.suppressed += 1
        }
        SyncDecision {
            send,
            deadband: band,
            forced,
            critical,
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct CrowdSleepFlags {
    pub combat: bool,
    pub hurt: bool,
    pub water_or_lava: bool,
    pub breeding: bool,
    pub moved: bool,
    pub gravity: bool,
    pub cramming: bool,
}
#[derive(Default)]
pub struct CrowdSleepGovernor;
impl CrowdSleepGovernor {
    pub fn stride(density: u32, stationary_ticks: u64) -> u64 {
        let level = if density < 9 || stationary_ticks < 20 {
            1
        } else if stationary_ticks < 60 {
            2
        } else if stationary_ticks < 120 {
            3
        } else if stationary_ticks < 240 {
            5
        } else if stationary_ticks < 480 {
            11
        } else {
            19
        };
        if density < 9 {
            1
        } else {
            level
        }
    }
    /// Decides only whether optional idle AI may run. Gravity, fluids,
    /// cramming, combat, hurt, breeding, and movement always force a run; the
    /// caller must still execute physics and damage every tick.
    pub fn should_run_ai(
        eid: i32,
        tick: u64,
        density: u32,
        stationary_ticks: u64,
        flags: CrowdSleepFlags,
    ) -> bool {
        if flags.combat
            || flags.hurt
            || flags.water_or_lava
            || flags.breeding
            || flags.moved
            || flags.gravity
            || flags.cramming
        {
            return true;
        }
        let stride = Self::stride(density, stationary_ticks);
        (eid.unsigned_abs() as u64 + tick) % stride == 0
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FloorSupport {
    pub block_x: i32,
    pub block_y: i32,
    pub block_z: i32,
    pub section_rev: u64,
    pub has_support: bool,
}
#[derive(Default)]
pub struct FloorSupportCache {
    entries: HashMap<i32, FloorSupport, BuildHasherDefault<FastHasher>>,
}
impl FloorSupportCache {
    pub fn get(&self, eid: i32, block: (i32, i32, i32), rev: u64) -> Option<bool> {
        self.entries
            .get(&eid)
            .filter(|v| {
                (v.block_x, v.block_y, v.block_z, v.section_rev) == (block.0, block.1, block.2, rev)
            })
            .map(|v| v.has_support)
    }
    pub fn insert(&mut self, eid: i32, block: (i32, i32, i32), rev: u64, has_support: bool) {
        self.entries.insert(
            eid,
            FloorSupport {
                block_x: block.0,
                block_y: block.1,
                block_z: block.2,
                section_rev: rev,
                has_support,
            },
        );
    }
    pub fn invalidate(&mut self, eid: i32) {
        self.entries.remove(&eid);
    }
    pub fn clear(&mut self) {
        self.entries.clear()
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct RenderEntity {
    pub id: i32,
    pub x: f64,
    pub y: f64,
    pub z: f64,
    pub camera_distance: f64,
    pub player: bool,
    pub boss: bool,
    pub custom_named: bool,
    pub hurt: bool,
    pub baby_in_love: bool,
    pub combat_target: bool,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RenderLod {
    #[default]
    Full,
    CulledBuried,
}
#[derive(Clone, Debug)]
pub struct CrowdRenderBudget {
    pub max_rigs_per_cell: usize,
    pub max_total_rigs: usize,
    pub lods: Vec<RenderLod>,
}
impl Default for CrowdRenderBudget {
    fn default() -> Self {
        Self {
            max_rigs_per_cell: 12,
            max_total_rigs: 512,
            lods: Vec::new(),
        }
    }
}
impl CrowdRenderBudget {
    pub fn apply(&mut self, entities: &[RenderEntity]) -> &[RenderLod] {
        self.lods.clear();
        self.lods.resize(entities.len(), RenderLod::Full);
        let mut groups: HashMap<(i32, i32, i32), Vec<usize>, BuildHasherDefault<FastHasher>> =
            HashMap::default();
        for (i, e) in entities.iter().enumerate() {
            if e.player || e.boss || e.custom_named || e.hurt || e.baby_in_love || e.combat_target {
                continue;
            }
            if let (Some(x), Some(y), Some(z)) = (floor_i32(e.x), floor_i32(e.y), floor_i32(e.z)) {
                groups.entry((x, y, z)).or_default().push(i)
            }
        }
        let mut order = Vec::new();
        for ids in groups.values_mut() {
            ids.sort_by(|a, b| {
                entities[*a]
                    .camera_distance
                    .total_cmp(&entities[*b].camera_distance)
                    .then(entities[*a].id.cmp(&entities[*b].id))
            });
            if self.max_rigs_per_cell > 0 {
                for &i in ids.iter().skip(self.max_rigs_per_cell) {
                    self.lods[i] = RenderLod::CulledBuried
                }
                order.extend(ids.iter().take(self.max_rigs_per_cell).copied())
            } else {
                order.extend(ids.iter().copied())
            }
        }
        order.sort_by(|a, b| {
            entities[*a]
                .camera_distance
                .total_cmp(&entities[*b].camera_distance)
                .then(entities[*a].id.cmp(&entities[*b].id))
        });
        for &i in order.iter().skip(self.max_total_rigs) {
            self.lods[i] = RenderLod::CulledBuried
        }
        &self.lods
    }
}
fn floor_i32(v: f64) -> Option<i32> {
    if !v.is_finite() {
        return None;
    }
    let f = v.floor();
    if f < i32::MIN as f64 || f >= i32::MAX as f64 + 1.0 {
        None
    } else {
        Some(f as i32)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PoseQuality {
    Full,
    HalfRate,
    CoarseStatic,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct CrowdPoseKey {
    pub limb_swing: i32,
    pub head_yaw: i32,
    pub pitch: i32,
    pub quality: PoseQuality,
}
pub fn quantize_crowd_pose_key(
    limb_swing: f32,
    head_yaw: f32,
    pitch: f32,
    distance: f64,
    density: u32,
) -> CrowdPoseKey {
    let q = if density >= 65 && distance > 12.0 {
        PoseQuality::CoarseStatic
    } else if density >= 25 || distance > 24.0 {
        PoseQuality::HalfRate
    } else {
        PoseQuality::Full
    };
    let step = match q {
        PoseQuality::Full => 1.0,
        PoseQuality::HalfRate => 2.0,
        PoseQuality::CoarseStatic => 4.0,
    };
    let quant = |v: f32| {
        if v.is_finite() {
            (v / step).round() as i32
        } else {
            0
        }
    };
    CrowdPoseKey {
        limb_swing: quant(limb_swing),
        head_yaw: quant(head_yaw),
        pitch: quant(pitch),
        quality: q,
    }
}
