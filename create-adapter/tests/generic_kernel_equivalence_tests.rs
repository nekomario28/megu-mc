// SPDX-License-Identifier: LGPL-3.0-or-later

//! Differential equivalence and dormancy proofs for the generic Create
//! offload path. The bridge is exercised as a whole — factory regions,
//! contraptions, orbital 6-DOF ships, rail trains and companion-storage
//! digest caching — while independent worker counts and freshly invalidated
//! caches must produce bit-identical observable state.

use std::collections::{BTreeMap, BTreeSet};

use create_adapter::addon_profile::{create_registry_with_addons, createaeronautics_profile};
use create_adapter::aeronautics::RigidBodySubWorld;
use create_adapter::blockstates::{
    core_create_registry, CreateBlockRegistry, DEFAULT_BASE_STATE_ID,
};
use create_adapter::contraption_adapter::{ContraptionKind, LiveContraption};
use create_adapter::factorio_sim::{ActiveFactoryScheduler, CreateBeltLine, MachineKind};
use create_adapter::factory::{CreateFactory, DimBlockPos};
use create_adapter::offload_bridge::{
    AssemblyStress, CompanionRecord, OffloadBridge, OffloadBudget, OffloadTickReport,
    PlayerCompanionStorage, MAX_BLOCKS_PER_ASSEMBLY,
};
use create_adapter::orbital::OrbitalTransitionRouter;
use create_adapter::rail_network::{
    encode_snapshot as encode_rail_snapshot, tick_rail_network, ArticulatedTrain, CarriageSpec,
    ChunkKey, DimId, RailNetwork, TrackGauge, TrainId, TrainRunState, TrainSchedule,
};
use mod_offload::kinetic_graph::{NetworkStatus, Solution};
use mod_offload::region_scheduler::RegionKey;

const OWNER: [u8; 16] = [0x5a; 16];

fn budget(workers: usize) -> OffloadBudget {
    let mut budget = OffloadBudget::new(1_024).with_workers(workers);
    // Wall-clock deadlines are intentionally outside the semantic contract;
    // disabling them keeps independent sequential and parallel runs perfectly
    // comparable even on a heavily loaded CI host.
    budget.factory_region_micros = None;
    budget
}

fn linear(velocity: [f64; 3]) -> ContraptionKind {
    ContraptionKind::Linear {
        velocity_per_tick: velocity,
    }
}

fn bearing(rpm: f32) -> ContraptionKind {
    ContraptionKind::Bearing {
        angular_deg_per_tick: rpm,
    }
}

fn stress() -> AssemblyStress {
    AssemblyStress::new(128.0, 8.0).unwrap()
}

fn line_contraption(
    id: u32,
    dim: i32,
    anchor: [i32; 3],
    len: usize,
    kind: ContraptionKind,
) -> LiveContraption {
    let mut world = BTreeMap::new();
    for i in 0..len {
        world.insert([anchor[0] + i as i32, anchor[1], anchor[2]], 1u32);
    }
    let (contraption, _) = LiveContraption::assemble_from_world(
        id,
        dim,
        anchor,
        anchor,
        &world,
        kind,
        MAX_BLOCKS_PER_ASSEMBLY,
    )
    .expect("test assembly must build");
    contraption
}

fn straight(a: [f64; 3], b: [f64; 3]) -> [[f64; 3]; 4] {
    let lerp = |t: f64| {
        [
            a[0] + (b[0] - a[0]) * t,
            a[1] + (b[1] - a[1]) * t,
            a[2] + (b[2] - a[2]) * t,
        ]
    };
    [a, lerp(1.0 / 3.0), lerp(2.0 / 3.0), b]
}

fn free_train(id: u64) -> ArticulatedTrain {
    ArticulatedTrain::new(
        TrainId(id),
        vec![CarriageSpec {
            length: 8.0,
            leading_bogey_offset: 1.5,
            trailing_bogey_offset: 6.5,
            gauge: TrackGauge::Standard,
        }],
        6.0,
        2.0,
        2.0,
        TrainSchedule::default(),
    )
    .unwrap()
}

fn terminus_train(id: u64) -> ArticulatedTrain {
    let mut train = free_train(id);
    train.speed = 0.0;
    train.state = TrainRunState::Terminus;
    train
}

fn canonical(reg: &CreateBlockRegistry, id: &str) -> String {
    reg.encode_canonical(reg.default_state(id).unwrap())
        .unwrap()
        .to_string()
}

fn install_factory(factory: &mut CreateFactory) {
    let reg = factory.registry().clone();
    let wheel_z = reg
        .with_property(
            reg.default_state("create:water_wheel").unwrap(),
            "facing",
            "south",
        )
        .unwrap();
    let shaft_z = reg
        .with_property(reg.default_state("create:shaft").unwrap(), "axis", "z")
        .unwrap();

    factory.place_block(DimBlockPos::new(0, 0, 64, 0), wheel_z);
    factory.place_block(DimBlockPos::new(0, 0, 64, 1), shaft_z);
    factory
        .add_belt_segment(DimBlockPos::new(0, 1, 64, 2), 1, 4, 1)
        .unwrap();
    factory
        .add_belt_segment(DimBlockPos::new(0, 1, 64, 3), 2, 4, 1)
        .unwrap();
    factory
        .link_belt(DimBlockPos::new(0, 1, 64, 2), 1, Some(2))
        .unwrap();
    factory
        .insert_belt_item(DimBlockPos::new(0, 1, 64, 2), 1, 42)
        .unwrap();
}

fn companion_storage() -> PlayerCompanionStorage {
    let mut storage = PlayerCompanionStorage::with_box_count(1).unwrap();
    let mut record = CompanionRecord::new([0x33; 16], OWNER, "testcompanion:ember_cub");
    record.current_hp = 12;
    record.max_hp = 20;
    record.moves.push("testcompanion:tackle".to_string());
    storage.add_captured(record).unwrap();
    storage
}

/// One bridge containing every generic surface relevant to Create-class
/// worlds: a kinetic factory with belt transport, two contraption driver
/// families in separate regions, one moving orbital rigid body, one running
/// train, and one companion storage whose encoded bytes are cacheable.
fn build_complex_bridge() -> OffloadBridge {
    let core = core_create_registry(DEFAULT_BASE_STATE_ID);
    let mut bridge = OffloadBridge::with_factory(CreateFactory::new(core));
    install_factory(bridge.factory_mut().unwrap());

    bridge
        .register_contraption(
            line_contraption(1, 0, [4, 64, 4], 2, bearing(12.0)),
            stress(),
        )
        .unwrap();
    bridge
        .register_contraption(
            line_contraption(2, 0, [40, 64, 4], 3, linear([0.25, 0.0, -0.5])),
            stress(),
        )
        .unwrap();

    let addon = createaeronautics_profile();
    let aero_reg =
        create_registry_with_addons(DEFAULT_BASE_STATE_ID, std::slice::from_ref(&addon)).unwrap();
    let mut cells = BTreeMap::new();
    cells.insert([0, 0, 0], canonical(&aero_reg, "create:shaft"));
    let mut ship = RigidBodySubWorld::assemble(7, 0, cells, &aero_reg, Some(&addon), 100.0)
        .expect("single-block hull assembles");
    ship.pos = [10.0, 80.0, -20.0];
    ship.vel = [0.5, 1.25, -0.75];
    bridge.register_orbital_ship(ship).unwrap();

    let dim = DimId::new("minecraft:overworld");
    let a = bridge.rail_add_node(dim.clone(), [0.0, 0.0, 0.0]);
    let b = bridge.rail_add_node(dim, [100.0, 0.0, 0.0]);
    let edge = bridge
        .rail_add_edge(
            a,
            b,
            TrackGauge::Standard,
            straight([0.0, 0.0, 0.0], [100.0, 0.0, 0.0]),
        )
        .unwrap();
    bridge.rail_add_block(&[edge]).unwrap();
    bridge
        .rail_network_mut()
        .add_train(free_train(1), edge, 0.0)
        .unwrap();

    bridge
        .register_companion_storage(OWNER, companion_storage())
        .unwrap();
    bridge
}

fn machine_state(bridge: &OffloadBridge, id: u32) -> mod_offload::sleeping_transport::MachineState {
    bridge
        .factory()
        .unwrap()
        .scheduler()
        .region(RegionKey::for_block(0, 1, 2))
        .unwrap()
        .transport()
        .machine(id)
        .unwrap()
}

fn assert_f64_array_bits_eq<const N: usize>(a: [f64; N], b: [f64; N], context: &str) {
    for axis in 0..N {
        assert_eq!(
            a[axis].to_bits(),
            b[axis].to_bits(),
            "{context}: axis {axis} bits"
        );
    }
}

fn assert_solution_bits_eq(a: Solution, b: Solution, context: &str) {
    assert_eq!(a.component, b.component, "{context}: component");
    assert_eq!(a.status, b.status, "{context}: status");
    assert_eq!(
        a.capacity.to_bits(),
        b.capacity.to_bits(),
        "{context}: capacity"
    );
    assert_eq!(a.load.to_bits(), b.load.to_bits(), "{context}: load");
    assert_eq!(a.speed.to_bits(), b.speed.to_bits(), "{context}: speed");
}

fn assert_report_counters_eq(a: &OffloadTickReport, b: &OffloadTickReport) {
    assert_eq!(a.tick, b.tick);
    assert_eq!(a.work_units_budget, b.work_units_budget);
    assert_eq!(a.work_units_used, b.work_units_used);
    assert_eq!(a.stepped, b.stepped);
    assert_eq!(a.deferred, b.deferred);
    assert_eq!(a.overstressed, b.overstressed);
    assert_eq!(a.halted, b.halted);
    assert_eq!(a.over_budget, b.over_budget);
    assert_eq!(a.batches, b.batches);
    assert_eq!(a.factory_regions, b.factory_regions);
    assert_eq!(a.factory_records, b.factory_records);
    assert_eq!(a.orbital_transitions, b.orbital_transitions);
    assert_eq!(a.rail_events, b.rail_events);
    assert_eq!(a.combat_motions_active, b.combat_motions_active);
}

fn assert_complex_bridge_eq(a: &OffloadBridge, b: &OffloadBridge, tick: u64) {
    assert_eq!(a.state_digest(), b.state_digest(), "tick {tick}: digest");

    for id in [1u32, 2] {
        let ca = a.assembly(id).unwrap();
        let cb = b.assembly(id).unwrap();
        assert_eq!(
            ca.yaw_deg.to_bits(),
            cb.yaw_deg.to_bits(),
            "tick {tick} yaw"
        );
        assert_eq!(ca.pitch_deg.to_bits(), cb.pitch_deg.to_bits());
        for axis in 0..3 {
            assert_eq!(
                ca.translation[axis].to_bits(),
                cb.translation[axis].to_bits(),
                "tick {tick} assembly {id} translation axis {axis}"
            );
        }
    }

    let sa = a.orbital_ship(7).unwrap();
    let sb = b.orbital_ship(7).unwrap();
    assert_eq!(sa.parent_dim, sb.parent_dim);
    assert_f64_array_bits_eq(sa.pos, sb.pos, "orbital position");
    assert_f64_array_bits_eq(sa.vel, sb.vel, "orbital velocity");
    assert_f64_array_bits_eq(sa.quat, sb.quat, "orbital attitude");
    assert_f64_array_bits_eq(sa.omega, sb.omega, "orbital omega");

    assert_eq!(
        a.rail_save_bytes(),
        b.rail_save_bytes(),
        "tick {tick}: rail snapshot bytes"
    );
    assert_eq!(
        a.rail_replication_payloads(),
        b.rail_replication_payloads(),
        "tick {tick}: rail replication output"
    );
    assert_eq!(machine_state(a, 1), machine_state(b, 1));
    assert_eq!(machine_state(a, 2), machine_state(b, 2));
    assert_solution_bits_eq(
        a.factory()
            .unwrap()
            .kinetic_solution(DimBlockPos::new(0, 0, 64, 1))
            .unwrap(),
        b.factory()
            .unwrap()
            .kinetic_solution(DimBlockPos::new(0, 0, 64, 1))
            .unwrap(),
        "kinetic solution",
    );
    assert_eq!(
        a.encode_companion_storage(&OWNER),
        b.encode_companion_storage(&OWNER),
        "tick {tick}: companion storage bytes"
    );
}

#[test]
fn bridge_workers_companion_cache_and_empty_ticks_are_bit_identical() {
    let mut sequential = build_complex_bridge();
    let mut four_way = build_complex_bridge();
    let mut cache_invalidated = build_complex_bridge();

    // Mutable access invalidates the encoded companion cache even when the
    // caller ultimately changes nothing. The digest must be byte-identical
    // whether it folds the retained cache or re-encodes the same state.
    assert!(cache_invalidated.companion_storage_mut(&OWNER).is_some());
    assert_eq!(sequential.state_digest(), cache_invalidated.state_digest());

    let mut saw_factory_output = false;
    for tick in 1..=16u64 {
        let one = sequential.step_tick(tick, budget(1)).unwrap();
        let four = four_way.step_tick(tick, budget(4)).unwrap();
        let cached = cache_invalidated.step_tick(tick, budget(1)).unwrap();

        assert_report_counters_eq(&one, &four);
        assert_report_counters_eq(&one, &cached);
        assert_eq!(
            one.digest, four.digest,
            "tick {tick}: workers changed state"
        );
        assert_eq!(
            one.digest, cached.digest,
            "tick {tick}: cache path changed state"
        );
        saw_factory_output |= one.factory_records > 0;
        assert_complex_bridge_eq(&sequential, &four_way, tick);
        assert_complex_bridge_eq(&sequential, &cache_invalidated, tick);
    }

    // Prove every surface actually executed rather than comparing two frozen
    // zero states: belts emitted records, the bearing rotated, the ship and
    // train moved, and the kinetic network is running.
    assert!(
        saw_factory_output,
        "factory belt produced no output records"
    );
    assert!(sequential.assembly(1).unwrap().yaw_deg != 0.0);
    assert!(sequential.orbital_ship(7).unwrap().pos[0] > 10.0);
    let train = &sequential.rail_network().trains[&TrainId(1)];
    assert!(train.pos_s > 0.0 || train.pos_edge.0 != 0 || train.speed > 0.0);
    let solution = sequential
        .factory()
        .unwrap()
        .kinetic_solution(DimBlockPos::new(0, 0, 64, 1))
        .unwrap();
    assert_eq!(solution.status, NetworkStatus::Running);

    // A bridge with no registered work is dormant: both worker shapes agree
    // tick-for-tick and every surface reports exactly zero work.
    let mut empty_one = OffloadBridge::new();
    let mut empty_four = OffloadBridge::new();
    for tick in 1..=8u64 {
        let a = empty_one.step_tick(tick, budget(1)).unwrap();
        let b = empty_four.step_tick(tick, budget(4)).unwrap();
        assert_eq!(a.digest, b.digest);
        assert_eq!(a.work_units_used, 0);
        assert_eq!(a.stepped, 0);
        assert_eq!(a.factory_regions, 0);
        assert_eq!(a.factory_records, 0);
        assert_eq!(a.orbital_transitions, 0);
        assert_eq!(a.rail_events, 0);
        assert_eq!(a.combat_motions_active, 0);
        assert!(a.batches.is_empty());
    }
    assert_eq!(empty_one.state_digest(), empty_four.state_digest());
}

fn scheduler_world() -> ActiveFactoryScheduler {
    let mut scheduler = ActiveFactoryScheduler::new();
    assert_eq!(
        scheduler.add_machine(MachineKind::MechanicalPress, 4, 1, 3, true),
        0
    );
    for _ in 0..1_000 {
        scheduler.add_machine(MachineKind::MechanicalMixer, 4, 4, 5, true);
    }
    scheduler
}

#[test]
fn thousand_dormant_machines_and_belts_sleep_while_active_work_is_exact() {
    let mut jump = scheduler_world();
    let mut stepped = scheduler_world();
    assert_eq!(jump.push_input(0, 2), Ok(2));
    assert_eq!(stepped.push_input(0, 2), Ok(2));

    // Long catch-up is event-exact: processing deadlines, the output-full
    // stall, and the resulting loss of wake work match one-tick stepping.
    jump.run_to(200).unwrap();
    for tick in 1..=200u64 {
        stepped.run_to(tick).unwrap();
    }
    assert_eq!(jump.digest(), stepped.digest());
    assert_eq!(jump.total_visits(), stepped.total_visits());
    assert_eq!(
        jump.total_visits(),
        2,
        "start plus first completion; the full output blocks the next start"
    );
    assert_eq!(jump.sleeping_machines(), 1_001);
    assert_eq!(jump.awake_machines(), 0);
    assert_eq!(jump.snapshot(0).unwrap().output_items, 1);

    // Freeing the output slot wakes exactly the stalled machine; the other
    // 1,000 machines remain asleep through the long gap and the release.
    assert_eq!(jump.pop_output(0, 1), Ok(1));
    assert_eq!(stepped.pop_output(0, 1), Ok(1));
    jump.run_to(201).unwrap();
    stepped.run_to(201).unwrap();
    assert_eq!(jump.digest(), stepped.digest());
    assert_eq!(jump.total_visits(), 3);
    assert_eq!(jump.last_visits(), 1);
    assert_eq!(jump.awake_machines(), 1);
    assert_eq!(jump.sleeping_machines(), 1_000);
    assert_eq!(jump.snapshot(0).unwrap().output_items, 0);

    // The restarted craft reaches its exact deadline three ticks later;
    // catch-up and one-tick stepping remain bit-identical through completion.
    jump.run_to(203).unwrap();
    stepped.run_to(203).unwrap();
    assert_eq!(jump.digest(), stepped.digest());
    assert_eq!(jump.total_visits(), stepped.total_visits());
    assert_eq!(jump.total_visits(), 4);
    assert_eq!(jump.sleeping_machines(), 1_001);
    assert_eq!(jump.snapshot(0).unwrap().output_items, 1);

    // One thousand genuinely idle belt lines (moving RPM but no payload, or
    // stopped outright) pay zero stack visits. One active line beside them
    // still advances one stack per tick with conservation intact.
    let mut dormant_belts: Vec<CreateBeltLine> = (0..1_000)
        .map(|i| {
            let mut belt = CreateBeltLine::for_blocks(8);
            belt.set_rpm(if i % 2 == 0 { 64 } else { 0 });
            belt
        })
        .collect();
    let mut active = CreateBeltLine::for_blocks(64);
    active.set_rpm(64);
    active.try_insert_back(42, 1, 0).unwrap();

    for _ in 0..24 {
        for belt in &mut dormant_belts {
            assert_eq!(belt.advance(), 0, "dormant belt performed stack work");
            assert!(belt.is_asleep());
        }
        assert_eq!(
            active.advance(),
            1,
            "free-flowing active belt must stay O(1)"
        );
    }
    assert_eq!(
        dormant_belts
            .iter()
            .map(CreateBeltLine::total_visits)
            .sum::<u64>(),
        0
    );
    assert_eq!(active.total_visits(), 24);
    assert_eq!(active.extracted_items(), 0);
    assert_eq!(active.inserted_items(), active.on_belt_items());
}

fn aero_hull_ship(id: u64) -> RigidBodySubWorld {
    let addon = createaeronautics_profile();
    let reg =
        create_registry_with_addons(DEFAULT_BASE_STATE_ID, std::slice::from_ref(&addon)).unwrap();
    let mut cells = BTreeMap::new();
    cells.insert([0, 0, 0], canonical(&reg, "create:shaft"));
    RigidBodySubWorld::assemble(id, 0, cells, &reg, Some(&addon), 100.0)
        .expect("single-block hull assembles")
}

#[test]
fn sleeping_orbital_subworld_is_pose_frozen_while_active_ship_steps_with_parity() {
    let mut bridge = OffloadBridge::new();

    let mut asleep = aero_hull_ship(1);
    asleep.pos = [100.0, 64.0, 100.0];
    asleep.vel = [0.0; 3];
    asleep.omega = [0.0; 3];
    asleep.awake = false;
    let asleep_pos = asleep.pos;
    let asleep_vel = asleep.vel;
    let asleep_quat = asleep.quat;
    let asleep_omega = asleep.omega;
    bridge.register_orbital_ship(asleep).unwrap();

    let mut active = aero_hull_ship(2);
    active.pos = [0.0, 80.0, 0.0];
    active.vel = [0.5, 1.0, -0.25];
    bridge.register_orbital_ship(active).unwrap();

    // Independent per-tick reference for the awake ship. It uses the same
    // authoritative server ticks but no batching/bridge machinery at all.
    let mut reference_active = aero_hull_ship(2);
    reference_active.pos = [0.0, 80.0, 0.0];
    reference_active.vel = [0.5, 1.0, -0.25];
    let mut reference_router = OrbitalTransitionRouter::new();

    for tick in 1..=4u64 {
        bridge.step_tick(tick, budget(1)).unwrap();
        reference_router.step_with_dimension_transitions_at(&mut reference_active, 0.05, tick);

        let sleeping = bridge.orbital_ship(1).unwrap();
        assert_f64_array_bits_eq(sleeping.pos, asleep_pos, "sleeping ship position");
        assert_f64_array_bits_eq(sleeping.vel, asleep_vel, "sleeping ship velocity");
        assert_f64_array_bits_eq(sleeping.quat, asleep_quat, "sleeping ship attitude");
        assert_f64_array_bits_eq(sleeping.omega, asleep_omega, "sleeping ship spin");
        assert_eq!(
            sleeping.tick(),
            tick,
            "embedded factory tick must still advance"
        );

        let active = bridge.orbital_ship(2).unwrap();
        assert_f64_array_bits_eq(active.pos, reference_active.pos, "active ship position");
        assert_f64_array_bits_eq(active.vel, reference_active.vel, "active ship velocity");
        assert_f64_array_bits_eq(active.quat, reference_active.quat, "active ship attitude");
        assert_f64_array_bits_eq(active.omega, reference_active.omega, "active ship spin");
    }
    assert!(bridge.orbital_ship(2).unwrap().pos[0] > 0.0);

    // A wake edge rejoins the normal integrator immediately: gravity acts on
    // the next tick rather than the ship remaining frozen forever.
    bridge.orbital_ship_mut(1).unwrap().wake();
    bridge.step_tick(5, budget(1)).unwrap();
    let woken = bridge.orbital_ship(1).unwrap();
    assert!(woken.awake);
    assert!(
        woken.vel[1] < 0.0,
        "woken ship did not resume gravity integration"
    );
    assert!(woken.pos[1] < asleep_pos[1]);
}

fn rail_world(parked: usize) -> RailNetwork {
    let mut net = RailNetwork::new();
    let dim = DimId::new("minecraft:overworld");

    let a = net.add_node(dim.clone(), [0.0, 0.0, 0.0]);
    let b = net.add_node(dim.clone(), [100.0, 0.0, 0.0]);
    let active_edge = net
        .add_edge(
            a,
            b,
            TrackGauge::Standard,
            straight([0.0, 0.0, 0.0], [100.0, 0.0, 0.0]),
        )
        .unwrap();
    net.add_block(&[active_edge]).unwrap();
    net.add_train(free_train(1), active_edge, 0.0).unwrap();

    // The parked fleet occupies one long continuous signal block. That keeps
    // the full-refresh reference cheap enough to run in CI while preserving
    // the important shape: 1,000 distinct trains whose coverage never changes.
    let mut parked_edges = Vec::with_capacity(parked);
    let mut prev = net.add_node(dim.clone(), [200.0, 0.0, 0.0]);
    for i in 0..parked {
        let x0 = 200.0 + i as f64 * 100.0;
        let x1 = x0 + 100.0;
        let next = net.add_node(dim.clone(), [x1, 0.0, 0.0]);
        let edge = net
            .add_edge(
                prev,
                next,
                TrackGauge::Standard,
                straight([x0, 0.0, 0.0], [x1, 0.0, 0.0]),
            )
            .unwrap();
        parked_edges.push(edge);
        net.add_train(terminus_train(i as u64 + 2), edge, 10.0)
            .unwrap();
        prev = next;
    }
    if !parked_edges.is_empty() {
        net.add_block(&parked_edges).unwrap();
    }
    net
}

#[test]
fn thousand_terminus_trains_skip_work_and_gated_rail_matches_full_refresh() {
    let mut gated = rail_world(1_000);
    // The reference is the same semantic network, but its occupancy epoch is
    // invalidated before every tick, forcing the complete refresh path the
    // fingerprint gate is optimizing.
    let mut full_refresh = gated.clone();
    let loaded: BTreeSet<ChunkKey> = BTreeSet::new();

    // Warm the gate once so steady-state counters start from committed
    // fingerprints, then compare gated vs forced-full-refresh bit-for-bit.
    tick_rail_network(&mut gated, &loaded);
    full_refresh.invalidate_occupancy_cache();
    tick_rail_network(&mut full_refresh, &loaded);
    assert_eq!(gated, full_refresh);

    // One post-warmup tick is the differential proof: the optimized gate and
    // a forced full refresh produce the same semantic network while the active
    // train is still moving through the same world as the dormant fleet.
    tick_rail_network(&mut gated, &loaded);
    full_refresh.invalidate_occupancy_cache();
    tick_rail_network(&mut full_refresh, &loaded);
    assert_eq!(
        gated, full_refresh,
        "dirty-fingerprint occupancy gate diverged from full refresh"
    );
    assert_eq!(
        encode_rail_snapshot(&gated),
        encode_rail_snapshot(&full_refresh),
        "rail snapshot bytes diverged from full refresh"
    );
    assert!(
        gated.trains[&TrainId(1)].pos_s > 0.0,
        "active train never moved"
    );

    // Steady-state cost proof: all 1,000 terminus trains are motionless and
    // cover unchanged blocks. They skip the occupancy sweep every tick; the
    // one running train may skip too once it reaches terminus, never fewer.
    for _ in 0..5 {
        let before = gated.occupancy_gate_stats();
        tick_rail_network(&mut gated, &loaded);
        let after = gated.occupancy_gate_stats();
        assert!(
            after.skips - before.skips >= 1_000,
            "terminus trains failed to skip occupancy work"
        );
    }
    for id in 2..=1_001u64 {
        let parked = &gated.trains[&TrainId(id)];
        assert_eq!(parked.state, TrainRunState::Terminus);
        assert_eq!(parked.speed, 0.0);
        assert_eq!(parked.pos_s, 10.0);
    }
    assert!(gated.trains[&TrainId(1)].pos_s > 0.0);
}
