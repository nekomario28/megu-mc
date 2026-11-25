// SPDX-License-Identifier: LGPL-3.0-or-later
#![forbid(unsafe_code)]
//! `megu_core::dep_graph` — an incremental dependency DAG with Adapton /
//! Salsa-style early cutoff.
//!
//! [`IncrementalDag`] models derived-computation graphs where upstream
//! `Input` nodes feed `Derived` nodes that feed further downstream
//! consumers — `SectionBlocks → Light → Mesh`, `Inventory + Recipes →
//! CraftableSet → JEI grid`, `ModInputs → MachineNetwork → Outputs`.
//!
//! The contract:
//!
//! * [`IncrementalDag::add_edge`] rejects edges that would close a cycle,
//!   so evaluation order is always well-defined.
//! * [`IncrementalDag::mark_input_changed`] commits a new digest on an
//!   input and marks every transitive descendant stale.
//! * [`IncrementalDag::recompute_with_cutoff`] is the payoff: when a
//!   recomputed digest matches the stored one (**early cutoff**), the node
//!   is verified clean *without* propagating invalidation — and any
//!   downstream node whose only staleness flowed through it is rescued
//!   to clean immediately, cascading transitively.
//! * [`IncrementalDag::dirty_order_into`] emits the still-dirty set in
//!   topological order into a caller-owned scratch `Vec` — zero
//!   allocation across reuses.
//!
//! Staleness is tracked with two facts per node: a `dirty` flag and a
//! `pending` count of upstream edges that reported a *changed* digest
//! since the node's last verification. A dirty node with `pending == 0`
//! is dirty only because an upstream is still dirty — it can be cleared
//! the moment all its upstreams verify clean. That distinction is what
//! lets a mid-chain early cutoff rescue whole untouched subgraphs instead
//! of merely stopping at one edge.
//!
//! Pure `std`, deterministic, `unsafe`-free.

/// Opaque node handle.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct NodeId(pub u32);

/// Errors from [`IncrementalDag::add_edge`] and node lookups.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DagError {
    /// A `NodeId` this graph never issued (or that was never added).
    NodeNotFound(NodeId),
    /// Self-edges are degenerate cycles and always rejected.
    SelfLoop(NodeId),
    /// Adding `upstream → downstream` would create a cycle: `downstream`
    /// already reaches `upstream` transitively.
    Cycle {
        /// Proposed upstream endpoint.
        upstream: NodeId,
        /// Proposed downstream endpoint.
        downstream: NodeId,
    },
}

impl std::fmt::Display for DagError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DagError::NodeNotFound(id) => write!(f, "node {} not in graph", id.0),
            DagError::SelfLoop(id) => write!(f, "self-loop on node {}", id.0),
            DagError::Cycle { upstream, downstream } => write!(
                f,
                "edge {} -> {} would close a dependency cycle",
                upstream.0, downstream.0
            ),
        }
    }
}

impl std::error::Error for DagError {}

/// Result of [`IncrementalDag::recompute_with_cutoff`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CutoffOutcome {
    /// The recomputed digest equals the stored one: the node was verified
    /// clean and *no* downstream invalidation was issued — downstream
    /// nodes kept dirty only by this chain were rescued to clean.
    EarlyCutoff,
    /// The digest changed: the node is clean and its transitive
    /// descendants were marked dirty.
    Changed,
}

struct DagNode {
    /// Last committed value fingerprint.
    digest: u64,
    /// Needs (re)evaluation before its output may be trusted.
    dirty: bool,
    /// Upstream edges that reported a *changed* digest since this node's
    /// last verification. `dirty && pending == 0` means "dirty only
    /// because some upstream is still dirty" — rescuable by cutoff.
    pending: u32,
    upstream: Vec<u32>,
    downstream: Vec<u32>,
}

/// Incremental dependency DAG with digest-driven early cutoff.
pub struct IncrementalDag {
    nodes: Vec<DagNode>,
}

impl Default for IncrementalDag {
    fn default() -> Self {
        Self::new()
    }
}

impl IncrementalDag {
    /// Empty graph.
    pub fn new() -> Self {
        Self { nodes: Vec::new() }
    }

    /// Empty graph with room for `capacity` nodes.
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            nodes: Vec::with_capacity(capacity),
        }
    }

    /// Add a node carrying `initial_digest`; starts clean and verified.
    pub fn add_node(&mut self, initial_digest: u64) -> NodeId {
        let id = NodeId(self.nodes.len() as u32);
        self.nodes.push(DagNode {
            digest: initial_digest,
            dirty: false,
            pending: 0,
            upstream: Vec::new(),
            downstream: Vec::new(),
        });
        id
    }

    /// Number of nodes in the graph.
    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    /// Stored digest of `node`.
    pub fn digest(&self, node: NodeId) -> Result<u64, DagError> {
        Ok(self.node(node)?.digest)
    }

    /// Whether `node` is marked stale.
    pub fn is_dirty(&self, node: NodeId) -> Result<bool, DagError> {
        Ok(self.node(node)?.dirty)
    }

    /// Number of currently dirty nodes.
    pub fn dirty_count(&self) -> usize {
        self.nodes.iter().filter(|n| n.dirty).count()
    }

    /// Add a `upstream → downstream` edge (data flows upstream to
    /// downstream). Idempotent: re-adding an existing edge is a no-op.
    /// Returns [`DagError::Cycle`] when `downstream` already reaches
    /// `upstream` transitively.
    pub fn add_edge(&mut self, upstream: NodeId, downstream: NodeId) -> Result<(), DagError> {
        let u = upstream.0 as usize;
        let d = downstream.0 as usize;
        self.node(upstream)?;
        self.node(downstream)?;
        if u == d {
            return Err(DagError::SelfLoop(upstream));
        }
        if self.nodes[u].downstream.contains(&(d as u32)) {
            return Ok(());
        }
        if self.reaches(d, u) {
            return Err(DagError::Cycle {
                upstream,
                downstream,
            });
        }
        self.nodes[u].downstream.push(d as u32);
        self.nodes[d].upstream.push(u as u32);
        Ok(())
    }

    /// Commit `new_digest` on input `node` and mark every transitive
    /// descendant stale. Direct children record a `pending` changed-edge
    /// count; deeper descendants are dirty through their still-dirty
    /// upstreams and are rescued automatically if the chain cuts off.
    pub fn mark_input_changed(&mut self, node: NodeId, new_digest: u64) -> Result<(), DagError> {
        let idx = node.0 as usize;
        self.node(node)?;
        self.nodes[idx].digest = new_digest;
        // The input itself is verified by the caller's commit; only its
        // descendants go stale.
        self.nodes[idx].dirty = false;
        self.nodes[idx].pending = 0;
        self.mark_descendants(idx);
        Ok(())
    }

    /// Recompute `node` to `new_digest` and settle its dirty flag.
    ///
    /// * Equal digest → [`CutoffOutcome::EarlyCutoff`]: the node is
    ///   verified clean, nothing is invalidated, and each direct
    ///   descendant is re-examined — one whose `pending` is zero and whose
    ///   upstreams are all clean is rescued to clean, cascading further.
    /// * Different digest → [`CutoffOutcome::Changed`]: the node is clean
    ///   and every transitive descendant is marked dirty (direct children
    ///   additionally record a `pending` change).
    pub fn recompute_with_cutoff(&mut self, node: NodeId, new_digest: u64) -> CutoffOutcome {
        let Some(dag_node) = self.nodes.get_mut(node.0 as usize) else {
            return CutoffOutcome::EarlyCutoff;
        };
        let idx = node.0 as usize;
        dag_node.dirty = false;
        dag_node.pending = 0;
        if dag_node.digest == new_digest {
            // Verified unchanged: rescue downstream nodes whose only
            // staleness flowed through here.
            let children = self.nodes[idx].downstream.clone();
            for child in children {
                self.try_rescue(child as usize);
            }
            CutoffOutcome::EarlyCutoff
        } else {
            self.nodes[idx].digest = new_digest;
            self.mark_descendants(idx);
            CutoffOutcome::Changed
        }
    }

    /// Append all dirty nodes in topological order (upstreams first) to
    /// `out`. Only *dirty* upstreams count toward ordering — a node dirty
    /// through a clean (already verified) upstream may evaluate anywhere.
    /// Reuse `out` across frames for zero allocation.
    pub fn dirty_order_into(&self, out: &mut Vec<NodeId>) {
        // In-degree restricted to dirty upstreams.
        let mut indegree: Vec<u32> = vec![0; self.nodes.len()];
        let mut ready: Vec<u32> = Vec::new();
        for (i, node) in self.nodes.iter().enumerate() {
            if !node.dirty {
                continue;
            }
            let dirty_ins = node
                .upstream
                .iter()
                .filter(|&&u| self.nodes[u as usize].dirty)
                .count() as u32;
            indegree[i] = dirty_ins;
            if dirty_ins == 0 {
                ready.push(i as u32);
            }
        }
        // `ready` is filled in index order; Kahn's traversal preserves
        // determinism since children append in scan order.
        let mut cursor = 0;
        while cursor < ready.len() {
            let idx = ready[cursor] as usize;
            cursor += 1;
            if !self.nodes[idx].dirty {
                continue;
            }
            out.push(NodeId(idx as u32));
            for &child in &self.nodes[idx].downstream {
                let c = child as usize;
                if self.nodes[c].dirty {
                    indegree[c] -= 1;
                    if indegree[c] == 0 {
                        ready.push(child);
                    }
                }
            }
        }
    }

    /// All dirty nodes in topological order (convenience over
    /// [`IncrementalDag::dirty_order_into`]).
    pub fn dirty_order(&self) -> Vec<NodeId> {
        let mut out = Vec::new();
        self.dirty_order_into(&mut out);
        out
    }

    fn node(&self, id: NodeId) -> Result<&DagNode, DagError> {
        self.nodes.get(id.0 as usize).ok_or(DagError::NodeNotFound(id))
    }

    /// Whether a walk along downstream edges starting at `from` reaches
    /// `target` (strictly — a node does not reach itself here).
    fn reaches(&self, from: usize, target: usize) -> bool {
        let mut seen = vec![false; self.nodes.len()];
        let mut stack = vec![from];
        while let Some(idx) = stack.pop() {
            for &next in &self.nodes[idx].downstream {
                let n = next as usize;
                if n == target {
                    return true;
                }
                if !seen[n] {
                    seen[n] = true;
                    stack.push(n);
                }
            }
        }
        false
    }

    /// Mark every transitive descendant of `idx` dirty; direct children
    /// also record a `pending` changed-edge count.
    fn mark_descendants(&mut self, idx: usize) {
        let mut stack = self.nodes[idx].downstream.clone();
        for &child in &stack {
            self.nodes[child as usize].pending += 1;
        }
        let mut seen = vec![false; self.nodes.len()];
        while let Some(i) = stack.pop() {
            let i = i as usize;
            if seen[i] {
                continue;
            }
            seen[i] = true;
            self.nodes[i].dirty = true;
            stack.extend_from_slice(&self.nodes[i].downstream);
        }
    }

    /// Rescue `idx` to clean when it is dirty only through upstreams that
    /// have since verified clean (`pending == 0` and no dirty upstream),
    /// cascading the check to its own descendants.
    fn try_rescue(&mut self, idx: usize) {
        if !self.nodes[idx].dirty || self.nodes[idx].pending != 0 {
            return;
        }
        let has_dirty_upstream = self.nodes[idx]
            .upstream
            .iter()
            .any(|&u| self.nodes[u as usize].dirty);
        if has_dirty_upstream {
            return;
        }
        self.nodes[idx].dirty = false;
        let children = self.nodes[idx].downstream.clone();
        for child in children {
            self.try_rescue(child as usize);
        }
    }
}
