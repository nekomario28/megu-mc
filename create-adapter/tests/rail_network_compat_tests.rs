// SPDX-License-Identifier: LGPL-3.0-or-later

//! Compatibility hardening suite for Create 6.0.10-style train networks:
//! shortest-path routing over spline cost, directed-loop station schedules,
//! recursive chain-signal lookahead, broken-track re-planning, opposing
//! single-track exclusion, hostile runtime values, fail-closed snapshots,
//! deterministic replay, and production-engine consumption.

use std::collections::BTreeSet;

use create_adapter::offload_bridge::OffloadBudget;
use create_adapter::rail_network::*;
use create_adapter::CreateAdapterEngine;

const OW: &str = "minecraft:overworld";

fn dim() -> DimId {
    DimId::new(OW)
}

fn empty_loaded() -> BTreeSet<ChunkKey> {
    BTreeSet::new()
}

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

fn carriage() -> CarriageSpec {
    CarriageSpec {
        length: 4.0,
        leading_bogey_offset: 1.0,
        trailing_bogey_offset: 1.0,
        gauge: TrackGauge::Standard,
    }
}

fn free_train(id: u64) -> ArticulatedTrain {
    ArticulatedTrain::new(
        TrainId(id),
        vec![carriage()],
        4.0,
        0.5,
        1.0,
        TrainSchedule::default(),
    )
    .unwrap()
}

fn scheduled_train(id: u64, station: &str) -> ArticulatedTrain {
    ArticulatedTrain::new(
        TrainId(id),
        vec![carriage()],
        4.0,
        0.5,
        1.0,
        TrainSchedule::point_to_point(vec![StationInstruction {
            station_name: station.into(),
            wait_ticks: 0,
        }]),
    )
    .unwrap()
}

fn line_network(length: f64) -> (RailNetwork, NodeId, NodeId, EdgeId) {
    let mut net = RailNetwork::new();
    let a = net.add_node(dim(), [0.0, 0.0, 0.0]);
    let b = net.add_node(dim(), [length, 0.0, 0.0]);
    let e = net
        .add_edge(
            a,
            b,
            TrackGauge::Standard,
            seg([0.0, 0.0, 0.0], [length, 0.0, 0.0]),
        )
        .unwrap();
    (net, a, b, e)
}

#[test]
fn dijkstra_prefers_shortest_spline_distance_not_fewest_edges() {
    let mut net = RailNetwork::new();
    let start = net.add_node(dim(), [0.0, 0.0, 0.0]);
    let mid = net.add_node(dim(), [30.0, 0.0, 0.0]);
    let goal = net.add_node(dim(), [60.0, 0.0, 0.0]);
    let end = net.add_node(dim(), [80.0, 0.0, 0.0]);

    // Insert the one-edge but very long route first. BFS would choose it;
    // the routing contract is cost over the actual curved/straight splines.
    let long_direct = net
        .add_edge(
            start,
            goal,
            TrackGauge::Standard,
            seg([0.0, 0.0, 0.0], [60.0, 200.0, 0.0]),
        )
        .unwrap();
    assert!(net.graph.edges[&long_direct].length() > 100.0);
    let detour_a = net
        .add_edge(
            start,
            mid,
            TrackGauge::Standard,
            seg([0.0, 0.0, 0.0], [30.0, 0.0, 0.0]),
        )
        .unwrap();
    let detour_b = net
        .add_segmented_edge(
            mid,
            goal,
            TrackGauge::Standard,
            vec![
                seg([30.0, 0.0, 0.0], [45.0, 3.0, 0.0]),
                seg([45.0, 3.0, 0.0], [60.0, 0.0, 0.0]),
            ],
        )
        .unwrap();
    let target = net
        .add_edge(
            goal,
            end,
            TrackGauge::Standard,
            seg([60.0, 0.0, 0.0], [80.0, 0.0, 0.0]),
        )
        .unwrap();

    let route = net
        .find_route(start, target, &TrackGauge::Standard)
        .expect("detour route exists");
    assert_eq!(route, vec![detour_a, detour_b, target]);
    assert_ne!(route.first(), Some(&long_direct));
    for _ in 0..8 {
        assert_eq!(
            net.find_route(start, target, &TrackGauge::Standard),
            Some(route.clone()),
            "equal inputs must reproduce the same deterministic route"
        );
    }
}

#[test]
fn station_behind_on_same_directed_edge_is_reached_around_the_loop() {
    let mut net = RailNetwork::new();
    let n0 = net.add_node(dim(), [0.0, 0.0, 0.0]);
    let n1 = net.add_node(dim(), [50.0, 0.0, 0.0]);
    let n2 = net.add_node(dim(), [50.0, 0.0, 50.0]);
    let e0 = net
        .add_edge(
            n0,
            n1,
            TrackGauge::Standard,
            seg([0.0, 0.0, 0.0], [50.0, 0.0, 0.0]),
        )
        .unwrap();
    let e1 = net
        .add_edge(
            n1,
            n2,
            TrackGauge::Standard,
            seg([50.0, 0.0, 0.0], [50.0, 0.0, 50.0]),
        )
        .unwrap();
    let e2 = net
        .add_edge(
            n2,
            n0,
            TrackGauge::Standard,
            seg([50.0, 0.0, 50.0], [0.0, 0.0, 0.0]),
        )
        .unwrap();
    net.add_station("loop_halt", e0, 20.0).unwrap();
    net.add_train(scheduled_train(7, "loop_halt"), e0, 30.0)
        .unwrap();

    let loaded = empty_loaded();
    tick_rail_network(&mut net, &loaded);
    assert_eq!(
        net.trains[&TrainId(7)]
            .route
            .iter()
            .copied()
            .collect::<Vec<_>>(),
        vec![e1, e2, e0],
        "the planner must continue around the directed loop instead of declaring NoRoute"
    );

    let mut arrived = false;
    for _ in 0..1000 {
        let events = tick_rail_network(&mut net, &loaded);
        arrived |= events.iter().any(|event| {
            matches!(event, TransitEvent::ArrivedAtStation { train, station } if *train == TrainId(7) && station == "loop_halt")
        });
        if arrived {
            break;
        }
    }
    assert!(arrived, "train never reached the behind-spawn loop station");
    assert_eq!(net.trains[&TrainId(7)].pos_edge, e0);
    assert_eq!(net.trains[&TrainId(7)].pos_s, 20.0);
}

#[test]
fn broken_intermediate_track_reroutes_to_detour_and_still_arrives() {
    let mut net = RailNetwork::new();
    let n0 = net.add_node(dim(), [0.0, 0.0, 0.0]);
    let n1 = net.add_node(dim(), [30.0, 0.0, 0.0]);
    let n2 = net.add_node(dim(), [50.0, 0.0, 0.0]);
    let n3 = net.add_node(dim(), [30.0, 0.0, 60.0]);
    let n4 = net.add_node(dim(), [70.0, 0.0, 0.0]);
    let n5 = net.add_node(dim(), [100.0, 0.0, 0.0]);
    let e0 = net
        .add_edge(
            n0,
            n1,
            TrackGauge::Standard,
            seg([0.0, 0.0, 0.0], [30.0, 0.0, 0.0]),
        )
        .unwrap();
    let primary_a = net
        .add_edge(
            n1,
            n2,
            TrackGauge::Standard,
            seg([30.0, 0.0, 0.0], [50.0, 0.0, 0.0]),
        )
        .unwrap();
    let primary_b = net
        .add_edge(
            n2,
            n4,
            TrackGauge::Standard,
            seg([50.0, 0.0, 0.0], [70.0, 0.0, 0.0]),
        )
        .unwrap();
    let detour_a = net
        .add_edge(
            n1,
            n3,
            TrackGauge::Standard,
            seg([30.0, 0.0, 0.0], [30.0, 0.0, 60.0]),
        )
        .unwrap();
    let detour_b = net
        .add_edge(
            n3,
            n4,
            TrackGauge::Standard,
            seg([30.0, 0.0, 60.0], [70.0, 0.0, 0.0]),
        )
        .unwrap();
    let target = net
        .add_edge(
            n4,
            n5,
            TrackGauge::Standard,
            seg([70.0, 0.0, 0.0], [100.0, 0.0, 0.0]),
        )
        .unwrap();
    net.add_station("far", target, 10.0).unwrap();
    net.add_train(scheduled_train(9, "far"), e0, 0.0).unwrap();

    let loaded = empty_loaded();
    tick_rail_network(&mut net, &loaded);
    assert_eq!(
        net.trains[&TrainId(9)]
            .route
            .iter()
            .copied()
            .collect::<Vec<_>>(),
        vec![primary_a, primary_b, target]
    );

    assert!(net.remove_edge(primary_a).is_some());
    tick_rail_network(&mut net, &loaded);
    assert_eq!(
        net.trains[&TrainId(9)]
            .route
            .iter()
            .copied()
            .collect::<Vec<_>>(),
        vec![detour_a, detour_b, target],
        "missing intermediate edge must trigger same-tick replanning"
    );

    let mut arrived = false;
    let mut used_detour = false;
    for _ in 0..2000 {
        let events = tick_rail_network(&mut net, &loaded);
        used_detour |= matches!(
            net.trains[&TrainId(9)].pos_edge,
            edge if edge == detour_a || edge == detour_b
        );
        arrived |= events.iter().any(|event| {
            matches!(event, TransitEvent::ArrivedAtStation { train, station } if *train == TrainId(9) && station == "far")
        });
        if arrived {
            break;
        }
    }
    assert!(arrived, "rerouted train never arrived");
    assert!(used_detour, "rerouted train never traversed the detour");
    let train = &net.trains[&TrainId(9)];
    assert_eq!(train.pos_edge, target);
    assert_eq!(train.pos_s, 10.0);
    assert!(!train.history.contains(&primary_a));
}

#[test]
fn broken_target_track_fails_closed_with_no_route() {
    let mut net = RailNetwork::new();
    let n0 = net.add_node(dim(), [0.0, 0.0, 0.0]);
    let n1 = net.add_node(dim(), [40.0, 0.0, 0.0]);
    let n2 = net.add_node(dim(), [80.0, 0.0, 0.0]);
    let e0 = net
        .add_edge(
            n0,
            n1,
            TrackGauge::Standard,
            seg([0.0, 0.0, 0.0], [40.0, 0.0, 0.0]),
        )
        .unwrap();
    let target = net
        .add_edge(
            n1,
            n2,
            TrackGauge::Standard,
            seg([40.0, 0.0, 0.0], [80.0, 0.0, 0.0]),
        )
        .unwrap();
    net.add_station("gone", target, 10.0).unwrap();
    net.add_train(scheduled_train(10, "gone"), e0, 0.0).unwrap();

    let loaded = empty_loaded();
    tick_rail_network(&mut net, &loaded);
    assert!(net.remove_edge(target).is_some());
    let mut no_route = false;
    for _ in 0..4 {
        let events = tick_rail_network(&mut net, &loaded);
        no_route |= events.iter().any(|event| {
            matches!(event, TransitEvent::NoRoute { train, station } if *train == TrainId(10) && station == "gone")
        });
    }
    assert!(no_route, "broken target edge did not fail closed");
    assert_eq!(net.trains[&TrainId(10)].state, TrainRunState::Terminus);
    assert_eq!(net.trains[&TrainId(10)].pos_edge, e0);
}

#[test]
fn recursive_chain_signals_keep_train_out_of_whole_occupied_relay() {
    let mut net = RailNetwork::new();
    let a = net.add_node(dim(), [0.0, 0.0, 0.0]);
    let j1 = net.add_node(dim(), [40.0, 0.0, 0.0]);
    let j2 = net.add_node(dim(), [80.0, 0.0, 0.0]);
    let j3 = net.add_node(dim(), [120.0, 0.0, 0.0]);
    let end = net.add_node(dim(), [160.0, 0.0, 0.0]);
    let e0 = net
        .add_edge(
            a,
            j1,
            TrackGauge::Standard,
            seg([0.0, 0.0, 0.0], [40.0, 0.0, 0.0]),
        )
        .unwrap();
    let e1 = net
        .add_edge(
            j1,
            j2,
            TrackGauge::Standard,
            seg([40.0, 0.0, 0.0], [80.0, 0.0, 0.0]),
        )
        .unwrap();
    let e2 = net
        .add_edge(
            j2,
            j3,
            TrackGauge::Standard,
            seg([80.0, 0.0, 0.0], [120.0, 0.0, 0.0]),
        )
        .unwrap();
    let e3 = net
        .add_edge(
            j3,
            end,
            TrackGauge::Standard,
            seg([120.0, 0.0, 0.0], [160.0, 0.0, 0.0]),
        )
        .unwrap();
    net.add_block(&[e0]).unwrap();
    net.add_block(&[e1]).unwrap();
    net.add_block(&[e2]).unwrap();
    net.add_block(&[e3]).unwrap();
    net.add_signal(e1, SignalKind::Chain).unwrap();
    net.add_signal(e2, SignalKind::Chain).unwrap();
    net.add_signal(e3, SignalKind::Block).unwrap();

    let mut leader = free_train(1);
    leader.state = TrainRunState::Terminus;
    net.add_train(leader, e3, 5.0).unwrap();
    net.add_train(free_train(2), e0, 0.0).unwrap();

    let loaded = empty_loaded();
    let e0_len = net.graph.edges[&e0].length();
    for _ in 0..200 {
        tick_rail_network(&mut net, &loaded);
        let follower = &net.trains[&TrainId(2)];
        assert_eq!(
            follower.pos_edge, e0,
            "recursive chain lookahead let the follower into the relay"
        );
        assert_eq!(net.signal_aspect_for(e1, TrainId(2)), SignalAspect::Red);
    }
    assert_eq!(net.trains[&TrainId(2)].pos_s, e0_len);

    net.remove_train(TrainId(1));
    let mut entered_relay = false;
    for _ in 0..600 {
        tick_rail_network(&mut net, &loaded);
        if net.trains[&TrainId(2)].pos_edge == e1 || net.trains[&TrainId(2)].pos_edge == e2 {
            entered_relay = true;
            break;
        }
    }
    assert!(
        entered_relay,
        "chain relay never reopened after the exit cleared"
    );
}

#[test]
fn chain_signal_lookahead_is_not_capped_below_network_size() {
    let mut net = RailNetwork::new();
    let mut nodes = Vec::new();
    for i in 0..76 {
        nodes.push(net.add_node(dim(), [i as f64, 0.0, 0.0]));
    }
    let mut edges = Vec::new();
    for i in 0..75 {
        let x = i as f64;
        edges.push(
            net.add_edge(
                nodes[i],
                nodes[i + 1],
                TrackGauge::Standard,
                seg([x, 0.0, 0.0], [x + 1.0, 0.0, 0.0]),
            )
            .unwrap(),
        );
    }
    for edge in &edges {
        net.add_block(&[*edge]).unwrap();
    }
    // Seventy chained relays exceed the old fixed recursion budget; with a
    // clear exit the first signal must still prove green exactly.
    for edge in &edges[1..71] {
        net.add_signal(*edge, SignalKind::Chain).unwrap();
    }
    net.add_signal(edges[71], SignalKind::Block).unwrap();
    net.add_train(free_train(93), edges[0], 0.0).unwrap();
    assert_eq!(
        net.signal_aspect_for(edges[1], TrainId(93)),
        SignalAspect::Green
    );
}

#[test]
fn cyclic_chain_signal_lookup_fails_closed() {
    let mut net = RailNetwork::new();
    let a = net.add_node(dim(), [0.0, 0.0, 0.0]);
    let b = net.add_node(dim(), [10.0, 0.0, 0.0]);
    let forward = net
        .add_edge(
            a,
            b,
            TrackGauge::Standard,
            seg([0.0, 0.0, 0.0], [10.0, 0.0, 0.0]),
        )
        .unwrap();
    let backward = net
        .add_edge(
            b,
            a,
            TrackGauge::Standard,
            seg([10.0, 0.0, 0.0], [0.0, 0.0, 0.0]),
        )
        .unwrap();
    net.add_block(&[forward]).unwrap();
    net.add_block(&[backward]).unwrap();
    net.add_signal(forward, SignalKind::Chain).unwrap();
    net.add_signal(backward, SignalKind::Chain).unwrap();
    net.add_train(free_train(94), forward, 0.0).unwrap();
    assert_eq!(
        net.signal_aspect_for(backward, TrainId(94)),
        SignalAspect::Red
    );
}

#[test]
fn opposing_single_track_is_excluded_by_reservation_and_never_cooccupied() {
    let mut net = RailNetwork::new();
    let left = net.add_node(dim(), [0.0, 0.0, 0.0]);
    let j = net.add_node(dim(), [30.0, 0.0, 0.0]);
    let k = net.add_node(dim(), [80.0, 0.0, 0.0]);
    let right = net.add_node(dim(), [110.0, 0.0, 0.0]);
    let far_left = net.add_node(dim(), [-20.0, 0.0, 0.0]);
    let far_right = net.add_node(dim(), [130.0, 0.0, 0.0]);
    let approach_l = net
        .add_edge(
            left,
            j,
            TrackGauge::Standard,
            seg([0.0, 0.0, 0.0], [30.0, 0.0, 0.0]),
        )
        .unwrap();
    let forward = net
        .add_edge(
            j,
            k,
            TrackGauge::Standard,
            seg([30.0, 0.0, 0.0], [80.0, 0.0, 0.0]),
        )
        .unwrap();
    let backward = net
        .add_edge(
            k,
            j,
            TrackGauge::Standard,
            seg([80.0, 0.0, 0.0], [30.0, 0.0, 0.0]),
        )
        .unwrap();
    let approach_r = net
        .add_edge(
            right,
            k,
            TrackGauge::Standard,
            seg([110.0, 0.0, 0.0], [80.0, 0.0, 0.0]),
        )
        .unwrap();
    let exit_l = net
        .add_edge(
            j,
            far_left,
            TrackGauge::Standard,
            seg([30.0, 0.0, 0.0], [-20.0, 0.0, 0.0]),
        )
        .unwrap();
    let exit_r = net
        .add_edge(
            k,
            far_right,
            TrackGauge::Standard,
            seg([80.0, 0.0, 0.0], [130.0, 0.0, 0.0]),
        )
        .unwrap();
    net.add_block(&[approach_l]).unwrap();
    net.add_block(&[approach_r]).unwrap();
    let single = net.add_block(&[forward, backward]).unwrap();
    net.add_block(&[exit_l]).unwrap();
    net.add_block(&[exit_r]).unwrap();
    net.add_signal(forward, SignalKind::Block).unwrap();
    net.add_signal(backward, SignalKind::Block).unwrap();
    net.add_station("east", exit_r, 20.0).unwrap();
    net.add_station("west", exit_l, 20.0).unwrap();
    net.add_train(scheduled_train(1, "east"), approach_l, 0.0)
        .unwrap();
    net.add_train(scheduled_train(2, "west"), approach_r, 0.0)
        .unwrap();

    let loaded = empty_loaded();
    let mut saw_either_inside = false;
    for _ in 0..1600 {
        tick_rail_network(&mut net, &loaded);
        let one = &net.trains[&TrainId(1)];
        let two = &net.trains[&TrainId(2)];
        let one_inside = one.pos_edge == forward || one.pos_edge == backward;
        let two_inside = two.pos_edge == forward || two.pos_edge == backward;
        saw_either_inside |= one_inside || two_inside;
        assert!(
            !(one_inside && two_inside),
            "opposing trains co-occupied the single-track section"
        );
        assert!(
            net.blocks[&single].occupants.len() <= 1,
            "signal block occupancy leaked multiple trains"
        );
        if one.state == TrainRunState::Terminus && two.state == TrainRunState::Terminus {
            break;
        }
    }
    assert!(
        saw_either_inside,
        "neither train entered the single-track section"
    );
    assert_eq!(net.trains[&TrainId(1)].state, TrainRunState::Terminus);
    assert_eq!(net.trains[&TrainId(2)].state, TrainRunState::Terminus);
}

#[test]
fn duplicate_or_conflicting_spawn_is_rejected() {
    let (mut net, _, _, edge) = line_network(40.0);
    net.add_block(&[edge]).unwrap();
    net.add_train(free_train(3), edge, 0.0).unwrap();
    assert!(matches!(
        net.add_train(free_train(3), edge, 1.0),
        Err(RailError::DuplicateTrain(TrainId(3)))
    ));
    assert!(matches!(
        net.add_train(free_train(4), edge, 2.0),
        Err(RailError::BlockConflict { .. })
    ));
}

#[test]
fn discontinuous_segmented_track_geometry_is_rejected() {
    let mut net = RailNetwork::new();
    let a = net.add_node(dim(), [0.0, 0.0, 0.0]);
    let b = net.add_node(dim(), [10.0, 0.0, 0.0]);
    let result = net.add_segmented_edge(
        a,
        b,
        TrackGauge::Standard,
        vec![
            seg([0.0, 0.0, 0.0], [5.0, 0.0, 0.0]),
            // A spline chain cannot teleport five blocks between segments.
            seg([10.0, 0.0, 0.0], [15.0, 0.0, 0.0]),
        ],
    );
    assert!(result.is_err());
}

#[test]
fn snapshot_no_train_sentinel_cannot_be_a_live_train_id() {
    assert!(ArticulatedTrain::new(
        TrainId(u64::MAX),
        vec![carriage()],
        1.0,
        1.0,
        1.0,
        TrainSchedule::default(),
    )
    .is_err());
}

#[test]
fn nan_and_overflow_inputs_are_clamped_or_rejected() {
    let straight = CubicBezierSegment::new(seg([0.0, 0.0, 0.0], [10.0, 0.0, 0.0])).unwrap();
    for distance in [f64::NAN, f64::NEG_INFINITY, f64::INFINITY] {
        let (pos, tangent) = straight.sample_at_distance(distance);
        assert!(
            pos.iter().all(|v| v.is_finite()),
            "position became non-finite"
        );
        assert!(
            tangent.iter().all(|v| v.is_finite()),
            "tangent became non-finite"
        );
    }
    let huge = CarriageSpec {
        length: f64::MAX,
        leading_bogey_offset: 0.0,
        trailing_bogey_offset: 0.0,
        gauge: TrackGauge::Standard,
    };
    assert!(ArticulatedTrain::new(
        TrainId(90),
        vec![huge.clone(), huge],
        1.0,
        1.0,
        1.0,
        TrainSchedule::default(),
    )
    .is_err());

    let (mut net, _, _, edge) = line_network(50.0);
    net.add_train(free_train(91), edge, 10.0).unwrap();
    {
        let train = net.trains.get_mut(&TrainId(91)).unwrap();
        train.speed = f64::NAN;
        train.pos_s = f64::NAN;
    }
    tick_rail_network(&mut net, &empty_loaded());
    let train = &net.trains[&TrainId(91)];
    assert!(train.pos_s.is_finite() && train.pos_s >= 0.0);
    assert!(train.speed.is_finite() && train.speed >= 0.0);
}

#[test]
fn huge_finite_speed_cannot_spin_forever_at_track_crossings() {
    let mut net = RailNetwork::new();
    let n0 = net.add_node(dim(), [0.0, 0.0, 0.0]);
    let n1 = net.add_node(dim(), [1.0, 0.0, 0.0]);
    let n2 = net.add_node(dim(), [1.0, 0.0, 1.0]);
    let e0 = net
        .add_edge(
            n0,
            n1,
            TrackGauge::Standard,
            seg([0.0, 0.0, 0.0], [1.0, 0.0, 0.0]),
        )
        .unwrap();
    net.add_edge(
        n1,
        n2,
        TrackGauge::Standard,
        seg([1.0, 0.0, 0.0], [1.0, 0.0, 1.0]),
    )
    .unwrap();
    net.add_edge(
        n2,
        n0,
        TrackGauge::Standard,
        seg([1.0, 0.0, 1.0], [0.0, 0.0, 0.0]),
    )
    .unwrap();
    let mut train = free_train(92);
    train.max_speed = f64::MAX;
    train.speed = f64::MAX;
    net.add_train(train, e0, 0.0).unwrap();

    tick_rail_network(&mut net, &empty_loaded());
    let train = &net.trains[&TrainId(92)];
    assert!(train.pos_s.is_finite());
    assert!(train.speed.is_finite());
}

#[test]
fn snapshot_empty_network_roundtrips_and_hostile_runtime_fields_fail_closed() {
    let empty = RailNetwork::new();
    let decoded = decode_snapshot(&encode_snapshot(&empty)).unwrap();
    assert_eq!(decoded, empty);

    let (mut net, _, _, edge) = line_network(40.0);
    net.add_train(free_train(20), edge, 0.0).unwrap();
    net.trains.get_mut(&TrainId(20)).unwrap().speed = 99.0;
    assert!(decode_snapshot(&encode_snapshot(&net)).is_err());

    let (mut net, a, _b, edge) = line_network(40.0);
    let isolated = net.add_node(dim(), [100.0, 0.0, 0.0]);
    let dangling = net
        .add_edge(
            a,
            isolated,
            TrackGauge::Standard,
            seg([0.0, 0.0, 0.0], [100.0, 0.0, 0.0]),
        )
        .unwrap();
    net.add_train(free_train(21), edge, 0.0).unwrap();
    {
        let train = net.trains.get_mut(&TrainId(21)).unwrap();
        train.target = None;
        train.route.push_back(dangling);
    }
    assert!(decode_snapshot(&encode_snapshot(&net)).is_err());
}

#[test]
fn snapshot_rejects_conflicting_block_edges_and_dangling_occupants() {
    let (mut net, _, _, edge) = line_network(40.0);
    let _b1 = net.add_block(&[edge]).unwrap();
    let b2 = net.add_block(&[]).unwrap();
    net.blocks.get_mut(&b2).unwrap().edges.insert(edge);
    assert!(decode_snapshot(&encode_snapshot(&net)).is_err());

    let (mut net, _, _, edge) = line_network(40.0);
    let block = net.add_block(&[edge]).unwrap();
    {
        let b = net.blocks.get_mut(&block).unwrap();
        b.occupants.insert(TrainId(999));
        b.occupied_by = Some(TrainId(999));
    }
    assert!(decode_snapshot(&encode_snapshot(&net)).is_err());

    let (mut net, _, _, edge) = line_network(40.0);
    let block = net.add_block(&[edge]).unwrap();
    net.blocks.get_mut(&block).unwrap().reserved_by = Some(TrainId(999));
    assert!(decode_snapshot(&encode_snapshot(&net)).is_err());
}

#[test]
fn snapshot_rejects_empty_route_to_station_behind_the_head() {
    let (mut net, _, _, edge) = line_network(40.0);
    net.add_station("behind", edge, 10.0).unwrap();
    net.add_train(scheduled_train(22, "behind"), edge, 30.0)
        .unwrap();
    {
        let train = net.trains.get_mut(&TrainId(22)).unwrap();
        train.target = Some(StationTarget {
            station_name: "behind".into(),
            edge,
            s: 10.0,
            wait_ticks: 0,
        });
        train.route.clear();
    }
    assert!(decode_snapshot(&encode_snapshot(&net)).is_err());
}

#[test]
fn replaying_same_tick_sequence_is_bit_identical() {
    fn fixture() -> RailNetwork {
        let (mut net, _, _, edge) = line_network(120.0);
        net.add_train(free_train(30), edge, 0.0).unwrap();
        net
    }
    let mut a = fixture();
    let mut b = fixture();
    let loaded = empty_loaded();
    for _ in 0..80 {
        let ea = tick_rail_network(&mut a, &loaded);
        let eb = tick_rail_network(&mut b, &loaded);
        assert_eq!(ea, eb);
    }
    assert_eq!(a, b);
}

#[test]
fn production_create_adapter_engine_consumes_and_ticks_rail_network() {
    let mut engine = CreateAdapterEngine::new();
    {
        let net = engine.rail_network_mut();
        let a = net.add_node(dim(), [0.0, 0.0, 0.0]);
        let b = net.add_node(dim(), [50.0, 0.0, 0.0]);
        let edge = net
            .add_edge(
                a,
                b,
                TrackGauge::Standard,
                seg([0.0, 0.0, 0.0], [50.0, 0.0, 0.0]),
            )
            .unwrap();
        net.add_train(free_train(40), edge, 0.0).unwrap();
    }

    let report = engine.step_tick(1, OffloadBudget::new(128)).unwrap();
    assert_eq!(report.rail_tick, 1);
    assert_eq!(engine.rail_network().tick, 1);
    assert!(engine.rail_network().trains[&TrainId(40)].pos_s > 0.0);
    assert!(report.rail_events.is_empty());
    assert_eq!(report.offload.tick, 1);
}
