use super::*;
use effect_tests::client_pair;

fn furnace(input_count: u8) -> Furnace {
    Furnace {
        input: (name_to_item("raw_iron").unwrap(), input_count, Vec::new()),
        fuel: (name_to_item("coal").unwrap(), 64, Vec::new()),
        output: (0, 0, Vec::new()), burn_left: 0, progress: 0, dirty: false, xp: 0.0,
        recipe_cache: CachedLookup::default(), fuel_cache: CachedLookup::default(),
    }
}

#[test]
fn game_tick_progresses_warm_furnace_and_keeps_cold_furnace_unchanged() {
    let _world_guard = WORLD_TEST_LOCK.lock().unwrap();
    let old_furnaces = std::mem::take(&mut *furnaces().lock().unwrap());
    let old_forced = std::mem::take(&mut *forceloaded().lock().unwrap());
    let (old_hot, old_warm) = test_region();
    let old_tick = TICK.swap(1, Ordering::Relaxed);
    let old_index = TICK_IDX.swap(0, Ordering::Relaxed);
    let old_time = WORLD_TIME.swap(1000, Ordering::Relaxed);
    let old_daylight = GR_DAYLIGHT_CYCLE.swap(1, Ordering::Relaxed);
    forceloaded().lock().unwrap().insert((0, 0));
    let warm_pos = (0, 70, 0);
    let cold_pos = (1600, 70, 1600);
    furnaces().lock().unwrap().insert(warm_pos, furnace(2));
    furnaces().lock().unwrap().insert(cold_pos, furnace(2));
    game_tick(|_, tick| tick());
    let actual = {
        let fs = furnaces().lock().unwrap();
        let hot = &fs[&warm_pos];
        let cold = &fs[&cold_pos];
        (hot.progress, hot.burn_left, hot.fuel.1, cold.progress, cold.burn_left, cold.fuel.1)
    };
    let clocks = (TICK.load(Ordering::Relaxed), TICK_IDX.load(Ordering::Relaxed), WORLD_TIME.load(Ordering::Relaxed));
    *furnaces().lock().unwrap() = old_furnaces;
    *forceloaded().lock().unwrap() = old_forced;
    set_test_region(old_hot, old_warm);
    TICK.store(old_tick, Ordering::Relaxed);
    TICK_IDX.store(old_index, Ordering::Relaxed);
    WORLD_TIME.store(old_time, Ordering::Relaxed);
    GR_DAYLIGHT_CYCLE.store(old_daylight, Ordering::Relaxed);
    // These are the current 4TPS contract, NOT vanilla 20TPS acceptance values.
    assert_eq!(actual, (1, 1599, 63, 0, 0, 64));
    assert_eq!(clocks, (2, 1, 1001));
}

#[test]
fn region_membership_matches_hot_plus_skirt_across_input_changes() {
    let _world_guard = WORLD_TEST_LOCK.lock().unwrap();
    assert!(clients().lock().unwrap().is_empty());
    let old_actors = std::mem::take(&mut *actors().lock().unwrap());
    let old_forced = std::mem::take(&mut *forceloaded().lock().unwrap());
    let (old_hot, old_warm) = test_region();
    let mut observations = Vec::new();
    refresh_hot();
    observations.push((test_region().0, test_region().1, HashSet::new(), HashSet::new()));
    for name in ["moving", "stationary"] {
        actors().lock().unwrap().insert(name.into(), Arc::new(Actor {
            name: name.into(), eid: 4242, pos: Mutex::new((0.0, 70.0, 0.0)),
            look: Mutex::new(None), action: Mutex::new(""), held: Mutex::new((0, 0, Vec::new())),
            inv: Mutex::new(HashMap::new()),
        }));
    }
    for (i, (x, z, cx, cz)) in [
        (0.0, 0.0, 0, 0), (15.999, 15.999, 0, 0), (16.0, 16.0, 1, 1),
        (-0.01, -0.01, -1, -1), (-16.0, -16.0, -1, -1), (-16.01, -16.01, -2, -2),
        (160.0, -160.0, 10, -10), (30_000_000.0, -30_000_000.0, 1_875_000, -1_875_000),
    ].into_iter().enumerate() {
        *actors().lock().unwrap()["moving"].pos.lock().unwrap() = (x, 70.0, z);
        forceloaded().lock().unwrap().clear();
        let mut expected_hot = HashSet::new();
        if i % 2 == 0 {
            forceloaded().lock().unwrap().insert((200, -200));
            expected_hot.insert((200, -200));
        }
        for (px, pz) in [(cx, cz), (0, 0)] {
            for dx in -5..=5 { for dz in -5..=5 { expected_hot.insert((px + dx, pz + dz)); } }
        }
        // Independent oracle: the original hot-then-skirt algorithm.
        let mut expected_warm = expected_hot.clone();
        for &(px, pz) in &expected_hot {
            for dx in -3..=3 { for dz in -3..=3 { expected_warm.insert((px + dx, pz + dz)); } }
        }
        refresh_hot();
        observations.push((test_region().0, test_region().1, expected_hot, expected_warm));
    }
    actors().lock().unwrap().clear();
    for x in 0..10 { for z in 0..10 {
        let name = format!("dense-{x}-{z}");
        actors().lock().unwrap().insert(name.clone(), Arc::new(Actor {
            name, eid: 4242, pos: Mutex::new((16.0 * x as f64, 70.0, 16.0 * z as f64)),
            look: Mutex::new(None), action: Mutex::new(""), held: Mutex::new((0, 0, Vec::new())),
            inv: Mutex::new(HashMap::new()),
        }));
    } }
    refresh_hot();
    let expected_hot = (-5..=14).flat_map(|x| (-5..=14).map(move |z| (x, z))).collect();
    let expected_warm = (-8..=17).flat_map(|x| (-8..=17).map(move |z| (x, z))).collect();
    observations.push((test_region().0, test_region().1, expected_hot, expected_warm));
    actors().lock().unwrap().clear();
    forceloaded().lock().unwrap().insert((-12, 9));
    refresh_hot();
    let expected_hot = HashSet::from([(-12, 9)]);
    let mut expected_warm = HashSet::new();
    for dx in -3..=3 { for dz in -3..=3 { expected_warm.insert((-12 + dx, 9 + dz)); } }
    observations.push((test_region().0, test_region().1, expected_hot, expected_warm));
    forceloaded().lock().unwrap().clear();
    refresh_hot();
    observations.push((test_region().0, test_region().1, HashSet::new(), HashSet::new()));
    *actors().lock().unwrap() = old_actors;
    *forceloaded().lock().unwrap() = old_forced;
    set_test_region(old_hot, old_warm);
    for (actual_hot, actual_warm, expected_hot, expected_warm) in observations {
        assert_eq!(actual_hot, expected_hot);
        assert_eq!(actual_warm, expected_warm);
    }
}

#[test]
fn cadence_owner_keeps_world_clock_at_vanilla_rate() {
    let _world_guard = WORLD_TEST_LOCK.lock().unwrap();
    // 20 game ticks per real second is the invariant the world clock must keep.
    // A cadence change is only coherent if this product stays 20.
    assert_eq!(TICKS_PER_SECOND * GAME_TICKS_PER_TICK, 20);
    assert_eq!(TICKS_PER_SECOND as u64 * TICK_MS, 1000);
    let old_time = WORLD_TIME.swap(1000, Ordering::Relaxed);
    let old_tick = TICK.swap(1, Ordering::Relaxed);
    let old_index = TICK_IDX.swap(0, Ordering::Relaxed);
    let old_actors = std::mem::take(&mut *actors().lock().unwrap());
    let old_forced = std::mem::take(&mut *forceloaded().lock().unwrap());
    let (old_hot, old_warm) = test_region();
    let advanced = {
        game_tick(|_, tick| tick());
        WORLD_TIME.load(Ordering::Relaxed) - 1000
    };
    *actors().lock().unwrap() = old_actors;
    *forceloaded().lock().unwrap() = old_forced;
    set_test_region(old_hot, old_warm);
    TICK.store(old_tick, Ordering::Relaxed);
    TICK_IDX.store(old_index, Ordering::Relaxed);
    WORLD_TIME.store(old_time, Ordering::Relaxed);
    assert_eq!(advanced, GAME_TICKS_PER_TICK as u64);
}

#[test]
fn wall_clock_boundaries_are_gated_on_elapsed_time_not_tick_count() {
    let _world_guard = WORLD_TEST_LOCK.lock().unwrap();
    let (client, mut receiver) = client_pair();
    let old_clients = std::mem::take(&mut *clients().lock().unwrap());
    let old_tick = TICK.swap(0, Ordering::Relaxed);
    let old_time = WORLD_TIME.swap(1000, Ordering::Relaxed);
    let old_index = TICK_IDX.swap(0, Ordering::Relaxed);
    let old_actors = std::mem::take(&mut *actors().lock().unwrap());
    let old_forced = std::mem::take(&mut *forceloaded().lock().unwrap());
    let (old_hot, old_warm) = test_region();
    let old_anchors: Vec<_> = [&KEEPALIVE_AT, &TIME_NOTIFY_AT, &SIZE_LOG_AT]
        .iter().map(|cell| *wall_anchor(cell).lock().unwrap()).collect();
    // Re-anchor every period so this test starts from a known wall clock and
    // cannot inherit an anchor another test initialized long ago.
    for cell in [&KEEPALIVE_AT, &TIME_NOTIFY_AT, &SIZE_LOG_AT] {
        *wall_anchor(cell).lock().unwrap() = Instant::now();
    }
    clients().lock().unwrap().push(Arc::new(client));
    // 40 is the old keepalive divisor and 4 the old time-notify divisor, so a
    // tick-count implementation would have fired both within these 40 ticks.
    // 40 ticks cost microseconds against a 1s period, so elapsed time cannot
    // legitimately reach it here.
    for _ in 0..40 { game_tick(|_, tick| tick()); }
    let mut seen = Vec::new();
    while let Some((packet, _)) = read_packet(&mut receiver, Instant::now() + Duration::from_secs(1)) {
        if packet == 0x26 || packet == 0x64 { seen.push(packet); }
    }
    // Rewinding an anchor past its period must make the next tick fire it,
    // which is what proves the mechanism is a wall clock and not a counter.
    *wall_anchor(&KEEPALIVE_AT).lock().unwrap() = Instant::now() - Duration::from_millis(KEEPALIVE_MS + 1);
    *wall_anchor(&TIME_NOTIFY_AT).lock().unwrap() = Instant::now() - Duration::from_millis(TIME_NOTIFY_MS + 1);
    game_tick(|_, tick| tick());
    let mut fired = Vec::new();
    while let Some((packet, _)) = read_packet(&mut receiver, Instant::now() + Duration::from_secs(1)) {
        if packet == 0x26 || packet == 0x64 { fired.push(packet); }
    }
    // Restore before asserting so a failure cannot poison the shared fixture.
    *clients().lock().unwrap() = old_clients;
    TICK.store(old_tick, Ordering::Relaxed);
    WORLD_TIME.store(old_time, Ordering::Relaxed);
    TICK_IDX.store(old_index, Ordering::Relaxed);
    *actors().lock().unwrap() = old_actors;
    *forceloaded().lock().unwrap() = old_forced;
    set_test_region(old_hot, old_warm);
    for (cell, anchor) in [&KEEPALIVE_AT, &TIME_NOTIFY_AT, &SIZE_LOG_AT].iter().zip(old_anchors) {
        *wall_anchor(cell).lock().unwrap() = anchor;
    }
    assert!(seen.is_empty(), "tick-count boundaries fired immediately: {seen:?}");
    assert_eq!(fired.len(), 2, "expected exactly one keepalive and one time notify: {fired:?}");
    assert_eq!(TICKS_PER_SECOND as u64 * TICK_MS, 1000);
}

#[test]
fn game_tick_periods_preserve_the_vanilla_real_time_interval() {
    // The property that must survive a cadence change: a vanilla interval of N
    // game ticks is N * 50ms, so period * TICK_MS must equal that exactly.
    for game_ticks in [10, 20, 40, 80, 100] {
        assert_eq!(server_ticks(game_ticks) * TICK_MS, game_ticks * 50,
            "game-tick period {game_ticks} no longer spans {}ms", game_ticks * 50);
    }
    // At the current cadence these are exactly the counts that used to be literal.
    assert_eq!(server_ticks(10), 10); // lava, void, suffocation, contact
    assert_eq!(server_ticks(20), 20); // fire, drowning, elytra, cactus, berries
    assert_eq!(server_ticks(80), 80); // slow natural regeneration
    // A period below one server tick must never collapse to zero, which would
    // make the modulo divide by zero at a faster cadence.
    assert_eq!(server_ticks(0), 1);
    assert_eq!(server_ticks(1), 1);
}

#[test]
fn stored_tick_durations_match_the_vanilla_game_tick_values() {
    // The real-time duration must survive a cadence change, so each stored
    // threshold is the vanilla game-tick value divided down to server ticks.
    for game_ticks in [10, 20, 40, 80, 100, 200, 300, 1600, 3600, 12000, 16000] {
        assert_eq!(server_ticks(game_ticks) * TICK_MS, game_ticks * 50,
            "game-tick duration {} no longer spans {}ms", game_ticks, game_ticks * 50);
    }
    // At the current cadence these are exactly the literals used before.
    assert_eq!(server_ticks(200), 200);   // furnace total cook time
    assert_eq!(fuel_ticks(name_to_item("coal").unwrap()), 1600);
    assert_eq!(fuel_ticks(name_to_item("coal_block").unwrap()), 16000);
    assert_eq!(server_ticks(300), 300);   // air supply
    assert_eq!(server_ticks(12000), 12000); // rain
    // Thunder used to share rain's counter, which was 600s rather than the
    // vanilla 180s minimum. Pinning the derived value keeps that fixed.
    assert_eq!(server_ticks(3600), 3600);
    // The lazy-schedule ladder keeps its 0.5s..4s real-time steps.
    assert_eq!((1..=4).map(backoff_ticks).collect::<Vec<_>>(), vec![10, 20, 40, 80]);
    assert_eq!(backoff_ticks(4) * TICK_MS, 4000);
}

#[test]
fn per_tick_rates_keep_the_same_chance_per_real_second() {
    // A per-tick probability is not an interval: the denominator has to scale
    // WITH ticks per second so the chance per real second is unchanged.
    // Crop growth keeps its current 1/30 per second at any cadence.
    assert_eq!(CROP_GROWTH_DENOMINATOR, 600); // 120 at 4TPS, 600 at 20TPS
    assert_eq!(CROP_GROWTH_DENOMINATOR * TICK_MS, 30 * 1000);
    assert_eq!(TICKS_PER_SECOND as u64 * 30, CROP_GROWTH_DENOMINATOR);
    // Every recurring schedule and one-shot life that used to be a tick literal
    // now names its vanilla game-tick value. Each must keep both its previous
    // server-tick literal and its real-time spacing.
    let schedules: [(&str, u64, u64, u64); 5] = [
        ("autosave", AUTOSAVE_GT, 600, 30_000),
        ("spawn cycle", SPAWN_CYCLE_GT, 200, 10_000),
        ("creeper fuse", CREEPER_FUSE_GT, 100, 5_000),
        ("bobber packet", BOBBER_PACKET_GT, 15, 750),
        ("lightning life", BOLT_LIFE_GT, 10, 500),
    ];
    for (name, game_ticks, expected_ticks, millis) in schedules {
        assert_eq!(server_ticks(game_ticks), expected_ticks, "{name} no longer spans its game-tick count");
        assert_eq!(server_ticks(game_ticks) * TICK_MS, millis, "{name} no longer spans {millis}ms");
    }
}

#[test]
fn tnt_fuse_counts_down_in_server_ticks_and_pauses_when_cold() {
    let _world_guard = WORLD_TEST_LOCK.lock().unwrap();
    let old_primed = std::mem::take(&mut *primed().lock().unwrap());
    let old_forced = std::mem::take(&mut *forceloaded().lock().unwrap());
    let old_tick = TICK.swap(1, Ordering::Relaxed);
    let old_index = TICK_IDX.swap(0, Ordering::Relaxed);
    let (old_hot, old_warm) = test_region();
    // Pin the vanilla game-tick values themselves. At 4TPS a one-game-tick drift
    // quantises away, so the projection alone cannot catch it.
    assert_eq!(TNT_FUSE_GT, 80);
    assert_eq!(server_ticks(TNT_FUSE_GT) * TICK_MS, 4000);
    // Chain-lit TNT keeps 0.5s..1.5s of fuse. Assert the production expression,
    // because its 4TPS projection quantises a one-game-tick drift away.
    assert_eq!(chain_fuse_left(0), server_ticks(10));
    assert_eq!(chain_fuse_left(1023), server_ticks(30));
    assert_eq!(chain_fuse_left(0) * TICK_MS, 500);
    assert_eq!(chain_fuse_left(1023) * TICK_MS, 1500);
    forceloaded().lock().unwrap().insert((0, 0));
    primed().lock().unwrap().push(Primed { eid: 9001, x: 0.5, y: 70.0, z: 0.5, left: server_ticks(TNT_FUSE_GT) as i32 });
    let mut exploded_at = None;
    for step in 1..=200 {
        game_tick(|_, tick| tick());
        if primed().lock().unwrap().is_empty() { exploded_at = Some(step); break; }
    }
    // Far outside the hot set the fuse must not advance at all, so an unloaded
    // TNT survives indefinitely instead of exploding on a wall-clock timer.
    primed().lock().unwrap().push(Primed { eid: 9002, x: 30_000_000.0, y: 70.0, z: 30_000_000.0, left: 3 });
    for _ in 0..20 { game_tick(|_, tick| tick()); }
    let cold_left = primed().lock().unwrap().iter().find(|p| p.eid == 9002).map(|p| p.left);
    // Chain-lit TNT keeps its 0.5s..1.5s remaining fuse in game ticks.
    let mut chain_range = Vec::new();
    for jitter in [0u64, 1, 20, 1000, 1023] {
        chain_range.push(server_ticks(10 + jitter.min(20)) as i32);
    }
    *primed().lock().unwrap() = old_primed;
    *forceloaded().lock().unwrap() = old_forced;
    TICK.store(old_tick, Ordering::Relaxed);
    TICK_IDX.store(old_index, Ordering::Relaxed);
    set_test_region(old_hot, old_warm);
    assert_eq!(exploded_at, Some(80), "the 4s fuse must fire on its 80th hot tick");
    assert_eq!(cold_left, Some(3), "a cold fuse must not count down");
    // jitter 1 now survives the integer divide, which 4TPS quantised away.
    assert_eq!(chain_range, vec![10, 11, 30, 30, 30]);
}

#[test]
fn entity_life_and_pickup_delay_count_server_ticks_and_freeze_when_cold() {
    let _world_guard = WORLD_TEST_LOCK.lock().unwrap();
    let old_drops = std::mem::take(&mut *drops().lock().unwrap());
    let old_xps = std::mem::take(&mut *xps().lock().unwrap());
    let old_forced = std::mem::take(&mut *forceloaded().lock().unwrap());
    let old_tick = TICK.swap(1, Ordering::Relaxed);
    let old_index = TICK_IDX.swap(0, Ordering::Relaxed);
    let (old_hot, old_warm) = test_region();
    // Pin the vanilla game-tick values, not just their 4TPS projection.
    assert_eq!(PICKUP_DELAY_GT, 10);
    assert_eq!(DROP_LIFE_GT, 6000);
    assert_eq!(XP_LIFE_GT, 6000);
    assert_eq!(server_ticks(PICKUP_DELAY_GT) * TICK_MS, 500);
    assert_eq!(server_ticks(DROP_LIFE_GT) * TICK_MS, 300_000);
    forceloaded().lock().unwrap().insert((0, 0));
    drops().lock().unwrap().push(Drop { eid: 9101, x: 0.5, y: 70.0, z: 0.5, item: 1, cnt: 1, comps: Vec::new(), age: 0 });
    xps().lock().unwrap().push(Xp { eid: 9102, x: 0.5, y: 70.0, z: 0.5, val: 1, age: 0 });
    // A drop far outside the hot set must not age, so an unloaded world keeps it.
    drops().lock().unwrap().push(Drop { eid: 9103, x: 30_000_000.0, y: 70.0, z: 30_000_000.0, item: 1, cnt: 1, comps: Vec::new(), age: 0 });
    for _ in 0..5 { game_tick(|_, tick| tick()); }
    let hot_age = drops().lock().unwrap().iter().find(|d| d.eid == 9101).map(|d| d.age);
    let cold_age = drops().lock().unwrap().iter().find(|d| d.eid == 9103).map(|d| d.age);
    let xp_age = xps().lock().unwrap().iter().find(|x| x.eid == 9102).map(|x| x.age);
    // A drop past its lifespan must be removed by the despawn pass.
    let life = server_ticks(DROP_LIFE_GT) as i32;
    drops().lock().unwrap().iter_mut().find(|d| d.eid == 9101).unwrap().age = life;
    for _ in 0..2 { game_tick(|_, tick| tick()); }
    let expired = drops().lock().unwrap().iter().all(|d| d.eid != 9101);
    // Pickup must stay blocked until the delay has elapsed in ticks. Use a hot
    // drop: 9103 is deliberately outside the hot set and therefore never ages.
    drops().lock().unwrap().push(Drop { eid: 9104, x: 0.5, y: 70.0, z: 0.5, item: 1, cnt: 1, comps: Vec::new(), age: 0 });
    let mut pickup_ready_at = None;
    for step in 1..=20 {
        game_tick(|_, tick| tick());
        let d = drops().lock().unwrap().iter().find(|d| d.eid == 9104).map(|d| d.age).unwrap_or(0);
        if d >= server_ticks(PICKUP_DELAY_GT) as i32 { pickup_ready_at = Some(step); break; }
    }
    *drops().lock().unwrap() = old_drops;
    *xps().lock().unwrap() = old_xps;
    *forceloaded().lock().unwrap() = old_forced;
    TICK.store(old_tick, Ordering::Relaxed);
    TICK_IDX.store(old_index, Ordering::Relaxed);
    set_test_region(old_hot, old_warm);
    assert_eq!(hot_age, Some(5), "a hot drop must age once per tick");
    // XP aging runs in the environment stage, which precedes refresh_hot, so it
    // observes the previous tick hot set and trails the drop pass by one tick.
    assert_eq!(xp_age, Some(4), "a hot xp orb must age once per tick, one stage late");
    assert_eq!(cold_age, Some(0), "a cold drop must not age");
    assert!(expired, "a drop past its lifespan must be removed");
    assert_eq!(pickup_ready_at, Some(10), "the 0.5s pickup delay is 10 server ticks");
}

#[test]
fn air_supply_is_derived_everywhere_a_creature_is_constructed() {
    // The air supply is a duration, so it must come from the cadence owner. A
    // leftover 4TPS literal would give 3s instead of the vanilla 15s, and only
    // the reset paths had been converted, not the construction sites.
    assert_eq!(AIR_SUPPLY_GT, 300);
    assert_eq!(server_ticks(AIR_SUPPLY_GT) * TICK_MS, 15_000);
    let _world_guard = WORLD_TEST_LOCK.lock().unwrap();
    let old_mobs = std::mem::take(&mut *mobs().lock().unwrap());
    let old_forced = std::mem::take(&mut *forceloaded().lock().unwrap());
    let old_tick = TICK.swap(1, Ordering::Relaxed);
    let old_index = TICK_IDX.swap(0, Ordering::Relaxed);
    let (old_hot, old_warm) = test_region();
    let expected = server_ticks(AIR_SUPPLY_GT) as i32;
    // Every construction path shares one helper, so exercising the spawn path
    // covers the sites that build creatures directly.
    forceloaded().lock().unwrap().insert((0, 0));
    game_tick(|_, tick| tick());
    let mut seen: Vec<i32> = mobs().lock().unwrap().iter().map(|m| *m.air.lock().unwrap()).collect();
    *mobs().lock().unwrap() = old_mobs;
    *forceloaded().lock().unwrap() = old_forced;
    TICK.store(old_tick, Ordering::Relaxed);
    TICK_IDX.store(old_index, Ordering::Relaxed);
    set_test_region(old_hot, old_warm);
    seen.retain(|air| *air > 0);
    assert!(seen.iter().all(|air| *air == expected), "air supply was {seen:?}, expected {expected}");
}

#[test]
fn projectile_lifetimes_keep_their_real_time_at_any_cadence() {
    // These were bare tick counts written for 4TPS, so the 20TPS cutover silently
    // shortened them fivefold: an arrow lived 12s instead of 60s. The cutover
    // claimed TTLs kept their real time, which was wrong.
    for (name, game_ticks, millis) in [
        ("arrow", ARROW_TTL_GT, 60_000u64),
        ("mob arrow", MOB_ARROW_TTL_GT, 40_000),
        ("pearl", PEARL_TTL_GT, 12_500),
        ("snowball", SNOWBALL_TTL_GT, 15_000u64),
    ] {
        assert_eq!(server_ticks(game_ticks) * TICK_MS, millis,
            "{name} no longer spans {millis}ms");
    }
    // A bare tick count would fail this: at 20TPS, 240 ticks is 12s, not 60s.
    assert!(server_ticks(ARROW_TTL_GT) != 240 || TICK_MS == 250);
}

#[test]
fn remaining_tick_periods_and_chances_keep_their_real_time() {
    // Same class as the projectile TTLs: recurring periods must divide by
    // GAME_TICKS_PER_TICK, and per-tick probabilities must multiply by
    // TICKS_PER_SECOND. Getting the direction wrong changes the rate 5x or 8x.
    for (name, game_ticks, millis) in [
        ("starvation", STARVE_GT, 4_000u64),
        ("fluid tick", FLUID_TICK_GT, 1_500),
    ] {
        assert_eq!(server_ticks(game_ticks) * TICK_MS, millis, "{name} no longer spans {millis}ms");
    }
    // Per-tick probabilities keep the same chance per real second. The rate is
    // ticks per second over the denominator, and it must equal the 4TPS rate.
    for (name, denominator, old_denominator) in [
        ("rain", RAIN_CHANCE_DENOMINATOR, 6000u64),
        ("mob spawn", MOB_SPAWN_CHANCE_DENOMINATOR, 400u64),
    ] {
        assert_eq!(TICKS_PER_SECOND as u64 * old_denominator, 4 * denominator,
            "{name} no longer fires at the same rate per second as at 4TPS");
    }
}

#[test]
fn tick_pacing_holds_cadence_and_refuses_to_catch_up() {
    // The deadline must advance by exactly one interval each tick, so tick work
    // that overruns does not walk the clock later. A plain sleep(TICK_MS) would
    // lose the overrun every tick and drift.
    let start = Instant::now();
    let mut deadline = start;
    for _ in 0..100 { deadline += Duration::from_millis(TICK_MS); }
    assert_eq!(deadline, start + Duration::from_millis(TICK_MS * 100));
    // Without an overrun the deadline is kept, so cadence does not slip.
    assert_eq!(next_deadline(deadline, false), deadline);
    // With an overrun the deadline is re-anchored to now, so the loop cannot
    // spin to catch up and add more work to an already overloaded server.
    let reanchored = next_deadline(deadline, true);
    assert!(reanchored >= start);
    assert!(reanchored < deadline);
    // The interval itself must be the single cadence owner.
    assert_eq!(Duration::from_millis(TICK_MS), Duration::from_millis(1000 / TICKS_PER_SECOND as u64));
}

#[test]
#[ignore = "explicit release-profile run from the isolated tick profiler"]
fn profile_game_tick() {
    let _world_guard = WORLD_TEST_LOCK.lock().unwrap();
    let scratch = std::env::var("MC_SPINE_TICK_PROFILE_DIR").expect("use the isolated profiling runner");
    assert_eq!(std::env::current_dir().unwrap(), std::path::PathBuf::from(scratch));
    assert_ne!(std::env::current_dir().unwrap(), std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")));
    assert!(!worldgen::enabled(), "this fixture measures the flat/headless tick only");
    for (scenario, actor_count, machines, active) in [
        ("empty", 0, 0, false),
        ("idle_actor", 1, 0, false),
        ("idle_furnaces", 1, 400, false),
        ("active_furnaces", 1, 400, true),
        ("crowded_actors", 100, 0, false),
        ("dense_actors", 100, 0, false),
        // Without drops, xp, crops and mobs in the fixture the per-item membership
        // locks never fire, so hoisting them cannot be measured.
        ("entity_heavy", 1, 0, false),
        ("entity_heavy_furnaces", 1, 400, true),
        // The arm the other furnace scenarios cannot exercise. A fixture with no
        // datapacks leaves the smelt list empty, so resolving a result costs nothing
        // there and the recipe cache has nothing to save. This one loads 40 pack
        // recipes that match nothing, so each furnace walks the whole list.
        ("active_furnaces_pack", 1, 400, true),
        // A pile: the bucket merge is O(n x neighbourhood), so concentrating
        // drops in one area grows the neighbourhood back toward n.
        ("drop_pile", 1, 0, false),
    ] {
        actors().lock().unwrap().clear();
        furnaces().lock().unwrap().clear();
        forceloaded().lock().unwrap().clear();
        drops().lock().unwrap().clear();
        xps().lock().unwrap().clear();
        crops().lock().unwrap().clear();
        mobs().lock().unwrap().clear();
        json_smelts().lock().unwrap().clear();
        if scenario == "active_furnaces_pack" {
            for i in 0..40i64 {
                json_smelts()
                    .lock()
                    .unwrap()
                    .push((Ingr::Item(900_000 + i), 42, 1, 0.0));
            }
            CONTENT_REVISION.fetch_add(1, Ordering::Relaxed);
        }
        if scenario == "drop_pile" {
            // 600 drops inside a 3x3 block area, all mergeable, plus 600 spread
            // far apart that must not merge, to separate merge cost from scan cost.
            for i in 0..600 {
                let (x, y, z) = (0.5 + (i % 3) as f64 * 0.05, 70.0, 0.5 + (i / 3 % 3) as f64 * 0.05);
                drops().lock().unwrap().push(Drop { eid: 30_000 + i, x, y, z,
                    item: 811, cnt: 1, comps: Vec::new(), age: i as i32 });
            }
            for i in 0..600 {
                let (x, y, z) = (400.0 + (i % 20) as f64 * 0.4, 70.0, 400.0 + (i / 20) as f64 * 0.4);
                drops().lock().unwrap().push(Drop { eid: 31_000 + i, x, y, z,
                    item: 812, cnt: 1, comps: Vec::new(), age: i as i32 });
            }
        }
        if scenario.starts_with("entity_heavy") {
            for i in 0..600 {
                let (x, y, z) = (16.0 * (i % 20) as f64, 70.0, 16.0 * (i / 20) as f64);
                drops().lock().unwrap().push(Drop { eid: 20_000 + i, x, y, z,
                    item: 811, cnt: 1, comps: Vec::new(), age: (i % 400) as i32 });
                xps().lock().unwrap().push(Xp { eid: 21_000 + i, x, y, z, val: 1, age: (i % 400) as i32 });
                crops().lock().unwrap().insert((x as i32, y as i32, z as i32), (i % 7) as u8);
                mobs().lock().unwrap().push(Arc::new(Mob { eid: 22_000 + i, uuid: [0u8; 16], etype: 124,
                    pos: Mutex::new((x + 1.0, y, z, 0.0, 0.0)), hp: Mutex::new(20.0),
                    last_hit: Mutex::new(Instant::now()), hurt_at: Mutex::new(Instant::now()),
                    fuse: Mutex::new(0), love: Mutex::new(0),
                    air: Mutex::new(server_ticks(AIR_SUPPLY_GT) as i32) }));
            }
        }
        TICK.store(1, Ordering::Relaxed);
        TICK_IDX.store(0, Ordering::Relaxed);
        WORLD_TIME.store(1000, Ordering::Relaxed);
        for i in 0..actor_count {
            let name = format!("tick-profile-{i}");
            let pos = if scenario == "dense_actors" { (16.0 * (i % 10) as f64, 70.0, 16.0 * (i / 10) as f64) } else { (0.0, 70.0, 0.0) };
            actors().lock().unwrap().insert(name.clone(), Arc::new(Actor {
                name, eid: 4242 + i, pos: Mutex::new(pos),
                look: Mutex::new(None), action: Mutex::new(""), held: Mutex::new((0, 0, Vec::new())),
                inv: Mutex::new(HashMap::new()),
            }));
        }
        for i in 0..machines {
            furnaces().lock().unwrap().insert((i % 16, 70 + i / 256, (i / 16) % 16), furnace(if active { 64 } else { 0 }));
        }
        for _ in 0..100 { game_tick(|_, tick| tick()); }
        let mut totals: std::collections::BTreeMap<&str, (u128, u128, usize)> = std::collections::BTreeMap::new();
        let mut samples = Vec::with_capacity(1000);
        for _ in 0..1000 {
            let start = Instant::now();
            game_tick(|name, tick| {
                let begin = Instant::now();
                tick();
                let elapsed = begin.elapsed().as_nanos();
                let item = totals.entry(name).or_default();
                item.0 += elapsed; item.1 = item.1.max(elapsed); item.2 += 1;
            });
            samples.push(start.elapsed().as_nanos());
        }
        if machines > 0 {
            // Assert the machines actually ran, so a faster measurement cannot be
            // a stage that silently stopped doing work. The counts themselves are
            // cadence dependent, so bound them rather than pin them.
            let fs = furnaces().lock().unwrap();
            for f in fs.values() {
                assert_eq!((f.output.1 > 0, f.progress > 0, f.burn_left > 0, f.fuel.1 < 64),
                    if active { (true, true, true, true) } else { (false, false, false, false) });
            }
        }
        samples.sort_unstable();
        println!("PROFILE\t{scenario}\ttick\t{}\t{}\t{}\t{}\t{}",
            samples.len(), samples.iter().sum::<u128>(), samples[499], samples[949], samples[989]);
        for (stage, (total, max, calls)) in totals {
            println!("PROFILE\t{scenario}\t{stage}\t{calls}\t{total}\t{max}");
        }
    }
}


/// The velocity itself must decay, which only a test driving the production loop
/// can observe. Checking the arithmetic alone passes even when arrow_tick still
/// applies drag to the displacement.
#[test]
fn the_production_arrow_loop_decays_horizontal_velocity() {
    let _world_guard = WORLD_TEST_LOCK.lock().unwrap();
    let old_arrows = std::mem::take(&mut *arrows().lock().unwrap());
    let old_forced = std::mem::take(&mut *forceloaded().lock().unwrap());
    let old_tick = TICK.swap(1, Ordering::Relaxed);
    let old_index = TICK_IDX.swap(0, Ordering::Relaxed);
    let (old_hot, old_warm) = test_region();
    forceloaded().lock().unwrap().insert((0, 0));
    arrows().lock().unwrap().push(Arrow { eid: 8001, owner: 1, x: 0.5, y: 120.0, z: 0.5,
        dx: 1.0, dy: 0.0, dz: 0.0, ttl: server_ticks(ARROW_TTL_GT) as i32, dmg: 6.0, fire: 0,
        punch: 0, kind: 0, loy: 0, ret_item: 0, ret_comps: Vec::new(), pierce: 0, last_hit: -1, stuck: false });
    let (mut prev_x, mut prev_v) = (0.5f64, 1.0f64);
    let mut steps = Vec::new();
    for _ in 0..5 {
        game_tick(|_, tick| tick());
        let a = arrows().lock().unwrap();
        let Some(a) = a.iter().find(|a| a.eid == 8001) else { break };
        let (x, v) = (a.x, a.dx);
        drop(a);
        steps.push((x - prev_x, v));
        prev_x = x; prev_v = v;
    }
    *arrows().lock().unwrap() = old_arrows;
    *forceloaded().lock().unwrap() = old_forced;
    TICK.store(old_tick, Ordering::Relaxed);
    TICK_IDX.store(old_index, Ordering::Relaxed);
    set_test_region(old_hot, old_warm);
    assert_eq!(steps.len(), 5, "the arrow left the hot set: {steps:?}");
    // Horizontal velocity must shrink by the drag factor every tick, and the step
    // must equal the velocity rather than the velocity times the drag.
    let mut expected = 1.0f64;
    for (step, velocity) in &steps {
        expected *= 0.99;
        assert!((*velocity - expected).abs() < 1e-9, "velocity {velocity} != {expected}");
        assert!((*step - *velocity).abs() < 1e-9, "step {step} != velocity {velocity}");
        assert!(*step < 1.0, "the step must be the damped velocity, not the initial one: {step}");
    }
    assert!(prev_v < 0.96, "velocity barely decayed: {prev_v}");
}

/// Pins how water spreads under the current implementation, cell by cell and tick
/// by tick. The scheduled-tick migration must reproduce this trace exactly: it
/// replaces the per-tick sweep of every registered cell with a delay queue, and a
/// scheduling site that is missed would make a cell stop updating. A trace is the
/// only way to see that, because the failure is silent.
///
/// This pins the pre-migration behaviour, not vanilla's. It is an equivalence
/// gate for the migration, not a parity claim.
#[test]
fn water_spread_trace_is_stable_across_the_scheduler_migration() {
    let _world_guard = WORLD_TEST_LOCK.lock().unwrap();
    let old_waters = std::mem::take(&mut *waters().lock().unwrap());
    let old_forced = std::mem::take(&mut *forceloaded().lock().unwrap());
    let old_tick = TICK.swap(1, Ordering::Relaxed);
    let old_index = TICK_IDX.swap(0, Ordering::Relaxed);
    let (old_hot, old_warm) = test_region();
    forceloaded().lock().unwrap().insert((0, 0));
    refresh_hot();
    // One source block at the surface of a flat warm area.
    // In the air above the surface: this exercises the falling column, which
    // schedules a new cell every tick and is the path the queue replaces. A
    // surface source barely flows in the current implementation, so it would not
    // exercise the scheduler at all.
    // High enough that forty ticks of falling cannot reach the ground whatever
    // the surface height is, so the trace does not depend on whether another test
    // has left worldgen enabled.
    let y = 150;
    let src = name_to_state("water").unwrap_or(80);
    // Snapshot the column so the test can put it back: leaving flooded cells in
    // the shared world makes any later test that reads the world order-dependent.
    // The trace is also restricted to this column, so unrelated world content
    // cannot leak into it.
    let original: Vec<(i32, u32)> = (105..=151).map(|y| (y, block_at(0, y, 0))).collect();
    set_block(0, y, 0, src);
    // Record the water cells and their levels after each tick.
    let mut trace: Vec<Vec<(i32, i32, i32, u32)>> = Vec::new();
    for _ in 0..40 {
        game_tick(|_, tick| tick());
        let mut cells: Vec<(i32, i32, i32, u32)> = {
            let w = world().lock().unwrap();
            let mut v = Vec::new();
            for (&(cx, cz), c) in w.iter() {
                for (idx, st) in c.iter() {
                    let (wx, wy, wz) = (cx * 16 + (*idx & 15) as i32, (*idx >> 8) as i32 + MIN_Y,
                                        cz * 16 + ((*idx >> 4) & 15) as i32);
                    if (80..=95).contains(st) && wx == 0 && wz == 0 {
                        v.push((wx, wy, wz, *st - 80));
                    }
                }
            }
            v
        };
        cells.sort_unstable();
        trace.push(cells);
    }
    let flooded = trace.last().unwrap().len();
    let max_level = trace.last().unwrap().iter().map(|c| c.3).max().unwrap_or(0);
    for (y, st) in original { set_block(0, y, 0, st); }
    *waters().lock().unwrap() = old_waters;
    *forceloaded().lock().unwrap() = old_forced;
    TICK.store(old_tick, Ordering::Relaxed);
    TICK_IDX.store(old_index, Ordering::Relaxed);
    set_test_region(old_hot, old_warm);
    // The scenario must actually do something, or the trace proves nothing.
    assert!(flooded > 3, "the source barely spread: {flooded} cells");
    assert!(max_level >= 1, "no flowing level appeared");
    let per_tick: Vec<(usize, u32)> = trace.iter()
        .map(|c| (c.len(), c.iter().map(|x| x.3).max().unwrap_or(0))).collect();
    let expected_per_tick: Vec<(usize, u32)> = (2..=41).map(|n| (n, 8u32)).collect();
    // The source keeps level 0 and the falling column below it is level 8.
    let mut expected_final: Vec<(i32, i32, i32, u32)> = (110..=149).map(|y| (0, y, 0, 8u32)).collect();
    expected_final.push((0, 150, 0, 0));
    assert_eq!(per_tick, expected_per_tick,
        "the fluid scheduler changed how many cells run or when they run");
    let mut actual = trace.last().unwrap().clone();
    let mut exp = expected_final.clone();
    actual.sort_unstable(); exp.sort_unstable();
    let missing: Vec<_> = exp.iter().filter(|c| !actual.contains(c)).collect();
    let extra: Vec<_> = actual.iter().filter(|c| !exp.contains(c)).collect();
    assert_eq!((missing, extra), (vec![], vec![]), "flooded set differs");
}

/// Pins hopper item movement tick by tick, for the same reason as the water
/// trace: the scheduler migration replaces the sweep of every registered hopper,
/// and a missed re-queue would silently stop a hopper moving items.
#[test]
fn hopper_transfer_trace_is_stable_across_the_scheduler_migration() {
    let _world_guard = WORLD_TEST_LOCK.lock().unwrap();
    let old_hoppers = std::mem::take(&mut *hoppers().lock().unwrap());
    let old_chests = std::mem::take(&mut *chests().lock().unwrap());
    let old_forced = std::mem::take(&mut *forceloaded().lock().unwrap());
    let old_tick = TICK.swap(1, Ordering::Relaxed);
    let old_index = TICK_IDX.swap(0, Ordering::Relaxed);
    let (old_hot, old_warm) = test_region();
    forceloaded().lock().unwrap().insert((0, 0));
    refresh_hot();
    let hy = 120;
    let hopper_st = 9225u32; // facing down
    let chest_st = name_to_state("chest").unwrap_or(9225);
    set_block(0, hy, 0, hopper_st);
    set_block(0, hy - 1, 0, chest_st);
    hoppers().lock().unwrap().insert((0, hy, 0));
    let item = name_to_item("raw_iron").unwrap_or(1);
    chests().lock().unwrap().entry((0, hy, 0)).or_default().insert(0, (item, 3, Vec::new()));
    let mut trace: Vec<(u8, u8)> = Vec::new();
    for _ in 0..24 {
        game_tick(|_, tick| tick());
        let own = chests().lock().unwrap().get(&(0, hy, 0))
            .map(|m| m.values().map(|v| v.1).sum::<u8>()).unwrap_or(0);
        let dst = chests().lock().unwrap().get(&(0, hy - 1, 0))
            .map(|m| m.values().map(|v| v.1).sum::<u8>()).unwrap_or(0);
        trace.push((own, dst));
    }
    // restore
    set_block(0, hy, 0, 0);
    set_block(0, hy - 1, 0, 0);
    *hoppers().lock().unwrap() = old_hoppers;
    *chests().lock().unwrap() = old_chests;
    *forceloaded().lock().unwrap() = old_forced;
    TICK.store(old_tick, Ordering::Relaxed);
    TICK_IDX.store(old_index, Ordering::Relaxed);
    set_test_region(old_hot, old_warm);
    // One item per tick for three ticks, then nothing left to move.
    let expected: Vec<(u8, u8)> = vec![(2, 1), (1, 2), (0, 3)]
        .into_iter().chain(std::iter::repeat((0, 3)).take(21)).collect();
    assert_eq!(trace, expected,
        "the hopper scheduler changed what moves, or when");
}

/// Pins lava flow for the same reason as water and hoppers. Lava's cadence is the
/// complication: lava_tick runs once per server_ticks(FLUID_TICK_GT), while the
/// queue advances one step per due() call, so the delay must be expressed in
/// invocations. Because a cell can only ever be visited on those boundaries,
/// "due at tick T" and "first visit at or after T" are the same thing, and the
/// conversion is exact rather than an approximation.
#[test]
fn lava_spread_trace_is_stable_across_the_scheduler_migration() {
    let _world_guard = WORLD_TEST_LOCK.lock().unwrap();
    let old_lavas = std::mem::take(&mut *lavas().lock().unwrap());
    let old_forced = std::mem::take(&mut *forceloaded().lock().unwrap());
    let old_tick = TICK.swap(1, Ordering::Relaxed);
    let old_index = TICK_IDX.swap(0, Ordering::Relaxed);
    let (old_hot, old_warm) = test_region();
    forceloaded().lock().unwrap().insert((0, 0));
    refresh_hot();
    // High enough that the test never reaches terrain whatever worldgen state
    // another test leaves behind.
    let top = 150;
    let src = name_to_state("lava").unwrap_or(96);
    let original: Vec<(i32, u32)> = (95..=151).map(|y| (y, block_at(0, y, 0))).collect();
    set_block(0, top, 0, src);
    let mut trace: Vec<(u64, Vec<u32>)> = Vec::new();
    let mut last: Vec<u32> = Vec::new();
    for tick in 0..400u64 {
        game_tick(|_, t| t());
        let mut col: Vec<u32> = (80..=152).map(|y| block_at(0, y, 0)).collect();
        col.retain(|st| (96..=111).contains(st));
        if col != last {
            trace.push((tick, col.clone()));
            last = col;
        }
    }
    for (y, st) in original { set_block(0, y, 0, st); }
    *lavas().lock().unwrap() = old_lavas;
    *forceloaded().lock().unwrap() = old_forced;
    TICK.store(old_tick, Ordering::Relaxed);
    TICK_IDX.store(old_index, Ordering::Relaxed);
    set_test_region(old_hot, old_warm);
    assert!(trace.len() > 3, "lava never flowed: {trace:?}");
    // One falling-lava cell (104) is added below the source (96) per invocation,
    // and invocations land every server_ticks(FLUID_TICK_GT).
    assert_eq!(trace[0], (0, vec![96u32]), "the source must start alone");
    for (k, (tick, col)) in trace.iter().enumerate().skip(1) {
        let mut expected = vec![104u32; k];
        expected.push(96);
        assert_eq!(col, &expected, "column shape at change {k}");
        assert_eq!(*tick, 29 + 30 * (k as u64 - 1), "timing at change {k}");
    }
}

/// A neighbour change has to make a backed-off fluid cell due promptly. Clearing
/// the backoff map alone did not: the delay queues carry their own deadline, so a
/// waiting cell kept its old slot and was not visited until it came round, which
/// is the opposite of being woken. This is the regression for that.
#[test]
fn a_neighbour_change_promotes_a_waiting_fluid_cell() {
    let _world_guard = WORLD_TEST_LOCK.lock().unwrap();
    let pos = (3, 70, 3);
    // Park the cell far out in the queue, as a backed-off cell would be.
    water_queue().lock().unwrap().schedule(pos, tick_queue::MAX_DELAY);
    assert!(water_queue().lock().unwrap().len() > 0, "the cell must be waiting");
    // A block change next to it must pull it forward.
    sched_invalidate(pos.0 + 1, pos.1, pos.2);
    // drain one tick's worth: the promoted cell must be in it
    let mut found = false;
    for _ in 0..2 {
        let due = { let mut q = water_queue().lock().unwrap(); q.due() };
        if due.contains(&pos) { found = true; break; }
    }
    // clean up whatever is left
    water_queue().lock().unwrap().cancel(pos);
    assert!(found, "a neighbour change must wake a waiting cell within two ticks");
}

/// The spread traces cannot cover this: a flowing cell returns before it is ever
/// backed off, so its re-queue always takes the no-backoff branch where the period
/// does not matter. Replacing the period with 1 left every trace green, which is
/// the gap this closes. The conversion itself is what a periodic system gets wrong
/// (lava was thirty times too slow before), so it is pinned directly.
#[test]
fn a_periodic_system_expresses_its_backoff_in_invocations() {
    let _world_guard = WORLD_TEST_LOCK.lock().unwrap();
    let pos = (11, 70, 11);
    let period = server_ticks(FLUID_TICK_GT);
    // How many invocations pass before the cell comes due.
    let invocations = |ahead: u64| -> usize {
        backoff()
            .lock()
            .unwrap()
            .insert(pos, (TICK_IDX.load(Ordering::Relaxed) + ahead, 1));
        lava_queue().lock().unwrap().cancel(pos);
        requeue_lava(pos.0, pos.1, pos.2);
        for n in 1..=12 {
            let due = { let mut q = lava_queue().lock().unwrap(); q.due() };
            if due.contains(&pos) { return n }
        }
        panic!("the cell never came due");
    };
    // A backoff inside the next period still lands on the next invocation, because
    // that is the earliest moment the system can visit it at all.
    assert_eq!(invocations(1), invocations(period), "within one period");
    assert_eq!(invocations(period + 1), invocations(2 * period), "one period later");
    assert_eq!(invocations(2 * period + 1), invocations(3 * period), "two periods later");
    assert!(
        invocations(period + 1) > invocations(period),
        "crossing a period boundary must cost an invocation"
    );
    lava_queue().lock().unwrap().cancel(pos);
    backoff().lock().unwrap().remove(&pos);
}

/// Vanilla grows sugar cane one stage per random tick and adds a block on the tick that
/// would pass age 15, so sixteen random ticks raise the column by one. That makes it the
/// one behaviour a random tick can drive that can be pinned exactly, which is why it is
/// the first consumer of the stream.
#[test]
fn sixteen_random_ticks_raise_sugar_cane_one_block() {
    let _world_guard = WORLD_TEST_LOCK.lock().unwrap();
    let old_forced = std::mem::take(&mut *forceloaded().lock().unwrap());
    let (old_hot, old_warm) = test_region();
    let top = 150;
    let cane = name_to_state("sugar_cane").expect("sugar_cane must exist");
    let original: Vec<(i32, u32)> = (top..=top + 4).map(|y| (y, block_at(0, y, 0))).collect();
    for y in top..=top + 4 {
        set_block(0, y, 0, 0);
    }
    set_block(0, top, 0, cane);

    let reg = crate::feature::breg().expect("the block registry must load from assets/");
    let tick = || on_random_tick(0, top, 0);
    let age = |y: i32| reg.get_prop(block_at(0, y, 0), "age").and_then(|a| a.parse::<u32>().ok());
    let is_cane = |y: i32| reg.name_of(block_at(0, y, 0)) == "minecraft:sugar_cane";

    assert!(is_cane(top) && !is_cane(top + 1), "the test must start with one cane");
    assert_eq!(age(top), Some(0), "a placed cane starts at age 0");

    // Fifteen ticks walk the age up without adding a block.
    for i in 0..15 {
        tick();
        assert!(!is_cane(top + 1), "no block may appear before the sixteenth tick (tick {i})");
    }
    assert_eq!(age(top), Some(15), "fifteen ticks must reach age 15");

    // The sixteenth adds the block above and resets the age.
    tick();
    assert!(is_cane(top + 1), "the sixteenth tick must add a cane above");
    assert_eq!(age(top), Some(0), "adding a block must reset the age");
    assert_eq!(age(top + 1), Some(0), "the new cane starts at age 0");

    // Sixteen more make three, and a third round must not make four.
    for _ in 0..16 {
        tick();
        on_random_tick(0, top + 1, 0);
    }
    assert!(is_cane(top + 2) && !is_cane(top + 3), "the column must stop at height 3");
    for _ in 0..40 {
        tick();
        on_random_tick(0, top + 1, 0);
        on_random_tick(0, top + 2, 0);
    }
    assert!(!is_cane(top + 3), "a column of three must not grow a fourth");

    for (y, st) in original {
        set_block(0, y, 0, st);
    }
    *forceloaded().lock().unwrap() = old_forced;
    set_test_region(old_hot, old_warm);
}

