// SPDX-License-Identifier: LGPL-3.0-or-later

//! Differential equivalence proofs for the universal offload kernels.
//!
//! Every optimized kernel is compared with an independent, deliberately
//! unoptimized per-tick/full-recompute reference model. The references are
//! written for this integration suite only: they favour clarity over speed,
//! while the kernels under test use dirty components, wake queues, and O(1)
//! catch-up algebra.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use mod_offload::combat_motion::{ActionSpec, HitShape};
use mod_offload::fluid_network::FluidNetwork;
use mod_offload::kernel_router::{
    GraphDomain, ModKernelRouter, RoutingBudget, UniversalKernelKind,
};
use mod_offload::kinetic_graph::{KineticGraph, KineticNode, NetworkStatus, Solution};
use mod_offload::lazy_catchup::{
    catch_up_furnace, catch_up_machine_batch, CatchUpLimits, FurnaceCatchUpState,
    MachineCatchUpState, HARD_MAX_DELTA_TICKS,
};
use mod_offload::me_network::{MeChannelGraph, MeNodeKind};
use mod_offload::recipe_processor::{
    Drive, Recipe, RecipeEvent, RecipeProcessor, Resource, ResourceKind, Stack,
};
use mod_offload::sleeping_transport::{MachineStatus, SleepingTransport};
use mod_offload::{DeclarativeProfile, FeatureProfile as RouterFeatureProfile};

fn item(id: u32, amount: u32) -> Stack {
    Stack {
        resource: Resource {
            kind: ResourceKind::Item,
            id,
        },
        amount,
    }
}

fn limits() -> CatchUpLimits {
    CatchUpLimits {
        enabled: true,
        max_delta_ticks: HARD_MAX_DELTA_TICKS,
    }
}

// ---------------------------------------------------------------------------
// Kinetic graph: independent full-component recompute
// ---------------------------------------------------------------------------

#[derive(Default)]
struct ReferenceKinetic {
    nodes: BTreeMap<u32, KineticNode>,
    edges: BTreeMap<u32, BTreeMap<u32, f64>>,
    solutions: BTreeMap<u32, Solution>,
}

impl ReferenceKinetic {
    fn add_node(&mut self, id: u32, input: KineticNode) {
        self.nodes.insert(id, input);
        self.edges.entry(id).or_default();
        self.solve_all();
    }

    fn update_node(&mut self, id: u32, input: KineticNode) {
        *self.nodes.get_mut(&id).unwrap() = input;
        self.solve_all();
    }

    fn connect(&mut self, a: u32, b: u32, ratio: f64) {
        self.edges.get_mut(&a).unwrap().insert(b, ratio);
        self.edges.get_mut(&b).unwrap().insert(a, ratio.recip());
        self.solve_all();
    }

    fn disconnect(&mut self, a: u32, b: u32) {
        self.edges.get_mut(&a).unwrap().remove(&b);
        self.edges.get_mut(&b).unwrap().remove(&a);
        self.solve_all();
    }

    fn remove_node(&mut self, id: u32) {
        self.nodes.remove(&id);
        let neighbours: Vec<_> = self.edges.remove(&id).unwrap().into_keys().collect();
        for other in neighbours {
            self.edges.get_mut(&other).unwrap().remove(&id);
        }
        self.solutions.remove(&id);
        self.solve_all();
    }

    fn solve_all(&mut self) {
        self.solutions.clear();
        let mut unseen: BTreeSet<_> = self.nodes.keys().copied().collect();
        while let Some(start) = unseen.first().copied() {
            let mut scales = BTreeMap::from([(start, 1.0f64)]);
            let mut queue = VecDeque::from([start]);
            let mut conflict = false;
            while let Some(id) = queue.pop_front() {
                let base = scales[&id];
                for (&neighbour, &ratio) in &self.edges[&id] {
                    let scale = base * ratio;
                    if !scale.is_finite() || scale == 0.0 {
                        conflict = true;
                    }
                    match scales.entry(neighbour) {
                        std::collections::btree_map::Entry::Vacant(entry) => {
                            entry.insert(scale);
                            queue.push_back(neighbour);
                        }
                        std::collections::btree_map::Entry::Occupied(entry) => {
                            conflict |= !close(*entry.get(), scale);
                        }
                    }
                }
            }
            for &id in scales.keys() {
                unseen.remove(&id);
            }
            let component = *scales.keys().next().unwrap();
            let mut drive = None;
            let mut capacity = 0.0;
            let mut load = 0.0;
            for (&id, &scale) in &scales {
                let input = self.nodes[&id];
                capacity += input.capacity;
                load += input.load;
                if let Some(speed) = input.driven_speed {
                    let speed = speed / scale;
                    conflict |=
                        !speed.is_finite() || (speed == 0.0 && input.driven_speed != Some(0.0));
                    let first = *drive.get_or_insert(speed);
                    conflict |= !close(first, speed);
                }
            }
            let base = drive.unwrap_or(0.0);
            conflict |= !capacity.is_finite() || !load.is_finite();
            conflict |= scales.values().any(|scale| {
                let speed = base * scale;
                !speed.is_finite() || (speed == 0.0 && base != 0.0)
            });
            let status = if conflict {
                NetworkStatus::Conflict
            } else if drive.is_none() {
                NetworkStatus::Unpowered
            } else if load > capacity {
                NetworkStatus::Overstressed
            } else {
                NetworkStatus::Running
            };
            for (&id, &scale) in &scales {
                self.solutions.insert(
                    id,
                    Solution {
                        component,
                        capacity,
                        load,
                        speed: if status == NetworkStatus::Running {
                            base * scale
                        } else {
                            0.0
                        },
                        status,
                    },
                );
            }
        }
    }
}

fn close(a: f64, b: f64) -> bool {
    a == b || (a - b).abs() <= 1e-9 * a.abs().max(b.abs())
}

fn assert_solution_eq(got: Solution, want: Solution, context: &str) {
    assert_eq!(got.component, want.component, "{context}: component");
    assert_eq!(
        got.capacity.to_bits(),
        want.capacity.to_bits(),
        "{context}: capacity"
    );
    assert_eq!(got.load.to_bits(), want.load.to_bits(), "{context}: load");
    assert_eq!(
        got.speed.to_bits(),
        want.speed.to_bits(),
        "{context}: speed"
    );
    assert_eq!(got.status, want.status, "{context}: status");
}

fn drive(capacity: f64, speed: f64) -> KineticNode {
    KineticNode {
        capacity,
        driven_speed: Some(speed),
        ..KineticNode::default()
    }
}

fn load(amount: f64) -> KineticNode {
    KineticNode {
        load: amount,
        ..KineticNode::default()
    }
}

fn assert_kinetic_pair(graph: &KineticGraph, reference: &ReferenceKinetic, ids: &[u32]) {
    for &id in ids {
        let context = format!("node {id}");
        assert_solution_eq(
            graph.solution(id).unwrap(),
            reference.solutions[&id],
            &context,
        );
    }
}

#[test]
fn kinetic_dirty_components_match_full_recompute_through_stress_and_topology_edits() {
    let mut graph = KineticGraph::default();
    let mut reference = ReferenceKinetic::default();
    let mut ids = Vec::new();

    // A powered gearbox/gearshift chain with negative and fractional ratios.
    for (id, node) in [
        (0, drive(64.0, 32.0)),
        (1, load(8.0)),
        (2, KineticNode::default()),
        (3, load(8.0)),
        (4, KineticNode::default()),
    ] {
        graph.add_node(id, node).unwrap();
        reference.add_node(id, node);
        ids.push(id);
    }
    for (a, b, ratio) in [(0, 1, -0.5), (1, 2, 2.0), (2, 3, -1.0), (3, 4, 0.25)] {
        let visits = graph.connect(a, b, ratio).unwrap();
        assert!(visits > 0, "topology edit did not solve the dirty graph");
        reference.connect(a, b, ratio);
        assert_kinetic_pair(&graph, &reference, &ids);
    }
    assert_eq!(graph.tick(), 0, "clean steady-state tick must cost zero");

    // Exact signed speeds prove gearbox/gearshift reversal propagation.
    assert_eq!(
        graph.solution(1).unwrap().speed.to_bits(),
        (-16.0f64).to_bits()
    );
    assert_eq!(
        graph.solution(2).unwrap().speed.to_bits(),
        (-32.0f64).to_bits()
    );
    assert_eq!(
        graph.solution(3).unwrap().speed.to_bits(),
        (32.0f64).to_bits()
    );
    assert_eq!(
        graph.solution(4).unwrap().speed.to_bits(),
        (8.0f64).to_bits()
    );

    // RPM-independent additive stress crosses the overstress threshold and
    // recovers at the exact same node states as the full reference solve.
    let overloaded = load(65.0);
    assert!(graph.update_node(3, overloaded).unwrap() > 0);
    reference.update_node(3, overloaded);
    assert_kinetic_pair(&graph, &reference, &ids);
    assert_eq!(
        graph.solution(0).unwrap().status,
        NetworkStatus::Overstressed
    );
    assert_eq!(graph.solution(4).unwrap().speed.to_bits(), 0.0f64.to_bits());

    let recovered = load(8.0);
    assert!(graph.update_node(3, recovered).unwrap() > 0);
    reference.update_node(3, recovered);
    assert_kinetic_pair(&graph, &reference, &ids);
    assert_eq!(
        graph.solution(4).unwrap().speed.to_bits(),
        (8.0f64).to_bits()
    );
    assert_eq!(graph.tick(), 0);

    // A contradictory cycle fails closed, then removing the contradiction
    // recovers without changing the fixed-point bit pattern elsewhere.
    assert!(graph.connect(0, 4, -4.0).unwrap() > 0);
    reference.connect(0, 4, -4.0);
    assert_kinetic_pair(&graph, &reference, &ids);
    assert_eq!(graph.solution(2).unwrap().status, NetworkStatus::Conflict);
    assert!(graph.disconnect(0, 4).unwrap() > 0);
    reference.disconnect(0, 4);
    assert_kinetic_pair(&graph, &reference, &ids);
    assert_eq!(graph.solution(2).unwrap().status, NetworkStatus::Running);

    // Splitting the network wakes both dirty sides exactly once and then
    // returns to a zero-cost steady tick.
    assert!(graph.disconnect(1, 2).unwrap() > 0);
    reference.disconnect(1, 2);
    assert_kinetic_pair(&graph, &reference, &ids);
    assert_eq!(graph.solution(2).unwrap().status, NetworkStatus::Unpowered);
    assert_eq!(graph.tick(), 0);

    // Removing a junction is likewise a topology edit, not a per-tick scan.
    assert!(graph.remove_node(1).unwrap() > 0);
    reference.remove_node(1);
    ids.retain(|&id| id != 1);
    assert_kinetic_pair(&graph, &reference, &ids);
    assert_eq!(graph.tick(), 0);
}

// ---------------------------------------------------------------------------
// Sleeping transport: independent one-tick FIFO machine model
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct RefTransportMachine {
    capacity: usize,
    delay: u64,
    input: VecDeque<u32>,
    output: VecDeque<u32>,
    downstream: Option<u32>,
    upstream: Option<u32>,
    timer: Option<u64>,
}

struct ReferenceTransport {
    machines: BTreeMap<u32, RefTransportMachine>,
    events: Vec<(u64, u32, Option<u32>, u32)>,
    now: u64,
}

impl ReferenceTransport {
    fn add_machine(&mut self, id: u32, capacity: u32, delay: u64) {
        self.machines.insert(
            id,
            RefTransportMachine {
                capacity: capacity as usize,
                delay,
                input: VecDeque::new(),
                output: VecDeque::new(),
                downstream: None,
                upstream: None,
                timer: None,
            },
        );
    }

    fn set_downstream(&mut self, id: u32, to: Option<u32>) {
        if let Some(old) = self.machines[&id].downstream {
            self.machines.get_mut(&old).unwrap().upstream = None;
        }
        if let Some(target) = to {
            self.machines.get_mut(&target).unwrap().upstream = Some(id);
        }
        self.machines.get_mut(&id).unwrap().downstream = to;
        self.reconcile(id);
        if let Some(upstream) = self.machines[&id].upstream {
            self.reconcile(upstream);
        }
    }

    fn insert(&mut self, id: u32, item: u32) {
        self.machines.get_mut(&id).unwrap().input.push_back(item);
        self.reconcile(id);
        if let Some(upstream) = self.machines[&id].upstream {
            self.reconcile(upstream);
        }
    }

    fn extract_output(&mut self, id: u32) -> Option<u32> {
        let item = self.machines.get_mut(&id).unwrap().output.pop_front();
        if item.is_some() {
            self.reconcile(id);
            if let Some(upstream) = self.machines[&id].upstream {
                self.reconcile(upstream);
            }
        }
        item
    }

    fn can_output(&self, id: u32) -> bool {
        let machine = &self.machines[&id];
        match machine.downstream {
            Some(target) => {
                let target = &self.machines[&target];
                target.input.len() < target.capacity
            }
            None => machine.output.len() < machine.capacity,
        }
    }

    fn reconcile(&mut self, id: u32) {
        let can_output = self.can_output(id);
        let machine = self.machines.get_mut(&id).unwrap();
        if machine.input.is_empty() || !can_output {
            machine.timer = None;
        } else if machine.timer.is_none() {
            machine.timer = Some(machine.delay);
        }
    }

    fn tick(&mut self) {
        self.now += 1;
        let ids: Vec<_> = self.machines.keys().copied().collect();
        let mut transfers = Vec::new();
        for id in ids {
            let can_output = self.can_output(id);
            let machine = self.machines.get_mut(&id).unwrap();
            if machine.timer.is_none() {
                continue;
            }
            if machine.input.is_empty() || !can_output {
                machine.timer = None;
                continue;
            }
            let timer = machine.timer.unwrap();
            if timer > 1 {
                machine.timer = Some(timer - 1);
                continue;
            }
            machine.timer = None;
            if let Some(item) = machine.input.pop_front() {
                transfers.push((id, item));
            }
        }
        for (source, item) in transfers {
            let destination = self.machines[&source].downstream;
            if let Some(destination) = destination {
                self.machines
                    .get_mut(&destination)
                    .unwrap()
                    .input
                    .push_back(item);
            } else {
                self.machines
                    .get_mut(&source)
                    .unwrap()
                    .output
                    .push_back(item);
            }
            self.events.push((self.now, source, destination, item));
            self.reconcile(source);
            if let Some(destination) = destination {
                self.reconcile(destination);
            }
            if let Some(upstream) = self.machines[&source].upstream {
                self.reconcile(upstream);
            }
        }
    }

    fn advance_to(&mut self, tick: u64) {
        while self.now < tick {
            self.tick();
        }
    }

    fn next_wake_tick(&self) -> Option<u64> {
        self.machines
            .iter()
            .filter_map(|(&id, machine)| machine.timer.map(|timer| (self.now + timer, id)))
            .min()
            .map(|(tick, _)| tick)
    }

    fn drain_events(&mut self) -> Vec<(u64, u32, Option<u32>, u32)> {
        std::mem::take(&mut self.events)
    }
}

fn machine_tuple(
    transport: &SleepingTransport,
    id: u32,
) -> (MachineStatus, usize, usize, Option<u64>) {
    let machine = transport.machine(id).unwrap();
    (
        machine.status,
        machine.input_items,
        machine.output_items,
        machine.scheduled_wake_tick,
    )
}

fn assert_transport_pair(
    optimized: &SleepingTransport,
    reference: &ReferenceTransport,
    ids: &[u32],
) {
    assert_eq!(optimized.now(), reference.now);
    assert_eq!(optimized.next_wake_tick(), reference.next_wake_tick());
    for &id in ids {
        let want = &reference.machines[&id];
        let want_status = if want.timer.is_some() {
            MachineStatus::Waiting
        } else {
            MachineStatus::Dormant
        };
        assert_eq!(
            machine_tuple(optimized, id),
            (
                want_status,
                want.input.len(),
                want.output.len(),
                want.timer.map(|timer| reference.now + timer)
            ),
            "transport machine {id} diverged at tick {}",
            reference.now
        );
    }
}

#[test]
fn sleeping_transport_matches_one_tick_stepping_exactly_under_backpressure() {
    let mut optimized = SleepingTransport::default();
    let mut reference = ReferenceTransport {
        machines: BTreeMap::new(),
        events: Vec::new(),
        now: 0,
    };
    optimized.add_machine(0, 2, 3).unwrap();
    optimized.add_machine(1, 1, 5).unwrap();
    reference.add_machine(0, 2, 3);
    reference.add_machine(1, 1, 5);
    optimized.set_downstream(0, Some(1)).unwrap();
    reference.set_downstream(0, Some(1));
    let ids = [0, 1];

    // Backpressure cancels machine 0's first timer. Machine 1's output handoff
    // then restarts it from a clean full delay, not from stale elapsed work.
    optimized.insert(0, 101).unwrap();
    reference.insert(0, 101);
    optimized.insert(1, 202).unwrap();
    reference.insert(1, 202);
    assert_transport_pair(&optimized, &reference, &ids);
    assert_eq!(optimized.machine(0).unwrap().status, MachineStatus::Dormant);

    let report = optimized.advance_to(5, 16).unwrap();
    reference.advance_to(5);
    assert_eq!(
        report
            .deltas
            .iter()
            .map(|d| (d.tick, d.source, d.destination, d.item))
            .collect::<Vec<_>>(),
        reference.drain_events()
    );
    assert_transport_pair(&optimized, &reference, &ids);
    assert_eq!(optimized.next_wake_tick(), Some(8));

    // Machine 0 transfers into machine 1 at the exact wake tick. Its input
    // then stalls because machine 1's output slot is still occupied.
    let report = optimized.advance_to(8, 16).unwrap();
    reference.advance_to(8);
    assert_eq!(
        report
            .deltas
            .iter()
            .map(|d| (d.tick, d.source, d.destination, d.item))
            .collect::<Vec<_>>(),
        reference.drain_events()
    );
    assert_transport_pair(&optimized, &reference, &ids);
    assert_eq!(optimized.next_wake_tick(), None);

    // Freeing the output slot is an external wake: the same full five-tick
    // delay starts in both models and no hidden wake fires before it.
    assert_eq!(optimized.extract_output(1), Ok(Some(202)));
    assert_eq!(reference.extract_output(1), Some(202));
    assert_transport_pair(&optimized, &reference, &ids);
    assert_eq!(optimized.next_wake_tick(), Some(13));
    for tick in [9, 10, 12] {
        let report = optimized.advance_to(tick, 16).unwrap();
        reference.advance_to(tick);
        assert_eq!(report.processed, 0, "idle jump to {tick} performed work");
        assert!(report.deltas.is_empty());
        assert_transport_pair(&optimized, &reference, &ids);
    }

    let report = optimized.advance_to(500, 16).unwrap();
    reference.advance_to(500);
    assert_eq!(
        report
            .deltas
            .iter()
            .map(|d| (d.tick, d.source, d.destination, d.item))
            .collect::<Vec<_>>(),
        reference.drain_events()
    );
    assert_transport_pair(&optimized, &reference, &ids);
    assert_eq!(optimized.extract_output(1), Ok(Some(101)));
    assert_eq!(reference.extract_output(1), Some(101));
    assert_transport_pair(&optimized, &reference, &ids);
}

// ---------------------------------------------------------------------------
// Due-process recipes: independent one-tick machine model
// ---------------------------------------------------------------------------

struct ReferenceRecipeMachine {
    input_capacity: u32,
    output_capacity: u32,
    recipes: Vec<Recipe>,
    drive: Drive,
    input: BTreeMap<Resource, u32>,
    output: BTreeMap<Resource, u32>,
    active: Option<usize>,
    remaining: u64,
    now: u64,
    events: Vec<RecipeEvent>,
}

impl ReferenceRecipeMachine {
    fn rate(&self) -> u64 {
        match self.drive {
            Drive::Mechanical {
                rpm,
                overstressed: false,
            } => u64::from(rpm),
            Drive::Furnace { fuel_ticks: 1.. } => 16,
            _ => 0,
        }
    }

    fn input_used(&self) -> u32 {
        self.input.values().sum()
    }

    fn output_used(&self) -> u32 {
        self.output.values().sum()
    }

    fn insert(&mut self, machine: u32, stack: Stack) -> u32 {
        if !self
            .recipes
            .iter()
            .flat_map(|recipe| recipe.inputs.iter())
            .any(|input| input.resource == stack.resource)
        {
            return 0;
        }
        let amount = stack.amount.min(self.input_capacity - self.input_used());
        if amount > 0 {
            *self.input.entry(stack.resource).or_default() += amount;
            self.try_start();
        }
        let _ = machine;
        amount
    }

    fn extract(&mut self, stack: Stack) -> u32 {
        let have = self.output.get(&stack.resource).copied().unwrap_or(0);
        let amount = have.min(stack.amount);
        if amount > 0 {
            let left = have - amount;
            if left == 0 {
                self.output.remove(&stack.resource);
            } else {
                self.output.insert(stack.resource, left);
            }
            self.try_start();
        }
        amount
    }

    fn try_start(&mut self) {
        if self.active.is_some() || self.rate() == 0 {
            return;
        }
        let space = self.output_capacity - self.output_used();
        self.active =
            self.recipes.iter().position(|recipe| {
                recipe.inputs.iter().all(|stack| {
                    self.input.get(&stack.resource).copied().unwrap_or(0) >= stack.amount
                }) && recipe.outputs.iter().map(|stack| stack.amount).sum::<u32>() <= space
            });
        if let Some(index) = self.active {
            self.remaining = u64::from(self.recipes[index].base_ticks) * 16;
        }
    }

    fn tick(&mut self, machine: u32) {
        self.now += 1;
        let Some(index) = self.active else {
            return;
        };
        let rate = self.rate();
        let mut powered = 0u64;
        let mut fuel_hit_zero = false;
        if rate > 0 {
            powered = 1;
            if let Drive::Furnace { fuel_ticks } = &mut self.drive {
                if *fuel_ticks == 0 {
                    powered = 0;
                } else {
                    *fuel_ticks -= 1;
                    fuel_hit_zero = *fuel_ticks == 0;
                }
            }
        }
        self.remaining = self.remaining.saturating_sub(powered.saturating_mul(rate));
        if self.remaining == 0 {
            let recipe = self.recipes[index].clone();
            for stack in &recipe.inputs {
                let left = self.input[&stack.resource] - stack.amount;
                if left == 0 {
                    self.input.remove(&stack.resource);
                } else {
                    self.input.insert(stack.resource, left);
                }
            }
            for stack in &recipe.outputs {
                *self.output.entry(stack.resource).or_default() += stack.amount;
            }
            self.active = None;
            self.events.push(RecipeEvent {
                tick: self.now,
                machine,
                recipe: Some(recipe.id),
            });
            self.try_start();
        } else if fuel_hit_zero {
            self.events.push(RecipeEvent {
                tick: self.now,
                machine,
                recipe: None,
            });
        }
    }

    fn advance_to(&mut self, tick: u64, machine: u32) {
        while self.now < tick {
            self.tick(machine);
        }
    }

    fn next_due(&self) -> Option<u64> {
        self.active?;
        if self.remaining == 0 {
            return Some(self.now);
        }
        let rate = self.rate();
        if rate == 0 {
            return None;
        }
        let duration = self.remaining.div_ceil(rate);
        let delay = match self.drive {
            Drive::Mechanical { .. } => duration,
            Drive::Furnace { fuel_ticks } => duration.min(fuel_ticks),
        };
        self.now.checked_add(delay)
    }

    fn drain_events(&mut self) -> Vec<RecipeEvent> {
        std::mem::take(&mut self.events)
    }
}

fn run_recipe_pair(
    optimized: &mut RecipeProcessor,
    reference: &mut ReferenceRecipeMachine,
    machine: u32,
    tick: u64,
) {
    let mut optimized_events = Vec::new();
    while optimized.next_due().is_some_and(|due| due <= tick) {
        optimized_events.push(optimized.step().unwrap());
    }
    optimized.advance_idle_to(tick);
    reference.advance_to(tick, machine);
    assert_eq!(
        optimized_events,
        reference.drain_events(),
        "recipe events diverged by tick {tick}"
    );
    assert_eq!(
        optimized.next_due(),
        reference.next_due(),
        "next wake diverged"
    );
}

fn recipe_machine_reference(drive: Drive) -> ReferenceRecipeMachine {
    ReferenceRecipeMachine {
        input_capacity: 4,
        output_capacity: 1,
        recipes: vec![Recipe {
            id: 77,
            base_ticks: 10,
            inputs: vec![item(1, 1)],
            outputs: vec![item(2, 1)],
        }],
        drive,
        input: BTreeMap::new(),
        output: BTreeMap::new(),
        active: None,
        remaining: 0,
        now: 0,
        events: Vec::new(),
    }
}

fn mechanical_recipe() -> Recipe {
    Recipe {
        id: 77,
        base_ticks: 10,
        inputs: vec![item(1, 1)],
        outputs: vec![item(2, 1)],
    }
}

#[test]
fn due_process_recipes_match_per_tick_speed_backpressure_and_fuel_models() {
    let mut optimized = RecipeProcessor::default();
    optimized
        .add_machine(
            3,
            4,
            1,
            vec![mechanical_recipe()],
            Drive::Mechanical {
                rpm: 16,
                overstressed: false,
            },
        )
        .unwrap();
    let mut reference = recipe_machine_reference(Drive::Mechanical {
        rpm: 16,
        overstressed: false,
    });
    assert_eq!(optimized.insert(3, item(1, 3)), Ok(3));
    assert_eq!(reference.insert(3, item(1, 3)), 3);
    assert_eq!(optimized.next_due(), reference.next_due());

    // Variable RPM preserves already-accrued fixed-point work exactly.
    run_recipe_pair(&mut optimized, &mut reference, 3, 3);
    optimized
        .set_drive(
            3,
            Drive::Mechanical {
                rpm: 48,
                overstressed: false,
            },
        )
        .unwrap();
    reference.drive = Drive::Mechanical {
        rpm: 48,
        overstressed: false,
    };
    run_recipe_pair(&mut optimized, &mut reference, 3, 6);
    assert_eq!(optimized.extract(3, item(2, 1)), Ok(1));
    assert_eq!(reference.extract(item(2, 1)), 1);

    // Output backpressure suppresses even a speculative wake. Extracting the
    // product is the exact wake event in both models.
    run_recipe_pair(&mut optimized, &mut reference, 3, 40);
    assert_eq!(optimized.next_due(), None);
    assert_eq!(optimized.extract(3, item(2, 1)), Ok(1));
    assert_eq!(reference.extract(item(2, 1)), 1);
    assert_eq!(optimized.next_due(), reference.next_due());
    run_recipe_pair(&mut optimized, &mut reference, 3, 100);

    // Overstress freezes progress without forgetting it; recovery finishes at
    // the same tick as the scalar per-tick loop.
    optimized
        .set_drive(
            3,
            Drive::Mechanical {
                rpm: 64,
                overstressed: true,
            },
        )
        .unwrap();
    reference.drive = Drive::Mechanical {
        rpm: 64,
        overstressed: true,
    };
    assert_eq!(optimized.next_due(), None);
    run_recipe_pair(&mut optimized, &mut reference, 3, 140);
    optimized
        .set_drive(
            3,
            Drive::Mechanical {
                rpm: 32,
                overstressed: false,
            },
        )
        .unwrap();
    reference.drive = Drive::Mechanical {
        rpm: 32,
        overstressed: false,
    };
    run_recipe_pair(&mut optimized, &mut reference, 3, 200);

    // Furnace drive: fuel exhaustion emits the same non-completion wake, a
    // long idle gap costs nothing, and refuelling resumes only the residue.
    let mut furnace = RecipeProcessor::default();
    furnace
        .add_machine(
            4,
            4,
            1,
            vec![mechanical_recipe()],
            Drive::Furnace { fuel_ticks: 7 },
        )
        .unwrap();
    let mut furnace_ref = recipe_machine_reference(Drive::Furnace { fuel_ticks: 7 });
    assert_eq!(furnace.insert(4, item(1, 1)), Ok(1));
    assert_eq!(furnace_ref.insert(4, item(1, 1)), 1);
    run_recipe_pair(&mut furnace, &mut furnace_ref, 4, 7);
    assert_eq!(furnace.next_due(), None);
    run_recipe_pair(&mut furnace, &mut furnace_ref, 4, 10_000);
    furnace
        .set_drive(4, Drive::Furnace { fuel_ticks: 3 })
        .unwrap();
    furnace_ref.drive = Drive::Furnace { fuel_ticks: 3 };
    run_recipe_pair(&mut furnace, &mut furnace_ref, 4, 10_003);
    assert_eq!(furnace.extract(4, item(2, 1)), Ok(1));
    assert_eq!(furnace_ref.extract(item(2, 1)), 1);
}

// ---------------------------------------------------------------------------
// Fluid conservation: dirty components versus full-network equilibrium
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Default)]
struct RefTank {
    capacity: u32,
    elevation: i32,
    fluid: Option<u32>,
    amount: u32,
}

struct ReferenceFluid {
    tanks: BTreeMap<u32, RefTank>,
    links: BTreeMap<u32, BTreeMap<u32, i64>>,
}

impl ReferenceFluid {
    fn add_tank(&mut self, id: u32, capacity: u32, elevation: i32) {
        self.tanks.insert(
            id,
            RefTank {
                capacity,
                elevation,
                fluid: None,
                amount: 0,
            },
        );
        self.links.entry(id).or_default();
    }

    fn set_tank(&mut self, id: u32, capacity: u32, elevation: i32) {
        let tank = self.tanks.get_mut(&id).unwrap();
        tank.capacity = capacity;
        tank.elevation = elevation;
    }

    fn set_link(&mut self, a: u32, b: u32, head: Option<i32>) {
        match head {
            Some(head) => {
                self.links.get_mut(&a).unwrap().insert(b, i64::from(head));
                self.links.get_mut(&b).unwrap().insert(a, -i64::from(head));
            }
            None => {
                self.links.get_mut(&a).unwrap().remove(&b);
                self.links.get_mut(&b).unwrap().remove(&a);
            }
        }
    }

    fn insert(&mut self, id: u32, fluid: u32, amount: u32) -> u32 {
        let tank = self.tanks.get_mut(&id).unwrap();
        let accepted = amount.min(tank.capacity - tank.amount);
        if accepted > 0 {
            tank.fluid = Some(fluid);
            tank.amount += accepted;
        }
        accepted
    }

    fn extract(&mut self, id: u32, amount: u32) -> u32 {
        let tank = self.tanks.get_mut(&id).unwrap();
        let extracted = tank.amount.min(amount);
        tank.amount -= extracted;
        if tank.amount == 0 {
            tank.fluid = None;
        }
        extracted
    }

    fn solve_all(&mut self) {
        let mut unseen: BTreeSet<_> = self.tanks.keys().copied().collect();
        while let Some(start) = unseen.first().copied() {
            let mut offsets = BTreeMap::from([(start, 0i128)]);
            let mut queue = VecDeque::from([start]);
            while let Some(id) = queue.pop_front() {
                let offset = offsets[&id];
                for (&other, &head) in &self.links[&id] {
                    if let std::collections::btree_map::Entry::Vacant(entry) = offsets.entry(other)
                    {
                        entry.insert(offset + i128::from(head));
                        queue.push_back(other);
                    }
                }
            }
            let mut ids: Vec<_> = offsets.keys().copied().collect();
            ids.sort_unstable();
            for &id in &ids {
                unseen.remove(&id);
            }
            let mut volume = 0u64;
            let mut capacity = 0u64;
            let mut fluid = None;
            let mut columns = Vec::new();
            for &id in &ids {
                let tank = self.tanks[&id];
                volume += u64::from(tank.amount);
                capacity += u64::from(tank.capacity);
                fluid = fluid.or(tank.fluid);
                columns.push((id, i128::from(tank.elevation) - offsets[&id], tank.capacity));
            }
            if columns.len() == 1 || volume == 0 || volume == capacity {
                continue;
            }
            let filled =
                |level: i128, base: i128, cap: u32| (level - base).clamp(0, i128::from(cap)) as u32;
            let mut low = columns[0].1;
            let mut high = columns[0].1 + i128::from(columns[0].2);
            for &(_, base, cap) in &columns[1..] {
                low = low.min(base);
                high = high.max(base + i128::from(cap));
            }
            while high - low > 1 {
                let mid = low + (high - low) / 2;
                let total = columns
                    .iter()
                    .map(|&(_, base, cap)| u64::from(filled(mid, base, cap)))
                    .sum::<u64>();
                if total <= volume {
                    low = mid;
                } else {
                    high = mid;
                }
            }
            let mut remainder = volume
                - columns
                    .iter()
                    .map(|&(_, base, cap)| u64::from(filled(low, base, cap)))
                    .sum::<u64>();
            for &(id, base, cap) in &columns {
                let mut amount = filled(low, base, cap);
                if remainder > 0 && filled(high, base, cap) > amount {
                    amount += 1;
                    remainder -= 1;
                }
                let tank = self.tanks.get_mut(&id).unwrap();
                tank.amount = amount;
                tank.fluid = fluid.filter(|_| amount > 0);
            }
            assert_eq!(remainder, 0);
        }
    }
}

fn assert_fluid_pair(optimized: &FluidNetwork, reference: &ReferenceFluid, ids: &[u32]) {
    for &id in ids {
        let got = optimized.tank(id).unwrap();
        let tank = reference.tanks[&id];
        assert_eq!(
            (got.fluid, got.amount),
            (tank.fluid, tank.amount),
            "tank {id} diverged"
        );
    }
}

fn solve_all_optimized(network: &mut FluidNetwork) -> usize {
    let mut visited = 0;
    while let Some(step) = network.step() {
        visited += step.visited_nodes;
    }
    visited
}

#[test]
fn fluid_dirty_only_resolution_matches_full_conservation_solve() {
    let mut optimized = FluidNetwork::default();
    let mut reference = ReferenceFluid {
        tanks: BTreeMap::new(),
        links: BTreeMap::new(),
    };
    let ids = [1, 2, 3, 4, 5];
    for (id, capacity, elevation) in [(1, 10, 0), (2, 10, 2), (3, 12, -1), (4, 8, 5), (5, 20, 0)] {
        optimized.add_tank(id, capacity, elevation).unwrap();
        reference.add_tank(id, capacity, elevation);
    }
    for (a, b, head) in [(1, 2, 4), (2, 3, 0), (3, 4, -2)] {
        optimized.set_link(a, b, Some(head)).unwrap();
        reference.set_link(a, b, Some(head));
    }
    assert_eq!(optimized.insert(1, 9, 27), Ok(10));
    assert_eq!(reference.insert(1, 9, 27), 10);
    let dirty_visits = solve_all_optimized(&mut optimized);
    reference.solve_all();
    assert!(dirty_visits > 0);
    assert!(dirty_visits < 5, "dirty solve scanned unrelated tank 5");
    assert_fluid_pair(&optimized, &reference, &ids);
    let conserved: u32 = ids[..4]
        .iter()
        .map(|id| optimized.tank(*id).unwrap().amount)
        .sum();
    assert_eq!(conserved, 10);
    assert_eq!(optimized.step(), None, "clean fluid network did not sleep");

    // Elevation/capacity edits re-dirty exactly the affected component and
    // preserve volume bit-for-bit.
    optimized.set_tank(2, 10, 7).unwrap();
    reference.set_tank(2, 10, 7);
    solve_all_optimized(&mut optimized);
    reference.solve_all();
    assert_fluid_pair(&optimized, &reference, &ids);
    let conserved: u32 = ids[..4]
        .iter()
        .map(|id| optimized.tank(*id).unwrap().amount)
        .sum();
    assert_eq!(conserved, 10);

    // Extraction and reinsertion are conservation events, and a link split
    // allows the detached component to hold an incompatible fluid.
    assert_eq!(optimized.extract(1, 9), Ok(3));
    assert_eq!(reference.extract(1, 9), 3);
    assert_eq!(optimized.extract(3, 9), Ok(7));
    assert_eq!(reference.extract(3, 9), 7);
    optimized.set_link(2, 3, None).unwrap();
    reference.set_link(2, 3, None);
    assert_eq!(optimized.insert(3, 11, 6), Ok(6));
    assert_eq!(reference.insert(3, 11, 6), 6);
    solve_all_optimized(&mut optimized);
    reference.solve_all();
    assert_fluid_pair(&optimized, &reference, &ids);
    assert_eq!(optimized.tank(3).unwrap().fluid, Some(11));
    assert_eq!(optimized.step(), None);
}

// ---------------------------------------------------------------------------
// ME channel allocation and lazy catch-up
// ---------------------------------------------------------------------------

#[test]
fn me_dirty_resolution_is_canonical_and_repeatable() {
    let mut graph = MeChannelGraph::default();
    graph.add_node(0, MeNodeKind::Controller).unwrap();
    graph.add_node(1, MeNodeKind::DenseCable).unwrap();
    graph.add_node(2, MeNodeKind::NormalCable).unwrap();
    for id in 10..14 {
        graph
            .add_node(
                id,
                MeNodeKind::Device {
                    requires_channel: true,
                },
            )
            .unwrap();
    }
    graph.set_link(0, 1, true).unwrap();
    graph.set_link(1, 2, true).unwrap();
    for id in 10..14 {
        graph.set_link(2, id, true).unwrap();
    }

    let first = graph.solve_channels();
    let second = graph.solve_channels();
    assert_eq!(first, second, "clean re-solve is not bit-identical");
    assert_eq!(
        first.online_devices,
        [10, 11, 12, 13].into_iter().collect::<BTreeSet<_>>()
    );
    assert!(first.channel_starved_devices.is_empty());
    assert_eq!(first.used_channels[&1], 4);
    assert_eq!(first.used_channels[&2], 4);
    assert!(first.conflicts.is_empty());

    // Ad-hoc controller-less allocation is capped deterministically at eight.
    let mut adhoc = MeChannelGraph::default();
    for id in 0..12 {
        adhoc
            .add_node(
                id,
                MeNodeKind::Device {
                    requires_channel: true,
                },
            )
            .unwrap();
        if id > 0 {
            adhoc.set_link(id - 1, id, true).unwrap();
        }
    }
    let allocation = adhoc.solve_channels();
    assert_eq!(
        allocation.online_devices,
        (0..8).collect::<BTreeSet<_>>(),
        "first eight IDs must win the ad-hoc cap"
    );
    assert_eq!(
        allocation.channel_starved_devices,
        (8..12).collect::<BTreeSet<_>>()
    );
}

fn ref_furnace_step(state: &mut FurnaceCatchUpState) -> (u64, u64) {
    let total = state.cook_time_total.max(1);
    let mut smelted = 0;
    let mut fuel = 0;
    if state.current_burn_remaining == 0
        && state.fuel_count > 0
        && state.input_count > 0
        && state.output_count < state.output_max_stack
    {
        state.fuel_count -= 1;
        state.current_burn_remaining = state.fuel_burn_ticks_each;
        fuel += 1;
    }
    if state.current_burn_remaining > 0
        && state.input_count > 0
        && state.output_count < state.output_max_stack
    {
        state.cook_progress += 1;
        if state.cook_progress >= total {
            state.cook_progress -= total;
            state.input_count -= 1;
            state.output_count += 1;
            smelted += 1;
        }
    }
    if state.current_burn_remaining > 0 {
        state.current_burn_remaining -= 1;
    }
    (smelted, fuel)
}

fn ref_furnace_run(mut state: FurnaceCatchUpState, ticks: u64) -> (FurnaceCatchUpState, u64, u64) {
    let (mut smelted, mut fuel) = (0, 0);
    for _ in 0..ticks {
        let (s, f) = ref_furnace_step(&mut state);
        smelted += s;
        fuel += f;
    }
    (state, smelted, fuel)
}

fn ref_machine_run(mut state: MachineCatchUpState, ticks: u64) -> (MachineCatchUpState, u64) {
    let mut cycles = 0;
    for _ in 0..ticks {
        let tpc = state.ticks_per_cycle.max(1);
        state.cycle_progress += 1;
        if state.cycle_progress >= tpc {
            state.cycle_progress -= tpc;
            if state.input_count > 0
                && state.output_space > 0
                && (state.energy_per_cycle == 0 || state.energy >= state.energy_per_cycle)
            {
                state.input_count -= 1;
                state.output_space -= 1;
                state.output_count += 1;
                state.energy = state.energy.saturating_sub(state.energy_per_cycle);
                cycles += 1;
            }
        }
    }
    (state, cycles)
}

#[test]
fn lazy_furnace_and_machine_catchup_match_one_tick_loops_across_stalls() {
    let furnace_cases = [
        FurnaceCatchUpState {
            input_count: 12,
            fuel_count: 2,
            fuel_burn_ticks_each: 40,
            cook_time_total: 20,
            output_max_stack: 64,
            ..Default::default()
        },
        // One output slot left: the final smelt completes, then the stack is
        // full and the burn coasts down without inventing another smelt.
        FurnaceCatchUpState {
            input_count: 9,
            fuel_count: 4,
            fuel_burn_ticks_each: 100,
            current_burn_remaining: 37,
            cook_progress: 18,
            cook_time_total: 20,
            output_count: 63,
            output_max_stack: 64,
        },
        // Fuel exhaustion mid-cook with a long headless idle gap.
        FurnaceCatchUpState {
            input_count: 64,
            fuel_count: 1,
            fuel_burn_ticks_each: 30,
            current_burn_remaining: 5,
            cook_progress: 17,
            cook_time_total: 200,
            output_max_stack: 64,
            ..Default::default()
        },
    ];
    for state in furnace_cases {
        for delta in [1, 7, 29, 30, 31, 200, 5_000, 72_000] {
            let got = catch_up_furnace(&limits(), &state, delta);
            let (want, smelted, fuel) = ref_furnace_run(state, delta);
            assert_eq!(got.state, want, "furnace diverged for {state:?} d{delta}");
            assert_eq!(got.items_smelted, smelted);
            assert_eq!(got.fuel_consumed, fuel);
        }
    }

    let machine_cases = [
        MachineCatchUpState {
            input_count: 17,
            energy: 91,
            energy_per_cycle: 7,
            ticks_per_cycle: 11,
            cycle_progress: 4,
            output_count: 2,
            output_space: 20,
        },
        // Energy covers only a prefix of the periodic slots; later starved
        // slots are wasted exactly like the per-tick loop.
        MachineCatchUpState {
            input_count: 100,
            energy: 20,
            energy_per_cycle: 10,
            ticks_per_cycle: 3,
            output_space: 100,
            ..Default::default()
        },
        // Output-full stall: zero completion but the cycle phase still advances.
        MachineCatchUpState {
            input_count: 100,
            energy: 1_000,
            energy_per_cycle: 1,
            ticks_per_cycle: 5,
            output_space: 0,
            ..Default::default()
        },
    ];
    for state in machine_cases {
        for delta in [1, 4, 5, 11, 101, 5_000, 72_000] {
            let got = catch_up_machine_batch(&limits(), &state, delta);
            let (want, cycles) = ref_machine_run(state, delta);
            assert_eq!(got.state, want, "machine diverged for {state:?} d{delta}");
            assert_eq!(got.cycles_completed, cycles);
            assert_eq!(
                state.energy - got.state.energy,
                got.energy_consumed,
                "machine energy conservation diverged"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Router-level dormancy and structural coverage
// ---------------------------------------------------------------------------

#[test]
fn router_routes_all_six_shapes_and_clean_steady_ticks_cost_zero() {
    let mut router = ModKernelRouter::default();
    let profile = DeclarativeProfile {
        features: vec![RouterFeatureProfile {
            feature_id: "generic_factory".into(),
            network_domain: Some(GraphDomain::Kinetic),
            processing_recipes: true,
            sleep_on_idle: true,
            subworld_assembly: true,
            resource_ports: true,
            track_node: true,
            action_timeline: true,
        }],
    };
    let report = router
        .register_mod_ir(
            "generic_mod",
            profile.lower(),
            RoutingBudget {
                max_features_per_mod: 8,
                max_total_routes: 32,
                max_nodes_per_tick: 32,
            },
        )
        .unwrap();
    assert!(report.unrouted_features.is_empty());
    assert_eq!(
        report
            .routed
            .iter()
            .map(|binding| binding.target)
            .collect::<BTreeSet<_>>(),
        [
            UniversalKernelKind::GraphNetwork(GraphDomain::Kinetic),
            UniversalKernelKind::DueProcessMachine,
            UniversalKernelKind::Subworld6Dof,
            UniversalKernelKind::ConservationTransfer,
            UniversalKernelKind::TrackGraph,
            UniversalKernelKind::TimelineAction,
        ]
        .into_iter()
        .collect()
    );

    // Exercise the router-owned wake paths that exist today: transport,
    // recipes, ME, an action timeline, and an idle furnace.
    assert!(router.attach_sleeping_transport("generic_mod", 1, 2));
    assert!(router.insert_transport_item("generic_mod", 1, 42));
    router
        .attach_recipe_machine(
            "generic_mod",
            2,
            1,
            1,
            vec![mechanical_recipe()],
            Drive::Mechanical {
                rpm: 16,
                overstressed: false,
            },
        )
        .unwrap();
    assert_eq!(
        router
            .insert_recipe_input("generic_mod", 2, item(1, 1))
            .unwrap(),
        1
    );
    assert!(router.attach_me_node("generic_mod", 10, MeNodeKind::Controller));
    assert!(router.attach_action(
        "generic_mod",
        ActionSpec {
            action_id: 9,
            anticipation_ticks: 0,
            active_ticks: 1,
            recovery_ticks: 1,
            cancel_after_tick: None,
            invulnerable_window: None,
            movement_speed_factor: 1.0,
            hit_shape: HitShape::RadialAoe { radius: 1.0 },
        },
        0,
    ));
    router.attach_furnace_catchup("generic_mod", FurnaceCatchUpState::default(), 0);

    let budget = RoutingBudget {
        max_features_per_mod: 8,
        max_total_routes: 32,
        max_nodes_per_tick: 32,
    };
    let active = router.tick_routed_kernels(10, budget);
    assert!(
        active.woken >= 2,
        "scheduled transport/recipe work did not wake"
    );
    assert!(active.routed_nodes_ticked > 0);

    // Once due work and the short action finish, a second identical tick shape
    // performs zero node work. This is the router-level dormant-shape proof.
    let steady = router.tick_routed_kernels(11, budget);
    assert_eq!(steady.routed_nodes_ticked, 0);
    assert_eq!(steady.woken, 0);
    assert!(!steady.budget_exhausted);
}
