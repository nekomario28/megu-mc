// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later

//! Generic cubic-Bezier spline rail network for Create-style trains and any
//! track-gauge addon (narrow, wide, monorail, or a fully custom gauge id —
//! zero hardcoded addon names).
//!
//! - [`TrackGraph`]: dimension-scoped nodes ([`DimId`]), directional
//!   [`TrackEdge`] splines built from [`CubicBezierSegment`]s with a
//!   deterministic 32-sample arc-length LUT for O(1)
//!   `sample_at_distance` queries (smooth 3D slopes and curves), plus
//!   junction/switch selection. An edge whose endpoints live in different
//!   dimensions is automatically an inter-dimensional portal track link.
//! - Block & chain signals: [`SignalBlock`]s partition edges into signal
//!   sections tracking occupancy and reservations per [`TrainId`]. A
//!   `Block` signal is green iff its section is unoccupied and unreserved;
//!   a `Chain` signal is green only when its section *and* the exit
//!   section beyond the junction are both clear, holding trains in front
//!   of the junction so they can never deadlock it.
//! - [`ArticulatedTrain`]: multi-carriage trains with per-carriage
//!   leading/trailing bogey offsets, smooth acceleration/braking, station
//!   stopping via [`TrainSchedule`], and exact bogey world poses along
//!   multi-edge paths.
//! - [`tick_rail_network`]: advances every scheduled train one tick across
//!   the graph — including through unloaded chunks — updating virtual
//!   `(cx, cz)` chunk coordinates and signal occupancy without loading a
//!   single chunk, and reports [`TrainRenderMode::Virtual`] /
//!   [`TrainRenderMode::Loaded`] transitions as heads cross the loaded set.
//! - `RAIL` magic + schema v1 + IEEE CRC32 binary snapshot for crash-safe
//!   persistence; decode fails closed on bad magic/version, truncation,
//!   CRC mismatch, or oversized counts.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt;

use mc_spine_core::perf_kit::{
    BoundedScratchVec, DirtyFingerprintGate, FingerprintBuilder, FingerprintStats,
};
use mod_adapter_core::neoforge_wire::{
    read_mc_string, read_varint, write_mc_string, write_varint,
};

/// Train-id scratch capacity retained between ticks; a burst past this
/// shrinks back via [`BoundedScratchVec::clear_bounded`].
const MAX_TRAIN_SCRATCH_RETAIN: usize = 4096;

/// Deterministic binary snapshot magic (`"RAIL"`).
pub const RAIL_MAGIC: [u8; 4] = *b"RAIL";
/// `RAIL` snapshot schema version.
pub const RAIL_VERSION: u8 = 1;
/// Arc-length LUT sample count per [`CubicBezierSegment`] (fixed, so lookups
/// are O(1) regardless of curve shape).
pub const ARC_LUT_SAMPLES: usize = 32;
/// Maximum string length accepted by the snapshot codec (MC cap).
const MAX_NAME_LEN: usize = 32_767;
/// Blocks per chunk axis; chunk `(cx, cz)` = `floor(pos / CHUNK_SIZE)`.
const CHUNK_SIZE: f64 = 16.0;
/// Sentinel `u64` marking "no train" in the snapshot occupancy fields.
const NO_TRAIN: u64 = u64::MAX;

// ---------------------------------------------------------------------------
// Id newtypes
// ---------------------------------------------------------------------------

macro_rules! id_newtype {
    ($name:ident) => {
        /// Deterministic 64-bit identifier.
        #[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(pub u64);
        impl $name {
            /// Raw numeric value.
            pub fn get(self) -> u64 {
                self.0
            }
        }
        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}", self.0)
            }
        }
    };
}

id_newtype!(NodeId);
id_newtype!(EdgeId);
id_newtype!(BlockId);
id_newtype!(TrainId);

/// String dimension key (`"minecraft:overworld"`, a modded dimension id, or
/// any adapter-scoped name). Nodes are dimension-scoped; an edge whose
/// endpoints live in different `DimId`s is a portal track link.
#[derive(Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DimId(pub String);

impl DimId {
    pub fn new(name: impl Into<String>) -> Self {
        Self(name.into())
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for DimId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Loaded-set membership key: one chunk column inside one dimension.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ChunkKey {
    pub dim: DimId,
    pub cx: i32,
    pub cz: i32,
}

impl ChunkKey {
    pub fn new(dim: DimId, cx: i32, cz: i32) -> Self {
        Self { dim, cx, cz }
    }
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// All rail-network failures. Snapshot corruption is reported through
/// [`RailError::CorruptSnapshot`] and always fails closed.
#[derive(Clone, Debug, PartialEq)]
pub enum RailError {
    NodeNotFound(NodeId),
    EdgeNotFound(EdgeId),
    StationNotFound(String),
    /// A bogey/train of one gauge attempted to enter an edge of an
    /// incompatible gauge (e.g. wide-gauge stock on a narrow-gauge edge).
    IncompatibleGauge {
        train: TrackGauge,
        edge: TrackGauge,
    },
    /// An edge spline had zero (or non-finite) arc length.
    DegenerateEdge,
    /// A station anchor or spawn point outside `[0, edge_length]`.
    InvalidAnchor(String),
    /// A switch selection that does not reference an outgoing edge of the
    /// junction node.
    InvalidSwitch { node: NodeId, edge: EdgeId },
    /// Carriages of one articulated train disagree on gauge.
    MixedCarriageGauge,
    /// Bogey offsets outside `[0, carriage_length]`.
    InvalidCarriageSpec(String),
    CorruptSnapshot(String),
}

impl fmt::Display for RailError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NodeNotFound(n) => write!(f, "node {n} not found"),
            Self::EdgeNotFound(e) => write!(f, "edge {e} not found"),
            Self::StationNotFound(s) => write!(f, "station {s:?} not found"),
            Self::IncompatibleGauge { train, edge } => {
                write!(f, "gauge {train:?} cannot enter {edge:?} edge")
            }
            Self::DegenerateEdge => f.write_str("degenerate (zero-length) edge"),
            Self::InvalidAnchor(what) => write!(f, "invalid anchor: {what}"),
            Self::InvalidSwitch { node, edge } => {
                write!(f, "edge {edge} is not an outgoing edge of junction {node}")
            }
            Self::MixedCarriageGauge => f.write_str("mixed-gauge carriages"),
            Self::InvalidCarriageSpec(what) => write!(f, "invalid carriage spec: {what}"),
            Self::CorruptSnapshot(what) => write!(f, "corrupt RAIL snapshot: {what}"),
        }
    }
}

impl std::error::Error for RailError {}

fn corrupt(what: &str) -> RailError {
    RailError::CorruptSnapshot(what.to_string())
}

// ---------------------------------------------------------------------------
// Track gauge
// ---------------------------------------------------------------------------

/// Track gauge of an edge or a carriage bogey. `Custom` covers any addon
/// gauge id (e.g. a 600 mm narrow gauge or a monorail beam) without the
/// adapter hardcoding addon names.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum TrackGauge {
    /// Standard-gauge Create track.
    Standard,
    /// Narrow-gauge track.
    Narrow,
    /// Wide-gauge track.
    Wide,
    /// Monorail beam track.
    Monorail,
    /// Addon-defined gauge identified by a generic string key.
    Custom(String),
}

impl TrackGauge {
    /// Whether a bogey of gauge `self` may roll on an edge of gauge `other`.
    /// Compatible (equal) gauges share junctions; incompatible gauges never
    /// do, so a wide bogey physically cannot enter a narrow edge.
    pub fn is_compatible_with(&self, other: &TrackGauge) -> bool {
        self == other
    }
}

// ---------------------------------------------------------------------------
// Cubic Bezier segment with deterministic arc-length LUT
// ---------------------------------------------------------------------------

/// One cubic Bezier spline (`P0..P3`, `[f64; 3]` world coordinates) with a
/// precomputed 32-sample arc-length LUT. [`Self::sample_at_distance`] is O(1):
/// the LUT has a fixed 32 entries, so the interval scan is bounded.
#[derive(Clone, Debug, PartialEq)]
pub struct CubicBezierSegment {
    /// Control points `P0, P1, P2, P3` in world coordinates.
    pub control: [[f64; 3]; 4],
    lut_pts: [[f64; 3]; ARC_LUT_SAMPLES],
    lut_cum: [f64; ARC_LUT_SAMPLES],
    length: f64,
}

impl CubicBezierSegment {
    /// Build a segment and its arc-length LUT. Fails on non-finite control
    /// points or a degenerate (zero-length) curve.
    pub fn new(control: [[f64; 3]; 4]) -> Result<Self, RailError> {
        if !control.iter().flatten().all(|v| v.is_finite()) {
            return Err(RailError::DegenerateEdge);
        }
        let mut lut_pts = [[0.0; 3]; ARC_LUT_SAMPLES];
        let mut lut_cum = [0.0; ARC_LUT_SAMPLES];
        for (i, p) in lut_pts.iter_mut().enumerate() {
            let t = i as f64 / (ARC_LUT_SAMPLES - 1) as f64;
            *p = bezier_eval(&control, t);
        }
        let mut length = 0.0;
        for i in 1..ARC_LUT_SAMPLES {
            length += dist3(lut_pts[i - 1], lut_pts[i]);
            lut_cum[i] = length;
        }
        if !(length.is_finite() && length > 0.0) {
            return Err(RailError::DegenerateEdge);
        }
        Ok(Self {
            control,
            lut_pts,
            lut_cum,
            length,
        })
    }

    /// Total chord-approximated arc length (32-sample LUT; exact for
    /// straight segments).
    pub fn length(&self) -> f64 {
        self.length
    }

    /// O(1) arc-length lookup: world position and unit tangent at arc
    /// distance `s` (clamped to `[0, length]`). The tangent falls back to
    /// the local LUT chord on cusps where the analytic derivative vanishes.
    pub fn sample_at_distance(&self, s: f64) -> ([f64; 3], [f64; 3]) {
        let s = s.clamp(0.0, self.length);
        // Largest LUT index i with lut_cum[i] <= s, bounded to 0..N-2.
        let mut i = self.lut_cum.partition_point(|&l| l <= s).saturating_sub(1);
        if i > ARC_LUT_SAMPLES - 2 {
            i = ARC_LUT_SAMPLES - 2;
        }
        let span = self.lut_cum[i + 1] - self.lut_cum[i];
        let frac = if span > 0.0 {
            ((s - self.lut_cum[i]) / span).clamp(0.0, 1.0)
        } else {
            0.0
        };
        let t = (i as f64 + frac) / (ARC_LUT_SAMPLES - 1) as f64;
        let pos = bezier_eval(&self.control, t);
        let mut tangent = bezier_deriv(&self.control, t);
        if norm3(tangent) < 1e-9 {
            tangent = sub3(self.lut_pts[i + 1], self.lut_pts[i]);
        }
        (pos, normalize3(tangent))
    }
}

// ---------------------------------------------------------------------------
// Track graph: nodes, edges, switches
// ---------------------------------------------------------------------------

/// One directional spline edge between two nodes (possibly in different
/// dimensions — a portal link). Composed of one or more
/// [`CubicBezierSegment`]s sampled as a single arc-length domain.
#[derive(Clone, Debug, PartialEq)]
pub struct TrackEdge {
    pub id: EdgeId,
    pub from: NodeId,
    pub to: NodeId,
    pub gauge: TrackGauge,
    pub segments: Vec<CubicBezierSegment>,
    seg_starts: Vec<f64>,
    length: f64,
}

impl TrackEdge {
    fn new(
        id: EdgeId,
        from: NodeId,
        to: NodeId,
        gauge: TrackGauge,
        segments: Vec<CubicBezierSegment>,
    ) -> Result<Self, RailError> {
        if segments.is_empty() {
            return Err(RailError::DegenerateEdge);
        }
        let mut seg_starts = Vec::with_capacity(segments.len() + 1);
        let mut length = 0.0;
        for seg in &segments {
            seg_starts.push(length);
            length += seg.length();
        }
        if !(length.is_finite() && length > 0.0) {
            return Err(RailError::DegenerateEdge);
        }
        seg_starts.push(length);
        Ok(Self {
            id,
            from,
            to,
            gauge,
            segments,
            seg_starts,
            length,
        })
    }

    /// Total arc length of the spline chain.
    pub fn length(&self) -> f64 {
        self.length
    }

    /// O(1) world position + unit tangent at arc distance `s` along the
    /// whole edge (spans segments transparently).
    pub fn sample_at_distance(&self, s: f64) -> ([f64; 3], [f64; 3]) {
        let s = s.clamp(0.0, self.length);
        let i = self
            .seg_starts
            .partition_point(|&start| start <= s)
            .saturating_sub(1)
            .min(self.segments.len() - 1);
        self.segments[i].sample_at_distance(s - self.seg_starts[i])
    }

    /// Whether this edge links two dimensions (portal track).
    pub fn is_portal(&self, graph: &TrackGraph) -> bool {
        match (graph.nodes.get(&self.from), graph.nodes.get(&self.to)) {
            (Some(a), Some(b)) => a.dim != b.dim,
            _ => false,
        }
    }
}

/// A dimension-scoped track node. Nodes with more than two incident edges
/// act as junctions; `TrackGraph::switches` selects the chosen exit.
#[derive(Clone, Debug, PartialEq)]
pub struct TrackNode {
    pub id: NodeId,
    pub dim: DimId,
    /// Reference world position (junction/station marker).
    pub pos: [f64; 3],
    pub out_edges: BTreeSet<EdgeId>,
    pub in_edges: BTreeSet<EdgeId>,
}

/// Spline track graph: dimension-scoped nodes + directional Bezier edges +
/// junction switch selections.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TrackGraph {
    pub nodes: BTreeMap<NodeId, TrackNode>,
    pub edges: BTreeMap<EdgeId, TrackEdge>,
    /// Junction selection: node -> chosen outgoing edge (set by a switch).
    pub switches: BTreeMap<NodeId, EdgeId>,
    next_node: u64,
    next_edge: u64,
}

impl TrackGraph {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a dimension-scoped node at a reference world position.
    pub fn add_node(&mut self, dim: DimId, pos: [f64; 3]) -> NodeId {
        let id = NodeId(self.next_node);
        self.next_node += 1;
        self.nodes.insert(
            id,
            TrackNode {
                id,
                dim,
                pos,
                out_edges: BTreeSet::new(),
                in_edges: BTreeSet::new(),
            },
        );
        id
    }

    /// Add a directional edge as a spline chain of cubic Bezier control
    /// quads. `from`/`to` may live in different dimensions (portal link).
    pub fn add_segmented_edge(
        &mut self,
        from: NodeId,
        to: NodeId,
        gauge: TrackGauge,
        segments: Vec<[[f64; 3]; 4]>,
    ) -> Result<EdgeId, RailError> {
        if !self.nodes.contains_key(&from) {
            return Err(RailError::NodeNotFound(from));
        }
        if !self.nodes.contains_key(&to) {
            return Err(RailError::NodeNotFound(to));
        }
        let segs = segments
            .into_iter()
            .map(CubicBezierSegment::new)
            .collect::<Result<Vec<_>, _>>()?;
        let id = EdgeId(self.next_edge);
        self.next_edge += 1;
        let edge = TrackEdge::new(id, from, to, gauge, segs)?;
        self.nodes.get_mut(&from).map(|n| n.out_edges.insert(id));
        self.nodes.get_mut(&to).map(|n| n.in_edges.insert(id));
        self.edges.insert(id, edge);
        Ok(id)
    }

    /// Convenience: single-segment Bezier edge.
    pub fn add_edge(
        &mut self,
        from: NodeId,
        to: NodeId,
        gauge: TrackGauge,
        control: [[f64; 3]; 4],
    ) -> Result<EdgeId, RailError> {
        self.add_segmented_edge(from, to, gauge, vec![control])
    }

    /// Point junction `node`'s switch at outgoing `edge`.
    pub fn set_switch(&mut self, node: NodeId, edge: EdgeId) -> Result<(), RailError> {
        let n = self.nodes.get(&node).ok_or(RailError::NodeNotFound(node))?;
        if !n.out_edges.contains(&edge) {
            return Err(RailError::InvalidSwitch { node, edge });
        }
        self.switches.insert(node, edge);
        Ok(())
    }

    /// Clear a junction switch selection (free routing resumes).
    pub fn clear_switch(&mut self, node: NodeId) {
        self.switches.remove(&node);
    }

    /// Edge a train of `gauge` would take leaving `node`: the selected
    /// switch edge when one is set and gauge-compatible, otherwise the
    /// lowest-id compatible outgoing edge. `None` when no compatible exit
    /// exists (e.g. only narrow-gauge exits for a wide-gauge bogey) — a
    /// switch locked onto an incompatible edge blocks the train entirely.
    pub fn next_edge_for(&self, node: NodeId, gauge: &TrackGauge) -> Option<EdgeId> {
        let n = self.nodes.get(&node)?;
        let compatible = |e: &EdgeId| {
            self.edges
                .get(e)
                .is_some_and(|edge| edge.gauge.is_compatible_with(gauge))
        };
        if let Some(sw) = self.switches.get(&node) {
            return compatible(sw).then_some(*sw);
        }
        n.out_edges.iter().copied().find(compatible)
    }

    /// Outgoing edges a train of `gauge` may traverse from `node`. A set
    /// switch restricts traversal to the selected edge when compatible.
    pub fn traversable_from(&self, node: NodeId, gauge: &TrackGauge) -> Vec<EdgeId> {
        let Some(n) = self.nodes.get(&node) else {
            return Vec::new();
        };
        let compatible = |e: &EdgeId| {
            self.edges
                .get(e)
                .is_some_and(|edge| edge.gauge.is_compatible_with(gauge))
        };
        if let Some(sw) = self.switches.get(&node) {
            return if compatible(sw) { vec![*sw] } else { Vec::new() };
        }
        n.out_edges.iter().copied().filter(compatible).collect()
    }
}

// ---------------------------------------------------------------------------
// Signals & blocks
// ---------------------------------------------------------------------------

/// Signal type guarding entry into an edge's block section.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SignalKind {
    /// Green iff the guarded section is unoccupied and unreserved.
    Block,
    /// Green only when the guarded section *and* the exit section beyond
    /// the junction are both clear — holds a train in front of the
    /// junction so it can never enter and deadlock it.
    Chain,
}

/// Signal aspect. Ordered `Green < Yellow < Red` so `max` combines the
/// guarded and exit sections of a chain signal pessimistically.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum SignalAspect {
    Green,
    /// Section reserved (but not yet occupied) by another train.
    Yellow,
    /// Section occupied by another train.
    Red,
}

/// A signal mounted at the entry of an edge.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Signal {
    pub kind: SignalKind,
}

/// A signal section: a partition of edges sharing one occupancy/reservation
/// state, tracked per [`TrainId`].
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SignalBlock {
    pub id: BlockId,
    pub edges: BTreeSet<EdgeId>,
    pub occupied_by: Option<TrainId>,
    pub reserved_by: Option<TrainId>,
}

impl SignalBlock {
    /// Aspect contribution of this block for `train`: `Red` when occupied
    /// by another train, `Yellow` when only reserved by another, `Green`
    /// when free for `train` (empty, or already occupied/reserved *by*
    /// `train`).
    fn aspect_for(&self, train: TrainId) -> SignalAspect {
        if self.occupied_by.is_some_and(|t| t != train) {
            SignalAspect::Red
        } else if self.reserved_by.is_some_and(|t| t != train) {
            SignalAspect::Yellow
        } else {
            SignalAspect::Green
        }
    }
}

/// A station stop point at arc distance `s` along `edge`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct StationAnchor {
    pub edge: EdgeId,
    pub s: f64,
}

// ---------------------------------------------------------------------------
// Trains
// ---------------------------------------------------------------------------

/// Which end of a carriage a bogey sits under.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BogeyEnd {
    /// Bogey near the carriage's front (direction of travel).
    Leading,
    /// Bogey near the carriage's rear.
    Trailing,
}

/// Static geometry of one carriage: body `length` (blocks) and bogey
/// offsets measured inward from each end — `leading_bogey_offset` behind
/// the front end, `trailing_bogey_offset` ahead of the rear end (so the
/// trailing bogey sits `length - trailing_bogey_offset` behind the front).
#[derive(Clone, Debug, PartialEq)]
pub struct CarriageSpec {
    pub length: f64,
    pub leading_bogey_offset: f64,
    pub trailing_bogey_offset: f64,
    pub gauge: TrackGauge,
}

impl CarriageSpec {
    fn validate(&self) -> Result<(), RailError> {
        let bad = |what: &str| RailError::InvalidCarriageSpec(what.to_string());
        if !(self.length.is_finite() && self.length > 0.0) {
            return Err(bad("length"));
        }
        for (name, v) in [
            ("leading_bogey_offset", self.leading_bogey_offset),
            ("trailing_bogey_offset", self.trailing_bogey_offset),
        ] {
            if !(v.is_finite() && v >= 0.0 && v <= self.length) {
                return Err(bad(name));
            }
        }
        Ok(())
    }
}

/// One schedule entry: stop at `station_name` for `wait_ticks` ticks.
#[derive(Clone, Debug, PartialEq)]
pub struct StationInstruction {
    pub station_name: String,
    pub wait_ticks: u64,
}

/// Ordered station schedule. With `looping`, the train wraps back to the
/// first instruction after the last departure; otherwise it terminates.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TrainSchedule {
    pub instructions: Vec<StationInstruction>,
    pub looping: bool,
}

impl TrainSchedule {
    /// Non-looping point-to-point schedule.
    pub fn point_to_point(instructions: Vec<StationInstruction>) -> Self {
        Self {
            instructions,
            looping: false,
        }
    }
    /// Looping schedule (wraps after the last instruction).
    pub fn looping(instructions: Vec<StationInstruction>) -> Self {
        Self {
            instructions,
            looping: true,
        }
    }
}

/// Whether a train's head currently sits inside a loaded chunk column.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrainRenderMode {
    /// Head chunk is unloaded — train advances virtually in O(1).
    Virtual,
    /// Head chunk is loaded — full entity rendering applies.
    Loaded,
}

/// Runtime train state.
#[derive(Clone, Debug, PartialEq)]
pub enum TrainRunState {
    /// Moving (or signal-held) along the graph.
    Running,
    /// Dwelling at a station for `ticks_left` more ticks.
    WaitingAtStation { station: String, ticks_left: u64 },
    /// Schedule exhausted / line ended — stays put (still occupies track).
    Terminus,
}

/// The station a running train is currently driving toward.
#[derive(Clone, Debug, PartialEq)]
pub struct StationTarget {
    pub station_name: String,
    pub edge: EdgeId,
    pub s: f64,
    pub wait_ticks: u64,
}

/// A multi-carriage articulated train. `pos_edge`/`pos_s` locate the train
/// head (front of the leading carriage) along the graph; `history` retains
/// just enough traversed edges to pose the rearmost bogey.
#[derive(Clone, Debug, PartialEq)]
pub struct ArticulatedTrain {
    pub id: TrainId,
    pub carriages: Vec<CarriageSpec>,
    /// Top speed in blocks/tick.
    pub max_speed: f64,
    /// Acceleration in blocks/tick².
    pub accel: f64,
    /// Braking deceleration in blocks/tick² (must be > 0).
    pub decel: f64,
    pub schedule: TrainSchedule,
    /// Current speed in blocks/tick.
    pub speed: f64,
    /// Edge the head currently occupies and arc distance along it.
    pub pos_edge: EdgeId,
    pub pos_s: f64,
    /// Dimension the head currently reports (the `from` node of `pos_edge`;
    /// while crossing a portal edge the train is "in" the entry dimension
    /// until it arrives at the far node).
    pub current_dim: DimId,
    pub render_mode: TrainRenderMode,
    pub state: TrainRunState,
    /// Index of the next [`TrainSchedule`] instruction to pursue.
    pub schedule_idx: usize,
    /// Upcoming edges toward `target` (front element is next to enter).
    pub route: VecDeque<EdgeId>,
    /// Traversed edges (oldest first) covering at least `total_length`.
    pub history: VecDeque<EdgeId>,
    /// Active driving target, if any.
    pub target: Option<StationTarget>,
}

impl ArticulatedTrain {
    /// Build a train (not yet placed — see [`RailNetwork::add_train`]).
    /// All carriages must share one gauge; `max_speed`, `accel`, `decel`
    /// must be finite with `decel > 0`.
    pub fn new(
        id: TrainId,
        carriages: Vec<CarriageSpec>,
        max_speed: f64,
        accel: f64,
        decel: f64,
        schedule: TrainSchedule,
    ) -> Result<Self, RailError> {
        let bad = |what: &str| RailError::InvalidCarriageSpec(what.to_string());
        if carriages.is_empty() {
            return Err(bad("no carriages"));
        }
        for c in &carriages {
            c.validate()?;
        }
        let gauge = &carriages[0].gauge;
        if !carriages.iter().all(|c| &c.gauge == gauge) {
            return Err(RailError::MixedCarriageGauge);
        }
        if !(max_speed.is_finite() && max_speed > 0.0) {
            return Err(bad("max_speed"));
        }
        if !(accel.is_finite() && accel > 0.0) {
            return Err(bad("accel"));
        }
        if !(decel.is_finite() && decel > 0.0) {
            return Err(bad("decel"));
        }
        Ok(Self {
            id,
            carriages,
            max_speed,
            accel,
            decel,
            schedule,
            speed: 0.0,
            pos_edge: EdgeId(0),
            pos_s: 0.0,
            current_dim: DimId::default(),
            render_mode: TrainRenderMode::Virtual,
            state: TrainRunState::Running,
            schedule_idx: 0,
            route: VecDeque::new(),
            history: VecDeque::new(),
            target: None,
        })
    }

    /// The train's uniform gauge (first carriage).
    pub fn gauge(&self) -> &TrackGauge {
        &self.carriages[0].gauge
    }

    /// Total body length of all carriages (coupler gaps excluded).
    pub fn total_length(&self) -> f64 {
        self.carriages.iter().map(|c| c.length).sum()
    }

    /// World position + unit tangent of the train head.
    pub fn head_world_pose(&self, net: &RailNetwork) -> Option<([f64; 3], [f64; 3])> {
        net.graph
            .edges
            .get(&self.pos_edge)
            .map(|e| e.sample_at_distance(self.pos_s))
    }

    /// Exact world pose of one bogey along the multi-edge path: `Leading`
    /// sits `leading_bogey_offset` behind that carriage's front; `Trailing`
    /// sits `length - trailing_bogey_offset` behind it. `None` when the
    /// bogey would hang off the retained path history.
    pub fn bogey_world_pose(
        &self,
        net: &RailNetwork,
        carriage: usize,
        end: BogeyEnd,
    ) -> Option<([f64; 3], [f64; 3])> {
        let c = self.carriages.get(carriage)?;
        let front: f64 = self.carriages[..carriage].iter().map(|c| c.length).sum();
        let depth = match end {
            BogeyEnd::Leading => front + c.leading_bogey_offset,
            BogeyEnd::Trailing => front + (c.length - c.trailing_bogey_offset),
        };
        self.sample_behind_head(net, depth)
    }

    /// Pose `depth` blocks behind the head, walking back across `history`.
    fn sample_behind_head(&self, net: &RailNetwork, depth: f64) -> Option<([f64; 3], [f64; 3])> {
        let mut edge = net.graph.edges.get(&self.pos_edge)?;
        let mut s = self.pos_s - depth;
        let mut back = self.history.len();
        while s < 0.0 {
            if back == 0 {
                return None;
            }
            back -= 1;
            edge = net.graph.edges.get(&self.history[back])?;
            s += edge.length();
        }
        Some(edge.sample_at_distance(s))
    }
}

// ---------------------------------------------------------------------------
// Events
// ---------------------------------------------------------------------------

/// Tick-level report from [`tick_rail_network`].
#[derive(Clone, Debug, PartialEq)]
pub enum TransitEvent {
    ArrivedAtStation { train: TrainId, station: String },
    DepartedStation { train: TrainId, station: String },
    RenderModeChanged {
        train: TrainId,
        mode: TrainRenderMode,
    },
    /// A running train could not route to its next station.
    NoRoute { train: TrainId, station: String },
    /// End of schedule or end of line reached; the train stays put.
    ReachedTerminus { train: TrainId },
}

// ---------------------------------------------------------------------------
// RailNetwork
// ---------------------------------------------------------------------------

/// The whole live rail network: spline graph, signal blocks and signals,
/// stations, and articulated trains.
#[derive(Clone, Debug, Default)]
pub struct RailNetwork {
    pub graph: TrackGraph,
    pub blocks: BTreeMap<BlockId, SignalBlock>,
    /// Edge -> containing signal section.
    pub edge_block: BTreeMap<EdgeId, BlockId>,
    /// Entry signal per edge.
    pub signals: BTreeMap<EdgeId, Signal>,
    pub stations: BTreeMap<String, StationAnchor>,
    pub trains: BTreeMap<TrainId, ArticulatedTrain>,
    pub tick: u64,
    next_block: u64,
    /// Per-train occupancy fingerprints: a train whose covered-block set
    /// is unchanged since the last epoch-skippable commit skips the
    /// `O(blocks)` refresh sweep. Cache only — never persisted or compared.
    occupancy_gate: DirtyFingerprintGate<TrainId>,
    /// Epoch bumped on every write to block occupancy/reservation fields
    /// or to the graph topology coverage derives from; committed into each
    /// train's fingerprint so a skip is only allowed while the write set
    /// the refresh would produce is provably already in place.
    occupancy_epoch: u64,
    /// Reusable train-id snapshot for the tick loop — one allocation
    /// serves every tick instead of a fresh `Vec` each call.
    id_scratch: BoundedScratchVec<TrainId>,
}

/// Occupancy fingerprints, epoch, and scratch are tick-loop caches: they
/// accelerate refresh decisions but hold no semantic state, so equality —
/// including snapshot round-trips — compares the network itself only.
impl PartialEq for RailNetwork {
    fn eq(&self, other: &Self) -> bool {
        self.graph == other.graph
            && self.blocks == other.blocks
            && self.edge_block == other.edge_block
            && self.signals == other.signals
            && self.stations == other.stations
            && self.trains == other.trains
            && self.tick == other.tick
            && self.next_block == other.next_block
    }
}

impl RailNetwork {
    pub fn new() -> Self {
        Self::default()
    }

    /// Bump the occupancy epoch: every committed refresh fingerprint is
    /// invalidated, so the next refresh decision re-runs the sweep instead
    /// of skipping on stale coverage.
    fn bump_occupancy_epoch(&mut self) {
        self.occupancy_epoch += 1;
    }

    /// Invalidate every cached occupancy fingerprint. Call after mutating
    /// `self.graph` directly (edge/block topology outside this API) — the
    /// covered-block derivation may have shifted under committed states.
    pub fn invalidate_occupancy_cache(&mut self) {
        self.bump_occupancy_epoch();
    }

    /// Skip/update counters for the occupancy refresh gate: `skips` is the
    /// measurable proof parked trains re-evaluate in O(1).
    pub fn occupancy_gate_stats(&self) -> FingerprintStats {
        self.occupancy_gate.stats()
    }

    /// See [`TrackGraph::add_node`].
    pub fn add_node(&mut self, dim: DimId, pos: [f64; 3]) -> NodeId {
        self.bump_occupancy_epoch();
        self.graph.add_node(dim, pos)
    }

    /// See [`TrackGraph::add_edge`].
    pub fn add_edge(
        &mut self,
        from: NodeId,
        to: NodeId,
        gauge: TrackGauge,
        control: [[f64; 3]; 4],
    ) -> Result<EdgeId, RailError> {
        self.bump_occupancy_epoch();
        self.graph.add_edge(from, to, gauge, control)
    }

    /// See [`TrackGraph::add_segmented_edge`].
    pub fn add_segmented_edge(
        &mut self,
        from: NodeId,
        to: NodeId,
        gauge: TrackGauge,
        segments: Vec<[[f64; 3]; 4]>,
    ) -> Result<EdgeId, RailError> {
        self.bump_occupancy_epoch();
        self.graph.add_segmented_edge(from, to, gauge, segments)
    }

    /// Partition `edges` into a new signal section. An edge already in
    /// another block is moved into the new one.
    pub fn add_block(&mut self, edges: &[EdgeId]) -> Result<BlockId, RailError> {
        self.bump_occupancy_epoch();
        for e in edges {
            if !self.graph.edges.contains_key(e) {
                return Err(RailError::EdgeNotFound(*e));
            }
        }
        let id = BlockId(self.next_block);
        self.next_block += 1;
        let mut set = BTreeSet::new();
        for e in edges {
            if let Some(old) = self.edge_block.insert(*e, id) {
                if let Some(b) = self.blocks.get_mut(&old) {
                    b.edges.remove(e);
                }
            }
            set.insert(*e);
        }
        self.blocks.insert(
            id,
            SignalBlock {
                id,
                edges: set,
                occupied_by: None,
                reserved_by: None,
            },
        );
        Ok(id)
    }

    /// Mount a `kind` signal guarding entry into `edge`'s block.
    pub fn add_signal(&mut self, edge: EdgeId, kind: SignalKind) -> Result<(), RailError> {
        self.bump_occupancy_epoch();
        if !self.graph.edges.contains_key(&edge) {
            return Err(RailError::EdgeNotFound(edge));
        }
        self.signals.insert(edge, Signal { kind });
        Ok(())
    }

    /// Register a named station stop on `edge` at arc distance `s`.
    pub fn add_station(&mut self, name: &str, edge: EdgeId, s: f64) -> Result<(), RailError> {
        self.bump_occupancy_epoch();
        let len = self
            .graph
            .edges
            .get(&edge)
            .ok_or(RailError::EdgeNotFound(edge))?
            .length();
        if !(s.is_finite() && s >= 0.0 && s <= len) {
            return Err(RailError::InvalidAnchor(format!(
                "station {name:?} s={s} outside edge {edge} length {len}"
            )));
        }
        self.stations
            .insert(name.to_string(), StationAnchor { edge, s });
        Ok(())
    }

    /// Place `train` with its head on `edge` at arc distance `s`. The
    /// train's gauge must be compatible with the entry edge.
    pub fn add_train(
        &mut self,
        mut train: ArticulatedTrain,
        edge: EdgeId,
        s: f64,
    ) -> Result<(), RailError> {
        let e = self.graph.edges.get(&edge).ok_or(RailError::EdgeNotFound(edge))?;
        if !train.gauge().is_compatible_with(&e.gauge) {
            return Err(RailError::IncompatibleGauge {
                train: train.gauge().clone(),
                edge: e.gauge.clone(),
            });
        }
        if !(s.is_finite() && s >= 0.0 && s <= e.length()) {
            return Err(RailError::InvalidAnchor(format!(
                "spawn s={s} outside edge {edge} length {}",
                e.length()
            )));
        }
        train.pos_edge = edge;
        train.pos_s = s;
        train.current_dim = self
            .graph
            .nodes
            .get(&e.from)
            .map(|n| n.dim.clone())
            .unwrap_or_default();
        let id = train.id;
        self.trains.insert(id, train);
        refresh_occupancy_gated(self, id);
        Ok(())
    }

    /// Remove a train and release every block it occupied or reserved.
    pub fn remove_train(&mut self, id: TrainId) {
        self.bump_occupancy_epoch();
        self.trains.remove(&id);
        self.occupancy_gate.remove(&id);
        for b in self.blocks.values_mut() {
            if b.occupied_by == Some(id) {
                b.occupied_by = None;
            }
            if b.reserved_by == Some(id) {
                b.reserved_by = None;
            }
        }
    }

    /// Section containing `edge`, if partitioned.
    pub fn block_of(&self, edge: EdgeId) -> Option<&SignalBlock> {
        self.edge_block.get(&edge).and_then(|b| self.blocks.get(b))
    }

    /// Aspect the entry signal on `edge` shows `train` right now. Unguarded
    /// or unpartitioned edges read `Green`. Chain signals look through the
    /// junction to the exit block the train would take next.
    pub fn signal_aspect_for(&self, edge: EdgeId, train: TrainId) -> SignalAspect {
        let Some(&bid) = self.edge_block.get(&edge) else {
            return SignalAspect::Green;
        };
        let Some(block) = self.blocks.get(&bid) else {
            return SignalAspect::Green;
        };
        let kind = self.signals.get(&edge).map(|s| s.kind).unwrap_or(SignalKind::Block);
        let mine = block.aspect_for(train);
        match kind {
            SignalKind::Block => mine,
            SignalKind::Chain => {
                let exit_aspect = self
                    .chain_exit_block(edge, train)
                    .map(|b| b.aspect_for(train))
                    .unwrap_or(SignalAspect::Green);
                mine.max(exit_aspect)
            }
        }
    }

    /// Block of the edge a train would enter *after* `edge` (through the
    /// junction at `edge.to`), following the train's route, switch, or the
    /// lowest-id compatible exit.
    fn chain_exit_block(&self, edge: EdgeId, train: TrainId) -> Option<&SignalBlock> {
        let e = self.graph.edges.get(&edge)?;
        let gauge = self
            .trains
            .get(&train)
            .map(|t| t.gauge())
            .unwrap_or(&e.gauge);
        let next = self
            .trains
            .get(&train)
            .and_then(|t| {
                if t.pos_edge == edge {
                    t.route.front().copied()
                } else if t.route.front() == Some(&edge) {
                    t.route.get(1).copied()
                } else {
                    None
                }
            })
            .or_else(|| self.graph.next_edge_for(e.to, gauge))?;
        self.block_of(next)
    }

    /// Reserve the block containing `edge` for `train` when it is free for
    /// that train (no-op otherwise — a reservation by another train is
    /// never stolen).
    pub fn reserve_block_for(&mut self, edge: EdgeId, train: TrainId) {
        self.bump_occupancy_epoch();
        let Some(&bid) = self.edge_block.get(&edge) else {
            return;
        };
        if let Some(b) = self.blocks.get_mut(&bid) {
            if b.occupied_by.is_none() && b.reserved_by.is_none() {
                b.reserved_by = Some(train);
            }
        }
    }

    /// BFS route (fewest edges, gauge-filtered, switch-respecting) from
    /// `start` node to the entry of `target_edge`. Returns the edge list
    /// ending in `target_edge`, or `None` when unreachable.
    pub fn find_route(
        &self,
        start: NodeId,
        target_edge: EdgeId,
        gauge: &TrackGauge,
    ) -> Option<Vec<EdgeId>> {
        let target = self.graph.edges.get(&target_edge)?;
        if !target.gauge.is_compatible_with(gauge) {
            return None;
        }
        let goal = target.from;
        if start == goal {
            return Some(vec![target_edge]);
        }
        let mut prev: BTreeMap<NodeId, (NodeId, EdgeId)> = BTreeMap::new();
        let mut visited: BTreeSet<NodeId> = BTreeSet::from([start]);
        let mut queue = VecDeque::from([start]);
        while let Some(n) = queue.pop_front() {
            for eid in self.graph.traversable_from(n, gauge) {
                let e = &self.graph.edges[&eid];
                if visited.insert(e.to) {
                    prev.insert(e.to, (n, eid));
                    queue.push_back(e.to);
                }
            }
        }
        let mut edges = vec![target_edge];
        let mut cur = goal;
        while cur != start {
            let (p, e) = *prev.get(&cur)?;
            edges.push(e);
            cur = p;
        }
        edges.reverse();
        Some(edges)
    }
}

// ---------------------------------------------------------------------------
// Tick: virtual transit, signals, stations, render modes
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum StopKind {
    None,
    Station,
    Boundary,
    EndOfLine,
}

/// Advance every scheduled train one tick across the graph — including
/// through unloaded chunks — updating virtual chunk coordinates, block
/// occupancy/reservations, and per-train render modes in O(1) per train per
/// tick without loading a single chunk. Returns the tick's events.
///
/// `loaded_chunks` is the set of loaded `(dim, cx, cz)` columns; a train
/// whose head chunk is absent advances in [`TrainRenderMode::Virtual`].
pub fn tick_rail_network(
    net: &mut RailNetwork,
    loaded_chunks: &BTreeSet<ChunkKey>,
) -> Vec<TransitEvent> {
    let mut events = Vec::new();
    // `mem::take` the shared id snapshot so the collection borrow doesn't
    // overlap `&mut net` below; it goes back with its capacity intact, so
    // steady-state ticks allocate nothing for the snapshot.
    let mut ids = std::mem::take(&mut net.id_scratch);
    ids.clear_bounded(MAX_TRAIN_SCRATCH_RETAIN);
    ids.extend(net.trains.keys().copied());
    for i in 0..ids.len() {
        let id = ids[i];
        if !net.trains.contains_key(&id) {
            continue;
        }
        release_reservations(net, id);
        if matches!(&net.trains[&id].state, TrainRunState::Running) {
            step_running(net, id, &mut events);
        } else {
            let t = net.trains.get_mut(&id).expect("checked");
            if let TrainRunState::WaitingAtStation {
                station,
                ticks_left,
            } = &mut t.state
            {
                if *ticks_left > 0 {
                    *ticks_left -= 1;
                } else {
                    // Move the station name into the departure event only on
                    // departure, avoiding a String clone on every waiting tick.
                    let station = std::mem::take(station);
                    // A decoded snapshot can carry schedule_idx = usize::MAX:
                    // saturate rather than panic on the increment.
                    t.schedule_idx = t.schedule_idx.saturating_add(1);
                    if t.schedule.looping && t.schedule_idx >= t.schedule.instructions.len() {
                        t.schedule_idx = 0;
                    }
                    t.target = None;
                    t.state = TrainRunState::Running;
                    events.push(TransitEvent::DepartedStation { train: id, station });
                }
            }
        }
        refresh_occupancy_gated(net, id);
        update_render_mode(net, id, loaded_chunks, &mut events);
    }
    net.tick = net.tick.wrapping_add(1);
    net.id_scratch = ids;
    events
}

/// Clear all block reservations held by `train`; they are re-acquired fresh
/// during the same tick's running step, so stale holds never linger.
fn release_reservations(net: &mut RailNetwork, train: TrainId) {
    for b in net.blocks.values_mut() {
        if b.reserved_by == Some(train) {
            b.reserved_by = None;
        }
    }
}

/// Fingerprint of everything [`refresh_occupancy`] reads to decide which
/// blocks `train` covers: head edge, arc position, body length, and the
/// traversed-edge history. The network's occupancy `epoch` is folded in so
/// any write to block fields or coverage-defining topology since the
/// commit invalidates the fingerprint.
fn occupancy_fingerprint(t: &ArticulatedTrain, epoch: u64) -> u64 {
    let mut f = FingerprintBuilder::new();
    f.write_u64(t.pos_edge.get())
        .write_u64(t.pos_s.to_bits())
        .write_u64(epoch)
        .write_u64(t.carriages.len() as u64);
    for c in &t.carriages {
        f.write_u64(c.length.to_bits());
    }
    for e in &t.history {
        f.write_u64(e.get());
    }
    f.finish()
}

/// [`refresh_occupancy`] behind the fingerprint gate: a train whose
/// coverage-defining state and the block-field epoch are both unchanged
/// since its last commit would only rewrite the same values, so the sweep
/// is skipped in O(1). Any real change — the train moved, another refresh
/// or mutation touched the fields — invalidates the fingerprint and the
/// refresh runs exactly as before.
fn refresh_occupancy_gated(net: &mut RailNetwork, train: TrainId) {
    let Some(t) = net.trains.get(&train) else {
        return;
    };
    let candidate = occupancy_fingerprint(t, net.occupancy_epoch);
    if net.occupancy_gate.should_skip(&train, candidate) {
        return;
    }
    refresh_occupancy(net, train);
    // Commit at the *post-refresh* epoch: if this run wrote fields it
    // bumped the epoch itself, and the commit must reflect that newer
    // write-set for next tick's skip decision.
    let epoch = net.occupancy_epoch;
    let fp = occupancy_fingerprint(&net.trains[&train], epoch);
    net.occupancy_gate.commit(train, fp);
}

/// Recompute which blocks `train` physically occupies (any edge under its
/// full body length) and clear occupancy it left behind. A write that
/// actually changes a block field bumps the occupancy epoch, invalidating
/// every committed refresh fingerprint.
fn refresh_occupancy(net: &mut RailNetwork, train: TrainId) {
    let Some(t) = net.trains.get(&train) else {
        return;
    };
    let mut covered: BTreeSet<BlockId> = BTreeSet::new();
    if let Some(b) = net.edge_block.get(&t.pos_edge) {
        covered.insert(*b);
    }
    let mut need = t.total_length() - t.pos_s;
    for e in t.history.iter().rev() {
        if need <= 0.0 {
            break;
        }
        if let Some(b) = net.edge_block.get(e) {
            covered.insert(*b);
        }
        need -= net.graph.edges.get(e).map(|e| e.length()).unwrap_or(0.0);
    }
    let mut wrote = false;
    for (bid, b) in net.blocks.iter_mut() {
        if covered.contains(bid) {
            if b.occupied_by != Some(train) {
                b.occupied_by = Some(train);
                wrote = true;
            }
            if b.reserved_by == Some(train) {
                b.reserved_by = None;
                wrote = true;
            }
        } else if b.occupied_by == Some(train) {
            b.occupied_by = None;
            wrote = true;
        }
    }
    if wrote {
        net.bump_occupancy_epoch();
    }
}

/// Report `Virtual`/`Loaded` from the head's chunk column, emitting a
/// `RenderModeChanged` event on transition.
fn update_render_mode(
    net: &mut RailNetwork,
    train: TrainId,
    loaded: &BTreeSet<ChunkKey>,
    events: &mut Vec<TransitEvent>,
) {
    let Some(t) = net.trains.get(&train) else {
        return;
    };
    let mode = t
        .head_world_pose(net)
        .map(|(pos, _)| {
            let key = ChunkKey::new(
                t.current_dim.clone(),
                (pos[0] / CHUNK_SIZE).floor() as i32,
                (pos[2] / CHUNK_SIZE).floor() as i32,
            );
            if loaded.contains(&key) {
                TrainRenderMode::Loaded
            } else {
                TrainRenderMode::Virtual
            }
        })
        .unwrap_or(TrainRenderMode::Virtual);
    if mode != t.render_mode {
        let t = net.trains.get_mut(&train).expect("checked");
        t.render_mode = mode;
        events.push(TransitEvent::RenderModeChanged { train, mode });
    }
}

/// Resolve the train's next schedule target into a route when needed.
/// Returns `false` when the train was parked (`Terminus`).
fn ensure_target(net: &mut RailNetwork, id: TrainId, events: &mut Vec<TransitEvent>) -> bool {
    let t = net.trains.get(&id).expect("checked");
    if t.target.is_some() {
        return true;
    }
    if t.schedule.instructions.is_empty() {
        return true; // free-running: follow switches to end of line
    }
    if t.schedule_idx >= t.schedule.instructions.len() {
        let t = net.trains.get_mut(&id).expect("checked");
        t.speed = 0.0;
        t.state = TrainRunState::Terminus;
        events.push(TransitEvent::ReachedTerminus { train: id });
        return false;
    }
    let instr = t.schedule.instructions[t.schedule_idx].clone();
    let Some(anchor) = net.stations.get(&instr.station_name).copied() else {
        let t = net.trains.get_mut(&id).expect("checked");
        t.speed = 0.0;
        t.state = TrainRunState::Terminus;
        events.push(TransitEvent::NoRoute {
            train: id,
            station: instr.station_name,
        });
        return false;
    };
    let gauge = t.gauge().clone();
    let pos_edge = t.pos_edge;
    let pos_s = t.pos_s;
    let route: Option<VecDeque<EdgeId>> = if anchor.edge == pos_edge {
        if anchor.s + 1e-9 >= pos_s {
            Some(VecDeque::new())
        } else {
            None
        }
    } else {
        match net.graph.edges.get(&pos_edge) {
            // The edge under the train vanished from the public graph map:
            // park it rather than panic on the index.
            None => None,
            Some(e) => net
                .find_route(e.to, anchor.edge, &gauge)
                .map(VecDeque::from),
        }
    };
    let t = net.trains.get_mut(&id).expect("checked");
    match route {
        Some(route) => {
            t.route = route;
            t.target = Some(StationTarget {
                station_name: instr.station_name,
                edge: anchor.edge,
                s: anchor.s,
                wait_ticks: instr.wait_ticks,
            });
            true
        }
        None => {
            t.speed = 0.0;
            t.state = TrainRunState::Terminus;
            events.push(TransitEvent::NoRoute {
                train: id,
                station: instr.station_name,
            });
            false
        }
    }
}

/// One tick of running physics for `id`: pick the nearest stop point
/// (scheduled station, signal-held boundary, or end of line), update
/// speed under accel/decel, and advance — never crossing a boundary whose
/// guarding signal is not green for this train.
fn step_running(net: &mut RailNetwork, id: TrainId, events: &mut Vec<TransitEvent>) {
    if !ensure_target(net, id, events) {
        return;
    }
    let (pos_edge, pos_s, speed, target, next_via_route) = {
        let t = net.trains.get(&id).expect("checked");
        (
            t.pos_edge,
            t.pos_s,
            t.speed,
            t.target.as_ref().map(|target| (target.edge, target.s)),
            t.route.front().copied(),
        )
    };
    let Some(edge) = net.graph.edges.get(&pos_edge) else {
        // The edge under the train vanished from the public graph map:
        // park it rather than panic on the index.
        let t = net.trains.get_mut(&id).expect("checked");
        t.speed = 0.0;
        t.state = TrainRunState::Terminus;
        events.push(TransitEvent::ReachedTerminus { train: id });
        return;
    };
    let edge_len = edge.length();
    let remaining = (edge_len - pos_s).max(0.0);
    let head_node = edge.to;

    let mut dist_to_stop = f64::INFINITY;
    let mut stop_kind = StopKind::None;
    let mut arrived_station: Option<String> = None;

    if let Some((target_edge, target_s)) = target {
        let d = if next_via_route.is_none() && target_edge == pos_edge {
            target_s - pos_s
        } else {
            let mut d = remaining;
            let n = net.trains[&id].route.len();
            for (i, e) in net.trains[&id].route.iter().enumerate() {
                d += if i + 1 == n {
                    target_s
                } else {
                    net.graph.edges.get(e).map(|x| x.length()).unwrap_or(0.0)
                };
            }
            d
        };
        dist_to_stop = d.max(0.0);
        stop_kind = StopKind::Station;
    }

    // Signal-guarded boundary ahead (or end of line when nothing follows).
    let next_edge = next_via_route.or_else(|| {
        if target.is_none() {
            net.graph.next_edge_for(head_node, net.trains[&id].gauge())
        } else {
            None
        }
    });
    match next_edge {
        Some(ne) => {
            if net.signal_aspect_for(ne, id) == SignalAspect::Green {
                net.reserve_block_for(ne, id);
            } else if remaining < dist_to_stop {
                dist_to_stop = remaining;
                stop_kind = StopKind::Boundary;
            }
        }
        None => {
            if target.is_none() && remaining < dist_to_stop {
                dist_to_stop = remaining;
                stop_kind = StopKind::EndOfLine;
            }
        }
    }

    let t = net.trains.get(&id).expect("checked");
    let braking_dist = speed * speed / (2.0 * t.decel);
    let new_speed = if dist_to_stop <= braking_dist {
        (speed - t.decel).max(0.0)
    } else {
        (speed + t.accel).min(t.max_speed)
    };

    // The move reaches the nearest stop point (or the head already stands
    // on it, e.g. spawned on the anchor or held at a red boundary).
    let reaches = dist_to_stop.is_finite()
        && (dist_to_stop <= 0.0 || (new_speed > 0.0 && new_speed >= dist_to_stop));

    if reaches {
        if dist_to_stop > 0.0 {
            advance(net, id, dist_to_stop);
        }
        let t = net.trains.get_mut(&id).expect("checked");
        t.speed = 0.0;
        match stop_kind {
            StopKind::Station => {
                if let Some(tg) = t.target.take() {
                    arrived_station = Some(tg.station_name.clone());
                    t.state = TrainRunState::WaitingAtStation {
                        station: tg.station_name,
                        ticks_left: tg.wait_ticks,
                    };
                    t.route.clear();
                }
            }
            StopKind::EndOfLine => {
                t.state = TrainRunState::Terminus;
                events.push(TransitEvent::ReachedTerminus { train: id });
            }
            // Held at a red boundary; re-evaluated next tick.
            StopKind::Boundary | StopKind::None => {}
        }
    } else {
        advance(net, id, new_speed);
        let t = net.trains.get_mut(&id).expect("checked");
        t.speed = new_speed;
    }

    if let Some(station) = arrived_station {
        events.push(TransitEvent::ArrivedAtStation { train: id, station });
    }
}

/// Whether `train` may physically enter `edge` right now: gauge-compatible
/// *and* its entry signal green for `train` (fail-closed).
fn can_enter(net: &RailNetwork, edge: EdgeId, train: TrainId) -> bool {
    let Some(t) = net.trains.get(&train) else {
        return false;
    };
    let Some(e) = net.graph.edges.get(&edge) else {
        return false;
    };
    t.gauge().is_compatible_with(&e.gauge) && net.signal_aspect_for(edge, train) == SignalAspect::Green
}

/// Move the head `d` blocks along the route, crossing nodes only through
/// passable edges; clamps at a boundary that turned red mid-move.
fn advance(net: &mut RailNetwork, id: TrainId, mut d: f64) {
    loop {
        let (pos_edge, pos_s) = {
            let t = net.trains.get(&id).expect("checked");
            (t.pos_edge, t.pos_s)
        };
        let Some(edge_len) = net.graph.edges.get(&pos_edge).map(|e| e.length()) else {
            // The edge under the train vanished from the public graph
            // map: hold position rather than panic on the index.
            let t = net.trains.get_mut(&id).expect("checked");
            t.speed = 0.0;
            return;
        };
        if pos_s + d <= edge_len + 1e-9 {
            let t = net.trains.get_mut(&id).expect("checked");
            t.pos_s = (pos_s + d).min(edge_len);
            return;
        }
        let next = {
            let t = net.trains.get(&id).expect("checked");
            t.route.front().copied().or_else(|| {
                net.graph
                    .edges
                    .get(&pos_edge)
                    .and_then(|e| net.graph.next_edge_for(e.to, t.gauge()))
            })
        };
        match next {
            Some(ne) if can_enter(net, ne, id) => {
                d -= edge_len - pos_s;
                let t = net.trains.get_mut(&id).expect("checked");
                t.history.push_back(pos_edge);
                if t.route.front() == Some(&ne) {
                    t.route.pop_front();
                }
                t.pos_edge = ne;
                t.pos_s = 0.0;
                let need = t.total_length();
                while t.history.len() > 1 {
                    let sum: f64 = t
                        .history
                        .iter()
                        .map(|e| net.graph.edges.get(e).map(|x| x.length()).unwrap_or(0.0))
                        .sum();
                    let front_len = t
                        .history
                        .front()
                        .and_then(|e| net.graph.edges.get(e))
                        .map(|e| e.length())
                        .unwrap_or(0.0);
                    if sum - front_len >= need {
                        t.history.pop_front();
                    } else {
                        break;
                    }
                }
                t.current_dim = net
                    .graph
                    .edges
                    .get(&ne)
                    .and_then(|e| net.graph.nodes.get(&e.from))
                    .map(|n| n.dim.clone())
                    .unwrap_or_else(|| t.current_dim.clone());
            }
            _ => {
                let t = net.trains.get_mut(&id).expect("checked");
                t.pos_s = edge_len;
                t.speed = 0.0;
                return;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// RAIL v1 snapshot codec (magic + version + len + CRC32 envelope)
// ---------------------------------------------------------------------------

const RAIL_HEADER_LEN: usize = 13; // 4 magic + 1 version + 4 len + 4 crc

/// Encode the whole network as a `RAIL` v1 envelope:
/// `RAIL` | `u8` version | `u32` payload length | `u32` IEEE CRC32 of the
/// payload | payload bytes.
pub fn encode_snapshot(net: &RailNetwork) -> Vec<u8> {
    let payload = encode_payload(net);
    let mut out = Vec::with_capacity(RAIL_HEADER_LEN + payload.len());
    out.extend_from_slice(&RAIL_MAGIC);
    out.push(RAIL_VERSION);
    out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    out.extend_from_slice(&crc32_ieee(&payload).to_be_bytes());
    out.extend_from_slice(&payload);
    out
}

/// Decode a `RAIL` v1 envelope. Fails closed on bad magic/version,
/// truncated header or payload, length mismatch, CRC mismatch, oversized
/// counts, dangling id references, or non-finite geometry.
pub fn decode_snapshot(bytes: &[u8]) -> Result<RailNetwork, RailError> {
    if bytes.len() < RAIL_HEADER_LEN {
        return Err(corrupt("truncated header"));
    }
    if bytes[..4] != RAIL_MAGIC {
        return Err(corrupt("bad magic"));
    }
    if bytes[4] != RAIL_VERSION {
        return Err(corrupt("bad version"));
    }
    let payload_len = u32::from_be_bytes([bytes[5], bytes[6], bytes[7], bytes[8]]) as usize;
    let crc = u32::from_be_bytes([bytes[9], bytes[10], bytes[11], bytes[12]]);
    let payload = &bytes[RAIL_HEADER_LEN..];
    if payload.len() != payload_len {
        return Err(corrupt("payload length mismatch"));
    }
    if crc32_ieee(payload) != crc {
        return Err(corrupt("crc32 mismatch"));
    }
    decode_payload(payload)
}

fn encode_payload(net: &RailNetwork) -> Vec<u8> {
    let mut out = Vec::with_capacity(256);
    w_u64(&mut out, net.tick);
    w_u64(&mut out, net.graph.next_node);
    w_u64(&mut out, net.graph.next_edge);
    w_u64(&mut out, net.next_block);

    write_varint(&mut out, net.graph.nodes.len() as u32);
    for n in net.graph.nodes.values() {
        w_u64(&mut out, n.id.0);
        write_mc_string(&mut out, n.dim.as_str());
        w_f64s(&mut out, &n.pos);
    }
    write_varint(&mut out, net.graph.edges.len() as u32);
    for e in net.graph.edges.values() {
        w_u64(&mut out, e.id.0);
        w_u64(&mut out, e.from.0);
        w_u64(&mut out, e.to.0);
        w_gauge(&mut out, &e.gauge);
        write_varint(&mut out, e.segments.len() as u32);
        for s in &e.segments {
            for p in &s.control {
                w_f64s(&mut out, p);
            }
        }
    }
    write_varint(&mut out, net.graph.switches.len() as u32);
    for (n, e) in &net.graph.switches {
        w_u64(&mut out, n.0);
        w_u64(&mut out, e.0);
    }
    write_varint(&mut out, net.blocks.len() as u32);
    for b in net.blocks.values() {
        w_u64(&mut out, b.id.0);
        write_varint(&mut out, b.edges.len() as u32);
        for e in &b.edges {
            w_u64(&mut out, e.0);
        }
        w_u64(&mut out, b.occupied_by.map(|t| t.0).unwrap_or(NO_TRAIN));
        w_u64(&mut out, b.reserved_by.map(|t| t.0).unwrap_or(NO_TRAIN));
    }
    write_varint(&mut out, net.signals.len() as u32);
    for (e, s) in &net.signals {
        w_u64(&mut out, e.0);
        out.push(match s.kind {
            SignalKind::Block => 0,
            SignalKind::Chain => 1,
        });
    }
    write_varint(&mut out, net.stations.len() as u32);
    for (name, a) in &net.stations {
        write_mc_string(&mut out, name);
        w_u64(&mut out, a.edge.0);
        w_f64(&mut out, a.s);
    }
    write_varint(&mut out, net.trains.len() as u32);
    for t in net.trains.values() {
        w_u64(&mut out, t.id.0);
        w_f64(&mut out, t.max_speed);
        w_f64(&mut out, t.accel);
        w_f64(&mut out, t.decel);
        w_f64(&mut out, t.speed);
        w_u64(&mut out, t.pos_edge.0);
        w_f64(&mut out, t.pos_s);
        write_mc_string(&mut out, t.current_dim.as_str());
        write_varint(&mut out, t.carriages.len() as u32);
        for c in &t.carriages {
            w_f64(&mut out, c.length);
            w_f64(&mut out, c.leading_bogey_offset);
            w_f64(&mut out, c.trailing_bogey_offset);
            w_gauge(&mut out, &c.gauge);
        }
        out.push(t.schedule.looping as u8);
        write_varint(&mut out, t.schedule.instructions.len() as u32);
        for i in &t.schedule.instructions {
            write_mc_string(&mut out, &i.station_name);
            w_u64(&mut out, i.wait_ticks);
        }
        w_u64(&mut out, t.schedule_idx as u64);
        match &t.state {
            TrainRunState::Running => out.push(0),
            TrainRunState::WaitingAtStation {
                station,
                ticks_left,
            } => {
                out.push(1);
                write_mc_string(&mut out, station);
                w_u64(&mut out, *ticks_left);
            }
            TrainRunState::Terminus => out.push(2),
        }
        out.push(t.render_mode as u8);
        out.push(t.target.is_some() as u8);
        if let Some(tg) = &t.target {
            write_mc_string(&mut out, &tg.station_name);
            w_u64(&mut out, tg.edge.0);
            w_f64(&mut out, tg.s);
            w_u64(&mut out, tg.wait_ticks);
        }
        write_varint(&mut out, t.route.len() as u32);
        for e in &t.route {
            w_u64(&mut out, e.0);
        }
        write_varint(&mut out, t.history.len() as u32);
        for e in &t.history {
            w_u64(&mut out, e.0);
        }
    }
    out
}

fn decode_payload(bytes: &[u8]) -> Result<RailNetwork, RailError> {
    let mut c = 0usize;
    let mut net = RailNetwork::new();
    net.tick = r_u64(bytes, &mut c).ok_or_else(|| corrupt("truncated tick"))?;
    net.graph.next_node = r_u64(bytes, &mut c).ok_or_else(|| corrupt("truncated counters"))?;
    net.graph.next_edge = r_u64(bytes, &mut c).ok_or_else(|| corrupt("truncated counters"))?;
    net.next_block = r_u64(bytes, &mut c).ok_or_else(|| corrupt("truncated counters"))?;

    let n_nodes = r_count(bytes, &mut c).ok_or_else(|| corrupt("bad node count"))?;
    for _ in 0..n_nodes {
        let id = NodeId(r_u64(bytes, &mut c).ok_or_else(|| corrupt("truncated node"))?);
        let dim = DimId(
            read_mc_string(bytes, &mut c, MAX_NAME_LEN).ok_or_else(|| corrupt("bad node dim"))?,
        );
        let pos = r_f64s(bytes, &mut c).ok_or_else(|| corrupt("truncated node pos"))?;
        if !pos.iter().all(|v| v.is_finite()) {
            return Err(corrupt("non-finite node pos"));
        }
        net.graph.nodes.insert(
            id,
            TrackNode {
                id,
                dim,
                pos,
                out_edges: BTreeSet::new(),
                in_edges: BTreeSet::new(),
            },
        );
    }
    let n_edges = r_count(bytes, &mut c).ok_or_else(|| corrupt("bad edge count"))?;
    for _ in 0..n_edges {
        let id = EdgeId(r_u64(bytes, &mut c).ok_or_else(|| corrupt("truncated edge"))?);
        let from = NodeId(r_u64(bytes, &mut c).ok_or_else(|| corrupt("truncated edge"))?);
        let to = NodeId(r_u64(bytes, &mut c).ok_or_else(|| corrupt("truncated edge"))?);
        if !net.graph.nodes.contains_key(&from) || !net.graph.nodes.contains_key(&to) {
            return Err(corrupt("edge references unknown node"));
        }
        let gauge = r_gauge(bytes, &mut c).ok_or_else(|| corrupt("bad edge gauge"))?;
        let n_segs = r_count(bytes, &mut c).ok_or_else(|| corrupt("bad segment count"))?;
        if n_segs == 0 || n_segs > 1024 {
            return Err(corrupt("segment count out of range"));
        }
        let mut controls = Vec::with_capacity(n_segs);
        for _ in 0..n_segs {
            let mut control = [[0.0; 3]; 4];
            for p in &mut control {
                *p = r_f64s(bytes, &mut c).ok_or_else(|| corrupt("truncated control point"))?;
            }
            controls.push(control);
        }
        let segs = controls
            .into_iter()
            .map(CubicBezierSegment::new)
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| corrupt("degenerate edge geometry"))?;
        let edge = TrackEdge::new(id, from, to, gauge, segs)
            .map_err(|_| corrupt("degenerate edge"))?;
        net.graph
            .nodes
            .get_mut(&from)
            .map(|n| n.out_edges.insert(id));
        net.graph.nodes.get_mut(&to).map(|n| n.in_edges.insert(id));
        net.graph.edges.insert(id, edge);
    }
    let n_sw = r_count(bytes, &mut c).ok_or_else(|| corrupt("bad switch count"))?;
    for _ in 0..n_sw {
        let n = NodeId(r_u64(bytes, &mut c).ok_or_else(|| corrupt("truncated switch"))?);
        let e = EdgeId(r_u64(bytes, &mut c).ok_or_else(|| corrupt("truncated switch"))?);
        let node = net.graph.nodes.get(&n).ok_or_else(|| corrupt("switch node missing"))?;
        if !node.out_edges.contains(&e) {
            return Err(corrupt("switch edge not outgoing"));
        }
        net.graph.switches.insert(n, e);
    }
    let n_blocks = r_count(bytes, &mut c).ok_or_else(|| corrupt("bad block count"))?;
    for _ in 0..n_blocks {
        let id = BlockId(r_u64(bytes, &mut c).ok_or_else(|| corrupt("truncated block"))?);
        let n_be = r_count(bytes, &mut c).ok_or_else(|| corrupt("bad block edge count"))?;
        let mut edges = BTreeSet::new();
        for _ in 0..n_be {
            let e = EdgeId(r_u64(bytes, &mut c).ok_or_else(|| corrupt("truncated block edge"))?);
            if !net.graph.edges.contains_key(&e) {
                return Err(corrupt("block references unknown edge"));
            }
            net.edge_block.insert(e, id);
            edges.insert(e);
        }
        let occ = r_u64(bytes, &mut c).ok_or_else(|| corrupt("truncated occupancy"))?;
        let res = r_u64(bytes, &mut c).ok_or_else(|| corrupt("truncated occupancy"))?;
        net.blocks.insert(
            id,
            SignalBlock {
                id,
                edges,
                occupied_by: (occ != NO_TRAIN).then_some(TrainId(occ)),
                reserved_by: (res != NO_TRAIN).then_some(TrainId(res)),
            },
        );
    }
    let n_sig = r_count(bytes, &mut c).ok_or_else(|| corrupt("bad signal count"))?;
    for _ in 0..n_sig {
        let e = EdgeId(r_u64(bytes, &mut c).ok_or_else(|| corrupt("truncated signal"))?);
        if !net.graph.edges.contains_key(&e) {
            return Err(corrupt("signal references unknown edge"));
        }
        let kind = match r_u8(bytes, &mut c).ok_or_else(|| corrupt("truncated signal"))? {
            0 => SignalKind::Block,
            1 => SignalKind::Chain,
            _ => return Err(corrupt("bad signal kind")),
        };
        net.signals.insert(e, Signal { kind });
    }
    let n_st = r_count(bytes, &mut c).ok_or_else(|| corrupt("bad station count"))?;
    for _ in 0..n_st {
        let name =
            read_mc_string(bytes, &mut c, MAX_NAME_LEN).ok_or_else(|| corrupt("bad station name"))?;
        let edge = EdgeId(r_u64(bytes, &mut c).ok_or_else(|| corrupt("truncated station"))?);
        let s = r_f64(bytes, &mut c).ok_or_else(|| corrupt("truncated station"))?;
        let len = net
            .graph
            .edges
            .get(&edge)
            .ok_or_else(|| corrupt("station references unknown edge"))?
            .length();
        if !(s.is_finite() && s >= 0.0 && s <= len) {
            return Err(corrupt("station anchor out of range"));
        }
        net.stations.insert(name, StationAnchor { edge, s });
    }
    let n_trains = r_count(bytes, &mut c).ok_or_else(|| corrupt("bad train count"))?;
    for _ in 0..n_trains {
        let id = TrainId(r_u64(bytes, &mut c).ok_or_else(|| corrupt("truncated train"))?);
        let max_speed = r_f64(bytes, &mut c).ok_or_else(|| corrupt("truncated train"))?;
        let accel = r_f64(bytes, &mut c).ok_or_else(|| corrupt("truncated train"))?;
        let decel = r_f64(bytes, &mut c).ok_or_else(|| corrupt("truncated train"))?;
        let speed = r_f64(bytes, &mut c).ok_or_else(|| corrupt("truncated train"))?;
        let pos_edge = EdgeId(r_u64(bytes, &mut c).ok_or_else(|| corrupt("truncated train"))?);
        let pos_s = r_f64(bytes, &mut c).ok_or_else(|| corrupt("truncated train"))?;
        let current_dim = DimId(
            read_mc_string(bytes, &mut c, MAX_NAME_LEN).ok_or_else(|| corrupt("bad train dim"))?,
        );
        let n_car = r_count(bytes, &mut c).ok_or_else(|| corrupt("bad carriage count"))?;
        if n_car == 0 || n_car > 4096 {
            return Err(corrupt("carriage count out of range"));
        }
        let mut carriages = Vec::with_capacity(n_car);
        for _ in 0..n_car {
            let length = r_f64(bytes, &mut c).ok_or_else(|| corrupt("truncated carriage"))?;
            let leading_bogey_offset =
                r_f64(bytes, &mut c).ok_or_else(|| corrupt("truncated carriage"))?;
            let trailing_bogey_offset =
                r_f64(bytes, &mut c).ok_or_else(|| corrupt("truncated carriage"))?;
            let gauge = r_gauge(bytes, &mut c).ok_or_else(|| corrupt("bad carriage gauge"))?;
            carriages.push(CarriageSpec {
                length,
                leading_bogey_offset,
                trailing_bogey_offset,
                gauge,
            });
        }
        let looping = match r_u8(bytes, &mut c).ok_or_else(|| corrupt("truncated schedule"))? {
            0 => false,
            1 => true,
            _ => return Err(corrupt("bad looping flag")),
        };
        let n_ins = r_count(bytes, &mut c).ok_or_else(|| corrupt("bad schedule count"))?;
        let mut instructions = Vec::with_capacity(n_ins.min(4096));
        for _ in 0..n_ins {
            let station_name = read_mc_string(bytes, &mut c, MAX_NAME_LEN)
                .ok_or_else(|| corrupt("bad instruction station"))?;
            let wait_ticks =
                r_u64(bytes, &mut c).ok_or_else(|| corrupt("truncated instruction"))?;
            instructions.push(StationInstruction {
                station_name,
                wait_ticks,
            });
        }
        let schedule_idx =
            r_u64(bytes, &mut c).ok_or_else(|| corrupt("truncated schedule_idx"))? as usize;
        let state = match r_u8(bytes, &mut c).ok_or_else(|| corrupt("truncated state"))? {
            0 => TrainRunState::Running,
            1 => {
                let station = read_mc_string(bytes, &mut c, MAX_NAME_LEN)
                    .ok_or_else(|| corrupt("bad waiting station"))?;
                let ticks_left =
                    r_u64(bytes, &mut c).ok_or_else(|| corrupt("truncated waiting"))?;
                TrainRunState::WaitingAtStation {
                    station,
                    ticks_left,
                }
            }
            2 => TrainRunState::Terminus,
            _ => return Err(corrupt("bad train state")),
        };
        let render_mode = match r_u8(bytes, &mut c).ok_or_else(|| corrupt("truncated mode"))? {
            0 => TrainRenderMode::Virtual,
            1 => TrainRenderMode::Loaded,
            _ => return Err(corrupt("bad render mode")),
        };
        let has_target = r_u8(bytes, &mut c).ok_or_else(|| corrupt("truncated target"))?;
        let target = match has_target {
            0 => None,
            1 => {
                let station_name = read_mc_string(bytes, &mut c, MAX_NAME_LEN)
                    .ok_or_else(|| corrupt("bad target station"))?;
                let edge =
                    EdgeId(r_u64(bytes, &mut c).ok_or_else(|| corrupt("truncated target"))?);
                let s = r_f64(bytes, &mut c).ok_or_else(|| corrupt("truncated target"))?;
                let wait_ticks =
                    r_u64(bytes, &mut c).ok_or_else(|| corrupt("truncated target"))?;
                Some(StationTarget {
                    station_name,
                    edge,
                    s,
                    wait_ticks,
                })
            }
            _ => return Err(corrupt("bad target flag")),
        };
        let n_route = r_count(bytes, &mut c).ok_or_else(|| corrupt("bad route count"))?;
        let mut route = VecDeque::with_capacity(n_route.min(4096));
        for _ in 0..n_route {
            route.push_back(EdgeId(
                r_u64(bytes, &mut c).ok_or_else(|| corrupt("truncated route"))?,
            ));
        }
        let n_hist = r_count(bytes, &mut c).ok_or_else(|| corrupt("bad history count"))?;
        let mut history = VecDeque::with_capacity(n_hist.min(4096));
        for _ in 0..n_hist {
            history.push_back(EdgeId(
                r_u64(bytes, &mut c).ok_or_else(|| corrupt("truncated history"))?,
            ));
        }
        if !net.graph.edges.contains_key(&pos_edge) {
            return Err(corrupt("train on unknown edge"));
        }
        if !pos_s.is_finite() || pos_s < 0.0 || pos_s > net.graph.edges[&pos_edge].length() {
            return Err(corrupt("train position out of range"));
        }
        if !speed.is_finite() {
            return Err(corrupt("non-finite train speed"));
        }
        for e in route.iter().chain(history.iter()) {
            if !net.graph.edges.contains_key(e) {
                return Err(corrupt("train path references unknown edge"));
            }
        }
        if let Some(tg) = &target {
            if !net.graph.edges.contains_key(&tg.edge) {
                return Err(corrupt("target references unknown edge"));
            }
        }
        let mut train = ArticulatedTrain::new(
            id,
            carriages,
            max_speed,
            accel,
            decel,
            TrainSchedule {
                instructions,
                looping,
            },
        )
        .map_err(|_| corrupt("invalid train spec"))?;
        train.speed = speed;
        train.pos_edge = pos_edge;
        train.pos_s = pos_s;
        train.current_dim = current_dim;
        train.render_mode = render_mode;
        train.state = state;
        train.schedule_idx = schedule_idx;
        train.target = target;
        train.route = route;
        train.history = history;
        net.trains.insert(id, train);
    }
    if c != bytes.len() {
        return Err(corrupt("trailing bytes"));
    }
    // Counters must cover every id actually in use.
    let max_node = net.graph.nodes.keys().next_back().map(|n| n.0).unwrap_or(0);
    let max_edge = net.graph.edges.keys().next_back().map(|e| e.0).unwrap_or(0);
    let max_block = net.blocks.keys().next_back().map(|b| b.0).unwrap_or(0);
    if net.graph.next_node <= max_node
        || net.graph.next_edge <= max_edge
        || net.next_block <= max_block
    {
        return Err(corrupt("id counters below live ids"));
    }
    Ok(net)
}

/// IEEE 802.3 CRC32 (poly `0xEDB88320`, reflected, init/xorout
/// `0xFFFFFFFF`) — exposed so integrators can verify envelopes and tests
/// can craft corruption behind the checksum.
pub fn crc32_ieee(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &b in data {
        crc = CRC32_IEEE_TABLE[((crc ^ u32::from(b)) & 0xFF) as usize] ^ (crc >> 8);
    }
    crc ^ 0xFFFF_FFFF
}

const CRC32_IEEE_TABLE: [u32; 256] = crc32_table();

const fn crc32_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut i = 0usize;
    while i < 256 {
        let mut c = i as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 {
                0xEDB8_8320 ^ (c >> 1)
            } else {
                c >> 1
            };
            k += 1;
        }
        table[i] = c;
        i += 1;
    }
    table
}

// ---------------------------------------------------------------------------
// Vector & codec helpers
// ---------------------------------------------------------------------------

fn bezier_eval(c: &[[f64; 3]; 4], t: f64) -> [f64; 3] {
    let u = 1.0 - t;
    let (u2, u3) = (u * u, u * u * u);
    let (t2, t3) = (t * t, t * t * t);
    let mut out = [0.0; 3];
    for i in 0..3 {
        out[i] = u3 * c[0][i] + 3.0 * u2 * t * c[1][i] + 3.0 * u * t2 * c[2][i] + t3 * c[3][i];
    }
    out
}

fn bezier_deriv(c: &[[f64; 3]; 4], t: f64) -> [f64; 3] {
    let u = 1.0 - t;
    let mut out = [0.0; 3];
    for i in 0..3 {
        out[i] = 3.0 * u * u * (c[1][i] - c[0][i])
            + 6.0 * u * t * (c[2][i] - c[1][i])
            + 3.0 * t * t * (c[3][i] - c[2][i]);
    }
    out
}

fn sub3(a: [f64; 3], b: [f64; 3]) -> [f64; 3] {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}

fn norm3(v: [f64; 3]) -> f64 {
    (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt()
}

fn dist3(a: [f64; 3], b: [f64; 3]) -> f64 {
    norm3(sub3(a, b))
}

fn normalize3(v: [f64; 3]) -> [f64; 3] {
    let n = norm3(v);
    if !n.is_finite() || n < 1e-12 {
        return [1.0, 0.0, 0.0];
    }
    [v[0] / n, v[1] / n, v[2] / n]
}

fn w_gauge(out: &mut Vec<u8>, g: &TrackGauge) {
    match g {
        TrackGauge::Standard => out.push(0),
        TrackGauge::Narrow => out.push(1),
        TrackGauge::Wide => out.push(2),
        TrackGauge::Monorail => out.push(3),
        TrackGauge::Custom(name) => {
            out.push(4);
            write_mc_string(out, name);
        }
    }
}

fn r_gauge(bytes: &[u8], c: &mut usize) -> Option<TrackGauge> {
    match r_u8(bytes, c)? {
        0 => Some(TrackGauge::Standard),
        1 => Some(TrackGauge::Narrow),
        2 => Some(TrackGauge::Wide),
        3 => Some(TrackGauge::Monorail),
        4 => Some(TrackGauge::Custom(read_mc_string(
            bytes,
            c,
            MAX_NAME_LEN,
        )?)),
        _ => None,
    }
}

fn w_u64(out: &mut Vec<u8>, v: u64) {
    out.extend_from_slice(&v.to_be_bytes());
}

fn w_f64(out: &mut Vec<u8>, v: f64) {
    out.extend_from_slice(&v.to_be_bytes());
}

fn w_f64s(out: &mut Vec<u8>, vs: &[f64; 3]) {
    for v in vs {
        w_f64(out, *v);
    }
}

fn r_u8(bytes: &[u8], c: &mut usize) -> Option<u8> {
    let b = *bytes.get(*c)?;
    *c += 1;
    Some(b)
}

fn r_u64(bytes: &[u8], c: &mut usize) -> Option<u64> {
    let s = bytes.get(*c..*c + 8)?;
    *c += 8;
    Some(u64::from_be_bytes(s.try_into().ok()?))
}

fn r_f64(bytes: &[u8], c: &mut usize) -> Option<f64> {
    let s = bytes.get(*c..*c + 8)?;
    *c += 8;
    Some(f64::from_be_bytes(s.try_into().ok()?))
}

fn r_f64s(bytes: &[u8], c: &mut usize) -> Option<[f64; 3]> {
    Some([r_f64(bytes, c)?, r_f64(bytes, c)?, r_f64(bytes, c)?])
}

fn r_count(bytes: &[u8], c: &mut usize) -> Option<usize> {
    let n = read_varint(bytes, c)? as usize;
    // Each record needs at least 1 byte; an inflated count is provably corrupt.
    (n <= bytes.len()).then_some(n)
}
