// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later

//! Integration tests for [`mod_adapter_core::mine_cut_all`].

use std::collections::BTreeMap;

use mod_adapter_core::mine_cut_all::*;

fn pos(x: i32, y: i32, z: i32) -> MineCutAllPos {
    MineCutAllPos::new(x, y, z)
}

fn tag_map(entries: &[(&str, &[&str])]) -> BTreeMap<String, Vec<String>> {
    entries
        .iter()
        .map(|(tag, values)| {
            (
                (*tag).to_string(),
                values.iter().map(|value| (*value).to_string()).collect(),
            )
        })
        .collect()
}

fn world_with_tags(
    blocks: &[(MineCutAllPos, &str)],
    tags: &BTreeMap<String, Vec<String>>,
) -> MineCutAllWorldData {
    let mut world = MineCutAllWorldData::new();
    for (position, block_id) in blocks {
        world.set_block(*position, block_id);
    }
    world.set_tag_index(MineCutAllTagIndex::from_tag_map(tags));
    world
}

fn ore_world() -> MineCutAllWorldData {
    world_with_tags(
        &[
            (pos(0, 0, 0), "example_mod:gem_ore"),
            (pos(1, 0, 0), "example_mod:gem_ore"),
            (pos(0, 1, 0), "example_mod:gem_ore"),
            (pos(-1, 0, 0), "example_mod:gem_ore"),
            (pos(0, 0, 1), "example_mod:gem_ore"),
            (pos(2, 0, 0), "example_mod:gem_ore"),
            (pos(3, 0, 0), "example_mod:gem_ore"),
        ],
        &tag_map(&[("c:ores", &["example_mod:gem_ore"])]),
    )
}

fn ore_tool() -> ToolDescriptor {
    ToolDescriptor::new("example_mod:gem_pick").with_mineable_tags(["#c:ores"])
}

fn axe_tool() -> ToolDescriptor {
    ToolDescriptor::new("example_mod:wood_axe").with_item_tags(["#c:tools/axe"])
}

#[test]
fn default_config_is_strictly_off_and_returns_an_empty_plan() {
    let config = MineCutAllConfig::default();
    assert_eq!(config.mine_all, TriggerMode::Off);
    assert_eq!(config.cut_all, TriggerMode::Off);
    assert_eq!(config.max_blocks, DEFAULT_MAX_BLOCKS);
    assert!(config.stop_before_tool_break);
    assert!(config.allow_leaf_capped_custom_trees);

    let world = ore_world();
    let tool = ore_tool();
    let plan = plan_vein_or_tree_break(&config, &tool, &world, pos(0, 0, 0), false);
    assert_eq!(plan.kind, MineCutAllKind::None);
    assert!(plan.is_empty());
    assert_eq!(plan.durability_cost, 0);
    assert_eq!(plan.exhaustion_cost, 0.0);
    assert!(!plan.stopped_by_durability);
    assert!(!plan.skipped_by_claim);

    assert!(plan_mine_all(&config, &tool, &world, pos(0, 0, 0), true).is_empty());
    assert!(plan_cut_all(&config, &tool, &world, pos(0, 0, 0), true).is_empty());
}

#[test]
fn mine_all_uses_tool_component_tags_and_stops_at_claims() {
    let mut world = ore_world();
    world.protect(pos(2, 0, 0));

    let config = MineCutAllConfig::default().with_trigger(TriggerMode::Always, TriggerMode::Off);
    let plan = plan_vein_or_tree_break(&config, &ore_tool(), &world, pos(0, 0, 0), false);

    assert_eq!(plan.kind, MineCutAllKind::MineAll);
    assert_eq!(plan.positions.len(), 5);
    assert_eq!(plan.positions[0], pos(0, 0, 0));
    assert!(plan.positions.contains(&pos(1, 0, 0)));
    assert!(plan.positions.contains(&pos(0, 1, 0)));
    assert!(!plan.positions.contains(&pos(2, 0, 0)));
    // The protected cell is a BFS boundary, so the ore behind it is unreachable.
    assert!(!plan.positions.contains(&pos(3, 0, 0)));
    assert!(plan.skipped_by_claim);
    assert!(!plan.stopped_by_durability);
    assert_eq!(plan.durability_cost, 5);
    assert_eq!(plan.exhaustion_cost, 5.0 * BLOCK_BREAK_EXHAUSTION);
}

#[test]
fn mine_all_never_breaks_past_durability_and_preserves_the_last_point() {
    let mut world = ore_world();
    world.protect(pos(2, 0, 0));

    let config = MineCutAllConfig::default().with_trigger(TriggerMode::Always, TriggerMode::Off);
    let tool = ore_tool().with_remaining_durability(3);
    let plan = plan_mine_all(&config, &tool, &world, pos(0, 0, 0), false);

    assert_eq!(plan.kind, MineCutAllKind::MineAll);
    assert_eq!(plan.positions.len(), 2);
    assert_eq!(plan.positions[0], pos(0, 0, 0));
    assert!(plan.stopped_by_durability);
    assert_eq!(plan.durability_cost, 2);
    assert_eq!(plan.exhaustion_cost, 2.0 * BLOCK_BREAK_EXHAUSTION);

    let unguarded_tool = ore_tool().with_remaining_durability(3);
    let mut unguarded_config = config.clone();
    unguarded_config.stop_before_tool_break = false;
    let unguarded = plan_mine_all(
        &unguarded_config,
        &unguarded_tool,
        &world,
        pos(0, 0, 0),
        false,
    );
    assert_eq!(unguarded.positions.len(), 3);
    assert!(unguarded.stopped_by_durability);
    assert_eq!(unguarded.durability_cost, 3);
}

#[test]
fn cut_all_fells_tagged_vanilla_logs_with_nested_tag_expansion() {
    let tags = tag_map(&[
        ("minecraft:logs", &["#minecraft:oak_logs"]),
        ("minecraft:oak_logs", &["minecraft:oak_log"]),
        ("minecraft:mineable/axe", &["minecraft:oak_log"]),
    ]);
    let world = world_with_tags(
        &[
            (pos(0, 0, 0), "minecraft:oak_log"),
            (pos(0, 1, 0), "minecraft:oak_log"),
            (pos(0, 2, 0), "minecraft:oak_log"),
            (pos(1, 1, 0), "minecraft:oak_log"),
        ],
        &tags,
    );

    let config = MineCutAllConfig::default().with_trigger(TriggerMode::Off, TriggerMode::Always);
    let plan = plan_vein_or_tree_break(&config, &axe_tool(), &world, pos(0, 0, 0), false);

    assert_eq!(plan.kind, MineCutAllKind::CutAll);
    assert_eq!(plan.positions.len(), 4);
    assert_eq!(plan.positions[0], pos(0, 0, 0));
    assert_eq!(plan.durability_cost, 4);
    assert_eq!(plan.exhaustion_cost, 4.0 * BLOCK_BREAK_EXHAUSTION);
}

#[test]
fn cut_all_accepts_leaf_capped_custom_tree_but_not_a_plain_wall() {
    let tags = tag_map(&[("example_tofu:foliage", &["example_tofu:tofu_leaf"])]);
    let mut blocks = vec![
        (pos(0, 0, 0), "example_tofu:tofu_stem"),
        (pos(0, 1, 0), "example_tofu:tofu_stem"),
        (pos(0, 2, 0), "example_tofu:tofu_stem"),
        (pos(0, 3, 0), "example_tofu:tofu_stem"),
        (pos(1, 4, 0), "example_tofu:tofu_leaf"),
        (pos(-1, 4, 0), "example_tofu:tofu_leaf"),
        (pos(0, 4, 1), "example_tofu:tofu_leaf"),
    ];
    let world = world_with_tags(&blocks, &tags);

    let mut config =
        MineCutAllConfig::default().with_trigger(TriggerMode::Off, TriggerMode::Always);
    config
        .extra_leaf_blocks
        .insert("#example_tofu:foliage".to_string());
    let plan = plan_cut_all(&config, &axe_tool(), &world, pos(0, 0, 0), false);
    assert_eq!(plan.kind, MineCutAllKind::CutAll);
    assert_eq!(plan.positions.len(), 4);

    config.allow_leaf_capped_custom_trees = false;
    let disabled = plan_cut_all(&config, &axe_tool(), &world, pos(0, 0, 0), false);
    assert_eq!(disabled.kind, MineCutAllKind::None);
    assert!(disabled.is_empty());

    blocks.clear();
    blocks.extend([
        (pos(0, 0, 0), "minecraft:oak_planks"),
        (pos(0, 1, 0), "minecraft:oak_planks"),
        (pos(0, 2, 0), "minecraft:oak_planks"),
        (pos(0, 3, 0), "minecraft:oak_planks"),
    ]);
    let house = world_with_tags(&blocks, &BTreeMap::new());
    let wall = plan_cut_all(
        &MineCutAllConfig::default().with_trigger(TriggerMode::Off, TriggerMode::Always),
        &axe_tool(),
        &house,
        pos(0, 0, 0),
        false,
    );
    assert_eq!(wall.kind, MineCutAllKind::None);
    assert!(wall.is_empty());
}

#[test]
fn sneak_and_not_sneak_trigger_modes_gate_expansion() {
    let mut world = ore_world();
    world.protect(pos(2, 0, 0));
    let tool = ore_tool();

    let sneak_only =
        MineCutAllConfig::default().with_trigger(TriggerMode::SneakOnly, TriggerMode::Off);
    assert!(plan_mine_all(&sneak_only, &tool, &world, pos(0, 0, 0), false).is_empty());
    assert_eq!(
        plan_mine_all(&sneak_only, &tool, &world, pos(0, 0, 0), true)
            .positions
            .len(),
        5
    );

    let not_sneaking =
        MineCutAllConfig::default().with_trigger(TriggerMode::WhileNotSneaking, TriggerMode::Off);
    assert!(plan_mine_all(&not_sneaking, &tool, &world, pos(0, 0, 0), true).is_empty());
    assert_eq!(
        plan_mine_all(&not_sneaking, &tool, &world, pos(0, 0, 0), false)
            .positions
            .len(),
        5
    );
}

#[test]
fn item_tag_only_mining_tools_are_recognized_without_hardcoded_ids() {
    let mut world = ore_world();
    world.protect(pos(2, 0, 0));
    let config = MineCutAllConfig::default().with_trigger(TriggerMode::Always, TriggerMode::Off);

    let generic =
        ToolDescriptor::new("example_mod:generic_miner").with_item_tags(["#c:tools/mining_tool"]);
    let plan = plan_mine_all(&config, &generic, &world, pos(0, 0, 0), false);
    assert_eq!(plan.kind, MineCutAllKind::MineAll);
    assert_eq!(plan.positions.len(), 5);

    let axe = axe_tool();
    assert!(plan_mine_all(&config, &axe, &world, pos(0, 0, 0), false).is_empty());
}

#[test]
fn max_blocks_is_hard_capped_at_256_even_for_out_of_range_config() {
    let blocks: Vec<(MineCutAllPos, &str)> = (0..300)
        .map(|x| (pos(x, 0, 0), "example_mod:gem_ore"))
        .collect();
    let world = world_with_tags(&blocks, &tag_map(&[("c:ores", &["example_mod:gem_ore"])]));
    let tool = ore_tool();

    let mut config =
        MineCutAllConfig::default().with_trigger(TriggerMode::Always, TriggerMode::Off);
    config.max_blocks = 999;
    let plan = plan_mine_all(&config, &tool, &world, pos(0, 0, 0), false);
    assert_eq!(plan.positions.len(), MAX_MAX_BLOCKS);
    assert_eq!(plan.durability_cost, MAX_MAX_BLOCKS as u32);

    config.max_blocks = 0;
    let single = plan_mine_all(&config, &tool, &world, pos(0, 0, 0), false);
    assert_eq!(single.positions, vec![pos(0, 0, 0)]);
}

#[test]
fn tag_index_flattens_nested_block_tags() {
    let tags = tag_map(&[
        ("minecraft:logs", &["#minecraft:native_logs"]),
        ("minecraft:native_logs", &["modpack:palm_log"]),
    ]);
    let index = MineCutAllTagIndex::from_tag_map(&tags);
    assert!(index.has_tag("modpack:palm_log", "#minecraft:logs"));
    assert!(index.has_tag("modpack:palm_log", "minecraft:native_logs"));
    assert!(!index.has_tag("minecraft:stone", "minecraft:logs"));

    let mut tags_with_custom = tags.clone();
    tags_with_custom.insert(
        "modpack:custom_logs".to_string(),
        vec!["modpack:palm_log".to_string()],
    );
    let index = MineCutAllTagIndex::from_tag_map(&tags_with_custom);
    assert!(index.has_tag("modpack:palm_log", "modpack:custom_logs"));
}
