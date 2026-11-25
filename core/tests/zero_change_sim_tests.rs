// SPDX-License-Identifier: LGPL-3.0-or-later
//! Verification for `megu_core::zero_change_sim`: bit-exact parity against
//! the naive `BTreeMap`/`BTreeSet`/O(n^2) references, zero heap allocations
//! after warm-up, and measurable speedups.

use megu_core::zero_change_sim::{
    BlockPathType, BlockPos, NodeEvaluator, PathFinderScratch, PathNode, PathWorld,
    RestVerdict, RestingEntitySleepState, ReusableCellMerger, SimItem, SimXp, WakeReason,
};
use std::alloc::{GlobalAlloc, Layout, System};
use std::collections::{BTreeMap, BTreeSet, BinaryHeap, HashMap, HashSet};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

// ---------------------------------------------------------------------------
// Allocation counter.
// ---------------------------------------------------------------------------

static ALLOCS: AtomicUsize = AtomicUsize::new(0);
static ALLOC_SIZES: [AtomicUsize; 64] = [const { AtomicUsize::new(0) }; 64];

struct CountingAlloc;

unsafe impl GlobalAlloc for CountingAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCS.fetch_add(1, Ordering::Relaxed);
        let bucket = (usize::BITS - layout.size().max(1).leading_zeros() - 1).min(63);
        ALLOC_SIZES[bucket as usize].fetch_add(1, Ordering::Relaxed);
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
}

fn dump_alloc_sizes() -> String {
    let mut s = String::new();
    for (i, c) in ALLOC_SIZES.iter().enumerate() {
        let n = c.load(Ordering::Relaxed);
        if n > 0 {
            s += &format!(" 2^{i}~{n}");
        }
    }
    s
}

/// Serializes allocation-measuring tests: `#[global_allocator]` counts every
/// allocation in the process, so a parallel test would pollute the window.
static ALLOC_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[global_allocator]
static ALLOC: CountingAlloc = CountingAlloc;

// ---------------------------------------------------------------------------
// Deterministic RNG (splitmix64).
// ---------------------------------------------------------------------------

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

// ---------------------------------------------------------------------------
// Grid world: per-column floors plus solid/water cells.
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct GridWorld {
    /// Highest solid y per (x, z) column; defaults to 0 (ground plane).
    floor: HashMap<(i32, i32), i32>,
    /// Extra solid cells (walls, pillars).
    solid: HashSet<BlockPos>,
    /// Water cells (path type `Water`, malus 8).
    water: HashSet<BlockPos>,
}

impl GridWorld {
    fn floor_y(&self, x: i32, z: i32) -> i32 {
        self.floor.get(&(x, z)).copied().unwrap_or(0)
    }
    fn rev(&self) -> u64 {
        let mut h = 0xCBF2_9CE4_8422_2325u64;
        let mut mix = |v: u64| {
            h ^= v;
            h = h.wrapping_mul(0x0000_0100_0000_01B3);
        };
        for (&(x, z), &y) in &self.floor {
            mix((x as u64) << 40 ^ (z as u64) << 12 ^ y as u64);
        }
        h ^= self.solid.len() as u64 ^ ((self.water.len() as u64) << 32);
        h
    }
}

impl PathWorld for GridWorld {
    fn path_type(&self, x: i32, y: i32, z: i32) -> BlockPathType {
        if self.water.contains(&(x, y, z)) {
            BlockPathType::Water
        } else if self.is_solid(x, y, z) {
            BlockPathType::Blocked
        } else {
            BlockPathType::Open
        }
    }
    fn is_solid(&self, x: i32, y: i32, z: i32) -> bool {
        y <= self.floor_y(x, z) || self.solid.contains(&(x, y, z))
    }
    fn collision_height(&self, x: i32, y: i32, z: i32) -> f64 {
        if self.is_solid(x, y, z) {
            1.0
        } else {
            0.0
        }
    }
    fn min_y(&self) -> i32 {
        -4
    }
    fn max_y(&self) -> i32 {
        10
    }
}

/// A deterministic maze: mostly flat ground with raised columns, pits,
/// scattered wall pillars and the occasional water cell.
fn gen_world(seed: u64) -> GridWorld {
    let mut rng = Rng(seed);
    let mut w = GridWorld {
        floor: HashMap::new(),
        solid: HashSet::new(),
        water: HashSet::new(),
    };
    let size = 22 + rng.below(10) as i32;
    for x in -2..size {
        for z in -2..size {
            let roll = rng.below(100);
            if roll < 8 {
                // Raised platform — climb/fall edges.
                w.floor.insert((x, z), 1);
            } else if roll < 11 {
                // Pit column — cannot stand here at all.
                w.floor.insert((x, z), -4);
            }
            let wy = w.floor_y(x, z);
            if rng.below(100) < 14 {
                // A two-block wall segment blocks the body at feet level.
                w.solid.insert((x, wy + 1, z));
                w.solid.insert((x, wy + 2, z));
            } else if rng.below(100) < 3 {
                w.water.insert((x, wy + 1, z));
                w.water.insert((x, wy + 2, z));
            }
        }
    }
    w
}

fn random_standable(ev: &NodeEvaluator, w: &GridWorld, rng: &mut Rng) -> BlockPos {
    loop {
        let x = rng.below(24) as i32 - 1;
        let z = rng.below(24) as i32 - 1;
        let y = w.floor_y(x, z) + 1;
        if ev.standable(w, (x, y, z)) {
            return (x, y, z);
        }
    }
}

// ---------------------------------------------------------------------------
// Naive reference pathfinder: the BTreeMap/BTreeSet/BinaryHeap walk.
// ---------------------------------------------------------------------------

struct NRec {
    g: f64,
    parent: Option<BlockPos>,
}

#[derive(Clone, Copy)]
struct NEntry {
    pos: BlockPos,
    f: f64,
    h: f64,
    g: f64,
}

impl PartialEq for NEntry {
    fn eq(&self, o: &Self) -> bool {
        self.cmp(o) == std::cmp::Ordering::Equal
    }
}
impl Eq for NEntry {}
impl PartialOrd for NEntry {
    fn partial_cmp(&self, o: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(o))
    }
}
impl Ord for NEntry {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        other
            .f
            .total_cmp(&self.f)
            .then_with(|| other.h.total_cmp(&self.h))
            .then_with(|| other.pos.cmp(&self.pos))
    }
}

fn pdist(a: BlockPos, b: BlockPos) -> f64 {
    let dx = f64::from(a.0) - f64::from(b.0);
    let dy = f64::from(a.1) - f64::from(b.1);
    let dz = f64::from(a.2) - f64::from(b.2);
    (dx * dx + dy * dy + dz * dz).sqrt()
}

fn naive_find_path(
    ev: &NodeEvaluator,
    world: &dyn PathWorld,
    start: BlockPos,
    target: BlockPos,
    max_visited: usize,
) -> (Vec<PathNode>, bool) {
    if !ev.standable(world, start) {
        return (Vec::new(), false);
    }
    let mut records: BTreeMap<BlockPos, NRec> = BTreeMap::new();
    let mut expanded: BTreeSet<BlockPos> = BTreeSet::new();
    let mut heap: BinaryHeap<NEntry> = BinaryHeap::new();
    let h = pdist(start, target);
    records.insert(start, NRec { g: 0.0, parent: None });
    heap.push(NEntry {
        pos: start,
        f: h,
        h,
        g: 0.0,
    });
    let mut closest = start;
    let mut closest_distance = h;
    let mut closest_cost = 0.0;
    let mut reached = false;
    while expanded.len() < max_visited {
        let Some(entry) = heap.pop() else { break };
        let Some(rec) = records.get(&entry.pos) else { continue };
        let rec_g = rec.g;
        if expanded.contains(&entry.pos) || entry.g > rec_g {
            continue;
        }
        expanded.insert(entry.pos);
        let distance = pdist(entry.pos, target);
        if distance < closest_distance
            || (distance == closest_distance
                && (entry.g < closest_cost
                    || (entry.g == closest_cost && entry.pos < closest)))
        {
            closest = entry.pos;
            closest_distance = distance;
            closest_cost = entry.g;
        }
        if entry.pos == target {
            reached = true;
            break;
        }
        for node in ev.neighbors(world, entry.pos) {
            let pos = node.pos();
            if expanded.contains(&pos) {
                continue;
            }
            let cost = entry.g + pdist(entry.pos, pos) + f64::from(node.malus);
            let better = records.get(&pos).is_none_or(|old| cost < old.g);
            if better {
                records.insert(pos, NRec {
                    g: cost,
                    parent: Some(entry.pos),
                });
                let h = pdist(pos, target);
                heap.push(NEntry {
                    pos,
                    f: cost + h,
                    h,
                    g: cost,
                });
            }
        }
    }
    let mut nodes = Vec::new();
    let mut current = closest;
    loop {
        nodes.push(ev.accepted_node(world, current));
        match records.get(&current).and_then(|r| r.parent) {
            Some(parent) => current = parent,
            None => break,
        }
    }
    nodes.reverse();
    (nodes, reached)
}

// ===========================================================================
// PathFinderScratch tests.
// ===========================================================================

fn path_cases() -> Vec<(GridWorld, Vec<(BlockPos, BlockPos)>)> {
    let ev = NodeEvaluator::default();
    let mut suite = Vec::new();
    for seed in 0..50u64 {
        let world = gen_world(seed);
        let mut rng = Rng(seed ^ 0x5EED_1234);
        let mut pairs = Vec::new();
        for _ in 0..6 {
            let s = random_standable(&ev, &world, &mut rng);
            let t = random_standable(&ev, &world, &mut rng);
            pairs.push((s, t));
        }
        suite.push((world, pairs));
    }
    suite
}

#[test]
fn pathfinder_bit_exact_parity() {
    let _g = ALLOC_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let ev = NodeEvaluator::default();
    let mut pf = PathFinderScratch::new();
    let mut total = 0usize;
    for (world, pairs) in path_cases() {
        let rev = world.rev();
        for &(s, t) in &pairs {
            let (want_nodes, want_reached) =
                naive_find_path(&ev, &world, s, t, pf.max_visited_nodes);
            let got = pf.find_path(&world, s, t, rev);
            assert_eq!(
                got.nodes.len(),
                want_nodes.len(),
                "len mismatch s={s:?} t={t:?}"
            );
            assert_eq!(got.nodes, &want_nodes[..], "nodes s={s:?} t={t:?}");
            assert_eq!(got.reached, want_reached, "reached s={s:?} t={t:?}");
            assert_eq!(got.target, t);
            total += 1;
        }
    }
    assert_eq!(total, 300);
    eprintln!(
        "parity: {total} cases, fast_path hits = {}, astar runs = {}",
        pf.fast_path_hits, pf.astar_runs
    );
}

#[test]
fn pathfinder_unreachable_and_edges() {
    let _g = ALLOC_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let ev = NodeEvaluator::default();
    let mut pf = PathFinderScratch::new();

    // Wall the target inside a sealed box — must produce a closest-partial.
    let mut w = GridWorld {
        floor: HashMap::new(),
        solid: HashSet::new(),
        water: HashSet::new(),
    };
    let t = (5, 1, 5);
    for (dx, dz) in [
        (-1, -1),
        (-1, 0),
        (-1, 1),
        (0, -1),
        (0, 1),
        (1, -1),
        (1, 0),
        (1, 1),
    ] {
        w.solid.insert((t.0 + dx, 1, t.2 + dz));
        w.solid.insert((t.0 + dx, 2, t.2 + dz));
    }
    let s = (0, 1, 0);
    let (want_nodes, want_reached) = naive_find_path(&ev, &w, s, t, 1024);
    let got = pf.find_path(&w, s, t, w.rev());
    assert_eq!(got.nodes, &want_nodes[..]);
    assert_eq!(got.reached, want_reached);
    assert!(!got.reached, "sealed target must not be reached");

    // Non-standable start (a wall cell) -> empty path, reached=false.
    let bad = (t.0 - 1, 1, t.2);
    assert!(!ev.standable(&w, bad), "test setup: bad start must be blocked");
    let got = pf.find_path(&w, bad, s, w.rev());
    assert!(got.nodes.is_empty());
    assert!(!got.reached);
    let (want_nodes, want_reached) = naive_find_path(&ev, &w, bad, s, 1024);
    assert!(want_nodes.is_empty() && !want_reached);

    // start == target -> single-node path, reached.
    let got = pf.find_path(&w, s, s, w.rev());
    assert_eq!(got.nodes.len(), 1);
    assert!(got.reached);
    let (want_nodes, want_reached) = naive_find_path(&ev, &w, s, s, 1024);
    assert_eq!(got.nodes, &want_nodes[..]);
    assert_eq!(got.reached, want_reached);
}

#[test]
fn pathfinder_zero_alloc_after_warmup() {
    let _g = ALLOC_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut pf = PathFinderScratch::new();
    let suite = path_cases();
    // Warm every buffer to steady-state capacity.
    for (world, pairs) in &suite {
        let rev = world.rev();
        for &(s, t) in pairs {
            let p = pf.find_path(world, s, t, rev);
            std::hint::black_box(p.nodes.len());
        }
    }
    ALLOCS.store(0, Ordering::SeqCst);
    let mut lens = 0usize;
    for (world, pairs) in &suite {
        let rev = world.rev();
        for &(s, t) in pairs {
            let p = pf.find_path(world, s, t, rev);
            lens += p.nodes.len();
        }
    }
    std::hint::black_box(lens);
    let allocs = ALLOCS.load(Ordering::SeqCst);
    assert_eq!(
        allocs,
        0,
        "find_path allocated {allocs} times after warmup; sizes:{}",
        dump_alloc_sizes()
    );
}

#[test]
fn pathfinder_scratch_is_faster() {
    let _g = ALLOC_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let ev = NodeEvaluator::default();
    let mut pf = PathFinderScratch::new();
    let suite = path_cases();
    let mut naive_best = u128::MAX;
    let mut scratch_best = u128::MAX;
    let mut engine_best = u128::MAX;
    for _ in 0..3 {
        let t0 = Instant::now();
        let mut sink = 0usize;
        for (world, pairs) in &suite {
            for &(s, t) in pairs {
                let (n, r) = naive_find_path(&ev, world, s, t, pf.max_visited_nodes);
                sink += n.len() + usize::from(r);
            }
        }
        naive_best = naive_best.min(t0.elapsed().as_nanos());
        std::hint::black_box(sink);
        let t0 = Instant::now();
        for (world, pairs) in &suite {
            let rev = world.rev();
            for &(s, t) in pairs {
                let p = pf.find_path(world, s, t, rev);
                sink += p.nodes.len() + usize::from(p.reached);
            }
        }
        scratch_best = scratch_best.min(t0.elapsed().as_nanos());
        std::hint::black_box(sink);
        // Engine alone (certified shortcut off) vs the naive containers.
        pf.fast_path = false;
        let t0 = Instant::now();
        for (world, pairs) in &suite {
            for &(s, t) in pairs {
                let p = pf.find_path(world, s, t, 0);
                sink += p.nodes.len() + usize::from(p.reached);
            }
        }
        engine_best = engine_best.min(t0.elapsed().as_nanos());
        pf.fast_path = true;
        std::hint::black_box(sink);
    }
    eprintln!(
        "pathfinder: naive {naive_best}ns engine-only {engine_best}ns scratch {scratch_best}ns ({:.2}x / {:.2}x)",
        naive_best as f64 / engine_best.max(1) as f64,
        naive_best as f64 / scratch_best.max(1) as f64
    );
    assert!(
        scratch_best < naive_best,
        "scratch ({scratch_best}ns) not faster than naive ({naive_best}ns)"
    );
}

// ===========================================================================
// ReusableCellMerger tests.
// ============================================================================

#[derive(Clone, Debug, PartialEq)]
struct Txp {
    eid: i32,
    pos: [f64; 3],
    val: i16,
    age: i32,
}

impl SimXp for Txp {
    fn eid(&self) -> i32 {
        self.eid
    }
    fn pos(&self) -> [f64; 3] {
        self.pos
    }
    fn age(&self) -> i32 {
        self.age
    }
    fn value(&self) -> i16 {
        self.val
    }
    fn set_value(&mut self, val: i16) {
        self.val = val;
    }
}

fn naive_merge_orbs(xl: &mut [Txp], dims: &mut HashMap<i32, u64>) -> Vec<i32> {
    let mut merged = Vec::new();
    for i in 0..xl.len() {
        for j in (i + 1)..xl.len() {
            let (ax, ay, az, aage) = (xl[i].pos[0], xl[i].pos[1], xl[i].pos[2], xl[i].age);
            let (bx, by, bz, bage) = (xl[j].pos[0], xl[j].pos[1], xl[j].pos[2], xl[j].age);
            if (ax - bx).powi(2) + (ay - by).powi(2) + (az - bz).powi(2) < 1.0
                && dims.get(&xl[i].eid).copied().unwrap_or(0)
                    == dims.get(&xl[j].eid).copied().unwrap_or(0)
            {
                let (keep, eat) = if aage <= bage { (i, j) } else { (j, i) };
                xl[keep].val = xl[keep].val.saturating_add(xl[eat].val);
                merged.push(xl[eat].eid);
                dims.remove(&xl[eat].eid);
                break;
            }
        }
    }
    merged
}

fn orb(rng: &mut Rng, eid: i32, cluster: [f64; 3], jitter: f64) -> Txp {
    Txp {
        eid,
        pos: [
            cluster[0] + (rng.below(1000) as f64 / 1000.0 - 0.5) * jitter,
            cluster[1] + (rng.below(1000) as f64 / 1000.0 - 0.5) * jitter,
            cluster[2] + (rng.below(1000) as f64 / 1000.0 - 0.5) * jitter,
        ],
        val: (rng.below(120) + 1) as i16,
        age: rng.below(200) as i32,
    }
}

#[test]
fn merger_orbs_bit_exact() {
    let _g = ALLOC_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut rng = Rng(0x0B5EED);
    let mut merger = ReusableCellMerger::new();
    for round in 0..40 {
        let mut orbs = Vec::new();
        let clusters = 2 + rng.below(4) as usize;
        for _ in 0..clusters {
            let c = [
                rng.below(200) as f64,
                64.0,
                rng.below(200) as f64,
            ];
            let n = 1 + rng.below(12) as usize;
            for _ in 0..n {
                orbs.push(orb(&mut rng, orbs.len() as i32, c, 0.6));
            }
        }
        let mut dims_ref = HashMap::new();
        let mut dims_kernel = HashMap::new();
        for x in &orbs {
            if rng.below(100) < 25 {
                let d = rng.below(3);
                dims_ref.insert(x.eid, d);
                dims_kernel.insert(x.eid, d);
            }
        }
        let mut a = orbs.clone();
        let want = naive_merge_orbs(&mut a, &mut dims_ref);
        let mut b = orbs.clone();
        let dm = std::cell::RefCell::new(dims_kernel.clone());
        let got: Vec<i32> = {
            let m = merger.merge_orbs(
                &mut b,
                |eid| dm.borrow().get(&eid).copied().unwrap_or(0),
                |eid| {
                    dm.borrow_mut().remove(&eid);
                },
            );
            m.to_vec()
        };
        let dm = dm.into_inner();
        assert_eq!(got, want, "merged order round {round}");
        assert_eq!(a, b, "orb state round {round}");
        assert_eq!(dims_ref, dm, "dim side-table round {round}");
    }
}

#[test]
fn merger_orbs_dead_orb_duplicates() {
    let _g = ALLOC_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    // Orb 1 sits too far from orb 0 to be its merge partner but within
    // reach of orb 2 — so orb 2 gets eaten by orb 0 AND re-eaten by orb 1
    // (its dim was reset to default by the first eat).  `merged` carries
    // eid 2 twice, and orb 2's value is absorbed into *both* keepers.
    let mut merger = ReusableCellMerger::new();
    let orbs = vec![
        Txp { eid: 0, pos: [10.0, 64.0, 10.0], val: 5, age: 1 },
        Txp { eid: 1, pos: [11.1, 64.0, 10.0], val: 7, age: 2 },
        Txp { eid: 2, pos: [10.4, 64.0, 10.0], val: 9, age: 3 },
    ];
    let mut dims: HashMap<i32, u64> = HashMap::new();
    let mut a = orbs.clone();
    let want = naive_merge_orbs(&mut a, &mut dims);
    let mut b = orbs;
    let dm = std::cell::RefCell::new(HashMap::<i32, u64>::new());
    let got = merger
        .merge_orbs(
            &mut b,
            |eid| dm.borrow().get(&eid).copied().unwrap_or(0),
            |eid| {
                dm.borrow_mut().remove(&eid);
            },
        )
        .to_vec();
    assert_eq!(want, vec![2, 2], "sanity: reference produces the duplicate");
    assert_eq!(got, want);
    assert_eq!(a, b);
    assert_eq!(a[0].val, 14);
    assert_eq!(a[1].val, 16);
    assert_eq!(a[2].val, 9, "eaten orb keeps its own value");
}

#[derive(Clone, Debug, PartialEq)]
struct Tit {
    eid: i32,
    pos: [f64; 3],
    item: i64,
    key: u64,
    cnt: u32,
    max: u32,
    age: i32,
}

static SAME_STACK_OPS: AtomicUsize = AtomicUsize::new(0);

impl SimItem for Tit {
    fn eid(&self) -> i32 {
        self.eid
    }
    fn pos(&self) -> [f64; 3] {
        self.pos
    }
    fn age(&self) -> i32 {
        self.age
    }
    fn count(&self) -> u32 {
        self.cnt
    }
    fn set_count(&mut self, cnt: u32) {
        self.cnt = cnt;
    }
    fn stack_cap(&self) -> u32 {
        self.max
    }
    fn same_stack(&self, other: &Self) -> bool {
        SAME_STACK_OPS.fetch_add(1, Ordering::Relaxed);
        self.item == other.item && self.key == other.key
    }
}

fn naive_merge_items(dp: &mut [Tit]) -> Vec<i32> {
    let mut merged = Vec::new();
    for i in 0..dp.len() {
        for j in (i + 1)..dp.len() {
            let (pa, pb) = (dp[i].pos, dp[j].pos);
            if dp[i].same_stack(&dp[j])
                && (pa[0] - pb[0]).powi(2) + (pa[1] - pb[1]).powi(2) + (pa[2] - pb[2]).powi(2)
                    < 0.25
                && dp[i].cnt + dp[j].cnt <= dp[i].max
            {
                let (keep, eat) = if dp[i].age <= dp[j].age { (i, j) } else { (j, i) };
                dp[keep].cnt = dp[keep].cnt.saturating_add(dp[eat].cnt);
                merged.push(dp[eat].eid);
                break;
            }
        }
    }
    merged
}

#[test]
fn merger_items_bit_exact() {
    let _g = ALLOC_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut rng = Rng(0x1DEA);
    let mut merger = ReusableCellMerger::new();
    for round in 0..40 {
        let mut drops = Vec::new();
        let groups = 2 + rng.below(4) as usize;
        for g in 0..groups {
            let c = [rng.below(200) as f64, 64.0, rng.below(200) as f64];
            let item = g as i64;
            let key = rng.below(4);
            let n = 1 + rng.below(10) as usize;
            for _ in 0..n {
                drops.push(Tit {
                    eid: drops.len() as i32,
                    pos: [
                        c[0] + (rng.below(1000) as f64 / 1000.0 - 0.5) * 0.4,
                        c[1],
                        c[2] + (rng.below(1000) as f64 / 1000.0 - 0.5) * 0.4,
                    ],
                    item,
                    key,
                    cnt: (rng.below(30) + 1) as u32,
                    max: 64,
                    age: rng.below(200) as i32,
                });
            }
        }
        let mut a = drops.clone();
        let want = naive_merge_items(&mut a);
        let mut b = drops.clone();
        let got = merger.merge_items(&mut b).to_vec();
        assert_eq!(got, want, "merged order round {round}");
        assert_eq!(a, b, "drop state round {round}");
    }
}

#[test]
fn merger_items_full_stack_prefilter() {
    let _g = ALLOC_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut rng = Rng(0xF011);
    let mut merger = ReusableCellMerger::new();
    // 5,000 full stacks scattered across a floor plus a handful of partials
    // that really do merge.
    let mut drops: Vec<Tit> = (0..5000)
        .map(|i| Tit {
            eid: i,
            pos: [
                (i % 100) as f64 * 0.2,
                64.0,
                (i / 100) as f64 * 0.2,
            ],
            item: 7,
            key: 1,
            cnt: 64,
            max: 64,
            age: i,
        })
        .collect();
    for k in 0..6 {
        drops.push(Tit {
            eid: 5000 + k,
            pos: [2.0 + k as f64 * 0.05, 64.0, 2.0],
            item: 9,
            key: 2,
            cnt: 10,
            max: 64,
            age: 100 + k,
        });
    }
    // Fisher-Yates shuffle for determinism.
    for i in (1..drops.len()).rev() {
        let j = rng.below((i + 1) as u64) as usize;
        drops.swap(i, j);
    }
    let mut a = drops.clone();
    SAME_STACK_OPS.store(0, Ordering::SeqCst);
    let want = naive_merge_items(&mut a);
    let naive_ops = SAME_STACK_OPS.load(Ordering::SeqCst);
    let mut b = drops.clone();
    SAME_STACK_OPS.store(0, Ordering::SeqCst);
    let got = merger.merge_items(&mut b).to_vec();
    let kernel_ops = SAME_STACK_OPS.load(Ordering::SeqCst);
    assert_eq!(got, want);
    assert_eq!(a, b);
    eprintln!(
        "full-stack prefilter: naive same_stack ops {naive_ops}, kernel {kernel_ops}"
    );
    assert!(
        kernel_ops * 100 < naive_ops,
        "kernel did {kernel_ops} ops vs naive {naive_ops}"
    );

    // With a count==0 stack present the filter is off — still bit-exact.
    drops.push(Tit {
        eid: 9000,
        pos: [2.0, 64.0, 2.0],
        item: 9,
        key: 2,
        cnt: 0,
        max: 64,
        age: 3,
    });
    let mut a = drops.clone();
    let want = naive_merge_items(&mut a);
    let mut b = drops.clone();
    let got = merger.merge_items(&mut b).to_vec();
    assert_eq!(got, want);
    assert_eq!(a, b);
}

fn naive_merge_pending(dp: &mut [Tit], pending: &HashSet<i32>) -> Vec<i32> {
    const CELL: f64 = 2.0;
    let mut pool: Vec<usize> = (0..dp.len())
        .filter(|&i| !pending.contains(&dp[i].eid) && dp[i].cnt > 0)
        .collect();
    pool.extend(
        (0..dp.len()).filter(|&i| pending.contains(&dp[i].eid) && dp[i].cnt > 0),
    );
    if pool.len() < 2 {
        return Vec::new();
    }
    let mut cnts: Vec<u32> = pool.iter().map(|&i| dp[i].cnt).collect();
    let maxes: Vec<u32> = pool.iter().map(|&i| dp[i].max.max(1)).collect();
    let mut cells: HashMap<(i32, i32, i32), Vec<usize>> = HashMap::new();
    for i in 0..pool.len() {
        if cnts[i] == 0 {
            continue;
        }
        let p = dp[pool[i]].pos;
        let cell = (
            (p[0] / CELL).floor() as i32,
            (p[1] / CELL).floor() as i32,
            (p[2] / CELL).floor() as i32,
        );
        let mut cand = Vec::new();
        for dx in -1..=1i32 {
            for dy in -1..=1i32 {
                for dz in -1..=1i32 {
                    if let (Some(kx), Some(ky), Some(kz)) = (
                        cell.0.checked_add(dx),
                        cell.1.checked_add(dy),
                        cell.2.checked_add(dz),
                    ) {
                        if let Some(m) = cells.get(&(kx, ky, kz)) {
                            cand.extend(m.iter().copied());
                        }
                    }
                }
            }
        }
        cand.sort_unstable();
        let mut alive = true;
        for j in cand {
            let (pa, pb) = (dp[pool[i]].pos, dp[pool[j]].pos);
            if dp[pool[i]].item == dp[pool[j]].item
                && dp[pool[i]].key == dp[pool[j]].key
                && (pa[0] - pb[0]).powi(2) + (pa[1] - pb[1]).powi(2) + (pa[2] - pb[2]).powi(2)
                    <= 0.25
            {
                let max = maxes[j].min(maxes[i]);
                let transfer = cnts[i].min(max.saturating_sub(cnts[j]));
                cnts[j] += transfer;
                cnts[i] -= transfer;
                if cnts[i] == 0 {
                    alive = false;
                    break;
                }
            }
        }
        if alive && cnts[i] < maxes[i] {
            cells.entry(cell).or_default().push(i);
        }
    }
    let mut order = Vec::new();
    let mut seen = HashSet::new();
    for (k, &i) in pool.iter().enumerate() {
        if cnts[k] == 0 {
            if seen.insert(dp[i].eid) {
                order.push(dp[i].eid);
            }
        } else {
            dp[i].cnt = cnts[k].min(u32::from(u8::MAX));
        }
    }
    order
}

#[test]
fn merger_pending_bit_exact() {
    let _g = ALLOC_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut rng = Rng(0xBEEF);
    let mut merger = ReusableCellMerger::new();
    for round in 0..40 {
        let mut drops = Vec::new();
        let mut pending = HashSet::new();
        let groups = 2 + rng.below(4) as usize;
        for g in 0..groups {
            let c = [rng.below(60) as f64, 64.0, rng.below(60) as f64];
            let n = 1 + rng.below(14) as usize;
            for _ in 0..n {
                let eid = drops.len() as i32;
                drops.push(Tit {
                    eid,
                    pos: [
                        c[0] + (rng.below(1000) as f64 / 1000.0 - 0.5) * 1.5,
                        c[1],
                        c[2] + (rng.below(1000) as f64 / 1000.0 - 0.5) * 1.5,
                    ],
                    item: g as i64,
                    key: 1,
                    cnt: (rng.below(40) + 1) as u32,
                    max: 64,
                    age: rng.below(200) as i32,
                });
                if rng.below(100) < 40 {
                    pending.insert(eid);
                }
            }
        }
        let mut a = drops.clone();
        let want = naive_merge_pending(&mut a, &pending);
        let mut b = drops.clone();
        let got = merger
            .merge_pending_items(&mut b, |eid| pending.contains(&eid))
            .to_vec();
        assert_eq!(got, want, "eaten order round {round}");
        assert_eq!(a, b, "drop state round {round}");
    }
}

#[test]
fn merger_zero_alloc_after_warmup() {
    let _g = ALLOC_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut rng = Rng(0xA110C);
    let mut merger = ReusableCellMerger::new();
    let mut sets: Vec<Vec<Txp>> = Vec::new();
    let mut dsets: Vec<Vec<Tit>> = Vec::new();
    for _ in 0..8 {
        let mut orbs = Vec::new();
        for _ in 0..200 {
            orbs.push(orb(&mut rng, orbs.len() as i32, [5.0, 64.0, 5.0], 8.0));
        }
        sets.push(orbs);
        let mut drops = Vec::new();
        for i in 0..300 {
            drops.push(Tit {
                eid: i,
                pos: [(i % 50) as f64 * 0.1, 64.0, (i / 50) as f64 * 0.1],
                item: 1,
                key: 1,
                cnt: (rng.below(30) + 1) as u32,
                max: 64,
                age: i,
            });
        }
        dsets.push(drops);
    }
    for orbs in &mut sets {
        let dm = std::cell::RefCell::new(HashMap::<i32, u64>::new());
        merger.merge_orbs(
            orbs,
            |eid| dm.borrow().get(&eid).copied().unwrap_or(0),
            |eid| {
                dm.borrow_mut().remove(&eid);
            },
        );
    }
    for drops in &mut dsets {
        merger.merge_items(drops);
    }
    ALLOCS.store(0, Ordering::SeqCst);
    for orbs in &mut sets {
        let dm = std::cell::RefCell::new(HashMap::<i32, u64>::new());
        merger.merge_orbs(
            orbs,
            |eid| dm.borrow().get(&eid).copied().unwrap_or(0),
            |eid| {
                dm.borrow_mut().remove(&eid);
            },
        );
    }
    for drops in &mut dsets {
        merger.merge_items(drops);
    }
    let allocs = ALLOCS.load(Ordering::SeqCst);
    assert_eq!(allocs, 0, "merger allocated {allocs} times after warmup");
}

#[test]
fn merger_scratch_is_faster() {
    let _g = ALLOC_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut rng = Rng(0x5EEDBEEF);
    let mut merger = ReusableCellMerger::new();
    let mut orbs = Vec::new();
    for i in 0..2000 {
        orbs.push(orb(&mut rng, i, [10.0, 64.0, 10.0], 30.0));
    }
    let mut naive_best = u128::MAX;
    let mut kernel_best = u128::MAX;
    for _ in 0..5 {
        let mut a = orbs.clone();
        let t0 = Instant::now();
        let mut dm = HashMap::new();
        let got = naive_merge_orbs(&mut a, &mut dm);
        naive_best = naive_best.min(t0.elapsed().as_nanos());
        std::hint::black_box(got);
        let mut b = orbs.clone();
        let t0 = Instant::now();
        let dm = std::cell::RefCell::new(HashMap::<i32, u64>::new());
        let got = merger
            .merge_orbs(
                &mut b,
                |eid| dm.borrow().get(&eid).copied().unwrap_or(0),
                |eid| {
                    dm.borrow_mut().remove(&eid);
                },
            )
            .to_vec();
        kernel_best = kernel_best.min(t0.elapsed().as_nanos());
        std::hint::black_box(got);
    }
    eprintln!(
        "orb merge: naive {naive_best}ns kernel {kernel_best}ns speedup {:.2}x",
        naive_best as f64 / kernel_best.max(1) as f64
    );
    assert!(
        kernel_best < naive_best,
        "kernel ({kernel_best}ns) not faster than naive ({naive_best}ns)"
    );
}

// ===========================================================================
// RestingEntitySleepState tests.
// ===========================================================================

/// Section revision map: block cell -> section coords -> rev.
struct RevMap {
    revs: HashMap<(i32, i32, i32), u64>,
}

impl RevMap {
    fn new() -> Self {
        Self { revs: HashMap::new() }
    }
    fn rev(&mut self, x: i32, y: i32, z: i32) -> u64 {
        self.revs
            .get(&(x.div_euclid(16), y.div_euclid(16), z.div_euclid(16)))
            .copied()
            .unwrap_or(0)
    }
    fn bump_block(&mut self, x: i32, y: i32, z: i32) {
        *self
            .revs
            .entry((x.div_euclid(16), y.div_euclid(16), z.div_euclid(16)))
            .or_insert(0) += 1;
    }
    fn closure(&mut self) -> impl FnMut(i32, i32, i32) -> u64 + '_ {
        |x, y, z| self.rev(x, y, z)
    }
}

struct RestEnt {
    eid: i32,
    pos: [f64; 3],
    vel: [f64; 3],
    on_ground: bool,
    in_fluid: bool,
    impulse: bool,
    age: i32,
    collision_q: u64,
    fluid_q: u64,
    despawned: bool,
}

impl RestEnt {
    fn item(eid: i32, pos: [f64; 3]) -> Self {
        Self {
            eid,
            pos,
            vel: [0.0; 3],
            on_ground: true,
            in_fluid: false,
            impulse: false,
            age: 0,
            collision_q: 0,
            fluid_q: 0,
            despawned: false,
        }
    }

    /// One full entity tick; returns whether the entity ran its spatial
    /// queries this tick.
    fn tick(&mut self, sleep: &mut RestingEntitySleepState, rev: &mut RevMap) -> bool {
        let verdict = {
            let mut revf = rev.closure();
            sleep.tick(
                self.eid, self.pos, self.on_ground, self.vel, self.impulse, &mut revf,
            )
        };
        let awake = match verdict {
            RestVerdict::Asleep => false,
            RestVerdict::Awake(_) => {
                self.collision_q += 1;
                self.fluid_q += 1;
                // The "physics" an awake tick performs.
                if self.impulse {
                    self.vel = [0.1, 0.2, 0.0];
                    self.impulse = false;
                    self.on_ground = false;
                } else if self.vel != [0.0; 3] {
                    self.pos[0] += self.vel[0];
                    self.pos[1] += self.vel[1];
                    self.pos[2] += self.vel[2];
                    if self.pos[1] <= self.pos[1].floor() + 0.001 {
                        self.vel = [0.0; 3];
                        self.on_ground = true;
                    }
                }
                {
                    let mut revf2 = rev.closure();
                    sleep.offer_sleep(
                        self.eid,
                        self.pos,
                        self.in_fluid,
                        self.on_ground,
                        self.vel,
                        &mut revf2,
                    );
                }
                true
            }
        };
        self.age += 1;
        if self.age >= 6000 {
            self.despawned = true;
        }
        awake
    }
}

#[test]
fn sleep_skips_queries_and_wakes_on_rev() {
    let _g = ALLOC_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut sleep = RestingEntitySleepState::new(64);
    let mut rev = RevMap::new();
    let mut e = RestEnt::item(1, [3.2, 65.0, 3.4]);
    // First tick is a full awake pass that offers sleep.
    assert!(e.tick(&mut sleep, &mut rev));
    assert_eq!(sleep.sleeping_count(), 1);
    // Sleep ticks run zero spatial queries.
    let (cq, fq) = (e.collision_q, e.fluid_q);
    for _ in 0..50 {
        assert!(!e.tick(&mut sleep, &mut rev), "unexpected wake");
    }
    assert_eq!(e.collision_q, cq);
    assert_eq!(e.fluid_q, fq);
    assert!(sleep.ticks_skipped >= 50);
    assert_eq!(e.age, 51);

    // A block break under the entity bumps its section rev -> wake the
    // exact same tick.
    rev.bump_block(3, 64, 3);
    let verdict = {
        let mut revf = rev.closure();
        sleep.tick(e.eid, e.pos, e.on_ground, e.vel, false, &mut revf)
    };
    assert_eq!(verdict, RestVerdict::Awake(WakeReason::RevChanged));
    assert_eq!(sleep.sleeping_count(), 0);
}

#[test]
fn sleep_wakes_on_impulse_and_move() {
    let _g = ALLOC_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut sleep = RestingEntitySleepState::new(16);
    let mut rev = RevMap::new();
    let mut e = RestEnt::item(7, [0.5, 65.0, 0.5]);
    e.tick(&mut sleep, &mut rev);
    assert_eq!(sleep.sleeping_count(), 1);
    // Impulse: wakes on the spot with the Impulse reason.
    e.impulse = true;
    let verdict = {
        let mut revf = rev.closure();
        sleep.tick(e.eid, e.pos, e.on_ground, e.vel, e.impulse, &mut revf)
    };
    assert_eq!(verdict, RestVerdict::Awake(WakeReason::Impulse));
    assert_eq!(sleep.sleeping_count(), 0);
    // The entity now moves; it must stay awake until velocity is zero.
    for _ in 0..5 {
        assert!(e.tick(&mut sleep, &mut rev));
    }
    assert_eq!(sleep.sleeping_count(), 0);
    // Settle again -> sleeps again on the new cell.
    e.vel = [0.0; 3];
    e.on_ground = true;
    e.pos[1] = 65.0;
    e.tick(&mut sleep, &mut rev);
    assert_eq!(sleep.sleeping_count(), 1);
}

#[test]
fn sleep_fluid_blocks_sleep_and_arrival_wakes() {
    let _g = ALLOC_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut sleep = RestingEntitySleepState::new(16);
    let mut rev = RevMap::new();
    let mut e = RestEnt::item(9, [8.5, 65.0, 8.5]);
    e.tick(&mut sleep, &mut rev);
    assert_eq!(sleep.sleeping_count(), 1);
    // Fluid arrives in a neighbouring cell -> section rev bump -> wake on
    // the exact tick.
    rev.bump_block(9, 65, 8);
    let verdict = {
        let mut revf = rev.closure();
        sleep.tick(e.eid, e.pos, e.on_ground, e.vel, false, &mut revf)
    };
    assert_eq!(verdict, RestVerdict::Awake(WakeReason::RevChanged));
    // While in fluid the entity is refused sleep.
    e.in_fluid = true;
    {
        let mut revf = rev.closure();
        sleep.offer_sleep(e.eid, e.pos, e.in_fluid, e.on_ground, e.vel, &mut revf);
    }
    assert_eq!(sleep.sleeping_count(), 0);
    // Once it drains it can sleep again.
    e.in_fluid = false;
    e.tick(&mut sleep, &mut rev);
    assert_eq!(sleep.sleeping_count(), 1);
}

#[test]
fn sleep_despawn_still_fires_at_6000() {
    let _g = ALLOC_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut sleep = RestingEntitySleepState::new(16);
    let mut rev = RevMap::new();
    let mut e = RestEnt::item(42, [1.5, 65.0, 1.5]);
    e.age = 5990;
    e.tick(&mut sleep, &mut rev);
    assert_eq!(sleep.sleeping_count(), 1);
    // The remaining ticks run asleep — zero queries — and the despawn
    // deadline is honored on the exact tick.
    let (cq, fq) = (e.collision_q, e.fluid_q);
    for _ in 0..9 {
        e.tick(&mut sleep, &mut rev);
    }
    assert!(e.despawned, "despawn must fire at age 6000 while asleep");
    assert_eq!(e.age, 6000);
    assert_eq!(e.collision_q, cq);
    assert_eq!(e.fluid_q, fq);
}
