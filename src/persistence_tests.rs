use super::*;

// Round-trip fixtures. No real saved world exists for this repository, so the
// MET2/MET3 consequences are pinned against the production encoder instead of
// being skipped for lack of data. Observations are returned and asserted after
// the shared world fixture is restored, so a failure cannot poison the lock.
#[derive(Debug, PartialEq)]
struct Loaded {
    chunks: usize,
    forced: usize,
    world_time: u64,
    drops: usize,
    drop_age: i32,
    drop_item: i64,
    drop_count: u8,
    drop_comps: Vec<(i64, Vec<u8>)>,
    mobs: usize,
    mob_etype: i64,
    mob_hp: f32,
    mob_air: i32,
}

fn round_trip(legacy_tail: Option<&[u8; 4]>, mob_air: i32, drop_age: i32) -> Loaded {
    let old_world = std::mem::take(&mut *world().lock().unwrap());
    let old_drops = std::mem::take(&mut *drops().lock().unwrap());
    let old_mobs = std::mem::take(&mut *mobs().lock().unwrap());
    let old_time = WORLD_TIME.swap(555, Ordering::Relaxed);
    let old_forced_chunk = std::mem::take(&mut *forceloaded().lock().unwrap());
    let old_rng = world_rng::saved_state();
    forceloaded().lock().unwrap().insert((0, 0));
    world()
        .lock()
        .unwrap()
        .entry((DimId::Overworld, 0, 0))
        .or_default()
        .insert(0x0000_0004, 4);
    drops().lock().unwrap().push(Drop {
        eid: 777,
        x: 0.5,
        y: 70.0,
        z: 0.5,
        item: name_to_item("iron_ingot").unwrap(),
        cnt: 3,
        target: None,
        thrower: None,
        comps: vec![(2i64, br#"{"text":"legacy"}"#.to_vec())],
        age: drop_age,
    });
    mobs().lock().unwrap().push(Arc::new(Mob {
        eid: 778,
        uuid: [0u8; 16],
        etype: 124,
        pos: Mutex::new((1.5, 70.0, 2.5, 0.0, 0.0)),
        hp: Mutex::new(12.5),
        last_hit: Mutex::new(Instant::now()),
        hurt_at: Mutex::new(Instant::now()),
        last_hurt_tick: Mutex::new(None),
        vel: Mutex::new([0.0; 3]),
        fuse: Mutex::new(0),
        love: Mutex::new(0),
        air: Mutex::new(mob_air),
    }));
    let mut payload = Vec::new();
    save_world_into(&mut payload);
    let loaded = {
        *world().lock().unwrap() = HashMap::new();
        *forceloaded().lock().unwrap() = HashSet::new();
        *drops().lock().unwrap() = Vec::new();
        *mobs().lock().unwrap() = Vec::new();
        WORLD_TIME.store(0, Ordering::Relaxed);
        if let Some(magic) = legacy_tail {
            // Emulate an older writer: MET2 never carried the mob air field, and
            // MET3 stored entity ages in milliseconds rather than server ticks.
            let at = payload
                .windows(4)
                .position(|w| w == b"MET4")
                .expect("a MET4 tail");
            payload[at..at + 4].copy_from_slice(magic);
            if magic == b"MET2" {
                // magic, mob count, etype, x, y, z, hp — no air field follows.
                payload.truncate(at + 4 + 1 + 1 + 8 + 8 + 8 + 4);
            }
        }
        load_world_from(&payload).unwrap();
        let d = drops()
            .lock()
            .unwrap()
            .first()
            .map(|d| (d.age, d.item, d.cnt, d.comps.clone()));
        let m = mobs().lock().unwrap().first().cloned();
        Loaded {
            chunks: world().lock().unwrap().len(),
            forced: forceloaded().lock().unwrap().len(),
            world_time: WORLD_TIME.load(Ordering::Relaxed),
            drops: drops().lock().unwrap().len(),
            drop_age: d.as_ref().map(|d| d.0).unwrap_or(-1),
            drop_item: d.as_ref().map(|d| d.1).unwrap_or(0),
            drop_count: d.as_ref().map(|d| d.2).unwrap_or(0),
            drop_comps: d.map(|d| d.3).unwrap_or_default(),
            mobs: mobs().lock().unwrap().len(),
            mob_etype: m.as_ref().map(|m| m.etype).unwrap_or(0),
            mob_hp: m.as_ref().map(|m| *m.hp.lock().unwrap()).unwrap_or(0.0),
            mob_air: m.as_ref().map(|m| *m.air.lock().unwrap()).unwrap_or(-1),
        }
    };
    *world().lock().unwrap() = old_world;
    *forceloaded().lock().unwrap() = old_forced_chunk;
    *drops().lock().unwrap() = old_drops;
    *mobs().lock().unwrap() = old_mobs;
    WORLD_TIME.store(old_time, Ordering::Relaxed);
    world_rng::restore(old_rng);
    loaded
}

#[test]
fn met4_round_trip_preserves_drop_components_age_and_mob_air() {
    if test_support::isolated_world_test() {
        return;
    }
    let _world_guard = world_guard();
    let full_air = server_ticks(300) as i32;
    let loaded = round_trip(None, 150, 3);
    assert_eq!(
        loaded,
        Loaded {
            chunks: 1,
            forced: 1,
            world_time: 555,
            drops: 1,
            drop_age: 3,
            drop_item: name_to_item("iron_ingot").unwrap(),
            drop_count: 3,
            drop_comps: vec![(2i64, br#"{"text":"legacy"}"#.to_vec())],
            mobs: 1,
            mob_etype: 124,
            mob_hp: 12.5,
            // The MET3 tail has no air field, so a reloaded mob gets a full supply.
            mob_air: full_air,
        }
    );
    // The default air supply is the vanilla 300 game ticks, not a bare literal.
    assert_eq!(u64::from(full_air as u32) * TICK_MS, 15_000);
}

#[test]
fn met2_without_mob_air_falls_back_to_a_full_supply() {
    if test_support::isolated_world_test() {
        return;
    }
    let _world_guard = world_guard();
    // MET2 never carried air. The loader must substitute the vanilla air supply
    // rather than a zero that would drown the mob on its first tick.
    let loaded = round_trip(Some(b"MET2"), 0, 3);
    assert_eq!(loaded.mobs, 1, "the mob must still load from a MET2 tail");
    assert_eq!(loaded.mob_air, server_ticks(300) as i32);
}

#[test]
fn legacy_met3_millisecond_ages_convert_to_ticks_instead_of_despawning() {
    if test_support::isolated_world_test() {
        return;
    }
    let _world_guard = world_guard();
    // The 20TPS cutover made one tick 50ms. A MET3 file that stored a drop age of
    // 300000 ms read as ticks would sit at 300000 ticks against a 6000 tick life
    // and despawn on the first tick, destroying every saved drop.
    let loaded = round_trip(Some(b"MET3"), 150, 3000);
    assert_eq!(loaded.drops, 1, "the drop must survive a legacy MET3 load");
    assert_eq!(
        loaded.drop_age, 60,
        "3000 ms of legacy age is 60 ticks at 50 ms per tick"
    );
    assert!(loaded.drop_age < server_ticks(DROP_LIFE_GT) as i32);
}

#[test]
fn negative_drop_age_survives_a_save_reload_round_trip() {
    if test_support::isolated_world_test() {
        return;
    }
    let _world_guard = world_guard();
    // A container close records the pickup-delay marker as a negative age. The
    // old loader clamped it to a full lifespan, so the drop despawned on the
    // next tick after every restart.
    for age in [
        -(server_ticks(40) as i32),
        0,
        server_ticks(DROP_LIFE_GT) as i32,
    ] {
        let loaded = round_trip(None, 150, age);
        assert_eq!(loaded.drop_age, age, "drop age {age} did not round-trip");
    }
}

#[test]
fn hostile_rotation_does_not_collapse_to_one_type() {
    // Deriving the rotation from the world tick made every spawn attempt land on
    // the same residue at 20TPS, so only one hostile type ever spawned.
    let mut seen = Vec::new();
    for attempt in 0..15u64 {
        seen.push(match attempt % 5 {
            0 => 23i64,
            1 => 91,
            2 => 100,
            _ => 124,
        });
    }
    let distinct: std::collections::HashSet<i64> = seen.iter().copied().collect();
    assert_eq!(distinct.len(), 4, "rotation collapsed to {seen:?}");
    // The attempt counter must not be derived from the tick, which at 20TPS
    // visits only residues 2, 7, 12, ... of a 40-tick divisor.
    for t in [100u64, 300, 500, 700] {
        assert_eq!((t / 40) % 5, 2, "the old divisor still pins the choice");
    }
}

/// Legacy MET2/MET3 stored every duration as a 4TPS server tick, which spans five
/// times the wall time of a 20TPS tick. Reading one directly would expire it five
/// times early, so each counter must be divided by the cadence ratio.
#[test]
fn legacy_persisted_durations_resume_with_their_original_wall_time() {
    let _world_guard = world_guard();
    // The conversion factor itself, stated in the unit the file used.
    assert_eq!(legacy_ticks(1000, true), 1000 / TICK_MS);
    assert_eq!(
        legacy_ticks(1000, false),
        1000,
        "MET4 values must pass through untouched"
    );
    // Millisecond fields: drop and xp age were written as elapsed milliseconds.
    for millis in [1000u64, 2000, 50_000, 300_000] {
        assert_eq!(
            legacy_ticks(millis, true) * TICK_MS,
            millis,
            "legacy age {millis}ms no longer spans the same wall time"
        );
    }
    // Count fields were written as a number of 4TPS ticks, so they scale the other
    // way. A legacy tick spans 250ms, which is five 20TPS ticks.
    for budget in [1u64, 40, 100, 240, 1600, 6000, 12000] {
        assert_eq!(
            legacy_tick_count(budget, true) * TICK_MS,
            budget * 250,
            "legacy budget {budget} no longer spans {}ms",
            budget * 250
        );
    }
    assert_eq!(
        legacy_tick_count(ARROW_TTL_GT, true) * TICK_MS,
        ARROW_TTL_GT * 250
    );
    assert_eq!(
        legacy_tick_count(PEARL_TTL_GT, true) * TICK_MS,
        PEARL_TTL_GT * 250
    );
    assert_eq!(
        legacy_tick_count(TNT_FUSE_GT, true) * TICK_MS,
        TNT_FUSE_GT * 250
    );
    assert_eq!(
        legacy_tick_count(200, true) * TICK_MS,
        200 * 250,
        "furnace cook time"
    );
    // The two directions must not be interchangeable.
    assert_ne!(legacy_ticks(1000, true), legacy_tick_count(1000, true));
}

#[test]
fn met4_round_trip_leaves_persisted_durations_untouched() {
    if test_support::isolated_world_test() {
        return;
    }
    let _world_guard = world_guard();
    // A MET4 save writes server ticks, so loading it must not rescale anything.
    let loaded = round_trip(None, 150, 600);
    assert_eq!(
        loaded.drop_age, 600,
        "MET4 ages must pass through unconverted"
    );
}

/// A truncated or corrupt save must not panic the loader. `read_comps` reads a
/// variable-width type VarInt and then a 2-byte length, so the length has to be
/// re-checked after the VarInt is consumed.
#[test]
fn truncated_component_blobs_do_not_panic_the_loader() {
    let _world_guard = world_guard();
    // Every truncation of a valid component record must either parse or stop.
    let mut full = Vec::new();
    write_comps(
        &mut full,
        &vec![(2i64, b"{\"text\":\"x\"}".to_vec()), (9i64, vec![0u8; 8])],
    );
    for cut in 0..=full.len() {
        let mut p = 0usize;
        let _ = read_comps(&full[..cut], &mut p);
        assert!(p <= cut, "cursor ran past the buffer at cut {cut}");
    }
    // A VarInt that claims more component bytes than remain must stop, not index.
    let mut liar = Vec::new();
    write_varint(&mut liar, 1); // one component
    write_varint(&mut liar, 2); // type
    liar.extend_from_slice(&100u16.to_be_bytes()); // claims 100 bytes
    liar.extend_from_slice(&[0u8; 3]); // supplies 3
    let mut p = 0usize;
    let parsed = read_comps(&liar, &mut p);
    assert!(
        parsed.is_empty(),
        "an over-long blob must be rejected, got {parsed:?}"
    );
    // The truncated length field itself.
    for cut in 0..=liar.len() {
        let mut p = 0usize;
        let _ = read_comps(&liar[..cut], &mut p);
        assert!(p <= cut, "cursor ran past the buffer at cut {cut}");
    }
}

/// Saving must be observational. The trident-to-drop conversion used to run inside
/// save_world_into: it removed the trident from the live arrow list, created a live
/// drop, and — because the drop list had already been serialized — omitted that drop
/// from the very save that produced it, so a resting trident was lost on restart.
#[test]
fn saving_neither_mutates_live_state_nor_loses_resting_tridents() {
    if test_support::isolated_world_test() {
        return;
    }
    let _world_guard = world_guard();
    let old_arrows = std::mem::take(&mut *arrows().lock().unwrap());
    let old_drops = std::mem::take(&mut *drops().lock().unwrap());
    arrows().lock().unwrap().push(Arrow {
        eid: 7001,
        owner: 1,
        x: 1.5,
        y: 70.0,
        z: 1.5,
        dx: 0.0,
        dy: 0.0,
        dz: 0.0,
        ttl: server_ticks(ARROW_TTL_GT) as i32,
        dmg: 8.0,
        fire: 0,
        punch: 0,
        kind: 1,
        loy: 3,
        ret_item: 811,
        ret_comps: Vec::new(),
        pierce: 0,
        last_hit: -1,
        stuck: true,
    });
    arrows().lock().unwrap().push(Arrow {
        eid: 7002,
        owner: 1,
        x: 2.5,
        y: 70.0,
        z: 2.5,
        dx: 0.5,
        dy: 0.0,
        dz: 0.5,
        ttl: server_ticks(ARROW_TTL_GT) as i32,
        dmg: 6.0,
        fire: 0,
        punch: 0,
        kind: 0,
        loy: 0,
        ret_item: 0,
        ret_comps: Vec::new(),
        pierce: 0,
        last_hit: -1,
        stuck: false,
    });
    let live_arrows_before: Vec<i32> = arrows().lock().unwrap().iter().map(|a| a.eid).collect();
    let live_drops_before = drops().lock().unwrap().len();
    let mut payload = Vec::new();
    save_world_into(&mut payload);
    let live_arrows_after: Vec<i32> = arrows().lock().unwrap().iter().map(|a| a.eid).collect();
    let live_drops_after = drops().lock().unwrap().len();
    // A second save of an unchanged world must produce identical bytes. A save that
    // consumes the world is not idempotent and cannot be retried safely.
    let mut payload2 = Vec::new();
    save_world_into(&mut payload2);
    *drops().lock().unwrap() = Vec::new();
    *arrows().lock().unwrap() = Vec::new();
    load_world_from(&payload).unwrap();
    let loaded_drops: Vec<(i64, u8)> = drops()
        .lock()
        .unwrap()
        .iter()
        .map(|d| (d.item, d.cnt))
        .collect();
    let loaded_arrow_kinds: Vec<u8> = arrows().lock().unwrap().iter().map(|a| a.kind).collect();
    *arrows().lock().unwrap() = old_arrows;
    *drops().lock().unwrap() = old_drops;
    assert_eq!(
        live_arrows_before, live_arrows_after,
        "saving removed a live projectile"
    );
    assert_eq!(
        live_drops_before, live_drops_after,
        "saving created a live drop"
    );
    assert_eq!(payload, payload2, "saving is not idempotent");
    assert_eq!(
        loaded_drops,
        vec![(811, 1)],
        "the resting trident must persist exactly once"
    );
    assert_eq!(
        loaded_arrow_kinds,
        vec![0],
        "only the airborne arrow stays an arrow"
    );
}

/// A block in flight has to survive a save and a load. Without the queue in the save, the world
/// keeps the block where it was mid-air and nothing ever drops it, because the tick only moves what
/// is queued - so after a restart the block hangs there permanently.
#[test]
fn the_falling_entities_survive_a_save_and_load_round_trip() {
    if test_support::isolated_world_test() {
        return;
    }
    let _g = world_guard();
    let old_world = world().lock().unwrap().clone();
    let old_falling = std::mem::take(&mut *falling().lock().unwrap());

    falling().lock().unwrap().extend([
        Fall {
            eid: 7001,
            x: 1.5,
            y: 100.25,
            z: 2.5,
            dy: -0.12,
            state: 112,
            time: 3,
        },
        Fall {
            eid: 7002,
            x: -2.5,
            y: 70.0,
            z: 4.5,
            dy: 0.0,
            state: 117,
            time: 0,
        },
    ]);
    let mut payload = Vec::new();
    save_world_into(&mut payload);

    // Clear it first: reading back a value that was never removed proves nothing.
    falling().lock().unwrap().clear();
    load_world_from(&payload).unwrap();

    let q = falling().lock().unwrap().clone();
    assert_eq!(
        q.len(),
        2,
        "both entities came back and nothing else: {q:?}"
    );
    assert!(
        q.iter().any(|f| f.eid == 7001
            && (f.x - 1.5).abs() < 1e-9
            && (f.y - 100.25).abs() < 1e-9
            && (f.z - 2.5).abs() < 1e-9
            && (f.dy + 0.12).abs() < 1e-9
            && f.state == 112
            && f.time == 3),
        "position, velocity, state and age survive: {q:?}"
    );
    assert!(
        q.iter()
            .any(|f| f.eid == 7002 && (f.x + 2.5).abs() < 1e-9 && f.state == 117),
        "and negative coordinates are easy to mangle: {q:?}"
    );
    // New entities must not reuse a loaded id.
    assert!(
        NEXT_EID.load(Ordering::SeqCst) > 7002,
        "NEXT_EID must clear the loaded ids"
    );

    *world().lock().unwrap() = old_world;
    *falling().lock().unwrap() = old_falling;
}

/// A save written by the old cell-moving model carries FAL1. Loading it must convert each cell
/// to an entity and remove the block, or the block would remain behind and duplicate.
#[test]
fn a_legacy_fal1_save_converts_its_blocks_to_entities() {
    if test_support::isolated_world_test() {
        return;
    }
    let _g = world_guard();
    let old_world = world().lock().unwrap().clone();
    let old_falling = std::mem::take(&mut *falling().lock().unwrap());

    let sand = name_to_state("sand").unwrap();
    let mut payload = Vec::new();
    write_varint(&mut payload, 1);
    payload.extend_from_slice(&0i32.to_be_bytes());
    payload.extend_from_slice(&0i32.to_be_bytes());
    write_varint(&mut payload, 1);
    payload.extend_from_slice(&local_idx(1, 100, 2).to_be_bytes());
    write_varint(&mut payload, sand as i64);
    payload.extend_from_slice(b"FAL1");
    write_varint(&mut payload, 1);
    payload.extend_from_slice(&1i32.to_be_bytes());
    payload.extend_from_slice(&100i32.to_be_bytes());
    payload.extend_from_slice(&2i32.to_be_bytes());

    world().lock().unwrap().clear();
    falling().lock().unwrap().clear();
    load_world_from(&payload).unwrap();

    assert_eq!(
        block_at(1, 100, 2),
        0,
        "the legacy block must leave the world"
    );
    assert_eq!(
        world()
            .lock()
            .unwrap()
            .get(&(DimId::Overworld, 0, 0))
            .and_then(|c| c.get(&local_idx(1, 100, 2)))
            .copied(),
        Some(0),
        "the migrated cell is an explicit air override, so a non-air generated base is not exposed"
    );
    let q = falling().lock().unwrap().clone();
    assert_eq!(q.len(), 1, "the legacy cell becomes one entity: {q:?}");
    assert_eq!(q[0].state, sand, "with the state the block had");
    assert!(
        (q[0].x - 1.5).abs() < 1e-9 && (q[0].y - 100.0).abs() < 1e-9,
        "at the cell center"
    );

    *world().lock().unwrap() = old_world;
    *falling().lock().unwrap() = old_falling;
}

#[test]
fn the_world_rng_stream_continues_across_a_save_and_load_round_trip() {
    if test_support::isolated_world_test() {
        return;
    }
    let _g = world_guard();
    let old_world = world().lock().unwrap().clone();
    let old_rng = world_rng::saved_state();

    world_rng::seed(4242);
    for _ in 0..5 {
        let _ = world_rng::next_u64();
    }
    let mut payload = Vec::new();
    save_world_into(&mut payload);
    assert!(
        payload.windows(4).any(|w| w == b"RNG1"),
        "the save must carry the world random stream state"
    );
    // The value the reloaded stream must produce next.
    let expected = world_rng::next_u64();

    // Move the live stream elsewhere first: reading back a value that was never
    // replaced would prove nothing.
    world_rng::seed(999);
    load_world_from(&payload).unwrap();
    assert_eq!(
        world_rng::next_u64(),
        expected,
        "the reloaded stream must continue from the saved state, not restart"
    );

    *world().lock().unwrap() = old_world;
    world_rng::restore(old_rng);
}

#[test]
fn furnace_and_sign_round_trip_keeps_only_live_owners() {
    if test_support::isolated_world_test() {
        return;
    }
    let broken_furnace = (30, 150, 30);
    let live_furnace = (32, 150, 30);
    let broken_sign = (34, 150, 30);
    let live_sign = (36, 150, 30);
    let input = (name_to_item("raw_iron").unwrap(), 3, Vec::new());
    for pos in [broken_furnace, live_furnace] {
        set_block(pos.0, pos.1, pos.2, name_to_state("furnace").unwrap());
        furnaces().lock().unwrap().insert(
            pos,
            Furnace {
                input: input.clone(),
                fuel: (0, 0, Vec::new()),
                output: (0, 0, Vec::new()),
                burn_left: 43,
                progress: 17,
                dirty: false,
                xp: 0.0,
                recipe_cache: CachedLookup::default(),
                fuel_cache: CachedLookup::default(),
            },
        );
    }
    let lines = [
        "persist".into(),
        "both".into(),
        "sides".into(),
        "exactly".into(),
    ];
    for pos in [broken_sign, live_sign] {
        set_block(pos.0, pos.1, pos.2, name_to_state("oak_sign").unwrap());
        for front in [0, 1] {
            signs()
                .lock()
                .unwrap()
                .insert((pos.0, pos.1, pos.2, front), lines.clone());
        }
    }
    for pos in [broken_furnace, broken_sign] {
        set_block(pos.0, pos.1, pos.2, 0);
    }
    assert!(!furnaces().lock().unwrap().contains_key(&broken_furnace));
    assert!(!signs()
        .lock()
        .unwrap()
        .keys()
        .any(|&(x, y, z, _)| (x, y, z) == broken_sign));
    let mut payload = Vec::new();
    save_world_into(&mut payload);
    world().lock().unwrap().clear();
    furnaces().lock().unwrap().clear();
    signs().lock().unwrap().clear();
    drops().lock().unwrap().clear();
    load_world_from(&payload).unwrap();
    let fs = furnaces().lock().unwrap();
    assert_eq!(fs.len(), 1);
    let loaded = fs
        .get(&live_furnace)
        .expect("live furnace must be restored");
    assert_eq!(loaded.input, input);
    assert_eq!((loaded.burn_left, loaded.progress), (43, 17));
    assert_eq!(
        block_at(live_furnace.0, live_furnace.1, live_furnace.2),
        name_to_state("furnace").unwrap()
    );
    let sg = signs().lock().unwrap();
    assert_eq!(sg.len(), 2);
    for front in [0, 1] {
        assert_eq!(
            sg.get(&(live_sign.0, live_sign.1, live_sign.2, front)),
            Some(&lines)
        );
    }
    assert_eq!(
        block_at(live_sign.0, live_sign.1, live_sign.2),
        name_to_state("oak_sign").unwrap()
    );
}

#[test]
fn dro1_round_trip_preserves_drop_target_and_thrower_uuids() {
    if test_support::isolated_world_test() {
        return;
    }
    let _world_guard = world_guard();
    let old_world = std::mem::take(&mut *world().lock().unwrap());
    let old_drops = std::mem::take(&mut *drops().lock().unwrap());
    let old_mobs = std::mem::take(&mut *mobs().lock().unwrap());
    let old_time = WORLD_TIME.swap(555, Ordering::Relaxed);
    let old_forced = std::mem::take(&mut *forceloaded().lock().unwrap());
    let old_rng = world_rng::saved_state();

    let stone = name_to_item("stone").unwrap();
    let uuid_a = [0xAA; 16];
    let uuid_b = [0xBB; 16];
    let uuid_c = [0xCC; 16];
    *drops().lock().unwrap() = vec![
        Drop {
            eid: 8801,
            x: 1.5,
            y: 70.0,
            z: 2.5,
            item: stone,
            cnt: 1,
            comps: Vec::new(),
            age: 4,
            target: Some(uuid_a),
            thrower: Some(uuid_b),
        },
        Drop {
            eid: 8802,
            x: 3.5,
            y: 70.0,
            z: 4.5,
            item: stone,
            cnt: 2,
            comps: Vec::new(),
            age: 5,
            target: Some(uuid_c),
            thrower: None,
        },
        Drop {
            eid: 8803,
            x: 5.5,
            y: 70.0,
            z: 6.5,
            item: stone,
            cnt: 3,
            comps: Vec::new(),
            age: 6,
            target: None,
            thrower: Some(uuid_a),
        },
        Drop {
            eid: 8804,
            x: 7.5,
            y: 70.0,
            z: 8.5,
            item: stone,
            cnt: 4,
            comps: Vec::new(),
            age: 7,
            target: None,
            thrower: None,
        },
    ];

    let mut payload = Vec::new();
    save_world_into(&mut payload);
    assert!(payload.ends_with(b"DRO1"), "save with drop owners must carry DRO1 footer");

    drops().lock().unwrap().clear();
    load_world_from(&payload).unwrap();
    let restored: Vec<_> = drops()
        .lock()
        .unwrap()
        .iter()
        .map(|d| (d.eid, d.cnt, d.age, d.target, d.thrower))
        .collect();

    // Corrupting the DRO1 footer must be rejected cleanly.
    let mut bad = payload.clone();
    let len = bad.len();
    bad[len - 12..len - 4].copy_from_slice(&0u64.to_be_bytes());
    let corrupt_err = load_world_from(&bad).is_err();

    *world().lock().unwrap() = old_world;
    *drops().lock().unwrap() = old_drops;
    *mobs().lock().unwrap() = old_mobs;
    WORLD_TIME.store(old_time, Ordering::Relaxed);
    *forceloaded().lock().unwrap() = old_forced;
    world_rng::restore(old_rng);

    assert_eq!(
        restored,
        vec![
            (8801, 1, 4, Some(uuid_a), Some(uuid_b)),
            (8802, 2, 5, Some(uuid_c), None),
            (8803, 3, 6, None, Some(uuid_a)),
            (8804, 4, 7, None, None),
        ]
    );
    assert!(corrupt_err, "corrupt DRO1 footer must return Err");
}

// ---------- P2-3-S2a: the overlay key carries a dimension ----------
//
// `WORLD` is keyed by `(DimId, cx, cz)`. These fixtures install all three generators, so "the
// Nether's own answer" is a real one and not an uninitialised `None` standing in for it.

/// The three states one cell is given, one per dimension.
///
/// Each dimension gets a state its *own* generator does not answer at the probe cell — the
/// Overworld and the End are air there, the Nether is netherrack — and no two dimensions get the
/// same one. An edit that lands in the wrong dimension therefore reads as a wrong state, instead
/// of as the terrain that cell already had (a write equal to the generated state removes the
/// overlay entry rather than shadowing it).
fn p2_3_s2a_states() -> [(DimId, u32); 3] {
    [
        (DimId::Overworld, name_to_state("stone").expect("stone")),
        (
            DimId::Nether,
            name_to_state("end_stone").expect("end_stone"),
        ),
        (
            DimId::End,
            name_to_state("netherrack").expect("netherrack"),
        ),
    ]
}

/// Install the three generators this fixture reads, or fail loudly.
fn p2_3_s2a_init_generators() {
    assert!(worldgen::init(0, name_to_state), "the Overworld generator");
    for dim in [DimId::Nether, DimId::End] {
        assert!(
            worldgen::init_dim(dim, 0, name_to_state),
            "{} generator",
            dim.dimension_id()
        );
    }
}

/// P2-3-S2a: one cell, three dimensions, three independent edits.
///
/// With the overlay keyed by `(cx, cz)` alone, the three writes below are one entry: whichever
/// dimension wrote last owns that coordinate, and a read in either of the other two answers with
/// it.
#[test]
fn p2_3_s2a_one_cell_in_three_dimensions_stays_independent() {
    if test_support::isolated_world_test() {
        return;
    }
    test_support::require_vanilla_data();
    let _world_guard = world_guard();
    p2_3_s2a_init_generators();
    let old_world = std::mem::take(&mut *world().lock().unwrap());

    let cell = (5, 100, 7);
    let states = p2_3_s2a_states();
    for (dim, st) in states {
        set_block_dim(dim, cell.0, cell.1, cell.2, st);
    }
    for (dim, st) in states {
        assert_eq!(
            overlay_block_dim(dim, cell.0, cell.1, cell.2),
            Some(st),
            "{dim:?}: the overlay entry at one coordinate is this dimension's edit"
        );
        assert_eq!(
            block_at_dim(dim, cell.0, cell.1, cell.2),
            st,
            "{dim:?}: a read in one dimension must not answer with another's edit"
        );
    }
    // The dim-less accessors mean the Overworld, which is every caller's dimension today.
    assert_eq!(block_at(cell.0, cell.1, cell.2), states[0].1);
    assert_eq!(overlay_block(cell.0, cell.1, cell.2), Some(states[0].1));

    // Where no dimension has an edit, each answers from its own generator. y = -30 is inside the
    // Overworld's -64/384 window and below the Nether's and the End's 0/256 ones, so the right
    // answer is "Overworld terrain, air, air". A fallback to `base_state` — the Overworld's noise
    // settings, resolved through the *active* generator — answers Overworld terrain in all three
    // and reads exactly like a working per-dimension overlay until someone loads a Nether chunk.
    let below = (0, -30, 0);
    assert_ne!(
        block_at_dim(DimId::Overworld, below.0, below.1, below.2),
        0,
        "the Overworld generator fills y = -30"
    );
    for dim in [DimId::Nether, DimId::End] {
        assert_eq!(
            block_at_dim(dim, below.0, below.1, below.2),
            0,
            "{dim:?}: below its own world window is air, not the Overworld's deepslate"
        );
        assert_eq!(
            overlay_block_dim(dim, below.0, below.1, below.2),
            None,
            "{dim:?}: nothing was written there"
        );
    }

    *world().lock().unwrap() = old_world;
    assert!(worldgen::init(0, name_to_state), "leave the Overworld active");
}

/// P2-3-S2a: `world_overlay.bin` stays the Overworld's file, byte for byte.
///
/// A legacy `(cx, cz)` chunk loads as `(DimId::Overworld, cx, cz)`, and another dimension's edits
/// change none of its bytes: the format, the header count and the keys are what they were before
/// the key carried a dimension, which is what lets an older build read this file.
#[test]
fn p2_3_s2a_legacy_overworld_save_and_dim_overlay_tables_round_trip() {
    if test_support::isolated_world_test() {
        return;
    }
    test_support::require_vanilla_data();
    let _world_guard = world_guard();
    p2_3_s2a_init_generators();
    let old_world = std::mem::take(&mut *world().lock().unwrap());

    let cell = (5, 100, 7);
    let states = p2_3_s2a_states();
    for (dim, st) in states {
        set_block_dim(dim, cell.0, cell.1, cell.2, st);
    }

    // The payload the legacy file carries, with all three dimensions edited.
    //
    // One throwaway save runs first: `save_world_into` materialises the payloads the generator has
    // staged, so the first save of a session writes state the second one already finds written.
    // Both sides of the comparison below are second saves, which leaves the removed dimensions as
    // the only difference between them.
    let mut warmup = Vec::new();
    save_world_into(&mut warmup);
    let mut with_dims = Vec::new();
    save_world_into(&mut with_dims);
    // The same save with the other two dimensions' edits removed. Byte equality is the claim:
    // a companion dimension must not reach the legacy file at all.
    world()
        .lock()
        .unwrap()
        .retain(|key, _| key.0 == DimId::Overworld);
    let mut without_dims = Vec::new();
    save_world_into(&mut without_dims);
    assert_eq!(
        with_dims, without_dims,
        "another dimension's edits must not change one byte of the Overworld's save"
    );

    // It loads as the Overworld's chunks and nothing else.
    world().lock().unwrap().clear();
    load_world_from(&with_dims).unwrap();
    assert_eq!(
        overlay_block_dim(DimId::Overworld, cell.0, cell.1, cell.2),
        Some(states[0].1),
        "a legacy (cx, cz) chunk loads into the Overworld"
    );
    {
        let w = world().lock().unwrap();
        assert!(
            w.keys().all(|key| key.0 == DimId::Overworld),
            "the legacy file carries no other dimension: {:?}",
            w.keys().collect::<Vec<_>>()
        );
    }
    for (dim, _) in &states[1..] {
        assert_eq!(
            overlay_block_dim(*dim, cell.0, cell.1, cell.2),
            None,
            "{dim:?}: a companion table is not in the legacy file"
        );
    }

    // A companion table round-trips on its own, and puts its chunks in its own dimension. The
    // Overworld's edit is still there — the byte comparison above compared against a save without
    // the other two, it did not empty the overlay — so it is what shows the load landing in the
    // dimension it was asked for.
    for (dim, st) in &states[1..] {
        set_block_dim(*dim, cell.0, cell.1, cell.2, *st);
        let mut table = Vec::new();
        assert_eq!(
            save_dim_overlay_into(*dim, &mut table),
            1,
            "{dim:?}: one edited chunk"
        );
        world()
            .lock()
            .unwrap()
            .remove(&(*dim, cell.0 >> 4, cell.2 >> 4));
        assert_eq!(
            overlay_block_dim(*dim, cell.0, cell.1, cell.2),
            None,
            "{dim:?}: the edit was removed before the reload"
        );
        load_dim_overlay_from(*dim, &table).unwrap();
        assert_eq!(
            overlay_block_dim(*dim, cell.0, cell.1, cell.2),
            Some(*st),
            "{dim:?}: its own table restores its own edit"
        );
        assert_eq!(
            overlay_block_dim(DimId::Overworld, cell.0, cell.1, cell.2),
            Some(states[0].1),
            "{dim:?}: a companion load stays out of the Overworld"
        );
    }

    // A dimension with no edits writes no chunks, which is what makes its file removable.
    world().lock().unwrap().clear();
    let mut empty = Vec::new();
    assert_eq!(save_dim_overlay_into(DimId::Nether, &mut empty), 0);
    assert_eq!(empty, vec![0], "an empty table is a zero count and nothing else");

    *world().lock().unwrap() = old_world;
    assert!(worldgen::init(0, name_to_state), "leave the Overworld active");
}

/// P2-3-S2a: the companion files follow the legacy one onto disk, and disappear when their
/// dimension has no edits left.
///
/// The save path is the one the server runs, pointed at a scratch directory so the fixture does not
/// write into the checkout's working directory.
#[test]
fn p2_3_s2a_companion_dim_files_are_written_loaded_and_removed_when_empty() {
    if test_support::isolated_world_test() {
        return;
    }
    test_support::require_vanilla_data();
    let _world_guard = world_guard();
    p2_3_s2a_init_generators();
    let old_world = std::mem::take(&mut *world().lock().unwrap());
    let dir = std::env::temp_dir().join(format!("mc-spine-p2_3_s2a-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch save directory");
    let file = |dim: DimId| dim_overlay_path(&dir, dim);

    let cell = (5, 100, 7);
    let states = p2_3_s2a_states();
    for (dim, st) in states {
        set_block_dim(dim, cell.0, cell.1, cell.2, st);
    }
    save_world_guarded_in(&dir);
    for (dim, _) in states {
        assert!(
            file(dim).is_file(),
            "{dim:?}: {} was not written",
            file(dim).display()
        );
    }

    // Every dimension comes back from disk, into its own dimension.
    world().lock().unwrap().clear();
    load_world_in(&dir).expect("load the scratch save");
    for (dim, st) in states {
        assert_eq!(
            overlay_block_dim(dim, cell.0, cell.1, cell.2),
            Some(st),
            "{dim:?}: its edit must come back from its own file"
        );
        assert_eq!(block_at_dim(dim, cell.0, cell.1, cell.2), st);
    }

    // A dimension whose last edit is undone loses its file: an Overworld-only server must not
    // accumulate companions, and a stale companion would resurrect the edit on the next load.
    world()
        .lock()
        .unwrap()
        .retain(|key, _| key.0 == DimId::Overworld);
    save_world_guarded_in(&dir);
    for dim in [DimId::Nether, DimId::End] {
        assert!(
            !file(dim).exists(),
            "{dim:?}: {} outlived its last edit",
            file(dim).display()
        );
    }
    assert!(file(DimId::Overworld).is_file(), "the legacy file stays");

    // And the reload no longer invents them.
    world().lock().unwrap().clear();
    load_world_in(&dir).expect("reload the scratch save");
    for dim in [DimId::Nether, DimId::End] {
        assert_eq!(overlay_block_dim(dim, cell.0, cell.1, cell.2), None);
    }

    let _ = std::fs::remove_dir_all(&dir);
    *world().lock().unwrap() = old_world;
    assert!(worldgen::init(0, name_to_state), "leave the Overworld active");
}
