// SPDX-License-Identifier: LGPL-3.0-or-later

use std::hint::black_box;
use std::thread;
use std::time::Instant;

use mod_offload::contraption::{Aabb, BlockCluster, Contraption, Pose};
use mod_offload::kinetic_graph::{KineticGraph, KineticNode, NetworkStatus};
use mod_offload::recipe_processor::{Drive, Recipe, Resource, ResourceKind, Stack};
use mod_offload::region_scheduler::{Region, RegionBudget, RegionKey, MAX_BATCH_BYTES};
use mod_offload::sleeping_transport::{MachineStatus, SleepingTransport};

const MACHINES: u32 = 10_000;
const NODES: u32 = 2_000;
const NODES_PER_NETWORK: u32 = 100;
const OFFLOAD_TICKS: u64 = 2_000_000;
const NAIVE_TICKS: u64 = 100_000;
const WARMUP_TICKS: u64 = 1_000;
const DORMANT_CHAINS: u32 = 5_000;
const ACTIVE_CHAINS: u32 = 500;
const FACTORY_WARMUP: u64 = 20;
const FACTORY_CYCLES: u64 = 250;
const CONTRAPTION_TICKS: u64 = 2_000;
const PARALLEL_REGIONS: usize = 4;
const PARALLEL_CHAINS: u32 = 64;
const PARALLEL_WARMUP: u64 = 10;
const PARALLEL_CYCLES: u64 = 120;
const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 1_099_511_628_211;
const FACTORY_BUDGET: RegionBudget = RegionBudget {
    max_steps: 2_000,
    max_time: None,
};

type BenchResult<T> = Result<T, String>;

trait BenchContext<T> {
    fn bench_context(self, context: &'static str) -> BenchResult<T>;
}

impl<T, E: std::fmt::Debug> BenchContext<T> for Result<T, E> {
    fn bench_context(self, context: &'static str) -> BenchResult<T> {
        self.map_err(|error| format!("{context}: {error:?}"))
    }
}

impl<T> BenchContext<T> for Option<T> {
    fn bench_context(self, context: &'static str) -> BenchResult<T> {
        self.ok_or_else(|| context.to_owned())
    }
}

const fn stack(kind: ResourceKind, id: u32) -> Stack {
    Stack {
        resource: Resource { kind, id },
        amount: 1,
    }
}

const ORE: Stack = stack(ResourceKind::Item, 1);
const WATER: Stack = stack(ResourceKind::Fluid, 1);
const PRODUCT: Stack = stack(ResourceKind::Item, 2);
const BYPRODUCT: Stack = stack(ResourceKind::Fluid, 2);

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

fn measure(
    ticks: u64,
    mut run_tick: impl FnMut(u64) -> BenchResult<usize>,
) -> BenchResult<Timing> {
    let cpu_start = thread_cpu_ns();
    let start = Instant::now();
    let mut changed = 0;
    for tick in 0..ticks {
        changed += black_box(run_tick(black_box(tick))?);
    }
    let wall = start.elapsed();
    let cpu = thread_cpu_ns()
        .zip(cpu_start)
        .and_then(|(end, start)| end.checked_sub(start))
        .filter(|&elapsed| elapsed != 0);
    assert_eq!(changed, 0, "unexpected timed workload result");
    Ok(Timing {
        wall_ns_per_tick: wall.as_nanos() as f64 / ticks as f64,
        cpu_ns_per_tick: cpu.map(|ns| ns as f64 / ticks as f64),
    })
}

fn main() -> BenchResult<()> {
    let mut transport = SleepingTransport::default();
    for id in 0..MACHINES {
        transport
            .add_machine(id, 1, 1)
            .bench_context("add transport machine")?;
        if id >= MACHINES / 2 {
            transport
                .insert(id, 7)
                .bench_context("insert first transport item")?;
        }
    }
    let initial = transport
        .advance_to(1, MACHINES as usize)
        .bench_context("advance initial transport workload")?;
    assert_eq!(initial.processed, (MACHINES / 2) as usize);
    for id in MACHINES / 2..MACHINES {
        transport
            .insert(id, 8)
            .bench_context("insert second transport item")?;
    }
    let mut machines = Vec::new();
    for id in 0..MACHINES {
        let state = transport
            .machine(id)
            .bench_context("read transport machine state")?;
        assert_eq!(state.status, MachineStatus::Dormant);
        assert_eq!(state.scheduled_wake_tick, None);
        machines.push(NaiveMachine {
            input: state.input_items as u32,
            output: state.output_items as u32,
            capacity: 1,
            ready_at: 2,
        });
    }

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
        graph
            .add_node(id, input)
            .bench_context("add kinetic node")?;
        if id % NODES_PER_NETWORK != 0 {
            graph
                .connect(id - 1, id, -1.0)
                .bench_context("connect kinetic node")?;
        }
    }
    let mut dirty_nodes = Vec::new();
    for id in 0..NODES {
        let solution = graph
            .solution(id)
            .bench_context("read kinetic solution")?;
        assert_eq!(solution.status, NetworkStatus::Running);
        dirty_nodes.push(false);
    }

    let naive = |tick| -> BenchResult<usize> {
        Ok(full_scan(
            tick,
            black_box(machines.as_slice()),
            black_box(dirty_nodes.as_slice()),
        ))
    };
    let mut offload = |tick| -> BenchResult<usize> {
        Ok(transport
            .advance_to(tick, 1)
            .bench_context("advance transport workload")?
            .processed
            + black_box(&graph).tick())
    };
    for tick in 2..2 + WARMUP_TICKS {
        assert_eq!(naive(tick)?, 0);
        assert_eq!(offload(tick)?, 0);
    }
    let first_measured_tick = 2 + WARMUP_TICKS;
    let scan = measure(NAIVE_TICKS, |tick| naive(tick + first_measured_tick))?;
    let event = measure(OFFLOAD_TICKS, |tick| offload(tick + first_measured_tick))?;

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
    mixed_factory()?;
    moving_contraption()?;
    parallel_scaling()?;
    Ok(())
}

fn moving_contraption() -> BenchResult<()> {
    let blocks = (0..16)
        .flat_map(|x| (0..16).map(move |z| ([2 * x, 0, 2 * z], 1)))
        .collect();
    let cluster = BlockCluster::new(blocks).bench_context("build contraption cluster")?;
    let mut body = Contraption::new(cluster, Pose::default());
    let actors = [
        (0.5, 0.5, true),
        (8.5, 6.5, true),
        (18.5, 20.5, true),
        (30.5, 30.5, true),
        (1.5, 0.5, false),
        (16.5, 7.5, false),
        (30.5, 29.5, false),
        (32.5, 32.5, false),
    ];
    let mut run_tick = |tick: u64| -> BenchResult<usize> {
        let previous = body.pose();
        let offset = 1.0 + (tick % 16) as f64;
        let turned = !tick.is_multiple_of(2);
        let pose = Pose::from_yaw_pitch(
            [offset, 1.0, 0.0],
            if turned { 90.0 } else { 0.0 },
            0.0,
        )
        .bench_context("construct contraption pose")?;
        assert!(body.set_pose(pose).is_some());
        let passenger = [previous.translation()[0] + 0.5, 1.5, 0.5];
        let carried = pose
            .carried_delta(previous, black_box(passenger))
            .bench_context("compute carried passenger delta")?;
        black_box(carried);
        let mut mismatches = 0;
        for &(x, z, expected) in &actors {
            let (x, z) = if turned { (z, -x) } else { (x, z) };
            let center = [offset + x, 1.5, z];
            let bounds = Aabb::new(center.map(|v| v - 0.2), center.map(|v| v + 0.2))
                .bench_context("construct actor bounds")?;
            if black_box(body.intersects(black_box(bounds))) != expected {
                mismatches += 1;
            }
        }
        Ok(mismatches)
    };
    for tick in 0..WARMUP_TICKS {
        assert_eq!(run_tick(tick)?, 0);
    }
    let timing = measure(CONTRAPTION_TICKS, |tick| run_tick(tick + WARMUP_TICKS))?;
    println!(
        "contraption: 256 blocks, {WARMUP_TICKS} warmup ticks, {CONTRAPTION_TICKS} measured ticks, 1 pose + 1 passenger delta + 8 actor queries/tick"
    );
    println!(
        "contraption wall: {:.2} us/tick",
        timing.wall_ns_per_tick / 1000.0
    );
    if let Some(cpu) = timing.cpu_ns_per_tick {
        println!("contraption thread CPU: {:.2} us/tick", cpu / 1000.0);
    }
    Ok(())
}

fn mixed_factory() -> BenchResult<()> {
    let mut region = build_factory(
        RegionKey::for_block(0, 0, 0),
        ACTIVE_CHAINS + DORMANT_CHAINS,
    )?;
    let mut frame = vec![0; MAX_BATCH_BYTES];
    assert_eq!(
        region
            .export_into(&mut frame, usize::MAX)
            .bench_context("export empty factory batch")?
            .records,
        0
    );
    for cycle in 0..FACTORY_WARMUP {
        let _ = factory_cycle(&mut region, &mut frame, cycle, ACTIVE_CHAINS)?;
    }

    let cpu_start = thread_cpu_ns();
    let start = Instant::now();
    for cycle in FACTORY_WARMUP..FACTORY_WARMUP + FACTORY_CYCLES {
        let _ = factory_cycle(&mut region, &mut frame, cycle, ACTIVE_CHAINS)?;
    }
    let micros = start.elapsed().as_secs_f64() * 1e6 / FACTORY_CYCLES as f64;
    let cpu = thread_cpu_ns()
        .zip(cpu_start)
        .and_then(|(end, start)| end.checked_sub(start));
    println!("mixed factory: {DORMANT_CHAINS} dormant + {ACTIVE_CHAINS} active chains, {FACTORY_CYCLES} cycles, 2 ticks/cycle");
    println!("mixed factory work/cycle: {ACTIVE_CHAINS} belt transfers, {} fluid solves, {ACTIVE_CHAINS} recipes, {} S5 records", ACTIVE_CHAINS * 2, ACTIVE_CHAINS * 4);
    println!(
        "mixed factory wall: {micros:.2} us/cycle, {:.2} us/tick",
        micros / 2.0
    );
    if let Some(cpu) = cpu {
        let micros = cpu as f64 / (1000.0 * FACTORY_CYCLES as f64);
        println!(
            "mixed factory thread CPU: {micros:.2} us/cycle, {:.2} us/tick",
            micros / 2.0
        );
    }
    Ok(())
}

/// Four independent regions with disjoint keys run sequentially, then in
/// parallel via `thread::scope`. Every region's exported S5 bytes must match
/// its sequential run, proving the kernels share no state.
fn parallel_scaling() -> BenchResult<()> {
    fn build(index: usize) -> BenchResult<Region> {
        build_factory(
            RegionKey {
                dimension: 0,
                x: index as i32,
                y: 0,
                z: -3,
            },
            PARALLEL_CHAINS,
        )
    }

    let mut sequential: Vec<Region> = (0..PARALLEL_REGIONS)
        .map(build)
        .collect::<BenchResult<Vec<_>>>()?;
    let mut frames: Vec<Vec<u8>> = (0..PARALLEL_REGIONS)
        .map(|_| vec![0; MAX_BATCH_BYTES])
        .collect();
    for (region, frame) in sequential.iter_mut().zip(frames.iter_mut()) {
        for cycle in 0..PARALLEL_WARMUP {
            let _ = factory_cycle(region, frame, cycle, PARALLEL_CHAINS)?;
        }
    }
    let start = Instant::now();
    let expected: Vec<u64> = sequential
        .iter_mut()
        .zip(frames.iter_mut())
        .map(|(region, frame)| {
            factory_digest(
                region,
                frame,
                PARALLEL_WARMUP,
                PARALLEL_CYCLES,
                PARALLEL_CHAINS,
            )
        })
        .collect::<BenchResult<Vec<_>>>()?;
    let sequential_wall = start.elapsed();

    let mut concurrent: Vec<Region> = (0..PARALLEL_REGIONS)
        .map(build)
        .collect::<BenchResult<Vec<_>>>()?;
    let mut concurrent_frames: Vec<Vec<u8>> = (0..PARALLEL_REGIONS)
        .map(|_| vec![0; MAX_BATCH_BYTES])
        .collect();
    for (region, frame) in concurrent.iter_mut().zip(concurrent_frames.iter_mut()) {
        for cycle in 0..PARALLEL_WARMUP {
            let _ = factory_cycle(region, frame, cycle, PARALLEL_CHAINS)?;
        }
    }
    let start = Instant::now();
    let actual: BenchResult<Vec<u64>> = thread::scope(|scope| {
        let handles: Vec<_> = concurrent
            .iter_mut()
            .zip(concurrent_frames.iter_mut())
            .map(|(region, frame)| {
                scope.spawn(move || {
                    factory_digest(
                        region,
                        frame,
                        PARALLEL_WARMUP,
                        PARALLEL_CYCLES,
                        PARALLEL_CHAINS,
                    )
                })
            })
            .collect();
        let mut joined = Vec::with_capacity(handles.len());
        for handle in handles {
            joined.push(
                handle
                    .join()
                    .map_err(|_| "parallel benchmark worker panicked".to_owned())??,
            );
        }
        Ok(joined)
    });
    let actual = actual?;
    let concurrent_wall = start.elapsed();

    assert_eq!(
        actual, expected,
        "concurrent region export diverged from sequential output"
    );

    let region_cycles = (PARALLEL_REGIONS * PARALLEL_CYCLES as usize) as f64;
    let sequential_s = sequential_wall.as_secs_f64();
    let concurrent_s = concurrent_wall.as_secs_f64();
    println!("parallel scaling: {PARALLEL_REGIONS} isolated regions x {PARALLEL_CHAINS} chains x {PARALLEL_CYCLES} cycles");
    println!(
        "parallel sequential: {:.2} ms wall, {:.0} region-cycles/s",
        sequential_s * 1e3,
        region_cycles / sequential_s
    );
    println!(
        "parallel concurrent: {:.2} ms wall, {:.0} region-cycles/s",
        concurrent_s * 1e3,
        region_cycles / concurrent_s
    );
    println!(
        "parallel speedup: {:.2}x with {PARALLEL_REGIONS} threads",
        sequential_s / concurrent_s
    );
    println!(
        "parallel determinism: {PARALLEL_REGIONS} region digests match sequential export bytes"
    );
    Ok(())
}

fn build_factory(key: RegionKey, chains: u32) -> BenchResult<Region> {
    let mut region = Region::new(key);
    let recipe = Recipe {
        id: 1,
        base_ticks: 2,
        inputs: vec![ORE, WATER],
        outputs: vec![PRODUCT, BYPRODUCT],
    };
    for id in 0..chains {
        let source = id * 2;
        let sink = source + 1;
        region
            .kinetic
            .add_node(
                source,
                KineticNode {
                    capacity: 2.0,
                    load: 0.0,
                    driven_speed: Some(32.0),
                },
            )
            .bench_context("add factory kinetic source")?;
        region
            .kinetic
            .add_node(
                sink,
                KineticNode {
                    load: 1.0,
                    ..KineticNode::default()
                },
            )
            .bench_context("add factory kinetic sink")?;
        region
            .kinetic
            .connect(source, sink, 1.0)
            .bench_context("connect factory kinetic nodes")?;
        region
            .add_machine(id, 1, 1)
            .bench_context("add factory transport machine")?;
        region
            .add_tank(source, 2, 0)
            .bench_context("add factory source tank")?;
        region
            .add_tank(sink, 2, 0)
            .bench_context("add factory sink tank")?;
        region
            .set_fluid_link(source, sink, Some(0))
            .bench_context("link factory tanks")?;
        let drive = factory_drive(&region, sink)?;
        region
            .add_processor(id, 2, 2, vec![recipe.clone()], drive)
            .bench_context("add factory processor")?;
    }
    while region
        .advance_to(0, FACTORY_BUDGET)
        .bench_context("settle factory region")?
        .backlog
        .is_some()
    {}
    Ok(region)
}

fn factory_drive(region: &Region, node: u32) -> BenchResult<Drive> {
    let solution = region
        .kinetic
        .solution(node)
        .bench_context("read factory kinetic solution")?;
    assert_eq!(solution.status, NetworkStatus::Running);
    // This fixture uses only whole RPM values representable as u32.
    Ok(Drive::Mechanical {
        rpm: solution.speed.abs() as u32,
        overstressed: false,
    })
}

fn factory_digest(
    region: &mut Region,
    frame: &mut [u8],
    first_cycle: u64,
    cycles: u64,
    chains: u32,
) -> BenchResult<u64> {
    let mut digest = FNV_OFFSET;
    for cycle in first_cycle..first_cycle + cycles {
        let step = factory_cycle(region, frame, cycle, chains)?;
        digest = digest.wrapping_mul(FNV_PRIME) ^ step;
    }
    Ok(digest)
}

fn digest_bytes(mut hash: u64, bytes: &[u8]) -> u64 {
    for &byte in bytes {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

fn factory_cycle(
    region: &mut Region,
    frame: &mut [u8],
    cycle: u64,
    chains: u32,
) -> BenchResult<u64> {
    for id in 0..chains {
        let source = id * 2;
        if cycle.is_multiple_of(8) {
            let speed = if cycle.is_multiple_of(16) { 64.0 } else { 32.0 };
            let visited = region
                .kinetic
                .update_node(
                    source,
                    KineticNode {
                        capacity: 2.0,
                        load: 0.0,
                        driven_speed: Some(speed),
                    },
                )
                .bench_context("update factory kinetic source")?;
            assert_eq!(visited, 2);
            let drive = factory_drive(region, source + 1)?;
            region
                .set_processor_drive(id, drive)
                .bench_context("update factory processor drive")?;
        }
        region
            .insert_item(id, ORE.resource.id)
            .bench_context("insert factory item")?;
        assert_eq!(region.insert_fluid(source, WATER.resource.id, 2), Ok(2));
    }
    let belt = region
        .advance_to(cycle * 2 + 1, FACTORY_BUDGET)
        .bench_context("advance factory transport")?;
    assert_eq!(belt.processed, 3 * chains as usize);
    assert_eq!(belt.backlog, None);
    for id in 0..chains {
        let source = id * 2;
        assert_eq!(region.extract_output(id), Ok(Some(ORE.resource.id)));
        assert_eq!(region.extract_fluid(source, 2), Ok(1));
        assert_eq!(region.extract_fluid(source + 1, 2), Ok(1));
        assert_eq!(region.insert_ingredient(id, ORE), Ok(1));
        assert_eq!(region.insert_ingredient(id, WATER), Ok(1));
    }
    let recipe = region
        .advance_to(cycle * 2 + 2, FACTORY_BUDGET)
        .bench_context("advance factory recipes")?;
    assert_eq!(recipe.processed, 2 * chains as usize);
    assert_eq!(recipe.backlog, None);
    for id in 0..chains {
        assert_eq!(region.extract_product(id, PRODUCT), Ok(1));
        assert_eq!(region.extract_product(id, BYPRODUCT), Ok(1));
    }
    let batch = region
        .export_into(black_box(&mut *frame), usize::MAX)
        .bench_context("export factory batch")?;
    assert_eq!(batch.records, 4 * chains as usize);
    assert_eq!(batch.bytes, 32 + 96 * chains as usize);
    Ok(digest_bytes(FNV_OFFSET, &frame[..batch.bytes]))
}
