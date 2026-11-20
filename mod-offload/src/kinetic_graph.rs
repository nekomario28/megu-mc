// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later

//! Kinetic topology and cached component solutions. Edits solve synchronously and
//! only traverse adjacent edges of affected components.

use std::collections::{btree_map::Entry, BTreeMap, BTreeSet, VecDeque};

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
    nodes: BTreeMap<u32, Node>,
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
            self.nodes.get_mut(neighbor).unwrap().edges.remove(&id);
        }
        let mut visited = 0;
        while let Some(start) = neighbors.pop_first() {
            let component = self.solve(start);
            visited += component.len();
            for member in component.keys() {
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
        Ok(self.solve(root).len())
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
        self.nodes.get_mut(&a).unwrap().edges.insert(b, ratio);
        self.nodes.get_mut(&b).unwrap().edges.insert(a, inverse);
        Ok(self.solve(root).len())
    }

    /// Remove an edge and solve each resulting component once.
    pub fn disconnect(&mut self, a: u32, b: u32) -> Result<usize, GraphError> {
        self.check_endpoints(a, b)?;
        if self.nodes.get_mut(&a).unwrap().edges.remove(&b).is_none() {
            return Err(GraphError::MissingEdge);
        }
        self.nodes.get_mut(&b).unwrap().edges.remove(&a);
        let root = self.nodes[&a].solution.component;
        let first = self.solve(root);
        for endpoint in [a, b] {
            if !first.contains_key(&endpoint) {
                return Ok(first.len() + self.solve(endpoint).len());
            }
        }
        Ok(first.len())
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

    fn solve(&mut self, start: u32) -> BTreeMap<u32, f64> {
        let mut queue = VecDeque::from([start]);
        let mut scales = BTreeMap::from([(start, 1.0)]);
        let mut conflict = false;
        while let Some(id) = queue.pop_front() {
            for (&neighbor, &ratio) in &self.nodes[&id].edges {
                let scale = scales[&id] * ratio;
                if !scale.is_finite() || scale == 0.0 {
                    conflict = true;
                }
                match scales.entry(neighbor) {
                    Entry::Vacant(entry) => {
                        entry.insert(scale);
                        queue.push_back(neighbor);
                    }
                    Entry::Occupied(entry) => conflict |= !close(*entry.get(), scale),
                }
            }
        }

        let component = *scales.first_key_value().unwrap().0;
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
            self.nodes.get_mut(&id).unwrap().solution = Solution {
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
        }
        scales
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
