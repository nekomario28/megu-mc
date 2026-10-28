use super::*;
use effect_tests::client_pair;

/// `adv_event` used to build its (lazily initialized) event index while holding the
/// advancement map's own lock, whose initializer locks that same non-reentrant mutex:
/// the first call deadlocked the server and chunk streaming stopped after eight packets.
/// The failure mode is a hang, so the call runs on another thread and the test fails
/// (rather than hanging) if it does not return.
#[test]
fn adv_event_returns_with_a_loaded_advancement_set() {
    advancements().lock().unwrap().insert(
        "test:ping".to_string(),
        Advancement {
            title: "ping".to_string(),
            criteria: vec![("crit".to_string(), "tick".to_string(), None)],
        },
    );
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        adv_event("GuardTester", "tick", None);
        adv_event("GuardTester", "tick", None); // the index is built by now
        let _ = tx.send(());
    });
    assert!(
        rx.recv_timeout(std::time::Duration::from_secs(10)).is_ok(),
        "adv_event did not return: the event index build deadlocks on the advancement map"
    );
    advancements().lock().unwrap().remove("test:ping");
    adv_prog().lock().unwrap().remove("GuardTester");
    adv_done().lock().unwrap().remove("GuardTester");
}

/// Captures the globals a world test mutates and restores them when dropped, so a failing assertion
/// cannot leak them into a later test. A leaked hot set was the eight-thread flake: the pins made it
/// deterministic for one test, but a panic on the straight-line restore left it polluted for the
/// next one. Dropping this guard runs the restore on every unwind path.
struct WorldSnapshot {
    world: HashMap<(i32, i32), HashMap<u32, u32>>,
    falling: Vec<Fall>,
    drops: Vec<Drop>,
    clients: Vec<Arc<Client>>,
    forced: HashSet<(i32, i32)>,
    region: crate::RegionSets,
    scheduled_blocks: scheduled_ticks::ScheduledTicks,
    generated_fluids: Option<GeneratedFluidOwner>,
}
impl WorldSnapshot {
    fn take() -> WorldSnapshot {
        WorldSnapshot {
            world: std::mem::take(&mut *world().lock().unwrap()),
            falling: std::mem::take(&mut *falling().lock().unwrap()),
            drops: std::mem::take(&mut *drops().lock().unwrap()),
            clients: std::mem::take(&mut *clients().lock().unwrap()),
            forced: std::mem::take(&mut *forceloaded().lock().unwrap()),
            region: test_region(),
            scheduled_blocks: std::mem::replace(
                &mut *block_ticks().lock().unwrap(),
                scheduled_ticks::ScheduledTicks::new(),
            ),
            generated_fluids: GENERATED_FLUID_OWNER.get().map(|owner| {
                std::mem::replace(&mut *owner.lock().unwrap(), GeneratedFluidOwner::new())
            }),
        }
    }
}
impl std::ops::Drop for WorldSnapshot {
    fn drop(&mut self) {
        // Poison-tolerant: an assertion that fails while holding one of these mutexes poisons it, and
        // `unwrap()` here would panic inside a destructor -- a double panic aborts the whole test
        // binary and skips the restore, the opposite of what this guard is for.
        *world().lock().unwrap_or_else(|e| e.into_inner()) = std::mem::take(&mut self.world);
        *falling().lock().unwrap_or_else(|e| e.into_inner()) = std::mem::take(&mut self.falling);
        *drops().lock().unwrap_or_else(|e| e.into_inner()) = std::mem::take(&mut self.drops);
        *clients().lock().unwrap_or_else(|e| e.into_inner()) = std::mem::take(&mut self.clients);
        *forceloaded().lock().unwrap_or_else(|e| e.into_inner()) = std::mem::take(&mut self.forced);
        set_test_region(self.region.0.clone(), self.region.1.clone());
        *block_ticks().lock().unwrap_or_else(|e| e.into_inner()) = std::mem::replace(
            &mut self.scheduled_blocks,
            scheduled_ticks::ScheduledTicks::new(),
        );
        if let Some(owner) = GENERATED_FLUID_OWNER.get() {
            *owner.lock().unwrap_or_else(|e| e.into_inner()) = self
                .generated_fluids
                .take()
                .unwrap_or_else(GeneratedFluidOwner::new);
        }
    }
}

#[test]
fn scheduled_leaf_distance_matches_native_vectors() {
    let reg = crate::block_registry::breg().unwrap();
    let make = |kind: &str| -> u32 {
        match kind {
            "A" => reg.state_of("minecraft:air", None),
            "S" => reg.state_of("minecraft:stone", None),
            "T" => reg.state_of("minecraft:oak_log", None),
            _ if kind.starts_with('C') => reg.with_prop(
                reg.state_of("minecraft:scaffolding", None),
                "distance",
                &kind[1..],
            ),
            _ => {
                let st = reg.with_prop(
                    reg.state_of("minecraft:oak_leaves", None),
                    "distance",
                    &kind[1..],
                );
                let st = reg.with_prop(
                    st,
                    "persistent",
                    if kind.starts_with(['P', 'B']) {
                        "true"
                    } else {
                        "false"
                    },
                );
                reg.with_prop(
                    st,
                    "waterlogged",
                    if kind.starts_with(['W', 'B']) {
                        "true"
                    } else {
                        "false"
                    },
                )
            }
        }
    };
    let mut count = 0;
    for line in include_str!("../tests/data/leaf_tick_1211.tsv").lines() {
        let fields: Vec<_> = line.split('\t').collect();
        assert_eq!(fields.len(), 4);
        let mut neighbors = [make("A"); 6];
        for (direction, kind) in fields[1].split(',').zip(fields[2].split(',')) {
            neighbors[direction.parse::<usize>().unwrap()] = make(kind);
        }
        assert_eq!(
            leaf_distance_update(reg, make(fields[0]), neighbors),
            make(fields[3]),
            "{line}"
        );
        count += 1;
    }
    assert_eq!(count, 172);
}

#[test]
fn leaf_updates_wait_for_an_eligible_chunk_and_propagate() {
    let _guard = world_guard();
    let _snapshot = WorldSnapshot::take();
    let old_tick = TICK_IDX.load(Ordering::Relaxed);
    let reg = crate::block_registry::breg().unwrap();
    let log = reg.state_of("minecraft:oak_log", None);
    let leaves = reg.with_prop(reg.state_of("minecraft:oak_leaves", None), "distance", "7");
    let p = (2, 220, 0);
    set_test_region(HashSet::new(), HashSet::new());
    set_block(1, 220, 0, leaves);
    set_block(p.0, p.1, p.2, leaves);
    set_block(0, 220, 0, log);
    TICK_IDX.store(old_tick + 1, Ordering::Relaxed);
    scheduled_block_tick();
    assert_eq!(
        reg.get_prop(block_at(1, 220, 0), "distance").as_deref(),
        Some("7")
    );
    set_test_region(HashSet::from([(0, 0)]), HashSet::from([(0, 0)]));
    scheduled_block_tick();
    assert_eq!(
        reg.get_prop(block_at(1, 220, 0), "distance").as_deref(),
        Some("1")
    );
    TICK_IDX.store(old_tick + 2, Ordering::Relaxed);
    scheduled_block_tick();
    assert_eq!(
        reg.get_prop(block_at(p.0, p.1, p.2), "distance").as_deref(),
        Some("2")
    );
    set_block(0, 220, 0, 0);
    for tick in 3..=12 {
        TICK_IDX.store(old_tick + tick, Ordering::Relaxed);
        scheduled_block_tick();
    }
    assert_eq!(
        reg.get_prop(block_at(1, 220, 0), "distance").as_deref(),
        Some("7")
    );
    assert_eq!(
        reg.get_prop(block_at(p.0, p.1, p.2), "distance").as_deref(),
        Some("7")
    );
    TICK_IDX.store(old_tick, Ordering::Relaxed);
}

#[test]
fn pickup_tick_drains_across_selected_offhand_then_hotbar() {
    let _guard = world_guard();
    let _snapshot = WorldSnapshot::take();
    set_test_region(HashSet::from([(0, 0)]), HashSet::new());
    let (client, _receiver) = client_pair();
    *client.held.lock().unwrap() = 4;
    let stone = name_to_item("stone").unwrap();
    *client.inv.lock().unwrap() = [9, 36, 40, 45]
        .into_iter()
        .map(|key| (key, (stone, 60, vec![])))
        .collect();
    let client = Arc::new(client);
    clients().lock().unwrap().push(client.clone());
    drops().lock().unwrap().push(Drop {
        eid: 9999,
        x: 0.0,
        y: 70.0,
        z: 0.0,
        item: stone,
        cnt: 10,
        comps: vec![],
        age: 100,
    });
    drop_tick();
    let remaining: Vec<_> = drops().lock().unwrap().iter().map(|d| d.cnt).collect();
    let actual = client.inv.lock().unwrap().clone();
    assert!(
        remaining.is_empty(),
        "pickup left entity counts {remaining:?}"
    );
    assert_eq!(
        actual,
        HashMap::from([
            (40, (stone, 64, vec![])),
            (45, (stone, 64, vec![])),
            (36, (stone, 62, vec![])),
            (9, (stone, 60, vec![])),
        ])
    );
}

#[test]
fn pickup_tick_keeps_the_partial_entity_without_a_take_event() {
    let _guard = world_guard();
    let _snapshot = WorldSnapshot::take();
    set_test_region(HashSet::from([(0, 0)]), HashSet::new());
    let (client, mut receiver) = client_pair();
    let stone = name_to_item("stone").unwrap();
    let dirt = name_to_item("dirt").unwrap();
    *client.inv.lock().unwrap() = (9..=44).map(|key| (key, (dirt, 64, vec![]))).collect();
    client.inv.lock().unwrap().insert(45, (stone, 60, vec![]));
    client.seen.lock().unwrap().insert(9999);
    let client = Arc::new(client);
    clients().lock().unwrap().push(client.clone());
    drops().lock().unwrap().push(Drop {
        eid: 9999,
        x: 0.0,
        y: 70.0,
        z: 0.0,
        item: stone,
        cnt: 10,
        comps: vec![],
        age: 100,
    });
    drop_tick();
    let remaining: Vec<_> = drops().lock().unwrap().iter().map(|d| d.cnt).collect();
    let offhand = client.inv.lock().unwrap().get(&45).cloned();
    client
        .sock
        .lock()
        .unwrap()
        .shutdown(std::net::Shutdown::Write)
        .unwrap();
    let mut packets = Vec::new();
    while let Some((id, _)) = read_packet(&mut receiver, Instant::now() + Duration::from_secs(1)) {
        packets.push(id);
    }
    assert_eq!(remaining, [6]);
    assert_eq!(offhand, Some((stone, 64, vec![])));
    assert!(
        packets.contains(&0x15) && packets.contains(&0x58),
        "missing slot/entity updates: {packets:?}"
    );
    assert!(
        !packets.contains(&0x6f),
        "partial add returned false but sent take: {packets:?}"
    );
}

#[test]
fn pickup_tick_tries_the_next_player_but_never_inserts_a_stale_stack() {
    let _guard = world_guard();
    let _snapshot = WorldSnapshot::take();
    set_test_region(HashSet::from([(0, 0)]), HashSet::new());
    let (first, _first_rx) = client_pair();
    let (mut second, _second_rx) = client_pair();
    second.eid += 1;
    let first = Arc::new(first);
    let second = Arc::new(second);
    *clients().lock().unwrap() = vec![first.clone(), second.clone()];
    let stone = name_to_item("stone").unwrap();
    // With two eligible players, the live drop is consumed only once.
    for (full, creative) in [(false, false), (true, false), (true, true)] {
        *first.gm.lock().unwrap() = u8::from(creative);
        first.inv.lock().unwrap().clear();
        second.inv.lock().unwrap().clear();
        if full {
            *first.inv.lock().unwrap() = (9..=44).map(|key| (key, (stone, 64, vec![]))).collect();
        }
        drops().lock().unwrap().push(Drop {
            eid: 9999,
            x: 0.0,
            y: 70.0,
            z: 0.0,
            item: stone,
            cnt: 10,
            comps: vec![],
            age: 100,
        });
        drop_tick();
        let remaining = drops().lock().unwrap().len();
        let a = first.inv.lock().unwrap().clone();
        let b = second.inv.lock().unwrap().clone();
        assert_eq!(remaining, 0);
        if full {
            assert_eq!(a[&36].1, 64);
            if creative {
                assert!(
                    b.is_empty(),
                    "creative fallback consumes the entity before the next player"
                );
            } else {
                assert_eq!(b, HashMap::from([(36, (stone, 10, vec![]))]));
            }
        } else {
            assert_eq!(a, HashMap::from([(36, (stone, 10, vec![]))]));
            assert!(b.is_empty(), "a stale candidate duplicated the item");
        }
    }
}

/// Sets a test's saver-thread stop flag when dropped, so a panic does not leave the thread taking
/// `SAVE_LOCK` for the rest of the test binary.
struct StopOnDrop(std::sync::Arc<std::sync::atomic::AtomicBool>);
impl std::ops::Drop for StopOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

fn furnace(input_count: u8) -> Furnace {
    Furnace {
        input: (name_to_item("raw_iron").unwrap(), input_count, Vec::new()),
        fuel: (name_to_item("coal").unwrap(), 64, Vec::new()),
        output: (0, 0, Vec::new()),
        burn_left: 0,
        progress: 0,
        dirty: false,
        xp: 0.0,
        recipe_cache: CachedLookup::default(),
        fuel_cache: CachedLookup::default(),
    }
}

#[test]
fn game_tick_progresses_warm_furnace_and_keeps_cold_furnace_unchanged() {
    let _world_guard = world_guard();
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
        (
            hot.progress,
            hot.burn_left,
            hot.fuel.1,
            cold.progress,
            cold.burn_left,
            cold.fuel.1,
        )
    };
    let clocks = (
        TICK.load(Ordering::Relaxed),
        TICK_IDX.load(Ordering::Relaxed),
        WORLD_TIME.load(Ordering::Relaxed),
    );
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
    let _world_guard = world_guard();
    assert!(clients().lock().unwrap().is_empty());
    let old_actors = std::mem::take(&mut *actors().lock().unwrap());
    let old_forced = std::mem::take(&mut *forceloaded().lock().unwrap());
    let (old_hot, old_warm) = test_region();
    let mut observations = Vec::new();
    refresh_hot();
    observations.push((
        test_region().0,
        test_region().1,
        HashSet::new(),
        HashSet::new(),
    ));
    for name in ["moving", "stationary"] {
        actors().lock().unwrap().insert(
            name.into(),
            Arc::new(Actor {
                name: name.into(),
                eid: 4242,
                pos: Mutex::new((0.0, 70.0, 0.0)),
                look: Mutex::new(None),
                action: Mutex::new(""),
                held: Mutex::new((0, 0, Vec::new())),
                inv: Mutex::new(HashMap::new()),
            }),
        );
    }
    for (i, (x, z, cx, cz)) in [
        (0.0, 0.0, 0, 0),
        (15.999, 15.999, 0, 0),
        (16.0, 16.0, 1, 1),
        (-0.01, -0.01, -1, -1),
        (-16.0, -16.0, -1, -1),
        (-16.01, -16.01, -2, -2),
        (160.0, -160.0, 10, -10),
        (30_000_000.0, -30_000_000.0, 1_875_000, -1_875_000),
    ]
    .into_iter()
    .enumerate()
    {
        *actors().lock().unwrap()["moving"].pos.lock().unwrap() = (x, 70.0, z);
        forceloaded().lock().unwrap().clear();
        let mut expected_hot = HashSet::new();
        if i % 2 == 0 {
            forceloaded().lock().unwrap().insert((200, -200));
            expected_hot.insert((200, -200));
        }
        for (px, pz) in [(cx, cz), (0, 0)] {
            for dx in -5..=5 {
                for dz in -5..=5 {
                    expected_hot.insert((px + dx, pz + dz));
                }
            }
        }
        // Independent oracle: the original hot-then-skirt algorithm.
        let mut expected_warm = expected_hot.clone();
        for &(px, pz) in &expected_hot {
            for dx in -3..=3 {
                for dz in -3..=3 {
                    expected_warm.insert((px + dx, pz + dz));
                }
            }
        }
        refresh_hot();
        observations.push((
            test_region().0,
            test_region().1,
            expected_hot,
            expected_warm,
        ));
    }
    actors().lock().unwrap().clear();
    for x in 0..10 {
        for z in 0..10 {
            let name = format!("dense-{x}-{z}");
            actors().lock().unwrap().insert(
                name.clone(),
                Arc::new(Actor {
                    name,
                    eid: 4242,
                    pos: Mutex::new((16.0 * x as f64, 70.0, 16.0 * z as f64)),
                    look: Mutex::new(None),
                    action: Mutex::new(""),
                    held: Mutex::new((0, 0, Vec::new())),
                    inv: Mutex::new(HashMap::new()),
                }),
            );
        }
    }
    refresh_hot();
    let expected_hot = (-5..=14)
        .flat_map(|x| (-5..=14).map(move |z| (x, z)))
        .collect();
    let expected_warm = (-8..=17)
        .flat_map(|x| (-8..=17).map(move |z| (x, z)))
        .collect();
    observations.push((
        test_region().0,
        test_region().1,
        expected_hot,
        expected_warm,
    ));
    actors().lock().unwrap().clear();
    forceloaded().lock().unwrap().insert((-12, 9));
    refresh_hot();
    let expected_hot = HashSet::from([(-12, 9)]);
    let mut expected_warm = HashSet::new();
    for dx in -3..=3 {
        for dz in -3..=3 {
            expected_warm.insert((-12 + dx, 9 + dz));
        }
    }
    observations.push((
        test_region().0,
        test_region().1,
        expected_hot,
        expected_warm,
    ));
    forceloaded().lock().unwrap().clear();
    refresh_hot();
    observations.push((
        test_region().0,
        test_region().1,
        HashSet::new(),
        HashSet::new(),
    ));
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
    let _world_guard = world_guard();
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
    let _world_guard = world_guard();
    let (client, mut receiver) = client_pair();
    let old_clients = std::mem::take(&mut *clients().lock().unwrap());
    let old_tick = TICK.swap(0, Ordering::Relaxed);
    let old_time = WORLD_TIME.swap(1000, Ordering::Relaxed);
    let old_index = TICK_IDX.swap(0, Ordering::Relaxed);
    let old_actors = std::mem::take(&mut *actors().lock().unwrap());
    let old_forced = std::mem::take(&mut *forceloaded().lock().unwrap());
    let (old_hot, old_warm) = test_region();
    let old_anchors: Vec<_> = [&KEEPALIVE_AT, &TIME_NOTIFY_AT, &SIZE_LOG_AT]
        .iter()
        .map(|cell| *wall_anchor(cell).lock().unwrap())
        .collect();
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
    for _ in 0..40 {
        game_tick(|_, tick| tick());
    }
    let mut seen = Vec::new();
    while let Some((packet, _)) =
        read_packet(&mut receiver, Instant::now() + Duration::from_secs(1))
    {
        if packet == 0x26 || packet == 0x64 {
            seen.push(packet);
        }
    }
    // Rewinding an anchor past its period must make the next tick fire it,
    // which is what proves the mechanism is a wall clock and not a counter.
    *wall_anchor(&KEEPALIVE_AT).lock().unwrap() =
        Instant::now() - Duration::from_millis(KEEPALIVE_MS + 1);
    *wall_anchor(&TIME_NOTIFY_AT).lock().unwrap() =
        Instant::now() - Duration::from_millis(TIME_NOTIFY_MS + 1);
    game_tick(|_, tick| tick());
    let mut fired = Vec::new();
    while let Some((packet, _)) =
        read_packet(&mut receiver, Instant::now() + Duration::from_secs(1))
    {
        if packet == 0x26 || packet == 0x64 {
            fired.push(packet);
        }
    }
    // Restore before asserting so a failure cannot poison the shared fixture.
    *clients().lock().unwrap() = old_clients;
    TICK.store(old_tick, Ordering::Relaxed);
    WORLD_TIME.store(old_time, Ordering::Relaxed);
    TICK_IDX.store(old_index, Ordering::Relaxed);
    *actors().lock().unwrap() = old_actors;
    *forceloaded().lock().unwrap() = old_forced;
    set_test_region(old_hot, old_warm);
    for (cell, anchor) in [&KEEPALIVE_AT, &TIME_NOTIFY_AT, &SIZE_LOG_AT]
        .iter()
        .zip(old_anchors)
    {
        *wall_anchor(cell).lock().unwrap() = anchor;
    }
    assert!(
        seen.is_empty(),
        "tick-count boundaries fired immediately: {seen:?}"
    );
    assert_eq!(
        fired.len(),
        2,
        "expected exactly one keepalive and one time notify: {fired:?}"
    );
    assert_eq!(TICKS_PER_SECOND as u64 * TICK_MS, 1000);
}

#[test]
fn game_tick_periods_preserve_the_vanilla_real_time_interval() {
    // The property that must survive a cadence change: a vanilla interval of N
    // game ticks is N * 50ms, so period * TICK_MS must equal that exactly.
    for game_ticks in [10, 20, 40, 80, 100] {
        assert_eq!(
            server_ticks(game_ticks) * TICK_MS,
            game_ticks * 50,
            "game-tick period {game_ticks} no longer spans {}ms",
            game_ticks * 50
        );
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
        assert_eq!(
            server_ticks(game_ticks) * TICK_MS,
            game_ticks * 50,
            "game-tick duration {} no longer spans {}ms",
            game_ticks,
            game_ticks * 50
        );
    }
    // At the current cadence these are exactly the literals used before.
    assert_eq!(server_ticks(200), 200); // furnace total cook time
    assert_eq!(fuel_ticks(name_to_item("coal").unwrap()), 1600);
    assert_eq!(fuel_ticks(name_to_item("coal_block").unwrap()), 16000);
    assert_eq!(server_ticks(300), 300); // air supply
    assert_eq!(server_ticks(12000), 12000); // rain
                                            // Thunder used to share rain's counter, which was 600s rather than the
                                            // vanilla 180s minimum. Pinning the derived value keeps that fixed.
    assert_eq!(server_ticks(3600), 3600);
    // The lazy-schedule ladder keeps its 0.5s..4s real-time steps.
    assert_eq!(
        (1..=4).map(backoff_ticks).collect::<Vec<_>>(),
        vec![10, 20, 40, 80]
    );
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
        assert_eq!(
            server_ticks(game_ticks),
            expected_ticks,
            "{name} no longer spans its game-tick count"
        );
        assert_eq!(
            server_ticks(game_ticks) * TICK_MS,
            millis,
            "{name} no longer spans {millis}ms"
        );
    }
}

#[test]
fn tnt_fuse_counts_down_in_server_ticks_and_pauses_when_cold() {
    let _world_guard = world_guard();
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
    primed().lock().unwrap().push(Primed {
        eid: 9001,
        x: 0.5,
        y: 70.0,
        z: 0.5,
        left: server_ticks(TNT_FUSE_GT) as i32,
    });
    let mut exploded_at = None;
    for step in 1..=200 {
        game_tick(|_, tick| tick());
        if primed().lock().unwrap().is_empty() {
            exploded_at = Some(step);
            break;
        }
    }
    // Far outside the hot set the fuse must not advance at all, so an unloaded
    // TNT survives indefinitely instead of exploding on a wall-clock timer.
    primed().lock().unwrap().push(Primed {
        eid: 9002,
        x: 30_000_000.0,
        y: 70.0,
        z: 30_000_000.0,
        left: 3,
    });
    for _ in 0..20 {
        game_tick(|_, tick| tick());
    }
    let cold_left = primed()
        .lock()
        .unwrap()
        .iter()
        .find(|p| p.eid == 9002)
        .map(|p| p.left);
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
    assert_eq!(
        exploded_at,
        Some(80),
        "the 4s fuse must fire on its 80th hot tick"
    );
    assert_eq!(cold_left, Some(3), "a cold fuse must not count down");
    // jitter 1 now survives the integer divide, which 4TPS quantised away.
    assert_eq!(chain_range, vec![10, 11, 30, 30, 30]);
}

#[test]
fn entity_life_and_pickup_delay_count_server_ticks_and_freeze_when_cold() {
    let _world_guard = world_guard();
    let old_drops = std::mem::take(&mut *drops().lock().unwrap());
    let old_xps = std::mem::take(&mut *xps().lock().unwrap());
    let old_forced = std::mem::take(&mut *forceloaded().lock().unwrap());
    let old_tick = TICK.swap(1, Ordering::Relaxed);
    let old_index = TICK_IDX.swap(0, Ordering::Relaxed);
    let (old_hot, old_warm) = test_region();
    // The first tick's environment stage reads whatever hot set a previous test left behind; pin
    // it empty so the xp orb is not already hot when this test starts (it is made hot by
    // refresh_hot on the first tick instead).
    set_test_region(HashSet::new(), old_warm.clone());
    // Pin the vanilla game-tick values, not just their 4TPS projection.
    assert_eq!(PICKUP_DELAY_GT, 10);
    assert_eq!(DROP_LIFE_GT, 6000);
    assert_eq!(XP_LIFE_GT, 6000);
    assert_eq!(server_ticks(PICKUP_DELAY_GT) * TICK_MS, 500);
    assert_eq!(server_ticks(DROP_LIFE_GT) * TICK_MS, 300_000);
    forceloaded().lock().unwrap().insert((0, 0));
    drops().lock().unwrap().push(Drop {
        eid: 9101,
        x: 0.5,
        y: 70.0,
        z: 0.5,
        item: 1,
        cnt: 1,
        comps: Vec::new(),
        age: 0,
    });
    xps().lock().unwrap().push(Xp {
        eid: 9102,
        x: 0.5,
        y: 70.0,
        z: 0.5,
        val: 1,
        age: 0,
    });
    // A drop far outside the hot set must not age, so an unloaded world keeps it.
    drops().lock().unwrap().push(Drop {
        eid: 9103,
        x: 30_000_000.0,
        y: 70.0,
        z: 30_000_000.0,
        item: 1,
        cnt: 1,
        comps: Vec::new(),
        age: 0,
    });
    for _ in 0..5 {
        game_tick(|_, tick| tick());
    }
    let hot_age = drops()
        .lock()
        .unwrap()
        .iter()
        .find(|d| d.eid == 9101)
        .map(|d| d.age);
    let cold_age = drops()
        .lock()
        .unwrap()
        .iter()
        .find(|d| d.eid == 9103)
        .map(|d| d.age);
    let xp_age = xps()
        .lock()
        .unwrap()
        .iter()
        .find(|x| x.eid == 9102)
        .map(|x| x.age);
    // A drop past its lifespan must be removed by the despawn pass.
    let life = server_ticks(DROP_LIFE_GT) as i32;
    drops()
        .lock()
        .unwrap()
        .iter_mut()
        .find(|d| d.eid == 9101)
        .unwrap()
        .age = life;
    for _ in 0..2 {
        game_tick(|_, tick| tick());
    }
    let expired = drops().lock().unwrap().iter().all(|d| d.eid != 9101);
    // Pickup must stay blocked until the delay has elapsed in ticks. Use a hot
    // drop: 9103 is deliberately outside the hot set and therefore never ages.
    drops().lock().unwrap().push(Drop {
        eid: 9104,
        x: 0.5,
        y: 70.0,
        z: 0.5,
        item: 1,
        cnt: 1,
        comps: Vec::new(),
        age: 0,
    });
    let mut pickup_ready_at = None;
    for step in 1..=20 {
        game_tick(|_, tick| tick());
        let d = drops()
            .lock()
            .unwrap()
            .iter()
            .find(|d| d.eid == 9104)
            .map(|d| d.age)
            .unwrap_or(0);
        if d >= server_ticks(PICKUP_DELAY_GT) as i32 {
            pickup_ready_at = Some(step);
            break;
        }
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
    assert_eq!(
        xp_age,
        Some(4),
        "a hot xp orb must age once per tick, one stage late"
    );
    assert_eq!(cold_age, Some(0), "a cold drop must not age");
    assert!(expired, "a drop past its lifespan must be removed");
    assert_eq!(
        pickup_ready_at,
        Some(10),
        "the 0.5s pickup delay is 10 server ticks"
    );
}

#[test]
fn air_supply_is_derived_everywhere_a_creature_is_constructed() {
    // The air supply is a duration, so it must come from the cadence owner. A
    // leftover 4TPS literal would give 3s instead of the vanilla 15s, and only
    // the reset paths had been converted, not the construction sites.
    assert_eq!(AIR_SUPPLY_GT, 300);
    assert_eq!(server_ticks(AIR_SUPPLY_GT) * TICK_MS, 15_000);
    let _world_guard = world_guard();
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
    let mut seen: Vec<i32> = mobs()
        .lock()
        .unwrap()
        .iter()
        .map(|m| *m.air.lock().unwrap())
        .collect();
    *mobs().lock().unwrap() = old_mobs;
    *forceloaded().lock().unwrap() = old_forced;
    TICK.store(old_tick, Ordering::Relaxed);
    TICK_IDX.store(old_index, Ordering::Relaxed);
    set_test_region(old_hot, old_warm);
    seen.retain(|air| *air > 0);
    assert!(
        seen.iter().all(|air| *air == expected),
        "air supply was {seen:?}, expected {expected}"
    );
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
        assert_eq!(
            server_ticks(game_ticks) * TICK_MS,
            millis,
            "{name} no longer spans {millis}ms"
        );
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
        assert_eq!(
            server_ticks(game_ticks) * TICK_MS,
            millis,
            "{name} no longer spans {millis}ms"
        );
    }
    // Per-tick probabilities keep the same chance per real second. The rate is
    // ticks per second over the denominator, and it must equal the 4TPS rate.
    for (name, denominator, old_denominator) in [
        ("rain", RAIN_CHANCE_DENOMINATOR, 6000u64),
        ("mob spawn", MOB_SPAWN_CHANCE_DENOMINATOR, 400u64),
    ] {
        assert_eq!(
            TICKS_PER_SECOND as u64 * old_denominator,
            4 * denominator,
            "{name} no longer fires at the same rate per second as at 4TPS"
        );
    }
}

#[test]
fn tick_pacing_holds_cadence_and_refuses_to_catch_up() {
    // The deadline must advance by exactly one interval each tick, so tick work
    // that overruns does not walk the clock later. A plain sleep(TICK_MS) would
    // lose the overrun every tick and drift.
    let start = Instant::now();
    let mut deadline = start;
    for _ in 0..100 {
        deadline += Duration::from_millis(TICK_MS);
    }
    assert_eq!(deadline, start + Duration::from_millis(TICK_MS * 100));
    // Without an overrun the deadline is kept, so cadence does not slip.
    assert_eq!(next_deadline(deadline, false), deadline);
    // With an overrun the deadline is re-anchored to now, so the loop cannot
    // spin to catch up and add more work to an already overloaded server.
    let reanchored = next_deadline(deadline, true);
    assert!(reanchored >= start);
    assert!(reanchored < deadline);
    // The interval itself must be the single cadence owner.
    assert_eq!(
        Duration::from_millis(TICK_MS),
        Duration::from_millis(1000 / TICKS_PER_SECOND as u64)
    );
}

#[test]
#[ignore = "explicit release-profile run from the isolated tick profiler"]
fn profile_game_tick() {
    let _world_guard = world_guard();
    let scratch =
        std::env::var("MC_SPINE_TICK_PROFILE_DIR").expect("use the isolated profiling runner");
    assert_eq!(
        std::env::current_dir().unwrap(),
        std::path::PathBuf::from(scratch)
    );
    assert_ne!(
        std::env::current_dir().unwrap(),
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
    );
    assert!(
        !worldgen::enabled(),
        "this fixture measures the flat/headless tick only"
    );
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
                let (x, y, z) = (
                    0.5 + (i % 3) as f64 * 0.05,
                    70.0,
                    0.5 + (i / 3 % 3) as f64 * 0.05,
                );
                drops().lock().unwrap().push(Drop {
                    eid: 30_000 + i,
                    x,
                    y,
                    z,
                    item: 811,
                    cnt: 1,
                    comps: Vec::new(),
                    age: i,
                });
            }
            for i in 0..600 {
                let (x, y, z) = (
                    400.0 + (i % 20) as f64 * 0.4,
                    70.0,
                    400.0 + (i / 20) as f64 * 0.4,
                );
                drops().lock().unwrap().push(Drop {
                    eid: 31_000 + i,
                    x,
                    y,
                    z,
                    item: 812,
                    cnt: 1,
                    comps: Vec::new(),
                    age: i,
                });
            }
        }
        if scenario.starts_with("entity_heavy") {
            for i in 0..600 {
                let (x, y, z) = (16.0 * (i % 20) as f64, 70.0, 16.0 * (i / 20) as f64);
                drops().lock().unwrap().push(Drop {
                    eid: 20_000 + i,
                    x,
                    y,
                    z,
                    item: 811,
                    cnt: 1,
                    comps: Vec::new(),
                    age: (i % 400),
                });
                xps().lock().unwrap().push(Xp {
                    eid: 21_000 + i,
                    x,
                    y,
                    z,
                    val: 1,
                    age: (i % 400),
                });
                crops()
                    .lock()
                    .unwrap()
                    .insert((x as i32, y as i32, z as i32), (i % 7) as u8);
                mobs().lock().unwrap().push(Arc::new(Mob {
                    eid: 22_000 + i,
                    uuid: [0u8; 16],
                    etype: 124,
                    pos: Mutex::new((x + 1.0, y, z, 0.0, 0.0)),
                    hp: Mutex::new(20.0),
                    last_hit: Mutex::new(Instant::now()),
                    hurt_at: Mutex::new(Instant::now()),
                    fuse: Mutex::new(0),
                    love: Mutex::new(0),
                    air: Mutex::new(server_ticks(AIR_SUPPLY_GT) as i32),
                }));
            }
        }
        TICK.store(1, Ordering::Relaxed);
        TICK_IDX.store(0, Ordering::Relaxed);
        WORLD_TIME.store(1000, Ordering::Relaxed);
        for i in 0..actor_count {
            let name = format!("tick-profile-{i}");
            let pos = if scenario == "dense_actors" {
                (16.0 * (i % 10) as f64, 70.0, 16.0 * (i / 10) as f64)
            } else {
                (0.0, 70.0, 0.0)
            };
            actors().lock().unwrap().insert(
                name.clone(),
                Arc::new(Actor {
                    name,
                    eid: 4242 + i,
                    pos: Mutex::new(pos),
                    look: Mutex::new(None),
                    action: Mutex::new(""),
                    held: Mutex::new((0, 0, Vec::new())),
                    inv: Mutex::new(HashMap::new()),
                }),
            );
        }
        for i in 0..machines {
            furnaces().lock().unwrap().insert(
                (i % 16, 70 + i / 256, (i / 16) % 16),
                furnace(if active { 64 } else { 0 }),
            );
        }
        for _ in 0..100 {
            game_tick(|_, tick| tick());
        }
        let mut totals: std::collections::BTreeMap<&str, (u128, u128, usize)> =
            std::collections::BTreeMap::new();
        let mut samples = Vec::with_capacity(1000);
        for _ in 0..1000 {
            let start = Instant::now();
            game_tick(|name, tick| {
                let begin = Instant::now();
                tick();
                let elapsed = begin.elapsed().as_nanos();
                let item = totals.entry(name).or_default();
                item.0 += elapsed;
                item.1 = item.1.max(elapsed);
                item.2 += 1;
            });
            samples.push(start.elapsed().as_nanos());
        }
        if machines > 0 {
            // Assert the machines actually ran, so a faster measurement cannot be
            // a stage that silently stopped doing work. The counts themselves are
            // cadence dependent, so bound them rather than pin them.
            let fs = furnaces().lock().unwrap();
            for f in fs.values() {
                assert_eq!(
                    (
                        f.output.1 > 0,
                        f.progress > 0,
                        f.burn_left > 0,
                        f.fuel.1 < 64
                    ),
                    if active {
                        (true, true, true, true)
                    } else {
                        (false, false, false, false)
                    }
                );
            }
        }
        samples.sort_unstable();
        println!(
            "PROFILE\t{scenario}\ttick\t{}\t{}\t{}\t{}\t{}",
            samples.len(),
            samples.iter().sum::<u128>(),
            samples[499],
            samples[949],
            samples[989]
        );
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
    let _world_guard = world_guard();
    let old_arrows = std::mem::take(&mut *arrows().lock().unwrap());
    let old_forced = std::mem::take(&mut *forceloaded().lock().unwrap());
    let old_tick = TICK.swap(1, Ordering::Relaxed);
    let old_index = TICK_IDX.swap(0, Ordering::Relaxed);
    let (old_hot, old_warm) = test_region();
    forceloaded().lock().unwrap().insert((0, 0));
    arrows().lock().unwrap().push(Arrow {
        eid: 8001,
        owner: 1,
        x: 0.5,
        y: 120.0,
        z: 0.5,
        dx: 1.0,
        dy: 0.0,
        dz: 0.0,
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
    let (mut prev_x, mut prev_v) = (0.5f64, 1.0f64);
    let mut steps = Vec::new();
    for _ in 0..5 {
        game_tick(|_, tick| tick());
        let a = arrows().lock().unwrap();
        let Some(a) = a.iter().find(|a| a.eid == 8001) else {
            break;
        };
        let (x, v) = (a.x, a.dx);
        steps.push((x - prev_x, v));
        prev_x = x;
        prev_v = v;
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
        assert!(
            (*velocity - expected).abs() < 1e-9,
            "velocity {velocity} != {expected}"
        );
        assert!(
            (*step - *velocity).abs() < 1e-9,
            "step {step} != velocity {velocity}"
        );
        assert!(
            *step < 1.0,
            "the step must be the damped velocity, not the initial one: {step}"
        );
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
    let _world_guard = world_guard();
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
                    let (wx, wy, wz) = (
                        cx * 16 + (*idx & 15) as i32,
                        (*idx >> 8) as i32 + MIN_Y,
                        cz * 16 + ((*idx >> 4) & 15) as i32,
                    );
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
    for (y, st) in original {
        set_block(0, y, 0, st);
    }
    *waters().lock().unwrap() = old_waters;
    *forceloaded().lock().unwrap() = old_forced;
    TICK.store(old_tick, Ordering::Relaxed);
    TICK_IDX.store(old_index, Ordering::Relaxed);
    set_test_region(old_hot, old_warm);
    // The scenario must actually do something, or the trace proves nothing.
    assert!(flooded > 3, "the source barely spread: {flooded} cells");
    assert!(max_level >= 1, "no flowing level appeared");
    let per_tick: Vec<(usize, u32)> = trace
        .iter()
        .map(|c| (c.len(), c.iter().map(|x| x.3).max().unwrap_or(0)))
        .collect();
    let expected_per_tick: Vec<(usize, u32)> = (2..=41).map(|n| (n, 8u32)).collect();
    // The source keeps level 0 and the falling column below it is level 8.
    let mut expected_final: Vec<(i32, i32, i32, u32)> =
        (110..=149).map(|y| (0, y, 0, 8u32)).collect();
    expected_final.push((0, 150, 0, 0));
    assert_eq!(
        per_tick, expected_per_tick,
        "the fluid scheduler changed how many cells run or when they run"
    );
    let mut actual = trace.last().unwrap().clone();
    let mut exp = expected_final.clone();
    actual.sort_unstable();
    exp.sort_unstable();
    let missing: Vec<_> = exp.iter().filter(|c| !actual.contains(c)).collect();
    let extra: Vec<_> = actual.iter().filter(|c| !exp.contains(c)).collect();
    assert_eq!((missing, extra), (vec![], vec![]), "flooded set differs");
}

/// Pins hopper item movement tick by tick, for the same reason as the water
/// trace: the scheduler migration replaces the sweep of every registered hopper,
/// and a missed re-queue would silently stop a hopper moving items.
#[test]
fn hopper_transfer_trace_is_stable_across_the_scheduler_migration() {
    let _world_guard = world_guard();
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
    chests()
        .lock()
        .unwrap()
        .entry((0, hy, 0))
        .or_default()
        .insert(0, (item, 3, Vec::new()));
    let mut trace: Vec<(u8, u8)> = Vec::new();
    for _ in 0..24 {
        game_tick(|_, tick| tick());
        let own = chests()
            .lock()
            .unwrap()
            .get(&(0, hy, 0))
            .map(|m| m.values().map(|v| v.1).sum::<u8>())
            .unwrap_or(0);
        let dst = chests()
            .lock()
            .unwrap()
            .get(&(0, hy - 1, 0))
            .map(|m| m.values().map(|v| v.1).sum::<u8>())
            .unwrap_or(0);
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
        .into_iter()
        .chain(std::iter::repeat_n((0, 3), 21))
        .collect();
    assert_eq!(
        trace, expected,
        "the hopper scheduler changed what moves, or when"
    );
}

/// Pins lava flow for the same reason as water and hoppers. Lava's cadence is the
/// complication: lava_tick runs once per server_ticks(FLUID_TICK_GT), while the
/// queue advances one step per due() call, so the delay must be expressed in
/// invocations. Because a cell can only ever be visited on those boundaries,
/// "due at tick T" and "first visit at or after T" are the same thing, and the
/// conversion is exact rather than an approximation.
#[test]
fn lava_spread_trace_is_stable_across_the_scheduler_migration() {
    let _world_guard = world_guard();
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
    for (y, st) in original {
        set_block(0, y, 0, st);
    }
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
    let _world_guard = world_guard();
    let pos = (3, 70, 3);
    // Park the cell far out in the queue, as a backed-off cell would be.
    water_queue()
        .lock()
        .unwrap()
        .schedule(pos, tick_queue::MAX_DELAY);
    assert!(
        !water_queue().lock().unwrap().is_empty(),
        "the cell must be waiting"
    );
    // A block change next to it must pull it forward.
    sched_invalidate(pos.0 + 1, pos.1, pos.2);
    // drain one tick's worth: the promoted cell must be in it
    let mut found = false;
    for _ in 0..2 {
        let due = {
            let mut q = water_queue().lock().unwrap();
            q.due()
        };
        if due.contains(&pos) {
            found = true;
            break;
        }
    }
    // clean up whatever is left
    water_queue().lock().unwrap().cancel(pos);
    assert!(
        found,
        "a neighbour change must wake a waiting cell within two ticks"
    );
}

/// The spread traces cannot cover this: a flowing cell returns before it is ever
/// backed off, so its re-queue always takes the no-backoff branch where the period
/// does not matter. Replacing the period with 1 left every trace green, which is
/// the gap this closes. The conversion itself is what a periodic system gets wrong
/// (lava was thirty times too slow before), so it is pinned directly.
#[test]
fn a_periodic_system_expresses_its_backoff_in_invocations() {
    let _world_guard = world_guard();
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
            let due = {
                let mut q = lava_queue().lock().unwrap();
                q.due()
            };
            if due.contains(&pos) {
                return n;
            }
        }
        panic!("the cell never came due");
    };
    // A backoff inside the next period still lands on the next invocation, because
    // that is the earliest moment the system can visit it at all.
    assert_eq!(invocations(1), invocations(period), "within one period");
    assert_eq!(
        invocations(period + 1),
        invocations(2 * period),
        "one period later"
    );
    assert_eq!(
        invocations(2 * period + 1),
        invocations(3 * period),
        "two periods later"
    );
    assert!(
        invocations(period + 1) > invocations(period),
        "crossing a period boundary must cost an invocation"
    );
    lava_queue().lock().unwrap().cancel(pos);
    backoff().lock().unwrap().remove(&pos);
}

/// The delay queue caps a wait at four seconds, but the backoff ladder does not:
/// after four consecutive no-op visits `backoff_ticks` returns more than
/// `MAX_DELAY`, so the queue on its own would visit the cell early. The
/// `sched_ready` guard is what holds the cell to its real deadline, which makes it
/// load-bearing rather than the "cannot fire" net a reachability model suggested.
/// This drives the real `game_tick`/`water_tick` path and reads the visit times
/// off the backoff map's fail counter, because the guard re-queues without
/// touching that counter.
#[test]
fn a_backoff_past_the_queue_cap_waits_for_its_real_deadline() {
    let _world_guard = world_guard();
    let old_waters = std::mem::take(&mut *waters().lock().unwrap());
    let old_queue = std::mem::replace(
        &mut *water_queue().lock().unwrap(),
        tick_queue::TickQueue::new(),
    );
    let old_backoff = std::mem::take(&mut *backoff().lock().unwrap());
    let old_forced = std::mem::take(&mut *forceloaded().lock().unwrap());
    let old_tick = TICK.swap(1, Ordering::Relaxed);
    let old_index = TICK_IDX.swap(0, Ordering::Relaxed);
    let (old_hot, old_warm) = test_region();
    forceloaded().lock().unwrap().insert((0, 0));
    refresh_hot();

    assert!(
        backoff_ticks(5) > tick_queue::MAX_DELAY as u64,
        "this test is vacuous unless the ladder exceeds the queue cap"
    );
    let pos = (0, 150, 0);
    let stone = name_to_state("stone").unwrap();
    let water = name_to_state("water").unwrap();
    let cells = [
        (0, 150, 0),
        (0, 149, 0),
        (1, 150, 0),
        (-1, 150, 0),
        (0, 150, 1),
        (0, 150, -1),
    ];
    let original: Vec<(i32, i32, i32, u32)> = cells
        .iter()
        .map(|&(x, y, z)| (x, y, z, block_at(x, y, z)))
        .collect();
    for &(x, y, z) in &cells[1..] {
        set_block(x, y, z, stone);
    }
    // Placing the water registers and schedules it. Walled in on every side it has
    // no work to do, so each visit is a no-op and the ladder climbs past the cap.
    set_block(pos.0, pos.1, pos.2, water);

    let mut visits: Vec<u64> = Vec::new();
    let mut fails = 0u32;
    for t in 0..=700u64 {
        game_tick(|_, tick| tick());
        if let Some((_, f)) = backoff().lock().unwrap().get(&pos).copied() {
            if f != fails {
                fails = f;
                visits.push(t);
            }
        }
    }
    for (x, y, z, st) in original {
        set_block(x, y, z, st);
    }
    *waters().lock().unwrap() = old_waters;
    *water_queue().lock().unwrap() = old_queue;
    *backoff().lock().unwrap() = old_backoff;
    *forceloaded().lock().unwrap() = old_forced;
    TICK.store(old_tick, Ordering::Relaxed);
    TICK_IDX.store(old_index, Ordering::Relaxed);
    set_test_region(old_hot, old_warm);

    let deltas: Vec<u64> = visits.windows(2).map(|w| w[1] - w[0]).collect();
    assert!(
        deltas.len() >= 6,
        "the ladder must climb past the cap: {visits:?}"
    );
    let expected: Vec<u64> = (1..=deltas.len() as u64).map(backoff_ticks).collect();
    assert_eq!(
        deltas, expected,
        "the guard must hold each visit to the real backoff, not the queue's 4s cap: {visits:?}"
    );
}

/// Vanilla grows sugar cane one stage per random tick and adds a block on the tick that
/// would pass age 15, so sixteen random ticks raise the column by one. That makes it the
/// one behaviour a random tick can drive that can be pinned exactly, which is why it is
/// the first consumer of the stream.
#[test]
fn sixteen_random_ticks_raise_sugar_cane_one_block() {
    crate::test_support::require_vanilla_data();
    let _world_guard = world_guard();
    let old_forced = std::mem::take(&mut *forceloaded().lock().unwrap());
    let (old_hot, old_warm) = test_region();
    let top = 150;
    let cane = name_to_state("sugar_cane").expect("sugar_cane must exist");
    let original: Vec<(i32, u32)> = (top..=top + 4).map(|y| (y, block_at(0, y, 0))).collect();
    for y in top..=top + 4 {
        set_block(0, y, 0, 0);
    }
    set_block(0, top, 0, cane);

    let reg = crate::block_registry::breg().expect("the block registry must load from assets/");
    let tick = || on_random_tick(crate::block_registry::breg().unwrap(), 0, top, 0);
    let age = |y: i32| {
        reg.get_prop(block_at(0, y, 0), "age")
            .and_then(|a| a.parse::<u32>().ok())
    };
    let is_cane = |y: i32| reg.name_of(block_at(0, y, 0)) == "minecraft:sugar_cane";

    assert!(
        is_cane(top) && !is_cane(top + 1),
        "the test must start with one cane"
    );
    assert_eq!(age(top), Some(0), "a placed cane starts at age 0");

    // Fifteen ticks walk the age up without adding a block.
    for i in 0..15 {
        tick();
        assert!(
            !is_cane(top + 1),
            "no block may appear before the sixteenth tick (tick {i})"
        );
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
        on_random_tick(crate::block_registry::breg().unwrap(), 0, top + 1, 0);
    }
    assert!(
        is_cane(top + 2) && !is_cane(top + 3),
        "the column must stop at height 3"
    );
    for _ in 0..40 {
        tick();
        on_random_tick(crate::block_registry::breg().unwrap(), 0, top + 1, 0);
        on_random_tick(crate::block_registry::breg().unwrap(), 0, top + 2, 0);
    }
    assert!(
        !is_cane(top + 3),
        "a column of three must not grow a fourth"
    );

    for (y, st) in original {
        set_block(0, y, 0, st);
    }
    *forceloaded().lock().unwrap() = old_forced;
    set_test_region(old_hot, old_warm);
}

/// The two light computations run per chunk send, which the tick fixture never reaches, so
/// they are measured here directly. Run with --nocapture to see the numbers. The point of the
/// pair is the difference: with no light source in the chunk the block light spread must do
/// nothing, and that is what makes it safe to compute unconditionally.
#[test]
fn measure_the_light_cost_per_chunk() {
    crate::test_support::require_vanilla_data();
    let _g = world_guard();
    let old_forced = std::mem::take(&mut *forceloaded().lock().unwrap());
    forceloaded().lock().unwrap().insert((0, 0));
    refresh_hot();

    let time_it = |f: &mut dyn FnMut()| {
        let t = std::time::Instant::now();
        f();
        t.elapsed()
    };
    let rounds = 200;

    let sky = time_it(&mut || {
        for _ in 0..rounds {
            std::hint::black_box(sky_light_sections(0, 0));
        }
    });
    let gen = GenSources::new();
    let none = time_it(&mut || {
        for _ in 0..rounds {
            std::hint::black_box(block_light_sections(0, 0, &gen));
        }
    });

    // With a torch placed, the block light spread has something to do.
    let (tx, ty, tz) = (5, height_at(5, 7), 7);
    let old = block_at(tx, ty, tz);
    set_block(
        tx,
        ty,
        tz,
        name_to_state("torch").expect("torch must exist"),
    );
    let lit = time_it(&mut || {
        for _ in 0..rounds {
            std::hint::black_box(block_light_sections(0, 0, &gen));
        }
    });
    set_block(tx, ty, tz, old);

    *forceloaded().lock().unwrap() = old_forced;

    // Split the sky cost: the surface lookup and the walk down through block_at are the two
    // candidates, and which one dominates decides what to fix.
    let heights = time_it(&mut || {
        for _ in 0..rounds {
            for lz in 0..16 {
                for lx in 0..16 {
                    std::hint::black_box(height_at(lx, lz));
                }
            }
        }
    });
    let walks = time_it(&mut || {
        for _ in 0..rounds {
            let reg = crate::block_registry::breg().unwrap();
            for lz in 0..16 {
                for lx in 0..16 {
                    let mut y = height_at(lx, lz) - 1;
                    let mut steps = 0;
                    while steps < 16 {
                        if reg.opacity_of(block_at(lx, y, lz)) > 0 {
                            break;
                        }
                        y -= 1;
                        steps += 1;
                    }
                }
            }
        }
    });
    // The two halves of sky_light_sections, measured apart: building the floors, and packing
    // them. The split so far did not add up, and guessing which half was missing has been
    // wrong twice, so both are timed with the same inputs the real call uses.
    let reg = crate::block_registry::breg().unwrap();
    let mut real_floors = [0i32; 256];
    for lz in 0..16usize {
        for lx in 0..16usize {
            let mut y = height_at(lx as i32, lz as i32) - 1;
            let mut steps = 0;
            while steps < 16 {
                if reg.opacity_of(block_at(lx as i32, y, lz as i32)) > 0 {
                    break;
                }
                y -= 1;
                steps += 1;
            }
            real_floors[(lz << 4) | lx] = y;
        }
    }
    let floors_only = time_it(&mut || {
        for _ in 0..rounds {
            for lz in 0..16usize {
                for lx in 0..16usize {
                    let mut y = height_at(lx as i32, lz as i32) - 1;
                    let mut steps = 0;
                    while steps < 16 {
                        if reg.opacity_of(block_at(lx as i32, y, lz as i32)) > 0 {
                            break;
                        }
                        y -= 1;
                        steps += 1;
                    }
                    std::hint::black_box(y);
                }
            }
        }
    });
    let pack_only = time_it(&mut || {
        for _ in 0..rounds {
            std::hint::black_box(sky_light_sections_from(0, 0, &real_floors));
        }
    });
    // The halves did not sum to the whole, so the suspicion is that black_box on a Vec did not
    // force the bytes to be produced - black_box sees the pointer, not the 53 kilobytes behind
    // it. Reading every byte into a checksum removes that doubt: if the packing was being
    // optimised away, this is where it shows up.
    let checksum = |v: &Vec<[u8; 2048]>| -> u64 {
        let mut acc = 0u64;
        for s in v {
            for b in s.iter() {
                acc = acc.wrapping_add(*b as u64);
            }
        }
        acc
    };
    let pack_read = time_it(&mut || {
        for _ in 0..rounds {
            std::hint::black_box(checksum(&sky_light_sections_from(0, 0, &real_floors)));
        }
    });
    let sky_read = time_it(&mut || {
        for _ in 0..rounds {
            std::hint::black_box(checksum(&sky_light_sections(0, 0)));
        }
    });
    // The same rig for block light, which is where the earlier numbers came from and where
    // none of them were trustworthy. Reading the bytes is what makes this one comparable.
    let gen = GenSources::new();
    let block_read = time_it(&mut || {
        for _ in 0..rounds {
            std::hint::black_box(checksum(&block_light_sections(0, 0, &gen)));
        }
    });
    set_block(
        tx,
        ty,
        tz,
        name_to_state("torch").expect("torch must exist"),
    );
    let block_read_lit = time_it(&mut || {
        for _ in 0..rounds {
            std::hint::black_box(checksum(&block_light_sections(0, 0, &gen)));
        }
    });
    set_block(tx, ty, tz, old);

    // Where the block light spread's time goes: the same sources with an opacity function that
    // reads nothing, against the one that reads the world through block_at.
    let free_op = time_it(&mut || {
        for _ in 0..rounds {
            // one sample per round: this used to be a degenerate
            // `for x in -16..32 { for z in -16..32 { ..; break } break }`,
            // which clippy reads as a loop that never loops.
            let _ = std::hint::black_box(block_light_sections_from(
                0,
                0,
                &[((5, height_at(5, 7), 7), 15)],
                |_, _, _| 0,
            ));
        }
    });
    println!(
        "LIGHT_READ per chunk over {rounds} rounds: pack_read={pack_read:?} sky_read={sky_read:?} block_read={block_read:?} block_read_lit={block_read_lit:?} free_op={free_op:?}"
    );
    println!(
        "LIGHT_HALVES per chunk over {rounds} rounds: floors_only={floors_only:?} pack_only={pack_only:?}"
    );
    println!(
        "LIGHT_SPLIT per chunk over {rounds} rounds: surface_lookup={heights:?} surface_plus_walk={walks:?}"
    );
    println!(
        "LIGHT_COST per chunk over {rounds} rounds: sky={sky:?} block_no_source={none:?} block_with_torch={lit:?}"
    );
    // No source, no work: the spread must not be doing a scan per chunk.
    assert!(
        none.as_nanos() < lit.as_nanos(),
        "a chunk with no source must be cheaper than one with a torch ({none:?} vs {lit:?})"
    );
}

/// Sand over air falls as a vanilla falling-block entity: the block leaves the world, the
/// entity integrates `dy -= 0.04` per tick (`y_n = y0 - 0.02 n(n+1)`), and it lands on the
/// first solid block below. The bytecode contract is evidence/gameplay/falling-vanilla.
#[test]
fn sand_falls_under_vanilla_gravity_until_it_lands() {
    crate::test_support::require_vanilla_data();
    let _world_guard = world_guard();
    let old_falling = std::mem::take(&mut *falling().lock().unwrap());
    let old_forced = std::mem::take(&mut *forceloaded().lock().unwrap());
    let (oh, ow) = test_region();
    let mut warm = ow.clone();
    warm.extend([(0, 0), (1, 0), (2, 0), (3, 0), (4, 0)]);
    set_test_region(oh.clone(), warm);
    let sand = name_to_state("sand").expect("sand must exist");
    let base = 150;
    let original: Vec<(i32, u32)> = (base..=base + 4).map(|y| (y, block_at(3, y, 3))).collect();
    for y in base..=base + 4 {
        set_block(3, y, 3, 0);
    }
    set_block(
        3,
        base,
        3,
        name_to_state("stone").expect("stone must exist"),
    );
    set_block(
        3,
        base + 2,
        3,
        name_to_state("dirt").expect("dirt must exist"),
    );
    set_block(3, base + 3, 3, sand);
    assert!(
        falling().lock().unwrap().is_empty(),
        "supported sand must not fall"
    );

    let name = |y: i32| {
        crate::block_registry::breg()
            .unwrap()
            .name_of(block_at(3, y, 3))
            .to_string()
    };
    // Removing the support converts the block into an entity in the same call; the block does
    // not stay in the world while it falls.
    set_block(3, base + 2, 3, 0);
    assert_eq!(
        block_at(3, base + 3, 3),
        0,
        "the block must leave the world"
    );
    let entity = falling().lock().unwrap().clone();
    assert_eq!(entity.len(), 1, "exactly one falling entity");
    assert_eq!(entity[0].state, sand, "the entity carries the block state");
    assert!(
        (entity[0].y - (base + 3) as f64).abs() < 1e-9,
        "it starts at the block cell"
    );

    // The exact gravity trace, tick by tick, for the nine airborne ticks.
    let mut expected = (base + 3) as f64;
    for n in 1..=9 {
        falling_tick();
        expected -= 0.04 * n as f64;
        let cur = falling().lock().unwrap().clone();
        assert_eq!(cur.len(), 1, "still in flight at tick {n}");
        assert!(
            (cur[0].y - expected).abs() < 1e-9,
            "tick {n}: y={} but vanilla gravity gives {expected}",
            cur[0].y
        );
    }
    // Tick ten reaches the floor's top face and places the block there.
    falling_tick();
    assert!(
        falling().lock().unwrap().is_empty(),
        "the entity landed and despawned"
    );
    assert_eq!(
        name(base + 1),
        "minecraft:sand",
        "the block is placed on the floor"
    );
    assert_eq!(name(base + 2), "minecraft:air", "nothing is left above it");

    for (y, st) in original {
        set_block(3, y, 3, st);
    }
    *falling().lock().unwrap() = old_falling;
    *forceloaded().lock().unwrap() = old_forced;
    set_test_region(oh, ow);
}

/// A fall that moves more than one block per tick must still land on a one-block platform it
/// crosses. The support scan starts at the old feet and sweeps down to the new feet, so it sees
/// a platform whose cell is entirely above `floor(new_y)`.
#[test]
fn a_fast_falling_block_lands_on_a_thin_platform_it_crosses() {
    crate::test_support::require_vanilla_data();
    let _world_guard = world_guard();
    let old_falling = std::mem::take(&mut *falling().lock().unwrap());
    let old_forced = std::mem::take(&mut *forceloaded().lock().unwrap());
    let (oh, ow) = test_region();
    let mut warm = ow.clone();
    warm.extend([(0, 0), (1, 0), (2, 0), (3, 0), (4, 0)]);
    set_test_region(oh.clone(), warm);
    let sand = name_to_state("sand").expect("sand must exist");
    let stone = name_to_state("stone").expect("stone must exist");
    let start = 150;
    let original: Vec<(i32, u32)> = (60..=start).map(|y| (y, block_at(3, y, 3))).collect();
    for y in 60..=start {
        set_block(3, y, 3, 0);
    }
    set_block(3, 60, 3, stone); // the world floor below the platform
    set_block(3, 97, 3, stone); // one-block platform at 97 (top face 98)
    set_block(3, start, 3, sand);

    // Tick 50 leaves the feet at y=99 (above the platform); tick 51 moves to y=96.96, whose
    // floor is below the platform. A scan starting at the new cell would pass through it.
    for _ in 0..51 {
        falling_tick();
    }
    assert!(
        falling().lock().unwrap().is_empty(),
        "the entity must have landed"
    );
    let name = |y: i32| {
        crate::block_registry::breg()
            .unwrap()
            .name_of(block_at(3, y, 3))
            .to_string()
    };
    assert_eq!(
        name(98),
        "minecraft:sand",
        "it lands on the platform's top face"
    );
    assert_eq!(name(97), "minecraft:stone", "the platform is untouched");

    for (y, st) in original {
        set_block(3, y, 3, st);
    }
    *falling().lock().unwrap() = old_falling;
    *forceloaded().lock().unwrap() = old_forced;
    set_test_region(oh, ow);
}

/// A tick must not drain the falling queue: a save on another thread clones the queue during
/// the pass, and an empty queue would drop blocks that are already out of the world.
#[test]
fn beginning_a_falling_tick_leaves_the_queue_visible() {
    let _world_guard = world_guard();
    let old_falling = std::mem::take(&mut *falling().lock().unwrap());
    falling().lock().unwrap().push(Fall {
        eid: 7,
        x: 0.5,
        y: 80.0,
        z: 0.5,
        dy: 0.0,
        state: 1,
        time: 0,
    });
    let (active, processed) = begin_falling_tick();
    assert_eq!(active.len(), 1, "the tick advances the in-flight entity");
    assert_eq!(processed.len(), 1);
    assert_eq!(
        falling().lock().unwrap().len(),
        1,
        "a concurrent save must still see the in-flight entity"
    );
    *falling().lock().unwrap() = old_falling;
}

/// A save that runs while blocks are falling must not lose or duplicate them. A saver thread
/// snapshots under `SAVE_LOCK` throughout the fall, and a snapshot taken mid-fall is reloaded and
/// ticked to completion: every starting block must reappear exactly once.
#[test]
fn a_save_concurrent_with_falling_conserves_every_block() {
    crate::test_support::require_vanilla_data();
    let _world_guard = world_guard();
    let _snap = WorldSnapshot::take();
    let (oh, ow) = test_region();
    let mut warm = ow.clone();
    warm.extend([(0, 0), (1, 0), (2, 0), (3, 0), (4, 0)]);
    set_test_region(oh.clone(), warm);
    let sand = name_to_state("sand").expect("sand must exist");
    let stone = name_to_state("stone").expect("stone must exist");
    let base = 150;
    let height = 4;
    let columns = [(1, 1), (2, 1), (3, 1), (1, 2), (2, 2)];
    let expected = columns.len() * height as usize;
    for &(x, z) in &columns {
        for y in base..=base + height + 2 {
            set_block(x, y, z, 0);
        }
        set_block(x, base, z, stone);
        set_block(x, base + 1, z, name_to_state("dirt").unwrap());
        for i in 1..=height {
            set_block(x, base + 1 + i, z, sand);
        }
    }

    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let _stop_guard = StopOnDrop(stop.clone());
    let snapshots_store = std::sync::Arc::new(std::sync::Mutex::new(Vec::<Vec<u8>>::new()));
    let saver = {
        let stop = stop.clone();
        let snapshots_store = snapshots_store.clone();
        std::thread::spawn(move || {
            let mut n = 0u32;
            while !stop.load(Ordering::Relaxed) {
                let _lock = SAVE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
                let mut buf = Vec::new();
                save_world_into(&mut buf);
                let mut store = snapshots_store.lock().unwrap();
                if store.len() < 16 {
                    store.push(buf);
                }
                drop(store);
                n += 1;
            }
            n
        })
    };

    // Knock out every support: all blocks become entities.
    for &(x, z) in &columns {
        set_block(x, base + 1, z, 0);
    }
    assert_eq!(
        falling().lock().unwrap().len(),
        expected,
        "every block becomes an entity"
    );
    let mut ticks = 0;
    while !falling().lock().unwrap().is_empty() && ticks < 200 {
        falling_tick();
        ticks += 1;
    }
    assert!(ticks < 200, "every entity must land");
    stop.store(true, Ordering::Relaxed);
    let saves = saver.join().expect("saver thread");
    assert!(saves > 0, "a save must have run during the fall");

    let name = |x: i32, y: i32, z: i32| {
        crate::block_registry::breg()
            .unwrap()
            .name_of(block_at(x, y, z))
            .to_string()
    };
    let placed = |columns: &[(i32, i32)]| -> usize {
        let mut n = 0;
        for &(x, z) in columns {
            for y in base..=base + height + height {
                if name(x, y, z) == "minecraft:sand" {
                    n += 1;
                }
            }
        }
        n
    };
    assert_eq!(
        placed(&columns),
        expected,
        "live: no block is lost or duplicated"
    );
    assert!(
        falling().lock().unwrap().is_empty(),
        "nothing is left in flight"
    );
    assert!(
        drops().lock().unwrap().is_empty(),
        "landed blocks do not drop"
    );

    // Reload every concurrent snapshot into a clean state and land it: each must conserve its
    // blocks. A snapshot taken mid-transition would drop or duplicate one.
    let saved = snapshots_store.lock().unwrap().clone();
    assert!(!saved.is_empty(), "the saver stored a snapshot");
    for (i, snap) in saved.iter().enumerate() {
        world().lock().unwrap().clear();
        falling().lock().unwrap().clear();
        drops().lock().unwrap().clear();
        load_world_from(snap).unwrap();
        let mut t = 0;
        while !falling().lock().unwrap().is_empty() && t < 200 {
            falling_tick();
            t += 1;
        }
        assert_eq!(
            placed(&columns),
            expected,
            "snapshot {i} loses or duplicates a block"
        );
    }
}

/// A block that falls out of the world becomes exactly one item; a save racing that
/// transition must neither duplicate nor lose the item when its snapshot is reloaded.
#[test]
fn a_saved_below_world_drop_is_not_duplicated() {
    crate::test_support::require_vanilla_data();
    let _world_guard = world_guard();
    let _snap = WorldSnapshot::take();
    let (_, ow) = test_region();
    let mut warm = ow.clone();
    warm.extend([(0, 0), (1, 0)]);
    // Falling does not depend on the hot set, so start from an empty one rather than whatever a
    // previous test left, and restore the ambient region at the end.
    set_test_region(HashSet::new(), warm);
    let sand = name_to_state("sand").expect("sand must exist");
    let sand_item = name_to_item("sand").expect("sand item must exist");
    let stone = name_to_state("stone").expect("stone must exist");
    // Clear a column down to the world floor, then perch one sand on a support.
    for y in MIN_Y..=(MIN_Y + 6) {
        set_block(1, y, 1, 0);
    }
    set_block(1, MIN_Y + 3, 1, stone);
    set_block(1, MIN_Y + 4, 1, sand);

    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let _stop_guard = StopOnDrop(stop.clone());
    let snapshots_store = std::sync::Arc::new(std::sync::Mutex::new(Vec::<Vec<u8>>::new()));
    let saver = {
        let stop = stop.clone();
        let snapshots_store = snapshots_store.clone();
        std::thread::spawn(move || {
            let mut n = 0u32;
            while !stop.load(Ordering::Relaxed) {
                let _lock = SAVE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
                let mut buf = Vec::new();
                save_world_into(&mut buf);
                let mut store = snapshots_store.lock().unwrap();
                if store.len() < 16 {
                    store.push(buf);
                }
                drop(store);
                n += 1;
            }
            n
        })
    };

    // The saver's first snapshot can be slower than the whole fall (the world may be
    // larger than when this test runs alone), and a test that asserted "a save ran"
    // without waiting for one would fail on scheduling and world size rather than on
    // the race it is about. Wait for the thread to prove itself, then start the fall.
    let wait = std::time::Instant::now();
    while snapshots_store.lock().unwrap().is_empty() {
        assert!(
            wait.elapsed().as_secs() < 10,
            "the saver never stored a snapshot"
        );
        std::thread::yield_now();
    }
    // Knock out the support so the block falls out of the world.
    set_block(1, MIN_Y + 3, 1, 0);
    assert_eq!(
        falling().lock().unwrap().len(),
        1,
        "the block becomes an entity"
    );
    let mut ticks = 0;
    while !falling().lock().unwrap().is_empty() && ticks < 400 {
        falling_tick();
        ticks += 1;
    }
    assert!(ticks < 400, "the entity must leave the world");
    stop.store(true, Ordering::Relaxed);
    let saves = saver.join().expect("saver thread");
    assert!(saves > 0, "a save must have run during the fall");

    let live: u32 = drops()
        .lock()
        .unwrap()
        .iter()
        .filter(|d| d.item == sand_item)
        .map(|d| d.cnt as u32)
        .sum();
    assert_eq!(live, 1, "the below-world block becomes exactly one item");
    assert!(
        falling().lock().unwrap().is_empty(),
        "nothing is left in flight"
    );

    // A snapshot may predate the fall (the block is still on its support), be mid-fall (an entity),
    // or be after it (the item). Conservation holds in every case: blocks + items == 1.
    let sand_blocks = || {
        (MIN_Y..=MIN_Y + 6)
            .filter(|&y| {
                crate::block_registry::breg()
                    .unwrap()
                    .name_of(block_at(1, y, 1))
                    == "minecraft:sand"
            })
            .count() as u32
    };
    assert_eq!(sand_blocks() + live, 1, "live: block + item is conserved");
    let saved = snapshots_store.lock().unwrap().clone();
    assert!(!saved.is_empty(), "the saver stored a snapshot");
    for (i, snap) in saved.iter().enumerate() {
        world().lock().unwrap().clear();
        falling().lock().unwrap().clear();
        drops().lock().unwrap().clear();
        load_world_from(snap).unwrap();
        let mut t = 0;
        while !falling().lock().unwrap().is_empty() && t < 400 {
            falling_tick();
            t += 1;
        }
        let n: u32 = drops()
            .lock()
            .unwrap()
            .iter()
            .filter(|d| d.item == sand_item)
            .map(|d| d.cnt as u32)
            .sum();
        assert_eq!(
            sand_blocks() + n,
            1,
            "snapshot {i} loses or duplicates the below-world block"
        );
    }
}

/// The connection thread drops a gravity block's support while the tick thread advances the fall.
/// Neither takes the test's world guard, which is the point: `set_block` from a non-tick thread
/// runs against `falling_tick`, and every block must still land exactly once.
#[test]
fn concurrent_knockout_and_falling_conserve_blocks() {
    let _world_guard = world_guard();
    let _snap = WorldSnapshot::take();
    let (_, ow) = test_region();
    let mut warm = ow.clone();
    for x in 0..6 {
        for z in 0..6 {
            warm.insert((x, z));
        }
    }
    set_test_region(HashSet::new(), warm);
    let sand = name_to_state("sand").expect("sand must exist");
    let stone = name_to_state("stone").expect("stone must exist");
    let base = 120;
    let height = 3;
    let cols: Vec<(i32, i32)> = (1..5).flat_map(|x| (1..5).map(move |z| (x, z))).collect();
    let expected = cols.len() * height as usize;
    for &(x, z) in &cols {
        set_block(x, base - 1, z, stone);
        set_block(x, base, z, stone);
        for i in 1..=height {
            set_block(x, base + i, z, sand);
        }
    }

    let knocker = {
        let cols = cols.clone();
        std::thread::spawn(move || {
            for &(x, z) in &cols {
                // Remove the support only while it is the support, so the knock-out never deletes a
                // block that has already landed in the cell.
                if block_at(x, base, z) == stone {
                    set_block(x, base, z, 0);
                }
                std::thread::yield_now();
            }
        })
    };
    let mut ticks = 0;
    while ticks < 800 {
        falling_tick();
        ticks += 1;
        if knocker.is_finished() && falling().lock().unwrap().is_empty() {
            break;
        }
    }
    knocker.join().expect("knocker thread");
    while !falling().lock().unwrap().is_empty() && ticks < 1600 {
        falling_tick();
        ticks += 1;
    }
    assert!(ticks < 1600, "the fall must settle");

    let mut placed = 0;
    for &(x, z) in &cols {
        for y in (base - 1)..=(base + height + 4) {
            if block_at(x, y, z) == sand {
                placed += 1;
            }
        }
    }
    assert_eq!(
        placed, expected,
        "a concurrent knock-out duplicated or lost a block"
    );
    assert!(
        falling().lock().unwrap().is_empty(),
        "nothing is left in flight"
    );
    assert!(
        drops().lock().unwrap().is_empty(),
        "landed blocks do not drop"
    );
}

/// Two stacked blocks must both fall, and must not deadlock. The first version held the queue lock
/// while calling set_block, which reaches note_falling_above, which takes the same lock: with one
/// block it never showed, because the check returns before locking when there is nothing above.
#[test]
fn two_stacked_sand_blocks_fall_without_deadlocking() {
    crate::test_support::require_vanilla_data();
    let _world_guard = world_guard();
    let old_falling = std::mem::take(&mut *falling().lock().unwrap());
    let old_forced = std::mem::take(&mut *forceloaded().lock().unwrap());
    let (oh, ow) = test_region();
    let mut warm = ow.clone();
    warm.extend([(0, 0), (1, 0), (2, 0), (3, 0), (4, 0)]);
    set_test_region(oh.clone(), warm);
    let sand = name_to_state("sand").expect("sand must exist");
    let base = 150;
    let original: Vec<(i32, u32)> = (base..=base + 5).map(|y| (y, block_at(5, y, 5))).collect();
    for y in base..=base + 5 {
        set_block(5, y, 5, 0);
    }
    set_block(
        5,
        base,
        5,
        name_to_state("stone").expect("stone must exist"),
    );
    set_block(
        5,
        base + 2,
        5,
        name_to_state("dirt").expect("dirt must exist"),
    );
    set_block(5, base + 3, 5, sand);
    set_block(5, base + 4, 5, sand);
    // Removing the support converts the lower block, whose removal makes the upper one
    // unsupported, so both become entities in the same call.
    set_block(5, base + 2, 5, 0);
    assert_eq!(
        falling().lock().unwrap().len(),
        2,
        "both blocks must become entities"
    );

    let name = |y: i32| {
        crate::block_registry::breg()
            .unwrap()
            .name_of(block_at(5, y, 5))
            .to_string()
    };
    let mut ticks = 0;
    while !falling().lock().unwrap().is_empty() && ticks < 30 {
        falling_tick();
        ticks += 1;
    }
    assert!(ticks < 30, "both entities must land");
    assert!(
        falling().lock().unwrap().is_empty(),
        "nothing left in flight"
    );
    assert_eq!(name(base + 1), "minecraft:sand", "the lower one lands");
    assert_eq!(
        name(base + 2),
        "minecraft:sand",
        "and the upper one rests on it"
    );
    assert_eq!(
        name(base + 3),
        "minecraft:air",
        "nothing is left in the air"
    );

    for (y, st) in original {
        set_block(5, y, 5, st);
    }
    *falling().lock().unwrap() = old_falling;
    *forceloaded().lock().unwrap() = old_forced;
    set_test_region(oh, ow);
}

/// A falling entity has already left its block cell, so a *new* gravity block placed into that
/// same cell must still fall. Keying the spawn guard on "an entity occupies this cell" suppressed
/// it and left the block hanging in the air forever.
#[test]
fn a_new_block_in_a_passing_entity_cell_still_falls() {
    crate::test_support::require_vanilla_data();
    let _g = world_guard();
    let old_falling = std::mem::take(&mut *falling().lock().unwrap());
    let old_forced = std::mem::take(&mut *forceloaded().lock().unwrap());
    let (oh, ow) = test_region();
    let mut warm = ow.clone();
    warm.extend([(0, 0), (1, 0), (2, 0), (3, 0), (4, 0)]);
    set_test_region(oh.clone(), warm);
    let sand = name_to_state("sand").expect("sand must exist");
    let base = 150;
    let (x, z) = (7, 7);
    let original: Vec<(i32, u32)> = (base..=base + 4).map(|y| (y, block_at(x, y, z))).collect();
    for y in base..=base + 4 {
        set_block(x, y, z, 0);
    }
    set_block(x, base, z, name_to_state("stone").unwrap());
    set_block(x, base + 3, z, sand);
    assert_eq!(
        falling().lock().unwrap().len(),
        1,
        "the first block becomes an entity"
    );
    // The entity is still at y = base+3, so its cell is exactly the cell we now refill.
    set_block(x, base + 3, z, sand);
    assert_eq!(
        falling().lock().unwrap().len(),
        2,
        "a new block in the passing entity's cell must still fall"
    );
    assert_eq!(block_at(x, base + 3, z), 0, "and must leave the world");
    // Both entities are co-located and descend in lockstep. Landing both must keep both blocks:
    // the batch must not overwrite one with the other.
    let mut ticks = 0;
    while !falling().lock().unwrap().is_empty() && ticks < 40 {
        falling_tick();
        ticks += 1;
    }
    assert!(
        falling().lock().unwrap().is_empty(),
        "both entities must land"
    );
    let name = |y: i32| {
        crate::block_registry::breg()
            .unwrap()
            .name_of(block_at(x, y, z))
            .to_string()
    };
    assert_eq!(
        name(base + 1),
        "minecraft:sand",
        "the first lands on the floor"
    );
    assert_eq!(
        name(base + 2),
        "minecraft:sand",
        "the second must not be destroyed"
    );
    for (y, st) in original {
        set_block(x, y, z, st);
    }

    *falling().lock().unwrap() = old_falling;
    *forceloaded().lock().unwrap() = old_forced;
    set_test_region(oh, ow);
}

/// Vanilla's collision uses the block shape, so a falling block passes through a replaceable
/// block (a flower) and replaces it, rather than resting on top of it.
#[test]
fn a_falling_block_passes_through_a_replaceable_block() {
    crate::test_support::require_vanilla_data();
    let _g = world_guard();
    let old_falling = std::mem::take(&mut *falling().lock().unwrap());
    let old_forced = std::mem::take(&mut *forceloaded().lock().unwrap());
    let (oh, ow) = test_region();
    let mut warm = ow.clone();
    warm.extend([(0, 0), (1, 0), (2, 0), (3, 0), (4, 0)]);
    set_test_region(oh.clone(), warm);
    let reg = crate::block_registry::breg().unwrap();
    let flower = name_to_state("short_grass")
        .or_else(|| name_to_state("dandelion"))
        .expect("a replaceable plant must exist in the asset");
    assert!(
        reg.is_replaceable(flower),
        "the fixture plant must be replaceable"
    );
    let sand = name_to_state("sand").unwrap();
    let base = 150;
    let (x, z) = (9, 9);
    let original: Vec<(i32, u32)> = (base..=base + 4).map(|y| (y, block_at(x, y, z))).collect();
    for y in base..=base + 4 {
        set_block(x, y, z, 0);
    }
    set_block(x, base, z, name_to_state("stone").unwrap());
    set_block(x, base + 1, z, flower);
    set_block(x, base + 3, z, sand);
    set_block(x, base + 2, z, 0);
    assert_eq!(
        falling().lock().unwrap().len(),
        1,
        "the sand must fall, not rest on the plant"
    );
    let mut ticks = 0;
    while !falling().lock().unwrap().is_empty() && ticks < 40 {
        falling_tick();
        ticks += 1;
    }
    let name = |y: i32| reg.name_of(block_at(x, y, z)).to_string();
    assert_eq!(
        name(base + 1),
        "minecraft:sand",
        "the sand replaces the plant on the floor"
    );
    assert!(
        !reg.in_tag(block_at(x, base + 1, z), "minecraft:replaceable"),
        "the plant is gone"
    );
    for (y, st) in original {
        set_block(x, y, z, st);
    }

    *falling().lock().unwrap() = old_falling;
    *forceloaded().lock().unwrap() = old_forced;
    set_test_region(oh, ow);
}

/// A long unsupported column must convert entirely in one pass, not stop at the neighbour-update
/// recursion bound.
#[test]
fn a_long_unsupported_column_converts_in_one_pass() {
    crate::test_support::require_vanilla_data();
    let _g = world_guard();
    let old_falling = std::mem::take(&mut *falling().lock().unwrap());
    let old_forced = std::mem::take(&mut *forceloaded().lock().unwrap());
    let (oh, ow) = test_region();
    let mut warm = ow.clone();
    warm.extend([(0, 0), (1, 0), (2, 0), (3, 0), (4, 0)]);
    set_test_region(oh.clone(), warm);
    let sand = name_to_state("sand").unwrap();
    let stone = name_to_state("stone").unwrap();
    let base = 150;
    let (x, z) = (13, 9);
    // Taller than the neighbour-update recursion cap (64), so the cap must defer the rest rather
    // than discard it: every block still has to leave the world in one pass.
    let height = 70;
    let original: Vec<(i32, u32)> = (base..=base + height + 2)
        .map(|y| (y, block_at(x, y, z)))
        .collect();
    for y in base..=base + height + 2 {
        set_block(x, y, z, 0);
    }
    set_block(x, base, z, stone);
    for i in 1..=height {
        set_block(x, base + i, z, sand);
    }
    assert!(
        falling().lock().unwrap().is_empty(),
        "a supported column must not fall yet"
    );
    set_block(x, base, z, 0);
    assert_eq!(
        falling().lock().unwrap().len(),
        height as usize,
        "the whole column must become entities in one pass"
    );
    for (y, st) in original {
        set_block(x, y, z, st);
    }

    *falling().lock().unwrap() = old_falling;
    *forceloaded().lock().unwrap() = old_forced;
    set_test_region(oh, ow);
}

/// Placing unsupported sand must queue the air the world now holds, not the sand that was
/// requested: the block converts to an entity in the same call, and notifying the requested state
/// would leave a ghost block on the client.
#[test]
fn placing_unsupported_sand_queues_air_not_a_ghost() {
    crate::test_support::require_vanilla_data();
    let _g = world_guard();
    let old_falling = std::mem::take(&mut *falling().lock().unwrap());
    let old_forced = std::mem::take(&mut *forceloaded().lock().unwrap());
    let old_world = world().lock().unwrap().clone();
    pending_blocks().lock().unwrap().clear();
    let (oh, ow) = test_region();
    let mut warm = ow.clone();
    warm.extend([(0, 0), (1, 0), (2, 0), (3, 0), (4, 0)]);
    set_test_region(oh.clone(), warm);
    let sand = name_to_state("sand").unwrap();
    let (x, y, z) = (3, 150, 3);
    let original: Vec<(i32, u32)> = (y - 1..=y + 1).map(|yy| (yy, block_at(x, yy, z))).collect();
    set_block(x, y - 1, z, 0);
    set_block(x, y, z, 0);
    place_block(x, y, z, sand);
    assert_eq!(block_at(x, y, z), 0, "unsupported sand leaves the world");
    let idx = ((x & 15) as u16) << 8 | ((z & 15) as u16) << 4 | (y & 15) as u16;
    let queued = pending_blocks()
        .lock()
        .unwrap()
        .get(&(x >> 4, y >> 4, z >> 4))
        .and_then(|m| m.get(&idx))
        .copied();
    assert_eq!(
        queued,
        Some(0),
        "the queued update is the air the world holds, not a ghost sand"
    );
    for (yy, st) in original {
        set_block(x, yy, z, st);
    }
    *world().lock().unwrap() = old_world;
    *falling().lock().unwrap() = old_falling;
    *forceloaded().lock().unwrap() = old_forced;
    set_test_region(oh, ow);
}

/// Placing a gravity block in mid-air changes that cell and leaves everything under it untouched,
/// so a scheduler that only looked at the cell above would leave it hanging forever. Every falling
/// block name from the asset is walked here, because the registry test alone cannot tell whether a
/// name that is classified as falling is also scheduled and moved - reverting the tick to a hand
/// list of three states kept that test green.
#[test]
fn every_falling_block_becomes_an_entity_and_lands() {
    crate::test_support::require_vanilla_data();
    let _g = world_guard();
    let old_falling = std::mem::take(&mut *falling().lock().unwrap());
    let old_forced = std::mem::take(&mut *forceloaded().lock().unwrap());
    let (oh, ow) = test_region();
    let mut warm = ow.clone();
    warm.extend([(0, 0), (1, 0), (2, 0), (3, 0), (4, 0)]);
    set_test_region(oh.clone(), warm);
    let reg = crate::block_registry::breg().unwrap();
    let names: std::collections::BTreeSet<String> = (0..reg.state_count() as u32)
        .filter(|&st| reg.behaviour_of(st) == crate::block_registry::B_FALLING)
        .map(|st| reg.name_of(st).trim_start_matches("minecraft:").to_string())
        .collect();
    assert_eq!(names.len(), 19, "the asset set changed: {names:?}");
    let base = 150;
    let mut x = 20;
    for n in &names {
        x += 2;
        let st = name_to_state(n).unwrap_or_else(|| panic!("{n} must be a placeable block"));
        for y in base..=base + 4 {
            set_block(x, y, 9, 0);
        }
        set_block(x, base, 9, name_to_state("stone").unwrap());
        *falling().lock().unwrap() = Vec::new();
        set_block(x, base + 3, 9, st);
        assert_eq!(
            block_at(x, base + 3, 9),
            0,
            "{n} must leave the world when it falls"
        );
        assert_eq!(
            falling().lock().unwrap().len(),
            1,
            "{n} must become one entity"
        );
        assert_eq!(
            falling().lock().unwrap()[0].state,
            st,
            "{n} must keep its state"
        );
        let mut ticks = 0;
        while !falling().lock().unwrap().is_empty() && ticks < 30 {
            falling_tick();
            ticks += 1;
        }
        assert!(ticks < 30, "{n} must land");
        assert_eq!(
            reg.name_of(block_at(x, base + 1, 9))
                .trim_start_matches("minecraft:"),
            n.as_str(),
            "{n} must land on the floor's top face"
        );
    }
    *falling().lock().unwrap() = old_falling;
    *forceloaded().lock().unwrap() = old_forced;
    set_test_region(oh, ow);
}

/// A queued block in a chunk nobody is near must not move. Vanilla's falling block is an entity and
/// entities do not tick where there is no player, so without this the queue would carry a block all
/// the way down and land it in a chunk nobody had ever visited.
#[test]
fn a_falling_block_waits_in_a_cold_chunk() {
    crate::test_support::require_vanilla_data();
    let _g = world_guard();
    let old_falling = std::mem::take(&mut *falling().lock().unwrap());
    let old_forced = std::mem::take(&mut *forceloaded().lock().unwrap());
    let (oh, ow) = test_region();
    set_test_region(oh.clone(), std::collections::HashSet::new());
    let base = 150;
    for y in base..=base + 4 {
        set_block(11, y, 11, 0);
    }
    set_block(11, base, 11, name_to_state("stone").unwrap());
    set_block(11, base + 3, 11, name_to_state("sand").unwrap());
    // The block still leaves the world and becomes an entity even in a cold chunk; only the
    // entity's tick is deferred.
    assert_eq!(block_at(11, base + 3, 11), 0, "the block leaves the world");
    let y0 = {
        let q = falling().lock().unwrap();
        assert_eq!(q.len(), 1);
        q[0].y
    };

    falling_tick();
    let q = falling().lock().unwrap().clone();
    assert_eq!(q.len(), 1, "a cold chunk must not land or drop the entity");
    assert_eq!(q[0].y, y0, "and must not move it");
    // Warming the chunk must let it resume, not leave it frozen.
    let mut warm = ow.clone();
    warm.extend([(0, 0)]);
    set_test_region(oh.clone(), warm);
    falling_tick();
    let q = falling().lock().unwrap().clone();
    assert!(
        q.len() == 1 && q[0].y < y0,
        "a warmed chunk must resume the fall: {q:?}"
    );

    set_test_region(oh, ow);
    *falling().lock().unwrap() = old_falling;
    *forceloaded().lock().unwrap() = old_forced;
}

/// A live container is the positive control: a no-op reader must fail this round trip.
#[test]
fn a_broken_chest_leaves_no_orphan_entry_across_a_save_and_load() {
    if test_support::isolated_world_test() {
        return;
    }
    let broken = (21, 150, 21);
    let live = (23, 150, 21);
    let barrel = (25, 150, 21);
    let stack = (name_to_item("iron_ingot").unwrap(), 5, vec![(2, vec![0])]);
    for (pos, name) in [(broken, "chest"), (live, "chest"), (barrel, "barrel")] {
        set_block(pos.0, pos.1, pos.2, name_to_state(name).unwrap());
        chests()
            .lock()
            .unwrap()
            .entry(pos)
            .or_default()
            .insert(0, stack.clone());
    }
    set_block(broken.0, broken.1, broken.2, 0);
    assert!(
        !chests().lock().unwrap().contains_key(&broken),
        "cleanup on break"
    );
    let mut payload = Vec::new();
    save_world_into(&mut payload);
    world().lock().unwrap().clear();
    chests().lock().unwrap().clear();
    drops().lock().unwrap().clear();
    load_world_from(&payload).unwrap();
    assert_eq!(
        block_at(live.0, live.1, live.2),
        name_to_state("chest").unwrap()
    );
    let containers = chests().lock().unwrap().clone();
    assert_eq!(containers.len(), 2);
    assert!(
        !containers.contains_key(&broken),
        "broken contents must not resurrect"
    );
    for pos in [live, barrel] {
        assert_eq!(
            containers.get(&pos).and_then(|slots| slots.get(&0)),
            Some(&stack)
        );
        assert!(container_kind(block_at(pos.0, pos.1, pos.2)).is_some());
    }
    let ground = drops().lock().unwrap();
    assert_eq!(
        ground.len(),
        1,
        "the spilled stack persists once, not twice"
    );
    assert_eq!(
        (ground[0].item, ground[0].cnt, &ground[0].comps),
        (stack.0, stack.1, &stack.2)
    );
}

#[test]
fn client_information_carries_the_view_distance_after_the_locale() {
    let body = |locale: &str, view: i8| {
        let mut b = Vec::new();
        write_str(&mut b, locale);
        b.push(view as u8);
        b.extend_from_slice(&[0, 1, 127, 1, 0, 1]);
        b
    };
    for view in [12, 127, -1, -128, 0] {
        assert_eq!(
            client_info_view_distance(&body("en_us", view)),
            Some(view as i32)
        );
    }
    let b = body("en_us", 2);
    for end in 0..b.len() {
        assert_eq!(client_info_view_distance(&b[..end]), None);
    }
    let mut oversized = vec![0x80; 5];
    oversized.extend_from_slice(&body("", 2));
    assert_eq!(
        client_info_view_distance(&oversized),
        None,
        "six-byte locale VarInt"
    );
    assert_eq!(
        client_info_view_distance(&body(&"a".repeat(17), 2)),
        None,
        "locale exceeds 16 UTF-16 units"
    );
    assert_eq!(
        client_info_view_distance(&body(&"😀".repeat(8), 2)),
        Some(2)
    );
    assert_eq!(client_info_view_distance(&body(&"😀".repeat(9), 2)), None);
    let mut invalid_utf8 = b.clone();
    invalid_utf8[1] = 0xff;
    assert_eq!(client_info_view_distance(&invalid_utf8), None);
}

// ---------- falling and concurrent operations (specs/falling-concurrency) ----------

fn sand_count(x: i32, z: i32, lo: i32, hi: i32) -> usize {
    let sand = name_to_state("sand").expect("sand must exist");
    (lo..=hi).filter(|&y| block_at(x, y, z) == sand).count()
}

fn drop_count(item: i64) -> usize {
    drops()
        .lock()
        .unwrap()
        .iter()
        .filter(|d| d.item == item)
        .map(|d| d.cnt as usize)
        .sum()
}

/// A support placed while the block is in flight lands the entity on its top face, and a save
/// taken at that moment reloads to the same landing. This is the ownership case: the entity has
/// already left its block cell, so the support is placed into an empty cell and the landing is
/// deferred to the next tick's re-check.
#[test]
fn a_support_placed_under_a_falling_entity_is_landed_on() {
    crate::test_support::require_vanilla_data();
    let _world_guard = world_guard();
    let _snap = WorldSnapshot::take();
    let (_, ow) = test_region();
    let mut warm = ow.clone();
    warm.insert((0, 0));
    set_test_region(HashSet::new(), warm);
    let sand = name_to_state("sand").expect("sand must exist");
    let stone = name_to_state("stone").expect("stone must exist");
    let (x, z, base) = (3, 3, 150);
    for y in base..=base + 8 {
        set_block(x, y, z, 0);
    }
    set_block(x, base, z, stone);
    set_block(x, base + 4, z, sand);
    assert_eq!(
        falling().lock().unwrap().len(),
        1,
        "the block becomes an entity"
    );
    falling_tick();
    // Place the support one block under the in-flight entity.
    set_block(x, base + 2, z, stone);
    let mut ticks = 0;
    while !falling().lock().unwrap().is_empty() && ticks < 200 {
        falling_tick();
        ticks += 1;
    }
    assert!(ticks < 200, "the entity must land");
    assert_eq!(
        block_at(x, base + 3, z),
        sand,
        "it lands on the new support's top face"
    );
    assert_eq!(block_at(x, base + 2, z), stone, "the support is untouched");
    assert!(
        drops().lock().unwrap().is_empty(),
        "a landed block does not drop"
    );

    // The same state survives a save/reload: the support and the in-flight entity are written,
    // and the reloaded world lands the entity on the same cell.
    falling().lock().unwrap().clear();
    drops().lock().unwrap().clear();
    for y in base..=base + 8 {
        set_block(x, y, z, 0);
    }
    // Clearing a lower cell can spawn a fall for the block above it; those entities belong to the
    // previous scenario, not to the snapshot.
    falling().lock().unwrap().clear();
    drops().lock().unwrap().clear();
    set_block(x, base, z, stone);
    set_block(x, base + 4, z, sand);
    falling_tick();
    set_block(x, base + 2, z, stone);
    let mut buf = Vec::new();
    {
        let _save = SAVE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        save_world_into(&mut buf);
    }
    world().lock().unwrap().clear();
    falling().lock().unwrap().clear();
    drops().lock().unwrap().clear();
    load_world_from(&buf).unwrap();
    let mut t = 0;
    while !falling().lock().unwrap().is_empty() && t < 200 {
        falling_tick();
        t += 1;
    }
    assert!(t < 200, "the reloaded entity must land");
    assert_eq!(
        block_at(x, base + 3, z),
        sand,
        "reload lands on the same support"
    );
    assert_eq!(
        sand_count(x, z, base, base + 8),
        1,
        "exactly one block, not a ghost"
    );
}

/// A support removal and a save in the same tick: the snapshot is taken at the transition, so it
/// must conserve the column (world + falling + drops) both live and after a reload.
#[test]
fn a_knockout_and_a_save_in_the_same_tick_conserve_the_block() {
    let _world_guard = world_guard();
    let _snap = WorldSnapshot::take();
    let (_, ow) = test_region();
    let mut warm = ow.clone();
    warm.insert((0, 0));
    set_test_region(HashSet::new(), warm);
    let sand = name_to_state("sand").expect("sand must exist");
    let sand_item = name_to_item("sand").expect("sand item must exist");
    let stone = name_to_state("stone").expect("stone must exist");
    let (x, z, base, height) = (5, 5, 150, 3);
    for _ in 0..4 {
        world().lock().unwrap().clear();
        falling().lock().unwrap().clear();
        drops().lock().unwrap().clear();
        for y in base - 1..=base + height + 4 {
            set_block(x, y, z, 0);
        }
        set_block(x, base - 1, z, stone); // the floor the column lands on
        set_block(x, base, z, stone); // the support that is knocked out
        for i in 1..=height {
            set_block(x, base + i, z, sand);
        }
        set_block(x, base, z, 0); // the column becomes entities in this call
        let live = sand_count(x, z, base, base + height + 4)
            + falling().lock().unwrap().len()
            + drop_count(sand_item);
        assert_eq!(
            live, height as usize,
            "live: block + entity + drop is conserved"
        );
        let mut buf = Vec::new();
        {
            let _save = SAVE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            save_world_into(&mut buf);
        }
        world().lock().unwrap().clear();
        falling().lock().unwrap().clear();
        drops().lock().unwrap().clear();
        load_world_from(&buf).unwrap();
        let mut t = 0;
        while !falling().lock().unwrap().is_empty() && t < 400 {
            falling_tick();
            t += 1;
        }
        assert!(t < 400, "the snapshot's entities must settle");
        let after = sand_count(x, z, base, base + height + 4) + drop_count(sand_item);
        assert_eq!(
            after, height as usize,
            "reload: no block is lost or duplicated"
        );
    }
}

/// Two threads removing adjacent supports while the tick thread runs: every block ends up either
/// placed or dropped, and no block falls twice.
#[test]
fn two_knockouts_of_adjacent_supports_conserve_every_block() {
    crate::test_support::require_vanilla_data();
    let _world_guard = world_guard();
    let _snap = WorldSnapshot::take();
    let (_, ow) = test_region();
    let mut warm = ow.clone();
    for x in 0..3 {
        for z in 0..3 {
            warm.insert((x, z));
        }
    }
    set_test_region(HashSet::new(), warm);
    let sand = name_to_state("sand").expect("sand must exist");
    let sand_item = name_to_item("sand").expect("sand item must exist");
    let stone = name_to_state("stone").expect("stone must exist");
    let base = 120;
    let cols: Vec<(i32, i32)> = [(1, 1), (2, 1), (1, 2), (2, 2)].to_vec();
    let height = 2;
    for &(x, z) in &cols {
        for y in base - 2..=base + height + 2 {
            set_block(x, y, z, 0);
        }
        set_block(x, base - 1, z, stone);
        set_block(x, base, z, stone);
        for i in 1..=height {
            set_block(x, base + i, z, sand);
        }
    }
    let expected = cols.len() * height as usize;
    let knock = |part: Vec<(i32, i32)>| {
        std::thread::spawn(move || {
            for (x, z) in part {
                // Only remove the support while it still is the support.
                if block_at(x, base, z) == name_to_state("stone").unwrap() {
                    set_block(x, base, z, 0);
                }
                std::thread::yield_now();
            }
        })
    };
    let a = knock(vec![(1, 1), (2, 1)]);
    let b = knock(vec![(1, 2), (2, 2)]);
    let mut ticks = 0;
    while ticks < 800 {
        falling_tick();
        ticks += 1;
        if a.is_finished() && b.is_finished() && falling().lock().unwrap().is_empty() {
            break;
        }
    }
    a.join().expect("knocker a");
    b.join().expect("knocker b");
    while !falling().lock().unwrap().is_empty() && ticks < 1600 {
        falling_tick();
        ticks += 1;
    }
    assert!(ticks < 1600, "the fall must settle");
    let mut placed = 0;
    for &(x, z) in &cols {
        placed += sand_count(x, z, base - 1, base + height + 4);
        assert_eq!(
            sand_count(x, z, base, base + height - 1),
            height as usize,
            "column ({x},{z}) fell one block onto the floor"
        );
        assert_eq!(
            sand_count(x, z, base + height, base + height + 4),
            0,
            "column ({x},{z}) left its top cell empty"
        );
    }
    let dropped = drop_count(sand_item);
    assert_eq!(placed + dropped, expected, "no block is lost or duplicated");
    assert!(
        falling().lock().unwrap().is_empty(),
        "nothing is left in flight"
    );
}

/// Conversion to a drop at the world floor is one item whether the snapshot is taken in flight or
/// after conversion.
#[test]
fn a_save_during_entity_conversion_has_no_duplicate_drop() {
    crate::test_support::require_vanilla_data();
    let _world_guard = world_guard();
    let _snap = WorldSnapshot::take();
    let (_, ow) = test_region();
    let mut warm = ow.clone();
    warm.insert((0, 0));
    set_test_region(HashSet::new(), warm);
    let sand = name_to_state("sand").expect("sand must exist");
    let sand_item = name_to_item("sand").expect("sand item must exist");
    let (x, z) = (7, 7);
    for y in MIN_Y..=MIN_Y + 4 {
        set_block(x, y, z, 0);
    }
    set_block(x, MIN_Y + 1, z, sand);
    assert_eq!(
        falling().lock().unwrap().len(),
        1,
        "the block becomes an entity"
    );

    // Snapshot in flight, before the world floor.
    let mut mid = Vec::new();
    {
        let _save = SAVE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        save_world_into(&mut mid);
    }
    // Let it convert, then snapshot again.
    let mut t = 0;
    while !falling().lock().unwrap().is_empty() && t < 200 {
        falling_tick();
        t += 1;
    }
    assert!(t < 200, "the entity must leave the world");
    assert_eq!(drop_count(sand_item), 1, "one drop in the live world");
    let mut after = Vec::new();
    {
        let _save = SAVE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        save_world_into(&mut after);
    }
    for (name, snap) in [("mid-flight", &mid), ("after conversion", &after)] {
        world().lock().unwrap().clear();
        falling().lock().unwrap().clear();
        drops().lock().unwrap().clear();
        load_world_from(snap).unwrap();
        let mut t = 0;
        while !falling().lock().unwrap().is_empty() && t < 200 {
            falling_tick();
            t += 1;
        }
        assert_eq!(
            drop_count(sand_item),
            1,
            "{name}: exactly one drop, not duplicated"
        );
        assert_eq!(
            sand_count(x, z, MIN_Y, MIN_Y + 4),
            0,
            "{name}: no block is left behind"
        );
    }
}

/// A block in a chunk that leaves the warm set stays in flight without moving, and lands once the
/// chunk is warm again.
#[test]
fn region_change_mid_fall_keeps_the_entity() {
    crate::test_support::require_vanilla_data();
    let _world_guard = world_guard();
    let _snap = WorldSnapshot::take();
    let (_, ow) = test_region();
    let mut warm = ow.clone();
    warm.insert((0, 0));
    set_test_region(HashSet::new(), warm.clone());
    let sand = name_to_state("sand").expect("sand must exist");
    let stone = name_to_state("stone").expect("stone must exist");
    let (x, z, base) = (9, 9, 140);
    for y in base..=base + 6 {
        set_block(x, y, z, 0);
    }
    set_block(x, base, z, stone);
    set_block(x, base + 4, z, sand);
    falling_tick();
    let before = falling().lock().unwrap()[0].y;
    // Cold: the entity must be kept and not advanced.
    set_test_region(HashSet::new(), HashSet::new());
    for _ in 0..20 {
        falling_tick();
    }
    let held = falling().lock().unwrap();
    assert_eq!(held.len(), 1, "a cold chunk keeps the entity");
    assert_eq!(held[0].y, before, "a cold chunk does not advance it");
    drop(held);
    // Warm again: it lands on the support.
    set_test_region(HashSet::new(), warm);
    let mut ticks = 0;
    while !falling().lock().unwrap().is_empty() && ticks < 200 {
        falling_tick();
        ticks += 1;
    }
    assert!(ticks < 200, "the entity must land once the chunk is warm");
    assert_eq!(
        sand_count(x, z, base + 1, base + 5),
        1,
        "exactly one block lands"
    );
    assert_eq!(block_at(x, base + 1, z), sand, "it lands on the support");
}

/// The continuous sweep includes the original tick start, not the old 0.675
/// fraction. Keep the historical contact regression on the production geometry.
#[test]
fn the_projectile_sweep_covers_the_whole_tick() {
    assert!(projectile_segment_hits(
        [6.0, 64.5, -3.0],
        [10.0, 64.0, -3.0],
        (6.0, 64.0, -3.0),
        1.2,
        0.4,
        2.1,
    ));
    assert!(!projectile_segment_hits(
        [7.975, 64.16875, -3.0],
        [10.0, 64.0, -3.0],
        (6.0, 64.0, -3.0),
        1.2,
        0.4,
        2.1,
    ));
}

/// Preserve point-blank start-inside behavior while retiring sampled intersection.
#[test]
fn the_production_arrow_loop_hits_a_target_it_leaves_the_bow_inside() {
    let (hp, arrow_x) = arrow_sweep_outcome(0.6, 5.0);
    assert_eq!(
        hp,
        Some(14.0),
        "the arrow crossed the target but did not hit it: {arrow_x:?}"
    );
}

#[test]
fn primitives_arrow_tick_hits_between_the_old_samples() {
    let (hp, arrow_x) = arrow_sweep_outcome(3.0, 20.0);
    assert_eq!(
        hp,
        Some(14.0),
        "continuous segment missed target between samples: {arrow_x:?}"
    );
}

fn arrow_sweep_outcome(target_x: f64, velocity: f64) -> (Option<f32>, Option<f64>) {
    let _world_guard = world_guard();
    let old_arrows = std::mem::take(&mut *arrows().lock().unwrap());
    let old_mobs = std::mem::take(&mut *mobs().lock().unwrap());
    let old_forced = std::mem::take(&mut *forceloaded().lock().unwrap());
    let old_tick = TICK.swap(1, Ordering::Relaxed);
    let old_index = TICK_IDX.swap(0, Ordering::Relaxed);
    let (old_hot, old_warm) = test_region();
    forceloaded().lock().unwrap().insert((0, 0));
    // The point-blank case uses ordinary falling-arrow speed. The between-sample
    // discriminator deliberately uses higher velocity, without claiming native physics.
    arrows().lock().unwrap().push(Arrow {
        eid: 8100,
        owner: 1,
        x: 0.5,
        y: 120.0,
        z: 0.5,
        dx: velocity,
        dy: 0.0,
        dz: 0.0,
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
    // One target isolates intersection from entity ordering or block occlusion.
    mobs().lock().unwrap().push(Arc::new(Mob {
        eid: 8101,
        uuid: [0u8; 16],
        etype: 124,
        pos: Mutex::new((target_x, 120.0, 0.5, 0.0, 0.0)),
        hp: Mutex::new(20.0),
        last_hit: Mutex::new(Instant::now() - Duration::from_secs(10)),
        hurt_at: Mutex::new(Instant::now() - Duration::from_secs(10)),
        fuse: Mutex::new(0),
        love: Mutex::new(0),
        air: Mutex::new(server_ticks(AIR_SUPPLY_GT) as i32),
    }));
    game_tick(|_, tick| tick());
    let hp = mobs()
        .lock()
        .unwrap()
        .iter()
        .find(|m| m.eid == 8101)
        .map(|m| *m.hp.lock().unwrap());
    let arrow_x = arrows()
        .lock()
        .unwrap()
        .iter()
        .find(|a| a.eid == 8100)
        .map(|a| a.x);
    *arrows().lock().unwrap() = old_arrows;
    *mobs().lock().unwrap() = old_mobs;
    *forceloaded().lock().unwrap() = old_forced;
    TICK.store(old_tick, Ordering::Relaxed);
    TICK_IDX.store(old_index, Ordering::Relaxed);
    set_test_region(old_hot, old_warm);
    (hp, arrow_x)
}

#[test]
fn tmp_geode_noise_scale() {
    let n = crate::generation_math::normal_new_l(0, -4, &[1.0]);
    let mut vs = vec![];
    for i in 0..64 {
        for j in 0..64 {
            for k in 0..8 {
                vs.push(n.get((5 + i * 3) as f64, (-32 + j) as f64, (3 + k * 3) as f64));
            }
        }
    }
    let mean = vs.iter().sum::<f64>() / vs.len() as f64;
    let var = vs.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / vs.len() as f64;
    let mn = vs.iter().cloned().fold(f64::MAX, f64::min);
    let mx = vs.iter().cloned().fold(f64::MIN, f64::max);
    eprintln!(
        "GEODENOISE mean={mean:.4} sd={:.4} min={mn:.4} max={mx:.4}",
        var.sqrt()
    );
}

#[test]
fn nightly_hostile_type_uses_the_pinned_biome_spawners() {
    // Plains' supported entries in list order: spider(100), zombie(95),
    // skeleton(100), creeper(100); the runtime's entity ids are 100/124/91/23.
    assert_eq!(spawn_etype_for_biome(Some("minecraft:plains"), 0), 100);
    assert_eq!(spawn_etype_for_biome(Some("minecraft:plains"), 100), 124);
    assert_eq!(spawn_etype_for_biome(Some("minecraft:plains"), 195), 91);
    assert_eq!(spawn_etype_for_biome(Some("minecraft:plains"), 295), 23);
    // The End only lists endermen, which this runtime does not own; keep the
    // existing rotation rather than inventing a spawner entry.
    assert_eq!(spawn_etype_for_biome(Some("minecraft:the_end"), 0), 23);
    assert_eq!(spawn_etype_for_biome(None, 1), 91);
}

/// A deterministic yardstick for the chunk delivery path, with no socket in the loop.
///
/// The end-to-end numbers on this host swing by 2-3x between runs (other lanes), and the
/// load client's own pacing adds a second source of variance, so a wall-clock A/B there
/// cannot resolve a change. This drives the same sequence `stream_chunks` drives —
/// section terrain, band install, overlay, light arrays — over a fixed sweep of chunk
/// centres, and prints the same counters `MC_SPINE_CHUNK_PROFILE` prints in production.
///
///   MC_SPINE_CHUNK_BENCH=1 cargo test --release --nocapture chunk_delivery_path_bench
#[test]
fn chunk_delivery_path_bench() {
    if std::env::var("MC_SPINE_CHUNK_BENCH").is_err() {
        return;
    }
    let _guard = WORLD_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _gen_guard = crate::worldgen::lock_gen_for_test();
    assert!(worldgen::init(0, crate::name_to_state), "worldgen");
    let mut gen_sources: GenSources = HashMap::new();
    // a left-to-right sweep, the shape a moving viewer drives: each step moves one chunk
    let steps: Vec<(i32, i32)> = (0..12).map(|i| (i, 0)).collect();
    let mut outers_first = Vec::new();
    let mut inners_first = Vec::new();
    // Alternate the order between passes and re-init before each, so neither order gets a
    // warm cache the other did not: pass 2 is compared with pass 3, not with pass 1.
    for (run, central) in [
        (0usize, false),
        (1usize, true),
        (2usize, true),
        (3usize, false),
    ] {
        assert!(worldgen::init(0, crate::name_to_state), "worldgen re-init");
        let before = crate::chunkprof::counters();
        let mut per_chunk: Vec<f64> = Vec::new();
        for &(ox, oz) in &steps {
            let mut news: Vec<(i32, i32)> = Vec::new();
            for dx in -2..=2 {
                for dz in -2..=2 {
                    news.push((ox + dx, oz + dz));
                }
            }
            // outers-first (the order `stream_chunks` now uses) vs the natural order
            if central {
                news.sort_by_key(|&(cx, cz)| {
                    std::cmp::Reverse((cx - ox).abs().max((cz - oz).abs()))
                });
            }
            for (cx, cz) in news {
                let t = std::time::Instant::now();
                let _ = chunk_data(cx, cz, &mut gen_sources);
                per_chunk.push(t.elapsed().as_secs_f64() * 1e3);
            }
        }
        let n = per_chunk.len() as f64;
        let total: f64 = per_chunk.iter().sum();
        let after = crate::chunkprof::counters();
        let deltas: Vec<u64> = after
            .iter()
            .zip(before.iter())
            .map(|(a, b)| a - b)
            .collect();
        // counters(): [0] chunks, [1] chunk_data ns, [2] section ns, [3] light ns,
        // [4] band builds, [5] band cells, [6] block_at calls, [7] of which missed the
        // overlay, [8] terrain sections. The ns ones and the counts are both load-free.
        eprintln!(
            "CHUNKBENCH   work/chunk: data={:.1}ms sections={} ({:.1}ms) light={:.1}ms band_new={} band_cells={} block_at={} miss={}",
            deltas[1] as f64 / 1e6 / n,
            deltas[8] / 300,
            deltas[2] as f64 / 1e6 / n,
            deltas[3] as f64 / 1e6 / n,
            deltas[4] / 300,
            deltas[5] / 300,
            deltas[6] / 300,
            deltas[7] / 300
        );
        let med = {
            let mut v = per_chunk.clone();
            v.sort_by(|a, b| a.partial_cmp(b).unwrap());
            v[v.len() / 2]
        };
        eprintln!(
            "CHUNKBENCH pass={run} order={} chunks={n} total={total:.0}ms median={med:.1}ms mean={:.1}ms",
            if central { "outer-first" } else { "natural" },
            total / n
        );
        if central {
            inners_first = per_chunk;
        } else {
            outers_first = per_chunk;
        }
    }
    assert_eq!(outers_first.len(), inners_first.len());
}
