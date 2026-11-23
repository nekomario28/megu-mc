use super::*;
use crate::nbt::Tag;
use std::collections::BTreeMap;

#[derive(Default)]
struct ChunkCreatureScene {
    spawned: Vec<(String, Pos)>,
    checks: Vec<(CreaturePlacement, String, Pos)>,
    accept: bool,
}

impl ChunkGenerationWorld for ChunkCreatureScene {
    fn spawn_y(&mut self, _: &str, _: i32, _: i32) -> i32 { 72 }
    fn ground_spawnable(&mut self, kind: &str, pos: Pos) -> bool {
        self.checks.push((CreaturePlacement::Ground, kind.to_owned(), pos)); self.accept
    }
    fn water_spawnable(&mut self, kind: &str, pos: Pos) -> bool {
        self.checks.push((CreaturePlacement::Water, kind.to_owned(), pos)); self.accept
    }
    fn lava_spawnable(&mut self, kind: &str, pos: Pos) -> bool {
        self.checks.push((CreaturePlacement::Lava, kind.to_owned(), pos)); self.accept
    }
    fn air_spawnable(&mut self, kind: &str, pos: Pos) -> bool {
        self.checks.push((CreaturePlacement::Air, kind.to_owned(), pos)); self.accept
    }
    fn add_creature(&mut self, kind: &str, pos: Pos) { self.spawned.push((kind.to_owned(), pos)); }
}

fn seed_passing(probability: f32) -> u64 {
    (0..10_000).find(|seed| { let mut rng = Lcg::new(*seed); rng.next_f32() < probability })
        .expect("fixture seed passing probability")
}

fn plains_creature_spawners(biome: &str, category: &str) -> Vec<CreatureSpawnerData> {
    assert_eq!(biome, "minecraft:plains");
    assert_eq!(category, "creature");
    ["sheep", "pig", "chicken", "cow"].into_iter().map(|name| CreatureSpawnerData {
        kind: format!("minecraft:{name}"), min_count: 4, max_count: 4, weight: 10,
    }).collect()
}

#[test]
fn chunk_generation_creatures_use_deterministic_scatter_and_pack_bounds() {
    let _guard = crate::world_guard();
    let mut a = ChunkCreatureScene { accept: true, ..Default::default() };
    let mut b = ChunkCreatureScene { accept: true, ..Default::default() };
    let seed = seed_passing(0.2);
    let mut rng_a = Lcg::new(seed);
    let mut rng_b = Lcg::new(seed);
    spawn_mobs_for_chunk_generation(3, -2, "minecraft:plains", Some(0.2), plains_creature_spawners, &mut a, &mut rng_a);
    spawn_mobs_for_chunk_generation(3, -2, "minecraft:plains", Some(0.2), plains_creature_spawners, &mut b, &mut rng_b);
    assert!(!a.spawned.is_empty());
    assert_eq!(a.spawned, b.spawned);
    assert_eq!(a.checks, b.checks);
    assert_eq!(rng_a.state48(), rng_b.state48());
    assert!(a.spawned.iter().all(|(_, (x, y, z))| (-40..-8).contains(z) && (40..72).contains(x) && *y == 72));
    assert_eq!(a.spawned.len() % 4, 0, "plains packs contain four creatures");
}

#[test]
fn chunk_generation_creature_probability_gates_and_checks_ground_placement() {
    let _guard = crate::world_guard();
    let mut scene = ChunkCreatureScene { accept: true, ..Default::default() };
    let mut rng = Lcg::new(0x117);
    let before = rng.state48();
    spawn_mobs_for_chunk_generation(0, 0, "minecraft:plains", Some(0.0), plains_creature_spawners, &mut scene, &mut rng);
    assert!(scene.spawned.is_empty() && scene.checks.is_empty());
    assert_ne!(rng.state48(), before, "probability check consumes a float");

    let mut rejected = ChunkCreatureScene { accept: false, ..Default::default() };
    let mut rng = Lcg::new(seed_passing(0.2));
    spawn_mobs_for_chunk_generation(3, -2, "minecraft:plains", Some(0.2), plains_creature_spawners, &mut rejected, &mut rng);
    assert!(!rejected.checks.is_empty());
    assert!(rejected.checks.iter().all(|(family, _, _)| *family == CreaturePlacement::Ground));
    assert!(rejected.spawned.is_empty());
}

#[test]
fn chunk_generation_spawns_nether_creature_table_and_keeps_end_monsters_out() {
    let _guard = crate::world_guard();
    let nether_spawners = |biome: &str, category: &str| {
        assert_eq!(category, "creature");
        assert_eq!(biome, "minecraft:nether_wastes");
        vec![CreatureSpawnerData {
            kind: "minecraft:strider".to_owned(),
            min_count: 1,
            max_count: 2,
            weight: 60,
        }]
    };
    let mut nether = ChunkCreatureScene { accept: true, ..Default::default() };
    let seed = seed_passing(0.9);
    let mut rng = Lcg::new(seed);
    let mut serial_reference = ChunkCreatureScene { accept: true, ..Default::default() };
    let mut reference_rng = Lcg::new(seed);
    spawn_mobs_for_chunk_generation(
        -3, 5, "minecraft:nether_wastes", Some(0.9), nether_spawners, &mut nether, &mut rng,
    );
    spawn_mobs_for_chunk_generation(
        -3,
        5,
        "minecraft:nether_wastes",
        Some(0.9),
        nether_spawners,
        &mut serial_reference,
        &mut reference_rng,
    );
    assert!(!nether.spawned.is_empty());
    assert_eq!(nether.spawned, serial_reference.spawned);
    assert_eq!(nether.checks, serial_reference.checks);
    assert_eq!(rng.state48(), reference_rng.state48());
    assert!(nether.spawned.iter().all(|(kind, _)| kind == "minecraft:strider"));
    assert!(!nether.checks.is_empty());
    assert!(nether.checks.iter().all(|(placement, kind, _)| {
        *placement == CreaturePlacement::Lava && kind == "minecraft:strider"
    }));

    // Vanilla 1.21.1 `the_end` has no CREATURE entries. Its Endermen are in
    // MONSTER, which spawnMobsForChunkGeneration does not query.
    let end_spawners = |biome: &str, category: &str| {
        assert_eq!(biome, "minecraft:the_end");
        assert_eq!(category, "creature");
        Vec::new()
    };
    let mut end = ChunkCreatureScene { accept: true, ..Default::default() };
    let mut end_rng = Lcg::new(seed_passing(0.9));
    spawn_mobs_for_chunk_generation(
        7, -2, "minecraft:the_end", Some(0.9), end_spawners, &mut end, &mut end_rng,
    );
    assert!(end.spawned.is_empty());
    assert!(end.checks.is_empty());
}

fn compound(values: impl IntoIterator<Item = (&'static str, Tag)>) -> Tag {
    Tag::Compound(
        values
            .into_iter()
            .map(|(k, v)| (NbtString::text(k), v))
            .collect(),
    )
}

fn s(text: &str) -> Tag {
    Tag::String(NbtString::text(text))
}

fn build_spawner(
    delay: i16,
    spawn_count: i16,
    max_nearby: i16,
    fixed_pos: Option<[f64; 3]>,
    block_light_range: [i32; 2],
) -> Tag {
    let mut entity = BTreeMap::new();
    entity.insert(NbtString::text("id"), s("minecraft:cave_spider"));
    if let Some(pos) = fixed_pos {
        entity.insert(
            NbtString::text("Pos"),
            Tag::List(6, pos.into_iter().map(|v| Tag::Double(v.to_bits())).collect()),
        );
    }
    let rules = compound([
        (
            "block_light_limit",
            compound([
                ("min_inclusive", Tag::Int(block_light_range[0])),
                ("max_inclusive", Tag::Int(block_light_range[1])),
            ]),
        ),
        (
            "sky_light_limit",
            compound([("min_inclusive", Tag::Int(0)), ("max_inclusive", Tag::Int(15))]),
        ),
    ]);
    compound([
        ("Delay", Tag::Short(delay)),
        ("MinSpawnDelay", Tag::Short(200)),
        ("MaxSpawnDelay", Tag::Short(800)),
        ("SpawnCount", Tag::Short(spawn_count)),
        ("MaxNearbyEntities", Tag::Short(max_nearby)),
        ("RequiredPlayerRange", Tag::Short(16)),
        ("SpawnRange", Tag::Short(4)),
        ("SpawnPotentials", Tag::List(0, vec![])),
        (
            "SpawnData",
            compound([
                ("entity", Tag::Compound(entity)),
                ("custom_spawn_rules", rules),
            ]),
        ),
    ])
}

struct NativeScene {
    player_pos: [f64; 3],
    blocked: bool,
    spiders: Vec<[f64; 3]>,
    added: Vec<[f64; 3]>,
    effects: Vec<(Pos, [f64; 3])>,
}

impl NativeScene {
    fn aabb(pos: [f64; 3]) -> ([f64; 3], [f64; 3]) {
        (
            [pos[0] - 0.35, pos[1], pos[2] - 0.35],
            [pos[0] + 0.35, pos[1] + 0.5, pos[2] + 0.35],
        )
    }

    fn overlaps(a: ([f64; 3], [f64; 3]), b: ([f64; 3], [f64; 3])) -> bool {
        a.0[0] < b.1[0]
            && a.1[0] > b.0[0]
            && a.0[1] < b.1[1]
            && a.1[1] > b.0[1]
            && a.0[2] < b.1[2]
            && a.1[2] > b.0[2]
    }
}

impl World for NativeScene {
    fn active(&mut self, origin: Pos, range: i32) -> bool {
        let dx = self.player_pos[0] - (origin.0 as f64 + 0.5);
        let dy = self.player_pos[1] - (origin.1 as f64 + 0.5);
        let dz = self.player_pos[2] - (origin.2 as f64 + 0.5);
        let r = range as f64;
        dx * dx + dy * dy + dz * dz <= r * r
    }

    fn no_collision(&mut self, pos: [f64; 3]) -> bool {
        let (min, max) = Self::aabb(pos);
        // Stone floor covers y=200 ([200, 201)); optional blocked stone at (27, 201, 0)
        if min[1] < 201.0 && max[1] > 200.0 {
            return false;
        }
        if self.blocked {
            let block = ([27.0, 201.0, 0.0], [28.0, 202.0, 1.0]);
            if Self::overlaps((min, max), block) {
                return false;
            }
        }
        true
    }

    fn rules(&mut self, _pos: [f64; 3], custom: Option<LightRules>, _random: &mut Lcg) -> bool {
        // Scene block light = 0, sky light = 15
        match custom {
            Some(c) => (c.block[0]..=c.block[1]).contains(&0) && (c.sky[0]..=c.sky[1]).contains(&15),
            None => true,
        }
    }

    fn nearby(&mut self, origin: Pos, range: i32) -> usize {
        let box_min = [
            (origin.0 - range) as f64,
            (origin.1 - range) as f64,
            (origin.2 - range) as f64,
        ];
        let box_max = [
            (origin.0 + 1 + range) as f64,
            (origin.1 + 1 + range) as f64,
            (origin.2 + 1 + range) as f64,
        ];
        self.spiders
            .iter()
            .filter(|&&s| Self::overlaps(Self::aabb(s), (box_min, box_max)))
            .count()
    }

    fn unobstructed(&mut self, pos: [f64; 3]) -> bool {
        let bb = Self::aabb(pos);
        let player_bb = (
            [
                self.player_pos[0] - 0.3,
                self.player_pos[1],
                self.player_pos[2] - 0.3,
            ],
            [
                self.player_pos[0] + 0.3,
                self.player_pos[1] + 1.8,
                self.player_pos[2] + 0.3,
            ],
        );
        if Self::overlaps(bb, player_bb) {
            return false;
        }
        !self.spiders.iter().any(|&s| Self::overlaps(bb, Self::aabb(s)))
    }

    fn insert(&mut self, pos: [f64; 3], _yaw: f32) -> bool {
        self.spiders.push(pos);
        self.added.push(pos);
        true
    }

    fn effects(&mut self, origin: Pos, pos: [f64; 3]) {
        self.effects.push((origin, pos));
    }
}

#[cfg(megu_vanilla_data)]
#[test]
fn native_spawner_ticks_match_all_1211_cases() {
    let _guard = crate::world_guard();
    let tsv = include_str!("../tests/data/mineshaft_spawner_1211.tsv");
    let origin = (24, 201, 0);
    let mut persistent_spiders = Vec::new();
    let mut cases = 0;

    for line in tsv.lines().skip(1).filter(|l| !l.trim().is_empty()) {
        let fields: Vec<_> = line.split('\t').collect();
        assert_eq!(fields.len(), 6, "bad line: {line:?}");
        let mode = fields[0];
        let delay_before: i16 = fields[1].parse().unwrap();
        let delay_after: i32 = fields[2].parse().unwrap();
        let rng_before: u64 = fields[3].parse().unwrap();
        let rng_after: u64 = fields[4].parse().unwrap();
        let expected_spawned: Vec<[f64; 3]> = if fields[5] == "-" {
            Vec::new()
        } else {
            fields[5]
                .split(';')
                .map(|entry| {
                    let coords: Vec<f64> = entry.split(',').map(|x| x.parse().unwrap()).collect();
                    [coords[0], coords[1], coords[2]]
                })
                .collect()
        };

        let mut tag = build_spawner(
            delay_before,
            if mode == "random_positions" { 4 } else { 1 },
            if mode == "cap" { 0 } else { 6 },
            if mode == "random_positions" {
                None
            } else {
                Some([27.5, 201.0, 0.5])
            },
            if mode == "light_rejected" {
                [1, 1]
            } else {
                [0, 15]
            },
        );
        let mut scene = NativeScene {
            player_pos: if mode == "inactive" {
                [1000.0, 201.0, 0.0]
            } else {
                [26.0, 201.0, 0.0]
            },
            blocked: mode == "blocked",
            spiders: persistent_spiders.clone(),
            added: Vec::new(),
            effects: Vec::new(),
        };
        let mut rng = Lcg::from_raw_state(rng_before);
        tick(&mut tag, origin, &mut scene, &mut rng).unwrap();

        assert_eq!(short(&tag, "Delay"), delay_after, "mode={mode}");
        assert_eq!(rng.state48(), rng_after, "mode={mode}");
        assert_eq!(
            scene.added.len(),
            expected_spawned.len(),
            "mode={mode} added={:?}",
            scene.added
        );
        for (idx, (actual, expected)) in scene.added.iter().zip(&expected_spawned).enumerate() {
            for axis in 0..3 {
                assert!(
                    (actual[axis] - expected[axis]).abs() < 1e-12,
                    "mode={mode} spawn={idx} axis={axis}: {} != {}",
                    actual[axis],
                    expected[axis]
                );
            }
        }
        assert_eq!(scene.effects.len(), expected_spawned.len(), "mode={mode}");
        persistent_spiders = scene.spiders;
        cases += 1;
    }
    assert_eq!(cases, 8);
}

#[test]
fn spawner_fails_closed_on_unsupported_edited_nbt() {
    let _guard = crate::world_guard();
    struct DummyWorld;
    impl World for DummyWorld {
        fn active(&mut self, _: Pos, _: i32) -> bool {
            true
        }
        fn no_collision(&mut self, _: [f64; 3]) -> bool {
            true
        }
        fn rules(&mut self, _: [f64; 3], _: Option<LightRules>, _: &mut Lcg) -> bool {
            true
        }
        fn nearby(&mut self, _: Pos, _: i32) -> usize {
            0
        }
        fn unobstructed(&mut self, _: [f64; 3]) -> bool {
            true
        }
        fn insert(&mut self, _: [f64; 3], _: f32) -> bool {
            true
        }
        fn effects(&mut self, _: Pos, _: [f64; 3]) {}
    }

    let mut rng = Lcg::new(42);
    let mut world = DummyWorld;

    // Malformed SpawnPotentials: an entry without `data`.
    let mut potentials_tag = build_spawner(0, 1, 6, None, [0, 15]);
    if let Tag::Compound(map) = &mut potentials_tag {
        map.insert(
            NbtString::text("SpawnPotentials"),
            Tag::List(10, vec![compound([("weight", Tag::Int(1))])]),
        );
    }
    assert!(tick(&mut potentials_tag, (0, 0, 0), &mut world, &mut rng).is_err());

    // Invalid potential entries: non-positive weight, missing
    // `data.entity.id`, and an unsupported mob id.
    for potentials in [
        Tag::List(
            10,
            vec![compound([
                ("weight", Tag::Int(0)),
                (
                    "data",
                    compound([("entity", compound([("id", s("minecraft:zombie"))]))]),
                ),
            ]),
        ]),
        Tag::List(
            10,
            vec![compound([
                ("weight", Tag::Int(1)),
                ("data", compound([("entity", compound([]))])),
            ]),
        ]),
        Tag::List(
            10,
            vec![compound([
                ("weight", Tag::Int(1)),
                (
                    "data",
                    compound([(
                        "entity",
                        compound([("id", s("minecraft:unknown_mob"))]),
                    )]),
                ),
            ]),
        ]),
    ] {
        let mut tag = build_spawner(0, 1, 6, None, [0, 15]);
        if let Tag::Compound(map) = &mut tag {
            map.insert(NbtString::text("SpawnPotentials"), potentials);
        }
        assert!(tick(&mut tag, (0, 0, 0), &mut world, &mut rng).is_err());
    }

    // Unsupported spawner entity ids still fail closed.
    let mut wrong_mob = build_spawner(0, 1, 6, None, [0, 15]);
    if let Tag::Compound(map) = &mut wrong_mob {
        map.insert(
            NbtString::text("SpawnData"),
            compound([("entity", compound([("id", s("minecraft:unknown_mob"))]))]),
        );
    }
    assert!(tick(&mut wrong_mob, (0, 0, 0), &mut world, &mut rng).is_err());

    // Edited entity fields still fail closed (the allowlist is `id`/`Pos`).
    let mut extra_field = build_spawner(0, 1, 6, None, [0, 15]);
    if let Tag::Compound(map) = &mut extra_field {
        map.insert(
            NbtString::text("SpawnData"),
            compound([(
                "entity",
                compound([
                    ("id", s("minecraft:cave_spider")),
                    ("CustomName", s("bob")),
                ]),
            )]),
        );
    }
    assert!(tick(&mut extra_field, (0, 0, 0), &mut world, &mut rng).is_err());
}

#[test]
fn spawner_validates_general_1211_mobs_and_weighted_potentials() {
    let _guard = crate::world_guard();
    struct DummyWorld;
    impl World for DummyWorld {
        fn active(&mut self, _: Pos, _: i32) -> bool {
            true
        }
        fn no_collision(&mut self, _: [f64; 3]) -> bool {
            true
        }
        fn rules(&mut self, _: [f64; 3], _: Option<LightRules>, _: &mut Lcg) -> bool {
            true
        }
        fn nearby(&mut self, _: Pos, _: i32) -> usize {
            0
        }
        fn unobstructed(&mut self, _: [f64; 3]) -> bool {
            true
        }
        fn insert(&mut self, _: [f64; 3], _: f32) -> bool {
            true
        }
        fn effects(&mut self, _: Pos, _: [f64; 3]) {}
    }

    // Every supported 1.21.1 mob id validates as a direct `SpawnData` entity.
    for id in [
        "minecraft:zombie",
        "minecraft:skeleton",
        "minecraft:spider",
        "minecraft:cave_spider",
        "minecraft:blaze",
        "minecraft:magma_cube",
        "minecraft:silverfish",
        "minecraft:drowned",
        "minecraft:stray",
        "minecraft:husk",
        "minecraft:breeze",
        "minecraft:pig",
    ] {
        let mut tag = build_spawner(0, 1, 6, None, [0, 15]);
        if let Tag::Compound(map) = &mut tag {
            map.insert(
                NbtString::text("SpawnData"),
                compound([("entity", compound([("id", s(id))]))]),
            );
        }
        let mut world = DummyWorld;
        let mut rng = Lcg::new(7);
        assert!(tick(&mut tag, (0, 0, 0), &mut world, &mut rng).is_ok(), "{id}");
    }

    // A non-empty SpawnPotentials list validates, and the selected entry
    // becomes the active SpawnData (vanilla `BaseSpawner` re-picks
    // `spawnData` after a cycle).
    let mut tag = build_spawner(0, 1, 6, None, [0, 15]);
    let potentials = Tag::List(
        10,
        vec![
            compound([
                ("weight", Tag::Int(3)),
                (
                    "data",
                    compound([("entity", compound([("id", s("minecraft:blaze"))]))]),
                ),
            ]),
            compound([
                ("weight", Tag::Int(1)),
                (
                    "data",
                    compound([("entity", compound([("id", s("minecraft:zombie"))]))]),
                ),
            ]),
        ],
    );
    if let Tag::Compound(map) = &mut tag {
        map.insert(NbtString::text("SpawnPotentials"), potentials);
    }
    let mut world = DummyWorld;
    let mut rng = Lcg::new(7);
    assert!(tick(&mut tag, (0, 0, 0), &mut world, &mut rng).is_ok());
    let active = tag
        .get("SpawnData")
        .and_then(|d| d.get("entity"))
        .and_then(|e| e.get("id"))
        .and_then(Tag::string)
        .map(|v| v.to_utf8().unwrap_or_default());
    assert!(
        active == Some("minecraft:blaze".to_owned())
            || active == Some("minecraft:zombie".to_owned()),
        "selected SpawnData id: {active:?}"
    );
}

/// The live pass each generated chunk runs at most once, plus the daytime
/// animal, water and End spawn gates the drivers feed. Globals the live path
/// touches are swapped out and restored like `container_tests`' LiveWorld.
#[test]
fn chunk_gen_and_natural_spawning_cover_passive_aquatic_and_end() {
    let _guard = crate::world_guard();
    use crate::{DimId, MobCategory};
    use std::sync::atomic::Ordering;

    // This test rewrites shared spawn/world state; hold the world-test lock for its whole
    // body so a sibling mutating the same globals (or reading `world()`/`mobs()` under it)
    // cannot overlap. The swaps below alone leave the window between them unguarded.
    let mob_spawning = crate::GR_MOB_SPAWNING.swap(1, Ordering::Relaxed);
    let difficulty = crate::DIFFICULTY.swap(1, Ordering::Relaxed);
    let saved_visited = std::mem::take(&mut *crate::spawned_creature_chunks().lock().unwrap_or_else(|e| e.into_inner()));
    let saved_mobs = std::mem::take(&mut *crate::mobs().lock().unwrap_or_else(|e| e.into_inner()));
    let mut cells: Vec<(crate::DimBlockPos, u32)> = Vec::new();
    let mut place = |dim: DimId, x: i32, y: i32, z: i32, state: u32| {
        cells.push(((dim, x, y, z), crate::block_at_dim(dim, x, y, z)));
        crate::set_block_dim(dim, x, y, z, state);
    };
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        // First-visit creature herds: one pass per dimension+chunk.
        assert!(crate::live_chunkgen_visit(DimId::Overworld, 41, -17));
        assert!(
            !crate::live_chunkgen_visit(DimId::Overworld, 41, -17),
            "the chunk-generation creature pass must run once per chunk"
        );
        assert!(crate::spawned_creature_chunks()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains(&(DimId::Overworld, 41, -17)));

        // A forced pass on plains actually inserts live creature mobs: a
        // grass plateau across the chunk and its jitter margin is the herd's
        // ground regardless of the terrain generator underneath. Chunk
        // (43, -17) is chosen because its deterministic per-chunk seed draws
        // below the forced 0.9 probability - a chunk whose first draw clears
        // the vanilla `while nextFloat < probability` gate legitimately
        // spawns no herd (about one chunk in ten at 0.9).
        let grass = crate::name_to_state("grass_block").unwrap();
        let air = crate::name_to_state("air").unwrap_or(0);
        for x in (43 * 16 - 10)..=(43 * 16 + 25) {
            for z in (-17 * 16 - 10)..=(-17 * 16 + 25) {
                place(DimId::Overworld, x, 250, z, grass);
                place(DimId::Overworld, x, 251, z, air);
                place(DimId::Overworld, x, 252, z, air);
            }
        }
        let before = crate::mobs().lock().unwrap_or_else(|e| e.into_inner()).len();
        assert!(crate::live_chunkgen_run(
            DimId::Overworld,
            43,
            -17,
            Some(0.9),
        ));
        assert!(
            crate::mobs().lock().unwrap_or_else(|e| e.into_inner()).len() > before,
            "a high-probability creature pass should insert mobs"
        );

        // Daytime animals: grass below, clear standing room and light >= 9,
        // bounded by the creature cap.
        let (gx, gy, gz) = (70_000, 100, 70_000);
        let grass = crate::name_to_state("grass_block").unwrap();
        let air = crate::name_to_state("air").unwrap_or(0);
        place(DimId::Overworld, gx, gy - 1, gz, grass);
        place(DimId::Overworld, gx, gy, gz, air);
        place(DimId::Overworld, gx, gy + 1, gz, air);
        let creatures = crate::live_category_count(DimId::Overworld, MobCategory::Creature);
        assert!(
            crate::live_passive_spawn_attempt(gx, gy, gz, 15, creatures + 1, "sheep"),
            "a lit grass site should accept a daytime animal"
        );
        let sheep = crate::entity_spec_by_name("sheep").unwrap().id;
        assert!(crate::mobs().lock().unwrap_or_else(|e| e.into_inner()).iter().any(|mob| {
            mob.dim == DimId::Overworld && mob.etype == sheep
        }));
        assert!(
            !crate::live_passive_spawn_attempt(gx, gy, gz, 15, creatures + 1, "cow"),
            "the creature cap bounds the pass"
        );
        assert!(
            !crate::live_passive_spawn_attempt(gx, gy, gz, 8, creatures + 2, "cow"),
            "animals need light above 8"
        );

        // Aquatic mobs: a water cell inside the 50..=63 sea-level band under
        // the water-creature cap; outside the band or on dry land fails.
        let (wx, wy, wz) = (70_010, 55, 70_000);
        let water = crate::name_to_state("water").unwrap();
        place(DimId::Overworld, wx, wy, wz, water);
        assert!(crate::live_aquatic_spawn_attempt(
            wx,
            wy,
            wz,
            MobCategory::WaterCreature,
            usize::MAX,
            "squid"
        ));
        assert!(!crate::live_aquatic_spawn_attempt(
            wx,
            40,
            wz,
            MobCategory::WaterCreature,
            usize::MAX,
            "squid"
        ));
        assert!(crate::mobs().lock().unwrap_or_else(|e| e.into_inner()).iter().any(|mob| {
            mob.dim == DimId::Overworld
                && mob.etype == crate::entity_spec_by_name("squid").unwrap().id
        }));

        // End endermen: end_stone below, clear room, light <= 7, hostile cap
        // and a non-peaceful difficulty.
        let (ex, ey, ez) = (70_000, 100, 70_010);
        let end_stone = crate::name_to_state("end_stone").unwrap();
        place(DimId::End, ex, ey - 1, ez, end_stone);
        place(DimId::End, ex, ey, ez, air);
        place(DimId::End, ex, ey + 1, ez, air);
        assert!(crate::live_end_spawn_attempt(ex, ey, ez, 7, usize::MAX));
        assert!(crate::mobs().lock().unwrap_or_else(|e| e.into_inner()).iter().any(|mob| {
            mob.dim == DimId::End
                && mob.etype == crate::entity_spec_by_name("enderman").unwrap().id
        }));
        assert!(
            !crate::live_end_spawn_attempt(ex, ey, ez, 8, usize::MAX),
            "endermen need light 7 or darker"
        );
        assert!(
            !crate::live_end_spawn_attempt(ex, ey, ez, 7, 0),
            "the hostile cap bounds the End pass"
        );
        let peaceful = crate::DIFFICULTY.swap(0, Ordering::Relaxed);
        assert!(
            !crate::live_end_spawn_attempt(ex, ey, ez, 0, usize::MAX),
            "peaceful difficulty blocks enderman spawning"
        );
        crate::DIFFICULTY.store(peaceful, Ordering::Relaxed);

        // doMobSpawning = false blocks every category.
        crate::GR_MOB_SPAWNING.store(0, Ordering::Relaxed);
        assert!(
            !crate::live_passive_spawn_attempt(gx, gy, gz, 15, usize::MAX, "pig"),
            "doMobSpawning=false blocks animals"
        );
        assert!(
            !crate::live_aquatic_spawn_attempt(
                wx,
                wy,
                wz,
                MobCategory::WaterCreature,
                usize::MAX,
                "cod"
            ),
            "doMobSpawning=false blocks water mobs"
        );
        assert!(
            !crate::live_end_spawn_attempt(ex, ey, ez, 0, usize::MAX),
            "doMobSpawning=false blocks endermen"
        );
        crate::GR_MOB_SPAWNING.store(1, Ordering::Relaxed);
    }));
    for &((dim, x, y, z), state) in cells.iter().rev() {
        crate::set_block_dim(dim, x, y, z, state);
    }
    crate::GR_MOB_SPAWNING.store(mob_spawning, Ordering::Relaxed);
    crate::DIFFICULTY.store(difficulty, Ordering::Relaxed);
    *crate::spawned_creature_chunks().lock().unwrap_or_else(|e| e.into_inner()) = saved_visited;
    *crate::mobs().lock().unwrap_or_else(|e| e.into_inner()) = saved_mobs;
    if let Err(payload) = result {
        std::panic::resume_unwind(payload);
    }
}
