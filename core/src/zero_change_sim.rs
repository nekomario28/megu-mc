// SPDX-License-Identifier: LGPL-3.0-or-later
//! `megu_core::zero_change_sim` — zero-allocation, zero-behavior-change
//! simulation kernels for the entity-heavy per-tick stages.
//!
//! Every kernel in this module is a drop-in accelerator for an existing
//! vanilla-parity routine: the output is **bit-identical** to the naive
//! reference implementation on every input, while the steady-state cost is a
//! flat reusable scratch footprint instead of a per-call allocation storm.
//!
//! * [`PathFinderScratch`] — the land-mob A* engine minus the allocation
//!   profile.  The reference walk spends every call growing a `BTreeMap`
//!   node table, a `BTreeSet` expanded set and a `BinaryHeap` frontier; this
//!   engine replaces all three with a reusable open-addressed record table
//!   invalidated by a `u32` generation stamp plus a re-warmed binary heap.
//!   Node expansion order — `(f_cost, h_cost, position)` — is preserved
//!   verbatim, so the chosen path, the `closest` fallback and the
//!   `reached` flag are bit-exact.  On top of the engine sit two certified
//!   shortcuts: a straight-line fast path and a [`LosCache`] line-of-sight
//!   memo.  The fast path is **not** an approximation: a candidate chain is
//!   only returned when the engine can *prove* it equals the A* result —
//!   every link's parent must be the unique cheapest predecessor (all
//!   alternative parents eliminated by a grid-distance lower bound), and a
//!   closed-form frontier bound must show the target pops inside
//!   `max_visited_nodes`.  Otherwise the full A* runs and the answer is
//!   still exact.
//! * [`ReusableCellMerger`] — the per-tick XP-orb and dropped-item merge
//!   kernels over pooled spatial cells.  A chained open-addressed cell
//!   table, a candidate scratch and a generation-stamped `eaten` bitset are
//!   reused every tick; ascending-index candidate order, the
//!   `keep = older, eat = younger` age tie-break and the merged-eid output
//!   order match the pairwise reference scan exactly — including the dead
//!   orbs that stay merge candidates until the final `retain`.  For item
//!   merges a full-stack pre-filter drops stacks that can never pair
//!   (`count >= stack cap`, which is inert in both merge directions unless
//!   a `count == 0` stack exists) so a floor of finished stacks costs ~0
//!   merge work.
//! * [`RestingEntitySleepState`] — exact-tick sleep for resting items, XP
//!   orbs and lodged projectiles.  An entity only sleeps on the concrete
//!   condition (`on_ground && vel == 0 && !in_fluid`) and wakes on the
//!   *exact* tick a watched section revision changes — floor break under
//!   the feet block, fluid arrival in any neighbouring cell, or an impulse
//!   landing non-zero velocity — while age, despawn and pickup checks keep
//!   running untouched.  Sleeping skips every block-collision and
//!   fluid-drag query, which is the entire point: the only per-tick cost
//!   left is a handful of revision-counter reads.

#![forbid(unsafe_code)]

use std::collections::BinaryHeap;

/// Integer block-space coordinate used by every kernel in this module.
pub type BlockPos = (i32, i32, i32);

// ===========================================================================
// Path world contract (the same surface the live PathWorld adapter answers).
// ===========================================================================

/// Path types a walk evaluator can classify a cell as, mirroring the live
/// navigation taxonomy.  Only the malus sign and the `DoorOpen` pass-through
/// affect the kernels below; the rest round-trips for parity checks.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum BlockPathType {
    Blocked,
    Open,
    Walkable,
    WalkableDoor,
    Trapdoor,
    PowderSnow,
    DangerPowdersnow,
    Fence,
    Lava,
    Water,
    WaterBorder,
    Rail,
    UnpassableRail,
    DangerFire,
    DamageFire,
    DangerOther,
    DamageOther,
    DoorOpen,
    DoorWoodClosed,
    DoorIronClosed,
    Breach,
    Leaves,
    StickyHoney,
    Cocoa,
    DamageCautious,
    DangerTrapdoor,
}

/// Default path-type malus; identical to the live table.
pub fn default_malus(path_type: BlockPathType) -> f32 {
    use BlockPathType::*;
    match path_type {
        Blocked | PowderSnow | Fence | Lava | UnpassableRail | DamageOther | DoorWoodClosed
        | DoorIronClosed | Leaves => -1.0,
        Water | WaterBorder | DangerFire | DangerOther | StickyHoney => 8.0,
        DamageFire => 16.0,
        Breach => 4.0,
        Open | Walkable | WalkableDoor | Trapdoor | DangerPowdersnow | Rail | DoorOpen | Cocoa
        | DamageCautious | DangerTrapdoor => 0.0,
    }
}

/// The block queries a walk evaluator needs, identical in shape to the live
/// `PathWorld` surface so adapters can forward verbatim.
pub trait PathWorld {
    fn path_type(&self, x: i32, y: i32, z: i32) -> BlockPathType;
    fn is_solid(&self, x: i32, y: i32, z: i32) -> bool;
    fn collision_height(&self, x: i32, y: i32, z: i32) -> f64;
    fn min_y(&self) -> i32;
    fn max_y(&self) -> i32;
}

/// One accepted path node: position, its path type and that type's malus.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PathNode {
    pub x: i32,
    pub y: i32,
    pub z: i32,
    pub path_type: BlockPathType,
    pub malus: f32,
}

impl PathNode {
    #[inline]
    pub fn pos(&self) -> BlockPos {
        (self.x, self.y, self.z)
    }
}

/// The land-mob node evaluator: mob body clearance, ground support, legal
/// horizontal / climb / bounded-fall steps.  Ported verbatim so every
/// predicate agrees with the live implementation bit-for-bit.
#[derive(Clone, Copy, Debug)]
pub struct NodeEvaluator {
    pub mob_height: f64,
    pub max_fall_distance: i32,
}

impl Default for NodeEvaluator {
    fn default() -> Self {
        Self {
            mob_height: 1.8,
            max_fall_distance: 3,
        }
    }
}

impl NodeEvaluator {
    fn height_in_blocks(&self) -> i32 {
        self.mob_height.ceil().max(1.0) as i32
    }

    /// A clear body requires each occupied block to be passable.  The node's
    /// path type comes from the feet position.
    pub fn clear_body(&self, world: &dyn PathWorld, pos: BlockPos) -> bool {
        let (x, y, z) = pos;
        let Some(top) = y.checked_add(self.height_in_blocks() - 1) else {
            return false;
        };
        if y < world.min_y() || top > world.max_y() {
            return false;
        }
        for block_y in y..=top {
            let path_type = world.path_type(x, block_y, z);
            // Ground navigation can pass the swung panel of an open door.
            if path_type == BlockPathType::DoorOpen {
                continue;
            }
            if default_malus(path_type) < 0.0
                || world.is_solid(x, block_y, z)
                || world.collision_height(x, block_y, z) > 0.0
            {
                return false;
            }
        }
        true
    }

    pub fn has_support(&self, world: &dyn PathWorld, pos: BlockPos) -> bool {
        let (x, y, z) = pos;
        if y <= world.min_y() {
            return false;
        }
        let below = y - 1;
        world.is_solid(x, below, z)
            || world.collision_height(x, below, z) > 0.0
            || matches!(world.path_type(x, below, z), BlockPathType::Water)
    }

    pub fn standable(&self, world: &dyn PathWorld, pos: BlockPos) -> bool {
        self.clear_body(world, pos) && self.has_support(world, pos)
    }

    pub fn accepted_node(&self, world: &dyn PathWorld, pos: BlockPos) -> PathNode {
        let path_type = world.path_type(pos.0, pos.1, pos.2);
        PathNode {
            x: pos.0,
            y: pos.1,
            z: pos.2,
            path_type,
            malus: default_malus(path_type),
        }
    }

    /// The single-direction step evaluation used by the reference walker:
    /// same-level stand, one-block climb, then a bounded safe fall.
    pub fn neighbor_at(
        &self,
        world: &dyn PathWorld,
        from: BlockPos,
        dx: i32,
        dz: i32,
    ) -> Option<PathNode> {
        let x = from.0.checked_add(dx)?;
        let z = from.2.checked_add(dz)?;
        if self.standable(world, (x, from.1, z)) {
            return Some(self.accepted_node(world, (x, from.1, z)));
        }

        let up = from.1.checked_add(1)?;
        if self.clear_body(world, (from.0, up, from.2)) && self.standable(world, (x, up, z)) {
            return Some(self.accepted_node(world, (x, up, z)));
        }

        // A fall begins through air or fluid, never through a blocked cell.
        if !self.clear_body(world, (x, from.1, z)) {
            return None;
        }
        for drop in 1..=self.max_fall_distance {
            let y = from.1.checked_sub(drop)?;
            if !self.clear_body(world, (x, y, z)) {
                return None;
            }
            if self.standable(world, (x, y, z)) {
                return Some(self.accepted_node(world, (x, y, z)));
            }
        }
        None
    }

    /// Cardinal steps first, then diagonals whose two cardinal sides stand at
    /// the diagonal's elevation — the exact enumeration of the reference
    /// walker, appending to `out` instead of allocating.
    pub fn neighbors_into(&self, world: &dyn PathWorld, from: BlockPos, out: &mut Vec<PathNode>) {
        const CARDINALS: [(i32, i32); 4] = [(-1, 0), (0, -1), (0, 1), (1, 0)];
        const DIAGONALS: [(i32, i32); 4] = [(-1, -1), (-1, 1), (1, -1), (1, 1)];
        for (dx, dz) in CARDINALS {
            if let Some(node) = self.neighbor_at(world, from, dx, dz) {
                out.push(node);
            }
        }
        for (dx, dz) in DIAGONALS {
            let (Some(x_side), Some(z_side)) = (
                self.neighbor_at(world, from, dx, 0),
                self.neighbor_at(world, from, 0, dz),
            ) else {
                continue;
            };
            let Some(diagonal) = self.neighbor_at(world, from, dx, dz) else {
                continue;
            };
            if x_side.y == diagonal.y && z_side.y == diagonal.y {
                out.push(diagonal);
            }
        }
    }

    /// Allocating convenience wrapper matching the reference `neighbors`.
    pub fn neighbors(&self, world: &dyn PathWorld, from: BlockPos) -> Vec<PathNode> {
        let mut out = Vec::with_capacity(8);
        self.neighbors_into(world, from, &mut out);
        out
    }
}

/// Euclidean node distance — the same `f64` arithmetic the reference uses,
/// which is what keeps the frontier ordering and `closest` fallback
/// bit-identical.
#[inline]
fn path_distance(a: BlockPos, b: BlockPos) -> f64 {
    let dx = f64::from(a.0) - f64::from(b.0);
    let dy = f64::from(a.1) - f64::from(b.1);
    let dz = f64::from(a.2) - f64::from(b.2);
    (dx * dx + dy * dy + dz * dz).sqrt()
}

/// Lower bound on the path cost `S -> n`: every step pays at least its
/// horizontal displacement, so the cheapest 8-connected horizontal cover is a
/// valid bound for `g_min(n)` in any world (obstacles only raise the real
/// cost).  `min(|dx|,|dz|)` diagonal steps plus the remainder.
#[inline]
fn grid_bound(s: BlockPos, n: BlockPos) -> f64 {
    let dx = (s.0 - n.0).unsigned_abs();
    let dz = (s.2 - n.2).unsigned_abs();
    let (lo, hi) = (dx.min(dz), dx.max(dz));
    lo as f64 * std::f64::consts::SQRT_2 + (hi - lo) as f64
}

/// The path result shared by the scratch finder: borrowed node storage so a
/// hot tick never allocates.
pub struct SimPath<'a> {
    pub nodes: &'a [PathNode],
    pub target: BlockPos,
    pub reached: bool,
}

/// Frontier queue entry — ordered exactly like the reference: `BinaryHeap`
/// pops the largest entry, so every comparison is reversed into a minimum
/// heap keyed `(f_cost, h_cost, position)`.
#[derive(Clone, Copy, Debug)]
struct QueueEntry {
    position: BlockPos,
    f_cost: f64,
    h_cost: f64,
    g_cost: f64,
}

impl PartialEq for QueueEntry {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == std::cmp::Ordering::Equal
    }
}
impl Eq for QueueEntry {}
impl PartialOrd for QueueEntry {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for QueueEntry {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        other
            .f_cost
            .total_cmp(&self.f_cost)
            .then_with(|| other.h_cost.total_cmp(&self.h_cost))
            .then_with(|| other.position.cmp(&self.position))
    }
}

// ---------------------------------------------------------------------------
// Flat generation-stamped record table.
// ---------------------------------------------------------------------------

/// Open-addressed `BlockPos -> search record` table replacing the reference
/// `BTreeMap` + `BTreeSet`.  A `u32` generation stamp invalidates stale slots
/// for free between calls; `expanded` rides the record since the reference
/// closed set is always a subset of `records`.
struct RecordTable {
    pos: Vec<BlockPos>,
    gen: Vec<u32>,
    g: Vec<f64>,
    parent: Vec<BlockPos>,
    /// bit0: has_parent, bit1: expanded.
    flags: Vec<u8>,
    mask: usize,
    live: usize,
    cur_gen: u32,
}

impl RecordTable {
    const FLAG_PARENT: u8 = 1;
    const FLAG_EXPANDED: u8 = 2;

    fn new() -> Self {
        Self {
            pos: Vec::new(),
            gen: Vec::new(),
            g: Vec::new(),
            parent: Vec::new(),
            flags: Vec::new(),
            mask: 0,
            live: 0,
            cur_gen: 0,
        }
    }

    /// Advance the generation; stale slots stop existing without a clear
    /// pass.  A wrap-around physically clears once per `u32::MAX` searches.
    fn begin(&mut self) {
        self.cur_gen = self.cur_gen.wrapping_add(1);
        if self.cur_gen == 0 {
            self.gen.fill(0);
            self.cur_gen = 1;
        }
        self.live = 0;
    }

    #[inline]
    fn slot(pos: BlockPos) -> u32 {
        let h = (pos.0 as u32)
            .wrapping_mul(0x9E37_79B1)
            .wrapping_add((pos.1 as u32).wrapping_mul(0x85EB_CA77))
            .wrapping_add((pos.2 as u32).wrapping_mul(0xC2B2_AE3D));
        h ^ (h >> 15) ^ (h >> 7)
    }

    #[inline]
    fn probe(&self, pos: BlockPos) -> usize {
        let mut at = (Self::slot(pos) as usize) & self.mask;
        loop {
            if self.gen[at] != self.cur_gen || self.pos[at] == pos {
                return at;
            }
            at = (at + 1) & self.mask;
        }
    }

    /// Number of live records the table can hold before a grow is required.
    /// Probing correctness relies on always having at least one empty slot.
    fn grow_if_needed(&mut self) {
        let cap = self.pos.len();
        if cap > 0 && self.live * 4 < cap * 3 {
            return;
        }
        let new_cap = (cap * 2).max(1024);
        let mut fresh = RecordTable {
            pos: vec![(0, 0, 0); new_cap],
            gen: vec![0; new_cap],
            g: vec![0.0; new_cap],
            parent: vec![(0, 0, 0); new_cap],
            flags: vec![0; new_cap],
            mask: new_cap - 1,
            live: 0,
            cur_gen: self.cur_gen,
        };
        for at in 0..cap {
            if self.gen[at] == self.cur_gen {
                let to = fresh.probe(self.pos[at]);
                fresh.pos[to] = self.pos[at];
                fresh.gen[to] = self.cur_gen;
                fresh.g[to] = self.g[at];
                fresh.parent[to] = self.parent[at];
                fresh.flags[to] = self.flags[at];
                fresh.live += 1;
            }
        }
        self.live = fresh.live;
        *self = fresh;
    }

    /// Record `pos` with no parent (the search start).
    fn seed(&mut self, pos: BlockPos) {
        self.grow_if_needed();
        let at = self.probe(pos);
        self.gen[at] = self.cur_gen;
        self.pos[at] = pos;
        self.flags[at] = 0;
        self.g[at] = 0.0;
        self.parent[at] = (0, 0, 0);
        self.live += 1;
    }

    /// The pop-time check fused into one probe: returns the recorded g and
    /// marks the node expanded when the entry is current (`entry_g <= g`);
    /// `None` for stale entries and already-expanded nodes.
    #[inline]
    fn try_expand(&mut self, pos: BlockPos, entry_g: f64) -> Option<f64> {
        let at = self.probe(pos);
        if self.gen[at] != self.cur_gen
            || self.flags[at] & Self::FLAG_EXPANDED != 0
            || entry_g > self.g[at]
        {
            return None;
        }
        self.flags[at] |= Self::FLAG_EXPANDED;
        Some(self.g[at])
    }

    /// Neighbor-offer update fused into one probe: skips expanded nodes and
    /// non-improving offers, otherwise records `cost`/`parent` and returns
    /// `true` so the caller pushes a fresh queue entry.
    #[inline]
    fn offer(&mut self, pos: BlockPos, cost: f64, parent: BlockPos) -> bool {
        self.grow_if_needed();
        let at = self.probe(pos);
        if self.gen[at] == self.cur_gen {
            if self.flags[at] & Self::FLAG_EXPANDED != 0 || cost >= self.g[at] {
                return false;
            }
        } else {
            self.gen[at] = self.cur_gen;
            self.pos[at] = pos;
            self.flags[at] = 0;
            self.live += 1;
        }
        self.g[at] = cost;
        self.parent[at] = parent;
        self.flags[at] = Self::FLAG_PARENT;
        true
    }

    #[inline]
    fn parent_of(&self, pos: BlockPos) -> Option<BlockPos> {
        let at = self.probe(pos);
        (self.gen[at] == self.cur_gen && self.flags[at] & Self::FLAG_PARENT != 0)
            .then(|| self.parent[at])
    }
}

/// Generation-stamped memo of per-cell `standable`/`clear_body` verdicts.
/// Both predicates are pure over a stable world for the duration of one
/// `find_path` call, so memoizing them changes nothing but the number of
/// world probes — one flat probe replaces up to nine `path_type` /
/// `is_solid` / `collision_height` lookups per repeat query.
struct VerdictCache {
    pos: Vec<BlockPos>,
    gen: Vec<u32>,
    /// bit0: standable computed, bit1: standable value,
    /// bit2: clear-body computed, bit3: clear-body value.
    val: Vec<u8>,
    mask: usize,
    live: usize,
    cur_gen: u32,
}

impl VerdictCache {
    const STAND_KNOWN: u8 = 1;
    const STAND_YES: u8 = 2;
    const CLEAR_KNOWN: u8 = 4;
    const CLEAR_YES: u8 = 8;

    fn new() -> Self {
        Self {
            pos: Vec::new(),
            gen: Vec::new(),
            val: Vec::new(),
            mask: 0,
            live: 0,
            cur_gen: 0,
        }
    }

    fn begin(&mut self) {
        self.cur_gen = self.cur_gen.wrapping_add(1);
        if self.cur_gen == 0 {
            self.gen.fill(0);
            self.cur_gen = 1;
        }
        self.live = 0;
    }

    #[inline]
    fn probe(&self, pos: BlockPos) -> usize {
        let mut at = (RecordTable::slot(pos) as usize) & self.mask;
        loop {
            if self.gen[at] != self.cur_gen || self.pos[at] == pos {
                return at;
            }
            at = (at + 1) & self.mask;
        }
    }

    fn grow_if_needed(&mut self) {
        let cap = self.pos.len();
        if cap > 0 && self.live * 4 < cap * 3 {
            return;
        }
        let new_cap = (cap * 2).max(1024);
        let mut pos = vec![(0, 0, 0); new_cap];
        let mut gen = vec![0; new_cap];
        let mut val = vec![0u8; new_cap];
        let mask = new_cap - 1;
        for at in 0..cap {
            if self.gen[at] == self.cur_gen {
                let p = self.pos[at];
                let mut to = (RecordTable::slot(p) as usize) & mask;
                while gen[to] == self.cur_gen {
                    to = (to + 1) & mask;
                }
                pos[to] = p;
                gen[to] = self.cur_gen;
                val[to] = self.val[at];
            }
        }
        self.pos = pos;
        self.gen = gen;
        self.val = val;
        self.mask = mask;
    }

    /// Look up `pos` or claim its slot for a fill.
    #[inline]
    fn entry(&mut self, pos: BlockPos) -> usize {
        self.grow_if_needed();
        let at = self.probe(pos);
        if self.gen[at] != self.cur_gen {
            self.gen[at] = self.cur_gen;
            self.pos[at] = pos;
            self.val[at] = 0;
            self.live += 1;
        }
        at
    }

    #[inline]
    fn get(&self, at: usize, known: u8, yes: u8) -> Option<bool> {
        let v = self.val[at];
        (v & known != 0).then_some(v & yes != 0)
    }

    #[inline]
    fn set(&mut self, at: usize, known: u8, yes: u8, verdict: bool) {
        self.val[at] |= known;
        if verdict {
            self.val[at] |= yes;
        }
    }
}

/// Open-addressed position set with the same generation-stamp trick, used by
/// the certified fast-path walk to refuse revisiting a node.
struct PosSet {
    pos: Vec<BlockPos>,
    gen: Vec<u32>,
    mask: usize,
    live: usize,
    cur_gen: u32,
}

impl PosSet {
    fn new() -> Self {
        Self {
            pos: Vec::new(),
            gen: Vec::new(),
            mask: 0,
            live: 0,
            cur_gen: 0,
        }
    }

    fn begin(&mut self) {
        self.cur_gen = self.cur_gen.wrapping_add(1);
        if self.cur_gen == 0 {
            self.gen.fill(0);
            self.cur_gen = 1;
        }
        self.live = 0;
    }

    #[inline]
    fn probe(&self, pos: BlockPos) -> usize {
        let mut at = (RecordTable::slot(pos) as usize) & self.mask;
        loop {
            if self.gen[at] != self.cur_gen || self.pos[at] == pos {
                return at;
            }
            at = (at + 1) & self.mask;
        }
    }

    fn grow_if_needed(&mut self) {
        let cap = self.pos.len();
        if cap > 0 && self.live * 4 < cap * 3 {
            return;
        }
        let new_cap = (cap * 2).max(64);
        let mut pos = vec![(0, 0, 0); new_cap];
        let mut gen = vec![0; new_cap];
        let fresh_mask = new_cap - 1;
        for at in 0..cap {
            if self.gen[at] == self.cur_gen {
                let p = self.pos[at];
                let mut to = (RecordTable::slot(p) as usize) & fresh_mask;
                while gen[to] == self.cur_gen {
                    to = (to + 1) & fresh_mask;
                }
                pos[to] = p;
                gen[to] = self.cur_gen;
            }
        }
        self.pos = pos;
        self.gen = gen;
        self.mask = fresh_mask;
    }

    #[inline]
    fn contains(&self, pos: BlockPos) -> bool {
        self.mask != 0 && self.gen[self.probe(pos)] == self.cur_gen
    }

    fn insert(&mut self, pos: BlockPos) {
        self.grow_if_needed();
        let at = self.probe(pos);
        if self.gen[at] != self.cur_gen {
            self.gen[at] = self.cur_gen;
            self.pos[at] = pos;
            self.live += 1;
        }
    }
}

// ---------------------------------------------------------------------------
// LosCache: bounded ring of "straight corridor still clear" verdicts.
// ---------------------------------------------------------------------------

/// A bounded ring of line-of-sight verdicts keyed
/// `(mob_cell, target_cell, rev) -> clear`.  `rev` is a caller-supplied
/// section-revision hash: while it is unchanged the world cells the corridor
/// crosses are unchanged, so a cached verdict stays valid.  Ring eviction is
/// oldest-first; a stale or evicted entry only ever costs a re-check, never a
/// wrong answer, because the verdict is re-derived from the live world.
pub struct LosCache {
    ring: Vec<LosEntry>,
    head: usize,
    pub probes: u64,
    pub hits: u64,
}

#[derive(Clone, Copy, Default)]
struct LosEntry {
    mob: BlockPos,
    target: BlockPos,
    rev: u64,
    clear: bool,
    occupied: bool,
}

impl LosCache {
    /// A small ring covers the hot mob set; 64 entries is the default scale.
    pub fn new(capacity: usize) -> Self {
        Self {
            ring: vec![LosEntry::default(); capacity.max(1)],
            head: 0,
            probes: 0,
            hits: 0,
        }
    }

    /// The cached verdict for `(mob, target, rev)`, if the ring still holds
    /// it.  Linear scan: the ring is deliberately small.
    pub fn get(&mut self, mob: BlockPos, target: BlockPos, rev: u64) -> Option<bool> {
        self.probes += 1;
        for e in &self.ring {
            if e.occupied && e.mob == mob && e.target == target && e.rev == rev {
                self.hits += 1;
                return Some(e.clear);
            }
        }
        None
    }

    /// Record a verdict, evicting the oldest ring slot.
    pub fn put(&mut self, mob: BlockPos, target: BlockPos, rev: u64, clear: bool) {
        // Prefer a still-empty or identical slot before true eviction.
        let mut free = None;
        for (i, e) in self.ring.iter().enumerate() {
            if !e.occupied || (e.mob == mob && e.target == target) {
                free = Some(i);
                break;
            }
        }
        let at = free.unwrap_or_else(|| {
            let at = self.head;
            self.head = (self.head + 1) % self.ring.len();
            at
        });
        self.ring[at] = LosEntry {
            mob,
            target,
            rev,
            clear,
            occupied: true,
        };
    }

    pub fn len(&self) -> usize {
        self.ring.iter().filter(|e| e.occupied).count()
    }

    pub fn is_empty(&self) -> bool {
        self.ring.iter().all(|e| !e.occupied)
    }
}

// ---------------------------------------------------------------------------
// PathFinderScratch
// ---------------------------------------------------------------------------

/// Zero-allocation A* engine.  All mutable state lives in reusable flat
/// buffers: after a warm-up call a `find_path` performs **zero heap
/// allocations**.
///
/// The search loop is a line-for-line port of the reference `BTreeMap` /
/// `BTreeSet` / `BinaryHeap` walk: identical expansion order, identical
/// `closest` fallback, identical `reached` flag.
pub struct PathFinderScratch {
    pub evaluator: NodeEvaluator,
    pub max_visited_nodes: usize,
    /// Section-rev keyed line-of-sight memo consulted before each fast-path
    /// attempt.  Public so adapters can size and inspect it.
    pub los: LosCache,
    rec: RecordTable,
    seen: PosSet,
    vcache: VerdictCache,
    heap: BinaryHeap<QueueEntry>,
    out: Vec<PathNode>,
    chain: Vec<PathNode>,
    chain_g: Vec<f64>,
    nbuf: Vec<PathNode>,
    /// Set by `run_astar` alongside `out`; read by `find_path` after the
    /// borrow of `out` settles.
    reached_flag: bool,
    /// Master switch for the certified straight-line shortcut.  Defaults on;
    /// when off, `find_path` always runs the full search (still zero-alloc).
    pub fast_path: bool,
    /// Diagnostics for parity audits: how many calls resolved through the
    /// certified fast path vs the full search.
    pub fast_path_hits: u64,
    pub astar_runs: u64,
}

impl Default for PathFinderScratch {
    fn default() -> Self {
        Self::new()
    }
}

impl PathFinderScratch {
    pub fn new() -> Self {
        Self {
            evaluator: NodeEvaluator::default(),
            max_visited_nodes: 1024,
            los: LosCache::new(512),
            rec: RecordTable::new(),
            seen: PosSet::new(),
            vcache: VerdictCache::new(),
            heap: BinaryHeap::with_capacity(1024),
            out: Vec::new(),
            chain: Vec::new(),
            chain_g: Vec::new(),
            nbuf: Vec::with_capacity(8),
            reached_flag: false,
            fast_path: true,
            fast_path_hits: 0,
            astar_runs: 0,
        }
    }

    /// Find a path identical to the reference `find_path`: same node list,
    /// same `target`, same `reached`.  `los_rev` is the section-revision hash
    /// the caller associates with the world cells the corridor can touch;
    /// pass any stable value (e.g. `0`) when revisions are not tracked —
    /// correctness never depends on it, only the cache hit rate does.
    pub fn find_path<'a>(
        &'a mut self,
        world: &dyn PathWorld,
        start: BlockPos,
        target: BlockPos,
        los_rev: u64,
    ) -> SimPath<'a> {
        self.astar_runs += 1;
        self.out.clear();
        self.vcache.begin();
        if !self.s_standable(world, start) {
            return SimPath {
                nodes: &self.out,
                target,
                reached: false,
            };
        }
        if self.fast_path {
            if let Some(len) = self.try_fast_path(world, start, target, los_rev) {
                self.fast_path_hits += 1;
                return SimPath {
                    nodes: &self.chain[..len],
                    target,
                    reached: true,
                };
            }
        }
        self.run_astar(world, start, target);
        SimPath {
            nodes: &self.out,
            target,
            reached: self.reached_flag,
        }
    }

    /// Memoized `standable`: identical verdict, one flat probe instead of up
    /// to nine world lookups on repeats.
    #[inline]
    fn s_standable(&mut self, world: &dyn PathWorld, pos: BlockPos) -> bool {
        let at = self.vcache.entry(pos);
        if let Some(v) = self
            .vcache
            .get(at, VerdictCache::STAND_KNOWN, VerdictCache::STAND_YES)
        {
            return v;
        }
        let v = self.evaluator.standable(world, pos);
        self.vcache
            .set(at, VerdictCache::STAND_KNOWN, VerdictCache::STAND_YES, v);
        v
    }

    /// Memoized `clear_body`.
    #[inline]
    fn s_clear(&mut self, world: &dyn PathWorld, pos: BlockPos) -> bool {
        let at = self.vcache.entry(pos);
        if let Some(v) = self
            .vcache
            .get(at, VerdictCache::CLEAR_KNOWN, VerdictCache::CLEAR_YES)
        {
            return v;
        }
        let v = self.evaluator.clear_body(world, pos);
        self.vcache
            .set(at, VerdictCache::CLEAR_KNOWN, VerdictCache::CLEAR_YES, v);
        v
    }

    /// `NodeEvaluator::neighbor_at` routed through the verdict memo —
    /// identical accepted nodes, a fraction of the world probes.
    fn neighbor_at_cached(
        &mut self,
        world: &dyn PathWorld,
        from: BlockPos,
        dx: i32,
        dz: i32,
    ) -> Option<PathNode> {
        let x = from.0.checked_add(dx)?;
        let z = from.2.checked_add(dz)?;
        if self.s_standable(world, (x, from.1, z)) {
            return Some(self.evaluator.accepted_node(world, (x, from.1, z)));
        }

        let up = from.1.checked_add(1)?;
        if self.s_clear(world, (from.0, up, from.2))
            && self.s_standable(world, (x, up, z))
        {
            return Some(self.evaluator.accepted_node(world, (x, up, z)));
        }

        if !self.s_clear(world, (x, from.1, z)) {
            return None;
        }
        for drop in 1..=self.evaluator.max_fall_distance {
            let y = from.1.checked_sub(drop)?;
            if !self.s_clear(world, (x, y, z)) {
                return None;
            }
            if self.s_standable(world, (x, y, z)) {
                return Some(self.evaluator.accepted_node(world, (x, y, z)));
            }
        }
        None
    }

    /// `NodeEvaluator::neighbors_into` through the verdict memo, same
    /// cardinal-then-diagonal enumeration order.
    fn neighbors_into_cached(
        &mut self,
        world: &dyn PathWorld,
        from: BlockPos,
        out: &mut Vec<PathNode>,
    ) {
        const CARDINALS: [(i32, i32); 4] = [(-1, 0), (0, -1), (0, 1), (1, 0)];
        const DIAGONALS: [(i32, i32); 4] = [(-1, -1), (-1, 1), (1, -1), (1, 1)];
        for (dx, dz) in CARDINALS {
            if let Some(node) = self.neighbor_at_cached(world, from, dx, dz) {
                out.push(node);
            }
        }
        for (dx, dz) in DIAGONALS {
            let (Some(x_side), Some(z_side)) = (
                self.neighbor_at_cached(world, from, dx, 0),
                self.neighbor_at_cached(world, from, 0, dz),
            ) else {
                continue;
            };
            let Some(diagonal) = self.neighbor_at_cached(world, from, dx, dz) else {
                continue;
            };
            if x_side.y == diagonal.y && z_side.y == diagonal.y {
                out.push(diagonal);
            }
        }
    }

    /// The reference A* loop, verbatim: pop the frontier minimum, skip stale
    /// or already-expanded entries, track `closest`, expand neighbours in
    /// enumeration order.  Only the containers changed shape.
    fn run_astar(&mut self, world: &dyn PathWorld, start: BlockPos, target: BlockPos) {
        self.rec.begin();
        self.heap.clear();
        self.nbuf.clear();
        let h = path_distance(start, target);
        self.rec.seed(start);
        self.heap.push(QueueEntry {
            position: start,
            f_cost: h,
            h_cost: h,
            g_cost: 0.0,
        });
        let mut closest = start;
        let mut closest_distance = h;
        let mut closest_cost = 0.0;
        let mut expanded = 0usize;
        let mut reached = false;
        while expanded < self.max_visited_nodes {
            let Some(entry) = self.heap.pop() else { break };
            if self.rec.try_expand(entry.position, entry.g_cost).is_none() {
                continue;
            }
            expanded += 1;
            let distance = path_distance(entry.position, target);
            if distance < closest_distance
                || (distance == closest_distance
                    && (entry.g_cost < closest_cost
                        || (entry.g_cost == closest_cost && entry.position < closest)))
            {
                closest = entry.position;
                closest_distance = distance;
                closest_cost = entry.g_cost;
            }
            if entry.position == target {
                reached = true;
                break;
            }
            let mut nbuf = std::mem::take(&mut self.nbuf);
            nbuf.clear();
            self.neighbors_into_cached(world, entry.position, &mut nbuf);
            for &node in &nbuf {
                let pos = node.pos();
                let cost = entry.g_cost + path_distance(entry.position, pos) + f64::from(node.malus);
                if self.rec.offer(pos, cost, entry.position) {
                    let h = path_distance(pos, target);
                    self.heap.push(QueueEntry {
                        position: pos,
                        f_cost: cost + h,
                        h_cost: h,
                        g_cost: cost,
                    });
                }
            }
            self.nbuf = nbuf;
        }
        // Reconstruct from `closest` exactly like the reference: parent links
        // back to the record with no parent (the start).
        let mut current = closest;
        loop {
            self.out
                .push(self.evaluator.accepted_node(world, current));
            match self.rec.parent_of(current) {
                Some(parent) => current = parent,
                None => break,
            }
        }
        self.out.reverse();
        self.reached_flag = reached;
    }

    // ---------------------------------------------------------------------
    // Certified straight-line fast path.
    //
    // A candidate chain S..T is returned only when it provably equals the A*
    // result.  The proof has two halves:
    //
    //  1. Unique-parent: along the chain, every alternative predecessor `m`
    //     of a chain node `n` is shown unable to offer `<= g_chain(n)`, using
    //     the horizontal grid-distance lower bound `grid_bound(S, m)`.  Then
    //     the chain parent is the ONLY node that can produce `g_min(n)`, so
    //     it is the recorded parent no matter when side branches expand —
    //     the A* parent pointers reconstruct this exact chain.
    //  2. In-time target: every node the A* can possibly expand before the
    //     target pops satisfies `grid_bound(S, n) + dist(n, T) <= C*`
    //     (frontier `f` never falls below the chain cost once the chain pins
    //     `g_min`).  Counting the cells under that bound and requiring the
    //     count to fit inside `max_visited_nodes` proves `reached = true`
    //     instead of a `closest` fallback.
    //
    // If either half fails, the caller runs the full A* — the answer is
    // always bit-exact, the fast path just amortizes the common case.
    // ---------------------------------------------------------------------

    fn try_fast_path(
        &mut self,
        world: &dyn PathWorld,
        start: BlockPos,
        target: BlockPos,
        los_rev: u64,
    ) -> Option<usize> {
        // A 1-node answer (start == target) is cheaper to emit directly.
        if start == target {
            self.chain.clear();
            self.chain_g.clear();
            self.chain.push(self.evaluator.accepted_node(world, start));
            self.chain_g.push(0.0);
            return Some(1);
        }
        // Cheap cached gate: a corridor that cannot carry *any* straight-ish
        // walk cannot carry a certified chain either.  The scan costs one or
        // two standability probes per column, so declining is nearly free.
        match self.los.get(start, target, los_rev) {
            Some(false) => return None,
            Some(true) => {}
            None => {
                let clear = self.corridor_scan(world, start, target);
                self.los.put(start, target, los_rev, clear);
                if !clear {
                    return None;
                }
            }
        }
        self.chain.clear();
        self.chain_g.clear();
        self.seen.begin();
        let mut cur = start;
        let mut g = 0.0f64;
        self.seen.insert(start);
        self.chain.push(self.evaluator.accepted_node(world, start));
        self.chain_g.push(0.0);
        while cur != target {
            // The chain can never outlive the expansion budget: A* would cap
            // first anyway, so stop and let it.
            if self.chain.len() > self.max_visited_nodes {
                return None;
            }
            let mut nbuf = std::mem::take(&mut self.nbuf);
            nbuf.clear();
            self.neighbors_into_cached(world, cur, &mut nbuf);
            let mut best: Option<(f64, f64, PathNode, f64)> = None;
            for &node in &nbuf {
                let npos = node.pos();
                if self.seen.contains(npos) {
                    continue;
                }
                let step = path_distance(cur, npos) + f64::from(node.malus);
                let g_next = g + step;
                if !self.parent_unique(world, start, cur, node, g_next) {
                    continue;
                }
                let h = path_distance(npos, target);
                let cand = (g_next + h, h, node, g_next);
                if best.is_none_or(|b| (cand.0, cand.1, npos) < (b.0, b.1, b.2.pos())) {
                    best = Some(cand);
                }
            }
            self.nbuf = nbuf;
            let (_, _, node, g_next) = best?;
            let npos = node.pos();
            self.seen.insert(npos);
            self.chain.push(node);
            self.chain_g.push(g_next);
            cur = npos;
            g = g_next;
        }
        // `g` is now the certified optimum C*: the chain is a real path and
        // no alternative parent can beat any link.
        if self.frontier_bound(world, start, target, g) >= self.max_visited_nodes {
            return None;
        }
        Some(self.chain.len())
    }

    /// Geometric corridor scan for the LOS verdict: step the (x, z) line
    /// `start -> target` and require every sampled column to offer a walk
    /// level reachable from the previous column's level (same height, a
    /// one-block climb, or a safe fall).  False negatives only ever cost a
    /// fast-path opportunity — the certified walk stays authoritative for
    /// the path it returns.
    fn corridor_scan(&mut self, world: &dyn PathWorld, start: BlockPos, target: BlockPos) -> bool {
        let dx = i64::from(target.0 - start.0);
        let dz = i64::from(target.2 - start.2);
        // Oversample x2 so columns a diagonal step only clips are not missed.
        let steps = (dx.abs().max(dz.abs()) * 2).max(1);
        let mut py = start.1;
        let mut prev = (i32::MIN, i32::MIN);
        for k in 0..=steps {
            let nx = start.0 + (dx * k / steps) as i32;
            let nz = start.2 + (dz * k / steps) as i32;
            if (nx, nz) == prev {
                continue;
            }
            prev = (nx, nz);
            match self.level_at(world, nx, nz, py) {
                Some(y) => py = y,
                None => return false,
            }
        }
        true
    }

    /// The single-column walk-level probe used by `corridor_scan`: the same
    /// standability contract a neighbor step implies — try the current
    /// level, one block up, then bounded falls.
    fn level_at(&mut self, world: &dyn PathWorld, x: i32, z: i32, py: i32) -> Option<i32> {
        if self.s_standable(world, (x, py, z)) {
            return Some(py);
        }
        let up = py.checked_add(1)?;
        if self.s_standable(world, (x, up, z)) {
            return Some(up);
        }
        for drop in 1..=self.evaluator.max_fall_distance {
            let y = py.checked_sub(drop)?;
            if self.s_standable(world, (x, y, z)) {
                return Some(y);
            }
        }
        None
    }

    /// Unique-parent certificate for edge `cur -> n` with chain cost
    /// `g_next`.  Returns `false` when any other real predecessor of `n`
    /// could match or beat the offer, because then expansion order — which
    /// this certificate deliberately does not model — could change the
    /// recorded parent.
    fn parent_unique(
        &mut self,
        world: &dyn PathWorld,
        start: BlockPos,
        cur: BlockPos,
        node: PathNode,
        g_next: f64,
    ) -> bool {
        let npos = node.pos();
        let malus_n = f64::from(node.malus);
        // Tolerance covers accumulated f64 rounding between the bound and
        // the chain sum — a slack comparison only ever declines more.
        let slack = g_next.abs().max(1.0) * 1.0e-9;
        let y_lo = npos.1 - 1;
        let y_hi = npos.1 + self.evaluator.max_fall_distance;
        for (dx, dz) in [
            (-1, 0),
            (0, -1),
            (0, 1),
            (1, 0),
            (-1, -1),
            (-1, 1),
            (1, -1),
            (1, 1),
        ] {
            for my in y_lo..=y_hi {
                let m = (npos.0 - dx, my, npos.2 - dz);
                if m == cur {
                    continue;
                }
                // Real offer lower bound: g_min(m) >= grid_bound(S, m), and
                // the edge m -> n costs dist + malus(n).
                let step = path_distance(m, npos) + malus_n;
                let offer_lb = grid_bound(start, m) + step;
                if offer_lb > g_next + slack {
                    continue;
                }
                // Potentially a competing parent — only a real predecessor
                // can actually record a cheaper-or-equal parent though.
                if self.is_parent(world, m, node) {
                    return false;
                }
            }
        }
        true
    }

    /// Whether `neighbors(m)` contains `n`, replicating the walker's cardinal
    /// and diagonal-side-clearance rules for a single direction.
    fn is_parent(&mut self, world: &dyn PathWorld, m: BlockPos, node: PathNode) -> bool {
        let npos = node.pos();
        let dx = npos.0 - m.0;
        let dz = npos.2 - m.2;
        if dx == 0 && dz == 0 {
            return false;
        }
        if dx == 0 || dz == 0 {
            return matches!(
                self.neighbor_at_cached(world, m, dx, dz),
                Some(p) if p.pos() == npos
            );
        }
        let (Some(diag), Some(xs), Some(zs)) = (
            self.neighbor_at_cached(world, m, dx, dz),
            self.neighbor_at_cached(world, m, dx, 0),
            self.neighbor_at_cached(world, m, 0, dz),
        ) else {
            return false;
        };
        diag.pos() == npos && xs.y == npos.1 && zs.y == npos.1
    }

    /// Conservative count of cells that could expand before the certified
    /// target pops: `{grid_bound(S, n) + dist3d(n, T) <= C*}` plus a fixed
    /// tie-shell margin, computed per column with early exit.
    fn frontier_bound(
        &self,
        world: &dyn PathWorld,
        start: BlockPos,
        target: BlockPos,
        c_star: f64,
    ) -> usize {
        const TIE_SLACK: usize = 512;
        let reach = c_star.ceil() as i64;
        // The box scan is O(reach^2); long certified chains are rare enough
        // that declining above a small reach beats paying for them.
        if reach > 96 {
            return usize::MAX;
        }
        let mut count = 0usize;
        for dx in -reach..=reach {
            for dz in -reach..=reach {
                let nx = start.0 as i64 + dx;
                let nz = start.2 as i64 + dz;
                if nx > i32::MAX as i64 || nx < i32::MIN as i64 {
                    continue;
                }
                if nz > i32::MAX as i64 || nz < i32::MIN as i64 {
                    continue;
                }
                let (adx, adz) = (dx.unsigned_abs(), dz.unsigned_abs());
                let (lo, hi) = (adx.min(adz), adx.max(adz));
                let dg = lo as f64 * std::f64::consts::SQRT_2 + (hi - lo) as f64;
                let rem = c_star - dg;
                if rem < 0.0 {
                    continue;
                }
                let dtx = f64::from(nx as i32 - target.0);
                let dtz = f64::from(nz as i32 - target.2);
                let dxz = dtx * dtx + dtz * dtz;
                if dxz > rem * rem {
                    continue;
                }
                let half = (rem * rem - dxz).sqrt();
                if !half.is_finite() {
                    continue;
                }
                let span = half.floor() as i64;
                let lo_y = (target.1 as i64 - span).max(world.min_y() as i64);
                let hi_y = (target.1 as i64 + span).min(world.max_y() as i64);
                if hi_y >= lo_y {
                    count += (hi_y - lo_y + 1) as usize;
                }
                if count >= self.max_visited_nodes + TIE_SLACK {
                    return count;
                }
            }
        }
        count + TIE_SLACK
    }
}

// ===========================================================================
// ReusableCellMerger
// ===========================================================================

/// Chained open-addressed `cell -> member head` index shared by every merge
/// kernel — the same shape the live merge uses: no resizing mid-pass, no
/// per-cell allocation, three multiplies and a mask per probe.
struct CellTable {
    cell: Vec<(i32, i32, i32)>,
    used: Vec<bool>,
    head: Vec<u32>,
    next: Vec<u32>,
    mask: usize,
}

const NO_MEMBER: u32 = u32::MAX;

impl CellTable {
    fn new() -> Self {
        Self {
            cell: Vec::new(),
            used: Vec::new(),
            head: Vec::new(),
            next: Vec::new(),
            mask: 0,
        }
    }

    /// Rebuild for `items`.  The table only ever grows: a larger-than-needed
    /// table probes identically (the mask adapts to its own length), so
    /// alternating between differently-sized merge sets never reallocates.
    /// `cell` needs no clearing because every read is gated by `used`.
    fn reset(&mut self, items: usize) {
        let cap = (items.max(4) * 2).next_power_of_two();
        if cap > self.cell.len() {
            self.cell = vec![(0, 0, 0); cap];
            self.used = vec![false; cap];
            self.head = vec![NO_MEMBER; cap];
            self.mask = cap - 1;
        } else {
            self.used.fill(false);
            self.head.fill(NO_MEMBER);
        }
        self.next.clear();
        self.next.resize(items, NO_MEMBER);
    }

    #[inline]
    fn slot(&self, cell: (i32, i32, i32)) -> usize {
        let h = (cell.0 as u32)
            .wrapping_mul(0x9E37_79B1)
            .wrapping_add((cell.1 as u32).wrapping_mul(0x85EB_CA77))
            .wrapping_add((cell.2 as u32).wrapping_mul(0xC2B2_AE3D));
        let h = h ^ (h >> 15) ^ (h >> 7);
        (h as usize) & self.mask
    }

    /// Link `idx` into `cell`'s bucket, newest first — callers sort their
    /// collected candidates back into ascending order.
    fn push(&mut self, cell: (i32, i32, i32), idx: u32) {
        let mut at = self.slot(cell);
        while self.used[at] && self.cell[at] != cell {
            at = (at + 1) & self.mask;
        }
        if !self.used[at] {
            self.used[at] = true;
            self.cell[at] = cell;
        }
        self.next[idx as usize] = self.head[at];
        self.head[at] = idx;
    }

    #[inline]
    fn members(&self, cell: (i32, i32, i32)) -> Option<u32> {
        let mut at = self.slot(cell);
        while self.used[at] {
            if self.cell[at] == cell {
                return Some(self.head[at]);
            }
            at = (at + 1) & self.mask;
        }
        None
    }
}

/// Generation-stamped eaten bitset: one `u32` stamp per word, so a new pass
/// "clears" the whole set by bumping the generation instead of touching
/// every bit.
struct EatenBits {
    bits: Vec<u64>,
    stamp: Vec<u32>,
    cur_gen: u32,
}

impl EatenBits {
    fn new() -> Self {
        Self {
            bits: Vec::new(),
            stamp: Vec::new(),
            cur_gen: 0,
        }
    }

    fn begin(&mut self, n: usize) {
        self.cur_gen = self.cur_gen.wrapping_add(1);
        if self.cur_gen == 0 {
            self.stamp.fill(0);
            self.cur_gen = 1;
        }
        let words = n.div_ceil(64);
        if self.bits.len() < words {
            self.bits.resize(words, 0);
            self.stamp.resize(words, 0);
        }
    }

    #[inline]
    fn set(&mut self, idx: usize) {
        let word = idx / 64;
        if self.stamp[word] != self.cur_gen {
            self.stamp[word] = self.cur_gen;
            self.bits[word] = 0;
        }
        self.bits[word] |= 1u64 << (idx % 64);
    }

    #[inline]
    fn get(&self, idx: usize) -> bool {
        let word = idx / 64;
        word < self.bits.len()
            && self.stamp[word] == self.cur_gen
            && self.bits[word] & (1u64 << (idx % 64)) != 0
    }
}

/// The view the orb merge kernel needs of one XP-orb-like entity.
pub trait SimXp {
    fn eid(&self) -> i32;
    fn pos(&self) -> [f64; 3];
    fn age(&self) -> i32;
    fn value(&self) -> i16;
    fn set_value(&mut self, val: i16);
}

/// The view the dropped-item merge kernels need of one item entity.
/// `same_stack` must be an *exact* mergeability test (vanilla compares item
/// id, pickup target and component blob verbatim); `stack_cap` is the
/// `item_stack_max`-equivalent, already clamped to at least 1.
pub trait SimItem {
    fn eid(&self) -> i32;
    fn pos(&self) -> [f64; 3];
    fn age(&self) -> i32;
    fn count(&self) -> u32;
    fn set_count(&mut self, cnt: u32);
    fn stack_cap(&self) -> u32;
    fn same_stack(&self, other: &Self) -> bool;
}

/// Zero-allocation, exact-order merge kernels for XP orbs and dropped items.
///
/// All three entry points return the eaten eids in the order the merge
/// produced them — the order destroy packets go out in — borrowed from the
/// reusable output buffer.
pub struct ReusableCellMerger {
    cells: CellTable,
    candidates: Vec<u32>,
    merged: Vec<i32>,
    pool: Vec<u32>,
    cnts: Vec<u32>,
    maxes: Vec<u32>,
    eaten: EatenBits,
}

impl Default for ReusableCellMerger {
    fn default() -> Self {
        Self::new()
    }
}

impl ReusableCellMerger {
    pub fn new() -> Self {
        Self {
            cells: CellTable::new(),
            candidates: Vec::new(),
            merged: Vec::new(),
            pool: Vec::new(),
            cnts: Vec::new(),
            maxes: Vec::new(),
            eaten: EatenBits::new(),
        }
    }

    /// The eaten-eid output of the last merge call, in merge order.
    pub fn merged(&self) -> &[i32] {
        &self.merged
    }

    /// Whether pool position `idx` was eaten during the last merge — for
    /// audits and drain-order verification.
    pub fn was_eaten(&self, idx: usize) -> bool {
        self.eaten.get(idx)
    }

    /// Collect one cell's members with index `> after`, in chain order.
    fn collect_after(&self, cell: (i32, i32, i32), after: usize, out: &mut Vec<u32>) {
        let Some(mut member) = self.cells.members(cell) else {
            return;
        };
        while member != NO_MEMBER {
            let idx = member as usize;
            if idx > after {
                out.push(member);
            }
            member = self.cells.next[idx];
        }
    }

    /// Collect one cell's whole member list, in chain order.
    fn collect_all(&self, cell: (i32, i32, i32), out: &mut Vec<u32>) {
        let Some(mut member) = self.cells.members(cell) else {
            return;
        };
        while member != NO_MEMBER {
            out.push(member);
            member = self.cells.next[member as usize];
        }
    }

    /// The per-tick XP-orb merge: orbs closer than one block coalesce into
    /// the older orb.  `dim_of`/`clear_dim` replicate the entity-dimension
    /// side table exactly — including the quirk that clearing an eaten orb's
    /// dimension lets it compare as the default dimension for the rest of
    /// the same pass.
    ///
    /// Dead orbs stay candidates until the caller's `retain`: an orb eaten
    /// by an earlier `i` still scans and still eats, so `merged` can carry
    /// an eid twice.  That is the contract.
    pub fn merge_orbs<M, D, C>(
        &mut self,
        xl: &mut [M],
        mut dim_of: D,
        mut clear_dim: C,
    ) -> &[i32]
    where
        M: SimXp,
        D: FnMut(i32) -> u64,
        C: FnMut(i32),
    {
        self.merged.clear();
        self.eaten.begin(xl.len());
        if xl.len() < 2 {
            return &self.merged;
        }
        self.cells.reset(xl.len());
        for (idx, x) in xl.iter().enumerate() {
            let p = x.pos();
            self.cells.push(
                (p[0].floor() as i32, p[1].floor() as i32, p[2].floor() as i32),
                idx as u32,
            );
        }
        let mut candidates = std::mem::take(&mut self.candidates);
        for i in 0..xl.len() {
            let pi = xl[i].pos();
            let (cx, cy, cz) = (
                pi[0].floor() as i32,
                pi[1].floor() as i32,
                pi[2].floor() as i32,
            );
            candidates.clear();
            for dx in -1..=1i32 {
                for dy in -1..=1i32 {
                    for dz in -1..=1i32 {
                        let key = (
                            cx.checked_add(dx),
                            cy.checked_add(dy),
                            cz.checked_add(dz),
                        );
                        if let (Some(kx), Some(ky), Some(kz)) = key {
                            self.collect_after((kx, ky, kz), i, &mut candidates);
                        }
                    }
                }
            }
            candidates.sort_unstable();
            for &j in &candidates {
                let j = j as usize;
                let (ax, ay, az, aage, aeid) = {
                    let a = &xl[i];
                    let p = a.pos();
                    (p[0], p[1], p[2], a.age(), a.eid())
                };
                let (bx, by, bz, bage, beid) = {
                    let b = &xl[j];
                    let p = b.pos();
                    (p[0], p[1], p[2], b.age(), b.eid())
                };
                if (ax - bx).powi(2) + (ay - by).powi(2) + (az - bz).powi(2) < 1.0
                    && dim_of(aeid) == dim_of(beid)
                {
                    let (keep, eat) = if aage <= bage { (i, j) } else { (j, i) };
                    let sum = xl[keep].value().saturating_add(xl[eat].value());
                    xl[keep].set_value(sum);
                    self.merged.push(xl[eat].eid());
                    self.eaten.set(eat);
                    clear_dim(xl[eat].eid());
                    break;
                }
            }
        }
        self.candidates = candidates;
        &self.merged
    }

    /// The per-tick dropped-item merge: same-stack drops within half a block
    /// coalesce into the older entity while the combined count fits the
    /// stack cap.
    ///
    /// Full-stack pre-filter: a stack at `count >= stack_cap` can never
    /// satisfy `a + b <= cap` for a partner with `count >= 1`, so it is
    /// inert in both merge directions and is skipped before bucketing.  When
    /// a `count == 0` stack exists the filter is off entirely, because an
    /// empty stack *can* merge into a full one.
    pub fn merge_items<M: SimItem>(&mut self, dp: &mut [M]) -> &[i32] {
        self.merged.clear();
        self.eaten.begin(dp.len());
        if dp.len() < 2 {
            return &self.merged;
        }
        let has_empty = dp.iter().any(|d| d.count() == 0);
        let skip_full = |d: &M| !has_empty && d.count() >= d.stack_cap();
        self.cells.reset(dp.len());
        for (idx, d) in dp.iter().enumerate() {
            if skip_full(d) {
                continue;
            }
            let p = d.pos();
            self.cells.push(
                (p[0].floor() as i32, p[1].floor() as i32, p[2].floor() as i32),
                idx as u32,
            );
        }
        let mut candidates = std::mem::take(&mut self.candidates);
        for i in 0..dp.len() {
            if skip_full(&dp[i]) {
                continue;
            }
            let pi = dp[i].pos();
            let (cx, cy, cz) = (
                pi[0].floor() as i32,
                pi[1].floor() as i32,
                pi[2].floor() as i32,
            );
            candidates.clear();
            for dx in -1..=1i32 {
                for dy in -1..=1i32 {
                    for dz in -1..=1i32 {
                        let key = (
                            cx.checked_add(dx),
                            cy.checked_add(dy),
                            cz.checked_add(dz),
                        );
                        if let (Some(kx), Some(ky), Some(kz)) = key {
                            self.collect_after((kx, ky, kz), i, &mut candidates);
                        }
                    }
                }
            }
            candidates.sort_unstable();
            for &j in &candidates {
                let j = j as usize;
                let (aage, acnt, amax) = {
                    let a = &dp[i];
                    (a.age(), a.count(), a.stack_cap())
                };
                let (bage, bcnt) = {
                    let b = &dp[j];
                    (b.age(), b.count())
                };
                let (pa, pb) = (dp[i].pos(), dp[j].pos());
                if dp[i].same_stack(&dp[j])
                    && (pa[0] - pb[0]).powi(2) + (pa[1] - pb[1]).powi(2) + (pa[2] - pb[2]).powi(2)
                        < 0.25
                    && acnt + bcnt <= amax
                {
                    let (keep, eat) = if aage <= bage { (i, j) } else { (j, i) };
                    let sum = dp[keep].count().saturating_add(dp[eat].count());
                    dp[keep].set_count(sum);
                    self.merged.push(dp[eat].eid());
                    self.eaten.set(eat);
                    break;
                }
            }
        }
        self.candidates = candidates;
        &self.merged
    }

    /// The fresh-burst pending merge: `pending` donors feed the earliest
    /// compatible keeper within half a block, up to the stack cap.  The pool
    /// lists keepers (non-pending, `count > 0`) first, then donors — the
    /// exact order the reference merge walks — and a donor that survives
    /// part of its stack becomes a keeper for later donors.
    ///
    /// Full-stack keepers are never inserted into cells: a stack at
    /// `count >= cap` can absorb nothing, so 5,000 finished stacks cost ~0
    /// merge ops while partials merge bit-identically.
    ///
    /// Returns the tombstoned eids in pool order; survivors' counts are
    /// written back clamped to `u8::MAX` exactly like the reference.
    pub fn merge_pending_items<M, P>(&mut self, dp: &mut [M], mut pending: P) -> &[i32]
    where
        M: SimItem,
        P: FnMut(i32) -> bool,
    {
        const CELL: f64 = 2.0;
        self.merged.clear();
        self.eaten.begin(dp.len());
        self.pool.clear();
        self.pool
            .extend(dp.iter().enumerate().filter_map(|(i, d)| {
                (!pending(d.eid()) && d.count() > 0).then_some(i as u32)
            }));
        self.pool
            .extend(dp.iter().enumerate().filter_map(|(i, d)| {
                (pending(d.eid()) && d.count() > 0).then_some(i as u32)
            }));
        if self.pool.len() < 2 {
            return &self.merged;
        }
        self.cnts.clear();
        self.cnts
            .extend(self.pool.iter().map(|&i| dp[i as usize].count()));
        self.maxes.clear();
        self.maxes
            .extend(self.pool.iter().map(|&i| dp[i as usize].stack_cap().max(1)));
        self.cells.reset(self.pool.len());
        // NOTE: pending merge cells are keyed on pool position and only ever
        // hold *earlier* pool indices, so membership needs no index filter.
        let mut candidates = std::mem::take(&mut self.candidates);
        for i in 0..self.pool.len() {
            if self.cnts[i] == 0 {
                continue;
            }
            let pi = dp[self.pool[i] as usize].pos();
            let (cx, cy, cz) = (
                (pi[0] / CELL).floor() as i32,
                (pi[1] / CELL).floor() as i32,
                (pi[2] / CELL).floor() as i32,
            );
            candidates.clear();
            for dx in -1..=1i32 {
                for dy in -1..=1i32 {
                    for dz in -1..=1i32 {
                        let key = (
                            cx.checked_add(dx),
                            cy.checked_add(dy),
                            cz.checked_add(dz),
                        );
                        if let (Some(kx), Some(ky), Some(kz)) = key {
                            self.collect_all((kx, ky, kz), &mut candidates);
                        }
                    }
                }
            }
            candidates.sort_unstable();
            let mut alive = true;
            for &j in &candidates {
                let j = j as usize;
                let (a, b) = (self.pool[i] as usize, self.pool[j] as usize);
                let (pa, pb) = (dp[a].pos(), dp[b].pos());
                if dp[a].same_stack(&dp[b])
                    && (pa[0] - pb[0]).powi(2) + (pa[1] - pb[1]).powi(2) + (pa[2] - pb[2]).powi(2)
                        <= 0.25
                {
                    let max = self.maxes[j].min(self.maxes[i]);
                    let transfer = self.cnts[i].min(max.saturating_sub(self.cnts[j]));
                    self.cnts[j] += transfer;
                    self.cnts[i] -= transfer;
                    if self.cnts[i] == 0 {
                        alive = false;
                        break;
                    }
                }
            }
            // A stack that can never absorb never joins the index.
            if alive && self.cnts[i] < self.maxes[i] {
                self.cells.push((cx, cy, cz), i as u32);
            }
        }
        self.candidates = candidates;
        for (k, &i) in self.pool.iter().enumerate() {
            let i = i as usize;
            if self.cnts[k] == 0 {
                self.eaten.set(i);
                self.merged.push(dp[i].eid());
            } else {
                let cnt = self.cnts[k].min(u32::from(u8::MAX));
                dp[i].set_count(cnt);
            }
        }
        &self.merged
    }
}

// ===========================================================================
// RestingEntitySleepState
// ===========================================================================

const MAX_WATCH: usize = 6;

#[derive(Clone, Copy, Default)]
struct SleepEntry {
    block: BlockPos,
    watch: [BlockPos; MAX_WATCH],
    revs: [u64; MAX_WATCH],
    watch_len: u8,
}

#[derive(Clone, Copy)]
struct SleepSlot {
    /// 0 = empty, 1 = live, 2 = tombstone.
    state: u8,
    eid: i32,
    entry: SleepEntry,
}

/// Why a resting entity was woken or refused sleep on the last `tick`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WakeReason {
    /// The caller signalled an impulse (knockback, explosion, piston throw).
    Impulse,
    /// Velocity was non-zero, the entity was not on the ground, or it
    /// changed block cell.
    Moved,
    /// A watched section revision changed: block break underneath, fluid
    /// arrival, or any edit in a watched section.
    RevChanged,
    /// No sleep entry existed for this entity.
    NotTracked,
}

/// Per-tick verdict of [`RestingEntitySleepState::tick`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RestVerdict {
    /// Run the full physics tick: collision scan, fluid drag, move, then
    /// `offer_sleep` if it settled.
    Awake(WakeReason),
    /// Skip every block-collision and fluid-drag query.  Age, despawn,
    /// player-pickup and merge checks still run normally.
    Asleep,
}

/// Exact-tick sleep for resting items, XP orbs and lodged projectiles.
///
/// An entity is eligible to sleep only when a full awake tick reports
/// `on_ground && vel == 0 && !in_fluid` — the vanilla stationary condition —
/// at which point [`Self::offer_sleep`] pins its block cell plus the cells a
/// fluid arrival or floor break could reach (the block under the feet and
/// the four horizontal neighbours).  Every later tick answers
/// [`RestVerdict::Asleep`] while all watched section revisions hold; the
/// *exact tick* any of them bumps, the answer is `Awake` again.  Sleeping
/// entities keep every non-spatial behaviour — age accrual, despawn
/// deadlines, pickup radii — the sleep only suppresses redundant
/// block-collision and fluid queries.
pub struct RestingEntitySleepState {
    slots: Vec<SleepSlot>,
    mask: usize,
    /// Live (state==1) entries.
    live: usize,
    /// Non-empty slots (live + tombstone) — drives the grow threshold so a
    /// probe always has an empty slot to terminate on.
    used: usize,
    sleeping: usize,
    /// Lifetime diagnostics: ticks answered `Asleep` and total wake events.
    pub ticks_skipped: u64,
    pub wake_events: u64,
}

const EMPTY_SLOT: SleepSlot = SleepSlot {
    state: 0,
    eid: 0,
    entry: SleepEntry {
        block: (0, 0, 0),
        watch: [(0, 0, 0); MAX_WATCH],
        revs: [0; MAX_WATCH],
        watch_len: 0,
    },
};

impl RestingEntitySleepState {
    /// `capacity` slots, rounded to a power of two; grow-on-demand keeps the
    /// table under 3/4 load.
    pub fn new(capacity: usize) -> Self {
        let cap = capacity.max(16).next_power_of_two();
        Self {
            slots: vec![EMPTY_SLOT; cap],
            mask: cap - 1,
            live: 0,
            used: 0,
            sleeping: 0,
            ticks_skipped: 0,
            wake_events: 0,
        }
    }

    #[inline]
    fn slot_of(&self, eid: i32) -> usize {
        let h = (eid as u32).wrapping_mul(0x9E37_79B1);
        let h = h ^ (h >> 15) ^ (h >> 7);
        (h as usize) & self.mask
    }

    /// Lookup probe: walks past tombstones, returns the live slot for `eid`
    /// or the first empty slot (which proves `eid` is absent).
    #[inline]
    fn probe(&self, eid: i32) -> usize {
        let mut at = self.slot_of(eid);
        loop {
            let s = &self.slots[at];
            if s.state == 0 || (s.state == 1 && s.eid == eid) {
                return at;
            }
            at = (at + 1) & self.mask;
        }
    }

    /// Insert probe: returns the first slot that is empty, a tombstone, or
    /// already holds `eid` — tombstones get recycled instead of skipped.
    #[inline]
    fn probe_insert(&self, eid: i32) -> usize {
        let mut at = self.slot_of(eid);
        loop {
            let s = &self.slots[at];
            if s.state != 1 || s.eid == eid {
                return at;
            }
            at = (at + 1) & self.mask;
        }
    }

    /// Grow when non-empty slots reach 3/4 of the table; the rehash also
    /// drops every tombstone, restoring probe termination slack.
    fn grow_if_needed(&mut self) {
        let cap = self.slots.len();
        if self.used * 4 < cap * 3 {
            return;
        }
        let mut fresh = Self::new(cap * 2);
        for s in &self.slots {
            if s.state == 1 {
                let at = fresh.probe_insert(s.eid);
                fresh.slots[at] = *s;
                fresh.live += 1;
                fresh.used += 1;
            }
        }
        fresh.sleeping = self.sleeping;
        fresh.ticks_skipped = self.ticks_skipped;
        fresh.wake_events = self.wake_events;
        *self = fresh;
    }

    fn remove(&mut self, eid: i32) -> bool {
        let at = self.probe(eid);
        if self.slots[at].state == 1 {
            self.slots[at].state = 2;
            self.live -= 1;
            self.sleeping -= 1;
            true
        } else {
            false
        }
    }

    /// Number of entities currently asleep.
    pub fn sleeping_count(&self) -> usize {
        self.sleeping
    }

    /// The per-tick gate.  `rev(x, y, z)` must return the section revision of
    /// the section containing block cell `(x, y, z)` — a counter the world
    /// bumps on every block or fluid edit inside that 16³ section.  Anything
    /// else the entity needs for its full tick (collision, fluid state) is
    /// deliberately *not* asked while asleep: that is the saving.
    pub fn tick(
        &mut self,
        eid: i32,
        pos: [f64; 3],
        on_ground: bool,
        vel: [f64; 3],
        impulse: bool,
        rev: &mut impl FnMut(i32, i32, i32) -> u64,
    ) -> RestVerdict {
        if impulse {
            self.remove(eid);
            self.wake_events += 1;
            return RestVerdict::Awake(WakeReason::Impulse);
        }
        if !on_ground || vel != [0.0; 3] {
            self.remove(eid);
            self.wake_events += 1;
            return RestVerdict::Awake(WakeReason::Moved);
        }
        let at = self.probe(eid);
        if self.slots[at].state != 1 {
            return RestVerdict::Awake(WakeReason::NotTracked);
        }
        let entry = self.slots[at].entry;
        let block = (
            pos[0].floor() as i32,
            pos[1].floor() as i32,
            pos[2].floor() as i32,
        );
        if block != entry.block {
            self.remove(eid);
            self.wake_events += 1;
            return RestVerdict::Awake(WakeReason::Moved);
        }
        for w in 0..entry.watch_len as usize {
            if rev(entry.watch[w].0, entry.watch[w].1, entry.watch[w].2) != entry.revs[w] {
                self.remove(eid);
                self.wake_events += 1;
                return RestVerdict::Awake(WakeReason::RevChanged);
            }
        }
        self.ticks_skipped += 1;
        RestVerdict::Asleep
    }

    /// Offer an entity for sleep after a fully-ticked awake pass.  Only the
    /// exact stationary condition `on_ground && vel == 0 && !in_fluid` is
    /// accepted; anything else drops whatever entry existed.
    pub fn offer_sleep(
        &mut self,
        eid: i32,
        pos: [f64; 3],
        in_fluid: bool,
        on_ground: bool,
        vel: [f64; 3],
        rev: &mut impl FnMut(i32, i32, i32) -> u64,
    ) {
        if in_fluid || !on_ground || vel != [0.0; 3] {
            self.remove(eid);
            return;
        }
        self.grow_if_needed();
        let block = (
            pos[0].floor() as i32,
            pos[1].floor() as i32,
            pos[2].floor() as i32,
        );
        let mut entry = SleepEntry {
            block,
            ..SleepEntry::default()
        };
        // Watch the cells a physics-relevant edit could arrive through: the
        // entity's own cell, the support block beneath it, and the four
        // horizontal neighbours a fluid can occupy next door.
        let mut watch = [block; MAX_WATCH];
        watch[1] = (block.0, block.1 - 1, block.2);
        watch[2] = (block.0 - 1, block.1, block.2);
        watch[3] = (block.0 + 1, block.1, block.2);
        watch[4] = (block.0, block.1, block.2 - 1);
        watch[5] = (block.0, block.1, block.2 + 1);
        let mut len = 0usize;
        'cells: for w in watch {
            for seen in entry.watch[..len].iter() {
                if *seen == w {
                    continue 'cells;
                }
            }
            entry.watch[len] = w;
            entry.revs[len] = rev(w.0, w.1, w.2);
            len += 1;
        }
        entry.watch_len = len as u8;
        let at = self.probe_insert(eid);
        if self.slots[at].state != 1 {
            if self.slots[at].state == 0 {
                self.used += 1;
            }
            self.live += 1;
            self.sleeping += 1;
        }
        self.slots[at] = SleepSlot {
            state: 1,
            eid,
            entry,
        };
    }

    /// Force one entity awake (e.g. it was picked up or removed).  Entities
    /// the tracker never saw are ignored.
    pub fn wake(&mut self, eid: i32) {
        self.remove(eid);
    }
}
