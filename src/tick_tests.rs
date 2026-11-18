use super::*;
use effect_tests::client_pair;

#[test]
fn generated_fluid_handoff_matches_native_proto_unpack() {
    fn ticks(text: &str) -> Vec<scheduled_ticks::ScheduledTick> {
        text.split(';')
            .filter(|v| !v.is_empty())
            .map(|v| {
                let a: Vec<_> = v.split(',').collect();
                assert_eq!(a.len(), 7);
                assert_eq!(a[5], "NORMAL");
                scheduled_ticks::ScheduledTick {
                    pos: (
                        a[1].parse().unwrap(),
                        a[2].parse().unwrap(),
                        a[3].parse().unwrap(),
                    ),
                    block: match a[0] {
                        "W" => 0,
                        "L" => 2,
                        _ => panic!("unknown native identity"),
                    },
                    due: a[4].parse().unwrap(),
                    priority: 0,
                    sub_order: a[6].parse().unwrap(),
                }
            })
            .collect()
    }
    fn schedule_live(
        owner: &mut GeneratedFluidOwner,
        events: Vec<scheduled_ticks::ScheduledTick>,
        now: i64,
    ) {
        for t in events {
            assert_eq!(t.due, now);
            owner.schedule(
                t.pos,
                flowing_fluid::Fluid {
                    kind: if t.block == 0 {
                        flowing_fluid::Kind::Water
                    } else {
                        flowing_fluid::Kind::Lava
                    },
                    level: 0,
                },
                now,
                true,
            );
        }
    }
    fn render(events: Vec<scheduled_ticks::ScheduledTick>) -> String {
        events
            .into_iter()
            .map(|t| {
                assert_eq!(t.priority, 0);
                format!(
                    "{},{},{},{},{},NORMAL,{}",
                    match t.block {
                        0 => "W",
                        2 => "L",
                        _ => panic!("unknown delivered identity"),
                    },
                    t.pos.0,
                    t.pos.1,
                    t.pos.2,
                    t.due,
                    t.sub_order
                )
            })
            .collect::<Vec<_>>()
            .join(";")
    }
    let mut cases = 0;
    for row in include_str!("../tests/data/proto_tick_1211.tsv").lines() {
        let a: Vec<_> = row.split('\t').collect();
        assert_eq!(a.len(), 8);
        let now: i64 = a[1].parse().unwrap();
        let limit: usize = a[2].parse().unwrap();
        let cold: HashSet<_> = a[3]
            .split(';')
            .filter(|v| !v.is_empty())
            .map(|v| {
                let (x, z) = v.split_once(',').unwrap();
                (x.parse::<i32>().unwrap(), z.parse::<i32>().unwrap())
            })
            .collect();
        let mut owner = GeneratedFluidOwner::new();
        schedule_live(&mut owner, ticks(a[4]), now);
        let pending: Vec<_> = ticks(a[5])
            .into_iter()
            .map(|t| {
                assert_eq!(t.due, 0);
                (t.pos, t.block)
            })
            .collect();
        owner.unpack_generated(pending.clone(), now);
        for (p, _) in pending {
            assert!(owner.owned.contains(&p), "generated fluid ownership {p:?}");
        }
        schedule_live(&mut owner, ticks(a[6]), now);
        if cases % 3 == 2 {
            let mut bytes = Vec::new();
            owner.queue.encode(&mut bytes);
            let mut input = bytes.as_slice();
            owner.queue = scheduled_ticks::ScheduledTicks::decode(&mut input).unwrap();
            assert!(input.is_empty());
        }
        let first = render(owner.queue.collect(now, limit, |c| !cold.contains(&c)));
        let second = render(owner.queue.collect(now + 1, limit, |_| true));
        assert_eq!(
            format!("{first}|{second}"),
            a[7],
            "actual native proto unpack -> LevelTicks callback, case {} (not fluid physics)",
            a[0]
        );
        cases += 1;
    }
    assert_eq!(cases, 18);
}

#[test]
fn generated_dispatchers_unpack_the_published_generator_queues() {
    let _world_guard = world_guard();
    let _gen_guard = worldgen::lock_gen_for_tests();
    let is_parent = std::env::var_os("MC_SPINE_GENERATED_DISPATCH_CHILD").as_deref()
        != Some(std::ffi::OsStr::new("1"));
    let epoch = worldgen::generation_seq();
    if is_parent {
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "tick_tests::generated_dispatchers_unpack_the_published_generator_queues",
                "--test-threads=1",
            ])
            .env("MC_SPINE_GENERATED_DISPATCH_CHILD", "1")
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            if child.try_wait().unwrap().is_some() {
                break;
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let output = child.wait_with_output().unwrap();
                panic!(
                    "isolated dispatcher fixture timed out: {}",
                    String::from_utf8_lossy(&output.stderr)
                );
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let output = child.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "isolated dispatcher fixture failed: {} {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            String::from_utf8_lossy(&output.stdout).contains("1 passed; 0 failed; 0 ignored;"),
            "isolated dispatcher test did not execute"
        );
    } else {
        generated_dispatcher_fixture();
    }
    if is_parent {
        assert_eq!(worldgen::generation_seq(),epoch,"dispatcher fixture changed the parent generator; even restoring world maps leaves real generation active in later tick tests");
    }
}

// Gen initialization is process-global and cannot be undone by WorldSnapshot.
// Keep this fixture in the child; otherwise later cadence tests generate terrain.
fn generated_dispatcher_fixture() {
    struct TickTime(u64);
    impl std::ops::Drop for TickTime {
        fn drop(&mut self) {
            TICK_IDX.store(self.0, Ordering::Relaxed);
        }
    }
    let _time = TickTime(TICK_IDX.load(Ordering::Relaxed));
    let _snapshot = WorldSnapshot::take();
    assert!(worldgen::init(0, name_to_state));
    set_test_region(HashSet::new(), HashSet::new());
    let row: Vec<_> = include_str!("../tests/data/proto_tick_1211.tsv")
        .lines()
        .next()
        .unwrap()
        .split('\t')
        .collect();
    assert_eq!(row[0], "0");
    assert!(row[3].is_empty() && row[4].is_empty() && row[6].is_empty());
    let now: i64 = row[1].parse().unwrap();
    TICK_IDX.store(now as u64, Ordering::Relaxed);
    let reg = crate::block_registry::breg().unwrap();
    for fluid in [true, false] {
        // Native W/L are opaque identities here. Real registry leaf identities
        // exercise the block dispatcher; ineligible chunks prevent physics.
        let ids = if fluid {
            [0, 2]
        } else {
            [
                reg.state_of("minecraft:oak_leaves", None),
                reg.state_of("minecraft:birch_leaves", None),
            ]
        };
        worldgen::with_gen(|g| {
            for entry in row[5].split(';') {
                let a: Vec<_> = entry.split(',').collect();
                let p = (
                    a[1].parse().unwrap(),
                    a[2].parse().unwrap(),
                    a[3].parse().unwrap(),
                );
                let id = ids[usize::from(a[0] == "L")];
                if fluid {
                    g.schedule_generated_fluid(p, id);
                } else {
                    g.schedule_generated_leaf(p, id);
                }
            }
        })
        .unwrap();
        let events = if fluid {
            generated_fluid_tick();
            assert!(
                worldgen::take_generated_fluid_ticks().is_empty(),
                "dispatcher did not drain published fluids"
            );
            GENERATED_FLUID_OWNER
                .get()
                .unwrap()
                .lock()
                .unwrap()
                .queue
                .collect(now, 65536, |_| true)
        } else {
            scheduled_block_tick();
            assert!(
                worldgen::take_generated_leaf_ticks().is_empty(),
                "dispatcher did not drain published leaves"
            );
            block_ticks().lock().unwrap().collect(now, 65536, |_| true)
        };
        let text = events
            .into_iter()
            .map(|t| {
                assert_eq!(t.priority, 0);
                let label = if t.block == ids[0] {
                    "W"
                } else {
                    assert_eq!(t.block, ids[1]);
                    "L"
                };
                format!(
                    "{label},{},{},{},{},NORMAL,{}",
                    t.pos.0, t.pos.1, t.pos.2, t.due, t.sub_order
                )
            })
            .collect::<Vec<_>>()
            .join(";");
        assert_eq!(
            text,
            row[7].split_once('|').unwrap().0,
            "native collector metadata through production dispatcher; fluid={fluid}"
        );
    }
}

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
    pending_block_loot: HashMap<BlockPos, PendingBlockLoot>,
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
            pending_block_loot: std::mem::take(&mut *pending_block_loot().lock().unwrap()),
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
        *pending_block_loot().lock().unwrap_or_else(|e| e.into_inner()) =
            std::mem::take(&mut self.pending_block_loot);
    }
}

#[test]
fn fragile_support_loss_drops_torches_and_lanterns_exactly_once() {
    crate::test_support::require_vanilla_data();
    let _guard = world_guard();
    let _snapshot = WorldSnapshot::take();
    set_test_region(HashSet::from([(0, 0)]), HashSet::from([(0, 0)]));
    let reg = crate::block_registry::breg().unwrap();
    let stone = reg.state_of("minecraft:stone", None);
    let air = reg.state_of("minecraft:air", None);
    let wall_torch = reg.with_prop(
        reg.state_of("minecraft:wall_torch", None), "facing", "north",
    );
    let lantern = reg.with_prop(
        reg.state_of("minecraft:lantern", None), "hanging", "true",
    );
    let cases = [
        ((3, 230, 3), (3, 229, 3), reg.state_of("minecraft:torch", None)),
        ((7, 230, 3), (7, 230, 4), wall_torch),
        ((11, 230, 3), (11, 231, 3), lantern),
    ];
    for (block, support, state) in cases {
        assert!(reg.block_drop(state).is_some(),
            "supported fragile family must have a known drop");
        set_block(support.0, support.1, support.2, stone);
        set_block(block.0, block.1, block.2, state);
        assert_eq!(block_at(block.0, block.1, block.2), state);
        // Removing an unrelated side does not break an attached block.
        let unrelated = (block.0 + 1, block.1, block.2);
        set_block(unrelated.0, unrelated.1, unrelated.2, air);
        assert_eq!(block_at(block.0, block.1, block.2), state);
        let before = drops().lock().unwrap().len();
        set_block(support.0, support.1, support.2, air);
        assert_eq!(block_at(block.0, block.1, block.2), air,
            "support loss must clear the fragile block");
        assert_eq!(drops().lock().unwrap().len(), before + 1,
            "exactly one drop from the detached block");
        let last_item = drops().lock().unwrap().last().unwrap().item;
        assert_eq!(last_item, reg.block_drop(state).unwrap());
        set_block(support.0, support.1, support.2, air);
        assert_eq!(drops().lock().unwrap().len(), before + 1,
            "repeated notifications must not duplicate drops");
    }
}

#[test]
fn fragile_direction_matching_is_not_just_any_air_neighbor() {
    let reg = crate::block_registry::breg().unwrap();
    let air = reg.state_of("minecraft:air", None);
    let stone = reg.state_of("minecraft:stone", None);
    let floor = reg.state_of("minecraft:torch", None);
    let wall = reg.with_prop(reg.state_of("minecraft:wall_torch", None), "facing", "north");
    let hanging = reg.with_prop(reg.state_of("minecraft:lantern", None), "hanging", "true");
    assert!(reg.fragile_loses_support(floor, 0, air));
    assert!(!reg.fragile_loses_support(floor, 1, air));
    assert!(!reg.fragile_loses_support(floor, 0, stone));
    assert!(reg.fragile_loses_support(wall, 3, air));
    assert!(!reg.fragile_loses_support(wall, 2, air));
    assert!(reg.fragile_loses_support(hanging, 1, air));
    assert!(!reg.fragile_loses_support(hanging, 0, air));
    assert!(!reg.fragile_loses_support(stone, 0, air));
}

#[test]
fn multiway_wall_post_and_compatible_fence_pane_connections() {
    let reg = crate::block_registry::breg().unwrap();
    let air = reg.state_of("minecraft:air", None);
    let stone = reg.state_of("minecraft:stone", None);
    let waterlogged = reg.with_prop(
        reg.state_of("minecraft:cobblestone_wall", None), "waterlogged", "true",
    );
    let north = reg.wall_after_full_neighbor(waterlogged, stone, air, 2).unwrap();
    assert_eq!(reg.get_prop(north, "north").as_deref(), Some("low"));
    assert_eq!(reg.get_prop(north, "up").as_deref(), Some("true"));
    let straight = reg.wall_after_full_neighbor(north, stone, air, 3).unwrap();
    assert_eq!(reg.get_prop(straight, "south").as_deref(), Some("low"));
    assert_eq!(reg.get_prop(straight, "up").as_deref(), Some("false"));
    let tee = reg.wall_after_full_neighbor(straight, stone, air, 5).unwrap();
    assert_eq!(reg.get_prop(tee, "east").as_deref(), Some("low"));
    assert_eq!(reg.get_prop(tee, "up").as_deref(), Some("true"));
    let straight_again = reg.wall_after_full_neighbor(tee, air, air, 5).unwrap();
    assert_eq!(straight_again, straight, "T junction disconnect restores straight wall");
    assert_eq!(reg.get_prop(straight_again, "waterlogged").as_deref(), Some("true"));
    assert_eq!(
        reg.wall_after_full_neighbor(straight_again, air, air, 3).unwrap(),
        north,
        "single-arm wall must keep its post"
    );

    let fence = reg.state_of("minecraft:oak_fence", None);
    let spruce = reg.state_of("minecraft:spruce_fence", None);
    let nether = reg.state_of("minecraft:nether_brick_fence", None);
    let joined = reg.connection_after_neighbor(fence, spruce, air, 5).unwrap();
    assert_eq!(reg.get_prop(joined, "east").as_deref(), Some("true"));
    assert!(reg.connection_after_neighbor(fence, nether, air, 5).is_none());
    assert_eq!(
        reg.connection_after_neighbor(joined, air, air, 5).unwrap(),
        fence,
        "wood fence disconnect"
    );
    let pane = reg.state_of("minecraft:glass_pane", None);
    let bars = reg.state_of("minecraft:iron_bars", None);
    let joined = reg.connection_after_neighbor(pane, bars, air, 4).unwrap();
    assert_eq!(reg.get_prop(joined, "west").as_deref(), Some("true"));
    assert_eq!(
        reg.connection_after_neighbor(joined, air, air, 4).unwrap(),
        pane,
        "glass pane disconnect"
    );
}

#[test]
fn runtime_connected_shapes_connect_and_disconnect_with_waterlogged_retained() {
    crate::test_support::require_vanilla_data();
    let _guard = world_guard();
    let _snapshot = WorldSnapshot::take();
    WATERLOGGED_DEFER.with(|v| v.borrow_mut().clear());
    set_test_region(HashSet::from([(0, 0)]), HashSet::from([(0, 0)]));
    let reg = crate::block_registry::breg().unwrap();
    let stone = reg.state_of("minecraft:stone", None);
    let air = reg.state_of("minecraft:air", None);
    let fence = reg.state_of("minecraft:oak_fence", None);
    let pane = reg.with_prop(
        reg.state_of("minecraft:glass_pane", None), "waterlogged", "true",
    );
    let wall = reg.state_of("minecraft:cobblestone_wall", None);
    let p = (6, 220, 6);
    let east = (7, 220, 6);
    let west = (5, 220, 6);
    let north = (6, 220, 5);
    for (pos, state) in [(p, air), (east, fence), (west, pane),
        (north, wall), ((6, 221, 5), air)] {
        set_block(pos.0, pos.1, pos.2, state);
    }
    set_block(p.0, p.1, p.2, stone);
    assert_eq!(reg.get_prop(block_at(east.0, east.1, east.2), "west").as_deref(), Some("true"));
    assert_eq!(reg.get_prop(block_at(west.0, west.1, west.2), "east").as_deref(), Some("true"));
    assert_eq!(reg.get_prop(block_at(north.0, north.1, north.2), "south").as_deref(), Some("low"));
    set_block(p.0, p.1, p.2, air);
    assert_eq!(reg.get_prop(block_at(east.0, east.1, east.2), "west").as_deref(), Some("false"));
    assert_eq!(reg.get_prop(block_at(west.0, west.1, west.2), "east").as_deref(), Some("false"));
    assert_eq!(reg.get_prop(block_at(north.0, north.1, north.2), "south").as_deref(), Some("none"));
    assert_eq!(reg.get_prop(block_at(west.0, west.1, west.2), "waterlogged").as_deref(), Some("true"));
    // Re-applying the same state does not start an endless shape cascade.
    set_block(p.0, p.1, p.2, air);
    assert_eq!(reg.get_prop(block_at(west.0, west.1, west.2), "east").as_deref(), Some("false"));
    WATERLOGGED_DEFER.with(|v| v.borrow_mut().clear());
}

#[test]
fn waterlogged_source_flows_on_fifth_game_tick_without_replacing_its_block() {
    crate::test_support::require_vanilla_data();
    let _guard = world_guard();
    let _snapshot = WorldSnapshot::take();
    WATERLOGGED_DEFER.with(|v| v.borrow_mut().clear());
    let reg = crate::block_registry::breg().unwrap();
    let stone = reg.state_of("minecraft:stone", None);
    let air = reg.state_of("minecraft:air", None);
    let pane = reg.with_prop(
        reg.state_of("minecraft:glass_pane", None), "waterlogged", "true",
    );
    let p = (6, 211, 6);
    let east = (7, 211, 6);
    assert!(waterlogged_source(pane));
    assert_eq!(reg.fluid_name(pane), "minecraft:water");
    {
        // Explicit closed chamber, so FluidReader never silently consults
        // unspecified generated terrain as a fixture input.
        let mut world = world().lock().unwrap();
        let chunk = world.entry((0, 0)).or_default();
        for y in 210..=212 {
            for z in 2..=10 {
                for x in 2..=10 {
                    chunk.insert(local_idx(x, y, z), stone);
                }
            }
        }
        chunk.insert(local_idx(p.0, p.1, p.2), pane);
        chunk.insert(local_idx(east.0, east.1, east.2), air);
    }
    assert_eq!(
        flowing_fluid::Reader::cell(&mut FluidReader, p).unwrap(),
        flowing_fluid::Cell::Fluid(flowing_fluid::Fluid {
            kind: flowing_fluid::Kind::Water, level: 0,
        })
    );
    let mut owner = GeneratedFluidOwner::new();
    owner.schedule(
        p, flowing_fluid::Fluid { kind: flowing_fluid::Kind::Water, level: 0 },
        100, false,
    );
    let eligible = HashSet::from([(0, 0)]);
    owner.tick(104, &eligible).unwrap();
    assert_eq!(block_at(east.0, east.1, east.2), air,
        "waterlogged water must not flow before the 5-GT deadline");
    owner.tick(105, &eligible).unwrap();
    assert_eq!(block_at(p.0, p.1, p.2), pane,
        "the fluid source must not destroy its waterlogged block");
    assert_eq!(block_at(east.0, east.1, east.2), 81,
        "the 5-GT source tick must spread to the only open side");
    WATERLOGGED_DEFER.with(|v| v.borrow_mut().clear());
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
        target: None,
        thrower: None,
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
        target: None,
        thrower: None,
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
            target: None,
            thrower: None,
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

// The pickup delay is one rule with two consumers. These fixtures drive the real `drop_tick`
// caller over a TCP `Client`, so a green tick means the client's slots, the live entity and the
// packets agree - not that the age counter advanced. The insertion rules themselves are pinned
// by the native 1.21.1 fixture; what is measured here is when they are allowed to run.

/// The negative control for the delay guard. The candidate scan in `drop_tick` copies positions,
/// not ages, so the decision has to be taken on the live entity under the take's own locks. Before
/// the guard a client standing on a fresh drop collected it on the tick it spawned: this test then
/// sees a changed inventory, an emptied entity and packets on a socket that must stay silent.
#[test]
fn pickup_tick_refuses_a_drop_inside_its_pickup_delay() {
    let _guard = world_guard();
    let _snapshot = WorldSnapshot::take();
    set_test_region(HashSet::from([(0, 0)]), HashSet::new());
    let (client, mut receiver) = client_pair();
    let stone = name_to_item("stone").unwrap();
    let eid = client.eid;
    // Register the client as an observer, so "no packet" means the take did not happen rather
    // than the client being outside the broadcast audience.
    client.seen.lock().unwrap().insert(9120);
    let client = Arc::new(client);
    *clients().lock().unwrap() = vec![client.clone()];
    let delay = server_ticks(PICKUP_DELAY_GT) as i32;
    assert!(
        delay > 1,
        "the boundary fixture needs at least one tick below the delay"
    );
    drops().lock().unwrap().push(Drop {
        eid: 9120,
        x: 0.0,
        y: 70.0,
        z: 0.0,
        item: stone,
        cnt: 10,
        comps: vec![],
        age: 0,
        target: None,
        thrower: None,
    });
    // Fresh. The cell is hot, so the same tick that refuses the take advances the age.
    drop_tick();
    let fresh = drops()
        .lock()
        .unwrap()
        .iter()
        .find(|d| d.eid == 9120)
        .map(|d| (d.age, d.cnt));
    assert_eq!(
        fresh,
        Some((1, 10)),
        "a refused take must leave the entity's count and its aging stage alone"
    );
    let young = client.inv.lock().unwrap().clone();
    assert!(
        young.is_empty(),
        "a fresh drop was collected: {young:?}"
    );
    // Boundary: one tick short still refuses, and that tick is also the one that ages the entity
    // up to the threshold. The age stage stays after the take; the test reads the order, not a
    // reordering done to make the test pass.
    for d in drops().lock().unwrap().iter_mut() {
        if d.eid == 9120 {
            d.age = delay - 1;
        }
    }
    drop_tick();
    let at_edge = drops()
        .lock()
        .unwrap()
        .iter()
        .find(|d| d.eid == 9120)
        .map(|d| (d.age, d.cnt));
    assert_eq!(
        at_edge,
        Some((delay, 10)),
        "age {} is inside the delay and must refuse the take",
        delay - 1
    );
    let edge = client.inv.lock().unwrap().clone();
    assert!(
        edge.is_empty(),
        "the tick before the threshold collected: {edge:?}"
    );
    client
        .sock
        .lock()
        .unwrap()
        .shutdown(std::net::Shutdown::Write)
        .unwrap();
    let mut packets = Vec::new();
    while let Some((id, body)) = read_packet(&mut receiver, Instant::now() + Duration::from_secs(1)) {
        packets.push((id, body));
    }
    assert!(
        packets.is_empty(),
        "a drop inside its delay was reported to the client: {packets:?}"
    );
    // The call after the boundary is the first one allowed to take it, so the refusal above was
    // the delay and not a candidate the scan never offers again.
    drop_tick();
    assert!(
        drops().lock().unwrap().iter().all(|d| d.eid != 9120),
        "the drop that reached the delay was never collected"
    );
    assert_eq!(
        client.inv.lock().unwrap().get(&36),
        Some(&(stone, 10, vec![])),
        "the aged drop must reach the selected slot, collector {eid}"
    );
}

/// The positive half of the same boundary: at exactly the delay the take runs, and it keeps the
/// whole contract the native fixture pins - both slots the stack spills into, the take event
/// carrying the entity's original count, and the destroy that removes the entity.
#[test]
fn pickup_tick_collects_at_the_delay_and_reports_the_original_count() {
    let _guard = world_guard();
    let _snapshot = WorldSnapshot::take();
    set_test_region(HashSet::from([(0, 0)]), HashSet::new());
    let (client, mut receiver) = client_pair();
    let stone = name_to_item("stone").unwrap();
    let eid = client.eid;
    client.seen.lock().unwrap().insert(9130);
    // The offhand holds 60, so the ten items spill from it into the selected slot: two slots
    // must change, and the entity is still consumed whole.
    client.inv.lock().unwrap().insert(45, (stone, 60, vec![]));
    let client = Arc::new(client);
    *clients().lock().unwrap() = vec![client.clone()];
    drops().lock().unwrap().push(Drop {
        eid: 9130,
        x: 0.0,
        y: 70.0,
        z: 0.0,
        item: stone,
        cnt: 10,
        comps: vec![],
        age: server_ticks(PICKUP_DELAY_GT) as i32,
        target: None,
        thrower: None,
    });
    drop_tick();
    assert!(
        drops().lock().unwrap().is_empty(),
        "a drop at its delay must be consumed whole"
    );
    assert_eq!(
        client.inv.lock().unwrap().get(&45),
        Some(&(stone, 64, vec![])),
        "the offhand must be filled from the aged drop"
    );
    assert_eq!(
        client.inv.lock().unwrap().get(&36),
        Some(&(stone, 6, vec![])),
        "the spill must reach the selected slot"
    );
    client
        .sock
        .lock()
        .unwrap()
        .shutdown(std::net::Shutdown::Write)
        .unwrap();
    let mut packets = Vec::new();
    while let Some(packet) = read_packet(&mut receiver, Instant::now() + Duration::from_secs(1)) {
        packets.push(packet);
    }
    let ids: Vec<i64> = packets.iter().map(|(id, _)| *id).collect();
    assert_eq!(
        ids,
        vec![0x15, 0x15, 0x6f, 0x42],
        "pickup packets were {packets:?}"
    );
    let (_, body) = packets
        .iter()
        .find(|(id, _)| *id == 0x6f)
        .expect("the take event");
    let mut body = body.as_slice();
    assert_eq!(read_varint(&mut body), Some(9130), "take names the entity");
    assert_eq!(read_varint(&mut body), Some(eid as i64), "take names the collector");
    assert_eq!(read_varint(&mut body), Some(10), "take carries the original count");
}

/// A full inventory only partially absorbs a drop, and that partial result is a `false` return in
/// 1.21.1 - so no take event, a metadata update with the remainder, and the entity still alive.
/// The delay must not change any of it: the tick before does nothing at all.
#[test]
fn pickup_tick_delay_boundary_keeps_the_partial_insert_and_leftover() {
    let _guard = world_guard();
    let _snapshot = WorldSnapshot::take();
    set_test_region(HashSet::from([(0, 0)]), HashSet::new());
    let (client, mut receiver) = client_pair();
    let stone = name_to_item("stone").unwrap();
    let dirt = name_to_item("dirt").unwrap();
    client.seen.lock().unwrap().insert(9140);
    *client.inv.lock().unwrap() = (9..=44).map(|key| (key, (dirt, 64, vec![]))).collect();
    client.inv.lock().unwrap().insert(45, (stone, 60, vec![]));
    let client = Arc::new(client);
    *clients().lock().unwrap() = vec![client.clone()];
    let delay = server_ticks(PICKUP_DELAY_GT) as i32;
    drops().lock().unwrap().push(Drop {
        eid: 9140,
        x: 0.0,
        y: 70.0,
        z: 0.0,
        item: stone,
        cnt: 10,
        comps: vec![],
        age: delay - 1,
        target: None,
        thrower: None,
    });
    drop_tick();
    assert_eq!(
        client.inv.lock().unwrap().get(&45),
        Some(&(stone, 60, vec![])),
        "the tick before the threshold merged into the offhand"
    );
    assert_eq!(
        client.inv.lock().unwrap().get(&9),
        Some(&(dirt, 64, vec![])),
        "the tick before the threshold rewrote another slot"
    );
    let before = drops()
        .lock()
        .unwrap()
        .iter()
        .find(|d| d.eid == 9140)
        .map(|d| (d.age, d.cnt));
    assert_eq!(before, Some((delay, 10)), "the entity moved inside its delay");
    // One tick later the same fixture runs the pinned insertion rule. Nothing about the partial
    // result is different for having waited.
    drop_tick();
    let remaining: Vec<u8> = drops().lock().unwrap().iter().map(|d| d.cnt).collect();
    let offhand = client.inv.lock().unwrap().get(&45).cloned();
    assert_eq!(remaining, [6], "the leftover must stay on the entity");
    assert_eq!(offhand, Some((stone, 64, vec![])));
    client
        .sock
        .lock()
        .unwrap()
        .shutdown(std::net::Shutdown::Write)
        .unwrap();
    let mut packets = Vec::new();
    while let Some(packet) = read_packet(&mut receiver, Instant::now() + Duration::from_secs(1)) {
        packets.push(packet);
    }
    let ids: Vec<i64> = packets.iter().map(|(id, _)| *id).collect();
    assert_eq!(ids, vec![0x15, 0x58], "partial pickup packets were {packets:?}");
    let (_, body) = packets
        .iter()
        .find(|(id, _)| *id == 0x58)
        .expect("the entity metadata update");
    let mut body = body.as_slice();
    assert_eq!(read_varint(&mut body), Some(9140));
    assert_eq!(read_varint(&mut body), Some(8), "metadata key 8 is the item");
    assert_eq!(read_varint(&mut body), Some(7), "type 7 is item_stack");
    assert_eq!(read_varint(&mut body), Some(6), "the leftover count on the wire");
}

/// The delay is per entity, not per player: a full player keeps refusing the take, and the next
/// eligible player must still get the drop once it has aged. Judging the age before the candidate
/// list is built would have to remember this; judging it on the live entity cannot lose it.
#[test]
fn pickup_tick_delay_keeps_the_next_player_eligible() {
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
    let dirt = name_to_item("dirt").unwrap();
    *first.inv.lock().unwrap() = (9..=44).map(|key| (key, (dirt, 64, vec![]))).collect();
    let full = first.inv.lock().unwrap().clone();
    let delay = server_ticks(PICKUP_DELAY_GT) as i32;
    drops().lock().unwrap().push(Drop {
        eid: 9150,
        x: 0.0,
        y: 70.0,
        z: 0.0,
        item: stone,
        cnt: 10,
        comps: vec![],
        age: delay - 1,
        target: None,
        thrower: None,
    });
    drop_tick();
    assert!(
        second.inv.lock().unwrap().is_empty(),
        "the second player collected a drop inside its delay"
    );
    assert_eq!(
        drops().lock().unwrap().iter().map(|d| (d.eid, d.cnt)).collect::<Vec<_>>(),
        [(9150, 10)],
        "a drop inside its delay must stay on the ground"
    );
    drop_tick();
    assert!(
        drops().lock().unwrap().is_empty(),
        "the full player monopolized the drop instead of letting the next one take it"
    );
    assert_eq!(
        *second.inv.lock().unwrap(),
        HashMap::from([(36, (stone, 10, vec![]))]),
        "the next eligible player must receive the aged drop"
    );
    assert_eq!(
        *first.inv.lock().unwrap(),
        full,
        "the full player's inventory must not change"
    );
}

#[test]
fn pickup_tick_excludes_spectator_and_dead_players() {
    let _guard = world_guard();
    let _snapshot = WorldSnapshot::take();
    set_test_region(HashSet::from([(0, 0)]), HashSet::new());
    let (spectator, mut spectator_rx) = client_pair();
    *spectator.gm.lock().unwrap() = 3;
    spectator.seen.lock().unwrap().insert(9201);
    let spectator = Arc::new(spectator);

    let (mut dead, mut dead_rx) = client_pair();
    dead.eid += 1;
    *dead.hp.lock().unwrap() = 0.0;
    dead.seen.lock().unwrap().insert(9201);
    let dead = Arc::new(dead);

    *clients().lock().unwrap() = vec![spectator.clone(), dead.clone()];
    let stone = name_to_item("stone").unwrap();
    drops().lock().unwrap().push(Drop {
        eid: 9201,
        x: 0.0,
        y: 70.0,
        z: 0.0,
        item: stone,
        cnt: 8,
        comps: vec![],
        age: 100,
        target: None,
        thrower: None,
    });
    drop_tick();
    assert!(
        spectator.inv.lock().unwrap().is_empty(),
        "spectator player must never pick up drops"
    );
    assert!(
        dead.inv.lock().unwrap().is_empty(),
        "dead player (hp <= 0) must never pick up drops"
    );
    assert_eq!(
        drops().lock().unwrap().iter().map(|d| (d.eid, d.cnt)).collect::<Vec<_>>(),
        [(9201, 8)],
        "spectator/dead players must leave the drop intact"
    );

    spectator
        .sock
        .lock()
        .unwrap()
        .shutdown(std::net::Shutdown::Write)
        .unwrap();
    dead.sock
        .lock()
        .unwrap()
        .shutdown(std::net::Shutdown::Write)
        .unwrap();
    assert!(read_packet(&mut spectator_rx, Instant::now() + Duration::from_secs(1)).is_none());
    assert!(read_packet(&mut dead_rx, Instant::now() + Duration::from_secs(1)).is_none());

    // Switching the player back out of spectator into survival allows pickup.
    *spectator.gm.lock().unwrap() = 0;
    drop_tick();
    assert!(drops().lock().unwrap().is_empty());
    assert_eq!(
        spectator.inv.lock().unwrap().get(&36),
        Some(&(stone, 8, vec![]))
    );
}

#[test]
fn pickup_tick_matches_1_21_1_player_touch_aabb_boundaries() {
    let _guard = world_guard();
    let _snapshot = WorldSnapshot::take();
    set_test_region(HashSet::from([(0, 0)]), HashSet::new());
    let (client, _rx) = client_pair();
    let client = Arc::new(client);
    *clients().lock().unwrap() = vec![client.clone()];
    let stone = name_to_item("stone").unwrap();
    let ready = server_ticks(PICKUP_DELAY_GT) as i32;

    // Player at (0.0, 70.0, 0.0):
    // Standing bounding box (0.6f x 1.8f) inflated by (1.0, 0.5, 1.0) covers
    // X,Z in [-1.300000011920929, +1.300000011920929], Y in [69.5, 72.29999995231628].
    // ItemEntity bounding box (0.25 x 0.25) is [x - 0.125, x + 0.125] x [y, y + 0.25].
    let cases = [
        // (eid, x, y, z, sneaking, fall_fly, should_pick_up, reason)
        // Above player head (y=72.0): old <1.3 sphere had dist^2 = 4.0 and failed; 1.21.1 AABB touches!
        (9210, 0.0, 72.0, 0.0, false, false, true, "vertical reach inside [py-0.5, py+2.3]"),
        // Exact top edge y=72.3 >= max_y (strict `<` in AABB.intersects): must NOT touch.
        (9211, 0.0, 72.3, 0.0, false, false, false, "vertical top boundary y=py+2.3 must not touch"),
        // Drop bottom y=69.25 -> drop max_y=69.5 == player min_y (strict `>`): must NOT touch.
        (9212, 0.0, 69.25, 0.0, false, false, false, "vertical bottom boundary y+0.25==py-0.5 must not touch"),
        // Slightly higher (y=69.26 -> max_y=69.51 > 69.5): touches!
        (9213, 0.0, 69.26, 0.0, false, false, true, "vertical bottom inside reach"),
        // Horizontal corner (1.4, 70.0, 1.4): old sphere failed (dist^2 = 3.92 > 1.69), AABB touches (1.4 - 0.125 = 1.275 < 1.3)!
        (9214, 1.4, 70.0, 1.4, false, false, true, "horizontal corner inside AABB"),
        // Horizontal reach just outside 1.3 + 0.125 = 1.425: must NOT touch.
        (9215, 1.426, 70.0, 0.0, false, false, false, "horizontal outside 1.3+0.125 must not touch"),
        // Horizontal inside 1.42: touches!
        (9216, 1.42, 70.0, 0.0, false, false, true, "horizontal inside 1.3+0.125"),
        // Crouching (height 1.5 -> max_y = 72.0): y=72.05 must NOT touch while crouching.
        (9217, 0.0, 72.05, 0.0, true, false, false, "crouching max_y=py+2.0 excludes y=72.05"),
        // Same y=72.05 when standing (height 1.8 -> max_y ~ 72.3): touches!
        (9218, 0.0, 72.05, 0.0, false, false, true, "standing max_y=py+2.3 includes y=72.05"),
        // Fall-flying (height 0.6 -> max_y ~ 71.1): y=71.15 must NOT touch.
        (9219, 0.0, 71.15, 0.0, false, true, false, "fall-flying max_y=py+1.1 excludes y=71.15"),
    ];

    for (eid, x, y, z, sneaking, fall_fly, expect_pickup, reason) in cases {
        client.inv.lock().unwrap().clear();
        drops().lock().unwrap().clear();
        *client.sneaking.lock().unwrap() = sneaking;
        *client.fall_fly.lock().unwrap() = fall_fly;
        drops().lock().unwrap().push(Drop {
            eid,
            x,
            y,
            z,
            item: stone,
            cnt: 1,
            comps: vec![],
            age: ready,
            target: None,
            thrower: None,
        });
        drop_tick();
        let picked = drops().lock().unwrap().is_empty();
        assert_eq!(picked, expect_pickup, "case {eid} ({reason})");
    }
}

#[test]
fn pickup_tick_enforces_target_uuid_over_closer_non_target() {
    let _guard = world_guard();
    let _snapshot = WorldSnapshot::take();
    set_test_region(HashSet::from([(0, 0)]), HashSet::new());
    let uuid_non_target = [0x11; 16];
    let uuid_owner = [0x22; 16];

    let (mut closer, _rx1) = client_pair();
    closer.uuid = uuid_non_target;
    *closer.pos.lock().unwrap() = (0.0, 70.0, 0.0, 0.0, 0.0, true);
    let closer = Arc::new(closer);

    let (mut owner, _rx2) = client_pair();
    owner.eid += 1;
    owner.uuid = uuid_owner;
    *owner.pos.lock().unwrap() = (1.2, 70.0, 0.0, 0.0, 0.0, true);
    let owner = Arc::new(owner);

    *clients().lock().unwrap() = vec![closer.clone(), owner.clone()];
    let stone = name_to_item("stone").unwrap();
    let ready = server_ticks(PICKUP_DELAY_GT) as i32;

    // 1. Target = uuid_owner, Thrower = uuid_non_target: closer (even at distance 0 & thrower) is rejected; owner collects!
    drops().lock().unwrap().push(Drop {
        eid: 9230,
        x: 0.0,
        y: 70.0,
        z: 0.0,
        item: stone,
        cnt: 5,
        comps: vec![],
        age: ready,
        target: Some(uuid_owner),
        thrower: Some(uuid_non_target),
    });
    drop_tick();
    assert!(drops().lock().unwrap().is_empty());
    assert!(closer.inv.lock().unwrap().is_empty(), "non-target closer player must not pick up targeted drop");
    assert_eq!(
        owner.inv.lock().unwrap().get(&36),
        Some(&(stone, 5, vec![])),
        "target owner must pick up targeted drop"
    );

    // 2. Target = None, Thrower = uuid_owner: thrower alone does NOT restrict pickup in 1.21.1 ItemEntity.playerTouch.
    closer.inv.lock().unwrap().clear();
    owner.inv.lock().unwrap().clear();
    drops().lock().unwrap().push(Drop {
        eid: 9231,
        x: 0.0,
        y: 70.0,
        z: 0.0,
        item: stone,
        cnt: 3,
        comps: vec![],
        age: ready,
        target: None,
        thrower: Some(uuid_owner),
    });
    drop_tick();
    assert!(drops().lock().unwrap().is_empty());
    assert_eq!(
        closer.inv.lock().unwrap().get(&36),
        Some(&(stone, 3, vec![])),
        "thrower alone must not block another player from picking up an untargeted drop"
    );

    // 3. Targeted drop with non-present owner stays on the ground even when closer non-target or an Actor is on top.
    closer.inv.lock().unwrap().clear();
    let old_actors = std::mem::take(&mut *actors().lock().unwrap());
    let actor = Arc::new(Actor {
        name: "bot".into(),
        eid: 9239,
        pos: Mutex::new((0.0, 70.0, 0.0)),
        look: Mutex::new(None),
        action: Mutex::new(""),
        held: Mutex::new((0, 0, Vec::new())),
        inv: Mutex::new(HashMap::new()),
    });
    actors().lock().unwrap().insert("bot".into(), actor.clone());
    *clients().lock().unwrap() = vec![closer.clone()];
    drops().lock().unwrap().push(Drop {
        eid: 9232,
        x: 0.0,
        y: 70.0,
        z: 0.0,
        item: stone,
        cnt: 4,
        comps: vec![],
        age: ready,
        target: Some(uuid_owner),
        thrower: None,
    });
    drop_tick();
    actor_tick();
    assert!(closer.inv.lock().unwrap().is_empty());
    assert!(actor.inv.lock().unwrap().is_empty(), "actor without matching UUID must not absorb targeted drop");
    assert_eq!(drops().lock().unwrap().len(), 1);
    *actors().lock().unwrap() = old_actors;
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
        target: None,
        thrower: None,
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
        target: None,
        thrower: None,
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
        target: None,
        thrower: None,
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
                    target: None,
                    thrower: None,
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
                    target: None,
                    thrower: None,
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
                    target: None,
                    thrower: None,
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
                    last_hurt_tick: Mutex::new(None),
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
        .entry(ContainerKey::Block((0, hy, 0)))
        .or_default()
        .insert(0, (item, 3, Vec::new()));
    let mut trace: Vec<(u8, u8)> = Vec::new();
    for _ in 0..24 {
        game_tick(|_, tick| tick());
        let own = chests()
            .lock()
            .unwrap()
            .get(&ContainerKey::Block((0, hy, 0)))
            .map(|m| m.values().map(|v| v.1).sum::<u8>())
            .unwrap_or(0);
        let dst = chests()
            .lock()
            .unwrap()
            .get(&ContainerKey::Block((0, hy - 1, 0)))
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
    // The claim is "not early": a queue-cap failure visits at MAX_DELAY (80 ticks) where the
    // real ladder is 160 or 320, so an early visit is the bug. The upper bound is the
    // measured clock drift: under the full gate's load a visit lands 1–3 ticks late (41,
    // 162, 163 against 40, 160, 160), which is the global tick counter moving inside the
    // window, not the guard releasing early. Six isolated runs and two full 8-thread suites
    // show no drift at all, so the tolerance stays small and one-sided.
    for (index, (got, want)) in deltas.iter().zip(expected.iter()).enumerate() {
        assert!(
            *got >= *want,
            "visit {} came {} ticks into a {}-tick backoff: the guard released early \
             instead of holding the real deadline (visits: {visits:?})",
            index + 1,
            got,
            want
        );
        assert!(
            *got <= want + 4,
            "visit {} came {} ticks into a {}-tick backoff, more than clock drift explains \
             (visits: {visits:?})",
            index + 1,
            got,
            want
        );
    }
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

/// Only count bookable gravity callbacks in currently eligible chunks.
/// Cold chunks can retain persistent callbacks without blocking live tests.
fn eligible_falling_bookings() -> bool {
    let Some(reg) = crate::block_registry::breg() else { return false };
    let eligible = scheduled_tick_chunks();
    block_ticks().lock().unwrap().has_pending_matching(|t| {
        reg.behaviour_of(t.block) == crate::block_registry::B_FALLING
            && eligible.contains(&(t.pos.0 >> 4, t.pos.2 >> 4))
    })
}

fn advance_falling_scheduled_tick() {
    TICK_IDX.fetch_add(1, Ordering::Relaxed);
    scheduled_block_tick();
}

fn drain_falling_bookings(max: usize) {
    for _ in 0..max {
        if !eligible_falling_bookings() { return; }
        advance_falling_scheduled_tick();
    }
    assert!(!eligible_falling_bookings(), "falling callbacks did not drain within {max} game ticks");
}

fn falling_work_in_progress() -> bool {
    !falling().lock().unwrap().is_empty() || eligible_falling_bookings()
}

/// The real game tick always runs scheduled callbacks before entity physics.
/// Never advance only the entity in a test that expects a pending block to move.
fn falling_world_tick_test() {
    advance_falling_scheduled_tick();
    falling_tick();
}

/// A delayed gravity callback must re-read support; re-adding the floor
/// before due time prevents conversion, and a subsequent removal rebooks.
#[test]
fn falling_booking_is_two_game_ticks_and_support_rescue_cancels_it() {
    crate::test_support::require_vanilla_data();
    let _guard = world_guard();
    let _snapshot = WorldSnapshot::take();
    let old_tick = TICK_IDX.load(Ordering::Relaxed);
    let (hot, warm) = test_region();
    let mut eligible = warm.clone();
    eligible.insert((0, 0));
    set_test_region(eligible.clone(), eligible);
    let reg = crate::block_registry::breg().unwrap();
    let sand = reg.state_of("minecraft:sand", None);
    let stone = reg.state_of("minecraft:stone", None);
    let (x, y, z) = (12, 220, 12);
    set_block(x, y, z, 0);
    set_block(x, y + 1, z, 0);
    set_block(x, y, z, stone);
    set_block(x, y + 1, z, sand);
    set_block(x, y, z, 0);
    assert_eq!(block_at(x, y + 1, z), sand);
    advance_falling_scheduled_tick();
    assert_eq!(block_at(x, y + 1, z), sand, "t+1 must not spawn a falling entity");
    set_block(x, y, z, stone);
    advance_falling_scheduled_tick();
    assert_eq!(block_at(x, y + 1, z), sand, "a restored support cancels the fall");
    assert!(falling().lock().unwrap().iter().all(|f| (f.x.floor() as i32, f.y.floor() as i32, f.z.floor() as i32) != (x, y + 1, z)));
    set_block(x, y, z, 0);
    advance_falling_scheduled_tick();
    assert_eq!(block_at(x, y + 1, z), sand, "rescheduled callback still waits +1 GT");
    advance_falling_scheduled_tick();
    assert_eq!(block_at(x, y + 1, z), 0, "second callback converts unsupported sand");
    assert_eq!(falling().lock().unwrap().iter().filter(|f| f.state == sand
        && f.x.floor() as i32 == x && f.z.floor() as i32 == z).count(), 1,
        "gravity conversion is exactly once");
    TICK_IDX.store(old_tick, Ordering::Relaxed);
    set_test_region(hot, warm);
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
    set_test_region(warm.clone(), warm);
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
    // Removing support books a callback for two game ticks later, and only
    // the due callback can convert the still-unsupported block into an entity.
    set_block(3, base + 2, 3, 0);
    assert_eq!(block_at(3, base + 3, 3), sand, "reserved, not immediately converted");
    assert!(falling().lock().unwrap().is_empty());
    advance_falling_scheduled_tick();
    assert_eq!(block_at(3, base + 3, 3), sand, "no fall at +1 GT");
    advance_falling_scheduled_tick();
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
    set_test_region(warm.clone(), warm);
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
    drain_falling_bookings(4);

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
    set_test_region(warm.clone(), warm);
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

    // Knock out every support; queued callbacks then convert the blocks.
    for &(x, z) in &columns {
        set_block(x, base + 1, z, 0);
    }
    drain_falling_bookings(2 * height as usize + 4);
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
        while falling_work_in_progress() && t < 200 {
            falling_world_tick_test();
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
    set_test_region(warm.clone(), warm);
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
    drain_falling_bookings(4);
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
        while falling_work_in_progress() && t < 400 {
            falling_world_tick_test();
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
    set_test_region(warm.clone(), warm);
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
        falling_world_tick_test();
        ticks += 1;
        if knocker.is_finished() && !falling_work_in_progress() {
            break;
        }
    }
    knocker.join().expect("knocker thread");
    while falling_work_in_progress() && ticks < 1600 {
        falling_world_tick_test();
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
    set_test_region(warm.clone(), warm);
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
    drain_falling_bookings(8);
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
    set_test_region(warm.clone(), warm);
    let sand = name_to_state("sand").expect("sand must exist");
    let base = 150;
    let (x, z) = (7, 7);
    let original: Vec<(i32, u32)> = (base..=base + 4).map(|y| (y, block_at(x, y, z))).collect();
    for y in base..=base + 4 {
        set_block(x, y, z, 0);
    }
    set_block(x, base, z, name_to_state("stone").unwrap());
    set_block(x, base + 3, z, sand);
    drain_falling_bookings(4);
    assert_eq!(
        falling().lock().unwrap().len(),
        1,
        "the first block becomes an entity"
    );
    // The entity is still at y = base+3, so its cell is exactly the cell we now refill.
    set_block(x, base + 3, z, sand);
    drain_falling_bookings(4);
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
    set_test_region(warm.clone(), warm);
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
    drain_falling_bookings(4);
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
    set_test_region(warm.clone(), warm);
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
    drain_falling_bookings(2 * height as usize + 10);
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
    set_test_region(warm.clone(), warm);
    let sand = name_to_state("sand").unwrap();
    let (x, y, z) = (3, 150, 3);
    let original: Vec<(i32, u32)> = (y - 1..=y + 1).map(|yy| (yy, block_at(x, yy, z))).collect();
    set_block(x, y - 1, z, 0);
    set_block(x, y, z, 0);
    place_block(x, y, z, sand);
    assert_eq!(block_at(x, y, z), sand, "sand remains during 2-GT booking");
    drain_falling_bookings(4);
    assert_eq!(block_at(x, y, z), 0, "due callback leaves air");
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
    set_test_region(warm.clone(), warm);
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
        drain_falling_bookings(4);
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
    set_test_region(HashSet::new(), HashSet::new());
    let base = 150;
    for y in base..=base + 4 {
        set_block(11, y, 11, 0);
    }
    set_block(11, base, 11, name_to_state("stone").unwrap());
    let sand = name_to_state("sand").unwrap();
    set_block(11, base + 3, 11, sand);
    // A scheduled block callback cannot run in a cold chunk, and a falling
    // entity must not be created before callback collection.
    for _ in 0..4 {
        advance_falling_scheduled_tick();
    }
    assert_eq!(block_at(11, base + 3, 11), sand, "cold: block remains");
    assert!(falling().lock().unwrap().is_empty(), "cold: no falling entity");
    // After warming, the overdue block callback runs first and the entity
    // moves only when the entity-physics stage runs.
    let mut warm = ow.clone();
    warm.extend([(0, 0)]);
    set_test_region(warm.clone(), warm);
    advance_falling_scheduled_tick();
    assert_eq!(block_at(11, base + 3, 11), 0, "warm: callback converts block");
    let y0 = {
        let q = falling().lock().unwrap();
        assert_eq!(q.len(), 1);
        q[0].y
    };
    falling_tick();
    let q = falling().lock().unwrap().clone();
    assert!(q.len() == 1 && q[0].y < y0,
            "warm: entity physics resumes after scheduled callback: {q:?}");

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
            .entry(ContainerKey::Block(pos))
            .or_default()
            .insert(0, stack.clone());
    }
    set_block(broken.0, broken.1, broken.2, 0);
    assert!(
        !chests().lock().unwrap().contains_key(&ContainerKey::Block(broken)),
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
        !containers.contains_key(&ContainerKey::Block(broken)),
        "broken contents must not resurrect"
    );
    for pos in [live, barrel] {
        assert_eq!(
            containers.get(&ContainerKey::Block(pos)).and_then(|slots| slots.get(&0)),
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
    set_test_region(warm.clone(), warm);
    let sand = name_to_state("sand").expect("sand must exist");
    let stone = name_to_state("stone").expect("stone must exist");
    let (x, z, base) = (3, 3, 150);
    for y in base..=base + 8 {
        set_block(x, y, z, 0);
    }
    set_block(x, base, z, stone);
    set_block(x, base + 4, z, sand);
    drain_falling_bookings(4);
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
    drain_falling_bookings(4);
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
    while falling_work_in_progress() && t < 200 {
        falling_world_tick_test();
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
    set_test_region(warm.clone(), warm);
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
        set_block(x, base, z, 0); // callbacks are pending; blocks remain until due
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
        while falling_work_in_progress() && t < 400 {
            falling_world_tick_test();
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
    set_test_region(warm.clone(), warm);
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
        falling_world_tick_test();
        ticks += 1;
        if a.is_finished() && b.is_finished() && !falling_work_in_progress() {
            break;
        }
    }
    a.join().expect("knocker a");
    b.join().expect("knocker b");
    while falling_work_in_progress() && ticks < 1600 {
        falling_world_tick_test();
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
    set_test_region(warm.clone(), warm);
    let sand = name_to_state("sand").expect("sand must exist");
    let sand_item = name_to_item("sand").expect("sand item must exist");
    let (x, z) = (7, 7);
    for y in MIN_Y..=MIN_Y + 4 {
        set_block(x, y, z, 0);
    }
    set_block(x, MIN_Y + 1, z, sand);
    drain_falling_bookings(4);
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
        while falling_work_in_progress() && t < 200 {
            falling_world_tick_test();
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
    set_test_region(warm.clone(), warm.clone());
    let sand = name_to_state("sand").expect("sand must exist");
    let stone = name_to_state("stone").expect("stone must exist");
    let (x, z, base) = (9, 9, 140);
    for y in base..=base + 6 {
        set_block(x, y, z, 0);
    }
    set_block(x, base, z, stone);
    set_block(x, base + 4, z, sand);
    drain_falling_bookings(4);
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
    set_test_region(warm.clone(), warm);
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
        last_hurt_tick: Mutex::new(None),
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
    // `MC_SPINE_BENCH_WRITER=1` runs a thread that writes blocks the way the tick loop does
    // (~500 writes/s for 90 chunks x 111 writes/chunk in 20 s), so the bench can tell
    // whether the cost the production profile shows is computation or contention on the
    // store/band/world locks that every write takes.
    let writer_stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let writer = if std::env::var("MC_SPINE_BENCH_WRITER").is_ok() {
        let stop = writer_stop.clone();
        Some(std::thread::spawn(move || {
            // `=inside` writes into the sweep's own chunks, so the encode loop sees a
            // non-empty overlay and takes its per-cell lookup path; the default writes far
            // away, so only the locks are shared.
            let inside = std::env::var("MC_SPINE_BENCH_WRITER").as_deref() == Ok("inside");
            let (x, z) = if inside {
                (0, 0)
            } else {
                (50 * 16 + 3, 50 * 16 + 5)
            };
            let base = block_at(x, 64, z);
            let mut n = 0u64;
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                let st = if n % 2 == 0 { base } else { 0 };
                let _ = set_block_if(x, 64, z, st, None);
                n += 1;
                std::thread::sleep(std::time::Duration::from_micros(2000));
            }
        }))
    } else {
        None
    };
    let _guard = WORLD_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _gen_guard = crate::worldgen::lock_gen_for_tests();
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
            "CHUNKBENCH   encode={:.1}ms/chunk hidden={} reads={} base_state={} terrain_passes={}",
            deltas[9] as f64 / 1e6 / n,
            deltas[10] / 300,
            deltas[11] / 300,
            deltas[12] / 300,
            deltas[13] / 300
        );
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
    writer_stop.store(true, std::sync::atomic::Ordering::Relaxed);
    if let Some(w) = writer {
        let _ = w.join();
    }
}

/// The delivered bytes of a fixed sweep, one digest per chunk, for comparing two builds or
/// two flag settings **from outside the process** (the prefetch switch changes when a
/// neighbour is populated, and population order is an input to the feature layer, so the
/// question "does it change the world?" needs a byte comparison).
///
///   MC_SPINE_CHUNK_DIGEST=1 cargo test --release --nocapture chunk_delivery_digest > a.txt
///   MC_SPINE_PREFETCH_NEIGHBOUR=1 MC_SPINE_CHUNK_DIGEST=1 ... > b.txt ; diff a.txt b.txt
///
/// The digest is over the `chunk_data` payload, which is what the client would receive for
/// that chunk (blocks, biomes, light arrays).
#[test]
fn chunk_delivery_digest() {
    if std::env::var("MC_SPINE_CHUNK_DIGEST").is_err() {
        return;
    }
    let _guard = WORLD_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _gen_guard = crate::worldgen::lock_gen_for_tests();
    assert!(worldgen::init(0, crate::name_to_state), "worldgen");
    let mut gen_sources: GenSources = HashMap::new();
    // A sweep that walks in +x, so the prefetch switch's +x neighbour is the one that will
    // be delivered next.
    // Hash the *terrain states*, not the encoded packet: the packet's bytes are not
    // reproducible across processes (two identical runs of this test differ), while the
    // question here is whether the world the worldgen layer produces is the same. Decoding
    // the packet would be equivalent and much longer.
    let sweep = |seed: u64,
                 dir: (i32, i32),
                 prefetch: bool,
                 gen_sources: &mut GenSources|
     -> Vec<((i32, i32), String)> {
        assert!(
            worldgen::init(seed, crate::name_to_state),
            "worldgen re-init"
        );
        let mut out = Vec::new();
        for step in 0..10 {
            for lane in -1..=1 {
                let (cx, cz) = (dir.0 * step, dir.1 * step);
                let (cx, cz) = if dir.0 != 0 {
                    (cx, cz + lane)
                } else {
                    (cx + lane, cz)
                };
                let _ = chunk_data_opt(cx, cz, gen_sources, prefetch);
                let mut bytes: Vec<u8> = Vec::new();
                for sec in 0..24 {
                    let sy = sec * 16 + crate::worldgen::min_y();
                    if let Some(arr) =
                        worldgen::terrain_section(cx, sy, cz, crate::feature::populate)
                    {
                        for st in arr.iter() {
                            bytes.extend_from_slice(&st.to_le_bytes());
                        }
                    }
                }
                out.push((
                    (cx, cz),
                    generation_math::md5(&bytes)
                        .iter()
                        .map(|v| format!("{v:02x}"))
                        .collect::<String>(),
                ));
            }
        }
        out
    };
    // Two seeds and three sweep directions. The switch prefetches the +x neighbour, so a
    // +z sweep exercises the case where the prefetched chunk is not the one delivered next.
    let mut changed: Vec<(u64, (i32, i32), (i32, i32))> = Vec::new();
    for seed in [0u64, 60007] {
        for dir in [(1, 0), (0, 1), (-1, 0)] {
            let plain = sweep(seed, dir, false, &mut gen_sources);
            // control: the same setting twice must reproduce, or the yardstick is not
            // measuring the world
            let again = sweep(seed, dir, false, &mut gen_sources);
            assert_eq!(plain, again, "the same sweep twice differs (seed {seed})");
            let prefetched = sweep(seed, dir, true, &mut gen_sources);
            for (((cx, cz), a), (_, b)) in plain.iter().zip(prefetched.iter()) {
                println!("DIGEST\t{seed}\t{dir:?}\t{cx}\t{cz}\t{a}");
                if a != b {
                    changed.push((seed, dir, (*cx, *cz)));
                }
            }
        }
    }
    // Pinned: the prefetch is world-neutral here — which is what makes it safe to consider
    // enabling. It populates the +x neighbour a step early, and population order is an input
    // to the feature layer, so this is a property to keep proving rather than assume: an
    // earlier version of this test reported one changed chunk, and that turned out to be a
    // stale band cache surviving `worldgen::init` (fixed in the same commit), not the
    // prefetch. A future change that makes the switch change the world has to fail here.
    assert!(
        changed.is_empty(),
        "the prefetch switch changed {} chunk(s): {changed:?}",
        changed.len()
    );
    println!("PREFETCH-DIFF\t{}", changed.len());
}


/// Paired with the authenticated native 1.21.1 canSurvive matrix, not an
/// independently guessed all-block survival model. It is compiled only with
/// native-oracle enabled; CI must pass a real matrix, not a placeholder.
#[cfg(feature = "native-oracle")]
#[test]
fn native_survival_fragile_port_pair() {
    const SCENES: [(&str, u8); 12] = [
        ("empty", 0),
        ("floor_stone", 1),
        ("ceiling_stone", 2),
        ("north_stone", 4),
        ("south_stone", 8),
        ("west_stone", 16),
        ("east_stone", 32),
        ("all_stone", 63),
        ("all_except_floor", 62),
        ("all_except_north", 59),
        ("floor_water", 0),
        ("floor_lava", 0),
    ];
    let path = std::env::var("MC_NATIVE_SURVIVAL_MATRIX")
        .expect("native fixture path required; not a synthetic fallback");
    let matrix = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("native matrix {path}: {e}"));
    let reg = crate::block_registry::breg().expect("load pinned block registry");
    assert_eq!(reg.state_count(), 26_684);

    let mut rows = 0_usize;
    let mut compared = 0_usize;
    let mut families = std::collections::HashSet::new();
    let mut disagreements = Vec::new();
    let air = reg.state_of("minecraft:air", None);
    for (index, line) in matrix.lines().enumerate() {
        let fields: Vec<&str> = line.split('\t').collect();
        assert_eq!(fields.len(), 5, "malformed native row {index}");
        let sid = (index / SCENES.len()) as u32;
        assert_eq!(fields[0], sid.to_string(), "native row state order {index}");
        assert_eq!(fields[1], SCENES[index % SCENES.len()].0,
                   "native scene order {index}");
        assert!(matches!(fields[2], "T" | "F"),
                "unavailable native truth in row {index}: {}", fields[2]);
        // Compare full property identity too: native state ids alone can
        // accidentally refer to different variants if property order changes.
        let mut properties = reg.properties_of(sid).expect("pinned state properties");
        properties.sort();
        let native_id = if properties.is_empty() {
            reg.name_of(sid).to_string()
        } else {
            format!("{}[{}]", reg.name_of(sid), properties.iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect::<Vec<_>>().join(","))
        };
        assert_eq!(fields[3], native_id, "native/Rust state identity for {sid}");
        rows += 1;

        let name = reg.name_of(sid);
        // Only these families have an implemented Rust negative-support rule.
        // Non-fragile families are intentionally not declared correct by default.
        let facing = match name {
            "minecraft:torch" | "minecraft:soul_torch"
            | "minecraft:redstone_torch" => Some(0_u8),
            "minecraft:lantern" | "minecraft:soul_lantern" =>
                Some(if reg.get_prop(sid, "hanging").as_deref() == Some("true")
                     { 1 } else { 0 }),
            "minecraft:wall_torch" | "minecraft:soul_wall_torch"
            | "minecraft:redstone_wall_torch" => {
                match reg.get_prop(sid, "facing").as_deref() {
                    Some("north") => Some(3),
                    Some("south") => Some(2),
                    Some("west") => Some(5),
                    Some("east") => Some(4),
                    other => panic!("invalid native-facing torch {sid}: {other:?}"),
                }
            }
            _ => None,
        };
        if let Some(toward) = facing {
            assert!(reg.fragile_loses_support(sid, toward, air),
                    "missing Rust support rule for {native_id}");
            let bit = 1_u8 << toward;
            let predicted = (SCENES[index % SCENES.len()].1 & bit) != 0;
            let observed = fields[2] == "T";
            if predicted != observed && disagreements.len() < 30 {
                disagreements.push(format!(
                    "{sid} {native_id} {}: native={} rust={}",
                    fields[1], observed, predicted));
            }
            families.insert(name.to_string());
            compared += 1;
        }
    }
    assert_eq!(rows, 26_684 * SCENES.len(), "not the full native matrix");
    assert_eq!(families.len(), 8, "fragile family coverage regressed");
    assert!(compared >= 150, "too few paired fragile-family observations: {compared}");
    assert!(disagreements.is_empty(),
            "native/Rust fragile support divergences: {}", disagreements.join("; "));
    println!("PAIRED_NATIVE_FRAGILE_SUPPORT states={rows} compared={compared} family_count={}",
             families.len());
}


/// Native BlockState.updateShape, one callback per scene, versus the Rust port.
/// Requires an actual JAR-produced 25-case TSV with the original/neighbor/result
/// states and the direction; normal tests never synthesize a passing baseline.
#[cfg(feature = "native-oracle")]
#[test]
fn native_neighbor_shape_port_pair() {
    fn parse(reg: &crate::block_registry::BReg, text: &str) -> u32 {
        let (name, properties) = match text.split_once('[') {
            Some((n, ps)) => (n, Some(ps.strip_suffix(']').expect("closing state ]"))),
            None => (text, None),
        };
        let pairs: Vec<(String, String)> = properties
            .map(|ps| {
                ps.split(',').map(|pair| {
                    let (key, value) = pair.split_once('=').expect("name=value");
                    (key.to_string(), value.to_string())
                }).collect()
            }).unwrap_or_default();
        let id = reg.checked_state(name, &pairs)
            .unwrap_or_else(|| panic!("Rust registry lacks native state {text}"));
        let mut rust_props = reg.properties_of(id).expect("valid native state");
        rust_props.sort();
        let canonical = if rust_props.is_empty() {
            reg.name_of(id).to_string()
        } else {
            format!("{}[{}]", reg.name_of(id), rust_props.iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect::<Vec<_>>().join(","))
        };
        assert_eq!(canonical, text, "native state is not the mapped Rust state");
        id
    }

    const CASES: [&str; 25] = [
        "oak_north_bone", "oak_south_bone", "oak_west_bone",
        "oak_east_bone", "nether_west_bone", "pane_west_bone",
        "bars_east_bone", "wall_west_bone", "stairs_outer_left",
        "stairs_inner_left", "torch_down_air", "wall_torch_south_air",
        "lantern_up_air", "wall_post_to_straight", "wall_straight_to_tee",
        "wall_straight_disconnect", "wall_tee_disconnect",
        "pane_east_disconnect", "fence_east_disconnect",
        "pane_bars_join", "wood_fence_join", "nether_no_wood_join",
        "oak_west_air", "pane_west_air", "bone_west_bone",
    ];
    let path = std::env::var("MC_NATIVE_NEIGHBOR_SHAPES")
        .expect("native updateShape TSV required");
    let input = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("native neighbor shapes {path}: {e}"));
    let reg = crate::block_registry::breg().expect("pinned block registry");
    let air = reg.state_of("minecraft:air", None);
    let mut checked = 0;
    let mut mismatches = Vec::new();
    for (i, line) in input.lines().enumerate() {
        let cols: Vec<_> = line.split('\t').collect();
        assert_eq!(cols.len(), 5, "incomplete native shape row {i}");
        assert!(i < CASES.len(), "unrecognized native shape row {i}");
        assert_eq!(cols[0], CASES[i], "reordered native shape at {i}");
        let original = parse(reg, cols[1]);
        let expected = parse(reg, cols[2]);
        let neighbor = parse(reg, cols[3]);
        let (dir, delta) = match cols[4] {
            "DOWN" => (0, (0, -1, 0)),
            "UP" => (1, (0, 1, 0)),
            "NORTH" => (2, (0, 0, -1)),
            "SOUTH" => (3, (0, 0, 1)),
            "WEST" => (4, (-1, 0, 0)),
            "EAST" => (5, (1, 0, 0)),
            other => panic!("unknown native direction {other}"),
        };
        let result = if reg.fragile_loses_support(original, dir, neighbor) {
            air
        } else if reg.name_of(original).ends_with("_stairs") {
            let target = (delta.0, 64 + delta.1, delta.2);
            reg.stair_after_neighbors(original, (0, 64, 0),
                |p| if p == target { neighbor } else { air })
                .unwrap_or(original)
        } else {
            reg.connection_after_neighbor(original, neighbor, air, dir)
                .unwrap_or(original)
        };
        if result != expected {
            mismatches.push(format!(
                "{} native={} rust={} (id {result})",
                cols[0], cols[2], reg.name_of(result)));
        }
        checked += 1;
    }
    assert_eq!(checked, CASES.len(), "native shape cases missing");
    assert!(mismatches.is_empty(),
            "native/Rust updateShape mismatches:\n{}", mismatches.join("\n"));
    println!("PAIRED_NATIVE_NEIGHBOR_SHAPES cases={checked} mismatches=0");
}


/// Replays native-created tick appointments through the Rust per-chunk scheduler,
/// compares every callback in source order, and never merges block/fluid queues.
/// Requires outputs of native_neighbor_trace.py from the same pinned Java run.
#[cfg(feature = "native-oracle")]
#[test]
fn native_level_ticks_port_delivery_pair() {
    #[derive(Clone)]
    struct Request {
        phase: String,
        identity: String,
        pos: (i32, i32, i32),
        due: i64,
        suborder: i64,
    }
    let requests_path = std::env::var("MC_NATIVE_TICK_REQUESTS")
        .expect("real native request TSV required");
    let delivery_path = std::env::var("MC_NATIVE_TICK_DELIVERIES")
        .expect("real native delivery TSV required");
    let requests = std::fs::read_to_string(&requests_path)
        .unwrap_or_else(|e| panic!("native requests {requests_path}: {e}"));
    let native_delivery = std::fs::read_to_string(&delivery_path)
        .unwrap_or_else(|e| panic!("native deliveries {delivery_path}: {e}"));
    let mut cases: Vec<(String, Vec<Request>)> = Vec::new();
    for (index, line) in requests.lines().enumerate() {
        let columns: Vec<_> = line.splitn(3, '\t').collect();
        assert_eq!(columns.len(), 3, "malformed native request line {index}");
        // The source index includes shape and set events; the native Python
        // validator checks complete per-scene event ordering before this runs.
        columns[1].parse::<usize>().expect("native event index");
        if !columns[2].starts_with("tick ") {
            // Preserve first-occurrence scene order, including scenes with no ticks.
            if !cases.iter().any(|(name, _)| name == columns[0]) {
                cases.push((columns[0].to_string(), Vec::new()));
            }
            continue;
        }
        let fields: Vec<_> = columns[2].split_ascii_whitespace().collect();
        assert_eq!(fields.len(), 7, "malformed scheduled tick at row {index}");
        assert_eq!(fields[0], "tick");
        assert!(matches!(fields[1], "block" | "fluid"), "unknown phase");
        assert_eq!(fields[5], "NORMAL",
                   "unmodelled priority must fail closed, never silently coerce");
        let coords: Vec<_> = fields[3].split(',').collect();
        assert_eq!(coords.len(), 3, "native tick position missing coordinate");
        let pos = (coords[0].parse::<i32>().unwrap(),
                   coords[1].parse::<i32>().unwrap(),
                   coords[2].parse::<i32>().unwrap());
        let request = Request {
            phase: fields[1].to_string(),
            identity: fields[2].to_string(),
            pos,
            due: fields[4].parse().expect("native due time"),
            suborder: fields[6].parse().expect("native suborder"),
        };
        if let Some((_, saved)) = cases.iter_mut().find(|(name, _)| name == columns[0]) {
            saved.push(request);
        } else {
            cases.push((columns[0].to_string(), vec![request]));
        }
    }
    let mut replay = Vec::new();
    let mut scheduled = 0;
    for (case, requests) in cases {
        for phase in ["block", "fluid"] {
            let entries: Vec<_> = requests.iter().filter(|r| r.phase == phase).collect();
            if entries.is_empty() {
                continue;
            }
            let mut queue = scheduled_ticks::ScheduledTicks::new();
            let mut names = std::collections::HashMap::<String, u32>::new();
            let mut reverse = Vec::<String>::new();
            for r in entries {
                let id = match names.get(&r.identity) {
                    Some(&id) => id,
                    None => {
                        let id = reverse.len() as u32;
                        names.insert(r.identity.clone(), id);
                        reverse.push(r.identity.clone());
                        id
                    }
                };
                assert!((100..=140).contains(&r.due), "native due outside capture horizon");
                let _inserted = queue.schedule(scheduled_ticks::ScheduledTick {
                    pos: r.pos,
                    block: id,
                    due: r.due,
                    priority: 0,
                    sub_order: r.suborder,
                });
                scheduled += 1;
            }
            // Java LevelTicks.collect runs once per game tick, not a single
            // batch at max time: preserve the callback's actual delivery time.
            for at in 100..=140 {
                for t in queue.collect(at, 65_536, |_| true) {
                    replay.push(format!(
                        "{case}\t{phase}\t{at}\t{}\t{},{},{}",
                        reverse[t.block as usize], t.pos.0, t.pos.1, t.pos.2,
                    ));
                }
            }
        }
    }
    let native: Vec<String> = native_delivery.lines().map(str::to_owned).collect();
    assert!(!native.is_empty(), "native delivery output cannot be empty");
    assert_eq!(replay.len(), native.len(),
               "native/Rust callback count diverged, scheduled {scheduled}");
    for (index, (port, reference)) in replay.iter().zip(native.iter()).enumerate() {
        assert_eq!(port, reference, "native/Rust LevelTicks diverged at callback #{index}");
    }
    println!("PAIRED_NATIVE_LEVEL_TICKS requests={scheduled} deliveries={}",
             native.len());
}

/// A save must not be able to observe the block write without the tick booking
/// that belongs to it. The save path holds `SAVE_LOCK` while it serialises the
/// world and the queue, so holding it here makes a write where a concurrent save
/// would be: the write must not complete until the snapshot is done.
#[test]
fn a_block_write_waits_for_an_in_progress_save_before_booking() {
    let _world_guard = world_guard();
    let _snap = WorldSnapshot::take();
    let (tag, _) = client_pair();
    drop(tag);
    let (oh, ow) = test_region();
    let mut warm = ow.clone();
    warm.extend([(0, 0)]);
    set_test_region(warm.clone(), warm);
    let sand = name_to_state("sand").expect("sand");
    set_block(1, 200, 1, 0);
    *block_ticks().lock().unwrap() = scheduled_ticks::ScheduledTicks::new();

    let save = crate::SAVE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (tx, rx) = std::sync::mpsc::channel();
    let writer = std::thread::spawn(move || {
        set_block(1, 200, 1, sand);
        tx.send(()).unwrap();
    });
    assert!(
        rx.recv_timeout(Duration::from_millis(150)).is_err(),
        "the write completed while a save held the lock, so a snapshot can persist \
         the block without its callback and leave it hanging after a reload"
    );
    drop(save);
    assert!(
        rx.recv_timeout(Duration::from_secs(10)).is_ok(),
        "the write must finish once the save releases the lock"
    );
    writer.join().unwrap();

    let due = TICK_IDX.load(Ordering::Relaxed) as i64 + 2;
    let booked: Vec<_> = block_ticks()
        .lock()
        .unwrap()
        .collect(due, 64, |_| true)
        .into_iter()
        .map(|t| (t.pos, t.block))
        .collect();
    assert_eq!(
        booked,
        vec![((1, 200, 1), sand)],
        "the write must leave exactly one callback for the block it placed"
    );
    let _ = oh;
}

/// The invariant the lock protects, checked on the persisted form: a snapshot may
/// not hold a gravity block standing over a fallable cell without the callback
/// that will move it.
#[test]
fn a_saved_snapshot_never_holds_an_unbooked_gravity_block() {
    crate::test_support::require_vanilla_data();
    let _world_guard = world_guard();
    let _snap = WorldSnapshot::take();
    let (oh, ow) = test_region();
    let mut warm = ow.clone();
    warm.extend([(0, 0), (1, 0)]);
    set_test_region(warm.clone(), warm);
    let sand = name_to_state("sand").unwrap();
    let stone = name_to_state("stone").unwrap();
    for y in 190..=196 {
        set_block(1, y, 1, 0);
    }
    set_block(1, 190, 1, stone);
    set_block(1, 191, 1, sand);

    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let _stop_guard = StopOnDrop(stop.clone());
    let snaps = std::sync::Arc::new(std::sync::Mutex::new(Vec::<Vec<u8>>::new()));
    let saver = {
        let stop = stop.clone();
        let snaps = snaps.clone();
        std::thread::spawn(move || {
            let mut n = 0u32;
            while !stop.load(Ordering::Relaxed) && n < 12 {
                let _lock = crate::SAVE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
                let mut buf = Vec::new();
                save_world_into(&mut buf);
                snaps.lock().unwrap().push(buf);
                n += 1;
            }
            n
        })
    };
    // A falling column exercises the write and the booking from several paths.
    for y in 191..=193 {
        set_block(1, y, 1, sand);
        TICK_IDX.fetch_add(1, Ordering::Relaxed);
    }
    set_block(1, 190, 1, 0);
    drain_falling_bookings(16);
    stop.store(true, Ordering::Relaxed);
    let n = saver.join().expect("saver");
    assert!(n > 0, "the saver must capture at least one snapshot");

    let reg = crate::block_registry::breg().unwrap();
    let snapshots = snaps.lock().unwrap().clone();
    for (i, snap) in snapshots.iter().enumerate() {
        world().lock().unwrap().clear();
        falling().lock().unwrap().clear();
        drops().lock().unwrap().clear();
        *block_ticks().lock().unwrap() = scheduled_ticks::ScheduledTicks::new();
        load_world_from(snap).unwrap();
        let pending: std::collections::HashSet<_> = block_ticks()
            .lock()
            .unwrap()
            .collect(i64::MAX / 4, 4096, |_| true)
            .into_iter()
            .map(|t| t.pos)
            .collect();
        let cells: Vec<_> = world()
            .lock()
            .unwrap()
            .iter()
            .flat_map(|(&(cx, cz), cells)| {
                cells.iter().map(move |(&idx, &st)| {
                    // local_idx: (y - MIN_Y) << 8 | (z & 15) << 4 | (x & 15)
                    let lx = (idx & 15) as i32;
                    let ly = (idx >> 8) as i32 + MIN_Y;
                    let lz = ((idx >> 4) & 15) as i32;
                    ((cx << 4) + lx, ly, (cz << 4) + lz, st)
                })
            })
            .collect();
        for (x, y, z, st) in cells {
            if reg.behaviour_of(st) != crate::block_registry::B_FALLING {
                continue;
            }
            let below = crate::fallable_state(block_at(x, y - 1, z));
            assert!(
                !below || pending.contains(&(x, y, z)),
                "snapshot {i} holds {:?} at {x},{y},{z} with a fallable cell below and no callback: \
                 it would hang after a reload",
                reg.name_of(st)
            );
        }
    }
    let _ = oh;
}

/// The "book the cell above" branch is the only scheduler when the support under
/// a gravity block is removed: that write never touches the gravity cell itself.
/// The placement booking has already fired by then, so it cannot stand in for it.
#[test]
fn removing_a_support_schedules_the_block_above_after_its_own_booking_fired() {
    crate::test_support::require_vanilla_data();
    let _g = world_guard();
    let _snap = WorldSnapshot::take();
    let (oh, ow) = test_region();
    let mut warm = ow.clone();
    warm.extend([(0, 0)]);
    set_test_region(warm.clone(), warm.clone());
    let stone = name_to_state("stone").unwrap();
    let sand = name_to_state("sand").unwrap();
    // Chunk (0,0) so the eligibility predicate below is not what decides the result.
    let (x, z, base) = (11, 11, 150);
    for y in base..=base + 3 {
        set_block(x, y, z, 0);
    }
    // A pending callback for this cell, read from the queue itself rather than
    // through chunk eligibility: a cold chunk would make the weaker check vacuous.
    let pending = |pos: (i32, i32, i32)| {
        block_ticks()
            .lock()
            .unwrap()
            .collect(i64::MAX / 4, 4096, |_| true)
            .into_iter()
            .any(|t| t.pos == pos)
    };
    set_block(x, base, z, stone);
    set_block(x, base + 1, z, sand);
    assert!(pending((x, base + 1, z)), "the placement books the sand itself");
    // Let that booking fire while the sand is still supported: the callback
    // re-checks support and consumes itself.
    for _ in 0..4 {
        advance_falling_scheduled_tick();
    }
    assert_eq!(block_at(x, base + 1, z), sand, "the supported sand stays");
    assert!(falling().lock().unwrap().is_empty(), "no entity while supported");
    assert!(
        !pending((x, base + 1, z)),
        "the placement booking must be consumed before this test means anything"
    );

    // Removing the support writes only the support cell; the sand above must be
    // scheduled from the "cell above" branch.
    set_block(x, base, z, 0);
    assert!(
        pending((x, base + 1, z)),
        "removing a support must schedule the gravity block above it"
    );
    // The conversion itself is covered by the knocked-out-block and column
    // tests; this one pins the branch that schedules it.
    let _ = (oh, stone);
}

/// An entity that is already in flight when its chunk goes cold has to stay put;
/// the block callback gate alone cannot cover this, because the entity exists.
#[test]
fn a_falling_entity_stops_moving_when_its_chunk_goes_cold() {
    crate::test_support::require_vanilla_data();
    let _g = world_guard();
    let _snap = WorldSnapshot::take();
    let (oh, ow) = test_region();
    let mut warm = ow.clone();
    warm.extend([(0, 0)]);
    set_test_region(warm.clone(), warm.clone());
    let stone = name_to_state("stone").unwrap();
    let sand = name_to_state("sand").unwrap();
    let (x, z, base) = (11, 11, 150);
    for y in base..=base + 6 {
        set_block(x, y, z, 0);
    }
    set_block(x, base, z, stone);
    set_block(x, base + 5, z, sand);
    drain_falling_bookings(8);
    let y0 = {
        let q = falling().lock().unwrap();
        assert_eq!(q.len(), 1, "the callback converted the block into one entity");
        q[0].y
    };

    set_test_region(HashSet::new(), HashSet::new());
    for _ in 0..3 {
        falling_tick();
    }
    let cold = falling().lock().unwrap().clone();
    assert_eq!(cold.len(), 1, "the entity stays in flight");
    assert_eq!(cold[0].y, y0, "a cold chunk must not integrate gravity: {cold:?}");

    set_test_region(warm.clone(), warm.clone());
    falling_tick();
    let q = falling().lock().unwrap().clone();
    assert!(q.len() == 1 && q[0].y < y0, "warming resumes the physics: {q:?}");
    let _ = oh;
}

// ---------- chest world-side contract (M2 slice 3) ----------
//
// Every expectation below is the 1.21.1 mechanism itself, named where it is used:
// `ChestBlock.getStateForPlacement` / `candidatePartnerFacing` / `updateShape`,
// `DoubleBlockCombiner`'s slot order, `Containers.dropContentsOnDestroy` (guarded by the
// *block* changing), and `ContainerOpenersCounter` -> `level.blockEvent(1, count)`.

/// Read one varint straight off a socket; `None` on a close or a read timeout.
fn read_frame_varint(stream: &mut TcpStream) -> Option<i64> {
    let mut value = 0i64;
    for shift in 0..5 {
        let mut byte = [0u8; 1];
        std::io::Read::read_exact(stream, &mut byte).ok()?;
        value |= i64::from(byte[0] & 0x7f) << (7 * shift);
        if byte[0] & 0x80 == 0 {
            return Some(value);
        }
    }
    None
}

/// Every frame that arrived on this socket, as `(packet id, payload)`.
fn frames_on(stream: &mut TcpStream) -> Vec<(i64, Vec<u8>)> {
    let mut out = Vec::new();
    while let Some(len) = read_frame_varint(stream) {
        let mut body = vec![0u8; usize::try_from(len).ok().unwrap_or(0)];
        if std::io::Read::read_exact(stream, &mut body).is_err() {
            break;
        }
        let mut sl = &body[..];
        let Some(pid) = read_varint(&mut sl) else {
            break;
        };
        out.push((pid, sl.to_vec()));
    }
    out
}

/// `ClientboundBlockEventPacket` (0x08) bodies: pos, action id, action param, block id.
fn block_events(frames: Vec<(i64, Vec<u8>)>) -> Vec<(BlockPos, u8, u8, i64)> {
    frames
        .into_iter()
        .filter(|(pid, _)| *pid == 0x08)
        .map(|(_, body)| {
            let at = read_pos(&body[..8]);
            let mut rest = &body[10..];
            (
                at,
                body[8],
                body[9],
                read_varint(&mut rest).expect("block id varint"),
            )
        })
        .collect()
}

/// A client whose socket the server writes into, standing at `at` so the gated lid
/// broadcast reaches it. The audience is passed explicitly, so no other test's
/// broadcast can land on these sockets.
fn chest_viewer(eid: i32, at: BlockPos) -> (Arc<Client>, TcpStream) {
    let (mut client, receiver) = client_pair();
    client.eid = eid;
    *client.pos.lock().unwrap() = (
        f64::from(at.0) + 0.5,
        f64::from(at.1),
        f64::from(at.2) + 0.5,
        0.0,
        0.0,
        true,
    );
    receiver
        .set_read_timeout(Some(std::time::Duration::from_millis(200)))
        .unwrap();
    (Arc::new(client), receiver)
}

fn chest_state(name: &str) -> u32 {
    name_to_state(name).expect("chest block state")
}

/// A chest placed by a player looking at yaw 0 (south), so `facing` is north.
fn placed_chest(name: &str, pos: BlockPos) -> u32 {
    chest_placement_state(name, pos, 0.0, false, 1)
}

/// `Containers.dropContentsOnDestroy` is guarded by `!state.is(newState.getBlock())`:
/// only the block changing may drop a container. Guarding on `st != old` threw the
/// contents away on every state write on the same chest.
#[test]
fn a_chest_state_change_keeps_its_contents() {
    let _world_guard = world_guard();
    let reg = breg().expect("block registry");
    let pos: BlockPos = (40, 160, 40);
    set_block(pos.0, pos.1, pos.2, chest_state("chest"));
    let stack: Stack = (name_to_item("iron_ingot").expect("iron"), 7, Vec::new());
    chests()
        .lock()
        .unwrap()
        .entry(ContainerKey::Block(pos))
        .or_default()
        .insert(0, stack.clone());
    let dropped = drops().lock().unwrap().len();
    for (prop, value) in [("waterlogged", "true"), ("facing", "east"), ("type", "left")] {
        let current = block_at(pos.0, pos.1, pos.2);
        let next = reg.with_prop(current, prop, value);
        assert_ne!(next, current, "{prop} must move the chest's state");
        set_block(pos.0, pos.1, pos.2, next);
        assert_eq!(
            chests()
                .lock()
                .unwrap()
                .get(&ContainerKey::Block(pos))
                .and_then(|slots| slots.get(&0)),
            Some(&stack),
            "{prop} changed the state, not the block, so the contents must stay"
        );
    }
    assert_eq!(
        drops().lock().unwrap().len(),
        dropped,
        "no state change may drop the contents"
    );
    // The break itself does drop, so take the map entry out before clearing the block.
    chests().lock().unwrap().remove(&ContainerKey::Block(pos));
    set_block(pos.0, pos.1, pos.2, 0);
}

/// `ChestBlock.getStateForPlacement`: the placed half takes `left`/`right` from the
/// neighbour it pairs with, and the neighbour is repaired to the opposite half.
#[test]
fn chest_placement_pairs_only_with_a_matching_single_chest() {
    let _world_guard = world_guard();
    let y = 162;
    // Two chests side by side, same facing: the pair axis is perpendicular to facing.
    let left = (41, y, 60);
    let right = (42, y, 60);
    set_block(left.0, left.1, left.2, chest_state("chest"));
    set_block(
        right.0,
        right.1,
        right.2,
        placed_chest("minecraft:chest", right),
    );
    assert_eq!(chest_type(block_at(left.0, left.1, left.2)), Some("left"));
    assert_eq!(chest_type(block_at(right.0, right.1, right.2)), Some("right"));
    assert_eq!(
        chest_facing(block_at(left.0, left.1, left.2)),
        chest_facing(block_at(right.0, right.1, right.2)),
        "both halves carry the placed facing"
    );
    assert_eq!(
        chest_pair(left.0, left.1, left.2),
        Some((right, left)),
        "the pair is ordered right-then-left, as DoubleBlockCombiner orders it"
    );

    // A trap chest beside a chest is a different Block, so `candidatePartnerFacing`
    // rejects it: both stay single.
    let plain = (44, y, 60);
    let trapped = (45, y, 60);
    set_block(plain.0, plain.1, plain.2, chest_state("chest"));
    set_block(
        trapped.0,
        trapped.1,
        trapped.2,
        placed_chest("minecraft:trapped_chest", trapped),
    );
    assert_eq!(chest_type(block_at(plain.0, plain.1, plain.2)), Some("single"));
    assert_eq!(
        chest_type(block_at(trapped.0, trapped.1, trapped.2)),
        Some("single"),
        "a chest and a trapped chest never pair"
    );

    // Three in a row: the middle half is already paired, so it is not a candidate and
    // the third chest stays single (a half cannot belong to two pairs).
    let row = [(47, y, 60), (48, y, 60), (49, y, 60)];
    set_block(row[0].0, row[0].1, row[0].2, chest_state("chest"));
    for cell in &row[1..] {
        set_block(
            cell.0,
            cell.1,
            cell.2,
            placed_chest("minecraft:chest", *cell),
        );
    }
    assert_eq!(chest_type(block_at(row[0].0, row[0].1, row[0].2)), Some("left"));
    assert_eq!(chest_type(block_at(row[1].0, row[1].1, row[1].2)), Some("right"));
    assert_eq!(
        chest_type(block_at(row[2].0, row[2].1, row[2].2)),
        Some("single"),
        "a third chest beside a paired half stays single"
    );
    assert_eq!(chest_partner(row[2].0, row[2].1, row[2].2), None);
    for cell in row.iter().chain([left, right, plain, trapped].iter()) {
        set_block(cell.0, cell.1, cell.2, 0);
    }
}

/// The sneaking branch of `getStateForPlacement`: only the clicked block may become the
/// partner, and only when its facing is on the other axis.
#[test]
fn sneak_placement_claims_the_clicked_face_only_across_the_facing_axis() {
    let _world_guard = world_guard();
    let y = 163;
    // Facing north (yaw 0 flip), so its pair axis is x. Sneak-clicking its NORTH face
    // would connect along z, which the axis test rejects.
    let host = (52, y, 60);
    set_block(host.0, host.1, host.2, chest_state("chest"));
    let along = (52, y, 59);
    set_block(
        along.0,
        along.1,
        along.2,
        chest_placement_state("minecraft:chest", along, 0.0, true, 2),
    );
    assert_eq!(
        chest_type(block_at(along.0, along.1, along.2)),
        Some("single"),
        "a click along the partner's facing axis cannot pair"
    );
    assert_eq!(chest_type(block_at(host.0, host.1, host.2)), Some("single"));
    // Sneak-clicking its EAST face connects across the facing axis.
    let across = (53, y, 60);
    set_block(
        across.0,
        across.1,
        across.2,
        chest_placement_state("minecraft:chest", across, 0.0, true, 5),
    );
    assert_eq!(chest_type(block_at(across.0, across.1, across.2)), Some("right"));
    assert_eq!(
        chest_type(block_at(host.0, host.1, host.2)),
        Some("left"),
        "the clicked chest is repaired to the opposite half"
    );
    for cell in [host, along, across] {
        set_block(cell.0, cell.1, cell.2, 0);
    }
}

/// `ChestBlock.updateShape`: breaking one half reverts the survivor to `single`.
#[test]
fn breaking_a_half_reverts_the_survivor_to_single() {
    let _world_guard = world_guard();
    let y = 164;
    let left = (55, y, 60);
    let right = (56, y, 60);
    set_block(left.0, left.1, left.2, chest_state("chest"));
    set_block(
        right.0,
        right.1,
        right.2,
        placed_chest("minecraft:chest", right),
    );
    assert_eq!(chest_type(block_at(right.0, right.1, right.2)), Some("right"));
    set_block(right.0, right.1, right.2, 0);
    assert_eq!(
        chest_type(block_at(left.0, left.1, left.2)),
        Some("single"),
        "the survivor must not stay a half with no partner"
    );
    assert_eq!(chest_partner(left.0, left.1, left.2), None);
    set_block(left.0, left.1, left.2, 0);
}

/// `DoubleBlockCombiner` order: the combined container is `(right, left)`, so window
/// slots 0..=26 are the right half whichever half was clicked.
#[test]
fn a_double_chest_window_opens_right_half_first() {
    let _world_guard = world_guard();
    let y = 165;
    let left = (58, y, 60);
    let right = (59, y, 60);
    set_block(left.0, left.1, left.2, chest_state("chest"));
    set_block(
        right.0,
        right.1,
        right.2,
        placed_chest("minecraft:chest", right),
    );
    let expected = (
        ContainerKey::Block(right),
        Some(ContainerKey::Block(left)),
    );
    assert_eq!(chest_window(left.0, left.1, left.2), expected, "left half");
    assert_eq!(chest_window(right.0, right.1, right.2), expected, "right half");
    // A single chest is its own window.
    let lone = (61, y, 60);
    set_block(lone.0, lone.1, lone.2, chest_state("chest"));
    assert_eq!(
        chest_window(lone.0, lone.1, lone.2),
        (ContainerKey::Block(lone), None)
    );
    for cell in [left, right, lone] {
        set_block(cell.0, cell.1, cell.2, 0);
    }
}

/// `ContainerOpenersCounter`: one counter per half, and `openerCountChanged` is what
/// carries the new count to every viewer.
#[test]
fn two_viewers_move_the_opener_count_and_broadcast_it() {
    let _world_guard = world_guard();
    let y = 166;
    let right = (63, y, 60);
    let left = (64, y, 60);
    for cell in [right, left] {
        set_block(cell.0, cell.1, cell.2, chest_state("chest"));
    }
    let (first, mut r_first) = chest_viewer(1, left);
    let (second, mut r_second) = chest_viewer(2, right);
    let audience = [first.clone(), second.clone()];
    let halves = [right, left];
    let window = (
        ContainerKey::Block(right),
        Some(ContainerKey::Block(left)),
    );
    let chest_id = i64::from(breg().unwrap().block_id_of(block_at(right.0, right.1, right.2)).unwrap());
    assert_eq!(chest_id, 177, "1.21.1's block id for minecraft:chest");

    let before = chest_opener_snapshot(&audience, &halves);
    assert_eq!(before, vec![(right, 0), (left, 0)]);
    *first.open_chest.lock().unwrap() = Some(window);
    sync_chest_openers(&audience, &before);
    let expected = vec![
        (right, 1u8, 1u8, chest_id),
        (left, 1u8, 1u8, chest_id),
    ];
    assert_eq!(block_events(frames_on(&mut r_first)), expected);
    assert_eq!(
        block_events(frames_on(&mut r_second)),
        expected,
        "a viewer that did not open it still learns the lid moved"
    );

    let before = chest_opener_snapshot(&audience, &halves);
    assert_eq!(before, vec![(right, 1), (left, 1)]);
    *second.open_chest.lock().unwrap() = Some(window);
    sync_chest_openers(&audience, &before);
    let expected = vec![
        (right, 1u8, 2u8, chest_id),
        (left, 1u8, 2u8, chest_id),
    ];
    assert_eq!(block_events(frames_on(&mut r_first)), expected);
    assert_eq!(block_events(frames_on(&mut r_second)), expected);

    let before = chest_opener_snapshot(&audience, &halves);
    assert_eq!(before, vec![(right, 2), (left, 2)]);
    *second.open_chest.lock().unwrap() = None;
    sync_chest_openers(&audience, &before);
    let expected = vec![
        (right, 1u8, 1u8, chest_id),
        (left, 1u8, 1u8, chest_id),
    ];
    assert_eq!(block_events(frames_on(&mut r_first)), expected);

    let before = chest_opener_snapshot(&audience, &halves);
    *first.open_chest.lock().unwrap() = None;
    sync_chest_openers(&audience, &before);
    let expected = vec![
        (right, 1u8, 0u8, chest_id),
        (left, 1u8, 0u8, chest_id),
    ];
    assert_eq!(block_events(frames_on(&mut r_first)), expected);

    // Nothing changed, so nothing is sent: `openerCountChanged` fires on a change.
    let before = chest_opener_snapshot(&audience, &halves);
    sync_chest_openers(&audience, &before);
    assert!(frames_on(&mut r_first).is_empty());
    for cell in [right, left] {
        set_block(cell.0, cell.1, cell.2, 0);
    }
}

/// A container the world no longer has closes the menu built on it, and its opener
/// count drops in the same breath (`ChestBlockEntity.setRemoved` -> recheckOpeners).
#[test]
fn a_removed_container_closes_the_window_built_on_it() {
    let _world_guard = world_guard();
    let y = 167;
    let pos: BlockPos = (66, y, 60);
    set_block(pos.0, pos.1, pos.2, chest_state("chest"));
    let (viewer, mut receiver) = chest_viewer(1, pos);
    *viewer.open_chest.lock().unwrap() = Some((ContainerKey::Block(pos), None));
    let closed = invalidate_container_windows(
        &[viewer.clone()],
        ContainerKey::Block(pos),
        &[pos],
    );
    assert_eq!(closed, 1, "one window was closed");
    assert!(
        viewer.open_chest.lock().unwrap().is_none(),
        "a stale window must not survive the block"
    );
    let frames = frames_on(&mut receiver);
    assert_eq!(frames[0].0, 0x12, "ClientboundContainerClosePacket");
    assert_eq!(frames[0].1, vec![1u8], "windowId 1");
    assert_eq!(
        block_events(frames),
        vec![(pos, 1, 0, 177)],
        "the last viewer left, so the count is 0"
    );
    // Second time round there is nothing to close and the count does not move.
    let again =
        invalidate_container_windows(&[viewer.clone()], ContainerKey::Block(pos), &[pos]);
    assert_eq!(again, 0);
    assert!(frames_on(&mut receiver).is_empty());
    set_block(pos.0, pos.1, pos.2, 0);
}

/// Breaking a chest spills one entity per non-empty stack and leaves no container
/// behind - the guard that makes a later click into the stale window refuse it.
#[test]
fn breaking_a_chest_drops_each_stack_once_and_leaves_no_entry() {
    let _world_guard = world_guard();
    let pos: BlockPos = (68, 168, 60);
    let iron = name_to_item("iron_ingot").unwrap();
    let gold = name_to_item("gold_ingot").unwrap();
    set_block(pos.0, pos.1, pos.2, chest_state("chest"));
    let mut slots: HashMap<i16, Stack> = HashMap::new();
    slots.insert(0, (iron, 5, Vec::new()));
    slots.insert(7, (gold, 2, Vec::new()));
    slots.insert(9, (0, 0, Vec::new()));
    chests()
        .lock()
        .unwrap()
        .insert(ContainerKey::Block(pos), slots);
    let before = drops().lock().unwrap().len();
    set_block(pos.0, pos.1, pos.2, 0);
    assert!(
        !chests().lock().unwrap().contains_key(&ContainerKey::Block(pos)),
        "no container entry may outlive the block"
    );
    assert!(
        !window_live((ContainerKey::Block(pos), None)),
        "the click guard must refuse a window whose block is gone"
    );
    assert!(!container_exists(ContainerKey::Block(pos)));
    let ds = drops().lock().unwrap();
    assert_eq!(
        ds.len(),
        before + 2,
        "one drop per non-empty stack, not one per item"
    );
    // The container is a `HashMap`, so the spill order is not a property of the contract.
    let mut spilled: Vec<_> = ds[before..].iter().map(|d| (d.item, d.cnt)).collect();
    spilled.sort_unstable();
    let mut expected = vec![(iron, 5), (gold, 2)];
    expected.sort_unstable();
    assert_eq!(spilled, expected);
    drop(ds);
    drops().lock().unwrap().truncate(before);
}

/// The pair relationship is block state and the viewer count is not persisted (`the
/// openCount is deliberately not saved`), so a save/reload keeps exactly the chests
/// that exist and their halves, and invents no container.
#[test]
fn chest_pairing_states_and_contents_survive_a_save_and_load() {
    if test_support::isolated_world_test() {
        return;
    }
    let y = 169;
    let left = (70, y, 60);
    let right = (71, y, 60);
    let lone = (73, y, 60);
    set_block(left.0, left.1, left.2, chest_state("chest"));
    set_block(
        right.0,
        right.1,
        right.2,
        placed_chest("minecraft:chest", right),
    );
    set_block(lone.0, lone.1, lone.2, chest_state("chest"));
    assert_eq!(chest_type(block_at(left.0, left.1, left.2)), Some("left"));
    assert_eq!(chest_type(block_at(right.0, right.1, right.2)), Some("right"));
    let iron = name_to_item("iron_ingot").unwrap();
    let gold = name_to_item("gold_ingot").unwrap();
    let diamond = name_to_item("diamond").unwrap();
    for (pos, stack) in [
        (left, (iron, 4, Vec::new())),
        (right, (gold, 1, Vec::new())),
        (lone, (diamond, 3, Vec::new())),
    ] {
        chests()
            .lock()
            .unwrap()
            .entry(ContainerKey::Block(pos))
            .or_default()
            .insert(0, stack);
    }
    let mut payload = Vec::new();
    save_world_into(&mut payload);
    world().lock().unwrap().clear();
    chests().lock().unwrap().clear();
    drops().lock().unwrap().clear();
    load_world_from(&payload).unwrap();
    assert_eq!(
        chest_type(block_at(left.0, left.1, left.2)),
        Some("left"),
        "the half is saved in the block state"
    );
    assert_eq!(chest_type(block_at(right.0, right.1, right.2)), Some("right"));
    assert_eq!(
        chest_pair(left.0, left.1, left.2),
        Some((right, left)),
        "the pair is re-derived from the reloaded states"
    );
    let containers = chests().lock().unwrap().clone();
    let keys: HashSet<ContainerKey> = containers.keys().copied().collect();
    assert_eq!(
        keys,
        HashSet::from([
            ContainerKey::Block(left),
            ContainerKey::Block(right),
            ContainerKey::Block(lone),
        ]),
        "exactly the three chests: no phantom key and no persisted viewer count"
    );
    assert_eq!(
        containers[&ContainerKey::Block(left)].get(&0),
        Some(&(iron, 4, Vec::new()))
    );
    assert_eq!(
        containers[&ContainerKey::Block(right)].get(&0),
        Some(&(gold, 1, Vec::new()))
    );
    assert_eq!(
        containers[&ContainerKey::Block(lone)].get(&0),
        Some(&(diamond, 3, Vec::new()))
    );
    for cell in [left, right, lone] {
        chests().lock().unwrap().remove(&ContainerKey::Block(cell));
        set_block(cell.0, cell.1, cell.2, 0);
    }
    drops().lock().unwrap().clear();
}

// ---------- chest world-side contract (M2 slice 3c) ----------
//
// `ChestBlock.isChestBlockedAt` / `isBlockedChestByBlock` / `isCatSittingOnChest`,
// `AbstractContainerMenu.broadcastChanges`, and `ChestBlockEntity.recheckOpeners` ->
// `Container.stillValidBlockEntity` (`Player.canInteractWithBlock(pos, 4.0)`).

fn decode_set_slot_frames(frames: Vec<(i64, Vec<u8>)>) -> Vec<(u8, i64, i16, Stack)> {
    frames
        .into_iter()
        .filter(|(pid, _)| *pid == 0x15)
        .map(|(_, body)| {
            let wid = body[0];
            let mut sl = &body[1..];
            let state_id = read_varint(&mut sl).expect("set_slot state_id");
            let (slot, stack) = slot_read_wire(sl).expect("set_slot item wire");
            (wid, state_id, slot, stack)
        })
        .collect()
}

fn fast_chest_viewer(eid: i32, at: BlockPos) -> (Arc<Client>, TcpStream) {
    let (viewer, receiver) = chest_viewer(eid, at);
    receiver
        .set_read_timeout(Some(std::time::Duration::from_millis(25)))
        .unwrap();
    (viewer, receiver)
}

#[test]
fn chest_contract_redstone_conductor_matches_1211_rules() {
    crate::test_support::require_vanilla_data();
    let _world_guard = world_guard();
    let reg = breg().expect("block registry");
    let cell: BlockPos = (74, 165, 60);

    // Conductors in 1.21.1 `BlockStateBase.isRedstoneConductor`: full collision blocks plus
    // `mud` and `soul_sand`, excluding transparent/redstone exceptions.
    let slab_base = name_to_state("oak_slab").expect("oak_slab");
    let double_slab = reg.with_prop(slab_base, "type", "double");
    let bottom_slab = reg.with_prop(slab_base, "type", "bottom");
    let top_slab = reg.with_prop(slab_base, "type", "top");
    let conductor_states: Vec<(&str, u32)> = [
        "stone",
        "oak_planks",
        "cobblestone",
        "deepslate",
        "bricks",
        "mud",
        "soul_sand",
    ]
    .into_iter()
    .map(|name| (name, name_to_state(name).unwrap_or_else(|| panic!("state for {name}"))))
    .chain(std::iter::once(("oak_slab[type=double]", double_slab)))
    .collect();
    for (name, st) in conductor_states {
        assert!(
            reg.is_redstone_conductor(st),
            "{name} must be isRedstoneConductor=true"
        );
        set_block(cell.0, cell.1 + 1, cell.2, st);
        assert!(
            is_blocked_chest_by_block(cell),
            "{name} above chest must block"
        );
    }

    let non_conductor_states: Vec<(&str, u32)> = [
        "air",
        "glass",
        "white_stained_glass",
        "tinted_glass",
        "ice",
        "frosted_ice",
        "beacon",
        "oak_leaves",
        "glowstone",
        "sea_lantern",
        "redstone_block",
        "tnt",
        "observer",
        "piston",
        "sticky_piston",
        "copper_grate",
        "torch",
        "chest",
        "trapped_chest",
        "ender_chest",
        "water",
    ]
    .into_iter()
    .map(|name| (name, name_to_state(name).unwrap_or_else(|| panic!("state for {name}"))))
    .chain([
        ("oak_slab[type=bottom]", bottom_slab),
        ("oak_slab[type=top]", top_slab),
    ])
    .collect();
    for (name, st) in non_conductor_states {
        assert!(
            !reg.is_redstone_conductor(st),
            "{name} must be isRedstoneConductor=false"
        );
        set_block(cell.0, cell.1 + 1, cell.2, st);
        assert!(
            !is_blocked_chest_by_block(cell),
            "{name} above chest must NOT block"
        );
    }
    set_block(cell.0, cell.1 + 1, cell.2, 0);
}

#[test]
fn chest_contract_blocked_single_double_cat_and_unblocked_barrel() {
    crate::test_support::require_vanilla_data();
    let _world_guard = world_guard();
    let y = 166;
    let stone = name_to_state("stone").unwrap();
    let glass = name_to_state("glass").unwrap();

    // Single chest blocked by stone, opened by glass.
    let single: BlockPos = (76, y, 60);
    set_block(single.0, single.1, single.2, chest_state("chest"));
    set_block(single.0, single.1 + 1, single.2, stone);
    assert!(is_chest_blocked_at(single));
    assert!(chest_window_blocked((ContainerKey::Block(single), None)));
    set_block(single.0, single.1 + 1, single.2, glass);
    assert!(!is_chest_blocked_at(single));
    assert!(!chest_window_blocked((ContainerKey::Block(single), None)));
    set_block(single.0, single.1 + 1, single.2, 0);

    // Double chest: blocked if EITHER right OR left half has a conductor above.
    let left: BlockPos = (78, y, 60);
    let right: BlockPos = (79, y, 60);
    set_block(left.0, left.1, left.2, chest_state("chest"));
    set_block(
        right.0,
        right.1,
        right.2,
        placed_chest("minecraft:chest", right),
    );
    let win = chest_window(left.0, left.1, left.2);
    assert_eq!(
        win,
        (ContainerKey::Block(right), Some(ContainerKey::Block(left)))
    );
    assert!(!chest_window_blocked(win));

    set_block(right.0, right.1 + 1, right.2, stone);
    assert!(
        chest_window_blocked(win),
        "stone above right half only must block the double chest"
    );
    set_block(right.0, right.1 + 1, right.2, 0);
    set_block(left.0, left.1 + 1, left.2, stone);
    assert!(
        chest_window_blocked(win),
        "stone above left half only must block the double chest"
    );
    set_block(left.0, left.1 + 1, left.2, glass);
    assert!(!chest_window_blocked(win), "glass above allows open");
    set_block(left.0, left.1 + 1, left.2, 0);

    // Sitting cat (`etype == 15`) intersecting AABB(x, y+1, z, x+1, y+2, z+1) blocks either half.
    let cat_eid = 910_001;
    let cat = Arc::new(Mob {
        eid: cat_eid,
        uuid: [0xCA; 16],
        etype: 15,
        pos: Mutex::new((
            f64::from(left.0) + 0.5,
            f64::from(left.1) + 1.0,
            f64::from(left.2) + 0.5,
            0.0,
            0.0,
        )),
        hp: Mutex::new(10.0),
        last_hit: Mutex::new(Instant::now()),
        hurt_at: Mutex::new(Instant::now()),
        last_hurt_tick: Mutex::new(None),
        fuse: Mutex::new(0),
        love: Mutex::new(0),
        air: Mutex::new(300),
    });
    mobs().lock().unwrap().push(cat.clone());
    assert!(
        !chest_window_blocked(win),
        "standing cat does not block ChestBlock.isCatSittingOnChest"
    );
    sitting_cats().lock().unwrap().insert(cat_eid);
    assert!(
        is_cat_sitting_on_chest(left),
        "sitting cat on left cell blocks left half"
    );
    assert!(
        chest_window_blocked(win),
        "sitting cat on left half blocks double chest window"
    );
    // Moving the sitting cat off the chest unblocks it.
    *cat.pos.lock().unwrap() = (
        f64::from(left.0) + 0.5,
        f64::from(left.1) + 2.5,
        f64::from(left.2) + 0.5,
        0.0,
        0.0,
    );
    assert!(!chest_window_blocked(win));
    sitting_cats().lock().unwrap().remove(&cat_eid);
    mobs().lock().unwrap().retain(|m| m.eid != cat_eid);

    for cell in [single, left, right] {
        set_block(cell.0, cell.1, cell.2, 0);
    }
}

#[test]
fn chest_contract_multi_viewer_broadcast_changes() {
    crate::test_support::require_vanilla_data();
    let _world_guard = world_guard();
    let y = 167;
    let left: BlockPos = (82, y, 60);
    let right: BlockPos = (83, y, 60);
    let other: BlockPos = (86, y, 60);
    set_block(left.0, left.1, left.2, chest_state("chest"));
    set_block(
        right.0,
        right.1,
        right.2,
        placed_chest("minecraft:chest", right),
    );
    set_block(other.0, other.1, other.2, chest_state("chest"));
    let win = chest_window(left.0, left.1, left.2);
    assert_eq!(
        win,
        (ContainerKey::Block(right), Some(ContainerKey::Block(left)))
    );
    let other_win = (ContainerKey::Block(other), None);

    let (v1, mut r1) = fast_chest_viewer(101, right);
    let (v2, mut r2) = fast_chest_viewer(102, left);
    let (v3, mut r3) = fast_chest_viewer(103, other);
    let audience = [v1.clone(), v2.clone(), v3.clone()];

    open_chest_for_client(&v1, win, None, &HashMap::new(), "Chest");
    open_chest_for_client(&v2, win, None, &HashMap::new(), "Chest");
    open_chest_for_client(&v3, other_win, None, &HashMap::new(), "Chest");
    // Drain initial 0x33 + 0x13 packets.
    assert!(frames_on(&mut r1).iter().any(|(pid, _)| *pid == 0x33));
    assert!(frames_on(&mut r2).iter().any(|(pid, _)| *pid == 0x33));
    assert!(frames_on(&mut r3).iter().any(|(pid, _)| *pid == 0x33));

    // Idle tick must emit no 0x15 updates.
    chest_viewer_tick_on(&audience);
    assert!(frames_on(&mut r1).is_empty());
    assert!(frames_on(&mut r2).is_empty());
    assert!(frames_on(&mut r3).is_empty());

    // Mutate right half slot 3 and left half slot 5 (window slots 3 and 32).
    let iron = name_to_item("iron_ingot").unwrap();
    let gold = name_to_item("gold_ingot").unwrap();
    let s_iron: Stack = (iron, 7, Vec::new());
    let s_gold: Stack = (gold, 4, Vec::new());
    {
        let mut all = chests().lock().unwrap();
        all.entry(ContainerKey::Block(right))
            .or_default()
            .insert(3, s_iron.clone());
        all.entry(ContainerKey::Block(left))
            .or_default()
            .insert(5, s_gold.clone());
    }
    chest_viewer_tick_on(&audience);
    let expected = vec![
        (1u8, 1i64, 3i16, s_iron.clone()),
        (1u8, 2i64, 32i16, s_gold.clone()),
    ];
    assert_eq!(decode_set_slot_frames(frames_on(&mut r1)), expected);
    assert_eq!(decode_set_slot_frames(frames_on(&mut r2)), expected);
    assert!(
        frames_on(&mut r3).is_empty(),
        "viewer of a separate chest must not receive updates"
    );

    // Viewer 1 takes slot 32 via click (`send_chest_slot`), so only viewer 2 gets tick diff.
    chests()
        .lock()
        .unwrap()
        .entry(ContainerKey::Block(left))
        .or_default()
        .remove(&5);
    let empty: Stack = (0, 0, Vec::new());
    send_chest_slot(&v1, 32, &empty);
    assert_eq!(
        decode_set_slot_frames(frames_on(&mut r1)),
        vec![(1u8, 3i64, 32i16, empty.clone())]
    );
    chest_viewer_tick_on(&audience);
    assert!(
        frames_on(&mut r1).is_empty(),
        "acting viewer already synced slot 32 via click echo"
    );
    assert_eq!(
        decode_set_slot_frames(frames_on(&mut r2)),
        vec![(1u8, 3i64, 32i16, empty)]
    );

    for cell in [right, left, other] {
        chests().lock().unwrap().remove(&ContainerKey::Block(cell));
        set_block(cell.0, cell.1, cell.2, 0);
    }
}

#[test]
fn chest_contract_recheck_openers_reach_and_lid_close() {
    crate::test_support::require_vanilla_data();
    let _world_guard = world_guard();
    let y = 168;
    let left: BlockPos = (88, y, 60);
    let right: BlockPos = (89, y, 60);
    let chest_id = i64::from(breg().unwrap().block_id_of(chest_state("chest")).unwrap());
    set_block(left.0, left.1, left.2, chest_state("chest"));
    set_block(
        right.0,
        right.1,
        right.2,
        placed_chest("minecraft:chest", right),
    );
    let win = chest_window(left.0, left.1, left.2);
    assert_eq!(
        win,
        (ContainerKey::Block(right), Some(ContainerKey::Block(left)))
    );

    // Viewer 1 interacts; Observer sits adjacent to watch 0x08 lid events.
    let (v1, mut r1) = fast_chest_viewer(201, right);
    let (obs, mut r_obs) = fast_chest_viewer(202, right);
    *v1.gm.lock().unwrap() = 0;
    let audience = [v1.clone(), obs.clone()];

    // Check exact `Player.canInteractWithBlock(pos, 4.0)` boundary:
    // For survival (`4.5 + 4.0 = 8.5`), eye at z = 69.40 gives dz = 8.40, dy = 0.62 -> dist = 8.423 < 8.5.
    *v1.pos.lock().unwrap() = (89.5, f64::from(y), 69.40, 0.0, 0.0, true);
    let before = chest_opener_snapshot(&audience, &[right, left]);
    open_chest_for_client(&v1, win, None, &HashMap::new(), "Chest");
    sync_chest_openers(&audience, &before);
    let _ = frames_on(&mut r1);
    assert_eq!(
        block_events(frames_on(&mut r_obs)),
        vec![(right, 1, 1, chest_id), (left, 1, 1, chest_id)]
    );

    chest_viewer_tick_on(&audience);
    assert!(
        v1.open_chest.lock().unwrap().is_some(),
        "inside 8.5 block reach stays open"
    );
    assert!(frames_on(&mut r1).is_empty());
    assert!(frames_on(&mut r_obs).is_empty());

    // Move to z = 69.49: dist = sqrt(0.62^2 + 8.49^2) = 8.513 (> 8.5 survival, < 9.0 creative).
    *v1.gm.lock().unwrap() = 1;
    *v1.pos.lock().unwrap() = (89.5, f64::from(y), 69.49, 0.0, 0.0, true);
    chest_viewer_tick_on(&audience);
    assert!(
        v1.open_chest.lock().unwrap().is_some(),
        "creative range 5.0 + 4.0 = 9.0 keeps chest open at 8.513"
    );

    // Switch back to survival at 8.513 -> closes window (`0x12`), drops both halves to 0 (`0x08`).
    *v1.gm.lock().unwrap() = 0;
    chest_viewer_tick_on(&audience);
    assert!(v1.open_chest.lock().unwrap().is_none());
    assert!(v1.chest_sync.lock().unwrap().is_none());
    let v1_frames = frames_on(&mut r1);
    assert!(
        v1_frames.iter().any(|(pid, body)| *pid == 0x12 && *body == vec![1u8]),
        "out-of-reach viewer must receive ClientboundContainerClosePacket(1)"
    );
    assert_eq!(
        block_events(frames_on(&mut r_obs)),
        vec![(right, 1, 0, chest_id), (left, 1, 0, chest_id)],
        "closing out-of-reach viewer must broadcast lid close (count=0) on both halves"
    );

    // Double chest: within 8.5 of `right` (x = 97.5 -> dx = 7.5) but > 8.5 from `left` (dx = 8.5)
    // must still fail `CompoundContainer.stillValid` and close.
    *v1.pos.lock().unwrap() = (97.5, f64::from(y), 60.5, 0.0, 0.0, true);
    open_chest_for_client(&v1, win, None, &HashMap::new(), "Chest");
    let _ = frames_on(&mut r1);
    chest_viewer_tick_on(&audience);
    assert!(
        v1.open_chest.lock().unwrap().is_none(),
        "double chest must close when farthest half is beyond reach"
    );
    let _ = frames_on(&mut r1);
    let _ = frames_on(&mut r_obs);

    // Crouching eye height (1.27 vs 1.62) affects vertical reach cleanly.
    *v1.pos.lock().unwrap() = (89.5, f64::from(y) + 7.89, 60.5, 0.0, 0.0, true);
    *v1.sneaking.lock().unwrap() = true;
    open_chest_for_client(&v1, win, None, &HashMap::new(), "Chest");
    let _ = frames_on(&mut r1);
    chest_viewer_tick_on(&audience);
    assert!(
        v1.open_chest.lock().unwrap().is_some(),
        "crouching eye height (y + 1.27 -> dy = 8.16 < 8.5) stays open"
    );
    *v1.sneaking.lock().unwrap() = false;
    chest_viewer_tick_on(&audience);
    assert!(
        v1.open_chest.lock().unwrap().is_none(),
        "standing up lifts eye to dy = 8.51 > 8.5 and closes"
    );

    // Ender chest anchor reach check (`is_ender_key`).
    let _ = frames_on(&mut r1);
    let ender_pos: BlockPos = (90, y, 60);
    set_block(
        ender_pos.0,
        ender_pos.1,
        ender_pos.2,
        breg().expect("block registry").state_of("minecraft:ender_chest", None),
    );
    let ek = ContainerKey::Block(ender_key(&v1.uuid));
    *v1.pos.lock().unwrap() = (88.5, f64::from(y), 60.5, 0.0, 0.0, true);
    open_chest_for_client(&v1, (ek, None), Some(ender_pos), &HashMap::new(), "Ender Chest");
    let _ = frames_on(&mut r1);
    chest_viewer_tick_on(&audience);
    assert!(v1.open_chest.lock().unwrap().is_some());
    *v1.pos.lock().unwrap() = (100.0, f64::from(y), 60.5, 0.0, 0.0, true);
    chest_viewer_tick_on(&audience);
    assert!(v1.open_chest.lock().unwrap().is_none());
    assert!(frames_on(&mut r1)
        .iter()
        .any(|(pid, body)| *pid == 0x12 && *body == vec![1u8]));

    for cell in [left, right, ender_pos] {
        set_block(cell.0, cell.1, cell.2, 0);
    }
}

// --- Behaviour identity -----------------------------------------------------------------
//
// A tick-cost change is only allowed if the world it produces is the same world. This
// pins that: build a deterministic scenario, run it, serialize the save payload and hash
// it. The fixture keeps the block overlay empty on purpose (no crops, no block edits),
// because `save_world_into` walks that overlay as a `HashMap` and its byte order would
// then come from the hasher's random seed instead of from the simulation.

fn payload_digest(bytes: &[u8]) -> String {
    // FNV-1a 128: no dependency, deterministic across processes, and wide enough that a
    // single changed byte cannot land on the same digest.
    let mut hash: u128 = 0x6c62_272e_07bb_0142_62b8_2175_6295_c58d;
    for byte in bytes {
        hash ^= *byte as u128;
        hash = hash.wrapping_mul(0x0000_0000_0100_0000_0000_0000_0000_013b);
    }
    format!("{hash:032x}")
}

struct IdentityFixture {
    orbs: usize,
    drops: usize,
}

fn identity_fixture(orbs: &mut Vec<Xp>, items: &mut Vec<Drop>) -> IdentityFixture {
    let mut eid = 40_000;
    let mut orb = |x: f64, y: f64, z: f64, val: i16, age: i32| {
        orbs.push(Xp {
            eid,
            x,
            y,
            z,
            val,
            age,
        });
        eid += 1;
    };
    // Coincident, then a chain where one step is in range and the next is not.
    for i in 0..6 {
        orb(0.5, 70.0, 0.5, 1, i);
    }
    for i in 0..6 {
        orb(0.2 + i as f64 * 0.8, 70.0, 0.5, (i % 3) as i16 + 1, i);
    }
    // Integer-boundary straddles on all three axes at once.
    for i in 0..4 {
        orb(16.99 - i as f64 * 0.9, 70.99, 0.99, 2, 5 - i);
    }
    // Negative coordinates: floor is not trunc, so these are one cell apart.
    for i in 0..4 {
        orb(-0.01 - i as f64 * 0.9, 70.0, -0.5, 1, i * 2);
    }
    // Out of range: exactly one block apart must never merge.
    for i in 0..8 {
        orb(200.0 + i as f64, 70.0, 200.0, 1, i);
    }
    // Same cell, y straddling the boundary.
    for i in 0..8 {
        orb(300.5, 70.0 + (i % 2) as f64 * 0.98, 300.5, 3, i);
    }
    let mut deid = 50_000;
    let mut push_drop = |x: f64, z: f64, item: i64, age: i32| {
        items.push(Drop {
            eid: deid,
            x,
            y: 70.0,
            z,
            item,
            cnt: 1,
            comps: Vec::new(),
            age,
            target: None,
            thrower: None,
        });
        deid += 1;
    };
    for i in 0..6 {
        push_drop(8.5, 8.5, 811, i);
    }
    for i in 0..6 {
        push_drop(8.2 + i as f64 * 0.2, 8.5, 812, i);
    }
    IdentityFixture {
        orbs: orbs.len(),
        drops: items.len(),
    }
}

#[test]
fn the_tick_preserves_the_saved_world_payload_byte_for_byte() {
    // The payload carries process-wide state - the world clock, the weather budget, the
    // random stream - and the digest is only meaningful if that state started where the
    // fixture says it did. A test running beside this one can advance any of them between
    // two ticks, which showed up as a second, perfectly stable digest under
    // `--test-threads=8` and the pinned one under a single thread. A fresh process is the
    // fixture's precondition, not a workaround, so it runs in one.
    if crate::test_support::isolated_world_test() {
        return;
    }
    let _world_guard = world_guard();
    // Self-check first: this must be the FNV-1a 128 offset basis, or the digest below is
    // not the function it claims to be.
    assert_eq!(
        payload_digest(b""),
        "6c62272e07bb014262b821756295c58d",
        "payload_digest is not FNV-1a 128"
    );
    let (old_hot, old_warm) = test_region();
    let old_drops = std::mem::take(&mut *drops().lock().unwrap());
    let old_orbs = std::mem::take(&mut *xps().lock().unwrap());
    let old_forced = std::mem::take(&mut *forceloaded().lock().unwrap());
    let old_tick = TICK.swap(1, Ordering::Relaxed);
    let old_index = TICK_IDX.swap(0, Ordering::Relaxed);
    let old_time = WORLD_TIME.swap(1000, Ordering::Relaxed);
    let old_weather = (
        RAINING.swap(0, Ordering::Relaxed),
        THUNDERING.swap(0, Ordering::Relaxed),
    );
    let old_rng = world_rng::saved_state();
    // Exactly one forceloaded chunk, so the orbs and drops in it age through the tick
    // while the payload stays deterministic: `save_world_into` writes the forceload set
    // in `HashSet` iteration order, and one element has only one order.
    let chunks: HashSet<(i32, i32)> = std::iter::once((0, 0)).collect();
    *forceloaded().lock().unwrap() = chunks.clone();
    set_test_region(chunks.clone(), chunks);

    let run_once = || {
        let mut orbs = Vec::new();
        let mut items = Vec::new();
        let counts = identity_fixture(&mut orbs, &mut items);
        *xps().lock().unwrap() = orbs;
        *drops().lock().unwrap() = items;
        TICK.store(1, Ordering::Relaxed);
        TICK_IDX.store(0, Ordering::Relaxed);
        WORLD_TIME.store(1000, Ordering::Relaxed);
        // Weather and the world's random stream are part of the save, so they are part
        // of what "the same world" means: pin the stream to one seed and start dry. The
        // digest is then also sensitive to the order and count of RNG draws.
        RAINING.store(0, Ordering::Relaxed);
        THUNDERING.store(0, Ordering::Relaxed);
        world_rng::restore(0x5EED_0000_0000_1234);
        for _ in 0..200 {
            game_tick(|_, tick| tick());
        }
        let left = (
            xps().lock().unwrap().len(),
            drops().lock().unwrap().len(),
        );
        let mut payload = Vec::new();
        save_world_into(&mut payload);
        let overlay = world().lock().unwrap().len();
        let mut state: Vec<String> = Vec::new();
        for x in xps().lock().unwrap().iter() {
            state.push(format!("orb {} {} {} {} {} {}", x.eid, x.x, x.y, x.z, x.val, x.age));
        }
        for d in drops().lock().unwrap().iter() {
            state.push(format!("drop {} {} {} {} {} {}", d.eid, d.x, d.y, d.z, d.item, d.age));
        }
        (payload, left, counts, overlay, state)
    };
    let (first_payload, first_left, counts, first_overlay, first_state) = run_once();
    // The digest is the oracle, so prove it is stable before trusting it.
    let (second_payload, second_left, _, second_overlay, second_state) = run_once();
    if first_state != second_state {
        for (a, b) in first_state.iter().zip(second_state.iter()) {
            eprintln!("A {a}\nB {b}");
        }
        eprintln!("A len {} B len {}", first_state.len(), second_state.len());
    }
    if first_payload != second_payload {
        let at = first_payload
            .iter()
            .zip(second_payload.iter())
            .position(|(a, b)| a != b);
        panic!(
            "the fixture itself is not deterministic: first difference at {at:?}, \
             lens {} / {}, overlay {} / {}, orbs {} / {}, drops {} / {}",
            first_payload.len(),
            second_payload.len(),
            first_overlay,
            second_overlay,
            first_left.0,
            second_left.0,
            first_left.1,
            second_left.1
        );
    }
    let first = payload_digest(&first_payload);
    assert_eq!(first_left, second_left);
    assert!(
        first_left.0 < counts.orbs - 10,
        "the fixture must merge orbs, {} of {} left",
        first_left.0,
        counts.orbs
    );
    assert!(
        first_left.1 < counts.drops,
        "the fixture must merge drops, {} of {} left",
        first_left.1,
        counts.drops
    );
    eprintln!("IDENTITY {first} orbs={} drops={}", first_left.0, first_left.1);

    *drops().lock().unwrap() = old_drops;
    *xps().lock().unwrap() = old_orbs;
    *forceloaded().lock().unwrap() = old_forced;
    TICK.store(old_tick, Ordering::Relaxed);
    TICK_IDX.store(old_index, Ordering::Relaxed);
    WORLD_TIME.store(old_time, Ordering::Relaxed);
    RAINING.store(old_weather.0, Ordering::Relaxed);
    THUNDERING.store(old_weather.1, Ordering::Relaxed);
    world_rng::restore(old_rng);
    set_test_region(old_hot, old_warm);
    assert_eq!(first, "54d762909d3321a41cfd670c7ff0baa9");
}

// ---------- chest world-side contract remainder (M2 slice 3d) ----------

fn decode_block_updates(frames: Vec<(i64, Vec<u8>)>) -> Vec<(BlockPos, u32)> {
    frames
        .into_iter()
        .filter(|(pid, _)| *pid == 0x09)
        .filter_map(|(_, body)| {
            if body.len() < 9 {
                return None;
            }
            let pos = read_pos(&body[..8]);
            let mut sl = &body[8..];
            let state = read_varint(&mut sl)? as u32;
            Some((pos, state))
        })
        .collect()
}

#[test]
fn chest_contract_3d_block_loot_stays_lazy_until_first_open() {
    crate::test_support::require_vanilla_data();
    let _world_guard = world_guard();
    let _snapshot = WorldSnapshot::take();

    let reg = breg().expect("block registry");
    let chest_pos: BlockPos = (92, 170, 60);
    let save_pos: BlockPos = (93, 170, 60);
    let break_pos: BlockPos = (94, 170, 60);
    set_block(chest_pos.0, chest_pos.1, chest_pos.2, chest_state("chest"));
    set_block(save_pos.0, save_pos.1, save_pos.2, reg.state_of("minecraft:barrel", None));
    set_block(break_pos.0, break_pos.1, break_pos.2, chest_state("chest"));

    let payloads = vec![
        (
            chest_pos,
            crate::generation_payload::GenerationPayload::LootContainer {
                kind: crate::generation_payload::ContainerKind::Chest,
                loot_table: "minecraft:chests/abandoned_mineshaft".to_owned(),
                loot_seed: Some(424242),
            },
        ),
        (
            save_pos,
            crate::generation_payload::GenerationPayload::LootContainer {
                kind: crate::generation_payload::ContainerKind::BlockContainer(27),
                loot_table: "minecraft:chests/abandoned_mineshaft".to_owned(),
                loot_seed: Some(13579),
            },
        ),
        (
            break_pos,
            crate::generation_payload::GenerationPayload::LootContainer {
                kind: crate::generation_payload::ContainerKind::Chest,
                loot_table: "minecraft:chests/abandoned_mineshaft".to_owned(),
                loot_seed: Some(24680),
            },
        ),
    ];
    materialize_generation_payloads(payloads);

    assert!(
        !chests().lock().unwrap().contains_key(&ContainerKey::Block(chest_pos)),
        "placed chest with LootTable must remain unmaterialized before first open"
    );
    assert!(
        !chests().lock().unwrap().contains_key(&ContainerKey::Block(save_pos)),
        "placed barrel with LootTable must remain unmaterialized before first open"
    );

    // Save and reload before opening preserves unexpanded LootTable+LootTableSeed.
    let mut saved = Vec::new();
    save_world_into(&mut saved);
    chests().lock().unwrap().clear();
    load_world_from(&saved).unwrap();
    assert!(
        !chests().lock().unwrap().contains_key(&ContainerKey::Block(save_pos)),
        "save/reload before opening must not expand LootTable early"
    );

    // Non-spectator open unpacks deterministically once and clears pending metadata.
    let (viewer, _rx) = fast_chest_viewer(301, chest_pos);
    *viewer.gm.lock().unwrap() = 0;
    open_chest_for_client(
        &viewer,
        (ContainerKey::Block(chest_pos), None),
        None,
        &HashMap::new(),
        "Chest",
    );
    let expected_chest = roll_cart_loot(424242);
    assert_eq!(
        chests().lock().unwrap().get(&ContainerKey::Block(chest_pos)).cloned(),
        Some(expected_chest.clone())
    );
    // Clearing slot 0 and re-opening must NOT re-roll the loot table.
    chests()
        .lock()
        .unwrap()
        .entry(ContainerKey::Block(chest_pos))
        .or_default()
        .clear();
    open_chest_for_client(
        &viewer,
        (ContainerKey::Block(chest_pos), None),
        None,
        &HashMap::new(),
        "Chest",
    );
    assert!(
        chests()
            .lock()
            .unwrap()
            .get(&ContainerKey::Block(chest_pos))
            .unwrap()
            .is_empty(),
        "loot table must clear after first unpack and not refill on second open"
    );

    // Saved/reloaded barrel unpacks deterministically from its saved LootTableSeed.
    open_chest_for_client(
        &viewer,
        (ContainerKey::Block(save_pos), None),
        None,
        &HashMap::new(),
        "Barrel",
    );
    assert_eq!(
        chests().lock().unwrap().get(&ContainerKey::Block(save_pos)).cloned(),
        Some(roll_cart_loot(13579))
    );

    // Breaking an unopened loot chest materializes and spills its drops.
    let drops_before = drops().lock().unwrap().len();
    set_block(break_pos.0, break_pos.1, break_pos.2, 0);
    let expected_drop_count = roll_cart_loot(24680)
        .values()
        .filter(|(it, cnt, _)| *it > 0 && *cnt > 0)
        .count();
    assert_eq!(
        drops().lock().unwrap().len(),
        drops_before + expected_drop_count,
        "breaking an unopened loot chest must unpack and drop its rolled loot stacks"
    );

    // Custom datapack loot table rolls deterministically from explicit LootTableSeed.
    let custom_table = "test:slice3d_seeded_loot";
    loot_tables().lock().unwrap().insert(
        custom_table.to_owned(),
        vec![LootPool {
            rolls: 3,
            chance: 1.0,
            entries: vec![
                LootEntry { item: 264, min: 1, max: 4, funcs: Vec::new() },
                LootEntry { item: 265, min: 2, max: 7, funcs: Vec::new() },
                LootEntry { item: 266, min: 1, max: 3, funcs: Vec::new() },
            ],
            funcs: Vec::new(),
        }],
    );
    let seed_a: BlockPos = (95, 170, 60);
    let seed_b: BlockPos = (95, 170, 62);
    set_block(seed_a.0, seed_a.1, seed_a.2, chest_state("chest"));
    set_block(seed_b.0, seed_b.1, seed_b.2, chest_state("chest"));
    materialize_generation_payloads(vec![
        (
            seed_a,
            crate::generation_payload::GenerationPayload::LootContainer {
                kind: crate::generation_payload::ContainerKind::Chest,
                loot_table: custom_table.to_owned(),
                loot_seed: Some(987654321),
            },
        ),
        (
            seed_b,
            crate::generation_payload::GenerationPayload::LootContainer {
                kind: crate::generation_payload::ContainerKind::Chest,
                loot_table: custom_table.to_owned(),
                loot_seed: Some(987654321),
            },
        ),
    ]);
    open_chest_for_client(
        &viewer,
        (ContainerKey::Block(seed_a), None),
        None,
        &HashMap::new(),
        "Chest",
    );
    open_chest_for_client(
        &viewer,
        (ContainerKey::Block(seed_b), None),
        None,
        &HashMap::new(),
        "Chest",
    );
    let rolled_a = chests().lock().unwrap().get(&ContainerKey::Block(seed_a)).cloned();
    let rolled_b = chests().lock().unwrap().get(&ContainerKey::Block(seed_b)).cloned();
    assert!(rolled_a.as_ref().is_some_and(|m| !m.is_empty()));
    assert_eq!(
        rolled_a, rolled_b,
        "custom loot_table with explicit LootTableSeed must materialize identically on first open"
    );
    loot_tables().lock().unwrap().remove(custom_table);

    for pos in [chest_pos, save_pos, break_pos, seed_a, seed_b] {
        chests().lock().unwrap().remove(&ContainerKey::Block(pos));
        set_block(pos.0, pos.1, pos.2, 0);
    }
}

#[test]
fn chest_contract_3d_barrel_toggles_open_property_without_block_event() {
    crate::test_support::require_vanilla_data();
    let _world_guard = world_guard();
    let _snapshot = WorldSnapshot::take();

    let reg = breg().expect("block registry");
    let barrel_pos: BlockPos = (96, 170, 60);
    let closed_north = reg.with_prop(
        reg.with_prop(reg.state_of("minecraft:barrel", None), "facing", "north"),
        "open",
        "false",
    );
    let open_north = reg.with_prop(closed_north, "open", "true");
    set_block(barrel_pos.0, barrel_pos.1, barrel_pos.2, closed_north);

    let (v1, mut r1) = fast_chest_viewer(311, barrel_pos);
    let (v2, mut r2) = fast_chest_viewer(312, barrel_pos);
    let (obs, mut r_obs) = fast_chest_viewer(313, barrel_pos);
    let audience = [v1.clone(), v2.clone(), obs.clone()];
    let win = (ContainerKey::Block(barrel_pos), None);

    // 0 -> 1: open becomes true preserving facing=north, 0x09 broadcast, NO 0x08.
    let before = chest_opener_snapshot(&audience, &[barrel_pos]);
    open_chest_for_client(&v1, win, None, &HashMap::new(), "Barrel");
    sync_chest_openers(&audience, &before);
    assert_eq!(block_at(barrel_pos.0, barrel_pos.1, barrel_pos.2), open_north);
    let obs_frames = frames_on(&mut r_obs);
    assert!(
        block_events(obs_frames.clone()).is_empty(),
        "BarrelBlockEntity must never emit 0x08 block events"
    );
    assert_eq!(
        decode_block_updates(obs_frames),
        vec![(barrel_pos, open_north)],
        "0 -> 1 transition must broadcast 0x09 BlockUpdate with open=true"
    );
    let _ = frames_on(&mut r1);

    // 1 -> 2: remains open=true, no extra 0x09 or 0x08.
    let before = chest_opener_snapshot(&audience, &[barrel_pos]);
    open_chest_for_client(&v2, win, None, &HashMap::new(), "Barrel");
    sync_chest_openers(&audience, &before);
    assert_eq!(block_at(barrel_pos.0, barrel_pos.1, barrel_pos.2), open_north);
    assert!(frames_on(&mut r_obs).is_empty(), "1 -> 2 must emit neither 0x08 nor 0x09");
    let _ = frames_on(&mut r2);

    // 2 -> 1: remains open=true, no extra 0x09 or 0x08.
    let before = chest_opener_snapshot(&audience, &[barrel_pos]);
    *v2.open_chest.lock().unwrap() = None;
    *v2.chest_sync.lock().unwrap() = None;
    sync_chest_openers(&audience, &before);
    assert_eq!(block_at(barrel_pos.0, barrel_pos.1, barrel_pos.2), open_north);
    assert!(frames_on(&mut r_obs).is_empty(), "2 -> 1 must stay open without packets");

    // 1 -> 0: open returns to false preserving facing=north, 0x09 broadcast, NO 0x08.
    let before = chest_opener_snapshot(&audience, &[barrel_pos]);
    *v1.open_chest.lock().unwrap() = None;
    *v1.chest_sync.lock().unwrap() = None;
    sync_chest_openers(&audience, &before);
    assert_eq!(block_at(barrel_pos.0, barrel_pos.1, barrel_pos.2), closed_north);
    let obs_frames = frames_on(&mut r_obs);
    assert!(block_events(obs_frames.clone()).is_empty());
    assert_eq!(
        decode_block_updates(obs_frames),
        vec![(barrel_pos, closed_north)],
        "1 -> 0 transition must broadcast 0x09 BlockUpdate with open=false"
    );

    set_block(barrel_pos.0, barrel_pos.1, barrel_pos.2, 0);
}

#[test]
fn chest_contract_3d_waterlogged_double_chest_schedules_and_spreads_without_dry_partner_error() {
    crate::test_support::require_vanilla_data();
    let _world_guard = world_guard();
    let _snapshot = WorldSnapshot::take();
    WATERLOGGED_DEFER.with(|v| v.borrow_mut().clear());
    set_test_region(HashSet::from([(6, 3)]), HashSet::from([(6, 3)]));

    let reg = breg().expect("block registry");
    let stone = reg.state_of("minecraft:stone", None);
    let air = reg.state_of("minecraft:air", None);

    // Build an enclosed chamber inside chunk (6, 3) around (100, 170, 56).
    {
        let mut w = world().lock().unwrap();
        let chunk = w.entry((6, 3)).or_default();
        for y in 169..=171 {
            for z in 54..=59 {
                for x in 98..=104 {
                    chunk.insert(local_idx(x, y, z), stone);
                }
            }
        }
    }
    let left: BlockPos = (100, 170, 56);
    let right: BlockPos = (101, 170, 56);
    let open_west: BlockPos = (99, 170, 56);
    let dry_chest = reg.with_prop(
        reg.with_prop(
            reg.with_prop(reg.state_of("minecraft:chest", None), "facing", "north"),
            "type",
            "single",
        ),
        "waterlogged",
        "false",
    );
    let wet_left = reg.with_prop(
        reg.with_prop(
            reg.with_prop(reg.state_of("minecraft:chest", None), "facing", "north"),
            "type",
            "left",
        ),
        "waterlogged",
        "true",
    );
    let wet_right = reg.with_prop(
        reg.with_prop(
            reg.with_prop(reg.state_of("minecraft:chest", None), "facing", "north"),
            "type",
            "right",
        ),
        "waterlogged",
        "false",
    );
    set_block(open_west.0, open_west.1, open_west.2, air);
    set_block(left.0, left.1, left.2, wet_left);
    set_block(right.0, right.1, right.2, dry_chest);

    // FluidReader must report dry chests as Solid instead of failing with unsupported fluid shape.
    assert_eq!(
        flowing_fluid::Reader::cell(&mut FluidReader, right),
        Ok(flowing_fluid::Cell::Solid),
        "dry chest adjacent to waterlogged chest must be Solid for FlowingFluid"
    );

    // Re-writing the right half to pair `(right, left)` schedules the waterlogged left half (+5 GT).
    let base_tick = TICK_IDX.load(Ordering::Relaxed);
    set_block(right.0, right.1, right.2, wet_right);
    for step in 1..=4 {
        TICK_IDX.store(base_tick + step, Ordering::Relaxed);
        generated_fluid_tick();
        assert_eq!(
            block_at(open_west.0, open_west.1, open_west.2),
            air,
            "waterlogged chest must not flow before +5 GT"
        );
    }
    TICK_IDX.store(base_tick + 5, Ordering::Relaxed);
    generated_fluid_tick();
    assert_eq!(
        block_at(left.0, left.1, left.2),
        wet_left,
        "waterlogged chest half keeps its waterlogged block"
    );
    assert_eq!(
        block_at(right.0, right.1, right.2),
        wet_right,
        "dry chest partner must not be auto-waterlogged by flowing water"
    );
    assert_eq!(
        block_at(open_west.0, open_west.1, open_west.2),
        81,
        "waterlogged chest half must spread water at +5 GT even beside dry chest half"
    );
    WATERLOGGED_DEFER.with(|v| v.borrow_mut().clear());
}

#[test]
fn chest_contract_3d_ender_chest_emits_block_event_and_invalidates_on_break() {
    crate::test_support::require_vanilla_data();
    let _world_guard = world_guard();
    let _snapshot = WorldSnapshot::take();

    let reg = breg().expect("block registry");
    let ender_st = reg.state_of("minecraft:ender_chest", None);
    let ender_id = i64::from(reg.block_id_of(ender_st).expect("ender_chest block id"));
    let ender_a: BlockPos = (105, 170, 60);
    let ender_b: BlockPos = (108, 170, 60);
    set_block(ender_a.0, ender_a.1, ender_a.2, ender_st);
    set_block(ender_b.0, ender_b.1, ender_b.2, ender_st);

    let (v1, mut r1) = fast_chest_viewer(321, ender_a);
    let (v2, mut r2) = fast_chest_viewer(322, ender_a);
    let (spec, mut r_spec) = fast_chest_viewer(323, ender_a);
    let (other, mut r_other) = fast_chest_viewer(324, ender_b);
    let (obs, mut r_obs) = fast_chest_viewer(325, ender_a);
    *spec.gm.lock().unwrap() = 3;
    let audience = [v1.clone(), v2.clone(), spec.clone(), other.clone(), obs.clone()];

    // v1 & v2 open ender_a; other opens ender_b; spec stays out of opener count.
    let before = chest_opener_snapshot(&audience, &[ender_a, ender_b]);
    open_chest_for_client(
        &v1,
        (ContainerKey::Block(ender_key(&v1.uuid)), None),
        Some(ender_a),
        &HashMap::new(),
        "Ender Chest",
    );
    sync_chest_openers(&audience, &before);
    assert_eq!(
        block_events(frames_on(&mut r_obs)),
        vec![(ender_a, 1, 1, ender_id)],
        "first ender chest viewer must emit 0x08 action=1 param=1 on anchor"
    );

    let before = chest_opener_snapshot(&audience, &[ender_a, ender_b]);
    open_chest_for_client(
        &v2,
        (ContainerKey::Block(ender_key(&v2.uuid)), None),
        Some(ender_a),
        &HashMap::new(),
        "Ender Chest",
    );
    sync_chest_openers(&audience, &before);
    assert_eq!(
        block_events(frames_on(&mut r_obs)),
        vec![(ender_a, 1, 2, ender_id)],
        "second ender chest viewer on same anchor must emit count=2"
    );

    // Opening ender_b increments ender_b without moving ender_a.
    let before = chest_opener_snapshot(&audience, &[ender_a, ender_b]);
    open_chest_for_client(
        &other,
        (ContainerKey::Block(ender_key(&other.uuid)), None),
        Some(ender_b),
        &HashMap::new(),
        "Ender Chest",
    );
    sync_chest_openers(&audience, &before);
    assert_eq!(
        block_events(frames_on(&mut r_obs)),
        vec![(ender_b, 1, 1, ender_id)],
        "viewer on separate ender chest increments only its own anchor"
    );

    // Spectator opening ender_a must not increment ender_a's opener count.
    let before = chest_opener_snapshot(&audience, &[ender_a]);
    open_chest_for_client(
        &spec,
        (ContainerKey::Block(ender_key(&spec.uuid)), None),
        Some(ender_a),
        &HashMap::new(),
        "Ender Chest",
    );
    sync_chest_openers(&audience, &before);
    assert!(
        block_events(frames_on(&mut r_obs)).is_empty(),
        "spectator must not increment ender chest opener count"
    );

    // Personal ender chest contents survive breaking ender_a, while open windows on ender_a close.
    let diamond = name_to_item("diamond").unwrap();
    chests()
        .lock()
        .unwrap()
        .entry(ContainerKey::Block(ender_key(&v1.uuid)))
        .or_default()
        .insert(0, (diamond, 3, Vec::new()));
    let _ = frames_on(&mut r1);
    let _ = frames_on(&mut r2);
    let _ = frames_on(&mut r_other);
    let _ = frames_on(&mut r_spec);

    // Put audience in global clients() so set_block invalidates windows on ender_a.
    *clients().lock().unwrap() = audience.to_vec();
    set_block(ender_a.0, ender_a.1, ender_a.2, 0);
    assert!(v1.open_chest.lock().unwrap().is_none(), "breaking ender_a closes v1");
    assert!(v2.open_chest.lock().unwrap().is_none(), "breaking ender_a closes v2");
    assert!(
        other.open_chest.lock().unwrap().is_some(),
        "breaking ender_a must NOT close viewer anchored on ender_b"
    );
    assert!(
        frames_on(&mut r1).iter().any(|(pid, b)| *pid == 0x12 && *b == vec![1]),
        "v1 receives ClientboundContainerClosePacket"
    );
    assert_eq!(
        chests()
            .lock()
            .unwrap()
            .get(&ContainerKey::Block(ender_key(&v1.uuid)))
            .and_then(|m| m.get(&0))
            .cloned(),
        Some((diamond, 3, Vec::new())),
        "breaking an ender chest must preserve player's personal ender inventory"
    );

    chests().lock().unwrap().remove(&ContainerKey::Block(ender_key(&v1.uuid)));
    set_block(ender_b.0, ender_b.1, ender_b.2, 0);
}

