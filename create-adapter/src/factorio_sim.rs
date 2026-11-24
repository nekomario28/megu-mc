// SPDX-License-Identifier: LGPL-3.0-or-later

//! Factorio-style data-oriented kernels wired directly onto Create factory
//! mechanics, keeping 1,000+ machine factories at a stable 20 TPS:
//!
//! * [`CreateBeltLine`] — a gap-encoded `O(1)` mechanical belt transport
//!   line. Each stack stores only the distance to the stack ahead of it, so
//!   a free-flowing advance touches exactly one counter while empty,
//!   stopped, or fully jammed lines sleep at zero per-tick visits.
//! * [`KineticNetworkPool`] — contiguous SoA storage for shafts, cogwheels,
//!   gearboxes (ratio + direction flip), waterwheels, windmills, steam
//!   engines, and the mechanical consumers they drive. Block place/break
//!   and clutch toggles re-solve only the touched connected component via
//!   reusable scratch BFS buffers; an untouched network ticks at zero node
//!   visits and zero heap allocation.
//! * [`ActiveFactoryScheduler`] — an event-driven active/sleeping machine
//!   set for presses, mixers, fans, crushing wheels, fluid pumps, and
//!   deployers: idle machines cost zero visits per tick and wake on item,
//!   fluid-pipe, or power events and on exact processing-deadline ticks in
//!   canonical `(tick, id)` order for bit-identical factory digests.
//!
//! Every hot-path quantity is a deterministic fixed-point integer — no
//! floating point anywhere.

#![forbid(unsafe_code)]

use std::collections::{BTreeMap, BTreeSet, VecDeque};

// ===========================================================================
// 1. O(1) gap-encoded Create mechanical belt transport line
// ===========================================================================

/// Fixed-point subdivision of one belt block. All belt positions are integer
/// multiples of `1 / SUB_UNITS_PER_BLOCK` of a block — zero drift.
pub const SUB_UNITS_PER_BLOCK: u32 = 256;

/// Minimum front-to-front spacing between two item stacks on a belt line
/// (a quarter block), matching Create's packed item behaviour.
pub const BELT_ITEM_SPAN: u32 = SUB_UNITS_PER_BLOCK / 4;

/// RPM → belt advance rate in sub-units per tick. At 64 rpm a belt moves one
/// block per tick; negative rpm reverses. The map is exactly odd, so
/// `rpm_to_belt_step(-r) == -rpm_to_belt_step(r)` — no asymmetric drift.
pub fn rpm_to_belt_step(rpm: i32) -> i32 {
    ((i64::from(rpm) * i64::from(SUB_UNITS_PER_BLOCK)) / 64)
        .clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32
}

/// One item stack riding a [`CreateBeltLine`].
///
/// `gap_ahead` is the gap-encoded position: for the head stack it is the
/// distance remaining to the belt exit; for every trailing stack it is the
/// front-to-front distance to the stack ahead (always `>= BELT_ITEM_SPAN`).
/// `processing_ticks` counts down while a stack is touched by `advance`
/// (belt-bound processing such as fan blasting over a depot section).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BeltItem {
    pub gap_ahead: u32,
    pub item_id: u32,
    pub count: u16,
    pub processing_ticks: u16,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BeltError {
    /// Belt shorter than one item span — cannot carry anything.
    InvalidLength,
    /// Zero-count stacks are not transportable.
    EmptyStack,
    /// No room at the insert end this tick — a funnel must retry.
    BeltFull,
}

/// A single Create mechanical belt transport line between its insert end
/// and its exit end (depot/funnel/chute handoff points).
///
/// Stack positions are implicit — stack `i` sits `Σ gap[0..=i]` sub-units
/// from the exit — so a free-flowing tick only rewrites the head gap (one
/// counter, `O(1)`), a jam propagates backward only while compressible
/// slack remains, and a fully packed or empty line sleeps at zero visits.
/// `try_insert_back`, `try_extract_front`, and `set_rpm` wake it instantly.
pub struct CreateBeltLine {
    len_sub: u32,
    /// Advance magnitude in sub-units per tick (0 = stopped).
    step: u32,
    /// Direction flag: `false` after a negative-RPM flip.
    forward: bool,
    /// Stacks ordered head → tail. Index 0 is closest to the exit.
    items: VecDeque<BeltItem>,
    /// `Σ gap_ahead` = tail stack's distance to the exit.
    total_gap: u64,
    /// `Σ_{i ≥ 1} (gap_i − ITEM_SPAN)`: compressible room. Zero ⇒ packed.
    slack: u64,
    last_visits: u32,
    total_visits: u64,
    inserted_items: u64,
    extracted_items: u64,
    /// Reusable buffer for direction flips — no steady-state allocation.
    reverse_scratch: Vec<BeltItem>,
}

impl CreateBeltLine {
    /// A transport line `len_sub_units` sub-units long.
    pub fn new(len_sub_units: u32) -> Self {
        Self {
            len_sub: len_sub_units,
            step: 0,
            forward: true,
            items: VecDeque::new(),
            total_gap: 0,
            slack: 0,
            last_visits: 0,
            total_visits: 0,
            inserted_items: 0,
            extracted_items: 0,
            reverse_scratch: Vec::new(),
        }
    }

    /// A transport line `blocks` Create blocks long.
    pub fn for_blocks(blocks: u32) -> Self {
        Self::new(blocks.saturating_mul(SUB_UNITS_PER_BLOCK))
    }

    pub fn len_sub_units(&self) -> u32 {
        self.len_sub
    }

    /// Stacks currently on the line, ordered head → tail.
    pub fn items(&self) -> &VecDeque<BeltItem> {
        &self.items
    }

    /// Number of item stacks on the line.
    pub fn item_slots(&self) -> usize {
        self.items.len()
    }

    /// Total item units on the line (`Σ count`). O(n) — for accounting only.
    pub fn on_belt_items(&self) -> u64 {
        self.items.iter().map(|it| u64::from(it.count)).sum()
    }

    /// Conservation ledgers: `inserted_items == extracted_items +
    /// on_belt_items()` at every observable point.
    pub fn inserted_items(&self) -> u64 {
        self.inserted_items
    }

    pub fn extracted_items(&self) -> u64 {
        self.extracted_items
    }

    /// Stacks touched by the most recent [`advance`](Self::advance) call:
    /// 1 while free-flowing, a bounded run while compressing into a jam,
    /// 0 while empty, stopped, or fully packed.
    pub fn last_visits(&self) -> u32 {
        self.last_visits
    }

    pub fn total_visits(&self) -> u64 {
        self.total_visits
    }

    /// Current belt advance rate magnitude in sub-units per tick.
    pub fn step(&self) -> u32 {
        self.step
    }

    /// `true` while the line can do no work this tick: empty, stopped, or
    /// head-blocked with zero compressible slack.
    pub fn is_asleep(&self) -> bool {
        self.step == 0 || self.items.is_empty() || (self.items[0].gap_ahead == 0 && self.slack == 0)
    }

    /// Absolute distance-to-exit of stack `index` in sub-units. O(n) —
    /// intended for assertions and sync snapshots, not the hot path.
    pub fn position_of(&self, index: usize) -> Option<u32> {
        if index >= self.items.len() {
            return None;
        }
        let mut pos = 0u64;
        for it in self.items.iter().take(index + 1) {
            pos += u64::from(it.gap_ahead);
        }
        Some(pos as u32)
    }

    /// Drive the line from kinetic RPM. A sign change wakes the line and
    /// mirrors the stack order in O(n) — a rare structural edit, never on
    /// the steady-state path.
    pub fn set_rpm(&mut self, rpm: i32) {
        let step = rpm_to_belt_step(rpm);
        let forward = step >= 0;
        if forward != self.forward {
            self.reverse_items();
            self.forward = forward;
        }
        self.step = step.unsigned_abs();
    }

    /// Funnel/chute handoff: drop a stack onto the line's back edge.
    /// Strict conservation — `inserted_items` grows by `count`. Fails with
    /// [`BeltError::BeltFull`] while the insert end lacks `BELT_ITEM_SPAN`.
    pub fn try_insert_back(
        &mut self,
        item_id: u32,
        count: u16,
        processing_ticks: u16,
    ) -> Result<(), BeltError> {
        if self.len_sub < BELT_ITEM_SPAN {
            return Err(BeltError::InvalidLength);
        }
        if count == 0 {
            return Err(BeltError::EmptyStack);
        }
        if self.items.is_empty() {
            // First stack lands on the back edge, `len_sub` from the exit.
            self.items.push_back(BeltItem {
                gap_ahead: self.len_sub,
                item_id,
                count,
                processing_ticks,
            });
            self.total_gap += u64::from(self.len_sub);
        } else {
            let room = u64::from(self.len_sub) - self.total_gap;
            if room < u64::from(BELT_ITEM_SPAN) {
                return Err(BeltError::BeltFull);
            }
            let gap = room as u32;
            self.items.push_back(BeltItem {
                gap_ahead: gap,
                item_id,
                count,
                processing_ticks,
            });
            self.total_gap += u64::from(gap);
            self.slack += u64::from(gap) - u64::from(BELT_ITEM_SPAN);
        }
        self.inserted_items += u64::from(count);
        Ok(())
    }

    /// Depot handoff: pop the head stack once it has reached the exit edge.
    /// Returns `None` while the head is still travelling — extraction is the
    /// only unjam wake-up.
    pub fn try_extract_front(&mut self) -> Option<BeltItem> {
        let head = self.items.front()?;
        if head.gap_ahead != 0 {
            return None;
        }
        let head = self.items.pop_front().unwrap();
        if let Some(next) = self.items.front_mut() {
            // The promoted head's old gap leaves the trailing-slack pool;
            // its new gap reaches the exit the old head vacated.
            self.slack -= u64::from(next.gap_ahead) - u64::from(BELT_ITEM_SPAN);
            next.gap_ahead += head.gap_ahead;
        }
        self.total_gap -= u64::from(head.gap_ahead);
        self.extracted_items += u64::from(head.count);
        Some(head)
    }

    /// Advance the line one tick. Returns the number of stacks visited:
    /// `1` while free-flowing (only the head gap is rewritten), a bounded
    /// run while a jam compresses slack toward the tail, and `0` while the
    /// line is asleep — empty, stopped, or head-blocked and fully packed.
    pub fn advance(&mut self) -> u32 {
        self.last_visits = 0;
        if self.is_asleep() {
            return 0;
        }
        let step = self.step;
        let head_move = step.min(self.items[0].gap_ahead);
        {
            let head = &mut self.items[0];
            head.gap_ahead -= head_move;
            head.processing_ticks = head.processing_ticks.saturating_sub(1);
        }
        self.total_gap -= u64::from(head_move);
        self.last_visits = 1;
        let mut m_prev = head_move;
        if m_prev == step {
            self.total_visits += 1;
            return 1; // free flow: trailing gaps all unchanged — O(1)
        }
        // Head reached the exit and is blocked: trailing stacks keep moving
        // while compressible slack remains. Stack i may advance into its own
        // slack plus the room stack i−1 vacated: m_i = min(step, slack_i +
        // m_{i−1}); its gap to the stack ahead becomes g_i + m_{i−1} − m_i.
        let n = self.items.len();
        let mut i = 1;
        while i < n && self.slack > 0 {
            let slack_i = self.items[i].gap_ahead - BELT_ITEM_SPAN;
            let room = u64::from(slack_i) + u64::from(m_prev);
            let m = u64::from(step).min(room) as u32;
            self.items[i].gap_ahead =
                (i64::from(self.items[i].gap_ahead) + i64::from(m_prev) - i64::from(m)) as u32;
            self.slack = (self.slack as i64 + i64::from(m_prev) - i64::from(m)) as u64;
            self.items[i].processing_ticks = self.items[i].processing_ticks.saturating_sub(1);
            self.last_visits += 1;
            m_prev = m;
            i += 1;
            if m == step {
                break; // everything behind moves freely — stop scanning
            }
        }
        self.total_visits += u64::from(self.last_visits);
        self.last_visits
    }

    /// Mirror the line in place for a direction flip: the old tail becomes
    /// the head at `len_sub − p_tail`; every other stack inherits the gap of
    /// the stack now ahead of it, preserving the slack pool exactly.
    fn reverse_items(&mut self) {
        let n = self.items.len();
        if n == 0 {
            return;
        }
        let old_head_gap = self.items[0].gap_ahead;
        let new_head_gap = self.len_sub - self.total_gap as u32;
        self.reverse_scratch.clear();
        self.reverse_scratch
            .extend(self.items.iter().rev().copied());
        // Each stack inherits the ORIGINAL gap of the stack now ahead of it
        // (scratch[j − 1]); iterate descending so those reads stay pristine.
        for j in (1..n).rev() {
            self.reverse_scratch[j].gap_ahead = self.reverse_scratch[j - 1].gap_ahead;
        }
        self.reverse_scratch[0].gap_ahead = new_head_gap;
        self.items.clear();
        self.items.extend(self.reverse_scratch.drain(..));
        self.total_gap = u64::from(self.len_sub) - u64::from(old_head_gap);
    }
}

// ===========================================================================
// 2. Sleep/wake Create kinetic stress network — contiguous SoA pool
// ===========================================================================

/// Fixed-point RPM unit (`Q16.16` signed): one RPM == `RPM_FP_ONE`.
pub const RPM_FP_ONE: i64 = 1 << 16;

/// Whole RPM → fixed-point.
pub fn rpm_fp(rpm: i64) -> i64 {
    rpm * RPM_FP_ONE
}

/// `num/den` gear ratio → signed fixed-point (negative flips direction).
pub fn ratio_fp(num: i64, den: i64) -> i64 {
    (num * RPM_FP_ONE) / den
}

/// Create kinetic block roles understood by the pool. Generators carry
/// stress capacity plus a driven RPM; consumers carry stress impact.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KineticKind {
    Shaft,
    Cogwheel,
    Gearbox,
    Waterwheel,
    Windmill,
    SteamEngine,
    MechanicalPress,
    MechanicalMixer,
    CrushingWheel,
    Clutch,
}

impl KineticKind {
    /// Stress units generated (Create-style `su` at its operating speed).
    pub fn default_capacity(self) -> u64 {
        match self {
            Self::Waterwheel => 256,
            Self::Windmill => 512,
            Self::SteamEngine => 2048,
            _ => 0,
        }
    }

    /// Stress units demanded while running (at the caller's solved speed —
    /// scale externally if speed-dependent impact is wanted).
    pub fn default_impact(self) -> u64 {
        match self {
            Self::MechanicalMixer => 32,
            Self::MechanicalPress => 64,
            Self::CrushingWheel => 128,
            _ => 0,
        }
    }

    /// Driven source RPM in fixed-point (0 ⇒ passive transmission block).
    pub fn default_drive_rpm_fp(self) -> i64 {
        match self {
            Self::Waterwheel | Self::Windmill => rpm_fp(8),
            Self::SteamEngine => rpm_fp(16),
            _ => 0,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KineticStatus {
    Unpowered,
    Running,
    /// `total_impact > total_capacity` — the component halts, exactly like
    /// an overstressed Create network, and recovers when capacity returns.
    Overstressed,
    /// Incompatible generator speeds or gear-ratio loop.
    Conflict,
}

/// Solved state of one connected component.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ComponentStats {
    pub capacity: u64,
    pub impact: u64,
    pub status: KineticStatus,
    /// Component base speed in fixed-point RPM: node rpm = `base * scale`.
    pub base_rpm: i64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KineticError {
    MissingNode(u32),
    SelfEdge,
    DuplicateEdge,
    MissingEdge,
    InvalidRatio,
}

/// Contiguous SoA kinetic network: shafts, cogwheels, gearboxes,
/// waterwheels, windmills, steam engines, clutches, and stress consumers in
/// parallel columns, linked by signed fixed-point gear-ratio edges.
///
/// Every mutation re-solves only the touched connected component with
/// reusable scratch buffers (generation-stamped marks — zero clearing and
/// zero heap allocation on the steady-state path). While topology and
/// generator RPMs are unchanged, [`tick`](Self::tick) visits zero nodes.
pub struct KineticNetworkPool {
    kinds: Vec<KineticKind>,
    alive: Vec<bool>,
    /// Clutch engagement — a disengaged clutch splits its component.
    engaged: Vec<bool>,
    /// Driven generator speed in fixed-point RPM (0 ⇒ passive node).
    drive_rpm: Vec<i64>,
    capacity: Vec<u64>,
    impact: Vec<u64>,
    /// Per-node adjacency `(neighbor, ratio_fp, forward)` sorted by
    /// neighbor id. `forward` records the `connect(a→b)` orientation:
    /// traversing a `forward` edge scales by `r`, its reverse by `1/r`.
    edges: Vec<Vec<(u32, i64, bool)>>,
    /// Solved speed in fixed-point RPM (0 unless `status == Running`).
    rpm: Vec<i64>,
    status: Vec<KineticStatus>,
    /// Component id = minimum member node id — canonical under any order.
    component: Vec<u32>,
    comps: BTreeMap<u32, ComponentStats>,
    free: Vec<u32>,
    alive_count: usize,
    // Reusable scratch buffers — zero steady-state heap allocation.
    queue: Vec<u32>,
    members: Vec<u32>,
    scales: Vec<i64>,
    mark: Vec<u32>,
    gen: u32,
    resolved: Vec<u32>,
    resolved_gen: u32,
    dirty: Vec<u32>,
    last_tick_visited: u32,
    total_visited: u64,
}

impl Default for KineticNetworkPool {
    fn default() -> Self {
        Self::new()
    }
}

impl KineticNetworkPool {
    pub fn new() -> Self {
        Self {
            kinds: Vec::new(),
            alive: Vec::new(),
            engaged: Vec::new(),
            drive_rpm: Vec::new(),
            capacity: Vec::new(),
            impact: Vec::new(),
            edges: Vec::new(),
            rpm: Vec::new(),
            status: Vec::new(),
            component: Vec::new(),
            comps: BTreeMap::new(),
            free: Vec::new(),
            alive_count: 0,
            queue: Vec::new(),
            members: Vec::new(),
            scales: Vec::new(),
            mark: Vec::new(),
            gen: 0,
            resolved: Vec::new(),
            resolved_gen: 0,
            dirty: Vec::new(),
            last_tick_visited: 0,
            total_visited: 0,
        }
    }

    /// Live node count (removed nodes leave reusable tombstone slots).
    pub fn node_count(&self) -> usize {
        self.alive_count
    }

    /// Solved speed in fixed-point RPM — 0 while Unpowered, Overstressed,
    /// or in Conflict (machines halt cleanly, Create-style).
    pub fn rpm(&self, id: u32) -> i64 {
        self.rpm.get(id as usize).copied().unwrap_or(0)
    }

    pub fn status(&self, id: u32) -> Option<KineticStatus> {
        self.status.get(id as usize).copied()
    }

    pub fn component_id(&self, id: u32) -> Option<u32> {
        self.component.get(id as usize).copied()
    }

    pub fn is_overstressed(&self, id: u32) -> bool {
        self.status(id) == Some(KineticStatus::Overstressed)
    }

    /// Solved totals for the component containing `id`. `base_rpm` is the
    /// canonical node's solved speed — order-independent by construction.
    pub fn component_stats(&self, id: u32) -> Option<ComponentStats> {
        self.comps.get(self.component.get(id as usize)?).copied()
    }

    /// Nodes visited during the most recent [`tick`](Self::tick).
    pub fn steady_nodes_visited(&self) -> u32 {
        self.last_tick_visited
    }

    /// Cumulative nodes ever solved — edits plus deferred wakes.
    pub fn total_nodes_visited(&self) -> u64 {
        self.total_visited
    }

    /// Append a node. Generators start solved as a Running singleton.
    pub fn add_node(&mut self, kind: KineticKind) -> u32 {
        let id = match self.free.pop() {
            Some(id) => id as usize,
            None => {
                self.kinds.push(kind);
                self.alive.push(false);
                self.engaged.push(true);
                self.drive_rpm.push(0);
                self.capacity.push(0);
                self.impact.push(0);
                self.edges.push(Vec::new());
                self.rpm.push(0);
                self.status.push(KineticStatus::Unpowered);
                self.component.push(0);
                self.scales.push(0);
                self.mark.push(0);
                self.resolved.push(0);
                self.kinds.len() - 1
            }
        };
        self.kinds[id] = kind;
        self.alive[id] = true;
        self.engaged[id] = true;
        self.drive_rpm[id] = kind.default_drive_rpm_fp();
        self.capacity[id] = kind.default_capacity();
        self.impact[id] = kind.default_impact();
        self.edges[id].clear();
        self.rpm[id] = self.drive_rpm[id];
        self.status[id] = if self.drive_rpm[id] != 0 {
            KineticStatus::Running
        } else {
            KineticStatus::Unpowered
        };
        self.component[id] = id as u32;
        self.comps.insert(
            id as u32,
            ComponentStats {
                capacity: self.capacity[id],
                impact: self.impact[id],
                status: self.status[id],
                base_rpm: self.drive_rpm[id],
            },
        );
        self.alive_count += 1;
        id as u32
    }

    /// Remove a node and re-solve each fragment of its old component once.
    /// Returns the number of nodes re-solved (the removed node excluded).
    pub fn remove_node(&mut self, id: u32) -> usize {
        let i = id as usize;
        if i >= self.alive.len() || !self.alive[i] {
            return 0;
        }
        // Every old member is a re-solve candidate; clutch engagement is
        // ignored here because a toggle may have hidden the true frontier.
        self.bfs_mark(id, false);
        let mut seeds = std::mem::take(&mut self.members);
        for k in 0..self.edges[i].len() {
            let n = self.edges[i][k].0 as usize;
            if let Some(pos) = self.edges[n].iter().position(|e| e.0 == id) {
                self.edges[n].remove(pos);
            }
        }
        self.edges[i].clear();
        self.alive[i] = false;
        self.drive_rpm[i] = 0;
        self.capacity[i] = 0;
        self.impact[i] = 0;
        self.rpm[i] = 0;
        self.status[i] = KineticStatus::Unpowered;
        self.comps.remove(&id);
        self.alive_count -= 1;
        self.free.push(id);
        let visits = self.resolve_all(&seeds);
        seeds.clear();
        self.members = seeds;
        visits
    }

    /// `connect(a, b, r)` declares `rpm[b] = rpm[a] * r` (negative `r` is a
    /// gearbox direction flip). Re-solves the merged component; returns the
    /// number of nodes visited.
    pub fn connect(&mut self, a: u32, b: u32, ratio: i64) -> Result<usize, KineticError> {
        self.check_endpoints(a, b)?;
        if ratio == 0 {
            return Err(KineticError::InvalidRatio);
        }
        let (ai, bi) = (a as usize, b as usize);
        if self.edges[ai].iter().any(|e| e.0 == b) {
            return Err(KineticError::DuplicateEdge);
        }
        let ins = |edges: &mut Vec<(u32, i64, bool)>, n, r, fwd| {
            let pos = edges.partition_point(|e| e.0 < n);
            edges.insert(pos, (n, r, fwd));
        };
        ins(&mut self.edges[ai], b, ratio, true);
        ins(&mut self.edges[bi], a, ratio, false);
        self.resolved_gen = self.resolved_gen.wrapping_add(1);
        let mut visits = self.solve_from(a);
        if self.mark[bi] != self.gen {
            visits += self.solve_from(b); // a clutch may still separate them
        }
        Ok(visits)
    }

    /// Remove an edge and re-solve each resulting component once.
    pub fn disconnect(&mut self, a: u32, b: u32) -> Result<usize, KineticError> {
        self.check_endpoints(a, b)?;
        let (ai, bi) = (a as usize, b as usize);
        let remove = |edges: &mut Vec<(u32, i64, bool)>, n| {
            edges
                .iter()
                .position(|e| e.0 == n)
                .map(|pos| edges.remove(pos))
        };
        if remove(&mut self.edges[ai], b).is_none() {
            return Err(KineticError::MissingEdge);
        }
        remove(&mut self.edges[bi], a);
        self.resolved_gen = self.resolved_gen.wrapping_add(1);
        let mut visits = self.solve_from(a);
        if self.mark[bi] != self.gen {
            visits += self.solve_from(b);
        }
        Ok(visits)
    }

    /// Set a generator's driven speed (0 = source off). Re-solves its
    /// component; returns nodes visited.
    pub fn set_generator_rpm(&mut self, id: u32, rpm: i64) -> usize {
        let i = id as usize;
        if i >= self.alive.len() || !self.alive[i] || self.drive_rpm[i] == rpm {
            return 0;
        }
        self.drive_rpm[i] = rpm;
        self.resolved_gen = self.resolved_gen.wrapping_add(1);
        self.solve_from(id)
    }

    /// Toggle a clutch: disengaging splits its component, engaging merges.
    /// Returns nodes re-solved across every affected component.
    pub fn set_clutch(&mut self, id: u32, engaged: bool) -> usize {
        let i = id as usize;
        if i >= self.alive.len() || !self.alive[i] || self.engaged[i] == engaged {
            return 0;
        }
        self.bfs_mark(id, false);
        let mut seeds = std::mem::take(&mut self.members);
        self.engaged[i] = engaged;
        let visits = self.resolve_all(&seeds);
        seeds.clear();
        self.members = seeds;
        visits
    }

    /// Override a node's stress impact (consumers) and re-solve.
    pub fn set_impact(&mut self, id: u32, impact: u64) -> usize {
        let i = id as usize;
        if i >= self.alive.len() || !self.alive[i] || self.impact[i] == impact {
            return 0;
        }
        self.impact[i] = impact;
        self.resolved_gen = self.resolved_gen.wrapping_add(1);
        self.solve_from(id)
    }

    /// Override a node's stress capacity (generators) and re-solve.
    pub fn set_capacity(&mut self, id: u32, capacity: u64) -> usize {
        let i = id as usize;
        if i >= self.alive.len() || !self.alive[i] || self.capacity[i] == capacity {
            return 0;
        }
        self.capacity[i] = capacity;
        self.resolved_gen = self.resolved_gen.wrapping_add(1);
        self.solve_from(id)
    }

    /// Queue a node for re-solve on the next [`tick`](Self::tick) — the
    /// "wake this component" entry point for lazy external edits.
    pub fn mark_dirty(&mut self, id: u32) {
        if (id as usize) < self.alive.len() && self.alive[id as usize] {
            self.dirty.push(id);
        }
    }

    /// Steady-state tick: while topology and generator RPMs are untouched
    /// this visits exactly zero nodes. Dirty seeds wake only their own
    /// connected component, in canonical id order.
    pub fn tick(&mut self) -> u32 {
        self.last_tick_visited = 0;
        if self.dirty.is_empty() {
            return 0;
        }
        let mut seeds = std::mem::take(&mut self.dirty);
        seeds.sort_unstable();
        seeds.dedup();
        let visits = self.resolve_all(&seeds) as u32;
        seeds.clear();
        self.dirty = seeds;
        self.last_tick_visited = visits;
        visits
    }

    fn check_endpoints(&self, a: u32, b: u32) -> Result<(), KineticError> {
        for id in [a, b] {
            if (id as usize) >= self.alive.len() || !self.alive[id as usize] {
                return Err(KineticError::MissingNode(id));
            }
        }
        if a == b {
            return Err(KineticError::SelfEdge);
        }
        Ok(())
    }

    /// Reachability pass filling `members`/`mark`. When `honor_clutches`,
    /// expansion stops at disengaged clutches (they are visited as leaves).
    fn bfs_mark(&mut self, seed: u32, honor_clutches: bool) {
        self.gen = self.gen.wrapping_add(1);
        let gen = self.gen;
        self.queue.clear();
        self.members.clear();
        self.mark[seed as usize] = gen;
        self.queue.push(seed);
        let mut head = 0;
        while head < self.queue.len() {
            let u = self.queue[head] as usize;
            head += 1;
            self.members.push(u as u32);
            if honor_clutches && self.kinds[u] == KineticKind::Clutch && !self.engaged[u] {
                continue;
            }
            for k in 0..self.edges[u].len() {
                let v = self.edges[u][k].0;
                if self.mark[v as usize] != gen {
                    self.mark[v as usize] = gen;
                    self.queue.push(v);
                }
            }
        }
    }

    /// Re-solve every not-yet-solved seed's component once; returns total
    /// nodes visited across all solves.
    fn resolve_all(&mut self, seeds: &[u32]) -> usize {
        self.resolved_gen = self.resolved_gen.wrapping_add(1);
        let g = self.resolved_gen;
        let mut visits = 0;
        for &s in seeds {
            let i = s as usize;
            if !self.alive[i] || self.resolved[i] == g {
                continue;
            }
            visits += self.solve_from(s);
        }
        visits
    }

    /// Solve the connected component containing `seed`: BFS for reach +
    /// cumulative signed speed scales, then sum stress, agree on a base
    /// speed, flag `overstressed = impact > capacity`, and publish rpm,
    /// status, and the canonical component id to every member.
    fn solve_from(&mut self, seed: u32) -> usize {
        self.gen = self.gen.wrapping_add(1);
        let gen = self.gen;
        self.queue.clear();
        self.members.clear();
        self.mark[seed as usize] = gen;
        self.scales[seed as usize] = RPM_FP_ONE;
        self.queue.push(seed);
        let mut head = 0;
        let mut conflict = false;
        while head < self.queue.len() {
            let u = self.queue[head] as usize;
            head += 1;
            self.members.push(u as u32);
            // A disengaged clutch is a leaf: its far side solves separately.
            if self.kinds[u] == KineticKind::Clutch && !self.engaged[u] {
                continue;
            }
            let base = i128::from(self.scales[u]);
            for k in 0..self.edges[u].len() {
                let (v, r, fwd) = self.edges[u][k];
                let (v, r) = (v as usize, i128::from(r));
                // A node already solved this pass (e.g. a disengaged clutch
                // claimed by an earlier component) is a wall, not a member.
                if self.resolved[v] == self.resolved_gen {
                    continue;
                }
                let scale = if fwd {
                    (base * r) >> 16
                } else {
                    (base << 16) / r
                };
                if self.mark[v] == gen {
                    conflict |= self.scales[v] != scale as i64;
                } else {
                    self.mark[v] = gen;
                    self.scales[v] = scale as i64;
                    self.queue.push(v as u32);
                }
            }
        }

        let cid = self.members.iter().copied().min().unwrap_or(seed);
        let resolved_gen = self.resolved_gen;
        let mut capacity = 0u64;
        let mut impact = 0u64;
        let mut drive_base: Option<i64> = None;
        for &m in &self.members {
            let mi = m as usize;
            capacity += self.capacity[mi];
            impact += self.impact[mi];
            if self.drive_rpm[mi] != 0 {
                let base = (i128::from(self.drive_rpm[mi]) * i128::from(RPM_FP_ONE)
                    / i128::from(self.scales[mi])) as i64;
                match drive_base {
                    None => drive_base = Some(base),
                    Some(prev) => conflict |= prev != base,
                }
            }
        }
        let status = if conflict {
            KineticStatus::Conflict
        } else if drive_base.is_none() {
            KineticStatus::Unpowered
        } else if impact > capacity {
            KineticStatus::Overstressed
        } else {
            KineticStatus::Running
        };
        // `base_rpm` is published as the solved speed of the canonical node
        // (minimum member id) — identical under any solve seed orientation.
        let base_rpm = drive_base.unwrap_or(0);
        let mut canonical_rpm = 0i64;
        for &m in &self.members {
            let mi = m as usize;
            self.rpm[mi] = if status == KineticStatus::Running {
                ((i128::from(base_rpm) * i128::from(self.scales[mi])) >> 16) as i64
            } else {
                0
            };
            if m == cid {
                canonical_rpm = self.rpm[mi];
            }
            self.status[mi] = status;
            self.component[mi] = cid;
            self.resolved[mi] = resolved_gen;
        }
        self.comps.insert(
            cid,
            ComponentStats {
                capacity,
                impact,
                status,
                base_rpm: canonical_rpm,
            },
        );
        let visited = self.members.len();
        self.total_visited += visited as u64;
        visited
    }
}

// ===========================================================================
// 3. Sleep/wake Create fluid-pipe + machine scheduler
// ===========================================================================

/// Machine families tracked by the active set.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MachineKind {
    MechanicalPress,
    MechanicalMixer,
    EncasedFan,
    CrushingWheels,
    FluidPump,
    Deployer,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SchedError {
    MissingMachine(u32),
    TimeWentBackwards { current: u64, requested: u64 },
}

/// Read-only view of one machine's counters.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MachineSnapshot {
    pub kind: MachineKind,
    pub input_items: u16,
    pub output_items: u16,
    pub busy: bool,
    pub powered: bool,
    /// Absolute tick the in-flight craft completes (`None` when idle).
    pub done_at: Option<u64>,
}

const NO_EVENT: u64 = u64::MAX;

struct Machine {
    kind: MachineKind,
    powered: bool,
    busy: bool,
    /// Mid-craft freeze from a power loss; `remaining` holds the residue.
    frozen: bool,
    input: u16,
    output: u16,
    in_cap: u16,
    out_cap: u16,
    process_ticks: u16,
    remaining: u16,
    done_at: u64,
    /// Currently queued wake tick (`NO_EVENT` = sleeping).
    event_tick: u64,
    /// Downstream belt/pipe targets woken when a craft completes.
    links: Vec<u32>,
}

/// Contiguous active/sleeping machine set driving presses, mixers, fans,
/// crushing wheels, fluid pumps, and deployers. Idle machines — empty input
/// or full output — sleep at zero visits per tick; every wake is an event
/// in a `BTreeSet<(tick, id)>`, so processing order is canonical and
/// digests are bit-identical for any worker partition.
pub struct ActiveFactoryScheduler {
    machines: Vec<Machine>,
    events: BTreeSet<(u64, u32)>,
    now: u64,
    last_visits: u32,
    total_visits: u64,
}

impl Default for ActiveFactoryScheduler {
    fn default() -> Self {
        Self::new()
    }
}

impl ActiveFactoryScheduler {
    pub fn new() -> Self {
        Self {
            machines: Vec::new(),
            events: BTreeSet::new(),
            now: 0,
            last_visits: 0,
            total_visits: 0,
        }
    }

    pub fn machine_count(&self) -> usize {
        self.machines.len()
    }

    /// Machines holding a queued wake (the active set).
    pub fn awake_machines(&self) -> usize {
        self.events.len()
    }

    /// Machines with no queued wake (the sleeping set).
    pub fn sleeping_machines(&self) -> usize {
        self.machines.len() - self.events.len()
    }

    /// Machines visited during the most recent [`run_to`](Self::run_to).
    pub fn last_visits(&self) -> u32 {
        self.last_visits
    }

    pub fn total_visits(&self) -> u64 {
        self.total_visits
    }

    pub fn now(&self) -> u64 {
        self.now
    }

    /// Register a machine; returns its contiguous slot id.
    pub fn add_machine(
        &mut self,
        kind: MachineKind,
        in_cap: u16,
        out_cap: u16,
        process_ticks: u16,
        powered: bool,
    ) -> u32 {
        let id = self.machines.len() as u32;
        self.machines.push(Machine {
            kind,
            powered,
            busy: false,
            frozen: false,
            input: 0,
            output: 0,
            in_cap,
            out_cap,
            process_ticks: process_ticks.max(1),
            remaining: 0,
            done_at: 0,
            event_tick: NO_EVENT,
            links: Vec::new(),
        });
        id
    }

    /// Wire `from` → `to`: completing a craft on `from` wakes `to` (the
    /// belt/funnel/pipe event that pulls its product downstream).
    pub fn link(&mut self, from: u32, to: u32) {
        if let Some(m) = self.machines.get_mut(from as usize) {
            m.links.push(to);
        }
    }

    /// Funnel handoff: feed up to `count` items into `id`'s input slot and
    /// wake it. Returns the accepted count — strict conservation.
    pub fn push_input(&mut self, id: u32, count: u16) -> Result<u16, SchedError> {
        let m = self
            .machines
            .get_mut(id as usize)
            .ok_or(SchedError::MissingMachine(id))?;
        let accepted = count.min(m.in_cap - m.input);
        m.input += accepted;
        self.schedule(id, Some(self.now));
        Ok(accepted)
    }

    /// Chute handoff: pull up to `count` items out of `id`'s output slot
    /// and wake it (a freed slot may release a held product). Returns the
    /// removed count.
    pub fn pop_output(&mut self, id: u32, count: u16) -> Result<u16, SchedError> {
        let m = self
            .machines
            .get_mut(id as usize)
            .ok_or(SchedError::MissingMachine(id))?;
        let removed = count.min(m.output);
        m.output -= removed;
        self.schedule(id, Some(self.now));
        Ok(removed)
    }

    /// Power (kinetic stress) edge: losing power freezes a craft mid-flight;
    /// regaining it resumes at the remaining duration.
    pub fn set_powered(&mut self, id: u32, powered: bool) -> Result<(), SchedError> {
        let m = self
            .machines
            .get_mut(id as usize)
            .ok_or(SchedError::MissingMachine(id))?;
        m.powered = powered;
        self.schedule(id, Some(self.now));
        Ok(())
    }

    /// Generic belt/funnel/pipe event wake.
    pub fn poke(&mut self, id: u32) {
        if (id as usize) < self.machines.len() {
            self.schedule(id, Some(self.now));
        }
    }

    /// Advance to `tick`, processing every due wake in `(tick, id)` order.
    /// Events evaluate at their own scheduled tick — a long catch-up jump
    /// is bit-identical to stepping every tick. Returns machines visited.
    pub fn run_to(&mut self, tick: u64) -> Result<u32, SchedError> {
        if tick < self.now {
            return Err(SchedError::TimeWentBackwards {
                current: self.now,
                requested: tick,
            });
        }
        self.last_visits = 0;
        while let Some(&(t, id)) = self.events.first() {
            if t > tick {
                break;
            }
            self.events.remove(&(t, id));
            self.machines[id as usize].event_tick = NO_EVENT;
            self.now = t;
            self.evaluate(id);
            self.last_visits += 1;
        }
        self.now = tick;
        self.total_visits += u64::from(self.last_visits);
        Ok(self.last_visits)
    }

    pub fn snapshot(&self, id: u32) -> Option<MachineSnapshot> {
        let m = self.machines.get(id as usize)?;
        Some(MachineSnapshot {
            kind: m.kind,
            input_items: m.input,
            output_items: m.output,
            busy: m.busy,
            powered: m.powered,
            done_at: if m.busy && m.powered && !m.frozen {
                Some(m.done_at)
            } else {
                None
            },
        })
    }

    /// Deterministic FNV-1a digest over every machine counter plus the wake
    /// queue — bit-identical regardless of how the due work was partitioned.
    pub fn digest(&self) -> u64 {
        let mut h = 0xcbf2_9ce4_8422_2325u64;
        let mut mix = |v: u64| {
            for b in v.to_le_bytes() {
                h = (h ^ u64::from(b)).wrapping_mul(0x0000_0100_0000_01b3);
            }
        };
        for m in &self.machines {
            mix(m.kind as u64);
            mix(u64::from(m.input));
            mix(u64::from(m.output));
            mix(u64::from(m.busy));
            mix(u64::from(m.frozen));
            mix(u64::from(m.remaining));
            mix(u64::from(m.powered));
            mix(m.done_at);
            mix(m.event_tick);
        }
        for &(t, id) in &self.events {
            mix(t);
            mix(u64::from(id));
        }
        h
    }

    fn schedule(&mut self, id: u32, at: Option<u64>) {
        let m = &mut self.machines[id as usize];
        let next = at.unwrap_or(NO_EVENT);
        if m.event_tick == next {
            return;
        }
        if m.event_tick != NO_EVENT {
            self.events.remove(&(m.event_tick, id));
        }
        if next != NO_EVENT {
            self.events.insert((next, id));
        }
        self.machines[id as usize].event_tick = next;
    }

    /// One wake evaluation: complete a due craft (unless output is full —
    /// the product is held until a `pop_output` poke frees the slot), start
    /// the next craft if powered and supplied, freeze on power loss, then
    /// reschedule the single next wake or park the machine at zero visits.
    fn evaluate(&mut self, id: u32) {
        let now = self.now;
        let mut produced = false;
        {
            let m = &mut self.machines[id as usize];
            if m.busy && m.powered {
                if m.frozen {
                    m.done_at = now + u64::from(m.remaining);
                    m.frozen = false;
                }
                if now >= m.done_at && m.output < m.out_cap {
                    m.output += 1;
                    m.busy = false;
                    produced = true;
                }
            }
            if !m.busy && m.powered && m.input > 0 && m.output < m.out_cap {
                m.input -= 1;
                m.busy = true;
                m.frozen = false;
                m.remaining = m.process_ticks;
                m.done_at = now + u64::from(m.process_ticks);
            }
            if m.busy && !m.powered && !m.frozen {
                m.remaining = m.done_at.saturating_sub(now).min(u64::from(u16::MAX)) as u16;
                m.frozen = true;
            }
        }
        // Still busy & powered & unfinished → wake at done_at; due-but-
        // blocked (output full) → no wake, a pop_output poke releases it.
        let wake = {
            let m = &self.machines[id as usize];
            if m.busy && m.powered && !m.frozen && m.done_at > now {
                Some(m.done_at)
            } else {
                None
            }
        };
        self.schedule(id, wake);
        if produced {
            for k in 0..self.machines[id as usize].links.len() {
                let target = self.machines[id as usize].links[k];
                self.schedule(target, Some(now));
            }
        }
    }
}
