#![forbid(unsafe_code)]

//! Bounded, deterministic simulation primitives for transport lines, connected
//! kinetic networks, and sleeping machine schedules.
//!
//! Values representing distance and rotation are integer fixed-point units.
//! These kernels are deliberately independent of Create's world/block APIs so
//! adapters can map belts, shafts, pipes, and contraption regions onto them.

use std::collections::HashMap;

pub type NodeId = u32;

/// One item and its gap from the preceding item (or the output boundary).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BeltEntry<Item> {
    pub gap_units: u32,
    pub item: Item,
}

/// A bounded transport line. `gap_units` is relative, so a free-flow tick only
/// changes the leading gap; all following item spacings remain unchanged.
pub struct TransportLine<Item> {
    entries: Vec<Option<BeltEntry<Item>>>,
    head: usize,
    live_len: usize,
    capacity_units: u32,
    speed_units: i32,
    output_blocked: bool,
    sleeping: bool,
}

impl<Item> TransportLine<Item> {
    pub fn with_capacity(capacity_units: u32) -> Self {
        assert!(capacity_units > 0, "transport capacity must be nonzero");
        Self {
            entries: Vec::with_capacity(capacity_units as usize),
            head: 0,
            live_len: 0,
            capacity_units,
            speed_units: 0,
            output_blocked: false,
            sleeping: true,
        }
    }

    pub fn len(&self) -> usize {
        self.live_len
    }
    pub fn is_empty(&self) -> bool {
        self.live_len == 0
    }
    pub fn entry(&self, index: usize) -> Option<&BeltEntry<Item>> {
        self.entries.get(self.head + index).and_then(Option::as_ref)
    }
    pub fn speed(&self) -> i32 {
        self.speed_units
    }
    pub fn set_output_blocked(&mut self, blocked: bool) {
        self.output_blocked = blocked;
        self.refresh_sleep();
    }

    /// Adds at the tail if one fixed-point item pitch and its gap fit.
    pub fn push_back(&mut self, item: Item) -> Result<(), Item> {
        if self.live_len >= self.capacity_units as usize {
            return Err(item);
        }
        let gap_units = 1;
        if self.head > 0 && self.entries.len() == self.entries.capacity() {
            self.entries.drain(..self.head);
            self.head = 0;
        }
        if self.entries.len() == self.entries.capacity() {
            return Err(item);
        }
        self.entries.push(Some(BeltEntry { gap_units, item }));
        self.live_len += 1;
        self.sleeping = false;
        Ok(())
    }

    pub fn pop_front(&mut self) -> Option<Item> {
        if self.live_len == 0 {
            return None;
        }
        let first = self.entries[self.head].take().expect("live line entry");
        self.head += 1;
        self.live_len -= 1;
        self.refresh_sleep();
        Some(first.item)
    }

    pub fn set_speed(&mut self, units_per_tick: i32) {
        self.speed_units = units_per_tick;
        self.refresh_sleep();
    }

    /// Advances only the head gap, O(1) independently of item count. Returns
    /// one unit of work when movement was attempted, zero when asleep.
    pub fn tick_step(&mut self) -> usize {
        if self.sleeping {
            return 0;
        }
        if self.live_len == 0 || self.speed_units == 0 {
            self.sleeping = true;
            return 0;
        }
        let head = self.entries[self.head].as_mut().expect("live line head");
        if self.speed_units > 0 {
            if self.output_blocked && head.gap_units == 0 {
                self.sleeping = true;
                return 0;
            }
            let step = self.speed_units as u32;
            let moved = step.min(head.gap_units);
            head.gap_units -= moved;
            if head.gap_units == 0 && self.output_blocked {
                self.sleeping = true;
            }
        } else {
            let step = self.speed_units.unsigned_abs();
            let room = self.capacity_units.saturating_sub(head.gap_units);
            let moved = step.min(room);
            head.gap_units = head.gap_units.saturating_add(moved);
            if moved == room {
                self.sleeping = true;
            }
        }
        1
    }

    pub fn is_sleeping(&self) -> bool {
        self.sleeping
    }

    fn refresh_sleep(&mut self) {
        self.sleeping = self.live_len == 0
            || self.speed_units == 0
            || (self.output_blocked
                && self.speed_units > 0
                && self.entry(0).is_some_and(|entry| entry.gap_units == 0))
            || (self.speed_units < 0
                && self
                    .entry(0)
                    .is_some_and(|entry| entry.gap_units >= self.capacity_units));
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct NetworkSolution {
    pub component_id: NodeId,
    pub rpm_fixed: i64,
    pub total_stress: i64,
    pub total_capacity: i64,
    pub overstressed: bool,
}

/// Dense node columns and reusable BFS scratch for deterministic connected
/// component solves. Topology/input edits solve synchronously; steady ticks are
/// cached and visit no nodes.
#[derive(Default)]
pub struct SteadyNetworkGraph<NodeMeta> {
    pub node_ids: Vec<NodeId>,
    pub net_ids: Vec<NodeId>,
    pub rpm_fixed: Vec<i64>,
    pub stress_impact: Vec<i64>,
    pub stress_capacity: Vec<i64>,
    pub dirty_flags: Vec<bool>,
    pub sleeping_nets: Vec<bool>,
    metadata: Vec<NodeMeta>,
    adjacency: Vec<Vec<usize>>,
    solutions: Vec<NetworkSolution>,
    indices: HashMap<NodeId, usize>,
    queue: Vec<usize>,
    members: Vec<usize>,
    seen: Vec<u32>,
    generation: u32,
    last_solve_visits: usize,
}

impl<NodeMeta> SteadyNetworkGraph<NodeMeta> {
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            node_ids: Vec::with_capacity(capacity),
            net_ids: Vec::with_capacity(capacity),
            rpm_fixed: Vec::with_capacity(capacity),
            stress_impact: Vec::with_capacity(capacity),
            stress_capacity: Vec::with_capacity(capacity),
            dirty_flags: Vec::with_capacity(capacity),
            sleeping_nets: Vec::with_capacity(capacity),
            metadata: Vec::with_capacity(capacity),
            adjacency: Vec::with_capacity(capacity),
            solutions: Vec::with_capacity(capacity),
            indices: HashMap::with_capacity(capacity),
            queue: Vec::with_capacity(capacity),
            members: Vec::with_capacity(capacity),
            seen: Vec::with_capacity(capacity),
            generation: 0,
            last_solve_visits: 0,
        }
    }

    pub fn add_node(
        &mut self,
        id: NodeId,
        rpm_fixed: i64,
        impact: i64,
        capacity: i64,
        meta: NodeMeta,
    ) -> bool {
        if self.indices.contains_key(&id) || impact < 0 || capacity < 0 {
            return false;
        }
        let index = self.node_ids.len();
        self.indices.insert(id, index);
        self.node_ids.push(id);
        self.net_ids.push(id);
        self.rpm_fixed.push(rpm_fixed);
        self.stress_impact.push(impact);
        self.stress_capacity.push(capacity);
        self.dirty_flags.push(true);
        self.sleeping_nets.push(false);
        self.metadata.push(meta);
        self.adjacency.push(Vec::new());
        self.solutions.push(NetworkSolution::default());
        self.seen.push(0);
        self.solve_component(index);
        true
    }

    pub fn metadata(&self, id: NodeId) -> Option<&NodeMeta> {
        self.indices.get(&id).map(|&i| &self.metadata[i])
    }
    pub fn last_edit_nodes_visited(&self) -> usize {
        self.last_solve_visits
    }

    pub fn set_node(&mut self, id: NodeId, rpm_fixed: i64, impact: i64, capacity: i64) -> bool {
        let Some(&i) = self.indices.get(&id) else {
            return false;
        };
        if impact < 0 || capacity < 0 {
            return false;
        }
        if self.rpm_fixed[i] == rpm_fixed
            && self.stress_impact[i] == impact
            && self.stress_capacity[i] == capacity
        {
            return true;
        }
        self.rpm_fixed[i] = rpm_fixed;
        self.stress_impact[i] = impact;
        self.stress_capacity[i] = capacity;
        self.dirty_flags[i] = true;
        self.solve_component(i);
        true
    }

    pub fn connect(&mut self, a: NodeId, b: NodeId) -> bool {
        let (Some(&ia), Some(&ib)) = (self.indices.get(&a), self.indices.get(&b)) else {
            return false;
        };
        if ia == ib || self.adjacency[ia].contains(&ib) {
            return false;
        }
        self.adjacency[ia].push(ib);
        self.adjacency[ib].push(ia);
        self.adjacency[ia].sort_unstable_by_key(|&i| self.node_ids[i]);
        self.adjacency[ib].sort_unstable_by_key(|&i| self.node_ids[i]);
        self.solve_component(ia);
        true
    }

    pub fn disconnect(&mut self, a: NodeId, b: NodeId) -> bool {
        let (Some(&ia), Some(&ib)) = (self.indices.get(&a), self.indices.get(&b)) else {
            return false;
        };
        let before = self.adjacency[ia].len();
        self.adjacency[ia].retain(|&i| i != ib);
        self.adjacency[ib].retain(|&i| i != ia);
        if before == self.adjacency[ia].len() {
            return false;
        }
        let first_visits = self.solve_component(ia);
        if self.seen[ib] != self.generation {
            let second_visits = self.solve_component(ib);
            self.last_solve_visits = first_visits + second_visits;
        }
        true
    }

    /// Returns cached solutions for all current component IDs.
    pub fn tick(&mut self) -> usize {
        0
    }

    pub fn solution(&self, id: NodeId) -> Option<NetworkSolution> {
        self.indices.get(&id).map(|&i| self.solutions[i])
    }

    pub fn state_hash(&self) -> u64 {
        let mut order: Vec<usize> = (0..self.node_ids.len()).collect();
        order.sort_unstable_by_key(|&i| self.node_ids[i]);
        let mut hash = 0xcbf29ce484222325u64;
        for i in order {
            for value in [
                self.node_ids[i] as u64,
                self.net_ids[i] as u64,
                self.rpm_fixed[i] as u64,
                self.stress_impact[i] as u64,
                self.stress_capacity[i] as u64,
                self.solutions[i].total_stress as u64,
                self.solutions[i].total_capacity as u64,
                u64::from(self.solutions[i].overstressed),
            ] {
                hash ^= value;
                hash = hash.wrapping_mul(0x100000001b3);
            }
        }
        hash
    }

    fn solve_component(&mut self, start: usize) -> usize {
        self.generation = self.generation.wrapping_add(1);
        if self.generation == 0 {
            self.seen.fill(0);
            self.generation = 1;
        }
        let generation = self.generation;
        self.queue.clear();
        self.members.clear();
        self.queue.push(start);
        self.seen[start] = generation;
        let mut cursor = 0;
        while cursor < self.queue.len() {
            let i = self.queue[cursor];
            cursor += 1;
            self.members.push(i);
            for &next in &self.adjacency[i] {
                if self.seen[next] != generation {
                    self.seen[next] = generation;
                    self.queue.push(next);
                }
            }
        }
        let component = self
            .members
            .iter()
            .map(|&i| self.node_ids[i])
            .min()
            .unwrap();
        let mut total_stress: i64 = 0;
        let mut total_capacity: i64 = 0;
        let mut rpm = 0;
        let mut rpm_driver = NodeId::MAX;
        for &i in &self.members {
            total_stress = total_stress.saturating_add(self.stress_impact[i]);
            total_capacity = total_capacity.saturating_add(self.stress_capacity[i]);
            if self.rpm_fixed[i] != 0 && self.node_ids[i] < rpm_driver {
                rpm = self.rpm_fixed[i];
                rpm_driver = self.node_ids[i];
            }
        }
        let solution = NetworkSolution {
            component_id: component,
            rpm_fixed: rpm,
            total_stress,
            total_capacity,
            overstressed: total_stress > total_capacity,
        };
        for &i in &self.members {
            self.net_ids[i] = component;
            self.solutions[i] = solution;
            self.dirty_flags[i] = false;
            self.sleeping_nets[i] = true;
        }
        self.last_solve_visits = self.members.len();
        self.members.len()
    }
}

/// Reusable index scratch for simulation owners that need explicit per-tick
/// staging without allocating a fresh temporary vector.
#[derive(Default)]
pub struct TickScratch {
    pub indices: Vec<usize>,
}
impl TickScratch {
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            indices: Vec::with_capacity(capacity),
        }
    }
    pub fn clear(&mut self) {
        self.indices.clear();
    }
}

/// Dense generic machine slots with a fixed-size deadline wheel. Callbacks
/// return the next deadline, or `None` to put a slot fully to sleep.
pub struct ActiveSlotPool<T> {
    values: Vec<T>,
    deadlines: Vec<Option<u64>>,
    active: Vec<usize>,
    wheel: Vec<Vec<usize>>,
    due_scratch: Vec<usize>,
    now: u64,
    capacity: usize,
}

impl<T> ActiveSlotPool<T> {
    pub fn with_capacity(capacity: usize, wheel_size: usize) -> Self {
        assert!(wheel_size > 0);
        Self {
            values: Vec::with_capacity(capacity),
            deadlines: Vec::with_capacity(capacity),
            active: Vec::with_capacity(capacity),
            wheel: (0..wheel_size)
                .map(|_| Vec::with_capacity(capacity))
                .collect(),
            due_scratch: Vec::with_capacity(capacity),
            now: 0,
            capacity,
        }
    }
    pub fn insert(&mut self, value: T) -> Option<usize> {
        if self.values.len() == self.capacity {
            return None;
        }
        let i = self.values.len();
        self.values.push(value);
        self.deadlines.push(None);
        Some(i)
    }
    pub fn wake(&mut self, i: usize) -> bool {
        if i >= self.values.len() || self.deadlines[i] == Some(u64::MAX) {
            return false;
        }
        self.deadlines[i] = Some(u64::MAX);
        self.active.push(i);
        true
    }
    pub fn schedule(&mut self, i: usize, deadline: u64) -> bool {
        if i >= self.values.len() {
            return false;
        }
        self.deadlines[i] = Some(deadline);
        let bucket = deadline as usize % self.wheel.len();
        self.wheel[bucket].push(i);
        true
    }
    pub fn tick(&mut self, now: u64, mut step: impl FnMut(usize, &mut T) -> Option<u64>) -> usize {
        assert!(now >= self.now, "tick time must be monotonic");
        self.now = now;
        let wheel_len = self.wheel.len();
        let bucket_index = now as usize % wheel_len;
        std::mem::swap(&mut self.wheel[bucket_index], &mut self.due_scratch);
        for position in 0..self.due_scratch.len() {
            let i = self.due_scratch[position];
            if self.deadlines[i].is_some_and(|d| d != u64::MAX && d <= now) {
                self.deadlines[i] = Some(u64::MAX);
                self.active.push(i);
            } else if let Some(d) = self.deadlines[i] {
                if d != u64::MAX {
                    self.wheel[d as usize % wheel_len].push(i);
                }
            }
        }
        self.due_scratch.clear();
        let count = self.active.len();
        for position in 0..count {
            let i = self.active[position];
            match step(i, &mut self.values[i]) {
                Some(deadline) => {
                    self.deadlines[i] = Some(deadline);
                    self.wheel[deadline as usize % wheel_len].push(i);
                }
                None => self.deadlines[i] = None,
            }
        }
        self.active.clear();
        count
    }
    pub fn len(&self) -> usize {
        self.values.len()
    }
    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }
    pub fn get(&self, i: usize) -> Option<&T> {
        self.values.get(i)
    }
    /// Total retained index capacity across the dense and wake scratch buffers.
    pub fn reserved_index_capacity(&self) -> usize {
        self.values.capacity()
            + self.deadlines.capacity()
            + self.active.capacity()
            + self.due_scratch.capacity()
            + self.wheel.iter().map(Vec::capacity).sum::<usize>()
    }
}
