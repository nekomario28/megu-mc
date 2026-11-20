// Minecraft 1.21.1 NaturalSpawner primitives. Included directly in main.rs.
// The world adapter owns actual entities, block queries and spawned-chunk selection.
use crate::generation_math::Lcg;

#[cfg_attr(not(test), allow(dead_code))]
const SPAWN_CAP_MAGIC_NUMBER: u64 = 17 * 17;
#[cfg_attr(not(test), allow(dead_code))]
const SPAWN_MIN_PLAYER_DIST_SQ: f64 = 24.0 * 24.0;
#[cfg_attr(not(test), allow(dead_code))]
const SPAWN_MAX_PLAYER_DIST_SQ: f64 = 128.0 * 128.0;

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) enum MobCategory {
    Monster,
    Creature,
    Ambient,
    Axolotls,
    UndergroundWaterCreature,
    WaterCreature,
    WaterAmbient,
}

#[cfg_attr(not(test), allow(dead_code))]
impl MobCategory {
    const fn index(self) -> usize {
        match self {
            Self::Monster => 0,
            Self::Creature => 1,
            Self::Ambient => 2,
            Self::Axolotls => 3,
            Self::UndergroundWaterCreature => 4,
            Self::WaterCreature => 5,
            Self::WaterAmbient => 6,
        }
    }

    pub(crate) const fn max_instances_per_chunk(self) -> u64 {
        match self {
            Self::Monster => 70,
            Self::Creature => 10,
            Self::Ambient => 15,
            Self::Axolotls | Self::UndergroundWaterCreature | Self::WaterCreature => 5,
            Self::WaterAmbient => 20,
        }
    }

    pub(crate) const fn is_friendly(self) -> bool {
        !matches!(self, Self::Monster)
    }

    pub(crate) const fn is_persistent(self) -> bool {
        matches!(self, Self::Creature)
    }

    pub(crate) const fn despawn_distance(self) -> f64 {
        if matches!(self, Self::WaterAmbient) {
            64.0
        } else {
            128.0
        }
    }

    pub(crate) const fn no_despawn_distance(self) -> f64 {
        32.0
    }

    pub(crate) const fn global_cap(self, spawnable_chunk_count: u32) -> u64 {
        self.max_instances_per_chunk() * spawnable_chunk_count as u64 / SPAWN_CAP_MAGIC_NUMBER
    }
}

#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn can_spawn_for_category(
    category: MobCategory,
    count: u64,
    spawnable_chunk_count: u32,
) -> bool {
    count < category.global_cap(spawnable_chunk_count)
}

/// For each chunk, ChunkMap supplies nearby non-spectator player IDs.
/// A category may spawn when at least one of those players is below its cap.
#[cfg_attr(not(test), allow(dead_code))]
#[derive(Default, Debug)]
pub(crate) struct LocalMobCapCalculator {
    counts: std::collections::HashMap<u64, [u64; 7]>,
}

#[cfg_attr(not(test), allow(dead_code))]
impl LocalMobCapCalculator {
    pub(crate) fn can_spawn(&self, category: MobCategory, players: &[u64]) -> bool {
        players.iter().any(|id| {
            self.counts
                .get(id)
                .is_none_or(|counts| counts[category.index()] < category.max_instances_per_chunk())
        })
    }

    pub(crate) fn add_mob(&mut self, category: MobCategory, players: &[u64]) {
        let mut seen = std::collections::HashSet::new();
        for id in players {
            if seen.insert(*id) {
                self.counts.entry(*id).or_default()[category.index()] += 1;
            }
        }
    }

    pub(crate) fn count(&self, player: u64, category: MobCategory) -> u64 {
        self.counts
            .get(&player)
            .map_or(0, |counts| counts[category.index()])
    }
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct NaturalSpawnPos {
    pub x: i32,
    pub y: i32,
    pub z: i32,
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct PointCharge {
    pub x: f64,
    pub y: f64,
    pub z: f64,
    pub charge: f64,
}

#[cfg_attr(not(test), allow(dead_code))]
impl PointCharge {
    fn new(pos: NaturalSpawnPos, charge: f64) -> Self {
        Self {
            x: pos.x as f64,
            y: pos.y as f64,
            z: pos.z as f64,
            charge,
        }
    }
}

/// The registry already carries the vanilla biome spawn-cost codec.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) type MobSpawnCost = crate::feature::SpawnCost;

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Default, Debug)]
pub(crate) struct PotentialCalculator {
    charges: Vec<PointCharge>,
}

#[cfg_attr(not(test), allow(dead_code))]
impl PotentialCalculator {
    pub(crate) fn get_potential_energy_change(&self, pos: NaturalSpawnPos, mass: f64) -> f64 {
        if mass == 0.0 {
            return 0.0;
        }
        self.charges
            .iter()
            .map(|point| {
                let dx = pos.x as f64 - point.x;
                let dy = pos.y as f64 - point.y;
                let dz = pos.z as f64 - point.z;
                let distance = (dx * dx + dy * dy + dz * dz).sqrt();
                if distance == 0.0 {
                    f64::INFINITY
                } else {
                    point.charge / distance
                }
            })
            .sum::<f64>()
            * mass
    }

    pub(crate) fn can_spawn(&self, pos: NaturalSpawnPos, cost: MobSpawnCost) -> bool {
        self.get_potential_energy_change(pos, cost.charge) <= cost.energy_budget
    }

    pub(crate) fn add_charge(&mut self, pos: NaturalSpawnPos, charge: f64) {
        if charge != 0.0 {
            self.charges.push(PointCharge::new(pos, charge));
        }
    }
}

/// Per-tick global census, shared potential field and optional local cap.
#[cfg_attr(not(test), allow(dead_code))]
#[derive(Debug)]
pub(crate) struct NaturalSpawnState {
    spawnable_chunk_count: u32,
    counts: [u64; 7],
    potential: PotentialCalculator,
    local_caps: Option<LocalMobCapCalculator>,
}

#[cfg_attr(not(test), allow(dead_code))]
impl NaturalSpawnState {
    pub(crate) fn new(spawnable_chunk_count: u32, use_local_caps: bool) -> Self {
        Self {
            spawnable_chunk_count,
            counts: [0; 7],
            potential: PotentialCalculator::default(),
            local_caps: use_local_caps.then(LocalMobCapCalculator::default),
        }
    }

    /// Preexisting cap-eligible mobs contribute their category and biome cost.
    pub(crate) fn count_existing(
        &mut self,
        category: MobCategory,
        pos: NaturalSpawnPos,
        cost: Option<MobSpawnCost>,
        players: &[u64],
    ) {
        self.record(category, pos, cost, players);
    }

    pub(crate) fn can_spawn(
        &self,
        category: MobCategory,
        pos: NaturalSpawnPos,
        cost: Option<MobSpawnCost>,
        players: &[u64],
    ) -> bool {
        can_spawn_for_category(
            category,
            self.counts[category.index()],
            self.spawnable_chunk_count,
        ) && self
            .local_caps
            .as_ref()
            .is_none_or(|cap| cap.can_spawn(category, players))
            && cost.is_none_or(|cost| self.potential.can_spawn(pos, cost))
    }

    pub(crate) fn record_success(
        &mut self,
        category: MobCategory,
        pos: NaturalSpawnPos,
        cost: Option<MobSpawnCost>,
        players: &[u64],
    ) {
        self.record(category, pos, cost, players);
    }

    fn record(
        &mut self,
        category: MobCategory,
        pos: NaturalSpawnPos,
        cost: Option<MobSpawnCost>,
        players: &[u64],
    ) {
        self.counts[category.index()] += 1;
        if let Some(caps) = &mut self.local_caps {
            caps.add_mob(category, players);
        }
        if let Some(cost) = cost {
            self.potential.add_charge(pos, cost.charge);
        }
    }

    pub(crate) fn count(&self, category: MobCategory) -> u64 {
        self.counts[category.index()]
    }
}

/// Vanilla's isRightDistanceToPlayerAndSpawnPoint excludes exactly 24 blocks.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn can_spawn_at_distance(dist_sq: f64, category: MobCategory) -> bool {
    dist_sq.is_finite()
        && dist_sq > SPAWN_MIN_PLAYER_DIST_SQ
        && dist_sq <= category.despawn_distance() * category.despawn_distance()
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DespawnDecision {
    Keep,
    DespawnNow,
    DespawnRandom,
    ResetNoActionTime,
}

/// Pass the closest non-spectator player's squared distance, or None if absent.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn evaluate_despawn(
    dist_sq_nearest_non_spectator: Option<f64>,
    no_action_time: i32,
    persistent_or_passenger: bool,
    remove_when_far_away: bool,
    despawn_distance: f64,
    no_despawn_distance: f64,
    rng_next_800: i32,
) -> DespawnDecision {
    if persistent_or_passenger {
        return DespawnDecision::ResetNoActionTime;
    }
    let Some(dist_sq) = dist_sq_nearest_non_spectator else {
        return DespawnDecision::Keep;
    };
    if remove_when_far_away && dist_sq > despawn_distance * despawn_distance {
        DespawnDecision::DespawnNow
    } else if remove_when_far_away
        && no_action_time > 600
        && rng_next_800 == 0
        && dist_sq > no_despawn_distance * no_despawn_distance
    {
        DespawnDecision::DespawnRandom
    } else if dist_sq < no_despawn_distance * no_despawn_distance {
        DespawnDecision::ResetNoActionTime
    } else {
        DespawnDecision::Keep
    }
}

/// Peaceful is checked before persistence in Mob.checkDespawn.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn evaluate_despawn_with_peaceful(
    peaceful: bool,
    despawns_in_peaceful: bool,
    distance_sq: Option<f64>,
    no_action_time: i32,
    persistent_or_passenger: bool,
    remove_when_far_away: bool,
    category: MobCategory,
    rng_next_800: i32,
) -> DespawnDecision {
    if peaceful && despawns_in_peaceful {
        DespawnDecision::DespawnNow
    } else {
        evaluate_despawn(
            distance_sq,
            no_action_time,
            persistent_or_passenger,
            remove_when_far_away,
            category.despawn_distance(),
            category.no_despawn_distance(),
            rng_next_800,
        )
    }
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SpawnPlacementType {
    OnGround,
    InWater,
    InLava,
    NoRestrictions,
}

/// The world resolves support and valid empty blocks using actual block/fluid rules.
#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct SpawnPlacementContext {
    pub ground_supports_spawn: bool,
    pub clear_here: bool,
    pub clear_above: bool,
    pub water_here: bool,
    pub water_below: bool,
    pub lava_here: bool,
}

#[cfg_attr(not(test), allow(dead_code))]
impl SpawnPlacementType {
    pub(crate) fn allows(self, site: SpawnPlacementContext) -> bool {
        match self {
            Self::OnGround => site.ground_supports_spawn && site.clear_here && site.clear_above,
            Self::InWater => site.water_here && site.water_below && site.clear_above,
            Self::InLava => site.lava_here,
            Self::NoRestrictions => true,
        }
    }
}

/// Dimension-sampled spawn-light threshold and separate skylight/blocklight.
#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Copy, Debug)]
pub(crate) struct MonsterLightSample {
    pub sky_light: u8,
    pub block_light: u8,
    pub sky_darken: u8,
    pub thundering: bool,
    pub monster_spawn_block_light_limit: u8,
    pub sampled_spawn_light_limit: u8,
}

#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn is_dark_enough_to_spawn(light: MonsterLightSample, sky_roll_0_to_31: i32) -> bool {
    if i32::from(light.sky_light) > sky_roll_0_to_31 {
        return false;
    }
    if light.monster_spawn_block_light_limit < 15
        && light.block_light > light.monster_spawn_block_light_limit
    {
        return false;
    }
    let darken = if light.thundering {
        10
    } else {
        light.sky_darken
    };
    light
        .block_light
        .max(light.sky_light.saturating_sub(darken))
        <= light.sampled_spawn_light_limit
}

/// Selected biome spawner entry, with type-specific darkness and cluster rules.
#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Debug)]
pub(crate) struct NaturalSpawnCandidate {
    pub entity_id: String,
    pub placement: SpawnPlacementType,
    pub cost: Option<MobSpawnCost>,
    pub min_group_size: u8,
    pub max_group_size: u8,
    pub max_cluster_size: u32,
    pub requires_darkness: bool,
}

#[cfg_attr(not(test), allow(dead_code))]
pub(crate) trait SpawnWorld {
    fn can_start_group(&self, origin: NaturalSpawnPos) -> bool;
    fn spawnable_chunk_at(&self, pos: NaturalSpawnPos) -> bool;
    fn within_world_spawn_exclusion(&self, pos: NaturalSpawnPos) -> bool;
    fn nearest_non_spectator_dist_sq(&self, pos: NaturalSpawnPos) -> Option<f64>;
    fn players_close_to_chunk(&self, chunk: (i32, i32)) -> Vec<u64>;
    fn choose_candidate(
        &mut self,
        category: MobCategory,
        pos: NaturalSpawnPos,
        rng: &mut Lcg,
    ) -> Option<NaturalSpawnCandidate>;
    fn placement_context(&self, pos: NaturalSpawnPos) -> SpawnPlacementContext;
    fn monster_light_sample(&self, pos: NaturalSpawnPos) -> Option<MonsterLightSample>;
    fn spawn_rules_allow(&self, candidate: &NaturalSpawnCandidate, pos: NaturalSpawnPos) -> bool;
    fn no_collision(&self, candidate: &NaturalSpawnCandidate, pos: NaturalSpawnPos) -> bool;
    /// True only when the entity was added successfully.
    fn spawn(
        &mut self,
        candidate: &NaturalSpawnCandidate,
        pos: NaturalSpawnPos,
        rng: &mut Lcg,
    ) -> bool;
}

/// Up to three groups, with biome-selected attempt counts, jittered on the same Y slice
/// by nextInt(6)-nextInt(6).
/// Rechecks the global cap for each insertion so a pack cannot overshoot it.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn try_spawn_mob_group(
    world: &mut impl SpawnWorld,
    state: &mut NaturalSpawnState,
    category: MobCategory,
    origin: NaturalSpawnPos,
    rng: &mut Lcg,
) -> u32 {
    if !can_spawn_for_category(category, state.count(category), state.spawnable_chunk_count)
        || !world.can_start_group(origin)
    {
        return 0;
    }

    let mut spawned = 0;
    for _ in 0..3 {
        let (mut x, mut z) = (origin.x, origin.z);
        let mut attempts = (rng.next_f32() * 4.0).ceil() as i32;
        let mut selected: Option<NaturalSpawnCandidate> = None;
        let mut trial = 0;
        while trial < attempts {
            trial += 1;
            x += rng.next_bound(6) - rng.next_bound(6);
            z += rng.next_bound(6) - rng.next_bound(6);
            let pos = NaturalSpawnPos { x, y: origin.y, z };
            let Some(dist_sq) = world.nearest_non_spectator_dist_sq(pos) else {
                continue;
            };
            if !can_spawn_at_distance(dist_sq, category)
                || world.within_world_spawn_exclusion(pos)
                || !world.spawnable_chunk_at(pos)
            {
                continue;
            }

            if selected.is_none() {
                let Some(candidate) = world.choose_candidate(category, pos, rng) else {
                    break;
                };
                if candidate.min_group_size == 0
                    || candidate.min_group_size > candidate.max_group_size
                {
                    break;
                }
                let span = i32::from(candidate.max_group_size - candidate.min_group_size) + 1;
                attempts = i32::from(candidate.min_group_size) + rng.next_bound(span);
                selected = Some(candidate);
            }
            let candidate = selected.as_ref().expect("candidate selected above");
            if !can_spawn_for_category(category, state.count(category), state.spawnable_chunk_count)
            {
                return spawned;
            }
            if !candidate.placement.allows(world.placement_context(pos))
                || !world.spawn_rules_allow(candidate, pos)
            {
                continue;
            }
            if candidate.requires_darkness {
                let Some(light) = world.monster_light_sample(pos) else {
                    continue;
                };
                if !is_dark_enough_to_spawn(light, rng.next_bound(32)) {
                    continue;
                }
            }
            if !world.no_collision(candidate, pos) {
                continue;
            }
            let players = world.players_close_to_chunk((pos.x >> 4, pos.z >> 4));
            if !state.can_spawn(category, pos, candidate.cost, &players) {
                continue;
            }
            if world.spawn(candidate, pos, rng) {
                state.record_success(category, pos, candidate.cost, &players);
                spawned += 1;
                if spawned >= candidate.max_cluster_size {
                    return spawned;
                }
            }
        }
    }
    spawned
}

const NATURAL_SPAWN_CHUNK_RADIUS: i32 = 8;

fn has_room_for_nightly_monster(mobs: &[Arc<Mob>], monster_cap: usize) -> bool {
    mobs.len() < MOB_CAP
        && mobs
            .iter()
            .filter(|mob| entity_category_1211(mob.etype) == EntityMobCategory::Monster)
            .take(monster_cap)
            .count()
            < monster_cap
}

fn spawn_nightly_hostile(tick: u64) {
    let players: Vec<_> = client_snapshot()
        .into_iter()
        .filter_map(|client| {
            if client.is_spectator() {
                return None;
            }
            let dim = client.dim.lock().unwrap();
            if *dim != DimId::Overworld {
                return None;
            }
            let (x, y, z, ..) = *client.pos.lock().unwrap();
            Some((x, y, z))
        })
        .collect();
    if players.is_empty() {
        return;
    }

    let mut spawn_chunks = HashSet::new();
    for &(x, _, z) in &players {
        let center = (
            (x.floor() as i32).div_euclid(16),
            (z.floor() as i32).div_euclid(16),
        );
        extend_chunk_square(&mut spawn_chunks, center, NATURAL_SPAWN_CHUNK_RADIUS);
    }
    let cap = MobCategory::Monster.global_cap(spawn_chunks.len() as u32) as usize;
    if !has_room_for_nightly_monster(&mobs().lock().unwrap(), cap) {
        return;
    }

    // Rotate eligible anchors on attempts, independently of the tick cadence.
    let attempt = SPAWN_ATTEMPT.fetch_add(1, Ordering::Relaxed);
    let (px, _, pz) = players[attempt as usize % players.len()];
    let angle = tick as f64 * 2.399;
    let bx = (px + angle.cos() * 32.0).floor() as i32;
    let bz = (pz + angle.sin() * 32.0).floor() as i32;
    let (x, y, z) = (f64::from(bx) + 0.5, surface_y(bx, bz), f64::from(bz) + 0.5);
    let nearest_squared = players
        .iter()
        .map(|&(px, py, pz)| (px - x).powi(2) + (py - y).powi(2) + (pz - z).powi(2))
        .fold(f64::INFINITY, f64::min);
    if !can_spawn_at_distance(nearest_squared, MobCategory::Monster) {
        return;
    }

    let biome = worldgen::biome_name_at_block(bx, y.floor() as i32, bz);
    let etype = spawn_etype_for_biome(biome.as_deref(), attempt);
    let mut current_mobs = mobs().lock().unwrap();
    // A concurrent writer can fill a slot while terrain and biome are read.
    if has_room_for_nightly_monster(&current_mobs, cap) {
        current_mobs.push(Arc::new(Mob {
            eid: NEXT_EID.fetch_add(1, Ordering::SeqCst),
            uuid: [0u8; 16],
            etype,
            pos: Mutex::new((x, y, z, 0.0, 0.0)),
            hp: Mutex::new(20.0),
            last_hit: Mutex::new(Instant::now() - Duration::from_secs(10)),
            hurt_at: Mutex::new(Instant::now() - Duration::from_secs(10)),
            last_hurt_tick: Mutex::new(None),
            vel: Mutex::new([0.0; 3]),
            fuse: Mutex::new(0),
            love: Mutex::new(0),
            air: Mutex::new(server_ticks(AIR_SUPPLY_GT) as i32),
        }));
    }
}

#[cfg(test)]
mod natural_spawner_tests {
    use super::*;

    const CATEGORIES: [MobCategory; 7] = [
        MobCategory::Monster,
        MobCategory::Creature,
        MobCategory::Ambient,
        MobCategory::Axolotls,
        MobCategory::UndergroundWaterCreature,
        MobCategory::WaterCreature,
        MobCategory::WaterAmbient,
    ];

    fn pos(x: i32, y: i32, z: i32) -> NaturalSpawnPos {
        NaturalSpawnPos { x, y, z }
    }

    #[test]
    fn mob_categories_and_caps_use_integer_truncation() {
        for (cat, max) in CATEGORIES.into_iter().zip([70, 10, 15, 5, 5, 5, 20]) {
            assert_eq!(cat.max_instances_per_chunk(), max);
            assert_eq!(cat.global_cap(289), max);
            assert_eq!(cat.global_cap(578), max * 2);
            assert_eq!(cat.global_cap(0), 0);
            assert_eq!(cat.is_friendly(), cat != MobCategory::Monster);
            assert_eq!(cat.is_persistent(), cat == MobCategory::Creature);
            assert_eq!(cat.no_despawn_distance(), 32.0);
        }
        assert_eq!(MobCategory::Monster.global_cap(288), 69);
        assert_eq!(MobCategory::Monster.global_cap(578), 140);
        assert!(can_spawn_for_category(MobCategory::Monster, 69, 289));
        assert!(!can_spawn_for_category(MobCategory::Monster, 70, 289));
        assert!(!can_spawn_for_category(MobCategory::Monster, 0, 1));
        assert_eq!(SPAWN_MAX_PLAYER_DIST_SQ, 16_384.0);
    }

    #[test]
    fn local_cap_uses_any_nearby_player_and_counts_each_once() {
        let mut caps = LocalMobCapCalculator::default();
        assert!(!caps.can_spawn(MobCategory::Monster, &[]));
        for _ in 0..70 {
            caps.add_mob(MobCategory::Monster, &[1, 1]);
        }
        assert_eq!(caps.count(1, MobCategory::Monster), 70);
        assert!(!caps.can_spawn(MobCategory::Monster, &[1]));
        assert!(caps.can_spawn(MobCategory::Monster, &[1, 2]));
        caps.add_mob(MobCategory::Monster, &[1, 2]);
        assert_eq!(caps.count(1, MobCategory::Monster), 71);
        assert_eq!(caps.count(2, MobCategory::Monster), 1);
        assert!(caps.can_spawn(MobCategory::Creature, &[1]));
    }

    #[test]
    fn soul_sand_valley_potential_is_sparse_and_rejects_dense_spawns() {
        let cost = MobSpawnCost {
            energy_budget: 0.15,
            charge: 0.7,
        };
        let mut potential = PotentialCalculator::default();
        let start = pos(0, 64, 0);
        assert!(potential.can_spawn(start, cost));
        potential.add_charge(start, cost.charge);
        assert!(potential
            .get_potential_energy_change(start, cost.charge)
            .is_infinite());
        assert!(!potential.can_spawn(start, cost));
        assert!(!potential.can_spawn(pos(1, 64, 0), cost));
        assert!(potential.can_spawn(pos(8, 64, 0), cost));
        assert!(
            (potential.get_potential_energy_change(pos(8, 64, 0), cost.charge) - 0.06125).abs()
                < 1e-12
        );
        potential.add_charge(pos(8, 64, 0), cost.charge);
        assert!(!potential.can_spawn(pos(4, 64, 0), cost));
        assert!(potential.can_spawn(pos(40, 64, 0), cost));
        assert_eq!(
            potential.charges[0],
            PointCharge {
                x: 0.0,
                y: 64.0,
                z: 0.0,
                charge: 0.7
            }
        );
    }

    #[test]
    fn potential_ignores_zero_charges_and_zero_candidate_mass() {
        let start = pos(0, 64, 0);
        let mut potential = PotentialCalculator::default();
        potential.add_charge(start, 0.0);
        potential.add_charge(start, -0.0);
        assert!(potential.charges.is_empty());
        assert_eq!(potential.get_potential_energy_change(start, 1.0), 0.0);

        potential.add_charge(start, -1.0);
        assert_eq!(potential.get_potential_energy_change(start, -0.0), 0.0);
        assert_eq!(potential.get_potential_energy_change(start, 1.0), f64::INFINITY);
    }

    #[test]
    fn spawn_state_census_and_optional_local_caps() {
        let mut state = NaturalSpawnState::new(289, true);
        let cost = MobSpawnCost {
            energy_budget: 0.15,
            charge: 0.7,
        };
        assert!(state.can_spawn(MobCategory::Monster, pos(0, 64, 0), Some(cost), &[3]));
        state.count_existing(MobCategory::Monster, pos(0, 64, 0), Some(cost), &[3]);
        assert!(!state.can_spawn(MobCategory::Monster, pos(0, 64, 0), Some(cost), &[3]));
        assert!(state.can_spawn(MobCategory::Monster, pos(8, 64, 0), Some(cost), &[3]));
        state.record_success(MobCategory::Monster, pos(8, 64, 0), None, &[3]);
        assert_eq!(state.count(MobCategory::Monster), 2);
        assert_eq!(
            state
                .local_caps
                .as_ref()
                .unwrap()
                .count(3, MobCategory::Monster),
            2
        );
        assert_eq!(state.potential.charges.len(), 1);
        assert!(NaturalSpawnState::new(289, false).can_spawn(
            MobCategory::Monster,
            pos(0, 64, 0),
            None,
            &[]
        ));
    }

    #[test]
    fn spawn_and_despawn_player_distance_contract() {
        assert!(!can_spawn_at_distance(23.0 * 23.0, MobCategory::Monster));
        // Vanilla's 24-block exclusion uses <= 576.
        assert!(!can_spawn_at_distance(576.0, MobCategory::Monster));
        assert!(can_spawn_at_distance(25.0 * 25.0, MobCategory::Monster));
        assert!(can_spawn_at_distance(128.0 * 128.0, MobCategory::Monster));
        assert!(!can_spawn_at_distance(129.0 * 129.0, MobCategory::Monster));
        assert_eq!(MobCategory::WaterAmbient.despawn_distance(), 64.0);
        assert!(can_spawn_at_distance(
            64.0 * 64.0,
            MobCategory::WaterAmbient
        ));
        assert!(!can_spawn_at_distance(
            65.0 * 65.0,
            MobCategory::WaterAmbient
        ));
        assert!(!can_spawn_at_distance(f64::NAN, MobCategory::Monster));

        let check = |distance, idle, persistent, remove, roll| {
            evaluate_despawn(distance, idle, persistent, remove, 128.0, 32.0, roll)
        };
        assert_eq!(check(None, 1000, false, true, 0), DespawnDecision::Keep);
        assert_eq!(
            check(Some(129.0 * 129.0), 0, false, true, 1),
            DespawnDecision::DespawnNow
        );
        assert_eq!(
            check(Some(31.0 * 31.0), 601, false, true, 0),
            DespawnDecision::ResetNoActionTime
        );
        // Mob.checkDespawn resets only when distance squared is strictly < 1024.
        assert_eq!(
            check(Some(32.0 * 32.0), 601, false, true, 0),
            DespawnDecision::Keep
        );
        assert_eq!(
            check(Some(33.0 * 33.0), 600, false, true, 0),
            DespawnDecision::Keep
        );
        assert_eq!(
            check(Some(33.0 * 33.0), 601, false, true, 1),
            DespawnDecision::Keep
        );
        assert_eq!(
            check(Some(33.0 * 33.0), 601, false, true, 0),
            DespawnDecision::DespawnRandom
        );
        assert_eq!(
            check(Some(129.0 * 129.0), 601, true, true, 0),
            DespawnDecision::ResetNoActionTime
        );
        assert_eq!(
            check(None, 601, true, false, 1),
            DespawnDecision::ResetNoActionTime
        );
        assert_eq!(
            check(Some(129.0 * 129.0), 601, false, false, 0),
            DespawnDecision::Keep
        );
        // removeWhenFarAway gates removal, but vanilla still resets idle time
        // inside the no-despawn radius when a player is present.
        assert_eq!(
            check(Some(31.0 * 31.0), 601, false, false, 0),
            DespawnDecision::ResetNoActionTime
        );
        assert_eq!(
            evaluate_despawn_with_peaceful(
                true,
                true,
                None,
                0,
                true,
                false,
                MobCategory::Monster,
                1
            ),
            DespawnDecision::DespawnNow
        );
        assert_eq!(
            evaluate_despawn_with_peaceful(
                true,
                false,
                Some(65.0 * 65.0),
                0,
                false,
                true,
                MobCategory::WaterAmbient,
                1
            ),
            DespawnDecision::DespawnNow
        );
        assert_eq!(
            evaluate_despawn_with_peaceful(
                false,
                true,
                None,
                0,
                false,
                true,
                MobCategory::Monster,
                0
            ),
            DespawnDecision::Keep
        );
    }

    fn ground() -> SpawnPlacementContext {
        SpawnPlacementContext {
            ground_supports_spawn: true,
            clear_here: true,
            clear_above: true,
            ..Default::default()
        }
    }

    fn dark() -> MonsterLightSample {
        MonsterLightSample {
            sky_light: 0,
            block_light: 0,
            sky_darken: 11,
            thundering: false,
            monster_spawn_block_light_limit: 0,
            sampled_spawn_light_limit: 0,
        }
    }

    #[test]
    fn placement_and_dimension_light_predicates() {
        assert!(SpawnPlacementType::OnGround.allows(ground()));
        assert!(!SpawnPlacementType::OnGround.allows(SpawnPlacementContext {
            ground_supports_spawn: false,
            ..ground()
        }));
        assert!(SpawnPlacementType::InWater.allows(SpawnPlacementContext {
            water_here: true,
            water_below: true,
            clear_above: true,
            ..Default::default()
        }));
        assert!(!SpawnPlacementType::InWater.allows(SpawnPlacementContext {
            water_here: true,
            clear_above: true,
            ..Default::default()
        }));
        assert!(SpawnPlacementType::InLava.allows(SpawnPlacementContext {
            lava_here: true,
            ..Default::default()
        }));
        assert!(!SpawnPlacementType::InLava.allows(ground()));
        assert!(SpawnPlacementType::NoRestrictions.allows(Default::default()));
        assert!(is_dark_enough_to_spawn(dark(), 0));
        assert!(!is_dark_enough_to_spawn(
            MonsterLightSample {
                block_light: 1,
                ..dark()
            },
            31
        ));
        assert!(!is_dark_enough_to_spawn(
            MonsterLightSample {
                sky_light: 8,
                ..dark()
            },
            7
        ));
        assert!(is_dark_enough_to_spawn(
            MonsterLightSample {
                sky_light: 8,
                thundering: true,
                ..dark()
            },
            8
        ));
        assert!(!is_dark_enough_to_spawn(
            MonsterLightSample {
                sky_light: 8,
                sky_darken: 0,
                ..dark()
            },
            31
        ));
        assert!(is_dark_enough_to_spawn(
            MonsterLightSample {
                block_light: 6,
                monster_spawn_block_light_limit: 15,
                sampled_spawn_light_limit: 7,
                ..dark()
            },
            0
        ));
        assert!(!is_dark_enough_to_spawn(
            MonsterLightSample {
                block_light: 8,
                monster_spawn_block_light_limit: 15,
                sampled_spawn_light_limit: 7,
                ..dark()
            },
            0
        ));
    }

    struct MockWorld {
        nearest: Option<f64>,
        light: Option<MonsterLightSample>,
        candidate: NaturalSpawnCandidate,
        placement: SpawnPlacementContext,
        collision: bool,
        inserted_ok: bool,
        rules_ok: bool,
        spawnable: bool,
        excluded: bool,
        candidate_choices: usize,
        inserted: Vec<NaturalSpawnPos>,
    }

    impl MockWorld {
        fn new() -> Self {
            Self {
                nearest: Some(40.0 * 40.0),
                light: Some(dark()),
                candidate: NaturalSpawnCandidate {
                    entity_id: "minecraft:skeleton".to_owned(),
                    placement: SpawnPlacementType::OnGround,
                    cost: None,
                    min_group_size: 4,
                    max_group_size: 4,
                    max_cluster_size: 100,
                    requires_darkness: true,
                },
                placement: ground(),
                collision: true,
                inserted_ok: true,
                rules_ok: true,
                spawnable: true,
                excluded: false,
                candidate_choices: 0,
                inserted: Vec::new(),
            }
        }
    }

    impl SpawnWorld for MockWorld {
        fn can_start_group(&self, _: NaturalSpawnPos) -> bool {
            self.spawnable
        }
        fn spawnable_chunk_at(&self, _: NaturalSpawnPos) -> bool {
            self.spawnable
        }
        fn within_world_spawn_exclusion(&self, _: NaturalSpawnPos) -> bool {
            self.excluded
        }
        fn nearest_non_spectator_dist_sq(&self, _: NaturalSpawnPos) -> Option<f64> {
            self.nearest
        }
        fn players_close_to_chunk(&self, _: (i32, i32)) -> Vec<u64> {
            vec![7]
        }
        fn choose_candidate(
            &mut self,
            _: MobCategory,
            _: NaturalSpawnPos,
            _: &mut Lcg,
        ) -> Option<NaturalSpawnCandidate> {
            self.candidate_choices += 1;
            Some(self.candidate.clone())
        }
        fn placement_context(&self, _: NaturalSpawnPos) -> SpawnPlacementContext {
            self.placement
        }
        fn monster_light_sample(&self, _: NaturalSpawnPos) -> Option<MonsterLightSample> {
            self.light
        }
        fn spawn_rules_allow(&self, candidate: &NaturalSpawnCandidate, _: NaturalSpawnPos) -> bool {
            self.rules_ok && candidate.entity_id == "minecraft:skeleton"
        }
        fn no_collision(&self, _: &NaturalSpawnCandidate, _: NaturalSpawnPos) -> bool {
            self.collision
        }
        fn spawn(&mut self, _: &NaturalSpawnCandidate, pos: NaturalSpawnPos, _: &mut Lcg) -> bool {
            if self.inserted_ok {
                self.inserted.push(pos);
            }
            self.inserted_ok
        }
    }

    #[test]
    fn pack_uses_five_attempts_for_soul_sand_valley_skeleton() {
        let mut world = MockWorld::new();
        world.candidate.min_group_size = 5;
        world.candidate.max_group_size = 5;
        world.candidate.max_cluster_size = 5;
        let mut state = NaturalSpawnState::new(289, false);

        let spawned = try_spawn_mob_group(
            &mut world,
            &mut state,
            MobCategory::Monster,
            pos(0, 65, 0),
            &mut Lcg::new(17),
        );

        assert_eq!(spawned, 5);
        assert_eq!(world.inserted.len(), 5);
        assert_eq!(state.count(MobCategory::Monster), 5);
        assert_eq!(world.candidate_choices, 1, "all five must spawn in the first group");
    }

    #[test]
    fn pack_stops_at_global_cap_without_overshooting() {
        let mut world = MockWorld::new();
        let mut state = NaturalSpawnState::new(289, true);
        for _ in 0..69 {
            state.count_existing(MobCategory::Monster, pos(300, 65, 300), None, &[7]);
        }
        let n = try_spawn_mob_group(
            &mut world,
            &mut state,
            MobCategory::Monster,
            pos(0, 65, 0),
            &mut Lcg::new(17),
        );
        assert_eq!(n, 1);
        assert_eq!(state.count(MobCategory::Monster), 70);
        assert_eq!(world.inserted.len(), 1);
        assert_eq!(
            state
                .local_caps
                .as_ref()
                .unwrap()
                .count(7, MobCategory::Monster),
            70
        );
        assert_eq!(
            try_spawn_mob_group(
                &mut world,
                &mut state,
                MobCategory::Monster,
                pos(0, 65, 0),
                &mut Lcg::new(17)
            ),
            0
        );
    }

    #[test]
    fn pack_gates_before_insert_and_only_records_success() {
        let mut world = MockWorld::new();
        let mut state = NaturalSpawnState::new(289, false);
        world.light = Some(MonsterLightSample {
            block_light: 1,
            ..dark()
        });
        assert_eq!(
            try_spawn_mob_group(
                &mut world,
                &mut state,
                MobCategory::Monster,
                pos(0, 65, 0),
                &mut Lcg::new(31)
            ),
            0
        );
        world.light = Some(dark());
        world.collision = false;
        assert_eq!(
            try_spawn_mob_group(
                &mut world,
                &mut state,
                MobCategory::Monster,
                pos(0, 65, 0),
                &mut Lcg::new(31)
            ),
            0
        );
        world.collision = true;
        world.nearest = None; // spectators do not count as nearby players.
        assert_eq!(
            try_spawn_mob_group(
                &mut world,
                &mut state,
                MobCategory::Monster,
                pos(0, 65, 0),
                &mut Lcg::new(31)
            ),
            0
        );
        world.nearest = Some(40.0 * 40.0);
        world.inserted_ok = false;
        assert_eq!(
            try_spawn_mob_group(
                &mut world,
                &mut state,
                MobCategory::Monster,
                pos(0, 65, 0),
                &mut Lcg::new(31)
            ),
            0
        );
        assert_eq!(state.count(MobCategory::Monster), 0);
        world.inserted_ok = true;
        world.candidate.max_cluster_size = 2;
        assert_eq!(
            try_spawn_mob_group(
                &mut world,
                &mut state,
                MobCategory::Monster,
                pos(0, 65, 0),
                &mut Lcg::new(31)
            ),
            2
        );
        assert_eq!(world.inserted.len(), 2);
        assert!(world
            .inserted
            .iter()
            .all(|p| p.y == 65 && p.x.abs() <= 15 && p.z.abs() <= 15));
    }

    #[test]
    fn pack_checks_energy_budget_and_chunk_restrictions() {
        let mut world = MockWorld::new();
        let mut state = NaturalSpawnState::new(289, false);
        world.candidate.cost = Some(MobSpawnCost {
            energy_budget: 0.15,
            charge: 0.7,
        });
        world.spawnable = false;
        assert_eq!(
            try_spawn_mob_group(
                &mut world,
                &mut state,
                MobCategory::Monster,
                pos(0, 65, 0),
                &mut Lcg::new(100)
            ),
            0
        );
        world.spawnable = true;
        world.excluded = true;
        assert_eq!(
            try_spawn_mob_group(
                &mut world,
                &mut state,
                MobCategory::Monster,
                pos(0, 65, 0),
                &mut Lcg::new(100)
            ),
            0
        );
        world.excluded = false;
        let n = try_spawn_mob_group(
            &mut world,
            &mut state,
            MobCategory::Monster,
            pos(0, 65, 0),
            &mut Lcg::new(100),
        );
        assert!(n > 0);
        assert_eq!(state.count(MobCategory::Monster), u64::from(n));
        assert_eq!(state.potential.charges.len(), n as usize);
        assert!(state.potential.charges.windows(2).all(|w| w[0] != w[1]));
    }

    #[test]
    fn pack_honors_local_cap_independently_of_global_cap() {
        let mut world = MockWorld::new();
        let mut state = NaturalSpawnState::new(578, true);
        for _ in 0..70 {
            state.count_existing(MobCategory::Monster, pos(300, 65, 300), None, &[7]);
        }
        assert!(can_spawn_for_category(MobCategory::Monster, 70, 578));
        let origin = pos(0, 65, 0);
        assert_eq!(
            try_spawn_mob_group(
                &mut world,
                &mut state,
                MobCategory::Monster,
                origin,
                &mut Lcg::new(17)
            ),
            0
        );
        assert!(world.inserted.is_empty());

        let mut no_local_cap = NaturalSpawnState::new(578, false);
        for _ in 0..70 {
            no_local_cap.count_existing(MobCategory::Monster, pos(300, 65, 300), None, &[]);
        }
        assert!(
            try_spawn_mob_group(
                &mut world,
                &mut no_local_cap,
                MobCategory::Monster,
                origin,
                &mut Lcg::new(17),
            ) > 0
        );
    }
}
