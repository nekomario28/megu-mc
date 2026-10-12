use super::*;

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
