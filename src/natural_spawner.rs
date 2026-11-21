// Minecraft 1.21.1 NaturalSpawner primitives. Included directly in main.rs.
// The world adapter owns actual entities, block queries and spawned-chunk selection.
use crate::generation_math::Lcg;
include!("mob_despawn.rs");
include!("spawn_costs.rs");

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
    pub above_redstone_conductor: bool,
    pub lava_here: bool,
}

#[cfg_attr(not(test), allow(dead_code))]
impl SpawnPlacementType {
    pub(crate) fn allows(self, site: SpawnPlacementContext) -> bool {
        match self {
            Self::OnGround => site.ground_supports_spawn && site.clear_here && site.clear_above,
            Self::InWater => site.water_here && !site.above_redstone_conductor,
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
    let sky_darken = if light.thundering {
        10
    } else {
        i32::from(light.sky_darken)
    };
    monster_spawn_light_allows(
        i32::from(light.sky_light),
        sky_roll_0_to_31,
        i32::from(light.monster_spawn_block_light_limit),
        || i32::from(light.block_light),
        sky_darken,
        || i32::from(light.sampled_spawn_light_limit),
    )
}

fn monster_spawn_light_allows(
    sky_light: i32,
    sky_roll: i32,
    block_light_limit: i32,
    read_block_light: impl FnOnce() -> i32,
    sky_darken: i32,
    sample_spawn_light_limit: impl FnOnce() -> i32,
) -> bool {
    if sky_light > sky_roll {
        return false;
    }
    let block_light = read_block_light();
    if block_light_limit < 15 && block_light > block_light_limit {
        return false;
    }
    block_light.max(sky_light - sky_darken) <= sample_spawn_light_limit()
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

struct SpawnPack {
    pos: NaturalSpawnPos,
    attempts: i32,
    tried: i32,
}

impl SpawnPack {
    fn new(origin: NaturalSpawnPos, roll: f32) -> Self {
        Self {
            pos: origin,
            attempts: (roll * 4.0).ceil() as i32,
            tried: 0,
        }
    }

    fn next_pos(&mut self, mut next_below: impl FnMut(i32) -> i32) -> Option<NaturalSpawnPos> {
        if self.tried >= self.attempts {
            return None;
        }
        self.tried += 1;
        self.pos.x += next_below(6) - next_below(6);
        self.pos.z += next_below(6) - next_below(6);
        Some(self.pos)
    }
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
        let mut pack = SpawnPack::new(origin, rng.next_f32());
        let mut selected: Option<NaturalSpawnCandidate> = None;
        while let Some(pos) = pack.next_pos(|bound| rng.next_bound(bound)) {
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
                pack.attempts = i32::from(candidate.min_group_size) + rng.next_bound(span);
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
const NIGHTLY_MONSTER_CLUSTER_LIMIT: u32 = 4;

fn has_room_for_nightly_monster(mobs: &[Arc<Mob>], monster_cap: usize) -> bool {
    mobs.len() < MOB_CAP
        && mobs
            .iter()
            .filter(|mob| mob.dim == DimId::Overworld && entity_category_1211(mob.etype) == EntityMobCategory::Monster)
            .take(monster_cap)
            .count()
            < monster_cap
}

fn nightly_spawn_placement_allows(bx: i32, by: i32, bz: i32) -> bool {
    let Some(reg) = crate::block_registry::breg() else {
        return false;
    };
    let clear = |state| {
        !reg.is_collision_shape_full_block(state)
            && !reg.is_liquid(state)
            && !reg.in_tag(state, "minecraft:prevent_mob_spawning_inside")
    };
    let site = SpawnPlacementContext {
        ground_supports_spawn: reg.side_solid(block_at(bx, by - 1, bz), 1),
        clear_here: clear(block_at(bx, by, bz)),
        clear_above: clear(block_at(bx, by + 1, bz)),
        ..SpawnPlacementContext::default()
    };
    SpawnPlacementType::OnGround.allows(site)
}

struct OverworldMonsterLight {
    block_limit: i32,
    min: i32,
    max: i32,
}

impl OverworldMonsterLight {
    fn from_dimension_type(data: &[u8]) -> Option<Self> {
        let dimension = jparse(data)?;
        let level = |object: &J, key| match object.get(key) {
            Some(J::Num(n)) if (0.0..=15.0).contains(n) && n.fract() == 0.0 => Some(*n as i32),
            _ => None,
        };
        let block_limit = level(&dimension, "monster_spawn_block_light_limit")?;
        let provider = dimension.get("monster_spawn_light_level")?;
        if provider.get("type")?.str() != "minecraft:uniform" {
            return None;
        }
        let min = level(provider, "min_inclusive")?;
        let max = level(provider, "max_inclusive")?;
        (min <= max).then_some(Self {
            block_limit,
            min,
            max,
        })
    }
}

fn nightly_spawn_light_sources(clients: &[Arc<Client>], cx: i32, cz: i32) -> GenSources {
    natural_spawn_light_sources(clients, DimId::Overworld, cx, cz)
}

fn natural_spawn_light_sources(clients: &[Arc<Client>], dim: DimId, cx: i32, cz: i32) -> GenSources {
    let mut nearby = GenSources::new();
    for client in clients {
        // Chunk streaming takes these locks in this order.
        let sources = client.gen_sources.lock().unwrap();
        if *client.dim.lock().unwrap() != dim {
            continue;
        }
        for dx in -1..=1 {
            for dz in -1..=1 {
                let key = (cx + dx, cz + dz);
                if let Some(chunk) = sources.get(&key) {
                    nearby.entry(key).or_insert_with(|| chunk.clone());
                }
            }
        }
    }
    nearby
}

fn nightly_spawn_is_dark(x: i32, y: i32, z: i32, sources: &GenSources) -> bool {
    static RULES: OnceLock<OverworldMonsterLight> = OnceLock::new();
    let rules = RULES.get_or_init(|| {
        match crate::structure_transform::pinned_data_bytes(
            "datapacks/vanilla/data/minecraft/dimension_type/overworld.json",
        ) {
            Some(bytes) => OverworldMonsterLight::from_dimension_type(&bytes)
                .expect("pinned Overworld dimension type must have valid monster spawn light rules"),
            // A scratch run with no `datapacks/` tree (the wire-test
            // `local_server` layout) still spawns by the pinned 1.21.1
            // values: block-light limit 0, uniform light level 0..=7.
            None => OverworldMonsterLight {
                block_limit: 0,
                min: 0,
                max: 7,
            },
        }
    });
    let (cx, cz) = (x >> 4, z >> 4);
    let sky_light = nibble_at(&sky_light_sections_world(cx, cz), x, y, z);
    let sky_roll = world_rng::next_below(32) as i32;
    let sky_darken = if THUNDERING.load(Ordering::Relaxed) > 0 {
        10
    } else {
        current_sky_darken()
    };
    monster_spawn_light_allows(
        sky_light,
        sky_roll,
        rules.block_limit,
        || nibble_at(&block_light_sections(cx, cz, sources), x, y, z),
        sky_darken,
        || rules.min + world_rng::next_below((rules.max - rules.min + 1) as u32) as i32,
    )
}

fn nightly_monster_spawners(pos: NaturalSpawnPos) -> &'static [feature::SpawnerData] {
    let biome = worldgen::with_gen_dim(DimId::Overworld, |gen| {
        let (_, leaf) = gen.biome_blended(pos.x, pos.y, pos.z)?;
        gen.biome_name_of_leaf(leaf)
    })
    .flatten();
    // The flat runtime sends plains in chunks without a generator.
    feature::spawners_for(biome.as_deref().unwrap_or("minecraft:plains"), "monster")
        .unwrap_or_default()
}

fn choose_nightly_spawner(entries: &[feature::SpawnerData]) -> Option<&feature::SpawnerData> {
    let weight = entries.iter().try_fold(0i32, |total, entry| {
        total
            .checked_add(entry.weight)
            .filter(|_| entry.weight >= 0)
    })?;
    if weight == 0 {
        return None;
    }
    let mut roll = world_rng::next_below(weight as u32) as i32;
    entries.iter().find(|entry| {
        roll -= entry.weight;
        roll < 0
    })
}

fn nightly_spawn_float() -> f32 {
    (world_rng::next_u64() >> 40) as f32 / (1 << 24) as f32
}

fn nightly_spawn_obstructed(bounds: EntityAabb) -> bool {
    // Mob.checkSpawnObstruction first rejects any nonempty fluid state in the AABB.
    let reg = crate::block_registry::breg().unwrap();
    let [min_x, min_y, min_z] = bounds.min().map(|edge| edge.floor() as i32);
    let [max_x, max_y, max_z] = bounds.max().map(|edge| edge.ceil() as i32);
    for x in min_x..max_x {
        for y in min_y..max_y {
            for z in min_z..max_z {
                if reg.is_liquid(block_at(x, y, z)) {
                    return true;
                }
            }
        }
    }
    let cart_type = entity_spec_by_name("chest_minecart").expect("registered chest minecart");
    generated_content()
        .lock()
        .unwrap()
        .carts
        .iter()
        .filter_map(|cart| cart.pos().ok())
        .any(|[x, y, z]| {
            bounds.intersects(EntityAabb::from_dimensions(
                x,
                y,
                z,
                cart_type.width,
                cart_type.height,
            ))
        })
}

/// Fill `buf` with OS CSPRNG bytes for entity UUIDs: `/dev/urandom` on
/// Unix, the Win32 `BCryptGenRandom` system-preferred RNG on Windows.
#[cfg(unix)]
fn fill_entity_uuid_entropy(buf: &mut [u8; 16]) -> std::io::Result<()> {
    let mut f = std::fs::File::open("/dev/urandom")?;
    std::io::Read::read_exact(&mut f, buf)
}

/// Win32 CSPRNG: `BCryptGenRandom` with `BCRYPT_USE_SYSTEM_PREFERRED_RNG`
/// — the OS CNG generator, zero external crates.
#[cfg(windows)]
fn fill_entity_uuid_entropy(buf: &mut [u8; 16]) -> std::io::Result<()> {
    const BCRYPT_USE_SYSTEM_PREFERRED_RNG: u32 = 0x0000_0002;

    #[link(name = "bcrypt")]
    extern "system" {
        fn BCryptGenRandom(
            hAlgorithm: *mut std::ffi::c_void,
            pbBuffer: *mut u8,
            cbBuffer: u32,
            dwFlags: u32,
        ) -> i32;
    }

    // NTSTATUS: STATUS_SUCCESS == 0; `hAlgorithm` must be NULL when
    // BCRYPT_USE_SYSTEM_PREFERRED_RNG selects the system generator.
    let status = unsafe {
        BCryptGenRandom(
            std::ptr::null_mut(),
            buf.as_mut_ptr(),
            buf.len() as u32,
            BCRYPT_USE_SYSTEM_PREFERRED_RNG,
        )
    };
    if status == 0 {
        Ok(())
    } else {
        Err(std::io::Error::other(format!(
            "BCryptGenRandom failed (NTSTATUS {status:#010x})"
        )))
    }
}

#[cfg(not(any(unix, windows)))]
fn fill_entity_uuid_entropy(_buf: &mut [u8; 16]) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "no OS entropy source on this platform",
    ))
}

enum NightlySpawnAttempt {
    Rejected,
    Spawned,
    AtCap,
}

fn try_nightly_monster_at(
    pos: NaturalSpawnPos,
    etype: i64,
    cap: usize,
    light_sources: &GenSources,
) -> NightlySpawnAttempt {
    // Block edits and spawn insertion share the world-write guard.
    with_save_guard(|| {
        let NaturalSpawnPos { x, y, z } = pos;
        if !has_room_for_nightly_monster(&mobs().lock().unwrap(), cap) {
            return NightlySpawnAttempt::AtCap;
        }
        // These four types use Monster.checkMonsterSpawnRules. The tick has
        // already checked difficulty, and the placement gate checked support.
        if !nightly_spawn_placement_allows(x, y, z)
            || !nightly_spawn_is_dark(x, y, z, light_sources)
        {
            return NightlySpawnAttempt::Rejected;
        }
        let (x, y, z) = (f64::from(x) + 0.5, f64::from(y), f64::from(z) + 0.5);
        let (width, height) = mob_dimensions(etype);
        let bounds = EntityAabb::from_dimensions(x, y, z, width, height);
        if aabb_collides_blocks(bounds.min(), bounds.max()) {
            return NightlySpawnAttempt::Rejected;
        }
        // Vanilla moves the new mob into place and rolls yaw before instance
        // checks. Mob.checkSpawnRules is true for these four types.
        let yaw = nightly_spawn_float() * 360.0;
        if nightly_spawn_obstructed(bounds) {
            return NightlySpawnAttempt::Rejected;
        }
        let mut current_mobs = mobs().lock().unwrap();
        if !has_room_for_nightly_monster(&current_mobs, cap) {
            return NightlySpawnAttempt::AtCap;
        }
        if current_mobs.iter().filter(|mob| mob.dim == DimId::Overworld).any(|mob| {
            let (x, y, z, ..) = *mob.pos.lock().unwrap();
            let (width, height) = entity_dimensions_1211(mob.etype);
            bounds.intersects(EntityAabb::from_dimensions(x, y, z, width, height))
        }) {
            return NightlySpawnAttempt::Rejected;
        }
        let mut uuid = [0; 16];
        fill_entity_uuid_entropy(&mut uuid).expect("entity UUID entropy");
        uuid[6] = (uuid[6] & 15) | 64;
        uuid[8] = (uuid[8] & 63) | 128;
        push_natural_hostile(&mut current_mobs, Arc::new(Mob {
            dim: DimId::Overworld,
            eid: NEXT_EID.fetch_add(1, Ordering::SeqCst),
            uuid,
            etype,
            pos: Mutex::new((x, y, z, yaw, 0.0)),
            hp: Mutex::new(20.0),
            last_hit: Mutex::new(Instant::now() - Duration::from_secs(10)),
            hurt_at: Mutex::new(Instant::now() - Duration::from_secs(10)),
            last_hurt_tick: Mutex::new(None),
            vel: Mutex::new([0.0; 3]),
            fuse: Mutex::new(0),
            fire_ticks: Mutex::new(0),
            love: Mutex::new(0),
            air: Mutex::new(server_ticks(AIR_SUPPLY_GT) as i32),
        }));
        if !has_room_for_nightly_monster(&current_mobs, cap) {
            return NightlySpawnAttempt::AtCap;
        }
        NightlySpawnAttempt::Spawned
    })
}

fn spawn_nightly_hostile(tick: u64) {
    let clients = client_snapshot();
    let players: Vec<_> = clients
        .iter()
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
    let by = surface_y(bx, bz).floor() as i32;
    let Some(reg) = crate::block_registry::breg() else {
        return;
    };
    if by <= MIN_Y || reg.is_redstone_conductor(block_at(bx, by, bz)) {
        return;
    }
    let origin = NaturalSpawnPos {
        x: bx,
        y: by,
        z: bz,
    };
    let (sx, sy, sz, _) = *WORLD_SPAWN.lock().unwrap();
    let spawn_point_sq = |x: f64, y: f64, z: f64| {
        (sx.floor() + 0.5 - x).powi(2)
            + (sy.floor() + 0.5 - y).powi(2)
            + (sz.floor() + 0.5 - z).powi(2)
    };
    let mut sources = HashMap::new();
    let mut spawned = 0;
    for _ in 0..3 {
        let mut pack = SpawnPack::new(origin, nightly_spawn_float());
        let mut selected = None;
        while let Some(pos) = pack.next_pos(|bound| world_rng::next_below(bound as u32) as i32) {
            let (x, y, z) = (
                f64::from(pos.x) + 0.5,
                f64::from(pos.y),
                f64::from(pos.z) + 0.5,
            );
            let nearest_squared = players
                .iter()
                .map(|&(px, py, pz)| (px - x).powi(2) + (py - y).powi(2) + (pz - z).powi(2))
                .fold(f64::INFINITY, f64::min);
            let chunk = (pos.x >> 4, pos.z >> 4);
            if nearest_squared <= SPAWN_MIN_PLAYER_DIST_SQ
                || spawn_point_sq(x, y, z) < SPAWN_MIN_PLAYER_DIST_SQ
                || !spawn_chunks.contains(&chunk)
            {
                continue;
            }

            let entries = nightly_monster_spawners(pos);
            let entry = match selected {
                Some(entry) => entry,
                None => {
                    let Some(entry) = choose_nightly_spawner(entries) else {
                        break;
                    };
                    if entry.min_count < 1 || entry.max_count < entry.min_count {
                        break;
                    }
                    let span = (entry.max_count - entry.min_count + 1) as u32;
                    pack.attempts = entry.min_count + world_rng::next_below(span) as i32;
                    selected = Some(entry);
                    entry
                }
            };
            let (min_x, min_z, max_x, max_z) =
                world_border().lock().unwrap().bounds(border_now_ms());
            // Vanilla tests membership of the selected SpawnerData object in
            // the current position's list, including after crossing a biome edge.
            if !(min_x..max_x).contains(&f64::from(pos.x))
                || !(min_z..max_z).contains(&f64::from(pos.z))
                || !can_spawn_at_distance(nearest_squared, MobCategory::Monster)
                || !matches!(
                    entry.kind.as_str(),
                    "minecraft:creeper"
                        | "minecraft:skeleton"
                        | "minecraft:spider"
                        | "minecraft:zombie"
                )
                || !entries.iter().any(|current| std::ptr::eq(current, entry))
            {
                continue;
            }
            let etype = name_to_etype(&entry.kind).expect("registered nightly monster");
            // Streaming can generate chunks while holding the client cache
            // lock. Copy the light sources before taking the world-write guard.
            let light_sources = sources
                .entry(chunk)
                .or_insert_with(|| nightly_spawn_light_sources(&clients, chunk.0, chunk.1));
            match try_nightly_monster_at(pos, etype, cap, light_sources) {
                NightlySpawnAttempt::Rejected => continue,
                NightlySpawnAttempt::AtCap => return,
                NightlySpawnAttempt::Spawned => spawned += 1,
            }
            // getMaxSpawnClusterSize counts successes across all three packs.
            if spawned >= NIGHTLY_MONSTER_CLUSTER_LIMIT {
                return;
            }
        }
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
            clear_above: false,
            ..Default::default()
        }));
        assert!(!SpawnPlacementType::InWater.allows(SpawnPlacementContext {
            water_here: true,
            above_redstone_conductor: true,
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

include!("nether_spawn_rules.rs");

fn nether_spawn_cycle() -> u64 {
    static EVERY_TICK: OnceLock<bool> = OnceLock::new();
    if cfg!(debug_assertions)
        && *EVERY_TICK.get_or_init(|| {
            std::env::var("MC_SPINE_TEST_NETHER_SPAWN_EVERY_TICK").as_deref() == Ok("1")
        })
    {
        1
    } else {
        server_ticks(20)
    }
}

const NETHER_SPAWN_DIM: DimId = DimId::Nether;

struct LiveNetherSpawnQueries<'a> {
    reg: &'a BReg,
    clients: &'a [Arc<Client>],
    light: RefCell<HashMap<(i32, i32), Vec<[u8; 2048]>>>,
}

impl LiveNetherSpawnQueries<'_> {
    fn state(&self, pos: NaturalSpawnPos) -> u32 {
        if !(NETHER_SPAWN_DIM.min_y()..NETHER_SPAWN_DIM.height()).contains(&pos.y) {
            return 0;
        }
        overlay_block_dim(NETHER_SPAWN_DIM, pos.x, pos.y, pos.z)
            .or_else(|| stored_state(NETHER_SPAWN_DIM, pos.x, pos.y, pos.z))
            .unwrap_or_else(|| base_state_dim(NETHER_SPAWN_DIM, pos.x, pos.y, pos.z))
    }

    fn placement_context(
        &self,
        pos: NaturalSpawnPos,
        mob: NetherSpawnMob,
    ) -> SpawnPlacementContext {
        let clear = |state| {
            !self.reg.is_collision_shape_full_block(state)
                && !self.reg.is_redstone_conductor(state)
                && !self.reg.is_liquid(state)
                && !self
                    .reg
                    .in_tag(state, "minecraft:prevent_mob_spawning_inside")
        };
        SpawnPlacementContext {
            ground_supports_spawn: self.is_valid_spawn(nether_spawn_below(pos), mob),
            clear_here: clear(self.state(pos)),
            clear_above: clear(self.state(NaturalSpawnPos {
                y: pos.y + 1,
                ..pos
            })),
            lava_here: self.is_lava(pos),
            ..SpawnPlacementContext::default()
        }
    }
}

impl NetherSpawnQueries for LiveNetherSpawnQueries<'_> {
    fn is_peaceful(&self) -> bool {
        DIFFICULTY.load(Ordering::Relaxed) == 0
    }

    fn block_id(&self, pos: NaturalSpawnPos) -> &str {
        self.reg.name_of(self.state(pos))
    }

    fn block_light(&self, pos: NaturalSpawnPos) -> i32 {
        let chunk = (pos.x >> 4, pos.z >> 4);
        let mut cache = self.light.borrow_mut();
        let sections = cache.entry(chunk).or_insert_with(|| {
            let sources =
                natural_spawn_light_sources(self.clients, NETHER_SPAWN_DIM, chunk.0, chunk.1);
            block_light_sections_dim(NETHER_SPAWN_DIM, chunk.0, chunk.1, &sources)
        });
        nibble_at_dim(sections, NETHER_SPAWN_DIM, pos.x, pos.y, pos.z)
    }

    fn is_valid_spawn(&self, pos: NaturalSpawnPos, _mob: NetherSpawnMob) -> bool {
        let state = self.state(pos);
        // BlockBehaviour.Properties defaults to a sturdy up face and emitted light below 14.
        // Bedrock and barrier override that predicate for every Nether spawn type.
        self.reg.side_solid(state, 1)
            && self.reg.luminance_of(state) < 14
            && !matches!(
                self.reg.name_of(state),
                "minecraft:bedrock" | "minecraft:barrier"
            )
    }

    fn is_lava(&self, pos: NaturalSpawnPos) -> bool {
        self.block_id(pos) == "minecraft:lava"
    }

    fn is_air(&self, pos: NaturalSpawnPos) -> bool {
        self.reg.is_air(self.state(pos))
    }
}

fn spawn_nether_natural(tick: u64) {
    let clients = client_snapshot();
    let players: Vec<_> = clients
        .iter()
        .filter_map(|client| {
            if client.is_spectator() {
                return None;
            }
            let (dim, x, y, z, ..) = client_dim_pos(client);
            (dim == NETHER_SPAWN_DIM).then_some((client.eid as u64, [x, y, z]))
        })
        .collect();
    let Some(reg) = breg().filter(|_| !players.is_empty()) else {
        return;
    };

    let mut chunks = HashMap::<_, Vec<_>>::new();
    let mut loaded = HashSet::new();
    for client in &clients {
        let held = client.chunks.lock().unwrap();
        if *client.dim.lock().unwrap() == NETHER_SPAWN_DIM {
            loaded.extend(held.iter().copied());
        }
    }
    for &(eid, [x, _, z]) in &players {
        let (cx, cz) = (x.floor() as i32 >> 4, z.floor() as i32 >> 4);
        for dx in -NATURAL_SPAWN_CHUNK_RADIUS..=NATURAL_SPAWN_CHUNK_RADIUS {
            for dz in -NATURAL_SPAWN_CHUNK_RADIUS..=NATURAL_SPAWN_CHUNK_RADIUS {
                chunks.entry((cx + dx, cz + dz)).or_default().push(eid);
            }
        }
    }
    let mut caps = NaturalSpawnState::new(chunks.len() as u32, true);
    let mut census = Vec::new();
    {
        let current = mobs().lock().unwrap();
        if current.len() >= MOB_CAP {
            return;
        }
        for mob in current.iter().filter(|mob| mob.dim == NETHER_SPAWN_DIM) {
            let Some(spec) = entity_spec_by_id(mob.etype) else {
                continue;
            };
            let category = match spec.category {
                EntityMobCategory::Monster => MobCategory::Monster,
                EntityMobCategory::Creature => MobCategory::Creature,
                _ => continue,
            };
            if *mob.hp.lock().unwrap() <= 0.0 {
                continue;
            }
            let (x, y, z, ..) = *mob.pos.lock().unwrap();
            let pos = NaturalSpawnPos {
                x: x.floor() as i32,
                y: y.floor() as i32,
                z: z.floor() as i32,
            };
            let chunk = (pos.x >> 4, pos.z >> 4);
            caps.count_existing(
                category,
                pos,
                None,
                chunks.get(&chunk).map_or(&[], Vec::as_slice),
            );
            census.push((spec.name, pos));
        }
    }

    // Seed this pass without advancing the existing Overworld spawn RNG.
    let mut rng =
        Lcg::new(WORLD_SEED.load(Ordering::Relaxed) ^ tick.wrapping_mul(0x9E37_79B9_7F4A_7C15));
    let [px, py, pz] = players[rng.next_bound(players.len() as i32) as usize].1;
    let angle = f64::from(rng.next_f32()) * std::f64::consts::TAU;
    let x = (px + angle.cos() * 32.0).floor() as i32;
    let z = (pz + angle.sin() * 32.0).floor() as i32;
    if !loaded.contains(&(x >> 4, z >> 4)) {
        return;
    }
    let query = LiveNetherSpawnQueries {
        reg,
        clients: &clients,
        light: RefCell::default(),
    };
    // The Nether has many floors below the roof. Sample a slice near the anchor,
    // then look down for one open surface instead of using the column's top.
    let top = (py.floor() as i32 + rng.next_bound(32) - 8).clamp(1, NETHER_SPAWN_DIM.height() - 2);
    let Some(origin) = ((top - 15).max(1)..=top).rev().find_map(|y| {
        let pos = NaturalSpawnPos { x, y, z };
        let site = query.placement_context(pos, NetherSpawnMob::Skeleton);
        (SpawnPlacementType::OnGround.allows(site) || site.lava_here).then_some(pos)
    }) else {
        return;
    };
    let mut costs = SpawnCostState::from_existing(census, |pos, chunk| {
        if !loaded.contains(&chunk) {
            return None;
        }
        worldgen::with_gen_dim(NETHER_SPAWN_DIM, |gen| {
            gen.biome_name_at(pos.x, pos.y, pos.z)
        })
        .flatten()
    });
    static RULES: OnceLock<NetherSpawnRules> = OnceLock::new();
    let rules = RULES.get_or_init(NetherSpawnRules::vanilla);
    let (min_x, min_z, max_x, max_z) = world_border().lock().unwrap().bounds(border_now_ms());
    // DerivedLevelData uses the same shared spawn position in the Nether.
    let (sx, sy, sz, _) = *WORLD_SPAWN.lock().unwrap();
    let mut fortresses = HashMap::new();
    let inside = |bb: &[i32; 6], pos: NaturalSpawnPos| {
        (bb[0]..=bb[3]).contains(&pos.x)
            && (bb[1]..=bb[4]).contains(&pos.y)
            && (bb[2]..=bb[5]).contains(&pos.z)
    };
    for category in [MobCategory::Monster, MobCategory::Creature] {
        if (category == MobCategory::Monster && query.is_peaceful())
            || (category == MobCategory::Creature && !tick.is_multiple_of(server_ticks(400)))
        {
            continue;
        }
        let mut spawned = 0;
        'packs: for _ in 0..3 {
            let mut pack = SpawnPack::new(origin, rng.next_f32());
            let mut selected = None;
            while let Some(pos) = pack.next_pos(|bound| rng.next_bound(bound)) {
                let chunk = (pos.x >> 4, pos.z >> 4);
                if !loaded.contains(&chunk)
                    || !(min_x..max_x).contains(&f64::from(pos.x))
                    || !(min_z..max_z).contains(&f64::from(pos.z))
                {
                    continue;
                }
                let Some(nearby) = chunks.get(&chunk) else {
                    continue;
                };
                let (x, y, z) = (
                    f64::from(pos.x) + 0.5,
                    f64::from(pos.y),
                    f64::from(pos.z) + 0.5,
                );
                let dist = players
                    .iter()
                    .map(|(_, [px, py, pz])| (px - x).powi(2) + (py - y).powi(2) + (pz - z).powi(2))
                    .fold(f64::INFINITY, f64::min);
                let spawn_dist = (sx.floor() + 0.5 - x).powi(2)
                    + (sy.floor() + 0.5 - y).powi(2)
                    + (sz.floor() + 0.5 - z).powi(2);
                if spawn_dist < SPAWN_MIN_PLAYER_DIST_SQ
                    || !can_spawn_at_distance(dist, category)
                    || !caps.can_spawn(category, pos, None, nearby)
                {
                    continue;
                }
                let Some(biome) = worldgen::with_gen_dim(NETHER_SPAWN_DIM, |gen| {
                    let (_, leaf) = gen.biome_blended(pos.x, pos.y, pos.z)?;
                    gen.biome_name_of_leaf(leaf)
                })
                .flatten() else {
                    return;
                };
                let entries = rules.spawners_at(
                    &biome,
                    category,
                    pos,
                    |below| query.block_id(below) == "minecraft:nether_bricks",
                    |pos, check| {
                        let bounds = fortresses.entry(chunk).or_insert_with(|| {
                            worldgen::with_gen_dim(NETHER_SPAWN_DIM, |gen| {
                                feature::fortress_spawn_bounds(gen, chunk.0, chunk.1)
                            })
                            .unwrap_or_default()
                        });
                        bounds.iter().any(|(structure, pieces)| match check {
                            NetherFortressBounds::Structure => inside(structure, pos),
                            NetherFortressBounds::Piece => {
                                pieces.iter().any(|piece| inside(piece, pos))
                            }
                        })
                    },
                );
                let entry = match selected {
                    Some(entry) => entry,
                    None => {
                        let Some(entry) =
                            choose_nether_spawner(entries, |bound| rng.next_bound(bound))
                        else {
                            break;
                        };
                        pack.attempts =
                            entry.min_count + rng.next_bound(entry.max_count - entry.min_count + 1);
                        selected = Some(entry);
                        entry
                    }
                };
                let Some(mob) = NetherSpawnMob::from_id(&entry.kind) else {
                    continue;
                };
                if !entries.iter().any(|current| std::ptr::eq(current, entry))
                    || !mob.placement().0.allows(query.placement_context(pos, mob))
                    || !rules.allows(mob, pos, &query, |bound| rng.next_bound(bound))
                    || !costs.can_spawn(&entry.kind, pos, chunk)
                {
                    continue;
                }
                let spec = entity_spec_by_name(&entry.kind).expect("registered Nether spawn type");
                let bounds = EntityAabb::from_dimensions(x, y, z, spec.width, spec.height);
                let yaw = rng.next_f32() * 360.0;
                let inserted = with_save_guard(|| {
                    if !live_mob_block_boxes(NETHER_SPAWN_DIM, reg, bounds.min(), bounds.max())
                        .is_empty()
                        || nether_spawn_in_fluid(&query, bounds, mob)
                    {
                        return false;
                    }
                    let mut current = mobs().lock().unwrap();
                    if current.len() >= MOB_CAP
                        || current.iter().any(|other| {
                            if other.dim != NETHER_SPAWN_DIM {
                                return false;
                            }
                            let (x, y, z, ..) = *other.pos.lock().unwrap();
                            let (w, h) = entity_dimensions_1211(other.etype);
                            bounds.intersects(EntityAabb::from_dimensions(x, y, z, w, h))
                        })
                    {
                        return false;
                    }
                    let mob = new_live_mob(
                        spec.id,
                        NETHER_SPAWN_DIM,
                        (x, y, z, yaw, 0.0),
                        spec.max_health,
                        None,
                    );
                    if category == MobCategory::Monster {
                        push_natural_hostile(&mut current, mob);
                    } else {
                        current.push(mob);
                    }
                    true
                });
                if inserted {
                    costs.after_spawn(&entry.kind, pos, chunk);
                    caps.record_success(category, pos, None, nearby);
                    spawned += 1;
                    if spawned
                        >= if mob == NetherSpawnMob::Ghast {
                            1
                        } else {
                            NIGHTLY_MONSTER_CLUSTER_LIMIT
                        }
                    {
                        break 'packs;
                    }
                }
            }
        }
    }
}

fn nether_spawn_in_fluid(
    query: &LiveNetherSpawnQueries<'_>,
    bounds: EntityAabb,
    mob: NetherSpawnMob,
) -> bool {
    // Striders use their own obstruction check, permitting lava in the AABB.
    if mob == NetherSpawnMob::Strider {
        return false;
    }
    let [min_x, min_y, min_z] = bounds.min().map(|v| v.floor() as i32);
    let [max_x, max_y, max_z] = bounds.max().map(|v| v.ceil() as i32);
    (min_x..max_x).any(|x| {
        (min_y..max_y).any(|y| {
            (min_z..max_z).any(|z| {
                query
                    .reg
                    .is_liquid(query.state(NaturalSpawnPos { x, y, z }))
            })
        })
    })
}
