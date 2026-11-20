// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later

use std::hint::black_box;
use std::time::Instant;

use mod_offload::kinetic_graph::{KineticGraph, KineticNode, NetworkStatus};
use mod_offload::sleeping_transport::{MachineStatus, SleepingTransport};

const MACHINES: u32 = 10_000;
const NODES: u32 = 2_000;
const NODES_PER_NETWORK: u32 = 100;
const OFFLOAD_TICKS: u64 = 2_000_000;
const NAIVE_TICKS: u64 = 100_000;
const WARMUP_TICKS: u64 = 1_000;

struct NaiveMachine {
    input: u32,
    output: u32,
    capacity: u32,
    ready_at: u64,
}

#[inline(never)]
fn full_scan(tick: u64, machines: &[NaiveMachine], dirty_nodes: &[bool]) -> usize {
    let ready = machines
        .iter()
        .filter(|machine| {
            machine.input != 0 && machine.output < machine.capacity && machine.ready_at <= tick
        })
        .count();
    ready + dirty_nodes.iter().filter(|&&dirty| dirty).count()
}

struct Timing {
    wall_ns_per_tick: f64,
    cpu_ns_per_tick: Option<f64>,
}

fn thread_cpu_ns() -> Option<u64> {
    // Linux schedstat exports the time this thread has spent on CPU in ns.
    // Reading the counter outside the loop keeps the probe out of each tick.
    std::fs::read_to_string("/proc/thread-self/schedstat")
        .ok()?
        .split_whitespace()
        .next()?
        .parse()
        .ok()
}

fn measure(ticks: u64, mut run_tick: impl FnMut(u64) -> usize) -> Timing {
    let cpu_start = thread_cpu_ns();
    let start = Instant::now();
    let mut changed = 0;
    for tick in 0..ticks {
        changed += black_box(run_tick(black_box(tick)));
    }
    let wall = start.elapsed();
    let cpu = thread_cpu_ns()
        .zip(cpu_start)
        .and_then(|(end, start)| end.checked_sub(start))
        .filter(|&elapsed| elapsed != 0);
    assert_eq!(changed, 0, "timed workload must stay settled");
    Timing {
        wall_ns_per_tick: wall.as_nanos() as f64 / ticks as f64,
        cpu_ns_per_tick: cpu.map(|ns| ns as f64 / ticks as f64),
    }
}

fn main() {
    let mut transport = SleepingTransport::default();
    for id in 0..MACHINES {
        transport.add_machine(id, 1, 1).unwrap();
        if id >= MACHINES / 2 {
            transport.insert(id, 7).unwrap();
        }
    }
    assert_eq!(
        transport
            .advance_to(1, MACHINES as usize)
            .unwrap()
            .processed,
        (MACHINES / 2) as usize
    );
    for id in MACHINES / 2..MACHINES {
        transport.insert(id, 8).unwrap();
    }
    let machines: Vec<_> = (0..MACHINES)
        .map(|id| {
            let state = transport.machine(id).unwrap();
            assert_eq!(state.status, MachineStatus::Dormant);
            assert_eq!(state.scheduled_wake_tick, None);
            NaiveMachine {
                input: state.input_items as u32,
                output: state.output_items as u32,
                capacity: 1,
                ready_at: 2,
            }
        })
        .collect();

    let mut graph = KineticGraph::default();
    for id in 0..NODES {
        let input = if id % NODES_PER_NETWORK == 0 {
            KineticNode {
                capacity: f64::from(NODES_PER_NETWORK),
                driven_speed: Some(16.0),
                load: 0.0,
            }
        } else {
            KineticNode {
                load: 1.0,
                ..KineticNode::default()
            }
        };
        graph.add_node(id, input).unwrap();
        if id % NODES_PER_NETWORK != 0 {
            graph.connect(id - 1, id, -1.0).unwrap();
        }
    }
    let dirty_nodes: Vec<_> = (0..NODES)
        .map(|id| {
            assert_eq!(graph.solution(id).unwrap().status, NetworkStatus::Running);
            false
        })
        .collect();

    let naive = |tick| {
        full_scan(
            tick,
            black_box(machines.as_slice()),
            black_box(dirty_nodes.as_slice()),
        )
    };
    let mut offload =
        |tick| transport.advance_to(tick, 1).unwrap().processed + black_box(&graph).tick();
    for tick in 2..2 + WARMUP_TICKS {
        assert_eq!(naive(tick), 0);
        assert_eq!(offload(tick), 0);
    }
    let first_measured_tick = 2 + WARMUP_TICKS;
    let scan = measure(NAIVE_TICKS, |tick| naive(tick + first_measured_tick));
    let event = measure(OFFLOAD_TICKS, |tick| offload(tick + first_measured_tick));

    println!("settled workload: {MACHINES} machines (5000 empty, 5000 blocked), {NODES} kinetic nodes across {} networks", NODES / NODES_PER_NETWORK);
    println!(
        "naive full scan: {NAIVE_TICKS} ticks, {} checks/tick, {:.2} ns/tick wall",
        MACHINES + NODES,
        scan.wall_ns_per_tick
    );
    println!(
        "event driven: {OFFLOAD_TICKS} ticks, 0 transfers or graph visits/tick, {:.2} ns/tick wall",
        event.wall_ns_per_tick
    );
    println!(
        "wall speedup: {:.1}x",
        scan.wall_ns_per_tick / event.wall_ns_per_tick
    );
    if let Some((scan, event)) = scan.cpu_ns_per_tick.zip(event.cpu_ns_per_tick) {
        println!(
            "thread CPU: {scan:.2} ns/tick naive, {event:.2} ns/tick event driven, {:.1}x speedup",
            scan / event
        );
    } else {
        println!("thread CPU: unavailable (requires Linux thread schedstat)");
    }
}
