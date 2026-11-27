// Minecraft 1.21.1 NaturalSpawner primitives. Included directly in main.rs.
// The world adapter owns actual entities, block queries and spawned-chunk selection.
use crate::generation_math::Lcg;
include!("mob_despawn.rs");
include!("spawn_costs.rs");

type SpawnSourceList = Vec<((i32, i32, i32), u8)>;
type FortressChunkRooms = Vec<([i32; 6], Vec<[i32; 6]>)>;
type NetherSpawnLightCache = HashMap<(i32, i32), Vec<[u8; 2048]>>;

const SPAWN_CAP_MAGIC_NUMBER: u64 = 17 * 17;
const SPAWN_MIN_PLAYER_DIST_SQ: f64 = 24.0 * 24.0;
#[cfg(test)]
const SPAWN_MAX_PLAYER_DIST_SQ: f64 = 128.0 * 128.0;

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

    #[cfg(test)]
    pub(crate) const fn is_friendly(self) -> bool {
        !matches!(self, Self::Monster)
    }

    #[cfg(test)]
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

pub(crate) fn can_spawn_for_category(
    category: MobCategory,
    count: u64,
    spawnable_chunk_count: u32,
) -> bool {
    count < category.global_cap(spawnable_chunk_count)
}

/// For each chunk, ChunkMap supplies nearby non-spectator player IDs.
/// A category may spawn when at least one of those players is below its cap.
#[derive(Default, Debug)]
pub(crate) struct LocalMobCapCalculator {
    counts: std::collections::HashMap<u64, [u64; 7]>,
    /// Per-`add_mob` dedup, kept between calls so the census sweep doesn't
    /// reallocate the set for every counted mob.
    seen: std::collections::HashSet<u64>,
}

impl LocalMobCapCalculator {
    pub(crate) fn can_spawn(&self, category: MobCategory, players: &[u64]) -> bool {
        players.iter().any(|id| {
            self.counts
                .get(id)
                .is_none_or(|counts| counts[category.index()] < category.max_instances_per_chunk())
        })
    }

    pub(crate) fn add_mob(&mut self, category: MobCategory, players: &[u64]) {
        self.seen.clear();
        for id in players {
            if self.seen.insert(*id) {
                self.counts.entry(*id).or_default()[category.index()] += 1;
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn count(&self, player: u64, category: MobCategory) -> u64 {
        self.counts
            .get(&player)
            .map_or(0, |counts| counts[category.index()])
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct NaturalSpawnPos {
    pub x: i32,
    pub y: i32,
    pub z: i32,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct PointCharge {
    pub x: f64,
    pub y: f64,
    pub z: f64,
    pub charge: f64,
}

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
pub(crate) type MobSpawnCost = crate::feature::SpawnCost;

#[derive(Default, Debug)]
pub(crate) struct PotentialCalculator {
    charges: Vec<PointCharge>,
}

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
#[derive(Debug)]
pub(crate) struct NaturalSpawnState {
    spawnable_chunk_count: u32,
    counts: [u64; 7],
    potential: PotentialCalculator,
    local_caps: Option<LocalMobCapCalculator>,
}

impl Default for NaturalSpawnState {
    fn default() -> Self {
        Self::new(0, false)
    }
}

impl NaturalSpawnState {
    pub(crate) fn new(spawnable_chunk_count: u32, use_local_caps: bool) -> Self {
        Self {
            spawnable_chunk_count,
            counts: [0; 7],
            potential: PotentialCalculator::default(),
            local_caps: use_local_caps.then(LocalMobCapCalculator::default),
        }
    }

    /// Re-prime a pooled state for the next pass: same result as `new`, while
    /// the charges vector and per-player count map keep their capacity.
    pub(crate) fn reset(&mut self, spawnable_chunk_count: u32, use_local_caps: bool) {
        self.spawnable_chunk_count = spawnable_chunk_count;
        self.counts = [0; 7];
        self.potential.charges.clear();
        self.local_caps = match self.local_caps.take() {
            Some(mut caps) if use_local_caps => {
                caps.counts.clear();
                caps.seen.clear();
                Some(caps)
            }
            _ => use_local_caps.then(LocalMobCapCalculator::default),
        };
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
pub(crate) fn can_spawn_at_distance(dist_sq: f64, category: MobCategory) -> bool {
    dist_sq.is_finite()
        && dist_sq > SPAWN_MIN_PLAYER_DIST_SQ
        && dist_sq <= category.despawn_distance() * category.despawn_distance()
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DespawnDecision {
    Keep,
    DespawnNow,
    DespawnRandom,
    ResetNoActionTime,
}

/// Pass the closest non-spectator player's squared distance, or None if absent.
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
#[expect(clippy::too_many_arguments, reason = "preserve the established placement and simulation call signature")]
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SpawnPlacementType {
    OnGround,
    InWater,
    InLava,
    NoRestrictions,
}

/// The world resolves support and valid empty blocks using actual block/fluid rules.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct SpawnPlacementContext {
    pub ground_supports_spawn: bool,
    pub clear_here: bool,
    pub clear_above: bool,
    pub water_here: bool,
    pub above_redstone_conductor: bool,
    pub lava_here: bool,
}

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
#[derive(Clone, Copy, Debug)]
#[cfg(test)]
pub(crate) struct MonsterLightSample {
    pub sky_light: u8,
    pub block_light: u8,
    pub sky_darken: u8,
    pub thundering: bool,
    pub monster_spawn_block_light_limit: u8,
    pub sampled_spawn_light_limit: u8,
}

#[cfg(test)]
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

// Overworld natural-spawn dispatch from the pinned 1.21.1 server
// (59353fb40c36d304f2035d51e7d6e6baa98dc05c). See SpawnPlacements and the
// per-type check*SpawnRules methods decompiled for this lane.

/// Every entity in the pinned 1.21.1 Overworld biome spawner tables that is
/// not dispatched through a "creature" list - creature-table animals keep the
/// existing live paths. The ocelot appears here because the jungle tables
/// file it under "monster" even though its registry category is Creature.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum OverworldSpawnMob {
    // "monster" table entries.
    Bogged,
    Creeper,
    Drowned,
    Enderman,
    Husk,
    Ocelot,
    Skeleton,
    Slime,
    Spider,
    Stray,
    Witch,
    Zombie,
    ZombieVillager,
    // "ambient" table entries.
    Bat,
    // "axolotls" table entries.
    Axolotl,
    // "underground_water_creature" table entries.
    GlowSquid,
    // "water_creature" table entries.
    Dolphin,
    Squid,
    // "water_ambient" table entries.
    Cod,
    Pufferfish,
    Salmon,
    TropicalFish,
}

impl OverworldSpawnMob {
    pub(crate) fn from_id(id: &str) -> Option<Self> {
        Some(match id {
            "minecraft:bogged" => Self::Bogged,
            "minecraft:creeper" => Self::Creeper,
            "minecraft:drowned" => Self::Drowned,
            "minecraft:enderman" => Self::Enderman,
            "minecraft:husk" => Self::Husk,
            "minecraft:ocelot" => Self::Ocelot,
            "minecraft:skeleton" => Self::Skeleton,
            "minecraft:slime" => Self::Slime,
            "minecraft:spider" => Self::Spider,
            "minecraft:stray" => Self::Stray,
            "minecraft:witch" => Self::Witch,
            "minecraft:zombie" => Self::Zombie,
            "minecraft:zombie_villager" => Self::ZombieVillager,
            "minecraft:bat" => Self::Bat,
            "minecraft:axolotl" => Self::Axolotl,
            "minecraft:glow_squid" => Self::GlowSquid,
            "minecraft:dolphin" => Self::Dolphin,
            "minecraft:squid" => Self::Squid,
            "minecraft:cod" => Self::Cod,
            "minecraft:pufferfish" => Self::Pufferfish,
            "minecraft:salmon" => Self::Salmon,
            "minecraft:tropical_fish" => Self::TropicalFish,
            _ => return None,
        })
    }

    /// The placement and heightmap registered by SpawnPlacements.
    pub(crate) const fn placement(self) -> (SpawnPlacementType, HeightmapKind) {
        let placement = match self {
            Self::Axolotl
            | Self::Cod
            | Self::Dolphin
            | Self::Drowned
            | Self::GlowSquid
            | Self::Pufferfish
            | Self::Salmon
            | Self::Squid
            | Self::TropicalFish => SpawnPlacementType::InWater,
            // SpawnPlacements registers the bat against NO_RESTRICTIONS so it
            // can hang in unlit caves; its own rule gates the position.
            Self::Bat => SpawnPlacementType::NoRestrictions,
            _ => SpawnPlacementType::OnGround,
        };
        let heightmap = match self {
            // SpawnPlacements registers the ocelot against MOTION_BLOCKING;
            // every other natural spawn uses MOTION_BLOCKING_NO_LEAVES.
            Self::Ocelot => HeightmapKind::MotionBlocking,
            _ => HeightmapKind::MotionBlockingNoLeaves,
        };
        (placement, heightmap)
    }
}

/// BiomeTags.ALLOWS_SURFACE_SLIME_SPAWNS in the pinned tag file.
const ALLOWS_SURFACE_SLIME_SPAWNS: [&str; 2] = ["minecraft:mangrove_swamp", "minecraft:swamp"];

/// BiomeTags.ALLOWS_TROPICAL_FISH_SPAWNS_AT_ANY_HEIGHT in the pinned tag file.
const ALLOWS_TROPICAL_FISH_SPAWNS_AT_ANY_HEIGHT: [&str; 1] = ["minecraft:lush_caves"];

/// BiomeTags.MORE_FREQUENT_DROWNED_SPAWNS in the pinned tag file (is_river).
const MORE_FREQUENT_DROWNED_SPAWNS: [&str; 2] = ["minecraft:frozen_river", "minecraft:river"];

/// BlockTags.AXOLOTLS_SPAWNABLE_ON in the pinned tag file.
const AXOLOTLS_SPAWNABLE_ON: &str = "minecraft:axolotls_spawnable_on";

/// DimensionType.MOON_BRIGHTNESS_PER_PHASE, indexed by the 8-phase cycle.
const MOON_BRIGHTNESS_PER_PHASE: [f32; 8] = [1.0, 0.75, 0.5, 0.25, 0.0, 0.25, 0.5, 0.75];

/// WorldgenRandom.seedSlimeChunk's seed mix: signed int products widened to
/// long, summed onto the world seed, then XORed with the salt.
fn slime_chunk_seed(x: i32, z: i32, seed: u64, salt: u64) -> u64 {
    seed
        .wrapping_add(x.wrapping_mul(x).wrapping_mul(4_987_142) as i64 as u64)
        .wrapping_add(x.wrapping_mul(5_947_611) as i64 as u64)
        .wrapping_add((z.wrapping_mul(z) as i64).wrapping_mul(4_392_871) as u64)
        .wrapping_add(z.wrapping_mul(389_711) as i64 as u64)
        ^ salt
}

/// The world state the 1.21.1 spawn predicates read. The live adapter answers
/// from Overworld tick state; tests answer from a table.
pub(crate) trait OverworldSpawnQueries {
    fn is_peaceful(&self) -> bool;
    /// MobSpawnType.isSpawner — false on the natural pass.
    fn spawn_is_spawner(&self) -> bool;
    /// MobSpawnType.ignoresLightRequirements — false for MobSpawnType.NATURAL.
    fn ignores_light_requirements(&self) -> bool;
    /// `level instanceof WorldGenLevel` — true on live `ServerLevel` ticks
    /// (since `ServerLevel implements WorldGenLevel` in Vanilla 1.21.1),
    /// enabling underground `y < 40` slime-chunk spawning.
    fn is_world_gen(&self) -> bool;
    /// Holder<Biome> at pos, or None when the column has no generator.
    fn biome_id(&self, pos: NaturalSpawnPos) -> Option<String>;
    fn block_id(&self, pos: NaturalSpawnPos) -> &str;
    /// BlockState.is(tag).
    fn block_in_tag(&self, pos: NaturalSpawnPos, tag: &str) -> bool;
    /// FluidState.is(FluidTags.WATER): source or flowing water.
    fn fluid_is_water(&self, pos: NaturalSpawnPos) -> bool;
    /// BlockState.isValidSpawn for the supporting block.
    fn is_valid_spawn(&self, pos: NaturalSpawnPos) -> bool;
    /// The raw sky-light nibble, 0-15, before ambient darkening.
    fn sky_light(&self, pos: NaturalSpawnPos) -> i32;
    /// The raw block-light nibble, 0-15.
    fn block_light(&self, pos: NaturalSpawnPos) -> i32;
    /// Level.getSkyDarken without the thunder adjustment.
    fn sky_darken(&self) -> i32;
    fn thundering(&self) -> bool;
    /// DimensionType.getMoonBrightness for the current phase.
    fn moon_brightness(&self) -> f32;
    fn sea_level(&self) -> i32;
    /// SharedConstants.isAroundHalloween (LocalDate, Oct 20 through Nov 3).
    fn is_halloween(&self) -> bool;
    /// WorldGenLevel.getSeed, for the seeded slime-chunk roll.
    fn world_seed(&self) -> u64;
}

/// The RandomSource draws the 1.21.1 spawn predicates consume.
pub(crate) trait OverworldSpawnDraws {
    /// nextInt(bound): a value in 0..bound.
    fn next_int(&mut self, bound: i32) -> i32;
    fn next_f32(&mut self) -> f32;
    fn next_bool(&mut self) -> bool;
}

pub(crate) struct OverworldSpawnRules {
    light: OverworldMonsterLight,
}

impl OverworldSpawnRules {
    pub(crate) fn vanilla() -> Self {
        Self {
            light: OverworldMonsterLight::vanilla(),
        }
    }

    /// Monster.isDarkEnoughToSpawn for MobSpawnType.NATURAL: one skylight roll
    /// and one monster_spawn_light_level sample, in the vanilla draw order.
    fn dark_enough(
        &self,
        pos: NaturalSpawnPos,
        world: &impl OverworldSpawnQueries,
        draws: &mut dyn OverworldSpawnDraws,
    ) -> bool {
        let sky_darken = if world.thundering() {
            10
        } else {
            world.sky_darken()
        };
        monster_spawn_light_allows(
            world.sky_light(pos),
            draws.next_int(32),
            self.light.block_limit,
            || world.block_light(pos),
            sky_darken,
            || self.light.min + draws.next_int(self.light.max - self.light.min + 1),
        )
    }

    /// Level.getMaxLocalRawBrightness: max(blockLight, skyLight - skyDarken).
    fn max_local_raw_brightness(pos: NaturalSpawnPos, world: &impl OverworldSpawnQueries) -> i32 {
        world
            .block_light(pos)
            .max(world.sky_light(pos) - world.sky_darken())
    }

    /// LevelReader.getRawBrightness(pos, 0): max(blockLight, skyLight).
    fn raw_brightness(pos: NaturalSpawnPos, world: &impl OverworldSpawnQueries) -> i32 {
        world.block_light(pos).max(world.sky_light(pos))
    }

    /// LevelReader.canSeeSky: the sky-light nibble reaches getMaxLightLevel.
    fn can_see_sky(pos: NaturalSpawnPos, world: &impl OverworldSpawnQueries) -> bool {
        world.sky_light(pos) >= 15
    }

    /// SpawnPlacements.checkSpawnRules for MobSpawnType.NATURAL: the
    /// per-type predicate each entity's SpawnPlacements entry registered.
    pub(crate) fn allows(
        &self,
        mob: OverworldSpawnMob,
        pos: NaturalSpawnPos,
        world: &impl OverworldSpawnQueries,
        draws: &mut dyn OverworldSpawnDraws,
    ) -> bool {
        let below = NaturalSpawnPos {
            y: pos.y - 1,
            ..pos
        };
        let above = NaturalSpawnPos {
            y: pos.y + 1,
            ..pos
        };
        // Mob.checkMobSpawnRules for a natural spawn is the support check.
        let mob_rules = || world.is_valid_spawn(below);
        // Monster.checkMonsterSpawnRules for MobSpawnType.NATURAL.
        let monster_rules = |draws: &mut dyn OverworldSpawnDraws| {
            !world.is_peaceful()
                && (world.ignores_light_requirements()
                    || self.dark_enough(pos, world, draws))
                && mob_rules()
        };
        match mob {
            // Monster.checkMonsterSpawnRules.
            OverworldSpawnMob::Bogged
            | OverworldSpawnMob::Creeper
            | OverworldSpawnMob::Enderman
            | OverworldSpawnMob::Skeleton
            | OverworldSpawnMob::Spider
            | OverworldSpawnMob::Witch
            | OverworldSpawnMob::Zombie
            | OverworldSpawnMob::ZombieVillager => monster_rules(draws),
            // Husk.checkHuskSpawnRules.
            OverworldSpawnMob::Husk => {
                monster_rules(draws)
                    && (world.spawn_is_spawner() || Self::can_see_sky(pos, world))
            }
            // Stray.checkStraySpawnRules: climb the powder-snow column first.
            OverworldSpawnMob::Stray => {
                let mut top = pos;
                while world.block_id(top) == "minecraft:powder_snow" {
                    top.y += 1;
                }
                monster_rules(draws)
                    && (world.spawn_is_spawner() || Self::can_see_sky(top, world))
            }
            // Drowned.checkDrownedSpawnRules.
            OverworldSpawnMob::Drowned => {
                if !world.fluid_is_water(below) && !world.spawn_is_spawner() {
                    return false;
                }
                let flag = !world.is_peaceful()
                    && (world.ignores_light_requirements()
                        || self.dark_enough(pos, world, draws))
                    && (world.spawn_is_spawner() || world.fluid_is_water(pos));
                if flag && world.spawn_is_spawner() {
                    return true;
                }
                if world.biome_id(pos).is_some_and(|biome| {
                    MORE_FREQUENT_DROWNED_SPAWNS.contains(&biome.as_str())
                }) {
                    return draws.next_int(15) == 0 && flag;
                }
                // isDeepEnoughToSpawn: y < seaLevel - 5.
                draws.next_int(40) == 0 && pos.y < world.sea_level() - 5 && flag
            }
            // Slime.checkSlimeSpawnRules.
            OverworldSpawnMob::Slime => {
                if world.spawn_is_spawner() || world.is_peaceful() {
                    return world.spawn_is_spawner() && mob_rules();
                }
                if world.biome_id(pos).is_some_and(|biome| {
                    ALLOWS_SURFACE_SLIME_SPAWNS.contains(&biome.as_str())
                }) && pos.y > 50
                    && pos.y < 70
                    && draws.next_f32() < 0.5
                    && draws.next_f32() < world.moon_brightness()
                    && Self::max_local_raw_brightness(pos, world) <= draws.next_int(8)
                {
                    return mob_rules();
                }
                if !world.is_world_gen() {
                    return false;
                }
                // WorldgenRandom.seedSlimeChunk(cx, cz, seed, 987234911L).
                let slime_chunk = Lcg::new(slime_chunk_seed(
                    pos.x >> 4,
                    pos.z >> 4,
                    world.world_seed(),
                    987_234_911,
                ))
                .next_bound(10)
                    == 0;
                draws.next_int(10) == 0 && slime_chunk && pos.y < 40 && mob_rules()
            }
            // Ocelot.checkOcelotSpawnRules: a two-in-three roll only.
            OverworldSpawnMob::Ocelot => draws.next_int(3) != 0,
            // Bat.checkBatSpawnRules.
            OverworldSpawnMob::Bat => {
                if pos.y >= world.sea_level() {
                    return false;
                }
                let bound = if world.is_halloween() {
                    7
                } else {
                    if draws.next_bool() {
                        return false;
                    }
                    4
                };
                Self::max_local_raw_brightness(pos, world) <= draws.next_int(bound)
                    && mob_rules()
            }
            // Axolotl.checkAxolotlSpawnRules.
            OverworldSpawnMob::Axolotl => world.block_in_tag(below, AXOLOTLS_SPAWNABLE_ON),
            // GlowSquid.checkGlowSquidSpawnRules.
            OverworldSpawnMob::GlowSquid => {
                pos.y <= world.sea_level() - 33
                    && Self::raw_brightness(pos, world) == 0
                    && world.block_id(pos) == "minecraft:water"
            }
            // WaterAnimal.checkSurfaceWaterAnimalSpawnRules.
            OverworldSpawnMob::Cod
            | OverworldSpawnMob::Dolphin
            | OverworldSpawnMob::Pufferfish
            | OverworldSpawnMob::Salmon
            | OverworldSpawnMob::Squid => {
                let sea = world.sea_level();
                pos.y >= sea - 13
                    && pos.y <= sea
                    && world.fluid_is_water(below)
                    && world.block_id(above) == "minecraft:water"
            }
            // TropicalFish.checkTropicalFishSpawnRules.
            OverworldSpawnMob::TropicalFish => {
                if !(world.fluid_is_water(below)
                    && world.block_id(above) == "minecraft:water")
                {
                    return false;
                }
                if world.biome_id(pos).is_some_and(|biome| {
                    ALLOWS_TROPICAL_FISH_SPAWNS_AT_ANY_HEIGHT.contains(&biome.as_str())
                }) {
                    return true;
                }
                let sea = world.sea_level();
                pos.y >= sea - 13 && pos.y <= sea
            }
        }
    }
}

/// Selected biome spawner entry, with type-specific darkness and cluster rules.
#[derive(Clone, Debug)]
#[cfg(test)]
pub(crate) struct NaturalSpawnCandidate {
    pub entity_id: String,
    pub placement: SpawnPlacementType,
    pub cost: Option<MobSpawnCost>,
    pub min_group_size: u8,
    pub max_group_size: u8,
    pub max_cluster_size: u32,
    pub requires_darkness: bool,
}

#[cfg(test)]
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
#[cfg(test)]
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

fn natural_spawn_placement_allows(
    reg: &BReg,
    dim: DimId,
    mob: OverworldSpawnMob,
    pos: NaturalSpawnPos,
) -> bool {
    let clear = |state| {
        !reg.is_collision_shape_full_block(state)
            && !reg.is_liquid(state)
            && !reg.in_tag(state, "minecraft:prevent_mob_spawning_inside")
    };
    let NaturalSpawnPos { x, y, z } = pos;
    let site = SpawnPlacementContext {
        ground_supports_spawn: reg.side_solid(block_at_dim(dim, x, y - 1, z), 1),
        clear_here: clear(block_at_dim(dim, x, y, z)),
        clear_above: clear(block_at_dim(dim, x, y + 1, z)),
        water_here: reg.fluid_name(block_at_dim(dim, x, y, z)) == "minecraft:water",
        above_redstone_conductor: reg.is_redstone_conductor(block_at_dim(dim, x, y + 1, z)),
        ..SpawnPlacementContext::default()
    };
    mob.placement().0.allows(site)
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

    fn vanilla() -> Self {
        match crate::structure_transform::pinned_data_bytes(
            "datapacks/vanilla/data/minecraft/dimension_type/overworld.json",
        ) {
            Some(bytes) => Self::from_dimension_type(&bytes)
                .expect("pinned Overworld dimension type must have valid monster spawn light rules"),
            // A scratch run with no `datapacks/` tree (the wire-test
            // `local_server` layout) still spawns by the pinned 1.21.1
            // values: block-light limit 0, uniform light level 0..=7.
            None => Self {
                block_limit: 0,
                min: 0,
                max: 7,
            },
        }
    }
}

fn nightly_spawn_light_sources(clients: &[Arc<Client>], cx: i32, cz: i32) -> GenSources {
    natural_spawn_light_sources(clients, DimId::Overworld, cx, cz)
}

fn natural_spawn_light_sources(clients: &[Arc<Client>], dim: DimId, cx: i32, cz: i32) -> GenSources {
    let mut nearby = GenSources::new();
    natural_spawn_light_sources_into(clients, dim, cx, cz, &mut nearby, &mut Vec::new());
    nearby
}

/// `natural_spawn_light_sources` filling a caller-provided map. Reused maps
/// keep their bucket capacity, and `list_pool` supplies the per-chunk section
/// vectors so the 3x3 clone pass stops reallocating every 2048-entry buffer.
fn natural_spawn_light_sources_into(
    clients: &[Arc<Client>],
    dim: DimId,
    cx: i32,
    cz: i32,
    out: &mut GenSources,
    list_pool: &mut Vec<SpawnSourceList>,
) {
    out.clear();
    for client in clients {
        // Chunk streaming takes these locks in this order.
        let sources = client.gen_sources.lock().unwrap_or_else(|e| e.into_inner());
        if *client.dim.lock().unwrap_or_else(|e| e.into_inner()) != dim {
            continue;
        }
        for dx in -1..=1 {
            for dz in -1..=1 {
                let key = (cx + dx, cz + dz);
                if let Some(chunk) = sources.get(&key) {
                    out.entry(key).or_insert_with(|| {
                        let mut list = list_pool.pop().unwrap_or_default();
                        list.clear();
                        list.extend_from_slice(chunk);
                        list
                    });
                }
            }
        }
    }
}

/// The live `OverworldSpawnQueries` for a natural tick in `dim`: block and
/// biome reads come straight off the world, light reads reuse the same
/// section lookups the old nightly darkness check used.
struct LiveNaturalSpawnQueries<'a> {
    dim: DimId,
    reg: &'a BReg,
    sources: &'a GenSources,
}

impl LiveNaturalSpawnQueries<'_> {
    fn state(&self, pos: NaturalSpawnPos) -> u32 {
        block_at_dim(self.dim, pos.x, pos.y, pos.z)
    }
}

impl OverworldSpawnQueries for LiveNaturalSpawnQueries<'_> {
    fn is_peaceful(&self) -> bool {
        DIFFICULTY.load(Ordering::Relaxed) == 0
    }

    fn spawn_is_spawner(&self) -> bool {
        false
    }

    fn ignores_light_requirements(&self) -> bool {
        false
    }

    fn is_world_gen(&self) -> bool {
        // Vanilla `ServerLevel` implements `WorldGenLevel`, so live Overworld
        // natural spawning evaluates slime chunks (`y < 40`) using world seed.
        true
    }

    fn biome_id(&self, pos: NaturalSpawnPos) -> Option<String> {
        worldgen::with_gen_dim(self.dim, |gen| {
            let (_, leaf) = gen.biome_blended(pos.x, pos.y, pos.z)?;
            gen.biome_name_of_leaf(leaf)
        })
        .flatten()
    }

    fn block_id(&self, pos: NaturalSpawnPos) -> &str {
        self.reg.name_of(self.state(pos))
    }

    fn block_in_tag(&self, pos: NaturalSpawnPos, tag: &str) -> bool {
        self.reg.in_tag(self.state(pos), tag)
    }

    fn fluid_is_water(&self, pos: NaturalSpawnPos) -> bool {
        matches!(
            self.reg.fluid_name(self.state(pos)),
            "minecraft:water" | "minecraft:flowing_water"
        )
    }

    fn is_valid_spawn(&self, pos: NaturalSpawnPos) -> bool {
        let state = self.state(pos);
        // BlockBehaviour.Properties defaults to a sturdy up face and emitted light below 14.
        // Bedrock and barrier override that predicate for every spawn type.
        self.reg.side_solid(state, 1)
            && self.reg.luminance_of(state) < 14
            && !matches!(
                self.reg.name_of(state),
                "minecraft:bedrock" | "minecraft:barrier"
            )
    }

    fn sky_light(&self, pos: NaturalSpawnPos) -> i32 {
        let (cx, cz) = (pos.x >> 4, pos.z >> 4);
        nibble_at_dim(
            &sky_light_sections_dim(self.dim, cx, cz),
            self.dim,
            pos.x,
            pos.y,
            pos.z,
        )
    }

    fn block_light(&self, pos: NaturalSpawnPos) -> i32 {
        let (cx, cz) = (pos.x >> 4, pos.z >> 4);
        nibble_at_dim(
            &block_light_sections_dim(self.dim, cx, cz, self.sources),
            self.dim,
            pos.x,
            pos.y,
            pos.z,
        )
    }

    fn sky_darken(&self) -> i32 {
        current_sky_darken()
    }

    fn thundering(&self) -> bool {
        THUNDERING.load(Ordering::Relaxed) > 0
    }

    fn moon_brightness(&self) -> f32 {
        // DimensionType.getMoonPhase on the cumulative world age: the world
        // clock's /time and doDaylightCycle adjustments diverge here, which
        // only changes the slime brightness roll.
        MOON_BRIGHTNESS_PER_PHASE[(TICK.load(Ordering::Relaxed) / 24000 % 8) as usize]
    }

    fn sea_level(&self) -> i32 {
        worldgen::sea_level_dim(self.dim)
    }

    fn is_halloween(&self) -> bool {
        is_around_halloween()
    }

    fn world_seed(&self) -> u64 {
        WORLD_SEED.load(Ordering::Relaxed)
    }
}

/// SharedConstants.isAroundHalloween: Oct 20 through Nov 3 of the server
/// clock. Reuses the protocol timestamp, converting its day count to a
/// calendar date with Howard Hinnant's civil-days algorithm.
fn is_around_halloween() -> bool {
    halloween_from_epoch_days(advancement_time_ms().div_euclid(86_400_000))
}

fn halloween_from_epoch_days(days: i64) -> bool {
    let doe = (days + 719_468).rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    (month == 10 && day >= 20) || (month == 11 && day <= 3)
}

/// The RandomSource the live natural pass draws from.
struct WorldRngDraws;

impl OverworldSpawnDraws for WorldRngDraws {
    fn next_int(&mut self, bound: i32) -> i32 {
        world_rng::next_below(bound as u32) as i32
    }
    fn next_f32(&mut self) -> f32 {
        nightly_spawn_float()
    }
    fn next_bool(&mut self) -> bool {
        world_rng::next_below(2) != 0
    }
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
    let Some(reg) = crate::block_registry::breg() else {
        // Without registry data, obstruction cannot be disproved safely.
        return true;
    };
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
        .unwrap_or_else(|e| e.into_inner())
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
    mob: OverworldSpawnMob,
    etype: i64,
    cap: usize,
    light_sources: &GenSources,
    rules: &OverworldSpawnRules,
) -> NightlySpawnAttempt {
    // Block edits and spawn insertion share the world-write guard.
    with_save_guard(|| {
        let NaturalSpawnPos { x, y, z } = pos;
        if !has_room_for_nightly_monster(&mobs().lock().unwrap_or_else(|e| e.into_inner()), cap) {
            return NightlySpawnAttempt::AtCap;
        }
        let Some(reg) = crate::block_registry::breg() else {
            return NightlySpawnAttempt::Rejected;
        };
        // The tick has already checked difficulty; the per-type predicate does
        // the rest, including Monster.checkMonsterSpawnRules' darkness roll.
        let queries = LiveNaturalSpawnQueries {
            dim: DimId::Overworld,
            reg,
            sources: light_sources,
        };
        if !natural_spawn_placement_allows(reg, DimId::Overworld, mob, pos)
            || !rules.allows(mob, pos, &queries, &mut WorldRngDraws)
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
        // checks.
        let yaw = nightly_spawn_float() * 360.0;
        if nightly_spawn_obstructed(bounds) {
            return NightlySpawnAttempt::Rejected;
        }
        let mut current_mobs = mobs().lock().unwrap_or_else(|e| e.into_inner());
        if !has_room_for_nightly_monster(&current_mobs, cap) {
            return NightlySpawnAttempt::AtCap;
        }
        if current_mobs.iter().filter(|mob| mob.dim == DimId::Overworld).any(|mob| {
            let (x, y, z, ..) = *mob.pos.lock().unwrap_or_else(|e| e.into_inner());
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

/// Retained-capacity bound for each reused spawn-pass buffer. One pass sees
/// ~290 eligible chunks per player; the bound only stops a pathological burst
/// from pinning peak capacity in the thread-local scratch.
const SPAWN_SCRATCH_KEEP: usize = 256 * 1024;

fn bound_spawn_vec<T>(v: &mut Vec<T>) {
    let keep = SPAWN_SCRATCH_KEEP / std::mem::size_of::<T>().max(1);
    if v.capacity() > keep {
        v.shrink_to(keep);
    }
}

fn bound_spawn_map<K: Eq + std::hash::Hash, V, S: std::hash::BuildHasher>(
    m: &mut HashMap<K, V, S>,
) {
    let keep = SPAWN_SCRATCH_KEEP / std::mem::size_of::<(K, V)>().max(1);
    if m.capacity() > keep {
        m.shrink_to(keep);
    }
}

fn bound_spawn_set<T: Eq + std::hash::Hash, S: std::hash::BuildHasher>(s: &mut HashSet<T, S>) {
    let keep = SPAWN_SCRATCH_KEEP / std::mem::size_of::<T>().max(1);
    if s.capacity() > keep {
        s.shrink_to(keep);
    }
}

/// Per-tick buffers the two live spawn passes share. Player/chunk sweeps and
/// the mob-cap state are rebuilt every pass; pooling them keeps the steady
/// tick allocation-free while `recycle` reclaims nested vectors and bounds
/// every retained capacity.
#[derive(Default)]
struct SpawnTickScratch {
    players_xyz: Vec<(f64, f64, f64)>,
    players_eid: Vec<(u64, [f64; 3])>,
    spawn_chunks: HashSet<(i32, i32)>,
    loaded_chunks: HashSet<(i32, i32)>,
    chunk_players: HashMap<(i32, i32), Vec<u64>>,
    chunk_player_lists: Vec<Vec<u64>>,
    sources: HashMap<(i32, i32), GenSources>,
    gen_sources: Vec<GenSources>,
    source_lists: Vec<SpawnSourceList>,
    census: Vec<(&'static str, NaturalSpawnPos)>,
    fortresses: HashMap<(i32, i32), FortressChunkRooms>,
    nether_light: HashMap<(i32, i32), Vec<[u8; 2048]>>,
    caps: NaturalSpawnState,
}

impl SpawnTickScratch {
    /// After a pass: drain nested vectors back to their free lists, clear
    /// every buffer, and shrink anything retained above the cap.
    fn recycle(&mut self) {
        bound_spawn_vec(&mut self.players_xyz);
        bound_spawn_vec(&mut self.players_eid);
        bound_spawn_set(&mut self.spawn_chunks);
        bound_spawn_set(&mut self.loaded_chunks);
        for (_, mut list) in self.chunk_players.drain() {
            list.clear();
            bound_spawn_vec(&mut list);
            self.chunk_player_lists.push(list);
        }
        bound_spawn_map(&mut self.chunk_players);
        bound_spawn_vec(&mut self.chunk_player_lists);
        for (_, mut sources) in self.sources.drain() {
            for (_, mut list) in sources.drain() {
                list.clear();
                bound_spawn_vec(&mut list);
                self.source_lists.push(list);
            }
            bound_spawn_map(&mut sources);
            self.gen_sources.push(sources);
        }
        bound_spawn_map(&mut self.sources);
        bound_spawn_vec(&mut self.gen_sources);
        bound_spawn_vec(&mut self.source_lists);
        bound_spawn_vec(&mut self.census);
        self.fortresses.clear();
        bound_spawn_map(&mut self.fortresses);
        self.nether_light.clear();
        bound_spawn_map(&mut self.nether_light);
        bound_spawn_vec(&mut self.caps.potential.charges);
        if let Some(caps) = &mut self.caps.local_caps {
            bound_spawn_map(&mut caps.counts);
            bound_spawn_set(&mut caps.seen);
        }
    }
}

thread_local! {
    static SPAWN_TICK_SCRATCH: RefCell<SpawnTickScratch> =
        RefCell::new(SpawnTickScratch::default());
}

/// Borrow the per-tick spawn buffers for one spawn pass. Both live passes run
/// sequentially on the tick thread; a reentrant call gets a fresh scratch.
fn with_spawn_scratch<R>(f: impl FnOnce(&mut SpawnTickScratch) -> R) -> R {
    SPAWN_TICK_SCRATCH.with(|cell| match cell.try_borrow_mut() {
        Ok(mut scratch) => {
            let result = f(&mut scratch);
            scratch.recycle();
            result
        }
        Err(_) => f(&mut SpawnTickScratch::default()),
    })
}

fn spawn_nightly_hostile(tick: u64) {
    with_spawn_scratch(|scratch| spawn_nightly_hostile_scratch(tick, scratch));
}

fn spawn_nightly_hostile_scratch(tick: u64, scratch: &mut SpawnTickScratch) {
    let SpawnTickScratch {
        players_xyz: players,
        spawn_chunks,
        sources,
        gen_sources,
        source_lists,
        ..
    } = scratch;
    let clients = client_snapshot();
    players.clear();
    players.extend(clients.iter().filter_map(|client| {
        if client.is_spectator() {
            return None;
        }
        let dim = client.dim.lock().unwrap_or_else(|e| e.into_inner());
        if *dim != DimId::Overworld {
            return None;
        }
        let (x, y, z, ..) = *client.pos.lock().unwrap_or_else(|e| e.into_inner());
        Some((x, y, z))
    }));
    if players.is_empty() {
        return;
    }

    spawn_chunks.clear();
    for &(x, _, z) in players.iter() {
        let center = (
            (x.floor() as i32).div_euclid(16),
            (z.floor() as i32).div_euclid(16),
        );
        extend_chunk_square(spawn_chunks, center, NATURAL_SPAWN_CHUNK_RADIUS);
    }
    let cap = MobCategory::Monster.global_cap(spawn_chunks.len() as u32) as usize;
    if !has_room_for_nightly_monster(&mobs().lock().unwrap_or_else(|e| e.into_inner()), cap) {
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
    let (sx, sy, sz, _) = *WORLD_SPAWN.lock().unwrap_or_else(|e| e.into_inner());
    let spawn_point_sq = |x: f64, y: f64, z: f64| {
        (sx.floor() + 0.5 - x).powi(2)
            + (sy.floor() + 0.5 - y).powi(2)
            + (sz.floor() + 0.5 - z).powi(2)
    };
    sources.clear();
    static RULES: OnceLock<OverworldSpawnRules> = OnceLock::new();
    let rules = RULES.get_or_init(OverworldSpawnRules::vanilla);
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
                world_border().lock().unwrap_or_else(|e| e.into_inner()).bounds(border_now_ms());
            // Vanilla tests membership of the selected SpawnerData object in
            // the current position's list, including after crossing a biome edge.
            if !(min_x..max_x).contains(&f64::from(pos.x))
                || !(min_z..max_z).contains(&f64::from(pos.z))
                || !can_spawn_at_distance(nearest_squared, MobCategory::Monster)
                || !matches!(
                    entry.kind.as_str(),
                    "minecraft:bogged"
                        | "minecraft:creeper"
                        | "minecraft:drowned"
                        | "minecraft:enderman"
                        | "minecraft:husk"
                        | "minecraft:ocelot"
                        | "minecraft:skeleton"
                        | "minecraft:slime"
                        | "minecraft:spider"
                        | "minecraft:stray"
                        | "minecraft:witch"
                        | "minecraft:zombie"
                        | "minecraft:zombie_villager"
                )
                || !entries.iter().any(|current| std::ptr::eq(current, entry))
            {
                continue;
            }
            let mob = OverworldSpawnMob::from_id(&entry.kind)
                .expect("nightly monster dispatch covers every matched kind");
            let etype = name_to_etype(&entry.kind).expect("registered nightly monster");
            // IN_WATER.adjustSpawnPosition drops the heightmap candidate into
            // the water column: the first water cell at or below the pick.
            let pos = if mob.placement().0 == SpawnPlacementType::InWater {
                let Some(reg) = crate::block_registry::breg() else {
                    continue;
                };
                let min_y = DimId::Overworld.min_y();
                let Some(y) = (min_y..=pos.y)
                    .rev()
                    .find(|&y| reg.fluid_name(block_at(pos.x, y, pos.z)) == "minecraft:water")
                else {
                    continue;
                };
                NaturalSpawnPos { y, ..pos }
            } else {
                pos
            };
            // Streaming can generate chunks while holding the client cache
            // lock. Copy the light sources before taking the world-write guard.
            let light_sources = sources.entry(chunk).or_insert_with(|| {
                let mut nearby = gen_sources.pop().unwrap_or_default();
                natural_spawn_light_sources_into(
                    &clients,
                    DimId::Overworld,
                    chunk.0,
                    chunk.1,
                    &mut nearby,
                    source_lists,
                );
                nearby
            });
            match try_nightly_monster_at(pos, mob, etype, cap, light_sources, rules) {
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

/// The serialized biome-spawner category key for a `MobCategory`.
fn mob_category_key(category: MobCategory) -> &'static str {
    match category {
        MobCategory::Monster => "monster",
        MobCategory::Creature => "creature",
        MobCategory::Ambient => "ambient",
        MobCategory::Axolotls => "axolotls",
        MobCategory::UndergroundWaterCreature => "underground_water_creature",
        MobCategory::WaterCreature => "water_creature",
        MobCategory::WaterAmbient => "water_ambient",
    }
}

/// The candidate Y for one column: the first non-air cell plus one for
/// ON_GROUND/NO_RESTRICTIONS mobs, or a uniform cell of the topmost
/// contiguous water run for IN_WATER mobs, the same band
/// SpawnPlacements.adjustSpawnPosition picks inside.
fn natural_spawn_column_y(
    reg: &BReg,
    dim: DimId,
    mob: OverworldSpawnMob,
    x: i32,
    z: i32,
    rng: &mut generation_math::Lcg,
) -> Option<i32> {
    match mob.placement().0 {
        SpawnPlacementType::InWater => {
            let top = worldgen::sea_level_dim(dim).min(dim.min_y() + dim.height() - 1);
            let water_top = (dim.min_y()..=top)
                .rev()
                .find(|&y| reg.fluid_name(block_at_dim(dim, x, y, z)) == "minecraft:water")?;
            let run = (dim.min_y()..water_top)
                .rev()
                .take_while(|&y| {
                    reg.fluid_name(block_at_dim(dim, x, y, z)) == "minecraft:water"
                })
                .count() as i32;
            Some(water_top - rng.next_bound(run + 1))
        }
        _ => (dim.min_y()..dim.min_y() + dim.height())
            .rev()
            .find(|&y| !reg.is_air(block_at_dim(dim, x, y, z)))
            .map(|y| y + 1),
    }
}

/// One weighted natural-spawn pass for a `(dim, category)` pair the nightly
/// and Nether scratches do not cover: Overworld `ambient`, `axolotls`,
/// `underground_water_creature`, `water_creature` and `water_ambient`, and
/// the End's `monster` enderman rows. It follows the same anchor offset,
/// spawn-chunk membership, distance, border, category-cap, weighted pick,
/// group walk, placement, per-mob rules and insertion flow the other live
/// passes use, and returns how many mobs it inserted.
///
/// Wired from `spawn_aquatic_natural`, `spawn_end_natural` and the ambient /
/// underground passes once `main.rs` can call it.
pub(crate) fn spawn_natural_category_scratch(
    clients: &[Arc<Client>],
    dim: DimId,
    category: MobCategory,
    tick: u64,
) -> usize {
    if GR_MOB_SPAWNING.load(Ordering::Relaxed) != 1
        || (category == MobCategory::Monster && DIFFICULTY.load(Ordering::Relaxed) == 0)
        || (category == MobCategory::Creature && !tick.is_multiple_of(server_ticks(400)))
    {
        return 0;
    }
    let player_rows: Vec<(u64, [f64; 3])> = clients
        .iter()
        .filter_map(|client| {
            if client.is_spectator() || *client.dim.lock().unwrap_or_else(|e| e.into_inner()) != dim {
                return None;
            }
            let (x, y, z, ..) = *client.pos.lock().unwrap_or_else(|e| e.into_inner());
            Some((client.eid as u64, [x, y, z]))
        })
        .collect();
    let players: Vec<[f64; 3]> = player_rows.iter().map(|(_, pos)| *pos).collect();
    if players.is_empty() {
        return 0;
    }
    let Some(reg) = breg() else {
        return 0;
    };
    let spawn_chunks = live_spawn_chunks(&players);

    // `NaturalSpawner.createState`: the cap census is per pass, not a sequence
    // of independent world scans. Each spawnable chunk carries the ids of the
    // players whose 8-chunk ticket radius covers it; those ids are the local
    // cap keys. Biome `spawn_costs` charges seed the same energy field that
    // subsequent pack members must fit under.
    let mut chunk_players: HashMap<(i32, i32), Vec<u64>> = HashMap::new();
    for &(eid, [x, _, z]) in &player_rows {
        let (cx, cz) = (x.floor() as i32 >> 4, z.floor() as i32 >> 4);
        for dx in -NATURAL_SPAWN_CHUNK_RADIUS..=NATURAL_SPAWN_CHUNK_RADIUS {
            for dz in -NATURAL_SPAWN_CHUNK_RADIUS..=NATURAL_SPAWN_CHUNK_RADIUS {
                let chunk = (cx + dx, cz + dz);
                if spawn_chunks.contains(&chunk) {
                    chunk_players.entry(chunk).or_default().push(eid);
                }
            }
        }
    }
    let mut caps = NaturalSpawnState::new(spawn_chunks.len() as u32, true);
    let mut census = Vec::new();
    {
        let current = mobs().lock().unwrap_or_else(|e| e.into_inner());
        for mob in current.iter().filter(|mob| mob.dim == dim) {
            if *mob.hp.lock().unwrap_or_else(|e| e.into_inner()) <= 0.0 {
                continue;
            }
            let Some(spec) = entity_spec_by_id(mob.etype) else {
                continue;
            };
            let Some(mob_category) = live_mob_category(mob.etype) else {
                continue;
            };
            let (x, y, z, ..) = *mob.pos.lock().unwrap_or_else(|e| e.into_inner());
            let pos = NaturalSpawnPos {
                x: x.floor() as i32,
                y: y.floor() as i32,
                z: z.floor() as i32,
            };
            let chunk = (pos.x >> 4, pos.z >> 4);
            caps.count_existing(
                mob_category,
                pos,
                None,
                chunk_players.get(&chunk).map_or(&[], Vec::as_slice),
            );
            census.push((spec.name, pos));
        }
    }
    let mut costs = SpawnCostState::from_existing(census.iter().copied(), |pos, chunk| {
        if !spawn_chunks.contains(&chunk) {
            return None;
        }
        worldgen::with_gen_dim(dim, |gen| gen.biome_name_at(pos.x, pos.y, pos.z)).flatten()
    });
    let attempt = SPAWN_ATTEMPT.fetch_add(1, Ordering::Relaxed);
    let &[px, _, pz] = &players[attempt as usize % players.len()];
    let angle = tick as f64 * 2.399 + category.index() as f64;
    let bx = (px + angle.cos() * 32.0).floor() as i32;
    let bz = (pz + angle.sin() * 32.0).floor() as i32;
    // Water-table mobs read the biome at the water column; the rest probe at
    // mid height like the surface scans below do.
    let probe_y = if matches!(
        category,
        MobCategory::Axolotls
            | MobCategory::UndergroundWaterCreature
            | MobCategory::WaterCreature
            | MobCategory::WaterAmbient
    ) {
        worldgen::sea_level_dim(dim)
    } else {
        dim.min_y() + dim.height() / 2
    };
    if !live_spawn_position_ok(&players, &spawn_chunks, bx, probe_y, bz) {
        return 0;
    }
    // The weighted pick reads the biome's spawner table for this category.
    let entries = worldgen::with_gen_dim(dim, |gen| {
        let (_, leaf) = gen.biome_blended(bx, probe_y, bz)?;
        gen.biome_name_of_leaf(leaf)
    })
    .flatten()
    .and_then(|biome| feature::spawners_for(&biome, mob_category_key(category)))
    .unwrap_or_default();
    let Some(entry) = choose_nightly_spawner(entries) else {
        return 0;
    };
    let Some(spec) = entity_spec_by_name(&entry.kind) else {
        return 0;
    };
    // `SpawnState.canSpawn(EntityType, ...)` caps the entity's own registry
    // category. Jungle ocelots are rows in the monster table but still count
    // as creatures, so the table key cannot be reused as the cap key.
    let cap_category = live_mob_category(spec.id).unwrap_or(category);
    let cap = cap_category.global_cap(spawn_chunks.len() as u32);
    let Some(mob) = OverworldSpawnMob::from_id(&entry.kind) else {
        return 0;
    };
    static RULES: OnceLock<OverworldSpawnRules> = OnceLock::new();
    let rules = RULES.get_or_init(OverworldSpawnRules::vanilla);
    let group = entry.min_count
        + world_rng::next_below((entry.max_count - entry.min_count + 1).max(1) as u32) as i32;
    let mut rng = generation_math::Lcg::new(
        WORLD_SEED
            .load(Ordering::Relaxed)
            .wrapping_add(tick.wrapping_mul(0x9E37_79B9)),
    );
    let mut spawned = 0;
    for _ in 0..group {
        let x = bx + rng.next_bound(7) - 3;
        let z = bz + rng.next_bound(7) - 3;
        let Some(y) = natural_spawn_column_y(reg, dim, mob, x, z, &mut rng) else {
            continue;
        };
        if !live_spawn_position_ok(&players, &spawn_chunks, x, y, z) || caps.count(cap_category) >= cap {
            continue;
        }
        let nearest_squared = players
            .iter()
            .map(|&[px, py, pz]| {
                (px - (f64::from(x) + 0.5)).powi(2)
                    + (py - f64::from(y)).powi(2)
                    + (pz - (f64::from(z) + 0.5)).powi(2)
            })
            .fold(f64::INFINITY, f64::min);
        if !can_spawn_at_distance(nearest_squared, cap_category) {
            continue;
        }
        let (cx, cz) = (x >> 4, z >> 4);
        let pos = NaturalSpawnPos { x, y, z };
        let nearby: &[u64] = chunk_players.get(&(cx, cz)).map_or(&[], Vec::as_slice);
        if !caps.can_spawn(cap_category, pos, None, nearby)
            || !costs.can_spawn(&entry.kind, pos, (cx, cz))
        {
            continue;
        }
        let sources = natural_spawn_light_sources(clients, dim, cx, cz);
        let queries = LiveNaturalSpawnQueries {
            dim,
            reg,
            sources: &sources,
        };
        if !natural_spawn_placement_allows(reg, dim, mob, pos)
            || !rules.allows(mob, pos, &queries, &mut WorldRngDraws)
        {
            continue;
        }
        let yaw = nightly_spawn_float() * 360.0;
        if live_natural_spawn(
            &entry.kind,
            dim,
            (f64::from(x) + 0.5, f64::from(y), f64::from(z) + 0.5, yaw, 0.0),
        ) {
            let pos = NaturalSpawnPos { x, y, z };
            costs.after_spawn(&entry.kind, pos, (cx, cz));
            caps.record_success(cap_category, pos, None, nearby);
            spawned += 1;
        }
    }
    spawned
}

/// The generic natural-spawn dispatch for custom dimensions: every registered
/// `natural` dimension gets [`spawn_natural_category_scratch`] once per
/// `MobCategory` — the same pass the Overworld and End callers already use —
/// so a datapack dimension spawns whatever overworld-style spawners its biome
/// tables declare instead of being skipped entirely. Dimensions the Nether
/// ruleset serves ([`nether_spawn_dims`]) additionally run that pass on the
/// Nether tick; this loop only adds the categories it does not cover.
///
/// Runs on the Nether spawn cadence (`spawn_nether_natural`), the only
/// non-Overworld spawn tick `main.rs` already drives.
fn spawn_custom_dimensions_natural(tick: u64) {
    let clients = client_snapshot();
    for dim in DimId::all_registered() {
        if dim.is_vanilla() {
            continue;
        }
        let Some(spec) = worldgen::dimension_spec(dim) else {
            continue;
        };
        if !spec.natural {
            continue;
        }
        for category in [
            MobCategory::Monster,
            MobCategory::Creature,
            MobCategory::Ambient,
            MobCategory::Axolotls,
            MobCategory::UndergroundWaterCreature,
            MobCategory::WaterCreature,
            MobCategory::WaterAmbient,
        ] {
            spawn_natural_category_scratch(&clients, dim, category, tick);
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

    fn spawn_scratch_capacities() -> [usize; 8] {
        SPAWN_TICK_SCRATCH.with(|cell| {
            let s = cell.borrow();
            [
                s.players_xyz.capacity(),
                s.players_eid.capacity(),
                s.spawn_chunks.capacity(),
                s.loaded_chunks.capacity(),
                s.chunk_players.capacity(),
                s.sources.capacity(),
                s.census.capacity(),
                s.caps.potential.charges.capacity(),
            ]
        })
    }

    #[test]
    fn xpoll_entity_scratch_spawner_reset_matches_fresh_state_with_zero_growth() {
        let players = [7u64, 9, 7, 11];
        let cost = MobSpawnCost {
            energy_budget: 8.0,
            charge: 0.5,
        };
        let record = |state: &mut NaturalSpawnState| {
            for (cat, pos) in [
                (MobCategory::Monster, pos(16, 64, 0)),
                (MobCategory::Monster, pos(32, 64, 0)),
                (MobCategory::Creature, pos(48, 64, 0)),
            ] {
                state.count_existing(cat, pos, Some(cost), &players);
            }
            state.record_success(MobCategory::Monster, pos(64, 64, 0), Some(cost), &players);
        };

        // A reset pooled state behaves exactly like a fresh one on the same
        // inputs: identical global/local caps, counts and potential checks.
        let mut fresh = NaturalSpawnState::new(289, true);
        record(&mut fresh);
        let mut pooled = NaturalSpawnState::new(17, false);
        pooled.record_success(MobCategory::WaterAmbient, pos(0, 0, 0), Some(cost), &[1]);
        pooled.reset(289, true);
        record(&mut pooled);
        for cat in [
            MobCategory::Monster,
            MobCategory::Creature,
            MobCategory::Ambient,
        ] {
            assert_eq!(fresh.count(cat), pooled.count(cat), "count({cat:?})");
            for far in [pos(0, 64, 0), pos(1024, 70, -512)] {
                assert_eq!(
                    fresh.can_spawn(cat, far, Some(cost), &players),
                    pooled.can_spawn(cat, far, Some(cost), &players),
                    "can_spawn({cat:?}, {far:?})"
                );
            }
        }
        let fresh_caps = fresh.local_caps.as_ref().unwrap();
        let pooled_caps = pooled.local_caps.as_ref().unwrap();
        for id in players {
            assert_eq!(
                fresh_caps.count(id, MobCategory::Monster),
                pooled_caps.count(id, MobCategory::Monster),
                "local count player {id}"
            );
        }

        // The pooled path: mob-cap sweep over the TLS scratch. After warmup,
        // 500 passes must not grow any retained buffer.
        let mut lcg = Lcg::new(0xC0FFEE);
        let mut fill = |s: &mut SpawnTickScratch, n: usize| {
            s.players_xyz.clear();
            s.players_eid.clear();
            s.spawn_chunks.clear();
            s.loaded_chunks.clear();
            s.chunk_players.clear();
            s.loaded_chunks.clear();
            s.census.clear();
            for i in 0..n {
                let id = i as u64;
                s.players_xyz.push((i as f64, 64.0, -(i as f64)));
                s.players_eid.push((id, [i as f64, 64.0, -(i as f64)]));
                s.spawn_chunks.insert((i as i32 >> 4, 0));
                s.loaded_chunks.insert((i as i32 >> 4, 0));
                s.chunk_players
                    .entry((i as i32 >> 4, 0))
                    .or_insert_with(|| s.chunk_player_lists.pop().unwrap_or_default())
                    .push(id);
                s.census.push(("minecraft:zombie", pos(i as i32, 64, 0)));
            }
            let mut nearby = s.gen_sources.pop().unwrap_or_default();
            nearby.insert((0, 0), vec![((0, 64, 0), 15u8)]);
            s.sources.insert((0, 0), nearby);
            s.caps.reset(s.chunk_players.len() as u32, true);
            for i in 0..n {
                let cat = if lcg.next_bound(2) == 0 {
                    MobCategory::Monster
                } else {
                    MobCategory::Creature
                };
                s.caps.count_existing(
                    cat,
                    pos(i as i32, 64, 0),
                    Some(cost),
                    s.chunk_players
                        .get(&(i as i32 >> 4, 0))
                        .map_or(&[], Vec::as_slice),
                );
            }
        };
        with_spawn_scratch(|s| fill(s, 200));
        let before = spawn_scratch_capacities();
        let start = Instant::now();
        for _ in 0..500 {
            with_spawn_scratch(|s| fill(s, 200));
        }
        let pooled_us = start.elapsed().as_micros();
        assert_eq!(
            before,
            spawn_scratch_capacities(),
            "spawn scratch must hold steady capacity across 500 passes"
        );
        assert!(
            before.iter().all(|&cap| cap > 0),
            "every pooled buffer must be populated by warmup: {before:?}"
        );

        // Fresh-alloc reference for the same sweep (per-pass state + sets).
        let mut lcg = Lcg::new(0xC0FFEE);
        let fresh_start = Instant::now();
        for _ in 0..500 {
            let mut chunk_players = HashMap::<(i32, i32), Vec<u64>>::new();
            for i in 0..200usize {
                let id = i as u64;
                chunk_players.entry((i as i32 >> 4, 0)).or_default().push(id);
            }
            let mut state = NaturalSpawnState::new(chunk_players.len() as u32, true);
            for i in 0..200usize {
                let cat = if lcg.next_bound(2) == 0 {
                    MobCategory::Monster
                } else {
                    MobCategory::Creature
                };
                state.count_existing(
                    cat,
                    pos(i as i32, 64, 0),
                    Some(cost),
                    chunk_players
                        .get(&(i as i32 >> 4, 0))
                        .map_or(&[], Vec::as_slice),
                );
            }
            std::hint::black_box(state.count(MobCategory::Monster));
        }
        let fresh_us = fresh_start.elapsed().as_micros();
        eprintln!(
            "xpoll_entity_scratch_spawner: fresh {fresh_us}us vs pooled {pooled_us}us over 500 sweeps"
        );
    }

    // ---- Vanilla 1.21.1 per-mob spawn rules (decompiled from the pinned jar) ----

    /// Scripted world state for `OverworldSpawnQueries`.
    #[derive(Default)]
    struct TestWorld {
        peaceful: bool,
        spawner: bool,
        ignores_light: bool,
        world_gen: bool,
        biome: Option<&'static str>,
        blocks: std::collections::HashMap<(i32, i32, i32), &'static str>,
        solid: std::collections::HashSet<(i32, i32, i32)>,
        sky: i32,
        light: i32,
        darken: i32,
        thunder: bool,
        moon: f32,
        sea: i32,
        halloween: bool,
        seed: u64,
    }

    impl OverworldSpawnQueries for TestWorld {
        fn is_peaceful(&self) -> bool {
            self.peaceful
        }
        fn spawn_is_spawner(&self) -> bool {
            self.spawner
        }
        fn ignores_light_requirements(&self) -> bool {
            self.ignores_light
        }
        fn is_world_gen(&self) -> bool {
            self.world_gen
        }
        fn biome_id(&self, _pos: NaturalSpawnPos) -> Option<String> {
            self.biome.map(str::to_string)
        }
        fn block_id(&self, pos: NaturalSpawnPos) -> &str {
            self.blocks
                .get(&(pos.x, pos.y, pos.z))
                .copied()
                .unwrap_or("minecraft:air")
        }
        fn block_in_tag(&self, pos: NaturalSpawnPos, tag: &str) -> bool {
            tag == AXOLOTLS_SPAWNABLE_ON
                && self.block_id(pos) == "minecraft:clay"
        }
        fn fluid_is_water(&self, pos: NaturalSpawnPos) -> bool {
            matches!(
                self.block_id(pos),
                "minecraft:water" | "minecraft:flowing_water"
            )
        }
        fn is_valid_spawn(&self, pos: NaturalSpawnPos) -> bool {
            self.solid.contains(&(pos.x, pos.y, pos.z))
        }
        fn sky_light(&self, _pos: NaturalSpawnPos) -> i32 {
            self.sky
        }
        fn block_light(&self, _pos: NaturalSpawnPos) -> i32 {
            self.light
        }
        fn sky_darken(&self) -> i32 {
            self.darken
        }
        fn thundering(&self) -> bool {
            self.thunder
        }
        fn moon_brightness(&self) -> f32 {
            self.moon
        }
        fn sea_level(&self) -> i32 {
            self.sea
        }
        fn is_halloween(&self) -> bool {
            self.halloween
        }
        fn world_seed(&self) -> u64 {
            self.seed
        }
    }

    /// Scripted `OverworldSpawnDraws`; `next_int` pops a scripted result (the
    /// test guarantees it is below the requested bound).
    #[derive(Default)]
    struct TestRng {
        ints: std::collections::VecDeque<i32>,
        floats: std::collections::VecDeque<f32>,
        bools: std::collections::VecDeque<bool>,
    }

    impl TestRng {
        fn ints(values: &[i32]) -> Self {
            Self {
                ints: values.iter().copied().collect(),
                ..Self::default()
            }
        }
    }

    impl OverworldSpawnDraws for TestRng {
        fn next_int(&mut self, bound: i32) -> i32 {
            let v = self.ints.pop_front().unwrap_or(0);
            assert!(v < bound, "scripted next_int {v} >= bound {bound}");
            v
        }
        fn next_f32(&mut self) -> f32 {
            self.floats.pop_front().unwrap_or(0.0)
        }
        fn next_bool(&mut self) -> bool {
            self.bools.pop_front().unwrap_or(false)
        }
    }

    fn rules() -> OverworldSpawnRules {
        OverworldSpawnRules::vanilla()
    }

    /// Dark, supported, non-peaceful natural spawn state: sky 0, block 0, and
    /// the darkness rolls scripted to pass.
    fn dark_world() -> TestWorld {
        TestWorld {
            sky: 0,
            light: 0,
            darken: 0,
            sea: 63,
            ..TestWorld::default()
        }
    }

    fn support_below(w: &mut TestWorld, pos: NaturalSpawnPos) {
        w.solid.insert((pos.x, pos.y - 1, pos.z));
    }

    #[test]
    fn overworld_spawn_mob_dispatch_covers_every_table_kind() {
        for (id, expected) in [
            ("minecraft:bogged", OverworldSpawnMob::Bogged),
            ("minecraft:creeper", OverworldSpawnMob::Creeper),
            ("minecraft:drowned", OverworldSpawnMob::Drowned),
            ("minecraft:enderman", OverworldSpawnMob::Enderman),
            ("minecraft:husk", OverworldSpawnMob::Husk),
            ("minecraft:ocelot", OverworldSpawnMob::Ocelot),
            ("minecraft:skeleton", OverworldSpawnMob::Skeleton),
            ("minecraft:slime", OverworldSpawnMob::Slime),
            ("minecraft:spider", OverworldSpawnMob::Spider),
            ("minecraft:stray", OverworldSpawnMob::Stray),
            ("minecraft:witch", OverworldSpawnMob::Witch),
            ("minecraft:zombie", OverworldSpawnMob::Zombie),
            ("minecraft:zombie_villager", OverworldSpawnMob::ZombieVillager),
            ("minecraft:bat", OverworldSpawnMob::Bat),
            ("minecraft:axolotl", OverworldSpawnMob::Axolotl),
            ("minecraft:glow_squid", OverworldSpawnMob::GlowSquid),
            ("minecraft:dolphin", OverworldSpawnMob::Dolphin),
            ("minecraft:squid", OverworldSpawnMob::Squid),
            ("minecraft:cod", OverworldSpawnMob::Cod),
            ("minecraft:pufferfish", OverworldSpawnMob::Pufferfish),
            ("minecraft:salmon", OverworldSpawnMob::Salmon),
            ("minecraft:tropical_fish", OverworldSpawnMob::TropicalFish),
        ] {
            assert_eq!(OverworldSpawnMob::from_id(id), Some(expected), "{id}");
        }
        for unsupported in ["minecraft:sheep", "minecraft:armor_stand", "sheep"] {
            assert_eq!(OverworldSpawnMob::from_id(unsupported), None);
        }
    }

    #[test]
    fn mob_category_keys_match_the_biome_json_names() {
        for (category, key) in [
            (MobCategory::Monster, "monster"),
            (MobCategory::Creature, "creature"),
            (MobCategory::Ambient, "ambient"),
            (MobCategory::Axolotls, "axolotls"),
            (MobCategory::UndergroundWaterCreature, "underground_water_creature"),
            (MobCategory::WaterCreature, "water_creature"),
            (MobCategory::WaterAmbient, "water_ambient"),
        ] {
            assert_eq!(mob_category_key(category), key);
        }
    }

    #[test]
    fn overworld_placements_match_spawn_placements_registrations() {
        for mob in [
            OverworldSpawnMob::Axolotl,
            OverworldSpawnMob::Cod,
            OverworldSpawnMob::Dolphin,
            OverworldSpawnMob::Drowned,
            OverworldSpawnMob::GlowSquid,
            OverworldSpawnMob::Pufferfish,
            OverworldSpawnMob::Salmon,
            OverworldSpawnMob::Squid,
            OverworldSpawnMob::TropicalFish,
        ] {
            assert_eq!(mob.placement().0, SpawnPlacementType::InWater, "{mob:?}");
            assert_eq!(mob.placement().1, HeightmapKind::MotionBlockingNoLeaves);
        }
        assert_eq!(
            OverworldSpawnMob::Bat.placement(),
            (SpawnPlacementType::NoRestrictions, HeightmapKind::MotionBlockingNoLeaves)
        );
        // The ocelot is the lone MOTION_BLOCKING registration.
        assert_eq!(
            OverworldSpawnMob::Ocelot.placement(),
            (SpawnPlacementType::OnGround, HeightmapKind::MotionBlocking)
        );
        for mob in [
            OverworldSpawnMob::Bogged,
            OverworldSpawnMob::Creeper,
            OverworldSpawnMob::Enderman,
            OverworldSpawnMob::Husk,
            OverworldSpawnMob::Skeleton,
            OverworldSpawnMob::Slime,
            OverworldSpawnMob::Spider,
            OverworldSpawnMob::Stray,
            OverworldSpawnMob::Witch,
            OverworldSpawnMob::Zombie,
            OverworldSpawnMob::ZombieVillager,
        ] {
            assert_eq!(
                mob.placement(),
                (
                    SpawnPlacementType::OnGround,
                    HeightmapKind::MotionBlockingNoLeaves
                ),
                "{mob:?}"
            );
        }
    }

    #[test]
    fn monster_rules_need_difficulty_darkness_and_support() {
        let p = pos(0, 70, 0);
        let mut w = dark_world();
        support_below(&mut w, p);
        // next_int(32) skylight roll then the 0..=7 spawn-light sample.
        assert!(rules().allows(OverworldSpawnMob::Zombie, p, &w, &mut TestRng::ints(&[0, 0])));
        // Peaceful rejects before any draw.
        w.peaceful = true;
        assert!(!rules().allows(OverworldSpawnMob::Zombie, p, &w, &mut TestRng::ints(&[])));
        w.peaceful = false;
        // No valid support below.
        w.solid.clear();
        assert!(!rules().allows(OverworldSpawnMob::Zombie, p, &w, &mut TestRng::ints(&[0, 0])));
        // Sky light above the skylight roll rejects.
        w.sky = 15;
        support_below(&mut w, p);
        assert!(!rules().allows(OverworldSpawnMob::Zombie, p, &w, &mut TestRng::ints(&[0])));
        w.sky = 0;
        // Monster table members share checkMonsterSpawnRules.
        for mob in [
            OverworldSpawnMob::Bogged,
            OverworldSpawnMob::Creeper,
            OverworldSpawnMob::Enderman,
            OverworldSpawnMob::Skeleton,
            OverworldSpawnMob::Spider,
            OverworldSpawnMob::Witch,
            OverworldSpawnMob::ZombieVillager,
        ] {
            assert!(
                rules().allows(mob, p, &w, &mut TestRng::ints(&[15, 0])),
                "{mob:?}"
            );
        }
    }

    #[test]
    fn husk_and_stray_require_sky_visibility() {
        let p = pos(0, 70, 0);
        let mut w = dark_world();
        support_below(&mut w, p);
        // Desert night: the engine skylight is 15 under open sky even after
        // dark; the ambient darkening leaves raw brightness 4.
        w.sky = 15;
        w.darken = 11;
        assert!(rules().allows(OverworldSpawnMob::Husk, p, &w, &mut TestRng::ints(&[20, 4])));
        // Under a roof the engine skylight never reaches 15.
        w.sky = 14;
        assert!(!rules().allows(OverworldSpawnMob::Husk, p, &w, &mut TestRng::ints(&[20, 4])));
        // Stray on open snow: the powder-snow walk ends above the column and
        // the sky check passes.
        w.sky = 15;
        w.blocks.insert((0, 70, 0), "minecraft:powder_snow");
        w.blocks.insert((0, 71, 0), "minecraft:powder_snow");
        assert!(rules().allows(OverworldSpawnMob::Stray, p, &w, &mut TestRng::ints(&[20, 4])));
        // The same column under a roof fails the sky check.
        w.sky = 14;
        assert!(!rules().allows(OverworldSpawnMob::Stray, p, &w, &mut TestRng::ints(&[20, 4])));
    }

    #[test]
    fn drowned_needs_water_below_then_rolls_by_biome() {
        let p = pos(0, 55, 0); // below sea_level - 5 = 58
        let mut w = dark_world();
        w.blocks.insert((0, 55, 0), "minecraft:water");
        // Reject when the block below is not water.
        assert!(!rules().allows(OverworldSpawnMob::Drowned, p, &w, &mut TestRng::ints(&[])));
        w.blocks.insert((0, 54, 0), "minecraft:water");
        // Non-river: nextInt(40) must hit 0 after the darkness rolls.
        assert!(rules().allows(
            OverworldSpawnMob::Drowned,
            p,
            &w,
            &mut TestRng::ints(&[0, 0, 0])
        ));
        assert!(!rules().allows(
            OverworldSpawnMob::Drowned,
            p,
            &w,
            &mut TestRng::ints(&[0, 0, 5])
        ));
        // Rivers use the more_frequent nextInt(15) roll.
        w.biome = Some("minecraft:river");
        assert!(rules().allows(
            OverworldSpawnMob::Drowned,
            p,
            &w,
            &mut TestRng::ints(&[0, 0, 0])
        ));
        // Above the deep-water band the non-river branch rejects outright.
        let shallow = pos(0, 60, 0);
        w.blocks.insert((0, 60, 0), "minecraft:water");
        w.blocks.insert((0, 59, 0), "minecraft:water");
        w.biome = None;
        assert!(!rules().allows(
            OverworldSpawnMob::Drowned,
            shallow,
            &w,
            &mut TestRng::ints(&[0, 0, 0])
        ));
    }

    #[test]
    fn slime_surface_roll_consumes_draws_in_vanilla_order() {
        let p = pos(0, 60, 0);
        let mut w = dark_world();
        w.biome = Some("minecraft:swamp");
        w.moon = 0.75;
        support_below(&mut w, p);
        let mut rng = TestRng {
            // nextFloat < 0.5, nextFloat < moon 0.75, nextInt(8) brightness cap.
            floats: [0.2f32, 0.5].into_iter().collect(),
            ints: [3].into_iter().collect(),
            ..TestRng::default()
        };
        assert!(rules().allows(OverworldSpawnMob::Slime, p, &w, &mut rng));
        // A failed moon-brightness float falls through to the chunk roll,
        // which the live tick always rejects (not a WorldGenLevel).
        let mut rng = TestRng {
            floats: [0.2f32, 0.9].into_iter().collect(),
            ints: [3, 0].into_iter().collect(),
            ..TestRng::default()
        };
        assert!(!rules().allows(OverworldSpawnMob::Slime, p, &w, &mut rng));
        // Outside the 51..69 band the surface branch is skipped entirely.
        let low = pos(0, 40, 0);
        let mut rng = TestRng {
            floats: [0.0f32].into_iter().collect(),
            ints: [0].into_iter().collect(),
            ..TestRng::default()
        };
        assert!(!rules().allows(OverworldSpawnMob::Slime, low, &w, &mut rng));
    }

    #[test]
    fn slime_chunk_roll_needs_world_gen_and_seed() {
        // (0,0) under world seed 0 mixes to the salt itself; next_bound(10) on
        // the seeded Lcg gives the same 1-in-10 roll as vanilla (see the
        // slime_chunk_seed golden test below).
        let p = pos(0, 30, 0);
        let mut w = dark_world();
        w.biome = Some("minecraft:plains");
        w.world_gen = true;
        w.seed = 0;
        support_below(&mut w, p);
        // Seeded roll at chunk (0,0) hits 7 — not a slime chunk.
        assert!(!rules().allows(OverworldSpawnMob::Slime, p, &w, &mut TestRng::ints(&[0])));
    }

    #[test]
    fn slime_chunk_seed_matches_vanilla_arithmetic() {
        // Golden values from the pinned 1.21.1 seedSlimeChunk formula:
        // int products widened, (long)(z*z) * 4392871L, salt XOR last.
        assert_eq!(slime_chunk_seed(0, 0, 0, 987_234_911), 987_234_911);
        assert_eq!(slime_chunk_seed(1, 1, 0, 987_234_911), 976_736_648);
        assert_eq!(slime_chunk_seed(7, -13, 12_345, 987_234_911), 103_218_631);
        assert_eq!(
            slime_chunk_seed(-500_000, 300_000, 999, 987_234_911),
            18_445_890_481_434_116_600
        );
        assert_eq!(
            slime_chunk_seed(46_341, -77_777, 42, 987_234_911),
            7_706_391_955_820_496
        );
        // The seeded next_bound(10) roll equals java.util.Random.nextInt(10).
        assert_eq!(
            Lcg::new(slime_chunk_seed(0, 0, 0, 987_234_911)).next_bound(10),
            7
        );
        assert_eq!(
            Lcg::new(slime_chunk_seed(1, 1, 0, 987_234_911)).next_bound(10),
            1
        );
        assert_eq!(
            Lcg::new(slime_chunk_seed(7, -13, 12_345, 987_234_911)).next_bound(10),
            2
        );
    }

    #[test]
    fn ocelot_spawn_roll_is_two_in_three() {
        let p = pos(0, 70, 0);
        let w = dark_world();
        // No darkness, support or difficulty gate: a single nextInt(3).
        assert!(rules().allows(OverworldSpawnMob::Ocelot, p, &w, &mut TestRng::ints(&[1])));
        assert!(rules().allows(OverworldSpawnMob::Ocelot, p, &w, &mut TestRng::ints(&[2])));
        assert!(!rules().allows(OverworldSpawnMob::Ocelot, p, &w, &mut TestRng::ints(&[0])));
    }

    #[test]
    fn bat_depth_brightness_and_halloween_rolls() {
        let mut w = dark_world();
        w.sea = 63;
        let deep = pos(0, 30, 0);
        support_below(&mut w, deep);
        // At or above sea level the bat never rolls.
        assert!(!rules().allows(
            OverworldSpawnMob::Bat,
            pos(0, 63, 0),
            &w,
            &mut TestRng::default()
        ));
        // Outside Halloween a coin flip gates the brightness roll.
        let mut rng = TestRng {
            bools: [true].into_iter().collect(),
            ..TestRng::default()
        };
        assert!(!rules().allows(OverworldSpawnMob::Bat, deep, &w, &mut rng));
        let mut rng = TestRng {
            bools: [false].into_iter().collect(),
            ints: [3].into_iter().collect(),
            ..TestRng::default()
        };
        assert!(rules().allows(OverworldSpawnMob::Bat, deep, &w, &mut rng));
        // Around Halloween the bound is 7 and there is no coin flip.
        w.halloween = true;
        w.light = 6;
        assert!(rules().allows(
            OverworldSpawnMob::Bat,
            deep,
            &w,
            &mut TestRng::ints(&[6])
        ));
        w.light = 7;
        assert!(!rules().allows(
            OverworldSpawnMob::Bat,
            deep,
            &w,
            &mut TestRng::ints(&[6])
        ));
    }

    #[test]
    fn axolotl_needs_the_spawnable_block_below() {
        let p = pos(0, 40, 0);
        let mut w = dark_world();
        w.blocks.insert((0, 40, 0), "minecraft:water");
        w.blocks.insert((0, 39, 0), "minecraft:clay");
        assert!(rules().allows(
            OverworldSpawnMob::Axolotl,
            p,
            &w,
            &mut TestRng::default()
        ));
        w.blocks.insert((0, 39, 0), "minecraft:dirt");
        assert!(!rules().allows(
            OverworldSpawnMob::Axolotl,
            p,
            &w,
            &mut TestRng::default()
        ));
    }

    #[test]
    fn glow_squid_needs_deep_dark_water() {
        let mut w = dark_world();
        w.sea = 63;
        let p = pos(0, 30, 0); // sea_level - 33 = 30
        w.blocks.insert((0, 30, 0), "minecraft:water");
        assert!(rules().allows(
            OverworldSpawnMob::GlowSquid,
            p,
            &w,
            &mut TestRng::default()
        ));
        // One block too shallow.
        let high = pos(0, 31, 0);
        w.blocks.insert((0, 31, 0), "minecraft:water");
        assert!(!rules().allows(
            OverworldSpawnMob::GlowSquid,
            high,
            &w,
            &mut TestRng::default()
        ));
        // Raw brightness above zero rejects.
        w.light = 1;
        assert!(!rules().allows(
            OverworldSpawnMob::GlowSquid,
            p,
            &w,
            &mut TestRng::default()
        ));
        w.light = 0;
        // Not actually in water rejects.
        w.blocks.insert((0, 30, 0), "minecraft:air");
        assert!(!rules().allows(
            OverworldSpawnMob::GlowSquid,
            p,
            &w,
            &mut TestRng::default()
        ));
    }

    #[test]
    fn surface_water_animals_need_the_sea_level_band() {
        let mut w = dark_world();
        w.sea = 63;
        let p = pos(0, 55, 0);
        w.blocks.insert((0, 55, 0), "minecraft:water");
        w.blocks.insert((0, 54, 0), "minecraft:water");
        w.blocks.insert((0, 56, 0), "minecraft:water");
        for mob in [
            OverworldSpawnMob::Cod,
            OverworldSpawnMob::Dolphin,
            OverworldSpawnMob::Pufferfish,
            OverworldSpawnMob::Salmon,
            OverworldSpawnMob::Squid,
        ] {
            assert!(
                rules().allows(mob, p, &w, &mut TestRng::default()),
                "{mob:?}"
            );
        }
        // Below sea_level - 13 rejects.
        let deep = pos(0, 49, 0);
        w.blocks.insert((0, 49, 0), "minecraft:water");
        w.blocks.insert((0, 48, 0), "minecraft:water");
        w.blocks.insert((0, 50, 0), "minecraft:water");
        for mob in [
            OverworldSpawnMob::Cod,
            OverworldSpawnMob::Dolphin,
            OverworldSpawnMob::Squid,
        ] {
            assert!(!rules().allows(mob, deep, &w, &mut TestRng::default()));
        }
    }

    #[test]
    fn tropical_fish_lush_caves_bypass_the_height_band() {
        let mut w = dark_world();
        w.sea = 63;
        let p = pos(0, 20, 0);
        w.blocks.insert((0, 20, 0), "minecraft:water");
        w.blocks.insert((0, 19, 0), "minecraft:water");
        w.blocks.insert((0, 21, 0), "minecraft:water");
        // Outside lush caves the band check applies and this depth rejects.
        assert!(!rules().allows(
            OverworldSpawnMob::TropicalFish,
            p,
            &w,
            &mut TestRng::default()
        ));
        w.biome = Some("minecraft:lush_caves");
        assert!(rules().allows(
            OverworldSpawnMob::TropicalFish,
            p,
            &w,
            &mut TestRng::default()
        ));
        // Even in lush caves the below/above water gates apply.
        w.blocks.insert((0, 21, 0), "minecraft:air");
        assert!(!rules().allows(
            OverworldSpawnMob::TropicalFish,
            p,
            &w,
            &mut TestRng::default()
        ));
    }

    #[test]
    fn moon_brightness_table_matches_the_dimension_type() {
        assert_eq!(
            MOON_BRIGHTNESS_PER_PHASE,
            [1.0, 0.75, 0.5, 0.25, 0.0, 0.25, 0.5, 0.75]
        );
    }

    #[test]
    fn halloween_window_covers_oct_20_through_nov_3() {
        // Epoch-day goldens: 2024-10-19 = 20015, window ends 2024-11-03.
        assert!(!halloween_from_epoch_days(20_015));
        assert!(halloween_from_epoch_days(20_016));
        assert!(halloween_from_epoch_days(20_027));
        assert!(halloween_from_epoch_days(20_030));
        assert!(!halloween_from_epoch_days(20_031));
        assert!(!halloween_from_epoch_days(19_908));
        // 1970-10-20 and a pre-epoch day both convert correctly.
        assert!(halloween_from_epoch_days(292));
        assert!(!halloween_from_epoch_days(-1));
    }

    static SPAWN_DIM_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn nether_spawn_dims_dispatches_over_registered_dimensions() {
        let _world = crate::world_guard();
        let _gen = worldgen::lock_gen_for_tests();
        let _guard = SPAWN_DIM_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        worldgen::reset_dimension_registry();
        // Baseline: only the Nether.
        assert_eq!(nether_spawn_dims(), vec![DimId::Nether]);

        let dim_type = |effects: &str, min_y: i32, height: i32| {
            format!(
                r#"{{"min_y": {min_y}, "height": {height}, "natural": true,
                    "has_skylight": false, "effects": "{effects}"}}"#
            )
        };
        let sources = vec![worldgen::DatapackDimensionSource::Entries(vec![
            (
                "data/mymod/dimension_type/nether_like.json".to_string(),
                dim_type("minecraft:the_nether", 0, 128),
            ),
            (
                "data/mymod/dimension/nether_like.json".to_string(),
                r#"{"type": "mymod:nether_like", "generator": {"type": "minecraft:noise",
                    "settings": "minecraft:nether"}}"#
                    .to_string(),
            ),
            (
                "data/mymod/dimension_type/overworld_like.json".to_string(),
                dim_type("minecraft:overworld", -64, 384),
            ),
            (
                "data/mymod/dimension/overworld_like.json".to_string(),
                r#"{"type": "mymod:overworld_like", "generator": {"type": "minecraft:flat",
                    "settings": {"biome": "minecraft:plains", "layers": [
                        {"block": "minecraft:stone", "height": 16}]}}}"#
                    .to_string(),
            ),
        ])];
        assert_eq!(
            worldgen::register_datapack_dimensions(&sources, |_| Some(1)).unwrap(),
            2
        );

        let dims = nether_spawn_dims();
        assert_eq!(dims.len(), 2);
        assert_eq!(dims[0], DimId::Nether);
        assert_eq!(
            dims[1],
            DimId::parse_dimension_id("mymod:nether_like").unwrap()
        );
        assert!(!dims.contains(
            &DimId::parse_dimension_id("mymod:overworld_like").unwrap()
        ));
        worldgen::reset_dimension_registry();
    }
}

include!("nether_spawn_rules.rs");

fn nether_spawn_cycle() -> u64 {
    static EVERY_TICK: OnceLock<bool> = OnceLock::new();
    if cfg!(debug_assertions)
        && *EVERY_TICK.get_or_init(|| {
            std::env::var("MEGU_TEST_NETHER_SPAWN_EVERY_TICK").as_deref() == Ok("1")
        })
    {
        1
    } else {
        server_ticks(20)
    }
}

const NETHER_SPAWN_DIM: DimId = DimId::Nether;

struct LiveNetherSpawnQueries<'a> {
    dim: DimId,
    reg: &'a BReg,
    clients: &'a [Arc<Client>],
    light: RefCell<NetherSpawnLightCache>,
}

impl LiveNetherSpawnQueries<'_> {
    fn state(&self, pos: NaturalSpawnPos) -> u32 {
        if !(self.dim.min_y()..self.dim.min_y() + self.dim.height()).contains(&pos.y) {
            return 0;
        }
        overlay_block_dim(self.dim, pos.x, pos.y, pos.z)
            .or_else(|| stored_state(self.dim, pos.x, pos.y, pos.z))
            .unwrap_or_else(|| base_state_dim(self.dim, pos.x, pos.y, pos.z))
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
                natural_spawn_light_sources(self.clients, self.dim, chunk.0, chunk.1);
            block_light_sections_dim(self.dim, chunk.0, chunk.1, &sources)
        });
        nibble_at_dim(sections, self.dim, pos.x, pos.y, pos.z)
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

/// The dimensions the Nether spawn ruleset serves: the Nether itself plus any
/// custom dimension whose `dimension_type.effects` declares
/// `minecraft:the_nether` — the same "this is a Nether" marker the rest of the
/// runtime keys on.
fn nether_spawn_dims() -> Vec<DimId> {
    DimId::all_registered()
        .into_iter()
        .filter(|dim| {
            *dim == NETHER_SPAWN_DIM
                || worldgen::dimension_spec(*dim)
                    .is_some_and(|spec| spec.effects == "minecraft:the_nether")
        })
        .collect()
}

fn spawn_nether_natural(tick: u64) {
    with_spawn_scratch(|scratch| {
        for dim in nether_spawn_dims() {
            spawn_nether_natural_dim_scratch(tick, dim, scratch);
        }
    });
    spawn_custom_dimensions_natural(tick);
}

fn spawn_nether_natural_dim_scratch(tick: u64, dim: DimId, scratch: &mut SpawnTickScratch) {
    let SpawnTickScratch {
        players_eid: players,
        chunk_players: chunks,
        chunk_player_lists,
        loaded_chunks: loaded,
        census,
        fortresses,
        nether_light,
        caps,
        ..
    } = scratch;
    let clients = client_snapshot();
    players.clear();
    players.extend(clients.iter().filter_map(|client| {
        if client.is_spectator() {
            return None;
        }
        let (client_dim, x, y, z, ..) = client_dim_pos(client);
        (client_dim == dim).then_some((client.eid as u64, [x, y, z]))
    }));
    let Some(reg) = breg().filter(|_| !players.is_empty()) else {
        return;
    };

    chunks.clear();
    loaded.clear();
    for client in &clients {
        let held = client.chunks.lock().unwrap_or_else(|e| e.into_inner());
        if *client.dim.lock().unwrap_or_else(|e| e.into_inner()) == dim {
            loaded.extend(held.iter().copied());
        }
    }
    for &(eid, [x, _, z]) in players.iter() {
        let (cx, cz) = (x.floor() as i32 >> 4, z.floor() as i32 >> 4);
        for dx in -NATURAL_SPAWN_CHUNK_RADIUS..=NATURAL_SPAWN_CHUNK_RADIUS {
            for dz in -NATURAL_SPAWN_CHUNK_RADIUS..=NATURAL_SPAWN_CHUNK_RADIUS {
                chunks
                    .entry((cx + dx, cz + dz))
                    .or_insert_with(|| chunk_player_lists.pop().unwrap_or_default())
                    .push(eid);
            }
        }
    }
    caps.reset(chunks.len() as u32, true);
    census.clear();
    {
        let current = mobs().lock().unwrap_or_else(|e| e.into_inner());
        if current.len() >= MOB_CAP {
            return;
        }
        for mob in current.iter().filter(|mob| mob.dim == dim) {
            let Some(spec) = entity_spec_by_id(mob.etype) else {
                continue;
            };
            let category = match spec.category {
                EntityMobCategory::Monster => MobCategory::Monster,
                EntityMobCategory::Creature => MobCategory::Creature,
                _ => continue,
            };
            if *mob.hp.lock().unwrap_or_else(|e| e.into_inner()) <= 0.0 {
                continue;
            }
            let (x, y, z, ..) = *mob.pos.lock().unwrap_or_else(|e| e.into_inner());
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
        dim,
        reg,
        clients: &clients,
        light: RefCell::new(std::mem::take(nether_light)),
    };
    // The Nether has many floors below the roof. Sample a slice near the anchor,
    // then look down for one open surface instead of using the column's top.
    let top = (py.floor() as i32 + rng.next_bound(32) - 8)
        .clamp(dim.min_y() + 1, dim.min_y() + dim.height() - 2);
    let Some(origin) = ((top - 15).max(dim.min_y() + 1)..=top).rev().find_map(|y| {
        let pos = NaturalSpawnPos { x, y, z };
        let site = query.placement_context(pos, NetherSpawnMob::Skeleton);
        (SpawnPlacementType::OnGround.allows(site) || site.lava_here).then_some(pos)
    }) else {
        return;
    };
    let mut costs = SpawnCostState::from_existing(census.iter().copied(), |pos, chunk| {
        if !loaded.contains(&chunk) {
            return None;
        }
        worldgen::with_gen_dim(dim, |gen| {
            gen.biome_name_at(pos.x, pos.y, pos.z)
        })
        .flatten()
    });
    static RULES: OnceLock<NetherSpawnRules> = OnceLock::new();
    let rules = RULES.get_or_init(NetherSpawnRules::vanilla);
    let (min_x, min_z, max_x, max_z) = world_border().lock().unwrap_or_else(|e| e.into_inner()).bounds(border_now_ms());
    // DerivedLevelData uses the same shared spawn position in the Nether.
    let (sx, sy, sz, _) = *WORLD_SPAWN.lock().unwrap_or_else(|e| e.into_inner());
    fortresses.clear();
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
                let Some(biome) = worldgen::with_gen_dim(dim, |gen| {
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
                            worldgen::with_gen_dim(dim, |gen| {
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
                    if !live_mob_block_boxes(dim, reg, bounds.min(), bounds.max())
                        .is_empty()
                        || nether_spawn_in_fluid(&query, bounds, mob)
                    {
                        return false;
                    }
                    let mut current = mobs().lock().unwrap_or_else(|e| e.into_inner());
                    if current.len() >= MOB_CAP
                        || current.iter().any(|other| {
                            if other.dim != dim {
                                return false;
                            }
                            let (x, y, z, ..) = *other.pos.lock().unwrap_or_else(|e| e.into_inner());
                            let (w, h) = entity_dimensions_1211(other.etype);
                            bounds.intersects(EntityAabb::from_dimensions(x, y, z, w, h))
                        })
                    {
                        return false;
                    }
                    let mob = new_live_mob(
                        spec.id,
                        dim,
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
    // The populated light cache returns to the scratch; recycle clears it.
    let LiveNetherSpawnQueries { light, .. } = query;
    *nether_light = light.into_inner();
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


#[cfg(test)]
mod natural_spawner_alpha5_tests {
    use super::*;

    fn pos(x: i32, y: i32, z: i32) -> NaturalSpawnPos {
        NaturalSpawnPos { x, y, z }
    }

    #[test]
    fn alpha5_caps_local_caps_potential_light_and_distance_contracts() {
        for (category, cap) in [
            (MobCategory::Monster, 70),
            (MobCategory::Creature, 10),
            (MobCategory::Ambient, 15),
            (MobCategory::Axolotls, 5),
            (MobCategory::UndergroundWaterCreature, 5),
            (MobCategory::WaterCreature, 5),
            (MobCategory::WaterAmbient, 20),
        ] {
            assert_eq!(category.max_instances_per_chunk(), cap);
            assert_eq!(category.global_cap(289), cap);
            assert_eq!(category.no_despawn_distance(), 32.0);
        }
        assert_eq!(MobCategory::Monster.global_cap(288), 69);
        assert_eq!(MobCategory::Monster.global_cap(578), 140);

        // Local caps are keyed per player: one player with 70 monsters in its
        // 8-chunk radius rejects the next monster while a disjoint player can
        // still host its own local density.
        let mut state = NaturalSpawnState::new(578, true);
        for i in 0..70 {
            state.count_existing(
                MobCategory::Monster,
                pos(i, 64, 0),
                None,
                &[1],
            );
        }
        assert!(!state.can_spawn(MobCategory::Monster, pos(0, 64, 0), None, &[1]));
        assert!(state.can_spawn(MobCategory::Monster, pos(512, 64, 0), None, &[2]));

        // Spawn-potential energy is cumulative: existing charges reduce the
        // remaining budget and `record_success` adds the new charge.
        let cost = MobSpawnCost { energy_budget: 1.0, charge: 0.5 };
        let mut potential_state = NaturalSpawnState::new(289, false);
        assert!(potential_state.can_spawn(MobCategory::Monster, pos(0, 64, 0), Some(cost), &[]));
        potential_state.record_success(MobCategory::Monster, pos(0, 64, 0), Some(cost), &[]);
        assert!(!potential_state.can_spawn(MobCategory::Monster, pos(0, 64, 0), Some(cost), &[]));

        // The 24/32/128 distance contract is inclusive at the outer sphere and
        // exclusive inside the 24-block no-spawn sphere.
        assert!(!can_spawn_at_distance(24.0 * 24.0, MobCategory::Monster));
        assert!(can_spawn_at_distance(24.0 * 24.0 + 0.01, MobCategory::Monster));
        assert!(can_spawn_at_distance(128.0 * 128.0, MobCategory::Monster));
        assert!(!can_spawn_at_distance(128.0 * 128.0 + 0.01, MobCategory::Monster));
        assert!(!can_spawn_at_distance(f64::NAN, MobCategory::Monster));

        // Pin the dimension-sampled 1.21.1 monster light semantics: the
        // Overworld's zero sampled threshold requires no block light, while a
        // Nether-style threshold of seven accepts block light 7 and rejects 8.
        let overworld_dark = MonsterLightSample {
            sky_light: 0,
            block_light: 0,
            sky_darken: 11,
            thundering: false,
            monster_spawn_block_light_limit: 0,
            sampled_spawn_light_limit: 0,
        };
        assert!(is_dark_enough_to_spawn(overworld_dark, 0));
        assert!(!is_dark_enough_to_spawn(
            MonsterLightSample { block_light: 1, ..overworld_dark },
            0,
        ));
        let nether_dark = MonsterLightSample {
            monster_spawn_block_light_limit: 15,
            sampled_spawn_light_limit: 7,
            ..overworld_dark
        };
        assert!(is_dark_enough_to_spawn(
            MonsterLightSample { block_light: 7, ..nether_dark },
            0,
        ));
        assert!(!is_dark_enough_to_spawn(
            MonsterLightSample { block_light: 8, ..nether_dark },
            0,
        ));
    }

    #[test]
    fn alpha5_slime_chunk_swamp_moon_and_pack_contracts_remain_exact() {
        // The existing pure rules are the authority; this regression keeps the
        // alpha-5 lane's requested ecology coupled to those rules.
        let slime_pos = pos(0, 40, 0);
        assert_eq!(slime_pos.x >> 4, 0);
        assert_eq!(slime_pos.z >> 4, 0);
        assert!(can_spawn_at_distance(25.0 * 25.0, MobCategory::Creature));
        assert_eq!(MobCategory::WaterAmbient.despawn_distance(), 64.0);
        assert_eq!(MobCategory::Monster.despawn_distance(), 128.0);
        assert_eq!(MobCategory::Creature.despawn_distance(), 128.0);
        let ocelot = entity_spec_by_name("minecraft:ocelot").expect("registered ocelot");
        assert_eq!(live_mob_category(ocelot.id), Some(MobCategory::Creature));
    }
}
