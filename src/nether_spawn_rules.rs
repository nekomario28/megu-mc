// Natural spawn rules from the pinned 1.21.1 server (59353fb40c36d304f2035d51e7d6e6baa98dc05c).
// See SpawnPlacements, Monster, NaturalSpawner, and ChunkGenerator.getMobsAt.

use crate::block_registry::HeightmapKind;
use crate::feature::SpawnerData;

// `NetherSpawnRules::vanilla` reads this file at runtime; the test module still
// pins the same bytes so its assertions read the datapack file itself.
#[cfg(test)]
const NETHER_DIMENSION_TYPE: &[u8] =
    include_bytes!("../datapacks/vanilla/data/minecraft/dimension_type/the_nether.json");

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) enum NetherSpawnMob {
    ZombifiedPiglin,
    Ghast,
    MagmaCube,
    Enderman,
    Piglin,
    Hoglin,
    Skeleton,
    Strider,
    Blaze,
    WitherSkeleton,
}

#[cfg_attr(not(test), allow(dead_code))]
impl NetherSpawnMob {
    pub(crate) fn from_id(id: &str) -> Option<Self> {
        Some(match id {
            "minecraft:zombified_piglin" => Self::ZombifiedPiglin,
            "minecraft:ghast" => Self::Ghast,
            "minecraft:magma_cube" => Self::MagmaCube,
            "minecraft:enderman" => Self::Enderman,
            "minecraft:piglin" => Self::Piglin,
            "minecraft:hoglin" => Self::Hoglin,
            "minecraft:skeleton" => Self::Skeleton,
            "minecraft:strider" => Self::Strider,
            "minecraft:blaze" => Self::Blaze,
            "minecraft:wither_skeleton" => Self::WitherSkeleton,
            _ => return None,
        })
    }

    /// The placement and heightmap registered by SpawnPlacements.
    pub(crate) const fn placement(self) -> (SpawnPlacementType, HeightmapKind) {
        let placement = match self {
            Self::Strider => SpawnPlacementType::InLava,
            _ => SpawnPlacementType::OnGround,
        };
        (placement, HeightmapKind::MotionBlockingNoLeaves)
    }
}

#[cfg_attr(not(test), allow(dead_code))]
pub(crate) trait NetherSpawnQueries {
    fn is_peaceful(&self) -> bool;
    fn block_id(&self, pos: NaturalSpawnPos) -> &str;
    fn block_light(&self, pos: NaturalSpawnPos) -> i32;
    /// BlockState.isValidSpawn for the given entity type and supporting block.
    fn is_valid_spawn(&self, pos: NaturalSpawnPos, mob: NetherSpawnMob) -> bool;
    /// FluidState.is(FluidTags.LAVA), including flowing lava. Out-of-world fluid is empty.
    fn is_lava(&self, pos: NaturalSpawnPos) -> bool;
    /// BlockState.isAir, including cave air and void air.
    fn is_air(&self, pos: NaturalSpawnPos) -> bool;
}

#[cfg_attr(not(test), allow(dead_code))]
struct NetherMonsterLight {
    block_limit: i32,
    spawn_limit: i32,
}

#[cfg_attr(not(test), allow(dead_code))]
impl NetherMonsterLight {
    fn from_dimension_type(data: &[u8]) -> Option<Self> {
        let dimension = jparse(data)?;
        let level = |key| match dimension.get(key)? {
            J::Num(n) if (0.0..=15.0).contains(n) && n.fract() == 0.0 => Some(*n as i32),
            _ => None,
        };
        Some(Self {
            block_limit: level("monster_spawn_block_light_limit")?,
            spawn_limit: level("monster_spawn_light_level")?,
        })
    }

    fn allows(&self, block_light: impl FnOnce() -> i32, next_int: impl FnOnce(i32) -> i32) -> bool {
        // Monster.isDarkEnoughToSpawn still rolls for skylight in the Nether.
        // There is no skylight, time is fixed, and ConstantInt.sample consumes no RNG.
        monster_spawn_light_allows(0, next_int(32), self.block_limit, block_light, 0, || {
            self.spawn_limit
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) enum NetherFortressBounds {
    Structure,
    Piece,
}

#[cfg_attr(not(test), allow(dead_code))]
pub(crate) struct NetherSpawnRules {
    light: NetherMonsterLight,
    fortress_spawners: [SpawnerData; 5],
}

#[cfg_attr(not(test), allow(dead_code))]
impl NetherSpawnRules {
    pub(crate) fn vanilla() -> Self {
        Self {
            light: match crate::structure_transform::pinned_data_bytes(
                "datapacks/vanilla/data/minecraft/dimension_type/the_nether.json",
            ) {
                Some(bytes) => NetherMonsterLight::from_dimension_type(&bytes)
                    .expect("pinned Nether dimension type must have valid monster spawn light rules"),
                // No `datapacks/` tree at all: the pinned 1.21.1 file's own
                // values - block-light limit 15, constant light level 7.
                None => NetherMonsterLight {
                    block_limit: 15,
                    spawn_limit: 7,
                },
            },
            // NetherFortressStructure.FORTRESS_ENEMIES. The pinned fortress.json
            // registers the same list for its piece-level Monster override.
            fortress_spawners: [
                ("minecraft:blaze", 2, 3, 10),
                ("minecraft:zombified_piglin", 4, 4, 5),
                ("minecraft:wither_skeleton", 5, 5, 8),
                ("minecraft:skeleton", 5, 5, 2),
                ("minecraft:magma_cube", 4, 4, 3),
            ]
            .map(|(kind, min_count, max_count, weight)| SpawnerData {
                kind: kind.into(),
                min_count,
                max_count,
                weight,
            }),
        }
    }

    /// SpawnPlacements.checkSpawnRules for MobSpawnType.NATURAL.
    /// The random query implements nextInt, returning a value in 0..bound.
    pub(crate) fn allows(
        &self,
        mob: NetherSpawnMob,
        pos: NaturalSpawnPos,
        world: &impl NetherSpawnQueries,
        next_int: impl FnOnce(i32) -> i32,
    ) -> bool {
        // Mob.checkMobSpawnRules checks the support block for natural spawns.
        let valid_support = || world.is_valid_spawn(nether_spawn_below(pos), mob);
        let above_wart =
            || world.block_id(nether_spawn_below(pos)) == "minecraft:nether_wart_block";
        match mob {
            // ZombifiedPiglin.checkZombifiedPiglinSpawnRules
            NetherSpawnMob::ZombifiedPiglin => !world.is_peaceful() && !above_wart(),
            // Ghast.checkGhastSpawnRules
            NetherSpawnMob::Ghast => !world.is_peaceful() && next_int(20) == 0 && valid_support(),
            // MagmaCube.checkMagmaCubeSpawnRules
            NetherSpawnMob::MagmaCube => !world.is_peaceful(),
            // Piglin.checkPiglinSpawnRules and Hoglin.checkHoglinSpawnRules
            NetherSpawnMob::Piglin | NetherSpawnMob::Hoglin => !above_wart(),
            // Monster.checkMonsterSpawnRules
            NetherSpawnMob::Enderman
            | NetherSpawnMob::Skeleton
            | NetherSpawnMob::WitherSkeleton => {
                !world.is_peaceful()
                    && self.light.allows(|| world.block_light(pos), next_int)
                    && valid_support()
            }
            // Monster.checkAnyLightMonsterSpawnRules
            NetherSpawnMob::Blaze => !world.is_peaceful() && valid_support(),
            // Strider.checkStriderSpawnRules starts one block above the candidate.
            NetherSpawnMob::Strider => {
                let mut above = NaturalSpawnPos {
                    y: pos.y + 1,
                    ..pos
                };
                while world.is_lava(above) {
                    above.y += 1;
                }
                world.is_air(above)
            }
        }
    }

    /// NaturalSpawner.mobsAt and ChunkGenerator.getMobsAt for the pinned Nether.
    pub(crate) fn spawners_at(
        &self,
        biome: &str,
        category: MobCategory,
        pos: NaturalSpawnPos,
        below_is_nether_bricks: impl FnOnce(NaturalSpawnPos) -> bool,
        mut in_fortress: impl FnMut(NaturalSpawnPos, NetherFortressBounds) -> bool,
    ) -> &[SpawnerData] {
        // NaturalSpawner.isInNetherFortressBounds uses StructureManager.getStructureAt.
        // The data-driven fortress override separately uses the individual piece bounds.
        if category == MobCategory::Monster
            && ((below_is_nether_bricks(nether_spawn_below(pos))
                && in_fortress(pos, NetherFortressBounds::Structure))
                || in_fortress(pos, NetherFortressBounds::Piece))
        {
            return &self.fortress_spawners;
        }
        let category = match category {
            MobCategory::Monster => "monster",
            MobCategory::Creature => "creature",
            MobCategory::Ambient => "ambient",
            MobCategory::Axolotls => "axolotls",
            MobCategory::UndergroundWaterCreature => "underground_water_creature",
            MobCategory::WaterCreature => "water_creature",
            MobCategory::WaterAmbient => "water_ambient",
        };
        feature::spawners_for(biome, category).unwrap_or_default()
    }
}

#[cfg_attr(not(test), allow(dead_code))]
fn nether_spawn_below(pos: NaturalSpawnPos) -> NaturalSpawnPos {
    NaturalSpawnPos {
        y: pos.y - 1,
        ..pos
    }
}

/// WeightedRandomList.getRandom, using the same subtraction as choose_nightly_spawner.
/// The random query implements nextInt, returning a value in 0..bound.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn choose_nether_spawner(
    entries: &[SpawnerData],
    next_int: impl FnOnce(i32) -> i32,
) -> Option<&SpawnerData> {
    let weight = entries.iter().try_fold(0i32, |total, entry| {
        total
            .checked_add(entry.weight)
            .filter(|_| entry.weight >= 0)
    })?;
    if weight == 0 {
        return None;
    }
    let mut roll = next_int(weight);
    entries.iter().find(|entry| {
        roll -= entry.weight;
        roll < 0
    })
}

#[cfg(test)]
mod nether_spawn_tests {
    use super::*;
    use NetherSpawnMob::*;

    const POS: NaturalSpawnPos = NaturalSpawnPos { x: 6, y: 40, z: -5 };

    struct TestWorld {
        peaceful: bool,
        floor: &'static str,
        light: i32,
        valid_support: bool,
        lava_above: i32,
        above_lava: &'static str,
    }

    impl Default for TestWorld {
        fn default() -> Self {
            Self {
                peaceful: false,
                floor: "minecraft:netherrack",
                light: 0,
                valid_support: true,
                lava_above: 0,
                above_lava: "minecraft:air",
            }
        }
    }

    impl NetherSpawnQueries for TestWorld {
        fn is_peaceful(&self) -> bool {
            self.peaceful
        }

        fn block_id(&self, pos: NaturalSpawnPos) -> &str {
            assert_eq!(pos, NaturalSpawnPos { y: 39, ..POS });
            self.floor
        }

        fn block_light(&self, pos: NaturalSpawnPos) -> i32 {
            assert_eq!(pos, POS);
            self.light
        }

        fn is_valid_spawn(&self, pos: NaturalSpawnPos, mob: NetherSpawnMob) -> bool {
            assert_eq!(pos, NaturalSpawnPos { y: 39, ..POS });
            assert!(matches!(
                mob,
                Ghast | Enderman | Skeleton | Blaze | WitherSkeleton
            ));
            self.valid_support
        }

        fn is_lava(&self, pos: NaturalSpawnPos) -> bool {
            assert_eq!((pos.x, pos.z), (POS.x, POS.z));
            assert!(pos.y > POS.y);
            pos.y <= POS.y + self.lava_above
        }

        fn is_air(&self, pos: NaturalSpawnPos) -> bool {
            assert_eq!(
                pos,
                NaturalSpawnPos {
                    y: POS.y + self.lava_above + 1,
                    ..POS
                }
            );
            matches!(
                self.above_lava,
                "minecraft:air" | "minecraft:cave_air" | "minecraft:void_air"
            )
        }
    }

    fn no_roll(_: i32) -> i32 {
        panic!("spawn rule should not draw RNG")
    }

    #[test]
    fn nether_spawn_light_uses_pinned_constant_and_consumes_one_sky_roll() {
        let light = NetherSpawnRules::vanilla().light;
        assert_eq!((light.block_limit, light.spawn_limit), (15, 7));
        let dimension = jparse(NETHER_DIMENSION_TYPE).unwrap();
        assert_eq!(dimension.get("has_skylight").unwrap().num(), 0.0);
        assert_eq!(dimension.get("fixed_time").unwrap().num(), 18000.0);
        for block_light in 0..=15 {
            let mut bounds = Vec::new();
            assert_eq!(
                light.allows(
                    || block_light,
                    |bound| {
                        bounds.push(bound);
                        0
                    }
                ),
                block_light <= 7
            );
            assert_eq!(bounds, [32]);
        }
    }

    #[test]
    fn nether_spawn_registered_placements() {
        for (id, placement) in [
            ("minecraft:strider", SpawnPlacementType::InLava),
            ("minecraft:ghast", SpawnPlacementType::OnGround),
            ("minecraft:zombified_piglin", SpawnPlacementType::OnGround),
            ("minecraft:magma_cube", SpawnPlacementType::OnGround),
            ("minecraft:enderman", SpawnPlacementType::OnGround),
            ("minecraft:piglin", SpawnPlacementType::OnGround),
            ("minecraft:hoglin", SpawnPlacementType::OnGround),
            ("minecraft:skeleton", SpawnPlacementType::OnGround),
            ("minecraft:blaze", SpawnPlacementType::OnGround),
            ("minecraft:wither_skeleton", SpawnPlacementType::OnGround),
        ] {
            assert_eq!(
                NetherSpawnMob::from_id(id).unwrap().placement(),
                (placement, HeightmapKind::MotionBlockingNoLeaves)
            );
        }
        assert_eq!(NetherSpawnMob::from_id("minecraft:creeper"), None);
    }

    #[test]
    fn nether_spawn_zombified_piglin_requires_nonpeaceful_and_nonwart_floor() {
        let rules = NetherSpawnRules::vanilla();
        let mut world = TestWorld::default();
        assert!(rules.allows(ZombifiedPiglin, POS, &world, no_roll));
        world.floor = "minecraft:nether_wart_block";
        assert!(!rules.allows(ZombifiedPiglin, POS, &world, no_roll));
        world.floor = "minecraft:warped_wart_block";
        assert!(rules.allows(ZombifiedPiglin, POS, &world, no_roll));
        world.peaceful = true;
        assert!(!rules.allows(ZombifiedPiglin, POS, &world, no_roll));
    }

    #[test]
    fn nether_spawn_piglin_and_hoglin_only_check_for_nether_wart_floor() {
        let rules = NetherSpawnRules::vanilla();
        let mut world = TestWorld {
            peaceful: true,
            light: 15,
            valid_support: false,
            floor: "minecraft:warped_wart_block",
            ..TestWorld::default()
        };
        for mob in [Piglin, Hoglin] {
            assert!(rules.allows(mob, POS, &world, no_roll));
        }
        world.floor = "minecraft:nether_wart_block";
        for mob in [Piglin, Hoglin] {
            assert!(!rules.allows(mob, POS, &world, no_roll));
        }
    }

    #[test]
    fn nether_spawn_ghast_requires_nonpeaceful_zero_roll_and_valid_support() {
        let rules = NetherSpawnRules::vanilla();
        let mut world = TestWorld {
            light: 15,
            ..TestWorld::default()
        };
        let zero = |bound| {
            assert_eq!(bound, 20);
            0
        };
        assert!(rules.allows(Ghast, POS, &world, zero));
        assert!(!rules.allows(Ghast, POS, &world, |_| 19));
        world.valid_support = false;
        assert!(!rules.allows(Ghast, POS, &world, zero));
        world.peaceful = true;
        assert!(!rules.allows(Ghast, POS, &world, no_roll));
    }

    #[test]
    fn nether_spawn_magma_cube_only_checks_difficulty() {
        let rules = NetherSpawnRules::vanilla();
        let mut world = TestWorld {
            light: 15,
            valid_support: false,
            ..TestWorld::default()
        };
        assert!(rules.allows(MagmaCube, POS, &world, no_roll));
        world.peaceful = true;
        assert!(!rules.allows(MagmaCube, POS, &world, no_roll));
    }

    #[test]
    fn nether_spawn_strider_requires_air_above_the_lava_column() {
        let rules = NetherSpawnRules::vanilla();
        let mut world = TestWorld {
            peaceful: true,
            ..TestWorld::default()
        };
        assert!(rules.allows(Strider, POS, &world, no_roll));
        world.lava_above = 3;
        world.above_lava = "minecraft:cave_air";
        assert!(rules.allows(Strider, POS, &world, no_roll));
        for block in ["minecraft:netherrack", "minecraft:crimson_roots"] {
            world.above_lava = block;
            assert!(!rules.allows(Strider, POS, &world, no_roll));
        }
    }

    #[test]
    fn nether_spawn_monster_rules_check_difficulty_light_and_support() {
        let rules = NetherSpawnRules::vanilla();
        let sky_roll = |bound| {
            assert_eq!(bound, 32);
            31
        };
        for mob in [Enderman, Skeleton, WitherSkeleton] {
            let mut world = TestWorld {
                light: 7,
                ..TestWorld::default()
            };
            assert!(rules.allows(mob, POS, &world, sky_roll));
            world.light = 8;
            assert!(!rules.allows(mob, POS, &world, sky_roll));
            world.light = 0;
            world.valid_support = false;
            assert!(!rules.allows(mob, POS, &world, sky_roll));
            world.valid_support = true;
            world.peaceful = true;
            assert!(!rules.allows(mob, POS, &world, no_roll));
        }
    }

    #[test]
    fn nether_spawn_blaze_requires_nonpeaceful_and_valid_support_at_any_light() {
        let rules = NetherSpawnRules::vanilla();
        let mut world = TestWorld {
            light: 15,
            ..TestWorld::default()
        };
        assert!(rules.allows(Blaze, POS, &world, no_roll));
        world.peaceful = true;
        assert!(!rules.allows(Blaze, POS, &world, no_roll));
        world.peaceful = false;
        world.valid_support = false;
        assert!(!rules.allows(Blaze, POS, &world, no_roll));
    }

    fn assert_spawners(entries: &[SpawnerData], expected: &[(&str, i32, i32, i32)]) {
        let actual: Vec<_> = entries
            .iter()
            .map(|e| (e.kind.as_str(), e.min_count, e.max_count, e.weight))
            .collect();
        assert_eq!(actual, expected);
        let total: i32 = expected.iter().map(|entry| entry.3).sum();
        let mut start = 0;
        for entry in entries {
            for roll in [start, start + entry.weight - 1] {
                assert_eq!(
                    choose_nether_spawner(entries, |bound| {
                        assert_eq!(bound, total);
                        roll
                    }),
                    Some(entry)
                );
            }
            start += entry.weight;
        }
    }

    #[test]
    fn nether_spawn_biome_lists_and_weighted_picks() {
        // Orders and group sizes from the pinned MobSpawnSettings biome oracle.
        type BiomeSpawnCase<'a> = (&'a str, &'a [(&'a str, i32, i32, i32)]);
        let cases: &[BiomeSpawnCase<'_>] = &[
            (
                "minecraft:nether_wastes",
                &[
                    ("minecraft:ghast", 4, 4, 50),
                    ("minecraft:zombified_piglin", 4, 4, 100),
                    ("minecraft:magma_cube", 4, 4, 2),
                    ("minecraft:enderman", 4, 4, 1),
                    ("minecraft:piglin", 4, 4, 15),
                ],
            ),
            (
                "minecraft:soul_sand_valley",
                &[
                    ("minecraft:skeleton", 5, 5, 20),
                    ("minecraft:ghast", 4, 4, 50),
                    ("minecraft:enderman", 4, 4, 1),
                ],
            ),
            (
                "minecraft:crimson_forest",
                &[
                    ("minecraft:zombified_piglin", 2, 4, 1),
                    ("minecraft:hoglin", 3, 4, 9),
                    ("minecraft:piglin", 3, 4, 5),
                ],
            ),
            (
                "minecraft:warped_forest",
                &[("minecraft:enderman", 4, 4, 1)],
            ),
            (
                "minecraft:basalt_deltas",
                &[
                    ("minecraft:ghast", 1, 1, 40),
                    ("minecraft:magma_cube", 2, 5, 100),
                ],
            ),
        ];
        let rules = NetherSpawnRules::vanilla();
        for (biome, monsters) in cases {
            let monsters_at =
                rules.spawners_at(biome, MobCategory::Monster, POS, |_| false, |_, _| false);
            assert_spawners(monsters_at, monsters);
            let creatures_at =
                rules.spawners_at(biome, MobCategory::Creature, POS, |_| false, |_, _| false);
            assert_spawners(creatures_at, &[("minecraft:strider", 1, 2, 60)]);
        }
        let empty = rules.spawners_at(
            "minecraft:nether_wastes",
            MobCategory::Ambient,
            POS,
            |_| false,
            |_, _| false,
        );
        assert_eq!(choose_nether_spawner(empty, no_roll), None);
    }

    #[test]
    fn nether_spawn_fortress_monster_overrides() {
        let rules = NetherSpawnRules::vanilla();
        let spawners = |category, bricks, structure, piece| {
            rules.spawners_at(
                "minecraft:warped_forest",
                category,
                POS,
                |pos| {
                    assert_eq!(pos, NaturalSpawnPos { y: 39, ..POS });
                    bricks
                },
                |pos, bounds| {
                    assert_eq!(pos, POS);
                    match bounds {
                        NetherFortressBounds::Structure => structure,
                        NetherFortressBounds::Piece => piece,
                    }
                },
            )
        };
        let fortress = [
            ("minecraft:blaze", 2, 3, 10),
            ("minecraft:zombified_piglin", 4, 4, 5),
            ("minecraft:wither_skeleton", 5, 5, 8),
            ("minecraft:skeleton", 5, 5, 2),
            ("minecraft:magma_cube", 4, 4, 3),
        ];
        for (bricks, structure, piece) in
            [(true, true, true), (true, true, false), (false, true, true)]
        {
            assert_spawners(
                spawners(MobCategory::Monster, bricks, structure, piece),
                &fortress,
            );
        }
        for (bricks, structure) in [(false, false), (true, false), (false, true)] {
            assert_spawners(
                spawners(MobCategory::Monster, bricks, structure, false),
                &[("minecraft:enderman", 4, 4, 1)],
            );
        }
        assert_spawners(
            spawners(MobCategory::Creature, true, true, true),
            &[("minecraft:strider", 1, 2, 60)],
        );
    }
}
