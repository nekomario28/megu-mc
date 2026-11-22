// SPDX-License-Identifier: LGPL-3.0-or-later

use std::collections::BTreeMap;

use megu_kernel::{
    qol_gameplay::{
        evaluate_looking_at_hud, plan_mouse_tweak, sort_and_merge_inventory, ContainerFill,
        CropGrowth, FurnaceProgress, HarvestTool, HudTarget, InventoryLocation, InventorySide,
        InventorySnapshot, ItemStack, LookingAtBlock, LookingAtEntity, LookingAtTarget,
        MouseTweakAction, QolError, SlotRef, SortOrder,
    },
    BlockHitFace, EntityRayHit, RayHit,
};

fn block_hit() -> RayHit {
    RayHit {
        block_pos: [5, 64, -3],
        face: BlockHitFace::Up,
        hit_pos: [5.5, 65.0, -2.5],
        cursor_in_block: [0.5, 1.0, 0.5],
        distance: 3.0,
        inside: false,
    }
}

fn block<'a>(id: &'a str) -> LookingAtBlock<'a> {
    LookingAtBlock {
        hit: block_hit(),
        block_id: id,
        display_name: None,
        properties: &[],
        harvestable: None,
        preferred_tool: None,
        break_progress: None,
        crop_growth: None,
        container_fill: None,
        furnace_progress: None,
    }
}

fn entity_hit() -> EntityRayHit {
    EntityRayHit {
        entity_id: 23,
        hit_pos: [5.0, 64.0, -2.0],
        distance: 2.0,
    }
}

#[test]
fn hud_block_harvesting_crop_maturity_and_localization() {
    let mut wheat = block("minecraft:wheat");
    wheat.display_name = Some("小麦");
    wheat.properties = &[("facing", "north")];
    wheat.harvestable = Some(true);
    wheat.preferred_tool = Some(HarvestTool::Hoe);
    wheat.crop_growth = Some(CropGrowth { age: 3, max_age: 4 });
    wheat.break_progress = Some(0.25);
    let hud = evaluate_looking_at_hud(LookingAtTarget::Block(wheat.clone()), None).unwrap();
    assert_eq!(hud.target, HudTarget::Block([5, 64, -3]));
    assert_eq!(hud.name, "小麦");
    assert_eq!(hud.mod_badge, "Minecraft");
    assert_eq!(
        hud.lines,
        [
            "Harvestable: Yes (Hoe)",
            "Growth: 75%",
            "Breaking: 25%",
            "Facing: north"
        ]
    );

    wheat.crop_growth = Some(CropGrowth { age: 4, max_age: 4 });
    let mature = evaluate_looking_at_hud(LookingAtTarget::Block(wheat), None).unwrap();
    assert!(mature.lines.contains(&"Mature".to_string()));
    assert!(!mature.lines.iter().any(|line| line.starts_with("Growth:")));

    for tool in [
        HarvestTool::Pickaxe,
        HarvestTool::Axe,
        HarvestTool::Shovel,
        HarvestTool::Hoe,
    ] {
        let mut b = block("minecraft:stone");
        b.preferred_tool = Some(tool);
        b.harvestable = Some(false);
        let hud = evaluate_looking_at_hud(LookingAtTarget::Block(b), None).unwrap();
        assert!(hud.lines[0].starts_with("Harvestable: No ("));
    }
}

#[test]
fn hud_furnace_chest_badges_and_mod_deference() {
    let mut furnace = block("create:mechanical_press");
    furnace.furnace_progress = Some(FurnaceProgress {
        completed: 3,
        total: 5,
    });
    furnace.container_fill = Some(ContainerFill {
        occupied_slots: 12,
        total_slots: 27,
    });
    let hud = evaluate_looking_at_hud(LookingAtTarget::Block(furnace), None).unwrap();
    assert_eq!(hud.name, "Mechanical Press");
    assert_eq!(hud.mod_badge, "Create");
    assert_eq!(hud.lines, ["Smelting: 60%", "Container: 12 / 27 slots"]);

    let hud = evaluate_looking_at_hud(
        LookingAtTarget::Block(block("farmersdelight:tomato_crate")),
        None,
    )
    .unwrap();
    assert_eq!(hud.name, "Tomato Crate");
    assert_eq!(hud.mod_badge, "Farmer's Delight");

    let mut invalid_progress = block("minecraft:furnace");
    invalid_progress.break_progress = Some(f32::NAN);
    invalid_progress.furnace_progress = Some(FurnaceProgress {
        completed: 4,
        total: 0,
    });
    assert!(
        evaluate_looking_at_hud(LookingAtTarget::Block(invalid_progress), None)
            .unwrap()
            .lines
            .is_empty()
    );
    assert!(evaluate_looking_at_hud(
        LookingAtTarget::Block(block("minecraft:chest")),
        Some("jade"),
    )
    .is_none());
}

#[test]
fn hud_entity_health_armor_taming_owner_and_override() {
    let target = LookingAtEntity {
        hit: entity_hit(),
        entity_type_id: "minecraft:wolf",
        display_name: Some("Companion"),
        health: Some(18.0),
        max_health: Some(20.0),
        armor: Some(3.5),
        owner: Some("Player"),
        tamed: Some(true),
        mod_namespace: Some("farmersdelight"),
    };
    let hud = evaluate_looking_at_hud(LookingAtTarget::Entity(target.clone()), None).unwrap();
    assert_eq!(hud.target, HudTarget::Entity(23));
    assert_eq!(hud.name, "Companion");
    assert_eq!(hud.mod_badge, "Farmer's Delight");
    assert_eq!(
        hud.lines,
        ["HP: 18 / 20", "Armor: 3.5", "Tamed: Yes", "Owner: Player"]
    );
    assert!(evaluate_looking_at_hud(LookingAtTarget::Entity(target), Some("wthit")).is_none());
}

fn stack(id: &str, component: &str, name: &str, count: u32, max: u32) -> ItemStack {
    ItemStack {
        item_id: id.into(),
        component_fingerprint: component.into(),
        display_name: name.into(),
        count,
        max_stack: max,
    }
}

fn diamond(count: u32) -> ItemStack {
    stack("minecraft:diamond", "", "Diamond", count, 64)
}

fn slot(side: InventorySide, index: usize) -> SlotRef {
    SlotRef { side, index }
}

fn totals<'a>(stacks: impl IntoIterator<Item = &'a ItemStack>) -> BTreeMap<(String, String), u128> {
    let mut totals = BTreeMap::new();
    for stack in stacks {
        assert!((1..=stack.max_stack).contains(&stack.count));
        *totals
            .entry((stack.item_id.clone(), stack.component_fingerprint.clone()))
            .or_insert(0) += u128::from(stack.count);
    }
    totals
}

fn snapshot_totals(snapshot: &InventorySnapshot) -> BTreeMap<(String, String), u128> {
    totals(
        snapshot
            .container
            .iter()
            .chain(&snapshot.player)
            .chain(std::iter::once(&snapshot.cursor))
            .filter_map(Option::as_ref),
    )
}

fn slot_totals(slots: &[Option<ItemStack>]) -> BTreeMap<(String, String), u128> {
    totals(slots.iter().filter_map(Option::as_ref))
}

#[test]
fn wheel_push_moves_one_and_respects_stack_cap_and_empty_fallback() {
    let starting = InventorySnapshot {
        container: vec![Some(diamond(3)), Some(diamond(1))],
        player: vec![Some(diamond(63)), None],
        ..InventorySnapshot::default()
    };
    let plan = plan_mouse_tweak(
        MouseTweakAction::WheelPush {
            hovered: slot(InventorySide::Container, 0),
        },
        &starting,
        None,
    )
    .unwrap()
    .unwrap();
    assert_eq!(plan.transfers.len(), 1);
    assert_eq!(plan.transfers[0].count, 1);
    assert_eq!(plan.resulting.container[0].as_ref().unwrap().count, 2);
    assert_eq!(plan.resulting.player[0].as_ref().unwrap().count, 64);
    assert_eq!(snapshot_totals(&starting), snapshot_totals(&plan.resulting));

    let second = plan_mouse_tweak(
        MouseTweakAction::WheelPush {
            hovered: slot(InventorySide::Container, 0),
        },
        &plan.resulting,
        None,
    )
    .unwrap()
    .unwrap();
    assert_eq!(second.resulting.player[1].as_ref().unwrap().count, 1);
    assert_eq!(
        snapshot_totals(&starting),
        snapshot_totals(&second.resulting)
    );

    let mut full = starting.clone();
    full.player = vec![Some(diamond(64))];
    let no_space = plan_mouse_tweak(
        MouseTweakAction::WheelPush {
            hovered: slot(InventorySide::Container, 0),
        },
        &full,
        None,
    )
    .unwrap()
    .unwrap();
    assert!(no_space.transfers.is_empty());
    assert_eq!(no_space.resulting, full);
}

#[test]
fn wheel_pull_chooses_matching_item_and_moves_exactly_one() {
    let starting = InventorySnapshot {
        container: vec![
            Some(diamond(5)),
            Some(stack("minecraft:apple", "", "Apple", 3, 64)),
        ],
        player: vec![Some(diamond(9)), None],
        ..InventorySnapshot::default()
    };
    let plan = plan_mouse_tweak(
        MouseTweakAction::WheelPull {
            hovered: slot(InventorySide::Player, 0),
        },
        &starting,
        None,
    )
    .unwrap()
    .unwrap();
    assert_eq!(plan.resulting.container[0].as_ref().unwrap().count, 4);
    assert_eq!(plan.resulting.player[0].as_ref().unwrap().count, 10);
    assert_eq!(snapshot_totals(&starting), snapshot_totals(&plan.resulting));

    let empty_target = plan_mouse_tweak(
        MouseTweakAction::WheelPull {
            hovered: slot(InventorySide::Player, 1),
        },
        &starting,
        None,
    )
    .unwrap()
    .unwrap();
    assert_eq!(empty_target.resulting.player[1].as_ref().unwrap().count, 1);
    assert_eq!(
        snapshot_totals(&starting),
        snapshot_totals(&empty_target.resulting)
    );
}

#[test]
fn shift_drag_sweeps_matching_sources_in_ascending_slot_order() {
    let starting = InventorySnapshot {
        container: vec![
            Some(diamond(5)),
            Some(stack("minecraft:apple", "", "Apple", 2, 64)),
            Some(diamond(6)),
            Some(diamond(8)),
        ],
        player: vec![Some(diamond(62)), None],
        ..InventorySnapshot::default()
    };
    let visited = vec![
        slot(InventorySide::Container, 2),
        slot(InventorySide::Container, 0),
        slot(InventorySide::Container, 3),
        slot(InventorySide::Container, 2),
        slot(InventorySide::Container, 1),
        slot(InventorySide::Player, 0),
    ];
    let plan = plan_mouse_tweak(
        MouseTweakAction::ShiftLeftDragSweep { visited },
        &starting,
        None,
    )
    .unwrap()
    .unwrap();
    assert_eq!(
        plan.transfers[0].from,
        InventoryLocation::Slot(slot(InventorySide::Container, 0))
    );
    assert_eq!(plan.resulting.container[0], None);
    assert_eq!(plan.resulting.container[2], None);
    assert_eq!(plan.resulting.container[3], None);
    assert_eq!(plan.resulting.container[1], starting.container[1]);
    assert_eq!(plan.resulting.player[0].as_ref().unwrap().count, 64);
    assert_eq!(plan.resulting.player[1].as_ref().unwrap().count, 17);
    assert_eq!(snapshot_totals(&starting), snapshot_totals(&plan.resulting));
}

#[test]
fn right_drag_never_repaints_slot_or_underflows_cursor() {
    let starting = InventorySnapshot {
        container: vec![],
        player: vec![
            None,
            Some(stack("minecraft:apple", "", "Apple", 1, 64)),
            Some(diamond(63)),
            None,
            None,
        ],
        cursor: Some(diamond(3)),
    };
    let plan = plan_mouse_tweak(
        MouseTweakAction::RightDragNoDup {
            visited: vec![
                slot(InventorySide::Player, 0),
                slot(InventorySide::Player, 0),
                slot(InventorySide::Player, 1),
                slot(InventorySide::Player, 2),
                slot(InventorySide::Player, 2),
                slot(InventorySide::Player, 3),
                slot(InventorySide::Player, 4),
            ],
        },
        &starting,
        None,
    )
    .unwrap()
    .unwrap();
    assert_eq!(plan.transfers.len(), 3);
    assert!(plan.transfers.iter().all(|transfer| transfer.count == 1));
    assert_eq!(plan.resulting.cursor, None);
    assert_eq!(plan.resulting.player[0].as_ref().unwrap().count, 1);
    assert_eq!(plan.resulting.player[1], starting.player[1]);
    assert_eq!(plan.resulting.player[2].as_ref().unwrap().count, 64);
    assert_eq!(plan.resulting.player[3].as_ref().unwrap().count, 1);
    assert_eq!(plan.resulting.player[4], None);
    assert_eq!(snapshot_totals(&starting), snapshot_totals(&plan.resulting));
}

#[test]
fn gesture_deferral_invalid_inputs_and_atomic_failures() {
    let state = InventorySnapshot {
        player: vec![Some(diamond(3))],
        ..InventorySnapshot::default()
    };
    let action = MouseTweakAction::WheelPush {
        hovered: slot(InventorySide::Player, 0),
    };
    assert!(
        plan_mouse_tweak(action.clone(), &state, Some("mousetweaks"))
            .unwrap()
            .is_none()
    );
    assert_eq!(
        plan_mouse_tweak(action, &state, None)
            .unwrap()
            .unwrap()
            .resulting,
        state
    );

    let bad_slot = slot(InventorySide::Container, 5);
    let error = plan_mouse_tweak(
        MouseTweakAction::RightDragNoDup {
            visited: vec![bad_slot],
        },
        &state,
        None,
    );
    assert_eq!(error, Err(QolError::InvalidSlot(bad_slot)));

    let mut bad = state.clone();
    bad.player[0].as_mut().unwrap().count = 65;
    assert_eq!(
        plan_mouse_tweak(
            MouseTweakAction::WheelPush {
                hovered: slot(InventorySide::Player, 0)
            },
            &bad,
            None
        ),
        Err(QolError::InvalidStack)
    );
    assert!(plan_mouse_tweak(
        MouseTweakAction::WheelPush {
            hovered: slot(InventorySide::Player, 0)
        },
        &bad,
        Some("mousetweaks")
    )
    .unwrap()
    .is_none());
}

#[test]
fn sort_merges_partials_preserves_component_distinctions_and_hotbar() {
    let sentinel = Some(stack("minecraft:hotbar", "", "Pinned", 1, 1));
    let mut slots = vec![sentinel.clone(); 9];
    slots.extend([
        Some(diamond(35)),
        Some(stack(
            "minecraft:diamond",
            "enchanted",
            "Enchanted Diamond",
            7,
            64,
        )),
        Some(diamond(40)),
        Some(stack(
            "minecraft:diamond",
            "enchanted",
            "Enchanted Diamond",
            6,
            64,
        )),
        Some(stack("minecraft:sword", "sharpness:1", "Sword", 1, 1)),
        Some(stack("minecraft:sword", "sharpness:1", "Sword", 1, 1)),
        None,
    ]);
    let before = slot_totals(&slots);
    assert_eq!(
        sort_and_merge_inventory(&mut slots, 9..16, SortOrder::ByIdAsc, None),
        Ok(true)
    );
    assert!(slots[0..9].iter().all(|stack| *stack == sentinel));
    assert_eq!(slot_totals(&slots), before);
    assert_eq!(slots[9].as_ref().unwrap().count, 64);
    assert_eq!(slots[10].as_ref().unwrap().component_fingerprint, "");
    assert_eq!(slots[10].as_ref().unwrap().count, 11);
    assert_eq!(
        slots[11].as_ref().unwrap().component_fingerprint,
        "enchanted"
    );
    assert_eq!(slots[11].as_ref().unwrap().count, 13);
    assert_eq!(slots[12].as_ref().unwrap().max_stack, 1);
    assert_eq!(slots[13].as_ref().unwrap().max_stack, 1);
    assert!(slots[14..16].iter().all(Option::is_none));
}

#[test]
fn sorting_all_orders_and_deterministic_tie_breakers() {
    let contents = [
        Some(stack("minecraft:z", "", "Apple", 10, 64)),
        Some(stack("minecraft:a", "z", "Zucchini", 20, 64)),
        Some(stack("minecraft:a", "a", "Zucchini", 5, 64)),
        Some(stack("minecraft:b", "", "Banana", 20, 64)),
        None,
    ];
    let cases = [
        (
            SortOrder::ByIdAsc,
            vec![
                ("minecraft:a", "a"),
                ("minecraft:a", "z"),
                ("minecraft:b", ""),
                ("minecraft:z", ""),
            ],
        ),
        (
            SortOrder::ByNameAsc,
            vec![
                ("minecraft:z", ""),
                ("minecraft:b", ""),
                ("minecraft:a", "a"),
                ("minecraft:a", "z"),
            ],
        ),
        (
            SortOrder::ByCountDesc,
            vec![
                ("minecraft:a", "z"),
                ("minecraft:b", ""),
                ("minecraft:z", ""),
                ("minecraft:a", "a"),
            ],
        ),
    ];
    for (order, expected) in cases {
        let mut slots = contents.to_vec();
        assert_eq!(
            sort_and_merge_inventory(&mut slots, 0..5, order, None),
            Ok(true)
        );
        let actual: Vec<_> = slots
            .iter()
            .filter_map(Option::as_ref)
            .map(|item| (item.item_id.as_str(), item.component_fingerprint.as_str()))
            .collect();
        assert_eq!(actual, expected);
        assert!(slots[4].is_none());
    }
}

#[test]
fn sort_defers_and_rejects_bad_range_or_capacity_without_mutation() {
    let mut slots = vec![Some(diamond(2)), Some(diamond(3))];
    let original = slots.clone();
    assert_eq!(
        sort_and_merge_inventory(
            &mut slots,
            0..2,
            SortOrder::ByIdAsc,
            Some("inventorysorter")
        ),
        Ok(false)
    );
    assert_eq!(slots, original);
    assert_eq!(
        sort_and_merge_inventory(&mut slots, 1..3, SortOrder::ByIdAsc, None),
        Err(QolError::InvalidRange)
    );
    assert_eq!(slots, original);
    slots[1].as_mut().unwrap().max_stack = 16;
    let bad = slots.clone();
    assert_eq!(
        sort_and_merge_inventory(&mut slots, 0..2, SortOrder::ByIdAsc, None),
        Err(QolError::InconsistentMaxStack)
    );
    assert_eq!(slots, bad);
}

fn next_random(state: &mut u64) -> u64 {
    *state = state
        .wrapping_mul(6364136223846793005)
        .wrapping_add(1442695040888963407);
    *state
}

#[test]
fn seeded_inventory_sorting_preserves_exact_component_multisets() {
    for seed in 0..128_u64 {
        let mut random = seed ^ 0xC0DE_5EED_CAFE_1234;
        let mut original = Vec::with_capacity(60);
        for _ in 0..60 {
            let choice = next_random(&mut random);
            if choice.is_multiple_of(9) {
                original.push(None);
                continue;
            }
            let id = (choice >> 8) % 13;
            let component = (choice >> 17) % 5;
            let max = if id.is_multiple_of(7) {
                1
            } else if id.is_multiple_of(3) {
                16
            } else {
                64
            };
            let count = 1 + (next_random(&mut random) % max);
            original.push(Some(stack(
                &format!("example:item_{id}"),
                &format!("component_{component}"),
                &format!("Name {}", 20 - id),
                count as u32,
                max as u32,
            )));
        }
        for range in [9..36, 0..27, 0..54] {
            for order in [
                SortOrder::ByIdAsc,
                SortOrder::ByNameAsc,
                SortOrder::ByCountDesc,
            ] {
                let mut result = original.clone();
                assert_eq!(
                    sort_and_merge_inventory(&mut result, range.clone(), order, None),
                    Ok(true)
                );
                assert_eq!(
                    slot_totals(&result),
                    slot_totals(&original),
                    "seed {seed}, {range:?}, {order:?}"
                );
                assert_eq!(&result[..range.start], &original[..range.start]);
                assert_eq!(&result[range.end..], &original[range.end..]);
                let mut reached_empty = false;
                for slot in &result[range.clone()] {
                    reached_empty |= slot.is_none();
                    assert!(!reached_empty || slot.is_none());
                }
                let stable = result.clone();
                sort_and_merge_inventory(&mut result, range.clone(), order, None).unwrap();
                assert_eq!(
                    result, stable,
                    "sort must be idempotent for {seed}, {range:?}, {order:?}"
                );
            }
        }
    }
}
