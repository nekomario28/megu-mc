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
    let old_hot = hot().lock().unwrap().clone();
    let old_warm = warm().lock().unwrap().clone();
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
    *hot().lock().unwrap() = old_hot;
    *warm().lock().unwrap() = old_warm;
    TICK.store(old_tick, Ordering::Relaxed);
    TICK_IDX.store(old_index, Ordering::Relaxed);
    WORLD_TIME.store(old_time, Ordering::Relaxed);
    GR_DAYLIGHT_CYCLE.store(old_daylight, Ordering::Relaxed);
    // These are the current 4TPS contract, NOT vanilla 20TPS acceptance values.
    assert_eq!(actual, (1, 319, 63, 0, 0, 64));
    assert_eq!(clocks, (2, 1, 1005));
}

#[test]
fn region_membership_matches_hot_plus_skirt_across_input_changes() {
    let _world_guard = WORLD_TEST_LOCK.lock().unwrap();
    assert!(clients().lock().unwrap().is_empty());
    let old_actors = std::mem::take(&mut *actors().lock().unwrap());
    let old_forced = std::mem::take(&mut *forceloaded().lock().unwrap());
    let old_hot = hot().lock().unwrap().clone();
    let old_warm = warm().lock().unwrap().clone();
    let mut observations = Vec::new();
    refresh_hot();
    observations.push((hot().lock().unwrap().clone(), warm().lock().unwrap().clone(), HashSet::new(), HashSet::new()));
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
        observations.push((hot().lock().unwrap().clone(), warm().lock().unwrap().clone(), expected_hot, expected_warm));
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
    observations.push((hot().lock().unwrap().clone(), warm().lock().unwrap().clone(), expected_hot, expected_warm));
    actors().lock().unwrap().clear();
    forceloaded().lock().unwrap().insert((-12, 9));
    refresh_hot();
    let expected_hot = HashSet::from([(-12, 9)]);
    let mut expected_warm = HashSet::new();
    for dx in -3..=3 { for dz in -3..=3 { expected_warm.insert((-12 + dx, 9 + dz)); } }
    observations.push((hot().lock().unwrap().clone(), warm().lock().unwrap().clone(), expected_hot, expected_warm));
    forceloaded().lock().unwrap().clear();
    refresh_hot();
    observations.push((hot().lock().unwrap().clone(), warm().lock().unwrap().clone(), HashSet::new(), HashSet::new()));
    *actors().lock().unwrap() = old_actors;
    *forceloaded().lock().unwrap() = old_forced;
    *hot().lock().unwrap() = old_hot;
    *warm().lock().unwrap() = old_warm;
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
    let old_hot = hot().lock().unwrap().clone();
    let old_warm = warm().lock().unwrap().clone();
    let advanced = {
        game_tick(|_, tick| tick());
        WORLD_TIME.load(Ordering::Relaxed) - 1000
    };
    *actors().lock().unwrap() = old_actors;
    *forceloaded().lock().unwrap() = old_forced;
    *hot().lock().unwrap() = old_hot;
    *warm().lock().unwrap() = old_warm;
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
    let old_hot = hot().lock().unwrap().clone();
    let old_warm = warm().lock().unwrap().clone();
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
    while let Some((packet, _)) = read_packet(&mut receiver) {
        if packet == 0x26 || packet == 0x64 { seen.push(packet); }
    }
    // Rewinding an anchor past its period must make the next tick fire it,
    // which is what proves the mechanism is a wall clock and not a counter.
    *wall_anchor(&KEEPALIVE_AT).lock().unwrap() = Instant::now() - Duration::from_millis(KEEPALIVE_MS + 1);
    *wall_anchor(&TIME_NOTIFY_AT).lock().unwrap() = Instant::now() - Duration::from_millis(TIME_NOTIFY_MS + 1);
    game_tick(|_, tick| tick());
    let mut fired = Vec::new();
    while let Some((packet, _)) = read_packet(&mut receiver) {
        if packet == 0x26 || packet == 0x64 { fired.push(packet); }
    }
    // Restore before asserting so a failure cannot poison the shared fixture.
    *clients().lock().unwrap() = old_clients;
    TICK.store(old_tick, Ordering::Relaxed);
    WORLD_TIME.store(old_time, Ordering::Relaxed);
    TICK_IDX.store(old_index, Ordering::Relaxed);
    *actors().lock().unwrap() = old_actors;
    *forceloaded().lock().unwrap() = old_forced;
    *hot().lock().unwrap() = old_hot;
    *warm().lock().unwrap() = old_warm;
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
        assert_eq!(server_tick_period(game_ticks) * TICK_MS, game_ticks * 50,
            "game-tick period {game_ticks} no longer spans {}ms", game_ticks * 50);
    }
    // At the current cadence these are exactly the counts that used to be literal.
    assert_eq!(server_tick_period(10), 2); // lava, void, suffocation, contact
    assert_eq!(server_tick_period(20), 4); // fire, drowning, elytra, cactus, berries
    assert_eq!(server_tick_period(80), 16); // slow natural regeneration
    // A period below one server tick must never collapse to zero, which would
    // make the modulo divide by zero at a faster cadence.
    assert_eq!(server_tick_period(0), 1);
    assert_eq!(server_tick_period(1), 1);
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
    ] {
        actors().lock().unwrap().clear();
        furnaces().lock().unwrap().clear();
        forceloaded().lock().unwrap().clear();
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
            let fs = furnaces().lock().unwrap();
            for f in fs.values() {
                assert_eq!((f.output.1, f.progress, f.burn_left, f.fuel.1),
                    if active { (27, 20, 180, 60) } else { (0, 0, 0, 64) });
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
