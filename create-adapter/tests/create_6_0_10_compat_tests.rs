// SPDX-License-Identifier: LGPL-3.0-or-later
//! Permanent Create 6.0.10 compatibility regression tests for kinetic
//! networks, belts/funnels, fluids, mechanical processors, arms, and glue.

use create_adapter::blockstates::{core_create_registry, DEFAULT_BASE_STATE_ID};
use create_adapter::contraption_adapter::{SuperGlueBox, SuperGlueVolume};
use create_adapter::create_compat::{
    create_gearbox_transfer_sign, create_mixer_processing_ticks, create_press_running_tick_speed,
    ArmDistributionMode, CreateFace, CreateFluidNetwork, CreateItemFilter, CreateMechanicalArm,
    CreateProcessingMachine, CreateProcessingRecipe, ProcessingMachineKind, ProcessorTickState,
};
use create_adapter::factory::{CreateFactory, DimBlockPos};
use create_adapter::factorio_sim::{
    clamp_create_rpm_fp, ratio_fp, rpm_fp, rpm_to_belt_step, ActiveFactoryScheduler,
    CreateBeltLine, KineticKind, KineticNetworkPool, KineticStatus, MachineKind,
    CREATE_MAX_RPM_FP, RPM_FP_ONE, SUB_UNITS_PER_BLOCK,
};
use mod_offload::kinetic_graph::NetworkStatus;

fn drain_belt(belt: &mut CreateBeltLine) -> Vec<u32> {
    let mut drained = Vec::new();
    let mut guard = 0u32;
    while belt.item_slots() != 0 {
        while let Some(item) = belt.try_extract_front() {
            drained.push(item.item_id);
        }
        belt.advance();
        guard += 1;
        assert!(guard < 20_000, "belt drain made no progress");
    }
    drained
}

fn insert_stack_eventually(belt: &mut CreateBeltLine, item: u32, count: u16) {
    let mut guard = 0u32;
    while belt.try_insert_back(item, count, 0).is_err() {
        belt.advance();
        guard += 1;
        assert!(guard < 20_000, "belt never accepted funnel insert");
    }
}

fn complete_after(machine: &mut CreateProcessingMachine, max_ticks: u64) -> u64 {
    for tick in 1..=max_ticks {
        if machine.tick() == ProcessorTickState::Completed {
            return tick;
        }
    }
    panic!("machine did not complete within {max_ticks} ticks");
}

// ---------------------------------------------------------------------------
// Kinetic network and stress
// ---------------------------------------------------------------------------

#[test]
fn create_6_0_10_meshing_cogs_invert_shafts_propagate_and_gear_ratio_is_exact() {
    let mut pool = KineticNetworkPool::new();
    let wheel = pool.add_node(KineticKind::Waterwheel); // 8 RPM
    let large = pool.add_node(KineticKind::Cogwheel);
    let small = pool.add_node(KineticKind::Cogwheel);
    let shaft = pool.add_node(KineticKind::Shaft);
    let output_large = pool.add_node(KineticKind::Cogwheel);

    // Large driving small doubles speed and reverses direction.
    pool.connect(wheel, large, RPM_FP_ONE).unwrap();
    pool.connect(large, small, -ratio_fp(2, 1)).unwrap();
    assert_eq!(pool.rpm(large), rpm_fp(8));
    assert_eq!(pool.rpm(small), rpm_fp(-16));

    // Co-axial shafts preserve direction; small driving large halves speed
    // and reverses again.
    pool.connect(small, shaft, RPM_FP_ONE).unwrap();
    pool.connect(shaft, output_large, -ratio_fp(1, 2)).unwrap();
    assert_eq!(pool.rpm(shaft), rpm_fp(-16));
    assert_eq!(pool.rpm(output_large), rpm_fp(8));
    for id in [wheel, large, small, shaft, output_large] {
        assert_eq!(pool.status(id), Some(KineticStatus::Running));
    }
}

#[test]
fn create_6_0_10_gearbox_reversal_and_90_degree_signs_are_direction_sensitive() {
    // Faces are outward from the gearbox. Opposite ends of one shaft axis
    // reverse, matching Create's straight-through gearbox behaviour.
    assert_eq!(create_gearbox_transfer_sign(CreateFace::East, CreateFace::West), -1);
    assert_eq!(create_gearbox_transfer_sign(CreateFace::North, CreateFace::South), -1);

    // Perpendicular transfer depends on the signs of both axis directions:
    // same axis-direction polarity reverses, opposite polarity preserves.
    assert_eq!(create_gearbox_transfer_sign(CreateFace::East, CreateFace::South), -1);
    assert_eq!(create_gearbox_transfer_sign(CreateFace::East, CreateFace::North), 1);
    assert_eq!(create_gearbox_transfer_sign(CreateFace::Up, CreateFace::East), -1);
    assert_eq!(create_gearbox_transfer_sign(CreateFace::Up, CreateFace::West), 1);
}

#[test]
fn create_6_0_10_rpm_is_clamped_symmetrically_at_256_after_gearing_up() {
    assert_eq!(clamp_create_rpm_fp(rpm_fp(300)), CREATE_MAX_RPM_FP);
    assert_eq!(clamp_create_rpm_fp(rpm_fp(-300)), -CREATE_MAX_RPM_FP);

    let mut pool = KineticNetworkPool::new();
    let engine = pool.add_node(KineticKind::SteamEngine); // 16 RPM
    let first = pool.add_node(KineticKind::Shaft);
    let second = pool.add_node(KineticKind::Shaft);
    let third = pool.add_node(KineticKind::Shaft);
    pool.connect(engine, first, ratio_fp(4, 1)).unwrap(); // 64
    pool.connect(first, second, ratio_fp(4, 1)).unwrap(); // 256
    pool.connect(second, third, ratio_fp(2, 1)).unwrap(); // would be 512 upstream
    assert_eq!(pool.rpm(first), rpm_fp(64));
    assert_eq!(pool.rpm(second), rpm_fp(256));
    assert_eq!(pool.rpm(third), rpm_fp(256));
    assert_eq!(pool.status(third), Some(KineticStatus::Running));
}

#[test]
fn create_6_0_10_competing_rpm_sources_and_odd_cogwheel_loops_jam_as_conflict() {
    let mut pool = KineticNetworkPool::new();
    let left = pool.add_node(KineticKind::Waterwheel);
    let right = pool.add_node(KineticKind::Waterwheel);
    let shaft = pool.add_node(KineticKind::Shaft);
    pool.connect(left, shaft, RPM_FP_ONE).unwrap();
    pool.connect(right, shaft, RPM_FP_ONE).unwrap();
    assert_eq!(pool.status(shaft), Some(KineticStatus::Running));

    pool.set_generator_rpm(right, rpm_fp(12));
    assert_eq!(pool.status(shaft), Some(KineticStatus::Conflict));
    assert_eq!(pool.rpm(shaft), 0);

    let mut ring = KineticNetworkPool::new();
    let cogs: Vec<_> = (0..3).map(|_| ring.add_node(KineticKind::Cogwheel)).collect();
    ring.set_generator_rpm(cogs[0], rpm_fp(8));
    ring.connect(cogs[0], cogs[1], -RPM_FP_ONE).unwrap();
    ring.connect(cogs[1], cogs[2], -RPM_FP_ONE).unwrap();
    ring.connect(cogs[2], cogs[0], -RPM_FP_ONE).unwrap();
    assert_eq!(ring.status(cogs[0]), Some(KineticStatus::Conflict));
    assert!(cogs.iter().all(|&id| ring.rpm(id) == 0));
}

#[test]
fn create_6_0_10_zero_rpm_source_is_unpowered_without_divide_by_zero() {
    let mut pool = KineticNetworkPool::new();
    let source = pool.add_node(KineticKind::Waterwheel);
    let shaft = pool.add_node(KineticKind::Shaft);
    pool.connect(source, shaft, ratio_fp(1, 2)).unwrap();
    pool.set_generator_rpm(source, 0);
    assert_eq!(pool.status(source), Some(KineticStatus::Unpowered));
    assert_eq!(pool.status(shaft), Some(KineticStatus::Unpowered));
    assert_eq!(pool.rpm(source), 0);
    assert_eq!(pool.rpm(shaft), 0);
}


#[test]
fn create_6_0_10_deep_fixed_point_gear_underflow_jams_instead_of_dividing_by_zero() {
    let mut pool = KineticNetworkPool::new();
    let mut previous = pool.add_node(KineticKind::Shaft);
    let mut nodes = vec![previous];
    for _ in 0..20 {
        let next = pool.add_node(KineticKind::Shaft);
        pool.connect(previous, next, ratio_fp(1, 2)).unwrap();
        nodes.push(next);
        previous = next;
    }
    // A nonzero source at the deep end is not representable through the
    // underflowed chain. Create-equivalent fail-closed behaviour is a jammed
    // conflict with every published speed at zero — never a divide-by-zero.
    pool.set_generator_rpm(previous, rpm_fp(16));
    // A later edit seeded at the shallow end re-solves the same component
    // from the underflowed representation. That must fail closed as a jam.
    pool.set_impact(nodes[0], 1);
    assert_eq!(pool.status(previous), Some(KineticStatus::Conflict));
    assert!(nodes.iter().all(|&id| pool.rpm(id) == 0));

    // The same failure is closed when the source is at the shallow end and
    // only passive downstream nodes underflow: they may not publish Running
    // at a silent 0 RPM.
    let mut driven = KineticNetworkPool::new();
    let shallow = driven.add_node(KineticKind::SteamEngine);
    let mut previous = shallow;
    let mut nodes = vec![previous];
    for _ in 0..20 {
        let next = driven.add_node(KineticKind::Shaft);
        driven.connect(previous, next, ratio_fp(1, 2)).unwrap();
        nodes.push(next);
        previous = next;
    }
    // Force the authoritative re-solve to seed at the shallow powered end.
    driven.set_impact(shallow, 1);
    assert_eq!(driven.status(shallow), Some(KineticStatus::Conflict));
    assert!(nodes.iter().all(|&id| driven.rpm(id) == 0));
}

#[test]
fn create_6_0_10_overstress_halts_and_recovers_without_oscillation() {
    let mut pool = KineticNetworkPool::new();
    let engine = pool.add_node(KineticKind::SteamEngine);
    let press = pool.add_node(KineticKind::MechanicalPress);
    pool.connect(engine, press, RPM_FP_ONE).unwrap();
    assert_eq!(pool.status(press), Some(KineticStatus::Running));

    pool.set_impact(press, 4096);
    assert_eq!(pool.status(press), Some(KineticStatus::Overstressed));
    assert_eq!(pool.rpm(engine), 0);
    assert_eq!(pool.rpm(press), 0);
    for _ in 0..8 {
        assert_eq!(pool.tick(), 0, "overstressed network must remain settled");
        assert_eq!(pool.status(press), Some(KineticStatus::Overstressed));
    }

    pool.set_impact(press, 64);
    assert_eq!(pool.status(press), Some(KineticStatus::Running));
    assert_eq!(pool.rpm(press), rpm_fp(16));
    for _ in 0..8 {
        assert_eq!(pool.tick(), 0, "recovered network must not oscillate");
        assert_eq!(pool.status(press), Some(KineticStatus::Running));
    }
}

#[test]
fn create_6_0_10_disconnected_subnetwork_splits_cleanly() {
    let mut pool = KineticNetworkPool::new();
    let engine = pool.add_node(KineticKind::SteamEngine);
    let shaft = pool.add_node(KineticKind::Shaft);
    let press = pool.add_node(KineticKind::MechanicalPress);
    pool.connect(engine, shaft, RPM_FP_ONE).unwrap();
    pool.connect(shaft, press, RPM_FP_ONE).unwrap();
    assert_eq!(pool.component_id(engine), pool.component_id(press));

    pool.disconnect(shaft, press).unwrap();
    assert_ne!(pool.component_id(shaft), pool.component_id(press));
    assert_eq!(pool.status(shaft), Some(KineticStatus::Running));
    assert_eq!(pool.status(press), Some(KineticStatus::Unpowered));
    assert_eq!(pool.rpm(press), 0);

    pool.connect(shaft, press, RPM_FP_ONE).unwrap();
    assert_eq!(pool.component_id(engine), pool.component_id(press));
    assert_eq!(pool.rpm(press), rpm_fp(16));
}


#[test]
fn create_6_0_10_factory_clamps_geared_creative_motor_at_256_rpm() {
    let registry = core_create_registry(DEFAULT_BASE_STATE_ID);
    let motor = registry
        .with_property(
            registry.default_state("create:creative_motor").unwrap(),
            "facing",
            "north",
        )
        .unwrap();
    let large = registry
        .with_property(
            registry.default_state("create:large_cogwheel").unwrap(),
            "axis",
            "z",
        )
        .unwrap();
    let small = registry
        .with_property(registry.default_state("create:cogwheel").unwrap(), "axis", "z")
        .unwrap();

    let mut factory = CreateFactory::new(registry);
    let p_motor = DimBlockPos::new(0, 0, 64, 0);
    let p_large = DimBlockPos::new(0, 0, 64, 1);
    let p_small = DimBlockPos::new(0, 1, 65, 1);
    factory.place_block(p_motor, motor);
    factory.place_block(p_large, large);
    factory.place_block(p_small, small);

    let motor_solution = factory.kinetic_solution(p_motor).unwrap();
    let small_solution = factory.kinetic_solution(p_small).unwrap();
    assert_eq!(motor_solution.status, NetworkStatus::Running);
    assert_eq!(motor_solution.speed, 256.0);
    // The raw 1:2 gear-up would be 512 RPM; Create's hard ceiling publishes
    // exactly -256 RPM (the meshing inversion is preserved).
    assert_eq!(small_solution.status, NetworkStatus::Running);
    assert_eq!(small_solution.speed, -256.0);
}

#[test]
fn create_6_0_10_factory_gearbox_straight_reversal_and_90_degree_polarity() {
    let registry = core_create_registry(DEFAULT_BASE_STATE_ID);
    let motor = registry
        .with_property(
            registry.default_state("create:creative_motor").unwrap(),
            "facing",
            "north",
        )
        .unwrap();
    let gearbox_z = registry
        .with_property(registry.default_state("create:gearbox").unwrap(), "axis", "z")
        .unwrap();
    let shaft_z = registry
        .with_property(registry.default_state("create:shaft").unwrap(), "axis", "z")
        .unwrap();
    let shaft_x = registry
        .with_property(registry.default_state("create:shaft").unwrap(), "axis", "x")
        .unwrap();

    let mut straight = CreateFactory::new(core_create_registry(DEFAULT_BASE_STATE_ID));
    let p_motor = DimBlockPos::new(0, 0, 64, 0);
    let p_gearbox = DimBlockPos::new(0, 0, 64, 1);
    let p_out = DimBlockPos::new(0, 0, 64, 2);
    straight.place_block(p_motor, motor);
    straight.place_block(p_gearbox, gearbox_z);
    straight.place_block(p_out, shaft_z);
    assert_eq!(straight.kinetic_solution(p_motor).unwrap().speed, 256.0);
    assert_eq!(straight.kinetic_solution(p_out).unwrap().speed, -256.0);

    // North (negative axis polarity) to east (positive perpendicular polarity)
    // preserves sign in Create's gearbox rule, even though it is not a
    // universal meshing inversion.
    let mut angled = CreateFactory::new(registry);
    let p_side = DimBlockPos::new(0, 1, 64, 1);
    angled.place_block(p_motor, motor);
    angled.place_block(p_gearbox, gearbox_z);
    angled.place_block(p_side, shaft_x);
    assert_eq!(angled.kinetic_solution(p_side).unwrap().speed, 256.0);
}

#[test]
fn create_6_0_10_factory_large_cogwheels_mesh_only_on_cross_axis_diagonals() {
    let registry = core_create_registry(DEFAULT_BASE_STATE_ID);
    let motor = registry
        .with_property(
            registry.default_state("create:creative_motor").unwrap(),
            "facing",
            "north",
        )
        .unwrap();
    let large_z = registry
        .with_property(
            registry.default_state("create:large_cogwheel").unwrap(),
            "axis",
            "z",
        )
        .unwrap();
    let large_x = registry
        .with_property(
            registry.default_state("create:large_cogwheel").unwrap(),
            "axis",
            "x",
        )
        .unwrap();

    let mut factory = CreateFactory::new(registry);
    let p_motor = DimBlockPos::new(0, 0, 64, 0);
    let p_z = DimBlockPos::new(0, 0, 64, 1);
    let p_x = DimBlockPos::new(0, 1, 64, 0);
    factory.place_block(p_motor, motor);
    factory.place_block(p_z, large_z);
    factory.place_block(p_x, large_x);

    // diff(z-axis cog -> x-axis cog) = (+1,0,-1). The xor of the source-axis
    // sign (-z) and target-axis sign (+x) inverts rotation at 1:1.
    let first = factory.kinetic_solution(p_z).unwrap();
    let second = factory.kinetic_solution(p_x).unwrap();
    assert_eq!(first.status, NetworkStatus::Running);
    assert_eq!(second.status, NetworkStatus::Running);
    assert_eq!(first.speed, 256.0);
    assert_eq!(second.speed, -256.0);

    // Coplanar-adjacent large cogs overlap in real Create geometry and are
    // not a meshing connection. In particular they must not be driven.
    let p_invalid = DimBlockPos::new(0, 0, 65, 1);
    factory.place_block(p_invalid, large_z);
    let invalid = factory.kinetic_solution(p_invalid).unwrap();
    assert_ne!(invalid.component, first.component);
    assert_eq!(invalid.status, NetworkStatus::Unpowered);
    assert_eq!(invalid.speed, 0.0);
}

#[test]
fn create_6_0_10_factory_competing_generator_speeds_conflict_and_split_recovers() {
    let registry = core_create_registry(DEFAULT_BASE_STATE_ID);
    let waterwheel = registry
        .with_property(
            registry.default_state("create:water_wheel").unwrap(),
            "axis",
            "z",
        )
        .unwrap();
    let shaft = registry
        .with_property(registry.default_state("create:shaft").unwrap(), "axis", "z")
        .unwrap();
    let windmill = registry
        .with_property(
            registry.default_state("create:windmill_bearing").unwrap(),
            "facing",
            "north",
        )
        .unwrap();

    let mut factory = CreateFactory::new(registry);
    let p_water = DimBlockPos::new(0, 0, 64, 0);
    let p_shaft = DimBlockPos::new(0, 0, 64, 1);
    let p_wind = DimBlockPos::new(0, 0, 64, 2);
    factory.place_block(p_water, waterwheel);
    factory.place_block(p_shaft, shaft);
    factory.place_block(p_wind, windmill);

    let conflicted = factory.kinetic_solution(p_shaft).unwrap();
    assert_eq!(conflicted.status, NetworkStatus::Conflict);
    assert_eq!(conflicted.speed, 0.0);

    factory.remove_block(p_wind);
    let recovered = factory.kinetic_solution(p_shaft).unwrap();
    assert_eq!(recovered.status, NetworkStatus::Running);
    assert_eq!(recovered.speed, 8.0);
}

// ---------------------------------------------------------------------------
// Mechanical belts, funnels, and chutes
// ---------------------------------------------------------------------------

#[test]
fn create_6_0_10_belt_speed_is_proportional_and_signed_to_rpm() {
    assert_eq!(rpm_to_belt_step(32), SUB_UNITS_PER_BLOCK as i32 / 2);
    assert_eq!(rpm_to_belt_step(64), SUB_UNITS_PER_BLOCK as i32);
    assert_eq!(rpm_to_belt_step(128), SUB_UNITS_PER_BLOCK as i32 * 2);
    assert_eq!(rpm_to_belt_step(-64), -(SUB_UNITS_PER_BLOCK as i32));
    assert_eq!(rpm_to_belt_step(0), 0);
}

#[test]
fn create_6_0_10_belt_stop_preserves_reversed_orientation_regression() {
    let mut belt = CreateBeltLine::for_blocks(8);
    belt.set_rpm(64);
    for item in 1..=4u32 {
        insert_stack_eventually(&mut belt, item, 1);
    }
    for _ in 0..3 {
        belt.advance();
    }
    assert_eq!(
        belt.items().iter().map(|item| item.item_id).collect::<Vec<_>>(),
        vec![1, 2, 3, 4]
    );

    belt.set_rpm(-64);
    let reversed: Vec<_> = belt.items().iter().map(|item| item.item_id).collect();
    assert_eq!(reversed, vec![4, 3, 2, 1]);
    belt.set_rpm(0);
    assert_eq!(
        belt.items().iter().map(|item| item.item_id).collect::<Vec<_>>(),
        reversed,
        "zero RPM must stop the belt, not mirror it a second time"
    );
    assert_eq!(
        belt.inserted_items(),
        belt.extracted_items() + belt.on_belt_items()
    );

    belt.set_rpm(-64);
    assert_eq!(drain_belt(&mut belt), vec![4, 3, 2, 1]);
    assert_eq!(belt.inserted_items(), belt.extracted_items());
}

#[test]
fn create_6_0_10_belt_jam_against_full_output_conserves_and_unjams_fifo() {
    let mut belt = CreateBeltLine::for_blocks(6);
    belt.set_rpm(64);
    for item in 1..=6u32 {
        insert_stack_eventually(&mut belt, item, 7);
    }

    let mut guard = 0u32;
    while belt.advance() != 0 {
        guard += 1;
        assert!(guard < 20_000);
        assert_eq!(
            belt.inserted_items(),
            belt.extracted_items() + belt.on_belt_items()
        );
    }
    assert!(belt.is_asleep(), "jammed packed belt must sleep");
    for _ in 0..8 {
        assert_eq!(belt.advance(), 0);
    }
    assert_eq!(belt.on_belt_items(), 42);

    let mut order = Vec::new();
    let mut units = 0u64;
    while belt.item_slots() != 0 {
        while let Some(stack) = belt.try_extract_front() {
            order.push(stack.item_id);
            units += u64::from(stack.count);
        }
        belt.advance();
    }
    assert_eq!(order, vec![1, 2, 3, 4, 5, 6]);
    assert_eq!(units, 42);
    assert_eq!(belt.inserted_items(), belt.extracted_items());
}

#[test]
fn create_6_0_10_two_funnels_merge_deterministically_without_duplication() {
    let mut belt = CreateBeltLine::for_blocks(10);
    belt.set_rpm(64);
    // Andesite funnel stream (odd IDs) and brass funnel stream (even IDs)
    // alternate insertion only when the receiving belt edge has room.
    for round in 0..5u32 {
        insert_stack_eventually(&mut belt, round * 2 + 1, 1);
        insert_stack_eventually(&mut belt, round * 2 + 2, 1);
        belt.advance();
    }
    assert_eq!(belt.inserted_items(), 10);
    assert_eq!(belt.on_belt_items() + belt.extracted_items(), 10);
    assert_eq!(
        belt.items().iter().map(|item| item.item_id).collect::<Vec<_>>(),
        (1..=10).collect::<Vec<_>>()
    );
}

#[test]
fn create_6_0_10_funnel_push_and_chute_pull_conserve_items_through_backpressure() {
    let mut scheduler = ActiveFactoryScheduler::new();
    let press = scheduler.add_machine(MachineKind::MechanicalPress, 1, 1, 2, true);

    // Funnel insertion is partial and exact: a one-slot input accepts one of
    // two offered items and never duplicates the refused item.
    assert_eq!(scheduler.push_input(press, 2), Ok(1));
    scheduler.run_to(0).unwrap();
    assert_eq!(scheduler.snapshot(press).unwrap().input_items, 0);
    assert!(scheduler.snapshot(press).unwrap().busy);

    scheduler.run_to(2).unwrap();
    assert_eq!(scheduler.snapshot(press).unwrap().output_items, 1);

    // A second funnel push can refill input, but output backpressure holds
    // the craft instead of overwriting the occupied output slot.
    assert_eq!(scheduler.push_input(press, 1), Ok(1));
    scheduler.run_to(2).unwrap();
    assert_eq!(scheduler.snapshot(press).unwrap().input_items, 1);
    assert_eq!(scheduler.snapshot(press).unwrap().output_items, 1);
    assert!(!scheduler.snapshot(press).unwrap().busy);

    // Chute extraction removes exactly what is present and wakes the held
    // machine; the input item then appears exactly once downstream.
    assert_eq!(scheduler.pop_output(press, 4), Ok(1));
    scheduler.run_to(2).unwrap();
    assert_eq!(scheduler.snapshot(press).unwrap().input_items, 0);
    assert!(scheduler.snapshot(press).unwrap().busy);
    scheduler.run_to(4).unwrap();
    assert_eq!(scheduler.snapshot(press).unwrap().output_items, 1);
    assert_eq!(scheduler.pop_output(press, 4), Ok(1));
    assert_eq!(scheduler.snapshot(press).unwrap().output_items, 0);
}

#[test]
fn create_6_0_10_funnel_item_and_attribute_filters_are_exact() {
    let allow = CreateItemFilter::allow_items([7]);
    assert!(allow.matches(7, &[]));
    assert!(!allow.matches(8, &[]));

    let deny = CreateItemFilter::deny_items([7]);
    assert!(!deny.matches(7, &[]));
    assert!(deny.matches(8, &[]));

    const ENCHANTED: u32 = 9001;
    let enchanted_only = CreateItemFilter::allow_items([]).with_attributes([ENCHANTED]);
    assert!(enchanted_only.matches(8, &[ENCHANTED]));
    assert!(!enchanted_only.matches(8, &[1, 2]));
    assert!(CreateItemFilter::unfiltered().matches(8, &[]));
}

// ---------------------------------------------------------------------------
// Fluid pipes, pumps, and tanks
// ---------------------------------------------------------------------------

#[test]
fn create_6_0_10_pump_flow_rate_scales_linearly_with_rpm() {
    let mut slow = CreateFluidNetwork::new();
    let mut fast = CreateFluidNetwork::new();
    for net in [&mut slow, &mut fast] {
        net.add_tank(1, 1000).unwrap();
        net.add_tank(2, 1000).unwrap();
        net.add_pump(1, 1, 2).unwrap();
        assert_eq!(net.insert_fluid(1, 50, 1000), Ok(1000));
    }
    slow.set_pump_rpm(1, 8).unwrap();
    fast.set_pump_rpm(1, 16).unwrap();
    for _ in 0..8 {
        slow.tick();
        fast.tick();
    }
    assert_eq!(slow.tank(2).unwrap().amount_mb, 32);
    assert_eq!(fast.tank(2).unwrap().amount_mb, 64);
    assert_eq!(slow.tank(1).unwrap().amount_mb, 968);
    assert_eq!(fast.tank(1).unwrap().amount_mb, 936);

    // Rotation sign changes magnitude only. The pump arrow — not negative
    // RPM — chooses flow direction, so another tick still moves left-to-right.
    fast.set_pump_rpm(1, -16).unwrap();
    fast.tick();
    assert_eq!(fast.tank(1).unwrap().amount_mb, 928);
    assert_eq!(fast.tank(2).unwrap().amount_mb, 72);

    // Wrench reversal is the directional control.
    fast.set_pump_reversed(1, true).unwrap();
    fast.tick();
    assert_eq!(fast.tank(1).unwrap().amount_mb, 936);
    assert_eq!(fast.tank(2).unwrap().amount_mb, 64);
}

#[test]
fn create_6_0_10_fluid_type_and_volume_are_conserved_with_tank_backpressure() {
    let mut net = CreateFluidNetwork::new();
    net.add_tank(1, 100).unwrap();
    net.add_tank(2, 10).unwrap();
    net.add_pump(1, 1, 2).unwrap();
    net.set_pump_rpm(1, 64).unwrap();
    assert_eq!(net.insert_fluid(1, 50, 50), Ok(50));

    assert_eq!(net.tick(), 10);
    assert_eq!(net.tick(), 0, "full destination must apply backpressure");
    assert_eq!(net.tank(1).unwrap().amount_mb, 40);
    assert_eq!(net.tank(2).unwrap().amount_mb, 10);
    assert_eq!(
        net.tank(1).unwrap().amount_mb + net.tank(2).unwrap().amount_mb,
        50
    );

    // A tank containing fluid 51 rejects fluid 50 outright: no mixing and no
    // silently voided insertion.
    let mut incompatible = CreateFluidNetwork::new();
    incompatible.add_tank(1, 100).unwrap();
    incompatible.add_tank(2, 100).unwrap();
    incompatible.add_pump(1, 1, 2).unwrap();
    incompatible.set_pump_rpm(1, 64).unwrap();
    incompatible.insert_fluid(1, 50, 20).unwrap();
    incompatible.insert_fluid(2, 51, 3).unwrap();
    assert_eq!(incompatible.tick(), 0);
    assert_eq!(incompatible.tank(1).unwrap().amount_mb, 20);
    assert_eq!(incompatible.tank(2).unwrap().fluid, Some(51));
    assert_eq!(incompatible.tank(2).unwrap().amount_mb, 3);
}

#[test]
fn create_6_0_10_competing_pumps_are_deterministic_and_conserving() {
    let mut net = CreateFluidNetwork::new();
    net.add_tank(1, 1000).unwrap();
    net.add_tank(2, 1000).unwrap();
    net.add_pump(1, 1, 2).unwrap();
    net.add_pump(2, 1, 2).unwrap();
    net.insert_fluid(1, 50, 100).unwrap();
    net.set_pump_rpm(1, 64).unwrap(); // 32 mB/tick left-to-right
    net.set_pump_rpm(2, 32).unwrap(); // 16 mB/tick after wrench reversal
    net.set_pump_reversed(2, true).unwrap();

    for _ in 0..4 {
        net.tick();
        let a = net.tank(1).unwrap().amount_mb;
        let b = net.tank(2).unwrap().amount_mb;
        assert_eq!(a + b, 100);
        assert_eq!(net.tank(1).unwrap().fluid, Some(50));
        assert_eq!(net.tank(2).unwrap().fluid, Some(50));
    }
    // Fixed pump-ID order makes every tick move 32 forward then 16 back.
    assert_eq!(net.tank(1).unwrap().amount_mb, 36);
    assert_eq!(net.tank(2).unwrap().amount_mb, 64);
}

// ---------------------------------------------------------------------------
// Mixer / basin / press processing
// ---------------------------------------------------------------------------

fn processing_recipe(processing_time: u32) -> CreateProcessingRecipe {
    CreateProcessingRecipe {
        input_item: 10,
        input_count: 1,
        outputs: vec![(20, 1)],
        processing_time,
    }
}

fn byproduct_recipe() -> CreateProcessingRecipe {
    CreateProcessingRecipe {
        input_item: 10,
        input_count: 1,
        outputs: vec![(20, 3), (21, 1)],
        processing_time: 4,
    }
}

#[test]
fn create_6_0_10_mixer_refuses_to_process_below_30_rpm() {
    let recipe = processing_recipe(0);
    let mut mixer = CreateProcessingMachine::new(
        ProcessingMachineKind::MechanicalMixer,
        recipe,
        4,
        4,
    )
    .unwrap();
    mixer.insert_input(10, 1).unwrap();
    mixer.set_rpm(29);
    for _ in 0..16 {
        assert_eq!(mixer.tick(), ProcessorTickState::Idle);
    }
    assert_eq!(mixer.snapshot().input_items, 1);
    assert!(!mixer.snapshot().active);

    mixer.set_rpm(30);
    assert_eq!(complete_after(&mut mixer, 150), 101);
    assert_eq!(mixer.extract_output(20, 1), 1);
}

#[test]
fn create_6_0_10_processing_time_scales_with_exact_press_and_mixer_speed_curves() {
    // PressingBehaviour: lerp(clamp(abs(rpm)/512,0,1),1,60), cast to int.
    assert_eq!(create_press_running_tick_speed(64), 8);
    assert_eq!(create_press_running_tick_speed(128), 15);
    assert_eq!(create_press_running_tick_speed(256), 30);
    assert_eq!(create_press_running_tick_speed(0), 0);

    // MechanicalMixerBlockEntity: log2((int)(512/rpm)) * ceil(recipeSpeed*15) + 1.
    assert_eq!(create_mixer_processing_ticks(30, 0), 61);
    assert_eq!(create_mixer_processing_ticks(64, 0), 46);
    assert_eq!(create_mixer_processing_ticks(256, 0), 16);
    assert_eq!(create_mixer_processing_ticks(64, 20), 10);

    let mut normal = CreateProcessingMachine::new(
        ProcessingMachineKind::MechanicalPress,
        processing_recipe(32),
        4,
        4,
    )
    .unwrap();
    normal.insert_input(10, 1).unwrap();
    normal.set_rpm(64);
    assert_eq!(complete_after(&mut normal, 100), 31);

    let mut fast = CreateProcessingMachine::new(
        ProcessingMachineKind::MechanicalPress,
        processing_recipe(32),
        4,
        4,
    )
    .unwrap();
    fast.insert_input(10, 1).unwrap();
    fast.set_rpm(128);
    assert_eq!(complete_after(&mut fast, 100), 17);
}

#[test]
fn create_6_0_10_near_full_basin_holds_all_or_nothing_byproduct_output() {
    let recipe = byproduct_recipe();
    let mut machine = CreateProcessingMachine::new(
        ProcessingMachineKind::MechanicalPress,
        recipe,
        4,
        4,
    )
    .unwrap();
    machine.insert_input(10, 2).unwrap();
    machine.set_rpm(64);
    assert_eq!(complete_after(&mut machine, 40), 31);
    assert_eq!(machine.snapshot().output_items, 4);

    // Create simulates the whole output before consuming inputs. The second
    // craft therefore refuses to start while all four result units cannot fit
    // into the already full basin; the input is not consumed or duplicated.
    assert_eq!(machine.tick(), ProcessorTickState::Idle);
    assert_eq!(machine.snapshot().output_items, 4);
    assert_eq!(machine.snapshot().input_items, 1);
    assert!(!machine.snapshot().active);

    // Freeing only the one-count byproduct slot is still insufficient for
    // all-or-nothing output. No partial byproduct is dropped or fabricated.
    assert_eq!(machine.extract_output(21, 1), 1);
    assert_eq!(machine.tick(), ProcessorTickState::Idle);
    assert_eq!(machine.snapshot().input_items, 1);
    assert!(!machine.snapshot().active);

    // Once every output unit fits, the retained input starts exactly one new
    // craft and completes with both primary and secondary outputs intact.
    assert_eq!(machine.extract_output(20, 3), 3);
    assert_eq!(machine.tick(), ProcessorTickState::Working);
    assert_eq!(machine.snapshot().input_items, 0);
    assert_eq!(complete_after(&mut machine, 40), 30);
    assert_eq!(machine.extract_output(20, 3), 3);
    assert_eq!(machine.extract_output(21, 1), 1);
    assert_eq!(machine.snapshot().output_items, 0);

    // If another machine fills the shared basin after this craft has started,
    // completion backpressures instead of emitting a partial byproduct set.
    let mut shared = CreateProcessingMachine::new(
        ProcessingMachineKind::MechanicalPress,
        byproduct_recipe(),
        4,
        5,
    )
    .unwrap();
    shared.insert_input(10, 1).unwrap();
    shared.set_rpm(64);
    assert_eq!(shared.tick(), ProcessorTickState::Working);
    assert_eq!(shared.insert_output(99, 2), 2);
    for _ in 1..30 {
        assert_eq!(shared.tick(), ProcessorTickState::Working);
    }
    assert_eq!(shared.tick(), ProcessorTickState::Backpressured);
    assert_eq!(shared.output_items(), 2);
    assert_eq!(shared.extract_output(99, 2), 2);
    assert_eq!(shared.tick(), ProcessorTickState::Completed);
    assert_eq!(shared.extract_output(20, 3), 3);
    assert_eq!(shared.extract_output(21, 1), 1);
}

// ---------------------------------------------------------------------------
// Mechanical Arm and Super Glue
// ---------------------------------------------------------------------------

#[test]
fn create_6_0_10_mechanical_arm_round_robin_forced_mode_and_reservation() {
    let mut arm = CreateMechanicalArm::new(ArmDistributionMode::RoundRobin, &[0, 2, 2]);
    // Round-robin skips unavailable target 0 rather than waiting forever.
    assert_eq!(arm.begin_transfer(1).unwrap(), Some(1));
    assert_eq!(arm.in_flight(), Some((1, 1)));
    // The in-flight reservation is exclusive: a second dispatch cannot use it.
    assert_eq!(arm.begin_transfer(1).unwrap(), None);
    assert_eq!(arm.complete_transfer().unwrap(), 1);
    assert_eq!(arm.target(1).unwrap().held, 1);
    assert_eq!(arm.next_target(), 2);

    let mut forced = CreateMechanicalArm::new(ArmDistributionMode::ForcedRoundRobin, &[0, 2]);
    assert_eq!(forced.begin_transfer(1).unwrap(), None);
    assert_eq!(forced.in_flight(), None);

    // Prefer-first mode keeps using target 0 while it has room instead of
    // advancing the circular cursor.
    let mut prefer = CreateMechanicalArm::new(ArmDistributionMode::PreferFirstTarget, &[2, 2]);
    assert_eq!(prefer.begin_transfer(1).unwrap(), Some(0));
    prefer.complete_transfer().unwrap();
    assert_eq!(prefer.begin_transfer(1).unwrap(), Some(0));
    prefer.complete_transfer().unwrap();
    assert_eq!(prefer.begin_transfer(1).unwrap(), Some(1));

    // Reserved capacity counts before completion, preventing overfill.
    let mut reserved = CreateMechanicalArm::new(ArmDistributionMode::RoundRobin, &[1]);
    assert_eq!(reserved.begin_transfer(1).unwrap(), Some(0));
    reserved.cancel_transfer().unwrap();
    assert_eq!(reserved.target(0).unwrap().reserved, 0);
    assert_eq!(reserved.begin_transfer(1).unwrap(), Some(0));
    reserved.complete_transfer().unwrap();
    assert_eq!(reserved.begin_transfer(1).unwrap(), None);
}

#[test]
fn create_6_0_10_super_glue_bounds_are_inclusive_and_separated_seams_stay_unglued() {
    // Opposite corners normalize; both inclusive boundary cells are covered.
    let glue_box = SuperGlueBox::new([1, 64, 0], [0, 65, 1]);
    assert!(glue_box.contains([0, 64, 0]));
    assert!(glue_box.contains([1, 65, 1]));
    assert!(!glue_box.contains([2, 64, 0]));

    let mut volume = SuperGlueVolume::from_boxes(vec![glue_box]);
    assert!(volume.bridges([0, 64, 0], [1, 64, 0]));
    assert!(volume.bridges([0, 64, 0], [0, 65, 0]));
    assert!(!volume.bridges([1, 64, 0], [2, 64, 0]));

    // A second glued island does not weld the unglued seam between islands:
    // each seam still needs one box covering both cells.
    volume.add_box(SuperGlueBox::new([2, 64, 0], [3, 64, 0]));
    assert!(volume.bridges([2, 64, 0], [3, 64, 0]));
    assert!(!volume.bridges([1, 64, 0], [2, 64, 0]));
    assert!(!volume.bridges([3, 64, 0], [4, 64, 0]));
}
