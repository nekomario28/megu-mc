use super::{freg, gen_cf, Cf, LargeDripstoneCfg};
use crate::block_registry::breg;
use crate::generation_context::FC;
use crate::generation_math::{mth_cos, mth_next_f32, mth_sin, normal_new_l, Xor};
use crate::generation_providers::{p_fp, p_hp, p_ip, test_pred, test_rt, Bsp, Pr, FP, RT};
use crate::json::{jparse, J};
use crate::worldgen::Gen;

fn context(g: &mut Gen) -> FC<'_> {
    FC::new(g)
}

#[test]
fn a_chunk_claims_its_carver_pass_once() {
    let mut g = Gen::load(0, crate::name_to_state).expect("datapack");
    assert!(g.begin_carve(1, 2), "the first pass is claimed");
    assert!(!g.begin_carve(1, 2), "the second pass is a no-op");
    assert!(g.begin_carve(1, 3), "a different chunk has its own claim");
}

/// A feature runs after the carver pass and must read the carver's air/fluid
/// writes, not the uncarved terrain. This is the lapis-in-a-cave regression.
#[test]
fn feature_context_sees_carver_writes() {
    let mut g = Gen::load(0, crate::name_to_state).expect("datapack");
    let cave_air = crate::name_to_state("minecraft:cave_air").expect("cave air");
    let (x, y, z) = (0, -60, 0);
    assert_ne!(g.terrain_state(x, y, z), cave_air);
    g.feat_write(x, y, z, cave_air);
    let mut ctx = context(&mut g);
    assert_eq!(ctx.get(x, y, z), cave_air);
}

/// Native canPlaceOre vectors pin both the decision and the following RNG draw.
/// In particular discard=1 means check all six neighbours, not reject all ore.
#[test]
fn ore_air_exposure_matches_native_decisions_and_rng() {
    let reg = breg().unwrap();
    let mut g = Gen::load(0, crate::name_to_state).expect("datapack");
    let stone = reg.state_of("minecraft:stone", None);
    let air = reg.state_of("minecraft:air", None);
    let rule = RT::Block("minecraft:stone".into());
    let mut ctx = context(&mut g);
    let rows = include_str!("../tests/data/ore_air_1211.tsv");
    let mut checked = 0;
    for line in rows.lines() {
        let fields: Vec<_> = line.split('\t').collect();
        assert_eq!(fields.len(), 6, "{line}");
        let seed = fields[0].parse().unwrap();
        let chance = fields[1].parse().unwrap();
        let exposed: i32 = fields[2].parse().unwrap();
        let matches: bool = fields[3].parse().unwrap();
        let expected: bool = fields[4].parse().unwrap();
        let following: i64 = fields[5].parse().unwrap();
        ctx.set(0, 16, 0, if matches { stone } else { air });
        for face in 0..6u8 {
            let (x, y, z) = crate::generation_providers::dir_off(face);
            ctx.set(
                x,
                16 + y,
                z,
                if exposed == face as i32 { air } else { stone },
            );
        }
        let mut rng = Xor::seed(seed);
        assert_eq!(
            super::ore_should_place(chance, &mut ctx, &rule, (0, 16, 0), &mut rng),
            expected,
            "native canPlaceOre decision: {line}"
        );
        assert_eq!(
            rng.next_long() as i64,
            following,
            "native following RNG: {line}"
        );
        checked += 1;
    }
    assert_eq!(
        checked, 126,
        "native exposure matrix must not be empty/truncated"
    );
}

/// Compare every dungeon write (including chest facing) and the following RNG
/// against the actual MonsterRoomFeature with real chest/spawner entities.
fn monster_room_volume(seed: u64, wrapped: bool) -> Gen {
    let reg = breg().unwrap();
    let mut dimensions = if wrapped {
        Xor::seed_worldgen(seed)
    } else {
        Xor::seed(seed)
    };
    let xr = dimensions.next_bound(2) + 2;
    let mut g = Gen::load(0, crate::name_to_state).expect("datapack");
    let stone = reg.state_of("minecraft:stone", None);
    let cave_air = reg.state_of("minecraft:cave_air", None);
    for x in -6..=6 {
        for y in 14..=20 {
            for z in -6..=6 {
                let hole = x == -xr - 1 && z == 0 && (y == 16 || y == 17);
                g.feat_write(x, y, z, if hole { cave_air } else { stone });
            }
        }
    }
    g
}

#[test]
fn monster_room_cells_and_rng_match_native() {
    let reg = breg().unwrap();
    let mut count = 0;
    let mut total_chests = 0;
    let mut wrapped_chests = 0;
    for row in include_str!("../tests/data/monster_room_1211.tsv").lines() {
        let f: Vec<_> = row.split('\t').collect();
        assert_eq!(f.len(), 7);
        let seed = f[0].parse().unwrap();
        let wrapped = match f[6] {
            "raw" => false,
            "worldgen" => true,
            other => panic!("unknown RNG: {other}"),
        };
        assert_eq!(f[1], "true", "native dungeon must place");
        let mut g = monster_room_volume(seed, wrapped);
        let mut ctx = context(&mut g);
        let mut rng = if wrapped {
            Xor::seed_worldgen(seed)
        } else {
            Xor::seed(seed)
        };
        gen_cf(&Cf::MonsterRoom, freg(), &mut ctx, &mut rng, (0, 16, 0));
        let mut cells = Vec::new();
        let mut chests = 0;
        for (&(x, y, z), &st) in &ctx.wr {
            let name = reg.name_of(st);
            let facing = if name == "minecraft:chest" {
                chests += 1;
                reg.get_prop(st, "facing").unwrap()
            } else {
                "-".into()
            };
            cells.push(format!("{x},{y},{z}:{name}:{facing}\n"));
        }
        cells.sort();
        let hash: String = crate::generation_math::md5(cells.concat().as_bytes())
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        assert_eq!(cells.len(), f[2].parse::<usize>().unwrap(), "writes: {row}");
        assert_eq!(chests, f[3].parse::<usize>().unwrap(), "chests: {row}");
        total_chests += chests;
        if wrapped {
            wrapped_chests += chests;
        }
        assert_eq!(hash, f[4], "native cell/facing set: {row}");
        assert_eq!(rng.next_i64(), f[5].parse::<i64>().unwrap(), "RNG: {row}");
        count += 1;
    }
    assert_eq!(count, 8);
    assert!(wrapped_chests > 0, "exercise wrapped nextLong loot seeds");
    assert!(
        total_chests > 0,
        "native fixture must exercise chest placement"
    );
}

#[test]
fn monster_room_preserves_protected_floor_and_centre() {
    let mut g = monster_room_volume(0, false);
    let bedrock = breg().unwrap().state_of("minecraft:bedrock", None);
    for y in [15, 16] {
        g.feat_write(0, y, 0, bedrock);
    }
    let mut ctx = context(&mut g);
    gen_cf(
        &Cf::MonsterRoom,
        freg(),
        &mut ctx,
        &mut Xor::seed(0),
        (0, 16, 0),
    );
    assert!(!ctx.wr.is_empty(), "fixture must reach room placement");
    for y in [15, 16] {
        assert_eq!(
            ctx.get(0, y, 0),
            bedrock,
            "Feature.safeSetBlock protection at {y}"
        );
        assert!(!ctx.wr.contains_key(&(0, y, 0)));
    }
}

#[test]
fn simple_block_places_both_halves_or_neither() {
    let reg = breg().unwrap();
    for blocked in [false, true] {
        let mut g = Gen::load(0, crate::name_to_state).unwrap();
        g.feat_write(0, 199, 0, reg.state_of("minecraft:grass_block", None));
        g.feat_write(0, 200, 0, 0);
        g.feat_write(
            0,
            201,
            0,
            if blocked {
                reg.state_of("minecraft:stone", None)
            } else {
                0
            },
        );
        let mut ctx = context(&mut g);
        let state = reg.state_of("minecraft:lilac", None);
        let cf = Cf::SimpleBlock {
            to: Bsp::Simple(state),
            on: vec![],
            in_: vec![],
            under: vec![],
        };
        let mut rng = Xor::seed_worldgen(11);
        let mut untouched = Xor::seed_worldgen(11);
        gen_cf(&cf, freg(), &mut ctx, &mut rng, (0, 200, 0));
        assert_eq!(rng.next_i64(), untouched.next_i64(), "no placement RNG");
        if blocked {
            assert!(ctx.wr.is_empty(), "do not leave a lower half");
        } else {
            assert_eq!(ctx.wr.len(), 2);
            assert_eq!(ctx.get(0, 200, 0), reg.with_prop(state, "half", "lower"));
            assert_eq!(ctx.get(0, 201, 0), reg.with_prop(state, "half", "upper"));
        }
    }
    let mut g = Gen::load(0, crate::name_to_state).unwrap();
    g.feat_write(0, 199, 0, reg.state_of("minecraft:clay", None));
    g.feat_write(0, 200, 0, reg.state_of("minecraft:water", None));
    g.feat_write(0, 201, 0, 0);
    let mut ctx = context(&mut g);
    let cf = Cf::SimpleBlock {
        to: Bsp::Simple(reg.state_of("minecraft:small_dripleaf", None)),
        on: vec![],
        in_: vec![],
        under: vec![],
    };
    gen_cf(
        &cf,
        freg(),
        &mut ctx,
        &mut Xor::seed_worldgen(11),
        (0, 200, 0),
    );
    assert_eq!(ctx.wr.len(), 2);
    assert_eq!(
        reg.get_prop(ctx.get(0, 200, 0), "waterlogged").as_deref(),
        Some("true")
    );
    assert_eq!(
        reg.get_prop(ctx.get(0, 201, 0), "waterlogged").as_deref(),
        Some("false")
    );
    assert!(!reg.is_double_plant(reg.state_of("minecraft:oak_stairs", None)));
}

#[test]
fn mushrooms_in_fresh_generation_need_the_special_substrate() {
    let reg = breg().unwrap();
    let mut g = Gen::load(0, crate::name_to_state).unwrap();
    let mut ctx = context(&mut g);
    // The native fixture's uninitialized sky light reads 15 even under trees.
    ctx.set(0, 202, 0, reg.state_of("minecraft:oak_leaves", None));
    for (soil, expected) in [
        ("dirt", false),
        ("grass_block", false),
        ("stone", false),
        ("basalt", false),
        ("podzol", true),
        ("mycelium", true),
    ] {
        ctx.set(0, 199, 0, reg.state_of(&format!("minecraft:{soil}"), None));
        for mushroom in ["minecraft:brown_mushroom", "minecraft:red_mushroom"] {
            assert_eq!(
                super::can_place(reg, &mut ctx, mushroom, 0, 200, 0),
                expected,
                "{soil} {mushroom}"
            );
        }
    }
}

#[test]
fn normal_cache_identity_survives_slot_reuse() {
    use crate::generation_math::normal_new_amps;
    let mut slot = Box::new(normal_new_amps(&mut Xor::seed(0), -4, &[1.0]));
    let address = &*slot as *const _ as usize;
    let first = slot.get(-982.0, -35.0, 520.0);
    let next = normal_new_amps(&mut Xor::seed(1), -4, &[1.0]);
    let expected = next.get(-982.0, -35.0, 520.0);
    assert_ne!(first.to_bits(), expected.to_bits());
    *slot = next;
    assert_eq!(&*slot as *const _ as usize, address, "reuse the same slot");
    assert_eq!(slot.get(-982.0, -35.0, 520.0).to_bits(), expected.to_bits());
}

#[test]
fn normal_noise_legacy_source_matches_native_create() {
    let cases: &[(i32, &[f64])] = &[
        (-4, &[1.0]),
        (-2, &[1.0, 0.5]),
        (-3, &[1.0, 0.0, 0.25]),
        (-2, &[0.0, 1.0]),
        (1, &[1.0]),
        (-2, &[0.0, 0.0]),
    ];
    let mut count = 0;
    for row in include_str!("../tests/data/normal_noise_legacy_source_1211.tsv").lines() {
        let f: Vec<_> = row.split('\t').collect();
        let (first, amps) = cases[f[1].parse::<usize>().unwrap()];
        let n = normal_new_l(f[0].parse().unwrap(), first, amps);
        let got = n.get(
            f[2].parse().unwrap(),
            f[3].parse().unwrap(),
            f[4].parse().unwrap(),
        );
        assert_eq!(
            got.to_bits(),
            u64::from_str_radix(f[5], 16).unwrap(),
            "native NormalNoise.create: {row}"
        );
        count += 1;
    }
    assert_eq!(count, 168);
}

/// Native OreFeature.place queried these 121 OCEAN_FLOOR_WG columns for
/// (-1003,71,513), then skipped its vein. A neighbour's canopy must not cause
/// the port to consume the 17 sphere-size draws that vanilla skipped.
#[test]
fn ore_uses_frozen_worldgen_floor() {
    let mut g = Gen::load(0, crate::name_to_state).expect("data pack");
    let reg = breg().expect("blocks");
    let fr = freg();
    let mut ctx = context(&mut g);
    // The actual failing trace read an earlier neighbour's oak leaf at y=70.
    ctx.set(-1003, 70, 513, reg.state_of("minecraft:oak_leaves", None));
    assert_eq!(ctx.hm_oc(-1003, 513), 71, "live floor sees the canopy");
    let mut columns = 0;
    for row in include_str!("../tests/data/ore_floor_wg_1211.tsv").lines() {
        let v: Vec<i32> = row.split('\t').map(|s| s.parse().unwrap()).collect();
        assert_eq!(ctx.hm_oc_wg(v[0], v[1]), v[2], "native WG floor at {row}");
        columns += 1;
    }
    assert_eq!(columns, 121);
    let mut expected = Xor::seed(0);
    expected.next_f32();
    expected.next_bound(3);
    expected.next_bound(3);
    let mut rng = Xor::seed(0);
    let ore = Cf::Ore {
        size: 17,
        discard: 0.5,
        targets: vec![(RT::True, reg.state_of("minecraft:coal_ore", None))],
        scattered: false,
    };
    gen_cf(&ore, fr, &mut ctx, &mut rng, (-1003, 71, 513));
    assert_eq!(
        rng.next_long(),
        expected.next_long(),
        "native early-out RNG"
    );
    assert_eq!(ctx.wr.len(), 1, "vein must not run above the frozen floor");
}

/// The actual patch_grass_forest origins from neighbouring chunk (-64,33).
/// Its 388 typed draws matched; only the parsed WORLD_SURFACE_WG height was
/// wrong (+5 from the canopy), losing a grass cell in the target chunk.
#[test]
fn grass_patch_keeps_worldgen_heightmap_through_the_modifier() {
    let mut g = Gen::load(0, crate::name_to_state).expect("data pack");
    let reg = breg().unwrap();
    let mut ctx = context(&mut g);
    let modifier = crate::generation_providers::p_mod(
        &jparse(br#"{"type":"minecraft:heightmap","heightmap":"WORLD_SURFACE_WG"}"#).unwrap(),
    )
    .unwrap();
    let pf = super::Pf {
        feat: super::FRef::Name(String::new()),
        mods: vec![],
        name: String::new(),
    };
    let mut rows = 0;
    for row in include_str!("../tests/data/grass_patch_floor_wg_1211.tsv").lines() {
        let v: Vec<i32> = row.split('\t').map(|s| s.parse().unwrap()).collect();
        ctx.set(
            v[0],
            v[1] + 4,
            v[2],
            reg.state_of("minecraft:oak_leaves", None),
        );
        assert_eq!(ctx.hm_ws(v[0], v[2]), v[1] + 5, "live canopy height");
        let mut out = Vec::new();
        super::mod_positions(
            &modifier,
            freg(),
            &mut ctx,
            &mut Xor::seed(0),
            (v[0], 0, v[2]),
            &pf,
            &mut out,
        );
        assert_eq!(out, vec![(v[0], v[1], v[2])], "native patch origin: {row}");
        rows += 1;
    }
    assert_eq!(rows, 2);
}

#[test]
fn multiface_cells_and_following_rng_match_native() {
    let reg = breg().unwrap();
    let air = reg.state_of("minecraft:air", None);
    let stone = reg.state_of("minecraft:stone", None);
    let water = reg.state_of("minecraft:water", None);
    let flowing = reg.with_prop(water, "level", "8");
    let leaves = reg.state_of("minecraft:oak_leaves", None);
    let slab = reg.state_of("minecraft:oak_slab", None);
    let lichen = reg.state_of("minecraft:glow_lichen", None);
    let mut seen = std::collections::HashSet::new();
    let mut spread_vectors = 0;
    for row in include_str!("../tests/data/multiface_1211.tsv").lines() {
        let f: Vec<_> = row.split('\t').collect();
        assert_eq!(f.len(), 7, "{row}");
        let kind: usize = f[0].parse().unwrap();
        assert!(kind < 10);
        let seed = f[1].parse::<i64>().unwrap() as u64;
        let chance: f32 = f[2].parse().unwrap();
        assert!(
            seen.insert((kind, seed, chance.to_bits())),
            "duplicate native case"
        );
        let mut g = Gen::load(0, crate::name_to_state).unwrap();
        // Native asserts that every world read stays inside this controlled volume.
        for x in -4i32..=4 {
            for dy in -4i32..=4 {
                for z in -4i32..=4 {
                    let state = if kind == 9 && (x, dy, z) == (0, 0, 0) {
                        lichen
                    } else if kind == 0 || kind == 7 {
                        if (x, dy, z) == (0, 0, 0) {
                            air
                        } else {
                            stone
                        }
                    } else if x
                        == match kind {
                            2 => 2,
                            3 => 3,
                            _ => 1,
                        }
                    {
                        match kind {
                            5 => leaves,
                            6 => slab,
                            _ => stone,
                        }
                    } else {
                        match kind {
                            4 => water,
                            8 => flowing,
                            _ => air,
                        }
                    };
                    g.feat_write(x, 200 + dy, z, state);
                }
            }
        }
        let on = match kind {
            5 => "oak_leaves",
            6 => "oak_slab",
            _ => "stone",
        };
        let floor = if kind == 7 {
            ",\"can_place_on_floor\":true"
        } else {
            ""
        };
        let json = format!(
            r#"{{"type":"multiface_growth","block":"minecraft:glow_lichen","search_range":20,"can_place_on_ceiling":true,"can_place_on_wall":true,"chance_of_spreading":{chance},"can_be_placed_on":["minecraft:{on}"]{floor}}}"#
        );
        let cf = super::p_cf(&jparse(json.as_bytes()).unwrap()).unwrap();
        let mut ctx = context(&mut g);
        let mut rng = Xor::seed_worldgen(seed);
        gen_cf(&cf, freg(), &mut ctx, &mut rng, (0, 200, 0));
        assert_eq!(
            !ctx.wr.is_empty(),
            f[3].parse::<bool>().unwrap(),
            "placed: {row}"
        );
        assert_eq!(
            ctx.wr.len(),
            f[4].parse::<usize>().unwrap(),
            "writes: {row}"
        );
        spread_vectors += usize::from(ctx.wr.len() > 1);
        let mut cells = Vec::new();
        for (&(x, y, z), &state) in &ctx.wr {
            assert_eq!(reg.name_of(state), "minecraft:glow_lichen");
            let mut mask = 0u8;
            for (i, p) in [
                "down",
                "up",
                "north",
                "south",
                "west",
                "east",
                "waterlogged",
            ]
            .iter()
            .enumerate()
            {
                if reg.get_prop(state, p).as_deref() == Some("true") {
                    mask |= 1 << i;
                }
            }
            cells.push(format!("{x},{},{z}:{mask}\n", y - 200));
        }
        cells.sort();
        let hash: String = crate::generation_math::md5(cells.concat().as_bytes())
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        assert_eq!(hash, f[6], "cell/faces: {row}");
        assert_eq!(
            rng.next_i64(),
            f[5].parse::<i64>().unwrap(),
            "following RNG: {row}"
        );
    }
    assert_eq!(seen.len(), 120);
    assert!(spread_vectors > 0, "exercise spreading into another cell");
}

#[test]
fn matching_fluids_reads_the_fluid_state_not_the_block_name() {
    let mut g = Gen::load(0, crate::name_to_state).unwrap();
    let reg = breg().unwrap();
    let water = reg.state_of("minecraft:water", None);
    let lava = reg.state_of("minecraft:lava", None);
    let stairs = reg.state_of("minecraft:oak_stairs", None);
    let mut ctx = context(&mut g);
    for (state, expected) in [
        (reg.state_of("minecraft:stone", None), "minecraft:empty"),
        (water, "minecraft:water"),
        (
            reg.with_prop(water, "level", "8"),
            "minecraft:flowing_water",
        ),
        (reg.state_of("minecraft:seagrass", None), "minecraft:water"),
        (
            reg.state_of("minecraft:tall_seagrass", None),
            "minecraft:water",
        ),
        (reg.state_of("minecraft:kelp", None), "minecraft:water"),
        (
            reg.with_prop(stairs, "waterlogged", "true"),
            "minecraft:water",
        ),
        (
            reg.with_prop(stairs, "waterlogged", "false"),
            "minecraft:empty",
        ),
        (lava, "minecraft:lava"),
        (reg.with_prop(lava, "level", "1"), "minecraft:flowing_lava"),
    ] {
        ctx.set(0, 200, 0, state);
        for fluid in [
            "minecraft:empty",
            "minecraft:water",
            "minecraft:flowing_water",
            "minecraft:lava",
            "minecraft:flowing_lava",
        ] {
            let json = format!(r#"{{"type":"matching_fluids","fluids":"{fluid}"}}"#);
            let predicate =
                crate::generation_providers::p_pred(&jparse(json.as_bytes()).unwrap()).unwrap();
            assert_eq!(
                test_pred(&predicate, reg, &mut ctx, 0, 200, 0),
                fluid == expected,
                "state={state}, wanted={fluid}"
            );
        }
    }
}

#[test]
fn surface_relative_filter_keeps_its_heightmap_and_long_bounds() {
    let mut g = Gen::load(0, crate::name_to_state).unwrap();
    let reg = breg().unwrap();
    let mut ctx = context(&mut g);
    let (x, z) = (-1003, 513);
    let floor = ctx.hm_oc_wg(x, z);
    ctx.set(x, 200, z, reg.state_of("minecraft:stone", None));
    assert_eq!(ctx.hm_ws(x, z), 201);
    let pf = super::Pf {
        feat: super::FRef::Name(String::new()),
        mods: vec![],
        name: String::new(),
    };
    for (min, max, y, accept) in [
        (i32::MIN, -13, floor - 13, true),
        (i32::MIN, -13, floor - 12, false),
        (i32::MAX, i32::MAX, i32::MAX, false),
    ] {
        let json = format!(
            r#"{{"type":"surface_relative_threshold_filter","heightmap":"OCEAN_FLOOR_WG","min_inclusive":{min},"max_inclusive":{max}}}"#
        );
        let modifier =
            crate::generation_providers::p_mod(&jparse(json.as_bytes()).unwrap()).unwrap();
        let mut out = Vec::new();
        super::mod_positions(
            &modifier,
            freg(),
            &mut ctx,
            &mut Xor::seed(0),
            (x, y, z),
            &pf,
            &mut out,
        );
        assert_eq!(!out.is_empty(), accept, "{json}, y={y}");
    }
}

#[test]
fn heightmap_types_keep_their_predicates_and_removed_tops() {
    use crate::block_registry::HeightmapKind as H;
    let reg = breg().unwrap();
    for (name, expected) in [
        ("minecraft:short_grass", [true, false, false, false]),
        ("minecraft:water", [true, false, true, true]),
        ("minecraft:oak_leaves", [true, true, true, false]),
        ("minecraft:stone", [true, true, true, true]),
    ] {
        let state = reg.state_of(name, None);
        for (kind, counts) in [
            H::WorldSurface,
            H::OceanFloor,
            H::MotionBlocking,
            H::MotionBlockingNoLeaves,
        ]
        .into_iter()
        .zip(expected)
        {
            assert_eq!(reg.heightmap_counts(state, kind), counts, "{name} {kind:?}");
        }
    }
    assert!(!reg.is_solid_render(reg.state_of("minecraft:tinted_glass", None)));
    let mut g = Gen::load(0, crate::name_to_state).expect("data pack");
    let mut ctx = context(&mut g);
    for (y, name) in [
        (220, "stone"),
        (221, "oak_leaves"),
        (222, "water"),
        (223, "short_grass"),
    ] {
        ctx.set(0, y, 0, reg.state_of(&format!("minecraft:{name}"), None));
    }
    for (kind, expected) in [
        (H::WorldSurface, 224),
        (H::OceanFloor, 222),
        (H::MotionBlocking, 223),
        (H::MotionBlockingNoLeaves, 223),
    ] {
        assert_eq!(ctx.heightmap(kind, 0, 0), expected, "{kind:?}");
    }
    // A maximum is a scan bound, not a permanent answer after another writer.
    ctx.set(0, 223, 0, 0);
    ctx.set(0, 222, 0, 0);
    assert_eq!(ctx.heightmap(H::WorldSurface, 0, 0), 222);
    assert_eq!(ctx.heightmap(H::MotionBlockingNoLeaves, 0, 0), 221);
    ctx.set(0, 221, 0, 0);
    assert_eq!(ctx.hm_oc(0, 0), 221);
    assert_eq!(ctx.hm_ws(0, 0), 221);
    for name in [
        "WORLD_SURFACE_WG",
        "WORLD_SURFACE",
        "OCEAN_FLOOR_WG",
        "OCEAN_FLOOR",
        "MOTION_BLOCKING",
        "MOTION_BLOCKING_NO_LEAVES",
    ] {
        let parsed = crate::generation_providers::p_mod(
            &jparse(format!(r#"{{"type":"heightmap","heightmap":"{name}"}}"#).as_bytes()).unwrap(),
        )
        .unwrap();
        assert!(
            matches!(parsed, crate::generation_providers::PM::Heightmap(h) if h == H::parse(name).unwrap())
        );
    }
    assert!(crate::generation_providers::p_mod(
        &jparse(br#"{"type":"heightmap","heightmap":"UNKNOWN_FLOOR"}"#).unwrap()
    )
    .is_none());
}

/// Pin how much randomness a provider consumed, without pinning platform libm's
/// last bit: draw one Gaussian (so its spare is populated and later consumed),
/// then record two raw draws. A provider that draws a different number of times
/// moves the following longs and changes the fixture.
fn follow(trace: &mut Vec<u8>, r: &mut Xor) {
    let _ = r.next_gauss();
    trace.extend_from_slice(&r.next_long().to_le_bytes());
    trace.extend_from_slice(&r.next_long().to_le_bytes());
}

#[test]
fn kelp_feature_grows_up_from_the_ocean_floor_and_places_one_head() {
    let reg = breg().unwrap();
    let mut g = Gen::load(0, crate::name_to_state).expect("datapack");
    let mut ctx = context(&mut g);
    let (x, z) = (1280, 0); // sampled as lukewarm_ocean for seed 0
    let base = ctx.hm_oc(x, z);
    let sand = reg.state_of("minecraft:sand", None);
    let water = reg.state_of("minecraft:water", None);
    ctx.set(x, base - 1, z, sand);
    for y in base..=base + 20 {
        ctx.set(x, y, z, water);
    }

    let mut rng = Xor::seed(0);
    gen_cf(&Cf::Kelp, freg(), &mut ctx, &mut rng, (x, 0, z));

    let plant = reg.state_of("minecraft:kelp_plant", None);
    let placed_heads: Vec<_> = ctx
        .wr
        .iter()
        .filter_map(|(&(px, y, pz), &st)| {
            (px == x && pz == z && reg.name_of(st) == "minecraft:kelp").then_some((y, st))
        })
        .collect();
    assert_eq!(
        placed_heads.len(),
        1,
        "KelpFeature must place exactly one terminal head"
    );
    let (top, head_state) = placed_heads[0];
    let age = reg
        .get_prop(head_state, "age")
        .unwrap()
        .parse::<u8>()
        .unwrap();
    assert!(
        (20..=23).contains(&age),
        "head age must come from nextInt(4)+20"
    );
    assert_eq!(reg.name_of(ctx.get(x, top, z)), "minecraft:kelp");
    assert!(
        top > base,
        "the kelp head must be above its OCEAN_FLOOR origin"
    );
    for y in base..top {
        assert_eq!(
            ctx.get(x, y, z),
            plant,
            "kelp stem must be continuous at y={y}"
        );
    }
}

#[test]
fn kelp_stops_at_the_water_surface_and_replaces_the_last_plant_with_a_head() {
    let reg = breg().unwrap();
    let mut g = Gen::load(0, crate::name_to_state).expect("datapack");
    let mut ctx = context(&mut g);
    let (x, z) = (1280, 0);
    let base = ctx.hm_oc(x, z);
    ctx.set(x, base - 1, z, reg.state_of("minecraft:sand", None));
    let water = reg.state_of("minecraft:water", None);
    ctx.set(x, base, z, water);
    ctx.set(x, base + 1, z, water);
    ctx.set(x, base + 2, z, 0);

    let mut rng = Xor::seed(0);
    gen_cf(&Cf::Kelp, freg(), &mut ctx, &mut rng, (x, 0, z));

    assert_eq!(reg.name_of(ctx.get(x, base, z)), "minecraft:kelp");
    assert_eq!(reg.name_of(ctx.get(x, base + 1, z)), "minecraft:water");
    let age = reg
        .get_prop(ctx.get(x, base, z), "age")
        .unwrap()
        .parse::<u8>()
        .unwrap();
    assert!((20..=23).contains(&age));
}

#[test]
fn kelp_places_its_head_when_the_column_is_one_water_cell_deep() {
    // `KelpFeature.place` moves the position up on *every* iteration - `pos.move(UP)`
    // sits outside its if/else - so a base cell that is water with air above it
    // rejects at i=0, then rejects again at i=1 as the head branch, and the head
    // lands on the base cell itself. A loop that only advances on a placement
    // examines the base cell twice and writes the head one cell lower, into the
    // floor, where `canSurvive` refuses it and nothing is placed at all.
    let reg = breg().unwrap();
    let mut g = Gen::load(0, crate::name_to_state).expect("datapack");
    let mut ctx = context(&mut g);
    let (x, z) = (1280, 0);
    let base = ctx.hm_oc(x, z);
    ctx.set(x, base - 1, z, reg.state_of("minecraft:sand", None));
    ctx.set(x, base, z, reg.state_of("minecraft:water", None));
    ctx.set(x, base + 1, z, 0);

    let mut rng = Xor::seed(0);
    gen_cf(&Cf::Kelp, freg(), &mut ctx, &mut rng, (x, 0, z));

    let written: Vec<_> = ctx
        .wr
        .iter()
        .filter(|(&(px, _, pz), &st)| {
            px == x
                && pz == z
                && matches!(reg.name_of(st), "minecraft:kelp" | "minecraft:kelp_plant")
        })
        .map(|(&(_, y, _), &st)| (y, reg.name_of(st).to_string()))
        .collect();
    assert_eq!(
        written,
        vec![(base, "minecraft:kelp".to_string())],
        "one head on the base cell: no stem above it, nothing written into the floor"
    );
}

#[test]
fn provider_values_and_following_rng_match_pre_extraction() {
    let mut trace = Vec::new();
    let ints = [
        "3",
        r#"{"type":"constant","value":-7}"#,
        r#"{"type":"uniform","min_inclusive":-3,"max_inclusive":7}"#,
        r#"{"type":"uniform","min_inclusive":4,"max_inclusive":4}"#,
        r#"{"type":"biased_to_bottom","min_inclusive":-3,"max_inclusive":7}"#,
        r#"{"type":"very_biased_to_bottom","min_inclusive":-3,"max_inclusive":7,"inner":2}"#,
        r#"{"type":"clamped","source":{"type":"uniform","min":-10,"max":10},"min_inclusive":-2,"max_inclusive":2}"#,
        r#"{"type":"trapezoid","min":-6,"max":8,"plateau":2}"#,
        r#"{"type":"trapezoid","min":2,"max":3,"plateau":4}"#,
        r#"{"type":"clamped_normal","mean":2,"deviation":3,"min_inclusive":-4,"max_inclusive":6}"#,
        r#"{"type":"weighted_list","distribution":[{"weight":-1,"data":99},{"weight":1,"data":4},{"weight":3,"data":{"type":"uniform","min":8,"max":12}}]}"#,
        r#"{"type":"weighted_list","distribution":[]}"#,
    ];
    for seed in [0, 7, u64::MAX] {
        for text in ints {
            let p = p_ip(&jparse(text.as_bytes()).unwrap()).unwrap();
            let mut r = Xor::seed(seed);
            for _ in 0..4 {
                trace.extend_from_slice(&p.get(&mut r).to_le_bytes());
                follow(&mut trace, &mut r);
            }
        }
        for text in [
            r#"{"type":"constant","value":{"above_bottom":5}}"#,
            r#"{"type":"uniform","min_inclusive":{"absolute":-12},"max_inclusive":{"below_top":3}}"#,
            r#"{"type":"trapezoid","min_inclusive":{"above_bottom":6},"max_inclusive":{"below_top":7}}"#,
            r#"{"type":"very_biased_to_bottom","min_inclusive":-3,"max_inclusive":15}"#,
            r#"{"type":"trapezoid","min_inclusive":8,"max_inclusive":2}"#,
            r#"{"type":"very_biased_to_bottom","min_inclusive":8,"max_inclusive":2}"#,
        ] {
            let p = p_hp(&jparse(text.as_bytes()).unwrap()).unwrap();
            let mut r = Xor::seed(seed);
            for _ in 0..4 {
                trace.extend_from_slice(&p.get(&mut r, -64, 384).to_le_bytes());
                follow(&mut trace, &mut r);
            }
        }
    }
    let reg = breg().unwrap();
    let stone = reg.state_of("stone", None);
    let dirt = reg.state_of("dirt", None);
    let water = reg.state_of("water", None);
    let log = reg.state_of("oak_log", None);
    let wheat = reg.state_of("wheat", None);
    let noise = || normal_new_l(11, -2, &[1.0, 0.5]);
    let blocks = vec![
        Bsp::Simple(stone),
        Bsp::Rotated {
            base: log,
            axis: true,
        },
        Bsp::Rotated {
            base: stone,
            axis: false,
        },
        Bsp::Weighted(vec![(-1, water), (1, dirt), (3, stone)]),
        Bsp::Weighted(vec![]),
        Bsp::Weighted(vec![(-1, dirt)]),
        Bsp::RandomInt {
            src: Box::new(Bsp::Simple(wheat)),
            prop: "age".into(),
            vals: vec![0, 3, 7],
        },
        Bsp::RandomInt {
            src: Box::new(Bsp::Simple(wheat)),
            prop: "age".into(),
            vals: vec![],
        },
        Bsp::Noise {
            n: noise(),
            scale: 0.2,
            states: vec![stone, dirt, water],
        },
        Bsp::Noise {
            n: noise(),
            scale: 0.2,
            states: vec![],
        },
        Bsp::NoiseThr {
            n: noise(),
            scale: 0.2,
            thr: f32::INFINITY,
            hchance: 1.0,
            def: stone,
            low: vec![dirt, water],
            high: vec![log],
        },
        Bsp::NoiseThr {
            n: noise(),
            scale: 0.2,
            thr: f32::INFINITY,
            hchance: 1.0,
            def: stone,
            low: vec![],
            high: vec![log],
        },
        Bsp::NoiseThr {
            n: noise(),
            scale: 0.2,
            thr: f32::NEG_INFINITY,
            hchance: 1.0,
            def: stone,
            low: vec![],
            high: vec![log, dirt],
        },
        Bsp::NoiseThr {
            n: noise(),
            scale: 0.2,
            thr: f32::NEG_INFINITY,
            hchance: 0.0,
            def: stone,
            low: vec![],
            high: vec![log],
        },
        Bsp::NoiseThr {
            n: noise(),
            scale: 0.2,
            thr: f32::NEG_INFINITY,
            hchance: 1.0,
            def: stone,
            low: vec![],
            high: vec![],
        },
        Bsp::Dual {
            n: noise(),
            scale: 0.2,
            variety: (1, 3),
            slow_scale: 0.03,
            slow_variety: (1, 2),
            states: vec![stone, dirt, water, log, wheat, 0],
        },
        Bsp::Dual {
            n: noise(),
            scale: 0.2,
            variety: (1, 3),
            slow_scale: 0.03,
            slow_variety: (1, 2),
            states: vec![],
        },
        Bsp::RuleBased {
            fb: Some(Box::new(Bsp::Simple(dirt))),
            rules: vec![
                (Pr::Not(Box::new(Pr::True)), Bsp::Simple(log)),
                (Pr::True, Bsp::Weighted(vec![(1, stone), (2, water)])),
            ],
        },
        Bsp::RuleBased {
            fb: Some(Box::new(Bsp::Simple(dirt))),
            rules: vec![],
        },
        Bsp::RuleBased {
            fb: None,
            rules: vec![],
        },
    ];
    let mut g = Gen::load(0, crate::name_to_state).unwrap();
    let mut ctx = context(&mut g);
    ctx.set(0, 100, 0, stone);
    for seed in [0, 7, u64::MAX] {
        for p in &blocks {
            let mut r = Xor::seed(seed);
            for (x, y, z) in [(0, 100, 0), (-3, 101, 2), (16, -63, -17)] {
                trace.extend_from_slice(&p.get(&mut ctx, &mut r, x, y, z).to_le_bytes());
                follow(&mut trace, &mut r);
            }
        }
    }
    for st in [stone, dirt, water, 0] {
        ctx.set(0, 100, 0, st);
        ctx.set(0, 99, 0, dirt);
        for p in [
            Pr::True,
            Pr::Blocks(vec!["minecraft:stone".into()]),
            Pr::Tag("dirt".into()),
            Pr::Fluids(vec!["minecraft:water".into()]),
            Pr::Solid,
            Pr::Replaceable,
            Pr::WouldSurvive("minecraft:oak_sapling".into()),
            Pr::InsideWB,
            Pr::HasSturdy([0, -1, 0], 1),
            Pr::Any(vec![]),
            Pr::All(vec![]),
            Pr::Any(vec![Pr::Solid, Pr::True]),
            Pr::All(vec![Pr::Solid, Pr::Not(Box::new(Pr::Replaceable))]),
        ] {
            trace.push(u8::from(test_pred(&p, reg, &mut ctx, 0, 100, 0)));
        }
    }
    for y in [
        ctx.g.min_y - 1,
        ctx.g.min_y,
        ctx.g.min_y + ctx.g.height - 1,
        ctx.g.min_y + ctx.g.height,
    ] {
        trace.push(u8::from(test_pred(&Pr::InsideWB, reg, &mut ctx, 0, y, 0)));
    }
    ctx.set(0, 319, 0, 0);
    trace.push(u8::from(test_pred(
        &Pr::Unobstructed,
        reg,
        &mut ctx,
        0,
        318,
        0,
    )));
    ctx.set(0, 319, 0, stone);
    trace.push(u8::from(test_pred(
        &Pr::Unobstructed,
        reg,
        &mut ctx,
        0,
        318,
        0,
    )));
    // A solid block several cells above must still be found: the loop has to walk,
    // not only inspect the first cell. This stays out of `trace`, so the frozen
    // fixture above is unchanged.
    let base = ctx.g.min_y;
    for y in base + 2..base + 5 {
        ctx.set(0, y, 0, 0);
    }
    ctx.set(0, base + 5, 0, stone);
    assert!(!test_pred(&Pr::Unobstructed, reg, &mut ctx, 0, base + 1, 0));
    for seed in [0, 7, u64::MAX] {
        for rt in [
            RT::True,
            RT::Tag("dirt".into()),
            RT::Block("minecraft:stone".into()),
            RT::BlockState(stone),
            RT::Any(vec![RT::BlockState(dirt), RT::True]),
            RT::All(vec![RT::True, RT::Not(Box::new(RT::BlockState(dirt)))]),
            RT::Height(90, 110),
            RT::RandBlock("minecraft:stone".into(), 0.5),
            RT::RandBlock("minecraft:dirt".into(), 1.0),
            RT::RandBlockState(stone, 0.5),
            RT::RandBlockState(dirt, 1.0),
        ] {
            let mut r = Xor::seed(seed);
            trace.push(u8::from(test_rt(&rt, reg, stone, 100, &mut r)));
            follow(&mut trace, &mut r);
        }
    }
    // `UPDATE_BASELINE=1` re-records the fixture after a *verified* correction
    // (height providers and legacy-source NormalNoise have independent native vectors).
    if std::env::var("UPDATE_BASELINE").is_ok() {
        std::fs::write(
            "evidence/foundation/module-layers/provider-baseline.bin",
            &trace,
        )
        .unwrap();
    }
    let expected = std::fs::read("evidence/foundation/module-layers/provider-baseline.bin")
        .expect("the pre-extraction provider/context baseline fixture");
    assert_eq!(
        trace, expected,
        "provider/predicate values or their RNG consumption changed from the pre-extraction baseline"
    );
}

#[test]
fn context_overrides_heights_bounds_and_biomes_track_live_state() {
    let mut g = Gen::load(0, crate::name_to_state).unwrap();
    let reg = breg().unwrap();
    let stone = reg.state_of("stone", None);
    let water = reg.state_of("water", None);
    let x = -1;
    let z = -1;
    let ws = g.top_ws(x, z);
    let oc = g.top_oc(x, z);
    let y = ws.max(oc) + 2;
    assert!(y + 3 < g.min_y + g.height);
    let original = g.terrain_state(x, y, z);
    let biome = g.biome_name_at(x, y, z);
    let other_biome = g.biome_name_at(16, y, 16);
    let mut ctx = context(&mut g);
    assert_eq!(ctx.get(x, y, z), original);
    assert_eq!(ctx.get(x, y, z), original);
    assert_eq!(ctx.hm_ws(x, z), ws);
    assert_eq!(ctx.hm_oc(x, z), oc);
    ctx.set(x, y, z, stone);
    assert_eq!(ctx.get(x, y, z), stone);
    assert_eq!(ctx.hm_ws(x, z), y + 1);
    assert_eq!(ctx.hm_oc(x, z), y + 1);
    ctx.set(x, y + 2, z, water);
    assert_eq!(ctx.hm_ws(x, z), y + 3);
    assert_eq!(ctx.hm_oc(x, z), y + 1);
    ctx.set(x, y + 2, z, 0);
    assert_eq!(ctx.get(x, y + 2, z), 0);
    // Removing the water top reveals the stone again, like native Heightmap.update.
    assert_eq!(ctx.hm_ws(x, z), y + 1);
    assert_eq!(ctx.hm_oc(x, z), y + 1);
    assert_eq!(ctx.biome(x, y, z), biome);
    assert_eq!(ctx.biome(x, y, z), biome);
    assert_eq!(ctx.biome(16, y, 16), other_biome);
    assert!(ctx.out_of_h(ctx.g.min_y - 1));
    assert!(!ctx.out_of_h(ctx.g.min_y));
    assert!(!ctx.out_of_h(ctx.g.min_y + ctx.g.height - 1));
    assert!(ctx.out_of_h(ctx.g.min_y + ctx.g.height));
}

/// The carver configs are read with the 1.21.1 field shapes, and two of them are
/// easy to get subtly wrong: `yScale` is a bare number in `canyon.json` (the codec
/// is `either(float, FloatProvider)`), and `replaceable` carries the `#` tag
/// marker in the file but is looked up without it.
#[test]
fn carver_configs_parse_the_1_21_1_field_shapes() {
    use crate::generation_math::Lcg;

    let reg = super::creg();
    let cave = reg.cfg.get("minecraft:cave").expect("cave config");
    assert_eq!(cave.probability, 0.15);
    assert_eq!(cave.replaceable, "minecraft:overworld_carver_replaceables");
    assert_eq!(
        cave.kind,
        super::CKind::Cave,
        "the cave type must not become another algorithm"
    );
    // y is uniform(above_bottom 8 .. absolute 180) and the world's min y is -64
    let mut r = Lcg::new(7);
    let (mut lo, mut hi) = (i32::MAX, i32::MIN);
    for _ in 0..2000 {
        let v = cave.y.sample(&mut r, -64, 384);
        lo = lo.min(v);
        hi = hi.max(v);
    }
    assert!(lo >= -56, "y never below the above_bottom anchor: {lo}");
    assert!(hi <= 180, "y never above the absolute anchor: {hi}");
    assert!(lo < 0 && hi > 100, "the range is exercised: {lo}..{hi}");

    // canyon.json's yScale is a plain number, not a provider object
    let canyon = reg.cfg.get("minecraft:canyon").expect("canyon config");
    assert_eq!(canyon.y_scale.sample(&mut Lcg::new(0)), 3.0);
    assert!(canyon.canyon.is_some(), "the canyon shape must parse");
    // and the biome list order is what the carver seed index counts
    let forest = reg.biome.get("minecraft:forest").expect("forest carvers");
    assert_eq!(
        forest,
        &vec![
            "minecraft:cave".to_string(),
            "minecraft:cave_extra_underground".to_string(),
            "minecraft:canyon".to_string()
        ]
    );
}

/// Every biome's AIR carver list, in the order the carver seed index counts,
/// against the pinned jar's own `BiomeGenerationSettings`. The list is written as
/// `{"air": [...]}` in 1.21.1, not as a bare array.
#[test]
fn carver_lists_match_the_pinned_biomes() {
    let fixture = jparse(include_bytes!(
        "../evidence/foundation/biome-carvers/vanilla.json"
    ))
    .unwrap();
    let J::Obj(biomes) = fixture.get("biomes").unwrap() else {
        panic!("biomes must be an object");
    };
    let reg = super::creg();
    let mut nonempty = 0;
    for (name, list) in biomes {
        let want: Vec<String> = list
            .as_arr()
            .expect("carver list")
            .iter()
            .map(|v| v.str().to_string())
            .collect();
        assert_eq!(reg.biome.get(name), Some(&want), "carvers for {name}");
        nonempty += usize::from(!want.is_empty());
    }
    assert_eq!(biomes.len(), 64, "pinned biome coverage changed");
    assert_eq!(nonempty, 58, "non-empty carver coverage changed");
    assert_eq!(
        reg.biome.get("minecraft:forest").map(Vec::as_slice),
        Some(
            [
                "minecraft:cave",
                "minecraft:cave_extra_underground",
                "minecraft:canyon"
            ]
            .map(str::to_string)
            .as_slice()
        )
    );
}

/// Converted packs wrote the AIR step as a bare list or string. No bundled
/// biome uses that shape any more, so pin it directly.
#[test]
fn converted_carver_lists_still_parse() {
    let list = jparse(br#"{"carvers":["minecraft:cave"]}"#).unwrap();
    assert_eq!(
        super::carver_list(&list),
        Some(vec!["minecraft:cave".to_string()])
    );
    let single = jparse(br#"{"carvers":"minecraft:canyon"}"#).unwrap();
    assert_eq!(
        super::carver_list(&single),
        Some(vec!["minecraft:canyon".to_string()])
    );
}

/// Every biome's `MobSpawnSettings` from the pinned jar: all eight categories,
/// each weighted entry, the two spawn-cost fields and the creature probability.
/// The nightly runtime uses only the supported monsters, but the whole loaded
/// table is pinned so a loader change cannot silently drop a category.
#[test]
fn biome_spawn_tables_match_the_pinned_classes() {
    let fixture = jparse(include_bytes!(
        "../evidence/foundation/biome-spawners/vanilla.json"
    ))
    .unwrap();
    let J::Obj(biomes) = fixture.get("biomes").unwrap() else {
        panic!("biomes must be an object");
    };
    let reg = super::freg();
    let mut entries = 0;
    let mut costs = 0;
    for (name, row) in biomes {
        let got = reg.spawners.get(name).expect("biome spawners");
        let J::Obj(want) = row.get("spawners").unwrap() else {
            panic!("spawners must be an object");
        };
        assert_eq!(got.len(), want.len(), "categories for {name}");
        for (category, list) in want {
            let want_list = list.as_arr().unwrap();
            let got_list = got.get(category).unwrap();
            assert_eq!(got_list.len(), want_list.len(), "{name}/{category}");
            for (g, w) in got_list.iter().zip(want_list) {
                assert_eq!(g.kind, w.get("type").unwrap().str(), "{name}/{category}");
                assert_eq!(g.min_count, w.get("min").unwrap().num() as i32);
                assert_eq!(g.max_count, w.get("max").unwrap().num() as i32);
                assert_eq!(g.weight, w.get("weight").unwrap().num() as i32);
                entries += 1;
            }
        }
        let J::Obj(want_costs) = row.get("spawn_costs").unwrap() else {
            panic!("spawn_costs must be an object");
        };
        let got_costs = reg.spawn_costs.get(name).unwrap();
        assert_eq!(got_costs.len(), want_costs.len(), "costs for {name}");
        for (kind, want_cost) in want_costs {
            let got_cost = got_costs.get(kind).unwrap();
            assert_eq!(
                got_cost.energy_budget.to_bits(),
                want_cost.get("energy_budget").unwrap().num().to_bits()
            );
            assert_eq!(
                got_cost.charge.to_bits(),
                want_cost.get("charge").unwrap().num().to_bits()
            );
            costs += 1;
        }
        let probability = reg.spawn_probability.get(name).unwrap();
        assert_eq!(
            probability.to_bits(),
            (row.get("creature_generation_probability").unwrap().num() as f32).to_bits()
        );
    }
    assert_eq!(biomes.len(), 64, "pinned biome coverage changed");
    assert_eq!(entries, 746, "weighted spawner entries changed");
    assert_eq!(costs, 5, "spawn costs changed");
    assert_eq!(
        super::choose_supported_hostile("minecraft:plains", 0),
        Some("minecraft:spider")
    );
    assert_eq!(
        super::choose_supported_hostile("minecraft:the_end", 0),
        None,
        "the End has only unsupported endermen"
    );
    assert_eq!(
        super::spawn_cost_for("minecraft:soul_sand_valley", "minecraft:enderman"),
        Some(super::SpawnCost {
            energy_budget: 0.15,
            charge: 0.7
        })
    );
}

/// A geode config whose shape is fully determined: one point at a fixed offset, no
/// noise, no crack, no alternate layer, no inner placements. The decompiled
/// `GeodeFeature` then compares `1/sqrt(distSqr + pointOffset)` against
/// `1/sqrt(layer + points / outerWallDistance.max)`, which makes every layer a
/// distance band around that one point.
const GEODE_TEST_JSON: &str = r##"{
  "type": "minecraft:geode",
  "config": {
    "blocks": {
      "filling_provider": {"type":"minecraft:simple_state_provider","state":{"Name":"minecraft:air"}},
      "inner_layer_provider": {"type":"minecraft:simple_state_provider","state":{"Name":"minecraft:amethyst_block"}},
      "alternate_inner_layer_provider": {"type":"minecraft:simple_state_provider","state":{"Name":"minecraft:budding_amethyst"}},
      "middle_layer_provider": {"type":"minecraft:simple_state_provider","state":{"Name":"minecraft:calcite"}},
      "outer_layer_provider": {"type":"minecraft:simple_state_provider","state":{"Name":"minecraft:smooth_basalt"}},
      "inner_placements": [
        {"Name":"minecraft:small_amethyst_bud","Properties":{"facing":"up","waterlogged":"false"}}
      ],
      "cannot_replace": "#minecraft:features_cannot_replace",
      "invalid_blocks": "#minecraft:geode_invalid_blocks"
    },
    "layers": {"filling": 1.7, "inner_layer": 2.2, "middle_layer": 3.2, "outer_layer": 4.2},
    "crack": {"generate_crack_chance": 0.0, "base_crack_size": 2.0, "crack_point_offset": 2},
    "use_alternate_layer0_chance": 0.0,
    "use_potential_placements_chance": 0.0,
    "placements_require_layer0_alternate": true,
    "outer_wall_distance": {"type":"minecraft:uniform","min_inclusive":1,"max_inclusive":1},
    "distribution_points": {"type":"minecraft:uniform","min_inclusive":1,"max_inclusive":1},
    "point_offset": {"type":"minecraft:uniform","min_inclusive":1,"max_inclusive":1},
    "min_gen_offset": -16,
    "max_gen_offset": 16,
    "noise_multiplier": 0.0,
    "invalid_blocks_threshold": 1
  }
}"##;

/// The four layers are concentric, and which one a cell belongs to follows from the
/// distance thresholds alone: with `d0 = points / max = 1`, a cell at squared
/// distance `d` gets `1/sqrt(d + 1)` and the layers are `1/sqrt(1.7)`,
/// `1/sqrt(2.2+1)`, `1/sqrt(3.2+1)` and `1/sqrt(4.2+1)` counting inwards. The
/// expected layer of every written cell is derived here, not read back from the
/// code, so a swapped branch, a wrong threshold, a wrong `d0` or a distance that
/// skips `pointOffset` all fail.
#[test]
fn geode_layers_are_concentric_bands_around_its_points() {
    let reg = breg().unwrap();
    let cf = super::p_cf(&jparse(GEODE_TEST_JSON.as_bytes()).expect("json"))
        .expect("the geode config must parse");
    let mut g = Gen::load(0, crate::name_to_state).expect("datapack");
    let mut ctx = context(&mut g);
    let origin = (8, 8, 8);
    let mut rng = Xor::seed(7);
    gen_cf(&cf, freg(), &mut ctx, &mut rng, origin);

    // the single point is at origin + outer_wall_distance = (9,9,9), point_offset = 1
    let (d0, fill, inner, middle, outer) = (1.0 / 1.0, 1.7f64, 2.2f64, 3.2f64, 4.2f64);
    let (d1, d2, d3, d4) = (
        1.0 / fill.sqrt(),
        1.0 / (inner + d0).sqrt(),
        1.0 / (middle + d0).sqrt(),
        1.0 / (outer + d0).sqrt(),
    );
    let expect = |x: i32, y: i32, z: i32| -> &'static str {
        let sq = ((x - 9).pow(2) + (y - 9).pow(2) + (z - 9).pow(2)) as f64;
        let v = 1.0 / (sq + 1.0).sqrt();
        if v < d4 {
            "outside"
        } else if v >= d1 {
            "minecraft:air"
        } else if v >= d2 {
            "minecraft:amethyst_block"
        } else if v >= d3 {
            "minecraft:calcite"
        } else {
            "minecraft:smooth_basalt"
        }
    };
    let mut counts: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
    let mut wrong = vec![];
    for (&(x, y, z), &st) in ctx.wr.iter() {
        let want = expect(x, y, z);
        *counts.entry(want).or_default() += 1;
        if reg.name_of(st) != want {
            wrong.push(((x, y, z), reg.name_of(st).to_string(), want));
        }
    }
    assert!(
        wrong.is_empty(),
        "{} of {} written cells are in the wrong layer, first few: {:?}",
        wrong.len(),
        ctx.wr.len(),
        &wrong[..wrong.len().min(5)]
    );
    for layer in [
        "minecraft:air",
        "minecraft:amethyst_block",
        "minecraft:calcite",
        "minecraft:smooth_basalt",
    ] {
        assert!(
            counts.get(layer).copied().unwrap_or(0) > 0,
            "layer {layer} was never placed: {counts:?}"
        );
    }
    assert_eq!(
        counts.get("outside").copied().unwrap_or(0),
        0,
        "nothing outside the outermost threshold may be written"
    );
}

/// `invalid_blocks_threshold` is a guard, not a filter: the first point that lands
/// in air past the threshold abandons the whole geode, which is why an aborted one
/// leaves no trace at all.
#[test]
fn geode_abandons_itself_when_a_point_lands_in_air() {
    let reg = breg().unwrap();
    let cf = super::p_cf(&jparse(GEODE_TEST_JSON.as_bytes()).expect("json"))
        .expect("the geode config must parse");
    let mut g = Gen::load(0, crate::name_to_state).expect("datapack");
    let mut ctx = context(&mut g);
    let air = reg.state_of("minecraft:air", None);
    // the point is `origin + outer_wall_distance` = (9,9,9); make it air so the
    // first point already counts as invalid and `0 >= threshold` cannot hold
    ctx.set(9, 9, 9, air);
    let cf = match cf {
        Cf::Geode(mut g) => {
            g.invalid_blocks_threshold = 0;
            Cf::Geode(g)
        }
        _ => unreachable!("the test config is a geode"),
    };
    let mut rng = Xor::seed(7);
    gen_cf(&cf, freg(), &mut ctx, &mut rng, (8, 8, 8));
    let placed = ctx
        .wr
        .values()
        .filter(|&&st| {
            matches!(
                reg.name_of(st),
                "minecraft:amethyst_block" | "minecraft:calcite" | "minecraft:smooth_basalt"
            )
        })
        .count();
    assert_eq!(placed, 0, "an abandoned geode must place nothing");
}

/// The heightmap a feature reads is the block that is there, not the noise surface.
/// Vanilla maintains `OCEAN_FLOOR_WG` from every `setBlock` during terrain, so a
/// solid the aquifer pushes above the noise surface counts; the port's basis was
/// `ChunkCols::surface`, the density top, which reads the floor too low there.
///
/// Measured on the pinned kelp fixture (seed 0, chunk (79,-1)): this column's
/// density top is 18 while both worlds hold stone at y=33, so the floor is 34. With
/// the old basis the whole fixture region disagreed with vanilla's saved heightmap
/// on 329 of 9216 columns and agreed on none of them; with the walk it is 0 of 9216.
#[test]
fn heightmap_reads_the_floor_the_blocks_are_at_not_the_noise_surface() {
    let reg = breg().unwrap();
    let mut g = Gen::load(0, crate::name_to_state).expect("datapack");
    let mut ctx = context(&mut g);
    let (x, z) = (1264, -16);
    let stone = reg.state_of("minecraft:stone", None);
    assert_eq!(
        reg.name_of(ctx.get(x, 33, z)),
        "minecraft:stone",
        "the fixture column holds a solid at y=33"
    );
    assert!(
        ctx.hm_oc(x, z) > 33,
        "OCEAN_FLOOR must be above that solid, got {}",
        ctx.hm_oc(x, z)
    );
    assert_eq!(
        ctx.hm_oc(x, z),
        34,
        "the first free y above the floor is 34 (vanilla's saved OCEAN_FLOOR)"
    );
    // and the port's own block at the floor is above the density top it started from
    assert!(
        reg.name_of(ctx.get(x, 18, z)) != "minecraft:air",
        "the density top itself is solid, so the walk has a floor to stop at"
    );
    let _ = stone;
}

/// `minecraft:sea_pickle` (`edj`): `count` attempts, each `nextInt(8) - nextInt(8)`
/// on both horizontal axes, the OCEAN_FLOOR height at that column, then a
/// `nextInt(4) + 1` pickle count *before* the two survival tests. The count here is
/// 2 so both the placements and the draws are pinned.
const SEA_PICKLE_TEST_JSON: &str = r#"{
  "type": "minecraft:sea_pickle",
  "config": { "count": 2 }
}"#;

fn sea_pickle_cf() -> Cf {
    super::p_cf(&jparse(SEA_PICKLE_TEST_JSON.as_bytes()).expect("json"))
        .expect("the sea pickle config must parse")
}

/// The two tests are ordered after the pickle-count draw, so a rejected attempt
/// still consumes its five draws. Both halves are checked: an ocean patch where
/// both attempts land, and the generated terrain around it where neither does.
#[test]
fn sea_pickle_places_on_water_over_a_solid_top_face_and_draws_first() {
    let reg = breg().unwrap();
    let water = reg.state_of("minecraft:water", None);
    let sand = reg.state_of("minecraft:sand", None);
    let mut g = Gen::load(0, crate::name_to_state).expect("datapack");
    let mut ctx = context(&mut g);
    let (x, z) = (1280, 0); // lukewarm_ocean for seed 0

    // a flat patch: sand with water above, everywhere the offsets can reach
    for dx in -8..=8 {
        for dz in -8..=8 {
            let (px, pz) = (x + dx, z + dz);
            let fy = ctx.hm_oc_terrain(px, pz);
            ctx.set(px, fy - 1, pz, sand);
            for y in fy..=fy + 4 {
                ctx.set(px, y, pz, water);
            }
        }
    }
    let mut rng = Xor::seed(3);
    gen_cf(&sea_pickle_cf(), freg(), &mut ctx, &mut rng, (x, 0, z));

    let placed: Vec<((i32, i32, i32), u32)> = ctx
        .wr
        .iter()
        .filter(|(_, &st)| reg.name_of(st) == "minecraft:sea_pickle")
        .map(|(&p, &st)| (p, st))
        .collect();
    assert_eq!(
        placed.len(),
        2,
        "both attempts land on prepared water: {placed:?}"
    );
    for ((px, py, pz), st) in &placed {
        let pickles = reg.get_prop(*st, "pickles").expect("pickles property");
        assert!(
            (1..=4).contains(&pickles.parse::<i32>().unwrap()),
            "pickles must come from nextInt(4) + 1, got {pickles}"
        );
        assert_eq!(
            reg.get_prop(*st, "waterlogged").as_deref(),
            Some("true"),
            "the default sea pickle is waterlogged"
        );
        assert_eq!(
            *py,
            ctx.hm_oc_terrain(*px, *pz),
            "the pickle sits at the column's OCEAN_FLOOR height"
        );
        assert!(
            reg.side_solid(ctx.get(*px, py - 1, *pz), 1),
            "the block below must have a solid top face"
        );
    }

    // two attempts, five bound draws each (dx twice, dz twice, pickles once)
    let mut expect = Xor::seed(3);
    for _ in 0..10 {
        let v = expect.next_bound(8);
        let _ = v;
    }
    assert_eq!(
        rng.next_bound(1 << 30),
        expect.next_bound(1 << 30),
        "the stream must advance by exactly two attempts of five draws"
    );
}

/// The other half of the same rule: where the drawn cell is not water, nothing is
/// placed and the draws are still spent. The patch is filled with air rather than
/// left as generated - the origin is lukewarm_ocean, so its own terrain would be
/// water and the attempt would legitimately succeed.
#[test]
fn sea_pickle_skips_cells_that_are_not_water() {
    let reg = breg().unwrap();
    let air = reg.state_of("minecraft:air", None);
    let mut g = Gen::load(0, crate::name_to_state).expect("datapack");
    let mut ctx = context(&mut g);
    let (x, z) = (1280, 0);
    for dx in -8..=8 {
        for dz in -8..=8 {
            let (px, pz) = (x + dx, z + dz);
            let fy = ctx.hm_oc_terrain(px, pz);
            for y in fy..=fy + 4 {
                ctx.set(px, y, pz, air);
            }
        }
    }
    let before = ctx.wr.len();
    let mut rng = Xor::seed(3);
    gen_cf(&sea_pickle_cf(), freg(), &mut ctx, &mut rng, (x, 0, z));
    let pickles = ctx
        .wr
        .iter()
        .filter(|(_, &st)| reg.name_of(st) == "minecraft:sea_pickle")
        .count();
    assert_eq!(pickles, 0, "nothing may be placed on unprepared terrain");
    assert_eq!(
        ctx.wr.len(),
        before,
        "a skipped attempt writes nothing at all"
    );
    let mut expect = Xor::seed(3);
    for _ in 0..10 {
        let v = expect.next_bound(8);
        let _ = v;
    }
    assert_eq!(
        rng.next_bound(1 << 30),
        expect.next_bound(1 << 30),
        "the skipped attempts still spent their draws"
    );
}

/// `minecraft:ice_spike` (`ecq`), whose config is `{}`. The feature walks down
/// through air, requires a snow block, then stacks `radius` shrinking over
/// `nextInt(4) + 7` layers; the drawn base offset and height are replayed here from
/// a fresh stream, so the draw order is pinned as well as the shape.
#[test]
fn ice_spike_builds_a_tapering_spike_on_snow() {
    let reg = breg().unwrap();
    let snow = reg.state_of("minecraft:snow_block", None);
    let packed = reg.state_of("minecraft:packed_ice", None);
    let mut g = Gen::load(0, crate::name_to_state).expect("datapack");
    let mut ctx = context(&mut g);
    let (x, z) = (8, 8);
    // a snow platform at y=70, high enough for the downward drill (it stops at 50)
    let top = 70;
    for dx in -10..=10 {
        for dz in -10..=10 {
            for y in top - 6..=top {
                ctx.set(x + dx, y, z + dz, snow);
            }
            for y in top + 1..=top + 30 {
                ctx.set(x + dx, y, z + dz, reg.state_of("minecraft:air", None));
            }
        }
    }

    // replay the first draws: base offset, height, radius term, and the
    // floating-spike roll, which is only drawn when the radius exceeds one
    let mut probe = Xor::seed(11);
    let base_off = probe.next_bound(4);
    let height = probe.next_bound(4) + 7;
    let radius_max = height / 4 + probe.next_bound(2);
    let mut float_off = 0;
    if radius_max > 1 && probe.next_bound(60) == 0 {
        float_off = 10 + probe.next_bound(30);
    }
    let base_y = top + base_off + float_off;

    let mut rng = Xor::seed(11);
    gen_cf(&Cf::IceSpike, freg(), &mut ctx, &mut rng, (x, top + 20, z));

    let cells: Vec<(i32, i32, i32)> = ctx
        .wr
        .iter()
        .filter(|(_, &st)| st == packed)
        .map(|(&p, _)| p)
        .collect();
    assert!(!cells.is_empty(), "a snow platform must produce a spike");
    assert!(
        cells
            .iter()
            .any(|(x0, y, z0)| (*x0, *y, *z0) == (x, base_y, z)),
        "the base cell is always placed (dx = dz = 0 is inside, and r >= 1 at h = 0)"
    );
    let tip = cells.iter().map(|(_, y, _)| *y).max().unwrap();
    assert_eq!(
        tip,
        base_y + height - 1,
        "the tip must be the drawn height above the drawn base"
    );
    let tip_cols: Vec<_> = cells.iter().filter(|(_, y, _)| *y == tip).collect();
    assert_eq!(
        tip_cols.len(),
        1,
        "the tip is a single column: {tip_cols:?}"
    );
    assert!(
        cells.iter().any(|(_, y, _)| *y < top),
        "the downward drill must replace the snow below the base"
    );
    // and the drill only writes the one material
    assert!(ctx
        .wr
        .values()
        .all(|&st| { st == packed || st == snow || reg.name_of(st) == "minecraft:air" }));
}

/// The snow requirement is checked before any draw, so a spike that finds stone
/// places nothing and leaves the stream untouched.
#[test]
fn ice_spike_does_nothing_without_snow() {
    let reg = breg().unwrap();
    let stone = reg.state_of("minecraft:stone", None);
    let mut g = Gen::load(0, crate::name_to_state).expect("datapack");
    let mut ctx = context(&mut g);
    let (x, z) = (8, 8);
    for dx in -10..=10 {
        for dz in -10..=10 {
            for y in 60..=70 {
                ctx.set(x + dx, y, z + dz, stone);
            }
        }
    }
    let before = ctx.wr.len();
    let mut rng = Xor::seed(11);
    gen_cf(&Cf::IceSpike, freg(), &mut ctx, &mut rng, (x, 90, z));
    assert_eq!(
        ctx.wr.len(),
        before,
        "a non-snow surface places nothing at all"
    );
    assert_eq!(
        rng.next_bound(1 << 30),
        Xor::seed(11).next_bound(1 << 30),
        "and draws nothing"
    );
}

/// `minecraft:root_system` (`edg`): the ported *rules*, unit-tested because they are
/// where the obfuscated readings went wrong (`BlockState.e()` is `isSolid()`, not
/// `isAir()`; `FluidTags.a`/`b` are WATER/LAVA).
///
/// The end-to-end path (tree placed -> root column -> hanging roots) is not covered
/// here: it needs the nested placed feature to write, and in this harness the
/// synthetic nested features tried (an inline `disk` and an inline `simple_block`,
/// each also run directly at the candidate position) wrote nothing even though the
/// target blocks were present. That is an open harness question, recorded in
/// evidence/gameplay/feature-parity/README.md, not a claim about this feature.
#[test]
fn root_system_vertical_water_allowance_follows_the_config() {
    let reg = breg().unwrap();
    let air = reg.state_of("minecraft:air", None);
    let water = reg.state_of("minecraft:water", None);
    let stone = reg.state_of("minecraft:stone", None);
    // air is always fine
    assert!(super::root_valid_vertical(reg, air, 1, 2));
    assert!(super::root_valid_vertical(reg, air, 9, 2));
    // water only up to allowed_vertical_water_for_tree, counted from the base
    assert!(super::root_valid_vertical(reg, water, 1, 2));
    assert!(
        !super::root_valid_vertical(reg, water, 2, 2),
        "i + 1 <= max_water, so index 2 is past an allowance of 2"
    );
    assert!(!super::root_valid_vertical(reg, water, 1, 0));
    // anything else is not a valid vertical state at all
    assert!(!super::root_valid_vertical(reg, stone, 1, 2));
    // and the fluid predicates separate the two tags
    assert!(super::is_water_fluid(reg, water));
    assert!(!super::is_lava_fluid(reg, water));
    let lava = reg.state_of("minecraft:lava", None);
    assert!(super::is_lava_fluid(reg, lava));
    assert!(!super::is_water_fluid(reg, lava));
}

/// The origin must be air: a root system under a solid block returns immediately,
/// before a single draw (the check is the first statement in vanilla's `place`).
#[test]
fn root_system_does_nothing_when_its_origin_is_not_air() {
    let reg = breg().unwrap();
    let stone = reg.state_of("minecraft:stone", None);
    let cf = pack_cf("rooted_azalea_tree");
    let mut g = Gen::load(0, crate::name_to_state).expect("datapack");
    let mut ctx = context(&mut g);
    let (x, z) = (8, 8);
    for dx in -8..=8 {
        for dz in -8..=8 {
            for y in 60..=90 {
                ctx.set(x + dx, y, z + dz, stone);
            }
        }
    }
    let before = ctx.wr.len();
    let mut rng = Xor::seed(5);
    gen_cf(&cf, freg(), &mut ctx, &mut rng, (x, 70, z));
    assert_eq!(ctx.wr.len(), before, "a buried origin places nothing");
    assert_eq!(
        rng.next_bound(1 << 30),
        Xor::seed(5).next_bound(1 << 30),
        "and draws nothing"
    );
}

/// `minecraft:desert_well` (`ebw`), config `{}`. Every cell is a fixed offset from
/// the sand block the walk-down lands on, so the shape is asserted exactly; the only
/// randomness is which of the five shaft columns receives the two suspicious-sand
/// blocks (`nextInt(5)` twice), and that is replayed from the same seed.
#[test]
fn desert_well_builds_its_shape_on_sand() {
    let reg = breg().unwrap();
    let states = |n: &str| reg.state_of(n, None);
    let (sand, slab, sandstone, water, suspicious) = (
        states("minecraft:sand"),
        states("minecraft:sandstone_slab"),
        states("minecraft:sandstone"),
        states("minecraft:water"),
        states("minecraft:suspicious_sand"),
    );
    let mut g = Gen::load(0, crate::name_to_state).expect("datapack");
    let mut ctx = context(&mut g);
    let (x, z) = (8, 8);
    let top = 70;
    for dx in -6..=6 {
        for dz in -6..=6 {
            for y in top - 4..=top {
                ctx.set(x + dx, y, z + dz, sand);
            }
        }
    }
    let mut rng = Xor::seed(9);
    gen_cf(&Cf::DesertWell, freg(), &mut ctx, &mut rng, (x, top + 3, z));

    let at = |ctx: &mut FC, p: (i32, i32, i32)| ctx.get(p.0, p.1, p.2);
    // the walk-down lands on the sand's top cell and the well is built there
    assert_eq!(
        at(&mut ctx, (x, top, z)),
        water,
        "the well's column is water"
    );
    for (dx, dz) in [(1, 0), (0, 1), (-1, 0), (0, -1)] {
        assert_eq!(
            at(&mut ctx, (x + dx, top, z + dz)),
            water,
            "and one cell wider"
        );
        assert_eq!(
            at(&mut ctx, (x + dx, top - 1, z + dz)),
            sand,
            "with a sand floor under the arms"
        );
    }
    assert_eq!(at(&mut ctx, (x, top - 1, z)), sand);
    // base plate, roof, posts and pillars
    assert_eq!(at(&mut ctx, (x - 2, top - 2, z)), slab, "base plate");
    assert_eq!(at(&mut ctx, (x, top + 4, z)), sandstone, "roof centre");
    assert_eq!(at(&mut ctx, (x + 1, top + 4, z)), slab, "roof ring");
    assert_eq!(at(&mut ctx, (x + 2, top + 1, z)), slab, "a post");
    assert_eq!(at(&mut ctx, (x - 1, top + 1, z - 1)), sandstone, "a pillar");
    assert_eq!(
        at(&mut ctx, (x + 1, top + 3, z + 1)),
        sandstone,
        "up the pillar"
    );

    // the two suspicious-sand blocks, on the columns the two draws choose
    let shaft = [
        (x, top, z),
        (x + 1, top, z),
        (x, top, z + 1),
        (x - 1, top, z),
        (x, top, z - 1),
    ];
    let mut probe = Xor::seed(9);
    let first = shaft[probe.next_bound(5) as usize];
    let second = shaft[probe.next_bound(5) as usize];
    assert_eq!(
        at(&mut ctx, (first.0, first.1 + 1, first.2)),
        suspicious,
        "the first draw picks the column at +1"
    );
    assert_eq!(
        at(&mut ctx, (second.0, second.1 + 2, second.2)),
        suspicious,
        "the second draw picks the column at +2"
    );
    let cells: Vec<((i32, i32, i32), u32)> = ctx.wr.iter().map(|(&p, &st)| (p, st)).collect();
    assert_eq!(
        cells.iter().filter(|(_, st)| *st == suspicious).count(),
        2,
        "exactly two suspicious-sand blocks"
    );
}

/// The sand gate is the first thing after the walk-down and precedes the feature's
/// only two draws, so a stone surface places nothing and spends nothing.
#[test]
fn desert_well_needs_sand_and_draws_nothing_otherwise() {
    let reg = breg().unwrap();
    let stone = reg.state_of("minecraft:stone", None);
    let mut g = Gen::load(0, crate::name_to_state).expect("datapack");
    let mut ctx = context(&mut g);
    let (x, z) = (8, 8);
    for dx in -6..=6 {
        for dz in -6..=6 {
            for y in 60..=70 {
                ctx.set(x + dx, y, z + dz, stone);
            }
        }
    }
    let before = ctx.wr.clone();
    let mut rng = Xor::seed(9);
    gen_cf(&Cf::DesertWell, freg(), &mut ctx, &mut rng, (x, 73, z));
    assert!(ctx.wr == before, "a stone surface places nothing");
    assert_eq!(
        rng.next_bound(1 << 30),
        Xor::seed(9).next_bound(1 << 30),
        "and draws nothing"
    );
}

/// `minecraft:iceberg` (`ecr`), config `{state}`. The shape parameters all come from
/// the head of the draw stream (`nextDouble > 0.7`, `nextDouble*2pi`, `nextInt(5)`,
/// `nextInt(3)`, a second `nextDouble > 0.7` choosing the tall shape, then the height
/// and the two radii), so they are replayed here and used as the bounds the placed
/// cells must satisfy - which ties the replay to what the placement actually used.
#[test]
fn iceberg_builds_a_shape_around_sea_level() {
    let reg = breg().unwrap();
    let packed = reg.state_of("minecraft:packed_ice", None);
    let snow = reg.state_of("minecraft:snow_block", None);
    let water = reg.state_of("minecraft:water", None);
    let stone = reg.state_of("minecraft:stone", None);
    let mut g = Gen::load(0, crate::name_to_state).expect("datapack");
    let mut ctx = context(&mut g);
    let sea = crate::worldgen::sea_level();
    let (x, z) = (8, 8);
    // an ocean deep enough for the underside and wide enough for the widest shape
    for dx in -40..=40 {
        for dz in -40..=40 {
            for y in sea - 40..=sea - 1 {
                ctx.set(x + dx, y, z + dz, water);
            }
            for y in sea..=sea + 40 {
                ctx.set(x + dx, y, z + dz, water);
            }
            ctx.set(x + dx, sea - 41, z + dz, stone);
        }
    }

    // replay the head of the stream
    let mut probe = Xor::seed(21);
    let _fire = probe.next_f64() > 0.7;
    let _angle = probe.next_f64() * 2.0 * std::f64::consts::PI;
    let i = 11 - probe.next_bound(5);
    let _j = 3 + probe.next_bound(3);
    let tall = probe.next_f64() > 0.7;
    let mut l = if tall {
        probe.next_bound(6) + 6
    } else {
        probe.next_bound(15) + 3
    };
    if !tall && probe.next_f64() > 0.9 {
        l += probe.next_bound(19) + 7;
    }
    let i1 = (l + probe.next_bound(11)).min(18);
    let _j1 = (l + probe.next_bound(7) - probe.next_bound(5)).min(11);
    let k1 = if tall { i } else { 11 };

    let mut rng = Xor::seed(21);
    // the origin is handed in *above* sea level and the feature re-anchors it, which
    // is what the mutation anchor for that line checks
    gen_cf(
        &Cf::Iceberg { state: packed },
        freg(),
        &mut ctx,
        &mut rng,
        (x, sea + 12, z),
    );

    let cells: Vec<((i32, i32, i32), u32)> = ctx.wr.iter().map(|(&p, &st)| (p, st)).collect();
    let ice: Vec<_> = cells
        .iter()
        .filter(|(_, st)| *st == packed || *st == snow)
        .collect();
    assert!(!ice.is_empty(), "the iceberg must place its material");
    assert!(
        cells.iter().any(|(_, st)| *st == packed),
        "the config's state is what most of the shape is built from"
    );
    let mut dmin = i32::MAX;
    let mut dmax = i32::MIN;
    for ((px, py, pz), _) in ice {
        let (dx, dy, dz) = (px - x, py - sea, pz - z);
        assert!(
            dx.abs() <= k1 + 4 && dz.abs() <= k1 + 4,
            "a cell must stay near the footprint (k1 = {k1}): {dx},{dy},{dz}"
        );
        dmin = dmin.min(dy);
        dmax = dmax.max(dy);
    }
    assert!(
        dmin >= -(i1 - 1),
        "the underside must not go below -i1 + 1 (i1 = {i1}), got {dmin}"
    );
    assert!(
        dmax < l,
        "the top must not go above l - 1 (l = {l}), got {dmax}"
    );
    assert!(
        dmin < 0 && dmax > 0,
        "an iceberg straddles sea level: below {dmin}, above {dmax}"
    );
    // and the water it was built in is still water elsewhere
    assert_eq!(ctx.get(x + 30, sea + 30, z), water, "far away stays water");
}

/// Load a configured feature straight from the pack's own JSON, so a test never
/// restates a config that the pack already holds.
fn pack_cf(name: &str) -> Cf {
    // Gen::dp_files_any(["configured_feature", "feature"]) overlays `feature`
    // last, so a converted pack's `feature/` wins. Match that precedence.
    for dir in ["feature", "configured_feature"] {
        let path = format!("datapacks/vanilla/data/minecraft/worldgen/{dir}/{name}.json");
        if let Ok(text) = std::fs::read_to_string(&path) {
            return super::p_cf(&jparse(text.as_bytes()).expect("json"))
                .expect("the config must parse");
        }
    }
    panic!("no configured feature {name} in the pack");
}

/// The gallery of `dripstone_cluster_hangs_...`: a stone slab with a span of `fill`
/// cells between a floor endpoint at y = 59 and a ceiling endpoint at y = 67. The
/// origin sits in the middle of the span.
fn dripstone_gallery(ctx: &mut FC, ox: i32, oz: i32, floor_y: i32, span: i32, fill: u32) {
    let reg = breg().unwrap();
    let stone = reg.state_of("minecraft:stone", None);
    for dx in -12..=12 {
        for dz in -12..=12 {
            for y in 40..=floor_y {
                ctx.set(ox + dx, y, oz + dz, stone);
            }
            for y in floor_y + 1..=floor_y + span {
                ctx.set(ox + dx, y, oz + dz, fill);
            }
            for y in floor_y + span + 1..=100 {
                ctx.set(ox + dx, y, oz + dz, stone);
            }
        }
    }
}

/// The cells a feature changed, given the fixture's own write set: a cell counts as
/// the feature's when it is new *or* its state differs (a replacement is not "new").
fn changed(
    before: &std::collections::HashMap<(i32, i32, i32), u32>,
    ctx: &FC,
) -> Vec<((i32, i32, i32), u32)> {
    let mut out: Vec<((i32, i32, i32), u32)> = ctx
        .wr
        .iter()
        .filter(|(p, st)| before.get(*p) != Some(*st))
        .map(|(&p, &st)| (p, st))
        .collect();
    out.sort();
    out
}

/// `minecraft:dripstone_cluster` (`eby`, `DripstoneClusterFeature`) with a dry gallery
/// (span of air). Pinned here: the orientation of every tip (a stalactite hangs below
/// the ceiling with `vertical_direction=down` and starts at the ceiling's own
/// neighbour cell, a stalagmite stands the other way up), the layer sequence
/// (base -> middles -> frustum -> tip), that no dripstone block lands inside the span,
/// that nothing is written outside the drawn radius, and the exact number of cells this
/// seeded stream produces.
#[test]
fn dripstone_cluster_hangs_stalactites_below_and_stands_stalagmites_above() {
    let reg = breg().unwrap();
    let cf = pack_cf("dripstone_cluster");
    let Cf::DripstoneCluster(dcfg) = &cf else {
        panic!("dripstone_cluster must parse as a cluster");
    };
    let mut g = Gen::load(0, crate::name_to_state).expect("datapack");
    let mut ctx = context(&mut g);
    let air = reg.state_of("minecraft:air", None);
    let (ox, oz, oy) = (8, 8, 64);
    let (floor_y, ceil_y) = (59, 67);
    dripstone_gallery(&mut ctx, ox, oz, floor_y, 7, air);

    // replay the head of the stream for the radii the footprint bound needs
    let (rx, rz) = {
        let mut probe = Xor::seed(31);
        let _ = dcfg.height.get(&mut probe);
        let _ = dcfg.wetness.get(&mut probe);
        let _ = dcfg.density.get(&mut probe);
        let rx = dcfg.radius.get(&mut probe);
        let rz = dcfg.radius.get(&mut probe);
        (rx, rz)
    };
    let before = ctx.wr.clone();
    let mut rng = Xor::seed(31);
    gen_cf(&cf, freg(), &mut ctx, &mut rng, (ox, oy, oz));

    let mut blocks = 0usize;
    let mut cols: std::collections::BTreeMap<(i32, i32), Vec<(i32, u32)>> = Default::default();
    for ((px, py, pz), st) in changed(&before, &ctx) {
        assert!(
            (px - ox).abs() <= rx && (pz - oz).abs() <= rz,
            "a write at {px},{py},{pz} is outside the drawn radius {rx}x{rz}"
        );
        match reg.name_of(st) {
            "minecraft:pointed_dripstone" => {
                cols.entry((px, pz)).or_default().push((py, st));
            }
            "minecraft:dripstone_block" => {
                blocks += 1;
                assert!(
                    !(floor_y + 1..=ceil_y - 1).contains(&py),
                    "a dripstone block inside the span at {px},{py},{pz}"
                );
            }
            _ => {}
        }
    }

    let mut tips = 0usize;
    for ((px, pz), cells) in &cols {
        for up in [false, true] {
            let want = if up { "up" } else { "down" };
            let mut run: Vec<(i32, String)> = cells
                .iter()
                .filter(|(_, st)| reg.get_prop(*st, "vertical_direction").as_deref() == Some(want))
                .map(|(y, st)| (*y, reg.get_prop(*st, "thickness").unwrap_or_default()))
                .collect();
            if run.is_empty() {
                continue;
            }
            run.sort_by_key(|(y, _)| if up { *y } else { -*y });
            tips += run.len();
            let first_y = run[0].0;
            let want_y = if up { floor_y + 1 } else { ceil_y - 1 };
            assert_eq!(
                first_y, want_y,
                "a {want} tip at {px},{pz} must start at the span's own neighbour cell"
            );
            let n = run.len() as i32;
            let mut expect: Vec<&str> = Vec::new();
            if n >= 3 {
                expect.push("base");
                expect.extend(std::iter::repeat_n("middle", (n - 3) as usize));
            }
            if n >= 2 {
                expect.push("frustum");
            }
            expect.push("tip");
            let last = run.last().unwrap().1.as_str();
            assert!(
                last == "tip" || last == "tip_merge",
                "the tip of a {want} run at {px},{pz} is {last}"
            );
            let got: Vec<&str> = run[..run.len() - 1]
                .iter()
                .map(|(_, t)| t.as_str())
                .collect();
            assert_eq!(
                got,
                expect[..expect.len() - 1],
                "layer sequence of the {want} run at {px},{pz}"
            );
            if want == "down" {
                assert!(
                    first_y < ceil_y && run.iter().all(|(y, _)| *y > floor_y),
                    "a stalactite belongs between the floor and the ceiling"
                );
            }
        }
    }
    println!(
        "DRIPSTONE dry: tips={tips} blocks={blocks} columns={} rx={rx} rz={rz}",
        cols.len()
    );
    assert!(tips > 0, "the seeded stream must grow at least one tip");
    // A regression pin, not a vanilla comparison: the seeded stream with the pack's
    // own config produces exactly this, so any change to a draw, a gate or a loop
    // bound moves it.
    assert_eq!((tips, blocks), (209, 675), "golden cell counts");
}

/// The same feature with a **water** span. `canPlacePool` wants base stone or water on
/// all four sides and above the floor cell, which a dry gallery's air cannot provide -
/// so a pool only appears in an already-wet column. The pool turns the floor endpoint
/// into water and moves the column's floor one cell *down* into the rock, so the
/// stalagmite that follows starts one cell lower than in the dry case, replaces the
/// pool's own water, and is waterlogged. That lowered tip is what this pins.
#[test]
fn dripstone_cluster_pools_lower_the_stalagmite_into_the_water() {
    let reg = breg().unwrap();
    let cf = pack_cf("dripstone_cluster");
    let mut g = Gen::load(0, crate::name_to_state).expect("datapack");
    let mut ctx = context(&mut g);
    let water = reg.state_of("minecraft:water", None);
    let (ox, oz, oy) = (8, 8, 64);
    let floor_y = 59;
    dripstone_gallery(&mut ctx, ox, oz, floor_y, 7, water);

    let before = ctx.wr.clone();
    let mut rng = Xor::seed(31);
    gen_cf(&cf, freg(), &mut ctx, &mut rng, (ox, oy, oz));

    let mut lowered = 0usize;
    for ((px, py, pz), st) in changed(&before, &ctx) {
        if py != floor_y {
            continue;
        }
        if reg.name_of(st) == "minecraft:pointed_dripstone" {
            // the pool's cell: the tip replaced the water it had just written
            assert_eq!(
                reg.get_prop(st, "vertical_direction").as_deref(),
                Some("up"),
                "a lowered tip at {px},{pz} must grow up"
            );
            assert_eq!(
                reg.get_prop(st, "waterlogged").as_deref(),
                Some("true"),
                "the tip standing in the pool at {px},{pz} is waterlogged"
            );
            lowered += 1;
        }
        // Otherwise the column kept its water: that happens when the floor tip is not
        // grown at all, and the two never share a cell.
        assert!(
            reg.name_of(st) == "minecraft:water"
                || reg.name_of(st) == "minecraft:pointed_dripstone"
                || reg.name_of(st) == "minecraft:dripstone_block",
            "unexpected {} at the floor endpoint {px},{pz}",
            reg.name_of(st)
        );
    }
    println!("DRIPSTONE wet: lowered_tips={lowered} water_cells={water}");
    assert!(
        lowered > 0,
        "a water-filled span must produce pools for this seed"
    );
}

/// The float providers (`bpu` and its four types) arrived with the dripstone cluster.
/// The feature's whole shape shifts if any of them consumes a different number of draws,
/// so each is checked against a twin `Xor` allowed to draw exactly what the vanilla
/// method draws - value *and* follow-on stream position.
#[test]
fn float_providers_draw_and_clamp_like_the_vanilla_types() {
    // constant (`bps`): no draw
    let p = p_fp(&jparse(br#"{"type":"constant","value":0.25}"#).unwrap()).unwrap();
    let (mut a, mut b) = (Xor::seed(3), Xor::seed(3));
    assert_eq!(p.get(&mut a), 0.25);
    assert_eq!(a.next_long(), b.next_long(), "a constant draws nothing");

    // uniform (`bqb`): one nextFloat, `min + r * (max - min)`, max exclusive
    let p =
        p_fp(&jparse(br#"{"type":"uniform","min_inclusive":0.3,"max_exclusive":0.7}"#).unwrap())
            .unwrap();
    let (mut a, mut b) = (Xor::seed(3), Xor::seed(3));
    assert_eq!(p.get(&mut a), b.next_f32() * (0.7f32 - 0.3) + 0.3);
    assert_eq!(a.next_long(), b.next_long(), "a uniform float draws once");

    // clamped normal (`bpq`): one Gaussian (two doubles), f64 arithmetic, then clamp
    let p = p_fp(
        &jparse(br#"{"type":"clamped_normal","mean":0.1,"deviation":0.3,"min":0.1,"max":0.9}"#)
            .unwrap(),
    )
    .unwrap();
    let (mut a, mut b) = (Xor::seed(3), Xor::seed(3));
    // the config's floats are promoted to f64 (`Mth.nextGaussian(RandomSource, double,
    // double)`), so the f32->f64 widening of `0.1` is part of the value
    let want =
        (0.1f32 as f64 + b.next_gauss() * 0.3f32 as f64).clamp(0.1f32 as f64, 0.9f32 as f64) as f32;
    assert_eq!(p.get(&mut a), want);
    assert_eq!(
        a.next_long(),
        b.next_long(),
        "a clamped normal draws one Gaussian"
    );

    // trapezoid (`bqa`): two nextFloats, `min + r1*h + r2*g`
    let p = p_fp(&jparse(br#"{"type":"trapezoid","min":-1.0,"max":2.0,"plateau":1.0}"#).unwrap())
        .unwrap();
    let (mut a, mut b) = (Xor::seed(3), Xor::seed(3));
    let (f, g) = (3.0f32, (3.0f32 - 1.0) / 2.0);
    assert_eq!(
        p.get(&mut a),
        -1.0 + b.next_f32() * (f - g) + b.next_f32() * g
    );
    assert_eq!(
        a.next_long(),
        b.next_long(),
        "a trapezoid float draws twice"
    );
}

/// `minecraft:large_dripstone` (`ecu`, `LargeDripstoneFeature`) in a gallery whose span
/// is 7 tall. The span's height gives the radius (`height * 0.33` clamped into the
/// pack's `column_radius` 3..19, so exactly 3) and the two spikes reach their bases at
/// the span's own neighbour cells. The body is built *in* the empty cells and stops when
/// it walks into base stone, so every placed cell is inside the span.
#[test]
fn large_dripstone_hangs_and_stands_a_tapered_spike() {
    let reg = breg().unwrap();
    let cf = pack_cf("large_dripstone");
    let Cf::LargeDripstone(dcfg) = &cf else {
        panic!("large_dripstone must parse as a large dripstone");
    };
    let mut g = Gen::load(0, crate::name_to_state).expect("datapack");
    let mut ctx = context(&mut g);
    let air = reg.state_of("minecraft:air", None);
    let drip = reg.state_of("minecraft:dripstone_block", None);
    let (ox, oz, oy) = (8, 8, 63);
    let (floor_y, ceil_y) = (59, 67);
    dripstone_gallery(&mut ctx, ox, oz, floor_y, 7, air);

    // replay the radius draw (the span height and the config decide it)
    let want_radius = {
        let mut probe = Xor::seed(11);
        let est = ((ceil_y - floor_y - 1) as f32 * dcfg.ratio) as i32;
        let est = est.clamp(dcfg.radius.min_inclusive(), dcfg.radius.max_inclusive());
        probe.next_inbetween(dcfg.radius.min_inclusive(), est)
    };
    let before = ctx.wr.clone();
    let mut rng = Xor::seed(11);
    gen_cf(&cf, freg(), &mut ctx, &mut rng, (ox, oy, oz));

    let changed = changed(&before, &ctx);
    assert!(!changed.is_empty(), "the gallery must grow something");
    let mut above = 0usize;
    let mut below = 0usize;
    let mut cols: std::collections::BTreeMap<(i32, i32), Vec<i32>> = Default::default();
    for ((px, py, pz), st) in &changed {
        assert_eq!(
            *st, drip,
            "the large dripstone is built out of dripstone blocks: {px},{py},{pz}"
        );
        assert!(
            (floor_y + 1..=ceil_y - 1).contains(py),
            "the body is built in the span, not in the rock: {px},{py},{pz}"
        );
        assert!(
            ((px - ox).pow(2) + (pz - oz).pow(2)) as f64 <= (want_radius as f64).powi(2) + 1.0,
            "a cell at {px},{pz} is outside the drawn radius {want_radius}"
        );
        if *py > oy {
            above += 1;
        } else {
            below += 1;
        }
        cols.entry((*px, *pz)).or_default().push(*py);
    }
    assert!(
        above > 0 && below > 0,
        "both spikes must appear: {above} / {below}"
    );

    // Each spike's cells in a column are anchored at its own base and run contiguously
    // away from the span's end - a spike can only place on empty cells and can only stop
    // where a cell was already filled or its taper ran out, so the two anchored walks
    // (down from the ceiling's neighbour, up from the floor's) reproduce the column
    // exactly. A spike that reached neither end would show up here.
    let (mut hang_runs, mut stand_runs) = (0, 0);
    for ((px, pz), ys) in &cols {
        let set: std::collections::BTreeSet<i32> = ys.iter().copied().collect();
        let mut walk: std::collections::BTreeSet<i32> = Default::default();
        let mut y = ceil_y - 1;
        while set.contains(&y) {
            walk.insert(y);
            y -= 1;
        }
        if set.contains(&(ceil_y - 1)) {
            hang_runs += 1;
        }
        y = floor_y + 1;
        while set.contains(&y) {
            walk.insert(y);
            y += 1;
        }
        if set.contains(&(floor_y + 1)) {
            stand_runs += 1;
        }
        assert_eq!(
            &walk, &set,
            "the cells at {px},{pz} must be the two anchored runs: {ys:?}"
        );
    }
    assert!(
        hang_runs > 0 && stand_runs > 0,
        "both spikes must build: {hang_runs} hanging / {stand_runs} standing"
    );
    println!(
        "LARGE dry: cells={} columns={} above={} below={} radius={want_radius} runs={hang_runs}/{stand_runs}",
        changed.len(),
        cols.len(),
        above,
        below
    );
    assert_eq!(changed.len(), 37, "golden cell count");
}

/// The same feature in a span tall enough to satisfy the wind gate
/// (`min_radius_for_wind` / `min_bluntness_for_wind`). The sway is a per-y translation
/// `floor(wind * (origin_y - y))`, so running the identical seed with the pack's wind
/// speed and with a *zero-drawing* zero speed (same draws, no displacement) must produce
/// the same cells moved by exactly that vector.
#[test]
fn large_dripstone_sways_when_both_spikes_clear_the_wind_gate() {
    let reg = breg().unwrap();
    let cf = pack_cf("large_dripstone");
    let Cf::LargeDripstone(dcfg) = &cf else {
        panic!("large_dripstone must parse as a large dripstone");
    };
    let mut g = Gen::load(0, crate::name_to_state).expect("datapack");
    let mut ctx = context(&mut g);
    let air = reg.state_of("minecraft:air", None);
    let (ox, oz, oy) = (8, 8, 63);
    let (floor_y, ceil_y) = (55, 71);
    dripstone_gallery(&mut ctx, ox, oz, floor_y, 15, air);

    // The gate is a precondition: with a failing seed there is no sway, and the
    // comparison below would prove nothing. Pick the first seed that clears it and say
    // which one, so the test cannot silently degrade into comparing two still shapes.
    let est = ((ceil_y - floor_y - 1) as f32 * dcfg.ratio) as i32;
    let est = est.clamp(dcfg.radius.min_inclusive(), dcfg.radius.max_inclusive());
    let (seed, radius, wind_v) = (0u64..64)
        .find_map(|seed| {
            let mut probe = Xor::seed(seed);
            let radius = probe.next_inbetween(dcfg.radius.min_inclusive(), est);
            let hang_blunt = dcfg.stalactite_bluntness.get(&mut probe);
            let _hang_scale = dcfg.height_scale.get(&mut probe);
            let stand_blunt = dcfg.stalagmite_bluntness.get(&mut probe);
            let _stand_scale = dcfg.height_scale.get(&mut probe);
            if radius < dcfg.min_radius_for_wind
                || hang_blunt < dcfg.min_bluntness_for_wind
                || stand_blunt < dcfg.min_bluntness_for_wind
            {
                return None;
            }
            let speed = dcfg.wind_speed.get(&mut probe);
            let angle = mth_next_f32(&mut probe, 0.0, std::f32::consts::PI);
            Some((
                seed,
                radius,
                (
                    (mth_cos(angle) * speed) as f64,
                    (mth_sin(angle) * speed) as f64,
                ),
            ))
        })
        .expect("some seed under 64 must clear the wind gate");

    let before = ctx.wr.clone();
    let mut rng = Xor::seed(seed);
    gen_cf(&cf, freg(), &mut ctx, &mut rng, (ox, oy, oz));
    let with_wind = changed(&before, &ctx);

    // `Uniform(0.0, 0.0)` draws exactly as much as the pack's uniform speed, so the two
    // runs share a stream and differ only in the displacement
    let plain_cfg = LargeDripstoneCfg {
        range: dcfg.range,
        radius: dcfg.radius.clone(),
        height_scale: dcfg.height_scale.clone(),
        ratio: dcfg.ratio,
        stalactite_bluntness: dcfg.stalactite_bluntness.clone(),
        stalagmite_bluntness: dcfg.stalagmite_bluntness.clone(),
        wind_speed: FP::Uniform(0.0, 0.0),
        min_radius_for_wind: dcfg.min_radius_for_wind,
        min_bluntness_for_wind: dcfg.min_bluntness_for_wind,
    };
    let mut ctx2 = context(&mut g);
    dripstone_gallery(&mut ctx2, ox, oz, floor_y, 15, air);
    let before2 = ctx2.wr.clone();
    let mut rng2 = Xor::seed(seed);
    gen_cf(
        &Cf::LargeDripstone(Box::new(plain_cfg)),
        freg(),
        &mut ctx2,
        &mut rng2,
        (ox, oy, oz),
    );
    let plain = changed(&before2, &ctx2);

    assert!(!with_wind.is_empty() && with_wind.len() == plain.len());
    let (vx, vz) = wind_v;
    let expect: std::collections::BTreeSet<((i32, i32, i32), u32)> = with_wind
        .iter()
        .map(|((px, py, pz), st)| {
            let dy = (oy - py) as f64;
            (
                (
                    px - (vx * dy).floor() as i32,
                    *py,
                    pz - (vz * dy).floor() as i32,
                ),
                *st,
            )
        })
        .collect();
    let got: std::collections::BTreeSet<((i32, i32, i32), u32)> = plain.iter().copied().collect();
    assert_eq!(
        expect, got,
        "the wind moves each cell by floor(wind * (origin_y - y))"
    );
    println!(
        "LARGE wind: seed={seed} cells={} radius={radius} wind={vx:.4},{vz:.4}",
        with_wind.len()
    );
    assert_eq!(with_wind.len(), 126, "golden cell count");
}
