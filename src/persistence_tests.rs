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
    forceloaded().lock().unwrap().insert((0, 0));
    world().lock().unwrap().entry((0, 0)).or_default().insert(0x0000_0004, 4);
    drops().lock().unwrap().push(Drop { eid: 777, x: 0.5, y: 70.0, z: 0.5,
        item: name_to_item("iron_ingot").unwrap(), cnt: 3,
        comps: vec![(2i64, br#"{"text":"legacy"}"#.to_vec())], age: drop_age });
    mobs().lock().unwrap().push(Arc::new(Mob { eid: 778, uuid: [0u8; 16], etype: 124,
        pos: Mutex::new((1.5, 70.0, 2.5, 0.0, 0.0)), hp: Mutex::new(12.5),
        last_hit: Mutex::new(Instant::now()), hurt_at: Mutex::new(Instant::now()),
        fuse: Mutex::new(0), love: Mutex::new(0), air: Mutex::new(mob_air) }));
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
            let at = payload.windows(4).position(|w| w == b"MET4").expect("a MET4 tail");
            payload[at..at + 4].copy_from_slice(magic);
            if magic == b"MET2" {
                // magic, mob count, etype, x, y, z, hp — no air field follows.
                payload.truncate(at + 4 + 1 + 1 + 8 + 8 + 8 + 4);
            }
        }
        load_world_from(&payload);
        let d = drops().lock().unwrap().first()
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
    loaded
}

#[test]
fn met4_round_trip_preserves_drop_components_age_and_mob_air() {
    let _world_guard = WORLD_TEST_LOCK.lock().unwrap();
    let full_air = server_ticks(300) as i32;
    let loaded = round_trip(None, 150, 3);
    assert_eq!(loaded, Loaded {
        chunks: 1, forced: 1, world_time: 555, drops: 1, drop_age: 3,
        drop_item: name_to_item("iron_ingot").unwrap(), drop_count: 3,
        drop_comps: vec![(2i64, br#"{"text":"legacy"}"#.to_vec())],
        mobs: 1, mob_etype: 124, mob_hp: 12.5,
        // The MET3 tail has no air field, so a reloaded mob gets a full supply.
        mob_air: full_air,
    });
    // The default air supply is the vanilla 300 game ticks, not a bare literal.
    assert_eq!(u64::from(full_air as u32) * TICK_MS, 15_000);
}

#[test]
fn met2_without_mob_air_falls_back_to_a_full_supply() {
    let _world_guard = WORLD_TEST_LOCK.lock().unwrap();
    // MET2 never carried air. The loader must substitute the vanilla air supply
    // rather than a zero that would drown the mob on its first tick.
    let loaded = round_trip(Some(b"MET2"), 0, 3);
    assert_eq!(loaded.mobs, 1, "the mob must still load from a MET2 tail");
    assert_eq!(loaded.mob_air, server_ticks(300) as i32);
}

#[test]
fn legacy_met3_millisecond_ages_convert_to_ticks_instead_of_despawning() {
    let _world_guard = WORLD_TEST_LOCK.lock().unwrap();
    // The 20TPS cutover made one tick 50ms. A MET3 file that stored a drop age of
    // 300000 ms read as ticks would sit at 300000 ticks against a 6000 tick life
    // and despawn on the first tick, destroying every saved drop.
    let loaded = round_trip(Some(b"MET3"), 150, 3000);
    assert_eq!(loaded.drops, 1, "the drop must survive a legacy MET3 load");
    assert_eq!(loaded.drop_age, 60, "3000 ms of legacy age is 60 ticks at 50 ms per tick");
    assert!(loaded.drop_age < server_ticks(DROP_LIFE_GT) as i32);
}
