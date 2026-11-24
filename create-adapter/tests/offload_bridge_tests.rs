// SPDX-License-Identifier: LGPL-3.0-or-later

//! Focused suite for [`create_adapter::offload_bridge`]: region-disjoint
//! partitioning, FNV-1a digest parity across worker counts, fail-closed
//! stress/cap/budget behaviour, and factory-side region stepping.

use std::collections::BTreeMap;

use create_adapter::blockstates::{core_create_registry, DEFAULT_BASE_STATE_ID};
use create_adapter::contraption_adapter::{ContraptionKind, LiveContraption};
use create_adapter::factory::{CreateFactory, DimBlockPos};
use create_adapter::offload_bridge::{
    AssemblyStatus, AssemblyStress, ContraptionOffloadPlan, OffloadBudget, OffloadBridge,
    OffloadError, OffloadTickReport, MAX_ASSEMBLIES, MAX_BLOCKS_PER_ASSEMBLY,
};
use mod_offload::kinetic_graph::NetworkStatus;
use mod_offload::region_scheduler::RegionKey;

fn bearing(rpm: f32) -> ContraptionKind {
    ContraptionKind::Bearing {
        angular_deg_per_tick: rpm,
    }
}

fn linear(velocity: [f64; 3]) -> ContraptionKind {
    ContraptionKind::Linear { velocity_per_tick: velocity }
}

/// A `len`-block contraption: a straight run of shaft blocks along +X from
/// `anchor`, assembled through the real flood-fill path.
fn line_contraption(
    id: u32,
    dim: i32,
    anchor: [i32; 3],
    len: usize,
    kind: ContraptionKind,
    max_blocks: usize,
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
        max_blocks,
    )
    .expect("test assembly must build");
    contraption
}

fn stress(capacity: f64, consumption: f64) -> AssemblyStress {
    AssemblyStress::new(capacity, consumption).unwrap()
}

/// Eight assemblies — two per 32-block region across all four
/// `(region_x, region_z)` cells of dimension 0 — with mixed drivers and
/// block costs (total demand 20 work units per tick).
fn build_fleet() -> OffloadBridge {
    let mut bridge = OffloadBridge::new();
    let specs: [(u32, [i32; 3], usize, ContraptionKind); 8] = [
        (1, [4, 64, 4], 1, bearing(15.0)),
        (2, [20, 64, 20], 2, linear([0.5, 0.0, 0.0])),
        (
            3,
            [40, 64, 4],
            3,
            ContraptionKind::Minecart {
                speed_per_tick: 0.25,
                heading_yaw: 90.0,
            },
        ),
        (
            4,
            [60, 64, 20],
            4,
            ContraptionKind::BoundedLinear {
                velocity_per_tick: [0.0, 0.5, 0.0],
                min_offset: [-2.0, -2.0, -2.0],
                max_offset: [2.0, 8.0, 2.0],
                reverse_at_bounds: true,
            },
        ),
        (5, [4, 64, 40], 1, bearing(-9.0)),
        (6, [20, 64, 60], 2, linear([0.0, 0.0, -0.25])),
        (
            7,
            [40, 64, 40],
            3,
            ContraptionKind::Minecart {
                speed_per_tick: 0.5,
                heading_yaw: 180.0,
            },
        ),
        (
            8,
            [60, 64, 60],
            4,
            ContraptionKind::BoundedLinear {
                velocity_per_tick: [0.25, 0.0, 0.0],
                min_offset: [-4.0, -4.0, -4.0],
                max_offset: [4.0, 4.0, 4.0],
                reverse_at_bounds: false,
            },
        ),
    ];
    for (id, anchor, len, kind) in specs {
        let contraption =
            line_contraption(id, 0, anchor, len, kind, MAX_BLOCKS_PER_ASSEMBLY);
        bridge
            .register_contraption(contraption, stress(100.0, 10.0))
            .unwrap();
    }
    assert_eq!(bridge.plan().batches().len(), 4);
    bridge
}

/// Every registered assembly is accounted for exactly once per tick.
fn assert_accounted(report: &OffloadTickReport, assemblies: usize) {
    assert_eq!(
        report.stepped
            + report.deferred
            + report.overstressed
            + report.halted
            + report.over_budget,
        assemblies,
        "tick {}: assembly accounting mismatch",
        report.tick
    );
}

#[test]
fn partitioned_execution_is_bit_identical_across_worker_counts() {
    let mut sequential = build_fleet();
    let mut two_way = build_fleet();
    let mut four_way = build_fleet();

    // Generous budget: all eight assemblies step every tick.
    for tick in 1..=8u64 {
        let a = sequential
            .step_tick(tick, OffloadBudget::new(1_000))
            .unwrap();
        let b = two_way
            .step_tick(tick, OffloadBudget::new(1_000).with_workers(2))
            .unwrap();
        let c = four_way
            .step_tick(tick, OffloadBudget::new(1_000).with_workers(4))
            .unwrap();
        assert_eq!(a.stepped, 8);
        assert_eq!(a.deferred, 0);
        assert_accounted(&a, 8);
        assert_eq!(a.digest, b.digest, "tick {tick}: workers=2 diverged");
        assert_eq!(a.digest, c.digest, "tick {tick}: workers=4 diverged");
        assert_eq!(a.work_units_used, b.work_units_used);
        assert_eq!(a.work_units_used, c.work_units_used);
    }

    // Tight budget (12 < 20 demand): throttling engages and digests must
    // still be bit-identical — partitioning changes scheduling order, never
    // results.
    let mut seq_tight = build_fleet();
    let mut par_tight = build_fleet();
    for tick in 1..=16u64 {
        let a = seq_tight.step_tick(tick, OffloadBudget::new(12)).unwrap();
        let b = par_tight
            .step_tick(tick, OffloadBudget::new(12).with_workers(4))
            .unwrap();
        assert!(a.deferred > 0, "tick {tick}: throttle never engaged");
        assert!(a.work_units_used <= 12);
        assert_accounted(&a, 8);
        assert_eq!(a.digest, b.digest, "tick {tick}: throttled parity broke");
        assert_eq!(a.work_units_used, b.work_units_used);
    }
    assert_eq!(seq_tight.state_digest(), par_tight.state_digest());

    // Prove the changed path really executed: bearings yawed, linears moved.
    assert!(sequential.assembly(1).unwrap().yaw_deg != 0.0);
    assert!(sequential.assembly(2).unwrap().translation[0] > 4.0);
    assert!(par_tight.assembly(5).unwrap().yaw_deg != 0.0);
    assert!(par_tight.assembly(6).unwrap().translation[2] < 60.0);
}

#[test]
fn overstressed_halts_fail_closed_and_recovers() {
    let mut bridge = OffloadBridge::new();
    // id1: consumption > capacity -> fail-closed freeze.
    bridge
        .register_contraption(
            line_contraption(1, 0, [4, 64, 4], 1, bearing(15.0), MAX_BLOCKS_PER_ASSEMBLY),
            stress(10.0, 20.0),
        )
        .unwrap();
    // id2: healthy bearing keeps moving.
    bridge
        .register_contraption(
            line_contraption(2, 0, [40, 64, 4], 1, bearing(15.0), MAX_BLOCKS_PER_ASSEMBLY),
            stress(100.0, 5.0),
        )
        .unwrap();
    // id3: non-finite driver state -> driver error latches Halted.
    let mut nan_driver = line_contraption(
        3,
        0,
        [4, 64, 40],
        1,
        bearing(15.0),
        MAX_BLOCKS_PER_ASSEMBLY,
    );
    nan_driver.translation[0] = f64::NAN;
    bridge
        .register_contraption(nan_driver, stress(100.0, 5.0))
        .unwrap();

    let r1 = bridge.step_tick(1, OffloadBudget::new(100)).unwrap();
    assert_eq!(r1.overstressed, 1);
    assert_eq!(r1.stepped, 1);
    assert_eq!(r1.halted, 1);
    assert_accounted(&r1, 3);
    assert_eq!(
        bridge.assembly_status(1),
        Some(AssemblyStatus::Overstressed)
    );
    assert_eq!(bridge.assembly(1).unwrap().yaw_deg, 0.0);
    assert!(bridge.assembly(2).unwrap().yaw_deg > 0.0);
    assert_eq!(bridge.assembly_status(3), Some(AssemblyStatus::Halted));

    // The freeze is stable: overstress and the halt latch persist.
    let r2 = bridge.step_tick(2, OffloadBudget::new(100)).unwrap();
    assert_eq!(bridge.assembly(1).unwrap().yaw_deg, 0.0);
    assert_eq!(r2.stepped, 1);
    assert_eq!(r2.halted, 1);
    assert_eq!(bridge.assembly(2).unwrap().yaw_deg, 30.0);

    // Raising capacity resumes the contraption; resuming the halted one
    // re-halts deterministically because the driver is still corrupt.
    bridge
        .set_assembly_stress(1, stress(50.0, 20.0))
        .unwrap();
    bridge.resume_assembly(3).unwrap();
    let r3 = bridge.step_tick(3, OffloadBudget::new(100)).unwrap();
    assert_eq!(bridge.assembly_status(1), Some(AssemblyStatus::Running));
    assert!(bridge.assembly(1).unwrap().yaw_deg > 0.0);
    assert_eq!(bridge.assembly_status(3), Some(AssemblyStatus::Halted));
    assert_eq!(r3.stepped, 2);
    assert_eq!(r3.halted, 1);
    assert_accounted(&r3, 3);
}

#[test]
fn assembly_and_block_caps_fail_closed() {
    let region = RegionKey::for_block(0, 0, 0);

    // Plan-level caps, no contraption construction needed.
    let too_many: Vec<(u32, RegionKey, usize)> = (0..=MAX_ASSEMBLIES as u32)
        .map(|id| (id, region, 1usize))
        .collect();
    assert!(matches!(
        ContraptionOffloadPlan::build(&too_many),
        Err(OffloadError::TooManyAssemblies { .. })
    ));
    let at_cap: Vec<(u32, RegionKey, usize)> = (0..MAX_ASSEMBLIES as u32)
        .map(|id| (id, region, 1usize))
        .collect();
    assert!(ContraptionOffloadPlan::build(&at_cap).is_ok());
    assert!(matches!(
        ContraptionOffloadPlan::build(&[(7, region, MAX_BLOCKS_PER_ASSEMBLY + 1)]),
        Err(OffloadError::AssemblyTooLarge { id: 7, .. })
    ));
    assert!(matches!(
        ContraptionOffloadPlan::build(&[
            (1, region, 1),
            (1, RegionKey::for_block(0, 64, 0), 1),
        ]),
        Err(OffloadError::DuplicateAssembly(1))
    ));

    // Registration-level block cap against a real 4097-block assembly.
    let mut bridge = OffloadBridge::new();
    let oversized = line_contraption(
        9,
        0,
        [0, 64, 0],
        MAX_BLOCKS_PER_ASSEMBLY + 1,
        bearing(15.0),
        usize::MAX,
    );
    assert!(matches!(
        bridge.register_contraption(oversized, stress(100.0, 1.0)),
        Err(OffloadError::AssemblyTooLarge { id: 9, .. })
    ));
    assert_eq!(bridge.assembly_count(), 0);

    // Exactly at the cap registers and plans cleanly.
    let at_limit = line_contraption(
        10,
        0,
        [100, 64, 0],
        MAX_BLOCKS_PER_ASSEMBLY,
        bearing(15.0),
        usize::MAX,
    );
    bridge
        .register_contraption(at_limit, stress(100.0, 1.0))
        .unwrap();
    assert_eq!(bridge.assembly_count(), 1);
    assert_eq!(bridge.plan().order(), &[10]);
}

#[test]
fn work_unit_budget_throttles_without_dropping_assemblies() {
    // Six 4-block assemblies (cost 4 each, demand 24/tick) on a budget of
    // 10 -> exactly two assemblies step per tick.
    let anchors: [[i32; 3]; 6] = [
        [4, 64, 4],
        [20, 64, 20],
        [40, 64, 4],
        [4, 64, 40],
        [20, 64, 60],
        [40, 64, 40],
    ];
    let build = || {
        let mut bridge = OffloadBridge::new();
        for (i, anchor) in anchors.iter().enumerate() {
            let id = i as u32 + 1;
            let c = line_contraption(id, 0, *anchor, 4, linear([0.25, 0.0, 0.0]), MAX_BLOCKS_PER_ASSEMBLY);
            bridge
                .register_contraption(c, stress(100.0, 1.0))
                .unwrap();
        }
        assert_eq!(bridge.plan().batches().len(), 4);
        bridge
    };

    let mut bridge = build();
    let mut mirror = build();
    let mut mirror_parallel = build();
    let mut total_stepped = 0usize;
    let mut saw_deferred = false;
    for tick in 1..=12u64 {
        let r = bridge.step_tick(tick, OffloadBudget::new(10)).unwrap();
        let m = mirror
            .step_tick(tick, OffloadBudget::new(10).with_workers(2))
            .unwrap();
        let p = mirror_parallel
            .step_tick(tick, OffloadBudget::new(10).with_workers(4))
            .unwrap();
        assert!(r.work_units_used <= 10);
        assert_eq!(bridge.assembly_count(), 6, "tick {tick}: assembly dropped");
        assert_accounted(&r, 6);
        total_stepped += r.stepped;
        saw_deferred |= r.deferred > 0;
        assert_eq!(r.digest, m.digest, "tick {tick}: workers=2 diverged");
        assert_eq!(r.digest, p.digest, "tick {tick}: workers=4 diverged");
    }
    assert!(saw_deferred);
    assert_eq!(total_stepped, 24, "expected 2 assemblies x 12 ticks");

    // Fairness: every assembly kept progressing — none starved behind the
    // budget. Each moved at least 3 ticks of 0.25 blocks.
    for (i, anchor) in anchors.iter().enumerate() {
        let c = bridge.assembly(i as u32 + 1).unwrap();
        let moved = c.translation[0] - f64::from(anchor[0]);
        assert!(
            moved >= 0.75,
            "assembly {} starved under budget: moved {moved}",
            i + 1
        );
    }
    assert_eq!(bridge.state_digest(), mirror.state_digest());
    assert_eq!(bridge.state_digest(), mirror_parallel.state_digest());
}

#[test]
fn over_budget_assembly_is_reported_not_dropped() {
    let mut bridge = OffloadBridge::new();
    // id1: 100-block contraption, cost 100 > budget 50 -> OverBudget forever.
    bridge
        .register_contraption(
            line_contraption(1, 0, [0, 64, 0], 100, bearing(15.0), usize::MAX),
            stress(100.0, 1.0),
        )
        .unwrap();
    // id2/id3: tiny assemblies that must keep stepping regardless.
    bridge
        .register_contraption(
            line_contraption(2, 0, [200, 64, 0], 1, bearing(10.0), MAX_BLOCKS_PER_ASSEMBLY),
            stress(100.0, 1.0),
        )
        .unwrap();
    bridge
        .register_contraption(
            line_contraption(3, 0, [232, 64, 0], 1, bearing(10.0), MAX_BLOCKS_PER_ASSEMBLY),
            stress(100.0, 1.0),
        )
        .unwrap();

    for tick in 1..=4u64 {
        let r = bridge.step_tick(tick, OffloadBudget::new(50)).unwrap();
        assert_eq!(r.over_budget, 1);
        assert_eq!(r.stepped, 2);
        assert_eq!(r.work_units_used, 2);
        assert_eq!(
            bridge.assembly_status(1),
            Some(AssemblyStatus::OverBudget)
        );
        assert_eq!(bridge.assembly_count(), 3);
        assert_accounted(&r, 3);
    }
    // The oversized-cost assembly never moved and was never dropped.
    assert_eq!(bridge.assembly(1).unwrap().yaw_deg, 0.0);

    // Raising the budget above its cost lets it run — still fail-closed,
    // just under a sufficient budget.
    let r = bridge.step_tick(5, OffloadBudget::new(200)).unwrap();
    assert_eq!(r.over_budget, 0);
    assert_eq!(r.stepped, 3);
    assert_eq!(bridge.assembly_status(1), Some(AssemblyStatus::Running));
    assert!(bridge.assembly(1).unwrap().yaw_deg > 0.0);
}

#[test]
fn factory_side_steps_regions_and_folds_outputs_into_digest() {
    let reg = core_create_registry(DEFAULT_BASE_STATE_ID);
    let wheel_z = reg
        .with_property(
            reg.default_state("create:water_wheel").unwrap(),
            "axis",
            "z",
        )
        .unwrap();
    let shaft_z = reg
        .with_property(reg.default_state("create:shaft").unwrap(), "axis", "z")
        .unwrap();

    let mut sequential = OffloadBridge::with_factory(CreateFactory::new(reg.clone()));
    let mut four_way = OffloadBridge::with_factory(CreateFactory::new(reg));
    for bridge in [&mut sequential, &mut four_way] {
        let factory = bridge.factory_mut().unwrap();
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
        let contraption = line_contraption(
            7,
            0,
            [4, 64, 4],
            2,
            bearing(10.0),
            MAX_BLOCKS_PER_ASSEMBLY,
        );
        bridge
            .register_contraption(contraption, stress(50.0, 5.0))
            .unwrap();
    }

    let mut saw_factory_output = false;
    for tick in 1..=10u64 {
        let a = sequential
            .step_tick(tick, OffloadBudget::new(100).with_workers(1))
            .unwrap();
        let b = four_way
            .step_tick(tick, OffloadBudget::new(100).with_workers(4))
            .unwrap();
        assert_eq!(a.digest, b.digest, "tick {tick}: factory parity broke");
        saw_factory_output |= a.factory_records > 0;
    }
    assert!(
        saw_factory_output,
        "belt transport never produced S5 batch records"
    );
    // The kinetic network really solved through the region scheduler.
    let solution = sequential
        .factory()
        .unwrap()
        .kinetic_solution(DimBlockPos::new(0, 0, 64, 1))
        .unwrap();
    assert_eq!(solution.status, NetworkStatus::Running);
    assert!(sequential.assembly(7).unwrap().yaw_deg != 0.0);
}
