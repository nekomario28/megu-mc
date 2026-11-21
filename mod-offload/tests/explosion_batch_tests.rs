// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later

//! Lane PERF-TNT-BLAST: bit-exact Vanilla raycast parity, a real 1,000-TNT
//! chain stress run, and a power-32 modded mega-TNT sliced across ticks.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::time::Instant;

use mod_offload::explosion_batch::{
    blast_bounds, coalesce_item_counts, scrub_power, BlastCell, BlastResult, BlastWorld,
    ExplosionBatch, ExplosionError, ItemStack, MegaBlastPlan, XorShift64, BLOCK_COST_BASE,
    MAX_BLAST_POWER, MAX_RAY_STEPS, RAY_GRID, RAY_POWER_MIN, RAY_POWER_RANGE, RAY_STEP, STEP_DRAIN,
    TNT_POWER,
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
        while let Some(slice) = plan.next_slice() {
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
    while let Some(slice) = plan.next_slice() {
        slices += 1;
        assert!(slice.removals.len() <= 4096, "tick budget exceeded");
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
