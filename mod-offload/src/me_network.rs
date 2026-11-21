// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later

//! Clean-room ME-style logistics: a channel-routing graph, a priority
//! storage-cell index, and a fail-closed auto-crafting DAG planner.
//!
//! Every traversal and allocation runs in a deterministic order (ascending
//! node IDs, descending cell priority, pattern slice order), so identical
//! inputs always produce identical allocations, plans, and error paths.
//! Nothing here reads a wall clock or depends on crate-external state.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

/// Node identifier inside one [`MeChannelGraph`].
pub type MeNodeId = u32;
/// Storage-cell identifier inside one [`MeStorageIndex`].
pub type CellId = u64;

/// One controller node exposes at most this many channel faces: each link to
/// a non-controller neighbour occupies one face.
pub const CONTROLLER_FACES: usize = 6;
/// Each controller face supplies at most this many channels.
pub const CHANNELS_PER_FACE: u32 = 32;
/// A dense cable carries at most this many channels.
pub const DENSE_CABLE_CHANNELS: u32 = 32;
/// A normal cable carries at most this many channels.
pub const NORMAL_CABLE_CHANNELS: u32 = 8;
/// A controller-less component supports at most this many channelled devices.
pub const ADHOC_CHANNEL_CAP: usize = 8;
/// A classic storage cell accounts eight items per used byte.
pub const ITEMS_PER_BYTE: u64 = 8;

// ---- channel routing -------------------------------------------------------

/// What a network node does inside [`MeChannelGraph`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MeNodeKind {
    /// Supplies up to [`CHANNELS_PER_FACE`] channels on each of at most
    /// [`CONTROLLER_FACES`] faces. Directly linked controllers merge into one
    /// multiblock group and source channels jointly.
    Controller,
    /// Carries up to [`DENSE_CABLE_CHANNELS`] channels of through traffic.
    DenseCable,
    /// Carries up to [`NORMAL_CABLE_CHANNELS`] channels of through traffic.
    NormalCable,
    /// Consumes one channel when `requires_channel` is set and stays online
    /// without one otherwise. Devices terminate paths; they relay nothing.
    Device { requires_channel: bool },
}

/// Build errors for [`MeChannelGraph`] edits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChannelError {
    DuplicateNode(MeNodeId),
    MissingNode(MeNodeId),
    SelfLink(MeNodeId),
}

/// A topology defect detected during [`MeChannelGraph::solve_channels`]. A
/// defect fails closed: its component supplies no channels at all.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ChannelConflict {
    /// Two or more controller multiblocks (maximal directly-linked controller
    /// groups) share one connected component. `groups` holds the lowest node
    /// ID of each conflicting multiblock in ascending order.
    ConflictingControllers { groups: Vec<MeNodeId> },
    /// A controller multiblock exposes more than [`CONTROLLER_FACES`] channel
    /// faces. `controllers` holds the offending multiblock's members in
    /// ascending order.
    InvalidControllerMultiblock { controllers: Vec<MeNodeId> },
}

/// The deterministic result of [`MeChannelGraph::solve_channels`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ChannelAllocation {
    /// Devices that are powered: every device not requiring a channel plus
    /// every channelled device that received one.
    pub online_devices: BTreeSet<MeNodeId>,
    /// Channelled devices that received no channel, including every
    /// channelled device inside a conflicted component.
    pub channel_starved_devices: BTreeSet<MeNodeId>,
    /// Channels flowing through each cable node; zero for unused cables.
    pub used_channels: BTreeMap<MeNodeId, u32>,
    /// Topology defects found while solving, in component discovery order.
    pub conflicts: Vec<ChannelConflict>,
}

struct MeNode {
    kind: MeNodeKind,
    links: BTreeSet<MeNodeId>,
}

/// Undirected channel-routing graph. Channels originate at controller faces,
/// flow along shortest paths through cables, and terminate at devices.
#[derive(Default)]
pub struct MeChannelGraph {
    nodes: BTreeMap<MeNodeId, MeNode>,
}

/// Channel capacity of a node used as through traffic. Controllers source
/// channels and devices sink them; only cables relay.
fn transit_capacity(kind: MeNodeKind) -> u32 {
    match kind {
        MeNodeKind::DenseCable => DENSE_CABLE_CHANNELS,
        MeNodeKind::NormalCable => NORMAL_CABLE_CHANNELS,
        MeNodeKind::Controller | MeNodeKind::Device { .. } => 0,
    }
}

impl MeChannelGraph {
    pub fn add_node(&mut self, id: MeNodeId, kind: MeNodeKind) -> Result<(), ChannelError> {
        let std::collections::btree_map::Entry::Vacant(entry) = self.nodes.entry(id) else {
            return Err(ChannelError::DuplicateNode(id));
        };
        entry.insert(MeNode {
            kind,
            links: BTreeSet::new(),
        });
        Ok(())
    }

    pub fn remove_node(&mut self, id: MeNodeId) -> Result<(), ChannelError> {
        let node = self
            .nodes
            .remove(&id)
            .ok_or(ChannelError::MissingNode(id))?;
        for other in node.links {
            self.nodes.get_mut(&other).unwrap().links.remove(&id);
        }
        Ok(())
    }

    /// Set or remove an undirected link. Re-setting an existing link is a
    /// no-op; removing an absent one reports `MissingNode` for `b` unchanged.
    pub fn set_link(&mut self, a: MeNodeId, b: MeNodeId, linked: bool) -> Result<(), ChannelError> {
        if a == b {
            return Err(ChannelError::SelfLink(a));
        }
        self.nodes.get(&a).ok_or(ChannelError::MissingNode(a))?;
        self.nodes.get(&b).ok_or(ChannelError::MissingNode(b))?;
        if linked {
            self.nodes.get_mut(&a).unwrap().links.insert(b);
            self.nodes.get_mut(&b).unwrap().links.insert(a);
        } else {
            self.nodes.get_mut(&a).unwrap().links.remove(&b);
            self.nodes.get_mut(&b).unwrap().links.remove(&a);
        }
        Ok(())
    }

    pub fn node_kind(&self, id: MeNodeId) -> Option<MeNodeKind> {
        self.nodes.get(&id).map(|node| node.kind)
    }

    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    /// Route channels for every component in deterministic order.
    ///
    /// A component with exactly one controller multiblock runs a multi-source
    /// BFS from that group; every member then chooses its lowest-ID neighbour
    /// at one hop closer as its tree parent. Channelled devices are allocated
    /// in ascending node-ID order, each claiming one slot on its face edge
    /// and every cable on its tree path; a device whose path is saturated or
    /// relays through another device starves. A component without a
    /// controller is ad-hoc and grants the first [`ADHOC_CHANNEL_CAP`]
    /// channelled devices by ID a channel. A component with several
    /// multiblocks or an oversized one conflicts and supplies nothing.
    pub fn solve_channels(&self) -> ChannelAllocation {
        let mut allocation = ChannelAllocation::default();
        for (&id, node) in &self.nodes {
            if matches!(node.kind, MeNodeKind::DenseCable | MeNodeKind::NormalCable) {
                allocation.used_channels.insert(id, 0);
            }
        }
        let mut visited = BTreeSet::new();
        for &start in self.nodes.keys() {
            if !visited.insert(start) {
                continue;
            }
            let mut members = vec![start];
            let mut queue = VecDeque::from([start]);
            while let Some(id) = queue.pop_front() {
                for &other in &self.nodes[&id].links {
                    if visited.insert(other) {
                        members.push(other);
                        queue.push_back(other);
                    }
                }
            }
            members.sort_unstable();
            self.solve_component(&members, &mut allocation);
        }
        allocation
    }

    fn solve_component(&self, members: &[MeNodeId], allocation: &mut ChannelAllocation) {
        let mut channelled = Vec::new();
        for &id in members {
            match self.nodes[&id].kind {
                MeNodeKind::Device {
                    requires_channel: true,
                } => channelled.push(id),
                MeNodeKind::Device {
                    requires_channel: false,
                } => {
                    allocation.online_devices.insert(id);
                }
                _ => {}
            }
        }
        let groups = self.controller_groups(members);
        match groups.as_slice() {
            [] => self.solve_adhoc(members, &channelled, allocation),
            [group] => {
                let within: BTreeSet<MeNodeId> = group.iter().copied().collect();
                let oversized = group.iter().any(|&controller| {
                    self.nodes[&controller]
                        .links
                        .iter()
                        .filter(|link| !within.contains(link))
                        .count()
                        > CONTROLLER_FACES
                });
                if oversized {
                    allocation
                        .conflicts
                        .push(ChannelConflict::InvalidControllerMultiblock {
                            controllers: group.clone(),
                        });
                    allocation.channel_starved_devices.extend(channelled);
                } else {
                    self.solve_controlled(group, &channelled, allocation);
                }
            }
            _ => {
                allocation
                    .conflicts
                    .push(ChannelConflict::ConflictingControllers {
                        groups: groups.iter().map(|group| group[0]).collect(),
                    });
                allocation.channel_starved_devices.extend(channelled);
            }
        }
    }

    /// Maximal directly-linked controller groups inside one component.
    fn controller_groups(&self, members: &[MeNodeId]) -> Vec<Vec<MeNodeId>> {
        let mut groups = Vec::new();
        let mut seen = BTreeSet::new();
        for &id in members {
            if self.nodes[&id].kind != MeNodeKind::Controller || !seen.insert(id) {
                continue;
            }
            let mut group = vec![id];
            let mut queue = VecDeque::from([id]);
            while let Some(controller) = queue.pop_front() {
                for &other in &self.nodes[&controller].links {
                    if self.nodes[&other].kind == MeNodeKind::Controller && seen.insert(other) {
                        group.push(other);
                        queue.push_back(other);
                    }
                }
            }
            group.sort_unstable();
            groups.push(group);
        }
        groups
    }

    /// Shortest-path tree rooted at `roots`; parents are the lowest-ID
    /// neighbour one hop closer to a root.
    fn spanning_tree(&self, roots: &[MeNodeId]) -> BTreeMap<MeNodeId, MeNodeId> {
        let mut distance: BTreeMap<MeNodeId, u32> = roots.iter().map(|&root| (root, 0)).collect();
        let mut queue: VecDeque<MeNodeId> = roots.iter().copied().collect();
        while let Some(id) = queue.pop_front() {
            let hop = distance[&id] + 1;
            for &other in &self.nodes[&id].links {
                if let std::collections::btree_map::Entry::Vacant(entry) = distance.entry(other) {
                    entry.insert(hop);
                    queue.push_back(other);
                }
            }
        }
        let mut parents = BTreeMap::new();
        for (&id, &hop) in &distance {
            if hop == 0 {
                continue;
            }
            let parent = self.nodes[&id]
                .links
                .iter()
                .filter(|link| distance.get(link) == Some(&(hop - 1)))
                .min()
                .copied()
                .unwrap();
            parents.insert(id, parent);
        }
        parents
    }

    /// Nodes on the tree path from `device` up to and excluding the root,
    /// ordered from the device towards the root's first hop.
    fn tree_path(&self, device: MeNodeId, parents: &BTreeMap<MeNodeId, MeNodeId>) -> Vec<MeNodeId> {
        let mut path = vec![device];
        while let Some(&parent) = parents.get(path.last().unwrap()) {
            path.push(parent);
        }
        path
    }

    fn solve_controlled(
        &self,
        group: &[MeNodeId],
        channelled: &[MeNodeId],
        allocation: &mut ChannelAllocation,
    ) {
        let parents = self.spanning_tree(group);
        let mut face_used: BTreeMap<(MeNodeId, MeNodeId), u32> = BTreeMap::new();
        for &device in channelled {
            let path = self.tree_path(device, &parents);
            let root = *path.last().unwrap();
            let first_hop = path[path.len() - 2];
            let transit = &path[1..path.len() - 1];
            let face_free =
                face_used.get(&(root, first_hop)).copied().unwrap_or(0) < CHANNELS_PER_FACE;
            let path_free = face_free
                && transit.iter().all(|&id| {
                    allocation.used_channels.get(&id).copied().unwrap_or(0)
                        < transit_capacity(self.nodes[&id].kind)
                });
            if path_free {
                *face_used.entry((root, first_hop)).or_default() += 1;
                for &id in transit {
                    *allocation.used_channels.get_mut(&id).unwrap() += 1;
                }
                allocation.online_devices.insert(device);
            } else {
                allocation.channel_starved_devices.insert(device);
            }
        }
    }

    fn solve_adhoc(
        &self,
        members: &[MeNodeId],
        channelled: &[MeNodeId],
        allocation: &mut ChannelAllocation,
    ) {
        let (online, starved) = channelled.split_at(channelled.len().min(ADHOC_CHANNEL_CAP));
        allocation.online_devices.extend(online.iter().copied());
        allocation
            .channel_starved_devices
            .extend(starved.iter().copied());
        // Report the load a self-organised tree rooted at the lowest member
        // ID would place on each cable, for parity with controlled solving.
        let parents = self.spanning_tree(&members[..1]);
        for &device in online {
            for &id in self.tree_path(device, &parents).iter().skip(1) {
                if transit_capacity(self.nodes[&id].kind) > 0 {
                    *allocation.used_channels.get_mut(&id).unwrap() += 1;
                }
            }
        }
    }
}

// ---- priority storage cells ------------------------------------------------

/// One storage cell. `used_bytes` follows the classic cell budget: every
/// distinct type costs `bytes_per_type` plus one byte per eight stored items.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StorageCell {
    pub id: CellId,
    pub priority: i32,
    pub max_bytes: u64,
    pub max_types: u16,
    pub bytes_per_type: u64,
    pub contents: BTreeMap<String, u64>,
}

/// Build errors for [`MeStorageIndex`] edits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StorageError {
    DuplicateCell(CellId),
    MissingCell(CellId),
}

/// Shortfall report for [`MeStorageIndex::reserve_batch`]. `missing` lists
/// each under-supplied item with the amount that could not be sourced, in
/// lexicographic order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MissingItems {
    pub missing: Vec<(String, u64)>,
}

/// Items held out of the index by a successful `reserve_batch`. Dropping it
/// keeps the items extracted; [`MeStorageIndex::release_reservation`] returns
/// them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StorageReservation {
    items: Vec<(String, u64)>,
}

impl StorageReservation {
    /// Items and counts held by this reservation, in lexicographic order.
    pub fn items(&self) -> &[(String, u64)] {
        &self.items
    }
}

impl StorageCell {
    pub fn new(
        id: CellId,
        priority: i32,
        max_bytes: u64,
        max_types: u16,
        bytes_per_type: u64,
    ) -> Self {
        Self {
            id,
            priority,
            max_bytes,
            max_types,
            bytes_per_type,
            contents: BTreeMap::new(),
        }
    }

    /// Bytes committed: `types * bytes_per_type + ceil(items / 8)`.
    pub fn used_bytes(&self) -> u64 {
        let items: u64 = self.contents.values().copied().sum();
        self.contents.len() as u64 * self.bytes_per_type + items.div_ceil(ITEMS_PER_BYTE)
    }

    pub fn free_bytes(&self) -> u64 {
        self.max_bytes.saturating_sub(self.used_bytes())
    }

    /// The most items of `item` this cell can still accept, respecting both
    /// the type budget and the byte budget.
    fn insert_capacity(&self, item: &str) -> u64 {
        let held = self.contents.get(item).copied().unwrap_or(0);
        let free = self.free_bytes();
        if held == 0 {
            if self.contents.len() >= usize::from(self.max_types) || free < self.bytes_per_type {
                return 0;
            }
            (free - self.bytes_per_type).saturating_mul(ITEMS_PER_BYTE)
        } else {
            (held.div_ceil(ITEMS_PER_BYTE) + free)
                .saturating_mul(ITEMS_PER_BYTE)
                .saturating_sub(held)
        }
    }
}

/// Priority-ordered collection of [`StorageCell`]s. Insertions prefer higher
/// priorities and, within one priority, cells already holding the item;
/// extractions drain lower priorities first.
#[derive(Clone, Default)]
pub struct MeStorageIndex {
    cells: BTreeMap<CellId, StorageCell>,
}

impl MeStorageIndex {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add_cell(&mut self, cell: StorageCell) -> Result<(), StorageError> {
        let std::collections::btree_map::Entry::Vacant(entry) = self.cells.entry(cell.id) else {
            return Err(StorageError::DuplicateCell(cell.id));
        };
        entry.insert(cell);
        Ok(())
    }

    pub fn remove_cell(&mut self, id: CellId) -> Result<StorageCell, StorageError> {
        self.cells.remove(&id).ok_or(StorageError::MissingCell(id))
    }

    pub fn cell(&self, id: CellId) -> Option<&StorageCell> {
        self.cells.get(&id)
    }

    pub fn cell_mut(&mut self, id: CellId) -> Option<&mut StorageCell> {
        self.cells.get_mut(&id)
    }

    pub fn cell_count(&self) -> usize {
        self.cells.len()
    }

    pub fn available(&self, item_id: &str) -> u64 {
        self.cells
            .values()
            .map(|cell| cell.contents.get(item_id).copied().unwrap_or(0))
            .sum()
    }

    pub fn total_items(&self) -> u64 {
        self.cells
            .values()
            .flat_map(|cell| cell.contents.values())
            .copied()
            .sum()
    }

    /// Insert up to `count` items, walking cells by descending priority; ties
    /// prefer cells already holding `item_id`, then lower cell IDs. Returns
    /// the accepted count.
    pub fn insert(&mut self, item_id: &str, count: u64) -> u64 {
        let mut order: Vec<CellId> = self.cells.keys().copied().collect();
        order.sort_by(|a, b| {
            let (cell_a, cell_b) = (&self.cells[a], &self.cells[b]);
            cell_b
                .priority
                .cmp(&cell_a.priority)
                .then(
                    cell_b
                        .contents
                        .contains_key(item_id)
                        .cmp(&cell_a.contents.contains_key(item_id)),
                )
                .then(a.cmp(b))
        });
        let mut remaining = count;
        for id in order {
            if remaining == 0 {
                break;
            }
            let cell = self.cells.get_mut(&id).unwrap();
            let accepted = remaining.min(cell.insert_capacity(item_id));
            if accepted > 0 {
                *cell.contents.entry(item_id.to_string()).or_default() += accepted;
                remaining -= accepted;
            }
        }
        count - remaining
    }

    /// Extract up to `count` items, draining cells by ascending priority then
    /// ascending cell ID. Returns the extracted count.
    pub fn extract(&mut self, item_id: &str, count: u64) -> u64 {
        let mut order: Vec<CellId> = self.cells.keys().copied().collect();
        order.sort_by(|a, b| {
            self.cells[a]
                .priority
                .cmp(&self.cells[b].priority)
                .then(a.cmp(b))
        });
        let mut remaining = count;
        for id in order {
            if remaining == 0 {
                break;
            }
            let cell = self.cells.get_mut(&id).unwrap();
            let Some(held) = cell.contents.get_mut(item_id) else {
                continue;
            };
            let taken = remaining.min(*held);
            *held -= taken;
            remaining -= taken;
            if *held == 0 {
                cell.contents.remove(item_id);
            }
        }
        count - remaining
    }

    /// Atomically extract every listed amount. When any item is short the
    /// index is untouched and the full shortfall is reported.
    pub fn reserve_batch(
        &mut self,
        items: &[(String, u64)],
    ) -> Result<StorageReservation, MissingItems> {
        let mut wanted: BTreeMap<String, u64> = BTreeMap::new();
        for (item, count) in items {
            if *count > 0 {
                *wanted.entry(item.clone()).or_default() = wanted
                    .get(item)
                    .copied()
                    .unwrap_or(0)
                    .saturating_add(*count);
            }
        }
        let mut missing = Vec::new();
        for (item, count) in &wanted {
            let short = count.saturating_sub(self.available(item));
            if short > 0 {
                missing.push((item.clone(), short));
            }
        }
        if !missing.is_empty() {
            return Err(MissingItems { missing });
        }
        let taken: Vec<(String, u64)> = wanted.into_iter().collect();
        for (item, count) in &taken {
            debug_assert_eq!(self.extract(item, *count), *count);
        }
        Ok(StorageReservation { items: taken })
    }

    /// Return a reservation's items through the normal insert path. Returns
    /// the count that no cell could take back.
    pub fn release_reservation(&mut self, reservation: StorageReservation) -> u64 {
        reservation
            .items
            .iter()
            .map(|(item, count)| count - self.insert(item, *count))
            .sum()
    }
}

// ---- auto-crafting planner -------------------------------------------------

/// One recipe: `inputs` consumed per run, `outputs` produced per run.
/// Multi-output patterns are how byproducts enter a plan.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CraftingPattern {
    pub id: u32,
    pub inputs: Vec<(String, u64)>,
    pub outputs: Vec<(String, u64)>,
}

/// One planned pattern invocation group: run `times` times, with `inputs`
/// and `outputs` already scaled by `times`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CraftStep {
    pub pattern_id: u32,
    pub times: u64,
    pub inputs: Vec<(String, u64)>,
    pub outputs: Vec<(String, u64)>,
}

/// A resolved request. `steps` is topologically ordered leaf-first: every
/// crafted ingredient is produced by an earlier step than its consumer.
/// `consumed_from_storage`, `produced_surplus`, and `missing_ingredients` are
/// exact per-item tallies; `estimated_cpu_bytes` counts every item that
/// flows through the simulated crafting CPU (step inputs plus step outputs).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CraftingPlan {
    pub steps: Vec<CraftStep>,
    pub consumed_from_storage: BTreeMap<String, u64>,
    pub produced_surplus: BTreeMap<String, u64>,
    pub missing_ingredients: BTreeMap<String, u64>,
    pub estimated_cpu_bytes: u64,
}

/// Hard planning failures. Partial coverage of a request is not a failure:
/// it is reported inside [`CraftingPlan::missing_ingredients`] instead.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CraftPlanError {
    /// A pattern loop was detected; the path lists the items from the first
    /// occurrence of the repeated item back to it, e.g. `[ingot, nugget,
    /// ingot]`.
    CyclicPattern(Vec<String>),
    /// Crafting `item` would exceed `max_depth` nested crafts.
    MaxDepthExceeded { item: String, depth: usize },
}

/// What [`MeCraftingPlanner::execute_plan`] committed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExecutionReport {
    /// Storage items the plan consumed, now held out of the index.
    pub reservation: StorageReservation,
    /// Items actually stored back (deliverables plus surplus), capped by
    /// remaining cell space.
    pub committed_outputs: BTreeMap<String, u64>,
    /// Deliverable or surplus items no cell could accept.
    pub uncommitted_outputs: BTreeMap<String, u64>,
}

impl CraftingPlan {
    /// Net items this plan delivers to its requester: produced plus
    /// storage-drawn plus missing minus step-consumed minus leftover surplus.
    /// For a well-formed plan this is exactly the requested item and count.
    pub fn deliverables(&self) -> BTreeMap<String, u64> {
        let mut balance: BTreeMap<String, i128> = BTreeMap::new();
        let mut merge = |items: &BTreeMap<String, u64>, sign: i128| {
            for (item, count) in items {
                *balance.entry(item.clone()).or_default() += sign * i128::from(*count);
            }
        };
        merge(&self.consumed_from_storage, 1);
        merge(&self.missing_ingredients, 1);
        merge(&self.produced_surplus, -1);
        for step in &self.steps {
            for (item, count) in &step.outputs {
                *balance.entry(item.clone()).or_default() += i128::from(*count);
            }
            for (item, count) in &step.inputs {
                *balance.entry(item.clone()).or_default() -= i128::from(*count);
            }
        }
        balance
            .into_iter()
            .filter(|(_, amount)| *amount > 0)
            .map(|(item, amount)| (item, amount as u64))
            .collect()
    }
}

struct PlanContext<'a> {
    storage: &'a MeStorageIndex,
    patterns: &'a [CraftingPattern],
    max_depth: usize,
    stack: Vec<String>,
    surplus: BTreeMap<String, u64>,
    consumed: BTreeMap<String, u64>,
    missing: BTreeMap<String, u64>,
    steps: Vec<CraftStep>,
    cpu_bytes: u64,
}

impl PlanContext<'_> {
    /// Source `need` units of `item`: surplus first, then storage, then a
    /// craft through the first pattern in slice order that outputs it.
    fn resolve(&mut self, item: &str, mut need: u64, depth: usize) -> Result<(), CraftPlanError> {
        if need == 0 {
            return Ok(());
        }
        if let Some(left) = self.surplus.get_mut(item) {
            let taken = need.min(*left);
            *left -= taken;
            need -= taken;
            if *left == 0 {
                self.surplus.remove(item);
            }
            if need == 0 {
                return Ok(());
            }
        }
        let drawn = self.consumed.get(item).copied().unwrap_or(0);
        let stored = self.storage.available(item).saturating_sub(drawn);
        let taken = need.min(stored);
        if taken > 0 {
            *self.consumed.entry(item.to_string()).or_default() += taken;
            need -= taken;
            if need == 0 {
                return Ok(());
            }
        }
        if let Some(position) = self.stack.iter().position(|open| open == item) {
            let mut cycle = self.stack[position..].to_vec();
            cycle.push(item.to_string());
            return Err(CraftPlanError::CyclicPattern(cycle));
        }
        if depth >= self.max_depth {
            return Err(CraftPlanError::MaxDepthExceeded {
                item: item.to_string(),
                depth,
            });
        }
        let Some(pattern) = self
            .patterns
            .iter()
            .find(|pattern| pattern.outputs.iter().any(|(out, _)| out == item))
        else {
            *self.missing.entry(item.to_string()).or_default() += need;
            return Ok(());
        };
        let per_run: u64 = pattern
            .outputs
            .iter()
            .filter(|(out, _)| out == item)
            .map(|(_, count)| *count)
            .sum();
        if per_run == 0 {
            *self.missing.entry(item.to_string()).or_default() += need;
            return Ok(());
        }
        let runs = need.div_ceil(per_run);
        self.stack.push(item.to_string());
        let mut failure = None;
        for (input, count) in &pattern.inputs {
            if let Err(error) = self.resolve(input, count.saturating_mul(runs), depth + 1) {
                failure = Some(error);
                break;
            }
        }
        self.stack.pop();
        if let Some(error) = failure {
            return Err(error);
        }
        let mut inputs = Vec::with_capacity(pattern.inputs.len());
        let mut input_total = 0_u64;
        for (input, count) in &pattern.inputs {
            let scaled = count.saturating_mul(runs);
            input_total = input_total.saturating_add(scaled);
            inputs.push((input.clone(), scaled));
        }
        let mut outputs = Vec::with_capacity(pattern.outputs.len());
        let mut output_total = 0_u64;
        for (out, count) in &pattern.outputs {
            let scaled = count.saturating_mul(runs);
            output_total = output_total.saturating_add(scaled);
            outputs.push((out.clone(), scaled));
            *self.surplus.entry(out.clone()).or_default() = self
                .surplus
                .get(out)
                .copied()
                .unwrap_or(0)
                .saturating_add(scaled);
        }
        let left = self.surplus.get_mut(item).unwrap();
        *left -= need;
        if *left == 0 {
            self.surplus.remove(item);
        }
        self.cpu_bytes = self
            .cpu_bytes
            .saturating_add(input_total)
            .saturating_add(output_total);
        self.steps.push(CraftStep {
            pattern_id: pattern.id,
            times: runs,
            inputs,
            outputs,
        });
        Ok(())
    }
}

/// Stateless planner: resolves a request into a [`CraftingPlan`] and commits
/// plans back into a [`MeStorageIndex`].
pub struct MeCraftingPlanner;

impl MeCraftingPlanner {
    /// Resolve `count` units of `target_item` against `storage` and
    /// `patterns`. Storage and surplus byproducts are consumed before new
    /// crafts are scheduled, so shared intermediates are produced once and
    /// reused. Pattern loops and over-deep recursion fail closed.
    pub fn plan_craft(
        target_item: &str,
        count: u64,
        storage: &MeStorageIndex,
        patterns: &[CraftingPattern],
        max_depth: usize,
    ) -> Result<CraftingPlan, CraftPlanError> {
        let mut context = PlanContext {
            storage,
            patterns,
            max_depth,
            stack: Vec::new(),
            surplus: BTreeMap::new(),
            consumed: BTreeMap::new(),
            missing: BTreeMap::new(),
            steps: Vec::new(),
            cpu_bytes: 0,
        };
        context.resolve(target_item, count, 0)?;
        Ok(CraftingPlan {
            steps: context.steps,
            consumed_from_storage: context.consumed,
            produced_surplus: context.surplus,
            missing_ingredients: context.missing,
            estimated_cpu_bytes: context.cpu_bytes,
        })
    }

    /// Atomically reserve the plan's storage inputs, then commit its
    /// deliverables and surplus back to storage. A short reservation leaves
    /// the index untouched.
    pub fn execute_plan(
        plan: &CraftingPlan,
        storage: &mut MeStorageIndex,
    ) -> Result<ExecutionReport, MissingItems> {
        let consumed: Vec<(String, u64)> = plan
            .consumed_from_storage
            .iter()
            .map(|(item, count)| (item.clone(), *count))
            .collect();
        let reservation = storage.reserve_batch(&consumed)?;
        let mut committed_outputs: BTreeMap<String, u64> = BTreeMap::new();
        let mut uncommitted_outputs: BTreeMap<String, u64> = BTreeMap::new();
        let outbound: Vec<(String, u64)> = plan
            .deliverables()
            .into_iter()
            .chain(
                plan.produced_surplus
                    .iter()
                    .map(|(item, count)| (item.clone(), *count)),
            )
            .collect();
        // Deliverables insert before surplus so the requester's items win
        // when the remaining cell space cannot take everything.
        for (item, count) in outbound {
            let accepted = storage.insert(&item, count);
            if accepted > 0 {
                *committed_outputs.entry(item.clone()).or_default() += accepted;
            }
            if accepted < count {
                *uncommitted_outputs.entry(item).or_default() += count - accepted;
            }
        }
        Ok(ExecutionReport {
            reservation,
            committed_outputs,
            uncommitted_outputs,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cell(id: CellId, priority: i32, max_types: u16, max_bytes: u64) -> StorageCell {
        StorageCell::new(id, priority, max_bytes, max_types, 8)
    }

    fn graph(edges: &[(MeNodeId, MeNodeId)], nodes: &[(MeNodeId, MeNodeKind)]) -> MeChannelGraph {
        let mut graph = MeChannelGraph::default();
        for &(id, kind) in nodes {
            graph.add_node(id, kind).unwrap();
        }
        for &(a, b) in edges {
            graph.set_link(a, b, true).unwrap();
        }
        graph
    }

    #[test]
    fn cell_byte_budget_matches_classic_cell_math() {
        let mut c = cell(1, 0, 63, 600);
        assert_eq!(c.used_bytes(), 0);
        c.contents.insert("a".into(), 1);
        assert_eq!(c.used_bytes(), 9);
        c.contents.insert("a".into(), 8);
        assert_eq!(c.used_bytes(), 9);
        c.contents.insert("b".into(), 9);
        assert_eq!(c.used_bytes(), 8 + 1 + 8 + 2);
        c.contents.insert("b".into(), 64);
        assert_eq!(c.used_bytes(), 16 + 9);
        // The type budget blocks a 64th type even with bytes to spare, and a
        // small byte budget caps items of a single type.
        let mut index = MeStorageIndex::new();
        index.add_cell(cell(2, 0, 63, 1_024)).unwrap();
        index.add_cell(cell(3, 0, 63, 9)).unwrap();
        for t in 0..63 {
            assert_eq!(index.insert(&format!("t{t}"), 12), 12);
        }
        assert_eq!(index.insert("u", 16), 8);
        assert_eq!(index.insert("t64", 1), 0);
    }

    #[test]
    fn insert_prefers_priority_then_existing_holders() {
        let mut index = MeStorageIndex::new();
        index.add_cell(cell(1, 10, 2, 1_000)).unwrap();
        index.add_cell(cell(2, 10, 2, 1_000)).unwrap();
        index.add_cell(cell(3, 5, 63, 1_000)).unwrap();
        index.insert("x", 8);
        index.insert("y", 8);
        // Same priority: cell 1 already holds x, so x refills it before cell 2.
        index.insert("x", 8);
        assert_eq!(index.cell(1).unwrap().contents["x"], 16);
        // Higher priority beats a lower cell that already holds the item:
        // cell 1 is type-full, so z lands in cell 2 ahead of cell 3's stack.
        index.cell_mut(3).unwrap().contents.insert("z".into(), 8);
        index.insert("z", 8);
        assert_eq!(index.cell(2).unwrap().contents["z"], 8);
        assert_eq!(index.cell(3).unwrap().contents["z"], 8);
    }

    #[test]
    fn extract_drains_lowest_priority_first() {
        let mut index = MeStorageIndex::new();
        index.add_cell(cell(1, 10, 63, 1_000)).unwrap();
        index.add_cell(cell(2, 0, 63, 1_000)).unwrap();
        index.cell_mut(1).unwrap().contents.insert("s".into(), 8);
        index.cell_mut(2).unwrap().contents.insert("s".into(), 8);
        index.extract("s", 8);
        assert_eq!(index.cell(2).unwrap().contents.get("s"), None);
        assert_eq!(index.cell(1).unwrap().contents["s"], 8);
    }

    #[test]
    fn reserve_batch_is_atomic() {
        let mut index = MeStorageIndex::new();
        index.add_cell(cell(1, 0, 63, 1_000)).unwrap();
        index.insert("a", 4);
        let before = index.available("a");
        let err = index
            .reserve_batch(&[("a".into(), 2), ("b".into(), 9)])
            .unwrap_err();
        assert_eq!(err.missing, vec![("b".to_string(), 9)]);
        assert_eq!(index.available("a"), before);
        let reservation = index
            .reserve_batch(&[("a".into(), 3), ("a".into(), 1)])
            .unwrap();
        assert_eq!(reservation.items(), &[("a".to_string(), 4)]);
        assert_eq!(index.available("a"), 0);
        assert_eq!(index.release_reservation(reservation), 0);
        assert_eq!(index.available("a"), 4);
    }

    #[test]
    fn controlled_network_allocates_and_starves_deterministically() {
        let nodes = [
            (0, MeNodeKind::Controller),
            (1, MeNodeKind::DenseCable),
            (2, MeNodeKind::NormalCable),
            (
                3,
                MeNodeKind::Device {
                    requires_channel: true,
                },
            ),
            (
                4,
                MeNodeKind::Device {
                    requires_channel: true,
                },
            ),
            (
                5,
                MeNodeKind::Device {
                    requires_channel: false,
                },
            ),
        ];
        let mut g = graph(&[(0, 1), (1, 2), (1, 3), (2, 4), (2, 5)], &nodes);
        let allocation = g.solve_channels();
        assert!(allocation.conflicts.is_empty());
        assert_eq!(allocation.online_devices, BTreeSet::from([3, 4, 5]));
        assert!(allocation.channel_starved_devices.is_empty());
        assert_eq!(allocation.used_channels, BTreeMap::from([(1, 2), (2, 1)]));
        // Device on a device path starves: 6 behind device 3.
        g.add_node(
            6,
            MeNodeKind::Device {
                requires_channel: true,
            },
        )
        .unwrap();
        g.set_link(3, 6, true).unwrap();
        let allocation = g.solve_channels();
        assert_eq!(allocation.channel_starved_devices, BTreeSet::from([6]));
    }

    #[test]
    fn adhoc_component_caps_at_eight_channels() {
        let mut nodes = vec![(0, MeNodeKind::NormalCable)];
        for id in 1..=10 {
            nodes.push((
                id,
                MeNodeKind::Device {
                    requires_channel: true,
                },
            ));
        }
        let edges: Vec<_> = (1..=10).map(|id| (0, id)).collect();
        let g = graph(&edges, &nodes);
        let allocation = g.solve_channels();
        assert_eq!(allocation.online_devices, (1..=8).collect::<BTreeSet<_>>());
        assert_eq!(
            allocation.channel_starved_devices,
            (9..=10).collect::<BTreeSet<_>>()
        );
        assert_eq!(allocation.used_channels[&0], 8);
    }

    #[test]
    fn controller_conflicts_fail_closed() {
        let nodes = [
            (0, MeNodeKind::Controller),
            (1, MeNodeKind::DenseCable),
            (2, MeNodeKind::Controller),
            (
                3,
                MeNodeKind::Device {
                    requires_channel: true,
                },
            ),
        ];
        let g = graph(&[(0, 1), (1, 2), (1, 3)], &nodes);
        let allocation = g.solve_channels();
        assert_eq!(
            allocation.conflicts,
            vec![ChannelConflict::ConflictingControllers { groups: vec![0, 2] }]
        );
        assert_eq!(allocation.channel_starved_devices, BTreeSet::from([3]));

        // Directly linked controllers are one multiblock; seven faces on it
        // is an invalid multiblock and also fails closed.
        let nodes = [
            (0, MeNodeKind::Controller),
            (1, MeNodeKind::Controller),
            (
                10,
                MeNodeKind::Device {
                    requires_channel: true,
                },
            ),
        ];
        let mut g = graph(&[(0, 1)], &nodes);
        for face in 20..27 {
            g.add_node(face, MeNodeKind::NormalCable).unwrap();
            g.set_link(0, face, true).unwrap();
        }
        g.set_link(20, 10, true).unwrap();
        let allocation = g.solve_channels();
        assert_eq!(
            allocation.conflicts,
            vec![ChannelConflict::InvalidControllerMultiblock {
                controllers: vec![0, 1]
            }]
        );
        assert_eq!(allocation.channel_starved_devices, BTreeSet::from([10]));
    }

    fn pattern(id: u32, inputs: &[(&str, u64)], outputs: &[(&str, u64)]) -> CraftingPattern {
        CraftingPattern {
            id,
            inputs: inputs.iter().map(|(i, n)| (i.to_string(), *n)).collect(),
            outputs: outputs.iter().map(|(o, n)| (o.to_string(), *n)).collect(),
        }
    }

    #[test]
    fn planner_reuses_storage_surplus_and_reports_missing() {
        let mut index = MeStorageIndex::new();
        index.add_cell(cell(1, 0, 63, 1_000)).unwrap();
        index.insert("rod", 2);
        index.insert("ingot", 3);
        // pick = 2 rod + 1 head + 1 slag; head = 3 ingot with 1 slag byproduct.
        let patterns = [
            pattern(1, &[("rod", 2), ("head", 1), ("slag", 1)], &[("pick", 1)]),
            pattern(2, &[("ingot", 3)], &[("head", 1), ("slag", 1)]),
        ];
        let plan = MeCraftingPlanner::plan_craft("pick", 1, &index, &patterns, 8).unwrap();
        // slag needed by pick arrives as head's byproduct: no second craft.
        assert_eq!(plan.steps.len(), 2);
        assert_eq!(plan.steps[0].pattern_id, 2);
        assert_eq!(plan.steps[1].pattern_id, 1);
        assert_eq!(plan.consumed_from_storage["rod"], 2);
        assert_eq!(plan.consumed_from_storage["ingot"], 3);
        assert!(plan.missing_ingredients.is_empty());
        assert!(plan.produced_surplus.is_empty());
        assert_eq!(
            plan.deliverables(),
            BTreeMap::from([("pick".to_string(), 1)])
        );
    }

    #[test]
    fn planner_fails_closed_on_pattern_cycles() {
        let patterns = [
            pattern(1, &[("nugget", 9)], &[("ingot", 1)]),
            pattern(2, &[("ingot", 1)], &[("nugget", 9)]),
        ];
        let index = MeStorageIndex::new();
        let error = MeCraftingPlanner::plan_craft("ingot", 2, &index, &patterns, 16).unwrap_err();
        assert_eq!(
            error,
            CraftPlanError::CyclicPattern(vec![
                "ingot".to_string(),
                "nugget".to_string(),
                "ingot".to_string()
            ])
        );
        // A cycle is only an error when the re-entered item truly needs a new
        // craft: here b needs a, and b's other input d produces exactly that a
        // as a byproduct, so the a<->b loop converges.
        let convergent = [
            pattern(1, &[("a", 1)], &[("t", 1)]),
            pattern(2, &[("b", 1)], &[("a", 1)]),
            pattern(3, &[("d", 1), ("a", 1)], &[("b", 1)]),
            pattern(4, &[("c", 1)], &[("d", 1), ("a", 1)]),
        ];
        let mut index = MeStorageIndex::new();
        index.add_cell(cell(1, 0, 63, 1_000)).unwrap();
        index.insert("c", 1);
        let plan = MeCraftingPlanner::plan_craft("t", 1, &index, &convergent, 16).unwrap();
        assert_eq!(plan.steps.len(), 4);
        assert_eq!(plan.consumed_from_storage["c"], 1);
        assert!(plan.missing_ingredients.is_empty());
        assert_eq!(plan.deliverables(), BTreeMap::from([("t".to_string(), 1)]));
    }

    #[test]
    fn planner_bounds_depth_and_marks_uncraftable() {
        let patterns = [
            pattern(1, &[("b", 1)], &[("a", 1)]),
            pattern(2, &[("c", 1)], &[("b", 1)]),
        ];
        let index = MeStorageIndex::new();
        assert_eq!(
            MeCraftingPlanner::plan_craft("a", 1, &index, &patterns, 1).unwrap_err(),
            CraftPlanError::MaxDepthExceeded {
                item: "b".to_string(),
                depth: 1
            }
        );
        let plan = MeCraftingPlanner::plan_craft("a", 1, &index, &patterns, 8).unwrap();
        assert_eq!(plan.missing_ingredients["c"], 1);
        let flat = MeCraftingPlanner::plan_craft("z", 3, &index, &patterns, 8).unwrap();
        assert_eq!(flat.missing_ingredients["z"], 3);
        assert!(flat.steps.is_empty());
    }

    #[test]
    fn execute_plan_reserves_inputs_and_commits_outputs() {
        let mut index = MeStorageIndex::new();
        index.add_cell(cell(1, 0, 63, 1_000)).unwrap();
        index.insert("stick", 8);
        let patterns = [pattern(7, &[("stick", 2)], &[("torch", 4)])];
        let plan = MeCraftingPlanner::plan_craft("torch", 5, &index, &patterns, 4).unwrap();
        let report = MeCraftingPlanner::execute_plan(&plan, &mut index).unwrap();
        assert_eq!(report.reservation.items(), &[("stick".to_string(), 4)]);
        assert_eq!(index.available("stick"), 4);
        // 8 torches crafted, 5 delivered, 3 surplus returned to storage.
        assert_eq!(report.committed_outputs["torch"], 8);
        assert_eq!(index.available("torch"), 8);
        assert!(report.uncommitted_outputs.is_empty());
    }
}
