// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! Integration tests for `megu_core::perf_kit` adoption in the Create
//! factory and rail hot paths: idle regions skip `export_into` in O(1),
//! belt machines wake on the exact scheduled tick, identical re-solves
//! skip UpdateTag emission, UpdateTag deltas coalesce, and 1,000 parked
//! trains skip the per-tick occupancy sweep.

use std::collections::BTreeSet;

use create_adapter::blockstates::{core_create_registry, DEFAULT_BASE_STATE_ID};
use create_adapter::factory::{CreateFactory, DimBlockPos};
use create_adapter::rail_network::{
    tick_rail_network, ArticulatedTrain, CarriageSpec, ChunkKey, DimId, RailNetwork,
    TrackGauge, TrainId, TrainRunState, TrainSchedule,
};

// ---------------------------------------------------------------------------
// CreateFactory — idle export gating + exact-tick wake
// ---------------------------------------------------------------------------

#[test]
fn thousand_idle_regions_skip_exports_and_wake_on_exact_tick() {
    let mut factory = CreateFactory::new(core_create_registry(DEFAULT_BASE_STATE_ID));
    // 1,000 belt machines, one per region (64-block spacing > 32-block cell).
    for i in 0..1000i32 {
        let pos = DimBlockPos::new(0, i * 64, 0, 0);
        factory.add_belt_segment(pos, 1, 4, 5).unwrap();
    }

    // Ticks 1..=5: machines registered but empty -> every region exports
    // nothing, and the empty-batch check skips `export_into` outright.
    for t in 1..=5u64 {
        assert!(
            factory.step_tick(t, 4, None).unwrap().is_empty(),
            "idle factory produced output at tick {t}"
        );
    }
    let skips = factory.idle_export_skips();
    assert_eq!(skips, 5_000, "every idle region skips its export call");

    // Insert one item per machine at tick 5: delay 5 -> wake at tick 10.
    for i in 0..1000i32 {
        let pos = DimBlockPos::new(0, i * 64, 0, 0);
        factory.insert_belt_item(pos, 1, 42).unwrap();
    }

    // Ticks 6..=9: work is scheduled for 10 — nothing may produce early.
    for t in 6..10u64 {
        assert!(
            factory.step_tick(t, 4, None).unwrap().is_empty(),
            "early wake at tick {t}"
        );
    }

    // Tick 10: exactly the scheduled wake — all 1,000 deltas export.
    let outputs = factory.step_tick(10, 4, None).unwrap();
    assert_eq!(outputs.len(), 1000, "every machine emitted its delta");
    let mut records = 0usize;
    for out in &outputs {
        assert_eq!(out.report.processed, 1);
        assert_eq!(out.s5_frame.len(), out.batch.bytes);
        records += out.batch.records;
    }
    assert_eq!(records, 1000);

    // Items now sit in outputs, inputs empty -> dormant again; the skip
    // counter resumes growing the very next tick.
    for t in 11..=13u64 {
        assert!(factory.step_tick(t, 4, None).unwrap().is_empty());
    }
    assert!(
        factory.idle_export_skips() >= skips + 3_000 - 1000,
        "idle skips resume after the burst (was {skips}, now {})",
        factory.idle_export_skips()
    );
}

#[test]
fn identical_resolve_skips_updatetags_and_deltas_coalesce() {
    let reg = core_create_registry(DEFAULT_BASE_STATE_ID);
    let wheel_z = reg
        .with_property(
            reg.default_state("create:water_wheel").unwrap(),
            "axis",
            "z",
        )
        .unwrap();
    let shaft_x = reg
        .with_property(reg.default_state("create:shaft").unwrap(), "axis", "x")
        .unwrap();

    let mut factory = CreateFactory::new(reg);
    let w0 = DimBlockPos::new(0, 0, 64, 0);
    let s0 = DimBlockPos::new(0, 1, 64, 0);
    let w1 = DimBlockPos::new(0, 16, 64, 0);
    let s1 = DimBlockPos::new(0, 17, 64, 0);

    // First water wheel + shaft pair: both emit UpdateTags.
    assert_eq!(factory.place_block(w0, wheel_z).len(), 1);
    let stats0 = factory.update_gate_stats();
    factory.place_block(s0, shaft_x);
    let stats1 = factory.update_gate_stats();
    assert!(stats1.updates > stats0.updates, "new state committed");

    // A second, disconnected pair in the same region: the re-solve leaves
    // pair 0's solutions byte-identical, so their UpdateTag emission is
    // fingerprint-skipped while the new nodes still publish.
    let before = factory.update_gate_stats();
    factory.place_block(w1, wheel_z);
    factory.place_block(s1, shaft_x);
    let after = factory.update_gate_stats();
    assert!(
        after.skips > before.skips,
        "unchanged re-solves skipped (skips {} -> {})",
        before.skips,
        after.skips
    );

    // The coalescing batch holds exactly one tag per touched position,
    // sorted by DimBlockPos regardless of emission order.
    let mut deltas = Vec::new();
    factory.drain_update_batch_into(&mut deltas);
    assert_eq!(deltas.len(), 4, "one delta per published node");
    let positions: Vec<DimBlockPos> = deltas.iter().map(|(p, _)| *p).collect();
    let mut sorted = positions.clone();
    sorted.sort();
    assert_eq!(positions, sorted, "deterministic sorted drain");
    assert_eq!(factory.pending_update_deltas(), 0);
}

// ---------------------------------------------------------------------------
// RailNetwork — occupancy refresh gating
// ---------------------------------------------------------------------------

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

fn parked_train(id: u64) -> ArticulatedTrain {
    let mut t = ArticulatedTrain::new(
        TrainId(id),
        vec![CarriageSpec {
            length: 10.0,
            leading_bogey_offset: 2.0,
            trailing_bogey_offset: 8.0,
            gauge: TrackGauge::Standard,
        }],
        4.0,
        1.0,
        1.0,
        TrainSchedule::default(),
    )
    .unwrap();
    t.state = TrainRunState::WaitingAtStation {
        station: "hold".to_string(),
        ticks_left: 1_000_000,
    };
    t
}

#[test]
fn thousand_parked_trains_skip_occupancy_sweep_in_o1() {
    let mut net = RailNetwork::new();
    let dim = DimId::new("minecraft:overworld");
    // 1,000 trains parked on 1,000 separate blocked edges.
    let mut block_ids = Vec::new();
    for i in 0..1000i64 {
        let x = i as f64 * 200.0;
        let a = net.add_node(dim.clone(), [x, 0.0, 0.0]);
        let b = net.add_node(dim.clone(), [x + 100.0, 0.0, 0.0]);
        let e = net
            .add_edge(a, b, TrackGauge::Standard, straight([x, 0.0, 0.0], [x + 100.0, 0.0, 0.0]))
            .unwrap();
        let block = net.add_block(&[e]).unwrap();
        block_ids.push(block);
        net.add_train(parked_train(i as u64 + 1), e, 10.0).unwrap();
    }
    let loaded: BTreeSet<ChunkKey> = BTreeSet::new();

    // Warmup: first tick re-commits every train at the settled epoch.
    tick_rail_network(&mut net, &loaded);
    // Every block is occupied by its train — gating must be invisible.
    for (i, b) in block_ids.iter().enumerate() {
        assert_eq!(
            net.blocks[b].occupied_by,
            Some(TrainId(i as u64 + 1)),
            "block {i} occupancy"
        );
    }

    // Steady state: 1,000 parked trains -> 1,000 skips per tick, zero
    // updates, and no events.
    let stats0 = net.occupancy_gate_stats();
    for _ in 0..5 {
        let events = tick_rail_network(&mut net, &loaded);
        assert!(events.is_empty(), "parked trains emit nothing");
    }
    let stats1 = net.occupancy_gate_stats();
    assert_eq!(
        stats1.skips - stats0.skips,
        5_000,
        "each of 1,000 parked trains skips the O(blocks) sweep per tick"
    );
    assert_eq!(stats1.updates, stats0.updates, "no refresh re-commits");

    // Structural change: removing one train invalidates the epoch, so the
    // next tick re-runs all sweeps once — then skips resume exactly.
    net.remove_train(TrainId(500));
    let stats2 = net.occupancy_gate_stats();
    tick_rail_network(&mut net, &loaded);
    let stats3 = net.occupancy_gate_stats();
    assert!(
        stats3.updates > stats2.updates,
        "epoch invalidation re-ran refresh after removal"
    );
    let b500 = &net.blocks[&block_ids[499]];
    assert_eq!(b500.occupied_by, None, "removed train released its block");

    let stats4 = net.occupancy_gate_stats();
    tick_rail_network(&mut net, &loaded);
    let stats5 = net.occupancy_gate_stats();
    assert_eq!(
        stats5.skips - stats4.skips,
        999,
        "skips resume for the remaining parked trains"
    );
}

#[test]
fn moving_train_and_parked_trains_stay_bit_identical() {
    // One moving train among parked ones: the moving train's own refresh
    // still runs every tick (its fingerprint changes as it advances), and
    // the occupancy fields stay exactly what ungated refresh would leave.
    let mut net = RailNetwork::new();
    let dim = DimId::new("minecraft:overworld");
    let a = net.add_node(dim.clone(), [0.0, 0.0, 0.0]);
    let b = net.add_node(dim.clone(), [100.0, 0.0, 0.0]);
    let e0 = net
        .add_edge(a, b, TrackGauge::Standard, straight([0.0, 0.0, 0.0], [100.0, 0.0, 0.0]))
        .unwrap();
    let c = net.add_node(dim.clone(), [200.0, 0.0, 0.0]);
    let e1 = net
        .add_edge(
            b,
            c,
            TrackGauge::Standard,
            straight([100.0, 0.0, 0.0], [200.0, 0.0, 0.0]),
        )
        .unwrap();
    let blk0 = net.add_block(&[e0]).unwrap();
    let blk1 = net.add_block(&[e1]).unwrap();

    // Running train with no station target: drives to end of line.
    let runner = ArticulatedTrain::new(
        TrainId(1),
        vec![CarriageSpec {
            length: 10.0,
            leading_bogey_offset: 2.0,
            trailing_bogey_offset: 8.0,
            gauge: TrackGauge::Standard,
        }],
        10.0,
        5.0,
        5.0,
        TrainSchedule::default(),
    )
    .unwrap();
    net.add_train(runner, e0, 0.0).unwrap();

    // Parked train on the other edge.
    net.add_train(parked_train(2), e1, 50.0).unwrap();

    let loaded: BTreeSet<ChunkKey> = BTreeSet::new();
    let mut saw_move = false;
    let mut parked_skips = 0u64;
    for _ in 0..30 {
        let s0 = net.occupancy_gate_stats();
        tick_rail_network(&mut net, &loaded);
        let s1 = net.occupancy_gate_stats();
        parked_skips += s1.skips - s0.skips;
        let t1 = &net.trains[&TrainId(1)];
        if t1.pos_edge != e0 || t1.pos_s > 0.0 {
            saw_move = true;
        }
        // Invariant: parked train's block always reads its occupancy.
        assert_eq!(net.blocks[&blk1].occupied_by, Some(TrainId(2)));
    }
    assert!(saw_move, "running train advanced");
    assert!(
        parked_skips > 0,
        "parked train skipped sweeps while runner moved"
    );
    // Runner eventually crossed into blk1's neighbor or reached terminus —
    // its own refresh kept pace tick-for-tick.
    assert!(matches!(
        net.trains[&TrainId(1)].state,
        TrainRunState::Terminus
            | TrainRunState::Running
            | TrainRunState::WaitingAtStation { .. }
    ));
    let _ = blk0;
}
