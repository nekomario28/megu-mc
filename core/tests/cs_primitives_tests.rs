// SPDX-License-Identifier: LGPL-3.0-or-later
//! Unit and stress tests for the Phase 0C CS primitives:
//! `intern` (StringInterner + GenArena), `bvh` (DynamicBvh), `dep_graph`
//! (IncrementalDag), `bloom` (BlockedBloomFilter + VoxelBitset4096) and
//! `ring_spsc` (WaitFreeSpscU64 + spsc_channel + BoundedRing).

use std::collections::BTreeSet;

use megu_core::bloom::{BlockedBloomFilter, VoxelBitset4096};
use megu_core::bvh::{Aabb3, BvhNodeId, DynamicBvh};
use megu_core::dep_graph::{CutoffOutcome, DagError, IncrementalDag};
use megu_core::intern::{GenArena, StringInterner};
use megu_core::ring_spsc::{spsc_channel, spsc_u64, BoundedRing};

/// Deterministic splitmix64 PRNG — reproducible across machines, no
/// wall-clock entropy (the repo ratchet forbids `SystemTime` anyway).
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed)
    }
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }
    /// Uniform float in `[lo, hi)`.
    fn f32(&mut self, lo: f32, hi: f32) -> f32 {
        let unit = (self.next() >> 40) as f32 / (1u64 << 24) as f32;
        lo + unit * (hi - lo)
    }
    fn usize(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }
}

fn rand_aabb(rng: &mut Rng, lo: f32, hi: f32, max_extent: f32) -> Aabb3 {
    let mut min = [0.0; 3];
    let mut max = [0.0; 3];
    for axis in 0..3 {
        let a = rng.f32(lo, hi);
        let b = a + rng.f32(0.01, max_extent);
        min[axis] = a;
        max[axis] = b;
    }
    Aabb3::new(min, max)
}

// ---------------------------------------------------------------------------
// intern.rs — StringInterner
// ---------------------------------------------------------------------------

#[test]
fn interner_deduplicates_and_resolves() {
    let mut interner = StringInterner::new();
    let a = interner.intern("minecraft:stone");
    let b = interner.intern("minecraft:dirt");
    let a2 = interner.intern("minecraft:stone");
    assert_eq!(a, a2);
    assert_ne!(a, b);
    assert_eq!(interner.len(), 2);
    assert_eq!(interner.resolve(a), Some("minecraft:stone"));
    assert_eq!(interner.resolve(b), Some("minecraft:dirt"));
}

#[test]
fn interner_get_is_read_only_lookup() {
    let mut interner = StringInterner::new();
    assert_eq!(interner.get("missing"), None);
    let sym = interner.intern("mod:biome/crystal_caves");
    assert_eq!(interner.get("mod:biome/crystal_caves"), Some(sym));
    assert_eq!(interner.len(), 1, "get must not intern");
    assert_eq!(interner.get("mod:biome/crystal_caves_extra"), None);
}

#[test]
fn interner_handles_empty_unicode_and_many_strings() {
    let mut interner = StringInterner::new();
    let empty = interner.intern("");
    assert_eq!(interner.resolve(empty), Some(""));
    let jp = interner.intern("こんにちは世界");
    assert_eq!(interner.resolve(jp), Some("こんにちは世界"));
    for i in 0..5_000u32 {
        let s = format!("key_{i}_{}", i.wrapping_mul(7));
        let sym = interner.intern(&s);
        assert_eq!(interner.resolve(sym).unwrap(), s);
    }
    assert_eq!(interner.len(), 5_002);
    // Re-interning every string returns the same symbols, no growth.
    for i in 0..5_000u32 {
        let s = format!("key_{i}_{}", i.wrapping_mul(7));
        interner.intern(&s);
    }
    assert_eq!(interner.len(), 5_002);
}

#[test]
fn interner_resolve_rejects_foreign_symbols() {
    let mut a = StringInterner::new();
    let _ = a.intern("x");
    // Symbols beyond len are always rejected.
    assert_eq!(a.resolve(megu_core::intern::Symbol(9_999)), None);
}

// ---------------------------------------------------------------------------
// intern.rs — GenArena
// ---------------------------------------------------------------------------

#[test]
fn gen_arena_insert_get_remove() {
    let mut arena: GenArena<String> = GenArena::new();
    let id = arena.insert("entity:zombie".to_string());
    assert_eq!(arena.get(id).map(String::as_str), Some("entity:zombie"));
    *arena.get_mut(id).unwrap() = "entity:husk".to_string();
    assert_eq!(arena.get(id).map(String::as_str), Some("entity:husk"));
    assert_eq!(arena.remove(id).unwrap(), "entity:husk");
    assert_eq!(arena.get(id), None);
    assert_eq!(arena.remove(id), None, "double remove must fail");
    assert!(arena.is_empty());
}

#[test]
fn gen_arena_prevents_aba_across_10k_churn_cycles() {
    let mut arena: GenArena<u32> = GenArena::new();
    let mut stale_ids = Vec::new();
    for cycle in 0..10_000u32 {
        let id = arena.insert(cycle);
        let dead = arena.insert(cycle | 0x8000_0000);
        assert_eq!(arena.remove(dead), Some(cycle | 0x8000_0000));
        stale_ids.push(dead);
        // The live handle still works.
        assert_eq!(arena.get(id), Some(&cycle));
        // The retired handle can never alias the recycled slot even when
        // the slot index is reused — the generation moved on.
        assert_eq!(arena.get(dead), None, "ABA violation at cycle {cycle}");
        assert!(!arena.contains(dead));
        assert_eq!(arena.remove(id), Some(cycle));
        assert!(arena.is_empty());
    }
    // Every retired handle stays dead even after the arena refills.
    for i in 0..stale_ids.len() {
        arena.insert(i as u32);
    }
    for dead in &stale_ids {
        assert_eq!(arena.get(*dead), None);
    }
}

#[test]
fn gen_arena_iterates_only_live_pairs() {
    let mut arena: GenArena<u32> = GenArena::new();
    let mut live = Vec::new();
    for i in 0..100u32 {
        live.push(arena.insert(i));
    }
    // Remove every third handle.
    let mut dead = Vec::new();
    for (i, id) in live.iter().enumerate() {
        if i % 3 == 0 {
            dead.push(*id);
            arena.remove(*id);
        }
    }
    live.retain(|id| !dead.contains(id));
    let seen: BTreeSet<_> = arena.iter().map(|(id, _)| id).collect();
    assert_eq!(seen, live.iter().copied().collect::<BTreeSet<_>>());
    // Mutating through iter_mut lands on the right values.
    for (id, v) in arena.iter_mut() {
        *v += id.index;
    }
    let total: u64 = arena.iter().map(|(_, v)| u64::from(*v)).sum();
    assert_eq!(
        total,
        live.iter()
            .map(|id| u64::from(id.index) * 2)
            .sum::<u64>()
    );
}

// ---------------------------------------------------------------------------
// bvh.rs — Aabb3
// ---------------------------------------------------------------------------

#[test]
fn aabb3_union_surface_area_and_containment() {
    let a = Aabb3::new([0.0, 0.0, 0.0], [1.0, 1.0, 1.0]);
    let b = Aabb3::new([1.0, 0.0, 0.0], [3.0, 2.0, 1.0]);
    let u = a.union(&b);
    assert_eq!(u.min, [0.0, 0.0, 0.0]);
    assert_eq!(u.max, [3.0, 2.0, 1.0]);
    assert_eq!(u.surface_area(), 22.0);
    assert!(a.intersects(&b), "touching faces count as intersecting");
    assert!(u.contains_aabb(&a));
    assert!(!a.contains_aabb(&b));
    assert!(a.contains_point([0.5, 0.5, 0.5]));
    assert!(!a.contains_point([1.5, 0.5, 0.5]));
}

#[test]
fn aabb3_ray_slab_test_including_zero_dir_and_nan() {
    let b = Aabb3::new([1.0, 1.0, 1.0], [2.0, 2.0, 2.0]);
    // Straight +X hit: enters at x=1, t=1.
    let t = b.ray_intersects([0.0, 1.5, 1.5], [1.0, f32::INFINITY, f32::INFINITY], 10.0);
    assert_eq!(t, Some(1.0));
    // Miss: parallel to the slab, origin outside the x range... actually
    // outside on y: dir.y == 0 and origin.y below min → miss.
    assert_eq!(
        b.ray_intersects([0.0, 0.5, 1.5], [1.0, f32::INFINITY, f32::INFINITY], 10.0),
        None
    );
    // Too short max_t.
    assert_eq!(
        b.ray_intersects([0.0, 1.5, 1.5], [1.0, f32::INFINITY, f32::INFINITY], 0.5),
        None
    );
    // Origin inside the box: t == 0.
    assert_eq!(
        b.ray_intersects([1.5, 1.5, 1.5], [1.0, 0.0, 0.0], 10.0),
        Some(0.0)
    );
    // NaN bounds never match.
    let nan = Aabb3::new([f32::NAN; 3], [f32::NAN; 3]);
    assert!(!nan.intersects(&b));
    assert!(nan.ray_intersects([0.0; 3], [1.0, 1.0, 1.0], 100.0).is_none());
}

// ---------------------------------------------------------------------------
// bvh.rs — DynamicBvh vs brute-force oracle
// ---------------------------------------------------------------------------

/// Reference model: `slot = Some(tight aabb)` while the payload lives.
fn brute_aabb(model: &[Option<Aabb3>], query: &Aabb3) -> Vec<usize> {
    let mut hits: Vec<usize> = model
        .iter()
        .enumerate()
        .filter_map(|(i, s)| s.filter(|a| a.intersects(query)).map(|_| i))
        .collect();
    hits.sort_unstable();
    hits
}

fn brute_ray(model: &[Option<Aabb3>], o: [f32; 3], d: [f32; 3], max_t: f32) -> Vec<(usize, f32)> {
    let inv = [1.0 / d[0], 1.0 / d[1], 1.0 / d[2]];
    let mut hits: Vec<(usize, f32)> = model
        .iter()
        .enumerate()
        .filter_map(|(i, s)| s.and_then(|a| a.ray_intersects(o, inv, max_t).map(|t| (i, t))))
        .collect();
    hits.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap().then(a.0.cmp(&b.0)));
    hits
}

#[test]
fn dynamic_bvh_matches_brute_force_over_1000_boxes() {
    let mut rng = Rng::new(0xC0FFEE);
    let mut tree: DynamicBvh<usize> = DynamicBvh::with_margin(0.5);
    let mut model: Vec<Option<Aabb3>> = Vec::new();
    let mut handles: Vec<Option<BvhNodeId>> = Vec::new();

    // 1000 inserts.
    for i in 0..1_000usize {
        let aabb = rand_aabb(&mut rng, -100.0, 100.0, 10.0);
        let id = tree.insert(aabb, i);
        model.push(Some(aabb));
        handles.push(Some(id));
    }
    assert_eq!(tree.len(), 1_000);

    // 200 mixed updates and removals; BvhNodeId stays stable across moves.
    for _ in 0..200 {
        let i = rng.usize(model.len());
        match rng.usize(3) {
            0 if handles[i].is_some() => {
                let new_aabb = rand_aabb(&mut rng, -100.0, 100.0, 10.0);
                tree.update(handles[i].unwrap(), new_aabb);
                model[i] = Some(new_aabb);
                assert_eq!(tree.get(handles[i].unwrap()), Some(&i));
            }
            1 if handles[i].is_some() => {
                assert_eq!(tree.remove(handles[i].unwrap()), Some(i));
                model[i] = None;
                handles[i] = None;
            }
            _ => {
                let aabb = rand_aabb(&mut rng, -100.0, 100.0, 10.0);
                model.push(Some(aabb));
                handles.push(Some(tree.insert(aabb, model.len() - 1)));
            }
        }
    }

    // AABB queries must equal the brute-force scan exactly.
    let mut out = Vec::new();
    for _ in 0..200 {
        let q = rand_aabb(&mut rng, -120.0, 120.0, 40.0);
        out.clear();
        tree.query_aabb(q, &mut out);
        let mut got: Vec<usize> = out.iter().map(|(_, &p)| p).collect();
        got.sort_unstable();
        assert_eq!(got, brute_aabb(&model, &q));
    }

    // Ray queries must equal brute force — same hits, same order.
    let mut ray_out = Vec::new();
    for _ in 0..100 {
        let o = [
            rng.f32(-150.0, 150.0),
            rng.f32(-150.0, 150.0),
            rng.f32(-150.0, 150.0),
        ];
        let d = [rng.f32(-1.0, 1.0), rng.f32(-1.0, 1.0), rng.f32(-1.0, 1.0)];
        ray_out.clear();
        tree.query_ray(o, d, 500.0, &mut ray_out);
        let got: Vec<(usize, f32)> = ray_out.iter().map(|(_, t, &p)| (p, *t)).collect();
        assert_eq!(got, brute_ray(&model, o, d, 500.0));
    }
}

#[test]
fn dynamic_bvh_update_skips_reinsert_inside_fat_box() {
    let mut tree: DynamicBvh<u32> = DynamicBvh::with_margin(1.0);
    let tight = Aabb3::new([0.0, 0.0, 0.0], [1.0, 1.0, 1.0]);
    let id = tree.insert(tight, 7);
    // Small jitter well inside the 1.0 fatten margin → no reinsert.
    let nudge = Aabb3::new([0.2, 0.2, 0.2], [1.2, 1.2, 1.2]);
    assert!(!tree.update(id, nudge));
    assert_eq!(tree.tight_aabb(id), Some(nudge));
    // Oversized jump escapes the fat box → reinsert, same handle.
    let far = Aabb3::new([50.0, 50.0, 50.0], [51.0, 51.0, 51.0]);
    assert!(tree.update(id, far));
    assert_eq!(tree.get(id), Some(&7));
    let mut out = Vec::new();
    tree.query_aabb(far, &mut out);
    assert_eq!(out.len(), 1);
    // Stale handles are rejected.
    let gone = tree.remove(id);
    assert_eq!(gone, Some(7));
    assert_eq!(tree.get(id), None);
    assert!(!tree.update(id, tight));
    assert_eq!(tree.remove(id), None);
}

#[test]
fn dynamic_bvh_handles_zero_and_degenerate_trees() {
    let mut tree: DynamicBvh<u32> = DynamicBvh::new();
    let mut out = Vec::new();
    tree.query_aabb(Aabb3::new([0.0; 3], [1.0; 3]), &mut out);
    assert!(out.is_empty());
    let mut ray = Vec::new();
    tree.query_ray([0.0; 3], [1.0, 0.0, 0.0], 10.0, &mut ray);
    assert!(ray.is_empty());
    // A lone leaf is its own root — insert/remove cycles stay consistent.
    for i in 0..50u32 {
        let id = tree.insert(Aabb3::from_point([i as f32; 3]), i);
        assert_eq!(tree.remove(id), Some(i));
    }
    assert!(tree.is_empty());
}

// ---------------------------------------------------------------------------
// dep_graph.rs — IncrementalDag
// ---------------------------------------------------------------------------

#[test]
fn dag_rejects_cycles_self_loops_and_unknown_nodes() {
    let mut dag = IncrementalDag::new();
    let a = dag.add_node(1);
    let b = dag.add_node(2);
    let c = dag.add_node(3);
    dag.add_edge(a, b).unwrap();
    dag.add_edge(b, c).unwrap();
    assert!(matches!(
        dag.add_edge(c, a),
        Err(DagError::Cycle { .. })
    ));
    assert!(matches!(dag.add_edge(a, a), Err(DagError::SelfLoop(_))));
    let ghost = megu_core::dep_graph::NodeId(9_999);
    assert!(matches!(
        dag.add_edge(a, ghost),
        Err(DagError::NodeNotFound(_))
    ));
    // Duplicate edges are a no-op, not an error.
    dag.add_edge(a, b).unwrap();
    // A long chain still permits closing on a *new* node.
    let d = dag.add_node(4);
    dag.add_edge(c, d).unwrap();
    assert!(matches!(dag.add_edge(d, a), Err(DagError::Cycle { .. })));
}

#[test]
fn dag_dirty_order_is_topological() {
    let mut dag = IncrementalDag::new();
    // input -> d1 -> out, input -> d2 -> out  (diamond)
    let input = dag.add_node(10);
    let d1 = dag.add_node(20);
    let d2 = dag.add_node(30);
    let out = dag.add_node(40);
    dag.add_edge(input, d1).unwrap();
    dag.add_edge(input, d2).unwrap();
    dag.add_edge(d1, out).unwrap();
    dag.add_edge(d2, out).unwrap();

    assert_eq!(dag.dirty_count(), 0);
    dag.mark_input_changed(input, 11).unwrap();
    assert!(!dag.is_dirty(input).unwrap(), "input verified by commit");
    let order = dag.dirty_order();
    assert_eq!(order.len(), 3);
    assert!(order.contains(&d1) && order.contains(&d2));
    assert_eq!(
        order.last().copied(),
        Some(out),
        "downstream-most node must evaluate last"
    );
}

#[test]
fn dag_early_cutoff_rescues_downstream_without_recompute() {
    let mut dag = IncrementalDag::new();
    // Chain A -> B -> C -> D with all digests = 1.
    let a = dag.add_node(1);
    let b = dag.add_node(1);
    let c = dag.add_node(1);
    let d = dag.add_node(1);
    dag.add_edge(a, b).unwrap();
    dag.add_edge(b, c).unwrap();
    dag.add_edge(c, d).unwrap();

    dag.mark_input_changed(a, 2).unwrap();
    assert_eq!(dag.dirty_count(), 3);

    // B recomputes to its stored digest → early cutoff: B verified, and
    // C + D are rescued — no downstream recomputation needed at all.
    assert_eq!(
        dag.recompute_with_cutoff(b, 1),
        CutoffOutcome::EarlyCutoff
    );
    assert!(!dag.is_dirty(b).unwrap());
    assert!(!dag.is_dirty(c).unwrap(), "rescued through the cutoff");
    assert!(!dag.is_dirty(d).unwrap(), "rescue cascades");
    assert_eq!(dag.dirty_count(), 0);
}

#[test]
fn dag_changed_digest_propagates_and_cutoff_still_rescues() {
    let mut dag = IncrementalDag::new();
    let a = dag.add_node(1);
    let b = dag.add_node(1);
    let c = dag.add_node(1);
    dag.add_edge(a, b).unwrap();
    dag.add_edge(b, c).unwrap();

    dag.mark_input_changed(a, 2).unwrap();
    // B's value actually changed → downstream goes dirty.
    assert_eq!(
        dag.recompute_with_cutoff(b, 2),
        CutoffOutcome::Changed
    );
    assert!(dag.is_dirty(c).unwrap());
    // C recomputes unchanged → verified, nothing further to invalidate.
    assert_eq!(
        dag.recompute_with_cutoff(c, 1),
        CutoffOutcome::EarlyCutoff
    );
    assert_eq!(dag.dirty_count(), 0);
}

#[test]
fn dag_cutoff_does_not_clear_node_dirty_from_another_changed_input() {
    let mut dag = IncrementalDag::new();
    // Two inputs feed M; M feeds N.
    let x = dag.add_node(1);
    let y = dag.add_node(1);
    let m = dag.add_node(1);
    let n = dag.add_node(1);
    dag.add_edge(x, m).unwrap();
    dag.add_edge(y, m).unwrap();
    dag.add_edge(m, n).unwrap();

    dag.mark_input_changed(x, 9).unwrap();
    // Y verifies unchanged — but M must STAY dirty because X changed.
    assert_eq!(dag.recompute_with_cutoff(y, 1), CutoffOutcome::EarlyCutoff);
    assert!(dag.is_dirty(m).unwrap(), "X's change is still pending");
    assert!(dag.is_dirty(n).unwrap());
    // Once M recomputes unchanged too, N is rescued.
    assert_eq!(dag.recompute_with_cutoff(m, 1), CutoffOutcome::EarlyCutoff);
    assert!(!dag.is_dirty(n).unwrap());
    assert_eq!(dag.dirty_count(), 0);
}

#[test]
fn dag_dirty_order_into_reuses_caller_scratch() {
    let mut dag = IncrementalDag::new();
    let a = dag.add_node(0);
    let mut prev = a;
    for _ in 0..64 {
        let next = dag.add_node(0);
        dag.add_edge(prev, next).unwrap();
        prev = next;
    }
    dag.mark_input_changed(a, 1).unwrap();
    let mut scratch = Vec::new();
    dag.dirty_order_into(&mut scratch);
    assert_eq!(scratch.len(), 64);
    // Strictly increasing indices = topo order on a chain.
    for w in scratch.windows(2) {
        assert!(w[0].0 < w[1].0);
    }
}

// ---------------------------------------------------------------------------
// bloom.rs — BlockedBloomFilter
// ---------------------------------------------------------------------------

#[test]
fn bloom_zero_false_negatives_over_50k_keys() {
    let mut filter = BlockedBloomFilter::with_capacity(50_000, 10);
    for i in 0..50_000u64 {
        filter.insert_u64(i.wrapping_mul(0x9e37_79b9_7f4a_7c15));
    }
    for i in 0..50_000u64 {
        assert!(
            filter.contains_u64(i.wrapping_mul(0x9e37_79b9_7f4a_7c15)),
            "false negative on inserted key {i}"
        );
    }
}

#[test]
fn bloom_false_positive_rate_stays_under_two_percent() {
    let mut filter = BlockedBloomFilter::with_capacity(50_000, 10);
    for i in 0..50_000u64 {
        filter.insert_u64(i.wrapping_mul(0x9e37_79b9_7f4a_7c15));
    }
    let probes = 150_000u64;
    let mut fp = 0u64;
    for i in 0..probes {
        // Keys drawn from a disjoint generator stream.
        let key = 0xdead_beef_u64
            .wrapping_mul(i + 1)
            .wrapping_add(0x517c_c1b7_2722_0a95);
        if filter.contains_u64(key) {
            fp += 1;
        }
    }
    let rate = fp as f64 / probes as f64;
    assert!(
        rate < 0.02,
        "false-positive rate {rate:.4} exceeds 2% budget"
    );
}

#[test]
fn bloom_bytes_api_and_clear() {
    let mut filter = BlockedBloomFilter::with_capacity(1_000, 10);
    filter.insert_bytes(b"minecraft:overworld");
    filter.insert_bytes(b"mod:dim/aether");
    assert!(filter.contains_bytes(b"minecraft:overworld"));
    assert!(filter.contains_bytes(b"mod:dim/aether"));
    filter.clear();
    // After clear every previously-hit key must be gone (no negatives).
    assert!(!filter.contains_bytes(b"minecraft:overworld"));
    assert!(!filter.contains_bytes(b"mod:dim/aether"));
    assert_eq!(
        filter.bit_capacity(),
        filter.block_count() * 512
    );
}

// ---------------------------------------------------------------------------
// bloom.rs — VoxelBitset4096
// ---------------------------------------------------------------------------

#[test]
fn voxel_bitset_matches_btreset_exactly() {
    let mut rng = Rng::new(42);
    let mut set = VoxelBitset4096::new();
    let mut oracle = BTreeSet::new();
    for _ in 0..20_000 {
        let idx = rng.next() % 4096;
        if rng.next() & 1 == 0 {
            set.set(idx as u16);
            oracle.insert(idx as u16);
        } else {
            set.clear(idx as u16);
            oracle.remove(&(idx as u16));
        }
    }
    assert_eq!(set.count_ones() as usize, oracle.len());
    assert_eq!(set.is_empty(), oracle.is_empty());
    for idx in 0..4096u16 {
        assert_eq!(set.get(idx), oracle.contains(&idx), "mismatch at {idx}");
    }
    // Sparse iteration emits the same ascending sequence.
    let mut visited = Vec::new();
    set.for_each_set(|i| visited.push(i));
    assert_eq!(visited, oracle.iter().copied().collect::<Vec<_>>());
}

#[test]
fn voxel_bitset_set_algebra() {
    let mut rng = Rng::new(7);
    let mut a = VoxelBitset4096::new();
    let mut b = VoxelBitset4096::new();
    let mut oa = BTreeSet::new();
    let mut ob = BTreeSet::new();
    for _ in 0..5_000 {
        let ia = (rng.next() % 4096) as u16;
        let ib = (rng.next() % 4096) as u16;
        a.set(ia);
        oa.insert(ia);
        b.set(ib);
        ob.insert(ib);
    }
    let mut u = a.clone();
    u.union_with(&b);
    let mut i = a.clone();
    i.intersect_with(&b);
    let mut d = a.clone();
    d.difference_with(&b);
    let expect_u: BTreeSet<u16> = oa.union(&ob).copied().collect();
    let expect_i: BTreeSet<u16> = oa.intersection(&ob).copied().collect();
    let expect_d: BTreeSet<u16> = oa.difference(&ob).copied().collect();
    for idx in 0..4096u16 {
        assert_eq!(u.get(idx), expect_u.contains(&idx));
        assert_eq!(i.get(idx), expect_i.contains(&idx));
        assert_eq!(d.get(idx), expect_d.contains(&idx));
    }
    assert_eq!(u.count_ones() as usize, expect_u.len());
    assert_eq!(i.count_ones() as usize, expect_i.len());
    assert_eq!(d.count_ones() as usize, expect_d.len());
}

#[test]
fn voxel_bitset_edges_and_summary_skip() {
    let mut set = VoxelBitset4096::new();
    assert!(set.is_empty());
    // Out-of-range indices are inert.
    set.set(4096);
    set.set(u16::MAX);
    assert!(set.is_empty());
    assert!(!set.get(4095));
    // First and last voxel.
    set.set(0);
    set.set(4095);
    assert_eq!(set.count_ones(), 2);
    let mut order = Vec::new();
    set.for_each_set(|i| order.push(i));
    assert_eq!(order, vec![0, 4095]);
    set.clear(0);
    set.clear(4095);
    assert!(set.is_empty());
}

// ---------------------------------------------------------------------------
// ring_spsc.rs — WaitFreeSpscU64
// ---------------------------------------------------------------------------

#[test]
fn spsc_u64_basic_fifo_full_empty() {
    let (tx, rx) = spsc_u64(8);
    assert!(rx.is_empty());
    assert_eq!(rx.try_pop(), None);
    for i in 0..8u64 {
        tx.try_push(i).unwrap();
    }
    assert!(tx.is_full());
    assert_eq!(tx.try_push(99), Err(99), "full ring rejects");
    for i in 0..8u64 {
        assert_eq!(rx.try_pop(), Some(i));
    }
    assert_eq!(rx.try_pop(), None);
    // Capacity rounds up to a power of two.
    let (tx2, _rx2) = spsc_u64(5);
    assert_eq!(tx2.capacity(), 8);
    let (tx3, _rx3) = spsc_u64(1);
    assert_eq!(tx3.capacity(), 2, "minimum capacity is 2");
}

#[test]
fn spsc_u64_threads_pass_100k_messages_in_order() {
    let (tx, rx) = spsc_u64(1024);
    const N: u64 = 100_000;
    let producer = std::thread::spawn(move || {
        for i in 0..N {
            tx.push_blocking(i);
        }
    });
    let consumer = std::thread::spawn(move || {
        for i in 0..N {
            assert_eq!(rx.pop_blocking(), i, "message {i} out of order");
        }
    });
    producer.join().unwrap();
    consumer.join().unwrap();
}

#[test]
fn spsc_channel_generic_moves_owned_values() {
    let (tx, rx) = spsc_channel::<String>(4);
    tx.try_push("alpha".to_string()).unwrap();
    tx.try_push("beta".to_string()).unwrap();
    assert_eq!(rx.try_pop().as_deref(), Some("alpha"));
    assert_eq!(rx.try_pop().as_deref(), Some("beta"));
    assert!(rx.is_empty());

    // Cross-thread: owned values move safely through the ring.
    let (tx2, rx2) = spsc_channel::<usize>(16);
    let producer = std::thread::spawn(move || {
        for i in 0..10_000usize {
            let mut v = i;
            while let Err(rejected) = tx2.try_push(v) {
                v = rejected;
                std::hint::spin_loop();
            }
        }
    });
    let consumer = std::thread::spawn(move || {
        let mut expect = 0usize;
        while expect < 10_000 {
            match rx2.try_pop() {
                Some(v) => {
                    assert_eq!(v, expect);
                    expect += 1;
                }
                None => std::hint::spin_loop(),
            }
        }
    });
    producer.join().unwrap();
    consumer.join().unwrap();
}

// ---------------------------------------------------------------------------
// ring_spsc.rs — BoundedRing
// ---------------------------------------------------------------------------

#[test]
fn bounded_ring_fifo_and_full() {
    let mut ring: BoundedRing<u32> = BoundedRing::with_capacity(4);
    for i in 0..4 {
        ring.push_back(i).unwrap();
    }
    assert!(ring.is_full());
    assert_eq!(ring.push_back(9), Err(9));
    assert_eq!(ring.front(), Some(&0));
    assert_eq!(ring.back(), Some(&3));
    for i in 0..4 {
        assert_eq!(ring.pop_front(), Some(i));
    }
    assert_eq!(ring.pop_front(), None);
    assert!(ring.is_empty());
}

#[test]
fn bounded_ring_overwrite_oldest_keeps_newest() {
    let mut ring: BoundedRing<u32> = BoundedRing::with_capacity(3);
    for i in 0..10 {
        ring.push_overwrite_oldest(i);
    }
    assert_eq!(ring.len(), 3);
    assert_eq!(ring.pop_front(), Some(7));
    assert_eq!(ring.pop_front(), Some(8));
    assert_eq!(ring.pop_front(), Some(9));
    assert!(ring.is_empty());
}

#[test]
fn bounded_ring_mixed_churn_matches_vecdeque() {
    use std::collections::VecDeque;
    let mut rng = Rng::new(99);
    let mut ring: BoundedRing<u64> = BoundedRing::with_capacity(32);
    let mut oracle: VecDeque<u64> = VecDeque::new();
    for _ in 0..10_000 {
        match rng.usize(3) {
            0 => {
                let v = rng.next();
                assert_eq!(ring.push_back(v).is_ok(), oracle.len() < 32);
                if oracle.len() < 32 {
                    oracle.push_back(v);
                }
            }
            1 => {
                let v = rng.next();
                ring.push_overwrite_oldest(v);
                if oracle.len() == 32 {
                    oracle.pop_front();
                }
                oracle.push_back(v);
            }
            _ => {
                assert_eq!(ring.pop_front(), oracle.pop_front());
            }
        }
        assert_eq!(ring.len(), oracle.len());
        assert_eq!(ring.front(), oracle.front());
        assert_eq!(ring.back(), oracle.back());
    }
    ring.clear();
    assert!(ring.is_empty());
    oracle.clear();
}
