use super::*;
use crate::block_registry::{breg, HeightmapKind};
use crate::worldgen::DimId;

#[cfg(megu_vanilla_data)]
fn fixture() -> J {
    jparse(include_bytes!("../tests/data/mineshaft_pipeline_1211.json")).unwrap()
}
fn array<'a>(v: &'a J, key: &str) -> &'a Vec<J> {
    field(v, key).as_arr().unwrap()
}
fn int(v: &J, key: &str) -> i32 {
    field(v, key).num() as i32
}
fn seed(v: &J) -> u64 {
    field(v, "seed").str().parse::<i64>().unwrap() as u64
}
fn state128(v: &J, key: &str) -> (u64, u64) {
    let a = array(v, key);
    (
        a[0].str().parse::<i64>().unwrap() as u64,
        a[1].str().parse::<i64>().unwrap() as u64,
    )
}

#[test]
fn native_admission_retries_with_fresh_layout_and_noise_biomes() {
    let f = fixture();
    let cfg = config().unwrap();
    for e in &cfg.entries {
        let slot = array(&f, "slots")
            .iter()
            .find(|v| v.as_arr().unwrap()[0].str() == e.id)
            .unwrap()
            .as_arr()
            .unwrap();
        assert_eq!(
            (e.step, e.slot),
            (slot[1].num() as usize, slot[2].num() as usize),
            "{}",
            e.id
        );
        let members: HashSet<_> = field(field(&f, "memberships"), &e.id)
            .as_arr()
            .unwrap()
            .iter()
            .map(|s| s.str().to_string())
            .collect();
        assert_eq!(e.biomes, members, "{}", e.id);
    }
    let mut trials = 0;
    let mut accepted = 0;
    for case in array(&f, "cases") {
        let mut g = Gen::load(seed(case), crate::name_to_state).unwrap();
        let expected = array(case, "accepted_starts");
        for row in array(case, "frequency_trials") {
            let row = row.as_arr().unwrap();
            let (x, z) = (row[0].num() as i32, row[1].num() as i32);
            let frequency = cfg.spread.candidate(g.wseed(), x, z) == (x, z)
                && cfg.reducer.allows(g.wseed(), cfg.salt, x, z, cfg.frequency);
            assert_eq!(
                frequency,
                row[2].num() != 0.,
                "frequency seed={} chunk={x},{z}",
                g.wseed()
            );
            let actual = admit(&mut g, x, z);
            let expected = expected.iter().find(|e| {
                let r = e.as_arr().unwrap();
                r[0].num() as i32 == x && r[1].num() as i32 == z
            });
            assert_eq!(
                actual.is_some(),
                expected.is_some(),
                "admission seed={} chunk={x},{z}",
                g.wseed()
            );
            if let (Some(actual), Some(expected)) = (actual, expected) {
                let expected = expected.as_arr().unwrap();
                assert_eq!(cfg.entries[actual.entry].id, expected[2].str());
                assert_eq!(actual.pieces.len(), expected[3].num() as usize);
                accepted += 1;
            }
            trials += 1;
        }
    }
    assert_eq!((trials, accepted), (1734, 6));
    for row in array(&f, "mesa_admissions") {
        let row = row.as_arr().unwrap();
        let mut g = Gen::load(row[0].str().parse::<u64>().unwrap(), crate::name_to_state).unwrap();
        assert_eq!(
            field(&f, "mesa_admission_biome_source").str(),
            "fixed:minecraft:badlands"
        );
        let mut queried = Vec::new();
        let actual = admit_with_biome(&mut g, row[1].num() as i32, row[2].num() as i32, |_, p| {
            queried.push(p);
            Some("minecraft:badlands".into())
        })
        .expect("fixed-biome Mesa start rejected");
        assert_eq!(cfg.entries[actual.entry].id, row[3].str());
        assert_eq!(actual.pieces.len(), row[4].num() as usize);
        assert_eq!(queried.last().unwrap().1, row[5].num() as i32);
        assert_eq!(queried.last().unwrap().0, (row[1].num() as i32 * 16) + 8);
        assert_eq!(queried.last().unwrap().2, (row[2].num() as i32 * 16) + 8);
    }
}

#[test]
fn native_mesa_adjustment_uses_generator_height_and_preserves_rng() {
    for case in array(&fixture(), "mesa") {
        let mut queried = Vec::new();
        let l = mineshaft_layout::mesa_layout(
            seed(case),
            int(case, "sx"),
            int(case, "sz"),
            63,
            -64,
            |x, z| {
                queried.push((x, z));
                int(case, "height")
            },
        );
        assert_eq!(queried, vec![(int(case, "x"), int(case, "z"))]);
        assert_eq!(l.vertical_shift, int(case, "shift"));
        assert_eq!(l.pieces.len(), int(case, "count") as usize);
        assert_eq!(
            l.checkpoints[4],
            field(case, "rng_state48").str().parse::<u64>().unwrap()
        );
        let mut g = Gen::load(seed(case), crate::name_to_state).unwrap();
        assert_eq!(
            g.top_ws(int(case, "x"), int(case, "z")),
            int(case, "height"),
            "Mesa generator height seed={}",
            seed(case)
        );
    }
}

#[test]
fn native_reference_iteration_survives_duplicates_zero_and_rehash() {
    for case in array(&fixture(), "reference_orders") {
        let a = case.as_arr().unwrap();
        let read = |j: &J| {
            j.as_arr()
                .unwrap()
                .iter()
                .map(|s| s.str().parse::<i64>().unwrap() as u64)
                .collect::<Vec<_>>()
        };
        let input = read(&a[0]);
        let expected = read(&a[1]);
        assert_eq!(
            reference_order(input.into_iter().flat_map(|k| [k, k])),
            expected
        );
    }
}

fn authored_scene<'a>(g: &'a mut Gen, case: &J) -> FC<'a> {
    let leaf = g.biome_leaf_of_name("minecraft:snowy_plains").unwrap();
    let stone = breg().unwrap().state_of("minecraft:stone", None);
    let (cx, cz) = (int(case, "cx"), int(case, "cz"));
    let mut ctx = FC::new(g);
    ctx.test_no_terrain = true;
    ctx.test_biome_leaf = Some(leaf);
    ctx.test_writes = Some(Vec::new());
    for z in cz * 16..cz * 16 + 16 {
        for x in cx * 16..cx * 16 + 16 {
            ctx.test_frozen_height(
                HeightmapKind::OceanFloorWg,
                x,
                z,
                int(case, "first_available"),
            );
            for y in -64..320 {
                let occupied = if field(case, "scene").str() == "suspended" {
                    y >= int(case, "cavern") + 6
                } else {
                    field(case, "scene").str() == "solid" || y != int(case, "cavern")
                };
                let s = if y < int(case, "terrain_top") && occupied {
                    stone
                } else {
                    0
                };
                ctx.test_base_cell((x, y, z), s);
            }
        }
    }
    ctx
}

fn actual_events(ctx: &FC) -> Vec<((i32, i32, i32), String)> {
    ctx.test_writes
        .as_ref()
        .unwrap()
        .iter()
        .map(|(p, s)| (*p, breg().unwrap().canonical_state(*s)))
        .collect()
}
fn expected_events(case: &J) -> Vec<((i32, i32, i32), String)> {
    array(case, "events")
        .iter()
        .map(|v| {
            let r = v.as_arr().unwrap();
            (
                (r[0].num() as i32, r[1].num() as i32, r[2].num() as i32),
                r[3].str().to_string(),
            )
        })
        .collect()
}

#[test]
fn native_dispatcher_preserves_registry_slot_and_shared_piece_rng() {
    let f = fixture();
    let mut cases = 0;
    for case in array(&f, "writers")
        .iter()
        .filter(|v| int(v, "child") < 0 && field(v, "variant").str() == "normal")
    {
        let mut g = Gen::load(seed(case), crate::name_to_state).unwrap();
        let (cx, cz) = (int(case, "cx"), int(case, "cz"));
        let mut references = References::collect(&mut g, cx, cz);
        let mut ctx = authored_scene(&mut g, case);
        let pop = field(case, "population_seed").str().parse::<i64>().unwrap() as u64;
        references.place_step(&mut ctx, 3, pop, cx, cz);
        assert_eq!(
            references.rng_after[&(3, int(case, "slot") as usize)],
            state128(case, "rng_after"),
            "dispatcher RNG after all referenced pieces"
        );
        assert_eq!(
            actual_events(&ctx),
            expected_events(case),
            "procedural dispatcher, WG={}",
            int(case, "first_available")
        );
        cases += 1;
    }
    assert_eq!(cases, 2);
}

#[test]
fn native_registry_dispatch_order_is_independent_of_selector_order() {
    let f = fixture();
    let mut entries = config().unwrap().entries.clone();
    entries.reverse();
    let expected: Vec<_> = array(&f, "slots")
        .iter()
        .filter_map(|v| {
            let r = v.as_arr().unwrap();
            entries
                .iter()
                .any(|e| e.id == r[0].str())
                .then_some(r[0].str())
        })
        .collect();
    let actual: Vec<_> = decoration_entries(&entries, 3)
        .into_iter()
        .map(|(_, e)| e.id.as_str())
        .collect();
    assert_eq!(actual, expected);
}

#[test]
fn native_writer_order_clipping_and_rng_on_authored_scenes() {
    let f = fixture();
    let mut counts = HashMap::new();
    for case in array(&f, "writers") {
        let mut g = Gen::load(seed(case), crate::name_to_state).unwrap();
        let (cx, cz) = (int(case, "cx"), int(case, "cz"));
        let variant = match field(case, "variant").str() {
            "normal" => Variant::Normal,
            "mesa" => Variant::Mesa,
            _ => panic!("unknown variant"),
        };
        let mut layout = match variant {
            Variant::Normal => mineshaft_layout::normal_layout(
                seed(case),
                int(case, "sx"),
                int(case, "sz"),
                63,
                -64,
            ),
            Variant::Mesa => mineshaft_layout::mesa_layout(
                seed(case),
                int(case, "sx"),
                int(case, "sz"),
                63,
                -64,
                |x, z| g.top_ws(x, z),
            ),
        };
        let child = int(case, "child");
        let pieces = if child < 0 {
            &mut layout.pieces[..]
        } else {
            &mut layout.pieces[child as usize..=child as usize]
        };
        let mut ctx = authored_scene(&mut g, case);
        let pop = field(case, "population_seed").str().parse::<i64>().unwrap() as u64;
        let mut rng = Xor::seed_worldgen(pop.wrapping_add(int(case, "slot") as u64 + 30_000));
        assert_eq!(rng.state128(), state128(case, "rng_before"));
        crate::mineshaft_piece::place(
            &mut ctx,
            &mut rng,
            cx,
            cz,
            int(case, "sx"),
            int(case, "sz"),
            variant,
            pieces,
            &config().unwrap().blocking,
        );
        let actual = actual_events(&ctx);
        let expected = expected_events(case);
        for (pos, name) in &expected {
            if name == "minecraft:spawner" {
                assert!(
                    ctx.payloads.iter().any(|(p, data)| {
                        p == pos
                            && matches!(data, crate::generation_payload::GenerationPayload::Spawner)
                    }),
                    "native spawner write has no staged block entity at {pos:?}"
                );
            }
        }
        let carts: Vec<_> = ctx
            .payloads
            .iter()
            .filter_map(|(p, payload)| {
                if let crate::generation_payload::GenerationPayload::ChestMinecart { loot_seed } =
                    payload
                {
                    Some((
                        [p.0 as f64 + 0.5, p.1 as f64 + 0.5, p.2 as f64 + 0.5],
                        *loot_seed,
                    ))
                } else {
                    None
                }
            })
            .collect();
        let expected_carts: Vec<_> = array(case, "entities")
            .iter()
            .map(|v| {
                let a = v.as_arr().unwrap();
                assert_eq!(a[3].str(), "minecraft:chests/abandoned_mineshaft");
                (
                    [a[0].num(), a[1].num(), a[2].num()],
                    a[4].str().parse::<i64>().unwrap(),
                )
            })
            .collect();
        assert_eq!(carts, expected_carts, "actual addFreshEntity child={child}");
        for (ordinal, (actual, expected)) in actual.iter().zip(&expected).enumerate() {
            assert_eq!(
                actual, expected,
                "writer child={child} chunk={cx},{cz} event={ordinal}"
            );
        }
        assert_eq!(
            actual.len(),
            expected.len(),
            "writer event count child={child}"
        );
        assert_eq!(
            rng.state128(),
            state128(case, "rng_after"),
            "writer RNG child={child}"
        );
        *counts
            .entry(if child < 0 {
                "start"
            } else {
                match pieces[0].kind {
                    mineshaft_layout::Kind::Room { .. } => "room",
                    mineshaft_layout::Kind::Stairs => "stairs",
                    mineshaft_layout::Kind::Crossing { .. } => "crossing",
                    _ => "corridor",
                }
            })
            .or_insert(0) += 1;
    }
    assert!(counts
        .keys()
        .all(|k| ["start", "room", "stairs", "crossing", "corridor"].contains(k)));
    assert_eq!(counts.len(), 5);
    let writers = array(&f, "writers");
    assert!(writers.iter().any(|v| field(v, "variant").str() == "mesa"));
    assert!(writers
        .iter()
        .any(|v| field(v, "scene").str() == "suspended"));
    assert!(writers
        .iter()
        .any(|v| int(v, "first_available") != int(v, "terrain_top")));
}

#[test]
fn production_populate_reaches_admission_dispatch_and_out_of_bounds_floor() {
    let reg = breg().unwrap();
    let mut g = Gen::load(4242, crate::name_to_state).unwrap();
    crate::feature::populate(&mut g, 3, -2);
    for x in [62, 63] {
        let s = g
            .feature_override(x, 21, -17)
            .expect("production mineshaft floor never written");
        assert_eq!(reg.canonical_state(s), "minecraft:oak_planks");
    }
    let s = g
        .feature_override(63, 5, -30)
        .expect("production stairs never reached");
    assert_eq!(reg.canonical_state(s), "minecraft:cave_air");
}

#[cfg(megu_vanilla_data)]
#[test]
fn mineshaft_support_distinguishes_full_faces_center_and_falling_blocks() {
    let r = breg().unwrap();
    assert_eq!(
        include_bytes!("../assets/mineshaft_support_1211.bin").len(),
        r.state_count()
    );
    let stone = r.state_of("minecraft:stone", None);
    assert!(r.mineshaft_sturdy_face(stone, 1));
    assert!(r.mineshaft_chain_anchor(stone));
    let sand = r.state_of("minecraft:sand", None);
    assert!(r.mineshaft_sturdy_face(sand, 1));
    assert!(!r.mineshaft_chain_anchor(sand));
    assert!(!r.mineshaft_sturdy_face(r.state_of("minecraft:air", None), 1));
    // Absent moving-piston entity is an explicit authored oracle input, not an
    // ignored unsupported proxy call or a claimed runtime piston shape.
    assert!(!r.mineshaft_sturdy_face(r.state_of("minecraft:moving_piston", None), 1));
}

#[test]
fn native_reference_containers_do_not_share_capacity_between_types() {
    let f = fixture();
    let orders = array(&f, "reference_orders");
    let read = |n: usize, side: usize| {
        orders[n].as_arr().unwrap()[side]
            .as_arr()
            .unwrap()
            .iter()
            .map(|v| v.str().parse::<i64>().unwrap() as u64)
            .collect::<Vec<_>>()
    };
    let first = read(4, 0);
    let second = read(6, 0);
    let actual = reference_order_by_type(
        first
            .iter()
            .map(|&k| (0, k))
            .chain(second.iter().map(|&k| (1, k))),
    );
    for (entry, row) in [(0, 4), (1, 6)] {
        assert_eq!(
            actual
                .iter()
                .filter(|(e, _)| *e == entry)
                .map(|(_, k)| *k)
                .collect::<Vec<_>>(),
            read(row, 1),
            "native LongSet capacity belongs to one structure type"
        );
    }
}

#[test]
fn natural_overworld_mesa_admissions_match_native_without_fixed_biomes() {
    let f = fixture();
    assert_eq!(
        field(&f, "natural_mesa_biome_source").str(),
        "minecraft:multi_noise/overworld"
    );
    for case in array(&f, "natural_mesas") {
        let a = case.as_arr().unwrap();
        let mut g = Gen::load(
            a[0].str().parse::<i64>().unwrap() as u64,
            crate::name_to_state,
        )
        .unwrap();
        let start = admit(&mut g, a[1].num() as i32, a[2].num() as i32)
            .expect("natural Mesa start rejected");
        assert_eq!(
            config().unwrap().entries[start.entry].id,
            "minecraft:mineshaft_mesa"
        );
        assert_eq!(start.pieces.len(), a[3].num() as usize);
        let tags = a[4].as_arr().unwrap();
        assert_eq!(start.pieces.len(), tags.len());
        for (piece, tag) in start.pieces.iter().zip(tags) {
            let bounds: Vec<_> = array(tag, "BB").iter().map(|v| v.num() as i32).collect();
            assert_eq!(&piece.bounds[..], bounds);
            assert_eq!(piece.depth, int(tag, "GD"));
            assert_eq!(int(tag, "MST"), 1);
            use crate::mineshaft_layout::Facing;
            let facing = match piece.orientation {
                None => -1,
                Some(Facing::South) => 0,
                Some(Facing::West) => 1,
                Some(Facing::North) => 2,
                Some(Facing::East) => 3,
            };
            assert_eq!(facing, int(tag, "O"));
        }
    }
}

#[test]
fn native_create_references_and_manager_order_drive_multiple_start_writes() {
    let f = fixture();
    let case = field(&f, "multi_start");
    let mut g = Gen::load(seed(case), crate::name_to_state).unwrap();
    let (cx, cz) = (int(case, "cx"), int(case, "cz"));
    let mut refs = References::collect(&mut g, cx, cz);
    let order: Vec<_> = refs.starts.iter().map(|(k, _)| *k).collect();
    let expected: Vec<_> = array(case, "reference_order")
        .iter()
        .map(|v| v.str().parse::<i64>().unwrap() as u64)
        .collect();
    assert_eq!(order, expected);
    assert!(order.len() >= 2);
    assert!(
        array(case, "start_write_counts")
            .iter()
            .filter(|n| n.num() > 0.)
            .count()
            >= 2
    );
    let pop = field(case, "population_seed").str().parse::<i64>().unwrap() as u64;
    let mut ctx = authored_scene(&mut g, case);
    refs.place_step(&mut ctx, 3, pop, cx, cz);
    assert_eq!(actual_events(&ctx), expected_events(case));
    assert_eq!(refs.rng_after[&(3, 1)], state128(case, "rng_after"));
}

#[test]
fn queued_generation_payloads_never_take_the_save_lock_on_a_cell_read() {
    if crate::test_support::isolated_world_test() {
        return;
    }
    assert!(crate::worldgen::init(0, crate::name_to_state));
    crate::worldgen::with_gen(|g| {
        let mut p = vec![(
            (7, 20, 7),
            crate::generation_payload::GenerationPayload::Spawner,
        )];
        g.stage_payloads((512, 512), &mut p);
    });
    crate::worldgen::block_state(0, 64, 0, |_, _, _| {}); // warm before the timeout assertion
    let save = crate::SAVE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (tx, rx) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        crate::wg2_block(0, 64, 0);
        tx.send(()).unwrap();
    });
    let ready = rx.recv_timeout(std::time::Duration::from_secs(2)).is_ok();
    drop(save);
    worker.join().unwrap();
    assert!(
        ready,
        "a cell read acquired SAVE_LOCK (mob/player lock inversion)"
    );
    assert!(
        crate::generated_content()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .spawners
            .is_empty(),
        "a block read prematurely materialized payloads"
    );
    crate::materialize_pending_generation();
    assert!(crate::generated_content()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .spawners
        .contains_key(&(DimId::Overworld, (7, 20, 7))));
    let p = (8, 20, 7);
    crate::world()
        .write(&(crate::worldgen::DimId::Overworld, 0, 0))
        .entry((crate::worldgen::DimId::Overworld, 0, 0))
        .or_default()
        .insert(crate::local_idx(p.0, p.1, p.2), 0);
    crate::materialize_generation_payloads(vec![(
        p,
        crate::generation_payload::GenerationPayload::Spawner,
    )]);
    assert!(
        !crate::generated_content()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .spawners
            .contains_key(&(DimId::Overworld, p)),
        "handoff overwrote a user air edit"
    );
    assert_eq!(crate::block_at(p.0, p.1, p.2), 0);
}

fn stage_native_case(g: &mut Gen, case: &J) {
    assert_eq!(field(case, "variant").str(), "normal");
    let mut layout =
        mineshaft_layout::normal_layout(seed(case), int(case, "sx"), int(case, "sz"), 63, -64);
    let child = int(case, "child") as usize;
    let mut ctx = authored_scene(g, case);
    let pop = field(case, "population_seed").str().parse::<i64>().unwrap() as u64;
    let mut rng = Xor::seed_worldgen(pop.wrapping_add(int(case, "slot") as u64 + 30000));
    crate::mineshaft_piece::place(
        &mut ctx,
        &mut rng,
        int(case, "cx"),
        int(case, "cz"),
        int(case, "sx"),
        int(case, "sz"),
        Variant::Normal,
        &mut layout.pieces[child..=child],
        &config().unwrap().blocking,
    );
    let mut payloads = std::mem::take(&mut ctx.payloads);
    g.stage_payloads((int(case, "cx"), int(case, "cz")), &mut payloads);
}

pub(crate) fn strip_outer_footers(payload: &[u8]) -> &[u8] {
    let (payload, _) = crate::read_world_seed_footer(payload).unwrap();
    let (payload, _) = crate::read_mob_dimensions_footer(payload).unwrap();
    let (payload, _) = crate::read_brewing_footer(payload).unwrap();
    let (payload, _) = crate::read_dim2_footer(payload).unwrap();
    let (payload, _, _) = crate::read_dim_footer(payload).unwrap();
    let (payload, _) = crate::read_block_loot_footer(payload).unwrap();
    let (payload, _) = crate::read_drop_owners_footer(payload).unwrap();
    payload
}

#[cfg(megu_vanilla_data)]
#[test]
fn generated_entities_and_mutated_spawner_nbt_resume_in_a_new_process_once() {
    if crate::test_support::isolated_world_test() {
        return;
    }
    assert!(crate::worldgen::init(0, crate::name_to_state));
    let f = fixture();
    let chest = array(&f, "writers")
        .iter()
        .find(|c| !array(c, "entities").is_empty())
        .unwrap();
    let spider = array(&f, "writers")
        .iter()
        .find(|c| {
            expected_events(c)
                .iter()
                .any(|(_, n)| n == "minecraft:spawner")
        })
        .unwrap();
    if let Ok(path) = std::env::var("MEGU_MINESHAFT_RESTART") {
        let raw = std::fs::read(path).unwrap();
        let payload = crate::unwrap_world_payload(raw).unwrap();
        let saved = crate::generated_content::Snapshot::read(
            crate::tick_persistence::prefix(strip_outer_footers(&payload)).unwrap(),
        )
        .unwrap()
        .unwrap();
        crate::load_world_from(&payload).unwrap();
        let before = crate::generated_content().lock().unwrap_or_else(|e| e.into_inner()).clone();
        assert_eq!(before.carts.len(), 1);
        assert_eq!(
            before.carts[0].data, saved.content.carts[0].data,
            "cart UUID/position/loot changed on load"
        );
        let expected_seed = array(chest, "entities")[0].as_arr().unwrap()[4]
            .str()
            .parse::<i64>()
            .unwrap();
        assert_eq!(
            before.carts[0].data.get("LootTableSeed"),
            Some(&crate::nbt::Tag::Long(expected_seed))
        );
        assert_eq!(
            before.spawners.values().next().unwrap().get("Delay"),
            Some(&crate::nbt::Tag::Short(137))
        );
        let packet = crate::spawn_generated_cart(&before.carts[0]);
        let mut wire = &packet[..];
        assert_eq!(
            crate::read_varint(&mut wire).unwrap(),
            before.carts[0].eid as i64
        );
        assert_eq!(&wire[..16], &before.carts[0].uuid().unwrap());
        wire = &wire[16..];
        assert_eq!(crate::read_varint(&mut wire).unwrap(), 18);
        crate::worldgen::with_gen(|g| {
            assert_eq!(g.payload_sources.len(), 2);
            assert!(!g.mineshaft_spawners.is_empty());
            stage_native_case(g, chest);
            stage_native_case(g, spider);
            assert!(
                g.take_staged_payloads().is_empty(),
                "restart/recomputed source emitted a second payload"
            );
        });
        assert_eq!(*crate::generated_content().lock().unwrap_or_else(|e| e.into_inner()), before);
        let &(dim, (x, y, z)) = before.spawners.keys().next().unwrap();
        assert_eq!(dim, DimId::Overworld);
        let entities = crate::generated_spawner_chunk_entities(dim, x >> 4, z >> 4);
        assert_eq!(entities.len(), 1);
        let meta = jparse(include_bytes!("../tests/data/mineshaft_data_1211.json")).unwrap();
        assert_eq!(
            entities[0].1,
            field(&meta, "spawner_packet_type_id").num() as i64
        );
        let mut named = vec![10, 0, 0];
        named.extend_from_slice(&entities[0].2[1..]);
        assert_eq!(
            crate::nbt::read_named(&named).unwrap().1,
            before.spawners[&(dim, (x, y, z))]
        );
        assert_eq!(
            crate::block_at_dim(dim, x, y, z),
            crate::name_to_state("spawner").unwrap()
        );
        // Breaking the block removes its NBT and an explicit air overlay survives.
        crate::set_block_dim(dim, x, y, z, 0);
        assert!(crate::generated_content()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .spawners
            .is_empty());
        crate::generated_content().lock().unwrap_or_else(|e| e.into_inner()).carts.clear();
        crate::worldgen::with_gen(|g| {
            stage_native_case(g, chest);
            stage_native_case(g, spider);
            assert!(g.take_staged_payloads().is_empty());
        });
        let mut deleted = Vec::new();
        crate::save_world_into(&mut deleted);
        let deleted = crate::generated_content::Snapshot::read(
            crate::tick_persistence::prefix(strip_outer_footers(&deleted)).unwrap(),
        )
        .unwrap()
        .unwrap();
        assert!(deleted.content.spawners.is_empty() && deleted.content.carts.is_empty());
        assert!(deleted.sources.is_superset(&saved.sources),
            "entity removal must not erase the source tombstone (neighbor updates may populate additional chunks)");
        return;
    }
    crate::worldgen::with_gen(|g| stage_native_case(g, chest));
    let mut pending_save = Vec::new();
    crate::save_world_into(&mut pending_save);
    assert_eq!(
        crate::generated_content().lock().unwrap_or_else(|e| e.into_inner()).carts.len(),
        1,
        "save dropped an unmaterialized payload"
    );
    crate::worldgen::with_gen(|g| stage_native_case(g, spider));
    crate::materialize_pending_generation();
    {
        let mut content = crate::generated_content().lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(content.carts.len(), 1);
        assert_eq!(content.spawners.len(), 1);
        if let crate::nbt::Tag::Compound(tag) = content.spawners.values_mut().next().unwrap() {
            tag.insert(
                crate::nbt::NbtString::text("Delay"),
                crate::nbt::Tag::Short(137),
            );
        }
    }
    // Exercise the real interest/tracker dispatch, not just a packet constructor.
    let cart = crate::generated_content().lock().unwrap_or_else(|e| e.into_inner()).carts[0].clone();
    let (client, mut receiver) = crate::effect_tests::client_pair();
    let [x, y, z] = cart.pos().unwrap();
    *client.pos.lock().unwrap_or_else(|e| e.into_inner()) = (x, y, z, 0., 0., true);
    client.chunks.lock().unwrap_or_else(|e| e.into_inner()).insert((
        (x.floor() as i32).div_euclid(16),
        (z.floor() as i32).div_euclid(16),
    ));
    crate::clients()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push(std::sync::Arc::new(client));
    crate::interest_tick();
    use std::io::Read;
    let mut head = Vec::new();
    loop {
        let mut b = [0];
        receiver.read_exact(&mut b).unwrap();
        head.push(b[0]);
        if b[0] & 128 == 0 {
            break;
        }
    }
    let size = crate::read_varint(&mut &head[..]).unwrap() as usize;
    let mut frame = vec![0; size];
    receiver.read_exact(&mut frame).unwrap();
    let mut wire = &frame[..];
    assert_eq!(crate::read_varint(&mut wire).unwrap(), 1);
    assert_eq!(wire, crate::spawn_generated_cart(&cart));
    crate::clients().lock().unwrap_or_else(|e| e.into_inner()).clear();
    let mut payload = Vec::new();
    crate::save_world_into(&mut payload);
    let mut again = Vec::new();
    crate::save_world_into(&mut again);
    assert_eq!(
        payload, again,
        "saving must not reroll loot or regenerate entities"
    );
    let path = std::env::temp_dir().join(format!(
        "megu-mineshaft-restart-{}.bin",
        std::process::id()
    ));
    std::fs::write(&path, crate::wrap_world_payload(&payload)).unwrap();
    let name =
        "mineshaft::tests::generated_entities_and_mutated_spawner_nbt_resume_in_a_new_process_once";
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", name, "--test-threads=1", "--nocapture"])
        .env("MEGU_TEST_CHILD", name)
        .env("MEGU_MINESHAFT_RESTART", &path)
        .status()
        .unwrap();
    assert!(
        status.success(),
        "fresh-process resume failed; retained {}",
        path.display()
    );
    std::fs::remove_file(path).unwrap();
}
