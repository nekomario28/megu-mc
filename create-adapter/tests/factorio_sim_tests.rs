// SPDX-License-Identifier: LGPL-3.0-or-later
//! Measured proof for `create_adapter::factorio_sim`:
//!
//! * a 100-block Create belt carrying 64 item stacks conserves every item,
//!   advances in O(1) while free-flowing, jams and unjams through
//!   depot/funnel handoff, flips direction on negative RPM, and sleeps at
//!   zero visits while empty or fully packed;
//! * a 1,000-node kinetic factory of steam engines, shafts, gearboxes, and
//!   crushing wheels propagates signed fixed-point RPM exactly, halts on
//!   `impact > capacity` and recovers, and visits zero nodes across 500
//!   steady-state ticks;
//! * 2,000 idle machines sleep at zero visits while 20 continuously-fed
//!   presses cost 20 visits per tick — a ≥100:1 visit reduction — and a
//!   long craft wakes on its exact deadline tick;
//! * shuffled block placement yields bit-identical steady-state RPMs and
//!   stress totals.

use create_adapter::factorio_sim::{
    ratio_fp, rpm_fp, rpm_to_belt_step, ActiveFactoryScheduler, ComponentStats, CreateBeltLine,
    KineticKind, KineticNetworkPool, KineticStatus, MachineKind, SUB_UNITS_PER_BLOCK,
};

// ---------------------------------------------------------------------------
// CreateBeltLine
// ---------------------------------------------------------------------------

#[test]
fn belt_line_100_blocks_conserves_64_stacks_with_o1_advance() {
    let mut belt = CreateBeltLine::for_blocks(100);
    assert_eq!(belt.len_sub_units(), 100 * SUB_UNITS_PER_BLOCK);
    assert_eq!(rpm_to_belt_step(64), SUB_UNITS_PER_BLOCK as i32);
    assert_eq!(rpm_to_belt_step(-64), -(SUB_UNITS_PER_BLOCK as i32));
    assert_eq!(rpm_to_belt_step(0), 0);

    // Empty lines sleep at zero visits even while powered.
    belt.set_rpm(64);
    for _ in 0..8 {
        assert_eq!(belt.advance(), 0);
        assert!(belt.is_asleep());
    }

    // Funnel-paced loading: a stack enters whenever the back edge has room.
    // Each advance frees `step` sub-units of insert room.
    for id in 1..=64u32 {
        let mut spins = 0u32;
        while belt.try_insert_back(id, 64, 0).is_err() {
            assert_eq!(belt.advance(), 1);
            spins += 1;
            assert!(spins < 64, "belt never opened insert room");
        }
        // Strict conservation at every observable point.
        assert_eq!(
            belt.inserted_items(),
            belt.extracted_items() + belt.on_belt_items()
        );
    }
    assert_eq!(belt.item_slots(), 64);
    assert_eq!(belt.inserted_items(), 64 * 64);
    assert_eq!(belt.on_belt_items(), 64 * 64);

    // Steady state: free-flowing advance rewrites only the head gap — O(1),
    // exactly one visited stack per tick regardless of stack count.
    for _ in 0..32 {
        assert_eq!(belt.advance(), 1);
    }
    assert!(!belt.is_asleep());

    // Jam: stop extracting. The head parks at the exit edge, slack compresses
    // toward the tail, and once fully packed the line sleeps at zero visits.
    let mut guard = 0u32;
    while belt.advance() > 0 {
        guard += 1;
        assert!(guard < 10_000, "packed sleep never reached");
    }
    assert!(belt.is_asleep());
    assert_eq!(belt.items()[0].gap_ahead, 0, "head sits at the exit edge");
    for _ in 0..4 {
        assert_eq!(belt.advance(), 0, "packed line must not scan");
    }

    // Unjam: depot handoff wakes the line; stacks drain in FIFO order.
    let mut popped = vec![belt.try_extract_front().unwrap().item_id];
    assert_eq!(popped[0], 1);
    assert!(belt.advance() >= 1, "extraction must wake the line");
    guard = 0;
    while belt.item_slots() > 0 {
        while let Some(it) = belt.try_extract_front() {
            popped.push(it.item_id);
        }
        belt.advance();
        guard += 1;
        assert!(guard < 10_000);
    }
    assert_eq!(popped, (1..=64).collect::<Vec<_>>());
    assert_eq!(belt.extracted_items(), belt.inserted_items());
    assert_eq!(belt.on_belt_items(), 0);
    assert_eq!(belt.advance(), 0, "empty line sleeps again");
}

#[test]
fn belt_line_reverse_rpm_flips_direction_and_fifo_order() {
    let mut belt = CreateBeltLine::for_blocks(16);
    belt.set_rpm(64);
    for id in 1..=8u32 {
        let mut spins = 0u32;
        while belt.try_insert_back(id, 1, 0).is_err() {
            belt.advance();
            spins += 1;
            assert!(spins < 64);
        }
    }
    for _ in 0..8 {
        belt.advance();
    }
    let order_before: Vec<u32> = belt.items().iter().map(|it| it.item_id).collect();
    assert_eq!(order_before, (1..=8).collect::<Vec<_>>());

    // Negative RPM mirrors the line: the old tail becomes the head at the
    // (former) insert edge.
    belt.set_rpm(-64);
    let mut popped = Vec::new();
    let mut guard = 0u32;
    while belt.item_slots() > 0 {
        while let Some(it) = belt.try_extract_front() {
            popped.push(it.item_id);
        }
        belt.advance();
        guard += 1;
        assert!(guard < 10_000);
    }
    let mut expected = order_before;
    expected.reverse();
    assert_eq!(popped, expected);
    assert_eq!(belt.inserted_items(), belt.extracted_items());
}

// ---------------------------------------------------------------------------
// KineticNetworkPool
// ---------------------------------------------------------------------------

#[test]
fn kinetic_pool_1000_nodes_propagates_rpm_and_sleeps_at_zero_visits() {
    let mut pool = KineticNetworkPool::new();
    const COMPONENTS: usize = 250;
    // Component: steam engine → shaft → gearbox (direction flip) → crusher (2x).
    for _ in 0..COMPONENTS {
        let engine = pool.add_node(KineticKind::SteamEngine);
        let shaft = pool.add_node(KineticKind::Shaft);
        let gear = pool.add_node(KineticKind::Gearbox);
        let crusher = pool.add_node(KineticKind::CrushingWheel);
        pool.connect(engine, shaft, create_adapter::factorio_sim::RPM_FP_ONE)
            .unwrap();
        pool.connect(shaft, gear, -create_adapter::factorio_sim::RPM_FP_ONE)
            .unwrap();
        pool.connect(gear, crusher, ratio_fp(2, 1)).unwrap();
    }
    assert_eq!(pool.node_count(), 1000);

    // Exact signed fixed-point propagation: 16 → 16 → −16 → −32 rpm.
    for c in 0..COMPONENTS {
        let base = (c * 4) as u32;
        assert_eq!(pool.rpm(base), rpm_fp(16));
        assert_eq!(pool.rpm(base + 1), rpm_fp(16));
        assert_eq!(pool.rpm(base + 2), rpm_fp(-16));
        assert_eq!(pool.rpm(base + 3), rpm_fp(-32));
        let stats = pool.component_stats(base).unwrap();
        assert_eq!(stats.status, KineticStatus::Running);
        assert_eq!(stats.capacity, 2048);
        assert_eq!(stats.impact, 128);
        assert_eq!(stats.base_rpm, rpm_fp(16));
        assert_eq!(pool.component_id(base + 3), Some(base));
    }

    // Steady state: 500 ticks visit zero kinetic nodes.
    let mut visited = 0u64;
    for _ in 0..500 {
        visited += u64::from(pool.tick());
    }
    assert_eq!(visited, 0);
    assert_eq!(pool.steady_nodes_visited(), 0);

    // A deferred wake touches only the marked component (4 nodes).
    pool.mark_dirty(0);
    assert_eq!(pool.tick(), 4);
    assert_eq!(pool.tick(), 0);

    // Overstress: pushing one crusher's impact past capacity halts the whole
    // component (rpm → 0 everywhere) — Create's overstressed state.
    assert_eq!(pool.set_impact(3, 5000), 4);
    let stats = pool.component_stats(0).unwrap();
    assert_eq!(stats.status, KineticStatus::Overstressed);
    for i in 0..4u32 {
        assert_eq!(pool.rpm(i), 0);
        assert!(pool.is_overstressed(i));
    }
    // Adding capacity recovers the same component cleanly.
    pool.set_capacity(0, 8192);
    assert_eq!(
        pool.component_stats(0).unwrap().status,
        KineticStatus::Running
    );
    assert_eq!(pool.rpm(3), rpm_fp(-32));
    for _ in 0..10 {
        assert_eq!(pool.tick(), 0);
    }

    // Clutch toggle: disengaging splits the component, engaging re-merges.
    let engine = pool.add_node(KineticKind::SteamEngine);
    let clutch = pool.add_node(KineticKind::Clutch);
    let shaft = pool.add_node(KineticKind::Shaft);
    pool.connect(engine, clutch, create_adapter::factorio_sim::RPM_FP_ONE)
        .unwrap();
    pool.connect(clutch, shaft, create_adapter::factorio_sim::RPM_FP_ONE)
        .unwrap();
    assert_eq!(pool.rpm(shaft), rpm_fp(16));
    pool.set_clutch(clutch, false);
    assert_eq!(pool.rpm(shaft), 0);
    assert_eq!(pool.status(shaft), Some(KineticStatus::Unpowered));
    assert_ne!(pool.component_id(clutch), pool.component_id(shaft));
    pool.set_clutch(clutch, true);
    assert_eq!(pool.rpm(shaft), rpm_fp(16));
    assert_eq!(pool.component_id(clutch), pool.component_id(shaft));
}

// ---------------------------------------------------------------------------
// ActiveFactoryScheduler
// ---------------------------------------------------------------------------

#[test]
fn scheduler_2000_idle_20_active_100_to_1_visit_reduction() {
    let mut sched = ActiveFactoryScheduler::new();
    for _ in 0..2000 {
        sched.add_machine(MachineKind::MechanicalMixer, 4, 4, 5, true);
    }
    // Output buffers are deep enough that no press ever back-pressures.
    let mut active = Vec::new();
    for _ in 0..20 {
        let id = sched.add_machine(MachineKind::MechanicalPress, 1024, 1024, 1, true);
        assert_eq!(sched.push_input(id, 600).unwrap(), 600);
        active.push(id);
    }

    let mut visits = 0u64;
    for t in 1..=500u64 {
        visits += u64::from(sched.run_to(t).unwrap());
    }
    let naive = 2020u64 * 500;
    assert!(
        naive >= visits * 100,
        "visit reduction {naive}:{visits} must be ≥ 100:1"
    );
    // Every press is visited exactly once per tick at steady state.
    assert_eq!(sched.run_to(501).unwrap(), 20);
    assert_eq!(sched.sleeping_machines(), 2000);
    assert_eq!(sched.awake_machines(), 20);
    for &id in &active {
        assert!(sched.snapshot(id).unwrap().busy);
    }

    // Exact processing deadline: a 37-tick craft completes at its scheduled
    // tick — never early, never late.
    let slow = sched.add_machine(MachineKind::FluidPump, 4, 4, 37, true);
    sched.push_input(slow, 1).unwrap();
    let now = sched.now();
    sched.run_to(now).unwrap();
    let done = sched.snapshot(slow).unwrap().done_at.unwrap();
    assert_eq!(done, now + 37);
    sched.run_to(done - 1).unwrap();
    assert_eq!(sched.snapshot(slow).unwrap().output_items, 0);
    sched.run_to(done).unwrap();
    assert_eq!(sched.snapshot(slow).unwrap().output_items, 1);

    // Power loss freezes a craft mid-flight; repowering resumes the residue.
    let freezer = sched.add_machine(MachineKind::CrushingWheels, 4, 4, 50, true);
    sched.push_input(freezer, 2).unwrap();
    let t0 = sched.now();
    sched.run_to(t0).unwrap(); // consume the push poke, start the craft
    sched.run_to(t0 + 10).unwrap();
    sched.set_powered(freezer, false).unwrap();
    sched.run_to(t0 + 10).unwrap(); // freeze: 50 − 10 = 40 ticks remain
    sched.run_to(t0 + 100).unwrap();
    assert_eq!(sched.snapshot(freezer).unwrap().output_items, 0);
    sched.set_powered(freezer, true).unwrap();
    sched.run_to(t0 + 100).unwrap(); // resume: done_at = t0+100 + 40
    sched.run_to(t0 + 139).unwrap();
    assert_eq!(sched.snapshot(freezer).unwrap().output_items, 0);
    sched.run_to(t0 + 140).unwrap();
    assert_eq!(sched.snapshot(freezer).unwrap().output_items, 1);

    // One-shot catch-up equals per-tick stepping: bit-identical digests.
    let build_sched = || {
        let mut s = ActiveFactoryScheduler::new();
        for _ in 0..64 {
            s.add_machine(MachineKind::Deployer, 8, 8, 7, true);
        }
        for id in 0..64 {
            s.push_input(id, 8).unwrap();
        }
        s
    };
    let mut stepped = build_sched();
    let mut jumped = build_sched();
    for t in 1..=1000u64 {
        stepped.run_to(t).unwrap();
    }
    jumped.run_to(1000).unwrap();
    assert_eq!(stepped.digest(), jumped.digest());
}

// ---------------------------------------------------------------------------
// Order-independent determinism
// ---------------------------------------------------------------------------

#[test]
fn kinetic_pool_shuffled_placement_yields_identical_steady_state() {
    const COMPONENTS: usize = 100;

    // Same node ids, same topology; only the connect() call order differs.
    let build = |order: &[usize]| {
        let mut pool = KineticNetworkPool::new();
        for _ in 0..COMPONENTS {
            pool.add_node(KineticKind::SteamEngine);
            pool.add_node(KineticKind::Shaft);
            pool.add_node(KineticKind::Gearbox);
            pool.add_node(KineticKind::CrushingWheel);
        }
        for &c in order {
            let base = (c * 4) as u32;
            pool.connect(base, base + 1, create_adapter::factorio_sim::RPM_FP_ONE)
                .unwrap();
            pool.connect(
                base + 1,
                base + 2,
                -create_adapter::factorio_sim::RPM_FP_ONE,
            )
            .unwrap();
            pool.connect(base + 2, base + 3, ratio_fp(2, 1)).unwrap();
        }
        pool
    };

    let sequential: Vec<usize> = (0..COMPONENTS).collect();
    // Deterministic Fisher–Yates shuffle (LCG — no external crates).
    let mut rng = 0x9e37_79b9_7f4a_7c15u64;
    let mut shuffled = sequential.clone();
    for i in (1..COMPONENTS).rev() {
        rng = rng
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        let j = (rng >> 33) as usize % (i + 1);
        shuffled.swap(i, j);
    }
    assert_ne!(sequential, shuffled);

    let pool_a = build(&sequential);
    let pool_b = build(&shuffled);

    for id in 0..(COMPONENTS as u32 * 4) {
        assert_eq!(pool_a.rpm(id), pool_b.rpm(id), "rpm differs at {id}");
        assert_eq!(pool_a.status(id), pool_b.status(id));
        assert_eq!(pool_a.component_id(id), pool_b.component_id(id));
    }
    let key = |s: &ComponentStats| (s.capacity, s.impact, s.status as u8, s.base_rpm);
    let mut stats_a: Vec<ComponentStats> = (0..COMPONENTS)
        .map(|c| pool_a.component_stats((c * 4) as u32).unwrap())
        .collect();
    let mut stats_b: Vec<ComponentStats> = (0..COMPONENTS)
        .map(|c| pool_b.component_stats((c * 4) as u32).unwrap())
        .collect();
    stats_a.sort_by_key(key);
    stats_b.sort_by_key(key);
    assert_eq!(stats_a, stats_b);
}
