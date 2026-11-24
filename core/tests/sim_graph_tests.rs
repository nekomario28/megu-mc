use megu_core::sim_graph::{ActiveSlotPool, SteadyNetworkGraph, TransportLine};

#[test]
fn sim_graph_cached_network_tick_is_zero_visit() {
    let mut graph = SteadyNetworkGraph::with_capacity(1);
    assert!(graph.add_node(7, 64, 2, 4, ()));
    assert_eq!(graph.solution(7).unwrap().rpm_fixed, 64);
    assert_eq!(graph.tick(), 0);
}

#[test]
fn transport_line_gap_motion_jam_wake_and_conservation() {
    let mut line = TransportLine::with_capacity(500);
    assert!(line.is_sleeping());
    for item in 0..500u32 {
        line.push_back(item).unwrap();
    }
    assert!(line.push_back(500).is_err());
    line.set_speed(1);
    assert_eq!(line.tick_step(), 1);
    assert_eq!(line.entry(0).unwrap().gap_units, 0);
    assert_eq!(line.entry(1).unwrap().gap_units, 1);
    line.tick_step();
    assert_eq!(line.entry(0).unwrap().gap_units, 0);
    line.set_output_blocked(true);
    assert!(line.is_sleeping());
    for _ in 0..1000 {
        assert_eq!(line.tick_step(), 0);
    }
    assert_eq!(line.len(), 500);
    assert_eq!(line.pop_front(), Some(0));
    assert!(!line.is_sleeping());
    line.tick_step();
    assert_eq!(line.entry(0).unwrap().gap_units, 0);
    let collected: Vec<_> = std::iter::from_fn(|| line.pop_front()).collect();
    assert_eq!(collected.len(), 499);
    assert_eq!(collected[0], 1);
    assert_eq!(collected[498], 499);
    assert!(line.is_sleeping());
    line.push_back(700).unwrap();
    assert!(!line.is_sleeping());
    line.set_speed(0);
    assert!(line.is_sleeping());
}

fn factory_network(order: &[u32], edges: &[(u32, u32)]) -> SteadyNetworkGraph<()> {
    let mut graph = SteadyNetworkGraph::with_capacity(order.len());
    for &id in order {
        let rpm = if id == 1 { 640 } else { 0 };
        let stress = if id == 2000 { 250 } else { 1 };
        let capacity = 2;
        assert!(graph.add_node(id, rpm, stress, capacity, ()));
    }
    for &(a, b) in edges {
        assert!(graph.connect(a, b));
    }
    graph
}

#[test]
fn kinetic_network_solves_on_edits_then_sleeps_and_wakes_local_component() {
    let ids: Vec<_> = (1..=2000).collect();
    let edges: Vec<_> = (1..2000).map(|id| (id, id + 1)).collect();
    let mut graph = factory_network(&ids, &edges);
    let solution = graph.solution(1000).unwrap();
    assert_eq!(solution.component_id, 1);
    assert_eq!(solution.rpm_fixed, 640);
    assert_eq!(solution.total_stress, 2249);
    assert_eq!(solution.total_capacity, 4000);
    assert!(!solution.overstressed);
    assert_eq!(graph.tick(), 0);
    for _ in 0..1000 {
        assert_eq!(graph.tick(), 0);
    }
    assert!(graph.set_node(2000, 0, 5000, 2));
    assert!(graph.solution(1000).unwrap().overstressed);
    assert_eq!(graph.last_edit_nodes_visited(), 2000);
    assert_eq!(graph.tick(), 0);

    // A separate component edit leaves the large factory's cached solution intact.
    assert!(graph.add_node(9000, 160, 8, 1, ()));
    assert!(graph.add_node(9001, 0, 1, 1, ()));
    assert!(graph.connect(9000, 9001));
    let factory_before = graph.solution(1).unwrap();
    assert!(graph.set_node(9001, 0, 20, 0));
    assert_eq!(graph.last_edit_nodes_visited(), 2);
    assert_eq!(graph.solution(1), Some(factory_before));
    assert!(graph.solution(9000).unwrap().overstressed);
    assert_eq!(graph.tick(), 0);
}

#[test]
fn active_slots_skip_idle_population_and_wake_exact_deadlines() {
    let mut pool = ActiveSlotPool::with_capacity(10_010, 256);
    for _ in 0..10_000 {
        pool.insert(0u32).unwrap();
    }
    let mut active = Vec::new();
    for _ in 0..10 {
        active.push(pool.insert(0u32).unwrap());
    }
    for &i in &active {
        assert!(pool.wake(i));
    }
    let mut total_steps = 0;
    for tick in 0..1000 {
        let visits = pool.tick(tick, |_, value| {
            *value += 1;
            None
        });
        total_steps += visits;
        assert_eq!(visits, if tick == 0 { 10 } else { 0 });
    }
    assert_eq!(total_steps, 10);
    assert!(active.iter().all(|&i| pool.get(i) == Some(&1)));

    assert!(pool.schedule(0, 1004));
    let reserved = pool.reserved_index_capacity();
    assert_eq!(pool.tick(1000, |_, _| None), 0);
    assert_eq!(pool.tick(1004, |_, _| None), 1);
    assert_eq!(pool.reserved_index_capacity(), reserved);
}

#[test]
fn shuffled_and_parallel_component_evaluation_has_identical_state_hash() {
    let order_a: Vec<_> = (1..=300).collect();
    let order_b: Vec<_> = order_a.iter().rev().copied().collect();
    let edges_a: Vec<_> = (1..300).map(|id| (id, id + 1)).collect();
    let edges_b: Vec<_> = edges_a.iter().rev().map(|&(a, b)| (b, a)).collect();
    let serial = factory_network(&order_a, &edges_a);
    let shuffled = factory_network(&order_b, &edges_b);
    assert_eq!(serial.state_hash(), shuffled.state_hash());

    let component_hashes = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..4)
            .map(|lane| {
                scope.spawn(move || {
                    let order = [lane + 1, lane + 5, lane + 9];
                    let mut graph = SteadyNetworkGraph::with_capacity(3);
                    for &id in &order {
                        graph.add_node(id, i64::from(id * 32), i64::from(id), 100, ());
                    }
                    graph.connect(order[0], order[1]);
                    graph.connect(order[1], order[2]);
                    graph.state_hash()
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect::<Vec<_>>()
    });
    let repeated = (0..4)
        .map(|lane| {
            let order = [lane + 1, lane + 5, lane + 9];
            let mut graph = SteadyNetworkGraph::with_capacity(3);
            for &id in &order {
                graph.add_node(id, i64::from(id * 32), i64::from(id), 100, ());
            }
            graph.connect(order[0], order[1]);
            graph.connect(order[1], order[2]);
            graph.state_hash()
        })
        .collect::<Vec<_>>();
    assert_eq!(component_hashes, repeated);
}

#[test]
fn deterministic_operation_counts_measure_sleep_tick_speedup() {
    let idle = 10_000usize;
    let active = 10usize;
    let naive_visits = (0..idle + active).count();
    let mut pool = ActiveSlotPool::with_capacity(idle + active, 16);
    for _ in 0..idle + active {
        pool.insert(()).unwrap();
    }
    for i in idle..idle + active {
        pool.wake(i);
    }
    let sleeping_visits = pool.tick(0, |_, _| None);
    let speedup = naive_visits / sleeping_visits;
    assert_eq!(speedup, 1001);
    assert_eq!(sleeping_visits, 10);
    assert_eq!(idle, 10_000);
}
