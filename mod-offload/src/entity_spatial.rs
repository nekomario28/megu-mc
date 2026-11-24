// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later

//! Standalone, side-effect-free spatial kernels. The caller owns world state,
//! movement, damage, random draws, and the AI/visibility rules not represented
//! by these inputs. A spatial index reduces broad-phase work to O(N + P), where
//! P is the number of candidate pairs; dense overlapping crowds have O(N²) P.

use std::collections::HashMap;

const CELL_SIZE: f64 = 2.0;
const MAX_INDEX_CELLS: i64 = 64;
const MIN_PUSH_SEPARATION: f64 = 0.009999999776482582; // Vanilla 0.01f
const PUSH_STRENGTH: f64 = 0.05000000074505806; // Vanilla 0.05f

pub const FLAG_PUSHABLE: u8 = 1;
pub const FLAG_PASSENGER: u8 = 2;
pub const FLAG_VEHICLE: u8 = 4;
pub const FLAG_NO_PHYSICS: u8 = 8;
pub const FLAG_SPECTATOR: u8 = 16;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum EntityKind {
    Player,
    Sheep,
    Cow,
    Pig,
    Chicken,
    Villager,
    Enderman,
    Endermite,
    Silverfish,
    Zombie,
    Piglin,
    SpawnerMob,
    PointOfInterest,
    Other,
}

impl EntityKind {
    fn is_living(self) -> bool {
        !matches!(self, Self::PointOfInterest | Self::Other)
    }

    fn can_idle_sleep(self) -> bool {
        matches!(
            self,
            Self::Sheep | Self::Cow | Self::Pig | Self::Chicken | Self::Villager
        )
    }
}

/// Each entity is represented exclusively by one slot in six contiguous arrays.
#[derive(Clone, Debug, Default)]
pub struct SoaEntityPool {
    pub ids: Vec<u32>,
    pub pos: Vec<[f64; 3]>,
    pub vel: Vec<[f64; 3]>,
    /// Horizontal and vertical half-extents, both centered on `pos`.
    pub half_extents: Vec<[f32; 2]>,
    pub kinds: Vec<EntityKind>,
    pub flags: Vec<u8>,
}

impl SoaEntityPool {
    pub fn len(&self) -> usize {
        self.ids.len()
    }

    pub fn is_empty(&self) -> bool {
        self.ids.is_empty()
    }

    pub fn push(
        &mut self,
        id: u32,
        pos: [f64; 3],
        vel: [f64; 3],
        half_extents: [f32; 2],
        kind: EntityKind,
        flags: u8,
    ) {
        assert!(pos.iter().chain(vel.iter()).all(|v| v.is_finite()));
        assert!(half_extents.iter().all(|v| v.is_finite() && *v >= 0.0));
        self.ids.push(id);
        self.pos.push(pos);
        self.vel.push(vel);
        self.half_extents.push(half_extents);
        self.kinds.push(kind);
        self.flags.push(flags);
    }

    fn check_shape(&self) {
        let len = self.len();
        assert!(
            self.pos.len() == len
                && self.vel.len() == len
                && self.half_extents.len() == len
                && self.kinds.len() == len
                && self.flags.len() == len
        );
    }
}

type Cell = (i32, i32, i32);

fn cell_coord(value: f64) -> i32 {
    (value / CELL_SIZE).floor() as i32
}

fn cell_of(pos: [f64; 3]) -> Cell {
    (cell_coord(pos[0]), cell_coord(pos[1]), cell_coord(pos[2]))
}

fn entity_bounds(pool: &SoaEntityPool, index: usize) -> ([f64; 3], [f64; 3]) {
    let pos = pool.pos[index];
    let [horizontal, vertical] = pool.half_extents[index].map(f64::from);
    (
        [pos[0] - horizontal, pos[1] - vertical, pos[2] - horizontal],
        [pos[0] + horizontal, pos[1] + vertical, pos[2] + horizontal],
    )
}

fn overlaps(a: ([f64; 3], [f64; 3]), b: ([f64; 3], [f64; 3])) -> bool {
    (0..3).all(|axis| a.0[axis] < b.1[axis] && a.1[axis] > b.0[axis])
}

fn cell_range(bounds: ([f64; 3], [f64; 3])) -> Option<(Cell, Cell)> {
    let lo = cell_of(bounds.0);
    let hi = cell_of(bounds.1);
    let spans = [
        i64::from(hi.0) - i64::from(lo.0) + 1,
        i64::from(hi.1) - i64::from(lo.1) + 1,
        i64::from(hi.2) - i64::from(lo.2) + 1,
    ];
    if spans.iter().any(|&n| n <= 0 || n > MAX_INDEX_CELLS)
        || spans.into_iter().product::<i64>() > MAX_INDEX_CELLS
    {
        None
    } else {
        Some((lo, hi))
    }
}

fn visit_cells((lo, hi): (Cell, Cell), mut visit: impl FnMut(Cell)) {
    for x in lo.0..=hi.0 {
        for y in lo.1..=hi.1 {
            for z in lo.2..=hi.2 {
                visit((x, y, z));
            }
        }
    }
}

/// Deltas are *impulses* to be applied by the owner, never position writes.
/// Cramming flags mean the 6.0f damage attempt is eligible; the owning runtime
/// must still apply its invulnerability window and damage rules.
#[derive(Clone, Debug, PartialEq)]
pub struct CollisionResult {
    pub push_deltas: Vec<[f64; 3]>,
    pub cramming_damage: Vec<bool>,
    pub candidate_counts: Vec<usize>,
}

/// Broad-phase index + exact Vanilla 1.21.1 `Entity.push` arithmetic/order.
/// `cramming_rolls[i]` is the caller's corresponding `nextInt(4)` draw; no
/// random source is advanced internally. Candidate order follows pool order.
pub fn solve_collisions_spatial(
    pool: &SoaEntityPool,
    max_entity_cramming: usize,
    cramming_rolls: &[u8],
) -> CollisionResult {
    pool.check_shape();
    let len = pool.len();
    assert_eq!(cramming_rolls.len(), len);
    let bounds: Vec<_> = (0..len).map(|index| entity_bounds(pool, index)).collect();
    let mut cells: HashMap<Cell, Vec<usize>> = HashMap::with_capacity(len);
    let mut large = Vec::new();
    for (index, &aabb) in bounds.iter().enumerate() {
        if let Some(range) = cell_range(aabb) {
            visit_cells(range, |cell| cells.entry(cell).or_default().push(index));
        } else {
            large.push(index);
        }
    }

    let mut result = CollisionResult {
        push_deltas: vec![[0.0; 3]; len],
        cramming_damage: vec![false; len],
        candidate_counts: vec![0; len],
    };
    // Reuse the candidate buffer across every entity within this solve.
    let mut candidates = Vec::with_capacity(len);
    for i in 0..len {
        if !pool.kinds[i].is_living() || pool.flags[i] & FLAG_SPECTATOR != 0 {
            continue;
        }
        candidates.clear();
        if let Some(range) = cell_range(bounds[i]) {
            visit_cells(range, |cell| {
                if let Some(entries) = cells.get(&cell) {
                    candidates.extend_from_slice(entries);
                }
            });
            candidates.extend_from_slice(&large);
            candidates.sort_unstable();
            candidates.dedup();
        } else {
            candidates.extend(0..len);
        }

        let mut nonpassengers = 0;
        for &j in &candidates {
            if i == j
                || pool.flags[j] & (FLAG_PUSHABLE | FLAG_SPECTATOR) != FLAG_PUSHABLE
                || !overlaps(bounds[i], bounds[j])
            {
                continue;
            }
            result.candidate_counts[i] += 1;
            if pool.flags[j] & FLAG_PASSENGER == 0 {
                nonpassengers += 1;
            }
            if pool.flags[i] & FLAG_NO_PHYSICS != 0 || pool.flags[j] & FLAG_NO_PHYSICS != 0 {
                continue;
            }

            // `LivingEntity.doPush(candidate)` invokes `candidate.push(self)`.
            let dx = pool.pos[i][0] - pool.pos[j][0];
            let dz = pool.pos[i][2] - pool.pos[j][2];
            let d = dx.abs().max(dz.abs());
            if d < MIN_PUSH_SEPARATION {
                continue;
            }
            let sqrt_d = d.sqrt();
            let scale = (1.0 / sqrt_d).min(1.0);
            let push_x = ((dx / sqrt_d) * scale) * PUSH_STRENGTH;
            let push_z = ((dz / sqrt_d) * scale) * PUSH_STRENGTH;
            if pool.flags[j] & FLAG_VEHICLE == 0 {
                result.push_deltas[j][0] += -push_x;
                result.push_deltas[j][2] += -push_z;
            }
            if pool.flags[i] & (FLAG_PUSHABLE | FLAG_VEHICLE) == FLAG_PUSHABLE {
                result.push_deltas[i][0] += push_x;
                result.push_deltas[i][2] += push_z;
            }
        }
        result.cramming_damage[i] = max_entity_cramming > 0
            && result.candidate_counts[i] >= max_entity_cramming
            && nonpassengers >= max_entity_cramming
            && cramming_rolls[i].is_multiple_of(4);
    }
    result
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DroppedItem {
    pub id: u32,
    pub pos: [f64; 3],
    pub vel: [f64; 3],
    pub item_id: u32,
    pub component_hash: u64,
    pub count: u32,
    pub max_stack_size: u32,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct XpOrb {
    pub id: u32,
    pub pos: [f64; 3],
    pub vel: [f64; 3],
    pub value: u64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MergeReport {
    pub eliminated: usize,
    pub transferred: u128,
}

fn nearby(a: [f64; 3], b: [f64; 3], radius: f64) -> bool {
    (0..3).map(|axis| (a[axis] - b[axis]).powi(2)).sum::<f64>() <= radius * radius
}

fn same_velocity(a: [f64; 3], b: [f64; 3]) -> bool {
    (0..3).all(|axis| a[axis].to_bits() == b[axis].to_bits())
}

/// Deterministic in-place consolidation. Zero-count stacks are tombstones.
/// Existing positions and velocities remain untouched, so water/chute movement
/// continues from the surviving physical stack. Divergent flows never coalesce.
pub fn merge_item_stacks(items: &mut [DroppedItem]) -> MergeReport {
    let mut cells: HashMap<Cell, Vec<usize>> = HashMap::with_capacity(items.len());
    let mut report = MergeReport::default();
    let mut candidates = Vec::with_capacity(items.len());
    for i in 0..items.len() {
        if items[i].count == 0 {
            continue;
        }
        let center = cell_of(items[i].pos);
        candidates.clear();
        for x in center.0.saturating_sub(1)..=center.0.saturating_add(1) {
            for y in center.1.saturating_sub(1)..=center.1.saturating_add(1) {
                for z in center.2.saturating_sub(1)..=center.2.saturating_add(1) {
                    if let Some(keepers) = cells.get(&(x, y, z)) {
                        candidates.extend_from_slice(keepers);
                    }
                }
            }
        }
        candidates.sort_unstable();
        for &j in &candidates {
            if items[j].item_id != items[i].item_id
                || items[j].component_hash != items[i].component_hash
                || !nearby(items[i].pos, items[j].pos, 0.5)
                || !same_velocity(items[i].vel, items[j].vel)
            {
                continue;
            }
            let max = items[j].max_stack_size.min(items[i].max_stack_size);
            let transfer = items[i].count.min(max.saturating_sub(items[j].count));
            items[j].count += transfer;
            items[i].count -= transfer;
            report.transferred += u128::from(transfer);
            if items[i].count == 0 {
                report.eliminated += 1;
                break;
            }
        }
        if items[i].count != 0 {
            cells.entry(center).or_default().push(i);
        }
    }
    report
}

/// XP values are accumulated with checked arithmetic. A zero-value orb is a
/// tombstone; incompatible velocities remain independent flow participants.
pub fn merge_xp_orbs(orbs: &mut [XpOrb]) -> MergeReport {
    let mut cells: HashMap<Cell, Vec<usize>> = HashMap::with_capacity(orbs.len());
    let mut report = MergeReport::default();
    let mut candidates = Vec::with_capacity(orbs.len());
    for i in 0..orbs.len() {
        if orbs[i].value == 0 {
            continue;
        }
        let center = cell_of(orbs[i].pos);
        candidates.clear();
        let mut merged = false;
        for x in center.0.saturating_sub(1)..=center.0.saturating_add(1) {
            for y in center.1.saturating_sub(1)..=center.1.saturating_add(1) {
                for z in center.2.saturating_sub(1)..=center.2.saturating_add(1) {
                    if let Some(keepers) = cells.get(&(x, y, z)) {
                        candidates.extend_from_slice(keepers);
                    }
                }
            }
        }
        candidates.sort_unstable();
        for &j in &candidates {
            if nearby(orbs[i].pos, orbs[j].pos, 1.0) && same_velocity(orbs[i].vel, orbs[j].vel) {
                if let Some(sum) = orbs[j].value.checked_add(orbs[i].value) {
                    orbs[j].value = sum;
                    report.transferred += u128::from(orbs[i].value);
                    orbs[i].value = 0;
                    report.eliminated += 1;
                    merged = true;
                    break;
                }
            }
        }
        if !merged {
            cells.entry(center).or_default().push(i);
        }
    }
    report
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct EntityAiState {
    pub kind: EntityKind,
    pub head_yaw: f32,
    pub head_pitch: f32,
    pub has_wool: bool,
    pub grass_eating_ticks: u32,
    pub egg_timer: u32,
    pub eggs_laid: u32,
    /// Negative ages grow toward zero at the caller's simulation tick rate.
    pub baby_age: i32,
    pub pathfinding_calls: u64,
}

impl EntityAiState {
    pub fn new(kind: EntityKind) -> Self {
        Self {
            kind,
            head_yaw: 0.0,
            head_pitch: 0.0,
            has_wool: true,
            grass_eating_ticks: 0,
            egg_timer: 6000,
            eggs_laid: 0,
            baby_age: 0,
            pathfinding_calls: 0,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct AiTickInput {
    pub pos: [f64; 3],
    pub nearest_player: Option<[f64; 3]>,
    pub enclosed: bool,
    /// Caller has confirmed that the baseline navigation would not move.
    pub navigation_idle: bool,
    pub wall_removed: bool,
    pub damaged: bool,
    pub breeding_food_nearby: bool,
    pub predator_or_zombie_nearby: bool,
    pub lured: bool,
    pub falling: bool,
    pub attacking: bool,
    pub infested_block_alarm: bool,
    pub spawner_drop_chute: bool,
    pub grass_eaten: bool,
    /// Deterministic value supplied by the caller's normal egg-timer RNG.
    pub next_egg_interval: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AiTickResult {
    pub pathfinding_ran: bool,
    pub woke: bool,
    pub egg_laid: bool,
    pub wool_regrown: bool,
}

/// Only the expensive pathfinding phase is suppressible. Visual orientation,
/// lifecycle and event timers execute in both paths on every simulation tick.
pub fn tick_confined_ai(
    state: &mut EntityAiState,
    input: &AiTickInput,
    optimization_enabled: bool,
) -> AiTickResult {
    if let Some(player) = input.nearest_player {
        let dx = player[0] - input.pos[0];
        let dy = player[1] - input.pos[1];
        let dz = player[2] - input.pos[2];
        if dx != 0.0 || dy != 0.0 || dz != 0.0 {
            state.head_yaw = (dz.atan2(dx).to_degrees() - 90.0) as f32;
            state.head_pitch = (-dy.atan2(dx.hypot(dz)).to_degrees()) as f32;
        }
    }

    if state.grass_eating_ticks > 0 {
        state.grass_eating_ticks -= 1;
    }
    let wool_regrown = state.kind == EntityKind::Sheep && input.grass_eaten && !state.has_wool;
    if wool_regrown {
        state.has_wool = true;
    }
    if state.baby_age < 0 {
        state.baby_age += 1;
    }
    let mut egg_laid = false;
    if state.kind == EntityKind::Chicken {
        state.egg_timer = state.egg_timer.saturating_sub(1);
        if state.egg_timer == 0 {
            state.eggs_laid = state.eggs_laid.saturating_add(1);
            state.egg_timer = input.next_egg_interval.max(1);
            egg_laid = true;
        }
    }

    let must_wake = input.wall_removed
        || input.damaged
        || input.breeding_food_nearby
        || input.predator_or_zombie_nearby
        || input.lured
        || input.falling
        || input.attacking
        || input.infested_block_alarm
        || input.spawner_drop_chute;
    let can_sleep = optimization_enabled
        && state.kind.can_idle_sleep()
        && input.enclosed
        && input.navigation_idle
        && !must_wake;
    if !can_sleep {
        state.pathfinding_calls = state.pathfinding_calls.saturating_add(1);
    }
    AiTickResult {
        pathfinding_ran: !can_sleep,
        woke: must_wake,
        egg_laid,
        wool_regrown,
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SensorEntity {
    pub id: u32,
    pub kind: EntityKind,
    pub dimension: i32,
    pub pos: [f64; 3],
    pub invisible: bool,
    /// Visibility multiplier provided by the host's exact armor/visibility
    /// calculation; 0.07 is the unarmored invisible-player default.
    pub invisible_range_multiplier: f64,
    pub gold_armor: bool,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MobSensor {
    pub id: u32,
    pub kind: EntityKind,
    pub dimension: i32,
    pub pos: [f64; 3],
    pub follow_range: f64,
}

type Chunk = (i32, i32, i32);

fn chunk_coord(value: f64) -> i32 {
    (value / 16.0).floor() as i32
}

fn target_eligible(mob: &MobSensor, target: &SensorEntity) -> bool {
    if mob.id == target.id || mob.dimension != target.dimension {
        return false;
    }
    match mob.kind {
        EntityKind::Enderman => target.kind == EntityKind::Endermite,
        EntityKind::Zombie => matches!(target.kind, EntityKind::Player | EntityKind::Villager),
        EntityKind::Piglin => target.kind == EntityKind::Player && !target.gold_armor,
        _ => false,
    }
}

fn evaluate_candidates(
    mob: &MobSensor,
    targets: &[SensorEntity],
    indices: impl IntoIterator<Item = usize>,
    mut has_line_of_sight: impl FnMut(&MobSensor, &SensorEntity) -> bool,
) -> Option<u32> {
    let mut best: Option<(f64, u32)> = None;
    for i in indices {
        let target = &targets[i];
        if !target_eligible(mob, target) {
            continue;
        }
        let range = if target.kind == EntityKind::Player && target.invisible {
            mob.follow_range * target.invisible_range_multiplier
        } else {
            mob.follow_range
        };
        if !range.is_finite() || range < 0.0 {
            continue;
        }
        let squared = (0..3)
            .map(|axis| (target.pos[axis] - mob.pos[axis]).powi(2))
            .sum::<f64>();
        if squared > range * range || !has_line_of_sight(mob, target) {
            continue;
        }
        if best.is_none_or(|(previous, _)| squared < previous) {
            best = Some((squared, target.id));
        }
    }
    best.map(|(_, id)| id)
}

/// Direct reference path, scanning every entity in stable input order.
pub fn evaluate_mob_targets_uncached(
    mob: &MobSensor,
    targets: &[SensorEntity],
    has_line_of_sight: impl FnMut(&MobSensor, &SensorEntity) -> bool,
) -> Option<u32> {
    evaluate_candidates(mob, targets, 0..targets.len(), has_line_of_sight)
}

/// Rebuilt once for a world tick and shared by all mob queries in its chunks.
/// `targets` can also carry points of interest for other sensor consumers.
#[derive(Clone, Debug)]
pub struct ChunkSensorBroadPhase {
    pub tick: u64,
    targets: Vec<SensorEntity>,
    chunks: HashMap<Chunk, Vec<usize>>,
    max_detection_multiplier: f64,
}

impl ChunkSensorBroadPhase {
    pub fn build(tick: u64, targets: &[SensorEntity]) -> Self {
        let mut chunks: HashMap<Chunk, Vec<usize>> = HashMap::new();
        let mut max_detection_multiplier = 1.0f64;
        for (index, entity) in targets.iter().enumerate() {
            if entity.kind == EntityKind::Player && entity.invisible {
                max_detection_multiplier =
                    max_detection_multiplier.max(entity.invisible_range_multiplier);
            }
            chunks
                .entry((
                    entity.dimension,
                    chunk_coord(entity.pos[0]),
                    chunk_coord(entity.pos[2]),
                ))
                .or_default()
                .push(index);
        }
        Self {
            tick,
            targets: targets.to_vec(),
            chunks,
            max_detection_multiplier,
        }
    }

    pub fn chunk_population(&self, dimension: i32, cx: i32, cz: i32) -> usize {
        self.chunks.get(&(dimension, cx, cz)).map_or(0, Vec::len)
    }

    /// Narrow-phase always runs distance, invisibility, LOS and kind rules per mob.
    pub fn evaluate_mob_targets(
        &self,
        mob: &MobSensor,
        has_line_of_sight: impl FnMut(&MobSensor, &SensorEntity) -> bool,
    ) -> Option<u32> {
        assert!(mob.follow_range.is_finite() && mob.follow_range >= 0.0);
        // A host-supplied invisibility multiplier may enlarge the default
        // range. The broad phase must be a superset of the full-scan results.
        let search_range = mob.follow_range * self.max_detection_multiplier;
        let min_x = chunk_coord(mob.pos[0] - search_range);
        let max_x = chunk_coord(mob.pos[0] + search_range);
        let min_z = chunk_coord(mob.pos[2] - search_range);
        let max_z = chunk_coord(mob.pos[2] + search_range);
        let span_x = i64::from(max_x) - i64::from(min_x) + 1;
        let span_z = i64::from(max_z) - i64::from(min_z) + 1;
        // For extreme follow ranges a full scan is cheaper than visiting every
        // empty chunk, and still preserves the exact same candidate set.
        if span_x > 256 || span_z > 256 || span_x * span_z > 256 {
            return evaluate_mob_targets_uncached(mob, &self.targets, has_line_of_sight);
        }
        let mut indices = Vec::new();
        for cx in min_x..=max_x {
            for cz in min_z..=max_z {
                if let Some(entries) = self.chunks.get(&(mob.dimension, cx, cz)) {
                    indices.extend_from_slice(entries);
                }
            }
        }
        indices.sort_unstable();
        evaluate_candidates(mob, &self.targets, indices, has_line_of_sight)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn push(
        pool: &mut SoaEntityPool,
        id: u32,
        pos: [f64; 3],
        kind: EntityKind,
        flags: u8,
    ) {
        pool.push(id, pos, [0.0; 3], [0.3, 0.9], kind, flags);
    }

    #[test]
    fn pool_shape_and_empty_collision_solve() {
        let mut pool = SoaEntityPool::default();
        assert!(pool.is_empty());
        push(&mut pool, 1, [0.0, 0.0, 0.0], EntityKind::Sheep, FLAG_PUSHABLE);
        assert_eq!(pool.len(), 1);
        assert!(!pool.is_empty());
        let empty = SoaEntityPool::default();
        let result = solve_collisions_spatial(&empty, 0, &[]);
        assert!(result.push_deltas.is_empty());
        assert!(result.candidate_counts.is_empty());
        // A lone entity has no candidates and no motion.
        let result = solve_collisions_spatial(&pool, 0, &[0]);
        assert_eq!(result.candidate_counts, vec![0]);
        assert_eq!(result.push_deltas[0], [0.0; 3]);
    }

    #[test]
    fn collisions_push_overlapping_entities_apart_symmetrically() {
        let mut pool = SoaEntityPool::default();
        push(&mut pool, 1, [0.0, 0.0, 0.0], EntityKind::Sheep, FLAG_PUSHABLE);
        push(&mut pool, 2, [0.5, 0.0, 0.0], EntityKind::Sheep, FLAG_PUSHABLE);
        let result = solve_collisions_spatial(&pool, 0, &[0, 0]);
        assert_eq!(result.candidate_counts, vec![1, 1]);
        assert!(result.push_deltas[0][0] < 0.0);
        assert!(result.push_deltas[1][0] > 0.0);
        assert!(
            (result.push_deltas[0][0] + result.push_deltas[1][0]).abs() < 1e-12,
            "push impulses conserve momentum"
        );
        assert_eq!(result.push_deltas[0][1], 0.0);
        assert!(!result.cramming_damage.iter().any(|d| *d));
    }

    #[test]
    fn coincident_entities_count_but_never_push() {
        let mut pool = SoaEntityPool::default();
        push(&mut pool, 1, [0.0, 0.0, 0.0], EntityKind::Sheep, FLAG_PUSHABLE);
        push(&mut pool, 2, [0.0, 0.0, 0.0], EntityKind::Cow, FLAG_PUSHABLE);
        let result = solve_collisions_spatial(&pool, 0, &[0, 0]);
        assert_eq!(result.candidate_counts, vec![1, 1]);
        assert_eq!(result.push_deltas[0], [0.0; 3]);
        assert_eq!(result.push_deltas[1], [0.0; 3]);
    }

    #[test]
    fn flags_gate_candidacy_physics_and_cramming() {
        // Spectator and non-living movers are inert; a spectator target is
        // invisible to the broad phase entirely.
        let mut pool = SoaEntityPool::default();
        push(&mut pool, 1, [0.0, 0.0, 0.0], EntityKind::Sheep, FLAG_PUSHABLE);
        push(
            &mut pool,
            2,
            [0.5, 0.0, 0.0],
            EntityKind::Cow,
            FLAG_PUSHABLE | FLAG_SPECTATOR,
        );
        push(
            &mut pool,
            3,
            [0.5, 0.0, 0.5],
            EntityKind::PointOfInterest,
            FLAG_PUSHABLE,
        );
        push(
            &mut pool,
            4,
            [1.0, 0.0, 0.0],
            EntityKind::Zombie,
            FLAG_PUSHABLE | FLAG_SPECTATOR,
        );
        let result = solve_collisions_spatial(&pool, 0, &[0; 4]);
        // Entity 1 sees only the point of interest; entity 2 is a spectator
        // mover; entity 3 is not living; entity 4 is a spectator.
        assert_eq!(result.candidate_counts, vec![1, 0, 0, 0]);
        assert!(result.push_deltas[1].iter().all(|v| *v == 0.0));
        assert!(result.push_deltas[3].iter().all(|v| *v == 0.0));

        // NO_PHYSICS still counts as a candidate but never exchanges motion.
        let mut pool = SoaEntityPool::default();
        push(&mut pool, 1, [0.0, 0.0, 0.0], EntityKind::Sheep, FLAG_PUSHABLE);
        push(
            &mut pool,
            2,
            [0.5, 0.0, 0.0],
            EntityKind::Cow,
            FLAG_PUSHABLE | FLAG_NO_PHYSICS,
        );
        let result = solve_collisions_spatial(&pool, 0, &[0, 0]);
        assert_eq!(result.candidate_counts, vec![1, 1]);
        assert_eq!(result.push_deltas[0], [0.0; 3]);
        assert_eq!(result.push_deltas[1], [0.0; 3]);

        // A vehicle target is never displaced, but the pusher still moves.
        let mut pool = SoaEntityPool::default();
        push(&mut pool, 1, [0.0, 0.0, 0.0], EntityKind::Sheep, FLAG_PUSHABLE);
        push(
            &mut pool,
            2,
            [0.5, 0.0, 0.0],
            EntityKind::Cow,
            FLAG_PUSHABLE | FLAG_VEHICLE,
        );
        let result = solve_collisions_spatial(&pool, 0, &[0, 0]);
        assert!(result.push_deltas[1].iter().all(|v| *v == 0.0));
        assert!(result.push_deltas[0][0] < 0.0);
        assert_eq!(result.push_deltas[0][1], 0.0);
    }

    #[test]
    fn cramming_requires_count_nonpassengers_and_a_multiple_of_four_roll() {
        let mut pool = SoaEntityPool::default();
        for id in 1..=3 {
            push(&mut pool, id, [0.0, 0.0, 0.0], EntityKind::Sheep, FLAG_PUSHABLE);
        }
        // Every entity sees two candidates; threshold 2 is met everywhere.
        let result = solve_collisions_spatial(&pool, 2, &[0, 4, 8]);
        assert_eq!(result.candidate_counts, vec![2, 2, 2]);
        assert_eq!(result.cramming_damage, vec![true, true, true]);
        // Rolls not divisible by 4 suppress damage per entity.
        let result = solve_collisions_spatial(&pool, 2, &[1, 4, 8]);
        assert_eq!(result.cramming_damage, vec![false, true, true]);
        // Threshold 0 disables cramming entirely.
        let result = solve_collisions_spatial(&pool, 0, &[0, 0, 0]);
        assert!(!result.cramming_damage.iter().any(|d| *d));

        // A passenger neighbour does not count toward nonpassenger cramming,
        // though the passenger entity itself still suffers from real mobs.
        pool.flags[2] |= FLAG_PASSENGER;
        let result = solve_collisions_spatial(&pool, 2, &[0, 0, 0]);
        assert_eq!(result.cramming_damage, vec![false, false, true]);
    }

    #[test]
    fn oversized_bounds_fall_back_to_full_scan() {
        let mut pool = SoaEntityPool::default();
        pool.push(
            1,
            [0.0, 0.0, 0.0],
            [0.0; 3],
            [200.0, 200.0],
            EntityKind::Sheep,
            FLAG_PUSHABLE,
        );
        push(&mut pool, 2, [100.0, 0.0, 0.0], EntityKind::Cow, FLAG_PUSHABLE);
        let result = solve_collisions_spatial(&pool, 0, &[0, 0]);
        assert_eq!(result.candidate_counts, vec![1, 1]);
    }

    #[test]
    fn item_stack_merging_respects_identity_velocity_and_stack_cap() {
        let item = |id: u32, pos: [f64; 3], vel: [f64; 3], count: u32| DroppedItem {
            id,
            pos,
            vel,
            item_id: 7,
            component_hash: 42,
            count,
            max_stack_size: 64,
        };
        let mut items = vec![
            item(1, [0.0, 0.0, 0.0], [0.0; 3], 60),
            item(2, [0.4, 0.0, 0.0], [0.0; 3], 10),
            item(3, [0.0, 0.0, 0.6], [0.0; 3], 4),
            item(4, [1.9, 0.0, 0.0], [0.0; 3], 5),
            item(5, [2.1, 0.0, 0.0], [1.0, 0.0, 0.0], 5),
        ];
        // Items 4 and 5 sit in adjacent spatial cells but differ in velocity;
        // item 3 is beyond the 0.5 merge radius.
        let report = merge_item_stacks(&mut items);
        // Item 2 fills stack 1 to 64 and keeps 6; item 4 merges into... nothing
        // identical is near, and item 5's velocity differs.
        assert_eq!(items[0].count, 64);
        assert_eq!(items[1].count, 6);
        assert_eq!(items[3].count, 5);
        assert_eq!(items[4].count, 5);
        assert_eq!(report.transferred, 4);
        assert_eq!(report.eliminated, 0);

        // A full drain eliminates the donor and leaves a tombstone.
        let mut items = vec![
            item(1, [0.0, 0.0, 0.0], [0.0; 3], 60),
            item(2, [0.0, 0.0, 0.0], [0.0; 3], 4),
        ];
        let report = merge_item_stacks(&mut items);
        assert_eq!(report, MergeReport {
            eliminated: 1,
            transferred: 4,
        });
        assert_eq!(items[1].count, 0);

        // Tombstones neither merge nor receive.
        let mut items = vec![
            item(1, [0.0, 0.0, 0.0], [0.0; 3], 0),
            item(2, [0.0, 0.0, 0.0], [0.0; 3], 7),
        ];
        let report = merge_item_stacks(&mut items);
        assert_eq!(report, MergeReport::default());
        assert_eq!(items[1].count, 7);

        // Different item identity or component hash never coalesces.
        let mut items = vec![
            item(1, [0.0, 0.0, 0.0], [0.0; 3], 3),
            DroppedItem {
                item_id: 8,
                ..item(2, [0.0, 0.0, 0.0], [0.0; 3], 3)
            },
            DroppedItem {
                component_hash: 43,
                ..item(3, [0.0, 0.0, 0.0], [0.0; 3], 3)
            },
        ];
        assert_eq!(merge_item_stacks(&mut items), MergeReport::default());
    }

    #[test]
    fn xp_orbs_merge_with_checked_arithmetic() {
        let orb = |id: u32, pos: [f64; 3], vel: [f64; 3], value: u64| XpOrb {
            id,
            pos,
            vel,
            value,
        };
        let mut orbs = vec![
            orb(1, [0.0, 0.0, 0.0], [0.0; 3], 1),
            orb(2, [0.5, 0.0, 0.0], [0.0; 3], 2),
            orb(3, [1.5, 0.0, 0.0], [0.0; 3], 4), // outside the 1.0 radius
        ];
        let report = merge_xp_orbs(&mut orbs);
        assert_eq!(report, MergeReport {
            eliminated: 1,
            transferred: 2,
        });
        assert_eq!(orbs[0].value, 3);
        assert_eq!(orbs[1].value, 0);
        assert_eq!(orbs[2].value, 4);

        // u64 overflow refuses the merge instead of wrapping.
        let mut orbs = vec![
            orb(1, [0.0, 0.0, 0.0], [0.0; 3], u64::MAX),
            orb(2, [0.0, 0.0, 0.0], [0.0; 3], 1),
        ];
        assert_eq!(merge_xp_orbs(&mut orbs), MergeReport::default());
        assert_eq!(orbs[0].value, u64::MAX);
        assert_eq!(orbs[1].value, 1);

        // Divergent velocities never coalesce.
        let mut orbs = vec![
            orb(1, [0.0, 0.0, 0.0], [0.0; 3], 5),
            orb(2, [0.0, 0.0, 0.0], [0.0, 1.0, 0.0], 5),
        ];
        assert_eq!(merge_xp_orbs(&mut orbs), MergeReport::default());
    }

    #[test]
    fn confined_ai_sleep_wake_timers_and_orientation() {
        let input = AiTickInput {
            pos: [0.0, 0.0, 0.0],
            nearest_player: Some([1.0, 0.0, 0.0]),
            enclosed: true,
            navigation_idle: true,
            ..AiTickInput::default()
        };
        let mut sheep = EntityAiState::new(EntityKind::Sheep);
        let result = tick_confined_ai(&mut sheep, &input, true);
        assert!(!result.pathfinding_ran);
        assert!(!result.woke);
        assert_eq!(sheep.pathfinding_calls, 0);
        // Player due east of the mob: yaw faces -90 degrees (north convention).
        assert!((f64::from(sheep.head_yaw) + 90.0).abs() < 1e-5);

        // Any wake stimulus restores pathfinding and reports the wake; an
        // open pen only suppresses sleep, it is not itself a wake event.
        for (wake, expected_woke) in [
            (
                AiTickInput {
                    damaged: true,
                    ..input
                },
                true,
            ),
            (
                AiTickInput {
                    predator_or_zombie_nearby: true,
                    ..input
                },
                true,
            ),
            (
                AiTickInput {
                    enclosed: false,
                    ..input
                },
                false,
            ),
        ] {
            let mut state = EntityAiState::new(EntityKind::Sheep);
            let result = tick_confined_ai(&mut state, &wake, true);
            assert!(result.pathfinding_ran);
            assert_eq!(result.woke, expected_woke);
        }

        // Non-idle-sleeping kinds never sleep even when enclosed and idle.
        let mut zombie = EntityAiState::new(EntityKind::Zombie);
        let result = tick_confined_ai(&mut zombie, &input, true);
        assert!(result.pathfinding_ran);
        assert_eq!(zombie.pathfinding_calls, 1);

        // Optimization disabled forces pathfinding for every kind.
        let mut pig = EntityAiState::new(EntityKind::Pig);
        let result = tick_confined_ai(&mut pig, &input, false);
        assert!(result.pathfinding_ran);
        assert_eq!(pig.pathfinding_calls, 1);
    }

    #[test]
    fn confined_ai_timers_and_lifecycle() {
        let mut sheep = EntityAiState::new(EntityKind::Sheep);
        sheep.has_wool = false;
        sheep.grass_eating_ticks = 3;
        sheep.baby_age = -5;
        let result = tick_confined_ai(
            &mut sheep,
            &AiTickInput {
                grass_eaten: true,
                ..AiTickInput::default()
            },
            false,
        );
        assert!(result.wool_regrown);
        assert!(sheep.has_wool);
        assert_eq!(sheep.grass_eating_ticks, 2);
        assert_eq!(sheep.baby_age, -4);

        // Grass cannot regrow wool on non-sheep or while already woolly.
        let mut cow = EntityAiState::new(EntityKind::Cow);
        let result = tick_confined_ai(
            &mut cow,
            &AiTickInput {
                grass_eaten: true,
                ..AiTickInput::default()
            },
            false,
        );
        assert!(!result.wool_regrown);

        let mut chicken = EntityAiState::new(EntityKind::Chicken);
        chicken.egg_timer = 1;
        let result = tick_confined_ai(
            &mut chicken,
            &AiTickInput {
                next_egg_interval: 0,
                ..AiTickInput::default()
            },
            false,
        );
        assert!(result.egg_laid);
        assert_eq!(chicken.eggs_laid, 1);
        assert_eq!(chicken.egg_timer, 1, "interval clamps to at least 1");
    }

    fn sensor(id: u32, kind: EntityKind, dimension: i32, pos: [f64; 3]) -> SensorEntity {
        SensorEntity {
            id,
            kind,
            dimension,
            pos,
            invisible: false,
            invisible_range_multiplier: 0.07,
            gold_armor: false,
        }
    }

    fn mob(id: u32, kind: EntityKind, pos: [f64; 3], follow_range: f64) -> MobSensor {
        MobSensor {
            id,
            kind,
            dimension: 0,
            pos,
            follow_range,
        }
    }

    fn los(_: &MobSensor, _: &SensorEntity) -> bool {
        true
    }

    fn no_los(_: &MobSensor, _: &SensorEntity) -> bool {
        false
    }

    #[test]
    fn mob_targeting_matches_reference_and_respects_kind_rules() {
        let targets = vec![
            sensor(1, EntityKind::Player, 0, [3.0, 0.0, 0.0]),
            sensor(2, EntityKind::Villager, 0, [6.0, 0.0, 0.0]),
            sensor(3, EntityKind::Player, 0, [30.0, 0.0, 0.0]), // out of range
            sensor(4, EntityKind::Zombie, 0, [1.0, 0.0, 0.0]),  // wrong kind
            sensor(5, EntityKind::Cow, 0, [2.0, 0.0, 0.0]),     // wrong kind
            sensor(6, EntityKind::Player, 1, [1.0, 0.0, 0.0]),  // wrong dimension
        ];
        let zombie = mob(10, EntityKind::Zombie, [0.0, 0.0, 0.0], 10.0);
        let broad = ChunkSensorBroadPhase::build(42, &targets);
        assert_eq!(evaluate_mob_targets_uncached(&zombie, &targets, los), Some(1));
        assert_eq!(broad.evaluate_mob_targets(&zombie, los), Some(1));

        // Nearest eligible target wins when several are in range.
        let targets = vec![
            sensor(7, EntityKind::Villager, 0, [8.0, 0.0, 0.0]),
            sensor(8, EntityKind::Player, 0, [4.0, 0.0, 0.0]),
        ];
        let broad = ChunkSensorBroadPhase::build(43, &targets);
        assert_eq!(
            evaluate_mob_targets_uncached(&zombie, &targets, los),
            Some(8)
        );
        assert_eq!(broad.evaluate_mob_targets(&zombie, los), Some(8));

        // Line of sight gates every candidate.
        assert_eq!(
            evaluate_mob_targets_uncached(&zombie, &targets, no_los),
            None
        );
        assert_eq!(broad.evaluate_mob_targets(&zombie, no_los), None);
    }

    #[test]
    fn mob_targeting_kind_specific_rules() {
        let enderman = mob(9, EntityKind::Enderman, [0.0, 0.0, 0.0], 64.0);
        let targets = vec![
            sensor(1, EntityKind::Endermite, 0, [5.0, 0.0, 0.0]),
            sensor(2, EntityKind::Player, 0, [1.0, 0.0, 0.0]),
        ];
        assert_eq!(
            evaluate_mob_targets_uncached(&enderman, &targets, los),
            Some(1)
        );

        let piglin = mob(2, EntityKind::Piglin, [0.0, 0.0, 0.0], 16.0);
        let mut gold = sensor(3, EntityKind::Player, 0, [2.0, 0.0, 0.0]);
        gold.gold_armor = true;
        let targets = vec![
            gold,
            sensor(4, EntityKind::Player, 0, [4.0, 0.0, 0.0]),
        ];
        assert_eq!(
            evaluate_mob_targets_uncached(&piglin, &targets, los),
            Some(4),
            "gold-armored players are ignored by piglins"
        );
        let all_gold = vec![SensorEntity {
            gold_armor: true,
            ..sensor(5, EntityKind::Player, 0, [2.0, 0.0, 0.0])
        }];
        assert_eq!(
            evaluate_mob_targets_uncached(&piglin, &all_gold, los),
            None
        );
    }

    #[test]
    fn invisibility_scales_range_and_broadphase_stays_a_superset() {
        let zombie = mob(10, EntityKind::Zombie, [0.0, 0.0, 0.0], 10.0);
        let mut invisible = sensor(1, EntityKind::Player, 0, [5.0, 0.0, 0.0]);
        invisible.invisible = true;
        // 10 * 0.07 = 0.7 effective range: distance 5 is out of range.
        assert_eq!(evaluate_mob_targets_uncached(&zombie, &[invisible], los), None);
        let broad = ChunkSensorBroadPhase::build(7, &[invisible]);
        assert_eq!(broad.evaluate_mob_targets(&zombie, los), None);

        // A multiplier above 1 widens both the narrow phase and the broad-phase
        // search window identically.
        invisible.invisible_range_multiplier = 2.0;
        invisible.pos = [15.0, 0.0, 0.0]; // distance 15 <= 10 * 2.0
        let targets = vec![invisible];
        let broad = ChunkSensorBroadPhase::build(8, &targets);
        assert_eq!(
            evaluate_mob_targets_uncached(&zombie, &targets, los),
            Some(1)
        );
        assert_eq!(broad.evaluate_mob_targets(&zombie, los), Some(1));
    }

    #[test]
    fn broadphase_matches_uncached_across_chunk_boundaries() {
        let targets = vec![
            sensor(1, EntityKind::Villager, 0, [-3.0, 0.0, 0.0]),  // chunk -1
            sensor(2, EntityKind::Player, 0, [20.0, 0.0, 0.0]),     // chunk 1
            sensor(3, EntityKind::Villager, 0, [15.9, 0.0, 0.0]),   // chunk 0 edge
            sensor(4, EntityKind::Player, 0, [-20.0, 0.0, 0.0]),    // chunk -2
        ];
        let broad = ChunkSensorBroadPhase::build(9, &targets);
        assert_eq!(broad.chunk_population(0, 0, 0), 1);
        assert_eq!(broad.chunk_population(0, -1, 0), 1);
        assert_eq!(broad.chunk_population(0, 1, 0), 1);
        assert_eq!(broad.chunk_population(1, 0, 0), 0);

        for pos in [
            [0.0, 0.0, 0.0],
            [15.5, 0.0, 0.0],
            [-15.5, 0.0, 0.0],
            [8.0, 0.0, 8.0],
        ] {
            for range in [4.0, 12.0, 1000.0] {
                let zombie = mob(10, EntityKind::Zombie, pos, range);
                let expected = evaluate_mob_targets_uncached(&zombie, &targets, los);
                assert_eq!(
                    broad.evaluate_mob_targets(&zombie, los),
                    expected,
                    "pos {pos:?} range {range}"
                );
            }
        }

        // A degenerate follow range never matches and never panics.
        let zombie = mob(10, EntityKind::Zombie, [0.0, 0.0, 0.0], f64::NAN);
        assert_eq!(
            evaluate_mob_targets_uncached(&zombie, &targets, los),
            None
        );
    }
}
