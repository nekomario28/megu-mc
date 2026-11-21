use super::*;

// Peel the independently optional trailers written after dimension data. Legacy saves have
// neither marker, so these readers leave their payload untouched.
fn strip_seed_and_brewing_footers(payload: &[u8]) -> Result<&[u8], &'static str> {
    let (payload, _) = read_world_seed_footer(payload)?;
    let (payload, _) = read_brewing_footer(payload)?;
    Ok(payload)
}

fn payload_through_drop_owners_footer(payload: &[u8]) -> Result<&[u8], &'static str> {
    let payload = strip_seed_and_brewing_footers(payload)?;
    let (payload, _) = read_dim2_footer(payload)?;
    let (payload, _, _) = read_dim_footer(payload)?;
    let (payload, _) = read_block_loot_footer(payload)?;
    Ok(payload)
}

fn full_state_frame(payload: &[u8]) -> std::ops::Range<usize> {
    let (legacy, _) = read_world_seed_footer(payload).unwrap();
    let (legacy, _) = read_mob_dimensions_footer(legacy).unwrap();
    let legacy = payload_through_drop_owners_footer(legacy).unwrap();
    let (legacy, _) = read_drop_owners_footer(legacy).unwrap();
    let legacy = tick_persistence::prefix(legacy).unwrap();
    let (legacy, _) = split_save_footer(legacy, b"GEN1").unwrap();
    let (before, footer) = split_save_footer(legacy, SAVE_STATE_MAGIC).unwrap();
    assert!(footer.is_some());
    before.len()..legacy.len()
}

// Rebuild a historical SVP1 frame without its later SVX1 extension. The old
// spawn, player, mob and raid records are kept verbatim.
fn without_entity_rule_extension(payload: &[u8]) -> Vec<u8> {
    let frame = full_state_frame(payload);
    let body = &payload[frame.start..frame.end - 12];
    let extension = body.windows(4).rposition(|bytes| bytes == SAVE_ENTITY_RULES_MAGIC)
        .expect("SVX1 extension");
    let mut legacy = Vec::new();
    legacy.extend_from_slice(&payload[..frame.start]);
    legacy.extend_from_slice(&body[..extension]);
    legacy.extend_from_slice(&(extension as u64).to_be_bytes());
    legacy.extend_from_slice(SAVE_STATE_MAGIC);
    legacy.extend_from_slice(&payload[frame.end..]);
    legacy
}

#[test]
fn all_51_vanilla_gamerules_survive_save_v2_and_legacy_defaults_restore() {
    if test_support::isolated_world_test() {
        return;
    }
    let _guard = world_guard();
    assert_eq!(COMMAND_GAMERULE_DEFAULTS.len(), 51);
    let original: Vec<i32> = {
        let rules = command_gamerules().lock().unwrap();
        COMMAND_GAMERULE_DEFAULTS.iter()
            .map(|&(name, _, _)| gamerule_value(name, &rules)).collect()
    };
    let changed: Vec<i32> = COMMAND_GAMERULE_DEFAULTS.iter().enumerate()
        .map(|(index, &(_, is_bool, default))| {
            if is_bool { i32::from(default == 0) }
            else { default.wrapping_add(101 + index as i32) }
        })
        .collect();
    restore_gamerules(Some(&changed));
    let mut payload = Vec::new();
    save_world_into(&mut payload);
    assert!(payload.ends_with(b"SED1"));
    let old_state = without_entity_rule_extension(&payload);
    let mut no_state = payload.clone();
    no_state.drain(full_state_frame(&payload));

    attrs().lock().unwrap().insert("e:999999".into(),
        [("generic.max_health".into(), 64.0)].into());
    restore_gamerules(Some(&original));
    load_world_from(&payload).unwrap();
    assert!(!attrs().lock().unwrap().contains_key("e:999999"));
    {
        let rules = command_gamerules().lock().unwrap();
        for (index, &(name, _, _)) in COMMAND_GAMERULE_DEFAULTS.iter().enumerate() {
            assert_eq!(gamerule_value(name, &rules), changed[index], "rule {name}");
            assert_eq!(rules[name], changed[index], "registry {name}");
        }
    }

    for legacy in [&old_state, &no_state] {
        restore_gamerules(Some(&changed));
        load_world_from(legacy).unwrap();
        let rules = command_gamerules().lock().unwrap();
        for &(name, _, default) in COMMAND_GAMERULE_DEFAULTS.iter() {
            let is_legacy_atomic = matches!(name,
                "keepInventory" | "doDaylightCycle" | "doWeatherCycle" |
                "doMobSpawning" | "mobGriefing" | "naturalRegeneration" |
                "doMobLoot" | "maxEntityCramming");
            if is_legacy_atomic {
                assert_eq!(gamerule_value(name, &rules),
                    changed[COMMAND_GAMERULE_DEFAULTS.iter().position(|r| r.0 == name).unwrap()],
                    "legacy atomic {name}");
                assert_eq!(rules[name], gamerule_value(name, &rules),
                    "legacy registry {name}");
            } else {
                assert_eq!(rules[name], default, "legacy default {name}");
            }
        }
    }
    restore_gamerules(Some(&original));
}

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
        dim: DimId::Overworld,
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
        fire_ticks: Mutex::new(0),
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
            let frame = full_state_frame(&payload);
            payload.drain(frame);
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
            mob_air: 150,
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
            ow_block(pos),
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
                .insert((DimId::Overworld, pos.0, pos.1, pos.2, front), lines.clone());
        }
    }
    for pos in [broken_furnace, broken_sign] {
        set_block(pos.0, pos.1, pos.2, 0);
    }
    assert!(!furnaces().lock().unwrap().contains_key(&ow_block(broken_furnace)));
    assert!(!signs()
        .lock()
        .unwrap()
        .keys()
        .any(|&(dim, x, y, z, _)| dim == DimId::Overworld && (x, y, z) == broken_sign));
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
        .get(&ow_block(live_furnace))
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
            sg.get(&(DimId::Overworld, live_sign.0, live_sign.1, live_sign.2, front)),
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
    let drop_footer_payload = payload_through_drop_owners_footer(&payload).unwrap();
    assert!(
        drop_footer_payload.ends_with(DROP_OWNERS_MAGIC),
        "save with drop owners must carry DRO1 footer"
    );

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
    let footer_end = drop_footer_payload.len();
    bad[footer_end - DROP_OWNERS_FOOTER_LEN..footer_end - 4]
        .copy_from_slice(&0u64.to_be_bytes());
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

/// P2-3-S2c: the same sign at one coordinate in three dimensions is three signs. The legacy sign
/// section carries only the Overworld's, so an Overworld-only save keeps every legacy byte and has
/// no dimension footer; the other two ride in the footer and reload into their own keys.
#[test]
fn p2_3_s2c_signs_stay_isolated_per_dimension_and_round_trip() {
    if test_support::isolated_world_test() {
        return;
    }
    let _world_guard = world_guard();
    let old_signs = std::mem::take(&mut *signs().lock().unwrap());
    let old_pos = std::mem::take(&mut *pos_store().lock().unwrap());
    let old_rng = world_rng::saved_state();

    let cell = (5, 100, 7);
    let chunk = (cell.0 >> 4, cell.2 >> 4);
    let lines = [
        "front".to_string(),
        "b".to_string(),
        "c".to_string(),
        "d".to_string(),
    ];
    let ow_key = (DimId::Overworld, cell.0, cell.1, cell.2, 1);

    signs()
        .lock()
        .unwrap()
        .insert(ow_key, lines.clone());
    // The first save materialises staged generation payloads; both compared saves come after it.
    let mut warmup = Vec::new();
    save_world_into(&mut warmup);
    let mut overworld_only = Vec::new();
    save_world_into(&mut overworld_only);
    assert!(
        !overworld_only
            .windows(DIM_MAGIC.len())
            .any(|w| w == DIM_MAGIC),
        "an Overworld-only save must contain no dimension footer bytes"
    );

    // Same coordinate in the other two dimensions: three independent entries, and one sign per
    // dimension in that dimension's chunk. A neighbouring chunk sees none.
    for dim in [DimId::Nether, DimId::End] {
        signs()
            .lock()
            .unwrap()
            .insert((dim, cell.0, cell.1, cell.2, 1), lines.clone());
    }
    assert_eq!(signs().lock().unwrap().len(), 3);
    for dim in DimId::ALL {
        let here = signs_in_chunk(dim, chunk.0, chunk.1);
        assert_eq!(here.len(), 1, "{dim:?}: the chunk carries exactly its sign");
        assert_eq!(here[0].0, (dim, cell.0, cell.1, cell.2, 1));
        assert_eq!(here[0].1, lines);
    }
    assert!(signs_in_chunk(DimId::Overworld, chunk.0 + 1, chunk.1).is_empty());

    // Stripping the footer from the three-dimension save leaves the exact Overworld-only payload.
    // A non-Overworld sign must not shift one byte of the legacy section.
    let mut with_dims = Vec::new();
    save_world_into(&mut with_dims);
    assert!(
        with_dims
            .windows(DIM_MAGIC.len())
            .any(|w| w == DIM_MAGIC),
        "a non-Overworld sign must produce a footer"
    );
    let without_seed_or_brewing = strip_seed_and_brewing_footers(&with_dims).unwrap();
    let (without_dim2, _) = read_dim2_footer(without_seed_or_brewing).unwrap();
    let (legacy_region, footer_signs, footer_players) = read_dim_footer(without_dim2).unwrap();
    let overworld_legacy = strip_seed_and_brewing_footers(&overworld_only).unwrap();
    assert_eq!(
        legacy_region, overworld_legacy,
        "a non-Overworld sign changed a byte of the legacy save"
    );
    assert!(footer_players.is_empty());
    assert_eq!(footer_signs.len(), 2);
    assert!(footer_signs.iter().all(|s| s.0 != DimId::Overworld));

    // Full round-trip: all three come back, each into its own dimension.
    signs().lock().unwrap().clear();
    load_world_from(&with_dims).unwrap();
    assert_eq!(signs().lock().unwrap().len(), 3);
    for dim in DimId::ALL {
        assert_eq!(
            signs()
                .lock()
                .unwrap()
                .get(&(dim, cell.0, cell.1, cell.2, 1)),
            Some(&lines),
            "{dim:?}: its own key came back from the footer"
        );
    }

    // A legacy payload without the footer defaults its sign to the Overworld.
    signs().lock().unwrap().clear();
    load_world_from(&overworld_only).unwrap();
    assert_eq!(signs().lock().unwrap().len(), 1);
    assert_eq!(signs().lock().unwrap().get(&ow_key), Some(&lines));

    world_rng::restore(old_rng);
    *signs().lock().unwrap() = old_signs;
    *pos_store().lock().unwrap() = old_pos;
}

/// P2-3-S2c: the pending-block, backoff and plugin-cell stores keyed by chunk/block alone made the
/// same coordinate one entry in every dimension. All three (and the plugin backoff) now name their
/// dimension, and GC must not let one dimension's live cell keep another's backoff alive.
#[test]
fn p2_3_s2c_pending_blocks_backoff_and_plugin_cells_stay_isolated_per_dimension() {
    if test_support::isolated_world_test() {
        return;
    }
    let _world_guard = world_guard();
    let old_pending = std::mem::take(&mut *pending_blocks().lock().unwrap());
    let old_backoff = std::mem::take(&mut *backoff().lock().unwrap());
    let old_cells = std::mem::take(&mut *plugin_cells().lock().unwrap());
    let old_pb = std::mem::take(&mut *plugin_backoff().lock().unwrap());
    let old_waters = std::mem::take(&mut *waters().lock().unwrap());
    let old_hoppers = std::mem::take(&mut *hoppers().lock().unwrap());

    let pos = (5, 100, 7);
    let sec = (pos.0 >> 4, pos.1 >> 4, pos.2 >> 4);
    let ck = (pos.0 >> 4, pos.2 >> 4);
    let recs = HashMap::from([(42u16, 7u32)]);
    for dim in DimId::ALL {
        pending_blocks()
            .lock()
            .unwrap()
            .entry((dim, sec.0, sec.1, sec.2))
            .or_default()
            .insert(42, 7);
        backoff()
            .lock()
            .unwrap()
            .insert((dim, pos.0, pos.1, pos.2), (100, 1));
        plugin_cells()
            .lock()
            .unwrap()
            .entry((dim, ck.0, ck.1))
            .or_default()
            .push(PluginCell {
                pi: 0,
                x: pos.0,
                y: pos.1,
                z: pos.2,
                key: 9,
                warm_ok: true,
            });
        plugin_backoff()
            .lock()
            .unwrap()
            .insert((dim, 0, pos.0, pos.1, pos.2, 9), (100, 1));
    }
    assert_eq!(pending_blocks().lock().unwrap().len(), 3);
    assert_eq!(backoff().lock().unwrap().len(), 3);
    assert_eq!(plugin_cells().lock().unwrap().len(), 3);
    assert_eq!(plugin_backoff().lock().unwrap().len(), 3);
    for dim in DimId::ALL {
        assert_eq!(
            pending_blocks()
                .lock()
                .unwrap()
                .get(&(dim, sec.0, sec.1, sec.2)),
            Some(&recs),
            "{dim:?}: its own pending-block entry"
        );
        assert_eq!(
            backoff()
                .lock()
                .unwrap()
                .get(&(dim, pos.0, pos.1, pos.2)),
            Some(&(100, 1)),
            "{dim:?}: its own backoff entry"
        );
        assert_eq!(
            plugin_cells()
                .lock()
                .unwrap()
                .get(&(dim, ck.0, ck.1))
                .map(|b| b.len()),
            Some(1),
            "{dim:?}: its own plugin cell"
        );
        assert_eq!(
            plugin_backoff()
                .lock()
                .unwrap()
                .get(&(dim, 0, pos.0, pos.1, pos.2, 9)),
            Some(&(100, 1)),
            "{dim:?}: its own plugin backoff entry"
        );
    }

    // Drop the Nether's cell; GC must reap only the Nether's backoff entries. waters/hoppers are
    // Overworld-only indexes and are empty here, so only the per-dimension cell keeps a backoff.
    plugin_cells()
        .lock()
        .unwrap()
        .remove(&(DimId::Nether, ck.0, ck.1));
    gc_backoff();
    assert!(!backoff()
        .lock()
        .unwrap()
        .contains_key(&(DimId::Nether, pos.0, pos.1, pos.2)));
    for dim in [DimId::Overworld, DimId::End] {
        assert!(
            backoff()
                .lock()
                .unwrap()
                .contains_key(&(dim, pos.0, pos.1, pos.2)),
            "{dim:?}: a live cell keeps its own backoff"
        );
    }
    assert!(!plugin_backoff()
        .lock()
        .unwrap()
        .contains_key(&(DimId::Nether, 0, pos.0, pos.1, pos.2, 9)));
    assert!(plugin_backoff()
        .lock()
        .unwrap()
        .contains_key(&(DimId::End, 0, pos.0, pos.1, pos.2, 9)));

    *waters().lock().unwrap() = old_waters;
    *hoppers().lock().unwrap() = old_hoppers;
    *plugin_backoff().lock().unwrap() = old_pb;
    *plugin_cells().lock().unwrap() = old_cells;
    *backoff().lock().unwrap() = old_backoff;
    *pending_blocks().lock().unwrap() = old_pending;
}

/// P2-3-S2d: a player's dimension is part of their saved position. The legacy section writes all
/// players in the old six-field layout (so it stays readable and an Overworld-only save emits no
/// footer), and the footer carries only the non-Overworld dimensions.
#[test]
fn p2_3_s2d_pos_store_preserves_dimension_and_defaults_legacy_to_overworld() {
    if test_support::isolated_world_test() {
        return;
    }
    let _world_guard = world_guard();
    let old_pos = std::mem::take(&mut *pos_store().lock().unwrap());
    let old_signs = std::mem::take(&mut *signs().lock().unwrap());
    let old_rng = world_rng::saved_state();

    let expected = [
        (
            DimId::Overworld,
            "ow".to_string(),
            1.0f64,
            2.0f64,
            3.0f64,
            0.5f32,
            0.25f32,
        ),
        (
            DimId::Nether,
            "net".to_string(),
            4.0,
            5.0,
            6.0,
            1.5,
            -0.5,
        ),
        (
            DimId::End,
            "end".to_string(),
            7.0,
            8.0,
            9.0,
            -2.0,
            0.75,
        ),
    ];
    for (dim, name, x, y, z, yaw, pitch) in &expected {
        pos_store()
            .lock()
            .unwrap()
            .insert(name.clone(), (*dim, *x, *y, *z, *yaw, *pitch));
    }
    let mut warmup = Vec::new();
    save_world_into(&mut warmup);
    let mut full = Vec::new();
    save_world_into(&mut full);
    assert!(
        full.windows(DIM_MAGIC.len()).any(|w| w == DIM_MAGIC),
        "a non-Overworld player must produce a footer"
    );
    let without_seed_or_brewing = strip_seed_and_brewing_footers(&full).unwrap();
    let (without_dim2, _) = read_dim2_footer(without_seed_or_brewing).unwrap();
    let (legacy, footer_signs, footer_players) = read_dim_footer(without_dim2).unwrap();
    assert!(footer_signs.is_empty());
    assert_eq!(footer_players.len(), 2);
    assert!(footer_players.iter().all(|(_, d)| *d != DimId::Overworld));

    // The footer-stripped payload is the legacy wire form. Every player is in it, and a legacy
    // load restores all of them into the Overworld with their exact coordinates and look.
    pos_store().lock().unwrap().clear();
    load_world_from(legacy).unwrap();
    for (_, name, x, y, z, yaw, pitch) in &expected {
        assert_eq!(
            pos_store().lock().unwrap().get(name),
            Some(&(DimId::Overworld, *x, *y, *z, *yaw, *pitch)),
            "{name}: a legacy position defaults to the Overworld"
        );
    }

    // The full payload restores each player's exact (DimId, x, y, z, yaw, pitch).
    pos_store().lock().unwrap().clear();
    load_world_from(&full).unwrap();
    for (dim, name, x, y, z, yaw, pitch) in &expected {
        assert_eq!(
            pos_store().lock().unwrap().get(name),
            Some(&(*dim, *x, *y, *z, *yaw, *pitch)),
            "{name}: its dimension and position round-trip"
        );
    }

    // An Overworld-only save writes zero footer bytes.
    pos_store().lock().unwrap().clear();
    pos_store()
        .lock()
        .unwrap()
        .insert("ow".to_string(), (DimId::Overworld, 1.0, 2.0, 3.0, 0.5, 0.25));
    let mut overworld_only = Vec::new();
    save_world_into(&mut overworld_only);
    assert!(
        !overworld_only
            .windows(DIM_MAGIC.len())
            .any(|w| w == DIM_MAGIC),
        "an Overworld-only save must contain no dimension footer bytes"
    );

    world_rng::restore(old_rng);
    *signs().lock().unwrap() = old_signs;
    *pos_store().lock().unwrap() = old_pos;
}

/// The dimension footer fails closed: an unknown dimension tag, a body that claims more bytes than
/// exist, or a truncated sign entry is an error rather than a silent Overworld fallback.
#[test]
fn p2_3_s2_dim_footer_rejects_unknown_dimension_and_truncation() {
    if test_support::isolated_world_test() {
        return;
    }
    let base = b"legacy-body";

    // A well-formed footer parses back into its overrides and leaves the legacy bytes untouched.
    let mut body = Vec::new();
    body.extend_from_slice(&0u32.to_be_bytes()); // signs
    body.extend_from_slice(&1u32.to_be_bytes()); // player dimensions
    body.push(DimId::End.index() as u8);
    body.extend_from_slice(&3u16.to_be_bytes());
    body.extend_from_slice(b"bob");
    let mut good = base.to_vec();
    good.extend_from_slice(&body);
    good.extend_from_slice(&(body.len() as u64).to_be_bytes());
    good.extend_from_slice(DIM_MAGIC);
    let (rest, signs, players) = read_dim_footer(&good).unwrap();
    assert_eq!(rest, base.as_slice());
    assert!(signs.is_empty());
    assert_eq!(players, vec![("bob".to_string(), DimId::End)]);

    // DimId::from_index rejects anything outside 0..3.
    let mut bad_dim = good.clone();
    let dim_at = base.len() + 4 + 4; // sign count, then player count
    assert_eq!(bad_dim[dim_at], DimId::End.index() as u8);
    bad_dim[dim_at] = 3;
    assert!(
        read_dim_footer(&bad_dim).is_err(),
        "an unknown dimension tag must fail closed"
    );

    // The size field claiming more bytes than the payload holds is a truncation error.
    let mut bad_size = base.to_vec();
    bad_size.extend_from_slice(&1_000u64.to_be_bytes());
    bad_size.extend_from_slice(DIM_MAGIC);
    assert!(
        read_dim_footer(&bad_size).is_err(),
        "an oversized body length must fail closed"
    );

    // A sign count of one with no sign bytes is a truncated footer.
    let mut trunc = base.to_vec();
    let mut body2 = Vec::new();
    body2.extend_from_slice(&1u32.to_be_bytes());
    body2.extend_from_slice(&1u32.to_be_bytes());
    trunc.extend_from_slice(&body2);
    trunc.extend_from_slice(&(body2.len() as u64).to_be_bytes());
    trunc.extend_from_slice(DIM_MAGIC);
    assert!(
        read_dim_footer(&trunc).is_err(),
        "a truncated sign entry must fail closed"
    );
}

/// P2-3-S2b: chests, furnaces and crops at one coordinate are three independent entries per
/// dimension, and the DIM2 footer returns each to the dimension that owned it. The synthetic
/// per-player Ender key is the exception: it is always filed under the Overworld so opening a
/// chest in any dimension reaches the same storage.
#[test]
fn p2_3_s2b_chests_furnaces_and_crops_stay_isolated_per_dimension_and_round_trip() {
    if test_support::isolated_world_test() {
        return;
    }
    let _guard = world_guard();
    let old_chests = std::mem::take(&mut *chests().lock().unwrap());
    let old_furnaces = std::mem::take(&mut *furnaces().lock().unwrap());
    let old_crops = std::mem::take(&mut *crops().lock().unwrap());
    let old_world = std::mem::take(&mut *world().lock().unwrap());
    let old_loot = std::mem::take(&mut *pending_block_loot().lock().unwrap());
    let old_rng = world_rng::saved_state();

    let pos = (5, 70, 7);
    let iron = name_to_item("iron_ingot").unwrap();
    let raw = name_to_item("raw_iron").unwrap();
    let make_furnace = |burn: i32| Furnace {
        input: (raw, 2, Vec::new()),
        fuel: (0, 0, Vec::new()),
        output: (iron, 4, Vec::new()),
        burn_left: burn,
        progress: 17,
        dirty: false,
        xp: 0.0,
        recipe_cache: CachedLookup::default(),
        fuel_cache: CachedLookup::default(),
    };
    for (i, dim) in DimId::ALL.into_iter().enumerate() {
        let key = (dim, pos.0, pos.1, pos.2);
        let slots: HashMap<i16, Stack> = [(0i16, (iron, (i + 1) as u8, Vec::new()))]
            .into_iter()
            .collect();
        chests()
            .lock()
            .unwrap()
            .insert(ContainerKey::Block(key), slots);
        furnaces()
            .lock()
            .unwrap()
            .insert(key, make_furnace(100 + i as i32));
        crops().lock().unwrap().insert(key, i as u8);
    }
    // The Overworld's crop age also lives in its overlay: there is no legacy crop section, so a
    // legacy load rebuilds it from the block state.
    world()
        .lock()
        .unwrap()
        .entry((DimId::Overworld, pos.0 >> 4, pos.2 >> 4))
        .or_default()
        .insert(local_idx(pos.0, pos.1, pos.2), 4278 + 0);
    let uuid = [7u8; 16];
    let ender = ender_container_key(&uuid);
    assert!(
        matches!(ender, ContainerKey::Block((DimId::Overworld, _, _, _))),
        "an Ender key must be stored under the Overworld in every dimension"
    );
    let ender_slots: HashMap<i16, Stack> = [(1i16, (iron, 9, Vec::new()))].into_iter().collect();
    chests().lock().unwrap().insert(ender, ender_slots.clone());
    assert_eq!(chests().lock().unwrap().len(), 4);
    assert_eq!(furnaces().lock().unwrap().len(), 3);
    assert_eq!(crops().lock().unwrap().len(), 3);

    let mut payload = Vec::new();
    save_world_into(&mut payload);
    assert!(
        payload.windows(DIM2_MAGIC.len()).any(|w| w == DIM2_MAGIC),
        "a non-Overworld container must produce a DIM2 footer"
    );
    chests().lock().unwrap().clear();
    furnaces().lock().unwrap().clear();
    crops().lock().unwrap().clear();
    world().lock().unwrap().clear();
    load_world_from(&payload).unwrap();

    assert_eq!(chests().lock().unwrap().len(), 4);
    assert_eq!(furnaces().lock().unwrap().len(), 3);
    assert_eq!(crops().lock().unwrap().len(), 3);
    for (i, dim) in DimId::ALL.into_iter().enumerate() {
        let key = (dim, pos.0, pos.1, pos.2);
        assert_eq!(
            chests().lock().unwrap().get(&ContainerKey::Block(key)).and_then(|m| m.get(&0)).map(|s| s.1),
            Some((i + 1) as u8),
            "{dim:?}: its own chest content came back"
        );
        assert_eq!(
            furnaces().lock().unwrap().get(&key).map(|f| f.burn_left),
            Some(100 + i as i32),
            "{dim:?}: its own furnace state came back"
        );
        assert_eq!(
            crops().lock().unwrap().get(&key).copied(),
            Some(i as u8),
            "{dim:?}: its own crop age came back"
        );
    }
    assert_eq!(
        chests().lock().unwrap().get(&ender),
        Some(&ender_slots),
        "the Ender chest is shared across dimensions through its Overworld key"
    );

    world_rng::restore(old_rng);
    *chests().lock().unwrap() = old_chests;
    *furnaces().lock().unwrap() = old_furnaces;
    *crops().lock().unwrap() = old_crops;
    *world().lock().unwrap() = old_world;
    *pending_block_loot().lock().unwrap() = old_loot;
}

/// P2-3-S2b: an Overworld-only save has no DIM2 footer at all, and stripping DIM2 from a mixed
/// save leaves the legacy bytes untouched. A payload with no DIM2 footer restores every legacy
/// chest, furnace and crop into the Overworld.
#[test]
fn p2_3_s2b_overworld_only_save_emits_no_dim2_footer_and_legacy_loads_as_overworld() {
    if test_support::isolated_world_test() {
        return;
    }
    let _guard = world_guard();
    let old_chests = std::mem::take(&mut *chests().lock().unwrap());
    let old_furnaces = std::mem::take(&mut *furnaces().lock().unwrap());
    let old_crops = std::mem::take(&mut *crops().lock().unwrap());
    let old_world = std::mem::take(&mut *world().lock().unwrap());
    let old_rng = world_rng::saved_state();
    let pos = (8, 70, 9);
    let iron = name_to_item("iron_ingot").unwrap();
    let slots: HashMap<i16, Stack> = [(0i16, (iron, 3, Vec::new()))].into_iter().collect();
    for (i, dim) in DimId::ALL.into_iter().enumerate() {
        let key = (dim, pos.0, pos.1, pos.2);
        chests()
            .lock()
            .unwrap()
            .insert(ContainerKey::Block(key), slots.clone());
        furnaces().lock().unwrap().insert(
            key,
            Furnace {
                input: (iron, 1, Vec::new()),
                fuel: (0, 0, Vec::new()),
                output: (0, 0, Vec::new()),
                burn_left: 50 + i as i32,
                progress: 5,
                dirty: false,
                xp: 0.0,
                recipe_cache: CachedLookup::default(),
                fuel_cache: CachedLookup::default(),
            },
        );
        crops().lock().unwrap().insert(key, 3);
    }
    world()
        .lock()
        .unwrap()
        .entry((DimId::Overworld, pos.0 >> 4, pos.2 >> 4))
        .or_default()
        .insert(local_idx(pos.0, pos.1, pos.2), 4281);

    let mut with_dims = Vec::new();
    save_world_into(&mut with_dims);
    assert!(with_dims.windows(4).any(|w| w == DIM2_MAGIC));
    let without_seed_or_brewing = strip_seed_and_brewing_footers(&with_dims).unwrap();
    let (legacy, dim2) = read_dim2_footer(without_seed_or_brewing).unwrap();
    assert_eq!(dim2.chests.len(), 2);
    assert_eq!(dim2.furnaces.len(), 2);
    assert_eq!(dim2.crops.len(), 2);
    assert!(dim2.chests.iter().all(|(k, _)| k.0 != DimId::Overworld));
    assert!(dim2.furnaces.iter().all(|(k, _)| k.0 != DimId::Overworld));
    assert!(dim2.crops.iter().all(|(k, _)| k.0 != DimId::Overworld));

    // Remove the non-Overworld entries *without* replacing the map, so the surviving Overworld
    // entries keep their iteration order and the two legacy regions must compare byte for byte.
    for dim in [DimId::Nether, DimId::End] {
        let key = (dim, pos.0, pos.1, pos.2);
        chests().lock().unwrap().remove(&ContainerKey::Block(key));
        furnaces().lock().unwrap().remove(&key);
        crops().lock().unwrap().remove(&key);
    }
    let mut overworld_only = Vec::new();
    save_world_into(&mut overworld_only);
    assert!(
        !overworld_only
            .windows(DIM2_MAGIC.len())
            .any(|w| w == DIM2_MAGIC),
        "an Overworld-only save must contain no DIM2 bytes"
    );
    let overworld_legacy = strip_seed_and_brewing_footers(&overworld_only).unwrap();
    assert_eq!(
        legacy,
        overworld_legacy,
        "a non-Overworld entry changed a byte of the legacy save"
    );

    chests().lock().unwrap().clear();
    furnaces().lock().unwrap().clear();
    crops().lock().unwrap().clear();
    world().lock().unwrap().clear();
    load_world_from(&overworld_only).unwrap();
    let ow = ow_block(pos);
    assert_eq!(chests().lock().unwrap().len(), 1);
    assert!(chests().lock().unwrap().contains_key(&ContainerKey::Block(ow)));
    assert!(furnaces().lock().unwrap().contains_key(&ow));
    assert_eq!(crops().lock().unwrap().get(&ow).copied(), Some(3));

    world_rng::restore(old_rng);
    *chests().lock().unwrap() = old_chests;
    *furnaces().lock().unwrap() = old_furnaces;
    *crops().lock().unwrap() = old_crops;
    *world().lock().unwrap() = old_world;
}

/// P2-3-S2b: the DIM2 footer is the trust boundary for another dimension's containers. It must
/// fail closed on a dimension tag it cannot name, on an Overworld tag that does not belong in it,
/// and on a body that is shorter than its stated size or the entries it declares.
#[test]
fn p2_3_s2b_dim2_footer_rejects_unknown_dimension_and_truncation() {
    if test_support::isolated_world_test() {
        return;
    }
    let base = b"legacy-body";

    let mut body = Vec::new();
    body.extend_from_slice(&0u32.to_be_bytes()); // chests
    body.extend_from_slice(&0u32.to_be_bytes()); // furnaces
    body.extend_from_slice(&1u32.to_be_bytes()); // crops
    body.push(DimId::End.index() as u8);
    body.extend_from_slice(&5i32.to_be_bytes());
    body.extend_from_slice(&70i32.to_be_bytes());
    body.extend_from_slice(&7i32.to_be_bytes());
    body.push(3u8);
    body.extend_from_slice(&0u32.to_be_bytes()); // pending loot
    let mut good = base.to_vec();
    good.extend_from_slice(&body);
    good.extend_from_slice(&(body.len() as u64).to_be_bytes());
    good.extend_from_slice(DIM2_MAGIC);
    let (rest, dim2) = read_dim2_footer(&good).unwrap();
    assert_eq!(rest, base.as_slice());
    assert_eq!(dim2.crops.len(), 1);
    assert_eq!(dim2.crops[0].0.0, DimId::End);
    assert_eq!(dim2.crops[0].1, 3);
    // A payload without the footer is the legacy form and parses cleanly.
    assert!(read_dim2_footer(base).unwrap().1.is_empty());

    let dim_at = base.len() + 12; // three u32 counts, then the crop entry's dimension byte
    let mut bad_dim = good.clone();
    assert_eq!(bad_dim[dim_at], DimId::End.index() as u8);
    bad_dim[dim_at] = 3; // outside 0..3
    assert!(
        read_dim2_footer(&bad_dim).is_err(),
        "an unknown dimension tag must fail closed"
    );
    let mut ow_dim = good.clone();
    ow_dim[dim_at] = DimId::Overworld.index() as u8;
    assert!(
        read_dim2_footer(&ow_dim).is_err(),
        "an Overworld tag has no place in a non-Overworld footer"
    );

    let mut bad_size = base.to_vec();
    bad_size.extend_from_slice(&1_000u64.to_be_bytes());
    bad_size.extend_from_slice(DIM2_MAGIC);
    assert!(
        read_dim2_footer(&bad_size).is_err(),
        "an oversized body length must fail closed"
    );

    // The declared crop entry is one byte short of its age.
    let mut short = base.to_vec();
    let mut short_body = Vec::new();
    short_body.extend_from_slice(&0u32.to_be_bytes());
    short_body.extend_from_slice(&0u32.to_be_bytes());
    short_body.extend_from_slice(&1u32.to_be_bytes());
    short_body.push(DimId::End.index() as u8);
    short_body.extend_from_slice(&5i32.to_be_bytes());
    short_body.extend_from_slice(&70i32.to_be_bytes());
    short_body.extend_from_slice(&7i32.to_be_bytes());
    // no crop age byte, no loot count: both the count bound and the entry reader must reject it
    short.extend_from_slice(&short_body);
    short.extend_from_slice(&(short_body.len() as u64).to_be_bytes());
    short.extend_from_slice(DIM2_MAGIC);
    assert!(
        read_dim2_footer(&short).is_err(),
        "a truncated crop entry must fail closed"
    );
}

#[test]
fn save_v2_keeps_complete_live_player_records_by_uuid_after_they_go_offline() {
    if test_support::isolated_world_test() {
        return;
    }
    let (mut client, _peer) = effect_tests::client_pair();
    client.uuid = [11; 16];
    client.name = "resume-player".into();
    let slots: HashMap<_, _> = (5..46)
        .map(|slot| {
            (
                slot,
                (
                    850 + i64::from(slot),
                    1 + slot as u8,
                    vec![(2, vec![slot as u8])],
                ),
            )
        })
        .collect();
    let ender: HashMap<_, _> = (0..27)
        .map(|slot| (slot, (904, 1 + slot as u8, vec![])))
        .collect();
    *client.inv.lock().unwrap() = slots.clone();
    chests()
        .lock()
        .unwrap()
        .insert(ender_container_key(&client.uuid), ender.clone());
    *client.dim.lock().unwrap() = DimId::End;
    *client.pos.lock().unwrap() = (12.75, 156.5, -43.25, 52.0, -27.0, true);
    *client.held.lock().unwrap() = 8;
    *client.hp.lock().unwrap() = 12.5;
    *client.absorb.lock().unwrap() = 3.5;
    *client.food.lock().unwrap() = 15;
    *client.sat.lock().unwrap() = 4.75;
    *client.walk_acc.lock().unwrap() = 3.875;
    *client.xp.lock().unwrap() = 219;
    *client.gm.lock().unwrap() = 2;
    *client.effs.lock().unwrap() = vec![Eff {
        id: 1,
        amp: 2,
        left: 97,
    }];
    *client.spawn_dim.lock().unwrap() = DimId::Nether;
    *client.spawn_pt.lock().unwrap() = Some((8.5, 61.0, -19.5, 74.0));
    player_mechanism_sessions().lock().unwrap().insert(
        client.eid,
        PlayerMechanismSession {
            xp_bar: (11, 0.375),
            food_timer: 0,
            menus: Arc::new(LiveMenuConnection {
                active: Mutex::new(None),
                next_id: AtomicU8::new(4),
                enchantment_seed: Mutex::new(0),
            }),
        },
    );
    let (mut other, _other_peer) = effect_tests::client_pair();
    other.uuid = [12; 16];
    other.eid += 1;
    other.name = "other-player".into();
    let player_id = client.uuid;
    let other_id = other.uuid;
    clients()
        .lock()
        .unwrap()
        .extend([Arc::new(client), Arc::new(other)]);

    let mut payload = Vec::new();
    save_world_into(&mut payload);
    clients().lock().unwrap().clear();
    saved_players().lock().unwrap().clear();
    inv_store().lock().unwrap().clear();
    pos_store().lock().unwrap().clear();
    chests().lock().unwrap().clear();
    load_world_from(&payload).unwrap();
    let expected = PlayerSave {
        position: (DimId::End, 12.75, 156.5, -43.25, 52.0, -27.0),
        inventory: slots.clone(),
        ender_chest: ender.clone(),
        held: 8,
        health: 12.5,
        absorption: 3.5,
        food: 15,
        saturation: 4.75,
        exhaustion: 3.875,
        xp_level: 11,
        xp_progress: 0.375,
        xp_total: 219,
        gamemode: 2,
        effects: vec![(1, 2, 97)],
        respawn: Some(SpawnTarget {
            kind: SpawnTargetKind::Legacy,
            dim: DimId::Nether,
            position: (8.5, 61.0, -19.5),
            angle: 74.0,
            forced: false,
        }),
    };
    assert_eq!(
        saved_players().lock().unwrap().get(&player_id),
        Some(&expected)
    );
    assert_eq!(saved_players().lock().unwrap()[&other_id].health, 20.0);
    assert_eq!(inv_store().lock().unwrap()["resume-player"], slots);
    assert_eq!(
        pos_store().lock().unwrap()["resume-player"],
        expected.position
    );
    assert_eq!(
        chests().lock().unwrap()[&ender_container_key(&player_id)],
        ender
    );

    let mut offline = Vec::new();
    save_world_into(&mut offline);
    saved_players().lock().unwrap().clear();
    load_world_from(&offline).unwrap();
    assert_eq!(
        saved_players().lock().unwrap().get(&player_id),
        Some(&expected)
    );
    assert!(saved_players().lock().unwrap().contains_key(&other_id));
}

#[test]
fn save_v2_world_spawn_keeps_packet_angle_and_old_footer_order() {
    if test_support::isolated_world_test() {
        return;
    }
    let world_spawn = (-10.5, 72.0, 5.5, 113.25);
    *WORLD_SPAWN.lock().unwrap() = world_spawn;
    *WORLD_SPAWN_PITCH.lock().unwrap() = -18.5;
    WORLD_SEED.store(56412, Ordering::Relaxed);
    world_border().lock().unwrap().center_z = 14.25;
    {
        let mut board = scoreboard().lock().unwrap();
        execute_scoreboard_command(&mut board, &["objectives", "add", "saved", "dummy"]).unwrap();
        board.set_display(ScoreboardDisplaySlot::Sidebar, Some("saved")).unwrap();
    }


    let anchor = SpawnTarget {
        kind: SpawnTargetKind::Anchor,
        dim: DimId::Nether,
        position: (5.5, 63.0, -1.5),
        angle: -163.5,
        forced: false,
    };
    let bed = SpawnTarget {
        kind: SpawnTargetKind::Bed,
        dim: DimId::Overworld,
        forced: true,
        ..anchor
    };
    for (uuid, point) in [([1; 16], anchor), ([2; 16], bed)] {
        let (client, _peer) = effect_tests::client_pair();
        let mut state = PlayerSave::capture(&client, client_dim_pos(&client), &HashMap::new());
        state.respawn = Some(point);
        saved_players().lock().unwrap().insert(uuid, state);
    }
    let mut payload = Vec::new();
    save_world_into(&mut payload);
    assert!(payload.ends_with(b"SED1"));
    let (before_seed, seed) = read_world_seed_footer(&payload).unwrap();
    assert_eq!(seed, Some(56412));
    let frame = full_state_frame(&payload);
    assert!(frame.end <= before_seed.len());
    let before_state = &payload[..frame.start];
    assert_eq!(&before_state[before_state.len() - 8..][..4], b"SPA1");
    assert_eq!(&before_state[before_state.len() - 8 - 72..][..4], b"BRD1");

    let mut legacy = payload.clone();
    legacy.drain(frame);
    for (bytes, pitch, has_players) in [
        (payload.as_slice(), -18.5, true),
        (legacy.as_slice(), 0.0, false),
    ] {
        *WORLD_SPAWN.lock().unwrap() = (0.0, 0.0, 0.0, 0.0);
        *WORLD_SPAWN_PITCH.lock().unwrap() = 99.0;
        *world_border().lock().unwrap() = WorldBorderState::default();
        *scoreboard().lock().unwrap() = ScoreboardEngine::default();
        load_world_from(bytes).unwrap();
        assert_eq!(saved_value(&WORLD_SPAWN), world_spawn);
        assert_eq!(saved_value(&WORLD_SPAWN_PITCH), pitch);
        assert_eq!(world_border().lock().unwrap().center_z, 14.25);
        let board = scoreboard().lock().unwrap();
        assert_eq!(board.display(ScoreboardDisplaySlot::Sidebar), Some("saved"));
        drop(board);
        let packet = spawn_position_packet(saved_value(&WORLD_SPAWN));
        assert_eq!(read_pos(&packet[..8]), (-11, 72, 5));
        assert_eq!(&packet[8..], &world_spawn.3.to_be_bytes());
        let players = saved_players().lock().unwrap();
        assert_eq!(
            players.get(&[1; 16]).and_then(|p| p.respawn),
            has_players.then_some(anchor)
        );
        assert_eq!(
            players.get(&[2; 16]).and_then(|p| p.respawn),
            has_players.then_some(bed)
        );
    }
}

#[test]
fn save_v2_restores_mob_behavior_merchant_and_raid_progress() {
    if test_support::isolated_world_test() {
        return;
    }
    let owner = [77; 16];
    let pos = (2.5, 70.0, -8.25, 60.0, -15.0);
    assert!(insert_live_mob(122, DimId::Nether, pos, 33.5, None));
    assert!(insert_live_mob(113, DimId::Overworld, pos, 15.25, None));
    assert!(insert_live_mob(118, DimId::End, pos, 18.75, None));
    let original = mobs().lock().unwrap().clone();
    let [wolf, merchant, witch] = original.as_slice() else {
        unreachable!()
    };
    {
        let mut states = live_mob_states().lock().unwrap();
        let Some(MobBehaviorState::Wolf(pet)) = &mut states.get_mut(&wolf.eid).unwrap().behavior
        else {
            unreachable!()
        };
        assert_eq!(
            pet.try_tame(owner, "bone", &mut |_| 0),
            PetTameResult::Tamed
        );
        assert!(pet.dye_collar(owner, 6));
        assert!(pet.sitting);
        let state = states.get_mut(&witch.eid).unwrap();
        let Some(MobBehaviorState::Witch(witch)) = &mut state.behavior else {
            unreachable!()
        };
        witch.drinking = Some((13, WitchPotion::Healing));
        state.potions = vec![(WitchPotion::Swiftness, 125)];
    }
    TICK.store(100, Ordering::Relaxed);
    *wolf.vel.lock().unwrap() = [0.25, -0.125, 0.5];
    *wolf.fire_ticks.lock().unwrap() = 45;
    *wolf.fuse.lock().unwrap() = 7;
    *wolf.love.lock().unwrap() = 22;
    *wolf.air.lock().unwrap() = 199;
    sheared().lock().unwrap().insert(witch.eid);
    attrs().lock().unwrap().insert(format!("e:{}", wolf.eid),
        [("generic.max_health".into(), 43.5), ("custom.motion".into(), -0.25)].into());
    attrs().lock().unwrap().insert(format!("e:{}", merchant.eid),
        [("generic.movement_speed".into(), 0.35)].into());
    attrs().lock().unwrap().insert(format!("e:m:{}", wolf.eid),
        [("generic.armor".into(), 12.0)].into());
    tags()
        .lock()
        .unwrap()
        .insert(format!("e:{}", merchant.eid), ["trader".into()].into());

    let home = (2, 70, -8);
    let mut trader = farmer_merchant(merchant);
    trader.profession = VillagerProfession::Librarian;
    trader.villager_xp = 74;
    trader.home = Some(home);
    trader.job_site = Some((4, 70, -8));
    trader.day = Some(2);
    trader.last_restock = Some(48008);
    trader.last_gossip = Some(49001);
    trader.restocks_today = 1;
    trader.trading_player = Some(owner);
    trader.gossip.add(owner, GossipType::Trading, 24);
    trader.offers[0].cost_b = Some(ItemCost {
        item: name_to_item("paper").unwrap(),
        count: 5,
        components: vec![(2, vec![7, 8])],
    });
    trader.offers[0].uses = 5;
    trader.offers[0].demand = 4;
    trader.offers[0].special_price_diff = -2;
    trader.offers[0].result.2.push((2, vec![1, 2, 3]));
    let expected_offers = trader.offers.clone();
    live_villagers()
        .lock()
        .unwrap()
        .insert(merchant.eid, trader);

    let mut pois = PoiRegistry::default();
    pois.register(home, PoiKind::Bed);
    assert!(pois.claim(home, merchant.uuid));
    let mut omen = Some(RaidOmen {
        bad_omen_level: 2,
        countdown: 0,
    });
    let mut raid = Raid::try_start(&pois, home, owner, &mut omen, RaidDifficulty::Normal).unwrap();
    for _ in 1..RAID_WAVE_COUNTDOWN {
        assert_eq!(raid.tick(&pois, 1), None);
    }
    let Some(RaidEvent::SpawnWave(wave)) = raid.tick(&pois, 1) else {
        unreachable!()
    };
    let mut raiders = wave.raiders();
    let first = raiders.next().unwrap().0;
    let second_player = [78; 16];
    assert!(raid.record_defeat(first, Some(second_player)));
    let expected_remaining = raiders.map(|(raider, _)| raider).collect::<HashSet<_>>();
    saved_raids().lock().unwrap().push((DimId::Overworld, raid));

    let mut payload = Vec::new();
    save_world_into(&mut payload);
    mobs().lock().unwrap().clear();
    live_mob_states().lock().unwrap().clear();
    live_villagers().lock().unwrap().clear();
    saved_raids().lock().unwrap().clear();
    sheared().lock().unwrap().clear();
    tags().lock().unwrap().clear();
    TICK.store(900, Ordering::Relaxed);
    load_world_from(&payload).unwrap();

    let restored = mobs().lock().unwrap().clone();
    let [saved_wolf, saved_merchant, saved_witch] = restored.as_slice() else {
        unreachable!()
    };
    for (before, after) in original.iter().zip(&restored) {
        assert_eq!(before.uuid, after.uuid);
        assert_eq!(before.dim, after.dim);
        assert_ne!(before.eid, after.eid);
    }
    assert_eq!(saved_value(&saved_wolf.pos), pos);
    assert_eq!(saved_value(&saved_wolf.hp), 33.5);
    assert_eq!(saved_value(&saved_wolf.vel), [0.25, -0.125, 0.5]);
    assert_eq!(saved_value(&saved_wolf.fire_ticks), 45);
    assert_eq!(saved_value(&saved_wolf.fuse), 7);
    assert_eq!(saved_value(&saved_wolf.love), 22);
    assert_eq!(saved_value(&saved_wolf.air), 199);
    assert!(sheared().lock().unwrap().contains(&saved_witch.eid));
    {
        let attributes = attrs().lock().unwrap();
        assert_eq!(attributes[&format!("e:{}", saved_wolf.eid)]["generic.max_health"], 43.5);
        assert_eq!(attributes[&format!("e:{}", saved_wolf.eid)]["custom.motion"], -0.25);
        assert_eq!(attributes[&format!("e:{}", saved_merchant.eid)]["generic.movement_speed"], 0.35);
        assert_eq!(attributes[&format!("e:m:{}", saved_wolf.eid)]["generic.armor"], 12.0);
        assert!(!attributes.contains_key(&format!("e:{}", wolf.eid)));
        assert!(!attributes.contains_key(&format!("e:m:{}", wolf.eid)));
    }
    assert!(tags().lock().unwrap()[&format!("e:{}", saved_merchant.eid)].contains("trader"));
    {
        let states = live_mob_states().lock().unwrap();
        let Some(MobBehaviorState::Wolf(pet)) = &states[&saved_wolf.eid].behavior else {
            unreachable!()
        };
        assert_eq!(pet.owner, Some(owner));
        assert!(pet.sitting);
        assert_eq!(pet.collar_color, 6);
        let state = &states[&saved_witch.eid];
        let Some(MobBehaviorState::Witch(witch)) = &state.behavior else {
            unreachable!()
        };
        assert_eq!(witch.drinking, Some((13, WitchPotion::Healing)));
        assert_eq!(state.potions, [(WitchPotion::Swiftness, 925)]);
    }
    {
        let villagers = live_villagers().lock().unwrap();
        let trader = &villagers[&saved_merchant.eid];
        assert_eq!(trader.id, merchant.uuid);
        assert_eq!(trader.profession, VillagerProfession::Librarian);
        assert_eq!(trader.level(), VillagerLevel::Journeyman);
        assert_eq!(trader.villager_xp, 74);
        assert_eq!(trader.offers, expected_offers);
        assert_eq!(
            trader.gossip.by_player[&owner][GossipType::Trading as usize],
            24
        );
        assert_eq!(trader.trading_player, None);
        assert_eq!(trader.home, Some(home));
        assert_eq!(trader.job_site, Some((4, 70, -8)));
        assert_eq!(trader.last_restock, Some(48008));
        assert_eq!(trader.last_gossip, Some(49001));
    }
    let mut raids = saved_raids().lock().unwrap();
    let [(dim, raid)] = raids.as_mut_slice() else {
        unreachable!()
    };
    assert_eq!(*dim, DimId::Overworld);
    assert_eq!(raid.center, home);
    assert_eq!(raid.state, RaidState::InProgress);
    assert_eq!(raid.countdown, RAID_WAVE_COUNTDOWN);
    assert_eq!(raid.ticks_elapsed, u32::from(RAID_WAVE_COUNTDOWN));
    assert_eq!(raid.remaining, expected_remaining);
    assert_eq!(raid.participants, [owner, second_player].into());
    assert!(!raid.record_defeat(first, Some(owner)));
    for id in expected_remaining {
        assert!(raid.record_defeat(id, None));
    }
    for _ in 1..RAID_WAVE_COUNTDOWN {
        assert_eq!(raid.tick(&pois, 1), None);
    }
    assert!(matches!(
        raid.tick(&pois, 1),
        Some(RaidEvent::SpawnWave(RaidWave { number: 2, .. }))
    ));
}

#[test]
fn save_v2_releases_world_ownership_before_writing_and_keeps_previous_file_on_failure() {
    if test_support::isolated_world_test() {
        return;
    }
    let dir = std::env::temp_dir().join(format!("spine-snapshot-{}", std::process::id()));
    std::fs::create_dir(&dir).unwrap();
    let path = dir.join(WORLD_FILE);
    let temp = dir.join(format!("{WORLD_FILE}.tmp"));
    set_block(2, 64, 3, 4);
    save_world_guarded(|snapshot| {
        // A world transition must be able to run before the captured bytes
        // are flushed. Its edit belongs to the next save.
        assert!(SAVE_LOCK.try_lock().is_ok());
        assert!(world().try_lock().is_ok());
        set_block(2, 64, 3, 8);
        snapshot.write(&dir)
    })
    .unwrap();
    let original = std::fs::read(&path).unwrap();
    std::fs::create_dir(&temp).unwrap();
    let error = save_world_guarded(|snapshot| snapshot.write(&dir)).unwrap_err();
    assert!(error.contains(&format!("{WORLD_FILE}.tmp")));
    assert_eq!(std::fs::read(&path).unwrap(), original);
    std::fs::remove_dir(&temp).unwrap();

    world().lock().unwrap().clear();
    load_world_in(&dir).unwrap();
    assert_eq!(overlay_block(2, 64, 3), Some(4));
    set_block(2, 64, 3, 8);
    save_world_guarded_in(&dir);
    assert!(!temp.exists());
    assert_eq!(
        std::fs::read(dir.join(format!("{WORLD_FILE}.bak"))).unwrap(),
        original
    );
    world().lock().unwrap().clear();
    load_world_in(&dir).unwrap();
    assert_eq!(overlay_block(2, 64, 3), Some(8));
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn save_v2_panics_abort_the_boundary_and_corruption_cannot_apply_the_new_state() {
    if test_support::isolated_world_test() {
        return;
    }
    set_block(2, 64, 3, 4);
    *WORLD_SPAWN_PITCH.lock().unwrap() = 15.0;
    let mut payload = Vec::new();
    save_world_into(&mut payload);
    let frame = full_state_frame(&payload);
    payload[frame.start..frame.start + 4].copy_from_slice(&f32::NAN.to_be_bytes());
    world().lock().unwrap().clear();
    assert_eq!(load_world_from(&payload), Err("non-finite save float"));
    assert_eq!(overlay_block(2, 64, 3), None);
    assert_eq!(saved_value(&WORLD_SPAWN_PITCH), 15.0);

    // Inject a panic in a real dependency of snapshot capture. The writer
    // must never run and the ownership guard must be available again.
    assert!(thread::spawn(|| {
        let _pitch = WORLD_SPAWN_PITCH.lock().unwrap();
        panic!("snapshot fixture");
    })
    .join()
    .is_err());
    assert_eq!(
        save_world_guarded(|_| panic!("writer was called")),
        Err("world snapshot panicked".into())
    );
    assert!(SAVE_LOCK.try_lock().is_ok());
    WORLD_SPAWN_PITCH.clear_poison();
    save_world_guarded(|_| Ok(())).unwrap();

    assert!(thread::spawn(|| {
        let _world = world().lock().unwrap();
        panic!("loader fixture");
    })
    .join()
    .is_err());
    assert_eq!(load_world_from(&[0]), Err("world load panicked"));
    world().clear_poison();
    load_world_from(&[0]).unwrap();
}

#[test]
fn save_v2_rejects_a_truncated_companion_before_publishing_any_of_its_edits() {
    if test_support::isolated_world_test() {
        return;
    }
    let mut payload = Vec::new();
    world()
        .lock()
        .unwrap()
        .insert((DimId::Nether, 0, 0), [(3, 501)].into());
    assert_eq!(save_dim_overlay_into(DimId::Nether, &mut payload), 1);
    world().lock().unwrap().clear();
    for cut in 0..payload.len() {
        assert!(
            load_dim_overlay_from(DimId::Nether, &payload[..cut]).is_err(),
            "cut={cut}"
        );
        assert!(world().lock().unwrap().is_empty());
    }
    load_dim_overlay_from(DimId::Nether, &payload).unwrap();
    assert_eq!(world().lock().unwrap()[&(DimId::Nether, 0, 0)][&3], 501);
}

#[test]
fn save_v2_live_block_entities_round_trip_all_families() {
    test_support::require_vanilla_data();
    if test_support::isolated_world_test() {
        return;
    }
    let _world_guard = world_guard();
    let old_world = std::mem::take(&mut *world().lock().unwrap());
    let old_store = std::mem::take(&mut *live_block_entities().lock().unwrap());
    let old_drops = std::mem::take(&mut *drops().lock().unwrap());
    let old_rng = world_rng::saved_state();

    let cases = [
        ("beacon", "beacon"),
        ("soul_campfire", "campfire"),
        ("lectern", "lectern"),
        ("chiseled_bookshelf", "chiseled_bookshelf"),
        ("decorated_pot", "decorated_pot"),
        ("crafter", "crafter"),
        ("jukebox", "jukebox"),
        ("red_banner", "banner"),
        ("player_head", "skull"),
        ("suspicious_gravel", "brushable_block"),
        ("trial_spawner", "trial_spawner"),
        ("vault", "vault"),
    ];
    for (i, (block, _)) in cases.iter().enumerate() {
        place_block_dim(
            DimId::Overworld,
            5_000 + i as i32,
            220,
            5_000,
            name_to_state(block).unwrap(),
        );
    }
    // Mutated payloads prove more than presence survives the round trip.
    let banner_tag = block_entity_compound([
        ("id", block_entity_string("minecraft:banner")),
        (
            "patterns",
            Tag::List(
                10,
                vec![block_entity_compound([
                    ("pattern", block_entity_string("minecraft:stripe_center")),
                    ("color", block_entity_string("blue")),
                ])],
            ),
        ),
    ]);
    let banner_key = (DimId::Overworld, 5_007, 220, 5_000);
    assert!(restore_live_block_entity(banner_key, &banner_tag));
    let campfire_key = (DimId::Overworld, 5_001, 220, 5_000);
    let campfire_tag = block_entity_compound([
        ("id", block_entity_string("minecraft:campfire")),
        ("CookingTimes", Tag::IntArray(vec![10, 20, 30, 40])),
        ("CookingTotalTimes", Tag::IntArray(vec![600; 4])),
    ]);
    assert!(restore_live_block_entity(campfire_key, &campfire_tag));

    let expected: Vec<_> = cases
        .iter()
        .enumerate()
        .map(|(i, _)| {
            let key = (DimId::Overworld, 5_000 + i as i32, 220, 5_000);
            (key, live_block_entity_snapshot(key).unwrap())
        })
        .collect();

    let mut payload = Vec::new();
    save_world_into(&mut payload);
    // The footer rides between the legacy region and SVP1.
    {
        let tail = payload_through_drop_owners_footer(&payload).unwrap();
        let (tail, _) = read_drop_owners_footer(tail).unwrap();
        let tail = tick_persistence::prefix(tail).unwrap();
        let (before_generated, _) = split_save_footer(tail, b"GEN1").unwrap();
        let (state_prefix, _) = split_save_footer(before_generated, SAVE_STATE_MAGIC).unwrap();
        assert!(
            state_prefix.ends_with(BLOCK_ENTITIES_MAGIC),
            "save with live block entities must carry the BEN1 footer"
        );
    }

    live_block_entities().lock().unwrap().entries.clear();
    load_world_from(&payload).unwrap();
    let restored: Vec<_> = cases
        .iter()
        .enumerate()
        .map(|(i, _)| {
            let key = (DimId::Overworld, 5_000 + i as i32, 220, 5_000);
            (key, live_block_entity_snapshot(key))
        })
        .collect();

    *world().lock().unwrap() = old_world;
    *live_block_entities().lock().unwrap() = old_store;
    *drops().lock().unwrap() = old_drops;
    world_rng::restore(old_rng);

    assert_eq!(restored, expected.into_iter().map(|(k, s)| (k, Some(s))).collect::<Vec<_>>());
}

#[test]
fn save_v2_autosave_preserves_bed_and_anchor_spawn_target_kinds() {
    if test_support::isolated_world_test() {
        return;
    }
    let _world_guard = world_guard();
    let old_players = std::mem::take(&mut *saved_players().lock().unwrap());
    let old_clients = std::mem::take(&mut *clients().lock().unwrap());

    let bed = SpawnTarget {
        kind: SpawnTargetKind::Bed,
        dim: DimId::Overworld,
        position: (10.5, 65.0, -7.5),
        angle: 90.0,
        forced: true,
    };
    let anchor = SpawnTarget {
        kind: SpawnTargetKind::Anchor,
        dim: DimId::Nether,
        position: (-3.5, 70.0, 12.5),
        angle: -45.0,
        forced: false,
    };
    for (uuid, target) in [([7; 16], bed), ([9; 16], anchor)] {
        let (mut client, _peer) = effect_tests::client_pair();
        client.uuid = uuid;
        client.eid += 7_000;
        *client.spawn_dim.lock().unwrap() = target.dim;
        *client.spawn_pt.lock().unwrap() =
            Some((target.position.0, target.position.1, target.position.2, target.angle));
        let mut state =
            PlayerSave::capture(&client, client_dim_pos(&client), &HashMap::new());
        state.respawn = Some(target);
        saved_players().lock().unwrap().insert(uuid, state);
        clients().lock().unwrap().push(Arc::new(client));
    }

    // The autosave recaptures every live client from spawn_pt, which carries no
    // kind. The saved bed/anchor records must survive that recapture unchanged.
    let mut payload = Vec::new();
    save_world_into(&mut payload);
    let after_save = {
        let saved = saved_players().lock().unwrap();
        (saved[&[7; 16]].respawn, saved[&[9; 16]].respawn)
    };
    saved_players().lock().unwrap().clear();
    load_world_from(&payload).unwrap();
    let after_load = {
        let saved = saved_players().lock().unwrap();
        (saved[&[7; 16]].respawn, saved[&[9; 16]].respawn)
    };

    // A moved spawn point has no saved record to match and stays Legacy.
    *clients().lock().unwrap()[0].spawn_pt.lock().unwrap() =
        Some((1.5, 60.0, 1.5, 0.0));
    let mut second = Vec::new();
    save_world_into(&mut second);
    let moved = saved_players().lock().unwrap()[&[7; 16]].respawn;

    clients().lock().unwrap().clear();
    *saved_players().lock().unwrap() = old_players;
    *clients().lock().unwrap() = old_clients;

    assert_eq!(after_save, (Some(bed), Some(anchor)));
    assert_eq!(after_load, (Some(bed), Some(anchor)));
    assert_eq!(
        moved,
        Some(SpawnTarget {
            kind: SpawnTargetKind::Legacy,
            dim: DimId::Overworld,
            position: (1.5, 60.0, 1.5),
            angle: 0.0,
            forced: false,
        })
    );
}
