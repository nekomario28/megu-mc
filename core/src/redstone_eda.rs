// SPDX-License-Identifier: LGPL-3.0-or-later
#![forbid(unsafe_code)]

//! Event-driven propagation over a caller-compiled redstone dust graph.
//!
//! Block-shape rules belong to the caller: a [`WireNode`] contains only links
//! which conduct, so callers can omit cut corners and encode step-up/down
//! connections explicitly. Sources inject power in `0..=15`; each traversed
//! dust node attenuates it by one, and competing signals combine with `max`.

use std::collections::{HashMap, HashSet};

/// Integer block position used to key compiled wire nodes.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct WirePos {
    pub x: i32,
    pub y: i32,
    pub z: i32,
}

impl WirePos {
    /// Returns whether two positions are in the local dust topology radius.
    /// This covers horizontal diagonals and one-block step-up/down links.
    fn touches(self, other: Self) -> bool {
        self.x.abs_diff(other.x) <= 1
            && self.y.abs_diff(other.y) <= 1
            && self.z.abs_diff(other.z) <= 1
    }
}

/// A directed conducting edge to another node in the netlist.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WireLink {
    pub to: usize,
}

/// A dust node and its caller-compiled conducting links.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WireNode {
    pub position: WirePos,
    /// Existing block-state power, used to produce minimal state updates.
    pub power: u8,
    /// External input at this node. `None` means the node is not a source.
    pub source_power: Option<u8>,
    /// Directed edges; add the reverse edge when the connection conducts both ways.
    pub links: Vec<WireLink>,
}

/// One changed dust block state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WireUpdate {
    pub node: usize,
    pub position: WirePos,
    pub old_power: u8,
    pub new_power: u8,
}

/// Work and change counts for one solve.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct WireSolveStats {
    /// Nodes popped at their current best power from a bucket.
    pub nodes_visited: usize,
    /// Number of emitted block-state changes.
    pub state_changes: usize,
    /// Per-edge neighbor writes consolidated into the final block-state delta.
    pub naive_vanilla_updates_avoided: usize,
}

/// Result of a single solve pass.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct WireSolveResult {
    pub updates: Vec<WireUpdate>,
    pub stats: WireSolveStats,
}

/// Static wire graph with deterministic 16-bucket max-power propagation.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RedstoneWireSolver {
    nodes: Vec<WireNode>,
}

impl RedstoneWireSolver {
    pub fn new(nodes: Vec<WireNode>) -> Self {
        Self { nodes }
    }

    pub fn nodes(&self) -> &[WireNode] {
        &self.nodes
    }

    pub fn nodes_mut(&mut self) -> &mut [WireNode] {
        &mut self.nodes
    }

    /// Propagates strongest source power with unit attenuation per dust node.
    ///
    /// The queue uses 16 power buckets rather than a heap. Since edge cost is
    /// exactly one and power is bounded to 15, every node is finalized in
    /// descending power order. A node state is written once after propagation.
    pub fn solve(&mut self) -> WireSolveResult {
        let mut buckets: [Vec<usize>; 16] = std::array::from_fn(|_| Vec::new());
        let mut best = vec![0_u8; self.nodes.len()];
        for (index, node) in self.nodes.iter().enumerate() {
            let source = node.source_power.unwrap_or(0).min(15);
            if source > best[index] {
                best[index] = source;
                buckets[usize::from(source)].push(index);
            }
        }

        let mut nodes_visited = 0;
        let mut neighbor_candidates = 0_usize;
        for level in (1_u8..=15).rev() {
            while let Some(index) = buckets[usize::from(level)].pop() {
                if best[index] != level {
                    continue;
                }
                nodes_visited += 1;
                let next_power = level - 1;
                if next_power == 0 {
                    continue;
                }
                for link in &self.nodes[index].links {
                    if link.to >= self.nodes.len() {
                        continue;
                    }
                    neighbor_candidates += 1;
                    if next_power > best[link.to] {
                        best[link.to] = next_power;
                        buckets[usize::from(next_power)].push(link.to);
                    }
                }
            }
        }

        let mut updates = Vec::new();
        for (index, (node, &new_power)) in self.nodes.iter_mut().zip(&best).enumerate() {
            let old_power = node.power.min(15);
            if old_power != new_power {
                updates.push(WireUpdate {
                    node: index,
                    position: node.position,
                    old_power,
                    new_power,
                });
                node.power = new_power;
            }
        }
        let state_changes = updates.len();
        WireSolveResult {
            updates,
            stats: WireSolveStats {
                nodes_visited,
                state_changes,
                naive_vanilla_updates_avoided: neighbor_candidates.saturating_sub(state_changes),
            },
        }
    }
}

/// Compiled topology cache with component-local dirty tracking.
///
/// A section revision change causes full discovery. With a stable revision,
/// [`refresh_invalidated`](Self::refresh_invalidated) invokes its discovery
/// callback only for components touched by [`invalidate_pos`](Self::invalidate_pos).
/// The callback receives all old positions in those components and returns the
/// replacement local graph; it may return multiple components after a cut.
#[derive(Clone, Debug, Default)]
pub struct RedstoneNetlistCache {
    section_rev: Option<u64>,
    components: Vec<Vec<WireNode>>,
    dirty_components: HashSet<usize>,
    dirty_positions: HashSet<WirePos>,
    solver: Option<RedstoneWireSolver>,
}

impl RedstoneNetlistCache {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn section_rev(&self) -> Option<u64> {
        self.section_rev
    }

    /// Compiles topology once for a revision, or after a section revision changes.
    pub fn get_or_compile<F>(&mut self, section_rev: u64, discover: F) -> &mut RedstoneWireSolver
    where
        F: FnOnce() -> Vec<WireNode>,
    {
        if self.section_rev != Some(section_rev) {
            self.components = connected_components(discover());
            self.reindex();
            self.section_rev = Some(section_rev);
            self.dirty_components.clear();
            self.dirty_positions.clear();
            self.solver = Some(RedstoneWireSolver::new(self.flatten_components()));
        }
        if self.solver.is_none() {
            self.solver = Some(RedstoneWireSolver::new(self.flatten_components()));
        }
        self.solver.as_mut().expect("solver was just populated")
    }

    /// Marks components touching a changed block position as topology-dirty.
    pub fn invalidate_pos(&mut self, position: WirePos) {
        self.dirty_positions.insert(position);
        for (component, nodes) in self.components.iter().enumerate() {
            if nodes.iter().any(|node| node.position.touches(position)) {
                self.dirty_components.insert(component);
            }
        }
    }

    /// Rebuilds only dirty components and returns a solver over the current cache.
    ///
    /// The discovery closure is not called when nothing is dirty and the section
    /// revision is unchanged. On revision mismatch it receives an empty position
    /// list and must return the complete section graph.
    pub fn refresh_invalidated<F>(
        &mut self,
        section_rev: u64,
        mut discover: F,
    ) -> &mut RedstoneWireSolver
    where
        F: FnMut(&[WirePos]) -> Vec<WireNode>,
    {
        let mut topology_changed = false;
        if self.section_rev != Some(section_rev) {
            self.components = connected_components(discover(&[]));
            self.section_rev = Some(section_rev);
            self.reindex();
            self.dirty_components.clear();
            self.dirty_positions.clear();
            topology_changed = true;
        } else if !self.dirty_components.is_empty() || !self.dirty_positions.is_empty() {
            let mut old_components: Vec<_> = self.dirty_components.drain().collect();
            old_components.sort_unstable();
            let mut affected_positions = self.dirty_positions.drain().collect::<Vec<_>>();
            for &component in &old_components {
                if let Some(nodes) = self.components.get(component) {
                    affected_positions.extend(nodes.iter().map(|node| node.position));
                }
            }
            affected_positions.sort_by_key(|p| (p.x, p.y, p.z));
            affected_positions.dedup();

            let mut retained = Vec::new();
            for (component, nodes) in std::mem::take(&mut self.components).into_iter().enumerate() {
                if !old_components.binary_search(&component).is_ok() {
                    retained.push(nodes);
                }
            }
            retained.extend(connected_components(discover(&affected_positions)));
            self.components = retained;
            self.reindex();
            topology_changed = true;
        }
        if topology_changed || self.solver.is_none() {
            self.solver = Some(RedstoneWireSolver::new(self.flatten_components()));
        }
        self.solver.as_mut().expect("solver was just populated")
    }

    pub fn dirty_component_count(&self) -> usize {
        self.dirty_components.len()
    }

    pub fn cached_node_count(&self) -> usize {
        self.components.iter().map(Vec::len).sum()
    }

    fn flatten_components(&self) -> Vec<WireNode> {
        let mut nodes = Vec::with_capacity(self.cached_node_count());
        for component in &self.components {
            let offset = nodes.len();
            nodes.extend(component.iter().cloned().map(|mut node| {
                for link in &mut node.links {
                    link.to += offset;
                }
                node
            }));
        }
        nodes
    }

    fn reindex(&mut self) {
        self.dirty_components
            .retain(|&id| id < self.components.len());
    }
}

fn connected_components(nodes: Vec<WireNode>) -> Vec<Vec<WireNode>> {
    let mut seen = vec![false; nodes.len()];
    let mut result = Vec::new();
    for start in 0..nodes.len() {
        if seen[start] {
            continue;
        }
        let mut stack = vec![start];
        let mut indices = Vec::new();
        seen[start] = true;
        while let Some(index) = stack.pop() {
            indices.push(index);
            for link in &nodes[index].links {
                if link.to < nodes.len() && !seen[link.to] {
                    seen[link.to] = true;
                    stack.push(link.to);
                }
            }
        }
        indices.sort_unstable();
        let mut remap = HashMap::with_capacity(indices.len());
        for (local, &global) in indices.iter().enumerate() {
            remap.insert(global, local);
        }
        let mut component = Vec::with_capacity(indices.len());
        for &global in &indices {
            let mut node = nodes[global].clone();
            node.links = node
                .links
                .iter()
                .filter_map(|link| remap.get(&link.to).map(|&to| WireLink { to }))
                .collect();
            component.push(node);
        }
        result.push(component);
    }
    result
}
