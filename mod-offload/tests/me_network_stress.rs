// SPDX-License-Identifier: LGPL-3.0-or-later

//! Lane M7a integration: a six-tier factory DAG with shared intermediates and
//! byproducts, a priority chain of 63-type storage cells, and a channel-routed
//! network with a starved branch — all solved deterministically with exact
//! item conservation.

use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

use mod_offload::me_network::{
    ChannelConflict, CraftingPattern, MeChannelGraph, MeCraftingPlanner, MeNodeKind,
    MeStorageIndex, StorageCell, ADHOC_CHANNEL_CAP,
};

const RAW_TYPES: usize = 400;
const SHARED_TYPES: usize = 40;
const SCRAP_TYPES: usize = 12;
/// Tier 1 is consumed by the target, tier 5 bottoms out in raw materials.
const TIER_SIZE: [usize; 6] = [0, 16, 64, 128, 256, 600];
const MAX_DEPTH: usize = 16;
const SOLVE_BUDGET: Duration = Duration::from_secs(1);

fn raw(index: usize) -> String {
    format!("raw_{index:04}")
}

fn shared(index: usize) -> String {
    format!("shared_{index:02}")
}

fn scrap(index: usize) -> String {
    format!("scrap_{index:02}")
}

fn tier(level: usize, index: usize) -> String {
    format!("t{level}_{index:04}")
}

/// Tier-k items consume three children of tier k+1 (raw materials for k = 5)
/// plus one of `SHARED_TYPES` shared intermediates, and yield an alternating
/// count plus a scrap byproduct. Shared intermediates craft in pairs from raw
/// materials so every second consumer is served from byproduct surplus.
fn factory_patterns() -> Vec<CraftingPattern> {
    let mut patterns = Vec::new();
    let mut inputs: Vec<(String, u64)> =
        (0..TIER_SIZE[1]).map(|index| (tier(1, index), 1)).collect();
    inputs.push(("unobtainium".to_string(), 1));
    patterns.push(CraftingPattern {
        id: 0,
        inputs,
        outputs: vec![("ultimate_machine".to_string(), 1)],
    });
    let mut id = 1;
    for level in 1..=5_usize {
        let child_size = if level == 5 {
            RAW_TYPES
        } else {
            TIER_SIZE[level + 1]
        };
        let child = |slot: usize| {
            if level == 5 {
                raw(slot % child_size)
            } else {
                tier(level + 1, slot % child_size)
            }
        };
        for index in 0..TIER_SIZE[level] {
            patterns.push(CraftingPattern {
                id,
                inputs: vec![
                    (child(index * 3), 1),
                    (child(index * 3 + 1), 1),
                    (child(index * 3 + 2), 1),
                    (shared(index % SHARED_TYPES), 1),
                ],
                outputs: vec![
                    (tier(level, index), 1 + (index % 2) as u64),
                    (scrap(index % SCRAP_TYPES), 1),
                ],
            });
            id += 1;
        }
    }
    for index in 0..SHARED_TYPES {
        // Coprime strides spread raw demand across every stocked type; a
        // clustered recipe would drain the few types it touches dry.
        patterns.push(CraftingPattern {
            id,
            inputs: vec![
                (raw((index * 97 + 11) % RAW_TYPES), 2),
                (raw((index * 53 + 37) % RAW_TYPES), 3),
            ],
            outputs: vec![(shared(index), 2)],
        });
        id += 1;
    }
    patterns
}

/// Eight 63-type cells under descending priorities hold 400 raw types: six
/// full cells, one partial, and an empty lowest-priority spill cell that ends
/// up taking plan outputs.
fn factory_storage() -> MeStorageIndex {
    let mut index = MeStorageIndex::new();
    for (slot, priority) in [70, 50, 30, 10, -10, -30, -50, -70].iter().enumerate() {
        index
            .add_cell(StorageCell::new(slot as u64 + 1, *priority, 2_048, 63, 8))
            .unwrap();
    }
    for item in 0..RAW_TYPES {
        assert_eq!(index.insert(&raw(item), 128), 128);
    }
    index
}

#[test]
fn priority_cells_fill_in_order_before_planning() {
    let index = factory_storage();
    for slot in 1..=6 {
        assert_eq!(index.cell(slot).unwrap().contents.len(), 63);
    }
    assert_eq!(index.cell(7).unwrap().contents.len(), RAW_TYPES - 6 * 63);
    assert!(index.cell(8).unwrap().contents.is_empty());
    // Extraction drains the lowest priority first.
    let mut index = index;
    let withdrawn = index.extract(&raw(0), 16);
    assert_eq!(withdrawn, 16);
    assert_eq!(index.cell(7).unwrap().contents.len(), 22);
    assert_eq!(index.cell(1).unwrap().contents[&raw(0)], 112);
}

#[test]
fn deep_factory_plan_is_deterministic_conserving_and_fast() {
    let index = factory_storage();
    let patterns = factory_patterns();
    let start = Instant::now();
    let plan =
        MeCraftingPlanner::plan_craft("ultimate_machine", 1, &index, &patterns, MAX_DEPTH).unwrap();
    let elapsed = start.elapsed();
    let rerun =
        MeCraftingPlanner::plan_craft("ultimate_machine", 1, &index, &patterns, MAX_DEPTH).unwrap();
    assert!(elapsed < SOLVE_BUDGET, "planning took {elapsed:?}");
    assert_eq!(plan, rerun, "planning is not deterministic");

    // Coverage: the tree demanded 1,000+ distinct intermediate item types.
    let crafted: BTreeSet<&String> = plan
        .steps
        .iter()
        .flat_map(|step| step.outputs.iter().map(|(item, _)| item))
        .collect();
    assert!(
        crafted.len() >= 1_000,
        "only {} intermediate types crafted",
        crafted.len()
    );
    assert_eq!(
        plan.missing_ingredients.len(),
        1,
        "unexpected missing ingredients: {:?}",
        plan.missing_ingredients
    );
    assert_eq!(plan.missing_ingredients["unobtainium"], 1);
    assert_eq!(
        plan.deliverables(),
        BTreeMap::from([("ultimate_machine".to_string(), 1)])
    );

    // Shared intermediates are crafted once and reused: pair-yield shared
    // recipes run about half as often as their consumers demand, and every
    // produced unit is either consumed or left over as surplus.
    let mut shared_demand = 0_u64;
    let mut shared_produced = 0_u64;
    let mut shared_runs = 0_u64;
    let mut cpu_bytes = 0_u64;
    for step in &plan.steps {
        for (item, count) in &step.inputs {
            if item.starts_with("shared_") {
                shared_demand += count;
            }
            cpu_bytes += count;
        }
        if step
            .outputs
            .iter()
            .any(|(item, _)| item.starts_with("shared_"))
        {
            shared_runs += step.times;
        }
        for (item, count) in &step.outputs {
            if item.starts_with("shared_") {
                shared_produced += count;
            }
            cpu_bytes += count;
        }
    }
    let shared_surplus: u64 = plan
        .produced_surplus
        .iter()
        .filter(|(item, _)| item.starts_with("shared_"))
        .map(|(_, count)| *count)
        .sum();
    assert_eq!(shared_produced, shared_demand + shared_surplus);
    assert!(shared_demand > 1_000);
    assert!(shared_runs < shared_demand);
    assert_eq!(plan.estimated_cpu_bytes, cpu_bytes);
    // Byproducts are tracked: every run drops a scrap nobody consumes.
    let scrap_surplus: u64 = plan
        .produced_surplus
        .iter()
        .filter(|(item, _)| item.starts_with("scrap_"))
        .map(|(_, count)| *count)
        .sum();
    assert!(scrap_surplus > 1_000);

    // Exact conservation simulating execution leaf-first: storage credit plus
    // produced items covers step inputs, leaving deliverables plus surplus
    // minus the missing ingredient.
    let mut pool: BTreeMap<String, i128> = plan
        .consumed_from_storage
        .iter()
        .map(|(item, count)| (item.clone(), i128::from(*count)))
        .collect();
    for step in &plan.steps {
        for (item, count) in &step.inputs {
            let left = pool.entry(item.clone()).or_default();
            *left -= i128::from(*count);
            assert!(
                *left >= -(plan.missing_ingredients.get(item).copied().unwrap_or(0) as i128),
                "step input {item} went deeper than the missing reserve"
            );
        }
        for (item, count) in &step.outputs {
            *pool.entry(item.clone()).or_default() += i128::from(*count);
        }
    }
    let mut expected: BTreeMap<String, i128> = BTreeMap::new();
    for (item, count) in plan.deliverables().iter().chain(&plan.produced_surplus) {
        *expected.entry(item.clone()).or_default() += i128::from(*count);
    }
    for (item, count) in &plan.missing_ingredients {
        *expected.entry(item.clone()).or_default() -= i128::from(*count);
    }
    expected.retain(|_, amount| *amount != 0);
    pool.retain(|_, amount| *amount != 0);
    assert_eq!(pool, expected, "items were duplicated or lost");
}

#[test]
fn execute_plan_reserves_inputs_and_commits_exact_totals() {
    let patterns = factory_patterns();
    let mut index = factory_storage();
    let plan =
        MeCraftingPlanner::plan_craft("ultimate_machine", 1, &index, &patterns, MAX_DEPTH).unwrap();
    let consumed_total: u64 = plan.consumed_from_storage.values().sum();
    let outbound_total: u64 = plan
        .deliverables()
        .values()
        .chain(plan.produced_surplus.values())
        .sum();
    let before = index.total_items();

    let report = MeCraftingPlanner::execute_plan(&plan, &mut index).unwrap();
    let expected_reservation: Vec<(String, u64)> = plan.consumed_from_storage.into_iter().collect();
    assert_eq!(report.reservation.items(), expected_reservation.as_slice());
    let committed: u64 = report.committed_outputs.values().sum();
    let uncommitted: u64 = report.uncommitted_outputs.values().sum();
    assert_eq!(committed + uncommitted, outbound_total);
    assert_eq!(index.total_items(), before - consumed_total + committed);
    // The requested machine always commits; surplus may spill past the cell
    // type budget and is reported instead of duplicated.
    assert_eq!(index.available("ultimate_machine"), 1);
    assert_eq!(report.committed_outputs["ultimate_machine"], 1);

    // A short reservation fails closed and leaves the index untouched.
    let before_raw = index.available(&raw(0));
    let error = index
        .reserve_batch(&[(raw(0), 1), ("ghost".to_string(), 5)])
        .unwrap_err();
    assert_eq!(error.missing, vec![("ghost".to_string(), 5)]);
    assert_eq!(index.available(&raw(0)), before_raw);
}

/// A controller on a dense trunk with a normal-cable branch (8 devices for 10
/// requests starves 2), an ad-hoc component over its 8-device cap, two
/// conflicting multiblocks, and one oversized multiblock — all solved in one
/// deterministic pass.
#[test]
fn channel_graph_allocates_starves_and_conflicts_deterministically() {
    let mut graph = MeChannelGraph::default();
    graph.add_node(0, MeNodeKind::Controller).unwrap();
    for cable in [10, 11, 12] {
        graph.add_node(cable, MeNodeKind::DenseCable).unwrap();
    }
    graph.add_node(20, MeNodeKind::NormalCable).unwrap();
    graph.set_link(0, 10, true).unwrap();
    graph.set_link(10, 11, true).unwrap();
    graph.set_link(11, 12, true).unwrap();
    graph.set_link(11, 20, true).unwrap();
    for (index, cable) in (0..20_u32).map(|index| (index, 10 + index / 7)) {
        let device = 100 + index;
        graph
            .add_node(
                device,
                MeNodeKind::Device {
                    requires_channel: true,
                },
            )
            .unwrap();
        graph.set_link(cable, device, true).unwrap();
    }
    for index in 0..10_u32 {
        let device = 200 + index;
        graph
            .add_node(
                device,
                MeNodeKind::Device {
                    requires_channel: true,
                },
            )
            .unwrap();
        graph.set_link(20, device, true).unwrap();
    }
    graph
        .add_node(
            300,
            MeNodeKind::Device {
                requires_channel: false,
            },
        )
        .unwrap();
    graph.set_link(20, 300, true).unwrap();

    graph.add_node(40, MeNodeKind::NormalCable).unwrap();
    for index in 0..12_u32 {
        let device = 400 + index;
        graph
            .add_node(
                device,
                MeNodeKind::Device {
                    requires_channel: true,
                },
            )
            .unwrap();
        graph.set_link(40, device, true).unwrap();
    }

    graph.add_node(500, MeNodeKind::Controller).unwrap();
    graph.add_node(505, MeNodeKind::DenseCable).unwrap();
    graph.add_node(510, MeNodeKind::Controller).unwrap();
    graph.set_link(500, 505, true).unwrap();
    graph.set_link(505, 510, true).unwrap();
    graph
        .add_node(
            520,
            MeNodeKind::Device {
                requires_channel: true,
            },
        )
        .unwrap();
    graph.set_link(505, 520, true).unwrap();

    graph.add_node(600, MeNodeKind::Controller).unwrap();
    for cable in 601..=607 {
        graph.add_node(cable, MeNodeKind::NormalCable).unwrap();
        graph.set_link(600, cable, true).unwrap();
    }
    graph
        .add_node(
            620,
            MeNodeKind::Device {
                requires_channel: true,
            },
        )
        .unwrap();
    graph.set_link(601, 620, true).unwrap();

    let start = Instant::now();
    let allocation = graph.solve_channels();
    let elapsed = start.elapsed();
    assert!(elapsed < SOLVE_BUDGET, "solving took {elapsed:?}");
    assert_eq!(allocation, graph.solve_channels());

    assert_eq!(
        allocation.conflicts,
        vec![
            ChannelConflict::ConflictingControllers {
                groups: vec![500, 510]
            },
            ChannelConflict::InvalidControllerMultiblock {
                controllers: vec![600]
            },
        ]
    );
    let online: BTreeSet<u32> = (100..120)
        .chain(200..208)
        .chain([300])
        .chain(400..400 + ADHOC_CHANNEL_CAP as u32)
        .collect();
    assert_eq!(allocation.online_devices, online);
    let starved: BTreeSet<u32> = [208, 209]
        .into_iter()
        .chain(408..412)
        .chain([520, 620])
        .collect();
    assert_eq!(allocation.channel_starved_devices, starved);
    // The 8-channel branch saturates; the dense trunk carries all 28 online
    // channels of the controller component through cable 10.
    assert_eq!(allocation.used_channels[&10], 28);
    assert_eq!(allocation.used_channels[&11], 21);
    assert_eq!(allocation.used_channels[&12], 6);
    assert_eq!(allocation.used_channels[&20], 8);
    assert_eq!(allocation.used_channels[&40], ADHOC_CHANNEL_CAP as u32);
}
