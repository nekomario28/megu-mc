// SPDX-License-Identifier: LGPL-3.0-or-later

//! Kinetic topology and cached component solutions. Edits solve synchronously and
//! only traverse adjacent edges of affected components.

use std::collections::{
    btree_map::Entry as BTreeEntry, hash_map::Entry, BTreeMap, BTreeSet, HashMap, VecDeque,
};

/// Capacity and load are additive stress units in a common frame. A mod adapter
/// can normalize its own speed-dependent rules before supplying these values.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct KineticNode {
    pub capacity: f64,
    pub load: f64,
    pub driven_speed: Option<f64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NetworkStatus {
    Unpowered,
    Running,
    Overstressed,
    Conflict,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Solution {
    pub component: u32,
    pub capacity: f64,
    pub load: f64,
    pub speed: f64,
    pub status: NetworkStatus,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GraphError {
    DuplicateNode(u32),
    MissingNode(u32),
    InvalidNode(u32),
    InvalidRatio,
    SelfEdge,
    DuplicateEdge,
    MissingEdge,
}

struct Node {
    input: KineticNode,
    edges: BTreeMap<u32, f64>,
    solution: Solution,
}

#[derive(Default)]
pub struct KineticGraph {
    nodes: HashMap<u32, Node>,
    scratch_scales: BTreeMap<u32, f64>,
    scratch_queue: VecDeque<u32>,
    scratch_members: Vec<u32>,
}

impl KineticGraph {
    pub fn add_node(&mut self, id: u32, input: KineticNode) -> Result<(), GraphError> {
        validate(id, input)?;
        let Entry::Vacant(entry) = self.nodes.entry(id) else {
            return Err(GraphError::DuplicateNode(id));
        };
        entry.insert(Node {
            input,
            edges: BTreeMap::new(),
            solution: Solution {
                component: id,
                capacity: input.capacity,
                load: input.load,
                speed: 0.0,
                status: NetworkStatus::Unpowered,
            },
        });
        self.solve(id);
        Ok(())
    }

    /// Remove a node and solve all pieces of its former component once.
    /// The visit count excludes the removed node.
    pub fn remove_node(&mut self, id: u32) -> Result<usize, GraphError> {
        let node = self.nodes.remove(&id).ok_or(GraphError::MissingNode(id))?;
        let mut neighbors: BTreeSet<_> = node.edges.into_keys().collect();
        for neighbor in &neighbors {
            if let Some(neighbor_node) = self.nodes.get_mut(neighbor) {
                neighbor_node.edges.remove(&id);
            }
        }
        let mut visited = 0;
        while let Some(start) = neighbors.pop_first() {
            visited += self.solve(start);
            for member in &self.scratch_members {
                neighbors.remove(member);
            }
        }
        Ok(visited)
    }

    /// Set node inputs and return the number of visited nodes.
    pub fn update_node(&mut self, id: u32, input: KineticNode) -> Result<usize, GraphError> {
        validate(id, input)?;
        let node = self.nodes.get_mut(&id).ok_or(GraphError::MissingNode(id))?;
        if node.input == input {
            return Ok(0);
        }
        node.input = input;
        let root = node.solution.component;
        Ok(self.solve(root))
    }

    /// Connect two nodes with `speed[b] = speed[a] * ratio`. A negative ratio
    /// inverts rotation. Return the number of nodes visited in the merged component.
    pub fn connect(&mut self, a: u32, b: u32, ratio: f64) -> Result<usize, GraphError> {
        self.check_endpoints(a, b)?;
        let inverse = ratio.recip();
        if !ratio.is_finite() || ratio == 0.0 || !inverse.is_finite() {
            return Err(GraphError::InvalidRatio);
        }
        if self.nodes[&a].edges.contains_key(&b) {
            return Err(GraphError::DuplicateEdge);
        }
        let root = self.nodes[&a]
            .solution
            .component
            .min(self.nodes[&b].solution.component);
        self.nodes
            .get_mut(&a)
            .ok_or(GraphError::MissingNode(a))?
            .edges
            .insert(b, ratio);
        self.nodes
            .get_mut(&b)
            .ok_or(GraphError::MissingNode(b))?
            .edges
            .insert(a, inverse);
        Ok(self.solve(root))
    }

    /// Remove an edge and solve each resulting component once.
    pub fn disconnect(&mut self, a: u32, b: u32) -> Result<usize, GraphError> {
        self.check_endpoints(a, b)?;
        if self
            .nodes
            .get_mut(&a)
            .ok_or(GraphError::MissingNode(a))?
            .edges
            .remove(&b)
            .is_none()
        {
            return Err(GraphError::MissingEdge);
        }
        if let Some(node) = self.nodes.get_mut(&b) {
            node.edges.remove(&a);
        }
        let root = self.nodes[&a].solution.component;
        let first = self.solve(root);
        if self.scratch_members.binary_search(&a).is_err() {
            return Ok(first + self.solve(a));
        }
        if self.scratch_members.binary_search(&b).is_err() {
            return Ok(first + self.solve(b));
        }
        Ok(first)
    }

    pub fn solution(&self, id: u32) -> Option<Solution> {
        self.nodes.get(&id).map(|node| node.solution)
    }

    /// Number of visited nodes during a tick. Edits settle before returning.
    pub fn tick(&self) -> usize {
        0
    }

    fn check_endpoints(&self, a: u32, b: u32) -> Result<(), GraphError> {
        for id in [a, b] {
            if !self.nodes.contains_key(&id) {
                return Err(GraphError::MissingNode(id));
            }
        }
        if a == b {
            return Err(GraphError::SelfEdge);
        }
        Ok(())
    }

    /// Solve one connected component, publishing its solution and leaving the
    /// sorted member IDs in `scratch_members`. Returns the number of members.
    fn solve(&mut self, start: u32) -> usize {
        let mut scales = std::mem::take(&mut self.scratch_scales);
        let mut queue = std::mem::take(&mut self.scratch_queue);
        scales.clear();
        queue.clear();
        scales.insert(start, 1.0);
        queue.push_back(start);
        let mut conflict = false;
        while let Some(id) = queue.pop_front() {
            let base = scales[&id];
            for (&neighbor, &ratio) in &self.nodes[&id].edges {
                let scale = base * ratio;
                if !scale.is_finite() || scale == 0.0 {
                    conflict = true;
                }
                match scales.entry(neighbor) {
                    BTreeEntry::Vacant(entry) => {
                        entry.insert(scale);
                        queue.push_back(neighbor);
                    }
                    BTreeEntry::Occupied(entry) => conflict |= !close(*entry.get(), scale),
                }
            }
        }

        let Some((&component, _)) = scales.first_key_value() else {
            return 0;
        };
        let mut drive = None;
        let mut capacity = 0.0;
        let mut load = 0.0;
        for (&id, &scale) in &scales {
            let input = self.nodes[&id].input;
            capacity += input.capacity;
            load += input.load;
            if let Some(speed) = input.driven_speed {
                let speed = speed / scale;
                conflict |= !speed.is_finite() || (speed == 0.0 && input.driven_speed != Some(0.0));
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
            let solution = Solution {
                component,
                capacity,
                load,
                speed: if status == NetworkStatus::Running {
                    base * scale
                } else {
                    0.0
                },
                status,
            };
            if let Some(node) = self.nodes.get_mut(&id) {
                node.solution = solution;
            }
        }

        let mut members = std::mem::take(&mut self.scratch_members);
        members.clear();
        members.extend(scales.keys().copied());
        let count = members.len();
        self.scratch_members = members;
        self.scratch_scales = scales;
        self.scratch_queue = queue;
        count
    }
}

fn validate(id: u32, node: KineticNode) -> Result<(), GraphError> {
    if !node.capacity.is_finite()
        || node.capacity < 0.0
        || !node.load.is_finite()
        || node.load < 0.0
        || node.driven_speed.is_some_and(|speed| !speed.is_finite())
    {
        return Err(GraphError::InvalidNode(id));
    }
    Ok(())
}

fn close(a: f64, b: f64) -> bool {
    a == b || (a - b).abs() <= 1e-9 * a.abs().max(b.abs())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn drive(capacity: f64, speed: f64) -> KineticNode {
        KineticNode {
            capacity,
            driven_speed: Some(speed),
            ..KineticNode::default()
        }
    }

    #[test]
    fn reversed_gear_and_additive_capacity_overstress_and_recover() {
        let mut graph = KineticGraph::default();
        graph.add_node(0, drive(8.0, 12.0)).unwrap();
        graph.add_node(1, drive(8.0, -24.0)).unwrap();
        graph
            .add_node(
                2,
                KineticNode {
                    load: 16.0,
                    ..KineticNode::default()
                },
            )
            .unwrap();
        graph.connect(1, 0, -0.5).unwrap();
        graph.connect(1, 2, 3.0).unwrap();
        let state = graph.solution(2).unwrap();
        assert_eq!(
            (state.speed, state.capacity, state.load),
            (-72.0, 16.0, 16.0)
        );
        let load = |load| KineticNode {
            load,
            ..KineticNode::default()
        };
        assert_eq!(graph.update_node(2, load(17.0)), Ok(3));
        for id in 0..3 {
            let state = graph.solution(id).unwrap();
            assert_eq!(
                (state.status, state.speed),
                (NetworkStatus::Overstressed, 0.0)
            );
        }
        assert_eq!(graph.update_node(2, load(16.0)), Ok(3));
        assert_eq!(graph.solution(2).unwrap().speed, -72.0);
        assert_eq!(graph.update_node(2, load(16.0)), Ok(0));
    }

    #[test]
    fn split_and_merge_visit_only_affected_networks() {
        let mut graph = KineticGraph::default();
        graph.add_node(4, drive(10.0, 4.0)).unwrap();
        for id in 5..2004 {
            graph.add_node(id, KineticNode::default()).unwrap();
        }
        for id in 7..2003 {
            graph.connect(id, id + 1, 1.0).unwrap();
        }
        let unrelated = graph.solution(2003).unwrap();
        assert_eq!(graph.connect(4, 5, 1.0), Ok(2));
        assert_eq!(graph.connect(5, 6, 1.0), Ok(3));
        assert_eq!(graph.disconnect(6, 5), Ok(3));
        assert_eq!(graph.solution(6).unwrap().status, NetworkStatus::Unpowered);
        assert_eq!(graph.solution(5).unwrap().speed, 4.0);
        assert_eq!(graph.connect(6, 5, 1.0), Ok(3));
        assert_eq!(graph.solution(6).unwrap().speed, 4.0);
        assert_eq!(graph.solution(2003), Some(unrelated));
        assert_eq!(graph.tick(), 0);
    }

    #[test]
    fn contradictory_cycle_and_drive_are_detected_and_recover() {
        let mut graph = KineticGraph::default();
        graph.add_node(0, drive(10.0, 10.0)).unwrap();
        graph.add_node(1, KineticNode::default()).unwrap();
        graph.add_node(2, KineticNode::default()).unwrap();
        graph.connect(0, 1, 2.0).unwrap();
        graph.connect(1, 2, -2.0).unwrap();
        graph.connect(0, 2, 4.0).unwrap();
        assert_eq!(graph.solution(2).unwrap().status, NetworkStatus::Conflict);
        assert_eq!(graph.disconnect(0, 2), Ok(3));
        assert_eq!(graph.solution(2).unwrap().speed, -40.0);
        graph.update_node(2, drive(10.0, 40.0)).unwrap();
        assert_eq!(graph.solution(0).unwrap().status, NetworkStatus::Conflict);
        graph.update_node(2, drive(10.0, -40.0)).unwrap();
        assert_eq!(graph.solution(0).unwrap().status, NetworkStatus::Running);
        assert_eq!(
            graph.connect(0, 2, f64::from_bits(1)),
            Err(GraphError::InvalidRatio)
        );
    }

    #[test]
    fn removing_a_junction_solves_each_remaining_component_once() {
        let mut graph = KineticGraph::default();
        for id in 0..5 {
            graph.add_node(id, KineticNode::default()).unwrap();
        }
        graph.update_node(3, drive(10.0, 15.0)).unwrap();
        for (a, b) in [(0, 1), (0, 2), (0, 3), (1, 2)] {
            graph.connect(a, b, 1.0).unwrap();
        }
        assert_eq!(graph.solution(2).unwrap().speed, 15.0);
        let unrelated = graph.solution(4);
        assert_eq!(graph.remove_node(0), Ok(3));
        assert_eq!(graph.solution(0), None);
        assert_eq!(graph.solution(2).unwrap().component, 1);
        assert_eq!(graph.solution(2).unwrap().status, NetworkStatus::Unpowered);
        assert_eq!(graph.solution(3).unwrap().speed, 15.0);
        assert_eq!(graph.solution(4), unrelated);
    }
}
