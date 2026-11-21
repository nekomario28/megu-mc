// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later

//! Lane RAIL-1 integration suite: generic cubic-Bezier spline track graph,
//! track-gauge compatibility, block & chain signals, articulated
//! multi-carriage trains with unloaded-chunk virtual transit, and `RAIL`
//! CRC32 snapshots.
//!
//! Everything is generic over gauge ids — the suite exercises a custom
//! addon gauge (`example_addon:600mm`-style [`TrackGauge::Custom`]) with
//! zero hardcoded addon names in the adapter.

use std::collections::BTreeSet;

use create_adapter::rail_network::*;

const OW: &str = "minecraft:overworld";

fn dim() -> DimId {
    DimId::new(OW)
}

fn empty_loaded() -> BTreeSet<ChunkKey> {
    BTreeSet::new()
}

/// Straight-line Bezier control quad between two world points.
fn seg(a: [f64; 3], b: [f64; 3]) -> [[f64; 3]; 4] {
    let lerp = |t: f64| {
        [
            a[0] + (b[0] - a[0]) * t,
            a[1] + (b[1] - a[1]) * t,
            a[2] + (b[2] - a[2]) * t,
        ]
    };
    [a, lerp(1.0 / 3.0), lerp(2.0 / 3.0), b]
}

fn carriage(len: f64, gauge: TrackGauge) -> CarriageSpec {
    CarriageSpec {
        length: len,
        leading_bogey_offset: 1.5,
        trailing_bogey_offset: 1.5,
        gauge,
    }
}

fn free_run_train(id: u64, gauge: TrackGauge) -> ArticulatedTrain {
    ArticulatedTrain::new(
        TrainId(id),
        vec![carriage(4.0, gauge)],
        4.0,
        0.5,
        1.0,
        TrainSchedule::default(),
    )
    .unwrap()
}

fn edge_len(net: &RailNetwork, e: EdgeId) -> f64 {
    net.graph.edges[&e].length()
}

// ---------------------------------------------------------------------------
// Cubic Bezier arc-length LUT & tangents
// ---------------------------------------------------------------------------

#[test]
fn bezier_arc_length_lut_straight_curve_and_3d_ramp() {
    // A straight segment's LUT length is exact.
    let seg = CubicBezierSegment::new(seg([0.0, 0.0, 0.0], [30.0, 0.0, 0.0])).unwrap();
    assert!((seg.length() - 30.0).abs() < 1e-9);
    let (p, t) = seg.sample_at_distance(15.0);
    assert!((p[0] - 15.0).abs() < 1e-6);
    assert!((p[1]).abs() < 1e-9 && (p[2]).abs() < 1e-9);
    assert!((t[0] - 1.0).abs() < 1e-6 && t[1].abs() < 1e-9 && t[2].abs() < 1e-9);

    // Quarter-circle approximated by a cubic (k = 4/3 * (sqrt(2)-1) * R):
    // LUT chord length lands within 1% of the analytic arc length and the
    // midpoint sample sits on the 45-degree ray with a unit tangent.
    let r = 20.0;
    let k = 4.0 / 3.0 * (2.0f64.sqrt() - 1.0) * r;
    let arc =
        CubicBezierSegment::new([[r, 0.0, 0.0], [r, k, 0.0], [k, r, 0.0], [0.0, r, 0.0]]).unwrap();
    let analytic = std::f64::consts::FRAC_PI_2 * r;
    assert!((arc.length() - analytic).abs() / analytic < 0.01);
    let (p, t) = arc.sample_at_distance(arc.length() / 2.0);
    let mid = r * std::f64::consts::FRAC_1_SQRT_2;
    assert!((p[0] - mid).abs() < 0.2, "x={} expected ~{mid}", p[0]);
    assert!((p[1] - mid).abs() < 0.2, "y={} expected ~{mid}", p[1]);
    let tn = (t[0] * t[0] + t[1] * t[1] + t[2] * t[2]).sqrt();
    assert!((tn - 1.0).abs() < 1e-9, "tangent not normalized: {t:?}");
    assert!(t[0] < -0.6 && t[1] > 0.6, "tangent direction wrong: {t:?}");

    // A 3D ramp rises steadily in +y; every sampled position is monotonic
    // in y and every tangent stays unit-length.
    let ramp = CubicBezierSegment::new([
        [0.0, 0.0, 0.0],
        [10.0, 2.0, 4.0],
        [20.0, 8.0, 8.0],
        [30.0, 10.0, 12.0],
    ])
    .unwrap();
    let mut last_y = f64::MIN;
    for i in 0..=10 {
        let (p, t) = ramp.sample_at_distance(ramp.length() * i as f64 / 10.0);
        assert!(p[1] >= last_y - 1e-9, "ramp y not monotonic at i={i}");
        last_y = p[1];
        let tn = (t[0] * t[0] + t[1] * t[1] + t[2] * t[2]).sqrt();
        assert!((tn - 1.0).abs() < 1e-9);
    }
    let (_, end_t) = ramp.sample_at_distance(ramp.length());
    assert!(end_t[1] > 0.0, "end tangent should still climb: {end_t:?}");
}

// ---------------------------------------------------------------------------
// Track-gauge compatibility & junction sharing
// ---------------------------------------------------------------------------

#[test]
fn track_gauge_compatibility_blocks_wrong_gauge_and_shares_junctions() {
    assert!(TrackGauge::Standard.is_compatible_with(&TrackGauge::Standard));
    assert!(TrackGauge::Narrow.is_compatible_with(&TrackGauge::Narrow));
    assert!(!TrackGauge::Wide.is_compatible_with(&TrackGauge::Narrow));
    assert!(!TrackGauge::Monorail.is_compatible_with(&TrackGauge::Standard));
    // Generic addon gauges: equality-compatible, no names hardcoded.
    let snr600 = TrackGauge::Custom("example_addon:600mm".to_string());
    let snr1067 = TrackGauge::Custom("example_addon:1067mm".to_string());
    assert!(snr600.is_compatible_with(&snr600));
    assert!(!snr600.is_compatible_with(&snr1067));
    assert!(!snr600.is_compatible_with(&TrackGauge::Narrow));

    let mut net = RailNetwork::new();
    let a = net.add_node(dim(), [0.0, 0.0, 0.0]);
    let j = net.add_node(dim(), [50.0, 0.0, 0.0]);
    let n_end = net.add_node(dim(), [100.0, 0.0, 0.0]);
    let w_end = net.add_node(dim(), [50.0, 0.0, 40.0]);
    let m_end = net.add_node(dim(), [50.0, 0.0, -40.0]);
    let e_in = net
        .add_edge(a, j, TrackGauge::Narrow, seg([0.0, 0.0, 0.0], [50.0, 0.0, 0.0]))
        .unwrap();
    let e_narrow = net
        .add_edge(
            j,
            n_end,
            TrackGauge::Narrow,
            seg([50.0, 0.0, 0.0], [100.0, 0.0, 0.0]),
        )
        .unwrap();
    let e_wide = net
        .add_edge(
            j,
            w_end,
            TrackGauge::Wide,
            seg([50.0, 0.0, 0.0], [50.0, 0.0, 40.0]),
        )
        .unwrap();
    let e_mono = net
        .add_edge(
            j,
            m_end,
            TrackGauge::Monorail,
            seg([50.0, 0.0, 0.0], [50.0, 0.0, -40.0]),
        )
        .unwrap();

    // Each gauge picks its own edge out of the shared junction.
    assert_eq!(
        net.graph.next_edge_for(j, &TrackGauge::Narrow),
        Some(e_narrow)
    );
    assert_eq!(net.graph.next_edge_for(j, &TrackGauge::Wide), Some(e_wide));
    assert_eq!(net.graph.next_edge_for(j, &TrackGauge::Monorail), Some(e_mono));
    assert_eq!(net.graph.next_edge_for(j, &snr600), None);

    // A wide-gauge bogey physically cannot be placed on a narrow edge.
    let wide_train = free_run_train(7, TrackGauge::Wide);
    assert!(matches!(
        net.add_train(wide_train, e_in, 0.0),
        Err(RailError::IncompatibleGauge { .. })
    ));

    // find_route refuses to route a narrow train onto a wide target edge.
    assert!(net.find_route(j, e_wide, &TrackGauge::Narrow).is_none());
    assert_eq!(
        net.find_route(j, e_mono, &TrackGauge::Monorail),
        Some(vec![e_mono])
    );

    // A locked switch is authoritative: narrow stock has no traversable
    // exit while the junction is set to the wide route.
    net.graph.set_switch(j, e_wide).unwrap();
    assert_eq!(net.graph.next_edge_for(j, &TrackGauge::Wide), Some(e_wide));
    assert_eq!(net.graph.next_edge_for(j, &TrackGauge::Narrow), None);
    assert!(net.graph.traversable_from(j, &TrackGauge::Narrow).is_empty());
    net.graph.clear_switch(j);
    assert_eq!(
        net.graph.next_edge_for(j, &TrackGauge::Narrow),
        Some(e_narrow)
    );

    // Switch validation rejects non-outgoing edges.
    assert!(matches!(
        net.graph.set_switch(j, e_in),
        Err(RailError::InvalidSwitch { .. })
    ));
}

// ---------------------------------------------------------------------------
// Block signals: a follower stops behind an occupied section
// ---------------------------------------------------------------------------

#[test]
fn block_signal_holds_follower_behind_occupied_section() {
    let mut net = RailNetwork::new();
    let n0 = net.add_node(dim(), [0.0, 0.0, 0.0]);
    let n1 = net.add_node(dim(), [50.0, 0.0, 0.0]);
    let n2 = net.add_node(dim(), [100.0, 0.0, 0.0]);
    let n3 = net.add_node(dim(), [150.0, 0.0, 0.0]);
    let e0 = net
        .add_edge(n0, n1, TrackGauge::Standard, seg([0.0, 0.0, 0.0], [50.0, 0.0, 0.0]))
        .unwrap();
    let e1 = net
        .add_edge(n1, n2, TrackGauge::Standard, seg([50.0, 0.0, 0.0], [100.0, 0.0, 0.0]))
        .unwrap();
    let e2 = net
        .add_edge(
            n2,
            n3,
            TrackGauge::Standard,
            seg([100.0, 0.0, 0.0], [150.0, 0.0, 0.0]),
        )
        .unwrap();
    net.add_block(&[e0]).unwrap();
    let b1 = net.add_block(&[e1]).unwrap();
    net.add_block(&[e2]).unwrap();
    net.add_signal(e1, SignalKind::Block).unwrap();
    net.add_signal(e2, SignalKind::Block).unwrap();

    // Leader parked mid-section; follower free-runs up behind it.
    let leader = free_run_train(1, TrackGauge::Standard);
    net.add_train(leader, e1, 20.0).unwrap();
    net.trains.get_mut(&TrainId(1)).unwrap().state = TrainRunState::Terminus;
    let follower = free_run_train(2, TrackGauge::Standard);
    net.add_train(follower, e0, 0.0).unwrap();

    let loaded = empty_loaded();
    let e0_len = edge_len(&net, e0);
    for _ in 0..200 {
        tick_rail_network(&mut net, &loaded);
        let f = &net.trains[&TrainId(2)];
        assert_eq!(
            f.pos_edge, e0,
            "follower entered occupied section at s={}",
            f.pos_s
        );
        assert_eq!(net.blocks[&b1].occupied_by, Some(TrainId(1)));
        if f.speed == 0.0 && f.pos_s == e0_len {
            break;
        }
    }
    let f = &net.trains[&TrainId(2)];
    assert_eq!(f.pos_s, e0_len, "follower must stop exactly at the boundary");
    assert_eq!(f.speed, 0.0);
    assert_eq!(net.signal_aspect_for(e1, TrainId(2)), SignalAspect::Red);

    // Once the section clears, the signal turns green for the follower and
    // it reserves, crosses, and runs to the end of the line.
    net.remove_train(TrainId(1));
    let mut crossed = false;
    for _ in 0..400 {
        tick_rail_network(&mut net, &loaded);
        if net.trains[&TrainId(2)].pos_edge == e1 {
            crossed = true;
        }
        if net.trains[&TrainId(2)].state == TrainRunState::Terminus {
            break;
        }
    }
    assert!(crossed, "follower never entered the cleared section");
    assert_eq!(net.trains[&TrainId(2)].state, TrainRunState::Terminus);
    assert_eq!(net.trains[&TrainId(2)].pos_edge, e2);
}

// ---------------------------------------------------------------------------
// Chain signals: hold before the junction until the exit block clears
// ---------------------------------------------------------------------------

#[test]
fn chain_signal_holds_train_before_junction_until_exit_clears() {
    let mut net = RailNetwork::new();
    let a = net.add_node(dim(), [0.0, 0.0, 0.0]);
    let b = net.add_node(dim(), [0.0, 0.0, -50.0]);
    let j = net.add_node(dim(), [50.0, 0.0, 0.0]);
    let k = net.add_node(dim(), [100.0, 0.0, 0.0]);
    let x = net.add_node(dim(), [150.0, 0.0, 0.0]);
    let y = net.add_node(dim(), [200.0, 0.0, 0.0]);
    let l = net.add_node(dim(), [50.0, 0.0, 60.0]);
    let m = net.add_node(dim(), [50.0, 0.0, 110.0]);

    let e0 = net
        .add_edge(a, j, TrackGauge::Standard, seg([0.0, 0.0, 0.0], [50.0, 0.0, 0.0]))
        .unwrap();
    // Junction crossing, chain-guarded.
    let e1 = net
        .add_edge(j, k, TrackGauge::Standard, seg([50.0, 0.0, 0.0], [100.0, 0.0, 0.0]))
        .unwrap();
    // Exit block beyond the junction.
    let e2 = net
        .add_edge(
            k,
            x,
            TrackGauge::Standard,
            seg([100.0, 0.0, 0.0], [150.0, 0.0, 0.0]),
        )
        .unwrap();
    // Free crossing route through the same junction.
    let e4 = net
        .add_edge(j, l, TrackGauge::Standard, seg([50.0, 0.0, 0.0], [50.0, 0.0, 60.0]))
        .unwrap();
    let e5 = net
        .add_edge(
            l,
            m,
            TrackGauge::Standard,
            seg([50.0, 0.0, 60.0], [50.0, 0.0, 110.0]),
        )
        .unwrap();
    let e6 = net
        .add_edge(
            x,
            y,
            TrackGauge::Standard,
            seg([150.0, 0.0, 0.0], [200.0, 0.0, 0.0]),
        )
        .unwrap();
    let e7 = net
        .add_edge(
            b,
            j,
            TrackGauge::Standard,
            seg([0.0, 0.0, -50.0], [50.0, 0.0, 0.0]),
        )
        .unwrap();

    net.add_block(&[e0]).unwrap();
    let bj = net.add_block(&[e1]).unwrap();
    let bx = net.add_block(&[e2]).unwrap();
    net.add_block(&[e4]).unwrap();
    net.add_block(&[e5]).unwrap();
    net.add_block(&[e6]).unwrap();
    net.add_block(&[e7]).unwrap();
    net.add_signal(e1, SignalKind::Chain).unwrap();
    net.add_signal(e2, SignalKind::Block).unwrap();

    net.add_station("hold", e2, 20.0).unwrap();
    net.add_station("far", e6, 25.0).unwrap();
    net.add_station("lm", e5, 25.0).unwrap();

    // Leader dwells at "hold" inside the exit block, then moves on to "far".
    let leader = ArticulatedTrain::new(
        TrainId(1),
        vec![carriage(4.0, TrackGauge::Standard)],
        4.0,
        0.5,
        1.0,
        TrainSchedule::point_to_point(vec![
            StationInstruction {
                station_name: "hold".into(),
                wait_ticks: 8,
            },
            StationInstruction {
                station_name: "far".into(),
                wait_ticks: 0,
            },
        ]),
    )
    .unwrap();
    net.add_train(leader, e2, 0.0).unwrap();

    // T2 free-runs toward the junction; the chain signal must hold it.
    net.add_train(free_run_train(2, TrackGauge::Standard), e0, 0.0)
        .unwrap();
    // T3 crosses the same junction via the free e4 route to prove the
    // junction is never blocked (zero deadlock).
    let t3 = ArticulatedTrain::new(
        TrainId(3),
        vec![carriage(4.0, TrackGauge::Standard)],
        4.0,
        0.5,
        1.0,
        TrainSchedule::point_to_point(vec![StationInstruction {
            station_name: "lm".into(),
            wait_ticks: 0,
        }]),
    )
    .unwrap();
    net.add_train(t3, e7, 0.0).unwrap();

    let loaded = empty_loaded();
    let e0_len = edge_len(&net, e0);

    // Phase 1: while the exit block is occupied, T2 is held at the end of
    // e0 — never inside the junction block — while T3 flows through.
    let mut t3_made_progress = false;
    for _ in 0..30 {
        tick_rail_network(&mut net, &loaded);
        if net.blocks[&bx].occupied_by == Some(TrainId(1)) {
            let t2 = &net.trains[&TrainId(2)];
            assert_eq!(
                t2.pos_edge, e0,
                "chain-held train entered the junction at s={}",
                t2.pos_s
            );
            assert_eq!(
                net.blocks[&bj].occupied_by, None,
                "junction block must stay free"
            );
            assert_eq!(net.signal_aspect_for(e1, TrainId(2)), SignalAspect::Red);
            if t2.pos_s == e0_len {
                assert_eq!(t2.speed, 0.0);
            }
        }
        let t3 = &net.trains[&TrainId(3)];
        if t3.pos_edge != e7 || t3.pos_s > 0.0 {
            t3_made_progress = true;
        }
    }
    assert!(t3_made_progress, "crossing train was deadlocked");
    assert_ne!(
        net.trains[&TrainId(3)].pos_edge, e7,
        "T3 should have crossed the junction while T2 was held"
    );

    // Phase 2: once the leader vacates the exit block, the chain signal
    // clears and T2 traverses the junction.
    let mut entered = false;
    for _ in 0..600 {
        tick_rail_network(&mut net, &loaded);
        let t2e = net.trains[&TrainId(2)].pos_edge;
        if t2e == e1 || t2e == e2 {
            entered = true;
            break;
        }
    }
    assert!(
        entered,
        "chain signal never released T2 after the exit cleared"
    );
}

// ---------------------------------------------------------------------------
// Articulated multi-carriage train: virtual transit, station stops, bogeys
// ---------------------------------------------------------------------------

#[test]
fn articulated_train_virtual_transit_station_stop_and_bogey_poses() {
    let mut net = RailNetwork::new();
    let s0 = net.add_node(dim(), [0.0, 0.0, 64.0]);
    let s1 = net.add_node(dim(), [64.0, 0.0, 64.0]);
    let s2 = net.add_node(dim(), [160.0, 0.0, 64.0]);
    let s3 = net.add_node(dim(), [224.0, 0.0, 64.0]);
    let e0 = net
        .add_edge(s0, s1, TrackGauge::Standard, seg([0.0, 0.0, 64.0], [64.0, 0.0, 64.0]))
        .unwrap();
    let e1 = net
        .add_edge(
            s1,
            s2,
            TrackGauge::Standard,
            seg([64.0, 0.0, 64.0], [160.0, 0.0, 64.0]),
        )
        .unwrap();
    let e2 = net
        .add_edge(
            s2,
            s3,
            TrackGauge::Standard,
            seg([160.0, 0.0, 64.0], [224.0, 0.0, 64.0]),
        )
        .unwrap();
    net.add_station("mid", e1, 48.0).unwrap(); // world x=112 -> chunk (7,4)
    net.add_station("terminus", e2, 40.0).unwrap(); // x=200 -> chunk (12,4)

    let train = ArticulatedTrain::new(
        TrainId(10),
        vec![
            carriage(8.0, TrackGauge::Standard),
            carriage(8.0, TrackGauge::Standard),
            carriage(8.0, TrackGauge::Standard),
        ],
        4.0,
        0.5,
        1.0,
        TrainSchedule::point_to_point(vec![
            StationInstruction {
                station_name: "mid".into(),
                wait_ticks: 5,
            },
            StationInstruction {
                station_name: "terminus".into(),
                wait_ticks: 0,
            },
        ]),
    )
    .unwrap();
    net.add_train(train, e0, 0.0).unwrap();
    assert_eq!(net.trains[&TrainId(10)].total_length(), 24.0);

    // Only the destination chunk is loaded: the whole run is virtual until
    // the head reaches chunk (12, 4).
    let loaded = BTreeSet::from([ChunkKey::new(dim(), 12, 4)]);

    let mut events = Vec::new();
    let mut arrived_tick = None;
    let mut departed_tick = None;
    let mut saw_virtual_run = false;
    let mut held_pos = None;
    for i in 0..2000 {
        let evs = tick_rail_network(&mut net, &loaded);
        let t = &net.trains[&TrainId(10)];
        if t.render_mode == TrainRenderMode::Virtual && t.speed > 0.0 {
            saw_virtual_run = true;
        }
        if matches!(t.state, TrainRunState::WaitingAtStation { .. }) {
            held_pos = Some((t.pos_edge, t.pos_s));
        } else if let Some((pe, ps)) = held_pos.take() {
            assert_eq!(
                (pe, ps),
                (t.pos_edge, t.pos_s),
                "train moved while waiting at the station"
            );
        }
        for ev in evs {
            match &ev {
                TransitEvent::ArrivedAtStation { station, .. } if station == "mid" => {
                    assert!(arrived_tick.is_none());
                    arrived_tick = Some(i);
                }
                TransitEvent::DepartedStation { station, .. } if station == "mid" => {
                    departed_tick = Some(i);
                }
                _ => {}
            }
            events.push(ev);
        }
        if t.state == TrainRunState::Terminus {
            break;
        }
    }

    assert!(saw_virtual_run, "train never ran in Virtual mode");
    let arrived = arrived_tick.expect("never arrived at mid");
    let departed = departed_tick.expect("never departed mid");
    // Arrival tick T sets the dwell; the train waits `wait_ticks` full
    // ticks and departs on T + wait_ticks + 1.
    assert_eq!(departed - arrived, 6);

    let t = &net.trains[&TrainId(10)];
    assert_eq!(t.state, TrainRunState::Terminus);
    assert_eq!(t.pos_edge, e2);
    assert_eq!(t.pos_s, 40.0);
    assert_eq!(t.render_mode, TrainRenderMode::Loaded);

    // Render-mode transition was reported exactly once: Virtual -> Loaded.
    let mode_events: Vec<TrainRenderMode> = events
        .iter()
        .filter_map(|e| match e {
            TransitEvent::RenderModeChanged { mode, .. } => Some(*mode),
            _ => None,
        })
        .collect();
    assert_eq!(mode_events, vec![TrainRenderMode::Loaded]);

    // Exact bogey poses along a multi-edge path: rewind the head to e1 s=5
    // with e0 in history so the rearmost bogey reaches back across the
    // boundary.
    {
        let t = net.trains.get_mut(&TrainId(10)).unwrap();
        t.pos_edge = e1;
        t.pos_s = 5.0;
        t.state = TrainRunState::Running;
        t.history.clear();
        t.history.push_back(e0);
    }
    let t = &net.trains[&TrainId(10)];
    // Head sits at world x = 64 + 5 = 69 on a straight +x spline.
    let expected_x = |depth: f64| 69.0 - depth;
    for (car, end, depth) in [
        (0usize, BogeyEnd::Leading, 1.5f64),
        (0, BogeyEnd::Trailing, 8.0 - 1.5),
        (1, BogeyEnd::Leading, 8.0 + 1.5),
        (1, BogeyEnd::Trailing, 16.0 - 1.5),
        (2, BogeyEnd::Leading, 16.0 + 1.5),
        // Trailing bogey of car 2 (depth 22.5) lies back on e0 — a
        // multi-edge pose resolved through the history queue.
        (2, BogeyEnd::Trailing, 24.0 - 1.5),
    ] {
        let (pos, tan) = t.bogey_world_pose(&net, car, end).expect("bogey pose missing");
        assert!(
            (pos[0] - expected_x(depth)).abs() < 1e-6,
            "car {car} {end:?} x={} expected {}",
            pos[0],
            expected_x(depth)
        );
        assert!(pos[1].abs() < 1e-9 && (pos[2] - 64.0).abs() < 1e-6);
        assert!((tan[0] - 1.0).abs() < 1e-6, "bogey tangent {tan:?}");
    }
}

// ---------------------------------------------------------------------------
// Inter-dimensional portal track link
// ---------------------------------------------------------------------------

#[test]
fn portal_track_link_carries_trains_across_dimensions() {
    let limbo = DimId::new("example_addon:limbo");
    let mut net = RailNetwork::new();
    let n0 = net.add_node(dim(), [0.0, 64.0, 0.0]);
    let n1 = net.add_node(limbo.clone(), [0.0, 200.0, 0.0]);
    let n2 = net.add_node(limbo.clone(), [50.0, 200.0, 0.0]);
    let e0 = net
        .add_edge(
            n0,
            n1,
            TrackGauge::Standard,
            [
                [0.0, 64.0, 0.0],
                [0.0, 110.0, 0.0],
                [0.0, 155.0, 0.0],
                [0.0, 200.0, 0.0],
            ],
        )
        .unwrap();
    let e1 = net
        .add_edge(
            n1,
            n2,
            TrackGauge::Standard,
            seg([0.0, 200.0, 0.0], [50.0, 200.0, 0.0]),
        )
        .unwrap();
    assert!(net.graph.edges[&e0].is_portal(&net.graph));
    assert!(!net.graph.edges[&e1].is_portal(&net.graph));

    net.add_train(free_run_train(5, TrackGauge::Standard), e0, 0.0)
        .unwrap();
    let loaded = empty_loaded();
    let mut saw_overworld = false;
    let mut saw_limbo = false;
    for _ in 0..400 {
        tick_rail_network(&mut net, &loaded);
        let t = &net.trains[&TrainId(5)];
        if t.current_dim == dim() {
            saw_overworld = true;
        }
        if t.current_dim == limbo {
            saw_limbo = true;
        }
        if t.state == TrainRunState::Terminus {
            break;
        }
    }
    let t = &net.trains[&TrainId(5)];
    assert!(saw_overworld && saw_limbo, "train never crossed dimensions");
    assert_eq!(t.current_dim, limbo);
    assert_eq!(t.pos_edge, e1);
}

// ---------------------------------------------------------------------------
// RAIL snapshot roundtrip & fail-closed decode
// ---------------------------------------------------------------------------

fn snapshot_fixture() -> RailNetwork {
    let mut net = RailNetwork::new();
    let custom = TrackGauge::Custom("example_addon:600mm".to_string());
    let n0 = net.add_node(dim(), [0.0, 0.0, 0.0]);
    let n1 = net.add_node(DimId::new("example_addon:limbo"), [40.0, 0.0, 0.0]);
    let n2 = net.add_node(DimId::new("example_addon:limbo"), [90.0, 8.0, 0.0]);
    let e0 = net
        .add_edge(
            n0,
            n1,
            custom.clone(),
            [
                [0.0, 0.0, 0.0],
                [15.0, 0.0, 3.0],
                [28.0, 0.0, -3.0],
                [40.0, 0.0, 0.0],
            ],
        )
        .unwrap();
    let e1 = net
        .add_edge(
            n1,
            n2,
            custom.clone(),
            [
                [40.0, 0.0, 0.0],
                [55.0, 3.0, 0.0],
                [75.0, 6.0, 0.0],
                [90.0, 8.0, 0.0],
            ],
        )
        .unwrap();
    net.add_block(&[e0]).unwrap();
    net.add_block(&[e1]).unwrap();
    net.add_signal(e0, SignalKind::Block).unwrap();
    net.add_signal(e1, SignalKind::Chain).unwrap();
    net.add_station("limbo_halt", e1, 25.0).unwrap();
    net.graph.set_switch(n1, e1).unwrap();

    let train = ArticulatedTrain::new(
        TrainId(42),
        vec![carriage(6.0, custom.clone()), carriage(6.0, custom)],
        3.0,
        0.4,
        0.8,
        TrainSchedule::looping(vec![StationInstruction {
            station_name: "limbo_halt".into(),
            wait_ticks: 7,
        }]),
    )
    .unwrap();
    net.add_train(train, e0, 12.5).unwrap();
    {
        // Mid-run state: moving with an active route/target reservation.
        let t = net.trains.get_mut(&TrainId(42)).unwrap();
        t.speed = 1.25;
        t.target = Some(StationTarget {
            station_name: "limbo_halt".into(),
            edge: e1,
            s: 25.0,
            wait_ticks: 7,
        });
        t.route.push_back(e1);
    }
    net.tick = 1234;
    net
}

#[test]
fn rail_snapshot_roundtrip_is_bit_exact() {
    let net = snapshot_fixture();
    let bytes = encode_snapshot(&net);
    assert_eq!(&bytes[..4], &RAIL_MAGIC);
    assert_eq!(bytes[4], RAIL_VERSION);
    let decoded = decode_snapshot(&bytes).unwrap();
    assert_eq!(decoded, net);
    // Re-encoding the decoded network reproduces the exact same bytes.
    assert_eq!(encode_snapshot(&decoded), bytes);

    // The canonical CRC32 check vector (zlib "123456789" -> 0xCBF43926).
    assert_eq!(crc32_ieee(b"123456789"), 0xCBF4_3926);
    assert_eq!(crc32_ieee(b""), 0);
}

#[test]
fn rail_snapshot_fails_closed_on_corruption() {
    let bytes = encode_snapshot(&snapshot_fixture());

    // Bad magic / version / truncation.
    assert!(decode_snapshot(&[]).is_err());
    let mut bad = bytes.clone();
    bad[0] = b'X';
    assert!(decode_snapshot(&bad).is_err());
    let mut bad = bytes.clone();
    bad[4] = 0x7F;
    assert!(decode_snapshot(&bad).is_err());
    assert!(decode_snapshot(&bytes[..bytes.len() - 1]).is_err());
    assert!(decode_snapshot(&bytes[..8]).is_err());

    // A single bit flip anywhere in the payload breaks the CRC32.
    for off in [13, bytes.len() / 2, bytes.len() - 1] {
        let mut bad = bytes.clone();
        bad[off] ^= 0x01;
        assert!(
            decode_snapshot(&bad).is_err(),
            "bit flip at {off} was not detected"
        );
    }

    // A forged payload with an oversized node count still fails closed
    // even under a valid CRC32 (defense in depth behind the checksum).
    let mut payload = Vec::new();
    payload.extend_from_slice(&0u64.to_be_bytes()); // tick
    payload.extend_from_slice(&0u64.to_be_bytes()); // next_node
    payload.extend_from_slice(&0u64.to_be_bytes()); // next_edge
    payload.extend_from_slice(&0u64.to_be_bytes()); // next_block
    payload.extend_from_slice(&[0xFF, 0xFF, 0xFF, 0xFF, 0x0F]); // varint u32::MAX
    let mut forged = Vec::new();
    forged.extend_from_slice(&RAIL_MAGIC);
    forged.push(RAIL_VERSION);
    forged.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    forged.extend_from_slice(&crc32_ieee(&payload).to_be_bytes());
    forged.extend_from_slice(&payload);
    assert!(decode_snapshot(&forged).is_err());
}
