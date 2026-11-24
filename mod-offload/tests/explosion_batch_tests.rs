// SPDX-License-Identifier: LGPL-3.0-or-later

//! Lane PERF-TNT-BLAST: bit-exact Vanilla raycast parity, a real 1,000-TNT
//! chain stress run, and a power-32 modded mega-TNT sliced across ticks.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::time::Instant;

use mod_offload::explosion_batch::{
    blast_bounds, coalesce_item_counts, scrub_power, AffectedCell, BlastCell, BlastResult,
    BlastStats, BlastWorld, ExplosionBatch, ExplosionError, ItemStack, MegaBlastPlan, XorShift64,
    BLOCK_COST_BASE, MAX_BLAST_POWER, MAX_RAY_STEPS, MEGA_BLAST_MAX_PER_TICK, RAY_GRID,
    RAY_POWER_MIN, RAY_POWER_RANGE, RAY_STEP, STEP_DRAIN, TNT_POWER,
};

type World = HashMap<(i32, i32, i32), BlastCell>;

const AIR: BlastCell = BlastCell::AIR;
const STONE: BlastCell = BlastCell::solid(1, 6.0, 201, 1);
const DIRT: BlastCell = BlastCell::solid(2, 0.5, 203, 1);
const OBSIDIAN: BlastCell = BlastCell::solid(3, 1200.0, 0, 0);
const BEDROCK: BlastCell = BlastCell::indestructible(4, 3_600_000.0, 0, 0);
const TNT_CELL: BlastCell = BlastCell::solid(5, 0.0, 2094, 1);

fn world() -> World {
    World::new()
}

fn put(w: &mut World, x: i32, y: i32, z: i32, cell: BlastCell) {
    w.insert((x, y, z), cell);
}

/// Independent scalar reference: the same Vanilla march expressed as plain
/// nested loops over a `HashMap` world with per-position `f32` density.
/// Deliberately shares no code with `march_rays` so parity is meaningful.
fn scalar_reference(
    world: &World,
    bounds: ([i64; 3], [i64; 3]),
    origin: [f64; 3],
    power: f32,
    seed: u64,
) -> BTreeMap<[i32; 3], f32> {
    let mut rng = XorShift64::new(seed);
    let mut density: HashMap<[i32; 3], f32> = HashMap::new();
    let in_bounds = |p: [i32; 3]| -> bool {
        p.iter().enumerate().all(|(axis, &v)| {
            let v = i64::from(v);
            v >= bounds.0[axis] && v <= bounds.1[axis]
        })
    };
    for i in 0..RAY_GRID {
        for j in 0..RAY_GRID {
            for k in 0..RAY_GRID {
                let (iu, ju, ku) = (i, j, k);
                if iu != 0
                    && iu != RAY_GRID - 1
                    && ju != 0
                    && ju != RAY_GRID - 1
                    && ku != 0
                    && ku != RAY_GRID - 1
                {
                    continue;
                }
                let mut dx = (f64::from(iu) / 15.0) * 2.0 - 1.0;
                let mut dy = (f64::from(ju) / 15.0) * 2.0 - 1.0;
                let mut dz = (f64::from(ku) / 15.0) * 2.0 - 1.0;
                let len = (dx * dx + dy * dy + dz * dz).sqrt();
                dx /= len;
                dy /= len;
                dz /= len;
                let mut remaining = power * (RAY_POWER_MIN + rng.next_f32() * RAY_POWER_RANGE);
                let (mut px, mut py, mut pz) = (origin[0], origin[1], origin[2]);
                let mut steps = 0u32;
                while remaining > 0.0 && steps < MAX_RAY_STEPS {
                    steps += 1;
                    let pos = [px.floor() as i32, py.floor() as i32, pz.floor() as i32];
                    let cell = if in_bounds(pos) {
                        world.get(&(pos[0], pos[1], pos[2])).copied().unwrap_or(AIR)
                    } else {
                        AIR
                    };
                    if !cell.is_air() {
                        let resistance = if cell.blast_resistance.is_nan() {
                            0.0
                        } else {
                            cell.blast_resistance.max(0.0)
                        };
                        remaining -= (resistance + BLOCK_COST_BASE) * BLOCK_COST_BASE;
                    }
                    if remaining > 0.0 && !cell.is_air() && !cell.is_indestructible() && in_bounds(pos)
                    {
                        density
                            .entry(pos)
                            .and_modify(|slot| {
                                if remaining > *slot {
                                    *slot = remaining;
                                }
                            })
                            .or_insert(remaining);
                    }
                    px += dx * RAY_STEP;
                    py += dy * RAY_STEP;
                    pz += dz * RAY_STEP;
                    remaining -= STEP_DRAIN;
                }
            }
        }
    }
    density.into_iter().collect()
}

/// Build a small arena: stone floor, dirt/stone walls, one obsidian block,
/// one bedrock block — enough variety to exercise every ray path.
fn parity_world() -> World {
    let mut w = world();
    for x in 8..=56 {
        for z in 8..=56 {
            put(&mut w, x, 34, z, STONE);
            put(&mut w, x, 35, z, STONE);
        }
    }
    for x in 16..=48 {
        for z in 16..=48 {
            for y in 36..=44 {
                let shell = x == 16 || x == 48 || z == 16 || z == 48 || y == 36 || y == 44;
                if shell {
                    put(&mut w, x, y, z, if (x + y + z) % 3 == 0 { DIRT } else { STONE });
                }
            }
        }
    }
    put(&mut w, 30, 40, 30, OBSIDIAN);
    put(&mut w, 34, 38, 26, BEDROCK);
    put(&mut w, 26, 42, 34, DIRT);
    w
}

#[test]
fn single_tnt_matches_scalar_vanilla_raycast_bit_for_bit() {
    let w = parity_world();
    let origin = [32.5, 46.0, 32.5];
    let power = TNT_POWER;
    let seed = 0x5EED_1234_ABCD_9876;

    let bounds = blast_bounds(origin, power).expect("finite origin");
    let reference = scalar_reference(&w, bounds, origin, power, seed);

    let started = Instant::now();
    let mut batch = ExplosionBatch::new();
    let result = batch
        .blast(&w, origin, power, seed)
        .expect("power-4 blast fits cache");
    let engine_ms = started.elapsed().as_secs_f64() * 1000.0;

    assert_eq!(result.stats.rays_cast, 1352, "Vanilla surface grid ray count");
    assert_eq!(
        result.cells.len(),
        reference.len(),
        "affected-cell count diverged"
    );
    for affected in &result.cells {
        let expected = reference
            .get(&affected.pos)
            .unwrap_or_else(|| panic!("engine recorded {:?} missing from scalar", affected.pos));
        assert_eq!(
            affected.intensity.to_bits(),
            expected.to_bits(),
            "intensity bits diverged at {:?}",
            affected.pos
        );
        let source_cell = w.get(&(affected.pos[0], affected.pos[1], affected.pos[2]))
            .copied()
            .unwrap_or(AIR);
        assert_eq!(affected.cell.block_id, source_cell.block_id);
        assert!(!affected.cell.is_air());
    }
    // Determinism: an identical rerun must be bit-identical.
    let mut batch2 = ExplosionBatch::new();
    let again = batch2.blast(&w, origin, power, seed).expect("rerun");
    assert_eq!(result.cells.len(), again.cells.len());
    for (a, b) in result.cells.iter().zip(again.cells.iter()) {
        assert_eq!(a.pos, b.pos);
        assert_eq!(a.intensity.to_bits(), b.intensity.to_bits());
    }
    println!(
        "single_tnt: rays={} affected={} engine={:.3}ms",
        result.stats.rays_cast,
        result.cells.len(),
        engine_ms
    );
}

/// Drive every `BlastResult` through `MegaBlastPlan` with a small budget,
/// returning (removals, coalesced stacks, section updates) totals.
fn drain_plans(
    results: &[BlastResult],
    budget: usize,
) -> (usize, Vec<ItemStack>, usize, usize) {
    let mut removals = 0usize;
    let mut stacks: Vec<ItemStack> = Vec::new();
    let mut sections = 0usize;
    let mut max_slice = 0usize;
    for result in results {
        let mut plan = MegaBlastPlan::new(result, budget);
        let mut last_ring = 0u32;
        let mut first = true;
        while let Some(slice) = plan.next_slice() {
            assert_eq!(slice.first_ring, slice.removals[0].ring);
            assert_eq!(slice.last_ring, slice.removals.last().unwrap().ring);
            for removal in &slice.removals {
                if first {
                    first = false;
                } else {
                    assert!(removal.ring >= last_ring, "rings must be innermost first");
                }
                last_ring = removal.ring;
            }
            removals += slice.removals.len();
            max_slice = max_slice.max(slice.removals.len());
            sections += slice.section_updates.len();
            stacks.extend(slice.drops.iter().copied());
        }
        assert!(plan.is_complete());
    }
    assert!(max_slice <= budget.max(1));
    (removals, stacks, sections, max_slice)
}

/// Total item count conserved inside coalesced stacks.
fn conserved_items(stacks: &[ItemStack]) -> u64 {
    stacks.iter().map(|s| u64::from(s.count)).sum()
}

/// Expected drops: sum of `drop_count` over every affected cell, per item.
fn expected_drops(results: &[BlastResult]) -> BTreeMap<u32, u64> {
    let mut totals = BTreeMap::new();
    for result in results {
        for cell in &result.cells {
            if cell.cell.drop_item != 0 && cell.cell.drop_count > 0 {
                *totals.entry(cell.cell.drop_item).or_insert(0) +=
                    u64::from(cell.cell.drop_count);
            }
        }
    }
    totals
}

fn assert_coalesced(stacks: &[ItemStack], expected: &BTreeMap<u32, u64>) {
    let mut actual: BTreeMap<u32, u64> = BTreeMap::new();
    for stack in stacks {
        assert!(stack.count > 0 && stack.count <= 64, "stack size out of bounds");
        *actual.entry(stack.item_id).or_insert(0) += u64::from(stack.count);
    }
    assert_eq!(&actual, expected, "drop conservation violated");
}

#[test]
fn stacked_and_chain_1000_tnt_survives_fast_with_exact_conservation() {
    // World: stone floor, an obsidian shield box with bedrock footing, a
    // dense stacked TNT cluster and a spaced chain grid.
    let mut w = world();
    for x in 0..160 {
        for z in 0..160 {
            put(&mut w, x, 63, z, STONE);
            put(&mut w, x, 62, z, BEDROCK);
        }
    }
    // Obsidian shield shell around the cluster area, bedrock floor inside.
    for x in 16..=36 {
        for z in 16..=36 {
            for y in 64..=80 {
                let shell = x == 16 || x == 36 || z == 16 || z == 36 || y == 64 || y == 80;
                if shell {
                    put(&mut w, x, y, z, OBSIDIAN);
                }
            }
        }
    }
    // Track immunity-critical positions.
    let mut obsidian_positions = BTreeSet::new();
    let mut bedrock_positions = BTreeSet::new();
    for (pos, cell) in &w {
        match cell.block_id {
            3 => {
                obsidian_positions.insert(*pos);
            }
            4 => {
                bedrock_positions.insert(*pos);
            }
            _ => {}
        }
    }

    // 1,000 unique primed TNT: 500 stacked adjacent (10x10x5) + 500 spaced
    // chain (25x20 grid, spacing 5) + 60 deliberate duplicates for dedup.
    let mut tnts: Vec<[i32; 3]> = Vec::with_capacity(1060);
    for dx in 0..10 {
        for dy in 0..10 {
            for dz in 0..5 {
                let pos = [20 + dx, 66 + dy, 20 + dz];
                put(&mut w, pos[0], pos[1], pos[2], TNT_CELL);
                tnts.push(pos);
            }
        }
    }
    for ix in 0..25 {
        for iz in 0..20 {
            let pos = [44 + 5 * ix, 66, 44 + 5 * iz];
            put(&mut w, pos[0], pos[1], pos[2], TNT_CELL);
            tnts.push(pos);
        }
    }
    // Duplicates: same positions repeated -> must dedup to one blast each.
    let dups: Vec<[i32; 3]> = tnts.iter().take(60).copied().collect();
    tnts.extend(dups);
    assert_eq!(tnts.len(), 1060);

    let mut view = BlastWorld::new(&w);
    let started = Instant::now();
    let results = view
        .detonate_tnt_tick(&tnts, TNT_POWER, 0xBEEF_0001)
        .expect("cluster blast fits cache");
    let detonate_ms = started.elapsed().as_secs_f64() * 1000.0;

    // Stacked primed TNT dedup + already-air skips must have fired.
    assert_eq!(view.stats.deduplicated, 60);
    assert!(view.stats.skipped_already_air > 0, "expected air skips");
    assert!(view.stats.blasts_run < 1000);
    assert_eq!(results.len() as u64, view.stats.blasts_run);

    // Immunity: no obsidian or bedrock position may ever be destroyed.
    let mut destroyed: BTreeSet<[i32; 3]> = BTreeSet::new();
    for result in &results {
        for affected in &result.cells {
            let apos = (affected.pos[0], affected.pos[1], affected.pos[2]);
            assert!(!bedrock_positions.contains(&apos), "bedrock destroyed");
            assert!(!obsidian_positions.contains(&apos), "obsidian destroyed");
            assert!(!affected.cell.is_indestructible());
            destroyed.insert(affected.pos);
        }
    }
    assert!(destroyed.len() > 1000, "cluster should level terrain");

    // Every result plans smoothly and conserves drops exactly.
    let started = Instant::now();
    let (removals, stacks, _sections, max_slice) = drain_plans(&results, 257);
    let plan_ms = started.elapsed().as_secs_f64() * 1000.0;
    let expected_cells: usize = results.iter().map(|r| r.cells.len()).sum();
    assert_eq!(removals, expected_cells);
    assert!(max_slice <= 257);
    let expected = expected_drops(&results);
    assert_coalesced(&stacks, &expected);

    let uncoalesced: u64 = expected.values().sum();
    let entities = stacks.len() as u64;
    println!(
        "1000_tnt: input={} dedup={} blasts={} skipped_air={} affected={} \
         detonate={:.2}ms plan={:.2}ms drops={} items conserved -> {} stacks ({}x, {:.2} items/stack)",
        tnts.len(),
        view.stats.deduplicated,
        view.stats.blasts_run,
        view.stats.skipped_already_air,
        expected_cells,
        detonate_ms,
        plan_ms,
        uncoalesced,
        entities,
        if entities > 0 {
            uncoalesced as f64 / entities as f64
        } else {
            0.0
        },
        if entities > 0 {
            conserved_items(&stacks) as f64 / entities as f64
        } else {
            0.0
        }
    );
}

#[test]
fn modded_mega_tnt_power_32_slices_smoothly_across_ticks() {
    // Large dirt slab (low resistance -> deep power-32 crater) with an
    // obsidian stripe and bedrock pillars.
    let mut w = world();
    for x in -60..=60 {
        for z in -60..=60 {
            for y in 55..=92 {
                put(&mut w, x, y, z, DIRT);
            }
        }
    }
    for x in -60..=60 {
        for y in 60..=96 {
            put(&mut w, x, y, 0, OBSIDIAN);
        }
    }
    for (bx, bz) in [(-50, -50), (50, -50), (-50, 50), (50, 50)] {
        for y in 60..=100 {
            put(&mut w, bx, y, bz, BEDROCK);
        }
    }

    let origin = [0.5, 76.0, 5.5];
    let power = 32.0f32;
    let started = Instant::now();
    let mut batch = ExplosionBatch::new();
    let result = batch.blast(&w, origin, power, 0x32_005EED).expect("power-32 fits cache");
    let blast_ms = started.elapsed().as_secs_f64() * 1000.0;
    assert!(
        result.cells.len() > 10_000,
        "mega blast should be huge, got {}",
        result.cells.len()
    );
    assert_eq!(result.stats.rays_cast, 1352);

    // No shielded block may be in the crater.
    for affected in &result.cells {
        let cell = w.get(&(affected.pos[0], affected.pos[1], affected.pos[2]))
            .copied()
            .unwrap_or(AIR);
        assert_ne!(cell.block_id, 4, "bedrock pillar destroyed");
    }

    // Slice the crater at 4096 blocks/tick: monotonic, bounded, complete.
    let started = Instant::now();
    let mut plan = MegaBlastPlan::new(&result, 4096);
    let total = plan.total();
    let mut slices = 0usize;
    let mut last_done = 0usize;
    let mut drop_entities = 0u64;
    let mut conserved = 0u64;
    let mut max_stack = 0u32;
    let mut section_updates = 0usize;
    let mut last_ring = 0u32;
    let mut first_removal = true;
    while let Some(slice) = plan.next_slice() {
        slices += 1;
        assert!(slice.removals.len() <= 4096, "tick budget exceeded");
        for removal in &slice.removals {
            if first_removal {
                first_removal = false;
            } else {
                assert!(removal.ring >= last_ring, "rings must be innermost first");
            }
            last_ring = removal.ring;
        }
        assert_eq!(slice.done, last_done + slice.removals.len());
        assert!(slice.done > last_done);
        last_done = slice.done;
        section_updates += slice.section_updates.iter().map(|u| u.updates.len()).sum::<usize>();
        for stack in &slice.drops {
            assert!(stack.count <= 64);
            max_stack = max_stack.max(stack.count);
            drop_entities += 1;
            conserved += u64::from(stack.count);
        }
    }
    let plan_ms = started.elapsed().as_secs_f64() * 1000.0;
    assert!(plan.is_complete());
    assert_eq!(last_done, total);
    assert_eq!(slices, total.div_ceil(4096));
    let expected = expected_drops(std::slice::from_ref(&result));
    let expected_total: u64 = expected.values().sum();
    assert_eq!(conserved, expected_total, "mega drop conservation");

    // Uncoalesced entity estimate vs coalesced stacks.
    let uncoalesced_entities: u64 = result
        .cells
        .iter()
        .filter(|c| c.cell.drop_item != 0 && c.cell.drop_count > 0)
        .count() as u64;
    println!(
        "mega_tnt_p32: affected={} ticks={} max_slice={} drops {} items -> {} entities \
         ({}x reduction) sections={} blast={:.2}ms plan={:.2}ms",
        total,
        slices,
        total.min(4096),
        conserved,
        drop_entities,
        if drop_entities > 0 {
            uncoalesced_entities as f64 / drop_entities as f64
        } else {
            0.0
        },
        section_updates,
        blast_ms,
        plan_ms
    );

    // Safety: non-finite / extreme / oversized inputs fail closed.
    assert_eq!(scrub_power(f32::NAN), 0.0);
    assert_eq!(scrub_power(f32::INFINITY), 0.0);
    assert_eq!(scrub_power(-7.0), 0.0);
    assert_eq!(scrub_power(1.0e30), MAX_BLAST_POWER);
    let mut batch = ExplosionBatch::new();
    let empty = batch
        .blast(&w, [f64::NAN, 0.0, 0.0], 4.0, 1)
        .expect("non-finite origin scrubs to empty");
    assert!(empty.cells.is_empty());
    let huge = batch
        .blast(&w, [f64::MAX, f64::MAX, f64::MAX], 4.0, 1)
        .expect("extreme coords saturate safely");
    assert!(huge.cells.len() < 100_000);
    let oversized = batch.blast(&w, origin, 64.0, 1);
    assert!(
        matches!(oversized, Err(ExplosionError::RegionTooLarge { .. })),
        "power-64 bounding volume must fail closed, got {:?}",
        oversized.as_ref().map(|r| r.cells.len())
    );
}

#[test]
fn coalesce_item_counts_conserves_and_chunks() {
    // 1000 identical drops -> 16 stacks (15*64 + 40), exact conservation.
    let stacks = coalesce_item_counts(std::iter::repeat_n((7u32, 1u32), 1000), 64);
    assert_eq!(stacks.len(), 16);
    assert_eq!(conserved_items(&stacks), 1000);
    assert!(stacks.iter().all(|s| s.item_id == 7 && s.count <= 64));
    // Mixed items aggregate per id in ascending order.
    let mixed = coalesce_item_counts(
        [(9u32, 70u32), (3u32, 1u32), (9u32, 70u32), (3u32, 64u32)],
        64,
    );
    assert_eq!(mixed[0].item_id, 3);
    assert_eq!(conserved_items(&mixed), 205);
    // Degenerate inputs stay safe: stack_size 0 clamps to 1.
    assert!(coalesce_item_counts([], 0).is_empty());
    let one = coalesce_item_counts([(1u32, 5u32)], 0);
    assert_eq!(conserved_items(&one), 5);
    assert!(one.iter().all(|s| s.count == 1));
}

// ---------- M32: deterministic multi-tick ring-sliced MegaBlastPlan ----------

/// Hand-built `BlastResult`: every position a destructible solid, engine
/// untouched, so ring geometry can be asserted exactly.
fn synthetic_result(origin: [f64; 3], positions: &[[i32; 3]]) -> BlastResult {
    let mut cells: Vec<AffectedCell> = positions
        .iter()
        .map(|&pos| AffectedCell {
            pos,
            intensity: 1.0,
            cell: STONE,
        })
        .collect();
    cells.sort_by_key(|c| c.pos);
    BlastResult {
        origin,
        power: TNT_POWER,
        bounds: ([i64::MIN; 3], [i64::MAX; 3]),
        cells,
        stats: BlastStats::default(),
    }
}

/// Ring index the plan assigns: floor of block-center distance to origin.
fn expected_ring(origin: [f64; 3], pos: [i32; 3]) -> u32 {
    let dx = f64::from(pos[0]) + 0.5 - origin[0];
    let dy = f64::from(pos[1]) + 0.5 - origin[1];
    let dz = f64::from(pos[2]) + 0.5 - origin[2];
    (dx * dx + dy * dy + dz * dz).sqrt() as u32
}

/// Flat `(ring, pos)` drain sequence plus per-slice ring boundaries.
struct DrainTrace {
    seq: Vec<(u32, [i32; 3])>,
    slice_bounds: Vec<(u32, u32)>,
}

/// Drain a plan into its full removal order and ring-span per slice.
fn drain_positions(plan: &mut MegaBlastPlan) -> DrainTrace {
    let mut trace = DrainTrace {
        seq: Vec::new(),
        slice_bounds: Vec::new(),
    };
    while let Some(slice) = plan.next_slice() {
        trace.slice_bounds.push((slice.first_ring, slice.last_ring));
        trace
            .seq
            .extend(slice.removals.iter().map(|r| (r.ring, r.pos)));
    }
    trace
}

#[test]
fn mega_blast_plan_rings_innermost_first_with_coordinate_order() {
    let origin = [0.5, 0.5, 0.5];
    // Hand-picked cells spanning rings 0..=10, listed here in shuffled
    // order to prove the plan sorts rather than trusting input order.
    let positions = [
        [-10, 0, 0],  // ring 10
        [1, 1, 0],    // ring 1
        [0, 0, 0],    // ring 0
        [2, 2, 2],    // ring 2
        [-1, 0, 0],   // ring 1
        [3, 3, 0],    // ring 4
        [1, 0, 0],    // ring 1
        [1, 1, 1],    // ring 1
        [2, 0, 0],    // ring 2
        [0, 2, 0],    // ring 2
        [0, 0, 2],    // ring 2
        [5, -4, 2],   // ring 6
        [-2, -2, -1], // ring 3
        [0, -1, 0],   // ring 1
        [4, 0, 0],    // ring 4
        [0, 4, 0],    // ring 4
        [3, 0, -1],   // ring 3
        [-3, 0, 0],   // ring 3
        [0, 0, -3],   // ring 3
        [6, 6, 6],    // ring 9
        [7, 0, 0],    // ring 7
        [0, 0, 7],    // ring 7
        [-1, -1, -1], // ring 2
        [1, -1, 0],   // ring 1
    ];
    let result = synthetic_result(origin, &positions);
    assert_eq!(result.cells.len(), positions.len());

    // Budget 5 forces slices that split individual rings.
    let mut plan = MegaBlastPlan::new(&result, 5);
    assert_eq!(plan.budget(), 5);
    assert_eq!(plan.total(), positions.len());
    assert!(!MegaBlastPlan::needs_slicing(&result));

    let expected_rings: BTreeSet<u32> = positions
        .iter()
        .map(|&p| expected_ring(origin, p))
        .collect();
    assert_eq!(plan.ring_count(), expected_rings.len());

    let trace = drain_positions(&mut plan);
    let seq = trace.seq;
    assert_eq!(seq.len(), positions.len());
    assert!(plan.is_complete());

    // Innermost rings first: the ring sequence is non-decreasing, starts
    // at ring 0, ends at the outermost ring, and every removal carries
    // the ring its distance implies.
    assert_eq!(seq[0].0, 0, "first removal must be the innermost ring");
    assert_eq!(seq[0].1, [0, 0, 0]);
    assert_eq!(
        seq.last().unwrap().0,
        *expected_rings.iter().next_back().unwrap()
    );
    for (i, &(ring, pos)) in seq.iter().enumerate() {
        assert_eq!(ring, expected_ring(origin, pos), "ring label at index {i}");
        if i > 0 {
            assert!(ring >= seq[i - 1].0, "ring regression at index {i}");
        }
    }
    // Deterministic coordinate order inside each ring: positions are
    // strictly increasing within every same-ring run.
    for run in seq.chunk_by(|a, b| a.0 == b.0) {
        for pair in run.windows(2) {
            assert!(pair[0].1 < pair[1].1, "coordinate order inside a ring");
        }
    }
    // Slices never revisit a completed ring.
    for pair in trace.slice_bounds.windows(2) {
        assert!(pair[1].0 >= pair[0].1, "slice ring bounds regressed");
    }
    // The drained set is exactly the destroyed set.
    let drained: BTreeSet<[i32; 3]> = seq.iter().map(|&(_, p)| p).collect();
    let planned: BTreeSet<[i32; 3]> = positions.iter().copied().collect();
    assert_eq!(drained, planned);
}

#[test]
fn mega_blast_plan_oversized_ring_splits_across_ticks_deterministically() {
    // A full shell of blocks at distance [10, 11) in the y=0 plane:
    // one ring larger than the per-tick budget.
    let origin = [0.5, 0.5, 0.5];
    let positions: Vec<[i32; 3]> = (-12..=12)
        .flat_map(|x| (-12..=12).map(move |z| [x, 0, z]))
        .filter(|&p| expected_ring(origin, p) == 10)
        .collect();
    assert!(positions.len() > 8, "shell must hold several blocks");
    let result = synthetic_result(origin, &positions);

    let mut plan = MegaBlastPlan::new(&result, 8);
    assert_eq!(plan.ring_count(), 1);
    let mut slices = Vec::new();
    while let Some(slice) = plan.next_slice() {
        assert_eq!(slice.first_ring, 10);
        assert_eq!(slice.last_ring, 10);
        assert!(slice.removals.len() <= 8);
        slices.push(slice);
    }
    assert_eq!(slices.len(), positions.len().div_ceil(8));
    // The concatenated run stays in strict coordinate order even where a
    // slice boundary cut the ring in two.
    let seq: Vec<[i32; 3]> = slices
        .iter()
        .flat_map(|s| s.removals.iter().map(|r| r.pos))
        .collect();
    for pair in seq.windows(2) {
        assert!(pair[0] < pair[1]);
    }
    let drained: BTreeSet<[i32; 3]> = seq.iter().copied().collect();
    assert_eq!(drained, positions.iter().copied().collect());
    // tick_index counts slices from zero.
    for (i, slice) in slices.iter().enumerate() {
        assert_eq!(slice.tick_index, i as u64);
    }

    // Determinism: a plan built from a differently-ordered result emits
    // the identical sequence.
    let mut reversed = positions.clone();
    reversed.reverse();
    let rerun = synthetic_result(origin, &reversed);
    let mut plan2 = MegaBlastPlan::new(&rerun, 8);
    let seq2 = drain_positions(&mut plan2).seq;
    let seq1: Vec<(u32, [i32; 3])> = seq.iter().map(|&p| (10, p)).collect();
    assert_eq!(seq1, seq2, "ring drain order must be deterministic");
}

#[test]
fn mega_blast_plan_budget_edges_and_empty_plan() {
    // Empty result: complete immediately, no slice ever emitted.
    let empty = synthetic_result([0.5, 0.5, 0.5], &[]);
    let mut plan = MegaBlastPlan::new(&empty, 16);
    assert!(plan.is_complete());
    assert_eq!(plan.total(), 0);
    assert_eq!(plan.ring_count(), 0);
    assert_eq!(plan.progress(), (0, 0));
    assert!(plan.next_slice().is_none());
    assert!(!MegaBlastPlan::needs_slicing(&empty));

    let cells = synthetic_result(
        [0.5, 0.5, 0.5],
        &[
            [0, 0, 0],
            [1, 0, 0],
            [2, 0, 0],
            [3, 0, 0],
            [4, 0, 0],
            [5, 0, 0],
        ],
    );

    // Budget 0 clamps to one removal per tick.
    let mut plan = MegaBlastPlan::new(&cells, 0);
    assert_eq!(plan.budget(), 1);
    let mut count = 0usize;
    while let Some(slice) = plan.next_slice() {
        assert_eq!(slice.removals.len(), 1);
        count += 1;
    }
    assert_eq!(count, 6);

    // Budget >= total emits exactly one slice; done == total.
    let mut plan = cells.plan(64);
    let slice = plan.next_slice().expect("one slice");
    assert_eq!(slice.removals.len(), 6);
    assert_eq!(slice.done, 6);
    assert_eq!(slice.total, 6);
    assert_eq!(slice.tick_index, 0);
    assert!(plan.next_slice().is_none());

    // Exact budget multiple drains in whole slices.
    let mut plan = cells.plan(3);
    let slices = plan.drain();
    assert_eq!(slices.len(), 2);
    assert!(slices.iter().all(|s| s.removals.len() == 3));
    assert_eq!(slices[1].done, 6);
}

#[test]
fn mega_blast_plan_needs_slicing_threshold_boundary() {
    let origin = [0.5, 0.5, 0.5];
    let positions: Vec<[i32; 3]> = (0..MEGA_BLAST_MAX_PER_TICK + 1)
        .map(|i| [i as i32, 0, 0])
        .collect();
    let at_limit = synthetic_result(origin, &positions[..MEGA_BLAST_MAX_PER_TICK]);
    let over_limit = synthetic_result(origin, &positions);
    assert!(!MegaBlastPlan::needs_slicing(&at_limit));
    assert!(MegaBlastPlan::needs_slicing(&over_limit));
    assert_eq!(MEGA_BLAST_MAX_PER_TICK, 16_384);
}

#[test]
fn mega_blast_plan_section_updates_pack_signed_block_coords() {
    // Cells spanning section boundaries on both sides of zero — signed
    // division must route them to the right section and local slot.
    let positions = [
        [-17, -5, 33],
        [-16, 0, 0],
        [-1, -1, -1],
        [0, 0, 0],
        [15, 15, 15],
        [16, 0, 0],
        [31, -9, -40],
    ];
    let result = synthetic_result([0.5, 0.5, 0.5], &positions);
    let mut plan = MegaBlastPlan::new(&result, 64);
    let slice = plan.next_slice().expect("one slice");

    // One packed update per removal; every update clears to air.
    let total_updates: usize = slice.section_updates.iter().map(|s| s.updates.len()).sum();
    assert_eq!(total_updates, positions.len());
    // Sections sorted by coordinate; updates sorted by packed position.
    let sections: Vec<[i32; 3]> = slice.section_updates.iter().map(|s| s.section).collect();
    let mut sorted_sections = sections.clone();
    sorted_sections.sort();
    assert_eq!(sections, sorted_sections);
    for section_update in &slice.section_updates {
        let packed: Vec<u16> = section_update
            .updates
            .iter()
            .map(|u| u.packed_pos)
            .collect();
        let mut sorted = packed.clone();
        sorted.sort();
        assert_eq!(packed, sorted);
        for update in &section_update.updates {
            assert_eq!(update.block_id, 0, "removal writes air");
        }
    }
    // Every removal round-trips through its section update: section =
    // div_euclid(16), packed local = x | z<<4 | y<<8 of rem_euclid(16).
    let mut seen: BTreeSet<[i32; 3]> = BTreeSet::new();
    for section_update in &slice.section_updates {
        for update in &section_update.updates {
            let p = update.packed_pos;
            let (lx, lz, ly) = (
                i32::from(p & 15),
                i32::from((p >> 4) & 15),
                i32::from(p >> 8),
            );
            let pos = [
                section_update.section[0] * 16 + lx,
                section_update.section[1] * 16 + ly,
                section_update.section[2] * 16 + lz,
            ];
            seen.insert(pos);
        }
    }
    let expected: BTreeSet<[i32; 3]> = positions.iter().copied().collect();
    assert_eq!(seen, expected);
    // Spot-check one negative-section packing: [-17,-5,33] -> section
    // [-2,-1,2], local (15,11,1) -> packed 15 | 1<<4 | 11<<8.
    let entry = slice
        .section_updates
        .iter()
        .find(|s| s.section == [-2, -1, 2])
        .expect("negative section present");
    assert!(entry
        .updates
        .iter()
        .any(|u| u.packed_pos == (15 | (1 << 4) | (11 << 8))));
}

#[test]
fn mega_blast_plan_over_16k_blocks_drains_bounded_multi_tick_slices() {
    // A solid dirt world served by a closure `BlastBlockSource` — no
    // giant map needed for a >16k-cell crater.
    let dirt_cube = |x: i32, y: i32, z: i32| -> BlastCell {
        if (-72..=72).contains(&x) && (8..=136).contains(&y) && (-72..=72).contains(&z) {
            DIRT
        } else {
            AIR
        }
    };
    let origin = [0.5, 72.0, 0.5];
    let mut batch = ExplosionBatch::new();
    let result = batch
        .blast(&dirt_cube, origin, 40.0, 0x4D32_0001)
        .expect("power-40 blast fits the cache");

    // The M32 trigger: strictly more than 16,384 affected blocks.
    assert!(
        result.cells.len() > MEGA_BLAST_MAX_PER_TICK,
        "test blast must exceed the >16k threshold, got {}",
        result.cells.len()
    );
    assert!(MegaBlastPlan::needs_slicing(&result));

    let mut plan = MegaBlastPlan::new(&result, MEGA_BLAST_MAX_PER_TICK);
    assert_eq!(plan.budget(), MEGA_BLAST_MAX_PER_TICK);
    let mut slices = 0usize;
    let mut last_done = 0usize;
    let mut last_ring = 0u32;
    let mut conserved = 0u64;
    let mut seen: BTreeSet<[i32; 3]> = BTreeSet::new();
    while let Some(slice) = plan.next_slice() {
        slices += 1;
        // Bounded block budget per tick slice.
        assert!(
            slice.removals.len() <= MEGA_BLAST_MAX_PER_TICK,
            "tick slice exceeded the >16k budget"
        );
        assert_eq!(slice.done, last_done + slice.removals.len());
        last_done = slice.done;
        // Innermost rings first, deterministic coordinate order inside.
        for removal in &slice.removals {
            assert!(
                removal.ring >= last_ring,
                "ring regression: {} < {}",
                removal.ring,
                last_ring
            );
            last_ring = removal.ring;
            seen.insert(removal.pos);
        }
        for stack in &slice.drops {
            assert!(stack.count <= 64);
            conserved += u64::from(stack.count);
        }
    }
    assert!(plan.is_complete());
    assert!(slices >= 2, "a >16k blast must span multiple ticks");
    assert_eq!(last_done, plan.total());
    assert_eq!(
        slices,
        plan.total().div_ceil(MEGA_BLAST_MAX_PER_TICK),
        "one tick slice per budget chunk"
    );
    // First slice starts at the innermost ring (the origin cell itself
    // is destroyed by every ray leaving it).
    let mut replay = result.plan(MEGA_BLAST_MAX_PER_TICK);
    assert_eq!(replay.next_slice().unwrap().first_ring, 0);
    // Full coverage: every destroyed cell was scheduled exactly once.
    let expected: BTreeSet<[i32; 3]> = result.cells.iter().map(|c| c.pos).collect();
    assert_eq!(seen, expected);
    // Every dropped item is conserved through per-tick coalescing.
    let expected_total: u64 = expected_drops(std::slice::from_ref(&result)).values().sum();
    assert_eq!(conserved, expected_total);
    assert!(conserved > u64::from(MEGA_BLAST_MAX_PER_TICK as u32));

    // Below the threshold the same API reports a single-tick blast — a
    // real sub-16k crater, not an empty result.
    let small = batch
        .blast(&dirt_cube, [0.5, 100.0, 0.5], TNT_POWER, 0x4D32_0002)
        .expect("small blast");
    assert!(!small.cells.is_empty());
    assert!(!MegaBlastPlan::needs_slicing(&small));
}
