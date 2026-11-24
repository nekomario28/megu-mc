// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later

//! Generic external-graph sync bridge: ANY kinetic, fluid, transport, or ME
//! storage graph — pure-Rust or bridged from a JVM mod — streams incremental
//! dirty-node [`ExternalGraphDelta`]s into the offload kernels and receives
//! deterministic, coalesced [`BridgeSyncReport`]s.
//!
//! The bridge carries no mod names and no block/item identity: every delta
//! addresses numeric node ids already assigned by the caller. Re-ingesting an
//! unchanged delta is a no-op (`skipped`), repeated deltas for one node inside
//! a batch coalesce last-wins, and application order is fixed by
//! [`ShadowKey`] so identical input batches always produce identical kernel
//! states regardless of arrival order within reason.

use std::collections::BTreeMap;

use crate::fluid_network::{FluidError, FluidNetwork};
use crate::kinetic_graph::{GraphError, KineticGraph, KineticNode};
use crate::me_network::{
    ChannelError, CellId, MeChannelGraph, MeNodeId, MeNodeKind, MeStorageIndex, StorageCell,
    StorageError,
};
use crate::region_scheduler::Region;
use crate::sleeping_transport::{SleepingTransport, TransportError};

/// Maximum deltas one [`OffloadSyncBridge::ingest`] call accepts — mirrors
/// `region_scheduler::MAX_BATCH_RECORDS`.
pub const MAX_INGEST_DELTAS: usize = 4096;

/// One dirty-node delta streamed by an external graph source.
///
/// State deltas are absolute ("ensure this node/link looks like this"), so a
/// source only needs to emit what changed. [`ExternalGraphDelta::TransportInsert`]
/// is the single event-style delta: it is applied every time and never
/// deduplicated, because re-delivering an item is meaningful work.
#[derive(Clone, Debug, PartialEq)]
pub enum ExternalGraphDelta {
    /// Upsert a kinetic node's absolute [`KineticNode`] input.
    KineticUpsert { id: u32, node: KineticNode },
    /// Remove a kinetic node and its edges.
    KineticRemove { id: u32 },
    /// Ensure a directed kinetic edge `speed[b] = speed[a] * ratio` exists.
    /// Re-linking an existing pair fails closed — send [`Self::KineticUnlink`]
    /// first.
    KineticLink { a: u32, b: u32, ratio: f64 },
    /// Remove the edge between `a` and `b` (either direction).
    KineticUnlink { a: u32, b: u32 },
    /// Upsert a fluid tank's capacity and elevation head.
    FluidTankUpsert {
        id: u32,
        capacity: u32,
        elevation: i32,
    },
    /// Set a tank's absolute contents. `amount == 0` drains the tank;
    /// `amount > 0` requires `fluid`. The network solver may subsequently
    /// redistribute connected tanks — this is ingress intent, not a freeze.
    FluidSet {
        id: u32,
        fluid: Option<u32>,
        amount: u32,
    },
    /// Set or remove a pumped fluid link (`head` lifts the `b` side when the
    /// call reads `set_link(a, b, head)`).
    FluidLink {
        a: u32,
        b: u32,
        head: Option<i32>,
    },
    /// Register a sleeping-transport machine. There is no in-place resize:
    /// re-declaring a different `(capacity, delay)` for a live id fails.
    TransportMachine {
        id: u32,
        capacity: u32,
        delay: u64,
    },
    /// Set a transport machine's downstream (or clear it with `None`).
    TransportLink { id: u32, downstream: Option<u32> },
    /// Deliver one item into a transport machine's input FIFO (event delta —
    /// never deduplicated).
    TransportInsert { id: u32, item: u32 },
    /// Upsert an ME channel-graph node. A kind change removes and re-adds the
    /// node deterministically.
    MeNodeUpsert { id: MeNodeId, kind: MeNodeKind },
    /// Remove an ME channel-graph node and its links.
    MeNodeRemove { id: MeNodeId },
    /// Set or clear an undirected ME channel link.
    MeLink {
        a: MeNodeId,
        b: MeNodeId,
        linked: bool,
    },
    /// Upsert a priority storage cell (contents included).
    MeCellUpsert { cell: StorageCell },
    /// Remove a storage cell, returning its contents to the caller's care.
    MeCellRemove { id: CellId },
}

/// Errors surfaced by [`GraphDeltaSink`] implementations and the bridge.
#[derive(Clone, Debug, PartialEq)]
pub enum BridgeError {
    /// The sink does not implement this delta family (e.g. ME deltas into a
    /// [`Region`], which owns no ME network).
    UnsupportedDelta,
    /// The delta is malformed regardless of sink state.
    InvalidDelta(&'static str),
    /// More than [`MAX_INGEST_DELTAS`] deltas in one batch.
    BatchTooLarge { len: usize, max: usize },
    Kinetic(GraphError),
    Fluid(FluidError),
    Transport(TransportError),
    Channel(ChannelError),
    Storage(StorageError),
}

impl From<GraphError> for BridgeError {
    fn from(e: GraphError) -> Self {
        Self::Kinetic(e)
    }
}
impl From<FluidError> for BridgeError {
    fn from(e: FluidError) -> Self {
        Self::Fluid(e)
    }
}
impl From<TransportError> for BridgeError {
    fn from(e: TransportError) -> Self {
        Self::Transport(e)
    }
}
impl From<ChannelError> for BridgeError {
    fn from(e: ChannelError) -> Self {
        Self::Channel(e)
    }
}
impl From<StorageError> for BridgeError {
    fn from(e: StorageError) -> Self {
        Self::Storage(e)
    }
}

/// Apply-side contract: one kernel (or composite target) that accepts
/// [`ExternalGraphDelta`]s. Implementations must be deterministic and
/// fail closed — a rejected delta leaves the sink untouched.
pub trait GraphDeltaSink {
    fn apply(&mut self, delta: &ExternalGraphDelta) -> Result<(), BridgeError>;
}

fn unsupported() -> BridgeError {
    BridgeError::UnsupportedDelta
}

fn kinetic_upsert(graph: &mut KineticGraph, id: u32, node: KineticNode) -> Result<(), BridgeError> {
    if graph.solution(id).is_some() {
        graph.update_node(id, node)?;
    } else {
        graph.add_node(id, node)?;
    }
    Ok(())
}

fn fluid_set(
    net: &mut FluidNetwork,
    id: u32,
    fluid: Option<u32>,
    amount: u32,
) -> Result<(), BridgeError> {
    if amount > 0 && fluid.is_none() {
        return Err(BridgeError::InvalidDelta("fluid set requires a fluid id"));
    }
    let current = net.tank(id).ok_or(FluidError::MissingTank(id))?;
    if current.amount > 0 {
        net.extract(id, current.amount)?;
    }
    if amount > 0 {
        net.insert(id, fluid.unwrap(), amount)?;
    }
    Ok(())
}

impl GraphDeltaSink for KineticGraph {
    fn apply(&mut self, delta: &ExternalGraphDelta) -> Result<(), BridgeError> {
        match *delta {
            ExternalGraphDelta::KineticUpsert { id, node } => {
                kinetic_upsert(self, id, node)
            }
            ExternalGraphDelta::KineticRemove { id } => {
                self.remove_node(id)?;
                Ok(())
            }
            ExternalGraphDelta::KineticLink { a, b, ratio } => {
                self.connect(a, b, ratio)?;
                Ok(())
            }
            ExternalGraphDelta::KineticUnlink { a, b } => {
                self.disconnect(a, b)?;
                Ok(())
            }
            _ => Err(unsupported()),
        }
    }
}

impl GraphDeltaSink for FluidNetwork {
    fn apply(&mut self, delta: &ExternalGraphDelta) -> Result<(), BridgeError> {
        match *delta {
            ExternalGraphDelta::FluidTankUpsert {
                id,
                capacity,
                elevation,
            } => match self.set_tank(id, capacity, elevation) {
                Ok(()) => Ok(()),
                Err(FluidError::MissingTank(_)) => self.add_tank(id, capacity, elevation).map_err(Into::into),
                Err(e) => Err(e.into()),
            },
            ExternalGraphDelta::FluidSet {
                id,
                fluid,
                amount,
            } => fluid_set(self, id, fluid, amount),
            ExternalGraphDelta::FluidLink { a, b, head } => {
                self.set_link(a, b, head)?;
                Ok(())
            }
            _ => Err(unsupported()),
        }
    }
}

impl GraphDeltaSink for SleepingTransport {
    fn apply(&mut self, delta: &ExternalGraphDelta) -> Result<(), BridgeError> {
        match *delta {
            ExternalGraphDelta::TransportMachine {
                id,
                capacity,
                delay,
            } => {
                self.add_machine(id, capacity, delay)?;
                Ok(())
            }
            ExternalGraphDelta::TransportLink { id, downstream } => {
                self.set_downstream(id, downstream)?;
                Ok(())
            }
            ExternalGraphDelta::TransportInsert { id, item } => {
                self.insert(id, item)?;
                Ok(())
            }
            _ => Err(unsupported()),
        }
    }
}

/// Combined ME channel-routing + priority-storage target for
/// [`ExternalGraphDelta::Me*`] deltas.
#[derive(Default)]
pub struct MeGraphTarget {
    pub channels: MeChannelGraph,
    pub storage: MeStorageIndex,
}

impl GraphDeltaSink for MeGraphTarget {
    fn apply(&mut self, delta: &ExternalGraphDelta) -> Result<(), BridgeError> {
        match delta {
            ExternalGraphDelta::MeNodeUpsert { id, kind } => {
                match self.channels.node_kind(*id) {
                    Some(existing) if existing == *kind => Ok(()),
                    Some(_) => {
                        self.channels.remove_node(*id)?;
                        self.channels.add_node(*id, *kind)?;
                        Ok(())
                    }
                    None => {
                        self.channels.add_node(*id, *kind)?;
                        Ok(())
                    }
                }
            }
            ExternalGraphDelta::MeNodeRemove { id } => {
                self.channels.remove_node(*id)?;
                Ok(())
            }
            ExternalGraphDelta::MeLink { a, b, linked } => {
                self.channels.set_link(*a, *b, *linked)?;
                Ok(())
            }
            ExternalGraphDelta::MeCellUpsert { cell } => {
                if self.storage.cell(cell.id).is_some() {
                    self.storage.remove_cell(cell.id)?;
                }
                self.storage.add_cell(cell.clone())?;
                Ok(())
            }
            ExternalGraphDelta::MeCellRemove { id } => {
                self.storage.remove_cell(*id)?;
                Ok(())
            }
            _ => Err(unsupported()),
        }
    }
}

/// Region-composite sink: routes kinetic/fluid/transport deltas into a
/// [`Region`]'s kernels. ME deltas are rejected — a region owns no ME
/// network; bridge them into a [`MeGraphTarget`] instead.
impl GraphDeltaSink for Region {
    fn apply(&mut self, delta: &ExternalGraphDelta) -> Result<(), BridgeError> {
        match *delta {
            ExternalGraphDelta::KineticUpsert { id, node } => {
                kinetic_upsert(&mut self.kinetic, id, node)
            }
            ExternalGraphDelta::KineticRemove { id } => {
                self.kinetic.remove_node(id)?;
                Ok(())
            }
            ExternalGraphDelta::KineticLink { a, b, ratio } => {
                self.kinetic.connect(a, b, ratio)?;
                Ok(())
            }
            ExternalGraphDelta::KineticUnlink { a, b } => {
                self.kinetic.disconnect(a, b)?;
                Ok(())
            }
            ExternalGraphDelta::FluidTankUpsert {
                id,
                capacity,
                elevation,
            } => match self.set_tank(id, capacity, elevation) {
                Ok(()) => Ok(()),
                Err(FluidError::MissingTank(_)) => self.add_tank(id, capacity, elevation).map_err(Into::into),
                Err(e) => Err(e.into()),
            },
            ExternalGraphDelta::FluidSet {
                id,
                fluid,
                amount,
            } => {
                if amount > 0 && fluid.is_none() {
                    return Err(BridgeError::InvalidDelta("fluid set requires a fluid id"));
                }
                let current = self
                    .fluid()
                    .tank(id)
                    .ok_or(FluidError::MissingTank(id))?;
                if current.amount > 0 {
                    self.extract_fluid(id, current.amount)?;
                }
                if amount > 0 {
                    self.insert_fluid(id, fluid.unwrap(), amount)?;
                }
                Ok(())
            }
            ExternalGraphDelta::FluidLink { a, b, head } => {
                self.set_fluid_link(a, b, head)?;
                Ok(())
            }
            ExternalGraphDelta::TransportMachine {
                id,
                capacity,
                delay,
            } => {
                self.add_machine(id, capacity, delay)?;
                Ok(())
            }
            ExternalGraphDelta::TransportLink { id, downstream } => {
                self.set_downstream(id, downstream)?;
                Ok(())
            }
            ExternalGraphDelta::TransportInsert { id, item } => {
                self.insert_item(id, item)?;
                Ok(())
            }
            _ => Err(unsupported()),
        }
    }
}

/// Shadow bookkeeping key: one entry per deduplicated node/link/cell. The
/// declaration order IS the deterministic apply order — node upserts always
/// precede the pair links that reference them, tank upserts precede fluid
/// contents, contents precede links, machine registration precedes
/// downstream/insert work.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum ShadowKey {
    KineticNode(u32),
    KineticPair(u32, u32),
    FluidTank(u32),
    FluidContent(u32),
    FluidPair(u32, u32),
    TransportMachine(u32),
    TransportDownstream(u32),
    MeNode(u32),
    MePair(u32, u32),
    MeCell(u64),
}

/// Last applied descriptor per [`ShadowKey`]; ingest skips a delta whose
/// resulting mark already matches.
#[derive(Clone, Debug, PartialEq)]
enum ShadowMark {
    KineticNode(KineticNode),
    KineticLink { a: u32, b: u32, ratio_bits: u64 },
    KineticAbsent,
    KineticUnlinked,
    FluidTank { capacity: u32, elevation: i32 },
    FluidContent { fluid: Option<u32>, amount: u32 },
    FluidLink { head: Option<i64> },
    FluidUnlinked,
    TransportMachine { capacity: u32, delay: u64 },
    TransportDownstream { downstream: Option<u32> },
    MeNode { kind: MeNodeKind },
    MeAbsent,
    MeLinked,
    MeUnlinked,
    MeCell(StorageCell),
    MeCellAbsent,
}

fn unordered_pair(a: u32, b: u32) -> (u32, u32) {
    if a <= b {
        (a, b)
    } else {
        (b, a)
    }
}

/// Canonical directed ratio for an undirected kinetic edge: for the
/// unordered pair `{a, b}` the mark stores the ratio oriented `lo -> hi`,
/// where `connect(lo, hi, r)` means `speed[hi] = speed[lo] * r`. `a == hi`
/// inverts the ratio so both call directions dedup identically.
fn kinetic_canonical_ratio(a: u32, b: u32, ratio: f64) -> f64 {
    if unordered_pair(a, b).0 == a {
        ratio
    } else {
        ratio.recip()
    }
}

/// Canonical pump head for a fluid link, stored as `i64` so negating
/// `i32::MIN` cannot overflow: head is expressed `lo -> hi` for the
/// unordered pair `{a, b}`.
fn fluid_canonical_head(a: u32, b: u32, head: Option<i32>) -> Option<i64> {
    head.map(|h| {
        let h = i64::from(h);
        if unordered_pair(a, b).0 == a {
            h
        } else {
            -h
        }
    })
}

/// Result of one [`OffloadSyncBridge::ingest`] batch.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct BridgeSyncReport {
    /// Deltas handed to `ingest`.
    pub received: usize,
    /// State deltas dropped because a later delta in the same batch wrote
    /// the same shadow key (last-wins coalescing).
    pub coalesced: usize,
    /// Deltas skipped because the shadow already held the resulting mark
    /// (unchanged nodes never re-solve the kernels).
    pub skipped: usize,
    /// Deltas actually applied to the sink.
    pub applied: usize,
    /// Deltas the sink rejected, in deterministic apply order. A failure
    /// never aborts the batch and never updates the shadow, so the same
    /// delta retries cleanly on the next ingest.
    pub failed: Vec<BridgeFailure>,
}

/// One rejected delta plus its sink error.
#[derive(Clone, Debug, PartialEq)]
pub struct BridgeFailure {
    pub delta: ExternalGraphDelta,
    pub error: BridgeError,
}

/// Bridges an external dirty-delta stream into any [`GraphDeltaSink`]. Owns
/// the shadow state used for unchanged-node skipping and same-batch
/// coalescing; the sink remains directly readable through
/// [`Self::sink`] for solutions, allocations, and exports.
pub struct OffloadSyncBridge<S> {
    sink: S,
    shadow: BTreeMap<ShadowKey, ShadowMark>,
}

impl<S: GraphDeltaSink> OffloadSyncBridge<S> {
    pub fn new(sink: S) -> Self {
        Self {
            sink,
            shadow: BTreeMap::new(),
        }
    }

    pub fn sink(&self) -> &S {
        &self.sink
    }

    pub fn sink_mut(&mut self) -> &mut S {
        &mut self.sink
    }

    pub fn into_sink(self) -> S {
        self.sink
    }

    /// Ingest one batch of dirty-node deltas.
    ///
    /// State deltas coalesce last-wins per shadow key, then apply in
    /// [`ShadowKey`] order; [`ExternalGraphDelta::TransportInsert`] events
    /// always apply, in their original relative order, after all state work.
    /// Unchanged deltas (shadow mark already equals the resulting mark) are
    /// skipped without touching the sink.
    pub fn ingest(
        &mut self,
        deltas: &[ExternalGraphDelta],
    ) -> Result<BridgeSyncReport, BridgeError> {
        if deltas.len() > MAX_INGEST_DELTAS {
            return Err(BridgeError::BatchTooLarge {
                len: deltas.len(),
                max: MAX_INGEST_DELTAS,
            });
        }
        let mut report = BridgeSyncReport {
            received: deltas.len(),
            ..BridgeSyncReport::default()
        };

        // Last-wins index per shadow key + event positions, one pass.
        let mut last: BTreeMap<ShadowKey, usize> = BTreeMap::new();
        let mut events: Vec<usize> = Vec::new();
        for (index, delta) in deltas.iter().enumerate() {
            match shadow_key(delta) {
                Some(key) => {
                    last.insert(key, index);
                }
                None => events.push(index),
            }
        }
        report.coalesced = deltas.len() - last.len() - events.len();

        for (&key, &index) in &last {
            let delta = &deltas[index];
            let Some(mark) = resulting_mark(delta) else {
                continue;
            };
            if self.shadow.get(&key) == Some(&mark) {
                report.skipped += 1;
                continue;
            }
            match self.sink.apply(delta) {
                Ok(()) => {
                    self.commit_shadow(key, mark, delta);
                    report.applied += 1;
                }
                Err(error) => report.failed.push(BridgeFailure {
                    delta: delta.clone(),
                    error,
                }),
            }
        }
        for index in events {
            let delta = &deltas[index];
            match self.sink.apply(delta) {
                Ok(()) => report.applied += 1,
                Err(error) => report.failed.push(BridgeFailure {
                    delta: delta.clone(),
                    error,
                }),
            }
        }
        Ok(report)
    }

    /// Record a successful apply plus any shadow bookkeeping the op implies
    /// beyond its own key (node removals orphan their pair marks).
    fn commit_shadow(&mut self, key: ShadowKey, mark: ShadowMark, delta: &ExternalGraphDelta) {
        match delta {
            ExternalGraphDelta::KineticRemove { id } => {
                self.shadow.remove(&ShadowKey::KineticNode(*id));
                let doomed: Vec<ShadowKey> = self
                    .shadow
                    .keys()
                    .filter(|k| matches!(k, ShadowKey::KineticPair(a, b) if *a == *id || *b == *id))
                    .copied()
                    .collect();
                for k in doomed {
                    self.shadow.insert(k, ShadowMark::KineticUnlinked);
                }
            }
            ExternalGraphDelta::MeNodeRemove { id } => {
                self.shadow.remove(&ShadowKey::MeNode(*id));
                let doomed: Vec<ShadowKey> = self
                    .shadow
                    .keys()
                    .filter(|k| matches!(k, ShadowKey::MePair(a, b) if *a == *id || *b == *id))
                    .copied()
                    .collect();
                for k in doomed {
                    self.shadow.insert(k, ShadowMark::MeUnlinked);
                }
            }
            _ => {}
        }
        self.shadow.insert(key, mark);
    }
}

/// Shadow key for a state delta; `None` for event deltas that always apply.
fn shadow_key(delta: &ExternalGraphDelta) -> Option<ShadowKey> {
    Some(match delta {
        ExternalGraphDelta::KineticUpsert { id, .. } | ExternalGraphDelta::KineticRemove { id } => {
            ShadowKey::KineticNode(*id)
        }
        ExternalGraphDelta::KineticLink { a, b, .. } | ExternalGraphDelta::KineticUnlink { a, b } => {
            let (lo, hi) = unordered_pair(*a, *b);
            ShadowKey::KineticPair(lo, hi)
        }
        ExternalGraphDelta::FluidTankUpsert { id, .. } => ShadowKey::FluidTank(*id),
        ExternalGraphDelta::FluidSet { id, .. } => ShadowKey::FluidContent(*id),
        ExternalGraphDelta::FluidLink { a, b, .. } => {
            let (lo, hi) = unordered_pair(*a, *b);
            ShadowKey::FluidPair(lo, hi)
        }
        ExternalGraphDelta::TransportMachine { id, .. } => ShadowKey::TransportMachine(*id),
        ExternalGraphDelta::TransportLink { id, .. } => ShadowKey::TransportDownstream(*id),
        ExternalGraphDelta::TransportInsert { .. } => return None,
        ExternalGraphDelta::MeNodeUpsert { id, .. } | ExternalGraphDelta::MeNodeRemove { id } => {
            ShadowKey::MeNode(*id)
        }
        ExternalGraphDelta::MeLink { a, b, .. } => {
            let (lo, hi) = unordered_pair(*a, *b);
            ShadowKey::MePair(lo, hi)
        }
        ExternalGraphDelta::MeCellUpsert { cell } => ShadowKey::MeCell(cell.id),
        ExternalGraphDelta::MeCellRemove { id } => ShadowKey::MeCell(*id),
    })
}

/// The shadow mark a successfully applied delta leaves behind.
fn resulting_mark(delta: &ExternalGraphDelta) -> Option<ShadowMark> {
    Some(match delta {
        ExternalGraphDelta::KineticUpsert { node, .. } => ShadowMark::KineticNode(*node),
        ExternalGraphDelta::KineticRemove { .. } => ShadowMark::KineticAbsent,
        ExternalGraphDelta::KineticLink { a, b, ratio } => {
            let canonical = kinetic_canonical_ratio(*a, *b, *ratio);
            ShadowMark::KineticLink {
                a: unordered_pair(*a, *b).0,
                b: unordered_pair(*a, *b).1,
                ratio_bits: canonical.to_bits(),
            }
        }
        ExternalGraphDelta::KineticUnlink { .. } => ShadowMark::KineticUnlinked,
        ExternalGraphDelta::FluidTankUpsert {
            capacity,
            elevation,
            ..
        } => ShadowMark::FluidTank {
            capacity: *capacity,
            elevation: *elevation,
        },
        ExternalGraphDelta::FluidSet {
            fluid, amount, ..
        } => ShadowMark::FluidContent {
            fluid: *fluid,
            amount: *amount,
        },
        ExternalGraphDelta::FluidLink { a, b, head } => match head {
            None => ShadowMark::FluidUnlinked,
            Some(_) => ShadowMark::FluidLink {
                head: fluid_canonical_head(*a, *b, *head),
            },
        },
        ExternalGraphDelta::MeNodeUpsert { kind, .. } => ShadowMark::MeNode { kind: *kind },
        ExternalGraphDelta::MeNodeRemove { .. } => ShadowMark::MeAbsent,
        ExternalGraphDelta::MeLink { linked, .. } => {
            if *linked {
                ShadowMark::MeLinked
            } else {
                ShadowMark::MeUnlinked
            }
        }
        ExternalGraphDelta::MeCellUpsert { cell } => ShadowMark::MeCell(cell.clone()),
        ExternalGraphDelta::MeCellRemove { .. } => ShadowMark::MeCellAbsent,
        ExternalGraphDelta::TransportMachine {
            capacity, delay, ..
        } => ShadowMark::TransportMachine {
            capacity: *capacity,
            delay: *delay,
        },
        ExternalGraphDelta::TransportLink { downstream, .. } => ShadowMark::TransportDownstream {
            downstream: *downstream,
        },
        ExternalGraphDelta::TransportInsert { .. } => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::region_scheduler::RegionKey;

    fn drive_node(capacity: f64, speed: f64) -> KineticNode {
        KineticNode {
            capacity,
            load: 0.0,
            driven_speed: Some(speed),
        }
    }

    #[test]
    fn empty_and_oversized_batches_fail_cleanly() {
        let mut bridge = OffloadSyncBridge::new(KineticGraph::default());
        assert_eq!(bridge.ingest(&[]).unwrap(), BridgeSyncReport::default());
        let deltas = vec![
            ExternalGraphDelta::KineticRemove { id: 0 };
            MAX_INGEST_DELTAS + 1
        ];
        assert_eq!(
            bridge.ingest(&deltas),
            Err(BridgeError::BatchTooLarge {
                len: MAX_INGEST_DELTAS + 1,
                max: MAX_INGEST_DELTAS,
            })
        );
        // The rejected batch leaves the bridge untouched.
        assert_eq!(bridge.ingest(&[]).unwrap(), BridgeSyncReport::default());
    }

    #[test]
    fn kinetic_deltas_apply_in_key_order_and_skip_unchanged() {
        let mut bridge = OffloadSyncBridge::new(KineticGraph::default());
        // The link is listed before its nodes; ShadowKey order still applies
        // node upserts first so the connect succeeds.
        let report = bridge
            .ingest(&[
                ExternalGraphDelta::KineticLink {
                    a: 1,
                    b: 2,
                    ratio: 2.0,
                },
                ExternalGraphDelta::KineticUpsert {
                    id: 2,
                    node: KineticNode::default(),
                },
                ExternalGraphDelta::KineticUpsert {
                    id: 1,
                    node: drive_node(10.0, 4.0),
                },
            ])
            .unwrap();
        assert_eq!((report.received, report.applied), (3, 3));
        assert!(report.failed.is_empty());
        assert_eq!(bridge.sink().solution(2).unwrap().speed, 8.0);

        // Re-ingesting identical state deltas skips the sink entirely.
        let report = bridge
            .ingest(&[
                ExternalGraphDelta::KineticUpsert {
                    id: 1,
                    node: drive_node(10.0, 4.0),
                },
                ExternalGraphDelta::KineticLink {
                    a: 1,
                    b: 2,
                    ratio: 2.0,
                },
            ])
            .unwrap();
        assert_eq!((report.skipped, report.applied), (2, 0));
    }

    #[test]
    fn same_batch_state_deltas_coalesce_last_wins() {
        let mut bridge = OffloadSyncBridge::new(KineticGraph::default());
        let report = bridge
            .ingest(&[
                ExternalGraphDelta::KineticUpsert {
                    id: 1,
                    node: drive_node(10.0, 4.0),
                },
                ExternalGraphDelta::KineticUpsert {
                    id: 1,
                    node: drive_node(20.0, 4.0),
                },
            ])
            .unwrap();
        assert_eq!(
            (report.received, report.coalesced, report.applied),
            (2, 1, 1)
        );
        assert_eq!(bridge.sink().solution(1).unwrap().capacity, 20.0);
    }

    #[test]
    fn reversed_pair_deltas_share_one_canonical_shadow_mark() {
        let mut bridge = OffloadSyncBridge::new(KineticGraph::default());
        bridge
            .ingest(&[
                ExternalGraphDelta::KineticUpsert {
                    id: 1,
                    node: drive_node(10.0, 4.0),
                },
                ExternalGraphDelta::KineticUpsert {
                    id: 2,
                    node: KineticNode::default(),
                },
            ])
            .unwrap();
        // Two spellings of the same undirected edge in one batch coalesce;
        // only the last (connect(2, 1, 0.5)) reaches the sink.
        let report = bridge
            .ingest(&[
                ExternalGraphDelta::KineticLink {
                    a: 1,
                    b: 2,
                    ratio: 2.0,
                },
                ExternalGraphDelta::KineticLink {
                    a: 2,
                    b: 1,
                    ratio: 0.5,
                },
            ])
            .unwrap();
        assert_eq!((report.coalesced, report.applied), (1, 1));
        assert_eq!(bridge.sink().solution(2).unwrap().speed, 8.0);

        // A later batch expressing the identical edge from the other side
        // produces the same canonical mark and is skipped.
        let report = bridge
            .ingest(&[ExternalGraphDelta::KineticLink {
                a: 2,
                b: 1,
                ratio: 0.5,
            }])
            .unwrap();
        assert_eq!((report.skipped, report.applied), (1, 0));
    }

    #[test]
    fn node_removal_orphans_pair_marks_and_repeats_skip() {
        let mut bridge = OffloadSyncBridge::new(KineticGraph::default());
        bridge
            .ingest(&[
                ExternalGraphDelta::KineticUpsert {
                    id: 1,
                    node: drive_node(5.0, 1.0),
                },
                ExternalGraphDelta::KineticUpsert {
                    id: 2,
                    node: KineticNode::default(),
                },
                ExternalGraphDelta::KineticLink {
                    a: 1,
                    b: 2,
                    ratio: 1.0,
                },
            ])
            .unwrap();
        let report = bridge
            .ingest(&[ExternalGraphDelta::KineticRemove { id: 1 }])
            .unwrap();
        assert_eq!(report.applied, 1);
        // The shadow records absence, so a repeated remove skips rather than
        // failing against the sink's MissingNode.
        let report = bridge
            .ingest(&[ExternalGraphDelta::KineticRemove { id: 1 }])
            .unwrap();
        assert_eq!((report.skipped, report.applied), (1, 0));
        // Re-linking the orphaned pair retries the sink and fails closed.
        let report = bridge
            .ingest(&[ExternalGraphDelta::KineticLink {
                a: 1,
                b: 2,
                ratio: 1.0,
            }])
            .unwrap();
        assert_eq!(report.failed.len(), 1);
        assert_eq!(
            report.failed[0].error,
            BridgeError::Kinetic(GraphError::MissingNode(1))
        );
    }

    #[test]
    fn failed_deltas_leave_no_shadow_and_retry_cleanly() {
        let mut bridge = OffloadSyncBridge::new(KineticGraph::default());
        let report = bridge
            .ingest(&[ExternalGraphDelta::KineticLink {
                a: 1,
                b: 2,
                ratio: 1.0,
            }])
            .unwrap();
        assert_eq!(report.failed.len(), 1);
        assert_eq!(
            report.failed[0].error,
            BridgeError::Kinetic(GraphError::MissingNode(1))
        );
        // The failed link wrote no shadow mark, so the identical batch plus
        // the missing nodes succeeds on retry.
        let report = bridge
            .ingest(&[
                ExternalGraphDelta::KineticLink {
                    a: 1,
                    b: 2,
                    ratio: 1.0,
                },
                ExternalGraphDelta::KineticUpsert {
                    id: 1,
                    node: drive_node(5.0, 2.0),
                },
                ExternalGraphDelta::KineticUpsert {
                    id: 2,
                    node: KineticNode::default(),
                },
            ])
            .unwrap();
        assert_eq!((report.applied, report.failed.len()), (3, 0));
        assert!(bridge.sink().solution(2).is_some());
    }

    #[test]
    fn unsupported_deltas_fail_without_aborting_the_batch() {
        let mut bridge = OffloadSyncBridge::new(KineticGraph::default());
        let report = bridge
            .ingest(&[
                ExternalGraphDelta::KineticUpsert {
                    id: 1,
                    node: drive_node(5.0, 1.0),
                },
                ExternalGraphDelta::MeNodeUpsert {
                    id: 1,
                    kind: MeNodeKind::Controller,
                },
                ExternalGraphDelta::TransportInsert { id: 1, item: 9 },
            ])
            .unwrap();
        assert_eq!(report.applied, 1);
        assert_eq!(report.failed.len(), 2);
        assert!(report
            .failed
            .iter()
            .all(|f| f.error == BridgeError::UnsupportedDelta));
        assert!(bridge.sink().solution(1).is_some());
    }

    #[test]
    fn transport_insert_events_are_never_deduplicated() {
        let mut bridge = OffloadSyncBridge::new(SleepingTransport::default());
        let report = bridge
            .ingest(&[
                ExternalGraphDelta::TransportMachine {
                    id: 7,
                    capacity: 4,
                    delay: 1,
                },
                ExternalGraphDelta::TransportInsert { id: 7, item: 10 },
                ExternalGraphDelta::TransportInsert { id: 7, item: 10 },
            ])
            .unwrap();
        assert_eq!(
            (report.applied, report.coalesced, report.skipped),
            (3, 0, 0)
        );
        assert_eq!(bridge.sink().machine(7).unwrap().input_items, 2);
        // Identical events in later batches still apply every time.
        let report = bridge
            .ingest(&[ExternalGraphDelta::TransportInsert { id: 7, item: 10 }])
            .unwrap();
        assert_eq!(report.applied, 1);
        assert_eq!(bridge.sink().machine(7).unwrap().input_items, 3);
        // State deltas on the same sink do dedup.
        let report = bridge
            .ingest(&[ExternalGraphDelta::TransportMachine {
                id: 7,
                capacity: 4,
                delay: 1,
            }])
            .unwrap();
        assert_eq!(report.skipped, 1);
        // Re-declaring different machine geometry fails closed.
        let report = bridge
            .ingest(&[ExternalGraphDelta::TransportMachine {
                id: 7,
                capacity: 8,
                delay: 1,
            }])
            .unwrap();
        assert_eq!(report.failed.len(), 1);
    }

    #[test]
    fn fluid_deltas_validate_contents_and_order() {
        let mut bridge = OffloadSyncBridge::new(FluidNetwork::default());
        // FluidTank sorts before FluidContent, so the set succeeds even
        // though it was listed first.
        let report = bridge
            .ingest(&[
                ExternalGraphDelta::FluidSet {
                    id: 1,
                    fluid: Some(7),
                    amount: 5,
                },
                ExternalGraphDelta::FluidTankUpsert {
                    id: 1,
                    capacity: 10,
                    elevation: 0,
                },
            ])
            .unwrap();
        assert_eq!((report.applied, report.failed.len()), (2, 0));
        let tank = bridge.sink().tank(1).unwrap();
        assert_eq!((tank.fluid, tank.amount), (Some(7), 5));

        // Positive amount without a fluid id is malformed input.
        let report = bridge
            .ingest(&[ExternalGraphDelta::FluidSet {
                id: 1,
                fluid: None,
                amount: 3,
            }])
            .unwrap();
        assert_eq!(report.failed.len(), 1);
        assert_eq!(
            report.failed[0].error,
            BridgeError::InvalidDelta("fluid set requires a fluid id")
        );
        // Zero amount drains the tank and needs no fluid id.
        let report = bridge
            .ingest(&[ExternalGraphDelta::FluidSet {
                id: 1,
                fluid: None,
                amount: 0,
            }])
            .unwrap();
        assert_eq!(report.applied, 1);
        let tank = bridge.sink().tank(1).unwrap();
        assert_eq!((tank.fluid, tank.amount), (None, 0));
    }

    #[test]
    fn me_deltas_route_through_me_graph_target() {
        let mut bridge = OffloadSyncBridge::new(MeGraphTarget::default());
        let report = bridge
            .ingest(&[
                ExternalGraphDelta::MeNodeUpsert {
                    id: 1,
                    kind: MeNodeKind::Controller,
                },
                ExternalGraphDelta::MeNodeUpsert {
                    id: 2,
                    kind: MeNodeKind::NormalCable,
                },
                ExternalGraphDelta::MeLink {
                    a: 1,
                    b: 2,
                    linked: true,
                },
                ExternalGraphDelta::MeCellUpsert {
                    cell: StorageCell::new(9, 1, 128, 4, 8),
                },
            ])
            .unwrap();
        assert_eq!((report.applied, report.failed.len()), (4, 0));
        assert_eq!(
            bridge.sink().channels.node_kind(1),
            Some(MeNodeKind::Controller)
        );
        assert!(bridge.sink().storage.cell(9).is_some());

        // A kind change removes and re-adds the node deterministically.
        let report = bridge
            .ingest(&[ExternalGraphDelta::MeNodeUpsert {
                id: 1,
                kind: MeNodeKind::DenseCable,
            }])
            .unwrap();
        assert_eq!(report.applied, 1);
        assert_eq!(
            bridge.sink().channels.node_kind(1),
            Some(MeNodeKind::DenseCable)
        );

        // Re-upserting a cell id replaces its contents.
        let mut cell = StorageCell::new(9, 5, 64, 2, 4);
        cell.contents.insert("megu:ingot".to_string(), 3);
        bridge
            .ingest(&[ExternalGraphDelta::MeCellUpsert { cell }])
            .unwrap();
        assert_eq!(bridge.sink().storage.cell(9).unwrap().priority, 5);

        // Removing a node orphans its pair marks; re-linking then fails
        // closed against the missing node.
        bridge
            .ingest(&[ExternalGraphDelta::MeNodeRemove { id: 2 }])
            .unwrap();
        let report = bridge
            .ingest(&[ExternalGraphDelta::MeLink {
                a: 1,
                b: 2,
                linked: true,
            }])
            .unwrap();
        assert_eq!(report.failed.len(), 1);
    }

    #[test]
    fn region_sink_routes_kernels_and_rejects_me_deltas() {
        let mut bridge = OffloadSyncBridge::new(Region::new(RegionKey::for_block(0, 0, 0)));
        let report = bridge
            .ingest(&[
                ExternalGraphDelta::KineticUpsert {
                    id: 3,
                    node: drive_node(8.0, 2.0),
                },
                ExternalGraphDelta::FluidTankUpsert {
                    id: 4,
                    capacity: 20,
                    elevation: 1,
                },
                ExternalGraphDelta::MeCellUpsert {
                    cell: StorageCell::new(1, 0, 8, 1, 1),
                },
            ])
            .unwrap();
        assert_eq!((report.applied, report.failed.len()), (2, 1));
        assert_eq!(report.failed[0].error, BridgeError::UnsupportedDelta);
        assert!(bridge.sink().kinetic.solution(3).is_some());
        assert!(bridge.sink().fluid().tank(4).is_some());
    }
}
