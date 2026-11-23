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

fn advancement_frame(payload: &[u8]) -> std::ops::Range<usize> {
    let end = full_state_frame(payload).start;
    let (prefix, body) = split_save_footer(&payload[..end], ADVANCEMENTS_MAGIC).unwrap();
    assert!(body.is_some(), "expected ADV1 footer");
    prefix.len()..end
}

fn with_advancement_body(payload: &[u8], body: &[u8]) -> Vec<u8> {
    let frame = advancement_frame(payload);
    let mut edited = payload[..frame.start].to_vec();
    edited.extend_from_slice(body);
    edited.extend_from_slice(&(body.len() as u64).to_be_bytes());
    edited.extend_from_slice(ADVANCEMENTS_MAGIC);
    edited.extend_from_slice(&payload[frame.end..]);
    edited
}

#[test]
fn save_v2_advancements_round_trip_grants_and_criteria() {
    if test_support::isolated_world_test() {
        return;
    }
    let _guard = world_guard();
    let old_grants = std::mem::take(&mut *adv_done().lock().unwrap_or_else(|e| e.into_inner()));
    let old_core = std::mem::take(&mut *player_advancements().lock().unwrap_or_else(|e| e.into_inner()));
    let old_datapack = std::mem::take(&mut *adv_prog().lock().unwrap_or_else(|e| e.into_inner()));

    let mut empty = Vec::new();
    save_world_into(&mut empty);
    let state_start = full_state_frame(&empty).start;
    assert!(!empty[..state_start].ends_with(ADVANCEMENTS_MAGIC));

    adv_done().lock().unwrap_or_else(|e| e.into_inner()).insert(
        "AdvTester".into(), ["datapack:complete".into(), "minecraft:story/root".into()].into());
    adv_done().lock().unwrap_or_else(|e| e.into_inner()).insert(
        "Second".into(), ["datapack:another".into()].into());
    {
        let mut tracker = player_advancements().lock().unwrap_or_else(|e| e.into_inner());
        assert!(tracker.grant_criterion("AdvTester", "minecraft:story/root", "crafting_table", 123_456));
        assert!(tracker.grant_criterion("AdvTester", "minecraft:story/obtain_armor", "iron_boots", -7));
    }
    adv_prog().lock().unwrap_or_else(|e| e.into_inner()).insert(
        "AdvTester".into(), [("datapack:partial".into(), ["first".into(), "third".into()].into())].into());
    let mut payload = Vec::new();
    save_world_into(&mut payload);
    let mut second = Vec::new();
    save_world_into(&mut second);
    assert_eq!(payload, second, "advancement output must be deterministic");

    adv_done().lock().unwrap_or_else(|e| e.into_inner()).clear();
    *player_advancements().lock().unwrap_or_else(|e| e.into_inner()) = AdvancementTracker::default();
    adv_prog().lock().unwrap_or_else(|e| e.into_inner()).clear();
    load_world_from(&payload).unwrap();
    let restored_grants = adv_done().lock().unwrap_or_else(|e| e.into_inner()).clone();
    assert_eq!(restored_grants["AdvTester"].len(), 2);
    assert!(restored_grants["AdvTester"].contains("datapack:complete"));
    assert!(restored_grants["Second"].contains("datapack:another"));
    let tracker = player_advancements().lock().unwrap_or_else(|e| e.into_inner());
    assert_eq!(tracker.progress["AdvTester"]["minecraft:story/root"]["crafting_table"], 123_456);
    assert_eq!(tracker.progress["AdvTester"]["minecraft:story/obtain_armor"]["iron_boots"], -7);
    assert!(tracker.sent_progress.is_empty());
    drop(tracker);
    assert_eq!(adv_prog().lock().unwrap_or_else(|e| e.into_inner())["AdvTester"]["datapack:partial"],
        ["first".to_owned(), "third".to_owned()].into());

    // Old MET4 snapshots have completed IDs but no criteria footer.
    let frame = advancement_frame(&payload);
    payload.drain(frame);
    adv_done().lock().unwrap_or_else(|e| e.into_inner()).clear();
    load_world_from(&payload).unwrap();
    assert!(adv_done().lock().unwrap_or_else(|e| e.into_inner())["AdvTester"].contains("datapack:complete"));
    assert!(player_advancements().lock().unwrap_or_else(|e| e.into_inner()).progress.is_empty());
    assert!(adv_prog().lock().unwrap_or_else(|e| e.into_inner()).is_empty());

    *adv_done().lock().unwrap_or_else(|e| e.into_inner()) = old_grants;
    *player_advancements().lock().unwrap_or_else(|e| e.into_inner()) = old_core;
    *adv_prog().lock().unwrap_or_else(|e| e.into_inner()) = old_datapack;
}

#[test]
fn save_v2_advancements_reject_corrupted_footers_before_restoring_grants() {
    if test_support::isolated_world_test() {
        return;
    }
    let _guard = world_guard();
    let old_grants = std::mem::take(&mut *adv_done().lock().unwrap_or_else(|e| e.into_inner()));
    let old_core = std::mem::take(&mut *player_advancements().lock().unwrap_or_else(|e| e.into_inner()));
    let old_datapack = std::mem::take(&mut *adv_prog().lock().unwrap_or_else(|e| e.into_inner()));

    adv_done().lock().unwrap_or_else(|e| e.into_inner()).insert(
        "AdvTester".into(), ["minecraft:story/root".into()].into());
    let mut payload = Vec::new();
    save_world_into(&mut payload);
    let frame = advancement_frame(&payload);
    let body = &payload[frame.start..frame.end - 12];

    let mut invalid_utf8 = Vec::new();
    write_varint(&mut invalid_utf8, 1);
    invalid_utf8.extend_from_slice(&1u16.to_be_bytes());
    invalid_utf8.push(0xff);
    write_varint(&mut invalid_utf8, 1);
    invalid_utf8.extend_from_slice(&1u16.to_be_bytes());
    invalid_utf8.push(b'x');
    write_varint(&mut invalid_utf8, 0);
    write_varint(&mut invalid_utf8, 0);

    let mut excessive_count = Vec::new();
    write_varint(&mut excessive_count, (ADV_MAX_PLAYERS + 1) as i64);
    excessive_count.resize(ADV_MAX_PLAYERS + 12, 0);
    let mut excessive_length = Vec::new();
    write_varint(&mut excessive_length, 1);
    excessive_length.extend_from_slice(&((ADV_MAX_NAME_BYTES + 1) as u16).to_be_bytes());
    let mut trailing_bytes = body.to_vec();
    trailing_bytes.push(0);

    adv_done().lock().unwrap_or_else(|e| e.into_inner()).clear();
    let corruptions = [
        ("truncated", body[..body.len() - 1].to_vec()),
        ("invalid UTF-8", invalid_utf8),
        ("excessive count", excessive_count),
        ("excessive string", excessive_length),
        ("trailing bytes", trailing_bytes),
    ];
    for (reason, corrupted) in corruptions {
        let edited = with_advancement_body(&payload, &corrupted);
        assert!(load_world_from(&edited).is_err(), "{reason}");
        assert!(adv_done().lock().unwrap_or_else(|e| e.into_inner()).is_empty(), "{reason}");
    }

    *adv_done().lock().unwrap_or_else(|e| e.into_inner()) = old_grants;
    *player_advancements().lock().unwrap_or_else(|e| e.into_inner()) = old_core;
    *adv_prog().lock().unwrap_or_else(|e| e.into_inner()) = old_datapack;
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
        let rules = command_gamerules().lock().unwrap_or_else(|e| e.into_inner());
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

    attrs().lock().unwrap_or_else(|e| e.into_inner()).insert("e:999999".into(),
        [("generic.max_health".into(), 64.0)].into());
    restore_gamerules(Some(&original));
    load_world_from(&payload).unwrap();
    assert!(!attrs().lock().unwrap_or_else(|e| e.into_inner()).contains_key("e:999999"));
    {
        let rules = command_gamerules().lock().unwrap_or_else(|e| e.into_inner());
        for (index, &(name, _, _)) in COMMAND_GAMERULE_DEFAULTS.iter().enumerate() {
            assert_eq!(gamerule_value(name, &rules), changed[index], "rule {name}");
            assert_eq!(rules[name], changed[index], "registry {name}");
        }
    }

    for legacy in [&old_state, &no_state] {
        restore_gamerules(Some(&changed));
        load_world_from(legacy).unwrap();
        let rules = command_gamerules().lock().unwrap_or_else(|e| e.into_inner());
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
    let old_world = world().take_all();
    let old_drops = std::mem::take(&mut *drops().lock().unwrap_or_else(|e| e.into_inner()));
    let old_mobs = std::mem::take(&mut *mobs().lock().unwrap_or_else(|e| e.into_inner()));
    let old_time = WORLD_TIME.swap(555, Ordering::Relaxed);
    let old_forced_chunk = std::mem::take(&mut *forceloaded().lock().unwrap_or_else(|e| e.into_inner()));
    let old_rng = world_rng::saved_state();
    forceloaded().lock().unwrap_or_else(|e| e.into_inner()).insert((0, 0));
    world()
        .write(&(DimId::Overworld, 0, 0))
        .entry((DimId::Overworld, 0, 0))
        .or_default()
        .insert(0x0000_0004, 4);
    drops().lock().unwrap_or_else(|e| e.into_inner()).push(Drop {
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
    mobs().lock().unwrap_or_else(|e| e.into_inner()).push(Arc::new(Mob {
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
        world().clear();
        *forceloaded().lock().unwrap_or_else(|e| e.into_inner()) = HashSet::new();
        *drops().lock().unwrap_or_else(|e| e.into_inner()) = Vec::new();
        *mobs().lock().unwrap_or_else(|e| e.into_inner()) = Vec::new();
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
        let m = mobs().lock().unwrap_or_else(|e| e.into_inner()).first().cloned();
        Loaded {
            chunks: world().len(),
            forced: forceloaded().lock().unwrap_or_else(|e| e.into_inner()).len(),
            world_time: WORLD_TIME.load(Ordering::Relaxed),
            drops: drops().lock().unwrap_or_else(|e| e.into_inner()).len(),
            drop_age: d.as_ref().map(|d| d.0).unwrap_or(-1),
            drop_item: d.as_ref().map(|d| d.1).unwrap_or(0),
            drop_count: d.as_ref().map(|d| d.2).unwrap_or(0),
            drop_comps: d.map(|d| d.3).unwrap_or_default(),
            mobs: mobs().lock().unwrap_or_else(|e| e.into_inner()).len(),
            mob_etype: m.as_ref().map(|m| m.etype).unwrap_or(0),
            mob_hp: m.as_ref().map(|m| *m.hp.lock().unwrap_or_else(|e| e.into_inner())).unwrap_or(0.0),
            mob_air: m.as_ref().map(|m| *m.air.lock().unwrap_or_else(|e| e.into_inner())).unwrap_or(-1),
        }
    };
    world().restore_all(old_world);
    *forceloaded().lock().unwrap_or_else(|e| e.into_inner()) = old_forced_chunk;
    *drops().lock().unwrap_or_else(|e| e.into_inner()) = old_drops;
    *mobs().lock().unwrap_or_else(|e| e.into_inner()) = old_mobs;
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
    let old_arrows = std::mem::take(&mut *arrows().lock().unwrap_or_else(|e| e.into_inner()));
    let old_drops = std::mem::take(&mut *drops().lock().unwrap_or_else(|e| e.into_inner()));
    arrows().lock().unwrap_or_else(|e| e.into_inner()).push(Arrow {
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
    arrows().lock().unwrap_or_else(|e| e.into_inner()).push(Arrow {
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
    let live_arrows_before: Vec<i32> = arrows().lock().unwrap_or_else(|e| e.into_inner()).iter().map(|a| a.eid).collect();
    let live_drops_before = drops().lock().unwrap_or_else(|e| e.into_inner()).len();
    let mut payload = Vec::new();
    save_world_into(&mut payload);
    let live_arrows_after: Vec<i32> = arrows().lock().unwrap_or_else(|e| e.into_inner()).iter().map(|a| a.eid).collect();
    let live_drops_after = drops().lock().unwrap_or_else(|e| e.into_inner()).len();
    // A second save of an unchanged world must produce identical bytes. A save that
    // consumes the world is not idempotent and cannot be retried safely.
    let mut payload2 = Vec::new();
    save_world_into(&mut payload2);
    *drops().lock().unwrap_or_else(|e| e.into_inner()) = Vec::new();
    *arrows().lock().unwrap_or_else(|e| e.into_inner()) = Vec::new();
    load_world_from(&payload).unwrap();
    let loaded_drops: Vec<(i64, u8)> = drops()
        .lock()
        .unwrap()
        .iter()
        .map(|d| (d.item, d.cnt))
        .collect();
    let loaded_arrow_kinds: Vec<u8> = arrows().lock().unwrap_or_else(|e| e.into_inner()).iter().map(|a| a.kind).collect();
    *arrows().lock().unwrap_or_else(|e| e.into_inner()) = old_arrows;
    *drops().lock().unwrap_or_else(|e| e.into_inner()) = old_drops;
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
    let old_world = world().snapshot();
    let old_falling = std::mem::take(&mut *falling().lock().unwrap_or_else(|e| e.into_inner()));

    falling().lock().unwrap_or_else(|e| e.into_inner()).extend([
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
    falling().lock().unwrap_or_else(|e| e.into_inner()).clear();
    load_world_from(&payload).unwrap();

    let q = falling().lock().unwrap_or_else(|e| e.into_inner()).clone();
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

    world().restore_all(old_world);
    *falling().lock().unwrap_or_else(|e| e.into_inner()) = old_falling;
}

/// A save written by the old cell-moving model carries FAL1. Loading it must convert each cell
/// to an entity and remove the block, or the block would remain behind and duplicate.
#[test]
fn a_legacy_fal1_save_converts_its_blocks_to_entities() {
    if test_support::isolated_world_test() {
        return;
    }
    let _g = world_guard();
    let old_world = world().snapshot();
    let old_falling = std::mem::take(&mut *falling().lock().unwrap_or_else(|e| e.into_inner()));

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

    world().clear();
    falling().lock().unwrap_or_else(|e| e.into_inner()).clear();
    load_world_from(&payload).unwrap();

    assert_eq!(
        block_at(1, 100, 2),
        0,
        "the legacy block must leave the world"
    );
    assert_eq!(
        world()
            .read(&(DimId::Overworld, 0, 0))
            .get(&(DimId::Overworld, 0, 0))
            .and_then(|c| c.get(&local_idx(1, 100, 2)))
            .copied(),
        Some(0),
        "the migrated cell is an explicit air override, so a non-air generated base is not exposed"
    );
    let q = falling().lock().unwrap_or_else(|e| e.into_inner()).clone();
    assert_eq!(q.len(), 1, "the legacy cell becomes one entity: {q:?}");
    assert_eq!(q[0].state, sand, "with the state the block had");
    assert!(
        (q[0].x - 1.5).abs() < 1e-9 && (q[0].y - 100.0).abs() < 1e-9,
        "at the cell center"
    );

    world().restore_all(old_world);
    *falling().lock().unwrap_or_else(|e| e.into_inner()) = old_falling;
}

#[test]
fn the_world_rng_stream_continues_across_a_save_and_load_round_trip() {
    if test_support::isolated_world_test() {
        return;
    }
    let _g = world_guard();
    let old_world = world().snapshot();
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

    world().restore_all(old_world);
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
        furnaces().lock().unwrap_or_else(|e| e.into_inner()).insert(
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
    assert!(!furnaces().lock().unwrap_or_else(|e| e.into_inner()).contains_key(&ow_block(broken_furnace)));
    assert!(!signs()
        .lock()
        .unwrap()
        .keys()
        .any(|&(dim, x, y, z, _)| dim == DimId::Overworld && (x, y, z) == broken_sign));
    let mut payload = Vec::new();
    save_world_into(&mut payload);
    world().clear();
    furnaces().lock().unwrap_or_else(|e| e.into_inner()).clear();
    signs().lock().unwrap_or_else(|e| e.into_inner()).clear();
    drops().lock().unwrap_or_else(|e| e.into_inner()).clear();
    load_world_from(&payload).unwrap();
    let fs = furnaces().lock().unwrap_or_else(|e| e.into_inner());
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
    let sg = signs().lock().unwrap_or_else(|e| e.into_inner());
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
    let old_world = world().take_all();
    let old_drops = std::mem::take(&mut *drops().lock().unwrap_or_else(|e| e.into_inner()));
    let old_mobs = std::mem::take(&mut *mobs().lock().unwrap_or_else(|e| e.into_inner()));
    let old_time = WORLD_TIME.swap(555, Ordering::Relaxed);
    let old_forced = std::mem::take(&mut *forceloaded().lock().unwrap_or_else(|e| e.into_inner()));
    let old_rng = world_rng::saved_state();

    let stone = name_to_item("stone").unwrap();
    let uuid_a = [0xAA; 16];
    let uuid_b = [0xBB; 16];
    let uuid_c = [0xCC; 16];
    *drops().lock().unwrap_or_else(|e| e.into_inner()) = vec![
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

    drops().lock().unwrap_or_else(|e| e.into_inner()).clear();
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

    world().restore_all(old_world);
    *drops().lock().unwrap_or_else(|e| e.into_inner()) = old_drops;
    *mobs().lock().unwrap_or_else(|e| e.into_inner()) = old_mobs;
    WORLD_TIME.store(old_time, Ordering::Relaxed);
    *forceloaded().lock().unwrap_or_else(|e| e.into_inner()) = old_forced;
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
#[cfg(megu_vanilla_data)]
#[test]
fn p2_3_s2a_one_cell_in_three_dimensions_stays_independent() {
    if test_support::isolated_world_test() {
        return;
    }
    test_support::require_vanilla_data();
    let _world_guard = world_guard();
    p2_3_s2a_init_generators();
    let old_world = world().take_all();

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

    world().restore_all(old_world);
    assert!(worldgen::init(0, name_to_state), "leave the Overworld active");
}

/// P2-3-S2a: `world_overlay.bin` stays the Overworld's file, byte for byte.
///
/// A legacy `(cx, cz)` chunk loads as `(DimId::Overworld, cx, cz)`, and another dimension's edits
/// change none of its bytes: the format, the header count and the keys are what they were before
/// the key carried a dimension, which is what lets an older build read this file.
#[cfg(megu_vanilla_data)]
#[test]
fn p2_3_s2a_legacy_overworld_save_and_dim_overlay_tables_round_trip() {
    if test_support::isolated_world_test() {
        return;
    }
    test_support::require_vanilla_data();
    let _world_guard = world_guard();
    p2_3_s2a_init_generators();
    let old_world = world().take_all();

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
        .write_all()
        .retain(|key, _| key.0 == DimId::Overworld);
    let mut without_dims = Vec::new();
    save_world_into(&mut without_dims);
    assert_eq!(
        with_dims, without_dims,
        "another dimension's edits must not change one byte of the Overworld's save"
    );

    // It loads as the Overworld's chunks and nothing else.
    world().clear();
    load_world_from(&with_dims).unwrap();
    assert_eq!(
        overlay_block_dim(DimId::Overworld, cell.0, cell.1, cell.2),
        Some(states[0].1),
        "a legacy (cx, cz) chunk loads into the Overworld"
    );
    {
        let w = world().read_all();
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
        world().remove(&(*dim, cell.0 >> 4, cell.2 >> 4));
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
    world().clear();
    let mut empty = Vec::new();
    assert_eq!(save_dim_overlay_into(DimId::Nether, &mut empty), 0);
    assert_eq!(empty, vec![0], "an empty table is a zero count and nothing else");

    world().restore_all(old_world);
    assert!(worldgen::init(0, name_to_state), "leave the Overworld active");
}

/// P2-3-S2a: the companion files follow the legacy one onto disk, and disappear when their
/// dimension has no edits left.
///
/// The save path is the one the server runs, pointed at a scratch directory so the fixture does not
/// write into the checkout's working directory.
#[cfg(megu_vanilla_data)]
#[test]
fn p2_3_s2a_companion_dim_files_are_written_loaded_and_removed_when_empty() {
    if test_support::isolated_world_test() {
        return;
    }
    test_support::require_vanilla_data();
    let _world_guard = world_guard();
    p2_3_s2a_init_generators();
    let old_world = world().take_all();
    let dir = std::env::temp_dir().join(format!("megu-p2_3_s2a-{}", std::process::id()));
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
    world().clear();
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
        .write_all()
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
    world().clear();
    load_world_in(&dir).expect("reload the scratch save");
    for dim in [DimId::Nether, DimId::End] {
        assert_eq!(overlay_block_dim(dim, cell.0, cell.1, cell.2), None);
    }

    let _ = std::fs::remove_dir_all(&dir);
    world().restore_all(old_world);
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
    let old_signs = std::mem::take(&mut *signs().lock().unwrap_or_else(|e| e.into_inner()));
    let old_pos = std::mem::take(&mut *pos_store().lock().unwrap_or_else(|e| e.into_inner()));
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
    assert_eq!(signs().lock().unwrap_or_else(|e| e.into_inner()).len(), 3);
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
    signs().lock().unwrap_or_else(|e| e.into_inner()).clear();
    load_world_from(&with_dims).unwrap();
    assert_eq!(signs().lock().unwrap_or_else(|e| e.into_inner()).len(), 3);
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
    signs().lock().unwrap_or_else(|e| e.into_inner()).clear();
    load_world_from(&overworld_only).unwrap();
    assert_eq!(signs().lock().unwrap_or_else(|e| e.into_inner()).len(), 1);
    assert_eq!(signs().lock().unwrap_or_else(|e| e.into_inner()).get(&ow_key), Some(&lines));

    world_rng::restore(old_rng);
    *signs().lock().unwrap_or_else(|e| e.into_inner()) = old_signs;
    *pos_store().lock().unwrap_or_else(|e| e.into_inner()) = old_pos;
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
    let old_pending = std::mem::take(&mut *pending_blocks().lock().unwrap_or_else(|e| e.into_inner()));
    let old_backoff = std::mem::take(&mut *backoff().lock().unwrap_or_else(|e| e.into_inner()));
    let old_cells = std::mem::take(&mut *plugin_cells().lock().unwrap_or_else(|e| e.into_inner()));
    let old_pb = std::mem::take(&mut *plugin_backoff().lock().unwrap_or_else(|e| e.into_inner()));
    let old_waters = std::mem::take(&mut *waters().lock().unwrap_or_else(|e| e.into_inner()));
    let old_hoppers = std::mem::take(&mut *hoppers().lock().unwrap_or_else(|e| e.into_inner()));

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
    assert_eq!(pending_blocks().lock().unwrap_or_else(|e| e.into_inner()).len(), 3);
    assert_eq!(backoff().lock().unwrap_or_else(|e| e.into_inner()).len(), 3);
    assert_eq!(plugin_cells().lock().unwrap_or_else(|e| e.into_inner()).len(), 3);
    assert_eq!(plugin_backoff().lock().unwrap_or_else(|e| e.into_inner()).len(), 3);
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

    *waters().lock().unwrap_or_else(|e| e.into_inner()) = old_waters;
    *hoppers().lock().unwrap_or_else(|e| e.into_inner()) = old_hoppers;
    *plugin_backoff().lock().unwrap_or_else(|e| e.into_inner()) = old_pb;
    *plugin_cells().lock().unwrap_or_else(|e| e.into_inner()) = old_cells;
    *backoff().lock().unwrap_or_else(|e| e.into_inner()) = old_backoff;
    *pending_blocks().lock().unwrap_or_else(|e| e.into_inner()) = old_pending;
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
    let old_pos = std::mem::take(&mut *pos_store().lock().unwrap_or_else(|e| e.into_inner()));
    let old_signs = std::mem::take(&mut *signs().lock().unwrap_or_else(|e| e.into_inner()));
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
    pos_store().lock().unwrap_or_else(|e| e.into_inner()).clear();
    load_world_from(legacy).unwrap();
    for (_, name, x, y, z, yaw, pitch) in &expected {
        assert_eq!(
            pos_store().lock().unwrap_or_else(|e| e.into_inner()).get(name),
            Some(&(DimId::Overworld, *x, *y, *z, *yaw, *pitch)),
            "{name}: a legacy position defaults to the Overworld"
        );
    }

    // The full payload restores each player's exact (DimId, x, y, z, yaw, pitch).
    pos_store().lock().unwrap_or_else(|e| e.into_inner()).clear();
    load_world_from(&full).unwrap();
    for (dim, name, x, y, z, yaw, pitch) in &expected {
        assert_eq!(
            pos_store().lock().unwrap_or_else(|e| e.into_inner()).get(name),
            Some(&(*dim, *x, *y, *z, *yaw, *pitch)),
            "{name}: its dimension and position round-trip"
        );
    }

    // An Overworld-only save writes zero footer bytes.
    pos_store().lock().unwrap_or_else(|e| e.into_inner()).clear();
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
    *signs().lock().unwrap_or_else(|e| e.into_inner()) = old_signs;
    *pos_store().lock().unwrap_or_else(|e| e.into_inner()) = old_pos;
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
    let old_chests = std::mem::take(&mut *chests().lock().unwrap_or_else(|e| e.into_inner()));
    let old_furnaces = std::mem::take(&mut *furnaces().lock().unwrap_or_else(|e| e.into_inner()));
    let old_crops = std::mem::take(&mut *crops().lock().unwrap_or_else(|e| e.into_inner()));
    let old_world = world().take_all();
    let old_loot = std::mem::take(&mut *pending_block_loot().lock().unwrap_or_else(|e| e.into_inner()));
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
        crops().lock().unwrap_or_else(|e| e.into_inner()).insert(key, i as u8);
    }
    // The Overworld's crop age also lives in its overlay: there is no legacy crop section, so a
    // legacy load rebuilds it from the block state.
    world()
        .write(&(DimId::Overworld, pos.0 >> 4, pos.2 >> 4))
        .entry((DimId::Overworld, pos.0 >> 4, pos.2 >> 4))
        .or_default()
        .insert(local_idx(pos.0, pos.1, pos.2), 4278);
    let uuid = [7u8; 16];
    let ender = ender_container_key(&uuid);
    assert!(
        matches!(ender, ContainerKey::Block((DimId::Overworld, _, _, _))),
        "an Ender key must be stored under the Overworld in every dimension"
    );
    let ender_slots: HashMap<i16, Stack> = [(1i16, (iron, 9, Vec::new()))].into_iter().collect();
    chests().lock().unwrap_or_else(|e| e.into_inner()).insert(ender, ender_slots.clone());
    assert_eq!(chests().lock().unwrap_or_else(|e| e.into_inner()).len(), 4);
    assert_eq!(furnaces().lock().unwrap_or_else(|e| e.into_inner()).len(), 3);
    assert_eq!(crops().lock().unwrap_or_else(|e| e.into_inner()).len(), 3);

    let mut payload = Vec::new();
    save_world_into(&mut payload);
    assert!(
        payload.windows(DIM2_MAGIC.len()).any(|w| w == DIM2_MAGIC),
        "a non-Overworld container must produce a DIM2 footer"
    );
    chests().lock().unwrap_or_else(|e| e.into_inner()).clear();
    furnaces().lock().unwrap_or_else(|e| e.into_inner()).clear();
    crops().lock().unwrap_or_else(|e| e.into_inner()).clear();
    world().clear();
    load_world_from(&payload).unwrap();

    assert_eq!(chests().lock().unwrap_or_else(|e| e.into_inner()).len(), 4);
    assert_eq!(furnaces().lock().unwrap_or_else(|e| e.into_inner()).len(), 3);
    assert_eq!(crops().lock().unwrap_or_else(|e| e.into_inner()).len(), 3);
    for (i, dim) in DimId::ALL.into_iter().enumerate() {
        let key = (dim, pos.0, pos.1, pos.2);
        assert_eq!(
            chests().lock().unwrap_or_else(|e| e.into_inner()).get(&ContainerKey::Block(key)).and_then(|m| m.get(&0)).map(|s| s.1),
            Some((i + 1) as u8),
            "{dim:?}: its own chest content came back"
        );
        assert_eq!(
            furnaces().lock().unwrap_or_else(|e| e.into_inner()).get(&key).map(|f| f.burn_left),
            Some(100 + i as i32),
            "{dim:?}: its own furnace state came back"
        );
        assert_eq!(
            crops().lock().unwrap_or_else(|e| e.into_inner()).get(&key).copied(),
            Some(i as u8),
            "{dim:?}: its own crop age came back"
        );
    }
    assert_eq!(
        chests().lock().unwrap_or_else(|e| e.into_inner()).get(&ender),
        Some(&ender_slots),
        "the Ender chest is shared across dimensions through its Overworld key"
    );

    world_rng::restore(old_rng);
    *chests().lock().unwrap_or_else(|e| e.into_inner()) = old_chests;
    *furnaces().lock().unwrap_or_else(|e| e.into_inner()) = old_furnaces;
    *crops().lock().unwrap_or_else(|e| e.into_inner()) = old_crops;
    world().restore_all(old_world);
    *pending_block_loot().lock().unwrap_or_else(|e| e.into_inner()) = old_loot;
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
    let old_chests = std::mem::take(&mut *chests().lock().unwrap_or_else(|e| e.into_inner()));
    let old_furnaces = std::mem::take(&mut *furnaces().lock().unwrap_or_else(|e| e.into_inner()));
    let old_crops = std::mem::take(&mut *crops().lock().unwrap_or_else(|e| e.into_inner()));
    let old_world = world().take_all();
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
        furnaces().lock().unwrap_or_else(|e| e.into_inner()).insert(
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
        crops().lock().unwrap_or_else(|e| e.into_inner()).insert(key, 3);
    }
    world()
        .write(&(DimId::Overworld, pos.0 >> 4, pos.2 >> 4))
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
        chests().lock().unwrap_or_else(|e| e.into_inner()).remove(&ContainerKey::Block(key));
        furnaces().lock().unwrap_or_else(|e| e.into_inner()).remove(&key);
        crops().lock().unwrap_or_else(|e| e.into_inner()).remove(&key);
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

    chests().lock().unwrap_or_else(|e| e.into_inner()).clear();
    furnaces().lock().unwrap_or_else(|e| e.into_inner()).clear();
    crops().lock().unwrap_or_else(|e| e.into_inner()).clear();
    world().clear();
    load_world_from(&overworld_only).unwrap();
    let ow = ow_block(pos);
    assert_eq!(chests().lock().unwrap_or_else(|e| e.into_inner()).len(), 1);
    assert!(chests().lock().unwrap_or_else(|e| e.into_inner()).contains_key(&ContainerKey::Block(ow)));
    assert!(furnaces().lock().unwrap_or_else(|e| e.into_inner()).contains_key(&ow));
    assert_eq!(crops().lock().unwrap_or_else(|e| e.into_inner()).get(&ow).copied(), Some(3));

    world_rng::restore(old_rng);
    *chests().lock().unwrap_or_else(|e| e.into_inner()) = old_chests;
    *furnaces().lock().unwrap_or_else(|e| e.into_inner()) = old_furnaces;
    *crops().lock().unwrap_or_else(|e| e.into_inner()) = old_crops;
    world().restore_all(old_world);
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
    *client.inv.lock().unwrap_or_else(|e| e.into_inner()) = slots.clone();
    chests()
        .lock()
        .unwrap()
        .insert(ender_container_key(&client.uuid), ender.clone());
    *client.dim.lock().unwrap_or_else(|e| e.into_inner()) = DimId::End;
    *client.pos.lock().unwrap_or_else(|e| e.into_inner()) = (12.75, 156.5, -43.25, 52.0, -27.0, true);
    *client.held.lock().unwrap_or_else(|e| e.into_inner()) = 8;
    *client.hp.lock().unwrap_or_else(|e| e.into_inner()) = 12.5;
    *client.absorb.lock().unwrap_or_else(|e| e.into_inner()) = 3.5;
    *client.food.lock().unwrap_or_else(|e| e.into_inner()) = 15;
    *client.sat.lock().unwrap_or_else(|e| e.into_inner()) = 4.75;
    *client.walk_acc.lock().unwrap_or_else(|e| e.into_inner()) = 3.875;
    *client.xp.lock().unwrap_or_else(|e| e.into_inner()) = 219;
    *client.gm.lock().unwrap_or_else(|e| e.into_inner()) = 2;
    *client.effs.lock().unwrap_or_else(|e| e.into_inner()) = vec![Eff {
        id: 1,
        amp: 2,
        left: 97,
    }];
    *client.spawn_dim.lock().unwrap_or_else(|e| e.into_inner()) = DimId::Nether;
    *client.spawn_pt.lock().unwrap_or_else(|e| e.into_inner()) = Some((8.5, 61.0, -19.5, 74.0));
    player_mechanism_sessions().lock().unwrap_or_else(|e| e.into_inner()).insert(
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
    clients().lock().unwrap_or_else(|e| e.into_inner()).clear();
    saved_players().lock().unwrap_or_else(|e| e.into_inner()).clear();
    inv_store().lock().unwrap_or_else(|e| e.into_inner()).clear();
    pos_store().lock().unwrap_or_else(|e| e.into_inner()).clear();
    chests().lock().unwrap_or_else(|e| e.into_inner()).clear();
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
        saved_players().lock().unwrap_or_else(|e| e.into_inner()).get(&player_id),
        Some(&expected)
    );
    assert_eq!(saved_players().lock().unwrap_or_else(|e| e.into_inner())[&other_id].health, 20.0);
    assert_eq!(inv_store().lock().unwrap_or_else(|e| e.into_inner())["resume-player"], slots);
    assert_eq!(
        pos_store().lock().unwrap_or_else(|e| e.into_inner())["resume-player"],
        expected.position
    );
    assert_eq!(
        chests().lock().unwrap_or_else(|e| e.into_inner())[&ender_container_key(&player_id)],
        ender
    );

    let mut offline = Vec::new();
    save_world_into(&mut offline);
    saved_players().lock().unwrap_or_else(|e| e.into_inner()).clear();
    load_world_from(&offline).unwrap();
    assert_eq!(
        saved_players().lock().unwrap_or_else(|e| e.into_inner()).get(&player_id),
        Some(&expected)
    );
    assert!(saved_players().lock().unwrap_or_else(|e| e.into_inner()).contains_key(&other_id));
}

/// ORACLE-PARITY-MATRIX-1 against vanilla 1.21.1 `59353fb4`: item data
/// components are part of the stack, not decoration. A named, enchanted,
/// lores diamond sword must come back from `save_world_into` /
/// `load_world_from` with byte-identical component blobs in the live-player
/// record, the offline inventory store, the ender chest and a block container.
#[test]
fn save_v2_item_components_survive_restart_in_player_and_chest() {
    if test_support::isolated_world_test() {
        return;
    }
    let _guard = world_guard();
    let old_inv = std::mem::take(&mut *inv_store().lock().unwrap_or_else(|e| e.into_inner()));
    let old_chests = std::mem::take(&mut *chests().lock().unwrap_or_else(|e| e.into_inner()));
    let old_players = std::mem::take(&mut *saved_players().lock().unwrap_or_else(|e| e.into_inner()));
    let old_pos = std::mem::take(&mut *pos_store().lock().unwrap_or_else(|e| e.into_inner()));
    let old_clients = std::mem::take(&mut *clients().lock().unwrap_or_else(|e| e.into_inner()));
    let old_world = world().take_all();
    let old_rng = world_rng::saved_state();

    let sword = name_to_item("diamond_sword").unwrap();
    let mut comps: Comps = Vec::new();
    comp_set(&mut comps, COMP_CUSTOM_NAME, nbt_string("§6Excalibur"));
    comp_set(&mut comps, COMP_ENCHANTMENTS, enchantments_data(&[(32, 5)], true));
    {
        let mut lore = Vec::new();
        write_varint(&mut lore, 2);
        lore.extend_from_slice(&nbt_string("first lore line"));
        lore.extend_from_slice(&nbt_string("second lore line"));
        comp_set(&mut comps, COMP_LORE, lore);
    }
    let expected: Stack = (sword, 1, comps);

    let (mut client, _peer) = effect_tests::client_pair();
    client.uuid = [21; 16];
    client.name = "component-player".into();
    client
        .inv
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(36, expected.clone());
    let ender_key = ender_container_key(&client.uuid);
    let ender_slots: HashMap<i16, Stack> = [(3i16, expected.clone())].into_iter().collect();
    chests()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(ender_key, ender_slots.clone());
    let chest_key = ContainerKey::Block((DimId::Overworld, 5, 70, 7));
    let chest_slots: HashMap<i16, Stack> = [(0i16, expected.clone())].into_iter().collect();
    chests()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(chest_key, chest_slots.clone());
    clients()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push(std::sync::Arc::new(client));

    let mut payload = Vec::new();
    save_world_into(&mut payload);
    clients().lock().unwrap_or_else(|e| e.into_inner()).clear();
    inv_store().lock().unwrap_or_else(|e| e.into_inner()).clear();
    chests().lock().unwrap_or_else(|e| e.into_inner()).clear();
    saved_players().lock().unwrap_or_else(|e| e.into_inner()).clear();
    pos_store().lock().unwrap_or_else(|e| e.into_inner()).clear();
    load_world_from(&payload).unwrap();

    let player_id = [21u8; 16];
    let players = saved_players().lock().unwrap_or_else(|e| e.into_inner());
    assert_eq!(
        players.get(&player_id).and_then(|p| p.inventory.get(&36)),
        Some(&expected),
        "the SVP1 player record must keep the stack's component blobs"
    );
    assert_eq!(
        players.get(&player_id).and_then(|p| p.ender_chest.get(&3)),
        Some(&expected),
        "the ender chest record must keep the stack's component blobs"
    );
    drop(players);
    assert_eq!(
        inv_store().lock().unwrap_or_else(|e| e.into_inner())["component-player"].get(&36),
        Some(&expected),
        "the offline inventory store must keep the stack's component blobs"
    );
    let ch = chests().lock().unwrap_or_else(|e| e.into_inner());
    assert_eq!(
        ch.get(&chest_key).and_then(|m| m.get(&0)),
        Some(&expected),
        "a block container must keep the stack's component blobs"
    );
    assert_eq!(
        ch.get(&ender_key),
        Some(&ender_slots),
        "the ender chest container must keep the stack's component blobs"
    );
    drop(ch);

    world_rng::restore(old_rng);
    world().restore_all(old_world);
    *clients().lock().unwrap_or_else(|e| e.into_inner()) = old_clients;
    *inv_store().lock().unwrap_or_else(|e| e.into_inner()) = old_inv;
    *chests().lock().unwrap_or_else(|e| e.into_inner()) = old_chests;
    *saved_players().lock().unwrap_or_else(|e| e.into_inner()) = old_players;
    *pos_store().lock().unwrap_or_else(|e| e.into_inner()) = old_pos;
}

fn sign_meta_frame(payload: &[u8]) -> std::ops::Range<usize> {
    let at = payload
        .windows(SIGN_META_MAGIC.len())
        .position(|w| w == SIGN_META_MAGIC)
        .expect("an SGN1 footer");
    let size_at = at - 8;
    let size = u64::from_be_bytes(payload[size_at..size_at + 8].try_into().unwrap()) as usize;
    size_at - size..at + SIGN_META_MAGIC.len()
}

fn with_sign_meta_body(payload: &[u8], body: &[u8]) -> Vec<u8> {
    let frame = sign_meta_frame(payload);
    let mut edited = payload[..frame.start].to_vec();
    edited.extend_from_slice(body);
    edited.extend_from_slice(&(body.len() as u64).to_be_bytes());
    edited.extend_from_slice(SIGN_META_MAGIC);
    edited.extend_from_slice(&payload[frame.end..]);
    edited
}

fn sign_side(color: &str, glow: bool) -> SignSideMeta {
    SignSideMeta {
        color: color.to_string(),
        has_glowing_text: glow,
    }
}

/// CREATIVE-P02-SALVAGE-1 gave signs live dye/glow/wax state that vanilla keeps
/// in the sign block entity across restarts. SGN1 persists exactly the records
/// that differ from the defaults, in every dimension, and restores them
/// byte-for-byte; a sign nobody dressed emits no footer at all.
#[test]
fn save_v2_sign_meta_round_trips_dye_glow_and_wax_per_dimension() {
    if test_support::isolated_world_test() {
        return;
    }
    let _guard = world_guard();
    let old_meta = std::mem::take(&mut *sign_meta().lock().unwrap_or_else(|e| e.into_inner()));
    let old_rng = world_rng::saved_state();

    // An empty store must not emit the footer: the save stays byte-identical
    // to a world that never heard of sign metadata.
    let mut plain = Vec::new();
    save_world_into(&mut plain);
    assert!(
        !plain
            .windows(SIGN_META_MAGIC.len())
            .any(|w| w == SIGN_META_MAGIC),
        "default sign state must not produce an SGN1 footer"
    );

    let cell = (5, 100, 7);
    let dressed = SignBlockMeta {
        front: sign_side("red", true),
        back: sign_side("blue", false),
        is_waxed: true,
    };
    let glowing = SignBlockMeta {
        front: sign_side("black", true),
        back: sign_side("light_blue", false),
        is_waxed: false,
    };
    // Nether entry sits at vanilla defaults on both sides and unwaxed: it must
    // not reach the footer even though a record exists for the position.
    let default = SignBlockMeta::default();
    {
        let mut meta = sign_meta().lock().unwrap_or_else(|e| e.into_inner());
        meta.insert((DimId::Overworld, cell.0, cell.1, cell.2), dressed.clone());
        meta.insert((DimId::Nether, cell.0, cell.1, cell.2), default);
        meta.insert((DimId::End, 9, 64, 9), glowing.clone());
    }

    let mut payload = Vec::new();
    save_world_into(&mut payload);
    let mut second = Vec::new();
    save_world_into(&mut second);
    assert_eq!(payload, second, "sign metadata output must be deterministic");
    assert!(
        payload
            .windows(SIGN_META_MAGIC.len())
            .any(|w| w == SIGN_META_MAGIC),
        "a dressed sign must produce an SGN1 footer"
    );

    sign_meta().lock().unwrap_or_else(|e| e.into_inner()).clear();
    load_world_from(&payload).unwrap();
    let meta = sign_meta().lock().unwrap_or_else(|e| e.into_inner());
    let ow = meta.get(&(DimId::Overworld, cell.0, cell.1, cell.2)).expect("overworld sign meta");
    assert_eq!(ow.front.color, "red");
    assert!(ow.front.has_glowing_text);
    assert_eq!(ow.back.color, "blue");
    assert!(!ow.back.has_glowing_text);
    assert!(ow.is_waxed);
    let end = meta.get(&(DimId::End, 9, 64, 9)).expect("end sign meta");
    assert_eq!(end.front.color, "black");
    assert!(end.front.has_glowing_text);
    assert_eq!(end.back.color, "light_blue");
    assert!(!end.back.has_glowing_text);
    assert!(!end.is_waxed);
    assert!(
        !meta.contains_key(&(DimId::Nether, cell.0, cell.1, cell.2)),
        "a default record must not be persisted"
    );
    drop(meta);

    world_rng::restore(old_rng);
    *sign_meta().lock().unwrap_or_else(|e| e.into_inner()) = old_meta;
}

/// A torn or hostile SGN1 footer must fail the whole load closed rather than
/// restore a partially decoded sign.
#[test]
fn save_v2_sign_meta_rejects_corrupt_footers() {
    if test_support::isolated_world_test() {
        return;
    }
    let _guard = world_guard();
    let old_meta = std::mem::take(&mut *sign_meta().lock().unwrap_or_else(|e| e.into_inner()));
    let old_rng = world_rng::saved_state();

    sign_meta()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(
            (DimId::Overworld, 5, 100, 7),
            SignBlockMeta {
                front: sign_side("red", true),
                back: sign_side("blue", false),
                is_waxed: true,
            },
        );
    let mut payload = Vec::new();
    save_world_into(&mut payload);
    let frame = sign_meta_frame(&payload);
    let body = &payload[frame.start..frame.end - SIGN_META_FOOTER_LEN];

    let mut corruptions: Vec<(&str, Vec<u8>)> = Vec::new();
    // Honest-length truncations at every prefix of the body.
    for cut in 0..body.len() {
        corruptions.push(("truncated body", body[..cut].to_vec()));
    }
    // A body one byte longer than the record claims.
    let mut trailing = body.to_vec();
    trailing.push(0);
    corruptions.push(("trailing byte", trailing));
    // A schema this reader does not know.
    let mut schema = body.to_vec();
    schema[0] = 9;
    corruptions.push(("unknown schema", schema));
    // An impossible entry count.
    let mut count = body.to_vec();
    count[1..5].copy_from_slice(&u32::MAX.to_be_bytes());
    corruptions.push(("excessive count", count));
    // A colour outside the sixteen vanilla dye names.
    let dye_at = body
        .windows(3)
        .position(|w| w == b"red")
        .expect("the red colour in the record");
    let mut colour = body.to_vec();
    colour[dye_at..dye_at + 3].copy_from_slice(b"xxx");
    corruptions.push(("unknown colour", colour));
    // A glow flag that is neither 0 nor 1: it sits right after "red".
    let mut flag = body.to_vec();
    flag[dye_at + 3] = 2;
    corruptions.push(("bad glow flag", flag));

    for (reason, edited_body) in corruptions {
        let edited = with_sign_meta_body(&payload, &edited_body);
        assert!(load_world_from(&edited).is_err(), "{reason}");
    }

    world_rng::restore(old_rng);
    *sign_meta().lock().unwrap_or_else(|e| e.into_inner()) = old_meta;
}

#[test]
fn save_v2_world_spawn_keeps_packet_angle_and_old_footer_order() {
    if test_support::isolated_world_test() {
        return;
    }
    let world_spawn = (-10.5, 72.0, 5.5, 113.25);
    *WORLD_SPAWN.lock().unwrap_or_else(|e| e.into_inner()) = world_spawn;
    *WORLD_SPAWN_PITCH.lock().unwrap_or_else(|e| e.into_inner()) = -18.5;
    WORLD_SEED.store(56412, Ordering::Relaxed);
    world_border().lock().unwrap_or_else(|e| e.into_inner()).center_z = 14.25;
    {
        let mut board = scoreboard().lock().unwrap_or_else(|e| e.into_inner());
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
        saved_players().lock().unwrap_or_else(|e| e.into_inner()).insert(uuid, state);
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
        *WORLD_SPAWN.lock().unwrap_or_else(|e| e.into_inner()) = (0.0, 0.0, 0.0, 0.0);
        *WORLD_SPAWN_PITCH.lock().unwrap_or_else(|e| e.into_inner()) = 99.0;
        *world_border().lock().unwrap_or_else(|e| e.into_inner()) = WorldBorderState::default();
        *scoreboard().lock().unwrap_or_else(|e| e.into_inner()) = ScoreboardEngine::default();
        load_world_from(bytes).unwrap();
        assert_eq!(saved_value(&WORLD_SPAWN), world_spawn);
        assert_eq!(saved_value(&WORLD_SPAWN_PITCH), pitch);
        assert_eq!(world_border().lock().unwrap_or_else(|e| e.into_inner()).center_z, 14.25);
        let board = scoreboard().lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(board.display(ScoreboardDisplaySlot::Sidebar), Some("saved"));
        drop(board);
        let packet = spawn_position_packet(saved_value(&WORLD_SPAWN));
        assert_eq!(read_pos(&packet[..8]), (-11, 72, 5));
        assert_eq!(&packet[8..], &world_spawn.3.to_be_bytes());
        let players = saved_players().lock().unwrap_or_else(|e| e.into_inner());
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
    let original = mobs().lock().unwrap_or_else(|e| e.into_inner()).clone();
    let [wolf, merchant, witch] = original.as_slice() else {
        unreachable!()
    };
    {
        let mut states = live_mob_states().lock().unwrap_or_else(|e| e.into_inner());
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
    *wolf.vel.lock().unwrap_or_else(|e| e.into_inner()) = [0.25, -0.125, 0.5];
    *wolf.fire_ticks.lock().unwrap_or_else(|e| e.into_inner()) = 45;
    *wolf.fuse.lock().unwrap_or_else(|e| e.into_inner()) = 7;
    *wolf.love.lock().unwrap_or_else(|e| e.into_inner()) = 22;
    *wolf.air.lock().unwrap_or_else(|e| e.into_inner()) = 199;
    sheared().lock().unwrap_or_else(|e| e.into_inner()).insert(witch.eid);
    attrs().lock().unwrap_or_else(|e| e.into_inner()).insert(format!("e:{}", wolf.eid),
        [("generic.max_health".into(), 43.5), ("custom.motion".into(), -0.25)].into());
    attrs().lock().unwrap_or_else(|e| e.into_inner()).insert(format!("e:{}", merchant.eid),
        [("generic.movement_speed".into(), 0.35)].into());
    attrs().lock().unwrap_or_else(|e| e.into_inner()).insert(format!("e:m:{}", wolf.eid),
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
    saved_raids().lock().unwrap_or_else(|e| e.into_inner()).push((DimId::Overworld, raid));

    let mut payload = Vec::new();
    save_world_into(&mut payload);
    mobs().lock().unwrap_or_else(|e| e.into_inner()).clear();
    live_mob_states().lock().unwrap_or_else(|e| e.into_inner()).clear();
    live_villagers().lock().unwrap_or_else(|e| e.into_inner()).clear();
    saved_raids().lock().unwrap_or_else(|e| e.into_inner()).clear();
    sheared().lock().unwrap_or_else(|e| e.into_inner()).clear();
    tags().lock().unwrap_or_else(|e| e.into_inner()).clear();
    TICK.store(900, Ordering::Relaxed);
    load_world_from(&payload).unwrap();

    let restored = mobs().lock().unwrap_or_else(|e| e.into_inner()).clone();
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
    assert!(sheared().lock().unwrap_or_else(|e| e.into_inner()).contains(&saved_witch.eid));
    {
        let attributes = attrs().lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(attributes[&format!("e:{}", saved_wolf.eid)]["generic.max_health"], 43.5);
        assert_eq!(attributes[&format!("e:{}", saved_wolf.eid)]["custom.motion"], -0.25);
        assert_eq!(attributes[&format!("e:{}", saved_merchant.eid)]["generic.movement_speed"], 0.35);
        assert_eq!(attributes[&format!("e:m:{}", saved_wolf.eid)]["generic.armor"], 12.0);
        assert!(!attributes.contains_key(&format!("e:{}", wolf.eid)));
        assert!(!attributes.contains_key(&format!("e:m:{}", wolf.eid)));
    }
    assert!(tags().lock().unwrap_or_else(|e| e.into_inner())[&format!("e:{}", saved_merchant.eid)].contains("trader"));
    {
        let states = live_mob_states().lock().unwrap_or_else(|e| e.into_inner());
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
        let villagers = live_villagers().lock().unwrap_or_else(|e| e.into_inner());
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
    let mut raids = saved_raids().lock().unwrap_or_else(|e| e.into_inner());
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
        assert!(world().try_write_all().is_some());
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

    world().clear();
    load_world_in(&dir).unwrap();
    assert_eq!(overlay_block(2, 64, 3), Some(4));
    set_block(2, 64, 3, 8);
    save_world_guarded_in(&dir);
    assert!(!temp.exists());
    assert_eq!(
        std::fs::read(dir.join(format!("{WORLD_FILE}.bak"))).unwrap(),
        original
    );
    world().clear();
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
    *WORLD_SPAWN_PITCH.lock().unwrap_or_else(|e| e.into_inner()) = 15.0;
    let mut payload = Vec::new();
    save_world_into(&mut payload);
    let frame = full_state_frame(&payload);
    payload[frame.start..frame.start + 4].copy_from_slice(&f32::NAN.to_be_bytes());
    world().clear();
    assert_eq!(load_world_from(&payload), Err("non-finite save float"));
    assert_eq!(overlay_block(2, 64, 3), None);
    assert_eq!(saved_value(&WORLD_SPAWN_PITCH), 15.0);

    // `saved_value` is poison-tolerant, so a poisoned spawn-pitch mutex no
    // longer stands in for a capture panic: capture reads right through it.
    // Trip a real encode assert inside `save_world_into` instead — the
    // advancement footer refuses more than ADV_MAX_PLAYERS grants, and the
    // panic must be contained by the capture boundary before the writer runs.
    let old_grants = std::mem::take(&mut *adv_done().lock().unwrap_or_else(|e| e.into_inner()));
    {
        let mut grants = adv_done().lock().unwrap_or_else(|e| e.into_inner());
        for i in 0..=ADV_MAX_PLAYERS {
            grants.insert(format!("boundary-{i}"), [format!("adv-{i}")].into_iter().collect());
        }
    }
    assert_eq!(
        save_world_guarded(|_| panic!("writer was called")),
        Err("world snapshot panicked".into())
    );
    // The boundary released the ownership guard and the writer never ran.
    assert!(SAVE_LOCK.try_lock().is_ok());
    assert!(SAVE_WRITER.try_lock().is_ok());
    *adv_done().lock().unwrap_or_else(|e| e.into_inner()) = old_grants;
    save_world_guarded(|_| Ok(())).unwrap();

    // A panic while flushing the snapshot is contained by the same boundary:
    // it reports the write stage instead of escaping into the tick loop.
    assert_eq!(
        save_world_guarded(|_| panic!("writer panicked")),
        Err("world save write panicked".into())
    );
    assert!(SAVE_LOCK.try_lock().is_ok());
    assert!(SAVE_WRITER.try_lock().is_ok());
    save_world_guarded(|_| Ok(())).unwrap();

    // Poison tolerance is respected on both sides: a mutex poisoned by a
    // panicking stranger neither fails the save nor turns the load boundary
    // into a panic report.
    assert!(thread::spawn(|| {
        let _pitch = WORLD_SPAWN_PITCH.lock().unwrap_or_else(|e| e.into_inner());
        panic!("pitch fixture");
    })
    .join()
    .is_err());
    save_world_guarded(|_| Ok(())).unwrap();
    WORLD_SPAWN_PITCH.clear_poison();

    assert!(thread::spawn(|| {
        let _world = world().write_all();
        panic!("loader fixture");
    })
    .join()
    .is_err());
    assert_ne!(load_world_from(&[0]), Err("world load panicked"));
    world().clear_poison();
    load_world_from(&[0]).unwrap();
}

#[test]
fn save_v2_rejects_a_truncated_companion_before_publishing_any_of_its_edits() {
    if test_support::isolated_world_test() {
        return;
    }
    let mut payload = Vec::new();
    world().insert((DimId::Nether, 0, 0), [(3, 501)].into());
    assert_eq!(save_dim_overlay_into(DimId::Nether, &mut payload), 1);
    world().clear();
    for cut in 0..payload.len() {
        assert!(
            load_dim_overlay_from(DimId::Nether, &payload[..cut]).is_err(),
            "cut={cut}"
        );
        assert!(world().is_empty());
    }
    load_dim_overlay_from(DimId::Nether, &payload).unwrap();
    assert_eq!(world().read(&(DimId::Nether, 0, 0))[&(DimId::Nether, 0, 0)][&3], 501);
}

#[cfg(megu_vanilla_data)]
#[test]
fn save_v2_live_block_entities_round_trip_all_families() {
    test_support::require_vanilla_data();
    if test_support::isolated_world_test() {
        return;
    }
    let _world_guard = world_guard();
    let old_world = world().take_all();
    let old_store = std::mem::take(&mut *live_block_entities().lock().unwrap_or_else(|e| e.into_inner()));
    let old_drops = std::mem::take(&mut *drops().lock().unwrap_or_else(|e| e.into_inner()));
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

    live_block_entities().lock().unwrap_or_else(|e| e.into_inner()).entries.clear();
    load_world_from(&payload).unwrap();
    let restored: Vec<_> = cases
        .iter()
        .enumerate()
        .map(|(i, _)| {
            let key = (DimId::Overworld, 5_000 + i as i32, 220, 5_000);
            (key, live_block_entity_snapshot(key))
        })
        .collect();

    world().restore_all(old_world);
    *live_block_entities().lock().unwrap_or_else(|e| e.into_inner()) = old_store;
    *drops().lock().unwrap_or_else(|e| e.into_inner()) = old_drops;
    world_rng::restore(old_rng);

    assert_eq!(restored, expected.into_iter().map(|(k, s)| (k, Some(s))).collect::<Vec<_>>());
}

#[test]
fn save_v2_autosave_preserves_bed_and_anchor_spawn_target_kinds() {
    if test_support::isolated_world_test() {
        return;
    }
    let _world_guard = world_guard();
    let old_players = std::mem::take(&mut *saved_players().lock().unwrap_or_else(|e| e.into_inner()));
    let old_clients = std::mem::take(&mut *clients().lock().unwrap_or_else(|e| e.into_inner()));

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
        *client.spawn_dim.lock().unwrap_or_else(|e| e.into_inner()) = target.dim;
        *client.spawn_pt.lock().unwrap_or_else(|e| e.into_inner()) =
            Some((target.position.0, target.position.1, target.position.2, target.angle));
        let mut state =
            PlayerSave::capture(&client, client_dim_pos(&client), &HashMap::new());
        state.respawn = Some(target);
        saved_players().lock().unwrap_or_else(|e| e.into_inner()).insert(uuid, state);
        clients().lock().unwrap_or_else(|e| e.into_inner()).push(Arc::new(client));
    }

    // The autosave recaptures every live client from spawn_pt, which carries no
    // kind. The saved bed/anchor records must survive that recapture unchanged.
    let mut payload = Vec::new();
    save_world_into(&mut payload);
    let after_save = {
        let saved = saved_players().lock().unwrap_or_else(|e| e.into_inner());
        (saved[&[7; 16]].respawn, saved[&[9; 16]].respawn)
    };
    saved_players().lock().unwrap_or_else(|e| e.into_inner()).clear();
    load_world_from(&payload).unwrap();
    let after_load = {
        let saved = saved_players().lock().unwrap_or_else(|e| e.into_inner());
        (saved[&[7; 16]].respawn, saved[&[9; 16]].respawn)
    };

    // A moved spawn point has no saved record to match and stays Legacy.
    *clients().lock().unwrap_or_else(|e| e.into_inner())[0].spawn_pt.lock().unwrap_or_else(|e| e.into_inner()) =
        Some((1.5, 60.0, 1.5, 0.0));
    let mut second = Vec::new();
    save_world_into(&mut second);
    let moved = saved_players().lock().unwrap_or_else(|e| e.into_inner())[&[7; 16]].respawn;

    clients().lock().unwrap_or_else(|e| e.into_inner()).clear();
    *saved_players().lock().unwrap_or_else(|e| e.into_inner()) = old_players;
    *clients().lock().unwrap_or_else(|e| e.into_inner()) = old_clients;

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

// ---------- Wave M3: MOF1 / AER1 mod-bridge trailers ----------

#[cfg(feature = "create-offload")]
#[test]
fn mega_w3_save_v2_roundtrips_mof1_and_aer1_while_empty_vanilla_stays_identical() {
    if test_support::isolated_world_test() {
        return;
    }
    let _guard = world_guard();

    // ---- 1. Empty bridges: zero extra bytes, deterministic byte-identical saves. ----
    {
        *mod_offload_bridge().lock().unwrap_or_else(|e| e.into_inner()) =
            ModOffloadBridge::new();
        *aeronautics_bridge().lock().unwrap_or_else(|e| e.into_inner()) =
            AeronauticsRuntimeBridge::new();
        *dimension_mod_bridge().lock().unwrap_or_else(|e| e.into_inner()) =
            DimensionModRuntimeBridge::new();
    }
    let mut empty_a = Vec::new();
    save_world_into(&mut empty_a);
    let mut empty_b = Vec::new();
    save_world_into(&mut empty_b);
    assert_eq!(empty_a, empty_b, "empty vanilla save must be deterministic");
    {
        let (rest, _) = read_world_seed_footer(&empty_a).unwrap();
        let (rest, aer) = split_save_footer(rest, b"AER1").unwrap();
        assert!(aer.is_none(), "empty aeronautics bridge must emit no AER1");
        let (_, mof) = split_save_footer(rest, b"MOF1").unwrap();
        assert!(mof.is_none(), "empty offload bridge must emit no MOF1");
    }

    // ---- 2. Populate both bridges and save: both trailers must ride the tail. ----
    {
        let mut bridge = mod_offload_bridge().lock().unwrap_or_else(|e| e.into_inner());
        let reg = create_adapter::blockstates::core_create_registry(
            mod_adapter_core::blockstates::DEFAULT_BASE_STATE_ID,
        );
        let shaft = reg.default_state("create:shaft").unwrap();
        bridge.place_create_block(
            create_adapter::factory::DimBlockPos::new(0, 100, 64, 100),
            shaft,
        );
        let mut blocks = BTreeMap::new();
        blocks.insert([300, 64, 300], shaft);
        bridge
            .assemble_contraption(
                DimId::Overworld,
                [300, 64, 300],
                [300, 64, 300],
                &blocks,
                create_adapter::contraption_adapter::ContraptionKind::Linear {
                    velocity_per_tick: [0.0, 0.0, 0.0],
                },
            )
            .unwrap();
    }
    {
        let mut b = aeronautics_bridge().lock().unwrap_or_else(|e| e.into_inner());
        b.ensure_registry().unwrap();
        let reg = b.registry.as_ref().unwrap().clone();
        let st = reg.default_state("create:shaft").unwrap();
        let canon = reg.encode_canonical(st).unwrap().to_string();
        let mut cells = BTreeMap::new();
        cells.insert([0, 0, 0], canon);
        b.assemble_airship(3, 0, cells, None, create_adapter::aeronautics::DEFAULT_BLOCK_MASS)
            .unwrap();
    }
    {
        let mut b = dimension_mod_bridge().lock().unwrap_or_else(|e| e.into_inner());
        b.registered_dims
            .insert("testdim:moon".to_string(), DimId::from_raw(9));
        b.env_by_name.insert(
            "testdim:moon".to_string(),
            mod_adapter_core::dimension_mod::DimensionEnvProfile {
                dimension_id: "testdim:moon".to_string(),
                gravity_multiplier: 0.5,
                requires_oxygen: true,
                has_skylight: false,
                has_ceiling: true,
                supports_underground_occlusion: false,
                sky_effects: "testdim:sky".to_string(),
            },
        );
    }
    assert!(mod_offload_has_state());
    assert!(aeronautics_bridge_has_state());
    let mut payload = Vec::new();
    save_world_into(&mut payload);
    let mut second = Vec::new();
    save_world_into(&mut second);
    assert_eq!(payload, second, "populated save must be deterministic");

    // Both trailers must be present, AER1 outermost (written last before WBD1).
    let mof_magic_at = payload
        .windows(4)
        .rposition(|w| w == b"MOF1")
        .expect("MOF1 trailer must be emitted");
    let aer_magic_at = payload
        .windows(4)
        .rposition(|w| w == b"AER1")
        .expect("AER1 trailer must be emitted");
    assert!(aer_magic_at > mof_magic_at, "AER1 wraps MOF1 in write order");
    let (rest, _) = read_world_seed_footer(&payload).unwrap();
    let (rest, aer_body) = read_mod_bridge_trailer(rest, b"AER1").unwrap();
    let aer_body = aer_body.expect("AER1 trailer present").to_vec();
    let (_, mof_body) = read_mod_bridge_trailer(rest, b"MOF1").unwrap();
    let mof_body = mof_body.expect("MOF1 trailer present").to_vec();

    // The trailer bodies alone must feed the bridge codecs: clear, decode,
    // confirm, and clear again for the whole-payload path below.
    {
        *mod_offload_bridge().lock().unwrap_or_else(|e| e.into_inner()) =
            ModOffloadBridge::new();
        *aeronautics_bridge().lock().unwrap_or_else(|e| e.into_inner()) =
            AeronauticsRuntimeBridge::new();
        *dimension_mod_bridge().lock().unwrap_or_else(|e| e.into_inner()) =
            DimensionModRuntimeBridge::new();
    }
    assert!(mod_offload_load_bytes(&mof_body), "MOF1 body must decode");
    assert!(aeronautics_state_load_bytes(&aer_body), "AER1 body must decode");
    {
        let bridge = mod_offload_bridge().lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(bridge.placed_create.len(), 1);
        assert_eq!(bridge.offload.assembly_count(), 1);
    }
    {
        let b = dimension_mod_bridge().lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(
            b.env_by_name["testdim:moon"].gravity_multiplier,
            0.5,
            "env record survives the codec even with no live DimId"
        );
    }

    // ---- 3. Reload into cleared bridges: the whole state comes back. ----
    {
        *mod_offload_bridge().lock().unwrap_or_else(|e| e.into_inner()) =
            ModOffloadBridge::new();
        *aeronautics_bridge().lock().unwrap_or_else(|e| e.into_inner()) =
            AeronauticsRuntimeBridge::new();
        *dimension_mod_bridge().lock().unwrap_or_else(|e| e.into_inner()) =
            DimensionModRuntimeBridge::new();
    }
    load_world_from(&payload).unwrap();
    {
        let bridge = mod_offload_bridge().lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(bridge.placed_create.len(), 1, "placed Create block restored");
        assert_eq!(bridge.offload.assembly_count(), 1, "contraption restored");
    }
    {
        let b = aeronautics_bridge().lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(b.airships.len(), 1, "airship restored");
        assert!(b.airships.contains_key(&3));
    }
    {
        let b = dimension_mod_bridge().lock().unwrap_or_else(|e| e.into_inner());
        let env = b
            .env_by_name
            .get("testdim:moon")
            .expect("dim env profile restored");
        assert_eq!(env.gravity_multiplier, 0.5);
        assert!(env.requires_oxygen);
    }

    // ---- 4. Corrupted/truncated trailers fail closed. ----
    let reset_bridges = || {
        *mod_offload_bridge().lock().unwrap_or_else(|e| e.into_inner()) =
            ModOffloadBridge::new();
        *aeronautics_bridge().lock().unwrap_or_else(|e| e.into_inner()) =
            AeronauticsRuntimeBridge::new();
        *dimension_mod_bridge().lock().unwrap_or_else(|e| e.into_inner()) =
            DimensionModRuntimeBridge::new();
    };

    // A flipped byte inside the MOF1 body trips the CRC — the load fails.
    let mut corrupt_mof = payload.clone();
    corrupt_mof[mof_magic_at - 9] ^= 0xFF;
    reset_bridges();
    assert!(
        load_world_from(&corrupt_mof).is_err(),
        "corrupt MOF1 body must fail closed"
    );
    assert!(!mod_offload_has_state(), "rejected trailer left bridge state");

    // A corrupted trailer size field is rejected by the footer framing itself.
    let mut corrupt_size = payload.clone();
    corrupt_size[mof_magic_at - 1] ^= 0xFF;
    reset_bridges();
    assert!(
        load_world_from(&corrupt_size).is_err(),
        "corrupt trailer size must fail closed"
    );

    // A flipped byte inside the AER1 body trips its CRC the same way.
    let mut corrupt_aer = payload.clone();
    corrupt_aer[aer_magic_at - 9] ^= 0xFF;
    reset_bridges();
    assert!(
        load_world_from(&corrupt_aer).is_err(),
        "corrupt AER1 body must fail closed"
    );

    // A corrupted AER1 size field is rejected by the footer framing.
    let mut corrupt_aer_size = payload.clone();
    corrupt_aer_size[aer_magic_at - 1] ^= 0xFF;
    reset_bridges();
    assert!(
        load_world_from(&corrupt_aer_size).is_err(),
        "corrupt AER1 size must fail closed"
    );

    // A schema byte the reader does not know is rejected before the inner
    // body is even length-checked.
    let aer_body_size = u64::from_be_bytes(
        payload[aer_magic_at - 8..aer_magic_at].try_into().unwrap(),
    ) as usize;
    let mut corrupt_schema = payload.clone();
    corrupt_schema[aer_magic_at - 8 - aer_body_size] ^= 0xFF;
    reset_bridges();
    assert!(
        load_world_from(&corrupt_schema).is_err(),
        "unknown trailer schema must fail closed"
    );

    // The untouched payload still loads cleanly after every rejection.
    reset_bridges();
    load_world_from(&payload).unwrap();
    assert!(mod_offload_has_state());
    assert!(aeronautics_bridge_has_state());

    // Clean up the live bridges so no later test inherits mod state.
    reset_bridges();
}

// ---------- RCP1 / STA1 per-player gameplay state ----------

fn player_section_frame(body: &[u8], magic: &[u8; 4]) -> Vec<u8> {
    let mut out = body.to_vec();
    out.extend_from_slice(&(body.len() as u64).to_be_bytes());
    out.extend_from_slice(magic);
    out
}

fn strip_player_section_tail(payload: &[u8]) -> &[u8] {
    // Peel the trailers that can sit above RCP1/STA1 in a vanilla test save:
    // SED1 (with VER1/WBD1 beneath it), then AER1 and MOF1 last-written-first.
    let (payload, _) = read_world_seed_footer(payload).unwrap();
    let (payload, _) = read_mod_bridge_trailer(payload, AERONAUTICS_TRAILER_MAGIC).unwrap();
    let (payload, _) = read_mod_bridge_trailer(payload, MOD_OFFLOAD_TRAILER_MAGIC).unwrap();
    payload
}

fn sample_recipe_book() -> PlayerRecipeBook {
    let mut book = PlayerRecipeBook::new();
    assert!(book.unlock("minecraft:crafting_table"));
    assert!(book.unlock("minecraft:stick"));
    book.seen_recipe("minecraft:stick");
    book.apply_book_settings(RecipeBookCategory::Crafting, true, true);
    book.apply_book_settings(RecipeBookCategory::Smoker, false, true);
    book
}

fn sample_player_stats() -> PlayerStats {
    let mut stats = PlayerStats::new();
    stats.increment(StatCategory::Mined, 1, 5);
    stats.increment(StatCategory::Mined, 1, -2);
    stats.increment(StatCategory::Custom, 42, i32::MAX);
    stats.increment(StatCategory::Killed, -7, 3);
    stats
}

#[test]
fn save_v2_recipe_books_and_player_stats_round_trip_bit_for_bit() {
    if test_support::isolated_world_test() {
        return;
    }
    let _guard = world_guard();
    let old_books = std::mem::take(&mut *recipe_books().lock().unwrap_or_else(|e| e.into_inner()));
    let old_stats = std::mem::take(&mut *player_stats().lock().unwrap_or_else(|e| e.into_inner()));

    // With both registries empty the save carries neither footer — the same
    // tail a build predating RCP1/STA1 would write.
    let mut legacy = Vec::new();
    save_world_into(&mut legacy);
    let tail = strip_player_section_tail(&legacy);
    let (tail, saved_stats) = read_player_stats_footer(tail).unwrap();
    assert!(saved_stats.is_empty());
    let (_, saved_books) = read_recipe_book_footer(tail).unwrap();
    assert!(saved_books.is_empty());

    let uuid_a = [0xA1; 16];
    let uuid_b = [0xB2; 16];
    {
        let mut books = recipe_books().lock().unwrap_or_else(|e| e.into_inner());
        books.insert(uuid_a, sample_recipe_book());
        books.insert(uuid_b, PlayerRecipeBook::new());
    }
    {
        let mut stats = player_stats().lock().unwrap_or_else(|e| e.into_inner());
        stats.insert(uuid_a, sample_player_stats());
        stats.insert(uuid_b, PlayerStats::new());
    }

    let mut payload = Vec::new();
    save_world_into(&mut payload);
    let mut again = Vec::new();
    save_world_into(&mut again);
    assert_eq!(payload, again, "player-section output must be deterministic");

    // STA1 is written after RCP1, so it sits closer to the tail.
    let tail = strip_player_section_tail(&payload);
    assert!(tail.ends_with(PLAYER_STATS_MAGIC));
    let (tail, _) = split_save_footer(tail, PLAYER_STATS_MAGIC).unwrap();
    assert!(tail.ends_with(RECIPE_BOOK_MAGIC));

    let expected_books = recipe_books().lock().unwrap_or_else(|e| e.into_inner()).clone();
    let expected_stats = player_stats().lock().unwrap_or_else(|e| e.into_inner()).clone();

    recipe_books().lock().unwrap_or_else(|e| e.into_inner()).clear();
    player_stats().lock().unwrap_or_else(|e| e.into_inner()).clear();
    load_world_from(&payload).unwrap();
    assert_eq!(
        *recipe_books().lock().unwrap_or_else(|e| e.into_inner()),
        expected_books
    );
    assert_eq!(
        *player_stats().lock().unwrap_or_else(|e| e.into_inner()),
        expected_stats
    );
    let restored = recipe_books().lock().unwrap_or_else(|e| e.into_inner());
    let book = &restored[&uuid_a];
    assert!(book.is_unlocked("minecraft:crafting_table"));
    assert!(book.is_unlocked("minecraft:stick"));
    assert!(!book.displayed.contains("minecraft:stick"));
    assert_eq!(book.to_highlight(), vec!["minecraft:crafting_table".to_string()]);
    assert!(restored[&uuid_b].unlocked.is_empty());
    drop(restored);
    let counters = player_stats().lock().unwrap_or_else(|e| e.into_inner());
    assert_eq!(counters[&uuid_a].get(StatCategory::Mined, 1), 3);
    assert_eq!(counters[&uuid_a].get(StatCategory::Custom, 42), i32::MAX);
    assert_eq!(counters[&uuid_a].get(StatCategory::Killed, -7), 3);
    assert!(counters[&uuid_b].counts.is_empty());
    drop(counters);

    // A second save after the load is byte-identical: restore is lossless.
    let mut resaved = Vec::new();
    save_world_into(&mut resaved);
    assert_eq!(payload, resaved);

    // The legacy blob (no RCP1/STA1) loads cleanly and leaves empty defaults
    // even when the live registries still hold entries.
    recipe_books().lock().unwrap_or_else(|e| e.into_inner())
        .insert(uuid_a, sample_recipe_book());
    player_stats().lock().unwrap_or_else(|e| e.into_inner())
        .insert(uuid_a, sample_player_stats());
    load_world_from(&legacy).unwrap();
    assert!(recipe_books().lock().unwrap_or_else(|e| e.into_inner()).is_empty());
    assert!(player_stats().lock().unwrap_or_else(|e| e.into_inner()).is_empty());

    *recipe_books().lock().unwrap_or_else(|e| e.into_inner()) = old_books;
    *player_stats().lock().unwrap_or_else(|e| e.into_inner()) = old_stats;
}

#[test]
fn save_v2_player_section_footers_fail_closed_on_corruption() {
    let uuid = [0xC3; 16];

    // A minimal well-formed RCP1 body: one player, two unlocked recipes, one
    // highlight, and non-default book settings.
    let mut book_body = vec![PLAYER_SECTION_SCHEMA];
    megu_core::varint::write_varlong_i64(1, &mut book_body);
    book_body.extend_from_slice(&uuid);
    megu_core::varint::write_varlong_i64(2, &mut book_body);
    write_registry_str(&mut book_body, "minecraft:crafting_table");
    write_registry_str(&mut book_body, "minecraft:stick");
    megu_core::varint::write_varlong_i64(1, &mut book_body);
    write_registry_str(&mut book_body, "minecraft:crafting_table");
    book_body.extend_from_slice(&[1, 1, 0, 0, 0, 1, 1, 0]);

    let framed = player_section_frame(&book_body, RECIPE_BOOK_MAGIC);
    let (rest, books) = read_recipe_book_footer(&framed).unwrap();
    assert!(rest.is_empty());
    assert_eq!(books.len(), 1);
    assert!(books[0].1.unlocked.contains("minecraft:stick"));
    assert_eq!(books[0].1.displayed.len(), 1);

    let mut bad_schema = book_body.clone();
    bad_schema[0] = 99;
    let mut trailing = book_body.clone();
    trailing.push(0);
    let mut truncated = book_body.clone();
    truncated.pop();
    // A highlight outside the unlocked set violates the runtime invariant.
    let mut highlight_orphan = book_body.clone();
    // Rewrite displayed count 1 -> keep, but swap the displayed id for one
    // that is not unlocked: rebuild the tail deterministically instead.
    highlight_orphan.clear();
    highlight_orphan.push(PLAYER_SECTION_SCHEMA);
    megu_core::varint::write_varlong_i64(1, &mut highlight_orphan);
    highlight_orphan.extend_from_slice(&uuid);
    megu_core::varint::write_varlong_i64(1, &mut highlight_orphan);
    write_registry_str(&mut highlight_orphan, "minecraft:crafting_table");
    megu_core::varint::write_varlong_i64(1, &mut highlight_orphan);
    write_registry_str(&mut highlight_orphan, "minecraft:furnace");
    highlight_orphan.extend_from_slice(&[0; 8]);
    // Duplicate uuid across two player records.
    let mut dup_uuid = vec![PLAYER_SECTION_SCHEMA];
    megu_core::varint::write_varlong_i64(2, &mut dup_uuid);
    dup_uuid.extend_from_slice(&book_body[2..]);
    dup_uuid.extend_from_slice(&book_body[2..]);
    // Oversized player count: varint far beyond what the body can hold.
    let mut huge_count = vec![PLAYER_SECTION_SCHEMA];
    megu_core::varint::write_varlong_i64(MAX_SAVED_PLAYER_SECTIONS as i64 + 1, &mut huge_count);
    huge_count.extend_from_slice(&book_body[1..]);
    // Duplicate recipe id inside one set.
    let mut dup_recipe = book_body.clone();
    // unlocked count is at offset 17 (16 uuid bytes after schema+count); the
    // two strings follow — append a third identical entry by rebuilding.
    dup_recipe.clear();
    dup_recipe.push(PLAYER_SECTION_SCHEMA);
    megu_core::varint::write_varlong_i64(1, &mut dup_recipe);
    dup_recipe.extend_from_slice(&uuid);
    megu_core::varint::write_varlong_i64(2, &mut dup_recipe);
    write_registry_str(&mut dup_recipe, "minecraft:crafting_table");
    write_registry_str(&mut dup_recipe, "minecraft:crafting_table");
    megu_core::varint::write_varlong_i64(0, &mut dup_recipe);
    dup_recipe.extend_from_slice(&[0; 8]);
    // Overlong registry string (> u16 cap is impossible; > MAX_REGISTRY_STR is).
    let mut long_string = book_body.clone();
    // Locate the first string's u16 length (after schema+count+uuid+count).
    let pos = 1 + 1 + 16 + 1;
    long_string[pos..pos + 2].copy_from_slice(&(MAX_REGISTRY_STR as u16 + 1).to_be_bytes());

    for (reason, body) in [
        ("bad schema", bad_schema),
        ("trailing bytes", trailing),
        ("truncated body", truncated),
        ("highlight outside unlocked", highlight_orphan),
        ("duplicate player uuid", dup_uuid),
        ("oversized player count", huge_count),
        ("duplicate recipe id", dup_recipe),
        ("oversized recipe string", long_string),
    ] {
        let framed = player_section_frame(&body, RECIPE_BOOK_MAGIC);
        assert!(
            read_recipe_book_footer(&framed).is_err(),
            "RCP1 {reason} must fail closed"
        );
    }

    // Minimal well-formed STA1 body: one player, two counters.
    let counters = sample_player_stats();
    let blob = encode_stats(&counters);
    let mut stats_body = vec![PLAYER_SECTION_SCHEMA];
    megu_core::varint::write_varlong_i64(1, &mut stats_body);
    stats_body.extend_from_slice(&uuid);
    megu_core::varint::write_varlong_i64(blob.len() as i64, &mut stats_body);
    stats_body.extend_from_slice(&blob);

    let framed = player_section_frame(&stats_body, PLAYER_STATS_MAGIC);
    let (rest, entries) = read_player_stats_footer(&framed).unwrap();
    assert!(rest.is_empty());
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].1.get(StatCategory::Mined, 1), 3);

    let mut bad_stats_schema = stats_body.clone();
    bad_stats_schema[0] = 0;
    let mut bad_stats_truncated = stats_body.clone();
    bad_stats_truncated.pop();
    let mut bad_stats_trailing = stats_body.clone();
    bad_stats_trailing.push(0);
    // Blob length claiming more bytes than the body holds.
    let mut bad_stats_len = stats_body.clone();
    bad_stats_len.truncate(1 + 1 + 16);
    megu_core::varint::write_varlong_i64(1_000, &mut bad_stats_len);
    bad_stats_len.extend_from_slice(&[0; 20]);
    // A blob whose stat category id is outside the registry.
    let mut bad_blob = vec![PLAYER_SECTION_SCHEMA];
    megu_core::varint::write_varlong_i64(1, &mut bad_blob);
    bad_blob.extend_from_slice(&uuid);
    let mut inner = Vec::new();
    megu_core::varint::write_varlong_i64(1, &mut inner);
    megu_core::varint::write_varlong_i64(99, &mut inner);
    megu_core::varint::write_varlong_i64(1, &mut inner);
    megu_core::varint::write_varlong_i64(1, &mut inner);
    megu_core::varint::write_varlong_i64(inner.len() as i64, &mut bad_blob);
    bad_blob.extend_from_slice(&inner);
    // Duplicate uuid across two player records.
    let mut dup_stats_uuid = vec![PLAYER_SECTION_SCHEMA];
    megu_core::varint::write_varlong_i64(2, &mut dup_stats_uuid);
    dup_stats_uuid.extend_from_slice(&stats_body[2..]);
    dup_stats_uuid.extend_from_slice(&stats_body[2..]);

    for (reason, body) in [
        ("bad schema", bad_stats_schema),
        ("truncated body", bad_stats_truncated),
        ("trailing bytes", bad_stats_trailing),
        ("oversized blob length", bad_stats_len),
        ("invalid stat category", bad_blob),
        ("duplicate player uuid", dup_stats_uuid),
    ] {
        let framed = player_section_frame(&body, PLAYER_STATS_MAGIC);
        assert!(
            read_player_stats_footer(&framed).is_err(),
            "STA1 {reason} must fail closed"
        );
    }
}

#[test]
fn save_v2_player_section_corruption_fails_load_without_touching_live_maps() {
    if test_support::isolated_world_test() {
        return;
    }
    let _guard = world_guard();
    let old_books = std::mem::take(&mut *recipe_books().lock().unwrap_or_else(|e| e.into_inner()));
    let old_stats = std::mem::take(&mut *player_stats().lock().unwrap_or_else(|e| e.into_inner()));

    let uuid = [0x5E; 16];
    recipe_books().lock().unwrap_or_else(|e| e.into_inner())
        .insert(uuid, sample_recipe_book());
    player_stats().lock().unwrap_or_else(|e| e.into_inner())
        .insert(uuid, sample_player_stats());
    let mut payload = Vec::new();
    save_world_into(&mut payload);

    // Sentinel live state that must survive every rejected load untouched:
    // wipe the saved entries first so their presence can only come from a
    // successful restore.
    let sentinel_book = PlayerRecipeBook::new();
    let mut sentinel_stats = PlayerStats::new();
    sentinel_stats.increment(StatCategory::Used, 9, 1);
    {
        let mut books = recipe_books().lock().unwrap_or_else(|e| e.into_inner());
        books.clear();
        books.insert([0x77; 16], sentinel_book.clone());
    }
    {
        let mut stats = player_stats().lock().unwrap_or_else(|e| e.into_inner());
        stats.clear();
        stats.insert([0x77; 16], sentinel_stats.clone());
    }

    // Locate the STA1 and RCP1 frames inside the real payload: both readers
    // return prefixes of `payload`, so a prefix's length is the frame offset.
    let tail = strip_player_section_tail(&payload);
    let (before_stats, sta1_body) = split_save_footer(tail, PLAYER_STATS_MAGIC).unwrap();
    assert!(sta1_body.is_some());
    let sta1_start = before_stats.len();
    let (before_recipes, _) = split_save_footer(before_stats, RECIPE_BOOK_MAGIC).unwrap();
    let rcp1_start = before_recipes.len();

    // Flip the schema byte of each footer in turn: the readers reject the
    // section during the strip phase, before any live map is restored.
    for (offset, reason) in [(sta1_start, "STA1 schema"), (rcp1_start, "RCP1 schema")] {
        let mut corrupt = payload.clone();
        corrupt[offset] = PLAYER_SECTION_SCHEMA + 1;
        assert!(
            load_world_from(&corrupt).is_err(),
            "{reason} corruption must fail the load"
        );
        let books = recipe_books().lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(books.get(&[0x77; 16]), Some(&sentinel_book), "{reason}");
        assert!(!books.contains_key(&uuid), "{reason}");
        drop(books);
        let stats = player_stats().lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(stats.get(&[0x77; 16]), Some(&sentinel_stats), "{reason}");
        assert!(!stats.contains_key(&uuid), "{reason}");
        drop(stats);
    }

    // The untouched payload still loads and restores the saved sections.
    load_world_from(&payload).unwrap();
    assert!(recipe_books().lock().unwrap_or_else(|e| e.into_inner())
        .contains_key(&uuid));
    assert!(player_stats().lock().unwrap_or_else(|e| e.into_inner())
        .contains_key(&uuid));

    *recipe_books().lock().unwrap_or_else(|e| e.into_inner()) = old_books;
    *player_stats().lock().unwrap_or_else(|e| e.into_inner()) = old_stats;
}
