// SPDX-License-Identifier: LGPL-3.0-or-later
//! Behavior tests for `InventorySortState` QoL container sorting.

use mc_spine_client_ui::container_screen::{
    menu_schema, ContainerScreenState, InventorySortState, UiItemStack,
};

fn state(menu_id: u32) -> ContainerScreenState {
    ContainerScreenState::new(menu_schema(menu_id).unwrap(), 1, 0, "test menu")
}

fn sorter() -> InventorySortState {
    InventorySortState::new()
}

#[test]
fn merges_partial_stacks_up_to_max_stack() {
    let mut screen = state(2); // generic_9x3: container 0..27, player 27..63
    let mut s = sorter();
    screen.set_slot(0, UiItemStack::new(7, 40));
    screen.set_slot(5, UiItemStack::new(7, 30));
    screen.set_slot(9, UiItemStack::new(7, 10));

    s.sort_container_range(&mut screen, 0..27);

    // 40+30 -> 64 (cap) + 6 remainder; remainder +10 -> 16. Total conserved.
    let non_empty: Vec<&UiItemStack> = screen.slots[..27]
        .iter()
        .filter(|st| !st.is_empty())
        .collect();
    assert_eq!(non_empty.len(), 2);
    assert!(non_empty.iter().all(|st| st.item_id == 7));
    let mut got: Vec<u8> = non_empty.iter().map(|st| st.count).collect();
    got.sort_unstable();
    assert_eq!(got, vec![16, 64]);
    // Count-desc order puts the 64-stack first; empties sink to range end.
    assert_eq!(screen.slots[0].count, 64);
    assert_eq!(screen.slots[1].count, 16);
    assert!(screen.slots[2..27].iter().all(|st| st.is_empty()));
}

#[test]
fn does_not_merge_stacks_with_different_names_or_enchantments() {
    let mut screen = state(2);
    let mut s = sorter();
    screen.set_slot(0, UiItemStack::new(7, 30).with_name("Named dirt"));
    screen.set_slot(1, UiItemStack::new(7, 30)); // default name "item #7"
    screen.set_slot(2, UiItemStack::new(7, 30).with_enchantment("sharpness", 1));
    screen.set_slot(3, UiItemStack::new(7, 30).with_enchantment("sharpness", 5));

    s.sort_container_range(&mut screen, 0..27);

    // Same item_id but different stackable-relevant fields: no merging.
    assert!(screen.slots[..4]
        .iter()
        .all(|st| st.count == 30 && st.item_id == 7));
    // All four variants preserved and grouped by item_id.
    let names: Vec<&str> = screen.slots[..4]
        .iter()
        .map(|st| st.display_name.as_str())
        .collect();
    assert!(names.contains(&"Named dirt"));
    assert!(names.contains(&"item #7"));
}

#[test]
fn orders_by_count_desc_then_item_id_asc_and_leaves_outside_range_untouched() {
    let mut screen = state(2);
    let mut s = sorter();
    // Inside range 0..27.
    screen.set_slot(0, UiItemStack::new(9, 5));
    screen.set_slot(1, UiItemStack::new(3, 50));
    screen.set_slot(2, UiItemStack::new(3, 20)); // merges toward 64, 6 remainder
    screen.set_slot(3, UiItemStack::new(5, 64));
    // Outside range: player inventory slot 40.
    screen.set_slot(40, UiItemStack::new(1, 1));

    s.sort_container_range(&mut screen, 0..27);

    let got: Vec<(u32, u8)> = screen.slots[..6]
        .iter()
        .filter(|st| !st.is_empty())
        .map(|st| (st.item_id, st.count))
        .collect();
    assert_eq!(
        got,
        vec![(3, 64), (5, 64), (3, 6), (9, 5)],
        "count desc, item_id asc tie-break"
    );
    // Slot outside the range untouched.
    assert_eq!(screen.slots[40].item_id, 1);
    assert_eq!(screen.slots[40].count, 1);
}

#[test]
fn sort_is_idempotent() {
    let mut screen = state(2);
    let mut s = sorter();
    screen.set_slot(0, UiItemStack::new(4, 10));
    screen.set_slot(1, UiItemStack::new(2, 60));
    screen.set_slot(2, UiItemStack::new(4, 30));
    screen.set_slot(3, UiItemStack::new(9, 1));

    s.sort_container_range(&mut screen, 0..27);
    let first: Vec<UiItemStack> = screen.slots[..27].to_vec();
    s.sort_container_range(&mut screen, 0..27);
    let second: Vec<UiItemStack> = screen.slots[..27].to_vec();
    assert_eq!(first, second);
    assert!(s.last_sort_tick.is_some());
    assert_eq!(s.sort_count, 2);
    assert_eq!(s.last_sorted_range, Some(0..27));
    assert!(!s.sort_in_progress);
}

#[test]
fn toggle_sort_key_sorts_player_region_when_hovering_player_slot() {
    let mut screen = state(2); // generic_9x3
    let mut s = sorter();
    // Player region is 27..63. Put unsorted stacks there.
    screen.set_slot(30, UiItemStack::new(8, 3));
    screen.set_slot(31, UiItemStack::new(2, 60));
    screen.set_slot(50, UiItemStack::new(8, 20)); // merges -> 23
    // Container region stays untouched.
    screen.set_slot(0, UiItemStack::new(5, 1));

    s.toggle_sort_key(Some(40), &mut screen);

    // Player region sorted: 60(i2) first, then 23(i8).
    assert_eq!(screen.slots[27].item_id, 2);
    assert_eq!(screen.slots[27].count, 60);
    assert_eq!(screen.slots[28].item_id, 8);
    assert_eq!(screen.slots[28].count, 23);
    assert!(screen.slots[29..63].iter().all(|st| st.is_empty()));
    // Container region untouched.
    assert_eq!(screen.slots[0].item_id, 5);
    assert_eq!(screen.slots[0].count, 1);
}

#[test]
fn toggle_sort_key_sorts_container_region_when_hovering_container_slot() {
    let mut screen = state(2);
    let mut s = sorter();
    screen.set_slot(0, UiItemStack::new(6, 2));
    screen.set_slot(1, UiItemStack::new(4, 60));

    s.toggle_sort_key(Some(1), &mut screen);

    assert_eq!(screen.slots[0].item_id, 4);
    assert_eq!(screen.slots[0].count, 60);
    assert_eq!(screen.slots[1].item_id, 6);
    assert_eq!(screen.slots[1].count, 2);
}

#[test]
fn toggle_sort_key_with_no_hover_sorts_player_inventory() {
    let mut screen = state(2);
    let mut s = sorter();
    screen.set_slot(60, UiItemStack::new(3, 7));
    screen.set_slot(61, UiItemStack::new(1, 64));

    s.toggle_sort_key(None, &mut screen);

    assert_eq!(screen.slots[27].item_id, 1);
    assert_eq!(screen.slots[27].count, 64);
    assert_eq!(screen.slots[28].item_id, 3);
    assert_eq!(screen.slots[28].count, 7);
}

#[test]
fn toggle_sort_key_is_idempotent_on_second_press() {
    let mut screen = state(2);
    let mut s = sorter();
    screen.set_slot(0, UiItemStack::new(9, 5));
    screen.set_slot(1, UiItemStack::new(3, 50));
    screen.set_slot(2, UiItemStack::new(5, 64));
    screen.set_slot(30, UiItemStack::new(8, 3));
    screen.set_slot(31, UiItemStack::new(2, 60));

    s.toggle_sort_key(Some(1), &mut screen);
    let after_first: Vec<UiItemStack> = screen.slots.clone();
    s.toggle_sort_key(Some(1), &mut screen);
    assert_eq!(after_first, screen.slots);
}

#[test]
fn toggle_sort_key_respects_disabled_state() {
    let mut screen = state(2);
    let mut s = sorter();
    s.enabled = false;
    screen.set_slot(0, UiItemStack::new(9, 5));
    screen.set_slot(1, UiItemStack::new(3, 50));

    s.toggle_sort_key(Some(0), &mut screen);

    // No sort performed, hover still tracked.
    assert_eq!(screen.slots[0].item_id, 9);
    assert_eq!(screen.slots[1].item_id, 3);
    assert_eq!(s.hovered_slot, Some(0));
    assert_eq!(s.sort_count, 0);
}

#[test]
fn container_range_excludes_output_and_locked_slots() {
    let screen = state(14); // furnace: input 0, fuel 1, output 2; player 3..39
    let s = sorter();
    // Furnace container region is 0..2 (input+fuel); output slot 2 excluded.
    assert_eq!(s.container_range(&screen), 0..2);
    // Hovering the output slot still resolves to the container region.
    assert_eq!(s.slot_range_for(&screen, 2), 0..2);
    // Hovering a player slot resolves to the player region.
    assert_eq!(s.slot_range_for(&screen, 10), 3..39);
    // Hovering an input slot resolves to the container region.
    assert_eq!(s.slot_range_for(&screen, 0), 0..2);
}

#[test]
fn sort_range_is_clamped_to_slot_count() {
    let mut screen = state(2);
    let mut s = sorter();
    screen.set_slot(0, UiItemStack::new(1, 5));
    screen.set_slot(1, UiItemStack::new(2, 9));

    s.sort_container_range(&mut screen, 0..1000);

    // Whole screen sorted: 9(i2) then 5(i1) at front, rest empty.
    assert_eq!(screen.slots[0].item_id, 2);
    assert_eq!(screen.slots[1].item_id, 1);
    assert!(screen.slots[2..].iter().all(|st| st.is_empty()));
}
